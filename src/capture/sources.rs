//! What this computer can capture (read-only enumeration).

use super::proto::{MonitorInfo, Sources, WindowInfo};

/// Monitors and visible windows with titles.
pub fn enumerate() -> Sources {
    #[cfg(windows)]
    {
        Sources { monitors: super::win::monitors(), windows: super::win::windows() }
    }
    #[cfg(target_os = "macos")]
    {
        Sources { monitors: mac_screens(), windows: Vec::new() }
    }
    #[cfg(all(unix, not(target_os = "macos")))]
    {
        linux::enumerate()
    }
}

/// macOS: the screens ffmpeg's AVFoundation input offers ("Capture screen N").
/// Sizes are not known here (0 = unknown). Single windows cannot be captured.
#[cfg(target_os = "macos")]
fn mac_screens() -> Vec<MonitorInfo> {
    let out = std::process::Command::new(super::ffmpeg::ffmpeg_path())
        .args(["-hide_banner", "-f", "avfoundation", "-list_devices", "true", "-i", ""])
        .stdin(std::process::Stdio::null())
        .output();
    out.map(|o| parse_avfoundation_screens(&String::from_utf8_lossy(&o.stderr))).unwrap_or_default()
}

/// `[AVFoundation indev @ 0x..] [3] Capture screen 0` -> monitor 0.
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
pub fn parse_avfoundation_screens(text: &str) -> Vec<MonitorInfo> {
    let mut out = Vec::new();
    for line in text.lines() {
        let Some(i) = line.find("Capture screen ") else { continue };
        let Ok(n) = line[i + "Capture screen ".len()..].trim().parse::<u32>() else { continue };
        if out.iter().any(|m: &MonitorInfo| m.index == n) {
            continue;
        }
        out.push(MonitorInfo { index: n, name: format!("Screen {}", n + 1), x: 0, y: 0, w: 0, h: 0, primary: n == 0 });
    }
    out
}

/// The open window with this title (and exe, when given).
pub fn find_window(title: &str, exe: Option<&str>) -> Option<WindowInfo> {
    let exe = exe.map(str::trim).filter(|e| !e.is_empty());
    enumerate().windows.into_iter().find(|w| w.title == title && exe.is_none_or(|e| w.exe.eq_ignore_ascii_case(e)))
}

pub fn find_monitor(index: u32) -> Option<MonitorInfo> {
    enumerate().monitors.into_iter().find(|m| m.index == index)
}

#[cfg(not(windows))]
#[cfg_attr(target_os = "macos", allow(dead_code))]
pub mod linux {
    use super::*;

    pub fn enumerate() -> Sources {
        if std::env::var_os("DISPLAY").is_none_or(|d| d.is_empty()) {
            return Sources::default();
        }
        let run = |prog: &str, args: &[&str]| -> Option<String> {
            let out = std::process::Command::new(prog).args(args).stdin(std::process::Stdio::null()).stderr(std::process::Stdio::null()).output().ok()?;
            out.status.success().then(|| String::from_utf8_lossy(&out.stdout).into_owned())
        };
        let monitors = run("xrandr", &["--listmonitors"]).map(|t| parse_xrandr(&t)).unwrap_or_default();
        let windows = run("wmctrl", &["-lpG"])
            .map(|t| parse_wmctrl(&t, |pid| std::fs::read_to_string(format!("/proc/{pid}/comm")).ok().map(|s| s.trim().to_string())))
            .unwrap_or_default();
        Sources { monitors, windows }
    }

    /// `xrandr --listmonitors`:
    /// ` 0: +*DP-1 2560/597x1440/336+0+0  DP-1`
    pub fn parse_xrandr(text: &str) -> Vec<MonitorInfo> {
        let mut out = Vec::new();
        for line in text.lines().skip(1) {
            let Some((idx, rest)) = line.trim().split_once(':') else { continue };
            let Ok(index) = idx.trim().parse::<u32>() else { continue };
            let mut parts = rest.split_whitespace();
            let (Some(flags_name), Some(geom)) = (parts.next(), parts.next()) else { continue };
            let primary = flags_name.contains('*');
            let name = parts.next().unwrap_or(flags_name.trim_start_matches(['+', '*'])).to_string();
            // 2560/597x1440/336+0+0
            let Some((wpart, rest)) = geom.split_once('x') else { continue };
            let w = wpart.split('/').next().and_then(|v| v.parse().ok());
            let mut hp = rest.splitn(3, '+');
            let h = hp.next().and_then(|v| v.split('/').next()).and_then(|v| v.parse().ok());
            let x = hp.next().and_then(|v| v.parse().ok());
            let y = hp.next().and_then(|v| v.parse().ok());
            if let (Some(w), Some(h), Some(x), Some(y)) = (w, h, x, y) {
                out.push(MonitorInfo { index, name, x, y, w, h, primary });
            }
        }
        out
    }

    /// `wmctrl -lpG`: `0x03000003  0 1234 10 20 800 600 host Title words`
    pub fn parse_wmctrl(text: &str, exe_of: impl Fn(u32) -> Option<String>) -> Vec<WindowInfo> {
        let mut out = Vec::new();
        for line in text.lines() {
            let f: Vec<&str> = line.split_whitespace().collect();
            if f.len() < 9 {
                continue;
            }
            // Desktop -1: sticky panels and docks.
            if f[1] == "-1" {
                continue;
            }
            let Some(xid) = f[0].strip_prefix("0x").and_then(|h| u64::from_str_radix(h, 16).ok()) else { continue };
            let (Ok(w), Ok(h)) = (f[5].parse::<u32>(), f[6].parse::<u32>()) else { continue };
            // The title is everything after the host field, spacing kept.
            let title = nth_field_rest(line, 8).trim_end().to_string();
            if title.is_empty() || w == 0 || h == 0 {
                continue;
            }
            let exe = f[2].parse::<u32>().ok().filter(|p| *p > 0).and_then(&exe_of).unwrap_or_default();
            out.push(WindowInfo { title, exe, w, h, xid: Some(xid) });
        }
        out
    }

    /// The rest of `line` from whitespace-separated field `n` on.
    fn nth_field_rest(line: &str, n: usize) -> &str {
        let mut rest = line.trim_start();
        for _ in 0..n {
            let end = rest.find(char::is_whitespace).unwrap_or(rest.len());
            rest = rest[end..].trim_start();
        }
        rest
    }
}

#[cfg(all(test, not(windows)))]
mod tests {
    use super::linux::*;

    #[test]
    fn xrandr_monitors() {
        let t = "Monitors: 2\n 0: +*DP-1 2560/597x1440/336+0+0  DP-1\n 1: +HDMI-A-1 1920/527x1080/296+2560+180  HDMI-A-1\n";
        let m = parse_xrandr(t);
        assert_eq!(m.len(), 2);
        assert_eq!((m[0].name.as_str(), m[0].w, m[0].h, m[0].x, m[0].y, m[0].primary), ("DP-1", 2560, 1440, 0, 0, true));
        assert_eq!((m[1].index, m[1].x, m[1].y, m[1].primary), (1, 2560, 180, false));
        assert!(parse_xrandr("Monitors: 0\n").is_empty());
    }

    #[test]
    fn wmctrl_windows() {
        let t = "0x01e00003 -1 900 0 0 1920 30 host panel\n0x03000007  0 1234 10 20 800 600 host My  Doc - Editor\n0x03000009  0 0 0 0 0 0 host zero\n";
        let w = parse_wmctrl(t, |p| (p == 1234).then(|| "editor".to_string()));
        assert_eq!(w.len(), 1);
        assert_eq!((w[0].title.as_str(), w[0].exe.as_str(), w[0].w, w[0].h, w[0].xid), ("My  Doc - Editor", "editor", 800, 600, Some(0x03000007)));
    }
}

#[cfg(test)]
mod mac_tests {
    #[test]
    fn avfoundation_screens() {
        let t = "[AVFoundation indev @ 0x7f] AVFoundation video devices:\n[AVFoundation indev @ 0x7f] [0] FaceTime HD Camera\n[AVFoundation indev @ 0x7f] [1] Capture screen 0\n[AVFoundation indev @ 0x7f] [2] Capture screen 1\n[AVFoundation indev @ 0x7f] AVFoundation audio devices:\n";
        let m = super::parse_avfoundation_screens(t);
        assert_eq!(m.iter().map(|x| x.index).collect::<Vec<_>>(), vec![0, 1]);
        assert!(m[0].primary && !m[1].primary);
    }
}
