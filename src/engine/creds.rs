use super::*;

/// Sanitize a camera config at ingest: trim all text fields, and move
/// credentials embedded in the URL (scheme://user:pw@host) into the dedicated
/// username/password fields so no API response or log can echo a secret.
pub fn normalize_creds(cfg: &mut CameraConfig) {
    cfg.name = cfg.name.trim().to_string();
    cfg.short_name = cfg.short_name.trim().to_string();
    cfg.normalize_groups();
    cfg.url = cfg.url.trim().to_string();
    cfg.username = cfg.username.trim().to_string();
    cfg.password = cfg.password.trim().to_string();

    if let Some((head, userinfo, host_and_path)) = split_userinfo(&cfg.url) {
        if cfg.username.is_empty() {
            // Split at the FIRST ':' (usernames cannot contain one unencoded),
            // then percent-decode both halves.
            let (u, p) = match userinfo.split_once(':') {
                Some((u, p)) => (u, Some(p)),
                None => (userinfo.as_str(), None),
            };
            cfg.username = pct_decode(u);
            if let Some(p) = p {
                cfg.password = pct_decode(p);
            }
        }
        cfg.url = format!("{}{}", head, host_and_path);
    }

    // The sub-stream keeps its own userinfo in the URL, but encoded, so a
    // space or other raw character in the password cannot break the URL or
    // slip past log redaction.
    if let Some(sub) = cfg.sub_url.as_mut() {
        *sub = sub.trim().to_string();
        if let Some((head, userinfo, host_and_path)) = split_userinfo(sub) {
            let enc = match userinfo.split_once(':') {
                Some((u, p)) => format!("{}:{}", pct_encode(&pct_decode(u)), pct_encode(&pct_decode(p))),
                None => pct_encode(&pct_decode(&userinfo)),
            };
            *sub = format!("{head}{enc}@{host_and_path}");
        }
    }
}

/// Characters left unescaped in userinfo: unreserved only. Everything else
/// (including @ : / # ? %) is percent-encoded.
const USERINFO: &percent_encoding::AsciiSet = &percent_encoding::NON_ALPHANUMERIC
    .remove(b'-')
    .remove(b'.')
    .remove(b'_')
    .remove(b'~');

fn pct_encode(s: &str) -> String {
    percent_encoding::utf8_percent_encode(s, USERINFO).to_string()
}

fn pct_decode(s: &str) -> String {
    percent_encoding::percent_decode_str(s).decode_utf8_lossy().into_owned()
}

/// Split "scheme://userinfo@host/path" into ("scheme://", "userinfo",
/// "host/path"). The authority ends at the first '/', and userinfo ends at
/// the LAST '@' in it (passwords may contain '@'). None when no userinfo.
fn split_userinfo(url: &str) -> Option<(String, String, String)> {
    // Local sources carry no sign-in; their names may contain '@'.
    if crate::local_source::is_local(url) {
        return None;
    }
    let (scheme, rest) = url.split_once("://")?;
    if rest.contains("://") {
        return None;
    }
    let (at, _) = userinfo_at(rest)?;
    Some((format!("{}://", scheme), rest[..at].to_string(), rest[at + 1..].to_string()))
}

/// Where a URL's sign-in ends: the index of the '@' before the host in
/// `rest` (the URL after "scheme://"), and where the host[:port] after it
/// ends. The one rule for every parser; static/js/camaddr.js `userinfoAt` is
/// the same rule, and both are checked against scripts/userinfo-cases.json.
/// - The last '@' before the first '/' (passwords may hold '@', '?', '#').
/// - Else, only when what comes before the first '/' is not already a
///   host[:port] (an address that worked before keeps its meaning: the '@'
///   in "camhost:554/live@2" or "[fe80::1]/live@2" is in the path), a
///   password typed raw with a '/' in it ("u:pa/ss@host/x"): the last '@'
///   before any '?', with user:password before it (the ':' before any '/')
///   and a host after it that looks like one (a dot, brackets, a port, or
///   "localhost").
pub(crate) fn userinfo_at(rest: &str) -> Option<(usize, usize)> {
    let slash = rest.find('/').unwrap_or(rest.len());
    if let Some(at) = rest[..slash].rfind('@') {
        return Some((at, slash));
    }
    if is_host_port(&rest[..slash]) {
        return None;
    }
    // (Up to the query only: a '#' is common in passwords, and a fragment
    // means nothing to a camera.)
    let pre = &rest[..rest.find('?').unwrap_or(rest.len())];
    let at = pre.rfind('@')?;
    let head = &pre[..at];
    let colon = head.find(':')?;
    if head[..colon].contains('/') || !head[colon..].contains('/') {
        return None;
    }
    let after = &rest[at + 1..];
    let end = after.find(['/', '?', '#']).unwrap_or(after.len());
    let host = &after[..end];
    let looks = host.contains('.') || host.starts_with('[') || host.contains(':') || host.eq_ignore_ascii_case("localhost");
    (looks && is_host_port(host)).then_some((at, at + 1 + end))
}

/// "host", "host:554", "[::1]:554" (camaddr.js HOSTPORT): a name without
/// spaces, '/', ':', '@' or brackets, or a bracketed address without spaces
/// or '/'; then digits only after a ':'.
fn is_host_port(s: &str) -> bool {
    let (host_ok, port) = if let Some(inner) = s.strip_prefix('[') {
        match inner.find(']') {
            Some(i) => (!inner[..i].contains(|c: char| c.is_whitespace() || c == '/'), &inner[i + 1..]),
            None => return false,
        }
    } else {
        let i = s.find(':').unwrap_or(s.len());
        (i > 0 && s[..i].chars().all(|c| !c.is_whitespace() && !"/:@[]".contains(c)), &s[i..])
    };
    host_ok && (port.is_empty() || (port.starts_with(':') && port[1..].bytes().all(|b| b.is_ascii_digit())))
}

/// The address without its sign-in, and that sign-in (percent-decoded),
/// when it has one: "rtsp://u:p@h/x" -> ("rtsp://h/x", Some(("u", "p"))).
pub(crate) fn split_login(url: &str) -> (String, Option<(String, String)>) {
    match split_userinfo(url) {
        Some((head, info, rest)) => {
            let (u, p) = info.split_once(':').unwrap_or((info.as_str(), ""));
            (format!("{head}{rest}"), Some((pct_decode(u), pct_decode(p))))
        }
        None => (url.to_string(), None),
    }
}

/// Display-safe URL for logs: strips userinfo even if normalization missed.
pub fn redact_url(url: String) -> String {
    redact_url_with(url, false)
}

/// [`redact_url`]; `strict` as in [`shown_userinfo_at`].
pub(crate) fn redact_url_with(url: String, strict: bool) -> String {
    let url = match url.split_once("://") {
        Some((scheme, rest)) if !crate::local_source::is_local(&url) && !rest.contains("://") => match shown_userinfo_at(rest, strict) {
            Some(at) => format!("{scheme}://{}", &rest[at + 1..]),
            None => url.clone(),
        },
        _ => url,
    };
    redact_query_password(url)
}

/// Where the sign-in ends for SHOWING an address (logs, the camera
/// report): wider than [`userinfo_at`], which decides how a camera is
/// connected to and is left alone. Some sign-ins the connection rule
/// leaves in place on purpose ("admin:12/34@192.0.2.5/x", whose start looks
/// like a host:port; "u:x/y@camhost/main", no dot after the '@') would
/// otherwise be shown. Here the last '@' before any '?' ends the sign-in
/// when what comes before it has a ':' (before any '/') and what follows it is a host-like
/// word (then '/', '?', '#' or the end). Not `strict` (logs): an address
/// that starts with a plain dotted, bracketed or localhost host[:port]
/// keeps its '@' as a path ("192.0.2.5:554/live@2" stays readable).
/// `strict` (the report): masked even then, since a report must not hold a
/// password ("rtsp://192.0.2.5:554/live@2" shows as "rtsp://2").
pub(crate) fn shown_userinfo_at(rest: &str, strict: bool) -> Option<usize> {
    if let Some((at, _)) = userinfo_at(rest) {
        return Some(at);
    }
    let pre = &rest[..rest.find('?').unwrap_or(rest.len())];
    let at = pre.rfind('@')?;
    // user:password: a ':' before any '/'.
    let colon = pre[..at].find(':')?;
    if pre[..colon].contains('/') {
        return None;
    }
    if !strict {
        let first = &rest[..rest.find('/').unwrap_or(rest.len())];
        let name = first.split(':').next().unwrap_or("");
        if is_host_port(first) && (first.starts_with('[') || name.contains('.') || name.eq_ignore_ascii_case("localhost")) {
            return None;
        }
    }
    let after = &rest[at + 1..];
    let end = after.find(['/', '?', '#']).unwrap_or(after.len());
    is_host_port(&after[..end]).then_some(at)
}

/// The sign-in of an address as [`shown_userinfo_at`] (strict) sees it,
/// percent-decoded: what the camera report must keep out.
pub(crate) fn shown_login(url: &str) -> Option<(String, String)> {
    let (_, rest) = url.split_once("://")?;
    if crate::local_source::is_local(url) {
        return None;
    }
    let at = shown_userinfo_at(rest, true)?;
    let info = &rest[..at];
    let (u, p) = info.split_once(':').unwrap_or((info, ""));
    Some((pct_decode(u), pct_decode(p)))
}

/// Query names whose values are secrets.
/// (User names too: a camera's sign-in is the pair.)
const SECRET_KEYS: [&str; 27] = [
    "password", "pwd", "pass", "passwd", "user", "username", "usr", "token", "key", "apikey", "api_key", "auth", "loginuse", "loginpas", "pw", "psw",
    "pswd", "access_token", "secret", "session", "sid", "sig", "signature", "loginpass", "adminpass", "userpass", "passkey",
];
/// A name ending in one of these is secret too when the ending stands as a
/// word of its own: after '_' or '-' ("x_pass", "cam-token") or a case
/// change ("camToken", "adminPwd"). "monkey", "bypass" and "hotkey" are not.
const SECRET_ENDINGS: [&str; 5] = ["pass", "pwd", "token", "key", "secret"];

/// Whether a query or key=value name holds a secret: one of SECRET_KEYS
/// (also as its last part after '_' or '-'), or a SECRET_ENDINGS word at its end.
pub(crate) fn is_secret_key(k: &str) -> bool {
    let low = k.to_ascii_lowercase();
    if SECRET_KEYS.contains(&low.as_str()) || low.rsplit(['_', '-']).next().is_some_and(|last| SECRET_KEYS.contains(&last)) {
        return true;
    }
    let b = k.as_bytes();
    SECRET_ENDINGS.iter().any(|e| {
        if !low.ends_with(e) || low.len() == e.len() {
            return false;
        }
        let i = k.len() - e.len();
        matches!(b[i - 1], b'_' | b'-') || (b[i].is_ascii_uppercase() && (b[i - 1].is_ascii_lowercase() || b[i - 1].is_ascii_digit()))
    })
}

/// Hide `password=` / `pwd=` (and token, key, ...) query values (HTTP-FLV and snapshot URLs
/// carry device sign-in in the query).
fn redact_query_password(url: String) -> String {
    let Some((base, query)) = url.split_once('?') else { return url };
    let parts: Vec<String> = query
        .split('&')
        .map(|kv| match kv.split_once('=') {
            Some((k, _)) if is_secret_key(k) => format!("{k}=***"),
            _ => kv.to_string(),
        })
        .collect();
    format!("{base}?{}", parts.join("&"))
}

/// Build the effective stream URL for a camera: when the config carries
/// device credentials and the URL has no userinfo yet, inject them
/// (scheme://[user:pass@]host/path). RTSP/HTTP/HTTPS all accept userinfo.
pub fn camera_url(cfg: &CameraConfig) -> String {
    if cfg.username.is_empty() || split_userinfo(&cfg.url).is_some() || crate::local_source::is_local(&cfg.url) {
        return cfg.url.clone();
    }
    match cfg.url.split_once("://") {
        Some((scheme, rest)) => format!(
            "{}://{}:{}@{}",
            scheme,
            pct_encode(&cfg.username),
            pct_encode(&cfg.password),
            rest
        ),
        None => cfg.url.clone(),
    }
}

/// What a sub stream address may start with.
const SUB_SCHEMES: [&str; 5] = ["rtsp://", "rtsps://", "http://", "https://", "rtmp://"];

/// Why a sub stream address cannot be used, or None. ("stream2" alone has
/// no scheme: the reader could not open it.)
pub fn sub_url_problem(u: &str) -> Option<String> {
    let t = u.trim();
    let low = t.to_ascii_lowercase();
    let parses = || url::Url::parse(&split_login(t).0).is_ok_and(|x| x.host_str().is_some_and(|h| !h.is_empty()));
    if SUB_SCHEMES.iter().any(|s| low.starts_with(s)) && parses() {
        return None;
    }
    Some(format!("the sub stream URL \"{}\" is not a stream address (it must start with rtsp://, rtsps://, http:// or https://)", redact_url(t.to_string())))
}

/// The camera's sub stream address when it is set and usable.
pub fn usable_sub_url(cfg: &CameraConfig) -> Option<String> {
    cfg.sub_url.as_deref().map(str::trim).filter(|u| !u.is_empty() && sub_url_problem(u).is_none()).map(str::to_string)
}

/// The check when a camera is saved: the main address needs a scheme
/// (rtsp://, http://, push:// ... or a local device), the sub stream one of
/// the stream schemes.
pub fn check_urls(cfg: &CameraConfig) -> Result<(), String> {
    let main = cfg.url.trim();
    if !main.is_empty() && !main.contains("://") && !crate::local_source::is_local(main) {
        return Err("Stream URL must start with rtsp:// (or another stream address such as http://)".into());
    }
    if let Some(s) = cfg.sub_url.as_deref().map(str::trim).filter(|s| !s.is_empty()) {
        if sub_url_problem(s).is_some() {
            return Err("Sub stream URL must start with rtsp:// (or rtsps://, http://, https://) and name the camera, e.g. rtsp://192.168.1.50/sub".into());
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn creds_normalize_trim_and_redact() {
        let mut c = CameraConfig {
            name: "  Front Door  ".to_string(),
            url: " rtsp://admin:secret99@192.0.2.5:554/main ".to_string(),
            sub_url: None, ingest: "auto".to_string(), transport: "auto".to_string(), connect: "rtsp".to_string(), baichuan_port: None, baichuan_channel: None, hwaccel: None, folder: None, copy_events_to: None, copy_events_keep_days: 0, copy_events_only_confirmed: false, fill_from_card: false, catch_up_from_card: None, catch_up_every: None, min_object: None, contrast: None, ai: None, ai_labels: None, onvif_port: None, trigger: "video".to_string(), on_demand: false, idle_timeout: 300, onvif_source: None, reolink_channel: None, uid: None, privacy_masks: Vec::new(), mask_mode: None, zone_polys: Vec::new(), ignore_polys: Vec::new(), named_zones: Vec::new(), groups: Vec::new(), motion_algorithm: None, max_object: None, min_travel: None, min_frames: None, break_time: None, pre_roll: None, post_roll: None, alerts: None, alert_channels: None, alert_interval: None, alert_schedule: None, alert_labels: None, alert_ignore_polys: Vec::new(), no_motion_minutes: None, no_motion_schedule: None, audio_trigger: false, audio_margin_db: None, audio_min_db: None, audio_sounds: Vec::new(), ai_min_score: None, also_trigger: Vec::new(), alert_priority: None, capture: None, watchdog: None, controls: None, share: false, strangers: None, tamper: None, unusual: None,
            username: "".to_string(),
            password: "".to_string(),
            short_name: "".to_string(),
            group: "".to_string(),
            schedule: None,
            fps: 10, scale: 640, record: true, motion: true, enabled: true,
            direct: false, overlay: false, overlay_text: None, overlay_pos: None, ai_ignore_parked: None, ai_strict: None, ai_hide_unconfirmed: None, ai_confirm_camera: None, packages: None, audio: false, record_motion_only: false, record_sub: false, no_sub_ok: false, sensitivity: None,
            zones: Vec::new(), ignore_zones: Vec::new(),
        };
        normalize_creds(&mut c);
        assert!(c.name == "Front Door");
        assert!(c.url == "rtsp://192.0.2.5:554/main");
        assert!(c.username == "admin");
        assert!(c.password == "secret99");
        assert!(!c.url.contains("secret99"));
        assert!(redact_url("rtsp://u:p@h/x".to_string()) == "rtsp://h/x");
        assert!(camera_url(&c) == "rtsp://admin:secret99@192.0.2.5:554/main");
    }

    fn blank_cfg(url: &str) -> CameraConfig {
        CameraConfig {
            name: "c".to_string(), url: url.to_string(), sub_url: None, ingest: "auto".to_string(), transport: "auto".to_string(), connect: "rtsp".to_string(), baichuan_port: None, baichuan_channel: None, hwaccel: None, folder: None, copy_events_to: None, copy_events_keep_days: 0, copy_events_only_confirmed: false, fill_from_card: false, catch_up_from_card: None, catch_up_every: None, min_object: None, contrast: None, ai: None, ai_labels: None, onvif_port: None, trigger: "video".to_string(), on_demand: false, idle_timeout: 300, onvif_source: None, reolink_channel: None, uid: None, privacy_masks: Vec::new(), mask_mode: None, zone_polys: Vec::new(), ignore_polys: Vec::new(), named_zones: Vec::new(), groups: Vec::new(), motion_algorithm: None, max_object: None, min_travel: None, min_frames: None, break_time: None, pre_roll: None, post_roll: None, alerts: None, alert_channels: None, alert_interval: None, alert_schedule: None, alert_labels: None, alert_ignore_polys: Vec::new(), no_motion_minutes: None, no_motion_schedule: None, audio_trigger: false, audio_margin_db: None, audio_min_db: None, audio_sounds: Vec::new(), ai_min_score: None, also_trigger: Vec::new(), alert_priority: None, capture: None, watchdog: None, controls: None, share: false, strangers: None, tamper: None, unusual: None, username: "".to_string(), password: "".to_string(),
            short_name: "".to_string(), group: "".to_string(), schedule: None,
            fps: 10, scale: 640, record: true, motion: true, enabled: true,
            direct: false, overlay: false, overlay_text: None, overlay_pos: None, ai_ignore_parked: None, ai_strict: None, ai_hide_unconfirmed: None, ai_confirm_camera: None, packages: None, audio: false, record_motion_only: false, record_sub: false, no_sub_ok: false, sensitivity: None,
            zones: Vec::new(), ignore_zones: Vec::new(),
        }
    }

    #[test]
    fn creds_special_chars_roundtrip() {
        // Raw (unencoded) '@' in the password is tolerated: split at last '@'.
        // '/' in a raw password would end the authority, so it is encoded here.
        let mut c = blank_cfg("rtsp://adm:p@ss:w%2Frd%23%3F%25@192.0.2.5:554/main");
        normalize_creds(&mut c);
        assert_eq!(c.username, "adm");
        assert_eq!(c.password, "p@ss:w/rd#?%");
        assert_eq!(c.url, "rtsp://192.0.2.5:554/main");
        let built = camera_url(&c);
        assert_eq!(built, "rtsp://adm:p%40ss%3Aw%2Frd%23%3F%25@192.0.2.5:554/main");
        // Parseable: last '@' separates host, first ':' separates user/pass.
        let (_, ui, host) = split_userinfo(&built).unwrap();
        assert_eq!(host, "192.0.2.5:554/main");
        let (u, p) = ui.split_once(':').unwrap();
        assert_eq!(pct_decode(u), "adm");
        assert_eq!(pct_decode(p), "p@ss:w/rd#?%");
        // Round trip again through normalize.
        let mut c2 = blank_cfg(&built);
        normalize_creds(&mut c2);
        assert_eq!(c2.password, "p@ss:w/rd#?%");
    }

    #[test]
    fn sub_url_userinfo_is_encoded() {
        let mut c = blank_cfg("rtsp://h/main");
        c.sub_url = Some(" rtsp://admin:my pass@h/s ".into());
        normalize_creds(&mut c);
        let sub = c.sub_url.clone().unwrap();
        assert_eq!(sub, "rtsp://admin:my%20pass@h/s");
        assert!(!camera_url(&CameraConfig { url: sub.clone(), ..c.clone() }).contains(' '));
        assert_eq!(crate::engine::worker::redact_text(&format!("{sub}: 401")), "rtsp://h/s: 401");
        // Already encoded stays the same.
        normalize_creds(&mut c);
        assert_eq!(c.sub_url.unwrap(), sub);
    }

    #[test]
    fn sub_stream_addresses_are_checked() {
        let mut c = blank_cfg("rtsp://192.0.2.5/main");
        assert!(check_urls(&c).is_ok());
        c.sub_url = Some("stream2".into());
        assert!(check_urls(&c).unwrap_err().starts_with("Sub stream URL must start with rtsp://"));
        assert_eq!(usable_sub_url(&c), None);
        assert!(sub_url_problem("stream2").unwrap().contains("\"stream2\""));
        assert!(!sub_url_problem("rtsp://admin:secret99@/x").unwrap().contains("secret99"));
        c.sub_url = Some("rtsp://admin:p%40ss@192.0.2.5:554/sub".into());
        assert!(check_urls(&c).is_ok());
        assert_eq!(usable_sub_url(&c).as_deref(), Some("rtsp://admin:p%40ss@192.0.2.5:554/sub"));
        c.sub_url = Some("  ".into());
        assert!(check_urls(&c).is_ok());
        c.url = "192.168.1.5/main".into();
        assert!(check_urls(&c).unwrap_err().starts_with("Stream URL must start"));
    }

    #[test]
    fn split_login_takes_the_sign_in_out() {
        assert_eq!(split_login("rtsp://admin:p%40ss%23@h:554/sub?x=1"), ("rtsp://h:554/sub?x=1".to_string(), Some(("admin".into(), "p@ss#".into()))));
        assert_eq!(split_login("rtsp://h/live@main"), ("rtsp://h/live@main".to_string(), None));
        assert_eq!(split_login("rtsp://u@h/x"), ("rtsp://h/x".to_string(), Some(("u".into(), "".into()))));
    }

    #[test]
    fn camera_url_injects_when_path_has_at() {
        let mut c = blank_cfg("rtsp://192.0.2.5/live@main");
        c.username = "u".to_string();
        c.password = "p".to_string();
        assert_eq!(camera_url(&c), "rtsp://u:p@192.0.2.5/live@main");
    }

    #[test]
    fn redact_handles_at_in_password() {
        assert_eq!(redact_url("rtsp://u:p@ss@host:554/x".to_string()), "rtsp://host:554/x");
        assert_eq!(redact_url("rtsp://u:p%40ss@host/x".to_string()), "rtsp://host/x");
        assert_eq!(redact_url("rtsp://host/x@y".to_string()), "rtsp://host/x@y");
    }

    /// A password typed raw with a '/' in it (the review's string): one rule
    /// for the settings, the report and the log.
    #[test]
    fn raw_slash_in_password() {
        let url = "rtsp://slashusr:pa/ss99xx@192.0.2.5/main";
        let mut c = blank_cfg(url);
        normalize_creds(&mut c);
        assert_eq!((c.username.as_str(), c.password.as_str(), c.url.as_str()), ("slashusr", "pa/ss99xx", "rtsp://192.0.2.5/main"));
        c.sub_url = Some("rtsp://slashusr:pa/ss99xx@192.0.2.5:554/sub?x=1".into());
        normalize_creds(&mut c);
        assert_eq!(c.sub_url.as_deref(), Some("rtsp://slashusr:pa%2Fss99xx@192.0.2.5:554/sub?x=1"));
        assert_eq!(redact_url(url.into()), "rtsp://192.0.2.5/main");
        assert_eq!(split_login(url).1, Some(("slashusr".into(), "pa/ss99xx".into())));
        let t = crate::engine::worker::redact_text(&format!("[rtsp] {url}: 401, then rtsp://a:b/c/d@cam.local:8554/x?pwd=z"));
        assert_eq!(t, "[rtsp] rtsp://192.0.2.5/main: 401, then rtsp://cam.local:8554/x?pwd=***");
        // An '@' in a path is not a sign-in (the re-review's cases too).
        for u in [
            "rtsp://192.0.2.5/live@2",
            "rtsp://192.0.2.5:554/live@2",
            "rtsp://camhost:554/live@2",
            "rtsp://localhost:8554/cam@1",
            "rtsp://[fe80::1]/live@2",
            "rtsp://[::1]:8554/live@2",
            "rtsp://h/x?mail=a@b.c",
            "rtsp://h/a:b@c d",
        ] {
            assert_eq!(split_login(u), (u.to_string(), None), "{u}");
            // Logs keep them readable, except a dotless host:port start,
            // which reads like user:password (see shown_userinfo_at).
            let shown = if u == "rtsp://camhost:554/live@2" { "rtsp://2" } else { u };
            assert_eq!(redact_url(u.into()), shown, "{u}");
            assert_eq!(crate::engine::worker::redact_text(u), shown, "{u}");
        }
    }

    /// Sign-ins the connection rule leaves in the address on purpose are
    /// still hidden in logs and the report (the final re-review's strings).
    #[test]
    fn shown_addresses_hide_unsplit_sign_ins() {
        for (u, shown) in [("rtsp://admin:12/34@192.0.2.5/x", "rtsp://192.0.2.5/x"), ("rtsp://u:x/y@camhost/main", "rtsp://camhost/main")] {
            assert_eq!(split_login(u), (u.to_string(), None), "the connection rule is unchanged: {u}");
            assert_eq!(redact_url(u.into()), shown);
            assert_eq!(crate::engine::worker::redact_text(&format!("[rtsp] {u}: 401 Unauthorized")), format!("[rtsp] {shown}: 401 Unauthorized"));
            assert_eq!(crate::engine::worker::redact_text_with(u, true), shown);
        }
        assert_eq!(shown_login("rtsp://admin:12%2F34@192.0.2.5/x"), Some(("admin".into(), "12/34".into())));
        assert_eq!(shown_login("rtsp://admin:12/34@192.0.2.5/x"), Some(("admin".into(), "12/34".into())));
        // Plain addresses: untouched in logs. The report (strict) masks an
        // '@' path after a host:port too: it cannot tell it from a password.
        for u in ["rtsp://192.0.2.5:554/live@2", "rtsp://[fe80::1]/live@2", "rtsp://localhost:8554/cam@1", "rtsp://cam.local/x", "rtsp://h/x?mail=a@b.c", "rtsp://192.0.2.5/live@2"] {
            assert_eq!(crate::engine::worker::redact_text(u), u, "{u}");
        }
        assert_eq!(crate::engine::worker::redact_text_with("rtsp://192.0.2.5:554/live@2", true), "rtsp://2");
        assert_eq!(crate::engine::worker::redact_text_with("rtsp://192.0.2.5/live@2", true), "rtsp://192.0.2.5/live@2");
        // The shared cases: the connection rule still gives the same answers,
        // and wherever it finds a sign-in the shown rule finds the same one.
        let cases: Vec<serde_json::Value> = serde_json::from_str(include_str!("../../scripts/userinfo-cases.json")).unwrap();
        for c in cases {
            let rest = c["rest"].as_str().unwrap();
            if let Some((at, _)) = userinfo_at(rest) {
                assert_eq!(shown_userinfo_at(rest, false), Some(at), "{rest}");
                assert_eq!(shown_userinfo_at(rest, true), Some(at), "{rest}");
            }
        }
        // A '/', a space and a '#' together (the review's other string).
        assert_eq!(split_login("rtsp://echouser:Long/Pass word#1@192.0.2.5:554/main").1, Some(("echouser".into(), "Long/Pass word#1".into())));
        // '@', '?' and '#' raw in a password still work, as before.
        assert_eq!(split_login("rtsp://u:p@s?s#x@h/a").1, Some(("u".into(), "p@s?s#x".into())));
    }

    /// An address that worked in 0.10.2 (what comes before its first '/' is
    /// a host[:port] with no '@') keeps its meaning when normalized at start.
    #[test]
    fn normalize_keeps_addresses_that_worked_before() {
        for u in ["rtsp://camhost:554/live@2", "rtsp://localhost:8554/cam@1", "rtsp://[fe80::1]/live@2", "rtsp://192.0.2.5/x:y@z", "rtsp://admin:1234/5@192.0.2.5/main"] {
            let mut c = blank_cfg(u);
            c.sub_url = Some(u.into());
            normalize_creds(&mut c);
            assert_eq!((c.url.as_str(), c.username.as_str(), c.password.as_str(), c.sub_url.as_deref()), (u, "", "", Some(u)), "{u}");
        }
    }

    /// The same cases as scripts/test-camaddr.mjs runs against camaddr.js
    /// `userinfoAt`: both sides give the same answers.
    #[test]
    fn userinfo_cases_shared_with_the_web_form() {
        let cases: Vec<serde_json::Value> = serde_json::from_str(include_str!("../../scripts/userinfo-cases.json")).unwrap();
        assert!(cases.len() >= 20);
        for c in cases {
            let rest = c["rest"].as_str().unwrap();
            let got = userinfo_at(rest).map(|(at, end)| {
                let ui = &rest[..at];
                let (u, p) = ui.split_once(':').unwrap_or((ui, ""));
                (u.to_string(), p.to_string(), rest[at + 1..end].to_string())
            });
            let want = c["user"].as_str().map(|u| (u.to_string(), c["pass"].as_str().unwrap().to_string(), c["host"].as_str().unwrap().to_string()));
            assert_eq!(got, want, "{rest}");
        }
    }

    #[test]
    fn secret_query_names() {
        for k in [
            "password", "PWD", "loginuse", "loginpas", "loginpass", "pw", "psw", "pswd", "access_token", "secret", "session", "sid", "sig", "signature", "adminpass",
            "x_password", "x_pass", "cam-token", "camToken", "adminPwd", "deviceKey", "apiKey", "user_secret", "x_access_token",
        ] {
            assert!(is_secret_key(k), "{k}");
        }
        // An ending inside a word is not one.
        for k in ["channel", "subtype", "stream", "port", "app", "uid", "monkey", "bypass", "hotkey", "turnkey", "compass", "Monkey", "HOTKEY"] {
            assert!(!is_secret_key(k), "{k}");
        }
        assert_eq!(
            redact_url("http://192.0.2.9/videostream.cgi?loginuse=admin&loginpas=LoginPas777&access_token=t&resolution=32".into()),
            "http://192.0.2.9/videostream.cgi?loginuse=***&loginpas=***&access_token=***&resolution=32"
        );
    }

    #[test]
    fn redacts_password_in_query() {
        assert_eq!(
            redact_url("https://h/flv?port=1935&user=admin&password=secret&stream=x".into()),
            "https://h/flv?port=1935&user=***&password=***&stream=x"
        );
        assert_eq!(redact_url("rtsp://u:p@h/a".into()), "rtsp://h/a");
    }

    #[test]
    fn notification_settings_do_not_restart_the_stream() {
        let old = blank_cfg("rtsp://192.0.2.5/main");
        let mut new = old.clone();
        new.alert_labels = Some(vec!["person".into()]);
        new.alert_interval = Some(120);
        new.alert_schedule = Some("08:00-18:00".into());
        new.alert_channels = Some(vec!["ntfy".into()]);
        new.alert_priority = Some("high".into());
        new.groups = vec!["Outside".into()];
        // Camera controls and their actions next to alerts.
        new.controls = Some(crate::config::ControlsConfig { light_on_detect: true, siren_on_detect: true, http_port: Some(8443), ..Default::default() });
        assert!(!super::restart_needed(&old, &new));
        // The light fields are copied, nothing else.
        let mut live = old.clone();
        super::copy_light_fields(&mut live, &new);
        assert_eq!(live.alert_labels, Some(vec!["person".to_string()]));
        assert_eq!(live.groups, vec!["Outside".to_string()]);
        // Anything the worker holds still restarts it.
        for f in [
            (|c: &mut CameraConfig| c.url = "rtsp://192.0.2.6/main".into()) as fn(&mut CameraConfig),
            |c| c.short_name = "Door".into(),
            |c| c.alerts = Some(false),
            |c| c.sensitivity = Some(0.5),
            |c| c.password = "x".into(),
            |c| c.enabled = !c.enabled,
            |c| c.record = !c.record,
        ] {
            let mut n = new.clone();
            f(&mut n);
            assert!(super::restart_needed(&old, &n));
        }
    }
}
