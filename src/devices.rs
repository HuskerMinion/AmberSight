//! Signing in from a new device: the alert admins get, and the labels of the
//! list of signed-in devices.
//!
//! A browser that signs in gets a long-lived random cookie. Only its hash is
//! kept, per account, and only after a sign-in has fully succeeded. The cookie
//! gives no access: it only says "this browser signed in to this account
//! before", and is only ever looked at together with a completed sign-in for
//! that same account. Every path that makes a session goes through
//! `complete_sign_in`, so none can be missed.
//!
//! Nothing a browser sends is ever shown or kept as it came: the User-Agent
//! becomes one of a fixed set of labels (`device_label`).

use crate::auth::{Identity, SessionMeta};
use crate::engine::Engine;
use axum::http::{header, HeaderMap};
use sha2::{Digest, Sha256};
use std::net::IpAddr;
use std::sync::Arc;

pub const DEVICE_COOKIE: &str = "as_device";
/// About two years.
const DEVICE_COOKIE_SECS: u64 = 2 * 365 * 86400;
/// Browsers remembered per account; past this the least recently used go.
pub const MAX_DEVICES: usize = 50;
/// New devices of one account within this long are told in one message.
const WINDOW_SECS: u64 = 60;
const TO_DO: &str = "If this was not you: open Your account, sign that device out, and change the password.";
/// A program using the password has no session to sign out.
const TO_DO_DIRECT: &str = "If this was not you: change the password.";
/// For the message after the first one, which can hold both kinds.
const TO_DO_LATER: &str = "If this was not you: change the password, and in Your account sign out any device you do not know.";

/// A short "Browser on System" for a User-Agent: always one of a fixed set of
/// words, whatever the header holds (it is whatever the sender wants it to be).
pub fn device_label(ua: &str) -> String {
    let browser = if ua.contains("Edg/") || ua.contains("EdgA/") || ua.contains("EdgiOS/") {
        "Edge"
    } else if ua.contains("OPR/") {
        "Opera"
    } else if ua.contains("Firefox/") || ua.contains("FxiOS/") {
        "Firefox"
    } else if ua.contains("Chrome/") || ua.contains("CriOS/") {
        "Chrome"
    } else if ua.contains("Safari/") {
        "Safari"
    } else if ua.starts_with("curl/") {
        "curl"
    } else {
        return "Unknown browser".to_string();
    };
    let os = if ua.contains("iPhone") {
        Some("iPhone")
    } else if ua.contains("iPad") {
        Some("iPad")
    } else if ua.contains("Android") {
        Some("Android")
    } else if ua.contains("CrOS") {
        Some("ChromeOS")
    } else if ua.contains("Windows") {
        Some("Windows")
    } else if ua.contains("Mac OS X") || ua.contains("Macintosh") {
        Some("Mac")
    } else if ua.contains("Linux") || ua.contains("X11") {
        Some("Linux")
    } else {
        None
    };
    match os {
        Some(o) => format!("{browser} on {o}"),
        None => browser.to_string(),
    }
}

fn random_cookie() -> String {
    use rand::RngCore;
    let mut b = [0u8; 32];
    rand::rngs::OsRng.fill_bytes(&mut b);
    b.iter().map(|x| format!("{x:02x}")).collect()
}

/// What is kept of a browser's cookie.
pub(crate) fn browser_hash(cookie: &str) -> String {
    hex_sha(&format!("device:{cookie}"))
}

fn hex_sha(s: &str) -> String {
    Sha256::digest(s.as_bytes()).iter().map(|x| format!("{x:02x}")).collect()
}

/// The device cookie the request carries, if it has the shape this server
/// makes (64 hex digits): anything else is ignored.
fn cookie_value(headers: &HeaderMap) -> Option<String> {
    headers
        .get_all(header::COOKIE)
        .iter()
        .filter_map(|v| v.to_str().ok())
        .flat_map(|h| h.split(';'))
        .filter_map(|c| c.trim().strip_prefix(DEVICE_COOKIE).and_then(|r| r.strip_prefix('=')))
        .find(|v| v.len() == 64 && v.bytes().all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b)))
        .map(|v| v.to_string())
}

/// The `Set-Cookie` value that keeps this browser's device cookie.
pub fn set_cookie(value: &str, secure: bool) -> String {
    format!("{DEVICE_COOKIE}={value}; HttpOnly; SameSite=Strict; Path=/; Max-Age={DEVICE_COOKIE_SECS}{}", if secure { "; Secure" } else { "" })
}

/// How a sign-in was done, in words for the message.
pub enum How {
    Password,
    TwoStepCode,
    /// With how many are left.
    RecoveryCode(usize),
    QrCode,
    LinkedServer,
}

impl How {
    fn words(&self) -> String {
        match self {
            How::Password => "the password".to_string(),
            How::TwoStepCode => "the password and a two-step code".to_string(),
            How::RecoveryCode(n) => format!("a recovery code ({n} left)"),
            How::QrCode => "a sign-in QR code".to_string(),
            How::LinkedServer => "a link from another AmberSight server".to_string(),
        }
    }
}

/// What the request that signs in says about where it comes from.
pub struct SignInCtx {
    pub ip: IpAddr,
    label: String,
    cookie: Option<String>,
    /// The account of a session cookie the request carries that is valid now.
    session_user: Option<String>,
}

impl SignInCtx {
    pub fn new(engine: &Engine, headers: &HeaderMap, ip: IpAddr) -> SignInCtx {
        let ua = headers.get(header::USER_AGENT).and_then(|v| v.to_str().ok()).unwrap_or("");
        let acc = crate::auth::accounts(&engine.config.lock().unwrap());
        SignInCtx { ip, label: device_label(ua), cookie: cookie_value(headers), session_user: crate::auth::session_from_headers(headers, &acc).map(|(_, id)| id.user) }
    }
}

/// A new session, and the cookie for the browser it was made in.
pub struct Started {
    pub token: String,
    pub device_cookie: String,
}

/// The one place a sign-in becomes a session: after the password (and code, if
/// the account has them) were right. None if the account changed meanwhile.
/// Remembers the browser, and tells admins if it is a new one. A sign-in with
/// a recovery code is told in the same message, not in two.
pub fn complete_sign_in(engine: &Arc<Engine>, ctx: &SignInCtx, id: Identity, generation: u64, how: How) -> Option<Started> {
    let user = id.user.clone();
    // (Asked before the session exists: it would count itself.)
    let used_before = crate::auth::has_sessions(&user) || engine.db.signed_in_before(&user).unwrap_or(true);
    let cookie = ctx.cookie.clone().unwrap_or_else(random_cookie);
    let hash = browser_hash(&cookie);
    let meta = SessionMeta::new(ctx.ip, ctx.label.clone(), hash.clone());
    let token = crate::auth::new_session_if_current(id, generation, meta)?;
    let covered = ctx.session_user.as_deref() == Some(user.as_str());
    let new = remember(engine, &user, &hash, &ctx.label, used_before, covered);
    if new {
        let _ = engine.db.insert_audit(&user, &ctx.ip.to_string(), "sign-in from a new device", &ctx.label, 200);
        announce(engine, &user, &ctx.label, ctx.ip, &how.words(), TO_DO);
    } else if let How::RecoveryCode(n) = how {
        crate::account_http::tell_admins(engine, format!("{user} signed in with a recovery code"), format!("{user} signed in with a recovery code from {}; {n} left. If that was not them, someone has the password and those codes: disable the account in Users.", ctx.ip));
    }
    Some(Started { token, device_cookie: cookie })
}

/// Write the browser down. True: tell admins. Not told when the browser is
/// known, when the same request also carries a live session of the account
/// (so after an upgrade a browser is told once, the next time it signs in
/// without a session), or when it is the account's very first sign-in.
fn remember(engine: &Engine, user: &str, hash: &str, label: &str, used_before: bool, covered: bool) -> bool {
    match engine.db.device_seen(user, hash, label, chrono::Utc::now().timestamp(), MAX_DEVICES) {
        Ok(seen) => !seen.known && !covered && (seen.had_any || used_before),
        Err(e) => {
            // Cannot tell: better one message too many.
            tracing::error!("could not note a sign-in device: {e:#}");
            !covered
        }
    }
}

fn announce(engine: &Arc<Engine>, user: &str, label: &str, ip: IpAddr, how: &str, todo: &'static str) {
    let u = user.to_string();
    crate::account_http::tell_gathered(
        engine,
        format!("new-device:{user}"),
        format!("{label} from {ip} with {how}"),
        std::time::Duration::from_secs(WINDOW_SECS),
        (format!("new device signed in to {user}"), format!("{user} signed in on a device this account has not used before: {label}, from {ip}, with {how}. {todo}")),
        move |items, _| (format!("more new devices signed in to {u}"), format!("{u} also signed in on new devices: {items}. {TO_DO_LATER}")),
    );
}

/// Someone used an account's password directly (HTTP Basic or RTSP: no web page,
/// no cookie, no session), the first time since the server started from this
/// network. Runs on a thread of its own, never in the request. Once per account
/// and network in the same list of known devices.
pub fn note_direct(engine: &Arc<Engine>, user: &str, ip: IpAddr) {
    if !crate::auth::basic_net_first(user, ip) {
        return;
    }
    let (engine, user) = (engine.clone(), user.to_string());
    let task = move || {
        let hash = hex_sha(&format!("basic:{}", crate::auth::net_key(ip)));
        let used_before = crate::auth::has_sessions(&user) || engine.db.signed_in_before(&user).unwrap_or(true);
        if remember(&engine, &user, &hash, "a program or script", used_before, false) {
            let _ = engine.db.insert_audit(&user, &ip.to_string(), "password used directly from a new network", "", 200);
            announce(&engine, &user, "a program or script (no web page)", ip, "the password sent directly by a program", TO_DO_DIRECT);
        }
    };
    if let Ok(rt) = tokio::runtime::Handle::try_current() {
        rt.spawn_blocking(task);
    } else {
        task();
    }
}

#[cfg(test)]
mod tests;
