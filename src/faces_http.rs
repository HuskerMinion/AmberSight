//! Settings > Smart features > Faces API: the faces seen, naming them, people, the models.
//! Admin only (checked in the web middleware for /faces and /people).

use crate::engine::Engine;
use axum::extract::{Path as AxPath, Query, State};
use axum::http::{header, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::Json;
use std::sync::Arc;

/// Unnamed faces at least this alike to one being named are named too
/// (when asked): a little stricter than matching.
const LOOK_ALIKE: f32 = 0.55;

fn err(code: StatusCode, msg: impl Into<String>) -> Response {
    (code, Json(serde_json::json!({ "error": msg.into() }))).into_response()
}

async fn blocking<T: Send + 'static>(f: impl FnOnce() -> anyhow::Result<T> + Send + 'static) -> Result<T, Response> {
    match tokio::task::spawn_blocking(f).await {
        Ok(Ok(v)) => Ok(v),
        Ok(Err(e)) => Err(err(StatusCode::INTERNAL_SERVER_ERROR, format!("{e:#}"))),
        Err(_) => Err(err(StatusCode::INTERNAL_SERVER_ERROR, "faces error")),
    }
}

pub async fn status(State(engine): State<Arc<Engine>>) -> Json<serde_json::Value> {
    Json(engine.face_status())
}

pub async fn download_models(State(engine): State<Arc<Engine>>) -> Json<serde_json::Value> {
    Json(serde_json::json!({ "started": engine.start_face_download() }))
}

#[derive(serde::Deserialize)]
pub struct ListQ {
    event: Option<String>,
    person: Option<i64>,
    /// "1" or "true": only faces nobody has named.
    #[serde(default)]
    unnamed: String,
    limit: Option<usize>,
}

pub async fn list(State(engine): State<Arc<Engine>>, Query(q): Query<ListQ>) -> Response {
    let limit = q.limit.unwrap_or(120).min(1000);
    let unnamed = q.unnamed == "1" || q.unnamed == "true";
    match blocking(move || engine.db.list_faces(q.event.as_deref(), q.person, unnamed, limit)).await {
        Ok(v) => Json(serde_json::json!({ "faces": v })).into_response(),
        Err(r) => r,
    }
}

pub async fn img(State(engine): State<Arc<Engine>>, AxPath(id): AxPath<i64>) -> Response {
    match blocking(move || engine.db.face_img(id)).await {
        Ok(Some((_, jpeg))) => Response::builder()
            .header(header::CONTENT_TYPE, "image/jpeg")
            // Not kept by the browser: a deleted face must not linger there.
            .header(header::CACHE_CONTROL, "no-store")
            .body(axum::body::Body::from(jpeg))
            .unwrap(),
        Ok(None) => err(StatusCode::NOT_FOUND, "no such face"),
        Err(r) => r,
    }
}

#[derive(serde::Deserialize)]
pub struct NameReq {
    /// The person's name; null or "" = not named.
    name: Option<String>,
    /// Also name the unnamed faces that look like this one.
    #[serde(default)]
    similar: bool,
}

pub async fn name(State(engine): State<Arc<Engine>>, AxPath(id): AxPath<i64>, Json(b): Json<NameReq>) -> Response {
    let name = b.name.map(|n| n.trim().to_string()).filter(|n| !n.is_empty());
    if name.as_ref().is_some_and(|n| n.chars().count() > 60) {
        return err(StatusCode::BAD_REQUEST, "that name is too long");
    }
    let r = blocking(move || -> anyhow::Result<serde_json::Value> {
        let Some(emb) = engine.db.face_emb(id)? else { anyhow::bail!("no such face") };
        let person = match &name {
            Some(n) => Some(engine.db.person_id(n, chrono::Utc::now().timestamp_millis())?),
            None => None,
        };
        let mut ids = vec![id];
        if b.similar && person.is_some() {
            let e = crate::faces::emb_from_bytes(&emb);
            for (fid, other) in engine.db.unnamed_face_embs(3000)? {
                if fid != id && crate::faces::cosine(&e, &crate::faces::emb_from_bytes(&other)) >= LOOK_ALIKE {
                    ids.push(fid);
                }
            }
        }
        let n = engine.db.label_faces(&ids, person)?;
        Ok(serde_json::json!({ "named": n, "person_id": person }))
    })
    .await;
    match r {
        Ok(v) => Json(v).into_response(),
        Err(r) => r,
    }
}

pub async fn delete(State(engine): State<Arc<Engine>>, AxPath(id): AxPath<i64>) -> Response {
    match blocking(move || engine.db.delete_face(id)).await {
        Ok(true) => Json(serde_json::json!({ "deleted": true })).into_response(),
        Ok(false) => err(StatusCode::NOT_FOUND, "no such face"),
        Err(r) => r,
    }
}

pub async fn people(State(engine): State<Arc<Engine>>) -> Response {
    match blocking(move || engine.db.people()).await {
        Ok(v) => Json(serde_json::json!({ "people": v })).into_response(),
        Err(r) => r,
    }
}

#[derive(serde::Deserialize)]
pub struct PersonReq {
    name: Option<String>,
    quiet: Option<bool>,
}

pub async fn update_person(State(engine): State<Arc<Engine>>, AxPath(id): AxPath<i64>, Json(b): Json<PersonReq>) -> Response {
    let name = b.name.map(|n| n.trim().to_string());
    if name.as_ref().is_some_and(|n| n.is_empty() || n.chars().count() > 60) {
        return err(StatusCode::BAD_REQUEST, "give a name of 1 to 60 characters");
    }
    match blocking(move || engine.db.update_person(id, name.as_deref(), b.quiet)).await {
        Ok(true) => Json(serde_json::json!({ "updated": true })).into_response(),
        Ok(false) => err(StatusCode::NOT_FOUND, "no such person"),
        Err(_) => err(StatusCode::CONFLICT, "someone already has that name"),
    }
}

pub async fn delete_person(State(engine): State<Arc<Engine>>, AxPath(id): AxPath<i64>) -> Response {
    match blocking(move || engine.db.delete_person(id)).await {
        Ok(true) => Json(serde_json::json!({ "deleted": true })).into_response(),
        Ok(false) => err(StatusCode::NOT_FOUND, "no such person"),
        Err(r) => r,
    }
}

#[derive(serde::Deserialize)]
pub struct ImportReq {
    path: String,
}

/// `POST /api/faces/import {path}`: name faces from a folder on this server
/// (one sub-folder per person).
pub async fn import(State(engine): State<Arc<Engine>>, Json(b): Json<ImportReq>) -> Response {
    let path = std::path::PathBuf::from(b.path.trim());
    if !path.is_dir() {
        return err(StatusCode::BAD_REQUEST, "that folder does not exist on the server");
    }
    // One import at a time.
    static BUSY: std::sync::Mutex<()> = std::sync::Mutex::new(());
    let r = blocking(move || {
        let Ok(_one) = BUSY.try_lock() else { anyhow::bail!("an import is already running") };
        engine.import_named_faces(&path)
    })
    .await;
    match r {
        Ok(r) => Json(r).into_response(),
        Err(r) => r,
    }
}
