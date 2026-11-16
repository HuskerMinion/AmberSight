//! Display mode sign-in: a wall TV, an old tablet or a spare monitor that has
//! no keyboard to type a password with.
//!
//! The display opens `/display` and, not signed in, asks for a display code
//! (`POST /api/display/start`). It shows the code on screen; the browser gets
//! a random secret for it in an HttpOnly cookie (never in an address). An
//! admin, signed in elsewhere, enters the code in Settings > Users & security
//! > Displays, sees where the request came from, and picks the viewer account
//! the display signs in as (`POST /api/display/approve`). The display, asking
//! every few seconds (`POST /api/display/wait`), then gets a display session
//! (see auth.rs, "display sessions"): never an admin's or an operator's,
//! held to watching, its expiry moved forward while it is used
//! (`POST /api/display/renew`), ended when unused for two weeks, and listed
//! with a Sign out button in Settings > Displays and in the account's own
//! device list.
//!
//! The code alone signs nobody in: only the browser holding the secret gets
//! the session, and only after an admin approved it. Codes last ten minutes,
//! and only a few can wait at once (per network and in all).

use crate::engine::Engine;
use axum::{
    body::Body,
    extract::{Path as AxPath, Request, State},
    http::{header, HeaderMap, StatusCode},
    response::{IntoResponse, Json, Response},
    Extension,
};
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::net::IpAddr;
use std::sync::{Arc, Mutex};

/// The cookie that holds a waiting display's secret (only sent to /api/display).
const WAIT_COOKIE: &str = "as_display_wait";
/// How long a display code can be used.
pub const CODE_SECS: i64 = 600;
/// Display codes waiting at once: in all, from addresses outside the home
/// network (so TVs at home always get one), and from one network (an IPv4
/// /24, an IPv6 /48: see auth::wide_net).
const MAX_WAITING: usize = 64;
const MAX_WAITING_AWAY: usize = 16;
const MAX_WAITING_PER_NET: usize = 4;
/// Letters and digits that cannot be mistaken for each other (no 0/O, 1/I).
const ALPHABET: &[u8] = b"ABCDEFGHJKLMNPQRSTUVWXYZ23456789";
const CODE_LEN: usize = 8;
/// The longest name a display gets in the list of devices.
const NAME_MAX: usize = 40;

struct Wait {
    code: String,
    expires: i64,
    ip: IpAddr,
    /// The browser, as devices::device_label names it.
    label: String,
    approved: Option<Approved>,
}

#[derive(Clone)]
struct Approved {
    user: String,
    /// The account's generation when it was approved: if it changes before
    /// the display picks the session up, no session is made.
    generation: u64,
    name: String,
    by: String,
}

/// Keyed by sha256 of the display's secret.
fn waits() -> &'static Mutex<HashMap<String, Wait>> {
    static W: std::sync::OnceLock<Mutex<HashMap<String, Wait>>> = std::sync::OnceLock::new();
    W.get_or_init(|| Mutex::new(HashMap::new()))
}

/// A home-network address: private IPv4 ranges, loopback, link-local, and
/// IPv6 unique-local. (Tailscale and other overlay addresses are not.)
pub fn is_lan(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(a) => a.is_private() || a.is_loopback() || a.is_link_local(),
        IpAddr::V6(a) => match a.to_ipv4_mapped() {
            Some(v4) => is_lan(IpAddr::V4(v4)),
            None => a.is_loopback() || (a.segments()[0] & 0xfe00) == 0xfc00 || (a.segments()[0] & 0xffc0) == 0xfe80,
        },
    }
}

fn now_secs() -> i64 {
    chrono::Utc::now().timestamp()
}

fn hash(secret: &str) -> String {
    Sha256::digest(format!("display-wait:{secret}").as_bytes()).iter().map(|x| format!("{x:02x}")).collect()
}

fn random_secret() -> String {
    use rand::RngCore;
    let mut b = [0u8; 32];
    rand::rngs::OsRng.fill_bytes(&mut b);
    b.iter().map(|x| format!("{x:02x}")).collect()
}

fn random_code() -> String {
    use rand::Rng;
    let mut r = rand::rngs::OsRng;
    (0..CODE_LEN).map(|_| ALPHABET[r.gen_range(0..ALPHABET.len())] as char).collect()
}

/// A code as typed ("abcd-efgh", "ABCD EFGH") the way it is kept.
pub fn normalize_code(s: &str) -> String {
    s.chars().filter(|c| c.is_ascii_alphanumeric()).map(|c| c.to_ascii_uppercase()).collect()
}

/// "ABCDEFGH" as shown: "ABCD-EFGH".
pub fn show_code(c: &str) -> String {
    if c.len() == CODE_LEN { format!("{}-{}", &c[..4], &c[4..]) } else { c.to_string() }
}

/// The name a display is listed under: printable, trimmed, at most NAME_MAX
/// characters; "Display" when nothing is left.
pub fn clean_name(s: &str) -> String {
    let n: String = s.chars().filter(|c| !c.is_control() && !matches!(*c, '\u{200b}'..='\u{200f}' | '\u{202a}'..='\u{202e}' | '\u{2066}'..='\u{2069}' | '\u{feff}')).collect();
    let n: String = n.split_whitespace().collect::<Vec<_>>().join(" ").chars().take(NAME_MAX).collect();
    if n.is_empty() { "Display".to_string() } else { n }
}

#[derive(Debug, PartialEq)]
pub enum StartError {
    /// Too many waiting from this network, or in all.
    Busy,
}

/// A new display code for a browser at `ip`. `old`: the secret it held for an
/// earlier code, which stops working. Returns (secret, code).
fn start_wait(ip: IpAddr, label: String, old: Option<&str>) -> Result<(String, String), StartError> {
    let now = now_secs();
    let mut w = waits().lock().unwrap();
    w.retain(|_, x| x.expires > now);
    if let Some(o) = old {
        w.remove(&hash(o));
    }
    let net = crate::auth::wide_net(ip);
    // A TV that asks again (it lost its secret, or reloaded) must not be held up by its own
    // earlier codes: when its network is full, its own oldest waiting code makes room.
    if w.values().filter(|x| crate::auth::wide_net(x.ip) == net).count() >= MAX_WAITING_PER_NET {
        if let Some(k) = w.iter().filter(|(_, x)| x.ip == ip).min_by_key(|(_, x)| x.expires).map(|(k, _)| k.clone()) {
            w.remove(&k);
        }
    }
    let away = w.values().filter(|x| !is_lan(x.ip)).count();
    if w.len() >= MAX_WAITING || (!is_lan(ip) && away >= MAX_WAITING_AWAY) || w.values().filter(|x| crate::auth::wide_net(x.ip) == net).count() >= MAX_WAITING_PER_NET {
        return Err(StartError::Busy);
    }
    let mut code = random_code();
    while w.values().any(|x| x.code == code) {
        code = random_code();
    }
    let secret = random_secret();
    w.insert(hash(&secret), Wait { code: code.clone(), expires: now + CODE_SECS, ip, label, approved: None });
    Ok((secret, code))
}

/// What an admin sees of a waiting display before approving it.
pub struct Found {
    pub ip: IpAddr,
    pub label: String,
    pub age: i64,
    pub approved: bool,
}

fn find_wait(code: &str) -> Option<Found> {
    let now = now_secs();
    let code = normalize_code(code);
    let w = waits().lock().unwrap();
    w.values().find(|x| x.code == code && x.expires > now).map(|x| Found { ip: x.ip, label: x.label.clone(), age: now - (x.expires - CODE_SECS), approved: x.approved.is_some() })
}

#[derive(Debug, PartialEq)]
enum ApproveError {
    NotFound,
    Already,
}

fn approve_wait(code: &str, a: Approved) -> Result<(), ApproveError> {
    let now = now_secs();
    let code = normalize_code(code);
    let mut w = waits().lock().unwrap();
    let x = w.values_mut().find(|x| x.code == code && x.expires > now).ok_or(ApproveError::NotFound)?;
    if x.approved.is_some() {
        return Err(ApproveError::Already);
    }
    x.approved = Some(a);
    Ok(())
}

enum WaitState {
    Gone,
    Pending { code: String, left: i64 },
    /// Approved: the wait is used up.
    Approved(Approved),
}

/// Whether a code asked for from `a` may be picked up from `b`: the same
/// network (auth::wide_net) when both use the same address family; a TV on a
/// dual-stack network may switch between IPv4 and IPv6, and then both must be
/// home-network addresses.
fn same_place(a: IpAddr, b: IpAddr) -> bool {
    let plain = |ip: IpAddr| match ip {
        IpAddr::V6(v) => v.to_ipv4_mapped().map(IpAddr::V4).unwrap_or(ip),
        v4 => v4,
    };
    let (a, b) = (plain(a), plain(b));
    if a.is_ipv4() == b.is_ipv4() {
        crate::auth::wide_net(a) == crate::auth::wide_net(b)
    } else {
        is_lan(a) && is_lan(b)
    }
}

/// The wait of `secret`, asked from `ip`: only from the network the code was
/// asked from (a secret taken elsewhere is of no use).
fn take_wait(secret: &str, ip: IpAddr) -> WaitState {
    let now = now_secs();
    let mut w = waits().lock().unwrap();
    let h = hash(secret);
    match w.get(&h) {
        Some(x) if !same_place(x.ip, ip) => WaitState::Gone,
        Some(x) if x.expires <= now => {
            w.remove(&h);
            WaitState::Gone
        }
        None => WaitState::Gone,
        Some(x) if x.approved.is_none() => WaitState::Pending { code: x.code.clone(), left: x.expires - now },
        Some(_) => WaitState::Approved(w.remove(&h).and_then(|x| x.approved).expect("approved above")),
    }
}

fn wait_secret(headers: &HeaderMap) -> Option<String> {
    headers
        .get_all(header::COOKIE)
        .iter()
        .filter_map(|v| v.to_str().ok())
        .flat_map(|h| h.split(';'))
        .filter_map(|c| c.trim().strip_prefix(WAIT_COOKIE).and_then(|r| r.strip_prefix('=')))
        .find(|v| v.len() == 64 && v.bytes().all(|b| b.is_ascii_hexdigit()))
        .map(str::to_string)
}

fn wait_cookie(secret: &str, max_age: i64, secure: bool) -> String {
    format!("{WAIT_COOKIE}={secret}; HttpOnly; SameSite=Strict; Path=/api/display; Max-Age={max_age}{}", if secure { "; Secure" } else { "" })
}

/// The account a display may sign in as: a viewer (not an admin or an
/// operator) that can sign in, and not the demo account. Err: why not.
fn display_account_ok(cfg: &crate::config::Config, user: &str) -> Result<(), &'static str> {
    let acc = crate::auth::accounts(cfg);
    let Some((_, _, admin)) = acc.iter().find(|(u, _, _)| u == user) else { return Err("no such account, or it is disabled") };
    if *admin {
        return Err("a display never signs in as an admin: pick or make a viewer account (Settings, Users)");
    }
    if crate::auth::is_operator(cfg, user) {
        return Err("a display signs in as a viewer account, not an operator: pick or make a viewer account (Settings, Users)");
    }
    if cfg.demo_account().as_deref() == Some(user) {
        return Err("this is the demo account: pick another viewer account");
    }
    Ok(())
}

fn json_with_cookies(v: serde_json::Value, cookies: &[String]) -> Response {
    let mut b = Response::builder().header(header::CONTENT_TYPE, "application/json").header(header::CACHE_CONTROL, "no-store");
    for c in cookies {
        b = b.header(header::SET_COOKIE, c);
    }
    b.body(Body::from(v.to_string())).unwrap()
}

/// `POST /api/display/start` (not signed in) → `{code, expires_in}` and the
/// wait cookie; `{open: true}` on a server without accounts.
pub async fn start(State(engine): State<Arc<Engine>>, req: Request) -> Response {
    let ip = crate::web::client_ip(&req);
    let secure = crate::web::arrived_secure(&req);
    if crate::auth::accounts(&engine.config.lock().unwrap()).is_empty() {
        return Json(serde_json::json!({ "open": true })).into_response();
    }
    if crate::auth::is_locked(ip) {
        return (StatusCode::TOO_MANY_REQUESTS, "Too many failed sign-ins came from this network, so sign-in is paused here for a few minutes. A new code comes by itself after that.").into_response();
    }
    let ua = req.headers().get(header::USER_AGENT).and_then(|v| v.to_str().ok()).unwrap_or("");
    let label = crate::devices::device_label(ua);
    let old = wait_secret(req.headers());
    match start_wait(ip, label, old.as_deref()) {
        Ok((secret, code)) => json_with_cookies(serde_json::json!({ "code": show_code(&code), "expires_in": CODE_SECS }), &[wait_cookie(&secret, CODE_SECS, secure)]),
        Err(StartError::Busy) => (StatusCode::TOO_MANY_REQUESTS, "Too many displays are waiting for their code to be approved. A new code comes by itself in a few minutes.").into_response(),
    }
}

/// `POST /api/display/wait` (not signed in, with the wait cookie) →
/// `{pending: true, code, expires_in}` until an admin approves the code, then
/// `{signed_in: true}` with the session cookie. 410 when the code has expired
/// (or was never there): ask for a new one.
pub async fn wait(State(engine): State<Arc<Engine>>, req: Request) -> Response {
    let ip = crate::web::client_ip(&req);
    let secure = crate::web::arrived_secure(&req);
    let Some(secret) = wait_secret(req.headers()) else {
        return (StatusCode::GONE, "no display code: ask for one").into_response();
    };
    let a = match take_wait(&secret, ip) {
        WaitState::Gone => return json_with_cookies(serde_json::json!({ "expired": true }), &[wait_cookie("", 0, secure)]).with_status(StatusCode::GONE),
        WaitState::Pending { code, left } => return Json(serde_json::json!({ "pending": true, "code": show_code(&code), "expires_in": left })).into_response(),
        WaitState::Approved(a) => a,
    };
    let refuse = |why: &str| {
        let _ = engine.db.insert_audit(&a.user, &ip.to_string(), "display sign-in refused", why, 409);
        json_with_cookies(serde_json::json!({ "expired": true, "why": why }), &[wait_cookie("", 0, secure)]).with_status(StatusCode::GONE)
    };
    if let Err(why) = display_account_ok(&engine.config.lock().unwrap(), &a.user) {
        return refuse(why);
    }
    let ctx = crate::devices::SignInCtx::new(&engine, req.headers(), ip);
    let id = crate::auth::Identity { user: a.user.clone(), admin: false };
    let Some(started) = crate::devices::complete_display_sign_in(&engine, &ctx, id, a.generation, &a.name, &a.by) else {
        return refuse("the account changed after the code was approved");
    };
    let _ = engine.db.insert_audit(&a.user, &ip.to_string(), "sign-in (display code)", &a.name, 200);
    json_with_cookies(
        serde_json::json!({ "signed_in": true, "user": a.user, "name": a.name }),
        &[crate::web::session_cookie(&started.token, crate::auth::DISPLAY_IDLE_SECS as u64, secure), crate::devices::set_cookie(&started.device_cookie, secure), wait_cookie("", 0, secure)],
    )
}

trait WithStatus {
    fn with_status(self, s: StatusCode) -> Response;
}
impl WithStatus for Response {
    fn with_status(mut self, s: StatusCode) -> Response {
        *self.status_mut() = s;
        self
    }
}

#[derive(serde::Deserialize)]
pub struct CodeReq {
    code: String,
}

/// `POST /api/display/lookup {code}` (admin) → where the waiting display is:
/// `{code, ip, browser, waiting_s, approved}`; 404 when no such code waits.
pub async fn lookup(Extension(admin): Extension<crate::web::IsAdmin>, Json(r): Json<CodeReq>) -> Response {
    if !admin.0 {
        return (StatusCode::FORBIDDEN, "admin access required").into_response();
    }
    match find_wait(&r.code) {
        Some(f) => Json(serde_json::json!({ "code": show_code(&normalize_code(&r.code)), "ip": f.ip.to_string(), "browser": f.label, "waiting_s": f.age, "approved": f.approved })).into_response(),
        None => (StatusCode::NOT_FOUND, "no display is waiting with that code: check it, or open the display page again for a new one").into_response(),
    }
}

#[derive(serde::Deserialize)]
pub struct ApproveReq {
    code: String,
    /// The viewer account the display signs in as.
    user: String,
    /// What the display is called in the list of devices ("Kitchen tablet").
    #[serde(default)]
    name: String,
    #[serde(flatten)]
    proof: crate::account_http::MyProof,
}

/// `POST /api/display/approve {code, user, name}` (admin) → `{ok, name}`. The
/// display signs in as `user`, a viewer account, at its next check. For an
/// account with two-step codes it takes the admin's own password (as the
/// phone QR code does): the display skips the account's code.
pub async fn approve(
    State(engine): State<Arc<Engine>>,
    caller: Option<Extension<crate::web::Caller>>,
    Extension(admin): Extension<crate::web::IsAdmin>,
    headers: HeaderMap,
    axum::extract::ConnectInfo(peer): axum::extract::ConnectInfo<std::net::SocketAddr>,
    Json(r): Json<ApproveReq>,
) -> Response {
    let who = caller.as_ref().map(|c| c.0 .0.clone()).unwrap_or_default();
    let generation = caller.as_ref().map(|c| c.0 .1).unwrap_or(0);
    if !admin.0 || !crate::users_http::caller_may_change_accounts(&engine.config.lock().unwrap(), &who) {
        return (StatusCode::FORBIDDEN, "admin access required").into_response();
    }
    if who == "(open)" {
        return (StatusCode::BAD_REQUEST, "this server has no accounts, so a display needs no code: open /display on it").into_response();
    }
    let user = r.user.trim().to_string();
    // (Read before looking: if the account changes after this, the display gets no session.)
    let target_generation = crate::auth::generation(&user);
    if let Err(why) = display_account_ok(&engine.config.lock().unwrap(), &user) {
        return (StatusCode::BAD_REQUEST, why).into_response();
    }
    if find_wait(&r.code).is_none_or(|f| f.approved) {
        return (StatusCode::NOT_FOUND, "no display is waiting with that code: check it, or open the display page again for a new one").into_response();
    }
    // A code for an account with two-step codes skips them: that takes the admin's own password.
    let two_step = crate::auth::two_step_on(&user) != Ok(false);
    if two_step {
        if let Err(resp) = crate::account_http::prove_admin(&engine, &who, peer.ip(), &headers, &r.proof, generation).await {
            return resp;
        }
    }
    if crate::auth::generation(&user) != target_generation || display_account_ok(&engine.config.lock().unwrap(), &user).is_err() {
        return (StatusCode::CONFLICT, "this account changed meanwhile: try again").into_response();
    }
    let name = clean_name(&r.name);
    match approve_wait(&r.code, Approved { user: user.clone(), generation: target_generation, name: name.clone(), by: who.clone() }) {
        Ok(()) => {
            let what = if two_step { "display paired (skips this account's two-step code)" } else { "display paired" };
            let _ = engine.db.insert_audit(&who, &peer.ip().to_string(), what, &format!("{name} as {user}"), 200);
            Json(serde_json::json!({ "ok": true, "name": name, "user": user })).into_response()
        }
        Err(ApproveError::Already) => (StatusCode::CONFLICT, "that code was approved already").into_response(),
        Err(ApproveError::NotFound) => (StatusCode::NOT_FOUND, "no display is waiting with that code: check it, or open the display page again for a new one").into_response(),
    }
}

/// `POST /api/display/renew` (signed in) → a display session ends
/// `DISPLAY_IDLE_SECS` from now (at most `DISPLAY_MAX_SECS` after pairing),
/// and its cookie is given again to last as long: `{display: true, expires,
/// pair_by}`. Only from the browser it was paired in (its device cookie).
/// Any other session (or an open server): `{display: false}`, nothing
/// changed; a display session from another browser: `{display: true,
/// renewed: false}`.
pub async fn renew(State(engine): State<Arc<Engine>>, secure: Option<Extension<crate::tls::Secure>>, headers: HeaderMap) -> Response {
    let acc = crate::auth::accounts(&engine.config.lock().unwrap());
    let Some((token, _)) = crate::auth::session_from_headers(&headers, &acc) else {
        return Json(serde_json::json!({ "display": false })).into_response();
    };
    if !crate::auth::session_is_display(token) {
        return Json(serde_json::json!({ "display": false })).into_response();
    }
    let device = crate::devices::browser_of(&headers);
    match crate::auth::renew_display_session(token, device.as_deref()) {
        Some((expires, pair_by)) => json_with_cookies(
            serde_json::json!({ "display": true, "renewed": true, "expires": expires, "pair_by": pair_by, "idle_days": crate::auth::DISPLAY_IDLE_SECS / 86400 }),
            &[crate::web::session_cookie(token, (expires - chrono::Utc::now().timestamp()).max(0) as u64, secure.is_some())],
        ),
        None => Json(serde_json::json!({ "display": true, "renewed": false })).into_response(),
    }
}

/// `GET /api/display/sessions` (admin) → every signed-in display:
/// `{displays: [{id, user, label, ip, signed_in, last_seen, expires}], idle_days}`.
pub async fn sessions(Extension(admin): Extension<crate::web::IsAdmin>) -> Response {
    if !admin.0 {
        return (StatusCode::FORBIDDEN, "admin access required").into_response();
    }
    let list: Vec<serde_json::Value> = crate::auth::display_sessions()
        .into_iter()
        .map(|(user, s, expires)| {
            serde_json::json!({
                "id": s.id,
                "user": user,
                "label": s.meta.label.strip_prefix(crate::auth::DISPLAY_LABEL).unwrap_or(&s.meta.label),
                "ip": s.meta.ip.map(|i| i.to_string()).unwrap_or_default(),
                "signed_in": (s.meta.created > 0).then_some(s.meta.created),
                "last_seen": (s.meta.last_seen > 0).then_some(s.meta.last_seen),
                "expires": expires,
                // However much it is used: paired again by then.
                "pair_by": s.meta.created + crate::auth::DISPLAY_MAX_SECS,
            })
        })
        .collect();
    Json(serde_json::json!({ "displays": list, "idle_days": crate::auth::DISPLAY_IDLE_SECS / 86400 })).into_response()
}

/// `POST /api/display/sessions/:id/sign-out` (admin) → `{ended}`; the
/// display's video stops and it shows a new display code.
pub async fn sign_out(State(engine): State<Arc<Engine>>, caller: Option<Extension<crate::web::Caller>>, Extension(admin): Extension<crate::web::IsAdmin>, axum::extract::ConnectInfo(peer): axum::extract::ConnectInfo<std::net::SocketAddr>, AxPath(id): AxPath<String>) -> Response {
    if !admin.0 {
        return (StatusCode::FORBIDDEN, "admin access required").into_response();
    }
    let who = caller.map(|c| c.0 .0.clone()).unwrap_or_default();
    match crate::auth::end_display_session(&id) {
        Ok(Some((user, gone))) => {
            let _ = engine.db.insert_audit(&who, &peer.ip().to_string(), "signed a display out", &format!("{} ({user})", gone.meta.label), 200);
            Json(serde_json::json!({ "ended": id })).into_response()
        }
        Ok(None) => (StatusCode::NOT_FOUND, "no such display: it may have signed out already").into_response(),
        Err(()) => (StatusCode::INTERNAL_SERVER_ERROR, "could not sign that display out, so it is still signed in. Try again, or see the server's log").into_response(),
    }
}

/// What a display session may use: exactly what the display page asks for
/// (static/js/display.js and the modules it loads), nothing else. Reading:
/// the camera list, the server status, the account's layouts and the shared
/// camera orders, a camera's latest picture (the poster before video) and its
/// live streams (info, WebSocket video, JPEG frames, MJPEG). Changes: keeping
/// its own session, and page timings. (whoami and logout are answered before
/// this check.) No recordings, playback, clips, events, search, bookmarks,
/// exports or anything about the account: a display stands where anyone
/// passing can touch it, so it shows only what is live.
pub fn display_allowed(path: &str, method: &axum::http::Method) -> bool {
    let read = method == axum::http::Method::GET || method == axum::http::Method::HEAD;
    let parts: Vec<&str> = path.trim_start_matches('/').split('/').collect();
    if read {
        matches!(parts.as_slice(), ["cameras"] | ["status"] | ["ui", "prefs"] | ["ui", "orders"] | ["snapshot", _] | ["live", _] | ["live", _, "info" | "ws" | "jpeg" | "stream"])
    } else {
        method == axum::http::Method::POST && matches!(path, "/display/renew" | "/ui/diag")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::Ipv4Addr;

    fn ip(n: u8) -> IpAddr {
        IpAddr::V4(Ipv4Addr::new(10, 0, n, 1))
    }

    #[test]
    fn codes_are_typed_loosely_and_shown_in_two_halves() {
        assert_eq!(normalize_code(" abcd-efgh "), "ABCDEFGH");
        assert_eq!(show_code("ABCDEFGH"), "ABCD-EFGH");
        let c = random_code();
        assert_eq!(c.len(), CODE_LEN);
        assert!(c.bytes().all(|b| ALPHABET.contains(&b)));
        assert_eq!(clean_name("  Kitchen \u{202e}tablet\n "), "Kitchen tablet");
        assert_eq!(clean_name(""), "Display");
        assert_eq!(clean_name(&"x".repeat(100)).len(), NAME_MAX);
    }

    #[test]
    fn a_code_signs_in_only_the_browser_that_asked_and_only_once_approved() {
        let (secret, code) = start_wait(ip(1), "Chrome on Android".into(), None).unwrap();
        assert!(matches!(take_wait(&secret, ip(1)), WaitState::Pending { .. }), "waiting");
        assert!(matches!(take_wait(&secret, ip(9)), WaitState::Gone), "the secret from another network");
        assert!(matches!(take_wait("f".repeat(64).as_str(), ip(1)), WaitState::Gone), "another browser's guess");
        let f = find_wait(&code.to_lowercase()).unwrap();
        assert_eq!((f.ip, f.label.as_str(), f.approved), (ip(1), "Chrome on Android", false));
        let a = Approved { user: "tv".into(), generation: 0, name: "TV".into(), by: "boss".into() };
        assert_eq!(approve_wait(&code, a.clone()), Ok(()));
        assert_eq!(approve_wait(&code, a), Err(ApproveError::Already), "once");
        assert!(matches!(take_wait(&secret, ip(1)), WaitState::Approved(x) if x.user == "tv"));
        assert!(matches!(take_wait(&secret, ip(1)), WaitState::Gone), "used up");
        assert!(find_wait(&code).is_none());
    }

    #[test]
    fn a_new_code_replaces_the_old_and_few_wait_per_network() {
        let (s1, c1) = start_wait(ip(2), "Safari on iPad".into(), None).unwrap();
        let (_s2, _c2) = start_wait(ip(2), "Safari on iPad".into(), Some(&s1)).unwrap();
        assert!(find_wait(&c1).is_none(), "the old code stopped working");
        assert!(matches!(take_wait(&s1, ip(2)), WaitState::Gone));
        // Others on the same network fill its slots; one more is refused.
        let mate = |n: u8| IpAddr::V4(Ipv4Addr::new(10, 0, 2, n));
        for n in 10..10 + MAX_WAITING_PER_NET as u8 - 1 {
            start_wait(mate(n), "x".into(), None).unwrap();
        }
        assert_eq!(start_wait(mate(99), "x".into(), None), Err(StartError::Busy));
        assert!(start_wait(ip(3), "x".into(), None).is_ok(), "another network is not held up");
    }

    #[test]
    fn a_tv_that_asks_again_replaces_its_own_waiting_code() {
        let tv = IpAddr::V4(Ipv4Addr::new(10, 0, 201, 20));
        let other = IpAddr::V4(Ipv4Addr::new(10, 0, 201, 21));
        let (_, oc) = start_wait(other, "Tablet".into(), None).unwrap();
        // Retries without its old secret never run out of slots, and never take the tablet's.
        for _ in 0..MAX_WAITING_PER_NET * 3 {
            start_wait(tv, "TV".into(), None).unwrap();
        }
        assert!(find_wait(&oc).is_some(), "the other display's code stays");
        let mine = waits().lock().unwrap().values().filter(|x| x.ip == tv).count();
        assert_eq!(mine, MAX_WAITING_PER_NET - 1);
        waits().lock().unwrap().retain(|_, x| crate::auth::wide_net(x.ip) != crate::auth::wide_net(tv));
    }

    #[test]
    fn addresses_away_from_home_get_only_part_of_the_codes() {
        let away = |n: u8| IpAddr::V4(Ipv4Addr::new(198, 18, n, 1));
        let mut kept = Vec::new();
        for n in 0..MAX_WAITING_AWAY as u8 {
            kept.push(start_wait(away(n), "x".into(), None).unwrap().0);
        }
        assert_eq!(start_wait(away(200), "x".into(), None), Err(StartError::Busy), "away addresses are capped");
        assert!(start_wait(ip(77), "x".into(), None).is_ok(), "a TV at home still gets a code");
        let mut w = waits().lock().unwrap();
        for k in &kept {
            w.remove(&hash(k));
        }
    }

    #[test]
    fn an_expired_code_is_gone() {
        let (secret, code) = start_wait(ip(4), "x".into(), None).unwrap();
        waits().lock().unwrap().get_mut(&hash(&secret)).unwrap().expires = now_secs() - 1;
        assert!(find_wait(&code).is_none());
        assert!(approve_wait(&code, Approved { user: "tv".into(), generation: 0, name: "TV".into(), by: "b".into() }).is_err());
        assert!(matches!(take_wait(&secret, ip(4)), WaitState::Gone));
    }

    #[test]
    fn only_viewer_accounts_can_be_displays() {
        let cfg: crate::config::Config = toml::from_str(
            "[server]\ndemo_user = \"demo\"\n[[server.users]]\nusername = \"boss\"\npassword = \"x\"\n[[server.users]]\nusername = \"tv\"\npassword = \"x\"\nadmin = false\n[[server.users]]\nusername = \"op\"\npassword = \"x\"\nadmin = false\nrole = \"operator\"\n[[server.users]]\nusername = \"off\"\npassword = \"x\"\nadmin = false\ndisabled = true\n[[server.users]]\nusername = \"demo\"\npassword = \"$argon2id$demo\"\nadmin = false\n",
        )
        .unwrap();
        assert!(display_account_ok(&cfg, "tv").is_ok());
        for u in ["boss", "op", "off", "demo", "nobody"] {
            assert!(display_account_ok(&cfg, u).is_err(), "{u}");
        }
    }

    #[test]
    fn a_display_may_only_watch_live() {
        use axum::http::Method;
        for (p, m) in [("/cameras", Method::GET), ("/status", Method::GET), ("/ui/prefs", Method::GET), ("/ui/orders", Method::GET), ("/snapshot/3", Method::GET), ("/live/1", Method::GET), ("/live/1/info", Method::GET), ("/live/1/ws", Method::GET), ("/live/1/jpeg", Method::GET), ("/live/1/stream", Method::HEAD), ("/display/renew", Method::POST), ("/ui/diag", Method::POST)] {
            assert!(display_allowed(p, &m), "{p} {m}");
        }
        for (p, m) in [
            ("/me/password", Method::POST), ("/me/tokens", Method::POST), ("/me/account", Method::GET), ("/ui/prefs", Method::PATCH), ("/ui/orders/all", Method::PUT),
            ("/cameras/1/wake", Method::POST), ("/cameras/1", Method::PUT), ("/cameras/1/tamper", Method::GET), ("/shield", Method::POST), ("/push/subscribe", Method::POST),
            ("/links", Method::GET), ("/export", Method::GET), ("/clips/record", Method::POST), ("/clips/abc", Method::GET), ("/events", Method::GET), ("/events/x/thumb", Method::GET),
            ("/recordings", Method::GET), ("/playback", Method::GET), ("/timeline/events", Method::GET), ("/timelapse", Method::GET), ("/smart/search", Method::GET), ("/plates", Method::GET),
            ("/bookmarks", Method::GET), ("/bookmarks", Method::POST), ("/stats", Method::GET), ("/health", Method::GET), ("/display/sessions", Method::GET), ("/live/1/ws", Method::POST), ("/snapshot", Method::GET),
        ] {
            assert!(!display_allowed(p, &m), "{p} {m}");
        }
    }

    #[test]
    fn a_code_is_picked_up_from_the_same_place() {
        let a = |s: &str| -> IpAddr { s.parse().unwrap() };
        assert!(same_place(a("192.168.1.5"), a("192.168.1.80")));
        assert!(same_place(a("192.168.1.5"), a("::ffff:192.168.1.9")));
        assert!(!same_place(a("192.168.1.5"), a("192.168.7.5")), "another IPv4 network");
        assert!(same_place(a("192.168.1.5"), a("fd12:3456::7")), "a dual-stack TV switching to IPv6 at home");
        assert!(same_place(a("fe80::1"), a("10.0.0.4")));
        assert!(!same_place(a("192.168.1.5"), a("2001:db8::7")), "IPv6 from outside");
        assert!(!same_place(a("8.8.8.8"), a("fd12::1")), "IPv4 from outside");
        assert!(same_place(a("2001:db8:1:5::1"), a("2001:db8:1:9::2")) && !same_place(a("2001:db8:1::1"), a("2001:db8:2::1")));
    }

    #[test]
    fn home_addresses_and_wide_networks() {
        let a = |s: &str| -> IpAddr { s.parse().unwrap() };
        for l in ["192.168.1.9", "10.1.2.3", "172.20.0.1", "127.0.0.1", "169.254.1.1", "fd00::1", "fe80::1", "::ffff:192.168.0.2"] {
            assert!(is_lan(a(l)), "{l}");
        }
        for o in ["100.64.0.9", "8.8.8.8", "2001:db8::1", "::ffff:8.8.8.8"] {
            assert!(!is_lan(a(o)), "{o}");
        }
        assert_eq!(crate::auth::wide_net(a("192.168.1.77")), a("192.168.1.0"));
        assert_eq!(crate::auth::wide_net(a("2001:db8:1:2:3::9")), a("2001:db8:1::"));
    }
}
