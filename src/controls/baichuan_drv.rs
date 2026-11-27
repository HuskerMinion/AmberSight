//! Camera controls for Reolink cameras over Baichuan, Reolink's own protocol
//! on TCP port 9000: for cameras whose web interface is off (the E1 Pro has
//! no switch for it). Everything goes through the device's one shared
//! Baichuan session (`ingest::baichuan::wired`, the same one live video over
//! port 9000 uses): Reolink firmware allows about two sessions and the
//! Reolink app needs one, so controls never open a second.
//!
//! What it uses (message ids, all on `bc::ALLOWED` or `bc::CONTROL_WRITES`):
//!   * 199 (read) `<Support>`: per channel `<item>` with `<chnID>`, flag
//!     fields read as bit masks: `audioVersion` bit 2 = the siren can be
//!     played; `ledCtrl` bit 0 = a status light, bits 1 and 2 = a spotlight;
//!     `battery` above 0 = a battery camera.
//!   * 151 (read) `<AbilityInfo>` for the account, part `image`: a
//!     `subModule` whose `abilityValue` lists `ledState_rw` = this account
//!     may change the infrared lights.
//!   * 208 (read) `<LedState>`: `state` (infrared: auto / open / close) and
//!     `lightState` (status light: open / close).
//!   * 209 (write) `<LedState>`: the 208 answer sent back with one field changed.
//!   * 263 (write) `<audioPlayInfo>`: `playMode` 0 with `playTimes` rounds
//!     and `onOff` `ctl::TIMED_ON_OFF` sounds the siren that many rounds (the
//!     camera is meant to stop by itself: UNPROVEN until the live test);
//!     `playMode` 2 with `onOff` 0 is the manual switch, off (the stop).
//!   * The siren's sound (`siren_sound.rs`; as the official app sends it,
//!     2026-11-27): 267 (read) what sound it takes, 264 (read) / 265 (write)
//!     `<audioCfg>` (`audioSelect` 1: the custom sound), 260 (read) the stored
//!     sound's size, 261 (write) a sound's header then its pieces, 262 (write)
//!     its end. Also for a channel of a hub, while the hub reports it awake.
//!
//! Not done here (UNVERIFIED or not wanted yet): the spotlight (a camera
//! that has one says so on its page, but it is not switched over port 9000:
//! its on/off state is only pushed by the camera, never answered to a
//! question, so it could not be read first and given back), the doorbell's
//! light, battery cameras (never woken), and channels of hubs and recorders.

use anyhow::{anyhow, bail, Result};
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use super::{Abilities, DevKind, Driver, LightState, Reading};
use crate::ingest::baichuan::{bc, ctl, wired};

/// The prefix of a Baichuan control address ("baichuan-tcp://host:port").
/// (Not `baichuan://`: that is a battery camera's stream address.)
pub const PREFIX: &str = "baichuan-tcp://";

pub fn is_base(b: &str) -> bool {
    b.starts_with(PREFIX)
}

/// The control address for `host` (as `host_port` writes it) and `port`.
pub fn base_for(host: &str, port: u16) -> String {
    let h = if host.contains(':') && !host.starts_with('[') { format!("[{host}]") } else { host.to_string() };
    format!("{PREFIX}{h}:{port}")
}

fn split(base: &str) -> Result<(String, u16)> {
    let rest = base.strip_prefix(PREFIX).ok_or_else(|| anyhow!("not a Baichuan address"))?;
    let (h, p) = rest.rsplit_once(':').ok_or_else(|| anyhow!("no port in the Baichuan address"))?;
    let port = p.parse::<u16>().map_err(|_| anyhow!("bad port in the Baichuan address"))?;
    let h = h.strip_prefix('[').and_then(|x| x.strip_suffix(']')).unwrap_or(h);
    Ok((h.to_string(), port))
}

/// Who the controls are to the shared session (they own no stream on it).
const OWNER: &str = "(camera controls)";

/// How long a page's state read waits for the camera to be free.
const IDLE_WAIT: Duration = Duration::from_millis(300);
/// A page's state read signs in to a camera whose session is not signed in
/// at most this often (otherwise the last reading is shown): an open page,
/// or an app asking over and over, never keeps a session signed in or signs
/// in again and again.
#[cfg(not(test))]
const READ_SIGN_IN_GAP: Duration = Duration::from_secs(5 * 60);
#[cfg(test)]
const READ_SIGN_IN_GAP: Duration = Duration::from_secs(30);

/// The longest one round of the siren may be: `playDuration` is 10 in every
/// public description, so a round is taken as up to 10 s until a real camera
/// is measured. With at most `MAX_ROUNDS`, a stop that never arrives still
/// leaves the siren on for about 30 s at most.
pub const ROUND_SECS: u32 = 10;
pub const MAX_ROUNDS: u32 = 3;

/// How many rounds for `secs` seconds (at least one, at most MAX_ROUNDS).
pub fn rounds_for(secs: u32) -> u32 {
    secs.div_ceil(ROUND_SECS).clamp(1, MAX_ROUNDS)
}

/// When a page's read last signed in, per device.
fn read_sign_ins() -> &'static Mutex<HashMap<String, Instant>> {
    static T: std::sync::OnceLock<Mutex<HashMap<String, Instant>>> = std::sync::OnceLock::new();
    T.get_or_init(Default::default)
}

/// One action at a time per device (on top of the session's one exchange at
/// a time): a light's read-then-write is not split by another.
fn turn(key: &str) -> Arc<Mutex<()>> {
    static T: Mutex<Option<HashMap<String, Arc<Mutex<()>>>>> = Mutex::new(None);
    T.lock().unwrap_or_else(|e| e.into_inner()).get_or_insert_with(HashMap::new).entry(key.to_string()).or_default().clone()
}

pub struct BaichuanDriver {
    host: String,
    port: u16,
    user: String,
    pass: String,
    channel: u8,
    base: String,
    /// A channel of a hub, asked only for the siren's sound while the hub reports it awake.
    hub: bool,
}

impl BaichuanDriver {
    pub fn at(base: &str, user: &str, pass: &str, channel: u32) -> Result<BaichuanDriver> {
        let (host, port) = split(base)?;
        let channel = u8::try_from(channel).map_err(|_| anyhow!("no such channel"))?;
        Ok(BaichuanDriver { host, port, user: user.to_string(), pass: pass.to_string(), channel, base: base.to_string(), hub: false })
    }

    /// For the siren's sound only: also a channel of a hub (`hub`; the
    /// caller saw the hub report it awake).
    pub fn for_sound(base: &str, user: &str, pass: &str, channel: u32, hub: bool) -> Result<BaichuanDriver> {
        let mut d = BaichuanDriver::at(base, user, pass, channel)?;
        d.hub = hub;
        Ok(d)
    }

    fn key(&self) -> String {
        format!("{}:{}", self.host, self.port)
    }

    /// The device and a fingerprint of the credentials (never the password).
    fn read_key(&self) -> String {
        use sha2::Digest;
        let h = sha2::Sha256::digest(format!("{}\u{0}{}", self.user, self.pass).as_bytes());
        format!("{}|{}", self.key(), h[..8].iter().map(|b| format!("{b:02x}")).collect::<String>())
    }

    /// One exchange; the camera's answer XML when it says done (200).
    fn ask(&self, msg_id: u32, ext: String, body: String, how: How) -> Result<String> {
        let c = wired::client_for(&self.host, self.port, &self.user, &self.pass, OWNER).map_err(|e| anyhow!(NotSent(e)))?;
        let (write, urgent, keep) = match how {
            How::Read => (false, false, false),
            How::Ask => (false, false, true),
            How::Write => (true, false, true),
            How::Stop => (true, true, true),
        };
        let a = c.control(msg_id, self.channel, ext, body, write, urgent, keep).map_err(|e| if e.contains(ctl::NOT_SENT) { anyhow!(NotSent(e)) } else { anyhow!(e) })?;
        if a.status != 200 {
            return Err(anyhow!(Refusal(a.status)));
        }
        Ok(a.xml)
    }

    fn led_xml(&self, how: How) -> Result<String> {
        self.ask(bc::MSG_LED_GET, ctl::channel_ext(self.channel), String::new(), how)
    }

    fn read_on(&self, ab: &Abilities, how: How) -> Result<Reading> {
        let mut r = Reading::default();
        if ab.ir.is_empty() && ab.status_led.is_empty() {
            return Ok(r);
        }
        let x = self.led_xml(how)?;
        if !ab.ir.is_empty() {
            r.ir = ctl::text(&x, "state").and_then(ir_word).map(str::to_string);
        }
        if !ab.status_led.is_empty() {
            r.status_led = ctl::text(&x, "lightState").and_then(led_word).map(str::to_string);
        }
        Ok(r)
    }

    /// Change one field of the lights' state: read it (208), send it back with
    /// that field changed (209).
    fn set_led_field(&self, field: &str, value: &str, what: &str) -> Result<()> {
        let x = self.led_xml(How::Ask).map_err(|e| plain(e, what))?;
        let el = ctl::element(&x, "LedState").ok_or_else(|| anyhow!("the camera did not give its lights' state"))?;
        let changed = ctl::with_text(el, field, value).ok_or_else(|| anyhow!("the camera says it cannot {what}"))?;
        let body = format!("<?xml version=\"1.0\" encoding=\"UTF-8\" ?>\n<body>\n{changed}\n</body>\n");
        self.ask(bc::MSG_LED_SET, ctl::channel_ext(self.channel), body, How::Write).map(|_| ()).map_err(|e| plain(e, what))
    }

    /// What the camera has, from its answers to 199, 151 and 208.
    pub fn probe_from(support: &str, ability: Option<&str>, led: Option<&str>, channel: u8) -> Result<Abilities> {
        let c = claimed(support, ability, channel);
        if c.battery {
            bail!("{}", ctl::NOT_FOR_BATTERY);
        }
        let mut ab = Abilities { siren: c.siren, light_unswitched: c.light, ..Default::default() };
        if let Some(x) = led {
            if c.ir && ctl::text(x, "state").and_then(ir_word).is_some() {
                // (Seen over the web API on every Reolink camera so far: Auto and Off, no On.)
                ab.ir = vec!["Auto".into(), "Off".into()];
            }
            if c.status_led && ctl::text(x, "lightState").and_then(led_word).is_some() {
                ab.status_led = vec!["On".into(), "Off".into()];
            }
        }
        Ok(ab)
    }
}

// ------------------------------------------------------- the siren's sound

/// The pieces a sound is sent in (the last may be shorter), as the app does.
pub const SOUND_PIECE: usize = 2048;
/// A whole sound sent (header, pieces, end) within this, past the sign-in.
#[cfg(not(test))]
const SOUND_JOB_TIME: Duration = Duration::from_secs(60);
#[cfg(test)]
const SOUND_JOB_TIME: Duration = Duration::from_secs(10);

/// What a camera says of its siren's sound.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct SoundState {
    /// It takes a sound of its own (ADPCM, 16 kHz, in its encode ability).
    pub takes: bool,
    /// The siren uses the custom sound (Some(false): the built-in one; None: not read).
    pub custom: Option<bool>,
    /// The size of the stored custom sound (0: none). None: not read, or
    /// not reliable (behind a hub).
    pub stored_bytes: Option<u64>,
}

/// Whether a 267 answer lists IMA ADPCM at 16 kHz.
pub fn takes_adpcm_16k(ability: &str) -> bool {
    ctl::elements(ability, "audioEncode").iter().any(|e| {
        ctl::text(e, "audioType").is_some_and(|t| t.eq_ignore_ascii_case("adpcm")) && ctl::text(e, "sampleRate") == Some("16000")
    })
}

impl BaichuanDriver {
    /// One exchange (or job) for the siren's sound; the camera's answer, any
    /// status. `keep`: a change (a page's read does not keep the session).
    fn ask_sound(&self, msg_id: u32, ext: String, body: String, then: Vec<ctl::Step>, write: bool, keep: bool) -> Result<ctl::Answer> {
        let c = wired::client_for(&self.host, self.port, &self.user, &self.pass, OWNER).map_err(|e| anyhow!(NotSent(e)))?;
        let within = if then.is_empty() { ctl::ANSWER_WAIT } else { SOUND_JOB_TIME };
        c.control_job(msg_id, self.channel, ext, body, then, write, self.hub, keep, within).map_err(|e| if e.contains(ctl::NOT_SENT) { anyhow!(NotSent(e)) } else { anyhow!(e) })
    }

    fn sound_read(&self, msg_id: u32, keep: bool) -> Result<String> {
        let a = self.ask_sound(msg_id, ctl::read_ext(self.channel), String::new(), Vec::new(), false, keep)?;
        if a.status != 200 {
            return Err(anyhow!(Refusal(a.status)));
        }
        Ok(a.xml)
    }

    /// The siren's sound now: what the camera takes (267), which sound it
    /// uses (264) and, not behind a hub, the stored sound's size (260).
    /// (Reads as a page's: they do not keep a controls-only session.)
    pub fn sound_state(&self) -> Result<SoundState> {
        let t = turn(&self.key());
        let _g = t.lock().unwrap_or_else(|e| e.into_inner());
        let takes = match self.sound_read(bc::MSG_AUDIO_ENCODE_ABILITY, false) {
            Ok(x) => takes_adpcm_16k(&x),
            // (A camera that does not know the question takes no sound.)
            Err(e) if e.downcast_ref::<Refusal>().is_some() => false,
            Err(e) => return Err(e),
        };
        if !takes {
            return Ok(SoundState::default());
        }
        let cfg = self.sound_read(bc::MSG_AUDIO_CFG_GET, false).map_err(|e| plain(e, "say which siren sound it uses"))?;
        let custom = ctl::text(&cfg, "audioSelect").map(|v| v == "1");
        let stored_bytes = if self.hub { None } else { self.sound_read(bc::MSG_AUDIO_FILE_INFO, false).ok().and_then(|x| ctl::text(&x, "fileSize").and_then(|v| v.parse().ok())) };
        Ok(SoundState { takes, custom, stored_bytes })
    }

    /// Switch the siren to its custom sound or its built-in one: the
    /// camera's own settings (264) back with only that changed (265), then
    /// read again to see it took.
    fn sound_pick_on(&self, custom: bool) -> Result<()> {
        let what = if custom { "switch its siren to the custom sound" } else { "switch its siren to its built-in sound" };
        let cfg = self.sound_read(bc::MSG_AUDIO_CFG_GET, true).map_err(|e| plain(e, what))?;
        let body = ctl::audio_cfg_xml(&cfg, custom, None).ok_or_else(|| anyhow!(super::NotDone("the camera did not say which siren sound it uses".into())))?;
        let a = self.ask_sound(bc::MSG_AUDIO_CFG_SET, ctl::channel_ext(self.channel), body, Vec::new(), true, true)?;
        if a.status != 200 {
            return Err(plain(anyhow!(Refusal(a.status)), what));
        }
        let back = self.sound_read(bc::MSG_AUDIO_CFG_GET, true).map_err(|e| anyhow!("the camera took the change but its setting could not be read back: {e:#}"))?;
        if ctl::text(&back, "audioSelect") != Some(if custom { "1" } else { "0" }) {
            bail!("the camera answered that it was done, but its siren sound did not change");
        }
        Ok(())
    }

    pub fn sound_pick(&self, custom: bool) -> Result<()> {
        let t = turn(&self.key());
        let _g = t.lock().unwrap_or_else(|e| e.into_inner());
        self.sound_pick_on(custom)
    }

    /// Send `adpcm` (the camera's format, see `siren_sound`) as the custom
    /// sound, then switch the siren to it. One job on the session: the
    /// header (261), the pieces (261, binary, the header's message number),
    /// the end (262); any answer but 200 stops it and says which part.
    pub fn sound_send(&self, adpcm: &[u8]) -> Result<()> {
        if adpcm.is_empty() {
            bail!("no sound to send");
        }
        let t = turn(&self.key());
        let _g = t.lock().unwrap_or_else(|e| e.into_inner());
        // (Its settings first: they must be there to switch to the sound after.)
        let cfg = self.sound_read(bc::MSG_AUDIO_CFG_GET, true).map_err(|e| plain(e, "say which siren sound it uses"))?;
        if ctl::text(&cfg, "audioSelect").is_none() {
            return Err(anyhow!(super::NotDone("the camera did not say which siren sound it uses".into())));
        }
        let pieces: Vec<&[u8]> = adpcm.chunks(SOUND_PIECE).collect();
        let n = pieces.len();
        let mut then: Vec<ctl::Step> = pieces.iter().map(|p| ctl::Step { msg_id: bc::MSG_AUDIO_FILE_SEND, ext: ctl::binary_ext(self.channel), payload: p.to_vec(), raw: true, same_num: true }).collect();
        then.push(ctl::Step { msg_id: bc::MSG_AUDIO_FILE_DONE, ext: ctl::channel_ext(self.channel), payload: Vec::new(), raw: false, same_num: false });
        let a = self.ask_sound(bc::MSG_AUDIO_FILE_SEND, ctl::channel_ext(self.channel), ctl::sound_header_xml(self.channel, adpcm.len()), then, true, true)?;
        if a.status != 200 {
            let part = match a.step {
                0 => "the sound's header".to_string(),
                i if i <= n => format!("part {i} of {n} of the sound"),
                _ => "the end of the sound".to_string(),
            };
            return Err(plain(anyhow!(Refusal(a.status)), &format!("take {part}")));
        }
        self.sound_pick_on(true).map_err(|e| anyhow!("the sound was sent, but the siren could not be switched to it: {e:#}"))
    }
}

/// How an exchange goes on the session.
#[derive(Clone, Copy)]
enum How {
    /// A page's state read (does not keep the session).
    Read,
    /// The ask of what a camera has, or the read before a write.
    Ask,
    Write,
    /// The siren's stop: ahead of everything waiting.
    Stop,
}

/// The request never reached the camera (so nothing it would do is being done).
#[derive(Debug)]
pub struct NotSent(pub String);

impl std::fmt::Display for NotSent {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for NotSent {}

/// The camera answered, and not with "done".
#[derive(Debug)]
pub struct Refusal(pub u16);

impl std::fmt::Display for Refusal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "the camera refused it (Baichuan status {})", self.0)
    }
}

impl std::error::Error for Refusal {}

fn plain(e: anyhow::Error, what: &str) -> anyhow::Error {
    match e.downcast_ref::<Refusal>() {
        // (401: this account may not.)
        Some(Refusal(401)) => anyhow!(super::NotDone(format!("the camera's account may not {what} (sign in to the camera with an admin account)"))),
        Some(Refusal(s)) => anyhow!(super::NotDone(format!("the camera did not {what} (Baichuan status {s})"))),
        None => e,
    }
}

/// The infrared lights' word on the camera, in the words pages use.
fn ir_word(s: &str) -> Option<&'static str> {
    match s.trim().to_ascii_lowercase().as_str() {
        "auto" => Some("Auto"),
        "close" => Some("Off"),
        "open" => Some("On"),
        _ => None,
    }
}

fn ir_wire(s: &str) -> Option<&'static str> {
    match s {
        "Auto" => Some("auto"),
        "Off" => Some("close"),
        "On" => Some("open"),
        _ => None,
    }
}

fn led_word(s: &str) -> Option<&'static str> {
    match s.trim().to_ascii_lowercase().as_str() {
        "open" => Some("On"),
        "close" => Some("Off"),
        _ => None,
    }
}

fn led_wire(s: &str) -> Option<&'static str> {
    match s {
        "On" => Some("open"),
        "Off" => Some("close"),
        _ => None,
    }
}

/// What 199 and 151 say a channel has.
#[derive(Debug, Default, PartialEq)]
pub struct Claimed {
    pub siren: bool,
    pub status_led: bool,
    pub light: bool,
    pub ir: bool,
    pub battery: bool,
}

fn num(x: &str, name: &str) -> u64 {
    ctl::text(x, name).and_then(|v| v.parse::<u64>().ok()).unwrap_or(0)
}

/// Read the `<Support>` answer (199) and the `<AbilityInfo>` answer (151,
/// None when it failed) for `channel`.
pub fn claimed(support: &str, ability: Option<&str>, channel: u8) -> Claimed {
    let item = ctl::elements(support, "item").into_iter().find(|i| ctl::text(i, "chnID").and_then(|v| v.parse::<u32>().ok()) == Some(channel as u32));
    let Some(item) = item else { return Claimed::default() };
    // (The channel's own fields, not those of a sub-item inside it.)
    let mut own = item.to_string();
    while let Some(sub) = ctl::element(&own, "subItem").map(str::to_string) {
        own = own.replacen(&sub, "", 1);
    }
    let led = num(&own, "ledCtrl");
    let audio = num(&own, "audioVersion");
    let ir = match ability {
        Some(a) => ctl::element(a, "image").is_some_and(|img| {
            ctl::elements(img, "subModule").iter().any(|m| {
                let ch_ok = ctl::text(m, "channelId").is_none_or(|c| c.parse::<u32>().ok() == Some(channel as u32));
                ch_ok && ctl::text(m, "abilityValue").is_some_and(|v| v.split(',').any(|w| w.trim() == "ledState_rw"))
            })
        }),
        // No answer to 151: the state read (208) decides.
        None => true,
    };
    Claimed { siren: audio >> 2 & 1 == 1, status_led: led & 1 == 1, light: led >> 1 & 1 == 1 && led >> 2 & 1 == 1, ir, battery: num(&own, "battery") > 0 }
}

impl Driver for BaichuanDriver {
    fn base(&self) -> String {
        self.base.clone()
    }

    fn probe(&self) -> Result<Abilities> {
        let t = turn(&self.key());
        let _g = t.lock().unwrap_or_else(|e| e.into_inner());
        let support = self.ask(bc::MSG_SUPPORT, String::new(), String::new(), How::Ask)?;
        let ability = self.ask(bc::MSG_ABILITY, ctl::ability_ext(&self.user), String::new(), How::Ask).ok();
        let led = match self.led_xml(How::Ask) {
            Ok(x) => Some(x),
            // The camera has no such state: none of those lights.
            Err(e) if e.downcast_ref::<Refusal>().is_some() => None,
            Err(e) => return Err(e),
        };
        BaichuanDriver::probe_from(&support, ability.as_deref(), led.as_deref(), self.channel)
    }

    fn read(&self, ab: &Abilities) -> Result<Reading> {
        // (The read-back after a write, or the state before one: part of that action.)
        let t = turn(&self.key());
        let _g = t.lock().unwrap_or_else(|e| e.into_inner());
        self.read_on(ab, How::Ask)
    }

    fn read_if_idle(&self, ab: &Abilities) -> Option<Result<Reading>> {
        // Not signed in now: sign in for a page's read only now and then.
        let signed_in = wired::status(&self.host, self.port).is_some_and(|s| s.signed_in);
        if !signed_in {
            // (By the credentials too: once a password is fixed, the next read signs in.)
            let k = self.read_key();
            let mut m = read_sign_ins().lock().unwrap_or_else(|e| e.into_inner());
            if m.get(&k).is_some_and(|t| t.elapsed() < READ_SIGN_IN_GAP) {
                return None;
            }
            m.insert(k, Instant::now());
        }
        let t = turn(&self.key());
        let until = Instant::now() + IDLE_WAIT;
        loop {
            match t.try_lock() {
                Ok(_g) => return Some(self.read_on(ab, How::Read)),
                Err(std::sync::TryLockError::Poisoned(p)) => {
                    let _g = p.into_inner();
                    return Some(self.read_on(ab, How::Read));
                }
                Err(std::sync::TryLockError::WouldBlock) if Instant::now() >= until => return None,
                Err(std::sync::TryLockError::WouldBlock) => std::thread::sleep(Duration::from_millis(10)),
            }
        }
    }

    fn set_light(&self, _w: &LightState) -> Result<()> {
        bail!("the spotlight is not switched over Reolink port 9000 yet")
    }

    fn siren_play(&self, secs: u32) -> Result<()> {
        let t = turn(&self.key());
        let _g = t.lock().unwrap_or_else(|e| e.into_inner());
        self.ask(bc::MSG_AUDIO_PLAY, ctl::channel_ext(self.channel), ctl::siren_rounds_xml(self.channel, rounds_for(secs)), How::Write).map(|_| ()).map_err(|e| plain(e, "sound its siren"))
    }

    fn siren_stop(&self) -> Result<()> {
        // (Not behind another action's turn: it goes ahead of what waits on the session.)
        self.ask(bc::MSG_AUDIO_PLAY, ctl::channel_ext(self.channel), ctl::siren_off_xml(self.channel), How::Stop).map(|_| ()).map_err(|e| plain(e, "stop its siren"))
    }

    fn set_ir(&self, state: &str) -> Result<()> {
        let w = ir_wire(state).ok_or_else(|| anyhow!("not an infrared light setting"))?;
        let t = turn(&self.key());
        let _g = t.lock().unwrap_or_else(|e| e.into_inner());
        self.set_led_field("state", w, "change its infrared lights")
    }

    fn set_status_led(&self, state: &str) -> Result<()> {
        let w = led_wire(state).ok_or_else(|| anyhow!("not a status light setting"))?;
        let t = turn(&self.key());
        let _g = t.lock().unwrap_or_else(|e| e.into_inner());
        self.set_led_field("lightState", w, "change its status light")
    }

    /// From the sign-in reply: its channel count and type (one read is sent
    /// to sign in when the session is not; a hub's or recorder's session
    /// refuses it, which says as much).
    fn device_kind(&self) -> Result<DevKind> {
        match self.ask(bc::MSG_SUPPORT, String::new(), String::new(), How::Ask) {
            Ok(_) => {}
            Err(e) if e.to_string().contains(ctl::NOT_FOR_HUB) => return Ok(DevKind { recorder: false, multi: true }),
            Err(e) => return Err(e),
        }
        let c = wired::client_for(&self.host, self.port, &self.user, &self.pass, OWNER).map_err(|e| anyhow!(e))?;
        let st = c.session_status();
        let ty = st.device_type.unwrap_or_default().to_ascii_lowercase();
        let multi = st.channels.is_some_and(|n| n > 1) || ty.contains("nvr") || ty.contains("hub");
        Ok(DevKind { recorder: ty.contains("nvr"), multi })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const E1: &str = "<body><Support version=\"1.1\"><channelNum>1</channelNum><item><chnID>0</chnID><ledCtrl>1</ledCtrl><audioVersion>4</audioVersion><battery>0</battery></item></Support></body>";

    #[test]
    fn what_a_camera_has_comes_from_its_flags() {
        let img = "<AbilityInfo><userName>admin</userName><image><subModule><channelId>0</channelId><abilityValue>ledState_rw, ispBasic_rw</abilityValue></subModule></image></AbilityInfo>";
        let c = claimed(E1, Some(img), 0);
        assert_eq!(c, Claimed { siren: true, status_led: true, light: false, ir: true, battery: false });
        // ledState read-only for this account: no infrared setting offered.
        let ro = img.replace("ledState_rw", "ledState_ro");
        assert!(!claimed(E1, Some(&ro), 0).ir);
        // No answer to 151: the state read decides.
        assert!(claimed(E1, None, 0).ir);
        // A channel it does not list has nothing.
        assert_eq!(claimed(E1, Some(img), 1), Claimed::default());
        // A spotlight needs both bits 1 and 2; the siren needs bit 2 of audioVersion.
        let s = E1.replace("<ledCtrl>1</ledCtrl>", "<ledCtrl>7</ledCtrl>").replace("<audioVersion>4</audioVersion>", "<audioVersion>3</audioVersion>");
        let c = claimed(&s, Some(img), 0);
        assert!(c.light && c.status_led && !c.siren);
        assert!(!claimed(&E1.replace("<ledCtrl>1</ledCtrl>", "<ledCtrl>2</ledCtrl>"), None, 0).light);
        // A sub-item's fields are not the channel's.
        let sub = "<Support><item><chnID>0</chnID><subItem><chnID>0</chnID><audioVersion>4</audioVersion></subItem></item></Support>";
        assert!(!claimed(sub, None, 0).siren);
    }

    #[test]
    fn the_siren_is_asked_for_few_rounds() {
        assert_eq!(rounds_for(0), 1);
        assert_eq!(rounds_for(1), 1);
        assert_eq!(rounds_for(10), 1);
        assert_eq!(rounds_for(11), 2);
        assert_eq!(rounds_for(30), 3);
        assert_eq!(rounds_for(u32::MAX), MAX_ROUNDS);
        assert!(MAX_ROUNDS * ROUND_SECS <= super::super::SIREN_MAX_SECS, "a lost stop never passes the longest siren");
        assert!(ctl::siren_rounds_xml(0, 2).contains(&format!("<onOff>{}</onOff>", ctl::TIMED_ON_OFF)));
    }

    #[test]
    fn every_sub_item_is_left_out() {
        let s = "<Support><item><chnID>0</chnID><subItem><chnID>0</chnID><audioVersion>4</audioVersion></subItem><subItem><chnID>1</chnID><ledCtrl>7</ledCtrl></subItem></item></Support>";
        let c = claimed(s, None, 0);
        assert!(!c.siren && !c.light && !c.status_led, "{c:?}");
    }

    #[test]
    fn a_battery_camera_gets_nothing() {
        let b = E1.replace("<battery>0</battery>", "<battery>1</battery>");
        assert!(BaichuanDriver::probe_from(&b, None, None, 0).unwrap_err().to_string().contains("battery"));
    }

    #[test]
    fn lights_are_offered_only_when_their_state_reads() {
        let led = "<body><LedState version=\"1.1\"><channelId>0</channelId><ledVersion>2</ledVersion><state>auto</state><lightState>open</lightState></LedState></body>";
        let ab = BaichuanDriver::probe_from(E1, None, Some(led), 0).unwrap();
        assert!(ab.siren && ab.light.is_none() && !ab.light_unswitched);
        assert_eq!((ab.ir.as_slice(), ab.status_led.as_slice()), (&["Auto".to_string(), "Off".to_string()][..], &["On".to_string(), "Off".to_string()][..]));
        let ab = BaichuanDriver::probe_from(E1, None, None, 0).unwrap();
        assert!(ab.ir.is_empty() && ab.status_led.is_empty(), "no state: none offered");
        let odd = led.replace("<state>auto</state>", "<state>weird</state>");
        assert!(BaichuanDriver::probe_from(E1, None, Some(&odd), 0).unwrap().ir.is_empty(), "a word it does not know: not offered");
        assert_eq!(ir_word("close"), Some("Off"));
        assert_eq!(led_word("close"), Some("Off"));
        assert_eq!(ir_wire("Off"), Some("close"));
        assert_eq!(led_wire("Bright"), None);
    }

    #[test]
    fn addresses() {
        assert_eq!(split(&base_for("192.0.2.5", 9000)).unwrap(), ("192.0.2.5".to_string(), 9000));
        assert_eq!(split(&base_for("fd00::5", 9001)).unwrap(), ("fd00::5".to_string(), 9001));
        assert_eq!(base_for("fd00::5", 9001), "baichuan-tcp://[fd00::5]:9001");
        assert!(split("https://x").is_err());
        assert!(!is_base("baichuan://x?uid=1"), "a battery camera's address is not one");
    }
}
