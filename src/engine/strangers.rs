//! Stranger alerts: a camera that alerts only for unknown people and
//! vehicles. Known faces and known plates stay quiet (the event is still
//! recorded and labelled); an unknown face, an unknown plate, or someone
//! who could not be identified alerts. Optionally only the first time an
//! unknown face or plate shows up within some days.
//!
//! `decide` is pure: it gets what recognition found and answers whether to
//! alert and how the alert should read ("Unknown person at Front Door").

use super::*;
use crate::config::StrangerConfig;

/// What recognition found in one event.
#[derive(Debug, Default, Clone)]
pub struct Facts {
    /// AI saw a person; the most people in one picture.
    pub people_at_once: usize,
    /// AI saw a vehicle (its label: "car", "truck"...).
    pub vehicle: Option<String>,
    /// People recognized by name.
    pub known_faces: usize,
    /// Faces nobody has named; per face, whether it was seen before within
    /// the look-back (always false when not asked).
    pub unknown_faces: Vec<bool>,
    /// Plates read: (text, known, seen before within the look-back).
    pub plates: Vec<PlateFact>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct PlateFact {
    pub text: String,
    pub known: bool,
    pub seen_before: bool,
}

#[derive(Debug, PartialEq)]
pub enum Verdict {
    /// The filter does not apply: the alert goes as it would without it.
    Pass,
    /// Alert; the headline replaces "Person at Front Door".
    Alert { headline: String },
    /// Stay quiet. The reason is for the AI review ("known", "seen_before", "outside").
    Quiet { why: &'static str },
}

/// Words for the vehicle in a headline ("car", "truck"); anything else is "vehicle".
fn vehicle_word(label: &str) -> &str {
    match label {
        "car" | "truck" | "motorcycle" | "bus" | "van" => label,
        _ => "vehicle",
    }
}

/// Decide an event. `minute` is the local minute of the day.
pub fn decide(cfg: &StrangerConfig, camera: &str, f: &Facts, minute: u32) -> Verdict {
    if !cfg.active() {
        return Verdict::Pass;
    }
    let person = f.people_at_once > 0;
    if !person && f.vehicle.is_none() {
        return Verdict::Pass;
    }
    if let Some(w) = cfg.schedule.as_deref().and_then(crate::config::parse_schedule) {
        if !crate::config::schedule_active(Some(w), minute) {
            return if cfg.quiet_outside() { Verdict::Quiet { why: "outside" } } else { Verdict::Pass };
        }
    }
    let mut whats: Vec<String> = Vec::new();
    // Quiet for known or already-seen subjects; the last such reason wins.
    let mut why = "known";
    if person {
        // Whoever AI saw that no face accounts for could not be identified.
        let identified = f.known_faces + f.unknown_faces.len();
        let unidentified = f.people_at_once.saturating_sub(identified);
        let mut new_unknown = 0;
        for seen_before in &f.unknown_faces {
            if cfg.first_seen && *seen_before {
                why = "seen_before";
            } else {
                new_unknown += 1;
            }
        }
        if new_unknown > 0 || (unidentified > 0 && cfg.unidentified_is_unknown) {
            whats.push("person".to_string());
        }
    }
    if let Some(label) = &f.vehicle {
        let word = vehicle_word(label);
        if f.plates.is_empty() {
            if cfg.unidentified_is_unknown {
                whats.push(word.to_string());
            }
        } else {
            for p in f.plates.iter().filter(|p| !p.known) {
                if cfg.first_seen && p.seen_before {
                    why = "seen_before";
                } else {
                    whats.push(format!("{word} {}", p.text));
                }
            }
        }
    }
    if whats.is_empty() {
        return Verdict::Quiet { why };
    }
    // "Unknown person at Front Door", "Unknown car ABC123 in Driveway".
    let prep = if whats[0] == "person" || whats.len() > 1 && whats.iter().any(|w| w == "person") { "at" } else { "in" };
    Verdict::Alert { headline: format!("Unknown {} {prep} {camera}", whats.join(" and ")) }
}

impl Engine {
    /// Facts for an event, after faces and plates were looked at.
    /// `faces`/`plates` are None when that recognition did not run.
    pub(super) fn stranger_facts(
        &self,
        cfg: &StrangerConfig,
        event_id: &str,
        ai: &super::ai_check::AiOutcome,
        faces: Option<&super::faces::FaceResult>,
        plates: Option<&super::lpr::PlateResult>,
    ) -> Facts {
        let people_at_once = ai.log.iter().map(|c| c.seen.iter().filter(|d| d.label == "person" && d.score >= 0.5).count()).max().unwrap_or(0).max(usize::from(ai.labels.iter().any(|l| l == "person")));
        let vehicle = ai.labels.iter().find(|l| super::lpr::is_vehicle(l)).cloned();
        let since_ms = Utc::now().timestamp_millis() - i64::from(cfg.first_seen_days()) * 24 * 3600 * 1000;
        let mut plate_facts = Vec::new();
        if let Some(p) = plates {
            let past: Vec<(i64, String)> = if cfg.first_seen {
                self.db.plate_texts_since(since_ms, event_id).unwrap_or_default().into_iter().enumerate().map(|(i, t)| (i as i64, t)).collect()
            } else {
                Vec::new()
            };
            let (max_diff, confusions) = {
                let c = self.config.lock().unwrap();
                (c.lpr.max_diff as usize, c.lpr.confusions)
            };
            for h in &p.plates {
                let seen_before = !past.is_empty() && crate::lpr::best_known(&h.text, &past, max_diff, confusions).is_some();
                plate_facts.push(PlateFact { text: h.text.clone(), known: h.known.is_some(), seen_before });
            }
        }
        Facts {
            people_at_once,
            vehicle,
            known_faces: faces.map_or(0, |f| f.people.len()),
            unknown_faces: faces.map_or_else(Vec::new, |f| (0..f.unknown).map(|i| f.repeats.get(i).copied().unwrap_or(false)).collect()),
            plates: plate_facts,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg() -> StrangerConfig {
        StrangerConfig { enabled: true, ..Default::default() }
    }

    fn person(known: usize, unknown: &[bool], at_once: usize) -> Facts {
        Facts { people_at_once: at_once, known_faces: known, unknown_faces: unknown.to_vec(), ..Default::default() }
    }

    fn car(plates: &[(&str, bool, bool)]) -> Facts {
        Facts { vehicle: Some("car".into()), plates: plates.iter().map(|(t, k, s)| PlateFact { text: t.to_string(), known: *k, seen_before: *s }).collect(), ..Default::default() }
    }

    #[test]
    fn off_or_nothing_to_judge_passes() {
        assert_eq!(decide(&StrangerConfig::default(), "Door", &person(0, &[false], 1), 0), Verdict::Pass);
        assert_eq!(decide(&cfg(), "Door", &Facts::default(), 0), Verdict::Pass);
    }

    #[test]
    fn known_person_is_quiet_unknown_alerts() {
        assert_eq!(decide(&cfg(), "Front Door", &person(1, &[], 1), 0), Verdict::Quiet { why: "known" });
        assert_eq!(decide(&cfg(), "Front Door", &person(0, &[false], 1), 0), Verdict::Alert { headline: "Unknown person at Front Door".into() });
        // A known person with a stranger behind: alert.
        assert!(matches!(decide(&cfg(), "Front Door", &person(1, &[false], 2), 0), Verdict::Alert { .. }));
    }

    #[test]
    fn no_face_visible_follows_the_option() {
        let f = person(0, &[], 1);
        assert!(matches!(decide(&cfg(), "Door", &f, 0), Verdict::Alert { .. }));
        let mut c = cfg();
        c.unidentified_is_unknown = false;
        assert_eq!(decide(&c, "Door", &f, 0), Verdict::Quiet { why: "known" });
        // Two people in the picture, one known by face: the other is unidentified.
        assert!(matches!(decide(&cfg(), "Door", &person(1, &[], 2), 0), Verdict::Alert { .. }));
        assert_eq!(decide(&c, "Door", &person(1, &[], 2), 0), Verdict::Quiet { why: "known" });
    }

    #[test]
    fn plates_known_unknown_and_unread() {
        assert_eq!(decide(&cfg(), "Driveway", &car(&[("ABC123", true, false)]), 0), Verdict::Quiet { why: "known" });
        assert_eq!(decide(&cfg(), "Driveway", &car(&[("ABC123", false, false)]), 0), Verdict::Alert { headline: "Unknown car ABC123 in Driveway".into() });
        // One known, one unknown plate: the unknown one is named.
        assert_eq!(decide(&cfg(), "Driveway", &car(&[("AAA111", true, false), ("ZZZ999", false, false)]), 0), Verdict::Alert { headline: "Unknown car ZZZ999 in Driveway".into() });
        // No plate read.
        assert_eq!(decide(&cfg(), "Driveway", &car(&[]), 0), Verdict::Alert { headline: "Unknown car in Driveway".into() });
        let mut c = cfg();
        c.unidentified_is_unknown = false;
        assert_eq!(decide(&c, "Driveway", &car(&[]), 0), Verdict::Quiet { why: "known" });
    }

    #[test]
    fn person_and_car_make_one_headline() {
        let mut f = car(&[("ABC123", false, false)]);
        f.people_at_once = 1;
        f.unknown_faces = vec![false];
        assert_eq!(decide(&cfg(), "Driveway", &f, 0), Verdict::Alert { headline: "Unknown person and car ABC123 at Driveway".into() });
    }

    #[test]
    fn first_seen_stays_quiet_for_repeat_visitors() {
        let mut c = cfg();
        c.enabled = false;
        c.first_seen = true;
        assert_eq!(decide(&c, "Door", &person(0, &[true], 1), 0), Verdict::Quiet { why: "seen_before" });
        assert!(matches!(decide(&c, "Door", &person(0, &[false], 1), 0), Verdict::Alert { .. }));
        assert_eq!(decide(&c, "Drive", &car(&[("ABC123", false, true)]), 0), Verdict::Quiet { why: "seen_before" });
        assert!(matches!(decide(&c, "Drive", &car(&[("ABC123", false, false)]), 0), Verdict::Alert { .. }));
        // Known ones stay quiet too, and a repeat does not hide a new one.
        assert_eq!(decide(&c, "Door", &person(1, &[], 1), 0), Verdict::Quiet { why: "known" });
        assert!(matches!(decide(&c, "Door", &person(0, &[true, false], 2), 0), Verdict::Alert { .. }));
        // Without first_seen a repeat unknown still alerts.
        assert!(matches!(decide(&cfg(), "Door", &person(0, &[true], 1), 0), Verdict::Alert { .. }));
    }

    #[test]
    fn window_and_outside_behaviour() {
        let mut c = cfg();
        c.schedule = Some("22:00-06:00".into());
        let known = person(1, &[], 1);
        // Inside the window (23:00): the filter applies.
        assert_eq!(decide(&c, "Door", &known, 23 * 60), Verdict::Quiet { why: "known" });
        // Outside (12:00): alerts as usual by default...
        assert_eq!(decide(&c, "Door", &known, 12 * 60), Verdict::Pass);
        // ...or nothing for people and vehicles.
        c.outside = "quiet".into();
        assert_eq!(decide(&c, "Door", &person(0, &[false], 1), 12 * 60), Verdict::Quiet { why: "outside" });
        assert!(matches!(decide(&c, "Door", &person(0, &[false], 1), 23 * 60), Verdict::Alert { .. }));
    }
}
