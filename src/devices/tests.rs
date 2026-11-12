use super::*;

fn engine() -> Arc<Engine> {
    Engine::for_test(toml::from_str("").unwrap())
}

fn ctx(cookie: Option<&str>, session_user: Option<&str>, ip: &str, ua: &str) -> SignInCtx {
    SignInCtx { ip: ip.parse().unwrap(), label: device_label(ua), cookie: cookie.map(str::to_string), session_user: session_user.map(str::to_string) }
}

const EDGE: &str = "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/120.0.0.0 Safari/537.36 Edg/120.0.0.0";
const SAFARI_IPHONE: &str = "Mozilla/5.0 (iPhone; CPU iPhone OS 17_0 like Mac OS X) AppleWebKit/605.1.15 (KHTML, like Gecko) Version/17.0 Mobile/15E148 Safari/604.1";

fn cookie(n: u8) -> String {
    format!("{n:02x}").repeat(32)
}

/// Everything sent whose title or text names this (test-only) account.
fn told(user: &str) -> Vec<(String, String)> {
    crate::account_http::sent_log().lock().unwrap().iter().filter(|(t, b)| t.contains(user) || b.contains(user)).cloned().collect()
}

fn flush(e: &Arc<Engine>, user: &str) {
    crate::account_http::flush_gathering(e, &format!("new-device:{user}"));
}

fn sign_in(e: &Arc<Engine>, user: &str, c: &SignInCtx, how: How) -> Option<Started> {
    complete_sign_in(e, c, Identity { user: user.into(), admin: false }, crate::auth::generation(user), how)
}

#[test]
fn labels_come_from_a_fixed_set() {
    assert_eq!(device_label(EDGE), "Edge on Windows");
    assert_eq!(device_label(SAFARI_IPHONE), "Safari on iPhone");
    assert_eq!(device_label("Mozilla/5.0 (X11; Linux x86_64; rv:121.0) Gecko/20100101 Firefox/121.0"), "Firefox on Linux");
    assert_eq!(device_label("Mozilla/5.0 (Linux; Android 14; Pixel 8) AppleWebKit/537.36 Chrome/120.0.0.0 Mobile Safari/537.36"), "Chrome on Android");
    assert_eq!(device_label("Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/605.1.15 Version/17.0 Safari/605.1.15"), "Safari on Mac");
    assert_eq!(device_label(""), "Unknown browser");
    assert_eq!(device_label("totally made up"), "Unknown browser");
    // Whatever a sender puts in the header, only a fixed label comes out.
    let browsers = ["Edge", "Opera", "Firefox", "Chrome", "Safari", "curl"];
    let systems = ["iPhone", "iPad", "Android", "ChromeOS", "Windows", "Mac", "Linux"];
    let mut allowed: Vec<String> = vec!["Unknown browser".into()];
    for b in browsers {
        allowed.push(b.to_string());
        allowed.extend(systems.iter().map(|s| format!("{b} on {s}")));
    }
    let hostile = [
        "<script>alert(1)</script> Chrome/1".to_string(),
        "Edg/1 \"><img src=x onerror=alert(1)> Windows".to_string(),
        "'; DROP TABLE sessions; -- Firefox/1 Linux".to_string(),
        "Chrome/1 \u{202e}Windows\u{200b}".to_string(),
        "curl/8 \r\nSet-Cookie: x=1".to_string(),
        format!("Safari/1 {}", "A".repeat(100_000)),
        "{{7*7}} ${jndi:ldap://x} %s %n Opera OPR/9 CrOS".to_string(),
        "Firefox/1 Windows Linux Android iPhone Macintosh".to_string(),
    ];
    for ua in hostile {
        let l = device_label(&ua);
        assert!(allowed.contains(&l), "{l}");
        assert!(l.len() < 40 && !l.contains('<') && !l.contains('\''), "{l}");
    }
}

#[test]
fn the_device_cookie_is_read_only_in_the_shape_this_server_makes() {
    let mut h = HeaderMap::new();
    h.append(header::COOKIE, format!("a=1; {DEVICE_COOKIE}=nothex; x=2").parse().unwrap());
    assert_eq!(cookie_value(&h), None);
    h.append(header::COOKIE, format!("{DEVICE_COOKIE}={}", cookie(7)).parse().unwrap());
    assert_eq!(cookie_value(&h), Some(cookie(7)), "also in a second header, past a stale one");
    let c = set_cookie(&cookie(7), true);
    assert!(c.contains("HttpOnly") && c.contains("SameSite=Strict") && c.contains("; Secure") && c.contains("Max-Age=63072000"), "{c}");
    assert!(!set_cookie(&cookie(7), false).contains("Secure"));
}

#[test]
fn a_first_sign_in_is_silent_and_a_new_browser_after_it_is_told() {
    let e = engine();
    let u = "nd-first";
    let a = sign_in(&e, u, &ctx(None, None, "192.0.2.10", EDGE), How::Password).unwrap();
    assert_eq!(a.device_cookie.len(), 64);
    assert_eq!(e.db.known_device_count(u), 1);
    assert!(told(u).is_empty(), "the account's first sign-in: {:?}", told(u));
    // The same browser again, from elsewhere.
    sign_in(&e, u, &ctx(Some(&a.device_cookie), None, "192.0.2.99", EDGE), How::Password).unwrap();
    assert!(told(u).is_empty());
    assert_eq!(e.db.known_device_count(u), 1);
    // Another browser.
    let b = sign_in(&e, u, &ctx(None, None, "192.0.2.11", SAFARI_IPHONE), How::Password).unwrap();
    assert_ne!(a.device_cookie, b.device_cookie);
    let sent = told(u);
    assert_eq!(sent.len(), 1, "{sent:?}");
    let (title, body) = &sent[0];
    assert!(title.contains("new device") && title.contains(u), "{title}");
    for part in ["Safari on iPhone", "192.0.2.11", "with the password", "If this was not you: open Your account, sign that device out, and change the password."] {
        assert!(body.contains(part), "{part}: {body}");
    }
    assert!(!body.contains('\u{2014}') && !body.contains("Mozilla"), "{body}");
    assert_eq!(e.db.known_device_count(u), 2);
}

#[test]
fn the_cookie_alone_does_not_say_known_for_another_account() {
    let e = engine();
    let a = sign_in(&e, "nd-x1", &ctx(None, None, "192.0.2.10", EDGE), How::Password).unwrap();
    sign_in(&e, "nd-x2", &ctx(None, None, "192.0.2.20", EDGE), How::Password).unwrap();
    // Another account, with the first one's cookie: new for it.
    sign_in(&e, "nd-x2", &ctx(Some(&a.device_cookie), None, "192.0.2.30", EDGE), How::Password).unwrap();
    assert_eq!(told("nd-x2").len(), 1);
    assert_eq!(e.db.known_device_count("nd-x2"), 2);
    assert_eq!(e.db.known_device_count("nd-x1"), 1);
}

#[test]
fn a_live_session_of_the_same_account_covers_the_sign_in() {
    let e = engine();
    let u = "nd-cover";
    sign_in(&e, u, &ctx(None, None, "192.0.2.10", EDGE), How::Password).unwrap();
    // After an upgrade: signing in again where a session already works. Not told, but remembered.
    sign_in(&e, u, &ctx(None, Some(u), "192.0.2.11", EDGE), How::Password).unwrap();
    assert!(told(u).is_empty());
    assert_eq!(e.db.known_device_count(u), 2);
    // A session of someone else covers nothing.
    sign_in(&e, u, &ctx(None, Some("someone-else"), "192.0.2.12", EDGE), How::Password).unwrap();
    assert_eq!(told(u).len(), 1);
}

#[test]
fn an_account_that_was_used_before_is_not_taken_for_a_new_one() {
    let e = engine();
    // Upgraded server: the account has a session from before, and no known browser yet.
    let old = "nd-upg";
    let _ = crate::auth::new_session(Identity { user: old.into(), admin: false });
    sign_in(&e, old, &ctx(None, None, "192.0.2.10", EDGE), How::Password).unwrap();
    assert_eq!(told(old).len(), 1, "the first sign-in after an upgrade, from a browser with no session");
    // Or only the audit log remembers it.
    let logged = "nd-audit";
    e.db.insert_audit(logged, "192.0.2.1", "sign-in", "", 200).unwrap();
    sign_in(&e, logged, &ctx(None, None, "192.0.2.10", EDGE), How::Password).unwrap();
    assert_eq!(told(logged).len(), 1);
    // Wrong passwords and a first step do not make an account old.
    let fresh = "nd-fresh";
    e.db.insert_audit(fresh, "192.0.2.1", "sign-in failed", "", 401).unwrap();
    e.db.insert_audit(fresh, "192.0.2.1", "sign-in: password right, code asked for", "", 200).unwrap();
    sign_in(&e, fresh, &ctx(None, None, "192.0.2.10", EDGE), How::Password).unwrap();
    assert!(told(fresh).is_empty());
}

#[test]
fn nothing_is_kept_when_the_sign_in_does_not_become_a_session() {
    let e = engine();
    let u = "nd-stale";
    let stale = crate::auth::generation(u);
    crate::auth::forget_user(u, None);
    assert!(complete_sign_in(&e, &ctx(None, None, "192.0.2.10", EDGE), Identity { user: u.into(), admin: false }, stale, How::Password).is_none());
    assert_eq!(e.db.known_device_count(u), 0);
    assert!(told(u).is_empty());
}

#[test]
fn each_way_of_signing_in_is_named() {
    let e = engine();
    let u = "nd-how";
    sign_in(&e, u, &ctx(None, None, "192.0.2.10", EDGE), How::Password).unwrap();
    let mut n = 0;
    for (i, (how, words)) in [(How::Password, "with the password."), (How::TwoStepCode, "password and a two-step code"), (How::RecoveryCode(3), "recovery code (3 left)"), (How::QrCode, "sign-in QR code"), (How::LinkedServer, "another AmberSight server")].into_iter().enumerate() {
        // (Each in its own gathering window.)
        flush(&e, u);
        sign_in(&e, u, &ctx(None, None, &format!("192.0.2.{}", 20 + i), EDGE), how).unwrap();
        n += 1;
        let sent = told(u);
        assert_eq!(sent.len(), n, "{sent:?}");
        assert!(sent.last().unwrap().1.contains(words), "{words}: {:?}", sent.last());
    }
}

#[test]
fn a_burst_gives_one_message_at_once_and_a_summary_after() {
    let e = engine();
    let u = "nd-burst";
    sign_in(&e, u, &ctx(None, None, "192.0.2.10", EDGE), How::Password).unwrap();
    for i in 0..5 {
        sign_in(&e, u, &ctx(None, None, &format!("192.0.2.{}", 50 + i), SAFARI_IPHONE), How::Password).unwrap();
    }
    assert_eq!(told(u).len(), 1, "one at once");
    flush(&e, u);
    let sent = told(u);
    assert_eq!(sent.len(), 2, "{sent:?}");
    assert_eq!(sent[1].1.matches("Safari on iPhone from 192.0.2.5").count(), 4, "{}", sent[1].1);
    assert!(sent[1].1.contains("If this was not you"));
}

#[test]
fn a_recovery_code_sign_in_is_told_in_one_message() {
    let e = engine();
    let u = "nd-recov";
    let a = sign_in(&e, u, &ctx(None, None, "192.0.2.10", EDGE), How::Password).unwrap();
    // Known browser: the existing recovery warning, once.
    sign_in(&e, u, &ctx(Some(&a.device_cookie), None, "192.0.2.10", EDGE), How::RecoveryCode(7)).unwrap();
    let sent = told(u);
    assert_eq!(sent.len(), 1, "{sent:?}");
    assert!(sent[0].0.contains("recovery code"), "{:?}", sent[0]);
    // A new browser: one message, with both.
    sign_in(&e, u, &ctx(None, None, "192.0.2.60", EDGE), How::RecoveryCode(6)).unwrap();
    let sent = told(u);
    assert_eq!(sent.len(), 2, "{sent:?}");
    assert!(sent[1].0.contains("new device") && sent[1].1.contains("recovery code (6 left)"), "{:?}", sent[1]);
}

#[test]
fn a_password_sent_directly_is_noted_once_per_network() {
    let e = engine();
    let u = "nd-basic";
    let ip = |s: &str| -> IpAddr { s.parse().unwrap() };
    note_direct(&e, u, ip("192.0.2.10"));
    assert!(told(u).is_empty(), "the first use of an account");
    assert_eq!(e.db.known_device_count(u), 1);
    // Again from the same network: nothing, not even a look at the database.
    note_direct(&e, u, ip("192.0.2.10"));
    assert_eq!(told(u).len(), 0);
    // Another network.
    note_direct(&e, u, ip("2001:db8:1:2::5"));
    let sent = told(u);
    assert_eq!(sent.len(), 1, "{sent:?}");
    assert!(sent[0].1.contains("2001:db8:1:2::5") && sent[0].1.contains("sent directly by a program") && sent[0].1.contains("If this was not you: change the password.") && !sent[0].1.contains("sign that device out"), "{}", sent[0].1);
    // Another address of the same IPv6 network is the same network.
    note_direct(&e, u, ip("2001:db8:1:2::77"));
    assert_eq!(told(u).len(), 1);
    assert_eq!(e.db.known_device_count(u), 2);
}

#[test]
fn at_most_so_many_browsers_are_kept_and_the_least_recent_go() {
    let e = engine();
    let u = "nd-cap";
    for i in 0..60 {
        e.db.device_seen(u, &format!("h{i}"), "x", 1000 + i, MAX_DEVICES).unwrap();
    }
    assert_eq!(e.db.known_device_count(u), MAX_DEVICES);
    assert!(e.db.device_seen(u, "h59", "x", 2000, MAX_DEVICES).unwrap().known, "the newest stay");
    assert!(!e.db.device_seen(u, "h0", "x", 2001, MAX_DEVICES).unwrap().known, "the oldest went");
    assert_eq!(e.db.known_device_count(u), MAX_DEVICES);
}
