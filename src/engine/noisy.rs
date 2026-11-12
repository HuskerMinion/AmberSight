//! "Too many notifications" helper.
//!
//! Two parts:
//! - the notification-only areas of a camera (`alert_ignore_polys`): an
//!   ordinary alert is skipped when every object that confirmed the event
//!   stood inside one of them. Nothing else changes: the event is still
//!   detected, recorded, labelled and searchable.
//! - a look at the last 24 hours per camera: how many notifications went
//!   out, for what, and where in the picture. A camera that sent a lot for
//!   one thing in one place gets a suggestion (notify for people only, or
//!   stay quiet for that area), shown on the Status page and in the
//!   morning summary.

use super::*;
use crate::config::{CameraConfig, ZonePoly};
use serde_json::Value;

/// A camera that sent more than this many notifications in 24 hours is looked at.
pub const NOISY_OVER: usize = 30;
/// A pattern has to explain at least this share of the notifications.
const MIN_SHARE: f64 = 0.5;
/// Of one kind of object, this share has to be in one band of the picture.
const MIN_BAND_SHARE: f64 = 0.6;
/// "Not now" hides a suggestion for this long.
pub const DISMISS_DAYS: i64 = 7;
const DISMISS_SCOPE: &str = "noisy_dismiss";
/// How long an answer is kept.
const NOISY_FRESH: std::time::Duration = std::time::Duration::from_secs(60);
/// How far past the objects the suggested area reaches (fraction of the picture).
const PAD: f64 = 0.04;

/// One object that confirmed an event, as the AI review recorded it: label,
/// centre of its box (fractions of the picture), score.
#[derive(Clone, Debug, PartialEq)]
pub struct Conf {
    pub label: String,
    pub x: f64,
    pub y: f64,
    pub score: f64,
}

/// The objects that confirmed an event: wanted label, score high enough, not
/// parked (as the AI review saved them). `keep` drops labels the camera
/// would not notify for anyway.
pub fn confirming(detail: &Value, keep: &dyn Fn(&str) -> bool) -> Vec<Conf> {
    let wanted: Vec<&str> = detail["wanted"].as_array().into_iter().flatten().filter_map(|w| w.as_str()).collect();
    let min = detail["min_score"].as_f64().unwrap_or(0.0);
    let mut out = Vec::new();
    for c in detail["checks"].as_array().into_iter().flatten() {
        let seen = c["seen"].as_array().cloned().unwrap_or_default();
        let parked = c["parked"].as_array().cloned().unwrap_or_default();
        for (i, d) in seen.iter().enumerate() {
            let label = d["label"].as_str().unwrap_or("");
            let score = d["score"].as_f64().unwrap_or(0.0);
            let (x, y, w, h) = (d["x"].as_f64().unwrap_or(0.0), d["y"].as_f64().unwrap_or(0.0), d["w"].as_f64().unwrap_or(0.0), d["h"].as_f64().unwrap_or(0.0));
            if label.is_empty() || !wanted.contains(&label) || score < min || parked.get(i).and_then(|p| p.as_bool()).unwrap_or(false) || w <= 0.0 || h <= 0.0 || !keep(label) {
                continue;
            }
            out.push(Conf { label: label.to_string(), x: x + w / 2.0, y: y + h / 2.0, score });
        }
    }
    out
}

/// Whether all of `conf` (at least one) stand inside one of `areas`.
pub fn all_in_areas(areas: &[ZonePoly], conf: &[Conf]) -> bool {
    !conf.is_empty() && conf.iter().all(|c| areas.iter().any(|a| a.contains(c.x, c.y)))
}

/// Whether the camera's notification-only areas silence an event with these
/// labels whose AI review is `detail` (JSON text). Sounds, doorbell presses and
/// packages have no place in the picture: never silenced.
pub fn silenced_by_area(cam: &CameraConfig, objects: &[String], detail: &str) -> bool {
    if cam.alert_ignore_polys.is_empty() || objects.iter().any(|o| crate::config::always_notifies(o)) {
        return false;
    }
    let Ok(v) = serde_json::from_str::<Value>(detail) else { return false };
    let conf = confirming(&v, &|l| cam.notifies_for(&[l.to_string()]));
    all_in_areas(&cam.alert_ignore_polys, &conf)
}

/// One notification that went out, for the look at a camera.
#[derive(Clone, Debug)]
pub struct Sent {
    pub event_id: String,
    pub conf: Vec<Conf>,
    /// The owner tapped "Not a real alert" on it.
    pub not_real: bool,
}

#[derive(Clone, Debug, serde::Serialize, PartialEq)]
pub struct Example {
    pub event_id: String,
}

#[derive(Clone, Debug, serde::Serialize)]
pub struct Suggestion {
    pub camera: String,
    pub camera_id: usize,
    /// Notifications sent in the last 24 hours.
    pub sent: usize,
    /// The kind of object most of them were for ("car").
    pub top: String,
    /// How many of the notifications that was, 0..1 (inside `region` when there is one).
    pub share: f64,
    /// Notifications by object, most first.
    pub labels: Vec<(String, usize)>,
    /// Padded outline of where they were (corners, 0..1), when they gathered in one place.
    pub region: Option<Vec<[f64; 2]>>,
    /// "top third", "left third" ...
    pub region_name: Option<String>,
    pub examples: Vec<Example>,
    /// "Not a real alert" taps on those notifications.
    pub not_real: usize,
    /// Notifications the camera's remembered spots held back in the same 24
    /// hours (something that stays in one place, see engine::spots).
    pub held_back: usize,
    /// "people_only", "ignore_area", "not_now".
    pub actions: Vec<&'static str>,
    /// The suggestion in plain words.
    pub text: String,
}

/// Vehicles count as one kind of object for the pattern.
fn kind(label: &str) -> &str {
    if ["car", "truck", "bus", "motorcycle", "bicycle", "van"].contains(&label) {
        "vehicle"
    } else {
        label
    }
}

fn plural(label: &str) -> String {
    match label {
        "person" => "people".into(),
        "bus" => "buses".into(),
        l if l.ends_with('s') => l.to_string(),
        l => format!("{l}s"),
    }
}

fn share_phrase(share: f64) -> &'static str {
    if share >= 0.9 {
        "Nearly all"
    } else if share >= 0.7 {
        "4 of 5"
    } else if share >= 0.58 {
        "3 of 5"
    } else {
        "About half"
    }
}

/// The four bands of the picture a pattern is looked for in.
const BANDS: [(&str, usize, bool); 4] = [("top third", 1, true), ("bottom third", 1, false), ("left third", 0, true), ("right third", 0, false)];

fn in_band(c: &Conf, axis: usize, low: bool) -> bool {
    let v = if axis == 0 { c.x } else { c.y };
    if low { v < 1.0 / 3.0 } else { v > 2.0 / 3.0 }
}

/// Padded outline around `pts` (a box), reaching the picture's edge when
/// the objects came close to it; clipped to 0..1.
fn outline(pts: &[(f64, f64)]) -> Vec<[f64; 2]> {
    let trimmed = |mut v: Vec<f64>| {
        v.sort_by(|a, b| a.total_cmp(b));
        let cut = v.len() / 20;
        (v[cut], v[v.len() - 1 - cut])
    };
    let (x0, x1) = trimmed(pts.iter().map(|p| p.0).collect());
    let (y0, y1) = trimmed(pts.iter().map(|p| p.1).collect());
    let edge = |lo: f64, hi: f64| {
        let (lo, hi) = ((lo - PAD).max(0.0), (hi + PAD).min(1.0));
        (if lo < 0.08 { 0.0 } else { lo }, if hi > 0.92 { 1.0 } else { hi })
    };
    let ((x0, x1), (y0, y1)) = (edge(x0, x1), edge(y0, y1));
    let r = |v: f64| (v * 1000.0).round() / 1000.0;
    vec![[r(x0), r(y0)], [r(x1), r(y0)], [r(x1), r(y1)], [r(x0), r(y1)]]
}

/// What the camera would notify for today: each past notification keeps only
/// the objects the camera still notifies for (set to people only, a car is
/// not to blame any more), and one with none left is not counted.
pub fn still_notifying(cam: &CameraConfig, sent: Vec<Sent>) -> Vec<Sent> {
    sent.into_iter()
        .map(|mut s| {
            s.conf.retain(|c| cam.notifies_for(&[c.label.clone()]));
            s
        })
        .filter(|s| !s.conf.is_empty())
        .collect()
}

/// Look at the notifications one camera sent in the last 24 hours (newest
/// first). A suggestion when it sent a lot and one thing in one place
/// explains half of them.
pub fn analyze(camera: &str, camera_id: usize, sent: &[Sent]) -> Option<Suggestion> {
    let total = sent.len();
    if total <= NOISY_OVER {
        return None;
    }
    // What each notification was for: the most sure confirming object.
    let mut each: Vec<(&Sent, &Conf)> = Vec::new();
    for s in sent {
        if let Some(c) = s.conf.iter().max_by(|a, b| a.score.total_cmp(&b.score)) {
            each.push((s, c));
        }
    }
    let mut by_kind: Vec<(String, usize)> = Vec::new();
    for (_, c) in &each {
        let k = kind(&c.label).to_string();
        match by_kind.iter_mut().find(|(l, _)| *l == k) {
            Some(e) => e.1 += 1,
            None => by_kind.push((k, 1)),
        }
    }
    by_kind.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
    let (top_kind, top_n) = by_kind.first().cloned()?;
    if (top_n as f64) / (total as f64) < MIN_SHARE {
        return None;
    }
    // The name to show: the commonest label of that kind ("car").
    let mut names: Vec<(&str, usize)> = Vec::new();
    for (_, c) in each.iter().filter(|(_, c)| kind(&c.label) == top_kind) {
        match names.iter_mut().find(|(l, _)| *l == c.label) {
            Some(e) => e.1 += 1,
            None => names.push((c.label.as_str(), 1)),
        }
    }
    names.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(b.0)));
    let top = names.first().map(|n| n.0.to_string()).unwrap_or_else(|| top_kind.clone());
    let mine: Vec<(&Sent, &Conf)> = each.iter().filter(|(_, c)| kind(&c.label) == top_kind).cloned().collect();
    // Where: the band holding most of them.
    let mut best: Option<(&str, usize, usize, bool)> = None;
    for (name, axis, low) in BANDS {
        let n = mine.iter().filter(|(_, c)| in_band(c, axis, low)).count();
        if best.is_none_or(|b| n > b.1) {
            best = Some((name, n, axis, low));
        }
    }
    let band = best.filter(|b| (b.1 as f64) / (mine.len() as f64) >= MIN_BAND_SHARE && (b.1 as f64) / (total as f64) >= MIN_SHARE);
    let (region, region_name, in_region): (Option<Vec<[f64; 2]>>, Option<String>, Vec<&(&Sent, &Conf)>) = match band {
        Some((name, _, axis, low)) => {
            let inside: Vec<&(&Sent, &Conf)> = mine.iter().filter(|(_, c)| in_band(c, axis, low)).collect();
            let pts: Vec<(f64, f64)> = inside.iter().map(|(_, c)| (c.x, c.y)).collect();
            (Some(outline(&pts)), Some(name.to_string()), inside)
        }
        None => (None, None, mine.iter().collect()),
    };
    let share = in_region.len() as f64 / total as f64;
    let examples: Vec<Example> = in_region.iter().take(3).map(|(s, _)| Example { event_id: s.event_id.clone() }).collect();
    let mut labels: Vec<(String, usize)> = Vec::new();
    for (_, c) in &each {
        match labels.iter_mut().find(|(l, _)| *l == c.label) {
            Some(e) => e.1 += 1,
            None => labels.push((c.label.clone(), 1)),
        }
    }
    labels.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
    let mut actions: Vec<&'static str> = Vec::new();
    if top_kind != "person" {
        actions.push("people_only");
    }
    if region.is_some() {
        actions.push("ignore_area");
    }
    if actions.is_empty() || share < MIN_SHARE {
        return None;
    }
    actions.push("not_now");
    let where_ = region_name.as_ref().map(|r| format!(" in the {r} of the picture")).unwrap_or_default();
    let why = match (top_kind.as_str(), region.is_some()) {
        ("vehicle", true) => ", probably passing traffic",
        ("person", true) => ", probably people passing by",
        _ => "",
    };
    let text = format!("{camera} sent {total} notifications in the last 24 hours. {} were {}{where_}{why}.", share_phrase(share), plural(&top));
    Some(Suggestion {
        camera: camera.to_string(),
        camera_id,
        sent: total,
        top,
        share: (share * 100.0).round() / 100.0,
        labels,
        region,
        region_name,
        examples,
        not_real: sent.iter().filter(|s| s.not_real).count(),
        held_back: 0,
        actions,
        text,
    })
}

impl Engine {
    /// Suggestions for the cameras that sent too many notifications in the
    /// last 24 hours, leaving out those hidden with "Not now".
    /// The answer is kept for about a minute (reading the reviews is heavy), or
    /// until a camera's notification settings or a "Not now" change it.
    pub fn noisy_suggestions(&self) -> Vec<Suggestion> {
        static CACHE: Mutex<Option<(std::time::Instant, String, Vec<Suggestion>)>> = Mutex::new(None);
        static COMPUTE: Mutex<()> = Mutex::new(());
        let hidden: std::collections::BTreeMap<String, i64> =
            self.db.ui_scope(DISMISS_SCOPE).unwrap_or_default().into_iter().filter_map(|(k, v)| v.parse().ok().map(|u| (k, u))).collect();
        let cams: Vec<(usize, CameraConfig)> = self.cameras.read().unwrap().iter().flatten().map(|h| (h.id, h.cfg.read().unwrap().clone())).collect();
        let settings: Vec<_> = cams.iter().map(|(id, c)| (*id, &c.name, c.alerts, &c.alert_labels)).collect();
        let key = format!("{:?}|{hidden:?}|{settings:?}", self.data_dir);
        // One at a time: asks that arrive meanwhile take its answer.
        let _one = COMPUTE.lock().unwrap_or_else(|e| e.into_inner());
        if let Some((at, k, v)) = &*CACHE.lock().unwrap_or_else(|e| e.into_inner()) {
            if at.elapsed() < NOISY_FRESH && *k == key {
                return v.clone();
            }
        }
        let v = self.noisy_suggestions_now(&hidden, cams);
        *CACHE.lock().unwrap_or_else(|e| e.into_inner()) = Some((std::time::Instant::now(), key, v.clone()));
        v
    }

    fn noisy_suggestions_now(&self, hidden: &std::collections::BTreeMap<String, i64>, cams: Vec<(usize, CameraConfig)>) -> Vec<Suggestion> {
        let now = Utc::now().timestamp();
        let rows = match self.db.sent_reviews(now - 24 * 3600) {
            Ok(r) => r,
            Err(e) => {
                warn!("noisy: {e:#}");
                return Vec::new();
            }
        };
        let mut out = Vec::new();
        for (id, cam) in cams {
            if cam.alerts == Some(false) || hidden.get(&cam.name).is_some_and(|&u| u > now) {
                continue;
            }
            let sent: Vec<Sent> = rows
                .iter()
                .filter(|r| r.1 == cam.name)
                .map(|r| Sent {
                    event_id: r.0.clone(),
                    conf: serde_json::from_str::<Value>(&r.3).map(|d| confirming(&d, &|_| true)).unwrap_or_default(),
                    not_real: r.4.as_deref() == Some("false_alarm"),
                })
                .collect();
            let sent = still_notifying(&cam, sent);
            if let Some(mut s) = analyze(&cam.name, id, &sent) {
                // Already doing what the suggestion says.
                let only_people = cam.alert_labels.as_ref().is_some_and(|l| l.len() == 1 && l[0].eq_ignore_ascii_case("person"));
                s.actions.retain(|a| !(*a == "people_only" && only_people));
                s.held_back = self.held_back_by_spots(&cam.name, now - 24 * 3600);
                if s.actions.iter().any(|a| *a != "not_now") {
                    out.push(s);
                }
            }
        }
        out.sort_by(|a, b| b.sent.cmp(&a.sent));
        out
    }

    /// Events of a camera since `since` that did not alert because a
    /// remembered spot held back what was seen (AI review "fixed_spot").
    pub fn held_back_by_spots(&self, camera: &str, since: i64) -> usize {
        if self.db.review_sums_ready() {
            return self.db.fixed_spot_reviews(since, camera).unwrap_or_default();
        }
        self.db
            .ai_reviews(since, Some(camera), 1000)
            .unwrap_or_default()
            .iter()
            .filter(|r| !r.3 && r.4.contains("\"spot\"") && serde_json::from_str::<Value>(&r.4).is_ok_and(|d| crate::ai_review::summarize(&d)["kind"] == "fixed_spot"))
            .count()
    }

    /// Hide a camera's suggestion for DISMISS_DAYS days.
    pub fn noisy_dismiss(&self, camera: &str) {
        let until = Utc::now().timestamp() + DISMISS_DAYS * 24 * 3600;
        if let Err(e) = self.db.ui_put(DISMISS_SCOPE, camera, &until.to_string()) {
            warn!("noisy: could not keep that: {e:#}");
        }
    }

    /// The morning summary's lines: one per suggestion.
    pub fn noisy_notes(&self) -> Vec<String> {
        self.noisy_suggestions().iter().map(|s| format!("{} See Status to quiet it.", s.text)).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn conf(label: &str, x: f64, y: f64) -> Conf {
        Conf { label: label.into(), x, y, score: 0.8 }
    }
    fn sent(i: usize, c: Vec<Conf>) -> Sent {
        Sent { event_id: format!("e{i}"), conf: c, not_real: i % 10 == 0 }
    }

    #[test]
    fn confirming_skips_parked_weak_and_unwanted() {
        let d = serde_json::json!({
            "wanted": ["person", "car"], "min_score": 0.5,
            "checks": [{"seen": [
                {"label": "car", "score": 0.9, "x": 0.1, "y": 0.0, "w": 0.2, "h": 0.1},
                {"label": "car", "score": 0.9, "x": 0.5, "y": 0.5, "w": 0.2, "h": 0.1},
                {"label": "car", "score": 0.3, "x": 0.5, "y": 0.5, "w": 0.2, "h": 0.1},
                {"label": "dog", "score": 0.9, "x": 0.5, "y": 0.5, "w": 0.2, "h": 0.1},
                {"label": "person", "score": 0.9, "x": 0.0, "y": 0.0, "w": 0.0, "h": 0.0}
            ], "parked": [false, true, false, false, false]}]
        });
        let c = confirming(&d, &|_| true);
        assert_eq!(c.len(), 1);
        assert!((c[0].x - 0.2).abs() < 1e-9 && (c[0].y - 0.05).abs() < 1e-9);
        assert!(confirming(&d, &|l| l == "person").is_empty(), "the camera does not notify for cars");
    }

    #[test]
    fn area_silences_only_when_every_object_is_inside() {
        let top = ZonePoly { points: vec![[0.0, 0.0], [1.0, 0.0], [1.0, 0.3], [0.0, 0.3]] };
        let areas = [top];
        assert!(all_in_areas(&areas, &[conf("car", 0.5, 0.1)]));
        assert!(all_in_areas(&areas, &[conf("car", 0.5, 0.1), conf("person", 0.2, 0.2)]));
        assert!(!all_in_areas(&areas, &[conf("car", 0.5, 0.1), conf("person", 0.2, 0.6)]), "a person below the strip still notifies");
        assert!(!all_in_areas(&areas, &[]), "nothing to judge: the alert stays");
        assert!(!all_in_areas(&[], &[conf("car", 0.5, 0.1)]));
    }

    #[test]
    fn silenced_by_area_reads_the_review_and_respects_notify_objects() {
        let mut cam: CameraConfig = toml::from_str("name = \"A\"\nurl = \"rtsp://x/a\"\n").unwrap();
        let review = serde_json::json!({"wanted": ["person", "car"], "min_score": 0.5, "checks": [{"seen": [
            {"label": "car", "score": 0.9, "x": 0.4, "y": 0.05, "w": 0.2, "h": 0.1},
            {"label": "person", "score": 0.9, "x": 0.4, "y": 0.6, "w": 0.1, "h": 0.3}], "parked": [false, false]}]})
        .to_string();
        assert!(!silenced_by_area(&cam, &[], &review), "no areas");
        cam.alert_ignore_polys = vec![ZonePoly { points: vec![[0.0, 0.0], [1.0, 0.0], [1.0, 0.3], [0.0, 0.3]] }];
        assert!(!silenced_by_area(&cam, &[], &review), "the person is outside");
        cam.alert_labels = Some(vec!["vehicle".into()]);
        assert!(silenced_by_area(&cam, &[], &review), "the person would not notify anyway; the car is in the strip");
        assert!(!silenced_by_area(&cam, &[], "not json"));
        for l in ["visitor", "sound", "package", "package picked up"] {
            assert!(!silenced_by_area(&cam, &["car".to_string(), l.to_string()], &review), "{l}");
        }
    }

    #[test]
    fn suggests_for_traffic_in_the_top_strip() {
        let mut rows = Vec::new();
        for i in 0..80 {
            rows.push(sent(i, vec![conf("car", 0.1 + (i % 8) as f64 * 0.1, 0.08 + (i % 5) as f64 * 0.03)]));
        }
        for i in 80..100 {
            rows.push(sent(i, vec![conf("person", 0.5, 0.7)]));
        }
        let s = analyze("Front Door", 3, &rows).expect("a suggestion");
        assert_eq!((s.sent, s.top.as_str(), s.camera_id), (100, "car", 3));
        assert_eq!(s.region_name.as_deref(), Some("top third"));
        let r = s.region.unwrap();
        assert_eq!(r[0][1], 0.0, "reaches the top edge");
        assert!(r[2][1] < 0.34 && r[2][1] > 0.2, "{r:?}");
        assert_eq!(s.actions, vec!["people_only", "ignore_area", "not_now"]);
        assert_eq!(s.examples.len(), 3);
        assert_eq!(s.not_real, 10);
        assert!((s.share - 0.8).abs() < 1e-9);
        assert_eq!(s.text, "Front Door sent 100 notifications in the last 24 hours. 4 of 5 were cars in the top third of the picture, probably passing traffic.");
    }

    #[test]
    fn a_camera_set_to_people_only_is_not_blamed_for_cars() {
        let mut cam: CameraConfig = toml::from_str("name = \"A\"\nurl = \"rtsp://x/a\"\n").unwrap();
        let mut rows: Vec<Sent> = (0..80).map(|i| sent(i, vec![conf("car", 0.5, 0.1)])).collect();
        rows.extend((80..100).map(|i| sent(i, vec![conf("person", 0.5, 0.7)])));
        assert!(analyze("A", 0, &still_notifying(&cam, rows.clone())).is_some(), "notifies for everything: cars are the cause");
        cam.alert_labels = Some(vec!["person".into()]);
        let now = still_notifying(&cam, rows.clone());
        assert_eq!(now.len(), 20, "only the people would notify now");
        assert!(analyze("A", 0, &now).is_none(), "20 is not too many, and no car card");
        // A notification for both keeps the object that still counts.
        let both = still_notifying(&cam, vec![sent(1, vec![conf("car", 0.5, 0.1), conf("person", 0.4, 0.5)])]);
        assert_eq!(both[0].conf.len(), 1);
        assert_eq!(both[0].conf[0].label, "person");
    }

    #[test]
    fn quiet_cameras_and_scattered_notifications_get_nothing() {
        let few: Vec<Sent> = (0..30).map(|i| sent(i, vec![conf("car", 0.5, 0.1)])).collect();
        assert!(analyze("A", 0, &few).is_none(), "30 is not too many");
        // Many, spread over the whole picture: cars all over, no band.
        let spread: Vec<Sent> = (0..60).map(|i| sent(i, vec![conf("car", (i % 10) as f64 / 10.0 + 0.05, (i % 7) as f64 / 7.0 + 0.05)])).collect();
        let s = analyze("A", 0, &spread).expect("cars are still most of it");
        assert!(s.region.is_none());
        assert_eq!(s.actions, vec!["people_only", "not_now"]);
        // A mix with no leader.
        let mix: Vec<Sent> = (0..60).map(|i| sent(i, vec![conf(["person", "dog", "cat", "bird"][i % 4], 0.5, 0.1)])).collect();
        assert!(analyze("A", 0, &mix).is_none());
        // People in one place: only the area can be quieted.
        let people: Vec<Sent> = (0..50).map(|i| sent(i, vec![conf("person", 0.9, 0.5)])).collect();
        assert_eq!(analyze("A", 0, &people).unwrap().actions, vec!["ignore_area", "not_now"]);
    }

    /// A webhook on a local port tells whether an alert went out.
    /// Webhooks are sent by running curl: without it these tests skip.
    /// (A slow test machine may take a few seconds to start it.)
    fn have_curl() -> bool {
        let ok = std::process::Command::new("curl").arg("--version").output().is_ok_and(|o| o.status.success());
        if !ok {
            eprintln!("skipping: curl is not installed");
        }
        ok
    }

    fn alert_goes_out(areas: &str, labels: &str, objects: &[&str], review: &Value, eid: &str) -> (bool, Option<String>) {
        let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        l.set_nonblocking(true).unwrap();
        let port = l.local_addr().unwrap().port();
        let cfg: crate::config::Config = toml::from_str(&format!(
            "[alerts]\nwebhook_url = \"http://127.0.0.1:{port}/hook\"\n[[cameras]]\nname = \"Street\"\nurl = \"rtsp://x/a\"\n{areas}\n{labels}\n"
        ))
        .unwrap();
        let e = Engine::for_test(cfg);
        let ev = Event { protect: false, id: eid.into(), camera_id: 0, camera: "Street".into(), start: Utc::now().timestamp(), end: Utc::now().timestamp() + 5, score: 1.0, clip: None, thumb: None, objects: objects.iter().map(|o| o.to_string()).collect() };
        e.db.save_ai_review(eid, "Street", ev.start, true, &review.to_string()).unwrap();
        e.notify_event(&ev, None, None);
        let t0 = std::time::Instant::now();
        let got = loop {
            if l.accept().is_ok() {
                break true;
            }
            if t0.elapsed() > std::time::Duration::from_secs(6) {
                break false;
            }
            std::thread::sleep(std::time::Duration::from_millis(50));
        };
        let why = e.db.ai_review(eid).unwrap().and_then(|d| serde_json::from_str::<Value>(&d).ok()).and_then(|v| v["alert"]["why"].as_str().map(String::from));
        (got, why)
    }

    #[test]
    fn a_notification_in_a_quiet_area_is_not_sent_but_the_event_is_kept() {
        if !have_curl() {
            return;
        }
        let strip = "alert_ignore_polys = [{ points = [[0.0, 0.0], [1.0, 0.0], [1.0, 0.3], [0.0, 0.3]] }]";
        let at = |y: f64| serde_json::json!({"wanted": ["person", "car"], "min_score": 0.5, "alert": {"sent": true, "why": "sent"},
            "checks": [{"seen": [{"label": "car", "score": 0.9, "x": 0.4, "y": y, "w": 0.2, "h": 0.1}], "parked": [false]}]});
        let (sent, why) = alert_goes_out(strip, "", &["car"], &at(0.05), "q1");
        assert!(!sent, "a car in the top strip is silent");
        assert_eq!(why.as_deref(), Some("notify_area"));
        let (sent, why) = alert_goes_out(strip, "", &["car"], &at(0.6), "q2");
        assert!(sent, "a car further down still notifies");
        assert_eq!(why.as_deref(), Some("sent"));
        let (sent, _) = alert_goes_out("", "", &["car"], &at(0.05), "q3");
        assert!(sent, "no areas: as before");
        let (sent, why) = alert_goes_out("", "alert_labels = [\"person\"]", &["car"], &at(0.6), "q4");
        assert!(!sent, "only people notify");
        assert_eq!(why.as_deref(), Some("notify_objects"));
    }

    #[test]
    fn doorbell_sound_and_package_always_notify_and_the_rest_stays_filtered() {
        if !have_curl() {
            return;
        }
        let person_only = "alert_labels = [\"person\"]";
        let strip = "alert_ignore_polys = [{ points = [[0.0, 0.0], [1.0, 0.0], [1.0, 1.0], [0.0, 1.0]] }]";
        let review = serde_json::json!({"wanted": ["car"], "min_score": 0.5, "alert": {"sent": true, "why": "sent"},
            "checks": [{"seen": [{"label": "car", "score": 0.9, "x": 0.4, "y": 0.4, "w": 0.2, "h": 0.1}], "parked": [false]}]});
        for (i, objects) in [&["visitor"][..], &["glass", "sound"], &["package"], &["package picked up"], &["car", "visitor"]].iter().enumerate() {
            let (sent, why) = alert_goes_out("", person_only, objects, &review, &format!("a{i}"));
            assert!(sent, "{objects:?} on a people-only camera");
            assert_eq!(why.as_deref(), Some("sent"));
            let (sent, _) = alert_goes_out(strip, "", objects, &review, &format!("b{i}"));
            assert!(sent, "{objects:?} in an area that does not notify");
        }
        let (sent, _) = alert_goes_out(strip, "", &["car"], &review, "b-car");
        assert!(!sent, "a car in the area stays quiet");
    }

    /// As `alert_goes_out`, on a people-only camera with or without AI.
    fn alert_with_ai(ai_on: bool, objects: &[&str], eid: &str) -> (bool, Option<String>) {
        let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        l.set_nonblocking(true).unwrap();
        let port = l.local_addr().unwrap().port();
        let ai = if ai_on { "[ai]\nenabled = true\nurl = \"http://127.0.0.1:9/detect\"\n" } else { "[ai]\nenabled = false\n" };
        let cfg: crate::config::Config = toml::from_str(&format!(
            "{ai}[alerts]\nwebhook_url = \"http://127.0.0.1:{port}/hook\"\n[[cameras]]\nname = \"Street\"\nurl = \"rtsp://x/a\"\nalert_labels = [\"person\"]\n"
        ))
        .unwrap();
        let e = Engine::for_test(cfg);
        let ev = Event { protect: false, id: eid.into(), camera_id: 0, camera: "Street".into(), start: Utc::now().timestamp(), end: Utc::now().timestamp() + 5, score: 1.0, clip: None, thumb: None, objects: objects.iter().map(|o| o.to_string()).collect() };
        e.db.save_ai_review(eid, "Street", ev.start, true, r#"{"alert":{"sent":true,"why":"sent"}}"#).unwrap();
        e.notify_event(&ev, None, None);
        let t0 = std::time::Instant::now();
        let got = loop {
            if l.accept().is_ok() {
                break true;
            }
            if t0.elapsed() > std::time::Duration::from_secs(6) {
                break false;
            }
            std::thread::sleep(std::time::Duration::from_millis(50));
        };
        let why = e.db.ai_review(eid).unwrap().and_then(|d| serde_json::from_str::<Value>(&d).ok()).and_then(|v| v["alert"]["why"].as_str().map(String::from));
        (got, why)
    }

    #[test]
    fn notify_only_labels_do_not_silence_a_camera_with_ai_off() {
        if !have_curl() {
            return;
        }
        assert!(alert_with_ai(false, &[], "off1").0, "AI off: no object labels at all");
        let (sent, why) = alert_with_ai(true, &[], "on1");
        assert!(!sent, "AI on and nothing found stays silent");
        assert_eq!(why.as_deref(), Some("notify_objects"));
        assert!(!alert_with_ai(false, &["dog"], "off2").0, "the camera itself said dog");
        assert!(alert_with_ai(true, &["person"], "on2").0);
    }

    #[test]
    fn what_held_an_alert_back_is_recorded_and_not_counted_as_sent() {
        let cfg = |extra: &str, alerts: &str| -> crate::config::Config {
            toml::from_str(&format!("[alerts]\nwebhook_url = \"http://127.0.0.1:9/hook\"\n{alerts}\n[[cameras]]\nname = \"Street\"\nurl = \"rtsp://x/a\"\n{extra}\n")).unwrap()
        };
        let now = Utc::now().timestamp();
        let ev = |id: &str| Event { protect: false, id: id.into(), camera_id: 0, camera: "Street".into(), start: now, end: now + 5, score: 1.0, clip: None, thumb: None, objects: vec!["person".into()] };
        let go = |e: &Engine, id: &str| {
            e.db.save_ai_review(id, "Street", now, true, r#"{"alert":{"sent":true,"why":"sent"}}"#).unwrap();
            e.notify_event(&ev(id), None, None);
            let d = e.db.ai_review(id).unwrap().unwrap();
            serde_json::from_str::<Value>(&d).unwrap()["alert"]["why"].as_str().unwrap().to_string()
        };
        let sent = |e: &Engine| e.db.sent_reviews(0).unwrap().iter().map(|r| r.0.clone()).collect::<Vec<_>>();

        let e = Engine::for_test(cfg("", "min_interval = 3600"));
        assert_eq!(go(&e, "first"), "sent");
        assert_eq!(go(&e, "second"), "alert_interval");
        assert_eq!(sent(&e), ["first"]);

        let e = Engine::for_test(cfg("", "min_interval = 0"));
        e.pause_alerts("Street", Some(10));
        assert_eq!(go(&e, "paused"), "paused");
        assert!(sent(&e).is_empty());

        // A window that is not now: two to three hours from now.
        let t = |h: i64| (chrono::Local::now() + chrono::Duration::hours(h)).format("%H:%M").to_string();
        let e = Engine::for_test(cfg(&format!("alert_schedule = \"{}-{}\"", t(2), t(3)), "min_interval = 0"));
        assert_eq!(go(&e, "window"), "alert_window");
        assert!(sent(&e).is_empty());
    }
    #[test]
    fn held_back_by_spots_counts_the_same_both_ways() {
        let e = Engine::for_test(toml::from_str::<crate::config::Config>("").unwrap());
        let review = |kind: &str| serde_json::json!({ "wanted": ["person"], "min_score": 0.5, "confirmed": false, "alert": { "sent": false },
            "checks": [{ "n": 0, "seen": [{ "label": "person", "score": 0.62 }], "parked": [true], "spot": [kind] }] }).to_string();
        e.db.save_ai_review("a", "Yard", 100, false, &review("remembered")).unwrap();
        e.db.save_ai_review("b", "Yard", 200, false, &review("marked")).unwrap();
        e.db.save_ai_review("c", "Yard", 50, false, &review("remembered")).unwrap();
        e.db.save_ai_review("d", "Gate", 100, false, &review("remembered")).unwrap();
        e.db.save_ai_review("f", "Yard", 300, false, &serde_json::json!({ "wanted": ["person"], "min_score": 0.5, "alert": { "sent": false }, "checks": [] }).to_string()).unwrap();
        assert!(e.db.review_sums_ready());
        assert_eq!(e.held_back_by_spots("Yard", 60), 2);
        // The slow way (the verdicts not ready yet) gives the same.
        e.db.as_if_restarted();
        assert!(!e.db.review_sums_ready());
        assert_eq!(e.held_back_by_spots("Yard", 60), 2);
    }
}
