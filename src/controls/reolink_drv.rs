//! Camera controls for Reolink cameras, hubs and recorders over the web API
//! Reolink documents (`/cgi-bin/api.cgi`, JSON commands): `GetAbility` for
//! what the camera has, `GetWhiteLed`/`SetWhiteLed` for the spotlight,
//! `AudioAlarmPlay` for the siren, `GetIrLights`/`SetIrLights` for the
//! infrared lights and `GetPowerLed`/`SetPowerLed` for the status light.
//!
//! What a camera offers is read from the camera (its abilities, and the
//! allowed values each "Get" lists with action 1), never from its model name:
//! firmware versions number the spotlight's modes differently.

use anyhow::{anyhow, bail, Result};
use serde_json::{json, Value};
use std::sync::Arc;

use super::session::{self, Shared};
use super::{Abilities, Driver, LightCaps, LightState, Reading};
use crate::reolink::{cam_error, is_unreachable};

/// About how long one round of the siren's sound lasts. Reolink's siren
/// plays a number of rounds and then stops by itself; AmberSight also stops
/// it when the time asked for is up. (To check on real cameras: reports put
/// one round at 2 to 8 seconds by model.)
const SIREN_ROUND_SECS: u32 = 5;
/// Most rounds asked for: the camera stops by itself after these even if the
/// stop never reaches it (a dropped network, AmberSight stopped).
const SIREN_MAX_ROUNDS: u32 = 6;

pub struct ReolinkDriver {
    shared: Arc<Shared>,
    channel: u32,
    user: String,
    base: String,
    /// The ability answer `connect` got (used by the first `probe`).
    first: std::sync::Mutex<Option<Value>>,
}

/// Find the camera's web API at one of `bases` (in turn; the next only when
/// nothing answers at one, never after a refused sign-in or a certificate
/// problem, so a password is not sent over plain HTTP for those).
pub fn connect(bases: &[String], user: &str, pass: &str, channel: u32) -> Result<ReolinkDriver> {
    let mut last = anyhow!("no address to try");
    for b in bases {
        let d = ReolinkDriver::at(b, user, pass, channel);
        match d.ability() {
            Ok(v) => {
                *d.first.lock().unwrap_or_else(|e| e.into_inner()) = Some(v);
                return Ok(d);
            }
            Err(e) => {
                let go_on = is_unreachable(&e);
                last = e;
                if !go_on {
                    break;
                }
            }
        }
    }
    Err(last)
}

/// Whether ability `k` of `obj` is there (`ver` not 0).
fn has(obj: &Value, k: &str) -> bool {
    obj[k]["ver"].as_i64().is_some_and(|v| v != 0)
}

/// Whether ability `k` is there and this account may use it (`permit` not 0:
/// a doorbell behind a hub lists `alarmAudio` with permit 0 and has no siren).
fn usable(obj: &Value, k: &str) -> bool {
    has(obj, k) && obj[k]["permit"].as_i64().is_none_or(|p| p != 0)
}

/// The ways a siren is stopped, in the order tried (see `siren_stop`).
const SIREN_STOPS: [fn() -> Value; 3] = [
    || json!({ "alarm_mode": "manu", "manual_switch": 0 }),
    || json!({ "alarm_mode": "manul", "manual_switch": 0 }),
    || json!({ "alarm_mode": "times", "times": 0, "manual_switch": 0 }),
];

/// Plain words for what went wrong with a command.
fn plain(e: anyhow::Error, what: &str) -> anyhow::Error {
    if let Some(c) = cam_error(&e) {
        if c.not_allowed() {
            return anyhow!(super::NotDone(format!("the camera's account may not {what} (sign in to the camera with an admin account)")));
        }
        if c.unsupported() {
            return anyhow!(super::NotDone(format!("the camera says it cannot {what}")));
        }
    }
    e
}

/// What the abilities say, before the "Get" commands confirm it.
#[derive(Debug, Default, PartialEq)]
pub struct Claimed {
    pub light: bool,
    pub brightness: bool,
    /// The camera's own modes it claims: on when something is seen at night,
    /// always on at night, on a schedule.
    pub smart: bool,
    pub keep_on: bool,
    pub schedule: bool,
    pub siren: bool,
    pub ir: bool,
    pub status_led: bool,
}

/// Read a `GetAbility` answer for channel `ch` (hubs and recorders list one
/// entry per channel). Seen on real cameras: `floodLight` reads 0 even on
/// floodlights, so the light is found by its `supportFL*` flags only;
/// `GetWhiteLed` answers even on cameras with no light, so it proves
/// nothing; `GetPowerLed` answers where `powerLed` is 0.
pub fn claimed(ability: &Value, ch: u32) -> Claimed {
    let a = &ability["value"]["Ability"];
    let c = &a["abilityChn"][ch as usize];
    let fl = |k: &str| has(c, k);
    Claimed {
        light: fl("supportFLswitch") || fl("supportFLBrightness") || fl("supportFLIntelligent") || fl("supportFLKeepOn") || fl("supportFLSchedule"),
        brightness: fl("supportFLBrightness"),
        smart: fl("supportFLIntelligent"),
        keep_on: fl("supportFLKeepOn"),
        schedule: fl("supportFLSchedule"),
        siren: usable(c, "alarmAudio") || (usable(a, "alarmAudio") && !a["abilityChn"].as_array().is_some_and(|l| l.len() > 1)),
        ir: has(c, "ledControl"),
        // (Asked of every camera: see above.)
        status_led: true,
    }
}

/// The values a "Get" with action 1 lists for `field` of `obj` (strings or numbers).
fn listed(r: &Value, obj: &str, field: &str) -> Option<Vec<Value>> {
    r["range"][obj][field].as_array().cloned().filter(|l| !l.is_empty())
}

/// Spotlight abilities from a `GetWhiteLed` (action 1) answer. Brightness
/// only when the abilities claim it (the range lists it either way).
pub fn light_caps(r: &Value, claimed_brightness: bool) -> LightCaps {
    let b = &r["range"]["WhiteLed"]["bright"];
    let brightness = match (b["min"].as_u64(), b["max"].as_u64()) {
        _ if !claimed_brightness => None,
        (Some(lo), Some(hi)) if hi > lo && hi <= 1000 => Some((lo as u32, hi as u32)),
        // No range given: Reolink's own 0-100.
        _ => Some((0, 100)),
    };
    let modes = listed(r, "WhiteLed", "mode").map(|l| l.iter().filter_map(|m| m.as_i64()).collect()).unwrap_or_default();
    LightCaps { on_off: r["value"]["WhiteLed"]["state"].is_i64() || r["value"]["WhiteLed"]["state"].is_u64(), brightness, modes, ..Default::default() }
}

pub fn light_state(r: &Value) -> LightState {
    let w = &r["value"]["WhiteLed"];
    LightState { on: w["state"].as_i64().map(|s| s != 0), brightness: w["bright"].as_u64().map(|b| b as u32), mode: w["mode"].as_i64() }
}

/// Whether a `GetDevInfo` answer is a recorder's (NVR), not a Home Hub's or a
/// camera's. (UNVERIFIED on a real recorder: from the vendor guide's `type`.)
pub fn is_recorder(r: &Value) -> bool {
    let d = &r["value"]["DevInfo"];
    let ty = d["type"].as_str().unwrap_or("").to_ascii_lowercase();
    let model = d["model"].as_str().unwrap_or("").to_ascii_lowercase();
    ty == "nvr" && !model.contains("hub")
}

/// What a `GetDevInfo` answer says the device is. (UNVERIFIED beyond the
/// vendor guide's `type` and model names.)
pub fn device_kind(r: &Value) -> super::DevKind {
    let d = &r["value"]["DevInfo"];
    let ty = d["type"].as_str().unwrap_or("").to_ascii_lowercase();
    let model = d["model"].as_str().unwrap_or("").to_ascii_lowercase();
    let recorder = is_recorder(r);
    super::DevKind { recorder, multi: recorder || ty.contains("hub") || ty.contains("nvr") || model.contains("hub") || model.contains("nvr") }
}

/// A doorbell's light settings from a `GetPowerLed` (action 1) answer: only
/// what the range lists (a doorbell behind a hub lists none), never "NotSupport".
pub fn doorbell_states(r: &Value) -> Vec<String> {
    if r["value"]["PowerLed"]["eDoorbellLightState"].as_str().is_none_or(|s| s == "NotSupport") {
        return Vec::new();
    }
    listed(r, "PowerLed", "eDoorbellLightState").map(|l| l.iter().filter_map(|s| s.as_str()).filter(|s| *s != "NotSupport").map(str::to_string).collect()).unwrap_or_default()
}

/// How many rounds to ask the siren for, for `secs` seconds.
pub fn siren_rounds(secs: u32) -> u32 {
    secs.div_ceil(SIREN_ROUND_SECS).clamp(1, SIREN_MAX_ROUNDS)
}

/// How long a page's state read waits for the device to be free.
const IDLE_WAIT: std::time::Duration = std::time::Duration::from_millis(300);

impl ReolinkDriver {
    /// Every part's state, in one turn on the device's session.
    fn read_on(&self, c: &mut crate::reolink::ReolinkClient, ab: &Abilities) -> Result<Reading> {
        let ch = json!({ "channel": self.channel });
        let mut r = Reading::default();
        if ab.light.is_some() {
            r.light = Some(light_state(&c.command("GetWhiteLed", 0, ch.clone())?));
        }
        if !ab.ir.is_empty() {
            r.ir = c.command("GetIrLights", 0, ch.clone())?["value"]["IrLights"]["state"].as_str().map(str::to_string);
        }
        if !ab.status_led.is_empty() || !ab.doorbell_led.is_empty() {
            let v = c.command("GetPowerLed", 0, ch)?;
            r.status_led = v["value"]["PowerLed"]["state"].as_str().map(str::to_string);
            r.doorbell_led = v["value"]["PowerLed"]["eDoorbellLightState"].as_str().filter(|s| *s != "NotSupport").map(str::to_string);
        }
        Ok(r)
    }

    /// The driver for the device at `base` (its shared session).
    pub fn at(base: &str, user: &str, pass: &str, channel: u32) -> ReolinkDriver {
        ReolinkDriver { shared: session::get(base, user, pass), channel, user: user.to_string(), base: base.to_string(), first: std::sync::Mutex::new(None) }
    }

    fn cmd(&self, cmd: &str, action: u32, param: Value) -> Result<Value> {
        self.shared.with(|c| c.command(cmd, action, param))
    }

    fn ability(&self) -> Result<Value> {
        self.cmd("GetAbility", 0, json!({ "User": { "userName": self.user } }))
    }

    /// A "Get" that the camera may not have: None when it says so.
    fn optional_get(&self, cmd: &str) -> Result<Option<Value>> {
        match self.cmd(cmd, 1, json!({ "channel": self.channel })) {
            Ok(v) => Ok(Some(v)),
            Err(e) if cam_error(&e).is_some_and(|c| c.unsupported()) => Ok(None),
            Err(e) => Err(e),
        }
    }

    /// What the camera has, from its ability answer and the "Get" of each part.
    pub fn probe_with(&self, ability: &Value) -> Result<Abilities> {
        let c = claimed(ability, self.channel);
        let mut ab = Abilities { siren: c.siren, ..Default::default() };
        if c.light {
            if let Some(r) = self.optional_get("GetWhiteLed")? {
                let mut caps = light_caps(&r, c.brightness);
                caps.smart = c.smart;
                caps.keep_on = c.keep_on;
                caps.schedule = c.schedule;
                if caps.on_off {
                    ab.light = Some(caps);
                }
            }
        }
        if c.ir {
            if let Some(r) = self.optional_get("GetIrLights")? {
                ab.ir = listed(&r, "IrLights", "state")
                    .map(|l| l.iter().filter_map(|s| s.as_str().map(str::to_string)).collect())
                    .unwrap_or_else(|| vec!["Auto".into(), "Off".into()]);
            }
        }
        if c.status_led {
            if let Some(r) = self.optional_get("GetPowerLed")? {
                ab.status_led = listed(&r, "PowerLed", "state")
                    .map(|l| l.iter().filter_map(|s| s.as_str().map(str::to_string)).collect())
                    .unwrap_or_else(|| vec!["On".into(), "Off".into()]);
                ab.doorbell_led = doorbell_states(&r);
            }
        }
        Ok(ab)
    }
}

impl Driver for ReolinkDriver {
    fn base(&self) -> String {
        self.base.clone()
    }

    fn probe(&self) -> Result<Abilities> {
        let first = self.first.lock().unwrap_or_else(|e| e.into_inner()).take();
        let a = match first {
            Some(a) => a,
            None => self.ability()?,
        };
        self.probe_with(&a)
    }

    fn read(&self, ab: &Abilities) -> Result<Reading> {
        self.shared.with(|c| self.read_on(c, ab))
    }

    fn read_if_idle(&self, ab: &Abilities) -> Option<Result<Reading>> {
        self.shared.try_with(IDLE_WAIT, |c| self.read_on(c, ab))
    }

    fn set_light(&self, w: &LightState) -> Result<()> {
        let mut led = json!({ "channel": self.channel });
        if let Some(on) = w.on {
            led["state"] = json!(i64::from(on));
        }
        if let Some(b) = w.brightness {
            led["bright"] = json!(b);
        }
        if let Some(m) = w.mode {
            led["mode"] = json!(m);
        }
        if led.as_object().is_some_and(|o| o.len() == 1) {
            bail!("nothing to change");
        }
        self.cmd("SetWhiteLed", 0, json!({ "WhiteLed": led })).map(|_| ()).map_err(|e| plain(e, "change its spotlight"))
    }

    fn siren_play(&self, secs: u32) -> Result<()> {
        let p = json!({ "channel": self.channel, "alarm_mode": "times", "times": siren_rounds(secs), "manual_switch": 0 });
        self.cmd("AudioAlarmPlay", 0, p).map(|_| ()).map_err(|e| plain(e, "sound its siren"))
    }

    fn siren_stop(&self) -> Result<()> {
        // Reolink's guide spells the manual mode "manu". Some cameras (a Duo
        // Floodlight PoE) refuse that with -4 "param error" while a timed siren
        // plays, and stop it on "manul" (the spelling their own software uses)
        // or on 0 rounds: those are tried after a -4 (tested 2026-11-19 on
        // that camera only; 0 rounds is reached only after two -4 answers).
        let mut first = None;
        for form in SIREN_STOPS {
            let mut p = form();
            p["channel"] = json!(self.channel);
            match self.cmd("AudioAlarmPlay", 0, p) {
                Ok(_) => return Ok(()),
                Err(e) if cam_error(&e).is_some_and(|c| c.code == -4) => {
                    first.get_or_insert(e);
                }
                // Another failure (unreachable, busy) is the one to tell.
                Err(e) => return Err(plain(e, "stop its siren")),
            }
        }
        Err(plain(first.unwrap_or_else(|| anyhow!("no siren stop to try")), "stop its siren"))
    }

    fn set_ir(&self, state: &str) -> Result<()> {
        self.cmd("SetIrLights", 0, json!({ "IrLights": { "channel": self.channel, "state": state } })).map(|_| ()).map_err(|e| plain(e, "change its infrared lights"))
    }

    fn set_status_led(&self, state: &str) -> Result<()> {
        self.cmd("SetPowerLed", 0, json!({ "PowerLed": { "channel": self.channel, "state": state } })).map(|_| ()).map_err(|e| plain(e, "change its status light"))
    }

    fn device_kind(&self) -> Result<super::DevKind> {
        Ok(device_kind(&self.cmd("GetDevInfo", 0, json!({}))?))
    }

    fn set_doorbell_led(&self, state: &str) -> Result<()> {
        self.cmd("SetPowerLed", 0, json!({ "PowerLed": { "channel": self.channel, "eDoorbellLightState": state } })).map(|_| ()).map_err(|e| plain(e, "change its doorbell light"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn abilities_per_channel() {
        // Shapes as real cameras answer: floodLight 0 even on a floodlight.
        let a = json!({ "code": 0, "value": { "Ability": {
            "alarmAudio": { "permit": 6, "ver": 1 },
            "abilityChn": [
                { "floodLight": { "permit": 0, "ver": 0 }, "supportFLswitch": { "permit": 6, "ver": 1 }, "supportFLBrightness": { "permit": 6, "ver": 1 },
                  "supportFLIntelligent": { "permit": 6, "ver": 1 }, "supportFLKeepOn": { "permit": 6, "ver": 1 }, "alarmAudio": { "permit": 6, "ver": 1 },
                  "ledControl": { "permit": 6, "ver": 1 }, "powerLed": { "permit": 0, "ver": 0 } },
                // A doorbell behind a hub: siren listed with no permission, no light flags.
                { "floodLight": { "permit": 0, "ver": 0 }, "alarmAudio": { "permit": 0, "ver": 1 } }
            ] } } });
        let c0 = claimed(&a, 0);
        assert!(c0.light && c0.brightness && c0.smart && c0.keep_on && !c0.schedule && c0.siren && c0.ir && c0.status_led, "{c0:?}");
        let c1 = claimed(&a, 1);
        assert!(!c1.light && !c1.siren, "{c1:?}");
        assert!(!claimed(&a, 5).light, "a channel it does not list has no light");
        // A spotlight with a switch only (no brightness flag).
        let a = json!({ "value": { "Ability": { "abilityChn": [ { "supportFLswitch": { "permit": 6, "ver": 1 }, "supportFLIntelligent": { "permit": 6, "ver": 1 } } ] } } });
        let c = claimed(&a, 0);
        assert!(c.light && !c.brightness && !c.siren);
        // A single camera with its siren only listed at device level.
        let a = json!({ "value": { "Ability": { "alarmAudio": { "permit": 6, "ver": 1 }, "abilityChn": [ {} ] } } });
        assert!(claimed(&a, 0).siren);
    }

    #[test]
    fn spotlight_ranges_come_from_the_camera() {
        let r = json!({ "code": 0, "value": { "WhiteLed": { "channel": 0, "state": 0, "mode": 1, "bright": 79 } },
            "range": { "WhiteLed": { "bright": { "min": 1, "max": 100 } } } });
        let c = light_caps(&r, true);
        assert_eq!((c.on_off, c.brightness), (true, Some((1, 100))));
        assert!(c.modes.is_empty(), "modes are never guessed");
        assert_eq!(light_caps(&r, false).brightness, None, "the range is there either way: the flag decides");
        assert_eq!(light_state(&r), LightState { on: Some(false), brightness: Some(79), mode: Some(1) });
        let r = json!({ "code": 0, "value": { "WhiteLed": { "state": 1 } } });
        assert_eq!(light_caps(&r, true).brightness, Some((0, 100)));
        assert!(!light_caps(&json!({ "value": {} }), true).on_off, "no state: no switch");
    }

    #[test]
    fn a_recorder_only_by_its_own_word() {
        let d = |ty: &str, model: &str| json!({ "value": { "DevInfo": { "type": ty, "model": model } } });
        assert!(is_recorder(&d("NVR", "RLN8-410")));
        assert!(!is_recorder(&d("NVR", "Reolink Home Hub")), "a hub that calls itself NVR");
        assert!(!is_recorder(&d("HOMEHUB", "Reolink Home Hub")));
        assert!(!is_recorder(&d("IPC", "RLC-811A")));
        assert!(!is_recorder(&json!({})));
        assert!(device_kind(&d("HOMEHUB", "Reolink Home Hub")).multi);
        assert!(device_kind(&d("NVR", "Reolink Home Hub")).multi);
        assert!(!device_kind(&d("IPC", "RLC-811A")).multi);
        assert!(!device_kind(&d("WIFI_IPC", "E1 Outdoor Pro")).multi);
    }

    #[test]
    fn a_doorbells_light_only_as_listed() {
        let direct = json!({ "value": { "PowerLed": { "state": "On", "eDoorbellLightState": "On" } },
            "range": { "PowerLed": { "state": ["On", "Off"], "eDoorbellLightState": ["On", "Off", "KeepOff", "KeepOn", "NotSupport"] } } });
        assert_eq!(doorbell_states(&direct), ["On", "Off", "KeepOff", "KeepOn"]);
        let hub = json!({ "value": { "PowerLed": { "state": "On", "eDoorbellLightState": "On" } }, "range": { "PowerLed": { "eDoorbellLightState": [] } } });
        assert!(doorbell_states(&hub).is_empty());
        let none = json!({ "value": { "PowerLed": { "state": "On", "eDoorbellLightState": "NotSupport" } }, "range": { "PowerLed": { "eDoorbellLightState": ["On", "Off"] } } });
        assert!(doorbell_states(&none).is_empty());
        assert!(doorbell_states(&json!({ "value": { "PowerLed": { "state": "On" } } })).is_empty());
    }

    #[test]
    fn the_siren_always_stops_by_itself() {
        assert_eq!(siren_rounds(0), 1);
        assert_eq!(siren_rounds(5), 1);
        assert_eq!(siren_rounds(6), 2);
        assert_eq!(siren_rounds(u32::MAX), SIREN_MAX_ROUNDS);
    }
}
