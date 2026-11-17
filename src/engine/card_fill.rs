//! Filling holes in a camera's recordings from the camera's own SD card (see
//! cardfill.rs for the work itself): finds the holes in a camera's index after
//! it comes back, one at a time and gently, remembers which it has tried, and
//! tells the camera's settings and health what it did.

use super::*;
use crate::cardfill::{self, Gap};
use crate::reolink::{CardState, ReolinkClient};
use std::collections::HashMap;
use std::sync::Mutex as StdMutex;

/// Only holes that ended this long ago are filled: the card closes a file
/// at most two minutes after it began, and a file still being written cannot
/// be read.
const READY_AFTER_MS: i64 = 3 * 60_000;
/// A camera is looked at again this often even when nothing changed.
const RESCAN_EVERY: std::time::Duration = std::time::Duration::from_secs(20 * 60);
/// A camera must have been back this long before its card is asked.
const SETTLE_SECS: i64 = 45;
/// A hole whose attempt failed is tried again after this, at most this often.
const RETRY_AFTER_MS: i64 = 10 * 60_000;
const MAX_TRIES: u32 = 3;
/// ui_kv scope holding what was tried (key `{camera}|{from_ms}`).
const SCOPE: &str = "card_fill";

/// What the camera's settings and health say about this.
#[derive(Clone, Debug, Default, serde::Serialize, PartialEq)]
pub struct Status {
    /// "waiting", "watching", "filling", "problem" or "unavailable".
    pub state: String,
    /// One plain sentence.
    pub text: String,
    /// The last hole filled: when (epoch ms), what part of the timeline, seconds.
    pub last_at: Option<i64>,
    pub last_from_ms: Option<i64>,
    pub last_to_ms: Option<i64>,
    pub last_secs: Option<f64>,
    /// Since the server started.
    pub filled_secs: f64,
    pub filled_gaps: u32,
}

static STATUS: StdMutex<Option<HashMap<String, Status>>> = StdMutex::new(None);

fn set_status(name: &str, f: impl FnOnce(&mut Status)) {
    let mut g = STATUS.lock().unwrap_or_else(|e| e.into_inner());
    f(g.get_or_insert_with(Default::default).entry(name.to_string()).or_default());
}

/// What this camera's filling is doing (None: not turned on).
pub fn status(name: &str) -> Option<Status> {
    STATUS.lock().unwrap_or_else(|e| e.into_inner()).as_ref().and_then(|m| m.get(name).cloned())
}

fn clear_status(name: &str) {
    if let Some(m) = STATUS.lock().unwrap_or_else(|e| e.into_inner()).as_mut() {
        m.remove(name);
    }
}

/// How to reach the camera's card.
#[derive(Clone, Debug, PartialEq)]
pub struct Target {
    pub host: String,
    pub user: String,
    pub pass: String,
    pub channel: u32,
    /// The card stream that stands in for what is recorded all the time.
    pub stream: &'static str,
    pub audio: bool,
}

/// Why a camera whose masks go into its recordings gets nothing from its
/// card (the card's video would come in without them).
pub const MASKED_CARD: &str = "Off while the privacy masks are set to \"Hide in recordings too\": the card's video has no masks, so it would add unmasked recordings.";

/// The device channel in a Reolink stream path (`Preview_02_main`: 1).
fn channel_in_path(url: &str) -> Option<u32> {
    let i = url.find("Preview_")? + "Preview_".len();
    let n: String = url[i..].chars().take_while(|c| c.is_ascii_digit()).collect();
    n.parse::<u32>().ok().and_then(|n| n.checked_sub(1))
}

/// Whether this camera could have its holes filled, and how; the reason when
/// not (shown in its settings).
pub fn target(cfg: &CameraConfig) -> Result<Target, String> {
    let mut c = cfg.clone();
    normalize_creds(&mut c);
    if c.battery_uid().is_some() || c.url.starts_with("baichuan://") {
        return Err("Battery cameras get the first seconds of each event from their card already.".into());
    }
    if c.on_demand {
        return Err("This camera sleeps between uses, so its recording has no holes to fill.".into());
    }
    if !(c.url.starts_with("rtsp://") || c.url.starts_with("rtsps://")) {
        return Err("Only for cameras read over RTSP.".into());
    }
    if !c.record {
        return Err("Recording is off for this camera.".into());
    }
    if c.record_motion_only {
        return Err("Needs continuous recording: a motion-only camera is meant to have holes.".into());
    }
    if c.mask_in_recording() {
        return Err(MASKED_CARD.into());
    }
    let Some(host) = crate::onvif::host_from_rtsp_url(&c.url) else { return Err("The camera's address could not be read from its URL.".into()) };
    if c.username.is_empty() {
        return Err("Needs the camera's user name and password.".into());
    }
    Ok(Target {
        host,
        user: c.username.clone(),
        pass: c.password.clone(),
        channel: c.reolink_channel.or_else(|| channel_in_path(&c.url)).unwrap_or(0),
        stream: if c.record_sub && c.sub_url.as_deref().is_some_and(|u| !u.is_empty()) { "sub" } else { "main" },
        audio: c.audio,
    })
}

/// A signed-in camera; signed out again when dropped (cameras allow only a
/// few sessions, and one left open lasts about an hour).
pub(crate) struct Session(ReolinkClient);

impl std::ops::Deref for Session {
    type Target = ReolinkClient;
    fn deref(&self) -> &ReolinkClient {
        &self.0
    }
}

impl std::ops::DerefMut for Session {
    fn deref_mut(&mut self) -> &mut ReolinkClient {
        &mut self.0
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        self.0.logout();
    }
}

/// Sign in to the camera's web API over HTTPS; plain HTTP only when nothing
/// answers on the HTTPS port (not after a refused sign-in or a certificate
/// problem, so the password is not sent unencrypted for those).
fn connect(t: &Target) -> anyhow::Result<Session> {
    connect_to(&[format!("https://{}", t.host), format!("http://{}", t.host)], &t.user, &t.pass)
}

fn connect_to(bases: &[String], user: &str, pass: &str) -> anyhow::Result<Session> {
    let mut last = anyhow::anyhow!("no address to try");
    for base in bases {
        // (Signed out on drop, also when the sign-in half worked.)
        let mut c = Session(ReolinkClient::with_base(base, user, pass));
        match c.camera_clock() {
            Ok(_) => return Ok(c),
            Err(e) => {
                let go_on = crate::reolink::is_unreachable(&e);
                last = e;
                if !go_on {
                    break;
                }
            }
        }
    }
    Err(last)
}

/// What the camera's settings show: can this camera do it, and is a card in.
/// Asked of the camera (a sign-in and two quick questions), so answers are
/// kept for five minutes.
pub fn check(cfg: &CameraConfig) -> serde_json::Value {
    static CACHE: StdMutex<Option<HashMap<String, (std::time::Instant, serde_json::Value)>>> = StdMutex::new(None);
    let t = match target(cfg) {
        Ok(t) => t,
        Err(why) => return serde_json::json!({ "offered": false, "why": why }),
    };
    let key = format!("{}|{}|{}", t.host, t.channel, t.stream);
    if let Some((at, v)) = CACHE.lock().unwrap_or_else(|e| e.into_inner()).as_ref().and_then(|m| m.get(&key)) {
        if at.elapsed() < std::time::Duration::from_secs(300) {
            return v.clone();
        }
    }
    let v = match connect(&t) {
        Err(_) => serde_json::json!({ "offered": false, "why": "The camera's web interface did not answer to this sign-in: it may not be a Reolink camera." }),
        Ok(mut c) => match c.card_state() {
            Ok(CardState::Present(mb)) => serde_json::json!({ "offered": true, "card_mb": mb, "stream": t.stream }),
            Ok(CardState::None) => serde_json::json!({ "offered": false, "why": "No memory card in this camera." }),
            Err(_) => serde_json::json!({ "offered": false, "why": "The camera did not say whether it has a card." }),
        },
    };
    c_store(&CACHE, key, v.clone());
    v
}

fn c_store(cache: &StdMutex<Option<HashMap<String, (std::time::Instant, serde_json::Value)>>>, key: String, v: serde_json::Value) {
    cache.lock().unwrap_or_else(|e| e.into_inner()).get_or_insert_with(Default::default).insert(key, (std::time::Instant::now(), v));
}

// ---- what was tried ----

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Tried {
    /// Filled (all or what the card had).
    Done,
    /// The card had nothing for it.
    Empty,
    /// Failed: tries so far and when the last one was (ms).
    Failed(u32, i64),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Attempt {
    pub from_ms: i64,
    pub to_ms: i64,
    pub what: Tried,
}

pub fn encode(a: &Attempt) -> String {
    match a.what {
        Tried::Done => format!("{}|done|0|0", a.to_ms),
        Tried::Empty => format!("{}|empty|0|0", a.to_ms),
        Tried::Failed(n, at) => format!("{}|failed|{n}|{at}", a.to_ms),
    }
}

pub fn decode(from_ms: i64, v: &str) -> Option<Attempt> {
    let mut p = v.split('|');
    let to_ms = p.next()?.parse().ok()?;
    let what = match p.next()? {
        "done" => Tried::Done,
        "empty" => Tried::Empty,
        "failed" => Tried::Failed(p.next()?.parse().ok()?, p.next()?.parse().ok()?),
        _ => return None,
    };
    Some(Attempt { from_ms, to_ms, what })
}

/// Whether a hole needs no attempt now: one that covers it was filled or came
/// back empty, or failed too often, or failed a short while ago.
pub fn already_handled(tried: &[Attempt], gap: Gap, now_ms: i64) -> bool {
    tried.iter().any(|a| {
        a.from_ms <= gap.from_ms + 2_000
            && a.to_ms >= gap.to_ms - 2_000
            && match a.what {
                Tried::Done | Tried::Empty => true,
                Tried::Failed(n, at) => n >= MAX_TRIES || now_ms - at < RETRY_AFTER_MS,
            }
    })
}

/// Whether a hole failed lately and is to be tried again once RETRY_AFTER_MS has passed.
fn to_retry(tried: &[Attempt], gap: Gap) -> bool {
    tried.iter().any(|a| a.from_ms <= gap.from_ms + 2_000 && a.to_ms >= gap.to_ms - 2_000 && matches!(a.what, Tried::Failed(n, _) if n < MAX_TRIES))
        && !tried.iter().any(|a| a.from_ms <= gap.from_ms + 2_000 && a.to_ms >= gap.to_ms - 2_000 && matches!(a.what, Tried::Done | Tried::Empty | Tried::Failed(MAX_TRIES.., _)))
}

fn load_tried(db: &crate::db::Db, slug_name: &str) -> Vec<Attempt> {
    load_tried_in(db, SCOPE, slug_name)
}

/// What was tried for a camera, kept in ui_kv `scope` (also used for battery
/// cameras' catch-up, card_catchup.rs).
pub(super) fn load_tried_in(db: &crate::db::Db, scope: &str, slug_name: &str) -> Vec<Attempt> {
    let prefix = format!("{slug_name}|");
    db.ui_scope_prefix(scope, &prefix)
        .unwrap_or_default()
        .into_iter()
        .filter_map(|(k, v)| k.strip_prefix(&prefix).and_then(|f| f.parse::<i64>().ok()).and_then(|f| decode(f, &v)))
        .collect()
}

/// Attempts are kept as long as their hole can be looked at, and an hour more.
const KEEP_TRIED_MS: i64 = cardfill::LOOK_BACK_MS + 3_600_000;

fn save_tried(db: &crate::db::Db, slug_name: &str, a: Attempt, now_ms: i64) {
    save_tried_in(db, SCOPE, slug_name, a, now_ms)
}

pub(super) fn save_tried_in(db: &crate::db::Db, scope: &str, slug_name: &str, a: Attempt, now_ms: i64) {
    let _ = db.ui_put(scope, &format!("{slug_name}|{}", a.from_ms), &encode(&a));
    // Older attempts (any camera's) are of holes no longer looked at.
    for (k, _) in db.ui_scope(scope).unwrap_or_default() {
        if k.rsplit('|').next().and_then(|f| f.parse::<i64>().ok()).is_none_or(|f| f < now_ms - KEEP_TRIED_MS) {
            let _ = db.ui_delete(scope, &k);
        }
    }
}

/// Put back what was known of a hole before an attempt that did not count.
fn restore_tried(db: &crate::db::Db, slug_name: &str, from_ms: i64, before: Option<Attempt>) {
    restore_tried_in(db, SCOPE, slug_name, from_ms, before)
}

pub(super) fn restore_tried_in(db: &crate::db::Db, scope: &str, slug_name: &str, from_ms: i64, before: Option<Attempt>) {
    match before {
        Some(a) => {
            let _ = db.ui_put(scope, &format!("{slug_name}|{from_ms}"), &encode(&a));
        }
        None => {
            let _ = db.ui_delete(scope, &format!("{slug_name}|{from_ms}"));
        }
    }
}

/// A camera was removed: what was tried for it, its recording mode and its
/// status go.
pub fn forget_camera(db: &crate::db::Db, name: &str) {
    let s = slug(name);
    let _ = db.ui_delete_prefix(SCOPE, &format!("{s}|"));
    let _ = db.ui_delete(MODE_SCOPE, &s);
    clear_status(name);
}

/// A camera was renamed: its status follows (the stored rows move with the
/// database's rename).
pub fn rename_camera(old: &str, new: &str) {
    let mut g = STATUS.lock().unwrap_or_else(|e| e.into_inner());
    if let Some(m) = g.as_mut() {
        if let Some(s) = m.remove(old) {
            m.insert(new.to_string(), s);
        }
    }
}

// ---- whether a hole was meant to be there ----

/// ui_kv scope holding each camera's recording mode as last seen (key: its
/// slug): `on|{since ms}` (recording continuously since then, as far as this
/// server saw) or `off|{ms}`.
const MODE_SCOPE: &str = "card_fill_mode";

fn read_mode(db: &crate::db::Db, slug_name: &str) -> Option<(bool, i64)> {
    read_mode_in(db, MODE_SCOPE, slug_name)
}

/// A recording mode noted in ui_kv `scope` (see note_mode_in).
pub(super) fn read_mode_in(db: &crate::db::Db, scope: &str, slug_name: &str) -> Option<(bool, i64)> {
    let v = db.ui_get(scope, slug_name).ok().flatten()?;
    let (state, at) = v.split_once('|')?;
    Some((state == "on", at.parse().ok()?))
}

/// Note whether a camera records continuously now (only a change is written).
/// A camera that starts recording continuously gets its holes filled only
/// from then on: the holes before were made on purpose (recording off, or
/// motion only), and filling them would add what was meant not to be kept.
pub fn note_mode(db: &crate::db::Db, name: &str, continuous: bool, now_ms: i64) {
    note_mode_in(db, MODE_SCOPE, name, continuous, now_ms)
}

/// Note in ui_kv `scope` whether a camera is `on` now, and since when it has
/// been (only a change is written).
pub(super) fn note_mode_in(db: &crate::db::Db, scope: &str, name: &str, on: bool, now_ms: i64) {
    let s = slug(name);
    let was = read_mode_in(db, scope, &s);
    let v = match (was, on) {
        (Some((true, _)), true) | (Some((false, _)), false) => return,
        (_, true) => format!("on|{now_ms}"),
        (_, false) => format!("off|{now_ms}"),
    };
    let _ = db.ui_put(scope, &s, &v);
}

/// Whether the camera was recording continuously all through the hole, as
/// far as can be known: seen so since before it began, and the weekly
/// schedule's profile for each of its minutes keeps it so.
fn meant_continuous(engine: &Engine, base: &CameraConfig, gap: Gap) -> bool {
    if !mode_allows(read_mode(&engine.db, &slug(&base.name)), gap) {
        return false;
    }
    let auto = engine.config.lock().unwrap().automation.clone();
    // Only the hold in effect now is known: minutes of a hole that fell in an
    // earlier hold (one since ended or replaced) are judged by the schedule.
    let hold = super::profiles::manual_hold(engine).map(|(name, since, _)| (name, since));
    schedule_keeps_continuous(&auto, base, gap, hold.as_ref(), |ms| {
        use chrono::{Datelike, TimeZone, Timelike};
        let t = chrono::Local.timestamp_millis_opt(ms).single()?;
        Some((t.weekday().num_days_from_monday() as usize, t.hour() * 60 + t.minute()))
    })
}

/// The noted recording mode allows filling `gap`: continuous since before it.
pub(super) fn mode_allows(mode: Option<(bool, i64)>, gap: Gap) -> bool {
    matches!(mode, Some((true, since)) if since <= gap.from_ms)
}

/// For each minute of the hole (`local` gives its weekday and minute of the
/// day), the profile in effect keeps `base` recording continuously: the one
/// held by hand now (`hold`: its name and since when, epoch ms) for the
/// minutes since it began, otherwise the one the schedule picks.
fn schedule_keeps_continuous(
    auto: &crate::config::AutomationConfig,
    base: &CameraConfig,
    gap: Gap,
    hold: Option<&(String, i64)>,
    local: impl Fn(i64) -> Option<(usize, u32)>,
) -> bool {
    schedule_keeps(auto, base, gap, hold, local, |o| o.record.unwrap_or(base.record) && !o.record_motion_only.unwrap_or(base.record_motion_only))
}

/// For each minute of `gap`, `keeps` holds for the camera's override under
/// the profile in effect then (held by hand since `hold`, otherwise the
/// weekly schedule's).
pub(super) fn schedule_keeps(
    auto: &crate::config::AutomationConfig,
    base: &CameraConfig,
    gap: Gap,
    hold: Option<&(String, i64)>,
    local: impl Fn(i64) -> Option<(usize, u32)>,
    keeps: impl Fn(&crate::config::CameraOverride) -> bool,
) -> bool {
    let sun = super::profiles::sun_today(auto);
    let mut t = gap.from_ms;
    loop {
        let Some((weekday, minute)) = local(t) else { return false };
        let name = match hold {
            Some((held, since)) if *since <= t => held.clone(),
            _ => super::profiles::scheduled_profile(auto, weekday, minute, sun).0,
        };
        let o = auto.profiles.iter().find(|p| p.name == name).and_then(|p| p.cameras.get(&base.name).cloned()).unwrap_or_default();
        if !keeps(&o) {
            return false;
        }
        if t >= gap.to_ms {
            return true;
        }
        t = (t + 60_000).min(gap.to_ms);
    }
}

// ---- the loop ----

/// One download at a time on this server.
static BUSY: StdMutex<()> = StdMutex::new(());
/// The server is stopping: no new work, and work under way ends.
static STOPPING: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// At shutdown: end the filling under way and wait (up to `wait`) until it
/// has ended, so nothing it found is handed to the index after the server
/// noted its recordings as complete. True when it ended in time; when not,
/// it ends later without adding anything (what it wrote is deleted, or at
/// the next start).
pub fn stop_and_wait(wait: std::time::Duration) -> bool {
    begin_stop();
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

/// The server is about to stop its cameras: from now on nothing new is
/// started, and their switching off is not taken for a change of recording.
pub fn begin_stop() {
    STOPPING.store(true, Ordering::SeqCst);
}

pub(super) fn stopping() -> bool {
    STOPPING.load(Ordering::SeqCst)
}

struct Seen {
    online_since: Option<i64>,
    at: std::time::Instant,
    /// Holes were left this pass (more to do, or a try to repeat soon).
    more: bool,
}

/// A camera's recordings folder.
fn segments_dir(engine: &Engine, cfg: &CameraConfig) -> std::path::PathBuf {
    let (_, folder_root) = engine.record_folder(cfg);
    folder_root.join("cameras").join(slug(&cfg.name)).join("segments")
}

pub fn card_fill_loop(engine: Arc<Engine>) {
    // What a fill stopped by a crash left: never a recording.
    let cfgs: Vec<CameraConfig> = engine.cameras.read().unwrap().iter().flatten().map(|h| h.cfg.read().unwrap().clone()).collect();
    for cfg in &cfgs {
        cardfill::clear_staging(&segments_dir(&engine, cfg));
    }
    // Not in the first minute after a start: the cameras are connecting.
    thread::sleep(std::time::Duration::from_secs(60));
    let mut seen: HashMap<usize, Seen> = HashMap::new();
    while !stopping() {
        let handles: Vec<Arc<CameraHandle>> = engine.cameras.read().unwrap().iter().flatten().cloned().collect();
        let mut worked = false;
        for h in handles {
            if stopping() {
                return;
            }
            worked |= watch(&engine, &h, &mut seen);
        }
        seen.retain(|id, _| engine.camera(*id).is_some());
        thread::sleep(std::time::Duration::from_secs(if worked { 5 } else { 30 }));
    }
}

/// One camera's turn: true when a hole was worked on.
fn watch(engine: &Arc<Engine>, h: &Arc<CameraHandle>, seen: &mut HashMap<usize, Seen>) -> bool {
    let cfg = h.cfg.read().unwrap().clone();
    // What the camera records under the active profile (not only its own
    // settings), noted for every camera so the history is there when
    // filling is turned on.
    // (Not for a camera being restarted, whose old handle is switched off,
    // nor while the server stops and switches every camera off.)
    let current = || engine.camera(h.id).is_some_and(|c| Arc::ptr_eq(&c, h));
    if !current() {
        return false;
    }
    let eff = engine.effective_camera(&cfg);
    let continuous = {
        let r = h.running.read().unwrap();
        h.active.load(Ordering::SeqCst) && eff.record && !eff.record_motion_only && r.record && !r.record_motion_only
    };
    if stopping() || !current() {
        return false;
    }
    note_mode(&engine.db, &cfg.name, continuous, Utc::now().timestamp_millis());
    if !cfg.fill_from_card {
        clear_status(&cfg.name);
        seen.remove(&h.id);
        return false;
    }
    let name = cfg.name.clone();
    if !h.active.load(Ordering::Relaxed) {
        return false;
    }
    let target = match target(&eff) {
        Ok(t) => t,
        Err(why) => {
            set_status(&name, |s| {
                s.state = "unavailable".into();
                s.text = why;
            });
            return false;
        }
    };
    // Recordings already on disk are not in the index yet (the start-up
    // walk after a crash): they would look like holes.
    if engine.db.importing() {
        set_status(&name, |s| {
            s.state = "waiting".into();
            s.text = "Waiting for the recordings on disk to be indexed before looking for holes.".into();
        });
        return false;
    }
    let (online, since) = {
        let st = h.state.read().unwrap();
        (st.online, st.online_since)
    };
    let now = Utc::now().timestamp();
    if !online || since.is_none_or(|s| now - s < SETTLE_SECS) {
        set_status(&name, |s| {
            if s.state != "filling" {
                s.state = "waiting".into();
                s.text = "Waiting for the camera to be back before looking for holes.".into();
            }
        });
        return false;
    }
    let due = match seen.get(&h.id) {
        None => true,
        Some(s) => s.online_since != since || s.more || s.at.elapsed() >= RESCAN_EVERY,
    };
    if !due {
        return false;
    }
    let slug_name = slug(&name);
    let now_ms = Utc::now().timestamp_millis();
    let rows = engine.db.segments_overlapping(&slug_name, now_ms - cardfill::LOOK_BACK_MS, now_ms).unwrap_or_default();
    let spans: Vec<(i64, i64)> = rows.iter().map(|r| (r.start_ms, r.end_ms)).collect();
    let tried = load_tried(&engine.db, &slug_name);
    // Holes made on purpose (recording off or motion only at the time) are left alone.
    let all: Vec<Gap> = cardfill::find_gaps(&spans, now_ms - cardfill::LOOK_BACK_MS, cardfill::MIN_GAP_MS, cardfill::MAX_GAP_MS)
        .into_iter()
        .filter(|g| meant_continuous(engine, &cfg, *g))
        .collect();
    let gaps: Vec<Gap> = all.iter().copied().filter(|g| g.to_ms <= now_ms - READY_AFTER_MS && !already_handled(&tried, *g, now_ms)).collect();
    let mut entry = Seen { online_since: since, at: std::time::Instant::now(), more: false };
    let Some(gap) = gaps.first().copied() else {
        // Holes too fresh to read yet, or failed recently: look again soon.
        entry.more = all.iter().any(|g| !already_handled(&tried, *g, now_ms) || to_retry(&tried, *g));
        set_status(&name, |s| {
            if s.state != "filling" && s.state != "problem" {
                s.state = "watching".into();
                s.text = match (s.last_secs, s.last_from_ms, s.last_to_ms) {
                    (Some(secs), Some(a), Some(b)) => format!("Watching for holes. Last filled {} from {} to {}.", length_text(secs), clock_text(a), clock_text(b)),
                    _ => "Watching for holes in the recordings; none to fill.".into(),
                };
            }
        });
        seen.insert(h.id, entry);
        return false;
    };
    // Others waiting after this one: come back soon.
    entry.more = gaps.len() > 1;
    let _busy = match BUSY.try_lock() {
        Ok(g) => g,
        Err(std::sync::TryLockError::Poisoned(p)) => p.into_inner(),
        Err(std::sync::TryLockError::WouldBlock) => {
            entry.more = true;
            seen.insert(h.id, entry);
            return false;
        }
    };
    if stopping() {
        return false;
    }
    // A panic in the work (a damaged recording the reader trips on) fails
    // this hole only; its attempt was written down before, so it is not
    // tried again and again after restarts either.
    let done = match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| fill_one(engine, h, &eff, &target, gap, &tried))) {
        Ok(d) => d,
        Err(_) => {
            cardfill::clear_staging(&segments_dir(engine, &eff));
            let span = format!("{} to {}", clock_text(gap.from_ms), clock_text(gap.to_ms));
            warn!("camera '{name}': filling the hole from {span} failed (an internal error); not trying it again");
            save_tried(&engine.db, &slug_name, Attempt { from_ms: gap.from_ms, to_ms: gap.to_ms, what: Tried::Failed(MAX_TRIES, Utc::now().timestamp_millis()) }, Utc::now().timestamp_millis());
            set_status(&name, |s| {
                s.state = "problem".into();
                s.text = format!("Could not fill the hole from {span}: the camera's recording could not be read. Not trying again.");
            });
            false
        }
    };
    entry.more |= !done;
    seen.insert(h.id, entry);
    true
}

fn clock_text(ms: i64) -> String {
    chrono::DateTime::from_timestamp_millis(ms).map(|t| t.with_timezone(&chrono::Local).format("%H:%M:%S").to_string()).unwrap_or_default()
}

fn length_text(secs: f64) -> String {
    let s = secs.round() as i64;
    if s >= 120 {
        format!("{} min {} s", s / 60, s % 60)
    } else {
        format!("{s} s")
    }
}

/// Fill one hole; false when it should be tried again later.
fn fill_one(engine: &Arc<Engine>, h: &Arc<CameraHandle>, cfg: &CameraConfig, target: &Target, gap: Gap, tried: &[Attempt]) -> bool {
    let name = cfg.name.clone();
    let slug_name = slug(&name);
    let span = format!("{} to {}", clock_text(gap.from_ms), clock_text(gap.to_ms));
    let started_ms = Utc::now().timestamp_millis();
    // The card's clock is read now: a hole from before a daylight-saving
    // change (on this computer, which the camera's zone usually follows)
    // would be placed an hour off, so it is left.
    if !cardfill::same_offset(cardfill::local_offset, &[gap.from_ms, gap.to_ms, started_ms]) {
        info!("camera '{name}': not filling the hole from {span}: the clocks changed for daylight saving time since");
        save_tried(&engine.db, &slug_name, Attempt { from_ms: gap.from_ms, to_ms: gap.to_ms, what: Tried::Empty }, started_ms);
        set_status(&name, |s| {
            s.state = "watching".into();
            s.text = format!("Left the hole from {span}: the clocks changed for daylight saving time since.");
        });
        return true;
    }
    set_status(&name, |s| {
        s.state = "filling".into();
        s.text = format!("Filling the hole from {span} from the camera's card.");
    });
    let dir = segments_dir(engine, cfg);
    let segment_seconds = engine.config.lock().unwrap().storage.segment_seconds;
    let before = tried.iter().find(|a| a.from_ms == gap.from_ms).copied();
    let previous_tries = before.map(|a| if let Tried::Failed(n, _) = a.what { n } else { 0 }).unwrap_or(0);
    // Written down before the camera is asked: work that crashes the server
    // counts as a try, so it is not repeated after every restart.
    save_tried(&engine.db, &slug_name, Attempt { from_ms: gap.from_ms, to_ms: gap.to_ms, what: Tried::Failed(previous_tries + 1, started_ms) }, started_ms);
    let stop = || stopping() || !h.active.load(Ordering::Relaxed) || engine.camera(h.id).is_none_or(|c| !Arc::ptr_eq(&c, h));
    let result = connect(target).and_then(|mut session| {
        let job = cardfill::Job {
            gap,
            channel: target.channel,
            stream: target.stream,
            audio: target.audio,
            dir: &dir,
            stem: &slug_name,
            segment_seconds,
            time_limit: cardfill::MAX_GAP_TIME,
        };
        cardfill::fill(&mut session, &job, &stop)
    });
    let now_ms = Utc::now().timestamp_millis();
    match result {
        Ok(cardfill::Outcome::Filled(f)) => {
            // (A rename of the camera waits for what is kept now; one that
            // came first stopped the camera: `stop`.)
            let _writing = engine.writing_into(&name);
            if stop() {
                // Stopped (the server, or the camera turned off) after the work:
                // nothing is added now; the hole is looked at again later.
                cardfill::clear_staging(&dir);
                restore_tried(&engine.db, &slug_name, gap.from_ms, before);
                return true;
            }
            let secs = f.seconds;
            let (mut kept, mut failed) = (0usize, 0usize);
            for seg in f.segments {
                // Never on top of what is recorded: if the live recording
                // reached into the hole meanwhile, this piece is dropped.
                let clash = engine.db.segments_overlapping(&slug_name, seg.start_ms + 1_000, seg.end_ms - 1_000).map(|r| !r.is_empty()).unwrap_or(true);
                if clash {
                    let _ = std::fs::remove_file(&seg.path);
                    continue;
                }
                match cardfill::commit_piece(seg, &dir) {
                    Ok(seg) => {
                        kept += 1;
                        engine.segment_closed(&slug_name, seg);
                    }
                    Err(e) => {
                        failed += 1;
                        warn!("camera '{name}': could not keep a recording from the card: {e}");
                    }
                }
            }
            cardfill::clear_staging(&dir);
            match pieces_outcome(kept, failed) {
                Pieces::Kept => {}
                Pieces::AllClashed => {
                    save_tried(&engine.db, &slug_name, Attempt { from_ms: gap.from_ms, to_ms: gap.to_ms, what: Tried::Done }, now_ms);
                    set_status(&name, |s| {
                        s.state = "watching".into();
                        s.text = "A hole was already filled by the live recording.".into();
                    });
                    return true;
                }
                Pieces::CouldNotKeep => {
                    // The failed try written down before stands: tried again later.
                    let n = previous_tries + 1;
                    set_status(&name, |s| {
                        s.state = "problem".into();
                        s.text = format!(
                            "Could not fill the hole from {span}: the recordings from the card could not be saved in the recording folder.{}",
                            if n >= MAX_TRIES { " Not trying again." } else { " Trying again later." }
                        );
                    });
                    return false;
                }
            }
            save_tried(&engine.db, &slug_name, Attempt { from_ms: gap.from_ms, to_ms: gap.to_ms, what: Tried::Done }, now_ms);
            info!(
                "camera '{name}': filled {} of the hole from {span} with the camera's own recording{}",
                length_text(secs),
                if f.partial { " (the card did not cover all of it)" } else { "" }
            );
            set_status(&name, |s| {
                s.state = "watching".into();
                s.last_at = Some(now_ms);
                s.last_from_ms = Some(gap.from_ms);
                s.last_to_ms = Some(gap.to_ms);
                s.last_secs = Some(secs);
                s.filled_secs += secs;
                s.filled_gaps += 1;
                s.text = format!("Filled {} from {span} from the camera's card.", length_text(secs));
            });
            true
        }
        Ok(cardfill::Outcome::NothingOnCard(why)) => {
            info!("camera '{name}': nothing to fill the hole from {span} with: {why}");
            save_tried(&engine.db, &slug_name, Attempt { from_ms: gap.from_ms, to_ms: gap.to_ms, what: Tried::Empty }, now_ms);
            set_status(&name, |s| {
                s.state = "watching".into();
                s.text = format!("The camera's card has nothing for the hole from {span}.");
            });
            true
        }
        Err(e) => {
            if stop() {
                // Switched off meanwhile: not a failure.
                restore_tried(&engine.db, &slug_name, gap.from_ms, before);
                return true;
            }
            let n = previous_tries + 1;
            let why = format!("{e:#}");
            warn!("camera '{name}': could not fill the hole from {span} ({why}); try {n} of {MAX_TRIES}");
            set_status(&name, |s| {
                s.state = "problem".into();
                s.text = format!("Could not fill the hole from {span}: {why}.{}", if n >= MAX_TRIES { " Not trying again." } else { " Trying again later." });
            });
            false
        }
    }
}

/// What came of keeping the pieces read from the card.
#[derive(Debug, PartialEq)]
enum Pieces {
    /// At least one is in the recordings now: the hole is done.
    Kept,
    /// Every piece overlapped the live recording: done, nothing to add.
    AllClashed,
    /// None kept, and some could not be saved (a rename refused): a failed try.
    CouldNotKeep,
}

fn pieces_outcome(kept: usize, failed: usize) -> Pieces {
    match (kept, failed) {
        (0, 0) => Pieces::AllClashed,
        (0, _) => Pieces::CouldNotKeep,
        _ => Pieces::Kept,
    }
}

/// Whether a recording file is a piece taken from the camera's card (filled
/// holes `{camera}-card-{ms}-seg…`; a battery camera's caught-up events
/// `{camera}-card-{ms}-rec-seg…`).
pub fn is_card_piece(path: &str) -> bool {
    let name = path.rsplit(['/', '\\']).next().unwrap_or(path);
    match name.find("-card-") {
        Some(i) => {
            let rest = &name[i + 6..];
            let digits = rest.bytes().take_while(|b| b.is_ascii_digit()).count();
            digits >= 10 && (rest[digits..].starts_with("-seg") || rest[digits..].starts_with("-rec-seg"))
        }
        None => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn what_was_tried_is_stored_and_read_back() {
        for what in [Tried::Done, Tried::Empty, Tried::Failed(2, 1_700_000_000_000)] {
            let a = Attempt { from_ms: 5, to_ms: 99, what };
            assert_eq!(decode(5, &encode(&a)), Some(a));
        }
        assert_eq!(decode(1, "junk"), None);
        assert_eq!(decode(1, "5|failed|x|0"), None);
    }

    #[test]
    fn a_hole_is_not_tried_forever() {
        let gap = Gap { from_ms: 1_000_000, to_ms: 1_100_000 };
        let now = 10_000_000;
        let a = |what| vec![Attempt { from_ms: 1_000_000, to_ms: 1_100_000, what }];
        assert!(!already_handled(&[], gap, now));
        assert!(already_handled(&a(Tried::Done), gap, now));
        assert!(already_handled(&a(Tried::Empty), gap, now));
        // A failure waits, then is tried again, up to MAX_TRIES times.
        assert!(already_handled(&a(Tried::Failed(1, now - 60_000)), gap, now));
        assert!(!already_handled(&a(Tried::Failed(1, now - RETRY_AFTER_MS - 1)), gap, now));
        assert!(already_handled(&a(Tried::Failed(MAX_TRIES, now - 10 * RETRY_AFTER_MS)), gap, now));
        // A failure waiting for its retry keeps the camera looked at soon, not
        // only at the next full rescan; one given up on, or done, does not.
        assert!(to_retry(&a(Tried::Failed(1, now - 60_000)), gap));
        assert!(!to_retry(&a(Tried::Failed(MAX_TRIES, now - 60_000)), gap));
        assert!(!to_retry(&a(Tried::Done), gap));
        assert!(!to_retry(&[], gap));
        // What is left of a filled hole (a stretch inside it) is not asked for again.
        let left = Gap { from_ms: 1_030_000, to_ms: 1_100_000 };
        assert!(already_handled(&a(Tried::Done), left, now));
        // A hole reaching beyond what was tried is new.
        let wider = Gap { from_ms: 1_000_000, to_ms: 1_200_000 };
        assert!(!already_handled(&a(Tried::Done), wider, now));
        // Another hole is untouched by it.
        assert!(!already_handled(&a(Tried::Done), Gap { from_ms: 5_000_000, to_ms: 5_100_000 }, now));
    }

    #[test]
    fn recordings_from_the_card_are_recognized_by_name() {
        assert!(is_card_piece("cameras/porch/segments/porch-card-1794148581609-seg000000.mp4"));
        assert!(is_card_piece("cameras/porch/segments/porch-sub-card-1794148581609-seg000003.mp4"));
        assert!(!is_card_piece("cameras/porch/segments/porch-1794148581609-seg000000.mp4"));
        assert!(!is_card_piece("cameras/porch/segments/porch-1794148581609-rec-seg000000.mp4"));
        // A battery camera's event caught up from its card.
        assert!(is_card_piece("cameras/porch/segments/porch-card-1794148581609-rec-seg000000.mp4"));

        // A camera whose own name has "card" in it is not mistaken.
        assert!(!is_card_piece("cameras/card-reader/segments/card-reader-1794148581609-seg000000.mp4"));
        assert!(is_card_piece("cameras/card-reader/segments/card-reader-card-1794148581609-seg000000.mp4"));
    }

    #[test]
    fn the_channel_comes_from_the_stream_path() {
        assert_eq!(channel_in_path("rtsp://192.0.2.5:554/Preview_02_main"), Some(1));
        assert_eq!(channel_in_path("rtsp://192.0.2.5:554/h264Preview_01_sub"), Some(0));
        assert_eq!(channel_in_path("rtsp://192.0.2.5:554/stream1"), None);
        assert!(is_card_piece("D:\\rec\\cameras\\porch\\segments\\porch-card-1794148581609-seg000000.mp4"));
    }

    fn cam(name: &str) -> CameraConfig {
        toml::from_str(&format!("name = \"{name}\"\nurl = \"rtsp://192.0.2.5:554/Preview_01_main\"\nusername = \"u\"\n")).unwrap()
    }

    #[test]
    fn masks_in_the_recordings_turn_card_filling_off() {
        let mut c = cam("Porch");
        assert!(target(&c).is_ok());
        c.privacy_masks = vec![crate::config::ZoneRect { x: 0.1, y: 0.1, w: 0.2, h: 0.2 }];
        c.mask_mode = Some("screen".into());
        assert!(target(&c).is_ok(), "screen masks: the recordings are the camera's own stream anyway");
        c.mask_mode = Some("recording".into());
        assert_eq!(target(&c), Err(MASKED_CARD.to_string()));
        c.mask_mode = None;
        assert_eq!(target(&c), Err(MASKED_CARD.to_string()), "masks from before the level count as in the recordings");
    }

    #[test]
    fn holes_from_before_continuous_recording_began_are_left() {
        let db = crate::db::Db::open_in_memory().unwrap();
        let gap = |a: i64, b: i64| Gap { from_ms: a, to_ms: b };
        // Never seen: not filled.
        assert!(!mode_allows(read_mode(&db, "porch"), gap(10_000, 20_000)));
        // Motion only (a profile) until 50 s, continuous after.
        note_mode(&db, "Porch", false, 1_000);
        assert!(!mode_allows(read_mode(&db, "porch"), gap(10_000, 20_000)));
        note_mode(&db, "Porch", true, 50_000);
        note_mode(&db, "Porch", true, 90_000); // no change: the time it began stays
        assert_eq!(read_mode(&db, "porch"), Some((true, 50_000)));
        assert!(!mode_allows(read_mode(&db, "porch"), gap(10_000, 20_000)), "made on purpose");
        assert!(!mode_allows(read_mode(&db, "porch"), gap(40_000, 70_000)), "began before");
        assert!(mode_allows(read_mode(&db, "porch"), gap(60_000, 80_000)));
        // Off again: nothing is filled until it is back.
        note_mode(&db, "Porch", false, 100_000);
        assert!(!mode_allows(read_mode(&db, "porch"), gap(60_000, 80_000)));
    }

    #[test]
    fn the_schedule_decides_what_a_hole_was_meant_to_be() {
        use crate::config::{AutomationConfig, CameraOverride, ProfileConfig, ScheduleRule};
        let mut night = ProfileConfig { name: "Night".into(), ..Default::default() };
        night.cameras.insert("Porch".into(), CameraOverride { record_motion_only: Some(true), ..Default::default() });
        let mut home = ProfileConfig { name: "Home".into(), ..Default::default() };
        home.cameras.insert("Porch".into(), CameraOverride { record: Some(false), ..Default::default() });
        let auto = AutomationConfig {
            profiles: vec![night, home],
            schedule: vec![
                ScheduleRule { days: Vec::new(), from: "22:00".into(), to: "06:00".into(), profile: "Night".into() },
                ScheduleRule { days: Vec::new(), from: "12:00".into(), to: "13:00".into(), profile: "Home".into() },
            ],
            ..Default::default()
        };
        // Times here are minutes of a Monday, as ms.
        let local = |ms: i64| Some((0usize, (ms / 60_000) as u32));
        let at = |h: i64, m: i64| (h * 60 + m) * 60_000;
        let gap = |a: i64, b: i64| Gap { from_ms: a, to_ms: b };
        let porch = cam("Porch");
        assert!(schedule_keeps_continuous(&auto, &porch, gap(at(8, 0), at(9, 30)), None, local));
        assert!(!schedule_keeps_continuous(&auto, &porch, gap(at(5, 30), at(6, 30)), None, local), "motion only until 06:00");
        assert!(!schedule_keeps_continuous(&auto, &porch, gap(at(11, 50), at(12, 1)), None, local), "recording off from 12:00");
        assert!(schedule_keeps_continuous(&auto, &porch, gap(at(13, 0), at(14, 0)), None, local));
        // Another camera is not touched by Porch's overrides.
        assert!(schedule_keeps_continuous(&auto, &cam("Yard"), gap(at(5, 30), at(6, 30)), None, local));
        // A camera of its own set to motion only: never.
        let mut mo = cam("Yard");
        mo.record_motion_only = true;
        assert!(!schedule_keeps_continuous(&auto, &mo, gap(at(8, 0), at(9, 0)), None, local));
        // Night held by hand since 07:00: a morning hole was meant (motion only).
        let held = ("Night".to_string(), at(7, 0));
        assert!(!schedule_keeps_continuous(&auto, &porch, gap(at(8, 0), at(9, 30)), Some(&held), local), "the held profile, not the schedule");
        // Minutes before the hold began go by the schedule.
        assert!(schedule_keeps_continuous(&auto, &porch, gap(at(6, 10), at(6, 50)), Some(&held), local));
        // Held "no profile" over the 12:00 Home rule: recording stays on.
        let none = (String::new(), at(11, 0));
        assert!(schedule_keeps_continuous(&auto, &porch, gap(at(11, 50), at(12, 10)), Some(&none), local));
    }

    #[test]
    fn a_hole_is_done_only_when_a_piece_was_kept_or_all_clashed() {
        assert_eq!(pieces_outcome(2, 1), Pieces::Kept);
        assert_eq!(pieces_outcome(0, 0), Pieces::AllClashed);
        assert_eq!(pieces_outcome(0, 3), Pieces::CouldNotKeep, "every save refused is a failed try, not 'already filled'");
    }

    #[test]
    fn a_mode_change_made_in_the_camera_form_is_noted_at_once() {
        let engine = Engine::for_test(toml::from_str("").unwrap());
        let h = engine.add_test_camera("Porch");
        let id = h.id;
        let set = |motion_only: bool| {
            assert!(engine.update_camera(id, CameraConfig { record: true, record_motion_only: motion_only, url: "rtsp://127.0.0.1:1/x".into(), ..cam("Porch") }));
        };
        set(false);
        let first = read_mode(&engine.db, "porch").expect("noted");
        assert!(first.0);
        // Continuous -> motion only -> continuous, quicker than the fill loop looks.
        std::thread::sleep(std::time::Duration::from_millis(5));
        set(true);
        assert_eq!(read_mode(&engine.db, "porch").map(|m| m.0), Some(false));
        std::thread::sleep(std::time::Duration::from_millis(5));
        set(false);
        let now = read_mode(&engine.db, "porch").unwrap();
        assert!(now.0 && now.1 > first.1, "continuous again only from the last change: {now:?} vs {first:?}");
        engine.remove_camera(id);
    }

    #[test]
    fn attempts_are_pruned_and_follow_the_camera() {
        let db = crate::db::Db::open_in_memory().unwrap();
        let now = 100 * 3_600_000i64;
        let a = |from_ms: i64| Attempt { from_ms, to_ms: from_ms + 60_000, what: Tried::Done };
        save_tried(&db, "porch", a(now - 2 * KEEP_TRIED_MS), now - 2 * KEEP_TRIED_MS);
        save_tried(&db, "porch", a(now - 3_600_000), now);
        save_tried(&db, "yard", a(now - 7_200_000), now);
        assert_eq!(load_tried(&db, "porch").len(), 1, "the old one went");
        note_mode(&db, "Porch", true, 5);
        // A rename moves them, a removal deletes them.
        db.rename_camera("Porch", "Front Porch").unwrap();
        assert!(load_tried(&db, "porch").is_empty());
        assert_eq!(load_tried(&db, "front_porch").len(), 1);
        assert_eq!(read_mode(&db, "front_porch"), Some((true, 5)));
        assert_eq!(load_tried(&db, "yard").len(), 1);
        forget_camera(&db, "Front Porch");
        assert!(load_tried(&db, "front_porch").is_empty() && read_mode(&db, "front_porch").is_none());
        assert_eq!(load_tried(&db, "yard").len(), 1, "another camera's are kept");
        // An attempt that did not count puts back what was there.
        save_tried(&db, "yard", Attempt { what: Tried::Failed(1, now), ..a(now - 600_000) }, now);
        restore_tried(&db, "yard", now - 600_000, None);
        assert_eq!(load_tried(&db, "yard").len(), 1);
    }

    /// A camera's web API on a local port answering sign-in (`token`: given,
    /// or refused), the clock and sign-out; returns its port and the commands asked.
    fn fake_api(token: bool) -> (u16, std::sync::Arc<std::sync::Mutex<Vec<String>>>) {
        use std::io::Write;
        let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = l.local_addr().unwrap().port();
        let log = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let seen = log.clone();
        std::thread::spawn(move || {
            for s in l.incoming() {
                let Ok(mut s) = s else { break };
                // The whole request, body too (an unread body turns the close into a reset on Windows).
                let (head, _) = crate::test_ports::read_http_request(&mut s);
                let line = head.lines().next().unwrap_or("").to_string();
                let cmd = line.split("cmd=").nth(1).and_then(|r| r.split(['&', ' ']).next()).unwrap_or("").to_string();
                seen.lock().unwrap().push(cmd.clone());
                let body = match cmd.as_str() {
                    "Login" if token => serde_json::json!([{"cmd": "Login", "code": 0, "value": {"Token": {"leaseTime": 3600, "name": "t0k"}}}]),
                    "Login" => serde_json::json!([{"cmd": "Login", "code": 1, "error": {"detail": "login failed", "rspCode": -7}}]),
                    "GetTime" => serde_json::json!([{"cmd": "GetTime", "code": 0, "value": {"Time": {"year": 2026, "mon": 11, "day": 16, "hour": 3, "min": 0, "sec": 0}}}]),
                    _ => serde_json::json!([{"cmd": cmd, "code": 0, "value": {"rspCode": 200}}]),
                }
                .to_string();
                let _ = s.write_all(format!("HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len()).as_bytes());
            }
        });
        (port, log)
    }

    #[test]
    fn a_session_signs_out_and_plain_http_is_only_for_a_closed_port() {
        // Signed in, used, dropped: signed out.
        let (port, log) = fake_api(true);
        {
            let mut s = connect_to(&[format!("http://127.0.0.1:{port}")], "u", "p").unwrap();
            s.camera_clock().unwrap();
        }
        assert_eq!(log.lock().unwrap().last().map(String::as_str), Some("Logout"), "{:?}", log.lock().unwrap());
        // Nothing on the first address: the next is tried.
        let (port, log) = fake_api(true);
        let closed = crate::test_ports::closed_port();
        assert!(connect_to(&[format!("http://127.0.0.1:{}", closed.port), format!("http://127.0.0.1:{port}")], "u", "p").is_ok());
        assert!(log.lock().unwrap().iter().any(|c| c == "Login"));
        // The first address refuses the sign-in: the password is not sent to the next.
        let (refuse, _) = fake_api(false);
        let (port, log) = fake_api(true);
        assert!(connect_to(&[format!("http://127.0.0.1:{refuse}"), format!("http://127.0.0.1:{port}")], "u", "p").is_err());
        assert!(log.lock().unwrap().is_empty(), "{:?}", log.lock().unwrap());
    }
}
