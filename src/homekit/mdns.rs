//! The `_hap._tcp` mDNS (Bonjour) advertisement Apple devices find the
//! accessory by, with the TXT record HAP defines.
//!
//! mdns-sd answers on UDP 5353 itself, with the port shared
//! (SO_REUSEADDR / SO_REUSEPORT), so it runs next to the system's own mDNS
//! responder (Avahi on Linux, mDNSResponder on macOS, the Windows DNS
//! client). The record names its own host name, "AmberSight-XXXXXX.local",
//! so it never claims the computer's name the system already announces.
//! It is announced only on the interfaces and address families the HomeKit
//! port listens on, never on loopback or virtual interfaces (Docker, VMs,
//! VPNs such as Tailscale or ZeroTier), and its name ends in a short part
//! of the accessory id so two bridges on one network never clash.

use anyhow::{anyhow, Result};
use base64::Engine as _;
use mdns_sd::{IfKind, IfPredicate, ServiceDaemon, ServiceInfo};
use sha2::{Digest, Sha512};

use super::server::Listen;

/// Interfaces that are not the home network (by name).
pub fn virtual_interface(name: &str) -> bool {
    let n = name.to_ascii_lowercase();
    let lo = n.strip_prefix("lo").is_some_and(|rest| rest.chars().all(|c| c.is_ascii_digit()));
    lo || ["docker", "veth", "br-", "virbr", "tailscale", "utun", "zt", "vethernet", "vmnet", "vboxnet", "cni", "flannel", "podman", "wg"].iter().any(|p| n.starts_with(p))
}

/// Whether to announce on an interface with this name and address.
pub fn announce_on(name: &str, ip: std::net::IpAddr, loopback: bool, listen: Listen) -> bool {
    if loopback || ip.is_loopback() || virtual_interface(name) {
        return false;
    }
    // Live video to a link-local controller needs its interface in ffmpeg's
    // target, tested on Linux only: elsewhere, controllers are steered to
    // IPv4 or a routable IPv6 address by not announcing link-local ones.
    if !cfg!(target_os = "linux") && super::stream::link_local(ip) {
        return false;
    }
    match listen {
        Listen::Dual => true,
        Listen::V4 => ip.is_ipv4(),
        Listen::Addr(a) => a == ip,
    }
}

/// The instance name: the bridge's name and the last 4 hex digits of the
/// accessory id ("Home NVR 3F2A"), at most 63 bytes.
pub fn instance_name(name: &str, device_id: &str) -> String {
    let hexd: String = device_id.chars().filter(|c| c.is_ascii_hexdigit()).collect();
    let tail = &hexd[hexd.len().saturating_sub(4)..];
    let mut base = String::new();
    for c in name.chars() {
        if base.len() + c.len_utf8() + 1 + tail.len() > 63 {
            break;
        }
        base.push(c);
    }
    format!("{} {}", base.trim_end(), tail.to_ascii_uppercase())
}

const SERVICE: &str = "_hap._tcp.local.";

/// The setup hash `sh`: the first 4 bytes of SHA-512(setup id + device id), base64.
pub fn setup_hash(setup_id: &str, device_id: &str) -> String {
    let d = Sha512::digest(format!("{setup_id}{device_id}").as_bytes());
    base64::engine::general_purpose::STANDARD.encode(&d[..4])
}

/// The X-HM:// setup payload (what the QR code holds): category, "IP
/// transport" flag and the setup code, base 36, then the setup id.
pub fn setup_uri(code: &str, setup_id: &str, category: u64) -> String {
    let digits: u64 = code.replace('-', "").parse().unwrap_or(0);
    let value = (category << 31) | (1 << 28) | (digits & ((1 << 27) - 1));
    let mut s = String::new();
    let mut v = value;
    while v > 0 {
        let d = (v % 36) as u32;
        s.insert(0, char::from_digit(d, 36).unwrap_or('0').to_ascii_uppercase());
        v /= 36;
    }
    format!("X-HM://{s:0>9}{setup_id}")
}

/// What goes in the TXT record.
pub struct Txt {
    pub name: String,
    pub device_id: String,
    pub config_number: u32,
    pub pairable: bool,
    pub setup_id: String,
    /// Accessory category (2 = bridge).
    pub category: u8,
}

impl Txt {
    pub fn pairs(&self) -> Vec<(String, String)> {
        vec![
            ("c#".into(), self.config_number.to_string()),
            ("ff".into(), "0".into()),
            ("id".into(), self.device_id.clone()),
            ("md".into(), self.name.clone()),
            ("pv".into(), "1.1".into()),
            ("s#".into(), "1".into()),
            ("sf".into(), if self.pairable { "1" } else { "0" }.into()),
            ("ci".into(), self.category.to_string()),
            ("sh".into(), setup_hash(&self.setup_id, &self.device_id)),
        ]
    }
}

/// The service record. Not probed: its instance name and host name carry
/// the accessory's random id, so nothing else on the network has them, and
/// probing (three queries, and again whenever the record changed) delayed
/// the first answer by many seconds on Windows. mdns-sd announces it at
/// once, and a second time about a second later (RFC 6762 section 8.3).
fn record(txt: &Txt, host: &str, port: u16) -> Result<ServiceInfo> {
    let props: Vec<(String, String)> = txt.pairs();
    let refs: Vec<(&str, &str)> = props.iter().map(|(k, v)| (k.as_str(), v.as_str())).collect();
    let mut info = ServiceInfo::new(SERVICE, &instance_name(&txt.name, &txt.device_id), host, "", port, &refs[..]).map_err(|e| anyhow!("mDNS record: {e}"))?.enable_addr_auto();
    info.set_requires_probe(false);
    Ok(info)
}

pub struct Advert {
    daemon: ServiceDaemon,
    fullname: String,
    host: String,
    port: u16,
}

/// The host name the record points at.
fn host_for(device_id: &str) -> String {
    let tail: String = device_id.chars().filter(|c| c.is_ascii_hexdigit()).collect();
    // (The whole id: two servers never claim the same host name.)
    format!("AmberSight-{tail}.local.")
}

/// One mDNS responder for every accessory announced (several standalone
/// cameras share it), stopped when the last one goes.
static DAEMON: std::sync::Mutex<Option<(ServiceDaemon, usize)>> = std::sync::Mutex::new(None);

fn acquire(listen: Listen) -> Result<ServiceDaemon> {
    let mut d = DAEMON.lock().unwrap();
    if let Some((daemon, n)) = d.as_mut() {
        *n += 1;
        return Ok(daemon.clone());
    }
    let daemon = ServiceDaemon::new().map_err(|e| anyhow!("mDNS could not start: {e}"))?;
    // (Interfaces that come and go later are judged the same way.)
    let _ = daemon.disable_interface(vec![IfKind::LoopbackV4, IfKind::LoopbackV6, IfKind::Predicate(IfPredicate::new(move |i| !announce_on(&i.name, i.ip(), i.is_loopback(), listen)))]);
    *d = Some((daemon.clone(), 1));
    Ok(daemon)
}

fn release() {
    let mut d = DAEMON.lock().unwrap();
    let last = match d.as_mut() {
        Some((_, n)) => {
            *n = n.saturating_sub(1);
            *n == 0
        }
        None => false,
    };
    if last {
        if let Some((daemon, _)) = d.take() {
            let _ = daemon.shutdown();
        }
    }
}

impl Advert {
    pub fn start(port: u16, listen: Listen, txt: &Txt) -> Result<Advert> {
        let daemon = acquire(listen)?;
        let host = host_for(&txt.device_id);
        let a = Advert { fullname: String::new(), daemon, host, port };
        match a.register(txt) {
            Ok(fullname) => Ok(Advert { fullname, ..a }),
            Err(e) => {
                release();
                Err(e)
            }
        }
    }

    fn register(&self, txt: &Txt) -> Result<String> {
        let info = record(txt, &self.host, self.port)?;
        let fullname = info.get_fullname().to_string();
        self.daemon.register(info).map_err(|e| anyhow!("mDNS register: {e}"))?;
        Ok(fullname)
    }

    /// Announce a changed TXT record (pairing state, c#, a new identity).
    pub fn update(&mut self, txt: &Txt) -> Result<()> {
        let host = host_for(&txt.device_id);
        // A new identity (reset) has a new name and host: the old record says goodbye.
        if host != self.host {
            if let Ok(rx) = self.daemon.unregister(&self.fullname) {
                let _ = rx.recv_timeout(std::time::Duration::from_secs(2));
            }
            self.host = host;
        }
        self.fullname = self.register(txt)?;
        Ok(())
    }

    /// Say goodbye and stop answering.
    pub fn stop(self) {
        if let Ok(rx) = self.daemon.unregister(&self.fullname) {
            let _ = rx.recv_timeout(std::time::Duration::from_secs(2));
        }
        release();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn setup_uri_matches_hap_nodejs() {
        // Code 031-45-154, setup id 1QJ8, a bridge: the URI HAP-NodeJS's
        // documentation shows (and HAP-python's xhm_uri gives).
        assert_eq!(setup_uri("031-45-154", "1QJ8", 2), "X-HM://0023ISYWY1QJ8");
        // HAP-python's setup hash of the same inputs.
        assert_eq!(setup_hash("ABCD", "AA:BB:CC:DD:EE:FF"), "rxqrlA==");
    }

    #[test]
    fn txt_record_has_what_hap_needs() {
        let t = Txt { name: "Home NVR".into(), device_id: "AA:BB:CC:DD:EE:FF".into(), config_number: 3, pairable: true, setup_id: "ABCD".into(), category: 2 };
        let p = t.pairs();
        let get = |k: &str| p.iter().find(|(n, _)| n == k).map(|(_, v)| v.clone()).unwrap();
        assert_eq!(get("c#"), "3");
        assert_eq!(get("sf"), "1");
        assert_eq!(get("ci"), "2");
        assert_eq!(get("id"), "AA:BB:CC:DD:EE:FF");
        assert_eq!(get("sh").len(), 8);
        assert_eq!(host_for("AA:BB:CC:DD:EE:FF"), "AmberSight-AABBCCDDEEFF.local.");
    }

    #[test]
    fn records_are_announced_without_probing() {
        let t = Txt { name: "Home NVR".into(), device_id: "AA:BB:CC:DD:EE:FF".into(), config_number: 1, pairable: true, setup_id: "ABCD".into(), category: 2 };
        let info = record(&t, "AmberSight-AABBCCDDEEFF.local.", 51830).unwrap();
        assert!(!info.requires_probe());
        assert_eq!(info.get_fullname(), "Home NVR EEFF._hap._tcp.local.");
    }

    #[test]
    fn instance_names_get_a_suffix_and_fit() {
        assert_eq!(instance_name("Home NVR", "AA:BB:CC:DD:3F:2a"), "Home NVR 3F2A");
        let long = instance_name(&"é".repeat(60), "AA:BB:CC:DD:EE:FF");
        assert!(long.len() <= 63 && long.ends_with(" EEFF"), "{long}");
    }

    #[test]
    fn only_real_interfaces_and_listened_families() {
        use std::net::IpAddr;
        let v4: IpAddr = "192.168.1.5".parse().unwrap();
        let v6: IpAddr = "fe80::1".parse().unwrap();
        for name in ["docker0", "veth12ab", "br-1a2b", "virbr0", "tailscale0", "utun3", "zt5u4y", "vEthernet (WSL)", "lo", "lo0"] {
            assert!(!announce_on(name, v4, false, Listen::Dual), "{name}");
        }
        for name in ["eth0", "enp3s0", "wlan0", "en0", "Ethernet", "Wi-Fi", "Local Area Connection"] {
            assert!(announce_on(name, v4, false, Listen::Dual), "{name}");
        }
        assert!(!announce_on("eth0", v4, true, Listen::Dual), "loopback flag");
        assert!(announce_on("eth0", v4, false, Listen::V4) && !announce_on("eth0", v6, false, Listen::V4));
        assert!(announce_on("eth0", "2001:db8::5".parse().unwrap(), false, Listen::Dual));
        assert!(announce_on("eth0", v4, false, Listen::Addr(v4)) && !announce_on("eth0", "192.168.1.6".parse().unwrap(), false, Listen::Addr(v4)));
        // Link-local IPv6: announced on Linux only (where streaming to it is tested).
        assert_eq!(announce_on("eth0", v6, false, Listen::Dual), cfg!(target_os = "linux"));
        assert!(announce_on("eth0", "fd00::5".parse().unwrap(), false, Listen::Dual));
    }
}
