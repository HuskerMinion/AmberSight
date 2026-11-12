use image::imageops::FilterType;

use crate::config::{ZonePoly, ZoneRect};

/// Frame-to-frame motion detector. Decodes a JPEG frame, reduces it to a
/// small grayscale image and compares it against the previous frame.
/// Optional motion zones (restrict) and ignore zones (suppress) are given
/// as normalized rectangles in 0.0..1.0 camera coordinates.
pub struct MotionDetector {
    prev: Option<Vec<u8>>,
    width: u32,
    height: u32,
    /// The object-based detector (default algorithm).
    pub objects: ObjectDetector,
}

impl MotionDetector {
    pub fn new() -> Self {
        MotionDetector { prev: None, width: 160, height: 90, objects: ObjectDetector::new() }
    }

    /// Returns a score in 0.0..1.0 (mean absolute luma difference / 255),
    /// counted only over pixels allowed by the zones. Returns None if the
    /// frame could not be decoded.
    pub fn score(
        &mut self,
        frame: &[u8],
        zones: &[ZoneRect],
        ignore_zones: &[ZoneRect],
    ) -> Option<f64> {
        let img = match image::load_from_memory(frame) {
            Ok(i) => i,
            Err(_) => return None,
        };
        let luma = img.to_luma8();
        let small = image::imageops::resize(&luma, self.width, self.height, FilterType::Triangle);
        let cur = small.into_raw();
        let score = match &self.prev {
            Some(p) => {
                let mask = self.zone_mask(zones, ignore_zones);
                let mut acc: u64 = 0;
                let mut counted: u64 = 0;
                for (i, (a, b)) in p.iter().zip(cur.iter()).enumerate() {
                    if !mask[i] {
                        continue;
                    }
                    counted += 1;
                    acc += if a > b { (*a - *b) as u64 } else { (*b - *a) as u64 };
                }
                if counted == 0 {
                    0.0
                } else {
                    acc as f64 / (counted as f64 * 255.0)
                }
            }
            None => 0.0,
        };
        self.prev = Some(cur);
        Some(score)
    }

    /// Build a per-pixel include mask for the analysis grid from normalized
    /// zone rectangles. Empty `zones` means "whole frame".
    fn zone_mask(&self, zones: &[ZoneRect], ignore_zones: &[ZoneRect]) -> Vec<bool> {
        let w = self.width as i64;
        let h = self.height as i64;
        let rect_cells = &(|r: &ZoneRect| -> (i64, i64, i64, i64) {
            (
                (r.x.max(0.0) * w as f64) as i64,
                (r.y.max(0.0) * h as f64) as i64,
                ((r.x + r.w).min(1.0) * w as f64) as i64,
                ((r.y + r.h).min(1.0) * h as f64) as i64,
            )
        });
        let in_any = &(|r: &ZoneRect, x: i64, y: i64| {
            let (x0, y0, x1, y1) = rect_cells(r);
            x >= x0 && x < x1 && y >= y0 && y < y1
        });
        let mut mask = Vec::with_capacity((w * h) as usize);
        for y in 0..h {
            for x in 0..w {
                let included = zones.is_empty()
                    || zones.iter().any(|r| in_any(r, x, y));
                let ignored = ignore_zones.iter().any(|r| in_any(r, x, y));
                mask.push(included && !ignored);
            }
        }
        mask
    }
}

/// Tuning for the object-based detector. Sizes and distances are
/// fractions of the frame (area / diagonal) so they work at any resolution.
#[derive(Clone, Debug)]
pub struct ObjectParams {
    /// Smallest object that counts, as a fraction of the frame area.
    pub min_object: f64,
    /// Larger "objects" are treated as a lighting change, not motion.
    pub max_object: f64,
    /// Per-pixel brightness difference (0-255) from the background that
    /// counts as changed.
    pub contrast: u8,
    /// An object must travel this far (fraction of the frame diagonal)
    /// before it triggers; 0 = any object triggers.
    pub min_travel: f64,
}

impl Default for ObjectParams {
    fn default() -> Self {
        ObjectParams { min_object: 0.002, max_object: 0.5, contrast: 25, min_travel: 0.03 }
    }
}

/// A moving region in normalized frame coordinates (0..1).
#[derive(Clone, Debug, PartialEq, serde::Serialize)]
pub struct Blob {
    pub x: f64,
    pub y: f64,
    pub w: f64,
    pub h: f64,
    /// Area as a fraction of the frame.
    pub area: f64,
}

#[derive(Clone, Debug, Default)]
pub struct MotionResult {
    /// Largest object's area as a fraction of the frame (0 when none).
    pub score: f64,
    /// Objects found in this frame (within zones, size-filtered).
    pub blobs: Vec<Blob>,
    /// True when an object has moved far enough to count as motion.
    pub triggered: bool,
    /// Most of the frame changed at once (lights, clouds, night mode): the
    /// background was reset and nothing triggered.
    pub lighting_change: bool,
}

struct Track {
    start: (f64, f64),
    last: (f64, f64),
    age: u32,
    missed: u32,
}

/// Object-based motion detector: background model, per-pixel threshold,
/// noise cleanup, connected regions, size filters and simple tracking.
pub struct ObjectDetector {
    w: usize,
    h: usize,
    bg: Option<Vec<f32>>,
    tracks: Vec<Track>,
    mask_cache: Option<(Vec<ZoneRect>, Vec<ZoneRect>, Vec<ZonePoly>, Vec<ZonePoly>, Vec<bool>)>,
    /// Polygon zones used by the next `analyze` calls (set via `set_polygons`).
    polys: (Vec<ZonePoly>, Vec<ZonePoly>),
}

impl Default for ObjectDetector {
    fn default() -> Self {
        Self::new()
    }
}

impl ObjectDetector {
    pub fn new() -> Self {
        ObjectDetector { w: 192, h: 108, bg: None, tracks: Vec::new(), mask_cache: None, polys: (Vec::new(), Vec::new()) }
    }

    /// Polygon zones (include, ignore) applied together with the rectangles.
    pub fn set_polygons(&mut self, include: &[ZonePoly], ignore: &[ZonePoly]) {
        if self.polys.0.as_slice() != include || self.polys.1.as_slice() != ignore {
            self.polys = (include.to_vec(), ignore.to_vec());
        }
    }

    pub fn analyze(
        &mut self,
        frame: &[u8],
        zones: &[ZoneRect],
        ignore_zones: &[ZoneRect],
        p: &ObjectParams,
    ) -> Option<MotionResult> {
        let img = image::load_from_memory(frame).ok()?.to_luma8();
        let small = image::imageops::resize(&img, self.w as u32, self.h as u32, FilterType::Triangle);
        let cur = box_blur(small.as_raw(), self.w, self.h);
        let mask = self.zone_mask(zones, ignore_zones);
        let counted = mask.iter().filter(|m| **m).count().max(1);

        let Some(bg) = self.bg.as_mut() else {
            self.bg = Some(cur.iter().map(|v| *v as f32).collect());
            return Some(MotionResult::default());
        };

        // Changed pixels against the background.
        let n = self.w * self.h;
        let mut fg = vec![false; n];
        let mut changed = 0usize;
        for i in 0..n {
            if mask[i] && (cur[i] as f32 - bg[i]).abs() > p.contrast as f32 {
                fg[i] = true;
                changed += 1;
            }
        }
        // Most of the watched area changed at once: lighting, not motion.
        if changed as f64 / counted as f64 > p.max_object.max(0.3) {
            for i in 0..n {
                bg[i] = cur[i] as f32;
            }
            self.tracks.clear();
            return Some(MotionResult { lighting_change: true, ..Default::default() });
        }
        // Background update: fast where nothing moves, very slow under
        // objects (a parked car is absorbed after a while).
        for i in 0..n {
            let a = if fg[i] { 0.005 } else { 0.05 };
            bg[i] += a * (cur[i] as f32 - bg[i]);
        }

        // Remove speckle (open), then join nearby fragments (dilate).
        let fg = dilate(&erode(&fg, self.w, self.h), self.w, self.h);
        let total = n as f64;
        let mut blobs: Vec<Blob> = components(&fg, self.w, self.h)
            .into_iter()
            .map(|(area, x0, y0, x1, y1)| Blob {
                x: x0 as f64 / self.w as f64,
                y: y0 as f64 / self.h as f64,
                w: (x1 - x0 + 1) as f64 / self.w as f64,
                h: (y1 - y0 + 1) as f64 / self.h as f64,
                area: area as f64 / total,
            })
            .filter(|b| b.area >= p.min_object && b.area <= p.max_object)
            .collect();
        blobs.sort_by(|a, b| b.area.total_cmp(&a.area));

        let triggered = self.track(&blobs, p.min_travel);
        Some(MotionResult { score: blobs.first().map(|b| b.area).unwrap_or(0.0), blobs, triggered, lighting_change: false })
    }

    /// Match blobs to tracks by nearest centroid; true when any track has
    /// travelled at least `min_travel` of the diagonal (or immediately if 0).
    fn track(&mut self, blobs: &[Blob], min_travel: f64) -> bool {
        let centers: Vec<(f64, f64)> = blobs.iter().map(|b| (b.x + b.w / 2.0, b.y + b.h / 2.0)).collect();
        let diag = (1.0f64 + 1.0).sqrt();
        let dist = |a: (f64, f64), b: (f64, f64)| ((a.0 - b.0).powi(2) + (a.1 - b.1).powi(2)).sqrt() / diag;
        let mut used = vec![false; centers.len()];
        for t in self.tracks.iter_mut() {
            let best = centers
                .iter()
                .enumerate()
                .filter(|(i, _)| !used[*i])
                .map(|(i, c)| (i, dist(t.last, *c)))
                .filter(|(_, d)| *d < 0.15)
                .min_by(|a, b| a.1.total_cmp(&b.1));
            match best {
                Some((i, _)) => {
                    used[i] = true;
                    t.last = centers[i];
                    t.age += 1;
                    t.missed = 0;
                }
                None => t.missed += 1,
            }
        }
        self.tracks.retain(|t| t.missed <= 3);
        for (i, c) in centers.iter().enumerate() {
            if !used[i] {
                self.tracks.push(Track { start: *c, last: *c, age: 1, missed: 0 });
            }
        }
        if min_travel <= 0.0 {
            return !blobs.is_empty();
        }
        self.tracks.iter().any(|t| t.missed == 0 && t.age >= 2 && dist(t.start, t.last) >= min_travel)
    }

    fn zone_mask(&mut self, zones: &[ZoneRect], ignore: &[ZoneRect]) -> Vec<bool> {
        if let Some((z, i, pz, pi, m)) = &self.mask_cache {
            if z.as_slice() == zones && i.as_slice() == ignore && *pz == self.polys.0 && *pi == self.polys.1 {
                return m.clone();
            }
        }
        let (pin, pout) = self.polys.clone();
        let (w, h) = (self.w, self.h);
        let inside = |r: &ZoneRect, x: usize, y: usize| {
            let (fx, fy) = ((x as f64 + 0.5) / w as f64, (y as f64 + 0.5) / h as f64);
            fx >= r.x && fx < r.x + r.w && fy >= r.y && fy < r.y + r.h
        };
        let mut m = Vec::with_capacity(w * h);
        for y in 0..h {
            for x in 0..w {
                let (fx, fy) = ((x as f64 + 0.5) / w as f64, (y as f64 + 0.5) / h as f64);
                let any_zone = !zones.is_empty() || !pin.is_empty();
                let inc = !any_zone || zones.iter().any(|r| inside(r, x, y)) || pin.iter().any(|p| p.contains(fx, fy));
                let out = ignore.iter().any(|r| inside(r, x, y)) || pout.iter().any(|p| p.contains(fx, fy));
                m.push(inc && !out);
            }
        }
        self.mask_cache = Some((zones.to_vec(), ignore.to_vec(), pin, pout, m.clone()));
        m
    }
}

fn box_blur(src: &[u8], w: usize, h: usize) -> Vec<u8> {
    let mut out = vec![0u8; w * h];
    for y in 0..h {
        for x in 0..w {
            let (mut sum, mut cnt) = (0u32, 0u32);
            for dy in -1i32..=1 {
                for dx in -1i32..=1 {
                    let (nx, ny) = (x as i32 + dx, y as i32 + dy);
                    if nx >= 0 && ny >= 0 && (nx as usize) < w && (ny as usize) < h {
                        sum += src[ny as usize * w + nx as usize] as u32;
                        cnt += 1;
                    }
                }
            }
            out[y * w + x] = (sum / cnt) as u8;
        }
    }
    out
}

fn morph(src: &[bool], w: usize, h: usize, all: bool) -> Vec<bool> {
    let mut out = vec![false; w * h];
    for y in 0..h {
        for x in 0..w {
            let mut acc = all;
            for dy in -1i32..=1 {
                for dx in -1i32..=1 {
                    let (nx, ny) = (x as i32 + dx, y as i32 + dy);
                    let v = nx >= 0 && ny >= 0 && (nx as usize) < w && (ny as usize) < h && src[ny as usize * w + nx as usize];
                    acc = if all { acc && v } else { acc || v };
                }
            }
            out[y * w + x] = acc;
        }
    }
    out
}

fn erode(src: &[bool], w: usize, h: usize) -> Vec<bool> {
    morph(src, w, h, true)
}

fn dilate(src: &[bool], w: usize, h: usize) -> Vec<bool> {
    morph(src, w, h, false)
}

/// 8-connected regions: (area, x0, y0, x1, y1).
fn components(fg: &[bool], w: usize, h: usize) -> Vec<(usize, usize, usize, usize, usize)> {
    let mut seen = vec![false; w * h];
    let mut out = Vec::new();
    let mut stack = Vec::new();
    for start in 0..w * h {
        if !fg[start] || seen[start] {
            continue;
        }
        seen[start] = true;
        stack.push(start);
        let (mut area, mut x0, mut y0, mut x1, mut y1) = (0, w, h, 0, 0);
        while let Some(i) = stack.pop() {
            let (x, y) = (i % w, i / w);
            area += 1;
            x0 = x0.min(x);
            y0 = y0.min(y);
            x1 = x1.max(x);
            y1 = y1.max(y);
            for dy in -1i32..=1 {
                for dx in -1i32..=1 {
                    let (nx, ny) = (x as i32 + dx, y as i32 + dy);
                    if nx >= 0 && ny >= 0 && (nx as usize) < w && (ny as usize) < h {
                        let j = ny as usize * w + nx as usize;
                        if fg[j] && !seen[j] {
                            seen[j] = true;
                            stack.push(j);
                        }
                    }
                }
            }
        }
        out.push((area, x0, y0, x1, y1));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::ZoneRect;
    use image::{ImageBuffer, Luma};

    // Two 320x180 frames that differ only in the LEFT half.
    fn half_changed_frames() -> (Vec<u8>, Vec<u8>) {
        let a: ImageBuffer<Luma<u8>, Vec<u8>> = ImageBuffer::from_pixel(320, 180, Luma([100u8]));
        let mut b = a.clone();
        for y in 0..180 {
            for x in 0..160 {
                b[(x, y)] = Luma([200u8]);
            }
        }
        let mut ba = Vec::new();
        image::DynamicImage::ImageLuma8(a).write_to(&mut std::io::Cursor::new(&mut ba), image::ImageFormat::Jpeg).unwrap();
        let mut bb = Vec::new();
        image::DynamicImage::ImageLuma8(b).write_to(&mut std::io::Cursor::new(&mut bb), image::ImageFormat::Jpeg).unwrap();
        (ba, bb)
    }

    #[test]
    fn detects_change() {
        let mut det = MotionDetector::new();
        let a: ImageBuffer<Luma<u8>, Vec<u8>> = ImageBuffer::from_pixel(320, 180, Luma([100u8]));
        let b: ImageBuffer<Luma<u8>, Vec<u8>> = ImageBuffer::from_pixel(320, 180, Luma([200u8]));
        let mut buf_a = Vec::new();
        image::DynamicImage::ImageLuma8(a)
            .write_to(&mut std::io::Cursor::new(&mut buf_a), image::ImageFormat::Jpeg)
            .unwrap();
        let mut buf_b = Vec::new();
        image::DynamicImage::ImageLuma8(b)
            .write_to(&mut std::io::Cursor::new(&mut buf_b), image::ImageFormat::Jpeg)
            .unwrap();
        let no_zones: Vec<ZoneRect> = Vec::new();
        let first = det.score(&buf_a, &no_zones, &no_zones).unwrap();
        let second = det.score(&buf_b, &no_zones, &no_zones).unwrap();
        assert_eq!(first, 0.0);
        assert!(second > 0.3);
    }

    #[test]
    fn zone_excludes_changed_side() {
        let (a, b) = half_changed_frames();
        let mut det = MotionDetector::new();
        let no_zones: Vec<ZoneRect> = Vec::new();
        det.score(&a, &no_zones, &no_zones);
        // Right half only: the change is on the left, so score must be ~0.
        let right_only: Vec<ZoneRect> = vec![ZoneRect { x: 0.5, y: 0.0, w: 0.5, h: 1.0 }];
        let s = det.score(&b, &right_only, &no_zones).unwrap();
        assert!(s < 0.02, "expected ~0, got {s}");
    }

    #[test]
    fn zone_including_changed_side_scores_high() {
        let (a, b) = half_changed_frames();
        let mut det = MotionDetector::new();
        let no_zones: Vec<ZoneRect> = Vec::new();
        det.score(&a, &no_zones, &no_zones);
        let left_only: Vec<ZoneRect> = vec![ZoneRect { x: 0.0, y: 0.0, w: 0.5, h: 1.0 }];
        let s = det.score(&b, &left_only, &no_zones).unwrap();
        assert!(s > 0.3, "expected high, got {s}");
    }

    #[test]
    fn ignore_zone_suppresses_everything() {
        let (a, b) = half_changed_frames();
        let mut det = MotionDetector::new();
        let no_zones: Vec<ZoneRect> = Vec::new();
        det.score(&a, &no_zones, &no_zones);
        let ignore_all: Vec<ZoneRect> = vec![ZoneRect { x: 0.0, y: 0.0, w: 1.0, h: 1.0 }];
        let s = det.score(&b, &no_zones, &ignore_all).unwrap();
        assert!(s < 0.02, "expected ~0, got {s}");
    }


    // ---- object detector ----

    fn scene(square_at: Option<(u32, u32)>, base: u8, noise_seed: u32) -> Vec<u8> {
        // 320x180 gray scene with mild noise and an optional bright 30x30 square.
        let mut img: ImageBuffer<Luma<u8>, Vec<u8>> = ImageBuffer::new(320, 180);
        let mut r = noise_seed.wrapping_mul(2654435761);
        for (x, y, p) in img.enumerate_pixels_mut() {
            r ^= r << 13;
            r ^= r >> 17;
            r ^= r << 5;
            let n = (r % 9) as i32 - 4;
            let mut v = base as i32 + n;
            if let Some((sx, sy)) = square_at {
                if x >= sx && x < sx + 30 && y >= sy && y < sy + 30 {
                    v = 230;
                }
            }
            *p = Luma([v.clamp(0, 255) as u8]);
        }
        let mut out = Vec::new();
        image::DynamicImage::ImageLuma8(img).write_to(&mut std::io::Cursor::new(&mut out), image::ImageFormat::Jpeg).unwrap();
        out
    }

    #[test]
    fn moving_object_triggers_noise_does_not() {
        let mut d = ObjectDetector::new();
        let p = ObjectParams::default();
        // Noise only: never triggers.
        for i in 0..10 {
            let r = d.analyze(&scene(None, 80, i), &[], &[], &p).unwrap();
            assert!(!r.triggered && r.blobs.is_empty(), "noise frame {i}: {:?}", r.blobs);
        }
        // A square walking left to right triggers within a few frames.
        let mut fired = false;
        for i in 0..8u32 {
            let r = d.analyze(&scene(Some((20 + i * 25, 70)), 80, 100 + i), &[], &[], &p).unwrap();
            fired |= r.triggered;
            if i > 0 {
                assert!(!r.blobs.is_empty(), "object missed at step {i}");
            }
        }
        assert!(fired);
    }

    #[test]
    fn lighting_change_and_stationary_flicker_do_not_trigger() {
        let mut d = ObjectDetector::new();
        let p = ObjectParams::default();
        d.analyze(&scene(None, 60, 1), &[], &[], &p);
        // Whole frame brightens (lights on / night mode switch).
        let r = d.analyze(&scene(None, 160, 2), &[], &[], &p).unwrap();
        assert!(r.lighting_change && !r.triggered);
        // Object blinking in place (no travel): never triggers.
        for i in 0..10u32 {
            let at = if i % 2 == 0 { Some((150, 70)) } else { None };
            let r = d.analyze(&scene(at, 160, 10 + i), &[], &[], &p).unwrap();
            assert!(!r.triggered, "flicker frame {i}");
        }
    }

    #[test]
    fn polygon_zone_limits_detection() {
        let p = ObjectParams::default();
        // Triangle in the lower right: the square walking along y = 70..100
        // (of 180) on the left stays outside it.
        let tri = ZonePoly { points: vec![[0.6, 1.0], [1.0, 0.3], [1.0, 1.0]] };
        assert!(tri.contains(0.95, 0.9) && !tri.contains(0.1, 0.5));
        let mut d = ObjectDetector::new();
        d.set_polygons(&[tri], &[]);
        d.analyze(&scene(None, 80, 1), &[], &[], &p);
        for i in 0..3u32 {
            let r = d.analyze(&scene(Some((20 + i * 25, 70)), 80, 90 + i), &[], &[], &p).unwrap();
            assert!(r.blobs.is_empty(), "outside the polygon at step {i}");
        }
        let mut seen = false;
        for i in 0..6u32 {
            let r = d.analyze(&scene(Some((240 + i * 10, 110)), 80, 120 + i), &[], &[], &p).unwrap();
            seen |= !r.blobs.is_empty();
        }
        assert!(seen, "object inside the polygon is seen");
    }

    #[test]
    fn ignore_zone_hides_object_and_min_size_filters() {
        let p = ObjectParams::default();
        let mut d = ObjectDetector::new();
        let ignore = vec![ZoneRect { x: 0.0, y: 0.0, w: 1.0, h: 1.0 }];
        d.analyze(&scene(None, 80, 1), &[], &ignore, &p);
        for i in 0..6u32 {
            let r = d.analyze(&scene(Some((20 + i * 25, 70)), 80, 50 + i), &[], &ignore, &p).unwrap();
            assert!(r.blobs.is_empty() && !r.triggered);
        }
        // Raise the minimum object size above the square's (30x30 of
        // 320x180 = 1.6%): nothing counts.
        let big = ObjectParams { min_object: 0.05, ..p };
        let mut d = ObjectDetector::new();
        d.analyze(&scene(None, 80, 1), &[], &[], &big);
        for i in 0..6u32 {
            let r = d.analyze(&scene(Some((20 + i * 25, 70)), 80, 70 + i), &[], &[], &big).unwrap();
            assert!(!r.triggered);
        }
    }
}
