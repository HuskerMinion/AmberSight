//! Replays saved AI reviews through the real confirmation logic (the
//! parked memory and the remembered spots), in time order, for measuring
//! what they change. Not a test of its own: it runs only when asked to, with
//!
//!   AMBERSIGHT_SPOT_REPLAY=reviews.json AMBERSIGHT_SPOT_REPLAY_OUT=out.jsonl \
//!     cargo test replay_saved_reviews -- --ignored
//!
//! The input is a JSON list of `{event_id, camera, start, detail}` where
//! `detail` is a saved AI review. Each output line says, for one event, what
//! confirmed it (if anything) with the spots off and on. Set
//! AMBERSIGHT_SPOT_REPLAY_ONLY=person to replay as if every camera alerted
//! for people only.

use super::ai_check::Parked;
use super::spots::{self, Held, Spots};
use crate::ai::Detection;
use std::collections::HashMap;
use std::io::Write;

#[test]
#[ignore]
fn replay_saved_reviews() {
    let (Ok(input), Ok(out)) = (std::env::var("AMBERSIGHT_SPOT_REPLAY"), std::env::var("AMBERSIGHT_SPOT_REPLAY_OUT")) else {
        eprintln!("set AMBERSIGHT_SPOT_REPLAY and AMBERSIGHT_SPOT_REPLAY_OUT");
        return;
    };
    let only: Option<Vec<String>> = std::env::var("AMBERSIGHT_SPOT_REPLAY_ONLY").ok().map(|s| s.split(',').map(String::from).collect());
    let mut all: Vec<serde_json::Value> = serde_json::from_str(&std::fs::read_to_string(input).unwrap()).unwrap();
    all.sort_by_key(|r| (r["start"].as_i64().unwrap_or(0), r["event_id"].as_str().unwrap_or("").to_string()));
    let t0 = all.first().and_then(|r| r["start"].as_i64()).unwrap_or(0) * 1000;
    let base = std::time::Instant::now();
    let at = |ms: i64| base + std::time::Duration::from_millis((ms - t0).max(0) as u64);
    let mut parked: [HashMap<String, Parked>; 2] = [HashMap::new(), HashMap::new()];
    let mut spots: HashMap<String, Spots> = HashMap::new();
    let mut f = std::io::BufWriter::new(std::fs::File::create(out).unwrap());
    for r in &all {
        let d = &r["detail"];
        let cam = r["camera"].as_str().unwrap_or("").to_string();
        let start_ms = r["start"].as_i64().unwrap_or(0) * 1000;
        let wanted: Vec<String> = only.clone().unwrap_or_else(|| d["wanted"].as_array().into_iter().flatten().filter_map(|v| v.as_str().map(String::from)).collect());
        let groups: Vec<String> = wanted.iter().map(|w| spots::group(w).to_string()).collect();
        let min = d["min_score"].as_f64().unwrap_or(0.5) as f32;
        let cam_labels: Vec<String> = d["camera_labels"].as_array().into_iter().flatten().filter_map(|l| l.as_str().map(String::from)).collect();
        let camconf = cam_labels.iter().any(|l| wanted.contains(l));
        let checks: Vec<(i64, Vec<Detection>)> = d["checks"]
            .as_array()
            .into_iter()
            .flatten()
            .filter(|c| c["error"].is_null())
            .map(|c| (start_ms + c["offset_ms"].as_i64().unwrap_or(0), serde_json::from_value(c["seen"].clone()).unwrap_or_default()))
            .collect();
        let mut result = serde_json::Map::new();
        // 0: as before (parked memory only); 1: with the remembered spots.
        for (mode, name) in [(0usize, "before"), (1, "after")] {
            let ev = spots::next_event();
            let p = parked[mode].entry(cam.clone()).or_default();
            let mut conf: Option<Vec<Detection>> = None;
            let mut held: Vec<serde_json::Value> = Vec::new();
            let mut looks = 0;
            for (ms, seen) in &checks {
                looks += 1;
                let old: Vec<bool> = seen.iter().map(|x| p.seen(x, at(*ms)) && x.score >= min).collect();
                let sp: Vec<Option<Held>> = if mode == 1 {
                    spots.entry(cam.clone()).or_default().look(seen, min, &|g| groups.iter().any(|w| w == g), true, *ms, ev)
                } else {
                    vec![None; seen.len()]
                };
                let mut counted = Vec::new();
                for ((x, o), h) in seen.iter().zip(&old).zip(&sp) {
                    if !wanted.contains(&x.label) || x.score < min {
                        continue;
                    }
                    if *o || h.is_some() {
                        let why = if *o { "parked" } else if *h == Some(Held::Marked) { "marked" } else { "spot" };
                        held.push(serde_json::json!({ "d": x, "why": why }));
                    } else {
                        counted.push(x.clone());
                    }
                }
                if !counted.is_empty() {
                    conf = Some(counted);
                    break;
                }
            }
            let waited = false;
            // The camera's own detection: as before it confirms on its own;
            // after, an object label needs AI to have seen that kind too
            // (down to the review floor, held or not), unless AI had no look.
            let saw = |l: &str| checks.iter().take(looks).any(|(_, seen)| seen.iter().any(|d| d.w > 0.0 && d.score >= spots::FLOOR && spots::group(&d.label) == spots::group(l)));
            let cam_conf = cam_labels.iter().filter(|l| wanted.contains(l)).any(|l| {
                let object = matches!(spots::group(l), "person" | "vehicle" | "animal");
                mode == 0 || !object || looks == 0 || saw(l)
            });
            result.insert(name.into(), serde_json::json!({ "conf": conf, "held": held, "looks": looks, "waited": waited, "cam_conf": cam_conf }));
        }
        let line = serde_json::json!({
            "event_id": r["event_id"], "camera": cam, "start": r["start"], "camconf": camconf, "camera_labels": cam_labels,
            "checks": checks.len(), "recorded_confirmed": d["confirmed"], "alerted": r["alerted"],
            "before": result["before"], "after": result["after"],
        });
        writeln!(f, "{line}").unwrap();
    }
    let n: usize = spots.values().map(|s| s.spots.len()).sum();
    eprintln!("replayed {} events; {n} spots at the end", all.len());
}
