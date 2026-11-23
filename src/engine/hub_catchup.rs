//! Catching up on a battery camera's events from its Reolink hub.
//!
//! A battery camera behind a Reolink Home Hub records each trigger of its
//! motion sensor to the hub's storage. AmberSight hears of a trigger only
//! when the hub flags it: and the hub flags only what the camera's own AI
//! calls a person, vehicle or animal (its plain motion flag stays off for a
//! battery camera). Every other recording is on the hub with no event here.
//! What the hub does say is when the camera wakes and sleeps again; a
//! battery camera wakes for a trigger (or for a viewer).
//!
//! So the hub's recordings of the last hours are listed (its web API: no
//! wake, the recordings are on the hub) shortly after the camera went back
//! to sleep, and every 15 minutes besides, and each one AmberSight has no
//! event or recording for is downloaded and imported as an ordinary event
//! (card_catchup.rs makes it: recording, thumbnail, AI check, a notification
//! when it is recent), marked as from the card.

use super::*;
use super::card_catchup::{self as cc, Imported, Verdict};
use super::card_fill::{already_handled, Attempt, Tried};
use crate::cardfill::{self, Gap};
use std::collections::HashMap;
use std::sync::Mutex as StdMutex;

/// ui_kv scope of what was tried (`{camera}|{clip start ms}`).
const SCOPE: &str = "hub_catchup_tries";
/// How far back the hub is looked at.
const LOOK_BACK_MS: i64 = cc::LOOK_BACK_MS;
/// A look this long after the camera went back to sleep (the hub closes the
/// recording first).
const AFTER_SLEEP_MS: i64 = 60_000;
/// And one this often besides (AmberSight was down, a wake not seen).
const EVERY_MS: i64 = 15 * 60_000;
/// The first look after a start.
const FIRST_LOOK_AFTER_MS: i64 = 2 * 60_000;
/// No two looks of a camera closer than this.
const MIN_SPACING_MS: i64 = 60_000;
/// Recordings that ended less than this long ago are left for the next look.
const ENDED_AGO_MS: i64 = 20_000;
/// One look: recordings imported, and how long it may take.
const MAX_IMPORTS: usize = 10;
const MAX_TIME: std::time::Duration = std::time::Duration::from_secs(10 * 60);
/// A camera whose hub could not be listed this many times in a row is looked
/// at only every EVERY_MS (not after each sleep).
const FAIL_LIMIT: u32 = 3;

/// How to reach the camera's recordings on its hub.
#[derive(Clone, Debug, PartialEq)]
pub struct HubTarget {
    pub fill: super::card_fill::Target,
}

/// Whether this camera's hub can be looked at, and how; the reason when not.
pub fn target(cfg: &CameraConfig) -> Result<HubTarget, &'static str> {
    let channel = cfg.hub_channel().ok_or("not a battery camera behind a Reolink hub")?;
    let mut c = cfg.clone();
    normalize_creds(&mut c);
    let host = crate::onvif::host_from_rtsp_url(&c.url).ok_or("the hub's address could not be read from the camera's URL")?;
    let (user, pass) = if c.username.is_empty() { crate::onvif::credentials_from_url(&c.url).unwrap_or_default() } else { (c.username.clone(), c.password.clone()) };
    if user.is_empty() {
        return Err("needs the hub's user name and password");
    }
    let stream = if c.record_sub && c.sub_url.as_deref().is_some_and(|u| !u.is_empty()) { "sub" } else { "main" };
    Ok(HubTarget { fill: super::card_fill::Target { host, user, pass, channel, stream, audio: c.audio } })
}

// ---- wakes the hub reports ----

#[derive(Clone, Copy, Debug, Default, PartialEq)]
struct Wakes {
    /// Awake since then (as the hub said).
    awake_since_ms: Option<i64>,
    /// A look is due at this time (the camera went back to sleep).
    look_at_ms: Option<i64>,
    /// The hub has said the camera sleeps: a battery camera, which this is
    /// for (a channel that never sleeps, e.g. an always-on camera set to
    /// connect only when needed, is not looked at).
    seen_asleep: bool,
}

/// A camera reported awake longer than this (charging, or a sleep the hub's
/// readings missed) no longer holds the looks back.
const AWAKE_AT_MOST_MS: i64 = 30 * 60_000;

static WAKES: StdMutex<Option<HashMap<String, Wakes>>> = StdMutex::new(None);

/// What the hub says of the camera's sleep (camera_events.rs, on each change):
/// a look follows soon after it sleeps again.
pub fn sleep_state(name: &str, sleeping: Option<bool>, now_ms: i64) {
    let mut g = WAKES.lock().unwrap_or_else(|e| e.into_inner());
    let w = g.get_or_insert_with(Default::default).entry(name.to_string()).or_default();
    note_sleep(w, sleeping, now_ms);
}

fn note_sleep(w: &mut Wakes, sleeping: Option<bool>, now_ms: i64) {
    match sleeping {
        Some(false) => {
            if w.awake_since_ms.is_none() {
                w.awake_since_ms = Some(now_ms);
            }
        }
        Some(true) => {
            if w.awake_since_ms.is_some() {
                w.look_at_ms = Some(now_ms + AFTER_SLEEP_MS);
            }
            w.awake_since_ms = None;
            w.seen_asleep = true;
        }
        None => {}
    }
}

fn wakes(name: &str) -> Wakes {
    WAKES.lock().unwrap_or_else(|e| e.into_inner()).as_ref().and_then(|m| m.get(name).copied()).unwrap_or_default()
}

/// The look that was due is being made (a later sleep sets the next).
fn take_look(name: &str, due_ms: i64) {
    if let Some(w) = WAKES.lock().unwrap_or_else(|e| e.into_inner()).as_mut().and_then(|m| m.get_mut(name)) {
        // (Any look covers what a sleep before it asked for; one "in the
        // future" after the clock was set back is covered too.)
        if w.look_at_ms.is_some_and(|t| t <= due_ms || t > due_ms + AFTER_SLEEP_MS) {
            w.look_at_ms = None;
        }
    }
}

/// A camera was removed or renamed: what was tried and noted goes.
pub fn forget_camera(db: &crate::db::Db, name: &str) {
    let _ = db.ui_delete_prefix(SCOPE, &format!("{}|", slug(name)));
    WAKES.lock().unwrap_or_else(|e| e.into_inner()).as_mut().map(|m| m.remove(name));
}

// ---- when to look ----

#[derive(Clone, Copy, Debug, Default)]
struct Cadence {
    since_ms: i64,
    last_look_ms: Option<i64>,
    failed_runs: u32,
}

/// Why a look is due now.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Why {
    Start,
    Slept,
    Periodic,
}

impl Why {
    fn text(self) -> &'static str {
        match self {
            Why::Start => "after the start",
            Why::Slept => "the camera was awake",
            Why::Periodic => "the regular look",
        }
    }
}

/// `periodic`: the 15-minute looks are on (catch_up_every is not 0).
fn due(c: &Cadence, w: &Wakes, periodic: bool, now_ms: i64) -> Option<Why> {
    if w.awake_since_ms.is_some_and(|t| now_ms - t < AWAKE_AT_MOST_MS && t <= now_ms) {
        // (Its recording is still being written.)
        return None;
    }
    // (A look "in the future": the clock was set back; it counts as long ago.)
    let last = c.last_look_ms.filter(|&t| t <= now_ms);
    if last.is_some_and(|t| now_ms - t < MIN_SPACING_MS) {
        return None;
    }
    let Some(last) = last else {
        return (now_ms - c.since_ms.min(now_ms) >= FIRST_LOOK_AFTER_MS || c.last_look_ms.is_some()).then_some(Why::Start);
    };
    if c.failed_runs < FAIL_LIMIT && w.look_at_ms.is_some_and(|t| now_ms >= t || t > now_ms + AFTER_SLEEP_MS) {
        return Some(Why::Slept);
    }
    // (A hub that kept failing is looked at on the regular looks even when they are off.)
    ((periodic || c.failed_runs >= FAIL_LIMIT) && now_ms - last >= EVERY_MS).then_some(Why::Periodic)
}

// ---- the loop ----

pub fn hub_catchup_loop(engine: Arc<Engine>) {
    // Not in the first minute after a start: the cameras are connecting.
    thread::sleep(std::time::Duration::from_secs(60));
    let mut seen: HashMap<usize, Cadence> = HashMap::new();
    while !super::card_fill::stopping() {
        let handles: Vec<Arc<CameraHandle>> = engine.cameras.read().unwrap().iter().flatten().cloned().collect();
        for h in handles {
            if super::card_fill::stopping() {
                return;
            }
            watch(&engine, &h, &mut seen);
        }
        seen.retain(|id, _| engine.camera(*id).is_some());
        thread::sleep(std::time::Duration::from_secs(10));
    }
}

fn watch(engine: &Arc<Engine>, h: &Arc<CameraHandle>, seen: &mut HashMap<usize, Cadence>) {
    let cfg = h.cfg.read().unwrap().clone();
    if cfg.hub_channel().is_none() {
        return;
    }
    let now = Utc::now().timestamp_millis();
    if !cfg.catch_up_on() {
        cc::clear_status(&cfg.name);
        seen.remove(&h.id);
        return;
    }
    let c = seen.entry(h.id).or_insert(Cadence { since_ms: now, ..Default::default() });
    // (The clock was set back: counted from now.)
    if c.since_ms > now {
        c.since_ms = now;
    }
    if !h.active.load(Ordering::Relaxed) || engine.camera(h.id).is_none_or(|x| !Arc::ptr_eq(&x, h)) {
        return;
    }
    let t = match target(&cfg) {
        Ok(t) => t,
        Err(why) => {
            cc::set_status(&cfg.name, |s| {
                s.state = "unavailable".into();
                s.text = format!("Cannot look at the hub's recordings: {why}.");
            });
            return;
        }
    };
    if !(cfg.record && engine.effective_camera(&cfg).record) {
        cc::set_status(&cfg.name, |s| {
            s.state = "unavailable".into();
            s.text = "Recording is off for this camera.".into();
        });
        return;
    }
    if engine.db.importing() {
        return;
    }
    let w = wakes(&cfg.name);
    if !w.seen_asleep {
        cc::set_status(&cfg.name, |s| {
            s.state = "waiting".into();
            s.text = "Waiting for the hub to say the camera is asleep (this is for battery cameras).".into();
        });
        return;
    }
    let Some(why) = due(c, &w, cfg.catch_up_minutes() != 0, now) else {
        cc::set_status(&cfg.name, |s| {
            if s.state.is_empty() || s.state == "waiting" || s.state == "unavailable" {
                s.state = "watching".into();
                s.text = "Looks at the hub's recordings a minute after the camera was awake, and every 15 minutes.".into();
            }
        });
        return;
    };
    // (One look at a time on this server, card or hub: shutdown waits for it.)
    let _busy = match cc::BUSY.try_lock() {
        Ok(g) => g,
        Err(std::sync::TryLockError::Poisoned(p)) => p.into_inner(),
        Err(std::sync::TryLockError::WouldBlock) => return,
    };
    if super::card_fill::stopping() {
        return;
    }
    take_look(&cfg.name, now);
    c.last_look_ms = Some(now);
    let ok = match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| look(engine, h, &cfg, &t, why))) {
        Ok(ok) => ok,
        Err(_) => {
            cardfill::clear_staging(&cc::segments_dir(engine, &cfg));
            warn!("camera '{}': looking at the hub's recordings failed (an internal error)", cfg.name);
            false
        }
    };
    c.failed_runs = if ok { 0 } else { c.failed_runs + 1 };
}

/// What one look may still take.
struct Budget {
    deadline: std::time::Instant,
    imports_left: usize,
}

/// List the hub's recordings of the camera and import what is missing.
/// False: the hub could not be listed.
fn look(engine: &Arc<Engine>, h: &Arc<CameraHandle>, cfg: &CameraConfig, t: &HubTarget, why: Why) -> bool {
    let name = cfg.name.clone();
    let slug_name = slug(&name);
    let stop = || super::card_fill::stopping() || !h.active.load(Ordering::Relaxed) || engine.camera(h.id).is_none_or(|c| !Arc::ptr_eq(&c, h));
    cc::set_status(&name, |s| {
        s.state = "checking".into();
        s.text = "Looking at the hub's recordings for events AmberSight missed.".into();
    });
    tracing::debug!("camera '{name}': looking at the hub's recordings ({})", why.text());
    let now = Utc::now().timestamp_millis();
    let from = now - LOOK_BACK_MS;
    let listed = super::card_fill::connect(&t.fill).and_then(|mut session| {
        let clock = cardfill::measure_clock(&mut session)?;
        let files = session.search(t.fill.channel, t.fill.stream, clock.to_card(from), clock.to_card(now))?;
        Ok((session, clock, cardfill::files_for(&files, &clock, from, now)))
    });
    let (mut session, clock, files) = match listed {
        Ok(l) => l,
        Err(e) => {
            warn!("camera '{name}': could not list the hub's recordings ({e:#})");
            cc::set_status(&name, |s| {
                s.state = "problem".into();
                s.text = "Could not list the hub's recordings. Trying again later.".into();
                s.last_check = Some(now);
            });
            return false;
        }
    };
    let events = engine.db.event_spans(&name, from / 1000 - 60, now / 1000).unwrap_or_default();
    let recordings: Vec<(i64, i64)> = engine.db.segments_overlapping(&slug_name, from - 60_000, now).unwrap_or_default().iter().map(|r| (r.start_ms, r.end_ms)).collect();
    let tried = super::card_fill::load_tried_in(&engine.db, SCOPE, &slug_name);
    let dir = cc::segments_dir(engine, cfg);
    let segment_seconds = engine.config.lock().unwrap().storage.segment_seconds;
    let mut budget = Budget { deadline: std::time::Instant::now() + MAX_TIME, imports_left: MAX_IMPORTS };
    let (mut imported, mut failed, mut not_meant) = (0u32, 0u32, 0u32);
    for f in &files {
        if stop() {
            break;
        }
        let (start_ms, end_ms) = (clock.to_server_ms(f.start), clock.to_server_ms(f.end));
        if end_ms > now - ENDED_AGO_MS {
            continue;
        }
        let gap = Gap { from_ms: start_ms, to_ms: end_ms };
        let verdict = cc::judge((start_ms, end_ms), &events, &recordings);
        if matches!(verdict, Verdict::Present) || already_handled(&tried, gap, now) {
            continue;
        }
        let before = cc::earlier_try(&tried, gap);
        let (key_from, key_to) = before.map_or((start_ms, end_ms), |a| (a.from_ms, a.to_ms));
        let mark = |what: Tried| super::card_fill::save_tried_in(&engine.db, SCOPE, &slug_name, Attempt { from_ms: key_from, to_ms: key_to, what }, Utc::now().timestamp_millis());
        if let Verdict::Skip(w) = verdict {
            info!("camera '{name}': left the hub's recording from {}: {w}", cc::clock_text(start_ms));
            mark(Tried::Empty);
            continue;
        }
        if !cc::meant_recording(engine, cfg, start_ms, end_ms) {
            not_meant += 1;
            mark(Tried::Empty);
            continue;
        }
        // The hub's clock was read now: a recording from before a daylight-saving change would be placed an hour off.
        if !cardfill::same_offset(cardfill::local_offset, &[start_ms, end_ms, now]) {
            info!("camera '{name}': left the hub's recording from {}: the clocks changed for daylight saving time since", cc::clock_text(start_ms));
            mark(Tried::Empty);
            continue;
        }
        if budget.imports_left == 0 || std::time::Instant::now() >= budget.deadline {
            break;
        }
        budget.imports_left -= 1;
        let n = before.map(|a| if let Tried::Failed(n, _) = a.what { n } else { 0 }).unwrap_or(0) + 1;
        // Written down before the hub is asked: work that crashes the server
        // counts as a try, so it is not repeated after every restart.
        mark(Tried::Failed(n, Utc::now().timestamp_millis()));
        cc::set_status(&name, |s| {
            s.state = "checking".into();
            s.text = format!("Importing the hub's recording from {}.", cc::clock_text(start_ms));
        });
        match import(engine, h, cfg, &mut session, f, gap, verdict == Verdict::VideoOnly, &dir, segment_seconds, &stop) {
            Ok(Imported::Event) => {
                mark(Tried::Done);
                imported += 1;
            }
            Ok(Imported::Video) => mark(Tried::Done),
            Ok(Imported::Nothing(w)) => {
                info!("camera '{name}': nothing imported for the hub's recording from {}: {w}", cc::clock_text(start_ms));
                mark(Tried::Empty);
            }
            Err(e) => {
                if stop() {
                    super::card_fill::restore_tried_in(&engine.db, SCOPE, &slug_name, key_from, before);
                    break;
                }
                failed += 1;
                warn!("camera '{name}': could not import the hub's recording from {} ({e:#}); try {n} of 3", cc::clock_text(start_ms));
            }
        }
    }
    cardfill::clear_staging(&dir);
    drop(session);
    let done_ms = Utc::now().timestamp_millis();
    if imported > 0 {
        info!("camera '{name}': imported {imported} event(s) from the hub's recordings");
    }
    if not_meant > 0 {
        tracing::debug!("camera '{name}': left {not_meant} recording(s) on the hub from times the camera was not armed or recording");
    }
    cc::set_status(&name, |s| {
        s.state = if failed > 0 { "problem".into() } else { "watching".into() };
        s.last_check = Some(done_ms);
        if imported > 0 {
            s.last_import = Some(done_ms);
            s.last_imported = imported;
            s.imported += imported;
        }
        let what = match (imported, s.last_import) {
            (0, Some(at)) => format!("Looked at the hub's recordings at {}: nothing missing. Last imported {} event{} at {}.", cc::clock_text(done_ms), s.last_imported, if s.last_imported == 1 { "" } else { "s" }, cc::clock_text(at)),
            (0, None) => format!("Looked at the hub's recordings at {}: nothing missing.", cc::clock_text(done_ms)),
            (n, _) => format!("Imported {n} event{} from the hub's recordings at {}.", if n == 1 { "" } else { "s" }, cc::clock_text(done_ms)),
        };
        s.text = if failed > 0 { format!("{what} {failed} could not be imported; trying again later.") } else { what };
    });
    true
}

/// Download one recording from the hub into the camera's recordings, then
/// make its event (card_catchup::make_event). The hub's recordings are
/// fragmented MP4 (a header with no sample table, then fragments): it is
/// downloaded whole (the hub ignores byte ranges; a battery camera's
/// recording is a few MB) and copied by ffmpeg, unchanged, into the layout
/// of AmberSight's own recordings, dated by the hub's listing.
#[allow(clippy::too_many_arguments)]
fn import(
    engine: &Arc<Engine>,
    h: &Arc<CameraHandle>,
    cfg: &CameraConfig,
    session: &mut crate::reolink::ReolinkClient,
    file: &crate::reolink::CardFile,
    gap: Gap,
    video_only: bool,
    dir: &std::path::Path,
    segment_seconds: u64,
    stop: &dyn Fn() -> bool,
) -> anyhow::Result<Imported> {
    use anyhow::{anyhow, bail, Context};
    let name = cfg.name.clone();
    let slug_name = slug(&name);
    if file.size > crate::reolink::MAX_WHOLE_BYTES {
        return Ok(Imported::Nothing(format!("too big to download whole ({} MB)", file.size >> 20)));
    }
    let bytes = download_whole(session, &file.name, stop)?;
    if stop() {
        bail!("stopped");
    }
    let staging = cardfill::staging_dir(dir);
    cardfill::clear_staging(dir);
    std::fs::create_dir_all(&staging).context("making the staging folder")?;
    let downloaded = staging.join("hub-download.mp4");
    std::fs::write(&downloaded, &bytes).context("saving the download")?;
    drop(bytes);
    let out = staging.join(format!("{slug_name}-card-{}-rec-seg000000.mp4", gap.from_ms));
    let copied = copy_into_layout(&downloaded, &out, cfg.audio);
    let _ = std::fs::remove_file(&downloaded);
    let (bytes, frames, frags, length_ms) = match copied {
        Ok(c) => c,
        Err(e) => {
            cardfill::clear_staging(dir);
            return Err(e);
        }
    };
    if frames == 0 {
        cardfill::clear_staging(dir);
        return Ok(Imported::Nothing("no video in the recording".into()));
    }
    // Its end: by its own length (the listing is to the second, and its own
    // idea of when it ended), the listing's only when the length cannot be told.
    // (A length far from the listing's, a timestamp jump in the file, is not believed.)
    let listed = gap.to_ms - gap.from_ms;
    let end_ms = length_ms
        .filter(|&l| l > 0 && l <= listed.saturating_add(10_000))
        .and_then(|l| gap.from_ms.checked_add(l))
        .unwrap_or(gap.to_ms)
        .max(gap.from_ms + 1_000);
    let seg = crate::ingest::recorder::SegmentInfo { path: out, start_ms: gap.from_ms, end_ms, bytes, video_frames: frames, fragments: frags };
    // (A rename of the camera waits for what is kept now.)
    let _writing = engine.writing_into(&name);
    if stop() {
        cardfill::clear_staging(dir);
        bail!("stopped");
    }
    // Never on top of what is recorded (the live recording may have begun
    // meanwhile; a second at each end is left for seams, if it is long enough).
    let margin = if seg.end_ms - seg.start_ms > 3_000 { 1_000 } else { 0 };
    let clash = engine.db.segments_overlapping(&slug_name, seg.start_ms + margin, seg.end_ms - margin).map(|r| !r.is_empty()).unwrap_or(true);
    if clash {
        cardfill::clear_staging(dir);
        return Ok(Imported::Nothing("recorded here meanwhile".into()));
    }
    let seg = cardfill::commit_piece(seg, dir).map_err(|e| anyhow!("could not keep the recording: {e}"))?;
    let (from_ms, to_ms) = (seg.start_ms, seg.end_ms);
    engine.segment_closed(&slug_name, seg);
    cardfill::clear_staging(dir);
    Ok(cc::make_event(engine, h, cfg, from_ms, to_ms, video_only, segment_seconds, dir))
}

/// A hub turns away a download that follows another closely (it hangs up
/// with nothing, or resets): a pause before each, and up to three tries.
const DOWNLOAD_PAUSE: std::time::Duration = std::time::Duration::from_secs(3);
const DOWNLOAD_RETRY: std::time::Duration = std::time::Duration::from_secs(5);
const DOWNLOAD_TRIES: u32 = 3;

/// Download a recording of the hub whole.
fn download_whole(session: &mut crate::reolink::ReolinkClient, name: &str, stop: &dyn Fn() -> bool) -> anyhow::Result<Vec<u8>> {
    let mut last = anyhow::anyhow!("not tried");
    for n in 0..DOWNLOAD_TRIES {
        thread::sleep(if n == 0 { DOWNLOAD_PAUSE } else { DOWNLOAD_RETRY });
        if stop() {
            anyhow::bail!("stopped");
        }
        match session.download_max(name, None, crate::reolink::MAX_WHOLE_BYTES) {
            // (The hub says no length: a download cut short is caught by
            // its boxes, and tried again.)
            Ok(b) if whole_mp4(&b) => return Ok(b),
            Ok(b) if b.is_empty() => last = anyhow::anyhow!("the hub sent nothing"),
            Ok(b) => last = anyhow::anyhow!("the download was cut short ({} bytes)", b.len()),
            Err(e) => last = e,
        }
    }
    Err(last.context("downloading the recording"))
}

/// The longest ffmpeg may take to copy one recording.
const COPY_TIME: std::time::Duration = std::time::Duration::from_secs(120);

/// Copy a downloaded recording (MP4) into the layout of AmberSight's
/// recordings at `out`, nothing re-encoded, the sound only when `audio`
/// (the camera's setting): (bytes, video samples, fragments, length in ms).
fn copy_into_layout(downloaded: &std::path::Path, out: &std::path::Path, audio: bool) -> anyhow::Result<(u64, u64, u32, Option<i64>)> {
    use anyhow::{anyhow, bail, Context};
    let mut cmd = crate::child::ffmpeg();
    // (Only this file, read as MP4: nothing in it can point ffmpeg elsewhere.)
    cmd.args(["-v", "error", "-y", "-protocol_whitelist", "file", "-f", "mov", "-i"]).arg(downloaded).args(["-map", "0:v:0"]);
    if audio {
        cmd.args(["-map", "0:a:0?"]);
    } else {
        cmd.arg("-an");
    }
    cmd.args(["-c", "copy", "-f", "mp4", "-movflags", "+frag_keyframe+empty_moov+default_base_moof"]).arg(out);
    // (What ffmpeg says goes to a file beside the copy: a pipe nobody reads
    // while it runs could fill and stop it.)
    let log = out.with_extension("log");
    let log_file = std::fs::File::create(&log).context("making ffmpeg's log")?;
    cmd.stdout(std::process::Stdio::null()).stderr(log_file);
    let mut child = cmd.spawn().map_err(|e| anyhow!("could not run ffmpeg: {e}"))?;
    let until = std::time::Instant::now() + COPY_TIME;
    let status = loop {
        if let Some(st) = child.try_wait().context("waiting for ffmpeg")? {
            break st;
        }
        if std::time::Instant::now() >= until {
            let _ = child.kill();
            let _ = child.wait();
            let _ = std::fs::remove_file(&log);
            bail!("ffmpeg took too long to copy the recording");
        }
        thread::sleep(std::time::Duration::from_millis(100));
    };
    let err = std::fs::read_to_string(&log).unwrap_or_default();
    let _ = std::fs::remove_file(&log);
    if !status.success() {
        bail!("ffmpeg could not read the recording ({})", err.lines().last().unwrap_or(""));
    }
    if err.contains("partial file") {
        bail!("the recording was cut short");
    }
    let _ = crate::ingest::mp4read::hev1_file_to_hvc1(out);
    let written = std::fs::read(out).context("reading the copy")?;
    Ok((written.len() as u64, video_samples(&written), fragments(&written), video_ms(&written)))
}

/// Whether a download is all of a fragmented MP4: it begins with ftyp, has
/// a moov, its boxes end where the bytes do (none open-ended), and each
/// fragment (moof) has its media (mdat) after it. A download cut short fails.
fn whole_mp4(b: &[u8]) -> bool {
    let mut i = 0usize;
    let mut types: Vec<[u8; 4]> = Vec::new();
    while i < b.len() {
        let Some(h) = b.get(i..i + 8) else { return false };
        let mut size = u64::from(u32::from_be_bytes(h[0..4].try_into().unwrap()));
        let mut head = 8u64;
        if size == 1 {
            let Some(l) = b.get(i + 8..i + 16) else { return false };
            size = u64::from_be_bytes(l.try_into().unwrap());
            head = 16;
        }
        // (Size 0, to the end of the file: cannot be told from one cut short.)
        if size < head {
            return false;
        }
        let Some(end) = (i as u64).checked_add(size).filter(|&e| e <= b.len() as u64) else { return false };
        types.push(h[4..8].try_into().unwrap());
        i = end as usize;
    }
    let moofs_have_media = types.iter().enumerate().all(|(n, t)| t != b"moof" || types.get(n + 1) == Some(b"mdat"));
    types.first() == Some(b"ftyp") && types.contains(b"moov") && moofs_have_media
}

/// The top-level boxes of an MP4: (type, body).
fn boxes(b: &[u8]) -> Vec<([u8; 4], &[u8])> {
    boxes_to(b).0
}

/// The boxes, and where the last whole one ends.
fn boxes_to(b: &[u8]) -> (Vec<([u8; 4], &[u8])>, usize) {
    let mut out = Vec::new();
    let mut i = 0usize;
    while i + 8 <= b.len() {
        let mut size = u32::from_be_bytes(b[i..i + 4].try_into().unwrap()) as u64;
        let typ: [u8; 4] = b[i + 4..i + 8].try_into().unwrap();
        let mut head = 8u64;
        if size == 1 && i + 16 <= b.len() {
            size = u64::from_be_bytes(b[i + 8..i + 16].try_into().unwrap());
            head = 16;
        } else if size == 0 {
            size = (b.len() - i) as u64;
        }
        if size < head || (i as u64).checked_add(size).is_none_or(|e| e > b.len() as u64) {
            break;
        }
        out.push((typ, &b[i + head as usize..i + size as usize]));
        i += size as usize;
    }
    (out, i)
}

/// The video's length in ms in a fragmented MP4 as ffmpeg writes it (its
/// first track): its timescale (mdhd) and the samples' durations (trun,
/// else tfhd's or trex's default). None when it cannot be told.
fn video_ms(b: &[u8]) -> Option<i64> {
    let be32 = |x: &[u8], at: usize| x.get(at..at + 4).map(|v| u32::from_be_bytes(v.try_into().unwrap()));
    let top = boxes(b);
    let moov = top.iter().find(|(t, _)| t == b"moov")?.1;
    let inner = boxes(moov);
    let trak = inner.iter().find(|(t, _)| t == b"trak")?.1;
    let mdia = boxes(trak).into_iter().find(|(t, _)| t == b"mdia")?.1;
    let mdhd = boxes(mdia).into_iter().find(|(t, _)| t == b"mdhd")?.1;
    let scale = if mdhd.first() == Some(&1) { be32(mdhd, 20)? } else { be32(mdhd, 12)? };
    if scale == 0 {
        return None;
    }
    let trex_default = inner
        .iter()
        .find(|(t, _)| t == b"mvex")
        .and_then(|(_, mvex)| boxes(mvex).into_iter().find(|(t, body)| t == b"trex" && be32(body, 4) == Some(1)))
        .and_then(|(_, trex)| be32(trex, 12));
    let mut ticks: u64 = 0;
    for (_, moof) in top.iter().filter(|(t, _)| t == b"moof") {
        for (_, traf) in boxes(moof).into_iter().filter(|(t, _)| t == b"traf") {
            let parts = boxes(traf);
            let tfhd = parts.iter().find(|(t, _)| t == b"tfhd")?.1;
            if be32(tfhd, 4) != Some(1) {
                continue;
            }
            let flags = be32(tfhd, 0)? & 0xff_ffff;
            let mut at = 8 + if flags & 0x01 != 0 { 8 } else { 0 } + if flags & 0x02 != 0 { 4 } else { 0 };
            let default = if flags & 0x08 != 0 { let d = be32(tfhd, at); at += 4; d } else { trex_default };
            let _ = at;
            for (_, trun) in parts.iter().filter(|(t, _)| t == b"trun") {
                let tf = be32(trun, 0)? & 0xff_ffff;
                let count = be32(trun, 4)? as usize;
                let mut p = 8 + if tf & 0x01 != 0 { 4 } else { 0 } + if tf & 0x04 != 0 { 4 } else { 0 };
                let per = [0x100, 0x200, 0x400, 0x800].iter().filter(|&&f| tf & f != 0).count() * 4;
                if count > trun.len() / per.max(1) + 1 {
                    return None;
                }
                for _ in 0..count {
                    let d = if tf & 0x100 != 0 { be32(trun, p)? } else { default? };
                    ticks = ticks.saturating_add(u64::from(d));
                    p += per;
                }
            }
        }
    }
    Some((ticks.saturating_mul(1000) / u64::from(scale)) as i64)
}

/// How many fragments (moof boxes) an MP4 has.
fn fragments(b: &[u8]) -> u32 {
    boxes(b).iter().filter(|(t, _)| t == b"moof").count() as u32
}

/// How many video samples a fragmented MP4 holds (its first track, the
/// video as ffmpeg was told to write it).
fn video_samples(b: &[u8]) -> u64 {
    let mut n = 0u64;
    for (_, moof) in boxes(b).into_iter().filter(|(t, _)| t == b"moof") {
        for (_, traf) in boxes(moof).into_iter().filter(|(t, _)| t == b"traf") {
            let inner = boxes(traf);
            let track = inner.iter().find(|(t, _)| t == b"tfhd").and_then(|(_, body)| body.get(4..8)).map(|x| u32::from_be_bytes(x.try_into().unwrap()));
            if track != Some(1) {
                continue;
            }
            for (_, trun) in inner.iter().filter(|(t, _)| t == b"trun") {
                if let Some(c) = trun.get(4..8) {
                    n += u64::from(u32::from_be_bytes(c.try_into().unwrap()));
                }
            }
        }
    }
    n
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_look_follows_a_sleep_and_the_regular_ones() {
        let min = 60_000;
        let mut w = Wakes::default();
        let mut c = Cadence { since_ms: 0, ..Default::default() };
        // The first look, two minutes after the start.
        assert_eq!(due(&c, &w, true, min), None);
        assert_eq!(due(&c, &w, true, 2 * min), Some(Why::Start));
        c.last_look_ms = Some(2 * min);
        // Awake: nothing (its recording is being written); asleep again: a minute later.
        assert!(!w.seen_asleep);
        note_sleep(&mut w, Some(false), 5 * min);
        assert_eq!(due(&c, &w, true, 6 * min), None);
        note_sleep(&mut w, Some(true), 6 * min);
        assert_eq!(w.look_at_ms, Some(7 * min));
        assert!(w.seen_asleep, "a battery camera: it sleeps");
        assert_eq!(due(&c, &w, true, 6 * min + 30_000), None);
        assert_eq!(due(&c, &w, true, 7 * min), Some(Why::Slept));
        c.last_look_ms = Some(7 * min);
        w.look_at_ms = None;
        // A sleep with no wake before it (first reading) sets no look.
        note_sleep(&mut w, Some(true), 8 * min);
        assert_eq!(w.look_at_ms, None);
        // Every 15 minutes besides, unless those are off.
        assert_eq!(due(&c, &w, true, 21 * min), None);
        assert_eq!(due(&c, &w, true, 22 * min), Some(Why::Periodic));
        assert_eq!(due(&c, &w, false, 22 * min), None);
        // Never two within a minute, even for a sleep.
        note_sleep(&mut w, Some(false), 7 * min);
        note_sleep(&mut w, Some(true), 7 * min);
        c.last_look_ms = Some(8 * min);
        w.look_at_ms = Some(8 * min);
        assert_eq!(due(&c, &w, true, 8 * min + 10_000), None);
        assert_eq!(due(&c, &w, true, 9 * min), Some(Why::Slept));
        // A hub that keeps failing is looked at only on the regular looks, also when those are off.
        c.failed_runs = FAIL_LIMIT;
        assert_eq!(due(&c, &w, true, 9 * min), None);
        assert_eq!(due(&c, &w, true, 23 * min), Some(Why::Periodic));
        assert_eq!(due(&c, &w, false, 23 * min), Some(Why::Periodic));
        c.failed_runs = 0;
        // Awake for longer than half an hour (charging, a missed sleep): looked at all the same.
        let mut w = Wakes::default();
        note_sleep(&mut w, Some(false), 30 * min);
        assert_eq!(due(&c, &w, true, 40 * min), None);
        assert_eq!(due(&c, &w, true, 61 * min), Some(Why::Periodic));
        // A sleep's look "in the future" (the clock set back) is taken once, not every minute.
        sleep_state("clock-back", Some(false), 100 * min);
        sleep_state("clock-back", Some(true), 100 * min);
        take_look("clock-back", 10 * min);
        assert_eq!(wakes("clock-back").look_at_ms, None);
        // The clock set back: a look "in the future" counts as long ago.
        let c = Cadence { since_ms: 0, last_look_ms: Some(500 * min), failed_runs: 0 };
        assert_eq!(due(&c, &Wakes::default(), true, 100 * min), Some(Why::Start));
    }

    /// A fragmented recording like the hub's (a header with no sample
    /// table, then fragments; H.265 tagged hev1) is copied into AmberSight's
    /// layout with every picture, tagged hvc1, and plays as a recording.
    #[test]
    fn a_hub_recording_is_copied_into_a_recording() {
        let dir = std::env::temp_dir().join(format!("amber-hubcopy-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let hub = dir.join("hub.mp4");
        let made = crate::child::ffmpeg()
            .args(["-v", "error", "-y", "-f", "lavfi", "-i", "testsrc2=size=320x240:rate=15:duration=4", "-f", "lavfi", "-i", "sine=frequency=440:sample_rate=16000:duration=4"])
            .args(["-c:v", "libx265", "-x265-params", "keyint=15:log-level=error", "-tag:v", "hev1", "-c:a", "aac", "-shortest", "-movflags", "+frag_keyframe+empty_moov"])
            .arg(&hub)
            .status()
            .is_ok_and(|s| s.success());
        if !made {
            eprintln!("skipped: no ffmpeg with libx265");
            return;
        }
        // A download cut short is told from a whole one.
        let full = std::fs::read(&hub).unwrap();
        assert!(whole_mp4(&full));
        assert!(!whole_mp4(&full[..full.len() / 2]), "cut in the middle");
        assert!(!whole_mp4(&full[..full.len() - 1]), "one byte short");
        assert!(!whole_mp4(&full[..600]), "only the header");
        assert!(!whole_mp4(b""));
        // A box after the last fragment (free space, an index) is fine; an open-ended one is not.
        let mut more = full.clone();
        more.extend_from_slice(&[0, 0, 0, 8, b'f', b'r', b'e', b'e']);
        assert!(whole_mp4(&more));
        let mut open = full.clone();
        open.extend_from_slice(&[0, 0, 0, 0, b'm', b'd', b'a', b't', 1, 2, 3]);
        assert!(!whole_mp4(&open));
        let out = dir.join("side-card-1-rec-seg000000.mp4");
        let (bytes, frames, frags, ms) = copy_into_layout(&hub, &out, true).unwrap();
        assert_eq!(frames, 60, "every picture");
        assert!(ms.is_some_and(|ms| (3900..=4100).contains(&ms)), "its own length: {ms:?}");
        let has_audio = |p: &std::path::Path| std::fs::read(p).unwrap().windows(4).any(|w| w == b"mp4a");
        assert!(has_audio(&out), "the sound, when the camera records sound");
        let quiet = dir.join("quiet.mp4");
        let (_, qframes, _, _) = copy_into_layout(&hub, &quiet, false).unwrap();
        assert!(qframes == 60 && !has_audio(&quiet), "no sound when the camera's Audio is off");
        assert!(frags >= 4 && bytes > 1000, "{frags} fragments, {bytes} bytes");
        let head = std::fs::read(&out).unwrap();
        assert!(head.windows(4).any(|w| w == b"hvc1") && !head.windows(4).any(|w| w == b"hev1"), "tagged hvc1 for Apple players");
        assert!(crate::ingest::splice::plays_from_file(&out), "plays as a recording");
        // Not an MP4: refused, nothing written.
        std::fs::write(&hub, b"not a video").unwrap();
        assert!(copy_into_layout(&hub, &dir.join("x.mp4"), true).is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_hub_camera_seen_first_counts_since_the_look_back_or_the_last_arming() {
        let h = 3600 * 1000;
        let now = 100 * h;
        assert_eq!(cc::first_seen_since(now, None), now - LOOK_BACK_MS);
        assert_eq!(cc::first_seen_since(now, Some(now - 30 * h)), now - LOOK_BACK_MS);
        assert_eq!(cc::first_seen_since(now, Some(now - 2 * h)), now - 2 * h);
        // The audit log's arming lines.
        let db = crate::db::Db::open_in_memory().unwrap();
        assert_eq!(db.last_shield_change().unwrap(), None);
        db.insert_audit("u", "", "shield: disarmed", "", 200).unwrap();
        db.insert_audit("u", "", "settings saved", "", 200).unwrap();
        let t = db.last_shield_change().unwrap().unwrap();
        assert!((t - Utc::now().timestamp()).abs() < 5);
    }

    /// A renamed camera keeps what catching up tried and since when it records.
    #[test]
    fn a_rename_keeps_what_was_tried_and_the_mode() {
        let db = crate::db::Db::open_in_memory().unwrap();
        db.ui_put(SCOPE, "garage_side|1000", "done").unwrap();
        db.ui_put("card_catchup", "garage_side|2000", "done").unwrap();
        db.ui_put("card_catchup_mode", "garage_side", "on|5").unwrap();
        db.ui_put(SCOPE, "garage_sidewall|3000", "done").unwrap();
        db.rename_camera("Garage Side", "Side Yard").unwrap();
        assert_eq!(db.ui_get(SCOPE, "side_yard|1000").unwrap().as_deref(), Some("done"));
        assert_eq!(db.ui_get("card_catchup", "side_yard|2000").unwrap().as_deref(), Some("done"));
        assert_eq!(db.ui_get("card_catchup_mode", "side_yard").unwrap().as_deref(), Some("on|5"));
        assert!(db.ui_get(SCOPE, "garage_side|1000").unwrap().is_none());
        assert_eq!(db.ui_get(SCOPE, "garage_sidewall|3000").unwrap().as_deref(), Some("done"), "another camera's");
    }

    #[test]
    fn only_a_battery_camera_on_a_hub_channel_is_looked_at() {
        let cam = |extra: &str| -> CameraConfig { toml::from_str(&format!("name = \"c\"\nurl = \"https://192.0.2.22/flv?port=1935&app=bcs&stream=channel0_main.bcs\"\nusername = \"u\"\npassword = \"p\"\n{extra}")).unwrap() };
        let t = target(&cam("on_demand = true\nreolink_channel = 0\n")).unwrap();
        assert_eq!((t.fill.host.as_str(), t.fill.channel, t.fill.stream), ("192.0.2.22", 0, "main"));
        assert!(cam("on_demand = true\nreolink_channel = 0\n").catch_up_on());
        assert!(!cam("on_demand = true\nreolink_channel = 0\ncatch_up_from_card = false\n").catch_up_on());
        // An always-on hub or recorder channel (its holes: card_fill), one with no channel, a battery Argus.
        assert!(target(&cam("reolink_channel = 0\n")).is_err());
        assert!(target(&cam("on_demand = true\n")).is_err());
        let argus: CameraConfig = toml::from_str("name = \"c\"\nurl = \"baichuan://192.0.2.9\"\nuid = \"95270000ABCDEFGH\"\non_demand = true\nreolink_channel = 0\n").unwrap();
        assert!(argus.hub_channel().is_none());
        let nouser: CameraConfig = toml::from_str("name = \"c\"\nurl = \"https://192.0.2.22/flv?x\"\non_demand = true\nreolink_channel = 0\n").unwrap();
        assert!(target(&nouser).is_err());
    }
}
