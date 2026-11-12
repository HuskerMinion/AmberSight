//! Alert rules in plain words ("a delivery truck stopped out front"):
//! the rule's words become a CLIP fingerprint (see crate::smart) that is
//! compared with the fingerprints of every new event's pictures. A picture
//! that matches at least the rule's threshold tags the event "rule: <name>"
//! and, by the rule's action, sends an alert. "Quiet" rules do the opposite:
//! an event that matches one is tagged "quiet: <name>" and its alerts are
//! not sent.
//!
//! The match score is how much more a picture looks like the words than
//! like words that say nothing in particular (the same measure smart search
//! ranks by), typically around 0 for unrelated pictures and 0.05 to 0.15
//! for good matches. It is approximate: the preview in Settings shows the
//! scores of recent events so the threshold can be set from real pictures.
#![cfg_attr(all(target_os = "macos", target_arch = "x86_64"), allow(dead_code))]

use serde::{Deserialize, Serialize};

pub const MAX_RULES: usize = 40;
/// Alert rules are applied to events this young only (seconds): finishing an
/// old backlog must not send alerts about last week.
pub const MAX_EVENT_AGE_S: i64 = 15 * 60;
pub const DEFAULT_THRESHOLD: f32 = 0.03;

fn d_kind() -> String {
    "alert".into()
}
fn d_action() -> String {
    "alert".into()
}
fn d_threshold() -> f32 {
    DEFAULT_THRESHOLD
}
fn d_true() -> bool {
    true
}
fn d_cooldown() -> u32 {
    5
}
fn d_hold() -> u32 {
    30
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct AlertRule {
    #[serde(default)]
    pub id: String,
    pub name: String,
    /// The words: what the picture should show.
    pub text: String,
    /// "alert" or "quiet".
    #[serde(default = "d_kind")]
    pub kind: String,
    /// Camera names; none = every camera.
    #[serde(default)]
    pub cameras: Vec<String>,
    #[serde(default = "d_threshold")]
    pub threshold: f32,
    /// "alert_high", "alert", "notice" or "tag" (alert rules only).
    #[serde(default = "d_action")]
    pub action: String,
    /// "HH:MM-HH:MM": when the event happened (like a camera's alert window).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub schedule: Option<String>,
    #[serde(default = "d_true")]
    pub enabled: bool,
    /// Minutes before the same rule alerts again for the same camera.
    #[serde(default = "d_cooldown")]
    pub cooldown_min: u32,
    /// Quiet rules: seconds an ordinary alert of the cameras may wait for the
    /// pictures to be checked.
    #[serde(default = "d_hold")]
    pub hold_secs: u32,
}

impl AlertRule {
    pub fn is_quiet(&self) -> bool {
        self.kind == "quiet"
    }

    /// Check and tidy what was sent (the numbers kept sensible). Errors are
    /// worded for the person who typed it.
    pub fn sanitize(&mut self) -> Result<(), String> {
        self.name = self.name.split_whitespace().collect::<Vec<_>>().join(" ");
        self.text = self.text.split_whitespace().collect::<Vec<_>>().join(" ");
        if self.name.is_empty() || self.name.chars().count() > 60 {
            return Err("give the rule a name of up to 60 characters".into());
        }
        if self.name.contains(['\n', '\r']) {
            return Err("the name must be on one line".into());
        }
        if self.text.is_empty() || self.text.chars().count() > 200 {
            return Err("describe what to look for in up to 200 characters".into());
        }
        if !matches!(self.kind.as_str(), "alert" | "quiet") {
            return Err("kind must be alert or quiet".into());
        }
        if !matches!(self.action.as_str(), "alert_high" | "alert" | "notice" | "tag") {
            return Err("action must be alert_high, alert, notice or tag".into());
        }
        if !self.threshold.is_finite() {
            return Err("the threshold must be a number".into());
        }
        self.threshold = self.threshold.clamp(-0.2, 0.6);
        self.cooldown_min = self.cooldown_min.min(24 * 60);
        self.hold_secs = self.hold_secs.clamp(5, 90);
        self.cameras = self.cameras.iter().map(|c| c.trim().to_string()).filter(|c| !c.is_empty()).take(100).collect();
        self.schedule = match self.schedule.as_deref().map(str::trim).filter(|s| !s.is_empty()) {
            None => None,
            Some(s) if crate::config::parse_schedule(s).is_some() => Some(s.to_string()),
            Some(_) => return Err("the time window must look like 18:00-23:30".into()),
        };
        Ok(())
    }

    /// Whether the rule looks at an event of `camera` that happened at
    /// `minute` of the day.
    pub fn applies(&self, camera: &str, minute: u32) -> bool {
        if !self.enabled {
            return false;
        }
        if !self.cameras.is_empty() && !self.cameras.iter().any(|c| c.eq_ignore_ascii_case(camera)) {
            return false;
        }
        match self.schedule.as_deref().and_then(crate::config::parse_schedule) {
            Some(w) => crate::config::schedule_active(Some(w), minute),
            None => true,
        }
    }

    /// 0 notice, 1 normal, 2 high: which action wins when several match.
    pub fn rank(&self) -> u8 {
        match self.action.as_str() {
            "alert_high" => 2,
            "alert" => 1,
            _ => 0,
        }
    }
}

/// The best (highest) score among an event's fingerprints against the
/// rule's fingerprint `q` (the words less the neutral words).
pub fn best_score<'a>(embs: impl IntoIterator<Item = &'a [u8]>, q: &[f32]) -> Option<f32> {
    embs.into_iter().map(|e| crate::smart::score(e, q)).fold(None, |a, s| Some(a.map_or(s, |a: f32| a.max(s))))
}

/// What to do with an event: the rules' scores in, the decision out.
#[derive(Debug, Default, PartialEq)]
pub struct Plan {
    /// A quiet rule matched (its index): the event is tagged and its alerts are not sent.
    pub quiet: Option<usize>,
    /// Alert rules that matched (tag the event "rule: <name>").
    pub tags: Vec<usize>,
    /// The rule whose alert goes out (one per event: the strongest action, then the best score).
    pub alert: Option<usize>,
}

/// `scores[i]`: rule i's best score on the event, None when the rule does not
/// look at it (off, other camera, outside its time window). `cooled(i)`:
/// whether rule i may alert again for this camera.
pub fn plan(rules: &[AlertRule], scores: &[Option<f32>], cooled: &dyn Fn(usize) -> bool) -> Plan {
    let mut p = Plan::default();
    let mut best_alert: Option<(u8, f32, usize)> = None;
    let mut best_quiet: Option<(f32, usize)> = None;
    for (i, r) in rules.iter().enumerate() {
        let Some(s) = scores.get(i).copied().flatten() else { continue };
        if s < r.threshold {
            continue;
        }
        if r.is_quiet() {
            if best_quiet.is_none_or(|(b, _)| s > b) {
                best_quiet = Some((s, i));
            }
            continue;
        }
        p.tags.push(i);
        if r.action != "tag" && cooled(i) && best_alert.is_none_or(|(rk, b, _)| (r.rank(), s) > (rk, b)) {
            best_alert = Some((r.rank(), s, i));
        }
    }
    p.quiet = best_quiet.map(|(_, i)| i);
    // A quiet match silences the rule alerts too.
    p.alert = if p.quiet.is_some() { None } else { best_alert.map(|(_, _, i)| i) };
    p
}

/// The alert's title: "Delivery truck out front at Front Door".
pub fn alert_title(rule: &AlertRule, camera: &str) -> String {
    format!("{} at {}", rule.name, camera)
}

/// The tag added to a matching event.
pub fn tag_for(rule: &AlertRule) -> String {
    format!("{}: {}", if rule.is_quiet() { "quiet" } else { "rule" }, rule.name)
}

/// A camera was renamed: the rules that name it follow.
pub fn rename_camera(rules: &mut [AlertRule], old: &str, new: &str) {
    for r in rules.iter_mut() {
        for c in r.cameras.iter_mut().filter(|c| *c == old) {
            *c = new.to_string();
        }
    }
}

/// A camera was removed: rules that named it stop naming it. A rule that
/// named only that camera is turned off (an empty list means every camera).
pub fn forget_camera(rules: &mut [AlertRule], name: &str) {
    for r in rules.iter_mut() {
        let before = r.cameras.len();
        r.cameras.retain(|c| c != name);
        if before > 0 && r.cameras.is_empty() {
            r.enabled = false;
        }
    }
}

/// A short unused id for a new rule.
pub fn new_id(existing: &[AlertRule], now_ms: i64) -> String {
    let mut n = now_ms.unsigned_abs();
    loop {
        let id = format!("r{n:x}");
        if !existing.iter().any(|r| r.id == id) {
            return id;
        }
        n += 1;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rule(name: &str, kind: &str, action: &str, threshold: f32) -> AlertRule {
        AlertRule {
            id: name.into(),
            name: name.into(),
            text: "a delivery truck".into(),
            kind: kind.into(),
            cameras: Vec::new(),
            threshold,
            action: action.into(),
            schedule: None,
            enabled: true,
            cooldown_min: 5,
            hold_secs: 30,
        }
    }

    #[test]
    fn best_score_takes_the_best_picture() {
        let q: Vec<f32> = (0..crate::smart::DIM).map(|i| if i == 0 { 1.0 } else { 0.0 }).collect();
        let mk = |x: f32| {
            let mut v = vec![0.0f32; crate::smart::DIM];
            v[0] = x;
            crate::smart::to_bytes(&v)
        };
        let (a, b) = (mk(0.1), mk(0.4));
        let s = best_score([a.as_slice(), b.as_slice()], &q).unwrap();
        assert!((s - 0.4).abs() < 0.01, "{s}");
        assert_eq!(best_score(std::iter::empty::<&[u8]>(), &q), None);
    }

    #[test]
    fn a_score_must_reach_the_threshold() {
        let rules = vec![rule("truck", "alert", "alert", 0.08)];
        let all = |_: usize| true;
        assert_eq!(plan(&rules, &[Some(0.079)], &all), Plan::default());
        assert_eq!(plan(&rules, &[Some(0.08)], &all), Plan { quiet: None, tags: vec![0], alert: Some(0) });
        // A rule that does not look at the event (None) never matches.
        assert_eq!(plan(&rules, &[None], &all), Plan::default());
    }

    #[test]
    fn tag_only_rules_tag_without_alerting_and_cooldown_holds_the_alert() {
        let rules = vec![rule("note", "alert", "tag", 0.05), rule("truck", "alert", "alert", 0.05)];
        let p = plan(&rules, &[Some(0.2), Some(0.1)], &|i| i != 1);
        assert_eq!(p.tags, vec![0, 1]);
        assert_eq!(p.alert, None);
        let p = plan(&rules, &[Some(0.2), Some(0.1)], &|_| true);
        assert_eq!(p.alert, Some(1));
    }

    #[test]
    fn the_strongest_action_then_the_best_score_sends_the_one_alert() {
        let rules = vec![rule("a", "alert", "alert", 0.05), rule("b", "alert", "alert_high", 0.05), rule("c", "alert", "alert_high", 0.05), rule("d", "alert", "notice", 0.05)];
        let p = plan(&rules, &[Some(0.3), Some(0.1), Some(0.2), Some(0.9)], &|_| true);
        assert_eq!(p.alert, Some(2));
        assert_eq!(p.tags, vec![0, 1, 2, 3]);
    }

    #[test]
    fn a_quiet_match_silences_the_rule_alerts_but_not_the_tags() {
        let rules = vec![rule("truck", "alert", "alert", 0.05), rule("my car", "quiet", "alert", 0.07)];
        let p = plan(&rules, &[Some(0.2), Some(0.09)], &|_| true);
        assert_eq!(p.quiet, Some(1));
        assert_eq!(p.alert, None);
        assert_eq!(p.tags, vec![0]);
        // Below its threshold the quiet rule does nothing.
        let p = plan(&rules, &[Some(0.2), Some(0.06)], &|_| true);
        assert_eq!((p.quiet, p.alert), (None, Some(0)));
    }

    #[test]
    fn rules_look_only_at_their_cameras_and_time_window() {
        let mut r = rule("x", "alert", "alert", 0.05);
        assert!(r.applies("Front Door", 600));
        r.cameras = vec!["front door".into()];
        assert!(r.applies("Front Door", 600));
        assert!(!r.applies("Back Yard", 600));
        r.schedule = Some("22:00-06:00".into());
        assert!(r.applies("Front Door", 23 * 60));
        assert!(r.applies("Front Door", 5 * 60));
        assert!(!r.applies("Front Door", 12 * 60));
        r.enabled = false;
        assert!(!r.applies("Front Door", 23 * 60));
    }

    #[test]
    fn sanitize_checks_and_tidies() {
        let mut r = rule("  Delivery   truck ", "alert", "alert_high", 5.0);
        r.hold_secs = 1000;
        r.sanitize().unwrap();
        assert_eq!((r.name.as_str(), r.threshold, r.hold_secs), ("Delivery truck", 0.6, 90));
        assert!(rule("", "alert", "alert", 0.1).sanitize().is_err());
        assert!(rule("x", "loud", "alert", 0.1).sanitize().is_err());
        assert!(rule("x", "alert", "shout", 0.1).sanitize().is_err());
        assert!(rule("x", "alert", "alert", f32::NAN).sanitize().is_err());
        let mut bad = rule("x", "alert", "alert", 0.1);
        bad.schedule = Some("evening".into());
        assert!(bad.sanitize().is_err());
        let mut empty_text = rule("x", "alert", "alert", 0.1);
        empty_text.text = "   ".into();
        assert!(empty_text.sanitize().is_err());
    }

    #[test]
    fn titles_and_tags_name_the_rule() {
        let r = rule("Delivery truck out front", "alert", "alert", 0.1);
        assert_eq!(alert_title(&r, "Front Door"), "Delivery truck out front at Front Door");
        assert_eq!(tag_for(&r), "rule: Delivery truck out front");
        assert_eq!(tag_for(&rule("my car", "quiet", "alert", 0.1)), "quiet: my car");
    }

    #[test]
    fn new_ids_do_not_repeat() {
        let a = rule("a", "alert", "alert", 0.1);
        let id = new_id(&[], 1000);
        let mut b = a.clone();
        b.id = id.clone();
        assert_ne!(new_id(&[b], 1000), id);
    }

    #[test]
    fn camera_rename_and_removal_follow_the_rules() {
        let mut a = rule("a", "alert", "alert", 0.3);
        a.cameras = vec!["Front".into(), "Back".into()];
        let mut b = rule("b", "alert", "alert", 0.3);
        b.cameras = vec!["Front".into()];
        let mut c = rule("c", "alert", "alert", 0.3);
        c.cameras = vec![];
        let mut v = vec![a, b, c];
        rename_camera(&mut v, "Front", "Porch");
        assert_eq!(v[0].cameras, ["Porch", "Back"]);
        assert_eq!(v[1].cameras, ["Porch"]);
        forget_camera(&mut v, "Porch");
        assert_eq!(v[0].cameras, ["Back"]);
        assert!(v[0].enabled);
        assert!(v[1].cameras.is_empty() && !v[1].enabled);
        assert!(v[2].enabled);
    }
}
