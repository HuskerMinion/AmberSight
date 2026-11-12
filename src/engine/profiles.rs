//! Profiles and the weekly schedule.
//!
//! A profile is a named set of overrides (arm/disarm, alerts, and per-camera
//! motion / alerts / AI / recording mode). The active profile is, in order:
//! a manual choice (optionally for N minutes), the first matching weekly
//! schedule rule, or the default profile. Overrides apply on top of each
//! camera's own settings and are never written back to config.toml.

use super::*;
use crate::config::{parse_time_spec, resolve_time, CameraOverride, ProfileConfig, ScheduleRule};
use chrono::Datelike;

#[derive(Clone, Debug, Default, serde::Serialize)]
pub struct ActiveProfile {
    /// Empty = no profile.
    pub name: String,
    /// "manual", "schedule" or "default".
    pub source: &'static str,
    /// Manual override end (epoch seconds), if timed.
    pub until: Option<i64>,
}

const DAYS: [&str; 7] = ["mon", "tue", "wed", "thu", "fri", "sat", "sun"];

/// Does `rule` cover `weekday` (0 = Monday) at `minute` of the day? `sun` =
/// today's (sunrise, sunset) minutes, for rules that use them.
pub(super) fn rule_matches(rule: &ScheduleRule, weekday: usize, minute: u32, sun: Option<(u32, u32)>) -> bool {
    let at = |s: &str| parse_time_spec(s).and_then(|t| resolve_time(t, sun));
    let (Some(from), Some(to)) = (at(&rule.from), at(&rule.to)) else { return false };
    let on = |d: usize| {
        rule.days.is_empty() || rule.days.iter().any(|x| x.trim().to_ascii_lowercase().starts_with(DAYS[d % 7]))
    };
    if from <= to {
        on(weekday) && minute >= from && minute < to
    } else {
        // Past midnight: the late part belongs to the start day, the early
        // part to the day after.
        (on(weekday) && minute >= from) || (on((weekday + 6) % 7) && minute < to)
    }
}

/// The profile the schedule picks now, or the default.
pub(super) fn scheduled_profile(
    auto: &crate::config::AutomationConfig,
    weekday: usize,
    minute: u32,
    sun: Option<(u32, u32)>,
) -> (String, &'static str) {
    match auto.schedule.iter().find(|r| rule_matches(r, weekday, minute, sun)) {
        Some(r) => (r.profile.clone(), "schedule"),
        None => (auto.default_profile.clone(), "default"),
    }
}

/// Today's (sunrise, sunset) at the configured location, local minutes.
pub(super) fn sun_today(auto: &crate::config::AutomationConfig) -> Option<(u32, u32)> {
    let (lat, lon) = auto.location()?;
    let now = chrono::Local::now();
    crate::config::sun_times(lat, lon, now.ordinal(), now.offset().local_minus_utc() / 60)
}

impl Engine {
    pub fn active_profile(&self) -> ActiveProfile {
        self.profile.read().unwrap().clone()
    }

    fn profile_config(&self, name: &str) -> Option<ProfileConfig> {
        if name.is_empty() {
            return None;
        }
        self.config.lock().unwrap().automation.profiles.iter().find(|p| p.name == name).cloned()
    }

    /// The active profile's override for one camera (empty when none).
    pub fn camera_override(&self, camera_name: &str) -> CameraOverride {
        let name = self.profile.read().unwrap().name.clone();
        self.profile_config(&name).and_then(|p| p.cameras.get(camera_name).cloned()).unwrap_or_default()
    }

    /// Motion triggers enabled for this camera under the active profile.
    pub fn motion_enabled(&self, cfg: &CameraConfig) -> bool {
        self.camera_override(&cfg.name).motion.unwrap_or(cfg.motion)
    }

    /// Alerts enabled for this camera under the active profile.
    pub fn alerts_enabled(&self, cfg: &CameraConfig) -> bool {
        let name = self.profile.read().unwrap().name.clone();
        let p = self.profile_config(&name);
        let global = p.as_ref().and_then(|p| p.alerts).unwrap_or(true);
        let cam = p.and_then(|p| p.cameras.get(&cfg.name).and_then(|c| c.alerts)).unwrap_or(true);
        global && cam
    }

    /// AI checks enabled for this camera under the active profile.
    pub fn ai_enabled(&self, cfg: &CameraConfig) -> bool {
        self.camera_override(&cfg.name).ai.unwrap_or(cfg.ai.unwrap_or(true))
    }

    /// The camera configuration a worker should run with: the camera's own
    /// settings plus the active profile's recording overrides.
    pub fn effective_camera(&self, base: &CameraConfig) -> CameraConfig {
        let o = self.camera_override(&base.name);
        let mut c = base.clone();
        if let Some(r) = o.record {
            c.record = r;
        }
        if let Some(m) = o.record_motion_only {
            c.record_motion_only = m;
        }
        c
    }

    /// The profile the weekly schedule picks right now (ignores manual holds).
    pub fn scheduled_now(&self) -> String {
        let now = chrono::Local::now();
        let weekday = now.weekday().num_days_from_monday() as usize;
        let minute = now.hour() * 60 + now.minute();
        let auto = self.config.lock().unwrap().automation.clone();
        scheduled_profile(&auto, weekday, minute, sun_today(&auto)).0
    }

    /// The state `GET /api/profiles` reports.
    pub fn profiles_json(&self) -> serde_json::Value {
        let auto = self.config.lock().unwrap().automation.clone();
        let manual = self.manual_profile.lock().unwrap().clone();
        profiles_json(&auto, &self.active_profile().name, &self.scheduled_now(), manual, Utc::now().timestamp())
    }

    /// Switch profile by hand. `minutes` = None holds it until changed;
    /// `name` = None returns to the schedule. `by` is who or what chose it
    /// (a user name, "(mqtt)", "(presence)"), `why` an optional note; a
    /// change goes to the log and the audit log (choosing what is already
    /// chosen records nothing).
    pub fn set_manual_profile(self: &Arc<Engine>, name: Option<String>, minutes: Option<i64>, by: &str, why: &str) -> Result<(), String> {
        if let Some(n) = &name {
            if !n.is_empty() && self.profile_config(n).is_none() {
                return Err(format!("unknown profile '{n}'"));
            }
        }
        let hold = name.map(|n| (n, minutes.map(|m| Utc::now().timestamp() + m * 60)));
        // Held while the choice is stored and applied, so the schedule's
        // loop cannot apply it first (and record it as the schedule's).
        let _apply = APPLYING.lock().unwrap();
        let prev = std::mem::replace(&mut *self.manual_profile.lock().unwrap(), hold.clone());
        if prev != hold {
            // Kept across restarts: Home Assistant switches a profile once
            // (when the house empties) and does not say it again after a
            // restart.
            let saved = serde_json::to_string(&hold).unwrap_or_default();
            if let Err(e) = self.db.ui_put(MANUAL_SCOPE, MANUAL_KEY, &saved) {
                tracing::warn!("could not save the profile choice: {e:#}");
            }
            self.record_change(by, &manual_change_text(&hold), why);
        }
        self.apply_profile_inner(false, Some(by));
        Ok(())
    }

    /// Work out which profile should be active and apply it if it changed.
    pub fn apply_profile(self: &Arc<Engine>) {
        let _apply = APPLYING.lock().unwrap();
        self.apply_profile_inner(false, None);
    }

    /// After the profiles themselves were edited: the active one may have
    /// changed what it does, so check every camera even if the name did not
    /// change. (The shield is left as the user set it.)
    pub fn reapply_profile(self: &Arc<Engine>) {
        let _apply = APPLYING.lock().unwrap();
        self.apply_profile_inner(true, None);
    }

    /// `by`: who chose a profile by hand just now (None: the schedule, or a
    /// timed choice running out).
    fn apply_profile_inner(self: &Arc<Engine>, force: bool, by: Option<&str>) {
        let now = chrono::Local::now();
        let weekday = now.weekday().num_days_from_monday() as usize;
        let minute = now.hour() * 60 + now.minute();
        let manual = {
            let mut m = self.manual_profile.lock().unwrap();
            if m.as_ref().is_some_and(|(_, until)| until.is_some_and(|u| Utc::now().timestamp() >= u)) {
                *m = None; // timed override over
            }
            m.clone()
        };
        let next = match manual {
            Some((name, until)) => ActiveProfile { name, source: "manual", until },
            None => {
                let auto = self.config.lock().unwrap().automation.clone();
                let (name, source) = scheduled_profile(&auto, weekday, minute, sun_today(&auto));
                ActiveProfile { name, source, until: None }
            }
        };
        let (changed, first) = {
            let mut cur = self.profile.write().unwrap();
            let changed = cur.name != next.name;
            // (Nothing applied yet: the server is starting.)
            let first = cur.source.is_empty();
            *cur = next.clone();
            (changed, first)
        };
        if !changed && !force {
            return;
        }
        if changed {
            let shown = if next.name.is_empty() { "none" } else { &next.name };
            let by = match by {
                Some(b) => b.to_string(),
                None if first => "(startup)".to_string(),
                None => "(schedule)".to_string(),
            };
            if by == "(schedule)" {
                // (A choice by hand was recorded when it was made.)
                let why = if next.source == "default" { "default profile" } else { "schedule rule" };
                self.record_change(&by, &format!("profile: {shown}"), why);
            } else {
                info!("profile: {shown} ({})", next.source);
            }
            if let Some(armed) = self.profile_config(&next.name).and_then(|p| p.armed) {
                self.set_shield(armed, &by, &format!("profile {shown}"));
            }
        }
        // Restart cameras whose recording mode changes under the new profile.
        let cams: Vec<Arc<CameraHandle>> = self.cameras.read().unwrap().iter().flatten().cloned().collect();
        for h in cams {
            let base = h.cfg.read().unwrap().clone();
            let eff = self.effective_camera(&base);
            let running = h.running.read().unwrap().clone();
            if (eff.record, eff.record_motion_only) != (running.record, running.record_motion_only) {
                info!("profile: camera '{}' recording mode changes; restarting it", base.name);
                self.restart_camera(h.id);
            }
        }
    }
}

/// Shape of `GET /api/profiles`: names, what is active, what the schedule
/// says, and the manual hold (if any and not yet over).
pub(super) fn profiles_json(
    auto: &crate::config::AutomationConfig,
    active: &str,
    scheduled: &str,
    manual: Option<(String, Option<i64>)>,
    now: i64,
) -> serde_json::Value {
    let manual = manual
        .filter(|(_, until)| until.is_none_or(|u| u > now))
        .map(|(name, until)| serde_json::json!({ "name": name, "until": until }));
    serde_json::json!({
        "profiles": auto.profiles.iter().map(|p| p.name.clone()).collect::<Vec<_>>(),
        "active": active,
        "scheduled": scheduled,
        "manual": manual,
        "default_profile": auto.default_profile,
        "schedule": auto.schedule,
        // Today's sun times (HH:MM) for "sunrise" / "sunset" rules, if a location is set.
        "sun": sun_today(auto).map(|(r, s)| serde_json::json!({
            "sunrise": format!("{:02}:{:02}", r / 60, r % 60),
            "sunset": format!("{:02}:{:02}", s / 60, s % 60),
        })),
    })
}

const MANUAL_SCOPE: &str = "server";

/// Taken while a profile is worked out and applied (and, choosing one by
/// hand, while the choice is stored): before manual_profile, the config,
/// ARMING and the database.
static APPLYING: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// The audit log text for a profile chosen by hand.
fn manual_change_text(hold: &Option<(String, Option<i64>)>) -> String {
    let shown = |n: &str| if n.is_empty() { "none".to_string() } else { n.to_string() };
    match hold {
        Some((n, Some(until))) => format!(
            "profile: {} chosen until {}",
            shown(n),
            chrono::DateTime::from_timestamp(*until, 0).map(|t| t.with_timezone(&chrono::Local).format("%Y-%m-%d %H:%M").to_string()).unwrap_or_default()
        ),
        Some((n, None)) => format!("profile: {} chosen", shown(n)),
        None => "profile: back to the schedule".to_string(),
    }
}
const MANUAL_KEY: &str = "manual_profile";

/// The profile chosen by hand before the last restart, if it still exists
/// and its time is not over.
pub(super) fn saved_manual(db: &crate::db::Db, config: &crate::config::Config) -> Option<(String, Option<i64>)> {
    let raw = db.ui_get(MANUAL_SCOPE, MANUAL_KEY).ok().flatten()?;
    let (name, until): (String, Option<i64>) = serde_json::from_str::<Option<(String, Option<i64>)>>(&raw).ok().flatten()?;
    let exists = name.is_empty() || config.automation.profiles.iter().any(|p| p.name == name);
    let current = until.is_none_or(|u| Utc::now().timestamp() < u);
    (exists && current).then_some((name, until))
}

/// Re-evaluate the schedule every 20 s.
pub(super) fn profile_loop(engine: Arc<Engine>) {
    loop {
        engine.apply_profile();
        thread::sleep(std::time::Duration::from_secs(20));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::AutomationConfig;

    #[test]
    fn choosing_the_chosen_profile_again_records_nothing() {
        let cfg: crate::config::Config = toml::from_str("[automation]\n[[automation.profiles]]\nname = \"Away\"\n").unwrap();
        let e = Engine::for_test(cfg);
        e.set_manual_profile(Some("Away".into()), None, "(presence)", "everyone away").unwrap();
        assert_eq!(e.active_profile().name, "Away");
        let rows = e.db.audit_rows();
        assert_eq!(rows, [("(presence)".to_string(), "profile: Away chosen".to_string(), "everyone away".to_string())]);
        // The same again (MQTT, presence, the page): no new entry.
        e.set_manual_profile(Some("Away".into()), None, "(mqtt)", "").unwrap();
        e.set_manual_profile(Some("Away".into()), None, "ann", "web page or API").unwrap();
        assert_eq!(e.db.audit_rows().len(), 1);
        // A real change is recorded, and the schedule's loop applying
        // afterwards finds nothing to do (nor anything to credit itself).
        e.set_manual_profile(None, None, "ann", "").unwrap();
        e.apply_profile();
        let rows = e.db.audit_rows();
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[1].1, "profile: back to the schedule");
        e.set_manual_profile(None, None, "ann", "").unwrap();
        assert_eq!(e.db.audit_rows().len(), 2);
        assert!(e.set_manual_profile(Some("Nope".into()), None, "ann", "").is_err());
    }

    #[test]
    fn a_hand_picked_profile_survives_a_restart() {
        let db = crate::db::Db::open_in_memory().unwrap();
        let mut cfg: crate::config::Config = toml::from_str("[automation]\n[[automation.profiles]]\nname = \"Away\"\n").unwrap();
        assert_eq!(saved_manual(&db, &cfg), None, "nothing chosen yet");
        let save = |v: &Option<(String, Option<i64>)>| db.ui_put(MANUAL_SCOPE, MANUAL_KEY, &serde_json::to_string(v).unwrap()).unwrap();
        save(&Some(("Away".into(), None)));
        assert_eq!(saved_manual(&db, &cfg), Some(("Away".into(), None)));
        // Back to the schedule ("Auto").
        save(&None);
        assert_eq!(saved_manual(&db, &cfg), None);
        // A timed choice that ran out meanwhile, and one still running.
        let now = Utc::now().timestamp();
        save(&Some(("Away".into(), Some(now - 10))));
        assert_eq!(saved_manual(&db, &cfg), None);
        save(&Some(("Away".into(), Some(now + 600))));
        assert_eq!(saved_manual(&db, &cfg), Some(("Away".into(), Some(now + 600))));
        // A profile removed from the settings meanwhile.
        cfg.automation.profiles.clear();
        assert_eq!(saved_manual(&db, &cfg), None);
    }

    fn rule(days: &[&str], from: &str, to: &str, p: &str) -> ScheduleRule {
        ScheduleRule { days: days.iter().map(|s| s.to_string()).collect(), from: from.into(), to: to.into(), profile: p.into() }
    }

    #[test]
    fn weekday_windows_and_midnight_wrap() {
        let work = rule(&["mon", "tue", "wed", "thu", "fri"], "08:00", "17:30", "Away");
        assert!(rule_matches(&work, 0, 8 * 60, None));
        assert!(!rule_matches(&work, 0, 17 * 60 + 30, None)); // end exclusive
        assert!(!rule_matches(&work, 5, 9 * 60, None)); // Saturday
        let night = rule(&["fri"], "22:00", "06:00", "Night");
        assert!(rule_matches(&night, 4, 23 * 60, None)); // Friday late
        assert!(rule_matches(&night, 5, 5 * 60, None)); // Saturday early belongs to Friday's rule
        assert!(!rule_matches(&night, 4, 5 * 60, None)); // Friday early: Thursday's night, not listed
        let every = rule(&[], "00:00", "23:59", "All");
        assert!(rule_matches(&every, 6, 12 * 60, None));
        assert!(!rule_matches(&rule(&[], "bad", "07:00", "X"), 0, 0, None));
        // Sun-based: Night from sunset-30 to sunrise, with sunrise 06:40 and sunset 17:50.
        let night = rule(&[], "sunset-30", "sunrise", "Night");
        let sun = Some((400, 1070));
        assert!(rule_matches(&night, 2, 1045, sun)); // 17:25
        assert!(!rule_matches(&night, 2, 1030, sun)); // 17:10
        assert!(rule_matches(&night, 2, 390, sun)); // 06:30
        assert!(!rule_matches(&night, 2, 400, sun)); // 06:40
        assert!(!rule_matches(&night, 2, 1045, None)); // no location: never matches
    }

    #[test]
    fn first_matching_rule_wins_else_default() {
        let auto = AutomationConfig {
            default_profile: "Home".into(),
            profiles: vec![],
            schedule: vec![rule(&["sat"], "10:00", "12:00", "Party"), rule(&[], "09:00", "17:00", "Away")],
            ..Default::default()
        };
        assert_eq!(scheduled_profile(&auto, 5, 11 * 60, None), ("Party".to_string(), "schedule"));
        assert_eq!(scheduled_profile(&auto, 5, 13 * 60, None), ("Away".to_string(), "schedule"));
        assert_eq!(scheduled_profile(&auto, 5, 20 * 60, None), ("Home".to_string(), "default"));
    }

    #[test]
    fn profiles_json_shape() {
        let auto = AutomationConfig {
            default_profile: "Home".into(),
            profiles: vec![
                crate::config::ProfileConfig { name: "Home".into(), ..Default::default() },
                crate::config::ProfileConfig { name: "Away".into(), ..Default::default() },
            ],
            schedule: vec![],
            ..Default::default()
        };
        let v = profiles_json(&auto, "Away", "Home", Some(("Away".into(), Some(2000))), 1000);
        assert_eq!(v["profiles"], serde_json::json!(["Home", "Away"]));
        assert_eq!((v["active"].as_str(), v["scheduled"].as_str()), (Some("Away"), Some("Home")));
        assert_eq!(v["manual"], serde_json::json!({ "name": "Away", "until": 2000 }));
        // Held until changed.
        let v = profiles_json(&auto, "Away", "Home", Some(("Away".into(), None)), 1000);
        assert_eq!(v["manual"], serde_json::json!({ "name": "Away", "until": null }));
        // Expired or absent hold.
        assert!(profiles_json(&auto, "Home", "Home", Some(("Away".into(), Some(900))), 1000)["manual"].is_null());
        assert!(profiles_json(&auto, "Home", "Home", None, 1000)["manual"].is_null());
    }
}
