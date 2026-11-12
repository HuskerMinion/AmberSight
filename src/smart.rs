//! Smart search: find recorded moments by describing them ("red truck",
//! "person with a yellow hat"). Event pictures and the objects AI found in
//! them are turned into CLIP fingerprints (512 numbers); a search turns the
//! words into one too and ranks the pictures by how alike they are.
//!
//! Model: OpenAI CLIP ViT-B/32 (MIT), the ONNX export in Xenova's
//! clip-vit-base-patch32 (8-bit), pinned by revision and checksum, about
//! 155 MB. Runs on the CPU (a picture takes well under a second), so no
//! graphics card is needed.

#![cfg_attr(all(target_os = "macos", target_arch = "x86_64"), allow(dead_code))]

use anyhow::{anyhow, bail, Context, Result};
use image::RgbImage;
use std::collections::HashMap;
use std::path::{Path, PathBuf};

pub struct SmartFile {
    pub file: &'static str,
    pub url: &'static str,
    pub sha256: &'static str,
    pub bytes: u64,
}

const REPO: &str = "https://huggingface.co/Xenova/clip-vit-base-patch32/resolve/6b7c523703ae26e39be4b03774966f8571c7f523/";

pub const FILES: [SmartFile; 4] = [
    SmartFile {
        file: "clip_vision_q8.onnx",
        url: "onnx/vision_model_quantized.onnx",
        sha256: "583fd1110a514667812fee7d684952aaf82a99b959760c8d7dca7e0ab9839299",
        bytes: 89_117_001,
    },
    SmartFile {
        file: "clip_text_q8.onnx",
        url: "onnx/text_model_quantized.onnx",
        sha256: "73baab855d406190da9faa498cfedf65f15cf309f4cc7385b7b032e6d08e5c3a",
        bytes: 64_504_507,
    },
    SmartFile { file: "clip_vocab.json", url: "vocab.json", sha256: "5047b556ce86ccaf6aa22b3ffccfc52d391ea4accdab9c2f2407da5b742d4363", bytes: 862_328 },
    SmartFile { file: "clip_merges.txt", url: "merges.txt", sha256: "9fd691f7c8039210e0fced15865466c65820d09b63988b0174bfe25de299051a", bytes: 524_619 },
];

/// Numbers in a fingerprint.
pub const DIM: usize = 512;

pub fn total_bytes() -> u64 {
    FILES.iter().map(|f| f.bytes).sum()
}

fn path_of(dir: &Path, f: &SmartFile) -> PathBuf {
    dir.join(f.file)
}

pub fn models_present(dir: &Path) -> bool {
    FILES.iter().all(|f| path_of(dir, f).metadata().is_ok_and(|m| m.len() == f.bytes))
}

/// Download the model files into `dir` (checked against their checksums).
/// `progress` gets (bytes so far, total).
pub fn download(dir: &Path, progress: &dyn Fn(u64, u64)) -> Result<()> {
    use sha2::{Digest, Sha256};
    use std::io::{Read, Write};
    std::fs::create_dir_all(dir)?;
    let total = total_bytes();
    let mut done = 0u64;
    for f in &FILES {
        let dest = path_of(dir, f);
        if dest.metadata().is_ok_and(|m| m.len() == f.bytes) {
            done += f.bytes;
            progress(done, total);
            continue;
        }
        let agent = ureq::Agent::config_builder().timeout_connect(Some(std::time::Duration::from_secs(20))).timeout_recv_response(Some(std::time::Duration::from_secs(60))).timeout_recv_body(Some(std::time::Duration::from_secs(3600))).build().new_agent();
        let url = format!("{REPO}{}", f.url);
        let mut resp = agent.get(&url).call().map_err(|e| anyhow!("download {}: {e}", f.file))?;
        let mut rd = resp.body_mut().with_config().limit(200 << 20).reader();
        let tmp = dest.with_extension("part");
        let mut out = std::fs::File::create(&tmp)?;
        let mut h = Sha256::new();
        let mut buf = vec![0u8; 1 << 16];
        loop {
            let n = rd.read(&mut buf)?;
            if n == 0 {
                break;
            }
            out.write_all(&buf[..n])?;
            h.update(&buf[..n]);
            done += n as u64;
            progress(done.min(total), total);
        }
        drop(out);
        let got: String = h.finalize().iter().map(|b| format!("{b:02x}")).collect();
        if got != f.sha256 {
            let _ = std::fs::remove_file(&tmp);
            bail!("{} did not match its checksum", f.file);
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

// ---------------------------------------------------------------------------
// Tokenizer (CLIP's byte-level BPE)
// ---------------------------------------------------------------------------

pub struct Tokenizer {
    vocab: HashMap<String, i64>,
    ranks: HashMap<(String, String), usize>,
    byte_char: [char; 256],
}

const START: i64 = 49406;
const END: i64 = 49407;
const MAX_TOKENS: usize = 77;

impl Tokenizer {
    pub fn load(vocab_json: &str, merges_txt: &str) -> Result<Self> {
        let vocab: HashMap<String, i64> = serde_json::from_str(vocab_json).context("vocab")?;
        let mut ranks = HashMap::new();
        // (As CLIP: the first 49152 - 256 - 2 merges after the version line.)
        for (i, line) in merges_txt.lines().skip(1).take(49152 - 256 - 2).enumerate() {
            if let Some((a, b)) = line.split_once(' ') {
                ranks.insert((a.to_string(), b.to_string()), i);
            }
        }
        if vocab.len() < 49000 || ranks.len() < 48000 {
            bail!("the tokenizer files are incomplete");
        }
        // GPT-2's byte to printable character table.
        let mut bs: Vec<u32> = (b'!' as u32..=b'~' as u32).chain(0xA1..=0xAC).chain(0xAE..=0xFF).collect();
        let mut cs = bs.clone();
        let mut n = 0;
        for b in 0..256u32 {
            if !bs.contains(&b) {
                bs.push(b);
                cs.push(256 + n);
                n += 1;
            }
        }
        let mut byte_char = [' '; 256];
        for (b, c) in bs.iter().zip(&cs) {
            byte_char[*b as usize] = char::from_u32(*c).unwrap();
        }
        Ok(Tokenizer { vocab, ranks, byte_char })
    }

    /// CLIP's pre-split: contractions, runs of letters, single digits, runs
    /// of other symbols; spaces dropped.
    fn words(text: &str) -> Vec<String> {
        let t: Vec<char> = text.split_whitespace().collect::<Vec<_>>().join(" ").to_lowercase().chars().collect();
        let mut out = Vec::new();
        let mut i = 0;
        while i < t.len() {
            let c = t[i];
            if c.is_whitespace() {
                i += 1;
                continue;
            }
            if c == '\'' {
                let rest: String = t[i + 1..t.len().min(i + 3)].iter().collect();
                if let Some(k) = ["re", "ve", "ll", "s", "t", "m", "d"].iter().find(|k| rest.starts_with(**k)) {
                    out.push(format!("'{k}"));
                    i += 1 + k.len();
                    continue;
                }
            }
            let start = i;
            if c.is_alphabetic() {
                while i < t.len() && t[i].is_alphabetic() {
                    i += 1;
                }
            } else if c.is_numeric() {
                i += 1;
            } else {
                while i < t.len() && !t[i].is_whitespace() && !t[i].is_alphabetic() && !t[i].is_numeric() {
                    i += 1;
                }
            }
            out.push(t[start..i].iter().collect());
        }
        out
    }

    fn bpe(&self, word: &str) -> Vec<String> {
        let chars: Vec<char> = word.bytes().map(|b| self.byte_char[b as usize]).collect();
        let mut parts: Vec<String> = chars.iter().map(|c| c.to_string()).collect();
        if let Some(last) = parts.last_mut() {
            last.push_str("</w>");
        }
        loop {
            let mut best: Option<(usize, usize)> = None;
            for i in 0..parts.len().saturating_sub(1) {
                if let Some(&r) = self.ranks.get(&(parts[i].clone(), parts[i + 1].clone())) {
                    if best.is_none_or(|b| r < b.0) {
                        best = Some((r, i));
                    }
                }
            }
            let Some((_, _)) = best else { break };
            let (a, b) = {
                let i = best.unwrap().1;
                (parts[i].clone(), parts[i + 1].clone())
            };
            // Merge every occurrence of the pair, left to right.
            let mut merged = Vec::with_capacity(parts.len());
            let mut i = 0;
            while i < parts.len() {
                if i + 1 < parts.len() && parts[i] == a && parts[i + 1] == b {
                    merged.push(format!("{a}{b}"));
                    i += 2;
                } else {
                    merged.push(parts[i].clone());
                    i += 1;
                }
            }
            parts = merged;
        }
        parts
    }

    /// Token ids with the start and end markers (at most 77).
    pub fn encode(&self, text: &str) -> Vec<i64> {
        let mut ids = vec![START];
        for w in Self::words(text) {
            for p in self.bpe(&w) {
                if let Some(&id) = self.vocab.get(&p) {
                    ids.push(id);
                }
            }
        }
        ids.truncate(MAX_TOKENS - 1);
        ids.push(END);
        ids
    }
}

// ---------------------------------------------------------------------------
// Pictures
// ---------------------------------------------------------------------------

const SIZE: u32 = 224;
const MEAN: [f32; 3] = [0.481_454_66, 0.457_827_5, 0.408_210_73];
const STD: [f32; 3] = [0.268_629_54, 0.261_302_58, 0.275_777_11];

#[cfg(not(all(target_os = "macos", target_arch = "x86_64")))]
/// The picture scaled to fit 224x224 whole (the sides padded with the
/// average color, which the model reads as nothing), as the model's input.
pub fn prepare(img: &RgbImage) -> ndarray::Array4<f32> {
    let (w, h) = (img.width().max(1) as f32, img.height().max(1) as f32);
    let r = (SIZE as f32 / w).min(SIZE as f32 / h);
    let (nw, nh) = (((w * r).round() as u32).clamp(1, SIZE), ((h * r).round() as u32).clamp(1, SIZE));
    let small = image::imageops::resize(img, nw, nh, image::imageops::FilterType::CatmullRom);
    let (ox, oy) = ((SIZE - nw) / 2, (SIZE - nh) / 2);
    let mut a = ndarray::Array4::<f32>::zeros((1, 3, SIZE as usize, SIZE as usize));
    for (x, y, p) in small.enumerate_pixels() {
        for c in 0..3 {
            a[[0, c, (y + oy) as usize, (x + ox) as usize]] = (p[c] as f32 / 255.0 - MEAN[c]) / STD[c];
        }
    }
    a
}

/// Part of a picture around a box (normalized 0..1), with some room around
/// it: what the object looks like close up.
pub fn crop_box(img: &RgbImage, x: f32, y: f32, w: f32, h: f32) -> Option<RgbImage> {
    let (iw, ih) = (img.width() as f32, img.height() as f32);
    let (mx, my) = (w * 0.15, h * 0.1);
    let x0 = ((x - mx) * iw).clamp(0.0, iw) as u32;
    let y0 = ((y - my) * ih).clamp(0.0, ih) as u32;
    let x1 = ((x + w + mx) * iw).clamp(0.0, iw) as u32;
    let y1 = ((y + h + my) * ih).clamp(0.0, ih) as u32;
    if x1 < x0 + 24 || y1 < y0 + 24 {
        return None;
    }
    Some(image::imageops::crop_imm(img, x0, y0, x1 - x0, y1 - y0).to_image())
}

fn normalized(mut v: Vec<f32>) -> Vec<f32> {
    let n = v.iter().map(|x| x * x).sum::<f32>().sqrt();
    if n > 0.0 {
        v.iter_mut().for_each(|x| *x /= n);
    }
    v
}

/// A fingerprint kept small: its largest number (f32), then each number as
/// a byte relative to it (516 bytes instead of 2 KB).
pub fn to_bytes(v: &[f32]) -> Vec<u8> {
    let m = v.iter().fold(0.0f32, |a, x| a.max(x.abs())).max(1e-9);
    let mut out = m.to_le_bytes().to_vec();
    out.extend(v.iter().map(|x| ((x / m * 127.0).round().clamp(-127.0, 127.0) as i8) as u8));
    out
}

/// A stored fingerprint as numbers (empty when it is not one).
pub fn unpack(stored: &[u8]) -> Vec<f32> {
    let Some((m, vals)) = stored.split_first_chunk::<4>() else { return Vec::new() };
    let m = f32::from_le_bytes(*m);
    vals.iter().map(|b| (*b as i8) as f32 * m / 127.0).collect()
}

/// How alike two stored fingerprints are (cosine: -1..1, 1 = the same).
#[cfg(test)]
pub fn similarity(a: &[u8], b: &[u8]) -> f32 {
    let (x, y) = (unpack(a), unpack(b));
    if x.is_empty() || x.len() != y.len() {
        return 0.0;
    }
    let dot: f32 = x.iter().zip(&y).map(|(p, q)| p * q).sum();
    let (nx, ny) = (x.iter().map(|v| v * v).sum::<f32>().sqrt(), y.iter().map(|v| v * v).sum::<f32>().sqrt());
    if nx > 0.0 && ny > 0.0 { dot / (nx * ny) } else { 0.0 }
}

/// How alike a stored fingerprint is to a search's (about -1..1).
pub fn score(stored: &[u8], q: &[f32]) -> f32 {
    let Some((m, vals)) = stored.split_first_chunk::<4>() else { return 0.0 };
    let m = f32::from_le_bytes(*m);
    vals.iter().zip(q).map(|(b, x)| (*b as i8) as f32 * x).sum::<f32>() * m / 127.0
}

// ---------------------------------------------------------------------------
// Models
// ---------------------------------------------------------------------------

#[cfg(not(all(target_os = "macos", target_arch = "x86_64")))]
pub struct SmartModels {
    vision: std::sync::Mutex<ort::session::Session>,
    text: std::sync::Mutex<ort::session::Session>,
    tok: Tokenizer,
    neutral: std::sync::OnceLock<Vec<f32>>,
    _env: ort::environment::Environment,
}

#[cfg(not(all(target_os = "macos", target_arch = "x86_64")))]
impl SmartModels {
    pub fn load(dir: &Path, threads: usize) -> Result<Self> {
        for f in &FILES {
            if file_sha256(&path_of(dir, f))? != f.sha256 {
                // Removed, so turning smart search off and on downloads it again.
                let _ = std::fs::remove_file(path_of(dir, f));
                bail!("{} does not match its checksum; turn smart search off and on to download it again", f.file);
            }
        }
        let env = crate::ai::runtime()?;
        let open = |f: &SmartFile| -> Result<ort::session::Session> {
            ort::session::Session::builder(&env)
                .map_err(|e| anyhow!("AI runtime: {e}"))?
                .with_intra_threads(threads.max(1))
                .map_err(|e| anyhow!("AI runtime: {e}"))?
                .commit_from_file(path_of(dir, f))
                .map_err(|e| anyhow!("could not load {}: {e}", f.file))
        };
        let tok = Tokenizer::load(&std::fs::read_to_string(path_of(dir, &FILES[2]))?, &std::fs::read_to_string(path_of(dir, &FILES[3]))?)?;
        Ok(SmartModels { vision: std::sync::Mutex::new(open(&FILES[0])?), text: std::sync::Mutex::new(open(&FILES[1])?), tok, neutral: Default::default(), _env: env })
    }

    /// The fingerprint of a picture (unit length).
    pub fn embed_image(&self, img: &RgbImage) -> Result<Vec<f32>> {
        let t = ort::value::Tensor::from_array(prepare(img)).map_err(|e| anyhow!("picture input: {e}"))?;
        let mut s = self.vision.lock().unwrap();
        let out = s.run(ort::inputs!["pixel_values" => t]).map_err(|e| anyhow!("smart search picture: {e}"))?;
        let v: Vec<f32> = out["image_embeds"].try_extract_array::<f32>().map_err(|e| anyhow!("picture output: {e}"))?.iter().copied().collect();
        if v.len() != DIM {
            bail!("unexpected picture output ({} numbers)", v.len());
        }
        Ok(normalized(v))
    }

    /// The fingerprint of a description (unit length).
    pub fn embed_text(&self, text: &str) -> Result<Vec<f32>> {
        let ids = self.tok.encode(text);
        let n = ids.len();
        let t = ort::value::Tensor::from_array(ndarray::Array2::from_shape_vec((1, n), ids)?).map_err(|e| anyhow!("text input: {e}"))?;
        let mask = ort::value::Tensor::from_array(ndarray::Array2::<i64>::ones((1, n))).map_err(|e| anyhow!("text input: {e}"))?;
        let mut s = self.text.lock().unwrap();
        let wants_mask = s.inputs().iter().any(|i| i.name() == "attention_mask");
        let out = if wants_mask {
            s.run(ort::inputs!["input_ids" => t, "attention_mask" => mask])
        } else {
            s.run(ort::inputs!["input_ids" => t])
        }
        .map_err(|e| anyhow!("smart search words: {e}"))?;
        let v: Vec<f32> = out["text_embeds"].try_extract_array::<f32>().map_err(|e| anyhow!("text output: {e}"))?.iter().copied().collect();
        if v.len() != DIM {
            bail!("unexpected text output ({} numbers)", v.len());
        }
        Ok(normalized(v))
    }

    /// The fingerprint of words that describe nothing in particular (kept).
    pub fn neutral(&self) -> Result<Vec<f32>> {
        if let Some(v) = self.neutral.get() {
            return Ok(v.clone());
        }
        let v = self.embed_query("something")?;
        let _ = self.neutral.set(v.clone());
        Ok(v)
    }

    /// A search's fingerprint: the description as written and as "a photo
    /// of ..." (how CLIP was taught), averaged.
    pub fn embed_query(&self, q: &str) -> Result<Vec<f32>> {
        let a = self.embed_text(q)?;
        let b = self.embed_text(&format!("a photo of {q}."))?;
        Ok(normalized(a.iter().zip(&b).map(|(x, y)| x + y).collect()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dir() -> Option<PathBuf> {
        std::env::var_os("AMBERSIGHT_CLIP_DIR").map(PathBuf::from)
    }

    #[test]
    fn words_split_like_clip() {
        assert_eq!(Tokenizer::words("A red  truck's 42 wheels!!"), vec!["a", "red", "truck", "'s", "4", "2", "wheels", "!!"]);
    }

    #[test]
    fn fingerprints_keep_their_order_when_stored_small() {
        let a = normalized((0..DIM).map(|i| (i as f32 * 0.37).sin()).collect());
        let b = normalized((0..DIM).map(|i| (i as f32 * 0.11).cos()).collect());
        let exact = |x: &[f32], y: &[f32]| x.iter().zip(y).map(|(p, q)| p * q).sum::<f32>();
        assert!((score(&to_bytes(&a), &a) - 1.0).abs() < 0.03);
        assert!((score(&to_bytes(&a), &b) - exact(&a, &b)).abs() < 0.03);
    }

    #[cfg(not(all(target_os = "macos", target_arch = "x86_64")))]
    #[test]
    fn padded_picture_input_is_224_square() {
        let img = RgbImage::from_pixel(640, 360, image::Rgb([255, 0, 0]));
        let a = prepare(&img);
        assert_eq!(a.shape(), &[1, 3, 224, 224]);
        // Padding (top) reads as the average color: 0.
        assert_eq!(a[[0, 0, 0, 100]], 0.0);
        assert!(a[[0, 0, 112, 100]] > 1.5);
    }

    #[cfg(not(all(target_os = "macos", target_arch = "x86_64")))]
    /// With the real files (AMBERSIGHT_CLIP_DIR): tokens as CLIP makes them,
    /// and a picture of cats matches "cats" better than "a red truck".
    #[test]
    #[ignore]
    fn real_model_matches_words_to_pictures() {
        let Some(d) = dir() else { return };
        let m = SmartModels::load(&d, 2).unwrap();
        assert_eq!(m.tok.encode("a photo of a cat"), vec![49406, 320, 1125, 539, 320, 2368, 49407]);
        let img = image::open(d.join("cats.jpg")).unwrap().to_rgb8();
        let v = m.embed_image(&img).unwrap();
        let s = |q: &str| {
            let t = m.embed_query(q).unwrap();
            v.iter().zip(&t).map(|(a, b)| a * b).sum::<f32>()
        };
        let (cats, truck, hat) = (s("two cats"), s("a red truck"), s("a person with a yellow hat"));
        eprintln!("cats {cats:.3} truck {truck:.3} hat {hat:.3}");
        assert!(cats > truck + 0.05 && cats > hat + 0.05);
    }
}
