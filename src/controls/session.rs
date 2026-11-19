//! One signed-in Reolink web session per device, shared by everything that
//! sends it short commands (the hub/recorder state poller and the camera
//! controls). Devices allow only a few signed-in sessions, and some firmware
//! rejects commands sent in parallel, so each device's commands take turns
//! through its session's lock.
//!
//! Recording downloads from a card (cardfill.rs) keep their own session: a
//! download takes minutes, and a siren's stop must never wait behind one.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use crate::reolink::ReolinkClient;

/// A session nobody has used for this long is signed out.
const IDLE: Duration = Duration::from_secs(15 * 60);

pub struct Shared {
    client: Mutex<ReolinkClient>,
    used: Mutex<Instant>,
}

impl Shared {
    /// As `with`, but only when the device is free within `wait`: a state read
    /// for a page gives way to an action (a siren's stop must not wait behind
    /// many page reads). None: busy.
    pub fn try_with<R>(&self, wait: std::time::Duration, f: impl FnOnce(&mut ReolinkClient) -> R) -> Option<R> {
        let until = Instant::now() + wait;
        loop {
            match self.client.try_lock() {
                Ok(mut c) => {
                    *self.used.lock().unwrap_or_else(|e| e.into_inner()) = Instant::now();
                    return Some(f(&mut c));
                }
                Err(std::sync::TryLockError::Poisoned(p)) => return Some(f(&mut p.into_inner())),
                Err(std::sync::TryLockError::WouldBlock) => {
                    if Instant::now() >= until {
                        return None;
                    }
                    std::thread::sleep(std::time::Duration::from_millis(10));
                }
            }
        }
    }

    /// Run `f` with the device's client; other users wait their turn.
    pub fn with<R>(&self, f: impl FnOnce(&mut ReolinkClient) -> R) -> R {
        *self.used.lock().unwrap_or_else(|e| e.into_inner()) = Instant::now();
        let mut c = self.client.lock().unwrap_or_else(|e| e.into_inner());
        let r = f(&mut c);
        *self.used.lock().unwrap_or_else(|e| e.into_inner()) = Instant::now();
        r
    }
}

type Registry = HashMap<String, Arc<Shared>>;
static SESSIONS: Mutex<Option<Registry>> = Mutex::new(None);

/// The key of a device's session: its address and account (a changed
/// password is another session; the old one is signed out when idle).
fn key(base: &str, user: &str, pass: &str) -> String {
    use sha2::Digest;
    let h = sha2::Sha256::digest(format!("{user}\u{0}{pass}").as_bytes());
    format!("{}|{}", base.trim_end_matches('/'), h[..8].iter().map(|b| format!("{b:02x}")).collect::<String>())
}

/// The shared session for a device at `base` ("https://host[:port]").
pub fn get(base: &str, user: &str, pass: &str) -> Arc<Shared> {
    let k = key(base, user, pass);
    let mut g = SESSIONS.lock().unwrap_or_else(|e| e.into_inner());
    g.get_or_insert_with(Default::default)
        .entry(k)
        .or_insert_with(|| Arc::new(Shared { client: Mutex::new(ReolinkClient::with_base(base, user, pass)), used: Mutex::new(Instant::now()) }))
        .clone()
}

/// Sign out sessions nobody holds that have been idle a while (called now
/// and then by the controls' timer).
pub fn reap() {
    let idle: Vec<Arc<Shared>> = {
        let mut g = SESSIONS.lock().unwrap_or_else(|e| e.into_inner());
        let Some(m) = g.as_mut() else { return };
        let gone: Vec<String> = m
            .iter()
            .filter(|(_, s)| Arc::strong_count(s) == 1 && s.used.lock().unwrap_or_else(|e| e.into_inner()).elapsed() > IDLE)
            .map(|(k, _)| k.clone())
            .collect();
        gone.iter().filter_map(|k| m.remove(k)).collect()
    };
    for s in idle {
        s.with(|c| c.logout());
    }
}

/// Sign every session out (shutdown).
pub fn logout_all() {
    let all: Vec<Arc<Shared>> = SESSIONS.lock().unwrap_or_else(|e| e.into_inner()).as_mut().map(|m| m.drain().map(|(_, s)| s).collect()).unwrap_or_default();
    for s in all {
        s.with(|c| c.logout());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn one_session_per_device_and_account() {
        let a = get("http://127.0.0.1:9/", "u", "p");
        let b = get("http://127.0.0.1:9", "u", "p");
        assert!(Arc::ptr_eq(&a, &b), "the same device shares one");
        let c = get("http://127.0.0.1:9", "u", "other");
        assert!(!Arc::ptr_eq(&a, &c), "a changed password is another session");
        assert_ne!(key("http://h", "u", "p"), key("http://h", "u", "q"));
        assert!(!key("http://h", "u", "secret").contains("secret"));
    }
}
