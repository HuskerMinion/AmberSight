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

#[derive(serde::Deserialize, Default)]
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
    /// What the player can decode (`codec:WxH[@fps],...`, see transcode/):
    /// the stream is sent untouched when it fits, else transcoded.
    #[serde(default)]
    caps: Option<String>,
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
///
/// With `caps` (what the player decodes), also what `stream` with the same
/// `caps`, `q` and `audio` sends: `{codec, width, height, fps, transcoded,
/// bitrate_kbps, source: {codec, width, height, fps, bit_depth}, encoder,
/// why}` (see docs/api-guide.md). Nothing is started by asking.
pub async fn live_info(
    State(engine): State<Arc<Engine>>,
    AxPath(id): AxPath<usize>,
    Query(q): Query<StreamQ>,
    caller: Option<axum::Extension<crate::web::Caller>>,
) -> Response {
    if crate::remote::is_remote(id) {
        return crate::remote::proxy_get(&engine, id, "/link/live/{id}/info", "application/json").await;
    }
    let caps = match q.caps.as_deref().map(crate::transcode::Caps::parse) {
        Some(Err(e)) => return (StatusCode::BAD_REQUEST, e).into_response(),
        Some(Ok(c)) => Some(c),
        None => None,
    };
    wake_if_on_demand(&engine, id);
    let sleeping = engine.camera(id).is_some_and(|h| h.state.read().unwrap().sleeping);
    match hubs(&engine, id) {
        None => (StatusCode::NOT_FOUND, "camera not found").into_response(),
        Some((main, sub)) => {
            let mut v = serde_json::json!({
                "main": main.info(),
                "sub": sub.info(),
                "viewers": main.viewers() + sub.viewers(),
                // true while a battery camera is still waking: ask again shortly.
                "waking": sleeping || (main.info().is_none() && sub.info().is_none() && engine.camera(id).is_some_and(|h| h.running.read().unwrap().on_demand)),
            });
            if let Some(caps) = caps {
                let user = caller.map(|c| c.0 .0.clone()).unwrap_or_default();
                if let Ok(p) = plan_live(&engine, id, &q, &caps, &user, false).await {
                    if let (Some(o), serde_json::Value::Object(add)) = (v.as_object_mut(), p.plan.json()) {
                        o.extend(add);
                    }
                }
            }
            Json(v).into_response()
        }
    }
}

/// What a viewer with caps is sent: a camera stream as it is, or a shared
/// transcode (with the account's place in its limit).
enum Serve {
    Hub(Arc<LiveHub>),
    Session(Arc<crate::transcode::live::Session>, crate::transcode::UserLease),
}

struct LivePlan {
    plan: crate::transcode::Plan,
    serve: Option<Serve>,
}

/// Choose for a player with `caps`. `start`: start (or join) the transcode
/// and hold a place for this viewer; else only say what would happen.
async fn plan_live(engine: &Arc<Engine>, id: usize, q: &StreamQ, caps: &crate::transcode::Caps, user: &str, start: bool) -> Result<LivePlan, Response> {
    use crate::transcode::{self as tc, Plan, Target, Why};
    tc::set_config(&engine.config.lock().unwrap().transcode);
    let Some((main, sub)) = hubs(engine, id) else {
        return Err((StatusCode::NOT_FOUND, "camera not found").into_response());
    };
    let order = if q.q.as_deref() == Some("sub") { [(true, sub), (false, main)] } else { [(false, main), (true, sub)] };
    // A battery camera is woken and given time to start streaming.
    let on_demand = wake_if_on_demand(engine, id);
    let deadline = std::time::Instant::now() + if on_demand && start { std::time::Duration::from_secs(60) } else { std::time::Duration::ZERO };
    while order.iter().all(|(_, h)| h.info().is_none()) && std::time::Instant::now() < deadline {
        tokio::time::sleep(std::time::Duration::from_millis(250)).await;
        engine.wake_camera(id);
    }
    let avail: Vec<(bool, Arc<LiveHub>, tc::Source)> = order
        .iter()
        .filter_map(|(s, h)| {
            let mut d = tc::mp4::probe(&h.init()?)?;
            d.fps = h.fps();
            Some((*s, h.clone(), d))
        })
        .collect();
    let Some((is_sub, hub, src)) = avail.first().cloned() else {
        return Err((StatusCode::SERVICE_UNAVAILABLE, "no live stream (camera offline or on the ffmpeg pipeline)").into_response());
    };
    let untouched = |why: Option<Why>, pick: &(bool, Arc<LiveHub>, tc::Source)| LivePlan {
        plan: Plan { source: src.clone(), target: None, encoder: None, why, sent: why.is_some().then(|| pick.2.clone()) },
        serve: Some(Serve::Hub(pick.1.clone())),
    };
    let Some((target, why)) = tc::decide(caps, &src) else {
        return Ok(untouched(None, &avail[0]));
    };
    // Busy or off: the largest stream that fits, else the smallest there is.
    let fallback = |why: Why| {
        let area = |d: &tc::Source| d.width as u64 * d.height as u64;
        let pick = avail.iter().filter(|a| caps.fits(&a.2)).max_by_key(|a| area(&a.2)).or_else(|| avail.iter().min_by_key(|a| area(&a.2))).unwrap();
        untouched(Some(why), pick)
    };
    // A picture too small to be worth making: the best stream there is.
    if !tc::usable(&target) {
        return Ok(fallback(why));
    }
    let codec = target.codec;
    let audio = !q.quiet() && hub.info().is_some_and(|i| i.audio);
    let key = tc::live::Key::new(id, is_sub, &target, audio);
    let existing = tc::live::existing(&key);
    let encoders = match tokio::task::spawn_blocking(move || tc::encoders_for(codec)).await.ok().flatten() {
        Some(e) => e,
        // The test encodes have not finished yet.
        None if existing.is_none() => return Ok(fallback(Why::BusyFallback)),
        None => Vec::new(),
    };
    if encoders.is_empty() && existing.is_none() {
        return Ok(fallback(Why::Off));
    }
    // (A running one is described as it runs: its bit rate was set when
    // it started.)
    let plan = |enc: Option<String>, t: Target| Plan { source: src.clone(), target: Some(t), encoder: enc, why: Some(why), sent: None };
    if !start {
        if let Some(s) = existing {
            let enc = s.encoder.lock().unwrap().clone();
            return Ok(LivePlan { plan: plan(enc, s.target), serve: None });
        }
        if !tc::user_has_room(user) {
            return Ok(fallback(Why::BusyFallback));
        }
        return Ok(match tc::would_reserve(&encoders) {
            Some(e) => LivePlan { plan: plan(Some(e.name(codec).to_string()), target), serve: None },
            None => fallback(Why::BusyFallback),
        });
    }
    let Some(lease) = tc::user_lease(user) else { return Ok(fallback(Why::BusyFallback)) };
    let name = engine.camera(id).map(|h| h.cfg.read().unwrap().name.clone()).unwrap_or_default();
    let fps = src.fps.unwrap_or(25.0);
    // Starting one takes a place of the account's own for as long as it
    // runs, and an account starts at most a few a minute.
    let starter = || {
        let l = tc::user_lease_of(tc::LeaseKind::Started, user)?;
        tc::may_start(user).then_some(l)
    };
    let Some(sess) = tc::live::get_or_start(key, target, &name, hub, &encoders, fps, starter) else { return Ok(fallback(Why::BusyFallback)) };
    if !tc::live::wait_ready(&sess).await {
        return Ok(fallback(Why::BusyFallback));
    }
    let enc = sess.encoder.lock().unwrap().clone();
    Ok(LivePlan { plan: plan(enc, sess.target), serve: Some(Serve::Session(sess, lease)) })
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
///
/// With `caps`: untouched when the stream fits the player, else a shared
/// transcode (see `live_info`); X-AS-Codec, X-AS-Width, X-AS-Height,
/// X-AS-Transcoded (and X-AS-Encoder, X-AS-Why) say which.
pub async fn live_stream(
    State(engine): State<Arc<Engine>>,
    AxPath(id): AxPath<usize>,
    Query(q): Query<StreamQ>,
    caller: Option<axum::Extension<crate::web::Caller>>,
    method: axum::http::Method,
) -> Response {
    let quiet = q.quiet();
    let who: crate::web::Who = caller.as_ref().map(|c| (engine.clone(), c.0 .0.clone(), c.0 .1));
    // HEAD with caps: the headers of what would be sent; nothing starts.
    if method == axum::http::Method::HEAD {
        if let Some(Ok(caps)) = q.caps.as_deref().map(crate::transcode::Caps::parse) {
            let user = caller.map(|c| c.0 .0.clone()).unwrap_or_default();
            let p = match plan_live(&engine, id, &q, &caps, &user, false).await {
                Ok(p) => p,
                Err(r) => return r,
            };
            let mut b = Response::builder().header(header::CONTENT_TYPE, "video/mp4").header(header::CACHE_CONTROL, "no-store");
            for (k, v) in p.plan.headers() {
                b = b.header(k, v);
            }
            return b.body(Body::empty()).unwrap();
        }
    }
    let mut extra: Vec<(&'static str, String)> = Vec::new();
    let mut keep: Option<(crate::transcode::UserLease, Option<crate::transcode::live::Viewer>)> = None;
    let (mime, first, rx, quiet) = match q.caps.as_deref().map(crate::transcode::Caps::parse) {
        Some(Err(e)) => return (StatusCode::BAD_REQUEST, e).into_response(),
        Some(Ok(caps)) => {
            let user = caller.map(|c| c.0 .0.clone()).unwrap_or_default();
            let p = match plan_live(&engine, id, &q, &caps, &user, true).await {
                Ok(p) => p,
                Err(r) => return r,
            };
            extra = p.plan.headers();
            match p.serve {
                Some(Serve::Session(sess, lease)) => {
                    keep = Some((lease, Some(sess.join())));
                    let Some((init, gop, rx)) = sess.hub.subscribe_for(false) else {
                        return (StatusCode::SERVICE_UNAVAILABLE, "the transcode stopped").into_response();
                    };
                    let mime = sess.hub.info().map(|i| i.mime).unwrap_or_else(|| "video/mp4".into());
                    (mime, std::iter::once(init).chain(gop).collect::<Vec<_>>(), rx, false)
                }
                Some(Serve::Hub(h)) => {
                    let Some((init, gop, rx)) = h.subscribe_for(quiet) else {
                        return (StatusCode::SERVICE_UNAVAILABLE, "no live stream (camera offline or on the ffmpeg pipeline)").into_response();
                    };
                    let mime = h.info().map(|i| if quiet { i.video_mime.unwrap_or(i.mime) } else { i.mime }).unwrap_or_else(|| "video/mp4".into());
                    (mime, std::iter::once(init).chain(gop).collect(), rx, quiet)
                }
                None => return (StatusCode::SERVICE_UNAVAILABLE, "no live stream").into_response(),
            }
        }
        None => match subscribe(&engine, id, q.q.as_deref(), quiet).await {
            Ok((m, f, r)) => (m, f, r, quiet),
            Err(r) => return r,
        },
    };
    let stream = futures::stream::unfold((first.into_iter(), rx, keep), move |(mut pending, mut rx, keep)| async move {
        if let Some(b) = pending.next() {
            return Some((Ok::<_, std::io::Error>(b.as_ref().clone()), (pending, rx, keep)));
        }
        match rx.recv().await {
            Ok(LiveMsg::Fragment(b)) => {
                // (A transcode counts only viewers that take what it makes.)
                if let Some((_, Some(v))) = keep.as_ref() {
                    v.seen();
                }
                Some((Ok(for_viewer(&b, quiet)), (pending, rx, keep)))
            }
            // Restart, or this viewer lagged behind: end; the page reconnects
            // and starts again at the latest keyframe.
            Ok(LiveMsg::Reset) | Err(_) => None,
        }
    });
    // Ends when the account is turned off or its password changed (as the
    // WebSocket form does).
    use futures::StreamExt;
    let stream = stream.take_until(Box::pin(crate::web::account_gone(who)));
    let mut b = Response::builder().header(header::CONTENT_TYPE, "video/mp4").header(header::CACHE_CONTROL, "no-store").header("X-Live-Mime", mime);
    for (k, v) in extra {
        b = b.header(k, v);
    }
    b.body(Body::from_stream(stream)).unwrap()
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
