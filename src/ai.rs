//! Built-in AI object detection (ONNX Runtime + a YOLOX model).
//!
//! YOLOX (Apache-2.0) models from the official release work as-is, e.g.
//! `yolox_s.onnx` (640x640, more accurate), `yolox_l.onnx` (640x640, most
//! accurate, about five times the work) or `yolox_tiny.onnx` (416x416, faster). The model input size is read from the file. Runs on a
//! graphics card where this build can use one (Windows: DirectML, any
//! DirectX 12 card; 64-bit Linux PCs: an NVIDIA card with CUDA 13 and cuDNN 9,
//! else Intel graphics with OpenVINO, else AMD graphics through a helper
//! program (WebGPU), see gpu_addon.rs, openvino.rs and ai_amd.rs; Apple
//! silicon Macs: Core ML), else on the CPU. On a card the checks run on a
//! thread of their own with a time limit (ai_card.rs); detection
//! only runs while motion is happening, so a few hundred milliseconds per
//! frame on the CPU is fine.

use anyhow::{anyhow, Result};
#[cfg(not(all(target_os = "macos", target_arch = "x86_64")))]
use anyhow::Context;
#[cfg(not(all(target_os = "macos", target_arch = "x86_64")))]
use ort::session::Session;
use std::path::Path;
#[cfg(not(all(target_os = "macos", target_arch = "x86_64")))]
use std::sync::Mutex;

#[cfg_attr(all(target_os = "macos", target_arch = "x86_64"), allow(dead_code))]
pub const COCO_LABELS: [&str; 80] = [
    "person", "bicycle", "car", "motorcycle", "airplane", "bus", "train", "truck", "boat", "traffic light",
    "fire hydrant", "stop sign", "parking meter", "bench", "bird", "cat", "dog", "horse", "sheep", "cow",
    "elephant", "bear", "zebra", "giraffe", "backpack", "umbrella", "handbag", "tie", "suitcase", "frisbee",
    "skis", "snowboard", "sports ball", "kite", "baseball bat", "baseball glove", "skateboard", "surfboard",
    "tennis racket", "bottle", "wine glass", "cup", "fork", "knife", "spoon", "bowl", "banana", "apple",
    "sandwich", "orange", "broccoli", "carrot", "hot dog", "pizza", "donut", "cake", "chair", "couch",
    "potted plant", "bed", "dining table", "toilet", "tv", "laptop", "mouse", "remote", "keyboard",
    "cell phone", "microwave", "oven", "toaster", "sink", "refrigerator", "book", "clock", "vase",
    "scissors", "teddy bear", "hair drier", "toothbrush",
];

/// A detected object; box in normalized image coordinates (0..1).
#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct Detection {
    pub label: String,
    pub score: f32,
    pub x: f32,
    pub y: f32,
    pub w: f32,
    pub h: f32,
}

#[cfg(not(all(target_os = "macos", target_arch = "x86_64")))]
pub struct AiDetector {
    run: Runner,
    size: usize,
    /// The graphics card it runs on (None: the CPU).
    gpu: Option<String>,
    // Keeps the runtime environment alive for the session.
    _env: Option<ort::environment::Environment>,
}

/// Where the checks run: here, on the caller's thread (the CPU), or on the
/// graphics card's own thread with a time limit (see ai_card.rs).
#[cfg(not(all(target_os = "macos", target_arch = "x86_64")))]
enum Runner {
    Here(Mutex<Session>),
    Card(crate::ai_card::CardThread),
    /// Tests: a processor detector with no model (`fake::cpu`).
    #[cfg(test)]
    Fake(Box<dyn Fn(crate::ai_card::Input) -> Result<crate::ai_card::Output, String> + Send + Sync>),
}

/// Why the last model load is not on the graphics card although one was
/// wanted and found (None: it is, or none was wanted or found), for Settings.
#[cfg(not(all(target_os = "macos", target_arch = "x86_64")))]
static GPU_NOTE: Mutex<Option<String>> = Mutex::new(None);

/// Why the AI is not on the graphics card (see `GPU_NOTE`); on Linux, a
/// download of an add-on in progress or failed comes first.
#[cfg(not(all(target_os = "macos", target_arch = "x86_64")))]
pub fn gpu_note() -> Option<String> {
    let note = GPU_NOTE.lock().unwrap().clone();
    // (gpu_addon::note is None once a usable copy is there, so a stale
    // download failure never hides the real reason.)
    #[cfg(all(target_os = "linux", target_arch = "x86_64", target_env = "gnu"))]
    if note.is_some() && crate::ai_card::blocked().is_none() {
        for a in [crate::gpu_addon::Addon::Cuda, crate::gpu_addon::Addon::OpenVino, crate::gpu_addon::Addon::Amd] {
            if let Some(n) = crate::gpu_addon::note(a) {
                return Some(n);
            }
        }
    }
    note
}

#[cfg(all(target_os = "macos", target_arch = "x86_64"))]
pub fn gpu_note() -> Option<String> {
    None
}

/// Say why the AI is on the processor now (the card gave errors while running).
#[cfg(not(all(target_os = "macos", target_arch = "x86_64")))]
pub fn set_gpu_note(note: Option<String>) {
    *GPU_NOTE.lock().unwrap() = note;
}

#[cfg(all(target_os = "macos", target_arch = "x86_64"))]
pub fn set_gpu_note(_note: Option<String>) {}

/// The AI runtime allows one environment per process: make it once, share it
/// (switching models makes a new session in the same environment).
#[cfg(not(all(target_os = "macos", target_arch = "x86_64")))]
pub(crate) fn runtime() -> Result<ort::environment::Environment> {
    static ENV: std::sync::Mutex<Option<ort::environment::Environment>> = std::sync::Mutex::new(None);
    let mut g = ENV.lock().unwrap();
    if let Some(e) = g.as_ref() {
        return Ok(e.clone());
    }
    let e = ort::init().with_name("ambersight").build().map_err(|e| anyhow!("AI runtime: {e}"))?;
    *g = Some(e.clone());
    Ok(e)
}

/// The model's input size ([1, 3, S, S]); 640 when it is dynamic.
#[cfg(not(all(target_os = "macos", target_arch = "x86_64")))]
fn input_size_of(session: &Session) -> usize {
    match session.inputs().first().map(|i| i.dtype()) {
        Some(ort::value::ValueType::Tensor { shape, .. }) => shape.get(2).copied().filter(|d| *d > 0).unwrap_or(640) as usize,
        _ => 640,
    }
}

/// Run a session on one input: its first output (shape, values).
#[cfg(not(all(target_os = "macos", target_arch = "x86_64")))]
fn run_session(session: &mut Session, input: crate::ai_card::Input) -> Result<crate::ai_card::Output, String> {
    let tensor = ort::value::Tensor::from_array(input).map_err(|e| format!("AI input: {e}"))?;
    let outputs = session.run(ort::inputs![tensor]).map_err(|e| e.to_string())?;
    let out = outputs[0].try_extract_array::<f32>().map_err(|e| format!("AI output: {e}"))?;
    Ok((out.shape().to_vec(), out.iter().copied().collect()))
}

/// Not worth a warning: graphics built into an Intel processor without
/// Intel's compute driver (most such computers never meant the AI to use
/// them; Settings still says what to install).
#[cfg(all(target_os = "linux", target_arch = "x86_64", target_env = "gnu"))]
fn quiet_reason(card: &crate::gpu::Gpu, why: &str) -> bool {
    card.api == "OpenVINO" && crate::gpu::built_in(&card.bus) && crate::openvino::is_driver_missing(why)
}

#[cfg(not(all(target_os = "linux", target_arch = "x86_64", target_env = "gnu")))]
#[cfg_attr(all(target_os = "macos", target_arch = "x86_64"), allow(dead_code))]
fn quiet_reason(_card: &crate::gpu::Gpu, _why: &str) -> bool {
    false
}

/// Whether `card` is the one named in the settings (not picked by the AI:
/// "Automatic", or a name no card listed has).
#[cfg_attr(all(target_os = "macos", target_arch = "x86_64"), allow(dead_code))]
fn named(card: &crate::gpu::Gpu, gpu_name: Option<&str>) -> bool {
    gpu_name.is_some_and(|n| !n.is_empty() && n == card.name)
}

/// Whether another kind of card may stand in for `card` when it cannot be
/// used: an NVIDIA or Intel card the AI picked itself, not one named in the
/// settings (`fall_over_cards` has the order).
#[cfg_attr(all(target_os = "macos", target_arch = "x86_64"), allow(dead_code))]
fn may_fall_over(card: &crate::gpu::Gpu, gpu_name: Option<&str>) -> bool {
    matches!(card.api, "CUDA" | "OpenVINO") && !named(card, gpu_name)
}

/// Whether `card`'s own add-on is being fetched now (the AI is loaded again
/// when that try ends, so no other card is tried meanwhile: that would start
/// a second download and move the AI twice; while a failed download waits to
/// try again, other cards are tried).
#[cfg(all(target_os = "linux", target_arch = "x86_64", target_env = "gnu"))]
fn add_on_coming(card: &crate::gpu::Gpu) -> bool {
    use crate::gpu_addon::{downloading, Addon};
    match card.api {
        "CUDA" => downloading(Addon::Cuda),
        "OpenVINO" => downloading(Addon::OpenVino),
        "WebGPU" => downloading(Addon::Amd),
        _ => false,
    }
}

#[cfg(not(all(target_os = "linux", target_arch = "x86_64", target_env = "gnu")))]
#[cfg_attr(all(target_os = "macos", target_arch = "x86_64"), allow(dead_code))]
fn add_on_coming(_card: &crate::gpu::Gpu) -> bool {
    false
}

/// Whether to try other cards now that `card` cannot be used: one the AI
/// picked (`may_fall_over`) whose add-on is not on its way (`add_on_coming`).
#[cfg_attr(all(target_os = "macos", target_arch = "x86_64"), allow(dead_code))]
fn falls_over(card: &crate::gpu::Gpu, gpu_name: Option<&str>) -> bool {
    may_fall_over(card, gpu_name) && !add_on_coming(card)
}

/// The cards to try, in turn, when `first` cannot be used (see
/// `may_fall_over`): the best Intel card after an NVIDIA one, then the
/// best AMD one (the order `gpu::pick` ranks them in).
#[cfg_attr(all(target_os = "macos", target_arch = "x86_64"), allow(dead_code))]
fn fall_over_cards(first: &crate::gpu::Gpu, cards: &[crate::gpu::Gpu]) -> Vec<crate::gpu::Gpu> {
    ["OpenVINO", "WebGPU"]
        .into_iter()
        .filter(|api| crate::gpu::rank(api) < crate::gpu::rank(first.api))
        .filter_map(|api| {
            let kind: Vec<crate::gpu::Gpu> = cards.iter().filter(|g| g.api == api).cloned().collect();
            crate::gpu::pick(&kind, None)
        })
        .collect()
}

/// Settings' note when no card could be used: why the first one was not,
/// then why each one tried after it was not.
#[cfg_attr(all(target_os = "macos", target_arch = "x86_64"), allow(dead_code))]
fn fall_over_note(first: &str, also: &[(String, String)]) -> String {
    let mut note = first.to_string();
    for (name, why) in also {
        note.push_str(&format!("; {name} was tried instead: {why}"));
    }
    note
}

/// Fake detectors for tests of what the engine does with the card (no
/// model, no card): the model path `MODEL` loads them.
#[cfg(all(test, not(all(target_os = "macos", target_arch = "x86_64"))))]
pub(crate) mod fake {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;

    pub const MODEL: &str = "/nowhere/fake-model-for-tests.onnx";

    /// What a move onto the card gets (None: no card).
    pub type MakeCard = Arc<dyn Fn() -> Option<AiDetector> + Send + Sync>;
    pub static CARD: Mutex<Option<MakeCard>> = Mutex::new(None);
    /// Checks done by fake processor detectors.
    pub static CPU_CHECKS: AtomicUsize = AtomicUsize::new(0);

    pub fn is_fake(model: &Path) -> bool {
        model == Path::new(MODEL)
    }

    /// An empty answer of the model's shape (nothing found).
    pub fn nothing() -> crate::ai_card::Output {
        (vec![1, 0, 85], Vec::new())
    }

    pub fn cpu() -> AiDetector {
        let run = |_i: crate::ai_card::Input| {
            CPU_CHECKS.fetch_add(1, Ordering::SeqCst);
            Ok(nothing())
        };
        AiDetector { run: Runner::Fake(Box::new(run)), size: 32, gpu: None, _env: None }
    }

    /// A card ("OpenVINO") whose checks `run` does, with this time limit.
    pub fn card<E: Into<crate::ai_card::RunError> + 'static>(deadline: std::time::Duration, run: impl FnMut(crate::ai_card::Input) -> Result<crate::ai_card::Output, E> + Send + 'static) -> AiDetector {
        let (t, _) = crate::ai_card::CardThread::start("OpenVINO", std::time::Duration::from_secs(5), move || Ok((run, ()))).unwrap();
        AiDetector { run: Runner::Card(t.with_deadline(deadline)), size: 32, gpu: Some("Test card".into()), _env: None }
    }
}

/// What `AiDetector::try_card` came to.
#[cfg_attr(all(target_os = "macos", target_arch = "x86_64"), allow(dead_code))]
pub enum CardTry {
    OnCard(AiDetector),
    /// No card, or it cannot be used now, and what Settings' note should say
    /// (None: nothing). The caller writes the note (`set_gpu_note`), once it
    /// knows its result is the one in use.
    NotUsed(Option<String>),
}

/// What a session made on a card says about itself.
#[cfg(not(all(target_os = "macos", target_arch = "x86_64")))]
struct OnCard {
    size: usize,
    /// The card's name as shown ("Intel(R) Arc(TM) B580 Graphics").
    name: String,
}

#[cfg(not(all(target_os = "macos", target_arch = "x86_64")))]
impl AiDetector {
    /// On the graphics card where possible (see `load_on`).
    pub fn load(model: &Path, threads: usize) -> Result<Self> {
        Self::load_on(model, threads, true, None, 1536)
    }

    /// `gpu`: run on a graphics card where this build can (see the top of
    /// this file, and `try_card`), waiting for it; the CPU if none can be
    /// used, or the cards are off for the AI until a restart
    /// (`ai_card::blocked`). `gpu_memory_mb` caps the memory pool on an
    /// NVIDIA card. (The engine does not wait: it starts on the CPU and
    /// moves to the card in the background.)
    pub fn load_on(model: &Path, threads: usize, gpu: bool, gpu_name: Option<&str>, gpu_memory_mb: u32) -> Result<Self> {
        if !gpu {
            #[cfg(all(target_os = "linux", target_arch = "x86_64", target_env = "gnu"))]
            crate::gpu_addon::none_wanted();
            *GPU_NOTE.lock().unwrap() = None;
            return Self::load_cpu(model, threads);
        }
        match Self::try_card(model, threads, gpu_name, gpu_memory_mb) {
            CardTry::OnCard(d) => {
                *GPU_NOTE.lock().unwrap() = None;
                Ok(d)
            }
            CardTry::NotUsed(note) => {
                *GPU_NOTE.lock().unwrap() = note;
                Self::load_cpu(model, threads)
            }
        }
    }

    /// On the processor only (Settings' note about the card is left as it is).
    pub fn load_cpu(model: &Path, threads: usize) -> Result<Self> {
        #[cfg(test)]
        if fake::is_fake(model) {
            return Ok(fake::cpu());
        }
        let env = runtime()?;
        let session = Session::builder(&env)
            .map_err(|e| anyhow!("AI runtime: {e}"))?
            .with_intra_threads(threads.max(1))
            .map_err(|e| anyhow!("AI runtime: {e}"))?
            .commit_from_file(model)
            .map_err(|e| anyhow!("could not load AI model {}: {e}", model.display()))?;
        Ok(AiDetector { size: input_size_of(&session), run: Runner::Here(Mutex::new(session)), gpu: None, _env: Some(env) })
    }

    /// On a graphics card, waiting for its session and one first check: the
    /// one named `gpu_name`, else the best one (`gpu::pick`); an NVIDIA card
    /// that cannot be used gives way to an Intel one, then an AMD one, and an
    /// Intel card to an AMD one, unless it was named (`fall_over_cards`).
    /// Says why not when it is not used.
    pub fn try_card(model: &Path, threads: usize, gpu_name: Option<&str>, gpu_memory_mb: u32) -> CardTry {
        if let Some(b) = crate::ai_card::blocked() {
            return CardTry::NotUsed(Some(b));
        }
        #[cfg(test)]
        if fake::is_fake(model) {
            let make = fake::CARD.lock().unwrap().clone();
            return match make.and_then(|m| m()) {
                Some(d) => CardTry::OnCard(d),
                None => CardTry::NotUsed(Some("no test card".into())),
            };
        }
        let env = match runtime() {
            Ok(e) => e,
            Err(e) => return CardTry::NotUsed(Some(format!("{e:#}"))),
        };
        let cards = crate::gpu::list();
        let Some(card) = crate::gpu::pick(&cards, gpu_name) else {
            // No card: a download waiting to retry stops.
            #[cfg(all(target_os = "linux", target_arch = "x86_64", target_env = "gnu"))]
            crate::gpu_addon::none_wanted();
            return CardTry::NotUsed(None);
        };
        // AMD graphics run in a helper program (ai_amd.rs), not in this runtime.
        #[cfg(all(target_os = "linux", target_arch = "x86_64", target_env = "gnu"))]
        if card.api == "WebGPU" {
            return Self::try_amd(model, threads, &card, &cards, gpu_name);
        }
        let why = match on_card(&env, model, &card, &cards, gpu_name, threads.max(1), gpu_memory_mb) {
            Ok((thread, found)) => return CardTry::OnCard(AiDetector { run: Runner::Card(thread), size: found.size, gpu: Some(found.name), _env: Some(env) }),
            Err(why) => why,
        };
        // Picked by the AI and cannot be used: the next kind of card may do.
        let mut also: Vec<(String, String)> = Vec::new();
        if falls_over(&card, gpu_name) {
            for next in fall_over_cards(&card, &cards) {
                if crate::ai_card::blocked().is_some() {
                    break;
                }
                if quiet_reason(&card, &why) {
                    tracing::info!("AI: the graphics card {} cannot be used ({why}); trying {}", card.name, next.name);
                } else {
                    tracing::warn!("AI: the graphics card {} cannot be used ({why}); trying {}", card.name, next.name);
                }
                #[cfg(all(target_os = "linux", target_arch = "x86_64", target_env = "gnu"))]
                if next.api == "WebGPU" {
                    match Self::try_amd(model, threads, &next, &cards, gpu_name) {
                        CardTry::OnCard(d) => return CardTry::OnCard(d),
                        CardTry::NotUsed(n) => also.push((next.name.clone(), n.unwrap_or_else(|| "it cannot be used".into()))),
                    }
                    continue;
                }
                match on_card(&env, model, &next, &cards, gpu_name, threads.max(1), gpu_memory_mb) {
                    Ok((thread, found)) => return CardTry::OnCard(AiDetector { run: Runner::Card(thread), size: found.size, gpu: Some(found.name), _env: Some(env) }),
                    Err(w) => {
                        tracing::warn!("AI: the graphics card {} cannot be used either ({w})", next.name);
                        also.push((next.name.clone(), w));
                        // Its add-on is on its way: the AI waits for it (no AMD try meanwhile).
                        if add_on_coming(&next) {
                            break;
                        }
                    }
                }
            }
        }
        if quiet_reason(&card, &why) {
            tracing::info!("AI: the graphics card {} cannot be used ({why}); using the CPU", card.name);
        } else {
            tracing::warn!("AI: the graphics card {} cannot be used ({why}); using the CPU", card.name);
        }
        CardTry::NotUsed(Some(fall_over_note(&why, &also)))
    }

    /// AMD graphics: the Vulkan driver and the add-on checked (the add-on is
    /// downloaded when it is missing), then the helper program on a card
    /// thread, timed against the processor when the AI picked the chip
    /// itself (`ai_amd::load`).
    #[cfg(all(target_os = "linux", target_arch = "x86_64", target_env = "gnu"))]
    fn try_amd(model: &Path, threads: usize, card: &crate::gpu::Gpu, cards: &[crate::gpu::Gpu], gpu_name: Option<&str>) -> CardTry {
        use crate::gpu_addon::Addon;
        let not_used = |why: String, quiet: bool| {
            if quiet {
                tracing::info!("AI: the graphics card {} cannot be used ({why}); using the CPU", card.name);
            } else {
                tracing::warn!("AI: the graphics card {} cannot be used ({why}); using the CPU", card.name);
            }
            CardTry::NotUsed(Some(why))
        };
        let render: Vec<String> = cards.iter().filter(|g| g.api == "WebGPU").map(|g| g.render.clone()).filter(|r| !r.is_empty()).collect();
        let icd = match crate::ai_amd::driver(&render) {
            Ok(p) => p,
            Err(why) => {
                let quiet = crate::ai_amd::is_driver_missing(&why);
                return not_used(why, quiet);
            }
        };
        let dir = match crate::gpu_addon::find(Addon::Amd) {
            Ok(Some(d)) => d,
            Ok(None) => {
                crate::gpu_addon::start_download(Addon::Amd);
                return CardTry::NotUsed(Some(crate::gpu_addon::note(Addon::Amd).unwrap_or_else(|| "the AMD graphics add-on for the AI is not installed".into())));
            }
            Err(why) => return not_used(why, false),
        };
        // The helper's home and shader cache: a folder of this user alone.
        let cache = match crate::gpu_addon::data_dir().map(|d| crate::ai_amd::cache_dir(&d)) {
            Some(c) => c,
            None => return not_used("the data folder is not known".into(), false),
        };
        if let Err(why) = crate::gpu_addon::private_dir(&cache) {
            return not_used(why, false);
        }
        if let Some(d) = crate::gpu_addon::data_dir() {
            crate::gpu_addon::remove_other_versions(&d.join("cache"), "amd-", &cache);
        }
        let spec = crate::ai_amd::Spec::for_addon(&dir, &icd, &cache);
        // Chosen in Settings (by name): used however fast it is. A saved name
        // that matches no card listed is "Automatic", and so is a fall-over.
        let chosen = named(card, gpu_name);
        let cpu_times = || -> Result<Vec<std::time::Duration>, String> {
            let cpu = Self::load_cpu(model, threads).map_err(|e| format!("{e:#}"))?;
            cpu.time_checks(6).map(|t| t[1..].to_vec())
        };
        match crate::ai_amd::load(spec, model, !chosen, &cpu_times, &crate::ai_amd::SPEED) {
            Ok(l) => {
                // Shown as the card list names it (the same name Settings
                // offers); ONNX Runtime does not tell the helper the chip's own name.
                tracing::debug!("AI: the AMD graphics helper runs on {}", l.name);
                CardTry::OnCard(AiDetector { run: Runner::Card(l.thread), size: l.size, gpu: Some(card.name.clone()), _env: None })
            }
            Err(why) => {
                // A chip slower than the processor is not a fault: the note says so.
                not_used(why.clone(), why.contains("slower than the processor"))
            }
        }
    }

    /// `n` timed checks on a blank picture (the first one warms up).
    #[cfg(all(target_os = "linux", target_arch = "x86_64", target_env = "gnu"))]
    fn time_checks(&self, n: usize) -> Result<Vec<std::time::Duration>, String> {
        let mut out = Vec::with_capacity(n);
        for _ in 0..n {
            let input = ndarray::Array4::<f32>::from_elem((1, 3, self.size, self.size), 114.0);
            let t = std::time::Instant::now();
            match &self.run {
                Runner::Here(s) => {
                    run_session(&mut s.lock().unwrap_or_else(|e| e.into_inner()), input)?;
                }
                Runner::Card(c) => {
                    c.run(input).map_err(|e| e.to_string())?;
                }
                #[cfg(test)]
                Runner::Fake(f) => {
                    f(input)?;
                }
            }
            out.push(t.elapsed());
        }
        Ok(out)
    }

    pub fn input_size(&self) -> usize {
        self.size
    }

    /// The graphics card it runs on (None: the CPU).
    pub fn gpu_name(&self) -> Option<&str> {
        self.gpu.as_deref()
    }

    /// What it runs the card with ("CUDA", "OpenVINO", "WebGPU", "DirectML", "Core ML"; None: the CPU).
    pub fn gpu_api(&self) -> Option<&'static str> {
        match &self.run {
            Runner::Card(t) => Some(t.api()),
            Runner::Here(_) => None,
            #[cfg(test)]
            Runner::Fake(_) => None,
        }
    }

    /// Its card thread is stuck in a check (or the card faulted).
    pub fn card_stuck(&self) -> bool {
        matches!(&self.run, Runner::Card(t) if t.stuck())
    }

    /// At shutdown: let a card's session go once its check in progress is
    /// done (`ai_card::CardThread::finish`). False when the card is stuck or
    /// faulted and its session is left alone.
    pub fn finish(self, wait: std::time::Duration) -> bool {
        match self.run {
            Runner::Card(t) => t.finish(wait),
            Runner::Here(_) => true,
            #[cfg(test)]
            Runner::Fake(_) => true,
        }
    }

    /// Detect objects in a JPEG (or any image format the image crate reads).
    pub fn detect(&self, image_bytes: &[u8], min_score: f32) -> Result<Vec<Detection>> {
        let img = image::load_from_memory(image_bytes).context("could not decode image")?.to_rgb8();
        self.detect_rgb(&img, min_score)
    }

    /// Detect in the whole picture and in four overlapping quarters of it at
    /// full resolution, merged: finds small, far-away objects that shrink to
    /// a few pixels when the whole picture is scaled to the model's input.
    /// Five times the work: for the graphics card.
    pub fn detect_tiled(&self, img: &image::RgbImage, min_score: f32) -> Result<Vec<Detection>> {
        let mut all = self.detect_rgb(img, min_score)?;
        let (w, h) = (img.width(), img.height());
        // (Only worth it when the picture is well over the model's input size.)
        if w as usize > self.size * 3 / 2 && h as usize > self.size {
            // Tiles of 60% each way: they overlap by 20% so an object on a
            // seam is whole in one of them.
            let (tw, th) = ((w as f32 * 0.6) as u32, (h as f32 * 0.6) as u32);
            for (tx, ty) in [(0, 0), (w - tw, 0), (0, h - th), (w - tw, h - th)] {
                let tile = image::imageops::crop_imm(img, tx, ty, tw, th).to_image();
                for d in self.detect_rgb(&tile, min_score)? {
                    all.push(Detection {
                        x: (tx as f32 + d.x * tw as f32) / w as f32,
                        y: (ty as f32 + d.y * th as f32) / h as f32,
                        w: d.w * tw as f32 / w as f32,
                        h: d.h * th as f32 / h as f32,
                        ..d
                    });
                }
            }
        }
        Ok(merge_same(all))
    }

    /// Detect objects in a decoded picture. On a card, a check that fails
    /// there is an `ai_card::CardFailure` (the caller can tell it).
    pub fn detect_rgb(&self, img: &image::RgbImage, min_score: f32) -> Result<Vec<Detection>> {
        let (iw, ih) = (img.width() as f32, img.height() as f32);
        let s = self.size;
        // Letterbox: scale to fit, top-left aligned, pad with gray 114.
        let r = (s as f32 / iw).min(s as f32 / ih);
        let (nw, nh) = (((iw * r).round() as u32).max(1), ((ih * r).round() as u32).max(1));
        let resized = image::imageops::resize(img, nw, nh, image::imageops::FilterType::Triangle);
        let mut input = ndarray::Array4::<f32>::from_elem((1, 3, s, s), 114.0);
        for (x, y, p) in resized.enumerate_pixels() {
            let (x, y) = (x as usize, y as usize);
            // YOLOX models are trained on BGR input, raw 0-255.
            input[[0, 0, y, x]] = p[2] as f32;
            input[[0, 1, y, x]] = p[1] as f32;
            input[[0, 2, y, x]] = p[0] as f32;
        }
        let (shape, flat) = match &self.run {
            Runner::Here(session) => run_session(&mut session.lock().unwrap(), input).map_err(|e| anyhow!("AI inference: {e}"))?,
            #[cfg(test)]
            Runner::Fake(f) => f(input).map_err(|e| anyhow!("AI inference: {e}"))?,
            Runner::Card(t) => {
                // (Not a card error: the check is just not done.)
                if crate::ai_card::stopping() {
                    return Err(anyhow!("AmberSight is stopping, so no new AI checks start"));
                }
                t.run(input).map_err(anyhow::Error::new)?
            }
        };
        // (At least 6 columns: the decoder reads box, objectness and one class per row. The
        // product is checked: a helper's shape is not trusted to fit in a number.)
        if shape.len() != 3 || shape[2] < 6 || shape[1].checked_mul(shape[2]).is_none_or(|n| flat.len() < n) {
            return Err(anyhow!("unexpected AI model output shape {shape:?}"));
        }
        let dets = decode_yolox(&flat, shape[1], shape[2], s, min_score);
        Ok(nms(dets, NMS_IOU)
            .into_iter()
            .map(|(cls, score, cx, cy, w, h)| {
                // Back to original image pixels, then normalize.
                let (x0, y0) = ((cx - w / 2.0) / r, (cy - h / 2.0) / r);
                let (bw, bh) = (w / r, h / r);
                Detection {
                    label: COCO_LABELS.get(cls).copied().unwrap_or("object").to_string(),
                    score,
                    x: (x0 / iw).clamp(0.0, 1.0),
                    y: (y0 / ih).clamp(0.0, 1.0),
                    w: (bw / iw).clamp(0.0, 1.0),
                    h: (bh / ih).clamp(0.0, 1.0),
                }
            })
            .collect())
    }
}

/// One box per object when several looks (whole picture, tiles) found the
/// same one: boxes of a label overlapping by half or more keep the surest.
#[cfg_attr(all(target_os = "macos", target_arch = "x86_64"), allow(dead_code))]
fn merge_same(mut d: Vec<Detection>) -> Vec<Detection> {
    d.sort_by(|a, b| b.score.total_cmp(&a.score));
    let iou = |a: &Detection, b: &Detection| {
        let ix = (a.x + a.w).min(b.x + b.w) - a.x.max(b.x);
        let iy = (a.y + a.h).min(b.y + b.h) - a.y.max(b.y);
        if ix <= 0.0 || iy <= 0.0 {
            return 0.0;
        }
        let i = ix * iy;
        // Against the smaller box: a tile's box of part of an object counts too.
        i / (a.w * a.h).min(b.w * b.h).max(1e-9)
    };
    let mut out: Vec<Detection> = Vec::new();
    for x in d {
        if !out.iter().any(|k| k.label == x.label && iou(k, &x) >= 0.5) {
            out.push(x);
        }
    }
    out
}

/// Make the session for `card` on a card thread of its own (ai_card.rs):
/// the thread and what it found, or why not. A load that takes too long
/// turns the cards off for the AI until a restart.
#[cfg(not(all(target_os = "macos", target_arch = "x86_64")))]
fn on_card(
    env: &ort::environment::Environment,
    model: &Path,
    card: &crate::gpu::Gpu,
    cards: &[crate::gpu::Gpu],
    want: Option<&str>,
    threads: usize,
    memory_mb: u32,
) -> Result<(crate::ai_card::CardThread, OnCard), String> {
    let (env, model, card, cards, want) = (env.clone(), model.to_path_buf(), card.clone(), cards.to_vec(), want.map(String::from));
    let api = card.api;
    let started = crate::ai_card::CardThread::start(api, crate::ai_card::LOAD_DEADLINE, move || {
        let (mut session, name) = gpu_session(&env, &model, &card, &cards, want.as_deref(), threads, memory_mb)?;
        let size = input_size_of(&session);
        let run = move |input: crate::ai_card::Input| run_session(&mut session, input);
        Ok((run, OnCard { size, name }))
    });
    match started {
        Ok(t) => Ok(t),
        Err(crate::ai_card::LoadError::Failed(why)) => Err(why),
        Err(crate::ai_card::LoadError::Timeout(d)) => {
            // (Often a card still wedged by an earlier process: the next
            // start gets it back once the driver has reset it.)
            let why = crate::ai_card::load_off_text(api, d);
            tracing::warn!("AI: the graphics card did not load the model and run a first check within {} s ({api}): {why}", d.as_secs());
            crate::ai_card::block(&why);
            Err(why)
        }
    }
}

/// A session on the graphics card, or why not (no DirectX 12 GPU, a driver
/// problem, missing libraries: the caller logs it and uses the CPU).
#[cfg(windows)]
fn gpu_session(env: &ort::environment::Environment, model: &Path, card: &crate::gpu::Gpu, _cards: &[crate::gpu::Gpu], _want: Option<&str>, _threads: usize, _memory_mb: u32) -> Result<(Session, String), String> {
    let mut session = (|| -> Result<Session> {
        Session::builder(env)
            .map_err(|e| anyhow!("{e}"))?
            // DirectML needs these two off.
            .with_memory_pattern(false)
            .map_err(|e| anyhow!("{e}"))?
            .with_parallel_execution(false)
            .map_err(|e| anyhow!("{e}"))?
            .with_execution_providers([ort::ep::DirectML::default().with_device_id(card.index as i32).build().error_on_failure()])
            .map_err(|e| anyhow!("{e}"))?
            .commit_from_file(model)
            .map_err(|e| anyhow!("{e}"))
    })()
    .map_err(|e| e.to_string())?;
    try_once(&mut session).map_err(|e| e.to_string())?;
    Ok((session, card.name.clone()))
}

/// 64-bit Linux: the chosen card, NVIDIA (CUDA) or Intel (OpenVINO).
#[cfg(all(target_os = "linux", target_arch = "x86_64", target_env = "gnu"))]
fn gpu_session(env: &ort::environment::Environment, model: &Path, card: &crate::gpu::Gpu, cards: &[crate::gpu::Gpu], want: Option<&str>, threads: usize, memory_mb: u32) -> Result<(Session, String), String> {
    // (A download of the other add-on is left to go on: an NVIDIA card whose
    // add-on is still coming is used once it is in.)
    if card.api == "OpenVINO" {
        return openvino_session(env, model, cards, want, threads);
    }
    cuda_session(env, model, card, threads, memory_mb).map(|s| (s, card.name.clone()))
}

/// An NVIDIA card through CUDA: needs NVIDIA's CUDA 13 and cuDNN 9
/// libraries (checked first: nothing is downloaded without them) and the
/// add-on (downloaded when missing; the AI is reloaded once it is in). One
/// picture is run through it before it is used, so a card that cannot run
/// the model is found now and not on every check.
#[cfg(all(target_os = "linux", target_arch = "x86_64", target_env = "gnu"))]
fn cuda_session(env: &ort::environment::Environment, model: &Path, card: &crate::gpu::Gpu, threads: usize, memory_mb: u32) -> Result<Session, String> {
    use crate::gpu_addon::Addon;
    if let Some(why) = crate::gpu_addon::missing_system_libs() {
        return Err(why);
    }
    let dir = match crate::gpu_addon::find(Addon::Cuda)? {
        Some(d) => d,
        None => {
            crate::gpu_addon::start_download(Addon::Cuda);
            return Err(crate::gpu_addon::note(Addon::Cuda).unwrap_or_else(|| "the NVIDIA add-on for the AI is not installed".into()));
        }
    };
    let device = if card.bus.is_empty() { card.index as i32 } else { crate::gpu_addon::cuda_device(&card.bus)? };
    let cuda = ort::ep::CUDA::default()
        .with_device_id(device)
        .with_memory_limit((memory_mb.max(256) as usize) << 20)
        .with_arena_extend_strategy(ort::ep::ArenaExtendStrategy::SameAsRequested)
        .build()
        .error_on_failure();
    let mut session = crate::gpu_addon::with_runtime_dir(&dir, || -> Result<Session> {
        Session::builder(env)
            .map_err(|e| anyhow!("{e}"))?
            .with_intra_threads(threads)
            .map_err(|e| anyhow!("{e}"))?
            .with_execution_providers([cuda])
            .map_err(|e| anyhow!("{e}"))?
            .commit_from_file(model)
            .map_err(|e| anyhow!("{e}"))
    })?
    .map_err(|e| short_ort_error(&e.to_string()))?;
    try_once(&mut session).map_err(|e| {
        let e = e.to_string();
        if e.contains("Failed to allocate memory") || e.contains("BFCArena") {
            format!("the model needs more video memory than the AI may take (ai.gpu_memory_mb = {memory_mb})")
        } else {
            short_ort_error(&e)
        }
    })?;
    Ok(session)
}

/// Intel graphics through OpenVINO: needs Intel's compute driver (checked
/// first: nothing is downloaded without it) and the Intel add-on
/// (downloaded when missing; the AI is reloaded once it is in). OpenVINO
/// is asked which cards it sees, and the one named `want` (else the best)
/// is used by OpenVINO's own number for it. One picture is run through it
/// before it is used. Default precision; compiled models are kept in the
/// data folder's cache (openvino.rs).
#[cfg(all(target_os = "linux", target_arch = "x86_64", target_env = "gnu"))]
fn openvino_session(env: &ort::environment::Environment, model: &Path, cards: &[crate::gpu::Gpu], want: Option<&str>, threads: usize) -> Result<(Session, String), String> {
    use crate::gpu_addon::Addon;
    let intel: Vec<crate::gpu::Gpu> = cards.iter().filter(|g| g.api == "OpenVINO").cloned().collect();
    let render: Vec<String> = intel.iter().map(|g| g.render.clone()).filter(|r| !r.is_empty()).collect();
    if let Some(why) = crate::openvino::missing_driver(&render) {
        return Err(why);
    }
    let dir = match crate::gpu_addon::find(Addon::OpenVino)? {
        Some(d) => d,
        None => {
            crate::gpu_addon::start_download(Addon::OpenVino);
            return Err(crate::gpu_addon::note(Addon::OpenVino).unwrap_or_else(|| "the Intel graphics add-on for the AI is not installed".into()));
        }
    };
    crate::openvino::preload(&dir)?;
    let found = crate::openvino::cards()?;
    crate::openvino::remember(&found);
    // The chosen card again, now with OpenVINO's names (Settings shows those).
    let named: Vec<crate::gpu::Gpu> = intel
        .iter()
        .map(|g| match found.iter().find(|f| f.bus.eq_ignore_ascii_case(&g.bus)) {
            Some(f) => crate::gpu::Gpu { name: f.name.clone(), memory_mb: f.memory_mb, ..g.clone() },
            None => g.clone(),
        })
        .collect();
    let mut named = named;
    crate::gpu::number_same_names(&mut named);
    let chosen = crate::gpu::pick(&named, want).ok_or("no Intel card")?;
    let ov = found
        .iter()
        .find(|f| f.bus.eq_ignore_ascii_case(&chosen.bus))
        .or_else(|| (found.len() == 1).then(|| &found[0]))
        .ok_or_else(|| format!("OpenVINO does not see the card at {} (it sees {})", chosen.bus, found.iter().map(|f| f.bus.as_str()).collect::<Vec<_>>().join(", ")))?;
    let options = crate::openvino::provider_options(&ov.id);
    let mut session = crate::gpu_addon::with_runtime_dir(&dir, || -> Result<Session, String> {
        let mut b = Session::builder(env).map_err(|e| e.to_string())?.with_intra_threads(threads).map_err(|e| e.to_string())?;
        crate::openvino::append_provider(&mut b, &options)?;
        b.commit_from_file(model).map_err(|e| e.to_string())
    })?
    .map_err(|e| short_ort_error(&e))?;
    try_once(&mut session).map_err(|e| short_ort_error(&e.to_string()))?;
    tracing::info!("AI: OpenVINO runs the model on {} ({})", chosen.name, ov.id);
    Ok((session, chosen.name))
}

/// Apple silicon: Core ML (Neural Engine and GPU). Parts of a model Core ML
/// cannot run stay on the CPU by themselves.
#[cfg(all(target_os = "macos", target_arch = "aarch64"))]
fn gpu_session(env: &ort::environment::Environment, model: &Path, card: &crate::gpu::Gpu, _cards: &[crate::gpu::Gpu], _want: Option<&str>, threads: usize, _memory_mb: u32) -> Result<(Session, String), String> {
    use ort::ep::coreml::{ComputeUnits, ModelFormat};
    let coreml = ort::ep::CoreML::default().with_model_format(ModelFormat::MLProgram).with_compute_units(ComputeUnits::All).build().error_on_failure();
    let mut session = (|| -> Result<Session> {
        Session::builder(env)
            .map_err(|e| anyhow!("{e}"))?
            .with_intra_threads(threads)
            .map_err(|e| anyhow!("{e}"))?
            .with_execution_providers([coreml])
            .map_err(|e| anyhow!("{e}"))?
            .commit_from_file(model)
            .map_err(|e| anyhow!("{e}"))
    })()
    .map_err(|e| short_ort_error(&e.to_string()))?;
    try_once(&mut session).map_err(|e| short_ort_error(&e.to_string()))?;
    Ok((session, card.name.clone()))
}

#[cfg(not(any(windows, all(target_os = "linux", target_arch = "x86_64", target_env = "gnu"), target_os = "macos")))]
fn gpu_session(_env: &ort::environment::Environment, _model: &Path, _card: &crate::gpu::Gpu, _cards: &[crate::gpu::Gpu], _want: Option<&str>, _threads: usize, _memory_mb: u32) -> Result<(Session, String), String> {
    Err("this build has no graphics card support".into())
}

/// Run one blank picture through a new session.
#[cfg(not(all(target_os = "macos", target_arch = "x86_64")))]
#[cfg_attr(not(any(windows, all(target_os = "linux", target_arch = "x86_64", target_env = "gnu"), all(target_os = "macos", target_arch = "aarch64"))), allow(dead_code))]
fn try_once(session: &mut Session) -> Result<()> {
    let s = input_size_of(session);
    let input = ndarray::Array4::<f32>::from_elem((1, 3, s, s), 114.0);
    run_session(session, input).map_err(|e| anyhow!("{e}"))?;
    Ok(())
}

/// ONNX Runtime's errors start with its source file and line; keep the
/// part that says what went wrong ("CUDA failure 100: no CUDA-capable
/// device is detected").
#[cfg_attr(not(any(all(target_os = "linux", target_arch = "x86_64", target_env = "gnu"), all(target_os = "macos", target_arch = "aarch64"))), allow(dead_code))]
fn short_ort_error(e: &str) -> String {
    let e = e.trim();
    if let Some(i) = e.find("CUDA failure ") {
        return e[i..].split(" ;").next().unwrap_or("").trim().to_string();
    }
    let Some(i) = e.rfind("] : ") else { return e.to_string() };
    // "1 : FAIL : what happened"
    let rest = e[i + 4..].trim_start_matches(|c: char| c.is_ascii_digit() || c == ' ' || c == ':');
    match rest.split_once(" : ") {
        Some((code, what)) if code.chars().all(|c| c.is_ascii_uppercase() || c == '_') => what.trim().to_string(),
        _ => rest.trim().to_string(),
    }
}

/// Intel Macs: ONNX Runtime publishes no prebuilt library for them, so
/// this build has no built-in detector. An external AI server still works.
#[cfg(all(target_os = "macos", target_arch = "x86_64"))]
pub struct AiDetector;

#[cfg(all(target_os = "macos", target_arch = "x86_64"))]
impl AiDetector {
    pub fn load(_model: &Path, _threads: usize) -> Result<Self> {
        Err(anyhow!("built-in AI detection is not available on Intel Macs; set an AI server URL instead"))
    }

    pub fn load_on(model: &Path, threads: usize, _gpu: bool, _gpu_name: Option<&str>, _gpu_memory_mb: u32) -> Result<Self> {
        Self::load(model, threads)
    }

    pub fn load_cpu(model: &Path, threads: usize) -> Result<Self> {
        Self::load(model, threads)
    }

    pub fn try_card(_model: &Path, _threads: usize, _gpu_name: Option<&str>, _gpu_memory_mb: u32) -> CardTry {
        CardTry::NotUsed(None)
    }

    pub fn input_size(&self) -> usize {
        0
    }

    pub fn gpu_name(&self) -> Option<&str> {
        None
    }

    pub fn gpu_api(&self) -> Option<&'static str> {
        None
    }

    pub fn card_stuck(&self) -> bool {
        false
    }

    pub fn finish(self, _wait: std::time::Duration) -> bool {
        true
    }

    pub fn detect(&self, _image_bytes: &[u8], _min_score: f32) -> Result<Vec<Detection>> {
        Err(anyhow!("built-in AI detection is not available in this build"))
    }

    pub fn detect_tiled(&self, _img: &image::RgbImage, _min_score: f32) -> Result<Vec<Detection>> {
        Err(anyhow!("built-in AI detection is not available in this build"))
    }

    pub fn detect_rgb(&self, _img: &image::RgbImage, _min_score: f32) -> Result<Vec<Detection>> {
        Err(anyhow!("built-in AI detection is not available in this build"))
    }
}

/// Decode raw YOLOX outputs (rows of [cx, cy, w, h, obj, class...] relative to
/// grid cells at strides 8/16/32) into (class, score, cx, cy, w, h) in input
/// pixels.
#[cfg_attr(all(target_os = "macos", target_arch = "x86_64"), allow(dead_code))]
pub(crate) fn decode_yolox(flat: &[f32], rows: usize, cols: usize, size: usize, min_score: f32) -> Vec<(usize, f32, f32, f32, f32, f32)> {
    let mut grid = Vec::with_capacity(rows);
    for stride in [8usize, 16, 32] {
        let n = size / stride;
        for gy in 0..n {
            for gx in 0..n {
                grid.push((gx as f32, gy as f32, stride as f32));
            }
        }
    }
    let mut out = Vec::new();
    for (i, g) in grid.iter().enumerate().take(rows) {
        let row = &flat[i * cols..(i + 1) * cols];
        let obj = row[4];
        if obj < min_score {
            continue;
        }
        let (cls, p) = row[5..].iter().enumerate().fold((0, 0.0f32), |b, (c, v)| if *v > b.1 { (c, *v) } else { b });
        let score = obj * p;
        if score < min_score {
            continue;
        }
        let (gx, gy, st) = *g;
        out.push((cls, score, (row[0] + gx) * st, (row[1] + gy) * st, row[2].exp() * st, row[3].exp() * st));
    }
    out
}

/// Boxes of one label overlapping more than this are one object (`nms`).
pub const NMS_IOU: f32 = 0.45;

/// Per-class non-maximum suppression.
#[cfg_attr(all(target_os = "macos", target_arch = "x86_64"), allow(dead_code))]
pub(crate) fn nms(mut dets: Vec<(usize, f32, f32, f32, f32, f32)>, iou_max: f32) -> Vec<(usize, f32, f32, f32, f32, f32)> {
    dets.sort_by(|a, b| b.1.total_cmp(&a.1));
    let mut keep: Vec<(usize, f32, f32, f32, f32, f32)> = Vec::new();
    for d in dets {
        if keep.iter().all(|k| k.0 != d.0 || iou(k, &d) <= iou_max) {
            keep.push(d);
        }
    }
    keep
}

#[cfg_attr(all(target_os = "macos", target_arch = "x86_64"), allow(dead_code))]
fn iou(a: &(usize, f32, f32, f32, f32, f32), b: &(usize, f32, f32, f32, f32, f32)) -> f32 {
    let (ax0, ay0, ax1, ay1) = (a.2 - a.4 / 2.0, a.3 - a.5 / 2.0, a.2 + a.4 / 2.0, a.3 + a.5 / 2.0);
    let (bx0, by0, bx1, by1) = (b.2 - b.4 / 2.0, b.3 - b.5 / 2.0, b.2 + b.4 / 2.0, b.3 + b.5 / 2.0);
    let iw = (ax1.min(bx1) - ax0.max(bx0)).max(0.0);
    let ih = (ay1.min(by1) - ay0.max(by0)).max(0.0);
    let inter = iw * ih;
    let union = a.4 * a.5 + b.4 * b.5 - inter;
    if union <= 0.0 { 0.0 } else { inter / union }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tile_and_whole_picture_boxes_of_one_object_merge() {
        let d = |label: &str, score, x, y, w, h| Detection { label: label.into(), score, x, y, w, h };
        let merged = merge_same(vec![
            d("person", 0.6, 0.50, 0.50, 0.04, 0.10),
            d("person", 0.9, 0.505, 0.51, 0.035, 0.09), // the same person, from a tile
            d("car", 0.8, 0.50, 0.50, 0.04, 0.10),       // another label: kept
            d("person", 0.7, 0.10, 0.10, 0.04, 0.10),   // someone else
        ]);
        assert_eq!(merged.len(), 3);
        assert_eq!(merged.iter().filter(|x| x.label == "person").count(), 2);
        assert!(merged.iter().any(|x| x.label == "person" && x.score == 0.9));
    }

    #[test]
    fn a_chosen_nvidia_card_is_not_swapped_for_an_intel_one() {
        let g = |api: &'static str, name: &str| crate::gpu::Gpu { index: 0, name: name.into(), memory_mb: 0, bus: String::new(), api, render: String::new() };
        let rtx = g("CUDA", "NVIDIA GeForce RTX 3060");
        assert!(may_fall_over(&rtx, None), "picked by the AI");
        assert!(may_fall_over(&rtx, Some("")), "Automatic");
        assert!(may_fall_over(&rtx, Some("a card no longer there")));
        assert!(!may_fall_over(&rtx, Some("NVIDIA GeForce RTX 3060")), "the owner's choice");
        // An Intel card the AI picked may give way to an AMD one; AMD is the last kind.
        let arc = g("OpenVINO", "Intel graphics card");
        assert!(may_fall_over(&arc, None) && may_fall_over(&arc, Some("gone")));
        assert!(!may_fall_over(&arc, Some("Intel graphics card")));
        assert!(!may_fall_over(&g("WebGPU", "AMD Radeon 780M Graphics"), None));
    }

    #[cfg(all(target_os = "linux", target_arch = "x86_64", target_env = "gnu"))]
    #[test]
    fn no_other_card_is_tried_while_the_first_cards_add_on_downloads() {
        use crate::gpu_addon::{set_downloading_for_test, Addon};
        let g = |api: &'static str| crate::gpu::Gpu { index: 0, name: api.into(), memory_mb: 0, bus: String::new(), api, render: String::new() };
        let (rtx, arc) = (g("CUDA"), g("OpenVINO"));
        assert!(falls_over(&rtx, None) && falls_over(&arc, None));
        // The NVIDIA add-on downloading: the AI waits for it on the processor.
        set_downloading_for_test(Addon::Cuda, true);
        let (nvidia_waits, intel_still) = (!falls_over(&rtx, None), falls_over(&arc, None));
        set_downloading_for_test(Addon::Cuda, false);
        assert!(nvidia_waits && intel_still);
        // The same for Intel's.
        set_downloading_for_test(Addon::OpenVino, true);
        let (intel_waits, coming) = (!falls_over(&arc, None), add_on_coming(&arc));
        set_downloading_for_test(Addon::OpenVino, false);
        assert!(intel_waits && coming);
        assert!(falls_over(&rtx, None) && falls_over(&arc, None));
    }

    #[test]
    fn a_card_that_cannot_be_used_gives_way_to_intel_then_amd() {
        let g = |api: &'static str, name: &str, mb: u64| crate::gpu::Gpu { index: 0, name: name.into(), memory_mb: mb, bus: String::new(), api, render: String::new() };
        let names = |v: Vec<crate::gpu::Gpu>| v.into_iter().map(|c| c.name).collect::<Vec<_>>();
        let cards = vec![g("CUDA", "RTX", 8192), g("OpenVINO", "Arc", 0), g("WebGPU", "780M", 512), g("WebGPU", "RX 7600", 8192)];
        // NVIDIA: Intel, then the best AMD one; Intel: AMD only; AMD: nothing.
        assert_eq!(names(fall_over_cards(&cards[0], &cards)), ["Arc", "RX 7600"]);
        assert_eq!(names(fall_over_cards(&cards[1], &cards)), ["RX 7600"]);
        assert!(fall_over_cards(&cards[2], &cards).is_empty());
        // NVIDIA and AMD only (an AMD processor's graphics beside an NVIDIA card).
        let two = vec![g("CUDA", "RTX", 8192), g("WebGPU", "780M", 512)];
        assert_eq!(names(fall_over_cards(&two[0], &two)), ["780M"]);
        // Settings' note keeps why the first card was not used.
        assert_eq!(fall_over_note("NVIDIA's CUDA 13 libraries are missing", &[]), "NVIDIA's CUDA 13 libraries are missing");
        assert_eq!(
            fall_over_note("no compute driver", &[("780M".into(), "the AMD graphics chip is slower than the processor here".into())]),
            "no compute driver; 780M was tried instead: the AMD graphics chip is slower than the processor here"
        );
    }

    #[test]
    fn only_a_saved_name_that_is_the_card_counts_as_chosen() {
        let amd = crate::gpu::Gpu { index: 0, name: "AMD Radeon 780M Graphics".into(), memory_mb: 0, bus: String::new(), api: "WebGPU", render: String::new() };
        assert!(named(&amd, Some("AMD Radeon 780M Graphics")));
        // A card no longer there (pick fell back to the best one), Automatic, nothing saved:
        // picked by the AI, so the speed check is done.
        assert!(!named(&amd, Some("NVIDIA GeForce RTX 3060")));
        assert!(!named(&amd, Some("")));
        assert!(!named(&amd, None));
    }

    #[test]
    fn ort_errors_lose_their_source_line() {
        let e = "/home/runner/work/onnxruntime/core/session/provider_bridge_ort.cc:2043 Provider &onnxruntime::ProviderLibrary::Get() [ONNXRuntimeError] : 1 : FAIL : Failed to load library /x/libonnxruntime_providers_cuda.so with error: libcublasLt.so.13: cannot open shared object file";
        assert_eq!(short_ort_error(e), "Failed to load library /x/libonnxruntime_providers_cuda.so with error: libcublasLt.so.13: cannot open shared object file");
        assert_eq!(short_ort_error("plain"), "plain");
        let e = "provider_options_utils.h:189 Parse Failed to parse provider option \"device_id\": CUDA failure 100: no CUDA-capable device is detected ; GPU=-1 ; hostname=x ; file=y";
        assert_eq!(short_ort_error(e), "CUDA failure 100: no CUDA-capable device is detected");
    }

    #[test]
    fn nms_keeps_best_per_class_and_other_classes() {
        let dets = vec![
            (0, 0.9, 100.0, 100.0, 50.0, 100.0),
            (0, 0.8, 105.0, 102.0, 50.0, 100.0), // overlaps the first person
            (2, 0.7, 104.0, 100.0, 50.0, 100.0), // a car in the same place stays
            (0, 0.6, 400.0, 100.0, 50.0, 100.0), // a second person elsewhere
        ];
        let kept = nms(dets, 0.45);
        assert_eq!(kept.len(), 3);
        assert_eq!(kept[0].1, 0.9);
    }

    #[test]
    fn decode_uses_grid_and_stride() {
        // 64x64 input: 8x8 + 4x4 + 2x2 = 84 grid cells, 6 columns (1 class).
        let (rows, cols) = (84, 6);
        let mut flat = vec![0.0f32; rows * cols];
        // Cell index 9 = stride 8, gx=1, gy=1: offsets 0.5 -> center 12.
        flat[9 * cols..9 * cols + 6].copy_from_slice(&[0.5, 0.5, 0.0, 0.0, 0.9, 0.9]);
        let d = decode_yolox(&flat, rows, cols, 64, 0.5);
        assert_eq!(d.len(), 1);
        let (cls, score, cx, cy, w, h) = d[0];
        assert_eq!(cls, 0);
        assert!((score - 0.81).abs() < 1e-5);
        assert_eq!((cx, cy, w, h), (12.0, 12.0, 8.0, 8.0));
    }

    /// Needs a model file that is not in the repo: set AMBER_TEST_MODEL_DIR to a
    /// folder holding `yolox_l.onnx` (skipped when it is not there).
    #[cfg(not(all(target_os = "macos", target_arch = "x86_64")))]
    #[test]
    fn large_model_loads_and_detects_when_present() {
        let Some(dir) = std::env::var_os("AMBER_TEST_MODEL_DIR") else { return };
        let path = std::path::Path::new(&dir).join("yolox_l.onnx");
        if !path.is_file() {
            return;
        }
        let det = AiDetector::load_on(&path, 4, false, None, 1536).unwrap();
        assert_eq!(det.input_size(), 640);
        let mut img = image::RgbImage::from_pixel(1280, 720, image::Rgb([90, 120, 90]));
        for y in 200..620 {
            for x in 500..700 {
                img.put_pixel(x, y, image::Rgb([200, 60, 60]));
            }
        }
        let t = std::time::Instant::now();
        let found = det.detect_rgb(&img, 0.3).unwrap();
        eprintln!("yolox_l: {} objects in {:?}", found.len(), t.elapsed());
        for d in &found {
            assert!((0.0..=1.0).contains(&d.x) && (0.0..=1.0).contains(&d.w) && d.score >= 0.3);
        }
    }
}
