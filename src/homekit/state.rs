//! What the HomeKit accessory keeps on disk (`<data_dir>/homekit/accessory.json`,
//! private to this account): its pairing identity and long-term key, the
//! setup code, the configuration number, the accessory ids of the cameras
//! and the paired controllers. Written atomically (temporary file, then
//! renamed). The file holds secrets: never log its content.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{anyhow, Context, Result};
use rand::{Rng, RngCore};
use serde::{Deserialize, Serialize};
use zeroize::Zeroize;

use super::crypto::LongTermKey;

/// Most controllers one accessory keeps (HAP: at least 16).
pub const MAX_PAIRINGS: usize = 16;
/// Failed pair-setup tries before pairing is refused until reset (HAP: 100).
pub const MAX_TRIES: u32 = 100;
/// The first accessory id given to a camera (1 is the bridge).
const FIRST_CAMERA_AID: u64 = 2;

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct Pairing {
    /// The controller's pairing identifier (a UUID string on Apple devices).
    pub id: String,
    /// Its Ed25519 long-term public key, hex.
    pub ltpk: String,
    pub admin: bool,
}

#[derive(Serialize, Deserialize)]
pub struct State {
    /// The accessory's pairing identifier, "AA:BB:CC:DD:EE:FF".
    pub device_id: String,
    /// Seed of the Ed25519 long-term key, hex.
    ltsk: String,
    pub setup_code: String,
    /// Four characters for the setup QR / mDNS setup hash.
    pub setup_id: String,
    /// `c#`: bumped whenever the accessory database changes (1-65535).
    pub config_number: u32,
    /// Fingerprint of the accessory database `config_number` belongs to.
    #[serde(default)]
    pub db_hash: String,
    /// Camera name -> accessory id, so ids stay put across restarts,
    /// reordering and cameras switched off and on again.
    #[serde(default)]
    pub aids: BTreeMap<String, u64>,
    #[serde(default)]
    pub failed_tries: u32,
    #[serde(default)]
    pub pairings: Vec<Pairing>,
    #[serde(skip)]
    path: PathBuf,
    /// The long-term key, made from `ltsk` once (signing then needs no lock).
    #[serde(skip)]
    lt: Option<Arc<LongTermKey>>,
}

impl Drop for State {
    fn drop(&mut self) {
        self.ltsk.zeroize();
        self.setup_code.zeroize();
    }
}

/// A random setup code "XXX-XX-XXX", never one HAP forbids.
pub fn new_setup_code() -> String {
    let mut r = rand::rngs::OsRng;
    loop {
        let digits: String = (0..8).map(|_| char::from(b'0' + r.gen_range(0..10u8))).collect();
        if code_allowed(&digits) {
            return format!("{}-{}-{}", &digits[..3], &digits[3..5], &digits[5..]);
        }
    }
}

/// HAP forbids codes of one digit repeated, 12345678 and 87654321.
pub fn code_allowed(digits: &str) -> bool {
    digits.len() == 8
        && digits.bytes().all(|b| b.is_ascii_digit())
        && !digits.bytes().all(|b| b == digits.as_bytes()[0])
        && digits != "12345678"
        && digits != "87654321"
}

fn new_setup_id() -> String {
    const CHARS: &[u8] = b"0123456789ABCDEFGHIJKLMNOPQRSTUVWXYZ";
    let mut r = rand::rngs::OsRng;
    (0..4).map(|_| CHARS[r.gen_range(0..CHARS.len())] as char).collect()
}

fn new_device_id() -> String {
    let mut b = [0u8; 6];
    rand::rngs::OsRng.fill_bytes(&mut b);
    // A locally administered, unicast address (it is not a real one).
    b[0] = (b[0] | 0x02) & !0x01;
    b.iter().map(|x| format!("{x:02X}")).collect::<Vec<_>>().join(":")
}

fn new_seed() -> String {
    let mut s = [0u8; 32];
    rand::rngs::OsRng.fill_bytes(&mut s);
    let h = hex(&s);
    s.zeroize();
    h
}

pub fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}

pub fn unhex(s: &str) -> Option<Vec<u8>> {
    if s.len() % 2 != 0 || !s.bytes().all(|b| b.is_ascii_hexdigit()) {
        return None;
    }
    (0..s.len()).step_by(2).map(|i| u8::from_str_radix(&s[i..i + 2], 16).ok()).collect()
}

impl State {
    pub fn file(dir: &Path) -> PathBuf {
        dir.join("accessory.json")
    }

    fn fresh(path: PathBuf) -> State {
        State {
            device_id: new_device_id(),
            ltsk: new_seed(),
            setup_code: new_setup_code(),
            setup_id: new_setup_id(),
            config_number: 1,
            db_hash: String::new(),
            aids: BTreeMap::new(),
            failed_tries: 0,
            pairings: Vec::new(),
            path,
            lt: None,
        }
    }

    /// Read the state, or make a new identity the first time.
    pub fn load_or_create(dir: &Path) -> Result<State> {
        let path = State::file(dir);
        if let Some(s) = State::load(dir)? {
            return Ok(s);
        }
        std::fs::create_dir_all(dir).with_context(|| format!("could not make {}", dir.display()))?;
        private_dir(dir);
        let s = State::fresh(path);
        s.save()?;
        Ok(s)
    }

    /// Read the state if there is one (it is never made here).
    pub fn load(dir: &Path) -> Result<Option<State>> {
        let path = State::file(dir);
        let text = match std::fs::read_to_string(&path) {
            Ok(t) => t,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(anyhow!("could not read {}: {e}", path.display())),
        };
        let mut s: State = serde_json::from_str(&text).map_err(|e| anyhow!("{} is damaged ({e}); reset HomeKit pairing to start over", path.display()))?;
        s.path = path;
        // Refuse a state that would not work (rather than failing later).
        s.key()?;
        if !code_allowed(&s.setup_code.replace('-', "")) || s.setup_id.len() != 4 {
            return Err(anyhow!("{} is damaged (setup code); reset HomeKit pairing to start over", s.path.display()));
        }
        Ok(Some(s))
    }

    pub fn save(&self) -> Result<()> {
        let text = serde_json::to_string_pretty(self)?;
        let tmp = self.path.with_extension("json.new");
        crate::tls::write_private(&tmp, text.as_bytes()).with_context(|| format!("could not write {}", tmp.display()))?;
        std::fs::rename(&tmp, &self.path).with_context(|| format!("could not replace {}", self.path.display()))?;
        Ok(())
    }

    /// The long-term key (made once, then shared).
    pub fn key(&mut self) -> Result<Arc<LongTermKey>> {
        if let Some(k) = &self.lt {
            return Ok(k.clone());
        }
        let mut seed: [u8; 32] = unhex(&self.ltsk).and_then(|v| v.try_into().ok()).ok_or_else(|| anyhow!("the HomeKit key in {} is damaged", self.path.display()))?;
        let k = LongTermKey::from_seed(&seed);
        seed.zeroize();
        let k = Arc::new(k?);
        self.lt = Some(k.clone());
        Ok(k)
    }

    /// Forget every controller and start as a new accessory: a new
    /// identity, key, setup code and configuration number.
    pub fn reset(&mut self) {
        let keep = std::mem::take(&mut self.aids);
        let path = std::mem::take(&mut self.path);
        let next = self.config_number;
        let mut fresh = State::fresh(path);
        fresh.aids = keep;
        fresh.config_number = next_config_number(next);
        *self = fresh;
    }

    pub fn paired(&self) -> bool {
        !self.pairings.is_empty()
    }

    /// Whether pair-setup may start now.
    pub fn pairable(&self) -> bool {
        !self.paired() && self.failed_tries < MAX_TRIES
    }

    pub fn pairing(&self, id: &str) -> Option<&Pairing> {
        self.pairings.iter().find(|p| p.id == id)
    }

    /// The accessory id of a camera, given a new one the first time.
    pub fn aid_for(&mut self, camera: &str) -> u64 {
        if let Some(a) = self.aids.get(camera) {
            return *a;
        }
        let next = self.aids.values().copied().max().map(|m| m + 1).unwrap_or(FIRST_CAMERA_AID).max(FIRST_CAMERA_AID);
        self.aids.insert(camera.to_string(), next);
        next
    }

    /// A renamed camera keeps its accessory id (also when an id was already
    /// given to the new name in the moment between the rename and this).
    pub fn rename(&mut self, old: &str, new: &str) -> bool {
        match self.aids.remove(old) {
            Some(a) => {
                self.aids.insert(new.to_string(), a);
                true
            }
            None => false,
        }
    }

    /// A rename is coming: the new name shares the old one's id meanwhile.
    pub fn alias(&mut self, old: &str, new: &str) -> bool {
        match self.aids.get(old).copied() {
            Some(a) if self.aids.get(new) != Some(&a) => {
                self.aids.insert(new.to_string(), a);
                true
            }
            _ => false,
        }
    }

    /// The rename did not happen: the new name lets go of the old one's id.
    pub fn unalias(&mut self, old: &str, new: &str) -> bool {
        if old != new && self.aids.get(new).is_some() && self.aids.get(new) == self.aids.get(old) {
            self.aids.remove(new);
            true
        } else {
            false
        }
    }

    /// Unpaired again (the last admin was removed): same identity and code.
    pub fn unpair(&mut self) {
        self.pairings.clear();
    }

    /// Record the database fingerprint; true when it changed (c# bumped).
    pub fn note_db(&mut self, hash: &str) -> bool {
        if self.db_hash == hash {
            return false;
        }
        if !self.db_hash.is_empty() {
            self.config_number = next_config_number(self.config_number);
        }
        self.db_hash = hash.to_string();
        true
    }
}

/// c# runs 1..=65535 and wraps to 1.
pub fn next_config_number(n: u32) -> u32 {
    if n >= 65535 {
        1
    } else {
        n + 1
    }
}

fn private_dir(dir: &Path) {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if let Err(e) = std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700)) {
            tracing::warn!("could not make {} private: {e}", dir.display());
        }
    }
    #[cfg(not(unix))]
    let _ = dir;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn setup_codes_follow_the_rules() {
        for bad in ["00000000", "11111111", "99999999", "12345678", "87654321", "1234567", "1234567a", ""] {
            assert!(!code_allowed(bad), "{bad}");
        }
        assert!(code_allowed("12345679"));
        for _ in 0..500 {
            let c = new_setup_code();
            assert_eq!(c.len(), 10);
            assert_eq!(&c[3..4], "-");
            assert_eq!(&c[6..7], "-");
            assert!(code_allowed(&c.replace('-', "")), "{c}");
        }
    }

    #[test]
    fn ids_look_right() {
        let id = new_device_id();
        assert_eq!(id.len(), 17);
        assert_eq!(id.split(':').count(), 6);
        assert!(id.split(':').all(|p| p.len() == 2 && p.chars().all(|c| c.is_ascii_hexdigit() && !c.is_ascii_lowercase())));
        let s = new_setup_id();
        assert_eq!(s.len(), 4);
        assert!(s.chars().all(|c| c.is_ascii_digit() || c.is_ascii_uppercase()));
    }

    #[test]
    fn config_number_wraps_to_one() {
        assert_eq!(next_config_number(1), 2);
        assert_eq!(next_config_number(65535), 1);
    }

    #[test]
    fn saved_privately_and_reloaded() {
        let dir = std::env::temp_dir().join(format!("as-hk-state-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let mut s = State::load_or_create(&dir).unwrap();
        let id = s.device_id.clone();
        assert_eq!(s.aid_for("Front"), 2);
        assert_eq!(s.aid_for("Back"), 3);
        assert_eq!(s.aid_for("Front"), 2);
        assert!(s.rename("Front", "Porch"));
        assert_eq!(s.aid_for("Porch"), 2);
        assert_eq!(s.aid_for("New"), 4);
        assert!(s.note_db("a"));
        assert_eq!(s.config_number, 1, "the first fingerprint keeps c# 1");
        assert!(!s.note_db("a"));
        assert!(s.note_db("b"));
        assert_eq!(s.config_number, 2);
        s.pairings.push(Pairing { id: "c1".into(), ltpk: hex(&[1; 32]), admin: true });
        s.save().unwrap();
        let back = State::load(&dir).unwrap().unwrap();
        assert_eq!(back.device_id, id);
        assert_eq!(back.pairings.len(), 1);
        assert_eq!(back.aids.get("Porch"), Some(&2));
        let mut back = back;
        assert_eq!(back.key().unwrap().public(), s.key().unwrap().public());
        // A rename onto a name that already got a new id keeps the old id.
        assert_eq!(back.aid_for("Late"), 5);
        assert!(back.rename("Back", "Late"));
        assert_eq!(back.aid_for("Late"), 3);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(std::fs::metadata(State::file(&dir)).unwrap().permissions().mode() & 0o777, 0o600);
            assert_eq!(std::fs::metadata(&dir).unwrap().permissions().mode() & 0o777, 0o700);
        }
        // Reset: a new identity and code, the camera ids kept, c# moved on.
        let mut r = back;
        let old_pub = r.key().unwrap().public();
        let old_code = r.setup_code.clone();
        r.failed_tries = 9;
        r.reset();
        assert_ne!(r.device_id, id);
        assert_ne!(r.key().unwrap().public(), old_pub, "a new key after reset");
        assert!(r.pairings.is_empty() && r.failed_tries == 0);
        assert_eq!(r.aids.get("Porch"), Some(&2));
        assert_eq!(r.config_number, 3);
        assert!(r.setup_code != old_code || r.setup_code.len() == 10);
        r.save().unwrap();
        assert!(State::load(&dir).unwrap().unwrap().pairable());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn damaged_file_is_reported_not_replaced() {
        let dir = std::env::temp_dir().join(format!("as-hk-bad-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(State::file(&dir), "{not json").unwrap();
        assert!(State::load(&dir).is_err());
        assert!(State::load_or_create(&dir).is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
