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
    // Baichuan TCP (or Auto) is the built-in reader's alone.
    if super::connect_way::mode_of(cfg) != super::connect_way::Mode::Rtsp && !cfg.overlay {
        return true;
    }
    // Burned-in text changes the picture: that needs re-encoding, which the
    // ffmpeg pipeline does.
    if cfg.overlay {
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
    l.contains("401 response") || l.contains("401 unauthorized") || l.contains("unauthorized") || l.contains("authentication failed") || l.contains("auth failed") || l.contains("sign-in refused")
        || l.contains("403 response") || l.contains("403 forbidden")
}

/// A clear refusal of the user name or password, after the sign-in was
/// really tried: RTSP 401 after the digest/basic answer was sent, RTSP 403,
/// or a refused Baichuan sign-in. Never a timeout, a refused connection or a
/// 5xx. Such a refusal is not asked again until the settings are saved
/// (cameras lock an account out after a few).
pub fn is_refused_password(e: &str) -> bool {
    let l = e.to_ascii_lowercase();
    l.contains("received unauthorized after trying digest auth") || l.contains("403 response to") || l.contains("sign-in refused")
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

/// RTSP status codes in an error ("404 response to DESCRIBE", "RTSP 401
/// Unauthorized"): three-digit words, so never a port ("192.0.2.10:554").
fn rtsp_statuses(e: &str) -> Vec<u16> {
    e.split_whitespace()
        .map(|w| w.trim_matches(|c: char| !c.is_ascii_alphanumeric()))
        .filter(|t| t.len() == 3)
        .filter_map(|t| t.parse::<u16>().ok())
        .filter(|n| (400..600).contains(n))
        .collect()
}

/// A built-in reader error in words for the owner, for the camera's last
/// error: what happened and what to check. retina's text holds the request
/// number and debug lines (`conn:`, `msg:`); those, and addresses, go to the
/// log only. Text that is already in words passes through unchanged.
pub fn readable_error(e: &str) -> String {
    let l = e.to_ascii_lowercase();
    let status = rtsp_statuses(e);
    let has = |n: u16| status.contains(&n);
    if has(404) || l.contains("no stream at that address") {
        "the camera has no stream at that address (404); check the path".into()
    } else if has(401) || is_auth_error(e) || l.contains("user name or password") {
        "the camera refused the user name or password".into()
    } else if has(453) || has(503) || l.contains("service unavailable") || l.contains("not enough bandwidth") || l.contains("too many connections") {
        "the camera is busy or has too many connections".into()
    } else if l.contains("connection refused") || l.contains("refused the connection") {
        "the camera refused the connection; check that it is on and that the port is right".into()
    } else if is_unreachable(e) || (l.contains("unable to connect") && (l.contains("timed out") || l.contains("timeout"))) {
        "the camera is not reachable; check that it is on and that its address is right".into()
    } else if l.contains("connection reset") || l.contains("broken pipe") {
        "the camera closed the connection".into()
    } else if let Some(n) = status.first() {
        format!("the camera turned the request down (RTSP {n})")
    } else {
        // Anything else: its first line, without addresses.
        let first = e.lines().map(str::trim).find(|x| !x.is_empty()).unwrap_or("");
        let kept: Vec<&str> = first.split_whitespace().filter(|w| !w.contains("://") && !w.contains('@')).collect();
        let t = kept.join(" ").replace("()", "").replace("( )", "");
        t.trim().trim_end_matches(':').trim().to_string()
    }
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
    /// Why the sub stream is not used although it is set (shown with the
    /// decoding note).
    sub_note: Option<String>,
    /// The camera's place on the NVIDIA card (see `NvdecSlot`).
    nvdec: NvdecSlot,
    /// End the session at the next clean point (a recording segment
    /// boundary, or a keyframe when nothing is being recorded): Auto is
    /// moving back to its primary way.
    clean_stop: Arc<AtomicBool>,
}

/// The Baichuan target for one of the camera's stream addresses (main for
/// `url`, sub for `sub_url`).
fn bc_target(cfg: &CameraConfig, url: &str) -> Option<crate::ingest::baichuan::wired::Target> {
    use crate::ingest::baichuan::wired::{target_for, StreamKind};
    let default = if url == cfg.url { StreamKind::Main } else { StreamKind::Sub };
    let port = cfg.baichuan_port.unwrap_or(crate::ingest::baichuan::transport::TCP_PORT);
    let channel = cfg.baichuan_channel.or(cfg.reolink_channel.and_then(|c| u8::try_from(c).ok()));
    target_for(url, port, channel, default).map(|t| crate::ingest::baichuan::wired::Target { owner: cfg.name.clone(), ..t })
}

/// A note for the way in use: a Baichuan session that is not AES-encrypted.
fn bc_note(cfg: &CameraConfig, use_bc: bool) -> Option<String> {
    if !use_bc {
        return None;
    }
    let t = bc_target(cfg, &cfg.url)?;
    let st = crate::ingest::baichuan::wired::status(&t.host, t.port)?;
    (st.aes == Some(false)).then(|| "the camera chose a Baichuan session that is not AES-encrypted (BC/XOR): trusted network only".to_string())
}

/// The camera's place on the NVIDIA card while its motion decoder runs
/// there. Owned by the worker, not the session: a reconnect or a stream
/// change keeps it until the new decoder has started (and takes it over or
/// lets it go), so a camera on a nearly full card stays there.
type NvdecSlot = Arc<std::sync::Mutex<Option<crate::hwdecode::NvdecHold>>>;

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
    /// The sub stream alone ended the session: it failed while the main
    /// stream was playing, and the main stream was still fine a while later
    /// (an outage takes both down, and that is not the sub stream's fault).
    sub_only: bool,
}

/// After the sub stream fails, how long the main stream must go on for the
/// failure to be the sub stream's alone (longer than the stall limit, so a
/// main stream that is about to stall too is seen to).
const SUB_GRACE: Duration = Duration::from_secs(12);

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
    // A sub stream that keeps ending the session: motion moves to the main stream.
    let mut sub_watch = super::ffpath::SubWatch::default();
    // Once the built-in reader has played this camera (here or before a
    // restart, at the same address), a later failure is the camera or its
    // hub (a hub answering 503 for a while), not something ffmpeg would do
    // better: it only switches after PLAYED_STUCK of failures (a firmware
    // change).
    let mut ever_played = played_recently(&engine, &slug_name, &cfg.url, chrono::Utc::now().timestamp());
    let mut busy_before_play: u32 = 0;
    // RTSP or Baichuan (the `connect` setting), and when Auto moves.
    let mode = super::connect_way::mode_of(&cfg);
    let mut picker = super::connect_way::WayPicker::new(mode, std::time::Instant::now());
    // Auto switches since video last came (both ways down: waits grow).
    let mut switches_without_video: u32 = 0;
    if mode != super::connect_way::Mode::Rtsp {
        info!("camera '{}': connect with {:?} ({} first)", cfg.name, mode, picker.current.label());
        // A new worker (its settings were saved, or a start): a refused
        // sign-in may be tried once more. Never retried on its own.
        if let Some(t) = bc_target(&cfg, &cfg.url) {
            crate::ingest::baichuan::wired::forget_refusal(&t.host, t.port);
        }
    }
    let mut failing_since: Option<std::time::Instant> = None;
    let mut backoff = Duration::from_secs(2);
    let mut port_watch = PortWatch::default();
    // The macOS Local Network note was logged for this outage.
    let mut said_blocked = false;
    let mut last_damaged_log: Option<std::time::Instant> = None;
    let nvdec: NvdecSlot = Arc::default();
    // Its place is kept through reconnects, not a long outage (see
    // `hwdecode::HOLD_THROUGH`).
    let mut outage = crate::hwdecode::Outage::default();
    handle.state.write().unwrap().ingest = "native";

    loop {
        // (Before `active` is looked at: see Engine::stop_for_rename.)
        handle.parked.store(false, Ordering::SeqCst);
        if !handle.active.load(Ordering::SeqCst) {
            handle.parked.store(true, Ordering::SeqCst);
            // Disabled: its decoder is gone, and so is its place on the card.
            nvdec.lock().unwrap().take();
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
                if st.sleeping && st.woken_by.is_none() {
                    st.woken_by = Some(if st.camera_detections.pulse_active() { "its motion push" } else { "camera-reported motion" });
                }
            }
            if now >= st.wake_until {
                if !st.sleeping {
                    info!("camera '{}': asleep until needed", cfg.name);
                }
                let close = super::worker::closes_at_sleep(event_start.is_some(), pending.len());
                st.sleeping = true;
                st.woken_by = None;
                st.online = false;
                st.offline_since = None;
                drop(st);
                nvdec.lock().unwrap().take();
                if close {
                    // The session ended with an error (stream dropped, no
                    // reconnect before the awake time ran out): the open
                    // event ends now and its clips are built, not at the
                    // next wake.
                    let segment_seconds = engine.config.lock().unwrap().storage.segment_seconds;
                    super::worker::close_events_for_sleep(
                        &handle, &engine, &cfg, segment_seconds,
                        &mut frames_over, &mut event_start, &mut event_score,
                        &mut best_frame, &mut pending, &mut ai_state,
                        &slug_name, &seg_dir, &clips_dir, &thumbs_dir,
                    );
                }
                thread::sleep(Duration::from_millis(300));
                continue;
            }
            if st.sleeping {
                // (Says what woke it: a battery camera kept awake by something
                // polling it drains its battery and posts no motion pushes.)
                let by = st.woken_by.take().unwrap_or("a request");
                info!("camera '{}': waking up for {by} (awake at least {} s)", cfg.name, (st.wake_until - now).max(0));
                st.woke_at_ms = Utc::now().timestamp_millis();
            }
            st.sleeping = false;
        }

        if outage.lapsed(std::time::Instant::now()) {
            nvdec.lock().unwrap().take();
        }
        let (motion_cfg, storage) = {
            let c = engine.config.lock().unwrap();
            (c.motion.for_camera(&cfg), c.storage.clone())
        };
        let segment_seconds = storage.segment_seconds;
        // One teardown group per session: awaited inside the session's own
        // runtime so TEARDOWN reaches the camera before the runtime ends.
        let group = Arc::new(retina::client::SessionGroup::default());
        let use_bc = picker.current == super::connect_way::Way::Baichuan;
        let creds = |url: &str| {
            let baichuan = if use_bc { bc_target(&cfg, url) } else { None };
            // A battery Argus carries its UID in the URL so open_stream can find
            // the persistent session the event poller started.
            let url = match cfg.battery_uid() {
                Some(u) if crate::ingest::baichuan::is_baichuan_url(url) && !url.contains("uid=") => {
                    format!("{url}{}uid={u}", if url.contains('?') { '&' } else { '?' })
                }
                _ => url.to_string(),
            };
            // A sub stream keeps its own sign-in in its address (see
            // normalize_creds); the reader takes it apart from the address.
            let (url, own) = super::creds::split_login(&url);
            let (username, password) = own.unwrap_or_else(|| (cfg.username.clone(), cfg.password.clone()));
            StreamSource { url, username, password, group: Some(group.clone()), transport: cfg.transport.clone(), baichuan }
        };
        // A sub stream address that cannot be opened ("stream2" without
        // rtsp://): motion uses the main stream, and the camera stays on
        // the built-in reader.
        let sub_url = if sub_watch.use_sub(std::time::Instant::now()) { super::usable_sub_url(&cfg) } else { None };
        if let (None, Some(why)) = (&sub_url, cfg.sub_url.as_deref().filter(|u| !u.trim().is_empty()).and_then(super::sub_url_problem)) {
            warn!("camera '{}': {why}; motion uses the main stream", cfg.name);
        }
        let plan = SessionPlan {
            main: creds(&cfg.url),
            sub: sub_url.as_deref().map(creds),
            record: match (cfg.record, cfg.record_motion_only) {
                (false, _) => None,
                (true, false) if cfg.record_sub && sub_url.is_some() => {
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
            sub_note: if sub_watch.trying() { Some("trying the sub stream again".into()) } else { sub_watch.off.clone() },
            nvdec: nvdec.clone(),
            clean_stop: Arc::new(AtomicBool::new(false)),
        };
        let clean_stop = plan.clean_stop.clone();
        if mode != super::connect_way::Mode::Rtsp {
            if !use_bc {
                // One way at a time: no Baichuan session to this camera may
                // still be signed in while RTSP is asked.
                if let Some(t) = bc_target(&cfg, &cfg.url) {
                    use crate::ingest::baichuan::wired::{release, Released};
                    let mut r = release(&t.host, t.port, &cfg.name, Duration::from_secs(8));
                    let until = std::time::Instant::now() + Duration::from_secs(30);
                    while r == Released::Busy && handle.active.load(Ordering::Relaxed) && std::time::Instant::now() < until {
                        thread::sleep(Duration::from_secs(1));
                        r = release(&t.host, t.port, &cfg.name, Duration::from_secs(8));
                    }
                    match r {
                        Released::Free => {}
                        Released::OthersOnly => tracing::debug!("camera '{}': the Baichuan session to {} stays for other cameras of that device", cfg.name, t.key()),
                        Released::Busy => {
                            // Not both ways at once: no RTSP while it is signed in.
                            let why = format!("the Baichuan session to {} did not sign out; RTSP waits", t.key());
                            warn!("camera '{}': {why}", cfg.name);
                            {
                                let mut st = handle.state.write().unwrap();
                                st.last_error = Some(why);
                                st.last_error_at = Some(Utc::now().timestamp());
                            }
                            continue;
                        }
                    }
                }
            }
            let mut st = handle.state.write().unwrap();
            st.via = Some(picker.current.label());
            st.via_note = picker.note.clone().or_else(|| bc_note(&cfg, use_bc));
        } else {
            let mut st = handle.state.write().unwrap();
            st.via = None;
            st.via_note = None;
        }

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
                    Err(e) => SessionEnd { reached_play: false, error: Some(format!("runtime: {e}")), transient: false, sub_only: false },
                }
            })
        };

        // Analysis loop: decoded frames drive motion, events and the live view.
        let mut idle_stop = false;
        // Pictures arrived this session (the camera really sent video).
        let mut got_video = false;
        // A given-up sub stream tried on the side while this session goes
        // on; when it works the session starts over on it.
        let mut probe: Option<mpsc::Receiver<Result<(), String>>> = None;
        let mut back_to_sub = false;
        loop {
            // (Not while on Baichuan: the probe would open the sub stream over
            // RTSP, both ways to the camera at once.)
            if probe.is_none() && !back_to_sub && !use_bc && sub_watch.probe_due(std::time::Instant::now()) {
                if let Some(u) = super::usable_sub_url(&cfg) {
                    let (tx, rx) = mpsc::channel();
                    let c = cfg.clone();
                    thread::spawn(move || {
                        let _ = tx.send(super::ffpath::probe_sub(&c, &u));
                    });
                    probe = Some(rx);
                }
            }
            if let Some(rx) = &probe {
                match rx.try_recv() {
                    Ok(outcome) => {
                        let (back, line) = sub_watch.probe_done(std::time::Instant::now(), outcome);
                        warn!("camera '{}': {line}", cfg.name);
                        if back {
                            back_to_sub = true;
                            stop.store(true, Ordering::Relaxed);
                        } else {
                            let mut d = handle.decode.lock().unwrap();
                            d.note = super::ffpath::replace_sub_note(d.note.take(), sub_watch.off.as_ref());
                        }
                        probe = None;
                    }
                    Err(mpsc::TryRecvError::Disconnected) => probe = None,
                    Err(mpsc::TryRecvError::Empty) => {}
                }
            }
            // Disabled, removed or shutting down: stop the session now (not
            // only when frames pause, which a busy camera never does).
            if !handle.active.load(Ordering::Relaxed) {
                stop.store(true, Ordering::Relaxed);
            }
            if cfg.on_demand && !idle_stop {
                // Viewers, an event in progress or camera-reported activity
                // keep it awake; otherwise it goes back to sleep. A battery
                // camera's event counts only for a while after its last push;
                // another camera's trigger counts while that camera has
                // motion, and a manual recording until it ends.
                let now_ms = Utc::now().timestamp_millis();
                let now = now_ms / 1000;
                let viewers = handle.live_main.viewers() + handle.live_sub.viewers() > 0;
                let battery = crate::ingest::baichuan::is_baichuan_url(&cfg.url);
                let mut st = handle.state.write().unwrap();
                let held = super::worker::on_demand_held(st.triggered_until, st.manual_until, now);
                let since = st.camera_detections.pulse_until().max(st.woke_at_ms);
                let busy = super::worker::on_demand_busy(viewers, event_start.is_some(), held, battery, now_ms, since);
                if busy || st.camera_detections.active() {
                    st.wake_until = st.wake_until.max(now + crate::config::idle_secs(cfg.idle_timeout));
                }
                if now >= st.wake_until {
                    info!("camera '{}': nothing needs it for {} s; ending the stream so it can sleep", cfg.name, crate::config::idle_secs(cfg.idle_timeout));
                    idle_stop = true;
                    stop.store(true, Ordering::Relaxed);
                }
            }
            if picker.primary_due(std::time::Instant::now()) && !clean_stop.load(Ordering::Relaxed) {
                info!(
                    "camera '{}': on {} for {} min; trying {} again at the next recording boundary",
                    cfg.name,
                    picker.current.label(),
                    super::connect_way::primary_retry().as_secs() / 60,
                    picker.primary.label()
                );
                clean_stop.store(true, Ordering::Relaxed);
            }
            match jpeg_rx.recv_timeout(Duration::from_millis(500)) {
                Ok(jpeg) => {
                    got_video = true;
                    if picker.video(std::time::Instant::now()) {
                        handle.state.write().unwrap().via_note = picker.note.clone();
                    }
                    process_frame(
                        &handle, &engine, &cfg, &mut detector, jpeg, segment_seconds,
                        &mut frames_over, &mut event_start, &mut event_score,
                        &mut best_frame, &mut pending, &mut ai_state,
                        &slug_name, &seg_dir, &clips_dir, &thumbs_dir, idle_stop,
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
            sub_only: false,
        });
        engine.live_sessions.fetch_sub(1, Ordering::Relaxed);
        handle.decode.lock().unwrap().stopped();
        outage.ended(got_video, std::time::Instant::now());
        if idle_stop {
            // Going back to sleep: an event still open (a battery camera's
            // awake time ran out) ends now, and waiting clips are built.
            super::worker::close_events_for_sleep(
                &handle, &engine, &cfg, segment_seconds,
                &mut frames_over, &mut event_start, &mut event_score,
                &mut best_frame, &mut pending, &mut ai_state,
                &slug_name, &seg_dir, &clips_dir, &thumbs_dir,
            );
        }
        if back_to_sub && handle.active.load(Ordering::Relaxed) {
            // Ended on purpose to move motion back to the sub stream.
            let mut st = handle.state.write().unwrap();
            st.online = false;
            st.motion = false;
            st.online_since = None;
            st.offline_since.get_or_insert(Utc::now().timestamp());
            drop(st);
            backoff = Duration::from_secs(2);
            continue;
        }
        {
            let mut st = handle.state.write().unwrap();
            st.online = false;
            st.motion = false;
            st.online_since = None;
            // Keep the time it first went offline across retries.
            st.offline_since.get_or_insert(Utc::now().timestamp());
            if handle.active.load(Ordering::Relaxed) && !idle_stop {
                st.reconnects += 1;
                let err = end.error.as_deref().unwrap_or("stream ended");
                st.last_error = Some(crate::macnet::blocked(err, &cfg.url).unwrap_or_else(|| readable_error(err)));
                st.last_error_at = Some(Utc::now().timestamp());
            }
            if idle_stop {
                st.offline_since = None;
            }
        }
        if !handle.active.load(Ordering::Relaxed) || idle_stop {
            continue; // disabled, replaced, or gone back to sleep
        }
        {
            use super::connect_way::{Ended, Next};
            let now = std::time::Instant::now();
            if clean_stop.load(Ordering::Relaxed) && picker.primary_due(now) {
                // Ended on purpose at a recording boundary: try the primary.
                picker.session_ended(&Ended { got_video, error: None, ..Default::default() }, now);
                picker.try_primary(now);
                info!("camera '{}': trying {} again", cfg.name, picker.current.label());
                failures_before_play = 0;
                backoff = Duration::from_secs(2);
                continue;
            }
            let err = end.error.clone().unwrap_or_else(|| "stream ended".into());
            // RTSP alone stops only on a clear refusal (anything looser keeps
            // the long waits below); Baichuan and Auto stop on any refusal, so
            // the same credentials are never tried the other way.
            let refused = if mode == super::connect_way::Mode::Rtsp { is_refused_password(&err) } else { is_auth_error(&err) };
            let ended = Ended {
                got_video,
                auth: !got_video && refused,
                battery: crate::ingest::baichuan::wired::is_battery_refusal(&err),
                error: Some(err),
            };
            if got_video {
                switches_without_video = 0;
            }
            match picker.session_ended(&ended, now) {
                Next::Same => {}
                Next::Switch { to, why } => {
                    warn!("camera '{}': {why}", cfg.name);
                    crate::camreport::note_end(&cfg.name, &why, false);
                    {
                        let mut st = handle.state.write().unwrap();
                        st.via = Some(to.label());
                        st.via_note = Some(why);
                    }
                    failures_before_play = 0;
                    busy_before_play = 0;
                    failing_since = None;
                    if switches_without_video == 0 {
                        // The other way gets a quick first try.
                        backoff = Duration::from_secs(2);
                        thread::sleep(Duration::from_millis(500));
                    } else {
                        // Both ways down: the waits keep growing (to a minute).
                        let until = std::time::Instant::now() + backoff;
                        while std::time::Instant::now() < until && handle.active.load(Ordering::Relaxed) {
                            thread::sleep(Duration::from_millis(250));
                        }
                        backoff = (backoff * 2).min(RETRY_MAX);
                    }
                    switches_without_video += 1;
                    continue;
                }
                Next::Stop { why } => {
                    // A refused password (or a channel that may be a battery
                    // camera): stop both ways until the settings change (a
                    // change replaces this worker).
                    warn!("camera '{}': {why}", cfg.name);
                    {
                        let mut st = handle.state.write().unwrap();
                        st.last_error = Some(why.clone());
                        st.last_error_at = Some(Utc::now().timestamp());
                        st.via_note = Some(why);
                    }
                    while handle.active.load(Ordering::Relaxed) {
                        thread::sleep(Duration::from_millis(500));
                        if outage.lapsed(std::time::Instant::now()) {
                            nvdec.lock().unwrap().take();
                        }
                    }
                    continue;
                }
            }
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
        // The sub stream alone ended the session: after a refusal, or a few
        // times in a row, motion moves to the main stream for a while (the
        // camera stays on the built-in reader); a later try takes it back.
        {
            let err = end.error.clone().unwrap_or_default();
            let why = readable_error(&err);
            let sub_failed = end.sub_only.then_some((err.as_str(), why.as_str()));
            if let Some(line) = sub_watch.ended(std::time::Instant::now(), session_began.elapsed(), sub_failed) {
                warn!("camera '{}': {line}", cfg.name);
                if end.sub_only {
                    failures_before_play = 0;
                    backoff = Duration::from_secs(2);
                    thread::sleep(Duration::from_millis(200));
                    continue;
                }
            }
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
        crate::camreport::note_end(&cfg.name, &why, end.reached_play && (lasted || got_video));
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
                if outage.lapsed(std::time::Instant::now()) {
                    nvdec.lock().unwrap().take();
                }
            }
            backoff = (backoff * 2).min(AUTH_RETRY_MAX);
            continue;
        }
        // (HTTP-FLV streams have no ffmpeg fallback: their sign-in is in
        // the query, which the ffmpeg pipeline does not add. Battery Argus
        // (baichuan://) has no ffmpeg fallback either: ffmpeg cannot open it,
        // and a camera that will not preview — e.g. older firmware — must keep
        // retrying the native client, not spin on ffmpeg.)
        if !end.reached_play && gave_up && mode == super::connect_way::Mode::Rtsp && cfg.ingest != "native" && !crate::ingest::flv::is_flv_url(&cfg.url) && !crate::ingest::push::is_push_url(&cfg.url) && !crate::ingest::baichuan::is_baichuan_url(&cfg.url) {
            warn!(
                "camera '{}': built-in RTSP reader could not play this camera ({why}); switching to ffmpeg ingest",
                cfg.name
            );
            crate::camreport::note_fallback(&cfg.name, &why);
            handle.state.write().unwrap().ingest = "ffmpeg (fallback)";
            camera_worker(handle, cfg, engine);
            return;
        }
        // (A long wait left from a refused sign-in does not apply here.)
        backoff = backoff.min(RETRY_MAX);
        warn!("camera '{}': {why}; reconnecting in {} s", cfg.name, backoff.as_secs());
        // macOS's Local Network privacy: say what to do, once per outage.
        if let Some(m) = crate::macnet::blocked(&why, &cfg.url) {
            if !said_blocked {
                warn!("camera '{}': {m}", cfg.name);
                said_blocked = true;
            }
        } else {
            said_blocked = false;
        }
        if !end.reached_play && is_unreachable(&why) {
            port_watch.failed_unreachable();
        }
        let until = std::time::Instant::now() + backoff;
        let probe_addr = if !port_watch.spent && probe_during(backoff) && !is_hub_channel(&cfg, &engine.config.lock().unwrap().cameras) { stream_addr(&cfg.url) } else { None };
        let mut next_probe = std::time::Instant::now() + PROBE_EVERY;
        while std::time::Instant::now() < until && handle.active.load(Ordering::Relaxed) {
            thread::sleep(Duration::from_millis(250));
            if outage.lapsed(std::time::Instant::now()) {
                nvdec.lock().unwrap().take();
            }
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

    // The main stream reached play this session (apart from the sub stream).
    let main_played = AtomicBool::new(false);
    let main = async {
        if !main_needed {
            return std::future::pending::<anyhow::Result<()>>().await;
        }
        let tx = main_decodes.then(|| jpeg_tx.clone());
        main_task(&plan, &handle, &engine, &stop, &reached_play, &main_played, tx).await
    };
    tokio::pin!(main);
    let sub = async {
        match &plan.sub {
            None => std::future::pending::<anyhow::Result<()>>().await,
            Some(src) => decode_only_task(&plan, src, &handle, &engine, &stop, &reached_play, jpeg_tx.clone()).await,
        }
    };
    // Whichever side ends first ends the session; the worker reconnects both.
    let (result, by_sub) = tokio::select! {
        r = &mut main => (r, false),
        r = sub => (r, true),
    };
    // The sub stream failed: its fault alone only when the main stream plays
    // (it may still be connecting when a sub stream is refused at once) and
    // still does a while later (not when both go down at once).
    let sub_only = by_sub
        && result.is_err()
        && main_needed
        && tokio::time::timeout(SUB_GRACE, &mut main).await.is_err()
        && main_played.load(Ordering::Relaxed);
    // Nothing keeps the main stream open to compare with: a refusal of the
    // sub stream (401/403/404) is its own fault. (If the main stream then
    // fails too, the camera shows that, and the later try of the sub
    // stream sorts it out.)
    let sub_only = sub_only || (by_sub && !main_needed && result.as_ref().err().is_some_and(|e| super::ffpath::is_refusal(&format!("{e:#}"))));
    // Let the camera free the session now (TEARDOWN), not after a timeout.
    if let Some(g) = &plan.main.group {
        let _ = tokio::time::timeout(Duration::from_secs(3), g.await_teardown()).await;
    }
    let transient = result.as_ref().err().is_some_and(|e| e.downcast_ref::<DamagedPacket>().is_some());
    SessionEnd { reached_play: reached_play.load(Ordering::Relaxed), error: result.err().map(|e| format!("{e:#}")), transient, sub_only }
}

enum Sink {
    Continuous(SegmentWriter),
    MotionOnly(TriggeredRecorder),
}

/// Whether a keyframe at `ticks` is a clean point to end the session at
/// (moving back to the primary way): a continuous recording where its
/// segment is full, a motion-only one when no clip is being written (never
/// in the middle of an event), and at once with nothing recorded.
fn at_clean_point(sink: Option<&Sink>, ticks: i64) -> bool {
    match sink {
        Some(Sink::Continuous(w)) => w.would_rotate(ticks),
        Some(Sink::MotionOnly(r)) => r.idle_at(ticks),
        None => true,
    }
}

/// The open recording of one stream task. However the task ends (its
/// stream stops, an error returns early, or the session drops it because
/// the other stream ended first), the file is finished and handed to the
/// indexer. Finishing is synchronous, so Drop can do it.
struct OpenSink<'a> {
    sink: Option<Sink>,
    engine: &'a Arc<Engine>,
    slug: &'a str,
}

impl<'a> OpenSink<'a> {
    fn new(engine: &'a Arc<Engine>, slug: &'a str) -> Self {
        OpenSink { sink: None, engine, slug }
    }
}

impl std::ops::Deref for OpenSink<'_> {
    type Target = Option<Sink>;
    fn deref(&self) -> &Option<Sink> {
        &self.sink
    }
}

impl std::ops::DerefMut for OpenSink<'_> {
    fn deref_mut(&mut self) -> &mut Option<Sink> {
        &mut self.sink
    }
}

#[cfg(test)]
thread_local! {
    /// Tests: finishing a sink panics (as a writer left half-updated by an
    /// earlier panic might).
    static PANIC_IN_FINISH: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

impl Drop for OpenSink<'_> {
    fn drop(&mut self) {
        let Some(mut k) = self.sink.take() else { return };
        // A task that panicked (inside the writer, say) drops this while
        // unwinding, with the writer maybe half-updated; a second panic in
        // here would abort the whole service, so it is caught.
        let finished = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            #[cfg(test)]
            if PANIC_IN_FINISH.with(|p| p.get()) {
                panic!("finish failed (test)");
            }
            finish_sink(&mut k, self.engine, self.slug);
        }));
        if let Err(p) = finished {
            if std::thread::panicking() {
                tracing::error!("camera '{}': could not finish the recording after a failure; its last file may be missing", self.slug);
            } else {
                std::panic::resume_unwind(p);
            }
        }
    }
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
    main_played: &AtomicBool,
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
    main_played.store(true, Ordering::Relaxed);
    match &plan.main.baichuan {
        Some(t) => info!("camera '{}': native stream playing ({})", handle.state.read().unwrap().name, t.describe()),
        None => info!("camera '{}': native stream playing ({})", handle.state.read().unwrap().name, redact_url(plan.main.url.clone())),
    }
    let mut sink = OpenSink::new(engine, &plan.slug);
    let mut motion = MotionDec::new("main", plan.nvdec.clone());
    let mut main_noted = false;
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
                // Moving back to the primary way: at a keyframe when there is
                // no continuous recording to end at a segment boundary.
                if key && plan.clean_stop.load(Ordering::Relaxed) && at_clean_point(sink.as_ref(), ticks) {
                    break;
                }
                handle.rx_frames.fetch_add(1, Ordering::Relaxed);
                handle.rx_bytes.fetch_add(data.len() as u64, Ordering::Relaxed);
                if new_params {
                    // New resolution/codec: restart the file and the decoder.
                    if let Some(mut old) = sink.take() {
                        finish_sink(&mut old, engine, &plan.slug);
                    }
                    motion.reset();
                    live = None;
                    main_noted = false;
                    shared.stop();
                }
                if key && !main_noted {
                    // The main stream's size, for the warning about decoding
                    // it for motion (no sub stream).
                    if let Ok(Some(v)) = s.video_params() {
                        handle.decode.lock().unwrap().main = Some((v.width, v.height, crate::hwdecode::codec_text(v.h265)));
                        main_noted = true;
                    }
                }
                if live.is_none() && key {
                    live = start_live(&s, &handle.live_main, &plan.slug)?;
                }
                if let Some(l) = live.as_mut() {
                    l.push_video(ticks, cts, &data, key);
                }
                share_video(&s, &shared, plan.with_audio, ticks + i64::from(cts), key, &data);
                if let Some(tx) = &jpeg_tx {
                    motion.feed(&s, plan, handle, &data, key, tx)?;
                }
                if sink.is_none() && key {
                    if let (Some(rec), Some(tracks)) = (plan.record, build_tracks(&s)?) {
                        let timescale = tracks[0].timescale;
                        let run = format!("{}-{}", plan.slug, Utc::now().timestamp_millis());
                        *sink = Some(match rec {
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
    let mut motion = MotionDec::new("sub", plan.nvdec.clone());
    let mut live: Option<LiveMuxer> = None;
    let mut sink = OpenSink::new(engine, &plan.slug);
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
            // Moving back to the primary way with no main stream session to
            // end it: stop at a keyframe.
            if key && plan.clean_stop.load(Ordering::Relaxed) && plan.record.is_none() && !handle.restream_main.wanted() {
                break;
            }
            if new_params {
                motion.reset();
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
            motion.feed(&s, plan, handle, &data, key, &jpeg_tx)?;
            // The sub-stream recording is extra: if writing it fails, only it
            // stops (for a minute), never the session and its main recording.
            let retry_ok = sub_failed.is_none_or(|t: std::time::Instant| t.elapsed() > Duration::from_secs(60));
            if let (None, Some(secs), true, true) = (&*sink, segment_seconds, key, retry_ok) {
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
                            *sink = Some(k);
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
            s.stalled();
            if s.stall_limit == WAKE_STALL {
                return Err(anyhow::anyhow!("{NO_PICTURE_AFTER_WAKE} within {} s", WAKE_STALL.as_secs()));
            }
            return Err(anyhow::anyhow!("no video from camera for {} s", s.stall_limit.as_secs()));
        }
        match tokio::time::timeout(Duration::from_millis(500), s.next()).await {
            Ok(Some(Ok(item))) => {
                if matches!(&item, MediaItem::Video { .. }) {
                    s.had_video = true;
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

/// How often a motion decoder looks whether it should run elsewhere: a
/// way that failed for this camera is due to be tried again, a way that
/// failed for every camera is free again, or Auto would now pick another.
const DEC_RECHECK: Duration = Duration::from_secs(60);

/// The motion decoder of one stream. It starts at a keyframe (a hardware
/// one at its turn: `hwdecode::reserve_start`) and starts again at a
/// keyframe on another way when the graphics card test finishes, the
/// Hardware decoding setting changes, or a recheck finds a different way.
struct MotionDec {
    stream: &'static str,
    d: Option<FrameDecoder>,
    /// What it was started with (None: the processor).
    spec: Option<String>,
    codec: Option<Codec>,
    /// Started while the graphics card test ran.
    pending: bool,
    generation: u64,
    recheck: std::time::Instant,
    /// The start time reserved for a hardware decoder.
    start_at: Option<std::time::Instant>,
    /// Its move to the processor has been noted.
    fell_noted: bool,
    seen_drops: u64,
    /// Its place on the NVIDIA card while it decodes there (the headroom
    /// guard keeps it there). Kept over a new decoder start (new stream
    /// parameters, a move, a reconnect) until that decoder has started.
    hold: NvdecSlot,
}

impl MotionDec {
    fn new(stream: &'static str, hold: NvdecSlot) -> Self {
        MotionDec {
            stream,
            d: None,
            spec: None,
            codec: None,
            pending: false,
            generation: crate::hwdecode::generation(),
            recheck: std::time::Instant::now() + DEC_RECHECK,
            start_at: None,
            fell_noted: false,
            seen_drops: 0,
            hold,
        }
    }

    /// New stream parameters: a new decoder at the next keyframe (the
    /// camera keeps its place on the card until then).
    fn reset(&mut self) {
        self.d = None;
        self.start_at = None;
    }

    fn feed(&mut self, s: &OpenStream, plan: &SessionPlan, handle: &CameraHandle, data: &[u8], key: bool, tx: &mpsc::SyncSender<Vec<u8>>) -> anyhow::Result<()> {
        if key && self.d.is_some() && self.should_move(plan, handle) {
            self.d = None;
        }
        if self.d.is_none() && key {
            self.start(s, plan, handle)?;
        }
        let Some(d) = self.d.as_mut() else { return Ok(()) };
        let _ = d.push(data, key);
        note_drops(handle, d, &mut self.seen_drops);
        if d.hw_fallback_used() && !self.fell_noted {
            // Shown as decoding on the processor; Auto may have another way
            // ready at once, so look again soon.
            self.fell_noted = true;
            self.hold.lock().unwrap().take();
            self.recheck = std::time::Instant::now() + Duration::from_secs(5);
            let mut dec = handle.decode.lock().unwrap();
            if let Some(was) = dec.gpu.take() {
                dec.note = Some(match d.hw_note() {
                    Some(n) => format!("{n}; using the processor until then"),
                    None => format!("hardware decoding ({was}) stopped working ({}); using the processor", d.hw_reason().unwrap_or("failed")),
                });
            }
        }
        forward_jpegs(d, tx);
        Ok(())
    }

    /// Whether the decoder should start again on another way (looked at
    /// when due only).
    fn should_move(&mut self, plan: &SessionPlan, handle: &CameraHandle) -> bool {
        let now = std::time::Instant::now();
        let generation = crate::hwdecode::generation();
        let changed = generation != self.generation;
        if !changed && now < self.recheck && !(self.pending && crate::hwdecode::probe_done()) {
            return false;
        }
        self.generation = generation;
        self.recheck = now + DEC_RECHECK;
        let running = if self.d.as_ref().is_some_and(|d| d.hw_fallback_used()) { None } else { self.spec.clone() };
        let name = handle.state.read().unwrap().name.clone();
        // (Never waits for the test: this runs on the camera's async task.
        // Only a look: `start` takes the card's place when it moves.)
        let r = crate::hwdecode::resolve(&name, plan.hwaccel.as_deref(), self.codec, Duration::ZERO);
        if !crate::hwdecode::worth_moving(running.as_deref(), r.spec.as_deref(), changed, self.codec) {
            self.pending = r.pending;
            // (The note says how it decodes now: only when that is what resolve says too.)
            if r.spec == running {
                handle.decode.lock().unwrap().note = super::ffpath::join_notes(r.why_not, plan.sub_note.as_ref());
            }
            return false;
        }
        let say = |s: &Option<String>| s.as_deref().map(crate::hwdecode::describe).unwrap_or_else(|| "the processor".into());
        info!("camera '{name}': motion decoding ({} stream) moves from {} to {}", self.stream, say(&running), say(&r.spec));
        true
    }

    /// Start the decoder: on the graphics card when the camera's setting,
    /// the server's preference and the start-up test allow it.
    fn start(&mut self, s: &OpenStream, plan: &SessionPlan, handle: &CameraHandle) -> anyhow::Result<()> {
        let Some(v) = s.video_params()? else { return Ok(()) };
        let codec = if v.h265 { Codec::H265 } else { Codec::H264 };
        let name = handle.state.read().unwrap().name.clone();
        let r = crate::hwdecode::resolve(&name, plan.hwaccel.as_deref(), Some(codec), Duration::ZERO);
        let now = std::time::Instant::now();
        if r.spec.is_some() {
            // Hardware decoders start one after another, a little apart.
            let at = *self.start_at.get_or_insert_with(|| crate::hwdecode::reserve_start(now));
            if at > now {
                return Ok(());
            }
        }
        self.start_at = None;
        let d = match FrameDecoder::start(codec, &v.extra_data, plan.fps, plan.scale, r.spec.clone()) {
            Ok(d) => d.with_camera(&name),
            Err(e) => {
                self.hold.lock().unwrap().take();
                return Err(e);
            }
        };
        // The new decoder takes the place over (the old one is let go only
        // after: the camera is never off the list in between).
        let h = crate::hwdecode::started(&name, &r);
        *self.hold.lock().unwrap() = h;
        self.spec = r.spec.clone();
        self.codec = Some(codec);
        self.pending = r.pending;
        self.generation = crate::hwdecode::generation();
        self.recheck = now + DEC_RECHECK;
        self.fell_noted = false;
        self.seen_drops = 0;
        let mut dec = handle.decode.lock().unwrap();
        dec.stream = Some(self.stream);
        dec.codec = Some(crate::hwdecode::codec_text(v.h265));
        dec.width = v.width;
        dec.height = v.height;
        dec.gpu = r.spec.as_deref().map(crate::hwdecode::describe);
        dec.hw_setting = Some(if r.camera_setting { "camera" } else { "server" });
        dec.note = super::ffpath::join_notes(r.why_not, plan.sub_note.as_ref());
        if self.stream == "main" {
            dec.main = Some((v.width, v.height, crate::hwdecode::codec_text(v.h265)));
        }
        self.d = Some(d);
        Ok(())
    }
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
        // (A piece from the camera's card says nothing about how its live stream is read.)
        let from_card = super::card_fill::is_card_piece(&seg.path.to_string_lossy());
        let job = move || {
            if !from_card {
                note_played(&engine, &slug);
            }
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

    /// A stream task with an open recording, as main_task / decode_only_task
    /// hold it: writes a few seconds, then either runs on (`pending`) or
    /// fails with `?`.
    async fn recording_task(engine: &Arc<Engine>, slug: &str, dir: &std::path::Path, then_fail: bool) -> anyhow::Result<()> {
        use crate::ingest::fmp4::{TrackInfo, TrackKind};
        let mut entry = vec![0, 0, 0, 16];
        entry.extend_from_slice(b"avc1");
        entry.extend_from_slice(&[0; 8]);
        let track = TrackInfo { kind: TrackKind::Video, timescale: 90000, sample_entry: entry, width: 640, height: 480 };
        let mut sink = OpenSink::new(engine, slug);
        *sink = Some(Sink::Continuous(SegmentWriter::new(dir, &format!("{slug}-1"), vec![track], 600)?));
        if let Some(Sink::Continuous(w)) = sink.as_mut() {
            for i in 0..30i64 {
                w.push_video_cts(i * 9000, 0, vec![0, 0, 0, 1, 0x65], i % 10 == 0)?;
            }
        }
        if then_fail {
            anyhow::bail!("stream error");
        }
        std::future::pending::<()>().await;
        Ok(())
    }

    #[test]
    fn moving_back_never_cuts_a_motion_only_clip_short() {
        use crate::ingest::fmp4::{TrackInfo, TrackKind};
        let dir = std::env::temp_dir().join(format!("amber-cleanpoint-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let mut entry = vec![0, 0, 0, 16];
        entry.extend_from_slice(b"avc1");
        entry.extend_from_slice(&[0; 8]);
        let track = TrackInfo { kind: TrackKind::Video, timescale: 90000, sample_entry: entry, width: 640, height: 480 };
        // 10 fps, a keyframe each second; pre-roll 1 s, post-roll 2 s.
        let mut r = TriggeredRecorder::new(dir.clone(), "cam", vec![track], 60, 1, 2, 90000);
        let frame = |i: i64| crate::ingest::preroll::Frame::Video { ticks: i * 9000, cts: 0, data: vec![0, 0, 0, 1, 0x65], is_key: i % 10 == 0 };
        for i in 0..20 {
            r.push(frame(i)).unwrap();
        }
        let mut sink = Some(Sink::MotionOnly(r));
        assert!(at_clean_point(sink.as_ref(), 20 * 9000), "idle: end at this keyframe");
        if let Some(Sink::MotionOnly(r)) = sink.as_mut() {
            r.trigger().unwrap();
            for i in 20..30 {
                r.push(frame(i)).unwrap();
            }
        }
        assert!(!at_clean_point(sink.as_ref(), 30 * 9000), "an event clip is being written");
        assert!(at_clean_point(sink.as_ref(), 40 * 9000), "its post-roll is over");
        assert!(at_clean_point(None, 0), "nothing recorded: the next keyframe");
        if let Some(Sink::MotionOnly(r)) = sink.as_mut() {
            r.stop().unwrap();
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_place_on_the_card_outlives_a_new_stream_and_a_reconnect() {
        let slot: NvdecSlot = Arc::default();
        *slot.lock().unwrap() = Some(crate::hwdecode::hold_for_test("NativeKeep"));
        // New stream parameters: the decoder goes, the place stays until the
        // next one starts.
        let mut m = MotionDec::new("sub", slot.clone());
        m.reset();
        assert!(slot.lock().unwrap().is_some());
        // The session ends (reconnect): the worker's slot keeps it.
        drop(m);
        assert!(crate::hwdecode::held_for_test("NativeKeep"));
        // The next session's decoder on another way lets it go.
        let _m = MotionDec::new("main", slot.clone());
        *slot.lock().unwrap() = None;
        assert!(!crate::hwdecode::held_for_test("NativeKeep"));
    }

    #[tokio::test]
    async fn the_open_file_is_indexed_when_the_other_stream_ends_the_session() {
        let dir = std::env::temp_dir().join(format!("amber-opensink-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let engine = Engine::for_test(toml::from_str("").unwrap());
        let indexed = |slug: &str| engine.db.segment_ids_overlapping(slug, 0, i64::MAX).unwrap().len();
        // The sub stream ends first: the main task is dropped mid-recording
        // (as run_session's select does).
        let main = recording_task(&engine, "cam_main", &dir, false);
        let sub = async {
            tokio::task::yield_now().await;
            anyhow::Ok(())
        };
        tokio::select! {
            biased;
            _ = main => unreachable!(),
            _ = sub => {}
        }
        assert_eq!(indexed("cam_main"), 1, "the dropped task's last file is in the index");
        // A task that returns early with an error finishes its file too.
        assert!(recording_task(&engine, "cam_err", &dir, true).await.is_err());
        assert_eq!(indexed("cam_err"), 1, "an error return still indexes the file");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_panic_while_finishing_after_a_panic_does_not_abort() {
        use crate::ingest::fmp4::{TrackInfo, TrackKind};
        let dir = std::env::temp_dir().join(format!("amber-opensink-panic-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let engine = Engine::for_test(toml::from_str("").unwrap());
        let (e, d) = (engine.clone(), dir.clone());
        // A stream task panics with an open recording whose finish panics
        // too: the thread ends with the first panic, the process lives on.
        let r = std::thread::spawn(move || {
            let mut entry = vec![0, 0, 0, 16];
            entry.extend_from_slice(b"avc1");
            entry.extend_from_slice(&[0; 8]);
            let track = TrackInfo { kind: TrackKind::Video, timescale: 90000, sample_entry: entry, width: 640, height: 480 };
            let mut sink = OpenSink::new(&e, "cam_panic");
            *sink = Some(Sink::Continuous(SegmentWriter::new(&d, "cam_panic-1", vec![track], 600).unwrap()));
            PANIC_IN_FINISH.with(|p| p.set(true));
            panic!("writer failed");
        })
        .join();
        let msg = r.expect_err("the task panicked");
        assert_eq!(msg.downcast_ref::<&str>(), Some(&"writer failed"), "the first panic is the one reported");
        let _ = std::fs::remove_dir_all(&dir);
    }

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
    fn errors_read_as_plain_words() {
        let not_found = "the camera has no stream at that address (404); check the path";
        for e in [
            "DESCRIBE failed: 404 response to DESCRIBE CSeq=3: Unexpected RTSP response status\n\nconn: 127.0.0.1:40000(me)->127.0.0.1:18554@2026-11-15T13:03:01Z\nmsg: RTSP/1.0 404 Not Found",
            "DESCRIBE failed: 404 response to DESCRIBE CSeq=2: Unexpected RTSP response status",
            not_found,
        ] {
            assert_eq!(readable_error(e), not_found, "{e}");
        }
        let auth = "the camera refused the user name or password";
        assert_eq!(readable_error("DESCRIBE failed: 401 response to DESCRIBE CSeq=3: Received Unauthorized after trying digest auth\n\nconn: x\nmsg: y"), auth);
        assert_eq!(readable_error("the camera refused the user name or password (check them in the camera's settings)"), auth);
        let busy = "the camera is busy or has too many connections";
        assert_eq!(readable_error("DESCRIBE failed: 503 response to DESCRIBE CSeq=3: Unexpected RTSP response status\n\nconn: x"), busy);
        assert_eq!(readable_error("SETUP failed: 453 response to SETUP CSeq=4: Not Enough Bandwidth"), busy);
        assert_eq!(readable_error(busy), busy);
        let refused = readable_error("DESCRIBE failed: Unable to connect to RTSP server: Connection refused (os error 111)");
        assert!(refused.starts_with("the camera refused the connection"), "{refused}");
        assert_eq!(readable_error(&refused), refused);
        let gone = readable_error("camera not reachable within 15 s (rtsp://192.0.2.10:554/stream)");
        assert!(gone.starts_with("the camera is not reachable"), "{gone}");
        assert_eq!(readable_error(&gone), gone);
        assert_eq!(readable_error("DESCRIBE failed: Unable to connect to RTSP server: connection timed out (os error 110)"), gone);
        assert_eq!(readable_error("RTSP framing error: Connection reset by peer (os error 104)\n\nconn: x"), "the camera closed the connection");
        assert_eq!(readable_error("PLAY failed: 500 response to PLAY CSeq=5: Unexpected RTSP response status\n\nconn: x"), "the camera turned the request down (RTSP 500)");
        // Already words: unchanged, and never with an address.
        assert_eq!(readable_error("no video from camera for 10 s"), "no video from camera for 10 s");
        assert_eq!(readable_error("camera closed the stream"), "camera closed the stream");
        assert_eq!(readable_error("camera did not answer within 15 s (rtsp://192.0.2.10:554/x)"), "camera did not answer within 15 s");
        // A port or address holding 404 is not a status.
        assert_eq!(readable_error("bad answer from 192.0.2.10:5404"), "bad answer from 192.0.2.10:5404");
        assert_eq!(readable_error("odd reply from rtsp://u:p@192.0.2.10:404/a"), "odd reply from");
        assert_eq!(readable_error("lost 192.0.2.10:554 at once"), "lost 192.0.2.10:554 at once");
    }

    #[test]
    fn sign_in_refusals_are_recognized() {
        assert!(is_auth_error("DESCRIBE failed: 401 response to DESCRIBE CSeq=3: Received Unauthorized after trying digest auth"));
        assert!(!is_auth_error("camera did not answer within 15 s"));
        assert!(!is_auth_error("stream ended"));
        assert!(!is_auth_error("cannot reach 192.168.1.40:1401"));
        assert!(is_auth_error("DESCRIBE failed: 403 response to DESCRIBE CSeq=2: Forbidden"), "403 is a refusal too");
    }

    #[test]
    fn only_a_clear_refusal_stops_an_rtsp_camera() {
        assert!(is_refused_password("DESCRIBE failed: 401 response to DESCRIBE CSeq=3: Received Unauthorized after trying digest auth"));
        assert!(is_refused_password("DESCRIBE failed: 403 response to DESCRIBE CSeq=2: Forbidden"));
        assert!(is_refused_password("sign-in refused: the camera did not accept the user name or password (Baichuan status 400)"));
        // Not a password verdict: a 401 with no sign-in tried (none set, or
        // no challenge), a timeout, a refused connection, a 5xx.
        for e in [
            "DESCRIBE failed: 401 response to DESCRIBE CSeq=2: Unauthorized without WWW-Authenticate header",
            "camera did not answer within 15 s (rtsp://192.0.2.5/x)",
            "DESCRIBE failed: Unable to connect to RTSP server: Connection refused (os error 111)",
            "DESCRIBE failed: 500 response to DESCRIBE CSeq=2: Internal Server Error",
            "DESCRIBE failed: 503 response to DESCRIBE CSeq=2: Service Unavailable",
            "cannot reach 192.168.1.40:1401",
        ] {
            assert!(!is_refused_password(e), "{e}");
        }
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

#[cfg(test)]
mod routing_tests {
    use super::*;

    fn cam(url: &str, extra: &str) -> CameraConfig {
        toml::from_str(&format!("name = \"a\"\nurl = \"{url}\"\n{extra}")).unwrap()
    }

    #[test]
    fn only_burned_text_needs_ffmpeg() {
        let rtsp = "rtsp://192.0.2.1/x";
        assert!(wants_native(&cam(rtsp, "")));
        assert!(!wants_native(&cam(rtsp, "overlay = true")));
        for url in ["push://yard-camera-0001", "baichuan://192.0.2.9?uid=TESTUID000000001"] {
            assert!(wants_native(&cam(url, "")), "{url}: no ffmpeg reader for it");
        }
    }
}
