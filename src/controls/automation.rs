//! Camera actions next to alerts: when the AI confirms a person (or another
//! chosen object), turn the camera's spotlight on for a few minutes, or sound
//! its siren while armed. Off by default, per camera (`[cameras.controls]`).
//!
//! Gates: only what the AI itself saw in this look (never raw motion, never
//! the camera's own report alone); the spotlight only at night unless told
//! otherwise; the siren only while armed, at most once per cooldown and a
//! few times an hour. The light stays on while the object is still seen
//! (each sighting pushes its end out) and then gets its own setting back.

use std::sync::Arc;
use std::time::{Duration, Instant};

use chrono::{Datelike, Timelike};

use super::{Action, Caller};
use crate::config::{CameraConfig, ControlsConfig};
use crate::engine::Engine;

/// Longest light and siren an action may ask for.
pub const MAX_LIGHT_MINUTES: u32 = 30;
pub const DEFAULT_LIGHT_MINUTES: u32 = 2;
pub const DEFAULT_SIREN_COOLDOWN_MINUTES: u32 = 10;
/// The light is asked again at most this often while the object is still seen.
const LIGHT_REFRESH: Duration = Duration::from_secs(20);
/// Sirens started by actions on one camera: at most this many an hour.
const SIREN_PER_HOUR: usize = 3;

const SCOPE: &str = "controls";

/// When the actions last sounded this camera's siren (epoch ms), as saved.
fn load_sirens(engine: &Engine, name: &str) -> Vec<i64> {
    engine.db.ui_get(SCOPE, &format!("auto_siren|{name}")).ok().flatten().and_then(|v| serde_json::from_str(&v).ok()).unwrap_or_default()
}

/// What the actions react to.
fn labels(c: &ControlsConfig) -> Vec<String> {
    if c.labels.is_empty() {
        vec!["person".into()]
    } else {
        c.labels.iter().map(|l| l.trim().to_ascii_lowercase()).collect()
    }
}

/// Whether it is night now: between sunset and sunrise at the location set
/// for schedules, else between 19:00 and 07:00 local time.
pub fn is_night(engine: &Engine) -> bool {
    let loc = engine.config.lock().unwrap().automation.location();
    let now = chrono::Local::now();
    let minute = now.hour() * 60 + now.minute();
    night_at(minute, loc.and_then(|(lat, lon)| crate::config::sun_times(lat, lon, now.ordinal(), now.offset().local_minus_utc() / 60)))
}

/// Night at `minute` of the day, given (sunrise, sunset) minutes when known.
pub fn night_at(minute: u32, sun: Option<(u32, u32)>) -> bool {
    let (rise, set) = sun.unwrap_or((7 * 60, 19 * 60));
    minute < rise || minute >= set
}

/// What the actions would do for a sighting (decided, not done).
#[derive(Debug, Default, PartialEq)]
pub struct Plan {
    pub light_secs: Option<u32>,
    pub siren_secs: Option<u32>,
}

/// Decide, from the settings and the moment (no camera asked). `last_siren`:
/// when this camera's actions last sounded its siren, and how many in the
/// last hour.
pub fn plan(c: &ControlsConfig, label: &str, armed: bool, night: bool, light_recent: bool, last_siren: Option<Instant>, sirens_last_hour: usize) -> Plan {
    let mut p = Plan::default();
    if !labels(c).iter().any(|l| l == &label.to_ascii_lowercase()) {
        return p;
    }
    if c.light_on_detect && !light_recent && (night || c.light_night_only == Some(false)) {
        p.light_secs = Some(c.light_minutes.unwrap_or(DEFAULT_LIGHT_MINUTES).clamp(1, MAX_LIGHT_MINUTES) * 60);
    }
    let cooldown = Duration::from_secs(u64::from(c.siren_cooldown_minutes.unwrap_or(DEFAULT_SIREN_COOLDOWN_MINUTES).max(1)) * 60);
    if c.siren_on_detect
        && armed
        && (night || c.siren_night_only != Some(true))
        && last_siren.is_none_or(|t| t.elapsed() >= cooldown)
        && sirens_last_hour < SIREN_PER_HOUR
    {
        p.siren_secs = Some(c.siren_seconds.unwrap_or(super::SIREN_DEFAULT_SECS).clamp(1, super::SIREN_MAX_SECS));
    }
    p
}

/// The AI confirmed `label` in a look at camera `id` (called from the
/// camera's worker: returns at once, the camera is asked on other threads).
pub fn on_confirmed(engine: &Arc<Engine>, id: usize, cfg: &CameraConfig, label: &str, armed: bool) {
    // (The settings as they are now: a change to them does not restart the camera.)
    let Some(h) = engine.camera(id) else { return };
    let Some(c) = h.cfg.read().unwrap().controls.clone().filter(|c| c.light_on_detect || c.siren_on_detect) else { return };
    let c = &c;
    let name = cfg.name.clone();
    let night = is_night(engine);
    let p = {
        let mut a = engine.controls.auto.lock().unwrap();
        let st = a.entry(name.clone()).or_insert_with(|| super::AutoState { light_at: None, sirens_ms: load_sirens(engine, &name) });
        let now = chrono::Utc::now().timestamp_millis();
        // The last hour's (one "from the future" after a clock change counts as now).
        st.sirens_ms = st.sirens_ms.iter().map(|t| (*t).min(now)).filter(|t| now - t < 3_600_000).collect();
        let last = st.sirens_ms.iter().max().and_then(|t| Instant::now().checked_sub(Duration::from_millis((now - t) as u64)));
        let light_recent = st.light_at.is_some_and(|t| t.elapsed() < LIGHT_REFRESH);
        let p = plan(c, label, armed, night, light_recent, last, st.sirens_ms.len());
        if p.light_secs.is_some() {
            st.light_at = Some(Instant::now());
        }
        if p.siren_secs.is_some() {
            st.sirens_ms.push(now);
            let _ = engine.db.ui_put(SCOPE, &format!("auto_siren|{name}"), &serde_json::to_string(&st.sirens_ms).unwrap_or_default());
        }
        p
    };
    let who = Caller { user: format!("(action on {label})"), ip: String::new(), source: "automation" };
    if let Some(secs) = p.light_secs {
        let (e, w, n) = (engine.clone(), who.clone(), name.clone());
        std::thread::spawn(move || {
            if let Err(r) = super::act(&e, id, &w, Action::LightOn { seconds: Some(secs), brightness: None }, true) {
                tracing::warn!("camera '{n}': the spotlight action did not work: {}", r.msg);
            }
        });
    }
    if let Some(secs) = p.siren_secs {
        let e = engine.clone();
        std::thread::spawn(move || {
            if let Err(r) = super::act(&e, id, &who, Action::Siren { seconds: Some(secs) }, true) {
                tracing::warn!("camera '{name}': the siren action did not work: {}", r.msg);
            }
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg(f: impl FnOnce(&mut ControlsConfig)) -> ControlsConfig {
        let mut c = ControlsConfig::default();
        f(&mut c);
        c
    }

    #[test]
    fn nothing_unless_turned_on() {
        assert_eq!(plan(&ControlsConfig::default(), "person", true, true, false, None, 0), Plan::default());
    }

    #[test]
    fn light_at_night_for_the_chosen_objects() {
        let c = cfg(|c| {
            c.light_on_detect = true;
            c.light_minutes = Some(3);
        });
        assert_eq!(plan(&c, "person", false, true, false, None, 0).light_secs, Some(180));
        assert_eq!(plan(&c, "Person", false, true, false, None, 0).light_secs, Some(180));
        assert_eq!(plan(&c, "person", false, false, false, None, 0).light_secs, None, "day");
        assert_eq!(plan(&c, "car", false, true, false, None, 0).light_secs, None, "not chosen");
        assert_eq!(plan(&c, "person", false, true, true, None, 0).light_secs, None, "asked a moment ago");
        let any = cfg(|c| {
            c.light_on_detect = true;
            c.light_night_only = Some(false);
            c.light_minutes = Some(999);
            c.labels = vec!["car".into()];
        });
        assert_eq!(plan(&any, "car", false, false, false, None, 0).light_secs, Some(MAX_LIGHT_MINUTES * 60));
        assert_eq!(plan(&any, "person", false, false, false, None, 0).light_secs, None);
    }

    #[test]
    fn siren_only_armed_with_a_cooldown_and_an_hourly_cap() {
        let c = cfg(|c| {
            c.siren_on_detect = true;
            c.siren_seconds = Some(500);
        });
        assert_eq!(plan(&c, "person", true, false, false, None, 0).siren_secs, Some(crate::controls::SIREN_MAX_SECS));
        assert_eq!(plan(&c, "person", false, true, false, None, 0).siren_secs, None, "disarmed");
        assert_eq!(plan(&c, "person", true, true, false, Some(Instant::now()), 1).siren_secs, None, "cooldown");
        assert_eq!(plan(&c, "person", true, true, false, None, SIREN_PER_HOUR).siren_secs, None, "hourly cap");
        let night = cfg(|c| {
            c.siren_on_detect = true;
            c.siren_night_only = Some(true);
        });
        assert_eq!(plan(&night, "person", true, false, false, None, 0).siren_secs, None);
        assert_eq!(plan(&night, "person", true, true, false, None, 0).siren_secs, Some(crate::controls::SIREN_DEFAULT_SECS));
    }

    #[test]
    fn night_by_the_sun_or_the_clock() {
        assert!(night_at(23 * 60, None) && night_at(6 * 60, None) && !night_at(12 * 60, None));
        let sun = Some((5 * 60 + 30, 20 * 60 + 30));
        assert!(!night_at(20 * 60, sun) && night_at(20 * 60 + 30, sun) && night_at(5 * 60, sun));
    }
}
