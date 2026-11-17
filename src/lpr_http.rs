//! License plate API: readings, the plates seen, the library of known
//! plates, the models. Changes and the model settings are admin only (the
//! web middleware refuses other writes); readings are shown to everyone
//! signed in, for the cameras their account may see.

use crate::engine::Engine;
use crate::web::Access;
use axum::extract::{Path as AxPath, Query, State};
use axum::http::{header, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::Json;
use std::sync::Arc;

fn err(code: StatusCode, msg: impl Into<String>) -> Response {
    (code, Json(serde_json::json!({ "error": msg.into() }))).into_response()
}

async fn blocking<T: Send + 'static>(f: impl FnOnce() -> anyhow::Result<T> + Send + 'static) -> Result<T, Response> {
    match tokio::task::spawn_blocking(f).await {
        Ok(Ok(v)) => Ok(v),
        Ok(Err(e)) => Err(err(StatusCode::INTERNAL_SERVER_ERROR, format!("{e:#}"))),
        Err(_) => Err(err(StatusCode::INTERNAL_SERVER_ERROR, "plates error")),
    }
}

pub async fn status(State(engine): State<Arc<Engine>>) -> Json<serde_json::Value> {
    Json(engine.lpr_status())
}

pub async fn download_models(State(engine): State<Arc<Engine>>) -> Json<serde_json::Value> {
    Json(serde_json::json!({ "started": engine.start_lpr_download() }))
}

#[derive(serde::Deserialize, Default)]
pub struct ListQ {
    /// One event's readings.
    event: Option<String>,
    /// A camera (its name).
    camera: Option<String>,
    /// Part of a plate or of a known plate's name.
    q: Option<String>,
    /// "1": only known plates; "0": only unknown ones.
    known: Option<String>,
    /// Epoch milliseconds.
    since: Option<i64>,
    until: Option<i64>,
    limit: Option<usize>,
    offset: Option<usize>,
}

/// The filter for a caller: the cameras they may see.
fn filter_for(engine: &Engine, access: &Access, q: ListQ, default_limit: usize) -> crate::db::PlateFilter {
    let cameras = access.limited().then(|| {
        engine.cameras.read().unwrap().iter().flatten().map(|h| crate::config::slug(&h.cfg.read().unwrap().name)).filter(|s| access.can_slug(s)).collect()
    });
    crate::db::PlateFilter {
        event: q.event.filter(|e| !e.is_empty()),
        cameras,
        camera: q.camera.filter(|c| !c.is_empty()).map(|c| crate::config::slug(&c)),
        q: q.q,
        known: match q.known.as_deref() {
            Some("1") | Some("true") => Some(true),
            Some("0") | Some("false") => Some(false),
            _ => None,
        },
        since_ms: q.since,
        until_ms: q.until,
        limit: q.limit.unwrap_or(default_limit).clamp(1, 1000),
        offset: q.offset.unwrap_or(0),
    }
}

/// Camera slug -> name, for showing readings.
fn camera_names(engine: &Engine) -> std::collections::HashMap<String, String> {
    engine.cameras.read().unwrap().iter().flatten().map(|h| {
        let n = h.cfg.read().unwrap().name.clone();
        (crate::config::slug(&n), n)
    }).collect()
}

fn with_names(engine: &Engine, rows: Vec<serde_json::Value>) -> Vec<serde_json::Value> {
    let names = camera_names(engine);
    rows.into_iter()
        .map(|mut r| {
            if let Some(n) = r["camera"].as_str().and_then(|s| names.get(s)) {
                r["camera_name"] = serde_json::json!(n);
            }
            if let Some(cams) = r["cameras"].as_array() {
                let list: Vec<String> = cams.iter().filter_map(|c| c.as_str()).map(|s| names.get(s).cloned().unwrap_or_else(|| s.to_string())).collect();
                r["camera_names"] = serde_json::json!(list);
            }
            r
        })
        .collect()
}

/// `GET /api/plates`: readings, newest first.
pub async fn list(State(engine): State<Arc<Engine>>, axum::Extension(access): axum::Extension<Access>, Query(q): Query<ListQ>) -> Response {
    let f = filter_for(&engine, &access, q, 100);
    let event = f.event.clone();
    match blocking(move || {
        let rows = with_names(&engine, engine.db.list_plates(&f)?);
        // For one event: whether a known / an unknown plate was read in it.
        // (From the rows the caller may see.)
        let flags = event.map(|_| (rows.iter().any(|r| r["known"] == true), rows.iter().any(|r| r["known"] == false)));
        Ok((rows, flags))
    })
    .await
    {
        Ok((v, flags)) => {
            let mut out = serde_json::json!({ "plates": v });
            if let Some((k, u)) = flags {
                out["known_plate"] = k.into();
                out["unknown_plate"] = u.into();
            }
            Json(out).into_response()
        }
        Err(r) => r,
    }
}

/// `GET /api/plates/recent`: one line per plate seen.
pub async fn recent(State(engine): State<Arc<Engine>>, axum::Extension(access): axum::Extension<Access>, Query(q): Query<ListQ>) -> Response {
    let f = filter_for(&engine, &access, q, 100);
    match blocking(move || Ok(with_names(&engine, engine.db.recent_plates(&f)?))).await {
        Ok(v) => Json(serde_json::json!({ "plates": v })).into_response(),
        Err(r) => r,
    }
}

pub async fn img(State(engine): State<Arc<Engine>>, axum::Extension(access): axum::Extension<Access>, AxPath(id): AxPath<i64>) -> Response {
    match blocking(move || engine.db.plate_img(id)).await {
        // A camera the caller may not see answers as if it does not exist.
        Ok(Some((cam, jpeg))) if access.can_slug(&cam) => Response::builder()
            .header(header::CONTENT_TYPE, "image/jpeg")
            .header(header::CACHE_CONTROL, "private, max-age=3600")
            .body(axum::body::Body::from(jpeg))
            .unwrap(),
        Ok(_) => err(StatusCode::NOT_FOUND, "no such reading"),
        Err(r) => r,
    }
}

pub async fn delete(State(engine): State<Arc<Engine>>, AxPath(id): AxPath<i64>) -> Response {
    match blocking(move || engine.db.delete_plate(id)).await {
        Ok(true) => Json(serde_json::json!({ "deleted": true })).into_response(),
        Ok(false) => err(StatusCode::NOT_FOUND, "no such reading"),
        Err(r) => r,
    }
}

/// `GET /api/plates/known`: the library.
pub async fn known(State(engine): State<Arc<Engine>>) -> Response {
    match blocking(move || engine.db.known_plates()).await {
        Ok(v) => Json(serde_json::json!({ "known": v })).into_response(),
        Err(r) => r,
    }
}

#[derive(serde::Deserialize)]
pub struct KnownReq {
    text: Option<String>,
    name: Option<String>,
    notes: Option<String>,
}

/// A plate's text as given by a person: normalized, 2 to 12 characters.
fn clean_text(t: &str) -> Result<String, &'static str> {
    let n = crate::lpr::normalize(t);
    if n.chars().count() < 2 || n.chars().count() > 12 {
        return Err("a plate is 2 to 12 letters and numbers");
    }
    Ok(n)
}

fn clean_name(n: &str) -> Result<String, &'static str> {
    let n = n.trim();
    if n.is_empty() || n.chars().count() > 60 {
        return Err("give a name of 1 to 60 characters");
    }
    Ok(n.to_string())
}

fn clean_notes(n: &str) -> Result<String, &'static str> {
    let n = n.trim();
    if n.chars().count() > 300 {
        return Err("notes can be 300 characters at most");
    }
    Ok(n.to_string())
}

/// `POST /api/plates/known {text, name, notes}`: name a plate.
pub async fn add_known(State(engine): State<Arc<Engine>>, Json(b): Json<KnownReq>) -> Response {
    let (text, name, notes) = match (clean_text(b.text.as_deref().unwrap_or("")), clean_name(b.name.as_deref().unwrap_or("")), clean_notes(b.notes.as_deref().unwrap_or(""))) {
        (Ok(t), Ok(n), Ok(o)) => (t, n, o),
        (Err(e), _, _) | (_, Err(e), _) | (_, _, Err(e)) => return err(StatusCode::BAD_REQUEST, e),
    };
    match blocking(move || engine.db.add_known_plate(&text, &name, &notes, chrono::Utc::now().timestamp_millis())).await {
        Ok(Some(id)) => Json(serde_json::json!({ "id": id })).into_response(),
        Ok(None) => err(StatusCode::CONFLICT, "that plate is already in the list"),
        Err(r) => r,
    }
}

/// `PUT /api/plates/known/:id {text?, name?, notes?}`.
pub async fn update_known(State(engine): State<Arc<Engine>>, AxPath(id): AxPath<i64>, Json(b): Json<KnownReq>) -> Response {
    let text = match b.text.as_deref().map(clean_text).transpose() {
        Ok(t) => t,
        Err(e) => return err(StatusCode::BAD_REQUEST, e),
    };
    let name = match b.name.as_deref().map(clean_name).transpose() {
        Ok(t) => t,
        Err(e) => return err(StatusCode::BAD_REQUEST, e),
    };
    let notes = match b.notes.as_deref().map(clean_notes).transpose() {
        Ok(t) => t,
        Err(e) => return err(StatusCode::BAD_REQUEST, e),
    };
    match blocking(move || engine.db.update_known_plate(id, text.as_deref(), name.as_deref(), notes.as_deref())).await {
        Ok(true) => Json(serde_json::json!({ "updated": true })).into_response(),
        Ok(false) => err(StatusCode::NOT_FOUND, "no such plate"),
        // The only way to fail is another entry with that text.
        Err(_) => err(StatusCode::CONFLICT, "that plate is already in the list"),
    }
}

pub async fn delete_known(State(engine): State<Arc<Engine>>, AxPath(id): AxPath<i64>) -> Response {
    match blocking(move || engine.db.delete_known_plate(id)).await {
        Ok(true) => Json(serde_json::json!({ "deleted": true })).into_response(),
        Ok(false) => err(StatusCode::NOT_FOUND, "no such plate"),
        Err(r) => r,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn typed_plates_names_and_notes_are_checked() {
        assert_eq!(clean_text(" abc-123 "), Ok("ABC123".to_string()));
        assert!(clean_text("A").is_err());
        assert!(clean_text("ABCDEFGHIJKLM").is_err());
        assert!(clean_text("--").is_err());
        assert_eq!(clean_name("  Mom's car "), Ok("Mom's car".to_string()));
        assert!(clean_name(" ").is_err());
        assert!(clean_name(&"x".repeat(61)).is_err());
        assert!(clean_notes(&"x".repeat(301)).is_err());
        assert_eq!(clean_notes(""), Ok(String::new()));
    }
}
