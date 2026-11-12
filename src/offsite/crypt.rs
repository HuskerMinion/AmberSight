//! Encryption of off-site copies.
//!
//! A passphrase is turned into a 32-byte key with Argon2id (slow on purpose,
//! so guessing passphrases is expensive). Each file gets its own key, made
//! from that key and 16 random bytes stored in the file, and is encrypted
//! with AES-256-GCM in 1 MB pieces, so files of any size are handled without
//! loading them into memory. Every piece carries a check value: a wrong
//! passphrase, a changed byte, pieces swapped around or a file cut short are
//! all detected.
//!
//! File layout (all numbers little-endian):
//!
//! ```text
//! "AMBSENC1"            8 bytes  marks an AmberSight encrypted file
//! version               1 byte   = 2 (1: older files, no name below)
//! argon2 memory (KiB)   4 bytes
//! argon2 passes         4 bytes
//! argon2 lanes          4 bytes
//! salt                 16 bytes  (for the passphrase key)
//! file nonce           16 bytes  (file key = HMAC-SHA256(key, "file" + nonce))
//! piece size            4 bytes  (plain bytes per piece; the last may be shorter)
//! name length           2 bytes  (version 2 only)
//! name                           (version 2 only) where the file belongs at
//!                                the destination, e.g. clips/<id>.mp4.enc
//! pieces...                      each = encrypted piece + 16-byte tag
//! ```
//!
//! Piece n is sealed with nonce = n (8 bytes, big-endian) + 3 zero bytes +
//! 1 if it is the last piece else 0, and the whole header (name included)
//! as associated data. So a file moved to another name, or swapped with
//! another one, no longer passes the check when restored under the name
//! the (signed) manifest expects.

use anyhow::{bail, Context, Result};
use ring::aead::{Aad, LessSafeKey, Nonce, UnboundKey, AES_256_GCM};
use std::io::{Read, Write};

pub const MAGIC: &[u8; 8] = b"AMBSENC1";
const VERSION: u8 = 2;
/// The fixed part of the header (all of it in version 1 files).
pub const HEADER_LEN: usize = 8 + 1 + 12 + 16 + 16 + 4;
/// Longest name stored in a header.
pub const MAX_NAME: usize = 1024;
/// Most Argon2 memory (KiB) and passes accepted from a file or key.json:
/// a damaged or hostile destination must not tie up the computer.
pub const MAX_MEMORY_KIB: u32 = 256 * 1024;
pub const MAX_PASSES: u32 = 10;
pub const MAX_LANES: u32 = 8;
const TAG_LEN: usize = 16;
/// Plain bytes per piece.
pub const PIECE: usize = 1 << 20;

/// Argon2id settings (memory in KiB, passes, lanes).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct KdfParams {
    pub m: u32,
    pub t: u32,
    pub p: u32,
}

/// 64 MB of memory and 3 passes: about half a second on a desktop.
pub const DEFAULT_PARAMS: KdfParams = KdfParams { m: 65536, t: 3, p: 1 };

/// A key made from a passphrase, with the salt it was made with.
#[derive(Clone)]
pub struct MasterKey {
    pub salt: [u8; 16],
    pub params: KdfParams,
    key: [u8; 32],
}

impl Drop for MasterKey {
    fn drop(&mut self) {
        use zeroize::Zeroize;
        self.key.zeroize();
    }
}

/// Are these Argon2 settings within what is accepted?
pub fn params_ok(p: KdfParams) -> bool {
    (8..=MAX_MEMORY_KIB).contains(&p.m) && (1..=MAX_PASSES).contains(&p.t) && (1..=MAX_LANES).contains(&p.p) && p.m >= 8 * p.p
}

impl MasterKey {
    /// Make the key from a passphrase (slow: Argon2id).
    pub fn derive(passphrase: &str, salt: [u8; 16], params: KdfParams) -> Result<MasterKey> {
        // Refuse settings that would make a damaged or hostile file eat the
        // computer's memory or time.
        if !params_ok(params) {
            bail!("unsupported key settings (the backup may be damaged)");
        }
        let p = argon2::Params::new(params.m, params.t, params.p, Some(32)).map_err(|e| anyhow::anyhow!("key settings: {e}"))?;
        let a = argon2::Argon2::new(argon2::Algorithm::Argon2id, argon2::Version::V0x13, p);
        let mut key = [0u8; 32];
        a.hash_password_into(passphrase.as_bytes(), &salt, &mut key).map_err(|e| anyhow::anyhow!("making the key: {e}"))?;
        Ok(MasterKey { salt, params, key })
    }

    /// A new key with a fresh random salt.
    pub fn new_random(passphrase: &str, params: KdfParams) -> Result<MasterKey> {
        Self::derive(passphrase, rand::random(), params)
    }

    /// A value that shows whether a passphrase gives this key (stored at the
    /// destination so a changed passphrase is noticed).
    pub fn check_value(&self) -> String {
        hex(ring::hmac::sign(&ring::hmac::Key::new(ring::hmac::HMAC_SHA256, &self.key), b"ambersight-offsite-check").as_ref())
    }

    /// Which key this is (its salt, as hex). Not secret.
    pub fn id(&self) -> String {
        hex(&self.salt)
    }

    fn mac_key(&self) -> ring::hmac::Key {
        let sub = ring::hmac::sign(&ring::hmac::Key::new(ring::hmac::HMAC_SHA256, &self.key), b"ambersight-offsite-manifest");
        ring::hmac::Key::new(ring::hmac::HMAC_SHA256, sub.as_ref())
    }

    /// A check value over `data` (the manifest), made with a key of its own.
    pub fn mac(&self, data: &[u8]) -> String {
        hex(ring::hmac::sign(&self.mac_key(), data).as_ref())
    }

    /// Does `mac` (hex) match `data`? Compared in constant time.
    pub fn mac_ok(&self, data: &[u8], mac: &str) -> bool {
        unhex(mac).is_some_and(|m| ring::hmac::verify(&self.mac_key(), data, &m).is_ok())
    }

    fn file_key(&self, nonce: &[u8; 16]) -> LessSafeKey {
        let mut msg = b"file".to_vec();
        msg.extend_from_slice(nonce);
        let k = ring::hmac::sign(&ring::hmac::Key::new(ring::hmac::HMAC_SHA256, &self.key), &msg);
        LessSafeKey::new(UnboundKey::new(&AES_256_GCM, k.as_ref()).expect("32-byte key"))
    }
}

pub fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}

pub fn unhex(s: &str) -> Option<Vec<u8>> {
    if s.len() % 2 != 0 {
        return None;
    }
    (0..s.len()).step_by(2).map(|i| u8::from_str_radix(s.get(i..i + 2)?, 16).ok()).collect()
}

fn piece_nonce(n: u64, last: bool) -> Nonce {
    let mut b = [0u8; 12];
    b[..8].copy_from_slice(&n.to_be_bytes());
    b[11] = last as u8;
    Nonce::assume_unique_for_key(b)
}

/// Size of the encrypted file for `plain` bytes and the name `name`.
pub fn encrypted_len(plain: u64, name: &str) -> u64 {
    encrypted_len_v1(plain) + 2 + name.len() as u64
}

/// Size of a version 1 encrypted file (no name) for `plain` bytes.
pub fn encrypted_len_v1(plain: u64) -> u64 {
    let pieces = plain.div_ceil(PIECE as u64).max(1);
    HEADER_LEN as u64 + plain + pieces * TAG_LEN as u64
}

/// What the start of an encrypted file says: (format version, salt, name).
/// `head` must hold the fixed header; the name is None for version 1 or
/// when `head` is too short to hold it.
pub fn peek(head: &[u8]) -> Option<(u8, [u8; 16], Option<String>)> {
    if head.len() < HEADER_LEN || &head[..8] != MAGIC || !matches!(head[8], 1 | 2) {
        return None;
    }
    let salt: [u8; 16] = head[21..37].try_into().ok()?;
    let name = if head[8] == 2 && head.len() >= HEADER_LEN + 2 {
        let n = u16::from_le_bytes([head[HEADER_LEN], head[HEADER_LEN + 1]]) as usize;
        head.get(HEADER_LEN + 2..HEADER_LEN + 2 + n).map(|b| String::from_utf8_lossy(b).to_string())
    } else {
        None
    };
    Some((head[8], salt, name))
}

/// Read until `buf` is full or the input ends; returns how much was read.
fn fill(r: &mut impl Read, buf: &mut [u8]) -> std::io::Result<usize> {
    let mut n = 0;
    while n < buf.len() {
        match r.read(&mut buf[n..]) {
            Ok(0) => break,
            Ok(k) => n += k,
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
            Err(e) => return Err(e),
        }
    }
    Ok(n)
}

/// What encrypting (or copying) a file found: plain size and SHA-256.
#[derive(Debug, Clone, PartialEq)]
pub struct Digest {
    pub size: u64,
    pub sha256: String,
}

/// Encrypt everything from `r` into `w`, for the destination name `name`
/// (it must be given again to restore it). Returns the plain size and SHA-256.
pub fn encrypt(key: &MasterKey, name: &str, r: impl Read, w: impl Write) -> Result<Digest> {
    encrypt_with(key, Some(name), r, w)
}

/// `name` None: the old version 1 format (only made by tests now).
fn encrypt_with(key: &MasterKey, name: Option<&str>, mut r: impl Read, mut w: impl Write) -> Result<Digest> {
    use sha2::Digest as _;
    if name.is_some_and(|n| n.len() > MAX_NAME) {
        bail!("file name too long");
    }
    let nonce: [u8; 16] = rand::random();
    let mut header = Vec::with_capacity(HEADER_LEN);
    header.extend_from_slice(MAGIC);
    header.push(if name.is_some() { VERSION } else { 1 });
    for v in [key.params.m, key.params.t, key.params.p] {
        header.extend_from_slice(&v.to_le_bytes());
    }
    header.extend_from_slice(&key.salt);
    header.extend_from_slice(&nonce);
    header.extend_from_slice(&(PIECE as u32).to_le_bytes());
    debug_assert_eq!(header.len(), HEADER_LEN);
    if let Some(name) = name {
        header.extend_from_slice(&(name.len() as u16).to_le_bytes());
        header.extend_from_slice(name.as_bytes());
    }
    w.write_all(&header)?;
    let fk = key.file_key(&nonce);
    let mut hash = sha2::Sha256::new();
    let mut total = 0u64;
    // Read one piece ahead, so the last piece is known when sealing it.
    let mut cur = vec![0u8; PIECE];
    let mut cur_len = fill(&mut r, &mut cur)?;
    let mut next = vec![0u8; PIECE];
    let mut n = 0u64;
    loop {
        let next_len = if cur_len == PIECE { fill(&mut r, &mut next)? } else { 0 };
        let last = next_len == 0;
        let mut piece = cur[..cur_len].to_vec();
        hash.update(&piece);
        total += cur_len as u64;
        fk.seal_in_place_append_tag(piece_nonce(n, last), Aad::from(&header[..]), &mut piece)
            .map_err(|_| anyhow::anyhow!("encryption failed"))?;
        w.write_all(&piece)?;
        if last {
            break;
        }
        n += 1;
        std::mem::swap(&mut cur, &mut next);
        cur_len = next_len;
    }
    w.flush()?;
    Ok(Digest { size: total, sha256: hex(&hash.finalize()) })
}

/// Read and check a file's header; returns (salt, params, nonce, piece size).
fn parse_header(h: &[u8]) -> Result<([u8; 16], KdfParams, [u8; 16], usize)> {
    if h.len() < HEADER_LEN || &h[..8] != MAGIC {
        bail!("not an AmberSight encrypted file");
    }
    if !matches!(h[8], 1 | VERSION) {
        bail!("encrypted with a newer AmberSight (format {}): update this program", h[8]);
    }
    let u = |i: usize| u32::from_le_bytes(h[i..i + 4].try_into().unwrap());
    let params = KdfParams { m: u(9), t: u(13), p: u(17) };
    let salt: [u8; 16] = h[21..37].try_into().unwrap();
    let nonce: [u8; 16] = h[37..53].try_into().unwrap();
    let piece = u(53) as usize;
    if piece == 0 || piece > 64 << 20 {
        bail!("damaged file (piece size)");
    }
    Ok((salt, params, nonce, piece))
}

/// Keys already made from the passphrase, by salt (making one is slow).
#[derive(Default)]
pub struct KeyCache {
    keys: Vec<MasterKey>,
}

impl KeyCache {
    pub fn get(&mut self, passphrase: &str, salt: [u8; 16], params: KdfParams) -> Result<MasterKey> {
        if let Some(k) = self.keys.iter().find(|k| k.salt == salt && k.params == params) {
            return Ok(k.clone());
        }
        let k = MasterKey::derive(passphrase, salt, params)?;
        self.keys.push(k.clone());
        Ok(k)
    }
}

/// Decrypt from `r` into `w`. A wrong passphrase, a changed byte or a file
/// cut short is an error (and `w` holds only checked pieces before it).
/// With `expect`, the file must be a version 2 file made for that name.
pub fn decrypt(passphrase: &str, keys: &mut KeyCache, expect: Option<&str>, mut r: impl Read, mut w: impl Write) -> Result<Digest> {
    use sha2::Digest as _;
    let mut header = vec![0u8; HEADER_LEN];
    if fill(&mut r, &mut header)? < HEADER_LEN {
        bail!("not an AmberSight encrypted file (too short)");
    }
    let (salt, params, nonce, piece) = parse_header(&header)?;
    if header[8] >= 2 {
        let mut nl = [0u8; 2];
        if fill(&mut r, &mut nl)? < 2 {
            bail!("the file is cut short");
        }
        let n = u16::from_le_bytes(nl) as usize;
        if n > MAX_NAME {
            bail!("damaged file (name)");
        }
        let mut name = vec![0u8; n];
        if fill(&mut r, &mut name)? < n {
            bail!("the file is cut short");
        }
        if let Some(e) = expect {
            if name != e.as_bytes() {
                bail!("this file belongs at {}, not {e} (moved or swapped?)", String::from_utf8_lossy(&name));
            }
        }
        header.extend_from_slice(&nl);
        header.extend_from_slice(&name);
    } else if let Some(e) = expect {
        bail!("{e}: expected a newer encrypted file (replaced?)");
    }
    let key = keys.get(passphrase, salt, params)?;
    let fk = key.file_key(&nonce);
    let mut hash = sha2::Sha256::new();
    let mut total = 0u64;
    let full = piece + TAG_LEN;
    let mut cur = vec![0u8; full];
    let mut cur_len = fill(&mut r, &mut cur)?;
    let mut next = vec![0u8; full];
    let mut n = 0u64;
    loop {
        if cur_len < TAG_LEN {
            bail!("the file is cut short");
        }
        let next_len = if cur_len == full { fill(&mut r, &mut next)? } else { 0 };
        let last = next_len == 0;
        let mut buf = cur[..cur_len].to_vec();
        let plain = fk.open_in_place(piece_nonce(n, last), Aad::from(&header[..]), &mut buf).map_err(|_| {
            if n == 0 {
                anyhow::anyhow!("wrong passphrase, or the file is damaged")
            } else {
                anyhow::anyhow!("the file is damaged or cut short (piece {n})")
            }
        })?;
        hash.update(&*plain);
        total += plain.len() as u64;
        w.write_all(plain)?;
        if last {
            break;
        }
        n += 1;
        std::mem::swap(&mut cur, &mut next);
        cur_len = next_len;
    }
    w.flush()?;
    Ok(Digest { size: total, sha256: hex(&hash.finalize()) })
}

/// Copy without encrypting, measuring size and SHA-256 on the way.
pub fn copy_hashed(mut r: impl Read, mut w: impl Write) -> Result<Digest> {
    use sha2::Digest as _;
    let mut hash = sha2::Sha256::new();
    let mut buf = vec![0u8; 256 * 1024];
    let mut total = 0u64;
    loop {
        let n = match r.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => n,
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(e.into()),
        };
        hash.update(&buf[..n]);
        w.write_all(&buf[..n])?;
        total += n as u64;
    }
    w.flush()?;
    Ok(Digest { size: total, sha256: hex(&hash.finalize()) })
}

/// Encrypt the file at `src` into a new file at `dst`.
pub fn encrypt_file(key: &MasterKey, name: &str, src: &std::path::Path, dst: &std::path::Path) -> Result<Digest> {
    let r = std::io::BufReader::new(std::fs::File::open(src).with_context(|| format!("open {}", src.display()))?);
    let w = std::io::BufWriter::new(super::create_private(dst)?);
    encrypt(key, name, r, w)
}

/// Does this data start like an encrypted file?
#[cfg(test)]
pub fn is_encrypted(head: &[u8]) -> bool {
    head.len() >= 8 && &head[..8] == MAGIC
}

#[cfg(test)]
pub(crate) const TEST_PARAMS: KdfParams = KdfParams { m: 64, t: 1, p: 1 };

/// Tests: a file in the old version 1 format.
#[cfg(test)]
pub(crate) fn encrypt_old(key: &MasterKey, r: impl Read, w: impl Write) -> Result<Digest> {
    encrypt_with(key, None, r, w)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn roundtrip(len: usize) {
        let data: Vec<u8> = (0..len).map(|i| (i * 31 % 251) as u8).collect();
        let key = MasterKey::new_random("correct horse", TEST_PARAMS).unwrap();
        let mut enc = Vec::new();
        let d = encrypt(&key, "clips/a.mp4.enc", &data[..], &mut enc).unwrap();
        assert_eq!(d.size, len as u64);
        assert_eq!(enc.len() as u64, encrypted_len(len as u64, "clips/a.mp4.enc"), "len {len}");
        let mut out = Vec::new();
        let d2 = decrypt("correct horse", &mut KeyCache::default(), Some("clips/a.mp4.enc"), &enc[..], &mut out).unwrap();
        assert_eq!(out, data);
        assert_eq!(d, d2);
    }

    #[test]
    fn roundtrip_sizes() {
        for len in [0, 1, 100, PIECE - 1, PIECE, PIECE + 1, 2 * PIECE, 2 * PIECE + 7] {
            roundtrip(len);
        }
    }

    #[test]
    fn wrong_passphrase_fails() {
        let key = MasterKey::new_random("right", TEST_PARAMS).unwrap();
        let mut enc = Vec::new();
        encrypt(&key, "x", &b"camera passwords"[..], &mut enc).unwrap();
        let err = decrypt("wrong", &mut KeyCache::default(), None, &enc[..], &mut Vec::new()).unwrap_err();
        assert!(err.to_string().contains("wrong passphrase"), "{err}");
    }

    #[test]
    fn tampering_detected() {
        let key = MasterKey::new_random("pw", TEST_PARAMS).unwrap();
        let data = vec![7u8; PIECE * 2 + 10];
        let mut enc = Vec::new();
        encrypt(&key, "n", &data[..], &mut enc).unwrap();
        let ok = |e: &[u8]| decrypt("pw", &mut KeyCache::default(), None, e, &mut Vec::new()).is_ok();
        let hl = HEADER_LEN + 3;
        assert!(ok(&enc));
        // A changed byte in the header, the first piece, or the last piece.
        for at in [20, HEADER_LEN + 2, hl + 5, enc.len() - 3] {
            let mut t = enc.clone();
            t[at] ^= 1;
            assert!(!ok(&t), "flip at {at}");
        }
        // Cut short at a piece boundary (looks like a complete file otherwise).
        let cut = hl + 2 * (PIECE + TAG_LEN);
        assert!(!ok(&enc[..cut]));
        // Cut anywhere else, or extra bytes added.
        assert!(!ok(&enc[..enc.len() - 1]));
        let mut longer = enc.clone();
        longer.push(0);
        assert!(!ok(&longer));
        // Pieces swapped.
        let mut sw = enc.clone();
        let (a, b) = (hl, hl + PIECE + TAG_LEN);
        let first = enc[a..b].to_vec();
        sw[a..b].copy_from_slice(&enc[b..b + PIECE + TAG_LEN]);
        sw[b..b + PIECE + TAG_LEN].copy_from_slice(&first);
        assert!(!ok(&sw));
    }

    /// A file is bound to its name: renamed (or its name byte changed) it
    /// is refused. Old version 1 files (no name) still decrypt, but not
    /// where a named file is expected.
    #[test]
    fn name_binding_and_old_files() {
        let key = MasterKey::new_random("pw", TEST_PARAMS).unwrap();
        let mut enc = Vec::new();
        encrypt(&key, "snapshots/1/config.toml.enc", &b"secret"[..], &mut enc).unwrap();
        let dec = |e: &[u8], want: Option<&str>| decrypt("pw", &mut KeyCache::default(), want, e, &mut Vec::new());
        assert!(dec(&enc, Some("snapshots/1/config.toml.enc")).is_ok());
        assert!(dec(&enc, None).is_ok());
        let err = dec(&enc, Some("snapshots/2/config.toml.enc")).unwrap_err();
        assert!(err.to_string().contains("moved or swapped"), "{err}");
        // Changing the stored name to match is caught by the check values.
        let at = HEADER_LEN + 2 + "snapshots/".len();
        let mut t = enc.clone();
        t[at] = b'2';
        assert!(dec(&t, Some("snapshots/2/config.toml.enc")).is_err());
        assert_eq!(peek(&enc), Some((2, key.salt, Some("snapshots/1/config.toml.enc".to_string()))));
        // Version 1.
        let mut old = Vec::new();
        encrypt_with(&key, None, &b"old file"[..], &mut old).unwrap();
        assert_eq!(old.len() as u64, encrypted_len_v1(8));
        let mut out = Vec::new();
        decrypt("pw", &mut KeyCache::default(), None, &old[..], &mut out).unwrap();
        assert_eq!(out, b"old file");
        assert!(dec(&old, Some("x")).is_err());
        assert_eq!(peek(&old), Some((1, key.salt, None)));
    }

    /// Argon2 settings from a file are capped, so a hostile destination
    /// cannot make a restore use gigabytes of memory or run for hours.
    #[test]
    fn kdf_limits() {
        let s = [0u8; 16];
        assert!(MasterKey::derive("pw", s, KdfParams { m: MAX_MEMORY_KIB + 1, t: 1, p: 1 }).is_err());
        assert!(MasterKey::derive("pw", s, KdfParams { m: 64, t: MAX_PASSES + 1, p: 1 }).is_err());
        assert!(MasterKey::derive("pw", s, KdfParams { m: 64, t: 1, p: MAX_LANES + 1 }).is_err());
        assert!(params_ok(DEFAULT_PARAMS));
        // A file asking for 2 GiB is refused before any memory is used.
        let key = MasterKey::new_random("pw", TEST_PARAMS).unwrap();
        let mut enc = Vec::new();
        encrypt(&key, "n", &b"x"[..], &mut enc).unwrap();
        enc[9..13].copy_from_slice(&(2u32 * 1024 * 1024).to_le_bytes());
        let err = decrypt("pw", &mut KeyCache::default(), None, &enc[..], &mut Vec::new()).unwrap_err();
        assert!(err.to_string().contains("unsupported key settings"), "{err}");
    }

    #[test]
    fn manifest_mac() {
        let a = MasterKey::new_random("pw", TEST_PARAMS).unwrap();
        let m = a.mac(b"list");
        assert!(a.mac_ok(b"list", &m));
        assert!(!a.mac_ok(b"lisT", &m));
        let other = MasterKey::derive("other", a.salt, TEST_PARAMS).unwrap();
        assert!(!other.mac_ok(b"list", &m));
        assert!(!a.mac_ok(b"list", "zz"));
    }

    #[test]
    fn same_passphrase_same_check_value() {
        let a = MasterKey::new_random("pw", TEST_PARAMS).unwrap();
        let b = MasterKey::derive("pw", a.salt, TEST_PARAMS).unwrap();
        let c = MasterKey::derive("other", a.salt, TEST_PARAMS).unwrap();
        assert_eq!(a.check_value(), b.check_value());
        assert_ne!(a.check_value(), c.check_value());
        assert_eq!(unhex(&hex(&a.salt)).unwrap(), a.salt.to_vec());
    }
}
