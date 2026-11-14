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
    let (authority, path) = match rest.find('/') {
        Some(i) => (&rest[..i], &rest[i..]),
        None => (rest, ""),
    };
    let at = authority.rfind('@')?;
    Some((
        format!("{}://", scheme),
        authority[..at].to_string(),
        format!("{}{}", &authority[at + 1..], path),
    ))
}

/// Display-safe URL for logs: strips userinfo even if normalization missed.
pub fn redact_url(url: String) -> String {
    let url = match split_userinfo(&url) {
        Some((head, _, rest)) => format!("{}{}", head, rest),
        None => url,
    };
    redact_query_password(url)
}

/// Query names whose values are secrets.
const SECRET_KEYS: [&str; 9] = ["password", "pwd", "pass", "passwd", "token", "key", "apikey", "api_key", "auth"];

/// Hide `password=` / `pwd=` (and token, key, ...) query values (HTTP-FLV and snapshot URLs
/// carry device sign-in in the query).
fn redact_query_password(url: String) -> String {
    let Some((base, query)) = url.split_once('?') else { return url };
    let parts: Vec<String> = query
        .split('&')
        .map(|kv| match kv.split_once('=') {
            Some((k, _)) if SECRET_KEYS.iter().any(|s| k.eq_ignore_ascii_case(s)) => format!("{k}=***"),
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn creds_normalize_trim_and_redact() {
        let mut c = CameraConfig {
            name: "  Front Door  ".to_string(),
            url: " rtsp://admin:secret99@10.0.0.5:554/main ".to_string(),
            sub_url: None, ingest: "auto".to_string(), hwaccel: None, folder: None, copy_events_to: None, copy_events_keep_days: 0, copy_events_only_confirmed: false, min_object: None, contrast: None, ai: None, ai_labels: None, onvif_port: None, trigger: "video".to_string(), on_demand: false, idle_timeout: 300, onvif_source: None, reolink_channel: None, uid: None, privacy_masks: Vec::new(), zone_polys: Vec::new(), ignore_polys: Vec::new(), named_zones: Vec::new(), groups: Vec::new(), motion_algorithm: None, max_object: None, min_travel: None, min_frames: None, break_time: None, pre_roll: None, post_roll: None, alerts: None, alert_channels: None, alert_interval: None, alert_schedule: None, alert_labels: None, alert_ignore_polys: Vec::new(), no_motion_minutes: None, no_motion_schedule: None, audio_trigger: false, audio_margin_db: None, audio_min_db: None, audio_sounds: Vec::new(), ai_min_score: None, also_trigger: Vec::new(), alert_priority: None, capture: None, watchdog: None, share: false, share_unmasked: false, strangers: None, tamper: None, unusual: None,
            username: "".to_string(),
            password: "".to_string(),
            short_name: "".to_string(),
            group: "".to_string(),
            schedule: None,
            fps: 10, scale: 640, record: true, motion: true, enabled: true,
            direct: false, overlay: false, overlay_text: None, overlay_pos: None, ai_ignore_parked: None, ai_strict: None, ai_hide_unconfirmed: None, ai_confirm_camera: None, packages: None, audio: false, record_motion_only: false, record_sub: false, sensitivity: None,
            zones: Vec::new(), ignore_zones: Vec::new(),
        };
        normalize_creds(&mut c);
        assert!(c.name == "Front Door");
        assert!(c.url == "rtsp://10.0.0.5:554/main");
        assert!(c.username == "admin");
        assert!(c.password == "secret99");
        assert!(!c.url.contains("secret99"));
        assert!(redact_url("rtsp://u:p@h/x".to_string()) == "rtsp://h/x");
        assert!(camera_url(&c) == "rtsp://admin:secret99@10.0.0.5:554/main");
    }

    fn blank_cfg(url: &str) -> CameraConfig {
        CameraConfig {
            name: "c".to_string(), url: url.to_string(), sub_url: None, ingest: "auto".to_string(), hwaccel: None, folder: None, copy_events_to: None, copy_events_keep_days: 0, copy_events_only_confirmed: false, min_object: None, contrast: None, ai: None, ai_labels: None, onvif_port: None, trigger: "video".to_string(), on_demand: false, idle_timeout: 300, onvif_source: None, reolink_channel: None, uid: None, privacy_masks: Vec::new(), zone_polys: Vec::new(), ignore_polys: Vec::new(), named_zones: Vec::new(), groups: Vec::new(), motion_algorithm: None, max_object: None, min_travel: None, min_frames: None, break_time: None, pre_roll: None, post_roll: None, alerts: None, alert_channels: None, alert_interval: None, alert_schedule: None, alert_labels: None, alert_ignore_polys: Vec::new(), no_motion_minutes: None, no_motion_schedule: None, audio_trigger: false, audio_margin_db: None, audio_min_db: None, audio_sounds: Vec::new(), ai_min_score: None, also_trigger: Vec::new(), alert_priority: None, capture: None, watchdog: None, share: false, share_unmasked: false, strangers: None, tamper: None, unusual: None, username: "".to_string(), password: "".to_string(),
            short_name: "".to_string(), group: "".to_string(), schedule: None,
            fps: 10, scale: 640, record: true, motion: true, enabled: true,
            direct: false, overlay: false, overlay_text: None, overlay_pos: None, ai_ignore_parked: None, ai_strict: None, ai_hide_unconfirmed: None, ai_confirm_camera: None, packages: None, audio: false, record_motion_only: false, record_sub: false, sensitivity: None,
            zones: Vec::new(), ignore_zones: Vec::new(),
        }
    }

    #[test]
    fn creds_special_chars_roundtrip() {
        // Raw (unencoded) '@' in the password is tolerated: split at last '@'.
        // '/' in a raw password would end the authority, so it is encoded here.
        let mut c = blank_cfg("rtsp://adm:p@ss:w%2Frd%23%3F%25@10.0.0.5:554/main");
        normalize_creds(&mut c);
        assert_eq!(c.username, "adm");
        assert_eq!(c.password, "p@ss:w/rd#?%");
        assert_eq!(c.url, "rtsp://10.0.0.5:554/main");
        let built = camera_url(&c);
        assert_eq!(built, "rtsp://adm:p%40ss%3Aw%2Frd%23%3F%25@10.0.0.5:554/main");
        // Parseable: last '@' separates host, first ':' separates user/pass.
        let (_, ui, host) = split_userinfo(&built).unwrap();
        assert_eq!(host, "10.0.0.5:554/main");
        let (u, p) = ui.split_once(':').unwrap();
        assert_eq!(pct_decode(u), "adm");
        assert_eq!(pct_decode(p), "p@ss:w/rd#?%");
        // Round trip again through normalize.
        let mut c2 = blank_cfg(&built);
        normalize_creds(&mut c2);
        assert_eq!(c2.password, "p@ss:w/rd#?%");
    }

    #[test]
    fn camera_url_injects_when_path_has_at() {
        let mut c = blank_cfg("rtsp://10.0.0.5/live@main");
        c.username = "u".to_string();
        c.password = "p".to_string();
        assert_eq!(camera_url(&c), "rtsp://u:p@10.0.0.5/live@main");
    }

    #[test]
    fn redact_handles_at_in_password() {
        assert_eq!(redact_url("rtsp://u:p@ss@host:554/x".to_string()), "rtsp://host:554/x");
        assert_eq!(redact_url("rtsp://u:p%40ss@host/x".to_string()), "rtsp://host/x");
        assert_eq!(redact_url("rtsp://host/x@y".to_string()), "rtsp://host/x@y");
    }

    #[test]
    fn redacts_password_in_query() {
        assert_eq!(
            redact_url("https://h/flv?port=1935&user=admin&password=secret&stream=x".into()),
            "https://h/flv?port=1935&user=admin&password=***&stream=x"
        );
        assert_eq!(redact_url("rtsp://u:p@h/a".into()), "rtsp://h/a");
    }

    #[test]
    fn notification_settings_do_not_restart_the_stream() {
        let old = blank_cfg("rtsp://10.0.0.5/main");
        let mut new = old.clone();
        new.alert_labels = Some(vec!["person".into()]);
        new.alert_interval = Some(120);
        new.alert_schedule = Some("08:00-18:00".into());
        new.alert_channels = Some(vec!["ntfy".into()]);
        new.alert_priority = Some("high".into());
        new.groups = vec!["Outside".into()];
        assert!(!super::restart_needed(&old, &new));
        // The light fields are copied, nothing else.
        let mut live = old.clone();
        super::copy_light_fields(&mut live, &new);
        assert_eq!(live.alert_labels, Some(vec!["person".to_string()]));
        assert_eq!(live.groups, vec!["Outside".to_string()]);
        // Anything the worker holds still restarts it.
        for f in [
            (|c: &mut CameraConfig| c.url = "rtsp://10.0.0.6/main".into()) as fn(&mut CameraConfig),
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
