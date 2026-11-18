//! Windows Firewall: while HomeKit is on, an inbound rule "AmberSight
//! HomeKit" for ambersight.exe that opens only what HomeKit uses: the HAP
//! TCP ports of the accessories running now, and UDP 5353 (mDNS) plus the
//! `[homekit] rtp_ports` range (RTCP of live video). Never the web page's
//! port (that is the "AmberSight NVR" rule's business). Like that rule it
//! names no profile (all of them). Made to match the ports in use whenever
//! they change, and at every start (a leftover rule of a killed process is
//! removed when HomeKit is off). Removed at uninstall too. Other systems:
//! the docs say which ports to open (ufw / firewalld).

pub const RULE: &str = "AmberSight HomeKit";

/// What the rule opens.
#[derive(Clone, Debug, PartialEq)]
pub struct Ports {
    /// The HAP TCP ports in use (none: no rule).
    pub tcp: Vec<u16>,
    /// The RTP/RTCP UDP range.
    pub rtp: (u16, u16),
}

impl Ports {
    pub fn none(rtp: (u16, u16)) -> Ports {
        Ports { tcp: Vec::new(), rtp }
    }
    pub fn sorted(mut self) -> Ports {
        self.tcp.sort_unstable();
        self.tcp.dedup();
        self
    }
    /// The TCP ports as netsh takes them: "51831-51833,51840".
    pub fn tcp_list(&self) -> String {
        let mut t = self.tcp.clone();
        t.sort_unstable();
        t.dedup();
        let mut out: Vec<String> = Vec::new();
        let mut i = 0;
        while i < t.len() {
            let mut j = i;
            while j + 1 < t.len() && t[j + 1] == t[j] + 1 {
                j += 1;
            }
            out.push(if i == j { t[i].to_string() } else { format!("{}-{}", t[i], t[j]) });
            i = j + 1;
        }
        out.join(",")
    }
    pub fn udp_list(&self) -> String {
        format!("5353,{}-{}", self.rtp.0, self.rtp.1)
    }
}

/// netsh arguments that remove the rule (both of its parts).
pub fn delete_args() -> Vec<String> {
    ["advfirewall", "firewall", "delete", "rule", &format!("name={RULE}")].map(String::from).to_vec()
}

/// netsh arguments that show the rule (they fail when there is none).
#[cfg_attr(not(windows), allow(dead_code))]
pub fn show_args() -> Vec<String> {
    ["advfirewall", "firewall", "show", "rule", &format!("name={RULE}")].map(String::from).to_vec()
}

/// netsh arguments that allow this program in on these ports of `protocol`.
#[cfg_attr(not(windows), allow(dead_code))]
pub fn add_args(exe: &str, protocol: &str, ports: &str) -> Vec<String> {
    vec![
        "advfirewall".into(),
        "firewall".into(),
        "add".into(),
        "rule".into(),
        format!("name={RULE}"),
        "dir=in".into(),
        "action=allow".into(),
        format!("program={exe}"),
        format!("protocol={protocol}"),
        format!("localport={ports}"),
        "enable=yes".into(),
    ]
}

/// What to open by hand when AmberSight cannot add the rule itself
/// (`elevated`: it could have, but netsh failed).
#[cfg_attr(not(windows), allow(dead_code))]
pub fn manual_hint(exe: &str, p: &Ports, elevated: bool) -> String {
    format!(
        "Windows Firewall: AmberSight could not add its \"{RULE}\" rule ({}). Open TCP {} and UDP {} for {exe}. As Administrator: netsh advfirewall firewall add rule name=\"{RULE}\" dir=in action=allow program=\"{exe}\" protocol=TCP localport={}, and the same with protocol=UDP localport={}",
        if elevated { "netsh refused it; the log may say more" } else { "it is not running as Administrator" },
        p.tcp_list(),
        p.udp_list(),
        p.tcp_list(),
        p.udp_list()
    )
}

/// Make the rule match `p` (no TCP ports: no rule). Err: why not, with
/// what to do by hand.
#[cfg(windows)]
pub fn apply(p: &Ports) -> Result<(), String> {
    let exe = std::env::current_exe().map(|e| e.display().to_string()).unwrap_or_else(|_| "ambersight.exe".into());
    let on = !p.tcp.is_empty();
    if !crate::setup::is_elevated() {
        return if on { Err(manual_hint(&exe, p, false)) } else { Ok(()) };
    }
    let run = |args: &[String]| std::process::Command::new("netsh").args(args).output().map(|o| o.status.success()).unwrap_or(false);
    // (Removed first, when there is one: never two copies, nor an old port set.)
    if run(&show_args()) {
        let _ = run(&delete_args());
    }
    if !on {
        return Ok(());
    }
    if !run(&add_args(&exe, "TCP", &p.tcp_list())) || !run(&add_args(&exe, "UDP", &p.udp_list())) {
        let _ = run(&delete_args());
        return Err(manual_hint(&exe, p, true));
    }
    tracing::info!("HomeKit: Windows Firewall rule \"{RULE}\": TCP {}, UDP {}", p.tcp_list(), p.udp_list());
    Ok(())
}

#[cfg(not(windows))]
pub fn apply(_p: &Ports) -> Result<(), String> {
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rule_opens_only_homekit_ports() {
        let exe = r"C:\Program Files\AmberSight NVR\ambersight.exe";
        let p = Ports { tcp: vec![51833, 51831, 51832, 51840, 51831], rtp: (51900, 51999) }.sorted();
        assert_eq!(p.tcp_list(), "51831-51833,51840");
        assert_eq!(p.udp_list(), "5353,51900-51999");
        assert_eq!(
            add_args(exe, "TCP", &p.tcp_list()).join(" "),
            r"advfirewall firewall add rule name=AmberSight HomeKit dir=in action=allow program=C:\Program Files\AmberSight NVR\ambersight.exe protocol=TCP localport=51831-51833,51840 enable=yes"
        );
        // (One argument each: Windows quotes the ones with spaces when it runs netsh.)
        assert!(add_args(exe, "UDP", "5353").contains(&format!("program={exe}")));
        assert_eq!(delete_args().join(" "), "advfirewall firewall delete rule name=AmberSight HomeKit");
        let h = manual_hint(exe, &p, false);
        assert!(h.contains("TCP 51831-51833,51840") && h.contains("UDP 5353,51900-51999") && !h.contains("8090") && h.contains("not running as Administrator"), "{h}");
        assert!(manual_hint(exe, &p, true).contains("netsh refused"));
        assert_eq!(show_args().join(" "), "advfirewall firewall show rule name=AmberSight HomeKit");
        assert_eq!(Ports { tcp: vec![51830], rtp: (51900, 51999) }.tcp_list(), "51830");
        assert!(Ports::none((1, 2)).tcp.is_empty());
    }
}
