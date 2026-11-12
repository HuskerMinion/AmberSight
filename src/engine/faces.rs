//! Face recognition on finished person events: a few full-quality frames
//! from the recordings (where AI saw a person), faces found inside the
//! person boxes, fingerprinted and matched against the faces people have
//! named in Settings > Smart features > Faces. Names become the event's labels ("Alex", or
//! "unknown face"); a person marked quiet does not alert on their own.
//!
//! Live (for Home Assistant, e.g. a doorbell greeting): as soon as AI sees a
//! person during an event, the camera's live pictures are watched for a short
//! while and each named face is announced over MQTT at once, while the person
//! is still there. The pass after the event then does not announce them again.

use super::*;
use crate::faces::{self, FaceModels};
use std::sync::{Mutex, RwLock};

static MODELS: RwLock<Option<Arc<FaceModels>>> = RwLock::new(None);

#[derive(Clone, Default, serde::Serialize)]
pub struct FaceDownload {
    pub running: bool,
    pub done: u64,
    pub total: u64,
    pub error: Option<String>,
}

static DOWNLOAD: Mutex<FaceDownload> = Mutex::new(FaceDownload { running: false, done: 0, total: 0, error: None });

/// Frames looked at per event at most.
const MAX_FRAMES: usize = 3;
/// Faces found less surely than this are ignored.
const MIN_FACE_SCORE: f32 = 0.75;
/// Two faces in one event this alike are one person (keep the larger).
const SAME_IN_EVENT: f32 = 0.5;
/// Unnamed faces are forgotten after this long.
pub(super) const KEEP_UNNAMED_MS: i64 = 60 * 24 * 3600 * 1000;

/// Live watch: how long after AI first sees a person, and how often.
const WATCH_SECS: u64 = 20;
const WATCH_STEP_MS: u64 = 250;
/// Live pictures are small (the motion picture, ~640 px wide): smaller faces count.
const WATCH_MIN_FACE: u32 = 32;
/// A person announced on a camera is not announced there again for this long.
const ANNOUNCE_AGAIN_SECS: u64 = 120;
/// Cameras being watched now.
static WATCHING: Mutex<Vec<String>> = Mutex::new(Vec::new());
/// (camera, person id) -> when last announced.
static ANNOUNCED: Mutex<Vec<(String, i64, std::time::Instant)>> = Mutex::new(Vec::new());

/// Claim an announcement: false if this person was announced on this camera recently.
fn claim_announce(camera: &str, pid: i64) -> bool {
    let mut a = ANNOUNCED.lock().unwrap();
    let now = std::time::Instant::now();
    a.retain(|(_, _, t)| now.duration_since(*t).as_secs() < ANNOUNCE_AGAIN_SECS);
    if a.iter().any(|(c, p, _)| c == camera && *p == pid) {
        return false;
    }
    a.push((camera.to_string(), pid, now));
    true
}

/// AI just saw a person on a camera during an event: watch its live pictures
/// for named faces (only when Home Assistant is connected and faces are named).
pub(super) fn start_watch(engine: &Arc<Engine>, handle: &Arc<CameraHandle>, camera: &str) {
    if engine.mqtt.get().is_none() || MODELS.read().unwrap().is_none() {
        return;
    }
    {
        let mut w = WATCHING.lock().unwrap();
        if w.iter().any(|c| c == camera) {
            return;
        }
        w.push(camera.to_string());
    }
    let (engine, handle, name) = (engine.clone(), handle.clone(), camera.to_string());
    let spawned = std::thread::Builder::new().name("face-watch".into()).spawn(move || {
        watch(&engine, &handle, &name);
        WATCHING.lock().unwrap().retain(|c| c != &name);
    });
    if spawned.is_err() {
        WATCHING.lock().unwrap().retain(|c| c != camera);
    }
}

fn watch(engine: &Engine, handle: &CameraHandle, camera: &str) {
    let Some(models) = MODELS.read().unwrap().clone() else { return };
    let refs: Vec<(i64, Vec<f32>)> = engine.db.labeled_faces().unwrap_or_default().into_iter().map(|(p, e)| (p, faces::emb_from_bytes(&e))).collect();
    if refs.is_empty() {
        return;
    }
    let mut fc = engine.config.lock().unwrap().faces.clone();
    fc.sanitize();
    let min_face = fc.min_face.min(WATCH_MIN_FACE);
    let started = std::time::Instant::now();
    let mut last: Vec<u8> = Vec::new();
    while started.elapsed().as_secs() < WATCH_SECS {
        let frame = handle.state.read().unwrap().latest_frame.clone();
        if frame.is_empty() || frame == last {
            std::thread::sleep(std::time::Duration::from_millis(WATCH_STEP_MS));
            continue;
        }
        let Ok(img) = image::load_from_memory(&frame).map(|i| i.to_rgb8()) else { last = frame; continue };
        last = frame;
        let found = match models.detect(&img, MIN_FACE_SCORE) {
            Ok(f) => f,
            Err(e) => {
                warn!("faces (live): {e:#}");
                return;
            }
        };
        for f in found.into_iter().take(3) {
            if (f.w.round().max(0.0) as u32) < min_face {
                continue;
            }
            let Ok(emb) = models.embed(&img, &f) else { continue };
            let best = refs
                .iter()
                .map(|(pid, e)| (*pid, faces::cosine(&emb, e)))
                .filter(|(_, s)| *s >= fc.match_score)
                .max_by(|a, b| a.1.total_cmp(&b.1));
            let Some((pid, sim)) = best else { continue };
            if !claim_announce(camera, pid) {
                continue;
            }
            let Ok(Some(name)) = engine.db.person_name(pid) else { continue };
            if let Some(m) = engine.mqtt.get() {
                m.publish_faces(camera, "", &[name.clone()], 0);
            }
            info!("camera '{camera}': face (live): {name} ({:.2}) after {:.1} s", sim, started.elapsed().as_secs_f32());
        }
        std::thread::sleep(std::time::Duration::from_millis(WATCH_STEP_MS));
    }
}

/// What recognition found in one event.
#[derive(Default, Debug)]
pub struct FaceResult {
    pub names: Vec<String>,
    pub people: Vec<i64>,
    pub unknown: usize,
    /// Person boxes in the frames looked at where no usable face was found
    /// (turned away, too small): whoever that was is unknown.
    pub faceless: usize,
    /// For each unknown face (in order): the same face was seen in an
    /// earlier event within the look-back asked for. Empty when not asked.
    pub repeats: Vec<bool>,
}

impl Engine {
    pub fn face_models_dir(&self) -> std::path::PathBuf {
        self.data_dir.join("models")
    }

    pub fn faces_loaded(&self) -> bool {
        MODELS.read().unwrap().is_some()
    }

    /// Load (or drop) the face models to match the settings now.
    pub fn reload_faces(&self) -> anyhow::Result<()> {
        let (enabled, threads) = {
            let c = self.config.lock().unwrap();
            (c.faces.enabled, c.ai.threads)
        };
        let dir = self.face_models_dir();
        let new = if enabled && faces::models_present(&dir) {
            let m = FaceModels::load(&dir, threads)?;
            info!("faces: models loaded");
            Some(Arc::new(m))
        } else {
            None
        };
        *MODELS.write().unwrap() = new;
        Ok(())
    }

    /// Download the face models in the background, then load them.
    pub fn start_face_download(self: &Arc<Self>) -> bool {
        {
            let mut d = DOWNLOAD.lock().unwrap();
            if d.running {
                return false;
            }
            *d = FaceDownload { running: true, done: 0, total: faces::DETECTOR.bytes + faces::RECOGNIZER.bytes, error: None };
        }
        let engine = self.clone();
        thread::spawn(move || {
            let r = faces::download_models(&engine.face_models_dir(), &|done, total| {
                let mut d = DOWNLOAD.lock().unwrap();
                d.done = done;
                d.total = total;
            });
            let r = r.and_then(|_| engine.reload_faces());
            let mut d = DOWNLOAD.lock().unwrap();
            d.running = false;
            if let Err(e) = r {
                warn!("faces: {e:#}");
                d.error = Some(super::download_failure("the face models", &e));
            }
        });
        true
    }

    pub fn face_status(&self) -> serde_json::Value {
        let c = self.config.lock().unwrap().faces.clone();
        let open = {
            let k = self.config.lock().unwrap();
            k.server.auth.is_none() && k.server.users.is_empty()
        };
        serde_json::json!({
            "settings": c,
            "models": faces::models_present(&self.face_models_dir()),
            "loaded": self.faces_loaded(),
            "download": DOWNLOAD.lock().unwrap().clone(),
            "ai": self.detector().is_some() || self.config.lock().unwrap().ai.url.as_deref().is_some_and(|u| !u.is_empty()),
            // No sign-in accounts: anyone on the network is an admin.
            "open": open,
        })
    }
}

/// Look for faces in a finished event that AI saw a person in.
/// `repeat_days`: also find out whether each unknown face was seen in an
/// earlier event within that many days (for first-time-seen alerts).
pub(super) fn recognize(engine: &Engine, camera: &str, event_id: &str, ai: &super::ai_check::AiOutcome, repeat_days: Option<u32>) -> FaceResult {
    let mut out = FaceResult::default();
    let Some(models) = MODELS.read().unwrap().clone() else { return out };
    let mut fc = engine.config.lock().unwrap().faces.clone();
    fc.sanitize();
    // The frames with the largest person in them (nearest the camera).
    let mut looks: Vec<(i64, Vec<crate::ai::Detection>, f32)> = ai
        .log
        .iter()
        .filter_map(|c| {
            let people: Vec<crate::ai::Detection> = c.seen.iter().filter(|d| d.label == "person" && d.score >= 0.5).cloned().collect();
            let biggest = people.iter().map(|d| d.w * d.h).fold(0.0f32, f32::max);
            (!people.is_empty()).then_some((c.at_ms, people, biggest))
        })
        .collect();
    looks.sort_by(|a, b| b.2.total_cmp(&a.2));
    looks.truncate(MAX_FRAMES);
    if looks.is_empty() {
        return out;
    }
    let refs: Vec<(i64, Vec<f32>)> = engine.db.labeled_faces().unwrap_or_default().into_iter().map(|(p, e)| (p, faces::emb_from_bytes(&e))).collect();
    // (fingerprint, size, score, at_ms, picture, match) of the faces kept.
    let mut kept: Vec<(Vec<f32>, u32, f32, i64, Vec<u8>, Option<(i64, f32)>)> = Vec::new();
    for (at_ms, people, _) in looks {
        let jpeg = match engine.still_at(camera, at_ms) {
            Ok(Some(j)) => j,
            Ok(None) => continue,
            Err(e) => {
                tracing::debug!("faces: no picture of {camera} at {at_ms}: {e:#}");
                continue;
            }
        };
        let Ok(img) = image::load_from_memory(&jpeg).map(|i| i.to_rgb8()) else { continue };
        let (iw, ih) = (img.width() as f32, img.height() as f32);
        for p in &people {
            // The person's box with a little room around it.
            let (mx, my) = (p.w * 0.1, p.h * 0.05);
            let x0 = ((p.x - mx) * iw).max(0.0) as u32;
            let y0 = ((p.y - my) * ih).max(0.0) as u32;
            let x1 = (((p.x + p.w + mx) * iw) as u32).min(img.width());
            let y1 = (((p.y + p.h + my) * ih) as u32).min(img.height());
            if x1 <= x0 + 8 || y1 <= y0 + 8 {
                continue;
            }
            let crop = image::imageops::crop_imm(&img, x0, y0, x1 - x0, y1 - y0).to_image();
            let found = match models.detect(&crop, MIN_FACE_SCORE) {
                Ok(f) => f,
                Err(e) => {
                    warn!("faces: {e:#}");
                    return out;
                }
            };
            let mut usable = 0;
            for mut f in found.into_iter().take(2) {
                // Back to the full picture.
                f.x += x0 as f32;
                f.y += y0 as f32;
                f.kps = f.kps.map(|(x, y)| (x + x0 as f32, y + y0 as f32));
                let size = f.w.round().max(0.0) as u32;
                if size < fc.min_face {
                    continue;
                }
                let Ok(emb) = models.embed(&img, &f) else { continue };
                let Ok(pic) = faces::face_jpeg(&img, &f, 128) else { continue };
                usable += 1;
                // Best named person.
                let mut best: Option<(i64, f32)> = None;
                for (pid, e) in &refs {
                    let s = faces::cosine(&emb, e);
                    if s >= fc.match_score && best.is_none_or(|b| s > b.1) {
                        best = Some((*pid, s));
                    }
                }
                // One entry per person per event: the largest face.
                let same = kept.iter().position(|k| match (k.5, best) {
                    (Some(a), Some(b)) => a.0 == b.0,
                    (None, None) => faces::cosine(&k.0, &emb) >= SAME_IN_EVENT,
                    _ => false,
                });
                match same {
                    Some(i) if kept[i].1 >= size => {}
                    Some(i) => kept[i] = (emb, size, f.score, at_ms, pic, best),
                    None => kept.push((emb, size, f.score, at_ms, pic, best)),
                }
            }
            if usable == 0 {
                out.faceless += 1;
            }
        }
    }
    static INSERTS: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
    // Unnamed faces of earlier events, for "seen before" (read before this
    // event's own faces are saved).
    let past: Vec<Vec<f32>> = match repeat_days {
        Some(d) if kept.iter().any(|k| k.5.is_none()) => engine
            .db
            .unnamed_face_embs_since(Utc::now().timestamp_millis() - i64::from(d) * 24 * 3600 * 1000, event_id)
            .unwrap_or_default()
            .iter()
            .map(|b| faces::emb_from_bytes(b))
            .collect(),
        _ => Vec::new(),
    };
    for (emb, size, score, at_ms, pic, best) in kept {
        if let Err(e) = engine.db.insert_face(event_id, &slug(camera), at_ms, score, size, &faces::emb_to_bytes(&emb), &pic, best) {
            warn!("faces: could not save: {e:#}");
            continue;
        }
        match best {
            Some((pid, _)) => {
                if let Ok(Some(n)) = engine.db.person_name(pid) {
                    if !out.names.contains(&n) {
                        out.names.push(n);
                        out.people.push(pid);
                    }
                }
            }
            None => {
                out.unknown += 1;
                if repeat_days.is_some() {
                    out.repeats.push(past.iter().any(|p| faces::cosine(p, &emb) >= fc.match_score));
                }
            }
        }
        if INSERTS.fetch_add(1, std::sync::atomic::Ordering::Relaxed) % 50 == 0 {
            let _ = engine.db.prune_faces(Utc::now().timestamp_millis() - KEEP_UNNAMED_MS);
        }
    }
    if !out.names.is_empty() || out.unknown > 0 {
        // Home Assistant: whose face it was (sensors "Last face", event "Face").
        // (Not again for people the live watch just announced.)
        if let Some(m) = engine.mqtt.get() {
            let fresh: Vec<String> = out.people.iter().zip(&out.names).filter(|(p, _)| claim_announce(camera, **p)).map(|(_, n)| n.clone()).collect();
            if !fresh.is_empty() || (out.names.is_empty() && out.unknown > 0) {
                m.publish_faces(camera, event_id, &fresh, out.unknown);
            }
        }
        info!("camera '{camera}': faces: {}{}", out.names.join(", "), if out.unknown > 0 { format!("{}{} unknown", if out.names.is_empty() { "" } else { ", " }, out.unknown) } else { String::new() });
    }
    out
}

/// Whether recognition should keep this event from alerting: every person
/// AI saw is accounted for by a recognized quiet person (no unknown face, no
/// person whose face could not be seen, never more people at once than were
/// recognized), and AI saw nothing else it alerts for.
pub(super) fn quiet_only(engine: &Engine, r: &FaceResult, ai: &super::ai_check::AiOutcome) -> bool {
    if r.people.is_empty() {
        return false;
    }
    quiet_decision(r, ai, &engine.db.quiet_people().unwrap_or_default())
}

fn quiet_decision(r: &FaceResult, ai: &super::ai_check::AiOutcome, quiet: &[i64]) -> bool {
    if r.people.is_empty() || r.unknown > 0 || r.faceless > 0 {
        return false;
    }
    let most_at_once = ai.log.iter().map(|c| c.seen.iter().filter(|d| d.label == "person" && d.score >= 0.5).count()).max().unwrap_or(0);
    if most_at_once > r.people.len() {
        return false;
    }
    let others = ai.labels.iter().any(|l| l != "person" && ai.wanted.contains(l));
    !others && r.people.iter().all(|p| quiet.contains(p))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ai::Detection;

    fn look(people: usize) -> super::super::ai_check::AiCheck {
        let p = Detection { label: "person".into(), score: 0.9, x: 0.1, y: 0.1, w: 0.1, h: 0.3 };
        super::super::ai_check::AiCheck { seen: vec![p; people], ..Default::default() }
    }

    fn ai(looks: &[usize]) -> super::super::ai_check::AiOutcome {
        super::super::ai_check::AiOutcome {
            labels: vec!["person".into()],
            ran: true,
            confirmed: true,
            log: looks.iter().map(|&n| look(n)).collect(),
            wanted: vec!["person".into(), "car".into()],
            ..Default::default()
        }
    }

    #[test]
    fn quiet_person_alone_is_quiet_but_not_with_anyone_else() {
        let named = FaceResult { names: vec!["A".into()], people: vec![7], unknown: 0, faceless: 0, repeats: vec![] };
        assert!(quiet_decision(&named, &ai(&[1, 1]), &[7]));
        // Not marked quiet.
        assert!(!quiet_decision(&named, &ai(&[1]), &[]));
        // A second person whose face was not seen (two people in one look).
        assert!(!quiet_decision(&named, &ai(&[1, 2]), &[7]));
        // A person box without a usable face.
        let turned = FaceResult { faceless: 1, ..FaceResult { names: vec!["A".into()], people: vec![7], unknown: 0, faceless: 0, repeats: vec![] } };
        assert!(!quiet_decision(&turned, &ai(&[1]), &[7]));
        // An unknown face as well.
        let stranger = FaceResult { unknown: 1, ..FaceResult { names: vec!["A".into()], people: vec![7], unknown: 0, faceless: 0, repeats: vec![] } };
        assert!(!quiet_decision(&stranger, &ai(&[1]), &[7]));
        // AI also saw a car it alerts for.
        let mut with_car = ai(&[1]);
        with_car.labels.push("car".into());
        assert!(!quiet_decision(&named, &with_car, &[7]));
    }
}

/// Import limits: pictures per import, bytes per picture, pixels per side.
const MAX_IMPORT_FILES: usize = 2000;
const MAX_IMPORT_BYTES: u64 = 20 * 1024 * 1024;
const MAX_IMPORT_SIDE: u32 = 8000;

/// Decode a picture with limits (a huge or crafted file cannot exhaust memory).
fn open_limited(p: &std::path::Path) -> anyhow::Result<image::RgbImage> {
    let mut r = image::io::Reader::open(p)?.with_guessed_format()?;
    let mut l = image::io::Limits::default();
    l.max_image_width = Some(MAX_IMPORT_SIDE);
    l.max_image_height = Some(MAX_IMPORT_SIDE);
    l.max_alloc = Some(256 * 1024 * 1024);
    r.limits(l);
    Ok(r.decode()?.to_rgb8())
}

/// What an import of named faces did.
#[derive(Default, serde::Serialize)]
pub struct ImportReport {
    pub people: Vec<String>,
    pub faces: usize,
    /// Pictures where no clear face was found.
    pub skipped: Vec<String>,
}

impl Engine {
    /// Name faces from pictures on this server (see [`import_named_faces`]).
    pub fn import_named_faces(&self, dir: &std::path::Path) -> anyhow::Result<ImportReport> {
        let models = match MODELS.read().unwrap().clone() {
            Some(m) => m,
            None if faces::models_present(&self.face_models_dir()) => Arc::new(FaceModels::load(&self.face_models_dir(), 2)?),
            None => anyhow::bail!("download the face models first"),
        };
        import_named_faces(&self.db, &models, dir)
    }
}

/// Name faces from pictures: `dir` holds one folder per person (the
/// folder's name is theirs) with pictures of their face. Loose pictures
/// directly in `dir` are left alone (nobody named them).
pub fn import_named_faces(db: &crate::db::Db, models: &FaceModels, dir: &std::path::Path) -> anyhow::Result<ImportReport> {
    let mut report = ImportReport::default();
    let mut seen_files = 0usize;
    let mut folders: Vec<_> = std::fs::read_dir(dir)
        .map_err(|e| anyhow::anyhow!("cannot read {}: {e}", dir.display()))?
        .filter_map(|e| e.ok())
        .filter(|e| e.file_type().is_ok_and(|t| t.is_dir()))
        .collect();
    folders.sort_by_key(|e| e.file_name());
    for f in folders {
        let name = f.file_name().to_string_lossy().trim().to_string();
        if name.is_empty() || name.starts_with('.') || name.chars().count() > 60 {
            continue;
        }
        // Plain files only (no links out of the folder), a sensible number.
        let mut pics: Vec<_> = std::fs::read_dir(f.path())?
            .filter_map(|e| e.ok())
            .filter(|e| e.file_type().is_ok_and(|t| t.is_file()))
            .filter(|e| e.metadata().is_ok_and(|m| m.len() <= MAX_IMPORT_BYTES))
            .map(|e| e.path())
            .collect();
        pics.sort();
        let mut pid: Option<i64> = None;
        for p in pics {
            let ext = p.extension().map(|x| x.to_string_lossy().to_ascii_lowercase()).unwrap_or_default();
            if !matches!(ext.as_str(), "jpg" | "jpeg" | "png" | "bmp" | "webp") {
                continue;
            }
            let label = format!("{name}/{}", p.file_name().unwrap_or_default().to_string_lossy());
            if seen_files >= MAX_IMPORT_FILES {
                break;
            }
            seen_files += 1;
            let Ok(img) = open_limited(&p) else {
                report.skipped.push(label);
                continue;
            };
            // The clearest face in the picture.
            let Some(face) = models.detect(&img, 0.6)?.into_iter().max_by(|a, b| (a.w * a.h * a.score).total_cmp(&(b.w * b.h * b.score))) else {
                report.skipped.push(label);
                continue;
            };
            let emb = models.embed(&img, &face)?;
            let pic = faces::face_jpeg(&img, &face, 128)?;
            let person = match pid {
                Some(x) => x,
                None => {
                    let x = db.person_id(&name, Utc::now().timestamp_millis())?;
                    pid = Some(x);
                    x
                }
            };
            let at = std::fs::metadata(&p).and_then(|m| m.modified()).ok().and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok()).map(|d| d.as_millis() as i64).unwrap_or(0);
            let id = db.insert_face("imported", "", at, face.score, face.w.round() as u32, &faces::emb_to_bytes(&emb), &pic, None)?;
            db.label_faces(&[id], Some(person))?;
            report.faces += 1;
            if !report.people.contains(&name) {
                report.people.push(name.clone());
            }
        }
    }
    info!("faces: imported {} faces of {} people from {}", report.faces, report.people.len(), dir.display());
    Ok(report)
}
