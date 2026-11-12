//! Remembered spots: things that only look like a person, a car or an animal
//! (a shrub, a wheelie bin, a headrest in a parked car) and stay where they
//! are, and vehicles parked in one place. AI checks run only during motion
//! events, minutes apart, so "seen all along" (the parked memory in
//! `ai_check`) never sees them sit still; this memory counts the separate
//! events instead.
//!
//! - A spot is remembered once the same kind of object (people; vehicles
//!   together; animals together) has been seen there surely in 3 separate
//!   events within 24 hours. A person- or animal-shaped one also needs 2
//!   other events in which it was seen there only weakly (under the
//!   camera's minimum, never surely): a real person is sure when they are
//!   there, a fixed object that only looks like one flickers.
//! - A detection on a remembered spot of its kind does not confirm the
//!   event. Everything else about it is unchanged: it is still drawn, kept
//!   in the AI review, and labels and records the event.
//! - A person-shaped box taller than HOLD_MAX_H is never held: fixed things
//!   taken for a person were small (far) boxes; a real person nearer the
//!   camera, or standing at a door, is taller.
//! - Something in the spot that moves (between two looks of one event)
//!   confirms as usual, and the spot is dropped: it was not fixed.
//! - Only looks that are not held keep a spot alive and teach it: a spot not
//!   seen so for 24 hours is forgotten, and a remembered spot lasts 24 hours
//!   from when it was learned, then has to be learned again.
//! - A vehicle spot remembers whether the last look at it saw a vehicle
//!   there. A vehicle is held only if, when the event began, the spot was
//!   last seen occupied (and within PARKED_STALE_MS): still parked. Last
//!   seen empty, or not looked at for long: it is an arrival, and confirms.
//!   When something else confirms the event its looks stop, and a parked
//!   vehicle may leave unseen: its spot is then taken as empty.
//! - A spot holds back one thing a look: a second car on a parked car's
//!   spot (one driving past in front of it) counts.
//! - The owner can mark an event "not a person" (or "not a car" ...): the
//!   boxes of that kind that sat still across its looks are remembered at
//!   once, for 30 days.
//! - All of it is where one AI model drew its boxes: when another model (or
//!   AI server) looks, every camera's spots and marks start over.
//!
//! Kept per camera, in memory and saved in the database (at most once a
//! minute, and at once after a mark or a forget).

use crate::ai::Detection;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

/// Detections under this score are noise (the AI review keeps them down to it).
pub const FLOOR: f32 = 0.25;
/// Separate events a spot must be seen surely in before it is remembered ...
pub const TIMES: usize = 3;
/// ... within this long.
const WINDOW_MS: i64 = 24 * 3600 * 1000;
/// Events a person- or animal-shaped spot must have been seen in only weakly ...
const WEAK_EVENTS: usize = 2;
/// ... at least this far apart.
const WEAK_APART_MS: i64 = 3600 * 1000;
/// A spot not seen (unheld) for this long is forgotten.
pub const FORGET_MS: i64 = 24 * 3600 * 1000;
/// A remembered spot lasts this long from when it was learned.
pub const REMEMBER_MS: i64 = 24 * 3600 * 1000;
/// How long the owner's "not a person" mark lasts.
pub const MARK_MS: i64 = 30 * 24 * 3600 * 1000;
/// Centre shift (fraction of the picture) between two looks of one event
/// that counts as moving. Fixed objects jitter by under 0.02 (measured).
const MOVED: f32 = 0.03;
/// A person-shaped box taller than this (fraction of the picture high) is
/// never held. 95% of the boxes of fixed things taken for a person in the
/// measured data were at most 0.105 high (shrubs 0.08-0.09).
pub const HOLD_MAX_H: f32 = 0.11;
/// A vehicle spot last looked at longer ago than this is not known to be
/// still parked.
const PARKED_STALE_MS: i64 = 12 * 3600 * 1000;
/// Spots kept per camera.
pub const MAX_SPOTS: usize = 64;
/// A vehicle spot is occupied when a vehicle box overlaps it this much.
const OCCUPIED_IOU: f32 = 0.3;
/// Sure sightings kept per spot (the box is their running mean).
const KEEP: usize = 8;

/// Overlap (intersection over union) for a box to be the same spot when
/// learning, and to be held back by a remembered spot. Parked vehicles'
/// boxes vary more (partly hidden, car or truck) than fixed objects'.
fn same_spot(group: &str) -> (f32, f32) {
    if group == "vehicle" { (0.5, 0.5) } else { (0.6, 0.7) }
}

fn is_animal(label: &str) -> bool {
    matches!(label, "animal" | "dog" | "cat" | "bird" | "horse" | "sheep" | "cow" | "bear" | "elephant" | "zebra" | "giraffe")
}

/// The kind of object a label belongs to: "person", "vehicle" (every
/// vehicle label together: the AI calls one parked car a car, then a
/// truck), "animal", or the label itself.
pub fn group(label: &str) -> &str {
    if label == "person" {
        "person"
    } else if super::ai_check::is_vehicle(label) {
        "vehicle"
    } else if is_animal(label) {
        "animal"
    } else {
        label
    }
}

/// Why a detection did not count.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Held {
    /// Seen in that spot in several earlier events.
    Remembered,
    /// The owner marked an earlier event there "not a <label>".
    Marked,
}

fn iou(a: (f32, f32, f32, f32), b: (f32, f32, f32, f32)) -> f32 {
    let (ax2, ay2, bx2, by2) = (a.0 + a.2, a.1 + a.3, b.0 + b.2, b.1 + b.3);
    let iw = (ax2.min(bx2) - a.0.max(b.0)).max(0.0);
    let ih = (ay2.min(by2) - a.1.max(b.1)).max(0.0);
    let inter = iw * ih;
    let union = a.2 * a.3 + b.2 * b.3 - inter;
    if union <= 0.0 { 0.0 } else { inter / union }
}

/// Whether a second box held by a spot in one look is the same thing as
/// the first: the built-in AI keeps one box per kind for each object, so
/// two boxes of one label are two objects; a car and a truck box that
/// overlap that much are one vehicle it was unsure of.
fn one_thing(a: &Detection, b: &Detection) -> bool {
    a.label != b.label && iou(bx(a), bx(b)) >= crate::ai::NMS_IOU
}

fn bx(d: &Detection) -> (f32, f32, f32, f32) {
    (d.x, d.y, d.w, d.h)
}

fn centre(d: &Detection) -> (f32, f32) {
    (d.x + d.w / 2.0, d.y + d.h / 2.0)
}

fn dist(a: (f32, f32), b: (f32, f32)) -> f32 {
    ((a.0 - b.0).powi(2) + (a.1 - b.1).powi(2)).sqrt()
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Spot {
    pub group: String,
    /// The label it was last seen as (for the plain words).
    pub label: String,
    pub x: f32,
    pub y: f32,
    pub w: f32,
    pub h: f32,
    /// When it was seen surely, one time per event (the last 24 hours).
    #[serde(default)]
    pub sure: Vec<i64>,
    /// Events (the last 24 hours) it was seen in only weakly.
    #[serde(default)]
    pub weak_at: Vec<i64>,
    /// Seen at all (down to FLOOR) in a look that was not held.
    pub last_ms: i64,
    /// Seen surely in TIMES events: holds until REMEMBER_MS after this.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub remembered_at: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub marked_until: Option<i64>,
    /// The event whose "not a ..." mark made it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub marked_by: Option<String>,
    /// A vehicle spot: the last look at it, and whether a vehicle was there.
    #[serde(default)]
    pub seen_ms: i64,
    #[serde(default)]
    pub occupied: bool,
    // This run's event numbers (never 0 for a real event): what has been
    // counted in the event in progress, and where it was held in it.
    #[serde(skip)]
    sure_event: u64,
    #[serde(skip)]
    held_event: u64,
    #[serde(skip)]
    held_at: Vec<(f32, f32)>,
    /// The event it was last seen in (unheld), when, and how.
    #[serde(skip)]
    cur_event: u64,
    #[serde(skip)]
    cur_ms: i64,
    #[serde(skip)]
    cur_weak: bool,
    #[serde(skip)]
    cur_sure: bool,
    /// A vehicle spot: the event its state at the start was taken for, and
    /// that state (still parked).
    #[serde(skip)]
    obs_event: u64,
    #[serde(skip)]
    parked_at_start: bool,
    /// This look: the detection it held back (see `one_thing`).
    #[serde(skip)]
    held_now: Option<Detection>,
    /// This look: something moved in it; it goes at the end of the look.
    #[serde(skip)]
    gone: bool,
}

impl Spot {
    fn new(d: &Detection, group: &str, now: i64) -> Spot {
        Spot { group: group.to_string(), label: d.label.clone(), x: d.x, y: d.y, w: d.w, h: d.h, last_ms: now, seen_ms: now, occupied: true, ..Default::default() }
    }
    fn b(&self) -> (f32, f32, f32, f32) {
        (self.x, self.y, self.w, self.h)
    }
    pub fn marked(&self, now: i64) -> bool {
        self.marked_until.is_some_and(|u| u > now)
    }
    fn remembered(&self) -> bool {
        self.remembered_at.is_some()
    }
    /// Times after `now` (saved before the clock was set back) become `now`,
    /// so a spot cannot hold, or a mark last, for ever.
    fn clamp(&mut self, now: i64) {
        self.last_ms = self.last_ms.min(now);
        self.seen_ms = self.seen_ms.min(now);
        self.remembered_at = self.remembered_at.map(|t| t.min(now));
        self.marked_until = self.marked_until.map(|t| t.min(now + MARK_MS));
        for t in self.sure.iter_mut().chain(self.weak_at.iter_mut()) {
            *t = (*t).min(now);
        }
    }
    /// Whether it holds detections back in event `ev` (counting only the
    /// sightings from other events).
    fn active(&self, ev: u64, now: i64) -> bool {
        if self.marked(now) {
            return true;
        }
        let sure = self.sure.len() - usize::from(self.sure_event == ev && !self.sure.is_empty());
        self.remembered() || (sure + 1 >= TIMES && self.weak_enough())
    }
    /// A person- or animal-shaped spot needs WEAK_EVENTS weak-only events.
    fn weak_enough(&self) -> bool {
        self.group == "vehicle" || self.weak_at.len() >= WEAK_EVENTS
    }
    /// The look of another event than the one it was last seen in: that
    /// event is over, and counts as weak if it was only seen weakly.
    fn close_event(&mut self, ev: u64) {
        if self.cur_event != 0 && self.cur_event != ev {
            if self.cur_weak && !self.cur_sure && self.weak_at.last().is_none_or(|&t| self.cur_ms - t >= WEAK_APART_MS) {
                self.weak_at.push(self.cur_ms);
                if self.weak_at.len() > KEEP {
                    self.weak_at.remove(0);
                }
            }
            self.cur_event = 0;
            self.cur_weak = false;
            self.cur_sure = false;
        }
    }
}

/// One camera's spots.
#[derive(Clone, Debug, Default)]
pub struct Spots {
    pub spots: Vec<Spot>,
}

/// Numbers for the camera's settings.
#[derive(Clone, Debug, Default, PartialEq, Serialize)]
pub struct Counts {
    /// Remembered by being seen again and again.
    pub remembered: usize,
    /// Marked by the owner.
    pub marked: usize,
    /// Seen, not (yet) remembered.
    pub learning: usize,
}

impl Spots {
    /// One AI look (detections down to FLOOR) in event `ev` at `now` (ms):
    /// for each detection, whether a spot holds it back. Learns from the
    /// look: only kinds `learn` says (those the camera alerts for). `auto`:
    /// remembered spots hold (else only the owner's marks).
    pub fn look(&mut self, dets: &[Detection], min: f32, learn: &dyn Fn(&str) -> bool, auto: bool, now: i64, ev: u64) -> Vec<Option<Held>> {
        self.spots.retain_mut(|s| {
            s.clamp(now);
            s.held_now = None;
            s.gone = false;
            s.close_event(ev);
            if s.marked_until.is_some_and(|u| u <= now) {
                s.marked_until = None;
                s.marked_by = None;
            }
            s.sure.retain(|&t| now - t < WINDOW_MS);
            s.weak_at.retain(|&t| now - t < WINDOW_MS);
            if s.remembered_at.is_some_and(|t| now - t >= REMEMBER_MS) {
                // Learned a day ago: learn it again from fresh sightings.
                s.remembered_at = None;
                s.sure.clear();
            } else if s.remembered_at.is_none() && s.sure.len() >= TIMES && s.weak_enough() {
                s.remembered_at = Some(now);
            }
            s.marked_until.is_some() || now - s.last_ms < FORGET_MS
        });
        // Vehicle spots: still parked when the event began? Then this look.
        for s in self.spots.iter_mut().filter(|s| s.group == "vehicle") {
            if s.obs_event != ev {
                s.obs_event = ev;
                s.parked_at_start = s.occupied && now - s.seen_ms <= PARKED_STALE_MS;
            }
            s.seen_ms = now;
            s.occupied = dets.iter().any(|d| d.score >= FLOOR && d.w > 0.0 && group(&d.label) == "vehicle" && iou(bx(d), s.b()) >= OCCUPIED_IOU);
        }
        let mut out = vec![None; dets.len()];
        for (i, d) in dets.iter().enumerate() {
            let g = group(&d.label);
            if d.w <= 0.0 || d.h <= 0.0 || d.score < FLOOR || !learn(g) {
                continue; // the camera's own report (no box), noise, or not wanted
            }
            let (learn_iou, hold_iou) = same_spot(g);
            let best = self
                .spots
                .iter()
                .enumerate()
                .filter(|(_, s)| s.group == g && !s.gone)
                .map(|(k, s)| (k, iou(bx(d), s.b())))
                .max_by(|a, b| a.1.total_cmp(&b.1));
            if let Some((k, v)) = best {
                let s = &mut self.spots[k];
                // Too tall to be one of the fixed things; a vehicle that
                // was not parked there when the event began is an arrival.
                // A spot holds one thing a look: a car driving past in
                // front of a parked one is a second box on its spot.
                let may_hold = !(g == "person" && d.h > HOLD_MAX_H) && (g != "vehicle" || s.parked_at_start) && s.held_now.as_ref().is_none_or(|h| one_thing(h, d));
                if v >= hold_iou && d.score >= min && may_hold && (s.marked(now) || (auto && s.active(ev, now))) {
                    if s.held_event != ev {
                        s.held_event = ev;
                        s.held_at.clear();
                    }
                    let c = centre(d);
                    if s.held_at.iter().any(|p| dist(*p, c) >= MOVED) {
                        // It moved: a real one standing there. It counts,
                        // and the spot goes.
                        s.gone = true;
                        continue;
                    }
                    s.held_now = Some(d.clone());
                    if s.held_at.len() < KEEP {
                        s.held_at.push(c);
                    }
                    if auto && s.remembered_at.is_none() && s.active(ev, now) && !s.marked(now) {
                        // Held for the first time: learned now (it lasts a day).
                        s.remembered_at = Some(now);
                    }
                    out[i] = Some(if s.marked(now) {
                        Held::Marked
                    } else {
                        Held::Remembered
                    });
                    // (A held look neither teaches the spot nor keeps it alive.)
                    continue;
                }
                if v >= learn_iou {
                    s.last_ms = now;
                    s.label = d.label.clone();
                    if s.cur_event != ev {
                        s.cur_event = ev;
                        s.cur_ms = now;
                    }
                    if d.score < min {
                        s.cur_weak = true;
                    } else {
                        s.cur_sure = true;
                        if s.sure_event != ev {
                            s.sure_event = ev;
                            s.sure.push(now);
                            if s.sure.len() > KEEP {
                                s.sure.remove(0);
                            }
                            let n = s.sure.len() as f32;
                            s.x += (d.x - s.x) / n;
                            s.y += (d.y - s.y) / n;
                            s.w += (d.w - s.w) / n;
                            s.h += (d.h - s.h) / n;
                            if s.sure.len() >= TIMES && s.remembered_at.is_none() && s.weak_enough() {
                                s.remembered_at = Some(now);
                            }
                        }
                    }
                    continue;
                }
            }
            if d.score >= min {
                self.make_room(now);
                let mut s = Spot::new(d, g, now);
                s.sure.push(now);
                s.sure_event = ev;
                s.cur_event = ev;
                s.cur_ms = now;
                s.cur_sure = true;
                self.spots.push(s);
            }
        }
        self.spots.retain(|s| !s.gone);
        out
    }

    /// Keep under MAX_SPOTS: one that moved goes first, then the one still
    /// learning seen longest ago; a remembered spot (held looks do not
    /// refresh when it was seen) only after those, an owner's mark last.
    fn make_room(&mut self, now: i64) {
        while self.spots.len() >= MAX_SPOTS {
            let k = self.spots.iter().enumerate().min_by_key(|(_, s)| (!s.gone, s.marked(now), s.active(0, now), s.last_ms)).map(|(k, _)| k).unwrap_or(0);
            self.spots.remove(k);
        }
    }

    /// The event's looks stopped early (something confirmed it): a vehicle
    /// that was parked when it began may leave unseen. Those spots are no
    /// longer known to be parked, so the next vehicle there counts as an
    /// arrival once (and is parked again from the event after). A vehicle
    /// that arrived in this event is not touched.
    pub fn looks_cut_short(&mut self, ev: u64) {
        for s in self.spots.iter_mut().filter(|s| s.group == "vehicle" && s.obs_event == ev && s.parked_at_start) {
            s.occupied = false;
        }
    }


    /// The owner says these boxes (all of one kind, from event `by`) were
    /// not real: remember them for MARK_MS. Returns how many spots that is.
    pub fn mark(&mut self, boxes: &[Detection], by: &str, now: i64) -> usize {
        let mut touched: Vec<usize> = Vec::new();
        for d in boxes.iter().filter(|d| d.w > 0.0 && d.h > 0.0) {
            let g = group(&d.label);
            let (learn_iou, _) = same_spot(g);
            let best = self.spots.iter().enumerate().filter(|(_, s)| s.group == g).map(|(k, s)| (k, iou(bx(d), s.b()))).max_by(|a, b| a.1.total_cmp(&b.1));
            let k = match best {
                Some((k, v)) if v >= learn_iou => k,
                _ => {
                    self.make_room(now);
                    self.spots.push(Spot::new(d, g, now));
                    self.spots.len() - 1
                }
            };
            let s = &mut self.spots[k];
            s.marked_until = Some(now + MARK_MS);
            s.marked_by = Some(by.to_string());
            s.last_ms = s.last_ms.max(now);
            if !touched.contains(&k) {
                touched.push(k);
            }
        }
        touched.len()
    }

    /// Take back the marks event `by` made. Returns how many.
    pub fn unmark(&mut self, by: &str) -> usize {
        let mut n = 0;
        for s in self.spots.iter_mut().filter(|s| s.marked_by.as_deref() == Some(by)) {
            s.marked_until = None;
            s.marked_by = None;
            n += 1;
        }
        // One that was only the mark is gone with it.
        self.spots.retain(|s| s.marked_until.is_some() || !s.sure.is_empty() || s.remembered());
        n
    }

    pub fn counts(&self, now: i64) -> Counts {
        let mut c = Counts::default();
        for s in &self.spots {
            if s.marked(now) {
                c.marked += 1;
            } else if s.active(0, now) {
                c.remembered += 1;
            } else {
                c.learning += 1;
            }
        }
        c
    }
}

// ------------------------------------------------------------------ store

struct Store {
    cams: HashMap<String, Spots>,
    saved_at: Option<std::time::Instant>,
    /// The AI the spots were learned with (see `model_in_use`).
    model: Option<String>,
}

static STORE: std::sync::Mutex<Option<Store>> = std::sync::Mutex::new(None);
const KV: (&str, &str) = ("server", "spots");
const MODEL_KV: (&str, &str) = ("server", "spots_model");
const SAVE_EVERY: std::time::Duration = std::time::Duration::from_secs(60);
static EVENTS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);

/// A number for a new event's looks (unique in this run, never 0).
pub fn next_event() -> u64 {
    EVENTS.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
}

fn load(db: &crate::db::Db) -> Store {
    let mut saved: HashMap<String, Vec<Spot>> = db.ui_get(KV.0, KV.1).ok().flatten().and_then(|s| serde_json::from_str(&s).ok()).unwrap_or_default();
    let now = chrono::Utc::now().timestamp_millis();
    saved.values_mut().flatten().for_each(|s| s.clamp(now));
    let model = db.ui_get(MODEL_KV.0, MODEL_KV.1).ok().flatten();
    Store { cams: saved.into_iter().map(|(k, v)| (k, Spots { spots: v })).collect(), saved_at: None, model }
}

impl Store {
    /// Another model looks: forget every camera's spots. None before: spots
    /// from before the model was kept, learned with this one (kept).
    fn switch_model(&mut self, model: &str) -> bool {
        let forgot = self.model.replace(model.to_string()).is_some_and(|m| m != model);
        if forgot {
            self.cams.values_mut().for_each(|s| s.spots.clear());
        }
        forgot
    }
}

/// All cameras' spots, as saved.
fn to_json(st: &mut Store) -> Option<String> {
    st.saved_at = Some(std::time::Instant::now());
    let all: HashMap<&String, &Vec<Spot>> = st.cams.iter().filter(|(_, v)| !v.spots.is_empty()).map(|(k, v)| (k, &v.spots)).collect();
    serde_json::to_string(&all).ok()
}

fn write(db: &crate::db::Db, json: Option<String>) {
    if let Some(j) = json {
        if let Err(e) = db.ui_put(KV.0, KV.1, &j) {
            tracing::warn!("AI: could not save the remembered spots: {e:#}");
        }
    }
}

/// The AI that looks now (`model`: the built-in model's file name, or the
/// AI server's address). The spots and the owner's marks are where the
/// model drew its boxes and what it mistook: when another model looks, every
/// camera's memory starts over. Returns whether it did.
pub fn model_in_use(db: &crate::db::Db, model: &str) -> bool {
    let (forgot, json) = {
        let mut g = STORE.lock().unwrap_or_else(|e| e.into_inner());
        let st = g.get_or_insert_with(|| load(db));
        if st.model.as_deref() == Some(model) {
            return false;
        }
        let forgot = st.switch_model(model);
        (forgot, if forgot { to_json(st) } else { None })
    };
    write(db, json);
    if let Err(e) = db.ui_put(MODEL_KV.0, MODEL_KV.1, model) {
        tracing::warn!("AI: could not save which model the spots are for: {e:#}");
    }
    if forgot {
        tracing::info!("AI: another model looks now; the remembered spots and marks start over");
    }
    forgot
}

/// Work on a camera's spots (loaded from the database the first time);
/// `save`: write them to the database now, else at most once a minute.
pub fn with<R>(db: &crate::db::Db, camera: &str, save: bool, f: impl FnOnce(&mut Spots) -> R) -> R {
    let (r, json) = {
        let mut g = STORE.lock().unwrap_or_else(|e| e.into_inner());
        let st = g.get_or_insert_with(|| load(db));
        let r = f(st.cams.entry(camera.to_string()).or_default());
        let json = if save || st.saved_at.is_none_or(|t| t.elapsed() >= SAVE_EVERY) { to_json(st) } else { None };
        (r, json)
    };
    write(db, json);
    r
}

/// Forget the spots of cameras not in `cameras` (removed, or renamed: a
/// renamed camera may be pointed somewhere else, so it starts afresh).
pub fn keep_only(db: &crate::db::Db, cameras: &[String]) {
    let gone: Vec<String> = {
        let mut g = STORE.lock().unwrap_or_else(|e| e.into_inner());
        let st = g.get_or_insert_with(|| load(db));
        st.cams.keys().filter(|k| !cameras.contains(k)).cloned().collect()
    };
    for name in gone {
        with(db, &name, true, |s| s.spots.clear());
        STORE.lock().unwrap_or_else(|e| e.into_inner()).as_mut().map(|st| st.cams.remove(&name));
    }
}

/// What an owner's "not a ..." mark did.
#[derive(Clone, Debug, Serialize)]
pub struct Marked {
    pub camera: String,
    /// The label it was for ("person").
    pub label: String,
    /// Spots remembered for it.
    pub spots: usize,
    /// With none: why ("one_look": that kind was in one AI look only, too
    /// little to tell what stood still; "moved": all of it moved; "none":
    /// no such object at or above the camera's minimum).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub why: Option<&'static str>,
}

/// The boxes of an event's AI review to mark "not a `label`": those of
/// that kind (at or above the minimum) that sat still across the event's
/// looks. One that moved between looks (someone walking) is never marked,
/// and with that kind in only one look nothing is: too little to tell what
/// stood still. Err: why there is nothing ("one_look", "moved", "none").
pub fn boxes_to_mark(detail: &serde_json::Value, label: &str) -> Result<Vec<Detection>, &'static str> {
    let min = detail["min_score"].as_f64().unwrap_or(0.5) as f32;
    let g = group(label).to_string();
    let looks: Vec<Vec<Detection>> = detail["checks"]
        .as_array()
        .into_iter()
        .flatten()
        .map(|c| {
            let seen: Vec<Detection> = serde_json::from_value(c["seen"].clone()).unwrap_or_default();
            seen.into_iter().filter(|d| d.score >= min && d.w > 0.0 && d.h > 0.0 && group(&d.label) == g).collect::<Vec<_>>()
        })
        .filter(|l| !l.is_empty())
        .collect();
    match looks.len() {
        0 => return Err("none"),
        1 => return Err("one_look"),
        _ => {}
    }
    let mut out = Vec::new();
    for (i, l) in looks.iter().enumerate() {
        for d in l {
            let others: Vec<&Detection> = looks.iter().enumerate().filter(|(j, _)| *j != i).flat_map(|(_, o)| o.iter()).filter(|o| iou(bx(o), bx(d)) >= 0.3).collect();
            if !others.is_empty() && others.iter().all(|o| dist(centre(o), centre(d)) < MOVED) {
                out.push(d.clone());
            }
        }
    }
    if out.is_empty() { Err("moved") } else { Ok(out) }
}

impl super::Engine {
    /// The owner says event `id` was not a real `label`: its boxes of that
    /// kind that sat still are remembered as not real, on its camera, for
    /// MARK_MS. None: no such event or AI review; 0 spots: nothing of that
    /// kind sat still.
    pub fn mark_not_real(&self, id: &str, label: &str) -> anyhow::Result<Option<Marked>> {
        let Some(ev) = self.db.get_event(id)? else { return Ok(None) };
        let Some(detail) = self.db.ai_review(id)?.and_then(|s| serde_json::from_str::<serde_json::Value>(&s).ok()) else { return Ok(None) };
        let (n, why) = match boxes_to_mark(&detail, label) {
            Ok(boxes) => {
                let now = chrono::Utc::now().timestamp_millis();
                (with(&self.db, &ev.camera, true, |s| s.mark(&boxes, id, now)), None)
            }
            Err(why) => (0, Some(why)),
        };
        Ok(Some(Marked { camera: ev.camera, label: label.to_string(), spots: n, why }))
    }

    /// Take back the marks of event `id` (it was real after all).
    pub fn unmark_not_real(&self, id: &str) -> anyhow::Result<usize> {
        let Some(ev) = self.db.get_event(id)? else { return Ok(0) };
        Ok(with(&self.db, &ev.camera, true, |s| s.unmark(id)))
    }

    /// How many spots a camera remembers.
    pub fn spot_counts(&self, camera: &str) -> Counts {
        let now = chrono::Utc::now().timestamp_millis();
        with(&self.db, camera, false, |s| s.counts(now))
    }

    /// Forget all of a camera's spots (also the owner's marks). Returns how many.
    pub fn forget_spots(&self, camera: &str) -> usize {
        with(&self.db, camera, true, |s| std::mem::take(&mut s.spots).len())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const MIN: f32 = 0.5;
    const H: i64 = 3600 * 1000;
    const M: i64 = 60 * 1000;

    fn det(label: &str, score: f32, x: f32, y: f32, w: f32, h: f32) -> Detection {
        Detection { label: label.into(), score, x, y, w, h }
    }
    fn all(_: &str) -> bool {
        true
    }
    /// One event of one look; what the detections got.
    fn once(s: &mut Spots, d: &[Detection], now: i64) -> Vec<Option<Held>> {
        s.look(d, MIN, &all, true, now, next_event())
    }
    /// Teach a person-shaped spot: 3 sure events and 2 weak-only ones.
    fn teach(s: &mut Spots, sure: &Detection, t0: i64) {
        let weak = Detection { score: 0.3, ..sure.clone() };
        once(s, &[sure.clone()], t0);
        once(s, &[weak.clone()], t0 + H);
        once(s, &[sure.clone()], t0 + 2 * H);
        once(s, &[weak], t0 + 3 * H);
        once(s, &[sure.clone()], t0 + 4 * H);
    }

    #[test]
    fn a_shrub_seen_in_three_events_is_remembered() {
        let shrub = |score: f32| det("person", score, 0.40, 0.30, 0.04, 0.08);
        let mut s = Spots::default();
        assert_eq!(once(&mut s, &[shrub(0.6)], 0), vec![None]);
        assert_eq!(once(&mut s, &[shrub(0.3)], H), vec![None]);
        assert_eq!(once(&mut s, &[shrub(0.55)], 2 * H), vec![None]);
        // Three sure events, but only one weak one: not yet.
        assert_eq!(once(&mut s, &[shrub(0.58)], 3 * H), vec![None]);
        assert_eq!(once(&mut s, &[shrub(0.3)], 4 * H), vec![None]);
        // A second weak-only event: held back from now on.
        assert_eq!(once(&mut s, &[shrub(0.7)], 5 * H), vec![Some(Held::Remembered)]);
        assert_eq!(s.counts(5 * H), Counts { remembered: 1, marked: 0, learning: 0 });
        // A bigger box in the same place (someone standing in front): counts.
        assert_eq!(once(&mut s, &[det("person", 0.9, 0.37, 0.20, 0.12, 0.35)], 6 * H), vec![None]);
        // A dog there is not a person.
        assert_eq!(once(&mut s, &[det("dog", 0.9, 0.40, 0.30, 0.04, 0.08)], 7 * H), vec![None]);
    }

    #[test]
    fn weak_only_events_count_an_hour_apart() {
        let shrub = |score: f32| det("person", score, 0.40, 0.30, 0.04, 0.08);
        let mut s = Spots::default();
        for k in 0..3 {
            once(&mut s, &[shrub(0.6)], k * M);
        }
        // Two weak-only events 10 minutes apart are one.
        once(&mut s, &[shrub(0.3)], 10 * M);
        once(&mut s, &[shrub(0.3)], 20 * M);
        assert_eq!(once(&mut s, &[shrub(0.6)], 30 * M), vec![None]);
        once(&mut s, &[shrub(0.3)], 80 * M);
        assert_eq!(once(&mut s, &[shrub(0.6)], 90 * M), vec![Some(Held::Remembered)]);
    }

    #[test]
    fn a_far_door_visitor_who_stands_still_alerts_every_time() {
        // A door far off: the visitor's box is 0.18 high, in one spot, and
        // some visits see them only weakly. Every sure visit alerts.
        let at = |score: f32| det("person", score, 0.52, 0.06, 0.05, 0.18);
        let mut s = Spots::default();
        for v in 0..10 {
            let score = if v % 2 == 1 { 0.35 } else { 0.85 };
            let r = once(&mut s, &[at(score)], v * 2 * H);
            if score >= MIN {
                assert_eq!(r, vec![None], "visit {v}");
            }
        }
    }

    #[test]
    fn weak_and_sure_in_one_event_is_not_weak() {
        // A real person approaching: unsure first, then sure, in each visit.
        let p = |score: f32| det("person", score, 0.52, 0.06, 0.05, 0.18);
        let mut s = Spots::default();
        for k in 0..8 {
            let ev = next_event();
            assert_eq!(s.look(&[p(0.35)], MIN, &all, true, k * H, ev), vec![None]);
            assert_eq!(s.look(&[p(0.8)], MIN, &all, true, k * H + 500, ev), vec![None], "visit {k}");
        }
    }

    #[test]
    fn a_doorbell_visitor_alerts_every_time() {
        // 4 visits a day for a week, each with one weak look, at the same
        // spot near the camera: a real person, never held.
        let at = |score: f32| det("person", score, 0.35, 0.25, 0.20, 0.55);
        let mut s = Spots::default();
        for day in 0..7 {
            for v in 0..4 {
                let t = day * 24 * H + v * 3 * H;
                let ev = next_event();
                // The weak look may come before or after the sure one.
                let looks = if v % 2 == 0 { [at(0.4), at(0.85)] } else { [at(0.85), at(0.4)] };
                let mut alerted = false;
                for (n, d) in looks.iter().enumerate() {
                    let r = s.look(std::slice::from_ref(d), MIN, &all, true, t + n as i64 * 500, ev);
                    alerted |= d.score >= MIN && r[0].is_none();
                }
                assert!(alerted, "day {day} visit {v}");
                // A visit with only the weak look, now and then.
                once(&mut s, &[at(0.4)], t + H);
            }
        }
    }

    #[test]
    fn a_remembered_spot_lasts_a_day_and_held_looks_do_not_keep_it() {
        let shrub = det("person", 0.6, 0.40, 0.30, 0.04, 0.08);
        let mut s = Spots::default();
        teach(&mut s, &shrub, 0);
        // (The fifth look of teach() at 4 h is already held.)
        assert_eq!(once(&mut s, &[shrub.clone()], 5 * H), vec![Some(Held::Remembered)]);
        // Held looks all day: held until a day after the last look that was
        // not held (the weak one at 3 h) ...
        for k in 6..27 {
            assert_eq!(once(&mut s, &[shrub.clone()], k * H), vec![Some(Held::Remembered)], "hour {k}");
        }
        // ... then it must be learned again from looks that are not held.
        assert_eq!(once(&mut s, &[shrub.clone()], 27 * H), vec![None]);
        // Seen unheld all along, it still lasts only a day from when it was learned.
        let mut u = Spots::default();
        teach(&mut u, &shrub, 0);
        for k in 5..40 {
            once(&mut u, &[Detection { score: 0.3, ..shrub.clone() }], k * H);
        }
        assert!(u.spots.iter().all(|x| x.remembered_at.is_none_or(|t| t >= 28 * H)), "relearned, not kept");
        // Only held looks for a day: forgotten.
        let mut t = Spots::default();
        teach(&mut t, &shrub, 0);
        once(&mut t, &[shrub.clone()], 5 * H);
        assert!(t.spots.iter().all(|x| x.last_ms <= 4 * H));
    }

    #[test]
    fn a_tall_person_box_is_never_held() {
        let big = det("person", 0.6, 0.30, 0.20, 0.08, HOLD_MAX_H + 0.01);
        let mut s = Spots::default();
        teach(&mut s, &big, 0);
        for k in 5..10 {
            assert_eq!(once(&mut s, &[big.clone()], k * H), vec![None]);
        }
        s.mark(&[big.clone()], "e", 10 * H);
        assert_eq!(once(&mut s, &[big.clone()], 11 * H), vec![None], "not even marked");
    }

    #[test]
    fn three_looks_in_one_event_are_one_sighting() {
        let bin = det("person", 0.6, 0.7, 0.2, 0.03, 0.1);
        let mut s = Spots::default();
        let ev = next_event();
        for k in 0..3 {
            assert_eq!(s.look(&[bin.clone()], MIN, &all, true, H + k * 500, ev), vec![None]);
        }
        assert_eq!(s.spots[0].sure.len(), 1);
    }

    #[test]
    fn moving_in_a_remembered_spot_counts_and_drops_it() {
        // A wide, low box: someone crouching where the shrub is.
        let at = |x: f32| det("person", 0.6, x, 0.30, 0.25, 0.10);
        let mut s = Spots::default();
        teach(&mut s, &at(0.40), 0);
        let ev = next_event();
        assert_eq!(s.look(&[at(0.40)], MIN, &all, true, 5 * H, ev), vec![Some(Held::Remembered)]);
        // Jitter is not moving.
        assert_eq!(s.look(&[at(0.405)], MIN, &all, true, 5 * H + 500, ev), vec![Some(Held::Remembered)]);
        // A small step, still in the spot: counts, and the spot is gone.
        assert_eq!(s.look(&[at(0.44)], MIN, &all, true, 5 * H + 1000, ev), vec![None]);
        assert!(s.spots.iter().all(|x| x.remembered_at.is_none()));
        // Far from the camera (a small box), a step leaves the spot: counts too.
        let small = |x: f32| det("person", 0.6, x, 0.30, 0.03, 0.08);
        let mut t = Spots::default();
        teach(&mut t, &small(0.40), 0);
        assert_eq!(once(&mut t, &[small(0.40)], 5 * H), vec![Some(Held::Remembered)]);
        assert_eq!(once(&mut t, &[small(0.42)], 6 * H), vec![None]);
    }

    #[test]
    fn a_parked_car_and_the_one_after_it() {
        let car = |label: &str| det(label, 0.8, 0.30, 0.50, 0.20, 0.12);
        let person = det("person", 0.9, 0.8, 0.2, 0.05, 0.2);
        let mut s = Spots::default();
        assert_eq!(once(&mut s, &[car("car")], 0), vec![None], "arrives");
        assert_eq!(once(&mut s, &[car("truck")], 5 * M), vec![None], "car or truck: one kind");
        assert_eq!(once(&mut s, &[car("car")], 10 * M), vec![Some(Held::Remembered)], "parked");
        // Still there an hour later, last seen occupied: held, in every look.
        let ev = next_event();
        assert_eq!(s.look(&[car("car")], MIN, &all, true, 70 * M, ev), vec![Some(Held::Remembered)]);
        assert_eq!(s.look(&[car("car")], MIN, &all, true, 70 * M + 500, ev), vec![Some(Held::Remembered)]);
        // It leaves: one look sees the spot empty.
        once(&mut s, &[person.clone()], 80 * M);
        // The next car to park there alerts, in every look of its event.
        let ev = next_event();
        assert_eq!(s.look(&[car("car")], MIN, &all, true, 90 * M, ev), vec![None], "arrived and stopped by the first look");
        assert_eq!(s.look(&[car("car")], MIN, &all, true, 90 * M + 500, ev), vec![None], "judged by the start of the event");
        // From the next event it is parked again.
        assert_eq!(once(&mut s, &[car("car")], 100 * M), vec![Some(Held::Remembered)]);
    }

    #[test]
    fn a_drive_by_through_a_parked_spot_alerts() {
        let car = det("car", 0.8, 0.30, 0.50, 0.20, 0.12);
        let mut s = Spots::default();
        for k in 0..3 {
            once(&mut s, &[car.clone()], k * 4 * M);
        }
        once(&mut s, &[], 20 * M); // the parked car has gone
        // A car passes through the spot: seen there in one look, gone in the next.
        let ev = next_event();
        assert_eq!(s.look(&[car.clone()], MIN, &all, true, 30 * M, ev), vec![None]);
        assert_eq!(s.look(&[], MIN, &all, true, 30 * M + 500, ev), Vec::<Option<Held>>::new());
    }

    #[test]
    fn a_spot_not_looked_at_for_long_is_not_known_to_be_parked() {
        let car = det("car", 0.8, 0.30, 0.50, 0.20, 0.12);
        let mut s = Spots::default();
        for k in 0..3 {
            once(&mut s, &[car.clone()], k * 4 * M);
        }
        assert_eq!(once(&mut s, &[car.clone()], 12 * M + PARKED_STALE_MS + 1), vec![None]);
    }

    #[test]
    fn forgotten_after_a_day_unseen() {
        let car = det("car", 0.8, 0.30, 0.50, 0.20, 0.12);
        let mut s = Spots::default();
        for k in 0..3 {
            once(&mut s, &[car.clone()], k * 4 * M);
        }
        assert_eq!(once(&mut s, &[car.clone()], 12 * M), vec![Some(Held::Remembered)]);
        assert_eq!(once(&mut s, &[car.clone()], 12 * M + FORGET_MS + 1), vec![None]);
    }

    #[test]
    fn marks_hold_at_once_and_can_be_taken_back() {
        let seat = det("person", 0.62, 0.55, 0.40, 0.04, 0.08);
        let mut s = Spots::default();
        assert_eq!(s.mark(&[seat.clone(), Detection { score: 0.7, ..seat.clone() }], "ev1", 0), 1);
        assert_eq!(once(&mut s, &[seat.clone()], H), vec![Some(Held::Marked)]);
        assert_eq!(s.counts(H), Counts { remembered: 0, marked: 1, learning: 0 });
        // Still held after a week unseen (a mark lasts 30 days) ...
        assert_eq!(once(&mut s, &[seat.clone()], 8 * 24 * H), vec![Some(Held::Marked)]);
        // ... and taken back with the mark.
        assert_eq!(s.unmark("ev1"), 1);
        assert_eq!(once(&mut s, &[seat.clone()], 9 * 24 * H), vec![None]);
        // A mark ends after 30 days.
        let mut t = Spots::default();
        t.mark(&[seat.clone()], "ev2", 0);
        assert_eq!(once(&mut t, &[seat.clone()], MARK_MS + 1), vec![None]);
    }

    #[test]
    fn only_wanted_kinds_and_boxes_are_learned_and_room_is_kept() {
        let mut s = Spots::default();
        let only_people = |g: &str| g == "person";
        s.look(&[det("car", 0.9, 0.1, 0.1, 0.2, 0.1), det("person", 1.0, 0.0, 0.0, 0.0, 0.0)], MIN, &only_people, true, 0, next_event());
        assert!(s.spots.is_empty());
        for k in 0..(MAX_SPOTS as i64 + 10) {
            let x = (k % 10) as f32 * 0.1;
            let y = (k / 10) as f32 * 0.1;
            once(&mut s, &[det("person", 0.9, x, y, 0.02, 0.02)], k);
        }
        assert_eq!(s.spots.len(), MAX_SPOTS);
    }

    fn review(looks: &[Vec<Detection>]) -> serde_json::Value {
        serde_json::json!({ "wanted": ["person", "car"], "min_score": 0.5,
            "checks": looks.iter().map(|l| serde_json::json!({ "seen": l, "parked": vec![false; l.len()] })).collect::<Vec<_>>() })
    }

    #[test]
    fn marks_take_only_what_sat_still() {
        let shrub = det("person", 0.66, 0.4, 0.3, 0.05, 0.08);
        let walker = |x: f32| det("person", 0.8, x, 0.5, 0.06, 0.2);
        let car = det("car", 0.9, 0.1, 0.5, 0.2, 0.1);
        let looks: Vec<Vec<Detection>> = (0..5).map(|k| vec![Detection { x: 0.4 + k as f32 * 0.002, ..shrub.clone() }, walker(0.1 + k as f32 * 0.04), car.clone()]).collect();
        let r = review(&looks);
        let boxes = boxes_to_mark(&r, "person").unwrap();
        assert!(boxes.iter().all(|b| (b.y - 0.3).abs() < 1e-6), "only the shrub, never the walker");
        assert_eq!(boxes.len(), 5);
        let mut s = Spots::default();
        assert_eq!(s.mark(&boxes, "e", 0), 1, "the looks at one thing are one spot");
        assert_eq!(boxes_to_mark(&r, "truck").unwrap().len(), 5, "any vehicle label marks the vehicles");
        assert_eq!(boxes_to_mark(&r, "dog").unwrap_err(), "none");
        // One look: too little to tell what stood still, nothing is marked.
        let one = review(&[vec![shrub.clone(), walker(0.6)]]);
        assert_eq!(boxes_to_mark(&one, "person").unwrap_err(), "one_look");
    }

    #[test]
    fn a_walker_marked_by_mistake_still_alerts_later() {
        // An event of a person walking past, 5 looks, marked "not a person":
        // nothing sat still, so nothing is remembered ...
        let at = |x: f32| det("person", 0.8, x, 0.4, 0.05, 0.18);
        let looks: Vec<Vec<Detection>> = (0..5).map(|k| vec![at(0.1 + k as f32 * 0.15)]).collect();
        assert_eq!(boxes_to_mark(&review(&looks), "person").unwrap_err(), "moved");
        // ... and a later walker, one look per place, alerts every time.
        let mut s = Spots::default();
        for k in 0..5 {
            assert_eq!(once(&mut s, &[at(0.1 + k as f32 * 0.15)], H + k * M), vec![None], "place {k}");
        }
    }

    #[test]
    fn a_remembered_shrub_outlasts_many_passers_by() {
        let shrub = det("person", 0.6, 0.40, 0.30, 0.04, 0.08);
        let mut s = Spots::default();
        teach(&mut s, &shrub, 0);
        // 70 people, each once, in different places over 7 hours.
        for k in 0..70i64 {
            let (x, y) = ((k % 10) as f32 * 0.09, 0.5 + (k / 10) as f32 * 0.07);
            once(&mut s, &[det("person", 0.9, x, y, 0.02, 0.02)], 5 * H + k * 6 * M);
        }
        assert_eq!(s.spots.len(), MAX_SPOTS);
        assert_eq!(once(&mut s, &[shrub.clone()], 13 * H), vec![Some(Held::Remembered)]);
    }

    #[test]
    fn a_spot_that_moves_goes_and_not_another_one() {
        // Oldest first: a spot still learning, the shrub, the owner's mark.
        let at = |x: f32| det("person", 0.6, x, 0.30, 0.25, 0.10);
        let mut s = Spots::default();
        once(&mut s, &[det("person", 0.9, 0.9, 0.9, 0.02, 0.02)], 0);
        teach(&mut s, &at(0.40), 1);
        s.mark(&[det("person", 0.6, 0.05, 0.05, 0.03, 0.08)], "e", 5 * H);
        let mut k = 0i64;
        while s.spots.len() < MAX_SPOTS {
            let (x, y) = ((k % 10) as f32 * 0.09, 0.5 + (k / 10) as f32 * 0.06);
            once(&mut s, &[det("person", 0.9, x, y, 0.02, 0.02)], 5 * H + k);
            k += 1;
        }
        let ev = next_event();
        assert_eq!(s.look(&[at(0.40)], MIN, &all, true, 6 * H, ev), vec![Some(Held::Remembered)]);
        // It moves, and someone new comes into view (room is made for them).
        let r = s.look(&[at(0.44), det("person", 0.9, 0.9, 0.05, 0.02, 0.02)], MIN, &all, true, 6 * H + 500, ev);
        assert_eq!(r, vec![None, None]);
        assert_eq!(s.counts(6 * H).marked, 1, "the owner's mark is kept");
        assert!(s.spots.iter().all(|x| x.remembered_at.is_none()), "the shrub's spot went");
    }

    #[test]
    fn a_car_that_leaves_unseen_after_the_looks_stop() {
        let car = det("car", 0.8, 0.30, 0.50, 0.20, 0.12);
        let person = det("person", 0.9, 0.35, 0.40, 0.05, 0.2);
        let mut s = Spots::default();
        for k in 0..3 {
            once(&mut s, &[car.clone()], k * 4 * M);
        }
        // The owner walks to the parked car: confirmed at the first look,
        // which still sees the car. The looks stop; the car leaves unseen.
        let ev = next_event();
        assert_eq!(s.look(&[car.clone(), person], MIN, &all, true, 20 * M, ev), vec![Some(Held::Remembered), None]);
        s.looks_cut_short(ev);
        // Hours later a car arrives and stops there before the first look.
        assert_eq!(once(&mut s, &[car.clone()], 5 * H), vec![None], "an arrival");
        assert_eq!(once(&mut s, &[car.clone()], 5 * H + 10 * M), vec![Some(Held::Remembered)], "then parked");
        // A car that arrives in an event cut short is parked from the next one.
        once(&mut s, &[], 6 * H);
        let ev = next_event();
        assert_eq!(s.look(&[car.clone()], MIN, &all, true, 7 * H, ev), vec![None]);
        s.looks_cut_short(ev);
        assert_eq!(once(&mut s, &[car.clone()], 7 * H + 10 * M), vec![Some(Held::Remembered)]);
    }

    #[test]
    fn a_car_passing_in_front_of_a_parked_one_alerts() {
        let car = det("car", 0.8, 0.30, 0.50, 0.20, 0.12);
        let mut s = Spots::default();
        for k in 0..3 {
            once(&mut s, &[car.clone()], k * 4 * M);
        }
        // Another car, right in front of the parked one, in one look.
        let passing = det("car", 0.7, 0.31, 0.505, 0.20, 0.12);
        assert_eq!(once(&mut s, &[car.clone(), passing], 20 * M), vec![Some(Held::Remembered), None]);
        // The parked car taken for both a car and a truck is still one vehicle.
        let truck = Detection { label: "truck".into(), score: 0.6, ..car.clone() };
        assert_eq!(once(&mut s, &[car.clone(), truck], 30 * M), vec![Some(Held::Remembered), Some(Held::Remembered)]);
    }

    #[test]
    fn times_ahead_of_the_clock_do_not_hold_for_ever() {
        // Learned while the clock was a year ahead, then set right.
        let shrub = det("person", 0.6, 0.40, 0.30, 0.04, 0.08);
        let year = 365 * 24 * H;
        let mut s = Spots::default();
        teach(&mut s, &shrub, year);
        s.mark(&[det("cat", 0.7, 0.2, 0.6, 0.1, 0.1)], "e", year);
        assert_eq!(once(&mut s, &[shrub.clone()], 0), vec![Some(Held::Remembered)]);
        assert!(s.spots.iter().all(|x| x.last_ms <= 0 && x.remembered_at.is_none_or(|t| t <= 0) && x.marked_until.is_none_or(|t| t <= MARK_MS)));
        assert_eq!(once(&mut s, &[shrub.clone()], REMEMBER_MS + 1), vec![None], "and it ends");
        assert_eq!(once(&mut s, &[det("cat", 0.7, 0.2, 0.6, 0.1, 0.1)], MARK_MS + 1), vec![None], "the mark ends too");
    }

    #[test]
    fn another_model_starts_the_memory_over() {
        let mut spots = Spots::default();
        spots.mark(&[det("person", 0.6, 0.4, 0.3, 0.04, 0.08)], "e", 0);
        let mut st = Store { cams: HashMap::from([("Yard".to_string(), spots)]), saved_at: None, model: None };
        assert!(!st.switch_model("model l.onnx"), "spots from before are this model's");
        assert_eq!(st.cams["Yard"].spots.len(), 1);
        assert!(!st.switch_model("model l.onnx"));
        assert!(st.switch_model("model s.onnx"));
        assert!(st.cams["Yard"].spots.is_empty());
    }

    #[test]
    fn saved_spots_round_trip() {
        let mut s = Spots::default();
        s.mark(&[det("cat", 0.7, 0.2, 0.2, 0.1, 0.1)], "e", 5);
        let j = serde_json::to_string(&s.spots).unwrap();
        assert!(!j.contains("held_at"));
        let back: Vec<Spot> = serde_json::from_str(&j).unwrap();
        let mut t = Spots { spots: back };
        assert_eq!(t.look(&[det("dog", 0.8, 0.2, 0.2, 0.1, 0.1)], MIN, &all, true, 10, next_event()), vec![Some(Held::Marked)]);
    }
}
