//! Who is using this server right now (Status > Connections): one entry per
//! user + address + browser, fed by the auth middleware, kept in memory only.

use std::collections::HashMap;
use std::net::IpAddr;
use std::sync::Mutex;

/// Entries not seen for this long are dropped.
const KEEP_SECS: i64 = 3600;

#[derive(Clone)]
struct Conn {
    user: String,
    ip: IpAddr,
    agent: String,
    first: i64,
    last: i64,
    requests: u64,
    /// Camera id of the last live picture asked for, and when.
    live: Option<(usize, i64)>,
}

fn map() -> &'static Mutex<HashMap<(String, IpAddr, String), Conn>> {
    static M: std::sync::OnceLock<Mutex<HashMap<(String, IpAddr, String), Conn>>> = std::sync::OnceLock::new();
    M.get_or_init(Default::default)
}

/// A short "Browser on OS" from a User-Agent header.
pub fn short_agent(ua: &str) -> String {
    let browser = if ua.contains("AmberSight-Capture") {
        "Capture helper"
    } else if ua.contains("Edg/") {
        "Edge"
    } else if ua.contains("OPR/") {
        "Opera"
    } else if ua.contains("Firefox/") {
        "Firefox"
    } else if ua.contains("Chrome/") || ua.contains("CriOS/") {
        "Chrome"
    } else if ua.contains("Safari/") {
        "Safari"
    } else if ua.starts_with("curl/") {
        "curl"
    } else if ua.is_empty() {
        "Unknown"
    } else {
        "Other app"
    };
    let os = if ua.contains("Android") {
        Some("Android")
    } else if ua.contains("iPhone") || ua.contains("iPad") {
        Some("iOS")
    } else if ua.contains("Windows") {
        Some("Windows")
    } else if ua.contains("Mac OS X") || ua.contains("Macintosh") {
        Some("macOS")
    } else if ua.contains("CrOS") {
        Some("ChromeOS")
    } else if ua.contains("Linux") {
        Some("Linux")
    } else {
        None
    };
    match os {
        Some(o) => format!("{browser} on {o}"),
        None => browser.to_string(),
    }
}

/// Note one API request.
pub fn seen(user: &str, ip: IpAddr, ua: &str, path: &str) {
    let now = chrono::Utc::now().timestamp();
    let agent = short_agent(ua);
    let live = path
        .strip_prefix("/live/")
        .and_then(|r| r.split('/').next())
        .and_then(|id| id.parse::<usize>().ok());
    let mut m = map().lock().unwrap();
    if m.len() > 500 {
        m.retain(|_, c| now - c.last < KEEP_SECS);
    }
    let c = m.entry((user.to_string(), ip, agent.clone())).or_insert_with(|| Conn {
        user: user.to_string(),
        ip,
        agent,
        first: now,
        last: now,
        requests: 0,
        live: None,
    });
    c.last = now;
    c.requests += 1;
    if let Some(id) = live {
        c.live = Some((id, now));
    }
}

/// Connections seen in the last hour, most recent first. `camera_name` turns
/// a camera id into its name (None hides that camera).
pub fn list(camera_name: &dyn Fn(usize) -> Option<String>) -> Vec<serde_json::Value> {
    let now = chrono::Utc::now().timestamp();
    let mut v: Vec<Conn> = map().lock().unwrap().values().filter(|c| now - c.last < KEEP_SECS).cloned().collect();
    v.sort_by_key(|c| std::cmp::Reverse(c.last));
    v.into_iter()
        .map(|c| {
            let watching = c.live.filter(|(_, t)| now - t < 30).and_then(|(id, _)| camera_name(id));
            serde_json::json!({
                "user": c.user,
                "ip": c.ip.to_string(),
                "agent": c.agent,
                "first": c.first,
                "last": c.last,
                "requests": c.requests,
                "active": now - c.last < 90,
                "watching": watching,
            })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn agents() {
        assert_eq!(short_agent("Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/130.0 Safari/537.36 Edg/130.0"), "Edge on Windows");
        assert_eq!(short_agent("Mozilla/5.0 (Linux; Android 16; Pixel 10) AppleWebKit/537.36 Chrome/130.0 Mobile Safari/537.36"), "Chrome on Android");
        assert_eq!(short_agent("Mozilla/5.0 (iPhone; CPU iPhone OS 18_0 like Mac OS X) AppleWebKit/605.1.15 Version/18.0 Mobile/15E148 Safari/604.1"), "Safari on iOS");
        assert_eq!(short_agent("Mozilla/5.0 (X11; Linux x86_64; rv:140.0) Gecko/20100101 Firefox/140.0"), "Firefox on Linux");
        assert_eq!(short_agent(""), "Unknown");
    }

    #[test]
    fn tracks_and_names_live_views() {
        let ip: IpAddr = "10.9.8.7".parse().unwrap();
        seen("conn-test", ip, "curl/8", "/live/3/ws");
        seen("conn-test", ip, "curl/8", "/events");
        let l = list(&|id| (id == 3).then(|| "Porch".to_string()));
        let me = l.iter().find(|c| c["user"] == "conn-test").unwrap();
        assert_eq!(me["requests"], 2);
        assert_eq!(me["watching"], "Porch");
        assert_eq!(me["agent"], "curl");
    }
}
