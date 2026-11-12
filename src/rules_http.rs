//! Web API for "Where did they go?" (crate::follow) and alert rules in plain
//! words (crate::alert_rules, engine/smart_rules.rs).

use axum::{
    extract::{Path as AxPath, Query, State},
    http::StatusCode,
    response::{IntoResponse, Json, Response},
};
use std::sync::Arc;

use crate::alert_rules::AlertRule;
use crate::engine::Engine;
use crate::follow;
use crate::web::Access;

fn err(code: StatusCode, msg: &str) -> Response {
    (code, Json(serde_json::json!({ "error": msg }))).into_response()
}

#[derive(serde::Deserialize)]
pub struct FollowQ {
    /// The event to start from.
    event: String,
    /// The person or vehicle in it (a picture id from `sources` / a search result); default: the largest.
    pic: Option<i64>,
    /// Seconds before and after the moment (default 600 each).
    before: Option<i64>,
    after: Option<i64>,
    /// "1": also sightings on the same camera.
    #[serde(default)]
    same_camera: String,
    /// Weakest score returned (default 0.60).
    min: Option<f32>,
    limit: Option<usize>,
}

/// `GET /api/smart/follow?event=ID[&pic=][&before=600][&after=600][&same_camera=1][&min=][&limit=]`:
/// the same person or vehicle on other cameras around that moment, as a
/// time-ordered `path` (the start included, `source: true`) with each
/// sighting's `score` (look-alike + boosts), `sim`, `face`/`plate` matches
/// and `pic` (GET /api/smart/pic/:pic). Approximate: see crate::follow.
pub async fn smart_follow(State(engine): State<Arc<Engine>>, axum::Extension(access): axum::Extension<Access>, Query(q): Query<FollowQ>) -> Response {
    if !engine.config.lock().unwrap().smart_search.enabled {
        return err(StatusCode::SERVICE_UNAVAILABLE, "smart search is off (Settings, Smart features, Smart search)");
    }
    let win = |v: Option<i64>| v.unwrap_or(follow::DEFAULT_WINDOW_S).clamp(0, follow::MAX_WINDOW_S) * 1000;
    let query = follow::Query {
        event_id: q.event,
        pic: q.pic,
        before_ms: win(q.before),
        after_ms: win(q.after),
        same_camera: q.same_camera == "1",
        floor: q.min.filter(|m| m.is_finite()).unwrap_or(follow::FLOOR).clamp(0.3, 1.0),
        limit: q.limit.unwrap_or(follow::MAX_RESULTS).clamp(1, 200),
    };
    let (before, after) = (query.before_ms / 1000, query.after_ms / 1000);
    let eng = engine.clone();
    let r = tokio::task::spawn_blocking(move || follow::follow(&eng.db, &query, &|cam| access.can_name(cam))).await;
    match r {
        Ok(Ok(o)) => Json(serde_json::json!({
            "source": o.source, "others": o.others, "path": o.path, "scanned": o.scanned,
            "before": before, "after": after, "floor": follow::FLOOR, "default_threshold": follow::DEFAULT_THRESHOLD,
        }))
        .into_response(),
        Ok(Err(follow::Error::NotFound)) => err(StatusCode::NOT_FOUND, "no such event, or it has no pictures searched yet"),
        Ok(Err(follow::Error::NoObject)) => err(StatusCode::UNPROCESSABLE_ENTITY, "no person or vehicle was found in this event's pictures"),
        Ok(Err(follow::Error::Db(e))) => {
            tracing::warn!("follow: {e}");
            err(StatusCode::INTERNAL_SERVER_ERROR, "could not look (details are in the server log)")
        }
        Err(_) => err(StatusCode::INTERNAL_SERVER_ERROR, "follow error"),
    }
}

/// `GET /api/alert-rules` (admin): the rules and whether they can work now.
pub async fn list(State(engine): State<Arc<Engine>>) -> Json<serde_json::Value> {
    let st = engine.smart_status();
    Json(serde_json::json!({
        "rules": engine.alert_rules(),
        "smart_enabled": st["enabled"], "smart_loaded": st["loaded"], "supported": st["supported"],
        "default_threshold": crate::alert_rules::DEFAULT_THRESHOLD,
    }))
}

/// `POST /api/alert-rules` (admin): add a rule (no `id`) or change one
/// (with its `id`). Returns the saved rule.
pub async fn save(State(engine): State<Arc<Engine>>, Json(r): Json<AlertRule>) -> Response {
    match engine.save_alert_rule(r) {
        Ok(r) => Json(serde_json::json!({ "rule": r })).into_response(),
        Err(e) => err(StatusCode::BAD_REQUEST, &e),
    }
}

/// `DELETE /api/alert-rules/:id` (admin).
pub async fn remove(State(engine): State<Arc<Engine>>, AxPath(id): AxPath<String>) -> Response {
    match engine.delete_alert_rule(&id) {
        Ok(true) => Json(serde_json::json!({ "ok": true })).into_response(),
        Ok(false) => err(StatusCode::NOT_FOUND, "no such rule"),
        Err(e) => err(StatusCode::INTERNAL_SERVER_ERROR, &e),
    }
}

#[derive(serde::Deserialize)]
pub struct PreviewReq {
    text: String,
    #[serde(default)]
    cameras: Vec<String>,
    /// How far back to look (hours, default 72, at most 720).
    hours: Option<u32>,
}

/// `POST /api/alert-rules/preview {"text", "cameras": [], "hours": 72}`
/// (admin): the recent events that look most like the words, with their
/// scores (the same score a rule's threshold is compared with).
pub async fn preview(State(engine): State<Arc<Engine>>, Json(p): Json<PreviewReq>) -> Response {
    let r = tokio::task::spawn_blocking(move || engine.rule_preview(&p.text, &p.cameras, p.hours.unwrap_or(72), &|_| true)).await;
    match r {
        Ok(Ok(v)) => Json(v).into_response(),
        Ok(Err(e)) => err(StatusCode::SERVICE_UNAVAILABLE, &format!("{e:#}")),
        Err(_) => err(StatusCode::INTERNAL_SERVER_ERROR, "preview error"),
    }
}
