//! Local video sources: capture devices (`device://`) and screen capture
//! (`screen://`). Both go through the ffmpeg pipeline, which encodes the raw
//! frames to H.264 for recording.
//!
//! URL forms:
//! - `device://<name>[?size=WxH&fps=N&format=F&audio=<name>]`: Windows dshow
//!   device name; Linux v4l2 path (`device:///dev/video0`, or `device://video0`);
//!   macOS avfoundation name or index.
//! - `screen://[<n>][?fps=N&region=x,y,w,h]`: the whole desktop (Windows
//!   gdigrab, Linux x11grab on $DISPLAY); `<n>` picks a monitor (Windows
//!   ddagrab output index) or an X screen.
//!
//! Names and values may percent-encode `% ? & # =`.

use serde::Serialize;

/// Default screen capture rate: keeps the encoder's CPU use modest.
pub const SCREEN_FPS: f64 = 15.0;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Os {
    Windows,
    Linux,
    Mac,
}

pub fn host_os() -> Os {
    if cfg!(windows) {
        Os::Windows
    } else if cfg!(target_os = "macos") {
        Os::Mac
    } else {
        Os::Linux
    }
}

pub fn is_device(url: &str) -> bool {
    url.trim_start().starts_with("device://")
}

pub fn is_screen(url: &str) -> bool {
    url.trim_start().starts_with("screen://")
}

/// A capture device or the screen: never native ingest, never stream copy,
/// never credentials.
pub fn is_local(url: &str) -> bool {
    is_device(url) || is_screen(url)
}

fn pct_decode(s: &str) -> String {
    percent_encoding::percent_decode_str(s).decode_utf8_lossy().into_owned()
}

/// `(name, [(key, value)])` of a `device://` or `screen://` URL.
fn split(url: &str) -> (String, Vec<(String, String)>) {
    let rest = url.trim().split_once("://").map(|(_, r)| r).unwrap_or("");
    let (name, query) = rest.split_once('?').unwrap_or((rest, ""));
    let params = query
        .split('&')
        .filter(|kv| !kv.is_empty())
        .map(|kv| {
            let (k, v) = kv.split_once('=').unwrap_or((kv, ""));
            (pct_decode(k).to_ascii_lowercase(), pct_decode(v))
        })
        .collect();
    (pct_decode(name), params)
}

fn param<'a>(p: &'a [(String, String)], key: &str) -> Option<&'a str> {
    p.iter().find(|(k, _)| k == key).map(|(_, v)| v.trim()).filter(|v| !v.is_empty())
}

fn size_of(p: &[(String, String)]) -> Result<Option<(u32, u32)>, String> {
    let Some(s) = param(p, "size") else { return Ok(None) };
    let parsed = s
        .split_once(['x', 'X'])
        .and_then(|(w, h)| Some((w.parse::<u32>().ok()?, h.parse::<u32>().ok()?)))
        .filter(|(w, h)| *w > 0 && *h > 0);
    parsed.map(Some).ok_or_else(|| format!("size must look like 1920x1080, not '{s}'"))
}

fn fps_of(p: &[(String, String)]) -> Result<Option<String>, String> {
    let Some(s) = param(p, "fps") else { return Ok(None) };
    match s.parse::<f64>() {
        Ok(f) if f > 0.0 && f <= 240.0 => Ok(Some(s.to_string())),
        _ => Err(format!("fps must be a number from 1 to 240, not '{s}'")),
    }
}

fn region_of(p: &[(String, String)]) -> Result<Option<[u32; 4]>, String> {
    let Some(s) = param(p, "region") else { return Ok(None) };
    let v: Vec<u32> = s.split(',').filter_map(|x| x.trim().parse().ok()).collect();
    match v.as_slice() {
        // H.264 4:2:0 needs even sizes.
        [x, y, w, h] if *w >= 2 && *h >= 2 => Ok(Some([*x, *y, w & !1, h & !1])),
        _ => Err(format!("region must be x,y,width,height, not '{s}'")),
    }
}

/// ffmpeg input arguments for a local source URL on `os`.
pub fn input_args(url: &str, os: Os) -> Result<Vec<String>, String> {
    let (name, p) = split(url);
    let size = size_of(&p)?;
    let fps = fps_of(&p)?;
    let mut a: Vec<String> = Vec::new();
    let mut push = |xs: &[&str]| a.extend(xs.iter().map(|s| s.to_string()));
    if is_device(url) {
        let name = name.trim();
        if name.is_empty() {
            return Err("no capture device named in the URL".into());
        }
        let format = param(&p, "format");
        if let Some(f) = format {
            if !f.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') {
                return Err(format!("invalid format '{f}'"));
            }
        }
        let audio = param(&p, "audio");
        match os {
            Os::Windows => {
                push(&["-f", "dshow", "-rtbufsize", "256M"]);
                if let Some((w, h)) = size {
                    push(&["-video_size", &format!("{w}x{h}")]);
                }
                if let Some(f) = &fps {
                    push(&["-framerate", f]);
                }
                if let Some(f) = format {
                    // Compressed device formats are codecs in dshow, raw ones pixel formats.
                    let opt = if matches!(f, "mjpeg" | "h264" | "hevc") { "-vcodec" } else { "-pixel_format" };
                    push(&[opt, f]);
                }
                let mut input = format!("video={name}");
                if let Some(au) = audio {
                    input.push_str(&format!(":audio={au}"));
                }
                push(&["-i", &input]);
            }
            Os::Linux => {
                push(&["-f", "v4l2"]);
                if let Some((w, h)) = size {
                    push(&["-video_size", &format!("{w}x{h}")]);
                }
                if let Some(f) = &fps {
                    push(&["-framerate", f]);
                }
                if let Some(f) = format {
                    push(&["-input_format", f]);
                }
                let path = if name.starts_with('/') { name.to_string() } else { format!("/dev/{name}") };
                push(&["-i", &path]);
            }
            Os::Mac => {
                push(&["-f", "avfoundation"]);
                if let Some((w, h)) = size {
                    push(&["-video_size", &format!("{w}x{h}")]);
                }
                // avfoundation refuses most devices without an explicit rate.
                push(&["-framerate", fps.as_deref().unwrap_or("30")]);
                let input = match audio {
                    Some(au) => format!("{name}:{au}"),
                    None => format!("{name}:none"),
                };
                push(&["-i", &input]);
            }
        }
        return Ok(a);
    }
    if !is_screen(url) {
        return Err("not a local source URL".into());
    }
    let fps = fps.unwrap_or_else(|| SCREEN_FPS.to_string());
    let region = region_of(&p)?;
    let screen = name.trim().trim_end_matches('/');
    let screen: Option<u32> = if screen.is_empty() {
        None
    } else {
        Some(screen.parse().map_err(|_| format!("screen number expected, not '{screen}'"))?)
    };
    match os {
        Os::Windows => match screen {
            // gdigrab: the whole virtual desktop, no extra GPU requirements.
            None => {
                push(&["-f", "gdigrab", "-framerate", &fps, "-draw_mouse", "1"]);
                if let Some([x, y, w, h]) = region {
                    push(&["-offset_x", &x.to_string(), "-offset_y", &y.to_string(), "-video_size", &format!("{w}x{h}")]);
                }
                push(&["-i", "desktop"]);
            }
            // ddagrab (Desktop Duplication) picks one monitor; frames live on
            // the GPU, so download them for the software filters.
            Some(n) => {
                let mut g = format!("ddagrab=output_idx={n}:framerate={fps}");
                if let Some([x, y, w, h]) = region {
                    g.push_str(&format!(":offset_x={x}:offset_y={y}:video_size={w}x{h}"));
                }
                g.push_str(",hwdownload,format=bgra");
                push(&["-f", "lavfi", "-i", &g]);
            }
        },
        Os::Linux => {
            let display = std::env::var("DISPLAY").ok().filter(|d| !d.is_empty()).unwrap_or_else(|| ":0".into());
            push(&["-f", "x11grab", "-framerate", &fps, "-draw_mouse", "1"]);
            let mut input = display;
            if let Some(n) = screen {
                // ":0" or ":0.1" -> ":0.<n>"
                let base = match input.rsplit_once('.') {
                    Some((b, s)) if s.chars().all(|c| c.is_ascii_digit()) && b.contains(':') => b.to_string(),
                    _ => input.clone(),
                };
                input = format!("{base}.{n}");
            }
            if let Some([x, y, w, h]) = region {
                push(&["-video_size", &format!("{w}x{h}")]);
                input.push_str(&format!("+{x},{y}"));
            }
            push(&["-i", &input]);
        }
        Os::Mac => return Err("screen capture is not supported on macOS yet".into()),
    }
    Ok(a)
}

/// Extra recording filter: a multi-monitor desktop can be far wider than
/// browsers decode (e.g. 9840x3840), so screen capture is capped at 3840 wide.
pub fn record_filter(url: &str) -> Option<&'static str> {
    is_screen(url).then_some("scale='min(3840,iw)':-2")
}

/// True when this process runs in Windows session 0 (a service), which
/// cannot see any signed-in user's desktop.
#[cfg(windows)]
pub fn in_session_zero() -> bool {
    use windows_sys::Win32::System::RemoteDesktop::ProcessIdToSessionId;
    let mut sid: u32 = u32::MAX;
    // SAFETY: plain out-parameter call.
    let ok = unsafe { ProcessIdToSessionId(std::process::id(), &mut sid) };
    ok != 0 && sid == 0
}

#[cfg(not(windows))]
pub fn in_session_zero() -> bool {
    false
}

/// Why a local source cannot start at all on this host, before trying.
pub fn precheck(url: &str) -> Option<String> {
    if is_screen(url) && in_session_zero() {
        return Some(SESSION0_SCREEN.into());
    }
    None
}

pub const SESSION0_SCREEN: &str = "screen capture is not possible from the Windows service: services run in session 0 and cannot see the signed-in desktop. Stop the service and start AmberSight with `ambersight run` in the user's session to capture the screen.";

/// A readable camera error from ffmpeg's stderr for a local source.
pub fn explain_error(url: &str, stderr: &str, os: Os) -> String {
    let s = stderr.to_ascii_lowercase();
    // ffmpeg names the root cause first; later lines are follow-on errors.
    let first = stderr.lines().map(str::trim).find(|l| !l.is_empty()).unwrap_or("");
    let (name, _) = split(url);
    let has = |k: &str| s.contains(k);
    if is_device(url) {
        if has("could not find video device") || has("could not find audio") || has("no such file or directory") || has("no such device") {
            return format!("capture device '{}' not found (unplugged or renamed?)", name.trim());
        }
        if has("device or resource busy") || has("busy") {
            return format!("capture device '{}' is in use by another program", name.trim());
        }
        if has("permission denied") {
            return format!("no permission to open '{}' (on Linux, add the service user to the 'video' group)", name.trim());
        }
        if has("could not run graph") || has("could not set video options") || has("i/o error") || has("unable to") || has("could not") {
            let mut m = format!("capture device '{}' refused to start", name.trim());
            if os == Os::Windows && in_session_zero() {
                m.push_str(" (some cameras, especially virtual ones, do not work from the Windows service; try `ambersight run` in the user's session)");
            } else {
                m.push_str(" (in use by another program, or the size/fps is not supported)");
            }
            if !first.is_empty() {
                m.push_str(": ");
                m.push_str(first);
            }
            return m;
        }
    } else {
        if os == Os::Windows && in_session_zero() {
            return SESSION0_SCREEN.into();
        }
        if has("cannot open display") || has("can't open display") {
            return "cannot open the X display (no desktop session, or Wayland without X access)".into();
        }
        if has("ddagrab") || has("dxgi") || has("duplicat") {
            return format!("monitor capture failed (check the monitor number): {first}");
        }
    }
    if first.is_empty() {
        "capture ended".into()
    } else {
        format!("capture failed: {first}")
    }
}

#[derive(Serialize, Default, Debug, PartialEq)]
pub struct Entry {
    pub name: String,
    pub url: String,
}

#[derive(Serialize, Default, Debug)]
pub struct DeviceList {
    pub video: Vec<Entry>,
    pub audio: Vec<Entry>,
    pub screen: Vec<Entry>,
    /// Shown in the UI when the list could not be made, or a caveat applies.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
}

/// Percent-encode only what would break the URL shape, keeping names readable.
pub fn encode_part(s: &str) -> String {
    let mut o = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '%' | '?' | '&' | '#' | '=' => o.push_str(&format!("%{:02X}", c as u32)),
            _ => o.push(c),
        }
    }
    o
}

/// Device and audio names from `ffmpeg -list_devices true -f dshow -i dummy`
/// stderr. Handles the old section format ("DirectShow video devices") and
/// the newer per-line `"Name" (video)` format.
pub fn parse_dshow_list(text: &str) -> (Vec<String>, Vec<String>) {
    let mut video = Vec::new();
    let mut audio = Vec::new();
    #[derive(PartialEq)]
    enum Sec {
        None,
        Video,
        Audio,
    }
    let mut sec = Sec::None;
    for line in text.lines() {
        // Drop the "[dshow @ 0x...]" / "[in#0 @ ...]" prefix.
        let body = match line.find("] ") {
            Some(i) if line.trim_start().starts_with('[') => &line[i + 2..],
            _ => line,
        };
        let t = body.trim();
        let lower = t.to_ascii_lowercase();
        if lower.starts_with("alternative name") {
            continue;
        }
        if lower.contains("directshow video devices") {
            sec = Sec::Video;
            continue;
        }
        if lower.contains("directshow audio devices") {
            sec = Sec::Audio;
            continue;
        }
        if !t.starts_with('"') {
            continue;
        }
        let Some(end) = t[1..].find('"') else { continue };
        let name = t[1..1 + end].to_string();
        let tail = t[2 + end..].trim().to_ascii_lowercase();
        let push = |v: &mut Vec<String>, n: &str| {
            if !v.iter().any(|x| x == n) {
                v.push(n.to_string());
            }
        };
        if let Some(kinds) = tail.strip_prefix('(').and_then(|x| x.strip_suffix(')')) {
            // "(video)", "(audio)", "(video, audio)"; "(none)" = media types
            // not reported (e.g. a virtual camera that is not running): list it as video.
            let v = kinds.contains("video") || kinds.contains("none");
            let a = kinds.contains("audio");
            if v {
                push(&mut video, &name);
            }
            if a {
                push(&mut audio, &name);
            }
        } else {
            match sec {
                Sec::Video => push(&mut video, &name),
                Sec::Audio => push(&mut audio, &name),
                Sec::None => {}
            }
        }
    }
    (video, audio)
}

/// Capture devices on this host (blocking: runs ffmpeg on Windows).
pub fn list_devices() -> DeviceList {
    let mut out = DeviceList::default();
    match host_os() {
        Os::Windows => {
            let r = crate::child::ffmpeg()
                .args(["-hide_banner", "-list_devices", "true", "-f", "dshow", "-i", "dummy"])
                .stdin(std::process::Stdio::null())
                .stdout(std::process::Stdio::null())
                .output();
            match r {
                Ok(o) => {
                    let (v, a) = parse_dshow_list(&String::from_utf8_lossy(&o.stderr));
                    out.video = v.into_iter().map(|n| Entry { url: format!("device://{}", encode_part(&n)), name: n }).collect();
                    out.audio = a.into_iter().map(|n| Entry { url: n.clone(), name: n }).collect();
                }
                Err(e) => out.note = Some(format!("could not run ffmpeg to list devices: {e}")),
            }
            out.screen.push(Entry { name: "Whole desktop".into(), url: "screen://".into() });
            if in_session_zero() {
                out.note = Some("AmberSight runs as a Windows service (session 0): screen capture will not work, and some cameras (virtual ones especially) may refuse to open.".into());
            }
        }
        Os::Linux => {
            out.video = list_v4l2(std::path::Path::new("/dev"), std::path::Path::new("/sys/class/video4linux"));
            if std::env::var("DISPLAY").map(|d| !d.is_empty()).unwrap_or(false) {
                out.screen.push(Entry { name: "Whole screen ($DISPLAY)".into(), url: "screen://".into() });
            }
        }
        Os::Mac => {
            out.note = Some("device listing is not available on macOS; enter device://<index or name>".into());
        }
    }
    out
}

/// /dev/video* nodes, named from sysfs. Metadata nodes (index > 0 on the
/// same device) are skipped.
fn list_v4l2(dev: &std::path::Path, sys: &std::path::Path) -> Vec<Entry> {
    let mut nodes: Vec<(u32, String)> = std::fs::read_dir(dev)
        .map(|rd| {
            rd.filter_map(|e| e.ok())
                .filter_map(|e| {
                    let n = e.file_name().to_string_lossy().into_owned();
                    let num: u32 = n.strip_prefix("video")?.parse().ok()?;
                    Some((num, n))
                })
                .collect()
        })
        .unwrap_or_default();
    nodes.sort();
    nodes
        .into_iter()
        .filter(|(_, n)| {
            std::fs::read_to_string(sys.join(n).join("index")).map(|i| i.trim() == "0").unwrap_or(true)
        })
        .map(|(_, n)| {
            let path = dev.join(&n).to_string_lossy().into_owned();
            let label = std::fs::read_to_string(sys.join(&n).join("name")).map(|s| s.trim().to_string()).unwrap_or_default();
            Entry {
                name: if label.is_empty() { path.clone() } else { format!("{label} ({path})") },
                url: format!("device://{path}"),
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn v(xs: &[&str]) -> Vec<String> {
        xs.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn device_args_windows() {
        assert_eq!(
            input_args("device://Integrated Camera", Os::Windows).unwrap(),
            v(&["-f", "dshow", "-rtbufsize", "256M", "-i", "video=Integrated Camera"])
        );
        assert_eq!(
            input_args("device://USB Video?size=1920x1080&fps=30&audio=Microphone (USB Audio)&format=mjpeg", Os::Windows).unwrap(),
            v(&[
                "-f", "dshow", "-rtbufsize", "256M", "-video_size", "1920x1080", "-framerate", "30",
                "-vcodec", "mjpeg", "-i", "video=USB Video:audio=Microphone (USB Audio)"
            ])
        );
        // Encoded reserved characters come back decoded.
        assert_eq!(
            input_args("device://A%26B %3F Cam", Os::Windows).unwrap().last().unwrap(),
            "video=A&B ? Cam"
        );
        assert!(input_args("device://", Os::Windows).is_err());
        assert!(input_args("device://Cam?size=big", Os::Windows).is_err());
        assert!(input_args("device://Cam?fps=0", Os::Windows).is_err());
        assert!(input_args("device://Cam?format=-x y", Os::Windows).is_err());
    }

    #[test]
    fn device_args_linux_and_mac() {
        assert_eq!(input_args("device:///dev/video0", Os::Linux).unwrap(), v(&["-f", "v4l2", "-i", "/dev/video0"]));
        assert_eq!(
            input_args("device://video2?size=1280x720&fps=15&format=mjpeg", Os::Linux).unwrap(),
            v(&["-f", "v4l2", "-video_size", "1280x720", "-framerate", "15", "-input_format", "mjpeg", "-i", "/dev/video2"])
        );
        assert_eq!(
            input_args("device://0", Os::Mac).unwrap(),
            v(&["-f", "avfoundation", "-framerate", "30", "-i", "0:none"])
        );
    }

    #[test]
    fn screen_args() {
        assert_eq!(
            input_args("screen://", Os::Windows).unwrap(),
            v(&["-f", "gdigrab", "-framerate", "15", "-draw_mouse", "1", "-i", "desktop"])
        );
        assert_eq!(
            input_args("screen://?fps=5&region=10,20,801,601", Os::Windows).unwrap(),
            v(&[
                "-f", "gdigrab", "-framerate", "5", "-draw_mouse", "1", "-offset_x", "10", "-offset_y", "20",
                "-video_size", "800x600", "-i", "desktop"
            ])
        );
        assert_eq!(
            input_args("screen://1?fps=10", Os::Windows).unwrap(),
            v(&["-f", "lavfi", "-i", "ddagrab=output_idx=1:framerate=10,hwdownload,format=bgra"])
        );
        let a = input_args("screen://?region=0,0,640,480", Os::Linux).unwrap();
        assert_eq!(&a[..6], &v(&["-f", "x11grab", "-framerate", "15", "-draw_mouse", "1"])[..]);
        assert_eq!(a[6..8], v(&["-video_size", "640x480"])[..]);
        assert!(a[9].ends_with("+0,0"));
        assert!(input_args("screen://x", Os::Windows).is_err());
        assert!(input_args("screen://?region=1,2,3", Os::Linux).is_err());
        assert!(input_args("screen://", Os::Mac).is_err());
        assert!(input_args("rtsp://h/x", Os::Linux).is_err());
    }

    #[test]
    fn local_detection() {
        assert!(is_local("device://x") && is_local("screen://") && !is_local("rtsp://h") && !is_local("lavfi://testsrc"));
    }

    // Real output from ffmpeg 9.0 (gyan.dev build) on a Windows test host.
    const NEW_FORMAT: &str = r#"[in#0 @ 0000020564f4a780] "EOS Webcam Utility" (video)
[in#0 @ 0000020564f4a780]   Alternative name "@device_pnp_\\?\root#eoswebcamsource#0000#{65e8773d-8f56-11d0-a3b9-00a0c9223196}\eoswebcamcamerasource"
[in#0 @ 0000020564f4a780] "OBS Virtual Camera" (none)
[in#0 @ 0000020564f4a780]   Alternative name "@device_sw_{860BB310-5D01-11D0-BD3B-00A0C911CE86}\{A3FCE0F5-3493-419F-958A-ABA1250EC20B}"
[in#0 @ 0000020564f4a780] "EOS Webcam Utility Pro" (video)
[in#0 @ 0000020564f4a780]   Alternative name "@device_sw_{860BB310-5D01-11D0-BD3B-00A0C911CE86}\{C8B1B2BB-014B-4DF5-ADDC-F6391CEF3FE8}"
[in#0 @ 0000020564f4a780] "Microphone (USB Condenser Microphone)" (audio)
[in#0 @ 0000020564f4a780]   Alternative name "@device_cm_{33D9A762-90C8-11D0-BD43-00A0C911CE86}\wave_{5F38D4B8-4D45-4B3C-B013-02EE5811B047}"
[dshow @ 0000020564f4b000] "Capture Card" (video, audio)
Error opening input file dummy.
"#;

    // The section format of ffmpeg 4.x.
    const OLD_FORMAT: &str = r#"[dshow @ 000001f3c7b0a2c0] DirectShow video devices (some may be both video and audio devices)
[dshow @ 000001f3c7b0a2c0]  "Integrated Camera"
[dshow @ 000001f3c7b0a2c0]     Alternative name "@device_pnp_\\?\usb#vid_04f2&pid_b6d9&mi_00#6&1a2b3c4d&0&0000#{65e8773d-8f56-11d0-a3b9-00a0c9223196}\global"
[dshow @ 000001f3c7b0a2c0]  "USB Video"
[dshow @ 000001f3c7b0a2c0]     Alternative name "@device_pnp_\\?\usb#vid_534d&pid_2109&mi_00#7&2b&0&0000#{65e8773d-8f56-11d0-a3b9-00a0c9223196}\global"
[dshow @ 000001f3c7b0a2c0] DirectShow audio devices
[dshow @ 000001f3c7b0a2c0]  "Microphone Array (Realtek(R) Audio)"
[dshow @ 000001f3c7b0a2c0]     Alternative name "@device_cm_{33D9A762-90C8-11D0-BD43-00A0C911CE86}\wave_{1}"
[dshow @ 000001f3c7b0a2c0]  "Digital Audio Interface (USB Digital Audio)"
dummy: Immediate exit requested
"#;

    #[test]
    fn dshow_new_format() {
        let (vid, aud) = parse_dshow_list(NEW_FORMAT);
        assert_eq!(vid, v(&["EOS Webcam Utility", "OBS Virtual Camera", "EOS Webcam Utility Pro", "Capture Card"]));
        assert_eq!(aud, v(&["Microphone (USB Condenser Microphone)", "Capture Card"]));
    }

    #[test]
    fn dshow_old_format() {
        let (vid, aud) = parse_dshow_list(OLD_FORMAT);
        assert_eq!(vid, v(&["Integrated Camera", "USB Video"]));
        assert_eq!(aud, v(&["Microphone Array (Realtek(R) Audio)", "Digital Audio Interface (USB Digital Audio)"]));
    }

    #[test]
    fn encode_keeps_names_readable() {
        assert_eq!(encode_part("Cam (USB) #1 & more"), "Cam (USB) %231 %26 more");
        let url = format!("device://{}", encode_part("Cam (USB) #1 & more"));
        assert_eq!(input_args(&url, Os::Windows).unwrap().last().unwrap(), "video=Cam (USB) #1 & more");
    }

    #[test]
    fn v4l2_listing_skips_metadata_nodes() {
        let tmp = std::env::temp_dir().join(format!("as-v4l2-{}", std::process::id()));
        let dev = tmp.join("dev");
        let sys = tmp.join("sys");
        for (n, idx, name) in [("video0", "0", "HD Webcam"), ("video1", "1", "HD Webcam"), ("video10", "0", "")] {
            std::fs::create_dir_all(sys.join(n)).unwrap();
            std::fs::write(sys.join(n).join("index"), idx).unwrap();
            std::fs::write(sys.join(n).join("name"), name).unwrap();
        }
        std::fs::create_dir_all(&dev).unwrap();
        for n in ["video0", "video1", "video10", "vhci"] {
            std::fs::write(dev.join(n), "").unwrap();
        }
        let l = list_v4l2(&dev, &sys);
        std::fs::remove_dir_all(&tmp).ok();
        assert_eq!(l.len(), 2);
        assert_eq!(l[0].url, format!("device://{}", dev.join("video0").display()));
        assert!(l[0].name.starts_with("HD Webcam ("));
        assert_eq!(l[1].name, dev.join("video10").display().to_string());
    }

    /// Real ffmpeg on a missing v4l2 node: the camera error names the device.
    #[cfg(target_os = "linux")]
    #[test]
    fn missing_device_error_from_ffmpeg() {
        let url = "device:///dev/video-as-missing";
        let mut args = v(&["-hide_banner", "-loglevel", "error"]);
        args.extend(input_args(url, Os::Linux).unwrap());
        args.extend(v(&["-frames:v", "1", "-f", "null", "-"]));
        let Ok(o) = crate::child::ffmpeg().args(&args).output() else { return }; // no ffmpeg here
        assert!(!o.status.success());
        let e = explain_error(url, &String::from_utf8_lossy(&o.stderr), Os::Linux);
        assert!(e.contains("not found") && e.contains("/dev/video-as-missing"), "{e}");
    }

    /// The worker's recording chain (encode to H.264 segments + MJPEG pipe)
    /// works on raw frames like a capture device delivers; a lavfi source
    /// in a raw pixel format stands in for the device here.
    #[test]
    fn raw_frames_encode_like_a_device() {
        let dir = std::env::temp_dir().join(format!("as-local-enc-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let seg = dir.join("seg%03d.mp4").to_string_lossy().into_owned();
        let args = v(&[
            "-hide_banner", "-loglevel", "error", "-t", "3", "-f", "lavfi", "-i", "testsrc2=size=320x240:rate=15,format=yuyv422",
            "-filter_complex", "[0:v]null,split=2[rec][a0];[a0]fps=5,scale=160:-2[ana]", "-map", "[rec]",
            "-c:v", "libx264", "-preset", "veryfast", "-pix_fmt", "yuv420p", "-force_key_frames", "expr:gte(t,n_forced*1)",
            "-f", "segment", "-segment_time", "1", "-reset_timestamps", "1", &seg,
            "-map", "[ana]", "-f", "image2pipe", "-c:v", "mjpeg", "-q:v", "8", "-",
        ]);
        let Ok(o) = crate::child::ffmpeg().args(&args).output() else { return };
        let segs = std::fs::read_dir(&dir).map(|r| r.count()).unwrap_or(0);
        std::fs::remove_dir_all(&dir).ok();
        assert!(o.status.success(), "{}", String::from_utf8_lossy(&o.stderr));
        assert!(segs >= 2, "segments: {segs}");
        let jpegs = o.stdout.windows(3).filter(|w| w == &[0xFF, 0xD8, 0xFF]).count();
        assert!(jpegs >= 10, "jpegs: {jpegs}");
    }

    /// The screen cap shrinks a very wide desktop (lavfi stands in) and keeps sizes even.
    #[test]
    fn screen_cap_filter_limits_width() {
        let f = record_filter("screen://").unwrap();
        assert!(record_filter("device://x").is_none());
        let chain = format!("[0:v]{f},split=2[rec][a0];[a0]null[ana]");
        let args = v(&[
            "-hide_banner", "-loglevel", "error", "-f", "lavfi", "-i", "color=size=9840x3841:rate=1:duration=1",
            "-filter_complex", &chain, "-map", "[rec]", "-frames:v", "1", "-f", "rawvideo", "-pix_fmt", "gray", "-",
            "-map", "[ana]", "-frames:v", "1", "-f", "null", "-",
        ]);
        let Ok(o) = crate::child::ffmpeg().args(&args).output() else { return };
        assert!(o.status.success(), "{}", String::from_utf8_lossy(&o.stderr));
        // 9840x3841 -> 3840 x round_even(3841*3840/9840) = 3840x1498
        assert_eq!(o.stdout.len(), 3840 * 1498);
    }

    #[test]
    fn errors_are_readable() {
        let e = explain_error("device://Cam", "[dshow @ 0] Could not find video device with name [Cam] among source devices of type video.\nError opening input", Os::Windows);
        assert!(e.contains("not found"), "{e}");
        let e = explain_error("device:///dev/video0", "[video4linux2,v4l2 @ 0x1] Cannot open video device /dev/video0: Device or resource busy", Os::Linux);
        assert!(e.contains("in use"), "{e}");
        let e = explain_error("screen://", "[x11grab @ 0x1] Cannot open display :0, error 1.", Os::Linux);
        assert!(e.contains("X display"), "{e}");
        assert_eq!(explain_error("screen://", "", Os::Linux), "capture ended");
        // Real gdigrab output from session 0: the first line is the cause.
        let e = explain_error(
            "screen://",
            "[in#0/gdigrab @ 000002b9ff5f2380] Failed to capture image (error 5)\n[out#0/image2 @ 0] Output file does not contain any stream\nError opening output files: Invalid argument",
            Os::Windows,
        );
        assert!(e.contains("Failed to capture image"), "{e}");
    }
}
