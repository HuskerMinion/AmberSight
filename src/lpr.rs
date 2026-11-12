//! License plate recognition: find plates (a small YOLOX detector) and read
//! them (a small OCR network), both on the CPU through the AI runtime.
//!
//! Models:
//! - plate detector: `models/plate_yolox.onnx`, AmberSight's own, built into
//!   the program (no download). A YOLOX network (Megvii YOLOX, Apache-2.0,
//!   started from the official COCO weights) trained on the Open Images V7
//!   "Vehicle registration plate" boxes (annotations CC BY 4.0, images
//!   CC BY 2.0). Sources, license text and attribution:
//!   docs/models/plate-detector.md.
//! - plate reader: `global_mobile_vit_v2_ocr.onnx` from ankandrew/fast-plate-ocr
//!   (MIT; 5 MB; reads plates of many countries, up to 9 characters), pinned
//!   by checksum and downloaded when plates are first turned on.
//!
//! This file also holds the text rules: normalizing what was read, and the
//! fuzzy comparison with known plates (one wrong character, and letters
//! and digits that look alike).

// Intel Macs have no built-in AI runtime, so most of this goes unused there.
#![cfg_attr(all(target_os = "macos", target_arch = "x86_64"), allow(dead_code))]

use anyhow::{anyhow, bail, Context, Result};
use image::RgbImage;
use std::path::{Path, PathBuf};

pub struct LprModel {
    pub file: &'static str,
    pub url: &'static str,
    pub sha256: &'static str,
    pub bytes: u64,
}

/// The plate detector, built into the program.
#[cfg_attr(all(target_os = "macos", target_arch = "x86_64"), allow(dead_code))]
static DETECTOR_ONNX: &[u8] = include_bytes!("../models/plate_yolox.onnx");

pub const READER: LprModel = LprModel {
    file: "plate_ocr_global_mobile_vit_v2.onnx",
    url: "https://github.com/ankandrew/fast-plate-ocr/releases/download/arg-plates/global_mobile_vit_v2_ocr.onnx",
    sha256: "e2fe5946c02e73901f73d21b3b7223a0c3e3b02059d4964fbf279b01af38d63c",
    bytes: 4_991_602,
};

/// The reader's characters, one per output class, and its fixed shape.
const ALPHABET: &[u8] = b"0123456789ABCDEFGHIJKLMNOPQRSTUVWXYZ_";
const PAD: u8 = b'_';
const SLOTS: usize = 9;
const OCR_W: u32 = 140;
const OCR_H: u32 = 70;

/// A plate found in a picture, in that picture's pixels.
#[derive(Clone, Debug, PartialEq)]
pub struct PlateBox {
    pub score: f32,
    pub x: f32,
    pub y: f32,
    pub w: f32,
    pub h: f32,
}

/// What the reader made of one plate picture.
#[derive(Clone, Debug, PartialEq)]
pub struct Read {
    /// Normalized (see [`normalize`]).
    pub text: String,
    /// Mean sureness (0-1) over the characters.
    pub confidence: f32,
}

pub fn model_path(dir: &Path, m: &LprModel) -> PathBuf {
    dir.join(m.file)
}

pub fn models_present(dir: &Path) -> bool {
    model_path(dir, &READER).metadata().is_ok_and(|x| x.len() == READER.bytes)
}

pub fn total_bytes() -> u64 {
    READER.bytes
}

/// Download the reader model into `dir` (checked against their checksums).
/// `progress` gets (bytes so far, total).
pub fn download_models(dir: &Path, progress: &dyn Fn(u64, u64)) -> Result<()> {
    use sha2::{Digest, Sha256};
    use std::io::{Read, Write};
    std::fs::create_dir_all(dir)?;
    let total = total_bytes();
    let mut done = 0u64;
    for m in [&READER] {
        let dest = model_path(dir, m);
        if dest.metadata().is_ok_and(|x| x.len() == m.bytes) {
            done += m.bytes;
            progress(done, total);
            continue;
        }
        let agent = ureq::Agent::config_builder().timeout_connect(Some(std::time::Duration::from_secs(20))).timeout_recv_response(Some(std::time::Duration::from_secs(60))).timeout_recv_body(Some(std::time::Duration::from_secs(1800))).build().new_agent();
        let mut resp = agent.get(m.url).call().map_err(|e| anyhow!("download {}: {e}", m.file))?;
        let mut rd = resp.body_mut().with_config().limit(50 << 20).reader();
        let tmp = dest.with_extension("part");
        let mut f = std::fs::File::create(&tmp)?;
        let mut h = Sha256::new();
        let mut buf = vec![0u8; 1 << 16];
        loop {
            let n = rd.read(&mut buf)?;
            if n == 0 {
                break;
            }
            f.write_all(&buf[..n])?;
            h.update(&buf[..n]);
            done += n as u64;
            progress(done.min(total), total);
        }
        drop(f);
        let got: String = h.finalize().iter().map(|b| format!("{b:02x}")).collect();
        if got != m.sha256 {
            let _ = std::fs::remove_file(&tmp);
            bail!("{} did not match its checksum", m.file);
        }
        std::fs::rename(&tmp, &dest)?;
    }
    Ok(())
}

fn file_sha256(p: &Path) -> Result<String> {
    use sha2::{Digest, Sha256};
    use std::io::Read;
    let mut f = std::fs::File::open(p).with_context(|| format!("open {}", p.display()))?;
    let mut h = Sha256::new();
    let mut buf = vec![0u8; 1 << 16];
    loop {
        let n = f.read(&mut buf)?;
        if n == 0 {
            break;
        }
        h.update(&buf[..n]);
    }
    Ok(h.finalize().iter().map(|b| format!("{b:02x}")).collect())
}

// ---- text rules ----

/// A plate as compared and stored: upper case letters and digits only (no
/// spaces, dashes or dots). "abc 123" and "ABC-123" are the same plate.
pub fn normalize(s: &str) -> String {
    s.chars().filter(|c| c.is_alphanumeric()).flat_map(|c| c.to_uppercase()).collect()
}

/// Letters and digits that look alike to a reader (either way round).
const CONFUSED: [(char, char); 8] = [('O', '0'), ('I', '1'), ('B', '8'), ('S', '5'), ('Z', '2'), ('G', '6'), ('D', '0'), ('Q', '0')];

fn alike(a: char, b: char) -> bool {
    a == b || CONFUSED.iter().any(|&(x, y)| (a == x && b == y) || (a == y && b == x))
}

/// How many characters differ between two normalized plates: the edit
/// distance (a wrong, missing or extra character is 1). With `confusions`,
/// look-alike pairs (O/0, I/1, B/8 ...) count as the same character.
pub fn distance(a: &str, b: &str, confusions: bool) -> usize {
    let (a, b): (Vec<char>, Vec<char>) = (a.chars().collect(), b.chars().collect());
    let same = |x: char, y: char| if confusions { alike(x, y) } else { x == y };
    let mut prev: Vec<usize> = (0..=b.len()).collect();
    for i in 1..=a.len() {
        let mut cur = vec![i; b.len() + 1];
        for j in 1..=b.len() {
            let sub = prev[j - 1] + usize::from(!same(a[i - 1], b[j - 1]));
            cur[j] = sub.min(prev[j] + 1).min(cur[j - 1] + 1);
        }
        prev = cur;
    }
    prev[b.len()]
}

/// Plates shorter than this must match exactly (look-alike characters
/// aside): one wrong character in a short plate is another plate.
pub const FUZZY_MIN_LEN: usize = 5;

/// Whether a reading `read` counts as the known plate `known`: at most
/// `max_diff` characters apart (look-alike pairs free when `confusions`).
/// Returns how many differ.
pub fn matches(read: &str, known: &str, max_diff: usize, confusions: bool) -> Option<usize> {
    if read.is_empty() || known.is_empty() {
        return None;
    }
    let allowed = if read.chars().count().min(known.chars().count()) < FUZZY_MIN_LEN { 0 } else { max_diff };
    let d = distance(read, known, confusions);
    (d <= allowed).then_some(d)
}

/// The best known plate for a reading: the fewest differences, an exact
/// text first. `known` is (id, normalized text).
pub fn best_known(read: &str, known: &[(i64, String)], max_diff: usize, confusions: bool) -> Option<i64> {
    known
        .iter()
        .filter_map(|(id, t)| matches(read, t, max_diff, confusions).map(|d| (d + usize::from(read != t), *id)))
        .min()
        .map(|(_, id)| id)
}

/// One reading of a plate with where it came from (a picture, a time).
#[derive(Clone, Debug)]
pub struct Candidate<T> {
    pub text: String,
    pub confidence: f32,
    pub extra: T,
}

/// Many readings of the plates in one event, boiled down: readings of the
/// same plate (a character apart, look-alikes free) are one; the text
/// with the most weight (sum of confidences) wins and its sharpest
/// reading is kept. Best first, at most `max` plates.
pub fn best_readings<T: Clone>(all: &[Candidate<T>], max: usize) -> Vec<Candidate<T>> {
    let mut groups: Vec<Vec<&Candidate<T>>> = Vec::new();
    let mut sorted: Vec<&Candidate<T>> = all.iter().collect();
    sorted.sort_by(|a, b| b.confidence.total_cmp(&a.confidence));
    for c in sorted {
        let home = groups.iter().position(|g| {
            let lead = &g[0].text;
            lead.chars().count().min(c.text.chars().count()) >= 4 && distance(lead, &c.text, true) <= 1
        });
        match home {
            Some(i) => groups[i].push(c),
            None => groups.push(vec![c]),
        }
    }
    let mut out: Vec<(f32, Candidate<T>)> = groups
        .into_iter()
        .map(|g| {
            // Weight per distinct text.
            let mut weights: Vec<(&str, f32)> = Vec::new();
            for c in &g {
                match weights.iter_mut().find(|w| w.0 == c.text) {
                    Some(w) => w.1 += c.confidence,
                    None => weights.push((&c.text, c.confidence)),
                }
            }
            let winner = weights.iter().max_by(|a, b| a.1.total_cmp(&b.1)).map(|w| w.0).unwrap_or("");
            let best = g.iter().filter(|c| c.text == winner).max_by(|a, b| a.confidence.total_cmp(&b.confidence)).copied().unwrap_or(g[0]);
            (g.iter().map(|c| c.confidence).sum::<f32>(), best.clone())
        })
        .collect();
    out.sort_by(|a, b| b.1.confidence.total_cmp(&a.1.confidence).then(b.0.total_cmp(&a.0)));
    out.into_iter().take(max).map(|x| x.1).collect()
}

/// How a reading is shown in event labels and alerts: "plate ABC123 (Mom's
/// car)" for a known plate, "unknown plate ABC123" for any other.
pub fn label(text: &str, name: Option<&str>) -> String {
    match name {
        Some(n) if !n.is_empty() => format!("plate {text} ({n})"),
        _ => format!("unknown plate {text}"),
    }
}

// ---- reading the models' answers ----

/// The detector's raw YOLOX output (`rows` x `cols`: cx, cy, w, h, object
/// sureness, plate sureness, in grid cells at strides 8/16/32) as plates in
/// the original picture. The picture was scaled by `ratio` and put in the
/// top-left corner of the `size` x `size` input, so there is no padding to
/// remove. Overlapping boxes of one plate are merged (NMS).
pub fn decode_plates(flat: &[f32], rows: usize, cols: usize, size: usize, ratio: f32, min_score: f32) -> Vec<PlateBox> {
    if cols < 6 || ratio <= 0.0 || flat.len() < rows * cols || flat.iter().any(|v| !v.is_finite()) {
        return Vec::new();
    }
    let dets = crate::ai::decode_yolox(flat, rows, cols, size, min_score);
    crate::ai::nms(dets, 0.45)
        .into_iter()
        .filter(|d| d.4 > 0.0 && d.5 > 0.0)
        .map(|(_, score, cx, cy, w, h)| PlateBox { score, x: (cx - w / 2.0) / ratio, y: (cy - h / 2.0) / ratio, w: w / ratio, h: h / ratio })
        .collect()
}

/// The reader's output (9 slots of one probability per character) as a
/// plate text and its confidence. None when nothing was read.
pub fn decode_read(out: &[f32]) -> Option<Read> {
    if out.len() < SLOTS * ALPHABET.len() {
        return None;
    }
    let out = &out[..SLOTS * ALPHABET.len()];
    // The model ends in a softmax; if it did not, make probabilities.
    let plain = out.iter().all(|v| (0.0..=1.0001).contains(v));
    let (mut text, mut sum, mut n) = (String::new(), 0.0f32, 0usize);
    for slot in out.chunks_exact(ALPHABET.len()) {
        let probs: Vec<f32> = if plain {
            slot.to_vec()
        } else {
            let m = slot.iter().cloned().fold(f32::MIN, f32::max);
            let e: Vec<f32> = slot.iter().map(|v| (v - m).exp()).collect();
            let s: f32 = e.iter().sum();
            e.iter().map(|v| v / s).collect()
        };
        let (i, p) = probs.iter().enumerate().max_by(|a, b| a.1.total_cmp(b.1))?;
        if ALPHABET[i] != PAD {
            text.push(ALPHABET[i] as char);
            sum += p;
            n += 1;
        }
    }
    (n > 0).then(|| Read { text, confidence: sum / n as f32 })
}

/// A small picture of a plate with a margin, at most `max_w` wide, as JPEG.
pub fn plate_jpeg(img: &RgbImage, b: &PlateBox, max_w: u32) -> Result<Vec<u8>> {
    let (mx, my) = (b.w * 0.15, b.h * 0.25);
    let x0 = (b.x - mx).max(0.0) as u32;
    let y0 = (b.y - my).max(0.0) as u32;
    let x1 = ((b.x + b.w + mx) as u32).min(img.width());
    let y1 = ((b.y + b.h + my) as u32).min(img.height());
    if x1 <= x0 || y1 <= y0 {
        bail!("plate outside the picture");
    }
    let crop = image::imageops::crop_imm(img, x0, y0, x1 - x0, y1 - y0).to_image();
    let crop = if crop.width() > max_w {
        let h = (crop.height() as f32 * max_w as f32 / crop.width() as f32).round().max(1.0) as u32;
        image::imageops::resize(&crop, max_w, h, image::imageops::FilterType::Triangle)
    } else {
        crop
    };
    let mut out = Vec::new();
    image::codecs::jpeg::JpegEncoder::new_with_quality(&mut out, 88).encode_image(&crop).context("plate picture")?;
    Ok(out)
}

#[cfg(not(all(target_os = "macos", target_arch = "x86_64")))]
pub struct LprModels {
    det: std::sync::Mutex<ort::session::Session>,
    read: std::sync::Mutex<ort::session::Session>,
    size: usize,
    _env: ort::environment::Environment,
}

#[cfg(not(all(target_os = "macos", target_arch = "x86_64")))]
impl LprModels {
    pub fn load(dir: &Path, threads: usize) -> Result<Self> {
        // Only the files that were downloaded and checked (not just any file
        // of the right size).
        if file_sha256(&model_path(dir, &READER))? != READER.sha256 {
            // Removed, so the download button fetches it again.
            let _ = std::fs::remove_file(model_path(dir, &READER));
            bail!("{} does not match its checksum; download the plate models again", READER.file);
        }
        let env = crate::ai::runtime()?;
        let open = |m: &LprModel| -> Result<ort::session::Session> {
            ort::session::Session::builder(&env)
                .map_err(|e| anyhow!("AI runtime: {e}"))?
                .with_intra_threads(threads.max(1))
                .map_err(|e| anyhow!("AI runtime: {e}"))?
                .commit_from_file(model_path(dir, m))
                .map_err(|e| anyhow!("could not load {}: {e}", m.file))
        };
        let det = ort::session::Session::builder(&env)
            .map_err(|e| anyhow!("AI runtime: {e}"))?
            .with_intra_threads(threads.max(1))
            .map_err(|e| anyhow!("AI runtime: {e}"))?
            .commit_from_memory(DETECTOR_ONNX)
            .map_err(|e| anyhow!("could not load the plate detector: {e}"))?;
        let size = match det.inputs().first().map(|i| i.dtype()) {
            Some(ort::value::ValueType::Tensor { shape, .. }) => shape.get(2).copied().filter(|d| *d > 0).unwrap_or(416) as usize,
            _ => 416,
        };
        Ok(LprModels { det: std::sync::Mutex::new(det), read: std::sync::Mutex::new(open(&READER)?), size, _env: env })
    }

    /// Plates in `img` (its pixels), at least `min_score` sure, best first.
    pub fn detect(&self, img: &RgbImage, min_score: f32) -> Result<Vec<PlateBox>> {
        let s = self.size;
        let (iw, ih) = (img.width() as f32, img.height() as f32);
        let r = (s as f32 / iw).min(s as f32 / ih);
        let (nw, nh) = (((iw * r).round() as u32).clamp(1, s as u32), ((ih * r).round() as u32).clamp(1, s as u32));
        let resized = image::imageops::resize(img, nw, nh, image::imageops::FilterType::Triangle);
        // Top-left, gray (114) around, BGR 0-255: as the detector was trained.
        let mut input = ndarray::Array4::<f32>::from_elem((1, 3, s, s), 114.0);
        for (x, y, p) in resized.enumerate_pixels() {
            let (x, y) = (x as usize, y as usize);
            if x < s && y < s {
                input[[0, 0, y, x]] = p[2] as f32;
                input[[0, 1, y, x]] = p[1] as f32;
                input[[0, 2, y, x]] = p[0] as f32;
            }
        }
        let tensor = ort::value::Tensor::from_array(input).map_err(|e| anyhow!("plate input: {e}"))?;
        let mut sess = self.det.lock().unwrap();
        let out = sess.run(ort::inputs![tensor]).map_err(|e| anyhow!("plate detection: {e}"))?;
        let arr = out[0].try_extract_array::<f32>().map_err(|e| anyhow!("plate output: {e}"))?;
        let shape = arr.shape().to_vec();
        if shape.len() != 3 || shape[2] < 6 {
            bail!("unexpected plate detector output shape {shape:?}");
        }
        let flat: Vec<f32> = arr.iter().copied().collect();
        let mut found = decode_plates(&flat, shape[1], shape[2], s, r, min_score);
        found.sort_by(|a, b| b.score.total_cmp(&a.score));
        Ok(found)
    }

    /// Read the plate in `b` (a box from [`detect`]) of `img`.
    pub fn read(&self, img: &RgbImage, b: &PlateBox) -> Result<Option<Read>> {
        let (mx, my) = (b.w * 0.04, b.h * 0.08);
        let x0 = (b.x - mx).max(0.0) as u32;
        let y0 = (b.y - my).max(0.0) as u32;
        let x1 = ((b.x + b.w + mx) as u32).min(img.width());
        let y1 = ((b.y + b.h + my) as u32).min(img.height());
        if x1 <= x0 + 2 || y1 <= y0 + 2 {
            return Ok(None);
        }
        let crop = image::imageops::crop_imm(img, x0, y0, x1 - x0, y1 - y0).to_image();
        let gray = image::imageops::grayscale(&crop);
        let small = image::imageops::resize(&gray, OCR_W, OCR_H, image::imageops::FilterType::Triangle);
        let mut input = ndarray::Array4::<u8>::zeros((1, OCR_H as usize, OCR_W as usize, 1));
        for (x, y, p) in small.enumerate_pixels() {
            input[[0, y as usize, x as usize, 0]] = p[0];
        }
        let tensor = ort::value::Tensor::from_array(input).map_err(|e| anyhow!("plate reader input: {e}"))?;
        let mut sess = self.read.lock().unwrap();
        let out = sess.run(ort::inputs![tensor]).map_err(|e| anyhow!("plate reading: {e}"))?;
        let flat: Vec<f32> = out[0].try_extract_array::<f32>().map_err(|e| anyhow!("plate reader output: {e}"))?.iter().copied().collect();
        Ok(decode_read(&flat).map(|mut r| {
            r.text = normalize(&r.text);
            r
        }))
    }
}

/// Intel Macs have no built-in AI runtime.
#[cfg(all(target_os = "macos", target_arch = "x86_64"))]
pub struct LprModels;

#[cfg(all(target_os = "macos", target_arch = "x86_64"))]
impl LprModels {
    pub fn load(_dir: &Path, _threads: usize) -> Result<Self> {
        bail!("plate recognition needs the built-in AI runtime, which this Mac does not have")
    }
    pub fn detect(&self, _img: &RgbImage, _min_score: f32) -> Result<Vec<PlateBox>> {
        bail!("not available")
    }
    pub fn read(&self, _img: &RgbImage, _b: &PlateBox) -> Result<Option<Read>> {
        bail!("not available")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn text_is_normalized() {
        assert_eq!(normalize(" abc-123 "), "ABC123");
        assert_eq!(normalize("ab.c 1_2/3"), "ABC123");
        assert_eq!(normalize("---"), "");
    }

    #[test]
    fn distance_counts_wrong_missing_and_extra_characters() {
        assert_eq!(distance("ABC123", "ABC123", false), 0);
        assert_eq!(distance("ABC123", "ABC124", false), 1);
        assert_eq!(distance("ABC123", "ABC12", false), 1);
        assert_eq!(distance("ABC123", "ABXC123", false), 1);
        assert_eq!(distance("ABC123", "XYZ789", false), 6);
        // Look-alikes are free only when asked for.
        assert_eq!(distance("B0B123", "808123", false), 2);
        assert_eq!(distance("B0B123", "808123", true), 0);
        assert_eq!(distance("5ILLY1", "SILLYI", true), 0);
    }

    #[test]
    fn fuzzy_match_rules() {
        // Exact, one off, two off.
        assert_eq!(matches("ABC123", "ABC123", 1, true), Some(0));
        assert_eq!(matches("ABC123", "ABC124", 1, true), Some(1));
        assert_eq!(matches("ABC123", "ABD124", 1, true), None);
        // Zero allowed: exact (or look-alikes) only.
        assert_eq!(matches("ABC123", "ABC124", 0, true), None);
        assert_eq!(matches("ABC1O3", "ABC103", 0, true), Some(0));
        assert_eq!(matches("ABC1O3", "ABC103", 0, false), None);
        // Short plates never match loosely.
        assert_eq!(matches("AB12", "AB13", 1, true), None);
        assert_eq!(matches("AB12", "AB12", 1, true), Some(0));
        // Nothing matches nothing.
        assert_eq!(matches("", "", 1, true), None);
    }

    #[test]
    fn the_nearest_known_plate_wins() {
        let known = vec![(1, "ABC124".to_string()), (2, "ABC123".to_string()), (3, "ZZZ999".to_string())];
        assert_eq!(best_known("ABC123", &known, 1, true), Some(2), "exact beats one off");
        assert_eq!(best_known("ABC125", &known, 1, true), Some(1), "first of equals");
        assert_eq!(best_known("QQQ555", &known, 1, true), None);
        assert_eq!(best_known("ABC123", &[], 1, true), None);
    }

    #[test]
    fn readings_of_one_plate_become_one_with_the_sharpest() {
        let c = |t: &str, p: f32, n: u32| Candidate { text: t.to_string(), confidence: p, extra: n };
        let all = vec![c("ABC123", 0.90, 1), c("ABC128", 0.95, 2), c("ABC123", 0.80, 3), c("XYZ789", 0.70, 4)];
        let best = best_readings(&all, 4);
        assert_eq!(best.len(), 2);
        // Two readings agree on ABC123 (weight 1.7) against one ABC128 (0.95).
        assert_eq!((best[0].text.as_str(), best[0].extra), ("ABC123", 1));
        assert_eq!(best[1].text, "XYZ789");
        assert_eq!(best_readings(&all, 1).len(), 1);
        assert!(best_readings::<u32>(&[], 3).is_empty());
    }

    #[test]
    fn labels_show_the_name_when_known() {
        assert_eq!(label("ABC123", None), "unknown plate ABC123");
        assert_eq!(label("ABC123", Some("Mom's car")), "plate ABC123 (Mom's car)");
    }

    #[test]
    fn detector_cells_become_boxes_in_the_picture() {
        // 64x64 input: 8x8 + 4x4 + 2x2 = 84 cells, 6 columns. The picture was
        // fitted at half size. Cell 9 (stride 8, x=1, y=1): center (12, 12),
        // width and height e^ln(4) * 8 = 32 in the input.
        let (rows, cols) = (84, 6);
        let mut flat = vec![0.0f32; rows * cols];
        let r = &mut flat[9 * cols..10 * cols];
        r.copy_from_slice(&[0.5, 0.5, 4.0f32.ln(), 4.0f32.ln(), 0.9, 0.8]);
        // A second, weaker cell on the same plate is merged away.
        let r = &mut flat[10 * cols..11 * cols];
        r.copy_from_slice(&[0.0, 0.5, 4.0f32.ln(), 4.0f32.ln(), 0.6, 0.8]);
        let b = decode_plates(&flat, rows, cols, 64, 0.5, 0.4);
        assert_eq!(b.len(), 1);
        assert!((b[0].score - 0.72).abs() < 1e-5);
        assert_eq!((b[0].x, b[0].y, b[0].w, b[0].h), (-8.0, -8.0, 64.0, 64.0));
        assert!(decode_plates(&flat, rows, cols, 64, 0.5, 0.95).is_empty());
        assert!(decode_plates(&[1.0; 3], rows, cols, 64, 0.5, 0.0).is_empty());
        assert!(decode_plates(&flat, rows, cols, 64, 0.0, 0.1).is_empty());
    }

    #[test]
    fn reader_slots_become_text_without_padding() {
        let n = ALPHABET.len();
        let mut out = vec![0.0f32; SLOTS * n];
        let put = |out: &mut Vec<f32>, slot: usize, ch: u8, p: f32| {
            let i = ALPHABET.iter().position(|&c| c == ch).unwrap();
            out[slot * n + i] = p;
        };
        for (slot, ch) in b"AB12".iter().enumerate() {
            put(&mut out, slot, *ch, 0.9);
        }
        for slot in 4..SLOTS {
            put(&mut out, slot, PAD, 0.99);
        }
        let r = decode_read(&out).unwrap();
        assert_eq!(r.text, "AB12");
        assert!((r.confidence - 0.9).abs() < 1e-5);
        // All padding, or too short an answer: nothing.
        let mut blank = vec![0.0f32; SLOTS * n];
        for slot in 0..SLOTS {
            put(&mut blank, slot, PAD, 1.0);
        }
        assert!(decode_read(&blank).is_none());
        assert!(decode_read(&[0.5; 10]).is_none());
    }

    #[test]
    fn plate_picture_is_cut_and_small() {
        let img = RgbImage::from_pixel(400, 300, image::Rgb([200, 200, 200]));
        let j = plate_jpeg(&img, &PlateBox { score: 0.9, x: 100.0, y: 100.0, w: 200.0, h: 60.0 }, 120).unwrap();
        let back = image::load_from_memory(&j).unwrap();
        assert_eq!(back.width(), 120);
        assert!(plate_jpeg(&img, &PlateBox { score: 0.9, x: 500.0, y: 500.0, w: 10.0, h: 10.0 }, 120).is_err());
    }

    #[test]
    #[cfg(not(all(target_os = "macos", target_arch = "x86_64")))] // (no AI runtime there: loading always refuses)
    fn models_are_only_loaded_when_present_and_checked() {
        let dir = std::env::temp_dir().join(format!("ambersight-lpr-models-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        assert!(!models_present(&dir));
        assert!(LprModels::load(&dir, 1).is_err(), "missing files");
        // A file of the right size but not the right content is refused and removed.
        std::fs::write(model_path(&dir, &READER), vec![0u8; READER.bytes as usize]).unwrap();
        assert!(models_present(&dir), "present means the files are there (the size is checked)");
        let e = LprModels::load(&dir, 1).err().unwrap().to_string();
        assert!(e.contains("checksum"), "{e}");
        assert!(!models_present(&dir), "the bad file is gone, so the download button fetches it again");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// With LPR_TEST_DIR holding the reader model (as downloaded by
    /// `ambersight lpr-test`; the detector is built in) and LPR_TEST_IMAGE a picture of a car: reads a plate.
    #[test]
    #[ignore]
    fn real_models_read_a_plate() {
        let dir = PathBuf::from(std::env::var("LPR_TEST_DIR").unwrap());
        let m = LprModels::load(&dir, 2).unwrap();
        let img = image::open(std::env::var("LPR_TEST_IMAGE").unwrap()).unwrap().to_rgb8();
        let found = m.detect(&img, 0.4).unwrap();
        assert!(!found.is_empty(), "no plate found");
        let r = m.read(&img, &found[0]).unwrap().unwrap();
        println!("read {} ({:.2})", r.text, r.confidence);
        assert!(r.text.len() >= 3);
    }
}
