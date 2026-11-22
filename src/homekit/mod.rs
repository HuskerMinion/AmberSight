//! Apple HomeKit: AmberSight's cameras in Apple Home (HAP over IP,
//! uncertified, like Homebridge and HAP-python). Each camera switched on for
//! Apple Home (`homekit = true`) has a motion sensor, snapshots and live
//! video. By default each camera is an accessory of its own ("standalone":
//! own port, mDNS record and pairing, as Homebridge and Scrypted publish
//! cameras); `[homekit] cameras = "bridged"` puts them all behind one bridge.
//!
//! Nothing runs while `[homekit] enabled` is off: no port, no mDNS.
//! Written from the HAP specification (non-commercial edition), with
//! HAP-NodeJS and HAP-python (both Apache-2.0) and the aiohomekit
//! controller as references; no code was copied from them.
//!
//!   tlv.rs          TLV8
//!   srp.rs          SRP-6a for pair-setup
//!   crypto.rs       HKDF, ChaCha20-Poly1305, Ed25519, X25519, session frames
//!   state.rs        identity, setup code, pairings (data folder, private)
//!   pairing.rs      pair-setup, pair-verify, /pairings
//!   accessories.rs  the accessory database
//!   http.rs         the small HTTP/1.1 parser
//!   server.rs       connections, sessions, the HAP resources, events
//!   stream.rs       live video (SRTP from ffmpeg)
//!   mdns.rs         the _hap._tcp advertisement
//!   api.rs          the admin API for Settings
//!
//! Lock order. All are std mutexes, held briefly and never across an
//! `.await` (the futures are spawned, so the compiler refuses a guard held
//! across one), a camera wake or an ffmpeg start. When two are held at
//! once, they are taken in this order, never the other way:
//!
//!   1. an accessory's `state` (`Instance::state`; the bridge's is also
//!      `Running::main`)
//!   2. the pairing `window` (shared by all accessories)
//!   3. the accessory `db`
//!   4. `Streams::tables`
//!   5. a live stream's `Ctl::child`
//!
//! and, apart from those: the unverified connections (`Gate::list`) before
//! an accessory's pair-setup `slot`. `sessions` is taken alone. Settings
//! (`engine.config`) and the engine's camera locks are let go before any of
//! these is taken, and are taken one at a time (`Host for Engine`).

pub mod accessories;
pub mod api;
pub mod crypto;
pub mod firewall;
pub mod http;
pub mod mdns;
pub mod pairing;
pub mod server;
pub mod srp;
pub mod state;
pub mod stream;
#[cfg(test)]
pub mod testclient;
pub mod tlv;

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use tokio_util::sync::CancellationToken;

use crate::engine::Engine;
use accessories::{iid, Cam, Database};
use server::{Instance, Kind};
use state::State;

fn default_port() -> u16 {
    51830
}

fn default_bind() -> String {
    "[::]".into()
}

fn is_default_bind(b: &String) -> bool {
    *b == default_bind()
}

fn default_rtp_ports() -> String {
    "51900-51999".into()
}

fn is_default_rtp_ports(c: &String) -> bool {
    *c == default_rtp_ports()
}

fn default_cameras() -> String {
    "standalone".into()
}

fn is_default_cameras(c: &String) -> bool {
    *c == default_cameras()
}

/// Ports above `port` standalone cameras may use (one each).
pub const MAX_STANDALONE: u16 = 64;

/// `[homekit]`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct HomekitConfig {
    /// Off by default; only cameras with `homekit = true` are shown.
    #[serde(default)]
    pub enabled: bool,
    /// The HAP TCP port (default 51830; Homebridge uses 51826). Standalone
    /// cameras use the next ports (port + 1, + 2, ...).
    #[serde(default = "default_port")]
    pub port: u16,
    /// The address HomeKit listens on, apart from the web page's: "[::]"
    /// (IPv6 and IPv4, or IPv4 alone where there is no IPv6), "0.0.0.0", or
    /// one address of this computer.
    #[serde(default = "default_bind", skip_serializing_if = "is_default_bind")]
    pub bind: String,
    /// "standalone" (each camera its own accessory, the default) or
    /// "bridged" (all cameras behind one bridge).
    #[serde(default = "default_cameras", skip_serializing_if = "is_default_cameras")]
    pub cameras: String,
    /// The UDP ports live video's RTCP comes back to (two per stream),
    /// "first-last". The Windows Firewall rule opens these.
    #[serde(default = "default_rtp_ports", skip_serializing_if = "is_default_rtp_ports")]
    pub rtp_ports: String,
    /// The name Apple Home shows for the bridge (default: the server's name).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
}

impl Default for HomekitConfig {
    fn default() -> Self {
        HomekitConfig { enabled: false, port: default_port(), bind: default_bind(), cameras: default_cameras(), rtp_ports: default_rtp_ports(), name: None }
    }
}

impl HomekitConfig {
    pub fn is_default(&self) -> bool {
        *self == HomekitConfig::default()
    }

    pub fn standalone(&self) -> bool {
        self.cameras != "bridged"
    }

    /// The port (and in standalone mode the ports after it) must not be one
    /// AmberSight already uses.
    pub fn check_port(port: u16, cfg: &crate::config::Config) -> Result<(), String> {
        if port == 0 {
            return Err("the HomeKit port must be between 1 and 65535".into());
        }
        let span = if cfg.homekit.standalone() { MAX_STANDALONE } else { 0 };
        if port.checked_add(span).is_none() {
            return Err(format!("the HomeKit port must be at most {} (standalone cameras use the ports after it)", u16::MAX - span));
        }
        let s = &cfg.server;
        let ours = [s.port, s.rtmp_port, cfg.restream.port, 5353];
        if let Some(p) = ours.iter().find(|p| **p != 0 && **p >= port && **p <= port + span) {
            return Err(format!("port {p} is already used (AmberSight's web page, RTMP input, RTSP sharing, or mDNS){}; choose another HomeKit port", if span > 0 { " and standalone cameras use the ports after the HomeKit port" } else { "" }));
        }
        Ok(())
    }

    /// The bind address must be one (or "[::]" / "0.0.0.0").
    pub fn check_bind(bind: &str) -> Result<(), String> {
        let b = bind.trim().trim_start_matches('[').trim_end_matches(']');
        b.parse::<std::net::IpAddr>().map(|_| ()).map_err(|_| format!("'{bind}' is not an address (use [::], 0.0.0.0 or one of this computer's addresses)"))
    }

    /// `rtp_ports` as (first, last): at least 8 ports, none below 1024.
    pub fn rtp_range(v: &str) -> Result<(u16, u16), String> {
        let bad = || format!("rtp_ports must be \"first-last\" (for example \"51900-51999\": at least 8 UDP ports from 1024 on), not \"{v}\"");
        let (a, b) = v.trim().split_once('-').ok_or_else(bad)?;
        let (a, b): (u16, u16) = (a.trim().parse().map_err(|_| bad())?, b.trim().parse().map_err(|_| bad())?);
        if a < 1024 || b < a || b - a < 7 || (a..=b).contains(&5353) {
            return Err(bad());
        }
        Ok((a, b))
    }

    pub fn check_cameras(v: &str) -> Result<(), String> {
        matches!(v, "standalone" | "bridged").then_some(()).ok_or_else(|| format!("cameras must be \"standalone\" or \"bridged\", not \"{v}\""))
    }
}

/// The bridge's name: `[homekit] name`, else the server's name (as HomeKit
/// takes it, else "AmberSight").
pub fn bridge_name(cfg: &crate::config::Config) -> String {
    let n = cfg.homekit.name.clone().filter(|n| !n.trim().is_empty()).or_else(|| cfg.server.name.clone().filter(|n| !n.trim().is_empty())).unwrap_or_else(crate::capture::proto::machine_name);
    accessories::clean_name(&n).unwrap_or_else(|| "AmberSight".into())
}

/// What the supervisor follows.
#[derive(Clone, Debug, PartialEq)]
pub struct Settings {
    pub enabled: bool,
    /// The UDP ports for live video's RTCP.
    pub rtp: (u16, u16),
    pub bind: String,
    pub port: u16,
    pub name: String,
    pub standalone: bool,
}

/// A camera as HomeKit sees it now.
#[derive(Clone, Debug)]
pub struct CamNow {
    pub id: usize,
    pub name: String,
    /// Shown in Apple Home.
    pub homekit: bool,
    pub motion: bool,
    pub online: bool,
    /// Live video can be sent (the built-in reader has the stream, or it is
    /// a battery camera that wakes for it).
    pub streamable: bool,
    /// `stream::picture_fingerprint` of the camera now.
    pub picture: String,
}

/// What HomeKit needs from the rest of AmberSight (the engine; a fake in
/// the tests).
pub trait Host: Send + Sync + 'static {
    fn settings(&self) -> Settings;
    fn state_dir(&self) -> PathBuf;
    /// Every camera, shown in Apple Home or not.
    fn cameras(&self) -> Vec<CamNow>;
    /// The latest picture of a camera shown in Apple Home (None for any
    /// other camera; empty while there is none yet).
    fn frame(&self, id: usize, name: &str) -> Option<Vec<u8>>;
    /// The live stream of a camera shown in Apple Home to send at this size
    /// (the smaller of main and sub that is at least as big, else the
    /// bigger). None: not shown, or not streaming now.
    fn live_source(&self, id: usize, name: &str, width: u32, height: u32) -> Option<stream::Source>;
    /// Whether to announce over mDNS.
    fn mdns(&self) -> bool {
        true
    }
    /// Whether to manage the Windows Firewall rule.
    fn firewall(&self) -> bool {
        true
    }
}

impl Host for Engine {
    fn settings(&self) -> Settings {
        let c = self.config.lock().unwrap();
        let rtp = HomekitConfig::rtp_range(&c.homekit.rtp_ports).unwrap_or_else(|e| {
            crate::transcode::warn_limited("homekit rtp_ports", &format!("HomeKit: {e}; 51900-51999 is used"));
            (51900, 51999)
        });
        Settings { rtp, enabled: c.homekit.enabled && c.homekit.port != 0, bind: c.homekit.bind.clone(), port: c.homekit.port, name: bridge_name(&c), standalone: c.homekit.standalone() }
    }
    fn state_dir(&self) -> PathBuf {
        state_dir(self)
    }
    fn cameras(&self) -> Vec<CamNow> {
        self.cameras
            .read()
            .unwrap()
            .iter()
            .flatten()
            .map(|h| {
                // (One camera lock at a time: never one held while taking another.)
                let (name, homekit, enabled, on_demand, second) = {
                    let cfg = h.cfg.read().unwrap();
                    // (A camera read by ffmpeg streams through a second ffmpeg; a
                    // capture device cannot be opened twice.)
                    let second = !crate::engine::reads_natively(&cfg) && crate::engine::second_reader_args(&cfg, false).is_some();
                    (cfg.name.clone(), cfg.homekit, cfg.enabled, cfg.on_demand, second)
                };
                let (motion, online) = {
                    let st = h.state.read().unwrap();
                    (st.motion, st.online)
                };
                let picture = {
                    let run = h.running.read().unwrap();
                    stream::picture_fingerprint(&crate::engine::recording_filters(&run), crate::engine::reads_natively(&run), &run.url, run.sub_url.as_deref())
                };
                let streamable = on_demand || second || h.live_main.info().is_some() || h.live_sub.info().is_some();
                CamNow { id: h.id, name, homekit, motion, online: online && enabled, streamable, picture }
            })
            .collect()
    }
    fn frame(&self, id: usize, name: &str) -> Option<Vec<u8>> {
        let h = self.camera(id)?;
        let shown = {
            let cfg = h.cfg.read().unwrap();
            cfg.name == name && cfg.homekit
        };
        if !shown {
            return None;
        }
        let f = h.state.read().unwrap().latest_frame.clone();
        Some(f)
    }
    fn live_source(&self, id: usize, name: &str, width: u32, height: u32) -> Option<stream::Source> {
        let h = self.camera(id)?;
        {
            let cfg = h.cfg.read().unwrap();
            if cfg.name != name || !cfg.homekit {
                return None;
            }
        }
        let on_demand = h.running.read().unwrap().on_demand;
        if on_demand {
            // A battery camera wakes for someone watching it.
            self.wake_camera(id, "an Apple Home live view");
        }
        let run = h.running.read().unwrap().clone();
        let native = crate::engine::reads_natively(&run);
        let fingerprint = stream::picture_fingerprint(&crate::engine::recording_filters(&run), native, &run.url, run.sub_url.as_deref());
        if !native {
            // Read by ffmpeg (burned-in text, ...): a second ffmpeg reads the
            // camera, with the same text.
            if !h.state.read().unwrap().online {
                return None;
            }
            let sub = height <= 480 && run.sub_url.as_deref().is_some_and(|u| !u.trim().is_empty());
            let input = crate::engine::second_reader_args(&run, sub)?;
            return Some(stream::Source { feed: stream::Feed::Direct { input, picture: crate::engine::recording_filters(&run) }, fps: 0.0, audio: run.audio, fingerprint, name: name.to_string() });
        }
        let hubs: Vec<(Arc<crate::ingest::live::LiveHub>, crate::ingest::live::LiveInfo)> = [h.live_main.clone(), h.live_sub.clone()].into_iter().filter_map(|hub| hub.info().map(|i| (hub, i))).collect();
        let fits = |i: &crate::ingest::live::LiveInfo| i.width >= width && i.height >= height;
        let pick = hubs.iter().filter(|(_, i)| fits(i)).min_by_key(|(_, i)| i.width * i.height).or_else(|| hubs.iter().max_by_key(|(_, i)| i.width * i.height))?;
        Some(stream::Source { feed: stream::Feed::Hub(pick.0.clone()), fps: pick.0.fps().unwrap_or(15.0), audio: pick.1.audio, fingerprint, name: name.to_string() })
    }
}

/// One accessory server that runs now, and its state folder.
#[derive(Clone)]
pub struct Served {
    pub inst: Arc<Instance>,
    pub dir: PathBuf,
}

/// What runs while HomeKit is on.
#[derive(Clone)]
pub struct Running {
    /// The bridge's state (the setup code, camera ids): served itself in
    /// bridged mode, kept for the cameras in standalone mode.
    pub main: Arc<Mutex<State>>,
    pub dir: PathBuf,
    pub served: Vec<Served>,
    /// The pairing window (Settings, "Pair a device"), shared by every accessory.
    pub window: Arc<Mutex<pairing::Window>>,
}

static RUNNING: Mutex<Option<Running>> = Mutex::new(None);

pub fn running() -> Option<Running> {
    RUNNING.lock().unwrap().clone()
}

#[derive(Clone, Default)]
pub struct Status {
    pub listening: bool,
    pub error: Option<String>,
}

static STATUS: Mutex<Status> = Mutex::new(Status { listening: false, error: None });

/// What to open in the firewall by hand, when AmberSight could not.
static FIREWALL_NOTE: Mutex<Option<String>> = Mutex::new(None);

pub fn firewall_note() -> Option<String> {
    FIREWALL_NOTE.lock().unwrap().clone()
}

pub fn status() -> Status {
    STATUS.lock().unwrap().clone()
}

fn set_status(listening: bool, error: Option<String>) {
    *STATUS.lock().unwrap() = Status { listening, error };
}

pub fn state_dir(engine: &Engine) -> PathBuf {
    engine.data_dir.join("homekit")
}

/// A standalone camera's state folder (by its slot, the camera's id - 1).
pub fn camera_dir(main: &Path, aid: u64) -> PathBuf {
    main.join(aid.saturating_sub(1).to_string())
}

/// Change the stored camera ids (the running state, else the file).
fn with_main(engine: &Engine, f: impl FnOnce(&mut State) -> bool) {
    let save = |st: &mut State| {
        if let Err(e) = st.save() {
            tracing::warn!("HomeKit: {e:#}");
        }
    };
    if let Some(r) = running() {
        let mut st = r.main.lock().unwrap();
        if f(&mut st) {
            save(&mut st);
        }
        return;
    }
    if let Ok(Some(mut st)) = State::load(&state_dir(engine)) {
        if f(&mut st) {
            save(&mut st);
        }
    }
}

/// A camera is about to be renamed: the new name gets the old one's id
/// already, so the moment between the rename and `rename_camera` never
/// gives it a new id (and a standalone camera a new port).
pub fn rename_begin(engine: &Engine, old: &str, new: &str) {
    with_main(engine, |st| st.alias(old, new));
}

/// The rename was not saved: the new name's id is taken back.
pub fn rename_abort(engine: &Engine, old: &str, new: &str) {
    with_main(engine, |st| st.unalias(old, new));
}

/// A camera was renamed (called after the rename was saved): it keeps its
/// accessory id and slot.
pub fn rename_camera(engine: &Engine, old: &str, new: &str) {
    with_main(engine, |st| st.rename(old, new));
}

/// Start the supervisor: it opens the ports and mDNS records while HomeKit
/// is on and follows the settings and the cameras.
pub fn start(engine: Arc<Engine>) {
    tokio::spawn(run(engine));
}

/// One running accessory server's handles.
struct Task {
    served: Served,
    port: u16,
    stop: CancellationToken,
    join: tokio::task::JoinHandle<()>,
}

type Wanted = Vec<(Kind, String, u16, PathBuf)>;

/// The accessories wanted now: (kind, name, port, state folder), and the
/// cameras that cannot have one (beyond the 64 ports after `port`).
fn wanted(s: &Settings, main: &Mutex<State>, dir: &Path, cams: &[CamNow]) -> (Wanted, Vec<String>) {
    if !s.standalone {
        return (vec![(Kind::Bridge, s.name.clone(), s.port, dir.to_path_buf())], Vec::new());
    }
    let mut skipped = Vec::new();
    let mut st = main.lock().unwrap();
    let before = st.aids.len();
    let mut out = Vec::new();
    for c in cams.iter().filter(|c| c.homekit) {
        let aid = st.aid_for(&c.name);
        let slot = aid.saturating_sub(1);
        if slot == 0 || slot > MAX_STANDALONE as u64 {
            skipped.push(c.name.clone());
            continue;
        }
        let name = accessories::clean_name(&c.name).unwrap_or_else(|| format!("Camera {aid}"));
        out.push((Kind::Camera(c.name.clone()), name, s.port + slot as u16, camera_dir(dir, aid)));
    }
    if st.aids.len() != before {
        if let Err(e) = st.save() {
            tracing::warn!("HomeKit: {e:#}");
        }
    }
    (out, skipped)
}

/// A standalone camera's state: its own identity and pairings, the shared
/// setup code.
fn camera_state(dir: &Path, code: &str) -> anyhow::Result<State> {
    let mut st = State::load_or_create(dir)?;
    if st.setup_code != code {
        st.setup_code = code.to_string();
        st.save()?;
    }
    Ok(st)
}

pub async fn run<H: Host>(host: Arc<H>) {
    let mut tasks: HashMap<Kind, Task> = HashMap::new();
    let mut errors: HashMap<Kind, (String, Instant)> = HashMap::new();
    let mut logged: HashMap<Kind, String> = HashMap::new();
    let mut main: Option<(Arc<Mutex<State>>, PathBuf)> = None;
    let mut generation: Option<Settings> = None;
    let gate = Arc::new(server::Gate::default());
    let window = Arc::new(Mutex::new(pairing::Window::default()));
    // The firewall rule as last made (None: not looked at yet, so the first
    // pass also removes a rule a killed process left behind).
    let mut fw: (Option<firewall::Ports>, Option<Instant>) = (None, None);
    loop {
        let s = host.settings();
        stream::set_rtp_ports(s.rtp);
        // Any change but the camera list: everything starts over.
        if generation.as_ref().is_some_and(|g| *g != s) || !s.enabled {
            stop_all(&mut tasks).await;
            errors.clear();
            if !s.enabled {
                main = None;
            }
        }
        generation = Some(s.clone());
        if !s.enabled {
            // (Switched off: a window open then does not come back with it.)
            window.lock().unwrap().close();
            *RUNNING.lock().unwrap() = None;
            set_status(false, None);
            reconcile_firewall(&*host, &mut fw, firewall::Ports::none(s.rtp)).await;
            tokio::time::sleep(Duration::from_secs(1)).await;
            continue;
        }
        let dir = host.state_dir();
        if main.is_none() {
            match State::load_or_create(&dir) {
                Ok(st) => main = Some((Arc::new(Mutex::new(st)), dir.clone())),
                Err(e) => {
                    set_status(false, Some(format!("{e:#}")));
                    tokio::time::sleep(Duration::from_secs(10)).await;
                    continue;
                }
            }
        }
        let (main_state, _) = main.clone().unwrap();
        let cams = host.cameras();
        let (want, skipped) = wanted(&s, &main_state, &dir, &cams);
        if !skipped.is_empty() && logged.get(&Kind::Bridge).map(String::as_str) != Some("skipped") {
            tracing::warn!("HomeKit: {} cannot be shown on its own: at most {MAX_STANDALONE} cameras have their own port (use bridged mode for more)", skipped.join(", "));
            logged.insert(Kind::Bridge, "skipped".into());
        }
        // Stop what is no longer wanted (or moved).
        let keys: Vec<Kind> = tasks.keys().cloned().collect();
        for k in keys {
            if !want.iter().any(|(wk, _, port, _)| *wk == k && tasks.get(&k).is_some_and(|t| t.port == *port)) {
                if let Some(t) = tasks.remove(&k) {
                    stop_task(t).await;
                }
            }
        }
        errors.retain(|k, _| want.iter().any(|(wk, ..)| wk == k));
        // Start what is missing (a failed one again after 10 s).
        for (kind, name, port, sdir) in &want {
            if tasks.contains_key(kind) || errors.get(kind).is_some_and(|(_, at)| at.elapsed() < Duration::from_secs(10)) {
                continue;
            }
            let state = match kind {
                Kind::Bridge => Ok(main_state.clone()),
                Kind::Camera(_) => {
                    let code = main_state.lock().unwrap().setup_code.clone();
                    camera_state(sdir, &code).map(|st| Arc::new(Mutex::new(st)))
                }
            };
            let started = state.and_then(|st| {
                let (l, listen) = server::bind(&s.bind, *port)?;
                Ok((st, l, listen))
            });
            match started {
                Ok((st, l, listen)) => {
                    errors.remove(kind);
                    logged.remove(kind);
                    let inst = Arc::new(Instance::new_in(kind.clone(), name.clone(), *port, st, server::Limits::default(), gate.clone(), window.clone()));
                    sync_with(&inst, &cams);
                    tracing::info!("HomeKit: {} listening on {}", describe(kind, name), server::local(&l).map(|a| a.to_string()).unwrap_or_else(|| port.to_string()));
                    let stop = CancellationToken::new();
                    let dynhost: Arc<dyn Host> = host.clone();
                    let (i2, st2, mdns) = (inst.clone(), stop.clone(), host.mdns());
                    let join = tokio::spawn(async move {
                        let ad = mdns.then(|| tokio::spawn(advertise(i2.clone(), listen, st2.child_token())));
                        server::accept_loop(i2.clone(), dynhost, l, st2.child_token()).await;
                        if let Some(a) = ad {
                            let _ = a.await;
                        }
                    });
                    tasks.insert(kind.clone(), Task { served: Served { inst, dir: sdir.clone() }, port: *port, stop, join });
                }
                Err(e) => {
                    let msg = format!("{}: {e:#}", describe(kind, name));
                    if logged.get(kind) != Some(&msg) {
                        tracing::warn!("HomeKit: {msg}");
                        logged.insert(kind.clone(), msg.clone());
                    }
                    errors.insert(kind.clone(), (msg, Instant::now()));
                }
            }
        }
        reconcile_firewall(&*host, &mut fw, firewall::Ports { tcp: tasks.values().map(|t| t.port).collect(), rtp: s.rtp }).await;
        *RUNNING.lock().unwrap() = Some(Running { main: main_state.clone(), dir: dir.clone(), served: tasks.values().map(|t| t.served.clone()).collect(), window: window.clone() });
        let mut err: Vec<String> = errors.values().map(|(m, _)| m.clone()).collect();
        if !skipped.is_empty() {
            err.push(format!("{} not shown: at most {MAX_STANDALONE} cameras can be on their own (use \"Behind one bridge\" for more)", skipped.join(", ")));
        }
        set_status(!tasks.is_empty() || want.is_empty(), (!err.is_empty()).then(|| err.join("; ")));
        // Follow the cameras twice a second until the next look at the settings.
        for _ in 0..2 {
            tokio::time::sleep(Duration::from_millis(500)).await;
            close_expired_window(&window, tasks.values().map(|t| &t.served.inst));
            let cams = host.cameras();
            for t in tasks.values() {
                sync_with(&t.served.inst, &cams);
            }
        }
    }
}

/// Make the Windows Firewall rule match these ports (none: no rule).
/// `fw`: the rule as last made, and when it last failed (tried again 30 s
/// later, not on every pass).
async fn reconcile_firewall<H: Host + ?Sized>(host: &H, fw: &mut (Option<firewall::Ports>, Option<Instant>), want: firewall::Ports) {
    let want = want.sorted();
    if !host.firewall() || fw.0.as_ref() == Some(&want) || fw.1.is_some_and(|t| t.elapsed() < Duration::from_secs(30)) {
        return;
    }
    let w = want.clone();
    let r = tokio::task::spawn_blocking(move || firewall::apply(&w)).await.unwrap_or(Ok(()));
    match r {
        Ok(()) => {
            *FIREWALL_NOTE.lock().unwrap() = None;
            *fw = (Some(want), None);
        }
        Err(note) => {
            crate::transcode::warn_limited("homekit firewall", &format!("HomeKit: {note}"));
            *FIREWALL_NOTE.lock().unwrap() = Some(note);
            // (Not stored as made: tried again.)
            fw.1 = Some(Instant::now());
        }
    }
}

fn describe(kind: &Kind, name: &str) -> String {
    match kind {
        Kind::Bridge => format!("bridge \"{name}\""),
        Kind::Camera(_) => format!("camera \"{name}\""),
    }
}

async fn stop_task(t: Task) {
    t.stop.cancel();
    t.served.inst.end_all();
    t.served.inst.streams.end_all();
    let _ = tokio::time::timeout(Duration::from_secs(5), t.join).await;
}

async fn stop_all(tasks: &mut HashMap<Kind, Task>) {
    let any = !tasks.is_empty();
    for (_, t) in tasks.drain() {
        stop_task(t).await;
    }
    if any {
        tracing::info!("HomeKit stopped");
    }
}

/// A pairing window past its end: forgotten, its pair-setup in progress
/// ended, and its accessory announced as not pairable again.
fn close_expired_window<'a>(window: &Mutex<pairing::Window>, insts: impl Iterator<Item = &'a Arc<Instance>>) {
    let Some(target) = window.lock().unwrap().expire() else { return };
    tracing::info!("HomeKit: the pairing window closed (5 minutes)");
    for i in insts.filter(|i| i.kind == target) {
        i.end_setup(None);
        i.advertise.notify_one();
    }
}

/// After Settings changed the pairing window: what was there before, when
/// it was for another accessory than `now` (None: closed now) or past its
/// end (Stop or Open came before the supervisor saw it lapse), has its
/// pair-setup in progress ended and is announced again. The accessory, if any.
fn window_left<'a>(was: Option<pairing::Open>, now: Option<&Kind>, insts: impl Iterator<Item = &'a Arc<Instance>>) -> Option<Kind> {
    let o = was.filter(|o| Some(&o.target) != now || o.until <= std::time::Instant::now())?;
    for i in insts.filter(|i| i.kind == o.target) {
        i.end_setup(None);
        i.advertise.notify_one();
    }
    Some(o.target)
}

/// The mDNS record: `sf` = 1 (pairable) only while the pairing window is
/// open for this accessory, so Home lists it only then.
fn txt(inst: &Instance) -> mdns::Txt {
    let pairable = inst.pairable();
    let st = inst.state.lock().unwrap();
    mdns::Txt { name: inst.name.clone(), device_id: st.device_id.clone(), config_number: st.config_number, pairable, setup_id: st.setup_id.clone(), category: inst.category() }
}

/// Keep the mDNS record up to date until `stop`.
async fn advertise(inst: Arc<Instance>, listen: server::Listen, stop: CancellationToken) {
    let first = txt(&inst);
    let port = inst.port;
    let mut ad = match tokio::task::spawn_blocking(move || mdns::Advert::start(port, listen, &first)).await {
        Ok(Ok(a)) => a,
        Ok(Err(e)) => {
            tracing::warn!("HomeKit: {e:#}; Apple devices will not find it");
            stop.cancelled().await;
            return;
        }
        Err(_) => return,
    };
    loop {
        tokio::select! {
            _ = stop.cancelled() => break,
            _ = inst.advertise.notified() => {
                // (Several changes in a row become one announcement.)
                tokio::time::sleep(Duration::from_millis(300)).await;
                let t = txt(&inst);
                ad = match tokio::task::spawn_blocking(move || { let r = ad.update(&t); (ad, r) }).await {
                    Ok((a, r)) => {
                        if let Err(e) = r {
                            tracing::warn!("HomeKit: {e:#}");
                        }
                        a
                    }
                    Err(_) => return,
                };
            }
        }
    }
    let _ = tokio::task::spawn_blocking(move || ad.stop()).await;
}

/// StreamingStatus of a camera: 0 available, 1 in use (no more streams),
/// 2 unavailable (offline, or no stream to send: read by ffmpeg).
fn streaming_status(online: bool, streams: usize) -> u8 {
    if !online {
        2
    } else if streams >= stream::PER_CAMERA {
        1
    } else {
        0
    }
}

/// Bring the database in line with the cameras (rebuilt when cameras were
/// switched on or off, renamed or re-added; c# bumped when what controllers
/// cache changed), then pass motion, online and streaming changes on as
/// events. True when the database was rebuilt.
pub fn sync_with(inst: &Instance, all: &[CamNow]) -> bool {
    use base64::Engine as _;
    let mut changes: Vec<(u64, u64, serde_json::Value)> = Vec::new();
    let rebuilt;
    inst.streams.tidy();
    {
        let mut st = inst.state.lock().unwrap();
        let before = st.aids.len();
        // (accessory, motion, online, streamable)
        let mut cams: Vec<(Cam, bool, bool, bool)> = match &inst.kind {
            Kind::Bridge => all.iter().filter(|c| c.homekit).map(|c| (Cam { aid: st.aid_for(&c.name), id: c.id, name: c.name.clone() }, c.motion, c.online, c.streamable)).collect(),
            Kind::Camera(name) => all.iter().filter(|c| c.homekit && c.name == *name).take(1).map(|c| (Cam { aid: 1, id: c.id, name: c.name.clone() }, c.motion, c.online, c.streamable)).collect(),
        };
        // (The database lists cameras by accessory id.)
        cams.sort_by_key(|(c, ..)| c.aid);
        // A camera whose picture settings changed (burned-in text, address)
        // ends its live views: none goes on showing the old picture.
        for c in all.iter().filter(|c| c.homekit) {
            let aid = match &inst.kind {
                Kind::Bridge => st.aids.get(&c.name).copied(),
                Kind::Camera(name) => (*name == c.name).then_some(1),
            };
            if let Some(aid) = aid {
                inst.streams.end_changed(aid, &c.picture);
            }
        }
        let mut save = st.aids.len() != before;
        let mut db = inst.db.lock().unwrap();
        let list: Vec<Cam> = cams.iter().map(|(c, ..)| c.clone()).collect();
        rebuilt = db.cameras() != list && !(inst.kind != Kind::Bridge && list.is_empty());
        if rebuilt {
            let mut fresh = match &inst.kind {
                Kind::Bridge => Database::bridge(&inst.name, &st.device_id, &list),
                Kind::Camera(_) => Database::standalone(&list[0], &st.device_id),
            };
            fresh.keep_values_from(&db);
            *db = fresh;
            // Live views of cameras no longer in it end.
            let aids: Vec<u64> = db.cameras().iter().map(|c| c.aid).collect();
            inst.streams.keep_only(&aids);
        }
        if !db.accessories.is_empty() && st.note_db(&db.fingerprint()) {
            save = true;
            inst.advertise.notify_one();
        }
        if save {
            if let Err(e) = st.save() {
                tracing::warn!("HomeKit: {e:#}");
            }
        }
        for (cam, motion, online, streamable) in &cams {
            for (i, v) in [(iid::MOTION_DETECTED, *motion), (iid::STATUS_ACTIVE, *online)] {
                if let Some(ch) = db.get_mut(cam.aid, i) {
                    if ch.value.as_bool() != Some(v) {
                        ch.value = v.into();
                        changes.push((cam.aid, i, v.into()));
                    }
                }
            }
            let ss: serde_json::Value = base64::engine::general_purpose::STANDARD.encode(accessories::streaming_status(streaming_status(*online && *streamable, inst.streams.count(cam.aid)))).into();
            if let Some(ch) = db.get_mut(cam.aid, iid::STREAMING_STATUS) {
                if ch.value != ss {
                    ch.value = ss.clone();
                    changes.push((cam.aid, iid::STREAMING_STATUS, ss));
                }
            }
        }
    }
    inst.notify(&changes, None);
    rebuilt
}

/// Forget every paired Apple device of every accessory, with a new setup
/// code (shared by all) and new identities. Works while HomeKit is off too.
pub fn reset_all(dir: &Path) -> anyhow::Result<()> {
    let r = running().filter(|r| r.dir == dir);
    // The bridge's state first: its new code is everyone's.
    let code = match r.as_ref().and_then(|r| r.served.iter().find(|s| s.inst.kind == Kind::Bridge).cloned()) {
        Some(b) => {
            b.inst.reset(None)?;
            let c = b.inst.state.lock().unwrap().setup_code.clone();
            c
        }
        None => match &r {
            Some(r) => {
                let mut st = r.main.lock().unwrap();
                st.reset();
                st.save()?;
                st.setup_code.clone()
            }
            None => match State::load(dir) {
                Ok(Some(mut st)) => {
                    st.reset();
                    st.save()?;
                    st.setup_code.clone()
                }
                Ok(None) => return Ok(()),
                Err(_) => {
                    let _ = std::fs::remove_file(State::file(dir));
                    State::load_or_create(dir)?.setup_code.clone()
                }
            },
        },
    };
    // Every standalone camera's state (running or not).
    let Ok(entries) = std::fs::read_dir(dir) else { return Ok(()) };
    for e in entries.flatten() {
        let p = e.path();
        if !p.is_dir() || !p.file_name().and_then(|n| n.to_str()).is_some_and(|n| n.chars().all(|c| c.is_ascii_digit())) {
            continue;
        }
        match r.as_ref().and_then(|r| r.served.iter().find(|s| s.dir == p).cloned()) {
            Some(s) => s.inst.reset(Some(&code))?,
            None => {
                if let Ok(Some(mut st)) = State::load(&p) {
                    st.reset();
                    st.setup_code = code.clone();
                    st.save()?;
                }
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_are_off_and_not_written() {
        let c = HomekitConfig::default();
        assert!(!c.enabled && c.port == 51830 && c.bind == "[::]" && c.name.is_none() && c.is_default() && c.standalone());
        let parsed: HomekitConfig = toml::from_str("").unwrap();
        assert_eq!(parsed, c);
        let on: HomekitConfig = toml::from_str("enabled = true\nname = \"Cabin\"").unwrap();
        assert!(on.enabled && on.name.as_deref() == Some("Cabin") && !on.is_default());
        let text = toml::to_string(&on).unwrap();
        assert!(!text.contains("bind") && !text.contains("cameras"));
        let b: HomekitConfig = toml::from_str("cameras = \"bridged\"").unwrap();
        assert!(!b.standalone());
        assert!(HomekitConfig::check_cameras("bridged").is_ok() && HomekitConfig::check_cameras("x").is_err());
        assert_eq!(HomekitConfig::rtp_range("51900-51999"), Ok((51900, 51999)));
        for bad in ["", "51900", "51999-51900", "100-200", "51900-51902", "5350-5360", "a-b"] {
            assert!(HomekitConfig::rtp_range(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn port_and_bind_are_checked() {
        let mut cfg: crate::config::Config = toml::from_str("").unwrap();
        assert!(HomekitConfig::check_port(51830, &cfg).is_ok());
        assert!(HomekitConfig::check_port(0, &cfg).is_err());
        assert!(HomekitConfig::check_port(cfg.server.port, &cfg).is_err());
        assert!(HomekitConfig::check_port(cfg.restream.port, &cfg).is_err());
        assert!(HomekitConfig::check_port(5353, &cfg).is_err());
        // Standalone cameras use the ports after it: the web page's must not be among them.
        assert!(HomekitConfig::check_port(cfg.server.port - 10, &cfg).is_err());
        assert!(HomekitConfig::check_port(65500, &cfg).is_err());
        cfg.homekit.cameras = "bridged".into();
        assert!(HomekitConfig::check_port(cfg.server.port - 10, &cfg).is_ok());
        for ok in ["[::]", "::", "0.0.0.0", "192.168.1.5", "[fe80::1]"] {
            assert!(HomekitConfig::check_bind(ok).is_ok(), "{ok}");
        }
        for bad in ["", "localhost", "1.2.3", "::x"] {
            assert!(HomekitConfig::check_bind(bad).is_err(), "{bad}");
        }
    }

    fn cam(id: usize, name: &str, homekit: bool, motion: bool) -> CamNow {
        CamNow { id, name: name.into(), homekit, motion, online: true, streamable: true, picture: String::new() }
    }

    #[test]
    fn sync_rebuilds_only_when_cameras_change() {
        let dir = std::env::temp_dir().join(format!("as-hk-sync-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let mut st = State::load_or_create(&dir).unwrap();
        // Ids given in another order than the engine lists the cameras.
        assert_eq!(st.aid_for("B"), 2);
        assert_eq!(st.aid_for("A"), 3);
        let inst = Instance::new(Kind::Bridge, "Test".into(), 0, Arc::new(Mutex::new(st)), server::Limits::default());
        let cams = || vec![cam(0, "A", true, false), cam(1, "B", true, false), cam(2, "C", false, false)];
        assert!(sync_with(&inst, &cams()));
        let c1 = inst.state.lock().unwrap().config_number;
        assert!(!sync_with(&inst, &cams()), "nothing changed: no rebuild");
        assert!(!sync_with(&inst, &[cam(0, "A", true, true), cam(1, "B", true, false)]), "motion is a value, not a rebuild");
        assert_eq!(inst.db.lock().unwrap().get(3, iid::MOTION_DETECTED).unwrap().value, true);
        assert_eq!(inst.state.lock().unwrap().config_number, c1);
        // An offline camera: StreamingStatus says unavailable.
        let mut off = cam(0, "A", true, false);
        off.online = false;
        sync_with(&inst, &[off, cam(1, "B", true, false)]);
        use base64::Engine as _;
        let ss = inst.db.lock().unwrap().get(3, iid::STREAMING_STATUS).unwrap().value.clone();
        assert_eq!(base64::engine::general_purpose::STANDARD.decode(ss.as_str().unwrap()).unwrap(), vec![1, 1, 2]);
        // A hidden camera is never in the database; showing it rebuilds and bumps c#.
        assert!(inst.db.lock().unwrap().cameras().iter().all(|c| c.name != "C"));
        assert!(sync_with(&inst, &[cam(0, "A", true, false), cam(1, "B", true, false), cam(2, "C", true, false)]));
        assert_eq!(inst.state.lock().unwrap().config_number, c1 + 1);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn sf_follows_the_pairing_window() {
        let dir = std::env::temp_dir().join(format!("as-hk-sf-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let window = Arc::new(Mutex::new(pairing::Window::default()));
        let gate = Arc::new(server::Gate::default());
        let mk = |kind: Kind, sub: &str| Arc::new(Instance::new_in(kind, "X".into(), 0, Arc::new(Mutex::new(State::load_or_create(&dir.join(sub)).unwrap())), server::Limits::default(), gate.clone(), window.clone()));
        let (a, b) = (mk(Kind::Camera("A".into()), "a"), mk(Kind::Camera("B".into()), "b"));
        let sf = |i: &Instance| txt(i).pairs().into_iter().find(|(k, _)| k == "sf").unwrap().1;
        // Unpaired, but no window: not pairable (Home does not list it).
        assert_eq!((sf(&a), sf(&b)), ("0".into(), "0".into()));
        // Open for camera A: A only.
        window.lock().unwrap().open(Kind::Camera("A".into()), pairing::WINDOW);
        assert_eq!((sf(&a), sf(&b)), ("1".into(), "0".into()));
        // Moved to B (one window at a time).
        window.lock().unwrap().open(Kind::Camera("B".into()), pairing::WINDOW);
        assert_eq!((sf(&a), sf(&b)), ("0".into(), "1".into()));
        // Stopped.
        window.lock().unwrap().close();
        assert_eq!(sf(&b), "0");
        // Past its end: 0, and the supervisor forgets it.
        window.lock().unwrap().open(Kind::Camera("A".into()), pairing::WINDOW);
        window.lock().unwrap().age(pairing::WINDOW);
        assert_eq!(sf(&a), "0");
        close_expired_window(&window, [&a, &b].into_iter());
        assert!(window.lock().unwrap().expire().is_none());
        // Paired: 0 even with the window open.
        window.lock().unwrap().open(Kind::Camera("A".into()), pairing::WINDOW);
        a.state.lock().unwrap().pairings.push(state::Pairing { id: "C".into(), ltpk: "00".repeat(32), admin: true });
        assert_eq!(sf(&a), "0");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn standalone_cameras_get_stable_ports_and_the_shared_code() {
        let dir = std::env::temp_dir().join(format!("as-hk-sa-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let main = Mutex::new(State::load_or_create(&dir).unwrap());
        let s = Settings { enabled: true, rtp: (51900, 51999), bind: "[::]".into(), port: 51830, name: "NVR".into(), standalone: true };
        let (w, _) = wanted(&s, &main, &dir, &[cam(0, "B", true, false), cam(1, "A", true, false), cam(2, "Hidden", false, false)]);
        assert_eq!(w.iter().map(|(_, n, p, _)| (n.as_str(), *p)).collect::<Vec<_>>(), vec![("B", 51831), ("A", 51832)]);
        // Reordered, one switched off: the others keep their ports.
        let (w, _) = wanted(&s, &main, &dir, &[cam(0, "A", true, false), cam(1, "B", false, false)]);
        assert_eq!(w.iter().map(|(_, n, p, _)| (n.as_str(), *p)).collect::<Vec<_>>(), vec![("A", 51832)]);
        let code = main.lock().unwrap().setup_code.clone();
        let cs = camera_state(&w[0].3, &code).unwrap();
        assert_eq!(cs.setup_code, code);
        assert_ne!(cs.device_id, main.lock().unwrap().device_id);
        // Bridged: just the bridge.
        let (b, _) = wanted(&Settings { standalone: false, ..s.clone() }, &main, &dir, &[cam(0, "A", true, false)]);
        assert_eq!(b.len(), 1);
        assert_eq!(b[0].0, Kind::Bridge);
        // A standalone instance's database is the one camera.
        let inst = Instance::new(Kind::Camera("A".into()), "A".into(), 0, Arc::new(Mutex::new(cs)), server::Limits::default());
        assert!(sync_with(&inst, &[cam(0, "B", true, false), cam(5, "A", true, true)]));
        let db = inst.db.lock().unwrap().cameras();
        assert_eq!(db, vec![Cam { aid: 1, id: 5, name: "A".into() }]);
        assert_eq!(inst.category(), 17);
        // A live view ends when the camera's picture settings change.
        inst.streams.fake_running(1, "");
        sync_with(&inst, &[cam(5, "A", true, false)]);
        assert_eq!(inst.streams.count(1), 1);
        let mut texted = cam(5, "A", true, false);
        texted.picture = "texted".into();
        sync_with(&inst, &[texted]);
        assert_eq!(inst.streams.count(1), 0);
        // A rename keeps the slot, also when the new name is seen before the rename is told.
        main.lock().unwrap().alias("A", "A2");
        let (w2, _) = wanted(&s, &main, &dir, &[cam(1, "A2", true, false)]);
        assert_eq!(w2[0].2, 51832);
        main.lock().unwrap().rename("A", "A2");
        assert_eq!(main.lock().unwrap().aid_for("A2"), 3);
        // Beyond 64 cameras: told, not shown.
        {
            let mut st = main.lock().unwrap();
            st.aids.insert("Far".into(), 70);
        }
        let (_, skipped) = wanted(&s, &main, &dir, &[cam(9, "Far", true, false)]);
        assert_eq!(skipped, vec!["Far".to_string()]);
        // Reset: a new code for all, shared again.
        reset_all(&dir).unwrap();
        let new_code = State::load(&dir).unwrap().unwrap().setup_code.clone();
        assert_eq!(State::load(&w[0].3).unwrap().unwrap().setup_code, new_code);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn disabled_opens_nothing_and_enabled_listens() {
        let dir = std::env::temp_dir().join(format!("as-hk-run-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        // A free port, then let it go.
        let port = std::net::TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port();
        let host = Arc::new(server::tests::FakeHost { dir: dir.clone(), settings: Mutex::new(Settings { enabled: false, rtp: (51900, 51999), bind: "127.0.0.1".into(), port, name: "Test".into(), standalone: false }) });
        let task = tokio::spawn(run(host.clone()));
        tokio::time::sleep(Duration::from_millis(1500)).await;
        assert!(tokio::net::TcpStream::connect(("127.0.0.1", port)).await.is_err(), "off: no listener");
        assert!(!dir.exists(), "off: nothing written");
        host.settings.lock().unwrap().enabled = true;
        let mut up = false;
        for _ in 0..30 {
            tokio::time::sleep(Duration::from_millis(100)).await;
            if tokio::net::TcpStream::connect(("127.0.0.1", port)).await.is_ok() {
                up = true;
                break;
            }
        }
        assert!(up, "on: listening");
        host.settings.lock().unwrap().enabled = false;
        let mut down = false;
        for _ in 0..40 {
            tokio::time::sleep(Duration::from_millis(100)).await;
            if tokio::net::TcpStream::connect(("127.0.0.1", port)).await.is_err() {
                down = true;
                break;
            }
        }
        assert!(down, "off again: closed");
        task.abort();
        let _ = std::fs::remove_dir_all(&dir);
    }
}
