//! ONVIF discovery, device inspection and PTZ control.
//!
//! * WS-Discovery: a multicast Probe finds ONVIF devices on the LAN
//!   ([`discover`]).
//! * Inspection: with credentials, a device is asked for its identity, its
//!   media profiles and the RTSP URI of every profile ([`inspect`]), from which
//!   a main and a sub stream are chosen ([`select_main_sub`]).
//! * PTZ: continuous / absolute moves, presets and optional preset tours
//!   against the camera's PTZ service.
//!
//! Everything here blocks; async callers wrap it in `spawn_blocking`. SOAP
//! transport, authentication and clock handling are shared with the events
//! client in [`crate::onvif`].

use std::collections::HashMap;
use std::net::{Ipv4Addr, SocketAddr, SocketAddrV4, UdpSocket};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use anyhow::{anyhow, bail, Context, Result};
use serde::Serialize;

use crate::config::CameraConfig;
use crate::onvif::{find_desc, find_text, local_children, text_of, xml_escape, Client, Credentials};

const NS_MEDIA: &str = "http://www.onvif.org/ver10/media/wsdl";
const NS_PTZ: &str = "http://www.onvif.org/ver20/ptz/wsdl";
const NS_SCHEMA: &str = "http://www.onvif.org/ver10/schema";
const CALL_TIMEOUT: Duration = Duration::from_secs(30);

// ---------------------------------------------------------------------------
// WS-Discovery
// ---------------------------------------------------------------------------

/// One device that answered a WS-Discovery probe.
#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct DiscoveredDevice {
    /// WS-Discovery endpoint reference (usually `urn:uuid:...`).
    pub endpoint: String,
    /// Device service addresses (`http://ip:port/onvif/device_service`).
    pub xaddrs: Vec<String>,
    pub name: String,
    pub hardware: String,
    pub location: String,
    pub scopes: Vec<String>,
    pub types: Vec<String>,
    /// Host of the preferred address.
    pub ip: String,
}

fn scope_value(scopes: &[String], key: &str) -> String {
    let marker = format!("/{key}/");
    scopes
        .iter()
        .find_map(|s| {
            let i = s.find(&marker)?;
            let raw = &s[i + marker.len()..];
            Some(clean_text(&percent_encoding::percent_decode_str(raw).decode_utf8_lossy()))
        })
        .unwrap_or_default()
}

/// Text a device sent about itself, made safe to show, log and send on:
/// control characters dropped, at most 64 characters.
fn clean_text(s: &str) -> String {
    s.chars().filter(|c| !c.is_control()).take(64).collect::<String>().trim().to_string()
}

/// Most devices one scan keeps (a flood of fake answers stops here).
const MAX_DEVICES: usize = 512;

/// A device's answer, trusted only for the address it came from: its service
/// addresses must be on that host (a device cannot point the server, and the
/// logins Discovery tries, at another machine), and its address is the
/// sender's. None when no service address is on the sender.
fn from_sender(mut d: DiscoveredDevice, sender: std::net::IpAddr) -> Option<DiscoveredDevice> {
    let on_sender = |x: &String| host_of(x).and_then(|h| h.trim_matches(['[', ']']).parse::<std::net::IpAddr>().ok()) == Some(sender);
    d.xaddrs.retain(on_sender);
    if d.xaddrs.is_empty() || sender.is_loopback() || sender.is_unspecified() {
        return None;
    }
    d.ip = sender.to_string();
    Some(d)
}

/// Add the answers in one received message (from `sender`) to `found`.
fn take_answer(found: &mut Vec<DiscoveredDevice>, msg: &[u8], sender: SocketAddr) {
    for d in parse_probe_matches(&String::from_utf8_lossy(msg)) {
        let Some(d) = from_sender(d, sender.ip()) else { continue };
        if found.len() < MAX_DEVICES || found.iter().any(|x| x.endpoint == d.endpoint && !d.endpoint.is_empty()) {
            merge_device(found, d);
        }
    }
}

fn host_of(x: &str) -> Option<String> {
    url::Url::parse(x).ok().and_then(|u| u.host_str().map(str::to_string))
}

/// Pick the address most likely to be reachable: IPv4, not link-local.
fn preferred_xaddr(xaddrs: &[String]) -> Option<&String> {
    let score = |x: &String| -> u8 {
        match host_of(x) {
            Some(h) => match h.parse::<Ipv4Addr>() {
                Ok(ip) if !ip.is_link_local() => 0,
                Ok(_) => 1,
                Err(_) => 2,
            },
            None => 3,
        }
    };
    xaddrs.iter().min_by_key(|x| score(x))
}

/// Devices from a WS-Discovery `ProbeMatches` message.
pub fn parse_probe_matches(xml: &str) -> Vec<DiscoveredDevice> {
    let Ok(doc) = roxmltree::Document::parse(xml) else { return Vec::new() };
    let words = |n: roxmltree::Node, name: &str| -> Vec<String> {
        find_text(n, name).map(|t| t.split_whitespace().map(str::to_string).collect()).unwrap_or_default()
    };
    let mut out = Vec::new();
    for m in doc.descendants().filter(|n| n.is_element() && n.tag_name().name() == "ProbeMatch") {
        let endpoint = m
            .descendants()
            .find(|n| n.is_element() && n.tag_name().name() == "EndpointReference")
            .and_then(|e| find_text(e, "Address"))
            .unwrap_or_default();
        let xaddrs = words(m, "XAddrs");
        let scopes = words(m, "Scopes");
        if xaddrs.is_empty() {
            continue;
        }
        let ip = preferred_xaddr(&xaddrs).and_then(|x| host_of(x)).unwrap_or_default();
        out.push(DiscoveredDevice {
            endpoint,
            name: scope_value(&scopes, "name"),
            hardware: scope_value(&scopes, "hardware"),
            location: scope_value(&scopes, "location"),
            types: words(m, "Types"),
            xaddrs,
            scopes,
            ip,
        });
    }
    out
}

/// Merge `new` into `list`, deduplicating by endpoint address (or by the
/// first address when a device sends no endpoint).
fn merge_device(list: &mut Vec<DiscoveredDevice>, new: DiscoveredDevice) {
    let same = |d: &DiscoveredDevice| {
        if !new.endpoint.is_empty() {
            d.endpoint == new.endpoint
        } else {
            d.endpoint.is_empty() && d.xaddrs.first() == new.xaddrs.first()
        }
    };
    match list.iter_mut().find(|d| same(d)) {
        Some(d) => {
            for x in new.xaddrs {
                if !d.xaddrs.contains(&x) {
                    d.xaddrs.push(x);
                }
            }
            if d.name.is_empty() {
                d.name = new.name;
            }
            if d.hardware.is_empty() {
                d.hardware = new.hardware;
            }
            if d.location.is_empty() {
                d.location = new.location;
            }
            if let Some(p) = preferred_xaddr(&d.xaddrs).and_then(|x| host_of(x)) {
                d.ip = p;
            }
        }
        None => list.push(new),
    }
}

fn probe_message(types: &str) -> String {
    let id: String = crate::onvif::random_bytes::<16>().iter().map(|b| format!("{b:02x}")).collect();
    let uuid = format!("{}-{}-{}-{}-{}", &id[0..8], &id[8..12], &id[12..16], &id[16..20], &id[20..32]);
    format!(
        "<?xml version=\"1.0\" encoding=\"utf-8\"?>\
<e:Envelope xmlns:e=\"http://www.w3.org/2003/05/soap-envelope\" \
xmlns:w=\"http://schemas.xmlsoap.org/ws/2004/08/addressing\" \
xmlns:d=\"http://schemas.xmlsoap.org/ws/2005/04/discovery\" \
xmlns:dn=\"http://www.onvif.org/ver10/network/wsdl\" \
xmlns:tds=\"http://www.onvif.org/ver10/device/wsdl\">\
<e:Header><w:MessageID>uuid:{uuid}</w:MessageID>\
<w:To e:mustUnderstand=\"true\">urn:schemas-xmlsoap-org:ws:2005:04:discovery</w:To>\
<w:Action e:mustUnderstand=\"true\">http://schemas.xmlsoap.org/ws/2005/04/discovery/Probe</w:Action></e:Header>\
<e:Body><d:Probe><d:Types>{types}</d:Types></d:Probe></e:Body></e:Envelope>"
    )
}

/// IPv4 addresses of the up, multicast-capable, non-loopback interfaces.
#[cfg(target_os = "linux")]
fn local_ipv4s() -> Vec<Ipv4Addr> {
    let mut out = Vec::new();
    // SAFETY: getifaddrs fills a linked list that is only read and then freed.
    unsafe {
        let mut head: *mut libc::ifaddrs = std::ptr::null_mut();
        if libc::getifaddrs(&mut head) != 0 {
            return out;
        }
        let mut cur = head;
        while !cur.is_null() {
            let i = &*cur;
            let flags = i.ifa_flags as i32;
            if !i.ifa_addr.is_null()
                && (*i.ifa_addr).sa_family as i32 == libc::AF_INET
                && flags & libc::IFF_UP != 0
                && flags & libc::IFF_MULTICAST != 0
                && flags & libc::IFF_LOOPBACK == 0
            {
                let sin = &*(i.ifa_addr as *const libc::sockaddr_in);
                let ip = Ipv4Addr::from(u32::from_be(sin.sin_addr.s_addr));
                if !out.contains(&ip) {
                    out.push(ip);
                }
            }
            cur = i.ifa_next;
        }
        libc::freeifaddrs(head);
    }
    out
}

#[cfg(not(target_os = "linux"))]
fn local_ipv4s() -> Vec<Ipv4Addr> {
    // The address this machine uses to reach the LAN. Probing through it
    // keeps multicast off virtual adapters (WSL, VPN, Hyper-V) that the
    // system might otherwise pick.
    let s = UdpSocket::bind("0.0.0.0:0");
    match s.and_then(|s| s.connect("192.0.2.1:9").map(|_| s)).and_then(|s| s.local_addr()) {
        Ok(SocketAddr::V4(a)) if !a.ip().is_unspecified() => vec![*a.ip()],
        _ => Vec::new(),
    }
}

/// A UDP socket whose multicast traffic leaves through the interface owning `ip`.
fn probe_socket(ip: Option<Ipv4Addr>) -> Result<UdpSocket> {
    let bind = ip.unwrap_or(Ipv4Addr::UNSPECIFIED);
    let s = UdpSocket::bind(SocketAddrV4::new(bind, 0))?;
    s.set_read_timeout(Some(Duration::from_millis(100)))?;
    s.set_multicast_ttl_v4(4)?;
    if let Some(ip) = ip {
        socket2::SockRef::from(&s)
            .set_multicast_if_v4(&ip)
            .map_err(|e| anyhow!("IP_MULTICAST_IF failed for {ip}: {e}"))?;
    }
    Ok(s)
}

/// Probe the LAN for ONVIF devices and collect answers for `wait`.
pub fn discover(wait: Duration) -> Vec<DiscoveredDevice> {
    let group = SocketAddr::from((Ipv4Addr::new(239, 255, 255, 250), 3702));
    let mut sockets = Vec::new();
    for ip in std::iter::once(None).chain(local_ipv4s().into_iter().map(Some)) {
        match probe_socket(ip) {
            Ok(s) => sockets.push(s),
            Err(e) => tracing::debug!("discovery socket {ip:?}: {e:#}"),
        }
    }
    let probes = [probe_message("dn:NetworkVideoTransmitter"), probe_message("tds:Device")];
    for s in &sockets {
        for p in &probes {
            let _ = s.send_to(p.as_bytes(), group);
        }
    }
    let deadline = Instant::now() + wait;
    let mut found: Vec<DiscoveredDevice> = Vec::new();
    let mut buf = vec![0u8; 65536];
    while Instant::now() < deadline {
        for s in &sockets {
            if let Ok((n, from)) = s.recv_from(&mut buf) {
                take_answer(&mut found, &buf[..n], from);
            }
        }
    }
    found.sort_by(|a, b| a.ip.cmp(&b.ip));
    found
}

/// Hosts of an IPv4 range in CIDR form ("192.168.1.0/24"), at most 1024,
/// network and broadcast addresses excluded for /30 and larger.
pub fn parse_subnet(cidr: &str) -> Result<Vec<Ipv4Addr>> {
    let (ip, bits) = cidr.trim().split_once('/').unwrap_or((cidr.trim(), "32"));
    let ip: Ipv4Addr = ip.parse().map_err(|_| anyhow!("bad address in '{cidr}'"))?;
    let bits: u32 = bits.parse().map_err(|_| anyhow!("bad prefix length in '{cidr}'"))?;
    if !(22..=32).contains(&bits) {
        bail!("use a range of at most 1024 addresses (/22 to /32)");
    }
    let mask = if bits == 0 { 0 } else { u32::MAX << (32 - bits) };
    let net = u32::from(ip) & mask;
    let size = 1u32 << (32 - bits);
    let (first, last) = if size >= 4 { (net + 1, net + size - 2) } else { (net, net + size - 1) };
    Ok((first..=last).map(Ipv4Addr::from).collect())
}

/// Most addresses one scan checks (four /22s, or sixteen /24s).
pub const MAX_SCAN_HOSTS: usize = 4096;

/// Several ranges separated by commas, semicolons or spaces
/// ("192.168.1.0/24, 10.0.20.0/24"): their hosts, each once.
pub fn parse_subnets(list: &str) -> Result<Vec<Ipv4Addr>> {
    let mut out: Vec<Ipv4Addr> = Vec::new();
    let mut seen = std::collections::HashSet::new();
    for part in split_ranges(list) {
        for h in parse_subnet(&part)? {
            if seen.insert(h) {
                out.push(h);
            }
        }
        if out.len() > MAX_SCAN_HOSTS {
            bail!("too many addresses: scan at most {MAX_SCAN_HOSTS} at once (sixteen /24 ranges)");
        }
    }
    Ok(out)
}

/// The ranges of a list as typed, trimmed (empty parts dropped).
pub fn split_ranges(list: &str) -> Vec<String> {
    list.split(|c: char| c == ',' || c == ';' || c.is_whitespace()).map(str::trim).filter(|p| !p.is_empty()).map(String::from).collect()
}

/// A host that accepts RTSP connections but did not answer ONVIF discovery.
#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct RtspHost {
    pub ip: String,
    pub port: u16,
}

/// Probe each host directly (unicast WS-Discovery to port 3702): finds
/// ONVIF cameras on other subnets, where multicast discovery cannot reach.
/// Hosts that do not answer ONVIF but accept RTSP (port 554) are listed too.
pub fn discover_hosts(hosts: &[Ipv4Addr], wait: Duration) -> (Vec<DiscoveredDevice>, Vec<RtspHost>) {
    let mut found: Vec<DiscoveredDevice> = Vec::new();
    let Ok(sock) = UdpSocket::bind("0.0.0.0:0") else { return (found, Vec::new()) };
    let _ = sock.set_read_timeout(Some(Duration::from_millis(50)));
    let probes = [probe_message("dn:NetworkVideoTransmitter"), probe_message("tds:Device")];
    let mut buf = vec![0u8; 65536];
    let mut drain = |found: &mut Vec<DiscoveredDevice>| {
        while let Ok((n, from)) = sock.recv_from(&mut buf) {
            take_answer(found, &buf[..n], from);
        }
    };
    for (i, ip) in hosts.iter().enumerate() {
        for p in &probes {
            let _ = sock.send_to(p.as_bytes(), SocketAddrV4::new(*ip, 3702));
        }
        // Pace the sweep a little and read answers as they arrive.
        if i % 32 == 31 {
            drain(&mut found);
        }
    }
    let deadline = Instant::now() + wait;
    while Instant::now() < deadline {
        drain(&mut found);
    }
    // RTSP-only devices (no ONVIF answer), checked in parallel.
    let onvif: std::collections::HashSet<String> = found.iter().map(|d| d.ip.clone()).collect();
    let rest: Vec<Ipv4Addr> = hosts.iter().copied().filter(|h| !onvif.contains(&h.to_string())).collect();
    let rtsp = std::sync::Mutex::new(Vec::new());
    std::thread::scope(|sc| {
        for chunk in rest.chunks(64) {
            let rtsp = &rtsp;
            sc.spawn(move || {
                for ip in chunk {
                    let addr = SocketAddr::from((*ip, 554));
                    if std::net::TcpStream::connect_timeout(&addr, Duration::from_millis(400)).is_ok() {
                        rtsp.lock().unwrap().push(RtspHost { ip: ip.to_string(), port: 554 });
                    }
                }
            });
        }
    });
    let mut rtsp = rtsp.into_inner().unwrap();
    rtsp.sort_by_key(|h| h.ip.parse::<Ipv4Addr>().ok());
    found.sort_by_key(|d| d.ip.parse::<Ipv4Addr>().ok());
    (found, rtsp)
}

// ---------------------------------------------------------------------------
// Device inspection
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Default, Serialize, PartialEq)]
pub struct DeviceInfo {
    pub manufacturer: String,
    pub model: String,
    pub firmware: String,
    pub serial: String,
    pub hardware_id: String,
}

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct StreamProfile {
    pub name: String,
    pub token: String,
    pub width: u32,
    pub height: u32,
    pub encoding: String,
    pub has_ptz: bool,
    /// RTSP URI without credentials.
    pub uri: Option<String>,
    /// Video source the profile encodes (one per physical camera / channel).
    #[serde(default)]
    pub source: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct Inspection {
    pub info: DeviceInfo,
    pub media_xaddr: Option<String>,
    pub ptz_xaddr: Option<String>,
    pub profiles: Vec<StreamProfile>,
    /// Indexes into `profiles`.
    pub main: Option<usize>,
    pub sub: Option<usize>,
    /// Camera config to POST to `/api/cameras` (add the password first):
    /// the first channel. Multi-channel devices (hubs, recorders, dual-lens
    /// cameras) have one entry per channel in `cameras`.
    pub suggested: serde_json::Value,
    pub cameras: Vec<serde_json::Value>,
}

fn soap_ns(prefix: &str, ns: &str) -> String {
    format!("xmlns:{prefix}=\"{ns}\"")
}

fn body_get_all_capabilities() -> &'static str {
    "<tds:GetCapabilities><tds:Category>All</tds:Category></tds:GetCapabilities>"
}

fn body_get_profiles() -> String {
    format!("<trt:GetProfiles {}/>", soap_ns("trt", NS_MEDIA))
}

fn body_get_stream_uri(token: &str) -> String {
    format!(
        "<trt:GetStreamUri {} {}><trt:StreamSetup><tt:Stream>RTP-Unicast</tt:Stream>\
<tt:Transport><tt:Protocol>RTSP</tt:Protocol></tt:Transport></trt:StreamSetup>\
<trt:ProfileToken>{}</trt:ProfileToken></trt:GetStreamUri>",
        soap_ns("trt", NS_MEDIA),
        soap_ns("tt", NS_SCHEMA),
        xml_escape(token)
    )
}

pub fn parse_device_information(xml: &str) -> Option<DeviceInfo> {
    let doc = roxmltree::Document::parse(xml).ok()?;
    let r = find_desc(&doc, "GetDeviceInformationResponse")?;
    let f = |n: &str| find_text(r, n).unwrap_or_default();
    Some(DeviceInfo {
        manufacturer: f("Manufacturer"),
        model: f("Model"),
        firmware: f("FirmwareVersion"),
        serial: f("SerialNumber"),
        hardware_id: f("HardwareId"),
    })
}

/// Address of a service (`"Media"`, `"PTZ"`) from a GetCapabilities or
/// GetServices response. `ns_suffix` is the tail of the service namespace
/// used by GetServices (e.g. `/media/wsdl`).
pub fn parse_service_xaddr(xml: &str, capability: &str, ns_suffix: &str) -> Option<String> {
    let doc = roxmltree::Document::parse(xml).ok()?;
    if let Some(c) = doc.descendants().find(|n| n.is_element() && n.tag_name().name() == capability) {
        if let Some(x) = local_children(c, "XAddr").next() {
            let t = text_of(x);
            if !t.is_empty() {
                return Some(t);
            }
        }
    }
    for svc in doc.descendants().filter(|n| n.is_element() && n.tag_name().name() == "Service") {
        let ns = local_children(svc, "Namespace").next().map(text_of).unwrap_or_default();
        if ns.ends_with(ns_suffix) {
            if let Some(x) = local_children(svc, "XAddr").next() {
                return Some(text_of(x));
            }
        }
    }
    None
}

/// Media profiles from a GetProfilesResponse (no stream URIs yet).
pub fn parse_profiles(xml: &str) -> Vec<StreamProfile> {
    let Ok(doc) = roxmltree::Document::parse(xml) else { return Vec::new() };
    let mut out = Vec::new();
    for p in doc.descendants().filter(|n| n.is_element() && n.tag_name().name() == "Profiles") {
        let Some(token) = p.attribute("token") else { continue };
        let name = local_children(p, "Name").next().map(text_of).unwrap_or_default();
        let venc = local_children(p, "VideoEncoderConfiguration").next();
        let encoding = venc.and_then(|v| local_children(v, "Encoding").next()).map(text_of).unwrap_or_default();
        let res = venc.and_then(|v| local_children(v, "Resolution").next());
        let dim = |n: &str| res.and_then(|r| find_text(r, n)).and_then(|t| t.parse().ok()).unwrap_or(0);
        out.push(StreamProfile {
            name,
            token: token.to_string(),
            width: dim("Width"),
            height: dim("Height"),
            encoding,
            has_ptz: local_children(p, "PTZConfiguration").next().is_some(),
            uri: None,
            source: local_children(p, "VideoSourceConfiguration")
                .next()
                .and_then(|v| local_children(v, "SourceToken").next())
                .map(text_of)
                .filter(|t| !t.is_empty()),
        });
    }
    out
}

/// The `Uri` of a GetStreamUriResponse.
pub fn parse_stream_uri(xml: &str) -> Option<String> {
    let doc = roxmltree::Document::parse(xml).ok()?;
    let r = find_desc(&doc, "MediaUri")?;
    find_text(r, "Uri").filter(|u| !u.is_empty())
}

/// Drop `user:pass@` from a URI, and point it at `host` when the device
/// advertised a different (often internal) address. None for what cannot be
/// read and for any other kind of address than a network stream: what a device
/// says here goes to the stream reader, and it must not name a local file.
pub fn clean_stream_uri(uri: &str, host: &str) -> Option<String> {
    let mut u = url::Url::parse(uri).ok()?;
    if !matches!(u.scheme(), "rtsp" | "rtsps" | "http" | "https") {
        return None;
    }
    let _ = u.set_username("");
    let _ = u.set_password(None);
    if u.host_str() != Some(host) {
        let _ = u.set_host(Some(host));
    }
    Some(u.to_string())
}

/// Choose the main (highest resolution) and sub (lowest resolution) profiles
/// among those with a stream URI. Video formats beat JPEG; a sub is only
/// returned when it is genuinely smaller than the main.
pub fn select_main_sub(profiles: &[StreamProfile]) -> (Option<usize>, Option<usize>) {
    let is_jpeg = |p: &StreamProfile| p.encoding.to_ascii_lowercase().contains("jpeg");
    let mut cands: Vec<usize> =
        (0..profiles.len()).filter(|&i| profiles[i].uri.is_some() && !is_jpeg(&profiles[i])).collect();
    if cands.is_empty() {
        cands = (0..profiles.len()).filter(|&i| profiles[i].uri.is_some()).collect();
    }
    let px = |i: usize| u64::from(profiles[i].width) * u64::from(profiles[i].height);
    // First of the largest / smallest, so ties keep the camera's own order.
    let main = cands.iter().copied().fold(None, |best: Option<usize>, i| match best {
        Some(b) if px(b) >= px(i) => Some(b),
        _ => Some(i),
    });
    let Some(main) = main else { return (None, None) };
    let sub = cands.iter().copied().fold(None, |best: Option<usize>, i| match best {
        Some(b) if px(b) <= px(i) => Some(b),
        _ => Some(i),
    });
    let sub = sub.filter(|&s| s != main && px(s) < px(main));
    (Some(main), sub)
}

/// Group profiles into channels: by video source when the device reports
/// it, else by stream address with "main"/"sub" removed (so a hub's
/// Preview_02_main pairs with Preview_02_sub, not with another channel).
pub fn channel_groups(profiles: &[StreamProfile]) -> Vec<Vec<usize>> {
    let key = |p: &StreamProfile| -> String {
        if let Some(s) = &p.source {
            return format!("src:{s}");
        }
        let u = p.uri.clone().unwrap_or_default().to_ascii_lowercase();
        format!("uri:{}", u.replace("main", "").replace("sub", ""))
    };
    let mut groups: Vec<(String, Vec<usize>)> = Vec::new();
    for (i, p) in profiles.iter().enumerate() {
        let k = key(p);
        match groups.iter_mut().find(|(g, _)| *g == k) {
            Some((_, v)) => v.push(i),
            None => groups.push((k, vec![i])),
        }
    }
    groups.into_iter().map(|(_, v)| v).collect()
}

/// Main/sub per channel, as indexes into `profiles`.
pub fn channels(profiles: &[StreamProfile]) -> Vec<(Option<usize>, Option<usize>)> {
    channel_groups(profiles)
        .into_iter()
        .map(|g| {
            let subset: Vec<StreamProfile> = g.iter().map(|&i| profiles[i].clone()).collect();
            let (m, s) = select_main_sub(&subset);
            (m.map(|k| g[k]), s.map(|k| g[k]))
        })
        .filter(|(m, _)| m.is_some())
        .collect()
}

/// A ready-to-use camera configuration (no password).
fn suggested_camera(
    info: &DeviceInfo,
    host: &str,
    port: u16,
    username: &str,
    profiles: &[StreamProfile],
    main: Option<usize>,
    sub: Option<usize>,
) -> serde_json::Value {
    let name = if !info.model.is_empty() { info.model.clone() } else { host.to_string() };
    let mut v = serde_json::json!({
        "name": name,
        "url": main.and_then(|i| profiles[i].uri.clone()).unwrap_or_default(),
        "onvif_port": port,
        "username": username,
    });
    if let Some(s) = sub.and_then(|i| profiles[i].uri.clone()) {
        v["sub_url"] = s.into();
    }
    v
}

fn creds_for(xaddr: &str, username: &str, password: &str) -> Result<(Credentials, url::Url)> {
    let u = url::Url::parse(xaddr).with_context(|| format!("bad address {xaddr}"))?;
    if u.scheme() != "http" {
        bail!("only http:// ONVIF addresses are supported");
    }
    let host = u
        .host_str()
        .ok_or_else(|| anyhow!("address has no host"))?
        .trim_matches(|c| c == '[' || c == ']')
        .to_string();
    let port = u.port_or_known_default().unwrap_or(80);
    Ok((Credentials { host, port, username: username.to_string(), password: password.to_string() }, u))
}

/// Service address from GetCapabilities, falling back to GetServices.
fn service_xaddr(c: &mut Client, dev_url: &str, capability: &str, ns_suffix: &str) -> Option<String> {
    let bodies = [
        body_get_all_capabilities(),
        "<tds:GetServices><tds:IncludeCapability>false</tds:IncludeCapability></tds:GetServices>",
    ];
    for body in bodies {
        if let Ok(xml) = c.call(dev_url, "", body, true, CALL_TIMEOUT) {
            if let Some(x) = parse_service_xaddr(&xml, capability, ns_suffix) {
                return Some(c.fix_host(&x));
            }
        }
    }
    None
}

/// Ask a device who it is and what it streams. Takes tens of seconds on slow
/// cameras (one call per profile).
pub fn inspect(xaddr: &str, username: &str, password: &str) -> Result<Inspection> {
    let (creds, url) = creds_for(xaddr, username, password)?;
    let stop = AtomicBool::new(false);
    let mut c = Client::new(&creds, &stop);
    c.sync_clock();
    let dev_url = url.to_string();
    let info_xml = c
        .call(&dev_url, "", "<tds:GetDeviceInformation/>", true, CALL_TIMEOUT)
        .context("GetDeviceInformation")?;
    let info = parse_device_information(&info_xml).unwrap_or_default();
    let media = service_xaddr(&mut c, &dev_url, "Media", "/media/wsdl");
    let ptz = service_xaddr(&mut c, &dev_url, "PTZ", "/ptz/wsdl");
    let media_url = media
        .clone()
        .unwrap_or_else(|| format!("http://{}:{}/onvif/media_service", creds.host, creds.port));
    let xml = c.call(&media_url, "", &body_get_profiles(), true, CALL_TIMEOUT).context("GetProfiles")?;
    let mut profiles = parse_profiles(&xml);
    for p in &mut profiles {
        match c.call(&media_url, "", &body_get_stream_uri(&p.token), true, CALL_TIMEOUT) {
            Ok(xml) => p.uri = parse_stream_uri(&xml).and_then(|u| clean_stream_uri(&u, &creds.host)),
            Err(e) => tracing::debug!("GetStreamUri {}: {e:#}", p.token),
        }
    }
    let chans = channels(&profiles);
    let mut cameras: Vec<serde_json::Value> = chans
        .iter()
        .map(|(m, s)| suggested_camera(&info, &creds.host, creds.port, username, &profiles, *m, *s))
        .collect();
    if cameras.len() > 1 {
        for (n, c) in cameras.iter_mut().enumerate() {
            let base = c["name"].as_str().unwrap_or("Camera").to_string();
            c["name"] = format!("{base} channel {}", n + 1).into();
        }
    }
    let (main, sub) = chans.first().copied().unwrap_or((None, None));
    let suggested = cameras.first().cloned().unwrap_or_else(|| suggested_camera(&info, &creds.host, creds.port, username, &profiles, None, None));
    Ok(Inspection { info, media_xaddr: media, ptz_xaddr: ptz, profiles, main, sub, suggested, cameras })
}

/// Render a suggested camera as a `[[cameras]]` TOML block.
pub fn toml_block(suggested: &serde_json::Value, name: &str) -> String {
    let s = |k: &str| suggested.get(k).and_then(|v| v.as_str()).unwrap_or("");
    let q = |v: &str| serde_json::to_string(v).unwrap_or_else(|_| "\"\"".into());
    let mut out = String::from("[[cameras]]\n");
    out.push_str(&format!("name = {}\n", q(if name.is_empty() { s("name") } else { name })));
    out.push_str(&format!("url = {}\n", q(s("url"))));
    if !s("sub_url").is_empty() {
        out.push_str(&format!("sub_url = {}\n", q(s("sub_url"))));
    }
    if let Some(p) = suggested.get("onvif_port").and_then(|v| v.as_u64()) {
        out.push_str(&format!("onvif_port = {p}\n"));
    }
    if !s("username").is_empty() {
        out.push_str(&format!("username = {}\npassword = \"CHANGE_ME\"\n", q(s("username"))));
    }
    out
}

// ---------------------------------------------------------------------------
// PTZ
// ---------------------------------------------------------------------------

/// The camera has no PTZ service or no PTZ-capable media profile.
#[derive(Debug)]
pub struct NoPtz(pub String);

impl std::fmt::Display for NoPtz {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for NoPtz {}

/// Where and how to reach a camera's ONVIF services.
#[derive(Debug, Clone)]
pub struct PtzTarget {
    pub host: String,
    pub port: u16,
    pub username: String,
    pub password: String,
}

/// Build a target from a camera's config: URL host + `onvif_port`, with the
/// camera's credentials (falling back to those in the URL).
pub fn target_from_config(cfg: &CameraConfig) -> Result<PtzTarget> {
    let port = cfg.onvif_port.ok_or_else(|| anyhow!("camera has no onvif_port configured"))?;
    let host = crate::onvif::host_from_rtsp_url(&cfg.url).ok_or_else(|| anyhow!("camera url has no host"))?;
    let (mut username, mut password) = (cfg.username.clone(), cfg.password.clone());
    if username.is_empty() {
        if let Some((u, p)) = crate::onvif::credentials_from_url(&cfg.url) {
            username = u;
            password = p;
        }
    }
    Ok(PtzTarget { host, port, username, password })
}

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct Preset {
    pub token: String,
    pub name: String,
}

pub fn parse_presets(xml: &str) -> Vec<Preset> {
    let Ok(doc) = roxmltree::Document::parse(xml) else { return Vec::new() };
    doc.descendants()
        .filter(|n| n.is_element() && n.tag_name().name() == "Preset")
        .filter_map(|p| {
            let token = p.attribute("token")?.to_string();
            let name = local_children(p, "Name").next().map(text_of).unwrap_or_default();
            Some(Preset { name: if name.is_empty() { token.clone() } else { name }, token })
        })
        .collect()
}

fn clamp_unit(v: f64) -> f64 {
    if v.is_finite() {
        v.clamp(-1.0, 1.0)
    } else {
        0.0
    }
}

pub fn body_continuous_move(token: &str, pan: f64, tilt: f64, zoom: f64, timeout: Option<Duration>) -> String {
    let timeout = timeout
        .map(|t| format!("<tptz:Timeout>PT{:.1}S</tptz:Timeout>", t.as_secs_f64()))
        .unwrap_or_default();
    format!(
        "<tptz:ContinuousMove {} {}><tptz:ProfileToken>{}</tptz:ProfileToken><tptz:Velocity>\
<tt:PanTilt x=\"{:.3}\" y=\"{:.3}\"/><tt:Zoom x=\"{:.3}\"/></tptz:Velocity>{timeout}</tptz:ContinuousMove>",
        soap_ns("tptz", NS_PTZ),
        soap_ns("tt", NS_SCHEMA),
        xml_escape(token),
        clamp_unit(pan),
        clamp_unit(tilt),
        clamp_unit(zoom),
    )
}

pub fn body_absolute_move(token: &str, pan: f64, tilt: f64, zoom: f64) -> String {
    format!(
        "<tptz:AbsoluteMove {} {}><tptz:ProfileToken>{}</tptz:ProfileToken><tptz:Position>\
<tt:PanTilt x=\"{:.3}\" y=\"{:.3}\"/><tt:Zoom x=\"{:.3}\"/></tptz:Position></tptz:AbsoluteMove>",
        soap_ns("tptz", NS_PTZ),
        soap_ns("tt", NS_SCHEMA),
        xml_escape(token),
        clamp_unit(pan),
        clamp_unit(tilt),
        clamp_unit(zoom),
    )
}

pub fn body_stop(token: &str) -> String {
    format!(
        "<tptz:Stop {}><tptz:ProfileToken>{}</tptz:ProfileToken><tptz:PanTilt>true</tptz:PanTilt><tptz:Zoom>true</tptz:Zoom></tptz:Stop>",
        soap_ns("tptz", NS_PTZ),
        xml_escape(token)
    )
}

pub fn body_get_presets(token: &str) -> String {
    format!(
        "<tptz:GetPresets {}><tptz:ProfileToken>{}</tptz:ProfileToken></tptz:GetPresets>",
        soap_ns("tptz", NS_PTZ),
        xml_escape(token)
    )
}

pub fn body_goto_preset(token: &str, preset: &str) -> String {
    format!(
        "<tptz:GotoPreset {}><tptz:ProfileToken>{}</tptz:ProfileToken><tptz:PresetToken>{}</tptz:PresetToken></tptz:GotoPreset>",
        soap_ns("tptz", NS_PTZ),
        xml_escape(token),
        xml_escape(preset)
    )
}

pub fn body_set_preset(token: &str, name: &str) -> String {
    format!(
        "<tptz:SetPreset {}><tptz:ProfileToken>{}</tptz:ProfileToken><tptz:PresetName>{}</tptz:PresetName></tptz:SetPreset>",
        soap_ns("tptz", NS_PTZ),
        xml_escape(token),
        xml_escape(name)
    )
}

pub fn body_remove_preset(token: &str, preset: &str) -> String {
    format!(
        "<tptz:RemovePreset {}><tptz:ProfileToken>{}</tptz:ProfileToken><tptz:PresetToken>{}</tptz:PresetToken></tptz:RemovePreset>",
        soap_ns("tptz", NS_PTZ),
        xml_escape(token),
        xml_escape(preset)
    )
}

pub fn body_get_nodes() -> String {
    format!("<tptz:GetNodes {}/>", soap_ns("tptz", NS_PTZ))
}

pub fn body_get_status(token: &str) -> String {
    format!(
        "<tptz:GetStatus {}><tptz:ProfileToken>{}</tptz:ProfileToken></tptz:GetStatus>",
        soap_ns("tptz", NS_PTZ),
        xml_escape(token)
    )
}

/// Resolved PTZ endpoint for one camera.
#[derive(Clone)]
struct PtzSession {
    ptz_url: String,
    token: String,
    clock_offset: chrono::Duration,
}

fn sessions() -> &'static Mutex<HashMap<String, PtzSession>> {
    static S: OnceLock<Mutex<HashMap<String, PtzSession>>> = OnceLock::new();
    S.get_or_init(Default::default)
}

fn session_key(t: &PtzTarget) -> String {
    format!("{}:{}:{}", t.host, t.port, t.username)
}

fn creds_of(t: &PtzTarget) -> Credentials {
    Credentials { host: t.host.clone(), port: t.port, username: t.username.clone(), password: t.password.clone() }
}

/// Find the PTZ service address and the first PTZ-capable profile token.
fn resolve_session(t: &PtzTarget, c: &mut Client) -> Result<PtzSession> {
    c.sync_clock();
    let dev_url = format!("http://{}:{}/onvif/device_service", t.host, t.port);
    let ptz_url = service_xaddr(c, &dev_url, "PTZ", "/ptz/wsdl")
        .ok_or_else(|| NoPtz("this camera has no PTZ service".into()))?;
    let media_url = service_xaddr(c, &dev_url, "Media", "/media/wsdl")
        .unwrap_or_else(|| format!("http://{}:{}/onvif/media_service", t.host, t.port));
    let xml = c.call(&media_url, "", &body_get_profiles(), true, CALL_TIMEOUT).context("GetProfiles")?;
    let token = parse_profiles(&xml)
        .into_iter()
        .find(|p| p.has_ptz)
        .map(|p| p.token)
        .ok_or_else(|| NoPtz("this camera has no media profile with a PTZ configuration".into()))?;
    let s = PtzSession { ptz_url, token, clock_offset: c.clock_offset };
    // Some devices advertise a PTZ service and PTZ configurations without any
    // motors; a PTZ node (real or none) tells the truth.
    match ptz_call(c, &s, "GetNodes", &body_get_nodes()) {
        Ok(xml) if !has_ptz_node(&xml) => return Err(NoPtz("this camera reports no PTZ hardware".into()).into()),
        Err(e) => tracing::debug!("GetNodes: {e:#}"),
        Ok(_) => {}
    }
    Ok(s)
}

/// Whether a GetNodesResponse lists at least one PTZ node.
fn has_ptz_node(xml: &str) -> bool {
    roxmltree::Document::parse(xml)
        .map(|d| d.descendants().any(|n| n.is_element() && n.tag_name().name() == "PTZNode"))
        .unwrap_or(true)
}

/// Run `f` with an authenticated client and the camera's PTZ session.
fn with_session<T>(t: &PtzTarget, f: impl FnOnce(&mut Client, &PtzSession) -> Result<T>) -> Result<T> {
    let creds = creds_of(t);
    let stop = AtomicBool::new(false);
    let mut c = Client::new(&creds, &stop);
    let key = session_key(t);
    let cached = sessions().lock().unwrap().get(&key).cloned();
    let session = match cached {
        Some(s) => {
            c.clock_offset = s.clock_offset;
            s
        }
        None => {
            let s = resolve_session(t, &mut c)?;
            sessions().lock().unwrap().insert(key.clone(), s.clone());
            s
        }
    };
    let r = f(&mut c, &session);
    if r.is_err() {
        // The profile or address may have changed; look again next time.
        sessions().lock().unwrap().remove(&key);
    }
    r
}

fn ptz_call(c: &mut Client, s: &PtzSession, op: &str, body: &str) -> Result<String> {
    c.call(&s.ptz_url, &format!("{NS_PTZ}/{op}"), body, true, CALL_TIMEOUT).with_context(|| op.to_string())
}

/// Start moving at the given velocities (each -1..1). With a timeout the
/// camera stops by itself.
pub fn ptz_move(t: &PtzTarget, pan: f64, tilt: f64, zoom: f64, timeout: Option<Duration>) -> Result<()> {
    with_session(t, |c, s| {
        ptz_call(c, s, "ContinuousMove", &body_continuous_move(&s.token, pan, tilt, zoom, timeout)).map(drop)
    })
}

pub fn ptz_absolute(t: &PtzTarget, pan: f64, tilt: f64, zoom: f64) -> Result<()> {
    with_session(t, |c, s| ptz_call(c, s, "AbsoluteMove", &body_absolute_move(&s.token, pan, tilt, zoom)).map(drop))
}

pub fn ptz_stop(t: &PtzTarget) -> Result<()> {
    with_session(t, |c, s| ptz_call(c, s, "Stop", &body_stop(&s.token)).map(drop))
}

pub fn ptz_presets(t: &PtzTarget) -> Result<Vec<Preset>> {
    with_session(t, |c, s| match ptz_call(c, s, "GetPresets", &body_get_presets(&s.token)) {
        Ok(xml) => Ok(parse_presets(&xml)),
        // Some multi-channel recorders list PTZ profiles for every channel and
        // answer a motorless one with a bare fault (no reason, no subcode).
        Err(e) if format!("{e:#}").contains("SOAP fault: (HTTP") => {
            Err(NoPtz("the camera rejected the PTZ request; it most likely has no PTZ motor".into()).into())
        }
        Err(e) => Err(e),
    })
}

pub fn ptz_goto(t: &PtzTarget, preset: &str) -> Result<()> {
    with_session(t, |c, s| ptz_call(c, s, "GotoPreset", &body_goto_preset(&s.token, preset)).map(drop))
}

/// Store the current position as a preset; returns its token.
pub fn ptz_set_preset(t: &PtzTarget, name: &str) -> Result<String> {
    with_session(t, |c, s| {
        let xml = ptz_call(c, s, "SetPreset", &body_set_preset(&s.token, name))?;
        let doc = roxmltree::Document::parse(&xml)?;
        Ok(find_text(doc.root_element(), "PresetToken").unwrap_or_default())
    })
}

pub fn ptz_remove_preset(t: &PtzTarget, preset: &str) -> Result<()> {
    with_session(t, |c, s| ptz_call(c, s, "RemovePreset", &body_remove_preset(&s.token, preset)).map(drop))
}

/// Current position and whether the camera is moving.
pub fn ptz_status(t: &PtzTarget) -> Result<serde_json::Value> {
    with_session(t, |c, s| {
        let xml = ptz_call(c, s, "GetStatus", &body_get_status(&s.token))?;
        let doc = roxmltree::Document::parse(&xml)?;
        let num = |n: roxmltree::Node, name: &str, attr: &str| -> Option<f64> {
            n.descendants().find(|d| d.is_element() && d.tag_name().name() == name)?.attribute(attr)?.parse().ok()
        };
        let pos = find_desc(&doc, "Position");
        let moving = find_desc(&doc, "MoveStatus")
            .map(|m| {
                m.descendants()
                    .filter(|d| d.is_element() && matches!(d.tag_name().name(), "PanTilt" | "Zoom"))
                    .any(|d| text_of(d).eq_ignore_ascii_case("MOVING"))
            })
            .unwrap_or(false);
        Ok(serde_json::json!({
            "pan": pos.and_then(|p| num(p, "PanTilt", "x")),
            "tilt": pos.and_then(|p| num(p, "PanTilt", "y")),
            "zoom": pos.and_then(|p| num(p, "Zoom", "x")),
            "moving": moving,
        }))
    })
}

// --- Capabilities and click-to-aim ---

const FOV_SPACE: &str = "http://www.onvif.org/ver10/tptz/PanTiltSpaces/TranslationSpaceFov";
const GENERIC_SPACE: &str = "http://www.onvif.org/ver10/tptz/PanTiltSpaces/TranslationGenericSpace";

/// How a relative pan/tilt move is expressed on this camera.
#[derive(Debug, Clone, PartialEq)]
pub enum AimSpace {
    /// Offsets in fractions of the picture; `range` = the camera's maximum
    /// (a full picture width/height).
    Fov { range: f64 },
    /// Offsets in the normalized pan/tilt range (needs an assumed field of view).
    Generic,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct PtzCaps {
    /// A PTZ node exists.
    pub ptz: bool,
    pub pan_tilt: bool,
    pub zoom: bool,
    /// Relative pan/tilt in a space we can use.
    pub aim: Option<AimSpace>,
    /// Relative zoom is supported (box-zoom).
    pub aim_zoom: bool,
}

/// What a GetNodes answer says the first PTZ node can do.
pub fn parse_nodes(xml: &str) -> PtzCaps {
    let Ok(doc) = roxmltree::Document::parse(xml) else { return PtzCaps::default() };
    let Some(node) = doc.descendants().find(|n| n.is_element() && n.tag_name().name() == "PTZNode") else {
        return PtzCaps::default();
    };
    let spaces = node.descendants().find(|n| n.is_element() && n.tag_name().name() == "SupportedPTZSpaces");
    let Some(sp) = spaces else {
        // No detail given: assume plain pan/tilt only.
        return PtzCaps { ptz: true, pan_tilt: true, ..Default::default() };
    };
    let find_all = |name: &'static str| sp.children().filter(move |n| n.is_element() && n.tag_name().name() == name);
    let has = |names: &[&'static str]| names.iter().any(|n| find_all(n).next().is_some());
    let pan_tilt = has(&["ContinuousPanTiltVelocitySpace", "AbsolutePanTiltPositionSpace", "RelativePanTiltTranslationSpace"]);
    let zoom = has(&["ContinuousZoomVelocitySpace", "AbsoluteZoomPositionSpace", "RelativeZoomTranslationSpace"]);
    let mut aim = None;
    for s in find_all("RelativePanTiltTranslationSpace") {
        let uri = local_children(s, "URI").next().map(text_of).unwrap_or_default();
        if uri.ends_with("TranslationSpaceFov") {
            let range = local_children(s, "XRange")
                .next()
                .and_then(|r| local_children(r, "Max").next().map(text_of))
                .and_then(|m| m.trim().parse::<f64>().ok())
                .filter(|m| *m > 0.0)
                .unwrap_or(100.0);
            aim = Some(AimSpace::Fov { range });
            break;
        }
        if uri.ends_with("TranslationGenericSpace") && aim.is_none() {
            aim = Some(AimSpace::Generic);
        }
    }
    let aim_zoom = find_all("RelativeZoomTranslationSpace").any(|s| {
        local_children(s, "URI").next().map(text_of).unwrap_or_default().ends_with("TranslationGenericSpace")
    });
    PtzCaps { ptz: pan_tilt || zoom, pan_tilt, zoom, aim, aim_zoom }
}

/// A computed relative move.
#[derive(Debug, Clone, PartialEq)]
pub struct AimMove {
    pub pan: f64,
    pub tilt: f64,
    pub zoom: Option<f64>,
    pub space: &'static str,
}

/// Turn a point (or box) in the picture (0..1, y down) into a relative move.
/// `Err` = not supported or bad input.
pub fn aim_move(caps: &PtzCaps, x: f64, y: f64, size: Option<(f64, f64)>) -> Result<AimMove, &'static str> {
    let aim = caps.aim.as_ref().ok_or("aim not supported")?;
    if ![x, y].iter().all(|v| v.is_finite() && (0.0..=1.0).contains(v)) {
        return Err("x and y must be between 0 and 1");
    }
    let (dx, dy) = (x - 0.5, 0.5 - y); // image y grows downwards, ONVIF tilt up is positive
    let (pan, tilt, space) = match aim {
        AimSpace::Fov { range } => (dx * range, dy * range, FOV_SPACE),
        // Assumed field of view: about a third of the pan range, a quarter of the tilt range.
        AimSpace::Generic => (dx * 0.35, dy * 0.25, GENERIC_SPACE),
    };
    let zoom = match size {
        Some((w, h)) if caps.aim_zoom && w.is_finite() && h.is_finite() && w > 0.0 && h > 0.0 => {
            let frac = w.max(h).clamp(0.05, 1.0);
            // The smaller the box, the more we zoom in (generic zoom range 0..1).
            (frac < 0.98).then_some(((1.0 - frac) * 0.5).clamp(0.0, 1.0))
        }
        Some((w, h)) if !(w.is_finite() && h.is_finite() && w > 0.0 && h > 0.0) => return Err("w and h must be positive"),
        _ => None,
    };
    Ok(AimMove { pan, tilt, zoom, space })
}

pub fn body_relative_move(token: &str, m: &AimMove) -> String {
    let zoom = m.zoom.map(|z| format!("<tt:Zoom x=\"{z:.3}\"/>")).unwrap_or_default();
    format!(
        "<tptz:RelativeMove {} {}><tptz:ProfileToken>{}</tptz:ProfileToken><tptz:Translation>\
<tt:PanTilt x=\"{:.3}\" y=\"{:.3}\" space=\"{}\"/>{zoom}</tptz:Translation></tptz:RelativeMove>",
        soap_ns("tptz", NS_PTZ),
        soap_ns("tt", NS_SCHEMA),
        xml_escape(token),
        m.pan,
        m.tilt,
        m.space,
    )
}

/// Send a relative move (click-to-aim).
pub fn ptz_relative(t: &PtzTarget, m: &AimMove) -> Result<()> {
    with_session(t, |c, s| ptz_call(c, s, "RelativeMove", &body_relative_move(&s.token, m)).map(drop))
}

/// Ask the camera what its PTZ node supports. No PTZ => default caps.
pub fn ptz_probe_caps(t: &PtzTarget) -> Result<PtzCaps> {
    let r = with_session(t, |c, s| Ok(parse_nodes(&ptz_call(c, s, "GetNodes", &body_get_nodes())?)));
    match r {
        Ok(c) => Ok(c),
        Err(e) if e.downcast_ref::<NoPtz>().is_some() => Ok(PtzCaps::default()),
        Err(e) => Err(e),
    }
}

/// Run `f` on a helper thread and wait at most `d` for it.
pub fn run_with_timeout<T: Send + 'static>(d: Duration, f: impl FnOnce() -> T + Send + 'static) -> Option<T> {
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let _ = tx.send(f());
    });
    rx.recv_timeout(d).ok()
}

/// One camera's remembered capabilities.
#[derive(Debug, Clone)]
pub struct CapsEntry {
    /// Identifies the connection settings the answer belongs to.
    pub fingerprint: String,
    pub at: Instant,
    pub caps: PtzCaps,
    pub error: Option<String>,
}

fn caps_cache() -> &'static Mutex<HashMap<usize, CapsEntry>> {
    static C: OnceLock<Mutex<HashMap<usize, CapsEntry>>> = OnceLock::new();
    C.get_or_init(Default::default)
}

pub fn fingerprint(cfg: &CameraConfig) -> String {
    format!("{}|{:?}|{}|{}", cfg.url, cfg.onvif_port, cfg.username, cfg.password.len())
}

/// The cached answer for a camera, whatever settings it was made for.
pub fn cached_any(id: usize) -> Option<CapsEntry> {
    caps_cache().lock().unwrap().get(&id).cloned()
}

/// The cached answer, only if it was made for these connection settings.
pub fn cached_caps(id: usize, fp: &str) -> Option<CapsEntry> {
    caps_cache().lock().unwrap().get(&id).filter(|e| e.fingerprint == fp).cloned()
}

/// Check (and remember) a camera's capabilities; waits at most ~5 s.
pub fn check_caps(id: usize, cfg: &CameraConfig) -> CapsEntry {
    let fp = fingerprint(cfg);
    let (caps, error) = match target_from_config(cfg) {
        Err(_) => (PtzCaps::default(), None), // no ONVIF configured
        Ok(t) => match run_with_timeout(Duration::from_secs(5), move || ptz_probe_caps(&t)) {
            Some(Ok(c)) => (c, None),
            Some(Err(e)) => (PtzCaps::default(), Some(format!("{e:#}"))),
            None => (PtzCaps::default(), Some("the camera did not answer in time".to_string())),
        },
    };
    let e = CapsEntry { fingerprint: fp, at: Instant::now(), caps, error };
    caps_cache().lock().unwrap().insert(id, e.clone());
    e
}

// --- Finding a camera's ONVIF port when none is configured ---

/// Ports cameras commonly serve ONVIF on, in the order they are tried.
pub const ONVIF_PORTS: [u16; 8] = [80, 8000, 8080, 2020, 8899, 5000, 10080, 888];
/// A camera where nothing was found is not searched again for this long
/// (unless its connection settings change or the owner asks).
pub const FIND_BACKOFF: Duration = Duration::from_secs(6 * 3600);

/// What the background search knows about one camera.
#[derive(Debug, Clone)]
pub struct FindEntry {
    /// Host and login the answer belongs to.
    pub fingerprint: String,
    pub port: Option<u16>,
    pub at: Instant,
    pub scanning: bool,
}

fn find_cache() -> &'static Mutex<HashMap<usize, FindEntry>> {
    static C: OnceLock<Mutex<HashMap<usize, FindEntry>>> = OnceLock::new();
    C.get_or_init(Default::default)
}

/// Whether the search applies: a network camera with no ONVIF port set and
/// no Reolink channel (those are controlled through their own API).
pub fn find_wanted(cfg: &CameraConfig) -> bool {
    cfg.onvif_port.is_none()
        && cfg.reolink_channel.is_none()
        && ["rtsp://", "rtsps://", "http://", "https://"].iter().any(|p| cfg.url.to_ascii_lowercase().starts_with(p))
        && crate::onvif::host_from_rtsp_url(&cfg.url).is_some()
}

/// Identifies the settings a search answer belongs to (host and login).
pub fn find_fingerprint(cfg: &CameraConfig) -> String {
    format!("{}|{}|{}", crate::onvif::host_from_rtsp_url(&cfg.url).unwrap_or_default(), cfg.username, cfg.password.len())
}

/// Whether a search should start now: never searched, settings changed, or
/// nothing was found and the back-off has passed. Never while one runs, and
/// never again once a port was found.
pub fn find_due(e: Option<&FindEntry>, fp: &str, now: Instant) -> bool {
    match e {
        None => true,
        Some(e) if e.fingerprint != fp => !e.scanning,
        Some(e) if e.scanning || e.port.is_some() => false,
        Some(e) => now.saturating_duration_since(e.at) >= FIND_BACKOFF,
    }
}

/// Whether an answer to an unauthenticated GetSystemDateAndTime shows an
/// ONVIF device: a proper answer, or a SOAP fault / login demand.
pub fn looks_like_onvif(answer: &Result<String, String>) -> bool {
    is_soap_answer(answer) || answer.as_ref().is_err_and(|e| e.contains("HTTP 401"))
}

/// Whether the answer can only be ONVIF: the time, or a SOAP fault. A bare 401
/// is not: any web server with a password says that.
fn is_soap_answer(answer: &Result<String, String>) -> bool {
    match answer {
        Ok(xml) => xml.contains("SystemDateAndTime"),
        Err(e) => e.contains(crate::onvif::SOAP_401) || (e.contains("(HTTP ") && !e.starts_with("HTTP ")),
    }
}

fn tcp_open(host: &str, port: u16, wait: Duration) -> bool {
    use std::net::ToSocketAddrs;
    (host, port).to_socket_addrs().ok().and_then(|mut a| a.next()).is_some_and(|a| std::net::TcpStream::connect_timeout(&a, wait).is_ok())
}

/// Try the common ONVIF ports on `host`: the first with a real SOAP answer wins.
/// A port that only says 401 may be a password-protected web page, so it counts
/// only when none does better (some cameras do want a login even for this).
pub fn probe_ports(host: &str, ports: &[u16]) -> Option<u16> {
    let mut only_401 = None;
    for &port in ports {
        if !tcp_open(host, port, Duration::from_millis(1200)) {
            continue;
        }
        let creds = Credentials { host: host.to_string(), port, username: String::new(), password: String::new() };
        let stop = AtomicBool::new(false);
        let mut c = Client::new(&creds, &stop);
        let url = format!("http://{host}:{port}/onvif/device_service");
        let ans = c
            .call(&url, "", "<tds:GetSystemDateAndTime/>", false, Duration::from_secs(4))
            .map_err(|e| format!("{e:#}"));
        if is_soap_answer(&ans) {
            return Some(port);
        }
        if looks_like_onvif(&ans) && only_401.is_none() {
            only_401 = Some(port);
        }
    }
    only_401
}

/// Mark a search as running if one is due (or `force`); false if not.
pub fn find_begin(id: usize, cfg: &CameraConfig, force: bool) -> bool {
    if !find_wanted(cfg) {
        return false;
    }
    let fp = find_fingerprint(cfg);
    let mut m = find_cache().lock().unwrap();
    let e = m.get(&id);
    let ok = if force { !e.is_some_and(|e| e.scanning) } else { find_due(e, &fp, Instant::now()) };
    if ok {
        m.insert(id, FindEntry { fingerprint: fp, port: None, at: Instant::now(), scanning: true });
    }
    ok
}

/// Run the search (after [`find_begin`] said yes) and remember the answer.
pub fn find_run(id: usize, cfg: &CameraConfig) -> Option<u16> {
    let fp = find_fingerprint(cfg);
    let host = crate::onvif::host_from_rtsp_url(&cfg.url).unwrap_or_default();
    let port = probe_ports(&host, &ONVIF_PORTS);
    find_cache().lock().unwrap().insert(id, FindEntry { fingerprint: fp, port, at: Instant::now(), scanning: false });
    port
}

/// The remembered search answer for these settings.
pub fn find_entry(id: usize, cfg: &CameraConfig) -> Option<FindEntry> {
    let fp = find_fingerprint(cfg);
    find_cache().lock().unwrap().get(&id).filter(|e| e.fingerprint == fp).cloned()
}

/// The camera's config with the port the search found filled in (if it has
/// none of its own). Used at runtime only; the file is never rewritten.
pub fn effective_cfg(id: usize, cfg: &CameraConfig) -> CameraConfig {
    let mut c = cfg.clone();
    if c.onvif_port.is_none() && find_wanted(&c) {
        c.onvif_port = find_entry(id, &c).and_then(|e| e.port);
    }
    c
}

// --- Tours: cycle through presets on a timer ---

fn tours() -> &'static Mutex<HashMap<String, Arc<AtomicBool>>> {
    static T: OnceLock<Mutex<HashMap<String, Arc<AtomicBool>>>> = OnceLock::new();
    T.get_or_init(Default::default)
}

/// Start (replacing any running) tour for `key`: visit each preset in turn,
/// staying `dwell` at each. Gives up after repeated failures.
pub fn start_tour(key: &str, target: PtzTarget, presets: Vec<String>, dwell: Duration) {
    stop_tour(key);
    let stop = Arc::new(AtomicBool::new(false));
    tours().lock().unwrap().insert(key.to_string(), stop.clone());
    let key = key.to_string();
    std::thread::Builder::new()
        .name("ptz-tour".into())
        .spawn(move || {
            let mut failures = 0;
            'outer: while !stop.load(Ordering::Relaxed) {
                for p in &presets {
                    if stop.load(Ordering::Relaxed) {
                        break 'outer;
                    }
                    match ptz_goto(&target, p) {
                        Ok(()) => failures = 0,
                        Err(e) => {
                            failures += 1;
                            tracing::warn!("ptz tour {key}: goto {p}: {e:#}");
                            if failures >= 5 {
                                break 'outer;
                            }
                        }
                    }
                    let until = Instant::now() + dwell;
                    while Instant::now() < until && !stop.load(Ordering::Relaxed) {
                        std::thread::sleep(Duration::from_millis(250));
                    }
                }
            }
            let mut t = tours().lock().unwrap();
            if t.get(&key).is_some_and(|s| Arc::ptr_eq(s, &stop)) {
                t.remove(&key);
            }
        })
        .expect("spawn ptz tour thread");
}

/// Stop the tour for `key`; true when one was running.
pub fn stop_tour(key: &str) -> bool {
    match tours().lock().unwrap().remove(key) {
        Some(s) => {
            s.store(true, Ordering::Relaxed);
            true
        }
        None => false,
    }
}

pub fn tour_running(key: &str) -> bool {
    tours().lock().unwrap().contains_key(key)
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn answers_are_trusted_only_for_their_sender() {
        let xml = |xaddr: &str, name: &str| format!(r#"<e:Envelope xmlns:e="http://www.w3.org/2003/05/soap-envelope" xmlns:d="http://schemas.xmlsoap.org/ws/2005/04/discovery" xmlns:a="http://schemas.xmlsoap.org/ws/2004/08/addressing"><e:Body><d:ProbeMatches><d:ProbeMatch><a:EndpointReference><a:Address>urn:uuid:1</a:Address></a:EndpointReference><d:Scopes>onvif://www.onvif.org/name/{name}</d:Scopes><d:XAddrs>{xaddr}</d:XAddrs></d:ProbeMatch></d:ProbeMatches></e:Body></e:Envelope>"#);
        let from: SocketAddr = "10.1.2.3:3702".parse().unwrap();
        let mut found = Vec::new();
        // Points somewhere else: dropped.
        take_answer(&mut found, xml("http://127.0.0.1:8090/x", "a").as_bytes(), from);
        take_answer(&mut found, xml("http://10.9.9.9/onvif/device_service", "a").as_bytes(), from);
        assert!(found.is_empty());
        // On the sender: kept; its name cleaned and cut.
        let long = format!("cam%0A%1Bevil{}", "x".repeat(200));
        take_answer(&mut found, xml("http://10.1.2.3/onvif/device_service http://10.9.9.9/x", &long).as_bytes(), from);
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].ip, "10.1.2.3");
        assert_eq!(found[0].xaddrs, vec!["http://10.1.2.3/onvif/device_service".to_string()]);
        assert!(found[0].name.starts_with("camevil") && found[0].name.chars().count() == 64);
    }

    #[test]
    fn several_subnets() {
        let h = parse_subnets("192.168.1.0/24, 10.0.20.0/24;10.1.2.7 192.168.1.9").unwrap();
        assert_eq!(h.len(), 254 + 254 + 1);
        assert_eq!(h[254], Ipv4Addr::new(10, 0, 20, 1));
        assert!(parse_subnets("").unwrap().is_empty());
        assert!(parse_subnets("192.168.1.0/24, bad/24").is_err());
        let many: Vec<String> = (0..17).map(|i| format!("10.0.{i}.0/24")).collect();
        assert!(parse_subnets(&many.join(",")).is_err());
        assert_eq!(split_ranges(" a, ,b;c "), vec!["a", "b", "c"]);
    }

    #[test]
    fn subnet_parsing() {
        let h = parse_subnet("192.168.1.0/24").unwrap();
        assert_eq!(h.len(), 254);
        assert_eq!(h[0], Ipv4Addr::new(192, 168, 1, 1));
        assert_eq!(*h.last().unwrap(), Ipv4Addr::new(192, 168, 1, 254));
        assert_eq!(parse_subnet("10.1.2.7").unwrap(), vec![Ipv4Addr::new(10, 1, 2, 7)]);
        assert_eq!(parse_subnet("10.0.0.5/22").unwrap().len(), 1022);
        assert!(parse_subnet("10.0.0.0/16").is_err());
        assert!(parse_subnet("nope/24").is_err());
    }

    #[test]
    fn hub_channels_pair_main_and_sub_of_the_same_camera() {
        let p = |name: &str, w: u32, h: u32, uri: &str| StreamProfile {
            name: name.into(),
            token: name.into(),
            width: w,
            height: h,
            encoding: "H264".into(),
            has_ptz: false,
            uri: Some(uri.into()),
            source: None,
        };
        // The hub seen on the LAN: two cameras, the first one larger.
        let profiles = vec![
            p("000", 5120, 1440, "rtsp://h:554/Preview_01_main"),
            p("001", 1536, 432, "rtsp://h:554/Preview_01_sub"),
            p("010", 2560, 1920, "rtsp://h:554/Preview_02_main"),
            p("011", 640, 480, "rtsp://h:554/Preview_02_sub"),
        ];
        assert_eq!(channels(&profiles), vec![(Some(0), Some(1)), (Some(2), Some(3))]);
        // With source tokens reported, those decide.
        let mut q = profiles.clone();
        for (i, x) in q.iter_mut().enumerate() {
            x.source = Some(if i < 2 { "A".into() } else { "B".into() });
            x.uri = Some(format!("rtsp://h/stream{i}"));
        }
        assert_eq!(channels(&q), vec![(Some(0), Some(1)), (Some(2), Some(3))]);
    }

    const PROBE_A: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<SOAP-ENV:Envelope xmlns:SOAP-ENV="http://www.w3.org/2003/05/soap-envelope" xmlns:wsa="http://schemas.xmlsoap.org/ws/2004/08/addressing" xmlns:d="http://schemas.xmlsoap.org/ws/2005/04/discovery" xmlns:dn="http://www.onvif.org/ver10/network/wsdl">
<SOAP-ENV:Header><wsa:MessageID>uuid:1</wsa:MessageID></SOAP-ENV:Header>
<SOAP-ENV:Body><d:ProbeMatches><d:ProbeMatch>
<wsa:EndpointReference><wsa:Address>urn:uuid:2419d68a-2dd2-21b2-a205-ec71dbaa1111</wsa:Address></wsa:EndpointReference>
<d:Types>dn:NetworkVideoTransmitter tds:Device</d:Types>
<d:Scopes>onvif://www.onvif.org/type/video_encoder onvif://www.onvif.org/name/Front%20Door onvif://www.onvif.org/hardware/DB-1 onvif://www.onvif.org/location/city/home</d:Scopes>
<d:XAddrs>http://192.168.1.22:8000/onvif/device_service</d:XAddrs>
<d:MetadataVersion>1</d:MetadataVersion>
</d:ProbeMatch></d:ProbeMatches></SOAP-ENV:Body></SOAP-ENV:Envelope>"#;

    const PROBE_B: &str = r#"<?xml version="1.0"?>
<s:Envelope xmlns:s="http://www.w3.org/2003/05/soap-envelope" xmlns:a="http://schemas.xmlsoap.org/ws/2004/08/addressing">
<s:Body><ProbeMatches xmlns="http://schemas.xmlsoap.org/ws/2005/04/discovery"><ProbeMatch>
<a:EndpointReference><a:Address>uuid:fc8a0a5e-0000-1000-8000-00408c123456</a:Address></a:EndpointReference>
<Types xmlns:dn="http://www.onvif.org/ver10/network/wsdl">dn:NetworkVideoTransmitter</Types>
<Scopes>onvif://www.onvif.org/location/country/china onvif://www.onvif.org/name/IPC-PTZ onvif://www.onvif.org/hardware/PTZ-9</Scopes>
<XAddrs>http://169.254.12.3/onvif/device_service http://192.168.1.50/onvif/device_service</XAddrs>
</ProbeMatch></ProbeMatches></s:Body></s:Envelope>"#;

    #[test]
    fn parses_probe_matches_two_vendors() {
        let a = parse_probe_matches(PROBE_A);
        assert_eq!(a.len(), 1);
        assert_eq!(a[0].name, "Front Door");
        assert_eq!(a[0].hardware, "DB-1");
        assert_eq!(a[0].location, "city/home");
        assert_eq!(a[0].ip, "192.168.1.22");
        assert!(a[0].endpoint.starts_with("urn:uuid:2419"));
        assert_eq!(a[0].types.len(), 2);
        let b = parse_probe_matches(PROBE_B);
        assert_eq!(b.len(), 1);
        assert_eq!(b[0].name, "IPC-PTZ");
        assert_eq!(b[0].xaddrs.len(), 2);
        assert_eq!(b[0].ip, "192.168.1.50", "link-local address must lose");
        assert!(parse_probe_matches("not xml").is_empty());
    }

    #[test]
    fn dedupes_by_endpoint() {
        let mut list = Vec::new();
        for d in parse_probe_matches(PROBE_A)
            .into_iter()
            .chain(parse_probe_matches(PROBE_A))
            .chain(parse_probe_matches(PROBE_B))
        {
            merge_device(&mut list, d);
        }
        assert_eq!(list.len(), 2);
        let mut extra = parse_probe_matches(PROBE_A).remove(0);
        extra.xaddrs = vec!["http://10.0.0.5:8000/onvif/device_service".into()];
        merge_device(&mut list, extra);
        assert_eq!(list.len(), 2);
        assert_eq!(list.iter().find(|d| d.name == "Front Door").unwrap().xaddrs.len(), 2);
    }

    #[test]
    fn probe_message_is_well_formed() {
        let m = probe_message("dn:NetworkVideoTransmitter");
        let doc = roxmltree::Document::parse(&m).unwrap();
        assert!(find_desc(&doc, "Probe").is_some());
        assert_eq!(find_text(doc.root_element(), "Types").unwrap(), "dn:NetworkVideoTransmitter");
    }

    const PROFILES: &str = r#"<env:Envelope xmlns:env="http://www.w3.org/2003/05/soap-envelope"><env:Body>
<trt:GetProfilesResponse xmlns:trt="http://www.onvif.org/ver10/media/wsdl" xmlns:tt="http://www.onvif.org/ver10/schema">
<trt:Profiles token="000" fixed="true"><tt:Name>mainStream</tt:Name>
<tt:VideoSourceConfiguration token="vs"><tt:Name>vs</tt:Name></tt:VideoSourceConfiguration>
<tt:VideoEncoderConfiguration token="ve0"><tt:Name>e0</tt:Name><tt:Encoding>H265</tt:Encoding><tt:Resolution><tt:Width>2560</tt:Width><tt:Height>1920</tt:Height></tt:Resolution></tt:VideoEncoderConfiguration>
<tt:PTZConfiguration token="ptz0"><tt:Name>PTZ</tt:Name></tt:PTZConfiguration></trt:Profiles>
<trt:Profiles token="001" fixed="true"><tt:Name>subStream</tt:Name>
<tt:VideoEncoderConfiguration token="ve1"><tt:Name>e1</tt:Name><tt:Encoding>H264</tt:Encoding><tt:Resolution><tt:Width>640</tt:Width><tt:Height>480</tt:Height></tt:Resolution></tt:VideoEncoderConfiguration>
<tt:PTZConfiguration token="ptz0"><tt:Name>PTZ</tt:Name></tt:PTZConfiguration></trt:Profiles>
</trt:GetProfilesResponse></env:Body></env:Envelope>"#;

    const PROFILES_NO_PTZ: &str = r#"<s:Envelope xmlns:s="http://www.w3.org/2003/05/soap-envelope"><s:Body>
<GetProfilesResponse xmlns="http://www.onvif.org/ver10/media/wsdl" xmlns:t="http://www.onvif.org/ver10/schema">
<Profiles token="Profile_1"><t:Name>Profile_1</t:Name><t:VideoEncoderConfiguration token="v"><t:Name>v</t:Name><t:Encoding>H264</t:Encoding><t:Resolution><t:Width>1920</t:Width><t:Height>1080</t:Height></t:Resolution></t:VideoEncoderConfiguration></Profiles>
</GetProfilesResponse></s:Body></s:Envelope>"#;

    #[test]
    fn parses_profiles_with_and_without_ptz() {
        let p = parse_profiles(PROFILES);
        assert_eq!(p.len(), 2);
        assert_eq!((p[0].token.as_str(), p[0].width, p[0].height, p[0].encoding.as_str()), ("000", 2560, 1920, "H265"));
        assert!(p[0].has_ptz && p[1].has_ptz);
        let q = parse_profiles(PROFILES_NO_PTZ);
        assert_eq!(q.len(), 1);
        assert!(!q[0].has_ptz);
        assert_eq!(q[0].name, "Profile_1");
    }

    #[test]
    fn parses_stream_uri_and_strips_credentials() {
        let xml = r#"<s:Envelope xmlns:s="http://www.w3.org/2003/05/soap-envelope"><s:Body><trt:GetStreamUriResponse xmlns:trt="http://www.onvif.org/ver10/media/wsdl" xmlns:tt="http://www.onvif.org/ver10/schema"><trt:MediaUri><tt:Uri>rtsp://admin:secret@10.1.1.9:554/Preview_01_main</tt:Uri><tt:InvalidAfterConnect>false</tt:InvalidAfterConnect></trt:MediaUri></trt:GetStreamUriResponse></s:Body></s:Envelope>"#;
        let u = parse_stream_uri(xml).unwrap();
        let clean = clean_stream_uri(&u, "192.168.1.22").unwrap();
        assert_eq!(clean, "rtsp://192.168.1.22:554/Preview_01_main");
        assert!(!clean.contains("secret"));
        assert_eq!(clean_stream_uri("rtsp://192.168.1.22/x", "192.168.1.22").as_deref(), Some("rtsp://192.168.1.22/x"));
        for ok in ["rtsps://h/x", "http://h/x", "https://h/x"] {
            assert!(clean_stream_uri(ok, "h").is_some(), "{ok}");
        }
        for bad in ["file:///etc/passwd", "concat:a|b", "ftp://h/x", "not a url", ""] {
            assert_eq!(clean_stream_uri(bad, "h"), None, "{bad}");
        }
    }

    fn prof(token: &str, w: u32, h: u32, enc: &str, uri: bool) -> StreamProfile {
        StreamProfile {
            name: token.into(),
            token: token.into(),
            width: w,
            height: h,
            encoding: enc.into(),
            has_ptz: false,
            uri: uri.then(|| format!("rtsp://h/{token}")),
            source: None,
        }
    }

    #[test]
    fn main_sub_selection() {
        let p = [
            prof("a", 640, 480, "H264", true),
            prof("b", 2560, 1440, "H265", true),
            prof("c", 1280, 720, "H264", true),
            prof("d", 3840, 2160, "H264", false),
        ];
        assert_eq!(select_main_sub(&p), (Some(1), Some(0)));
        assert_eq!(select_main_sub(&[prof("a", 1920, 1080, "H264", true)]), (Some(0), None));
        let same = [prof("a", 1920, 1080, "H264", true), prof("b", 1920, 1080, "H264", true)];
        assert_eq!(select_main_sub(&same), (Some(0), None));
        let j = [prof("j", 3840, 2160, "JPEG", true), prof("v", 1280, 720, "H264", true)];
        assert_eq!(select_main_sub(&j), (Some(1), None));
        assert_eq!(select_main_sub(&[prof("j", 640, 480, "JPEG", true)]), (Some(0), None));
        assert_eq!(select_main_sub(&[]), (None, None));
    }

    #[test]
    fn parses_device_info_and_services() {
        let info = r#"<s:Envelope xmlns:s="http://www.w3.org/2003/05/soap-envelope"><s:Body><tds:GetDeviceInformationResponse xmlns:tds="http://www.onvif.org/ver10/device/wsdl"><tds:Manufacturer>Acme</tds:Manufacturer><tds:Model>X1</tds:Model><tds:FirmwareVersion>v3.1</tds:FirmwareVersion><tds:SerialNumber>S123</tds:SerialNumber><tds:HardwareId>H1</tds:HardwareId></tds:GetDeviceInformationResponse></s:Body></s:Envelope>"#;
        let i = parse_device_information(info).unwrap();
        assert_eq!(
            (i.manufacturer.as_str(), i.model.as_str(), i.firmware.as_str(), i.serial.as_str()),
            ("Acme", "X1", "v3.1", "S123")
        );
        let caps = r#"<s:Envelope xmlns:s="http://www.w3.org/2003/05/soap-envelope"><s:Body><tds:GetCapabilitiesResponse xmlns:tds="http://www.onvif.org/ver10/device/wsdl" xmlns:tt="http://www.onvif.org/ver10/schema"><tds:Capabilities><tt:Media><tt:XAddr>http://1.2.3.4/onvif/media</tt:XAddr></tt:Media><tt:PTZ><tt:XAddr>http://1.2.3.4/onvif/ptz</tt:XAddr></tt:PTZ></tds:Capabilities></tds:GetCapabilitiesResponse></s:Body></s:Envelope>"#;
        assert_eq!(parse_service_xaddr(caps, "PTZ", "/ptz/wsdl").unwrap(), "http://1.2.3.4/onvif/ptz");
        let svcs = r#"<s:Envelope xmlns:s="http://www.w3.org/2003/05/soap-envelope"><s:Body><tds:GetServicesResponse xmlns:tds="http://www.onvif.org/ver10/device/wsdl"><tds:Service><tds:Namespace>http://www.onvif.org/ver10/media/wsdl</tds:Namespace><tds:XAddr>http://1.2.3.4/m</tds:XAddr></tds:Service></tds:GetServicesResponse></s:Body></s:Envelope>"#;
        assert_eq!(parse_service_xaddr(svcs, "Media", "/media/wsdl").unwrap(), "http://1.2.3.4/m");
        assert!(parse_service_xaddr(svcs, "PTZ", "/ptz/wsdl").is_none());
    }

    #[test]
    fn parses_presets() {
        let xml = r#"<s:Envelope xmlns:s="http://www.w3.org/2003/05/soap-envelope"><s:Body><tptz:GetPresetsResponse xmlns:tptz="http://www.onvif.org/ver20/ptz/wsdl" xmlns:tt="http://www.onvif.org/ver10/schema">
<tptz:Preset token="1"><tt:Name>Gate</tt:Name><tt:PTZPosition><tt:PanTilt x="0.1" y="0.2"/></tt:PTZPosition></tptz:Preset>
<tptz:Preset token="2"><tt:Name>Driveway</tt:Name></tptz:Preset>
<tptz:Preset token="3"/></tptz:GetPresetsResponse></s:Body></s:Envelope>"#;
        let p = parse_presets(xml);
        assert_eq!(
            p,
            vec![
                Preset { token: "1".into(), name: "Gate".into() },
                Preset { token: "2".into(), name: "Driveway".into() },
                Preset { token: "3".into(), name: "3".into() },
            ]
        );
    }

    fn wrapped(body: &str) -> String {
        format!("<s:Envelope xmlns:s=\"http://www.w3.org/2003/05/soap-envelope\"><s:Body>{body}</s:Body></s:Envelope>")
    }

    #[test]
    fn ptz_envelopes_are_well_formed() {
        let m = body_continuous_move("Profile<1>", 2.0, -0.5, f64::NAN, Some(Duration::from_millis(500)));
        let xml = wrapped(&m);
        let doc = roxmltree::Document::parse(&xml).unwrap();
        let cm = find_desc(&doc, "ContinuousMove").unwrap();
        assert_eq!(cm.tag_name().namespace(), Some(NS_PTZ));
        assert_eq!(find_text(cm, "ProfileToken").unwrap(), "Profile<1>");
        assert_eq!(find_text(cm, "Timeout").unwrap(), "PT0.5S");
        let pt = find_desc(&doc, "PanTilt").unwrap();
        assert_eq!(pt.tag_name().namespace(), Some(NS_SCHEMA));
        assert_eq!((pt.attribute("x"), pt.attribute("y")), (Some("1.000"), Some("-0.500")));
        assert_eq!(find_desc(&doc, "Zoom").unwrap().attribute("x"), Some("0.000"));
        let no_to = wrapped(&body_continuous_move("p", 0.0, 0.0, 0.0, None));
        assert!(find_desc(&roxmltree::Document::parse(&no_to).unwrap(), "Timeout").is_none());

        let g = wrapped(&body_goto_preset("p1", "7"));
        let doc = roxmltree::Document::parse(&g).unwrap();
        let n = find_desc(&doc, "GotoPreset").unwrap();
        assert_eq!(n.tag_name().namespace(), Some(NS_PTZ));
        assert_eq!(find_text(n, "PresetToken").unwrap(), "7");

        for body in [
            body_stop("p"),
            body_get_presets("p"),
            body_set_preset("p", "A & B"),
            body_remove_preset("p", "3"),
            body_get_status("p"),
            body_absolute_move("p", 0.1, 0.2, 0.3),
            body_get_stream_uri("p"),
            body_get_profiles(),
        ] {
            roxmltree::Document::parse(&wrapped(&body)).unwrap();
        }
        let s = wrapped(&body_set_preset("p", "A & B"));
        let doc = roxmltree::Document::parse(&s).unwrap();
        assert_eq!(find_text(doc.root_element(), "PresetName").unwrap(), "A & B");
    }

    #[test]
    fn target_uses_url_credentials_as_fallback() {
        let mut cfg: CameraConfig =
            toml::from_str("name = \"c\"\nurl = \"rtsp://u:p%40x@10.0.0.2:554/s\"\nonvif_port = 8000\n").unwrap();
        let t = target_from_config(&cfg).unwrap();
        assert_eq!((t.host.as_str(), t.port, t.username.as_str(), t.password.as_str()), ("10.0.0.2", 8000, "u", "p@x"));
        cfg.onvif_port = None;
        assert!(target_from_config(&cfg).is_err());
    }

    #[test]
    fn toml_block_has_no_real_password() {
        let v = serde_json::json!({"name":"Doorbell","url":"rtsp://h/main","sub_url":"rtsp://h/sub","onvif_port":8000,"username":"admin"});
        let t = toml_block(&v, "");
        assert!(t.contains("sub_url = \"rtsp://h/sub\"") && t.contains("onvif_port = 8000") && t.contains("CHANGE_ME"));
        toml::from_str::<toml::Value>(&t).unwrap();
    }

    const NODES_FULL: &str = r#"<s:Envelope xmlns:s="http://www.w3.org/2003/05/soap-envelope"><s:Body><tptz:GetNodesResponse xmlns:tptz="http://www.onvif.org/ver20/ptz/wsdl" xmlns:tt="http://www.onvif.org/ver10/schema"><tptz:PTZNode token="n1"><tt:Name>n</tt:Name><tt:SupportedPTZSpaces>
<tt:AbsolutePanTiltPositionSpace><tt:URI>http://www.onvif.org/ver10/tptz/PanTiltSpaces/PositionGenericSpace</tt:URI></tt:AbsolutePanTiltPositionSpace>
<tt:RelativePanTiltTranslationSpace><tt:URI>http://www.onvif.org/ver10/tptz/PanTiltSpaces/TranslationGenericSpace</tt:URI></tt:RelativePanTiltTranslationSpace>
<tt:RelativePanTiltTranslationSpace><tt:URI>http://www.onvif.org/ver10/tptz/PanTiltSpaces/TranslationSpaceFov</tt:URI><tt:XRange><tt:Min>-100</tt:Min><tt:Max>100</tt:Max></tt:XRange></tt:RelativePanTiltTranslationSpace>
<tt:RelativeZoomTranslationSpace><tt:URI>http://www.onvif.org/ver10/tptz/ZoomSpaces/TranslationGenericSpace</tt:URI></tt:RelativeZoomTranslationSpace>
<tt:ContinuousPanTiltVelocitySpace><tt:URI>http://www.onvif.org/ver10/tptz/PanTiltSpaces/VelocityGenericSpace</tt:URI></tt:ContinuousPanTiltVelocitySpace>
<tt:ContinuousZoomVelocitySpace><tt:URI>http://www.onvif.org/ver10/tptz/ZoomSpaces/VelocityGenericSpace</tt:URI></tt:ContinuousZoomVelocitySpace>
</tt:SupportedPTZSpaces></tptz:PTZNode></tptz:GetNodesResponse></s:Body></s:Envelope>"#;

    const NODES_PAN_ONLY: &str = r#"<s:Envelope xmlns:s="http://www.w3.org/2003/05/soap-envelope"><s:Body><tptz:GetNodesResponse xmlns:tptz="http://www.onvif.org/ver20/ptz/wsdl" xmlns:tt="http://www.onvif.org/ver10/schema"><tptz:PTZNode token="n1"><tt:SupportedPTZSpaces>
<tt:ContinuousPanTiltVelocitySpace><tt:URI>http://www.onvif.org/ver10/tptz/PanTiltSpaces/VelocityGenericSpace</tt:URI></tt:ContinuousPanTiltVelocitySpace>
</tt:SupportedPTZSpaces></tptz:PTZNode></tptz:GetNodesResponse></s:Body></s:Envelope>"#;

    #[test]
    fn nodes_describe_capabilities() {
        let c = parse_nodes(NODES_FULL);
        assert!(c.ptz && c.pan_tilt && c.zoom && c.aim_zoom);
        assert_eq!(c.aim, Some(AimSpace::Fov { range: 100.0 }));
        let p = parse_nodes(NODES_PAN_ONLY);
        assert!(p.ptz && p.pan_tilt && !p.zoom && p.aim.is_none() && !p.aim_zoom);
        assert_eq!(parse_nodes("<x/>"), PtzCaps::default());
        assert_eq!(parse_nodes("not xml"), PtzCaps::default());
    }

    #[test]
    fn aim_point_and_box_math() {
        let c = parse_nodes(NODES_FULL);
        // Center: no move. Right of center: pan right. Above center: tilt up (positive).
        let m = aim_move(&c, 0.5, 0.5, None).unwrap();
        assert_eq!((m.pan, m.tilt, m.zoom), (0.0, 0.0, None));
        let m = aim_move(&c, 1.0, 0.0, None).unwrap();
        assert_eq!((m.pan, m.tilt), (50.0, 50.0));
        assert_eq!(m.space, FOV_SPACE);
        // A small box zooms in more than a large one.
        let small = aim_move(&c, 0.25, 0.25, Some((0.1, 0.1))).unwrap().zoom.unwrap();
        let big = aim_move(&c, 0.25, 0.25, Some((0.6, 0.4))).unwrap().zoom.unwrap();
        assert!(small > big && big > 0.0 && small <= 0.5);
        // Whole picture: no zoom.
        assert_eq!(aim_move(&c, 0.5, 0.5, Some((1.0, 1.0))).unwrap().zoom, None);
        // Bad input and unsupported cameras.
        assert!(aim_move(&c, 1.5, 0.5, None).is_err());
        assert!(aim_move(&c, f64::NAN, 0.5, None).is_err());
        assert!(aim_move(&c, 0.5, 0.5, Some((0.0, 0.2))).is_err());
        assert_eq!(aim_move(&parse_nodes(NODES_PAN_ONLY), 0.5, 0.5, None), Err("aim not supported"));
        // Generic space without zoom support: pan/tilt only.
        let g = PtzCaps { ptz: true, pan_tilt: true, aim: Some(AimSpace::Generic), ..Default::default() };
        let m = aim_move(&g, 0.0, 1.0, Some((0.1, 0.1))).unwrap();
        assert_eq!(m.zoom, None);
        assert!(m.pan < 0.0 && m.tilt < 0.0 && m.space == GENERIC_SPACE);
    }

    #[test]
    fn relative_move_body() {
        let m = AimMove { pan: 12.5, tilt: -3.0, zoom: Some(0.25), space: FOV_SPACE };
        let b = body_relative_move("tok<1>", &m);
        assert!(b.contains("<tptz:RelativeMove "));
        assert!(b.contains("<tptz:ProfileToken>tok&lt;1&gt;</tptz:ProfileToken>"));
        assert!(b.contains("<tt:PanTilt x=\"12.500\" y=\"-3.000\" space=\"http://www.onvif.org/ver10/tptz/PanTiltSpaces/TranslationSpaceFov\"/>"));
        assert!(b.contains("<tt:Zoom x=\"0.250\"/>"));
        let b2 = body_relative_move("t", &AimMove { zoom: None, ..m });
        assert!(!b2.contains("tt:Zoom"));
        assert!(roxmltree::Document::parse(&format!("<r>{}</r>", b)).is_ok());
    }

    fn test_cfg(url: &str) -> CameraConfig {
        serde_json::from_value(serde_json::json!({ "name": "a", "url": url })).unwrap()
    }

    #[test]
    fn find_wanted_only_for_network_cameras_without_port() {
        let mut c = test_cfg("rtsp://u:p@10.0.0.5:554/stream0");
        assert!(find_wanted(&c));
        c.onvif_port = Some(80);
        assert!(!find_wanted(&c));
        c.onvif_port = None;
        c.reolink_channel = Some(0);
        assert!(!find_wanted(&c));
        c.reolink_channel = None;
        c.url = "lavfi://testsrc".into();
        assert!(!find_wanted(&c));
    }

    #[test]
    fn find_backoff_and_retry_rules() {
        let t0 = Instant::now();
        let mk = |fp: &str, port, scanning, at| FindEntry { fingerprint: fp.into(), port, at, scanning };
        assert!(find_due(None, "a", t0));
        // running: never start a second one
        assert!(!find_due(Some(&mk("a", None, true, t0)), "a", t0 + FIND_BACKOFF * 2));
        // found: done
        assert!(!find_due(Some(&mk("a", Some(80), false, t0)), "a", t0 + FIND_BACKOFF * 2));
        // nothing found: wait 6 h
        let none = mk("a", None, false, t0);
        assert!(!find_due(Some(&none), "a", t0 + Duration::from_secs(3600)));
        assert!(find_due(Some(&none), "a", t0 + FIND_BACKOFF));
        // settings changed: search again at once
        assert!(find_due(Some(&none), "b", t0));
        assert!(!find_due(Some(&mk("a", None, true, t0)), "b", t0));
    }

    #[test]
    fn port_order_is_common_first_and_unique() {
        assert_eq!(ONVIF_PORTS[0], 80);
        let mut v = ONVIF_PORTS.to_vec();
        v.sort();
        v.dedup();
        assert_eq!(v.len(), ONVIF_PORTS.len());
    }

    #[test]
    fn onvif_answers_are_told_from_other_web_servers() {
        assert!(looks_like_onvif(&Ok("<x><SystemDateAndTime/></x>".into())));
        assert!(looks_like_onvif(&Err("HTTP 401 unauthorized (check username/password)".into())));
        assert!(looks_like_onvif(&Err("Sender not authorized (HTTP 400)".into())));
        assert!(!looks_like_onvif(&Err("HTTP 404".into())));
        assert!(!looks_like_onvif(&Err("connect 10.0.0.5:80: refused".into())));
        assert!(!looks_like_onvif(&Ok("<html>router</html>".into())));
    }

    #[test]
    fn effective_cfg_uses_found_port_only_when_unset() {
        let mut c = test_cfg("rtsp://10.9.9.9:554/s");
        let id = 987_654;
        assert_eq!(effective_cfg(id, &c).onvif_port, None);
        find_cache().lock().unwrap().insert(id, FindEntry { fingerprint: find_fingerprint(&c), port: Some(8080), at: Instant::now(), scanning: false });
        assert_eq!(effective_cfg(id, &c).onvif_port, Some(8080));
        c.onvif_port = Some(2020);
        assert_eq!(effective_cfg(id, &c).onvif_port, Some(2020));
        c.onvif_port = None;
        c.username = "other".into();
        assert_eq!(effective_cfg(id, &c).onvif_port, None);
    }

    #[test]
    fn probe_finds_a_listening_onvif_port() {
        use std::io::{Read, Write};
        let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = l.local_addr().unwrap().port();
        std::thread::spawn(move || {
            for conn in l.incoming() {
                let Ok(mut s) = conn else { break };
                let mut b = [0u8; 4096];
                let _ = s.read(&mut b);
                let body = "<s:Envelope xmlns:s=\"http://www.w3.org/2003/05/soap-envelope\"><s:Body><tds:GetSystemDateAndTimeResponse xmlns:tds=\"x\"><tds:SystemDateAndTime/></tds:GetSystemDateAndTimeResponse></s:Body></s:Envelope>";
                let _ = write!(s, "HTTP/1.1 200 OK\r\nContent-Type: application/soap+xml\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}", body.len(), body);
            }
        });
        assert_eq!(probe_ports("127.0.0.1", &[1, port]), Some(port));
        assert_eq!(probe_ports("127.0.0.1", &[1]), None);
    }

    /// A local port that answers every request with this.
    fn answering(status: &str, body: &str, extra: &str) -> u16 {
        use std::io::{Read, Write};
        let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = l.local_addr().unwrap().port();
        let reply = format!("HTTP/1.1 {status}\r\n{extra}Content-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len());
        std::thread::spawn(move || {
            for conn in l.incoming() {
                let Ok(mut s) = conn else { break };
                let mut b = [0u8; 4096];
                let _ = s.read(&mut b);
                let _ = s.write_all(reply.as_bytes());
            }
        });
        port
    }

    #[test]
    fn a_web_page_behind_a_password_does_not_beat_real_onvif() {
        let time = "<s:Envelope xmlns:s=\"http://www.w3.org/2003/05/soap-envelope\"><s:Body><tds:GetSystemDateAndTimeResponse xmlns:tds=\"x\"><tds:SystemDateAndTime/></tds:GetSystemDateAndTimeResponse></s:Body></s:Envelope>";
        let fault = "<s:Envelope xmlns:s=\"http://www.w3.org/2003/05/soap-envelope\"><s:Body><s:Fault><s:Code><s:Value>s:Sender</s:Value></s:Code><s:Reason><s:Text>Sender not authorized</s:Text></s:Reason></s:Fault></s:Body></s:Envelope>";
        let web = answering("401 Unauthorized", "<html>sign in</html>", "");
        let web2 = answering("401 Unauthorized", "<html>sign in</html>", "");
        let onvif = answering("200 OK", time, "Content-Type: application/soap+xml\r\n");
        let onvif_401 = answering("401 Unauthorized", fault, "Content-Type: application/soap+xml\r\n");
        let onvif_400 = answering("400 Bad Request", fault, "Content-Type: application/soap+xml\r\n");
        let closed = 1;
        assert_eq!(probe_ports("127.0.0.1", &[web, onvif]), Some(onvif), "the real answer wins, whatever the order");
        assert_eq!(probe_ports("127.0.0.1", &[onvif, web]), Some(onvif));
        assert_eq!(probe_ports("127.0.0.1", &[web, onvif_400]), Some(onvif_400), "a SOAP fault is a real answer");
        assert_eq!(probe_ports("127.0.0.1", &[web, onvif_401]), Some(onvif_401), "so is a 401 with a SOAP fault");
        assert_eq!(probe_ports("127.0.0.1", &[closed, web, web2]), Some(web), "only 401s: the first, as always");
        assert_eq!(probe_ports("127.0.0.1", &[closed]), None);
    }
}
