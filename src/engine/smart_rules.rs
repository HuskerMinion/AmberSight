//! Alert rules in plain words (see crate::alert_rules), applied as events
//! get their picture fingerprints, and the "quiet rule" hold: an ordinary
//! alert for a camera that has a quiet rule waits (a few seconds, the
//! rule's setting) until the event's pictures are checked, then goes out
//! unless a quiet rule matched. Cameras without a quiet rule are never held.

#![cfg_attr(all(target_os = "macos", target_arch = "x86_64"), allow(dead_code, unused_imports, unused_variables))]

use super::alerts::AlertOver;
use super::*;
use crate::alert_rules::{self as rules, AlertRule};
use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

/// A rule's words as a fingerprint (less the neutral words), by the words.
static VECS: Mutex<Vec<(String, std::sync::Arc<Vec<f32>>)>> = Mutex::new(Vec::new());
/// Ordinary alerts waiting for their event's pictures to be checked.
static HELD: Mutex<Option<HashMap<String, (Event, Instant, Option<String>)>>> = Mutex::new(None);
/// Events the rules have been applied to: id -> (a quiet rule matched, when).
/// Lets an alert that is put on hold after the loop was already done decide at once.
static DECIDED: Mutex<Option<HashMap<String, (bool, Instant)>>> = Mutex::new(None);
/// When a rule last alerted for a camera ("rule id|camera" -> seconds).
static LAST: Mutex<Option<HashMap<String, i64>>> = Mutex::new(None);

const MAX_HELD: usize = 100;
const MAX_DECIDED: usize = 300;
/// Events a preview looks at at most.
const PREVIEW_ITEMS: usize = 60;

fn local_minute(secs: i64) -> u32 {
    use chrono::Timelike;
    chrono::DateTime::<chrono::Utc>::from_timestamp(secs, 0).map(|t| t.with_timezone(&chrono::Local)).map_or(0, |t| t.hour() * 60 + t.minute())
}

/// Whether an AI review (JSON) says the ordinary alert was kept quiet
/// because the person was known: strangers only, a person marked "no alerts",
/// or the object detector's "only known" setting.
fn ordinary_alert_withheld(detail: &str) -> bool {
    serde_json::from_str::<serde_json::Value>(detail).is_ok_and(|d| {
        d["alert"]["sent"].as_bool() == Some(false) && matches!(d["alert"]["why"].as_str(), Some("known_only" | "quiet_people" | "odt_only_known"))
    })
}

fn last_key(rule: &AlertRule, camera: &str) -> String {
    format!("{}|{}", rule.id, camera)
}

impl Engine {
    pub fn alert_rules(&self) -> Vec<AlertRule> {
        self.config.lock().unwrap().alert_rules.clone()
    }

    /// Add a rule (empty id) or change one; the saved rule, or what is wrong with it.
    pub fn save_alert_rule(&self, mut r: AlertRule) -> Result<AlertRule, String> {
        r.sanitize()?;
        let mut cfg = self.config.lock().unwrap();
        if r.id.is_empty() {
            if cfg.alert_rules.len() >= rules::MAX_RULES {
                return Err(format!("at most {} rules", rules::MAX_RULES));
            }
            r.id = rules::new_id(&cfg.alert_rules, chrono::Utc::now().timestamp_millis());
            cfg.alert_rules.push(r.clone());
        } else {
            let Some(slot) = cfg.alert_rules.iter_mut().find(|x| x.id == r.id) else { return Err("no such rule".into()) };
            *slot = r.clone();
        }
        cfg.save(&self.config_path).map_err(|e| format!("could not save the settings: {e:#}"))?;
        Ok(r)
    }

    pub fn delete_alert_rule(&self, id: &str) -> Result<bool, String> {
        let mut cfg = self.config.lock().unwrap();
        let before = cfg.alert_rules.len();
        cfg.alert_rules.retain(|r| r.id != id);
        let gone = cfg.alert_rules.len() != before;
        if gone {
            cfg.save(&self.config_path).map_err(|e| format!("could not save the settings: {e:#}"))?;
        }
        Ok(gone)
    }

    /// Whether any rule is on (the background loop then checks more often).
    pub(super) fn rules_active(&self) -> bool {
        let c = self.config.lock().unwrap();
        c.smart_search.enabled && c.alert_rules.iter().any(|r| r.enabled)
    }

    /// An ordinary alert for a camera with a quiet rule waits for the
    /// event's pictures to be checked: true when it was put on hold (the
    /// background loop sends it, or drops it when a quiet rule matches).
    pub(super) fn hold_for_quiet(&self, ev: &Event, headline: Option<&str>) -> bool {
        #[cfg(not(all(target_os = "macos", target_arch = "x86_64")))]
        {
            let zone_event = ev.objects.first().is_some_and(|o| crate::tracker::is_zone_tag(o));
            if zone_event || super::packages::is_package_event(ev) || self.smart_models().is_none() {
                return false;
            }
            let minute = local_minute(ev.start);
            let secs = {
                let c = self.config.lock().unwrap();
                if !c.smart_search.enabled {
                    return false;
                }
                c.alert_rules.iter().filter(|r| r.is_quiet() && r.applies(&ev.camera, minute)).map(|r| r.hold_secs).max()
            };
            let Some(secs) = secs else { return false };
            let mut held = HELD.lock().unwrap();
            let map = held.get_or_insert_with(HashMap::new);
            if map.len() >= MAX_HELD {
                return false;
            }
            map.insert(ev.id.clone(), (ev.clone(), Instant::now() + Duration::from_secs(u64::from(secs)), headline.map(str::to_string)));
            drop(held);
            // The loop may have been done with this event before it was held:
            // then no one else will release it, so decide from that result.
            let decided = DECIDED.lock().unwrap().as_ref().and_then(|m| m.get(&ev.id).map(|d| d.0));
            if let Some(quiet) = decided {
                self.release_held(&ev.id, quiet);
            }
            true
        }
        #[cfg(all(target_os = "macos", target_arch = "x86_64"))]
        {
            let _ = (ev, headline);
            false
        }
    }

    /// Send (or, when `drop` is set, forget) the held alert of an event.
    fn release_held(&self, event_id: &str, drop: bool) {
        let held = HELD.lock().unwrap().as_mut().and_then(|m| m.remove(event_id));
        if let Some((ev, _, headline)) = held {
            if drop {
                info!("alert for event {} on {} not sent: a quiet rule matched", ev.id, ev.camera);
                if let Err(e) = self.db.set_ai_review_alert(&ev.id, false, "quiet_rule") {
                    warn!("could not update the AI review of {}: {e:#}", ev.id);
                }
            } else {
                // Names and tags added while it waited are in the alert.
                let ev = self.db.get_event(&ev.id).ok().flatten().unwrap_or(ev);
                self.notify_event(&ev, None, headline.as_deref());
            }
        }
    }

    /// The rules are done with an event: remember the result and release its
    /// held alert (sent, or dropped when a quiet rule matched).
    fn settle(&self, event_id: &str, quiet: bool) {
        {
            let mut d = DECIDED.lock().unwrap();
            let m = d.get_or_insert_with(HashMap::new);
            if m.len() >= MAX_DECIDED {
                let cut = Instant::now() - Duration::from_secs(600);
                m.retain(|_, v| v.1 > cut);
                if m.len() >= MAX_DECIDED {
                    m.clear();
                }
            }
            m.insert(event_id.to_string(), (quiet, Instant::now()));
        }
        self.release_held(event_id, quiet);
    }

    /// Held alerts whose time is up go out (the pictures were not checked
    /// in time, or smart search was turned off meanwhile).
    pub(super) fn release_expired_holds(&self) {
        let now = Instant::now();
        let due: Vec<String> = HELD.lock().unwrap().as_ref().map(|m| m.iter().filter(|(_, (_, t, _))| *t <= now).map(|(k, _)| k.clone()).collect()).unwrap_or_default();
        for id in due {
            self.release_held(&id, false);
        }
    }

    /// A rule's words as a fingerprint (the words less the neutral words),
    /// kept for the next time.
    #[cfg(not(all(target_os = "macos", target_arch = "x86_64")))]
    fn rule_vec(&self, m: &crate::smart::SmartModels, text: &str) -> anyhow::Result<std::sync::Arc<Vec<f32>>> {
        if let Some((_, v)) = VECS.lock().unwrap().iter().find(|(t, _)| t == text) {
            return Ok(v.clone());
        }
        let (q, n) = (m.embed_query(text)?, m.neutral()?);
        let v = std::sync::Arc::new(q.iter().zip(&n).map(|(a, b)| a - b).collect::<Vec<f32>>());
        let mut cache = VECS.lock().unwrap();
        if cache.len() >= 200 {
            cache.remove(0);
        }
        cache.push((text.to_string(), v.clone()));
        Ok(v)
    }

    /// Apply the rules to an event whose pictures have just got fingerprints
    /// (`embs`): tag it, and send the rule's alert or drop the held ordinary
    /// alert. Called from the background loop for every event it indexes.
    #[cfg(not(all(target_os = "macos", target_arch = "x86_64")))]
    pub(super) fn apply_rules(&self, m: &crate::smart::SmartModels, id: &str, embs: &[&[u8]]) {
        let Ok(Some(ev)) = self.db.get_event(id) else {
            self.settle(id, false);
            return;
        };
        let (rules_all, enabled) = {
            let c = self.config.lock().unwrap();
            (c.alert_rules.clone(), c.smart_search.enabled)
        };
        let now = chrono::Utc::now().timestamp();
        // Only young events: the backlog of older ones is made searchable
        // without alerting about last week.
        if !enabled || embs.is_empty() || now - ev.end > rules::MAX_EVENT_AGE_S || !rules_all.iter().any(|r| r.enabled) {
            self.settle(id, false);
            return;
        }
        let minute = local_minute(ev.start);
        let scores: Vec<Option<f32>> = rules_all
            .iter()
            .map(|r| {
                if !r.applies(&ev.camera, minute) {
                    return None;
                }
                match self.rule_vec(m, &r.text) {
                    Ok(q) => rules::best_score(embs.iter().copied(), &q),
                    Err(e) => {
                        warn!("alert rule '{}': {e:#}", r.name);
                        None
                    }
                }
            })
            .collect();
        let cooled = |i: usize| {
            let r = &rules_all[i];
            let last = LAST.lock().unwrap().as_ref().and_then(|m| m.get(&last_key(r, &ev.camera)).copied());
            last.is_none_or(|t| now - t >= i64::from(r.cooldown_min) * 60)
        };
        let plan = rules::plan(&rules_all, &scores, &cooled);
        if plan.tags.is_empty() && plan.quiet.is_none() {
            self.settle(id, false);
            return;
        }
        let tags: Vec<String> = plan.tags.iter().chain(plan.quiet.iter()).map(|&i| rules::tag_for(&rules_all[i])).collect();
        // Added to what the event has now (names may be added at the same time).
        let mut ev2 = ev.clone();
        match self.db.add_event_objects(id, &tags) {
            Ok(all) if !all.is_empty() => ev2.objects = all,
            Ok(_) => {}
            Err(e) => warn!("could not tag event {id}: {e:#}"),
        }
        if let Some(q) = plan.quiet {
            info!("event {id} on {}: quiet rule '{}' matched ({:.3})", ev.camera, rules_all[q].name, scores[q].unwrap_or(0.0));
            self.settle(id, true);
            return;
        }
        // The ordinary alert first: the rule's alert is the more specific one after it.
        self.settle(id, false);
        let Some(i) = plan.alert else { return };
        // An event whose ordinary alert was kept quiet because the person was
        // known gets no rule alert either.
        if self.db.ai_review(id).ok().flatten().is_some_and(|d| ordinary_alert_withheld(&d)) {
            info!("event {id} on {}: rule alert not sent, the ordinary alert was withheld (known person)", ev.camera);
            return;
        }
        let r = &rules_all[i];
        let cam = self.config.lock().unwrap().cameras.iter().find(|c| c.name == ev.camera).cloned();
        if cam.as_ref().is_some_and(|c| !self.alerts_enabled(c)) {
            return;
        }
        LAST.lock().unwrap().get_or_insert_with(HashMap::new).insert(last_key(r, &ev.camera), now);
        info!("event {id} on {}: rule '{}' matched ({:.3})", ev.camera, r.name, scores[i].unwrap_or(0.0));
        let over = AlertOver {
            title: rules::alert_title(r, &ev.camera),
            priority: match r.action.as_str() {
                "alert_high" => super::notify::Priority::High,
                "notice" => super::notify::Priority::Low,
                _ => super::notify::Priority::Normal,
            },
            rule_id: r.id.clone(),
            name: r.name.clone(),
        };
        self.notify_event(&ev2, Some(&over), None);
    }

    /// Which recent events a rule's words would have matched, with their
    /// scores (best first), so the threshold can be tuned. `cameras`: only
    /// those (empty: all the caller may see).
    pub fn rule_preview(&self, text: &str, cameras: &[String], hours: u32, allowed: &dyn Fn(&str) -> bool) -> anyhow::Result<serde_json::Value> {
        #[cfg(not(all(target_os = "macos", target_arch = "x86_64")))]
        {
            let Some(m) = self.smart_models() else { anyhow::bail!("smart search is not ready yet") };
            let text = text.trim();
            if text.is_empty() || text.chars().count() > 200 {
                anyhow::bail!("describe what to look for in up to 200 characters");
            }
            let q = self.rule_vec(&m, text)?;
            let now = chrono::Utc::now().timestamp_millis();
            let from = now - i64::from(hours.clamp(1, 24 * 30)) * 3_600_000;
            let mut best: HashMap<String, (f32, i64, String, i64, Option<String>, Option<String>)> = HashMap::new();
            self.db.smart_scan(from, now, &mut |id, ev, cam, at, bx, label, emb| {
                if !allowed(cam) || (!cameras.is_empty() && !cameras.iter().any(|c| c.eq_ignore_ascii_case(cam))) {
                    return;
                }
                let s = crate::smart::score(emb, &q);
                if best.get(ev).is_none_or(|b| s > b.0) {
                    best.insert(ev.to_string(), (s, id, cam.to_string(), at, bx.map(String::from), label.map(String::from)));
                }
            })?;
            let scanned = best.len();
            let mut all: Vec<(String, (f32, i64, String, i64, Option<String>, Option<String>))> = best.into_iter().collect();
            all.sort_by(|a, b| b.1 .0.total_cmp(&a.1 .0));
            let median = all.get(all.len() / 2).map(|x| x.1 .0);
            let items: Vec<serde_json::Value> = all
                .into_iter()
                .take(PREVIEW_ITEMS)
                .map(|(ev, (score, pic, cam, at, bx, label))| {
                    serde_json::json!({
                        "event_id": ev, "camera": cam, "at_ms": at, "pic": pic, "score": score, "label": label,
                        "box": bx.and_then(|b| serde_json::from_str::<serde_json::Value>(&b).ok()),
                    })
                })
                .collect();
            Ok(serde_json::json!({ "scanned": scanned, "hours": hours.clamp(1, 24 * 30), "median": median, "items": items }))
        }
        #[cfg(all(target_os = "macos", target_arch = "x86_64"))]
        {
            let _ = (text, cameras, hours, allowed);
            anyhow::bail!("smart search is not available on this computer")
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn known_people_keep_rule_alerts_quiet_too() {
        let d = |why: &str, sent: bool| format!(r#"{{"alert":{{"sent":{sent},"why":"{why}"}}}}"#);
        for why in ["known_only", "quiet_people", "odt_only_known"] {
            assert!(ordinary_alert_withheld(&d(why, false)), "{why}");
        }
        assert!(!ordinary_alert_withheld(&d("sent", true)));
        assert!(!ordinary_alert_withheld(&d("alerts_off", false)));
        assert!(!ordinary_alert_withheld("not json"));
    }
}
