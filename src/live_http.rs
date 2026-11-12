//! HTTP endpoints for browser live view (fragmented MP4 for Media Source
//! Extensions). Cameras on the ffmpeg pipeline have no live fragments; the
//! page falls back to the MJPEG stream (`/api/live/:id`) for those.

use crate::engine::Engine;
use crate::ingest::live::{LiveHub, LiveMsg};
use axum::{
    body::Body,
    extract::{Path as AxPath, Query, State},
    http::{header, StatusCode},
    response::{IntoResponse, Json, Response},
};
use std::sync::Arc;

#[derive(serde::Deserialize)]
pub struct StreamQ {
    /// "main" (full resolution) or "sub"; main falls back to sub when the
    /// main stream is not running (and the other way round).
    #[serde(default)]
    q: Option<String>,
    /// `0`: the picture without sound, for a viewer that does not play it
    /// (a muted tile). Takes less data, and a problem in a camera's audio
    /// cannot stop the picture. Streams without audio are unchanged.
    #[serde(default)]
    audio: Option<String>,
}

impl StreamQ {
    fn quiet(&self) -> bool {
        self.audio.as_deref() == Some("0")
    }
}

/// Viewing an on-demand (battery) camera wakes it. Returns true if the
/// camera is on-demand (so callers may wait for its stream to start).
fn wake_if_on_demand(engine: &Engine, id: usize) -> bool {
    let on_demand = engine.camera(id).is_some_and(|h| h.running.read().unwrap().on_demand);
    if on_demand {
        engine.wake_camera(id);
    }
    on_demand
}

fn hubs(engine: &Engine, id: usize) -> Option<(Arc<LiveHub>, Arc<LiveHub>)> {
    let h = engine.camera(id)?;
    Some((h.live_main.clone(), h.live_sub.clone()))
}

/// `GET /api/live/:id/info` → which live streams are available and their
/// codecs: `{"main": {mime,width,height,audio} | null, "sub": ... }`.
pub async fn live_info(State(engine): State<Arc<Engine>>, AxPath(id): AxPath<usize>) -> Response {
    if crate::remote::is_remote(id) {
        return crate::remote::proxy_get(&engine, id, "/link/live/{id}/info", "application/json").await;
    }
    wake_if_on_demand(&engine, id);
    let sleeping = engine.camera(id).is_some_and(|h| h.state.read().unwrap().sleeping);
    match hubs(&engine, id) {
        None => (StatusCode::NOT_FOUND, "camera not found").into_response(),
        Some((main, sub)) => Json(serde_json::json!({
            "main": main.info(),
            "sub": sub.info(),
            "viewers": main.viewers() + sub.viewers(),
            // true while a battery camera is still waking: ask again shortly.
            "waking": sleeping || (main.info().is_none() && sub.info().is_none() && engine.camera(id).is_some_and(|h| h.running.read().unwrap().on_demand)),
        }))
        .into_response(),
    }
}

/// A live fragment for a viewer: without its audio when it asked for the
/// picture only (a stream without audio is sent as it is).
fn for_viewer(f: &Arc<Vec<u8>>, quiet: bool) -> Vec<u8> {
    if quiet {
        if let Some(v) = crate::ingest::fmp4::video_only(f) {
            return v;
        }
    }
    f.as_ref().clone()
}

/// A viewer's subscription: mime type, the init segment plus the current
/// GOP, and the receiver for new fragments.
type Subscription = (String, Vec<Arc<Vec<u8>>>, tokio::sync::broadcast::Receiver<LiveMsg>);

/// Subscribe to a camera's live fragments. A sleeping battery camera is woken
/// and given time to start streaming (a hub may need a few attempts; the
/// camera is kept awake meanwhile). None: unknown camera or no live stream.
async fn subscribe(engine: &Arc<Engine>, id: usize, q: Option<&str>, quiet: bool) -> Result<Subscription, Response> {
    let Some((main, sub)) = hubs(engine, id) else {
        return Err((StatusCode::NOT_FOUND, "camera not found").into_response());
    };
    let order = if q == Some("sub") { [sub, main] } else { [main, sub] };
    let on_demand = wake_if_on_demand(engine, id);
    let deadline = std::time::Instant::now()
        + if on_demand { std::time::Duration::from_secs(60) } else { std::time::Duration::ZERO };
    let mut got = order.iter().find_map(|h| h.subscribe_for(quiet).map(|s| (h.info(), s)));
    while got.is_none() && std::time::Instant::now() < deadline {
        tokio::time::sleep(std::time::Duration::from_millis(250)).await;
        if on_demand {
            engine.wake_camera(id);
        }
        got = order.iter().find_map(|h| h.subscribe_for(quiet).map(|s| (h.info(), s)));
    }
    let Some((which, (init, gop, rx))) = got else {
        return Err((StatusCode::SERVICE_UNAVAILABLE, "no live stream (camera offline or on the ffmpeg pipeline)").into_response());
    };
    let mime = which.map(|i| if quiet { i.video_mime.unwrap_or(i.mime) } else { i.mime }).unwrap_or_else(|| "video/mp4".into());
    Ok((mime, std::iter::once(init).chain(gop).collect(), rx))
}

/// `GET /api/live/:id/stream?q=main|sub` → endless fragmented MP4. The
/// stream ends when the camera restarts or the viewer falls too far behind;
/// the page reconnects. (Browsers allow only six HTTP/1.1 connections per
/// server, so the web page uses the WebSocket form below; this one is for
/// other players, e.g. Home Assistant.)
pub async fn live_stream(
    State(engine): State<Arc<Engine>>,
    AxPath(id): AxPath<usize>,
    Query(q): Query<StreamQ>,
) -> Response {
    let (mime, first, rx) = match subscribe(&engine, id, q.q.as_deref(), q.quiet()).await {
        Ok(s) => s,
        Err(r) => return r,
    };
    let quiet = q.quiet();
    let stream = futures::stream::unfold((first.into_iter(), rx), move |(mut pending, mut rx)| async move {
        if let Some(b) = pending.next() {
            return Some((Ok::<_, std::io::Error>(b.as_ref().clone()), (pending, rx)));
        }
        match rx.recv().await {
            Ok(LiveMsg::Fragment(b)) => Some((Ok(for_viewer(&b, quiet)), (pending, rx))),
            // Restart, or this viewer lagged behind: end; the page reconnects
            // and starts again at the latest keyframe.
            Ok(LiveMsg::Reset) | Err(_) => None,
        }
    });
    Response::builder()
        .header(header::CONTENT_TYPE, "video/mp4")
        .header(header::CACHE_CONTROL, "no-store")
        .header("X-Live-Mime", mime)
        .body(Body::from_stream(stream))
        .unwrap()
}

/// `GET /api/live/:id/ws?q=main|sub` (WebSocket) → the same fragmented MP4,
/// one binary message per fragment. WebSockets do not count against the
/// browser's six-connections-per-server limit, so a grid of many cameras
/// leaves room for the page's other requests. The socket closes when the
/// camera restarts or the viewer falls behind; the page reconnects.
pub async fn live_ws(
    State(engine): State<Arc<Engine>>,
    AxPath(id): AxPath<usize>,
    Query(q): Query<StreamQ>,
    caller: Option<axum::Extension<crate::web::Caller>>,
    ws: axum::extract::ws::WebSocketUpgrade,
) -> Response {
    let who = caller.map(|c| (engine.clone(), c.0 .0, c.0 .1));
    live_ws_with(engine, id, q, ws, None, who)
}

/// Asked every 10 s while a stream runs: false closes it (a linked server's
/// view when sharing is turned off or the link removed).
pub type StillAllowed = Option<Arc<dyn Fn() -> bool + Send + Sync>>;

pub fn live_ws_with(engine: Arc<Engine>, id: usize, q: StreamQ, ws: axum::extract::ws::WebSocketUpgrade, allowed: StillAllowed, who: crate::web::Who) -> Response {
    use axum::extract::ws::Message;
    let quiet = q.quiet();
    let q = q.q.clone();
    if crate::remote::is_remote(id) {
        let query = q.as_deref().map(|v| format!("q={}{}", url::form_urlencoded::byte_serialize(v.as_bytes()).collect::<String>(), if quiet { "&audio=0" } else { "" }));
        return crate::remote::proxy_ws(&engine, id, "/link/live/{id}/ws", query, ws, who);
    }
    ws.on_upgrade(move |mut socket| crate::web::while_account_allowed(who, async move {
        let (_mime, first, mut rx) = match subscribe(&engine, id, q.as_deref(), quiet).await {
            Ok(s) => s,
            Err(_) => {
                let _ = socket.send(Message::Close(None)).await;
                return;
            }
        };
        for b in first {
            if socket.send(Message::Binary(b.as_ref().clone())).await.is_err() {
                return;
            }
        }
        let mut check = tokio::time::interval(std::time::Duration::from_secs(10));
        check.tick().await;
        loop {
            tokio::select! {
                _ = check.tick(), if allowed.is_some() => {
                    if !allowed.as_ref().is_some_and(|f| f()) {
                        break;
                    }
                }
                m = rx.recv() => match m {
                    Ok(LiveMsg::Fragment(b)) => {
                        if socket.send(Message::Binary(for_viewer(&b, quiet))).await.is_err() {
                            return;
                        }
                    }
                    Ok(LiveMsg::Reset) | Err(_) => break,
                },
                // The page closed the socket (or sent something): stop.
                incoming = socket.recv() => match incoming {
                    None | Some(Err(_)) | Some(Ok(Message::Close(_))) => return,
                    Some(Ok(_)) => {}
                },
            }
        }
        let _ = socket.send(Message::Close(None)).await;
    }))
}

#[derive(serde::Deserialize)]
pub struct JpegQ {
    /// Highest frame rate to send (default 6, at most 15).
    #[serde(default)]
    fps: Option<u32>,
}

/// `GET /api/live/:id/jpeg` (WebSocket) → the camera's latest analysis frame
/// as a JPEG, one binary message per new frame, at most `fps` per second.
/// Light enough for a grid of many cameras (the frames are already decoded
/// for motion detection, so this costs the server nothing extra), and works
/// for cameras on the ffmpeg pipeline too. Does not wake battery cameras.
pub async fn live_jpeg_ws(
    State(engine): State<Arc<Engine>>,
    AxPath(id): AxPath<usize>,
    Query(q): Query<JpegQ>,
    caller: Option<axum::Extension<crate::web::Caller>>,
    ws: axum::extract::ws::WebSocketUpgrade,
) -> Response {
    let who = caller.map(|c| (engine.clone(), c.0 .0, c.0 .1));
    live_jpeg_ws_with(engine, id, q, ws, None, who)
}

pub fn live_jpeg_ws_with(engine: Arc<Engine>, id: usize, q: JpegQ, ws: axum::extract::ws::WebSocketUpgrade, allowed: StillAllowed, who: crate::web::Who) -> Response {
    use axum::extract::ws::Message;
    if crate::remote::is_remote(id) {
        return crate::remote::proxy_ws(&engine, id, "/link/live/{id}/jpeg", Some(format!("fps={}", q.fps.unwrap_or(6).clamp(1, 15))), ws, who);
    }
    let Some(handle) = engine.camera(id) else {
        return (StatusCode::NOT_FOUND, "camera not found").into_response();
    };
    let gap = std::time::Duration::from_millis(1000 / u64::from(q.fps.unwrap_or(6).clamp(1, 15)));
    ws.on_upgrade(move |mut socket| crate::web::while_account_allowed(who, async move {
        let mut last_seq = u64::MAX;
        let mut next = tokio::time::Instant::now();
        let mut checked = tokio::time::Instant::now();
        loop {
            if allowed.is_some() && checked.elapsed() >= std::time::Duration::from_secs(10) {
                checked = tokio::time::Instant::now();
                if !allowed.as_ref().is_some_and(|f| f()) {
                    let _ = socket.send(Message::Close(None)).await;
                    return;
                }
            }
            let due = tokio::time::Instant::now() >= next;
            let (seq, frame) = {
                let st = handle.state.read().unwrap();
                let fresh = due && st.frame_seq != last_seq;
                (st.frame_seq, if fresh { st.latest_frame.clone() } else { Vec::new() })
            };
            if !frame.is_empty() {
                last_seq = seq;
                next = tokio::time::Instant::now() + gap;
                if socket.send(Message::Binary(frame)).await.is_err() {
                    return;
                }
            }
            // Wait a little for the next frame, or for the page to close.
            tokio::select! {
                _ = tokio::time::sleep(std::time::Duration::from_millis(40)) => {}
                incoming = socket.recv() => match incoming {
                    None | Some(Err(_)) | Some(Ok(Message::Close(_))) => return,
                    Some(Ok(_)) => {}
                },
            }
        }
    }))
}
