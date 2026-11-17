//! The camera report: `GET /api/cameras/:id/report` (admin). One text file
//! with what a bug report about a camera needs: this server (version, system,
//! how it is installed, ffmpeg), the camera's settings without sign-in, how
//! it is read and why, its recent errors and log lines, the SDP of an RTSP
//! DESCRIBE of each stream, an ffmpeg probe of each stream and its ONVIF
//! profiles.
//!
//! The file is a readable summary followed by the same facts as JSON (for
//! turning a user's camera into a test profile). User names, passwords,
//! stream keys and secret query values are removed from every part, and a
//! last pass looks for the camera's actual sign-in (as typed, percent-encoded,
//! JSON- and XML-escaped) in the whole file.

use crate::config::CameraConfig;
use base64::Engine as _;
use serde_json::{json, Value};
use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

/// The whole report takes at most this long.
pub const TOTAL: Duration = Duration::from_secs(20);
const DESCRIBE_TIMEOUT: Duration = Duration::from_secs(5);
const PROBE_TIMEOUT: Duration = Duration::from_secs(7);
const ONVIF_TIMEOUT: Duration = Duration::from_secs(12);
const LOG_LINES: usize = 50;
const ENDS_KEPT: usize = 10;
/// Failed connections in a row before the log points at the report.
const HINT_AFTER: u32 = 5;

// ---------------------------------------------------------------------------
// What the camera readers tell us while they run
// ---------------------------------------------------------------------------

#[derive(Default)]
struct Notes {
    /// Why the stream ended, newest last: (epoch s, reason).
    ends: VecDeque<(i64, String)>,
    /// Why the built-in reader handed the camera to ffmpeg.
    fallback: Option<(i64, String)>,
    failures: u32,
    hinted: bool,
}

fn notes() -> &'static Mutex<HashMap<String, Notes>> {
    static N: OnceLock<Mutex<HashMap<String, Notes>>> = OnceLock::new();
    N.get_or_init(Default::default)
}

/// A camera's stream ended (or did not start) for `why`. `played`: it sent
/// video this time, which starts the count of failures over. After a few
/// failures in a row, the log says once per camera (per run) where the
/// camera report is.
pub fn note_end(name: &str, why: &str, played: bool) {
    let hint = {
        let mut all = notes().lock().unwrap();
        let n = all.entry(name.to_string()).or_default();
        if n.ends.len() == ENDS_KEPT {
            n.ends.pop_front();
        }
        n.ends.push_back((chrono::Utc::now().timestamp(), crate::engine::redact_text(why)));
        n.failures = if played { 0 } else { n.failures + 1 };
        let hint = n.failures >= HINT_AFTER && !n.hinted;
        n.hinted |= hint;
        hint
    };
    if hint {
        tracing::info!("camera '{name}': keeps failing to connect. {}", hint_text(name));
    }
}

/// Where to find the report, for the log.
pub fn hint_text(name: &str) -> String {
    format!("Settings > Cameras > {name} > Camera report collects details for a bug report")
}

/// The built-in reader gave up on the camera and ffmpeg reads it now.
pub fn note_fallback(name: &str, why: &str) {
    let mut all = notes().lock().unwrap();
    all.entry(name.to_string()).or_default().fallback = Some((chrono::Utc::now().timestamp(), crate::engine::redact_text(why)));
}

/// Why the built-in reader last gave up on this camera (since the server started).
pub fn fallback_reason(name: &str) -> Option<String> {
    notes().lock().unwrap().get(name).and_then(|n| n.fallback.as_ref().map(|f| f.1.clone()))
}

fn recorded(name: &str) -> (Vec<(i64, String)>, Option<(i64, String)>) {
    let all = notes().lock().unwrap();
    match all.get(name) {
        Some(n) => (n.ends.iter().cloned().collect(), n.fallback.clone()),
        None => (Vec::new(), None),
    }
}

// ---------------------------------------------------------------------------
// One report per camera, two at a time
// ---------------------------------------------------------------------------

/// Reports at most at once. An ONVIF lookup cannot be stopped (it runs on a
/// blocking thread), so one still going after its report ended counts too.
const MAX_AT_ONCE: usize = 2;

#[derive(Default)]
struct Running {
    /// Cameras whose report is being made.
    reports: HashSet<usize>,
    /// ONVIF lookups still going (each report makes one at most; one may
    /// outlive its report).
    onvif: usize,
}

impl Running {
    /// Two reports, or two ONVIF lookups, fill it.
    fn full(&self) -> bool {
        self.reports.len().max(self.onvif) >= MAX_AT_ONCE
    }
}

fn running() -> &'static Mutex<Running> {
    static R: OnceLock<Mutex<Running>> = OnceLock::new();
    R.get_or_init(Default::default)
}

/// Held while a camera's report is made.
pub struct Busy(usize);

impl Drop for Busy {
    fn drop(&mut self) {
        running().lock().unwrap().reports.remove(&self.0);
    }
}

/// None when a report for this camera is being made already, or two reports
/// (or two ONVIF lookups left from reports) are going.
pub fn begin(id: usize) -> Option<Busy> {
    let mut r = running().lock().unwrap();
    if r.reports.contains(&id) || r.full() {
        return None;
    }
    r.reports.insert(id);
    Some(Busy(id))
}

/// Held by an ONVIF lookup's thread until the lookup ends.
struct OnvifBusy;

impl OnvifBusy {
    fn new() -> Self {
        running().lock().unwrap().onvif += 1;
        OnvifBusy
    }
}

impl Drop for OnvifBusy {
    fn drop(&mut self) {
        running().lock().unwrap().onvif -= 1;
    }
}

// ---------------------------------------------------------------------------
// Removing sign-in details
// ---------------------------------------------------------------------------

/// Whether a query (or `key=value`) name's value is removed in the report:
/// those the log hides (`creds::is_secret_key`) and a battery camera's UID.
fn secret_key(k: &str) -> bool {
    crate::engine::is_secret_key(k) || k.eq_ignore_ascii_case("uid")
}

/// Unreserved characters stay; everything else is percent-encoded.
const ENC: &percent_encoding::AsciiSet = &percent_encoding::NON_ALPHANUMERIC.remove(b'-').remove(b'.').remove(b'_').remove(b'~');
/// The same with spaces left as they are (form encoding turns them into '+').
const FORM: &percent_encoding::AsciiSet = &ENC.remove(b' ');

/// The ways a secret can appear in text.
fn variants(s: &str) -> Vec<String> {
    let upper = percent_encoding::utf8_percent_encode(s, ENC).to_string();
    let lower = lower_hex(&upper);
    let form = percent_encoding::utf8_percent_encode(s, FORM).to_string().replace(' ', "+");
    let xml = s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;").replace('"', "&quot;").replace('\'', "&apos;");
    // As a camera may echo it back: only spaces encoded (retina's a=control), base64.
    let spaces = s.replace(' ', "%20");
    let b64 = base64::engine::general_purpose::STANDARD.encode(s);
    let mut out = vec![s.to_string(), upper, lower, form.clone(), lower_hex(&form), xml, spaces, b64.trim_end_matches('=').to_string(), b64];
    let json: Vec<String> = out.iter().filter_map(|v| serde_json::to_string(v).ok().map(|j| j[1..j.len() - 1].to_string())).collect();
    out.extend(json);
    out.retain(|v| !v.is_empty());
    out.sort();
    out.dedup();
    out
}

/// "%2F" -> "%2f" (only the two digits after each '%').
fn lower_hex(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut left = 0;
    for c in s.chars() {
        if left > 0 {
            out.push(c.to_ascii_lowercase());
            left -= 1;
        } else {
            if c == '%' {
                left = 2;
            }
            out.push(c);
        }
    }
    out
}

/// Secrets shorter than this are replaced only where a secret can stand
/// (inside addresses and secret key=value pairs): "1234" replaced
/// everywhere would also hit times, sizes and numbers, and could break the
/// report's JSON.
const SCRUB_EVERYWHERE_MIN: usize = 6;

/// The secrets of one camera and what replaces them: long ones replaced
/// anywhere, short ones only inside addresses and key=value pairs.
pub struct Secrets {
    long: Vec<(String, &'static str)>,
    short: Vec<(String, &'static str)>,
    /// Which kinds were too short to scrub everywhere ("password", "user name").
    short_kinds: Vec<&'static str>,
}

impl Secrets {
    /// Everything secret in a camera's settings: the user name and password,
    /// those in the sub stream's address, secret query values in either
    /// address, a pushed stream's key and a battery camera's UID.
    pub fn from_config(cfg: &CameraConfig) -> Self {
        let mut users: Vec<String> = vec![cfg.username.clone()];
        let mut keys: Vec<String> = vec![cfg.password.clone()];
        let mut pairs: Vec<(String, String)> = vec![(cfg.username.clone(), cfg.password.clone())];
        for u in std::iter::once(cfg.url.as_str()).chain(cfg.sub_url.as_deref()) {
            if let Some((user, pw)) = userinfo(u) {
                users.push(user.clone());
                keys.push(pw.clone());
                pairs.push((user, pw));
            }
            if let Some((_, q)) = u.split_once('?') {
                for kv in q.split(['&', ';']) {
                    if let Some((k, v)) = kv.split_once('=') {
                        if secret_key(k) {
                            keys.push(v.to_string());
                            keys.push(percent_encoding::percent_decode_str(&v.replace('+', " ")).decode_utf8_lossy().into_owned());
                            keys.push(percent_encoding::percent_decode_str(v).decode_utf8_lossy().into_owned());
                        }
                    }
                }
            }
        }
        if let Some(k) = crate::ingest::push::push_key(&cfg.url) {
            keys.push(k.to_string());
        }
        if let Some(u) = cfg.battery_uid() {
            keys.push(u);
        }
        // HTTP Basic sign-in as a camera may echo it (seen in an SDP "s=" line):
        // base64 of user:password, with and without its padding.
        for (u, p) in pairs.into_iter().filter(|(u, p)| !u.is_empty() || !p.is_empty()) {
            let b64 = base64::engine::general_purpose::STANDARD.encode(format!("{u}:{p}"));
            keys.push(b64.trim_end_matches('=').to_string());
            keys.push(b64);
        }
        let mut long: Vec<(String, &'static str)> = Vec::new();
        let mut short: Vec<(String, &'static str)> = Vec::new();
        let mut short_kinds: Vec<&'static str> = Vec::new();
        for (list, with, kind) in [(keys, "***", "password"), (users, "(user)", "user name")] {
            for k in list.into_iter().filter(|k| !k.is_empty()) {
                let is_short = k.chars().count() < SCRUB_EVERYWHERE_MIN;
                if is_short && !short_kinds.contains(&kind) {
                    short_kinds.push(kind);
                }
                (if is_short { &mut short } else { &mut long }).extend(variants(&k).into_iter().map(|v| (v, with)));
            }
        }
        // Longest first: a user name inside a password is not left half done.
        for all in [&mut long, &mut short] {
            all.sort_by(|a, b| b.0.len().cmp(&a.0.len()).then(a.0.cmp(&b.0)));
            all.dedup_by(|a, b| a.0 == b.0);
        }
        Secrets { long, short, short_kinds }
    }

    /// What the report's header says when a secret is too short to be
    /// replaced everywhere. None: every secret is replaced everywhere.
    pub fn short_note(&self) -> Option<String> {
        if self.short_kinds.is_empty() {
            return None;
        }
        Some(format!(
            "The camera's {} {} too short to scrub everywhere: removed from addresses and from named values only. Review the report before sharing it.",
            self.short_kinds.join(" and "),
            if self.short_kinds.len() > 1 { "are" } else { "is" }
        ))
    }

    /// Put the camera's own secrets out of `text`: long ones anywhere, short
    /// ones inside addresses (and their query strings).
    pub fn replace_known(&self, text: &str) -> String {
        let mut t = text.to_string();
        for (s, with) in &self.long {
            if t.contains(s.as_str()) {
                t = t.replace(s.as_str(), with);
            }
        }
        if !self.short.is_empty() {
            t = in_addresses(&t, |a| {
                let mut a = a.to_string();
                for (s, with) in &self.short {
                    if a.contains(s.as_str()) {
                        a = a.replace(s.as_str(), with);
                    }
                }
                a
            });
        }
        t
    }

    /// Free text made safe: the camera's own secrets first (a password with
    /// a space or punctuation in it would otherwise end the masking early and
    /// leave its tail), then URL sign-in and secret `key=value` pairs, then
    /// the camera's own secrets again.
    pub fn scrub(&self, text: &str) -> String {
        let known = self.replace_known(text);
        let lines: Vec<String> = known.split('\n').map(|l| mask_kv(&crate::engine::redact_text_with(l, true))).collect();
        self.replace_known(&lines.join("\n"))
    }

    /// Every string in a JSON tree scrubbed.
    pub fn scrub_json(&self, v: &mut Value) {
        match v {
            Value::String(s) => *s = self.scrub(s),
            Value::Array(a) => a.iter_mut().for_each(|x| self.scrub_json(x)),
            Value::Object(o) => o.values_mut().for_each(|x| self.scrub_json(x)),
            _ => {}
        }
    }
}

/// `text` with every address in it (`scheme://...` up to a space or a
/// quote) passed through `f`.
fn in_addresses(text: &str, f: impl Fn(&str) -> String) -> String {
    let stop = |c: char| c.is_whitespace() || matches!(c, '"' | '\'' | '<' | '>' | '`');
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(i) = rest.find("://") {
        // The scheme: letters, digits, + - . before "://".
        let start = rest[..i].rfind(|c: char| !(c.is_ascii_alphanumeric() || matches!(c, '+' | '-' | '.'))).map(|p| p + 1).unwrap_or(0);
        let end = rest[i..].find(stop).map(|e| i + e).unwrap_or(rest.len());
        out.push_str(&rest[..start]);
        out.push_str(&f(&rest[start..end]));
        rest = &rest[end..];
    }
    out.push_str(rest);
    out
}

/// (user, password) from `scheme://user:pw@host/...`, percent-decoded.
/// (The widest rule, `creds::shown_userinfo_at` strict: also a sign-in the
/// connection leaves in the address, such as "admin:12/34@192.0.2.5/x".)
fn userinfo(url: &str) -> Option<(String, String)> {
    crate::engine::shown_login(url)
}

/// Hide the value of every secret `key=value` in free text (also outside a
/// URL, and quoted values such as `username="admin"`). The name is the word
/// before the '=' (letters, digits, '_' and '-'), so `x_password=` and
/// `cam-token=` count as well.
pub fn mask_kv(text: &str) -> String {
    let b = text.as_bytes();
    let mut cuts: Vec<(usize, usize)> = Vec::new();
    for (i, _) in text.match_indices('=') {
        let start = text[..i].rfind(|c: char| !(c.is_ascii_alphanumeric() || c == '_' || c == '-')).map(|p| p + 1).unwrap_or(0);
        if start == i || !secret_key(&text[start..i]) {
            continue;
        }
        let v = i + 1;
        let end = if b.get(v) == Some(&b'"') {
            text[v + 1..].find('"').map(|e| v + 1 + e + 1).unwrap_or(text.len())
        } else {
            text[v..].find(|c: char| c.is_whitespace() || "&;,\"'<>)]}".contains(c)).map(|e| v + e).unwrap_or(text.len())
        };
        if end > v && &text[v..end] != "***" {
            cuts.push((v, end));
        }
    }
    if cuts.is_empty() {
        return text.to_string();
    }
    let mut out = String::with_capacity(text.len());
    let mut at = 0;
    for (s, e) in cuts {
        if s < at {
            continue;
        }
        out.push_str(&text[at..s]);
        out.push_str("***");
        at = e;
    }
    out.push_str(&text[at..]);
    out
}

/// An SDP without the lines a camera may echo its sign-in in: `a=control`,
/// `u=` and `o=` lines holding an '@'.
fn clean_sdp(sdp: &str) -> String {
    sdp.lines()
        .map(|l| {
            let risky = (l.starts_with("a=control:") || l.starts_with("u=") || l.starts_with("o=")) && l.contains('@');
            if risky {
                let keep = if l.starts_with("a=control:") { "a=control:" } else { &l[..2] };
                format!("{keep}(removed: the line held an @)")
            } else {
                l.to_string()
            }
        })
        .collect::<Vec<_>>()
        .join("\n")
}

// ---------------------------------------------------------------------------
// Parts of the report
// ---------------------------------------------------------------------------

/// How one part went: ok, failed, timed out or skipped (with why), and how
/// long it took.
fn part(status: &str, started: Instant, note: impl Into<String>, data: Value) -> Value {
    let note: String = note.into();
    json!({ "status": status, "ms": started.elapsed().as_millis() as u64, "note": if note.is_empty() { Value::Null } else { Value::String(note) }, "data": data })
}

fn skipped(why: &str) -> Value {
    json!({ "status": "skipped", "ms": 0, "note": why, "data": null })
}

/// Why a stream cannot be asked directly (not RTSP, or asleep).
fn network_skip(url: &str, rtsp_only: bool) -> Option<&'static str> {
    let l = url.to_ascii_lowercase();
    if crate::local_source::is_local(url) {
        return Some("a local device, not a network stream");
    }
    if crate::ingest::push::is_push_url(url) {
        return Some("a pushed stream: the sender connects to AmberSight");
    }
    if crate::ingest::baichuan::is_baichuan_url(url) {
        return Some("a battery camera over the Reolink protocol, not RTSP");
    }
    if rtsp_only && !(l.starts_with("rtsp://") || l.starts_with("rtsps://")) {
        return Some("not an RTSP address");
    }
    if !rtsp_only && !["rtsp://", "rtsps://", "http://", "https://", "rtmp://"].iter().any(|p| l.starts_with(p)) {
        return Some("not a network stream address");
    }
    None
}

/// The address without sign-in, and the sign-in to use with it.
fn source(url: &str, cfg: &CameraConfig) -> crate::ingest::StreamSource {
    let (user, pw) = userinfo(url).unwrap_or_else(|| (cfg.username.clone(), cfg.password.clone()));
    let bare = match url.split_once("://") {
        Some((scheme, rest)) => {
            let auth_end = rest.find(['/', '?', '#']).unwrap_or(rest.len());
            match rest[..auth_end].rfind('@') {
                Some(at) => format!("{scheme}://{}", &rest[at + 1..]),
                None => url.to_string(),
            }
        }
        None => url.to_string(),
    };
    crate::ingest::StreamSource { url: bare, username: user, password: pw, group: None, transport: cfg.transport.clone() }
}

/// The address with sign-in, for ffmpeg.
fn ffmpeg_url(url: &str, cfg: &CameraConfig) -> String {
    if userinfo(url).is_some() {
        return url.to_string();
    }
    crate::engine::camera_url(&CameraConfig { url: url.to_string(), ..cfg.clone() })
}

/// RTSP DESCRIBE: the camera's SDP, its server name and what retina reads in it.
async fn describe(url: &str, cfg: &CameraConfig) -> Value {
    if let Some(why) = network_skip(url, true) {
        return skipped(why);
    }
    let t0 = Instant::now();
    let src = source(url, cfg);
    let Ok(parsed) = url::Url::parse(&src.url) else { return part("failed", t0, "the address could not be read", Value::Null) };
    let opts = crate::ingest::session_options(&src);
    match tokio::time::timeout(DESCRIBE_TIMEOUT, retina::client::Session::describe(parsed, opts)).await {
        Err(_) => part("timed out", t0, format!("no answer within {} s", DESCRIBE_TIMEOUT.as_secs()), Value::Null),
        Ok(Err(e)) => part("failed", t0, e.to_string(), Value::Null),
        Ok(Ok(s)) => {
            let streams: Vec<Value> = s
                .streams()
                .iter()
                .map(|st| {
                    json!({
                        "media": st.media(),
                        "encoding": st.encoding_name(),
                        "clock_rate": st.clock_rate_hz(),
                        "channels": st.channels().map(|c| c.get()),
                        "framerate": st.framerate(),
                        "control": st.control().map(|c| c.to_string()),
                    })
                })
                .collect();
            let sdp = clean_sdp(&String::from_utf8_lossy(s.sdp()).replace("\r\n", "\n"));
            part("ok", t0, "", json!({ "server": s.tool().map(|t| t.to_string()), "streams": streams, "sdp": sdp }))
        }
    }
}

/// One stream as ffmpeg sees it.
#[derive(Debug, Default, PartialEq, serde::Serialize)]
pub struct ProbedStream {
    pub kind: String,
    pub codec: String,
    pub width: Option<u32>,
    pub height: Option<u32>,
    pub fps: Option<f64>,
    pub sample_rate: Option<u32>,
    pub channels: Option<String>,
    pub line: String,
}

/// Read ffmpeg's "Stream #0:0: Video: h264 (High), yuv420p, 2560x1440, 20 fps ..." lines.
pub fn parse_ffmpeg_streams(stderr: &str) -> Vec<ProbedStream> {
    let mut out = Vec::new();
    for line in stderr.lines().map(str::trim).filter(|l| l.starts_with("Stream #")) {
        let (kind, rest) = if let Some(i) = line.find(": Video: ") {
            ("video", &line[i + 9..])
        } else if let Some(i) = line.find(": Audio: ") {
            ("audio", &line[i + 9..])
        } else if let Some(i) = line.find(": Data: ") {
            ("data", &line[i + 8..])
        } else {
            continue;
        };
        let mut s = ProbedStream { kind: kind.into(), line: line.to_string(), ..Default::default() };
        s.codec = rest.split([' ', ',']).next().unwrap_or("").to_string();
        let parts: Vec<&str> = rest.split(", ").map(str::trim).collect();
        for p in &parts {
            let first = p.split_whitespace().next().unwrap_or("");
            if let Some((w, h)) = first.split_once('x') {
                if let (Ok(w), Ok(h)) = (w.parse::<u32>(), h.parse::<u32>()) {
                    s.width.get_or_insert(w);
                    s.height.get_or_insert(h);
                }
            }
            if let Some(v) = p.strip_suffix(" fps") {
                s.fps = v.trim().parse().ok();
            }
            if let Some(v) = p.strip_suffix(" Hz") {
                s.sample_rate = v.trim().parse().ok();
            }
            if kind == "audio" && (["mono", "stereo"].contains(p) || p.ends_with("channels") || p.contains('.') && p.chars().next().is_some_and(|c| c.is_ascii_digit()) && !p.contains(' ')) {
                s.channels.get_or_insert(p.to_string());
            }
        }
        out.push(s);
    }
    out
}

/// ffmpeg's own look at a stream (`ffmpeg -i`: ffprobe is not installed
/// everywhere AmberSight runs, ffmpeg is).
async fn probe(url: &str, cfg: &CameraConfig) -> Value {
    if let Some(why) = network_skip(url, false) {
        return skipped(why);
    }
    let t0 = Instant::now();
    let full = ffmpeg_url(url, cfg);
    let mut cmd = tokio::process::Command::new(crate::ffmpeg_bin::program());
    cmd.args(["-hide_banner", "-nostdin", "-analyzeduration", "3000000", "-probesize", "5000000"]);
    if full.to_ascii_lowercase().starts_with("rtsp") {
        cmd.args(["-rtsp_transport", if crate::ingest::wants_udp(&cfg.transport, &full) { "udp" } else { "tcp" }]);
        cmd.args(crate::ffmpeg_bin::rtsp_timeout_args(5_000_000));
    }
    cmd.arg("-i").arg(&full);
    cmd.stdin(std::process::Stdio::null()).stdout(std::process::Stdio::null()).stderr(std::process::Stdio::piped()).kill_on_drop(true);
    #[cfg(windows)]
    cmd.creation_flags(0x0800_0000); // CREATE_NO_WINDOW
    let child = match cmd.spawn() {
        Ok(c) => c,
        Err(e) => return part("failed", t0, format!("could not run ffmpeg: {e}"), Value::Null),
    };
    match tokio::time::timeout(PROBE_TIMEOUT, child.wait_with_output()).await {
        Err(_) => part("timed out", t0, format!("ffmpeg did not finish within {} s", PROBE_TIMEOUT.as_secs()), Value::Null),
        Ok(Err(e)) => part("failed", t0, e.to_string(), Value::Null),
        Ok(Ok(out)) => {
            let text = String::from_utf8_lossy(&out.stderr).to_string();
            let streams = parse_ffmpeg_streams(&text);
            // ffmpeg ends with "At least one output file must be specified":
            // that is expected; the input lines are what counts.
            let lines: Vec<&str> = text.lines().filter(|l| !l.contains("At least one output file")).collect();
            let tail = lines[lines.len().saturating_sub(30)..].join("\n");
            if streams.is_empty() {
                part("failed", t0, "ffmpeg found no streams", json!({ "output": tail }))
            } else {
                part("ok", t0, "", json!({ "streams": streams, "output": tail }))
            }
        }
    }
}

/// The camera's ONVIF profiles (names, sizes, stream addresses).
async fn onvif(cfg: &CameraConfig) -> Value {
    let Some(port) = cfg.onvif_port else { return skipped("no ONVIF port set or found for this camera") };
    let Some(host) = crate::onvif::host_from_rtsp_url(&cfg.url) else { return skipped("the camera address has no host") };
    let t0 = Instant::now();
    let shown = if host.contains(':') { format!("[{host}]") } else { host };
    let xaddr = format!("http://{shown}:{port}/onvif/device_service");
    let (u, p) = (cfg.username.clone(), cfg.password.clone());
    let held = OnvifBusy::new();
    let job = tokio::task::spawn_blocking(move || {
        let _held = held; // counted until the lookup really ends
        crate::onvif_ptz::inspect(&xaddr, &u, &p)
    });
    match tokio::time::timeout(ONVIF_TIMEOUT, job).await {
        Err(_) => part("timed out", t0, format!("the camera did not answer within {} s", ONVIF_TIMEOUT.as_secs()), Value::Null),
        Ok(Err(e)) => part("failed", t0, e.to_string(), Value::Null),
        Ok(Ok(Err(e))) => part("failed", t0, format!("{e:#}"), Value::Null),
        Ok(Ok(Ok(i))) => {
            let profiles: Vec<Value> = i
                .profiles
                .iter()
                .map(|p| json!({ "name": p.name, "width": p.width, "height": p.height, "encoding": p.encoding, "uri": p.uri, "ptz": p.has_ptz, "source": p.source }))
                .collect();
            // (No serial number: it is not needed and identifies the device.)
            let device = json!({ "manufacturer": i.info.manufacturer, "model": i.info.model, "firmware": i.info.firmware });
            part("ok", t0, "", json!({ "port": port, "device": device, "profiles": profiles, "main": i.main, "sub": i.sub }))
        }
    }
}

// ---------------------------------------------------------------------------
// This server
// ---------------------------------------------------------------------------

fn os_name() -> String {
    #[cfg(target_os = "linux")]
    {
        if let Ok(t) = std::fs::read_to_string("/etc/os-release") {
            if let Some(n) = t.lines().find_map(|l| l.strip_prefix("PRETTY_NAME=")) {
                return n.trim_matches('"').to_string();
            }
        }
    }
    std::env::consts::OS.to_string()
}

fn install_kind() -> String {
    let mut kind = if crate::update::in_container() {
        "container (Docker or Podman)".to_string()
    } else if crate::update::program_path().is_ok_and(|p| crate::update::is_packaged(&p)) {
        "package (deb/rpm)".to_string()
    } else {
        "program files (installer, zip or tarball)".to_string()
    };
    if crate::update::under_service() {
        kind.push_str(", running as a service");
    }
    kind
}

fn system() -> Value {
    system_with(&crate::proclimit::read(), std::thread::available_parallelism().map(|n| n.get()).ok())
}

/// The "This server" part with the process limits and processor count given
/// (the limits are left out when none could be read).
fn system_with(limits: &crate::proclimit::ProcLimits, cores: Option<usize>) -> Value {
    json!({
        "version": env!("CARGO_PKG_VERSION"),
        "os": os_name(),
        "arch": std::env::consts::ARCH,
        "install": install_kind(),
        "ffmpeg": crate::ffmpeg_bin::status(),
        "cpu_cores": cores,
        "process_limits": if limits.is_empty() { Value::Null } else { limits.to_json() },
    })
}

/// "cgroup pids.max 100, in use 97; ulimit -u 63000" from the report's JSON
/// (None when no limit could be read).
fn process_limits_line(sys: &Value) -> Option<String> {
    let l = &sys["process_limits"];
    if l.is_null() {
        return None;
    }
    let limits = crate::proclimit::ProcLimits {
        pids_max: l["pids_max"].as_str().map(String::from),
        pids_current: l["pids_current"].as_u64(),
        nproc: l["nproc_limit"].as_str().map(String::from),
    };
    limits.report_line()
}

/// The setting that puts a camera on the ffmpeg pipeline, for the report:
/// text burned into its recordings.
fn picture_settings(cfg: &CameraConfig) -> bool {
    cfg.overlay
}

// ---------------------------------------------------------------------------
// The report
// ---------------------------------------------------------------------------

/// What a camera reader knows now.
pub struct Live {
    pub ingest: String,
    pub online: bool,
    pub sleeping: bool,
    pub last_error: Option<String>,
    pub last_error_at: Option<i64>,
    pub reconnects: u32,
    pub online_since: Option<i64>,
    pub offline_since: Option<i64>,
    pub frames: u64,
    pub clock_skew: Option<f64>,
    /// Behind a hub (an NVR or Reolink Home Hub channel): the report opens
    /// no extra stream connections to it.
    pub hub: bool,
}

/// Whether the camera is a channel of a hub or NVR: a Reolink channel set,
/// or another camera at the same address. (As the built-in reader judges
/// it before probing a port.)
pub fn behind_hub(cfg: &CameraConfig, all: &[CameraConfig]) -> bool {
    if cfg.reolink_channel.is_some() {
        return true;
    }
    let host = |u: &str| crate::onvif::host_from_rtsp_url(u).map(|h| h.to_ascii_lowercase());
    let mine = host(&cfg.url);
    mine.is_some() && all.iter().any(|c| c.name != cfg.name && host(&c.url) == mine)
}

/// A hub camera's DESCRIBE: the SDP of the session running now, or skipped.
fn hub_describe(url: &str) -> Value {
    if let Some(why) = network_skip(url, true) {
        return skipped(why);
    }
    match crate::ingest::recorder::last_sdp(url) {
        Some(sdp) => json!({ "status": "ok", "ms": 0, "note": "from the session running now (a hub camera is not asked again)", "data": { "server": null, "streams": [], "sdp": clean_sdp(&sdp) } }),
        None => skipped("skipped for hub cameras (a hub allows few connections), and no session is running"),
    }
}

fn when(t: Option<i64>) -> Value {
    t.and_then(|t| chrono::DateTime::from_timestamp(t, 0)).map(|d| Value::String(d.to_rfc3339())).unwrap_or(Value::Null)
}

fn shown_url(u: &str) -> String {
    match crate::ingest::push::push_key(u) {
        Some(_) => "push://(key removed)".into(),
        None => crate::engine::redact_url(u.to_string()),
    }
}

fn settings(cfg: &CameraConfig) -> Value {
    let set = |b: bool| if b { "set" } else { "not set" };
    json!({
        "name": cfg.name,
        "enabled": cfg.enabled,
        "url": shown_url(&cfg.url),
        "sub_url": cfg.sub_url.as_deref().filter(|s| !s.is_empty()).map(shown_url),
        "username": set(!cfg.username.is_empty()),
        "password": set(!cfg.password.is_empty()),
        "ingest_setting": cfg.ingest,
        "overlay": picture_settings(cfg),
        "onvif_host": crate::onvif::host_from_rtsp_url(&cfg.url),
        "onvif_port": cfg.onvif_port,
        "reolink_channel": cfg.reolink_channel,
        "uid": cfg.battery_uid().map(|_| "set"),
        "on_demand": cfg.on_demand,
        "record": cfg.record,
        "record_motion_only": cfg.record_motion_only,
        "record_sub": cfg.record_sub,
        "direct": cfg.direct,
        "audio": cfg.audio,
        "fps": cfg.fps,
        "scale": cfg.scale,
        "hwaccel": cfg.hwaccel,
        "motion": cfg.motion,
        "trigger": cfg.trigger,
    })
}

/// The camera's log lines (newest last), at most `LOG_LINES`.
fn log_lines(name: &str) -> Vec<String> {
    let tag = format!("'{name}'");
    let mut lines: Vec<String> = crate::logbuf::recent(tracing::Level::INFO, 2000)
        .into_iter()
        .filter(|l| l.message.contains(&tag))
        .map(|l| {
            let at = chrono::DateTime::from_timestamp(l.ts, 0).map(|d| d.format("%Y-%m-%d %H:%M:%S").to_string()).unwrap_or_default();
            format!("{at} {} {}: {}", l.level, l.target, l.message)
        })
        .collect();
    let from = lines.len().saturating_sub(LOG_LINES);
    lines.drain(..from);
    lines
}

/// Make the report for one camera (`cfg` with its sign-in moved into the
/// user name and password fields, and the ONVIF port it uses). At most
/// `TOTAL` long; parts that fail or run out of time say so.
pub async fn make(cfg: CameraConfig, live: Live) -> (String, String) {
    let t0 = Instant::now();
    let secrets = Secrets::from_config(&cfg);
    let sub = cfg.sub_url.clone().filter(|s| !s.is_empty());
    let asleep = cfg.on_demand && live.sleeping;

    let (main_sdp, sub_sdp, main_probe, sub_probe, onvif_part) = if asleep {
        let s = || skipped("the battery camera is asleep: asking it would wake it (open its live view first, then make the report)");
        (s(), s(), s(), s(), s())
    } else if live.hub {
        // A hub allows few connections: its running session's SDP, and no
        // extra DESCRIBE or ffmpeg probe.
        let s = || skipped("skipped for hub cameras (a hub allows few connections)");
        let sub_sdp = match &sub {
            Some(u) => hub_describe(u),
            None => skipped("no sub stream set"),
        };
        let onvif_part = match tokio::time::timeout(TOTAL.saturating_sub(Duration::from_secs(1)), onvif(&cfg)).await {
            Ok(o) => o,
            Err(_) => json!({ "status": "timed out", "ms": TOTAL.as_millis() as u64, "note": "the report ran out of time", "data": null }),
        };
        (hub_describe(&cfg.url), sub_sdp, s(), s(), onvif_part)
    } else {
        // Two at a time against the camera at most (some allow few connections).
        let sdp_lane = async {
            let m = describe(&cfg.url, &cfg).await;
            let s = match &sub {
                Some(u) => describe(u, &cfg).await,
                None => skipped("no sub stream set"),
            };
            (m, s)
        };
        let probe_lane = async {
            let m = probe(&cfg.url, &cfg).await;
            let s = match &sub {
                Some(u) => probe(u, &cfg).await,
                None => skipped("no sub stream set"),
            };
            (m, s)
        };
        let work = async { tokio::join!(sdp_lane, probe_lane, onvif(&cfg)) };
        match tokio::time::timeout(TOTAL.saturating_sub(Duration::from_secs(1)), work).await {
            Ok(((a, b), (c, d), e)) => (a, b, c, d, e),
            Err(_) => {
                let s = || json!({ "status": "timed out", "ms": TOTAL.as_millis() as u64, "note": "the report ran out of time", "data": null });
                (s(), s(), s(), s(), s())
            }
        }
    };

    let (ends, fallback) = recorded(&cfg.name);
    let fallback_words = fallback.as_ref().map(|f| crate::engine::health::plain_error(&f.1)).filter(|w| !w.is_empty());
    let mut report = json!({
        "report": "AmberSight camera report",
        "made": chrono::Utc::now().to_rfc3339(),
        "scrub_note": secrets.short_note(),
        "seconds": t0.elapsed().as_secs_f64(),
        "system": system(),
        "camera": settings(&cfg),
        "state": {
            "ingest": live.ingest,
            "ffmpeg_reason": crate::engine::ffpath::why_ffmpeg(&cfg, &live.ingest, fallback_words.as_deref()),
            "fallback_reason": fallback.as_ref().map(|f| f.1.clone()),
            "fallback_at": when(fallback.map(|f| f.0)),
            "online": live.online,
            "sleeping": live.sleeping,
            "last_error": live.last_error,
            "last_error_at": when(live.last_error_at),
            "reconnects": live.reconnects,
            "online_since": when(live.online_since),
            "offline_since": when(live.offline_since),
            "frames": live.frames,
            "clock_skew_s": live.clock_skew,
        },
        "stream_ends": ends.iter().map(|(t, w)| json!({ "at": when(Some(*t)), "why": w })).collect::<Vec<_>>(),
        "describe": { "main": main_sdp, "sub": sub_sdp },
        "probe": { "main": main_probe, "sub": sub_probe },
        "onvif": onvif_part,
        "log": log_lines(&cfg.name),
    });
    secrets.scrub_json(&mut report);
    let text = format!(
        "{}\n=== JSON (the same, for tools) ===\n{}\n",
        summary(&report),
        serde_json::to_string_pretty(&report).unwrap_or_default()
    );
    // Last pass: the camera's actual sign-in, in any spelling, anywhere.
    let text = secrets.replace_known(&text);
    let slug: String = cfg.name.chars().map(|c| if c.is_ascii_alphanumeric() { c.to_ascii_lowercase() } else { '-' }).collect();
    let file = format!("ambersight-camera-report-{}-{}.txt", slug.trim_matches('-'), chrono::Local::now().format("%Y%m%d-%H%M"));
    (file, text)
}

/// The readable part, from the (already scrubbed) JSON.
fn summary(r: &Value) -> String {
    use std::fmt::Write;
    let s = |v: &Value| match v {
        Value::Null => "-".to_string(),
        Value::String(s) => s.clone(),
        o => o.to_string(),
    };
    let mut t = String::new();
    let _ = writeln!(t, "AmberSight camera report");
    let _ = writeln!(t, "Made {} in {:.1} s. User names, passwords, stream keys and secret address parts are removed.", s(&r["made"]), r["seconds"].as_f64().unwrap_or(0.0));
    if let Some(n) = r["scrub_note"].as_str() {
        let _ = writeln!(t, "WARNING: {n}");
    }
    let _ = writeln!(t, "It does include the camera's network address. Attach it to a GitHub issue about this camera.\n");
    let sys = &r["system"];
    let _ = writeln!(t, "== This server ==");
    let _ = writeln!(t, "AmberSight {} on {} ({}), {}", s(&sys["version"]), s(&sys["os"]), s(&sys["arch"]), s(&sys["install"]));
    let _ = writeln!(t, "ffmpeg: {}", s(&sys["ffmpeg"]));
    if !sys["cpu_cores"].is_null() {
        let _ = writeln!(t, "Processor cores: {}", s(&sys["cpu_cores"]));
    }
    if let Some(l) = process_limits_line(sys) {
        let _ = writeln!(t, "Process and thread limits: {l}");
    }
    let _ = writeln!(t);
    let c = &r["camera"];
    let _ = writeln!(t, "== Camera settings ==");
    for k in ["name", "enabled", "url", "sub_url", "username", "password", "ingest_setting", "overlay", "onvif_host", "onvif_port", "reolink_channel", "uid", "on_demand", "record", "record_motion_only", "record_sub", "direct", "audio", "fps", "scale", "hwaccel", "motion", "trigger"] {
        if !c[k].is_null() {
            let _ = writeln!(t, "{k}: {}", s(&c[k]));
        }
    }
    let st = &r["state"];
    let _ = writeln!(t, "\n== Now ==");
    if !st["ffmpeg_reason"].is_null() {
        let _ = writeln!(t, "Why ffmpeg: {}", s(&st["ffmpeg_reason"]));
    }
    let _ = writeln!(t, "Read by: {}{}", s(&st["ingest"]), if st["fallback_reason"].is_null() { String::new() } else { format!(" (fallback since {}: {})", s(&st["fallback_at"]), s(&st["fallback_reason"])) });
    let _ = writeln!(t, "Online: {}  sleeping: {}  reconnects: {}  frames: {}", s(&st["online"]), s(&st["sleeping"]), s(&st["reconnects"]), s(&st["frames"]));
    let _ = writeln!(t, "Last error: {} ({})", s(&st["last_error"]), s(&st["last_error_at"]));
    if let Some(e) = r["stream_ends"].as_array().filter(|e| !e.is_empty()) {
        let _ = writeln!(t, "\n== Recent stream ends ==");
        for x in e {
            let _ = writeln!(t, "{}  {}", s(&x["at"]), s(&x["why"]));
        }
    }
    for (title, key) in [("RTSP DESCRIBE", "describe"), ("ffmpeg probe", "probe")] {
        for which in ["main", "sub"] {
            let p = &r[key][which];
            let _ = writeln!(t, "\n== {title}, {which} stream: {} ({} ms){} ==", s(&p["status"]), s(&p["ms"]), if p["note"].is_null() { String::new() } else { format!(": {}", s(&p["note"])) });
            let d = &p["data"];
            if key == "describe" {
                if !d["server"].is_null() {
                    let _ = writeln!(t, "Server: {}", s(&d["server"]));
                }
                for x in d["streams"].as_array().into_iter().flatten() {
                    let _ = writeln!(t, "{} {} clock {} Hz, channels {}, fps {}", s(&x["media"]), s(&x["encoding"]), s(&x["clock_rate"]), s(&x["channels"]), s(&x["framerate"]));
                }
                if let Some(sdp) = d["sdp"].as_str() {
                    let _ = writeln!(t, "--- SDP ---\n{}", sdp.trim_end());
                }
            } else {
                for x in d["streams"].as_array().into_iter().flatten() {
                    if x["kind"] == "video" {
                        let _ = writeln!(t, "video {} {}x{} {} fps", s(&x["codec"]), s(&x["width"]), s(&x["height"]), s(&x["fps"]));
                    } else {
                        let _ = writeln!(t, "{} {} {} Hz {}", s(&x["kind"]), s(&x["codec"]), s(&x["sample_rate"]), s(&x["channels"]));
                    }
                }
                if d["streams"].is_null() {
                    if let Some(o) = d["output"].as_str() {
                        let _ = writeln!(t, "{}", o.trim_end());
                    }
                }
            }
        }
    }
    let o = &r["onvif"];
    let _ = writeln!(t, "\n== ONVIF: {} ({} ms){} ==", s(&o["status"]), s(&o["ms"]), if o["note"].is_null() { String::new() } else { format!(": {}", s(&o["note"])) });
    if !o["data"].is_null() {
        let d = &o["data"]["device"];
        let _ = writeln!(t, "{} {} firmware {}", s(&d["manufacturer"]), s(&d["model"]), s(&d["firmware"]));
        for p in o["data"]["profiles"].as_array().into_iter().flatten() {
            let _ = writeln!(t, "{}: {} {}x{} {}", s(&p["name"]), s(&p["encoding"]), s(&p["width"]), s(&p["height"]), s(&p["uri"]));
        }
    }
    let _ = writeln!(t, "\n== Log lines about this camera (newest last) ==");
    for l in r["log"].as_array().into_iter().flatten() {
        let _ = writeln!(t, "{}", s(l));
    }
    t
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg(url: &str) -> CameraConfig {
        let mut c: CameraConfig = toml::from_str(&format!("name = \"Test cam\"\nurl = {}\n", toml::Value::String(url.into()))).unwrap();
        crate::engine::normalize_creds(&mut c);
        c
    }

    fn live() -> Live {
        Live { ingest: "native".into(), online: false, sleeping: false, last_error: None, last_error_at: None, reconnects: 0, online_since: None, offline_since: None, frames: 0, clock_skew: None, hub: false }
    }

    /// Every spelling of `secret` that must never be in the report.
    fn spellings(secret: &str) -> Vec<String> {
        variants(secret)
    }

    fn assert_clean(text: &str, secrets: &[&str]) {
        for s in secrets {
            for v in spellings(s) {
                assert!(!text.contains(&v), "{v:?} (from {s:?}) is in the report:\n{text}");
            }
        }
    }

    #[test]
    fn kv_values_are_masked_anywhere() {
        assert_eq!(mask_kv("a?user=admin&password=x1&b=2"), "a?user=***&password=***&b=2");
        assert_eq!(mask_kv("Digest username=\"admin\", realm=\"x\""), "Digest username=***, realm=\"x\"");
        assert_eq!(mask_kv("PWD=abc pass=def;usr=g"), "PWD=*** pass=***;usr=***");
        // Names the review found leaking, and any name ending like a secret.
        assert_eq!(
            mask_kv("/cgi?loginuse=admin&loginpas=LoginPas777&pw=a&psw=b&pswd=c&access_token=tok1&secret=s&session=x&sid=y&sig=z&signature=q"),
            "/cgi?loginuse=***&loginpas=***&pw=***&psw=***&pswd=***&access_token=***&secret=***&session=***&sid=***&sig=***&signature=***"
        );
        assert_eq!(mask_kv("adminpass=1 x_password=2 cam-token=3 apiKey=4 my_secret=5"), "adminpass=*** x_password=*** cam-token=*** apiKey=*** my_secret=***");
        // Not secret names.
        assert_eq!(mask_kv("channel=1 subtype=0 stream=main port=554"), "channel=1 subtype=0 stream=main port=554");
        assert_eq!(mask_kv("no secrets"), "no secrets");
    }

    /// The review's leaks, with its exact strings.
    #[tokio::test]
    async fn review_leaks_stay_out_of_the_report() {
        // 1. A raw '/' in the password.
        let mut c = cfg("rtsp://slashusr:pa/ss99xx@127.0.0.1:9/main");
        assert_eq!((c.username.as_str(), c.password.as_str(), c.url.as_str()), ("slashusr", "pa/ss99xx", "rtsp://127.0.0.1:9/main"));
        c.sub_url = Some("rtsp://slashusr:pa/ss99xx@127.0.0.1:9/sub".into());
        crate::engine::normalize_creds(&mut c);
        assert_eq!(c.sub_url.as_deref(), Some("rtsp://slashusr:pa%2Fss99xx@127.0.0.1:9/sub"));
        c.onvif_port = None;
        let mut l = live();
        l.last_error = Some("401 for rtsp://slashusr:pa/ss99xx@127.0.0.1:9/main".into());
        note_end(&c.name, "rtsp://slashusr:pa/ss99xx@127.0.0.1:9/main: 401", false);
        let (_, text) = make(c, l).await;
        assert_clean(&text, &["pa/ss99xx", "slashusr"]);
        assert!(!text.contains("ss99xx"), "{text}");

        // 2. Query names outside the old list.
        let mut c = cfg("http://127.0.0.1:9/videostream.cgi?loginuse=admin&loginpas=LoginPas777&x_access_token=Tok3nValue9");
        c.onvif_port = None;
        let (_, text) = make(c, live()).await;
        assert_clean(&text, &["LoginPas777", "Tok3nValue9"]);
        assert!(text.contains("loginpas=***") && text.contains("access_token=***"), "{text}");

        // 3 and 4. A password with a space and punctuation, echoed back by a
        // camera: masked by name, half encoded (retina's a=control), base64.
        let pw = "Long/Pass word#1";
        let mut c = cfg("rtsp://127.0.0.1:9/main");
        c.username = "echouser".into();
        c.password = pw.into();
        let s = Secrets::from_config(&c);
        let b64 = base64::engine::general_purpose::STANDARD.encode(format!("echouser:{pw}"));
        let b64_pw = base64::engine::general_purpose::STANDARD.encode(pw);
        let echoed = format!(
            "s=Basic {b64}\nu=rtsp://echouser:{pw}@h/\na=control:rtsp://echouser:Long/Pass%20word#1@h/main/trackID=1\nlog pass={pw} done\nkey {b64_pw} and {}",
            b64.trim_end_matches('=')
        );
        let t = s.scrub(&clean_sdp(&echoed));
        for bad in [pw, "word#1", "Pass%20word", b64.as_str(), b64.trim_end_matches('='), b64_pw.as_str(), "echouser"] {
            assert!(!t.contains(bad), "{bad:?} in {t}");
        }
        assert!(t.contains("u=(removed: the line held an @)") && t.contains("a=control:(removed"), "{t}");
    }

    #[test]
    fn sdp_lines_with_an_at_are_dropped() {
        let sdp = "v=0\no=- 1 1 IN IP4 u:p@h\ns=x\na=control:rtsp://u:p@h/x\na=control:trackID=1\nm=video 0 RTP/AVP 96";
        assert_eq!(clean_sdp(sdp), "v=0\no=(removed: the line held an @)\ns=x\na=control:(removed: the line held an @)\na=control:trackID=1\nm=video 0 RTP/AVP 96");
    }

    /// The short-secret note says nothing the scrubbing would change.
    #[test]
    fn the_short_note_survives_scrubbing() {
        let s = Secrets::from_config(&cfg("rtsp://ab:12@127.0.0.1/x"));
        let note = s.short_note().unwrap();
        assert_eq!(s.scrub(&note), note);
    }

    #[test]
    fn variants_cover_encodings() {
        let v = variants("p@ss w/rd#\"&");
        for want in ["p@ss w/rd#\"&", "p%40ss%20w%2Frd%23%22%26", "p%40ss%20w%2frd%23%22%26", "p%40ss+w%2Frd%23%22%26", "p@ss w/rd#\\\"&", "p@ss w/rd#&quot;&amp;"] {
            assert!(v.iter().any(|x| x == want), "{want} missing from {v:?}");
        }
    }

    /// The password in every place a camera address can hold one, with
    /// special characters, raw and percent-encoded.
    #[tokio::test]
    async fn report_never_contains_the_sign_in() {
        let pw = "S3cr:t@P#ss/w%rd&x y";
        let enc = percent_encoding::utf8_percent_encode(pw, ENC).to_string();
        let qpw = "Qu3ryP4ss!";
        let sub_pw = "Sub$ecret?9";
        let user = "camadmin";
        let urls = [
            format!("rtsp://{user}:{enc}@127.0.0.1:9/main?user={user}&password={qpw}&pwd={qpw}&channel=1"),
            format!("rtsp://127.0.0.1:9/cam/realmonitor?channel=1&subtype=0&username={user}&passwd={qpw}&usr={user}&pass={qpw}"),
            format!("http://127.0.0.1:9/flv?port=1935&app=bcs&stream=channel0_main.bcs&user={user}&password={qpw}&token={qpw}&key={qpw}&auth={qpw}"),
        ];
        for url in urls {
            let mut c = cfg(&url);
            if c.password.is_empty() {
                c.username = user.into();
                c.password = pw.into();
            }
            c.sub_url = Some(format!("rtsp://{user}:{}@127.0.0.1:9/sub?pwd={qpw}", percent_encoding::utf8_percent_encode(sub_pw, ENC)));
            crate::engine::normalize_creds(&mut c);
            // What the readers logged and noted, with the secrets in them.
            note_end(&c.name, &format!("rtsp://{user}:{enc}@127.0.0.1:9/main?password={qpw}: 401 Unauthorized"), false);
            note_fallback(&c.name, &format!("DESCRIBE failed for http://h/x?pwd={qpw}"));
            let mut l = live();
            l.last_error = Some(format!("401 for rtsp://{user}:{pw}@127.0.0.1/main password={qpw}"));
            c.onvif_port = None;
            let (file, text) = make(c, l).await;
            assert!(file.starts_with("ambersight-camera-report-test-cam-") && file.ends_with(".txt"), "{file}");
            assert_clean(&text, &[pw, qpw, sub_pw, user]);
            assert!(text.contains("=== JSON"), "{text}");
            assert!(text.contains("\"password\": \"set\""), "{text}");
            let json_part = text.split("=== JSON (the same, for tools) ===\n").nth(1).unwrap();
            serde_json::from_str::<Value>(json_part).expect("the JSON part reads");
        }
    }

    #[test]
    fn replace_known_catches_text_that_slipped_through() {
        let mut c = cfg("rtsp://user01:pa%2Fss%40x@h/x");
        c.uid = Some("95270000ABCDEFGH".into());
        let s = Secrets::from_config(&c);
        assert_eq!(s.short_note(), None);
        let t = s.scrub("odd: pa/ss@x and pa%2Fss%40x and pa%2fss%40x and user01 and 95270000ABCDEFGH");
        assert!(!t.contains("pa/ss@") && !t.contains("pa%2") && !t.contains("user01") && !t.contains("ABCDEFGH"), "{t}");
        // A pushed camera's key.
        let s = Secrets::from_config(&cfg("push://Abcdefghijkmnop23456"));
        assert_eq!(s.scrub("sender on push://Abcdefghijkmnop23456"), "sender on push://***");
        assert_eq!(shown_url("push://Abcdefghijkmnop23456"), "push://(key removed)");
    }

    /// A very short password is scrubbed inside addresses and key=value
    /// pairs only: elsewhere "1234" is a number like any other, and the
    /// report's JSON stays whole.
    #[tokio::test]
    async fn short_password_is_scrubbed_where_it_can_stand() {
        let mut c = cfg("rtsp://admin:1234@127.0.0.1:9/main?channel=1");
        c.sub_url = Some("rtsp://127.0.0.1:9/sub".into());
        crate::engine::normalize_creds(&mut c);
        c.onvif_port = None;
        let s = Secrets::from_config(&c);
        let note = s.short_note().unwrap();
        assert!(note.contains("password and user name") && note.contains("Review the report"), "{note}");
        // In addresses and key=value pairs: gone.
        let t = s.scrub("tried rtsp://127.0.0.1:9/x?pw=1234&a=1 and http://h/y/1234 with password=1234");
        assert!(!t.contains("1234"), "{t}");
        // Elsewhere: left (a frame count, a port, a time).
        assert_eq!(s.scrub("12345 frames at 12:34, \"1234\": [1234]"), "12345 frames at 12:34, \"1234\": [1234]");
        let mut l = live();
        l.frames = 1234;
        l.last_error = Some("401 for rtsp://admin:1234@127.0.0.1/main".into());
        let (_, text) = make(c, l).await;
        assert!(text.contains("WARNING: The camera's password and user name are too short to scrub everywhere"), "{text}");
        assert!(!text.contains("admin:1234") && !text.contains("/main?pw=1234"), "{text}");
        let json_part = text.split("=== JSON (the same, for tools) ===\n").nth(1).unwrap();
        let v: Value = serde_json::from_str(json_part).expect("the JSON part reads");
        assert_eq!(v["state"]["frames"], 1234);
    }

    /// A hub camera gets no extra DESCRIBE or probe: the running session's
    /// SDP when there is one, else a note.
    #[tokio::test]
    async fn hub_cameras_are_not_asked_again() {
        let mut c = cfg("rtsp://u1:longpassword@127.0.0.1:9/hubmain");
        c.reolink_channel = Some(1);
        c.onvif_port = None;
        assert!(behind_hub(&c, &[]));
        assert!(!behind_hub(&cfg("rtsp://192.0.2.9/x"), &[cfg("rtsp://192.0.2.8/x")]));
        let mut l = live();
        l.hub = true;
        let t0 = Instant::now();
        let (_, text) = make(c, l).await;
        assert!(text.contains("skipped for hub cameras"), "{text}");
        assert!(t0.elapsed() < Duration::from_secs(4), "it must not wait for the camera");
    }

    #[test]
    fn the_report_names_what_puts_a_camera_on_ffmpeg() {
        let mut c = cfg("rtsp://192.0.2.9/main");
        c.overlay = true;
        assert!(picture_settings(&c));
        let w = crate::engine::ffpath::why_ffmpeg(&c, "ffmpeg", None).unwrap();
        assert!(w.contains("burn it into recordings"), "{w}");
        let j = settings(&c);
        assert_eq!(j["overlay"], true);
        assert!(j.get("privacy_masks").is_none() && j.get("mask_mode").is_none());
        assert!(crate::engine::ffpath::why_ffmpeg(&cfg("rtsp://192.0.2.9/main"), "native", None).is_none());
    }

    #[test]
    fn the_report_shows_process_limits_and_leaves_them_out_when_unreadable() {
        let l = crate::proclimit::ProcLimits { pids_max: Some("100".into()), pids_current: Some(97), nproc: Some("63000".into()) };
        let sys = system_with(&l, Some(24));
        assert_eq!(sys["process_limits"]["pids_max"], "100");
        assert_eq!(sys["cpu_cores"], 24);
        assert_eq!(process_limits_line(&sys).unwrap(), "cgroup pids.max 100, in use 97; ulimit -u 63000");
        let none = system_with(&Default::default(), None);
        assert!(none["process_limits"].is_null() && process_limits_line(&none).is_none());
        let r = json!({ "system": sys, "camera": {}, "state": { "ffmpeg_reason": "its ingest setting is \"ffmpeg\"" } });
        let t = summary(&r);
        assert!(t.contains("Process and thread limits: cgroup pids.max 100, in use 97") && t.contains("Processor cores: 24") && t.contains("Why ffmpeg: its ingest"), "{t}");
    }

    #[test]
    fn reads_ffmpeg_stream_lines() {
        let err = "Input #0, rtsp, from 'rtsp://h/x':\n  Duration: N/A, start: 0.1, bitrate: N/A\n  \
            Stream #0:0: Video: h264 (High), yuv420p(progressive), 2560x1440, 20 fps, 20 tbr, 90k tbn\n  \
            Stream #0:1: Audio: aac (LC), 16000 Hz, mono, fltp\n  \
            Stream #0:2: Audio: pcm_mulaw, 8000 Hz, 1 channels, s16, 64 kb/s\n";
        let s = parse_ffmpeg_streams(err);
        assert_eq!(s.len(), 3);
        assert_eq!((s[0].codec.as_str(), s[0].width, s[0].height, s[0].fps), ("h264", Some(2560), Some(1440), Some(20.0)));
        assert_eq!((s[1].codec.as_str(), s[1].sample_rate, s[1].channels.as_deref()), ("aac", Some(16000), Some("mono")));
        assert_eq!((s[2].codec.as_str(), s[2].sample_rate, s[2].channels.as_deref()), ("pcm_mulaw", Some(8000), Some("1 channels")));
    }

    #[test]
    fn one_report_per_camera_at_a_time() {
        let a = begin(4242).expect("first");
        assert!(begin(4242).is_none());
        assert!(begin(4243).is_some());
        drop(a);
        assert!(begin(4242).is_some());
        // Two at a time in all.
        let (a, b) = (begin(4242).unwrap(), begin(4243).unwrap());
        assert!(begin(4244).is_none());
        drop(b);
        assert!(begin(4244).is_some());
        // An ONVIF lookup that outlives its report keeps its place.
        let o1 = OnvifBusy::new();
        let o2 = OnvifBusy::new();
        drop(a);
        assert!(begin(4245).is_none());
        drop(o1);
        assert!(begin(4245).is_some());
        drop(o2);
    }

    #[test]
    fn hint_once_after_failures() {
        let name = "hint test cam";
        for _ in 0..HINT_AFTER - 1 {
            note_end(name, "x", false);
        }
        assert!(!notes().lock().unwrap()[name].hinted);
        note_end(name, "x", true);
        for _ in 0..HINT_AFTER {
            note_end(name, "x", false);
        }
        assert!(notes().lock().unwrap()[name].hinted);
        assert_eq!(notes().lock().unwrap()[name].ends.len(), ENDS_KEPT);
        assert_eq!(hint_text("Door"), "Settings > Cameras > Door > Camera report collects details for a bug report");
    }
}
