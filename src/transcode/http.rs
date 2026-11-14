//! `caps` on recordings (`/api/playback`) and event clips (`/api/clips/:id`):
//! untouched when the recording fits the player (the caller serves it as
//! before), else transcoded on the fly; `info=1` answers what would be sent.

use super::file::{Input, Job};
use super::{mp4, Caps, Plan, Why};
use crate::engine::Engine;
use axum::{
    http::{header, StatusCode},
    response::{IntoResponse, Json, Response},
};
use std::sync::Arc;

/// Which recording.
#[derive(Clone)]
pub enum ClipSrc {
    /// A window of a camera's recordings, played from `start` (ms).
    Window { camera: String, from: i64, to: i64, start: i64 },
    /// An event's clip, from `offset` ms into it.
    Event { id: String, offset: i64 },
}

struct Opened {
    input: Input,
    seek_ms: i64,
    /// Video to send (ms), when the clip read runs past the end.
    limit_ms: Option<i64>,
    /// The whole clip or window: first frame and length (a saved clip
    /// file: from the event's start minus pre-roll, as recorded).
    start_ms: i64,
    duration_ms: i64,
    /// Where a transcode begins (exactly the moment asked for) and where an
    /// untouched answer begins (the keyframe at or before it; a clip sent
    /// as it is ignores `start`).
    exact_start_ms: i64,
    untouched_start_ms: i64,
    end_ms: i64,
    sub: bool,
    desc: mp4::VideoDesc,
    camera: String,
}

impl Opened {
    /// (wall time of output position 0, ms of video sent).
    fn media(&self, transcoded: bool) -> (i64, i64) {
        let s = if transcoded { self.exact_start_ms } else { self.untouched_start_ms };
        (s, (self.end_ms - s).max(0))
    }

    /// The timeline's x-clip-* headers, for answers made here.
    fn clip_headers(&self, transcoded: bool) -> Vec<(&'static str, String)> {
        let (s, _) = self.media(transcoded);
        vec![
            ("x-clip-start-ms", s.to_string()),
            ("x-clip-end-ms", self.end_ms.to_string()),
            ("x-clip-sub", (self.sub as u8).to_string()),
            ("x-clip-reverse", "0".into()),
            ("x-clip-frames", "0".into()),
        ]
    }
}

/// The start of a clip, for its description (its header and the first
/// fragments' headers; not the pictures).
fn probe_clip(c: &crate::ingest::splice::VirtualClip) -> Option<mp4::VideoDesc> {
    mp4::probe_with(c.len(), &|a, b| c.read_range(a, b).ok())
}

fn open(engine: &Engine, src: &ClipSrc, low: bool) -> anyhow::Result<Option<Opened>> {
    match src {
        ClipSrc::Window { camera, from, to, start } => {
            let Some(clip) = engine.playback_clip(camera, *start, *to, low)? else { return Ok(None) };
            let full = if start == from { (clip.start_ms, clip.end_ms) } else { engine.playback_clip(camera, *from, *to, low)?.map(|c| (c.start_ms, c.end_ms)).unwrap_or((clip.start_ms, clip.end_ms)) };
            let Some(desc) = probe_clip(&clip) else { return Ok(None) };
            let exact = (*start).max(clip.start_ms).min(clip.end_ms.max(clip.start_ms));
            Ok(Some(Opened {
                seek_ms: exact - clip.start_ms,
                limit_ms: None,
                start_ms: full.0,
                duration_ms: full.1 - full.0,
                exact_start_ms: exact,
                untouched_start_ms: clip.start_ms,
                end_ms: clip.end_ms,
                sub: clip.sub,
                input: Input::Clip(clip),
                desc,
                camera: camera.clone(),
            }))
        }
        ClipSrc::Event { id, offset } => {
            let Some(ev) = engine.db.get_event(id)? else { return Ok(None) };
            let path = engine.clip_file_path(id);
            if path.is_file() && !low {
                let Some(desc) = mp4::probe_file(&path)? else { return Ok(None) };
                let pre = engine.config.lock().unwrap().motion_for(&ev.camera).pre_roll as i64;
                let start_ms = (ev.start - pre) * 1000;
                let duration_ms = desc.duration_ms.unwrap_or((ev.end - ev.start) * 1000);
                let off = (*offset).max(0).min(duration_ms.max(0));
                return Ok(Some(Opened {
                    input: Input::File(path),
                    seek_ms: off,
                    limit_ms: None,
                    start_ms,
                    duration_ms,
                    exact_start_ms: start_ms + off,
                    untouched_start_ms: start_ms,
                    end_ms: start_ms + duration_ms,
                    sub: false,
                    desc,
                    camera: ev.camera,
                }));
            }
            let Some(clip) = engine.virtual_clip_q(&ev, low)? else { return Ok(None) };
            let (start_ms, end_ms, sub) = (clip.start_ms, clip.end_ms, clip.sub);
            let duration_ms = end_ms - start_ms;
            let Some(desc) = probe_clip(&clip) else { return Ok(None) };
            let off = (*offset).max(0).min(duration_ms.max(0));
            let mut o = Opened { input: Input::Clip(clip), seek_ms: off, limit_ms: None, start_ms, duration_ms, exact_start_ms: start_ms + off, untouched_start_ms: start_ms, end_ms, sub, desc, camera: ev.camera.clone() };
            if off > 0 && off < duration_ms {
                // From the keyframe before the moment, not the clip's start.
                let at = start_ms + off;
                if let Some(c) = engine.playback_clip(&ev.camera, at, end_ms, sub)? {
                    o.seek_ms = (at - c.start_ms).max(0);
                    o.limit_ms = Some(duration_ms - off);
                    o.input = Input::Clip(c);
                }
            }
            Ok(Some(o))
        }
    }
}

pub enum Outcome {
    /// Answered here (transcoded video, the info JSON, or an error).
    Done(Response),
    /// Serve it as before (`low`: the sub stream recording), adding these
    /// headers.
    Untouched { low: bool, headers: Vec<(&'static str, String)> },
}

fn info_json(plan: &Plan, o: &Opened) -> Response {
    let mut v = plan.json();
    let (ms, md) = o.media(plan.target.is_some());
    if let Some(m) = v.as_object_mut() {
        m.insert("start_ms".into(), o.start_ms.into());
        m.insert("duration_ms".into(), o.duration_ms.into());
        m.insert("media_start_ms".into(), ms.into());
        m.insert("media_duration_ms".into(), md.into());
    }
    Json(v).into_response()
}

/// How the request asks for the bytes.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Ask {
    /// From the start (no Range, or `bytes=0-`).
    Start,
    /// `Range: bytes=0-0` (the timeline reading the x-clip-* headers) or
    /// HEAD: a transcode is answered with its headers and an empty body,
    /// and none is started.
    Headers,
    /// A Range from further in: a transcode cannot answer it (it has no
    /// byte positions); the untouched choice is sent, as for busy.
    Resume,
}

/// Handle a request with `caps` (None: `info=1` alone, what is sent as it
/// is). `info`: answer JSON.
pub async fn handle(engine: Arc<Engine>, src: ClipSrc, caps: Option<Caps>, info: bool, ask: Ask, low: bool, user: String) -> Outcome {
    let probe = ask == Ask::Headers;
    super::set_config(&engine.config.lock().unwrap().transcode);
    let (e, s) = (engine.clone(), src.clone());
    let opened = tokio::task::spawn_blocking(move || open(&e, &s, low)).await;
    let o = match opened {
        Ok(Ok(Some(o))) => o,
        Ok(Ok(None)) => return Outcome::Done((StatusCode::NOT_FOUND, "nothing recorded there").into_response()),
        Ok(Err(e)) => return Outcome::Done(crate::web::internal_error("transcode", &e)),
        Err(_) => return Outcome::Done((StatusCode::INTERNAL_SERVER_ERROR, "transcode error").into_response()),
    };
    let is_event = matches!(src, ClipSrc::Event { .. });
    if let ClipSrc::Event { offset, .. } = &src {
        if *offset > 0 && *offset >= o.duration_ms {
            return Outcome::Done((StatusCode::BAD_REQUEST, "start is past the end of the clip").into_response());
        }
    }
    // Untouched answers are made by the caller. The playback path sends its
    // own x-clip-* headers; a clip gets start, end and sub here.
    let untouched = |low: bool, plan: Plan, o: &Opened| {
        if info {
            return Outcome::Done(info_json(&plan, o));
        }
        let mut headers = plan.headers();
        if is_event {
            headers.extend(o.clip_headers(false).into_iter().take(3));
        }
        Outcome::Untouched { low, headers }
    };
    let untouched_plan = Plan { source: o.desc.clone(), target: None, encoder: None, why: None, sent: None };
    let Some((target, why)) = caps.as_ref().and_then(|c| super::decide(c, &o.desc)) else {
        return untouched(low, untouched_plan, &o);
    };
    let caps = caps.unwrap_or_default();
    // Busy or off: the other stream's recording (sub, or main when sub was
    // asked for) when it fits, else as it is.
    let fallback = |why: Why| {
        let (e, s) = (engine.clone(), src.clone());
        let caps = caps.clone();
        let source = o.desc.clone();
        async move {
            let other = tokio::task::spawn_blocking(move || open(&e, &s, !low)).await.ok().and_then(|r| r.ok()).flatten();
            match other.filter(|s| caps.fits(&s.desc) && s.desc != source) {
                Some(s) => (!low, Plan { source: source.clone(), target: None, encoder: None, why: Some(why), sent: Some(s.desc.clone()) }, Some(s)),
                None => (low, Plan { source: source.clone(), target: None, encoder: None, why: Some(why), sent: Some(source) }, None),
            }
        }
    };
    let codec = target.codec;
    let encoders = tokio::task::spawn_blocking(move || super::encoders_for(codec)).await.ok().flatten();
    let busy = if ask == Ask::Resume {
        Some(Why::BusyFallback)
    } else if !super::usable(&target) {
        // Too small to be worth making: the best there is instead.
        Some(why)
    } else if encoders.is_none() {
        // The test encodes have not finished yet.
        Some(Why::BusyFallback)
    } else if encoders.as_ref().is_some_and(|e| e.is_empty()) {
        Some(Why::Off)
    } else if !super::user_has_room(&user) || encoders.as_deref().and_then(super::would_reserve).is_none() {
        Some(Why::BusyFallback)
    } else {
        None
    };
    let encoders = encoders.unwrap_or_default();
    if let Some(w) = busy {
        let (low, plan, sub) = fallback(w).await;
        return untouched(low, plan, sub.as_ref().unwrap_or(&o));
    }
    if info || probe {
        let plan = Plan { source: o.desc.clone(), target: Some(target), encoder: super::would_reserve(&encoders).map(|e| e.name(codec).to_string()), why: Some(why), sent: None };
        if info {
            return Outcome::Done(info_json(&plan, &o));
        }
        let (_, sent_ms) = o.media(true);
        let mut b = Response::builder()
            .header(header::CONTENT_TYPE, "video/mp4")
            .header(header::CACHE_CONTROL, "no-store")
            .header(header::CONTENT_LENGTH, "0")
            .header("x-as-duration-ms", o.duration_ms.to_string())
            .header("x-as-media-duration-ms", sent_ms.to_string());
        for (k, v) in plan.headers().into_iter().chain(o.clip_headers(true)) {
            b = b.header(k, v);
        }
        return Outcome::Done(b.body(axum::body::Body::empty()).unwrap());
    }
    let all_encoders = encoders.clone();
    // The account's start is counted only once a place on the encoder is had: a busy server does
    // not use up its starts.
    let reserved = super::user_lease(&user).and_then(|l| super::reserve(&encoders).map(|r| (l, r))).filter(|_| super::may_start(&user));
    let Some((lease, (encoders, slot))) = reserved else {
        let (low, plan, sub) = fallback(Why::BusyFallback).await;
        return untouched(low, plan, sub.as_ref().unwrap_or(&o));
    };
    let enc_name = encoders.first().map(|e| e.name(codec).to_string());
    let plan = Plan { source: o.desc.clone(), target: Some(target), encoder: enc_name, why: Some(why), sent: None };
    let clip_headers = o.clip_headers(true);
    let (_, sent_ms) = o.media(true);
    let job = Job {
        input: o.input,
        target,
        software_after: slot.hardware && all_encoders.iter().any(|e| e.hw.is_none()),
        encoders,
        slot,
        lease: Some(lease),
        fps: o.desc.fps.unwrap_or(25.0),
        depth: o.desc.bit_depth,
        seek_ms: o.seek_ms,
        limit_ms: o.limit_ms,
        what: if is_event { "clip" } else { "playback" },
        camera: o.camera,
    };
    let mut b = Response::builder()
        .header(header::CONTENT_TYPE, "video/mp4")
        .header(header::CACHE_CONTROL, "no-store")
        .header("x-as-duration-ms", o.duration_ms.to_string())
        .header("x-as-media-duration-ms", sent_ms.to_string());
    for (k, v) in plan.headers().into_iter().chain(clip_headers) {
        b = b.header(k, v);
    }
    Outcome::Done(b.body(super::file::stream(job)).unwrap())
}
