//! Signing in from a new device and the list of signed-in devices, through the
//! handlers a browser reaches.

use crate::account_http::{flush_gathering, login_totp, me_account, session_sign_out, sent_log, sessions_sign_out_others};
use crate::engine::Engine;
use crate::web::{login, pair, Caller, IsAdmin};
use axum::body::Body;
use axum::extract::{ConnectInfo, Path, Request, State};
use axum::http::{header, StatusCode};
use axum::response::Response;
use axum::Extension;
use sha2::{Digest, Sha256};
use std::net::SocketAddr;
use std::sync::Arc;

const EDGE: &str = "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/120.0.0.0 Safari/537.36 Edg/120.0.0.0";
const SAFARI_IPHONE: &str = "Mozilla/5.0 (iPhone; CPU iPhone OS 17_0 like Mac OS X) AppleWebKit/605.1.15 (KHTML, like Gecko) Version/17.0 Mobile/15E148 Safari/604.1";

fn engine(users: &[&str]) -> Arc<Engine> {
    let mut cfg: crate::config::Config = toml::from_str("").unwrap();
    for u in users {
        cfg.server.users.push(crate::config::UserConfig { disabled: false, username: (*u).into(), password: crate::auth::hash_password("pw-test-1"), admin: false, cameras: Vec::new(), ..Default::default() });
    }
    Engine::for_test(cfg)
}

fn peer(ip: &str) -> ConnectInfo<SocketAddr> {
    ConnectInfo(format!("{ip}:5000").parse().unwrap())
}

fn post(path: &str, body: serde_json::Value, ua: &str, cookies: &str, ip: &str) -> Request {
    let mut b = Request::builder().method("POST").uri(path).header(header::USER_AGENT, ua);
    if !cookies.is_empty() {
        b = b.header(header::COOKIE, cookies);
    }
    let mut r = b.body(Body::from(body.to_string())).unwrap();
    r.extensions_mut().insert(peer(ip));
    r
}

fn cookie_named(r: &Response, name: &str) -> Option<String> {
    r.headers().get_all(header::SET_COOKIE).iter().filter_map(|v| v.to_str().ok()).find_map(|c| c.strip_prefix(&format!("{name}=")).map(|v| v.split(';').next().unwrap().to_string()))
}

async fn text(r: Response) -> String {
    String::from_utf8_lossy(&axum::body::to_bytes(r.into_body(), 1 << 20).await.unwrap()).to_string()
}

/// Everything sent that names this account.
fn told(user: &str) -> Vec<(String, String)> {
    sent_log().lock().unwrap().iter().filter(|(t, b)| t.contains(user) || b.contains(user)).cloned().collect()
}

fn flush(e: &Arc<Engine>, user: &str) {
    flush_gathering(e, &format!("new-device:{user}"));
}

async fn sign_in(e: &Arc<Engine>, user: &str, ua: &str, cookies: &str, ip: &str) -> Response {
    login(State(e.clone()), post("/api/login", serde_json::json!({ "username": user, "password": "pw-test-1" }), ua, cookies, ip)).await
}

fn session_header(token: &str) -> axum::http::HeaderMap {
    let mut h = axum::http::HeaderMap::new();
    h.insert(header::COOKIE, format!("as_session={token}").parse().unwrap());
    h
}

fn sessions_json(v: &serde_json::Value) -> Vec<serde_json::Value> {
    v["sessions"].as_array().unwrap().clone()
}

async fn account_of(e: &Arc<Engine>, user: &str, token: &str) -> serde_json::Value {
    let r = me_account(State(e.clone()), Extension(Caller(user.into(), 0)), Extension(IsAdmin(false)), None, session_header(token)).await;
    assert_eq!(r.status(), StatusCode::OK);
    serde_json::from_str(&text(r).await).unwrap()
}

async fn sign_out(e: &Arc<Engine>, user: &str, token: &str, id: &str) -> Response {
    session_sign_out(State(e.clone()), Extension(Caller(user.into(), 0)), None, session_header(token), peer("192.0.2.7"), Path(id.to_string())).await
}

#[tokio::test]
async fn every_way_in_notes_the_browser_and_says_how() {
    let e = engine(&["dl-ann"]);
    let u = "dl-ann";
    // Password: the first sign-in of the account is silent; it sets both cookies.
    let r = sign_in(&e, u, EDGE, "", "192.0.2.10").await;
    assert_eq!(r.status(), StatusCode::OK);
    let (session, device) = (cookie_named(&r, "as_session").unwrap(), cookie_named(&r, "as_device").unwrap());
    assert_eq!(device.len(), 64);
    assert!(r.headers().get_all(header::SET_COOKIE).iter().all(|c| c.to_str().unwrap().contains("HttpOnly")));
    assert!(told(u).is_empty());
    // A wrong password stores nothing and sets nothing.
    let r = login(State(e.clone()), post("/api/login", serde_json::json!({ "username": u, "password": "wrong" }), SAFARI_IPHONE, "", "192.0.2.11")).await;
    assert_eq!(r.status(), StatusCode::UNAUTHORIZED);
    assert!(cookie_named(&r, "as_device").is_none() && cookie_named(&r, "as_session").is_none());
    assert_eq!(e.db.known_device_count(u), 1);
    // The same browser, with its cookie: known.
    sign_in(&e, u, EDGE, &format!("as_device={device}"), "192.0.2.10").await;
    assert!(told(u).is_empty());
    // A new browser by password.
    let r = sign_in(&e, u, SAFARI_IPHONE, "", "192.0.2.12").await;
    assert_eq!(r.status(), StatusCode::OK);
    let sent = told(u);
    assert_eq!(sent.len(), 1, "{sent:?}");
    assert!(sent[0].1.contains("Safari on iPhone") && sent[0].1.contains("192.0.2.12") && sent[0].1.contains("with the password"), "{}", sent[0].1);
    // QR code.
    flush(&e, u);
    let code = crate::auth::new_pair_code(u);
    let r = pair(State(e.clone()), post("/api/pair", serde_json::json!({ "code": code }), EDGE, "", "192.0.2.13")).await;
    assert_eq!(r.status(), StatusCode::OK);
    assert!(cookie_named(&r, "as_device").is_some());
    assert!(told(u).last().unwrap().1.contains("sign-in QR code"), "{:?}", told(u));
    // A linked server.
    flush(&e, u);
    let code = crate::auth::new_link_signin_code(u, false, crate::auth::generation(u)).unwrap();
    let r = pair(State(e.clone()), post("/api/pair", serde_json::json!({ "code": code }), EDGE, "", "192.0.2.14")).await;
    assert_eq!(r.status(), StatusCode::OK);
    assert!(told(u).last().unwrap().1.contains("another AmberSight server"), "{:?}", told(u));
    // A request that also has a working session of the account: remembered, not told.
    flush(&e, u);
    let before = told(u).len();
    let r = sign_in(&e, u, EDGE, &format!("as_session={session}"), "192.0.2.15").await;
    assert_eq!(r.status(), StatusCode::OK);
    assert_eq!(told(u).len(), before);
    assert_eq!(e.db.known_device_count(u), 5);
    // A QR code that is not good makes nothing.
    let r = pair(State(e.clone()), post("/api/pair", serde_json::json!({ "code": "nope" }), EDGE, "", "192.0.2.16")).await;
    assert_eq!(r.status(), StatusCode::UNAUTHORIZED);
    assert!(cookie_named(&r, "as_device").is_none());
    assert_eq!(e.db.known_device_count(u), 5);
}

#[tokio::test]
async fn two_step_and_recovery_sign_ins_are_told_once_each() {
    let e = engine(&["dl-tom"]);
    let u = "dl-tom";
    // An account that has been signed in to once, with two-step codes on.
    sign_in(&e, u, EDGE, "", "192.0.2.10").await;
    let secret = crate::twostep::new_secret();
    let now = chrono::Utc::now().timestamp();
    let (codes, hashes) = crate::twostep::new_recovery_codes();
    assert!(e.db.two_step_begin(u, &secret, now).unwrap());
    assert!(e.db.two_step_enable(u, 0, &secret, &hashes, now).unwrap());
    let totp = |ua: &str, ip: &str, body: serde_json::Value| {
        let ticket = crate::auth::new_ticket(u, crate::auth::generation(u), false).unwrap();
        let mut body = body;
        body["ticket"] = ticket.into();
        post("/api/login/totp", body, ua, "", ip)
    };
    // A six-digit code from a new browser.
    let r = login_totp(State(e.clone()), totp(SAFARI_IPHONE, "192.0.2.20", serde_json::json!({ "code": crate::twostep::code_for_test(&secret, now) }))).await;
    assert_eq!(r.status(), StatusCode::OK);
    assert!(cookie_named(&r, "as_device").is_some() && cookie_named(&r, "as_session").is_some());
    let sent = told(u);
    assert_eq!(sent.len(), 1, "{sent:?}");
    assert!(sent[0].1.contains("password and a two-step code") && sent[0].1.contains("Safari on iPhone"), "{}", sent[0].1);
    // A wrong code stores nothing.
    let r = login_totp(State(e.clone()), totp(EDGE, "192.0.2.21", serde_json::json!({ "code": "000000" }))).await;
    assert_eq!(r.status(), StatusCode::UNAUTHORIZED);
    assert!(cookie_named(&r, "as_device").is_none());
    assert_eq!(e.db.known_device_count(u), 2);
    // A recovery code from another new browser: one message, with both.
    flush(&e, u);
    let r = login_totp(State(e.clone()), totp(EDGE, "192.0.2.22", serde_json::json!({ "recovery_code": codes[0] }))).await;
    assert_eq!(r.status(), StatusCode::OK);
    let sent = told(u);
    assert_eq!(sent.len(), 2, "{sent:?}");
    assert!(sent[1].0.contains("new device") && sent[1].1.contains("recovery code (9 left)"), "{:?}", sent[1]);
    // A recovery code from a browser that is known: the recovery-code warning, once.
    let known = cookie_named(&r, "as_device").unwrap();
    let ticket = crate::auth::new_ticket(u, crate::auth::generation(u), false).unwrap();
    let r = login_totp(State(e.clone()), post("/api/login/totp", serde_json::json!({ "ticket": ticket, "recovery_code": codes[1] }), EDGE, &format!("as_device={known}"), "192.0.2.22")).await;
    assert_eq!(r.status(), StatusCode::OK);
    let sent = told(u);
    assert_eq!(sent.len(), 3, "{sent:?}");
    assert!(sent[2].0.contains("recovery code") && !sent[2].0.contains("new device"), "{:?}", sent[2]);
}

#[tokio::test]
async fn the_list_shows_this_account_only_with_safe_labels_and_no_secrets() {
    let e = engine(&["dl-list", "dl-other"]);
    let hostile = "<script>alert(1)</script> Chrome/1";
    let a = sign_in(&e, "dl-list", EDGE, "", "192.0.2.30").await;
    let b = sign_in(&e, "dl-list", hostile, "", "192.0.2.31").await;
    let c = sign_in(&e, "dl-other", EDGE, "", "192.0.2.32").await;
    let (ta, tb, tc) = (cookie_named(&a, "as_session").unwrap(), cookie_named(&b, "as_session").unwrap(), cookie_named(&c, "as_session").unwrap());
    let acct = account_of(&e, "dl-list", &ta).await;
    let list = sessions_json(&acct);
    assert_eq!(list.len(), 2, "not the other account's");
    let this: Vec<_> = list.iter().filter(|s| s["this"] == true).collect();
    assert_eq!(this.len(), 1);
    assert_eq!(this[0]["label"], "Edge on Windows");
    assert_eq!(this[0]["ip"], "192.0.2.30");
    assert!(this[0]["signed_in"].as_i64().unwrap() > 0 && this[0]["last_seen"].as_i64().unwrap() > 0);
    let other = list.iter().find(|s| s["this"] == false).unwrap();
    assert_eq!(other["label"], "Chrome", "a hostile header is a fixed label");
    let all = acct.to_string();
    for secret in [&ta, &tb, &tc] {
        let hash: String = Sha256::digest(secret.as_bytes()).iter().map(|x| format!("{x:02x}")).collect();
        assert!(!all.contains(secret.as_str()) && !all.contains(&hash), "the session or its hash is in the answer");
    }
    assert!(!all.contains("script") && !all.contains("Mozilla"), "{all}");
    assert!(list.iter().all(|s| s["id"].as_str().unwrap().starts_with("ses-")));
}

#[tokio::test]
async fn signing_a_device_out_ends_it_and_what_is_open_and_only_for_its_own_account() {
    let e = engine(&["dl-sue", "dl-bob"]);
    let a = sign_in(&e, "dl-sue", EDGE, "", "192.0.2.40").await;
    let b = sign_in(&e, "dl-sue", SAFARI_IPHONE, "", "192.0.2.41").await;
    let c = sign_in(&e, "dl-bob", EDGE, "", "192.0.2.42").await;
    let (ta, tb, tc) = (cookie_named(&a, "as_session").unwrap(), cookie_named(&b, "as_session").unwrap(), cookie_named(&c, "as_session").unwrap());
    let acc = crate::auth::accounts(&e.config.lock().unwrap());
    let bobs_id = sessions_json(&account_of(&e, "dl-bob", &tc).await)[0]["id"].as_str().unwrap().to_string();
    // Someone else's id answers like one that does not exist.
    let theirs = sign_out(&e, "dl-sue", &ta, &bobs_id).await;
    let unknown = sign_out(&e, "dl-sue", &ta, "ses-0000000000000000").await;
    assert_eq!((theirs.status(), text(theirs).await), (unknown.status(), text(unknown).await));
    assert!(crate::auth::session_identity(&tc, &acc).is_some(), "bob is still signed in");
    // A token's id (or anything made up) is no session.
    assert_eq!(sign_out(&e, "dl-sue", &ta, &tb).await.status(), StatusCode::NOT_FOUND);
    // The other device is signed out; video open for the account ends, this device stays signed in.
    let generation = crate::auth::generation("dl-sue");
    let list = sessions_json(&account_of(&e, "dl-sue", &ta).await);
    let phone = list.iter().find(|s| s["this"] == false).unwrap()["id"].as_str().unwrap().to_string();
    let r = sign_out(&e, "dl-sue", &ta, &phone).await;
    assert_eq!(r.status(), StatusCode::OK);
    assert!(cookie_named(&r, "as_session").is_none(), "this device's cookie is left alone");
    assert!(crate::auth::session_identity(&tb, &acc).is_none());
    assert!(crate::auth::session_identity(&ta, &acc).is_some());
    assert!(!crate::auth::still_allowed(&e.config.lock().unwrap(), "dl-sue", generation), "streams and sockets that were open end");
    assert_eq!(sessions_json(&account_of(&e, "dl-sue", &ta).await).len(), 1);
    assert_eq!(sign_out(&e, "dl-sue", &ta, &phone).await.status(), StatusCode::NOT_FOUND, "already gone");
    assert!(e.db.audit_rows().iter().any(|(u, a, d)| u == "dl-sue" && a == "signed a device out" && d == "Safari on iPhone"));
    // This device from the list: the same as signing out.
    let mine = sessions_json(&account_of(&e, "dl-sue", &ta).await)[0]["id"].as_str().unwrap().to_string();
    let r = sign_out(&e, "dl-sue", &ta, &mine).await;
    assert_eq!(r.status(), StatusCode::OK);
    assert!(cookie_named(&r, "as_session").is_some_and(|v| v.is_empty()), "the cookie is cleared");
    assert!(crate::auth::session_identity(&ta, &acc).is_none());
    assert!(e.db.audit_rows().iter().any(|(u, a, _)| u == "dl-sue" && a == "sign-out"));
}

#[tokio::test]
async fn sign_out_all_other_devices_keeps_this_one() {
    let e = engine(&["dl-kim", "dl-lee"]);
    let mut tokens = Vec::new();
    for ip in ["192.0.2.50", "192.0.2.51", "192.0.2.52"] {
        tokens.push(cookie_named(&sign_in(&e, "dl-kim", EDGE, "", ip).await, "as_session").unwrap());
    }
    let lee = cookie_named(&sign_in(&e, "dl-lee", EDGE, "", "192.0.2.53").await, "as_session").unwrap();
    let acc = crate::auth::accounts(&e.config.lock().unwrap());
    let r = sessions_sign_out_others(State(e.clone()), Extension(Caller("dl-kim".into(), 0)), session_header(&tokens[1]), peer("192.0.2.7")).await;
    assert_eq!(r.status(), StatusCode::OK);
    assert_eq!(serde_json::from_str::<serde_json::Value>(&text(r).await).unwrap()["ended"], 2);
    assert!(crate::auth::session_identity(&tokens[1], &acc).is_some());
    assert!(crate::auth::session_identity(&tokens[0], &acc).is_none() && crate::auth::session_identity(&tokens[2], &acc).is_none());
    assert!(crate::auth::session_identity(&lee, &acc).is_some(), "another account is not touched");
    assert!(e.db.audit_rows().iter().any(|(u, a, d)| u == "dl-kim" && a == "signed out all other devices" && d == "2"));
    // Nothing to sign out: nothing changes.
    let generation = crate::auth::generation("dl-kim");
    let r = sessions_sign_out_others(State(e.clone()), Extension(Caller("dl-kim".into(), 0)), session_header(&tokens[1]), peer("192.0.2.7")).await;
    assert_eq!(serde_json::from_str::<serde_json::Value>(&text(r).await).unwrap()["ended"], 0);
    assert_eq!(crate::auth::generation("dl-kim"), generation);
}

// ---- a signed-out device is a new browser again ----

/// These tests use the account database the way the server does (sessions and
/// known browsers in one place), and one at a time.
fn store(e: &Arc<Engine>) -> std::sync::MutexGuard<'static, ()> {
    static ONE_AT_A_TIME: std::sync::Mutex<()> = std::sync::Mutex::new(());
    let g = ONE_AT_A_TIME.lock().unwrap_or_else(|p| p.into_inner());
    crate::auth::attach_store(e.db.clone());
    g
}

/// How many "new device" alerts went out for the account (each writes this entry).
fn alerts(e: &Arc<Engine>, user: &str) -> usize {
    e.db.audit_rows().iter().filter(|(u, a, _)| u == user && a == "sign-in from a new device").count()
}

fn sub(e: &Arc<Engine>, user: &str, endpoint: &str, cookie: Option<&str>) {
    e.db.push_save(&crate::db::PushSub { endpoint: endpoint.into(), user: user.into(), p256dh: "p".into(), auth: "a".into(), device: "d".into(), kind: "browser".into(), ..Default::default() }, 1).unwrap();
    if let Some(c) = cookie {
        e.db.push_set_session_device(endpoint, Some(&crate::devices::browser_hash(c))).unwrap();
    }
}

fn endpoints(e: &Arc<Engine>, user: &str) -> Vec<String> {
    let mut v: Vec<String> = e.db.push_list(Some(user)).unwrap().into_iter().map(|s| s.endpoint).collect();
    v.sort();
    v
}

struct Browser {
    token: String,
    cookie: String,
}

async fn browser(e: &Arc<Engine>, user: &str, ua: &str, ip: &str, old_cookie: &str) -> Browser {
    let jar = if old_cookie.is_empty() { String::new() } else { format!("as_device={old_cookie}") };
    let r = sign_in(e, user, ua, &jar, ip).await;
    assert_eq!(r.status(), StatusCode::OK);
    Browser { token: cookie_named(&r, "as_session").unwrap(), cookie: cookie_named(&r, "as_device").unwrap() }
}

async fn id_of_other(e: &Arc<Engine>, user: &str, token: &str) -> String {
    let list = sessions_json(&account_of(e, user, token).await);
    list.iter().find(|s| s["this"] == false).unwrap()["id"].as_str().unwrap().to_string()
}

#[tokio::test]
async fn signing_another_device_out_makes_its_browser_and_notifications_new_again() {
    let e = engine(&["so-ann"]);
    let _g = store(&e);
    let u = "so-ann";
    let mine = browser(&e, u, EDGE, "192.0.2.60", "").await;
    let thief = browser(&e, u, SAFARI_IPHONE, "192.0.2.61", "").await;
    assert_eq!(alerts(&e, u), 1);
    sub(&e, u, "https://push.example/mine", Some(&mine.cookie));
    sub(&e, u, "https://push.example/thief", Some(&thief.cookie));
    sub(&e, u, "https://push.example/old", None);
    let id = id_of_other(&e, u, &mine.token).await;
    let r = sign_out(&e, u, &mine.token, &id).await;
    assert_eq!(r.status(), StatusCode::OK);
    assert_eq!(e.db.known_device_count(u), 1, "only this browser is known");
    assert_eq!(endpoints(&e, u), vec!["https://push.example/mine", "https://push.example/old"], "its notifications stop; older ones cannot be matched");
    // The same browser signs in again with its cookie: told again.
    browser(&e, u, SAFARI_IPHONE, "192.0.2.61", &thief.cookie).await;
    assert_eq!(alerts(&e, u), 2);
    // Signing out this device from the list keeps its row (like plain sign-out).
    let known = e.db.known_device_count(u);
    let me = sessions_json(&account_of(&e, u, &mine.token).await).iter().find(|s| s["this"] == true).unwrap()["id"].as_str().unwrap().to_string();
    assert_eq!(sign_out(&e, u, &mine.token, &me).await.status(), StatusCode::OK);
    assert_eq!(e.db.known_device_count(u), known);
    browser(&e, u, EDGE, "192.0.2.60", &mine.cookie).await;
    assert_eq!(alerts(&e, u), 2, "no alert for a browser that only signed out");
}

#[tokio::test]
async fn signing_out_all_other_devices_forgets_their_browsers_and_notifications() {
    let e = engine(&["so-bea"]);
    let _g = store(&e);
    let u = "so-bea";
    let mine = browser(&e, u, EDGE, "192.0.2.62", "").await;
    let b = browser(&e, u, SAFARI_IPHONE, "192.0.2.63", "").await;
    let c = browser(&e, u, EDGE, "192.0.2.64", "").await;
    sub(&e, u, "https://push.example/mine", Some(&mine.cookie));
    sub(&e, u, "https://push.example/b", Some(&b.cookie));
    sub(&e, u, "https://push.example/c", Some(&c.cookie));
    let r = sessions_sign_out_others(State(e.clone()), Extension(Caller(u.into(), 0)), session_header(&mine.token), peer("192.0.2.7")).await;
    assert_eq!(r.status(), StatusCode::OK);
    assert_eq!(e.db.known_device_count(u), 1);
    assert_eq!(endpoints(&e, u), vec!["https://push.example/mine"]);
    let before = alerts(&e, u);
    browser(&e, u, EDGE, "192.0.2.64", &c.cookie).await;
    assert_eq!(alerts(&e, u), before + 1);
}

#[tokio::test]
async fn a_failed_forget_is_never_reported_as_a_sign_out() {
    let e = engine(&["so-cy"]);
    let _g = store(&e);
    let u = "so-cy";
    let mine = browser(&e, u, EDGE, "192.0.2.65", "").await;
    let other = browser(&e, u, SAFARI_IPHONE, "192.0.2.66", "").await;
    let id = id_of_other(&e, u, &mine.token).await;
    let acc = crate::auth::accounts(&e.config.lock().unwrap());
    e.db.break_known_devices();
    let r = sign_out(&e, u, &mine.token, &id).await;
    assert_eq!(r.status(), StatusCode::INTERNAL_SERVER_ERROR);
    assert!(crate::auth::session_identity(&other.token, &acc).is_some(), "nothing changed: it is still signed in");
    let r = sessions_sign_out_others(State(e.clone()), Extension(Caller(u.into(), 0)), session_header(&mine.token), peer("192.0.2.7")).await;
    assert_eq!(r.status(), StatusCode::INTERNAL_SERVER_ERROR);
}

#[tokio::test]
async fn a_password_change_makes_every_other_browser_new_again() {
    let e = engine(&["so-dee"]);
    let _g = store(&e);
    let u = "so-dee";
    let mine = browser(&e, u, EDGE, "192.0.2.67", "").await;
    let _ = browser(&e, u, EDGE, "192.0.2.68", "").await;
    // Someone who signed in once, was told, and logged out: their browser is still known.
    let thief = browser(&e, u, SAFARI_IPHONE, "192.0.2.69", "").await;
    crate::auth::end_session(&thief.token);
    assert_eq!(alerts(&e, u), 2);
    assert_eq!(e.db.known_device_count(u), 3);
    // The person changes the password from their browser: theirs stays known.
    assert!(crate::auth::password_changed(u, Some(&mine.token)).is_ok());
    assert_eq!(e.db.known_device_count(u), 1);
    browser(&e, u, EDGE, "192.0.2.67", &mine.cookie).await;
    assert_eq!(alerts(&e, u), 2);
    // The thief signs in with the new password from the same browser: told.
    flush(&e, u);
    browser(&e, u, SAFARI_IPHONE, "192.0.2.69", &thief.cookie).await;
    assert_eq!(alerts(&e, u), 3);
    // By an admin or from the command line (no session of the person): all forgotten.
    assert!(crate::auth::password_changed(u, None).is_ok());
    assert_eq!(e.db.known_device_count(u), 0);
    // It cannot be done: not reported as done.
    e.db.break_known_devices();
    assert_eq!(crate::auth::password_changed(u, None), Err(crate::auth::PwFail::Browsers));
}

fn admin_engine(user: &str) -> Arc<Engine> {
    let mut cfg: crate::config::Config = toml::from_str("").unwrap();
    cfg.server.users.push(crate::config::UserConfig { disabled: false, username: user.into(), password: crate::auth::hash_password("pw-test-1"), admin: true, cameras: Vec::new(), ..Default::default() });
    Engine::for_test(cfg)
}

/// "device removed" messages for the account (what `tell_device_change` sends).
fn removed_told(user: &str) -> usize {
    sent_log().lock().unwrap().iter().filter(|(t, _)| t.starts_with("device removed for ") && t.contains(user)).count()
}

#[tokio::test]
async fn an_admins_phone_losing_alerts_by_sign_out_is_told_to_the_admins() {
    let e = admin_engine("so-gus");
    let _g = store(&e);
    let u = "so-gus";
    let mine = browser(&e, u, EDGE, "192.0.2.70", "").await;
    let phone = browser(&e, u, SAFARI_IPHONE, "192.0.2.71", "").await;
    sub(&e, u, "https://push.example/phone", Some(&phone.cookie));
    let id = id_of_other(&e, u, &mine.token).await;
    assert_eq!(sign_out(&e, u, &mine.token, &id).await.status(), StatusCode::OK);
    assert_eq!(removed_told(u), 1, "the admins hear that a phone was taken off");
    assert!(endpoints(&e, u).is_empty());
    // Nothing removed, nothing said: a device with no notifications.
    let other = browser(&e, u, EDGE, "192.0.2.72", "").await;
    let id = id_of_other(&e, u, &mine.token).await;
    assert_eq!(sign_out(&e, u, &mine.token, &id).await.status(), StatusCode::OK);
    let _ = other;
    assert_eq!(removed_told(u), 1);
    // All others at once.
    let a = browser(&e, u, EDGE, "192.0.2.73", "").await;
    sub(&e, u, "https://push.example/a", Some(&a.cookie));
    let r = sessions_sign_out_others(State(e.clone()), Extension(Caller(u.into(), 0)), session_header(&mine.token), peer("192.0.2.7")).await;
    assert_eq!(r.status(), StatusCode::OK);
    // (Within the minute, a second one is gathered and sent at its end.)
    assert_eq!(removed_told(u), 1);
    flush_gathering(&e, "devices:so-gus:device removed");
    assert_eq!(removed_told(u), 2);
}

#[tokio::test]
async fn a_second_session_of_the_same_browser_does_not_forget_it() {
    let e = engine(&["so-eve"]);
    let _g = store(&e);
    let u = "so-eve";
    let mine = browser(&e, u, EDGE, "192.0.2.74", "").await;
    sub(&e, u, "https://push.example/mine", Some(&mine.cookie));
    // The same browser signs in again (its cookie came with it): another session, one browser.
    let again = browser(&e, u, EDGE, "192.0.2.74", &mine.cookie).await;
    assert_eq!(again.cookie, mine.cookie);
    let id = id_of_other(&e, u, &mine.token).await;
    assert_eq!(sign_out(&e, u, &mine.token, &id).await.status(), StatusCode::OK);
    assert_eq!(e.db.known_device_count(u), 1, "the browser in use is still known");
    assert_eq!(endpoints(&e, u), vec!["https://push.example/mine"]);
    // And with all others.
    browser(&e, u, EDGE, "192.0.2.74", &mine.cookie).await;
    let r = sessions_sign_out_others(State(e.clone()), Extension(Caller(u.into(), 0)), session_header(&mine.token), peer("192.0.2.7")).await;
    assert_eq!(r.status(), StatusCode::OK);
    assert_eq!(e.db.known_device_count(u), 1);
    assert_eq!(endpoints(&e, u), vec!["https://push.example/mine"]);
}

#[tokio::test]
async fn a_password_change_ends_the_notifications_of_the_sessions_it_ends_and_no_others_and_a_phone_signs_in_again() {
    let e = engine(&["so-fay"]);
    let _g = store(&e);
    let u = "so-fay";
    let mine = browser(&e, u, EDGE, "192.0.2.75", "").await;
    let live = browser(&e, u, SAFARI_IPHONE, "192.0.2.76", "").await;
    let gone = browser(&e, u, EDGE, "192.0.2.77", "").await;
    crate::auth::end_session(&gone.token);
    sub(&e, u, "https://push.example/mine", Some(&mine.cookie));
    sub(&e, u, "https://push.example/live", Some(&live.cookie));
    sub(&e, u, "https://push.example/nosession", Some(&gone.cookie));
    sub(&e, u, "https://push.example/old", None);
    assert!(crate::auth::password_changed(u, Some(&mine.token)).is_ok());
    assert_eq!(endpoints(&e, u), vec!["https://push.example/mine", "https://push.example/nosession", "https://push.example/old"], "only the session that ended lost its notifications");
    // The phone signs in with the new password and its page registers again.
    let phone = browser(&e, u, SAFARI_IPHONE, "192.0.2.76", &live.cookie).await;
    let body = {
        use base64::Engine as _;
        use p256::elliptic_curve::sec1::ToEncodedPoint as _;
        let k = p256::SecretKey::random(&mut rand::rngs::OsRng);
        let b = base64::engine::general_purpose::URL_SAFE_NO_PAD;
        serde_json::json!({ "subscription": { "endpoint": "https://fcm.googleapis.com/fcm/send/live", "keys": { "p256dh": b.encode(k.public_key().to_encoded_point(false).as_bytes()), "auth": b.encode([3u8; 16]) } } })
    };
    let mut req = post("/api/push/subscribe", body, SAFARI_IPHONE, &format!("as_session={}", phone.token), "192.0.2.76");
    req.extensions_mut().insert(Caller(u.into(), 0));
    req.extensions_mut().insert(IsAdmin(false));
    let r = crate::push_http::subscribe(State(e.clone()), Extension(Caller(u.into(), 0)), Extension(IsAdmin(false)), req).await;
    assert_eq!(r.status(), StatusCode::OK);
    assert!(endpoints(&e, u).contains(&"https://fcm.googleapis.com/fcm/send/live".to_string()));
    assert_eq!(e.db.push_session_device("https://fcm.googleapis.com/fcm/send/live"), Some(crate::devices::browser_hash(&live.cookie)), "tied to the new sign-in, so signing it out ends it again");
}

#[tokio::test]
async fn a_password_change_makes_a_direct_password_from_a_known_network_news_again() {
    let e = engine(&["so-gil"]);
    let _g = store(&e);
    let u = "so-gil";
    browser(&e, u, EDGE, "192.0.2.78", "").await;
    let direct = |e: &Arc<Engine>| e.db.audit_rows().iter().filter(|(x, a, _)| x == u && a == "password used directly from a new network").count();
    let wait_for = |e: Arc<Engine>, n: usize| async move {
        for _ in 0..100 {
            if e.db.audit_rows().iter().filter(|(x, a, _)| x == "so-gil" && a == "password used directly from a new network").count() >= n {
                return;
            }
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }
    };
    let net: std::net::IpAddr = "192.0.2.90".parse().unwrap();
    crate::devices::note_direct(&e, u, net);
    wait_for(e.clone(), 1).await;
    assert_eq!(direct(&e), 1);
    crate::devices::note_direct(&e, u, net);
    tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    assert_eq!(direct(&e), 1, "once per network");
    assert!(crate::auth::password_changed(u, None).is_ok());
    // Same network, new password, no restart.
    crate::devices::note_direct(&e, u, net);
    wait_for(e.clone(), 2).await;
    assert_eq!(direct(&e), 2);
}

#[tokio::test]
async fn signing_out_a_session_keeps_a_browser_that_another_live_session_has() {
    let e = engine(&["so-hal"]);
    let _g = store(&e);
    let u = "so-hal";
    let c = browser(&e, u, EDGE, "192.0.2.80", "").await;
    // Two sessions of one browser: A (older) and B.
    let a = browser(&e, u, SAFARI_IPHONE, "192.0.2.81", "").await;
    let b = browser(&e, u, SAFARI_IPHONE, "192.0.2.81", &a.cookie).await;
    assert_eq!(a.cookie, b.cookie);
    sub(&e, u, "https://push.example/phone", Some(&a.cookie));
    let acc = crate::auth::accounts(&e.config.lock().unwrap());
    // Signed out from a third device: the phone is still signed in by B.
    let list = sessions_json(&account_of(&e, u, &c.token).await);
    let ids: Vec<String> = list.iter().filter(|s| s["this"] == false).map(|s| s["id"].as_str().unwrap().to_string()).collect();
    assert_eq!(ids.len(), 2);
    assert_eq!(sign_out(&e, u, &c.token, &ids[0]).await.status(), StatusCode::OK);
    let live = [&a, &b].iter().filter(|x| crate::auth::session_identity(&x.token, &acc).is_some()).count();
    assert_eq!(live, 1);
    assert_eq!(e.db.known_device_count(u), 2, "the phone's browser is still known");
    assert_eq!(endpoints(&e, u), vec!["https://push.example/phone"], "and keeps its notifications");
    // The last session of that browser takes them with it.
    assert_eq!(sign_out(&e, u, &c.token, &ids[1]).await.status(), StatusCode::OK);
    assert_eq!(e.db.known_device_count(u), 1);
    assert!(endpoints(&e, u).is_empty());
}

#[tokio::test]
async fn a_password_change_ends_the_sessions_without_waiting_for_a_push_service() {
    let mut e = admin_engine("so-ivy");
    let path = std::env::temp_dir().join(format!("ambersight-pw-{}.toml", std::process::id()));
    Arc::get_mut(&mut e).unwrap().config_path = path.to_string_lossy().to_string();
    let _g = store(&e);
    let u = "so-ivy";
    let mine = browser(&e, u, EDGE, "192.0.2.82", "").await;
    let phone = browser(&e, u, SAFARI_IPHONE, "192.0.2.83", "").await;
    // A phone whose push service never answers.
    e.db.push_save(&crate::db::PushSub { endpoint: "https://192.0.2.254/push/dead".into(), user: u.into(), p256dh: "p".into(), auth: "a".into(), device: "d".into(), kind: "browser".into(), ..Default::default() }, 1).unwrap();
    e.db.push_set_session_device("https://192.0.2.254/push/dead", Some(&crate::devices::browser_hash(&phone.cookie))).unwrap();
    let mut req = post("/api/me/password", serde_json::json!({ "current": "pw-test-1", "new": "pw-new-long-1" }), EDGE, &format!("as_session={}", mine.token), "192.0.2.82");
    req.extensions_mut().insert(Caller(u.into(), crate::auth::generation(u)));
    let started = std::time::Instant::now();
    let r = crate::users_http::change_own_password(State(e.clone()), req).await;
    assert_eq!(r.status(), StatusCode::OK, "{}", text(r).await);
    assert!(started.elapsed() < std::time::Duration::from_secs(4), "{:?}", started.elapsed());
    let acc = crate::auth::accounts(&e.config.lock().unwrap());
    assert!(crate::auth::session_identity(&phone.token, &acc).is_none(), "ended in the request itself");
    assert!(crate::auth::session_identity(&mine.token, &acc).is_some());
    assert!(endpoints(&e, u).is_empty());
    let _ = std::fs::remove_file(&path);
}

// ---- demo sessions are never kept ----

#[test]
fn demo_sessions_never_reach_the_database() {
    let e = engine(&["dl-demo", "dl-norm"]);
    let _g = store(&e);
    let meta = || crate::auth::SessionMeta::new("10.0.0.9".parse().unwrap(), "Edge on Windows".into(), String::new());
    let demo = crate::auth::new_demo_session(crate::auth::Identity { user: "dl-demo".into(), admin: false }, crate::auth::generation("dl-demo"), meta()).unwrap();
    let normal = crate::auth::new_session_with(crate::auth::Identity { user: "dl-norm".into(), admin: false }, meta());
    crate::auth::session_seen(&demo, "10.0.0.10".parse().unwrap());
    let users: Vec<String> = e.db.load_sessions().unwrap().into_iter().map(|r| r.user).collect();
    // (An older version, or this one after a restart, finds nothing of it.)
    assert!(users.contains(&"dl-norm".to_string()) && !users.contains(&"dl-demo".to_string()), "{users:?}");
    crate::auth::end_session(&demo);
    crate::auth::end_session(&normal);
}
