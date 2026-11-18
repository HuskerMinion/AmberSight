//! Tests only: a HomeKit controller's side of pairing (the messages an
//! iPhone sends), and a tiny HAP client over TCP for the server tests.

use crypto_bigint::{U256, U3072, U512};
use sha2::{Digest, Sha512};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

use super::crypto::{self, hkdf, msg_nonce, LongTermKey, Session};
use super::tlv::{t, Tlv};

fn h(parts: &[&[u8]]) -> [u8; 64] {
    let mut d = Sha512::new();
    for p in parts {
        d.update(p);
    }
    d.finalize().into()
}

fn errored(m: &Tlv) -> Result<(), u8> {
    match m.byte(t::ERROR) {
        Some(e) => Err(e),
        None => Ok(()),
    }
}

/// A controller doing pair-setup with a code, as `id` with key seed `seed`.
#[derive(Clone)]
pub struct SetupClient {
    pub code: String,
    pub id: String,
    pub ltk: std::sync::Arc<LongTermKey>,
    a_pub: Vec<u8>,
    proof: Vec<u8>,
    key: [u8; 64],
}

impl SetupClient {
    pub fn new(code: &str, id: &str, seed: u8) -> SetupClient {
        SetupClient { code: code.into(), id: id.into(), ltk: std::sync::Arc::new(LongTermKey::from_seed(&[seed; 32]).unwrap()), a_pub: vec![], proof: vec![], key: [0; 64] }
    }

    pub fn m1() -> Vec<u8> {
        let mut m = Tlv::new();
        m.push_u8(t::STATE, 1).push_u8(t::METHOD, 0);
        m.encode()
    }

    pub fn m3(&mut self, m2: &[u8]) -> Result<Vec<u8>, u8> {
        let m2 = Tlv::decode(m2).ok_or(0u8)?;
        errored(&m2)?;
        let (salt, b_pub) = (m2.get(t::SALT).ok_or(0u8)?, m2.get(t::PUBLIC_KEY).ok_or(0u8)?);
        // (The big-number helpers are the accessory's; the formulas are the
        // controller's own.)
        use super::srp::{bytes, m, n, params};
        let n = n();
        let p = params();
        let g = m(&U3072::from_u8(5), &p);
        let a = U256::from_be_slice(&[0x42; 32]);
        let a_pub = bytes(&g.pow(&a).retrieve()).to_vec();
        let mut gp = [0u8; 384];
        gp[383] = 5;
        let mut kk = [0u8; 384];
        kk[320..].copy_from_slice(&h(&[&bytes(&n), &gp]));
        let k = U3072::from_be_slice(&kk);
        let x = U512::from_be_slice(&h(&[salt, &h(&[b"Pair-Setup:", self.code.as_bytes()])]));
        let u = U512::from_be_slice(&h(&[&a_pub, b_pub]));
        let base = m(&U3072::from_be_slice(b_pub), &p) - m(&k, &p) * g.pow(&x);
        let s = (base.pow(&a) * base.pow(&u).pow(&x)).retrieve();
        let key = h(&[&bytes(&s)]);
        let hn = h(&[&bytes(&n)]);
        let hg = h(&[&[5]]);
        let xg: Vec<u8> = hn.iter().zip(hg.iter()).map(|(a, b)| a ^ b).collect();
        let proof = h(&[&xg, &h(&[b"Pair-Setup"]), salt, &a_pub, b_pub, &key]).to_vec();
        let mut m = Tlv::new();
        m.push_u8(t::STATE, 3).push(t::PUBLIC_KEY, &a_pub).push(t::PROOF, &proof);
        self.a_pub = a_pub;
        self.proof = proof;
        self.key = key;
        Ok(m.encode())
    }

    pub fn m5(&mut self, m4: &[u8]) -> Result<Vec<u8>, u8> {
        let m4 = Tlv::decode(m4).ok_or(0u8)?;
        errored(&m4)?;
        assert_eq!(m4.get(t::PROOF).unwrap(), &h(&[&self.a_pub, &self.proof, &self.key])[..], "the accessory's proof M2");
        let enc = hkdf(&self.key, "Pair-Setup-Encrypt-Salt", "Pair-Setup-Encrypt-Info");
        let x = hkdf(&self.key, "Pair-Setup-Controller-Sign-Salt", "Pair-Setup-Controller-Sign-Info");
        let mut info = x.to_vec();
        info.extend_from_slice(self.id.as_bytes());
        info.extend_from_slice(&self.ltk.public());
        let mut sub = Tlv::new();
        sub.push(t::IDENTIFIER, self.id.as_bytes()).push(t::PUBLIC_KEY, &self.ltk.public()).push(t::SIGNATURE, &self.ltk.sign(&info));
        let mut m = Tlv::new();
        m.push_u8(t::STATE, 5).push(t::ENCRYPTED_DATA, &crypto::seal(&enc, msg_nonce(b"PS-Msg05"), b"", &sub.encode()));
        Ok(m.encode())
    }

    /// M5 with this plaintext (sub-TLV), sealed under the exchange's key
    /// (a controller sending what it likes, as the fuzzer does).
    pub fn m5_plain(&self, plain: &[u8]) -> Vec<u8> {
        let enc = hkdf(&self.key, "Pair-Setup-Encrypt-Salt", "Pair-Setup-Encrypt-Info");
        let mut m = Tlv::new();
        m.push_u8(t::STATE, 5).push(t::ENCRYPTED_DATA, &crypto::seal(&enc, msg_nonce(b"PS-Msg05"), b"", plain));
        m.encode()
    }

    pub fn check_m6(&self, m6: &[u8]) -> Result<(), u8> {
        let m6 = Tlv::decode(m6).ok_or(0u8)?;
        errored(&m6)?;
        let enc = hkdf(&self.key, "Pair-Setup-Encrypt-Salt", "Pair-Setup-Encrypt-Info");
        let plain = crypto::open(&enc, msg_nonce(b"PS-Msg06"), b"", m6.get(t::ENCRYPTED_DATA).unwrap()).expect("M6 decrypts");
        let sub = Tlv::decode(&plain).unwrap();
        let acc_x = hkdf(&self.key, "Pair-Setup-Accessory-Sign-Salt", "Pair-Setup-Accessory-Sign-Info");
        let mut acc_info = acc_x.to_vec();
        acc_info.extend_from_slice(sub.get(t::IDENTIFIER).unwrap());
        acc_info.extend_from_slice(sub.get(t::PUBLIC_KEY).unwrap());
        assert!(crypto::verify(sub.get(t::PUBLIC_KEY).unwrap(), &acc_info, sub.get(t::SIGNATURE).unwrap()), "M6 signature");
        Ok(())
    }
}

/// A controller doing pair-verify.
pub struct VerifyClient {
    secret: Option<ring::agreement::EphemeralPrivateKey>,
    my_pub: [u8; 32],
    shared: [u8; 32],
}

impl VerifyClient {
    pub fn new() -> VerifyClient {
        use ring::agreement;
        let secret = agreement::EphemeralPrivateKey::generate(&agreement::X25519, &ring::rand::SystemRandom::new()).unwrap();
        let my_pub: [u8; 32] = secret.compute_public_key().unwrap().as_ref().try_into().unwrap();
        VerifyClient { secret: Some(secret), my_pub, shared: [0; 32] }
    }

    pub fn m1(&self) -> Vec<u8> {
        let mut m = Tlv::new();
        m.push_u8(t::STATE, 1).push(t::PUBLIC_KEY, &self.my_pub);
        m.encode()
    }

    /// M3 as controller `id` with this signature (whatever it is), for M2.
    pub fn m3_signed(&mut self, m2: &[u8], id: &[u8], sig: &[u8]) -> Result<Vec<u8>, u8> {
        use ring::agreement;
        let m2 = Tlv::decode(m2).ok_or(0u8)?;
        errored(&m2)?;
        let acc_pub = m2.get(t::PUBLIC_KEY).ok_or(0u8)?.to_vec();
        let secret = self.secret.take().ok_or(0u8)?;
        self.shared = agreement::agree_ephemeral(secret, &agreement::UnparsedPublicKey::new(&agreement::X25519, &acc_pub), |k| <[u8; 32]>::try_from(k).unwrap()).map_err(|_| 0u8)?;
        let key = hkdf(&self.shared, "Pair-Verify-Encrypt-Salt", "Pair-Verify-Encrypt-Info");
        let mut s = Tlv::new();
        s.push(t::IDENTIFIER, id).push(t::SIGNATURE, sig);
        let mut m = Tlv::new();
        m.push_u8(t::STATE, 3).push(t::ENCRYPTED_DATA, &crypto::seal(&key, msg_nonce(b"PV-Msg03"), b"", &s.encode()));
        Ok(m.encode())
    }

    /// M3 for M2, checking the accessory's signature with its long-term key.
    pub fn m3(&mut self, m2: &[u8], acc_ltpk: &[u8], ctl: &SetupClient) -> Result<Vec<u8>, u8> {
        use ring::agreement;
        let m2 = Tlv::decode(m2).ok_or(0u8)?;
        errored(&m2)?;
        let acc_pub = m2.get(t::PUBLIC_KEY).ok_or(0u8)?.to_vec();
        let secret = self.secret.take().ok_or(0u8)?;
        self.shared = agreement::agree_ephemeral(secret, &agreement::UnparsedPublicKey::new(&agreement::X25519, &acc_pub), |k| <[u8; 32]>::try_from(k).unwrap()).map_err(|_| 0u8)?;
        let key = hkdf(&self.shared, "Pair-Verify-Encrypt-Salt", "Pair-Verify-Encrypt-Info");
        let plain = crypto::open(&key, msg_nonce(b"PV-Msg02"), b"", m2.get(t::ENCRYPTED_DATA).ok_or(0u8)?).ok_or(0u8)?;
        let sub = Tlv::decode(&plain).ok_or(0u8)?;
        let mut acc_info = acc_pub.clone();
        acc_info.extend_from_slice(sub.get(t::IDENTIFIER).ok_or(0u8)?);
        acc_info.extend_from_slice(&self.my_pub);
        assert!(crypto::verify(acc_ltpk, &acc_info, sub.get(t::SIGNATURE).unwrap()), "M2 signature");
        let mut info = self.my_pub.to_vec();
        info.extend_from_slice(ctl.id.as_bytes());
        info.extend_from_slice(&acc_pub);
        let mut s = Tlv::new();
        s.push(t::IDENTIFIER, ctl.id.as_bytes()).push(t::SIGNATURE, &ctl.ltk.sign(&info));
        let mut m = Tlv::new();
        m.push_u8(t::STATE, 3).push(t::ENCRYPTED_DATA, &crypto::seal(&key, msg_nonce(b"PV-Msg03"), b"", &s.encode()));
        Ok(m.encode())
    }

    pub fn check_m4(&self, m4: &[u8]) -> Result<[u8; 32], u8> {
        let m4 = Tlv::decode(m4).ok_or(0u8)?;
        errored(&m4)?;
        Ok(self.shared)
    }
}

/// A HAP client over one TCP connection (plain, then encrypted).
pub struct Hap {
    pub sock: TcpStream,
    session: Option<Session>,
    raw: Vec<u8>,
    plain: Vec<u8>,
}

/// An HTTP answer: status and body (or the event's body).
pub type Answer = (u16, Vec<u8>);

impl Hap {
    pub async fn connect(port: u16) -> Hap {
        Hap { sock: TcpStream::connect(("127.0.0.1", port)).await.unwrap(), session: None, raw: vec![], plain: vec![] }
    }

    /// Connect from this local address (Linux: any of 127/8).
    pub async fn connect_from(port: u16, from: std::net::IpAddr) -> Hap {
        let s = tokio::net::TcpSocket::new_v4().unwrap();
        s.bind((from, 0).into()).unwrap();
        Hap { sock: s.connect(([127, 0, 0, 1], port).into()).await.unwrap(), session: None, raw: vec![], plain: vec![] }
    }

    pub async fn send_raw(&mut self, bytes: &[u8]) -> std::io::Result<()> {
        let out = match self.session.as_mut() {
            Some(s) => s.encrypt(bytes).unwrap(),
            None => bytes.to_vec(),
        };
        self.sock.write_all(&out).await
    }

    pub async fn request(&mut self, method: &str, path: &str, ctype: &str, body: &[u8]) -> Option<Answer> {
        let head = format!("{method} {path} HTTP/1.1\r\nHost: x\r\nContent-Type: {ctype}\r\nContent-Length: {}\r\n\r\n", body.len());
        let mut msg = head.into_bytes();
        msg.extend_from_slice(body);
        self.send_raw(&msg).await.ok()?;
        self.read_message().await
    }

    pub async fn tlv(&mut self, path: &str, body: &[u8]) -> Option<Answer> {
        self.request("POST", path, "application/pairing+tlv8", body).await
    }

    /// The next response or event (None: closed or 5 s without one).
    pub async fn read_message(&mut self) -> Option<Answer> {
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(5);
        loop {
            if let Some(m) = self.take_message() {
                return Some(m);
            }
            let mut buf = [0u8; 4096];
            let n = tokio::time::timeout_at(deadline, self.sock.read(&mut buf)).await.ok()?.ok()?;
            if n == 0 {
                return None;
            }
            self.raw.extend_from_slice(&buf[..n]);
            match self.session.as_mut() {
                Some(s) => s.decrypt(&mut self.raw, &mut self.plain).ok()?,
                None => self.plain.append(&mut self.raw),
            }
        }
    }

    fn take_message(&mut self) -> Option<Answer> {
        let end = self.plain.windows(4).position(|w| w == b"\r\n\r\n")?;
        let head = String::from_utf8_lossy(&self.plain[..end]).to_string();
        let status: u16 = head.split(' ').nth(1)?.parse().ok()?;
        let len: usize = head.lines().find_map(|l| l.to_ascii_lowercase().strip_prefix("content-length:").map(|v| v.trim().parse().unwrap_or(0))).unwrap_or(0);
        if self.plain.len() < end + 4 + len {
            return None;
        }
        let body = self.plain[end + 4..end + 4 + len].to_vec();
        self.plain.drain(..end + 4 + len);
        Some((status, body))
    }

    /// Whether the server closed the connection (within `ms`; anything it
    /// still sends before is skipped).
    pub async fn closed_within(&mut self, ms: u64) -> bool {
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_millis(ms);
        let mut buf = [0u8; 4096];
        loop {
            match tokio::time::timeout_at(deadline, self.sock.read(&mut buf)).await {
                Ok(Ok(0)) | Ok(Err(_)) => return true,
                Ok(Ok(_)) => continue,
                Err(_) => return false,
            }
        }
    }

    /// Pair-setup on this connection.
    pub async fn pair_setup(&mut self, c: &SetupClient) -> Result<(), u8> {
        let mut c = c.clone();
        let (_, r) = self.tlv("/pair-setup", &SetupClient::m1()).await.ok_or(0u8)?;
        let (_, r) = self.tlv("/pair-setup", &c.m3(&r)?).await.ok_or(0u8)?;
        let (_, r) = self.tlv("/pair-setup", &c.m5(&r)?).await.ok_or(0u8)?;
        c.check_m6(&r)
    }

    /// Pair-verify; the session is encrypted after.
    pub async fn pair_verify(&mut self, c: &SetupClient, acc_ltpk: &[u8]) -> Result<(), u8> {
        let mut v = VerifyClient::new();
        let (_, r) = self.tlv("/pair-verify", &v.m1()).await.ok_or(0u8)?;
        let (_, r) = self.tlv("/pair-verify", &v.m3(&r, acc_ltpk, c)?).await.ok_or(0u8)?;
        let shared = v.check_m4(&r)?;
        self.session = Some(Session::controller(&shared));
        // Anything already read after M4 is encrypted.
        self.raw = std::mem::take(&mut self.plain);
        Ok(())
    }
}
