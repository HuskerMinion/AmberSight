//! Face recognition: find faces (YuNet) and turn each into a fingerprint
//! (SFace, 128 numbers) that is compared with the faces of people the user
//! has named. Both models come from the OpenCV model zoo (YuNet: MIT,
//! SFace: Apache-2.0), pinned by checksum, and run on this server.

// Intel Macs have no built-in AI runtime, so most of this goes unused there.
#![cfg_attr(all(target_os = "macos", target_arch = "x86_64"), allow(dead_code))]

use anyhow::{anyhow, bail, Context, Result};
use image::RgbImage;
use std::path::{Path, PathBuf};

pub struct FaceModel {
    pub file: &'static str,
    pub url: &'static str,
    pub sha256: &'static str,
    pub bytes: u64,
}

pub const DETECTOR: FaceModel = FaceModel {
    file: "face_detection_yunet_2023mar.onnx",
    url: "https://github.com/opencv/opencv_zoo/raw/main/models/face_detection_yunet/face_detection_yunet_2023mar.onnx",
    sha256: "8f2383e4dd3cfbb4553ea8718107fc0423210dc964f9f4280604804ed2552fa4",
    bytes: 232_589,
};

pub const RECOGNIZER: FaceModel = FaceModel {
    file: "face_recognition_sface_2021dec.onnx",
    url: "https://github.com/opencv/opencv_zoo/raw/main/models/face_recognition_sface/face_recognition_sface_2021dec.onnx",
    sha256: "0ba9fbfa01b5270c96627c4ef784da859931e02f04419c829e83484087c34e79",
    bytes: 38_696_353,
};

/// A face found in an image, in that image's pixels.
#[derive(Clone, Debug)]
pub struct Face {
    pub score: f32,
    pub x: f32,
    pub y: f32,
    pub w: f32,
    pub h: f32,
    /// Right eye, left eye, nose tip, right and left mouth corner.
    pub kps: [(f32, f32); 5],
}

pub fn model_path(dir: &Path, m: &FaceModel) -> PathBuf {
    dir.join(m.file)
}

pub fn models_present(dir: &Path) -> bool {
    [&DETECTOR, &RECOGNIZER].iter().all(|m| model_path(dir, m).metadata().is_ok_and(|x| x.len() == m.bytes))
}

/// Download both models into `dir` (checked against their checksums).
/// `progress` gets (bytes so far, total).
pub fn download_models(dir: &Path, progress: &dyn Fn(u64, u64)) -> Result<()> {
    use sha2::{Digest, Sha256};
    use std::io::{Read, Write};
    std::fs::create_dir_all(dir)?;
    let total = DETECTOR.bytes + RECOGNIZER.bytes;
    let mut done = 0u64;
    for m in [&DETECTOR, &RECOGNIZER] {
        let dest = model_path(dir, m);
        if dest.metadata().is_ok_and(|x| x.len() == m.bytes) {
            done += m.bytes;
            progress(done, total);
            continue;
        }
        let agent = ureq::Agent::config_builder().timeout_connect(Some(std::time::Duration::from_secs(20))).timeout_recv_response(Some(std::time::Duration::from_secs(60))).timeout_recv_body(Some(std::time::Duration::from_secs(3600))).build().new_agent();
        let mut resp = agent.get(m.url).call().map_err(|e| anyhow!("download {}: {e}", m.file))?;
        let mut rd = resp.body_mut().with_config().limit(100 << 20).reader();
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

/// Cosine similarity of two fingerprints.
pub fn cosine(a: &[f32], b: &[f32]) -> f32 {
    let (mut d, mut na, mut nb) = (0.0f32, 0.0f32, 0.0f32);
    for (x, y) in a.iter().zip(b) {
        d += x * y;
        na += x * x;
        nb += y * y;
    }
    if na <= 0.0 || nb <= 0.0 {
        0.0
    } else {
        d / (na.sqrt() * nb.sqrt())
    }
}

pub fn emb_to_bytes(e: &[f32]) -> Vec<u8> {
    e.iter().flat_map(|v| v.to_le_bytes()).collect()
}

pub fn emb_from_bytes(b: &[u8]) -> Vec<f32> {
    b.chunks_exact(4).map(|c| f32::from_le_bytes([c[0], c[1], c[2], c[3]])).collect()
}

/// Where the five landmarks sit in SFace's 112x112 input.
const ALIGNED: [(f32, f32); 5] = [(38.2946, 51.6963), (73.5318, 51.5014), (56.0252, 71.7366), (41.5493, 92.3655), (70.7299, 92.2041)];

/// The rotation+scale+shift (no mirroring) that best maps `src` onto `dst`
/// (least squares): returns (a, b, tx, ty) for x' = a x - b y + tx,
/// y' = b x + a y + ty.
pub fn similarity(src: &[(f32, f32)], dst: &[(f32, f32)]) -> (f32, f32, f32, f32) {
    let n = src.len() as f32;
    let (sx, sy) = src.iter().fold((0.0, 0.0), |a, p| (a.0 + p.0 / n, a.1 + p.1 / n));
    let (dx, dy) = dst.iter().fold((0.0, 0.0), |a, p| (a.0 + p.0 / n, a.1 + p.1 / n));
    let (mut num_a, mut num_b, mut den) = (0.0f32, 0.0f32, 0.0f32);
    for (s, d) in src.iter().zip(dst) {
        let (px, py, qx, qy) = (s.0 - sx, s.1 - sy, d.0 - dx, d.1 - dy);
        num_a += px * qx + py * qy;
        num_b += px * qy - py * qx;
        den += px * px + py * py;
    }
    let (a, b) = if den > 0.0 { (num_a / den, num_b / den) } else { (1.0, 0.0) };
    (a, b, dx - (a * sx - b * sy), dy - (b * sx + a * sy))
}

/// The face cut out and turned upright as SFace expects (112x112).
pub fn align(img: &RgbImage, kps: &[(f32, f32); 5]) -> RgbImage {
    let (a, b, tx, ty) = similarity(kps, &ALIGNED);
    let k = a * a + b * b;
    let mut out = RgbImage::new(112, 112);
    let (w, h) = (img.width() as i64, img.height() as i64);
    for (u, v, px) in out.enumerate_pixels_mut() {
        // Inverse map: the source point that lands on (u, v).
        let (qx, qy) = (u as f32 - tx, v as f32 - ty);
        let (x, y) = ((a * qx + b * qy) / k, (-b * qx + a * qy) / k);
        let (x0, y0) = (x.floor(), y.floor());
        let (fx, fy) = (x - x0, y - y0);
        let at = |xx: i64, yy: i64| -> [f32; 3] {
            if xx < 0 || yy < 0 || xx >= w || yy >= h {
                return [0.0; 3];
            }
            let p = img.get_pixel(xx as u32, yy as u32);
            [p[0] as f32, p[1] as f32, p[2] as f32]
        };
        let (x0, y0) = (x0 as i64, y0 as i64);
        let (p00, p10, p01, p11) = (at(x0, y0), at(x0 + 1, y0), at(x0, y0 + 1), at(x0 + 1, y0 + 1));
        for c in 0..3 {
            let top = p00[c] * (1.0 - fx) + p10[c] * fx;
            let bot = p01[c] * (1.0 - fx) + p11[c] * fx;
            px[c] = (top * (1.0 - fy) + bot * fy).round().clamp(0.0, 255.0) as u8;
        }
    }
    out
}

fn iou(a: &Face, b: &Face) -> f32 {
    let iw = ((a.x + a.w).min(b.x + b.w) - a.x.max(b.x)).max(0.0);
    let ih = ((a.y + a.h).min(b.y + b.h) - a.y.max(b.y)).max(0.0);
    let inter = iw * ih;
    let union = a.w * a.h + b.w * b.h - inter;
    if union <= 0.0 { 0.0 } else { inter / union }
}

/// YuNet's three output scales decoded into faces (input pixels), best
/// first, overlaps removed. `outs` per stride 8, 16, 32: (cls, obj, bbox, kps).
pub fn decode_yunet(size: usize, outs: &[(&[f32], &[f32], &[f32], &[f32]); 3], min_score: f32) -> Vec<Face> {
    let mut faces = Vec::new();
    for (si, stride) in [8usize, 16, 32].into_iter().enumerate() {
        let (cls, obj, bbox, kps) = outs[si];
        let cols = size / stride;
        // A model with other outputs than YuNet's: nothing, rather than a panic.
        if obj.len() < cls.len() || bbox.len() < cls.len() * 4 || kps.len() < cls.len() * 10 {
            continue;
        }
        for (idx, (&c, &o)) in cls.iter().zip(obj).enumerate() {
            let score = (c.clamp(0.0, 1.0) * o.clamp(0.0, 1.0)).sqrt();
            if score < min_score {
                continue;
            }
            let (r, col) = ((idx / cols) as f32, (idx % cols) as f32);
            let s = stride as f32;
            let b = &bbox[idx * 4..idx * 4 + 4];
            let (cx, cy) = ((col + b[0]) * s, (r + b[1]) * s);
            let (w, h) = (b[2].exp() * s, b[3].exp() * s);
            let k = &kps[idx * 10..idx * 10 + 10];
            let mut pts = [(0.0, 0.0); 5];
            for (n, p) in pts.iter_mut().enumerate() {
                *p = ((k[2 * n] + col) * s, (k[2 * n + 1] + r) * s);
            }
            faces.push(Face { score, x: cx - w / 2.0, y: cy - h / 2.0, w, h, kps: pts });
        }
    }
    faces.sort_by(|a, b| b.score.total_cmp(&a.score));
    let mut keep: Vec<Face> = Vec::new();
    for f in faces {
        if keep.iter().all(|k| iou(k, &f) < 0.3) {
            keep.push(f);
        }
    }
    keep
}

#[cfg(not(all(target_os = "macos", target_arch = "x86_64")))]
pub struct FaceModels {
    det: std::sync::Mutex<ort::session::Session>,
    rec: std::sync::Mutex<ort::session::Session>,
    _env: ort::environment::Environment,
}

#[cfg(not(all(target_os = "macos", target_arch = "x86_64")))]
impl FaceModels {
    pub fn load(dir: &Path, threads: usize) -> Result<Self> {
        // Only the files that were downloaded and checked (not just any file
        // of the right size).
        for m in [&DETECTOR, &RECOGNIZER] {
            if file_sha256(&model_path(dir, m))? != m.sha256 {
                // Removed, so the download button fetches it again.
                let _ = std::fs::remove_file(model_path(dir, m));
                bail!("{} does not match its checksum; download the face models again", m.file);
            }
        }
        let env = crate::ai::runtime()?;
        let open = |m: &FaceModel| -> Result<ort::session::Session> {
            ort::session::Session::builder(&env)
                .map_err(|e| anyhow!("AI runtime: {e}"))?
                .with_intra_threads(threads.max(1))
                .map_err(|e| anyhow!("AI runtime: {e}"))?
                .commit_from_file(model_path(dir, m))
                .map_err(|e| anyhow!("could not load {}: {e}", m.file))
        };
        Ok(FaceModels { det: std::sync::Mutex::new(open(&DETECTOR)?), rec: std::sync::Mutex::new(open(&RECOGNIZER)?), _env: env })
    }

    /// Faces in `img` (its pixels), at least `min_score` sure.
    pub fn detect(&self, img: &RgbImage, min_score: f32) -> Result<Vec<Face>> {
        const S: usize = 640;
        let (iw, ih) = (img.width() as f32, img.height() as f32);
        let r = (S as f32 / iw).min(S as f32 / ih);
        let (nw, nh) = (((iw * r).round() as u32).clamp(1, S as u32), ((ih * r).round() as u32).clamp(1, S as u32));
        let resized = image::imageops::resize(img, nw, nh, image::imageops::FilterType::Triangle);
        let mut input = ndarray::Array4::<f32>::zeros((1, 3, S, S));
        for (x, y, p) in resized.enumerate_pixels() {
            let (x, y) = (x as usize, y as usize);
            // BGR, 0-255.
            input[[0, 0, y, x]] = p[2] as f32;
            input[[0, 1, y, x]] = p[1] as f32;
            input[[0, 2, y, x]] = p[0] as f32;
        }
        let tensor = ort::value::Tensor::from_array(input).map_err(|e| anyhow!("face input: {e}"))?;
        let mut s = self.det.lock().unwrap();
        let out = s.run(ort::inputs![tensor]).map_err(|e| anyhow!("face detection: {e}"))?;
        let get = |name: &str| -> Result<Vec<f32>> {
            Ok(out[name].try_extract_array::<f32>().map_err(|e| anyhow!("face output {name}: {e}"))?.iter().copied().collect())
        };
        let mut v = Vec::new();
        for st in [8, 16, 32] {
            v.push((get(&format!("cls_{st}"))?, get(&format!("obj_{st}"))?, get(&format!("bbox_{st}"))?, get(&format!("kps_{st}"))?));
        }
        let outs = [
            (v[0].0.as_slice(), v[0].1.as_slice(), v[0].2.as_slice(), v[0].3.as_slice()),
            (v[1].0.as_slice(), v[1].1.as_slice(), v[1].2.as_slice(), v[1].3.as_slice()),
            (v[2].0.as_slice(), v[2].1.as_slice(), v[2].2.as_slice(), v[2].3.as_slice()),
        ];
        Ok(decode_yunet(S, &outs, min_score)
            .into_iter()
            .map(|f| Face {
                score: f.score,
                x: f.x / r,
                y: f.y / r,
                w: f.w / r,
                h: f.h / r,
                kps: f.kps.map(|(x, y)| (x / r, y / r)),
            })
            .collect())
    }

    /// The face's fingerprint (128 numbers, unit length).
    pub fn embed(&self, img: &RgbImage, face: &Face) -> Result<Vec<f32>> {
        let a = align(img, &face.kps);
        let mut input = ndarray::Array4::<f32>::zeros((1, 3, 112, 112));
        for (x, y, p) in a.enumerate_pixels() {
            // RGB, 0-255.
            for c in 0..3 {
                input[[0, c, y as usize, x as usize]] = p[c] as f32;
            }
        }
        let tensor = ort::value::Tensor::from_array(input).map_err(|e| anyhow!("face input: {e}"))?;
        let mut s = self.rec.lock().unwrap();
        let out = s.run(ort::inputs![tensor]).map_err(|e| anyhow!("face recognition: {e}"))?;
        let mut e: Vec<f32> = out[0].try_extract_array::<f32>().map_err(|e| anyhow!("face output: {e}"))?.iter().copied().collect();
        let n = e.iter().map(|x| x * x).sum::<f32>().sqrt();
        if n > 0.0 {
            e.iter_mut().for_each(|x| *x /= n);
        }
        Ok(e)
    }
}

/// Intel Macs have no built-in AI runtime.
#[cfg(all(target_os = "macos", target_arch = "x86_64"))]
pub struct FaceModels;

#[cfg(all(target_os = "macos", target_arch = "x86_64"))]
impl FaceModels {
    pub fn load(_dir: &Path, _threads: usize) -> Result<Self> {
        bail!("face recognition needs the built-in AI runtime, which this Mac does not have")
    }
    pub fn detect(&self, _img: &RgbImage, _min_score: f32) -> Result<Vec<Face>> {
        bail!("not available")
    }
    pub fn embed(&self, _img: &RgbImage, _face: &Face) -> Result<Vec<f32>> {
        bail!("not available")
    }
}

/// Whether a face is clear enough to recognize others by: big enough, the
/// detector sure it is a face, and sharp (fine detail, see sharpness_parts;
/// unknown counts as sharp). Measured on real camera faces: blurred or
/// distant ones have detail under ~12, clear ones 15-40.
pub const CLEAR_MIN_SIZE: u32 = 64;
pub const CLEAR_MIN_SCORE: f32 = 0.85;
pub const CLEAR_MIN_DETAIL: f32 = 12.0;
pub fn is_clear(size: u32, score: f32, detail: Option<f32>) -> bool {
    size >= CLEAR_MIN_SIZE && score >= CLEAR_MIN_SCORE && detail.is_none_or(|d| d >= CLEAR_MIN_DETAIL)
}

/// How sharp a face picture (from `face_jpeg`) is: the spread of fine detail
/// (a Laplacian) over the face, and the spread of brightness, both 0-255
/// scale. Small or blurred faces have little fine detail.
pub fn sharpness_parts(jpeg: &[u8]) -> Option<(f32, f32)> {
    let img = image::load_from_memory(jpeg).ok()?.to_luma8();
    let img = if img.width() != 128 || img.height() != 128 { image::imageops::resize(&img, 128, 128, image::imageops::FilterType::Triangle) } else { img };
    // The face itself: the picture has a margin around it.
    let (a, b) = (24u32, 104u32);
    let px = |x: u32, y: u32| img.get_pixel(x, y)[0] as f32;
    let (mut n, mut sl, mut sl2, mut sg, mut sg2) = (0f32, 0f32, 0f32, 0f32, 0f32);
    for y in a..b {
        for x in a..b {
            let c = px(x, y);
            let l = 4.0 * c - px(x - 1, y) - px(x + 1, y) - px(x, y - 1) - px(x, y + 1);
            n += 1.0;
            sl += l;
            sl2 += l * l;
            sg += c;
            sg2 += c * c;
        }
    }
    let sd = |s: f32, s2: f32| (s2 / n - (s / n).powi(2)).max(0.0).sqrt();
    Some((sd(sl, sl2), sd(sg, sg2)))
}

/// A square picture of the face with some margin, `side` pixels, as JPEG.
pub fn face_jpeg(img: &RgbImage, f: &Face, side: u32) -> Result<Vec<u8>> {
    let m = 0.35;
    let cx = f.x + f.w / 2.0;
    let cy = f.y + f.h / 2.0;
    let half = (f.w.max(f.h) * (1.0 + m)) / 2.0;
    let x0 = (cx - half).max(0.0) as u32;
    let y0 = (cy - half).max(0.0) as u32;
    let x1 = ((cx + half) as u32).min(img.width());
    let y1 = ((cy + half) as u32).min(img.height());
    if x1 <= x0 || y1 <= y0 {
        bail!("face outside the picture");
    }
    let crop = image::imageops::crop_imm(img, x0, y0, x1 - x0, y1 - y0).to_image();
    let small = image::imageops::resize(&crop, side, side, image::imageops::FilterType::Triangle);
    let mut out = Vec::new();
    image::codecs::jpeg::JpegEncoder::new_with_quality(&mut out, 85).encode_image(&small).context("face picture")?;
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn similarity_recovers_rotation_scale_and_shift() {
        // Rotate 30 degrees, scale 2, shift (5, -3).
        let (c, s) = (2.0 * 30f32.to_radians().cos(), 2.0 * 30f32.to_radians().sin());
        let src = [(0.0, 0.0), (10.0, 0.0), (5.0, 8.0), (1.0, 12.0), (9.0, 12.0)];
        let dst: Vec<(f32, f32)> = src.iter().map(|&(x, y)| (c * x - s * y + 5.0, s * x + c * y - 3.0)).collect();
        let (a, b, tx, ty) = similarity(&src, &dst);
        assert!((a - c).abs() < 1e-4 && (b - s).abs() < 1e-4 && (tx - 5.0).abs() < 1e-3 && (ty + 3.0).abs() < 1e-3);
    }

    #[test]
    fn cosine_and_bytes() {
        let a = [1.0, 0.0, 0.0];
        assert!((cosine(&a, &[2.0, 0.0, 0.0]) - 1.0).abs() < 1e-6);
        assert!(cosine(&a, &[0.0, 1.0, 0.0]).abs() < 1e-6);
        let e = vec![0.25f32, -1.5, 3.0];
        assert_eq!(emb_from_bytes(&emb_to_bytes(&e)), e);
    }

    #[test]
    fn yunet_decoding_places_a_face() {
        // One confident anchor at stride 32, row 2, col 3 of a 640 input.
        let size = 640;
        let n8 = (size / 8) * (size / 8);
        let n16 = (size / 16) * (size / 16);
        let n32 = (size / 32) * (size / 32);
        let z = |n: usize, k: usize| vec![0.0f32; n * k];
        let (c8, o8, b8, k8) = (z(n8, 1), z(n8, 1), z(n8, 4), z(n8, 10));
        let (c16, o16, b16, k16) = (z(n16, 1), z(n16, 1), z(n16, 4), z(n16, 10));
        let (mut c32, mut o32, mut b32, mut k32) = (z(n32, 1), z(n32, 1), z(n32, 4), z(n32, 10));
        let idx = 2 * (size / 32) + 3;
        c32[idx] = 0.9;
        o32[idx] = 0.9;
        b32[idx * 4..idx * 4 + 4].copy_from_slice(&[0.5, 0.5, 1f32.ln(), 2f32.ln()]);
        k32[idx * 10] = 0.25;
        k32[idx * 10 + 1] = 0.25;
        let f = decode_yunet(size, &[(&c8, &o8, &b8, &k8), (&c16, &o16, &b16, &k16), (&c32, &o32, &b32, &k32)], 0.5);
        assert_eq!(f.len(), 1);
        let f = &f[0];
        assert!((f.score - 0.9).abs() < 1e-5);
        // Center (3.5, 2.5) * 32 = (112, 80); 32 wide, 64 high.
        assert_eq!((f.x, f.y, f.w, f.h), (96.0, 48.0, 32.0, 64.0));
        assert_eq!(f.kps[0], (104.0, 72.0));
    }

    /// With FACE_TEST_DIR holding the two models and OpenCV's sample photos
    /// lena.jpg and messi5.jpg: one face each, the same face matches itself
    /// at another size, two people do not.
    #[test]
    #[ignore]
    fn real_models_on_sample_photos() {
        let dir = PathBuf::from(std::env::var("FACE_TEST_DIR").unwrap());
        let m = FaceModels::load(&dir, 2).unwrap();
        let lena = image::open(dir.join("lena.jpg")).unwrap().to_rgb8();
        let messi = image::open(dir.join("messi5.jpg")).unwrap().to_rgb8();
        let fl = m.detect(&lena, 0.8).unwrap();
        let fm = m.detect(&messi, 0.8).unwrap();
        println!("lena faces {:?}", fl.iter().map(|f| (f.score, f.x as i32, f.y as i32, f.w as i32)).collect::<Vec<_>>());
        println!("messi faces {:?}", fm.iter().map(|f| (f.score, f.x as i32, f.y as i32, f.w as i32)).collect::<Vec<_>>());
        assert_eq!(fl.len(), 1);
        assert!(!fm.is_empty());
        let e1 = m.embed(&lena, &fl[0]).unwrap();
        let small = image::imageops::resize(&lena, lena.width() * 3 / 5, lena.height() * 3 / 5, image::imageops::FilterType::Triangle);
        let fs = m.detect(&small, 0.8).unwrap();
        let e2 = m.embed(&small, &fs[0]).unwrap();
        let e3 = m.embed(&messi, &fm[0]).unwrap();
        let same = cosine(&e1, &e2);
        let other = cosine(&e1, &e3);
        println!("same person {same:.3}, different people {other:.3}");
        assert!(same > 0.6, "same face: {same}");
        assert!(other < 0.3, "different faces: {other}");
        std::fs::write(dir.join("lena-face.jpg"), face_jpeg(&lena, &fl[0], 128).unwrap()).unwrap();
        align(&lena, &fl[0].kps).save(dir.join("lena-aligned.png")).unwrap();
    }
}
