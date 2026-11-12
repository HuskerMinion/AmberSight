//! ONVIF Events client (PullPoint pattern).
//!
//! Many IP cameras run their own motion / person detection and publish the
//! result as ONVIF events. This module subscribes to a camera's event service
//! with `CreatePullPointSubscription`, long-polls it with `PullMessages`,
//! keeps the lease alive with `Renew`, and reports each notification as a
//! small [`OnvifEvent`] through a callback. Everything runs on one dedicated
//! blocking thread (see [`OnvifEventSource::spawn`]).
//!
//! SOAP is written by hand and sent over plain HTTP/1.1 on a `TcpStream`;
//! responses are parsed with `roxmltree`, matching on local element names
//! because namespace prefixes differ between vendors. Requests carry a
//! WS-Security UsernameToken (PasswordDigest) built from the device's own
//! clock, and fall back to HTTP Digest authentication if the device answers
//! 401 with a Digest challenge.

use std::io::{Read, Write};
use std::net::{TcpStream, ToSocketAddrs};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{anyhow, bail, Context, Result};
use base64::Engine as _;
use chrono::{TimeZone, Utc};
use md5::Md5;
use sha1::{Digest, Sha1};
use tracing::{debug, info, warn};

/// What a camera event means for an NVR.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum EventKind {
    Motion,
    Person,
    Vehicle,
    Animal,
    /// A visitor / doorbell press.
    Visitor,
    /// Anything else; carries the raw topic.
    Other(String),
}

impl EventKind {
    pub fn label(&self) -> &str {
        match self {
            EventKind::Motion => "motion",
            EventKind::Person => "person",
            EventKind::Vehicle => "vehicle",
            EventKind::Animal => "animal",
            EventKind::Visitor => "visitor",
            EventKind::Other(_) => "other",
        }
    }
}

/// One parsed ONVIF notification.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OnvifEvent {
    pub kind: EventKind,
    /// True while the condition holds (motion started / person present).
    pub active: bool,
    /// Raw topic, e.g. `tns1:RuleEngine/CellMotionDetector/Motion`.
    pub topic: String,
    /// Device-reported UTC time (`UtcTime` attribute), if present.
    pub utc_time: Option<String>,
    /// True when this is the initial-state message sent after subscribing
    /// (`PropertyOperation="Initialized"`) rather than a change.
    pub initial: bool,
    /// Value of the first Source item (rule / video source token), may be empty.
    pub source: String,
}

// ---------------------------------------------------------------------------
// Public entry points
// ---------------------------------------------------------------------------

pub struct OnvifEventSource;

impl OnvifEventSource {
    /// Run the subscribe / pull / renew / reconnect loop on its own thread.
    /// The thread unsubscribes and exits shortly after `stop` becomes true.
    pub fn spawn(
        host: String,
        port: u16,
        username: String,
        password: String,
        on_event: impl Fn(OnvifEvent) + Send + 'static,
        stop: Arc<AtomicBool>,
    ) -> std::thread::JoinHandle<()> {
        std::thread::Builder::new()
            .name(format!("onvif-{host}"))
            .spawn(move || {
                let creds = Credentials { host, port, username, password };
                run_loop(&creds, &on_event, &stop);
            })
            .expect("spawn onvif thread")
    }
}

/// Host part of an `rtsp://[user:pass@]host[:port]/path` URL.
/// Added to a 401's error when it came with a SOAP fault: a web page behind a
/// password does not answer like that, an ONVIF service does.
pub(crate) const SOAP_401: &str = " [SOAP fault]";

pub fn host_from_rtsp_url(url: &str) -> Option<String> {
    let u = url::Url::parse(url).ok()?;
    u.host_str().map(|h| h.trim_matches(|c| c == '[' || c == ']').to_string())
}

/// `(username, password)` embedded in a URL, percent-decoded.
pub fn credentials_from_url(url: &str) -> Option<(String, String)> {
    let u = url::Url::parse(url).ok()?;
    if u.username().is_empty() {
        return None;
    }
    let dec = |s: &str| percent_encoding::percent_decode_str(s).decode_utf8_lossy().into_owned();
    Some((dec(u.username()), dec(u.password().unwrap_or(""))))
}

// ---------------------------------------------------------------------------
// Pure helpers: auth, envelopes, parsing
// ---------------------------------------------------------------------------

const NS_WSSE: &str = "http://docs.oasis-open.org/wss/2004/01/oasis-200401-wss-wssecurity-secext-1.0.xsd";
const NS_WSU: &str = "http://docs.oasis-open.org/wss/2004/01/oasis-200401-wss-wssecurity-utility-1.0.xsd";
const PW_DIGEST_TYPE: &str = "http://docs.oasis-open.org/wss/2004/01/oasis-200401-wss-username-token-profile-1.0#PasswordDigest";
const NONCE_ENC: &str = "http://docs.oasis-open.org/wss/2004/01/oasis-200401-wss-soap-message-security-1.0#Base64Binary";

const SUBSCRIPTION_TTL: Duration = Duration::from_secs(60);
const PULL_TIMEOUT_SECS: u64 = 5;

/// `Base64(SHA1(nonce + created + password))` over the raw nonce bytes.
pub fn password_digest(nonce: &[u8], created: &str, password: &str) -> String {
    let mut h = Sha1::new();
    h.update(nonce);
    h.update(created.as_bytes());
    h.update(password.as_bytes());
    base64::engine::general_purpose::STANDARD.encode(h.finalize())
}

pub(crate) fn xml_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&apos;"),
            c => out.push(c),
        }
    }
    out
}

/// The `<wsse:Security>` header block.
fn security_header(user: &str, password: &str, nonce: &[u8], created: &str) -> String {
    let digest = password_digest(nonce, created, password);
    let nonce_b64 = base64::engine::general_purpose::STANDARD.encode(nonce);
    format!(
        "<wsse:Security s:mustUnderstand=\"1\" xmlns:wsse=\"{NS_WSSE}\" xmlns:wsu=\"{NS_WSU}\">\
<wsse:UsernameToken><wsse:Username>{}</wsse:Username>\
<wsse:Password Type=\"{PW_DIGEST_TYPE}\">{digest}</wsse:Password>\
<wsse:Nonce EncodingType=\"{NONCE_ENC}\">{nonce_b64}</wsse:Nonce>\
<wsu:Created>{created}</wsu:Created></wsse:UsernameToken></wsse:Security>",
        xml_escape(user)
    )
}

/// A complete SOAP 1.2 envelope. `header` is raw header-block XML (may be empty).
fn envelope(header: &str, body: &str) -> String {
    format!(
        "<?xml version=\"1.0\" encoding=\"utf-8\"?>\
<s:Envelope xmlns:s=\"http://www.w3.org/2003/05/soap-envelope\" \
xmlns:tds=\"http://www.onvif.org/ver10/device/wsdl\" \
xmlns:tev=\"http://www.onvif.org/ver10/events/wsdl\" \
xmlns:wsnt=\"http://docs.oasis-open.org/wsn/b-2\" \
xmlns:wsa=\"http://www.w3.org/2005/08/addressing\">\
<s:Header>{header}</s:Header><s:Body>{body}</s:Body></s:Envelope>"
    )
}

fn addressing_header(action: &str, to: &str) -> String {
    format!(
        "<wsa:Action>{}</wsa:Action><wsa:To>{}</wsa:To>",
        xml_escape(action),
        xml_escape(to)
    )
}

const ACT_CREATE: &str = "http://www.onvif.org/ver10/events/wsdl/EventPortType/CreatePullPointSubscriptionRequest";
const ACT_PULL: &str = "http://www.onvif.org/ver10/events/wsdl/PullPointSubscription/PullMessagesRequest";
const ACT_RENEW: &str = "http://docs.oasis-open.org/wsn/bw-2/SubscriptionManager/RenewRequest";
const ACT_UNSUB: &str = "http://docs.oasis-open.org/wsn/bw-2/SubscriptionManager/UnsubscribeRequest";

fn body_get_system_date_and_time() -> &'static str {
    "<tds:GetSystemDateAndTime/>"
}

fn body_get_capabilities() -> &'static str {
    "<tds:GetCapabilities><tds:Category>Events</tds:Category></tds:GetCapabilities>"
}

fn body_get_services() -> &'static str {
    "<tds:GetServices><tds:IncludeCapability>false</tds:IncludeCapability></tds:GetServices>"
}

fn body_create_subscription(ttl_secs: u64) -> String {
    format!(
        "<tev:CreatePullPointSubscription><tev:InitialTerminationTime>PT{ttl_secs}S</tev:InitialTerminationTime>\
</tev:CreatePullPointSubscription>"
    )
}

fn body_pull_messages(timeout_secs: u64, limit: u32) -> String {
    format!("<tev:PullMessages><tev:Timeout>PT{timeout_secs}S</tev:Timeout><tev:MessageLimit>{limit}</tev:MessageLimit></tev:PullMessages>")
}

fn body_renew(ttl_secs: u64) -> String {
    format!("<wsnt:Renew><wsnt:TerminationTime>PT{ttl_secs}S</wsnt:TerminationTime></wsnt:Renew>")
}

fn body_unsubscribe() -> &'static str {
    "<wsnt:Unsubscribe/>"
}

/// Renew once half of the lease has elapsed.
fn needs_renew(elapsed: Duration, ttl: Duration) -> bool {
    elapsed >= ttl / 2
}

pub(crate) fn local_children<'a, 'i>(
    n: roxmltree::Node<'a, 'i>,
    name: &'static str,
) -> impl Iterator<Item = roxmltree::Node<'a, 'i>> {
    n.children().filter(move |c| c.is_element() && c.tag_name().name() == name)
}

pub(crate) fn find_desc<'a, 'i>(doc: &'a roxmltree::Document<'i>, name: &str) -> Option<roxmltree::Node<'a, 'i>> {
    doc.descendants().find(|n| n.is_element() && n.tag_name().name() == name)
}

pub(crate) fn text_of(n: roxmltree::Node) -> String {
    n.text().unwrap_or("").trim().to_string()
}

pub(crate) fn find_text(n: roxmltree::Node, name: &str) -> Option<String> {
    n.descendants()
        .find(|c| c.is_element() && c.tag_name().name() == name)
        .map(text_of)
}

/// If the SOAP body is a Fault, describe it.
fn soap_fault(doc: &roxmltree::Document) -> Option<String> {
    let f = find_desc(doc, "Fault")?;
    let reason = f
        .descendants()
        .find(|n| n.is_element() && matches!(n.tag_name().name(), "Text" | "faultstring"))
        .map(text_of)
        .unwrap_or_default();
    let sub = f
        .descendants()
        .filter(|n| n.is_element() && n.tag_name().name() == "Subcode")
        .last()
        .and_then(|s| find_text(s, "Value"))
        .unwrap_or_default();
    Some(format!("SOAP fault: {reason} {sub}").trim().to_string())
}

/// Device UTC time from a GetSystemDateAndTimeResponse.
pub(crate) fn parse_system_time(xml: &str) -> Option<chrono::DateTime<Utc>> {
    let doc = roxmltree::Document::parse(xml).ok()?;
    let utc = find_desc(&doc, "UTCDateTime")?;
    let num = |parent: &str, field: &str| -> Option<u32> {
        let p = utc.descendants().find(|n| n.is_element() && n.tag_name().name() == parent)?;
        find_text(p, field)?.parse().ok()
    };
    let year = num("Date", "Year")? as i32;
    Utc.with_ymd_and_hms(
        year,
        num("Date", "Month")?,
        num("Date", "Day")?,
        num("Time", "Hour")?,
        num("Time", "Minute")?,
        num("Time", "Second")?,
    )
    .single()
}

/// Events service address from a GetCapabilities or GetServices response.
fn parse_events_xaddr(xml: &str) -> Option<String> {
    let doc = roxmltree::Document::parse(xml).ok()?;
    // GetCapabilities: <Capabilities><Events><XAddr>
    if let Some(ev) = doc.descendants().find(|n| n.is_element() && n.tag_name().name() == "Events") {
        if let Some(x) = local_children(ev, "XAddr").next() {
            return Some(text_of(x));
        }
    }
    // GetServices: <Service><Namespace>.../events/wsdl</Namespace><XAddr>
    for svc in doc.descendants().filter(|n| n.is_element() && n.tag_name().name() == "Service") {
        let ns = local_children(svc, "Namespace").next().map(text_of).unwrap_or_default();
        if ns.ends_with("/events/wsdl") {
            if let Some(x) = local_children(svc, "XAddr").next() {
                return Some(text_of(x));
            }
        }
    }
    None
}

/// Subscription manager address from a CreatePullPointSubscriptionResponse.
fn parse_subscription_address(xml: &str) -> Option<String> {
    let doc = roxmltree::Document::parse(xml).ok()?;
    let r = find_desc(&doc, "SubscriptionReference")?;
    find_text(r, "Address").filter(|a| !a.is_empty())
}

fn is_true(v: &str) -> bool {
    matches!(v.trim().to_ascii_lowercase().as_str(), "true" | "1" | "active" | "on" | "yes")
}

/// Classify a topic (and, failing that, the data item name) into an [`EventKind`].
fn classify(topic: &str, data_name: &str) -> EventKind {
    let t = topic.to_ascii_lowercase();
    let d = data_name.to_ascii_lowercase();
    let has = |needles: &[&str]| needles.iter().any(|n| t.contains(n));
    let dhas = |needles: &[&str]| needles.iter().any(|n| d.contains(n));
    if has(&["visitor", "doorbell"]) {
        EventKind::Visitor
    } else if has(&["people", "person", "human", "face"]) {
        EventKind::Person
    } else if has(&["vehicle", "plate"]) {
        EventKind::Vehicle
    } else if has(&["pet", "animal", "dogcat", "dog_cat"]) {
        EventKind::Animal
    } else if has(&["motion"]) {
        EventKind::Motion
    } else if dhas(&["ispeople", "isperson", "ishuman"]) {
        EventKind::Person
    } else if dhas(&["isvehicle"]) {
        EventKind::Vehicle
    } else if dhas(&["ispet", "isdogcat", "isanimal"]) {
        EventKind::Animal
    } else if dhas(&["ismotion"]) {
        EventKind::Motion
    } else {
        EventKind::Other(topic.to_string())
    }
}

/// Data item names that carry the on/off state, in preference order.
const STATE_NAMES: &[&str] = &[
    "IsMotion", "State", "IsPeople", "IsPerson", "IsVehicle", "IsPet", "IsDogCat", "IsVisitor",
    "IsTamper", "Value",
];

/// Parse every `NotificationMessage` in a PullMessagesResponse (or Notify).
pub fn parse_notifications(xml: &str) -> Result<Vec<OnvifEvent>> {
    let doc = roxmltree::Document::parse(xml).context("invalid XML")?;
    let mut out = Vec::new();
    for nm in doc.descendants().filter(|n| n.is_element() && n.tag_name().name() == "NotificationMessage") {
        let topic = local_children(nm, "Topic").next().map(text_of).unwrap_or_default();
        if topic.is_empty() {
            continue;
        }
        let items = |container: &str| -> Vec<(String, String)> {
            nm.descendants()
                .filter(|n| n.is_element() && n.tag_name().name() == container)
                .flat_map(|c| c.children().filter(|s| s.is_element() && s.tag_name().name() == "SimpleItem"))
                .map(|s| {
                    (
                        s.attribute("Name").unwrap_or("").to_string(),
                        s.attribute("Value").unwrap_or("").to_string(),
                    )
                })
                .collect()
        };
        let data = items("Data");
        let Some((dname, dvalue)) = STATE_NAMES
            .iter()
            .find_map(|want| data.iter().find(|(n, _)| n == want))
            .or_else(|| data.first())
        else {
            debug!(topic, "notification without data items; skipped");
            continue;
        };
        let msg = nm.descendants().find(|n| n.is_element() && n.attribute("UtcTime").is_some());
        out.push(OnvifEvent {
            kind: classify(&topic, dname),
            active: is_true(dvalue),
            utc_time: msg.and_then(|m| m.attribute("UtcTime")).map(str::to_string),
            initial: msg.and_then(|m| m.attribute("PropertyOperation")) == Some("Initialized"),
            source: items("Source").into_iter().next().map(|(_, v)| v).unwrap_or_default(),
            topic,
        });
    }
    Ok(out)
}

// ---------------------------------------------------------------------------
// HTTP transport
// ---------------------------------------------------------------------------

#[derive(Clone)]
pub(crate) struct Credentials {
    pub(crate) host: String,
    pub(crate) port: u16,
    pub(crate) username: String,
    pub(crate) password: String,
}

struct HttpResponse {
    status: u16,
    www_authenticate: Vec<String>,
    body: String,
}

fn find_subslice(h: &[u8], n: &[u8]) -> Option<usize> {
    h.windows(n.len()).position(|w| w == n)
}

/// Decode a chunked body; `None` while incomplete.
fn decode_chunked(mut b: &[u8]) -> Option<Vec<u8>> {
    let mut out = Vec::new();
    loop {
        let eol = find_subslice(b, b"\r\n")?;
        let size = usize::from_str_radix(std::str::from_utf8(&b[..eol]).ok()?.split(';').next()?.trim(), 16).ok()?;
        b = &b[eol + 2..];
        if size == 0 {
            return Some(out);
        }
        if b.len() < size + 2 {
            return None;
        }
        out.extend_from_slice(&b[..size]);
        b = &b[size + 2..];
    }
}

/// Try to parse a full HTTP response out of `buf`; `None` if more data is needed.
fn try_parse_response(buf: &[u8], eof: bool) -> Option<HttpResponse> {
    let split = find_subslice(buf, b"\r\n\r\n")?;
    let head = String::from_utf8_lossy(&buf[..split]).into_owned();
    let rest = &buf[split + 4..];
    let mut lines = head.lines();
    let status = lines.next()?.split_whitespace().nth(1)?.parse().ok()?;
    let mut www = Vec::new();
    let (mut clen, mut chunked) = (None, false);
    for l in lines {
        let Some((k, v)) = l.split_once(':') else { continue };
        let (k, v) = (k.trim().to_ascii_lowercase(), v.trim());
        match k.as_str() {
            "content-length" => clen = v.parse::<usize>().ok(),
            "transfer-encoding" => chunked = v.to_ascii_lowercase().contains("chunked"),
            "www-authenticate" => www.push(v.to_string()),
            _ => {}
        }
    }
    let body = if chunked {
        match decode_chunked(rest) {
            Some(b) => b,
            None if eof => rest.to_vec(),
            None => return None,
        }
    } else if let Some(n) = clen {
        if rest.len() < n && !eof {
            return None;
        }
        rest[..n.min(rest.len())].to_vec()
    } else if eof {
        rest.to_vec()
    } else {
        return None;
    };
    Some(HttpResponse { status, www_authenticate: www, body: String::from_utf8_lossy(&body).into_owned() })
}

/// POST `body` to `url` and read the reply, giving up after `timeout` or when
/// `stop` is set.
fn http_post(
    url: &url::Url,
    action: &str,
    extra_headers: &str,
    body: &str,
    timeout: Duration,
    stop: &AtomicBool,
) -> Result<HttpResponse> {
    let host = url.host_str().ok_or_else(|| anyhow!("url has no host"))?;
    let port = url.port_or_known_default().unwrap_or(80);
    let addr = (host, port).to_socket_addrs()?.next().ok_or_else(|| anyhow!("cannot resolve {host}"))?;
    let mut s = TcpStream::connect_timeout(&addr, Duration::from_secs(5)).with_context(|| format!("connect {addr}"))?;
    s.set_nodelay(true).ok();
    s.set_write_timeout(Some(Duration::from_secs(5)))?;
    s.set_read_timeout(Some(Duration::from_millis(250)))?;
    let mut path = url.path().to_string();
    if let Some(q) = url.query() {
        path.push('?');
        path.push_str(q);
    }
    let ct = if action.is_empty() {
        "application/soap+xml; charset=utf-8".to_string()
    } else {
        format!("application/soap+xml; charset=utf-8; action=\"{action}\"")
    };
    let req = format!(
        "POST {path} HTTP/1.1\r\nHost: {host}:{port}\r\nContent-Type: {ct}\r\nContent-Length: {}\r\n{extra_headers}Connection: close\r\n\r\n{body}",
        body.len()
    );
    s.write_all(req.as_bytes())?;
    let deadline = Instant::now() + timeout;
    let mut buf = Vec::new();
    let mut chunk = [0u8; 8192];
    loop {
        if stop.load(Ordering::Relaxed) {
            bail!("stopped");
        }
        if Instant::now() > deadline {
            bail!("timed out waiting for {host}");
        }
        match s.read(&mut chunk) {
            Ok(0) => {
                return try_parse_response(&buf, true).ok_or_else(|| anyhow!("truncated HTTP response"));
            }
            Ok(n) => {
                buf.extend_from_slice(&chunk[..n]);
                // ONVIF answers are a few KB; a device sending megabytes is broken or hostile.
                if buf.len() > 4 << 20 {
                    bail!("{host} sent an oversized answer");
                }
                if let Some(r) = try_parse_response(&buf, false) {
                    return Ok(r);
                }
            }
            Err(e) if matches!(e.kind(), std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut) => {}
            Err(e) => return Err(e.into()),
        }
    }
}

// --- HTTP Digest (RFC 2617, MD5, qop=auth) ---

#[derive(Clone, Debug, PartialEq)]
struct DigestChallenge {
    realm: String,
    nonce: String,
    opaque: Option<String>,
    qop_auth: bool,
}

fn parse_digest_challenge(header: &str) -> Option<DigestChallenge> {
    let h = header.trim();
    let rest = h.get(..6).filter(|p| p.eq_ignore_ascii_case("digest"))?;
    let mut s = h[rest.len()..].trim();
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
    Some(DigestChallenge {
        realm: map.get("realm")?.clone(),
        nonce: map.get("nonce")?.clone(),
        opaque: map.get("opaque").cloned(),
        qop_auth: map.get("qop").map(|q| q.split(',').any(|x| x.trim() == "auth")).unwrap_or(false),
    })
}

fn md5_hex(s: &str) -> String {
    use md5::Digest as _;
    let out = Md5::digest(s.as_bytes());
    out.iter().map(|b| format!("{b:02x}")).collect()
}

fn digest_authorization(
    ch: &DigestChallenge,
    user: &str,
    pass: &str,
    method: &str,
    uri: &str,
    nc: u32,
    cnonce: &str,
) -> String {
    let ha1 = md5_hex(&format!("{user}:{}:{pass}", ch.realm));
    let ha2 = md5_hex(&format!("{method}:{uri}"));
    let mut h = format!(
        "Digest username=\"{user}\", realm=\"{}\", nonce=\"{}\", uri=\"{uri}\"",
        ch.realm, ch.nonce
    );
    if ch.qop_auth {
        let resp = md5_hex(&format!("{ha1}:{}:{nc:08x}:{cnonce}:auth:{ha2}", ch.nonce));
        h += &format!(", qop=auth, nc={nc:08x}, cnonce=\"{cnonce}\", response=\"{resp}\"");
    } else {
        h += &format!(", response=\"{}\"", md5_hex(&format!("{ha1}:{}:{ha2}", ch.nonce)));
    }
    if let Some(o) = &ch.opaque {
        h += &format!(", opaque=\"{o}\"");
    }
    h
}

/// Unpredictable-enough bytes for nonces (not key material).
pub(crate) fn random_bytes<const N: usize>() -> [u8; N] {
    use std::hash::{BuildHasher, Hasher};
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let mut out = [0u8; N];
    for chunk in out.chunks_mut(8) {
        let mut h = std::collections::hash_map::RandomState::new().build_hasher();
        h.write_u64(COUNTER.fetch_add(1, Ordering::Relaxed));
        h.write_u128(
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0),
        );
        let v = h.finish().to_le_bytes();
        chunk.copy_from_slice(&v[..chunk.len()]);
    }
    out
}

// ---------------------------------------------------------------------------
// SOAP client
// ---------------------------------------------------------------------------

pub(crate) struct Client<'a> {
    creds: &'a Credentials,
    stop: &'a AtomicBool,
    /// Device clock minus local clock.
    pub(crate) clock_offset: chrono::Duration,
    digest: Option<DigestChallenge>,
    nc: u32,
}

impl<'a> Client<'a> {
    pub(crate) fn new(creds: &'a Credentials, stop: &'a AtomicBool) -> Self {
        Client { creds, stop, clock_offset: chrono::Duration::zero(), digest: None, nc: 0 }
    }

    fn device_url(&self) -> String {
        format!("http://{}:{}/onvif/device_service", self.creds.host, self.creds.port)
    }

    fn created_now(&self) -> String {
        (Utc::now() + self.clock_offset).format("%Y-%m-%dT%H:%M:%SZ").to_string()
    }

    fn http_auth_header(&mut self, url: &url::Url) -> String {
        let Some(ch) = self.digest.clone() else { return String::new() };
        self.nc += 1;
        let mut uri = url.path().to_string();
        if let Some(q) = url.query() {
            uri.push('?');
            uri.push_str(q);
        }
        let cnonce: String = random_bytes::<8>().iter().map(|b| format!("{b:02x}")).collect();
        let a = digest_authorization(&ch, &self.creds.username, &self.creds.password, "POST", &uri, self.nc, &cnonce);
        format!("Authorization: {a}\r\n")
    }

    /// One SOAP call; returns the response body XML. `secure` adds WS-Security.
    pub(crate) fn call(&mut self, url_str: &str, action: &str, body: &str, secure: bool, timeout: Duration) -> Result<String> {
        let url = url::Url::parse(url_str).with_context(|| format!("bad url {url_str}"))?;
        let mut attempts = 0;
        loop {
            attempts += 1;
            let mut header = String::new();
            if !action.is_empty() {
                header.push_str(&addressing_header(action, url_str));
            }
            if secure {
                let nonce = random_bytes::<16>();
                header.push_str(&security_header(
                    &self.creds.username,
                    &self.creds.password,
                    &nonce,
                    &self.created_now(),
                ));
            }
            let xml = envelope(&header, body);
            let extra = self.http_auth_header(&url);
            let started = Instant::now();
            let resp = http_post(&url, action, &extra, &xml, timeout, self.stop);
            debug!(url = url_str, ms = started.elapsed().as_millis() as u64, ok = resp.is_ok(), "soap call");
            let resp = resp?;
            if resp.status == 401 && attempts < 3 {
                if let Some(ch) = resp.www_authenticate.iter().find_map(|h| parse_digest_challenge(h)) {
                    debug!("device wants HTTP Digest auth");
                    self.digest = Some(ch);
                    self.nc = 0;
                    continue;
                }
            }
            if resp.status == 401 {
                let soap = roxmltree::Document::parse(&resp.body).is_ok_and(|doc| soap_fault(&doc).is_some());
                bail!("HTTP 401 unauthorized (check username/password){}", if soap { SOAP_401 } else { "" });
            }
            if let Ok(doc) = roxmltree::Document::parse(&resp.body) {
                if let Some(f) = soap_fault(&doc) {
                    bail!("{f} (HTTP {})", resp.status);
                }
            }
            if resp.status >= 400 {
                bail!("HTTP {}", resp.status);
            }
            return Ok(resp.body);
        }
    }

    pub(crate) fn sync_clock(&mut self) {
        let url = self.device_url();
        match self.call(&url, "", body_get_system_date_and_time(), false, Duration::from_secs(8)) {
            Ok(xml) => match parse_system_time(&xml) {
                Some(dev) => {
                    self.clock_offset = dev - Utc::now();
                    debug!(offset_secs = self.clock_offset.num_seconds(), "device clock offset");
                }
                None => warn!("could not parse device time; assuming clocks agree"),
            },
            Err(e) => warn!("GetSystemDateAndTime failed ({e:#}); assuming clocks agree"),
        }
    }

    fn events_url(&mut self) -> String {
        let dev = self.device_url();
        for (body, what) in [(body_get_capabilities(), "GetCapabilities"), (body_get_services(), "GetServices")] {
            match self.call(&dev, "", body, true, Duration::from_secs(8)) {
                Ok(xml) => {
                    if let Some(x) = parse_events_xaddr(&xml) {
                        return self.fix_host(&x);
                    }
                    debug!("{what}: no events XAddr");
                }
                Err(e) => debug!("{what} failed: {e:#}"),
            }
        }
        let fallback = format!("http://{}:{}/onvif/event_service", self.creds.host, self.creds.port);
        warn!("events service not advertised; trying {fallback}");
        fallback
    }

    /// Devices sometimes advertise 0.0.0.0 / localhost / an internal address;
    /// always talk to the host we were configured with, keeping the device's port.
    pub(crate) fn fix_host(&self, addr: &str) -> String {
        match url::Url::parse(addr) {
            Ok(mut u) => {
                if u.host_str() != Some(self.creds.host.as_str()) {
                    let _ = u.set_host(Some(&self.creds.host));
                }
                u.to_string()
            }
            Err(_) => addr.to_string(),
        }
    }

    fn subscribe(&mut self, events_url: &str) -> Result<String> {
        let body = body_create_subscription(SUBSCRIPTION_TTL.as_secs());
        let xml = self.call(events_url, ACT_CREATE, &body, true, Duration::from_secs(10))?;
        let addr = parse_subscription_address(&xml).ok_or_else(|| anyhow!("no subscription address in response"))?;
        Ok(self.fix_host(&addr))
    }

    fn pull(&mut self, sub: &str) -> Result<Vec<OnvifEvent>> {
        let xml = self.call(sub, ACT_PULL, &body_pull_messages(PULL_TIMEOUT_SECS, 10), true, Duration::from_secs(35))?;
        parse_notifications(&xml)
    }

    fn renew(&mut self, sub: &str) -> Result<()> {
        let body = body_renew(SUBSCRIPTION_TTL.as_secs());
        self.call(sub, ACT_RENEW, &body, true, Duration::from_secs(8)).map(|_| ())
    }

    fn unsubscribe(&mut self, sub: &str) {
        // `stop` is already set at shutdown, so use a private flag here.
        let never = AtomicBool::new(false);
        let mut c = Client {
            creds: self.creds,
            stop: &never,
            clock_offset: self.clock_offset,
            digest: self.digest.clone(),
            nc: self.nc,
        };
        if let Err(e) = c.call(sub, ACT_UNSUB, body_unsubscribe(), true, Duration::from_secs(25)) {
            debug!("Unsubscribe failed: {e:#}");
        }
    }
}

// ---------------------------------------------------------------------------
// Session loop
// ---------------------------------------------------------------------------

fn sleep_unless_stopped(d: Duration, stop: &AtomicBool) {
    let end = Instant::now() + d;
    while Instant::now() < end && !stop.load(Ordering::Relaxed) {
        std::thread::sleep(Duration::from_millis(100));
    }
}

fn run_loop(creds: &Credentials, on_event: &dyn Fn(OnvifEvent), stop: &AtomicBool) {
    let mut backoff = Duration::from_secs(1);
    while !stop.load(Ordering::Relaxed) {
        let mut pulled_ok = false;
        match session(creds, on_event, stop, &mut pulled_ok) {
            Ok(()) => break,
            Err(e) if stop.load(Ordering::Relaxed) => {
                debug!("onvif session ended on stop: {e:#}");
                break;
            }
            Err(e) => {
                if pulled_ok {
                    backoff = Duration::from_secs(1);
                }
                warn!(host = %creds.host, "onvif events: {e:#}; retrying in {}s", backoff.as_secs());
                sleep_unless_stopped(backoff, stop);
                backoff = (backoff * 2).min(Duration::from_secs(30));
            }
        }
    }
}

fn session(
    creds: &Credentials,
    on_event: &dyn Fn(OnvifEvent),
    stop: &AtomicBool,
    pulled_ok: &mut bool,
) -> Result<()> {
    let mut c = Client::new(creds, stop);
    c.sync_clock();
    let events_url = c.events_url();
    let sub = c.subscribe(&events_url)?;
    info!(host = %creds.host, "onvif events subscribed");
    let mut leased = Instant::now();
    // Some cameras re-deliver the same state message on every pull; only pass
    // on a message that differs from the last one seen for its topic.
    let mut last: std::collections::HashMap<String, (Option<String>, bool)> = Default::default();
    let result = (|| -> Result<()> {
        while !stop.load(Ordering::Relaxed) {
            if needs_renew(leased.elapsed(), SUBSCRIPTION_TTL) {
                c.renew(&sub).context("renew")?;
                leased = Instant::now();
                debug!("subscription renewed");
            }
            let events = c.pull(&sub)?;
            *pulled_ok = true;
            for ev in events {
                let key = (ev.utc_time.clone(), ev.active);
                if last.get(&ev.topic) != Some(&key) {
                    last.insert(ev.topic.clone(), key);
                    on_event(ev);
                }
            }
        }
        Ok(())
    })();
    c.unsubscribe(&sub);
    result
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn password_digest_known_answer() {
        // Example from the WS-Security UsernameToken Profile 1.0 specification.
        let nonce = base64::engine::general_purpose::STANDARD.decode("LKqI6G/AikKCQrN0zqZFlg==").unwrap();
        assert_eq!(
            password_digest(&nonce, "2010-09-16T07:50:45Z", "userpassword"),
            "tuOSpGlFlIXsozq4HFNeeGeFLEI="
        );
    }

    #[test]
    fn security_header_contains_token() {
        let h = security_header("adm<in", "pw", b"0123456789abcdef", "2024-01-02T03:04:05Z");
        assert!(h.contains("<wsse:Username>adm&lt;in</wsse:Username>"));
        assert!(h.contains("PasswordDigest"));
        assert!(h.contains("<wsu:Created>2024-01-02T03:04:05Z</wsu:Created>"));
        assert!(roxmltree::Document::parse(&envelope(&h, "<tds:GetSystemDateAndTime/>")).is_ok());
    }

    #[test]
    fn envelopes_are_well_formed() {
        for b in [
            body_create_subscription(60),
            body_pull_messages(10, 10),
            body_renew(60),
            body_unsubscribe().to_string(),
            body_get_capabilities().to_string(),
            body_get_services().to_string(),
        ] {
            let x = envelope(&addressing_header(ACT_PULL, "http://h/x?a=1&b=2"), &b);
            roxmltree::Document::parse(&x).unwrap();
        }
        assert!(body_pull_messages(10, 10).contains("PT10S"));
    }

    #[test]
    fn renew_timing() {
        let ttl = Duration::from_secs(60);
        assert!(!needs_renew(Duration::from_secs(10), ttl));
        assert!(needs_renew(Duration::from_secs(30), ttl));
        assert!(needs_renew(Duration::from_secs(59), ttl));
    }

    const PREFIXED_STYLE: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<SOAP-ENV:Envelope xmlns:SOAP-ENV="http://www.w3.org/2003/05/soap-envelope" xmlns:tt="http://www.onvif.org/ver10/schema" xmlns:wsnt="http://docs.oasis-open.org/wsn/b-2" xmlns:tev="http://www.onvif.org/ver10/events/wsdl" xmlns:tns1="http://www.onvif.org/ver10/topics">
<SOAP-ENV:Body><tev:PullMessagesResponse>
<tev:CurrentTime>2024-05-01T12:00:01Z</tev:CurrentTime><tev:TerminationTime>2024-05-01T12:01:01Z</tev:TerminationTime>
<wsnt:NotificationMessage>
<wsnt:Topic Dialect="http://www.onvif.org/ver10/tev/topicExpression/ConcreteSet">tns1:RuleEngine/CellMotionDetector/Motion</wsnt:Topic>
<wsnt:Message><tt:Message UtcTime="2024-05-01T12:00:00Z" PropertyOperation="Initialized">
<tt:Source><tt:SimpleItem Name="VideoSourceConfigurationToken" Value="000"/><tt:SimpleItem Name="VideoAnalyticsConfigurationToken" Value="000"/><tt:SimpleItem Name="Rule" Value="MyMotionDetectorRule"/></tt:Source>
<tt:Data><tt:SimpleItem Name="IsMotion" Value="true"/></tt:Data></tt:Message></wsnt:Message>
</wsnt:NotificationMessage>
<wsnt:NotificationMessage>
<wsnt:Topic Dialect="http://www.onvif.org/ver10/tev/topicExpression/ConcreteSet">tns1:RuleEngine/MyRuleDetector/PeopleDetect</wsnt:Topic>
<wsnt:Message><tt:Message UtcTime="2024-05-01T12:00:00Z" PropertyOperation="Initialized">
<tt:Source><tt:SimpleItem Name="Source" Value="000"/></tt:Source>
<tt:Data><tt:SimpleItem Name="State" Value="false"/></tt:Data></tt:Message></wsnt:Message>
</wsnt:NotificationMessage>
<wsnt:NotificationMessage>
<wsnt:Topic Dialect="http://www.onvif.org/ver10/tev/topicExpression/ConcreteSet">tns1:RuleEngine/MyRuleDetector/Visitor</wsnt:Topic>
<wsnt:Message><tt:Message UtcTime="2024-05-01T12:00:05Z" PropertyOperation="Changed">
<tt:Source><tt:SimpleItem Name="Source" Value="000"/></tt:Source>
<tt:Data><tt:SimpleItem Name="State" Value="true"/></tt:Data></tt:Message></wsnt:Message>
</wsnt:NotificationMessage>
</tev:PullMessagesResponse></SOAP-ENV:Body></SOAP-ENV:Envelope>"#;

    const DEFAULT_NS_STYLE: &str = r#"<s:Envelope xmlns:s="http://www.w3.org/2003/05/soap-envelope"><s:Body>
<PullMessagesResponse xmlns="http://www.onvif.org/ver10/events/wsdl">
<CurrentTime>2024-05-01T12:00:01Z</CurrentTime>
<NotificationMessage xmlns="http://docs.oasis-open.org/wsn/b-2" xmlns:t="http://www.onvif.org/ver10/topics" xmlns:sch="http://www.onvif.org/ver10/schema">
<SubscriptionReference><Address>http://192.0.2.9/onvif/Subscription?Idx=1</Address></SubscriptionReference>
<Topic Dialect="http://www.onvif.org/ver10/tev/topicExpression/ConcreteSet">t:VideoSource/MotionAlarm</Topic>
<ProducerReference><Address>http://192.0.2.9/onvif/event</Address></ProducerReference>
<Message><sch:Message UtcTime="2024-05-01T12:00:03.250Z" PropertyOperation="Changed">
<sch:Source><sch:SimpleItem Name="Source" Value="VideoSource_1"/></sch:Source>
<sch:Data><sch:SimpleItem Name="State" Value="1"/></sch:Data></sch:Message></Message>
</NotificationMessage>
<NotificationMessage xmlns="http://docs.oasis-open.org/wsn/b-2" xmlns:t="http://www.onvif.org/ver10/topics" xmlns:sch="http://www.onvif.org/ver10/schema">
<Topic Dialect="x">t:RuleEngine/TpSmartEventDetector/IsVehicle</Topic>
<Message><sch:Message UtcTime="2024-05-01T12:00:04Z" PropertyOperation="Changed">
<sch:Source><sch:SimpleItem Name="VideoSourceToken" Value="1"/></sch:Source>
<sch:Data><sch:SimpleItem Name="IsVehicle" Value="false"/></sch:Data></sch:Message></Message>
</NotificationMessage>
<NotificationMessage xmlns="http://docs.oasis-open.org/wsn/b-2" xmlns:t="http://www.onvif.org/ver10/topics" xmlns:sch="http://www.onvif.org/ver10/schema">
<Topic Dialect="x">t:Device/Trigger/DigitalInput</Topic>
<Message><sch:Message UtcTime="2024-05-01T12:00:04Z"><sch:Data><sch:SimpleItem Name="LogicalState" Value="true"/></sch:Data></sch:Message></Message>
</NotificationMessage>
</PullMessagesResponse></s:Body></s:Envelope>"#;

    #[test]
    fn parses_prefixed_style() {
        let ev = parse_notifications(PREFIXED_STYLE).unwrap();
        assert_eq!(ev.len(), 3);
        assert_eq!(ev[0].kind, EventKind::Motion);
        assert!(ev[0].active && ev[0].initial);
        assert_eq!(ev[0].topic, "tns1:RuleEngine/CellMotionDetector/Motion");
        assert_eq!(ev[0].source, "000");
        assert_eq!(ev[0].utc_time.as_deref(), Some("2024-05-01T12:00:00Z"));
        assert_eq!(ev[1].kind, EventKind::Person);
        assert!(!ev[1].active);
        assert_eq!(ev[2].kind, EventKind::Visitor);
        assert!(ev[2].active && !ev[2].initial);
    }

    #[test]
    fn parses_default_namespace_style() {
        let ev = parse_notifications(DEFAULT_NS_STYLE).unwrap();
        assert_eq!(ev.len(), 3);
        assert_eq!(ev[0].kind, EventKind::Motion);
        assert!(ev[0].active);
        assert_eq!(ev[0].topic, "t:VideoSource/MotionAlarm");
        assert_eq!(ev[0].source, "VideoSource_1");
        assert_eq!(ev[1].kind, EventKind::Vehicle);
        assert!(!ev[1].active);
        assert_eq!(ev[2].kind, EventKind::Other("t:Device/Trigger/DigitalInput".into()));
        assert!(ev[2].active);
    }

    #[test]
    fn empty_pull_response() {
        let x = r#"<e:Envelope xmlns:e="http://www.w3.org/2003/05/soap-envelope"><e:Body><PullMessagesResponse/></e:Body></e:Envelope>"#;
        assert!(parse_notifications(x).unwrap().is_empty());
    }

    #[test]
    fn classify_topics() {
        assert_eq!(classify("tns1:RuleEngine/MyRuleDetector/DogCatDetect", "State"), EventKind::Animal);
        assert_eq!(classify("tns1:RuleEngine/MyRuleDetector/FaceDetect", "State"), EventKind::Person);
        assert_eq!(classify("tns1:Foo/Bar", "IsPeople"), EventKind::Person);
        assert_eq!(
            classify("tns1:RuleEngine/TamperDetector/Tamper", "IsTamper"),
            EventKind::Other("tns1:RuleEngine/TamperDetector/Tamper".into())
        );
    }

    #[test]
    fn parses_service_discovery() {
        let caps = r#"<env:Envelope xmlns:env="http://www.w3.org/2003/05/soap-envelope"><env:Body><tds:GetCapabilitiesResponse xmlns:tds="a" xmlns:tt="b"><tds:Capabilities>
<tt:Device><tt:XAddr>http://10.0.0.5:8000/onvif/device_service</tt:XAddr></tt:Device>
<tt:Events><tt:XAddr>http://10.0.0.5:8000/onvif/event_service</tt:XAddr></tt:Events></tds:Capabilities></tds:GetCapabilitiesResponse></env:Body></env:Envelope>"#;
        assert_eq!(parse_events_xaddr(caps).as_deref(), Some("http://10.0.0.5:8000/onvif/event_service"));
        let svcs = r#"<e:Envelope xmlns:e="x"><e:Body><GetServicesResponse>
<Service><Namespace>http://www.onvif.org/ver10/device/wsdl</Namespace><XAddr>http://h/dev</XAddr></Service>
<Service><Namespace>http://www.onvif.org/ver10/events/wsdl</Namespace><XAddr>http://h/ev</XAddr></Service></GetServicesResponse></e:Body></e:Envelope>"#;
        assert_eq!(parse_events_xaddr(svcs).as_deref(), Some("http://h/ev"));
    }

    #[test]
    fn parses_subscription_time_and_fault() {
        let sub = r#"<e:Envelope xmlns:e="x"><e:Body><tev:CreatePullPointSubscriptionResponse xmlns:tev="t" xmlns:wsnt="w" xmlns:wsa5="a"><tev:SubscriptionReference><wsa5:Address>http://10.0.0.5:8000/onvif/Subscription?Idx=0</wsa5:Address></tev:SubscriptionReference></tev:CreatePullPointSubscriptionResponse></e:Body></e:Envelope>"#;
        assert_eq!(
            parse_subscription_address(sub).as_deref(),
            Some("http://10.0.0.5:8000/onvif/Subscription?Idx=0")
        );
        let t = r#"<e:Envelope xmlns:e="x"><e:Body><tds:GetSystemDateAndTimeResponse xmlns:tds="a" xmlns:tt="b"><tds:SystemDateAndTime><tt:UTCDateTime><tt:Time><tt:Hour>7</tt:Hour><tt:Minute>5</tt:Minute><tt:Second>9</tt:Second></tt:Time><tt:Date><tt:Year>2024</tt:Year><tt:Month>5</tt:Month><tt:Day>1</tt:Day></tt:Date></tt:UTCDateTime></tds:SystemDateAndTime></tds:GetSystemDateAndTimeResponse></e:Body></e:Envelope>"#;
        assert_eq!(parse_system_time(t).unwrap().to_rfc3339(), "2024-05-01T07:05:09+00:00");
        let f = r#"<e:Envelope xmlns:e="x"><e:Body><e:Fault><e:Code><e:Value>e:Sender</e:Value><e:Subcode><e:Value>ter:NotAuthorized</e:Value></e:Subcode></e:Code><e:Reason><e:Text xml:lang="en">Sender not authorized</e:Text></e:Reason></e:Fault></e:Body></e:Envelope>"#;
        let doc = roxmltree::Document::parse(f).unwrap();
        assert!(soap_fault(&doc).unwrap().contains("Sender not authorized"));
    }

    #[test]
    fn http_response_parsing() {
        let raw = b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n5\r\nhello\r\n6\r\n world\r\n0\r\n\r\n";
        assert!(try_parse_response(&raw[..40], false).is_none());
        assert_eq!(try_parse_response(raw, false).unwrap().body, "hello world");
        let raw = b"HTTP/1.1 401 Unauthorized\r\nContent-Length: 3\r\nWWW-Authenticate: Digest realm=\"r\", nonce=\"n\"\r\n\r\nabc";
        let r = try_parse_response(raw, false).unwrap();
        assert_eq!((r.status, r.body.as_str()), (401, "abc"));
        assert_eq!(r.www_authenticate.len(), 1);
    }

    #[test]
    fn http_digest_known_answer() {
        // RFC 2617 section 3.5 example.
        let ch = parse_digest_challenge(
            r#"Digest realm="testrealm@host.com", qop="auth,auth-int", nonce="dcd98b7102dd2f0e8b11d0f600bfb0c093", opaque="5ccc069c403ebaf9f0171e9517f40e41""#,
        )
        .unwrap();
        assert!(ch.qop_auth);
        let a = digest_authorization(&ch, "Mufasa", "Circle Of Life", "GET", "/dir/index.html", 1, "0a4f113b");
        assert!(a.contains("response=\"6629fae49393a05397450978507c4ef1\""), "{a}");
    }

    #[test]
    fn rtsp_host_extraction() {
        assert_eq!(
            host_from_rtsp_url("rtsp://admin:p%40ss@192.168.1.5:554/h264Preview_01_main").as_deref(),
            Some("192.168.1.5")
        );
        assert_eq!(host_from_rtsp_url("rtsp://cam.local/stream").as_deref(), Some("cam.local"));
        assert_eq!(host_from_rtsp_url("not a url"), None);
        assert_eq!(
            credentials_from_url("rtsp://admin:p%40ss@h/x"),
            Some(("admin".into(), "p@ss".into()))
        );
    }
}
