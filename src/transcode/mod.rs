//! Best quality on every device: a player says what it can decode
//! (`caps=hevc:4096x2304@60,h264:4096x2160@30,hevc10:3840x2160`); a stream
//! that fits is sent untouched, otherwise ffmpeg re-encodes it to the
//! largest size that fits, on the computer's video encoder (NVIDIA NVENC,
//! Intel Quick Sync, VAAPI, Apple VideoToolbox, AMD AMF) when it has one,
//! else in software under a small limit.
//!
//! Live transcodes are shared by everyone watching the same camera stream
//! at the same output and stop shortly after the last viewer leaves (see
//! `live.rs`); recordings and event clips are transcoded per request as
//! fragmented MP4 (`file.rs`).

pub mod file;
pub mod http;
pub mod live;
pub mod mp4;

use mp4::{VideoCodec, VideoDesc};
use serde::{Deserialize, Serialize};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, OnceLock};

// ---------------------------------------------------------------------------
// Settings
// ---------------------------------------------------------------------------

/// `[transcode]`: re-encoding video for players that cannot decode a stream.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TranscodeConfig {
    /// "auto" (the best video encoder this computer has, else software),
    /// "nvenc", "qsv", "vaapi", "videotoolbox", "amf", "software", or "off"
    /// (never transcode: players get the best stream that fits as it is).
    #[serde(default = "default_encoder")]
    pub encoder: String,
    /// Transcodes at a time on the video encoder (the whole server).
    #[serde(default = "default_max_hardware")]
    pub max_hardware: usize,
    /// Transcodes at a time in software (they take several CPU cores each).
    #[serde(default = "default_max_software")]
    pub max_software: usize,
    /// Per account: transcoded streams watched at a time, and (counted separately) live
    /// transcodes started that are still running.
    #[serde(default = "default_max_per_user")]
    pub max_per_user: usize,
}

fn default_encoder() -> String {
    "auto".into()
}
fn default_max_hardware() -> usize {
    6
}
fn default_max_software() -> usize {
    1
}
fn default_max_per_user() -> usize {
    4
}

impl Default for TranscodeConfig {
    fn default() -> Self {
        TranscodeConfig { encoder: default_encoder(), max_hardware: default_max_hardware(), max_software: default_max_software(), max_per_user: default_max_per_user() }
    }
}

impl TranscodeConfig {
    pub fn is_default(&self) -> bool {
        *self == TranscodeConfig::default()
    }

    /// The settings as used: an unknown encoder name is "auto" and a limit over 64 is 64; the
    /// other fields stay as written. With a warning for each field changed.
    pub fn sanitized(&self) -> (TranscodeConfig, Vec<String>) {
        let mut c = self.clone();
        let mut warn = Vec::new();
        if !ENCODER_NAMES.contains(&c.encoder.as_str()) {
            warn.push(format!("[transcode] encoder must be auto, nvenc, qsv, vaapi, videotoolbox, amf, software or off (not \"{}\"); \"auto\" is used", c.encoder));
            c.encoder = default_encoder();
        }
        for (name, v) in [("max_hardware", &mut c.max_hardware), ("max_software", &mut c.max_software), ("max_per_user", &mut c.max_per_user)] {
            if *v > MAX_LIMIT {
                warn.push(format!("[transcode] {name} must be {MAX_LIMIT} or less (not {v}); {MAX_LIMIT} is used"));
                *v = MAX_LIMIT;
            }
        }
        (c, warn)
    }
}

const ENCODER_NAMES: [&str; 8] = ["auto", "nvenc", "qsv", "vaapi", "videotoolbox", "amf", "software", "off"];
const MAX_LIMIT: usize = 64;

static CONFIG: Mutex<Option<TranscodeConfig>> = Mutex::new(None);

/// The settings in use (set at start and when the config is saved).
/// Values out of range are used corrected (see `sanitized`).
pub fn set_config(c: &TranscodeConfig) {
    *CONFIG.lock().unwrap() = Some(c.sanitized().0);
}

pub fn config() -> TranscodeConfig {
    CONFIG.lock().unwrap().clone().unwrap_or_default()
}

// ---------------------------------------------------------------------------
// What a player can decode
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CapCodec {
    H264,
    /// 8-bit HEVC.
    Hevc,
    /// 10-bit (and 8-bit) HEVC.
    Hevc10,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Cap {
    pub codec: CapCodec,
    pub w: u32,
    pub h: u32,
    pub fps: Option<u32>,
}

#[derive(Clone, Debug, PartialEq, Eq, Default)]
pub struct Caps(pub Vec<Cap>);

impl Caps {
    /// `codec:WxH[@fps]`, comma separated. Codecs this server does not know
    /// (a newer player's av1, say) are skipped; a malformed entry is an
    /// error.
    pub fn parse(s: &str) -> Result<Caps, String> {
        let mut out = Vec::new();
        for item in s.split(',').map(str::trim).filter(|x| !x.is_empty()) {
            if out.len() >= 16 {
                return Err("caps: at most 16 entries".into());
            }
            let (codec, rest) = item.split_once(':').ok_or_else(|| format!("caps: \"{item}\" should be codec:WxH[@fps]"))?;
            let (size, fps) = match rest.split_once('@') {
                Some((s, f)) => (s, Some(f)),
                None => (rest, None),
            };
            let (w, h) = size.split_once(['x', 'X']).ok_or_else(|| format!("caps: \"{item}\": size should be WxH"))?;
            let num = |v: &str, lo: u32, hi: u32| v.trim().parse::<u32>().ok().filter(|n| (lo..=hi).contains(n));
            let (Some(w), Some(h)) = (num(w, 16, 16384), num(h, 16, 16384)) else {
                return Err(format!("caps: \"{item}\": width and height must be 16 to 16384"));
            };
            let fps = match fps {
                Some(f) => Some(num(f, 1, 1000).ok_or_else(|| format!("caps: \"{item}\": fps must be 1 to 1000"))?),
                None => None,
            };
            let codec = match codec.trim().to_ascii_lowercase().as_str() {
                "h264" | "avc" => CapCodec::H264,
                "hevc" | "h265" => CapCodec::Hevc,
                "hevc10" | "h26510" => CapCodec::Hevc10,
                _ => continue,
            };
            out.push(Cap { codec, w, h, fps });
        }
        if out.is_empty() {
            return Err("caps: no codec this server can send (h264, hevc, hevc10)".into());
        }
        Ok(Caps(out))
    }

    /// Whether this source plays untouched.
    pub fn fits(&self, s: &Source) -> bool {
        self.0.iter().any(|c| codec_ok(c.codec, s) && size_fits(c.w, c.h, s.width, s.height) && fps_ok(c.fps, s.fps))
    }
}

/// The source's codec and bit depth decode on this kind of decoder.
fn codec_ok(c: CapCodec, s: &Source) -> bool {
    match s.codec {
        VideoCodec::H264 => c == CapCodec::H264 && s.bit_depth <= 8,
        VideoCodec::Hevc if s.bit_depth <= 8 => c == CapCodec::Hevc || c == CapCodec::Hevc10,
        VideoCodec::Hevc => c == CapCodec::Hevc10 && s.bit_depth <= 10,
        VideoCodec::Other => false,
    }
}

/// W×H is the largest frame either way round: a landscape source fits the
/// box as given, a portrait one the box turned.
fn size_fits(cw: u32, ch: u32, w: u32, h: u32) -> bool {
    w.max(h) <= cw.max(ch) && w.min(h) <= cw.min(ch)
}

fn fps_ok(cap: Option<u32>, fps: Option<f64>) -> bool {
    match (cap, fps) {
        (Some(c), Some(f)) => f <= c as f64 + 0.5,
        _ => true,
    }
}

/// A stream as it is.
pub type Source = VideoDesc;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum OutCodec {
    H264,
    Hevc,
}

impl OutCodec {
    pub fn name(self) -> &'static str {
        match self {
            OutCodec::H264 => "h264",
            OutCodec::Hevc => "hevc",
        }
    }
}

/// What a transcode makes.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Target {
    pub codec: OutCodec,
    pub width: u32,
    pub height: u32,
    /// Frame rate cap (None: the source's own).
    pub fps: Option<u32>,
    pub bitrate_kbps: u32,
}

/// Why a stream is not sent untouched.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Why {
    TooBig,
    Codec,
    Fps,
    BusyFallback,
    Off,
}

impl Why {
    pub fn name(self) -> &'static str {
        match self {
            Why::TooBig => "too_big",
            Why::Codec => "codec",
            Why::Fps => "fps",
            Why::BusyFallback => "busy_fallback",
            Why::Off => "off",
        }
    }
}

fn even_down(v: u64) -> u32 {
    ((v / 2) * 2).max(2) as u32
}

/// The largest size inside the box keeping the aspect (even sizes); the
/// source's own when it already fits.
pub fn fit_size(cw: u32, ch: u32, w: u32, h: u32) -> (u32, u32) {
    let (long, short) = (cw.max(ch) as u64, cw.min(ch) as u64);
    let (bw, bh) = if w >= h { (long, short) } else { (short, long) };
    let (w, h) = (w as u64, h as u64);
    if w <= bw && h <= bh {
        return (even_down(w), even_down(h));
    }
    if bw * h <= bh * w {
        (even_down(bw), even_down(h * bw / w))
    } else {
        (even_down(w * bh / h), even_down(bh))
    }
}

/// Bits per pixel per frame: H.264 0.08, HEVC 0.05 (about 10 Mbit/s for
/// 4096x1536 at 20 fps in H.264), between 0.3 and 40 Mbit/s.
pub fn bitrate_kbps(codec: OutCodec, w: u32, h: u32, fps: f64) -> u32 {
    let bpp = match codec {
        OutCodec::H264 => 0.08,
        OutCodec::Hevc => 0.05,
    };
    ((w as f64 * h as f64 * fps.clamp(1.0, 120.0) * bpp / 1000.0) as u32).clamp(300, 40_000)
}

/// None: the source plays as it is. Else what to make and why.
pub fn decide(caps: &Caps, s: &Source) -> Option<(Target, Why)> {
    if caps.fits(s) {
        return None;
    }
    let matching: Vec<&Cap> = caps.0.iter().filter(|c| codec_ok(c.codec, s)).collect();
    let why = if matching.is_empty() {
        Why::Codec
    } else if matching.iter().any(|c| size_fits(c.w, c.h, s.width, s.height)) {
        Why::Fps
    } else {
        Why::TooBig
    };
    // The listed decoder that takes the largest picture of this source
    // (each with its own size limit); on a tie H.264 (every decoder does it
    // well), then HEVC (8-bit, which a 10-bit decoder also takes).
    let area = |c: &Cap| {
        let (w, h) = fit_size(c.w, c.h, s.width, s.height);
        w as u64 * h as u64
    };
    let rank = |c: &Cap| match c.codec {
        CapCodec::H264 => 2,
        CapCodec::Hevc => 1,
        CapCodec::Hevc10 => 0,
    };
    let cap = caps.0.iter().max_by_key(|c| (area(c), rank(c)))?;
    let codec = if cap.codec == CapCodec::H264 { OutCodec::H264 } else { OutCodec::Hevc };
    let (width, height) = fit_size(cap.w, cap.h, s.width, s.height);
    // The source's rate capped; not before the source's rate is known (a
    // cap above it would repeat frames).
    let fps = match (cap.fps, s.fps) {
        (Some(c), Some(f)) if f > c as f64 + 0.5 => Some(c),
        _ => None,
    };
    let rate = fps.map(|f| f as f64).or(s.fps.map(f64::round)).unwrap_or(25.0);
    Some((Target { codec, width, height, fps, bitrate_kbps: bitrate_kbps(codec, width, height, rate) }, why))
}

// ---------------------------------------------------------------------------
// Encoders
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Hw {
    Nvenc,
    Qsv,
    Vaapi,
    Videotoolbox,
    Amf,
}

impl Hw {
    pub fn name(self) -> &'static str {
        match self {
            Hw::Nvenc => "nvenc",
            Hw::Qsv => "qsv",
            Hw::Vaapi => "vaapi",
            Hw::Videotoolbox => "videotoolbox",
            Hw::Amf => "amf",
        }
    }
}

/// An encoder that worked in the test encode.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct Encoder {
    /// None: software (libx264 / libx265).
    pub hw: Option<Hw>,
    /// Linux VAAPI / Quick Sync: the render node.
    pub device: Option<String>,
}

impl Encoder {
    pub fn software() -> Self {
        Encoder { hw: None, device: None }
    }

    /// As the API names it: nvenc, qsv, vaapi, videotoolbox, amf, x264, x265.
    pub fn name(&self, codec: OutCodec) -> &'static str {
        match (self.hw, codec) {
            (Some(h), _) => h.name(),
            (None, OutCodec::H264) => "x264",
            (None, OutCodec::Hevc) => "x265",
        }
    }

    fn ffmpeg_name(&self, codec: OutCodec) -> &'static str {
        match (self.hw, codec) {
            (Some(Hw::Nvenc), OutCodec::H264) => "h264_nvenc",
            (Some(Hw::Nvenc), OutCodec::Hevc) => "hevc_nvenc",
            (Some(Hw::Qsv), OutCodec::H264) => "h264_qsv",
            (Some(Hw::Qsv), OutCodec::Hevc) => "hevc_qsv",
            (Some(Hw::Vaapi), OutCodec::H264) => "h264_vaapi",
            (Some(Hw::Vaapi), OutCodec::Hevc) => "hevc_vaapi",
            (Some(Hw::Videotoolbox), OutCodec::H264) => "h264_videotoolbox",
            (Some(Hw::Videotoolbox), OutCodec::Hevc) => "hevc_videotoolbox",
            (Some(Hw::Amf), OutCodec::H264) => "h264_amf",
            (Some(Hw::Amf), OutCodec::Hevc) => "hevc_amf",
            (None, OutCodec::H264) => "libx264",
            (None, OutCodec::Hevc) => "libx265",
        }
    }
}

/// The test encodes' results: per output codec, the hardware encoders that
/// worked, best first.
#[derive(Clone, Debug, Default, Serialize)]
pub struct Probed {
    #[serde(skip)]
    pub h264: Vec<Encoder>,
    #[serde(skip)]
    pub hevc: Vec<Encoder>,
    /// For the status page: "h264_nvenc", "hevc_qsv (/dev/dri/renderD130)", ...
    pub working: Vec<String>,
    pub software: Vec<String>,
}

static PROBED: OnceLock<Probed> = OnceLock::new();

/// Render nodes (Linux), for VAAPI and Quick Sync.
fn render_nodes() -> Vec<String> {
    let mut v: Vec<String> = std::fs::read_dir("/dev/dri")
        .map(|d| d.filter_map(|e| e.ok()).map(|e| e.file_name().to_string_lossy().into_owned()).filter(|n| n.starts_with("renderD")).map(|n| format!("/dev/dri/{n}")).collect())
        .unwrap_or_default();
    v.sort();
    v
}

/// Hardware encoders worth trying on this computer, best first.
fn candidates() -> Vec<Encoder> {
    let hw = |h: Hw| Encoder { hw: Some(h), device: None };
    if cfg!(target_os = "macos") {
        vec![hw(Hw::Videotoolbox)]
    } else if cfg!(windows) {
        vec![hw(Hw::Nvenc), hw(Hw::Qsv), hw(Hw::Amf)]
    } else {
        let mut v = vec![hw(Hw::Nvenc)];
        let nodes = render_nodes();
        // An NVIDIA card's render node has no VAAPI driver of its own
        // (only through a translation layer): Intel and AMD nodes first.
        let driver = |n: &str| std::fs::read_link(format!("/sys/class/drm/{}/device/driver", n.trim_start_matches("/dev/dri/"))).ok().and_then(|p| p.file_name().map(|f| f.to_string_lossy().into_owned())).unwrap_or_default();
        let mut nodes: Vec<(String, String)> = nodes.into_iter().map(|n| (driver(&n), n)).collect();
        nodes.sort_by_key(|(d, _)| d == "nvidia");
        for (d, n) in &nodes {
            if d == "i915" || d == "xe" || d.is_empty() {
                v.push(Encoder { hw: Some(Hw::Qsv), device: Some(n.clone()) });
            }
        }
        for (d, n) in &nodes {
            if d != "nvidia" {
                v.push(Encoder { hw: Some(Hw::Vaapi), device: Some(n.clone()) });
            }
        }
        v
    }
}

/// One short test encode (half a second of a test picture). The same
/// arguments as a real transcode with software decoding.
fn test_encode(e: &Encoder, codec: OutCodec) -> bool {
    use std::process::Stdio;
    let t = Target { codec, width: 1280, height: 720, fps: None, bitrate_kbps: 2000 };
    let mut a: Vec<String> = ["-hide_banner", "-loglevel", "error", "-nostdin"].iter().map(|s| s.to_string()).collect();
    a.extend(device_args(e, false));
    a.extend(["-f", "lavfi", "-i", "testsrc2=s=1920x1080:r=30", "-frames:v", "15"].iter().map(|s| s.to_string()));
    a.extend(["-vf".to_string(), filters(e, false, &t, 8)]);
    a.extend(encoder_args(e, &t, 30.0, true));
    a.extend(["-f", "null", "-"].iter().map(|s| s.to_string()));
    let Ok(mut child) = crate::child::ffmpeg().args(&a).stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null()).spawn() else { return false };
    crate::child::adopt(&child);
    let t0 = std::time::Instant::now();
    loop {
        match child.try_wait() {
            Ok(Some(s)) => return s.success(),
            Ok(None) if t0.elapsed() < std::time::Duration::from_secs(20) => std::thread::sleep(std::time::Duration::from_millis(50)),
            _ => {
                let _ = child.kill();
                let _ = child.wait();
                return false;
            }
        }
    }
}

/// Test encodes (run once, on a thread of their own).
fn run_probe() -> Probed {
    let t0 = std::time::Instant::now();
    let mut p = Probed::default();
    // At most a minute for all of them (a stuck driver must not hold
    // transcoding back for long).
    let budget = std::time::Duration::from_secs(60);
    for e in candidates() {
        for codec in [OutCodec::H264, OutCodec::Hevc] {
            if t0.elapsed() > budget {
                break;
            }
            if test_encode(&e, codec) {
                p.working.push(format!("{}{}", e.ffmpeg_name(codec), e.device.as_ref().map(|d| format!(" ({d})")).unwrap_or_default()));
                match codec {
                    OutCodec::H264 => p.h264.push(e.clone()),
                    OutCodec::Hevc => p.hevc.push(e.clone()),
                }
            }
        }
    }
    for codec in [OutCodec::H264, OutCodec::Hevc] {
        if test_encode(&Encoder::software(), codec) {
            p.software.push(Encoder::software().ffmpeg_name(codec).to_string());
        }
    }
    tracing::info!(
        "transcode: video encoders: {} (software: {}), checked in {} ms",
        if p.working.is_empty() { "none".to_string() } else { p.working.join(", ") },
        if p.software.is_empty() { "none".to_string() } else { p.software.join(", ") },
        t0.elapsed().as_millis()
    );
    p
}

static PROBE_STARTED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// Start the test encodes now, once.
fn start_probe() {
    if !PROBE_STARTED.swap(true, Ordering::SeqCst) {
        std::thread::spawn(|| {
            let _ = PROBED.set(run_probe());
        });
    }
}

/// Start the test encodes in the background (at start), so the first
/// player does not wait for them.
pub fn probe_in_background() {
    if config().encoder == "off" {
        return;
    }
    std::thread::spawn(|| {
        std::thread::sleep(std::time::Duration::from_secs(5));
        start_probe();
    });
}

/// The encoders to use for this codec under the setting, best first
/// (hardware ones that worked, then software when it worked). None: the
/// test encodes have not finished (a request waits at most 3 s for them).
/// Blocks: call from a blocking thread.
pub fn encoders_for(codec: OutCodec) -> Option<Vec<Encoder>> {
    let cfg = config();
    if cfg.encoder == "off" {
        return Some(Vec::new());
    }
    start_probe();
    let t0 = std::time::Instant::now();
    let p = loop {
        if let Some(p) = PROBED.get() {
            break p;
        }
        if t0.elapsed() > std::time::Duration::from_secs(3) {
            return None;
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
    };
    let hw = match codec {
        OutCodec::H264 => &p.h264,
        OutCodec::Hevc => &p.hevc,
    };
    let mut v: Vec<Encoder> = match cfg.encoder.as_str() {
        "auto" => hw.clone(),
        "software" => Vec::new(),
        k => hw.iter().filter(|e| e.hw.map(|h| h.name()) == Some(k)).cloned().collect(),
    };
    let sw = Encoder::software().ffmpeg_name(codec);
    if p.software.iter().any(|s| s == sw) {
        v.push(Encoder::software());
    }
    Some(v)
}

/// Smallest picture worth making (short side, px): a player asking for
/// less gets the best stream there is instead.
pub const MIN_SHORT_SIDE: u32 = 128;

pub fn usable(t: &Target) -> bool {
    t.width.min(t.height) >= MIN_SHORT_SIDE
}

/// Log a warning at most once a minute for this key (else at debug level),
/// so a player asking again and again cannot fill the log.
pub fn warn_limited(key: &str, msg: &str) {
    static LAST: Mutex<Option<std::collections::HashMap<String, std::time::Instant>>> = Mutex::new(None);
    let mut m = LAST.lock().unwrap();
    let m = m.get_or_insert_with(Default::default);
    m.retain(|_, t| t.elapsed() < std::time::Duration::from_secs(60));
    if m.contains_key(key) {
        tracing::debug!("{msg}");
    } else {
        m.insert(key.to_string(), std::time::Instant::now());
        tracing::warn!("{msg}");
    }
}

// ---------------------------------------------------------------------------
// ffmpeg arguments
// ---------------------------------------------------------------------------

/// Arguments before the input: the decoder (`hw_decode`) and the encoder's
/// device.
pub fn device_args(e: &Encoder, hw_decode: bool) -> Vec<String> {
    let s = |v: &[&str]| v.iter().map(|x| x.to_string()).collect::<Vec<_>>();
    match (e.hw, hw_decode) {
        (Some(Hw::Nvenc), true) => s(&["-hwaccel", "cuda", "-hwaccel_output_format", "cuda"]),
        (Some(Hw::Qsv), true) => {
            let mut v = s(&["-hwaccel", "qsv", "-hwaccel_output_format", "qsv"]);
            if let Some(d) = &e.device {
                v.extend(["-qsv_device".to_string(), d.clone()]);
            }
            v
        }
        (Some(Hw::Qsv), false) => match &e.device {
            Some(d) => s(&["-init_hw_device", &format!("vaapi=va:{d}"), "-init_hw_device", "qsv=qs@va", "-filter_hw_device", "qs"]),
            None => s(&["-init_hw_device", "qsv=qs", "-filter_hw_device", "qs"]),
        },
        (Some(Hw::Vaapi), true) => s(&["-hwaccel", "vaapi", "-hwaccel_device", e.device.as_deref().unwrap_or("/dev/dri/renderD128"), "-hwaccel_output_format", "vaapi"]),
        (Some(Hw::Vaapi), false) => s(&["-vaapi_device", e.device.as_deref().unwrap_or("/dev/dri/renderD128")]),
        (Some(Hw::Videotoolbox), true) => s(&["-hwaccel", "videotoolbox"]),
        (Some(Hw::Amf), true) => s(&["-hwaccel", "d3d11va"]),
        _ => Vec::new(),
    }
}

/// The filter chain: frame rate cap, then scaling to the size (and 8-bit),
/// on the graphics card when it decoded the frames.
pub fn filters(e: &Encoder, hw_decode: bool, t: &Target, _depth: u8) -> String {
    let mut f = Vec::new();
    if let Some(r) = t.fps {
        f.push(format!("fps={r}"));
    }
    let (w, h) = (t.width, t.height);
    match (e.hw, hw_decode) {
        (Some(Hw::Nvenc), true) => f.push(format!("scale_cuda=w={w}:h={h}:format=nv12")),
        (Some(Hw::Qsv), true) => f.push(format!("vpp_qsv=w={w}:h={h}:format=nv12")),
        (Some(Hw::Vaapi), true) => f.push(format!("scale_vaapi=w={w}:h={h}:format=nv12")),
        (Some(Hw::Qsv), false) => f.push(format!("scale={w}:{h}:flags=bilinear,format=nv12,hwupload=extra_hw_frames=16")),
        (Some(Hw::Vaapi), false) => f.push(format!("scale={w}:{h}:flags=bilinear,format=nv12,hwupload")),
        (Some(_), _) => f.push(format!("scale={w}:{h}:flags=bilinear,format=nv12")),
        (None, _) => f.push(format!("scale={w}:{h}:flags=bilinear,format=yuv420p")),
    }
    f.join(",")
}

/// Encoder settings: the bit rate, a keyframe every second (live) or two,
/// no B-frames, and the low-delay settings of each encoder for live.
pub fn encoder_args(e: &Encoder, t: &Target, fps: f64, live: bool) -> Vec<String> {
    let fps = t.fps.map(|f| f as f64).unwrap_or(fps).clamp(1.0, 120.0);
    let gop = ((if live { 1.0 } else { 2.0 }) * fps).round().max(1.0) as u32;
    let k = t.bitrate_kbps;
    let mut v: Vec<String> = vec!["-c:v".into(), e.ffmpeg_name(t.codec).into()];
    let s = |v: &mut Vec<String>, a: &[&str]| v.extend(a.iter().map(|x| x.to_string()));
    s(&mut v, &["-b:v", &format!("{k}k"), "-maxrate", &format!("{}k", k * 3 / 2), "-bufsize", &format!("{}k", if live { k } else { k * 2 }), "-g", &gop.to_string(), "-bf", "0"]);
    match e.hw {
        Some(Hw::Nvenc) => {
            if live {
                s(&mut v, &["-preset", "p2", "-tune", "ull", "-zerolatency", "1", "-delay", "0", "-rc-lookahead", "0"]);
            } else {
                s(&mut v, &["-preset", "p4", "-tune", "hq"]);
            }
            s(&mut v, &["-forced-idr", "1"]);
        }
        Some(Hw::Qsv) => {
            s(&mut v, &["-preset", if live { "veryfast" } else { "medium" }, "-look_ahead", "0"]);
            if live {
                s(&mut v, &["-async_depth", "1"]);
            }
        }
        Some(Hw::Vaapi) => {
            if live {
                s(&mut v, &["-async_depth", "1"]);
            }
        }
        Some(Hw::Videotoolbox) => {
            s(&mut v, &["-realtime", if live { "1" } else { "0" }, "-allow_sw", "0"]);
        }
        Some(Hw::Amf) => {
            s(&mut v, &["-usage", if live { "ultralowlatency" } else { "transcoding" }]);
        }
        None => match t.codec {
            OutCodec::H264 => {
                s(&mut v, &["-preset", if live { "veryfast" } else { "faster" }, "-pix_fmt", "yuv420p"]);
                if live {
                    s(&mut v, &["-tune", "zerolatency"]);
                }
            }
            OutCodec::Hevc => {
                s(&mut v, &["-preset", if live { "ultrafast" } else { "veryfast" }, "-pix_fmt", "yuv420p", "-x265-params", "log-level=error"]);
                if live {
                    s(&mut v, &["-tune", "zerolatency"]);
                }
            }
        },
    }
    if t.codec == OutCodec::Hevc {
        s(&mut v, &["-tag:v", "hvc1"]);
    }
    v
}

// ---------------------------------------------------------------------------
// Limits
// ---------------------------------------------------------------------------

static HW_RUNNING: AtomicUsize = AtomicUsize::new(0);
static SW_RUNNING: AtomicUsize = AtomicUsize::new(0);

/// One running transcode's place under the limits; freed when dropped.
pub struct Slot {
    pub hardware: bool,
}

impl Drop for Slot {
    fn drop(&mut self) {
        if self.hardware { &HW_RUNNING } else { &SW_RUNNING }.fetch_sub(1, Ordering::SeqCst);
    }
}

fn take(counter: &AtomicUsize, max: usize) -> bool {
    let mut n = counter.load(Ordering::SeqCst);
    while n < max {
        match counter.compare_exchange(n, n + 1, Ordering::SeqCst, Ordering::SeqCst) {
            Ok(_) => return true,
            Err(now) => n = now,
        }
    }
    false
}

/// A place for one transcode with one of these encoders: the encoders it
/// may use (best first: hardware ones, and software after them only if it
/// got a software place too). None: every place is taken.
pub fn reserve(encoders: &[Encoder]) -> Option<(Vec<Encoder>, Slot)> {
    let cfg = config();
    let hw: Vec<Encoder> = encoders.iter().filter(|e| e.hw.is_some()).cloned().collect();
    let has_sw = encoders.iter().any(|e| e.hw.is_none());
    if !hw.is_empty() && take(&HW_RUNNING, cfg.max_hardware) {
        return Some((hw, Slot { hardware: true }));
    }
    if has_sw && take(&SW_RUNNING, cfg.max_software) {
        return Some((vec![Encoder::software()], Slot { hardware: false }));
    }
    None
}

/// A software place, for a transcode whose graphics card would not start
/// (a card's own limit on encodes at a time, say).
pub fn reserve_software() -> Option<Slot> {
    take(&SW_RUNNING, config().max_software).then_some(Slot { hardware: false })
}

/// Whether a place is free now (for `info`, which starts nothing).
pub fn would_reserve(encoders: &[Encoder]) -> Option<Encoder> {
    let cfg = config();
    if let Some(e) = encoders.iter().find(|e| e.hw.is_some()) {
        if HW_RUNNING.load(Ordering::SeqCst) < cfg.max_hardware {
            return Some(e.clone());
        }
    }
    if encoders.iter().any(|e| e.hw.is_none()) && SW_RUNNING.load(Ordering::SeqCst) < cfg.max_software {
        return Some(Encoder::software());
    }
    None
}

/// What an account's place is for: watching a transcoded stream, or a
/// live transcode it started (held for the transcode's whole life).
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum LeaseKind {
    Watching,
    Started,
}

type Counts = std::collections::HashMap<(LeaseKind, String), usize>;
static PER_USER: Mutex<Option<Counts>> = Mutex::new(None);

/// One account's place under its limit; freed when dropped.
pub struct UserLease(LeaseKind, String);

impl Drop for UserLease {
    fn drop(&mut self) {
        let mut m = PER_USER.lock().unwrap();
        let m = m.get_or_insert_with(Default::default);
        let key = (self.0, std::mem::take(&mut self.1));
        if let Some(n) = m.get_mut(&key) {
            *n = n.saturating_sub(1);
            if *n == 0 {
                m.remove(&key);
            }
        }
    }
}

/// A place for this account, within `max_per_user` of this kind.
pub fn user_lease_of(kind: LeaseKind, user: &str) -> Option<UserLease> {
    let max = config().max_per_user;
    let mut m = PER_USER.lock().unwrap();
    let n = m.get_or_insert_with(Default::default).entry((kind, user.to_string())).or_insert(0);
    if *n >= max {
        return None;
    }
    *n += 1;
    Some(UserLease(kind, user.to_string()))
}

/// A transcoded stream for this account to watch, within its limit.
pub fn user_lease(user: &str) -> Option<UserLease> {
    user_lease_of(LeaseKind::Watching, user)
}

pub fn user_has_room(user: &str) -> bool {
    let max = config().max_per_user;
    PER_USER.lock().unwrap().as_ref().and_then(|m| m.get(&(LeaseKind::Watching, user.to_string()))).copied().unwrap_or(0) < max
}

/// Transcodes one account may start in a minute (live or recordings).
pub const STARTS_PER_MINUTE: usize = 6;

/// Note a transcode this account starts; false: it started too many in
/// the last minute (nothing is noted).
pub fn may_start(user: &str) -> bool {
    use std::time::{Duration, Instant};
    static STARTS: Mutex<Option<std::collections::HashMap<String, Vec<Instant>>>> = Mutex::new(None);
    let mut m = STARTS.lock().unwrap();
    let m = m.get_or_insert_with(Default::default);
    m.retain(|_, v| {
        v.retain(|t| t.elapsed() < Duration::from_secs(60));
        !v.is_empty()
    });
    let v = m.entry(user.to_string()).or_default();
    if v.len() >= STARTS_PER_MINUTE {
        return false;
    }
    v.push(Instant::now());
    true
}

// ---------------------------------------------------------------------------
// What runs (Settings > System status)
// ---------------------------------------------------------------------------

pub struct Active {
    pub id: u64,
    pub what: String,
    pub camera: String,
    pub target: Target,
    pub encoder: Mutex<String>,
    pub started: std::time::Instant,
    pub viewers: Box<dyn Fn() -> usize + Send + Sync>,
    pub pid: Mutex<Option<u32>>,
}

static ACTIVE: Mutex<Vec<Arc<Active>>> = Mutex::new(Vec::new());
static NEXT_ID: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);

/// Listed while the returned guard lives.
pub struct Listed(pub Arc<Active>);

impl Drop for Listed {
    fn drop(&mut self) {
        let id = self.0.id;
        ACTIVE.lock().unwrap().retain(|a| a.id != id);
    }
}

pub fn list_active(what: &str, camera: &str, target: Target, viewers: Box<dyn Fn() -> usize + Send + Sync>) -> Listed {
    let a = Arc::new(Active {
        id: NEXT_ID.fetch_add(1, Ordering::Relaxed),
        what: what.into(),
        camera: camera.into(),
        target,
        encoder: Mutex::new(String::new()),
        started: std::time::Instant::now(),
        viewers,
        pid: Mutex::new(None),
    });
    ACTIVE.lock().unwrap().push(a.clone());
    Listed(a)
}

/// CPU time of a process, seconds (Linux), for a rough CPU share.
fn cpu_seconds(pid: u32) -> Option<f64> {
    let s = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    let rest = s.rsplit_once(')')?.1;
    let f: Vec<&str> = rest.split_whitespace().collect();
    let ticks: f64 = f.get(11)?.parse::<f64>().ok()? + f.get(12)?.parse::<f64>().ok()?;
    Some(ticks / 100.0)
}

/// The running transcodes, for admins.
pub fn status() -> serde_json::Value {
    let cfg = config();
    let list: Vec<serde_json::Value> = ACTIVE
        .lock()
        .unwrap()
        .iter()
        .map(|a| {
            let secs = a.started.elapsed().as_secs_f64();
            let pid = *a.pid.lock().unwrap();
            let cpu = pid.and_then(cpu_seconds).map(|c| (c / secs.max(1.0) * 100.0).round());
            serde_json::json!({
                "what": a.what,
                "camera": a.camera,
                "codec": a.target.codec.name(),
                "width": a.target.width,
                "height": a.target.height,
                "fps": a.target.fps,
                "bitrate_kbps": a.target.bitrate_kbps,
                "encoder": a.encoder.lock().unwrap().clone(),
                "viewers": (a.viewers)(),
                "seconds": secs as u64,
                "cpu_percent": cpu,
            })
        })
        .collect();
    serde_json::json!({
        "setting": cfg.encoder,
        "encoders": PROBED.get(),
        "hardware": { "running": HW_RUNNING.load(Ordering::SeqCst), "max": cfg.max_hardware },
        "software": { "running": SW_RUNNING.load(Ordering::SeqCst), "max": cfg.max_software },
        "active": list,
    })
}

// ---------------------------------------------------------------------------
// Answers
// ---------------------------------------------------------------------------

/// What a player gets: the fields `info` adds with caps.
#[derive(Clone, Debug)]
pub struct Plan {
    pub source: Source,
    /// Some: transcoded to this, with this encoder.
    pub target: Option<Target>,
    pub encoder: Option<String>,
    pub why: Option<Why>,
    /// The stream sent untouched instead (busy / off): its description.
    pub sent: Option<Source>,
}

impl Plan {
    pub fn json(&self) -> serde_json::Value {
        let src = &self.source;
        let source = serde_json::json!({
            "codec": src.codec,
            "width": src.width,
            "height": src.height,
            "fps": src.fps.map(|f| (f * 100.0).round() / 100.0),
            "bit_depth": src.bit_depth,
        });
        match &self.target {
            Some(t) => serde_json::json!({
                "codec": t.codec.name(),
                "width": t.width,
                "height": t.height,
                "fps": t.fps.map(|f| f as f64).or(src.fps.map(|f| (f * 100.0).round() / 100.0)),
                "transcoded": true,
                "bitrate_kbps": t.bitrate_kbps,
                "source": source,
                "encoder": self.encoder,
                "why": self.why.map(Why::name),
            }),
            None => {
                let s = self.sent.as_ref().unwrap_or(src);
                serde_json::json!({
                    "codec": s.codec,
                    "width": s.width,
                    "height": s.height,
                    "fps": s.fps.map(|f| (f * 100.0).round() / 100.0),
                    "transcoded": false,
                    "bitrate_kbps": null,
                    "source": source,
                    "encoder": null,
                    "why": self.why.map(Why::name),
                })
            }
        }
    }

    /// X-AS-* response headers.
    pub fn headers(&self) -> Vec<(&'static str, String)> {
        let mut h = Vec::new();
        match &self.target {
            Some(t) => {
                h.push(("x-as-codec", t.codec.name().to_string()));
                h.push(("x-as-width", t.width.to_string()));
                h.push(("x-as-height", t.height.to_string()));
                h.push(("x-as-transcoded", "1".into()));
                if let Some(e) = &self.encoder {
                    h.push(("x-as-encoder", e.clone()));
                }
            }
            None => {
                let s = self.sent.as_ref().unwrap_or(&self.source);
                let c = match s.codec {
                    VideoCodec::H264 => "h264",
                    VideoCodec::Hevc => "hevc",
                    VideoCodec::Other => "other",
                };
                h.push(("x-as-codec", c.to_string()));
                h.push(("x-as-width", s.width.to_string()));
                h.push(("x-as-height", s.height.to_string()));
                h.push(("x-as-transcoded", "0".into()));
            }
        }
        if let Some(w) = self.why {
            h.push(("x-as-why", w.name().to_string()));
        }
        h
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn src(codec: VideoCodec, depth: u8, w: u32, h: u32, fps: f64) -> Source {
        VideoDesc { codec, bit_depth: depth, width: w, height: h, fps: Some(fps), codec_string: None, track_id: 1, timescale: 90000, trex_flags: 0, trex_duration: 0, duration_ms: None }
    }

    #[test]
    fn parses_caps() {
        let c = Caps::parse("hevc:4096x2304@60, h264:4096x2160@30,hevc10:3840x2160,av1:8192x4320").unwrap();
        assert_eq!(c.0.len(), 3);
        assert_eq!(c.0[0], Cap { codec: CapCodec::Hevc, w: 4096, h: 2304, fps: Some(60) });
        assert_eq!(c.0[2].fps, None);
        assert!(Caps::parse("hevc:4096").is_err());
        assert!(Caps::parse("hevc:0x10").is_err());
        assert!(Caps::parse("av1:1920x1080").is_err(), "nothing we can send");
        assert!(Caps::parse("h264:1920x1080@0").is_err());
    }

    #[test]
    fn fits_untouched() {
        let s = src(VideoCodec::Hevc, 8, 4608, 1728, 20.0);
        assert_eq!(decide(&Caps::parse("hevc:8192x4320").unwrap(), &s), None);
        assert_eq!(decide(&Caps::parse("hevc10:8192x4320@30").unwrap(), &s), None, "a 10-bit decoder takes 8-bit");
        // Portrait source in a landscape box.
        assert_eq!(decide(&Caps::parse("hevc:4608x1728").unwrap(), &src(VideoCodec::Hevc, 8, 1728, 4608, 20.0)), None);
    }

    #[test]
    fn too_big_is_fitted_keeping_aspect() {
        let s = src(VideoCodec::Hevc, 8, 4608, 1728, 20.0);
        let (t, why) = decide(&Caps::parse("hevc:4096x2304").unwrap(), &s).unwrap();
        assert_eq!((t.codec, t.width, t.height, t.fps, why), (OutCodec::Hevc, 4096, 1536, None, Why::TooBig));
        let (t, _) = decide(&Caps::parse("hevc:4096x2304,h264:4096x2304").unwrap(), &s).unwrap();
        assert_eq!(t.codec, OutCodec::H264, "H.264 preferred");
        // Portrait.
        let (t, _) = decide(&Caps::parse("hevc:4096x2304").unwrap(), &src(VideoCodec::Hevc, 8, 1728, 4608, 20.0)).unwrap();
        assert_eq!((t.width, t.height), (1536, 4096));
        // Height-limited, odd results made even.
        assert_eq!(fit_size(1920, 1080, 3000, 2000), (1620, 1080));
        assert_eq!(fit_size(1000, 1000, 1999, 999), (1000, 498));
    }

    #[test]
    fn google_tv_streamer() {
        // A Google TV Streamer (MediaCodec hardware decoders).
        let caps = Caps::parse("h264:4096x2304@56,hevc:4096x2176@60,hevc10:4096x2176@60").unwrap();
        let s = src(VideoCodec::Hevc, 8, 4608, 1728, 20.0);
        let (t, why) = decide(&caps, &s).unwrap();
        assert_eq!((t.codec, t.width, t.height, t.fps, why), (OutCodec::H264, 4096, 1536, None, Why::TooBig));
        // A 60 fps source: H.264's own 56 applies.
        let (t, _) = decide(&caps, &src(VideoCodec::Hevc, 8, 4608, 1728, 60.0)).unwrap();
        assert_eq!(t.fps, Some(56));
        // HEVC 4096x2176 fits HEVC's own limit, 4096x2304 does not.
        assert_eq!(decide(&caps, &src(VideoCodec::Hevc, 8, 4096, 2176, 60.0)), None);
        assert_eq!(decide(&caps, &src(VideoCodec::Hevc, 8, 4096, 2304, 30.0)).unwrap().1, Why::TooBig);
        // ...but H.264 at 4096x2304 plays.
        assert_eq!(decide(&caps, &src(VideoCodec::H264, 8, 4096, 2304, 30.0)), None);
        assert_eq!(decide(&caps, &src(VideoCodec::Hevc, 10, 3840, 2160, 30.0)), None);
        assert!(caps.fits(&src(VideoCodec::H264, 8, 640, 240, 20.0)));
    }

    #[test]
    fn the_codec_that_gives_the_larger_picture() {
        let s = src(VideoCodec::Hevc, 8, 4608, 1728, 20.0);
        let (t, _) = decide(&Caps::parse("h264:1920x1080,hevc:4096x2304").unwrap(), &s).unwrap();
        assert_eq!((t.codec, t.width, t.height), (OutCodec::Hevc, 4096, 1536));
        let (t, _) = decide(&Caps::parse("hevc10:4096x2304,h264:1920x1080").unwrap(), &s).unwrap();
        assert_eq!((t.codec, t.width), (OutCodec::Hevc, 4096), "hevc10 is sent as 8-bit HEVC");
        // Source rate not known yet: no cap (it would repeat frames).
        let mut u = s.clone();
        u.fps = None;
        let (t, _) = decide(&Caps::parse("h264:4096x2304@15").unwrap(), &u).unwrap();
        assert_eq!(t.fps, None);
    }

    #[test]
    fn codec_depth_and_fps() {
        let s = src(VideoCodec::Hevc, 8, 1920, 1080, 30.0);
        let (t, why) = decide(&Caps::parse("h264:4096x2160").unwrap(), &s).unwrap();
        assert_eq!((t.codec, t.width, t.height, why), (OutCodec::H264, 1920, 1080, Why::Codec));
        let ten = src(VideoCodec::Hevc, 10, 3840, 2160, 30.0);
        let (t, why) = decide(&Caps::parse("hevc:4096x2160").unwrap(), &ten).unwrap();
        assert_eq!((t.codec, t.width, why), (OutCodec::Hevc, 3840, Why::Codec), "10-bit needs hevc10: 8-bit HEVC");
        let (t, why) = decide(&Caps::parse("hevc:4096x2160@15").unwrap(), &s).unwrap();
        assert_eq!((t.fps, why), (Some(15), Why::Fps));
        assert_eq!(decide(&Caps::parse("hevc:4096x2160@30").unwrap(), &s), None, "30 in 30");
        assert!(decide(&Caps::parse("hevc10:3840x2160").unwrap(), &ten).is_none());
    }

    #[test]
    fn bitrates_scale_with_pixels() {
        assert_eq!(bitrate_kbps(OutCodec::H264, 4096, 1536, 20.0), 10066);
        assert_eq!(bitrate_kbps(OutCodec::Hevc, 4096, 1536, 20.0), 6291);
        assert_eq!(bitrate_kbps(OutCodec::H264, 160, 90, 5.0), 300);
    }

    #[test]
    fn user_limit() {
        let a: Vec<_> = (0..default_max_per_user()).map(|_| user_lease("limit-test").unwrap()).collect();
        assert!(user_lease("limit-test").is_none());
        assert!(!user_has_room("limit-test"));
        drop(a);
        assert!(user_lease("limit-test").is_some());
        // Started transcodes count apart from watching.
        let s: Vec<_> = (0..default_max_per_user()).map(|_| user_lease_of(LeaseKind::Started, "limit-test").unwrap()).collect();
        assert!(user_lease_of(LeaseKind::Started, "limit-test").is_none());
        assert!(user_lease("limit-test").is_some());
        drop(s);
        // At most STARTS_PER_MINUTE starts a minute.
        assert!((0..STARTS_PER_MINUTE).all(|_| may_start("rate-test")));
        assert!(!may_start("rate-test"));
        assert!(may_start("rate-test-2"));
        assert!(!usable(&Target { codec: OutCodec::H264, width: 320, height: 120, fps: None, bitrate_kbps: 300 }));
        assert!(usable(&Target { codec: OutCodec::H264, width: 640, height: 240, fps: None, bitrate_kbps: 300 }));
    }
}
