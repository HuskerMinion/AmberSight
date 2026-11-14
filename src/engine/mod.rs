//! Camera engine: camera slots, workers, events, storage, alerts.

mod ai_check;
mod battery_clips;
mod alerts;
pub mod notify;
pub mod health;
pub mod camera_events;
mod clip;
mod copy_fs;
pub mod event_copy;
mod creds;
pub(crate) mod export;
mod native;
pub mod profiles;
pub(crate) mod playback;
mod storage;
pub mod system_alerts;
pub mod newcams;
pub mod timelapse;
pub mod faces;
pub mod lpr;
pub mod packages;
mod plan;
pub mod signals;
pub mod pause;
pub mod presence;
pub mod inactivity;
pub mod tamper;
pub mod digest;
pub mod noisy;
pub mod strangers;
pub mod unusual;
pub mod watchdog;
mod reindex;
pub mod smart;
pub mod smart_rules;
pub mod sound;
pub mod spots;
#[cfg(test)]
mod spots_replay;
pub use plan::list_drives;
mod worker;
mod zone_watch;

pub use creds::{camera_url, normalize_creds, redact_url};
pub use worker::{caption_drawtext, ffmpeg_input_args, video_filter};

use clip::*;
use storage::*;
use worker::*;

use crate::config::{parse_schedule, schedule_active, slug, CameraConfig, Config};
use std::collections::HashSet;
use crate::motion::MotionDetector;
use chrono::{Timelike, Utc};
use std::io::Read;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, RwLock};
use std::thread;
use tracing::{info, warn};

/// Kept events' clips waiting to be saved, and the threads saving them (see
/// `Engine::queue_clip_save`).
static CLIP_QUEUE: Mutex<(std::collections::VecDeque<String>, usize)> = Mutex::new((std::collections::VecDeque::new(), 0));
const CLIP_SAVERS: usize = 2;
const CLIP_WAITING_MAX: usize = 10_000;

#[derive(Debug, Clone, serde::Serialize)]
pub struct ClipInfo {
    pub id: String,
    pub camera: String,
    pub start: i64,
    pub end: i64,
    pub size: u64,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct Event {
    pub id: String,
    pub camera_id: usize,
    pub camera: String,
    pub start: i64,
    pub end: i64,
    pub score: f64,
    pub clip: Option<String>,
    pub thumb: Option<String>,
    /// Object labels from the ODT hook ([] when unused; older jsonl lines
    /// without the field deserialize fine via the default).
    #[serde(default)]
    pub objects: Vec<String>,
    /// Protected from retention/quota purges (classic NVR-style clip lock).
    #[serde(default)]
    pub protect: bool,
}

impl Event {
    /// As shown to clients: the clip is always its /api/clips/<id> address
    /// (events from the old recording pipeline stored a file path on the
    /// server; that same file is served at this address).
    pub fn public(mut self) -> Event {
        if self.clip.as_deref().is_some_and(|c| !c.starts_with("/api/")) {
            self.clip = Some(format!("/api/clips/{}", self.id));
        }
        self
    }
}

#[derive(Debug)]
pub struct CameraState {
    pub name: String,
    pub url: String,
    pub online: bool,
    pub motion: bool,
    pub score: f64,
    pub frames: u64,
    pub frame_seq: u64,
    pub latest_frame: Vec<u8>,
    pub clips: Vec<ClipInfo>,
    // Epoch until which motion is force-triggered (manual clip request).
    pub manual_until: i64,
    /// Epoch until which another camera's motion keeps this one triggered.
    pub triggered_until: i64,
    /// When this camera last passed its motion on to its also_trigger cameras.
    pub fanout_at: i64,
    /// Which pipeline receives the camera: "native", "ffmpeg", or
    /// "ffmpeg (fallback)" when native could not play it.
    pub ingest: &'static str,
    /// Moving objects in the latest analysed frame (normalized boxes).
    pub blobs: Vec<crate::motion::Blob>,
    /// What the camera's own detection currently reports (ONVIF events).
    pub camera_detections: camera_events::CameraDetections,
    /// Health: when frames started arriving (epoch s), when they stopped,
    /// how often the stream dropped, and why it last did.
    pub online_since: Option<i64>,
    pub offline_since: Option<i64>,
    pub reconnects: u32,
    pub last_error: Option<String>,
    /// When `last_error` happened (epoch s).
    pub last_error_at: Option<i64>,
    /// Last time motion was over the threshold (epoch s), for break time.
    pub last_motion_at: i64,
    /// Last time anything moved over the threshold, armed or not (epoch s),
    /// for the no-motion alert.
    pub last_activity_at: i64,
    /// On-demand (battery) cameras: awake until this time (epoch s), and
    /// whether the camera is currently asleep (not connected on purpose).
    pub wake_until: i64,
    pub sleeping: bool,
    /// What the camera/hub itself says about sleeping (battery cameras).
    pub device_sleeping: Option<bool>,
    /// Its battery as last reported (battery cameras; None: not known).
    pub battery: Option<camera_events::BatteryLevel>,
    /// How far the camera's own clock is from this computer's (seconds),
    /// from its RTCP sender reports.
    pub clock_skew: Option<f64>,
    /// The camera's microphone: loudness and loud sounds (sound trigger).
    pub sound: sound::SoundState,
    /// The last wanted object the AI saw live: label, score, when (epoch ms).
    /// Drives the detection chip and colored edge on live tiles.
    pub ai_last: Option<(String, f32, i64)>,
    /// Named areas with someone loitering in them now (their names).
    pub zones_occupied: Vec<String>,
}

/// Whether the camera is read by the built-in reader (the only one whose
/// streams can be shared).
pub fn reads_natively(cfg: &CameraConfig) -> bool {
    native::wants_native(cfg)
}

pub struct CameraHandle {
    pub id: usize,
    pub active: AtomicBool,
    /// The camera's own settings (what config.toml holds).
    pub cfg: RwLock<CameraConfig>,
    /// What the worker is running with: `cfg` plus profile overrides.
    pub running: RwLock<CameraConfig>,
    pub state: RwLock<CameraState>,
    /// Full-resolution live fragments (main stream) and the lighter sub
    /// stream, for browser live view. Idle when the camera runs on ffmpeg.
    pub live_main: Arc<crate::ingest::live::LiveHub>,
    pub live_sub: Arc<crate::ingest::live::LiveHub>,
    /// The same streams for sharing over RTSP (see restream/); idle unless shared.
    pub restream_main: Arc<crate::restream::hub::RestreamHub>,
    pub restream_sub: Arc<crate::restream::hub::RestreamHub>,
    /// Video frames received from the camera (counted before decoding), for the status panel's fps.
    pub rx_frames: std::sync::atomic::AtomicU64,
    /// Video bytes received, and frames the decoder had to drop (status panel).
    pub rx_bytes: std::sync::atomic::AtomicU64,
    pub dropped: std::sync::atomic::AtomicU64,
}

/// What became of a battery camera's motion push.
pub enum PushOutcome {
    /// Motion marked for this camera.
    Motion(String),
    /// No camera has that uid.
    UnknownUid,
    /// The camera exists but the push did not come from its address.
    WrongSender(String),
}

pub struct Engine {
    pub config: Arc<Mutex<Config>>,
    // Slot i corresponds to camera id i; removed cameras leave a None hole
    // so ids of running cameras never shift under a live session.
    pub cameras: RwLock<Vec<Option<Arc<CameraHandle>>>>,
    /// Events and the recordings index (`<data_dir>/ambersight.db`).
    pub db: Arc<crate::db::Db>,
    /// Built-in AI object detector, when [ai] has a model.
    /// The built-in detector; replaced while running (Settings > Detection & alerts > Objects (AI)).
    pub ai: RwLock<Option<Arc<crate::ai::AiDetector>>>,
    /// Active profile and the manual override (name, until epoch seconds).
    pub profile: RwLock<profiles::ActiveProfile>,
    /// Camera stream sessions currently running (for graceful shutdown).
    pub live_sessions: std::sync::atomic::AtomicUsize,
    pub manual_profile: Mutex<Option<(String, Option<i64>)>>,
    pub data_dir: std::path::PathBuf,
    pub config_path: String,
    /// The demo account as last seen (see web::demo_now); None: not looked yet.
    pub demo_seen: Mutex<Option<Option<String>>>,
    pub started: i64,
    // Global arm/disarm switch: false suppresses motion on every camera.
    pub shield: Arc<AtomicBool>,
    /// Arm delay running: the shield arms at this time (seconds); 0 = none.
    pub arm_at: std::sync::atomic::AtomicI64,
    /// Who started the running arm delay (for the audit log when it ends).
    pub arm_by: Mutex<String>,
    /// Presence (phones at home or away): the rule's state.
    pub presence: Mutex<presence::PresenceState>,
    /// Held across a whole presence evaluation (see presence_evaluate).
    pub presence_eval: Mutex<()>,
    /// Cameras whose notifications are paused: name -> until (seconds;
    /// i64::MAX = until resumed). Kept in the database across restarts.
    pub alert_pauses: Mutex<std::collections::HashMap<String, i64>>,
    /// MQTT publisher, set once the client has started.
    pub mqtt: std::sync::OnceLock<crate::mqtt::MqttHandle>,
    /// Cached machine/camera/storage figures for `/api/stats`.
    pub stats: crate::stats::Stats,
    /// Last notification times for [alerts] min_interval / group_interval.
    pub alert_throttle: Mutex<alerts::Throttle>,
    /// Camera watchdog state (see watchdog.rs).
    pub watchdog: Mutex<watchdog::Board>,
    /// Camera tamper detection state (see tamper.rs), by camera id.
    pub tamper: tamper::Board,
    /// Named overlay variables pushed in by automations (MQTT `<prefix>/overlay/
    /// <name>/set`, or `POST /api/overlay/<name>`): the live overlay shows them
    /// as `{v:<name>}`. Server-wide, in memory; a retained MQTT value restores
    /// itself on reconnect. name -> value.
    pub overlay_vars: Mutex<std::collections::HashMap<String, OverlayVar>>,
}

/// One named overlay variable: its current value and when it blanks itself.
#[derive(Clone)]
pub struct OverlayVar {
    pub value: String,
    /// Epoch seconds after which the value is dropped (a dead feed should blank,
    /// not keep showing a stale number). `None` = it stays until replaced.
    pub expires: Option<i64>,
}

const MAX_EVENT_SECONDS: i64 = 30;

impl Engine {
    pub fn start(config: Config, config_path: String) -> anyhow::Result<Arc<Engine>> {
        // Camera connections on Windows: a receive buffer large enough to
        // ride out a few seconds in which a camera is not read. With the
        // default, a Reolink camera not read for ~2 s cuts packets short and
        // the stream goes out of step (measured 2026-11-05: 2 s and 4 s pauses
        // broke it, with 8 MB none did). Linux sizes its buffers itself, and
        // a fixed size there would turn that off and be capped lower.
        #[cfg(windows)]
        retina::set_tcp_recv_buffer(8 << 20);
        crate::restream::set_enabled(config.restream.enabled);
        let data_dir = config.storage.data_dir.clone();
        std::fs::create_dir_all(&data_dir).ok();
        std::fs::create_dir_all(data_dir.join("clips")).ok();
        std::fs::create_dir_all(data_dir.join("thumbs")).ok();
        crate::transcode::cache::init(&data_dir, crate::transcode::cache::Limits::from_config(&config.transcode.sanitized().0));
        let db = Arc::new(crate::db::Db::open(&data_dir.join("ambersight.db"))?);
        crate::ingest::splice::open_index(&data_dir.join("layouts.db"));
        // Reviews from before the one-line verdicts were kept get theirs, a little at a time.
        {
            let db = db.clone();
            let _ = thread::Builder::new().name("review-sums".into()).spawn(move || match db.backfill_review_sums() {
                Ok(0) => {}
                Ok(n) => info!("database: worked out the one-line verdict of {n} AI reviews"),
                Err(e) => warn!("database: the AI review summaries could not be filled in: {e:#}"),
            });
        }
        // After the upgrade, a big recording index has its running totals counted, a little at a time.
        if !db.segment_totals_ready() {
            let db = db.clone();
            let _ = thread::Builder::new().name("segment-totals".into()).spawn(move || {
                let t0 = std::time::Instant::now();
                match db.fill_segment_totals() {
                    Ok(n) => info!("database: counted {n} recordings into the storage totals in {} s", t0.elapsed().as_secs()),
                    Err(e) => warn!("database: the storage totals could not be counted (the slower way is used): {e:#}"),
                }
            });
        }
        // One-time move of events.jsonl into the database (quick, and the
        // events should be there before cameras start).
        match db.import_events_log(&data_dir) {
            Ok(r) if r.events_imported + r.events_skipped > 0 => {
                info!("database: imported {} events ({} unreadable lines skipped)", r.events_imported, r.events_skipped)
            }
            Ok(_) => {}
            Err(e) => warn!("database: import of existing events failed: {e:#}"),
        }
        // Indexing of recordings already on disk (safe to repeat) walks every
        // folder, hundreds of thousands of files on a big site, and on a cold
        // disk that takes minutes: it runs on its own thread, a camera at a
        // time in small batches, while cameras and the web server start.
        // Retention waits for it (a half-built index must not judge anything).
        {
            let db = db.clone();
            let dir = data_dir.clone();
            let folders = config.storage.folder_roots();
            let seg = config.storage.segment_seconds;
            let running = db.begin_startup_import();
            let spawned = thread::Builder::new().name("index-existing".into()).spawn(move || {
                let _running = running;
                match db.import_existing_recordings(&dir, &folders, seg) {
                    Ok(0) => {}
                    Ok(n) => info!("database: indexed {n} existing recordings"),
                    Err(e) => warn!("database: indexing of existing recordings failed: {e:#}"),
                }
            });
            if let Err(e) = spawned {
                warn!("could not start indexing of existing recordings: {e}");
            }
        }
        // Where the NVIDIA add-on for the AI is kept or downloaded to, and
        // (set before the first load, which may start a download) reloading
        // the AI onto the card once it is in.
        #[cfg(all(target_os = "linux", target_arch = "x86_64", target_env = "gnu"))]
        let engine_slot: Arc<std::sync::OnceLock<std::sync::Weak<Engine>>> = Arc::new(std::sync::OnceLock::new());
        #[cfg(all(target_os = "linux", target_arch = "x86_64", target_env = "gnu"))]
        {
            crate::gpu_addon::configure(&data_dir, &config.update.repo, config.update.check);
            let slot = engine_slot.clone();
            crate::gpu_addon::on_ready(move || {
                if let Some(e) = slot.get().and_then(|w| w.upgrade()) {
                    if let Err(err) = e.reload_ai() {
                        warn!("AI: {err:#}");
                    }
                }
            });
        }
        let ai = match (&config.ai.enabled, &config.ai.model) {
            (true, Some(model)) => match crate::ai::AiDetector::load_on(model, config.ai.threads, config.ai.gpu, config.ai.gpu_name.as_deref(), config.ai.gpu_memory_mb) {
                Ok(d) => {
                    info!("AI: loaded {} ({}x{} input, on {})", model.display(), d.input_size(), d.input_size(), d.gpu_name().unwrap_or("CPU"));
                    Some(Arc::new(d))
                }
                Err(e) => {
                    warn!("AI: {e:#}; AI checks disabled");
                    None
                }
            },
            _ => None,
        };
        let manual = profiles::saved_manual(&db, &config);
        let engine = Arc::new(Engine {
            config: Arc::new(Mutex::new(config)),
            cameras: RwLock::new(Vec::new()),
            db,
            ai: RwLock::new(ai),
            profile: RwLock::new(profiles::ActiveProfile::default()),
            live_sessions: std::sync::atomic::AtomicUsize::new(0),
            manual_profile: Mutex::new(manual),
            data_dir,
            config_path,
            demo_seen: Mutex::new(None),
            started: Utc::now().timestamp(),
            shield: Arc::new(AtomicBool::new(true)),
            arm_at: std::sync::atomic::AtomicI64::new(0),
            arm_by: Mutex::new(String::new()),
            presence: Mutex::new(presence::PresenceState::default()),
            presence_eval: Mutex::new(()),
            alert_pauses: Mutex::new(std::collections::HashMap::new()),
            mqtt: std::sync::OnceLock::new(),
            stats: crate::stats::Stats::new(),
            alert_throttle: Mutex::new(alerts::Throttle::default()),
            watchdog: Mutex::new(watchdog::Board::default()),
            tamper: Mutex::new(std::collections::HashMap::new()),
            overlay_vars: Mutex::new(std::collections::HashMap::new()),
        });
        engine.load_alert_pauses();
        // Once the NVIDIA add-on is downloaded, load the AI again onto the card.
        #[cfg(all(target_os = "linux", target_arch = "x86_64", target_env = "gnu"))]
        let _ = engine_slot.set(Arc::downgrade(&engine));
        crate::push::init(&engine);
        {
            let mut cfgs = engine.config.lock().unwrap();
            for c in cfgs.cameras.iter_mut() {
                normalize_creds(c);
            }
        }
        let cams = engine.config.lock().unwrap().cameras.clone();
        // Pick the profile before cameras start, so they start with its
        // recording overrides.
        engine.apply_profile();
        if let Err(e) = engine.reload_faces() {
            warn!("faces: {e:#}");
        }
        if let Err(e) = engine.reload_lpr() {
            warn!("plates: {e:#}");
        }
        // The smart search and sound models load beside the cameras starting
        // (reading them takes seconds; recording must not wait).
        {
            let eng = engine.clone();
            thread::spawn(move || {
                eng.reload_smart();
                eng.reload_sound();
            });
        }
        for (i, c) in cams.iter().enumerate() {
            spawn_camera(engine.clone(), i, c.clone());
        }
        {
            let eng = engine.clone();
            thread::spawn(move || profiles::profile_loop(eng));
        }
        {
            let eng = engine.clone();
            thread::spawn(move || presence::presence_loop(eng));
        }
        let eng = engine.clone();
        let eng2 = eng.clone();
        {
            // On its own thread: on a big site it takes a while, and the
            // storage clean-up must not wait for it.
            let eng = engine.clone();
            thread::spawn(move || eng.mark_event_footage_backlog());
        }
        thread::spawn(move || retention_loop(eng));
        // Work out the storage plan once soon after start, so the first
        // Status page does not wait for it (later ones refresh it themselves).
        {
            let eng = engine.clone();
            let _ = thread::Builder::new().name("storage-plan-warm".into()).spawn(move || {
                thread::sleep(std::time::Duration::from_secs(45));
                let _ = eng.storage_plan(false);
            });
        }
        {
            let eng = engine.clone();
            thread::spawn(move || system_alerts::system_alert_loop(eng));
        }
        // Event clips also saved to a camera's copy folder.
        event_copy::start(engine.clone());
        {
            let eng = engine.clone();
            thread::spawn(move || newcams::watch_loop(eng));
        }
        {
            let eng = engine.clone();
            thread::spawn(move || inactivity::inactivity_loop(eng));
        }
        {
            let eng = engine.clone();
            thread::spawn(move || watchdog::watchdog_loop(eng));
        }
        {
            let eng = engine.clone();
            thread::spawn(move || unusual::unusual_loop(eng));
        }
        {
            let eng = engine.clone();
            thread::spawn(move || digest::outage_loop(eng));
        }
        {
            let eng = engine.clone();
            thread::spawn(move || digest::digest_loop(eng));
        }
        {
            let eng = engine.clone();
            thread::spawn(move || reindex::reindex_loop(eng));
        }
        {
            let eng = engine.clone();
            thread::spawn(move || smart::smart_loop(eng));
        }
        {
            let eng = engine.clone();
            thread::spawn(move || timelapse::timelapse_loop(eng));
        }
        thread::spawn(move || segment_scan_loop(eng2));
        crate::stats::start(engine.clone());
        crate::auth::attach_store(engine.db.clone());
        crate::account_http::start_account_requests(engine.clone());
        crate::backup::start(engine.clone());
        crate::missing::start(engine.clone());
        crate::offsite::start(engine.clone());
        if let Some(h) = crate::mqtt::start(&engine) { let _ = engine.mqtt.set(h); }
        crate::ingest::baichuan::pushx::start(engine.clone());
        {
            // A battery camera's "<camera> motion" marker left on by a restart
            // inside its window: nothing would turn it off until the next push.
            let pushed: Vec<String> = engine
                .config
                .lock()
                .unwrap()
                .cameras
                .iter()
                .filter(|c| c.uid.as_deref().is_some_and(|u| !u.is_empty()))
                .map(|c| format!("{} motion", c.name))
                .collect();
            let now = Utc::now().timestamp_millis();
            for (name, on, _) in engine.db.signal_spans(now, now, now).unwrap_or_default() {
                if on && pushed.contains(&name) {
                    engine.set_signal(&name, false, "restart");
                }
            }
        }
        {
            // Spots of cameras that are gone.
            let names: Vec<String> = engine.config.lock().unwrap().cameras.iter().map(|c| c.name.clone()).collect();
            spots::keep_only(&engine.db, &names);
        }
        Ok(engine)
    }

    /// Add a camera of this name to a test engine (nothing runs for it).
    #[cfg(test)]
    pub fn add_test_camera(&self, name: &str) -> Arc<CameraHandle> {
        let cfg: CameraConfig = toml::from_str(&format!("name = {name:?}\nurl = \"rtsp://127.0.0.1/x\"")).unwrap();
        let mut cams = self.cameras.write().unwrap();
        let h = make_handle(cams.len(), cfg.clone());
        cams.push(Some(h.clone()));
        self.config.lock().unwrap().cameras.push(cfg);
        h
    }

    /// An engine with no cameras, threads or files (an in-memory database),
    /// for tests of what it does by itself.
    #[cfg(test)]
    pub fn for_test(config: Config) -> Arc<Engine> {
        Arc::new(Engine {
            config: Arc::new(Mutex::new(config)),
            cameras: RwLock::new(Vec::new()),
            db: Arc::new(crate::db::Db::open_in_memory().unwrap()),
            ai: RwLock::new(None),
            profile: RwLock::new(profiles::ActiveProfile::default()),
            live_sessions: std::sync::atomic::AtomicUsize::new(0),
            manual_profile: Mutex::new(None),
            data_dir: std::env::temp_dir(),
            config_path: String::new(),
            demo_seen: Mutex::new(None),
            started: Utc::now().timestamp(),
            shield: Arc::new(AtomicBool::new(false)),
            arm_at: std::sync::atomic::AtomicI64::new(0),
            arm_by: Mutex::new(String::new()),
            presence: Mutex::new(presence::PresenceState::default()),
            presence_eval: Mutex::new(()),
            alert_pauses: Mutex::new(std::collections::HashMap::new()),
            mqtt: std::sync::OnceLock::new(),
            stats: crate::stats::Stats::new(),
            alert_throttle: Mutex::new(alerts::Throttle::default()),
            watchdog: Mutex::new(watchdog::Board::default()),
            tamper: Mutex::new(std::collections::HashMap::new()),
            overlay_vars: Mutex::new(std::collections::HashMap::new()),
        })
    }

    pub fn camera(&self, id: usize) -> Option<Arc<CameraHandle>> {
        self.cameras.read().unwrap().get(id).and_then(|h| h.clone())
    }

    /// Arm or disarm at once (cancels a running arm delay). `by` is who or
    /// what did it (a user name, "(mqtt)", "(schedule)", "(presence)"), `why` an
    /// optional note; a change is written to the log and the audit log.
    pub fn set_shield(&self, armed: bool, by: &str, why: &str) -> bool {
        let changed = {
            // (With the arm delay's own lock: a disarm can never be undone by
            // a delay finishing at the same moment.)
            let _g = pause::ARMING.lock().unwrap();
            let pending = self.arm_at.swap(0, Ordering::Relaxed) != 0;
            let was = self.shield.swap(armed, Ordering::Relaxed);
            was != armed || pending
        };
        if changed {
            self.record_change(by, if armed { "shield: armed" } else { "shield: disarmed" }, why);
        }
        changed
    }

    /// Set a named overlay variable from an automation. The payload is either
    /// the plain value, or a JSON object `{"value": "...", "ttl": <seconds>}`
    /// so a feed can have its value blank itself after `ttl` seconds instead of
    /// showing a stale number. An empty value clears the variable.
    pub fn set_overlay_var(&self, name: &str, payload: &str) {
        let name = name.trim();
        // Keep names tame: they travel in MQTT topics and the overlay template.
        if name.is_empty() || name.len() > 64 || !name.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '.')) {
            warn!("overlay variable: ignoring bad name {name:?}");
            return;
        }
        // A JSON object with a "value" field carries an optional ttl; anything
        // else (a bare number like "42" or text like "42°F") is the value itself.
        let (raw, ttl): (String, Option<i64>) = match serde_json::from_str::<serde_json::Value>(payload) {
            Ok(serde_json::Value::Object(o)) if o.contains_key("value") => {
                let v = match o.get("value") {
                    Some(serde_json::Value::String(s)) => s.clone(),
                    Some(serde_json::Value::Null) | None => String::new(),
                    Some(other) => other.to_string(),
                };
                (v, o.get("ttl").and_then(|t| t.as_i64()).filter(|&t| t > 0))
            }
            _ => (payload.to_string(), None),
        };
        // One line, no control characters, capped so an overlay stays legible.
        let value: String = raw.replace(['\r', '\n', '\t'], " ").chars().filter(|c| !c.is_control()).take(120).collect::<String>().trim().to_string();
        let now = Utc::now().timestamp();
        let mut m = self.overlay_vars.lock().unwrap();
        if value.is_empty() {
            m.remove(name);
            return;
        }
        // A misconfigured source must not grow the map without bound.
        if !m.contains_key(name) && m.len() >= 256 {
            warn!("overlay variable: too many set; ignoring new {name:?}");
            return;
        }
        m.insert(name.to_string(), OverlayVar { value, expires: ttl.map(|t| now.saturating_add(t)) });
    }

    /// The overlay variables that have not expired, as name -> value, for the
    /// browser overlay (also drops expired entries as it goes).
    pub fn overlay_vars_json(&self) -> serde_json::Map<String, serde_json::Value> {
        let now = Utc::now().timestamp();
        let mut m = self.overlay_vars.lock().unwrap();
        m.retain(|_, v| v.expires.is_none_or(|e| e > now));
        m.iter().map(|(k, v)| (k.clone(), serde_json::Value::String(v.value.clone()))).collect()
    }

    /// A battery camera's pushed PIR motion (pushx): find the camera whose
    /// config `uid` matches and count it as motion for `pushx.motion_seconds`,
    /// which wakes the on-demand camera and (trigger "camera"/"both") records
    /// the woken stream. Returns the camera's name, or None if no camera has
    /// that uid (an unknown or not-yet-added camera).
    pub fn pushx_motion(self: &Arc<Self>, uid: &str, from: Option<std::net::IpAddr>) -> PushOutcome {
        let uid = uid.trim();
        if uid.is_empty() {
            return PushOutcome::UnknownUid;
        }
        let secs = self.config.lock().unwrap().pushx.motion_seconds.max(1);
        let Some(handle) = self
            .cameras
            .read()
            .unwrap()
            .iter()
            .flatten()
            .find(|h| h.cfg.read().unwrap().uid.as_deref() == Some(uid))
            .cloned()
        else {
            return PushOutcome::UnknownUid;
        };
        let (name, url) = {
            let c = handle.cfg.read().unwrap();
            (c.name.clone(), c.url.clone())
        };
        // The push is unauthenticated: accept it only from the camera's own
        // address, so another host on the network cannot wake the camera
        // (draining its battery) or fake motion.
        let host = url.strip_prefix("baichuan://").unwrap_or("").split(['/', '?', ':']).next().unwrap_or("");
        if !crate::ingest::baichuan::pushx::sender_matches(host, from) {
            return PushOutcome::WrongSender(name);
        }
        let until = handle.state.write().unwrap().camera_detections.pulse(secs);
        // A visible marker on the timeline: "<camera> motion" turns on now and
        // off when this push's window ends. The end time doubles as a
        // generation: a later push extends it, and then that push's thread
        // turns the marker off instead of this one.
        let signal = format!("{name} motion");
        self.set_signal(&signal, true, "battery camera push");
        // The live recording starts seconds into the event: add the camera's
        // own recording of the first seconds from its SD card.
        battery_clips::spawn_lead_in(self.clone(), handle.clone(), Utc::now().timestamp_millis());
        let eng = self.clone();
        thread::spawn(move || {
            let wait = (until - Utc::now().timestamp_millis()).max(0) as u64;
            thread::sleep(std::time::Duration::from_millis(wait));
            if handle.state.read().unwrap().camera_detections.pulse_until() == until {
                eng.set_signal(&signal, false, "battery camera push");
            }
        });
        PushOutcome::Motion(name)
    }

    /// A change of arming or profile: one log line ("shield: armed by
    /// presence (everyone away 10 min)") and an audit log entry.
    pub fn record_change(&self, by: &str, what: &str, why: &str) {
        let by = if by.is_empty() { "?" } else { by };
        if why.is_empty() {
            info!("{what} by {by}");
        } else {
            info!("{what} by {by} ({why})");
        }
        let _ = self.db.insert_audit(by, "", what, why, 200);
    }

    pub fn manual_clip(self: &Arc<Engine>, id: usize, seconds: i64) -> bool {
        let handle = self.camera(id);
        if handle.is_none() {
            return false;
        }
        let until = Utc::now().timestamp().saturating_add(seconds);
        handle.unwrap().state.write().unwrap().manual_until = until;
        true
    }

    pub fn detector(&self) -> Option<Arc<crate::ai::AiDetector>> {
        self.ai.read().unwrap().clone()
    }

    /// Load (or drop) the built-in detector to match the AI settings now.
    /// One reload at a time, each reading the settings itself: a reload
    /// started earlier (the NVIDIA add-on arriving) never stores a model
    /// loaded with settings older than a later one's.
    pub fn reload_ai(&self) -> anyhow::Result<()> {
        static ONE_AT_A_TIME: Mutex<()> = Mutex::new(());
        let _one = ONE_AT_A_TIME.lock().unwrap_or_else(|e| e.into_inner());
        let (enabled, model, threads, gpu, gpu_name, gpu_mb) = {
            let c = self.config.lock().unwrap();
            (c.ai.enabled, c.ai.model.clone(), c.ai.threads, c.ai.gpu, c.ai.gpu_name.clone(), c.ai.gpu_memory_mb)
        };
        let new = match (enabled, model) {
            (true, Some(m)) => {
                let d = crate::ai::AiDetector::load_on(&m, threads, gpu, gpu_name.as_deref(), gpu_mb)?;
                info!("AI: loaded {} ({}x{} input, on {})", m.display(), d.input_size(), d.input_size(), d.gpu_name().unwrap_or("CPU"));
                Some(Arc::new(d))
            }
            _ => {
                // The AI is off: no card wanted.
                #[cfg(all(target_os = "linux", target_arch = "x86_64", target_env = "gnu"))]
                crate::gpu_addon::not_wanted();
                None
            }
        };
        *self.ai.write().unwrap() = new;
        Ok(())
    }

    /// The camera that takes pushed streams with this key.
    pub fn push_camera(&self, key: &str) -> Option<String> {
        let c = self.config.lock().unwrap();
        c.cameras.iter().find(|x| x.enabled && crate::ingest::push::push_key(&x.url) == Some(key)).map(|x| x.name.clone())
    }

    /// Keep `cfg`'s also_trigger cameras triggered for a few more seconds
    /// (called while its own motion event runs; at most once a second).
    pub fn trigger_others(&self, cfg: &CameraConfig, now: i64) {
        const HOLD_S: i64 = 4;
        if cfg.also_trigger.is_empty() {
            return;
        }
        let targets: Vec<Arc<CameraHandle>> = self
            .cameras
            .read()
            .unwrap()
            .iter()
            .flatten()
            .filter(|h| cfg.triggers(&h.cfg.read().unwrap()))
            .cloned()
            .collect();
        for h in targets {
            // A sleeping battery camera is woken so it can record.
            if h.cfg.read().unwrap().on_demand && h.state.read().unwrap().sleeping {
                self.wake_camera(h.id);
            }
            let mut st = h.state.write().unwrap();
            if st.triggered_until < now {
                tracing::info!("camera '{}': triggered by '{}'", h.cfg.read().unwrap().name, cfg.name);
            }
            st.triggered_until = st.triggered_until.max(now + HOLD_S);
        }
    }

    pub fn set_enabled(self: &Arc<Engine>, id: usize, enabled: bool) -> bool {
        let handle = self.camera(id);
        if handle.is_none() {
            return false;
        }
        let h = handle.unwrap();
        h.active.store(enabled, Ordering::Relaxed);
        h.cfg.write().unwrap().enabled = enabled;
        self.sync_config_cameras();
        true
    }

    pub fn add_camera_arc(self: &Arc<Engine>, mut cfg: CameraConfig) -> usize {
        normalize_creds(&mut cfg);
        let id = {
            let mut cams = self.cameras.write().unwrap();
            let id = match cams.iter().position(|c| c.is_none()) {
                Some(i) => i,
                None => cams.len(),
            };
            let handle = make_handle(id, cfg.clone());
            if id == cams.len() {
                cams.push(Some(handle.clone()));
            } else {
                cams[id] = Some(handle.clone());
            }
            spawn_worker(self.clone(), handle.clone(), cfg);
            id
        };
        self.sync_config_cameras();
        id
    }

    /// Replace a live camera's config in place (zones, sensitivity, direct,
    /// url...). The old worker parks and exits; a fresh one starts in the slot.
    pub fn update_camera(self: &Arc<Engine>, id: usize, cfg: CameraConfig) -> bool {
        self.update_camera_checked(id, |_| Ok::<_, ()>(cfg), |_| Ok(())).is_ok()
    }

    /// As `update_camera`, with the new settings made from the camera's
    /// settings as they are, under the cameras lock (no change made meanwhile
    /// is lost): `edit` gets the current settings and gives the new ones, and
    /// `may_restart` sees which settings would restart the camera (none: it
    /// keeps running). Either can refuse; then nothing changes. Ok: (before,
    /// after); Err(None): no such camera.
    pub fn update_camera_checked<E>(
        self: &Arc<Engine>,
        id: usize,
        edit: impl FnOnce(&CameraConfig) -> Result<CameraConfig, E>,
        may_restart: impl FnOnce(&[String]) -> Result<(), E>,
    ) -> Result<(CameraConfig, CameraConfig), Option<E>> {
        let (before, after) = {
            let mut cams = self.cameras.write().unwrap();
            if id >= cams.len() || cams[id].is_none() {
                return Err(None);
            }
            // Only notification and grouping settings changed: the stream keeps
            // running (no seconds of lost recording, no reconnect), and the new
            // values are read from the handle and the config as they are needed.
            let h = cams[id].as_ref().unwrap().clone();
            let before = h.cfg.read().unwrap().clone();
            let mut cfg = edit(&before).map_err(Some)?;
            normalize_creds(&mut cfg);
            let differs = restart_fields(&before, &cfg);
            may_restart(&differs).map_err(Some)?;
            if differs.is_empty() {
                for slot in [&h.cfg, &h.running] {
                    let mut c = slot.write().unwrap();
                    copy_light_fields(&mut c, &cfg);
                }
                drop(cams);
                self.sync_config_cameras();
                return Ok((before, cfg));
            }
            tracing::info!("camera '{}': restarting for a change in {}", cfg.name, differs.join(", "));
            cams[id].as_ref().unwrap().active.store(false, Ordering::Relaxed);
            let h = make_handle(id, cfg.clone());
            cams[id] = Some(h.clone());
            spawn_worker(self.clone(), h.clone(), cfg.clone());
            (before, cfg)
        };
        self.sync_config_cameras();
        Ok((before, after))
    }

    /// A camera is being renamed: move its "also trigger" links, profile
    /// overrides, user camera lists, recordings, events and AI reviews to the
    /// new name (the caller then saves the camera itself).
    pub fn rename_camera_refs(&self, old: &str, new: &str) {
        self.rename_alert_pause(old, new);
        event_copy::rename_camera(old, new);
        // Snoozes follow the camera (they are kept by its name's slug).
        if let Err(e) = self.db.snooze_rename_camera(&crate::config::slug(old), &crate::config::slug(new)) {
            tracing::warn!("could not move the snoozes of {old} to {new}: {e:#}");
        }
        {
            let mut c = self.config.lock().unwrap();
            for cam in c.cameras.iter_mut() {
                for t in cam.also_trigger.iter_mut() {
                    if t == old {
                        *t = new.to_string();
                    }
                }
            }
            for p in c.automation.profiles.iter_mut() {
                if let Some(o) = p.cameras.remove(old) {
                    p.cameras.insert(new.to_string(), o);
                }
            }
            for u in c.server.users.iter_mut() {
                for n in u.cameras.iter_mut() {
                    if n == old {
                        *n = new.to_string();
                    }
                }
            }
            let c = &mut *c;
            for n in c.faces.cameras.iter_mut().chain(c.timelapse.cameras.iter_mut()) {
                if n == old {
                    *n = new.to_string();
                }
            }
            for l in c.lpr.cameras.iter_mut() {
                if l.name == old {
                    l.name = new.to_string();
                }
            }
            crate::alert_rules::rename_camera(&mut c.alert_rules, old, new);
        }
        // Its time-lapses follow it.
        let (from, to) = (self.timelapse_dir().join(slug(old)), self.timelapse_dir().join(slug(new)));
        if from != to && from.is_dir() && !to.exists() {
            if let Err(e) = std::fs::rename(&from, &to) {
                warn!("could not move the time-lapses of '{old}' to '{new}': {e}");
            }
        }
        if let Err(e) = self.db.rename_camera(old, new) {
            warn!("could not move recordings of '{old}' to '{new}': {e:#}");
        }
    }

    /// Restart a camera's worker with its current settings (e.g. the active
    /// profile changed its recording mode). Nothing is written to config.toml.
    pub fn restart_camera(self: &Arc<Engine>, id: usize) {
        let mut cams = self.cameras.write().unwrap();
        let Some(Some(old)) = cams.get(id).cloned() else { return };
        let enabled = old.active.load(Ordering::Relaxed);
        old.active.store(false, Ordering::Relaxed);
        let base = old.cfg.read().unwrap().clone();
        let h = make_handle(id, base.clone());
        h.active.store(enabled, Ordering::Relaxed);
        cams[id] = Some(h.clone());
        drop(cams);
        spawn_worker(self.clone(), h, base);
    }

    pub fn remove_camera(&self, id: usize) -> bool {
        let removed = {
            let mut cams = self.cameras.write().unwrap();
            if id >= cams.len() || cams[id].is_none() {
                return false;
            }
            let h = cams[id].take().unwrap();
            h.active.store(false, Ordering::Relaxed);
            let name = h.cfg.read().unwrap().name.clone();
            Some(name)
        };
        // A new camera given the same name must not inherit its alert pause.
        if let Some(name) = &removed {
            self.pause_alerts(name, Some(0));
            // ... nor anyone's snooze of it.
            if let Err(e) = self.db.snooze_forget_camera(&crate::config::slug(name)) {
                tracing::warn!("could not forget the snoozes of {name}: {e:#}");
            }
            // ... nor its plate reading or alert rules.
            let mut c = self.config.lock().unwrap();
            c.lpr.cameras.retain(|l| &l.name != name);
            crate::alert_rules::forget_camera(&mut c.alert_rules, name);
        }
        let removed = removed.is_some();
        self.sync_config_cameras();
        removed
    }

    /// Rewrite config.cameras from the live camera slots (skipping holes)
    /// so add/remove/enable persist correctly regardless of index drift.
    fn sync_config_cameras(&self) {
        let live: Vec<CameraConfig> = self.cameras
            .read()
            .unwrap()
            .iter()
            .filter(|slot| slot.is_some())
            .map(|slot| {
                let h = slot.as_ref().unwrap();
                let mut c = h.cfg.read().unwrap().clone();
                c.enabled = h.active.load(Ordering::Relaxed);
                c
            })
            .collect();
        let names: Vec<String> = live.iter().map(|c| c.name.clone()).collect();
        {
            let mut cfg = self.config.lock().unwrap();
            cfg.cameras = live;
            let _ = cfg.save(&self.config_path);
        }
        // A removed or renamed camera's remembered spots go (a renamed one
        // may point somewhere else now).
        spots::keep_only(&self.db, &names);
    }

    pub fn record_event(&self, ev: Event, send_alert: bool) {
        self.record_event_as(ev, send_alert, None);
    }

    /// As `record_event`; `headline` replaces the alert's title ("Unknown
    /// person at Front Door").
    pub fn record_event_as(&self, ev: Event, send_alert: bool, headline: Option<&str>) {
        if let Err(e) = self.db.insert_event(&ev) {
            warn!("could not save event {}: {e:#}", ev.id);
        } else {
            self.mark_event_footage(&ev.id, &ev.camera, ev.start, ev.end);
        }
        if send_alert {
            self.fire_alert_as(&ev, headline);
        }
    }

    /// Mark the recordings an event covers (with its pre/post roll), so a
    /// full folder gives up quiet footage first.
    fn mark_event_footage(&self, id: &str, camera: &str, start: i64, end: i64) -> bool {
        let m = self.config.lock().unwrap().motion_for(camera);
        let (from, to) = ((start - m.pre_roll as i64) * 1000, (end + m.post_roll as i64) * 1000);
        if let Err(e) = self.db.mark_event_footage(id, &slug(camera), from, to) {
            warn!("could not mark the footage of event {id}: {e:#}");
            return false;
        }
        true
    }

    /// Events saved before footage marking existed (or by an older version).
    pub fn mark_event_footage_backlog(&self) {
        let mut done = 0usize;
        loop {
            let batch = match self.db.events_without_span(500) {
                Ok(b) if !b.is_empty() => b,
                _ => break,
            };
            let mut failed = false;
            for (id, cam, start, end) in batch {
                if self.mark_event_footage(&id, &cam, start, end) {
                    done += 1;
                } else {
                    failed = true;
                }
            }
            // A failing database (full disk, locked) would return the same
            // events forever: stop; the next start tries again.
            if failed {
                warn!("stopped marking earlier events' footage after errors");
                break;
            }
        }
        if done > 0 {
            info!("marked the footage of {done} earlier events");
        }
    }

    /// Path of a recording folder: "" (or an unknown name) is the data folder.
    pub fn folder_root(&self, name: &str) -> std::path::PathBuf {
        if name.is_empty() {
            return self.data_dir.clone();
        }
        let c = self.config.lock().unwrap();
        match c.storage.folders.iter().find(|f| f.name == name) {
            Some(f) => f.path.clone(),
            None => self.data_dir.clone(),
        }
    }

    /// Which recording folder a file is in: (folder name, path relative to
    /// it). The most specific matching folder wins; "" is the data folder.
    pub fn locate(&self, path: &std::path::Path) -> (String, String) {
        let mut best: (String, std::path::PathBuf) = (String::new(), self.data_dir.clone());
        let mut best_len = if path.starts_with(&self.data_dir) { self.data_dir.components().count() } else { 0 };
        for f in self.config.lock().unwrap().storage.folders.iter() {
            let n = f.path.components().count();
            if path.starts_with(&f.path) && n > best_len {
                best = (f.name.clone(), f.path.clone());
                best_len = n;
            }
        }
        (best.0, crate::db::relative_path(&best.1, path))
    }

    /// Full path of an indexed recording.
    pub fn segment_path(&self, row: &crate::db::SegmentRow) -> std::path::PathBuf {
        self.folder_root(&row.folder).join(&row.path)
    }

    /// Folder a camera records into: (name, path). Unknown names fall back
    /// to the data folder with a warning.
    pub fn record_folder(&self, cfg: &CameraConfig) -> (String, std::path::PathBuf) {
        match cfg.folder.as_deref().filter(|f| !f.is_empty()) {
            None => (String::new(), self.data_dir.clone()),
            Some(name) => {
                let c = self.config.lock().unwrap();
                match c.storage.folders.iter().find(|f| f.name == name) {
                    Some(f) => (f.name.clone(), f.path.clone()),
                    None => {
                        warn!("camera '{}': unknown recording folder '{name}', using the data folder", cfg.name);
                        (String::new(), self.data_dir.clone())
                    }
                }
            }
        }
    }

    /// Stop every camera so each session sends TEARDOWN and finishes its
    /// recording file, waiting up to `wait` for them. Nothing is written to
    /// config.toml (cameras keep their enabled setting).
    pub fn shutdown(&self, wait: std::time::Duration) {
        let cams: Vec<Arc<CameraHandle>> = self.cameras.read().unwrap().iter().flatten().cloned().collect();
        for h in &cams {
            h.active.store(false, Ordering::Relaxed);
        }
        let until = std::time::Instant::now() + wait;
        while self.live_sessions.load(Ordering::Relaxed) > 0 && std::time::Instant::now() < until {
            thread::sleep(std::time::Duration::from_millis(100));
        }
        let left = self.live_sessions.load(Ordering::Relaxed);
        if left > 0 {
            warn!("shutdown: {left} camera session(s) did not close in time");
        } else {
            info!("shutdown: all camera sessions closed");
        }
        let unindexed = self.wait_for_indexing(std::time::Duration::from_secs(5));
        if unindexed > 0 {
            warn!("shutdown: {unindexed} recording(s) not yet in the database (the next folder sweep adds them)");
        }
    }

    /// Wake an on-demand camera (or keep it awake) for its idle timeout.
    /// Returns false for unknown cameras; always-on cameras are unaffected.
    pub fn wake_camera(&self, id: usize) -> bool {
        let Some(h) = self.camera(id) else { return false };
        let idle = crate::config::idle_secs(h.running.read().unwrap().idle_timeout);
        let mut st = h.state.write().unwrap();
        st.wake_until = st.wake_until.max(Utc::now().timestamp() + idle);
        true
    }

    /// Toggle protection on an event. Returns true when the event exists.
    /// Protecting an event also saves its clip as a file, so it is kept even
    /// after retention deletes the recordings it was cut from.
    pub fn set_protect(self: &Arc<Engine>, id: &str, val: bool) -> bool {
        let found = self.db.set_protect(id, val).unwrap_or_else(|e| {
            warn!("could not update event {id}: {e:#}");
            false
        });
        if found && val {
            self.queue_clip_save(id);
        }
        found
    }

    /// No room to queue another kept clip for saving now (the keep request is
    /// then answered "busy", and changes nothing).
    pub fn clip_saves_full(&self) -> bool {
        CLIP_QUEUE.lock().unwrap_or_else(|p| p.into_inner()).0.len() >= CLIP_WAITING_MAX
    }

    /// Kept events' clips are saved by at most `CLIP_SAVERS` threads at a
    /// time, in turn: many kept at once (by hand or by a program) do not each
    /// start a thread. A save that panics is logged and the next one goes on.
    fn queue_clip_save(self: &Arc<Engine>, id: &str) {
        {
            let mut q = CLIP_QUEUE.lock().unwrap_or_else(|p| p.into_inner());
            if q.0.len() >= CLIP_WAITING_MAX {
                warn!("protect {id}: too many clips waiting to be saved; it is kept, but its clip is not saved now");
                return;
            }
            if !q.0.iter().any(|x| x == id) {
                q.0.push_back(id.to_string());
            }
            if q.1 >= CLIP_SAVERS {
                return;
            }
            q.1 += 1;
        }
        let engine = self.clone();
        thread::spawn(move || loop {
            let next = {
                let mut q = CLIP_QUEUE.lock().unwrap_or_else(|p| p.into_inner());
                let n = q.0.pop_front();
                if n.is_none() {
                    q.1 -= 1;
                }
                n
            };
            let Some(id) = next else { return };
            let saved = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                if let Ok(Some(ev)) = engine.db.get_event(&id) {
                    if let Err(e) = engine.save_clip_file(&ev) {
                        warn!("protect {id}: could not save clip file: {e:#}");
                    }
                }
            }));
            if saved.is_err() {
                warn!("protect {id}: saving its clip failed unexpectedly");
            }
        });
    }

    pub fn total_events(self: &Engine) -> usize {
        self.db.total_events().unwrap_or(0)
    }

    /// Names of the cameras whose unconfirmed events are left out of lists.
    pub fn hide_unconfirmed_cameras(&self) -> Vec<String> {
        let c = self.config.lock().unwrap();
        c.cameras.iter().filter(|cam| cam.ai_hide_unconfirmed.unwrap_or(c.ai.hide_unconfirmed)).map(|cam| cam.name.clone()).collect()
    }

    /// Events newest-first, filtered and paged.
    pub fn filtered_events(
        self: &Arc<Engine>,
        limit: usize,
        camera: Option<String>,
        since: Option<i64>,
        until: Option<i64>,
        offset: usize,
        object: Option<String>,
        all: bool,
    ) -> Vec<Event> {
        // Cameras set to leave out motion AI found nothing in (unless all are wanted).
        let hide_unconfirmed = if all { Vec::new() } else { self.hide_unconfirmed_cameras() };
        let q = crate::db::EventQuery { camera, since, until, object, limit, offset, hide_unconfirmed, ..Default::default() };
        self.db.query_events(&q).unwrap_or_else(|e| {
            warn!("event query failed: {e:#}");
            Vec::new()
        })
    }

    /// Recording covering a wall-clock time (epoch seconds) for a camera.
    pub fn seek(self: &Arc<Engine>, camera_name: &str, at: i64) -> serde_json::Value {
        match self.db.segment_at(&slug(camera_name), at * 1000) {
            Ok(Some(r)) => serde_json::json!({
                "found": true,
                // The file's name only: its folder is for admins (Admin pages).
                "segment": std::path::Path::new(&r.path).file_name().map(|n| n.to_string_lossy().into_owned()),
                "start_ms": r.start_ms,
                "end_ms": r.end_ms,
                "offset_ms": at * 1000 - r.start_ms,
                "size": r.bytes,
            }),
            Ok(None) => serde_json::json!({ "found": false }),
            Err(e) => serde_json::json!({ "found": false, "error": format!("{e:#}") }),
        }
    }
}

/// What the admin page says when a model download failed: plain words for the
/// owner (the technical error goes to the log only). `what` is "the plate reader".
pub(crate) fn download_failure(what: &str, e: &anyhow::Error) -> String {
    let t = format!("{e:#}").to_lowercase();
    let why = if t.contains("checksum") || t.contains("damaged") {
        "the file that arrived was incomplete or damaged"
    } else if t.contains("disk") || t.contains("permission") || t.contains("no space") || t.contains("create") {
        "the file could not be saved on this computer (check the free space on the drive)"
    } else if t.contains("download") || t.contains("timeout") || t.contains("timed out") || t.contains("connect") || t.contains("dns") || t.contains("http") || t.contains("receive") {
        "the download site did not answer"
    } else {
        "it could not be set up (the details are in Settings > System > Logs)"
    };
    format!("Could not download {what}: {why}. Nothing is wrong with your cameras or this server. Try again in a few minutes.")
}

/// Settings that only matter when a notification is decided (or how the
/// camera is listed): changing them does not touch the stream. Everything not
/// listed here (including the camera's own on/off for notifications and its
/// short name, which the worker holds) restarts the camera as before.
fn copy_light_fields(to: &mut CameraConfig, from: &CameraConfig) {
    to.alert_labels = from.alert_labels.clone();
    to.alert_ignore_polys = from.alert_ignore_polys.clone();
    to.alert_interval = from.alert_interval;
    to.alert_schedule = from.alert_schedule.clone();
    to.alert_channels = from.alert_channels.clone();
    to.alert_priority = from.alert_priority.clone();
    to.groups = from.groups.clone();
    to.group = from.group.clone();
    // Event copies are read from the config when an event is queued and copied.
    to.copy_events_to = from.copy_events_to.clone();
    to.copy_events_keep_days = from.copy_events_keep_days;
    to.copy_events_only_confirmed = from.copy_events_only_confirmed;
}

/// True when `new` differs from `old` in anything but the light fields.
#[cfg(test)]
fn restart_needed(old: &CameraConfig, new: &CameraConfig) -> bool {
    !restart_fields(old, new).is_empty()
}

/// The settings that differ between `old` and `new`, apart from the light fields.
fn restart_fields(old: &CameraConfig, new: &CameraConfig) -> Vec<String> {
    let mut a = old.clone();
    copy_light_fields(&mut a, new);
    let (Ok(serde_json::Value::Object(a)), Ok(serde_json::Value::Object(b))) = (serde_json::to_value(&a), serde_json::to_value(new)) else {
        return vec!["?".into()];
    };
    let mut keys: Vec<&String> = a.keys().chain(b.keys()).collect();
    keys.sort();
    keys.dedup();
    keys.into_iter().filter(|k| a.get(*k) != b.get(*k)).cloned().collect()
}

fn make_handle(id: usize, cfg: CameraConfig) -> Arc<CameraHandle> {
    // (Never shared with a privacy mask or overlay: the stream would bypass it.)
    let shared = cfg.share && crate::restream::enabled() && crate::restream::http::blocker(&cfg).is_none();
    Arc::new(CameraHandle {
        id,
        active: AtomicBool::new(cfg.enabled),
        cfg: RwLock::new(cfg.clone()),
        running: RwLock::new(cfg.clone()),
        live_main: Arc::new(crate::ingest::live::LiveHub::new()),
        live_sub: Arc::new(crate::ingest::live::LiveHub::new()),
        restream_main: Arc::new(crate::restream::hub::RestreamHub::new(shared)),
        restream_sub: Arc::new(crate::restream::hub::RestreamHub::new(shared)),
        rx_frames: std::sync::atomic::AtomicU64::new(0),
        rx_bytes: std::sync::atomic::AtomicU64::new(0),
        dropped: std::sync::atomic::AtomicU64::new(0),
        state: RwLock::new(CameraState {
            name: cfg.name.clone(),
            url: cfg.url.clone(),
            online: false,
            motion: false,
            score: 0.0,
            frames: 0,
            frame_seq: 0,
            latest_frame: Vec::new(),
            clips: Vec::new(),
            manual_until: 0,
            triggered_until: 0,
            fanout_at: 0,
            ingest: "starting",
            blobs: Vec::new(),
            camera_detections: Default::default(),
            online_since: None,
            offline_since: Some(Utc::now().timestamp()),
            reconnects: 0,
            last_error: None,
            last_error_at: None,
            last_motion_at: 0,
            last_activity_at: 0,
            wake_until: 0,
            sleeping: false,
            device_sleeping: None,
            battery: None,
            ai_last: None,
            zones_occupied: Vec::new(),
            clock_skew: None,
            sound: Default::default(),
        }),
    })
}

fn spawn_worker(engine: Arc<Engine>, handle: Arc<CameraHandle>, cfg: CameraConfig) {
    // Workers run with the active profile's overrides applied.
    let cfg = engine.effective_camera(&cfg);
    *handle.running.write().unwrap() = cfg.clone();
    camera_events::spawn_camera_events(engine.clone(), handle.clone(), &cfg);
    thread::spawn(move || {
        if native::wants_native(&cfg) {
            native::native_worker(handle, cfg, engine)
        } else {
            handle.state.write().unwrap().ingest = "ffmpeg";
            camera_worker(handle, cfg, engine)
        }
    });
}

fn spawn_camera(engine: Arc<Engine>, id: usize, cfg: CameraConfig) {
    let handle = make_handle(id, cfg.clone());
    let mut cams = engine.cameras.write().unwrap();
    cams.resize(id + 1, None);
    cams[id] = Some(handle.clone());
    spawn_worker(engine.clone(), handle.clone(), cfg);
}

#[cfg(test)]
mod overlay_var_tests {
    use super::*;

    fn engine() -> Arc<Engine> {
        Engine::for_test(toml::from_str("").unwrap())
    }

    #[test]
    fn plain_value_set_and_cleared() {
        let e = engine();
        e.set_overlay_var("outside_temp", "42°F");
        assert_eq!(e.overlay_vars_json().get("outside_temp").and_then(|x| x.as_str()), Some("42°F"));
        e.set_overlay_var("outside_temp", ""); // empty clears it
        assert!(e.overlay_vars_json().get("outside_temp").is_none());
    }

    #[test]
    fn json_payload_carries_an_expiry() {
        let e = engine();
        e.set_overlay_var("wind", r#"{"value":"7 mph","ttl":3600}"#);
        assert_eq!(e.overlay_vars_json().get("wind").and_then(|x| x.as_str()), Some("7 mph"));
        // Once past its expiry it drops out.
        e.overlay_vars.lock().unwrap().get_mut("wind").unwrap().expires = Some(0);
        assert!(e.overlay_vars_json().get("wind").is_none());
    }

    #[test]
    fn bad_names_rejected_and_values_flattened() {
        let e = engine();
        e.set_overlay_var("bad name", "x"); // a space is not allowed in a name
        e.set_overlay_var("ok", "line1\nline2\t!"); // newlines/tabs become spaces
        let v = e.overlay_vars_json();
        assert!(v.get("bad name").is_none());
        assert_eq!(v.get("ok").and_then(|x| x.as_str()), Some("line1 line2 !"));
    }
}

#[cfg(test)]
mod download_text_tests {
    use super::download_failure;

    #[test]
    fn a_failed_download_is_said_in_plain_words() {
        let e = anyhow::anyhow!("download plate_ocr.onnx: timeout: receive response");
        let t = download_failure("the plate reader", &e);
        assert_eq!(t, "Could not download the plate reader: the download site did not answer. Nothing is wrong with your cameras or this server. Try again in a few minutes.");
        assert!(!t.contains("onnx") && !t.contains("timeout"));
        assert!(download_failure("x", &anyhow::anyhow!("did not match its checksum")).contains("incomplete or damaged"));
    }
}
