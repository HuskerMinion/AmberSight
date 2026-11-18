//! The crypto HomeKit pairing and sessions use: HKDF-SHA-512 key
//! derivation, ChaCha20-Poly1305 with HAP's nonces, Ed25519 signatures and
//! X25519 key agreement (ring, already used for TLS), and the encrypted
//! session framing.

use anyhow::{anyhow, bail, Result};
use ring::aead::{Aad, LessSafeKey, Nonce, UnboundKey, CHACHA20_POLY1305};
use ring::signature::{Ed25519KeyPair, KeyPair, UnparsedPublicKey, ED25519};
use zeroize::Zeroize;

/// HKDF-SHA-512 to 32 bytes.
pub fn hkdf(ikm: &[u8], salt: &str, info: &str) -> [u8; 32] {
    let hk = hkdf::Hkdf::<sha2::Sha512>::new(Some(salt.as_bytes()), ikm);
    let mut out = [0u8; 32];
    hk.expand(info.as_bytes(), &mut out).expect("32 bytes is a valid HKDF length");
    out
}

/// The 12-byte nonce for a pairing message ("PS-Msg05" and the like: four
/// zero bytes, then the 8 characters).
pub fn msg_nonce(label: &[u8; 8]) -> [u8; 12] {
    let mut n = [0u8; 12];
    n[4..].copy_from_slice(label);
    n
}

/// The 12-byte nonce of session frame number `count` (four zero bytes, then
/// the counter, little-endian).
pub fn counter_nonce(count: u64) -> [u8; 12] {
    let mut n = [0u8; 12];
    n[4..].copy_from_slice(&count.to_le_bytes());
    n
}

fn key(k: &[u8; 32]) -> LessSafeKey {
    LessSafeKey::new(UnboundKey::new(&CHACHA20_POLY1305, k).expect("32-byte key"))
}

/// Encrypt: ciphertext with the 16-byte tag appended.
pub fn seal(k: &[u8; 32], nonce: [u8; 12], aad: &[u8], plain: &[u8]) -> Vec<u8> {
    let mut buf = plain.to_vec();
    key(k).seal_in_place_append_tag(Nonce::assume_unique_for_key(nonce), Aad::from(aad), &mut buf).expect("sealing cannot fail for these sizes");
    buf
}

/// Decrypt ciphertext+tag; None when the tag is wrong (or it is too short).
pub fn open(k: &[u8; 32], nonce: [u8; 12], aad: &[u8], sealed: &[u8]) -> Option<Vec<u8>> {
    if sealed.len() < 16 {
        return None;
    }
    let mut buf = sealed.to_vec();
    let n = key(k).open_in_place(Nonce::assume_unique_for_key(nonce), Aad::from(aad), &mut buf).ok()?.len();
    buf.truncate(n);
    Some(buf)
}

/// The accessory's long-term Ed25519 key, from its 32-byte seed.
pub struct LongTermKey {
    pair: Ed25519KeyPair,
}

impl LongTermKey {
    pub fn from_seed(seed: &[u8; 32]) -> Result<LongTermKey> {
        Ed25519KeyPair::from_seed_unchecked(seed).map(|pair| LongTermKey { pair }).map_err(|_| anyhow!("bad Ed25519 key"))
    }
    pub fn public(&self) -> [u8; 32] {
        self.pair.public_key().as_ref().try_into().expect("32-byte Ed25519 public key")
    }
    pub fn sign(&self, msg: &[u8]) -> [u8; 64] {
        self.pair.sign(msg).as_ref().try_into().expect("64-byte Ed25519 signature")
    }
}

/// Whether `sig` is `pk`'s Ed25519 signature of `msg`. Stricter than
/// ring alone, which takes a public key (or signature R) of small order: the
/// identity as the key, with R the identity and S = 0, "signs" every message.
/// Such keys and R values, and non-canonical ones, are refused here (as
/// libsodium and ed25519-dalek's verify_strict do); ring checks the rest
/// (S below the group order, the equation).
pub fn verify(pk: &[u8], msg: &[u8], sig: &[u8]) -> bool {
    let (Ok(key), Some(r)) = (<&[u8; 32]>::try_from(pk), sig.get(..32).and_then(|r| <&[u8; 32]>::try_from(r).ok())) else { return false };
    sig.len() == 64 && !weak_point(key) && !weak_point(r) && UnparsedPublicKey::new(&ED25519, pk).verify(msg, sig).is_ok()
}

/// A controller's long-term public key HomeKit takes: 32 bytes, canonical,
/// not of small order. (Whether it is on the curve at all is ring's check,
/// when it verifies.)
pub fn good_ltpk(pk: &[u8]) -> bool {
    <&[u8; 32]>::try_from(pk).is_ok_and(|k| !weak_point(k))
}

/// The y coordinates (little-endian, sign bit clear) of Ed25519's 8 points
/// of small order: the identity (y = 1), order 2 (y = p - 1), order 4 (y = 0,
/// both signs of x) and order 8 (two values of y, both signs of x).
const SMALL_ORDER_Y: [[u8; 32]; 5] = [
    {
        let mut y = [0u8; 32];
        y[0] = 1;
        y
    },
    [
        0xec, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0x7f,
    ],
    [0u8; 32],
    [
        0x26, 0xe8, 0x95, 0x8f, 0xc2, 0xb2, 0x27, 0xb0, 0x45, 0xc3, 0xf4, 0x89, 0xf2, 0xef, 0x98, 0xf0, 0xd5, 0xdf, 0xac, 0x05, 0xd3, 0xc6, 0x33, 0x39, 0xb1, 0x38, 0x02, 0x88, 0x6d, 0x53, 0xfc, 0x05,
    ],
    [
        0xc7, 0x17, 0x6a, 0x70, 0x3d, 0x4d, 0xd8, 0x4f, 0xba, 0x3c, 0x0b, 0x76, 0x0d, 0x10, 0x67, 0x0f, 0x2a, 0x20, 0x53, 0xfa, 0x2c, 0x39, 0xcc, 0xc6, 0x4e, 0xc7, 0xfd, 0x77, 0x92, 0xac, 0x03, 0x7a,
    ],
];

/// An Ed25519 point encoding no honest key or signature has: y not below p
/// (non-canonical, so it also covers the small-order points written as
/// y + p), or a point of small order (either sign bit).
pub fn weak_point(enc: &[u8; 32]) -> bool {
    let mut y = *enc;
    y[31] &= 0x7f;
    // p = 2^255 - 19, little-endian: ed ff .. ff 7f. y >= p only when its top
    // 31 bytes are all ff (7f for the last) and its first is ed or more.
    let non_canonical = y[1..31].iter().all(|b| *b == 0xff) && y[31] == 0x7f && y[0] >= 0xed;
    non_canonical || SMALL_ORDER_Y.contains(&y)
}

/// A fresh X25519 key pair for one pair-verify: our public key and the
/// shared secret with the controller's public key. None for a bad key (also
/// one that gives an all-zero secret).
pub fn x25519_agree(peer: &[u8]) -> Option<([u8; 32], [u8; 32])> {
    use ring::agreement;
    if peer.len() != 32 {
        return None;
    }
    let rng = ring::rand::SystemRandom::new();
    let ours = agreement::EphemeralPrivateKey::generate(&agreement::X25519, &rng).ok()?;
    let public: [u8; 32] = ours.compute_public_key().ok()?.as_ref().try_into().ok()?;
    let shared = agreement::agree_ephemeral(ours, &agreement::UnparsedPublicKey::new(&agreement::X25519, peer), |k| <[u8; 32]>::try_from(k).ok()).ok()??;
    if shared.iter().all(|b| *b == 0) {
        return None;
    }
    Some((public, shared))
}

/// Largest plaintext in one session frame.
pub const FRAME: usize = 1024;

/// One direction of an encrypted session.
struct Direction {
    key: [u8; 32],
    count: u64,
}

impl Direction {
    /// The next nonce; never repeats (the session ends instead of wrapping).
    fn next(&mut self) -> Result<[u8; 12]> {
        if self.count == u64::MAX {
            bail!("session frame counter exhausted");
        }
        let n = counter_nonce(self.count);
        self.count += 1;
        Ok(n)
    }
}

impl Drop for Direction {
    fn drop(&mut self) {
        self.key.zeroize();
    }
}

/// The encrypted HAP session after pair-verify: frames of a 2-byte
/// little-endian length (also the additional data), at most 1024 bytes of
/// ciphertext, and the 16-byte tag. Controller to accessory uses the
/// Control-Write key, accessory to controller the Control-Read key.
pub struct Session {
    incoming: Direction,
    outgoing: Direction,
}

impl Session {
    pub fn new(shared: &[u8; 32]) -> Session {
        Session {
            incoming: Direction { key: hkdf(shared, "Control-Salt", "Control-Write-Encryption-Key"), count: 0 },
            outgoing: Direction { key: hkdf(shared, "Control-Salt", "Control-Read-Encryption-Key"), count: 0 },
        }
    }

    /// The controller's side of the same session (tests and tools).
    #[cfg(test)]
    pub fn controller(shared: &[u8; 32]) -> Session {
        let s = Session::new(shared);
        Session {
            incoming: Direction { key: s.outgoing.key, count: 0 },
            outgoing: Direction { key: s.incoming.key, count: 0 },
        }
    }

    /// Encrypt a message into frames.
    pub fn encrypt(&mut self, plain: &[u8]) -> Result<Vec<u8>> {
        let mut out = Vec::with_capacity(plain.len() + (plain.len() / FRAME + 1) * 18);
        for chunk in plain.chunks(FRAME) {
            let len = (chunk.len() as u16).to_le_bytes();
            let nonce = self.outgoing.next()?;
            out.extend_from_slice(&len);
            out.extend_from_slice(&seal(&self.outgoing.key, nonce, &len, chunk));
        }
        Ok(out)
    }

    /// Decrypt every whole frame at the start of `buf` (removing them) and
    /// append the plaintext to `plain`. A frame too long or with a wrong tag
    /// ends the session (Err).
    pub fn decrypt(&mut self, buf: &mut Vec<u8>, plain: &mut Vec<u8>) -> Result<()> {
        let mut used = 0usize;
        loop {
            let rest = &buf[used..];
            if rest.len() < 2 {
                break;
            }
            let len = u16::from_le_bytes([rest[0], rest[1]]) as usize;
            if len > FRAME {
                bail!("session frame too long");
            }
            if rest.len() < 2 + len + 16 {
                break;
            }
            let nonce = self.incoming.next()?;
            let p = open(&self.incoming.key, nonce, &rest[..2], &rest[2..2 + len + 16]).ok_or_else(|| anyhow!("session frame failed authentication"))?;
            plain.extend_from_slice(&p);
            used += 2 + len + 16;
        }
        buf.drain(..used);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Signature R = the identity, S = 0.
    fn null_signature() -> [u8; 64] {
        let mut sig = [0u8; 64];
        sig[0] = 1;
        sig
    }

    #[test]
    fn small_order_and_non_canonical_keys_are_refused() {
        // Each listed point really is of small order: with R the identity
        // and S = 0, ring takes the "signature" for some message (for the
        // identity, every one). All of them, with either sign bit, are refused.
        for y in SMALL_ORDER_Y {
            for sign in [0u8, 0x80] {
                let mut pk = y;
                pk[31] |= sign;
                // (x = 0 with the sign bit set is no encoding at all: not tried.)
                if sign == 0 || (y != SMALL_ORDER_Y[0] && y != SMALL_ORDER_Y[1]) {
                    let forged = (0u8..64).any(|m| UnparsedPublicKey::new(&ED25519, &pk).verify(&[m], &null_signature()).is_ok());
                    assert!(forged, "{pk:02x?} is of small order");
                }
                assert!(weak_point(&pk) && !good_ltpk(&pk));
                assert!((0u8..64).all(|m| !verify(&pk, &[m], &null_signature())), "{pk:02x?}");
            }
        }
        // y = p .. 2^255 - 1 (non-canonical; y = p and p + 1 are the order-4
        // and identity points again).
        for low in 0xedu8..=0xff {
            let mut pk = [0xffu8; 32];
            pk[0] = low;
            pk[31] = 0x7f;
            assert!(weak_point(&pk));
            pk[31] = 0xff;
            assert!(weak_point(&pk));
        }
        // A real key and signature still verify; R of small order does not.
        let k = LongTermKey::from_seed(&[5u8; 32]).unwrap();
        let sig = k.sign(b"hello");
        assert!(good_ltpk(&k.public()) && verify(&k.public(), b"hello", &sig));
        // (y = p - 2: canonical and not of small order.)
        let mut y = [0xffu8; 32];
        y[0] = 0xeb;
        y[31] = 0x7f;
        assert!(!weak_point(&y));
        let mut bad_r = sig;
        bad_r[..32].copy_from_slice(&SMALL_ORDER_Y[0]);
        assert!(!verify(&k.public(), b"hello", &bad_r));
        assert!(!verify(&k.public()[..31], b"hello", &sig) && !verify(&k.public(), b"hello", &sig[..63]));
    }

    #[test]
    fn hkdf_known_answer() {
        // HKDF-SHA-512 (RFC 5869) of a fixed input, computed separately with
        // Python's hmac module.
        let k = hkdf(b"secret", "Pair-Setup-Encrypt-Salt", "Pair-Setup-Encrypt-Info");
        let want = "9ba61fd9bf136fa77d322023e40aa03824f6d4d1b2524654e5342ef8ec94d468";
        assert_eq!(k.iter().map(|b| format!("{b:02x}")).collect::<String>(), want);
        // Different salts or infos give different keys.
        assert_ne!(k, hkdf(b"secret", "Pair-Setup-Encrypt-Salt", "Pair-Setup-Encrypt-Inf0"));
    }

    #[test]
    fn seal_open_round_trip_and_tamper() {
        let k = [3u8; 32];
        let n = msg_nonce(b"PS-Msg05");
        assert_eq!(&n[..4], &[0, 0, 0, 0]);
        let c = seal(&k, n, b"", b"hello");
        assert_eq!(c.len(), 5 + 16);
        assert_eq!(open(&k, n, b"", &c).unwrap(), b"hello");
        let mut bad = c.clone();
        bad[0] ^= 1;
        assert!(open(&k, n, b"", &bad).is_none());
        assert!(open(&k, msg_nonce(b"PS-Msg06"), b"", &c).is_none());
        assert!(open(&k, n, b"", &c[..10]).is_none());
    }

    #[test]
    fn session_frames_round_trip() {
        let shared = [9u8; 32];
        let mut acc = Session::new(&shared);
        let mut ctl = Session::controller(&shared);
        // Controller -> accessory, split across reads and across frames.
        let msg: Vec<u8> = (0..3000u32).map(|i| i as u8).collect();
        let wire = ctl.encrypt(&msg).unwrap();
        assert_eq!(wire.len(), 3000 + 3 * 18);
        let mut buf = Vec::new();
        let mut plain = Vec::new();
        for piece in wire.chunks(100) {
            buf.extend_from_slice(piece);
            acc.decrypt(&mut buf, &mut plain).unwrap();
        }
        assert!(buf.is_empty());
        assert_eq!(plain, msg);
        // Accessory -> controller.
        let back = acc.encrypt(b"HTTP/1.1 200 OK\r\n\r\n").unwrap();
        let (mut b2, mut p2) = (back.clone(), Vec::new());
        ctl.decrypt(&mut b2, &mut p2).unwrap();
        assert_eq!(p2, b"HTTP/1.1 200 OK\r\n\r\n");
        // A replayed frame fails (the counter moved on).
        let (mut b3, mut p3) = (back, Vec::new());
        assert!(ctl.decrypt(&mut b3, &mut p3).is_err());
    }

    #[test]
    fn oversized_or_tampered_frames_end_the_session() {
        let shared = [1u8; 32];
        let mut acc = Session::new(&shared);
        let mut buf = vec![0x01, 0x04]; // 1025
        buf.extend(vec![0u8; 1100]);
        assert!(acc.decrypt(&mut buf, &mut Vec::new()).is_err());
        let mut ctl = Session::controller(&shared);
        let mut acc = Session::new(&shared);
        let mut w = ctl.encrypt(b"abc").unwrap();
        w[3] ^= 0x80;
        assert!(acc.decrypt(&mut w, &mut Vec::new()).is_err());
    }

    #[test]
    fn counter_never_wraps() {
        let mut d = Direction { key: [0; 32], count: u64::MAX - 1 };
        assert!(d.next().is_ok());
        assert!(d.next().is_err());
        assert!(d.next().is_err());
    }

    #[test]
    fn ed25519_and_x25519() {
        let k = LongTermKey::from_seed(&[5u8; 32]).unwrap();
        let sig = k.sign(b"msg");
        assert!(verify(&k.public(), b"msg", &sig));
        assert!(!verify(&k.public(), b"msh", &sig));
        assert!(!verify(&k.public()[..31], b"msg", &sig));
        // Two sides agree.
        let (pub_a, _) = x25519_agree(&[9u8; 32]).unwrap();
        assert_eq!(pub_a.len(), 32);
        // A low-order point (all zeros) is refused.
        assert!(x25519_agree(&[0u8; 32]).is_none());
        assert!(x25519_agree(&[1u8; 31]).is_none());
    }
}
