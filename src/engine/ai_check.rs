//! AI confirmation of motion events: while an event is active, check a few
//! frames for objects of interest (built-in detector or an HTTP AI server).

use super::*;
use crate::ai::Detection;

/// Detections down to this score are kept for AI review (near misses), even
/// though only those over the camera's minimum count.
const REVIEW_FLOOR: f32 = 0.25;
/// Frames bigger than this are not kept for review.
const REVIEW_FRAME_MAX: usize = 300 * 1024;
/// Package cameras: looks per event at most, and their spacing once a
/// person is confirmed.
const FOLLOW_MAX_CHECKS: u32 = 18;
const FOLLOW_GAP_MS: u64 = 1500;

/// One AI look at a frame, kept for AI review.
#[derive(Clone, Debug, Default)]
pub(super) struct AiCheck {
    pub at_ms: i64,
    /// Everything seen down to REVIEW_FLOOR.
    pub seen: Vec<Detection>,
    pub error: Option<String>,
    pub frame: Vec<u8>,
    /// For each of `seen`: an object that has not moved (parked, or in a
    /// remembered spot), which does not count.
    pub parked: Vec<bool>,
    /// For each of `seen`: held back by a remembered spot (see `spots`).
    pub spot: Vec<Option<super::spots::Held>>,
}

/// Where a camera has seen objects lately, to tell a parked car from one
/// that just arrived. Kept across events.
#[derive(Default)]
pub(super) struct Parked {
    spots: Vec<Spot>,
}

struct Spot {
    label: String,
    x: f32,
    y: f32,
    w: f32,
    h: f32,
    first: std::time::Instant,
    last: std::time::Instant,
}

/// An object that has sat in the same place this long counts as parked.
const PARKED_AFTER: std::time::Duration = std::time::Duration::from_secs(180);
/// Things that stay where they are left (vehicles of every kind).
pub(super) fn is_vehicle(label: &str) -> bool {
    crate::tracker::is_vehicle(label) || matches!(label, "bicycle" | "boat" | "train" | "airplane")
}
use crate::tracker::same_kind;

/// A person seen again after a longer gap than this counts as arriving anew.
const STILL_THERE: std::time::Duration = std::time::Duration::from_secs(90);
/// A spot not seen for this long is forgotten (the car has left).
const SPOT_FORGET: std::time::Duration = std::time::Duration::from_secs(1800);
/// Boxes overlapping this much (intersection over union) are the same place.
const SAME_PLACE: f32 = 0.6;

fn iou(a: (f32, f32, f32, f32), b: (f32, f32, f32, f32)) -> f32 {
    let (ax2, ay2, bx2, by2) = (a.0 + a.2, a.1 + a.3, b.0 + b.2, b.1 + b.3);
    let iw = (ax2.min(bx2) - a.0.max(b.0)).max(0.0);
    let ih = (ay2.min(by2) - a.1.max(b.1)).max(0.0);
    let inter = iw * ih;
    let union = a.2 * a.3 + b.2 * b.3 - inter;
    if union <= 0.0 { 0.0 } else { inter / union }
}

impl Parked {
    /// Note a detection at `now`; true if it has been in this place for
    /// PARKED_AFTER or longer.
    pub fn seen(&mut self, d: &Detection, now: std::time::Instant) -> bool {
        self.spots.retain(|s| now.duration_since(s.last) < SPOT_FORGET);
        if d.w <= 0.0 || d.h <= 0.0 {
            return false; // the camera's own report: no box
        }
        let b = (d.x, d.y, d.w, d.h);
        match self.spots.iter_mut().filter(|s| same_kind(&s.label, &d.label)).max_by(|p, q| iou((p.x, p.y, p.w, p.h), b).total_cmp(&iou((q.x, q.y, q.w, q.h), b))) {
            Some(s) if iou((s.x, s.y, s.w, s.h), b) >= SAME_PLACE => {
                // A person counts as parked only when seen all along (someone
                // at a desk while a fan keeps motion going): after a gap it is
                // someone new in the same spot, e.g. a second visitor at the
                // door. A vehicle is only seen when motion happens elsewhere,
                // so sightings minutes apart still mean the same parked car.
                if !is_vehicle(&d.label) && now.duration_since(s.last) > STILL_THERE {
                    s.first = now;
                }
                s.last = now;
                now.duration_since(s.first) >= PARKED_AFTER
            }
            _ => {
                if self.spots.len() >= 64 {
                    self.spots.sort_by_key(|s| std::cmp::Reverse(s.last));
                    self.spots.truncate(48);
                }
                self.spots.push(Spot { label: d.label.clone(), x: d.x, y: d.y, w: d.w, h: d.h, first: now, last: now });
                false
            }
        }
    }
}

/// The parked memory outlives the camera's worker and the server: kept per
/// camera here (a camera that reconnects keeps it) and saved in the
/// database every minute (a restart keeps it; before, every restart made
/// each parked car "arrive" again and alert).
#[derive(Clone, serde::Serialize, serde::Deserialize)]
struct SavedSpot {
    label: String,
    x: f32,
    y: f32,
    w: f32,
    h: f32,
    first_ms: i64,
    last_ms: i64,
}

struct ParkedStore {
    cams: std::collections::HashMap<String, Vec<SavedSpot>>,
    saved_at: Option<std::time::Instant>,
}

static PARKED_STORE: std::sync::Mutex<Option<ParkedStore>> = std::sync::Mutex::new(None);
const PARKED_KV: (&str, &str) = ("server", "parked");
const PARKED_SAVE_EVERY: std::time::Duration = std::time::Duration::from_secs(60);

fn with_store<R>(engine: &Engine, f: impl FnOnce(&mut ParkedStore) -> R) -> R {
    let mut g = PARKED_STORE.lock().unwrap();
    let st = g.get_or_insert_with(|| ParkedStore {
        cams: engine.db.ui_get(PARKED_KV.0, PARKED_KV.1).ok().flatten().and_then(|s| serde_json::from_str(&s).ok()).unwrap_or_default(),
        saved_at: None,
    });
    f(st)
}

impl Parked {
    fn to_saved(&self) -> Vec<SavedSpot> {
        let (now, wall) = (std::time::Instant::now(), Utc::now().timestamp_millis());
        let ms = |t: std::time::Instant| wall - now.duration_since(t).as_millis() as i64;
        self.spots.iter().map(|s| SavedSpot { label: s.label.clone(), x: s.x, y: s.y, w: s.w, h: s.h, first_ms: ms(s.first), last_ms: ms(s.last) }).collect()
    }
    fn from_saved(v: &[SavedSpot]) -> Parked {
        let (now, wall) = (std::time::Instant::now(), Utc::now().timestamp_millis());
        let at = |ms: i64| now.checked_sub(std::time::Duration::from_millis((wall - ms).max(0) as u64)).unwrap_or(now);
        let spots = v
            .iter()
            .filter(|s| wall - s.last_ms < SPOT_FORGET.as_millis() as i64)
            .map(|s| Spot { label: s.label.clone(), x: s.x, y: s.y, w: s.w, h: s.h, first: at(s.first_ms), last: at(s.last_ms) })
            .collect();
        Parked { spots }
    }
}

/// The parked memory of a camera, as last kept (for a starting worker).
pub(super) fn restore_parked(engine: &Engine, camera: &str) -> Parked {
    with_store(engine, |st| st.cams.get(camera).map(|v| Parked::from_saved(v)).unwrap_or_default())
}

/// Keep a camera's parked memory (saved to the database at most once a minute).
fn keep_parked(engine: &Engine, camera: &str, p: &Parked) {
    let json = with_store(engine, |st| {
        st.cams.insert(camera.to_string(), p.to_saved());
        if st.saved_at.is_some_and(|t| t.elapsed() < PARKED_SAVE_EVERY) {
            return None;
        }
        st.saved_at = Some(std::time::Instant::now());
        serde_json::to_string(&st.cams).ok()
    });
    if let Some(j) = json {
        if let Err(e) = engine.db.ui_put(PARKED_KV.0, PARKED_KV.1, &j) {
            warn!("AI: could not save where objects are parked: {e:#}");
        }
    }
}

/// Stricter confirmation: the same object must look like it moved by at
/// least this much (center shift as a fraction of the frame, or a size
/// change) between two sightings ...
const STRICT_MIN_MOVE: f32 = 0.01;
const STRICT_MIN_GROWTH: f32 = 0.10;
/// ... and be no further apart than this to count as the same object.
const STRICT_SAME_OBJECT: f32 = 0.3;

fn center(d: &Detection) -> (f32, f32) {
    (d.x + d.w / 2.0, d.y + d.h / 2.0)
}

/// Whether `now` is the object `before` was, having moved since.
fn moved_on(before: &Detection, now: &Detection) -> bool {
    if before.label != now.label {
        return false;
    }
    let ((ax, ay), (bx, by)) = (center(before), center(now));
    let dist = ((ax - bx).powi(2) + (ay - by).powi(2)).sqrt();
    let (a1, a2) = (before.w * before.h, now.w * now.h);
    let growth = if a1 > 0.0 { (a2 / a1 - 1.0).abs() } else { 0.0 };
    dist <= STRICT_SAME_OBJECT && (dist >= STRICT_MIN_MOVE || growth >= STRICT_MIN_GROWTH)
}

/// Stricter confirmation: the wanted objects in `now` that were also seen,
/// moving, in one of the `earlier` checks (the two before it).
pub(super) fn strict_confirmed<'a>(now: &'a [Detection], earlier: &[&[Detection]]) -> Vec<&'a Detection> {
    now.iter().filter(|d| earlier.iter().any(|e| e.iter().any(|p| moved_on(p, d)))).collect()
}

/// Per-camera AI progress for the event in progress.
#[derive(Default)]
pub(super) struct AiState {
    /// Every detection above the score threshold during this event.
    pub found: Vec<Detection>,
    /// Sure detections a remembered spot held back: they label the event
    /// (it is still found by them) but do not confirm it.
    pub held: Vec<Detection>,
    pub checks: u32,
    pub last: Option<std::time::Instant>,
    pub log: Vec<AiCheck>,
    /// Labels the camera itself reported (its own detection).
    pub camera_labels: Vec<String>,
    /// Where objects have been sitting (kept across events).
    pub parked: Parked,
    /// Stricter confirmation: the wanted objects each check counted, for the
    /// checks that worked (newest last).
    pub sightings: Vec<Vec<Detection>>,
    /// Stricter confirmation was on for this event.
    pub strict: bool,
    /// A loud sound (the camera's sound trigger) was part of this event.
    pub sound: bool,
    /// Named zones: what is tracked (kept across events).
    pub zones: super::zone_watch::Runtime,
    /// This event's number for the remembered spots (0: no look yet).
    pub spot_event: u64,
    /// The camera's own detection of a person, vehicle or animal (no box):
    /// confirms only if AI sees that kind of object too, or AI could not look.
    pub camera_found: Vec<Detection>,
    /// For each of `camera_found`: how many looks there were when it came.
    pub camera_since: Vec<usize>,
}

/// Whether any look saw something of `label`'s kind with a box, down to
/// the review floor (also a weak one, also one a remembered spot held).
fn ai_saw(log: &[AiCheck], label: &str) -> bool {
    let g = super::spots::group(label);
    log.iter().any(|c| c.seen.iter().any(|d| d.w > 0.0 && d.score >= REVIEW_FLOOR && super::spots::group(&d.label) == g))
}

/// Whether the camera's own detection of `label` waits for AI to agree:
/// people, vehicles and animals, on a camera with AI on and the setting on.
/// (Doorbell presses, sounds, packages and other labels confirm as before.)
pub(super) fn camera_label_needs_ai(engine: &Engine, cfg: &CameraConfig, label: &str) -> bool {
    matches!(super::spots::group(label), "person" | "vehicle" | "animal") && cfg.ai_confirm_camera == Some(true) && ai_active(engine, cfg)
}

impl AiState {
    /// The camera itself reported `d` (a person, vehicle or animal): AI gets
    /// a fresh set of looks to see it too.
    pub fn add_camera_label(&mut self, d: Detection) {
        if !self.camera_found.iter().any(|c| c.label == d.label) {
            self.camera_found.push(d);
            self.camera_since.push(self.log.len());
            self.checks = 0;
        }
    }

    /// The camera reported something AI has not seen yet.
    pub fn camera_pending(&self) -> bool {
        self.camera_found.iter().any(|c| !ai_saw(&self.log, &c.label))
    }

    /// Whether another look is due (the count only; spacing is separate).
    /// `packages`: the camera follows a confirmed person (package detection).
    pub fn may_look(&self, wanted: &[String], max_checks: u32, packages: bool) -> bool {
        let confirmed = self.found.iter().any(|d| wanted.contains(&d.label));
        let follow = confirmed && packages;
        // While the camera reports something AI has not seen, it keeps
        // looking longer: the object may come into view only later.
        let limit = if follow || (!confirmed && self.camera_pending()) { max_checks.max(FOLLOW_MAX_CHECKS) } else { max_checks };
        self.checks < limit && (!confirmed || follow)
    }

    pub fn labels(&self) -> Vec<String> {
        // Unique labels, most confident first.
        let mut d = self.found.clone();
        d.extend(self.held.iter().cloned());
        d.extend(self.camera_found.iter().cloned());
        d.sort_by(|a, b| b.score.total_cmp(&a.score));
        let mut out: Vec<String> = Vec::new();
        for x in d {
            if !out.contains(&x.label) {
                out.push(x.label);
            }
        }
        out
    }
}

/// What AI concluded about a finished event.
#[derive(Default, Clone, Debug)]
pub(super) struct AiOutcome {
    pub labels: Vec<String>,
    /// AI checked this event (so `confirmed` is meaningful).
    pub ran: bool,
    /// A wanted object was seen.
    pub confirmed: bool,
    /// The checks, for AI review.
    pub log: Vec<AiCheck>,
    pub wanted: Vec<String>,
    pub min_score: f32,
    pub camera_labels: Vec<String>,
    pub strict: bool,
    /// A loud sound was part of the event: it alerts whatever AI saw.
    pub sound: bool,
    /// What the camera itself reported that AI did not see (so it did not confirm).
    pub camera_unconfirmed: Vec<String>,
}

impl AiOutcome {
    /// Close out the event's AI state (resetting it for the next event).
    pub fn take(state: &mut AiState, wanted: &[String]) -> AiOutcome {
        // The memory of where objects sit outlives the event.
        let parked = std::mem::take(&mut state.parked);
        let zones = std::mem::take(&mut state.zones);
        let st = std::mem::take(state);
        state.parked = parked;
        state.zones = zones;
        let labels = st.labels();
        // Checks that failed (model error, AI server down) do not count:
        // with none that worked, the event alerts as if AI were off.
        let ran = st.log.iter().any(|c| c.error.is_none()) || (st.checks > 0 && st.log.is_empty());
        let mut found = st.found;
        // The camera's own detection: AI agrees when any look saw that kind
        // of object (even weakly, even where a remembered spot held it); and
        // with no look that worked after the camera's report, the report counts.
        let mut camera_unconfirmed = Vec::new();
        for (c, since) in st.camera_found.into_iter().zip(st.camera_since) {
            let looked_after = st.log.get(since..).is_some_and(|l| l.iter().any(|x| x.error.is_none()));
            if !ran || !looked_after || ai_saw(&st.log, &c.label) {
                found.push(c);
            } else {
                camera_unconfirmed.push(c.label);
            }
        }
        AiOutcome {
            ran,
            confirmed: found.iter().any(|d| wanted.contains(&d.label)),
            labels,
            log: st.log,
            wanted: wanted.to_vec(),
            min_score: 0.0,
            camera_labels: st.camera_labels,
            strict: st.strict,
            sound: st.sound,
            camera_unconfirmed,
        }
    }

    /// The saved review record (frames are stored separately as n.jpg).
    pub fn review_json(&self, event_start: i64, source: &str, alert: &serde_json::Value) -> serde_json::Value {
        let start_ms = event_start * 1000;
        serde_json::json!({
            "version": 1,
            "source": source,
            "min_score": crate::config::f32_json(self.min_score),
            "strict": self.strict,
            "wanted": self.wanted,
            "confirmed": self.confirmed,
            "labels": self.labels,
            "camera_labels": self.camera_labels,
            "camera_unconfirmed": self.camera_unconfirmed,
            "sound": self.sound,
            "alert": alert,
            "checks": self.log.iter().enumerate().map(|(i, c)| serde_json::json!({
                "n": i,
                "offset_ms": c.at_ms - start_ms,
                "frame": !c.frame.is_empty(),
                "error": c.error,
                "seen": c.seen,
                "parked": c.parked,
                "spot": c.spot,
            })).collect::<Vec<_>>(),
        })
    }
}

/// Labels that confirm events for this camera.
pub(super) fn wanted_labels(engine: &Engine, cfg: &CameraConfig) -> Vec<String> {
    cfg.ai_labels.clone().unwrap_or_else(|| engine.config.lock().unwrap().ai.labels.clone())
}

/// Whether AI checks apply to this camera right now.
pub(super) fn ai_active(engine: &Engine, cfg: &CameraConfig) -> bool {
    let configured = {
        // Released before the profile lookup below, which locks config too.
        let c = engine.config.lock().unwrap();
        c.ai.enabled && (engine.detector().is_some() || c.ai.url.as_deref().is_some_and(|u| !u.is_empty()))
    };
    configured && engine.ai_enabled(cfg)
}

/// Check one frame during an event if it is time to (about twice a second,
/// up to `max_checks`, until a wanted object has been seen).
pub(super) fn maybe_check(engine: &Engine, cfg: &CameraConfig, state: &mut AiState, jpeg: &[u8]) {
    let (max_checks, min_score, ignore_parked, strict, model, server) = {
        let c = engine.config.lock().unwrap();
        (
            c.ai.max_checks,
            cfg.ai_min_score.unwrap_or(c.ai.min_score),
            cfg.ai_ignore_parked.unwrap_or(c.ai.ignore_parked),
            cfg.ai_strict.unwrap_or(c.ai.strict),
            c.ai.model.as_ref().and_then(|m| m.file_name()).map(|f| f.to_string_lossy().into_owned()),
            c.ai.url.clone(),
        )
    };
    state.strict = strict;
    let wanted = wanted_labels(engine, cfg);
    // Package detection keeps looking (slower) after a person is confirmed:
    // whether they walk up to the door or away shows over several looks.
    let confirmed = state.found.iter().any(|d| wanted.contains(&d.label));
    let follow = confirmed && cfg.packages == Some(true);
    if !state.may_look(&wanted, max_checks, cfg.packages == Some(true)) {
        return;
    }
    let gap = if follow { FOLLOW_GAP_MS } else { 450 };
    if state.last.is_some_and(|t| t.elapsed() < std::time::Duration::from_millis(gap)) {
        return;
    }
    state.last = Some(std::time::Instant::now());
    state.checks += 1;
    let mut check = AiCheck {
        at_ms: Utc::now().timestamp_millis(),
        frame: if jpeg.len() <= REVIEW_FRAME_MAX { jpeg.to_vec() } else { Vec::new() },
        ..Default::default()
    };
    match engine.ai_detect(jpeg, min_score.min(REVIEW_FLOOR)) {
        Ok(d) => {
            // Objects that have not moved for minutes (a parked car) do not count.
            // Every sighting, also the unsure ones, keeps the memory of where
            // things sit: a car across the street scored 0.3-0.5 in most
            // looks, so only remembering sure ones made each sure look a
            // new arrival.
            let now = std::time::Instant::now();
            check.parked = d.iter().map(|x| ignore_parked && state.parked.seen(x, now) && x.score >= min_score).collect();
            keep_parked(engine, &cfg.name, &state.parked);
            // Things seen in the same spot event after event (a shrub, a
            // bin, a car parked between events) do not count either.
            // (With "ignore objects that have not moved" off, only the
            // owner's "not a person" marks hold anything back.)
            if state.spot_event == 0 {
                state.spot_event = super::spots::next_event();
                // Spots are learned from one model's boxes: another model starts them over.
                let who = if engine.detector().is_some() { format!("model {}", model.unwrap_or_default()) } else { format!("server {}", server.unwrap_or_default()) };
                super::spots::model_in_use(&engine.db, &who);
            }
            let groups: Vec<&str> = wanted.iter().map(|w| super::spots::group(w)).collect();
            let (ev, at) = (state.spot_event, check.at_ms);
            check.spot = super::spots::with(&engine.db, &cfg.name, false, |s| s.look(&d, min_score, &|g| groups.contains(&g), ignore_parked, at, ev));
            for ((p, h), x) in check.parked.iter_mut().zip(&check.spot).zip(&d) {
                if h.is_some() {
                    if !*p && x.score >= min_score && wanted.contains(&x.label) && !state.held.iter().any(|y| y.label == x.label) {
                        state.held.push(x.clone());
                    }
                    *p = true;
                }
            }
            let counted: Vec<Detection> = d.iter().zip(&check.parked).filter(|(x, p)| x.score >= min_score && !**p).map(|(x, _)| x.clone()).collect();
            if strict {
                // Wanted objects confirm only when seen, moving, in 2 of 3
                // checks in a row; other objects are noted as before.
                let (want, other): (Vec<Detection>, Vec<Detection>) = counted.into_iter().partition(|x| wanted.contains(&x.label));
                let earlier: Vec<&[Detection]> = state.sightings.iter().rev().take(2).map(|v| v.as_slice()).collect();
                let ok: Vec<Detection> = strict_confirmed(&want, &earlier).into_iter().cloned().collect();
                state.found.extend(other);
                state.found.extend(ok);
                state.sightings.push(want);
            } else {
                state.found.extend(counted);
            }
            // Confirmed: the looks stop here, and a car parked in a spot
            // seen may leave unseen (someone walking up to it and driving off).
            if !confirmed && state.found.iter().any(|d| wanted.contains(&d.label)) && cfg.packages != Some(true) {
                super::spots::with(&engine.db, &cfg.name, false, |s| s.looks_cut_short(ev));
            }
            check.seen = d;
        }
        Err(e) => {
            warn!("camera '{}': AI check failed: {e:#}", cfg.name);
            check.error = Some(format!("{e:#}"));
        }
    }
    state.log.push(check);
}

/// Which AI did the checks, for the review ("built-in Small", "AI server").
pub(super) fn source_name(engine: &Engine) -> String {
    if engine.detector().is_some() {
        let model = engine.config.lock().unwrap().ai.model.clone();
        let name = model.as_deref().and_then(crate::ai_setup::identify).map(|m| m.name).unwrap_or("custom");
        format!("built-in ({name} model)")
    } else {
        "AI server".into()
    }
}

impl Engine {
    /// Detect objects with the built-in model, or the configured AI server.
    pub fn ai_detect(&self, jpeg: &[u8], min_score: f32) -> anyhow::Result<Vec<Detection>> {
        let ticket = crate::telemetry::AiTicket::begin();
        let r = self.ai_detect_inner(jpeg, min_score);
        ticket.finish(r.is_ok());
        r
    }

    fn ai_detect_inner(&self, jpeg: &[u8], min_score: f32) -> anyhow::Result<Vec<Detection>> {
        if let Some(d) = self.detector() {
            return d.detect(jpeg, min_score);
        }
        let url = self.config.lock().unwrap().ai.url.clone().unwrap_or_default();
        if url.is_empty() {
            anyhow::bail!("no AI model or server configured");
        }
        http_detect(&url, jpeg, min_score, &self.data_dir)
    }
}

/// POST the image to `{url}/v1/vision/detection` (multipart field `image`)
/// and read `predictions: [{label, confidence, x_min, y_min, x_max, y_max}]`.
fn http_detect(url: &str, jpeg: &[u8], min_score: f32, data_dir: &std::path::Path) -> anyhow::Result<Vec<Detection>> {
    let (w, h) = image::io::Reader::new(std::io::Cursor::new(jpeg))
        .with_guessed_format()?
        .into_dimensions()?;
    let tmp_dir = data_dir.join("tmp");
    std::fs::create_dir_all(&tmp_dir)?;
    let tmp = tmp_dir.join(format!("ai-{}-{}.jpg", std::process::id(), Utc::now().timestamp_nanos_opt().unwrap_or(0)));
    std::fs::write(&tmp, jpeg)?;
    let endpoint = format!("{}/v1/vision/detection", url.trim_end_matches('/'));
    // (Settings on curl's input, not its command line: the address may
    // carry a sign-in.)
    let args = vec![
        "-s".to_string(),
        "-m".into(),
        "10".into(),
        "-F".into(),
        format!("image=@{}", tmp.to_string_lossy()),
        "-F".into(),
        format!("min_confidence={min_score}"),
        endpoint.clone(),
    ];
    let out = crate::child::curl(&args);
    let _ = std::fs::remove_file(&tmp);
    let out = out.map_err(|e| anyhow::anyhow!("could not run curl: {e}"))?;
    let v: serde_json::Value = serde_json::from_slice(&out.stdout)
        .map_err(|_| anyhow::anyhow!("AI server at {endpoint} did not answer with JSON"))?;
    Ok(parse_predictions(&v, w as f32, h as f32, min_score))
}

pub(super) fn parse_predictions(v: &serde_json::Value, w: f32, h: f32, min_score: f32) -> Vec<Detection> {
    let mut out = Vec::new();
    for p in v["predictions"].as_array().into_iter().flatten() {
        let score = p["confidence"].as_f64().unwrap_or(0.0) as f32;
        if score < min_score {
            continue;
        }
        let f = |k: &str| p[k].as_f64().unwrap_or(0.0) as f32;
        let (x0, y0, x1, y1) = (f("x_min"), f("y_min"), f("x_max"), f("y_max"));
        out.push(Detection {
            label: p["label"].as_str().unwrap_or("object").to_string(),
            score,
            x: (x0 / w).clamp(0.0, 1.0),
            y: (y0 / h).clamp(0.0, 1.0),
            w: ((x1 - x0) / w).clamp(0.0, 1.0),
            h: ((y1 - y0) / h).clamp(0.0, 1.0),
        });
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parked_memory_round_trips_with_its_times() {
        let mut p = Parked::default();
        let t0 = std::time::Instant::now();
        let car = Detection { label: "car".into(), score: 0.9, x: 0.1, y: 0.1, w: 0.2, h: 0.1 };
        p.seen(&car, t0);
        p.spots[0].first = t0 - std::time::Duration::from_secs(600);
        let mut back = Parked::from_saved(&p.to_saved());
        assert_eq!(back.spots.len(), 1);
        let age = back.spots[0].first.elapsed().as_secs();
        assert!((599..=601).contains(&age), "{age}");
        assert!(back.seen(&car, std::time::Instant::now()), "still parked after a restart");
        // Forgotten spots are not brought back.
        let mut old = p.to_saved();
        old[0].last_ms -= SPOT_FORGET.as_millis() as i64 + 1000;
        assert!(Parked::from_saved(&old).spots.is_empty());
    }

    #[test]
    fn parses_http_ai_predictions() {
        let v = serde_json::json!({"success": true, "predictions": [
            {"label": "person", "confidence": 0.91, "x_min": 64, "y_min": 48, "x_max": 128, "y_max": 240},
            {"label": "cat", "confidence": 0.2, "x_min": 0, "y_min": 0, "x_max": 10, "y_max": 10}
        ]});
        let d = parse_predictions(&v, 640.0, 480.0, 0.5);
        assert_eq!(d.len(), 1);
        assert_eq!(d[0].label, "person");
        assert!((d[0].x - 0.1).abs() < 1e-6 && (d[0].h - 0.4).abs() < 1e-6);
    }

    #[test]
    fn parked_objects_after_a_few_minutes() {
        let d = |x: f32| Detection { label: "car".into(), score: 0.9, x, y: 0.5, w: 0.2, h: 0.2 };
        let mut p = Parked::default();
        let t0 = std::time::Instant::now();
        assert!(!p.seen(&d(0.10), t0), "new");
        assert!(!p.seen(&d(0.11), t0 + std::time::Duration::from_secs(60)), "same place, 1 min");
        assert!(!p.seen(&d(0.11), t0 + std::time::Duration::from_secs(120)), "same place, 2 min");
        assert!(p.seen(&d(0.11), t0 + std::time::Duration::from_secs(200)), "same place, over 3 min, seen all along: parked");
        // A car seen only now and then (when motion happens elsewhere) is
        // still the same parked car.
        let mut q = Parked::default();
        assert!(!q.seen(&d(0.10), t0));
        assert!(q.seen(&d(0.10), t0 + std::time::Duration::from_secs(300)), "a car still there 5 min later: parked");
        // A person in the same doorstep spot later, unseen in between, is new.
        let pd = |x: f32| Detection { label: "person".into(), score: 0.9, x, y: 0.3, w: 0.1, h: 0.4 };
        let mut r = Parked::default();
        assert!(!r.seen(&pd(0.40), t0));
        assert!(!r.seen(&pd(0.40), t0 + std::time::Duration::from_secs(300)), "a person back after 5 min unseen: a new arrival");
        // Seen all along (every 30 s), a person does count as parked.
        let mut sit = Parked::default();
        for k in 0..8 {
            let parked = sit.seen(&pd(0.40), t0 + std::time::Duration::from_secs(30 * k));
            assert_eq!(parked, 30 * k >= 180, "sitting {} s", 30 * k);
        }
        assert!(!p.seen(&d(0.60), t0 + std::time::Duration::from_secs(200)), "another car elsewhere: new");
        // Gone for over half an hour: a car back in that spot counts again.
        assert!(!p.seen(&d(0.10), t0 + std::time::Duration::from_secs(200 + 1900)));
        // The detector calling the same parked vehicle a car, then a truck:
        // still the same parked vehicle.
        let mut v = Parked::default();
        let lab = |l: &str| Detection { label: l.into(), score: 0.4, x: 0.30, y: 0.54, w: 0.11, h: 0.05 };
        assert!(!v.seen(&lab("car"), t0));
        assert!(!v.seen(&lab("truck"), t0 + std::time::Duration::from_secs(100)));
        assert!(v.seen(&lab("truck"), t0 + std::time::Duration::from_secs(200)), "car then truck in one spot: parked");
        assert!(v.seen(&lab("car"), t0 + std::time::Duration::from_secs(260)));
        // A person is not a vehicle.
        let mut w = Parked::default();
        assert!(!w.seen(&lab("car"), t0));
        assert!(!w.seen(&lab("person"), t0 + std::time::Duration::from_secs(400)));
        // The camera's own report (no box) is never parked.
        let own = Detection { label: "person".into(), score: 1.0, x: 0.0, y: 0.0, w: 0.0, h: 0.0 };
        assert!(!p.seen(&own, t0 + std::time::Duration::from_secs(5000)));
    }

    #[test]
    fn strict_needs_two_of_three_sightings_and_movement() {
        let p = |x: f32, y: f32| Detection { label: "person".into(), score: 0.8, x, y, w: 0.1, h: 0.3 };
        // Seen once: nothing to compare with.
        assert!(strict_confirmed(&[p(0.40, 0.4)], &[]).is_empty());
        // Seen twice in the same spot (a statue): not moving.
        let statue = [p(0.40, 0.4)];
        assert!(strict_confirmed(&[p(0.401, 0.4)], &[&statue]).is_empty());
        // Walking: moved 3% of the frame between looks.
        let before = [p(0.40, 0.4)];
        assert_eq!(strict_confirmed(&[p(0.43, 0.4)], &[&before]).len(), 1);
        // 2 of 3: the look before was empty, the one before that saw it.
        let empty: [Detection; 0] = [];
        assert_eq!(strict_confirmed(&[p(0.45, 0.4)], &[&empty, &before]).len(), 1);
        // Walking towards the camera: grows rather than shifts.
        let far = [Detection { w: 0.10, h: 0.30, ..p(0.40, 0.4) }];
        let near = Detection { x: 0.395, y: 0.385, w: 0.11, h: 0.33, ..p(0.0, 0.0) };
        assert_eq!(strict_confirmed(&[near], &[&far]).len(), 1);
        // Two different spots far apart are not one moving object.
        assert!(strict_confirmed(&[p(0.90, 0.4)], &[&before]).is_empty());
        // A different label does not count.
        let car = [Detection { label: "car".into(), ..p(0.40, 0.4) }];
        assert!(strict_confirmed(&[p(0.43, 0.4)], &[&car]).is_empty());
    }

    #[test]
    fn labels_unique_by_confidence() {
        let mk = |l: &str, s: f32| Detection { label: l.into(), score: s, x: 0.0, y: 0.0, w: 0.1, h: 0.1 };
        let st = AiState { found: vec![mk("car", 0.6), mk("person", 0.9), mk("car", 0.95)], checks: 2, ..Default::default() };
        assert_eq!(st.labels().len(), 2);
        assert_eq!(st.labels(), vec!["car".to_string(), "person".to_string()]);
    }

    #[test]
    fn held_back_objects_label_the_event_but_do_not_confirm_it() {
        // A shrub a remembered spot held back: the event is still found by
        // "person" (and is not unconfirmed), but sends no alert.
        let shrub = Detection { label: "person".into(), score: 0.6, x: 0.4, y: 0.3, w: 0.05, h: 0.12 };
        let mut st = AiState { held: vec![shrub], checks: 1, log: vec![AiCheck::default()], ..Default::default() };
        let out = AiOutcome::take(&mut st, &["person".to_string()]);
        assert!(out.ran && !out.confirmed);
        assert_eq!(out.labels, vec!["person".to_string()]);
        assert!(st.held.is_empty(), "the next event starts afresh");
    }

    #[test]
    fn the_cameras_own_detection_needs_ai_to_agree() {
        let own = |l: &str| Detection { label: l.into(), score: 1.0, x: 0.0, y: 0.0, w: 0.0, h: 0.0 };
        let look = |seen: Vec<Detection>| AiCheck { seen, ..Default::default() };
        let person = |score: f32| Detection { label: "person".into(), score, x: 0.4, y: 0.3, w: 0.05, h: 0.15 };
        let car = Detection { label: "car".into(), score: 0.8, x: 0.1, y: 0.5, w: 0.2, h: 0.1 };
        let wanted: Vec<String> = ["person", "car", "vehicle"].iter().map(|s| s.to_string()).collect();
        // AI looked after the camera's report and saw no person: no alert,
        // but the event keeps the label.
        let mut st = AiState { checks: 2, ..Default::default() };
        st.add_camera_label(own("person"));
        st.log = vec![look(vec![car.clone()]), look(vec![])];
        let out = AiOutcome::take(&mut st, &["person".to_string()]);
        assert!(out.ran && !out.confirmed);
        assert_eq!(out.camera_unconfirmed, vec!["person".to_string()]);
        assert!(out.labels.contains(&"person".to_string()));
        // AI saw a car: the camera's "vehicle" agrees (one kind).
        let mut st = AiState::default();
        st.add_camera_label(own("vehicle"));
        st.log = vec![look(vec![car.clone()])];
        st.found = vec![car.clone()];
        assert!(AiOutcome::take(&mut st, &["vehicle".to_string()]).confirmed);
        // A weak sighting agrees too.
        let mut st = AiState::default();
        st.add_camera_label(own("person"));
        st.log = vec![look(vec![person(0.4)])];
        let out = AiOutcome::take(&mut st, &wanted);
        assert!(out.confirmed && out.camera_unconfirmed.is_empty());
        // AI could not look (no picture, or every look failed), or had no
        // look after the camera's report: the camera decides.
        let mut st = AiState::default();
        st.add_camera_label(own("person"));
        assert!(AiOutcome::take(&mut st, &wanted).confirmed);
        let failed = AiCheck { error: Some("AI server down".into()), ..Default::default() };
        let mut st = AiState { log: vec![look(vec![])], ..Default::default() };
        st.add_camera_label(own("person"));
        st.log.push(failed);
        assert!(AiOutcome::take(&mut st, &wanted).confirmed, "no look worked after the report");
    }

    #[test]
    fn headlights_then_the_camera_reports_a_person_ai_gets_more_looks() {
        // Headlights start motion; AI looks 10 times (twice a second) and
        // sees nothing. At second 8 the camera reports a person.
        let max = 10;
        let wanted = vec!["person".to_string()];
        let mut st = AiState::default();
        for _ in 0..max {
            assert!(st.may_look(&wanted, max, false));
            st.checks += 1;
            st.log.push(AiCheck::default());
        }
        assert!(!st.may_look(&wanted, max, false), "out of looks");
        st.add_camera_label(Detection { label: "person".into(), score: 1.0, x: 0.0, y: 0.0, w: 0.0, h: 0.0 });
        // AI looks again, and at second 9 sees the person (only 40% sure).
        assert!(st.may_look(&wanted, max, false), "a fresh set of looks");
        st.checks += 1;
        st.log.push(AiCheck::default());
        assert!(st.may_look(&wanted, max, false));
        st.checks += 1;
        st.log.push(AiCheck { seen: vec![Detection { label: "person".into(), score: 0.4, x: 0.6, y: 0.3, w: 0.05, h: 0.2 }], ..Default::default() });
        assert!(!st.camera_pending());
        assert!(AiOutcome::take(&mut st, &wanted).confirmed);
        // While the camera's report is unconfirmed, AI keeps looking past
        // max_checks, up to FOLLOW_MAX_CHECKS.
        let mut st = AiState::default();
        st.add_camera_label(Detection { label: "person".into(), score: 1.0, x: 0.0, y: 0.0, w: 0.0, h: 0.0 });
        let mut n = 0;
        while st.may_look(&wanted, max, false) {
            st.checks += 1;
            st.log.push(AiCheck::default());
            n += 1;
        }
        assert_eq!(n, FOLLOW_MAX_CHECKS);
    }
}
