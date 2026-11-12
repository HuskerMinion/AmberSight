//! Smart search (see crate::smart): turning it on downloads the model;
//! then every event's pictures (the frames AI checked, else its thumbnail)
//! and the objects AI found in them get fingerprints in the background,
//! newest events first, so older events become searchable too. A search
//! ranks events by their best-matching picture.

// Intel Macs have no built-in AI runtime, so much of this goes unused there.
#![cfg_attr(all(target_os = "macos", target_arch = "x86_64"), allow(dead_code, unused_imports))]

use super::*;
use crate::smart;
use std::collections::HashMap;
use std::sync::Mutex;
#[cfg(not(all(target_os = "macos", target_arch = "x86_64")))]
use std::sync::RwLock;
use std::time::Duration;

#[cfg(not(all(target_os = "macos", target_arch = "x86_64")))]
static MODELS: RwLock<Option<Arc<smart::SmartModels>>> = RwLock::new(None);

#[derive(Clone, Default, serde::Serialize)]
pub struct SmartDownload {
    pub running: bool,
    pub done: u64,
    pub total: u64,
    pub error: Option<String>,
}

static DOWNLOAD: Mutex<SmartDownload> = Mutex::new(SmartDownload { running: false, done: 0, total: 0, error: None });

/// Frames looked at per event at most, and objects per frame.
const MAX_FRAMES: usize = 2;
const MAX_OBJECTS: usize = 3;
/// Objects AI was less sure of than this are not fingerprinted on their own.
const MIN_OBJECT_SCORE: f32 = 0.35;
/// Objects worth a fingerprint of their own: what moves (people, vehicles,
/// animals) and what they carry. Furniture and the like that AI sometimes
/// sees indoors would only crowd the results.
const OBJECT_LABELS: &[&str] = &[
    "person", "people", "pedestrian", "bicycle", "bike", "car", "motorcycle", "motorbike", "bus", "truck", "van", "suv", "vehicle", "boat",
    "bird", "cat", "dog", "horse", "sheep", "cow", "bear", "deer", "fox", "squirrel", "rabbit", "raccoon", "coyote", "animal", "backpack",
    "umbrella", "handbag", "skateboard",
];

/// Bumped when what is fingerprinted changes: everything is done again.
const INDEX_VERSION: &str = "2";

/// Results must stand out at least this far (spreads above the average
/// picture). Loose: things seen often (cars) never stand out much, and a
/// strict cutoff dropped them; this only trims the long tail.
const MIN_STANDOUT: f32 = 1.0;

/// The CPU threads one picture may use (it runs beside everything else).
const THREADS: usize = 2;

/// One search result: an event and its best-matching picture.
#[derive(Clone, Debug, serde::Serialize)]
pub struct SmartHit {
    pub event_id: String,
    pub camera: String,
    pub at_ms: i64,
    /// The picture (GET /api/smart/pic/:pic).
    pub pic: i64,
    /// Where in the picture, when it was an object (x, y, w, h: 0..1).
    pub r#box: Option<Vec<f32>>,
    pub label: Option<String>,
    pub score: f32,
    /// How far it stands out from all pictures searched (spreads above
    /// the average).
    pub z: f32,
}

impl Engine {
    fn smart_dir(&self) -> std::path::PathBuf {
        self.data_dir.join("models")
    }

    #[cfg(not(all(target_os = "macos", target_arch = "x86_64")))]
    pub(super) fn smart_models(&self) -> Option<Arc<smart::SmartModels>> {
        MODELS.read().unwrap().clone()
    }

    /// Match the settings now: load the model (downloading it first if
    /// needed), or drop it when smart search is off.
    pub fn reload_smart(self: &Arc<Self>) {
        let enabled = self.config.lock().unwrap().smart_search.enabled;
        #[cfg(not(all(target_os = "macos", target_arch = "x86_64")))]
        {
            if !enabled {
                *MODELS.write().unwrap() = None;
                return;
            }
            if MODELS.read().unwrap().is_some() {
                return;
            }
            if !smart::models_present(&self.smart_dir()) {
                self.start_smart_download();
                return;
            }
            match smart::SmartModels::load(&self.smart_dir(), THREADS) {
                Ok(m) => {
                    info!("smart search: model loaded");
                    DOWNLOAD.lock().unwrap().error = None;
                    *MODELS.write().unwrap() = Some(Arc::new(m));
                }
                Err(e) => {
                    warn!("smart search: {e:#}");
                    DOWNLOAD.lock().unwrap().error = Some(format!("{e:#}"));
                }
            }
        }
        #[cfg(all(target_os = "macos", target_arch = "x86_64"))]
        let _ = enabled;
    }

    pub(super) fn start_smart_download(self: &Arc<Self>) {
        {
            let mut d = DOWNLOAD.lock().unwrap();
            if d.running {
                return;
            }
            *d = SmartDownload { running: true, done: 0, total: smart::total_bytes(), error: None };
        }
        info!("smart search: downloading the model ({} MB)", smart::total_bytes() >> 20);
        let engine = self.clone();
        thread::spawn(move || {
            let r = smart::download(&engine.smart_dir(), &|done, total| {
                let mut d = DOWNLOAD.lock().unwrap();
                d.done = done;
                d.total = total;
            });
            {
                let mut d = DOWNLOAD.lock().unwrap();
                d.running = false;
                if let Err(e) = &r {
                    warn!("smart search: {e:#}");
                    d.error = Some(super::download_failure("the smart search model", e));
                }
            }
            if r.is_ok() {
                engine.reload_smart();
            }
        });
    }

    pub fn smart_status(&self) -> serde_json::Value {
        let enabled = self.config.lock().unwrap().smart_search.enabled;
        #[cfg(not(all(target_os = "macos", target_arch = "x86_64")))]
        let (supported, loaded) = (true, self.smart_models().is_some());
        #[cfg(all(target_os = "macos", target_arch = "x86_64"))]
        let (supported, loaded) = (false, false);
        let (done, waiting, pictures) = if enabled { self.db.smart_counts().unwrap_or_default() } else { (0, 0, 0) };
        serde_json::json!({
            "enabled": enabled,
            "supported": supported,
            "loaded": loaded,
            "model_mb": smart::total_bytes() >> 20,
            "download": DOWNLOAD.lock().unwrap().clone(),
            "events_done": done,
            "events_waiting": waiting,
            "pictures": pictures,
        })
    }

    /// Events whose best picture matches `q`, best first. `camera`: only
    /// cameras whose name contains it (any case).
    pub fn smart_search(&self, q: &str, camera: &str, from_ms: i64, to_ms: i64, limit: usize, plain: bool, min_z: Option<f32>, allowed: &dyn Fn(&str) -> bool) -> anyhow::Result<Vec<SmartHit>> {
        #[cfg(not(all(target_os = "macos", target_arch = "x86_64")))]
        {
            let Some(m) = self.smart_models() else { anyhow::bail!("smart search is not ready yet") };
            // A picture's likeness to the words, less its likeness to words
            // that say nothing in particular: some pictures are a bit like
            // everything, and would otherwise top every search.
            let qv = m.embed_query(q)?;
            let nv = m.neutral()?;
            let qv: Vec<f32> = if plain { qv } else { qv.iter().zip(&nv).map(|(a, b)| a - b).collect() };
            let cam = camera.trim().to_lowercase();
            let mut best: HashMap<String, SmartHit> = HashMap::new();
            let (mut n, mut sum, mut sum2) = (0f64, 0f64, 0f64);
            self.db.smart_scan(from_ms, to_ms, &mut |id, ev, c, at, bx, label, emb| {
                if !cam.is_empty() && !c.to_lowercase().contains(&cam) || !allowed(c) {
                    return;
                }
                let s = smart::score(emb, &qv);
                n += 1.0;
                sum += s as f64;
                sum2 += (s as f64) * (s as f64);
                if best.get(ev).is_some_and(|h| h.score >= s) {
                    return;
                }
                best.insert(
                    ev.to_string(),
                    SmartHit {
                        event_id: ev.to_string(),
                        camera: c.to_string(),
                        at_ms: at,
                        pic: id,
                        r#box: bx.and_then(|b| serde_json::from_str(b).ok()),
                        label: label.map(String::from),
                        score: s,
                        z: 0.0,
                    },
                );
            })?;
            let mut hits: Vec<SmartHit> = best.into_values().collect();
            hits.sort_by(|a, b| b.score.total_cmp(&a.score));
            // Best first; the long tail of pictures no more like the words
            // than most is left out.
            if n >= 50.0 {
                let mean = sum / n;
                let sd = (sum2 / n - mean * mean).max(1e-12).sqrt();
                for h in hits.iter_mut() {
                    h.z = ((h.score as f64 - mean) / sd) as f32;
                }
                hits.retain(|h| h.z >= min_z.unwrap_or(MIN_STANDOUT));
            }
            hits.truncate(limit);
            Ok(hits)
        }
        #[cfg(all(target_os = "macos", target_arch = "x86_64"))]
        {
            let _ = (q, camera, from_ms, to_ms, limit, plain, min_z, allowed);
            anyhow::bail!("smart search is not available on this computer")
        }
    }

    /// The picture behind a search result (JPEG).
    pub fn smart_pic(&self, id: i64, allowed: &dyn Fn(&str) -> bool) -> anyhow::Result<Option<Vec<u8>>> {
        let Some((src, camera)) = self.db.smart_src(id)? else { return Ok(None) };
        if !allowed(&camera) {
            return Ok(None);
        }
        if src.split(['/', '\\']).any(|p| p == ".." || p.is_empty()) {
            return Ok(None);
        }
        Ok(std::fs::read(self.data_dir.join("thumbs").join(src)).ok())
    }
}

/// An event's pictures: (at_ms, path under thumbs/, objects AI found).
fn pictures_of(engine: &Engine, id: &str, start_s: i64) -> Vec<(i64, String, Vec<crate::ai::Detection>)> {
    let mut out = Vec::new();
    if let Ok(Some(detail)) = engine.db.ai_review(id) {
        if let Ok(v) = serde_json::from_str::<serde_json::Value>(&detail) {
            let mut frames: Vec<(i64, String, Vec<crate::ai::Detection>, f32)> = Vec::new();
            for c in v["checks"].as_array().into_iter().flatten() {
                if c["frame"].as_bool() != Some(true) {
                    continue;
                }
                let Some(n) = c["n"].as_u64() else { continue };
                let rel = format!("ai/{id}/{n}.jpg");
                if !engine.data_dir.join("thumbs").join(&rel).is_file() {
                    continue;
                }
                let seen: Vec<crate::ai::Detection> = serde_json::from_value(c["seen"].clone()).unwrap_or_default();
                let biggest = seen.iter().filter(|d| d.score >= MIN_OBJECT_SCORE && OBJECT_LABELS.contains(&d.label.as_str())).map(|d| d.w * d.h).fold(0.0f32, f32::max);
                frames.push((start_s * 1000 + c["offset_ms"].as_i64().unwrap_or(0), rel, seen, biggest));
            }
            // The frames with the largest objects (nearest the camera).
            frames.sort_by(|a, b| b.3.total_cmp(&a.3).then(a.0.cmp(&b.0)));
            out.extend(frames.into_iter().take(MAX_FRAMES).map(|(at, rel, seen, _)| (at, rel, seen)));
        }
    }
    if out.is_empty() {
        let rel = format!("{id}.jpg");
        if engine.data_dir.join("thumbs").join(&rel).is_file() {
            out.push((start_s * 1000, rel, Vec::new()));
        }
    }
    out
}

/// Fingerprint one event's pictures and objects.
#[cfg(not(all(target_os = "macos", target_arch = "x86_64")))]
fn index_event(engine: &Engine, m: &smart::SmartModels, id: &str, camera: &str, start_s: i64) -> anyhow::Result<usize> {
    let mut rows = Vec::new();
    for (at, rel, seen) in pictures_of(engine, id, start_s) {
        let Ok(bytes) = std::fs::read(engine.data_dir.join("thumbs").join(&rel)) else { continue };
        let Ok(img) = image::load_from_memory(&bytes).map(|i| i.to_rgb8()) else { continue };
        rows.push((camera.to_string(), at, rel.clone(), None, None, smart::to_bytes(&m.embed_image(&img)?)));
        let mut objs: Vec<&crate::ai::Detection> = seen.iter().filter(|d| d.score >= MIN_OBJECT_SCORE && OBJECT_LABELS.contains(&d.label.as_str())).collect();
        // People and vehicles first (they are what "Follow" and the alert
        // rules look for), then the rest, each the largest first.
        objs.sort_by(|a, b| crate::follow::kind_of(&a.label).is_none().cmp(&crate::follow::kind_of(&b.label).is_none()).then((b.w * b.h).total_cmp(&(a.w * a.h))));
        for d in objs.into_iter().take(MAX_OBJECTS) {
            let Some(crop) = smart::crop_box(&img, d.x, d.y, d.w, d.h) else { continue };
            let bx = serde_json::to_string(&[d.x, d.y, d.w, d.h]).ok();
            rows.push((camera.to_string(), at, rel.clone(), bx, Some(d.label.clone()), smart::to_bytes(&m.embed_image(&crop)?)));
        }
    }
    engine.db.smart_store(id, &rows)?;
    // Alert rules look at the new fingerprints (and a held alert is decided).
    let embs: Vec<&[u8]> = rows.iter().map(|r| r.5.as_slice()).collect();
    engine.apply_rules(m, id, &embs);
    Ok(rows.len())
}

/// Background: fingerprint events as they come, and older ones (newest
/// first) until all are done.
pub fn smart_loop(engine: Arc<Engine>) {
    // (Intel Macs have no built-in AI runtime: nothing to do there.)
    #[cfg(all(target_os = "macos", target_arch = "x86_64"))]
    let _ = engine;
    #[cfg(not(all(target_os = "macos", target_arch = "x86_64")))]
    smart_loop_on(engine);
}

#[cfg(not(all(target_os = "macos", target_arch = "x86_64")))]
fn smart_loop_on(engine: Arc<Engine>) {
    thread::sleep(Duration::from_secs(30));
    if engine.db.ui_get("smart", "index_version").ok().flatten().as_deref() != Some(INDEX_VERSION) {
        match engine.db.smart_reset() {
            Ok(()) => {
                let _ = engine.db.ui_put("smart", "index_version", INDEX_VERSION);
            }
            Err(e) => warn!("smart search: {e:#}"),
        }
    }
    let mut last_prune = std::time::Instant::now() - Duration::from_secs(3600);
    let mut backlog_logged = false;
    let mut since_log = 0usize;
    let mut failures: HashMap<String, u32> = HashMap::new();
    loop {
        engine.release_expired_holds();
        #[cfg(not(all(target_os = "macos", target_arch = "x86_64")))]
        if let Some(m) = engine.smart_models() {
            if last_prune.elapsed() > Duration::from_secs(3600) {
                last_prune = std::time::Instant::now();
                match engine.db.smart_prune() {
                    Ok(n) if n > 0 => info!("smart search: dropped {n} pictures of deleted events"),
                    Err(e) => warn!("smart search: {e:#}"),
                    _ => {}
                }
            }
            let batch = engine.db.smart_pending(20).unwrap_or_default();
            if batch.is_empty() {
                if backlog_logged {
                    info!("smart search: every event is searchable");
                    backlog_logged = false;
                }
                // (Quicker with alert rules: they wait for this.)
                thread::sleep(Duration::from_secs(if engine.rules_active() { 3 } else { 10 }));
                continue;
            }
            let t0 = std::time::Instant::now();
            let mut pics = 0;
            for (id, cam, start) in &batch {
                engine.release_expired_holds();
                if engine.smart_models().is_none() {
                    break;
                }
                match index_event(&engine, &m, id, cam, *start) {
                    Ok(n) => pics += n,
                    Err(e) => {
                        warn!("smart search: event {id}: {e:#}");
                        // Tried again a few times (it may be passing, e.g. the
                        // model was turned off meanwhile), then marked looked
                        // at so it is not tried forever.
                        let n = failures.entry(id.clone()).or_insert(0);
                        *n += 1;
                        if *n >= 3 {
                            let _ = engine.db.smart_store(id, &[]);
                            failures.remove(id);
                        }
                    }
                }
            }
            if batch.len() == 20 {
                since_log += batch.len();
                if !backlog_logged || since_log >= 1000 {
                    let (done, waiting, _) = engine.db.smart_counts().unwrap_or_default();
                    info!("smart search: making older events searchable ({done} done, {waiting} to go; the last 20 took {} ms, {pics} pictures)", t0.elapsed().as_millis());
                    backlog_logged = true;
                    since_log = 0;
                }
            }
            // Gentle on the CPU: rest as long as the work took.
            thread::sleep(t0.elapsed().min(Duration::from_secs(5)));
            continue;
        }
        thread::sleep(Duration::from_secs(if engine.rules_active() { 3 } else { 10 }));
    }
}
