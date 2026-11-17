//! Hardware video decoding for motion analysis.
//!
//! A camera's `hwaccel` setting says how the stream analysed for motion is
//! decoded: unset or "auto", "off" (the processor), or a named way: "cuda"
//! (NVIDIA NVDEC), "vaapi", "vaapi:/dev/dri/renderD129", "qsv",
//! "qsv:/dev/dri/renderD128", and the older pass-through names "vulkan",
//! "videotoolbox", "d3d11va", "dxva2".
//!
//! Auto follows the server-wide preference (`[hwdecode] prefer`, Settings,
//! System, Hardware) and uses the first way, in that order, that passed a
//! real test decode at start: a short H.264 and H.265 sample is decoded the
//! same way a camera is (the same ffmpeg arguments, fed through stdin) and
//! the pictures are compared with software decoding of the same sample. A
//! way that cannot start, or decodes wrong pictures, is never used by Auto.
//!
//! The order (`candidates`): "auto" and "nvidia": an NVIDIA card (NVDEC,
//! through CUDA) first, then each Intel or AMD render node. "intel": the
//! render nodes first, NVIDIA last. "off": the processor. On an Intel node
//! of Gen12 or newer (Tiger Lake, Rocket Lake, Alder Lake, Arc and later)
//! Quick Sync comes before VAAPI: VAAPI decoding on those chips can fail
//! with "Failed to sync surface" where Quick Sync, the same decoder through
//! Intel's own library, works. Older Intel: VAAPI, then Quick Sync. AMD:
//! VAAPI.
//!
//! A decoder that fails later (stops, stalls, or reports a burst of errors)
//! is replaced by software decoding at once (`ingest::decode`). That camera
//! then skips that way for 15 minutes, 30 the next time, and so on up to 4
//! hours (`CamFail`); Auto meanwhile uses the next way that works, and moves
//! back when the time is up. A way that stopped or stalled while software
//! decoding of the same stream then worked is skipped by every camera for an
//! hour for that codec (`transcode::HwMemory`); that memory is per way, so
//! a failed VAAPI does not keep a working Quick Sync or NVDEC from use.
//!
//! Windows: Auto uses NVDEC when an NVIDIA card passed the test, else the
//! processor (d3d11va can still be named per camera). macOS: the processor.

use crate::ingest::decode::Codec;
use serde::Serialize;
use std::collections::HashMap;
use std::io::{Read, Write};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicU8, Ordering};
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
        return Some(format!("hardware decoding must be auto, off, cuda, vaapi or qsv (not \"{s}\")"));
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
// The server-wide preference
// ---------------------------------------------------------------------------

mod vram;

/// What Auto prefers (`[hwdecode] prefer`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Prefer {
    /// NVIDIA, then Intel (Quick Sync on Gen12+) and AMD, then the processor.
    Auto,
    /// NVIDIA first (today the same order as Auto).
    Nvidia,
    /// Intel and AMD first, NVIDIA last.
    Intel,
    /// The processor (cameras that name a way keep it).
    Off,
}

pub const PREFER_NAMES: [&str; 4] = ["auto", "nvidia", "intel", "off"];

impl Prefer {
    pub fn parse(s: &str) -> Option<Prefer> {
        match s.trim().to_ascii_lowercase().as_str() {
            "" | "auto" => Some(Prefer::Auto),
            "nvidia" => Some(Prefer::Nvidia),
            "intel" => Some(Prefer::Intel),
            "off" => Some(Prefer::Off),
            _ => None,
        }
    }
    pub fn name(self) -> &'static str {
        PREFER_NAMES[self.code() as usize]
    }
    fn code(self) -> u8 {
        match self {
            Prefer::Auto => 0,
            Prefer::Nvidia => 1,
            Prefer::Intel => 2,
            Prefer::Off => 3,
        }
    }
    fn from_code(c: u8) -> Prefer {
        match c {
            1 => Prefer::Nvidia,
            2 => Prefer::Intel,
            3 => Prefer::Off,
            _ => Prefer::Auto,
        }
    }
}

/// `[hwdecode]` in the config.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, serde::Deserialize)]
pub struct HwDecodeConfig {
    /// "auto" (default), "nvidia", "intel" or "off".
    #[serde(default = "default_prefer")]
    pub prefer: String,
    /// Auto starts a camera on NVDEC only while the NVIDIA card keeps this
    /// much video memory (MB) free after it (and the decoder's estimate); 0
    /// turns the check off. "nvidia" and a camera's own "cuda" ignore it.
    #[serde(default = "default_min_free")]
    pub nvidia_min_free_mb: u32,
}

pub const DEFAULT_MIN_FREE_MB: u32 = 1536;
pub const MAX_MIN_FREE_MB: u32 = 1_000_000;

fn default_prefer() -> String {
    "auto".into()
}

fn default_min_free() -> u32 {
    DEFAULT_MIN_FREE_MB
}

impl Default for HwDecodeConfig {
    fn default() -> Self {
        HwDecodeConfig { prefer: default_prefer(), nvidia_min_free_mb: DEFAULT_MIN_FREE_MB }
    }
}

impl HwDecodeConfig {
    pub fn is_default(&self) -> bool {
        *self == HwDecodeConfig::default()
    }
    /// The preference as used, and a warning when the value is not one of
    /// the four (Auto is used; the value is kept as written).
    pub fn sanitized(&self) -> (Prefer, Option<String>) {
        match Prefer::parse(&self.prefer) {
            Some(p) => (p, None),
            None => (Prefer::Auto, Some(format!("[hwdecode] prefer must be auto, nvidia, intel or off (not \"{}\"); \"auto\" is used", self.prefer))),
        }
    }
}

static PREFER: AtomicU8 = AtomicU8::new(0);
static GENERATION: AtomicU64 = AtomicU64::new(0);

/// The preference in use.
pub fn prefer() -> Prefer {
    Prefer::from_code(PREFER.load(Ordering::SeqCst))
}

/// Use this preference from now on. When it changes, the cameras' own
/// failure memory starts afresh and `generation` moves on: decoders that
/// follow Auto look again at their next keyframe and move when the answer
/// differs. Returns whether it changed.
pub fn set_prefer(p: Prefer) -> bool {
    if PREFER.swap(p.code(), Ordering::SeqCst) == p.code() {
        return false;
    }
    setting_changed();
    true
}

/// A Hardware setting changed: the cameras' own failure memory starts
/// afresh, the guard reads the card again, and decoders that follow Auto
/// look again.
fn setting_changed() {
    cam_fails().lock().unwrap().clear();
    guard_reset();
    GENERATION.fetch_add(1, Ordering::SeqCst);
}

static MIN_FREE_MB: AtomicU64 = AtomicU64::new(DEFAULT_MIN_FREE_MB as u64);
/// Cameras with a decoder running on NVDEC (once per decoder: an
/// `NvdecHold` each), and cameras the guard turned away.
static HELD: Mutex<Vec<String>> = Mutex::new(Vec::new());
static DENIED: Mutex<Vec<String>> = Mutex::new(Vec::new());

/// A setting changed: cameras turned away are judged afresh. The cameras on
/// NVDEC stay on the list (their decoders still run there and the card's
/// reading already counts their memory), and the card is read again so
/// memory counted for decoders started since the last reading is not
/// counted twice.
fn guard_reset() {
    DENIED.lock().unwrap().clear();
    #[cfg(not(test))]
    if vram::reading() != vram::Reading::Missing {
        // (Not on this thread: the settings are saved under a lock, and
        // nvidia-smi may take seconds. At most one read runs at a time.)
        vram::refresh_in_background();
    }
}

/// Tests: a place on the card for this camera, without taking memory.
#[cfg(test)]
pub fn hold_for_test(camera: &str) -> NvdecHold {
    HELD.lock().unwrap().push(camera.to_string());
    NvdecHold(camera.to_string())
}

/// Tests: whether the guard counts this camera as on the card.
#[cfg(test)]
pub fn held_for_test(camera: &str) -> bool {
    list_has(&HELD, camera)
}

/// A decoder of a camera running on NVDEC: while it lives, the headroom
/// guard counts the camera as on the card (it stays whatever is free).
/// Dropped where the decoder stops or falls back to the processor.
#[derive(Debug)]
pub struct NvdecHold(String);

impl Drop for NvdecHold {
    fn drop(&mut self) {
        let mut g = HELD.lock().unwrap();
        if let Some(i) = g.iter().position(|x| *x == self.0) {
            g.remove(i);
        }
    }
}

/// A decoder of this camera actually started with `r` (call only there, not
/// for a look at what Auto would pick): on NVDEC it takes the card's memory
/// (counted in the reading until the next one) and holds the camera's place;
/// elsewhere the guard remembers whether it turned the camera away.
pub fn started(camera: &str, r: &Resolved) -> Option<NvdecHold> {
    if camera.is_empty() {
        return None;
    }
    if r.spec.as_deref() == Some("cuda") {
        let mut held = HELD.lock().unwrap();
        // A camera restarting its decoder (its old place is let go right
        // after) takes no more memory than it had.
        if !held.iter().any(|x| x == camera) {
            vram::reserve(vram::DECODER_MB);
        }
        held.push(camera.to_string());
        drop(held);
        list_put(&DENIED, camera, false);
        Some(NvdecHold(camera.to_string()))
    } else {
        list_put(&DENIED, camera, r.nvidia_full);
        None
    }
}

/// How long a camera keeps its place on the NVIDIA card with no working
/// session: a reconnect keeps it, a long outage does not (its memory shows
/// as free, so other cameras may take it, and the camera is judged as new
/// when it comes back).
pub const HOLD_THROUGH: Duration = Duration::from_secs(120);

/// A camera's time without a working session, for its place on the card.
#[derive(Debug, Default)]
pub struct Outage {
    since: Option<Instant>,
}

impl Outage {
    /// A session ended at `now`; `worked`: it brought pictures (the outage
    /// starts now), else the outage goes on from when it began.
    pub fn ended(&mut self, worked: bool, now: Instant) {
        if worked {
            self.since = Some(now);
        } else {
            self.since.get_or_insert(now);
        }
    }

    /// Whether the outage has gone on too long to keep the place.
    pub fn lapsed(&self, now: Instant) -> bool {
        self.since.is_some_and(|t| now.saturating_duration_since(t) > HOLD_THROUGH)
    }
}

/// The memory the NVIDIA card must keep free (MB; 0: no check).
pub fn min_free_mb() -> u64 {
    MIN_FREE_MB.load(Ordering::SeqCst)
}

/// Use this headroom from now on; as for `set_prefer`, the cameras' own
/// failure memory starts afresh and decoders on Auto look again at their
/// next keyframe. Returns whether it changed.
pub fn set_min_free_mb(mb: u32) -> bool {
    if MIN_FREE_MB.swap(mb as u64, Ordering::SeqCst) == mb as u64 {
        return false;
    }
    setting_changed();
    true
}

fn list_has(l: &Mutex<Vec<String>>, c: &str) -> bool {
    l.lock().unwrap().iter().any(|x| x == c)
}

/// Put the camera on the list or take it off; whether that changed it.
fn list_put(l: &Mutex<Vec<String>>, c: &str, on: bool) -> bool {
    let mut g = l.lock().unwrap();
    let at = g.iter().position(|x| x == c);
    match (on, at) {
        (true, None) => {
            g.push(c.to_string());
            true
        }
        (false, Some(i)) => {
            g.remove(i);
            true
        }
        _ => false,
    }
}

/// The headroom guard for Auto: Some(why) when the NVIDIA card has no room
/// for another decoder. A reading that is not there (no nvidia-smi, not read
/// yet) means no guard.
fn guard_full(camera: &str, p: Prefer, reading: vram::Reading, min_mb: u64) -> Option<String> {
    // Auto and "intel" are guarded ("intel" puts cameras on NVIDIA when the
    // Intel/AMD way fails or is busy); "nvidia" and a camera's own "cuda"
    // were asked for.
    if !matches!(p, Prefer::Auto | Prefer::Intel) || min_mb == 0 {
        return None;
    }
    let vram::Reading::Ok(g) = reading else { return None };
    if vram::has_room(g.free_mb, min_mb, list_has(&HELD, camera), list_has(&DENIED, camera)) {
        None
    } else {
        Some(format!("NVIDIA card memory is nearly full ({})", vram::used_text(&g)))
    }
}

/// Moves on each time the preference changes.
pub fn generation() -> u64 {
    GENERATION.load(Ordering::SeqCst)
}

// ---------------------------------------------------------------------------
// Graphics cards
// ---------------------------------------------------------------------------

#[derive(Clone, Debug, Serialize)]
pub struct Node {
    pub path: String,
    /// Kernel driver: i915 / xe (Intel), amdgpu, nvidia, ...
    pub driver: String,
    /// Intel Gen12 or newer (Quick Sync is tried before VAAPI).
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub gen12: bool,
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

/// Intel graphics of Gen12 or newer, by kernel driver and PCI device id:
/// everything the xe driver runs, and with i915 Tiger Lake (9Axx), Rocket
/// Lake (4Cxx), Alder Lake (46xx), DG1 (49xx), Arc / DG2 (4Fxx, 56xx), Meteor
/// and Arrow Lake (7Dxx), Raptor Lake (A7xx), Lunar Lake (64xx), Battlemage
/// (E2xx) and Panther Lake (B0xx).
pub fn intel_gen12(driver: &str, device_id: u16) -> bool {
    match driver {
        "xe" => true,
        "i915" => matches!(device_id >> 8, 0x9A | 0x4C | 0x46 | 0x49 | 0x4F | 0x56 | 0x7D | 0xA7 | 0x64 | 0xE2 | 0xB0),
        _ => false,
    }
}

/// Render nodes of this computer (Linux), best first for decoding: Intel,
/// then AMD, then others; NVIDIA last (it has no VAAPI driver of its own;
/// it decodes through CUDA instead).
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
                    let id = std::fs::read_to_string(format!("/sys/class/drm/{n}/device/device"))
                        .ok()
                        .and_then(|t| u16::from_str_radix(t.trim().trim_start_matches("0x"), 16).ok())
                        .unwrap_or(0);
                    let gen12 = intel_gen12(&driver, id);
                    Node { path: format!("/dev/dri/{n}"), driver, gen12 }
                })
                .collect()
        })
        .unwrap_or_default();
    v.sort_by(|a, b| a.rank().cmp(&b.rank()).then_with(|| a.path.cmp(&b.path)));
    v
}

/// Whether this computer has an NVIDIA card with NVIDIA's driver (Linux:
/// the driver's own list or a render node it runs; Windows: a display
/// adapter named NVIDIA). Looked at once.
pub fn nvidia_present() -> bool {
    static N: OnceLock<bool> = OnceLock::new();
    *N.get_or_init(|| {
        if cfg!(target_os = "linux") {
            std::fs::read_dir("/proc/driver/nvidia/gpus").is_ok_and(|mut d| d.next().is_some()) || render_nodes().iter().any(|n| n.driver == "nvidia")
        } else if cfg!(windows) {
            nvidia_adapter()
        } else {
            false
        }
    })
}

#[cfg(windows)]
fn nvidia_adapter() -> bool {
    crate::gpu::list().iter().any(|g| g.name.to_ascii_lowercase().contains("nvidia"))
}

#[cfg(not(windows))]
fn nvidia_adapter() -> bool {
    false
}

/// The cards hardware decoding can use.
#[derive(Clone, Debug, Default)]
pub struct Machine {
    pub nodes: Vec<Node>,
    pub nvidia: bool,
}

impl Machine {
    pub fn here() -> Machine {
        Machine { nodes: render_nodes(), nvidia: nvidia_present() }
    }
    /// Anything to decode on.
    pub fn any(&self) -> bool {
        self.nvidia || self.nodes.iter().any(|n| n.driver != "nvidia")
    }
}

/// The ways Auto tries for this preference on this machine, best first, as
/// specs ("cuda", "qsv:/dev/dri/renderD128", "vaapi:/dev/dri/renderD128").
/// Quick Sync needs the codec named up front: when it is not known (the
/// ffmpeg pipeline) VAAPI on the same node stands in for it.
pub fn candidates(prefer: Prefer, m: &Machine, codec: Option<Codec>) -> Vec<String> {
    let mut nodes: Vec<&Node> = m.nodes.iter().filter(|n| n.driver != "nvidia").collect();
    nodes.sort_by(|a, b| a.rank().cmp(&b.rank()).then_with(|| a.path.cmp(&b.path)));
    let mut cards = Vec::new();
    for n in nodes {
        let kinds: &[&str] = match (n.intel(), n.gen12) {
            (true, true) => &["qsv", "vaapi"],
            (true, false) => &["vaapi", "qsv"],
            _ => &["vaapi"],
        };
        for k in kinds {
            if *k == "qsv" && codec.is_none() {
                continue;
            }
            cards.push(format!("{k}:{}", n.path));
        }
    }
    let nv = m.nvidia.then(|| "cuda".to_string());
    match prefer {
        Prefer::Off => Vec::new(),
        Prefer::Auto | Prefer::Nvidia => nv.into_iter().chain(cards).collect(),
        Prefer::Intel => cards.into_iter().chain(nv).collect(),
    }
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

/// One test decode. `device` is the render node ("" for NVDEC).
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

/// Start the test decodes on a thread of their own, once (when there is a
/// card to test).
pub fn probe_in_background() {
    if PROBE_STARTED.swap(true, Ordering::SeqCst) {
        return;
    }
    std::thread::spawn(|| {
        let m = Machine::here();
        if m.nvidia {
            // The first memory reading is there before any camera asks.
            vram::refresh_now();
        }
        let _ = PROBE.set(if m.any() { run_probe(&m) } else { Vec::new() });
    });
}

pub fn probe_done() -> bool {
    PROBE.get().is_some()
}

/// The test results (None while it runs or when it was never started).
pub fn probe_results() -> Option<&'static [Trial]> {
    PROBE.get().map(|v| v.as_slice())
}

/// Whether [`resolve`] would not wait for the graphics card test now (it
/// has finished, or it was never started).
pub fn probe_settled() -> bool {
    !PROBE_STARTED.load(Ordering::SeqCst) || PROBE.get().is_some()
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

/// The ways to test: NVDEC when there is an NVIDIA card, VAAPI on each
/// Intel or AMD node, and Quick Sync on Intel nodes.
fn trial_ways(m: &Machine) -> Vec<(&'static str, String)> {
    let mut v: Vec<(&'static str, String)> = Vec::new();
    if m.nvidia {
        v.push(("cuda", String::new()));
    }
    for n in m.nodes.iter().filter(|n| n.driver != "nvidia") {
        v.push(("vaapi", n.path.clone()));
        if n.intel() {
            v.push(("qsv", n.path.clone()));
        }
    }
    v
}

fn run_probe(m: &Machine) -> Vec<Trial> {
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
        for (kind, device) in trial_ways(m) {
            let t = Instant::now();
            let spec = if device.is_empty() { kind.to_string() } else { format!("{kind}:{device}") };
            let r = if t0.elapsed() > budget {
                Err("not tested (the test took too long)".to_string())
            } else {
                decode_gray(Some(&spec), codec).and_then(|g| compare(&reference, &g))
            };
            out.push(Trial { kind, device, codec: codec_name(codec), ok: r.is_ok(), reason: r.err(), ms: t.elapsed().as_millis() as u64 });
        }
    }
    let spec = |t: &Trial| if t.device.is_empty() { t.kind.to_string() } else { format!("{}:{}", t.kind, t.device) };
    let ok: Vec<String> = out.iter().filter(|t| t.ok).map(|t| format!("{} {}", describe(&spec(t)), codec_label(t.codec))).collect();
    let bad: Vec<String> = out.iter().filter(|t| !t.ok).map(|t| format!("{} {}: {}", describe(&spec(t)), codec_label(t.codec), t.reason.as_deref().unwrap_or("failed"))).collect();
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

/// What the start-up test says when Quick Sync finds no Intel runtime.
pub const QSV_NO_RUNTIME: &str = "Intel's Quick Sync runtime is missing: install libmfx-gen1.2 (Debian, Ubuntu) or intel-vpl-gpu-rt (Fedora) and restart AmberSight; Gen12+ Intel only";

/// The first line ffmpeg printed that says something, without `[tag @ 0x..]`
/// prefixes, cut short.
pub fn reason_from_stderr(text: &str) -> Option<String> {
    // Quick Sync with no Intel runtime for the chip (oneVPL's GPU runtime is
    // a separate package): ffmpeg only says "Error creating a MFX session".
    if text.contains("Error creating a MFX session") {
        return Some(QSV_NO_RUNTIME.into());
    }
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
        // (libva prints its version on every start: not a reason.)
        if l.is_empty() || l.starts_with("ffmpeg version") || l.starts_with("built with") || l.starts_with("libva info:") {
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

/// A hardware decoder stopped or stalled while software decoding of the
/// same stream worked: every camera skips that way on that card for that
/// codec for an hour (other ways stay in use).
pub fn note_failure(spec: &str, codec: Option<Codec>, camera: &str, reason: &str) {
    let (kind, device) = split_spec(spec);
    let device = device.unwrap_or_default();
    let key = runtime_key(&kind, &device, codec);
    if RUNTIME.failed_at(&key, Instant::now()) {
        tracing::warn!(
            "camera '{camera}': hardware decoding ({}) failed for {} ({reason}); no camera uses it for that for the next hour",
            describe(spec),
            codec.map(|c| codec_label(codec_name(c))).unwrap_or("this stream")
        );
    }
    failures().lock().unwrap().insert(key, Failure { camera: camera.to_string(), reason: reason.to_string(), at: Instant::now() });
}

/// The first wait before a camera tries a way that failed for it again;
/// each further failure doubles it, up to `RETRY_MAX`.
pub const RETRY_FIRST: Duration = Duration::from_secs(15 * 60);
pub const RETRY_MAX: Duration = Duration::from_secs(4 * 3600);
/// After this many failures in a row a camera stops trying that way until
/// AmberSight restarts or the Hardware setting changes: a way that failed
/// twice on one camera's stream keeps failing on it (seen: VAAPI on a Rocket
/// Lake with Amcrest streams, every time within minutes). Failures because
/// the card's memory was full do not count (`memory_trouble`): the card may
/// have room later.
pub const GIVE_UP_AFTER: u32 = 2;
/// A failure this long after the one before counts as the first again.
const RETRY_RESET: Duration = Duration::from_secs(6 * 3600);
/// A decoder that has run this long on a way without trouble: the camera's
/// failures of that way are forgotten (`note_camera_ok`), so only failures
/// in a row count.
pub const HEALTHY_RUN: Duration = Duration::from_secs(30 * 60);

/// Whether a decoder's failure on `spec` reads as the NVIDIA card running
/// out of memory (it may have room later, so it is not held against the way
/// for good). Only NVDEC: the headroom guard watches that card's memory, and
/// QSV decoders that keep failing also say "Cannot allocate memory" (ffmpeg's
/// text for ENOMEM, from MFX_ERR_MEMORY_ALLOC), so on QSV and VAAPI any
/// wording counts as a normal failure.
pub fn memory_trouble(spec: &str, reason: &str) -> bool {
    split_spec(spec).0 == "cuda" && memory_words(reason)
}

/// Out-of-memory wording in a decoder's error, whatever the way. (Used to keep
/// such a failure from being blamed on the whole card; whether it counts
/// toward this camera giving the way up is [`memory_trouble`].)
pub fn memory_words(reason: &str) -> bool {
    // Only plain out-of-memory wording: "memory allocation error" (QSV) and
    // "Failed to allocate ... surface" (VAAPI) also come from decoders that
    // keep failing, and those must still be given up.
    const WORDS: [&str; 7] = [
        "out of memory",
        "out_of_memory",
        "outofmemory",
        "cudaerrormemoryallocation",
        "cannot allocate memory",
        "enomem",
        "not enough memory",
    ];
    let low = reason.to_ascii_lowercase();
    WORDS.iter().any(|w| low.contains(w))
}

/// How long a camera waits before trying a way again after its `count`th
/// failure in a row (1: the first).
pub fn retry_wait(count: u32) -> Duration {
    let f = 1u32 << count.saturating_sub(1).min(8);
    (RETRY_FIRST * f).min(RETRY_MAX)
}

/// A way that failed for one camera: how often in a row, why last time, and
/// until when the camera does not use it.
#[derive(Clone, Debug)]
pub struct CamFail {
    pub count: u32,
    /// The failures that count toward giving up (not the memory ones).
    strikes: u32,
    pub reason: String,
    last: Instant,
    until: Instant,
    /// `until` as Unix seconds (Status says the time).
    pub until_wall: i64,
    /// Not tried again for this camera (`GIVE_UP_AFTER`).
    pub given_up: bool,
}

fn cam_fails() -> &'static Mutex<HashMap<String, CamFail>> {
    static F: OnceLock<Mutex<HashMap<String, CamFail>>> = OnceLock::new();
    F.get_or_init(Default::default)
}

fn cam_key(camera: &str, spec: &str, codec: Option<Codec>) -> String {
    format!("{camera}|{spec}|{}", codec.map(codec_name).unwrap_or("any"))
}

fn note_camera_failure_at(camera: &str, spec: &str, codec: Option<Codec>, reason: &str, now: Instant, wall: i64) -> CamFail {
    let mut m = cam_fails().lock().unwrap();
    let key = cam_key(camera, spec, codec);
    let (count, strikes) = match m.get(&key) {
        Some(f) if now.saturating_duration_since(f.last) < RETRY_RESET => (f.count + 1, f.strikes),
        _ => (1, 0),
    };
    // (The card was full: waited out as any failure, never given up for.)
    let strikes = if memory_trouble(spec, reason) { strikes } else { strikes + 1 };
    let given_up = strikes >= GIVE_UP_AFTER;
    let (until, until_wall) = if given_up {
        (now.checked_add(Duration::from_secs(10 * 365 * 86400)).unwrap_or(now + RETRY_MAX), i64::MAX)
    } else {
        let wait = retry_wait(count);
        (now + wait, wall + wait.as_secs() as i64)
    };
    let f = CamFail { count, strikes, reason: reason.to_string(), last: now, until, until_wall, given_up };
    m.insert(key, f.clone());
    f
}

/// A hardware decoder of this camera failed (any way: it stopped, stalled
/// or reported a burst of errors): the camera does not use that way until
/// the wait is over. Returns the note for Status.
pub fn note_camera_failure(camera: &str, spec: &str, codec: Option<Codec>, reason: &str) -> String {
    let f = note_camera_failure_at(camera, spec, codec, reason, Instant::now(), chrono::Utc::now().timestamp());
    tracing::info!("camera '{camera}': {}", fail_note(spec, &f));
    fail_note(spec, &f)
}

/// A hardware decoder of this camera has run `HEALTHY_RUN` on this way
/// without trouble: its earlier failures of that way are forgotten (the next
/// failure counts as the first).
pub fn note_camera_ok(camera: &str, spec: &str, codec: Option<Codec>) {
    if cam_fails().lock().unwrap().remove(&cam_key(camera, spec, codec)).is_some() {
        tracing::info!("camera '{camera}': {} has run well for {} minutes; its earlier failures there are forgotten", describe(spec), HEALTHY_RUN.as_secs() / 60);
    }
}

/// This camera's failure of this way, while it is still not to be used.
fn camera_blocked_at(camera: &str, spec: &str, codec: Option<Codec>, now: Instant) -> Option<CamFail> {
    cam_fails().lock().unwrap().get(&cam_key(camera, spec, codec)).filter(|f| f.until > now).cloned()
}

/// "VAAPI on /dev/dri/renderD128 failed 3 times (...); trying it again at 10:42".
pub fn fail_note(spec: &str, f: &CamFail) -> String {
    let times = if f.count == 1 { "once".to_string() } else if f.count == 2 { "twice".to_string() } else { format!("{} times", f.count) };
    if f.given_up {
        return format!("{} failed {times} ({}); not used for this camera until AmberSight restarts or the Hardware setting changes", describe(spec), f.reason);
    }
    let at = clock(f.until_wall);
    format!("{} failed {times} ({}); trying it again at {at}", describe(spec), f.reason)
}

/// Why this camera may not use this way now (None: it may).
/// (Whichever lasts longer: this camera's own wait, or the hour every
/// camera leaves a way alone that stopped while software decoding worked.)
fn blocked_for(camera: &str, spec: &str, codec: Option<Codec>, now: Instant) -> Option<String> {
    let mine = camera_blocked_at(camera, spec, codec, now);
    let (kind, device) = split_spec(spec);
    let key = runtime_key(&kind, device.as_deref().unwrap_or(""), codec);
    let hour = Duration::from_secs(3600);
    // (The memory for every camera is looked at with the clock as it is: a
    // later `now` would forget other entries of that shared memory.)
    let real = Instant::now();
    let all = RUNTIME.skipped_at(&key, real).then(|| failures().lock().unwrap().get(&key).map(|f| f.at + hour).unwrap_or(real + hour));
    match (mine, all) {
        (Some(f), Some(until)) if until <= f.until => Some(fail_note(spec, &f)),
        (_, Some(until)) => {
            let wall = chrono::Utc::now().timestamp() + until.saturating_duration_since(real).as_secs() as i64;
            Some(format!("{} failed for {} while running; no camera uses it for that until {}", describe(spec), codec.map(|c| codec_label(codec_name(c))).unwrap_or("this stream"), clock(wall)))
        }
        (Some(f), None) => Some(fail_note(spec, &f)),
        (None, None) => None,
    }
}

/// "10:42" (this computer's time) for Unix seconds.
fn clock(wall: i64) -> String {
    use chrono::TimeZone;
    chrono::Local.timestamp_opt(wall, 0).single().map(|t| t.format("%H:%M").to_string()).unwrap_or_default()
}

// ---------------------------------------------------------------------------
// Starting decoders one after another
// ---------------------------------------------------------------------------

/// Hardware decoders start at least this far apart (16 cameras do not all
/// open a session on the card at the same instant).
pub const START_GAP: Duration = Duration::from_millis(250);

static LAST_START: Mutex<Option<Instant>> = Mutex::new(None);

fn reserve_in(last: &Mutex<Option<Instant>>, now: Instant) -> Instant {
    let mut g = last.lock().unwrap();
    let slot = match *g {
        Some(l) if l + START_GAP > now => l + START_GAP,
        _ => now,
    };
    *g = Some(slot);
    slot
}

/// A start time for a hardware decoder: now, or `START_GAP` after the one
/// reserved before.
pub fn reserve_start(now: Instant) -> Instant {
    reserve_in(&LAST_START, now)
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
    /// Why hardware decoding is not used, or why a better way is not (a way
    /// that failed for this camera, and when it is tried again).
    pub why_not: Option<String>,
    /// The start-up test had not finished (try again once it has).
    pub pending: bool,
    /// The camera names its own way (else the server's preference decides).
    pub camera_setting: bool,
    /// The headroom guard turned NVDEC down for it (the card was nearly full).
    pub nvidia_full: bool,
}

/// How to say a spec in Status: "NVIDIA (NVDEC)", "Intel Quick Sync on
/// /dev/dri/renderD128", "VAAPI on /dev/dri/renderD129".
pub fn describe(spec: &str) -> String {
    let (kind, device) = split_spec(spec);
    let k = match kind.as_str() {
        "vaapi" => "VAAPI".to_string(),
        "qsv" => "Intel Quick Sync".to_string(),
        "cuda" => "NVIDIA (NVDEC)".to_string(),
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

fn trial_for<'a>(trials: &'a [Trial], spec: &str, codec: Option<Codec>) -> Option<&'a Trial> {
    let (kind, device) = split_spec(spec);
    let device = device.unwrap_or_default();
    let c = codec.map(codec_name).unwrap_or("h264");
    trials.iter().find(|t| t.kind == kind && t.device == device && t.codec == c)
}

/// Whether a decoder that runs on `running` (None: the processor, also after
/// a fallback) should start again on `new`, what `resolve` says now. When
/// the setting changed: whenever they differ. Otherwise only for the
/// better: from the processor to a card, or to a way earlier in Auto's
/// order. A decoder that works is never moved off because its way failed
/// for another camera meanwhile.
pub fn worth_moving(running: Option<&str>, new: Option<&str>, setting_changed: bool, codec: Option<Codec>) -> bool {
    worth_moving_in(running, new, setting_changed, &candidates(prefer(), &Machine::here(), codec))
}

fn worth_moving_in(running: Option<&str>, new: Option<&str>, setting_changed: bool, order: &[String]) -> bool {
    if running == new {
        return false;
    }
    if setting_changed {
        return true;
    }
    match (running, new) {
        (None, Some(_)) => true,
        (Some(_), None) | (None, None) => false,
        (Some(old), Some(new)) => {
            let at = |s: &str| order.iter().position(|x| x == s);
            match (at(new), at(old)) {
                (Some(a), Some(b)) => a < b,
                _ => false,
            }
        }
    }
}

/// The decoder for this camera's stream of this codec (None: not known yet,
/// as for the ffmpeg pipeline) under its setting and the server's
/// preference. Waits up to `wait` for the start-up test. Only a look (for
/// rechecks and notes too): where a decoder then actually starts, `started`
/// is called with the answer.
pub fn resolve(camera: &str, setting: Option<&str>, codec: Option<Codec>, wait: Duration) -> Resolved {
    let now = Instant::now();
    let m = Machine::here();
    let p = prefer();
    let min_mb = min_free_mb();
    let turned_away = std::cell::Cell::new(false);
    let guard = || {
        let why = guard_full(camera, p, vram::reading(), min_mb);
        if why.is_some() {
            turned_away.set(true);
        }
        why
    };
    let r = resolve_with(choice(setting), codec, &m, p, || wait_probe(wait), PROBE_STARTED.load(Ordering::SeqCst), |spec| blocked_for(camera, spec, codec, now), guard);
    Resolved { nvidia_full: turned_away.get(), ..r }
}

fn resolve_with<'a>(
    choice: Choice,
    codec: Option<Codec>,
    m: &Machine,
    prefer: Prefer,
    probe: impl FnOnce() -> Option<&'a [Trial]>,
    started: bool,
    blocked: impl Fn(&str) -> Option<String>,
    nvidia_full: impl Fn() -> Option<String>,
) -> Resolved {
    match choice {
        Choice::Off => Resolved { camera_setting: true, ..Default::default() },
        Choice::Auto => {
            if !cfg!(any(target_os = "linux", windows)) && !cfg!(test) {
                return Resolved::default();
            }
            let list = candidates(prefer, m, codec);
            if list.is_empty() {
                let off = (prefer == Prefer::Off && m.any()).then(|| "hardware decoding is off in Settings".to_string());
                return Resolved { why_not: off, ..Default::default() };
            }
            if !started {
                return Resolved::default();
            }
            let Some(trials) = probe() else {
                return Resolved { spec: None, why_not: Some("the graphics card test is still running".into()), pending: true, camera_setting: false, nvidia_full: false };
            };
            let mut skipped: Option<String> = None;
            let mut full: Option<String> = None;
            for spec in &list {
                if !trial_for(trials, spec, codec).is_some_and(|t| t.ok) {
                    continue;
                }
                if spec == "cuda" {
                    if let Some(why) = nvidia_full() {
                        full = Some(why);
                        continue;
                    }
                }
                if let Some(why) = blocked(spec) {
                    skipped.get_or_insert(why);
                    continue;
                }
                let why_not = skipped.or(full.map(|f| format!("{f}; using {}", describe(spec))));
                return Resolved { spec: Some(spec.clone()), why_not, pending: false, camera_setting: false, nvidia_full: false };
            }
            // Why not: a way that failed while running, else the first way's own test result.
            // (Both: say both, so a camera on the processor shows that NVIDIA
            // was full as well as why the preferred way was not used.)
            let why = match (skipped, full) {
                (Some(s), Some(f)) => Some(format!("{s}; {f}")),
                (s, f) => s.or(f),
            }
                .or_else(|| {
                    list.iter().find_map(|s| trial_for(trials, s, codec).map(|t| format!("hardware decoding unavailable: {} failed the test ({})", describe(s), t.reason.as_deref().unwrap_or("failed"))))
                })
                .unwrap_or_else(|| "no graphics card decodes this stream".into());
            Resolved { spec: None, why_not: Some(format!("{why}; using the processor")), pending: false, camera_setting: false, nvidia_full: false }
        }
        Choice::Named { kind, device } => {
            let mut kind = kind;
            let trials = if matches!(kind.as_str(), "vaapi" | "qsv" | "cuda") && started { probe() } else { None };
            let device = match (kind.as_str(), device) {
                (_, Some(d)) => Some(d),
                ("vaapi" | "qsv", None) => {
                    // The first node where this way worked, else the first node.
                    let worked = trials.and_then(|t| m.nodes.iter().find(|n| trial_for(t, &format!("{kind}:{}", n.path), codec).is_some_and(|x| x.ok)).map(|n| n.path.clone()));
                    Some(worked.or_else(|| m.nodes.iter().find(|n| n.driver != "nvidia").map(|n| n.path.clone())).unwrap_or_else(|| "/dev/dri/renderD128".into()))
                }
                _ => None,
            };
            let spec_of = |k: &str| match &device {
                Some(d) => format!("{k}:{d}"),
                None => k.to_string(),
            };
            if let Some(x) = trials.and_then(|t| trial_for(t, &spec_of(&kind), codec)).filter(|x| !x.ok) {
                return Resolved {
                    spec: None,
                    why_not: Some(format!("{} failed the test ({}); using the processor", describe(&spec_of(&kind)), x.reason.as_deref().unwrap_or("failed"))),
                    pending: false,
                    camera_setting: true,
                    nvidia_full: false,
                };
            }
            // The ffmpeg pipeline does not know the codec before it starts,
            // and Quick Sync decoding needs it named: VAAPI drives the same
            // decoder on the same chip.
            if kind == "qsv" && codec.is_none() {
                kind = "vaapi".into();
            }
            let spec = spec_of(&kind);
            if let Some(why) = blocked(&spec) {
                return Resolved { spec: None, why_not: Some(format!("{why}; using the processor until then")), pending: false, camera_setting: true, nvidia_full: false };
            }
            Resolved { spec: Some(spec), why_not: None, pending: false, camera_setting: true, nvidia_full: false }
        }
    }
}

// ---------------------------------------------------------------------------
// Status
// ---------------------------------------------------------------------------

/// Notes for Status: cards that cannot decode (no way on them passed the
/// test), and failures while running (in the last hour).
pub fn notes() -> Vec<String> {
    let mut out = Vec::new();
    if let Some(trials) = probe_results() {
        out.extend(test_notes(&Machine::here(), prefer(), trials));
    }
    let now = Instant::now();
    let mut f: Vec<(String, Failure)> = failures().lock().unwrap().iter().filter(|(_, v)| now.saturating_duration_since(v.at) < Duration::from_secs(3600)).map(|(k, v)| (k.clone(), v.clone())).collect();
    f.sort_by(|a, b| a.0.cmp(&b.0));
    for (k, v) in f {
        let parts: Vec<&str> = k.split('|').collect();
        let (kind, device, codec) = (parts.get(1).copied().unwrap_or(""), parts.get(2).copied().unwrap_or(""), parts.get(3).copied().unwrap_or(""));
        let spec = if device.is_empty() { kind.to_string() } else { format!("{kind}:{device}") };
        out.push(format!(
            "Hardware decoding ({}) failed{} on {} ({}); no camera uses it for that for the next hour",
            describe(&spec),
            if codec == "any" { String::new() } else { format!(" for {}", codec_label(codec)) },
            v.camera,
            v.reason
        ));
    }
    out
}

/// A card (a render node, or "" for NVIDIA) where every way failed the
/// test for a codec, said once per card when both codecs failed alike,
/// with what Auto uses instead.
fn test_notes(m: &Machine, prefer: Prefer, trials: &[Trial]) -> Vec<String> {
    let mut out = Vec::new();
    let mut cards: Vec<String> = Vec::new();
    for (_, d) in trial_ways(m) {
        if !cards.contains(&d) {
            cards.push(d);
        }
    }
    let name = |d: &str| if d.is_empty() { "the NVIDIA card".to_string() } else { d.to_string() };
    let failed = |d: &str, codec: &str| -> Option<&Trial> {
        let mine: Vec<&Trial> = trials.iter().filter(|t| t.device == d && t.codec == codec).collect();
        (!mine.is_empty() && mine.iter().all(|t| !t.ok)).then(|| mine[0])
    };
    for codec in ["h264", "hevc"] {
        let c = if codec == "hevc" { Codec::H265 } else { Codec::H264 };
        let works = candidates(prefer, m, Some(c)).into_iter().find(|s| trial_for(trials, s, Some(c)).is_some_and(|t| t.ok));
        for d in &cards {
            let Some(t) = failed(d, codec) else { continue };
            // A card that fails both codecs for one reason is said once.
            if codec == "hevc" && failed(d, "h264").is_some_and(|x| x.reason == t.reason) {
                continue;
            }
            let both = codec == "h264" && failed(d, "hevc").is_some_and(|x| x.reason == t.reason);
            let what = if both { String::new() } else { format!(" for {}", codec_label(codec)) };
            let instead = match &works {
                Some(w) => format!("{} is used instead", describe(w)),
                None => "using the processor".to_string(),
            };
            out.push(format!("Hardware decoding unavailable on {}{what} ({}); {instead}", name(d), t.reason.as_deref().unwrap_or("failed")));
        }
    }
    out
}

/// `GET /api/hwdecode`: the preference, the cards, what the test found on
/// each, the order Auto tries, and what Auto uses.
pub fn status_json() -> serde_json::Value {
    let m = Machine::here();
    let trials = probe_results();
    let list: Vec<serde_json::Value> = m
        .nodes
        .iter()
        .map(|n| {
            let mut v = serde_json::to_value(n).unwrap_or_default();
            if let Some(t) = trials {
                let mine: Vec<&Trial> = t.iter().filter(|x| x.device == n.path).collect();
                v["tests"] = serde_json::to_value(mine).unwrap_or_default();
            }
            v
        })
        .collect();
    let mut nvidia = serde_json::json!({ "present": m.nvidia });
    if let (true, Some(t)) = (m.nvidia, trials) {
        let mine: Vec<&Trial> = t.iter().filter(|x| x.kind == "cuda").collect();
        nvidia["tests"] = serde_json::to_value(mine).unwrap_or_default();
    }
    let auto = |c: Codec| if probe_done() { resolve("", None, Some(c), Duration::ZERO).spec.map(|s| describe(&s)) } else { None };
    let order: Vec<String> = candidates(prefer(), &m, Some(Codec::H264)).iter().map(|s| describe(s)).collect();
    let reading = vram::reading_blocking();
    nvidia["memory"] = match reading {
        vram::Reading::Ok(g) => serde_json::json!({ "free_mb": g.free_mb, "total_mb": g.total_mb, "used_mb": g.used_mb() }),
        _ => serde_json::Value::Null,
    };
    nvidia["smi"] = serde_json::json!(match reading {
        vram::Reading::Ok(_) => "ok",
        vram::Reading::Missing => "missing",
        vram::Reading::Unknown => "unknown",
    });
    serde_json::json!({
        "prefer": prefer().name(),
        "nvidia_min_free_mb": min_free_mb(),
        "auto_supported": cfg!(any(target_os = "linux", windows)),
        "tested": probe_done(),
        "nodes": list,
        "nvidia": nvidia,
        "order": order,
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
    /// Hardware decoding was wanted but is not used: why (or why a better
    /// way is not used, and when it is tried again).
    pub note: Option<String>,
    /// Where the way comes from: "server" (the Hardware decoding setting in
    /// Settings, for a camera on Auto) or "camera" (its own setting).
    pub hw_setting: Option<&'static str>,
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
            "hw_setting": self.hw_setting,
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
        Node { path: p.into(), driver: d.into(), gen12: d == "xe" }
    }

    fn old_intel(p: &str) -> Node {
        Node { path: p.into(), driver: "i915".into(), gen12: false }
    }

    fn trial(kind: &'static str, dev: &str, codec: &'static str, ok: bool) -> Trial {
        Trial { kind, device: dev.into(), codec, ok, reason: (!ok).then(|| "Failed to initialise VAAPI connection".into()), ms: 1 }
    }

    /// Every way on this machine passed for both codecs.
    fn all_pass(m: &Machine) -> Vec<Trial> {
        let mut v = Vec::new();
        for codec in ["h264", "hevc"] {
            for (k, d) in trial_ways(m) {
                v.push(trial(k, &d, codec, true));
            }
        }
        v
    }

    fn machine(nodes: Vec<Node>, nvidia: bool) -> Machine {
        Machine { nodes, nvidia }
    }

    fn none(_: &str) -> Option<String> {
        None
    }

    /// Auto under a preference, nothing failed while running.
    fn auto(p: Prefer, m: &Machine, trials: &[Trial], codec: Option<Codec>) -> Resolved {
        resolve_with(Choice::Auto, codec, m, p, || Some(trials), true, none, || None)
    }

    #[test]
    fn settings_are_read() {
        assert_eq!(choice(None), Choice::Auto);
        assert_eq!(choice(Some("auto")), Choice::Auto);
        assert_eq!(choice(Some("Off")), Choice::Off);
        assert_eq!(choice(Some("vaapi")), Choice::Named { kind: "vaapi".into(), device: None });
        assert_eq!(choice(Some("vaapi:/dev/dri/renderD129")), Choice::Named { kind: "vaapi".into(), device: Some("/dev/dri/renderD129".into()) });
        assert_eq!(choice(Some("qsv:/dev/dri/renderD128")), Choice::Named { kind: "qsv".into(), device: Some("/dev/dri/renderD128".into()) });
        assert_eq!(choice(Some("cuda")), Choice::Named { kind: "cuda".into(), device: None });
        // A device that is not a render node is not passed on.
        assert_eq!(choice(Some("vaapi:/etc/passwd")), Choice::Named { kind: "vaapi".into(), device: None });
        assert_eq!(choice(Some("bogus")), Choice::Auto);
        assert!(check(Some("bogus")).is_some());
        assert!(check(Some("vaapi:/tmp/x")).is_some());
        assert!(check(Some("cuda:/dev/dri/renderD128")).is_some());
        for ok in [None, Some("auto"), Some("off"), Some("vaapi"), Some("qsv"), Some("cuda"), Some("vaapi:/dev/dri/renderD130"), Some("d3d11va"), Some("videotoolbox")] {
            assert_eq!(check(ok), None, "{ok:?}");
        }
        let (c, w) = sanitized(Some("gpu"));
        assert_eq!(c, Choice::Auto);
        assert!(w.is_some());
    }

    #[test]
    fn the_headroom_guard_moves_auto_on_and_back() {
        let h264 = Some(Codec::H264);
        let m = machine(vec![node("/dev/dri/renderD129", "nvidia"), Node { path: "/dev/dri/renderD128".into(), driver: "i915".into(), gen12: true }], true);
        let pass = all_pass(&m);
        let gpu = |free_mb| vram::Reading::Ok(vram::Gpu { free_mb, total_mb: 16311 });
        let run = |cam: &str, p: Prefer, min: u64, rd: vram::Reading| {
            resolve_with(Choice::Auto, h264, &m, p, || Some(&pass[..]), true, none, || guard_full(cam, p, rd, min))
        };
        // Room: NVDEC, no note.
        let r = run("A1", Prefer::Auto, 1536, gpu(5000));
        assert_eq!((r.spec.as_deref(), r.why_not), (Some("cuda"), None));
        // No room: the next way, and the note says why and what is used.
        let r = run("A2", Prefer::Auto, 1536, gpu(1400));
        assert_eq!(r.spec.as_deref(), Some("qsv:/dev/dri/renderD128"));
        assert_eq!(r.why_not.as_deref(), Some("NVIDIA card memory is nearly full (14.9 of 16.3 GB used); using Intel Quick Sync on /dev/dri/renderD128"));
        // Room for the threshold alone is not enough for the decoder too.
        assert_eq!(run("A3", Prefer::Auto, 1536, gpu(1700)).spec.as_deref(), Some("qsv:/dev/dri/renderD128"));
        // Hysteresis: a camera turned away needs 1 GB more.
        assert!(list_put(&DENIED, "Hy", true));
        assert_eq!(run("Hy", Prefer::Auto, 1536, gpu(1536 + 300 + 1000)).spec.as_deref(), Some("qsv:/dev/dri/renderD128"));
        assert_eq!(run("Hy", Prefer::Auto, 1536, gpu(1536 + 300 + 1024)).spec.as_deref(), Some("cuda"));
        list_put(&DENIED, "Hy", false);
        // A camera already on NVDEC stays whatever is free.
        list_put(&HELD, "On", true);
        assert_eq!(run("On", Prefer::Auto, 1536, gpu(10)).spec.as_deref(), Some("cuda"));
        list_put(&HELD, "On", false);
        // "intel" is guarded too: when the Intel way is not to be used (it
        // failed for this camera), NVIDIA takes the camera only while there
        // is room; else the processor.
        let intel_blocked = |s: &str| s.starts_with("qsv").then(|| "failed".to_string());
        let r = resolve_with(Choice::Auto, h264, &m, Prefer::Intel, || Some(&pass[..]), true, intel_blocked, || guard_full("I1", Prefer::Intel, gpu(5000), 1536));
        assert_eq!(r.spec.as_deref(), Some("vaapi:/dev/dri/renderD128"), "VAAPI before NVIDIA under intel");
        let both_blocked = |s: &str| (!s.starts_with("cuda")).then(|| "failed".to_string());
        let r = resolve_with(Choice::Auto, h264, &m, Prefer::Intel, || Some(&pass[..]), true, both_blocked, || guard_full("I2", Prefer::Intel, gpu(5000), 1536));
        assert_eq!(r.spec.as_deref(), Some("cuda"), "room: NVIDIA");
        let r = resolve_with(Choice::Auto, h264, &m, Prefer::Intel, || Some(&pass[..]), true, both_blocked, || guard_full("I3", Prefer::Intel, gpu(1400), 1536));
        assert_eq!(r.spec, None, "no room: the processor");
        assert!(r.why_not.as_deref().unwrap_or("").contains("NVIDIA card memory is nearly full"), "{:?}", r.why_not);
        // "nvidia" ignores the guard; so does 0 (off).
        assert_eq!(run("N1", Prefer::Nvidia, 1536, gpu(10)).spec.as_deref(), Some("cuda"));
        assert_eq!(run("N2", Prefer::Auto, 0, gpu(10)).spec.as_deref(), Some("cuda"));
        // No nvidia-smi, or not read yet: as before the guard.
        assert_eq!(run("N3", Prefer::Auto, 1536, vram::Reading::Missing).spec.as_deref(), Some("cuda"));
        assert_eq!(run("N4", Prefer::Auto, 1536, vram::Reading::Unknown).spec.as_deref(), Some("cuda"));
        // A camera's own "cuda" is never guarded (the guard is not asked).
        let named = resolve_with(choice(Some("cuda")), h264, &m, Prefer::Auto, || Some(&pass[..]), true, none, || Some("full".into()));
        assert_eq!(named.spec.as_deref(), Some("cuda"));
        // Nothing else to use: the processor, with the reason.
        let only = machine(vec![], true);
        let p1 = all_pass(&only);
        let r = resolve_with(Choice::Auto, h264, &only, Prefer::Auto, || Some(&p1[..]), true, none, || guard_full("Z", Prefer::Auto, gpu(1400), 1536));
        assert_eq!(r.spec, None);
        assert_eq!(r.why_not.as_deref(), Some("NVIDIA card memory is nearly full (14.9 of 16.3 GB used); using the processor"));
    }

    #[test]
    fn the_preference_is_read() {
        for (s, p) in [("auto", Prefer::Auto), ("", Prefer::Auto), ("NVIDIA", Prefer::Nvidia), ("intel", Prefer::Intel), ("off", Prefer::Off)] {
            assert_eq!(Prefer::parse(s), Some(p), "{s}");
        }
        assert_eq!(Prefer::parse("amd"), None);
        for p in [Prefer::Auto, Prefer::Nvidia, Prefer::Intel, Prefer::Off] {
            assert_eq!(Prefer::parse(p.name()), Some(p));
            assert_eq!(Prefer::from_code(p.code()), p);
        }
        let c = HwDecodeConfig { prefer: "gpu".into(), ..Default::default() };
        let (p, w) = c.sanitized();
        assert_eq!(p, Prefer::Auto);
        assert!(w.unwrap().contains("auto, nvidia, intel or off"));
        assert!(HwDecodeConfig::default().is_default());
        assert!(!HwDecodeConfig { nvidia_min_free_mb: 0, ..Default::default() }.is_default());
        let t: crate::config::Config = toml::from_str("[server]\n[hwdecode]\nprefer = \"intel\"\n").unwrap();
        assert_eq!(t.hwdecode.sanitized(), (Prefer::Intel, None));
        assert_eq!(t.hwdecode.nvidia_min_free_mb, 1536);
        let t: crate::config::Config = toml::from_str("[server]\n[hwdecode]\nnvidia_min_free_mb = 0\n").unwrap();
        assert_eq!(t.hwdecode.nvidia_min_free_mb, 0);
    }

    #[test]
    fn newer_intel_is_known() {
        assert!(intel_gen12("xe", 0xe223), "Battlemage");
        assert!(intel_gen12("i915", 0x4c8a), "Rocket Lake UHD 750");
        assert!(intel_gen12("i915", 0x9a49), "Tiger Lake");
        assert!(intel_gen12("i915", 0x4680), "Alder Lake");
        assert!(intel_gen12("i915", 0xa780), "Raptor Lake");
        assert!(intel_gen12("i915", 0x56a0), "Arc A770");
        assert!(!intel_gen12("i915", 0x3e92), "Coffee Lake UHD 630");
        assert!(!intel_gen12("i915", 0x5912), "Kaby Lake");
        assert!(!intel_gen12("i915", 0x8a52), "Ice Lake");
        assert!(!intel_gen12("amdgpu", 0x4c8a));
        assert!(!intel_gen12("i915", 0));
    }

    #[test]
    fn the_order_for_each_kind_of_machine() {
        let h264 = Some(Codec::H264);
        // NVIDIA only (its render node has no VAAPI of its own).
        let nv = machine(vec![node("/dev/dri/renderD128", "nvidia")], true);
        assert_eq!(candidates(Prefer::Auto, &nv, h264), ["cuda"]);
        assert_eq!(candidates(Prefer::Intel, &nv, h264), ["cuda"], "prefer, not only");
        assert!(candidates(Prefer::Off, &nv, h264).is_empty());
        // Intel Gen12+ only: Quick Sync, then VAAPI; the ffmpeg pipeline (no codec): VAAPI.
        let intel = machine(vec![node("/dev/dri/renderD128", "xe")], false);
        assert_eq!(candidates(Prefer::Auto, &intel, h264), ["qsv:/dev/dri/renderD128", "vaapi:/dev/dri/renderD128"]);
        assert_eq!(candidates(Prefer::Auto, &intel, None), ["vaapi:/dev/dri/renderD128"]);
        // Older Intel: VAAPI first.
        let old = machine(vec![old_intel("/dev/dri/renderD128")], false);
        assert_eq!(candidates(Prefer::Auto, &old, h264), ["vaapi:/dev/dri/renderD128", "qsv:/dev/dri/renderD128"]);
        // Both (the Rocket Lake + RTX case): NVIDIA first for Auto and NVIDIA; last for Intel.
        let both = machine(vec![node("/dev/dri/renderD129", "nvidia"), Node { path: "/dev/dri/renderD128".into(), driver: "i915".into(), gen12: true }], true);
        let a = candidates(Prefer::Auto, &both, h264);
        assert_eq!(a, ["cuda", "qsv:/dev/dri/renderD128", "vaapi:/dev/dri/renderD128"]);
        assert_eq!(candidates(Prefer::Nvidia, &both, h264), a);
        assert_eq!(candidates(Prefer::Intel, &both, h264), ["qsv:/dev/dri/renderD128", "vaapi:/dev/dri/renderD128", "cuda"]);
        assert!(candidates(Prefer::Off, &both, h264).is_empty());
        // AMD: VAAPI; Intel before AMD whatever the node numbers.
        let amd = machine(vec![node("/dev/dri/renderD128", "amdgpu")], false);
        assert_eq!(candidates(Prefer::Auto, &amd, h264), ["vaapi:/dev/dri/renderD128"]);
        let mixed = machine(vec![node("/dev/dri/renderD128", "amdgpu"), node("/dev/dri/renderD129", "xe")], false);
        assert_eq!(candidates(Prefer::Auto, &mixed, Some(Codec::H265)), ["qsv:/dev/dri/renderD129", "vaapi:/dev/dri/renderD129", "vaapi:/dev/dri/renderD128"]);
        // None.
        let bare = Machine::default();
        assert!(!bare.any());
        for p in [Prefer::Auto, Prefer::Nvidia, Prefer::Intel, Prefer::Off] {
            assert!(candidates(p, &bare, h264).is_empty());
        }
        // What gets tested: NVDEC, and VAAPI (+ Quick Sync on Intel) per node.
        assert_eq!(trial_ways(&both), [("cuda", String::new()), ("vaapi", "/dev/dri/renderD128".into()), ("qsv", "/dev/dri/renderD128".into())]);
    }

    #[test]
    fn auto_uses_the_first_way_that_passed() {
        let h264 = Some(Codec::H264);
        let both = machine(vec![node("/dev/dri/renderD129", "nvidia"), Node { path: "/dev/dri/renderD128".into(), driver: "i915".into(), gen12: true }], true);
        let pass = all_pass(&both);
        assert_eq!(auto(Prefer::Auto, &both, &pass, h264).spec.as_deref(), Some("cuda"));
        assert_eq!(auto(Prefer::Intel, &both, &pass, h264).spec.as_deref(), Some("qsv:/dev/dri/renderD128"));
        assert_eq!(auto(Prefer::Intel, &both, &pass, None).spec.as_deref(), Some("vaapi:/dev/dri/renderD128"), "ffmpeg pipeline");
        let off = auto(Prefer::Off, &both, &pass, h264);
        assert_eq!(off.spec, None);
        assert_eq!(off.why_not.as_deref(), Some("hardware decoding is off in Settings"));
        // NVDEC failed the test: Quick Sync.
        let mut no_nv = pass.clone();
        no_nv.iter_mut().filter(|t| t.kind == "cuda").for_each(|t| t.ok = false);
        assert_eq!(auto(Prefer::Auto, &both, &no_nv, h264).spec.as_deref(), Some("qsv:/dev/dri/renderD128"));
        // Quick Sync failed too: VAAPI.
        let mut vaapi_only = no_nv.clone();
        vaapi_only.iter_mut().filter(|t| t.kind == "qsv").for_each(|t| t.ok = false);
        assert_eq!(auto(Prefer::Auto, &both, &vaapi_only, h264).spec.as_deref(), Some("vaapi:/dev/dri/renderD128"));
        // Nothing passed: the processor, with the first way's reason.
        let bad: Vec<Trial> = pass.iter().map(|t| trial(t.kind, &t.device, t.codec, false)).collect();
        let r = auto(Prefer::Auto, &both, &bad, h264);
        assert_eq!(r.spec, None);
        assert!(r.why_not.as_deref().unwrap().starts_with("hardware decoding unavailable: NVIDIA (NVDEC) failed the test (Failed to initialise"), "{r:?}");
        // No card: nothing to say. Still testing: pending.
        assert_eq!(auto(Prefer::Auto, &Machine::default(), &pass, h264), Resolved::default());
        let r = resolve_with(Choice::Auto, h264, &both, Prefer::Auto, || None, true, none, || None);
        assert!(r.pending && r.spec.is_none());
        // A camera set to Off is off.
        assert_eq!(resolve_with(Choice::Off, h264, &both, Prefer::Auto, || Some(&pass[..]), true, none, || None).spec, None);
        // AMD only; Intel only (older chip: VAAPI first).
        let amd = machine(vec![node("/dev/dri/renderD128", "amdgpu")], false);
        assert_eq!(auto(Prefer::Nvidia, &amd, &all_pass(&amd), h264).spec.as_deref(), Some("vaapi:/dev/dri/renderD128"));
        let old = machine(vec![old_intel("/dev/dri/renderD128")], false);
        assert_eq!(auto(Prefer::Auto, &old, &all_pass(&old), h264).spec.as_deref(), Some("vaapi:/dev/dri/renderD128"));
    }

    #[test]
    fn a_named_way_that_failed_the_test_is_not_used() {
        let m = machine(vec![node("/dev/dri/renderD128", "xe"), node("/dev/dri/renderD129", "amdgpu")], true);
        let trials = vec![trial("vaapi", "/dev/dri/renderD128", "h264", false), trial("vaapi", "/dev/dri/renderD129", "h264", true), trial("qsv", "/dev/dri/renderD128", "h264", false), trial("cuda", "", "h264", false)];
        let named = |s: &str, codec: Option<Codec>, t: &[Trial]| resolve_with(choice(Some(s)), codec, &m, Prefer::Auto, || Some(t), true, none, || None);
        let r = named("vaapi:/dev/dri/renderD128", Some(Codec::H264), &trials);
        assert_eq!(r.spec, None);
        assert!(r.camera_setting);
        assert!(r.why_not.unwrap().starts_with("VAAPI on /dev/dri/renderD128 failed the test"));
        let r = named("cuda", Some(Codec::H264), &trials);
        assert!(r.why_not.unwrap().starts_with("NVIDIA (NVDEC) failed the test"));
        // No device named: the node where it worked.
        assert_eq!(named("vaapi", Some(Codec::H264), &trials).spec.as_deref(), Some("vaapi:/dev/dri/renderD129"));
        // Quick Sync for the ffmpeg pipeline (codec unknown) is VAAPI on that node.
        assert_eq!(named("qsv:/dev/dri/renderD129", None, &[]).spec.as_deref(), Some("vaapi:/dev/dri/renderD129"));
        assert_eq!(named("qsv:/dev/dri/renderD129", Some(Codec::H264), &[]).spec.as_deref(), Some("qsv:/dev/dri/renderD129"));
        // Names without a device pass through.
        assert_eq!(resolve_with(choice(Some("d3d11va")), Some(Codec::H264), &Machine::default(), Prefer::Auto, || None, false, none, || None).spec.as_deref(), Some("d3d11va"));
    }

    #[test]
    fn a_way_that_failed_while_running_moves_auto_to_the_next() {
        let m = machine(vec![node("/dev/dri/renderD177", "amdgpu"), node("/dev/dri/renderD178", "amdgpu")], false);
        let trials = vec![
            trial("vaapi", "/dev/dri/renderD177", "hevc", true),
            trial("vaapi", "/dev/dri/renderD177", "h264", true),
            trial("vaapi", "/dev/dri/renderD178", "hevc", true),
        ];
        note_failure("vaapi:/dev/dri/renderD177", Some(Codec::H265), "Porch", "decoding errors");
        let now = Instant::now();
        let r = resolve_with(Choice::Auto, Some(Codec::H265), &m, Prefer::Auto, || Some(&trials[..]), true, |s| blocked_for("Porch", s, Some(Codec::H265), now), || None);
        assert_eq!(r.spec.as_deref(), Some("vaapi:/dev/dri/renderD178"), "the next node");
        assert!(r.why_not.unwrap().contains("no camera uses it for that until "));
        let r = resolve_with(Choice::Auto, Some(Codec::H264), &m, Prefer::Auto, || Some(&trials[..]), true, |s| blocked_for("Porch", s, Some(Codec::H264), now), || None);
        assert_eq!(r.spec.as_deref(), Some("vaapi:/dev/dri/renderD177"), "H.264 still on the first");
        assert!(notes().iter().any(|n| n.contains("failed for H.265 on Porch (decoding errors)")), "{:?}", notes());
    }

    #[test]
    fn the_memory_is_per_way_so_a_failed_vaapi_does_not_block_quick_sync_or_nvdec() {
        let intel = Node { path: "/dev/dri/renderD181".into(), driver: "i915".into(), gen12: true };
        let m = machine(vec![intel], true);
        let pass = all_pass(&m);
        let h = Some(Codec::H265);
        note_failure("vaapi:/dev/dri/renderD181", h, "Yard", "Failed to sync surface");
        let now = Instant::now();
        let blocked = |s: &str| blocked_for("Yard", s, h, now);
        assert!(blocked("vaapi:/dev/dri/renderD181").is_some());
        assert!(blocked("qsv:/dev/dri/renderD181").is_none(), "Quick Sync on the same card is another way");
        assert!(blocked("cuda").is_none());
        // The camera's own 15 minutes end before the hour for every camera: the note says the later time.
        note_camera_failure_at("Yard", "vaapi:/dev/dri/renderD181", h, "Failed to sync surface", now, 0);
        let n = blocked("vaapi:/dev/dri/renderD181").unwrap();
        assert!(n.contains("no camera uses it for that until "), "{n}");
        let r = resolve_with(Choice::Auto, h, &m, Prefer::Intel, || Some(&pass[..]), true, blocked, || None);
        assert_eq!(r.spec.as_deref(), Some("qsv:/dev/dri/renderD181"));
        let r = resolve_with(Choice::Auto, h, &m, Prefer::Auto, || Some(&pass[..]), true, blocked, || None);
        assert_eq!(r.spec.as_deref(), Some("cuda"));
        // A camera that names VAAPI waits for it on the processor.
        let r = resolve_with(choice(Some("vaapi:/dev/dri/renderD181")), h, &m, Prefer::Auto, || Some(&pass[..]), true, blocked, || None);
        assert_eq!(r.spec, None);
        assert!(r.why_not.unwrap().ends_with("using the processor until then"));
    }

    /// Tests that empty every camera's failure memory (a setting change) or
    /// rely on it, one at a time.
    static SERIAL: Mutex<()> = Mutex::new(());

    fn serial() -> std::sync::MutexGuard<'static, ()> {
        SERIAL.lock().unwrap_or_else(|e| e.into_inner())
    }

    #[test]
    fn a_camera_retries_a_failed_way_later_and_later() {
        let _s = serial();
        assert_eq!(retry_wait(1), Duration::from_secs(15 * 60));
        assert_eq!(retry_wait(2), Duration::from_secs(30 * 60));
        assert_eq!(retry_wait(3), Duration::from_secs(60 * 60));
        assert_eq!(retry_wait(5), Duration::from_secs(4 * 3600));
        assert_eq!(retry_wait(40), RETRY_MAX, "capped");
        let t0 = Instant::now();
        let spec = "vaapi:/dev/dri/renderD190";
        let h = Some(Codec::H264);
        let f = note_camera_failure_at("Gate", spec, h, "many decoding errors", t0, 1_000_000);
        assert_eq!((f.count, f.until_wall), (1, 1_000_000 + 900));
        assert!(camera_blocked_at("Gate", spec, h, t0 + Duration::from_secs(899)).is_some());
        assert!(camera_blocked_at("Gate", spec, h, t0 + Duration::from_secs(900)).is_none(), "tried again after 15 minutes");
        assert!(camera_blocked_at("Gate", spec, Some(Codec::H265), t0).is_none(), "per codec");
        assert!(camera_blocked_at("Other", spec, h, t0).is_none(), "per camera");
        assert!(camera_blocked_at("Gate", "qsv:/dev/dri/renderD190", h, t0).is_none(), "per way");
        let note = fail_note(spec, &f);
        assert!(note.starts_with("VAAPI on /dev/dri/renderD190 failed once (many decoding errors); trying it again at "), "{note}");
        // It fails again: that camera gives the way up (until a restart or
        // a change of the Hardware setting).
        let t1 = t0 + Duration::from_secs(1000);
        let f = note_camera_failure_at("Gate", spec, h, "Failed to sync surface 0x1", t1, 1_001_000);
        assert_eq!((f.count, f.given_up), (2, true));
        assert!(camera_blocked_at("Gate", spec, h, t1 + Duration::from_secs(30 * 86400)).is_some(), "not tried again");
        assert!(camera_blocked_at("Other", spec, h, t1).is_none(), "other cameras still use it");
        let note = fail_note(spec, &f);
        assert_eq!(note, "VAAPI on /dev/dri/renderD190 failed twice (Failed to sync surface 0x1); not used for this camera until AmberSight restarts or the Hardware setting changes");
        // (A restart or a change of the Hardware setting empties the memory.)
        cam_fails().lock().unwrap().remove(&cam_key("Gate", spec, h));
        // A failure more than 6 hours after the last one counts as the first again.
        let t2 = t1 + Duration::from_secs(100);
        assert_eq!(note_camera_failure_at("Gate", spec, h, "x", t2, 1_100_000).count, 1);
        let t3 = t2 + Duration::from_secs(7 * 3600);
        assert_eq!(note_camera_failure_at("Gate", spec, h, "x", t3, 1_200_000).count, 1);
        // The camera's own memory keeps Auto off that way, with the note.
        let m = machine(vec![node("/dev/dri/renderD190", "amdgpu")], false);
        let pass = all_pass(&m);
        let r = resolve_with(Choice::Auto, h, &m, Prefer::Auto, || Some(&pass[..]), true, |s| blocked_for("Gate", s, h, t3), || None);
        assert_eq!(r.spec, None);
        assert!(r.why_not.unwrap().contains("failed once (x); trying it again at"));
        let r = resolve_with(Choice::Auto, h, &m, Prefer::Auto, || Some(&pass[..]), true, |s| blocked_for("Gate", s, h, t3 + retry_wait(1)), || None);
        assert_eq!(r.spec.as_deref(), Some(spec), "back on the card when the time is up");
    }

    #[test]
    fn a_full_card_is_waited_out_but_never_given_up_for() {
        let _s = serial();
        let t0 = Instant::now();
        let spec = "cuda";
        let h = Some(Codec::H264);
        let oom = "cu->cuCtxCreate(&hwctx->cuda_ctx, desired_flags, hwdev) failed -> CUDA_ERROR_OUT_OF_MEMORY: out of memory";
        for (i, why) in [oom, "hwupload: Cannot allocate memory", oom].iter().enumerate() {
            let t = t0 + Duration::from_secs(60 * i as u64);
            let f = note_camera_failure_at("Full", spec, h, why, t, 1_000_000);
            assert_eq!(f.count, i as u32 + 1, "the wait grows as for any failure");
            assert!(!f.given_up, "memory trouble is never given up for ({why})");
            assert_eq!(f.until_wall, 1_000_000 + retry_wait(i as u32 + 1).as_secs() as i64);
            assert!(fail_note(spec, &f).contains("trying it again at"));
        }
        // Other trouble still counts: the second such failure gives the way up.
        let f = note_camera_failure_at("Full", spec, h, "the decoder stopped", t0 + Duration::from_secs(600), 1_000_000);
        assert!(!f.given_up, "one failure that counts");
        let f = note_camera_failure_at("Full", spec, h, "no pictures came out", t0 + Duration::from_secs(700), 1_000_000);
        assert!(f.given_up, "two that count");
        cam_fails().lock().unwrap().remove(&cam_key("Full", spec, h));
        for (text, mem) in [
            (oom, true),
            ("Cannot allocate memory", true),
            ("cuvidCreateDecoder failed -> CUDA_ERROR_OUT_OF_MEMORY", true),
            ("Not enough memory to create the decoder", true),
            ("cudaErrorMemoryAllocation", true),
            ("hwupload: ENOMEM", true),
            ("E_OUTOFMEMORY", true),
            ("Error initializing the MFX video decoder: memory allocation error (-4)", false),
            ("Failed to allocate decode surface", false),
            ("Failed to create surface: 2 (resource allocation failed)", false),
            ("Could not allocate the decoder", false),
            ("Failed to sync surface 0x1", false),
            ("no pictures came out", false),
            ("many decoding errors: Invalid data found when processing input", false),
        ] {
            assert_eq!(memory_trouble("cuda", text), mem, "{text}");
            assert_eq!(memory_trouble("cuda:0", text), mem, "{text}");
            // QSV and VAAPI: out-of-memory wording is a normal failure.
            assert!(!memory_trouble("qsv", text), "{text}");
            assert!(!memory_trouble("vaapi:/dev/dri/renderD128", text), "{text}");
            // The words themselves (kept from blaming the whole card, on any way).
            assert_eq!(memory_words(text), mem, "{text}");
        }
        // On QSV, "Cannot allocate memory" that keeps coming gives the way up.
        let q = "qsv";
        let f = note_camera_failure_at("FullQ", q, h, "Error while opening decoder: Cannot allocate memory", t0, 1_000_000);
        assert!(!f.given_up);
        let f = note_camera_failure_at("FullQ", q, h, "Error while opening decoder: Cannot allocate memory", t0 + Duration::from_secs(60), 1_000_000);
        assert!(f.given_up, "QSV memory wording counts toward giving up");
        cam_fails().lock().unwrap().remove(&cam_key("FullQ", q, h));
    }

    #[test]
    fn only_failures_in_a_row_give_a_way_up() {
        let _s = serial();
        let t0 = Instant::now();
        let spec = "vaapi:/dev/dri/renderD191";
        let h = Some(Codec::H265);
        assert_eq!(note_camera_failure_at("Row", spec, h, "Failed to sync surface 0x1", t0, 1_000_000).count, 1);
        // Tried again after 15 minutes, it runs well for half an hour: the
        // failure before is forgotten, so the next one is the first again.
        note_camera_ok("Row", spec, h);
        assert!(camera_blocked_at("Row", spec, h, t0).is_none());
        let f = note_camera_failure_at("Row", spec, h, "Failed to sync surface 0x1", t0 + Duration::from_secs(5 * 3600), 1_000_000);
        assert_eq!((f.count, f.given_up), (1, false));
        // Without the good run in between it is given up.
        let f = note_camera_failure_at("Row", spec, h, "Failed to sync surface 0x1", t0 + Duration::from_secs(6 * 3600), 1_000_000);
        assert_eq!((f.count, f.given_up), (2, true));
        note_camera_ok("Other", spec, h);
        assert!(camera_blocked_at("Row", spec, h, t0 + Duration::from_secs(7 * 3600)).is_some(), "per camera");
        cam_fails().lock().unwrap().remove(&cam_key("Row", spec, h));
        assert_eq!(HEALTHY_RUN, Duration::from_secs(30 * 60));
    }

    #[test]
    fn changing_the_memory_kept_free_starts_the_failure_memory_afresh() {
        let _s = serial();
        let t0 = Instant::now();
        let spec = "cuda";
        let h = Some(Codec::H264);
        note_camera_failure_at("Clr", spec, h, "the decoder stopped", t0, 1_000_000);
        assert!(note_camera_failure_at("Clr", spec, h, "the decoder stopped", t0, 1_000_000).given_up);
        assert!(set_min_free_mb(DEFAULT_MIN_FREE_MB + 7));
        assert!(camera_blocked_at("Clr", spec, h, t0).is_none(), "tried again after the change");
        set_min_free_mb(DEFAULT_MIN_FREE_MB);
    }

    #[test]
    fn cameras_on_nvdec_stay_there_after_a_setting_change() {
        let _s = serial();
        let h264 = Some(Codec::H264);
        let m = machine(vec![node("/dev/dri/renderD129", "nvidia"), Node { path: "/dev/dri/renderD128".into(), driver: "i915".into(), gen12: true }], true);
        let pass = all_pass(&m);
        let min = DEFAULT_MIN_FREE_MB as u64;
        let gpu = |free_mb| vram::Reading::Ok(vram::Gpu { free_mb, total_mb: 16311 });
        let free = || match vram::reading() {
            vram::Reading::Ok(g) => g.free_mb,
            r => panic!("{r:?}"),
        };
        // What Auto picks now (as a recheck or a log line looks), from the
        // card's reading.
        let look = |cam: &str| {
            let rd = vram::reading();
            let full = std::cell::Cell::new(false);
            let r = resolve_with(Choice::Auto, h264, &m, Prefer::Auto, || Some(&pass[..]), true, none, || {
                let w = guard_full(cam, Prefer::Auto, rd, min);
                full.set(full.get() || w.is_some());
                w
            });
            Resolved { nvidia_full: full.get(), ..r }
        };
        let cams: Vec<String> = (0..8).map(|i| format!("Keep{i}")).collect();
        // Room for 8 decoders: each start takes its memory once.
        vram::set_for_test(gpu(min + vram::DECODER_MB * 8));
        let mut holds = Vec::new();
        for c in &cams {
            let r = look(c);
            assert_eq!(r.spec.as_deref(), Some("cuda"), "{c}");
            for _ in 0..3 {
                look(c);
            }
            holds.push(started(c, &r).expect("on NVDEC"));
        }
        assert_eq!(free(), min, "a look takes no memory, a start takes it once");
        // The next real reading, near the limit (it counts their real use).
        vram::set_for_test(gpu(min + 100));
        // A setting changes: every decoder on Auto looks again, and none
        // moves off the card.
        assert!(set_min_free_mb(DEFAULT_MIN_FREE_MB + 1));
        set_min_free_mb(DEFAULT_MIN_FREE_MB);
        for c in &cams {
            let r = look(c);
            assert_eq!(r.spec.as_deref(), Some("cuda"), "{c} stays");
            assert!(!worth_moving_in(Some("cuda"), r.spec.as_deref(), true, &candidates(Prefer::Auto, &m, h264)));
        }
        assert_eq!(free(), min + 100, "the looks took no memory");
        // A new camera has no room; turned away, it needs 1 GB more to come.
        let r = look("New9");
        assert_eq!(r.spec.as_deref(), Some("qsv:/dev/dri/renderD128"));
        assert!(r.nvidia_full);
        assert!(started("New9", &r).is_none());
        vram::set_for_test(gpu(min + vram::DECODER_MB + vram::BACK_MARGIN_MB - 1));
        assert_eq!(look("New9").spec.as_deref(), Some("qsv:/dev/dri/renderD128"));
        vram::set_for_test(gpu(min + vram::DECODER_MB + vram::BACK_MARGIN_MB));
        let r = look("New9");
        assert_eq!(r.spec.as_deref(), Some("cuda"));
        let h9 = started("New9", &r);
        assert!(h9.is_some() && !list_has(&DENIED, "New9"));
        // A decoder that stops leaves the card: that camera is judged anew.
        vram::set_for_test(gpu(10));
        drop(holds.remove(0));
        assert_ne!(look("Keep0").spec.as_deref(), Some("cuda"), "stopped: no longer held");
        assert_eq!(look("Keep1").spec.as_deref(), Some("cuda"), "the others stay");
        // Two decoders of one camera: it stays held while one runs.
        let r = Resolved { spec: Some("cuda".into()), ..Default::default() };
        let a = started("Twice", &r);
        let b = started("Twice", &r);
        drop(a);
        assert!(list_has(&HELD, "Twice"));
        drop(b);
        assert!(!list_has(&HELD, "Twice"));
        drop(h9);
        drop(holds);
        vram::set_for_test(vram::Reading::Unknown);
    }

    #[test]
    fn a_camera_on_nvdec_near_the_limit_stays_there_when_it_reconnects() {
        let _s = serial();
        let h264 = Some(Codec::H264);
        let m = machine(vec![node("/dev/dri/renderD129", "nvidia"), Node { path: "/dev/dri/renderD128".into(), driver: "i915".into(), gen12: true }], true);
        let pass = all_pass(&m);
        let min = DEFAULT_MIN_FREE_MB as u64;
        let gpu = |free_mb| vram::Reading::Ok(vram::Gpu { free_mb, total_mb: 16311 });
        let look = |cam: &str| {
            let rd = vram::reading();
            let full = std::cell::Cell::new(false);
            let r = resolve_with(Choice::Auto, h264, &m, Prefer::Auto, || Some(&pass[..]), true, none, || {
                let w = guard_full(cam, Prefer::Auto, rd, min);
                full.set(full.get() || w.is_some());
                w
            });
            Resolved { nvidia_full: full.get(), ..r }
        };
        vram::set_for_test(gpu(min + vram::DECODER_MB));
        let r = look("Recon");
        assert_eq!(r.spec.as_deref(), Some("cuda"));
        let mut hold = started("Recon", &r);
        assert!(hold.is_some());
        // The card's reading counts this camera's own decoder: 100 MB over
        // the limit, no room for one more.
        vram::set_for_test(gpu(min + 100));
        // Reconnects and stream changes, as the workers do them: the place
        // is kept until the new decoder has started, then swapped.
        for _ in 0..5 {
            let r = look("Recon");
            assert_eq!(r.spec.as_deref(), Some("cuda"), "stays on the card");
            assert!(!r.nvidia_full);
            let h = started("Recon", &r);
            assert!(h.is_some());
            hold = h;
            assert!(list_has(&HELD, "Recon") && !list_has(&DENIED, "Recon"));
        }
        assert_eq!(vram::reading(), gpu(min + 100), "a restart takes no more memory");
        // (Letting the place go before the new start moved it off: the bug.)
        drop(hold);
        let r = look("Recon");
        assert_ne!(r.spec.as_deref(), Some("cuda"));
        assert!(started("Recon", &r).is_none());
        list_put(&DENIED, "Recon", false);
        vram::set_for_test(vram::Reading::Unknown);
    }

    #[test]
    fn the_place_on_the_card_is_kept_through_a_reconnect_but_not_a_long_outage() {
        let _s = serial();
        let h264 = Some(Codec::H264);
        let m = machine(vec![node("/dev/dri/renderD129", "nvidia"), Node { path: "/dev/dri/renderD128".into(), driver: "i915".into(), gen12: true }], true);
        let pass = all_pass(&m);
        let min = DEFAULT_MIN_FREE_MB as u64;
        let gpu = |free_mb| vram::Reading::Ok(vram::Gpu { free_mb, total_mb: 16311 });
        let look = |cam: &str| {
            let rd = vram::reading();
            resolve_with(Choice::Auto, h264, &m, Prefer::Auto, || Some(&pass[..]), true, none, || guard_full(cam, Prefer::Auto, rd, min))
        };
        // On the card, which is then near its limit.
        vram::set_for_test(gpu(min + vram::DECODER_MB));
        let r = look("Away");
        let mut hold = started("Away", &r);
        assert!(hold.is_some());
        vram::set_for_test(gpu(min + 100));
        // As the workers do it: the place goes only once the outage lapses.
        let keep = |out: &Outage, at: Instant, hold: &mut Option<NvdecHold>| {
            if out.lapsed(at) {
                *hold = None;
            }
        };
        let t0 = Instant::now();
        let mut out = Outage::default();
        assert!(!out.lapsed(t0 + Duration::from_secs(86400)), "no outage before a session ends");
        // A short reconnect: a working session ends, the next ones fail for a
        // minute, then one works again. The place is kept, and it is still
        // on the card when it comes back.
        out.ended(true, t0);
        out.ended(false, t0 + Duration::from_secs(30));
        out.ended(false, t0 + Duration::from_secs(60));
        keep(&out, t0 + HOLD_THROUGH, &mut hold);
        assert!(hold.is_some() && list_has(&HELD, "Away"), "kept through a short gap");
        let r = look("Away");
        assert_eq!(r.spec.as_deref(), Some("cuda"), "back on the card");
        hold = started("Away", &r);
        // Working for a while, then a long outage (failed tries do not start it over).
        let t1 = t0 + Duration::from_secs(3600);
        out.ended(true, t1);
        for i in 1..10 {
            out.ended(false, t1 + Duration::from_secs(20 * i));
        }
        keep(&out, t1 + HOLD_THROUGH, &mut hold);
        assert!(hold.is_some(), "not yet");
        keep(&out, t1 + HOLD_THROUGH + Duration::from_secs(1), &mut hold);
        assert!(hold.is_none() && !list_has(&HELD, "Away"), "let go after the long outage");
        // Back, it is judged as a new camera: no room on the card.
        let r = look("Away");
        assert_ne!(r.spec.as_deref(), Some("cuda"));
        assert!(started("Away", &r).is_none());
        // With room it comes back, and its start counts its memory again.
        vram::set_for_test(gpu(min + vram::DECODER_MB + vram::BACK_MARGIN_MB));
        let r = look("Away");
        assert_eq!(r.spec.as_deref(), Some("cuda"));
        let h = started("Away", &r);
        assert_eq!(vram::reading(), gpu(min + vram::BACK_MARGIN_MB), "reserved as for a new camera");
        drop(h);
        list_put(&DENIED, "Away", false);
        vram::set_for_test(vram::Reading::Unknown);
    }

    #[test]
    fn a_working_decoder_moves_only_for_the_better() {
        let order: Vec<String> = ["cuda", "qsv:/dev/dri/renderD128", "vaapi:/dev/dri/renderD128"].map(String::from).into();
        let w = |run: Option<&str>, new: Option<&str>, changed: bool| worth_moving_in(run, new, changed, &order);
        // From the processor (or after a fallback) to a card: yes.
        assert!(w(None, Some("vaapi:/dev/dri/renderD128"), false));
        // A working NVDEC is not moved off because NVDEC failed for another camera.
        assert!(!w(Some("cuda"), None, false));
        assert!(!w(Some("cuda"), Some("qsv:/dev/dri/renderD128"), false));
        // Back to a better way once it may be used again.
        assert!(w(Some("vaapi:/dev/dri/renderD128"), Some("cuda"), false));
        // The setting changed: whatever it says now.
        assert!(w(Some("cuda"), None, true));
        assert!(w(Some("cuda"), Some("vaapi:/dev/dri/renderD128"), true));
        assert!(!w(Some("cuda"), Some("cuda"), true));
        assert!(!w(None, None, false));
    }

    #[test]
    fn hardware_decoders_start_apart() {
        let last = Mutex::new(None);
        let t = Instant::now();
        let slots: Vec<Duration> = (0..4).map(|_| reserve_in(&last, t) - t).collect();
        assert_eq!(slots, [Duration::ZERO, START_GAP, START_GAP * 2, START_GAP * 3]);
        // Long after: at once.
        let later = t + Duration::from_secs(10);
        assert_eq!(reserve_in(&last, later), later);
    }

    #[test]
    fn cards_that_failed_every_way_are_noted() {
        let intel = Node { path: "/dev/dri/renderD128".into(), driver: "i915".into(), gen12: true };
        let m = machine(vec![intel], true);
        // VAAPI failed but Quick Sync works: nothing to say about the node.
        let mut t = all_pass(&m);
        t.iter_mut().filter(|x| x.kind == "vaapi").for_each(|x| x.ok = false);
        assert!(test_notes(&m, Prefer::Auto, &t).is_empty(), "{:?}", test_notes(&m, Prefer::Auto, &t));
        // NVDEC failed both codecs alike: said once, with what is used.
        t.iter_mut().filter(|x| x.kind == "cuda").for_each(|x| {
            x.ok = false;
            x.reason = Some("cannot load libcuda.so.1".into());
        });
        let n = test_notes(&m, Prefer::Auto, &t);
        assert_eq!(n, ["Hardware decoding unavailable on the NVIDIA card (cannot load libcuda.so.1); Intel Quick Sync on /dev/dri/renderD128 is used instead"]);
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
        // libva's version line on every start is not the reason.
        assert_eq!(reason_from_stderr("libva info: VA-API version 1.23.0\n[h264 @ 0x1] decode failed\n").unwrap(), "decode failed");
        assert_eq!(reason_from_stderr("libva info: VA-API version 1.23.0\n"), None);
        assert_eq!(reason_from_stderr("\n\n"), None);
        let q = "[QSV @ 0x5ecc] Error creating a MFX session: -9.\nDevice creation failed: -1313558101.\n";
        assert_eq!(reason_from_stderr(q).unwrap(), QSV_NO_RUNTIME);
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
