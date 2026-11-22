//! Pair-setup (SRP, M1-M6), pair-verify (X25519 + Ed25519, M1-M4) and the
//! pairings list (add, remove, list), following the HAP specification
//! (non-commercial edition, chapter 5). Message bodies are TLV8.
//!
//! Defences: pair-setup only while unpaired, and only while an admin has
//! opened a pairing window for this accessory in Settings (5 minutes; it
//! closes when a pairing succeeds or the admin stops it). An admin who sees
//! a device they do not know pairing may stop it: its address is refused
//! until the window closes (it gets 5 minutes from then; a pair-setup from
//! another address goes on). One pair-setup at a time; one M1 per
//! connection (pair-setup and pair-verify alike; pair-setup again only after
//! that connection's wrong code); a pair-setup that does not send its proof
//! (M3) within 30 s may be taken over by another connection. After a wrong
//! code nobody may try again for a while (growing with the count), and
//! after 100 wrong codes pairing is refused until it is reset. Proofs are
//! compared in constant time (srp.rs). The SRP, X25519 and Ed25519 work runs
//! on blocking threads with no lock held. Nothing secret is logged.

use std::net::IpAddr;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use zeroize::{Zeroize, Zeroizing};

use super::crypto::{self, hkdf_secret, msg_nonce};
use super::server::Kind;
use super::srp;
use super::state::{hex, unhex, Pairing, State, MAX_PAIRINGS, MAX_TRIES};
use super::tlv::{err, t, Tlv};

/// How long the pairing steps may take.
#[derive(Clone, Copy, Debug)]
pub struct Timing {
    /// A pair-setup holds the slot (and its connection stays open) this long.
    pub setup_timeout: Duration,
    /// A pair-setup that has not sent M3 within this long may be taken over.
    pub m3_window: Duration,
}

impl Default for Timing {
    fn default() -> Self {
        Timing { setup_timeout: Duration::from_secs(300), m3_window: Duration::from_secs(30) }
    }
}

/// How long a pairing window stays open.
pub const WINDOW: Duration = Duration::from_secs(300);
/// Addresses one window refuses at most (the oldest is let go first).
const MAX_REFUSED: usize = 64;

/// The pairing window: pair-setup is refused unless an admin opened it in
/// Settings, for one accessory (the bridge, or one standalone camera). One
/// for every accessory server of AmberSight, so at most one accessory can
/// be paired at a time.
#[derive(Default)]
pub struct Window {
    open: Option<Open>,
    next_id: u64,
}

/// An open pairing window.
#[derive(Clone, Debug)]
pub struct Open {
    /// Changes when it opens after being closed (or past its end) or moves
    /// to another accessory: a pair-setup begun under an earlier one cannot
    /// finish. Kept by "Stop and start again" and by opening it again for
    /// the same accessory (M5 checks the refused addresses again).
    pub id: u64,
    pub target: Kind,
    pub until: Instant,
    /// Addresses an admin stopped ("Stop and start again"): refused until
    /// the window closes.
    pub refused: Vec<IpAddr>,
}

impl Window {
    /// Open the window for `target` for `len` (again: a fresh `len`, and the
    /// refused addresses stay while it is for the same accessory). Also what
    /// was there before, even when it was past its end (its accessory may
    /// still need its pair-setup ended and to be announced again).
    pub fn open(&mut self, target: Kind, len: Duration) -> (Open, Option<Open>) {
        let now = Instant::now();
        let was = self.open.take();
        // Open again for the same accessory: the same window, longer.
        let (id, refused) = match &was {
            Some(o) if o.target == target && o.until > now => (o.id, o.refused.clone()),
            _ => {
                self.next_id += 1;
                (self.next_id, Vec::new())
            }
        };
        let o = Open { id, target, until: now + len, refused };
        self.open = Some(o.clone());
        (o, was)
    }

    /// "Stop and start again": refuse `ip` and give the window a fresh `len`.
    /// The same window (id): a pair-setup from another address (the owner's)
    /// may still finish; one from `ip` cannot, as M5 checks the refused
    /// addresses again. None when no window is open.
    pub fn restart(&mut self, ip: IpAddr, len: Duration) -> Option<Open> {
        let o = self.current()?.clone();
        let mut refused = o.refused;
        if !refused.contains(&ip) {
            if refused.len() >= MAX_REFUSED {
                refused.remove(0);
            }
            refused.push(ip);
        }
        let fresh = Open { id: o.id, target: o.target, until: Instant::now() + len, refused };
        self.open = Some(fresh.clone());
        Some(fresh)
    }

    /// Close it (the admin pressed Stop). What was there, if anything, even
    /// when it was past its end (its accessory may still need its pair-setup
    /// ended and to be announced again).
    pub fn close(&mut self) -> Option<Open> {
        self.open.take()
    }

    /// Close it when it is still window `id` (a pairing succeeded under it).
    pub fn close_if(&mut self, id: u64) -> bool {
        let yes = self.open.as_ref().is_some_and(|o| o.id == id);
        if yes {
            self.open = None;
        }
        yes
    }

    /// The window while it is open (not past its end).
    pub fn current(&self) -> Option<&Open> {
        self.open.as_ref().filter(|o| o.until > Instant::now())
    }

    /// Whether it is open for this accessory.
    pub fn open_for(&self, target: &Kind) -> bool {
        self.current().is_some_and(|o| o.target == *target)
    }

    /// The window's id when this address may pair this accessory now.
    pub fn admits(&self, target: &Kind, ip: IpAddr) -> Option<u64> {
        self.current().filter(|o| o.target == *target && !o.refused.contains(&ip)).map(|o| o.id)
    }

    /// Forget a window past its end. The accessory it was for, to announce
    /// it as not pairable again.
    pub fn expire(&mut self) -> Option<Kind> {
        if self.open.as_ref().is_some_and(|o| o.until <= Instant::now()) {
            return self.open.take().map(|o| o.target);
        }
        None
    }

    #[cfg(test)]
    pub fn age(&mut self, by: Duration) {
        if let Some(o) = self.open.as_mut() {
            o.until -= by;
        }
    }
}

/// Pair-verify's shared secret on its way to the session keys: wiped when
/// dropped, never printed, compared in constant time.
pub struct Secret(pub [u8; 32]);

impl PartialEq for Secret {
    fn eq(&self, other: &Secret) -> bool {
        use subtle::ConstantTimeEq;
        bool::from(self.0.ct_eq(&other.0))
    }
}

impl Drop for Secret {
    fn drop(&mut self) {
        self.0.zeroize();
    }
}

impl std::fmt::Debug for Secret {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Secret(..)")
    }
}

/// The SRP session key K, on the heap (moving it copies no key bytes),
/// wiped when dropped.
struct SessionKey(Box<[u8; 64]>);

impl Drop for SessionKey {
    fn drop(&mut self) {
        self.0.zeroize();
    }
}

/// What the connection does after its answer is sent.
#[derive(Debug, PartialEq)]
pub enum After {
    /// Pair-verify done: the session is encrypted from now on.
    Verified { shared: Secret, controller: String },
    /// The pairing set changed: update the mDNS record (sf).
    Advertise,
    /// End the sessions of this controller (its pairing was removed).
    EndController(String),
    /// End every session (the last admin was removed).
    EndAll,
    /// Wait this long before answering (a wrong setup code).
    Delay(Duration),
}

/// One pair-setup in progress.
pub struct SetupExchange {
    pub conn: u64,
    ip: IpAddr,
    started: Instant,
    /// The pairing window it began under.
    window: u64,
    /// None while M2 is being worked out, and again once M3's proof was
    /// right (b and the verifier are wiped when it is dropped).
    srp: Option<Arc<srp::Server>>,
    /// The SRP session key, from a right M3 proof until M5 takes it (the
    /// slot keeps no copy while M5 is checked; wiped when dropped).
    key: Option<SessionKey>,
    /// M3 has been answered with a right proof (M5 may follow once).
    proven: bool,
    /// M3 or M5 is being checked (it cannot be taken over meanwhile).
    busy: bool,
}

/// The one pair-setup at a time, and the wait after a wrong code.
#[derive(Default)]
pub struct Slot {
    ex: Option<SetupExchange>,
    /// After a wrong code: no new pair-setup before this (for anyone).
    next_try: Option<Instant>,
}

impl Slot {
    /// The connection holding the pair-setup, if any.
    pub fn holder(&self) -> Option<u64> {
        self.ex.as_ref().map(|e| e.conn)
    }
    /// The pair-setup in progress: its address and when it began.
    pub fn exchange(&self) -> Option<(IpAddr, Instant)> {
        self.ex.as_ref().map(|e| (e.ip, e.started))
    }
    /// Forget the exchange of this connection (it closed).
    pub fn release(&mut self, conn: u64) {
        if self.holder() == Some(conn) {
            self.ex = None;
        }
    }
    /// End the exchange in progress (from `ip` only, when given): the
    /// connection that held it, to close.
    pub fn stop(&mut self, ip: Option<IpAddr>) -> Option<u64> {
        if ip.is_some_and(|ip| self.ex.as_ref().is_some_and(|e| e.ip != ip)) {
            return None;
        }
        self.ex.take().map(|e| e.conn)
    }
    /// Start over (the pairing was reset).
    pub fn clear(&mut self) {
        *self = Slot::default();
    }
    #[cfg(test)]
    pub fn age(&mut self, by: Duration) {
        if let Some(e) = self.ex.as_mut() {
            e.started -= by;
        }
    }
}

/// A connection's own pairing state.
pub struct ConnPairing {
    pub id: u64,
    pub ip: IpAddr,
    setup_m1: bool,
    verify_m1: bool,
    verify: Option<VerifyPending>,
}

impl ConnPairing {
    pub fn new(id: u64, ip: IpAddr) -> ConnPairing {
        ConnPairing { id, ip, setup_m1: false, verify_m1: false, verify: None }
    }
}

/// Pair-verify between M2 and M3.
struct VerifyPending {
    shared: [u8; 32],
    ours: [u8; 32],
    theirs: [u8; 32],
}

impl Drop for VerifyPending {
    fn drop(&mut self) {
        self.shared.zeroize();
    }
}

fn error(state: u8, code: u8) -> Vec<u8> {
    let mut m = Tlv::new();
    m.push_u8(t::STATE, state).push_u8(t::ERROR, code);
    m.encode()
}

/// Whether an answer is a success (no error item).
pub fn is_success(body: &[u8]) -> bool {
    Tlv::decode(body).is_some_and(|t| t.get(t::ERROR).is_none())
}

/// How long everyone waits after the n-th wrong setup code.
pub fn wrong_code_delay(tries: u32) -> Duration {
    Duration::from_millis(500 * u64::from(tries.clamp(1, 8)))
}

async fn blocking<T: Send + 'static>(f: impl FnOnce() -> T + Send + 'static) -> Option<T> {
    tokio::task::spawn_blocking(f).await.ok()
}

/// One accessory server's pair-setup: its state, its one exchange at a
/// time, which accessory it is, and the pairing window of them all.
pub struct Setup<'a> {
    pub state: &'a Mutex<State>,
    pub slot: &'a Mutex<Slot>,
    pub window: &'a Mutex<Window>,
    pub kind: &'a Kind,
    pub timing: &'a Timing,
}

/// `POST /pair-setup`.
pub async fn pair_setup(s: &Setup<'_>, c: &mut ConnPairing, body: &[u8]) -> (Vec<u8>, Vec<After>) {
    let Some(req) = Tlv::decode(body) else { return (error(2, err::UNKNOWN), vec![]) };
    match req.byte(t::STATE).unwrap_or(0) {
        1 => setup_m1(s, c, &req).await,
        3 => setup_m3(s, c, &req).await,
        5 => setup_m5(s, c, &req).await,
        _ => (error(2, err::UNKNOWN), vec![]),
    }
}

async fn setup_m1(s: &Setup<'_>, c: &mut ConnPairing, req: &Tlv) -> (Vec<u8>, Vec<After>) {
    if c.setup_m1 {
        return (error(2, err::UNKNOWN), vec![]);
    }
    c.setup_m1 = true;
    match req.byte(t::METHOD) {
        Some(0) | Some(1) | None => {}
        Some(_) => return (error(2, err::UNKNOWN), vec![]),
    }
    let mut code = {
        let st = s.state.lock().unwrap();
        if st.paired() {
            return (error(2, err::UNAVAILABLE), vec![]);
        }
        if st.failed_tries >= MAX_TRIES {
            return (error(2, err::MAX_TRIES), vec![]);
        }
        st.setup_code.clone()
    };
    // Not pairable now: no window open for this accessory (or this address
    // was stopped in it). HAP's answer for an accessory that takes no
    // pairing now. Nothing was started, so the connection may ask again (a
    // phone that tried before the window was opened may keep its connection).
    let Some(window) = s.window.lock().unwrap().admits(s.kind, c.ip) else {
        code.zeroize();
        c.setup_m1 = false;
        return (error(2, err::UNAVAILABLE), vec![]);
    };
    {
        let mut slot = s.slot.lock().unwrap();
        let now = Instant::now();
        if let Some(at) = slot.next_try.filter(|at| *at > now) {
            let mut m = Tlv::new();
            m.push_u8(t::STATE, 2).push_u8(t::ERROR, err::BACKOFF).push_u8(t::RETRY_DELAY, (at - now).as_secs().clamp(1, 255) as u8);
            code.zeroize();
            return (m.encode(), vec![]);
        }
        if let Some(ex) = &slot.ex {
            let age = now.duration_since(ex.started);
            let expired = age > s.timing.setup_timeout;
            let stale = !ex.proven && !ex.busy && age > s.timing.m3_window;
            if !(expired || stale) {
                code.zeroize();
                return (error(2, err::BUSY), vec![]);
            }
            // It lapsed: this connection takes it over.
            slot.ex = None;
        }
        slot.ex = Some(SetupExchange { conn: c.id, ip: c.ip, started: now, window, srp: None, key: None, proven: false, busy: false });
    }
    let Some(server) = blocking(move || {
        let s = srp::Server::random(&code);
        code.zeroize();
        s
    })
    .await
    else {
        s.slot.lock().unwrap().release(c.id);
        return (error(2, err::UNKNOWN), vec![]);
    };
    let mut m = Tlv::new();
    m.push_u8(t::STATE, 2).push(t::SALT, server.salt()).push(t::PUBLIC_KEY, server.public());
    let mut slot = s.slot.lock().unwrap();
    match slot.ex.as_mut() {
        Some(ex) if ex.conn == c.id && ex.srp.is_none() => ex.srp = Some(Arc::new(server)),
        _ => return (error(2, err::BUSY), vec![]),
    }
    (m.encode(), vec![])
}

async fn setup_m3(s: &Setup<'_>, c: &mut ConnPairing, req: &Tlv) -> (Vec<u8>, Vec<After>) {
    let server = {
        let mut slot = s.slot.lock().unwrap();
        match slot.ex.as_mut() {
            Some(ex) if ex.conn == c.id && !ex.proven && !ex.busy && ex.srp.is_some() => {
                ex.busy = true;
                ex.srp.clone().unwrap()
            }
            _ => return (error(4, err::UNKNOWN), vec![]),
        }
    };
    let (Some(a), Some(proof)) = (req.get(t::PUBLIC_KEY).map(<[u8]>::to_vec), req.get(t::PROOF).map(<[u8]>::to_vec)) else {
        s.slot.lock().unwrap().release(c.id);
        return (error(4, err::UNKNOWN), vec![]);
    };
    let checked = blocking(move || server.verify(&a, &proof)).await.flatten();
    match checked {
        Some(ok) => {
            let mut slot = s.slot.lock().unwrap();
            match slot.ex.as_mut() {
                Some(ex) if ex.conn == c.id => {
                    ex.key = Some(SessionKey(Box::new(ok.key)));
                    ex.proven = true;
                    ex.busy = false;
                    // (Its work is done: b and the verifier go now, not at M5.)
                    ex.srp = None;
                }
                _ => return (error(4, err::UNKNOWN), vec![]),
            }
            let mut m = Tlv::new();
            m.push_u8(t::STATE, 4).push(t::PROOF, &ok.proof);
            (m.encode(), vec![])
        }
        None => {
            let tries = {
                let mut st = s.state.lock().unwrap();
                st.failed_tries = st.failed_tries.saturating_add(1);
                if let Err(e) = st.save() {
                    tracing::warn!("HomeKit: {e:#}");
                }
                st.failed_tries
            };
            let delay = wrong_code_delay(tries);
            // This connection may try again (after the wait), as a phone does
            // when the code was mistyped; it could not restart an exchange it held.
            c.setup_m1 = false;
            {
                let mut slot = s.slot.lock().unwrap();
                slot.release(c.id);
                slot.next_try = Some(Instant::now() + delay);
            }
            tracing::warn!("HomeKit: a pairing attempt used a wrong setup code ({tries} of {MAX_TRIES} allowed)");
            let mut after = vec![After::Delay(delay)];
            if tries >= MAX_TRIES {
                tracing::warn!("HomeKit: too many wrong setup codes; pairing is refused until it is reset in Settings");
                after.push(After::Advertise);
            }
            (error(4, err::AUTHENTICATION), after)
        }
    }
}

/// M5 -> M6: check the controller's signed identity, store it as the first
/// (admin) pairing, answer with ours.
async fn setup_m5(s: &Setup<'_>, c: &mut ConnPairing, req: &Tlv) -> (Vec<u8>, Vec<After>) {
    let (key, window) = {
        let mut slot = s.slot.lock().unwrap();
        match slot.ex.as_mut() {
            Some(ex) if ex.conn == c.id && !ex.busy && ex.key.is_some() => {
                ex.busy = true;
                // (Taken, not copied: the slot keeps no K while M5 is checked.)
                (ex.key.take().unwrap(), ex.window)
            }
            _ => return (error(6, err::UNKNOWN), vec![]),
        }
    };
    let (device_id, lt) = {
        let mut st = s.state.lock().unwrap();
        match st.key() {
            Ok(k) => (st.device_id.clone(), k),
            Err(_) => {
                drop(st);
                s.slot.lock().unwrap().release(c.id);
                return (error(6, err::UNKNOWN), vec![]);
            }
        }
    };
    let data = req.get(t::ENCRYPTED_DATA).map(<[u8]>::to_vec);
    let dev = device_id.clone();
    let done = blocking(move || -> Result<(String, String, Vec<u8>), u8> {
        // (Best effort, either way this ends: our own copies of K and of the
        // keys derived from it are wiped. The hkdf crate's state, ring's
        // per-call cipher key and stack temporaries are not.)
        let key = key;
        let key: &[u8; 64] = &key.0;
        (|| {
            let enc_key = hkdf_secret(key, "Pair-Setup-Encrypt-Salt", "Pair-Setup-Encrypt-Info");
            let plain = crypto::open(&enc_key, msg_nonce(b"PS-Msg05"), b"", &data.ok_or(err::UNKNOWN)?).ok_or(err::AUTHENTICATION)?;
            let sub = Tlv::decode(&plain).ok_or(err::UNKNOWN)?;
            let (id, ltpk, sig) = (sub.get(t::IDENTIFIER).ok_or(err::UNKNOWN)?, sub.get(t::PUBLIC_KEY).ok_or(err::UNKNOWN)?, sub.get(t::SIGNATURE).ok_or(err::UNKNOWN)?);
            let id_str = controller_id(id).ok_or(err::UNKNOWN)?;
            // (A key of small order, or not canonical, could "sign" anything.)
            if !crypto::good_ltpk(ltpk) {
                tracing::warn!("HomeKit: a pairing was refused: the controller's key is not a usable Ed25519 key");
                return Err(err::AUTHENTICATION);
            }
            let x = hkdf_secret(key, "Pair-Setup-Controller-Sign-Salt", "Pair-Setup-Controller-Sign-Info");
            let mut info = Zeroizing::new(x.to_vec());
            info.extend_from_slice(id);
            info.extend_from_slice(ltpk);
            if !crypto::verify(ltpk, &info, sig) {
                return Err(err::AUTHENTICATION);
            }
            let acc_x = hkdf_secret(key, "Pair-Setup-Accessory-Sign-Salt", "Pair-Setup-Accessory-Sign-Info");
            let mut acc_info = Zeroizing::new(acc_x.to_vec());
            acc_info.extend_from_slice(dev.as_bytes());
            acc_info.extend_from_slice(&lt.public());
            let mut sub = Tlv::new();
            sub.push(t::IDENTIFIER, dev.as_bytes()).push(t::PUBLIC_KEY, &lt.public()).push(t::SIGNATURE, &lt.sign(&acc_info));
            let sealed = crypto::seal(&enc_key, msg_nonce(b"PS-Msg06"), b"", &sub.encode());
            Ok((id_str, hex(ltpk), sealed))
        })()
    })
    .await
    .unwrap_or(Err(err::UNKNOWN));
    s.slot.lock().unwrap().release(c.id);
    let (id, ltpk, sealed) = match done {
        Ok(v) => v,
        Err(code) => return (error(6, code), vec![]),
    };
    // The window it began under must still be open, and this address not
    // stopped meanwhile (the admin pressed Stop, or Stop and start again).
    if s.window.lock().unwrap().admits(s.kind, c.ip) != Some(window) {
        tracing::info!("HomeKit: a pairing was refused: its pairing window was closed or started again");
        return (error(6, err::UNAVAILABLE), vec![]);
    }
    {
        let mut st = s.state.lock().unwrap();
        // (Reset or paired by now: the answer would be for an old identity.)
        if st.paired() || st.device_id != device_id {
            return (error(6, err::UNAVAILABLE), vec![]);
        }
        st.pairings.push(Pairing { id, ltpk, admin: true });
        st.failed_tries = 0;
        if let Err(e) = st.save() {
            tracing::warn!("HomeKit: {e:#}");
            st.pairings.clear();
            return (error(6, err::UNKNOWN), vec![]);
        }
    }
    // Paired: the window has done its job.
    s.window.lock().unwrap().close_if(window);
    tracing::info!("HomeKit: paired with a controller; the pairing window is closed");
    let mut m = Tlv::new();
    m.push_u8(t::STATE, 6).push(t::ENCRYPTED_DATA, &sealed);
    (m.encode(), vec![After::Advertise])
}

/// A controller's pairing id: printable text (Apple uses a UUID), at most
/// 64 bytes.
fn controller_id(id: &[u8]) -> Option<String> {
    if id.is_empty() || id.len() > 64 {
        return None;
    }
    let s = std::str::from_utf8(id).ok()?;
    s.chars().all(|c| c.is_ascii_graphic()).then(|| s.to_string())
}

/// `POST /pair-verify`.
pub async fn pair_verify(state: &Mutex<State>, c: &mut ConnPairing, body: &[u8]) -> (Vec<u8>, Vec<After>) {
    let Some(req) = Tlv::decode(body) else { return (error(2, err::UNKNOWN), vec![]) };
    match req.byte(t::STATE) {
        Some(1) => {
            if c.verify_m1 {
                return (error(2, err::UNKNOWN), vec![]);
            }
            c.verify_m1 = true;
            let Some(theirs) = req.get(t::PUBLIC_KEY).and_then(|k| <[u8; 32]>::try_from(k).ok()) else {
                return (error(2, err::UNKNOWN), vec![]);
            };
            let (device_id, lt) = {
                let mut st = state.lock().unwrap();
                match st.key() {
                    Ok(k) => (st.device_id.clone(), k),
                    Err(_) => return (error(2, err::UNKNOWN), vec![]),
                }
            };
            let done = blocking(move || {
                let (ours, mut shared) = crypto::x25519_agree(&theirs)?;
                let mut info = ours.to_vec();
                info.extend_from_slice(device_id.as_bytes());
                info.extend_from_slice(&theirs);
                let mut sub = Tlv::new();
                sub.push(t::IDENTIFIER, device_id.as_bytes()).push(t::SIGNATURE, &lt.sign(&info));
                // (The pending exchange owns the secret from here: wiped when it goes.)
                let pending = VerifyPending { shared, ours, theirs };
                shared.zeroize();
                let key = hkdf_secret(&pending.shared, "Pair-Verify-Encrypt-Salt", "Pair-Verify-Encrypt-Info");
                let sealed = crypto::seal(&key, msg_nonce(b"PV-Msg02"), b"", &sub.encode());
                Some((pending, sealed))
            })
            .await
            .flatten();
            let Some((pending, sealed)) = done else { return (error(2, err::UNKNOWN), vec![]) };
            let mut m = Tlv::new();
            m.push_u8(t::STATE, 2).push(t::PUBLIC_KEY, &pending.ours).push(t::ENCRYPTED_DATA, &sealed);
            c.verify = Some(pending);
            (m.encode(), vec![])
        }
        Some(3) => {
            let Some(p) = c.verify.take() else { return (error(4, err::AUTHENTICATION), vec![]) };
            // (`p` is dropped, its secret wiped, on every way out below.)
            let key = hkdf_secret(&p.shared, "Pair-Verify-Encrypt-Salt", "Pair-Verify-Encrypt-Info");
            let Some(plain) = req.get(t::ENCRYPTED_DATA).and_then(|d| crypto::open(&key, msg_nonce(b"PV-Msg03"), b"", d)) else {
                return (error(4, err::AUTHENTICATION), vec![]);
            };
            let Some(sub) = Tlv::decode(&plain) else { return (error(4, err::AUTHENTICATION), vec![]) };
            let (Some(id), Some(sig)) = (sub.get(t::IDENTIFIER).map(<[u8]>::to_vec), sub.get(t::SIGNATURE).map(<[u8]>::to_vec)) else {
                return (error(4, err::AUTHENTICATION), vec![]);
            };
            let Some(id_str) = controller_id(&id) else { return (error(4, err::AUTHENTICATION), vec![]) };
            let Some(ltpk) = state.lock().unwrap().pairing(&id_str).and_then(|p| unhex(&p.ltpk)) else {
                return (error(4, err::AUTHENTICATION), vec![]);
            };
            let mut info = p.theirs.to_vec();
            info.extend_from_slice(&id);
            info.extend_from_slice(&p.ours);
            let good = blocking(move || crypto::verify(&ltpk, &info, &sig)).await.unwrap_or(false);
            // (Still paired: it may have been removed meanwhile.)
            if !good || state.lock().unwrap().pairing(&id_str).is_none() {
                return (error(4, err::AUTHENTICATION), vec![]);
            }
            let mut m = Tlv::new();
            m.push_u8(t::STATE, 4);
            (m.encode(), vec![After::Verified { shared: Secret(p.shared), controller: id_str }])
        }
        _ => (error(2, err::UNKNOWN), vec![]),
    }
}

/// `POST /pairings` from a verified controller: add, remove or list.
pub fn pairings(state: &mut State, controller: &str, body: &[u8]) -> (Vec<u8>, Vec<After>) {
    let Some(req) = Tlv::decode(body) else { return (error(2, err::UNKNOWN), vec![]) };
    if req.byte(t::STATE) != Some(1) {
        return (error(2, err::UNKNOWN), vec![]);
    }
    // Only admin controllers may see or change the pairings.
    if !state.pairing(controller).is_some_and(|p| p.admin) {
        return (error(2, err::AUTHENTICATION), vec![]);
    }
    let ok = || {
        let mut m = Tlv::new();
        m.push_u8(t::STATE, 2);
        m.encode()
    };
    match req.byte(t::METHOD) {
        // Add.
        Some(3) => {
            let (Some(id), Some(ltpk), Some(perm)) = (req.get(t::IDENTIFIER), req.get(t::PUBLIC_KEY), req.byte(t::PERMISSIONS)) else {
                return (error(2, err::UNKNOWN), vec![]);
            };
            let Some(id_str) = controller_id(id) else { return (error(2, err::UNKNOWN), vec![]) };
            // (32 bytes, canonical, not of small order: such a key could
            // "sign" any pair-verify.)
            if !crypto::good_ltpk(ltpk) {
                tracing::warn!("HomeKit: a controller tried to add another with a key that is not a usable Ed25519 key; refused");
                return (error(2, err::UNKNOWN), vec![]);
            }
            let admin = perm & 1 == 1;
            let ltpk_hex = hex(ltpk);
            // (Never no admin while paired: nobody could manage the pairings
            // or pair again until a reset. An admin leaves by removing itself.)
            let other_admin = state.pairings.iter().any(|p| p.admin && p.id != id_str);
            match state.pairings.iter_mut().find(|p| p.id == id_str) {
                Some(p) if p.ltpk != ltpk_hex => return (error(2, err::UNKNOWN), vec![]),
                Some(p) if p.admin && !admin && !other_admin => {
                    tracing::info!("HomeKit: refused to take admin rights from the last admin controller");
                    return (error(2, err::AUTHENTICATION), vec![]);
                }
                Some(p) => p.admin = admin,
                None => {
                    if state.pairings.len() >= MAX_PAIRINGS {
                        return (error(2, err::MAX_PEERS), vec![]);
                    }
                    state.pairings.push(Pairing { id: id_str, ltpk: ltpk_hex, admin });
                }
            }
            if let Err(e) = state.save() {
                tracing::warn!("HomeKit: {e:#}");
                return (error(2, err::UNKNOWN), vec![]);
            }
            tracing::info!("HomeKit: a controller added another one ({})", if admin { "admin" } else { "user" });
            (ok(), vec![])
        }
        // Remove.
        Some(4) => {
            let Some(id_str) = req.get(t::IDENTIFIER).and_then(controller_id) else { return (error(2, err::UNKNOWN), vec![]) };
            let before = state.pairings.len();
            state.pairings.retain(|p| p.id != id_str);
            if state.pairings.len() == before {
                // Removing one that is not there succeeds (HAP).
                return (ok(), vec![]);
            }
            let mut after = vec![After::EndController(id_str)];
            if !state.pairings.iter().any(|p| p.admin) {
                // No admin left: unpaired, with the same identity and setup
                // code (as HAP-NodeJS and HAP-python do).
                state.unpair();
                after.push(After::EndAll);
                tracing::info!("HomeKit: the last admin controller was removed; the bridge can be paired again");
            } else {
                tracing::info!("HomeKit: a controller was removed");
            }
            after.push(After::Advertise);
            if let Err(e) = state.save() {
                tracing::warn!("HomeKit: {e:#}");
            }
            (ok(), after)
        }
        // List.
        Some(5) => {
            let mut m = Tlv::new();
            m.push_u8(t::STATE, 2);
            for (i, p) in state.pairings.iter().enumerate() {
                if i > 0 {
                    m.push(t::SEPARATOR, &[]);
                }
                m.push(t::IDENTIFIER, p.id.as_bytes()).push(t::PUBLIC_KEY, &unhex(&p.ltpk).unwrap_or_default()).push_u8(t::PERMISSIONS, u8::from(p.admin));
            }
            (m.encode(), vec![])
        }
        _ => (error(2, err::UNKNOWN), vec![]),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::homekit::crypto::LongTermKey;
    use crate::homekit::testclient::{SetupClient, VerifyClient};

    pub fn state_in(name: &str) -> (Mutex<State>, std::path::PathBuf) {
        let dir = std::env::temp_dir().join(format!("as-hk-pair-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        (Mutex::new(State::load_or_create(&dir).unwrap()), dir)
    }

    fn ip(n: u8) -> IpAddr {
        IpAddr::from([192, 0, 2, n])
    }

    /// One accessory (the bridge) with its pairing window.
    struct Acc {
        st: Mutex<State>,
        dir: std::path::PathBuf,
        slot: Mutex<Slot>,
        win: Mutex<Window>,
        kind: Kind,
        tm: Timing,
    }

    impl Acc {
        fn new(name: &str) -> Acc {
            let (st, dir) = state_in(name);
            Acc { st, dir, slot: Mutex::new(Slot::default()), win: Mutex::new(Window::default()), kind: Kind::Bridge, tm: Timing::default() }
        }
        fn s(&self) -> Setup<'_> {
            Setup { state: &self.st, slot: &self.slot, window: &self.win, kind: &self.kind, timing: &self.tm }
        }
        fn open(&self) -> Open {
            self.win.lock().unwrap().open(self.kind.clone(), WINDOW).0
        }
        fn code(&self) -> String {
            self.st.lock().unwrap().setup_code.clone()
        }
        async fn m1(&self, conn: &mut ConnPairing) -> Vec<u8> {
            pair_setup(&self.s(), conn, &SetupClient::m1()).await.0
        }
        /// The whole pair-setup on one connection.
        async fn pair(&self, conn: &mut ConnPairing, ctl: &SetupClient) -> Result<(), u8> {
            let r = self.m1(conn).await;
            let mut ctl = ctl.clone();
            let (r, _) = pair_setup(&self.s(), conn, &ctl.m3(&r)?).await;
            let (r, _) = pair_setup(&self.s(), conn, &ctl.m5(&r)?).await;
            ctl.check_m6(&r)
        }
    }

    impl Drop for Acc {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.dir);
        }
    }

    fn err_of(r: &[u8]) -> Option<u8> {
        Tlv::decode(r).unwrap().byte(t::ERROR)
    }

    async fn verify(st: &Mutex<State>, conn: &mut ConnPairing, ctl: &SetupClient) -> Result<[u8; 32], u8> {
        let mut v = VerifyClient::new();
        let (r, _) = pair_verify(st, conn, &v.m1()).await;
        let acc_ltpk = st.lock().unwrap().key().unwrap().public();
        let (r, after) = pair_verify(st, conn, &v.m3(&r, &acc_ltpk, ctl)?).await;
        let shared = v.check_m4(&r)?;
        match after.as_slice() {
            [After::Verified { shared: s, controller }] if *controller == ctl.id => {
                assert_eq!(&s.0, &shared);
                Ok(shared)
            }
            other => panic!("{other:?}"),
        }
    }

    #[tokio::test]
    async fn setup_verify_and_pairings() {
        let a = Acc::new("full");
        let st = &a.st;
        let code = a.code();
        let device_id = st.lock().unwrap().device_id.clone();
        let c1 = SetupClient::new(&code, "C1", 1);
        a.open();
        a.pair(&mut ConnPairing::new(1, ip(1)), &c1).await.unwrap();
        assert!(st.lock().unwrap().paired() && a.slot.lock().unwrap().holder().is_none());
        // Pairing a second time is refused while paired, even with a window.
        let c2 = SetupClient::new(&code, "C2", 2);
        a.open();
        assert_eq!(a.pair(&mut ConnPairing::new(2, ip(2)), &c2).await, Err(err::UNAVAILABLE));
        a.win.lock().unwrap().close();
        // Verify works for the paired controller only.
        verify(st, &mut ConnPairing::new(3, ip(1)), &c1).await.unwrap();
        assert!(verify(st, &mut ConnPairing::new(4, ip(2)), &c2).await.is_err());
        let fake = SetupClient::new(&code, "C1", 9);
        assert!(verify(st, &mut ConnPairing::new(5, ip(2)), &fake).await.is_err(), "a wrong key for a known id");
        // One pair-verify M1 per connection.
        let mut conn = ConnPairing::new(6, ip(1));
        verify(st, &mut conn, &c1).await.unwrap();
        assert!(verify(st, &mut conn, &c1).await.is_err());
        // Add a non-admin (no window needed: an admin controller adds it), list,
        // and the non-admin may not change pairings.
        let mut add = Tlv::new();
        add.push_u8(t::STATE, 1).push_u8(t::METHOD, 3).push(t::IDENTIFIER, b"C2").push(t::PUBLIC_KEY, &c2.ltk.public()).push_u8(t::PERMISSIONS, 0);
        let (r, _) = pairings(&mut st.lock().unwrap(), "C1", &add.encode());
        assert!(is_success(&r));
        verify(st, &mut ConnPairing::new(7, ip(2)), &c2).await.unwrap();
        let mut list = Tlv::new();
        list.push_u8(t::STATE, 1).push_u8(t::METHOD, 5);
        let (r, _) = pairings(&mut st.lock().unwrap(), "C2", &list.encode());
        assert_eq!(err_of(&r), Some(err::AUTHENTICATION));
        let (r, _) = pairings(&mut st.lock().unwrap(), "C1", &list.encode());
        assert_eq!(Tlv::decode(&r).unwrap().items.iter().filter(|(ty, _)| *ty == t::IDENTIFIER).count(), 2);
        // A different key for a known id is refused.
        let mut clash = Tlv::new();
        clash.push_u8(t::STATE, 1).push_u8(t::METHOD, 3).push(t::IDENTIFIER, b"C2").push(t::PUBLIC_KEY, &[9; 32]).push_u8(t::PERMISSIONS, 1);
        let (r, _) = pairings(&mut st.lock().unwrap(), "C1", &clash.encode());
        assert_eq!(err_of(&r), Some(err::UNKNOWN));
        // Removing: a non-admin may not; the admin removes the non-admin, whose sessions end.
        let mut rm = Tlv::new();
        rm.push_u8(t::STATE, 1).push_u8(t::METHOD, 4).push(t::IDENTIFIER, b"C2");
        let (r, after) = pairings(&mut st.lock().unwrap(), "C2", &rm.encode());
        assert!(after.is_empty() && !is_success(&r), "a non-admin may not remove");
        let (_, after) = pairings(&mut st.lock().unwrap(), "C1", &rm.encode());
        assert_eq!(after, vec![After::EndController("C2".into()), After::Advertise]);
        assert!(verify(st, &mut ConnPairing::new(8, ip(2)), &c2).await.is_err(), "a removed controller cannot verify");
        // Removing the last admin unpairs: same identity and code.
        let mut rm1 = Tlv::new();
        rm1.push_u8(t::STATE, 1).push_u8(t::METHOD, 4).push(t::IDENTIFIER, b"C1");
        let (_, after) = pairings(&mut st.lock().unwrap(), "C1", &rm1.encode());
        assert!(after.contains(&After::EndAll) && after.contains(&After::Advertise));
        {
            let s = st.lock().unwrap();
            assert!(!s.paired() && s.pairable() && s.device_id == device_id && s.setup_code == code);
        }
        // Pairing again needs a window too.
        assert_eq!(a.pair(&mut ConnPairing::new(9, ip(1)), &c1).await, Err(err::UNAVAILABLE));
        a.open();
        a.pair(&mut ConnPairing::new(10, ip(1)), &c1).await.unwrap();
    }

    /// Ed25519 keys of small order "sign" anything with R of small order
    /// and S = 0 (ring takes them): refused in pair-setup's M5, in a
    /// /pairings add, and in pair-verify for one stored before.
    #[tokio::test]
    async fn small_order_controller_keys_are_refused() {
        let a = Acc::new("weak");
        let code = a.code();
        let identity = {
            let mut k = [0u8; 32];
            k[0] = 1;
            k
        };
        let null_sig = {
            let mut s = [0u8; 64];
            s[0] = 1;
            s
        };
        // The fuzzer's M5 (id "x", the order-4 key of 32 zero bytes, a signature
        // of zeros), then the identity key with R the identity and S = 0.
        let fuzzed = hex_bytes("010178032000000000000000000000000000000000000000000000000000000000000000000a4000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000");
        let mut forged = Tlv::new();
        forged.push(t::IDENTIFIER, b"weak").push(t::PUBLIC_KEY, &identity).push(t::SIGNATURE, &null_sig);
        for (n, plain) in [fuzzed, forged.encode()].into_iter().enumerate() {
            a.open();
            let mut conn = ConnPairing::new(20 + n as u64, ip(1));
            let mut ctl = SetupClient::new(&code, "C1", 1);
            let r = a.m1(&mut conn).await;
            let (r, _) = pair_setup(&a.s(), &mut conn, &ctl.m3(&r).unwrap()).await;
            assert!(is_success(&r));
            let (r, _) = pair_setup(&a.s(), &mut conn, &ctl.m5_plain(&plain)).await;
            assert_eq!(err_of(&r), Some(err::AUTHENTICATION), "M5 {n}");
            assert!(!a.st.lock().unwrap().paired());
        }
        // A real controller pairs; adding the identity key as another is refused.
        let c1 = SetupClient::new(&code, "C1", 1);
        a.open();
        a.pair(&mut ConnPairing::new(30, ip(1)), &c1).await.unwrap();
        let add = |key: &[u8]| {
            let mut m = Tlv::new();
            m.push_u8(t::STATE, 1).push_u8(t::METHOD, 3).push(t::IDENTIFIER, b"weak").push(t::PUBLIC_KEY, key).push_u8(t::PERMISSIONS, 0);
            m.encode()
        };
        for key in [identity, [0u8; 32]] {
            let (r, _) = pairings(&mut a.st.lock().unwrap(), "C1", &add(&key));
            assert!(!is_success(&r));
        }
        assert!(a.st.lock().unwrap().pairing("weak").is_none());
        // One stored before this check (an older version took it): pair-verify
        // with the forged signature is refused.
        a.st.lock().unwrap().pairings.push(Pairing { id: "weak".into(), ltpk: hex(&identity), admin: false });
        let mut conn = ConnPairing::new(31, ip(2));
        let mut v = VerifyClient::new();
        let (r, _) = pair_verify(&a.st, &mut conn, &v.m1()).await;
        let (r, after) = pair_verify(&a.st, &mut conn, &v.m3_signed(&r, b"weak", &null_sig).unwrap()).await;
        assert_eq!(err_of(&r), Some(err::AUTHENTICATION));
        assert!(after.is_empty());
    }

    fn hex_bytes(s: &str) -> Vec<u8> {
        (0..s.len()).step_by(2).map(|i| u8::from_str_radix(&s[i..i + 2], 16).unwrap()).collect()
    }

    #[test]
    fn the_last_admin_cannot_lose_its_admin_rights() {
        let (st, dir) = state_in("admins");
        let mut s = st.lock().unwrap();
        let (a, b) = (LongTermKey::from_seed(&[1; 32]).unwrap().public(), LongTermKey::from_seed(&[2; 32]).unwrap().public());
        s.pairings = vec![Pairing { id: "A".into(), ltpk: hex(&a), admin: true }];
        let set = |id: &[u8], key: &[u8; 32], perm: u8| {
            let mut m = Tlv::new();
            m.push_u8(t::STATE, 1).push_u8(t::METHOD, 3).push(t::IDENTIFIER, id).push(t::PUBLIC_KEY, key).push_u8(t::PERMISSIONS, perm);
            m.encode()
        };
        // The only admin demoting itself: refused, still an admin.
        let (r, _) = pairings(&mut s, "A", &set(b"A", &a, 0));
        assert_eq!(err_of(&r), Some(err::AUTHENTICATION));
        assert!(s.pairing("A").unwrap().admin);
        // A user added, still only one admin: refused again.
        assert!(is_success(&pairings(&mut s, "A", &set(b"B", &b, 0)).0));
        assert!(!is_success(&pairings(&mut s, "A", &set(b"A", &a, 0)).0));
        assert!(s.pairing("A").unwrap().admin);
        // With a second admin, either may step down, but not both.
        assert!(is_success(&pairings(&mut s, "A", &set(b"B", &b, 1)).0));
        assert!(is_success(&pairings(&mut s, "A", &set(b"A", &a, 0)).0));
        assert!(!s.pairing("A").unwrap().admin);
        assert!(!is_success(&pairings(&mut s, "B", &set(b"B", &b, 0)).0));
        assert!(s.pairings.iter().any(|p| p.admin));
        // Re-adding the last admin as an admin is fine.
        assert!(is_success(&pairings(&mut s, "B", &set(b"B", &b, 1)).0));
        drop(s);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn refused_outside_the_window() {
        let a = Acc::new("closed");
        let good = SetupClient::new(&a.code(), "C1", 1);
        // Never opened: refused at M1 with Unavailable, nothing held.
        let mut early = ConnPairing::new(1, ip(1));
        assert_eq!(err_of(&a.m1(&mut early).await), Some(err::UNAVAILABLE));
        assert!(a.slot.lock().unwrap().holder().is_none());
        // The same connection may ask again once it is opened.
        a.open();
        assert!(is_success(&a.m1(&mut early).await), "the connection asks again once open");
        a.slot.lock().unwrap().clear();
        a.win.lock().unwrap().close();
        assert_eq!(a.pair(&mut ConnPairing::new(2, ip(1)), &good).await, Err(err::UNAVAILABLE));
        // Open for another accessory (a standalone camera): still refused here.
        a.win.lock().unwrap().open(Kind::Camera("Other".into()), WINDOW);
        assert_eq!(err_of(&a.m1(&mut ConnPairing::new(3, ip(1))).await), Some(err::UNAVAILABLE));
        // Opened, then stopped by the admin.
        a.open();
        a.win.lock().unwrap().close();
        assert_eq!(err_of(&a.m1(&mut ConnPairing::new(4, ip(1))).await), Some(err::UNAVAILABLE));
        // A window opened, and an M1 that was let in: Stop pressed before M5,
        // so the pairing is not stored.
        a.open();
        let mut conn = ConnPairing::new(5, ip(1));
        let r = a.m1(&mut conn).await;
        let mut ctl = good.clone();
        let (r, _) = pair_setup(&a.s(), &mut conn, &ctl.m3(&r).unwrap()).await;
        a.win.lock().unwrap().close();
        let (r, _) = pair_setup(&a.s(), &mut conn, &ctl.m5(&r).unwrap()).await;
        assert_eq!(err_of(&r), Some(err::UNAVAILABLE));
        assert!(!a.st.lock().unwrap().paired());
        assert_eq!(a.st.lock().unwrap().failed_tries, 0, "no try counted");
    }

    #[tokio::test]
    async fn the_window_expires() {
        let a = Acc::new("expire");
        let good = SetupClient::new(&a.code(), "C1", 1);
        let o = a.open();
        assert!(o.until > Instant::now() + WINDOW - Duration::from_secs(5));
        assert!(a.win.lock().unwrap().open_for(&Kind::Bridge));
        // Five minutes later it is closed: refused, and forgotten by `expire`.
        a.win.lock().unwrap().age(WINDOW);
        assert!(!a.win.lock().unwrap().open_for(&Kind::Bridge));
        assert!(a.win.lock().unwrap().current().is_none());
        assert_eq!(a.pair(&mut ConnPairing::new(1, ip(1)), &good).await, Err(err::UNAVAILABLE));
        assert_eq!(a.win.lock().unwrap().expire(), Some(Kind::Bridge));
        assert_eq!(a.win.lock().unwrap().expire(), None);
        // A pair-setup begun in the window cannot finish after it ended.
        a.open();
        let mut conn = ConnPairing::new(2, ip(1));
        let r = a.m1(&mut conn).await;
        let mut ctl = good.clone();
        let (r, _) = pair_setup(&a.s(), &mut conn, &ctl.m3(&r).unwrap()).await;
        a.win.lock().unwrap().age(WINDOW);
        let (r, _) = pair_setup(&a.s(), &mut conn, &ctl.m5(&r).unwrap()).await;
        assert_eq!(err_of(&r), Some(err::UNAVAILABLE));
        assert!(!a.st.lock().unwrap().paired());
        // Opening it again gives a fresh five minutes.
        let o = a.open();
        assert!(o.until > Instant::now() + WINDOW - Duration::from_secs(5));
        a.pair(&mut ConnPairing::new(3, ip(1)), &good).await.unwrap();
    }

    #[tokio::test]
    async fn the_window_ends_when_a_pairing_succeeds() {
        let a = Acc::new("success");
        let o = a.open();
        a.pair(&mut ConnPairing::new(1, ip(1)), &SetupClient::new(&a.code(), "C1", 1)).await.unwrap();
        assert!(a.win.lock().unwrap().current().is_none(), "closed on success");
        // (A window opened again meanwhile is not closed by an older one.)
        let o2 = a.open();
        assert!(!a.win.lock().unwrap().close_if(o.id));
        assert!(a.win.lock().unwrap().close_if(o2.id));
        // Unpaired again: it stays closed until an admin opens it.
        a.st.lock().unwrap().unpair();
        assert_eq!(err_of(&a.m1(&mut ConnPairing::new(2, ip(1))).await), Some(err::UNAVAILABLE));
    }

    #[tokio::test]
    async fn stop_and_start_again_refuses_the_address() {
        let a = Acc::new("restart");
        let owner = SetupClient::new(&a.code(), "Owner", 1);
        let o = a.open();
        // The attacker (address 1) holds the pair-setup; the owner is told busy.
        let mut hog = ConnPairing::new(1, ip(1));
        let r = a.m1(&mut hog).await;
        assert!(is_success(&r));
        assert_eq!(err_of(&a.m1(&mut ConnPairing::new(2, ip(2))).await), Some(err::BUSY));
        assert_eq!(a.slot.lock().unwrap().exchange().map(|e| e.0), Some(ip(1)));
        // The admin sees address 1 and presses Stop and start again.
        let fresh = a.win.lock().unwrap().restart(ip(1), WINDOW).unwrap();
        assert!(fresh.id == o.id && fresh.refused == vec![ip(1)] && fresh.target == Kind::Bridge);
        assert!(fresh.until > o.until, "a fresh 5 minutes");
        assert_eq!(a.slot.lock().unwrap().stop(Some(ip(1))), Some(1), "its exchange ends");
        // Its exchange cannot go on (it was ended), and it may not start again.
        let mut hog_ctl = owner.clone();
        let (r, _) = pair_setup(&a.s(), &mut hog, &hog_ctl.m3(&r).unwrap()).await;
        assert_eq!(err_of(&r), Some(err::UNKNOWN));
        assert_eq!(err_of(&a.m1(&mut ConnPairing::new(3, ip(1))).await), Some(err::UNAVAILABLE));
        // The owner pairs.
        a.pair(&mut ConnPairing::new(4, ip(2)), &owner).await.unwrap();
        assert!(a.win.lock().unwrap().current().is_none());
        // Stopping only an exchange from that address.
        let a = Acc::new("restart2");
        a.open();
        assert!(is_success(&a.m1(&mut ConnPairing::new(1, ip(5))).await));
        assert_eq!(a.slot.lock().unwrap().stop(Some(ip(6))), None);
        assert_eq!(a.slot.lock().unwrap().holder(), Some(1));
        // A stopped address that already proved the code cannot finish either.
        let mut conn = ConnPairing::new(2, ip(7));
        a.slot.lock().unwrap().clear();
        let r = a.m1(&mut conn).await;
        let mut ctl = SetupClient::new(&a.code(), "X", 3);
        let (r, _) = pair_setup(&a.s(), &mut conn, &ctl.m3(&r).unwrap()).await;
        a.win.lock().unwrap().restart(ip(7), WINDOW).unwrap();
        let (r, _) = pair_setup(&a.s(), &mut conn, &ctl.m5(&r).unwrap()).await;
        assert_eq!(err_of(&r), Some(err::UNAVAILABLE));
        assert!(!a.st.lock().unwrap().paired());
        // Opening it again for the same accessory keeps the refused addresses;
        // once it closed, a new window starts with none.
        assert_eq!(a.open().refused, vec![ip(7)]);
        a.win.lock().unwrap().close();
        assert!(a.open().refused.is_empty());
        // Nothing to start again when it is closed.
        a.win.lock().unwrap().close();
        assert!(a.win.lock().unwrap().restart(ip(1), WINDOW).is_none());
    }

    #[tokio::test]
    async fn stop_and_start_again_for_another_address_leaves_the_owner_pairing() {
        let a = Acc::new("restart-owner");
        let owner = SetupClient::new(&a.code(), "Owner", 1);
        let o = a.open();
        // The owner's phone holds the pair-setup and proved the code (M4).
        let mut conn = ConnPairing::new(1, ip(2));
        let r = a.m1(&mut conn).await;
        let mut ctl = owner.clone();
        let (r, _) = pair_setup(&a.s(), &mut conn, &ctl.m3(&r).unwrap()).await;
        assert!(is_success(&r));
        // The admin stops another address (the one shown before the owner took over).
        let fresh = a.win.lock().unwrap().restart(ip(1), WINDOW).unwrap();
        assert_eq!(fresh.id, o.id);
        assert_eq!(a.slot.lock().unwrap().stop(Some(ip(1))), None, "the owner's exchange is not ended");
        // Opening it again for the same accessory keeps the window too.
        assert_eq!(a.open().id, o.id);
        // The owner's M5 succeeds.
        let (r, after) = pair_setup(&a.s(), &mut conn, &ctl.m5(&r).unwrap()).await;
        assert_eq!(err_of(&r), None);
        assert_eq!(after, vec![After::Advertise]);
        assert!(a.st.lock().unwrap().paired());
        assert!(a.win.lock().unwrap().current().is_none(), "closed on success");
        // Closed, then opened again: a new window.
        assert!(a.open().id != o.id);
    }

    #[tokio::test]
    async fn one_m1_per_connection_and_one_setup_at_a_time() {
        let a = Acc::new("slot");
        a.open();
        let mut c1 = ConnPairing::new(1, ip(1));
        assert!(is_success(&a.m1(&mut c1).await));
        // The same connection may not start again.
        assert!(!is_success(&a.m1(&mut c1).await));
        // Another connection is told "busy" while it is fresh...
        assert_eq!(err_of(&a.m1(&mut ConnPairing::new(2, ip(2))).await), Some(err::BUSY));
        // ... and may take it over once no M3 came within the window.
        a.slot.lock().unwrap().age(a.tm.m3_window + Duration::from_secs(1));
        assert!(is_success(&a.m1(&mut ConnPairing::new(3, ip(2))).await));
        assert_eq!(a.slot.lock().unwrap().holder(), Some(3));
        // Once it is free, anyone may start (no waiting for the one that lost it).
        a.slot.lock().unwrap().release(3);
        assert!(is_success(&a.m1(&mut ConnPairing::new(4, ip(1))).await));
        // M3/M5 from a connection that does not hold the exchange.
        let mut m5 = Tlv::new();
        m5.push_u8(t::STATE, 5).push(t::ENCRYPTED_DATA, &[0; 40]);
        let (r, _) = pair_setup(&a.s(), &mut ConnPairing::new(5, ip(3)), &m5.encode()).await;
        assert_eq!(err_of(&r), Some(err::UNKNOWN));
    }

    #[tokio::test]
    async fn wrong_codes_slow_everyone_and_stop_at_100() {
        let a = Acc::new("wrong");
        a.open();
        let code = a.code();
        let wrong = if code == "111-22-333" { "111-22-334" } else { "111-22-333" };
        let bad = SetupClient::new(wrong, "C1", 1);
        let mut first = ConnPairing::new(1, ip(1));
        assert_eq!(a.pair(&mut first, &bad).await, Err(err::AUTHENTICATION));
        assert_eq!(a.st.lock().unwrap().failed_tries, 1);
        assert!(a.slot.lock().unwrap().holder().is_none());
        assert!(a.win.lock().unwrap().current().is_some(), "a wrong code leaves the window open");
        // Right after, nobody may start (another address, the right code).
        let r = Tlv::decode(&a.m1(&mut ConnPairing::new(2, ip(2))).await).unwrap();
        assert_eq!(r.byte(t::ERROR), Some(err::BACKOFF));
        assert!(r.byte(t::RETRY_DELAY).is_some());
        // Once the wait is over, the connection with the wrong code may try again.
        tokio::time::sleep(wrong_code_delay(1) + Duration::from_millis(50)).await;
        assert!(is_success(&a.m1(&mut first).await), "a retry after a wrong code");
        // After 100 wrong codes: refused until reset, even with the right code.
        a.slot.lock().unwrap().clear();
        a.st.lock().unwrap().failed_tries = MAX_TRIES - 1;
        let mut conn = ConnPairing::new(3, ip(3));
        let r = a.m1(&mut conn).await;
        let mut b2 = bad.clone();
        let (_, after) = pair_setup(&a.s(), &mut conn, &b2.m3(&r).unwrap()).await;
        assert!(after.contains(&After::Advertise));
        a.slot.lock().unwrap().clear();
        let good = SetupClient::new(&code, "C1", 1);
        assert_eq!(a.pair(&mut ConnPairing::new(4, ip(4)), &good).await, Err(err::MAX_TRIES));
        assert!(!a.st.lock().unwrap().pairable());
    }

    #[tokio::test]
    async fn garbage_never_panics() {
        use rand::{Rng, SeedableRng};
        let a = Acc::new("fuzz");
        let mut r = rand::rngs::StdRng::seed_from_u64(3);
        for i in 0..800u64 {
            if i % 2 == 0 {
                a.open();
            } else {
                a.win.lock().unwrap().close();
            }
            let n = r.gen_range(0..120);
            let mut buf: Vec<u8> = (0..n).map(|_| r.gen()).collect();
            if r.gen_bool(0.5) {
                let mut head = vec![t::STATE, 1, r.gen_range(0..7)];
                head.append(&mut buf);
                buf = head;
            }
            let mut c = ConnPairing::new(i, ip((i % 200) as u8));
            let _ = pair_setup(&a.s(), &mut c, &buf).await;
            let _ = pair_verify(&a.st, &mut c, &buf).await;
            a.slot.lock().unwrap().clear();
            let mut s = a.st.lock().unwrap();
            s.pairings = vec![Pairing { id: "C1".into(), ltpk: hex(&[1; 32]), admin: true }];
            let _ = pairings(&mut s, "C1", &buf);
            s.pairings.clear();
            s.failed_tries = 0;
        }
        let _ = LongTermKey::from_seed(&[0; 32]);
    }

    /// The secrets of a pair-setup are kept no longer than needed: the SRP
    /// server (b and the verifier) goes as soon as M3's proof is right, the
    /// slot gives K up when M5 begins (no copy stays there while M5 is
    /// checked), and nothing is left once M5 fails.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn pair_setup_secrets_are_dropped_when_done() {
        let a = Arc::new(Acc::new("wipe"));
        a.open();
        let mut ctl = SetupClient::new(&a.code(), "C1", 1);
        let mut conn = ConnPairing::new(1, ip(1));
        let r = a.m1(&mut conn).await;
        let srp = Arc::downgrade(a.slot.lock().unwrap().ex.as_ref().unwrap().srp.as_ref().unwrap());
        let (r, _) = pair_setup(&a.s(), &mut conn, &ctl.m3(&r).unwrap()).await;
        assert_eq!(err_of(&r), None);
        {
            let slot = a.slot.lock().unwrap();
            let ex = slot.ex.as_ref().unwrap();
            assert!(ex.key.is_some() && ex.proven && ex.srp.is_none());
        }
        assert!(srp.upgrade().is_none(), "the SRP server is dropped after a right M3");
        // M5 begins (held at the state, which the test keeps locked): the
        // slot no longer has K.
        let held = a.st.lock().unwrap();
        let mut bad = Tlv::new();
        bad.push_u8(t::STATE, 5).push(t::ENCRYPTED_DATA, &[0u8; 40]);
        let (a2, body) = (a.clone(), bad.encode());
        let m5 = tokio::spawn(async move { pair_setup(&a2.s(), &mut conn, &body).await.0 });
        let deadline = Instant::now() + Duration::from_secs(5);
        while !a.slot.lock().unwrap().ex.as_ref().is_some_and(|e| e.busy) {
            assert!(Instant::now() < deadline, "M5 never began");
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        assert!(a.slot.lock().unwrap().ex.as_ref().unwrap().key.is_none(), "K is out of the slot while M5 is checked");
        drop(held);
        let r = tokio::time::timeout(Duration::from_secs(5), m5).await.unwrap().unwrap();
        assert_eq!(err_of(&r), Some(err::AUTHENTICATION));
        assert!(a.slot.lock().unwrap().ex.is_none(), "a failed M5 leaves nothing behind");
        // No second M5 on that exchange, and no M3 after a right one.
        let mut conn = ConnPairing::new(1, ip(1));
        let (r, _) = pair_setup(&a.s(), &mut conn, &bad.encode()).await;
        assert_eq!(err_of(&r), Some(err::UNKNOWN));
    }

    /// A pair-setup that lapsed and is taken over, or whose connection
    /// closed, leaves its SRP values (and K) behind nowhere.
    #[tokio::test]
    async fn a_lapsed_or_closed_pair_setup_drops_its_secrets() {
        let a = Acc::new("wipe-lapse");
        a.open();
        let mut first = ConnPairing::new(1, ip(1));
        a.m1(&mut first).await;
        let srp = Arc::downgrade(a.slot.lock().unwrap().ex.as_ref().unwrap().srp.as_ref().unwrap());
        a.slot.lock().unwrap().age(a.tm.m3_window + Duration::from_secs(1));
        let mut second = ConnPairing::new(2, ip(2));
        assert!(is_success(&a.m1(&mut second).await), "takes the lapsed one over");
        assert!(srp.upgrade().is_none(), "the lapsed exchange's SRP server is gone");
        // The connection closes after a right M3: K goes with it.
        a.slot.lock().unwrap().clear();
        let mut ctl = SetupClient::new(&a.code(), "C2", 2);
        let mut third = ConnPairing::new(3, ip(3));
        let r = a.m1(&mut third).await;
        let (r, _) = pair_setup(&a.s(), &mut third, &ctl.m3(&r).unwrap()).await;
        assert_eq!(err_of(&r), None);
        assert!(a.slot.lock().unwrap().ex.as_ref().unwrap().key.is_some());
        a.slot.lock().unwrap().release(3);
        assert!(a.slot.lock().unwrap().ex.is_none());
    }

    /// Pair-verify's shared secret is never printed (After is Debug).
    #[test]
    fn the_shared_secret_is_never_printed() {
        let v = After::Verified { shared: Secret([0xab; 32]), controller: "C1".into() };
        let text = format!("{v:?}");
        assert!(!text.contains("171") && !text.to_lowercase().contains("ab, ") && text.contains("Secret(..)"), "{text}");
        // (Compared in constant time; equal only when every byte is.)
        let mut other = [0xab; 32];
        assert!(Secret(other) == Secret([0xab; 32]));
        other[31] = 0xaa;
        assert!(Secret(other) != Secret([0xab; 32]));
    }
}
