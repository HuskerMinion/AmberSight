//! A linked server's cameras shown here (live view), off by default.
//!
//! The server that owns the cameras decides: per link, "share my cameras"
//! (`LinkConfig::share`). The viewing server decides whether to show them
//! (`LinkConfig::show`). Shared cameras appear in this server's camera list
//! with ids from REMOTE_BASE up and a `remote` badge; their live view,
//! snapshots and audio are fetched from the other server only while someone
//! watches, over the link's pinned HTTPS with the link key. Recording,
//! events and playback stay on the server the camera belongs to.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use axum::{
    body::Body,
    extract::{ws::WebSocketUpgrade, Path as AxPath, Query, State},
    http::{header, HeaderMap, StatusCode},
    response::{IntoResponse, Json, Response},
};
use futures::StreamExt;

use crate::capture::net::{self, Target};
use crate::config::LinkConfig;
use crate::engine::Engine;

/// Remote camera ids: REMOTE_BASE + link position * PER_LINK + its id there.
pub const REMOTE_BASE: usize = 1_000_000;
const PER_LINK: usize = 10_000;
/// Most cameras taken from one linked server.
const MAX_REMOTE: usize = 256;
/// How often the shown servers' camera lists are refreshed.
const REFRESH: Duration = Duration::from_secs(20);

pub fn is_remote(id: usize) -> bool {
    id >= REMOTE_BASE
}

fn remote_id(link_num: usize, cam: usize) -> Option<usize> {
    (cam < PER_LINK && link_num < 10_000).then(|| REMOTE_BASE + link_num * PER_LINK + cam)
}

/// Give every link a lasting number (saved), so remote camera ids stay put.
pub fn number_links(engine: &Engine) {
    let mut cfg = engine.config.lock().unwrap();
    if cfg.links.iter().all(|l| l.num > 0) {
        return;
    }
    let mut next = cfg.links.iter().map(|l| l.num).max().unwrap_or(0) + 1;
    for l in cfg.links.iter_mut().filter(|l| l.num == 0) {
        l.num = next;
        next += 1;
    }
    let _ = cfg.save(&engine.config_path);
}

/// (link number, camera id on that server)
fn split(id: usize) -> Option<(usize, usize)> {
    let r = id.checked_sub(REMOTE_BASE)?;
    Some((r / PER_LINK, r % PER_LINK))
}

/// The shown link a remote id belongs to, and the camera id there.
fn target_of(engine: &Engine, id: usize) -> Option<(LinkConfig, usize)> {
    let (num, cam) = split(id)?;
    let l = engine.config.lock().unwrap().links.iter().find(|l| l.num as usize == num).cloned()?;
    l.show.then_some((l, cam))
}

fn err(code: StatusCode, msg: &str) -> Response {
    (code, Json(serde_json::json!({ "error": msg }))).into_response()
}

// ------------------------------------------------------------- sharing side

/// The link presenting this request, if it may see camera `id` (our own
/// cameras only: never ones we show from a third server).
fn sharing_cam(engine: &Engine, h: &HeaderMap, id: usize) -> Result<LinkConfig, Response> {
    let l = sharing(engine, h)?;
    if is_remote(id) || engine.camera(id).is_none() {
        return Err(err(StatusCode::NOT_FOUND, "camera not found"));
    }
    Ok(l)
}

/// The link presenting this request, if it may see our cameras.
fn sharing(engine: &Engine, h: &HeaderMap) -> Result<LinkConfig, Response> {
    match crate::links::authenticate(engine, h) {
        Some(l) if l.share => Ok(l),
        Some(_) => Err(err(StatusCode::FORBIDDEN, "this server does not share its cameras with you")),
        None => Err(err(StatusCode::UNAUTHORIZED, "unknown server")),
    }
}

/// `GET /link/cameras`: our cameras, for a linked server we share them with.
pub async fn share_cameras(State(engine): State<Arc<Engine>>, h: HeaderMap) -> Response {
    if let Err(r) = sharing(&engine, &h) {
        return r;
    }
    let cams: Vec<serde_json::Value> = engine
        .cameras
        .read()
        .unwrap()
        .iter()
        .flatten()
        .filter(|c| c.id < PER_LINK)
        .map(|c| {
            let cfg = c.cfg.read().unwrap();
            let st = c.state.read().unwrap();
            serde_json::json!({
                "id": c.id,
                "name": cfg.name,
                "short_name": cfg.short_name,
                "online": st.online,
                "sleeping": st.sleeping,
                "enabled": c.active.load(std::sync::atomic::Ordering::Relaxed),
                "on_demand": cfg.on_demand,
                "audio": cfg.audio,
                "overlay_text": cfg.overlay_text,
                "overlay_pos": cfg.overlay_pos,
                "overlay": cfg.overlay,
            })
        })
        .collect();
    Json(serde_json::json!({ "name": crate::links::own_name(&engine), "cameras": cams })).into_response()
}

#[derive(serde::Deserialize)]
pub struct SinceQ {
    #[serde(default)]
    since: u64,
}

/// `GET /link/stats?since=`: this server's status (CPU, memory, frame rate,
/// disk) for a linked server we share with; no folder paths.
pub async fn share_stats(State(engine): State<Arc<Engine>>, h: HeaderMap, Query(q): Query<SinceQ>) -> Response {
    if let Err(r) = sharing(&engine, &h) {
        return r;
    }
    let up = chrono::Utc::now().timestamp() - engine.started;
    let mut j = engine.stats.json(up, q.since, &|_| true);
    for d in j.get_mut("storage").and_then(|s| s.as_array_mut()).into_iter().flatten() {
        if let Some(o) = d.as_object_mut() {
            o.remove("path");
        }
    }
    Json(j).into_response()
}

/// `GET /api/links/:id/stats?since=`: a shown linked server's status.
pub async fn api_link_stats(
    State(engine): State<Arc<Engine>>,
    axum::Extension(access): axum::Extension<crate::web::Access>,
    AxPath(id): AxPath<String>,
    Query(q): Query<SinceQ>,
) -> Response {
    // Camera-limited accounts do not see other servers at all.
    if access.limited() {
        return err(StatusCode::NOT_FOUND, "no such server shown here");
    }
    let Some(l) = engine.config.lock().unwrap().links.iter().find(|l| l.id == id && l.show).cloned() else {
        return err(StatusCode::NOT_FOUND, "no such server shown here");
    };
    let path = format!("/link/stats?since={}", q.since);
    let r = async {
        let t = Target::parse(&l.url)?;
        net::request_with(&t, net::pinned(&l.ca_pem)?, "GET", &path, &[("Authorization", format!("Bearer {}", l.token))], None, b"").await
    };
    match tokio::time::timeout(Duration::from_secs(10), r).await {
        Ok(Ok((200, body))) => ([(header::CONTENT_TYPE, "application/json"), (header::CACHE_CONTROL, "no-store")], Body::from(body)).into_response(),
        Ok(Ok((s, _))) => err(StatusCode::BAD_GATEWAY, &format!("{} refused (HTTP {s})", l.name)),
        Ok(Err(e)) => err(StatusCode::BAD_GATEWAY, &format!("could not reach {}: {e:#}", l.name)),
        Err(_) => err(StatusCode::GATEWAY_TIMEOUT, &format!("{} did not answer", l.name)),
    }
}

/// `GET /link/live/:id/info`
pub async fn share_live_info(State(engine): State<Arc<Engine>>, h: HeaderMap, AxPath(id): AxPath<usize>) -> Response {
    if let Err(r) = sharing_cam(&engine, &h, id) {
        return r;
    }
    crate::live_http::live_info(State(engine), AxPath(id)).await
}

/// `GET /link/live/:id/ws` (WebSocket, fMP4 fragments)
pub async fn share_live_ws(
    State(engine): State<Arc<Engine>>,
    h: HeaderMap,
    AxPath(id): AxPath<usize>,
    q: Query<crate::live_http::StreamQ>,
    ws: WebSocketUpgrade,
) -> Response {
    let l = match sharing_cam(&engine, &h, id) {
        Ok(l) => l,
        Err(r) => return r,
    };
    let allowed = still_shared(&engine, &l);
    crate::live_http::live_ws_with(engine, id, q.0, ws, Some(allowed), None)
}

/// For a running shared stream: is this link still there, still shared?
fn still_shared(engine: &Arc<Engine>, l: &LinkConfig) -> Arc<dyn Fn() -> bool + Send + Sync> {
    let e = engine.clone();
    let key = l.peer_token_sha256.clone();
    Arc::new(move || e.config.lock().unwrap().links.iter().any(|x| x.peer_token_sha256 == key && x.share))
}

/// `GET /link/live/:id/jpeg` (WebSocket, JPEG frames)
pub async fn share_live_jpeg(
    State(engine): State<Arc<Engine>>,
    h: HeaderMap,
    AxPath(id): AxPath<usize>,
    q: Query<crate::live_http::JpegQ>,
    ws: WebSocketUpgrade,
) -> Response {
    let l = match sharing_cam(&engine, &h, id) {
        Ok(l) => l,
        Err(r) => return r,
    };
    let allowed = still_shared(&engine, &l);
    crate::live_http::live_jpeg_ws_with(engine, id, q.0, ws, Some(allowed), None)
}

/// `GET /link/snapshot/:id`
pub async fn share_snapshot(State(engine): State<Arc<Engine>>, h: HeaderMap, AxPath(id): AxPath<usize>) -> Response {
    if let Err(r) = sharing_cam(&engine, &h, id) {
        return r;
    }
    crate::web::snapshot(State(engine), AxPath(id)).await
}

// ------------------------------------------------------------- viewing side

#[derive(Clone, Default)]
struct Shown {
    at: Option<Instant>,
    /// The other server's camera list, or why there is none.
    cams: Vec<serde_json::Value>,
    error: Option<String>,
}

fn cache() -> &'static Mutex<HashMap<String, Shown>> {
    static C: OnceLock<Mutex<HashMap<String, Shown>>> = OnceLock::new();
    C.get_or_init(Default::default)
}

async fn fetch_list(l: &LinkConfig) -> Shown {
    let r = tokio::time::timeout(Duration::from_secs(8), crate::links::call(l, "GET", "/link/cameras", &serde_json::Value::Null)).await;
    let (cams, error) = match r {
        Ok(Ok((200, v))) => (v["cameras"].as_array().cloned().unwrap_or_default(), None),
        Ok(Ok((403, _))) => (Vec::new(), Some(format!("{} does not share its cameras with this server (turn it on there)", l.name))),
        Ok(Ok((401, _))) => (Vec::new(), Some(format!("{} no longer accepts this link", l.name))),
        Ok(Ok((s, _))) => (Vec::new(), Some(format!("{} answered HTTP {s}", l.name))),
        Ok(Err(e)) => (Vec::new(), Some(format!("could not reach {}: {e:#}", l.name))),
        Err(_) => (Vec::new(), Some(format!("{} did not answer", l.name))),
    };
    Shown { at: Some(Instant::now()), cams, error }
}

/// Store a fetched list. A failed fetch keeps the last good list (shown
/// offline once it is old) instead of making the cameras disappear.
fn keep_list(link_id: &str, s: Shown) {
    let mut c = cache().lock().unwrap();
    match c.get_mut(link_id) {
        Some(old) if s.error.is_some() => old.error = s.error,
        _ => {
            c.insert(link_id.to_string(), s);
        }
    }
}

/// Refresh one link's list now (after turning "show" on).
pub async fn refresh(engine: &Arc<Engine>, link_id: &str) {
    let l = engine.config.lock().unwrap().links.iter().find(|l| l.id == link_id).cloned();
    match l {
        Some(l) if l.show => {
            let s = fetch_list(&l).await;
            keep_list(&l.id, s);
        }
        _ => {
            cache().lock().unwrap().remove(link_id);
        }
    }
}

/// Keep the shown servers' camera lists fresh.
pub fn start(engine: Arc<Engine>) {
    tokio::spawn(async move {
        loop {
            let links: Vec<LinkConfig> = engine.config.lock().unwrap().links.iter().filter(|l| l.show).cloned().collect();
            number_links(&engine);
            for l in &links {
                let s = fetch_list(l).await;
                keep_list(&l.id, s);
            }
            {
                let keep: Vec<String> = links.iter().map(|l| l.id.clone()).collect();
                cache().lock().unwrap().retain(|k, _| keep.contains(k));
            }
            tokio::time::sleep(REFRESH).await;
        }
    });
}

/// Why a shown link has no cameras (for Settings > Connections > Servers).
pub fn link_error(link_id: &str) -> Option<String> {
    cache().lock().unwrap().get(link_id).and_then(|s| s.error.clone())
}

/// Shown servers' cameras, in the shape of `/api/cameras` entries.
pub fn ui_cameras(engine: &Engine) -> Vec<serde_json::Value> {
    let links = engine.config.lock().unwrap().links.clone();
    let cache = cache().lock().unwrap();
    let mut out = Vec::new();
    for l in links.iter().filter(|l| l.show && l.num > 0) {
        let pos = l.num as usize;
        let Some(s) = cache.get(&l.id) else { continue };
        let fresh = s.at.is_some_and(|t| t.elapsed() < REFRESH * 4);
        // The other server's answer is checked field by field: types, lengths,
        // at most MAX_REMOTE cameras, no repeated ids or names.
        let txt = |v: &serde_json::Value, max: usize| -> Option<String> {
            v.as_str().map(|x| x.chars().filter(|c| !c.is_control()).take(max).collect())
        };
        let flag = |v: &serde_json::Value| v.as_bool().unwrap_or(false);
        let mut seen_ids = std::collections::HashSet::new();
        let mut seen_names = std::collections::HashSet::new();
        for c in s.cams.iter().take(MAX_REMOTE) {
            let (Some(cid), Some(name)) = (c["id"].as_u64(), txt(&c["name"], 64).filter(|n| !n.trim().is_empty())) else { continue };
            if !seen_ids.insert(cid) || !seen_names.insert(name.clone()) {
                continue;
            }
            let Some(id) = remote_id(pos, cid as usize) else { continue };
            let name = name.as_str();
            let pos_ok = ["top-left", "top-right", "bottom-left", "bottom-right"];
            let opos = txt(&c["overlay_pos"], 16).filter(|p| pos_ok.contains(&p.as_str()));
            out.push(serde_json::json!({
                "id": id,
                // Unique here (layouts and groups go by name), with the site shown as a badge.
                "name": format!("{name} ({})", l.name),
                "short_name": txt(&c["short_name"], 32).unwrap_or_default(),
                "remote": { "site": l.name, "link": l.id, "camera": name },
                "groups": l.remote_groups.get(name).cloned().unwrap_or_default(),
                "group": l.remote_groups.get(name).and_then(|g| g.first().cloned()).unwrap_or_default(),
                "online": fresh && flag(&c["online"]),
                "sleeping": flag(&c["sleeping"]),
                "enabled": c["enabled"].as_bool().unwrap_or(true),
                "on_demand": flag(&c["on_demand"]),
                "audio": flag(&c["audio"]),
                "overlay_text": txt(&c["overlay_text"], 120),
                "overlay_pos": opos,
                "overlay": flag(&c["overlay"]),
                "ptz": false,
                "record": false,
                "motion": false,
                "zones": [],
                "ignore_zones": [],
                "blobs": [],
                "clip_count": 0,
            }));
        }
    }
    out
}

/// Remember local groups for a shown camera (`PUT /api/cameras/:id` with
/// only `groups` for a remote id). Nothing else can be changed from here.
pub fn set_groups(engine: &Engine, id: usize, body: &serde_json::Value) -> Result<(), String> {
    let (num, cam) = split(id).ok_or("not a remote camera")?;
    let obj = body.as_object().ok_or("expected an object")?;
    if obj.keys().any(|k| k != "groups") || !obj.contains_key("groups") {
        return Err("this camera belongs to another server: only its groups can be changed here; change the rest on that server".into());
    }
    let groups: Vec<String> = obj["groups"]
        .as_array()
        .ok_or("groups must be a list")?
        .iter()
        .filter_map(|x| x.as_str())
        .map(|x| x.trim().chars().take(64).collect::<String>())
        .filter(|x| !x.is_empty())
        .collect();
    // Read the name and write the groups under the same locks (cache, then config).
    let c = cache().lock().unwrap();
    let mut cfg = engine.config.lock().unwrap();
    let l = cfg.links.iter_mut().find(|l| l.num as usize == num).ok_or("no such server")?;
    let name = c
        .get(&l.id)
        .and_then(|s| s.cams.iter().find(|x| x["id"].as_u64() == Some(cam as u64)).and_then(|x| x["name"].as_str().map(String::from)))
        .ok_or("that camera is not shown right now")?;
    if groups.is_empty() {
        l.remote_groups.remove(&name);
    } else {
        l.remote_groups.insert(name, groups);
    }
    drop(c);
    cfg.save(&engine.config_path).map_err(|e| format!("could not save: {e:#}"))
}

/// A plain GET forwarded to the camera's server (info JSON, snapshot JPEG).
pub async fn proxy_get(engine: &Arc<Engine>, id: usize, path: &str, content_type: &'static str) -> Response {
    let Some((l, cam)) = target_of(engine, id) else { return err(StatusCode::NOT_FOUND, "camera not found") };
    let path = path.replace("{id}", &cam.to_string());
    let r = async {
        let t = Target::parse(&l.url)?;
        net::request_with(&t, net::pinned(&l.ca_pem)?, "GET", &path, &[("Authorization", format!("Bearer {}", l.token))], None, b"").await
    };
    match tokio::time::timeout(Duration::from_secs(15), r).await {
        Ok(Ok((200, body))) => ([(header::CONTENT_TYPE, content_type), (header::CACHE_CONTROL, "no-cache")], Body::from(body)).into_response(),
        // Never pass the other server's 401/403 on: the page would take it as
        // "sign in again here".
        Ok(Ok((404, _))) => err(StatusCode::NOT_FOUND, "camera not found"),
        Ok(Ok((503, _))) => err(StatusCode::SERVICE_UNAVAILABLE, "no picture from that camera yet"),
        Ok(Ok((s, _))) => err(StatusCode::BAD_GATEWAY, &format!("{} refused (HTTP {s})", l.name)),
        Ok(Err(e)) => err(StatusCode::BAD_GATEWAY, &format!("could not reach {}: {e:#}", l.name)),
        Err(_) => err(StatusCode::GATEWAY_TIMEOUT, &format!("{} did not answer", l.name)),
    }
}

/// A live WebSocket (fMP4 or JPEG frames) relayed from the camera's server.
pub fn proxy_ws(engine: &Arc<Engine>, id: usize, path: &str, query: Option<String>, ws: WebSocketUpgrade, who: crate::web::Who) -> Response {
    use axum::extract::ws::Message as AMsg;
    use tokio_tungstenite::tungstenite::{client::IntoClientRequest, Message as TMsg};
    let Some((l, cam)) = target_of(engine, id) else { return err(StatusCode::NOT_FOUND, "camera not found") };
    let mut path = path.replace("{id}", &cam.to_string());
    if let Some(q) = query.filter(|q| !q.is_empty()) {
        path.push('?');
        path.push_str(&q);
    }
    ws.on_upgrade(move |mut browser| crate::web::while_account_allowed(who, async move {
        let peer = async {
            let t = Target::parse(&l.url)?;
            let io = net::connect(&t, net::pinned(&l.ca_pem)?).await?;
            let mut req = format!("wss://{}{}", t.authority, path).into_client_request()?;
            req.headers_mut().insert("Authorization", format!("Bearer {}", l.token).parse()?);
            // Live fragments are small; refuse anything huge from the other side.
            let mut wcfg = tokio_tungstenite::tungstenite::protocol::WebSocketConfig::default();
            wcfg.max_message_size = Some(8 << 20);
            wcfg.max_frame_size = Some(8 << 20);
            let (s, _) = tokio::time::timeout(Duration::from_secs(10), tokio_tungstenite::client_async_with_config(req, io, Some(wcfg))).await??;
            anyhow::Ok(s)
        };
        let mut peer = match peer.await {
            Ok(p) => p,
            Err(e) => {
                tracing::debug!("remote live {}: {e:#}", l.name);
                let _ = browser.send(AMsg::Close(None)).await;
                return;
            }
        };
        loop {
            tokio::select! {
                m = peer.next() => match m {
                    Some(Ok(TMsg::Binary(b))) => {
                        if browser.send(AMsg::Binary(b)).await.is_err() {
                            break;
                        }
                    }
                    Some(Ok(TMsg::Text(t))) => {
                        if browser.send(AMsg::Text(t)).await.is_err() {
                            break;
                        }
                    }
                    Some(Ok(TMsg::Ping(_) | TMsg::Pong(_) | TMsg::Frame(_))) => {}
                    Some(Ok(TMsg::Close(_))) | Some(Err(_)) | None => break,
                },
                m = browser.recv() => match m {
                    None | Some(Err(_)) | Some(Ok(AMsg::Close(_))) => break,
                    Some(Ok(_)) => {}
                },
            }
        }
        let _ = peer.close(None).await;
        let _ = browser.send(AMsg::Close(None)).await;
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ids_round_trip() {
        let id = remote_id(2, 15).unwrap();
        assert!(is_remote(id));
        assert_eq!(split(id), Some((2, 15)));
        assert!(!is_remote(15));
        assert_eq!(remote_id(0, PER_LINK), None);
    }
}
