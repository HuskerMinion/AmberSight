//! Hardware video decoding for motion analysis.
//!
//! A camera's `hwaccel` setting says how the stream analysed for motion is
//! decoded: unset or "auto", "off" (the processor), or a named way: "vaapi",
//! "vaapi:/dev/dri/renderD129", "qsv", "qsv:/dev/dri/renderD128", and the
//! older pass-through names "cuda", "vulkan", "videotoolbox", "d3d11va",
//! "dxva2".
//!
//! Auto (Linux only) uses VAAPI on the first render node that passed a real
//! test decode at start: a short H.264 and H.265 sample is decoded on the
//! card the same way a camera is (the same ffmpeg arguments, fed through
//! stdin) and the pictures are compared with software decoding of the same
//! sample. A node that is present but cannot start, or decodes wrong
//! pictures, is never used by Auto. VAAPI is used rather than Quick Sync on
//! Intel: both drive the same decoder on the chip, and VAAPI needs no codec
//! named up front and works on AMD too. Quick Sync is tested (Intel nodes) and
//! can be chosen per camera.
//!
//! A decoder that fails later (stops, stalls, or reports a burst of errors)
//! is replaced by software decoding at once (`ingest::decode`); when software
//! then works, the card is remembered as failing for that codec for an hour
//! (`transcode::HwMemory`), so other cameras skip it too.
//!
//! Windows and macOS: Auto means software for now (d3d11va / videotoolbox
//! can still be named per camera; they go through the same fallback).

use crate::ingest::decode::Codec;
use serde::Serialize;
use std::collections::HashMap;
use std::io::{Read, Write};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

/// Ways that can be named in `hwaccel` (besides auto and off).
pub const KINDS: [&str; 7] = ["vaapi", "qsv", "cuda", "vulkan", "videotoolbox", "d3d11va", "dxva2"];

/// The setting, read.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Choice {
    Auto,
    Off,
    Named { kind: String, device: Option<String> },
}

fn split_spec(spec: &str) -> (String, Option<String>) {
    match spec.split_once(':') {
        Some((k, d)) if !d.trim().is_empty() => (k.trim().to_ascii_lowercase(), Some(d.trim().to_string())),
        Some((k, _)) => (k.trim().to_ascii_lowercase(), None),
        None => (spec.trim().to_ascii_lowercase(), None),
    }
}

/// A render node path: `/dev/dri/renderD<number>`.
pub fn is_render_node(d: &str) -> bool {
    d.strip_prefix("/dev/dri/renderD").is_some_and(|n| !n.is_empty() && n.len() <= 4 && n.bytes().all(|b| b.is_ascii_digit()))
}

/// Read a setting. Anything not understood is Auto (`check` refuses it in
/// the API; a hand-written config gets a warning at load).
pub fn choice(setting: Option<&str>) -> Choice {
    let s = setting.map(str::trim).unwrap_or("");
    match s.to_ascii_lowercase().as_str() {
        "" | "auto" => return Choice::Auto,
        "off" | "none" | "software" | "cpu" => return Choice::Off,
        _ => {}
    }
    let (kind, device) = split_spec(s);
    if !KINDS.contains(&kind.as_str()) {
        return Choice::Auto;
    }
    let device = match kind.as_str() {
        "vaapi" | "qsv" => device.filter(|d| is_render_node(d)),
        _ => None,
    };
    Choice::Named { kind, device }
}

/// Why a setting is not one AmberSight knows (None: fine).
pub fn check(setting: Option<&str>) -> Option<String> {
    let s = setting.map(str::trim).unwrap_or("");
    if matches!(s.to_ascii_lowercase().as_str(), "" | "auto" | "off" | "none" | "software" | "cpu") {
        return None;
    }
    let (kind, device) = split_spec(s);
    if !KINDS.contains(&kind.as_str()) {
        return Some(format!("hardware decoding must be auto, off, vaapi or qsv (not \"{s}\")"));
    }
    match (kind.as_str(), device) {
        ("vaapi" | "qsv", Some(d)) if !is_render_node(&d) => Some(format!("hardware decoding: the device must be a render node such as /dev/dri/renderD128 (not \"{d}\")")),
        ("vaapi" | "qsv", _) => None,
        (_, Some(_)) => Some(format!("hardware decoding: {kind} takes no device")),
        _ => None,
    }
}

/// The setting as used, with a warning when it is not understood (it is
/// used as Auto and kept as written).
pub fn sanitized(setting: Option<&str>) -> (Choice, Option<String>) {
    (choice(setting), check(setting))
}

// ---------------------------------------------------------------------------
// Render nodes
// ---------------------------------------------------------------------------

#[derive(Clone, Debug, Serialize)]
pub struct Node {
    pub path: String,
    /// Kernel driver: i915 / xe (Intel), amdgpu, nvidia, ...
    pub driver: String,
}

impl Node {
    fn intel(&self) -> bool {
        matches!(self.driver.as_str(), "i915" | "xe")
    }
    fn rank(&self) -> u8 {
        match self.driver.as_str() {
            "i915" | "xe" => 0,
            "amdgpu" | "radeon" => 1,
            "nvidia" => 3,
            _ => 2,
        }
    }
}

/// Render nodes of this computer (Linux), best first for decoding: Intel,
/// then AMD, then others; NVIDIA last (it has no VAAPI driver of its own).
pub fn render_nodes() -> Vec<Node> {
    if !cfg!(target_os = "linux") {
        return Vec::new();
    }
    let mut v: Vec<Node> = std::fs::read_dir("/dev/dri")
        .map(|d| {
            d.filter_map(|e| e.ok())
                .map(|e| e.file_name().to_string_lossy().into_owned())
                .filter(|n| n.starts_with("renderD"))
                .map(|n| {
                    let driver = std::fs::read_link(format!("/sys/class/drm/{n}/device/driver"))
                        .ok()
                        .and_then(|p| p.file_name().map(|f| f.to_string_lossy().into_owned()))
                        .unwrap_or_default();
                    Node { path: format!("/dev/dri/{n}"), driver }
                })
                .collect()
        })
        .unwrap_or_default();
    v.sort_by(|a, b| a.rank().cmp(&b.rank()).then_with(|| a.path.cmp(&b.path)));
    v
}

// ---------------------------------------------------------------------------
// The start-up test
// ---------------------------------------------------------------------------

/// 8 frames of a 320x240 test picture, Annex B.
const SAMPLE_H264: &[u8] = include_bytes!("sample.h264");
const SAMPLE_HEVC: &[u8] = include_bytes!("sample.hevc");
const SAMPLE_FRAMES: usize = 8;
const GRAY_W: usize = 64;
const GRAY_H: usize = 48;
/// Mean difference per pixel (0-255) allowed between the card's pictures
/// and software decoding of the same sample (scaling differs a little).
const MAX_DIFF: f64 = 12.0;

/// One test decode.
#[derive(Clone, Debug, Serialize)]
pub struct Trial {
    pub kind: &'static str,
    pub device: String,
    pub codec: &'static str,
    pub ok: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    pub ms: u64,
}

static PROBE: OnceLock<Vec<Trial>> = OnceLock::new();
static PROBE_STARTED: AtomicBool = AtomicBool::new(false);

fn codec_name(c: Codec) -> &'static str {
    match c {
        Codec::H264 => "h264",
        Codec::H265 => "hevc",
    }
}

fn codec_label(c: &str) -> &'static str {
    if c == "hevc" { "H.265" } else { "H.264" }
}

/// Start the test decodes on a thread of their own, once (Linux with a
/// render node; elsewhere there is nothing to test).
pub fn probe_in_background() {
    if PROBE_STARTED.swap(true, Ordering::SeqCst) {
        return;
    }
    let nodes = render_nodes();
    if nodes.iter().all(|n| n.driver == "nvidia") {
        let _ = PROBE.set(Vec::new());
        return;
    }
    std::thread::spawn(move || {
        let _ = PROBE.set(run_probe(&nodes));
    });
}

pub fn probe_done() -> bool {
    PROBE.get().is_some()
}

/// The test results (None while it runs or when it was never started).
pub fn probe_results() -> Option<&'static [Trial]> {
    PROBE.get().map(|v| v.as_slice())
}

fn wait_probe(max: Duration) -> Option<&'static [Trial]> {
    if !PROBE_STARTED.load(Ordering::SeqCst) {
        return None;
    }
    let t0 = Instant::now();
    loop {
        if let Some(p) = PROBE.get() {
            return Some(p);
        }
        if t0.elapsed() >= max {
            return None;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

fn run_probe(nodes: &[Node]) -> Vec<Trial> {
    let t0 = Instant::now();
    let budget = Duration::from_secs(45);
    let mut out = Vec::new();
    for codec in [Codec::H264, Codec::H265] {
        let reference = match decode_gray(None, codec) {
            Ok(r) => r,
            Err(e) => {
                tracing::warn!("hardware decoding: software decoding of the {} test sample failed ({e}); not testing the graphics cards for it", codec_label(codec_name(codec)));
                continue;
            }
        };
        for n in nodes.iter().filter(|n| n.driver != "nvidia") {
            let kinds: &[&'static str] = if n.intel() { &["vaapi", "qsv"] } else { &["vaapi"] };
            for kind in kinds {
                let t = Instant::now();
                let r = if t0.elapsed() > budget {
                    Err("not tested (the test took too long)".to_string())
                } else {
                    decode_gray(Some(&format!("{kind}:{}", n.path)), codec).and_then(|g| compare(&reference, &g))
                };
                out.push(Trial { kind, device: n.path.clone(), codec: codec_name(codec), ok: r.is_ok(), reason: r.err(), ms: t.elapsed().as_millis() as u64 });
            }
        }
    }
    let ok: Vec<String> = out.iter().filter(|t| t.ok).map(|t| format!("{} {} {}", t.kind, t.device, codec_label(t.codec))).collect();
    let bad: Vec<String> = out.iter().filter(|t| !t.ok).map(|t| format!("{} {} {}: {}", t.kind, t.device, codec_label(t.codec), t.reason.as_deref().unwrap_or("failed"))).collect();
    tracing::info!(
        "hardware decoding: works with {}{}; tested in {} ms",
        if ok.is_empty() { "nothing".to_string() } else { ok.join(", ") },
        if bad.is_empty() { String::new() } else { format!("; failed: {}", bad.join("; ")) },
        t0.elapsed().as_millis()
    );
    out
}

/// Pictures alike enough (same count, small mean difference)?
fn compare(reference: &[u8], got: &[u8]) -> Result<(), String> {
    if got.len() != reference.len() {
        return Err(format!("{} of {} pictures decoded", got.len() / (GRAY_W * GRAY_H), reference.len() / (GRAY_W * GRAY_H)));
    }
    let diff: u64 = reference.iter().zip(got).map(|(a, b)| u64::from(a.abs_diff(*b))).sum();
    let mean = diff as f64 / reference.len().max(1) as f64;
    if mean > MAX_DIFF {
        return Err(format!("the pictures came out wrong (mean difference {mean:.0} from software decoding)"));
    }
    Ok(())
}

/// The first line ffmpeg printed that says something, without `[tag @ 0x..]`
/// prefixes, cut short.
pub fn reason_from_stderr(text: &str) -> Option<String> {
    for l in text.lines() {
        let mut l = l.trim();
        while l.starts_with('[') {
            match l.find(']') {
                Some(i) => l = l[i + 1..].trim_start(),
                None => break,
            }
        }
        // (It goes inside parentheses: no full stop of its own.)
        let l = l.trim_end_matches('.');
        if l.is_empty() || l.starts_with("ffmpeg version") || l.starts_with("built with") {
            continue;
        }
        let mut s: String = l.chars().take(140).collect();
        if l.chars().count() > 140 {
            s.push_str("...");
        }
        return Some(s);
    }
    None
}

/// Decode a test sample (on the card when `spec` names one) to 64x48 gray
/// pictures, the same way a camera's frames go to the decoder (stdin, the
/// same input and download arguments).
fn decode_gray(spec: Option<&str>, codec: Codec) -> Result<Vec<u8>, String> {
    use std::process::Stdio;
    let sample = match codec {
        Codec::H264 => SAMPLE_H264,
        Codec::H265 => SAMPLE_HEVC,
    };
    let mut a: Vec<String> = ["-hide_banner", "-loglevel", "error"].map(String::from).into();
    if let Some(s) = spec {
        a.extend(crate::ingest::decode::hw_input_args(s, Some(codec)).map_err(|e| e.to_string())?);
    }
    a.extend(["-f", codec.ffmpeg_format(), "-i", "-"].map(String::from));
    let scale = format!("scale={GRAY_W}:{GRAY_H},format=gray");
    a.push("-vf".into());
    a.push(if spec.is_some() { format!("{},{scale}", crate::ingest::decode::HW_DOWNLOAD) } else { scale });
    a.extend(["-f", "rawvideo", "-"].map(String::from));
    let mut child = crate::child::ffmpeg()
        .args(&a)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| format!("could not start ffmpeg: {e}"))?;
    crate::child::adopt(&child);
    let mut stdin = child.stdin.take().ok_or("no stdin")?;
    let mut stdout = child.stdout.take().ok_or("no stdout")?;
    let mut stderr = child.stderr.take().ok_or("no stderr")?;
    let w = std::thread::spawn(move || {
        let _ = stdin.write_all(sample);
    });
    let r = std::thread::spawn(move || {
        let mut b = Vec::new();
        let _ = (&mut stdout).take(4 << 20).read_to_end(&mut b);
        b
    });
    let e = std::thread::spawn(move || {
        let mut b = String::new();
        let _ = (&mut stderr).take(64 << 10).read_to_string(&mut b);
        b
    });
    let t0 = Instant::now();
    let status = loop {
        match child.try_wait() {
            Ok(Some(s)) => break Some(s),
            Ok(None) if t0.elapsed() < Duration::from_secs(10) => std::thread::sleep(Duration::from_millis(20)),
            _ => {
                let _ = child.kill();
                let _ = child.wait();
                break None;
            }
        }
    };
    let _ = w.join();
    let out = r.join().unwrap_or_default();
    let err = e.join().unwrap_or_default();
    match status {
        None => Err("the test decode did not finish within 10 s".into()),
        Some(s) if !s.success() => Err(reason_from_stderr(&err).unwrap_or_else(|| format!("ffmpeg failed ({s})"))),
        Some(_) if out.len() != SAMPLE_FRAMES * GRAY_W * GRAY_H => Err(format!(
            "{} of {SAMPLE_FRAMES} pictures decoded{}",
            out.len() / (GRAY_W * GRAY_H),
            reason_from_stderr(&err).map(|r| format!(" ({r})")).unwrap_or_default()
        )),
        Some(_) => Ok(out),
    }
}

// ---------------------------------------------------------------------------
// Failures while running
// ---------------------------------------------------------------------------

static RUNTIME: crate::transcode::HwMemory = crate::transcode::HwMemory::new();

#[derive(Clone)]
struct Failure {
    camera: String,
    reason: String,
    at: Instant,
}

fn failures() -> &'static Mutex<HashMap<String, Failure>> {
    static F: OnceLock<Mutex<HashMap<String, Failure>>> = OnceLock::new();
    F.get_or_init(Default::default)
}

fn runtime_key(kind: &str, device: &str, codec: Option<Codec>) -> String {
    format!("decode|{kind}|{device}|{}", codec.map(codec_name).unwrap_or("any"))
}

/// A hardware decoder failed while software decoding of the same stream
/// worked: skip that card for that codec for an hour (all cameras).
pub fn note_failure(spec: &str, codec: Option<Codec>, camera: &str, reason: &str) {
    let (kind, device) = split_spec(spec);
    let device = device.unwrap_or_default();
    let key = runtime_key(&kind, &device, codec);
    if RUNTIME.failed_at(&key, Instant::now()) {
        tracing::warn!(
            "camera '{camera}': hardware decoding ({kind}{}) failed for {} ({reason}); decoding on the processor, and not using it for the next hour",
            if device.is_empty() { String::new() } else { format!(" on {device}") },
            codec.map(|c| codec_label(codec_name(c))).unwrap_or("this stream")
        );
    }
    failures().lock().unwrap().insert(key, Failure { camera: camera.to_string(), reason: reason.to_string(), at: Instant::now() });
}

fn runtime_failed(kind: &str, device: &str, codec: Option<Codec>) -> bool {
    RUNTIME.skipped_at(&runtime_key(kind, device, codec), Instant::now())
}

// ---------------------------------------------------------------------------
// Choosing
// ---------------------------------------------------------------------------

/// What a decoder uses.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Resolved {
    /// The way and device for ffmpeg ("vaapi:/dev/dri/renderD129"); None:
    /// the processor.
    pub spec: Option<String>,
    /// Hardware decoding was wanted but is not used: why.
    pub why_not: Option<String>,
    /// The start-up test had not finished (try again at the next restart).
    pub pending: bool,
}

/// How to say a spec in Status: "VAAPI on /dev/dri/renderD129".
pub fn describe(spec: &str) -> String {
    let (kind, device) = split_spec(spec);
    let k = match kind.as_str() {
        "vaapi" => "VAAPI".to_string(),
        "qsv" => "Quick Sync".to_string(),
        "cuda" => "CUDA".to_string(),
        "d3d11va" => "Direct3D 11".to_string(),
        "dxva2" => "DXVA2".to_string(),
        "videotoolbox" => "VideoToolbox".to_string(),
        "vulkan" => "Vulkan".to_string(),
        k => k.to_string(),
    };
    match device {
        Some(d) => format!("{k} on {d}"),
        None => k,
    }
}

fn trial_for<'a>(trials: &'a [Trial], kind: &str, device: &str, codec: Option<Codec>) -> Option<&'a Trial> {
    let c = codec.map(codec_name).unwrap_or("h264");
    trials.iter().find(|t| t.kind == kind && t.device == device && t.codec == c)
}

/// The decoder for a stream of this codec (None: not known yet, as for the
/// ffmpeg pipeline) under this setting. Waits up to `wait` for the start-up
/// test.
pub fn resolve(setting: Option<&str>, codec: Option<Codec>, wait: Duration) -> Resolved {
    resolve_with(choice(setting), codec, render_nodes(), || wait_probe(wait), PROBE_STARTED.load(Ordering::SeqCst))
}

fn resolve_with<'a>(
    choice: Choice,
    codec: Option<Codec>,
    nodes: Vec<Node>,
    probe: impl FnOnce() -> Option<&'a [Trial]>,
    started: bool,
) -> Resolved {
    let label = codec.map(|c| codec_label(codec_name(c))).unwrap_or("this stream");
    match choice {
        Choice::Off => Resolved::default(),
        Choice::Auto => {
            if !cfg!(target_os = "linux") && !cfg!(test) {
                return Resolved::default();
            }
            let nodes: Vec<Node> = nodes.into_iter().filter(|n| n.driver != "nvidia").collect();
            if nodes.is_empty() || !started {
                return Resolved::default();
            }
            let Some(trials) = probe() else {
                return Resolved { spec: None, why_not: Some("the graphics card test is still running".into()), pending: true };
            };
            for n in &nodes {
                if trial_for(trials, "vaapi", &n.path, codec).is_some_and(|t| t.ok) && !runtime_failed("vaapi", &n.path, codec) {
                    return Resolved { spec: Some(format!("vaapi:{}", n.path)), why_not: None, pending: false };
                }
            }
            // Why not: the first node's own reason.
            let why = nodes
                .iter()
                .find_map(|n| {
                    if runtime_failed("vaapi", &n.path, codec) {
                        return Some(format!("hardware decoding failed on {} for {label} while running; using the processor", n.path));
                    }
                    trial_for(trials, "vaapi", &n.path, codec)
                        .map(|t| format!("hardware decoding unavailable on {} ({}); using the processor", n.path, t.reason.as_deref().unwrap_or("failed")))
                })
                .unwrap_or_else(|| "no graphics card decodes this stream; using the processor".into());
            Resolved { spec: None, why_not: Some(why), pending: false }
        }
        Choice::Named { kind, device } => {
            let mut kind = kind;
            let trials = if matches!(kind.as_str(), "vaapi" | "qsv") && started { probe() } else { None };
            let device = match (kind.as_str(), device) {
                (_, Some(d)) => Some(d),
                ("vaapi" | "qsv", None) => {
                    // The first node where this way worked, else the first node.
                    let worked = trials.and_then(|t| nodes.iter().find(|n| trial_for(t, &kind, &n.path, codec).is_some_and(|x| x.ok)).map(|n| n.path.clone()));
                    Some(worked.or_else(|| nodes.iter().find(|n| n.driver != "nvidia").map(|n| n.path.clone())).unwrap_or_else(|| "/dev/dri/renderD128".into()))
                }
                _ => None,
            };
            if let (Some(t), Some(d)) = (trials, device.as_deref()) {
                if let Some(x) = trial_for(t, &kind, d, codec).filter(|x| !x.ok) {
                    return Resolved {
                        spec: None,
                        why_not: Some(format!("{} failed the test ({}); using the processor", describe(&format!("{kind}:{d}")), x.reason.as_deref().unwrap_or("failed"))),
                        pending: false,
                    };
                }
            }
            if runtime_failed(&kind, device.as_deref().unwrap_or(""), codec) {
                return Resolved {
                    spec: None,
                    why_not: Some(format!("{} failed for {label} while running; using the processor for now", describe(&match &device {
                        Some(d) => format!("{kind}:{d}"),
                        None => kind.clone(),
                    }))),
                    pending: false,
                };
            }
            // The ffmpeg pipeline does not know the codec before it starts,
            // and Quick Sync decoding needs it named: VAAPI drives the same
            // decoder on the same chip.
            if kind == "qsv" && codec.is_none() {
                kind = "vaapi".into();
            }
            Resolved {
                spec: Some(match device {
                    Some(d) => format!("{kind}:{d}"),
                    None => kind,
                }),
                why_not: None,
                pending: false,
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Status
// ---------------------------------------------------------------------------

/// Notes for Status: render nodes that cannot decode, and failures while
/// running (in the last hour).
pub fn notes() -> Vec<String> {
    let mut out = Vec::new();
    let nodes: Vec<Node> = render_nodes().into_iter().filter(|n| n.driver != "nvidia").collect();
    if let Some(trials) = probe_results() {
        for codec in ["h264", "hevc"] {
            let works: Vec<&Node> = nodes.iter().filter(|n| trials.iter().any(|t| t.kind == "vaapi" && t.device == n.path && t.codec == codec && t.ok)).collect();
            for n in &nodes {
                let Some(t) = trials.iter().find(|t| t.kind == "vaapi" && t.device == n.path && t.codec == codec && !t.ok) else { continue };
                // A node that fails both codecs for one reason is said once.
                if codec == "hevc" && trials.iter().any(|x| x.kind == "vaapi" && x.device == n.path && x.codec == "h264" && !x.ok && x.reason == t.reason) {
                    continue;
                }
                let both = codec == "h264" && trials.iter().any(|x| x.kind == "vaapi" && x.device == n.path && x.codec == "hevc" && !x.ok && x.reason == t.reason);
                let what = if both { String::new() } else { format!(" for {}", codec_label(codec)) };
                let instead = match works.first() {
                    Some(w) => format!("{} is used instead", w.path),
                    None => "using the processor".to_string(),
                };
                out.push(format!("Hardware decoding unavailable on {}{what} ({}); {instead}", n.path, t.reason.as_deref().unwrap_or("failed")));
            }
        }
    }
    let now = Instant::now();
    let mut f: Vec<(String, Failure)> = failures().lock().unwrap().iter().filter(|(_, v)| now.saturating_duration_since(v.at) < Duration::from_secs(3600)).map(|(k, v)| (k.clone(), v.clone())).collect();
    f.sort_by(|a, b| a.0.cmp(&b.0));
    for (k, v) in f {
        let parts: Vec<&str> = k.split('|').collect();
        let (kind, device, codec) = (parts.get(1).copied().unwrap_or(""), parts.get(2).copied().unwrap_or(""), parts.get(3).copied().unwrap_or(""));
        let spec = if device.is_empty() { kind.to_string() } else { format!("{kind}:{device}") };
        out.push(format!(
            "Hardware decoding ({}) failed{} on {} ({}); using the processor for the next hour",
            describe(&spec),
            if codec == "any" { String::new() } else { format!(" for {}", codec_label(codec)) },
            v.camera,
            v.reason
        ));
    }
    out
}

/// `GET /api/hwdecode`: the render nodes, what the test found on each, and
/// what Auto uses.
pub fn status_json() -> serde_json::Value {
    let nodes = render_nodes();
    let trials = probe_results();
    let list: Vec<serde_json::Value> = nodes
        .iter()
        .map(|n| {
            let mut v = serde_json::json!({ "path": n.path, "driver": n.driver });
            if let Some(t) = trials {
                let mine: Vec<&Trial> = t.iter().filter(|x| x.device == n.path).collect();
                v["tests"] = serde_json::to_value(mine).unwrap_or_default();
            }
            v
        })
        .collect();
    let auto = |c: Codec| if probe_done() { resolve(None, Some(c), Duration::ZERO).spec.map(|s| describe(&s)) } else { None };
    serde_json::json!({
        "auto_supported": cfg!(target_os = "linux"),
        "tested": probe_done(),
        "nodes": list,
        "auto": { "h264": auto(Codec::H264), "hevc": auto(Codec::H265) },
        "notes": notes(),
    })
}

/// How a camera's motion decoding runs now (Status, camera settings).
#[derive(Clone, Debug, Default, Serialize)]
pub struct CamDecode {
    /// The stream decoded for motion: "main" or "sub" (None: not running).
    pub stream: Option<&'static str>,
    pub codec: Option<&'static str>,
    pub width: u32,
    pub height: u32,
    /// "VAAPI on /dev/dri/renderD129" while the card decodes it.
    pub gpu: Option<String>,
    /// Hardware decoding was wanted but is not used: why.
    pub note: Option<String>,
    /// The main stream's size and codec, when known (built-in reader).
    pub main: Option<(u32, u32, &'static str)>,
    /// The ffmpeg processes of the ffmpeg pipeline (the built-in reader's
    /// decoder is counted from `stream`).
    #[serde(skip)]
    pub procs: Vec<&'static str>,
    /// The ffmpeg pipeline re-encodes the full-size recording: the Status
    /// text saying so, and whether a hardware encoder does it.
    pub reencode: Option<String>,
    pub reencode_hw: bool,
    /// The encoder of that recording ("NVENC", "VAAPI on ...", "libx264"),
    /// and why it is not on the graphics card when it is not.
    pub encoder: Option<String>,
    pub encoder_note: Option<String>,
}

pub fn codec_text(h265: bool) -> &'static str {
    if h265 { "H.265" } else { "H.264" }
}

impl CamDecode {
    /// Not running: forget the session's details (keep the main stream's
    /// size, which the warning about a missing sub stream uses).
    pub fn stopped(&mut self) {
        let main = self.main;
        *self = CamDecode { main, ..Default::default() };
    }

    /// For the API: where it decodes now, and whether motion decodes a
    /// large main stream on the processor.
    pub fn json(&self) -> serde_json::Value {
        let heavy = self.heavy_main_on_cpu();
        serde_json::json!({
            "stream": self.stream,
            "on": self.stream.map(|_| if self.gpu.is_some() { "gpu" } else { "cpu" }),
            "gpu": self.gpu,
            "codec": self.codec,
            "width": (self.width > 0).then_some(self.width),
            "height": (self.height > 0).then_some(self.height),
            "note": self.note,
            "main": self.main.map(|(w, h, c)| serde_json::json!({ "width": w, "height": h, "codec": c })),
            "heavy_main": heavy.is_some(),
            "reencode": self.reencode,
            "reencode_hw": self.reencode_hw,
            "encoder": self.encoder,
            "encoder_note": self.encoder_note,
        })
    }

    /// Larger than about 1080p, on the processor.
    pub fn heavy_main_on_cpu(&self) -> Option<(u32, u32, &'static str)> {
        if self.stream != Some("main") || self.gpu.is_some() {
            return None;
        }
        let (w, h, c) = self.main.unwrap_or((self.width, self.height, self.codec.unwrap_or("")));
        (u64::from(w) * u64::from(h) > 1920 * 1080 * 23 / 20).then_some((w, h, c))
    }
}

// ---------------------------------------------------------------------------
// Sub stream suggestions
// ---------------------------------------------------------------------------

/// A likely sub-stream address for a main-stream address, from patterns
/// common cameras use (Reolink, Dahua / Amcrest, Hikvision).
pub fn sub_from_pattern(main: &str) -> Option<String> {
    let (base, query) = match main.split_once('?') {
        Some((b, q)) => (b, Some(q)),
        None => (main, None),
    };
    let join = |b: String, q: Option<String>| match q {
        Some(q) => format!("{b}?{q}"),
        None => b,
    };
    // Reolink: h264Preview_01_main, h265Preview_01_main, Preview_01_main.
    for (m, s) in [("Preview_01_main", "Preview_01_sub"), ("Preview_02_main", "Preview_02_sub"), ("Preview_03_main", "Preview_03_sub"), ("Preview_04_main", "Preview_04_sub")] {
        if let Some(i) = base.rfind(m) {
            let prefix = &base[..i];
            // h265Preview_01_main has its sub stream as h264Preview_01_sub.
            let prefix = prefix.strip_suffix("h265").map(|p| format!("{p}h264")).unwrap_or_else(|| prefix.to_string());
            return Some(join(format!("{prefix}{s}{}", &base[i + m.len()..]), query.map(String::from)));
        }
    }
    // Dahua / Amcrest: ...realmonitor?channel=1&subtype=0
    if let Some(q) = query {
        if q.split('&').any(|p| p == "subtype=0") {
            let q2: Vec<String> = q.split('&').map(|p| if p == "subtype=0" { "subtype=1".to_string() } else { p.to_string() }).collect();
            return Some(join(base.to_string(), Some(q2.join("&"))));
        }
    }
    // Hikvision: /Streaming/Channels/101 (channel 1, stream 1) -> 102.
    let lower = base.to_ascii_lowercase();
    if let Some(i) = lower.rfind("/streaming/channels/") {
        let start = i + "/streaming/channels/".len();
        let tail = &base[start..];
        let digits: String = tail.chars().take_while(|c| c.is_ascii_digit()).collect();
        if digits.len() >= 3 && digits.ends_with('1') && tail[digits.len()..].trim_matches('/').is_empty() {
            let sub = format!("{}2", &digits[..digits.len() - 1]);
            return Some(join(format!("{}{sub}{}", &base[..start], &tail[digits.len()..]), query.map(String::from)));
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn node(p: &str, d: &str) -> Node {
        Node { path: p.into(), driver: d.into() }
    }

    fn trial(kind: &'static str, dev: &str, codec: &'static str, ok: bool) -> Trial {
        Trial { kind, device: dev.into(), codec, ok, reason: (!ok).then(|| "Failed to initialise VAAPI connection".into()), ms: 1 }
    }

    #[test]
    fn settings_are_read() {
        assert_eq!(choice(None), Choice::Auto);
        assert_eq!(choice(Some("auto")), Choice::Auto);
        assert_eq!(choice(Some("Off")), Choice::Off);
        assert_eq!(choice(Some("vaapi")), Choice::Named { kind: "vaapi".into(), device: None });
        assert_eq!(choice(Some("vaapi:/dev/dri/renderD129")), Choice::Named { kind: "vaapi".into(), device: Some("/dev/dri/renderD129".into()) });
        assert_eq!(choice(Some("qsv:/dev/dri/renderD128")), Choice::Named { kind: "qsv".into(), device: Some("/dev/dri/renderD128".into()) });
        // A device that is not a render node is not passed on.
        assert_eq!(choice(Some("vaapi:/etc/passwd")), Choice::Named { kind: "vaapi".into(), device: None });
        assert_eq!(choice(Some("bogus")), Choice::Auto);
        assert!(check(Some("bogus")).is_some());
        assert!(check(Some("vaapi:/tmp/x")).is_some());
        assert!(check(Some("cuda:/dev/dri/renderD128")).is_some());
        for ok in [None, Some("auto"), Some("off"), Some("vaapi"), Some("qsv"), Some("vaapi:/dev/dri/renderD130"), Some("d3d11va"), Some("videotoolbox")] {
            assert_eq!(check(ok), None, "{ok:?}");
        }
        let (c, w) = sanitized(Some("gpu"));
        assert_eq!(c, Choice::Auto);
        assert!(w.is_some());
    }

    #[test]
    fn auto_takes_the_first_node_that_passed() {
        let nodes = vec![node("/dev/dri/renderD128", "xe"), node("/dev/dri/renderD129", "amdgpu"), node("/dev/dri/renderD130", "xe")];
        let trials = vec![
            trial("vaapi", "/dev/dri/renderD128", "h264", false),
            trial("vaapi", "/dev/dri/renderD128", "hevc", false),
            trial("vaapi", "/dev/dri/renderD129", "h264", true),
            trial("vaapi", "/dev/dri/renderD129", "hevc", true),
            trial("vaapi", "/dev/dri/renderD130", "h264", false),
            trial("vaapi", "/dev/dri/renderD130", "hevc", false),
        ];
        let r = resolve_with(Choice::Auto, Some(Codec::H265), nodes.clone(), || Some(&trials[..]), true);
        assert_eq!(r.spec.as_deref(), Some("vaapi:/dev/dri/renderD129"));
        // Nothing passed: the processor, with the first node's reason.
        let bad: Vec<Trial> = trials.iter().filter(|t| t.device != "/dev/dri/renderD129").cloned().collect();
        let r = resolve_with(Choice::Auto, Some(Codec::H264), nodes.clone(), || Some(&bad[..]), true);
        assert_eq!(r.spec, None);
        assert!(r.why_not.as_deref().unwrap().contains("unavailable on /dev/dri/renderD128 (Failed to initialise"), "{r:?}");
        // No render node, or an NVIDIA one only: nothing to say.
        assert_eq!(resolve_with(Choice::Auto, Some(Codec::H264), vec![], || Some(&trials[..]), true), Resolved::default());
        assert_eq!(resolve_with(Choice::Auto, Some(Codec::H264), vec![node("/dev/dri/renderD128", "nvidia")], || Some(&trials[..]), true), Resolved::default());
        // Still testing.
        let r = resolve_with(Choice::Auto, Some(Codec::H264), nodes.clone(), || None, true);
        assert!(r.pending && r.spec.is_none());
        // Off is off.
        assert_eq!(resolve_with(Choice::Off, Some(Codec::H264), nodes, || Some(&trials[..]), true), Resolved::default());
    }

    #[test]
    fn a_named_way_that_failed_the_test_is_not_used() {
        let nodes = vec![node("/dev/dri/renderD128", "xe"), node("/dev/dri/renderD129", "amdgpu")];
        let trials = vec![trial("vaapi", "/dev/dri/renderD128", "h264", false), trial("vaapi", "/dev/dri/renderD129", "h264", true), trial("qsv", "/dev/dri/renderD128", "h264", false)];
        let named = |s: &str| choice(Some(s));
        let r = resolve_with(named("vaapi:/dev/dri/renderD128"), Some(Codec::H264), nodes.clone(), || Some(&trials[..]), true);
        assert_eq!(r.spec, None);
        assert!(r.why_not.unwrap().starts_with("VAAPI on /dev/dri/renderD128 failed the test"));
        // No device named: the node where it worked.
        let r = resolve_with(named("vaapi"), Some(Codec::H264), nodes.clone(), || Some(&trials[..]), true);
        assert_eq!(r.spec.as_deref(), Some("vaapi:/dev/dri/renderD129"));
        // Quick Sync for the ffmpeg pipeline (codec unknown) is VAAPI on that node.
        let r = resolve_with(named("qsv:/dev/dri/renderD129"), None, nodes.clone(), || Some(&[][..]), true);
        assert_eq!(r.spec.as_deref(), Some("vaapi:/dev/dri/renderD129"));
        let r = resolve_with(named("qsv:/dev/dri/renderD129"), Some(Codec::H264), nodes.clone(), || Some(&[][..]), true);
        assert_eq!(r.spec.as_deref(), Some("qsv:/dev/dri/renderD129"));
        // Names without a device pass through.
        assert_eq!(resolve_with(named("d3d11va"), Some(Codec::H264), vec![], || None, false).spec.as_deref(), Some("d3d11va"));
    }

    #[test]
    fn a_failure_while_running_is_remembered_per_device_and_codec() {
        let nodes = vec![node("/dev/dri/renderD177", "amdgpu"), node("/dev/dri/renderD178", "amdgpu")];
        let trials = vec![
            trial("vaapi", "/dev/dri/renderD177", "hevc", true),
            trial("vaapi", "/dev/dri/renderD177", "h264", true),
            trial("vaapi", "/dev/dri/renderD178", "hevc", true),
        ];
        note_failure("vaapi:/dev/dri/renderD177", Some(Codec::H265), "Porch", "decoding errors");
        let r = resolve_with(Choice::Auto, Some(Codec::H265), nodes.clone(), || Some(&trials[..]), true);
        assert_eq!(r.spec.as_deref(), Some("vaapi:/dev/dri/renderD178"), "the next node");
        let r = resolve_with(Choice::Auto, Some(Codec::H264), nodes.clone(), || Some(&trials[..]), true);
        assert_eq!(r.spec.as_deref(), Some("vaapi:/dev/dri/renderD177"), "H.264 still on the first");
        assert!(notes().iter().any(|n| n.contains("failed for H.265 on Porch (decoding errors)")), "{:?}", notes());
    }

    #[test]
    fn pictures_are_compared() {
        let a = vec![100u8; GRAY_W * GRAY_H * 2];
        assert!(compare(&a, &a).is_ok());
        assert!(compare(&a, &a[..GRAY_W * GRAY_H]).is_err());
        let green = vec![0u8; a.len()];
        assert!(compare(&a, &green).unwrap_err().contains("wrong"));
        let near: Vec<u8> = a.iter().map(|x| x + 4).collect();
        assert!(compare(&a, &near).is_ok());
    }

    #[test]
    fn reasons_lose_their_tags() {
        let t = "[AVHWDeviceContext @ 0x5f] Failed to initialise VAAPI connection: -1 (unknown libva error).\nDevice creation failed: -5.\n";
        assert_eq!(reason_from_stderr(t).unwrap(), "Failed to initialise VAAPI connection: -1 (unknown libva error)");
        assert_eq!(reason_from_stderr("\n\n"), None);
    }

    #[test]
    fn heavy_main_streams_are_found() {
        let mut d = CamDecode { stream: Some("main"), codec: Some("H.265"), width: 3840, height: 2160, ..Default::default() };
        assert_eq!(d.heavy_main_on_cpu(), Some((3840, 2160, "H.265")));
        d.gpu = Some("VAAPI on /dev/dri/renderD128".into());
        assert_eq!(d.heavy_main_on_cpu(), None, "the card decodes it");
        let small = CamDecode { stream: Some("main"), codec: Some("H.264"), width: 1920, height: 1080, ..Default::default() };
        assert_eq!(small.heavy_main_on_cpu(), None, "1080p is fine");
        let sub = CamDecode { stream: Some("sub"), width: 3840, height: 2160, ..Default::default() };
        assert_eq!(sub.heavy_main_on_cpu(), None);
    }

    #[test]
    fn sub_streams_from_known_patterns() {
        let s = |u: &str| sub_from_pattern(u);
        assert_eq!(s("rtsp://10.0.0.5:554/h264Preview_01_main").as_deref(), Some("rtsp://10.0.0.5:554/h264Preview_01_sub"));
        assert_eq!(s("rtsp://10.0.0.5:554/h265Preview_01_main").as_deref(), Some("rtsp://10.0.0.5:554/h264Preview_01_sub"));
        assert_eq!(s("rtsp://10.0.0.5/Preview_01_main").as_deref(), Some("rtsp://10.0.0.5/Preview_01_sub"));
        assert_eq!(s("rtsp://10.0.0.6:554/cam/realmonitor?channel=1&subtype=0").as_deref(), Some("rtsp://10.0.0.6:554/cam/realmonitor?channel=1&subtype=1"));
        assert_eq!(s("rtsp://10.0.0.7:554/Streaming/Channels/101").as_deref(), Some("rtsp://10.0.0.7:554/Streaming/Channels/102"));
        assert_eq!(s("rtsp://10.0.0.7:554/ISAPI/Streaming/channels/201/").as_deref(), Some("rtsp://10.0.0.7:554/ISAPI/Streaming/channels/202/"));
        assert_eq!(s("rtsp://10.0.0.7:554/Streaming/Channels/102"), None, "already the sub stream");
        assert_eq!(s("rtsp://10.0.0.8/live"), None);
        assert_eq!(s("rtsp://10.0.0.6/cam/realmonitor?channel=1&subtype=1"), None);
    }
}
