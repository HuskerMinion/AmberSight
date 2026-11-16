//! Display mode sign-in (display.rs) through the whole router, as a TV
//! browser and an admin reach it: the code, approval, what a display session
//! may do, renewal, expiry and signing out.

use axum::body::Body;
use axum::http::{header, Request, StatusCode};
use base64::Engine as _;
use tower::ServiceExt;

/// Cameras Porch and Garage; an admin, a viewer limited to Porch, a viewer of
/// all cameras and an operator, each name starting with `tag` (sign-in state is global).
fn site(tag: &str) -> (std::sync::Arc<crate::engine::Engine>, axum::Router) {
    let mut text = "[[cameras]]\nname = \"Porch\"\nurl = \"rtsp://x/\"\n\n[[cameras]]\nname = \"Garage\"\nurl = \"rtsp://y/\"\n".to_string();
    for (name, extra) in [("boss", "admin = true"), ("tv", "admin = false\ncameras = [\"Porch\"]"), ("all", "admin = false"), ("op", "admin = false\nrole = \"operator\"")] {
        text += &format!("\n[[server.users]]\nusername = \"{tag}{name}\"\npassword = \"{}\"\n{extra}\n", crate::auth::hash_password(&format!("pw-{tag}{name}-1")));
    }
    let mut e = crate::engine::Engine::for_test(toml::from_str(&text).unwrap());
    std::sync::Arc::get_mut(&mut e).unwrap().config_path = std::env::temp_dir().join(format!("ambersight-display-{tag}-{}.toml", std::process::id())).to_string_lossy().to_string();
    let app = crate::web::build_router(e.clone());
    (e, app)
}

#[derive(Default)]
struct Call<'a> {
    method: &'a str,
    path: &'a str,
    ip: [u8; 4],
    /// Basic sign-in as this user (password pw-<user>-1).
    basic: Option<String>,
    /// The Cookie header.
    cookie: Option<String>,
    origin: Option<&'a str>,
    body: Option<String>,
}

fn call<'a>(method: &'a str, path: &'a str, ip: [u8; 4]) -> Call<'a> {
    Call { method, path, ip, ..Default::default() }
}

struct Got {
    status: StatusCode,
    body: String,
    cookies: Vec<String>,
}

impl Got {
    fn json(&self) -> serde_json::Value {
        serde_json::from_str(&self.body).unwrap_or_else(|e| panic!("{e}: {}", self.body))
    }
    /// A Set-Cookie line of this name (the whole line).
    fn cookie(&self, name: &str) -> Option<&str> {
        self.cookies.iter().find(|c| c.starts_with(&format!("{name}="))).map(String::as_str)
    }
}

/// The value of a Set-Cookie line.
fn value(set_cookie: &str) -> String {
    set_cookie.split_once('=').unwrap().1.split(';').next().unwrap().to_string()
}

const TV_UA: &str = "Mozilla/5.0 (Linux; Android 9; AFTMM) AppleWebKit/537.36 (KHTML, like Gecko) Silk/120.1.1 like Chrome/120.0.0.0 Safari/537.36";

async fn send(app: &axum::Router, c: Call<'_>) -> Got {
    let mut b = Request::builder().method(c.method).uri(c.path).header(header::HOST, "localhost:8090").header(header::USER_AGENT, TV_UA);
    if let Some(u) = &c.basic {
        b = b.header(header::AUTHORIZATION, format!("Basic {}", base64::engine::general_purpose::STANDARD.encode(format!("{u}:pw-{u}-1"))));
    }
    if let Some(k) = &c.cookie {
        b = b.header(header::COOKIE, k);
    }
    if let Some(o) = c.origin {
        b = b.header(header::ORIGIN, o);
    }
    let body = match &c.body {
        Some(j) => {
            b = b.header(header::CONTENT_TYPE, "application/json");
            Body::from(j.clone())
        }
        None => Body::empty(),
    };
    let mut req = b.body(body).unwrap();
    req.extensions_mut().insert(axum::extract::ConnectInfo(std::net::SocketAddr::from((c.ip, 5000))));
    let r = app.clone().oneshot(req).await.unwrap();
    let status = r.status();
    let cookies = r.headers().get_all(header::SET_COOKIE).iter().filter_map(|v| v.to_str().ok()).map(str::to_string).collect();
    let body = String::from_utf8_lossy(&axum::body::to_bytes(r.into_body(), 1 << 20).await.unwrap()).into_owned();
    Got { status, body, cookies }
}

/// A display asks for a code: (code, the wait cookie for the Cookie header).
async fn start(app: &axum::Router, ip: [u8; 4]) -> (String, String) {
    let g = send(app, call("POST", "/api/display/start", ip)).await;
    assert_eq!(g.status, StatusCode::OK, "{}", g.body);
    let c = g.cookie("as_display_wait").expect("a wait cookie").to_string();
    assert!(c.contains("HttpOnly") && c.contains("SameSite=Strict") && c.contains("Path=/api/display") && c.contains("Max-Age=600"), "{c}");
    let code = g.json()["code"].as_str().unwrap().to_string();
    assert_eq!(code.len(), 9, "{code}");
    (code, format!("as_display_wait={}", value(&c)))
}

async fn approve(app: &axum::Router, admin: &str, code: &str, user: &str, name: &str) -> Got {
    send(app, Call { basic: Some(admin.into()), body: Some(serde_json::json!({ "code": code, "user": user, "name": name }).to_string()), ..call("POST", "/api/display/approve", [10, 9, 0, 1]) }).await
}

/// Pair a display as `user`: its session token, and the Cookie header of its
/// browser (the session and the device cookie).
async fn paired(app: &axum::Router, tag: &str, user: &str, ip: [u8; 4]) -> (String, String) {
    let (code, wait) = start(app, ip).await;
    assert_eq!(approve(app, &format!("{tag}boss"), &code, user, "Hall TV").await.status, StatusCode::OK);
    let g = send(app, Call { cookie: Some(wait), ..call("POST", "/api/display/wait", ip) }).await;
    assert_eq!(g.status, StatusCode::OK, "{}", g.body);
    let tok = value(g.cookie("as_session").expect("a session"));
    let dev = value(g.cookie("as_device").expect("a device cookie"));
    (tok.clone(), format!("as_session={tok}; as_device={dev}"))
}

fn as_display(token: &str) -> Option<String> {
    Some(format!("as_session={token}"))
}

#[tokio::test]
async fn a_display_signs_in_with_an_approved_code_and_only_watches() {
    let (e, app) = site("dp1");
    let ip = [10, 61, 0, 1];
    let (code, wait) = start(&app, ip).await;
    // Waiting: the code comes back; without the cookie (another browser) there is nothing.
    let g = send(&app, Call { cookie: Some(wait.clone()), ..call("POST", "/api/display/wait", ip) }).await;
    assert_eq!((g.status, g.json()["pending"].as_bool(), g.json()["code"].as_str()), (StatusCode::OK, Some(true), Some(code.as_str())));
    assert_eq!(send(&app, call("POST", "/api/display/wait", ip)).await.status, StatusCode::GONE);
    let other = format!("as_display_wait={}", "a".repeat(64));
    assert_eq!(send(&app, Call { cookie: Some(other), ..call("POST", "/api/display/wait", ip) }).await.status, StatusCode::GONE);
    // Another site's page cannot ask for a code or pick up a session (as for every sign-in).
    for p in ["/api/display/start", "/api/display/wait"] {
        let g = send(&app, Call { origin: Some("http://evil.example"), cookie: Some(wait.clone()), ..call("POST", p, ip) }).await;
        assert_eq!((g.status, g.cookie("as_session").is_none()), (StatusCode::FORBIDDEN, true), "{p}");
    }
    // The admin looks it up: where it came from.
    let g = send(&app, Call { basic: Some("dp1boss".into()), body: Some(serde_json::json!({ "code": code.to_lowercase() }).to_string()), ..call("POST", "/api/display/lookup", [10, 9, 0, 1]) }).await;
    assert_eq!(g.status, StatusCode::OK, "{}", g.body);
    assert_eq!((g.json()["ip"].as_str(), g.json()["browser"].as_str()), (Some("10.61.0.1"), Some("Chrome on Android")));
    // Only admins approve, and only for a viewer account.
    let g = send(&app, Call { basic: Some("dp1all".into()), body: Some(serde_json::json!({ "code": code, "user": "dp1all" }).to_string()), ..call("POST", "/api/display/approve", [10, 9, 0, 2]) }).await;
    assert_eq!(g.status, StatusCode::FORBIDDEN, "a viewer: {}", g.body);
    let g = send(&app, Call { basic: Some("dp1op".into()), body: Some(serde_json::json!({ "code": code, "user": "dp1tv" }).to_string()), ..call("POST", "/api/display/approve", [10, 9, 0, 3]) }).await;
    assert_eq!(g.status, StatusCode::FORBIDDEN, "an operator: {}", g.body);
    for target in ["dp1boss", "dp1op", "nobody"] {
        let g = approve(&app, "dp1boss", &code, target, "x").await;
        assert_eq!(g.status, StatusCode::BAD_REQUEST, "{target}: {}", g.body);
    }
    let g = approve(&app, "dp1boss", "ZZZZ-ZZZZ", "dp1tv", "x").await;
    assert_eq!(g.status, StatusCode::NOT_FOUND);
    let g = approve(&app, "dp1boss", &code, "dp1tv", "Hall TV").await;
    assert_eq!(g.status, StatusCode::OK, "{}", g.body);
    assert_eq!(approve(&app, "dp1boss", &code, "dp1tv", "again").await.status, StatusCode::NOT_FOUND, "once");
    // The display's next check signs it in: a session cookie for the idle time, the wait cookie cleared.
    let g = send(&app, Call { cookie: Some(wait.clone()), ..call("POST", "/api/display/wait", ip) }).await;
    assert_eq!((g.status, g.json()["signed_in"].as_bool()), (StatusCode::OK, Some(true)), "{}", g.body);
    let sc = g.cookie("as_session").unwrap();
    assert!(sc.contains("HttpOnly") && sc.contains("SameSite=Strict") && sc.contains(&format!("Max-Age={}", crate::auth::DISPLAY_IDLE_SECS)), "{sc}");
    assert!(g.cookie("as_display_wait").unwrap().contains("Max-Age=0"));
    let tok = value(sc);
    assert_eq!(send(&app, Call { cookie: Some(wait), ..call("POST", "/api/display/wait", ip) }).await.status, StatusCode::GONE, "used up");
    // What it is: a display, a viewer, never an admin.
    let w = send(&app, Call { cookie: as_display(&tok), ..call("GET", "/api/whoami", ip) }).await.json();
    assert_eq!((w["display"].as_bool(), w["role"].as_str(), w["admin"].as_bool(), w["user"].as_str()), (Some(true), Some("viewer"), Some(false), Some("dp1tv")), "{w}");
    // What the display page uses works; a camera of another account's is not there.
    for p in ["/api/cameras", "/api/status", "/api/ui/prefs", "/api/ui/orders"] {
        assert_eq!(send(&app, Call { cookie: as_display(&tok), ..call("GET", p, ip) }).await.status, StatusCode::OK, "{p}");
    }
    for p in ["/api/snapshot/7", "/api/live/7/info", "/api/live/7/jpeg"] {
        assert_eq!(send(&app, Call { cookie: as_display(&tok), ..call("GET", p, ip) }).await.status, StatusCode::NOT_FOUND, "{p}: allowed, no such camera");
    }
    // The web app's address sends a display to display mode (it would only half work there).
    let g = send(&app, Call { cookie: as_display(&tok), ..call("GET", "/", ip) }).await;
    assert_eq!(g.status, StatusCode::FOUND);
    let g = send(&app, call("GET", "/", ip)).await;
    assert_eq!(g.status, StatusCode::OK, "not signed in: the web app");
    // Nothing recorded, nothing about the account, no changes (anyone passing can use it).
    for (m, p, body) in [
        ("GET", "/api/events?camera=Porch", ""),
        ("GET", "/api/events/porch-1/thumb", ""),
        ("GET", "/api/clips/porch-1", ""),
        ("GET", "/api/recordings?camera=Porch", ""),
        ("GET", "/api/playback?camera=Porch&from=1700000000000&to=1700000060000", ""),
        ("GET", "/api/timeline/events?since=0&until=1", ""),
        ("GET", "/api/timelapse", ""),
        ("GET", "/api/smart/search?q=car", ""),
        ("GET", "/api/plates", ""),
        ("GET", "/api/bookmarks", ""),
        ("POST", "/api/bookmarks", r#"{"camera":"Porch","at":1700000000000,"note":"x"}"#),
        ("GET", "/api/export", ""),
        ("GET", "/api/stats", ""),
        ("GET", "/api/links", ""),
        ("POST", "/api/me/tokens", r#"{"name":"x"}"#),
        ("POST", "/api/me/password", r#"{"current":"pw-dp1tv-1","new":"another-long-one"}"#),
        ("GET", "/api/me/account", ""),
        ("POST", "/api/me/sessions/sign-out-others", ""),
        ("PATCH", "/api/ui/prefs", "{}"),
        ("POST", "/api/push/subscribe", "{}"),
        ("POST", "/api/clips/record", r#"{"camera":0,"seconds":10}"#),
        ("POST", "/api/display/approve", r#"{"code":"x","user":"dp1tv"}"#),
        ("GET", "/api/display/sessions", ""),
    ] {
        let g = send(&app, Call { cookie: as_display(&tok), body: (!body.is_empty()).then(|| body.to_string()), ..call(m, p, ip) }).await;
        assert_eq!(g.status, StatusCode::FORBIDDEN, "{m} {p}: {}", g.body);
    }
    // (The same account signed in with its password may: only the display is held to live.)
    assert_ne!(send(&app, Call { basic: Some("dp1tv".into()), ..call("GET", "/api/events?camera=Porch", [10, 9, 0, 5]) }).await.status, StatusCode::FORBIDDEN);
    // The account's own device list shows it as a display.
    let acct = send(&app, Call { basic: Some("dp1tv".into()), ..call("GET", "/api/me/account", [10, 9, 0, 4]) }).await.json();
    assert!(acct["sessions"].as_array().unwrap().iter().any(|s| s["label"].as_str().unwrap() == "Display: Hall TV (Chrome on Android)"), "{acct}");
    // Admins see it in the list of displays, and the audit log has the pairing.
    let list = send(&app, Call { basic: Some("dp1boss".into()), ..call("GET", "/api/display/sessions", [10, 9, 0, 1]) }).await.json();
    let row = list["displays"].as_array().unwrap().iter().find(|d| d["user"] == "dp1tv").cloned().expect("listed");
    assert_eq!(row["label"].as_str(), Some("Hall TV (Chrome on Android)"));
    let rows = e.db.audit_rows();
    assert!(rows.iter().any(|r| r.1.contains("display paired")) && rows.iter().any(|r| r.1.contains("sign-in (display code)")), "{rows:?}");
    // Signed out by the admin: nothing works any more.
    let id = row["id"].as_str().unwrap();
    assert_eq!(send(&app, Call { basic: Some("dp1all".into()), ..call("POST", &format!("/api/display/sessions/{id}/sign-out"), [10, 9, 0, 2]) }).await.status, StatusCode::FORBIDDEN, "not by a viewer");
    let g = send(&app, Call { basic: Some("dp1boss".into()), ..call("POST", &format!("/api/display/sessions/{id}/sign-out"), [10, 9, 0, 1]) }).await;
    assert_eq!(g.status, StatusCode::OK, "{}", g.body);
    assert_eq!(send(&app, Call { cookie: as_display(&tok), ..call("GET", "/api/cameras", ip) }).await.status, StatusCode::UNAUTHORIZED);
    let w = send(&app, Call { cookie: as_display(&tok), ..call("GET", "/api/whoami", ip) }).await.json();
    assert_eq!(w["signed_in"].as_bool(), Some(false));
    assert_eq!(send(&app, Call { basic: Some("dp1boss".into()), ..call("POST", &format!("/api/display/sessions/{id}/sign-out"), [10, 9, 0, 1]) }).await.status, StatusCode::NOT_FOUND, "gone");
}

#[tokio::test]
async fn a_display_session_renews_while_used_and_ends_when_unused() {
    let (_e, app) = site("dp2");
    let (tok, browser) = paired(&app, "dp2", "dp2tv", [10, 62, 0, 1]).await;
    let now = chrono::Utc::now().timestamp();
    // Nearly run out: only its own browser renews it (the session cookie alone, copied elsewhere, does not).
    crate::auth::session_expire_at(&tok, now + 100);
    for other in [as_display(&tok).unwrap(), format!("as_session={tok}; as_device={}", "b".repeat(64))] {
        let g = send(&app, Call { cookie: Some(other), ..call("POST", "/api/display/renew", [10, 62, 0, 1]) }).await;
        assert_eq!((g.status, g.json()["renewed"].as_bool(), g.cookie("as_session").is_none()), (StatusCode::OK, Some(false), true), "{}", g.body);
    }
    assert_eq!(crate::auth::session_expires(&tok), Some(now + 100));
    // From its browser: the full idle time again, and the cookie again.
    let g = send(&app, Call { cookie: Some(browser.clone()), ..call("POST", "/api/display/renew", [10, 62, 0, 1]) }).await;
    assert_eq!((g.status, g.json()["display"].as_bool(), g.json()["renewed"].as_bool()), (StatusCode::OK, Some(true), Some(true)), "{}", g.body);
    assert_eq!(value(g.cookie("as_session").unwrap()), tok);
    let left = crate::auth::session_expires(&tok).unwrap() - now;
    assert!((crate::auth::DISPLAY_IDLE_SECS - 5..=crate::auth::DISPLAY_IDLE_SECS + 5).contains(&left), "{left}");
    let pair_by = g.json()["pair_by"].as_i64().unwrap();
    assert!((pair_by - now - crate::auth::DISPLAY_MAX_SECS).abs() < 5, "{pair_by}");
    // Close to six months after pairing: never past them; past them, it is over.
    crate::auth::session_created_at(&tok, now - crate::auth::DISPLAY_MAX_SECS + 3600);
    let g = send(&app, Call { cookie: Some(browser.clone()), ..call("POST", "/api/display/renew", [10, 62, 0, 1]) }).await;
    assert_eq!(g.json()["renewed"].as_bool(), Some(true));
    let left = crate::auth::session_expires(&tok).unwrap() - now;
    assert!(left <= 3600 && left > 3500, "capped: {left}");
    assert!(g.cookie("as_session").unwrap().contains("Max-Age=3"), "the cookie ends with it");
    // Unused for the idle time: ended, and a renewal cannot bring it back.
    crate::auth::session_expire_at(&tok, now - 1);
    assert_eq!(send(&app, Call { cookie: as_display(&tok), ..call("GET", "/api/cameras", [10, 62, 0, 1]) }).await.status, StatusCode::UNAUTHORIZED);
    let g = send(&app, Call { cookie: Some(browser), ..call("POST", "/api/display/renew", [10, 62, 0, 1]) }).await;
    assert_eq!(g.status, StatusCode::UNAUTHORIZED);
    assert!(g.cookie("as_session").is_none());
    // An ordinary session does not renew (it ends 30 days after sign-in, as before).
    let g = send(&app, Call { body: Some(r#"{"username":"dp2all","password":"pw-dp2all-1"}"#.into()), ..call("POST", "/api/login", [10, 62, 0, 2]) }).await;
    let plain = value(g.cookie("as_session").unwrap());
    let before = crate::auth::session_expires(&plain).unwrap();
    let g = send(&app, Call { cookie: as_display(&plain), ..call("POST", "/api/display/renew", [10, 62, 0, 2]) }).await;
    assert_eq!((g.status, g.json()["display"].as_bool(), g.cookie("as_session").is_none()), (StatusCode::OK, Some(false), true));
    assert_eq!(crate::auth::session_expires(&plain), Some(before));
    // Nor a session sent by another name: only the display session's own cookie counts.
    let w = send(&app, Call { cookie: as_display(&plain), ..call("GET", "/api/whoami", [10, 62, 0, 2]) }).await.json();
    assert_eq!(w["display"].as_bool(), Some(false));
}

#[tokio::test]
async fn an_account_change_after_approval_or_later_stops_the_display() {
    let (e, app) = site("dp3");
    // Approved, then the account's password changes before the display picks it up: no session.
    let ip = [10, 63, 0, 1];
    let (code, wait) = start(&app, ip).await;
    assert_eq!(approve(&app, "dp3boss", &code, "dp3tv", "TV").await.status, StatusCode::OK);
    let _ = crate::auth::forget_user("dp3tv", None);
    let g = send(&app, Call { cookie: Some(wait), ..call("POST", "/api/display/wait", ip) }).await;
    assert_eq!(g.status, StatusCode::GONE, "{}", g.body);
    assert!(g.cookie("as_session").is_none());
    // Paired, then the account becomes an operator: the display is still only a viewer.
    let (tok, _) = paired(&app, "dp3", "dp3all", [10, 63, 0, 2]).await;
    {
        let mut cfg = e.config.lock().unwrap();
        let u = cfg.server.users.iter_mut().find(|u| u.username == "dp3all").unwrap();
        u.role = "operator".into();
    }
    let w = send(&app, Call { cookie: as_display(&tok), ..call("GET", "/api/whoami", [10, 63, 0, 2]) }).await.json();
    assert_eq!((w["role"].as_str(), w["display"].as_bool()), (Some("viewer"), Some(true)), "{w}");
    let g = send(&app, Call { cookie: as_display(&tok), body: Some(r#"{"armed":false}"#.into()), ..call("POST", "/api/shield", [10, 63, 0, 2]) }).await;
    assert_eq!(g.status, StatusCode::FORBIDDEN, "{}", g.body);
    // And an admin cannot be chosen at all, even made one later: the session never gets admin rights.
    {
        let mut cfg = e.config.lock().unwrap();
        let u = cfg.server.users.iter_mut().find(|u| u.username == "dp3all").unwrap();
        u.role.clear();
        u.admin = true;
    }
    let w = send(&app, Call { cookie: as_display(&tok), ..call("GET", "/api/whoami", [10, 63, 0, 2]) }).await.json();
    assert_eq!((w["admin"].as_bool(), w["role"].as_str()), (Some(false), Some("viewer")), "{w}");
    assert_eq!(send(&app, Call { cookie: as_display(&tok), ..call("GET", "/api/settings", [10, 63, 0, 2]) }).await.status, StatusCode::FORBIDDEN);
}

#[tokio::test]
async fn admins_hear_when_a_display_is_used_from_another_network() {
    let (_e, app) = site("dp5");
    let (tok, _) = paired(&app, "dp5", "dp5tv", [10, 65, 0, 1]).await;
    let told = || crate::account_http::sent_log().lock().unwrap().iter().filter(|(t, _)| t.contains("dp5tv")).count();
    // The same network (another address in its /24): nothing.
    send(&app, Call { cookie: as_display(&tok), ..call("GET", "/api/cameras", [10, 65, 0, 9]) }).await;
    assert_eq!(told(), 0);
    // Another network: told once, however often it is used from there.
    for _ in 0..3 {
        assert_eq!(send(&app, Call { cookie: as_display(&tok), ..call("GET", "/api/cameras", [10, 99, 0, 1]) }).await.status, StatusCode::OK);
    }
    assert_eq!(told(), 1);
    let (t, b) = crate::account_http::sent_log().lock().unwrap().iter().find(|(t, _)| t.contains("dp5tv")).cloned().unwrap();
    assert!(t.contains("another network") && b.contains("10.65.0.1") && b.contains("10.99.0.1") && b.contains("Hall TV"), "{t}: {b}");
}

#[tokio::test]
async fn a_code_is_picked_up_only_from_the_network_that_asked() {
    let (_e, app) = site("dp6");
    let (code, wait) = start(&app, [10, 66, 0, 1]).await;
    assert_eq!(approve(&app, "dp6boss", &code, "dp6tv", "TV").await.status, StatusCode::OK);
    let g = send(&app, Call { cookie: Some(wait.clone()), ..call("POST", "/api/display/wait", [10, 67, 0, 1]) }).await;
    assert_eq!((g.status, g.cookie("as_session").is_none()), (StatusCode::GONE, true), "another network: {}", g.body);
    let g = send(&app, Call { cookie: Some(wait), ..call("POST", "/api/display/wait", [10, 66, 0, 20]) }).await;
    assert_eq!(g.status, StatusCode::OK, "the same /24: {}", g.body);
}

#[tokio::test]
async fn few_codes_wait_per_network_and_none_on_an_open_server() {
    let (_e, app) = site("dp4");
    for _ in 0..4 {
        start(&app, [10, 64, 0, 1]).await;
    }
    let g = send(&app, call("POST", "/api/display/start", [10, 64, 0, 77])).await;
    assert_eq!(g.status, StatusCode::TOO_MANY_REQUESTS, "the same /24: {}", g.body);
    assert!(g.body.contains("Too many displays are waiting"), "{}", g.body);
    assert_eq!(send(&app, call("POST", "/api/display/start", [10, 64, 9, 2])).await.status, StatusCode::OK, "another network is not held up");
    // A new code for a browser that had one replaces it (no pile-up from one TV asking again).
    let (_c, mut wait) = start(&app, [10, 64, 1, 1]).await;
    for _ in 0..6 {
        let g = send(&app, Call { cookie: Some(wait.clone()), ..call("POST", "/api/display/start", [10, 64, 1, 1]) }).await;
        assert_eq!(g.status, StatusCode::OK, "{}", g.body);
        wait = format!("as_display_wait={}", value(g.cookie("as_display_wait").unwrap()));
    }
    // No accounts: nothing to pair.
    let e = crate::engine::Engine::for_test(toml::from_str("").unwrap());
    let open = crate::web::build_router(e);
    let g = send(&open, call("POST", "/api/display/start", [10, 64, 2, 1])).await;
    assert_eq!((g.status, g.json()["open"].as_bool()), (StatusCode::OK, Some(true)));
    // The page itself is served with the build it was made from.
    let g = send(&open, call("GET", "/display", [10, 64, 2, 1])).await;
    assert_eq!(g.status, StatusCode::OK);
    assert!(g.body.contains("name=\"ambersight-ui\"") && g.body.contains("/static/js/display.js"));
}

#[test]
fn a_display_session_is_known_again_after_a_restart() {
    // Told apart by its label, which no browser can give itself.
    for ua in ["Display: Hall TV", "Display: x Chrome/1 Android", ""] {
        assert!(!crate::devices::device_label(ua).starts_with(crate::auth::DISPLAY_LABEL), "{ua}");
    }
    // Its moved expiry is what a restart loads.
    let db = crate::db::Db::open_in_memory().unwrap();
    let row = crate::db::StoredSession { hash: "h1".into(), user: "tv".into(), admin: false, expires: 100, created: 1, ip: String::new(), label: "Display: TV (Chrome on Android)".into(), device: String::new(), last_seen: 1 };
    db.save_session(&row).unwrap();
    db.extend_session("h1", 5000).unwrap();
    db.extend_session("gone", 5000).unwrap();
    let rows = db.load_sessions().unwrap();
    assert_eq!(rows.len(), 1, "an ended session is not made again");
    assert_eq!((rows[0].expires, rows[0].label.starts_with(crate::auth::DISPLAY_LABEL)), (5000, true));
}
