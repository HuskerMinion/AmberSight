//! Morning summary API: a preview, "send now", and the summaries kept (the
//! page a summary notification opens). Preview and send are admin only;
//! kept summaries are shown to everyone signed in, cut down to the cameras
//! their account may see.

use crate::digest::Digest;
use crate::engine::Engine;
use crate::web::Access;
use axum::extract::{Path as AxPath, Query, State};
use axum::http::StatusCode;
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
        Err(_) => Err(err(StatusCode::INTERNAL_SERVER_ERROR, "summary error")),
    }
}

/// A summary with its words for the page (title, short text).
fn shown(d: &Digest, id: Option<i64>) -> serde_json::Value {
    serde_json::json!({
        "id": id,
        "title": crate::digest::title(d),
        "period_name": crate::digest::period_name(&d.period),
        "range": crate::digest::range_text(d.from_ms, d.to_ms),
        "digest": d,
    })
}

/// `GET /api/digest/preview`: the summary as it would be built now.
pub async fn preview(State(engine): State<Arc<Engine>>) -> Response {
    match blocking(move || {
        let cfg = engine.config.lock().unwrap().digest.clone();
        Ok(engine.build_digest(&cfg, chrono::Local::now()))
    })
    .await
    {
        Ok(d) => Json(shown(&d, None)).into_response(),
        Err(r) => r,
    }
}

/// `POST /api/digest/send`: build the summary now and send it through the
/// chosen services (works while the morning summary is off).
pub async fn send_now(State(engine): State<Arc<Engine>>) -> Response {
    match blocking(move || {
        let cfg = engine.config.lock().unwrap().digest.clone();
        let d = engine.build_digest(&cfg, chrono::Local::now());
        let (sent, id) = engine.deliver_digest(&d, false);
        Ok((d, sent, id))
    })
    .await
    {
        Ok((d, sent, id)) => {
            let sent: Vec<serde_json::Value> = sent.iter().map(|(c, ok)| serde_json::json!({ "service": c, "ok": ok })).collect();
            let mut out = shown(&d, id);
            out["sent"] = sent.into();
            Json(out).into_response()
        }
        Err(r) => r,
    }
}

/// `GET /api/digests`: the summaries kept, newest first.
pub async fn list(State(engine): State<Arc<Engine>>) -> Response {
    match blocking(move || engine.db.digest_list(60)).await {
        Ok(rows) => {
            let v: Vec<serde_json::Value> = rows
                .into_iter()
                .map(|(id, created_ms, from_ms, to_ms, auto)| serde_json::json!({ "id": id, "created_ms": created_ms, "from_ms": from_ms, "to_ms": to_ms, "auto": auto, "range": crate::digest::range_text(from_ms, to_ms) }))
                .collect();
            Json(serde_json::json!({ "digests": v })).into_response()
        }
        Err(r) => r,
    }
}

/// Cut a summary down to the cameras the caller may see.
fn for_caller(mut d: Digest, access: &Access) -> Digest {
    if access.limited() {
        d.cameras.retain(|c| access.can_slug(&crate::config::slug(&c.camera)));
        d.overview = crate::digest::overview_of(&d.cameras);
        d.notes.clear();
    }
    d
}

/// `quiet=1`: none is 204 instead of 404 (the page's own request: no console error).
#[derive(serde::Deserialize, Default)]
pub struct GetQuery {
    #[serde(default)]
    quiet: String,
}

/// `GET /api/digests/:id` (`latest` for the newest).
pub async fn get(State(engine): State<Arc<Engine>>, axum::Extension(access): axum::Extension<Access>, AxPath(id): AxPath<String>, Query(q): Query<GetQuery>) -> Response {
    match blocking(move || {
        let id = if id == "latest" { engine.db.digest_latest_id()? } else { id.parse::<i64>().ok() };
        let Some(id) = id else { return Ok(None) };
        Ok(engine.db.digest_get(id)?.and_then(|b| serde_json::from_str::<Digest>(&b).ok()).map(|d| (id, d)))
    })
    .await
    {
        Ok(Some((id, d))) => {
            let d = for_caller(d, &access);
            Json(shown(&d, Some(id))).into_response()
        }
        Ok(None) if q.quiet == "1" => StatusCode::NO_CONTENT.into_response(),
        Ok(None) => err(StatusCode::NOT_FOUND, "no such summary"),
        Err(r) => r,
    }
}
