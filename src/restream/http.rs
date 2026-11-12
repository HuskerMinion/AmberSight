//! Web API for Settings > Connections > Share streams (admin only):
//! `GET /api/restream` and `PUT /api/restream`.

use axum::{
    extract::State,
    http::StatusCode,
    response::{IntoResponse, Json, Response},
};
use std::sync::Arc;

use super::{refresh, server, RestreamConfig};
use crate::engine::Engine;

/// A privacy mask or text overlay is on: the shared video (passed through
/// as the camera sends it) shows the picture without them.
pub fn masked(cfg: &crate::config::CameraConfig) -> bool {
    cfg.overlay || !cfg.privacy_masks.is_empty()
}

/// Why a camera's stream is not served now, if it is not: a masked camera
/// waits for the admin's "share without the mask" confirmation.
pub fn blocker(cfg: &crate::config::CameraConfig) -> Option<&'static str> {
    if masked(cfg) && !cfg.share_unmasked {
        return Some("A privacy mask or text overlay is on: the shared stream would show the picture without it. Confirm below to share it anyway");
    }
    hard_blocker(cfg)
}

/// Why a camera can never be shared, if it cannot.
pub fn hard_blocker(cfg: &crate::config::CameraConfig) -> Option<&'static str> {
    if !crate::engine::reads_natively(cfg) {
        return Some("This camera is read by ffmpeg (not the built-in reader), which cannot be shared");
    }
    None
}

/// A camera with a sub stream that does not record reads its main stream
/// only to share it: its session has to restart when sharing starts or stops.
fn main_session_missing(h: &crate::engine::CameraHandle) -> bool {
    let c = h.running.read().unwrap();
    c.sub_url.as_deref().is_some_and(|u| !u.is_empty()) && !c.record
}

fn stream_json(hub: &crate::restream::hub::RestreamHub) -> serde_json::Value {
    let info = hub.info();
    serde_json::json!({
        "live": info.is_some(),
        "clients": hub.clients(),
        "codec": info.as_ref().map(|i| if i.video.h265 { "H.265" } else { "H.264" }),
        "width": info.as_ref().map(|i| i.video.width),
        "height": info.as_ref().map(|i| i.video.height),
        "audio": info.as_ref().map(|i| i.audio.is_some()).unwrap_or(false),
    })
}

fn status_json(engine: &Engine) -> serde_json::Value {
    let (rc, accounts) = {
        let c = engine.config.lock().unwrap();
        (c.restream.clone(), !crate::auth::accounts(&c).is_empty())
    };
    let st = server::status();
    let cams: Vec<_> = engine.cameras.read().unwrap().iter().flatten().cloned().collect();
    let cameras: Vec<serde_json::Value> = cams
        .iter()
        .map(|h| {
            let c = h.cfg.read().unwrap();
            let has_sub = c.sub_url.as_deref().is_some_and(|u| !u.is_empty());
            serde_json::json!({
                "id": h.id,
                "name": c.name,
                "slug": crate::config::slug(&c.name),
                "share": c.share,
                "has_sub": has_sub,
                "blocked": hard_blocker(&c),
                "masked": masked(&c),
                "unmasked_ok": c.share_unmasked,
                "waiting": blocker(&c),
                "main": stream_json(&h.restream_main),
                "sub": has_sub.then(|| stream_json(&h.restream_sub)),
            })
        })
        .collect();
    serde_json::json!({
        "enabled": rc.enabled,
        "port": rc.port,
        "listening": st.listening,
        "error": st.error,
        // Without accounts the streams need no sign-in.
        "auth": accounts,
        "cameras": cameras,
    })
}

/// `GET /api/restream`: the settings, whether the port is open, and every
/// camera's sharing state.
pub async fn get(State(engine): State<Arc<Engine>>) -> Json<serde_json::Value> {
    Json(status_json(&engine))
}

#[derive(serde::Deserialize)]
pub struct Update {
    enabled: Option<bool>,
    port: Option<u16>,
    /// Camera name -> shared or not. Cameras left out keep their setting.
    cameras: Option<std::collections::HashMap<String, bool>>,
    /// Camera name -> share without its privacy mask / overlay (confirmed).
    unmasked: Option<std::collections::HashMap<String, bool>>,
}

/// `PUT /api/restream {enabled?, port?, cameras?: {name: bool}, unmasked?: {name: bool}}`.
pub async fn put(State(engine): State<Arc<Engine>>, Json(u): Json<Update>) -> Response {
    let bad = |m: String| (StatusCode::BAD_REQUEST, Json(serde_json::json!({ "error": m }))).into_response();
    // Cameras whose session must start or stop reading the main stream.
    let mut restart: Vec<usize> = Vec::new();
    {
        let mut cfg = engine.config.lock().unwrap();
        let mut new = cfg.restream.clone();
        if let Some(e) = u.enabled {
            new.enabled = e;
        }
        if let Some(p) = u.port {
            new.port = p;
        }
        if let Err(e) = RestreamConfig::check_port(new.port, &cfg.server) {
            return bad(e);
        }
        for map in [&u.cameras, &u.unmasked].into_iter().flatten() {
            for name in map.keys() {
                if !cfg.cameras.iter().any(|c| c.name == *name) {
                    return bad(format!("no camera named '{name}'"));
                }
            }
        }
        if let Some(map) = &u.cameras {
            for c in cfg.cameras.iter_mut() {
                if let Some(v) = map.get(&c.name) {
                    c.share = *v;
                }
            }
        }
        if let Some(map) = &u.unmasked {
            for c in cfg.cameras.iter_mut() {
                if let Some(v) = map.get(&c.name) {
                    c.share_unmasked = *v;
                }
            }
        }
        let was_on = cfg.restream.enabled;
        cfg.restream = new;
        // The running cameras hold their own copy of their settings.
        for h in engine.cameras.read().unwrap().iter().flatten() {
            let (name, was_shared, was_served) = {
                let c = h.cfg.read().unwrap();
                (c.name.clone(), c.share, c.share && blocker(&c).is_none())
            };
            let now_shared = u.cameras.as_ref().and_then(|m| m.get(&name)).copied().unwrap_or(was_shared);
            if now_shared != was_shared {
                h.cfg.write().unwrap().share = now_shared;
                h.running.write().unwrap().share = now_shared;
            }
            if let Some(v) = u.unmasked.as_ref().and_then(|m| m.get(&name)).copied() {
                h.cfg.write().unwrap().share_unmasked = v;
                h.running.write().unwrap().share_unmasked = v;
            }
            let served = now_shared && blocker(&h.cfg.read().unwrap()).is_none();
            if (cfg.restream.enabled && served) != (was_on && was_served) && main_session_missing(h) {
                restart.push(h.id);
            }
        }
        if let Err(e) = cfg.save(&engine.config_path) {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({ "error": format!("applied, but could not be saved to the config file: {e:#}") })),
            )
                .into_response();
        }
    }
    refresh(&engine);
    for id in restart {
        engine.restart_camera(id);
    }
    if u.enabled.is_none() && u.port.is_none() {
        return Json(status_json(&engine)).into_response();
    }
    // Give the listener a moment to open or close before reporting on it.
    tokio::time::sleep(std::time::Duration::from_millis(1300)).await;
    Json(status_json(&engine)).into_response()
}
