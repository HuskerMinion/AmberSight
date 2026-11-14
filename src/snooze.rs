//! Per-account snooze: `POST /api/me/snooze` mutes one account's push
//! notifications (browser and app) for one camera for a while. Nothing else
//! changes: the camera records, detects and sends to every other account and
//! to the server-wide channels (ntfy, Pushover, email...), unlike the
//! server-wide pause (`POST /api/cameras/:id/pause`). Kept in the database, so
//! a restart keeps it; expired rows are pruned whenever snoozes are read or set.

use axum::{
    extract::State,
    http::StatusCode,
    response::{IntoResponse, Json, Response},
};
use std::sync::Arc;

use crate::engine::Engine;
use crate::web::{Access, Caller};

/// The longest snooze: a day.
pub const MAX_MINUTES: u32 = 24 * 60;

#[derive(serde::Deserialize)]
pub struct SnoozeReq {
    camera: String,
    minutes: u32,
}

fn now_ms() -> i64 {
    chrono::Utc::now().timestamp_millis()
}

/// The configured name of the camera `asked` names (by name or slug), if the
/// caller may see it.
fn visible_camera(engine: &Engine, access: &Access, asked: &str) -> Option<String> {
    let want = crate::config::slug(asked.trim());
    if want.is_empty() {
        return None;
    }
    let cfg = engine.config.lock().unwrap();
    cfg.cameras.iter().map(|c| c.name.clone()).find(|n| crate::config::slug(n) == want).filter(|n| access.can_name(n))
}

/// When a snooze of `minutes` from `now_ms` ends; None: 0 (clears it).
fn until(now_ms: i64, minutes: u32) -> Option<i64> {
    (minutes > 0).then(|| now_ms + i64::from(minutes) * 60_000)
}

/// `POST /api/me/snooze {"camera", "minutes"}`: 1 to 1440 minutes, 0 clears.
/// Answers `{"camera", "until_ms"}` (`until_ms` null when cleared). A camera
/// the caller cannot see is 404, as if it did not exist.
pub async fn set(State(engine): State<Arc<Engine>>, axum::Extension(caller): axum::Extension<Caller>, access: Option<axum::Extension<Access>>, Json(req): Json<SnoozeReq>) -> Response {
    let access = access.map(|a| a.0).unwrap_or_default();
    if req.minutes > MAX_MINUTES {
        return (StatusCode::BAD_REQUEST, format!("minutes must be 0 (clear) to {MAX_MINUTES}")).into_response();
    }
    let Some(name) = visible_camera(&engine, &access, &req.camera) else {
        return (StatusCode::NOT_FOUND, "no such camera").into_response();
    };
    let now = now_ms();
    let slug = crate::config::slug(&name);
    let end = until(now, req.minutes);
    let e = engine.clone();
    let user = caller.0.clone();
    let done = tokio::task::spawn_blocking(move || -> anyhow::Result<()> {
        let _ = e.db.snooze_prune(now);
        match end {
            Some(t) => e.db.snooze_set(&user, &slug, t),
            None => e.db.snooze_clear(&user, &slug).map(|_| ()),
        }
    })
    .await;
    match done {
        Ok(Ok(())) => Json(serde_json::json!({ "camera": name, "until_ms": end })).into_response(),
        Ok(Err(e)) => (StatusCode::INTERNAL_SERVER_ERROR, format!("could not save the snooze: {e:#}")).into_response(),
        Err(_) => (StatusCode::INTERNAL_SERVER_ERROR, "could not save the snooze").into_response(),
    }
}

/// `GET /api/me/snooze` → `{"snoozed": [{"camera", "until_ms"}]}`: the caller's
/// snoozes still running, of cameras it may see, soonest end first.
pub async fn list(State(engine): State<Arc<Engine>>, axum::Extension(caller): axum::Extension<Caller>, access: Option<axum::Extension<Access>>) -> Response {
    let access = access.map(|a| a.0).unwrap_or_default();
    let now = now_ms();
    let e = engine.clone();
    let user = caller.0.clone();
    let rows = tokio::task::spawn_blocking(move || {
        let _ = e.db.snooze_prune(now);
        e.db.snooze_list(&user, now)
    })
    .await;
    let rows = match rows {
        Ok(Ok(r)) => r,
        _ => return (StatusCode::INTERNAL_SERVER_ERROR, "could not read the snoozes").into_response(),
    };
    let names: Vec<String> = engine.config.lock().unwrap().cameras.iter().map(|c| c.name.clone()).collect();
    let snoozed: Vec<serde_json::Value> = rows
        .into_iter()
        .filter_map(|r| names.iter().find(|n| crate::config::slug(n) == r.camera && access.can_name(n)).map(|n| serde_json::json!({ "camera": n, "until_ms": r.until_ms })))
        .collect();
    Json(serde_json::json!({ "snoozed": snoozed })).into_response()
}

/// The accounts that have snoozed the camera named `camera` now (their
/// devices get no push notification about it).
pub fn snoozed_users(engine: &Engine, camera: &str) -> std::collections::HashSet<String> {
    engine.db.snoozed_users(&crate::config::slug(camera), now_ms()).unwrap_or_else(|e| {
        tracing::warn!("could not read snoozes (notifications are sent): {e:#}");
        Default::default()
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn snooze_lengths() {
        assert_eq!(until(1000, 0), None);
        assert_eq!(until(1000, 1), Some(61_000));
        assert_eq!(until(0, MAX_MINUTES), Some(86_400_000));
    }
}
