//! Built-in AI object detection (ONNX Runtime + a YOLOX model).
//!
//! YOLOX (Apache-2.0) models from the official release work as-is, e.g.
//! `yolox_s.onnx` (640x640, more accurate), `yolox_l.onnx` (640x640, most
//! accurate, about five times the work) or `yolox_tiny.onnx` (416x416, faster). The model input size is read from the file. Runs on a
//! graphics card where this build can use one (Windows: DirectML, any
//! DirectX 12 card; 64-bit Linux PCs: an NVIDIA card with CUDA 13 and cuDNN 9,
//! see gpu_addon.rs; Apple silicon Macs: Core ML), else on the CPU; detection
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
    session: Mutex<Session>,
    size: usize,
    /// The graphics card it runs on (None: the CPU).
    gpu: Option<String>,
    // Keeps the runtime environment alive for the session.
    _env: ort::environment::Environment,
}

/// Why the last model load is not on the graphics card although one was
/// wanted and found (None: it is, or none was wanted or found), for Settings.
#[cfg(not(all(target_os = "macos", target_arch = "x86_64")))]
static GPU_NOTE: Mutex<Option<String>> = Mutex::new(None);

/// Why the AI is not on the graphics card (see `GPU_NOTE`); on Linux, a
/// download of the NVIDIA add-on in progress or failed comes first.
#[cfg(not(all(target_os = "macos", target_arch = "x86_64")))]
pub fn gpu_note() -> Option<String> {
    let note = GPU_NOTE.lock().unwrap().clone();
    // (gpu_addon::note is None once a usable copy is there, so a stale
    // download failure never hides the real reason.)
    #[cfg(all(target_os = "linux", target_arch = "x86_64", target_env = "gnu"))]
    if note.is_some() {
        if let Some(n) = crate::gpu_addon::note() {
            return Some(n);
        }
    }
    note
}

#[cfg(all(target_os = "macos", target_arch = "x86_64"))]
pub fn gpu_note() -> Option<String> {
    None
}

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

#[cfg(not(all(target_os = "macos", target_arch = "x86_64")))]
impl AiDetector {
    /// On the graphics card where possible (see `load_on`).
    pub fn load(model: &Path, threads: usize) -> Result<Self> {
        Self::load_on(model, threads, true, None, 1536)
    }

    /// `gpu`: run on a graphics card where this build can (see the top of
    /// this file): the one named `gpu_name`, else the one with the most
    /// video memory; the CPU if none can be used. `gpu_memory_mb` caps the
    /// memory pool on an NVIDIA card.
    pub fn load_on(model: &Path, threads: usize, gpu: bool, gpu_name: Option<&str>, gpu_memory_mb: u32) -> Result<Self> {
        let env = runtime()?;
        let cpu = || -> Result<Session> {
            Session::builder(&env)
                .map_err(|e| anyhow!("AI runtime: {e}"))?
                .with_intra_threads(threads.max(1))
                .map_err(|e| anyhow!("AI runtime: {e}"))?
                .commit_from_file(model)
                .map_err(|e| anyhow!("could not load AI model {}: {e}", model.display()))
        };
        let card = if gpu { crate::gpu::pick(&crate::gpu::list(), gpu_name) } else { None };
        // No card wanted (turned off, or none there): a download waiting to retry stops.
        #[cfg(all(target_os = "linux", target_arch = "x86_64", target_env = "gnu"))]
        if card.is_none() {
            crate::gpu_addon::not_wanted();
        }
        let tried = card.as_ref().map(|c| (c, gpu_session(&env, model, c, threads.max(1), gpu_memory_mb)));
        let (session, gpu, note) = match tried {
            Some((c, Ok(s))) => (s, Some(c.name.clone()), None),
            Some((c, Err(why))) => {
                tracing::warn!("AI: the graphics card {} cannot be used ({why}); using the CPU", c.name);
                (cpu()?, None, Some(why))
            }
            None => (cpu()?, None, None),
        };
        *GPU_NOTE.lock().unwrap() = note;
        // Input is [1, 3, S, S]; fall back to 640 when the size is dynamic.
        let size = match session.inputs().first().map(|i| i.dtype()) {
            Some(ort::value::ValueType::Tensor { shape, .. }) => shape.get(2).copied().filter(|d| *d > 0).unwrap_or(640) as usize,
            _ => 640,
        };
        Ok(AiDetector { session: Mutex::new(session), size, gpu, _env: env })
    }

    pub fn input_size(&self) -> usize {
        self.size
    }

    /// The graphics card it runs on (None: the CPU).
    pub fn gpu_name(&self) -> Option<&str> {
        self.gpu.as_deref()
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

    /// Detect objects in a decoded picture.
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
        let tensor = ort::value::Tensor::from_array(input).map_err(|e| anyhow!("AI input: {e}"))?;
        let mut session = self.session.lock().unwrap();
        let outputs = session.run(ort::inputs![tensor]).map_err(|e| anyhow!("AI inference: {e}"))?;
        let out = outputs[0].try_extract_array::<f32>().map_err(|e| anyhow!("AI output: {e}"))?;
        let shape = out.shape().to_vec();
        if shape.len() != 3 || shape[2] < 6 {
            return Err(anyhow!("unexpected AI model output shape {shape:?}"));
        }
        let flat: Vec<f32> = out.iter().copied().collect();
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

/// A session on the graphics card, or why not (no DirectX 12 GPU, a driver
/// problem, missing libraries: the caller logs it and uses the CPU).
#[cfg(windows)]
fn gpu_session(env: &ort::environment::Environment, model: &Path, card: &crate::gpu::Gpu, _threads: usize, _memory_mb: u32) -> Result<Session, String> {
    (|| -> Result<Session> {
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
    .map_err(|e| e.to_string())
}

/// An NVIDIA card through CUDA: needs NVIDIA's CUDA 13 and cuDNN 9
/// libraries (checked first: nothing is downloaded without them) and the
/// add-on (downloaded when missing; the AI is reloaded once it is in). One
/// picture is run through it before it is used, so a card that cannot run
/// the model is found now and not on every check.
#[cfg(all(target_os = "linux", target_arch = "x86_64", target_env = "gnu"))]
fn gpu_session(env: &ort::environment::Environment, model: &Path, card: &crate::gpu::Gpu, threads: usize, memory_mb: u32) -> Result<Session, String> {
    if let Some(why) = crate::gpu_addon::missing_system_libs() {
        return Err(why);
    }
    let dir = match crate::gpu_addon::find()? {
        Some(d) => d,
        None => {
            crate::gpu_addon::start_download();
            return Err(crate::gpu_addon::note().unwrap_or_else(|| "the NVIDIA add-on for the AI is not installed".into()));
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

/// Apple silicon: Core ML (Neural Engine and GPU). Parts of a model Core ML
/// cannot run stay on the CPU by themselves.
#[cfg(all(target_os = "macos", target_arch = "aarch64"))]
fn gpu_session(env: &ort::environment::Environment, model: &Path, _card: &crate::gpu::Gpu, threads: usize, _memory_mb: u32) -> Result<Session, String> {
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
    Ok(session)
}

#[cfg(not(any(windows, all(target_os = "linux", target_arch = "x86_64", target_env = "gnu"), target_os = "macos")))]
fn gpu_session(_env: &ort::environment::Environment, _model: &Path, _card: &crate::gpu::Gpu, _threads: usize, _memory_mb: u32) -> Result<Session, String> {
    Err("this build has no graphics card support".into())
}

/// Run one blank picture through a new session.
#[cfg(any(all(target_os = "linux", target_arch = "x86_64", target_env = "gnu"), all(target_os = "macos", target_arch = "aarch64")))]
fn try_once(session: &mut Session) -> Result<()> {
    let s = match session.inputs().first().map(|i| i.dtype()) {
        Some(ort::value::ValueType::Tensor { shape, .. }) => shape.get(2).copied().filter(|d| *d > 0).unwrap_or(640) as usize,
        _ => 640,
    };
    let input = ndarray::Array4::<f32>::from_elem((1, 3, s, s), 114.0);
    let tensor = ort::value::Tensor::from_array(input).map_err(|e| anyhow!("{e}"))?;
    session.run(ort::inputs![tensor]).map_err(|e| anyhow!("{e}"))?;
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

    pub fn input_size(&self) -> usize {
        0
    }

    pub fn gpu_name(&self) -> Option<&str> {
        None
    }

    pub fn detect(&self, _image_bytes: &[u8], _min_score: f32) -> Result<Vec<Detection>> {
        Err(anyhow!("built-in AI detection is not available in this build"))
    }

    pub fn detect_tiled(&self, _img: &image::RgbImage, _min_score: f32) -> Result<Vec<Detection>> {
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
