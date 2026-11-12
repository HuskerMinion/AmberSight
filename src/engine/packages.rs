// Intel Macs have no CLIP judge (no ONNX runtime build there), so its helpers go unused.
#![cfg_attr(all(target_os = "macos", target_arch = "x86_64"), allow(dead_code))]
//! Package detection on doorbell-style cameras (camera setting `packages`).
//!
//! After someone has been at the door (an event in which AI saw a person),
//! the picture from before the visit is compared with one taken a little
//! after it. Places that changed are cropped and judged by the smart-search
//! picture model (CLIP): "a cardboard box / parcel / mailer" against "a
//! doormat / shadow / plant / ...". Something new that looks like a package
//! is a delivery: an event "package" (alerts, Events, timeline, MQTT) and
//! the camera's Package sensor turns on. After a later visit, a package that
//! no longer looks like one where it lay is "package picked up".
//!
//! Where the porch is hard to see, the visit itself also tells: the AI's
//! pictures of the person are judged "a person carrying a box" against "a
//! person" (empty hands, a phone, shopping bags...). A carried package is
//! announced as a delivery too (without a place, so no "picked up" later).
//!
//! Only visits that come toward the door count as deliveries: the person
//! grows in the picture as they walk up to a doorbell (someone leaving the
//! house shrinks). The person's pictures are kept only while they come
//! closer. A pickup is looked for after any visit.
//!
//! The pictures are the camera's analysis pictures (~640 px wide). Nothing
//! runs while the camera sees motion; a check waits for a quiet moment.

use super::*;
use image::{GrayImage, RgbImage};
use std::collections::HashMap;
use std::sync::Mutex;

/// The picture from before a visit is at most this old.
const QUIET_REFRESH_SECS: i64 = 30;
/// Look this long after the visit ended (the person has walked away).
const CHECK_AFTER_SECS: i64 = 40;
/// Give up on a check when the camera never goes quiet this long.
const CHECK_GIVE_UP_SECS: i64 = 600;
/// How sure the picture model must be that the new thing is a package...
const NEW_MIN: f32 = 0.55;
/// ...and how unsure that the same place held one before the visit.
const BEFORE_MAX: f32 = 0.4;
/// A package is gone when its place looks like one less than this.
const GONE_MAX: f32 = 0.3;
/// At most this many changed places judged per check.
const MAX_REGIONS: usize = 4;
/// How sure the model must be that the person carried a package.
const CARRY_MIN: f32 = 0.6;
/// Pictures of the person kept per visit (the largest).
const CARRY_SAMPLES: usize = 4;

/// One package lying in view (box in 0..1 of the picture).
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize, PartialEq)]
pub struct Package {
    pub x: f32,
    pub y: f32,
    pub w: f32,
    pub h: f32,
    /// When it was found (unix seconds).
    pub since: i64,
    #[serde(skip)]
    pic: Vec<u8>,
}

#[derive(Default)]
struct Cam {
    /// The latest picture taken while the camera was quiet, and when.
    quiet: Option<(Vec<u8>, i64)>,
    /// A person was seen in the event under way.
    person: bool,
    /// The largest pictures of the person in this visit: (picture, box).
    samples: Vec<(Vec<u8>, [f32; 4])>,
    /// The person's height in each AI look of this visit, in order.
    heights: Vec<f32>,
    /// They got clearly smaller again: walking away; no more pictures kept.
    leaving: bool,
    in_event: bool,
    /// Check due at, give up after.
    check: Option<(i64, i64)>,
    busy: bool,
    present: Vec<Package>,
}

static CAMS: Mutex<Option<HashMap<String, Cam>>> = Mutex::new(None);
const KV_SCOPE: &str = "server";
const KV_KEY: &str = "packages";

fn with_cams<R>(engine: &Engine, f: impl FnOnce(&mut HashMap<String, Cam>) -> R) -> R {
    let mut g = CAMS.lock().unwrap();
    let cams = g.get_or_insert_with(|| {
        // Packages still lying there when the server last ran.
        let saved: HashMap<String, Vec<Package>> =
            engine.db.ui_get(KV_SCOPE, KV_KEY).ok().flatten().and_then(|s| serde_json::from_str(&s).ok()).unwrap_or_default();
        saved.into_iter().map(|(k, present)| (k, Cam { present, ..Default::default() })).collect()
    });
    f(cams)
}

fn save(engine: &Engine, cams: &HashMap<String, Cam>) {
    let m: HashMap<&String, &Vec<Package>> = cams.iter().filter(|(_, c)| !c.present.is_empty()).map(|(k, c)| (k, &c.present)).collect();
    if let Err(e) = engine.db.ui_put(KV_SCOPE, KV_KEY, &serde_json::to_string(&m).unwrap_or_else(|_| "{}".into())) {
        warn!("packages: could not save: {e:#}");
    }
}

/// Packages lying in view of a camera now (for the MQTT sensor).
pub fn present(engine: &Engine, camera: &str) -> usize {
    with_cams(engine, |m| m.get(camera).map_or(0, |c| c.present.len()))
}

/// Called for every analysis picture of a camera with package detection on.
/// `person_now`: an AI check just saw a person.
/// `person`: an AI check just saw a person: (that picture, the largest
/// person's box).
pub(super) fn on_frame(engine: &Arc<Engine>, handle: &Arc<CameraHandle>, camera: &str, event_active: bool, person: Option<(&[u8], [f32; 4])>) {
    let now = Utc::now().timestamp();
    let run = with_cams(engine, |m| {
        let c = m.entry(camera.to_string()).or_default();
        if let Some((pic, b)) = person {
            if !c.person {
                c.samples.clear();
                c.heights.clear();
                c.leaving = false;
            }
            c.person = true;
            let tallest = c.heights.iter().copied().fold(0.0f32, f32::max);
            c.heights.push(b[3]);
            if b[3] < tallest * 0.75 {
                c.leaving = true;
            }
            let size = |b: &[f32; 4]| b[2] * b[3];
            if c.leaving {
                // Walking away: what they carry now is not a delivery.
            } else if c.samples.len() < CARRY_SAMPLES {
                c.samples.push((pic.to_vec(), b));
            } else if let Some(i) = (0..c.samples.len()).min_by(|&i, &j| size(&c.samples[i].1).total_cmp(&size(&c.samples[j].1))) {
                if size(&b) > size(&c.samples[i].1) {
                    c.samples[i] = (pic.to_vec(), b);
                }
            }
        }
        if event_active {
            c.in_event = true;
            return None;
        }
        if c.in_event {
            c.in_event = false;
            if std::mem::take(&mut c.person) {
                c.check = Some((now + CHECK_AFTER_SECS, now + CHECK_GIVE_UP_SECS));
            }
        }
        if let Some((at, give_up)) = c.check {
            if now > give_up {
                c.check = None;
            } else if now >= at && !c.busy {
                c.check = None;
                let samples = std::mem::take(&mut c.samples);
                let toward = toward_door(&std::mem::take(&mut c.heights));
                if let Some((before, _)) = c.quiet.clone() {
                    let after = handle.state.read().unwrap().latest_frame.clone();
                    if !after.is_empty() {
                        c.busy = true;
                        return Some((before, after, c.present.clone(), samples, toward));
                    }
                }
            }
            // Keep the picture from before the visit until it is checked.
            return None;
        }
        if !c.busy && c.quiet.as_ref().is_none_or(|(_, t)| now - t >= QUIET_REFRESH_SECS) {
            let f = handle.state.read().unwrap().latest_frame.clone();
            if !f.is_empty() {
                c.quiet = Some((f, now));
            }
        }
        None
    });
    let Some((before, after, present, samples, toward)) = run else { return };
    let (eng, handle, cam) = (engine.clone(), handle.clone(), camera.to_string());
    let spawned = std::thread::Builder::new().name("packages".into()).spawn(move || {
        let (engine, camera) = (eng, cam);
        // Passing by or leaving: no delivery (a pickup is still looked for).
        if !toward {
            info!("camera '{camera}': visit did not come toward the door: no delivery check");
        }
        let people: Vec<[f32; 4]> = if toward { samples.iter().map(|s| s.1).collect() } else { Vec::new() };
        let r = check(&engine, &camera, &before, &after, &present, &people);
        let (found, gone) = match r {
            Ok(x) => x,
            Err(e) => {
                warn!("camera '{camera}': package check: {e:#}");
                (Vec::new(), Vec::new())
            }
        };
        // Nothing new seen at the door: did the person carry a package there?
        let carried = toward && found.is_empty() && carried(&engine, &camera, &samples).is_some_and(|c| c >= CARRY_MIN);
        let now = Utc::now().timestamp();
        with_cams(&engine, |m| {
            let c = m.entry(camera.clone()).or_default();
            c.busy = false;
            // The new "before" picture: what the camera shows now (with the package).
            c.quiet = Some((after.clone(), now));
            if !found.is_empty() || !gone.is_empty() {
                c.present.retain(|p| !gone.contains(p));
                c.present.extend(found.iter().cloned());
                save(&engine, m);
            }
        });
        if !found.is_empty() {
            announce(&engine, &handle, &camera, "package", &after, now);
        } else if carried {
            let pic = samples.iter().max_by(|a, b| (a.1[2] * a.1[3]).total_cmp(&(b.1[2] * b.1[3]))).map(|s| s.0.clone()).unwrap_or(after.clone());
            announce(&engine, &handle, &camera, "package", &pic, now);
        }
        if !gone.is_empty() {
            announce(&engine, &handle, &camera, "package picked up", &after, now);
        }
    });
    if spawned.is_err() {
        with_cams(engine, |m| {
            if let Some(c) = m.get_mut(camera) {
                c.busy = false;
            }
        });
    }
}

/// An event for a delivery or a pickup: Events, timeline, alerts, MQTT.
fn announce(engine: &Engine, handle: &CameraHandle, camera: &str, label: &str, jpeg: &[u8], at: i64) {
    let id = format!("{}-{}-{}", slug(camera), at, if label == "package" { "pkg" } else { "pkgoff" });
    let thumb = std::fs::write(engine.data_dir.join("thumbs").join(format!("{id}.jpg")), jpeg).is_ok();
    info!("camera '{camera}': {label}");
    engine.record_event(
        Event {
            id: id.clone(),
            camera_id: handle.id,
            camera: camera.to_string(),
            start: at,
            end: at,
            score: 1.0,
            clip: Some(format!("/api/clips/{id}")),
            thumb: thumb.then_some(id),
            objects: vec![label.to_string()],
            protect: false,
        },
        true,
    );
}

/// Events made by package detection skip the alert interval (a delivery
/// right after the "person at the door" alert must still be told).
pub fn is_package_event(ev: &Event) -> bool {
    ev.objects.first().is_some_and(|o| o == "package" || o == "package picked up")
}

/// (new packages, packages gone) between two pictures.
/// The visit came toward the door: the person (their height in each look,
/// in order) was clearly taller at some later look than when first seen.
fn toward_door(heights: &[f32]) -> bool {
    let Some(&first) = heights.first() else { return false };
    heights.iter().skip(1).any(|&h| h >= first * 1.25)
}

/// A changed place counts only near where the visitor was: a package is put
/// down where the person stood (the street beyond, where cars come and go,
/// is left out). Person boxes are widened by half their size each way.
fn near_visitor(r: &[f32; 4], people: &[[f32; 4]]) -> bool {
    people.iter().any(|p| {
        let wide = [p[0] - p[2] * 0.5, p[1] - p[3] * 0.25, p[2] * 2.0, p[3] * 1.5];
        overlap(r, &wide) > 0.5
    })
}

fn check(engine: &Arc<Engine>, camera: &str, before: &[u8], after: &[u8], present: &[Package], people: &[[f32; 4]]) -> anyhow::Result<(Vec<Package>, Vec<Package>)> {
    let Some(judge) = judge(engine) else { return Ok((Vec::new(), Vec::new())) };
    let b = image::load_from_memory(before)?.to_rgb8();
    let a = image::load_from_memory(after)?.to_rgb8();
    if a.dimensions() != b.dimensions() {
        return Ok((Vec::new(), Vec::new()));
    }
    // People, vehicles and animals still in view are not packages.
    let movers: Vec<[f32; 4]> = engine
        .ai_detect(after, 0.4)
        .unwrap_or_default()
        .into_iter()
        .filter(|d| !matches!(d.label.as_str(), "suitcase" | "backpack" | "handbag"))
        .map(|d| [d.x, d.y, d.w, d.h])
        .collect();
    let now = Utc::now().timestamp();
    let mut found = Vec::new();
    match changed_regions(&small(&b), &small(&a)) {
        None => tracing::debug!("camera '{camera}': package check: the whole picture changed (light), skipped"),
        Some(regions) => {
            for r in regions.into_iter().filter(|r| near_visitor(r, people) && !movers.iter().any(|m| overlap(r, m) > 0.3) && !present.iter().any(|p| overlap(r, &[p.x, p.y, p.w, p.h]) > 0.3)).take(MAX_REGIONS) {
                let pa = judge.package(&a, &r)?;
                if pa < NEW_MIN {
                    tracing::debug!("camera '{camera}': changed place {r:?}: package {pa:.2}");
                    continue;
                }
                let pb = judge.package(&b, &r)?;
                info!("camera '{camera}': changed place {r:?}: package {pa:.2} now, {pb:.2} before");
                if pb < BEFORE_MAX {
                    found.push(Package { x: r[0], y: r[1], w: r[2], h: r[3], since: now, pic: after.to_vec() });
                }
            }
        }
    }
    let mut gone = Vec::new();
    for p in present {
        let r = [p.x, p.y, p.w, p.h];
        if movers.iter().any(|m| overlap(&r, m) > 0.5) {
            continue; // someone standing over it: cannot tell
        }
        let pa = judge.package(&a, &r)?;
        if pa < GONE_MAX {
            info!("camera '{camera}': package place {r:?}: package {pa:.2} now: gone");
            gone.push(p.clone());
        }
    }
    Ok((found, gone))
}

/// The best chance that the person in these pictures carried a package.
fn carried(engine: &Arc<Engine>, camera: &str, samples: &[(Vec<u8>, [f32; 4])]) -> Option<f32> {
    let judge = judge(engine)?;
    let mut best = 0.0f32;
    for (pic, b) in samples {
        let Ok(img) = image::load_from_memory(pic).map(|i| i.to_rgb8()) else { continue };
        match judge.carrying(&img, b) {
            Ok(c) => best = best.max(c),
            Err(e) => warn!("camera '{camera}': package check: {e:#}"),
        }
    }
    info!("camera '{camera}': visit: carrying a package {best:.2}");
    Some(best)
}

/// The share of `a` covered by `b`.
fn overlap(a: &[f32; 4], b: &[f32; 4]) -> f32 {
    let w = (a[0] + a[2]).min(b[0] + b[2]) - a[0].max(b[0]);
    let h = (a[1] + a[3]).min(b[1] + b[3]) - a[1].max(b[1]);
    if w <= 0.0 || h <= 0.0 || a[2] * a[3] <= 0.0 {
        return 0.0;
    }
    w * h / (a[2] * a[3])
}

const SMALL_W: u32 = 160;

fn small(img: &RgbImage) -> GrayImage {
    let g = image::DynamicImage::ImageRgb8(img.clone()).to_luma8();
    let h = (SMALL_W as f32 * img.height() as f32 / img.width().max(1) as f32).round().max(1.0) as u32;
    let r = image::imageops::resize(&g, SMALL_W, h, image::imageops::FilterType::Triangle);
    image::imageops::blur(&r, 1.2)
}

/// Places that look different between two small grey pictures (boxes in
/// 0..1), largest first; None when most of the picture changed (light
/// switched, camera moved): nothing can be told from it.
fn changed_regions(a: &GrayImage, b: &GrayImage) -> Option<Vec<[f32; 4]>> {
    if a.dimensions() != b.dimensions() {
        return None;
    }
    let (w, h) = a.dimensions();
    let n = (w * h) as usize;
    let mean = |g: &GrayImage| g.pixels().map(|p| p.0[0] as f32).sum::<f32>() / n as f32;
    // Overall brighter or darker (clouds, dusk) is not a change.
    let shift = mean(b) - mean(a);
    let mut mask: Vec<bool> = a.pixels().zip(b.pixels()).map(|(p, q)| (q.0[0] as f32 - p.0[0] as f32 - shift).abs() > 28.0).collect();
    let changed = mask.iter().filter(|m| **m).count();
    if changed as f32 > 0.3 * n as f32 {
        return None;
    }
    // Join nearby pixels (one step).
    let src = mask.clone();
    for y in 0..h as i32 {
        for x in 0..w as i32 {
            if src[(y as u32 * w + x as u32) as usize] {
                continue;
            }
            let near = (-1..=1).any(|dy| (-1..=1).any(|dx| {
                let (nx, ny) = (x + dx, y + dy);
                nx >= 0 && ny >= 0 && nx < w as i32 && ny < h as i32 && src[(ny as u32 * w + nx as u32) as usize]
            }));
            if near {
                mask[(y as u32 * w + x as u32) as usize] = true;
            }
        }
    }
    let mut seen = vec![false; n];
    let mut out: Vec<(usize, [f32; 4])> = Vec::new();
    for start in 0..n {
        if !mask[start] || seen[start] {
            continue;
        }
        let (mut x0, mut y0, mut x1, mut y1, mut count) = (u32::MAX, u32::MAX, 0u32, 0u32, 0usize);
        let mut stack = vec![start];
        seen[start] = true;
        while let Some(i) = stack.pop() {
            let (x, y) = ((i as u32) % w, (i as u32) / w);
            count += 1;
            x0 = x0.min(x);
            y0 = y0.min(y);
            x1 = x1.max(x);
            y1 = y1.max(y);
            let mut push = |j: usize| {
                if mask[j] && !seen[j] {
                    seen[j] = true;
                    stack.push(j);
                }
            };
            if x > 0 { push(i - 1); }
            if x + 1 < w { push(i + 1); }
            if y > 0 { push(i - w as usize); }
            if y + 1 < h { push(i + w as usize); }
        }
        let area = ((x1 - x0 + 1) * (y1 - y0 + 1)) as f32;
        // A package is more than a speck and less than a quarter of the view.
        if count as f32 >= 0.002 * n as f32 && area <= 0.25 * n as f32 {
            out.push((count, [x0 as f32 / w as f32, y0 as f32 / h as f32, (x1 - x0 + 1) as f32 / w as f32, (y1 - y0 + 1) as f32 / h as f32]));
        }
    }
    out.sort_by(|a, b| b.0.cmp(&a.0));
    Some(out.into_iter().map(|(_, r)| r).collect())
}

/// Words the picture model compares a place with.
const PACKAGE_WORDS: &[&str] = &[
    "a cardboard box",
    "a package delivered on a porch",
    "a parcel on the doorstep",
    "a padded envelope",
    "a plastic delivery mailer bag",
    "a stack of delivery boxes",
];
const OTHER_WORDS: &[&str] = &[
    "an empty porch floor",
    "a doormat",
    "a shadow on the ground",
    "a patch of sunlight",
    "a potted plant",
    "a pair of shoes",
    "a chair",
    "a person",
    "a car",
    "dry leaves on the ground",
    "a wet floor",
    "a front door",
    "a lawn",
    "a cat",
    "a dog",
    "a flag",
];

const CARRY_WORDS: &[&str] = &[
    "a person carrying a cardboard box",
    "a delivery driver holding a package",
    "a person holding a parcel",
    "a person carrying a stack of boxes",
    "a person holding a padded envelope",
];
const NOT_CARRY_WORDS: &[&str] = &[
    "a person",
    "a person walking",
    "a person with empty hands",
    "a person holding a phone",
    "a person carrying a backpack",
    "a person carrying shopping bags",
    "a person walking a dog",
    "a person holding an umbrella",
    "a person carrying a child",
    "a person holding a cup of coffee",
];

/// Turn a place's picture fingerprint into the chance it is a package.
fn package_chance(pic: &[f32], package: &[Vec<f32>], other: &[Vec<f32>]) -> f32 {
    let dot = |t: &Vec<f32>| pic.iter().zip(t).map(|(a, b)| a * b).sum::<f32>() * 100.0;
    let all: Vec<f32> = package.iter().chain(other).map(dot).collect();
    let max = all.iter().copied().fold(f32::MIN, f32::max);
    let exp: Vec<f32> = all.iter().map(|v| (v - max).exp()).collect();
    let total: f32 = exp.iter().sum();
    exp[..package.len()].iter().sum::<f32>() / total
}

#[cfg(not(all(target_os = "macos", target_arch = "x86_64")))]
pub(crate) struct Judge {
    models: Arc<crate::smart::SmartModels>,
    package: Vec<Vec<f32>>,
    other: Vec<Vec<f32>>,
    carry: Vec<Vec<f32>>,
    not_carry: Vec<Vec<f32>>,
}

#[cfg(not(all(target_os = "macos", target_arch = "x86_64")))]
impl Judge {
    pub(crate) fn load(models: Arc<crate::smart::SmartModels>) -> anyhow::Result<Self> {
        let words = |list: &[&str]| list.iter().map(|w| models.embed_query(w)).collect::<anyhow::Result<Vec<_>>>();
        Ok(Judge { package: words(PACKAGE_WORDS)?, other: words(OTHER_WORDS)?, carry: words(CARRY_WORDS)?, not_carry: words(NOT_CARRY_WORDS)?, models })
    }
    /// The chance that the place `r` (0..1) of the picture holds a package.
    pub(crate) fn package(&self, img: &RgbImage, r: &[f32; 4]) -> anyhow::Result<f32> {
        let Some(crop) = crop(img, r) else { return Ok(0.0) };
        let e = self.models.embed_image(&crop)?;
        Ok(package_chance(&e, &self.package, &self.other))
    }
    /// The chance that the person in box `b` carries a package.
    pub(crate) fn carrying(&self, img: &RgbImage, b: &[f32; 4]) -> anyhow::Result<f32> {
        let Some(crop) = crate::smart::crop_box(img, b[0], b[1], b[2], b[3]) else { return Ok(0.0) };
        let e = self.models.embed_image(&crop)?;
        Ok(package_chance(&e, &self.carry, &self.not_carry))
    }
}

/// The place with room around it (the model needs some context), at least
/// 64 px each way.
fn crop(img: &RgbImage, r: &[f32; 4]) -> Option<RgbImage> {
    let (iw, ih) = (img.width() as f32, img.height() as f32);
    let (cx, cy) = ((r[0] + r[2] / 2.0) * iw, (r[1] + r[3] / 2.0) * ih);
    let w = (r[2] * iw * 1.6).max(64.0).min(iw);
    let h = (r[3] * ih * 1.6).max(64.0).min(ih);
    let x0 = (cx - w / 2.0).clamp(0.0, iw - w) as u32;
    let y0 = (cy - h / 2.0).clamp(0.0, ih - h) as u32;
    if w < 16.0 || h < 16.0 {
        return None;
    }
    Some(image::imageops::crop_imm(img, x0, y0, w as u32, h as u32).to_image())
}

#[cfg(not(all(target_os = "macos", target_arch = "x86_64")))]
static JUDGE: Mutex<Option<Arc<Judge>>> = Mutex::new(None);

/// The package judge, loading the smart-search picture model when needed
/// (downloading it first: then this check is skipped).
#[cfg(not(all(target_os = "macos", target_arch = "x86_64")))]
fn judge(engine: &Arc<Engine>) -> Option<Arc<Judge>> {
    let mut g = JUDGE.lock().unwrap();
    if let Some(j) = g.as_ref() {
        return Some(j.clone());
    }
    let models = match engine.smart_models() {
        Some(m) => m,
        None => {
            let dir = engine.data_dir.join("models");
            if !crate::smart::models_present(&dir) {
                engine.start_smart_download();
                return None;
            }
            match crate::smart::SmartModels::load(&dir, 2) {
                Ok(m) => Arc::new(m),
                Err(e) => {
                    warn!("packages: picture model: {e:#}");
                    return None;
                }
            }
        }
    };
    match Judge::load(models) {
        Ok(j) => {
            info!("packages: picture model ready");
            let j = Arc::new(j);
            *g = Some(j.clone());
            Some(j)
        }
        Err(e) => {
            warn!("packages: {e:#}");
            None
        }
    }
}

#[cfg(all(target_os = "macos", target_arch = "x86_64"))]
struct Judge;
#[cfg(all(target_os = "macos", target_arch = "x86_64"))]
impl Judge {
    fn package(&self, _img: &RgbImage, _r: &[f32; 4]) -> anyhow::Result<f32> {
        Ok(0.0)
    }
    fn carrying(&self, _img: &RgbImage, _b: &[f32; 4]) -> anyhow::Result<f32> {
        Ok(0.0)
    }
}
#[cfg(all(target_os = "macos", target_arch = "x86_64"))]
fn judge(_engine: &Arc<Engine>) -> Option<Arc<Judge>> {
    None
}

/// `ambersight packages-scan`: the check run over past visits from the
/// recordings (before = 15 s before the event, after = 40 s after it,
/// both at the analysis width). Prints numbers and event ids only.
#[cfg(not(all(target_os = "macos", target_arch = "x86_64")))]
pub fn scan(cfg: &crate::config::Config, camera: &str, hours: i64) -> anyhow::Result<()> {
    let data = cfg.storage.data_dir.clone();
    let db = crate::db::Db::open(&data.join("ambersight.db"))?;
    let dir = data.join("models");
    anyhow::ensure!(crate::smart::models_present(&dir), "the smart search picture model is not downloaded");
    let judge = Judge::load(Arc::new(crate::smart::SmartModels::load(&dir, 4)?))?;
    let width = cfg.cameras.iter().find(|c| c.name == camera).map(|c| c.scale).unwrap_or(640).max(160);
    let folders = cfg.storage.folders.clone();
    let resolve = |r: &crate::db::SegmentRow| {
        let root = if r.folder.is_empty() { data.clone() } else { folders.iter().find(|f| f.name == r.folder).map(|f| f.path.clone()).unwrap_or(data.clone()) };
        root.join(&r.path)
    };
    let pic = |at_ms: i64| -> Option<RgbImage> {
        let j = super::playback::still_jpeg(&db, &slug(camera), at_ms, &resolve).ok()??;
        let img = image::load_from_memory(&j).ok()?.to_rgb8();
        let h = (width as f32 * img.height() as f32 / img.width() as f32).round() as u32;
        Some(image::imageops::resize(&img, width, h, image::imageops::FilterType::Triangle))
    };
    let since = Utc::now().timestamp() - hours * 3600;
    let reviews = db.ai_reviews(since, Some(camera), 100_000)?;
    let (mut visits, mut lit, mut flagged) = (0, 0, 0);
    println!("event\tlocal time\ttoward door\tcarrying\tchanged places\tbest package now\tsame place before\tresult");
    for (id, _, _, _, detail) in reviews.iter().rev() {
        let Ok(j) = serde_json::from_str::<serde_json::Value>(detail) else { continue };
        let person = j["checks"].as_array().is_some_and(|cs| cs.iter().any(|c| c["seen"].as_array().is_some_and(|s| s.iter().any(|d| d["label"] == "person" && d["score"].as_f64().unwrap_or(0.0) >= 0.5))));
        if !person {
            continue;
        }
        let Ok(Some(ev)) = db.get_event(id) else { continue };
        visits += 1;
        // The AI's pictures of the person during the visit.
        let mut carry = 0.0f32;
        let mut people: Vec<[f32; 4]> = Vec::new();
        let (mut heights, mut leaving): (Vec<f32>, bool) = (Vec::new(), false);
        for c in j["checks"].as_array().into_iter().flatten() {
            let Some(b) = c["seen"].as_array().into_iter().flatten().filter(|d| d["label"] == "person" && d["score"].as_f64().unwrap_or(0.0) >= 0.5)
                .map(|d| [d["x"].as_f64().unwrap_or(0.0) as f32, d["y"].as_f64().unwrap_or(0.0) as f32, d["w"].as_f64().unwrap_or(0.0) as f32, d["h"].as_f64().unwrap_or(0.0) as f32])
                .max_by(|a, b| (a[2] * a[3]).total_cmp(&(b[2] * b[3]))) else { continue };
            let tallest = heights.iter().copied().fold(0.0f32, f32::max);
            heights.push(b[3]);
            leaving |= b[3] < tallest * 0.75;
            if leaving {
                continue;
            }
            people.push(b);
            let Ok(bytes) = std::fs::read(data.join("thumbs").join("ai").join(id).join(format!("{}.jpg", c["n"].as_i64().unwrap_or(-1)))) else { continue };
            let Ok(img) = image::load_from_memory(&bytes).map(|i| i.to_rgb8()) else { continue };
            carry = carry.max(judge.carrying(&img, &b)?);
        }
        let toward = toward_door(&heights);
        if !toward {
            people.clear();
        }
        let carried = toward && carry >= CARRY_MIN;
        let (Some(b), Some(a)) = (pic(ev.start * 1000 - 15_000), pic(ev.end * 1000 + 40_000)) else { continue };
        let when = chrono::DateTime::<Utc>::from_timestamp(ev.start, 0).map(|t| t.with_timezone(&chrono::Local).format("%a %H:%M").to_string()).unwrap_or_default();
        let Some(regions) = changed_regions(&small(&b), &small(&a)) else {
            lit += 1;
            flagged += carried as usize;
            println!("{id}\t{when}\t{}\t{carry:.2}\t-\t-\t-\t{}", if toward { "yes" } else { "no" }, if carried { "CARRIED" } else { "light changed" });
            continue;
        };
        let regions: Vec<[f32; 4]> = regions.into_iter().filter(|r| near_visitor(r, &people)).collect();
        let mut best = (0.0f32, 0.0f32);
        for r in regions.iter().take(MAX_REGIONS) {
            let pa = judge.package(&a, r)?;
            if pa > best.0 {
                best = (pa, judge.package(&b, r)?);
            }
        }
        let hit = best.0 >= NEW_MIN && best.1 < BEFORE_MAX;
        flagged += (hit || carried) as usize;
        println!("{id}\t{when}\t{}\t{carry:.2}\t{}\t{:.2}\t{:.2}\t{}", if toward { "yes" } else { "no" }, regions.len(), best.0, best.1, if hit { "PACKAGE" } else if carried { "CARRIED" } else { "-" });
    }
    println!("visits with a person: {visits}; light changed: {lit}; would announce a package: {flagged}");
    Ok(())
}

#[cfg(all(target_os = "macos", target_arch = "x86_64"))]
pub fn scan(_cfg: &crate::config::Config, _camera: &str, _hours: i64) -> anyhow::Result<()> {
    anyhow::bail!("not available on Intel Macs")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn grey(w: u32, h: u32, v: u8) -> GrayImage {
        GrayImage::from_pixel(w, h, image::Luma([v]))
    }

    #[test]
    fn a_new_thing_is_one_changed_place() {
        let a = grey(160, 120, 90);
        let mut b = a.clone();
        for y in 80..100 {
            for x in 60..90 {
                b.put_pixel(x, y, image::Luma([180]));
            }
        }
        let r = changed_regions(&a, &b).unwrap();
        assert_eq!(r.len(), 1);
        let [x, y, w, h] = r[0];
        assert!((x - 59.0 / 160.0).abs() < 0.01 && (y - 79.0 / 120.0).abs() < 0.01, "{:?}", r[0]);
        assert!((w - 32.0 / 160.0).abs() < 0.01 && (h - 22.0 / 120.0).abs() < 0.01, "{:?}", r[0]);
    }

    #[test]
    fn light_and_specks_are_not_changes() {
        let a = grey(160, 120, 90);
        // Brighter overall (clouds): nothing.
        assert!(changed_regions(&a, &grey(160, 120, 130)).unwrap().is_empty());
        // A speck: too small.
        let mut b = a.clone();
        b.put_pixel(10, 10, image::Luma([250]));
        assert!(changed_regions(&a, &b).unwrap().is_empty());
        // Half the picture different (light switched on in part): cannot tell.
        let mut c = a.clone();
        for y in 0..80 {
            for x in 0..160 {
                c.put_pixel(x, y, image::Luma([230]));
            }
        }
        assert!(changed_regions(&a, &c).is_none());
    }

    #[test]
    fn chance_follows_the_closest_words() {
        let pkg = vec![vec![1.0, 0.0, 0.0]];
        let other = vec![vec![0.0, 1.0, 0.0], vec![0.0, 0.0, 1.0]];
        assert!(package_chance(&[0.9, 0.1, 0.0], &pkg, &other) > 0.99);
        assert!(package_chance(&[0.1, 0.9, 0.0], &pkg, &other) < 0.01);
        let even = package_chance(&[0.0, 0.0, 0.0], &pkg, &other);
        assert!((even - 1.0 / 3.0).abs() < 1e-4);
    }

    #[test]
    fn only_visits_toward_the_door() {
        assert!(toward_door(&[0.2, 0.3, 0.5]), "walking up");
        assert!(!toward_door(&[0.6, 0.4, 0.2]), "walking away");
        assert!(!toward_door(&[0.3, 0.32, 0.31]), "passing by");
        assert!(!toward_door(&[0.5]), "one look");
    }

    #[test]
    fn only_places_near_the_visitor_count() {
        let person = [[0.4, 0.3, 0.1, 0.4]];
        assert!(near_visitor(&[0.42, 0.65, 0.05, 0.05], &person), "at their feet");
        assert!(!near_visitor(&[0.05, 0.1, 0.1, 0.05], &person), "the street far away");
    }

    #[test]
    fn overlap_and_crop() {
        assert_eq!(overlap(&[0.0, 0.0, 0.5, 0.5], &[0.0, 0.0, 1.0, 1.0]), 1.0);
        assert_eq!(overlap(&[0.0, 0.0, 0.2, 0.2], &[0.5, 0.5, 0.2, 0.2]), 0.0);
        let img = RgbImage::new(640, 480);
        let c = crop(&img, &[0.95, 0.95, 0.05, 0.05]).unwrap();
        assert_eq!((c.width(), c.height()), (64, 64), "at least 64 px, kept inside the picture");
    }

    #[test]
    fn only_package_events_skip_the_interval() {
        let ev = |o: &str| Event { id: "x".into(), camera_id: 0, camera: "c".into(), start: 0, end: 0, score: 0.0, clip: None, thumb: None, objects: vec![o.into()], protect: false };
        assert!(is_package_event(&ev("package")));
        assert!(is_package_event(&ev("package picked up")));
        assert!(!is_package_event(&ev("person")));
    }
}
