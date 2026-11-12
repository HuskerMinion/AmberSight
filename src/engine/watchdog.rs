//! Camera watchdog: when a camera stops sending video, AmberSight tries to
//! fix it by itself (turned on in Settings > Connections > Watchdog, off by default).
//!
//! For each outage, step by step:
//! 1. After [watchdog] minutes without video (10 by default): if the camera
//!    still answers on the network, restart it through its own interface
//!    (camreboot.rs picks ONVIF, Reolink, Dahua/Amcrest or Hikvision).
//! 2. Still no video 5 minutes later (or it does not answer at all): cut its
//!    power and turn it back on, when a smart plug / PoE port is set up.
//! 3. Still nothing 5 minutes after that, or nothing could be tried: the
//!    usual "camera offline" system alert goes out, saying it needs a hand.
//!
//! While the watchdog is working on a camera, the plain offline alert is
//! held back, so the owner hears about it once, with the right words.
//!
//! Safety rules: at most [watchdog] max_per_day actions per camera in 24
//! hours, a new round at most once an hour, nothing in the first 5 minutes
//! after AmberSight starts, nothing during quiet hours, and nothing at all
//! while most cameras are offline at once (that is the network or a power
//! cut, not the cameras: alerts only). Disabled cameras, sleeping battery
//! cameras and other servers' cameras are never touched. Video back for 15
//! minutes ends the outage. Every action is written to the log and the
//! audit list (Settings > System > Audit, and the camera's Watchdog tab).

use super::*;
use std::collections::HashMap;
use std::time::Duration;

/// Wait this long after a restart or power cycle for video to return.
pub const WAIT_SECS: i64 = 5 * 60;
/// A new round of actions starts at most this often per camera.
pub const MIN_GAP_SECS: i64 = 60 * 60;
/// Video back this long ends the outage (and the backoff).
pub const STABLE_SECS: i64 = 15 * 60;
/// Nothing is done this soon after AmberSight starts.
pub const GRACE_SECS: i64 = 5 * 60;
const DAY: i64 = 24 * 60 * 60;

/// Where one camera's outage stands.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub enum Step {
    #[default]
    Watching,
    /// Restarted at this time; waiting for video.
    Rebooted(i64),
    /// Power cycled at this time; waiting for video.
    PowerCycled(i64),
    /// Nothing more to try for now (the owner is told).
    GaveUp(Why),
}

/// Why the watchdog stopped trying.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Why {
    /// No restart method and no power switch.
    NoWay,
    /// It does not answer on the network and there is no power switch.
    NotAnswering,
    /// Tried; video did not come back.
    NoHelp,
    /// The daily limit is used up.
    Limit,
}

/// What to do now.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Action {
    Nothing,
    Reboot,
    PowerCycle,
    /// Just gave up (said once per round).
    GiveUp(Why),
}

/// One camera's watch.
#[derive(Debug, Clone, Default)]
pub struct Watch {
    pub step: Step,
    /// When the last round began (for the one-round-an-hour backoff).
    pub round_at: Option<i64>,
    /// Times of restarts and power cycles in the last 24 hours.
    pub actions: Vec<i64>,
    /// Video back since (to end the outage once it has lasted).
    pub online_since: Option<i64>,
    /// When the plain offline problem was first let through for this outage.
    pub shown_since: Option<i64>,
    /// Minutes offline when it gave up (for the message).
    pub gave_up_after: i64,
    /// The owner has been told about this outage (said once).
    pub told: bool,
    /// The power was cut during this outage.
    pub tried_power: bool,
}

/// What the watchdog knows about a camera right now.
#[derive(Debug, Clone, Copy)]
pub struct Facts {
    pub now: i64,
    pub online: bool,
    pub offline_since: Option<i64>,
    /// Watched at all: watchdog on for it, enabled, awake, a network camera.
    pub watched: bool,
    /// Do nothing right now (just started, quiet hours, network down).
    pub hold: bool,
    /// It will not act for a long while (quiet hours, network down): let
    /// the plain offline alert through. (Not during the start-up grace: it
    /// acts a few minutes later.)
    pub pass_alert: bool,
    pub can_reboot: bool,
    pub can_power: bool,
    pub act_after: i64,
    pub max_per_day: usize,
}

/// The state machine: one step per check. `answers` asks (only when
/// needed) whether the camera still answers on the network.
pub fn decide(w: &mut Watch, f: &Facts, answers: &mut dyn FnMut() -> bool) -> Action {
    let now = f.now;
    w.actions.retain(|&t| now - t < DAY);
    if f.online {
        let since = *w.online_since.get_or_insert(now);
        if now - since >= STABLE_SECS {
            // Back for good: the outage is over.
            *w = Watch { actions: std::mem::take(&mut w.actions), online_since: Some(since), ..Default::default() };
        }
        return Action::Nothing;
    }
    w.online_since = None;
    if !f.watched || f.hold {
        return Action::Nothing;
    }
    let down = now - f.offline_since.unwrap_or(now);
    let budget = w.actions.len() < f.max_per_day;
    match w.step {
        Step::Watching => {
            if down < f.act_after || w.round_at.is_some_and(|r| now - r < MIN_GAP_SECS) {
                return Action::Nothing;
            }
            start_round(w, f, down, budget, answers)
        }
        Step::Rebooted(at) => {
            if now - at < WAIT_SECS || down < WAIT_SECS {
                return Action::Nothing;
            }
            if f.can_power && budget {
                w.actions.push(now);
                w.step = Step::PowerCycled(now);
                w.tried_power = true;
                return Action::PowerCycle;
            }
            give_up(w, if f.can_power { Why::Limit } else { Why::NoHelp }, now, down)
        }
        Step::PowerCycled(at) => {
            if now - at < WAIT_SECS || down < WAIT_SECS {
                return Action::Nothing;
            }
            give_up(w, Why::NoHelp, now, down)
        }
        Step::GaveUp(_) => {
            // Try another round an hour later, if there is something to try.
            let can = f.can_reboot || f.can_power;
            if can && budget && w.round_at.is_none_or(|r| now - r >= MIN_GAP_SECS) {
                return start_round(w, f, down, budget, answers);
            }
            Action::Nothing
        }
    }
}

fn start_round(w: &mut Watch, f: &Facts, down: i64, budget: bool, answers: &mut dyn FnMut() -> bool) -> Action {
    let now = f.now;
    let why = if !f.can_reboot && !f.can_power {
        Why::NoWay
    } else if !budget {
        Why::Limit
    } else if f.can_reboot && answers() {
        w.actions.push(now);
        w.round_at = Some(now);
        w.step = Step::Rebooted(now);
        return Action::Reboot;
    } else if f.can_power {
        w.actions.push(now);
        w.round_at = Some(now);
        w.step = Step::PowerCycled(now);
        w.tried_power = true;
        return Action::PowerCycle;
    } else {
        Why::NotAnswering
    };
    give_up(w, why, now, down)
}

fn give_up(w: &mut Watch, why: Why, now: i64, down: i64) -> Action {
    w.step = Step::GaveUp(why);
    w.round_at = Some(now);
    if w.told {
        // Said once per outage: later rounds that give up again stay quiet.
        return Action::Nothing;
    }
    w.told = true;
    w.gave_up_after = down / 60;
    Action::GiveUp(why)
}

/// Most cameras offline at the same time: the network (or the power) is
/// down, not the cameras. `offline` has one entry per watched camera.
pub fn network_looks_down(offline: &[bool]) -> bool {
    let n = offline.len();
    let down = offline.iter().filter(|&&o| o).count();
    n >= 3 && down * 2 > n
}

/// What the health report should do with a camera's offline problem.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Note {
    /// The watchdog is handling it: hold the plain offline alert back.
    pub withhold: bool,
    /// It gave up: the alert text to send instead.
    pub alert: Option<String>,
    /// What it is doing, for the health page.
    pub doing: Option<String>,
}

fn alert_text(name: &str, w: &Watch, why: Why, max: usize) -> String {
    let mins = w.gave_up_after.max(1);
    match why {
        Why::Limit => format!("{name} keeps losing its video ({max} restarts in a day); check it by hand"),
        Why::NoWay => format!("{name} has been offline {mins} min and can't be restarted from here; power-cycle it by hand"),
        Why::NotAnswering => format!("{name} has been offline {mins} min and isn't responding; power-cycle it by hand"),
        Why::NoHelp if w.tried_power => format!("{name} has been offline {mins} min and a power cycle didn't help; check it by hand"),
        Why::NoHelp => format!("{name} has been offline {mins} min and a restart didn't help; power-cycle it by hand"),
    }
}

/// The note for one camera. `alert_secs` is how long a problem must last
/// before the system alert reports it.
pub fn note_for(name: &str, w: &mut Watch, f: &Facts, alert_secs: i64) -> Option<Note> {
    if f.online || !f.watched {
        w.shown_since = None;
        return None;
    }
    let now = f.now;
    let ago = |t: i64| format!("{} min ago", ((now - t) / 60).max(1));
    match w.step {
        Step::GaveUp(why) => {
            // The owner is being told: a later round (an hour on) must not
            // hold the alert back again, or it would read as "resolved".
            w.shown_since.get_or_insert(now - alert_secs);
            Some(Note { alert: Some(alert_text(name, w, why, f.max_per_day)), ..Default::default() })
        }
        _ if f.pass_alert => {
            w.shown_since.get_or_insert(now);
            None
        }
        step => {
            // Once the plain alert may have gone out, keep it (holding it
            // back now would send a false "resolved").
            if w.shown_since.is_some_and(|s| now - s >= alert_secs - 60) {
                return None;
            }
            w.shown_since = None;
            let doing = match step {
                Step::Rebooted(t) => format!("AmberSight restarted it {}; waiting for video", ago(t)),
                Step::PowerCycled(t) => format!("AmberSight power-cycled it {}; waiting for video", ago(t)),
                _ => "AmberSight keeps trying".to_string(),
            };
            Some(Note { withhold: true, alert: None, doing: Some(doing) })
        }
    }
}

/// The watchdog's state, shared with the health report and the web page.
#[derive(Default)]
pub struct Board {
    pub watches: HashMap<usize, Watch>,
    pub notes: HashMap<usize, Note>,
    pub network_down: bool,
}

/// Whether the watchdog is on for a camera.
pub fn enabled_for(cfg: &CameraConfig, global: &crate::config::WatchdogConfig) -> bool {
    cfg.watchdog.as_ref().and_then(|w| w.enabled).unwrap_or(global.enabled)
}

fn power_of(cfg: &CameraConfig) -> Option<&crate::config::PowerSwitch> {
    cfg.watchdog.as_ref().and_then(|w| w.power.as_ref()).filter(|p| crate::camreboot::power_requests(p).is_ok())
}

pub fn watchdog_loop(engine: Arc<Engine>) {
    loop {
        thread::sleep(Duration::from_secs(30));
        engine.watchdog_check();
    }
}

impl Engine {
    /// One check of every camera (every 30 s).
    fn watchdog_check(self: &Arc<Engine>) {
        let (global, alert_secs) = {
            let c = self.config.lock().unwrap();
            (c.watchdog.clone(), c.alerts.system_alert_secs())
        };
        let now = Utc::now().timestamp();
        let local = chrono::Local::now();
        let minute = local.hour() * 60 + local.minute();
        let quiet = global.quiet_hours.as_deref().and_then(parse_schedule).is_some_and(|w| schedule_active(Some(w), minute));
        let starting = now - self.started < GRACE_SECS;
        let handles: Vec<Arc<CameraHandle>> = self.cameras.read().unwrap().iter().flatten().cloned().collect();
        struct Cam {
            h: Arc<CameraHandle>,
            cfg: CameraConfig,
            f: Facts,
        }
        let mut cams = Vec::new();
        for h in handles {
            let mut cfg = h.cfg.read().unwrap().clone();
            normalize_creds(&mut cfg);
            let (online, offline_since, sleeping) = {
                let st = h.state.read().unwrap();
                (st.online, st.offline_since, st.sleeping)
            };
            let network = crate::camreboot::is_network_camera(&cfg);
            let watched = network && h.active.load(Ordering::Relaxed) && !sleeping && !cfg.on_demand && enabled_for(&cfg, &global);
            let f = Facts {
                now,
                online,
                offline_since,
                watched,
                hold: starting || quiet,
                pass_alert: quiet,
                can_reboot: crate::camreboot::pick_method(&cfg) != crate::camreboot::Method::None,
                can_power: power_of(&cfg).is_some(),
                act_after: i64::from(global.minutes.clamp(2, 24 * 60)) * 60,
                max_per_day: global.max_per_day as usize,
            };
            cams.push(Cam { h, cfg, f });
        }
        // The network guard counts every enabled, awake network camera
        // (offline for over a minute), watched or not.
        let offline: Vec<bool> = cams
            .iter()
            .filter(|c| crate::camreboot::is_network_camera(&c.cfg) && c.h.active.load(Ordering::Relaxed) && !c.h.state.read().unwrap().sleeping)
            .map(|c| !c.f.online && c.f.offline_since.is_some_and(|s| now - s > 60))
            .collect();
        // (Only matters, and only said, when the watchdog watches something.)
        let network_down = network_looks_down(&offline) && cams.iter().any(|c| c.f.watched);
        // Whether each long-offline camera still answers on the network,
        // asked before taking the lock (it can take a few seconds).
        let answers: Vec<bool> = std::thread::scope(|s| {
            let jobs: Vec<_> = cams
                .iter()
                .map(|c| {
                    let ask = c.f.watched && !c.f.online && !c.f.hold && !network_down && c.f.offline_since.is_some_and(|t| now - t >= c.f.act_after);
                    let cfg = &c.cfg;
                    s.spawn(move || ask && crate::camreboot::answers(cfg))
                })
                .collect();
            jobs.into_iter().map(|j| j.join().unwrap_or(false)).collect()
        });
        let mut todo = Vec::new();
        {
            let mut guard = self.watchdog.lock().unwrap();
            let b = &mut *guard;
            if network_down && !b.network_down {
                warn!("watchdog: most cameras are offline at once (network or power?); not restarting anything");
            }
            b.network_down = network_down;
            let ids: Vec<usize> = cams.iter().map(|c| c.h.id).collect();
            b.watches.retain(|id, _| ids.contains(id));
            b.notes.clear();
            for (c, &answers) in cams.iter_mut().zip(answers.iter()) {
                c.f.hold |= network_down;
                c.f.pass_alert |= network_down;
                let w = b.watches.entry(c.h.id).or_default();
                let act = decide(w, &c.f, &mut || answers);
                if let Some(n) = note_for(&c.cfg.name, w, &c.f, alert_secs) {
                    b.notes.insert(c.h.id, n);
                }
                if act != Action::Nothing {
                    todo.push((act, c.cfg.clone(), w.gave_up_after));
                }
            }
        }
        for (act, cfg, mins) in todo {
            let e = self.clone();
            thread::spawn(move || e.watchdog_act(act, &cfg, mins));
        }
    }

    fn watchdog_act(&self, act: Action, cfg: &CameraConfig, mins: i64) {
        let name = &cfg.name;
        match act {
            Action::Reboot => {
                info!("watchdog: {name} has had no video for a while and still answers; restarting it");
                let r = crate::camreboot::reboot(cfg);
                self.audit_action("(watchdog)", "camera restart", name, r.as_ref().map(|m| m.label().to_string()).map_err(|e| format!("{e:#}")));
            }
            Action::PowerCycle => {
                info!("watchdog: {name} still has no video; cutting its power for a moment");
                let r = match power_of(cfg) {
                    Some(p) => crate::camreboot::power_cycle(p, &thread::sleep).map(|_| p.kind.clone()).map_err(|e| format!("{e:#}")),
                    None => Err("no power switch".into()),
                };
                self.audit_action("(watchdog)", "camera power cycle", name, r);
            }
            Action::GiveUp(why) => {
                let what = match why {
                    Why::NoWay => "no way to restart it from here",
                    Why::NotAnswering => "it does not answer and has no power switch",
                    Why::NoHelp => "restarting did not bring the video back",
                    Why::Limit => "the daily limit of restarts is used up",
                };
                warn!("watchdog: {name} offline {mins} min; {what}: the owner is told");
                let _ = self.db.insert_audit("(watchdog)", "", "watchdog gave up", &format!("{name}: {what}"), 200);
            }
            Action::Nothing => {}
        }
    }

    /// Record a restart / power cycle in the audit list ("<camera>: <how> ok"
    /// or "...failed: <why>").
    pub fn audit_action(&self, who: &str, action: &str, camera: &str, r: Result<String, String>) {
        let (detail, status) = match &r {
            Ok(how) => (format!("{camera}: {how}, ok"), 200),
            Err(e) => (format!("{camera}: failed: {e}"), 502),
        };
        match &r {
            Ok(_) => info!("{action}: {detail}"),
            Err(_) => warn!("{action}: {detail}"),
        }
        let _ = self.db.insert_audit(who, "", action, &detail, status);
    }

    /// How the health report should treat camera `id` being offline.
    pub fn watchdog_note(&self, id: usize) -> Option<Note> {
        self.watchdog.lock().unwrap().notes.get(&id).cloned()
    }

    /// Restart a camera now (the "Reboot now" button). Blocking.
    pub fn reboot_camera_now(&self, id: usize, who: &str) -> anyhow::Result<String> {
        let mut cfg = self.camera(id).ok_or_else(|| anyhow::anyhow!("no such camera"))?.cfg.read().unwrap().clone();
        normalize_creds(&mut cfg);
        let r = crate::camreboot::reboot(&cfg).map(|m| m.label().to_string());
        self.audit_action(who, "camera restart", &cfg.name, r.as_ref().map(|s| s.clone()).map_err(|e| format!("{e:#}")));
        r
    }

    /// Power-cycle a camera now (the "Test power cycle" button), with its
    /// saved switch or `power` (settings not saved yet). Blocking.
    pub fn power_cycle_now(&self, id: usize, who: &str, power: Option<crate::config::PowerSwitch>) -> anyhow::Result<()> {
        let cfg = self.camera(id).ok_or_else(|| anyhow::anyhow!("no such camera"))?.cfg.read().unwrap().clone();
        let p = power.or_else(|| cfg.watchdog.as_ref().and_then(|w| w.power.clone())).ok_or_else(|| anyhow::anyhow!("no power switch is set up for this camera"))?;
        let r = crate::camreboot::power_cycle(&p, &thread::sleep);
        self.audit_action(who, "camera power cycle", &cfg.name, r.as_ref().map(|_| p.kind.clone()).map_err(|e| format!("{e:#}")));
        r
    }

    /// The watchdog's view of one camera, for its settings window.
    pub fn watchdog_status(&self, id: usize) -> Option<serde_json::Value> {
        let h = self.camera(id)?;
        let mut cfg = h.cfg.read().unwrap().clone();
        normalize_creds(&mut cfg);
        let global = self.config.lock().unwrap().watchdog.clone();
        let now = Utc::now().timestamp();
        let b = self.watchdog.lock().unwrap();
        let w = b.watches.get(&id).cloned().unwrap_or_default();
        let state = match w.step {
            _ if !enabled_for(&cfg, &global) => "Off".to_string(),
            Step::Watching => "Watching".to_string(),
            Step::Rebooted(t) => format!("Restarted {} min ago; waiting for video", (now - t) / 60),
            Step::PowerCycled(t) => format!("Power-cycled {} min ago; waiting for video", (now - t) / 60),
            Step::GaveUp(_) => "Gave up for now; you were told".to_string(),
        };
        let method = crate::camreboot::pick_method(&cfg);
        let mut actions = Vec::new();
        for (ts, user, action, detail, status) in self.db.camera_actions(&cfg.name, 20).unwrap_or_default() {
            actions.push(serde_json::json!({ "ts": ts, "user": user, "action": action, "detail": detail, "ok": status < 400 }));
        }
        Some(serde_json::json!({
            "enabled": enabled_for(&cfg, &global),
            "state": state,
            "network_down": b.network_down,
            "method": method.label(),
            "can_reboot": method != crate::camreboot::Method::None,
            "can_power": power_of(&cfg).is_some(),
            "actions_today": w.actions.len(),
            "actions": actions,
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const MIN: i64 = 60;

    fn facts(now: i64, online: bool, offline_since: i64) -> Facts {
        Facts { now, online, offline_since: Some(offline_since), watched: true, hold: false, pass_alert: false, can_reboot: true, can_power: true, act_after: 10 * MIN, max_per_day: 3 }
    }

    /// Run the machine once a minute from `from` to `to` (camera offline
    /// since `since`), collecting what it did and when.
    fn run(w: &mut Watch, from: i64, to: i64, f: impl Fn(i64) -> Facts, answers: bool) -> Vec<(i64, Action)> {
        let mut out = Vec::new();
        let mut t = from;
        while t <= to {
            let a = decide(w, &f(t), &mut || answers);
            if a != Action::Nothing {
                out.push((t, a));
            }
            t += MIN;
        }
        out
    }

    #[test]
    fn reboot_then_power_then_give_up() {
        let mut w = Watch::default();
        let got = run(&mut w, 0, 40 * MIN, |t| facts(t, false, 0), true);
        assert_eq!(got, vec![(10 * MIN, Action::Reboot), (15 * MIN, Action::PowerCycle), (20 * MIN, Action::GiveUp(Why::NoHelp))]);
        assert_eq!(w.actions.len(), 2);
    }

    #[test]
    fn not_answering_goes_straight_to_power() {
        let mut w = Watch::default();
        let got = run(&mut w, 0, 30 * MIN, |t| facts(t, false, 0), false);
        assert_eq!(got, vec![(10 * MIN, Action::PowerCycle), (15 * MIN, Action::GiveUp(Why::NoHelp))]);
    }

    #[test]
    fn nothing_to_try_alerts_at_once() {
        let mut w = Watch::default();
        let got = run(&mut w, 0, 30 * MIN, |t| Facts { can_reboot: false, can_power: false, ..facts(t, false, 0) }, true);
        assert_eq!(got, vec![(10 * MIN, Action::GiveUp(Why::NoWay))]);
        let mut w = Watch::default();
        let got = run(&mut w, 0, 30 * MIN, |t| Facts { can_power: false, ..facts(t, false, 0) }, false);
        assert_eq!(got, vec![(10 * MIN, Action::GiveUp(Why::NotAnswering))]);
        // A restart only, no power switch: restart, then give up.
        let mut w = Watch::default();
        let got = run(&mut w, 0, 30 * MIN, |t| Facts { can_power: false, ..facts(t, false, 0) }, true);
        assert_eq!(got, vec![(10 * MIN, Action::Reboot), (15 * MIN, Action::GiveUp(Why::NoHelp))]);
    }

    #[test]
    fn video_back_after_reboot_ends_the_outage() {
        let mut w = Watch::default();
        run(&mut w, 0, 10 * MIN, |t| facts(t, false, 0), true);
        assert_eq!(w.step, Step::Rebooted(10 * MIN));
        // Back at 12 min and stays: no power cycle; after 15 min the outage is over.
        let got = run(&mut w, 12 * MIN, 40 * MIN, |t| facts(t, true, 0), true);
        assert!(got.is_empty());
        assert_eq!(w.step, Step::Watching);
        assert_eq!(w.round_at, None, "backoff ends with the outage");
        assert_eq!(w.actions.len(), 1, "the daily count stays");
    }

    #[test]
    fn a_short_comeback_continues_the_same_outage() {
        let mut w = Watch::default();
        run(&mut w, 0, 10 * MIN, |t| facts(t, false, 0), true);
        // Video for 3 minutes, then gone again at 14 min.
        run(&mut w, 11 * MIN, 13 * MIN, |t| facts(t, true, 0), true);
        assert_eq!(w.step, Step::Rebooted(10 * MIN));
        // The power cycle waits for 5 minutes of the new outage, not 10.
        let got = run(&mut w, 14 * MIN, 30 * MIN, |t| facts(t, false, 14 * MIN), true);
        assert_eq!(got[0], (19 * MIN, Action::PowerCycle));
    }

    #[test]
    fn backoff_one_round_an_hour_and_daily_limit() {
        let mut w = Watch::default();
        // Restart only: reboot at 10, give up at 15, next round an hour after 15.
        let f = |t| Facts { can_power: false, ..facts(t, false, 0) };
        let got = run(&mut w, 0, 4 * 60 * MIN, f, true);
        let acts: Vec<(i64, Action)> = got;
        assert_eq!(acts[0], (10 * MIN, Action::Reboot));
        assert_eq!(acts[1], (15 * MIN, Action::GiveUp(Why::NoHelp)));
        // The second round: an hour after giving up; giving up again stays quiet.
        assert_eq!(acts[2], (75 * MIN, Action::Reboot));
        assert_eq!(acts[3], (140 * MIN, Action::Reboot));
        // Three restarts in 24 h: no more.
        assert_eq!(acts.len(), 4);
        assert_eq!(w.actions.len(), 3);
        // A day later the limit is free again.
        let got = run(&mut w, 24 * 60 * MIN + 16 * MIN, 24 * 60 * MIN + 20 * MIN, f, true);
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].1, Action::Reboot);
    }

    #[test]
    fn limit_reached_at_the_start_of_an_outage_alerts() {
        let mut w = Watch { actions: vec![0, 1, 2], ..Default::default() };
        let got = run(&mut w, 3, 3 + 20 * MIN, |t| facts(t, false, 3), true);
        assert_eq!(got, vec![(3 + 10 * MIN, Action::GiveUp(Why::Limit))]);
        // Limit hit between the restart and the power cycle.
        let mut w = Watch { actions: vec![0, 1], ..Default::default() };
        let got = run(&mut w, 3, 3 + 20 * MIN, |t| facts(t, false, 3), true);
        assert_eq!(got, vec![(3 + 10 * MIN, Action::Reboot), (3 + 15 * MIN, Action::GiveUp(Why::Limit))]);
    }

    #[test]
    fn held_and_unwatched_cameras_are_left_alone() {
        let mut w = Watch::default();
        assert!(run(&mut w, 0, 60 * MIN, |t| Facts { hold: true, ..facts(t, false, 0) }, true).is_empty());
        assert!(run(&mut w, 0, 60 * MIN, |t| Facts { watched: false, ..facts(t, false, 0) }, true).is_empty());
        // The hold lifts (network back, grace over): it acts at once if the outage is long enough.
        assert_eq!(run(&mut w, 61 * MIN, 61 * MIN, |t| facts(t, false, 0), true), vec![(61 * MIN, Action::Reboot)]);
    }

    #[test]
    fn the_network_guard() {
        assert!(!network_looks_down(&[true, true]), "too few cameras to tell");
        assert!(!network_looks_down(&[true, false, false]));
        assert!(network_looks_down(&[true, true, false]));
        assert!(!network_looks_down(&[true, true, false, false]), "half is not most");
        assert!(network_looks_down(&[true, true, true, false]));
        assert!(!network_looks_down(&[]));
    }

    #[test]
    fn a_new_round_after_giving_up_keeps_the_alert_up() {
        let mut w = Watch::default();
        let f = |t| facts(t, false, 0);
        w.step = Step::GaveUp(Why::NoHelp);
        assert!(note_for("Shed", &mut w, &f(30 * MIN), 5 * MIN).unwrap().alert.is_some());
        // An hour on the watchdog tries again: the problem must stay reported
        // (no "resolved" while the camera is still down).
        w.step = Step::Rebooted(90 * MIN);
        assert_eq!(note_for("Shed", &mut w, &f(91 * MIN), 5 * MIN), None);
        w.step = Step::PowerCycled(95 * MIN);
        assert_eq!(note_for("Shed", &mut w, &f(96 * MIN), 5 * MIN), None);
    }

    #[test]
    fn notes_hold_back_then_replace_the_offline_alert() {
        let mut w = Watch::default();
        let f = |t| facts(t, false, 0);
        // Working on it: hold the plain alert back.
        let n = note_for("Shed", &mut w, &f(2 * MIN), 5 * MIN).unwrap();
        assert!(n.withhold && n.alert.is_none());
        run(&mut w, 0, 25 * MIN, f, false);
        let n = note_for("Shed", &mut w, &f(25 * MIN), 5 * MIN).unwrap();
        assert!(!n.withhold);
        assert_eq!(n.alert.as_deref(), Some("Shed has been offline 15 min and a power cycle didn't help; check it by hand"));
        let mut w2 = Watch::default();
        run(&mut w2, 0, 25 * MIN, |t| Facts { can_power: false, ..f(t) }, true);
        assert_eq!(note_for("Shed", &mut w2, &f(25 * MIN), 5 * MIN).unwrap().alert.as_deref(), Some("Shed has been offline 15 min and a restart didn't help; power-cycle it by hand"));
        // Online again: nothing.
        assert_eq!(note_for("Shed", &mut w, &facts(26 * MIN, true, 0), 5 * MIN), None);
        // Not watched: the plain alert.
        assert_eq!(note_for("Shed", &mut Watch::default(), &Facts { watched: false, ..f(MIN) }, 5 * MIN), None);
        let mut w = Watch::default();
        run(&mut w, 0, 10 * MIN, |t| Facts { can_power: false, ..f(t) }, false);
        assert_eq!(
            note_for("Back Gate", &mut w, &f(10 * MIN), 5 * MIN).unwrap().alert.as_deref(),
            Some("Back Gate has been offline 10 min and isn't responding; power-cycle it by hand")
        );
    }

    #[test]
    fn an_alert_already_out_is_not_held_back_later() {
        // Network down (held) long enough for the plain alert to go out...
        let mut w = Watch::default();
        for t in 0..=6 {
            assert_eq!(note_for("Shed", &mut w, &Facts { hold: true, pass_alert: true, ..facts(t * MIN, false, 0) }, 5 * MIN), None);
        }
        // ...then the hold lifts: keep the plain alert (no false "resolved").
        assert_eq!(note_for("Shed", &mut w, &facts(7 * MIN, false, 0), 5 * MIN), None);
        // A short hold (2 min) is still held back afterwards.
        let mut w = Watch::default();
        note_for("Shed", &mut w, &Facts { hold: true, pass_alert: true, ..facts(0, false, 0) }, 5 * MIN);
        note_for("Shed", &mut w, &Facts { hold: true, pass_alert: true, ..facts(MIN, false, 0) }, 5 * MIN);
        assert!(note_for("Shed", &mut w, &facts(2 * MIN, false, 0), 5 * MIN).unwrap().withhold);
        // The start-up grace holds actions back but not the watch: the alert waits too.
        let mut w = Watch::default();
        for t in 0..=6 {
            assert!(note_for("Shed", &mut w, &Facts { hold: true, ..facts(t * MIN, false, 0) }, 5 * MIN).unwrap().withhold);
        }
    }
}
