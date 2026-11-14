//! Presence ("geofencing"): phone apps report whether the phone is at home
//! (`POST /api/presence`). When everyone has been away for `[presence]
//! away_delay_minutes` the server arms the shield or switches to the away
//! profile; when someone comes back it switches to the home profile (and
//! disarms, if set).
//!
//! Only changes act: home -> away starts the away timer (someone coming back
//! first cancels it), away -> home acts at once. The rule remembers the last
//! known state (home or away) through spells when no phone counts (unknown)
//! and across restarts (stored in the database): unknown only means "do
//! nothing now" (a running away timer is called off), and the next known
//! state is compared with the last known one. Only the very first state
//! after presence is turned on is a starting point; turning it off forgets
//! the memory. So an arm or profile switch by hand stays until the next
//! change.

use super::*;
use crate::config::PresenceConfig;
use crate::db::{PresenceDevice, PresenceRuleRow};

/// Most phones one account can report presence from.
pub const MAX_PER_USER: usize = 20;
/// Most phones on one server.
pub const MAX_TOTAL: usize = 200;
/// The longest device id and name.
pub const MAX_LEN: usize = 64;
/// Phones silent this long are removed.
const PURGE_AFTER_MS: i64 = 30 * 86_400_000;

/// What the rule does now.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Act {
    /// Everyone has been away long enough.
    Away,
    /// Someone came home.
    Home,
}

/// The rule's memory.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Rule {
    /// The state now: Some(true) someone home, Some(false) everyone away,
    /// None unknown (no phone counts).
    now: Option<bool>,
    /// The last known state (kept through unknown spells); None until the
    /// first one after presence was turned on.
    known: Option<bool>,
    /// When everyone left (epoch ms), while the away action is pending. The
    /// due time is this plus the away delay at the time of asking, so a
    /// changed delay applies to a running countdown.
    left_at: Option<i64>,
    /// Presence did the away action and nobody has come back since.
    acted_away: bool,
}

impl Rule {
    /// The rule as stored (after a restart); the state now is not known yet.
    pub fn from_row(r: PresenceRuleRow) -> Rule {
        Rule { now: None, known: r.known_home, left_at: r.left_at_ms, acted_away: r.acted_away }
    }

    /// What to store (None: nothing to remember).
    pub fn to_row(&self) -> Option<PresenceRuleRow> {
        let r = PresenceRuleRow { known_home: self.known, left_at_ms: self.left_at, acted_away: self.acted_away };
        (r != PresenceRuleRow::default()).then_some(r)
    }

    /// Take in the state now; returns what to do, if anything.
    pub fn step(&mut self, home: Option<bool>, now_ms: i64, delay_ms: i64) -> Option<Act> {
        self.now = home;
        let Some(h) = home else {
            // Unknown: nothing acts now. The last known state stays; a
            // running away timer is called off (it must not fire on phones
            // that no longer count).
            self.left_at = None;
            return None;
        };
        let Some(prev) = self.known.replace(h) else {
            // The very first state: a starting point only.
            self.left_at = None;
            self.acted_away = false;
            return None;
        };
        match (prev, h) {
            (true, false) => {
                self.left_at = Some(now_ms);
                self.acted_away = false;
                self.due(now_ms, delay_ms)
            }
            (false, true) => {
                self.acted_away = false;
                // Back before the away action: it is called off, nothing else.
                self.left_at.take().is_none().then_some(Act::Home)
            }
            (false, false) => self.due(now_ms, delay_ms),
            (true, true) => None,
        }
    }

    /// The away action, if its time has come.
    fn due(&mut self, now_ms: i64, delay_ms: i64) -> Option<Act> {
        let at = self.left_at?;
        if now_ms < at + delay_ms.max(0) {
            return None;
        }
        self.left_at = None;
        self.acted_away = true;
        Some(Act::Away)
    }

    /// Presence is off: forget everything (the state now is still shown).
    pub fn idle(&mut self, home: Option<bool>) {
        *self = Rule { now: home, ..Default::default() };
    }

    pub fn home(&self) -> Option<bool> {
        self.now
    }

    /// When the away action is due with this delay (epoch ms).
    pub fn pending_at(&self, delay_ms: i64) -> Option<i64> {
        self.left_at.map(|l| l + delay_ms.max(0))
    }

    /// Whether presence did the away action (nobody back since).
    pub fn acted_away(&self) -> bool {
        self.acted_away
    }
}

/// Whether a device has reported recently enough to count.
pub fn fresh(d: &PresenceDevice, now_ms: i64, stale_hours: u32) -> bool {
    now_ms - d.updated_ms <= i64::from(stale_hours) * 3_600_000
}

/// Someone home (Some(true)), everyone away (Some(false)), or unknown (no
/// device has reported within `stale_hours`).
pub fn anyone_home(devs: &[PresenceDevice], now_ms: i64, stale_hours: u32) -> Option<bool> {
    let mut any = false;
    let mut home = false;
    for d in devs.iter().filter(|d| fresh(d, now_ms, stale_hours)) {
        any = true;
        home |= d.home;
    }
    any.then_some(home)
}

/// "home", "away" or "unknown" (MQTT and the API).
pub fn state_word(home: Option<bool>) -> &'static str {
    match home {
        Some(true) => "home",
        Some(false) => "away",
        None => "unknown",
    }
}

/// What leaving does with these settings: "armed", "profile:<name>",
/// "profile:<name>+armed" or "none" (also when presence is off).
pub fn away_rule(cfg: &PresenceConfig) -> String {
    if !cfg.enabled {
        return "none".into();
    }
    let arm = cfg.arms_when_away();
    match (cfg.away_profile.is_empty(), arm) {
        (true, true) => "armed".into(),
        (true, false) => "none".into(),
        (false, false) => format!("profile:{}", cfg.away_profile),
        (false, true) => format!("profile:{}+armed", cfg.away_profile),
    }
}

/// A device id as the app sent it, checked: 1-64 of A-Z a-z 0-9 _ -.
pub fn clean_device_id(id: &str) -> Result<String, String> {
    let id = id.trim();
    if id.is_empty() || id.len() > MAX_LEN || !id.chars().all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-') {
        return Err(format!("device_id must be 1 to {MAX_LEN} letters, digits, '-' or '_'"));
    }
    Ok(id.to_string())
}

/// The name shown for a device: trimmed, at most 64 characters, no control
/// characters; empty becomes "Phone".
pub fn clean_name(name: &str) -> Result<String, String> {
    let n = name.trim();
    if n.chars().count() > MAX_LEN {
        return Err(format!("name must be at most {MAX_LEN} characters"));
    }
    if n.chars().any(char::is_control) {
        return Err("name must not contain control characters".into());
    }
    Ok(if n.is_empty() { "Phone".into() } else { n.to_string() })
}


/// The names of the server's accounts (none: an open server).
pub fn account_names(cfg: &crate::config::Config) -> Vec<String> {
    // The demo account's phones never count: it stands in the list under a
    // name nobody has (so the list is not empty, which would count everyone).
    crate::auth::accounts(cfg).into_iter().map(|(u, p, _)| if p == crate::auth::DEMO_HASH { "(demo)".to_string() } else { u }).collect()
}

/// The last thing presence did.
#[derive(Debug, Clone, serde::Serialize)]
pub struct LastAction {
    pub at_ms: i64,
    /// "away" or "home".
    pub event: &'static str,
    /// What was done: "armed", "disarmed", "profile:<name>", "schedule",
    /// joined with "+", or "none".
    pub did: String,
    pub reason: String,
}

#[derive(Debug, Default)]
pub struct PresenceState {
    pub rule: Rule,
    pub last_action: Option<LastAction>,
    /// The stored memory was read (at the first evaluation).
    loaded: bool,
    /// What is stored now (only changes are written).
    saved: Option<PresenceRuleRow>,
    /// The away delay last used (ms), for when the action is due.
    delay_ms: i64,
}

/// One evaluation's outcome.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Decision {
    pub act: Option<Act>,
    /// Presence had done the away action before this (for coming home).
    pub after_away: bool,
}

impl PresenceState {
    /// Take in the devices now (`cfg` as the rule uses it, see
    /// PresenceConfig::for_rule) and work out what to do; the rule's memory
    /// is read from and written to `db`.
    pub fn decide(&mut self, cfg: &PresenceConfig, devs: &[PresenceDevice], server_accounts: &[String], now: i64, db: &crate::db::Db) -> Decision {
        if !self.loaded {
            self.loaded = true;
            match db.presence_rule_get() {
                Ok(Some(r)) => {
                    self.rule = Rule::from_row(r);
                    self.saved = Some(r);
                }
                Ok(None) => {}
                Err(e) => warn!("presence: could not read the stored state: {e:#}"),
            }
        }
        let counted: Vec<PresenceDevice> = devs.iter().filter(|d| cfg.counts(&d.user, server_accounts)).cloned().collect();
        let home = anyone_home(&counted, now, cfg.stale_hours);
        let delay = i64::from(cfg.away_delay_minutes) * 60_000;
        self.delay_ms = delay;
        let after_away = self.rule.acted_away();
        let act = if cfg.enabled {
            let was_pending = self.rule.pending_at(delay).is_some();
            let act = self.rule.step(home, now, delay);
            match (was_pending, self.rule.pending_at(delay)) {
                (false, Some(at)) => info!("presence: everyone is away; acting in {} min unless someone comes back", (at - now + 59_999).max(0) / 60_000),
                (true, None) if act.is_none() && home == Some(true) => info!("presence: someone is back before the away delay ended; nothing changes"),
                (true, None) if home.is_none() => info!("presence: no phone counts any more; the away action is called off"),
                _ => {}
            }
            act
        } else {
            self.rule.idle(home);
            None
        };
        let row = self.rule.to_row();
        if row != self.saved {
            match db.presence_rule_put(row.as_ref()) {
                Ok(()) => self.saved = row,
                Err(e) => warn!("presence: could not store the state: {e:#}"),
            }
        }
        Decision { act, after_away }
    }
}

/// Evaluate at least twice a minute (the away timer and phones going
/// stale); once an hour, remove phones silent for 30 days.
pub(super) fn presence_loop(engine: Arc<Engine>) {
    let mut last_purge: Option<std::time::Instant> = None;
    loop {
        if last_purge.is_none_or(|t| t.elapsed() >= std::time::Duration::from_secs(3600)) {
            last_purge = Some(std::time::Instant::now());
            match engine.db.presence_purge(Utc::now().timestamp_millis() - PURGE_AFTER_MS) {
                Ok(0) => {}
                Ok(n) => info!("presence: removed {n} phone(s) silent for 30 days"),
                Err(e) => warn!("presence: could not remove old phones: {e:#}"),
            }
        }
        engine.presence_evaluate(None);
        thread::sleep(std::time::Duration::from_secs(30));
    }
}

impl Engine {
    /// Someone home now: Some(true) / Some(false) / None (unknown), as last
    /// worked out.
    pub fn presence_home(&self) -> Option<bool> {
        self.presence.lock().unwrap().rule.home()
    }

    /// (state, when the away action is due (ms), the last action).
    pub fn presence_status(&self) -> (Option<bool>, Option<i64>, Option<LastAction>) {
        let p = self.presence.lock().unwrap();
        (p.rule.home(), p.rule.pending_at(p.delay_ms), p.last_action.clone())
    }

    /// Whether phones of account `user` count now (see PresenceConfig::counts).
    pub fn presence_counts(&self, user: &str) -> bool {
        let c = self.config.lock().unwrap();
        c.presence.counts(user, &account_names(&c))
    }

    /// Work out who is home and act on a change. `trigger` names the device
    /// whose report caused this (for the log).
    ///
    /// The whole evaluation (settings, devices, the rule, the actions and
    /// the last action) runs under `presence_eval`, so two evaluations never
    /// act out of order and a change of settings in between is seen.
    /// Lock order: presence_eval, then config (briefly, alone), then
    /// presence -> db, then the actions' own locks (profiles, manual_profile,
    /// ARMING, db). Callers must hold none of config, db, presence,
    /// manual_profile or ARMING.
    pub fn presence_evaluate(self: &Arc<Engine>, trigger: Option<&str>) {
        let _eval = self.presence_eval.lock().unwrap();
        let (cfg, accounts) = {
            let c = self.config.lock().unwrap();
            (c.presence.for_rule(&c.automation), account_names(&c))
        };
        let devs = match self.db.presence_list(None) {
            Ok(d) => d,
            Err(e) => {
                warn!("presence: could not read the devices: {e:#}");
                return;
            }
        };
        let now = Utc::now().timestamp_millis();
        let d = self.presence.lock().unwrap().decide(&cfg, &devs, &accounts, now, &self.db);
        if let Some(a) = d.act {
            let (did, reason) = self.presence_act(a, &cfg, trigger, d.after_away);
            self.presence.lock().unwrap().last_action =
                Some(LastAction { at_ms: now, event: if a == Act::Away { "away" } else { "home" }, did, reason });
        }
    }

    /// Carry out the away or home action; returns (what was done, why).
    /// `after_away`: presence had done the away action.
    fn presence_act(self: &Arc<Engine>, a: Act, cfg: &PresenceConfig, trigger: Option<&str>, after_away: bool) -> (String, String) {
        let mut did: Vec<String> = Vec::new();
        let reason = match a {
            Act::Away if cfg.away_delay_minutes > 0 => format!("everyone away {} min", cfg.away_delay_minutes),
            Act::Away => "everyone away".to_string(),
            Act::Home => match trigger {
                Some(t) => format!("{t} came home"),
                None => "someone came home".to_string(),
            },
        };
        let profile = |name: Option<String>, did: &mut Vec<String>| {
            let label = name.as_ref().map(|n| format!("profile:{n}")).unwrap_or_else(|| "schedule".into());
            match self.set_manual_profile(name, None, ACTOR, &reason) {
                Ok(()) => did.push(label),
                Err(e) => warn!("presence: {e}"),
            }
        };
        match a {
            Act::Away => {
                if !cfg.away_profile.is_empty() {
                    profile(Some(cfg.away_profile.clone()), &mut did);
                }
                if cfg.arms_when_away() {
                    self.set_shield(true, ACTOR, &reason);
                    did.push("armed".into());
                }
            }
            Act::Home => {
                if !cfg.home_profile.is_empty() {
                    profile(Some(cfg.home_profile.clone()), &mut did);
                } else if !cfg.away_profile.is_empty() && after_away {
                    // Back to the schedule, unless the away profile was
                    // replaced by hand meanwhile.
                    let held = self.manual_profile.lock().unwrap().as_ref().map(|(n, _)| n.clone());
                    if held.as_deref() == Some(cfg.away_profile.as_str()) {
                        profile(None, &mut did);
                    }
                }
                if cfg.disarm_when_home {
                    self.set_shield(false, ACTOR, &reason);
                    did.push("disarmed".into());
                }
            }
        }
        if did.is_empty() {
            info!("presence: {reason}; nothing set to change");
            did.push("none".into());
        }
        (did.join("+"), reason)
    }
}

/// Who presence's changes are recorded as.
pub const ACTOR: &str = "(presence)";

/// How a device is named in reasons and the log: `"Phone A" (laptop)`.
pub fn device_label(name: &str, user: &str) -> String {
    if user.is_empty() { format!("\"{name}\"") } else { format!("\"{name}\" ({user})") }
}


#[cfg(test)]
mod tests {
    use super::*;

    const MIN: i64 = 60_000;
    const H: i64 = 3_600_000;

    fn dev(user: &str, id: &str, home: bool, at: i64) -> PresenceDevice {
        PresenceDevice { user: user.into(), device_id: id.into(), name: id.into(), home, updated_ms: at, created_ms: 0 }
    }

    #[test]
    fn leaving_waits_for_the_delay_and_coming_back_acts_at_once() {
        let mut r = Rule::default();
        // The first state is a starting point only.
        assert_eq!(r.step(Some(true), 0, 10 * MIN), None);
        assert_eq!(r.step(Some(false), MIN, 10 * MIN), None);
        assert_eq!(r.pending_at(10 * MIN), Some(11 * MIN));
        assert_eq!(r.step(Some(false), 5 * MIN, 10 * MIN), None);
        assert_eq!(r.step(Some(false), 11 * MIN, 10 * MIN), Some(Act::Away));
        assert!(r.acted_away());
        // Once only.
        assert_eq!(r.step(Some(false), 12 * MIN, 10 * MIN), None);
        assert_eq!(r.step(Some(true), 13 * MIN, 10 * MIN), Some(Act::Home));
        assert!(!r.acted_away());
        assert_eq!(r.step(Some(true), 14 * MIN, 10 * MIN), None);
        // No delay: at once.
        assert_eq!(r.step(Some(false), 15 * MIN, 0), Some(Act::Away));
    }

    #[test]
    fn coming_back_before_the_delay_calls_it_off() {
        let mut r = Rule::default();
        r.step(Some(true), 0, 10 * MIN);
        r.step(Some(false), MIN, 10 * MIN);
        assert_eq!(r.step(Some(true), 5 * MIN, 10 * MIN), None, "nothing happened, so nothing to undo");
        assert_eq!(r.pending_at(10 * MIN), None);
        assert_eq!(r.step(Some(true), 20 * MIN, 10 * MIN), None);
        // Leaving again starts a new wait.
        assert_eq!(r.step(Some(false), 21 * MIN, 10 * MIN), None);
        assert_eq!(r.step(Some(false), 30 * MIN, 10 * MIN), None);
        assert_eq!(r.step(Some(false), 31 * MIN, 10 * MIN), Some(Act::Away));
    }

    #[test]
    fn unknown_keeps_the_last_known_state() {
        let mut r = Rule::default();
        // Unknown before anything is known: nothing; the first known state
        // is the starting point.
        assert_eq!(r.step(None, 0, 0), None);
        assert_eq!(r.step(Some(false), MIN, 0), None, "a start, not a change");
        assert_eq!(r.step(Some(true), 2 * MIN, 0), Some(Act::Home));
        // Everyone leaves, the house is armed, and the trip outlasts
        // stale_hours: unknown for days, then the first phone back says
        // home. That is away -> home: it acts.
        assert_eq!(r.step(Some(false), 3 * MIN, 0), Some(Act::Away));
        for t in 1..100 {
            assert_eq!(r.step(None, 3 * MIN + t * H, 0), None);
        }
        assert_eq!(r.home(), None);
        assert_eq!(r.step(Some(true), 200 * H, 0), Some(Act::Home));
        // Home, unknown, home again: the same state, nothing.
        assert_eq!(r.step(None, 300 * H, 0), None);
        assert_eq!(r.step(Some(true), 301 * H, 0), None);
        // Home, unknown, away: home -> away, the timer starts.
        assert_eq!(r.step(None, 302 * H, 10 * MIN), None);
        assert_eq!(r.step(Some(false), 303 * H, 10 * MIN), None);
        assert_eq!(r.pending_at(10 * MIN), Some(303 * H + 10 * MIN));
        // A pending away does not fire while unknown: it is called off,
        // and away again afterwards is the same state.
        assert_eq!(r.step(None, 303 * H + 5 * MIN, 10 * MIN), None);
        assert_eq!(r.pending_at(10 * MIN), None);
        assert_eq!(r.step(Some(false), 303 * H + 20 * MIN, 10 * MIN), None);
        assert_eq!(r.step(Some(false), 304 * H, 10 * MIN), None);
        // Coming back then acts (the house was not changed by presence,
        // but someone did come home).
        assert_eq!(r.step(Some(true), 305 * H, 10 * MIN), Some(Act::Home));
    }

    #[test]
    fn a_change_by_hand_stands_until_the_next_change() {
        // The rule only answers changes of who is home: the same state
        // reported again and again (heartbeats) never repeats an action, so
        // disarming by hand while everyone is away stays disarmed.
        let mut r = Rule::default();
        r.step(Some(true), 0, 0);
        assert_eq!(r.step(Some(false), MIN, 0), Some(Act::Away));
        for t in 2..50 {
            assert_eq!(r.step(Some(false), t * MIN, 0), None);
        }
        assert_eq!(r.step(Some(true), 60 * MIN, 0), Some(Act::Home));
        for t in 61..90 {
            assert_eq!(r.step(Some(true), t * MIN, 0), None);
        }
        // Turned off and on again: a new start, no action.
        r.idle(Some(true));
        assert_eq!(r.to_row(), None);
        assert_eq!(r.step(Some(true), 100 * MIN, 0), None);
        assert_eq!(r.step(Some(false), 101 * MIN, 0), Some(Act::Away));
    }

    #[test]
    fn a_changed_delay_applies_to_a_running_countdown() {
        let mut r = Rule::default();
        r.step(Some(true), 0, 30 * MIN);
        r.step(Some(false), MIN, 30 * MIN);
        assert_eq!(r.step(Some(false), 6 * MIN, 30 * MIN), None);
        // Shortened to 5 min: due already.
        assert_eq!(r.pending_at(5 * MIN), Some(6 * MIN));
        assert_eq!(r.step(Some(false), 6 * MIN, 5 * MIN), Some(Act::Away));
        // Lengthened while running: later.
        r.step(Some(true), 7 * MIN, 5 * MIN);
        r.step(Some(false), 8 * MIN, 5 * MIN);
        assert_eq!(r.step(Some(false), 14 * MIN, 60 * MIN), None);
        assert_eq!(r.step(Some(false), 68 * MIN, 60 * MIN), Some(Act::Away));
    }

    fn on(delay_min: u32) -> PresenceConfig {
        PresenceConfig { enabled: true, away_delay_minutes: delay_min, stale_hours: 12, ..Default::default() }
    }

    #[test]
    fn the_memory_survives_a_restart() {
        let db = crate::db::Db::open_in_memory().unwrap();
        let cfg = on(10);
        let mut p = PresenceState::default();
        // The very first enable: a starting point.
        assert_eq!(p.decide(&cfg, &[dev("", "a", true, 0)], &[], 0, &db).act, None);
        assert_eq!(db.presence_rule_get().unwrap().unwrap().known_home, Some(true));
        // Everyone leaves; the server stops before the delay ends.
        assert_eq!(p.decide(&cfg, &[dev("", "a", false, MIN)], &[], MIN, &db).act, None);
        let stored = db.presence_rule_get().unwrap().unwrap();
        assert_eq!((stored.known_home, stored.left_at_ms, stored.acted_away), (Some(false), Some(MIN), false));
        // Started again 20 min later, everyone still away: the away action
        // that came due while it was down happens at the first evaluation.
        let mut p = PresenceState::default();
        let d = p.decide(&cfg, &[dev("", "a", false, MIN)], &[], 21 * MIN, &db);
        assert_eq!(d, Decision { act: Some(Act::Away), after_away: false });
        assert!(db.presence_rule_get().unwrap().unwrap().acted_away);
        // Another restart: away is remembered, so home acts (and knows
        // presence did the away action).
        let mut p = PresenceState::default();
        assert_eq!(p.decide(&cfg, &[dev("", "a", false, MIN)], &[], 22 * MIN, &db).act, None);
        let mut p = PresenceState::default();
        let d = p.decide(&cfg, &[dev("", "a", true, 23 * MIN)], &[], 23 * MIN, &db);
        assert_eq!(d, Decision { act: Some(Act::Home), after_away: true });
        // A trip longer than stale_hours, with a restart in the middle.
        p.decide(&cfg, &[dev("", "a", false, 24 * MIN)], &[], 24 * MIN, &db);
        assert_eq!(p.decide(&cfg, &[dev("", "a", false, 24 * MIN)], &[], 40 * MIN, &db).act, Some(Act::Away));
        let mut p = PresenceState::default();
        assert_eq!(p.decide(&cfg, &[dev("", "a", false, 24 * MIN)], &[], 48 * H, &db).act, None, "unknown: nothing");
        assert_eq!(p.rule.home(), None);
        let d = p.decide(&cfg, &[dev("", "a", true, 49 * H)], &[], 49 * H, &db);
        assert_eq!(d.act, Some(Act::Home), "back after the trip: home acts");
        // Turning presence off forgets it; on again is a new start.
        p.decide(&PresenceConfig { enabled: false, ..cfg.clone() }, &[dev("", "a", true, 50 * H)], &[], 50 * H, &db);
        assert_eq!(db.presence_rule_get().unwrap(), None);
        let mut p = PresenceState::default();
        assert_eq!(p.decide(&cfg, &[dev("", "a", false, 51 * H)], &[], 51 * H, &db).act, None, "a start");
        assert_eq!(p.rule.pending_at(10 * MIN), None);
    }

    #[test]
    fn a_pending_away_is_called_off_if_no_phone_counts_after_a_restart() {
        let db = crate::db::Db::open_in_memory().unwrap();
        let cfg = on(10);
        let mut p = PresenceState::default();
        p.decide(&cfg, &[dev("", "a", true, 0)], &[], 0, &db);
        p.decide(&cfg, &[dev("", "a", false, MIN)], &[], MIN, &db);
        // Down for a day: the phone went stale meanwhile.
        let mut p = PresenceState::default();
        assert_eq!(p.decide(&cfg, &[dev("", "a", false, MIN)], &[], 24 * H, &db).act, None);
        assert_eq!(db.presence_rule_get().unwrap().unwrap().left_at_ms, None);
    }

    #[test]
    fn only_counted_accounts_decide() {
        let db = crate::db::Db::open_in_memory().unwrap();
        let mut cfg = on(0);
        cfg.accounts = vec!["ann".into()];
        let accounts: Vec<String> = vec!["ann".into(), "bob".into()];
        let mut p = PresenceState::default();
        p.decide(&cfg, &[dev("ann", "a", true, 0)], &accounts, 0, &db);
        // Bob (not listed) and a deleted account's phone at home do not
        // keep the house "home".
        let devs = [dev("ann", "a", false, MIN), dev("bob", "b", true, MIN), dev("gone", "g", true, MIN)];
        assert_eq!(p.decide(&cfg, &devs, &accounts, MIN, &db).act, Some(Act::Away));
        // Every account counts with no list (but still not "gone").
        cfg.accounts.clear();
        assert_eq!(p.decide(&cfg, &devs, &accounts, 2 * MIN, &db).act, Some(Act::Home));
    }

    #[test]
    fn engine_acts_and_records_who() {
        let mut c: crate::config::Config = toml::from_str("").unwrap();
        c.presence = on(0);
        let e = Engine::for_test(c);
        e.shield.store(false, std::sync::atomic::Ordering::Relaxed);
        let report = |home: bool, t: i64| {
            e.db.presence_report(&dev("", "p1", home, t), MAX_PER_USER, MAX_TOTAL).unwrap().unwrap();
            e.presence_evaluate(Some(&device_label("Phone A", "laptop")));
        };
        let now = Utc::now().timestamp_millis();
        report(true, now);
        report(false, now);
        assert!(e.shield.load(std::sync::atomic::Ordering::Relaxed), "armed when everyone left");
        let rows = e.db.audit_rows();
        assert_eq!(rows.last().unwrap(), &("(presence)".to_string(), "shield: armed".to_string(), "everyone away".to_string()));
        // Coming back with "disarm when home".
        e.config.lock().unwrap().presence.disarm_when_home = true;
        report(true, now);
        assert!(!e.shield.load(std::sync::atomic::Ordering::Relaxed));
        let (_, _, last) = e.presence_status();
        assert_eq!(last.unwrap().reason, "\"Phone A\" (laptop) came home");
        // Two evaluations at once never interleave (the second waits).
        let (a, b) = (e.clone(), e.clone());
        let t1 = std::thread::spawn(move || a.presence_evaluate(None));
        let t2 = std::thread::spawn(move || b.presence_evaluate(None));
        t1.join().unwrap();
        t2.join().unwrap();
        assert_eq!(e.db.audit_rows().len(), 2, "heartbeats changed nothing");
    }

    #[test]
    fn device_labels_are_quoted() {
        assert_eq!(device_label("Phone A", "laptop"), "\"Phone A\" (laptop)");
        assert_eq!(device_label("Phone A", ""), "\"Phone A\"");
    }

    #[test]
    fn stale_phones_are_left_out() {
        let h = 3_600_000;
        let now = 100 * h;
        // A phone at home that last reported 13 h ago does not count.
        let devs = vec![dev("ann", "a", true, now - 13 * h), dev("bob", "b", false, now - h)];
        assert_eq!(anyone_home(&devs, now, 12), Some(false));
        assert_eq!(anyone_home(&devs, now, 24), Some(true));
        assert_eq!(anyone_home(&devs[..1], now, 12), None);
        assert_eq!(anyone_home(&[], now, 12), None);
        assert!(fresh(&devs[0], now, 13));
        assert_eq!(state_word(None), "unknown");
    }

    #[test]
    fn checks_device_ids_and_names() {
        assert_eq!(clean_device_id(" 3f2a-B_9 ").unwrap(), "3f2a-B_9");
        assert!(clean_device_id("").is_err());
        assert!(clean_device_id("a b").is_err());
        assert!(clean_device_id("a/b").is_err());
        assert!(clean_device_id(&"x".repeat(65)).is_err());
        assert!(clean_device_id(&"x".repeat(64)).is_ok());
        assert_eq!(clean_name("  Ann's Pixel ").unwrap(), "Ann's Pixel");
        assert_eq!(clean_name("").unwrap(), "Phone");
        assert!(clean_name(&"é".repeat(64)).is_ok());
        assert!(clean_name(&"é".repeat(65)).is_err());
        assert!(clean_name("a\nb").is_err());
    }

    #[test]
    fn away_rule_follows_the_settings() {
        let mut c = PresenceConfig { enabled: true, ..Default::default() };
        assert_eq!(away_rule(&c), "armed");
        c.away_profile = "Away".into();
        assert_eq!(away_rule(&c), "profile:Away");
        c.arm_when_away = Some(true);
        assert_eq!(away_rule(&c), "profile:Away+armed");
        c.away_profile.clear();
        c.arm_when_away = Some(false);
        assert_eq!(away_rule(&c), "none");
        c.enabled = false;
        c.arm_when_away = None;
        assert_eq!(away_rule(&c), "none");
        // Settings from config.toml.
        let cfg: crate::config::Config = toml::from_str("[presence]\nenabled = true\naway_delay_minutes = 500\nstale_hours = 0\n").unwrap();
        let mut p = cfg.presence;
        p.sanitize();
        assert_eq!((p.away_delay_minutes, p.stale_hours, p.arms_when_away(), p.disarm_when_home), (120, 1, true, false));
        let d: crate::config::Config = toml::from_str("").unwrap();
        assert_eq!((d.presence.enabled, d.presence.away_delay_minutes, d.presence.stale_hours), (false, 10, 12));
    }
}
