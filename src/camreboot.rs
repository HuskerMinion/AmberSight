//! Restarting a camera, and cutting its power, over the network (used by
//! the camera watchdog, engine/watchdog.rs, and the "Reboot now" button).
//!
//! Restart, by what the camera is:
//! - ONVIF (any brand that speaks it): `SystemReboot` on the device
//!   service, signed in with a WS-Security password digest.
//! - Reolink: its HTTP API (`Login`, then `Reboot` with the token).
//! - Dahua / Amcrest: `GET /cgi-bin/magicBox.cgi?action=reboot` (HTTP digest).
//! - Hikvision: `PUT /ISAPI/System/reboot` (HTTP digest).
//!
//! Power cycle (a smart plug or PoE switch port): Shelly Gen1
//! (`/relay/N?turn=off|on`), Shelly Gen2/Plus/Pro (RPC `Switch.Set`),
//! Tasmota (`/cm?cmnd=Power Off|On`), or any device with an "off" address and
//! an "on" address. Off, wait (10 s by default), on.
//!
//! Devices on the home network often use self-signed certificates, so
//! certificate checks are off here (we only ever talk to the address the
//! owner typed in).

use std::net::{TcpStream, ToSocketAddrs};
use std::sync::atomic::AtomicBool;
use std::time::Duration;

use anyhow::{anyhow, bail, Context, Result};

use crate::config::{CameraConfig, PowerSwitch};

/// How long one request may take.
const TIMEOUT: Duration = Duration::from_secs(10);

/// How a camera is restarted.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Method {
    Onvif,
    Reolink,
    Dahua,
    Hikvision,
    /// Cannot be restarted remotely (or the owner said not to).
    None,
}

impl Method {
    pub fn label(self) -> &'static str {
        match self {
            Method::Onvif => "ONVIF",
            Method::Reolink => "Reolink",
            Method::Dahua => "Dahua/Amcrest",
            Method::Hikvision => "Hikvision",
            Method::None => "none",
        }
    }
    pub fn parse(s: &str) -> Option<Method> {
        Some(match s.trim().to_ascii_lowercase().as_str() {
            "onvif" => Method::Onvif,
            "reolink" => Method::Reolink,
            "dahua" | "amcrest" => Method::Dahua,
            "hikvision" | "hik" => Method::Hikvision,
            "none" | "off" => Method::None,
            _ => return None,
        })
    }
}

/// A camera reached over the network (not a screen, a webcam or a pushed stream).
pub fn is_network_camera(cfg: &CameraConfig) -> bool {
    let u = cfg.url.trim().to_ascii_lowercase();
    (u.starts_with("rtsp") || u.starts_with("http")) && crate::onvif::host_from_rtsp_url(&cfg.url).is_some()
}

/// The restart method for a camera: the one chosen in its settings, else
/// guessed from its stream address, with ONVIF as the fallback.
pub fn pick_method(cfg: &CameraConfig) -> Method {
    if !is_network_camera(cfg) {
        return Method::None;
    }
    let chosen = cfg.watchdog.as_ref().and_then(|w| w.reboot.as_deref()).unwrap_or("auto");
    if let Some(m) = Method::parse(chosen) {
        return m;
    }
    let u = format!("{} {}", cfg.url, cfg.sub_url.as_deref().unwrap_or("")).to_ascii_lowercase();
    if u.contains("preview_0") || u.contains("h264preview") || u.contains("h265preview") || u.contains(".bcs") || u.contains("app=bcs") || cfg.reolink_channel.is_some() {
        // A camera behind a Reolink hub or recorder: restarting the hub would
        // take every camera on it down, so only when chosen by hand.
        if cfg.reolink_channel.is_some() {
            return Method::None;
        }
        return Method::Reolink;
    }
    if u.contains("/cam/realmonitor") {
        return Method::Dahua;
    }
    if u.contains("/streaming/channels") || u.contains("/isapi/") {
        return Method::Hikvision;
    }
    Method::Onvif
}

/// Where and how to sign in to a camera.
#[derive(Debug, Clone)]
pub struct Target {
    pub host: String,
    pub user: String,
    pub pass: String,
    /// The camera's web port, when set in its watchdog settings.
    pub http_port: Option<u16>,
    pub onvif_port: Option<u16>,
}

/// The camera's address and sign-in from its settings (call with a
/// normalized config: `engine::normalize_creds`).
pub fn target(cfg: &CameraConfig) -> Result<Target> {
    let host = crate::onvif::host_from_rtsp_url(&cfg.url).ok_or_else(|| anyhow!("the camera's address has no host"))?;
    let (mut user, mut pass) = (cfg.username.clone(), cfg.password.clone());
    if user.is_empty() {
        if let Some((u, p)) = crate::onvif::credentials_from_url(&cfg.url) {
            user = u;
            pass = p;
        }
    }
    Ok(Target { host, user, pass, http_port: cfg.watchdog.as_ref().and_then(|w| w.http_port), onvif_port: cfg.onvif_port })
}

/// "host" or "[v6]" with ":port" when given.
fn host_port(host: &str, port: Option<u16>) -> String {
    let h = if host.contains(':') && !host.starts_with('[') { format!("[{host}]") } else { host.to_string() };
    match port {
        Some(p) => format!("{h}:{p}"),
        None => h,
    }
}

/// Restart one camera. Returns the method used. Blocking (a few seconds).
pub fn reboot(cfg: &CameraConfig) -> Result<Method> {
    let m = pick_method(cfg);
    if m == Method::None {
        bail!("this camera has no way to be restarted remotely (set a restart method in its Watchdog settings)");
    }
    let t = target(cfg)?;
    reboot_with(m, &t)?;
    Ok(m)
}

pub fn reboot_with(m: Method, t: &Target) -> Result<()> {
    match m {
        Method::Onvif => {
            // The port named in the settings, else the usual ones.
            let mut ports: Vec<u16> = [t.onvif_port, t.http_port].into_iter().flatten().collect();
            ports.dedup();
            if ports.is_empty() {
                ports = vec![80, 8000];
            }
            onvif_reboot(&t.host, &ports, &t.user, &t.pass)
        }
        Method::Reolink => {
            let hp = host_port(&t.host, t.http_port);
            reolink_reboot(&[format!("https://{hp}"), format!("http://{hp}")], &t.user, &t.pass)
        }
        Method::Dahua => dahua_reboot(&format!("http://{}", host_port(&t.host, t.http_port)), &t.user, &t.pass),
        Method::Hikvision => hikvision_reboot(&format!("http://{}", host_port(&t.host, t.http_port)), &t.user, &t.pass),
        Method::None => bail!("no restart method"),
    }
}

const NS_DEVICE: &str = "http://www.onvif.org/ver10/device/wsdl";

/// ONVIF `SystemReboot` (the device service).
pub fn onvif_reboot(host: &str, ports: &[u16], user: &str, pass: &str) -> Result<()> {
    let stop = AtomicBool::new(false);
    let mut last = anyhow!("no ONVIF port to try");
    for &port in ports {
        let creds = crate::onvif::Credentials { host: host.to_string(), port, username: user.to_string(), password: pass.to_string() };
        let mut c = crate::onvif::Client::new(&creds, &stop);
        // Sign with the camera's own clock (a wrong clock fails the sign-in).
        c.sync_clock();
        let url = format!("http://{}/onvif/device_service", host_port(host, Some(port)));
        match c.call(&url, &format!("{NS_DEVICE}/SystemReboot"), "<tds:SystemReboot/>", true, TIMEOUT) {
            Ok(_) => return Ok(()),
            Err(e) => last = e.context(format!("ONVIF restart on port {port}")),
        }
    }
    Err(last)
}

/// Reolink HTTP API: sign in, then `Reboot`. Each base address is tried in
/// turn (HTTPS first: Reolink serves the API there by default).
pub fn reolink_reboot(bases: &[String], user: &str, pass: &str) -> Result<()> {
    let mut last = anyhow!("no address to try");
    for b in bases {
        let mut c = crate::reolink::ReolinkClient::with_base(b, user, pass);
        match c.reboot() {
            Ok(()) => return Ok(()),
            Err(e) => last = e,
        }
    }
    Err(last.context("Reolink restart"))
}

/// Dahua / Amcrest CGI.
pub fn dahua_reboot(base: &str, user: &str, pass: &str) -> Result<()> {
    let url = format!("{}/cgi-bin/magicBox.cgi?action=reboot", base.trim_end_matches('/'));
    let a = request("GET", &url, None, &Auth::Login(user, pass)).context("Dahua restart")?;
    ok_status(&a).context("Dahua restart")
}

/// Hikvision ISAPI.
pub fn hikvision_reboot(base: &str, user: &str, pass: &str) -> Result<()> {
    let url = format!("{}/ISAPI/System/reboot", base.trim_end_matches('/'));
    let a = request("PUT", &url, Some(""), &Auth::Login(user, pass)).context("Hikvision restart")?;
    ok_status(&a).context("Hikvision restart")
}

fn ok_status(a: &Answer) -> Result<()> {
    match a.status {
        200..=299 => Ok(()),
        401 | 403 => bail!("the camera refused the sign-in (HTTP {}): check the camera's user and password", a.status),
        404 => bail!("the camera does not know this command (HTTP 404): pick another restart method"),
        s => bail!("HTTP {s}{}", short_body(&a.body)),
    }
}

fn short_body(b: &str) -> String {
    let t: String = b.trim().chars().take(120).collect();
    if t.is_empty() { String::new() } else { format!(": {t}") }
}

// ---------------------------------------------------------------------------
// Power cycle
// ---------------------------------------------------------------------------

/// One request to a plug.
#[derive(Debug, Clone, PartialEq)]
pub struct PlugRequest {
    pub method: &'static str,
    pub url: String,
    pub auth: PlugAuth,
}

#[derive(Debug, Clone, PartialEq)]
pub enum PlugAuth {
    None,
    /// HTTP Basic, sent with the request.
    Basic(String, String),
    /// Whatever the plug asks for (Digest or Basic) after a 401.
    Login(String, String),
}

/// "http://host[:port]" for a plug's address as typed ("10.0.0.9",
/// "10.0.0.9:8080", "http://plug.local/").
fn plug_base(host: &str) -> Result<String> {
    let h = host.trim().trim_end_matches('/');
    if h.is_empty() {
        bail!("the plug's address is empty");
    }
    if h.starts_with("http://") || h.starts_with("https://") {
        return Ok(h.to_string());
    }
    if h.contains('/') || h.contains(' ') {
        bail!("the plug's address should be like 192.168.1.50");
    }
    Ok(format!("http://{h}"))
}

fn enc(s: &str) -> String {
    percent_encoding::utf8_percent_encode(s, percent_encoding::NON_ALPHANUMERIC).to_string()
}

/// The "off" and "on" requests for a plug.
pub fn power_requests(p: &PowerSwitch) -> Result<(PlugRequest, PlugRequest)> {
    let user = p.username.trim().to_string();
    let pass = p.password.clone();
    let has_login = !user.is_empty() || !pass.is_empty();
    let secs = p.off_secs();
    let ch = p.channel;
    Ok(match p.kind.trim().to_ascii_lowercase().as_str() {
        "shelly" | "shelly1" => {
            // Gen1: Basic sign-in. `timer` turns it back on by itself after
            // the wait, even if our "on" never arrives.
            let b = plug_base(&p.host)?;
            let auth = if has_login { PlugAuth::Basic(if user.is_empty() { "admin".into() } else { user }, pass) } else { PlugAuth::None };
            (
                PlugRequest { method: "GET", url: format!("{b}/relay/{ch}?turn=off&timer={secs}"), auth: auth.clone() },
                PlugRequest { method: "GET", url: format!("{b}/relay/{ch}?turn=on"), auth },
            )
        }
        "shelly2" | "shelly_gen2" | "shellyplus" => {
            // Gen2 / Plus / Pro: RPC over GET; Digest sign-in (user "admin").
            // `toggle_after` turns it back on by itself after the wait.
            let b = plug_base(&p.host)?;
            let auth = if has_login { PlugAuth::Login(if user.is_empty() { "admin".into() } else { user }, pass) } else { PlugAuth::None };
            (
                PlugRequest { method: "GET", url: format!("{b}/rpc/Switch.Set?id={ch}&on=false&toggle_after={secs}"), auth: auth.clone() },
                PlugRequest { method: "GET", url: format!("{b}/rpc/Switch.Set?id={ch}&on=true"), auth },
            )
        }
        "tasmota" => {
            // Tasmota takes its web sign-in in the address.
            let b = plug_base(&p.host)?;
            let relay = if ch == 0 { "Power".to_string() } else { format!("Power{}", ch + 1) };
            let login = if has_login { format!("&user={}&password={}", enc(if user.is_empty() { "admin" } else { &user }), enc(&pass)) } else { String::new() };
            (
                PlugRequest { method: "GET", url: format!("{b}/cm?cmnd={relay}%20Off{login}"), auth: PlugAuth::None },
                PlugRequest { method: "GET", url: format!("{b}/cm?cmnd={relay}%20On{login}"), auth: PlugAuth::None },
            )
        }
        "urls" | "url" | "generic" => {
            let method = match p.method.as_deref().map(|m| m.trim().to_ascii_uppercase()) {
                Some(m) if m == "POST" => "POST",
                _ => "GET",
            };
            for u in [&p.off_url, &p.on_url] {
                let l = u.trim().to_ascii_lowercase();
                if !(l.starts_with("http://") || l.starts_with("https://")) {
                    bail!("the off and on addresses must start with http:// or https://");
                }
            }
            let auth = if !user.is_empty() { PlugAuth::Basic(user, pass) } else { PlugAuth::None };
            (
                PlugRequest { method, url: p.off_url.trim().to_string(), auth: auth.clone() },
                PlugRequest { method, url: p.on_url.trim().to_string(), auth },
            )
        }
        other => bail!("unknown kind of power switch '{other}' (shelly, shelly2, tasmota or urls)"),
    })
}

fn send_plug(r: &PlugRequest) -> Result<()> {
    let auth = match &r.auth {
        PlugAuth::None => Auth::None,
        PlugAuth::Basic(u, p) => Auth::Basic(u, p),
        PlugAuth::Login(u, p) => Auth::Login(u, p),
    };
    let body = if r.method == "GET" { None } else { Some("") };
    let a = request(r.method, &r.url, body, &auth)?;
    ok_status(&a)
}

/// Turn the power off, wait, and turn it on again. `sleep` is how waiting
/// happens (tests pass one that does not wait).
pub fn power_cycle(p: &PowerSwitch, sleep: &dyn Fn(Duration)) -> Result<()> {
    let (off, on) = power_requests(p)?;
    send_plug(&off).context("turning the power off")?;
    sleep(Duration::from_secs(p.off_secs()));
    // Turning it back on matters most: try a few times.
    let mut last = None;
    for attempt in 0..3 {
        if attempt > 0 {
            sleep(Duration::from_secs(3));
        }
        match send_plug(&on) {
            Ok(()) => return Ok(()),
            Err(e) => last = Some(e),
        }
    }
    Err(last.unwrap_or_else(|| anyhow!("no answer")).context("turning the power back ON failed: the camera may still be without power"))
}

// ---------------------------------------------------------------------------
// Does the camera answer at all?
// ---------------------------------------------------------------------------

/// True when the camera accepts a connection on its stream port or its web
/// port (it is on the network, even if its video stopped).
pub fn answers(cfg: &CameraConfig) -> bool {
    let Ok(u) = url::Url::parse(cfg.url.trim()) else { return false };
    let Some(host) = u.host_str().map(|h| h.trim_matches(|c| c == '[' || c == ']').to_string()) else { return false };
    let stream_port = u.port().unwrap_or(if u.scheme().starts_with("rtsp") { 554 } else if u.scheme() == "https" { 443 } else { 80 });
    let mut ports = vec![stream_port];
    for p in [cfg.watchdog.as_ref().and_then(|w| w.http_port), cfg.onvif_port, Some(80)].into_iter().flatten() {
        if !ports.contains(&p) {
            ports.push(p);
        }
    }
    ports.iter().any(|&p| tcp_answers(&host, p))
}

fn tcp_answers(host: &str, port: u16) -> bool {
    let Ok(addrs) = (host, port).to_socket_addrs() else { return false };
    addrs.take(2).any(|a| TcpStream::connect_timeout(&a, Duration::from_secs(3)).is_ok())
}

// ---------------------------------------------------------------------------
// Small HTTP client with Basic and Digest sign-in
// ---------------------------------------------------------------------------

pub(crate) enum Auth<'a> {
    None,
    /// Basic, sent up front.
    Basic(&'a str, &'a str),
    /// Sent without sign-in first; after a 401, Digest (MD5 or SHA-256) or
    /// Basic, whichever the device asks for.
    Login(&'a str, &'a str),
}

pub(crate) struct Answer {
    pub status: u16,
    pub body: String,
}

fn agent() -> ureq::Agent {
    let tls = ureq::tls::TlsConfig::builder().disable_verification(true).build();
    ureq::Agent::config_builder()
        .tls_config(tls)
        .timeout_global(Some(TIMEOUT))
        .http_status_as_error(false)
        // Devices on the home network are never reached through a proxy.
        .proxy(None)
        .max_redirects(0)
        .build()
        .new_agent()
}

fn basic(user: &str, pass: &str) -> String {
    use base64::Engine as _;
    format!("Basic {}", base64::engine::general_purpose::STANDARD.encode(format!("{user}:{pass}")))
}

/// One request; returns the status, the WWW-Authenticate headers and the body.
fn send_once(agent: &ureq::Agent, method: &str, url: &str, body: Option<&str>, authz: Option<&str>) -> Result<(u16, Vec<String>, String)> {
    let mut resp = match method {
        "GET" => {
            let mut r = agent.get(url);
            if let Some(a) = authz {
                r = r.header("Authorization", a);
            }
            r.call()
        }
        "PUT" | "POST" => {
            let mut r = if method == "PUT" { agent.put(url) } else { agent.post(url) };
            if let Some(a) = authz {
                r = r.header("Authorization", a);
            }
            r.send(body.unwrap_or("").to_string())
        }
        m => bail!("unsupported method {m}"),
    }
    .map_err(|e| anyhow!("{}: {e}", crate::engine::redact_url(url.to_string())))?;
    let status = resp.status().as_u16();
    let www: Vec<String> = resp.headers().get_all("www-authenticate").iter().filter_map(|v| v.to_str().ok().map(String::from)).collect();
    let text = resp.body_mut().read_to_string().unwrap_or_default();
    Ok((status, www, text))
}

pub(crate) fn request(method: &str, url: &str, body: Option<&str>, auth: &Auth) -> Result<Answer> {
    let agent = agent();
    let first = match auth {
        Auth::Basic(u, p) => Some(basic(u, p)),
        _ => None,
    };
    let (status, www, text) = send_once(&agent, method, url, body, first.as_deref())?;
    if status == 401 {
        if let Auth::Login(user, pass) = auth {
            let parsed = url::Url::parse(url).context("bad address")?;
            let mut uri = parsed.path().to_string();
            if let Some(q) = parsed.query() {
                uri.push('?');
                uri.push_str(q);
            }
            let authz = if let Some(ch) = www.iter().filter_map(|h| parse_challenge(h)).find(|c| c.supported()) {
                let cnonce: String = crate::onvif::random_bytes::<8>().iter().map(|b| format!("{b:02x}")).collect();
                digest_header(&ch, user, pass, method, &uri, 1, &cnonce)
            } else if www.iter().any(|h| h.trim_start().to_ascii_lowercase().starts_with("basic")) {
                basic(user, pass)
            } else {
                return Ok(Answer { status, body: text });
            };
            let (status, _, text) = send_once(&agent, method, url, body, Some(&authz))?;
            return Ok(Answer { status, body: text });
        }
    }
    Ok(Answer { status, body: text })
}

/// An HTTP Digest challenge (RFC 7616).
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Challenge {
    pub realm: String,
    pub nonce: String,
    pub opaque: Option<String>,
    pub qop_auth: bool,
    /// As the device wrote it ("MD5", "SHA-256"); None = MD5.
    pub algorithm: Option<String>,
}

impl Challenge {
    fn sha256(&self) -> bool {
        self.algorithm.as_deref().is_some_and(|a| a.eq_ignore_ascii_case("SHA-256"))
    }
    fn supported(&self) -> bool {
        self.algorithm.as_deref().is_none_or(|a| a.eq_ignore_ascii_case("MD5") || a.eq_ignore_ascii_case("SHA-256"))
    }
}

pub(crate) fn parse_challenge(header: &str) -> Option<Challenge> {
    let h = header.trim();
    if !h.get(..6).is_some_and(|p| p.eq_ignore_ascii_case("digest")) {
        return None;
    }
    let mut s = h[6..].trim();
    let mut map = std::collections::HashMap::new();
    while !s.is_empty() {
        let eq = s.find('=')?;
        let key = s[..eq].trim().trim_start_matches(',').trim().to_ascii_lowercase();
        let after = s[eq + 1..].trim_start();
        let (val, remain) = if let Some(q) = after.strip_prefix('"') {
            let end = q.find('"')?;
            (q[..end].to_string(), &q[end + 1..])
        } else {
            let end = after.find(',').unwrap_or(after.len());
            (after[..end].trim().to_string(), &after[end..])
        };
        map.insert(key, val);
        s = remain.trim_start().trim_start_matches(',').trim_start();
    }
    Some(Challenge {
        realm: map.get("realm")?.clone(),
        nonce: map.get("nonce")?.clone(),
        opaque: map.get("opaque").cloned(),
        qop_auth: map.get("qop").is_some_and(|q| q.split(',').any(|x| x.trim() == "auth")),
        algorithm: map.get("algorithm").cloned(),
    })
}

fn hex_hash(sha256: bool, s: &str) -> String {
    if sha256 {
        use sha2::Digest as _;
        sha2::Sha256::digest(s.as_bytes()).iter().map(|b| format!("{b:02x}")).collect()
    } else {
        use md5::Digest as _;
        md5::Md5::digest(s.as_bytes()).iter().map(|b| format!("{b:02x}")).collect()
    }
}

/// The `Authorization: Digest ...` value for one request.
pub(crate) fn digest_header(ch: &Challenge, user: &str, pass: &str, method: &str, uri: &str, nc: u32, cnonce: &str) -> String {
    let sha = ch.sha256();
    let ha1 = hex_hash(sha, &format!("{user}:{}:{pass}", ch.realm));
    let ha2 = hex_hash(sha, &format!("{method}:{uri}"));
    let mut h = format!("Digest username=\"{user}\", realm=\"{}\", nonce=\"{}\", uri=\"{uri}\"", ch.realm, ch.nonce);
    if let Some(a) = &ch.algorithm {
        h += &format!(", algorithm={a}");
    }
    if ch.qop_auth {
        let resp = hex_hash(sha, &format!("{ha1}:{}:{nc:08x}:{cnonce}:auth:{ha2}", ch.nonce));
        h += &format!(", qop=auth, nc={nc:08x}, cnonce=\"{cnonce}\", response=\"{resp}\"");
    } else {
        h += &format!(", response=\"{}\"", hex_hash(sha, &format!("{ha1}:{}:{ha2}", ch.nonce)));
    }
    if let Some(o) = &ch.opaque {
        h += &format!(", opaque=\"{o}\"");
    }
    h
}

#[cfg(test)]
mod tests {
    //! Everything here talks to a tiny HTTP server on 127.0.0.1 started by
    //! the test itself: no real camera or plug is ever contacted.
    use super::*;
    use std::io::{Read, Write};
    use std::net::TcpListener;
    use std::sync::{Arc, Mutex};

    #[derive(Debug, Clone)]
    struct Seen {
        method: String,
        target: String,
        headers: Vec<(String, String)>,
        body: String,
    }

    impl Seen {
        fn header(&self, k: &str) -> Option<&str> {
            self.headers.iter().find(|(n, _)| n.eq_ignore_ascii_case(k)).map(|(_, v)| v.as_str())
        }
    }

    type Log = Arc<Mutex<Vec<Seen>>>;

    fn read_request(s: &mut TcpStream) -> Option<Seen> {
        let mut buf = Vec::new();
        let mut chunk = [0u8; 4096];
        let head_end = loop {
            let n = s.read(&mut chunk).ok()?;
            if n == 0 {
                return None;
            }
            buf.extend_from_slice(&chunk[..n]);
            if let Some(i) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
                break i + 4;
            }
        };
        let head = String::from_utf8_lossy(&buf[..head_end]).to_string();
        let mut lines = head.split("\r\n");
        let first = lines.next()?;
        let mut parts = first.split(' ');
        let method = parts.next()?.to_string();
        let target = parts.next()?.to_string();
        let headers: Vec<(String, String)> =
            lines.filter_map(|l| l.split_once(':').map(|(k, v)| (k.trim().to_string(), v.trim().to_string()))).collect();
        let len: usize = headers.iter().find(|(k, _)| k.eq_ignore_ascii_case("content-length")).and_then(|(_, v)| v.parse().ok()).unwrap_or(0);
        while buf.len() < head_end + len {
            let n = s.read(&mut chunk).ok()?;
            if n == 0 {
                break;
            }
            buf.extend_from_slice(&chunk[..n]);
        }
        let body = String::from_utf8_lossy(&buf[head_end..(head_end + len).min(buf.len())]).to_string();
        Some(Seen { method, target, headers, body })
    }

    /// A local server answering with `reply(request, index)` (status line +
    /// extra headers, body). Returns its port and what it received.
    fn mock(reply: impl Fn(&Seen, usize) -> (String, String) + Send + 'static) -> (u16, Log) {
        let l = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = l.local_addr().unwrap().port();
        let log: Log = Arc::new(Mutex::new(Vec::new()));
        let log2 = log.clone();
        std::thread::spawn(move || {
            for (i, conn) in l.incoming().enumerate() {
                let Ok(mut s) = conn else { continue };
                let Some(req) = read_request(&mut s) else { continue };
                log2.lock().unwrap().push(req.clone());
                let (head, body) = reply(&req, i);
                let resp = format!("HTTP/1.1 {head}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len());
                let _ = s.write_all(resp.as_bytes());
            }
        });
        (port, log)
    }

    fn md5h(s: &str) -> String {
        use md5::Digest as _;
        md5::Md5::digest(s.as_bytes()).iter().map(|b| format!("{b:02x}")).collect()
    }

    /// Fields of an Authorization: Digest header.
    fn digest_fields(h: &str) -> std::collections::HashMap<String, String> {
        let mut m = std::collections::HashMap::new();
        for part in h.trim_start_matches("Digest ").split(", ") {
            if let Some((k, v)) = part.split_once('=') {
                m.insert(k.to_string(), v.trim_matches('"').to_string());
            }
        }
        m
    }

    /// A camera that wants HTTP Digest (MD5, qop=auth) for user cam / pw "s3cret:x".
    fn digest_camera(answer_ok: &'static str) -> (u16, Log) {
        mock(move |req, _| {
            let Some(a) = req.header("Authorization") else {
                return ("401 Unauthorized\r\nWWW-Authenticate: Digest realm=\"Login to 1234\", qop=\"auth\", nonce=\"n0nce\", opaque=\"op\"".into(), String::new());
            };
            let f = digest_fields(a);
            // Checked here from the RFC formula, independently of the code under test.
            let ha1 = md5h("cam:Login to 1234:s3cret:x");
            let ha2 = md5h(&format!("{}:{}", req.method, f["uri"]));
            let want = md5h(&format!("{ha1}:n0nce:{}:{}:auth:{ha2}", f["nc"], f["cnonce"]));
            if f["response"] == want && f["uri"] == req.target && f["opaque"] == "op" {
                ("200 OK".into(), answer_ok.into())
            } else {
                ("401 Unauthorized".into(), String::new())
            }
        })
    }

    #[test]
    fn digest_known_answers() {
        // RFC 2617 section 3.5.
        let ch = Challenge { realm: "testrealm@host.com".into(), nonce: "dcd98b7102dd2f0e8b11d0f600bfb0c093".into(), opaque: Some("5ccc069c403ebaf9f0171e9517f40e41".into()), qop_auth: true, algorithm: None };
        let h = digest_header(&ch, "Mufasa", "Circle Of Life", "GET", "/dir/index.html", 1, "0a4f113b");
        assert!(h.contains("response=\"6629fae49393a05397450978507c4ef1\""), "{h}");
        assert!(h.contains("nc=00000001") && h.contains("opaque=\"5ccc069c403ebaf9f0171e9517f40e41\""));
        // RFC 7616 section 3.9.1 (SHA-256 and MD5).
        let mut ch = Challenge {
            realm: "http-auth@example.org".into(),
            nonce: "7ypf/xlj9XXwfDPEoM4URrv/xwf94BcCAzFZH4GiTo0v".into(),
            opaque: None,
            qop_auth: true,
            algorithm: Some("SHA-256".into()),
        };
        let cn = "f2/wE4q74E6zIJEtWaHKaf5wv/H5QzzpXusqGemxURZJ";
        let h = digest_header(&ch, "Mufasa", "Circle of Life", "GET", "/dir/index.html", 1, cn);
        assert!(h.contains("response=\"753927fa0e85d155564e2e272a28d1802ca10daf4496794697cf8db5856cb6c1\""), "{h}");
        assert!(h.contains("algorithm=SHA-256"));
        ch.algorithm = Some("MD5".into());
        let h = digest_header(&ch, "Mufasa", "Circle of Life", "GET", "/dir/index.html", 1, cn);
        assert!(h.contains("response=\"8ca523f5e9506fed4657c9700eebdbec\""), "{h}");
    }

    #[test]
    fn parses_challenges() {
        let c = parse_challenge(r#"Digest realm="Login to 1234", qop="auth,auth-int", nonce="abc", opaque="x""#).unwrap();
        assert_eq!((c.realm.as_str(), c.nonce.as_str(), c.qop_auth, c.opaque.as_deref()), ("Login to 1234", "abc", true, Some("x")));
        let c = parse_challenge(r#"Digest qop="auth", realm="shellyplus1-abc", nonce="60dc59c6", algorithm=SHA-256"#).unwrap();
        assert!(c.sha256() && c.supported());
        assert!(parse_challenge("Basic realm=\"x\"").is_none());
        assert!(!parse_challenge(r#"Digest realm="r", nonce="n", algorithm=SHA-512-256"#).unwrap().supported());
    }

    #[test]
    fn dahua_reboot_signs_in_with_digest() {
        let (port, log) = digest_camera("OK");
        dahua_reboot(&format!("http://127.0.0.1:{port}"), "cam", "s3cret:x").unwrap();
        let log = log.lock().unwrap();
        assert_eq!(log.len(), 2, "one try without sign-in, one with");
        for r in log.iter() {
            assert_eq!(r.method, "GET");
            assert_eq!(r.target, "/cgi-bin/magicBox.cgi?action=reboot");
        }
        assert!(log[0].header("Authorization").is_none());
        let f = digest_fields(log[1].header("Authorization").unwrap());
        assert_eq!((f["username"].as_str(), f["realm"].as_str(), f["qop"].as_str(), f["nc"].as_str()), ("cam", "Login to 1234", "auth", "00000001"));
    }

    #[test]
    fn hikvision_reboot_is_a_put_with_digest() {
        let (port, log) = digest_camera("<ResponseStatus><statusCode>1</statusCode></ResponseStatus>");
        hikvision_reboot(&format!("http://127.0.0.1:{port}/"), "cam", "s3cret:x").unwrap();
        let log = log.lock().unwrap();
        assert_eq!(log.len(), 2);
        assert!(log.iter().all(|r| r.method == "PUT" && r.target == "/ISAPI/System/reboot"));
        assert!(log[1].header("Authorization").unwrap().starts_with("Digest username=\"cam\""));
    }

    #[test]
    fn wrong_password_is_reported_plainly() {
        let (port, _log) = digest_camera("OK");
        let e = dahua_reboot(&format!("http://127.0.0.1:{port}"), "cam", "wrong").unwrap_err();
        assert!(format!("{e:#}").contains("refused the sign-in"), "{e:#}");
    }

    #[test]
    fn reolink_reboot_logs_in_then_reboots() {
        let (port, log) = mock(|req, _| {
            if req.target == "/cgi-bin/api.cgi?cmd=Login" {
                ("200 OK\r\nContent-Type: application/json".into(), r#"[{"cmd":"Login","code":0,"value":{"Token":{"leaseTime":3600,"name":"tok123"}}}]"#.into())
            } else if req.target == "/cgi-bin/api.cgi?cmd=Reboot&token=tok123" {
                ("200 OK\r\nContent-Type: application/json".into(), r#"[{"cmd":"Reboot","code":0,"value":{"rspCode":200}}]"#.into())
            } else {
                ("404 Not Found".into(), String::new())
            }
        });
        reolink_reboot(&[format!("http://127.0.0.1:{port}")], "admin", "pw\"1").unwrap();
        let log = log.lock().unwrap();
        assert_eq!(log.len(), 2);
        assert!(log.iter().all(|r| r.method == "POST"));
        let login: serde_json::Value = serde_json::from_str(&log[0].body).unwrap();
        assert_eq!(login, serde_json::json!([{ "cmd": "Login", "param": { "User": { "Version": "0", "userName": "admin", "password": "pw\"1" } } }]));
        let reboot: serde_json::Value = serde_json::from_str(&log[1].body).unwrap();
        assert_eq!(reboot, serde_json::json!([{ "cmd": "Reboot", "action": 0, "param": {} }]));
    }

    #[test]
    fn reolink_refusal_is_an_error() {
        let (port, _log) = mock(|req, _| {
            if req.target.contains("cmd=Login") {
                ("200 OK".into(), r#"[{"cmd":"Login","code":1,"error":{"detail":"login failed","rspCode":-7}}]"#.into())
            } else {
                ("500 Oops".into(), String::new())
            }
        });
        let e = reolink_reboot(&[format!("http://127.0.0.1:{port}")], "admin", "bad").unwrap_err();
        assert!(format!("{e:#}").contains("login failed"), "{e:#}");
    }

    #[test]
    fn onvif_reboot_sends_system_reboot_with_a_valid_password_digest() {
        let (port, log) = mock(|req, _| {
            if req.body.contains("GetSystemDateAndTime") {
                // No clock offered: the client signs with its own clock.
                ("500 Internal Server Error".into(), String::new())
            } else {
                ("200 OK\r\nContent-Type: application/soap+xml".into(),
                 r#"<s:Envelope xmlns:s="http://www.w3.org/2003/05/soap-envelope"><s:Body><tds:SystemRebootResponse xmlns:tds="http://www.onvif.org/ver10/device/wsdl"><tds:Message>Rebooting</tds:Message></tds:SystemRebootResponse></s:Body></s:Envelope>"#.into())
            }
        });
        onvif_reboot("127.0.0.1", &[port], "onv", "pässword").unwrap();
        let log = log.lock().unwrap();
        let r = log.iter().find(|r| r.body.contains("SystemReboot")).expect("a SystemReboot request");
        assert_eq!((r.method.as_str(), r.target.as_str()), ("POST", "/onvif/device_service"));
        assert!(r.header("Content-Type").unwrap().contains("action=\"http://www.onvif.org/ver10/device/wsdl/SystemReboot\""));
        let doc = roxmltree::Document::parse(&r.body).unwrap();
        let body = doc.descendants().find(|n| n.tag_name().name() == "Body").unwrap();
        let op = body.children().find(|n| n.is_element()).unwrap();
        assert_eq!((op.tag_name().name(), op.tag_name().namespace()), ("SystemReboot", Some(NS_DEVICE)));
        assert_eq!(op.children().filter(|n| n.is_element()).count(), 0);
        // The UsernameToken: Base64(SHA1(nonce + created + password)).
        let text = |name: &str| doc.descendants().find(|n| n.tag_name().name() == name).and_then(|n| n.text()).unwrap().to_string();
        assert_eq!(text("Username"), "onv");
        use base64::Engine as _;
        let nonce = base64::engine::general_purpose::STANDARD.decode(text("Nonce")).unwrap();
        assert_eq!(text("Password"), crate::onvif::password_digest(&nonce, &text("Created"), "pässword"));
    }

    #[test]
    fn onvif_soap_fault_is_an_error() {
        let (port, _log) = mock(|_, _| {
            ("400 Bad Request".into(), r#"<s:Envelope xmlns:s="http://www.w3.org/2003/05/soap-envelope"><s:Body><s:Fault><s:Code><s:Value>s:Sender</s:Value></s:Code><s:Reason><s:Text xml:lang="en">Sender not Authorized</s:Text></s:Reason></s:Fault></s:Body></s:Envelope>"#.into())
        });
        assert!(onvif_reboot("127.0.0.1", &[port], "u", "p").is_err());
    }

    fn plug(kind: &str) -> PowerSwitch {
        PowerSwitch { kind: kind.into(), host: "10.9.8.7".into(), ..Default::default() }
    }

    #[test]
    fn plug_requests_per_kind() {
        let (off, on) = power_requests(&plug("shelly")).unwrap();
        assert_eq!(off, PlugRequest { method: "GET", url: "http://10.9.8.7/relay/0?turn=off&timer=10".into(), auth: PlugAuth::None });
        assert_eq!(on.url, "http://10.9.8.7/relay/0?turn=on");

        let mut p = plug("shelly2");
        p.channel = 1;
        p.password = "pw".into();
        p.off_seconds = Some(15);
        let (off, on) = power_requests(&p).unwrap();
        assert_eq!(off.url, "http://10.9.8.7/rpc/Switch.Set?id=1&on=false&toggle_after=15");
        assert_eq!(on.url, "http://10.9.8.7/rpc/Switch.Set?id=1&on=true");
        assert_eq!(on.auth, PlugAuth::Login("admin".into(), "pw".into()));

        let (off, on) = power_requests(&plug("tasmota")).unwrap();
        assert_eq!(off.url, "http://10.9.8.7/cm?cmnd=Power%20Off");
        assert_eq!(on.url, "http://10.9.8.7/cm?cmnd=Power%20On");
        let mut p = plug("tasmota");
        p.channel = 2;
        p.username = "admin".into();
        p.password = "a&b".into();
        assert_eq!(power_requests(&p).unwrap().0.url, "http://10.9.8.7/cm?cmnd=Power3%20Off&user=admin&password=a%26b");

        let p = PowerSwitch { kind: "urls".into(), off_url: "http://sw/port/4/off".into(), on_url: "http://sw/port/4/on".into(), method: Some("post".into()), username: "u".into(), password: "p".into(), ..Default::default() };
        let (off, on) = power_requests(&p).unwrap();
        assert_eq!((off.method, off.url.as_str(), on.url.as_str()), ("POST", "http://sw/port/4/off", "http://sw/port/4/on"));
        assert_eq!(off.auth, PlugAuth::Basic("u".into(), "p".into()));

        assert!(power_requests(&PowerSwitch { kind: "urls".into(), off_url: "sw/off".into(), on_url: "http://sw/on".into(), ..Default::default() }).is_err());
        assert!(power_requests(&PowerSwitch { kind: "shelly".into(), ..Default::default() }).is_err());
        assert!(power_requests(&plug("kasa")).is_err());
    }

    #[test]
    fn shelly_power_cycle_off_wait_on() {
        let (port, log) = mock(|req, _| {
            // Gen1 answers 401 + Basic without a sign-in; we send Basic up front.
            match req.header("Authorization") {
                Some(a) if a == basic("admin", "plugpw") => ("200 OK".into(), r#"{"ison":false}"#.into()),
                _ => ("401 Unauthorized\r\nWWW-Authenticate: Basic realm=\"shelly\"".into(), String::new()),
            }
        });
        let p = PowerSwitch { kind: "shelly".into(), host: format!("127.0.0.1:{port}"), password: "plugpw".into(), ..Default::default() };
        let waited = Mutex::new(Vec::new());
        power_cycle(&p, &|d| waited.lock().unwrap().push(d.as_secs())).unwrap();
        let log = log.lock().unwrap();
        let targets: Vec<&str> = log.iter().map(|r| r.target.as_str()).collect();
        assert_eq!(targets, vec!["/relay/0?turn=off&timer=10", "/relay/0?turn=on"]);
        assert_eq!(*waited.lock().unwrap(), vec![10]);
    }

    #[test]
    fn shelly2_power_cycle_uses_sha256_digest() {
        let (port, log) = mock(|req, _| {
            let Some(a) = req.header("Authorization") else {
                return ("401 Unauthorized\r\nWWW-Authenticate: Digest qop=\"auth\", realm=\"shellyplus1-abc\", nonce=\"6f1d\", algorithm=SHA-256".into(), String::new());
            };
            let f = digest_fields(a);
            let sha = |s: &str| -> String {
                use sha2::Digest as _;
                sha2::Sha256::digest(s.as_bytes()).iter().map(|b| format!("{b:02x}")).collect()
            };
            let ha1 = sha("admin:shellyplus1-abc:pp");
            let ha2 = sha(&format!("GET:{}", f["uri"]));
            let want = sha(&format!("{ha1}:6f1d:{}:{}:auth:{ha2}", f["nc"], f["cnonce"]));
            if f["response"] == want && f.get("algorithm").map(String::as_str) == Some("SHA-256") {
                ("200 OK".into(), r#"{"was_on":true}"#.into())
            } else {
                ("401 Unauthorized".into(), String::new())
            }
        });
        let p = PowerSwitch { kind: "shelly2".into(), host: format!("http://127.0.0.1:{port}/"), password: "pp".into(), off_seconds: Some(5), ..Default::default() };
        power_cycle(&p, &|_| {}).unwrap();
        let log = log.lock().unwrap();
        let signed: Vec<&str> = log.iter().filter(|r| r.header("Authorization").is_some()).map(|r| r.target.as_str()).collect();
        assert_eq!(signed, vec!["/rpc/Switch.Set?id=0&on=false&toggle_after=5", "/rpc/Switch.Set?id=0&on=true"]);
    }

    #[test]
    fn generic_urls_post_with_basic_and_retry_on() {
        // The first "on" fails; the next one works.
        let (port, log) = mock(|req, i| {
            if req.target == "/on" && i == 1 {
                ("503 Busy".into(), String::new())
            } else {
                ("200 OK".into(), "done".into())
            }
        });
        let p = PowerSwitch {
            kind: "urls".into(),
            off_url: format!("http://127.0.0.1:{port}/off"),
            on_url: format!("http://127.0.0.1:{port}/on"),
            method: Some("POST".into()),
            username: "sw".into(),
            password: "pw".into(),
            ..Default::default()
        };
        let waited = Mutex::new(Vec::new());
        power_cycle(&p, &|d| waited.lock().unwrap().push(d.as_secs())).unwrap();
        let log = log.lock().unwrap();
        assert_eq!(log.iter().map(|r| (r.method.as_str(), r.target.as_str())).collect::<Vec<_>>(), vec![("POST", "/off"), ("POST", "/on"), ("POST", "/on")]);
        assert!(log.iter().all(|r| r.header("Authorization") == Some(basic("sw", "pw").as_str())));
        assert_eq!(*waited.lock().unwrap(), vec![10, 3]);
    }

    #[test]
    fn power_stays_off_is_reported() {
        let (port, _log) = mock(|req, _| if req.target.contains("turn=on") { ("500 Broken".into(), String::new()) } else { ("200 OK".into(), "{}".into()) });
        let p = PowerSwitch { kind: "shelly".into(), host: format!("127.0.0.1:{port}"), ..Default::default() };
        let e = power_cycle(&p, &|_| {}).unwrap_err();
        assert!(format!("{e:#}").contains("back ON failed"), "{e:#}");
    }

    fn cam(url: &str) -> CameraConfig {
        let mut c: CameraConfig = serde_json::from_value(serde_json::json!({ "name": "c", "url": url })).unwrap();
        c.username = "u".into();
        c
    }

    #[test]
    fn picks_the_method_by_what_the_camera_is() {
        assert_eq!(pick_method(&cam("rtsp://10.0.0.2:554/h264Preview_01_main")), Method::Reolink);
        assert_eq!(pick_method(&cam("rtsp://10.0.0.2/Preview_01_sub")), Method::Reolink);
        assert_eq!(pick_method(&cam("https://10.0.0.2/flv?port=1935&app=bcs&stream=channel0_main.bcs")), Method::Reolink);
        assert_eq!(pick_method(&cam("rtsp://10.0.0.3/cam/realmonitor?channel=1&subtype=0")), Method::Dahua);
        assert_eq!(pick_method(&cam("rtsp://10.0.0.4/Streaming/Channels/101")), Method::Hikvision);
        assert_eq!(pick_method(&cam("rtsp://10.0.0.5/stream1")), Method::Onvif);
        // A camera behind a Reolink hub: not the whole hub.
        let mut hub = cam("rtsp://10.0.0.6/Preview_02_main");
        hub.reolink_channel = Some(1);
        assert_eq!(pick_method(&hub), Method::None);
        // Chosen by hand wins.
        hub.watchdog = Some(crate::config::CameraWatchdog { reboot: Some("reolink".into()), ..Default::default() });
        assert_eq!(pick_method(&hub), Method::Reolink);
        let mut c = cam("rtsp://10.0.0.5/stream1");
        c.watchdog = Some(crate::config::CameraWatchdog { reboot: Some("none".into()), ..Default::default() });
        assert_eq!(pick_method(&c), Method::None);
        // Not a network camera.
        assert_eq!(pick_method(&cam("push://abcdefabcdef12")), Method::None);
        assert_eq!(pick_method(&cam("screen://0?fps=10")), Method::None);
    }

    #[test]
    fn target_takes_credentials_and_ports() {
        let mut c = cam("rtsp://10.0.0.5:554/stream1");
        c.password = "p".into();
        c.onvif_port = Some(8000);
        c.watchdog = Some(crate::config::CameraWatchdog { http_port: Some(8080), ..Default::default() });
        let t = target(&c).unwrap();
        assert_eq!((t.host.as_str(), t.user.as_str(), t.pass.as_str(), t.http_port, t.onvif_port), ("10.0.0.5", "u", "p", Some(8080), Some(8000)));
        assert_eq!(host_port("fe80::1", Some(80)), "[fe80::1]:80");
    }

    #[test]
    fn answers_checks_the_ports() {
        let l = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = l.local_addr().unwrap().port();
        assert!(answers(&cam(&format!("rtsp://127.0.0.1:{port}/x"))));
        drop(l);
        // Nothing listens on the closed port; port 80 is not expected on the test machine either.
        let closed = TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port();
        let mut c = cam(&format!("rtsp://127.0.0.1:{closed}/x"));
        c.watchdog = Some(crate::config::CameraWatchdog { http_port: Some(closed), ..Default::default() });
        if !tcp_answers("127.0.0.1", 80) {
            assert!(!answers(&c));
        }
    }
}
