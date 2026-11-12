//! Web API for browser and phone-app notifications (see push.rs). Every
//! signed-in user, viewers included, manages the notifications of their own
//! devices.

use axum::{
    extract::{Request, State},
    http::{header, StatusCode},
    response::{IntoResponse, Json, Response},
};
use sha2::Digest;
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use crate::engine::Engine;
use crate::web::{Caller, IsAdmin};

/// Most devices one account can have notifications on.
const MAX_DEVICES: usize = 25;

/// A short fingerprint of a push address, so a page can tell which listed
/// device is itself without the server echoing the address back.
fn key_of(endpoint: &str) -> String {
    sha2::Sha256::digest(endpoint.as_bytes()).iter().take(8).map(|b| format!("{b:02x}")).collect()
}

/// `GET /api/push/key`: the server's public key (the page hands it to the
/// browser when turning notifications on).
pub async fn key(State(engine): State<Arc<Engine>>) -> Response {
    match crate::push::keys(&engine.db) {
        Ok(k) => Json(serde_json::json!({ "key": k.public_b64() })).into_response(),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, format!("{e:#}")).into_response(),
    }
}

#[derive(serde::Deserialize)]
struct SubKeys {
    p256dh: String,
    auth: String,
}
#[derive(serde::Deserialize)]
struct Subscription {
    endpoint: String,
    keys: SubKeys,
}
#[derive(serde::Deserialize)]
struct SubscribeReq {
    subscription: Subscription,
    /// The name shown in the device list (apps send one; browsers get one
    /// from their User-Agent).
    #[serde(default)]
    device: Option<String>,
    /// "browser" (default) or "app" (a phone app through UnifiedPush).
    #[serde(default)]
    kind: Option<String>,
}

/// Unicode format characters that do not show (soft hyphen, zero-width
/// spaces and joiners, direction marks and overrides, word joiner and
/// invisible operators, isolates, byte order mark): a name holding them
/// can look like another device's or reorder the text around it.
pub(crate) fn invisible(c: char) -> bool {
    matches!(c, '\u{AD}' | '\u{61C}' | '\u{180E}' | '\u{200B}'..='\u{200F}' | '\u{202A}'..='\u{202E}' | '\u{2060}'..='\u{2064}' | '\u{2066}'..='\u{2069}' | '\u{FEFF}')
}

/// A device name as given: trimmed, at most 64 characters, no control or
/// invisible characters. Ok(None): none given.
fn device_name(given: Option<&str>) -> Result<Option<String>, &'static str> {
    let Some(d) = given.map(str::trim).filter(|d| !d.is_empty()) else { return Ok(None) };
    if d.chars().count() > 64 {
        return Err("the device name is too long (at most 64 characters)");
    }
    if d.chars().any(|c| c.is_control() || invisible(c)) {
        return Err("the device name must not hold control or invisible characters");
    }
    Ok(Some(d.to_string()))
}

/// What is wrong with a subscription request, before any lookup (Err: the
/// message for a 400). Ok: the kind and the device name if given.
fn check_request(r: &SubscribeReq, app_hosts: &[String]) -> Result<(&'static str, Option<String>), String> {
    let kind = match r.kind.as_deref().map(str::trim) {
        None | Some("") | Some("browser") => crate::push::KIND_BROWSER,
        Some("app") => crate::push::KIND_APP,
        Some(_) => return Err("kind must be \"browser\" or \"app\"".into()),
    };
    let device = device_name(r.device.as_deref()).map_err(String::from)?;
    let s = &r.subscription;
    if kind == crate::push::KIND_APP {
        crate::push::app_endpoint_rules(&s.endpoint, app_hosts)?;
    } else if !crate::push::endpoint_ok(&s.endpoint) {
        return Err("this browser's push service is not supported".into());
    }
    let pk_ok = crate::push::b64d(&s.keys.p256dh).ok().is_some_and(|b| p256::PublicKey::from_sec1_bytes(&b).is_ok() && b.len() == 65);
    let auth_ok = crate::push::b64d(&s.keys.auth).ok().is_some_and(|b| b.len() == 16);
    if !pk_ok || !auth_ok {
        return Err(if kind == crate::push::KIND_APP {
            "the app's keys are not valid (p256dh: a 65-byte P-256 public key, auth: 16 bytes, both base64url)".into()
        } else {
            "the browser's keys are not valid".into()
        });
    }
    Ok((kind, device))
}

/// Whether `caller` may register a push address now held by `owner`: a
/// new one or its own (keys and name updated), or as an admin.
fn may_register(owner: Option<&str>, caller: &str, admin: bool) -> bool {
    owner.is_none_or(|o| o == caller) || admin
}

/// `POST /api/push/subscribe {"subscription": <PushSubscription JSON>,
/// "kind"?: "browser"|"app", "device"?: name}`: this browser or phone app
/// gets notifications for the signed-in user.
pub async fn subscribe(
    State(engine): State<Arc<Engine>>,
    axum::Extension(caller): axum::Extension<Caller>,
    axum::Extension(admin): axum::Extension<IsAdmin>,
    req: Request,
) -> Response {
    let ip = crate::web::client_ip(&req);
    let ua = req.headers().get(header::USER_AGENT).and_then(|v| v.to_str().ok()).unwrap_or("").to_string();
    // Which sign-in this is from: signing that device out takes the address off the list.
    let session_device = crate::auth::session_device_from_headers(req.headers(), &crate::auth::accounts(&engine.config.lock().unwrap()));
    let Ok(bytes) = axum::body::to_bytes(req.into_body(), 8 * 1024).await else {
        return (StatusCode::PAYLOAD_TOO_LARGE, "too large").into_response();
    };
    let Ok(r) = serde_json::from_slice::<SubscribeReq>(&bytes) else {
        return (StatusCode::BAD_REQUEST, "expected {\"subscription\": {\"endpoint\", \"keys\": {\"p256dh\", \"auth\"}}}").into_response();
    };
    let app_hosts = engine.config.lock().unwrap().push.app_hosts.clone();
    let (kind, device) = match check_request(&r, &app_hosts) {
        Ok(v) => v,
        Err(e) => return (StatusCode::BAD_REQUEST, e).into_response(),
    };
    let s = r.subscription;
    if kind == crate::push::KIND_APP {
        // Looked up now too, so a wrong address is said at once (it is
        // checked again at every send).
        let (ep, hosts) = (s.endpoint.clone(), app_hosts);
        match tokio::task::spawn_blocking(move || crate::push::app_target(&ep, &hosts)).await {
            Ok(Ok(_)) => {}
            Ok(Err(e)) => return (StatusCode::BAD_REQUEST, e).into_response(),
            Err(_) => return (StatusCode::INTERNAL_SERVER_ERROR, "lookup failed").into_response(),
        }
    }
    // A push address already registered to another account is not taken
    // over by a non-admin (who could otherwise silence someone's phone, or
    // read their alerts if the keys were known). Admins may move it.
    let owner = match engine.db.push_owner(&s.endpoint) {
        Ok(o) => o,
        Err(e) => return (StatusCode::INTERNAL_SERVER_ERROR, format!("{e:#}")).into_response(),
    };
    if !may_register(owner.as_deref(), &caller.0, admin.0) {
        return (StatusCode::CONFLICT, "this push address belongs to another account").into_response();
    }
    let mine = engine.db.push_list(Some(&caller.0)).unwrap_or_default();
    if mine.len() >= MAX_DEVICES && !mine.iter().any(|m| m.endpoint == s.endpoint) {
        return (StatusCode::BAD_REQUEST, "too many devices; remove one first").into_response();
    }
    let device = device.unwrap_or_else(|| if kind == crate::push::KIND_APP { "Phone app".into() } else { crate::push::device_name(&ua) });
    let sub = crate::db::PushSub {
        endpoint: s.endpoint.clone(),
        user: caller.0.clone(),
        p256dh: s.keys.p256dh.trim().to_string(),
        auth: s.keys.auth.trim().to_string(),
        device,
        kind: kind.to_string(),
        ..Default::default()
    };
    let saved = engine.db.push_save(&sub, chrono::Utc::now().timestamp_millis());
    if saved.is_ok() {
        if let Err(e) = engine.db.push_set_session_device(&sub.endpoint, session_device.as_deref()) {
            tracing::warn!("could not note which sign-in a notification address is from: {e:#}");
        }
    }
    // Where alerts go has changed: admins hear of a new device of an admin (not of
    // one that only renews itself). It takes no password: this is a page's own button.
    if saved.is_ok() && owner.as_deref() != Some(caller.0.as_str()) && account_is_admin(&engine, &caller.0) {
        crate::account_http::tell_device_change(&engine, &caller.0, &sub.device, ip, true);
    }
    match saved {
        Ok(()) => Json(serde_json::json!({ "ok": true, "key": key_of(&s.endpoint), "device": sub.device, "kind": kind })).into_response(),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, format!("{e:#}")).into_response(),
    }
}

#[derive(serde::Deserialize)]
pub struct UnsubscribeReq {
    endpoint: Option<String>,
    id: Option<i64>,
}

/// Whether this account gets the admins' alerts (not: this caller's rights now).
pub(crate) fn account_is_admin(engine: &Engine, user: &str) -> bool {
    crate::auth::accounts(&engine.config.lock().unwrap()).iter().any(|(u, _, a)| u == user && *a)
}

/// `POST /api/push/unsubscribe {"endpoint"}` or `{"id"}`: stop
/// notifications on one of the caller's own devices. An admin's device hears of
/// it first, and the admins after: a stolen session must not go quiet unseen.
pub async fn unsubscribe(State(engine): State<Arc<Engine>>, axum::Extension(caller): axum::Extension<Caller>, axum::extract::ConnectInfo(peer): axum::extract::ConnectInfo<std::net::SocketAddr>, Json(r): Json<UnsubscribeReq>) -> Response {
    if r.endpoint.is_none() && r.id.is_none() {
        return (StatusCode::BAD_REQUEST, "endpoint or id needed").into_response();
    }
    let going: Vec<crate::db::PushSub> = if account_is_admin(&engine, &caller.0) {
        engine.db.push_list(Some(&caller.0)).unwrap_or_default().into_iter().filter(|s| r.endpoint.as_deref() == Some(s.endpoint.as_str()) || r.id == Some(s.id)).collect()
    } else {
        Vec::new()
    };
    crate::account_http::tell_devices_first(&engine, going.clone(), format!("device removed for {}", caller.0), format!("{} removed this device from notifications (from {}). If that was not meant to happen, act now: this is the last alert on this device.", caller.0, peer.ip())).await;
    match engine.db.push_delete(r.endpoint.as_deref(), r.id, Some(&caller.0)) {
        Ok(removed) => {
            for s in &going {
                crate::account_http::tell_device_change(&engine, &caller.0, &s.device, peer.ip(), false);
            }
            Json(serde_json::json!({ "removed": removed })).into_response()
        }
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, format!("{e:#}")).into_response(),
    }
}

/// `GET /api/push/devices`: the caller's devices with notifications on.
pub async fn devices(State(engine): State<Arc<Engine>>, axum::Extension(caller): axum::Extension<Caller>) -> Response {
    let list = engine.db.push_list(Some(&caller.0)).unwrap_or_default();
    let hosts = engine.config.lock().unwrap().push.app_hosts.clone();
    Json(serde_json::json!({
        "devices": list.iter().map(|s| {
            let app = s.kind == crate::push::KIND_APP;
            serde_json::json!({
                "id": s.id,
                "device": s.device,
                "kind": if app { crate::push::KIND_APP } else { crate::push::KIND_BROWSER },
                "key": key_of(&s.endpoint),
                "added_ms": s.created_ms,
                "last_ok_ms": s.last_ok_ms,
                // Apps: their push server, and whether it is still allowed.
                "server": if app { url::Url::parse(&s.endpoint).ok().and_then(|u| u.host_str().map(|h| match u.port() { Some(p) => format!("{h}:{p}"), None => h.to_string() })) } else { None },
                "allowed": !app || crate::push::app_endpoint_rules(&s.endpoint, &hosts).is_ok(),
            })
        }).collect::<Vec<_>>(),
    }))
    .into_response()
}

/// Least time between two test notifications from one account.
const TEST_COOLDOWN: Duration = Duration::from_secs(10);
static LAST_TEST: Mutex<Option<HashMap<String, Instant>>> = Mutex::new(None);

/// Whether `user` may send a test now (and if so, the time is noted).
/// Err: the seconds to wait.
fn test_allowed(last: &mut HashMap<String, Instant>, user: &str, now: Instant) -> Result<(), u64> {
    if let Some(t) = last.get(user) {
        let since = now.saturating_duration_since(*t);
        if since < TEST_COOLDOWN {
            return Err((TEST_COOLDOWN - since).as_secs_f64().ceil().max(1.0) as u64);
        }
    }
    last.retain(|_, t| now.saturating_duration_since(*t) < TEST_COOLDOWN);
    last.insert(user.to_string(), now);
    Ok(())
}

/// `POST /api/push/test`: a test notification to the caller's devices (at
/// most one every 10 seconds per account; 429 with Retry-After otherwise).
pub async fn test(State(engine): State<Arc<Engine>>, axum::Extension(caller): axum::Extension<Caller>) -> Response {
    let wait = {
        let mut g = LAST_TEST.lock().unwrap_or_else(|e| e.into_inner());
        test_allowed(g.get_or_insert_with(HashMap::new), &caller.0, Instant::now())
    };
    if let Err(secs) = wait {
        return (
            StatusCode::TOO_MANY_REQUESTS,
            [(header::RETRY_AFTER, secs.to_string())],
            format!("a test notification was just sent; try again in {secs} seconds"),
        )
            .into_response();
    }
    let subs = engine.db.push_list(Some(&caller.0)).unwrap_or_default();
    if subs.is_empty() {
        return Json(serde_json::json!({ "devices": 0, "sent": 0, "failed": 0, "removed": 0 })).into_response();
    }
    let n = subs.len();
    let site = crate::links::own_name(&engine);
    let eng = engine.clone();
    let r = tokio::task::spawn_blocking(move || {
        let m = crate::push::Message {
            title: format!("{site}: test"),
            body: "Notifications work on this device.".into(),
            url: "/".into(),
            tag: "test".into(),
            kind: crate::push::MsgType::Test,
            ..Default::default()
        };
        crate::push::send_to(&eng, &subs, &m)
    })
    .await
    .unwrap_or_default();
    Json(serde_json::json!({ "devices": n, "sent": r.sent, "failed": r.failed, "removed": r.removed })).into_response()
}

#[cfg(test)]
mod tests {
    use super::*;
    use base64::Engine as _;
    use p256::elliptic_curve::sec1::ToEncodedPoint;

    fn req(endpoint: &str, kind: Option<&str>, device: Option<&str>) -> SubscribeReq {
        let k = p256::SecretKey::random(&mut rand::rngs::OsRng);
        let b = base64::engine::general_purpose::URL_SAFE_NO_PAD;
        SubscribeReq {
            subscription: Subscription {
                endpoint: endpoint.into(),
                keys: SubKeys { p256dh: b.encode(k.public_key().to_encoded_point(false).as_bytes()), auth: b.encode([3u8; 16]) },
            },
            device: device.map(String::from),
            kind: kind.map(String::from),
        }
    }

    #[test]
    fn app_subscriptions_are_checked() {
        let hosts = vec!["ntfy.sh".to_string(), "ntfy.home.lan:8080".to_string()];
        assert_eq!(check_request(&req("https://ntfy.sh/upX?up=1", Some("app"), Some(" Pixel 8 ")), &hosts), Ok(("app", Some("Pixel 8".into()))));
        assert_eq!(check_request(&req("http://ntfy.home.lan:8080/upX", Some("app"), None), &hosts), Ok(("app", None)));
        let e = check_request(&req("https://push.example.org/upX", Some("app"), None), &hosts).unwrap_err();
        assert_eq!(e, "this push server (push.example.org) is not allowed on this AmberSight; an admin can add it under Settings, Notifications, Browser and phone push");
        assert!(check_request(&req("http://ntfy.sh/upX", Some("app"), None), &hosts).unwrap_err().contains("https://"));
        // Browsers keep their own rule (and ntfy is not a browser's service).
        assert_eq!(check_request(&req("https://fcm.googleapis.com/fcm/send/a", None, None), &hosts), Ok(("browser", None)));
        assert!(check_request(&req("https://ntfy.sh/upX", Some("browser"), None), &hosts).is_err());
        assert!(check_request(&req("https://ntfy.sh/upX", Some("phone"), None), &hosts).unwrap_err().contains("kind must be"));
        // Device names.
        assert!(check_request(&req("https://ntfy.sh/upX", Some("app"), Some(&"x".repeat(65))), &hosts).unwrap_err().contains("too long"));
        assert!(check_request(&req("https://ntfy.sh/upX", Some("app"), Some(&"é".repeat(64))), &hosts).is_ok());
        assert!(check_request(&req("https://ntfy.sh/upX", Some("app"), Some("a\u{7}b")), &hosts).unwrap_err().contains("control"));
        for c in ['\u{AD}', '\u{61C}', '\u{180E}', '\u{200B}', '\u{200D}', '\u{200F}', '\u{202A}', '\u{202E}', '\u{2060}', '\u{2064}', '\u{2066}', '\u{2069}', '\u{FEFF}'] {
            let name = format!("Pixel{c}8");
            for kind in [Some("app"), None] {
                let ep = if kind.is_some() { "https://ntfy.sh/upX" } else { "https://fcm.googleapis.com/fcm/send/a" };
                assert_eq!(check_request(&req(ep, kind, Some(&name)), &hosts).unwrap_err(), "the device name must not hold control or invisible characters", "{:04X}", c as u32);
            }
        }
        // Visible non-ASCII is fine (and the neighbours of the ranges).
        for ok in ["Ann’s Pixel", "Telefon für Oma", "手机", "a\u{2010}b", "a\u{2065}b", "a\u{2070}b"] {
            assert!(check_request(&req("https://ntfy.sh/upX", Some("app"), Some(ok)), &hosts).is_ok(), "{ok}");
        }
        assert_eq!(check_request(&req("https://ntfy.sh/upX", Some("app"), Some("  ")), &hosts), Ok(("app", None)));
        // Keys.
        let mut r = req("https://ntfy.sh/upX", Some("app"), None);
        r.subscription.keys.auth = "AAAA".into();
        assert!(check_request(&r, &hosts).unwrap_err().contains("app's keys"));
    }

    #[test]
    fn a_push_address_stays_with_its_account() {
        assert!(may_register(None, "ann", false));
        assert!(may_register(Some("ann"), "ann", false));
        assert!(!may_register(Some("bob"), "ann", false));
        assert!(may_register(Some("bob"), "ann", true));
        // And in the database: the owner is the account that registered it.
        let db = crate::db::Db::open_in_memory().unwrap();
        let sub = crate::db::PushSub { endpoint: "https://ntfy.sh/upX".into(), user: "bob".into(), p256dh: "k".into(), auth: "a".into(), device: "Pixel".into(), kind: "app".into(), ..Default::default() };
        assert_eq!(db.push_owner(&sub.endpoint).unwrap(), None);
        db.push_save(&sub, 1).unwrap();
        assert_eq!(db.push_owner(&sub.endpoint).unwrap().as_deref(), Some("bob"));
        assert_eq!(db.push_owner("https://ntfy.sh/other").unwrap(), None);
    }

    #[test]
    fn test_notifications_wait_ten_seconds() {
        let mut last = HashMap::new();
        let t0 = Instant::now();
        assert_eq!(test_allowed(&mut last, "ann", t0), Ok(()));
        assert_eq!(test_allowed(&mut last, "ann", t0 + Duration::from_millis(500)), Err(10));
        assert_eq!(test_allowed(&mut last, "ann", t0 + Duration::from_secs(7)), Err(3));
        // Another account is not held up.
        assert_eq!(test_allowed(&mut last, "bob", t0 + Duration::from_secs(1)), Ok(()));
        assert_eq!(test_allowed(&mut last, "ann", t0 + Duration::from_secs(10)), Ok(()));
        // A refused try does not restart the wait.
        assert_eq!(test_allowed(&mut last, "ann", t0 + Duration::from_secs(19)), Err(1));
        assert_eq!(test_allowed(&mut last, "ann", t0 + Duration::from_secs(20)), Ok(()));
    }

    #[test]
    fn the_body_an_app_sends_parses() {
        let r: SubscribeReq = serde_json::from_str(r#"{"subscription":{"endpoint":"https://ntfy.sh/up1?up=1","keys":{"p256dh":"x","auth":"y"}},"kind":"app","device":"Pixel"}"#).unwrap();
        assert_eq!((r.kind.as_deref(), r.device.as_deref()), (Some("app"), Some("Pixel")));
        let r: SubscribeReq = serde_json::from_str(r#"{"subscription":{"endpoint":"e","keys":{"p256dh":"x","auth":"y"}}}"#).unwrap();
        assert!(r.kind.is_none() && r.device.is_none());
    }
}
