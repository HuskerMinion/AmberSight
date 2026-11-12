//! AI review: what the AI saw in each event and why it did or did not alert
//! (`/api/events/:id/ai*`, `/api/ai/reviews`).

use axum::{
    body::Body,
    extract::{Path as AxPath, Query, State},
    http::{header, StatusCode},
    response::{IntoResponse, Json, Response},
};
use std::sync::Arc;

use crate::engine::Engine;

fn err(code: StatusCode, msg: &str) -> Response {
    (code, Json(serde_json::json!({ "error": msg }))).into_response()
}

fn good_id(id: &str) -> bool {
    !id.is_empty() && id.len() <= 200 && id.chars().all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
}

/// Which detections `best_of` looks at.
#[derive(Clone, Copy, PartialEq)]
enum Counted {
    /// Those that counted.
    Yes,
    /// Those that did not count because they sat still (parked).
    Parked,
    /// Those a remembered spot held back (see engine::spots).
    Spot,
}

/// Best detection of a label set in a review: (label, score, the spot's
/// reason: "remembered" or "marked").
fn best_of(detail: &serde_json::Value, pick: &dyn Fn(&str) -> bool, which: Counted) -> Option<(String, f64, Option<String>)> {
    let mut out: Option<(String, f64, Option<String>)> = None;
    for c in detail["checks"].as_array().into_iter().flatten() {
        for (i, d) in c["seen"].as_array().into_iter().flatten().enumerate() {
            let spot = c["spot"].get(i).and_then(|v| v.as_str());
            let was_parked = c["parked"].get(i).and_then(|v| v.as_bool()).unwrap_or(false);
            let kind = if spot.is_some() { Counted::Spot } else if was_parked { Counted::Parked } else { Counted::Yes };
            if kind != which {
                continue;
            }
            let (l, s) = (d["label"].as_str().unwrap_or(""), d["score"].as_f64().unwrap_or(0.0));
            if pick(l) && out.as_ref().is_none_or(|o| s > o.1) {
                out = Some((l.to_string(), s, spot.map(String::from)));
            }
        }
    }
    out
}

/// A one-line verdict for lists: kind + the objects behind it.
///   alerted      alert sent
///   near_miss    a wanted object was seen, but under the minimum score
///   strict       a wanted object was seen, but not in 2 of 3 looks while moving (stricter confirmation)
///   other_object only objects this camera does not alert for
///   nothing      nothing recognized
///   alerts_off   AI confirmed, but alerts were off for the camera
///   quiet_person only people marked quiet were recognized (Settings > Smart features > Faces)
///   known_only   the camera alerts only for strangers and everyone seen was known
///   quiet_rule   a quiet alert rule matched the pictures, so the alert was dropped
///   notify_area  everything seen stood in an area of the camera that never notifies
///   notify_objects  the camera notifies only for other objects ("Notify me for")
///   alerts_off   also when a pause, the alert window or the alert interval held it back
///   camera_only  the camera itself reported a person, vehicle or animal, and AI did
///                not see one (the camera's own detection needs AI to agree)
///   fixed_spot   what was seen stood in a spot where the same kind of thing had been
///                seen in several earlier events (or the owner marked it not real there)
pub fn summarize(detail: &serde_json::Value) -> serde_json::Value {
    let wanted: Vec<String> = detail["wanted"].as_array().into_iter().flatten().filter_map(|v| v.as_str().map(String::from)).collect();
    let min = detail["min_score"].as_f64().unwrap_or(0.5);
    let bw = best_of(detail, &|l| wanted.iter().any(|w| w == l), Counted::Yes);
    let bo = best_of(detail, &|l| !wanted.iter().any(|w| w == l), Counted::Yes);
    let bp = best_of(detail, &|l| wanted.iter().any(|w| w == l), Counted::Parked);
    let bs = best_of(detail, &|l| wanted.iter().any(|w| w == l), Counted::Spot).filter(|b| b.1 >= min);
    let sent = detail["alert"]["sent"].as_bool().unwrap_or(false);
    let confirmed = detail["confirmed"].as_bool().unwrap_or(false);
    let all_failed = detail["checks"].as_array().is_some_and(|a| !a.is_empty() && a.iter().all(|c| !c["error"].is_null()));
    let kind = if all_failed {
        "ai_failed"
    } else if sent {
        "alerted"
    } else if detail["alert"]["why"].as_str() == Some("quiet_people") {
        "quiet_person"
    } else if detail["alert"]["why"].as_str() == Some("known_only") {
        "known_only"
    } else if detail["alert"]["why"].as_str() == Some("quiet_rule") {
        "quiet_rule"
    } else if detail["alert"]["why"].as_str() == Some("notify_area") {
        "notify_area"
    } else if detail["alert"]["why"].as_str() == Some("notify_objects") {
        "notify_objects"
    } else if confirmed || matches!(detail["alert"]["why"].as_str(), Some("paused" | "alert_window" | "alert_interval")) {
        "alerts_off"
    } else if detail["camera_unconfirmed"].as_array().is_some_and(|a| !a.is_empty()) {
        "camera_only"
    } else if detail["strict"].as_bool() == Some(true) && bw.as_ref().is_some_and(|b| b.1 >= min) {
        "strict"
    } else if bs.is_some() && bw.as_ref().is_none_or(|b| b.1 < min) && bp.as_ref().is_none_or(|p| p.1 < min) {
        "fixed_spot"
    } else if (bp.is_some() || bs.is_some()) && bw.as_ref().is_none_or(|b| b.1 < min) {
        "parked"
    } else if bw.as_ref().is_some_and(|b| b.1 < min) {
        "near_miss"
    } else if bo.as_ref().is_some_and(|b| b.1 >= min) {
        "other_object"
    } else {
        "nothing"
    };
    let o = |b: Option<(String, f64, Option<String>)>| b.map(|(l, s, _)| serde_json::json!({ "label": l, "score": (s * 100.0).round() / 100.0 }));
    let spot = bs.map(|(l, s, why)| serde_json::json!({ "label": l, "score": (s * 100.0).round() / 100.0, "marked": why.as_deref() == Some("marked") }));
    serde_json::json!({
        "kind": kind,
        "best_wanted": o(bw),
        "best_other": o(bo),
        "best_parked": o(bp),
        "best_spot": spot,
        "checks": detail["checks"].as_array().map(|a| a.len()).unwrap_or(0),
        "camera_labels": detail["camera_labels"],
        "camera_unconfirmed": detail["camera_unconfirmed"],
    })
}

/// The camera's AI settings now, for comparing and for one-click changes.
fn camera_now(engine: &Engine, name: &str) -> serde_json::Value {
    let cfg = engine.config.lock().unwrap().clone();
    let id = engine.cameras.read().unwrap().iter().position(|h| h.as_ref().is_some_and(|h| h.cfg.read().unwrap().name == name));
    match cfg.cameras.iter().find(|c| c.name == name) {
        Some(c) => serde_json::json!({
            "id": id,
            "name": c.name,
            "ai": engine.ai_enabled(c),
            "labels": c.ai_labels.clone().unwrap_or_else(|| cfg.ai.labels.clone()),
            "labels_own": c.ai_labels.is_some(),
            "min_score": crate::config::f32_json(c.ai_min_score.unwrap_or(cfg.ai.min_score)),
            "min_score_own": c.ai_min_score.is_some(),
            "alerts": c.alerts,
            "confirm_camera": c.ai_confirm_camera == Some(true),
        }),
        None => serde_json::Value::Null,
    }
}

/// `GET /api/events/:id/ai`: the saved review plus today's settings.
pub async fn event_ai(State(engine): State<Arc<Engine>>, AxPath(id): AxPath<String>) -> Response {
    if !good_id(&id) {
        return err(StatusCode::BAD_REQUEST, "bad event id");
    }
    let e = engine.clone();
    let r = tokio::task::spawn_blocking(move || -> anyhow::Result<Option<serde_json::Value>> {
        let Some(ev) = e.db.get_event(&id)? else { return Ok(None) };
        let review: Option<serde_json::Value> = e.db.ai_review(&id)?.and_then(|s| serde_json::from_str(&s).ok());
        let ai = {
            let c = e.config.lock().unwrap();
            serde_json::json!({ "enabled": c.ai.enabled, "model": e.detector().is_some(), "server": c.ai.url.as_deref().is_some_and(|u| !u.is_empty()) })
        };
        Ok(Some(serde_json::json!({
            "event": { "id": ev.id, "camera": ev.camera, "start": ev.start, "end": ev.end, "objects": ev.objects, "thumb": ev.thumb.is_some() },
            "summary": review.as_ref().map(summarize),
            "review": review,
            "camera": camera_now(&e, &ev.camera),
            "ai": ai,
        })))
    })
    .await;
    match r {
        Ok(Ok(Some(v))) => Json(v).into_response(),
        Ok(Ok(None)) => err(StatusCode::NOT_FOUND, "no such event"),
        Ok(Err(e)) => {
            tracing::warn!("AI review: {e:#}");
            err(StatusCode::INTERNAL_SERVER_ERROR, "AI review failed (details are in the server log)")
        }
        Err(_) => err(StatusCode::INTERNAL_SERVER_ERROR, "failed"),
    }
}

/// `GET /api/events/:id/track?part=N`: what the AI sees through part N
/// (`part_ms` long, from 2 s before the event) of the event's recording, 3
/// frames a second, for drawing boxes on playback:
/// `{parts, part_ms, frames: [{t, seen: [...]}]}`.
#[derive(serde::Deserialize)]
pub struct TrackQ {
    #[serde(default)]
    part: u32,
}

pub async fn event_track(State(engine): State<Arc<Engine>>, AxPath(id): AxPath<String>, Query(q): Query<TrackQ>) -> Response {
    if !good_id(&id) {
        return err(StatusCode::BAD_REQUEST, "bad event id");
    }
    match tokio::task::spawn_blocking(move || engine.event_track(&id, q.part)).await {
        Ok(Ok(Some((parts, frames)))) => Json(serde_json::json!({
            "parts": parts,
            "part_ms": crate::engine::playback::TRACK_PART_MS,
            "frames": frames.into_iter().map(|(t, seen)| serde_json::json!({ "t": t, "seen": seen })).collect::<Vec<_>>(),
        }))
        .into_response(),
        Ok(Ok(None)) => err(StatusCode::NOT_FOUND, "no recording of that event"),
        Ok(Err(e)) if e.to_string() == "busy" => err(StatusCode::SERVICE_UNAVAILABLE, "busy, try again"),
        Ok(Err(e)) => {
            tracing::warn!("AI track: {e:#}");
            err(StatusCode::INTERNAL_SERVER_ERROR, "could not run the AI on this event")
        }
        Err(_) => err(StatusCode::INTERNAL_SERVER_ERROR, "failed"),
    }
}

/// `GET /api/events/:id/ai/:n`: a frame the AI checked.
pub async fn event_ai_frame(State(engine): State<Arc<Engine>>, AxPath((id, n)): AxPath<(String, u32)>) -> Response {
    if !good_id(&id) {
        return err(StatusCode::BAD_REQUEST, "bad event id");
    }
    match tokio::fs::read(engine.ai_review_dir(&id).join(format!("{n}.jpg"))).await {
        Ok(b) => ([(header::CONTENT_TYPE, "image/jpeg"), (header::CACHE_CONTROL, "private, max-age=86400")], Body::from(b)).into_response(),
        Err(_) => err(StatusCode::NOT_FOUND, "frame not kept"),
    }
}

/// `POST /api/events/:id/ai/recheck`: run today's AI on the saved frames
/// (after changing the model or settings). Nothing is saved.
pub async fn event_ai_recheck(State(engine): State<Arc<Engine>>, AxPath(id): AxPath<String>) -> Response {
    if !good_id(&id) {
        return err(StatusCode::BAD_REQUEST, "bad event id");
    }
    let e = engine.clone();
    let r = tokio::task::spawn_blocking(move || -> anyhow::Result<serde_json::Value> {
        let dir = e.ai_review_dir(&id);
        let mut frames: Vec<(u32, std::path::PathBuf)> = std::fs::read_dir(&dir)
            .map_err(|_| anyhow::anyhow!("no frames were kept for this event"))?
            .flatten()
            .filter_map(|f| {
                let p = f.path();
                let n = p.file_stem()?.to_str()?.parse::<u32>().ok()?;
                (p.extension()? == "jpg").then_some((n, p))
            })
            .collect();
        frames.sort();
        let mut checks = Vec::new();
        for (n, p) in frames.into_iter().take(30) {
            let jpeg = std::fs::read(&p)?;
            match e.ai_detect(&jpeg, 0.25) {
                Ok(seen) => checks.push(serde_json::json!({ "n": n, "seen": seen })),
                Err(x) => checks.push(serde_json::json!({ "n": n, "seen": [], "error": format!("{x:#}") })),
            }
        }
        Ok(serde_json::json!({ "checks": checks }))
    })
    .await;
    match r {
        Ok(Ok(v)) => Json(v).into_response(),
        Ok(Err(e)) => err(StatusCode::BAD_REQUEST, &format!("{e:#}")),
        Err(_) => err(StatusCode::INTERNAL_SERVER_ERROR, "failed"),
    }
}

/// Events of each camera that are looked through for the kinds (Almost, Parked, ...).
const PER_CAMERA: usize = 1000;

#[derive(serde::Deserialize)]
pub struct ReviewsQ {
    #[serde(default)]
    days: Option<i64>,
    #[serde(default)]
    camera: Option<String>,
    /// Only events with this verdict (`summary.kind`), or `noalert` (no alert
    /// sent), in the list: looked for in the whole period, not only the
    /// newest events.
    #[serde(default)]
    kind: Option<String>,
}

/// Events in the list of `/api/ai/reviews`.
const LIST_MAX: usize = 300;

/// `GET /api/ai/reviews?days=7&camera=&kind=`: recent AI-checked events with a
/// verdict each, and counts per camera.
pub async fn reviews(State(engine): State<Arc<Engine>>, Query(q): Query<ReviewsQ>) -> Response {
    let e = engine.clone();
    let r = tokio::task::spawn_blocking(move || -> anyhow::Result<serde_json::Value> {
        let days = q.days.unwrap_or(7).clamp(1, 90);
        let since = chrono::Utc::now().timestamp() - days * 86400;
        let cam = q.camera.as_deref().filter(|c| !c.is_empty());
        let kind = q.kind.as_deref().filter(|k| !k.is_empty() && *k != "all" && k.len() <= 40);
        let wanted = |alerted: bool, s: &serde_json::Value| match kind {
            None => true,
            Some("noalert") => !alerted,
            Some(k) => s["kind"].as_str() == Some(k),
        };
        // The page reads the one-line verdicts kept apart from the reviews (a few hundred bytes a
        // row, no JSON to read back). Until they are all filled in after an upgrade, the reviews.
        let fast = e.db.review_sums_ready();
        let rows: Vec<(String, String, i64, bool, serde_json::Value)> = if fast {
            e.db.ai_review_sums(since, cam, PER_CAMERA)?
                .into_iter()
                .filter_map(|(id, camera, start, alerted, sum)| Some((id, camera, start, alerted, serde_json::from_str(&sum).ok()?)))
                .collect()
        } else {
            e.db.ai_reviews(since, cam, PER_CAMERA)?
                .into_iter()
                .filter_map(|(id, camera, start, alerted, detail)| Some((id, camera, start, alerted, summarize(&serde_json::from_str(&detail).ok()?))))
                .collect()
        };
        let counts = if fast { e.db.ai_review_sum_counts(since, cam)? } else { e.db.ai_review_counts(since, cam)? };
        let mut per: std::collections::BTreeMap<String, serde_json::Map<String, serde_json::Value>> = Default::default();
        let mut list = Vec::new();
        // One kind asked for: from the whole period when the small table is there.
        let of_kind = match kind {
            Some(k) if fast => Some(e.db.ai_review_sums_of_kind(since, cam, k, LIST_MAX)?),
            _ => None,
        };
        for (id, camera, start, alerted, s) in rows {
            let k = s["kind"].as_str().unwrap_or("nothing").to_string();
            let c = per.entry(camera.clone()).or_default();
            *c.entry("total").or_insert(0.into()) = (c.get("total").and_then(|v| v.as_u64()).unwrap_or(0) + 1).into();
            *c.entry(k.clone()).or_insert(0.into()) = (c.get(&k).and_then(|v| v.as_u64()).unwrap_or(0) + 1).into();
            if of_kind.is_none() && list.len() < LIST_MAX && wanted(alerted, &s) {
                list.push(serde_json::json!({ "id": id, "camera": camera, "start": start, "alerted": alerted, "summary": s }));
            }
        }
        for (id, camera, start, alerted, sum) in of_kind.unwrap_or_default() {
            if let Ok(s) = serde_json::from_str::<serde_json::Value>(&sum) {
                list.push(serde_json::json!({ "id": id, "camera": camera, "start": start, "alerted": alerted, "summary": s }));
            }
        }
        // The true totals for the whole period; the other columns come from the
        // newest PER_CAMERA events of each camera (`sampled` says how many).
        for (cam, total, alerted) in counts {
            let c = per.entry(cam).or_default();
            let sampled = c.get("total").and_then(|v| v.as_u64()).unwrap_or(0);
            c.insert("sampled".into(), sampled.into());
            c.insert("total".into(), total.into());
            c.insert("alerted".into(), alerted.into());
        }
        let cams = engine_cameras(&e);
        Ok(serde_json::json!({
            "days": days,
            "per_camera_limit": PER_CAMERA,
            "list_limit": LIST_MAX,
            "kind": kind,
            "events": list,
            "cameras": per.into_iter().map(|(k, v)| {
                let mut o = serde_json::Value::Object(v);
                o["camera"] = k.clone().into();
                o["now"] = cams.get(&k).cloned().unwrap_or(serde_json::Value::Null);
                o
            }).collect::<Vec<_>>(),
        }))
    })
    .await;
    match r {
        Ok(Ok(v)) => Json(v).into_response(),
        Ok(Err(e)) => {
            tracing::warn!("AI review: {e:#}");
            err(StatusCode::INTERNAL_SERVER_ERROR, "AI review failed (details are in the server log)")
        }
        Err(_) => err(StatusCode::INTERNAL_SERVER_ERROR, "failed"),
    }
}

fn engine_cameras(e: &Engine) -> std::collections::HashMap<String, serde_json::Value> {
    let names: Vec<String> = e.config.lock().unwrap().cameras.iter().map(|c| c.name.clone()).collect();
    names.into_iter().map(|n| { let v = camera_now(e, &n); (n, v) }).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn review(wanted: &[&str], seen: &[(&str, f64)], sent: bool, confirmed: bool) -> serde_json::Value {
        serde_json::json!({
            "wanted": wanted, "min_score": 0.6, "confirmed": confirmed, "alert": { "sent": sent },
            "checks": [{ "n": 0, "seen": seen.iter().map(|(l, s)| serde_json::json!({ "label": l, "score": s })).collect::<Vec<_>>() }],
        })
    }

    #[test]
    fn verdicts() {
        assert_eq!(summarize(&review(&["person"], &[("person", 0.9)], true, true))["kind"], "alerted");
        assert_eq!(summarize(&review(&["person"], &[("person", 0.4)], false, false))["kind"], "near_miss");
        // Held back, also for a sound AI confirmed nothing in.
        for why in ["paused", "alert_window", "alert_interval"] {
            let mut r = review(&["person"], &[], false, false);
            r["alert"]["why"] = serde_json::json!(why);
            assert_eq!(summarize(&r)["kind"], "alerts_off", "{why}");
        }
        // Stricter confirmation turned a sure sighting down.
        let mut st = review(&["person"], &[("person", 0.9)], false, false);
        st["strict"] = serde_json::json!(true);
        assert_eq!(summarize(&st)["kind"], "strict");
        assert_eq!(summarize(&review(&["person"], &[("car", 0.8)], false, false))["kind"], "other_object");
        assert_eq!(summarize(&review(&["person"], &[("car", 0.3)], false, false))["kind"], "nothing");
        assert_eq!(summarize(&review(&["person"], &[("person", 0.9)], false, true))["kind"], "alerts_off");
        let failed = serde_json::json!({ "wanted": ["person"], "min_score": 0.5, "alert": { "sent": true },
            "checks": [{ "n": 0, "seen": [], "error": "AI server did not answer" }] });
        assert_eq!(summarize(&failed)["kind"], "ai_failed");
        let parked = serde_json::json!({ "wanted": ["car"], "min_score": 0.5, "confirmed": false, "alert": { "sent": false },
            "checks": [{ "n": 0, "seen": [{ "label": "car", "score": 0.9 }], "parked": [true] }] });
        assert_eq!(summarize(&parked)["kind"], "parked");
        assert_eq!(summarize(&parked)["best_parked"]["label"], "car");
        // Held back by a remembered spot (a shrub that looks like a person).
        let spot = serde_json::json!({ "wanted": ["person"], "min_score": 0.5, "confirmed": false, "alert": { "sent": false },
            "checks": [{ "n": 0, "seen": [{ "label": "person", "score": 0.62 }, { "label": "person", "score": 0.3 }], "parked": [true, false], "spot": ["remembered", null] }] });
        assert_eq!(summarize(&spot)["kind"], "fixed_spot");
        assert_eq!(summarize(&spot)["best_spot"]["marked"], false);
        let mut marked = spot.clone();
        marked["checks"][0]["spot"][0] = serde_json::json!("marked");
        assert_eq!(summarize(&marked)["best_spot"]["marked"], true);
        // A real one elsewhere in the picture: alerts as usual.
        let mut both = spot.clone();
        both["checks"][0]["seen"][1]["score"] = serde_json::json!(0.8);
        both["alert"]["sent"] = serde_json::json!(true);
        assert_eq!(summarize(&both)["kind"], "alerted");
        // The camera said "person", AI saw a car only.
        let mut cam = review(&["person", "car"], &[("car", 0.4)], false, false);
        cam["camera_unconfirmed"] = serde_json::json!(["person"]);
        assert_eq!(summarize(&cam)["kind"], "camera_only");
        let s = summarize(&review(&["person"], &[("person", 0.4), ("person", 0.55), ("car", 0.7)], false, false));
        assert_eq!(s["best_wanted"]["score"], 0.55);
        assert_eq!(s["best_other"]["label"], "car");
    }

    #[test]
    fn ids_are_checked() {
        assert!(good_id("front-door-1793540000"));
        assert!(!good_id("../etc"));
        assert!(!good_id(""));
    }
}
