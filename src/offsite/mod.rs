//! Off-site backup: if this computer dies, burns or is stolen, the settings,
//! the database (events, faces, users) and the clips that matter are
//! somewhere else.
//!
//! Every `[offsite] every_hours` (Settings > System > Backup) a copy goes to a
//! folder (a network share such as `\\nas\backup`, another drive) or to
//! S3-compatible storage (AWS S3, Backblaze B2, Wasabi, MinIO, Cloudflare
//! R2). What is there afterwards:
//!
//! ```text
//! key.json                              which passphrase key is in use now (no secrets)
//! snapshots/<UTC time>/config.toml.enc  the settings (camera passwords!)
//! snapshots/<UTC time>/ambersight.db.enc the database
//! snapshots/<UTC time>/manifest.json    what belongs to this copy, with the key it
//!                                       was made with and a check value; written last
//! clips/<event>.mp4.enc                 clips of protected events (sent once)
//! thumbs/<event>.jpg.enc, thumbs/ai/<event>/<n>.jpg.enc   their pictures
//! ```
//!
//! With a passphrase every file is encrypted first (`.enc`; see crypt.rs:
//! Argon2id + AES-256-GCM). S3 cannot be used without one. A folder can be,
//! but then the settings with the camera passwords sit there readable.
//! The newest `keep` copies of settings + database are kept; clips and
//! pictures stay while a kept copy lists them or the event is still
//! protected here.
//!
//! Trust: whoever can write to the destination must not be able to slip
//! anything into a restore. With a passphrase, each manifest carries a
//! check value (HMAC with a key made from the passphrase) that restore
//! verifies before using it, every file it lists must be encrypted, and
//! each encrypted file names where it belongs (checked by the encryption),
//! so files cannot be swapped or replaced by plain ones. A clip is sent
//! again unless a checked manifest (or the file's own header) shows it is
//! encrypted with the current key, so after a passphrase change nothing
//! under the old key is listed in new copies. Older copies keep their own
//! key information in their manifest and still open with the old
//! passphrase until they are removed (their clips may have been re-sent
//! under the new key by then).
//!
//! RESTORE (no web page needed, e.g. on a new computer):
//!
//! ```text
//! ambersight offsite-restore --from \\nas\backup --to C:\restored
//! ambersight offsite-restore --from s3://bucket/prefix --endpoint https://s3.example.com \
//!     --region us-east-1 --access-key KEY --to ./restored        (secret key: AMBERSIGHT_S3_SECRET_KEY)
//! ambersight offsite-restore --from s3 --config config.toml --to ./restored   (uses [offsite])
//! ambersight offsite-restore --from ... --list            shows the copies there
//! ambersight offsite-decrypt file.enc file                one file
//! ```
//!
//! The passphrase is asked for (or taken from AMBERSIGHT_OFFSITE_PASSPHRASE;
//! when that is set, or the copy is in S3, a copy that is not encrypted is
//! refused).
//! The newest copy is restored unless `--snapshot <time>` is given. `--to`
//! then holds config.toml, ambersight.db, clips/ and thumbs/: stop
//! AmberSight, put config.toml next to the program and the rest into the
//! data folder, start it again.

pub mod crypt;
pub mod http;
#[cfg(test)]
mod mock_s3;
pub mod restore;
pub mod s3;
pub mod store;

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use crate::config::OffsiteConfig;
use crypt::{KdfParams, MasterKey};
use store::Store;

pub const ENC: &str = ".enc";
const STAMP: &str = "%Y%m%d-%H%M%S";
/// After this many failed runs in a row, a system alert goes out.
const ALERT_AFTER: u32 = 3;
/// Clips changed this recently may still be being written: next time.
const SETTLE_SECS: u64 = 120;

/// A new file only the service account (and administrators) can read; the
/// folder it is in decides on Windows (see backup::private_dir).
pub fn create_private(path: &Path) -> std::io::Result<std::fs::File> {
    let mut o = std::fs::OpenOptions::new();
    o.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        o.mode(0o600);
    }
    o.open(path)
}

/// Which key files are encrypted with (key.json: the one in use now; each
/// manifest: the one that copy was made with). No secrets.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct KeyInfo {
    pub v: u32,
    pub salt: String,
    pub m: u32,
    pub t: u32,
    pub p: u32,
    pub check: String,
}

impl KeyInfo {
    pub fn of(k: &MasterKey) -> KeyInfo {
        KeyInfo { v: 1, salt: crypt::hex(&k.salt), m: k.params.m, t: k.params.t, p: k.params.p, check: k.check_value() }
    }

    /// The key `passphrase` gives with this salt; None when it is a
    /// different passphrase. Unreadable or oversized settings are an error.
    pub fn key(&self, passphrase: &str, keys: &mut crypt::KeyCache) -> Result<Option<MasterKey>> {
        let salt = crypt::unhex(&self.salt).and_then(|s| <[u8; 16]>::try_from(s).ok()).context("damaged key information")?;
        let k = keys.get(passphrase, salt, KdfParams { m: self.m, t: self.t, p: self.p })?;
        Ok((k.check_value() == self.check).then_some(k))
    }
}

/// Manifests written now (2: key information, check value, named files).
pub const MANIFEST_VERSION: u32 = 2;

/// The list of what one copy holds (`snapshots/<stamp>/manifest.json`).
#[derive(Debug, Serialize, Deserialize, Default, Clone)]
pub struct Manifest {
    pub version: u32,
    /// The copy's folder name (so a list moved to another copy is noticed).
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub stamp: String,
    /// Unix seconds.
    pub created: i64,
    pub server: String,
    pub encrypted: bool,
    /// The key this copy was made with (encrypted copies, version 2).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub key: Option<KeyInfo>,
    pub files: Vec<FileEntry>,
    pub media: Vec<MediaEntry>,
    /// HMAC over the rest (hex), with a key made from the passphrase.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub mac: String,
}

impl Manifest {
    fn signed_bytes(&self) -> Vec<u8> {
        let mut c = self.clone();
        c.mac.clear();
        serde_json::to_vec(&c).unwrap_or_default()
    }

    pub fn sign(&mut self, k: &MasterKey) {
        self.mac = k.mac(&self.signed_bytes());
    }

    /// Was this list made with key `k` and not changed since?
    pub fn verify(&self, k: &MasterKey) -> bool {
        !self.mac.is_empty() && k.mac_ok(&self.signed_bytes(), &self.mac)
    }
}

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct FileEntry {
    /// config.toml or ambersight.db
    pub name: String,
    pub key: String,
    pub size: u64,
    /// SHA-256 of the plain file; only for unencrypted copies (encrypted
    /// ones carry their own checks).
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub sha256: String,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct MediaEntry {
    pub key: String,
    /// Where it belongs in the data folder, e.g. clips/<id>.mp4.
    pub path: String,
    pub size: u64,
    /// Encrypted files: the key's id (its salt as hex).
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub kid: String,
    /// Encrypted files: their format (1: sent by an older version, not
    /// named; 2: named). 0 in old manifests.
    #[serde(default, skip_serializing_if = "is_zero")]
    pub fmt: u8,
}

fn is_zero(v: &u8) -> bool {
    *v == 0
}

/// Size of an encrypted file at the destination.
fn stored_len(plain: u64, fmt: u8, name: &str) -> u64 {
    if fmt == 1 {
        crypt::encrypted_len_v1(plain)
    } else {
        crypt::encrypted_len(plain, name)
    }
}

/// What a run did.
#[derive(Debug, Default)]
pub struct Report {
    pub stamp: String,
    pub bytes: u64,
    pub sent: u32,
    pub skipped: u32,
    pub deleted: u32,
}

/// Everything a run needs (kept apart from the engine so it can be tested).
pub struct Job<'a> {
    pub cfg: &'a OffsiteConfig,
    /// The settings as they are in use (Config::to_toml), not read from the
    /// file, which may be being rewritten at that moment.
    pub config_text: &'a str,
    pub db: &'a crate::db::Db,
    pub data_dir: &'a Path,
    pub store: &'a dyn Store,
    pub params: KdfParams,
    pub server: String,
    pub progress: &'a dyn Fn(&str),
}

fn safe_id(id: &str) -> bool {
    !id.is_empty() && id.len() < 200 && id.bytes().all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.')) && !id.starts_with('.')
}

/// The event a clip/picture key belongs to.
pub fn media_event(key: &str) -> Option<&str> {
    let k = key.strip_suffix(ENC).unwrap_or(key);
    if let Some(r) = k.strip_prefix("thumbs/ai/") {
        return r.split('/').next();
    }
    let name = k.strip_prefix("clips/").or_else(|| k.strip_prefix("thumbs/"))?;
    Some(name.rsplit_once('.').map(|(a, _)| a).unwrap_or(name))
}

/// The protected events' files to copy, as (path in the data folder, file, size).
fn media_files(job: &Job, protected: &[String]) -> Vec<(String, PathBuf, u64)> {
    let mut out = Vec::new();
    let settled = |m: &std::fs::Metadata| m.modified().ok().and_then(|t| t.elapsed().ok()).is_none_or(|e| e.as_secs() >= SETTLE_SECS);
    let mut add = |rel: String, p: PathBuf| {
        if let Ok(m) = std::fs::metadata(&p) {
            if m.is_file() && settled(&m) {
                out.push((rel, p, m.len()));
            }
        }
    };
    for id in protected.iter().filter(|i| safe_id(i)) {
        if job.cfg.clips {
            add(format!("clips/{id}.mp4"), job.data_dir.join("clips").join(format!("{id}.mp4")));
        }
        if job.cfg.pictures {
            add(format!("thumbs/{id}.jpg"), job.data_dir.join("thumbs").join(format!("{id}.jpg")));
            let dir = job.data_dir.join("thumbs").join("ai").join(id);
            let mut names: Vec<String> = std::fs::read_dir(&dir)
                .map(|rd| rd.flatten().map(|e| e.file_name().to_string_lossy().to_string()).filter(|n| safe_id(n)).collect())
                .unwrap_or_default();
            names.sort();
            for n in names {
                add(format!("thumbs/ai/{id}/{n}"), dir.join(&n));
            }
        }
    }
    out
}

/// The key to encrypt with (None: no passphrase, plain copies).
fn current_key(job: &Job) -> Result<Option<MasterKey>> {
    if job.cfg.passphrase.is_empty() {
        if job.cfg.kind == "s3" {
            bail!("S3 copies need an encryption passphrase");
        }
        return Ok(None);
    }
    if let Some(b) = job.store.get_bytes("key.json")? {
        if let Ok(ki) = serde_json::from_slice::<KeyInfo>(&b) {
            // Damaged or oversized settings there: start a new key.
            if let Ok(Some(k)) = ki.key(&job.cfg.passphrase, &mut crypt::KeyCache::default()) {
                return Ok(Some(k));
            }
        }
    }
    // First copy, or the passphrase changed. Older copies keep the key
    // information in their own manifests.
    let k = MasterKey::new_random(&job.cfg.passphrase, job.params)?;
    job.store.put_bytes("key.json", &serde_json::to_vec_pretty(&KeyInfo::of(&k))?)?;
    Ok(Some(k))
}

/// Clips and pictures the newest checked copy lists, by key. Only a
/// manifest signed with the current key counts.
fn trusted_media(job: &Job, key: &MasterKey) -> Result<HashMap<String, MediaEntry>> {
    let snaps = restore::list_snapshots(job.store)?;
    for s in snaps.iter().rev().take(3) {
        let Some(b) = job.store.get_bytes(&format!("snapshots/{s}/manifest.json"))? else { continue };
        let Ok(m) = serde_json::from_slice::<Manifest>(&b) else { continue };
        if m.version < 2 || m.key.as_ref().map(|k| k.salt.as_str()) != Some(key.id().as_str()) || !m.verify(key) {
            continue;
        }
        return Ok(m.media.into_iter().map(|e| (e.key.clone(), e)).collect());
    }
    Ok(HashMap::new())
}

/// Is the encrypted file `key_name` (`remote_size` bytes there) already this
/// clip (`size` plain bytes) under key `k`? Returns its format if so.
fn already_sent(job: &Job, k: &MasterKey, known: &HashMap<String, MediaEntry>, key_name: &str, size: u64, remote_size: u64) -> Option<u8> {
    if let Some(e) = known.get(key_name) {
        let fmt = if e.fmt == 0 { 2 } else { e.fmt };
        return (e.kid == k.id() && e.size == size && remote_size == stored_len(size, fmt, key_name)).then_some(fmt);
    }
    // Not in a checked list (sent by an older version, or by a run that
    // stopped early): look at the file's header for the key it used.
    let head = job.store.get_head(key_name, crypt::HEADER_LEN + 2 + key_name.len()).ok()??;
    let (fmt, salt, name) = crypt::peek(&head)?;
    let named_ok = fmt == 1 || name.as_deref() == Some(key_name);
    (salt == k.salt && named_ok && remote_size == stored_len(size, fmt, key_name)).then_some(fmt)
}

/// Make one off-site copy.
pub fn run_once(job: &Job) -> Result<Report> {
    let tmp = job.data_dir.join("offsite").join("tmp");
    let _ = std::fs::remove_dir_all(&tmp);
    crate::backup::private_dir(&tmp)?;
    let res = run_inner(job, &tmp);
    let _ = std::fs::remove_dir_all(&tmp);
    res
}

fn run_inner(job: &Job, tmp: &Path) -> Result<Report> {
    let mut rep = Report::default();
    let now = chrono::Utc::now();
    let mut stamp = now.format(STAMP).to_string();
    let existing: HashSet<String> = job.store.list("snapshots/")?.into_iter().filter_map(|(k, _)| k.split('/').nth(1).map(String::from)).collect();
    if existing.contains(&stamp) {
        // Two copies in the same second (a test, or a button pressed twice).
        std::thread::sleep(Duration::from_millis(1100));
        stamp = chrono::Utc::now().format(STAMP).to_string();
    }
    rep.stamp = stamp.clone();
    (job.progress)("Preparing");
    let key = current_key(job)?;
    let enc = key.is_some();
    let sfx = if enc { ENC } else { "" };

    // Settings and database: always.
    let mut man = Manifest {
        version: MANIFEST_VERSION,
        stamp: stamp.clone(),
        created: now.timestamp(),
        server: job.server.clone(),
        encrypted: enc,
        key: key.as_ref().map(KeyInfo::of),
        ..Default::default()
    };
    let db_copy = tmp.join("ambersight.db");
    (job.progress)("Copying the database");
    job.db.backup_to(&db_copy)?;
    let cfg_copy = tmp.join("config.toml");
    {
        use std::io::Write;
        let mut w = create_private(&cfg_copy)?;
        w.write_all(job.config_text.as_bytes())?;
    }
    for (name, plain) in [("config.toml", &cfg_copy), ("ambersight.db", &db_copy)] {
        (job.progress)(&format!("Sending {name}"));
        let key_name = format!("snapshots/{stamp}/{name}{sfx}");
        let (send, digest) = match &key {
            Some(k) => {
                let out = tmp.join(format!("{name}{ENC}"));
                (out.clone(), crypt::encrypt_file(k, &key_name, plain, &out)?)
            }
            None => (plain.to_path_buf(), crypt::copy_hashed(std::fs::File::open(plain)?, std::io::sink())?),
        };
        job.store.put_file(&key_name, &send).with_context(|| format!("sending {name}"))?;
        rep.bytes += std::fs::metadata(&send)?.len();
        rep.sent += 1;
        man.files.push(FileEntry { name: name.into(), key: key_name, size: digest.size, sha256: if enc { String::new() } else { digest.sha256 } });
        let _ = std::fs::remove_file(&send);
    }
    let _ = std::fs::remove_file(&db_copy);
    let _ = std::fs::remove_file(&cfg_copy);

    // Clips and pictures of protected events: only what is not there yet.
    let protected = job.db.protected_ids()?;
    let local = media_files(job, &protected);
    let mut failed: Vec<String> = Vec::new();
    if !local.is_empty() {
        let mut remote: HashMap<String, u64> = job.store.list("clips/")?.into_iter().collect();
        remote.extend(job.store.list("thumbs/")?);
        let known = match &key {
            Some(k) => trusted_media(job, k)?,
            None => HashMap::new(),
        };
        let kid = key.as_ref().map(|k| k.id()).unwrap_or_default();
        let total = local.len();
        for (i, (rel, path, size)) in local.into_iter().enumerate() {
            let key_name = format!("{rel}{sfx}");
            if let Some(&rs) = remote.get(&key_name) {
                // Skipped only if it is there under the current key (or
                // plain, for plain copies).
                let there = match &key {
                    Some(k) => already_sent(job, k, &known, &key_name, size, rs),
                    None => (rs == size).then_some(0),
                };
                if let Some(fmt) = there {
                    rep.skipped += 1;
                    man.media.push(MediaEntry { key: key_name, path: rel, size, kid: kid.clone(), fmt });
                    continue;
                }
            }
            if i % 10 == 0 {
                (job.progress)(&format!("Sending clips and pictures ({} of {total})", i + 1));
            }
            let res = (|| -> Result<u64> {
                match &key {
                    Some(k) => {
                        let out = tmp.join("media.enc");
                        crypt::encrypt_file(k, &key_name, &path, &out)?;
                        job.store.put_file(&key_name, &out)?;
                        let n = std::fs::metadata(&out)?.len();
                        let _ = std::fs::remove_file(&out);
                        Ok(n)
                    }
                    None => {
                        job.store.put_file(&key_name, &path)?;
                        Ok(size)
                    }
                }
            })();
            match res {
                Ok(n) => {
                    rep.bytes += n;
                    rep.sent += 1;
                    man.media.push(MediaEntry { key: key_name, path: rel, size, kid: kid.clone(), fmt: if enc { 2 } else { 0 } });
                }
                Err(e) => {
                    tracing::warn!("off-site backup: {rel}: {e:#}");
                    failed.push(format!("{rel}: {e:#}"));
                    // Nothing is getting through: stop trying the rest.
                    if failed.len() >= 5 && rep.sent <= 2 {
                        break;
                    }
                }
            }
        }
    }

    // The list goes last: a copy without it is unfinished.
    (job.progress)("Finishing");
    if let Some(k) = &key {
        man.sign(k);
    }
    job.store.put_bytes(&format!("snapshots/{stamp}/manifest.json"), &serde_json::to_vec_pretty(&man)?)?;

    (job.progress)("Removing old copies");
    rep.deleted = prune(job, &stamp, &protected)?;
    if !failed.is_empty() {
        bail!("{} clip{} or picture{} could not be copied (first: {})", failed.len(), if failed.len() == 1 { "" } else { "s" }, if failed.len() == 1 { "" } else { "s" }, failed[0]);
    }
    Ok(rep)
}

/// Copies to remove: (stamps kept, newest first; keys to delete). A copy
/// without its manifest is unfinished and goes too (not the one being made).
pub fn plan_prune(snapshot_keys: &[(String, u64)], keep: usize, current: &str) -> (Vec<String>, Vec<String>) {
    let mut by_stamp: HashMap<&str, Vec<&str>> = HashMap::new();
    for (k, _) in snapshot_keys {
        if let Some(s) = k.strip_prefix("snapshots/").and_then(|r| r.split('/').next()) {
            by_stamp.entry(s).or_default().push(k);
        }
    }
    let mut complete: Vec<&str> = by_stamp.iter().filter(|(_, ks)| ks.iter().any(|k| k.ends_with("/manifest.json"))).map(|(s, _)| *s).collect();
    complete.sort_unstable_by(|a, b| b.cmp(a));
    let kept: Vec<String> = complete.iter().take(keep.max(1)).map(|s| s.to_string()).collect();
    let mut delete: Vec<String> = by_stamp
        .iter()
        .filter(|(s, _)| !kept.iter().any(|k| k == *s) && **s != current)
        .flat_map(|(_, ks)| ks.iter().map(|k| k.to_string()))
        .collect();
    // The manifest last, so a copy cut off half-way is still seen as one.
    delete.sort_by_key(|k| (k.ends_with("/manifest.json"), k.clone()));
    (kept, delete)
}

/// Clips and pictures no kept copy lists and whose event is not protected here.
pub fn media_to_delete(remote: &[(String, u64)], referenced: &HashSet<String>, protected: &HashSet<String>) -> Vec<String> {
    remote
        .iter()
        .filter(|(k, _)| !referenced.contains(k))
        .filter(|(k, _)| !media_event(k).is_some_and(|id| protected.contains(id)))
        .map(|(k, _)| k.clone())
        .collect()
}

fn prune(job: &Job, current: &str, protected: &[String]) -> Result<u32> {
    let mut n = 0;
    let (kept, delete) = plan_prune(&job.store.list("snapshots/")?, job.cfg.keep as usize, current);
    for k in &delete {
        job.store.delete(k)?;
        n += 1;
    }
    // Which clips the kept copies still list. If one cannot be read, leave
    // every clip alone this time.
    let mut referenced = HashSet::new();
    for s in &kept {
        let Some(b) = job.store.get_bytes(&format!("snapshots/{s}/manifest.json"))? else { return Ok(n) };
        let Ok(m) = serde_json::from_slice::<Manifest>(&b) else { return Ok(n) };
        referenced.extend(m.media.into_iter().map(|e| e.key));
    }
    let mut remote = job.store.list("clips/")?;
    remote.extend(job.store.list("thumbs/")?);
    let protected: HashSet<String> = protected.iter().cloned().collect();
    for k in media_to_delete(&remote, &referenced, &protected) {
        job.store.delete(&k)?;
        n += 1;
    }
    Ok(n)
}

/// Try the destination: write, read back, list and remove a small file.
pub fn test_connection(cfg: &OffsiteConfig) -> Result<String> {
    let st = store::open(cfg)?;
    let key = format!("ambersight-test-{}.txt", crypt::hex(&rand::random::<[u8; 6]>()));
    let body = b"AmberSight connection test - safe to delete";
    st.put_bytes(&key, body).context("could not write a test file")?;
    let back = st.get_bytes(&key).context("could not read the test file back")?;
    let listed = st.list(&key).context("could not list files")?;
    st.delete(&key).context("could not remove the test file")?;
    if back.as_deref() != Some(&body[..]) {
        bail!("the test file came back different");
    }
    if !listed.iter().any(|(k, _)| k == &key) {
        bail!("the test file was written but does not show in the list");
    }
    Ok(format!("Connected to {}: a test file was written, read back and removed.", st.describe()))
}

/// Settings problems that stop a copy (or saving the settings turned on).
pub fn check_settings(cfg: &OffsiteConfig) -> Result<(), String> {
    match cfg.kind.as_str() {
        "folder" => {
            if cfg.folder.trim().is_empty() {
                return Err("choose the folder to copy to".into());
            }
        }
        "s3" => {
            if cfg.s3_bucket.trim().is_empty() {
                return Err("enter the bucket name".into());
            }
            if cfg.s3_access_key.trim().is_empty() || cfg.s3_secret_key.is_empty() {
                return Err("enter the access key and secret key".into());
            }
            if cfg.passphrase.is_empty() {
                return Err("cloud copies need an encryption passphrase (the settings hold camera passwords)".into());
            }
        }
        _ => return Err("choose a folder or S3 storage".into()),
    }
    Ok(())
}

// ---------------------------------------------------------------- status

/// What the page shows. Saved in `<data_dir>/offsite/status.json`.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Status {
    #[serde(skip)]
    pub running: bool,
    #[serde(skip)]
    pub phase: String,
    pub last_start: i64,
    pub last_end: i64,
    pub last_ok: Option<bool>,
    pub last_error: String,
    pub last_bytes: u64,
    pub last_sent: u32,
    pub last_skipped: u32,
    pub last_success: i64,
    /// Failed runs in a row.
    pub failures: u32,
}

static STATUS: Mutex<Option<Status>> = Mutex::new(None);
static RUN_NOW: AtomicBool = AtomicBool::new(false);
static STARTED: std::sync::OnceLock<i64> = std::sync::OnceLock::new();

pub fn status() -> Status {
    STATUS.lock().unwrap().clone().unwrap_or_default()
}

fn set_status(data_dir: &Path, f: impl FnOnce(&mut Status)) {
    let mut g = STATUS.lock().unwrap();
    let s = g.get_or_insert_with(Status::default);
    f(s);
    let dir = data_dir.join("offsite");
    if std::fs::create_dir_all(&dir).is_ok() {
        let _ = std::fs::write(dir.join("status.json"), serde_json::to_vec_pretty(s).unwrap_or_default());
    }
}

/// When the next copy is due (unix seconds; None: turned off).
pub fn next_run(cfg: &OffsiteConfig, st: &Status, started: i64) -> Option<i64> {
    if !cfg.enabled {
        return None;
    }
    let hours = i64::from(cfg.every_hours.max(1)) * 3600;
    Some(if st.last_end == 0 {
        // First ever: shortly after start, once cameras are going.
        started + 600
    } else if st.last_ok == Some(false) {
        st.last_end + hours.min(3600)
    } else {
        st.last_end + hours
    })
}

/// The health problem to report (and so a system alert) after repeated failures.
pub fn problem(cfg: &OffsiteConfig) -> Option<String> {
    let st = status();
    (cfg.enabled && st.failures >= ALERT_AFTER).then(|| {
        let e: String = st.last_error.chars().take(160).collect();
        format!("off-site backup failed {} times in a row ({e})", st.failures)
    })
}

/// Ask the background thread for a copy now. False if one is running.
pub fn request_run() -> bool {
    if status().running {
        return false;
    }
    RUN_NOW.store(true, Ordering::SeqCst);
    true
}

pub fn started_at() -> i64 {
    *STARTED.get().unwrap_or(&0)
}

fn run_with_status(engine: &crate::engine::Engine, cfg: &OffsiteConfig) {
    let data_dir = engine.data_dir.clone();
    let t0 = chrono::Utc::now().timestamp();
    set_status(&data_dir, |s| {
        s.running = true;
        s.phase = "Starting".into();
        s.last_start = t0;
    });
    let progress = |p: &str| {
        let mut g = STATUS.lock().unwrap();
        if let Some(s) = g.as_mut() {
            s.phase = p.to_string();
        }
    };
    let res = check_settings(cfg).map_err(|e| anyhow::anyhow!(e)).and_then(|_| store::open(cfg)).and_then(|st| {
        let text = engine.config.lock().unwrap().to_toml()?;
        let job = Job {
            cfg,
            config_text: &text,
            db: &engine.db,
            data_dir: &data_dir,
            store: st.as_ref(),
            params: crypt::DEFAULT_PARAMS,
            server: crate::links::own_name(engine),
            progress: &progress,
        };
        run_once(&job)
    });
    let t1 = chrono::Utc::now().timestamp();
    match &res {
        Ok(r) => tracing::info!(
            "off-site backup {} done: {} file(s), {:.1} MB sent, {} already there, {} old file(s) removed",
            r.stamp,
            r.sent,
            r.bytes as f64 / 1e6,
            r.skipped,
            r.deleted
        ),
        Err(e) => tracing::warn!("off-site backup failed: {e:#}"),
    }
    set_status(&data_dir, |s| {
        s.running = false;
        s.phase.clear();
        s.last_end = t1;
        match &res {
            Ok(r) => {
                s.last_ok = Some(true);
                s.last_error.clear();
                s.last_bytes = r.bytes;
                s.last_sent = r.sent;
                s.last_skipped = r.skipped;
                s.last_success = t1;
                s.failures = 0;
            }
            Err(e) => {
                s.last_ok = Some(false);
                s.last_error = format!("{e:#}");
                s.failures += 1;
            }
        }
    });
}

/// Background thread: a copy whenever one is due or asked for.
pub fn start(engine: Arc<crate::engine::Engine>) {
    let _ = STARTED.set(chrono::Utc::now().timestamp());
    if let Ok(b) = std::fs::read(engine.data_dir.join("offsite").join("status.json")) {
        if let Ok(s) = serde_json::from_slice::<Status>(&b) {
            *STATUS.lock().unwrap() = Some(s);
        }
    }
    std::thread::Builder::new()
        .name("offsite".into())
        .spawn(move || loop {
            std::thread::sleep(Duration::from_secs(5));
            let cfg = engine.config.lock().unwrap().offsite.clone();
            let asked = RUN_NOW.swap(false, Ordering::SeqCst);
            let due = next_run(&cfg, &status(), started_at()).is_some_and(|t| chrono::Utc::now().timestamp() >= t);
            if asked || due {
                run_with_status(&engine, &cfg);
            }
        })
        .ok();
}

#[cfg(test)]
mod tests {
    use super::*;

    fn k(s: &str) -> (String, u64) {
        (s.to_string(), 1)
    }

    #[test]
    fn prune_plan() {
        let keys = vec![
            k("snapshots/20261101-000000/config.toml.enc"),
            k("snapshots/20261101-000000/manifest.json"),
            k("snapshots/20261102-000000/config.toml.enc"),
            k("snapshots/20261102-000000/manifest.json"),
            k("snapshots/20261102-120000/config.toml.enc"), // unfinished
            k("snapshots/20261103-000000/ambersight.db.enc"),
            k("snapshots/20261103-000000/manifest.json"),
        ];
        let (kept, mut del) = plan_prune(&keys, 2, "20261103-000000");
        assert_eq!(kept, ["20261103-000000", "20261102-000000"]);
        assert_eq!(del.last().unwrap(), "snapshots/20261101-000000/manifest.json");
        del.sort();
        assert_eq!(del, ["snapshots/20261101-000000/config.toml.enc", "snapshots/20261101-000000/manifest.json", "snapshots/20261102-120000/config.toml.enc"]);
        // The copy being made is never removed, even unfinished.
        let (_, del) = plan_prune(&keys, 5, "20261102-120000");
        assert!(del.is_empty());
    }

    #[test]
    fn media_prune() {
        assert_eq!(media_event("clips/abc-1.mp4.enc"), Some("abc-1"));
        assert_eq!(media_event("thumbs/abc.jpg"), Some("abc"));
        assert_eq!(media_event("thumbs/ai/abc/3.jpg.enc"), Some("abc"));
        let remote = vec![k("clips/a.mp4.enc"), k("clips/b.mp4.enc"), k("clips/c.mp4.enc"), k("thumbs/ai/d/1.jpg.enc")];
        let referenced: HashSet<String> = ["clips/a.mp4.enc".to_string()].into();
        let protected: HashSet<String> = ["b".to_string()].into();
        assert_eq!(media_to_delete(&remote, &referenced, &protected), ["clips/c.mp4.enc", "thumbs/ai/d/1.jpg.enc"]);
    }

    #[test]
    fn schedule() {
        let mut cfg = OffsiteConfig { enabled: true, every_hours: 24, ..Default::default() };
        let mut st = Status::default();
        assert_eq!(next_run(&cfg, &st, 1000), Some(1600));
        st.last_end = 10_000;
        st.last_ok = Some(true);
        assert_eq!(next_run(&cfg, &st, 0), Some(10_000 + 86_400));
        st.last_ok = Some(false);
        assert_eq!(next_run(&cfg, &st, 0), Some(10_000 + 3600));
        cfg.enabled = false;
        assert_eq!(next_run(&cfg, &st, 0), None);
    }

    #[test]
    fn settings_checks() {
        let mut c = OffsiteConfig { kind: "s3".into(), s3_bucket: "b".into(), s3_access_key: "a".into(), s3_secret_key: "s".into(), ..Default::default() };
        assert!(check_settings(&c).unwrap_err().contains("passphrase"));
        c.passphrase = "long enough".into();
        assert!(check_settings(&c).is_ok());
        c.kind = "folder".into();
        assert!(check_settings(&c).is_err());
    }

    /// Folder destination, start to finish: copies, encryption, skipping
    /// clips already there, pruning, and a restore.
    #[test]
    fn folder_end_to_end() {
        let tmp = std::env::temp_dir().join(format!("ambs-offsite-e2e-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        let data = tmp.join("data");
        std::fs::create_dir_all(data.join("clips")).unwrap();
        std::fs::create_dir_all(data.join("thumbs").join("ai").join("ev1")).unwrap();
        let cfg_path = tmp.join("config.toml");
        std::fs::write(&cfg_path, "[server]\nport = 1\n# fake test config\n").unwrap();
        let cfg_text = std::fs::read_to_string(&cfg_path).unwrap();
        let db = crate::db::Db::open(&data.join("ambersight.db")).unwrap();
        for id in ["ev1", "ev2"] {
            db.insert_event(&crate::engine::Event { id: id.into(), camera_id: 0, camera: "Cam".into(), start: 1, end: 2, score: 0.5, clip: None, thumb: None, objects: vec![], protect: false }).unwrap();
        }
        db.set_protect("ev1", true).unwrap();
        db.set_protect("ev2", true).unwrap();
        // Old enough to count as finished.
        let old = |p: &Path, body: &[u8]| {
            std::fs::write(p, body).unwrap();
            let f = std::fs::File::options().write(true).open(p).unwrap();
            f.set_modified(std::time::SystemTime::now() - Duration::from_secs(600)).unwrap();
        };
        old(&data.join("clips").join("ev1.mp4"), &vec![9u8; 3_000_000]);
        old(&data.join("thumbs").join("ev1.jpg"), b"jpeg1");
        old(&data.join("thumbs").join("ai").join("ev1").join("0.jpg"), b"ai0");
        old(&data.join("clips").join("ev2.mp4"), b"clip2");

        let dest = tmp.join("dest");
        let cfg = OffsiteConfig { enabled: true, kind: "folder".into(), folder: dest.to_string_lossy().into(), keep: 2, passphrase: "test passphrase".into(), ..Default::default() };
        let st = store::open(&cfg).unwrap();
        let job = Job { cfg: &cfg, config_text: &cfg_text, db: &db, data_dir: &data, store: st.as_ref(), params: crypt::TEST_PARAMS, server: "test".into(), progress: &|_| {} };
        let r1 = run_once(&job).unwrap();
        assert_eq!((r1.sent, r1.skipped), (6, 0));
        assert!(dest.join("clips").join("ev1.mp4.enc").is_file());
        // Nothing readable leaves the machine.
        let cfg_enc = std::fs::read(dest.join("snapshots").join(&r1.stamp).join("config.toml.enc")).unwrap();
        assert!(crypt::is_encrypted(&cfg_enc));
        assert!(!String::from_utf8_lossy(&cfg_enc).contains("fake test config"));

        // Second run: clips already there are not sent again.
        let r2 = run_once(&job).unwrap();
        assert_eq!((r2.sent, r2.skipped), (2, 4));

        // ev2 no longer protected: its clip stays while a kept copy lists it.
        db.set_protect("ev2", false).unwrap();
        run_once(&job).unwrap();
        assert!(dest.join("clips").join("ev2.mp4.enc").is_file());
        run_once(&job).unwrap();
        // keep = 2: neither kept copy lists it now.
        assert!(!dest.join("clips").join("ev2.mp4.enc").exists());
        assert!(dest.join("clips").join("ev1.mp4.enc").is_file());
        let snaps = restore::list_snapshots(st.as_ref()).unwrap();
        assert_eq!(snaps.len(), 2);

        // Restore the newest copy.
        let out = tmp.join("restored");
        let n = restore::restore(st.as_ref(), &out, None, true, false, &mut || Ok("test passphrase".into()), &|_| {}).unwrap();
        assert_eq!(n, 5);
        assert_eq!(std::fs::read_to_string(out.join("config.toml")).unwrap(), "[server]\nport = 1\n# fake test config\n");
        assert_eq!(std::fs::read(out.join("clips").join("ev1.mp4")).unwrap(), vec![9u8; 3_000_000]);
        assert_eq!(std::fs::read(out.join("thumbs").join("ai").join("ev1").join("0.jpg")).unwrap(), b"ai0");
        let rdb = crate::db::Db::open(&out.join("ambersight.db")).unwrap();
        assert_eq!(rdb.protected_ids().unwrap(), ["ev1"]);
        // A wrong passphrase restores nothing usable.
        let bad = restore::restore(st.as_ref(), &tmp.join("bad"), None, true, false, &mut || Ok("nope".into()), &|_| {});
        assert!(format!("{:#}", bad.unwrap_err()).contains("wrong passphrase"));

        // A changed passphrase, and the first run with it stops right after
        // writing key.json (network gone). The next run must still send
        // every clip again under the new key, not skip the old-key ones.
        let old_stamp = snaps.last().unwrap().clone();
        let cfg2 = OffsiteConfig { passphrase: "another passphrase".into(), ..cfg.clone() };
        let job2 = Job { cfg: &cfg2, ..job };
        current_key(&job2).unwrap();
        let r = run_once(&job2).unwrap();
        assert_eq!(r.skipped, 0);
        let out2 = tmp.join("restored2");
        restore::restore(st.as_ref(), &out2, None, true, false, &mut || Ok("another passphrase".into()), &|_| {}).unwrap();
        assert_eq!(std::fs::read(out2.join("thumbs").join("ev1.jpg")).unwrap(), b"jpeg1");
        assert_eq!(std::fs::read(out2.join("clips").join("ev1.mp4")).unwrap(), vec![9u8; 3_000_000]);
        // The older copy still opens with the old passphrase (its manifest
        // keeps its key information; key.json now names the new key).
        let out3 = tmp.join("restored3");
        restore::restore(st.as_ref(), &out3, Some(&old_stamp), false, false, &mut || Ok("test passphrase".into()), &|_| {}).unwrap();
        assert_eq!(std::fs::read_to_string(out3.join("config.toml")).unwrap(), "[server]\nport = 1\n# fake test config\n");
        // Then everything is under the new key: skipped from now on.
        let r = run_once(&job2).unwrap();
        assert_eq!((r.sent, r.skipped), (2, 3));
        let _ = std::fs::remove_dir_all(&tmp);
    }

    /// Someone with write access to the destination tries to slip things
    /// into a restore: a changed list, a plain config.toml, swapped files.
    #[test]
    fn restore_refuses_tampering() {
        let tmp = std::env::temp_dir().join(format!("ambs-offsite-tamper-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        let data = tmp.join("data");
        std::fs::create_dir_all(data.join("clips")).unwrap();
        std::fs::create_dir_all(data.join("thumbs")).unwrap();
        let db = crate::db::Db::open(&data.join("ambersight.db")).unwrap();
        db.insert_event(&crate::engine::Event { id: "e1".into(), camera_id: 0, camera: "Cam".into(), start: 1, end: 2, score: 0.5, clip: None, thumb: None, objects: vec![], protect: true }).unwrap();
        for (p, body) in [(data.join("clips").join("e1.mp4"), &b"clip-bytes"[..]), (data.join("thumbs").join("e1.jpg"), &b"jpeg"[..])] {
            std::fs::write(&p, body).unwrap();
            std::fs::File::options().write(true).open(&p).unwrap().set_modified(std::time::SystemTime::now() - Duration::from_secs(600)).unwrap();
        }
        let dest = tmp.join("dest");
        let cfg = OffsiteConfig { enabled: true, kind: "folder".into(), folder: dest.to_string_lossy().into(), keep: 5, passphrase: "test passphrase".into(), ..Default::default() };
        let st = store::open(&cfg).unwrap();
        let job = Job { cfg: &cfg, config_text: "[server]\nport = 3\n", db: &db, data_dir: &data, store: st.as_ref(), params: crypt::TEST_PARAMS, server: "t".into(), progress: &|_| {} };
        let r = run_once(&job).unwrap();
        let man_key = format!("snapshots/{}/manifest.json", r.stamp);
        let good: Manifest = serde_json::from_slice(&st.get_bytes(&man_key).unwrap().unwrap()).unwrap();
        assert!(good.mac.len() == 64 && good.key.is_some());
        let key_of = |m: &Manifest| m.key.as_ref().unwrap().key("test passphrase", &mut crypt::KeyCache::default()).unwrap().unwrap();
        let pass = || -> Box<dyn FnMut() -> Result<String>> { Box::new(|| Ok("test passphrase".into())) };
        let n = std::sync::atomic::AtomicUsize::new(0);
        let attempt = |m: &Manifest, must: bool| {
            st.put_bytes(&man_key, &serde_json::to_vec(m).unwrap()).unwrap();
            let out = tmp.join(format!("out{}", n.fetch_add(1, Ordering::SeqCst)));
            restore::restore(st.as_ref(), &out, None, true, must, &mut pass(), &|_| {}).map_err(|e| format!("{e:#}"))
        };
        assert_eq!(attempt(&good, false).unwrap(), 4);

        // A plain config.toml put in, the list changed to point at it.
        st.put_bytes("evil/config.toml", b"[alerts]\naction_command = \"evil\"\n").unwrap();
        let mut m = good.clone();
        m.files[0].key = "evil/config.toml".into();
        m.files[0].size = 34;
        assert!(attempt(&m, false).unwrap_err().contains("does not check out"));
        // The same, made to look like an old unencrypted copy (no key, no
        // check value): refused, because key.json is there.
        let mut m = m.clone();
        (m.version, m.encrypted, m.key, m.mac) = (1, false, None, String::new());
        m.media.clear();
        m.files.truncate(1);
        assert!(attempt(&m, false).unwrap_err().contains("not encrypted"));
        // Even with key.json gone, when restoring from S3 or with the
        // passphrase given up front.
        st.delete("key.json").unwrap();
        assert!(attempt(&m, true).unwrap_err().contains("not encrypted"));
        st.put_bytes("key.json", b"{}").unwrap();
        // A changed size, server name or extra entry: refused.
        let mut m = good.clone();
        m.server = "other".into();
        assert!(attempt(&m, false).unwrap_err().contains("does not check out"));
        // The list moved to another copy's folder.
        let mut m = good.clone();
        m.stamp = "20200101-000000".into();
        m.sign(&key_of(&good));
        assert!(attempt(&m, false).unwrap_err().contains("does not check out"));

        // The clip copied over the picture: the picture fails (and no bad
        // file is left behind), the other files are still restored.
        let clip = st.get_bytes("clips/e1.mp4.enc").unwrap().unwrap();
        let mut m = good.clone();
        let thumb = m.media.iter_mut().find(|e| e.key == "thumbs/e1.jpg.enc").unwrap();
        thumb.size = 10;
        m.sign(&key_of(&good));
        st.put_bytes("thumbs/e1.jpg.enc", &clip).unwrap();
        let before = n.load(Ordering::SeqCst);
        let err = attempt(&m, false).unwrap_err();
        assert!(err.contains("1 of 4 files could not be restored (3 restored)") && err.contains("moved or swapped"), "{err}");
        let out = tmp.join(format!("out{before}"));
        assert_eq!(std::fs::read(out.join("clips").join("e1.mp4")).unwrap(), b"clip-bytes");
        assert!(!out.join("thumbs").join("e1.jpg").exists());
        assert!(!out.join("thumbs").join("e1.jpg.part").exists());
        assert_eq!(std::fs::read_to_string(out.join("config.toml")).unwrap(), "[server]\nport = 3\n");
        let _ = std::fs::remove_dir_all(&tmp);
    }

    /// Copies made by the previous version (manifest 1, files without
    /// names) still restore, and their clips are not sent again.
    #[test]
    fn old_format_still_restores() {
        let tmp = std::env::temp_dir().join(format!("ambs-offsite-old-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        let data = tmp.join("data");
        std::fs::create_dir_all(data.join("clips")).unwrap();
        let db = crate::db::Db::open(&data.join("ambersight.db")).unwrap();
        db.insert_event(&crate::engine::Event { id: "e1".into(), camera_id: 0, camera: "Cam".into(), start: 1, end: 2, score: 0.5, clip: None, thumb: None, objects: vec![], protect: true }).unwrap();
        let clip = data.join("clips").join("e1.mp4");
        std::fs::write(&clip, b"old clip").unwrap();
        std::fs::File::options().write(true).open(&clip).unwrap().set_modified(std::time::SystemTime::now() - Duration::from_secs(600)).unwrap();
        let dest = tmp.join("dest");
        let cfg = OffsiteConfig { enabled: true, kind: "folder".into(), folder: dest.to_string_lossy().into(), keep: 5, passphrase: "test passphrase".into(), ..Default::default() };
        let st = store::open(&cfg).unwrap();
        // What the previous version left there.
        let k = MasterKey::new_random("test passphrase", crypt::TEST_PARAMS).unwrap();
        st.put_bytes("key.json", &serde_json::to_vec(&KeyInfo::of(&k)).unwrap()).unwrap();
        let old = |plain: &[u8]| {
            let mut v = Vec::new();
            crypt::encrypt_old(&k, plain, &mut v).unwrap();
            v
        };
        st.put_bytes("snapshots/20260101-000000/config.toml.enc", &old(b"[server]\n")).unwrap();
        st.put_bytes("clips/e1.mp4.enc", &old(b"old clip")).unwrap();
        let man = serde_json::json!({ "version": 1, "created": 1, "server": "s", "encrypted": true,
            "files": [{ "name": "config.toml", "key": "snapshots/20260101-000000/config.toml.enc", "size": 9 }],
            "media": [{ "key": "clips/e1.mp4.enc", "path": "clips/e1.mp4", "size": 8 }] });
        st.put_bytes("snapshots/20260101-000000/manifest.json", man.to_string().as_bytes()).unwrap();
        let out = tmp.join("out");
        assert_eq!(restore::restore(st.as_ref(), &out, None, true, true, &mut || Ok("test passphrase".into()), &|_| {}).unwrap(), 2);
        assert_eq!(std::fs::read(out.join("clips").join("e1.mp4")).unwrap(), b"old clip");
        // A new run finds the clip there under the same key (from its header).
        let job = Job { cfg: &cfg, config_text: "[server]\n", db: &db, data_dir: &data, store: st.as_ref(), params: crypt::TEST_PARAMS, server: "t".into(), progress: &|_| {} };
        let r = run_once(&job).unwrap();
        assert_eq!((r.sent, r.skipped), (2, 1));
        let m: Manifest = serde_json::from_slice(&st.get_bytes(&format!("snapshots/{}/manifest.json", r.stamp)).unwrap().unwrap()).unwrap();
        assert_eq!(m.media[0].fmt, 1);
        let out2 = tmp.join("out2");
        restore::restore(st.as_ref(), &out2, None, true, false, &mut || Ok("test passphrase".into()), &|_| {}).unwrap();
        assert_eq!(std::fs::read(out2.join("clips").join("e1.mp4")).unwrap(), b"old clip");
        let _ = std::fs::remove_dir_all(&tmp);
    }

    /// S3 destination (an in-process stand-in that checks signatures):
    /// backup, skipping, pruning and restore; the database goes in parts.
    #[test]
    fn s3_end_to_end() {
        let tmp = std::env::temp_dir().join(format!("ambs-offsite-s3-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        let data = tmp.join("data");
        std::fs::create_dir_all(data.join("clips")).unwrap();
        let cfg_path = tmp.join("config.toml");
        std::fs::write(&cfg_path, "[server]\nport = 2\n").unwrap();
        let cfg_text = std::fs::read_to_string(&cfg_path).unwrap();
        let db = crate::db::Db::open(&data.join("ambersight.db")).unwrap();
        db.insert_event(&crate::engine::Event { id: "e1".into(), camera_id: 0, camera: "Cam".into(), start: 1, end: 2, score: 0.5, clip: None, thumb: None, objects: vec![], protect: true }).unwrap();
        let clip = data.join("clips").join("e1.mp4");
        std::fs::write(&clip, vec![5u8; 50_000]).unwrap();
        std::fs::File::options().write(true).open(&clip).unwrap().set_modified(std::time::SystemTime::now() - Duration::from_secs(600)).unwrap();

        let (port, mock) = mock_s3::start();
        let mut cfg = s3::tests::mock_cfg(port);
        cfg.enabled = true;
        cfg.keep = 1;
        // No passphrase: refused.
        let st = s3::S3::new(&cfg).unwrap().with_parts(30_000, 20_000);
        let job = Job { cfg: &cfg, config_text: &cfg_text, db: &db, data_dir: &data, store: &st, params: crypt::TEST_PARAMS, server: "t".into(), progress: &|_| {} };
        assert!(run_once(&job).unwrap_err().to_string().contains("passphrase"));
        let cfg2 = OffsiteConfig { passphrase: "cloud passphrase".into(), ..cfg.clone() };
        let job = Job { cfg: &cfg2, ..job };
        let r1 = run_once(&job).unwrap();
        assert_eq!((r1.sent, r1.skipped), (3, 0));
        assert!(mock.lock().unwrap().multipart_done >= 1);
        let r2 = run_once(&job).unwrap();
        assert_eq!((r2.sent, r2.skipped), (2, 1));
        assert_eq!(mock.lock().unwrap().range_gets, 0);
        // keep = 1: only the newest copy is left.
        assert_eq!(restore::list_snapshots(&st).unwrap(), [r2.stamp.clone()]);
        // Its list gone (a run cut off): the clip's own header (a ranged
        // read) shows it is under the current key, so it is not sent again.
        mock.lock().unwrap().objects.retain(|k, _| !k.ends_with("/manifest.json"));
        let r2 = run_once(&job).unwrap();
        assert_eq!((r2.sent, r2.skipped), (2, 1));
        assert_eq!(mock.lock().unwrap().range_gets, 1);
        assert_eq!(restore::list_snapshots(&st).unwrap(), [r2.stamp.clone()]);
        {
            let m = mock.lock().unwrap();
            assert_eq!(m.bad_signatures, 0);
            // Nothing readable in the cloud.
            for (k, v) in &m.objects {
                if k.ends_with(ENC) {
                    assert!(crypt::is_encrypted(v), "{k}");
                }
            }
            assert!(!m.objects.values().any(|v| String::from_utf8_lossy(v).contains("port = 2")));
        }
        let out = tmp.join("restored");
        let n = restore::restore(&st, &out, None, true, true, &mut || Ok("cloud passphrase".into()), &|_| {}).unwrap();
        assert_eq!(n, 3);
        assert_eq!(std::fs::read(out.join("clips").join("e1.mp4")).unwrap(), vec![5u8; 50_000]);
        assert_eq!(std::fs::read_to_string(out.join("config.toml")).unwrap(), "[server]\nport = 2\n");
        let _ = std::fs::remove_dir_all(&tmp);
    }

    /// Without a passphrase a folder gets plain files (with checksums).
    #[test]
    fn folder_plain() {
        let tmp = std::env::temp_dir().join(format!("ambs-offsite-plain-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        let data = tmp.join("data");
        std::fs::create_dir_all(&data).unwrap();
        let cfg_path = tmp.join("config.toml");
        std::fs::write(&cfg_path, "[server]\n").unwrap();
        let cfg_text = std::fs::read_to_string(&cfg_path).unwrap();
        let db = crate::db::Db::open(&data.join("ambersight.db")).unwrap();
        let cfg = OffsiteConfig { enabled: true, kind: "folder".into(), folder: tmp.join("dest").to_string_lossy().into(), ..Default::default() };
        let st = store::open(&cfg).unwrap();
        let job = Job { cfg: &cfg, config_text: &cfg_text, db: &db, data_dir: &data, store: st.as_ref(), params: crypt::TEST_PARAMS, server: "t".into(), progress: &|_| {} };
        let r = run_once(&job).unwrap();
        assert_eq!(std::fs::read_to_string(tmp.join("dest").join("snapshots").join(&r.stamp).join("config.toml")).unwrap(), "[server]\n");
        let out = tmp.join("out");
        restore::restore(st.as_ref(), &out, None, true, false, &mut || panic!("no passphrase needed"), &|_| {}).unwrap();
        assert!(out.join("ambersight.db").is_file());
        assert!(test_connection(&cfg).unwrap().starts_with("Connected"));
        let _ = std::fs::remove_dir_all(&tmp);
    }
}
