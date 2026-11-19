//! Reolink HTTP API (as documented by Reolink) for per-channel motion / AI
//! state and battery-camera sleep state. Needed for hubs and recorders:
//! their ONVIF events do not say which channel they belong to, while
//! `GetMdState` / `GetAiState` / `GetChannelstatus` are per channel.
//!
//! Devices use self-signed certificates on the LAN, so certificate checks
//! are off for this client (it only ever talks to the configured camera).

use anyhow::{anyhow, bail, Context, Result};
use serde_json::{json, Value};
use std::time::{Duration, Instant};

/// What one channel reports right now.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ChannelState {
    pub motion: bool,
    pub people: bool,
    pub vehicle: bool,
    pub animal: bool,
    /// Battery camera asleep (None when the device does not say).
    pub sleeping: Option<bool>,
    pub online: Option<bool>,
    pub name: Option<String>,
}

pub struct ReolinkClient {
    base: String,
    user: String,
    pass: String,
    token: Option<(String, Instant)>,
    agent: ureq::Agent,
    /// After a refused sign-in: no new sign-in before this (cameras lock an
    /// account after a few wrong ones, and refuse logins that come too often).
    login_blocked_until: Option<Instant>,
    /// For recording downloads: no total time limit (a big file takes a
    /// while), but each wait for the camera is limited.
    dl_agent: ureq::Agent,
}

/// Tokens last an hour; renew a little early.
const TOKEN_LIFE: Duration = Duration::from_secs(50 * 60);
/// The largest answer to a command (a search listing is the biggest: well under this).
const MAX_ANSWER: u64 = 2 << 20;
/// How long a refused sign-in keeps the client from signing in again.
pub const LOGIN_BACKOFF: Duration = Duration::from_secs(5 * 60);

impl ReolinkClient {
    /// `host` is an IP or name; HTTPS on port 443 is used (as Reolink hubs
    /// and cameras serve the API).
    pub fn new(host: &str, user: &str, pass: &str) -> Self {
        // Redirects are never followed: the camera's answer is the answer,
        // and a camera cannot send AmberSight to fetch from another host.
        let tls = ureq::tls::TlsConfig::builder().disable_verification(true).build();
        let agent = ureq::Agent::config_builder()
            .tls_config(tls)
            .timeout_connect(Some(Duration::from_secs(5)))
            .timeout_global(Some(Duration::from_secs(8)))
            .max_redirects(0)
            .http_status_as_error(false)
            .build()
            .new_agent();
        // Downloads: each wait for the camera is limited, and one request
        // (at most a megabyte) may take two minutes in all, about 9 KB/s, so
        // a slow Wi-Fi camera busy with its live streams still gets through.
        let tls = ureq::tls::TlsConfig::builder().disable_verification(true).build();
        let dl_agent = ureq::Agent::config_builder()
            .tls_config(tls)
            .timeout_connect(Some(Duration::from_secs(8)))
            .timeout_recv_response(Some(Duration::from_secs(20)))
            .timeout_per_read(Some(Duration::from_secs(20)))
            .timeout_recv_body(Some(Duration::from_secs(120)))
            .max_redirects(0)
            .http_status_as_error(false)
            .build()
            .new_agent();
        ReolinkClient { base: format!("https://{host}"), user: user.into(), pass: pass.into(), token: None, agent, login_blocked_until: None, dl_agent }
    }

    /// A client for a full base address ("https://host:port"), e.g. when
    /// the API is on another port or on plain HTTP.
    pub fn with_base(base: &str, user: &str, pass: &str) -> Self {
        let mut c = ReolinkClient::new("x", user, pass);
        c.base = base.trim_end_matches('/').to_string();
        c
    }

    /// Restart the camera (`Reboot`); re-signs-in once if the token expired.
    pub fn reboot(&mut self) -> Result<()> {
        for attempt in 0..2 {
            let t = self.login()?;
            let v = self.post("Reboot", Some(&t), &json!([{ "cmd": "Reboot", "action": 0, "param": {} }]))?;
            let r = &v[0];
            if r["code"].as_i64() == Some(0) {
                return Ok(());
            }
            if attempt == 0 && r["error"]["rspCode"].as_i64() == Some(-6) {
                self.token = None;
                continue;
            }
            bail!("the camera refused to restart: {}", error_text(r));
        }
        bail!("the camera keeps rejecting the sign-in token")
    }

    fn post(&self, cmd: &str, token: Option<&str>, body: &Value) -> Result<Value> {
        let mut url = format!("{}/cgi-bin/api.cgi?cmd={cmd}", self.base);
        if let Some(t) = token {
            url.push_str("&token=");
            url.push_str(&query_part(t));
        }
        let mut resp = self.agent.post(&url).send_json(body).map_err(|e| self.request_error(cmd, e))?;
        // (A command's answer is small: a camera sending megabytes is refused.)
        let v: Value = resp.body_mut().with_config().limit(MAX_ANSWER).read_json().map_err(|e| anyhow!("{cmd}: unexpected answer: {}", self.scrub(e.to_string())))?;
        Ok(v)
    }

    /// An error message safe to log and show: the sign-in token (as the
    /// camera gave it, or as written in an address) and any password in an
    /// address are taken out.
    fn scrub(&self, msg: String) -> String {
        let mut m = msg;
        if let Some((t, _)) = &self.token {
            if !t.is_empty() {
                m = m.replace(&query_part(t), "***").replace(t.as_str(), "***");
            }
        }
        crate::engine::redact_url(m)
    }

    /// A failed request as an error; one that never reached the camera's web
    /// server (nothing listening, no route, no answer to the connection) is
    /// marked [`Unreachable`].
    fn request_error(&self, what: &str, e: ureq::Error) -> anyhow::Error {
        use std::io::ErrorKind as K;
        let unreachable = match &e {
            ureq::Error::ConnectionFailed | ureq::Error::HostNotFound | ureq::Error::Timeout(ureq::Timeout::Connect) => true,
            ureq::Error::Io(io) => matches!(io.kind(), K::ConnectionRefused | K::TimedOut | K::HostUnreachable | K::NetworkUnreachable | K::AddrNotAvailable),
            _ => false,
        };
        let msg = format!("{what}: {}", self.scrub(e.to_string()));
        if unreachable {
            anyhow::Error::new(Unreachable(msg))
        } else {
            anyhow!(msg)
        }
    }

    fn login(&mut self) -> Result<String> {
        if let Some((t, at)) = &self.token {
            if at.elapsed() < TOKEN_LIFE {
                return Ok(t.clone());
            }
        }
        if let Some(until) = self.login_blocked_until {
            if Instant::now() < until {
                bail!("the camera refused the sign-in a moment ago; not trying again for a few minutes (so the account is not locked)");
            }
            self.login_blocked_until = None;
        }
        let body = json!([{ "cmd": "Login", "param": { "User": { "Version": "0", "userName": self.user, "password": self.pass } } }]);
        let v = self.post("Login", None, &body)?;
        let Some(t) = v[0]["value"]["Token"]["name"].as_str().map(str::to_string) else {
            // A wrong password, a locked account, too many logins: wait before
            // the next one (each retry would count against the account).
            if matches!(v[0]["error"]["rspCode"].as_i64(), Some(-7 | -105 | -502 | -503 | -506 | -507 | -5)) {
                self.login_blocked_until = Some(Instant::now() + LOGIN_BACKOFF);
            }
            bail!("sign-in to the camera failed: {}", error_text(&v[0]));
        };
        self.token = Some((t.clone(), Instant::now()));
        Ok(t)
    }

    /// Run several commands in one request; re-signs-in once if the token
    /// expired.
    pub fn batch(&mut self, cmds: &Value) -> Result<Vec<Value>> {
        for attempt in 0..2 {
            let t = self.login()?;
            let v = self.post("GetMdState", Some(&t), cmds)?;
            let arr = v.as_array().cloned().context("expected a list of answers")?;
            // -6 = "please login first" (token expired or evicted).
            if attempt == 0 && arr.iter().any(|r| r["error"]["rspCode"].as_i64() == Some(-6)) {
                self.token = None;
                continue;
            }
            return Ok(arr);
        }
        bail!("the camera keeps rejecting the sign-in token")
    }

    /// Motion / AI / sleep state of one channel (0-based).
    pub fn channel_state(&mut self, channel: u32) -> Result<ChannelState> {
        let cmds = json!([
            { "cmd": "GetMdState", "action": 0, "param": { "channel": channel } },
            { "cmd": "GetAiState", "action": 0, "param": { "channel": channel } },
            { "cmd": "GetChannelstatus", "action": 0, "param": {} }
        ]);
        let r = self.batch(&cmds)?;
        Ok(parse_state(&r, channel))
    }

    /// Best-effort sign-out (devices allow only a few sessions).
    pub fn logout(&mut self) {
        if let Some((t, _)) = self.token.take() {
            let _ = self.post("Logout", Some(&t), &json!([{ "cmd": "Logout", "param": {} }]));
        }
    }
}

/// A command the camera answered with an error code.
#[derive(Debug)]
pub struct CamError {
    pub cmd: String,
    /// Reolink's `rspCode`: -9 "not support", -26 "ability error" (this
    /// account may not), -6 "please login first", ...
    pub code: i64,
    pub detail: String,
}

impl CamError {
    /// The camera does not have this (as opposed to a refusal or a failure).
    pub fn unsupported(&self) -> bool {
        self.code == -9
    }
    /// The camera's account may not do this.
    pub fn not_allowed(&self) -> bool {
        self.code == -26
    }
}

impl std::fmt::Display for CamError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}: the camera answered {} ({})", self.cmd, self.code, self.detail)
    }
}

impl std::error::Error for CamError {}

/// The camera's own error in `e`, if it answered with one.
pub fn cam_error(e: &anyhow::Error) -> Option<&CamError> {
    e.chain().find_map(|c| c.downcast_ref::<CamError>())
}

/// A request that never reached the camera's web server at that address.
#[derive(Debug)]
pub struct Unreachable(String);

impl std::fmt::Display for Unreachable {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for Unreachable {}

/// Whether `e` says the address did not answer at all (rather than a
/// refused sign-in, a certificate or protocol problem, or a bad answer).
pub fn is_unreachable(e: &anyhow::Error) -> bool {
    e.chain().any(|c| c.is::<Unreachable>())
}

/// A value as written in an address's query (percent-encoded).
fn query_part(s: &str) -> String {
    percent_encoding::utf8_percent_encode(s, percent_encoding::NON_ALPHANUMERIC).to_string()
}

/// Most recordings one search may list: a two-hour hole holds about a
/// hundred; far more is a camera answering nonsense.
pub const MAX_SEARCH_FILES: usize = 2000;

/// The camera's own words for an error, made safe for a log line, the
/// audit and an API answer: no control characters (a line break would forge
/// a log line), no markup, at most a short line.
fn error_text(r: &Value) -> String {
    let raw = match r["error"]["detail"].as_str() {
        Some(d) => d.to_string(),
        None => r.to_string().chars().take(4096).collect(),
    };
    crate::onvif::scrub_device_text(&raw)
}

/// One recording on the camera's own card, from `Search`. Times are the
/// camera's own clock as it shows it (no zone).
#[derive(Debug, Clone, PartialEq)]
pub struct CardFile {
    pub name: String,
    pub start: chrono::NaiveDateTime,
    pub end: chrono::NaiveDateTime,
    pub size: u64,
    /// "main" or "sub".
    pub stream: String,
}

/// What the camera says about its card.
#[derive(Debug, Clone, PartialEq)]
pub enum CardState {
    /// A card (or disk) is in and mounted; capacity in MB.
    Present(u64),
    None,
}

fn naive_of(t: &Value) -> Option<chrono::NaiveDateTime> {
    let g = |k: &str| t[k].as_u64().map(|n| n as u32);
    chrono::NaiveDate::from_ymd_opt(g("year")? as i32, g("mon")?, g("day")?)?.and_hms_opt(g("hour")?, g("min")?, g("sec")?)
}

fn time_json(t: chrono::NaiveDateTime) -> Value {
    use chrono::{Datelike, Timelike};
    json!({ "year": t.year(), "mon": t.month(), "day": t.day(), "hour": t.hour(), "min": t.minute(), "sec": t.second() })
}

/// The recordings in a `Search` answer (`SearchResult.File`).
pub fn parse_search(answer: &Value) -> Vec<CardFile> {
    answer["value"]["SearchResult"]["File"]
        .as_array()
        .map(|l| {
            l.iter()
                .filter_map(|f| {
                    Some(CardFile {
                        name: f["name"].as_str()?.to_string(),
                        start: naive_of(&f["StartTime"])?,
                        end: naive_of(&f["EndTime"])?,
                        size: f["size"].as_str().and_then(|s| s.parse().ok()).or_else(|| f["size"].as_u64())?,
                        stream: f["type"].as_str().unwrap_or("main").to_string(),
                    })
                })
                .collect()
        })
        .unwrap_or_default()
}

impl ReolinkClient {
    /// One command by name; re-signs-in once if the token expired.
    fn call(&mut self, cmd: &str, action: u32, param: Value) -> Result<Value> {
        for attempt in 0..2 {
            let t = self.login()?;
            let v = self.post(cmd, Some(&t), &json!([{ "cmd": cmd, "action": action, "param": param }]))?;
            let r = v.get(0).cloned().ok_or_else(|| anyhow!("{cmd}: empty answer"))?;
            if attempt == 0 && r["error"]["rspCode"].as_i64() == Some(-6) {
                self.token = None;
                continue;
            }
            return Ok(r);
        }
        bail!("the camera keeps rejecting the sign-in token")
    }

    /// One command (`action` 1 also asks for the allowed ranges); the
    /// answer's object (`value`, and `range` when asked) on success, a
    /// [`CamError`] when the camera refused it.
    pub fn command(&mut self, cmd: &str, action: u32, param: Value) -> Result<Value> {
        let r = self.call(cmd, action, param)?;
        if r["code"].as_i64() == Some(0) {
            return Ok(r);
        }
        Err(anyhow::Error::new(CamError { cmd: cmd.to_string(), code: r["error"]["rspCode"].as_i64().unwrap_or(0), detail: error_text(&r) }))
    }

    /// The camera's clock as it shows it (local time, no zone).
    pub fn camera_clock(&mut self) -> Result<chrono::NaiveDateTime> {
        let r = self.call("GetTime", 0, json!({}))?;
        naive_of(&r["value"]["Time"]).ok_or_else(|| anyhow!("the camera did not give its time: {}", error_text(&r)))
    }

    /// Whether the camera has a card in.
    pub fn card_state(&mut self) -> Result<CardState> {
        let r = self.call("GetHddInfo", 0, json!({}))?;
        let Some(list) = r["value"]["HddInfo"].as_array() else { bail!("the camera did not report its card: {}", error_text(&r)) };
        Ok(match list.iter().find(|d| d["mount"].as_i64() == Some(1) && d["capacity"].as_u64().unwrap_or(0) > 0) {
            Some(d) => CardState::Present(d["capacity"].as_u64().unwrap_or(0)),
            None => CardState::None,
        })
    }

    /// The card's recordings of `stream` ("main" or "sub") that touch
    /// [from, to] (camera clock), oldest first. A window over midnight is
    /// asked one day at a time.
    pub fn search(&mut self, channel: u32, stream: &str, from: chrono::NaiveDateTime, to: chrono::NaiveDateTime) -> Result<Vec<CardFile>> {
        let mut out: Vec<CardFile> = Vec::new();
        let mut names: std::collections::HashSet<String> = std::collections::HashSet::new();
        let mut day = from.date();
        while day <= to.date() {
            let a = if day == from.date() { from } else { day.and_hms_opt(0, 0, 0).unwrap() };
            let b = if day == to.date() { to } else { day.and_hms_opt(23, 59, 59).unwrap() };
            let r = self.call(
                "Search",
                1,
                json!({ "Search": { "channel": channel, "onlyStatus": 0, "streamType": stream, "StartTime": time_json(a), "EndTime": time_json(b) } }),
            )?;
            if r["code"].as_i64() != Some(0) {
                // "rcv failed" (-17) is what a camera without a card answers.
                bail!("the camera could not list its recordings: {}", error_text(&r));
            }
            let listed = parse_search(&r);
            if listed.len() > MAX_SEARCH_FILES {
                bail!("the camera listed {} recordings for a short time, far more than it can hold", listed.len());
            }
            for f in listed {
                if names.insert(f.name.clone()) {
                    out.push(f);
                }
            }
            if out.len() > MAX_SEARCH_FILES {
                bail!("the camera listed {} recordings for a short time, far more than it can hold", out.len());
            }
            day = day.succ_opt().ok_or_else(|| anyhow!("date overflow"))?;
        }
        out.sort_by_key(|f| f.start);
        Ok(out)
    }

    /// Bytes of a card recording: all of it, or `range` (first, last
    /// inclusive).
    pub fn download(&mut self, name: &str, range: Option<(u64, u64)>) -> Result<Vec<u8>> {
        use std::io::Read;
        let t = self.login()?;
        let leaf = name.rsplit('/').next().unwrap_or(name);
        let url = format!("{}/cgi-bin/api.cgi?cmd=Download&source={}&output={}&token={}", self.base, query_part(name), query_part(leaf), query_part(&t));
        let mut req = self.dl_agent.get(&url);
        if let Some((a, b)) = range {
            req = req.header("Range", &format!("bytes={a}-{b}"));
        }
        let mut resp = req.call().map_err(|e| self.request_error("download", e))?;
        let status = resp.status().as_u16();
        if status != 200 && status != 206 {
            self.token = None;
            bail!("the camera refused the download (HTTP {status})");
        }
        if range.is_some() && status == 200 {
            // A camera that ignores Range sends the whole file: not what was asked.
            bail!("the camera does not serve part of a recording");
        }
        let limit = range.map(|(a, b)| b.saturating_sub(a).saturating_add(1)).unwrap_or(2 << 30);
        let mut body = Vec::new();
        resp.body_mut().with_config().limit(limit.saturating_add(1)).reader().read_to_end(&mut body).map_err(|e| anyhow!("download: {}", self.scrub(e.to_string())))?;
        if body.len() as u64 > limit {
            bail!("the camera sent more than was asked");
        }
        Ok(body)
    }
}

/// Pull the channel's state out of a batch answer.
pub fn parse_state(answers: &[Value], channel: u32) -> ChannelState {
    let mut st = ChannelState::default();
    for a in answers {
        let v = &a["value"];
        match a["cmd"].as_str().unwrap_or("") {
            "GetMdState" => st.motion = v["state"].as_i64() == Some(1),
            "GetAiState" => {
                let on = |k: &str| v[k]["alarm_state"].as_i64() == Some(1);
                st.people = on("people") || on("face");
                st.vehicle = on("vehicle");
                st.animal = on("dog_cat");
            }
            "GetChannelstatus" => {
                if let Some(ch) = v["status"].as_array().and_then(|l| l.iter().find(|c| c["channel"].as_u64() == Some(channel as u64))) {
                    st.sleeping = ch["sleep"].as_i64().map(|s| s == 1);
                    st.online = ch["online"].as_i64().map(|s| s == 1);
                    st.name = ch["name"].as_str().filter(|n| !n.is_empty()).map(|n| n.to_string());
                }
            }
            _ => {}
        }
    }
    st
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_hub_answers_per_channel() {
        // Shapes as returned by the Reolink Home Hub on the LAN.
        let answers: Vec<Value> = serde_json::from_value(json!([
            { "cmd": "GetMdState", "code": 0, "value": { "state": 1 } },
            { "cmd": "GetAiState", "code": 0, "value": { "channel": 0,
                "dog_cat": { "alarm_state": 0, "support": 1 },
                "face": { "alarm_state": 0, "support": 0 },
                "people": { "alarm_state": 1, "support": 1 },
                "vehicle": { "alarm_state": 0, "support": 1 } } },
            { "cmd": "GetChannelstatus", "code": 0, "value": { "count": 8, "status": [
                { "channel": 0, "name": "Garage Side", "online": 1, "sleep": 1, "uid": "x" },
                { "channel": 1, "name": "Back Porch", "online": 1, "sleep": 0, "uid": "y" } ] } }
        ]))
        .unwrap();
        let s = parse_state(&answers, 0);
        assert!(s.motion && s.people && !s.vehicle && !s.animal);
        assert_eq!(s.sleeping, Some(true));
        assert_eq!(s.name.as_deref(), Some("Garage Side"));
        let s1 = parse_state(&answers, 1);
        assert_eq!(s1.sleeping, Some(false));
        assert_eq!(s1.name.as_deref(), Some("Back Porch"));
    }

    /// Answers every request on a local port with `reply` (a whole HTTP
    /// response); returns the port and how many requests came.
    fn serve(reply: String) -> (u16, std::sync::Arc<std::sync::atomic::AtomicUsize>) {
        use std::io::Write;
        let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = l.local_addr().unwrap().port();
        let n = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let count = n.clone();
        std::thread::spawn(move || {
            for s in l.incoming() {
                let Ok(mut s) = s else { break };
                count.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                // The whole request, body too (an unread body turns the close into a reset on Windows).
                let _ = crate::test_ports::read_http_request(&mut s);
                std::thread::sleep(Duration::from_millis(20));
                let _ = s.write_all(reply.as_bytes());
            }
        });
        (port, n)
    }

    #[test]
    fn a_camera_cannot_redirect_the_client_elsewhere() {
        let (other, hits) = serve("HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\n[]".into());
        let (cam, _) = serve(format!("HTTP/1.1 302 Found\r\nLocation: http://127.0.0.1:{other}/x\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"));
        let mut c = ReolinkClient::with_base(&format!("http://127.0.0.1:{cam}"), "u", "p");
        assert!(c.camera_clock().is_err());
        assert!(c.download("a.mp4", Some((0, 9))).is_err());
        std::thread::sleep(Duration::from_millis(100));
        assert_eq!(hits.load(std::sync::atomic::Ordering::SeqCst), 0, "followed the redirect");
    }

    #[test]
    fn errors_never_carry_the_sign_in_token() {
        let mut c = ReolinkClient::with_base("http://127.0.0.1:9", "u", "p");
        c.token = Some(("a b&c/d".into(), Instant::now()));
        let msg = format!("download: bad uri http://127.0.0.1:9/cgi-bin/api.cgi?cmd=Download&source=x&token={}", query_part("a b&c/d"));
        let s = c.scrub(msg.clone());
        assert!(!s.contains("a%20b") && !s.contains("a b&c"), "{s}");
        assert!(c.scrub("token a b&c/d in text".into()).contains("***"));
        // The token goes into an address percent-encoded.
        assert_eq!(query_part("a b&c/d"), "a%20b%26c%2Fd");
    }

    #[test]
    fn a_cameras_error_text_cannot_forge_log_lines_or_flood() {
        let forged = "bad\n2026-11-18T03:00:00Z  INFO auth: admin signed in\r\u{1b}[31m";
        let t = error_text(&json!({ "error": { "detail": forged, "rspCode": -1 } }));
        assert!(!t.contains('\n') && !t.contains('\r') && !t.contains('\u{1b}'), "{t:?}");
        let huge = "A".repeat(8 << 20);
        let t = error_text(&json!({ "error": { "detail": huge, "rspCode": -1 } }));
        assert!(t.chars().count() <= 200, "{}", t.len());
        let t = error_text(&json!({ "no": "detail", "x": "y".repeat(100_000) }));
        assert!(t.chars().count() <= 200);
        // An answer of megabytes is refused, not read.
        let body = format!("[{{\"cmd\":\"GetTime\",\"code\":1,\"error\":{{\"rspCode\":-1,\"detail\":\"{}\"}}}}]", "A".repeat(3 << 20));
        let (cam, _) = serve(format!("HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len()));
        let mut c = ReolinkClient::with_base(&format!("http://127.0.0.1:{cam}"), "u", "p");
        c.token = Some(("tok".into(), Instant::now()));
        let e = format!("{:#}", c.camera_clock().unwrap_err());
        assert!(e.len() < 1000, "{}", e.len());
    }

    #[test]
    fn a_closed_port_is_unreachable_a_refusal_is_not() {
        let closed = crate::test_ports::closed_port();
        let mut c = ReolinkClient::with_base(&format!("http://127.0.0.1:{}", closed.port), "u", "p");
        let e = c.camera_clock().unwrap_err();
        assert!(is_unreachable(&e), "{e:#}");
        let body = r#"[{"cmd":"Login","code":1,"error":{"detail":"login failed","rspCode":-7}}]"#;
        let (cam, _) = serve(format!("HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len()));
        let mut c = ReolinkClient::with_base(&format!("http://127.0.0.1:{cam}"), "u", "p");
        let e = c.camera_clock().unwrap_err();
        assert!(!is_unreachable(&e), "{e:#}");
    }

    #[test]
    fn a_search_listing_more_than_a_card_could_hold_is_refused() {
        let t = |s: u32| json!({"year": 2026, "mon": 11, "day": 16, "hour": 3, "min": s / 60, "sec": s % 60});
        let files: Vec<Value> = (0..(MAX_SEARCH_FILES as u32 + 1)).map(|i| json!({"name": format!("f{i}.mp4"), "StartTime": t(i % 3000), "EndTime": t(i % 3000), "size": "1", "type": "main"})).collect();
        let body = json!([{"cmd": "Search", "code": 0, "value": {"SearchResult": {"File": files, "channel": 0}}}]).to_string();
        let (cam, _) = serve(format!("HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len()));
        let mut c = ReolinkClient::with_base(&format!("http://127.0.0.1:{cam}"), "u", "p");
        c.token = Some(("tok".into(), Instant::now()));
        let day = chrono::NaiveDate::from_ymd_opt(2026, 11, 16).unwrap();
        let r = c.search(0, "main", day.and_hms_opt(3, 0, 0).unwrap(), day.and_hms_opt(4, 0, 0).unwrap());
        assert!(r.is_err());
    }
}
