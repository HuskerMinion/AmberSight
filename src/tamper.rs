//! Camera tamper detection, the picture maths: is the camera covered or
//! blacked out (spray paint, a cloth, tape), blurred (smeared lens, spray)
//! or moved (knocked or turned so the scene no longer matches)?
//!
//! A frame is reduced to a few small numbers and maps ([`Feat`]) and
//! compared with the camera's reference picture of the same kind (day in
//! colour, night in grey: cameras switch to black and white with their
//! infrared light). The measures ignore overall brightness (headlights,
//! sunrise, an IR switch change brightness, not edges), so only a change in
//! what the picture shows counts. [`Judge`] then wants the condition to
//! last before anyone is told, and to be gone for a while before the
//! "view restored" message.

use image::imageops::FilterType;

/// Width/height of the grey picture the numbers come from.
const GW: usize = 128;
const GH: usize = 72;
/// Edge maps are half of that.
const EW: usize = 64;
const EH: usize = 36;
/// The scene is compared in this many cells across and down.
const CELLS: usize = 4;

/// What was found wrong.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Kind {
    Covered,
    Blurred,
    Moved,
}

impl Kind {
    pub fn word(self) -> &'static str {
        match self {
            Kind::Covered => "covered",
            Kind::Blurred => "blurred",
            Kind::Moved => "moved",
        }
    }
    /// A sentence for notifications.
    pub fn sentence(self, camera: &str) -> String {
        match self {
            Kind::Covered => format!("{camera} looks covered or blacked out"),
            Kind::Blurred => format!("{camera} looks blurred or out of focus"),
            Kind::Moved => format!("{camera} looks moved: the view has changed"),
        }
    }
}

/// Day (colour) or night (grey, infrared) reference.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Light {
    Day,
    Night,
}

impl Light {
    pub fn word(self) -> &'static str {
        match self {
            Light::Day => "day",
            Light::Night => "night",
        }
    }
    pub fn from_word(w: &str) -> Option<Light> {
        match w {
            "day" => Some(Light::Day),
            "night" => Some(Light::Night),
            _ => None,
        }
    }
}

/// The numbers of one picture.
#[derive(Debug, Clone)]
pub struct Feat {
    pub light: Light,
    pub mean: f32,
    pub std: f32,
    /// Mean edge strength at the coarse scale.
    pub edge: f32,
    /// Fine detail relative to the coarse edges (falls when blurred).
    pub fine: f32,
    /// Edge map (EW x EH), slightly smoothed.
    edges: Vec<f32>,
}

/// The measures of one picture against its reference.
#[derive(Debug, Clone, Copy, PartialEq, Default, serde::Serialize)]
pub struct Verdict {
    /// 1.0 or more = the condition is met at the chosen sensitivity.
    pub covered: f32,
    pub blurred: f32,
    pub moved: f32,
}

impl Verdict {
    /// The worst condition that is met, if any.
    pub fn bad(&self) -> Option<Kind> {
        if self.covered >= 1.0 {
            Some(Kind::Covered)
        } else if self.blurred >= 1.0 {
            Some(Kind::Blurred)
        } else if self.moved >= 1.0 {
            Some(Kind::Moved)
        } else {
            None
        }
    }
    /// The biggest of the three, for showing how close it is.
    #[cfg(test)]
    pub fn worst(&self) -> f32 {
        self.covered.max(self.blurred).max(self.moved)
    }
}

/// Thresholds for a sensitivity of 1 (calm: only plain cases) to 5
/// (touchy). 3 is the default.
#[derive(Debug, Clone, Copy)]
struct Limits {
    /// Covered when the edge strength falls below this share of the reference's.
    covered: f32,
    /// Blurred when the fine detail falls below this share.
    blurred: f32,
    /// Moved when this share of the scene cells no longer matches.
    moved: f32,
}

fn limits(sensitivity: u8) -> Limits {
    let i = (sensitivity.clamp(1, 5) - 1) as usize;
    Limits {
        covered: [0.10, 0.18, 0.25, 0.35, 0.45][i],
        blurred: [0.35, 0.45, 0.55, 0.65, 0.75][i],
        moved: [0.80, 0.70, 0.60, 0.50, 0.40][i],
    }
}

/// The numbers of a decoded picture (`rgb` is width*height*3 bytes).
pub fn features_rgb(rgb: &[u8], w: u32, h: u32) -> Option<Feat> {
    if w < 16 || h < 16 || rgb.len() < (w * h * 3) as usize {
        return None;
    }
    let img = image::RgbImage::from_raw(w, h, rgb[..(w * h * 3) as usize].to_vec())?;
    let small = image::imageops::resize(&img, GW as u32, GH as u32, FilterType::Triangle);
    let mut grey = vec![0f32; GW * GH];
    let mut chroma = 0f64;
    for (i, p) in small.pixels().enumerate() {
        let [r, g, b] = p.0;
        grey[i] = 0.299 * r as f32 + 0.587 * g as f32 + 0.114 * b as f32;
        chroma += ((r as i32 - g as i32).abs() + (g as i32 - b as i32).abs()) as f64 / 2.0;
    }
    let chroma = chroma / (GW * GH) as f64;
    let light = if chroma > 5.0 { Light::Day } else { Light::Night };
    Some(features_grey(&grey, light))
}

/// The numbers of a JPEG picture.
pub fn features_jpeg(jpeg: &[u8]) -> Option<Feat> {
    let img = image::load_from_memory(jpeg).ok()?.to_rgb8();
    let (w, h) = img.dimensions();
    features_rgb(img.as_raw(), w, h)
}

/// The numbers of a grey picture (GW x GH).
pub fn features_grey(grey: &[f32], light: Light) -> Feat {
    let n = (GW * GH) as f32;
    let mean = grey.iter().sum::<f32>() / n;
    let var = grey.iter().map(|v| (v - mean) * (v - mean)).sum::<f32>() / n;
    // Fine detail: mean absolute Laplacian at full size.
    let mut lap = 0f32;
    for y in 1..GH - 1 {
        for x in 1..GW - 1 {
            let i = y * GW + x;
            lap += (4.0 * grey[i] - grey[i - 1] - grey[i + 1] - grey[i - GW] - grey[i + GW]).abs();
        }
    }
    lap /= ((GW - 2) * (GH - 2)) as f32;
    // Half size (2x2 average) and its Sobel edge strength.
    let mut half = vec![0f32; EW * EH];
    for y in 0..EH {
        for x in 0..EW {
            let i = 2 * y * GW + 2 * x;
            half[y * EW + x] = (grey[i] + grey[i + 1] + grey[i + GW] + grey[i + GW + 1]) / 4.0;
        }
    }
    let mut edges = vec![0f32; EW * EH];
    for y in 1..EH - 1 {
        for x in 1..EW - 1 {
            let p = |dx: usize, dy: usize| half[(y + dy - 1) * EW + x + dx - 1];
            let gx = p(2, 0) + 2.0 * p(2, 1) + p(2, 2) - p(0, 0) - 2.0 * p(0, 1) - p(0, 2);
            let gy = p(0, 2) + 2.0 * p(1, 2) + p(2, 2) - p(0, 0) - 2.0 * p(1, 0) - p(2, 0);
            edges[y * EW + x] = (gx * gx + gy * gy).sqrt() / 4.0;
        }
    }
    let edge = edges.iter().sum::<f32>() / ((EW - 2) * (EH - 2)) as f32;
    // Smoothed a little, so a pixel of sway does not count as a move.
    let mut smooth = edges.clone();
    for y in 1..EH - 1 {
        for x in 1..EW - 1 {
            let mut s = 0.0;
            for dy in 0..3 {
                for dx in 0..3 {
                    s += edges[(y + dy - 1) * EW + x + dx - 1];
                }
            }
            smooth[y * EW + x] = s / 9.0;
        }
    }
    Feat { light, mean, std: var.sqrt(), edge, fine: lap / (edge + 1.0), edges: smooth }
}

/// Correlation of two edge maps over one cell (None: no detail to judge by).
fn cell_ncc(a: &[f32], b: &[f32], cx: usize, cy: usize) -> Option<f32> {
    let (x0, x1) = (cx * EW / CELLS, (cx + 1) * EW / CELLS);
    let (y0, y1) = (cy * EH / CELLS, (cy + 1) * EH / CELLS);
    let (mut sa, mut sb, mut n) = (0f32, 0f32, 0f32);
    for y in y0..y1 {
        for x in x0..x1 {
            sa += a[y * EW + x];
            sb += b[y * EW + x];
            n += 1.0;
        }
    }
    let (ma, mb) = (sa / n, sb / n);
    // A cell where the reference has almost no edges says nothing.
    if ma < 0.8 {
        return None;
    }
    let (mut ab, mut aa, mut bb) = (0f32, 0f32, 0f32);
    for y in y0..y1 {
        for x in x0..x1 {
            let (da, db) = (a[y * EW + x] - ma, b[y * EW + x] - mb);
            ab += da * db;
            aa += da * da;
            bb += db * db;
        }
    }
    if aa < 1e-6 {
        return None;
    }
    if bb < 1e-6 {
        return Some(0.0);
    }
    Some(ab / (aa.sqrt() * bb.sqrt()))
}

/// How much of the scene still matches: the mean correlation over the cells
/// that have detail in the reference, and the share of cells that do not
/// match. None when the reference has no detail at all.
fn scene_match(reference: &Feat, cur: &Feat) -> Option<(f32, f32)> {
    let mut nccs = Vec::new();
    for cy in 0..CELLS {
        for cx in 0..CELLS {
            if let Some(v) = cell_ncc(&reference.edges, &cur.edges, cx, cy) {
                nccs.push(v);
            }
        }
    }
    if nccs.len() < 4 {
        return None;
    }
    let mean = nccs.iter().sum::<f32>() / nccs.len() as f32;
    let off = nccs.iter().filter(|&&v| v < 0.35).count() as f32 / nccs.len() as f32;
    Some((mean, off))
}

/// Compare a picture with a reference of the same kind.
pub fn compare(reference: &Feat, cur: &Feat, sensitivity: u8) -> Verdict {
    let l = limits(sensitivity);
    // A reference with next to no detail (a dark night picture) cannot tell.
    if !has_detail(reference) {
        return Verdict::default();
    }
    let edge_ratio = cur.edge / reference.edge;
    let mut v = Verdict { covered: ((1.0 - edge_ratio) / (1.0 - l.covered)).max(0.0), ..Verdict::default() };
    if v.covered >= 1.0 {
        return v;
    }
    let Some((mean_ncc, off)) = scene_match(reference, cur) else { return v };
    // Blur: the fine detail fades while the scene itself is still there.
    if mean_ncc >= 0.4 && reference.fine > 0.05 {
        let fine_ratio = cur.fine / reference.fine;
        v.blurred = ((1.0 - fine_ratio) / (1.0 - l.blurred)).max(0.0);
        if v.blurred >= 1.0 {
            return v;
        }
    }
    v.moved = off / l.moved;
    v
}

/// Only "covered", against a reference of the other kind of light. A lens
/// that is covered or sprayed turns the picture grey (so it counts as night)
/// even when only a day reference exists; the loss of edges does not depend
/// on the light. Stricter than [`compare`], since night pictures are softer.
pub fn compare_covered_only(reference: &Feat, cur: &Feat, sensitivity: u8) -> Verdict {
    if !has_detail(reference) {
        return Verdict::default();
    }
    let l = limits(sensitivity);
    let edge_ratio = cur.edge / reference.edge;
    Verdict { covered: ((1.0 - edge_ratio) / (1.0 - l.covered * 0.4)).max(0.0), ..Verdict::default() }
}

/// What [`Judge::update`] says.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Change {
    Nothing,
    Raised(Kind),
    Cleared,
}

/// Turns single judgements into "tampered" (after the condition has held
/// for a while) and "restored" (after it has been gone for a while).
#[derive(Debug, Clone, Default)]
pub struct Judge {
    /// The bad stretch's start, and its latest kind.
    bad_since: Option<(i64, Kind)>,
    /// A good stretch began (inside a bad stretch or an active alert).
    good_since: Option<i64>,
    /// The alert that is on: since when, and its latest kind.
    pub active: Option<(i64, Kind)>,
}

/// A bad stretch survives this long without a bad picture (a person walking
/// past, rain gusts) before it counts as over.
const GRACE_SECS: i64 = 20;
/// A raised alert needs this long of good pictures to be restored.
const RESTORE_SECS: i64 = 30;

impl Judge {
    /// One judgement: `bad` is the condition found, None = looks fine.
    /// `need` is how long a condition must last before it is raised.
    pub fn update(&mut self, now: i64, bad: Option<Kind>, need: i64) -> Change {
        match bad {
            Some(k) => {
                self.good_since = None;
                let since = self.bad_since.map_or(now, |(s, _)| s);
                self.bad_since = Some((since, k));
                if self.active.is_none() && now - since >= need {
                    self.active = Some((now, k));
                    return Change::Raised(k);
                }
                if let Some((t, _)) = self.active {
                    self.active = Some((t, k));
                }
            }
            None => {
                let g = *self.good_since.get_or_insert(now);
                if self.active.is_none() {
                    if now - g >= GRACE_SECS {
                        self.bad_since = None;
                    }
                } else if now - g >= RESTORE_SECS {
                    self.active = None;
                    self.bad_since = None;
                    self.good_since = None;
                    return Change::Cleared;
                }
            }
        }
        Change::Nothing
    }

    /// Forget everything (a new reference was set by hand).
    pub fn reset(&mut self) {
        *self = Judge::default();
    }

    /// Seconds the current bad stretch has lasted.
    pub fn bad_for(&self, now: i64) -> Option<i64> {
        self.bad_since.map(|(s, _)| now - s)
    }
}

/// Whether two pictures show the same scene (used to keep a reference from
/// being replaced by a changed scene, and to see that a scene is still).
pub fn same_scene(a: &Feat, b: &Feat) -> bool {
    scene_match(a, b).is_some_and(|(mean, off)| mean >= 0.6 && off <= 0.35)
}

/// A scene that is still: the same picture, sample after sample.
pub fn steady(samples: &[&Feat]) -> bool {
    samples.len() >= 2 && samples.windows(2).all(|w| same_scene(w[0], w[1]) && (w[0].mean - w[1].mean).abs() < 25.0)
}

/// Enough detail for a reference picture to be worth anything.
pub fn has_detail(f: &Feat) -> bool {
    f.edge >= 1.5 && f.std >= 4.0
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A deterministic pseudo-random sequence.
    struct Lcg(u64);
    impl Lcg {
        fn next(&mut self) -> f32 {
            self.0 = self.0.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
            ((self.0 >> 33) as f32) / (1u64 << 31) as f32
        }
    }

    const W: usize = 320;
    const H: usize = 180;

    /// A scene of rectangles, a gradient and some fine texture, as grey.
    fn scene(seed: u64) -> Vec<f32> {
        let mut r = Lcg(seed);
        let mut g = vec![0f32; W * H];
        for y in 0..H {
            for x in 0..W {
                g[y * W + x] = 60.0 + 80.0 * (y as f32 / H as f32);
            }
        }
        for _ in 0..40 {
            let (x0, y0) = ((r.next() * W as f32) as usize, (r.next() * H as f32) as usize);
            let (w, h) = (10 + (r.next() * 50.0) as usize, 8 + (r.next() * 40.0) as usize);
            let val = r.next() * 255.0;
            for y in y0..(y0 + h).min(H) {
                for x in x0..(x0 + w).min(W) {
                    g[y * W + x] = val;
                }
            }
        }
        // Fine texture: a checker of small steps (foliage, gravel).
        for y in 0..H {
            for x in 0..W {
                if (x / 2 + y / 2) % 2 == 0 {
                    g[y * W + x] += 12.0;
                }
            }
        }
        g
    }

    fn noisy(g: &[f32], sigma: f32, seed: u64) -> Vec<f32> {
        let mut r = Lcg(seed);
        g.iter().map(|v| (v + (r.next() - 0.5) * 2.0 * sigma).clamp(0.0, 255.0)).collect()
    }

    fn gain(g: &[f32], k: f32, add: f32) -> Vec<f32> {
        g.iter().map(|v| (v * k + add).clamp(0.0, 255.0)).collect()
    }

    fn blur(g: &[f32], radius: usize) -> Vec<f32> {
        let mut out = g.to_vec();
        for _ in 0..radius {
            let src = out.clone();
            for y in 1..H - 1 {
                for x in 1..W - 1 {
                    let mut s = 0.0;
                    for dy in 0..3 {
                        for dx in 0..3 {
                            s += src[(y + dy - 1) * W + x + dx - 1];
                        }
                    }
                    out[y * W + x] = s / 9.0;
                }
            }
        }
        out
    }

    fn shift(g: &[f32], dx: usize, dy: usize) -> Vec<f32> {
        let mut out = vec![0f32; W * H];
        for y in 0..H {
            for x in 0..W {
                out[y * W + x] = g[((y + dy) % H) * W + (x + dx) % W];
            }
        }
        out
    }

    fn rgb(g: &[f32], colour: bool) -> Vec<u8> {
        let mut v = Vec::with_capacity(W * H * 3);
        for &p in g {
            let p = p.clamp(0.0, 255.0);
            if colour {
                v.extend_from_slice(&[p as u8, (p * 0.8) as u8, (p * 0.6) as u8]);
            } else {
                v.extend_from_slice(&[p as u8, p as u8, p as u8]);
            }
        }
        v
    }

    fn feat(g: &[f32], colour: bool) -> Feat {
        features_rgb(&rgb(g, colour), W as u32, H as u32).unwrap()
    }

    fn jpeg(g: &[f32], colour: bool) -> Vec<u8> {
        let img = image::RgbImage::from_raw(W as u32, H as u32, rgb(g, colour)).unwrap();
        let mut out = Vec::new();
        image::codecs::jpeg::JpegEncoder::new_with_quality(&mut out, 80).encode_image(&img).unwrap();
        out
    }

    #[test]
    fn works_on_jpeg_pictures_like_the_cameras_send() {
        let g = scene(11);
        let r = features_jpeg(&jpeg(&noisy(&g, 2.0, 1), true)).unwrap();
        let same = features_jpeg(&jpeg(&noisy(&g, 2.0, 2), true)).unwrap();
        assert_eq!(compare(&r, &same, 3).bad(), None);
        let black = features_jpeg(&jpeg(&vec![2.0; W * H], true)).unwrap();
        assert_eq!(compare(&r, &black, 3).bad(), Some(Kind::Covered));
        let blurred = features_jpeg(&jpeg(&blur(&g, 8), true)).unwrap();
        assert_eq!(compare(&r, &blurred, 3).bad(), Some(Kind::Blurred));
        let moved = features_jpeg(&jpeg(&shift(&g, 30, 12), true)).unwrap();
        assert_eq!(compare(&r, &moved, 3).bad(), Some(Kind::Moved));
    }

    #[test]
    fn colour_and_grey_pictures_are_day_and_night() {
        let g = scene(1);
        assert_eq!(feat(&g, true).light, Light::Day);
        assert_eq!(feat(&g, false).light, Light::Night);
    }

    #[test]
    fn a_black_frame_is_covered() {
        let g = scene(1);
        let r = feat(&g, true);
        let black = noisy(&vec![3.0; W * H], 1.5, 9);
        let v = compare(&r, &feat(&black, true), 3);
        assert_eq!(v.bad(), Some(Kind::Covered), "{v:?}");
        // Grey paint over the lens: no detail left either.
        let grey = noisy(&vec![120.0; W * H], 1.0, 10);
        assert_eq!(compare(&r, &feat(&grey, true), 3).bad(), Some(Kind::Covered));
    }

    #[test]
    fn a_covered_lens_is_found_with_only_a_day_reference() {
        let g = scene(1);
        let day = feat(&g, true);
        // Grey paint: low colour, so it counts as night, which has no reference.
        let paint = feat(&noisy(&vec![120.0; W * H], 1.0, 10), false);
        assert_eq!(paint.light, Light::Night);
        assert_eq!(compare_covered_only(&day, &paint, 3).bad(), Some(Kind::Covered));
        // The same scene in infrared is not.
        let ir = feat(&gain(&noisy(&g, 2.0, 3), 0.6, 0.0), false);
        assert_eq!(compare_covered_only(&day, &ir, 3).bad(), None);
    }

    #[test]
    fn a_blurred_frame_is_blurred_not_covered() {
        let g = scene(2);
        let r = feat(&g, true);
        let v = compare(&r, &feat(&blur(&g, 8), true), 3);
        assert_eq!(v.bad(), Some(Kind::Blurred), "{v:?}");
    }

    #[test]
    fn a_shifted_frame_is_moved() {
        let g = scene(3);
        let r = feat(&g, true);
        let v = compare(&r, &feat(&shift(&g, 30, 12), true), 3);
        assert_eq!(v.bad(), Some(Kind::Moved), "{v:?}");
    }

    #[test]
    fn the_same_scene_with_noise_is_fine() {
        let g = scene(4);
        let r = feat(&noisy(&g, 3.0, 1), true);
        for seed in 2..8 {
            let v = compare(&r, &feat(&noisy(&g, 3.0, seed), true), 3);
            assert_eq!(v.bad(), None, "{v:?}");
            assert!(v.worst() < 0.6, "{v:?}");
        }
    }

    #[test]
    fn brightness_changes_are_not_tampering() {
        // Headlights, sunrise, a cloud, auto exposure.
        let g = scene(5);
        let r = feat(&g, true);
        for (k, add) in [(1.4, 20.0), (0.6, -10.0), (0.4, 0.0), (1.2, 40.0)] {
            let v = compare(&r, &feat(&gain(&g, k, add), true), 3);
            assert_eq!(v.bad(), None, "gain {k} {add}: {v:?}");
        }
    }

    #[test]
    fn switching_to_infrared_is_judged_against_the_night_reference() {
        let g = scene(6);
        let day = feat(&g, true);
        // Night: grey, darker, noisier. It is a Night picture and so is
        // compared with the night reference, not the day one.
        let night = feat(&noisy(&gain(&g, 0.7, 0.0), 4.0, 3), false);
        assert_eq!(night.light, Light::Night);
        assert_ne!(night.light, day.light);
        let night_ref = feat(&noisy(&gain(&g, 0.7, 0.0), 4.0, 4), false);
        assert_eq!(compare(&night_ref, &night, 3).bad(), None);
        // Even compared across kinds the same scene is not tampering.
        let across = compare(&day, &night, 3);
        assert_eq!(across.bad(), None, "{across:?}");
    }

    #[test]
    fn a_person_close_to_the_lens_is_not_enough_alone() {
        // A big block over part of the picture: some cells change, the rest
        // of the scene still matches.
        let g = scene(7);
        let r = feat(&g, true);
        let mut p = g.clone();
        for y in 30..150 {
            for x in 20..120 {
                p[y * W + x] = 30.0;
            }
        }
        let v = compare(&r, &feat(&p, true), 3);
        assert_eq!(v.bad(), None, "{v:?}");
    }

    #[test]
    fn sensitivity_changes_what_counts() {
        let g = scene(8);
        let r = feat(&g, true);
        let slightly = feat(&blur(&g, 3), true);
        let calm = compare(&r, &slightly, 1).blurred;
        let touchy = compare(&r, &slightly, 5).blurred;
        assert!(touchy > calm, "{calm} {touchy}");
    }

    #[test]
    fn a_featureless_reference_judges_nothing() {
        let flat = feat(&noisy(&vec![10.0; W * H], 0.5, 1), false);
        let g = feat(&scene(1), false);
        assert_eq!(compare(&flat, &g, 5), Verdict::default());
        assert!(!has_detail(&flat));
        assert!(has_detail(&g));
    }

    #[test]
    fn steady_scenes_are_steady() {
        let g = scene(9);
        let a = feat(&noisy(&g, 2.0, 1), true);
        let b = feat(&noisy(&g, 2.0, 2), true);
        let c = feat(&shift(&g, 30, 12), true);
        assert!(steady(&[&a, &b]));
        assert!(!steady(&[&a, &c]));
        assert!(same_scene(&a, &b));
    }

    #[test]
    fn the_condition_must_last() {
        let mut j = Judge::default();
        let k = Some(Kind::Covered);
        assert_eq!(j.update(0, k, 60), Change::Nothing);
        assert_eq!(j.update(30, k, 60), Change::Nothing);
        assert_eq!(j.update(59, k, 60), Change::Nothing);
        assert_eq!(j.update(60, k, 60), Change::Raised(Kind::Covered));
        // Once only.
        assert_eq!(j.update(90, k, 60), Change::Nothing);
        assert_eq!(j.update(300, k, 60), Change::Nothing);
    }

    #[test]
    fn a_short_blip_or_a_walker_never_raises() {
        let mut j = Judge::default();
        let k = Some(Kind::Moved);
        // Bad for 40 s, good for 10 s (a gap shorter than the grace keeps
        // the stretch going), then bad again: it adds up.
        for t in (0..40).step_by(5) {
            j.update(t, k, 60);
        }
        for t in (40..50).step_by(5) {
            j.update(t, None, 60);
        }
        assert_eq!(j.update(60, k, 60), Change::Raised(Kind::Moved));
        // A real gap resets the count.
        let mut j = Judge::default();
        for t in (0..40).step_by(5) {
            j.update(t, k, 60);
        }
        for t in (40..70).step_by(5) {
            j.update(t, None, 60);
        }
        assert_eq!(j.update(70, k, 60), Change::Nothing);
        assert_eq!(j.update(125, k, 60), Change::Nothing);
        assert_eq!(j.update(130, k, 60), Change::Raised(Kind::Moved));
    }

    #[test]
    fn restored_after_a_good_stretch_once() {
        let mut j = Judge::default();
        let k = Some(Kind::Blurred);
        j.update(0, k, 10);
        assert_eq!(j.update(10, k, 10), Change::Raised(Kind::Blurred));
        // One good picture between bad ones does not restore it.
        assert_eq!(j.update(15, None, 10), Change::Nothing);
        assert_eq!(j.update(20, k, 10), Change::Nothing);
        assert_eq!(j.update(25, None, 10), Change::Nothing);
        assert_eq!(j.update(54, None, 10), Change::Nothing);
        assert_eq!(j.update(55, None, 10), Change::Cleared);
        assert_eq!(j.update(60, None, 10), Change::Nothing);
        assert!(j.active.is_none());
        // And a new condition can be raised again.
        j.update(100, k, 10);
        assert_eq!(j.update(110, k, 10), Change::Raised(Kind::Blurred));
    }
}
