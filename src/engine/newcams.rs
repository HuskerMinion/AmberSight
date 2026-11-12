//! The hourly new-camera check ([discovery] watch = true): this server's own
//! subnet (ONVIF multicast) and the listed other subnets (an ONVIF probe to
//! each address, then the RTSP port) are checked once an hour. Devices that
//! are not a camera here yet and were not ignored are listed in the health
//! report (the header chip, Find cameras) and announced once through the
//! notification services.

use super::*;
use std::sync::atomic::AtomicBool;
use std::sync::Mutex;
use std::time::Duration;

const EVERY_SECS: i64 = 3600;
/// How long each check waits for ONVIF answers.
const WAIT: Duration = Duration::from_secs(4);
const MAX_TOLD: usize = 1000;
/// Devices named in one notification (the rest are counted).
const MAX_NAMED: usize = 10;

#[derive(Debug, Clone, serde::Serialize, PartialEq)]
pub struct Found {
    pub ip: String,
    /// "onvif", or "rtsp" (answered on port 554 only).
    pub kind: &'static str,
    pub name: String,
    pub first_seen: i64,
}

struct Watch {
    found: Vec<Found>,
    last_check: i64,
    /// Announced already since this start, so each goes out once. The first
    /// check after a start only fills it (what was there before is not news).
    told: Vec<String>,
    primed: bool,
    /// Cameras set up by host name: the addresses those names had at the last check.
    named: Vec<String>,
}

static STATE: Mutex<Watch> = Mutex::new(Watch { found: Vec::new(), last_check: 0, told: Vec::new(), primed: false, named: Vec::new() });
static NOW: AtomicBool = AtomicBool::new(false);

/// Check soon (within a minute), e.g. right after the check is turned on.
pub fn request() { NOW.store(true, Ordering::Relaxed); }

/// Host of a camera URL ("rtsp://user:pw@192.168.1.5:554/x" -> "192.168.1.5").
pub fn host_of(url: &str) -> String {
    let rest = url.split_once("://").map(|(_, r)| r).unwrap_or(url);
    let rest = rest.split(['/', '?', '#']).next().unwrap_or("");
    let rest = rest.rsplit_once('@').map(|(_, h)| h).unwrap_or(rest);
    if let Some(v6) = rest.strip_prefix('[') {
        return v6.split(']').next().unwrap_or("").to_string();
    }
    rest.split(':').next().unwrap_or("").to_string()
}

/// The hosts of every camera's main and sub stream.
fn camera_hosts(engine: &Engine) -> Vec<String> {
    let c = engine.config.lock().unwrap();
    c.cameras.iter().flat_map(|k| [host_of(&k.url), k.sub_url.as_deref().map(host_of).unwrap_or_default()]).filter(|h| !h.is_empty()).collect()
}

/// The addresses of the cameras set up by host name ("cam.lan"). Blocking
/// (name lookups); done once per check.
fn resolve_names(hosts: &[String]) -> Vec<String> {
    use std::net::ToSocketAddrs;
    hosts
        .iter()
        .filter(|h| h.parse::<std::net::IpAddr>().is_err())
        .flat_map(|h| (h.as_str(), 554).to_socket_addrs().map(|a| a.map(|s| s.ip().to_string()).collect::<Vec<_>>()).unwrap_or_default())
        .collect()
}

/// What the last check found that is still news: not a camera here, not
/// ignored. Nothing while the check is off.
pub fn current(engine: &Engine) -> (Vec<Found>, i64) {
    let (watch, ignored) = {
        let c = engine.config.lock().unwrap();
        (c.discovery.watch, c.discovery.ignored.clone())
    };
    let taken = camera_hosts(engine);
    let st = STATE.lock().unwrap();
    if !watch {
        return (Vec::new(), st.last_check);
    }
    let list = st.found.iter().filter(|f| !taken.contains(&f.ip) && !st.named.contains(&f.ip) && !ignored.contains(&f.ip)).cloned().collect();
    (list, st.last_check)
}

pub fn watch_loop(engine: Arc<Engine>) {
    thread::sleep(Duration::from_secs(90));
    loop {
        let d = engine.config.lock().unwrap().discovery.clone();
        let due = Utc::now().timestamp() - STATE.lock().unwrap().last_check >= EVERY_SECS;
        if d.watch && (due || NOW.swap(false, Ordering::Relaxed)) {
            check(&engine, &d.ranges);
        }
        thread::sleep(Duration::from_secs(60));
    }
}

fn check(engine: &Arc<Engine>, ranges: &[String]) {
    let hosts = crate::onvif_ptz::parse_subnets(&ranges.join(",")).unwrap_or_default();
    let mut devs = crate::onvif_ptz::discover(WAIT);
    let (more, rtsp) = if hosts.is_empty() { (Vec::new(), Vec::new()) } else { crate::onvif_ptz::discover_hosts(&hosts, WAIT) };
    for m in more {
        if !devs.iter().any(|x| x.ip == m.ip) {
            devs.push(m);
        }
    }
    let now = Utc::now().timestamp();
    let named = resolve_names(&camera_hosts(engine));
    {
        let mut st = STATE.lock().unwrap();
        st.named = named;
        let since = |ip: &str| st.found.iter().find(|f| f.ip == ip).map(|f| f.first_seen).unwrap_or(now);
        let mut list: Vec<Found> = devs
            .iter()
            .map(|d| {
                let name = [d.name.as_str(), d.hardware.as_str()].iter().filter(|s| !s.is_empty()).copied().collect::<Vec<_>>();
                let name = if name.len() == 2 && name[0] == name[1] { name[0].to_string() } else { name.join(" ") };
                Found { ip: d.ip.clone(), kind: "onvif", name, first_seen: since(&d.ip) }
            })
            .collect();
        for r in rtsp {
            if !list.iter().any(|f| f.ip == r.ip) {
                list.push(Found { first_seen: since(&r.ip), ip: r.ip, kind: "rtsp", name: String::new() });
            }
        }
        st.found = list;
        st.last_check = now;
    }
    let (news, _) = current(engine);
    let fresh: Vec<Found> = {
        let mut st = STATE.lock().unwrap();
        let fresh: Vec<Found> = news.into_iter().filter(|f| !st.told.contains(&f.ip)).collect();
        st.told.extend(fresh.iter().map(|f| f.ip.clone()));
        // (Bounded: the oldest are forgotten first, and may be announced again.)
        let over = st.told.len().saturating_sub(MAX_TOLD);
        st.told.drain(..over);
        let primed = std::mem::replace(&mut st.primed, true);
        if primed { fresh } else { Vec::new() }
    };
    if fresh.is_empty() {
        return;
    }
    // (Names come from the devices: discovery has cleaned and shortened them.)
    let mut names: Vec<String> = fresh.iter().take(MAX_NAMED).map(|f| if f.name.is_empty() { format!("{} (RTSP)", f.ip) } else { format!("{} ({})", f.ip, f.name) }).collect();
    if fresh.len() > MAX_NAMED {
        names.push(format!("and {} more", fresh.len() - MAX_NAMED));
    }
    info!("new camera check: found {}", names.join(", "));
    let alerts = engine.config.lock().unwrap().alerts.clone();
    if !alerts.system_alerts_on() {
        return;
    }
    let site = crate::links::own_name(engine);
    let title = if fresh.len() == 1 { "New camera found".to_string() } else { format!("{} new cameras found", fresh.len()) };
    let body = format!("{site}: {} on the network, not added yet: {}. Add or ignore it under Settings, Cameras, Find cameras.", if fresh.len() == 1 { "a camera" } else { "cameras" }, names.join(", "));
    thread::spawn(move || {
        super::system_alerts::send(&alerts, &title, &body, false);
    });
}

#[cfg(test)]
mod tests {
    use super::host_of;

    #[test]
    fn hosts_of_camera_urls() {
        assert_eq!(host_of("rtsp://admin:p@ss@10.0.0.22:554/Preview_01_main"), "10.0.0.22");
        assert_eq!(host_of("https://10.0.0.22/flv?port=1935"), "10.0.0.22");
        assert_eq!(host_of("rtsp://cam.local/x"), "cam.local");
        assert_eq!(host_of("rtsp://[fe80::1]:554/x"), "fe80::1");
        assert_eq!(host_of("push:abc"), "push");
    }
}
