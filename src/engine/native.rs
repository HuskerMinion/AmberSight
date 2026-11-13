//! Native camera worker: receives the camera with the built-in RTSP reader,
//! records direct-to-disk (fragmented MP4, no re-encoding), and decodes a
//! stream for motion analysis and the live view.
//!
//! - With `sub_url`, the main stream is only recorded and the smaller sub
//!   stream is decoded; without it, the main stream does both.
//! - Continuous recording writes everything; motion-only recording keeps a
//!   pre-roll buffer in memory and records while the camera reports motion,
//!   plus the post-roll.
//! - In "auto" mode a camera that never gets as far as playing (three tries
//!   in a row) is handed to the older ffmpeg worker instead.

use super::*;
use crate::ingest::decode::{Codec, FrameDecoder};
use crate::ingest::live::LiveMuxer;
use crate::ingest::preroll::{Frame, TriggeredRecorder};
use crate::ingest::recorder::{build_tracks, open_stream, MediaItem, OpenStream, SegmentInfo, SegmentWriter};
use crate::ingest::StreamSource;
use std::sync::mpsc;
use std::time::Duration;

/// Native ingest is used for rtsp:// and HTTP-FLV cameras unless the camera
/// says otherwise.
pub(super) fn wants_native(cfg: &CameraConfig) -> bool {
    // Pushed streams only exist inside this process (no URL ffmpeg could open).
    if crate::ingest::push::is_push_url(&cfg.url) {
        return true;
    }
    // Battery Argus (Baichuan) frames come from our own client, never ffmpeg.
    if crate::ingest::baichuan::is_baichuan_url(&cfg.url) {
        return true;
    }
    // Masks and the overlay change the picture: that needs re-encoding,
    // which the ffmpeg pipeline does.
    if cfg.overlay || !cfg.privacy_masks.is_empty() {
        return false;
    }
    // Capture devices and the screen are always read by ffmpeg, even with
    // ingest = "native".
    if crate::local_source::is_local(&cfg.url) {
        return false;
    }
    match cfg.ingest.as_str() {
        "ffmpeg" => false,
        "native" => true,
        _ => {
            cfg.url.starts_with("rtsp://")
                || cfg.url.starts_with("rtsps://")
                || crate::ingest::flv::is_flv_url(&cfg.url)
                || crate::ingest::push::is_push_url(&cfg.url)
        }
    }
}

/// No frames for this long means the camera stalled: reconnect.
/// Longest wait between reconnects (2, 4, 8 ... s).
const RETRY_MAX: Duration = Duration::from_secs(60);
const STALL: Duration = Duration::from_secs(10);
/// After waking an on-demand camera, give up on a silent stream after this
/// long and ask again. (A hub wakes its battery camera during DESCRIBE;
/// when the stream works the first frame follows PLAY within ~2-3 s, and
/// when it does not, nothing ever comes but a fresh request usually works.)
const WAKE_STALL: Duration = Duration::from_secs(8);
/// Pause before asking a silent, just-woken camera again.
const WAKE_RETRY: Duration = Duration::from_secs(10);
const NO_PICTURE_AFTER_WAKE: &str = "camera woke but sent no picture";
/// Connection attempts that never reach PLAY before "auto" falls back.
const FALLBACK_AFTER: u32 = 3;
/// "Busy" (5xx) answers before PLAY that still switch a camera that has
/// never played to ffmpeg (a reader-specific 500 would otherwise retry forever).
const BUSY_FALLBACK_AFTER: u32 = 20;
/// A camera that has played (in the last PLAYED_MEMORY) only switches to
/// ffmpeg after failing before PLAY for this long (a firmware change).
const PLAYED_STUCK: Duration = Duration::from_secs(30 * 60);
const PLAYED_MEMORY_S: i64 = 7 * 24 * 3600;
const PLAYED_KV: &str = "native_played";

/// The camera address as a short fingerprint (no sign-in kept in the database).
fn url_tag(url: &str) -> String {
    let mut h: u64 = 0xcbf29ce484222325;
    for b in url.bytes() {
        h = (h ^ b as u64).wrapping_mul(0x100000001b3);
    }
    format!("{h:016x}")
}

/// Whether the built-in reader played this camera (same address) recently,
/// so a restart that finds its hub slow does not move it to ffmpeg.
fn played_recently(engine: &Engine, slug: &str, url: &str, now_s: i64) -> bool {
    let Ok(Some(v)) = engine.db.ui_get(PLAYED_KV, slug) else { return false };
    played_value_ok(&v, url, now_s)
}

/// The built-in reader recorded a file for this camera: remember it (at most
/// every few hours), so a restart keeps the camera on the built-in reader.
fn note_played(engine: &Engine, slug_name: &str) {
    static LAST: std::sync::Mutex<Option<std::collections::HashMap<String, std::time::Instant>>> = std::sync::Mutex::new(None);
    {
        let mut g = LAST.lock().unwrap();
        let m = g.get_or_insert_with(Default::default);
        if m.get(slug_name).is_some_and(|t| t.elapsed() < Duration::from_secs(6 * 3600)) {
            return;
        }
        m.insert(slug_name.to_string(), std::time::Instant::now());
    }
    let url = engine.config.lock().unwrap().cameras.iter().find(|c| slug(&c.name) == slug_name).map(|c| c.url.clone());
    if let Some(url) = url {
        let v = format!("{} {}", chrono::Utc::now().timestamp(), url_tag(&url));
        if let Err(e) = engine.db.ui_put(PLAYED_KV, slug_name, &v) {
            warn!("camera '{slug_name}': could not note that it played: {e:#}");
        }
    }
}

fn played_value_ok(v: &str, url: &str, now_s: i64) -> bool {
    let mut it = v.split_whitespace();
    let (Some(at), Some(tag)) = (it.next().and_then(|t| t.parse::<i64>().ok()), it.next()) else { return false };
    tag == url_tag(url) && now_s - at < PLAYED_MEMORY_S
}
/// After the camera refuses the user name or password: wait this long before
/// asking again, doubling up to AUTH_RETRY_MAX. Cameras (Reolink and others)
/// lock out an address after a few wrong sign-ins, and quick retries keep the
/// lock going.
const AUTH_RETRY: Duration = Duration::from_secs(60);
const AUTH_RETRY_MAX: Duration = Duration::from_secs(600);

/// Whether a connection error means the camera refused the sign-in.
pub fn is_auth_error(e: &str) -> bool {
    let l = e.to_ascii_lowercase();
    l.contains("401 response") || l.contains("401 unauthorized") || l.contains("unauthorized") || l.contains("authentication failed") || l.contains("auth failed")
}

/// Whether a connection error means the camera did not answer at all (off,
/// rebooting, unplugged) rather than answering in a way this reader cannot
/// handle: ffmpeg would do no better.
pub fn is_unreachable(e: &str) -> bool {
    // Only failures to reach the camera at all (no TCP connection). A camera
    // that accepts the connection and then stalls in the RTSP handshake does
    // count: that is the kind ffmpeg can often still play.
    let l = e.to_ascii_lowercase();
    ["not reachable", "connection refused", "unable to connect", "no route to host", "host unreachable", "network unreachable"]
        .iter()
        .any(|k| l.contains(k))
}

/// The camera answered DESCRIBE with 404: no stream at this address (a
/// wrong path, or a stream that has not started yet). ffmpeg would get the
/// same answer, so it does not count towards switching to ffmpeg. A 404 to
/// a later request (SETUP of one track, PLAY) still counts: ffmpeg builds
/// those addresses differently and can often play such a camera.
pub fn is_not_found(e: &str) -> bool {
    e.to_ascii_lowercase().contains("404 response to describe")
}

/// The camera (or its hub) answered but said it is busy or failing for now
/// (RTSP 5xx such as 503 Service Unavailable): ffmpeg would get the same.
pub fn is_busy(e: &str) -> bool {
    let l = e.to_ascii_lowercase();
    ["500 response", "502 response", "503 response", "504 response", "service unavailable"].iter().any(|k| l.contains(k))
}

/// Light check used only during a long wait between attempts: is the
/// camera's stream port open again? A plain TCP connect, no RTSP request.
/// The wait is cut short only the first time the port goes from closed (or
/// unreachable) to open in one outage, so a hub that accepts connections
/// but keeps failing the stream is not asked more often than before.
const PROBE_MIN_WAIT: Duration = Duration::from_secs(30);
const PROBE_EVERY: Duration = Duration::from_secs(5);
const PROBE_TIMEOUT: Duration = Duration::from_secs(2);

#[derive(Default)]
struct PortWatch {
    /// The port was seen closed or unreachable during this outage.
    saw_closed: bool,
    /// The one early retry of this outage has been used.
    spent: bool,
}

impl PortWatch {
    /// The last attempt failed because the camera could not be reached.
    fn failed_unreachable(&mut self) {
        self.saw_closed = true;
    }
    /// The camera played: the next outage starts fresh.
    fn healthy(&mut self) {
        *self = PortWatch::default();
    }
    /// Result of one port check; true means retry now.
    fn on_probe(&mut self, open: bool) -> bool {
        if !open {
            self.saw_closed = true;
            false
        } else if self.saw_closed && !self.spent {
            self.spent = true;
            self.saw_closed = false;
            true
        } else {
            false
        }
    }
}

/// Whether this camera must not have its port checked between connects: not
/// RTSP, or a channel of a hub or NVR (a Reolink channel, or another configured
/// camera on the same host). A hub can wedge when connected to too often, and it
/// serves all its cameras.
fn is_hub_channel(cfg: &CameraConfig, all: &[CameraConfig]) -> bool {
    if !cfg.url.starts_with("rtsp") || cfg.reolink_channel.is_some() {
        return true;
    }
    let host = |u: &str| crate::onvif::host_from_rtsp_url(u).map(|h| h.to_ascii_lowercase());
    let mine = host(&cfg.url);
    mine.is_some() && all.iter().any(|c| c.name != cfg.name && host(&c.url) == mine)
}

/// Whether a wait of this length is long enough to be worth probing.
fn probe_during(wait: Duration) -> bool {
    wait >= PROBE_MIN_WAIT
}

fn port_open(addr: &str) -> bool {
    use std::net::ToSocketAddrs;
    addr.to_socket_addrs().ok().and_then(|mut a| a.next()).is_some_and(|a| std::net::TcpStream::connect_timeout(&a, PROBE_TIMEOUT).is_ok())
}

/// host:port a stream URL connects to (default RTSP/HTTP ports).
fn stream_addr(url: &str) -> Option<String> {
    let (scheme, rest) = url.split_once("://")?;
    let hostport = rest.split(['/', '?']).next()?;
    let hostport = hostport.rsplit_once('@').map(|x| x.1).unwrap_or(hostport);
    if hostport.is_empty() {
        return None;
    }
    let has_port = hostport.rsplit_once(':').is_some_and(|(_, p)| !p.is_empty() && p.bytes().all(|b| b.is_ascii_digit())) && !hostport.ends_with(']');
    if has_port {
        return Some(hostport.to_string());
    }
    let port = match scheme {
        "rtsp" => 554,
        "rtsps" => 322,
        "https" => 443,
        _ => 80,
    };
    Some(format!("{hostport}:{port}"))
}

/// Settings one session needs, captured when it starts.
#[derive(Clone)]
struct SessionPlan {
    main: StreamSource,
    sub: Option<StreamSource>,
    record: Option<RecordPlan>,
    with_audio: bool,
    fps: u32,
    scale: u32,
    hwaccel: Option<String>,
    seg_dir: std::path::PathBuf,
    slug: String,
    on_demand: bool,
    /// Record the sub stream as well (continuous cameras, [storage] sub_copy).
    sub_copy: bool,
}

#[derive(Clone, Copy)]
enum RecordPlan {
    Continuous { segment_seconds: u64 },
    MotionOnly { segment_seconds: u64, pre_roll: u64, post_roll: u64 },
    /// The sub stream all the time, the main stream around events.
    SubAndEvents { segment_seconds: u64, pre_roll: u64, post_roll: u64 },
}

struct SessionEnd {
    reached_play: bool,
    error: Option<String>,
    /// The stream broke on a damaged packet: reconnect at once, quietly.
    transient: bool,
}

/// A damaged packet ended the session (the RTSP library cannot continue a
/// stream after one, so the session restarts immediately instead).
#[derive(Debug)]
struct DamagedPacket(String);

impl std::fmt::Display for DamagedPacket {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "damaged packet from camera: {}", self.0)
    }
}

impl std::error::Error for DamagedPacket {}

pub(super) fn native_worker(handle: Arc<CameraHandle>, cfg: CameraConfig, engine: Arc<Engine>) {
    let slug_name = slug(&cfg.name);
    let (_, folder_root) = engine.record_folder(&cfg);
    let seg_dir = folder_root.join("cameras").join(&slug_name).join("segments");
    let clips_dir = engine.data_dir.join("clips");
    let thumbs_dir = engine.data_dir.join("thumbs");
    for d in [&seg_dir, &clips_dir, &thumbs_dir] {
        std::fs::create_dir_all(d).ok();
    }
    let mut detector = MotionDetector::new();
    let mut frames_over: u32 = 0;
    let mut event_start: Option<i64> = None;
    let mut event_score: f64 = 0.0;
    let mut best_frame: Vec<u8> = Vec::new();
    let mut pending: Vec<PendingEvent> = Vec::new();
    let mut ai_state = super::ai_check::AiState::default();
    ai_state.parked = super::ai_check::restore_parked(&engine, &cfg.name);
    let mut failures_before_play: u32 = 0;
    // Once the built-in reader has played this camera (here or before a
    // restart, at the same address), a later failure is the camera or its
    // hub (a hub answering 503 for a while), not something ffmpeg would do
    // better: it only switches after PLAYED_STUCK of failures (a firmware
    // change).
    let mut ever_played = played_recently(&engine, &slug_name, &cfg.url, chrono::Utc::now().timestamp());
    let mut busy_before_play: u32 = 0;
    let mut failing_since: Option<std::time::Instant> = None;
    let mut backoff = Duration::from_secs(2);
    let mut port_watch = PortWatch::default();
    let mut last_damaged_log: Option<std::time::Instant> = None;
    handle.state.write().unwrap().ingest = "native";

    loop {
        if !handle.active.load(Ordering::Relaxed) {
            let in_slot = engine.camera(handle.id).is_some_and(|h| Arc::ptr_eq(&h, &handle));
            if !in_slot {
                return;
            }
            thread::sleep(Duration::from_millis(300));
            continue;
        }

        // On-demand (battery) camera: stay disconnected until woken by a
        // viewer, the wake button, or the camera/hub reporting activity.
        if cfg.on_demand {
            let now = Utc::now().timestamp();
            let mut st = handle.state.write().unwrap();
            if st.camera_detections.active() {
                st.wake_until = st.wake_until.max(now + crate::config::idle_secs(cfg.idle_timeout));
            }
            if now >= st.wake_until {
                if !st.sleeping {
                    info!("camera '{}': asleep until needed", cfg.name);
                }
                st.sleeping = true;
                st.online = false;
                st.offline_since = None;
                drop(st);
                thread::sleep(Duration::from_millis(300));
                continue;
            }
            if st.sleeping {
                info!("camera '{}': waking up", cfg.name);
            }
            st.sleeping = false;
        }

        let (motion_cfg, storage) = {
            let c = engine.config.lock().unwrap();
            (c.motion.for_camera(&cfg), c.storage.clone())
        };
        let segment_seconds = storage.segment_seconds;
        // One teardown group per session: awaited inside the session's own
        // runtime so TEARDOWN reaches the camera before the runtime ends.
        let group = Arc::new(retina::client::SessionGroup::default());
        let creds = |url: &str| {
            // A battery Argus carries its UID in the URL so open_stream can find
            // the persistent session the event poller started.
            let url = match &cfg.uid {
                Some(u) if !u.is_empty() && crate::ingest::baichuan::is_baichuan_url(url) && !url.contains("uid=") => {
                    format!("{url}{}uid={u}", if url.contains('?') { '&' } else { '?' })
                }
                _ => url.to_string(),
            };
            StreamSource {
                url,
                username: cfg.username.clone(),
                password: cfg.password.clone(),
                group: Some(group.clone()),
            }
        };
        let plan = SessionPlan {
            main: creds(&cfg.url),
            sub: cfg.sub_url.as_deref().filter(|u| !u.is_empty()).map(creds),
            record: match (cfg.record, cfg.record_motion_only) {
                (false, _) => None,
                (true, false) if cfg.record_sub && cfg.sub_url.as_deref().is_some_and(|u| !u.is_empty()) => {
                    Some(RecordPlan::SubAndEvents { segment_seconds, pre_roll: motion_cfg.pre_roll, post_roll: motion_cfg.post_roll })
                }
                (true, false) => Some(RecordPlan::Continuous { segment_seconds }),
                (true, true) => Some(RecordPlan::MotionOnly {
                    segment_seconds,
                    pre_roll: motion_cfg.pre_roll,
                    post_roll: motion_cfg.post_roll,
                }),
            },
            with_audio: cfg.audio,
            fps: cfg.fps.max(1),
            scale: cfg.scale,
            hwaccel: cfg.hwaccel.clone(),
            seg_dir: seg_dir.clone(),
            slug: slug_name.clone(),
            on_demand: cfg.on_demand,
            sub_copy: storage.sub_copy,
        };

        let stop = Arc::new(AtomicBool::new(false));
        let (jpeg_tx, jpeg_rx) = mpsc::sync_channel::<Vec<u8>>(4);
        // Shared so that even a crashed session reports that it had played
        // (a crash must never look like "cannot play this camera").
        let played = Arc::new(AtomicBool::new(false));
        engine.live_sessions.fetch_add(1, Ordering::Relaxed);
        let session_began = std::time::Instant::now();
        let session = {
            let stop = stop.clone();
            let handle = handle.clone();
            let engine = engine.clone();
            let played = played.clone();
            thread::spawn(move || {
                let rt = tokio::runtime::Builder::new_current_thread().enable_all().build();
                match rt {
                    Ok(rt) => rt.block_on(run_session(plan, handle, engine, stop, jpeg_tx, played)),
                    Err(e) => SessionEnd { reached_play: false, error: Some(format!("runtime: {e}")), transient: false },
                }
            })
        };

        // Analysis loop: decoded frames drive motion, events and the live view.
        let mut idle_stop = false;
        // Pictures arrived this session (the camera really sent video).
        let mut got_video = false;
        loop {
            // Disabled, removed or shutting down: stop the session now (not
            // only when frames pause, which a busy camera never does).
            if !handle.active.load(Ordering::Relaxed) {
                stop.store(true, Ordering::Relaxed);
            }
            if cfg.on_demand && !idle_stop {
                // Viewers, an event in progress or camera-reported activity
                // keep it awake; otherwise it goes back to sleep.
                let now = Utc::now().timestamp();
                let busy = handle.live_main.viewers() + handle.live_sub.viewers() > 0 || event_start.is_some();
                let mut st = handle.state.write().unwrap();
                if busy || st.camera_detections.active() {
                    st.wake_until = st.wake_until.max(now + crate::config::idle_secs(cfg.idle_timeout));
                }
                if now >= st.wake_until {
                    idle_stop = true;
                    stop.store(true, Ordering::Relaxed);
                }
            }
            match jpeg_rx.recv_timeout(Duration::from_millis(500)) {
                Ok(jpeg) => {
                    got_video = true;
                    process_frame(
                        &handle, &engine, &cfg, &mut detector, jpeg, segment_seconds,
                        &mut frames_over, &mut event_start, &mut event_score,
                        &mut best_frame, &mut pending, &mut ai_state,
                        &slug_name, &seg_dir, &clips_dir, &thumbs_dir,
                    );
                }
                Err(mpsc::RecvTimeoutError::Timeout) => {
                    if !handle.active.load(Ordering::Relaxed) {
                        stop.store(true, Ordering::Relaxed);
                    }
                }
                Err(mpsc::RecvTimeoutError::Disconnected) => break,
            }
        }
        let end = session.join().unwrap_or(SessionEnd {
            reached_play: played.load(Ordering::Relaxed),
            error: Some("session thread panicked".into()),
            transient: false,
        });
        engine.live_sessions.fetch_sub(1, Ordering::Relaxed);
        {
            let mut st = handle.state.write().unwrap();
            st.online = false;
            st.motion = false;
            st.online_since = None;
            // Keep the time it first went offline across retries.
            st.offline_since.get_or_insert(Utc::now().timestamp());
            if handle.active.load(Ordering::Relaxed) && !idle_stop {
                st.reconnects += 1;
                st.last_error = Some(end.error.clone().unwrap_or_else(|| "stream ended".into()));
                st.last_error_at = Some(Utc::now().timestamp());
            }
            if idle_stop {
                st.offline_since = None;
            }
        }
        if !handle.active.load(Ordering::Relaxed) || idle_stop {
            continue; // disabled, replaced, or gone back to sleep
        }
        if end.transient && end.reached_play {
            // Damaged packet: reconnect at once; log at most once a minute.
            if last_damaged_log.is_none_or(|t: std::time::Instant| t.elapsed() > Duration::from_secs(60)) {
                warn!("camera '{}': {}; reconnected", cfg.name, end.error.as_deref().unwrap_or("damaged packet"));
                last_damaged_log = Some(std::time::Instant::now());
            }
            failures_before_play = 0;
            backoff = Duration::from_secs(2);
            thread::sleep(Duration::from_millis(200));
            continue;
        }
        if end.reached_play && cfg.on_demand && end.error.as_deref().is_some_and(|e| e.starts_with(NO_PICTURE_AFTER_WAKE)) {
            // Woke but stayed silent. A hub relaying a battery camera does
            // this about half the time; asking again works, but not at once
            // (immediate retries tend to fail too), so pause first.
            info!("camera '{}': no picture after waking; asking again in {} s", cfg.name, WAKE_RETRY.as_secs());
            failures_before_play = 0;
            backoff = Duration::from_secs(2);
            let until = std::time::Instant::now() + WAKE_RETRY;
            while std::time::Instant::now() < until && handle.active.load(Ordering::Relaxed) {
                thread::sleep(Duration::from_millis(200));
            }
            continue;
        }
        let why = end.error.unwrap_or_else(|| "stream ended".into());
        // A session that sent video, or played for a while, starts the waits
        // over. One that ended soon after PLAY with no video counts as a
        // failure: the waits keep growing, so a hub that accepts and then
        // sends nothing is not asked again every few seconds (that can wedge
        // it until it is power-cycled).
        let lasted = session_began.elapsed() >= Duration::from_secs(60);
        if end.reached_play && (lasted || got_video) {
            ever_played = true;
            failures_before_play = 0;
            busy_before_play = 0;
            failing_since = None;
            backoff = Duration::from_secs(2);
            port_watch.healthy();
        } else if end.reached_play {
            failures_before_play = 0;
            busy_before_play = 0;
            failing_since = None;
        } else if is_busy(&why) {
            busy_before_play += 1;
            if ever_played {
                // Busy or off is the camera or its hub, not the reader:
                // the 30 minutes start over.
                failures_before_play = 0;
                failing_since = None;
            }
        } else if is_unreachable(&why) || is_not_found(&why) {
            if ever_played {
                failures_before_play = 0;
                failing_since = None;
            }
        } else {
            // Only a camera that answers but cannot be played counts towards
            // switching to ffmpeg; one that is off, rebooting or has no stream
            // at that path (yet) does not.
            failures_before_play += 1;
            failing_since.get_or_insert_with(std::time::Instant::now);
        }
        let gave_up = if ever_played {
            failures_before_play >= FALLBACK_AFTER && failing_since.is_some_and(|t| t.elapsed() >= PLAYED_STUCK)
        } else {
            failures_before_play >= FALLBACK_AFTER || busy_before_play >= BUSY_FALLBACK_AFTER
        };
        // Wrong user name or password: ffmpeg would be refused too, and quick
        // retries lock the camera. Say so and wait long between tries (a
        // settings change replaces this worker, so a fix applies at once).
        if !end.reached_play && is_auth_error(&why) {
            backoff = backoff.max(AUTH_RETRY);
            let msg = "the camera refused the user name or password (check them in the camera's settings)";
            warn!("camera '{}': {msg} ({why}); asking again in {} min", cfg.name, backoff.as_secs() / 60);
            {
                let mut st = handle.state.write().unwrap();
                st.last_error = Some(msg.to_string());
                st.last_error_at = Some(Utc::now().timestamp());
            }
            let until = std::time::Instant::now() + backoff;
            while std::time::Instant::now() < until && handle.active.load(Ordering::Relaxed) {
                thread::sleep(Duration::from_millis(500));
            }
            backoff = (backoff * 2).min(AUTH_RETRY_MAX);
            continue;
        }
        // (HTTP-FLV streams have no ffmpeg fallback: their sign-in is in
        // the query, which the ffmpeg pipeline does not add. Battery Argus
        // (baichuan://) has no ffmpeg fallback either: ffmpeg cannot open it,
        // and a camera that will not preview — e.g. older firmware — must keep
        // retrying the native client, not spin on ffmpeg.)
        if !end.reached_play && gave_up && cfg.ingest != "native" && !crate::ingest::flv::is_flv_url(&cfg.url) && !crate::ingest::push::is_push_url(&cfg.url) && !crate::ingest::baichuan::is_baichuan_url(&cfg.url) {
            warn!(
                "camera '{}': built-in RTSP reader could not play this camera ({why}); switching to ffmpeg ingest",
                cfg.name
            );
            handle.state.write().unwrap().ingest = "ffmpeg (fallback)";
            camera_worker(handle, cfg, engine);
            return;
        }
        // (A long wait left from a refused sign-in does not apply here.)
        backoff = backoff.min(RETRY_MAX);
        warn!("camera '{}': {why}; reconnecting in {} s", cfg.name, backoff.as_secs());
        if !end.reached_play && is_unreachable(&why) {
            port_watch.failed_unreachable();
        }
        let until = std::time::Instant::now() + backoff;
        let probe_addr = if !port_watch.spent && probe_during(backoff) && !is_hub_channel(&cfg, &engine.config.lock().unwrap().cameras) { stream_addr(&cfg.url) } else { None };
        let mut next_probe = std::time::Instant::now() + PROBE_EVERY;
        while std::time::Instant::now() < until && handle.active.load(Ordering::Relaxed) {
            thread::sleep(Duration::from_millis(250));
            if let Some(addr) = &probe_addr {
                if std::time::Instant::now() >= next_probe {
                    if port_watch.on_probe(port_open(addr)) {
                        info!("camera '{}': answering again; reconnecting now", cfg.name);
                        break;
                    }
                    next_probe = std::time::Instant::now() + PROBE_EVERY;
                }
            }
        }
        backoff = (backoff * 2).min(RETRY_MAX);
    }
}

/// Run the camera's stream session(s) until an error, a stall, or `stop`.
async fn run_session(
    plan: SessionPlan,
    handle: Arc<CameraHandle>,
    engine: Arc<Engine>,
    stop: Arc<AtomicBool>,
    jpeg_tx: mpsc::SyncSender<Vec<u8>>,
    reached_play: Arc<AtomicBool>,
) -> SessionEnd {
    let main_decodes = plan.sub.is_none();
    // (A shared main stream needs its session even when nothing records it.)
    let main_needed = plan.record.is_some() || main_decodes || handle.restream_main.wanted();

    let main = async {
        if !main_needed {
            return std::future::pending::<anyhow::Result<()>>().await;
        }
        let tx = main_decodes.then(|| jpeg_tx.clone());
        main_task(&plan, &handle, &engine, &stop, &reached_play, tx).await
    };
    let sub = async {
        match &plan.sub {
            None => std::future::pending::<anyhow::Result<()>>().await,
            Some(src) => decode_only_task(&plan, src, &handle, &engine, &stop, &reached_play, jpeg_tx.clone()).await,
        }
    };
    // Whichever side ends first ends the session; the worker reconnects both.
    let result = tokio::select! {
        r = main => r,
        r = sub => r,
    };
    // Let the camera free the session now (TEARDOWN), not after a timeout.
    if let Some(g) = &plan.main.group {
        let _ = tokio::time::timeout(Duration::from_secs(3), g.await_teardown()).await;
    }
    let transient = result.as_ref().err().is_some_and(|e| e.downcast_ref::<DamagedPacket>().is_some());
    SessionEnd { reached_play: reached_play.load(Ordering::Relaxed), error: result.err().map(|e| format!("{e:#}")), transient }
}

enum Sink {
    Continuous(SegmentWriter),
    MotionOnly(TriggeredRecorder),
}

impl Sink {
    /// Audio for a continuous recording (the sub stream's).
    fn push_audio(&mut self, ticks: i64, data: Vec<u8>, engine: &Arc<Engine>, slug: &str) {
        if let Sink::Continuous(w) = self {
            w.push_audio(ticks, data);
            for seg in w.take_closed() {
                engine.segment_closed(slug, seg);
            }
        }
    }

    /// `n`: 0 = video, 1 = audio.
    fn observe_sender_report(&mut self, n: usize, (ticks, ntp): (i64, f64)) {
        use crate::ingest::fmp4::TrackKind;
        let kind = if n == 0 { TrackKind::Video } else { TrackKind::Audio };
        match self {
            Sink::Continuous(w) => w.observe_sender_report(kind, ticks, ntp),
            Sink::MotionOnly(r) => r.observe_sender_report(kind, ticks, ntp),
        }
    }
}

/// Main stream: record it, and decode it too when there is no sub stream.
async fn main_task(
    plan: &SessionPlan,
    handle: &Arc<CameraHandle>,
    engine: &Arc<Engine>,
    stop: &AtomicBool,
    reached_play: &AtomicBool,
    jpeg_tx: Option<mpsc::SyncSender<Vec<u8>>>,
) -> anyhow::Result<()> {
    // Set up every track (some hubs stream nothing otherwise); audio is
    // only recorded / forwarded when the camera has audio enabled.
    let mut s = connect(&plan.main, true).await?;
    s.use_audio = plan.with_audio;
    // The stream analysed for motion is also listened to (loud-sound trigger).
    let mut sound = jpeg_tx.as_ref().map(|_| {
        s.tap_audio = true;
        super::sound::SoundWatch::new(handle, &handle.running.read().unwrap())
    });
    if plan.on_demand {
        s.stall_limit = WAKE_STALL;
    }
    reached_play.store(true, Ordering::Relaxed);
    info!("camera '{}': native stream playing ({})", handle.state.read().unwrap().name, redact_url(plan.main.url.clone()));
    let mut sink: Option<Sink> = None;
    let mut decoder: Option<FrameDecoder> = None;
    let mut seen_drops = 0u64;
    // Latest RTCP sender report per track (video, audio): (rtp ticks, NTP
    // epoch seconds). They align audio with video in the recordings; kept
    // here so a recorder created later starts aligned.
    let mut latest_sr: [Option<(i64, f64)>; 2] = [None, None];
    let mut live: Option<LiveMuxer> = None;
    let shared = handle.restream_main.clone();
    let _stop_sharing = crate::restream::hub::StopOnDrop(shared.clone());
    while let Some(item) = next_item(&mut s, stop).await? {
        match item {
            MediaItem::SenderReport { track: n, ticks, epoch_secs } => {
                let report = (ticks, epoch_secs);
                latest_sr[n] = Some(report);
                if n == 0 {
                    // A sender report describes "now" on the camera's clock.
                    let local = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs_f64()).unwrap_or(0.0);
                    handle.state.write().unwrap().clock_skew = Some(report.1 - local);
                }
                if let Some(k) = sink.as_mut() {
                    k.observe_sender_report(n, report);
                }
            }
            MediaItem::Video { ticks, cts, data, key, new_params } => {
                handle.rx_frames.fetch_add(1, Ordering::Relaxed);
                handle.rx_bytes.fetch_add(data.len() as u64, Ordering::Relaxed);
                if new_params {
                    // New resolution/codec: restart the file and the decoder.
                    if let Some(mut old) = sink.take() {
                        finish_sink(&mut old, engine, &plan.slug);
                    }
                    decoder = None;
                    live = None;
                    shared.stop();
                }
                if live.is_none() && key {
                    live = start_live(&s, &handle.live_main, &plan.slug)?;
                }
                if let Some(l) = live.as_mut() {
                    l.push_video(ticks, cts, &data, key);
                }
                share_video(&s, &shared, plan.with_audio, ticks + i64::from(cts), key, &data);
                if let Some(tx) = &jpeg_tx {
                    if decoder.is_none() && key {
                        decoder = start_decoder(&s, plan)?;
                        seen_drops = 0;
                    }
                    if let Some(d) = decoder.as_mut() {
                        let _ = d.push(&data, key);
                        note_drops(&handle, d, &mut seen_drops);
                        forward_jpegs(d, tx);
                    }
                }
                if sink.is_none() && key {
                    if let (Some(rec), Some(tracks)) = (plan.record, build_tracks(&s)?) {
                        let timescale = tracks[0].timescale;
                        let run = format!("{}-{}", plan.slug, Utc::now().timestamp_millis());
                        sink = Some(match rec {
                            RecordPlan::Continuous { segment_seconds } => {
                                Sink::Continuous(SegmentWriter::new(&plan.seg_dir, &run, tracks, segment_seconds)?)
                            }
                            RecordPlan::MotionOnly { segment_seconds, pre_roll, post_roll }
                            | RecordPlan::SubAndEvents { segment_seconds, pre_roll, post_roll } => Sink::MotionOnly(
                                TriggeredRecorder::new(plan.seg_dir.clone(), &plan.slug, tracks, segment_seconds, pre_roll, post_roll, timescale),
                            ),
                        });
                        for (n, report) in latest_sr.iter().enumerate() {
                            if let (Some(k), Some(r)) = (sink.as_mut(), report) {
                                k.observe_sender_report(n, *r);
                            }
                        }
                    }
                }
                if let Some(k) = sink.as_mut() {
                    let frame = Frame::Video { ticks, cts, data, is_key: key };
                    push_sink(k, frame, handle, engine, &plan.slug)?;
                }
            }
            MediaItem::Audio { ticks, data } => {
                if let Some(w) = sound.as_mut() {
                    w.push(&s, &data);
                }
                // Heard only for the sound trigger: not recorded or played.
                if !plan.with_audio {
                    continue;
                }
                if let Some(l) = live.as_mut() {
                    l.push_audio(ticks, &data);
                }
                if shared.wanted() {
                    shared.publish_audio(ticks, &data);
                }
                if let Some(k) = sink.as_mut() {
                    push_sink(k, Frame::Audio { ticks, data }, handle, engine, &plan.slug)?;
                }
            }
        }
    }
    if let Some(mut k) = sink.take() {
        finish_sink(&mut k, engine, &plan.slug);
    }
    if let Some(l) = live.take() {
        l.finish();
    }
    Ok(())
}

/// Start live view for a stream once its codec parameters are known.
fn start_live(s: &OpenStream, hub: &Arc<crate::ingest::live::LiveHub>, name: &str) -> anyhow::Result<Option<LiveMuxer>> {
    let Some(tracks) = build_tracks(s)? else { return Ok(None) };
    let mut codecs = Vec::new();
    if let Some(v) = s.video_params()? {
        codecs.push(v.codec);
    }
    if tracks.len() > 1 {
        if let Some(a) = s.audio_params() {
            codecs.push(a.codec);
        }
    }
    Ok(Some(LiveMuxer::start(hub.clone(), &tracks, &codecs).with_name(name)))
}

/// Share a video frame over RTSP (see restream/): the stream is described to
/// the hub at its first keyframe, then every frame is published. Nothing
/// happens while the stream is not shared.
fn share_video(s: &OpenStream, hub: &Arc<crate::restream::hub::RestreamHub>, with_audio: bool, pts: i64, key: bool, data: &[u8]) {
    if !hub.wanted() {
        return;
    }
    if key && hub.info().is_none() {
        let video = s.video_params().ok().flatten().and_then(|v| crate::restream::hub::video_info(v.h265, &v.extra_data, v.clock_rate, v.width, v.height));
        let Some(video) = video else { return };
        // AAC only: G.711 is turned into FLAC on the way in, which RTSP
        // players do not expect.
        let audio = match s.audio_codec() {
            Some(crate::sound::AudioCodec::Aac { config }) if with_audio => crate::restream::hub::audio_info(&config),
            _ => None,
        };
        hub.start(video, audio);
    }
    hub.publish_video(pts, key, data);
}

/// Sub stream: decode it, and record it when the camera records the sub
/// stream between events.
async fn decode_only_task(
    plan: &SessionPlan,
    src: &StreamSource,
    handle: &Arc<CameraHandle>,
    engine: &Arc<Engine>,
    stop: &AtomicBool,
    reached_play: &AtomicBool,
    jpeg_tx: mpsc::SyncSender<Vec<u8>>,
) -> anyhow::Result<()> {
    let mut s = connect(src, true).await?;
    // Audio rides along on the sub-stream live view and recording, and is
    // listened to for the loud-sound trigger.
    s.use_audio = plan.with_audio;
    s.tap_audio = true;
    let mut sound = super::sound::SoundWatch::new(handle, &handle.running.read().unwrap());
    if plan.on_demand {
        s.stall_limit = WAKE_STALL;
    }
    reached_play.store(true, Ordering::Relaxed);
    let segment_seconds = match plan.record {
        Some(RecordPlan::SubAndEvents { segment_seconds, .. }) => Some(segment_seconds),
        Some(RecordPlan::Continuous { segment_seconds }) if plan.sub_copy => Some(segment_seconds),
        _ => None,
    };
    let mut decoder: Option<FrameDecoder> = None;
    let mut seen_drops = 0u64;
    let mut live: Option<LiveMuxer> = None;
    let mut sink: Option<Sink> = None;
    let shared = handle.restream_sub.clone();
    let _stop_sharing = crate::restream::hub::StopOnDrop(shared.clone());
    let mut sub_failed: Option<std::time::Instant> = None;
    let mut latest_sr: [Option<(i64, f64)>; 2] = [None, None];
    while let Some(item) = next_item(&mut s, stop).await? {
        if let MediaItem::SenderReport { track: n, ticks, epoch_secs } = item {
            latest_sr[n] = Some((ticks, epoch_secs));
            if let Some(k) = sink.as_mut() {
                k.observe_sender_report(n, (ticks, epoch_secs));
            }
            continue;
        }
        if let MediaItem::Audio { ticks, data } = item {
            sound.push(&s, &data);
            if !plan.with_audio {
                continue;
            }
            if let Some(l) = live.as_mut() {
                l.push_audio(ticks, &data);
            }
            if shared.wanted() {
                shared.publish_audio(ticks, &data);
            }
            if let Some(k) = sink.as_mut() {
                k.push_audio(ticks, data, engine, &plan.slug);
            }
            continue;
        }
        if let MediaItem::Video { ticks, cts, data, key, new_params } = item {
            if new_params {
                decoder = None;
                live = None;
                shared.stop();
                if let Some(mut old) = sink.take() {
                    finish_sink(&mut old, engine, &plan.slug);
                }
            }
            if live.is_none() && key {
                live = start_live(&s, &handle.live_sub, &plan.slug)?;
            }
            if let Some(l) = live.as_mut() {
                l.push_video(ticks, cts, &data, key);
            }
            share_video(&s, &shared, plan.with_audio, ticks + i64::from(cts), key, &data);
            if decoder.is_none() && key {
                decoder = start_decoder(&s, plan)?;
                seen_drops = 0;
            }
            if let Some(d) = decoder.as_mut() {
                let _ = d.push(&data, key);
                note_drops(&handle, d, &mut seen_drops);
                forward_jpegs(d, &jpeg_tx);
            }
            // The sub-stream recording is extra: if writing it fails, only it
            // stops (for a minute), never the session and its main recording.
            let retry_ok = sub_failed.is_none_or(|t: std::time::Instant| t.elapsed() > Duration::from_secs(60));
            if let (None, Some(secs), true, true) = (&sink, segment_seconds, key, retry_ok) {
                if let Some(tracks) = build_tracks(&s)? {
                    // `{camera}-sub-{ms}-seg000000.mp4`: indexed as kind "sub".
                    let run = format!("{}-sub-{}", plan.slug, Utc::now().timestamp_millis());
                    match SegmentWriter::new(&plan.seg_dir, &run, tracks, secs) {
                        Ok(w) => {
                            let mut k = Sink::Continuous(w);
                            for (n, report) in latest_sr.iter().enumerate() {
                                if let Some(r) = report {
                                    k.observe_sender_report(n, *r);
                                }
                            }
                            sink = Some(k);
                        }
                        Err(e) => {
                            warn!("camera '{}': cannot record the sub stream: {e}", plan.slug);
                            sub_failed = Some(std::time::Instant::now());
                        }
                    }
                }
            }
            if let Some(Sink::Continuous(w)) = sink.as_mut() {
                match w.push_video_cts(ticks, cts, data, key) {
                    Ok(()) => {
                        for seg in w.take_closed() {
                            engine.segment_closed(&plan.slug, seg);
                        }
                    }
                    Err(e) => {
                        warn!("camera '{}': sub stream recording stopped: {e}", plan.slug);
                        if let Some(mut old) = sink.take() {
                            finish_sink(&mut old, engine, &plan.slug);
                        }
                        sub_failed = Some(std::time::Instant::now());
                    }
                }
            }
        }
    }
    if let Some(mut k) = sink.take() {
        finish_sink(&mut k, engine, &plan.slug);
    }
    Ok(())
}

/// Some cameras never answer a request for a stream they do not have (or
/// stall half-way through the handshake): give up after this long so the
/// worker can retry or fall back to ffmpeg.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(15);

async fn connect(src: &StreamSource, with_audio: bool) -> anyhow::Result<OpenStream> {
    match tokio::time::timeout(CONNECT_TIMEOUT, open_stream(src, with_audio)).await {
        Ok(r) => r,
        Err(_) => {
            // Silent: off (or rebooting), or answering TCP but stuck in RTSP?
            let reachable = match stream_addr(&src.url) {
                Some(a) => matches!(tokio::time::timeout(Duration::from_secs(3), tokio::net::TcpStream::connect(a)).await, Ok(Ok(_))),
                None => true,
            };
            Err(anyhow::anyhow!(
                "camera {} within {} s ({})",
                if reachable { "did not answer" } else { "not reachable" },
                CONNECT_TIMEOUT.as_secs(),
                redact_url(src.url.clone())
            ))
        }
    }
}

/// Errors about one RTP packet (bad NAL header, truncated fragment) rather
/// than the connection: retina reports them with the packet's ssrc/seq.
fn is_packet_error(msg: &str) -> bool {
    msg.contains("\nssrc: ") && msg.contains("\nseq: ")
}

/// Next frame, or None when asked to stop. Errors on stream failure or stall.
async fn next_item(s: &mut OpenStream, stop: &AtomicBool) -> anyhow::Result<Option<MediaItem>> {
    loop {
        if stop.load(Ordering::Relaxed) {
            return Ok(None);
        }
        // Only video frames prove the stream is alive (a stuck device may
        // keep sending RTCP reports, or audio, with no picture).
        if s.last_video.elapsed() >= s.stall_limit {
            if s.stall_limit == WAKE_STALL {
                return Err(anyhow::anyhow!("{NO_PICTURE_AFTER_WAKE} within {} s", WAKE_STALL.as_secs()));
            }
            return Err(anyhow::anyhow!("no video from camera for {} s", s.stall_limit.as_secs()));
        }
        match tokio::time::timeout(Duration::from_millis(500), s.next()).await {
            Ok(Some(Ok(item))) => {
                if matches!(&item, MediaItem::Video { .. }) {
                    // Awake and streaming: normal stall detection from now on.
                    s.stall_limit = STALL;
                    s.last_video = std::time::Instant::now();
                }
                return Ok(Some(item));
            }
            Ok(Some(Err(e))) => {
                let msg = e.to_string();
                // A single damaged packet (common over WiFi or through a hub)
                // is skipped; the picture recovers at the next keyframe.
                // Only a sustained burst means the stream is broken.
                if is_packet_error(&msg) {
                    return Err(anyhow::Error::new(DamagedPacket(msg.lines().next().unwrap_or("").to_string())));
                }
                return Err(anyhow::anyhow!("stream error: {e}"));
            }
            Ok(None) => return Err(anyhow::anyhow!("camera closed the stream")),
            Err(_) => {}
        }
    }
}

fn start_decoder(s: &OpenStream, plan: &SessionPlan) -> anyhow::Result<Option<FrameDecoder>> {
    let Some(v) = s.video_params()? else { return Ok(None) };
    let codec = if v.h265 { Codec::H265 } else { Codec::H264 };
    Ok(Some(FrameDecoder::start(codec, &v.extra_data, plan.fps, plan.scale, plan.hwaccel.clone())?))
}

/// Add the decoder's new dropped frames to the camera's counter (status panel).
fn note_drops(handle: &CameraHandle, d: &FrameDecoder, seen: &mut u64) {
    handle.dropped.fetch_add(new_drops(&d.stats(), seen), Ordering::Relaxed);
}

/// Real drops since the last reading (not the frames before a new decoder's
/// first keyframe). The caller resets `seen` to 0 whenever it starts a
/// replacement decoder.
fn new_drops(st: &crate::ingest::decode::DecoderStats, seen: &mut u64) -> u64 {
    let now = st.real_dropped + st.jpegs_dropped;
    let add = now.saturating_sub(*seen);
    *seen = now;
    add
}

fn forward_jpegs(d: &FrameDecoder, tx: &mpsc::SyncSender<Vec<u8>>) {
    for jpeg in d.frames().try_iter() {
        // If analysis is behind, skip frames rather than queue them up.
        let _ = tx.try_send(jpeg);
    }
}

fn push_sink(sink: &mut Sink, f: Frame, handle: &Arc<CameraHandle>, engine: &Arc<Engine>, slug: &str) -> anyhow::Result<()> {
    let t = std::time::Instant::now();
    let closed = match sink {
        Sink::Continuous(w) => {
            match f {
                Frame::Video { ticks, cts, data, is_key } => w.push_video_cts(ticks, cts, data, is_key)?,
                Frame::Audio { ticks, data } => w.push_audio(ticks, data),
            }
            w.take_closed()
        }
        Sink::MotionOnly(r) => {
            // Motion (or a manual clip) keeps extending the recording.
            if matches!(f, Frame::Video { .. }) && handle.state.read().unwrap().motion {
                r.trigger()?;
            }
            r.push(f)?;
            r.take_closed()
        }
    };
    // The camera is not being read meanwhile: say so when the disk is slow.
    let took = t.elapsed();
    if took > Duration::from_millis(500) {
        tracing::info!("camera '{slug}': writing the recording took {} ms", took.as_millis());
    }
    for seg in closed {
        engine.segment_closed(slug, seg);
    }
    Ok(())
}

/// Closed recordings queued for the indexer and not yet in the database.
static INDEX_PENDING: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

fn finish_sink(sink: &mut Sink, engine: &Arc<Engine>, slug: &str) {
    let closed = match sink {
        Sink::Continuous(w) => w.finish().map(|_| w.take_closed()),
        Sink::MotionOnly(r) => r.stop().map(|_| r.take_closed()),
    };
    match closed {
        Ok(segs) => {
            for seg in segs {
                engine.segment_closed(slug, seg);
            }
        }
        Err(e) => warn!("camera '{slug}': could not finish recording: {e}"),
    }
}

/// Index kind of a native recording file, from its name: motion-only and
/// around-event recordings `{camera}-{ms}-rec-seg…`, the sub stream
/// `{camera}-sub-{ms}-seg…`, otherwise continuous.
fn segment_kind(camera_slug: &str, path: &std::path::Path) -> &'static str {
    let name = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    if name.contains("-rec-seg") {
        "triggered"
    } else if name.starts_with(&format!("{camera_slug}-sub-")) {
        "sub"
    } else {
        "continuous"
    }
}

impl Engine {
    /// A recording segment file was completed: index it in the background,
    /// in order. The camera's reading thread must never wait here: the
    /// database (and the config lock `locate` takes) can be busy for
    /// seconds, and a camera that is not read for ~2 s can cut packets
    /// short (Reolink over TCP on Windows), putting the stream out of step.
    /// Wait (up to `wait`) until every closed recording handed to the
    /// indexer is in the database: at shutdown, so the last file of each
    /// camera is not left out of it.
    pub(super) fn wait_for_indexing(&self, wait: Duration) -> usize {
        let until = std::time::Instant::now() + wait;
        loop {
            let left = INDEX_PENDING.load(std::sync::atomic::Ordering::SeqCst);
            if left == 0 || std::time::Instant::now() >= until {
                return left;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
    }

    pub(super) fn segment_closed(self: &Arc<Self>, camera_slug: &str, seg: SegmentInfo) {
        let engine = self.clone();
        let slug = camera_slug.to_string();
        if cfg!(test) {
            engine.index_segment(&slug, seg);
            return;
        }
        INDEX_PENDING.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        let job = move || {
            note_played(&engine, &slug);
            engine.index_segment(&slug, seg);
            INDEX_PENDING.fetch_sub(1, std::sync::atomic::Ordering::SeqCst);
        };
        static TX: std::sync::OnceLock<std::sync::Mutex<std::sync::mpsc::Sender<Box<dyn FnOnce() + Send>>>> =
            std::sync::OnceLock::new();
        let tx = TX.get_or_init(|| {
            let (tx, rx) = std::sync::mpsc::channel::<Box<dyn FnOnce() + Send>>();
            let spawned = std::thread::Builder::new().name("segment-index".into()).spawn(move || {
                for job in rx {
                    job();
                }
            });
            if let Err(e) = spawned {
                warn!("could not start the recording indexer: {e}");
            }
            std::sync::Mutex::new(tx)
        });
        if let Err(std::sync::mpsc::SendError(job)) = tx.lock().unwrap().send(Box::new(job)) {
            job(); // indexer gone: do it here rather than lose the recording
        }
    }

    fn index_segment(&self, camera_slug: &str, seg: SegmentInfo) {
        let (folder, path) = self.locate(&seg.path);
        let row = crate::db::SegmentRow {
            path,
            folder,
            start_ms: seg.start_ms,
            end_ms: seg.end_ms,
            bytes: seg.bytes as i64,
            video_frames: seg.video_frames as i64,
            kind: segment_kind(camera_slug, &seg.path).into(),
        };
        let t = std::time::Instant::now();
        if let Err(e) = self.db.insert_segment(camera_slug, &row) {
            warn!("could not index recording {}: {e:#}", row.path);
        }
        let took = t.elapsed();
        if took > Duration::from_millis(500) {
            tracing::info!("indexing a recording of '{camera_slug}' waited {} ms for the database", took.as_millis());
        }
        crate::ingest::splice::remember(&seg.path);
        tracing::debug!(
            "segment closed: {camera_slug} {} ({} ms, {} bytes)",
            seg.path.display(),
            seg.end_ms - seg.start_ms,
            seg.bytes
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn drops_ignore_pre_keyframe_frames_and_new_decoders() {
        use crate::ingest::decode::DecoderStats;
        let mut seen = 0;
        // 50 frames waited for the first keyframe: not a drop.
        let st = DecoderStats { frames_dropped: 50, ..Default::default() };
        assert_eq!(new_drops(&st, &mut seen), 0);
        let st = DecoderStats { frames_dropped: 52, real_dropped: 2, jpegs_dropped: 1, ..Default::default() };
        assert_eq!(new_drops(&st, &mut seen), 3);
        assert_eq!(new_drops(&st, &mut seen), 0);
        // A replacement decoder (caller resets the baseline) with a higher count still counts.
        seen = 0;
        let st = DecoderStats { real_dropped: 5, ..Default::default() };
        assert_eq!(new_drops(&st, &mut seen), 5);
    }

    #[test]
    fn port_watch_skips_the_wait_once_after_seeing_it_closed() {
        let mut w = PortWatch::default();
        // Open from the start (a hub that accepts and then fails): never skips.
        assert!(!w.on_probe(true));
        assert!(!w.on_probe(true));
        // Seen closed, then open: skip once.
        assert!(!w.on_probe(false));
        assert!(w.on_probe(true));
        // Closed and open again in the same outage: normal backoff.
        assert!(!w.on_probe(false));
        assert!(!w.on_probe(true));
        // A healthy session starts a new outage.
        w.healthy();
        assert!(!w.on_probe(true));
        w.failed_unreachable();
        assert!(w.on_probe(true));
        assert!(!w.on_probe(true));
    }

    #[test]
    fn no_port_checks_on_a_hub_or_nvr() {
        let cam = |name: &str, url: &str| -> CameraConfig { toml::from_str(&format!("name = \"{name}\"\nurl = \"{url}\"\n")).unwrap() };
        let a = cam("A", "rtsp://u:p@10.0.0.5:554/ch1");
        let b = cam("B", "rtsp://10.0.0.5/ch2");
        let c = cam("C", "rtsp://10.0.0.6/live");
        let up = cam("Upper", "rtsp://CAM.LAN/1");
        let low = cam("Lower", "rtsp://cam.lan/2");
        assert!(is_hub_channel(&a, &[a.clone(), b.clone(), c.clone()]), "same host as B");
        assert!(!is_hub_channel(&c, &[a.clone(), b.clone(), c.clone()]));
        assert!(!is_hub_channel(&a, &[a.clone(), c.clone()]), "only itself on that host");
        assert!(is_hub_channel(&up, &[up.clone(), low.clone()]));
        let mut r = cam("R", "rtsp://10.0.0.7/x");
        r.reolink_channel = Some(0);
        assert!(is_hub_channel(&r, &[r.clone()]));
        assert!(is_hub_channel(&cam("F", "http://10.0.0.8/x"), &[]), "not RTSP");
    }

    #[test]
    fn probing_only_in_long_waits() {
        assert!(!probe_during(Duration::from_secs(16)));
        assert!(probe_during(Duration::from_secs(30)));
        assert!(probe_during(RETRY_MAX));
    }


    #[test]
    fn segment_kinds_come_from_file_names() {
        use std::path::Path;
        assert_eq!(segment_kind("porch", Path::new("/r/porch-1700000000000-seg000003.mp4")), "continuous");
        assert_eq!(segment_kind("porch", Path::new("/r/porch-1700000000000-rec-seg000000.mp4")), "triggered");
        let now = 1_800_000_000;
        let v = format!("{} {}", now - 3600, url_tag("rtsp://a/1"));
        assert!(played_value_ok(&v, "rtsp://a/1", now));
        assert!(!played_value_ok(&v, "rtsp://b/1", now), "another camera address");
        assert!(!played_value_ok(&v, "rtsp://a/1", now + PLAYED_MEMORY_S), "too long ago");
        assert!(!played_value_ok("junk", "rtsp://a/1", now));
        assert!(is_busy("DESCRIBE failed: 503 response to DESCRIBE CSeq=3: Unexpected RTSP response status"));
        assert!(!is_busy("DESCRIBE failed: 404 response to DESCRIBE CSeq=3"));
        assert_eq!(segment_kind("porch", Path::new("/r/porch-sub-1700000000000-seg000001.mp4")), "sub");
        // Another camera's prefix that merely contains "-sub-" is not the sub stream.
        assert_eq!(segment_kind("porch", Path::new("/r/porch2-sub-1-seg000001.mp4")), "continuous");
    }

    #[test]
    fn unreachable_cameras_are_recognized() {
        assert!(is_unreachable("camera not reachable within 15 s (rtsp://192.0.2.10:554/stream)"));
        assert!(is_unreachable("Unable to connect to RTSP server: connection refused"));
        // Answered TCP but stalled in RTSP: counts towards trying ffmpeg.
        assert!(!is_unreachable("camera did not answer within 15 s (rtsp://192.0.2.10:554/stream)"));
        assert!(!is_unreachable("DESCRIBE failed: 454 Session Not Found"));
        assert!(!is_unreachable("stream ended"));
        // No stream at this path (yet): retry the built-in reader, not ffmpeg.
        assert!(is_not_found("DESCRIBE failed: 404 response to DESCRIBE CSeq=2: Unexpected RTSP response status"));
        assert!(!is_not_found("SETUP failed: 404 response to SETUP CSeq=4: Unexpected RTSP response status"));
        assert!(!is_not_found("PLAY failed: 404 response to PLAY CSeq=5"));
        assert_eq!(crate::config::idle_secs(0), 30);
        assert_eq!(crate::config::idle_secs(300), 300);
        assert_eq!(crate::config::idle_secs(u64::MAX), 7 * 86400, "no overflow from a huge timeout");
        assert!(!is_not_found("DESCRIBE failed: 454 Session Not Found"));
        assert!(!is_not_found("DESCRIBE failed: 503 response to DESCRIBE CSeq=3"));
        assert_eq!(stream_addr("rtsp://u:p@192.0.2.10/stream").as_deref(), Some("192.0.2.10:554"));
        assert_eq!(stream_addr("rtsp://192.0.2.10:8554/a?b=c").as_deref(), Some("192.0.2.10:8554"));
        assert_eq!(stream_addr("https://cam.example/flv?x=1").as_deref(), Some("cam.example:443"));
    }

    #[test]
    fn sign_in_refusals_are_recognized() {
        assert!(is_auth_error("DESCRIBE failed: 401 response to DESCRIBE CSeq=3: Received Unauthorized after trying digest auth"));
        assert!(!is_auth_error("camera did not answer within 15 s"));
        assert!(!is_auth_error("stream ended"));
        assert!(!is_auth_error("cannot reach 192.168.1.40:1401"));
    }

    #[test]
    fn packet_errors_are_recognized() {
        // As reported by the doorbell through its hub.
        let pkt = "NAL header fc has F bit set\n\nconn: 192.168.1.10:44182(me)->192.168.1.20:554@2026-10-31T15:29:22Z\nstream: TCP, interleaved channel ids 0-1\nssrc: aae4377a\nseq: 64641\npkt: 39671972@2026-10-31T15:30:56Z";
        assert!(is_packet_error(pkt));
        assert!(!is_packet_error("Unable to connect to RTSP server: connection refused"));
        assert!(!is_packet_error("RTSP framing error: bad header\n\nconn: x\nmsg: y"));
    }
}
