//! Your account over the API: the second sign-in step, two-step codes and
//! device tokens. See twostep.rs for the codes and auth.rs for the tokens.
//!
//! Everything here is out of a device token's reach (web.rs `token_may`).
//! Secrets are shown once, in answers that are not to be stored, and are never
//! logged: the audit log has what happened and to whom, never a code. Errors
//! say what to do in plain words; the details go to the log.
//!
//! Three kinds of limit, so that nobody can lock anyone else out:
//! - Six-digit codes at sign-in: per account and address, and per account.
//! - Six-digit codes from a session that exists: per session (the sign-in
//!   locks do not apply, the owner may be locked out there by someone else).
//! - Recovery codes: past all of those, under a small limit of their own. The
//!   slow checks wait their turn, first come first served, so nobody can keep
//!   the owner's from running.

use crate::engine::Engine;
use crate::twostep::{self, Verdict};
use crate::web::{Caller, IsAdmin};
use axum::{
    extract::{ConnectInfo, Path as AxPath, Request, State},
    http::{header, HeaderMap, StatusCode},
    response::{IntoResponse, Json, Response},
    Extension,
};
use std::net::{IpAddr, SocketAddr};
use std::sync::Arc;

/// How long a new secret waits for its first correct code.
const ENROL_SECS: i64 = 900;
const TOKEN_NAME_MAX: usize = 64;
/// A warning about the same thing goes out at most this often (someone moving
/// from address to address must not be able to flood the owner).
const NOTE_SECS: u64 = 3600;
/// How often the server looks for changes made from the command line.
const REQUEST_SECS: u64 = 5;

fn now() -> i64 {
    chrono::Utc::now().timestamp()
}

fn no_store(v: serde_json::Value) -> Response {
    ([(header::CACHE_CONTROL, "no-store")], Json(v)).into_response()
}

fn signed_in_only(who: &str) -> Option<Response> {
    (who == "(open)" || who.is_empty()).then(|| (StatusCode::BAD_REQUEST, "add an account first: sign-in is off on this server").into_response())
}

fn cannot_tell() -> Response {
    (StatusCode::SERVICE_UNAVAILABLE, "cannot check two-step codes right now; try again").into_response()
}

/// A change that could not be kept: the reason goes to the log, not to the client.
fn not_saved(what: &str, e: impl std::fmt::Display) -> Response {
    tracing::error!("{what}: {e}");
    (StatusCode::INTERNAL_SERVER_ERROR, format!("{what}; nothing was changed. Try again, or see the server's log")).into_response()
}

fn changed_meanwhile() -> Response {
    (StatusCode::CONFLICT, "this account changed meanwhile: try again").into_response()
}

/// The change is made, but sessions meant to end are still in the database and
/// would return at a restart. Never answered as plain success.
/// What a password change could not do, as the answer to the person.
pub(crate) fn password_cleanup_failed(f: crate::auth::PwFail) -> Response {
    match f {
        crate::auth::PwFail::Sessions => sessions_still_stored(),
        crate::auth::PwFail::Browsers => (StatusCode::INTERNAL_SERVER_ERROR, "the password is changed and other sessions ended, but the list of browsers this account has signed in from could not be cleared, so a browser someone used before may not be told next time. See the server's log").into_response(),
    }
}

/// An admin account's notification addresses that are about to go with the sign-ins
/// they were turned on from (read before the change, so that the devices can be told
/// from this list after it, however slow their push service is): a stolen session must
/// not silence a phone unseen.
pub(crate) fn rows_losing_alerts(engine: &Arc<Engine>, user: &str, devices: &[String]) -> Vec<crate::db::PushSub> {
    if devices.is_empty() || !crate::push_http::account_is_admin(engine, user) {
        return Vec::new();
    }
    engine.db.push_by_session_devices(user, devices).unwrap_or_default()
}

/// Tell these devices on a task of their own: nothing waits for a push service, and a
/// client that leaves does not cut the change short. Only after the change was made.
pub(crate) fn tell_devices_detached(engine: &Arc<Engine>, rows: Vec<crate::db::PushSub>, title: String, body: String) {
    if rows.is_empty() {
        return;
    }
    let e = engine.clone();
    tokio::spawn(async move { tell_devices_first(&e, rows, title, body).await });
}

pub(crate) fn sessions_still_stored() -> Response {
    (StatusCode::INTERNAL_SERVER_ERROR, "the change is made, but other sessions of the account could not be removed from the database and could return at a restart. See the server's log, and sign out of them").into_response()
}

pub const TOO_MANY_CODES: &str = "too many wrong codes, so six-digit codes are refused for a while, and a recovery code works instead. If your codes keep failing, the clock on your phone or on this server may be off";
const ONLY_RECOVERY: &str = "too many wrong codes were tried for this account, so only a recovery code works right now";
const TOO_MANY_RECOVERY: &str = "too many tries at a recovery code; try again in 10 minutes";
const TOO_MANY_HERE: &str = "too many tries at a code from this session; try again later";
const RECOVERY_BUSY: &str = "many recovery codes are being checked right now, and this one was not: it did not count. Try again in a moment";

/// A message for the admins, through the services the system alerts use (the
/// browser and phone notifications among them go to admins only).
pub(crate) fn tell_admins(engine: &Arc<Engine>, title: String, body: String) {
    send_note(engine, title, body, None, false);
}

/// Messages being sent at one time, each on its own thread (a service that does
/// not answer holds one for seconds). Past this a flood costs threads no more: what
/// did not go out is named in one message when there is room.
const MAX_ALERT_THREADS: usize = 32;
/// Names in one gathered message: the rest are counted, and are in the audit log.
const MAX_GATHERED: usize = 200;
static ALERT_THREADS: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

/// Titles of messages that found no room, and how many past `MAX_GATHERED`.
fn alert_overflow() -> &'static std::sync::Mutex<(Vec<String>, usize)> {
    static OVERFLOW: std::sync::OnceLock<std::sync::Mutex<(Vec<String>, usize)>> = std::sync::OnceLock::new();
    OVERFLOW.get_or_init(Default::default)
}

struct AlertSeat;

impl Drop for AlertSeat {
    fn drop(&mut self) {
        ALERT_THREADS.fetch_sub(1, std::sync::atomic::Ordering::SeqCst);
    }
}

/// A place among the messages being sent. `always`: also past the limit.
fn alert_seat(always: bool) -> Option<AlertSeat> {
    use std::sync::atomic::Ordering;
    let n = ALERT_THREADS.fetch_add(1, Ordering::SeqCst);
    if n >= MAX_ALERT_THREADS && !always {
        ALERT_THREADS.fetch_sub(1, Ordering::SeqCst);
        return None;
    }
    Some(AlertSeat)
}

/// What was sent, for tests.
#[cfg(test)]
pub(crate) fn sent_log() -> &'static std::sync::Mutex<Vec<(String, String)>> {
    static LOG: std::sync::OnceLock<std::sync::Mutex<Vec<(String, String)>>> = std::sync::OnceLock::new();
    LOG.get_or_init(Default::default)
}

fn send_note(engine: &Arc<Engine>, title: String, body: String, wait_key: Option<String>, always: bool) {
    #[cfg(test)]
    sent_log().lock().unwrap().push((title.clone(), body.clone()));
    let Some(seat) = alert_seat(always) else {
        tracing::warn!("too many alerts at once: \"{title}\" did not go out by itself");
        let (items, more) = &mut *alert_overflow().lock().unwrap();
        add_gathered(items, more, title);
        if let Some(k) = wait_key {
            crate::auth::note_forget(&k);
        }
        return;
    };
    let alerts = engine.config.lock().unwrap().alerts.clone();
    let site = crate::links::own_name(engine);
    let left = title.clone();
    let sender = std::thread::Builder::new().name("account-alert".into()).spawn(move || {
        let _seat = seat;
        let sent = crate::engine::system_alerts::send(&alerts, &format!("{site}: {title}"), &body, true);
        // Nothing got through: the next warning need not wait.
        if let Some(k) = wait_key {
            if !sent.is_empty() && sent.iter().all(|s| !s.1) {
                crate::auth::note_forget(&k);
            }
        }
    });
    if let Err(e) = sender {
        tracing::warn!("could not start sending an alert ({e}): \"{left}\" did not go out by itself");
        let (items, more) = &mut *alert_overflow().lock().unwrap();
        add_gathered(items, more, left);
    }
}

/// The same, at most once in a while for `key`.
pub(crate) fn tell_admins_once(engine: &Arc<Engine>, key: &str, every_secs: u64, title: String, body: String) {
    if crate::auth::note_due(key, every_secs) {
        send_note(engine, title, body, Some(key.to_string()), false);
    }
}

fn add_gathered(items: &mut Vec<String>, more: &mut usize, item: String) {
    if items.len() < MAX_GATHERED {
        items.push(item);
    } else {
        *more += 1;
    }
}

/// Several in a row, in words, for a gathered message.
fn listed(items: &[String], more: usize) -> String {
    let names = items.join(", ");
    if more > 0 {
        format!("{names} and {more} more (see the audit log)")
    } else {
        names
    }
}

/// Something that must never be hidden and may come in bursts.
struct Gathering {
    until: std::time::Instant,
    items: Vec<String>,
    more: usize,
    /// The title and text for what came after the first: their items, and how many there were.
    later: Box<dyn Fn(&str, usize) -> (String, String) + Send>,
}

fn gatherings() -> &'static std::sync::Mutex<std::collections::HashMap<String, Gathering>> {
    static GATHERING: std::sync::OnceLock<std::sync::Mutex<std::collections::HashMap<String, Gathering>>> = std::sync::OnceLock::new();
    GATHERING.get_or_init(Default::default)
}

/// Admins hear of each of these. The first at once; any more about `key` within
/// `window` together in one message at its end. So a burst neither floods them nor
/// hides anything, also after someone has reacted to the first. `key` must not
/// hold what a caller made up: every key that exists at one time holds a place here.
pub(crate) fn tell_gathered(engine: &Arc<Engine>, key: String, item: String, window: std::time::Duration, first: (String, String), later: impl Fn(&str, usize) -> (String, String) + Send + 'static) {
    {
        let mut m = gatherings().lock().unwrap();
        if let Some(g) = m.get_mut(&key) {
            add_gathered(&mut g.items, &mut g.more, item);
            return;
        }
        m.insert(key, Gathering { until: std::time::Instant::now() + window, items: Vec::new(), more: 0, later: Box::new(later) });
    }
    tell_admins(engine, first.0, first.1);
}

/// Send what was gathered: those whose time is up, or (the server is stopping) all.
fn flush_gathered(engine: &Arc<Engine>, all: bool) {
    let now = std::time::Instant::now();
    let due: Vec<Gathering> = {
        let mut m = gatherings().lock().unwrap();
        let keys: Vec<String> = m.iter().filter(|(_, g)| all || g.until <= now).map(|(k, _)| k.clone()).collect();
        keys.iter().filter_map(|k| m.remove(k)).collect()
    };
    for g in due {
        if !g.items.is_empty() {
            let (title, body) = (g.later)(&listed(&g.items, g.more), g.items.len() + g.more);
            send_note(engine, title, body, None, all);
        }
    }
    // (Taken out first: sending it may need the same list.)
    let (items, more) = std::mem::take(&mut *alert_overflow().lock().unwrap());
    if !items.is_empty() {
        send_note(engine, "alerts that could not go out at once".to_string(), format!("There were too many alerts at one time. These did not go out by themselves: {}.", listed(&items, more)), None, all);
    }
}

/// Send what was gathered under one key now (tests run side by side, so they
/// must not flush each other's).
#[cfg(test)]
pub(crate) fn flush_gathering(engine: &Arc<Engine>, key: &str) {
    let g = gatherings().lock().unwrap().remove(key);
    if let Some(g) = g.filter(|g| !g.items.is_empty()) {
        let (title, body) = (g.later)(&listed(&g.items, g.more), g.items.len() + g.more);
        send_note(engine, title, body, None, false);
    }
}

/// The server is stopping: nothing waiting to go out may be lost (a restart must
/// not hide what it was about). Waits for at most `max`.
pub fn finish_alerts(engine: &Arc<Engine>, max: std::time::Duration) {
    flush_gathered(engine, true);
    let start = std::time::Instant::now();
    while ALERT_THREADS.load(std::sync::atomic::Ordering::SeqCst) > 0 && start.elapsed() < max {
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
}

/// How long new viewers by one admin are gathered into one message.
const NEW_ACCOUNT_WINDOW_SECS: u64 = 60;

/// Admins hear of each new viewer by name.
pub(crate) fn tell_new_account(engine: &Arc<Engine>, who: &str, name: &str) {
    let w = who.to_string();
    tell_gathered(
        engine,
        format!("new-accounts:{who}"),
        name.to_string(),
        std::time::Duration::from_secs(NEW_ACCOUNT_WINDOW_SECS),
        (format!("new account {name}"), format!("{who} created the account {name}.")),
        move |names, _| (format!("new accounts {names}"), format!("{w} also created these accounts: {names}.")),
    );
}

/// How long news of an admin's devices is gathered into one message.
const DEVICE_WINDOW_SECS: u64 = 60;

/// An admin's account has a new device for notifications, or lost one: that is
/// where alerts go, so someone with a stolen session must not do it unseen.
pub(crate) fn tell_device_change(engine: &Arc<Engine>, user: &str, device: &str, ip: IpAddr, added: bool) {
    let (u, what) = (user.to_string(), if added { "new device" } else { "device removed" });
    let word = if added { "turned on notifications on a new device" } else { "removed a device from notifications" };
    tell_gathered(
        engine,
        format!("devices:{user}:{what}"),
        device.to_string(),
        std::time::Duration::from_secs(DEVICE_WINDOW_SECS),
        (format!("{what} for {user}: {device}"), format!("{user} {word}: {device} (from {ip}). If that was not meant to happen, change that password and check the devices.")),
        move |devices, _| (format!("{what} for {u}: {devices}"), format!("{u} also {word}: {devices}.")),
    );
}

/// Tell these devices first, before what is being done takes them off the list
/// or makes them stop getting alerts. Not waiting long for a service that does
/// not answer: the sending carries on by itself.
pub(crate) async fn tell_devices_first(engine: &Arc<Engine>, subs: Vec<crate::db::PushSub>, title: String, body: String) {
    if subs.is_empty() {
        return;
    }
    let site = crate::links::own_name(engine);
    let e = engine.clone();
    let told = tokio::task::spawn_blocking(move || {
        let m = crate::push::Message { title: format!("{site}: {title}"), body, url: "/#settings/system/status".to_string(), tag: "system".to_string(), urgent: true, kind: crate::push::MsgType::Problem, ..Default::default() };
        crate::push::send_to(&e, &subs, &m);
    });
    let _ = tokio::time::timeout(std::time::Duration::from_secs(5), told).await;
}

/// May a recovery code be tried from here now? (Burst: admins hear of it.)
fn recovery_allowed(engine: &Arc<Engine>, user: &str, ip: IpAddr) -> bool {
    let ok = crate::auth::recovery_try(user, ip);
    if !ok || crate::auth::recovery_burst(user) {
        tell_admins_once(engine, &format!("recovery-burst:{user}"), NOTE_SECS, format!("many recovery codes tried for {user}"), format!("Someone keeps trying recovery codes for {user}, last from {ip}. Only someone with the password gets that far: change it now (menu at the top, Change password, or an admin in Users)."));
    }
    ok
}

/// Call before the password is replaced and any lock cleared: admins hear of a
/// password change on an admin account, or one with two-step codes or codes locked.
pub(crate) fn tell_of_password_change(engine: &Arc<Engine>, user: &str, how: &str) {
    if crate::auth::two_step_on(user) != Ok(false) || crate::auth::code_locked_any(user) {
        tell_admins(engine, format!("password changed for {user}"), format!("The password of {user}, an account with two-step codes, was changed ({how}). If that was not meant to happen, act now: disable the account in Users."));
    } else if crate::auth::accounts(&engine.config.lock().unwrap()).iter().any(|(u, _, a)| u == user && *a) {
        tell_admins(engine, format!("password changed for {user}"), format!("The password of {user}, an admin account, was changed ({how}). If that was not meant to happen, act now: disable the account in Users."));
    }
}

/// Who is proving something: the caller, and the session they came with.
#[derive(Clone)]
pub struct Actor {
    pub user: String,
    pub ip: IpAddr,
    pub session: Option<String>,
}

pub fn actor(engine: &Engine, user: &str, ip: IpAddr, headers: &HeaderMap) -> Actor {
    Actor { user: user.to_string(), ip, session: this_session(engine, headers) }
}

/// A wrong six-digit code at sign-in: counted for this address and the account.
/// Whoever gets here has the password. True if codes are locked now.
fn wrong_code_at_sign_in(engine: &Arc<Engine>, user: &str, ip: IpAddr) -> bool {
    let lock = crate::auth::code_wrong(user, ip);
    let what = match lock {
        Some(crate::auth::CodeLock::Address) => "two-step code wrong, six-digit codes from this address locked for 10 minutes",
        Some(crate::auth::CodeLock::Account) => "two-step code wrong, six-digit codes for this account locked for an hour",
        None => "two-step code wrong",
    };
    let _ = engine.db.insert_audit(user, &ip.to_string(), what, "", 401);
    match lock {
        Some(crate::auth::CodeLock::Address) => tell_admins_once(
            engine,
            &format!("code-lock-address:{user}"),
            NOTE_SECS,
            format!("wrong two-step codes for {user}"),
            format!("Someone who has the password for {user} entered too many wrong two-step codes, last from {ip}. They do not have a code, but they do have the password: change it now (menu at the top, Change password, or an admin in Users). Six-digit codes from that address wait 10 minutes; other addresses and recovery codes still work."),
        ),
        Some(crate::auth::CodeLock::Account) => tell_admins_once(
            engine,
            &format!("code-lock-account:{user}"),
            NOTE_SECS,
            format!("many wrong two-step codes for {user}"),
            format!("Someone who has the password for {user} entered too many wrong two-step codes from several addresses, last from {ip}. They do not have a code, but they do have the password: change it now (menu at the top, Change password, or an admin in Users). Six-digit codes at sign-in wait an hour; signed-in devices and recovery codes still work."),
        ),
        None => {}
    }
    lock.is_some()
}

/// Why a proof did not pass.
enum ProofFail {
    /// A wrong password or code: counts against this session.
    Wrong(Response),
    /// Nothing was tried (a missing code, a limit reached, a fault).
    NoTry(Response),
}

/// Why a check did not give an answer: nothing was tried either way.
enum NoCheck {
    /// A recovery code that did not get its turn.
    Busy,
    Fault,
}

/// What to say about it.
fn no_check(why: NoCheck) -> Response {
    match why {
        NoCheck::Busy => (StatusCode::SERVICE_UNAVAILABLE, RECOVERY_BUSY).into_response(),
        NoCheck::Fault => cannot_tell(),
    }
}

/// The check itself, off the async workers: a recovery code is slow.
async fn run_verify(engine: &Arc<Engine>, a: &Actor, code: Option<&str>, recovery: Option<&str>) -> Result<Verdict, NoCheck> {
    // A recovery code can run Argon2 ten times: only a few at once, whoever asks,
    // the others in line. Its turn goes along with the check, so a client that
    // leaves does not start more of them than the limit.
    let six_digits = code.is_some_and(|c| !c.trim().is_empty());
    let turn = if !six_digits && twostep::recovery_is_checked(recovery.unwrap_or("")) {
        match crate::auth::recovery_turn(&a.user, a.ip).await {
            Some(t) => Some(t),
            None => return Err(NoCheck::Busy),
        }
    } else {
        None
    };
    let (db, u, c, r) = (engine.db.clone(), a.user.clone(), code.map(str::to_string), recovery.map(str::to_string));
    let verdict = tokio::task::spawn_blocking(move || {
        let _turn = turn;
        twostep::verify(&db, &u, c.as_deref(), r.as_deref(), now())
    })
    .await;
    match verdict {
        Ok(Ok(v)) => {
            if let Verdict::Right { recovery_left: Some(n) } = v {
                let _ = engine.db.insert_audit(&a.user, &a.ip.to_string(), &format!("recovery code used, {n} left"), "", 200);
            }
            Ok(v)
        }
        Ok(Err(e)) => {
            tracing::error!("two-step check failed: {e:#}");
            Err(NoCheck::Fault)
        }
        Err(_) => Err(NoCheck::Fault),
    }
}

/// A code or recovery code from a session that already exists. The sign-in
/// locks do not apply: the owner may be locked out there by someone else.
async fn code_from_session(engine: &Arc<Engine>, a: &Actor, code: Option<&str>, recovery: Option<&str>) -> Result<Option<i64>, ProofFail> {
    match crate::auth::two_step_on(&a.user) {
        Ok(false) => return Ok(None),
        Ok(true) => {}
        Err(()) => return Err(ProofFail::NoTry(cannot_tell())),
    }
    let given = |x: Option<&str>| x.is_some_and(|c| !c.trim().is_empty());
    if !given(code) && !given(recovery) {
        return Err(ProofFail::NoTry((StatusCode::FORBIDDEN, "enter your two-step code").into_response()));
    }
    // (An account with codes on has no other way here than a session.)
    let Some(session) = a.session.as_deref() else {
        return Err(ProofFail::NoTry((StatusCode::FORBIDDEN, crate::auth::USE_A_TOKEN).into_response()));
    };
    let six_digits = given(code);
    if six_digits && !crate::auth::session_code_try(session) {
        return Err(ProofFail::NoTry((StatusCode::TOO_MANY_REQUESTS, TOO_MANY_HERE).into_response()));
    }
    if !six_digits && !recovery_allowed(engine, &a.user, a.ip) {
        return Err(ProofFail::NoTry((StatusCode::TOO_MANY_REQUESTS, TOO_MANY_RECOVERY).into_response()));
    }
    let refund = || {
        if six_digits {
            crate::auth::session_code_ok(session);
        }
    };
    match run_verify(engine, a, code, recovery).await {
        Ok(Verdict::Right { recovery_left }) => {
            refund();
            Ok(recovery_left)
        }
        Ok(Verdict::Wrong) => {
            let _ = engine.db.insert_audit(&a.user, &a.ip.to_string(), "two-step code wrong (from a session)", "", 403);
            Err(ProofFail::Wrong((StatusCode::FORBIDDEN, "that two-step code is wrong").into_response()))
        }
        Ok(Verdict::NotOn) => {
            refund();
            Ok(None)
        }
        // (No check ran: the try does not count.)
        Err(why) => {
            refund();
            if !six_digits {
                crate::auth::recovery_refund(&a.user, a.ip);
            }
            Err(ProofFail::NoTry(no_check(why)))
        }
    }
}

async fn proof_inner(engine: &Arc<Engine>, a: &Actor, password: &str, code: Option<&str>, recovery: Option<&str>, need_code: bool) -> Result<Option<i64>, ProofFail> {
    let acc = crate::auth::accounts(&engine.config.lock().unwrap());
    let (u, p) = (a.user.clone(), password.to_string());
    let ok = match crate::auth::password_check(a.ip, move || crate::auth::check_login(&acc, &u, &p).is_some()).await {
        Ok(ok) => ok,
        Err(crate::auth::NotChecked::Busy) => return Err(ProofFail::NoTry((StatusCode::SERVICE_UNAVAILABLE, crate::auth::PASSWORD_BUSY).into_response())),
        Err(crate::auth::NotChecked::Locked) => return Err(ProofFail::NoTry((StatusCode::TOO_MANY_REQUESTS, "too many wrong tries; try again in a few minutes").into_response())),
    };
    if !ok {
        let _ = engine.db.insert_audit(&a.user, &a.ip.to_string(), "account change refused: wrong password", "", 403);
        return Err(ProofFail::Wrong((StatusCode::FORBIDDEN, "your password is wrong").into_response()));
    }
    if need_code {
        return code_from_session(engine, a, code, recovery).await;
    }
    Ok(None)
}

/// The caller's password (and, with two-step on, code), for a change to their
/// account or a sensitive admin change. Err: the answer to send. Wrong tries
/// count against the session: after a few, it is ended and the admins hear of
/// it. So a stolen session cannot be used for guessing. Without a session
/// (HTTP Basic on an account with no codes) it counts against the address.
pub async fn prove(engine: &Arc<Engine>, a: &Actor, password: &str, code: Option<&str>, recovery: Option<&str>, need_code: bool) -> Result<Option<i64>, Response> {
    // On its own task: a client that leaves does not cut the counting short.
    let (engine, a, password, code, recovery) = (engine.clone(), a.clone(), password.to_string(), code.map(str::to_string), recovery.map(str::to_string));
    match tokio::spawn(async move { prove_to_the_end(&engine, &a, &password, code.as_deref(), recovery.as_deref(), need_code).await }).await {
        Ok(r) => r,
        Err(e) => {
            tracing::error!("checking a password or code failed: {e}");
            Err(cannot_tell())
        }
    }
}

async fn prove_to_the_end(engine: &Arc<Engine>, a: &Actor, password: &str, code: Option<&str>, recovery: Option<&str>, need_code: bool) -> Result<Option<i64>, Response> {
    let Some(session) = a.session.as_deref() else {
        if crate::auth::is_locked(a.ip) {
            return Err((StatusCode::TOO_MANY_REQUESTS, "too many wrong tries; try again in a few minutes").into_response());
        }
        return match proof_inner(engine, a, password, code, recovery, need_code).await {
            Ok(v) => Ok(v),
            Err(ProofFail::Wrong(r)) => {
                crate::auth::record_failure(a.ip);
                Err(r)
            }
            Err(ProofFail::NoTry(r)) => Err(r),
        };
    };
    if !crate::auth::proof_begin(&a.user, session) {
        return Err(end_this_session(engine, a, session));
    }
    match proof_inner(engine, a, password, code, recovery, need_code).await {
        Ok(v) => {
            crate::auth::proof_refund(&a.user, session);
            Ok(v)
        }
        Err(ProofFail::NoTry(r)) => {
            crate::auth::proof_refund(&a.user, session);
            Err(r)
        }
        Err(ProofFail::Wrong(r)) => {
            if crate::auth::proof_used_up(&a.user, session) {
                return Err(end_this_session(engine, a, session));
            }
            Err(r)
        }
    }
}

fn end_this_session(engine: &Arc<Engine>, a: &Actor, session: &str) -> Response {
    crate::auth::end_session(session);
    let _ = engine.db.insert_audit(&a.user, &a.ip.to_string(), "session ended: too many wrong passwords or codes", "", 403);
    tell_admins_once(
        engine,
        &format!("proof-session:{}", a.user),
        NOTE_SECS,
        format!("a session of {} was signed out", a.user),
        format!("A signed-in session of {} kept giving a wrong password or two-step code (last from {}), so it was signed out. If that was not them, someone had that session: change the password and check what they did in the audit log.", a.user, a.ip),
    );
    (StatusCode::FORBIDDEN, "too many wrong tries: this session has been signed out").into_response()
}

/// What an admin sends along with the few changes that hand out admin rights
/// or someone else's way in: their own password, and code if they use them.
/// A stolen session alone cannot do these.
#[derive(serde::Deserialize, Default)]
pub struct MyProof {
    #[serde(default)]
    pub my_password: Option<String>,
    #[serde(default)]
    pub my_code: Option<String>,
    #[serde(default)]
    pub my_recovery_code: Option<String>,
}

/// Check `who`'s own password and code for such a change: the same limits and
/// audit as their own account changes. Nothing sent is asked for, not counted.
/// The caller then checks the account again under the config lock.
pub async fn prove_admin(engine: &Arc<Engine>, who: &str, ip: IpAddr, headers: &HeaderMap, proof: &MyProof, generation: u64) -> Result<(), Response> {
    let password = proof.my_password.as_deref().unwrap_or("");
    if password.is_empty() {
        return Err((StatusCode::FORBIDDEN, "this change needs your own password: send my_password, and my_code if you use two-step codes").into_response());
    }
    let a = actor(engine, who, ip, headers);
    prove(engine, &a, password, proof.my_code.as_deref(), proof.my_recovery_code.as_deref(), true).await?;
    // (The check is slow: this account may have changed since it began.)
    if crate::auth::generation(who) != generation {
        return Err(changed_meanwhile());
    }
    Ok(())
}

/// This request's session, kept when the account's others end.
fn this_session(engine: &Engine, headers: &HeaderMap) -> Option<String> {
    let acc = crate::auth::accounts(&engine.config.lock().unwrap());
    crate::auth::session_from_headers(headers, &acc).map(|(t, _)| t.to_string())
}

#[derive(serde::Deserialize)]
struct CodeReq {
    #[serde(default)]
    ticket: String,
    #[serde(default)]
    code: Option<String>,
    #[serde(default)]
    recovery_code: Option<String>,
}

/// `POST /api/login/totp {"ticket", "code" | "recovery_code"}`: the second
/// sign-in step. Answers like /api/login; `recovery_left` when a recovery code
/// was used.
pub async fn login_totp(State(engine): State<Arc<Engine>>, req: Request) -> Response {
    let ip = crate::web::client_ip(&req);
    let secure = crate::web::arrived_secure(&req);
    if crate::auth::is_locked(ip) {
        return (StatusCode::TOO_MANY_REQUESTS, "too many failed sign-ins; try again in a few minutes").into_response();
    }
    let ctx = crate::devices::SignInCtx::new(&engine, req.headers(), ip);
    let Ok(bytes) = axum::body::to_bytes(req.into_body(), 16 * 1024).await else {
        return (StatusCode::BAD_REQUEST, "bad request").into_response();
    };
    let Ok(body) = serde_json::from_slice::<CodeReq>(&bytes) else {
        return (StatusCode::BAD_REQUEST, "expected {\"ticket\", \"code\"}").into_response();
    };
    // Nothing typed is no try: it counts against nobody and leaves the ticket alone.
    let typed = |x: &Option<String>| x.as_deref().is_some_and(|c| !c.trim().is_empty());
    if !typed(&body.code) && !typed(&body.recovery_code) {
        return (StatusCode::BAD_REQUEST, "enter the code").into_response();
    }
    let hold = match crate::auth::take_ticket(body.ticket.trim()) {
        crate::auth::TicketTry::Held(h) => h,
        crate::auth::TicketTry::Busy => return (StatusCode::CONFLICT, "this code is still being checked; wait a moment").into_response(),
        // One this server gave out (just used, a form sent twice; run out) is nobody guessing.
        crate::auth::TicketTry::Late => return ticket_expired(),
        crate::auth::TicketTry::MadeUp => {
            crate::auth::record_failure(ip);
            let _ = engine.db.insert_audit("?", &ip.to_string(), "sign-in failed: no such ticket", "", 401);
            return ticket_expired();
        }
    };
    // On its own task: a client that leaves does not cut the check short, so the
    // ticket stays held and the try counted for as long as the check really runs.
    match tokio::spawn(try_ticket(engine, hold, body, ip, secure, ctx)).await {
        Ok(r) => r,
        Err(e) => {
            tracing::error!("a sign-in code check failed: {e}");
            cannot_tell()
        }
    }
}

fn ticket_expired() -> Response {
    (StatusCode::UNAUTHORIZED, "this sign-in has expired; enter your password again").into_response()
}

/// One try with a held ticket. (Dropped, the ticket may be tried again; used up by `burn`.)
async fn try_ticket(engine: Arc<Engine>, hold: crate::auth::TicketHold, body: CodeReq, ip: IpAddr, secure: bool, ctx: crate::devices::SignInCtx) -> Response {
    let six_digits = body.code.as_deref().is_some_and(|c| !c.trim().is_empty());
    let acc = crate::auth::accounts(&engine.config.lock().unwrap());
    let Some(admin) = acc.iter().find(|(u, _, _)| *u == hold.user).map(|(_, _, a)| *a) else {
        return ticket_expired();
    };
    let user = hold.user.clone();
    let generation = hold.generation;
    // The limits: six-digit codes have the sign-in locks; a recovery code is
    // past them, under a small limit of its own. (The ticket stays good for a
    // recovery code whatever happens to six-digit codes.)
    if six_digits && hold.recovery_only {
        return (StatusCode::TOO_MANY_REQUESTS, ONLY_RECOVERY).into_response();
    }
    if six_digits && !crate::auth::code_try(&user, ip) {
        return (StatusCode::TOO_MANY_REQUESTS, TOO_MANY_CODES).into_response();
    }
    if !six_digits && !recovery_allowed(&engine, &user, ip) {
        return (StatusCode::TOO_MANY_REQUESTS, TOO_MANY_RECOVERY).into_response();
    }
    let a = Actor { user: user.clone(), ip, session: None };
    let refund = || {
        if six_digits {
            crate::auth::code_ok(&user, ip);
        }
    };
    match run_verify(&engine, &a, body.code.as_deref(), body.recovery_code.as_deref()).await {
        Ok(Verdict::Right { recovery_left: left }) => {
            refund();
            hold.burn();
            let id = crate::auth::Identity { user: user.clone(), admin };
            // (The recovery-code warning is sent from there, joined with the new-device one.)
            let how = match left {
                Some(n) => crate::devices::How::RecoveryCode(n as usize),
                None => crate::devices::How::TwoStepCode,
            };
            let Some(started) = crate::devices::complete_sign_in(&engine, &ctx, id, generation, how) else { return ticket_expired() };
            crate::auth::record_success(ip);
            let _ = engine.db.insert_audit(&user, &ip.to_string(), if left.is_some() { "sign-in (recovery code)" } else { "sign-in (two-step code)" }, "", 200);
            let mut answer = serde_json::json!({ "user": user, "admin": admin });
            crate::web::add_role(&engine, &mut answer, &user, admin);
            if let Some(n) = left {
                answer["recovery_left"] = n.into();
            }
            crate::web::signed_in_with(answer, started, secure)
        }
        Ok(Verdict::Wrong) => {
            let locked = if six_digits {
                wrong_code_at_sign_in(&engine, &user, ip)
            } else {
                let _ = engine.db.insert_audit(&user, &ip.to_string(), "recovery code wrong", "", 401);
                false
            };
            if locked {
                return (StatusCode::TOO_MANY_REQUESTS, TOO_MANY_CODES).into_response();
            }
            (StatusCode::UNAUTHORIZED, "that code is wrong").into_response()
        }
        // (Reset from the command line while this ticket was open.)
        Ok(Verdict::NotOn) => {
            refund();
            hold.burn();
            ticket_expired()
        }
        // No check ran: the try does not count, and the ticket stays good.
        Err(why) => {
            refund();
            if !six_digits {
                crate::auth::recovery_refund(&user, ip);
            }
            no_check(why)
        }
    }
}

/// `GET /api/me/account`: what "Your account" shows.
pub async fn me_account(State(engine): State<Arc<Engine>>, Extension(caller): Extension<Caller>, Extension(admin): Extension<IsAdmin>, role: Option<Extension<crate::auth::Role>>, headers: HeaderMap) -> Response {
    if let Some(r) = signed_in_only(&caller.0) {
        return r;
    }
    let on = match crate::auth::two_step_on(&caller.0) {
        Ok(on) => on,
        Err(()) => return cannot_tell(),
    };
    Json(serde_json::json!({
        "user": caller.0,
        "admin": admin.0,
        "role": role.map(|r| r.0.name()),
        "two_step": {
            "on": on,
            "recovery_left": if on { engine.db.recovery_count(&caller.0).unwrap_or(0) } else { 0 },
            // Too many wrong codes at sign-in, somewhere: someone has the password.
            "codes_locked": on && crate::auth::code_locked_any(&caller.0),
        },
        "tokens": crate::auth::list_tokens(&caller.0),
        "token_limit": crate::auth::MAX_TOKENS,
        "sessions": session_list(&engine, &caller.0, &headers),
    }))
    .into_response()
}

/// The signed-in devices of the account, for "Your account": a name that is
/// safe to show, never the session itself. Sessions from before the list
/// existed have no `signed_in` and no label.
fn session_list(engine: &Engine, user: &str, headers: &HeaderMap) -> Vec<serde_json::Value> {
    let current = this_session(engine, headers);
    crate::auth::list_sessions(user, current.as_deref())
        .into_iter()
        .map(|s| {
            serde_json::json!({
                "id": s.id,
                "this": s.this,
                "label": s.meta.label,
                "ip": s.meta.ip.map(|i| i.to_string()).unwrap_or_default(),
                "signed_in": (s.meta.created > 0).then_some(s.meta.created),
                "last_seen": (s.meta.last_seen > 0).then_some(s.meta.last_seen),
            })
        })
        .collect()
}

/// `POST /api/me/sessions/:id/sign-out`: one of the account's own devices is
/// signed out. An id that is not one of the account's answers as one that
/// does not exist. Signing out this device is the same as signing out.
pub async fn session_sign_out(State(engine): State<Arc<Engine>>, Extension(caller): Extension<Caller>, secure: Option<Extension<crate::tls::Secure>>, headers: HeaderMap, ConnectInfo(peer): ConnectInfo<SocketAddr>, AxPath(id): AxPath<String>) -> Response {
    if let Some(r) = signed_in_only(&caller.0) {
        return r;
    }
    let current = this_session(&engine, &headers);
    let going = rows_losing_alerts(&engine, &caller.0, &[crate::auth::device_forgotten_by_sign_out(&caller.0, &id, current.as_deref())]);
    match crate::auth::end_session_of(&caller.0, &id, current.as_deref()) {
        Ok(Some(gone)) => {
            tell_devices_detached(&engine, going.clone(), format!("device removed for {}", caller.0), format!("{} signed a device out (from {}), and that device no longer gets alerts. If that was not meant to happen, act now: this is the last alert on this device.", caller.0, peer.ip()));
            for s in &going {
                tell_device_change(&engine, &caller.0, &s.device, peer.ip(), false);
            }
            let what = if gone.this { "sign-out" } else { "signed a device out" };
            let _ = engine.db.insert_audit(&caller.0, &peer.ip().to_string(), what, &gone.meta.label, 200);
            let mut resp = Json(serde_json::json!({ "ended": id, "this_device": gone.this })).into_response();
            if gone.this {
                if let Ok(v) = crate::web::session_cookie("", 0, secure.is_some()).parse() {
                    resp.headers_mut().insert(header::SET_COOKIE, v);
                }
            }
            resp
        }
        Ok(None) => (StatusCode::NOT_FOUND, "no such device: it may have signed out already").into_response(),
        Err(()) => (StatusCode::INTERNAL_SERVER_ERROR, "could not sign that device out, so it is still signed in. Try again, or see the server's log").into_response(),
    }
}

/// `POST /api/me/sessions/sign-out-others`: every device of the account but
/// this one is signed out.
pub async fn sessions_sign_out_others(State(engine): State<Arc<Engine>>, Extension(caller): Extension<Caller>, headers: HeaderMap, ConnectInfo(peer): ConnectInfo<SocketAddr>) -> Response {
    if let Some(r) = signed_in_only(&caller.0) {
        return r;
    }
    let current = this_session(&engine, &headers);
    let others = crate::auth::list_sessions(&caller.0, current.as_deref()).iter().filter(|s| !s.this).count();
    if others == 0 {
        return Json(serde_json::json!({ "ended": 0 })).into_response();
    }
    let ending = crate::auth::devices_ending(&caller.0, current.as_deref());
    let going = rows_losing_alerts(&engine, &caller.0, &ending);
    if !crate::auth::forget_user_and_their_devices(&caller.0, current.as_deref()) {
        return sessions_still_stored();
    }
    tell_devices_detached(&engine, going.clone(), format!("devices removed for {}", caller.0), format!("{} signed out all its other devices (from {}), and they no longer get alerts. If that was not meant to happen, act now: this is the last alert on this device.", caller.0, peer.ip()));
    for s in &going {
        tell_device_change(&engine, &caller.0, &s.device, peer.ip(), false);
    }
    let _ = engine.db.insert_audit(&caller.0, &peer.ip().to_string(), "signed out all other devices", &others.to_string(), 200);
    Json(serde_json::json!({ "ended": others })).into_response()
}

#[derive(serde::Deserialize)]
pub struct StartReq {
    #[serde(default)]
    password: String,
}

/// Names one start of two-step set-up, so that Cancel in one tab does not
/// cancel a newer start in another. Tells nothing about the secret.
fn setup_id(secret: &str) -> String {
    use sha2::{Digest, Sha256};
    Sha256::digest(format!("setup:{secret}").as_bytes()).iter().take(8).map(|x| format!("{x:02x}")).collect()
}

/// `POST /api/me/two-step/start {"password"}`: a new secret to put in an
/// authenticator app. It is not on until a code from it is confirmed.
pub async fn two_step_start(State(engine): State<Arc<Engine>>, Extension(caller): Extension<Caller>, headers: HeaderMap, ConnectInfo(peer): ConnectInfo<SocketAddr>, Json(req): Json<StartReq>) -> Response {
    if let Some(r) = signed_in_only(&caller.0) {
        return r;
    }
    let a = actor(&engine, &caller.0, peer.ip(), &headers);
    if let Err(r) = prove(&engine, &a, &req.password, None, None, false).await {
        return r;
    }
    let secret = twostep::new_secret();
    match engine.db.two_step_begin(&caller.0, &secret, now()) {
        Ok(true) => {}
        Ok(false) => return (StatusCode::CONFLICT, "two-step codes are already on").into_response(),
        Err(e) => return not_saved("could not start two-step codes", format!("{e:#}")),
    }
    let uri = twostep::otpauth_uri(&crate::links::own_name(&engine), &caller.0, &secret);
    let svg = match qrcode::QrCode::with_error_correction_level(uri.as_bytes(), qrcode::EcLevel::M) {
        Ok(q) => q.render::<qrcode::render::svg::Color>().min_dimensions(220, 220).quiet_zone(true).build(),
        Err(e) => {
            let _ = engine.db.two_step_cancel(&caller.0, &secret);
            return not_saved("could not draw the QR code", e);
        }
    };
    no_store(serde_json::json!({ "secret": secret, "uri": uri, "svg": svg, "expires_in": ENROL_SECS, "setup_id": setup_id(&secret) }))
}

#[derive(serde::Deserialize)]
pub struct CancelReq {
    #[serde(default)]
    setup_id: String,
}

/// `POST /api/me/two-step/cancel {"setup_id"}`: forget the secret that start
/// gave out, if it is still waiting for its first code. Never touches
/// two-step codes that are on, or a newer start.
pub async fn two_step_cancel(State(engine): State<Arc<Engine>>, Extension(caller): Extension<Caller>, ConnectInfo(peer): ConnectInfo<SocketAddr>, Json(req): Json<CancelReq>) -> Response {
    if let Some(r) = signed_in_only(&caller.0) {
        return r;
    }
    let row = match engine.db.two_step_get(&caller.0) {
        Ok(r) => r,
        Err(e) => {
            tracing::error!("two-step state unreadable: {e:#}");
            return cannot_tell();
        }
    };
    let mut cancelled = false;
    if let Some(row) = row.filter(|r| !r.enabled && setup_id(&r.secret) == req.setup_id) {
        if let Err(e) = engine.db.two_step_cancel(&caller.0, &row.secret) {
            return not_saved("could not cancel", format!("{e:#}"));
        }
        cancelled = true;
        let _ = engine.db.insert_audit(&caller.0, &peer.ip().to_string(), "two-step set-up cancelled", &caller.0, 200);
    }
    Json(serde_json::json!({ "cancelled": cancelled })).into_response()
}

#[derive(serde::Deserialize)]
pub struct ConfirmReq {
    #[serde(default)]
    code: String,
}

/// `POST /api/me/two-step/confirm {"code"}`: on, once a code is right. Other
/// sessions of the account end; the recovery codes are answered once. Tries
/// are limited, and a mistyped first code is no reason to warn anyone.
pub async fn two_step_confirm(State(engine): State<Arc<Engine>>, Extension(caller): Extension<Caller>, headers: HeaderMap, ConnectInfo(peer): ConnectInfo<SocketAddr>, Json(req): Json<ConfirmReq>) -> Response {
    if let Some(r) = signed_in_only(&caller.0) {
        return r;
    }
    let user = caller.0.clone();
    let ip = peer.ip();
    let row = match engine.db.two_step_get(&user) {
        Ok(r) => r,
        Err(e) => {
            tracing::error!("two-step state unreadable: {e:#}");
            return cannot_tell();
        }
    };
    let Some(row) = row.filter(|r| !r.enabled && now() - r.created_at <= ENROL_SECS) else {
        return (StatusCode::CONFLICT, "start again: nothing is waiting for a code").into_response();
    };
    let keep = this_session(&engine, &headers);
    // (Limited per session; HTTP Basic has none, and then per account and address.)
    let may = match &keep {
        Some(s) => crate::auth::session_code_try(s),
        None => crate::auth::code_try(&user, ip),
    };
    if !may {
        return (StatusCode::TOO_MANY_REQUESTS, TOO_MANY_HERE).into_response();
    }
    let Some(step) = twostep::matching_step(&row.secret, &req.code, now(), row.last_step) else {
        let _ = engine.db.insert_audit(&user, &ip.to_string(), "two-step code wrong (while turning it on)", "", 403);
        return (StatusCode::FORBIDDEN, "that code is wrong: check the phone's clock and try the newest code").into_response();
    };
    match &keep {
        Some(s) => crate::auth::session_code_ok(s),
        None => crate::auth::code_ok(&user, ip),
    }
    let (codes, hashes) = tokio::task::spawn_blocking(twostep::new_recovery_codes).await.unwrap_or_default();
    if codes.is_empty() {
        return cannot_tell();
    }
    // (Only the secret just checked: a new start in the meantime replaced it.)
    match engine.db.two_step_enable(&user, step, &row.secret, &hashes, now()) {
        Ok(true) => {}
        Ok(false) => return (StatusCode::CONFLICT, "start again: this code was for an older picture").into_response(),
        Err(e) => return not_saved("could not turn on two-step codes", format!("{e:#}")),
    }
    // Other devices must really be signed out, also after a restart.
    let others_gone = crate::auth::forget_user(&user, keep.as_deref());
    crate::links::cancel_codes();
    if !others_gone {
        // If not, it is not on. If it cannot even be taken back it is on, and the
        // person must have the recovery codes: the truth, either way.
        return match engine.db.two_step_remove(&user) {
            Ok(_) => {
                tracing::error!("two-step codes not turned on for {user}: its sessions could not be removed from the database");
                (StatusCode::INTERNAL_SERVER_ERROR, "two-step codes were not turned on, because your other devices could not be signed out for good. Remove the account from your app and start again").into_response()
            }
            Err(e) => {
                tracing::error!("two-step codes stayed on for {user} though its sessions could not be removed: {e:#}");
                let _ = engine.db.insert_audit(&user, &ip.to_string(), "two-step turned on, other sessions may return at a restart", &user, 200);
                no_store(serde_json::json!({ "on": true, "recovery_codes": codes, "warning": "Two-step codes are on, but your other devices may stay signed in after a restart." }))
            }
        };
    }
    let _ = engine.db.insert_audit(&user, &ip.to_string(), "two-step turned on", &user, 200);
    no_store(serde_json::json!({ "on": true, "recovery_codes": codes }))
}

#[derive(serde::Deserialize)]
pub struct ProveReq {
    #[serde(default)]
    password: String,
    #[serde(default)]
    code: Option<String>,
    #[serde(default)]
    recovery_code: Option<String>,
}

/// `POST /api/me/two-step/off {"password", "code" | "recovery_code"}`.
pub async fn two_step_off(State(engine): State<Arc<Engine>>, Extension(caller): Extension<Caller>, headers: HeaderMap, ConnectInfo(peer): ConnectInfo<SocketAddr>, Json(req): Json<ProveReq>) -> Response {
    if let Some(r) = signed_in_only(&caller.0) {
        return r;
    }
    let user = caller.0.clone();
    match crate::auth::two_step_on(&user) {
        Ok(true) => {}
        Ok(false) => return (StatusCode::CONFLICT, "two-step codes are not on").into_response(),
        Err(()) => return cannot_tell(),
    }
    let a = actor(&engine, &user, peer.ip(), &headers);
    if let Err(r) = prove(&engine, &a, &req.password, req.code.as_deref(), req.recovery_code.as_deref(), true).await {
        return r;
    }
    // (The password check is slow: the account may have changed since.)
    if crate::auth::generation(&user) != caller.1 {
        return changed_meanwhile();
    }
    if let Err(e) = engine.db.two_step_remove(&user) {
        return not_saved("could not turn off two-step codes", format!("{e:#}"));
    }
    let gone = crate::auth::forget_user(&user, a.session.as_deref());
    let _ = engine.db.insert_audit(&user, &peer.ip().to_string(), "two-step turned off", &user, 200);
    tell_admins(&engine, format!("two-step codes turned off for {user}"), format!("{user} turned off their own two-step codes (from {}). If this was not them, change that password and turn the codes on again.", peer.ip()));
    if !gone {
        return sessions_still_stored();
    }
    Json(serde_json::json!({ "on": false })).into_response()
}

/// `POST /api/me/two-step/recovery {"password", "code" | "recovery_code"}`: ten new codes.
pub async fn two_step_recovery(State(engine): State<Arc<Engine>>, Extension(caller): Extension<Caller>, headers: HeaderMap, ConnectInfo(peer): ConnectInfo<SocketAddr>, Json(req): Json<ProveReq>) -> Response {
    if let Some(r) = signed_in_only(&caller.0) {
        return r;
    }
    let user = caller.0.clone();
    match crate::auth::two_step_on(&user) {
        Ok(true) => {}
        Ok(false) => return (StatusCode::CONFLICT, "two-step codes are not on").into_response(),
        Err(()) => return cannot_tell(),
    }
    let a = actor(&engine, &user, peer.ip(), &headers);
    if let Err(r) = prove(&engine, &a, &req.password, req.code.as_deref(), req.recovery_code.as_deref(), true).await {
        return r;
    }
    let (codes, hashes) = tokio::task::spawn_blocking(twostep::new_recovery_codes).await.unwrap_or_default();
    if codes.is_empty() {
        return cannot_tell();
    }
    // (All that hashing is slow: the account may have changed since.)
    if crate::auth::generation(&user) != caller.1 {
        return changed_meanwhile();
    }
    if let Err(e) = engine.db.recovery_replace(&user, &hashes) {
        return not_saved("could not make new recovery codes", format!("{e:#}"));
    }
    let _ = engine.db.insert_audit(&user, &peer.ip().to_string(), "new recovery codes made", &user, 200);
    tell_admins(&engine, format!("new recovery codes for {user}"), format!("{user} made new recovery codes (from {}), and the old ones stopped working. If that was not them, someone has that password and a code: disable the account in Users.", peer.ip()));
    no_store(serde_json::json!({ "recovery_codes": codes }))
}

/// Printable, on one line, not too long; empty if nothing is left.
fn clean_token_name(name: &str) -> String {
    name.chars().filter(|c| !c.is_control() && !crate::push_http::invisible(*c)).take(TOKEN_NAME_MAX).collect::<String>().trim().to_string()
}

#[derive(serde::Deserialize)]
pub struct TokenReq {
    #[serde(default)]
    name: String,
    /// "viewer" (the default) or "admin".
    #[serde(default)]
    scope: Option<String>,
    #[serde(default)]
    password: String,
    #[serde(default)]
    code: Option<String>,
    #[serde(default)]
    recovery_code: Option<String>,
}

/// `POST /api/me/tokens {"name", "scope"?, "password", "code"?}`: a new device
/// token, answered once.
pub async fn token_create(State(engine): State<Arc<Engine>>, Extension(caller): Extension<Caller>, Extension(admin): Extension<IsAdmin>, role: Option<Extension<crate::auth::Role>>, headers: HeaderMap, ConnectInfo(peer): ConnectInfo<SocketAddr>, Json(req): Json<TokenReq>) -> Response {
    if let Some(r) = signed_in_only(&caller.0) {
        return r;
    }
    let user = caller.0.clone();
    let name = clean_token_name(&req.name);
    if name.is_empty() {
        return (StatusCode::BAD_REQUEST, "give it a name, like the app or device that will use it").into_response();
    }
    let want_admin = match req.scope.as_deref().unwrap_or("viewer") {
        "viewer" => false,
        "admin" => true,
        _ => return (StatusCode::BAD_REQUEST, "scope must be viewer or admin").into_response(),
    };
    // Before the code is used up: this caller's rights (a session from a linked
    // server may have fewer than its account) and the account's, as they are now.
    let (account_admin, account_operator) = {
        let cfg = engine.config.lock().unwrap();
        (crate::auth::accounts(&cfg).iter().any(|(u, _, a)| *u == user && *a), crate::auth::is_operator(&cfg, &user))
    };
    // An operator's control token does what the operator may (see web.rs auth_mw).
    let session_operator = role.is_some_and(|r| r.0.is_operator());
    if want_admin && !((account_admin && admin.0) || (account_operator && session_operator)) {
        return (StatusCode::FORBIDDEN, "only an admin or an operator can have a token with control rights").into_response();
    }
    // (From when the request was let in: anything changing the account since,
    // like a disable during the slow password check, stops this.)
    let generation = caller.1;
    let a = actor(&engine, &user, peer.ip(), &headers);
    if let Err(r) = prove(&engine, &a, &req.password, req.code.as_deref(), req.recovery_code.as_deref(), true).await {
        return r;
    }
    match crate::auth::add_token(&user, &name, want_admin, generation) {
        Ok(Some((value, info))) => {
            let _ = engine.db.insert_audit(&user, &peer.ip().to_string(), &format!("device token made ({})", info.scope), &name, 200);
            if want_admin {
                tell_admins(&engine, format!("device token with control rights for {user}"), format!("{user} made a device token with control rights, \"{name}\" (from {}). If that was not them, revoke it in Users.", peer.ip()));
            }
            no_store(serde_json::json!({ "token": value, "info": info }))
        }
        Ok(None) => changed_meanwhile(),
        Err(crate::auth::AddTokenError::TooMany) => (StatusCode::BAD_REQUEST, format!("this account already has {} device tokens: revoke one first", crate::auth::MAX_TOKENS)).into_response(),
        Err(crate::auth::AddTokenError::NotSaved) => not_saved("could not make the token", "see above"),
    }
}

fn revoked(result: Result<bool, ()>, id: &str) -> Result<Response, Response> {
    match result {
        Ok(true) => Ok(Json(serde_json::json!({ "revoked": id })).into_response()),
        Ok(false) => Err((StatusCode::NOT_FOUND, "no such token").into_response()),
        Err(()) => Err((StatusCode::INTERNAL_SERVER_ERROR, "could not revoke it, so it still works. Try again, or see the server's log").into_response()),
    }
}

/// `DELETE /api/me/tokens/:id`.
pub async fn token_revoke(State(engine): State<Arc<Engine>>, Extension(caller): Extension<Caller>, ConnectInfo(peer): ConnectInfo<SocketAddr>, AxPath(id): AxPath<String>) -> Response {
    match revoked(crate::auth::revoke_token(&caller.0, &id), &id) {
        Ok(r) => {
            let _ = engine.db.insert_audit(&caller.0, &peer.ip().to_string(), "device token revoked", &caller.0, 200);
            r
        }
        Err(r) => r,
    }
}

/// `GET /api/users/:name/tokens` (admin): names and dates, never the values.
pub async fn user_tokens(Extension(admin): Extension<IsAdmin>, AxPath(name): AxPath<String>) -> Response {
    if !admin.0 {
        return (StatusCode::FORBIDDEN, "admin access required").into_response();
    }
    Json(serde_json::json!({ "tokens": crate::auth::list_tokens(name.trim()) })).into_response()
}

/// `DELETE /api/users/:name/tokens/:id` (admin).
pub async fn user_token_revoke(State(engine): State<Arc<Engine>>, Extension(caller): Extension<Caller>, Extension(admin): Extension<IsAdmin>, ConnectInfo(peer): ConnectInfo<SocketAddr>, AxPath((name, id)): AxPath<(String, String)>) -> Response {
    if !admin.0 {
        return (StatusCode::FORBIDDEN, "admin access required").into_response();
    }
    match revoked(crate::auth::revoke_token(name.trim(), &id), &id) {
        Ok(r) => {
            let _ = engine.db.insert_audit(&caller.0, &peer.ip().to_string(), "device token revoked", name.trim(), 200);
            r
        }
        Err(r) => r,
    }
}

/// `POST /api/users/:name/two-step/off {"my_password", "my_code"?}` (admin):
/// for someone who lost their phone. Not for yourself: that takes your code.
/// Their sessions end.
pub async fn user_two_step_off(State(engine): State<Arc<Engine>>, Extension(caller): Extension<Caller>, Extension(admin): Extension<IsAdmin>, headers: HeaderMap, ConnectInfo(peer): ConnectInfo<SocketAddr>, AxPath(name): AxPath<String>, Json(proof): Json<MyProof>) -> Response {
    let name = name.trim().to_string();
    if !admin.0 {
        return (StatusCode::FORBIDDEN, "admin access required").into_response();
    }
    if caller.0 == name {
        return (StatusCode::BAD_REQUEST, "turn off your own in Your account: it takes your password and code").into_response();
    }
    match engine.db.two_step_enabled(&name) {
        Ok(true) => {}
        Ok(false) => return (StatusCode::CONFLICT, "two-step codes are not on for this account").into_response(),
        Err(e) => {
            tracing::error!("two-step state unreadable: {e:#}");
            return cannot_tell();
        }
    }
    if let Err(r) = prove_admin(&engine, &caller.0, peer.ip(), &headers, &proof, caller.1).await {
        return r;
    }
    {
        // Checked again here: this admin may have been disabled or demoted during the slow check.
        let cfg = engine.config.lock().unwrap();
        if !crate::users_http::caller_may_change_accounts(&cfg, &caller.0) || crate::auth::generation(&caller.0) != caller.1 {
            return (StatusCode::UNAUTHORIZED, "authentication required").into_response();
        }
        if let Err(e) = engine.db.two_step_remove(&name) {
            return not_saved("could not turn off two-step codes", format!("{e:#}"));
        }
    }
    let gone = crate::auth::forget_user(&name, None);
    crate::auth::code_clear(&name);
    let _ = engine.db.insert_audit(&caller.0, &peer.ip().to_string(), "two-step turned off by an admin", &name, 200);
    tell_admins(&engine, format!("two-step codes turned off for {name}"), format!("{} turned off the two-step codes of {name} (from {}). If that was not meant to happen, change that password and turn the codes on again.", caller.0, peer.ip()));
    if !gone {
        return sessions_still_stored();
    }
    Json(serde_json::json!({ "on": false })).into_response()
}

/// Changes from the command line (`ambersight user set-password`, `enable`)
/// wait in the database, so that they also work while the server runs: the
/// server keeps its own copy of the settings and would save over a change made
/// to the file. Applied here, at start and every few seconds.
pub fn start_account_requests(engine: Arc<Engine>) {
    std::thread::spawn(move || {
        // What was done but could not be taken off the list: only that is left to do.
        let mut finished = std::collections::HashSet::new();
        loop {
            apply_account_requests(&engine, &mut finished);
            flush_gathered(&engine, false);
            std::thread::sleep(std::time::Duration::from_secs(REQUEST_SECS));
        }
    });
}

/// How keeping a change from the command line went.
#[derive(Debug, PartialEq)]
enum Kept {
    NoSuchAccount,
    /// It could not be saved, and stays on the list as it is, under its own
    /// number: later ones wait behind it, so an old password can never overwrite a newer one.
    NotSaved,
    /// In the settings, and marked on the list as being so.
    Saved,
    /// A newer change came first, just now: this one is not to be applied.
    Superseded,
}

/// A change was made from a web page: what the command line asked for before it
/// and is still waiting (its save kept failing, say) must not overwrite it. Call
/// under the settings lock, which `keep_request` holds too.
pub(crate) fn supersede_requests(engine: &Engine, user: &str, kind: &str) {
    if let Err(e) = engine.db.account_requests_supersede(user, kind) {
        tracing::error!("could not set aside an older change from the command line for {user}: {e:#}");
    }
}

/// Put a change from the command line in the settings, and mark it as done on the
/// list in the same step, under the settings lock: a crash leaves a row
/// that is either not in the settings or marked, or that has just been saved and
/// nothing can have changed since. So a crash neither loses it nor applies it
/// over something newer.
fn keep_request(engine: &Engine, r: &crate::db::AccountRequest) -> Kept {
    let mut cfg = engine.config.lock().unwrap();
    // (Read before the lock was taken: a web page may have been first.)
    match engine.db.account_request_waiting(r.id) {
        Ok(true) => {}
        Ok(false) => return Kept::Superseded,
        Err(e) => {
            tracing::error!("could not read a change from the command line: {e:#}");
            return Kept::NotSaved;
        }
    }
    crate::users_http::migrate_legacy(&mut cfg);
    let before = cfg.server.users.clone();
    let Some(u) = cfg.server.users.iter_mut().find(|u| u.username.trim() == r.user) else { return Kept::NoSuchAccount };
    let changes = if r.kind == "password" { u.password != r.hash } else { u.disabled };
    if r.kind == "password" {
        u.password = r.hash.clone();
    } else {
        u.disabled = false;
    }
    if changes {
        if let Err(e) = cfg.save(&engine.config_path) {
            tracing::error!("could not save a change from the command line: {e:#}");
            cfg.server.users = before;
            return Kept::NotSaved;
        }
    }
    if let Err(e) = engine.db.account_request_claim(r.id) {
        tracing::error!("could not mark a change from the command line as done, so it is taken back and tried again: {e:#}");
        cfg.server.users = before;
        if let Err(e) = cfg.save(&engine.config_path) {
            tracing::error!("could not take it back in the settings file: {e:#}");
        }
        return Kept::NotSaved;
    }
    Kept::Saved
}

fn apply_account_requests(engine: &Arc<Engine>, finished: &mut std::collections::HashSet<i64>) {
    let Ok(requests) = engine.db.account_requests() else { return };
    finished.retain(|id| requests.iter().any(|r| r.id == *id));
    // Accounts with a change that could not be kept: their later ones wait.
    let mut waiting: std::collections::HashSet<String> = std::collections::HashSet::new();
    for r in requests {
        if !finished.contains(&r.id) {
            if waiting.contains(&r.user) {
                continue;
            }
            if !apply_one_request(engine, &r) {
                waiting.insert(r.user);
                continue;
            }
        }
        // Off the list once all is done. If that fails it is only tried again.
        match engine.db.account_request_done(r.id) {
            Ok(()) => {
                finished.remove(&r.id);
            }
            Err(e) => {
                tracing::error!("could not take a change from the command line off the list: {e:#}");
                finished.insert(r.id);
            }
        }
    }
}

/// False: it could not be kept, and is looked at again later.
fn apply_one_request(engine: &Arc<Engine>, r: &crate::db::AccountRequest) -> bool {
    let dropped = |why: &str| {
        let _ = engine.db.insert_audit("(command line)", "", why, &r.user, 400);
        tracing::error!("command line request for {}: {why}", r.user);
    };
    let is_password = r.kind == "password";
    match r.kind.as_str() {
        // (Done by the command itself: what is left is telling people.)
        "two-step-reset" => {
            crate::auth::code_clear(&r.user);
            let _ = engine.db.insert_audit("(command line)", "", "two-step reset seen by the server", &r.user, 200);
            tell_admins(engine, format!("two-step codes turned off for {}", r.user), format!("The two-step codes of {} were turned off from the command line on the server. If that was not meant to happen, change that password and turn the codes on again.", r.user));
            return true;
        }
        "password" if !crate::auth::is_hashed(&r.hash) => {
            dropped("change from the command line dropped: not a password hash");
            return true;
        }
        "password" | "enable" => {}
        _ => {
            dropped("change from the command line dropped: unknown kind");
            return true;
        }
    }
    let mut superseded = r.superseded;
    // Marked: it is in the settings already, and was before anything newer could
    // be. What is left is only what follows from it.
    if !r.claimed {
        match keep_request(engine, r) {
            Kept::NoSuchAccount => {
                dropped("change from the command line dropped: no such account");
                return true;
            }
            Kept::NotSaved => return false,
            Kept::Superseded => superseded = true,
            Kept::Saved => {}
        }
    }
    if superseded {
        // The newer change stays. Someone asked for signing out and deleting tokens
        // from the computer, though: that still happens.
        if is_password {
            if let Err(f) = crate::auth::password_changed(&r.user, None) {
                tracing::error!("sessions or known browsers of {} could not be removed from the database after set-password ({f:?})", r.user);
            }
            if r.revoke_tokens && !crate::auth::delete_user_tokens(&r.user) {
                tracing::error!("device tokens of {} could not be removed after set-password --revoke-tokens", r.user);
            }
        }
        let _ = engine.db.insert_audit("(command line)", "", "change from the command line not applied: a newer change came first", &r.user, 409);
        tell_admins(engine, format!("command line change for {} not applied", r.user), format!("A change to {} from the command line on the server was not applied, because the account was changed from a web page after it was asked for. Run the command again if it is still wanted.", r.user));
        return true;
    }
    if is_password {
        tell_of_password_change(engine, &r.user, "from the command line on the server");
        if let Err(f) = crate::auth::password_changed(&r.user, None) {
            tracing::error!("sessions or known browsers of {} could not be removed from the database after set-password ({f:?})", r.user);
        }
        crate::auth::code_clear(&r.user);
        if r.revoke_tokens && !crate::auth::delete_user_tokens(&r.user) {
            tracing::error!("device tokens of {} could not be removed after set-password --revoke-tokens", r.user);
        }
    } else {
        crate::auth::forget_user(&r.user, None);
        tell_admins(engine, format!("{} enabled", r.user), format!("The account {} was enabled from the command line on the server.", r.user));
    }
    let what = if is_password { if r.revoke_tokens { "password changed (set-password), device tokens revoked" } else { "password changed (set-password)" } } else { "account enabled (user enable)" };
    let _ = engine.db.insert_audit("(command line)", "", what, &r.user, 200);
    tracing::info!("{what} for {}", r.user);
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn token_names_are_printable_and_short() {
        assert_eq!(clean_token_name("  Pixel\napp\t "), "Pixelapp");
        assert_eq!(clean_token_name("\n\t"), "");
        assert_eq!(clean_token_name("Pix\u{202e}el\u{200b}"), "Pixel", "nothing that does not show, in an alert");
        assert_eq!(clean_token_name(&"x".repeat(200)).len(), TOKEN_NAME_MAX);
    }

    fn engine_with_ann() -> (Arc<Engine>, String) {
        let cfg: crate::config::Config = toml::from_str("[[server.users]]\nusername = \"req-ann\"\npassword = \"$argon2id$now\"\n").unwrap();
        let path = std::env::temp_dir().join(format!("ambersight-requests-{}-{:?}.toml", std::process::id(), std::thread::current().id())).to_string_lossy().to_string();
        let mut e = Engine::for_test(cfg);
        Arc::get_mut(&mut e).unwrap().config_path = path.clone();
        (e, path)
    }

    fn pw(e: &Engine) -> String {
        e.config.lock().unwrap().server.users[0].password.clone()
    }

    #[test]
    fn a_failed_save_keeps_the_row_and_its_place_and_a_newer_web_change_wins() {
        let (e, path) = engine_with_ann();
        let mut done = std::collections::HashSet::new();
        e.db.account_request_add("req-ann", "password", "$argon2id$old", false).unwrap();
        e.db.account_request_add("req-ann", "password", "$argon2id$new", false).unwrap();
        let ids: Vec<i64> = e.db.account_requests().unwrap().iter().map(|r| r.id).collect();
        // A folder where the file should go: the save fails.
        let good = e.config_path.clone();
        std::fs::create_dir_all(&good).unwrap();
        apply_account_requests(&e, &mut done);
        assert_eq!(e.db.account_requests().unwrap().iter().map(|r| r.id).collect::<Vec<_>>(), ids, "both wait, in their order");
        assert_eq!(pw(&e), "$argon2id$now", "undone in memory");
        // A password set from a web page meanwhile: what waited from before does not overwrite it.
        {
            let mut c = e.config.lock().unwrap();
            c.server.users[0].password = "$argon2id$web".into();
            supersede_requests(&e, "req-ann", "password");
        }
        std::fs::remove_dir_all(&good).unwrap();
        apply_account_requests(&e, &mut done);
        assert_eq!(pw(&e), "$argon2id$web");
        assert!(e.db.account_requests().unwrap().is_empty());
        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_file(format!("{path}.last"));
    }

    #[test]
    fn requests_apply_in_order_and_a_marked_one_is_never_applied_again() {
        let (e, path) = engine_with_ann();
        let mut done = std::collections::HashSet::new();
        e.db.account_request_add("req-ann", "password", "$argon2id$one", false).unwrap();
        e.db.account_request_add("req-ann", "password", "$argon2id$two", false).unwrap();
        apply_account_requests(&e, &mut done);
        assert_eq!(pw(&e), "$argon2id$two");
        assert!(e.db.account_requests().unwrap().is_empty());
        // Left by a crash after it was saved, with something newer in the settings now.
        e.db.account_request_add("req-ann", "password", "$argon2id$one", false).unwrap();
        let id = e.db.account_requests().unwrap()[0].id;
        e.db.account_request_claim(id).unwrap();
        apply_account_requests(&e, &mut done);
        assert_eq!(pw(&e), "$argon2id$two");
        assert!(e.db.account_requests().unwrap().is_empty());
        // Left by a crash before it was saved: not lost.
        e.db.account_request_add("req-ann", "password", "$argon2id$three", false).unwrap();
        apply_account_requests(&e, &mut done);
        assert_eq!(pw(&e), "$argon2id$three");
        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_file(format!("{path}.last"));
    }

    #[test]
    fn a_row_read_before_a_web_change_is_not_applied_after_it() {
        let (e, path) = engine_with_ann();
        e.db.account_request_add("req-ann", "password", "$argon2id$old", false).unwrap();
        let row = e.db.account_requests().unwrap().remove(0);
        {
            let mut c = e.config.lock().unwrap();
            c.server.users[0].password = "$argon2id$web".into();
            supersede_requests(&e, "req-ann", "password");
        }
        assert_eq!(keep_request(&e, &row), Kept::Superseded);
        assert_eq!(pw(&e), "$argon2id$web");
        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_file(format!("{path}.last"));
    }

    #[test]
    fn gathered_names_are_all_there_or_counted() {
        let (mut items, mut more) = (Vec::new(), 0);
        for i in 0..MAX_GATHERED + 3 {
            add_gathered(&mut items, &mut more, format!("n{i}"));
        }
        assert_eq!((items.len(), more), (MAX_GATHERED, 3));
        assert!(listed(&items, more).ends_with("and 3 more (see the audit log)"));
        assert_eq!(listed(&["a".to_string(), "b".to_string()], 0), "a, b");
    }

    #[test]
    fn past_the_limit_no_thread_is_started_for_an_alert() {
        // (The count is shared with whatever else runs now: only over the limit is looked at.)
        let seats: Vec<AlertSeat> = (0..MAX_ALERT_THREADS).filter_map(|_| alert_seat(false)).collect();
        assert!(alert_seat(false).is_none());
        let forced = alert_seat(true);
        assert!(forced.is_some(), "when the server stops, all still go");
        drop(forced);
        drop(seats);
        assert!(alert_seat(false).is_some());
    }

    #[test]
    fn nothing_for_a_server_with_no_accounts() {
        assert!(signed_in_only("(open)").is_some());
        assert!(signed_in_only("").is_some());
        assert!(signed_in_only("ann").is_none());
    }

    #[test]
    fn a_failed_revoke_is_never_reported_as_revoked() {
        assert_eq!(revoked(Err(()), "x").unwrap_err().status(), StatusCode::INTERNAL_SERVER_ERROR);
        assert_eq!(revoked(Ok(false), "x").unwrap_err().status(), StatusCode::NOT_FOUND);
        assert!(revoked(Ok(true), "x").is_ok());
    }

    #[test]
    fn a_setup_id_names_one_start_and_says_nothing_of_the_secret() {
        assert_eq!(setup_id("AAAA"), setup_id("AAAA"));
        assert_ne!(setup_id("AAAA"), setup_id("BBBB"));
        assert!(!setup_id("AAAA").contains("AAAA"));
    }
}
