//! Push notifications with the event picture: ntfy (including self-hosted
//! servers) and Pushover. Sent with the system curl, like the other alerts.

use std::path::Path;
use tracing::warn;

/// How urgent an alert is; each service maps it onto its own levels.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Priority {
    Low,
    Normal,
    High,
    Urgent,
}

impl Priority {
    pub fn parse(s: Option<&str>) -> Priority {
        match s.map(|x| x.trim().to_ascii_lowercase()).as_deref() {
            Some("low") => Priority::Low,
            Some("high") => Priority::High,
            Some("urgent") => Priority::Urgent,
            _ => Priority::Normal,
        }
    }
    /// ntfy priorities run 1 (min) to 5 (max).
    pub fn ntfy(self) -> u8 {
        match self {
            Priority::Low => 2,
            Priority::Normal => 3,
            Priority::High => 4,
            Priority::Urgent => 5,
        }
    }
    /// Pushover: -1 quiet, 0 normal, 1 high (bypasses quiet hours), 2 emergency
    /// (repeats until acknowledged).
    pub fn pushover(self) -> i8 {
        match self {
            Priority::Low => -1,
            Priority::Normal => 0,
            Priority::High => 1,
            Priority::Urgent => 2,
        }
    }
}

/// What a notification says and links to.
pub struct Note<'a> {
    pub title: &'a str,
    pub body: &'a str,
    pub tags: &'a [String],
    pub priority: Priority,
    /// Opened when the notification is tapped.
    pub click: Option<&'a str>,
    pub image: Option<&'a Path>,
}

fn enc(s: &str) -> String {
    url::form_urlencoded::byte_serialize(s.as_bytes()).collect()
}

/// ntfy query parameters for a message (works for both text and attachments,
/// and keeps non-ASCII text out of HTTP headers).
pub fn ntfy_url(topic_url: &str, n: &Note, filename: Option<&str>) -> String {
    let mut q = vec![format!("title={}", enc(n.title)), format!("priority={}", n.priority.ntfy())];
    if !n.tags.is_empty() {
        q.push(format!("tags={}", enc(&n.tags.join(","))));
    }
    if let Some(c) = n.click {
        q.push(format!("click={}", enc(c)));
    }
    if let Some(f) = filename {
        q.push(format!("filename={}", enc(f)));
        q.push(format!("message={}", enc(n.body)));
    }
    let sep = if topic_url.contains('?') { '&' } else { '?' };
    format!("{}{}{}", topic_url.trim_end_matches('/'), sep, q.join("&"))
}

/// Run curl; returns the HTTP status (0 when curl itself failed).
fn curl(args: &[String]) -> u16 {
    // Tokens and addresses go to curl on stdin, not on its command line.
    let mut all: Vec<String> = ["-s", "-o", if cfg!(windows) { "NUL" } else { "/dev/null" }, "-w", "%{http_code}", "-m", "20"].iter().map(|s| s.to_string()).collect();
    all.extend_from_slice(args);
    let out = crate::child::curl(&all);
    match out {
        Ok(o) => String::from_utf8_lossy(&o.stdout).trim().parse().unwrap_or(0),
        Err(_) => 0,
    }
}

/// Send to an ntfy topic, with the picture attached when there is one. A server
/// that does not take attachments gets the message as plain text instead.
pub fn send_ntfy(topic_url: &str, token: Option<&str>, n: &Note) -> bool {
    let auth: Vec<String> = token.filter(|t| !t.is_empty()).map(|t| vec!["-H".into(), format!("Authorization: Bearer {t}")]).unwrap_or_default();
    if let Some(img) = n.image {
        let mut a = auth.clone();
        a.extend(["-X".into(), "PUT".into(), "-H".into(), "Content-Type: image/jpeg".into(), "--data-binary".into(), format!("@{}", img.display())]);
        a.push(ntfy_url(topic_url, n, Some("snapshot.jpg")));
        let st = curl(&a);
        if (200..300).contains(&st) {
            return true;
        }
        warn!("ntfy: picture not accepted (HTTP {st}); sending text only");
    }
    let mut a = auth;
    a.extend(["-X".into(), "POST".into(), "-H".into(), "Content-Type: text/plain; charset=utf-8".into(), "--data-binary".into(), n.body.to_string()]);
    a.push(ntfy_url(topic_url, n, None));
    let st = curl(&a);
    if !(200..300).contains(&st) {
        warn!("ntfy: sending failed (HTTP {st})");
        return false;
    }
    true
}

pub struct Pushover<'a> {
    pub token: &'a str,
    pub user: &'a str,
    pub device: Option<&'a str>,
    pub sound: Option<&'a str>,
    pub api_url: &'a str,
}

/// Form fields for a Pushover message (text fields use --form-string so a
/// leading '@' or '<' is never read as a file).
pub fn pushover_args(p: &Pushover, n: &Note) -> Vec<String> {
    let mut a: Vec<String> = Vec::new();
    let mut s = |k: &str, v: &str| {
        a.push("--form-string".into());
        a.push(format!("{k}={v}"));
    };
    s("token", p.token);
    s("user", p.user);
    s("title", n.title);
    s("message", n.body);
    s("priority", &n.priority.pushover().to_string());
    if n.priority == Priority::Urgent {
        // Emergency priority repeats every minute for up to an hour until acknowledged.
        s("retry", "60");
        s("expire", "3600");
    }
    if let Some(d) = p.device.filter(|d| !d.is_empty()) {
        s("device", d);
    }
    if let Some(snd) = p.sound.filter(|d| !d.is_empty()) {
        s("sound", snd);
    }
    if let Some(c) = n.click {
        s("url", c);
        s("url_title", "Open in AmberSight");
    }
    if let Some(img) = n.image {
        // Pushover takes pictures up to 2.5 MB.
        if std::fs::metadata(img).map(|m| m.len() <= 2_500_000).unwrap_or(false) {
            a.push("-F".into());
            a.push(format!("attachment=@{};type=image/jpeg", img.display()));
        }
    }
    a.push(p.api_url.to_string());
    a
}

pub fn send_pushover(p: &Pushover, n: &Note) -> bool {
    let st = curl(&pushover_args(p, n));
    if !(200..300).contains(&st) {
        warn!("pushover: sending failed (HTTP {st})");
        return false;
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    fn note<'a>(img: Option<&'a Path>) -> Note<'a> {
        static TAGS: std::sync::OnceLock<Vec<String>> = std::sync::OnceLock::new();
        Note {
            title: "Person at Front Door",
            body: "Sun 3:24 AM · person 87% & more",
            tags: TAGS.get_or_init(|| vec!["person".into(), "camera".into()]),
            priority: Priority::High,
            click: Some("https://nvr.lan:8090/?alert=1&at=5"),
            image: img,
        }
    }

    #[test]
    fn ntfy_url_encodes_everything_in_the_query() {
        let u = ntfy_url("https://ntfy.home/cams", &note(None), None);
        assert!(u.starts_with("https://ntfy.home/cams?title=Person+at+Front+Door&priority=4"));
        assert!(u.contains("tags=person%2Ccamera"));
        assert!(u.contains("click=https%3A%2F%2Fnvr.lan%3A8090%2F%3Falert%3D1%26at%3D5"));
        assert!(!u.contains("message="), "text body goes in the request body");
        let a = ntfy_url("https://ntfy.home/cams?auth=x", &note(None), Some("snapshot.jpg"));
        assert!(a.contains("?auth=x&title="));
        assert!(a.contains("filename=snapshot.jpg") && a.contains("message=Sun+3%3A24+AM+%C2%B7+person+87%25+%26+more"));
    }

    #[test]
    fn priorities_map_per_service() {
        assert_eq!(Priority::parse(Some("URGENT")), Priority::Urgent);
        assert_eq!(Priority::parse(None), Priority::Normal);
        assert_eq!((Priority::Low.ntfy(), Priority::Urgent.ntfy()), (2, 5));
        assert_eq!((Priority::Low.pushover(), Priority::Urgent.pushover()), (-1, 2));
    }

    #[test]
    fn pushover_form_is_safe_and_complete() {
        let dir = std::env::temp_dir().join(format!("amber-po-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let img = dir.join("t.jpg");
        std::fs::write(&img, [0xffu8, 0xd8, 0xff]).unwrap();
        let p = Pushover { token: "@tok", user: "u1", device: Some("pixel"), sound: None, api_url: "https://api.pushover.net/1/messages.json" };
        let mut n = note(Some(&img));
        n.priority = Priority::Urgent;
        let a = pushover_args(&p, &n);
        let joined = a.join("\n");
        assert!(joined.contains("--form-string\ntoken=@tok"), "a leading @ stays text");
        assert!(joined.contains("priority=2") && joined.contains("retry=60") && joined.contains("expire=3600"));
        assert!(joined.contains("device=pixel") && joined.contains("url_title=Open in AmberSight"));
        assert!(joined.contains(&format!("-F\nattachment=@{};type=image/jpeg", img.display())));
        assert_eq!(a.last().unwrap(), "https://api.pushover.net/1/messages.json");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
