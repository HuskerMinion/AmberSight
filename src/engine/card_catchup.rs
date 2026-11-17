//! Catching up on a battery camera's events from its own card.
//!
//! A battery Argus records each PIR trigger to its SD card whether or not
//! AmberSight hears about it: its push can be missed (push off in the Reolink
//! app, DNS or Wi-Fi trouble), or AmberSight was down or updating. Then
//! AmberSight has no event at all. This module lists the card's recordings of
//! the last hours (cardlist.rs: nothing is downloaded for that), compares them
//! with the camera's events and recordings here, and imports each one that is
//! missing as an ordinary event: recording, thumbnail, AI check, marked "from
//! the camera's card". An old event sends no notification.
//!
//! Every look wakes the camera, so it looks only after AmberSight starts,
//! after the camera was unreachable and is back, after a push that brought
//! no recording, and otherwise at most every `catch_up_every` minutes (60 by
//! default) while the camera is awake anyway, with a wake of its own at most
//! every three times that. A camera whose card lists nothing three times in
//! a row gets no wakes of its own any more (no card, or card recording off),
//! and neither does one whose card could not be listed three times in a row
//! (until it reconnects or AmberSight starts again).

use super::*;
use crate::cardfill::{self, Gap};
use crate::ingest::baichuan::client::{BaichuanClient, ListedClip};
use crate::ingest::baichuan::media::Demux;
use super::card_fill::{already_handled, Attempt, Tried};
use std::collections::HashMap;
use std::sync::Mutex as StdMutex;

/// How far back the card is looked at.
pub const LOOK_BACK_MS: i64 = 12 * 3600 * 1000;
/// Recordings that ended less than this long ago are left for the next look:
/// the camera cannot hand over a recording it is still writing.
const READY_AFTER_MS: i64 = 3 * 60_000;
/// The first look after a start, when the camera has not woken by then.
const FIRST_LOOK_AFTER_MS: i64 = 5 * 60_000;
/// No two looks closer than this (a reconnect right after a look, ...).
const MIN_SPACING_MS: i64 = 15 * 60_000;
/// A look of its own (waking the camera) at most every this many periods.
const WAKE_EVERY_PERIODS: i64 = 3;
/// After this many looks in a row that listed nothing, no more looks of its own.
const EMPTY_LIMIT: u32 = 3;
/// After this many looks in a row that could not list the card, no looks
/// that wake the camera (a wake of its own, a look again) until a reconnect
/// or a start.
const FAIL_LIMIT: u32 = 3;
/// A look cut short (live video, the budget ran out) is done again after this.
const AGAIN_AFTER_MS: i64 = 20 * 60_000;
/// One look: recordings listed and requests, recordings imported, bytes
/// downloaded, and how long it may take.
const MAX_LISTED: usize = 60;
const MAX_ASKS: u32 = 80;
const MAX_IMPORTS: usize = 5;
const MAX_BYTES: u64 = 150 << 20;
const MAX_TIME: std::time::Duration = std::time::Duration::from_secs(15 * 60);
/// One recording: the longest believed, and the most downloaded for it.
const MAX_CLIP_MS: i64 = 10 * 60_000;
const MAX_CLIP_BYTES: u64 = 64 << 20;
/// An imported event that ended less than this long ago notifies as usual.
pub const NOTIFY_WITHIN_MS: i64 = 5 * 60_000;
/// ui_kv scopes: what was tried (`{camera}|{clip start ms}`), and whether the
/// camera records events (`{camera}` -> `on|{since}` / `off|{since}`).
const SCOPE: &str = "card_catchup";
const MODE_SCOPE: &str = "card_catchup_mode";

// ---- status ----

/// What the camera's settings say about this.
#[derive(Clone, Debug, Default, serde::Serialize, PartialEq)]
pub struct Status {
    /// "waiting", "watching", "checking", "problem" or "unavailable".
    pub state: String,
    /// One plain sentence.
    pub text: String,
    /// When the card was last looked at, and when events were last imported (epoch ms).
    pub last_check: Option<i64>,
    pub last_import: Option<i64>,
    /// How many the last import brought; all since the server started.
    pub last_imported: u32,
    pub imported: u32,
}

static STATUS: StdMutex<Option<HashMap<String, Status>>> = StdMutex::new(None);

fn set_status(name: &str, f: impl FnOnce(&mut Status)) {
    let mut g = STATUS.lock().unwrap_or_else(|e| e.into_inner());
    f(g.get_or_insert_with(Default::default).entry(name.to_string()).or_default());
}

/// What this camera's catching up is doing (None: nothing yet).
pub fn status(name: &str) -> Option<Status> {
    STATUS.lock().unwrap_or_else(|e| e.into_inner()).as_ref().and_then(|m| m.get(name).cloned())
}

fn clear_status(name: &str) {
    if let Some(m) = STATUS.lock().unwrap_or_else(|e| e.into_inner()).as_mut() {
        m.remove(name);
    }
}

/// A camera was removed: what was tried, its noted mode and its status go.
pub fn forget_camera(db: &crate::db::Db, name: &str) {
    let s = slug(name);
    let _ = db.ui_delete_prefix(SCOPE, &format!("{s}|"));
    let _ = db.ui_delete(MODE_SCOPE, &s);
    clear_status(name);
    PUSHES.lock().unwrap_or_else(|e| e.into_inner()).as_mut().map(|m| m.remove(name));
}

/// A camera was renamed: its status follows.
pub fn rename_camera(old: &str, new: &str) {
    let mut g = STATUS.lock().unwrap_or_else(|e| e.into_inner());
    if let Some(m) = g.as_mut() {
        if let Some(s) = m.remove(old) {
            m.insert(new.to_string(), s);
        }
    }
}

// ---- when to look ----

/// Why the card is looked at now.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Why {
    /// AmberSight started: the first look.
    Start,
    /// The camera could not be reached for a while and is back.
    Reconnect,
    /// A push came and nothing was recorded, or a look was cut short.
    Again,
    /// The period passed while the camera is awake anyway.
    Periodic,
    /// The camera has not been awake for a long time: a wake of its own.
    Wake,
}

impl Why {
    fn text(self) -> &'static str {
        match self {
            Why::Start => "after the start",
            Why::Reconnect => "the camera is back after it could not be reached",
            Why::Again => "a push brought no recording, or the last look was cut short",
            Why::Periodic => "while the camera was awake",
            Why::Wake => "it had not been looked at for a long time",
        }
    }
}

/// What the cadence goes by (epoch ms).
#[derive(Clone, Copy, Debug, Default)]
pub struct Cadence {
    /// When this camera's catching up began (the server's start, or since it was turned on).
    pub since_ms: i64,
    pub last_look_ms: Option<i64>,
    pub reconnected_ms: Option<i64>,
    /// A look is due again from this time (a push without a recording, a look cut short).
    pub again_at_ms: Option<i64>,
    /// The camera is awake now (its session is signed in).
    pub awake: bool,
    /// `catch_up_every` (0: no periodic looks).
    pub every_min: u32,
    /// Looks in a row that listed nothing.
    pub empty_runs: u32,
    /// Looks in a row that could not list the card (camera unreachable, it
    /// stopped answering, its answers made no sense).
    pub failed_runs: u32,
    /// Where the next look starts on the card (epoch ms): what the last one
    /// did not get to, or a little before where it ended. None: LOOK_BACK_MS ago.
    pub resume_ms: Option<i64>,
}

/// Whether to look at the card now, and why.
pub fn due(c: &Cadence, now_ms: i64) -> Option<Why> {
    let Some(last) = c.last_look_ms else {
        // The first look: when the camera is next awake, or a wake of its own a few minutes in.
        return (c.awake || now_ms - c.since_ms >= FIRST_LOOK_AFTER_MS).then_some(Why::Start);
    };
    let since = now_ms - last;
    if since < MIN_SPACING_MS {
        return None;
    }
    if c.reconnected_ms.is_some_and(|r| r > last) {
        return Some(Why::Reconnect);
    }
    // A card that keeps failing to list: never woken for it again (a look while the camera is
    // awake anyway costs no battery, so a missed push is still looked after then).
    let failing = c.failed_runs >= FAIL_LIMIT;
    if (c.awake || !failing) && c.again_at_ms.is_some_and(|a| a <= now_ms && a > last) {
        return Some(Why::Again);
    }
    if c.every_min == 0 {
        return None;
    }
    let period = c.every_min as i64 * 60_000;
    if since >= period && c.awake {
        return Some(Why::Periodic);
    }
    if since >= period * WAKE_EVERY_PERIODS && c.empty_runs < EMPTY_LIMIT && !failing {
        return Some(Why::Wake);
    }
    None
}

// ---- what is missing ----

/// What to do with one recording on the card.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Verdict {
    /// AmberSight recorded it (or a part of it).
    Present,
    /// An event is here but no recording: import the video for it.
    VideoOnly,
    /// Missing: import it as an event.
    Import,
    /// Left alone, for good.
    Skip(&'static str),
}

/// Compare one card recording (ms) with the camera's events (s) and
/// recordings (ms) here.
pub fn judge(clip: (i64, i64), events_s: &[(i64, i64)], recordings_ms: &[(i64, i64)]) -> Verdict {
    let (a, b) = clip;
    if b <= a || b - a > MAX_CLIP_MS {
        return Verdict::Skip("longer than a battery camera's recording");
    }
    // Recorded here, if only in part (the live recording began late, the
    // lead-in filled the start): never written over.
    if recordings_ms.iter().any(|&(s, e)| s < b - 1_000 && e > a + 1_000) {
        return Verdict::Present;
    }
    // An event here (with a little room: the push lags the trigger).
    if events_s.iter().any(|&(s, e)| s * 1000 <= b + 30_000 && e * 1000 >= a - 30_000) {
        return Verdict::VideoOnly;
    }
    Verdict::Import
}

// ---- whether the camera was meant to record then ----

/// Whether camera `h` would make an event of a trigger now: on, recording
/// under the active profile, armed, motion detection on.
fn records_events(engine: &Engine, h: &CameraHandle, cfg: &CameraConfig) -> bool {
    let eff = engine.effective_camera(cfg);
    let o = engine.camera_override(&cfg.name);
    h.active.load(Ordering::Relaxed) && eff.record && h.running.read().unwrap().record && cfg.motion && o.motion != Some(false) && engine.armed_now()
}

/// Note for every battery camera whether it records events now (catching up
/// goes by when that changed). Called each pass and where arming or settings change.
pub fn note_all(engine: &Engine) {
    if super::card_fill::stopping() {
        return;
    }
    let now = Utc::now().timestamp_millis();
    let handles: Vec<Arc<CameraHandle>> = engine.cameras.read().unwrap().iter().flatten().cloned().collect();
    for h in handles {
        let cfg = h.cfg.read().unwrap().clone();
        if cfg.battery_uid().is_some() {
            super::card_fill::note_mode_in(&engine.db, MODE_SCOPE, &cfg.name, records_events(engine, &h, &cfg), now);
        }
    }
}

/// Whether the camera recorded events all through [from, to] as far as can
/// be known: so since before it began (armed, recording on), and the weekly
/// schedule's profile kept recording on for each of its minutes.
fn meant_recording(engine: &Engine, cfg: &CameraConfig, from_ms: i64, to_ms: i64) -> bool {
    let gap = Gap { from_ms, to_ms };
    if !super::card_fill::mode_allows(super::card_fill::read_mode_in(&engine.db, MODE_SCOPE, &slug(&cfg.name)), gap) {
        return false;
    }
    let auto = engine.config.lock().unwrap().automation.clone();
    let hold = super::profiles::manual_hold(engine).map(|(name, since, _)| (name, since));
    super::card_fill::schedule_keeps(&auto, cfg, gap, hold.as_ref(), local_minute, |o| o.record.unwrap_or(cfg.record) && o.motion != Some(false))
}

fn local_minute(ms: i64) -> Option<(usize, u32)> {
    use chrono::{Datelike, TimeZone, Timelike};
    let t = chrono::Local.timestamp_millis_opt(ms).single()?;
    Some((t.weekday().num_days_from_monday() as usize, t.hour() * 60 + t.minute()))
}

// ---- pushes that brought nothing ----

/// The last push of each camera (by name) not yet looked at: when it came.
static PUSHES: StdMutex<Option<HashMap<String, i64>>> = StdMutex::new(None);

/// A battery camera's push came (pushx): whether it brought a recording is
/// looked at a few minutes later.
pub fn push_seen(name: &str, at_ms: i64) {
    PUSHES.lock().unwrap_or_else(|e| e.into_inner()).get_or_insert_with(Default::default).insert(name.to_string(), at_ms);
}

/// A push this long ago is looked at: by then its recording is in the index.
const PUSH_JUDGED_AFTER_MS: i64 = 3 * 60_000;

/// The push of this camera that is old enough to judge (taken off the list).
fn take_push(name: &str, now_ms: i64) -> Option<i64> {
    let mut g = PUSHES.lock().unwrap_or_else(|e| e.into_inner());
    let m = g.as_mut()?;
    let at = *m.get(name)?;
    if now_ms - at < PUSH_JUDGED_AFTER_MS {
        return None;
    }
    m.remove(name);
    Some(at)
}

// ---- the loop ----

/// One check at a time on this server (also what shutdown waits for).
static BUSY: StdMutex<()> = StdMutex::new(());

/// At shutdown (after card_fill::begin_stop): wait up to `wait` for a check
/// under way to end; true when it did.
pub fn stop_and_wait(wait: std::time::Duration) -> bool {
    let until = std::time::Instant::now() + wait;
    loop {
        match BUSY.try_lock() {
            Ok(_) | Err(std::sync::TryLockError::Poisoned(_)) => return true,
            Err(std::sync::TryLockError::WouldBlock) => {}
        }
        if std::time::Instant::now() >= until {
            return false;
        }
        thread::sleep(std::time::Duration::from_millis(50));
    }
}

fn stopping() -> bool {
    super::card_fill::stopping()
}

pub fn card_catchup_loop(engine: Arc<Engine>) {
    // Not in the first minute after a start: the cameras are connecting.
    thread::sleep(std::time::Duration::from_secs(60));
    let mut seen: HashMap<usize, Cadence> = HashMap::new();
    while !stopping() {
        note_all(&engine);
        let handles: Vec<Arc<CameraHandle>> = engine.cameras.read().unwrap().iter().flatten().cloned().collect();
        for h in handles {
            if stopping() {
                return;
            }
            watch(&engine, &h, &mut seen);
        }
        seen.retain(|id, _| engine.camera(*id).is_some());
        thread::sleep(std::time::Duration::from_secs(20));
    }
}

/// Why nothing is imported from this camera's card, when its masks go into
/// its recordings (the card's clips have no masks).
pub fn masked_skip(cfg: &CameraConfig) -> Option<&'static str> {
    cfg.mask_in_recording().then_some(super::card_fill::MASKED_CARD)
}

/// One camera's turn.
fn watch(engine: &Arc<Engine>, h: &Arc<CameraHandle>, seen: &mut HashMap<usize, Cadence>) {
    let cfg = h.cfg.read().unwrap().clone();
    let now = Utc::now().timestamp_millis();
    let Some(uid) = cfg.battery_uid() else { return };
    if !cfg.catch_up_on() {
        clear_status(&cfg.name);
        seen.remove(&h.id);
        return;
    }
    let c = seen.entry(h.id).or_insert(Cadence { since_ms: now, ..Default::default() });
    c.every_min = cfg.catch_up_minutes();
    if !h.active.load(Ordering::Relaxed) || engine.camera(h.id).is_none_or(|x| !Arc::ptr_eq(&x, h)) {
        return;
    }
    if !(cfg.record && engine.effective_camera(&cfg).record) {
        set_status(&cfg.name, |s| {
            s.state = "unavailable".into();
            s.text = "Recording is off for this camera.".into();
        });
        return;
    }
    if let Some(why) = masked_skip(&cfg) {
        set_status(&cfg.name, |s| {
            s.state = "unavailable".into();
            s.text = why.into();
        });
        return;
    }
    let Some(client) = crate::ingest::baichuan::registry::get(&uid) else {
        set_status(&cfg.name, |s| {
            s.state = "waiting".into();
            s.text = "Waiting for the camera's session to start.".into();
        });
        return;
    };
    c.awake = client.is_connected();
    c.reconnected_ms = client.reconnected_at_ms();
    // A push some minutes ago that brought no recording, while it was meant to record.
    if let Some(p) = take_push(&cfg.name, now) {
        let recorded = engine.db.segments_overlapping(&slug(&cfg.name), p - 10_000, p + 120_000).map(|r| !r.is_empty()).unwrap_or(true);
        if !recorded && meant_recording(engine, &cfg, p - 10_000, p) {
            info!("camera '{}': a push brought no recording; looking at the camera's card for it", cfg.name);
            c.again_at_ms = Some(now + READY_AFTER_MS);
        }
    }
    // Recordings on disk not in the index yet (a start-up walk) would look missing.
    if engine.db.importing() {
        set_status(&cfg.name, |s| {
            s.state = "waiting".into();
            s.text = "Waiting for the recordings on disk to be indexed.".into();
        });
        return;
    }
    let Some(why) = due(c, now) else {
        set_status(&cfg.name, |s| {
            if s.state.is_empty() || s.state == "waiting" || s.state == "unavailable" {
                s.state = "watching".into();
                s.text = "Looks at the card after a start, a reconnect or a missed push, and about hourly while the camera is awake.".into();
            }
        });
        return;
    };
    let _busy = match BUSY.try_lock() {
        Ok(g) => g,
        Err(std::sync::TryLockError::Poisoned(p)) => p.into_inner(),
        Err(std::sync::TryLockError::WouldBlock) => return,
    };
    if stopping() {
        return;
    }
    c.last_look_ms = Some(now);
    let outcome = match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| check(engine, h, &cfg, &client, why, c.resume_ms))) {
        Ok(o) => o,
        Err(_) => {
            cardfill::clear_staging(&segments_dir(engine, &cfg));
            warn!("camera '{}': looking at the camera's card failed (an internal error)", cfg.name);
            Looked { failed: true, trouble: true, ..Default::default() }
        }
    };
    after_look(c, &outcome, Utc::now().timestamp_millis());
    if outcome.listed == 0 && !outcome.failed && c.empty_runs == EMPTY_LIMIT {
        info!("camera '{}': its card listed nothing {EMPTY_LIMIT} times in a row; it is not woken to look any more (only when awake anyway, or after a start or reconnect)", cfg.name);
    }
    if outcome.trouble && c.failed_runs == FAIL_LIMIT {
        info!("camera '{}': its card could not be listed {FAIL_LIMIT} times in a row; it is not woken to look any more (only when awake anyway, or after a start or reconnect)", cfg.name);
        set_status(&cfg.name, |s| {
            s.state = "problem".into();
            s.text = format!("{} Not woken to look again until the camera reconnects or AmberSight restarts.", s.text);
        });
    }
}

/// The cadence after a look.
fn after_look(c: &mut Cadence, outcome: &Looked, now_ms: i64) {
    c.empty_runs = if outcome.listed == 0 && !outcome.failed { c.empty_runs + 1 } else { 0 };
    c.failed_runs = if outcome.trouble { c.failed_runs + 1 } else { 0 };
    if outcome.again {
        c.again_at_ms = Some(now_ms + AGAIN_AFTER_MS);
    }
    if !outcome.failed {
        c.resume_ms = outcome.resume_ms;
    }
}

/// How a look went.
#[derive(Debug, Default)]
struct Looked {
    listed: usize,
    imported: u32,
    /// Cut short (live video, the budget ran out, a recording placed
    /// unsure): look again a little later.
    again: bool,
    /// Could not reach the camera or list its card: nothing to go on.
    failed: bool,
    /// The listing did not get through for the camera's own reasons
    /// (unreachable, stopped answering, answers that made no sense), even if
    /// it listed some: counts toward FAIL_LIMIT, and is never looked at
    /// again soon for it.
    trouble: bool,
    /// Where the next look starts (see Cadence::resume_ms).
    resume_ms: Option<i64>,
}

/// What a listing alone says of a look: looked at again soon only when it was
/// cut short for live video or by its own limits; ended early by the camera,
/// it is trouble (and with nothing listed, a failed look).
fn listing_outcome(list: &crate::ingest::baichuan::client::ClipList) -> Looked {
    let camera_stopped = !list.complete && !list.gave_way && !list.ran_out;
    Looked {
        listed: list.clips.len(),
        again: list.gave_way || list.ran_out,
        failed: list.clips.is_empty() && !list.complete,
        trouble: camera_stopped,
        ..Default::default()
    }
}

/// Where the next look starts: at the first recording left for later
/// (`left`), else where this walk stopped, else (it walked to `to`) a little
/// before `to`, so a recording closed late is not missed.
fn resume_at(left: Option<i64>, complete: bool, last_end: Option<i64>, from: i64, to: i64) -> i64 {
    match left {
        Some(l) => l - 1_000,
        None if complete => to - 10 * 60_000,
        None => last_end.unwrap_or(from),
    }
    .max(from)
}

fn segments_dir(engine: &Engine, cfg: &CameraConfig) -> std::path::PathBuf {
    let (_, folder_root) = engine.record_folder(cfg);
    folder_root.join("cameras").join(slug(&cfg.name)).join("segments")
}

fn clock_text(ms: i64) -> String {
    chrono::DateTime::from_timestamp_millis(ms).map(|t| t.with_timezone(&chrono::Local).format("%H:%M").to_string()).unwrap_or_default()
}

/// What one look may still take.
struct Budget {
    deadline: std::time::Instant,
    bytes_left: u64,
    imports_left: usize,
}

impl Budget {
    fn spent(&self) -> bool {
        self.imports_left == 0 || self.bytes_left == 0 || std::time::Instant::now() >= self.deadline
    }
}

/// Look at the card and import what is missing.
fn check(engine: &Arc<Engine>, h: &Arc<CameraHandle>, cfg: &CameraConfig, client: &Arc<BaichuanClient>, why: Why, resume_ms: Option<i64>) -> Looked {
    let name = cfg.name.clone();
    let slug_name = slug(&name);
    set_status(&name, |s| {
        s.state = "checking".into();
        s.text = "Looking at the camera's card for events AmberSight missed.".into();
    });
    info!("camera '{name}': looking at the camera's card for missed events ({})", why.text());
    let now = Utc::now().timestamp_millis();
    let to = now - READY_AFTER_MS;
    let from = resume_ms.unwrap_or(i64::MIN).max(now - LOOK_BACK_MS).min(to - 60_000);
    let Some(list) = client.list_clips(from, to, MAX_LISTED, MAX_ASKS) else {
        set_status(&name, |s| {
            s.state = "problem".into();
            s.text = "Could not reach the camera to look at its card. Trying again when it is back.".into();
            s.last_check = Some(now);
        });
        return Looked { failed: true, trouble: true, ..Default::default() };
    };
    let mut out = listing_outcome(&list);
    if out.failed {
        set_status(&name, |s| {
            s.state = "problem".into();
            s.text = format!("Could not list the camera's card ({}).", list.why.as_deref().unwrap_or("no answer"));
            s.last_check = Some(now);
        });
        out.failed = true;
        return out;
    }
    let events = engine.db.event_spans(&name, from / 1000 - 60, now / 1000).unwrap_or_default();
    let recordings: Vec<(i64, i64)> = engine.db.segments_overlapping(&slug_name, from - 60_000, now).unwrap_or_default().iter().map(|r| (r.start_ms, r.end_ms)).collect();
    let tried = super::card_fill::load_tried_in(&engine.db, SCOPE, &slug_name);
    let mut budget = Budget { deadline: std::time::Instant::now() + MAX_TIME, bytes_left: MAX_BYTES, imports_left: MAX_IMPORTS };
    let (mut not_meant, mut failed) = (0u32, 0u32);
    // The first recording left for a later look.
    let mut left: Option<i64> = None;
    let mut keep = |ms: i64| left = Some(left.map_or(ms, |l: i64| l.min(ms)));
    for clip in &list.clips {
        if stopping() || !h.active.load(Ordering::Relaxed) {
            out.again = true;
            keep(clip.start_ms);
            break;
        }
        let gap = Gap { from_ms: clip.start_ms, to_ms: clip.end_ms };
        let verdict = judge((clip.start_ms, clip.end_ms), &events, &recordings);
        if matches!(verdict, Verdict::Present) || already_handled(&tried, gap, now) {
            continue;
        }
        // The camera's clock is read again for each listing, so the same
        // recording can be listed a second off: kept under the key it was first saved under.
        let before = earlier_try(&tried, gap);
        let (key_from, key_to) = before.map_or((clip.start_ms, clip.end_ms), |a| (a.from_ms, a.to_ms));
        let mark = |what: Tried| super::card_fill::save_tried_in(&engine.db, SCOPE, &slug_name, Attempt { from_ms: key_from, to_ms: key_to, what }, Utc::now().timestamp_millis());
        if let Verdict::Skip(w) = verdict {
            info!("camera '{name}': left the card's recording from {}: {w}", clock_text(clip.start_ms));
            mark(Tried::Empty);
            continue;
        }
        if !clip.sure {
            // Which clock its header is on was not settled: placed maybe hours off.
            out.again = true;
            keep(clip.start_ms);
            continue;
        }
        if !meant_recording(engine, cfg, clip.start_ms, clip.end_ms) {
            not_meant += 1;
            mark(Tried::Empty);
            continue;
        }
        // The card's clock was read now: a recording from before a daylight-saving change would be placed an hour off.
        if !cardfill::same_offset(cardfill::local_offset, &[clip.start_ms, clip.end_ms, now]) {
            info!("camera '{name}': left the card's recording from {}: the clocks changed for daylight saving time since", clock_text(clip.start_ms));
            mark(Tried::Empty);
            continue;
        }
        if budget.spent() {
            out.again = true;
            keep(clip.start_ms);
            break;
        }
        let n = before.map(|a| if let Tried::Failed(n, _) = a.what { n } else { 0 }).unwrap_or(0) + 1;
        // Written down before the camera is asked: work that crashes the
        // server counts as a try, so it is not repeated after every restart.
        mark(Tried::Failed(n, Utc::now().timestamp_millis()));
        set_status(&name, |s| {
            s.state = "checking".into();
            s.text = format!("Importing the camera's recording from {} from its card.", clock_text(clip.start_ms));
        });
        match import(engine, h, cfg, client, clip, verdict == Verdict::VideoOnly, &mut budget) {
            Ok(Imported::Event) => {
                mark(Tried::Done);
                out.imported += 1;
            }
            Ok(Imported::Video) => mark(Tried::Done),
            Ok(Imported::Nothing(w)) => {
                info!("camera '{name}': nothing imported for the card's recording from {}: {w}", clock_text(clip.start_ms));
                mark(Tried::Empty);
            }
            Err(Failure::GaveWay) => {
                super::card_fill::restore_tried_in(&engine.db, SCOPE, &slug_name, key_from, before);
                out.again = true;
                keep(clip.start_ms);
                break;
            }
            Err(Failure::Failed(e)) => {
                failed += 1;
                keep(clip.start_ms);
                warn!("camera '{name}': could not import the card's recording from {} ({e:#}); try {n} of 3", clock_text(clip.start_ms));
            }
        }
    }
    cardfill::clear_staging(&segments_dir(engine, cfg));
    out.resume_ms = Some(resume_at(left, list.complete, list.clips.last().map(|c| c.end_ms), from, to));
    let done_ms = Utc::now().timestamp_millis();
    if out.imported > 0 {
        info!("camera '{name}': imported {} event(s) from the camera's card", out.imported);
    }
    if not_meant > 0 {
        info!("camera '{name}': left {not_meant} recording(s) on the card from times the camera was not armed or recording");
    }
    let imported = out.imported;
    set_status(&name, |s| {
        s.state = if failed > 0 { "problem".into() } else { "watching".into() };
        s.last_check = Some(done_ms);
        if imported > 0 {
            s.last_import = Some(done_ms);
            s.last_imported = imported;
            s.imported += imported;
        }
        let what = match (imported, s.last_import) {
            (0, Some(at)) => format!("Looked at the card at {}: nothing missing. Last imported {} event{} at {}.", clock_text(done_ms), s.last_imported, if s.last_imported == 1 { "" } else { "s" }, clock_text(at)),
            (0, None) => format!("Looked at the card at {}: nothing missing.", clock_text(done_ms)),
            (n, _) => format!("Imported {n} event{} from the card at {}.", if n == 1 { "" } else { "s" }, clock_text(done_ms)),
        };
        s.text = if failed > 0 { format!("{what} {failed} could not be imported; trying again later.") } else { what };
    });
    out
}

/// What was tried before for the recording `gap`, matched as already_handled
/// matches (within 2 s: the camera's clock offset is read to the second for
/// each listing); of several, the one with the most failed tries.
fn earlier_try(tried: &[Attempt], gap: Gap) -> Option<Attempt> {
    tried
        .iter()
        .filter(|a| a.from_ms <= gap.from_ms + 2_000 && a.to_ms >= gap.to_ms - 2_000 && a.from_ms >= gap.from_ms - 2_000 && a.to_ms <= gap.to_ms + 2_000)
        .max_by_key(|a| if let Tried::Failed(n, _) = a.what { n } else { u32::MAX })
        .copied()
}

/// What importing one recording did.
#[derive(Debug, PartialEq)]
enum Imported {
    /// A new event, with its recording.
    Event,
    /// The recording, for an event that had none.
    Video,
    /// Nothing worth keeping (a reason).
    Nothing(String),
}

enum Failure {
    /// Stopped for live video: not a failed try.
    GaveWay,
    Failed(anyhow::Error),
}

impl From<anyhow::Error> for Failure {
    fn from(e: anyhow::Error) -> Self {
        Failure::Failed(e)
    }
}

/// Download one recording from the card, write it as a triggered recording
/// marked as from the card, and make its event (unless `video_only`).
fn import(engine: &Arc<Engine>, h: &Arc<CameraHandle>, cfg: &CameraConfig, client: &BaichuanClient, clip: &ListedClip, video_only: bool, budget: &mut Budget) -> Result<Imported, Failure> {
    use anyhow::anyhow;
    let name = cfg.name.clone();
    let slug_name = slug(&name);
    let secs = ((clip.end_ms - clip.start_ms) / 1000).max(1) as u64;
    // Downloads run at about half real time: room for that and the wake.
    let limit = std::time::Duration::from_secs(secs * 4 + 60).min(budget.deadline.saturating_duration_since(std::time::Instant::now()));
    let max = budget.bytes_left.min(MAX_CLIP_BYTES) as usize;
    let d = client.download_within(clip.start_ms, clip.end_ms, limit, max).ok_or_else(|| anyhow!("could not reach the camera"))?;
    budget.bytes_left = budget.bytes_left.saturating_sub(d.bytes.len() as u64);
    budget.imports_left = budget.imports_left.saturating_sub(1);
    if d.bytes.is_empty() {
        if d.aborted {
            return Err(Failure::GaveWay);
        }
        return Err(anyhow!("the camera sent nothing").into());
    }
    let mut demux = Demux::new();
    let frames = demux.push(&d.bytes);
    let header = demux.clip_start.ok_or_else(|| anyhow!("the recording carried no start time"))?;
    let candidates = [d.card_offset_s, client.camera_clock_offset_s(), Some(0)];
    let start_ms = placed_start(header, &candidates, clip.start_ms).ok_or_else(|| anyhow!("the camera sent another recording than the one listed"))?;
    // Only this recording (the stream may run on into the next one).
    let keep = super::battery_clips::lead_in_frames(&frames, start_ms, clip.end_ms + 1_000);
    if keep.last().map_or(0, |l| l.0 - keep[0].0) < 1_000 {
        return Ok(Imported::Nothing("less than a second of video".into()));
    }
    let (from_ms, to_ms) = (keep[0].0, keep.last().unwrap().0);
    // (A rename of the camera waits for what is written now; one that came
    // first stopped the camera.)
    let _writing = engine.writing_into(&name);
    if !h.active.load(Ordering::Relaxed) || stopping() {
        return Err(Failure::GaveWay);
    }
    // Written beside the recordings folder, moved in only once kept.
    let dir = segments_dir(engine, cfg);
    let staging = cardfill::staging_dir(&dir);
    cardfill::clear_staging(&dir);
    let segment_seconds = engine.config.lock().unwrap().storage.segment_seconds;
    let (segs, _) = super::battery_clips::write_frames(&staging, &format!("{slug_name}-card-{from_ms}-rec"), &demux, &frames, &keep, segment_seconds)?;
    let mut kept = 0;
    for seg in segs {
        // Never on top of what is recorded (the live recording may have begun meanwhile).
        let clash = engine.db.segments_overlapping(&slug_name, seg.start_ms + 1_000, seg.end_ms - 1_000).map(|r| !r.is_empty()).unwrap_or(true);
        if clash {
            let _ = std::fs::remove_file(&seg.path);
            continue;
        }
        let seg = cardfill::commit_piece(seg, &dir).map_err(|e| anyhow!("could not keep the recording: {e}"))?;
        engine.segment_closed(&slug_name, seg);
        kept += 1;
    }
    cardfill::clear_staging(&dir);
    if kept == 0 {
        return Ok(Imported::Nothing("recorded here meanwhile".into()));
    }
    engine.wait_for_indexing(std::time::Duration::from_secs(10));
    let (start_s, end_s) = (from_ms.div_euclid(1000), (to_ms + 999).div_euclid(1000));
    if video_only {
        // The event that had no recording plays it now.
        set_clip_of_events(engine, &name, start_s, end_s);
        info!("camera '{name}': added the camera's recording from {} to its event", clock_text(from_ms));
        return Ok(Imported::Video);
    }
    let id = format!("{slug_name}-{start_s}");
    if engine.db.get_event(&id).ok().flatten().is_some() {
        return Ok(Imported::Video);
    }
    let (thumb, ai) = look_at(engine, cfg, from_ms, to_ms);
    let motion_cfg = engine.config.lock().unwrap().motion.for_camera(cfg);
    let notify = Utc::now().timestamp_millis() - to_ms <= NOTIFY_WITHIN_MS;
    super::clip::finalize_event(
        engine,
        h,
        cfg,
        segment_seconds,
        start_s,
        end_s,
        0.0,
        thumb,
        None,
        ai,
        &motion_cfg,
        &slug_name,
        &dir,
        &engine.data_dir.join("clips"),
        &engine.data_dir.join("thumbs"),
        Some(super::clip::CardImport { notify }),
    );
    Ok(Imported::Event)
}

/// The epoch ms of the downloaded recording's header start, read on the
/// clock that puts it at the listed start; None when no clock puts it within
/// 5 s of it (the camera sent another recording).
fn placed_start(header: chrono::NaiveDateTime, offsets: &[Option<i64>], listed_ms: i64) -> Option<i64> {
    super::battery_clips::clip_start_epoch_ms(header, offsets, listed_ms).filter(|ms| (ms - listed_ms).abs() <= 5_000)
}

/// Events of the camera around [start_s, end_s] that have no clip get the
/// recording's address.
fn set_clip_of_events(engine: &Engine, name: &str, start_s: i64, end_s: i64) {
    let q = crate::db::EventQuery { camera: Some(name.to_string()), since: Some(start_s - 120), until: Some(end_s + 30), limit: 8, ..Default::default() };
    for mut ev in engine.db.query_events(&q).unwrap_or_default() {
        if ev.clip.is_none() && ev.end >= start_s - 30 {
            ev.clip = Some(format!("/api/clips/{}", ev.id));
            if let Err(e) = engine.db.insert_event(&ev) {
                warn!("could not update event {}: {e:#}", ev.id);
            }
        }
    }
}

/// The thumbnail and the AI check of a recording just imported: pictures
/// from it, two a second, through the same AI checks a live event gets.
fn look_at(engine: &Arc<Engine>, cfg: &CameraConfig, from_ms: i64, to_ms: i64) -> (Vec<u8>, super::ai_check::AiOutcome) {
    let wanted = super::ai_check::wanted_labels(engine, cfg);
    let mut state = super::ai_check::AiState::default();
    state.parked = super::ai_check::restore_parked(engine, &cfg.name);
    let ai_on = super::ai_check::ai_active(engine, cfg);
    let clip = match super::playback::window_clip(&engine.db, &cfg.name, from_ms, to_ms + 1_000, |r| engine.segment_path(r)) {
        Ok(Some(c)) => c,
        _ => return (Vec::new(), super::ai_check::AiOutcome::take(&mut state, &wanted)),
    };
    let start = clip.start_ms;
    let mut thumb: Vec<u8> = Vec::new();
    let max_checks = engine.config.lock().unwrap().ai.max_checks;
    // (AI and the thumbnail see the camera's privacy masks blacked out.)
    let masks = crate::privacy::camera_mask_filter(cfg);
    let r = super::playback::clip_frames(clip, 2, 640, &masks, &mut |n, jpeg| {
        let at = start + n as i64 * 500;
        // The picture a second in (the object has come into view by then).
        if thumb.is_empty() && (n >= 2 || at >= to_ms) {
            thumb = jpeg.to_vec();
        }
        if !ai_on || !state.may_look(&wanted, max_checks, cfg.packages == Some(true)) {
            return if thumb.is_empty() { Ok(()) } else { Err(anyhow::anyhow!("enough")) };
        }
        let looks = state.log.len();
        let confirmed = |s: &super::ai_check::AiState| s.found.iter().any(|d| wanted.contains(&d.label));
        let before = confirmed(&state);
        state.last = None; // (spaced by the pictures' own times here)
        super::ai_check::maybe_check(engine, cfg, &mut state, jpeg);
        if state.log.len() > looks {
            if let Some(c) = state.log.last_mut() {
                c.at_ms = at;
            }
        }
        // The picture that confirmed a wanted object is the thumbnail.
        if !before && confirmed(&state) {
            thumb = jpeg.to_vec();
        }
        Ok(())
    });
    if let Err(e) = r {
        if e.to_string() != "enough" {
            tracing::debug!("camera '{}': pictures of the card's recording: {e:#}", cfg.name);
        }
    }
    (thumb, super::ai_check::AiOutcome::take(&mut state, &wanted))
}

#[cfg(test)]
mod tests {
    use super::*;

    const MIN: i64 = 60_000;

    #[test]
    fn missing_recordings_are_told_from_present_ones() {
        let s = |a: i64, b: i64| (a * 1000, b * 1000);
        let recs = [s(1_000, 1_060), s(2_000, 2_005)];
        let evs = [(3_000, 3_020)];
        // Recorded here (in full, or only its end: the live recording began late).
        assert_eq!(judge(s(1_000, 1_060), &evs, &recs), Verdict::Present);
        assert_eq!(judge(s(1_990, 2_030), &evs, &recs), Verdict::Present);
        // A recording that only touches one here by a second is not that one.
        assert_eq!(judge(s(1_059, 1_120), &[], &recs), Verdict::Import);
        // An event without a recording (a push whose wake failed): the video only.
        assert_eq!(judge(s(3_005, 3_050), &evs, &recs), Verdict::VideoOnly);
        assert_eq!(judge(s(3_040, 3_080), &evs, &recs), Verdict::VideoOnly, "the push lags the trigger");
        // Nothing here at all.
        assert_eq!(judge(s(5_000, 5_045), &evs, &recs), Verdict::Import);
        // Not believable.
        assert!(matches!(judge(s(5_000, 5_000), &evs, &recs), Verdict::Skip(_)));
        assert!(matches!(judge(s(5_000, 5_000 + 3_600), &evs, &recs), Verdict::Skip(_)));
    }

    #[test]
    fn the_card_is_looked_at_rarely() {
        let t0 = 1_800_000_000_000i64;
        let c = Cadence { since_ms: t0, every_min: 60, ..Default::default() };
        // After a start: when it is awake, else one wake a few minutes in.
        assert_eq!(due(&c, t0 + MIN), None);
        assert_eq!(due(&Cadence { awake: true, ..c }, t0 + MIN), Some(Why::Start));
        assert_eq!(due(&c, t0 + FIRST_LOOK_AFTER_MS), Some(Why::Start));
        let looked = Cadence { last_look_ms: Some(t0), ..c };
        // Never twice within a quarter hour, whatever happens.
        let busy = Cadence { awake: true, reconnected_ms: Some(t0 + MIN), again_at_ms: Some(t0 + MIN), ..looked };
        assert_eq!(due(&busy, t0 + 14 * MIN), None);
        // A reconnect or a push without a recording: soon after.
        assert_eq!(due(&busy, t0 + 15 * MIN), Some(Why::Reconnect));
        assert_eq!(due(&Cadence { reconnected_ms: None, ..busy }, t0 + 15 * MIN), Some(Why::Again));
        // An earlier reconnect (before the last look) does not count again.
        assert_eq!(due(&Cadence { reconnected_ms: Some(t0 - MIN), ..looked }, t0 + 30 * MIN), None);
        // Periodic: hourly, only while awake anyway; asleep, a wake of its own after three periods.
        assert_eq!(due(&Cadence { awake: true, ..looked }, t0 + 59 * MIN), None);
        assert_eq!(due(&Cadence { awake: true, ..looked }, t0 + 60 * MIN), Some(Why::Periodic));
        assert_eq!(due(&looked, t0 + 60 * MIN), None);
        assert_eq!(due(&looked, t0 + 179 * MIN), None);
        assert_eq!(due(&looked, t0 + 180 * MIN), Some(Why::Wake));
        // A card that lists nothing is not woken for any more (awake: still looked at).
        let empty = Cadence { empty_runs: EMPTY_LIMIT, ..looked };
        assert_eq!(due(&empty, t0 + 600 * MIN), None);
        assert_eq!(due(&Cadence { awake: true, ..empty }, t0 + 60 * MIN), Some(Why::Periodic));
        // No periodic looks at all: only after a start, a reconnect, a missed push.
        let never = Cadence { every_min: 0, ..looked };
        assert_eq!(due(&Cadence { awake: true, ..never }, t0 + 600 * MIN), None);
        assert_eq!(due(&Cadence { reconnected_ms: Some(t0 + MIN), ..never }, t0 + 20 * MIN), Some(Why::Reconnect));
    }

    #[test]
    fn the_period_setting_is_never_under_an_hour() {
        let cam = |extra: &str| -> CameraConfig { toml::from_str(&format!("name = \"c\"\nurl = \"baichuan://192.0.2.9?uid=TESTUID000000001\"\n{extra}")).unwrap() };
        assert_eq!(cam("").catch_up_minutes(), 60);
        assert_eq!(cam("catch_up_every = 5").catch_up_minutes(), 60);
        assert_eq!(cam("catch_up_every = 0").catch_up_minutes(), 0);
        assert_eq!(cam("catch_up_every = 180").catch_up_minutes(), 180);
        assert_eq!(cam("catch_up_every = 99999").catch_up_minutes(), 24 * 60);
        // On by default for battery cameras, never for others.
        assert!(cam("").catch_up_on());
        assert!(!cam("catch_up_from_card = false").catch_up_on());
        let wired: CameraConfig = toml::from_str("name = \"c\"\nurl = \"rtsp://192.0.2.9/x\"\ncatch_up_from_card = true\n").unwrap();
        assert!(!wired.catch_up_on());
    }

    #[test]
    fn the_downloaded_recording_must_be_the_listed_one() {
        let t = |s: &str| chrono::NaiveDateTime::parse_from_str(s, "%Y-%m-%d %H:%M:%S").unwrap();
        let listed = t("2026-11-16 14:00:00").and_utc().timestamp_millis();
        // Header in local time (UTC-6), or in UTC: both placed.
        assert_eq!(placed_start(t("2026-11-16 08:00:00"), &[Some(-6 * 3600), None, Some(0)], listed), Some(listed));
        assert_eq!(placed_start(t("2026-11-16 14:00:01"), &[Some(-6 * 3600), None, Some(0)], listed), Some(listed + 1000));
        // Another recording (a minute later): refused.
        assert_eq!(placed_start(t("2026-11-16 08:01:00"), &[Some(-6 * 3600), Some(0)], listed), None);
    }

    fn cam(name: &str) -> CameraConfig {
        toml::from_str(&format!("name = \"{name}\"\nurl = \"baichuan://192.0.2.9?uid=TESTUID000000001\"\non_demand = true\n")).unwrap()
    }

    #[test]
    fn masks_in_the_recordings_stop_catching_up() {
        let mut c = cam("Shed");
        assert_eq!(masked_skip(&c), None);
        c.privacy_masks = vec![crate::config::ZoneRect { x: 0.1, y: 0.1, w: 0.2, h: 0.2 }];
        c.mask_mode = Some("screen".into());
        assert_eq!(masked_skip(&c), None);
        c.mask_mode = Some("recording".into());
        assert!(masked_skip(&c).is_some_and(|w| w.contains("Hide in recordings too")));
    }

    #[test]
    fn recordings_from_times_the_camera_was_not_armed_or_recording_are_left() {
        let db = crate::db::Db::open_in_memory().unwrap();
        let mode = |db: &crate::db::Db| super::super::card_fill::read_mode_in(db, MODE_SCOPE, "shed");
        let allows = |db: &crate::db::Db, a: i64, b: i64| super::super::card_fill::mode_allows(mode(db), Gap { from_ms: a, to_ms: b });
        // Never seen: nothing imported.
        assert!(!allows(&db, 10_000, 20_000));
        // Disarmed until 50 s, armed from then on.
        super::super::card_fill::note_mode_in(&db, MODE_SCOPE, "Shed", false, 1_000);
        super::super::card_fill::note_mode_in(&db, MODE_SCOPE, "Shed", true, 50_000);
        super::super::card_fill::note_mode_in(&db, MODE_SCOPE, "Shed", true, 90_000);
        assert!(!allows(&db, 10_000, 20_000), "disarmed then");
        assert!(!allows(&db, 40_000, 60_000), "armed only partway");
        assert!(allows(&db, 60_000, 80_000));
        // Card fill's own record of the same camera is apart from this one.
        assert!(super::super::card_fill::read_mode_in(&db, "card_fill_mode", "shed").is_none());
        // A profile that turns recording or motion off for some hours.
        use crate::config::{AutomationConfig, CameraOverride, ProfileConfig, ScheduleRule};
        let mut away = ProfileConfig { name: "Home".into(), ..Default::default() };
        away.cameras.insert("Shed".into(), CameraOverride { record: Some(false), ..Default::default() });
        let mut quiet = ProfileConfig { name: "Quiet".into(), ..Default::default() };
        quiet.cameras.insert("Shed".into(), CameraOverride { motion: Some(false), ..Default::default() });
        let auto = AutomationConfig {
            profiles: vec![away, quiet],
            schedule: vec![
                ScheduleRule { days: Vec::new(), from: "12:00".into(), to: "13:00".into(), profile: "Home".into() },
                ScheduleRule { days: Vec::new(), from: "18:00".into(), to: "19:00".into(), profile: "Quiet".into() },
            ],
            ..Default::default()
        };
        let local = |ms: i64| Some((0usize, (ms / 60_000) as u32));
        let at = |h: i64, m: i64| (h * 60 + m) * 60_000;
        let rat = cam("Shed");
        let keeps = |a: i64, b: i64| super::super::card_fill::schedule_keeps(&auto, &rat, Gap { from_ms: a, to_ms: b }, None, local, |o| o.record.unwrap_or(rat.record) && o.motion != Some(false));
        assert!(keeps(at(8, 0), at(8, 1)));
        assert!(!keeps(at(12, 10), at(12, 11)), "recording off then");
        assert!(!keeps(at(11, 59), at(12, 0)), "into the hour recording was off");
        assert!(!keeps(at(18, 30), at(18, 31)), "motion off then");
        assert!(keeps(at(19, 0), at(19, 1)));
    }

    #[test]
    fn a_push_is_judged_once_a_few_minutes_later() {
        let t0 = 1_800_000_000_000i64;
        push_seen("Pushcam", t0);
        assert_eq!(take_push("Pushcam", t0 + MIN), None, "too soon: its recording may not be indexed yet");
        assert_eq!(take_push("Pushcam", t0 + PUSH_JUDGED_AFTER_MS), Some(t0));
        assert_eq!(take_push("Pushcam", t0 + 10 * MIN), None, "once");
        // A later push replaces an earlier one not judged yet.
        push_seen("Pushcam", t0 + MIN);
        push_seen("Pushcam", t0 + 2 * MIN);
        assert_eq!(take_push("Pushcam", t0 + 10 * MIN), Some(t0 + 2 * MIN));
    }

    #[test]
    fn what_was_tried_is_kept_apart_from_card_fill_and_not_repeated() {
        let db = crate::db::Db::open_in_memory().unwrap();
        let now = 100 * 3_600_000i64;
        let a = Attempt { from_ms: now - 600_000, to_ms: now - 540_000, what: Tried::Done };
        super::super::card_fill::save_tried_in(&db, SCOPE, "shed", a, now);
        let tried = super::super::card_fill::load_tried_in(&db, SCOPE, "shed");
        assert_eq!(tried, vec![a]);
        assert!(already_handled(&tried, Gap { from_ms: a.from_ms, to_ms: a.to_ms }, now));
        assert!(super::super::card_fill::load_tried_in(&db, "card_fill", "shed").is_empty());
        forget_camera(&db, "Shed");
        assert!(super::super::card_fill::load_tried_in(&db, SCOPE, "shed").is_empty());
    }

    #[test]
    fn the_next_look_starts_where_this_one_left_off() {
        let (from, to) = (1_000_000, 9_000_000);
        // A recording left for later (budget, failure): from just before it.
        assert_eq!(resume_at(Some(5_000_000), true, Some(8_000_000), from, to), 4_999_000);
        // The walk stopped partway (too many recordings): from the last one listed.
        assert_eq!(resume_at(None, false, Some(6_000_000), from, to), 6_000_000);
        assert_eq!(resume_at(None, false, None, from, to), from);
        // Walked to the end: a little before it, never before the window.
        assert_eq!(resume_at(None, true, Some(8_900_000), from, to), to - 600_000);
        assert_eq!(resume_at(None, true, None, 8_800_000, to), 8_800_000);
    }

    use crate::ingest::baichuan::client::ClipList;

    fn listed(n: usize) -> Vec<ListedClip> {
        (0..n as i64).map(|i| ListedClip { start_ms: i * MIN, end_ms: i * MIN + 30_000, sure: true }).collect()
    }

    #[test]
    fn a_listing_is_looked_at_again_soon_only_when_cut_short_on_this_side() {
        // Live video, or the limits of one look: again soon, not trouble.
        let o = listing_outcome(&ClipList { clips: listed(2), gave_way: true, ..Default::default() });
        assert!(o.again && !o.trouble && !o.failed);
        let o = listing_outcome(&ClipList { clips: listed(60), ran_out: true, ..Default::default() });
        assert!(o.again && !o.trouble && !o.failed);
        // Walked to the end: nothing more to do.
        let o = listing_outcome(&ClipList { clips: listed(3), complete: true, ..Default::default() });
        assert!(!o.again && !o.trouble && !o.failed);
        let o = listing_outcome(&ClipList { complete: true, ..Default::default() });
        assert!(!o.again && !o.trouble && !o.failed && o.listed == 0);
        // The camera stopped answering, or answered nonsense: trouble, not again.
        let o = listing_outcome(&ClipList { why: Some("the camera stopped answering".into()), ..Default::default() });
        assert!(!o.again && o.trouble && o.failed);
        let o = listing_outcome(&ClipList { clips: listed(2), why: Some("the camera's answer does not fit the time asked".into()), ..Default::default() });
        assert!(!o.again && o.trouble && !o.failed && o.listed == 2);
    }

    /// The looks a camera that is asleep throughout gets over `hours`, each
    /// going as `outcome` says.
    fn looks_while_asleep(c: &mut Cadence, from_ms: i64, hours: i64, outcome: &dyn Fn() -> Looked) -> Vec<(i64, Why)> {
        let mut looks = Vec::new();
        let mut now = from_ms;
        while now < from_ms + hours * 60 * MIN {
            if let Some(why) = due(c, now) {
                looks.push((now, why));
                c.last_look_ms = Some(now);
                after_look(c, &outcome(), now);
            }
            now += 20_000;
        }
        looks
    }

    #[test]
    fn a_camera_whose_card_keeps_failing_to_list_is_not_woken_for_it() {
        let t0 = 1_800_000_000_000i64;
        let failing: [&dyn Fn() -> Looked; 4] = [
            // Nothing listed: it stopped answering, or named a recording that cannot be.
            &|| listing_outcome(&ClipList { why: Some("the camera stopped answering".into()), ..Default::default() }),
            // Some listed, then answers that do not fit the time asked.
            &|| Looked { resume_ms: Some(t0), ..listing_outcome(&ClipList { clips: listed(2), why: Some("the camera's answer does not fit the time asked".into()), ..Default::default() }) },
            // The camera could not be reached at all.
            &|| Looked { failed: true, trouble: true, ..Default::default() },
            // Not listed fully, and what was listed was imported.
            &|| Looked { imported: 1, resume_ms: Some(t0), ..listing_outcome(&ClipList { clips: listed(1), why: Some("the camera's answers made no progress".into()), ..Default::default() }) },
        ];
        for (i, outcome) in failing.iter().enumerate() {
            let mut c = Cadence { since_ms: t0, every_min: 60, ..Default::default() };
            let looks = looks_while_asleep(&mut c, t0, 48, *outcome);
            // The first look, then two wakes of its own three hours apart; then none.
            let whys: Vec<Why> = looks.iter().map(|l| l.1).collect();
            assert_eq!(whys, vec![Why::Start, Why::Wake, Why::Wake], "case {i}: {looks:?}");
            assert_eq!(c.failed_runs, FAIL_LIMIT, "case {i}");
            // A push without a recording does not wake it for a look either.
            c.again_at_ms = Some(t0 + 48 * 60 * MIN + MIN);
            assert!(looks_while_asleep(&mut c, t0 + 48 * 60 * MIN, 24, *outcome).is_empty(), "case {i}");
            // A reconnect: one more look; still failing, none after it.
            let back = t0 + 72 * 60 * MIN;
            c.reconnected_ms = Some(back);
            let looks = looks_while_asleep(&mut c, back, 24, *outcome);
            assert_eq!(looks.iter().map(|l| l.1).collect::<Vec<_>>(), vec![Why::Reconnect], "case {i}: {looks:?}");
            // Awake anyway, it is still looked at about hourly (no wake for that).
            let awake = Cadence { awake: true, ..c };
            assert_eq!(due(&awake, back + 60 * MIN), Some(Why::Periodic), "case {i}");
            // With no periodic looks set, a missed push is still looked after while it is awake
            // (that costs no battery), never by waking it.
            let missed = Cadence { every_min: 0, again_at_ms: Some(back + 20 * MIN), ..c };
            assert_eq!(due(&missed, back + 30 * MIN), None, "case {i}");
            assert_eq!(due(&Cadence { awake: true, ..missed }, back + 30 * MIN), Some(Why::Again), "case {i}");
            // One that lists the card again ends it.
            after_look(&mut c, &listing_outcome(&ClipList { clips: listed(1), complete: true, ..Default::default() }), back + 61 * MIN);
            assert_eq!(c.failed_runs, 0);
            c.last_look_ms = Some(back + 61 * MIN);
            assert_eq!(due(&c, back + 61 * MIN + 180 * MIN), Some(Why::Wake), "case {i}");
        }
    }

    #[test]
    fn a_look_cut_short_on_this_side_is_done_again_soon() {
        let t0 = 1_800_000_000_000i64;
        let mut c = Cadence { since_ms: t0, every_min: 60, last_look_ms: Some(t0), ..Default::default() };
        after_look(&mut c, &listing_outcome(&ClipList { clips: listed(60), ran_out: true, ..Default::default() }), t0);
        assert_eq!(c.failed_runs, 0);
        assert_eq!(due(&c, t0 + AGAIN_AFTER_MS), Some(Why::Again));
    }

    #[test]
    fn a_recording_listed_a_second_off_counts_its_tries_under_one_key() {
        let db = crate::db::Db::open_in_memory().unwrap();
        let now = 100 * 3_600_000i64;
        let (a, b) = (now - 600_000, now - 540_000);
        let mut at = now;
        // Each listing reads the camera's clock to the second: the same
        // recording comes back up to a second either way.
        for (i, jitter) in [0i64, 1_000, -1_000, 1_000].iter().enumerate() {
            let gap = Gap { from_ms: a + jitter, to_ms: b + jitter };
            let tried = super::super::card_fill::load_tried_in(&db, SCOPE, "shed");
            if i == 3 {
                assert!(already_handled(&tried, gap, at), "three tries and no more: {tried:?}");
                break;
            }
            assert!(!already_handled(&tried, gap, at), "try {}: {tried:?}", i + 1);
            let before = earlier_try(&tried, gap);
            let (kf, kt) = before.map_or((gap.from_ms, gap.to_ms), |x| (x.from_ms, x.to_ms));
            let n = before.map(|x| if let Tried::Failed(n, _) = x.what { n } else { 0 }).unwrap_or(0) + 1;
            assert_eq!(n, i as u32 + 1);
            super::super::card_fill::save_tried_in(&db, SCOPE, "shed", Attempt { from_ms: kf, to_ms: kt, what: Tried::Failed(n, at) }, at);
            assert_eq!(super::super::card_fill::load_tried_in(&db, SCOPE, "shed").len(), 1, "one key for the recording");
            at += 11 * MIN;
        }
        // Another recording a minute later is not taken for it.
        let tried = super::super::card_fill::load_tried_in(&db, SCOPE, "shed");
        assert_eq!(earlier_try(&tried, Gap { from_ms: a + MIN, to_ms: b + MIN }), None);
    }

    #[test]
    fn a_look_has_a_budget() {
        let mut b = Budget { deadline: std::time::Instant::now() + std::time::Duration::from_secs(60), bytes_left: MAX_BYTES, imports_left: MAX_IMPORTS };
        assert!(!b.spent());
        b.imports_left = 0;
        assert!(b.spent());
        let b = Budget { deadline: std::time::Instant::now() + std::time::Duration::from_secs(60), bytes_left: 0, imports_left: 1 };
        assert!(b.spent());
        let b = Budget { deadline: std::time::Instant::now(), bytes_left: 1, imports_left: 1 };
        assert!(b.spent());
    }
}
