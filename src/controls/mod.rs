//! Camera controls: the spotlight (white light), the siren, the infrared
//! lights and the status light, for cameras that have them.
//!
//! Rules this follows everywhere:
//! - What a camera has is asked of the camera (and kept for a while); only
//!   what it has is offered.
//! - Turning the spotlight on from AmberSight is separate from the camera's
//!   own light mode: the light is on for a time, then the camera gets back
//!   exactly what it had (state, mode, brightness).
//! - Every action that does something has a time limit (the light, the
//!   siren), also when AmberSight stops or restarts in between.
//! - A state that cannot be read is "unknown", never a guess.
//! - Every change is read back: some cameras answer "done" and change nothing.
//! - Battery cameras are never woken for controls and never asked for their
//!   state on a timer; what they have is read while they are awake anyway.
//!
//! Brands and ways in are behind [`Driver`] (Reolink's web API in
//! reolink_drv.rs, Reolink's own port 9000 protocol in baichuan_drv.rs); the
//! checks, timers and records here are the same for all.

pub mod automation;
mod baichuan_drv;
pub mod http;
mod reolink_drv;
pub mod session;
pub mod siren_sound;
#[cfg(test)]
pub(crate) mod fake;
#[cfg(test)]
pub(crate) mod fake_bc;
#[cfg(test)]
mod tests;
#[cfg(test)]
mod tests_bc;
#[cfg(test)]
mod tests_sound;

use std::collections::{HashMap, VecDeque};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::Result;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use tracing::{info, warn};

use crate::config::CameraConfig;
use crate::engine::{CameraHandle, Engine};

/// The spotlight turned on from AmberSight: at most this long, this long when not said.
pub const LIGHT_MAX_SECS: u32 = 3600;
pub const LIGHT_DEFAULT_SECS: u32 = 300;
/// The siren: at most this long, this long when not said.
pub const SIREN_MAX_SECS: u32 = 30;
pub const SIREN_DEFAULT_SECS: u32 = 5;
/// One camera's siren starts at most once in this long (stopping: any time).
pub const SIREN_GAP: Duration = Duration::from_secs(10);
/// One account starts sirens at most this often in `SIREN_WINDOW`.
pub const SIREN_PER_USER: usize = 6;
pub const SIREN_WINDOW: Duration = Duration::from_secs(10 * 60);
/// The spotlight is switched at most once in this long (pages, API, MQTT).
pub const LIGHT_GAP: Duration = Duration::from_secs(1);
/// What a camera has is asked again after this long.
const PROBE_EVERY: Duration = Duration::from_secs(6 * 3600);
/// After a failed ask, not again before this.
const PROBE_RETRY: Duration = Duration::from_secs(2 * 60);
/// A state read this recently is not read again for a page.
const READ_FRESH: Duration = Duration::from_secs(3);
/// A siren's stop that keeps failing is given up after this (the camera stops
/// it by itself well before: its longest play plus a margin).
#[cfg(not(test))]
const SIREN_STOP_GIVE_UP: Duration = Duration::from_secs(SIREN_MAX_SECS as u64 + 60);
#[cfg(test)]
const SIREN_STOP_GIVE_UP: Duration = Duration::from_secs(4);
/// A light whose restore failed is tried again this often, for at most a day.
const RESTORE_RETRY: Duration = Duration::from_secs(30);
const RESTORE_GIVE_UP_MS: i64 = 24 * 3600 * 1000;
/// ui_kv scope for what must survive a restart: lights to give back, sirens to stop.
const SCOPE: &str = "controls";

// ------------------------------------------------------------------ model

/// The spotlight, as the camera describes it.
#[derive(Clone, Debug, Default, PartialEq, Serialize)]
pub struct LightCaps {
    /// It can be switched on and off.
    pub on_off: bool,
    /// Its brightness range, when it can be set.
    pub brightness: Option<(u32, u32)>,
    /// The camera's own light modes, when it lists their numbers (Reolink
    /// cameras seen so far do not: then none is offered to pick, and the mode
    /// the camera has is only kept and given back).
    pub modes: Vec<i64>,
    /// What the camera says its own modes can do: on when something is seen
    /// at night, always on at night, on a schedule.
    pub smart: bool,
    pub keep_on: bool,
    pub schedule: bool,
}

/// What a camera has.
#[derive(Clone, Debug, Default, PartialEq, Serialize)]
pub struct Abilities {
    pub light: Option<LightCaps>,
    pub siren: bool,
    /// Infrared light settings the camera lists ("Auto", "Off", ...); empty: none.
    pub ir: Vec<String>,
    /// Status light settings ("On", "Off"); empty: none.
    pub status_led: Vec<String>,
    /// A doorbell's light ("On", "Off", "KeepOn", "KeepOff" as the camera lists them); empty: none.
    pub doorbell_led: Vec<String>,
    /// The camera has a spotlight that is not switched the way it is reached
    /// (over Reolink port 9000): said on its page, never offered.
    pub light_unswitched: bool,
}

impl Abilities {
    pub fn any(&self) -> bool {
        self.light.is_some() || self.siren || !self.ir.is_empty() || !self.status_led.is_empty() || !self.doorbell_led.is_empty() || self.light_unswitched
    }
}

/// The spotlight's state (None: not known / not to change).
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct LightState {
    pub on: Option<bool>,
    pub brightness: Option<u32>,
    pub mode: Option<i64>,
}

/// What a camera says about its controls now.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Reading {
    pub light: Option<LightState>,
    pub ir: Option<String>,
    pub status_led: Option<String>,
    pub doorbell_led: Option<String>,
}

/// The camera surely did not do it: it answered with a refusal, or the
/// request never reached it. (Any other failure, such as an answer that was
/// lost or late, may mean it did.)
#[derive(Debug)]
pub struct NotDone(pub String);

impl std::fmt::Display for NotDone {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for NotDone {}

/// Whether `e` says for sure that the camera did not do what was asked.
pub fn not_done(e: &anyhow::Error) -> bool {
    e.chain().any(|c| c.is::<NotDone>() || c.is::<baichuan_drv::Refusal>() || c.is::<baichuan_drv::NotSent>()) || crate::reolink::cam_error(e).is_some()
}

/// One brand's way to reach a camera's controls. Calls block (a few
/// seconds at most) and are serialized per device by the brand's session.
pub trait Driver: Send + Sync {
    /// The address it talks to (kept, so later commands go straight there).
    fn base(&self) -> String;
    fn probe(&self) -> Result<Abilities>;
    fn read(&self, ab: &Abilities) -> Result<Reading>;
    /// As `read`, for a page: None when the device is busy (an action goes first).
    fn read_if_idle(&self, ab: &Abilities) -> Option<Result<Reading>> {
        Some(self.read(ab))
    }
    /// Change what is set in `w` (None fields stay as they are).
    fn set_light(&self, w: &LightState) -> Result<()>;
    /// Sound the siren for about `secs` (the camera stops it by itself).
    fn siren_play(&self, secs: u32) -> Result<()>;
    fn siren_stop(&self) -> Result<()>;
    fn set_ir(&self, state: &str) -> Result<()>;
    fn set_status_led(&self, state: &str) -> Result<()>;
    /// What the device says it is (a camera, a hub, a recorder).
    fn device_kind(&self) -> Result<DevKind> {
        Ok(DevKind::default())
    }
    fn set_doorbell_led(&self, _state: &str) -> Result<()> {
        anyhow::bail!("this camera has no doorbell light")
    }
}

/// The camera's own spotlight modes in words: only what the camera itself
/// names. Reolink cameras list no names for their mode numbers (and the
/// vendor's guides disagree about what 0 means), so none is named or guessed:
/// pages show "Camera's own setting", and the mode is kept and given back
/// exactly as read.
pub fn mode_label(_raw: i64) -> Option<&'static str> {
    None
}

// --------------------------------------------------------------- targets

/// How to reach a camera's controls.
#[derive(Clone)]
pub struct Target {
    /// Changes when anything that reaches the camera changes (its abilities are asked again).
    pub key: String,
    /// The device and channel it reaches (address, account, channel; nothing
    /// from the password): what a light turned on there belongs to.
    pub place: String,
    pub brand: &'static str,
    pub bases: Vec<String>,
    pub user: String,
    pass: String,
    pub channel: u32,
    /// A battery camera: never woken, never asked on a timer.
    pub battery: bool,
    /// A channel of a hub or recorder: it may be a battery camera the hub
    /// does not say is one, so it is treated like one (asked only while the
    /// hub reports it awake, never on a timer).
    pub hub: bool,
    /// The first channel, known only from the stream address (`Preview_01`):
    /// a camera of its own, or channel 1 of a hub or recorder. Which, the
    /// device is asked (once); until it has said, it counts as a hub's.
    pub ch0: bool,
    /// The device's Reolink port 9000 address (the siren's sound is set
    /// that way, however the other controls are reached).
    pub bc_base: String,
}

impl Target {
    /// May be asked on a timer (what it has every few hours, its state for Home Assistant).
    pub fn timers_ok(&self) -> bool {
        !self.battery && !is_hub(self)
    }
}

impl std::fmt::Debug for Target {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // (Never the password.)
        f.debug_struct("Target").field("brand", &self.brand).field("bases", &self.bases).field("user", &self.user).field("channel", &self.channel).field("battery", &self.battery).finish()
    }
}

impl Target {
    /// The brand's driver: at `base` (the address that answered before), or
    /// found among the target's addresses.
    fn driver(&self, base: Option<&str>) -> Result<Box<dyn Driver>> {
        let at = |b: &str| -> Result<Box<dyn Driver>> {
            if baichuan_drv::is_base(b) {
                Ok(Box::new(baichuan_drv::BaichuanDriver::at(b, &self.user, &self.pass, self.channel)?))
            } else {
                Ok(Box::new(reolink_drv::ReolinkDriver::at(b, &self.user, &self.pass, self.channel)))
            }
        };
        match base {
            Some(b) => at(b),
            // (One address: no need to find which answers; the first ask shows whether it does.)
            None if self.bases.len() == 1 => at(&self.bases[0]),
            None => {
                // In turn; the next only when nothing answers at one (never after a
                // refused sign-in or a certificate problem). Reolink port 9000 comes
                // last and is not asked first: the first ask of it shows whether it answers.
                let mut last = anyhow::anyhow!("no address to try");
                for b in &self.bases {
                    if baichuan_drv::is_base(b) {
                        return at(b);
                    }
                    match reolink_drv::connect(std::slice::from_ref(b), &self.user, &self.pass, self.channel) {
                        Ok(d) => return Ok(Box::new(d)),
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
        }
    }
}

/// A channel of a hub or recorder as far as is known without asking: a
/// channel set or in the stream address, or Preview_01 of a device the
/// controls found to have several channels.
pub fn known_channel_of_multi(cfg: &CameraConfig) -> bool {
    target(cfg).is_ok_and(|t| t.hub || (t.ch0 && device_known(&t).is_some_and(|k| k.multi)))
}

/// The device channel in a Reolink stream path (`Preview_02_main`: 1).
fn channel_in_path(url: &str) -> Option<u32> {
    let i = url.find("Preview_")? + "Preview_".len();
    let n: String = url[i..].chars().take_while(|c| c.is_ascii_digit()).collect();
    n.parse::<u32>().ok().and_then(|n| n.checked_sub(1))
}

/// A camera whose stream addresses look like a Reolink's.
fn looks_reolink(cfg: &CameraConfig) -> bool {
    let u = format!("{} {}", cfg.url, cfg.sub_url.as_deref().unwrap_or("")).to_ascii_lowercase();
    cfg.reolink_channel.is_some() || u.contains("preview_0") || u.contains("h264preview") || u.contains("h265preview") || u.contains(".bcs") || u.contains("app=bcs")
}

fn host_port(host: &str, port: Option<u16>) -> String {
    let h = if host.contains(':') && !host.starts_with('[') { format!("[{host}]") } else { host.to_string() };
    match port {
        Some(p) => format!("{h}:{p}"),
        None => h,
    }
}

/// How this camera's controls are reached, or why it has none (in words).
pub fn target(cfg: &CameraConfig) -> Result<Target, String> {
    let mut c = cfg.clone();
    crate::engine::normalize_creds(&mut c);
    let set = c.controls.clone().unwrap_or_default();
    let kind = set.kind.as_deref().map(|k| k.trim().to_ascii_lowercase()).unwrap_or_default();
    if kind == "off" {
        return Err("Controls are turned off for this camera.".into());
    }
    if crate::ingest::baichuan::is_baichuan_url(&c.url) || crate::ingest::push::is_push_url(&c.url) {
        return Err("Battery cameras reached without a hub: no controls yet.".into());
    }
    if !(kind == "reolink" || looks_reolink(&c)) {
        return Err("Controls work with Reolink cameras so far. Other brands may follow.".into());
    }
    let Some(host) = crate::onvif::host_from_rtsp_url(&c.url) else { return Err("The camera's address could not be read from its URL.".into()) };
    if c.username.is_empty() {
        return Err("Needs the camera's user name and password.".into());
    }
    let port = set.http_port.or_else(|| c.watchdog.as_ref().and_then(|w| w.http_port));
    let hp = host_port(&host, port);
    let channel = c.reolink_channel.or_else(|| channel_in_path(&c.url)).unwrap_or(0);
    let battery = c.on_demand || c.battery_uid().is_some();
    // A channel from the stream address: Preview_02 and up are a hub's or a
    // recorder's; Preview_01 may be either (see `ch0`).
    let path_ch = channel_in_path(&c.url);
    let hub = c.reolink_channel.is_some() || path_ch.is_some_and(|n| n > 0);
    let mut ch0 = c.reolink_channel.is_none() && path_ch == Some(0);
    // Reolink port 9000 (Baichuan): for a camera of its own that is not a
    // battery camera, when picked, or when the camera's video may use that
    // port too ("Connect with": Reolink port 9000, or Auto after HTTPS).
    // (The host as the video's Baichuan session is keyed: one session per device.)
    let bc_port = c.baichuan_port.unwrap_or(crate::ingest::baichuan::transport::TCP_PORT);
    let dev_host = crate::ingest::baichuan::wired::target_for(&c.url, bc_port, None, crate::ingest::baichuan::wired::StreamKind::Main)
        .map(|t| t.host)
        .unwrap_or_else(|| crate::ingest::baichuan::wired::norm_host(&host));
    let bc = baichuan_drv::base_for(&dev_host, bc_port);
    let bc_base = bc.clone();
    let bc_ok = !battery && !hub;
    let connect = c.connect.trim().to_ascii_lowercase();
    // HTTPS only, unless the admin picks plain HTTP (which sends the camera's
    // password unencrypted): never a quiet fall back to it.
    let bases = match set.scheme.as_deref().map(str::trim) {
        Some("http") => vec![format!("http://{hp}")],
        Some("baichuan") => {
            if battery {
                return Err(crate::ingest::baichuan::ctl::NOT_FOR_BATTERY.into());
            }
            if hub {
                return Err(crate::ingest::baichuan::ctl::NOT_FOR_HUB.into());
            }
            vec![bc]
        }
        Some("https") => vec![format!("https://{hp}")],
        _ if bc_ok && connect == "baichuan" => vec![bc],
        _ if bc_ok && connect == "auto" => vec![format!("https://{hp}"), bc],
        _ => vec![format!("https://{hp}")],
    };
    if bases.iter().all(|b| baichuan_drv::is_base(b)) {
        // (The session refuses a device with more channels: it is a camera of its own.)
        ch0 = false;
    }
    let pass_tag = {
        use sha2::Digest;
        let h = sha2::Sha256::digest(c.password.as_bytes());
        h[..6].iter().map(|b| format!("{b:02x}")).collect::<String>()
    };
    // Where a light is: the device (its host, and its ports when they are set:
    // behind port forwarding one address can be several cameras), the account
    // and the channel. Not the scheme or the way in.
    let place = place_of(&dev_host, port, c.baichuan_port, &c.username, channel);
    Ok(Target { key: format!("{place}|{pass_tag}|{battery}"), place, brand: "reolink", bases, user: c.username.clone(), pass: c.password.clone(), channel, battery, hub, ch0, bc_base })
}

/// Where a light is (`Target::place`).
fn place_of(host: &str, web_port: Option<u16>, bc_port: Option<u16>, user: &str, channel: u32) -> String {
    let mut h = crate::ingest::baichuan::wired::norm_host(host);
    if let Some(p) = web_port {
        h.push_str(&format!("#web={p}"));
    }
    // (Reolink port 9000 itself is no different from not setting it.)
    if let Some(p) = bc_port.filter(|p| *p != crate::ingest::baichuan::transport::TCP_PORT) {
        h.push_str(&format!("#bc={p}"));
    }
    format!("reolink|{h}|{user}|{channel}")
}

/// The place in a saved light's key: its first four parts. Older keys had
/// the addresses (`https://host:port,...`) where the host now is, and a
/// password-derived fifth part (not kept).
fn place_from_key(key: &str) -> String {
    let parts: Vec<&str> = key.splitn(5, '|').take(4).collect();
    match parts.as_slice() {
        [brand, addr, user, ch] if addr.contains("://") => {
            // Each address: scheme://host[:port]. A web address carried a port only
            // when one was set; a port 9000 address always did.
            let split = |a: &str| -> (bool, String, Option<u16>) {
                let (scheme, rest) = a.split_once("://").unwrap_or(("", a));
                let hp = rest.split('/').next().unwrap_or("");
                let (host, port) = match hp.strip_prefix('[') {
                    Some(b) => match b.split_once(']') {
                        Some((h, p)) => (h, p.strip_prefix(':')),
                        None => (b, None),
                    },
                    None => match hp.rsplit_once(':') {
                        Some((h, p)) => (h, Some(p)),
                        None => (hp, None),
                    },
                };
                (scheme == "baichuan-tcp", host.to_string(), port.and_then(|p| p.parse().ok()))
            };
            let all: Vec<(bool, String, Option<u16>)> = addr.split(',').map(split).collect();
            let host = all.first().map(|a| a.1.clone()).unwrap_or_default();
            let web = all.iter().find(|a| !a.0).and_then(|a| a.2);
            let bc = all.iter().find(|a| a.0).and_then(|a| a.2);
            let user = user.to_string();
            let ch: u32 = ch.parse().unwrap_or(0);
            let p = place_of(&host, web, bc, &user, ch);
            if *brand == "reolink" { p } else { p.replacen("reolink", brand, 1) }
        }
        _ => parts.join("|"),
    }
}

/// Controls settings a person can easily get wrong (a camera save is refused with these words).
pub fn check_settings(cam: &CameraConfig) -> Result<(), String> {
    let Some(c) = &cam.controls else { return Ok(()) };
    if let Some(k) = c.kind.as_deref().map(str::trim).filter(|k| !k.is_empty()) {
        if !matches!(k, "auto" | "reolink" | "off") {
            return Err(format!("Controls: '{k}' is not a kind: pick auto, reolink or off"));
        }
    }
    if let Some(s) = c.scheme.as_deref().map(str::trim).filter(|s| !s.is_empty()) {
        if !matches!(s, "auto" | "http" | "https" | "baichuan") {
            return Err(format!("Controls: '{s}': pick https, http (sends the camera's password unencrypted) or baichuan (Reolink port 9000)"));
        }
    }
    if c.http_port == Some(0) {
        return Err("Controls: the web port cannot be 0".into());
    }
    if c.light_minutes.is_some_and(|m| m == 0 || m > automation::MAX_LIGHT_MINUTES) {
        return Err(format!("Controls: the spotlight stays on 1 to {} minutes", automation::MAX_LIGHT_MINUTES));
    }
    if c.siren_seconds.is_some_and(|s| s == 0 || s > SIREN_MAX_SECS) {
        return Err(format!("Controls: the siren sounds 1 to {SIREN_MAX_SECS} seconds"));
    }
    if c.siren_cooldown_minutes.is_some_and(|m| m == 0 || m > 24 * 60) {
        return Err("Controls: the siren's pause between actions is 1 minute to a day".into());
    }
    if c.labels.len() > 20 || c.labels.iter().any(|l| l.trim().is_empty() || l.len() > 40) {
        return Err("Controls: pick up to 20 objects for the actions".into());
    }
    Ok(())
}

/// A battery camera is awake now (connected, and not reported asleep).
/// The camera may be asked now: a wired camera always; a battery camera while
/// it is awake; a hub's channel while the hub reports it awake. A channel the
/// device says nothing about sleeping for counts as wired only on a device
/// that calls itself a recorder (NVR), never on a Home Hub. (UNVERIFIED on a
/// real recorder: it may report "sleep" for wired cameras too.)
fn askable(h: &CameraHandle, t: &Target) -> bool {
    if t.battery {
        awake(h)
    } else if is_hub(t) {
        match h.state.read().unwrap().device_sleeping {
            Some(s) => !s && awake(h),
            None => awake(h) && device_known(t).is_some_and(|k| k.recorder),
        }
    } else {
        true
    }
}

/// What the device a camera is reached at says it is, as asked (no IO).
static RECORDERS: Mutex<Option<HashMap<String, (Option<DevKind>, Instant)>>> = Mutex::new(None);

/// What a device says it is.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct DevKind {
    /// A recorder (NVR), not a Home Hub.
    pub recorder: bool,
    /// A hub or a recorder (cameras on channels), not a camera of its own.
    pub multi: bool,
}

/// A channel of a hub or recorder (or possibly one: not yet said).
fn is_hub(t: &Target) -> bool {
    t.hub || (t.ch0 && device_known(t).is_none_or(|k| k.multi))
}

fn recorder_key(t: &Target) -> String {
    format!("{}|{}", t.bases.join(","), t.user)
}

fn device_known(t: &Target) -> Option<DevKind> {
    RECORDERS.lock().unwrap().as_ref().and_then(|m| m.get(&recorder_key(t))).and_then(|(v, _)| *v)
}

/// Ask the device (the hub or recorder itself: no channel is woken) what it
/// is, once (again after ten minutes when that failed).
fn learn_device(engine: &Engine, name: &str, t: &Target) {
    let k = recorder_key(t);
    let asked = RECORDERS.lock().unwrap().as_ref().and_then(|m| m.get(&k).cloned());
    if asked.is_some_and(|(v, at)| v.is_some() || at.elapsed() < Duration::from_secs(600)) {
        return;
    }
    let r = driver_now(engine, name, t).map_err(|r| r.msg).and_then(|d| match d.device_kind() {
        Ok(k) => Ok(k),
        // Signed in, and the device does not know the question: a camera of its own
        // (hubs and recorders answer it, per the vendor's guide).
        Err(e) if crate::reolink::cam_error(&e).is_some_and(|c| c.unsupported()) => Ok(DevKind::default()),
        Err(e) => Err(ask_error(&e)),
    });
    let v = match r {
        Ok(v) => Some(v),
        Err(e) => {
            warn!("camera '{name}': could not ask the device it is reached at what it is: {e}");
            // (Shown as the camera's problem: the same one asking it anything else would meet.)
            engine.controls.with(name, |c| {
                reset_if_moved(c, t);
                c.error = Some(e);
                c.error_at = Some(Instant::now());
            });
            None
        }
    };
    RECORDERS.lock().unwrap().get_or_insert_with(HashMap::new).insert(k, (v, Instant::now()));
}

/// Ask the device what it is when that decides whether the camera may be
/// asked (it asks the device itself, never a channel: nothing is woken).
fn know_device(engine: &Engine, name: &str, t: &Target, h: &CameraHandle) {
    if !t.battery && (t.ch0 || (t.hub && h.state.read().unwrap().device_sleeping.is_none())) {
        learn_device(engine, name, t);
    }
}

fn awake(h: &CameraHandle) -> bool {
    let st = h.state.read().unwrap();
    st.online && !st.sleeping && st.device_sleeping != Some(true)
}

// ------------------------------------------------------------------ board

/// The spotlight turned on from AmberSight, and what to give back.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Forced {
    pub until_ms: i64,
    /// The light as it was before (given back when the time is up).
    pub prev: LightState,
    pub by: String,
    /// When it was first turned on (a restore that keeps failing gives up after a day).
    pub since_ms: i64,
    /// Where the light is (`Target::place`: address, account, channel;
    /// nothing from the password). The light belongs to the device there,
    /// whatever camera name has it now.
    #[serde(default)]
    pub key: String,
    /// The camera's name when it was turned on (for the log and the audit).
    #[serde(default)]
    pub name: String,
}

/// A light turned on from AmberSight, by where it is (`Target::place`).
struct Lit {
    f: Forced,
    /// How it was reached (this run): the target and the address that answered.
    via: Option<(Target, String)>,
    restore_next: Option<Instant>,
    restore_fails: u32,
    restoring: bool,
}

#[derive(Default)]
struct Cam {
    key: String,
    ab: Option<Abilities>,
    probed_at: Option<Instant>,
    probed_ms: i64,
    /// The address that answered.
    base: Option<String>,
    error: Option<String>,
    error_at: Option<Instant>,
    reading: Option<(Reading, Instant)>,
    /// A state read is running (others wait for none: they use the last one).
    reading_busy: bool,
    read_error: Option<String>,
    siren_until_ms: Option<i64>,
    /// Each siren started has its own number: a stop (or the timer) clears
    /// only the siren it was for, never a newer one.
    siren_id: u64,
    /// The siren whose play is on its way to the camera now.
    siren_flying: Option<u64>,
    /// Where the sounding siren is (`Target::place`): its stop finds the camera
    /// there, also after a rename.
    siren_place: Option<String>,
    /// A stop went through while that play was on its way: the play may have
    /// reached the camera after it, so it is stopped again.
    stopped_flying: Option<u64>,
    siren_last: Option<Instant>,
    /// When the spotlight was last switched from a page, the API or MQTT.
    light_last: Option<Instant>,
    /// The latest switch asked for within that second: done when it is over.
    light_pending: Option<(Action, Caller, usize)>,
    /// A waiting switch replaced by a later one: said on the panel for a while.
    light_note: Option<(String, Instant)>,
    stopping: bool,
    /// A stop that failed: when to try again, how often it failed, since when.
    stop_next: Option<Instant>,
    stop_fails: u32,
    stop_first: Option<Instant>,
    battery: bool,
}

/// Every camera's controls (`Engine::controls`). Lock order: camera locks
/// (cfg, state) are never taken while `cams` is held.
#[derive(Default)]
pub struct Board {
    cams: Mutex<HashMap<String, Cam>>,
    /// Lights turned on from AmberSight, by where they are (never by camera
    /// name: a new camera given an old name must not inherit one). Lock order:
    /// `cams` before `lights` when both are held.
    lights: Mutex<HashMap<String, Lit>>,
    /// One action at a time per camera (a light's before-state is read once).
    ops: Mutex<HashMap<String, Arc<Mutex<()>>>>,
    /// When each account last started a siren.
    sirens: Mutex<VecDeque<(String, Instant)>>,
    /// Actions next to alerts, per camera (automation.rs).
    pub(crate) auto: Mutex<HashMap<String, AutoState>>,
    started: AtomicBool,
}

#[derive(Default)]
pub(crate) struct AutoState {
    pub light_at: Option<Instant>,
    /// When the actions sounded the siren in the last hour (epoch ms; kept
    /// across restarts, so a restart does not reset the cooldown or the cap).
    pub sirens_ms: Vec<i64>,
}

impl Board {
    fn op(&self, name: &str) -> Arc<Mutex<()>> {
        self.ops.lock().unwrap().entry(name.to_string()).or_default().clone()
    }
    fn with<R>(&self, name: &str, f: impl FnOnce(&mut Cam) -> R) -> R {
        let mut g = self.cams.lock().unwrap();
        f(g.entry(name.to_string()).or_default())
    }
}

/// Who asked, for the audit log and the rate limits.
#[derive(Clone, Debug)]
pub struct Caller {
    pub user: String,
    pub ip: String,
    /// "web", "mqtt" or "automation".
    pub source: &'static str,
}

/// An action refused or failed: an HTTP status and plain words.
#[derive(Debug, PartialEq)]
pub struct Refused {
    pub status: u16,
    pub msg: String,
}

fn refuse(status: u16, msg: impl Into<String>) -> Refused {
    Refused { status, msg: msg.into() }
}

/// What can be asked of a camera's controls.
#[derive(Clone, Debug, Deserialize, PartialEq)]
#[serde(tag = "action", rename_all = "snake_case")]
pub enum Action {
    /// The spotlight on for `seconds` (capped), then back to what it was.
    LightOn { seconds: Option<u32>, brightness: Option<u32> },
    /// Off now: a light AmberSight turned on gets back what it had; otherwise off.
    LightOff,
    /// The camera's own light mode (a setting: admins).
    LightMode { mode: i64 },
    Siren { seconds: Option<u32> },
    SirenStop,
    Ir { state: String },
    StatusLed { state: String },
    /// A doorbell's light (on, off, kept on, kept off: as the camera lists them).
    DoorbellLed { state: String },
    /// Ask the camera again what it has.
    Redetect,
}

impl Action {
    /// A setting kept on the camera (admins only), rather than something done for a while.
    pub fn is_setting(&self) -> bool {
        matches!(self, Action::LightMode { .. } | Action::Ir { .. } | Action::StatusLed { .. } | Action::DoorbellLed { .. } | Action::Redetect)
    }
}

fn now_ms() -> i64 {
    chrono::Utc::now().timestamp_millis()
}

fn persist(engine: &Engine, kind: &str, name: &str, v: Option<String>) {
    let key = format!("{kind}|{name}");
    let r = match v {
        Some(v) => engine.db.ui_put(SCOPE, &key, &v),
        None => engine.db.ui_delete(SCOPE, &key),
    };
    if let Err(e) = r {
        warn!("controls: could not save {key}: {e:#}");
    }
}

fn audit(engine: &Engine, who: &Caller, camera: &str, what: &str, r: &Result<(), String>) {
    let by = match who.source {
        "web" => who.user.clone(),
        s => format!("{} ({s})", who.user),
    };
    let (detail, status) = match r {
        Ok(()) => (format!("{camera}: {what}, ok"), 200),
        Err(e) => (format!("{camera}: {what}, failed: {e}"), 502),
    };
    match r {
        Ok(()) => info!("camera control by {by}: {detail}"),
        Err(_) => warn!("camera control by {by}: {detail}"),
    }
    let _ = engine.db.insert_audit(&by, &who.ip, "camera control", &detail, status);
}

fn handle(engine: &Engine, id: usize) -> Result<(Arc<CameraHandle>, CameraConfig), Refused> {
    let h = engine.camera(id).ok_or_else(|| refuse(404, "no such camera"))?;
    let cfg = h.cfg.read().unwrap().clone();
    Ok((h, cfg))
}

/// The camera's abilities, asked of it when not known (or old, or its
/// address changed) and the camera may be asked now.
fn ensure(engine: &Engine, name: &str, t: &Target, h: &CameraHandle, force: bool) -> Option<Abilities> {
    know_device(engine, name, t, h);
    let can_ask = askable(h, t);
    let need = engine.controls.with(name, |c| {
        reset_if_moved(c, t);
        // (Battery and hub cameras: asked again only when asked for, or after a change.)
        let stale = c.ab.is_none() || (t.timers_ok() && c.probed_at.is_none_or(|p| p.elapsed() > PROBE_EVERY));
        let wait = !force && c.error_at.is_some_and(|e| e.elapsed() < PROBE_RETRY);
        (stale || force) && !wait
    });
    if need && can_ask {
        let op = engine.controls.op(&t.place);
        let _g = op.lock().unwrap();
        // (Another caller may have asked meanwhile.)
        let again = engine.controls.with(name, |c| force || c.ab.is_none() || (t.timers_ok() && c.probed_at.is_none_or(|p| p.elapsed() > PROBE_EVERY)));
        if again {
            let r = t.driver(None).and_then(|d| Ok((d.probe()?, d.base())));
            match r {
                Ok((ab, base)) => {
                    info!("camera '{name}': controls: light {}, siren {}, IR {}, status light {}", ab.light.is_some(), ab.siren, !ab.ir.is_empty(), !ab.status_led.is_empty());
                    engine.controls.with(name, |c| {
                        c.ab = Some(ab);
                        c.base = Some(base);
                        c.probed_at = Some(Instant::now());
                        c.probed_ms = now_ms();
                        c.error = None;
                        c.error_at = None;
                        c.reading = None;
                    });
                }
                Err(e) => {
                    let msg = ask_error(&e);
                    warn!("camera '{name}': cannot read what controls it has: {msg}");
                    engine.controls.with(name, |c| {
                        c.error = Some(msg);
                        c.error_at = Some(Instant::now());
                    });
                }
            }
        }
    }
    engine.controls.with(name, |c| c.ab.clone())
}

/// The driver at the address that answered last.
fn driver_now(engine: &Engine, name: &str, t: &Target) -> Result<Box<dyn Driver>, Refused> {
    // Only while it is still reached the same way and is one of its addresses
    // now: after an address change nothing (not even a sign-in) goes to the old one.
    let base = engine.controls.with(name, |c| if c.key == t.key { c.base.clone() } else { None }).filter(|b| t.bases.contains(b));
    t.driver(base.as_deref()).map_err(|e| refuse(502, tls_hint(&e).unwrap_or_else(|| format!("The camera did not answer: {}", crate::engine::redact_url(format!("{e:#}"))))))
}

/// A camera reached another way than before (or new): what was known of it goes.
fn reset_if_moved(c: &mut Cam, t: &Target) {
    if c.key != t.key {
        *c = Cam {
            siren_until_ms: c.siren_until_ms,
            siren_id: c.siren_id,
            siren_flying: c.siren_flying,
            siren_place: c.siren_place.clone(),
            stopped_flying: c.stopped_flying,
            siren_last: c.siren_last,
            light_last: c.light_last,
            key: t.key.clone(),
            battery: t.battery,
            ..Default::default()
        };
    }
}

/// What went wrong asking a camera, in words for its page.
fn ask_error(e: &anyhow::Error) -> String {
    if crate::reolink::is_unreachable(e) {
        // (Some Reolink models offer no web interface at all: only their own app's protocol.)
        "Controls aren't available this way: the camera doesn't offer the web interface AmberSight uses. For a Reolink camera, choose Reolink port 9000 under Lights & siren, More settings.".to_string()
    } else {
        tls_hint(e).unwrap_or_else(|| crate::engine::redact_url(format!("{e:#}")))
    }
}

/// The words for a camera that answers, but not with HTTPS (its HTTPS is
/// off, or the port speaks plain HTTP): what to do about it.
fn tls_hint(e: &anyhow::Error) -> Option<String> {
    let t = format!("{e:#}").to_ascii_lowercase();
    let tls = ["tls", "handshake", "corrupt message", "invalidcontenttype", "ssl", "certificate"].iter().any(|w| t.contains(w));
    tls.then(|| "This camera's HTTPS didn't answer: turn on HTTPS on the camera, or choose HTTP under Lights & siren, More settings (that sends the camera's password unencrypted).".to_string())
}

/// Read the camera's state now (not for a battery camera that is asleep).
/// Read the camera's state for a page or Home Assistant: one read at a time
/// per camera (others use what it read), and none while the camera is busy
/// with an action.
fn read_now(engine: &Engine, name: &str, t: &Target, ab: &Abilities) -> Option<Reading> {
    let go = engine.controls.with(name, |c| {
        let fresh = c.reading.as_ref().is_some_and(|(_, at)| at.elapsed() < READ_FRESH);
        if fresh || c.reading_busy {
            false
        } else {
            c.reading_busy = true;
            true
        }
    });
    if !go {
        return engine.controls.with(name, |c| c.reading.as_ref().map(|(r, _)| r.clone()));
    }
    let r = read_once(engine, name, t, ab);
    engine.controls.with(name, |c| c.reading_busy = false);
    r
}

fn read_once(engine: &Engine, name: &str, t: &Target, ab: &Abilities) -> Option<Reading> {
    let d = driver_now(engine, name, t).ok()?;
    let Some(answer) = d.read_if_idle(ab) else {
        return engine.controls.with(name, |c| c.reading.as_ref().map(|(r, _)| r.clone()));
    };
    match answer {
        Ok(r) => {
            engine.controls.with(name, |c| {
                c.reading = Some((r.clone(), Instant::now()));
                c.read_error = None;
            });
            Some(r)
        }
        Err(e) => {
            let msg = crate::engine::redact_url(format!("{e:#}"));
            engine.controls.with(name, |c| {
                c.reading = None;
                c.read_error = Some(msg);
            });
            None
        }
    }
}

fn on_off_word(v: Option<bool>) -> &'static str {
    match v {
        Some(true) => "on",
        Some(false) => "off",
        None => "unknown",
    }
}

/// What may this caller do (for the page's buttons).
#[derive(Clone, Copy, Debug, Default)]
pub struct May {
    pub light: bool,
    pub siren: bool,
    pub settings: bool,
}

fn view_json(engine: &Engine, name: &str, t: &Target, may: May, asleep: bool) -> Value {
    let now = now_ms();
    let forced = forced_of(engine, &t.place).filter(|f| f.until_ms > now);
    engine.controls.with(name, |c| {
        let ab = c.ab.clone();
        let reading = c.reading.as_ref().map(|(r, _)| r.clone());
        let light = ab.as_ref().and_then(|a| a.light.as_ref()).map(|l| {
            json!({
                "brightness": l.brightness.map(|(lo, hi)| json!({ "min": lo, "max": hi })),
                "modes": l.modes.iter().map(|m| json!({ "id": m, "label": mode_label(*m), "pick": mode_label(*m).is_some() })).collect::<Vec<_>>(),
                "smart": l.smart,
                "keep_on": l.keep_on,
                "schedule": l.schedule,
                "max_seconds": LIGHT_MAX_SECS,
                "default_seconds": LIGHT_DEFAULT_SECS,
            })
        });
        let forced = forced.as_ref();
        let ls = reading.as_ref().and_then(|r| r.light.clone());
        let mode = ls.as_ref().and_then(|l| l.mode);
        json!({
            "supported": true,
            "brand": t.brand,
            // How it is reached: "web" (its web API) or "port9000" (Reolink's own protocol).
            "via": if c.base.as_deref().unwrap_or(t.bases.first().map(String::as_str).unwrap_or("")).starts_with(baichuan_drv::PREFIX) { "port9000" } else { "web" },
            "light_unswitched": ab.as_ref().is_some_and(|a| a.light_unswitched),
            "battery": t.battery,
            "asleep": asleep,
            "probed_ms": (c.probed_ms > 0).then_some(c.probed_ms),
            "error": c.error,
            "light": light,
            "siren": ab.as_ref().is_some_and(|a| a.siren).then(|| json!({ "max_seconds": SIREN_MAX_SECS, "default_seconds": SIREN_DEFAULT_SECS })),
            "ir": ab.as_ref().filter(|a| !a.ir.is_empty()).map(|a| json!({ "states": a.ir })),
            "status_led": ab.as_ref().filter(|a| !a.status_led.is_empty()).map(|a| json!({ "states": a.status_led })),
            "doorbell_led": ab.as_ref().filter(|a| !a.doorbell_led.is_empty()).map(|a| json!({ "states": a.doorbell_led })),
            "state": {
                "light": on_off_word(ls.as_ref().and_then(|l| l.on)),
                "brightness": ls.as_ref().and_then(|l| l.brightness),
                "mode": mode,
                "mode_label": mode.and_then(mode_label),
                "forced_until_ms": forced.map(|f| f.until_ms),
                // The siren's state cannot be read: what was asked, while it lasts.
                "siren": if c.siren_until_ms.is_some_and(|u| u > now) { "playing" } else { "unknown" },
                "siren_until_ms": c.siren_until_ms.filter(|u| *u > now),
                "ir": reading.as_ref().and_then(|r| r.ir.clone()).unwrap_or_else(|| "unknown".into()),
                "status_led": reading.as_ref().and_then(|r| r.status_led.clone()).unwrap_or_else(|| "unknown".into()),
                "doorbell_led": reading.as_ref().and_then(|r| r.doorbell_led.clone()).unwrap_or_else(|| "unknown".into()),
                "read_error": c.read_error,
                "note": c.light_note.as_ref().filter(|(_, at)| at.elapsed() < Duration::from_secs(60)).map(|(t, _)| t.clone()),
            },
            "may": { "light": may.light, "siren": may.siren, "settings": may.settings },
        })
    })
}

/// `GET` of a camera's controls: what it has and its state now.
pub fn view(engine: &Engine, id: usize, may: May) -> Result<Value, Refused> {
    let (h, cfg) = handle(engine, id)?;
    let t = match target(&cfg) {
        Ok(t) => t,
        Err(why) => return Ok(json!({ "supported": false, "why": why })),
    };
    let ab = ensure(engine, &cfg.name, &t, &h, false);
    let asleep = !askable(&h, &t);
    if let Some(ab) = ab.as_ref().filter(|a| a.any()) {
        if !asleep {
            read_now(engine, &cfg.name, &t, ab);
        }
    }
    // (A camera that could not be asked what it is: its problem is shown, not sleep.)
    let unknown = t.ch0 && !t.battery && device_known(&t).is_none();
    let asleep = asleep && !unknown;
    let mut v = view_json(engine, &cfg.name, &t, may, asleep);
    if ab.is_none() && asleep {
        v["why"] = if t.battery { "This battery camera is asleep: what it has is read the next time it is awake." } else { "Not asked yet: a camera behind a hub is asked only while the hub reports it awake." }.into();
    }
    Ok(v)
}

/// What the camera list shows of a camera's controls (from what is known;
/// asks the camera nothing).
pub fn summary(engine: &Engine, cfg: &CameraConfig) -> Value {
    let now = now_ms();
    let name = cfg.name.as_str();
    let forced_until = target(cfg).ok().and_then(|t| forced_of(engine, &t.place)).map(|f| f.until_ms).filter(|u| *u > now);
    let g = engine.controls.cams.lock().unwrap();
    let Some(c) = g.get(name) else { return Value::Null };
    let Some(ab) = c.ab.as_ref().filter(|a| a.any()) else { return Value::Null };
    json!({
        "light": ab.light.is_some(),
        "brightness": ab.light.as_ref().and_then(|l| l.brightness).map(|(lo, hi)| json!([lo, hi])),
        "siren": ab.siren,
        "light_on": c.reading.as_ref().and_then(|(r, _)| r.light.as_ref()).and_then(|l| l.on),
        "forced_until_ms": forced_until,
        "siren_until_ms": c.siren_until_ms.filter(|u| *u > now),
        "battery": c.battery,
    })
}

/// Light on (`Some`) / off / unknown as last read, and whether it has a light at all.
pub fn light_now(engine: &Engine, name: &str) -> Option<(Option<bool>, Option<u32>, Option<(u32, u32)>)> {
    let g = engine.controls.cams.lock().unwrap();
    let c = g.get(name)?;
    let caps = c.ab.as_ref()?.light.as_ref()?;
    let l = c.reading.as_ref().and_then(|(r, _)| r.light.clone());
    Some((l.as_ref().and_then(|l| l.on), l.and_then(|l| l.brightness), caps.brightness))
}

/// Whether a siren started from AmberSight is still in its time (its real state cannot be read).
pub fn siren_playing(engine: &Engine, name: &str) -> bool {
    let now = now_ms();
    engine.controls.cams.lock().unwrap().get(name).and_then(|c| c.siren_until_ms).is_some_and(|u| u > now)
}

/// Whether the camera's siren is sounding or its timed stop is still due (a
/// siren started from AmberSight, its play on its way, or its stop running).
pub fn siren_busy(engine: &Engine, name: &str) -> bool {
    engine.controls.cams.lock().unwrap().get(name).is_some_and(|c| c.siren_until_ms.is_some() || c.siren_flying.is_some() || c.stopping)
}

/// A battery camera behind a Home Hub (not a recorder's channel): one the
/// hub reports sleep or a charge for, or one set as a battery camera.
pub fn battery_behind_hub(h: &CameraHandle, t: &Target) -> bool {
    if !is_hub(t) || device_known(t).is_some_and(|k| k.recorder) {
        return false;
    }
    let st = h.state.read().unwrap();
    t.battery || st.device_sleeping.is_some() || st.battery.is_some()
}

/// Whether the camera has a siren (as known).
pub fn has_siren(engine: &Engine, name: &str) -> bool {
    engine.controls.cams.lock().unwrap().get(name).and_then(|c| c.ab.as_ref()).is_some_and(|a| a.siren)
}

/// Write, then read back until it shows (some cameras answer "done" and
/// change nothing). `shows` says whether a reading has it.
fn write_checked(d: &dyn Driver, ab: &Abilities, write: impl Fn() -> Result<()>, shows: impl Fn(&Reading) -> bool) -> Result<Reading, String> {
    write().map_err(|e| crate::engine::redact_url(format!("{e:#}")))?;
    let mut last = None;
    for wait in [0u64, 400, 1200] {
        std::thread::sleep(Duration::from_millis(wait));
        match d.read(ab) {
            Ok(r) if shows(&r) => return Ok(r),
            Ok(r) => last = Some(r),
            Err(e) => return Err(format!("the camera took the change but its state could not be read back: {}", crate::engine::redact_url(format!("{e:#}")))),
        }
    }
    let _ = last;
    Err("the camera answered that it was done, but nothing changed".into())
}

fn light_matches(want: &LightState) -> impl Fn(&Reading) -> bool + '_ {
    move |r: &Reading| {
        let Some(l) = &r.light else { return false };
        want.on.is_none_or(|w| l.on == Some(w)) && want.mode.is_none_or(|m| l.mode == Some(m)) && want.brightness.is_none_or(|b| l.brightness.is_none_or(|x| x == b))
    }
}

/// Give a light AmberSight turned on back what it had. Under the camera's op lock.
fn restore_light(d: &dyn Driver, ab: &Abilities, f: &Forced) -> Result<Reading, String> {
    let prev = &f.prev;
    write_checked(d, ab, || d.set_light(prev), light_matches(prev))
}

/// The light turned on from AmberSight at `place`, if any.
fn forced_of(engine: &Engine, place: &str) -> Option<Forced> {
    engine.controls.lights.lock().unwrap().get(place).map(|l| l.f.clone())
}

/// Keep (or, with None, forget) the light at `place`, saved for a restart.
fn store_forced(engine: &Engine, place: &str, f: Option<Forced>) {
    persist(engine, "light", place, f.as_ref().and_then(|f| serde_json::to_string(f).ok()));
    let mut g = engine.controls.lights.lock().unwrap();
    match f {
        None => {
            g.remove(place);
        }
        Some(f) => {
            let l = g.entry(place.to_string()).or_insert_with(|| Lit { f: f.clone(), via: None, restore_next: None, restore_fails: 0, restoring: false });
            l.f = f;
            l.restore_next = None;
        }
    }
}

fn set_via(engine: &Engine, place: &str, t: &Target, base: String) {
    if let Some(l) = engine.controls.lights.lock().unwrap().get_mut(place) {
        l.via = Some((t.clone(), base));
    }
}

/// The saved record of a sounding siren: until when, and where ("until|place").
fn persist_siren(engine: &Engine, name: &str, until: i64) {
    let place = engine.controls.with(name, |c| c.siren_place.clone()).unwrap_or_default();
    persist(engine, "siren", name, Some(format!("{until}|{place}")));
}

/// The siren started now at `place` has number (returned) and lasts until
/// `until` (saved first: a restart before the stop still stops it).
fn siren_begin(engine: &Engine, name: &str, place: &str, until: i64) -> u64 {
    engine.controls.with(name, |c| c.siren_place = Some(place.to_string()));
    persist_siren(engine, name, until);
    engine.controls.with(name, |c| {
        c.siren_id += 1;
        c.siren_last = Some(Instant::now());
        c.siren_until_ms = Some(until);
        c.siren_flying = Some(c.siren_id);
        c.stopped_flying = None;
        c.siren_id
    })
}

/// Siren `id`'s play is answered (`done`: the camera surely did not play it
/// when false). A stop that went through while it was on its way may have
/// reached the camera first: then it is stopped again at once.
fn siren_landed(engine: &Engine, name: &str, id: u64, not_done: bool) {
    let (current, stopped) = engine.controls.with(name, |c| {
        if c.siren_flying == Some(id) {
            c.siren_flying = None;
        }
        (c.siren_id == id, c.stopped_flying.take() == Some(id))
    });
    if !current {
        return;
    }
    if not_done {
        persist(engine, "siren", name, None);
        engine.controls.with(name, |c| c.siren_until_ms = None);
    } else if stopped {
        let now = now_ms();
        persist_siren(engine, name, now);
        engine.controls.with(name, |c| c.siren_until_ms = Some(now));
    }
}

/// A stop for siren `id` went through: its record goes, unless a newer
/// siren was started meanwhile (that one keeps its own).
fn siren_stopped(engine: &Engine, name: &str, id: u64) {
    let cleared = engine.controls.with(name, |c| {
        if c.siren_flying == Some(id) {
            c.stopped_flying = Some(id);
        }
        let mine = c.siren_id == id;
        if mine {
            c.siren_until_ms = None;
        }
        mine
    });
    if cleared {
        persist(engine, "siren", name, None);
    }
}

/// Do `action` on camera `id`. The caller has checked the caller's rights.
/// `extend_only`: a light already on stays on at least as long as it was
/// going to (actions next to alerts).
pub fn act(engine: &Engine, id: usize, who: &Caller, action: Action, extend_only: bool) -> Result<Value, Refused> {
    let (h, cfg) = handle(engine, id)?;
    let name = cfg.name.clone();
    let t = target(&cfg).map_err(|why| refuse(409, why))?;
    know_device(engine, &name, &t, &h);
    if !askable(&h, &t) {
        return Err(refuse(409, if t.battery {
            "This battery camera is asleep. Controls never wake it: they work while it is awake (open its live view)."
        } else {
            "The hub does not report this camera awake. Controls never wake it: they work while it is awake (open its live view)."
        }));
    }
    let ab = ensure(engine, &name, &t, &h, matches!(action, Action::Redetect));
    if matches!(action, Action::Redetect) {
        let r = if ab.is_some() { Ok(()) } else { Err(engine.controls.with(&name, |c| c.error.clone()).unwrap_or_else(|| "no answer".into())) };
        audit(engine, who, &name, "asked what controls it has", &r);
        return view(engine, id, May { light: true, siren: true, settings: true });
    }
    let ab = ab.ok_or_else(|| refuse(502, engine.controls.with(&name, |c| c.error.clone()).map(|e| format!("Could not find out what this camera has: {e}")).unwrap_or_else(|| "Could not find out what this camera has.".into())))?;
    if matches!(action, Action::SirenStop) {
        // Never behind another action (a light's write and read-back can take seconds).
        if !ab.siren {
            return Err(refuse(409, "This camera has no siren."));
        }
        let d = driver_now(engine, &name, &t)?;
        let for_id = engine.controls.with(&name, |c| c.siren_id);
        let r = d.siren_stop().map_err(|e| crate::engine::redact_url(format!("{e:#}")));
        if r.is_ok() {
            siren_stopped(engine, &name, for_id);
        }
        audit(engine, who, &name, "siren stopped", &r);
        r.map_err(|e| refuse(502, format!("The siren did not stop: {e}.")))?;
        let asleep = !askable(&h, &t);
        return Ok(view_json(engine, &name, &t, May { light: true, siren: true, settings: true }, asleep));
    }
    let op = engine.controls.op(&t.place);
    let _g = op.lock().unwrap();
    let d = driver_now(engine, &name, &t)?;
    let now = now_ms();
    // A light switched on and off as fast as a page or a broker can send it wears the
    // camera and floods it: at most once a second (the actions next to alerts have their own gap).
    if matches!(action, Action::LightOn { .. } | Action::LightOff) && !extend_only {
        let quick = engine.controls.with(&name, |c| {
            let q = c.light_last.is_some_and(|l| l.elapsed() < LIGHT_GAP);
            if !q {
                c.light_last = Some(Instant::now());
            }
            q
        });
        if quick {
            // Not dropped: the latest wish is done when the second is over (a switch
            // flipped on and off, or a slider dragged, ends where it was left).
            let word = |a: &Action| if matches!(a, Action::LightOff) { "off" } else { "on" };
            let replaced = engine.controls.with(&name, |c| {
                let old = c.light_pending.replace((action.clone(), who.clone(), id));
                if let Some((a, w, _)) = &old {
                    c.light_note = Some((format!("A spotlight {} asked by {} was replaced by a later {} from {}.", word(a), w.user, word(&action), who.user), Instant::now()));
                }
                old
            });
            // Said to whoever asked first (the audit, and the panel for a while).
            if let Some((a, w, _)) = replaced {
                audit(engine, &w, &name, &format!("spotlight {} not done: replaced by a later {} from {}", word(&a), word(&action), who.user), &Ok(()));
            }
            drop(_g);
            let mut v = view_json(engine, &name, &t, May { light: true, siren: true, settings: true }, false);
            v["queued"] = true.into();
            return Ok(v);
        }
    }
    match action {
        Action::LightOn { seconds, brightness } => {
            let caps = ab.light.clone().ok_or_else(|| refuse(409, "This camera has no spotlight."))?;
            let secs = seconds.unwrap_or(LIGHT_DEFAULT_SECS).clamp(1, LIGHT_MAX_SECS);
            let bright = match (brightness, caps.brightness) {
                (Some(b), Some((lo, hi))) => Some(b.clamp(lo.max(1), hi)),
                (Some(_), None) => return Err(refuse(400, "This camera's spotlight has no brightness setting.")),
                (None, _) => None,
            };
            let until = now + i64::from(secs) * 1000;
            let mut what = format!("spotlight on for {secs} s{}", bright.map(|b| format!(" at brightness {b}")).unwrap_or_default());
            let forced = forced_of(engine, &t.place);
            let r: Result<(), String> = match forced {
                Some(mut f) => {
                    // On already (by AmberSight): only the time (and brightness) change.
                    let r = match bright {
                        Some(b) => {
                            let want = LightState { on: Some(true), brightness: Some(b), mode: None };
                            write_checked(&*d, &ab, || d.set_light(&want), light_matches(&want)).map(|_| ())
                        }
                        None => Ok(()),
                    };
                    if r.is_ok() {
                        f.until_ms = if extend_only { f.until_ms.max(until) } else { until };
                        store_forced(engine, &t.place, Some(f));
                    }
                    r
                }
                None => {
                    let prev = d.read(&ab).ok().and_then(|r| r.light).filter(|l| l.on.is_some());
                    match prev {
                        None => Err("the light's state could not be read first (so it could not be given back after)".into()),
                        // On already by the camera's own setting: left alone. Taking it
                        // over would mean giving back "on" afterwards, which on these
                        // cameras is a manual on that its own setting would not end.
                        Some(prev) if prev.on == Some(true) => {
                            what = "spotlight left alone (already on by the camera's own setting)".into();
                            engine.controls.with(&name, |c| c.reading = Some((Reading { light: Some(prev.clone()), ..c.reading.clone().map(|r| r.0).unwrap_or_default() }, Instant::now())));
                            Ok(())
                        }
                        Some(prev) => {
                            let want = LightState { on: Some(true), brightness: bright, mode: None };
                            // Saved before the write: a restart in between still gives it back.
                            let f = Forced { until_ms: until, prev: prev.clone(), by: who.user.clone(), since_ms: now, key: t.place.clone(), name: name.clone() };
                            store_forced(engine, &t.place, Some(f));
                            set_via(engine, &t.place, &t, d.base());
                            match write_checked(&*d, &ab, || d.set_light(&want), light_matches(&want)) {
                                Ok(r) => {
                                    engine.controls.with(&name, |c| c.reading = Some((r, Instant::now())));
                                    Ok(())
                                }
                                Err(e) => {
                                    // Whatever happened, give back what it had. The record
                                    // goes only once the camera shows it back; otherwise its
                                    // time is up now and the timer keeps trying.
                                    match write_checked(&*d, &ab, || d.set_light(&prev), light_matches(&prev)) {
                                        Ok(r) => {
                                            engine.controls.with(&name, |c| c.reading = Some((r, Instant::now())));
                                            store_forced(engine, &t.place, None);
                                        }
                                        Err(e2) => {
                                            warn!("camera '{name}': the spotlight may be on and could not be given back yet ({e2}); trying again");
                                            store_forced(engine, &t.place, Some(Forced { until_ms: now, prev: prev.clone(), by: who.user.clone(), since_ms: now, key: t.place.clone(), name: name.clone() }));
                                            set_via(engine, &t.place, &t, d.base());
                                        }
                                    }
                                    Err(e)
                                }
                            }
                        }
                    }
                }
            };
            audit(engine, who, &name, &what, &r);
            r.map_err(|e| refuse(502, format!("The spotlight did not come on: {e}.")))?;
        }
        Action::LightOff => {
            ab.light.as_ref().ok_or_else(|| refuse(409, "This camera has no spotlight."))?;
            let forced = forced_of(engine, &t.place);
            let r = match &forced {
                Some(f) => {
                    let r = restore_light(&*d, &ab, f).map(|r| engine.controls.with(&name, |c| c.reading = Some((r, Instant::now()))));
                    if r.is_ok() {
                        store_forced(engine, &t.place, None);
                    }
                    r
                }
                None => {
                    let want = LightState { on: Some(false), brightness: None, mode: None };
                    write_checked(&*d, &ab, || d.set_light(&want), light_matches(&want)).map(|r| engine.controls.with(&name, |c| c.reading = Some((r, Instant::now()))))
                }
            };
            let what = if forced.is_some() { "spotlight back to the camera's own setting" } else { "spotlight off" };
            audit(engine, who, &name, what, &r);
            r.map_err(|e| refuse(502, format!("The spotlight did not go off: {e}.")))?;
        }
        Action::LightMode { mode } => {
            let caps = ab.light.clone().ok_or_else(|| refuse(409, "This camera has no spotlight."))?;
            if !caps.modes.contains(&mode) || mode_label(mode).is_none() {
                return Err(refuse(400, "Not one of this camera's light modes."));
            }
            let what = format!("spotlight mode {}", mode_label(mode).unwrap_or("?"));
            let forced = forced_of(engine, &t.place);
            let r = match forced {
                // On from AmberSight now: the new mode is what it gets back.
                Some(mut f) => {
                    f.prev.mode = Some(mode);
                    store_forced(engine, &t.place, Some(f));
                    Ok(())
                }
                None => {
                    let want = LightState { on: None, brightness: None, mode: Some(mode) };
                    write_checked(&*d, &ab, || d.set_light(&want), light_matches(&want)).map(|r| engine.controls.with(&name, |c| c.reading = Some((r, Instant::now()))))
                }
            };
            audit(engine, who, &name, &what, &r);
            r.map_err(|e| refuse(502, format!("The light mode did not change: {e}.")))?;
        }
        Action::Siren { seconds } => {
            if !ab.siren {
                return Err(refuse(409, "This camera has no siren."));
            }
            let secs = seconds.unwrap_or(SIREN_DEFAULT_SECS).clamp(1, SIREN_MAX_SECS);
            if engine.controls.with(&name, |c| c.siren_last.is_some_and(|l| l.elapsed() < SIREN_GAP)) {
                return Err(refuse(429, format!("This camera's siren was started less than {} seconds ago.", SIREN_GAP.as_secs())));
            }
            {
                let mut log = engine.controls.sirens.lock().unwrap();
                while log.front().is_some_and(|(_, at)| at.elapsed() > SIREN_WINDOW) {
                    log.pop_front();
                }
                if log.iter().filter(|(u, _)| *u == who.user).count() >= SIREN_PER_USER {
                    return Err(refuse(429, format!("Too many sirens from this account: at most {SIREN_PER_USER} in {} minutes.", SIREN_WINDOW.as_secs() / 60)));
                }
                log.push_back((who.user.clone(), Instant::now()));
            }
            let until = now + i64::from(secs) * 1000;
            let sid = siren_begin(engine, &name, &t.place, until);
            let played = d.siren_play(secs);
            siren_landed(engine, &name, sid, played.as_ref().err().is_some_and(not_done));
            match played {
                Ok(()) => audit(engine, who, &name, &format!("siren for {secs} s"), &Ok(())),
                Err(e) if not_done(&e) => {
                    let e = crate::engine::redact_url(format!("{e:#}"));
                    audit(engine, who, &name, &format!("siren for {secs} s"), &Err(e.clone()));
                    // (Behind a hub, a battery camera refuses while its own siren setting is off.)
                    let hint = if battery_behind_hub(&h, &t) { format!(" {}", siren_sound::HUB_SIREN_HINT) } else { String::new() };
                    return Err(refuse(502, format!("The siren did not start: {e}.{hint}")));
                }
                Err(e) => {
                    // No clear answer: it may be sounding. Its stop stays due when its time is up.
                    let e = crate::engine::redact_url(format!("{e:#}"));
                    audit(engine, who, &name, &format!("siren for {secs} s (may be playing; stopped when its time is up)"), &Err(e.clone()));
                    return Err(refuse(502, format!("The siren may be sounding: the camera did not answer clearly ({e}). It is stopped when its time is up.")));
                }
            }
        }
        Action::SirenStop => unreachable!(),
        Action::Ir { state } => {
            let s = ab.ir.iter().find(|x| x.eq_ignore_ascii_case(&state)).cloned().ok_or_else(|| refuse(400, "Not one of this camera's infrared light settings."))?;
            let r = write_checked(&*d, &ab, || d.set_ir(&s), |r| r.ir.as_deref() == Some(s.as_str())).map(|r| engine.controls.with(&name, |c| c.reading = Some((r, Instant::now()))));
            audit(engine, who, &name, &format!("infrared lights {s}"), &r);
            r.map_err(|e| refuse(502, format!("The infrared lights did not change: {e}.")))?;
        }
        Action::StatusLed { state } => {
            let s = ab.status_led.iter().find(|x| x.eq_ignore_ascii_case(&state)).cloned().ok_or_else(|| refuse(400, "Not one of this camera's status light settings."))?;
            let r = write_checked(&*d, &ab, || d.set_status_led(&s), |r| r.status_led.as_deref() == Some(s.as_str())).map(|r| engine.controls.with(&name, |c| c.reading = Some((r, Instant::now()))));
            audit(engine, who, &name, &format!("status light {s}"), &r);
            r.map_err(|e| refuse(502, format!("The status light did not change: {e}.")))?;
        }
        Action::DoorbellLed { state } => {
            let s = ab.doorbell_led.iter().find(|x| x.eq_ignore_ascii_case(&state)).cloned().ok_or_else(|| refuse(400, "Not one of this doorbell's light settings."))?;
            let r = write_checked(&*d, &ab, || d.set_doorbell_led(&s), |r| r.doorbell_led.as_deref() == Some(s.as_str())).map(|r| engine.controls.with(&name, |c| c.reading = Some((r, Instant::now()))));
            audit(engine, who, &name, &format!("doorbell light {s}"), &r);
            r.map_err(|e| refuse(502, format!("The doorbell light did not change: {e}.")))?;
        }
        Action::Redetect => unreachable!(),
    }
    drop(_g);
    let asleep = !askable(&h, &t);
    Ok(view_json(engine, &name, &t, May { light: true, siren: true, settings: true }, asleep))
}

// ------------------------------------------------------------------ timers

/// The camera handle and target of a camera by name.
fn by_name(engine: &Engine, name: &str) -> Option<(Arc<CameraHandle>, Target)> {
    let h = engine.cameras.read().unwrap().iter().flatten().find(|h| h.cfg.read().unwrap().name == name).cloned()?;
    let cfg = h.cfg.read().unwrap().clone();
    let t = target(&cfg).ok()?;
    Some((h, t))
}

/// Give back lights whose time is up and stop sirens whose time is up
/// (each on its own thread, so one slow camera holds up no other).
pub fn tick(engine: &Arc<Engine>) {
    let now = now_ms();
    let mut lights = Vec::new();
    {
        let mut g = engine.controls.lights.lock().unwrap();
        for (place, l) in g.iter_mut() {
            // Never longer than the longest allowed from now (the clock may have jumped back).
            l.f.until_ms = l.f.until_ms.min(now + i64::from(LIGHT_MAX_SECS) * 1000);
            if l.f.until_ms <= now && !l.restoring && l.restore_next.is_none_or(|n| Instant::now() >= n) {
                l.restoring = true;
                lights.push((place.clone(), l.f.clone()));
            }
        }
    }
    // Spotlight switches that waited for their second.
    let due: Vec<(Action, Caller, usize)> = {
        let mut g = engine.controls.cams.lock().unwrap();
        g.values_mut().filter(|c| c.light_pending.is_some() && c.light_last.is_none_or(|l| l.elapsed() >= LIGHT_GAP)).filter_map(|c| c.light_pending.take()).collect()
    };
    for (a, who, id) in due {
        let e = engine.clone();
        std::thread::spawn(move || {
            if let Err(r) = act(&e, id, &who, a, false) {
                warn!("camera control by {}: a waiting spotlight switch failed: {}", who.user, r.msg);
            }
        });
    }
    let sirens: Vec<(String, u64)> = {
        let mut g = engine.controls.cams.lock().unwrap();
        let mut sirens = Vec::new();
        for (name, c) in g.iter_mut() {
            if let Some(u) = c.siren_until_ms.as_mut() {
                *u = (*u).min(now + i64::from(SIREN_MAX_SECS) * 1000);
            }
            if c.siren_until_ms.is_some_and(|u| u <= now) && !c.stopping && c.stop_next.is_none_or(|n| Instant::now() >= n) {
                c.stopping = true;
                sirens.push((name.clone(), c.siren_id));
            }
        }
        sirens
    };
    for (place, f) in lights {
        let e = engine.clone();
        std::thread::spawn(move || {
            let done = finish_light(&e, &place, &f);
            if let Some(l) = e.controls.lights.lock().unwrap().get_mut(&place) {
                l.restoring = false;
                if !done {
                    l.restore_next = Some(Instant::now() + RESTORE_RETRY);
                }
            }
        });
    }
    for (name, sid) in sirens {
        let e = engine.clone();
        std::thread::spawn(move || {
            // (finish_siren clears the record of siren `sid` only: never a newer one.)
            let r = finish_siren(&e, &name, sid);
            let gave_up = e.controls.with(&name, |c| {
                c.stopping = false;
                match &r {
                    Ok(()) => {
                        c.stop_next = None;
                        c.stop_first = None;
                        false
                    }
                    Err(why) => {
                        let first = *c.stop_first.get_or_insert_with(Instant::now);
                        if first.elapsed() >= SIREN_STOP_GIVE_UP {
                            if c.siren_id == sid {
                                c.siren_until_ms = None;
                            }
                            c.stop_next = None;
                            c.stop_first = None;
                            warn!("camera '{name}': gave up stopping the siren ({why}); the camera stops it by itself");
                            c.siren_id == sid
                        } else {
                            // Again in 1, 2, 4, 8, then every 15 seconds; said once.
                            c.stop_fails = if c.stop_next.is_none() { 0 } else { c.stop_fails + 1 };
                            if c.stop_fails == 0 {
                                warn!("camera '{name}': could not stop the siren when its time was up ({why}); it stops by itself, trying again");
                            }
                            c.stop_next = Some(Instant::now() + Duration::from_secs(1u64 << c.stop_fails.min(4)).min(Duration::from_secs(15)));
                            false
                        }
                    }
                }
            });
            if gave_up {
                persist(&e, "siren", &name, None);
            }
        });
    }
}

/// The light's time is up: give it back. Whether that is done (or given up).
/// The camera reached at `place` now (any name), with its target.
/// (Its controls turned off count too: the camera is still configured there.)
fn camera_at(engine: &Engine, place: &str) -> Option<(Arc<CameraHandle>, Target)> {
    let cams: Vec<Arc<CameraHandle>> = engine.cameras.read().unwrap().iter().flatten().cloned().collect();
    cams.into_iter().find_map(|h| {
        let mut cfg = h.cfg.read().unwrap().clone();
        if let Some(k) = cfg.controls.as_mut().filter(|k| k.kind.as_deref().map(str::trim) == Some("off")) {
            k.kind = None;
        }
        target(&cfg).ok().filter(|t| t.place == place).map(|t| (h.clone(), t))
    })
}

fn finish_light(engine: &Engine, place: &str, f: &Forced) -> bool {
    let who = Caller { user: "(time up)".into(), ip: String::new(), source: "web" };
    let name = if f.name.is_empty() { place } else { f.name.as_str() };
    let give_up = |why: &str| {
        warn!("camera '{name}': could not give the spotlight back its own setting: {why}");
        audit(engine, &who, name, "spotlight back to the camera's own setting", &Err(why.to_string()));
        store_forced(engine, place, None);
        true
    };
    if now_ms() - f.since_ms > RESTORE_GIVE_UP_MS {
        return give_up("still failing after a day; gave up");
    }
    // A camera reached at the same place now (renamed or not): tried until it works.
    // None (its address changed, its controls turned off, removed): one try the way
    // it was turned on (this run), never through whatever is at a new address.
    // Only to a camera still configured at that place (renamed or not, its
    // controls on or off). Never a sign-in at an address no camera is set to any
    // more: whatever listens there now would be sent the camera's password.
    let Some((h, t)) = camera_at(engine, place) else {
        return give_up("no camera is configured at that address any more (removed, or its address changed); nothing was sent there");
    };
    // Through a way that can switch the light: the one it was turned on with (this
    // run; the same device, account and channel), else the camera's web address now.
    // Never over Reolink port 9000 (it does not switch lights).
    let via = engine.controls.lights.lock().unwrap().get(place).and_then(|l| l.via.clone()).filter(|(v, _)| v.place == place).map(|(_, b)| b);
    let Some(base) = via.filter(|b| !baichuan_drv::is_base(b)).or_else(|| t.bases.iter().find(|b| !baichuan_drv::is_base(b)).cloned()) else {
        return give_up("the camera is now reached only over Reolink port 9000, which does not switch its spotlight; nothing was sent");
    };
    let found = Some((h, t.clone()));
    // A battery camera asleep has its light off anyway; given back when it wakes.
    if let Some((h, _)) = &found {
        if !askable(h, &t) {
            return false;
        }
    }
    let cam_name = found.as_ref().map(|(h, _)| h.cfg.read().unwrap().name.clone());
    // (What the camera has as read the way it is reached now may list no light: port 9000.)
    let ab = cam_name.as_deref().and_then(|n| engine.controls.with(n, |c| c.ab.clone())).filter(|a| a.light.is_some()).unwrap_or(Abilities { light: Some(LightCaps { on_off: true, ..Default::default() }), ..Default::default() });
    let op = engine.controls.op(place);
    let _g = op.lock().unwrap();
    // (Turned on again meanwhile, or off by hand.)
    if forced_of(engine, place).as_ref() != Some(f) {
        return true;
    }
    let r = t
        .driver(Some(base.as_str()))
        .map_err(|e| format!("the camera did not answer: {}", crate::engine::redact_url(format!("{e:#}"))))
        .and_then(|d| restore_light(&*d, &ab, f));
    if let (Ok(reading), Some(n)) = (&r, &cam_name) {
        engine.controls.with(n, |c| c.reading = Some((reading.clone(), Instant::now())));
    }
    let r = r.map(|_| ());
    // In the audit: the first failure (it is tried again every half minute) and the end.
    let first_fail = {
        let mut g = engine.controls.lights.lock().unwrap();
        match g.get_mut(place) {
            Some(l) if r.is_err() => {
                l.restore_fails += 1;
                l.restore_fails == 1
            }
            _ => false,
        }
    };
    if r.is_ok() || first_fail {
        audit(engine, &who, name, "spotlight back to the camera's own setting", &r);
    }
    if r.is_ok() {
        store_forced(engine, place, None);
    }
    r.is_ok()
}

/// Stop siren `sid` of camera `name` (its time is up). Its record goes when
/// done; a newer siren's record stays. No action lock is taken (as for the
/// Stop button: a stop never waits behind a light's write and read-back);
/// the timer's `stopping` mark keeps it to one at a time per camera.
fn finish_siren(engine: &Engine, name: &str, sid: u64) -> Result<(), String> {
    // The camera configured where the siren is (renamed or not), else the one
    // of that name at its address now. Never an address no camera is set to.
    let place = engine.controls.with(name, |c| c.siren_place.clone());
    let found = place.as_deref().filter(|p| !p.is_empty()).and_then(|p| camera_at(engine, p)).or_else(|| by_name(engine, name));
    let Some((h, t)) = found else {
        siren_stopped(engine, name, sid);
        return Ok(());
    };
    if !askable(&h, &t) {
        // (It stops by itself; nothing wakes the camera for this.)
        siren_stopped(engine, name, sid);
        return Ok(());
    }
    let d = driver_now(engine, name, &t).map_err(|r| r.msg)?;
    d.siren_stop().map_err(|e| crate::engine::redact_url(format!("{e:#}")))?;
    siren_stopped(engine, name, sid);
    Ok(())
}

/// Load what a restart left: lights to give back, sirens to stop.
fn load_pending(engine: &Engine) {
    for (k, v) in engine.db.ui_scope(SCOPE).unwrap_or_default() {
        if let Some(saved) = k.strip_prefix("light|") {
            match serde_json::from_str::<Forced>(&v) {
                Ok(mut f) if !f.key.is_empty() => {
                    // (A clock that jumped back must not stretch it.)
                    f.until_ms = f.until_ms.min(now_ms() + i64::from(LIGHT_MAX_SECS) * 1000);
                    // Where it is: the first four parts of the key (an older key also had a
                    // password-derived part, which is not kept).
                    let place = place_from_key(&f.key);
                    f.key = place.clone();
                    if saved != place {
                        persist(engine, "light", saved, None);
                    }
                    store_forced(engine, &place, Some(f));
                }
                Ok(_) => {
                    warn!("controls: dropped a saved spotlight '{saved}' that does not say where it is: it is not given back");
                    persist(engine, "light", saved, None);
                }
                Err(e) => {
                    warn!("controls: dropped a saved spotlight '{saved}' that could not be read ({e}): it is not given back");
                    persist(engine, "light", saved, None);
                }
            }
        } else if let Some(name) = k.strip_prefix("siren|") {
            // ("until|place"; an older record has only the time.)
            if let Some((_, place)) = v.split_once('|').filter(|(_, p)| !p.is_empty()) {
                engine.controls.with(name, |c| c.siren_place = Some(place.to_string()));
            }
            // Stopped now (the time asked for may not be over, but a restart is a good moment to be sure).
            engine.controls.with(name, |c| c.siren_until_ms = Some(0));
        }
    }
}

/// Start the controls: the timer (lights given back, sirens stopped), and
/// the slow background ask of what each camera has (wired cameras now and
/// then; battery cameras only while they are awake anyway).
pub fn start(engine: Arc<Engine>) {
    if engine.controls.started.swap(true, Ordering::SeqCst) {
        return;
    }
    load_pending(&engine);
    {
        let e = engine.clone();
        std::thread::Builder::new()
            .name("controls-timer".into())
            .spawn(move || {
                let mut n = 0u64;
                loop {
                    tick(&e);
                    n += 1;
                    if n % 240 == 0 {
                        session::reap();
                    }
                    std::thread::sleep(Duration::from_millis(250));
                }
            })
            .ok();
    }
    std::thread::Builder::new()
        .name("controls-probe".into())
        .spawn(move || {
            // Cameras connect first.
            std::thread::sleep(Duration::from_secs(20));
            loop {
                background_pass(&engine);
                std::thread::sleep(Duration::from_secs(30));
            }
        })
        .ok();
}

/// One pass: ask cameras what they have where due; refresh the spotlight
/// state of wired cameras for Home Assistant now and then.
fn background_pass(engine: &Engine) {
    let cams: Vec<Arc<CameraHandle>> = engine.cameras.read().unwrap().iter().flatten().cloned().collect();
    let mqtt = engine.mqtt.get().is_some();
    for h in cams {
        if !h.active.load(Ordering::Relaxed) {
            continue;
        }
        let cfg = h.cfg.read().unwrap().clone();
        let Ok(t) = target(&cfg) else { continue };
        let Some(ab) = ensure(engine, &cfg.name, &t, &h, false) else { continue };
        // Battery and hub cameras: never asked for their state on a timer.
        if mqtt && t.timers_ok() && ab.light.is_some() {
            let old = engine.controls.with(&cfg.name, |c| c.reading.as_ref().is_none_or(|(_, at)| at.elapsed() > Duration::from_secs(60)));
            if old {
                read_now(engine, &cfg.name, &t, &ab);
            }
        }
    }
}

/// Shutdown: give back lights AmberSight turned on and stop sirens (within
/// `wait`). (The sessions are signed out once the cameras have stopped:
/// `session::logout_all`.)
pub fn shutdown(engine: &Engine, wait: Duration) {
    let until = Instant::now() + wait;
    let lights: Vec<(String, Forced)> = engine.controls.lights.lock().unwrap().iter().map(|(p, l)| (p.clone(), l.f.clone())).collect();
    let sirens: Vec<(String, u64)> = engine.controls.cams.lock().unwrap().iter().filter(|(_, c)| c.siren_until_ms.is_some_and(|u| u > now_ms())).map(|(n, c)| (n.clone(), c.siren_id)).collect();
    for (name, sid) in sirens {
        if Instant::now() < until {
            let _ = finish_siren(engine, &name, sid);
        }
    }
    for (place, f) in lights {
        if Instant::now() < until {
            let f = Forced { until_ms: 0, ..f };
            if let Some(l) = engine.controls.lights.lock().unwrap().get_mut(&place) {
                l.f.until_ms = 0;
            }
            finish_light(engine, &place, &f);
        }
    }
}
