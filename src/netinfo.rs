//! A camera's network facts for the camera settings page: address, MAC,
//! ports, stream path (never with a sign-in), reachability and, when the
//! camera speaks ONVIF, who made it. Admin only; nothing here holds a password.

use std::collections::HashMap;
use std::net::{IpAddr, TcpStream, ToSocketAddrs};
use std::sync::atomic::AtomicBool;
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

use serde_json::{json, Value};

use crate::config::CameraConfig;
use crate::onvif::{Client, Credentials};
use crate::onvif_ptz::DeviceInfo;

const CACHE_TTL: Duration = Duration::from_secs(300);
const ONVIF_TIMEOUT: Duration = Duration::from_secs(5);

/// Cached maker/model answers and MACs, keyed by camera + connection settings.
type CacheMap = HashMap<String, (Instant, Option<DeviceInfo>, Option<String>)>;
fn cache() -> &'static Mutex<CacheMap> {
    static C: OnceLock<Mutex<CacheMap>> = OnceLock::new();
    C.get_or_init(Default::default)
}

/// Host and port of a stream URL, with the scheme's usual port when none is written.
pub fn host_port(url: &str) -> Option<(String, u16)> {
    let u = url::Url::parse(url).ok()?;
    let host = u.host_str()?.trim_matches(|c| c == '[' || c == ']').to_string();
    let port = u.port().or(match u.scheme() {
        "rtsp" | "rtsps" => Some(554),
        "http" => Some(80),
        "https" => Some(443),
        "rtmp" => Some(1935),
        _ => None,
    })?;
    Some((host, port))
}

/// The URL's path and query, without any sign-in.
pub fn stream_path(url: &str) -> String {
    let clean = crate::engine::redact_url(url.to_string());
    match url::Url::parse(&clean) {
        Ok(c) => {
            let mut p = c.path().to_string();
            if let Some(q) = c.query() {
                p.push('?');
                p.push_str(q);
            }
            p
        }
        Err(_) => String::new(),
    }
}

fn norm_mac(s: &str) -> Option<String> {
    let t = s.trim().replace('-', ":").to_ascii_lowercase();
    let parts: Vec<&str> = t.split(':').collect();
    if parts.len() != 6 || parts.iter().any(|p| p.is_empty() || p.len() > 2 || !p.chars().all(|c| c.is_ascii_hexdigit())) {
        return None;
    }
    let m = parts.iter().map(|p| format!("{p:0>2}")).collect::<Vec<_>>().join(":");
    if m == "00:00:00:00:00:00" || m == "ff:ff:ff:ff:ff:ff" {
        return None;
    }
    Some(m)
}

/// MAC for `ip` from Linux `/proc/net/arp` text.
#[cfg(any(target_os = "linux", test))]
pub fn mac_from_proc_arp(text: &str, ip: &str) -> Option<String> {
    text.lines().skip(1).find_map(|l| {
        let f: Vec<&str> = l.split_whitespace().collect();
        // IP address, HW type, Flags, HW address, Mask, Device
        if f.len() >= 4 && f[0] == ip && f[2] != "0x0" { norm_mac(f[3]) } else { None }
    })
}

/// MAC for `ip` from `ip neigh` output (`10.0.0.5 dev eth0 lladdr aa:bb:.. REACHABLE`).
#[cfg(any(target_os = "linux", test))]
pub fn mac_from_ip_neigh(text: &str, ip: &str) -> Option<String> {
    text.lines().find_map(|l| {
        let f: Vec<&str> = l.split_whitespace().collect();
        if f.first() != Some(&ip) {
            return None;
        }
        let i = f.iter().position(|w| *w == "lladdr")?;
        f.get(i + 1).and_then(|m| norm_mac(m))
    })
}

/// MAC for `ip` from Windows / macOS `arp -a` output
/// (`  10.0.0.5   aa-bb-cc-dd-ee-ff   dynamic` or `? (10.0.0.5) at aa:bb:cc:dd:ee:ff on en0`).
#[cfg(any(not(target_os = "linux"), test))]
pub fn mac_from_arp_a(text: &str, ip: &str) -> Option<String> {
    let paren = format!("({ip})");
    text.lines().find_map(|l| {
        let f: Vec<&str> = l.split_whitespace().collect();
        if f.first() == Some(&ip) {
            return f.get(1).and_then(|m| norm_mac(m));
        }
        if f.iter().any(|w| *w == paren) {
            let i = f.iter().position(|w| *w == "at")?;
            return f.get(i + 1).and_then(|m| norm_mac(m));
        }
        None
    })
}

#[allow(dead_code)]
fn run_text(prog: &str, args: &[&str]) -> Option<String> {
    let mut c = std::process::Command::new(prog);
    c.args(args).stdin(std::process::Stdio::null()).stderr(std::process::Stdio::null());
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        c.creation_flags(0x0800_0000); // CREATE_NO_WINDOW
    }
    let out = c.output().ok()?;
    Some(String::from_utf8_lossy(&out.stdout).into_owned())
}

/// The camera's MAC from this machine's neighbour table (only known on the same LAN
/// and after something has talked to it).
pub fn lookup_mac(ip: &IpAddr) -> Option<String> {
    let ip = ip.to_string();
    #[cfg(target_os = "linux")]
    {
        if let Ok(t) = std::fs::read_to_string("/proc/net/arp") {
            if let Some(m) = mac_from_proc_arp(&t, &ip) {
                return Some(m);
            }
        }
        run_text("ip", &["neigh", "show", &ip]).and_then(|t| mac_from_ip_neigh(&t, &ip))
    }
    #[cfg(not(target_os = "linux"))]
    {
        run_text("arp", &["-a"]).and_then(|t| mac_from_arp_a(&t, &ip))
    }
}

fn resolve(host: &str, port: u16) -> Option<IpAddr> {
    let mut all: Vec<IpAddr> = (host, port).to_socket_addrs().ok()?.map(|a| a.ip()).collect();
    all.sort_by_key(|a| a.is_ipv6());
    all.into_iter().next()
}

/// `(reachable, milliseconds, error)` for a TCP connect to the stream port.
fn probe(ip: &IpAddr, port: u16) -> (bool, Option<u64>, Option<String>) {
    let t = Instant::now();
    match TcpStream::connect_timeout(&std::net::SocketAddr::new(*ip, port), Duration::from_secs(3)) {
        Ok(_) => (true, Some(t.elapsed().as_millis() as u64), None),
        Err(e) => (false, None, Some(e.kind().to_string())),
    }
}

fn onvif_info(cfg: &CameraConfig) -> Option<DeviceInfo> {
    let t = crate::onvif_ptz::target_from_config(cfg).ok()?;
    crate::onvif_ptz::run_with_timeout(ONVIF_TIMEOUT + Duration::from_secs(1), move || {
        let creds = Credentials { host: t.host.clone(), port: t.port, username: t.username.clone(), password: t.password.clone() };
        let stop = AtomicBool::new(false);
        let mut c = Client::new(&creds, &stop);
        c.sync_clock();
        let dev = format!("http://{}:{}/onvif/device_service", t.host, t.port);
        let xml = c.call(&dev, "", "<tds:GetDeviceInformation/>", true, ONVIF_TIMEOUT).ok()?;
        crate::onvif_ptz::parse_device_information(&xml)
    })
    .flatten()
}

fn nonblank(s: &str) -> Value {
    if s.trim().is_empty() { Value::Null } else { json!(s.trim()) }
}

/// The page's answer. Blocking (connect, ARP, ONVIF): call from `spawn_blocking`.
pub fn collect(id: usize, cfg: &CameraConfig, refresh: bool, ingest: &str, online: bool) -> Value {
    let Some((host, port)) = host_port(&cfg.url) else {
        return json!({ "id": id, "host": null, "ingest": ingest, "online": online, "note": "This camera is not on the network." });
    };
    let ip = resolve(&host, port);
    let (reachable, connect_ms, err) = match &ip {
        Some(ip) => probe(ip, port),
        None => (false, None, Some("name not found".to_string())),
    };
    let key = format!("{id}:{host}:{:?}:{}", cfg.onvif_port, cfg.username);
    let cached = if refresh { None } else { cache().lock().unwrap().get(&key).filter(|c| c.0.elapsed() < CACHE_TTL).cloned() };
    let (info, mac) = match cached {
        Some((_, i, m)) => (i, m),
        None => {
            let mac = ip.as_ref().and_then(lookup_mac);
            let info = if cfg.onvif_port.is_some() { onvif_info(cfg) } else { None };
            cache().lock().unwrap().insert(key, (Instant::now(), info.clone(), mac.clone()));
            (info, mac)
        }
    };
    let sub = cfg.sub_url.as_deref().filter(|s| !s.is_empty()).map(|s| json!({ "port": host_port(s).map(|h| h.1), "path": stream_path(s) }));
    let i = info.as_ref();
    json!({
        "id": id,
        "host": host,
        "ip": ip.map(|a| a.to_string()),
        "mac": mac,
        "stream_port": port,
        "onvif_port": cfg.onvif_port,
        "scheme": url::Url::parse(&cfg.url).ok().map(|u| u.scheme().to_string()),
        "stream_path": stream_path(&cfg.url),
        "sub": sub,
        "ingest": ingest,
        "online": online,
        "fps_setting": cfg.fps,
        "reachable": reachable,
        "connect_ms": connect_ms,
        "error": err,
        "maker": i.map(|d| nonblank(&d.manufacturer)).unwrap_or(Value::Null),
        "model": i.map(|d| nonblank(&d.model)).unwrap_or(Value::Null),
        "firmware": i.map(|d| nonblank(&d.firmware)).unwrap_or(Value::Null),
        "serial": i.map(|d| nonblank(&d.serial)).unwrap_or(Value::Null),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn proc_arp_sample() {
        let t = "IP address       HW type     Flags       HW address            Mask     Device\n\
192.168.1.20     0x1         0x2         AA:BB:CC:00:11:22     *        eth0\n\
192.168.1.21     0x1         0x0         00:00:00:00:00:00     *        eth0\n";
        assert_eq!(mac_from_proc_arp(t, "192.168.1.20").as_deref(), Some("aa:bb:cc:00:11:22"));
        assert_eq!(mac_from_proc_arp(t, "192.168.1.21"), None);
        assert_eq!(mac_from_proc_arp(t, "192.168.1.22"), None);
    }

    #[test]
    fn ip_neigh_sample() {
        let t = "192.168.1.20 dev eth0 lladdr aa:bb:cc:00:11:22 REACHABLE\n192.168.1.30 dev eth0  FAILED\n";
        assert_eq!(mac_from_ip_neigh(t, "192.168.1.20").as_deref(), Some("aa:bb:cc:00:11:22"));
        assert_eq!(mac_from_ip_neigh(t, "192.168.1.30"), None);
    }

    #[test]
    fn arp_a_samples() {
        let win = "Interface: 192.168.1.5 --- 0x7\n  Internet Address      Physical Address      Type\n  192.168.1.20          aa-bb-cc-00-11-22     dynamic\n  224.0.0.22            01-00-5e-00-00-16     static\n";
        assert_eq!(mac_from_arp_a(win, "192.168.1.20").as_deref(), Some("aa:bb:cc:00:11:22"));
        let mac = "? (192.168.1.20) at a:bb:cc:0:11:22 on en0 ifscope [ethernet]\n? (192.168.1.31) at (incomplete) on en0\n";
        assert_eq!(mac_from_arp_a(mac, "192.168.1.20").as_deref(), Some("0a:bb:cc:00:11:22"));
        assert_eq!(mac_from_arp_a(mac, "192.168.1.31"), None);
    }

    #[test]
    fn ports_and_path_hide_sign_in() {
        assert_eq!(host_port("rtsp://u:p@10.0.0.5/live/ch0"), Some(("10.0.0.5".into(), 554)));
        assert_eq!(host_port("rtsp://10.0.0.5:8554/x"), Some(("10.0.0.5".into(), 8554)));
        assert_eq!(host_port("lavfi://mandelbrot=size=1x1"), None);
        let p = stream_path("rtsp://user:secret@10.0.0.5:554/live/ch0?password=abc&x=1");
        assert!(!p.contains("secret") && !p.contains("abc") && !p.contains("user"), "{p}");
        assert!(p.starts_with("/live/ch0"));
    }
}
