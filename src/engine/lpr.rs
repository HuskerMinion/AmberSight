//! License plate recognition on finished vehicle events: a few full-quality
//! frames from the recordings (where AI saw a car, truck, bus or motorcycle
//! that was not parked), the plate found inside each vehicle's box and
//! read, the best reading per plate kept, and compared with the plates
//! people have named (Settings > License plates). The readings become
//! event labels ("plate ABC123 (Mom's car)", "unknown plate XYZ789"), are
//! saved with a picture and are published over MQTT.
//!
//! Nothing runs unless plates are turned on for the camera. A camera can
//! also be set to look at the whole picture when AI saw no vehicle.

use super::*;
use crate::lpr::{self, Candidate, LprModels, PlateBox};
use std::sync::{Mutex, RwLock};

static MODELS: RwLock<Option<Arc<LprModels>>> = RwLock::new(None);

#[derive(Clone, Default, serde::Serialize)]
pub struct LprDownload {
    pub running: bool,
    pub done: u64,
    pub total: u64,
    pub error: Option<String>,
}

static DOWNLOAD: Mutex<LprDownload> = Mutex::new(LprDownload { running: false, done: 0, total: 0, error: None });

/// Frames looked at per event at most.
const MAX_FRAMES: usize = 3;
/// Vehicles per frame at most.
const MAX_VEHICLES: usize = 3;
/// Plates kept per event at most.
const MAX_PLATES: usize = 4;
/// The detector must be this sure it saw a plate.
const MIN_DETECTION: f32 = 0.40;
/// Readings shorter than this are not plates.
const MIN_CHARS: usize = 3;
/// Unknown plates are forgotten after this long.
/// How long plate readings are kept (`[lpr] keep_days`).
pub(super) fn keep_ms(engine: &Engine) -> i64 {
    i64::from(engine.config.lock().unwrap().lpr.keep_days.max(1)) * 86_400_000
}

pub(super) use crate::tracker::is_vehicle;

/// One plate read in an event.
#[derive(Clone, Debug)]
pub struct PlateHit {
    pub text: String,
    pub confidence: f32,
    /// The known plate's id and name, when it matched one.
    pub known: Option<(i64, String)>,
}

impl PlateHit {
    pub fn label(&self) -> String {
        lpr::label(&self.text, self.known.as_ref().map(|k| k.1.as_str()))
    }
}

/// What reading plates found in one event.
#[derive(Default, Debug)]
pub struct PlateResult {
    pub plates: Vec<PlateHit>,
}

impl PlateResult {
    /// A plate of the library was read.
    pub fn known_plate(&self) -> bool {
        self.plates.iter().any(|p| p.known.is_some())
    }

    /// A plate that is not in the library was read.
    pub fn unknown_plate(&self) -> bool {
        self.plates.iter().any(|p| p.known.is_none())
    }

    pub fn labels(&self) -> Vec<String> {
        self.plates.iter().map(|p| p.label()).collect()
    }
}

impl Engine {
    pub fn lpr_models_dir(&self) -> std::path::PathBuf {
        self.data_dir.join("models")
    }

    pub fn lpr_loaded(&self) -> bool {
        MODELS.read().unwrap().is_some()
    }

    /// Load (or drop) the plate models to match the settings now.
    pub fn reload_lpr(&self) -> anyhow::Result<()> {
        let (enabled, threads) = {
            let c = self.config.lock().unwrap();
            (c.lpr.enabled, c.ai.threads)
        };
        let dir = self.lpr_models_dir();
        let new = if enabled && lpr::models_present(&dir) {
            let m = LprModels::load(&dir, threads)?;
            info!("plates: models loaded");
            Some(Arc::new(m))
        } else {
            None
        };
        *MODELS.write().unwrap() = new;
        Ok(())
    }

    /// Download the plate models in the background, then load them.
    pub fn start_lpr_download(self: &Arc<Self>) -> bool {
        {
            let mut d = DOWNLOAD.lock().unwrap();
            if d.running {
                return false;
            }
            *d = LprDownload { running: true, done: 0, total: lpr::total_bytes(), error: None };
        }
        let engine = self.clone();
        thread::spawn(move || {
            let r = lpr::download_models(&engine.lpr_models_dir(), &|done, total| {
                let mut d = DOWNLOAD.lock().unwrap();
                d.done = done;
                d.total = total;
            });
            let r = r.and_then(|_| engine.reload_lpr());
            let mut d = DOWNLOAD.lock().unwrap();
            d.running = false;
            if let Err(e) = r {
                warn!("plates: {e:#}");
                d.error = Some(super::download_failure("the plate reader", &e));
            }
        });
        true
    }

    pub fn lpr_status(&self) -> serde_json::Value {
        let c = self.config.lock().unwrap().lpr.clone();
        let open = {
            let k = self.config.lock().unwrap();
            k.server.auth.is_none() && k.server.users.is_empty()
        };
        serde_json::json!({
            "settings": c,
            "models": lpr::models_present(&self.lpr_models_dir()),
            "loaded": self.lpr_loaded(),
            "download": DOWNLOAD.lock().unwrap().clone(),
            "ai": self.detector().is_some() || self.config.lock().unwrap().ai.url.as_deref().is_some_and(|u| !u.is_empty()),
            // No sign-in accounts: anyone on the network is an admin.
            "open": open,
        })
    }

    /// Whether plates should be read for this event: on for the camera,
    /// with the models loaded.
    pub(super) fn lpr_wanted(&self, camera: &str, ai: &super::ai_check::AiOutcome) -> bool {
        if MODELS.read().unwrap().is_none() {
            return false;
        }
        let c = self.config.lock().unwrap();
        let Some(cam) = c.lpr.for_camera(camera) else { return false };
        cam.full_frame || ai.log.iter().any(|k| k.seen.iter().any(|d| is_vehicle(&d.label)))
    }

    /// (known, unknown): plates read in an event that are in the library or
    /// not. For rules that act on strangers only (the stranger filter reads
    /// the readings it just made instead).
    #[allow(dead_code)]
    pub fn event_plate_flags(&self, event_id: &str) -> (bool, bool) {
        let (k, u) = self.db.plate_flags(event_id).unwrap_or((0, 0));
        (k > 0, u > 0)
    }
}

/// A frame to look at: when, the vehicles in it (normalized boxes; none =
/// the whole picture) and the picture the AI check itself used.
struct Look {
    at_ms: i64,
    vehicles: Vec<crate::ai::Detection>,
    fallback: Vec<u8>,
}

/// The frames to look at: those with the largest vehicle that is not
/// parked; for a full-picture camera also frames with no vehicle.
fn pick_looks(ai: &super::ai_check::AiOutcome, full_frame: bool, event_ms: (i64, i64)) -> Vec<Look> {
    let mut looks: Vec<(f32, Look)> = ai
        .log
        .iter()
        .filter_map(|c| {
            let mut v: Vec<crate::ai::Detection> = c
                .seen
                .iter()
                .enumerate()
                .filter(|(i, d)| is_vehicle(&d.label) && d.score >= 0.4 && !c.parked.get(*i).copied().unwrap_or(false))
                .map(|(_, d)| d.clone())
                .collect();
            v.sort_by(|a, b| (b.w * b.h).total_cmp(&(a.w * a.h)));
            v.truncate(MAX_VEHICLES);
            let biggest = v.first().map(|d| d.w * d.h).unwrap_or(0.0);
            (!v.is_empty()).then(|| (biggest, Look { at_ms: c.at_ms, vehicles: v, fallback: c.frame.clone() }))
        })
        .collect();
    looks.sort_by(|a, b| b.0.total_cmp(&a.0));
    let mut out: Vec<Look> = looks.into_iter().take(MAX_FRAMES).map(|l| l.1).collect();
    if out.is_empty() && full_frame {
        // Whole pictures: the AI looks, or else the middle of the event.
        let mut whole: Vec<Look> = ai.log.iter().take(MAX_FRAMES).map(|c| Look { at_ms: c.at_ms, vehicles: Vec::new(), fallback: c.frame.clone() }).collect();
        if whole.is_empty() {
            whole.push(Look { at_ms: (event_ms.0 + event_ms.1) / 2, vehicles: Vec::new(), fallback: Vec::new() });
        }
        out = whole;
    }
    out
}

/// Plates in one picture region, read: (reading, box in the full picture).
fn read_region(models: &LprModels, img: &image::RgbImage, region: (u32, u32, u32, u32), min_px: u32) -> anyhow::Result<Vec<(lpr::Read, PlateBox)>> {
    let (x0, y0, x1, y1) = region;
    let crop = image::imageops::crop_imm(img, x0, y0, x1 - x0, y1 - y0).to_image();
    let mut out = Vec::new();
    for mut b in models.detect(&crop, MIN_DETECTION)?.into_iter().take(3) {
        // Back to the full picture.
        b.x += x0 as f32;
        b.y += y0 as f32;
        if b.w < min_px as f32 {
            continue;
        }
        if let Some(r) = models.read(img, &b)? {
            out.push((r, b));
        }
    }
    Ok(out)
}

/// Read the plates of a finished event that AI saw a vehicle in.
pub(super) fn recognize(engine: &Engine, camera: &str, event_id: &str, ai: &super::ai_check::AiOutcome, event_ms: (i64, i64)) -> PlateResult {
    let mut out = PlateResult::default();
    let Some(models) = MODELS.read().unwrap().clone() else { return out };
    let mut lc = engine.config.lock().unwrap().lpr.clone();
    lc.sanitize();
    let Some(cam) = lc.for_camera(camera).cloned() else { return out };
    let (min_conf, min_px) = (lc.confidence_for(&cam), lc.plate_px_for(&cam));
    let looks = pick_looks(ai, cam.full_frame, event_ms);
    if looks.is_empty() {
        return out;
    }
    let started = std::time::Instant::now();
    // (picture, plate width) of each reading.
    let mut all: Vec<Candidate<(i64, Vec<u8>, u32)>> = Vec::new();
    let mut reads = 0usize;
    for look in looks {
        // The recording's full-quality picture; else what the AI check saw.
        let jpeg = match engine.still_at(camera, look.at_ms) {
            Ok(Some(j)) => j,
            _ => look.fallback.clone(),
        };
        if jpeg.is_empty() {
            continue;
        }
        let Ok(img) = image::load_from_memory(&jpeg).map(|i| i.to_rgb8()) else { continue };
        let (iw, ih) = (img.width() as f32, img.height() as f32);
        let regions: Vec<(u32, u32, u32, u32)> = if look.vehicles.is_empty() {
            vec![(0, 0, img.width(), img.height())]
        } else {
            look.vehicles
                .iter()
                .filter_map(|v| {
                    // The vehicle's box with a little room around it.
                    let (mx, my) = (v.w * 0.08, v.h * 0.08);
                    let x0 = ((v.x - mx) * iw).max(0.0) as u32;
                    let y0 = ((v.y - my) * ih).max(0.0) as u32;
                    let x1 = (((v.x + v.w + mx) * iw) as u32).min(img.width());
                    let y1 = (((v.y + v.h + my) * ih) as u32).min(img.height());
                    (x1 > x0 + 16 && y1 > y0 + 16).then_some((x0, y0, x1, y1))
                })
                .collect()
        };
        for region in regions {
            let found = match read_region(&models, &img, region, min_px) {
                Ok(f) => f,
                Err(e) => {
                    warn!("plates: {e:#}");
                    return out;
                }
            };
            for (r, b) in found {
                reads += 1;
                if r.confidence < min_conf || r.text.chars().count() < MIN_CHARS {
                    continue;
                }
                let Ok(pic) = lpr::plate_jpeg(&img, &b, 240) else { continue };
                all.push(Candidate { text: r.text, confidence: r.confidence, extra: (look.at_ms, pic, b.w.round() as u32) });
            }
        }
    }
    tracing::debug!("camera '{camera}': plates: {reads} read in {:?}", started.elapsed());
    let best = lpr::best_readings(&all, MAX_PLATES);
    if best.is_empty() {
        return out;
    }
    let known = engine.db.known_plate_texts().unwrap_or_default();
    let cam_slug = slug(camera);
    static INSERTS: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
    for c in best {
        let known_id = lpr::best_known(&c.text, &known, lc.max_diff as usize, lc.confusions);
        let name = known_id.and_then(|id| engine.db.known_plate_name(id).ok().flatten());
        let known_hit = known_id.zip(name);
        let (at_ms, pic, width) = c.extra;
        let row = crate::db::PlateRow { event_id, camera: &cam_slug, at_ms, text: &c.text, confidence: c.confidence, width, img: &pic, known_id: known_hit.as_ref().map(|k| k.0) };
        if let Err(e) = engine.db.insert_plate(&row) {
            warn!("plates: could not save: {e:#}");
            continue;
        }
        out.plates.push(PlateHit { text: c.text, confidence: c.confidence, known: known_hit });
        if INSERTS.fetch_add(1, std::sync::atomic::Ordering::Relaxed) % 50 == 0 {
            let _ = engine.db.prune_plates(Utc::now().timestamp_millis() - keep_ms(engine));
        }
    }
    if !out.plates.is_empty() {
        if let Some(m) = engine.mqtt.get() {
            m.publish_plates(camera, event_id, &out.plates);
        }
        info!(
            "camera '{camera}': plates: {} ({})",
            out.labels().join(", "),
            match (out.known_plate(), out.unknown_plate()) {
                (true, true) => "known and unknown",
                (true, false) => "known",
                _ => "unknown",
            }
        );
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ai::Detection;

    fn det(label: &str, score: f32, w: f32) -> Detection {
        Detection { label: label.into(), score, x: 0.1, y: 0.1, w, h: w }
    }

    fn outcome(checks: Vec<(Vec<Detection>, Vec<bool>)>) -> super::super::ai_check::AiOutcome {
        super::super::ai_check::AiOutcome {
            log: checks
                .into_iter()
                .enumerate()
                .map(|(i, (seen, parked))| super::super::ai_check::AiCheck { at_ms: i as i64 * 1000, seen, parked, ..Default::default() })
                .collect(),
            ..Default::default()
        }
    }

    #[test]
    fn looks_are_the_biggest_moving_vehicles() {
        let ai = outcome(vec![
            (vec![det("car", 0.9, 0.2), det("person", 0.9, 0.5)], vec![false, false]),
            (vec![det("truck", 0.9, 0.4)], vec![false]),
            // A parked car, and one the AI doubted: neither counts.
            (vec![det("car", 0.9, 0.9)], vec![true]),
            (vec![det("car", 0.2, 0.9)], vec![false]),
            (vec![det("motorcycle", 0.8, 0.1)], vec![]),
            (vec![det("bus", 0.8, 0.3)], vec![false]),
        ]);
        let looks = pick_looks(&ai, false, (0, 10_000));
        assert_eq!(looks.iter().map(|l| l.at_ms).collect::<Vec<_>>(), vec![1000, 5000, 0], "largest first, three at most");
        assert!(looks.iter().all(|l| l.vehicles.iter().all(|v| is_vehicle(&v.label))));
        // No vehicle: nothing, unless the camera looks at the whole picture.
        let none = outcome(vec![(vec![det("person", 0.9, 0.5)], vec![false])]);
        assert!(pick_looks(&none, false, (0, 10_000)).is_empty());
        let whole = pick_looks(&none, true, (0, 10_000));
        assert_eq!((whole.len(), whole[0].vehicles.len()), (1, 0));
        let nolog = pick_looks(&outcome(vec![]), true, (2000, 6000));
        assert_eq!((nolog.len(), nolog[0].at_ms), (1, 4000));
    }

    /// With LPR_TEST_DIR (the models) and LPR_TEST_IMAGE (a car with a
    /// clear plate): the plate is found inside a crop of the vehicle and
    /// its box is in the whole picture's pixels.
    #[test]
    #[ignore]
    fn plate_is_read_inside_a_vehicle_crop() {
        let dir = std::path::PathBuf::from(std::env::var("LPR_TEST_DIR").unwrap());
        let m = LprModels::load(&dir, 2).unwrap();
        let img = image::open(std::env::var("LPR_TEST_IMAGE").unwrap()).unwrap().to_rgb8();
        let whole = m.detect(&img, 0.4).unwrap();
        let w = &whole[0];
        // A crop around the car that begins well inside the picture.
        let region = ((w.x as u32).saturating_sub(300).max(50), (w.y as u32).saturating_sub(200).max(50), (img.width() - 50).min(w.x as u32 + w.w as u32 + 300), (img.height() - 50).min(w.y as u32 + w.h as u32 + 200));
        let found = read_region(&m, &img, region, 40).unwrap();
        assert!(!found.is_empty(), "no plate in the crop");
        let (r, b) = &found[0];
        println!("crop read {} ({:.2}) at ({:.0}, {:.0}), whole-picture box at ({:.0}, {:.0})", r.text, r.confidence, b.x, b.y, w.x, w.y);
        assert!((b.x - w.x).abs() < 20.0 && (b.y - w.y).abs() < 20.0, "box not in full-picture pixels");
    }

    #[test]
    fn known_and_unknown_flags_and_labels() {
        let r = PlateResult {
            plates: vec![
                PlateHit { text: "ABC123".into(), confidence: 0.9, known: Some((1, "Mom's car".into())) },
                PlateHit { text: "XYZ789".into(), confidence: 0.8, known: None },
            ],
        };
        assert!(r.known_plate() && r.unknown_plate());
        assert_eq!(r.labels(), ["plate ABC123 (Mom's car)", "unknown plate XYZ789"]);
        let none = PlateResult::default();
        assert!(!none.known_plate() && !none.unknown_plate());
    }
}
