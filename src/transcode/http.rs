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
    /// The recording may still grow (a window or clip ending about now): its
    /// converted answer is not kept, since a later request would differ.
    growing: bool,
}

/// Past the time a recording's last piece is saved (a segment's length after
/// its end), this much more before it counts as complete.
const STILL_GROWING_MARGIN_MS: i64 = 15_000;

/// Whether what ends at `end_ms` (a window's `to`, an event's end with its
/// post-roll) may still be recorded: not yet `segment_ms` (the time a
/// recording is saved by) and a margin ago.
fn may_grow(end_ms: i64, segment_ms: i64) -> bool {
    let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_millis() as i64).unwrap_or(0);
    end_ms.saturating_add(segment_ms).saturating_add(STILL_GROWING_MARGIN_MS) >= now
}

fn segment_ms(engine: &Engine) -> i64 {
    engine.config.lock().unwrap().storage.segment_seconds as i64 * 1000
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
                growing: may_grow(*to, segment_ms(engine)) || may_grow(clip.end_ms, segment_ms(engine)),
                input: Input::Clip(clip),
                desc,
                camera: camera.clone(),
            }))
        }
        ClipSrc::Event { id, offset } => {
            let Some(ev) = engine.db.get_event(id)? else { return Ok(None) };
            // Still open or being recorded: its clip runs to the end of the post-roll.
            let post = engine.config.lock().unwrap().motion_for(&ev.camera).post_roll as i64;
            let ev_growing = may_grow((ev.end + post) * 1000, segment_ms(engine));
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
                    growing: ev_growing,
                }));
            }
            let Some(clip) = engine.virtual_clip_q(&ev, low)? else { return Ok(None) };
            let (start_ms, end_ms, sub) = (clip.start_ms, clip.end_ms, clip.sub);
            let duration_ms = end_ms - start_ms;
            let Some(desc) = probe_clip(&clip) else { return Ok(None) };
            let off = (*offset).max(0).min(duration_ms.max(0));
            let growing = ev_growing || may_grow(end_ms, segment_ms(engine));
            let mut o = Opened { input: Input::Clip(clip), seek_ms: off, limit_ms: None, start_ms, duration_ms, exact_start_ms: start_ms + off, untouched_start_ms: start_ms, end_ms, sub, desc, camera: ev.camera.clone(), growing };
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
    /// A Range other than `bytes=0-` and `bytes=0-0` (`bytes=N-` or
    /// `bytes=N-M`; None: another form). A kept copy of the converted answer
    /// (see `cache`) is read from. With none: a Range from byte 0 (`bytes=0-1`,
    /// as AVPlayer sends) starts the conversion and is answered from its new
    /// copy; from further in, or when the answer is not kept (the recording
    /// may still grow), it is 416 when the answer would be converted. An
    /// untouched answer is sent as usual.
    Resume(Option<(u64, Option<u64>)>),
}

impl Ask {
    fn range(self) -> Option<(u64, Option<u64>)> {
        match self {
            Ask::Resume(r) => r,
            _ => None,
        }
    }

    /// From byte 0: may start a conversion.
    fn starts(self) -> bool {
        matches!(self, Ask::Start | Ask::Resume(Some((0, _))))
    }
}

/// Handle a request with `caps` (None: `info=1` alone, what is sent as it
/// is). `info`: answer JSON.
pub async fn handle(engine: Arc<Engine>, src: ClipSrc, caps: Option<Caps>, info: bool, ask: Ask, low: bool, user: String) -> Outcome {
    let probe = ask == Ask::Headers;
    super::set_config(&engine.config.lock().unwrap().transcode);
    // A converted answer kept from before (this account, this recording, these caps): a repeat
    // or a Range is read from the copy; it needs no encoder and no place.
    let cache = super::cache::global().filter(|_| caps.is_some() && !info && !probe);
    let ckey = cache.map(|c| {
        c.set_limits(super::cache::Limits::from_config(&super::config()));
        cache_key(&user, &src, caps.as_ref().unwrap(), low)
    });
    // Found, or claimed: a second request for the same answer at the same
    // time waits for this one and reads its copy (one conversion, not two).
    let mut claim = None;
    if let (Some(c), Some(k)) = (cache, &ckey) {
        match c.lookup(k, ask.range(), ask.starts()).await {
            super::cache::Lookup::Found(e) => return Outcome::Done(serve(&e, ask).await),
            super::cache::Lookup::Claim(cl) => claim = Some(cl),
            super::cache::Lookup::Miss => {}
        }
    }
    let (e, s) = (engine.clone(), src.clone());
    let opened = tokio::task::spawn_blocking(move || open(&e, &s, low)).await;
    let o = match opened {
        Ok(Ok(Some(o))) => o,
        Ok(Ok(None)) => return Outcome::Done((StatusCode::NOT_FOUND, "nothing recorded there").into_response()),
        Ok(Err(e)) => return Outcome::Done(crate::web::internal_error("transcode", &e)),
        Err(_) => return Outcome::Done((StatusCode::INTERNAL_SERVER_ERROR, "transcode error").into_response()),
    };
    let mut ask = ask;
    if o.growing {
        // A later request would get more: nothing is kept, and a Range from further in is
        // 416. A Range from byte 0 (AVPlayer's bytes=0-1) gets the whole answer, streamed:
        // a 200 that ignores the Range is valid HTTP, and the player can play.
        claim = None;
        if ask.starts() {
            ask = Ask::Start;
        }
    }
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
    let busy = if !super::usable(&target) {
        // Too small to be worth making: the best there is instead.
        Some(why)
    } else if encoders.is_none() {
        // The test encodes have not finished yet.
        Some(Why::BusyFallback)
    } else if encoders.as_ref().is_some_and(|e| e.is_empty()) {
        Some(Why::Off)
    } else if matches!(ask, Ask::Resume(_)) && claim.is_none() {
        // This answer would be converted, a conversion has no byte positions
        // and no copy is kept to answer from. Sending the untouched file's
        // bytes from that offset would break a player that began on the
        // converted stream (it would read another file's bytes), so 416: the
        // player fails cleanly and the app asks again with start=.
        return Outcome::Done(no_byte_positions());
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
        src_codec: format!("{:?}", o.desc.codec),
        seek_ms: o.seek_ms,
        media_ms: Some(sent_ms),
        limit_ms: o.limit_ms,
        what: if is_event { "clip" } else { "playback" },
        camera: o.camera,
    };
    let first_wait = super::file::first_byte_wait(&job);
    let mut hs: Vec<(String, String)> = vec![
        ("content-type".into(), "video/mp4".into()),
        ("cache-control".into(), "no-store".into()),
        ("accept-ranges".into(), "none".into()),
        ("x-as-duration-ms".into(), o.duration_ms.to_string()),
        ("x-as-media-duration-ms".into(), sent_ms.to_string()),
    ];
    hs.extend(plan.headers().into_iter().chain(clip_headers).map(|(k, v)| (k.to_string(), v)));
    // Written to a kept copy as it is made; this answer reads from it too.
    let source = match &src {
        ClipSrc::Event { id, .. } => super::cache::event_source(id),
        ClipSrc::Window { camera, .. } => format!("camera:{camera}"),
    };
    if let Some(entry) = claim.and_then(|cl| cl.create(hs.clone(), &source)) {
        entry.set_first_wait(first_wait);
        super::file::start_cached(job, entry.clone());
        return Outcome::Done(serve(&entry, ask).await);
    }
    if matches!(ask, Ask::Resume(_)) {
        // No room for a copy: a Range cannot be answered from a stream.
        return Outcome::Done(no_byte_positions());
    }
    let mut b = Response::builder();
    for (k, v) in hs {
        b = b.header(k, v);
    }
    Outcome::Done(b.body(super::file::stream(job)).unwrap())
}

/// Everything that changes the converted output, hashed: the account, the recording, the
/// caps (in a fixed order), the quality.
fn cache_key(user: &str, src: &ClipSrc, caps: &Caps, low: bool) -> String {
    let what = match src {
        ClipSrc::Window { camera, from, to, start } => format!("w|{camera}|{from}|{to}|{start}"),
        ClipSrc::Event { id, offset } => format!("e|{id}|{offset}"),
    };
    let mut c: Vec<String> = caps.0.iter().map(|c| format!("{:?}", c)).collect();
    c.sort();
    super::cache::key(&[user, &what, &c.join(";"), if low { "low" } else { "main" }])
}

fn no_byte_positions() -> Response {
    Response::builder()
        .status(StatusCode::RANGE_NOT_SATISFIABLE)
        .header(header::CONTENT_RANGE, "bytes */*")
        .header(header::CACHE_CONTROL, "no-store")
        .header("x-as-transcoded", "1")
        .body(axum::body::Body::from("this video is converted for the device and cannot be resumed by bytes: ask again with start="))
        .unwrap()
}

fn range_not_satisfiable(total: Option<u64>) -> Response {
    Response::builder()
        .status(StatusCode::RANGE_NOT_SATISFIABLE)
        .header(header::CONTENT_RANGE, total.map(|t| format!("bytes */{t}")).unwrap_or_else(|| "bytes */*".into()))
        .header(header::CACHE_CONTROL, "no-store")
        .header("x-as-transcoded", "1")
        .body(axum::body::Body::from("this converted video cannot be resumed from there: ask again with start="))
        .unwrap()
}

/// Answer from a kept copy, with the converted answer's own headers (and
/// `Accept-Ranges: bytes`: ranges are answered from it).
///
/// A Range that begins inside what is written is a 206. While the conversion still runs the
/// size is not known, so an open-ended Range is answered `Content-Range: bytes N-*/*` with
/// no Content-Length (chunked), and the body follows the writer to its end. That form is
/// not valid HTTP, but players that read until the connection ends ignore a Content-Range
/// they cannot use (ExoPlayer takes the length as unknown) and need the bytes now, not after
/// the whole window is converted. A Range first waits for its first byte (a bounded Range
/// `N-M`: for byte M), counted as a reader all the while: up to every try's start-up time
/// while nothing is written yet, else 30 s. A bounded Range whose byte M came in time is
/// `bytes N-M/*` with Content-Length; when not, `bytes N-*/*` with no length, and the body
/// still ends after byte M (or earlier, if the conversion ends first). Once it is done:
/// `bytes N-M/total` and Content-Length.
async fn serve(e: &std::sync::Arc<super::cache::Entry>, ask: Ask) -> Response {
    use super::cache::Status;
    let (from, to, ranged) = match ask {
        Ask::Resume(Some((f, t))) => (f, t, true),
        Ask::Resume(None) => return range_not_satisfiable(None),
        _ => (0, None, false),
    };
    // A bounded range waits for its last byte (else its first): then the answer's size is
    // known. Not past what the conversion runs ahead of this reader (it would wait out the
    // whole time): further, the headers go without a length and the body streams on.
    let need = if ranged { to.unwrap_or(from).min(from.saturating_add(super::cache::AHEAD - 1)) } else { 0 };
    // Counted as a reader while it waits (the conversion is neither given up nor held back
    // short of where this request starts), and the hold passes to the body. Its first byte,
    // not its end: `bytes=N-<huge>` must not lift the pacing.
    let hold = e.hold(from);
    let p = if ranged { e.wait_past(need, e.wait_limit(e.progress().written)).await } else { e.progress() };
    let mut b = Response::builder();
    for (k, v) in e.headers.iter().filter(|(k, _)| !k.eq_ignore_ascii_case("accept-ranges")) {
        b = b.header(k.as_str(), v.as_str());
    }
    b = b.header(header::ACCEPT_RANGES, "bytes");
    let (status, cr, len, end) = match p.status {
        Status::Done => {
            if from >= p.written && (ranged || p.written > 0) {
                return range_not_satisfiable(Some(p.written));
            }
            let end = to.map(|t| t.saturating_add(1)).unwrap_or(u64::MAX).min(p.written);
            if ranged {
                (StatusCode::PARTIAL_CONTENT, Some(format!("bytes {from}-{}/{}", end - 1, p.written)), Some(end - from), Some(end))
            } else {
                (StatusCode::OK, None, Some(p.written), Some(p.written))
            }
        }
        _ if ranged && from >= p.written => return range_not_satisfiable(None),
        _ if ranged => match to {
            Some(t) if p.written > t => (StatusCode::PARTIAL_CONTENT, Some(format!("bytes {from}-{t}/*")), Some(t - from + 1), Some(t.saturating_add(1))),
            _ => (StatusCode::PARTIAL_CONTENT, Some(format!("bytes {from}-*/*")), None, to.map(|t| t.saturating_add(1))),
        },
        _ => (StatusCode::OK, None, None, None),
    };
    b = b.status(status);
    if let Some(cr) = cr {
        b = b.header(header::CONTENT_RANGE, cr);
    }
    if let Some(l) = len {
        b = b.header(header::CONTENT_LENGTH, l.to_string());
    }
    b.body(e.read_held(hold, from, end)).unwrap()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::transcode::cache::{Cache, Limits};
    use std::time::Duration;

    async fn body(r: Response) -> Vec<u8> {
        axum::body::to_bytes(r.into_body(), usize::MAX).await.unwrap_or_default().to_vec()
    }

    fn hdr(r: &Response, k: &str) -> Option<String> {
        r.headers().get(k).map(|v| v.to_str().unwrap().to_string())
    }

    #[tokio::test]
    async fn ranges_are_answered_from_the_kept_copy() {
        let dir = std::env::temp_dir().join(format!("amber-tcache-http-{}", std::process::id()));
        let c = Cache::new(dir.clone(), Limits { entry_max: 1 << 20, total_max: 1 << 20, expire: Duration::from_secs(60) });
        let hs = vec![("content-type".to_string(), "video/mp4".to_string()), ("x-as-transcoded".to_string(), "1".to_string()), ("x-clip-start-ms".to_string(), "5".to_string())];
        let e = c.create(&crate::transcode::cache::key(&["u"]), hs).unwrap();
        let data: Vec<u8> = (0..100u8).collect();
        assert!(e.write(&data[..40]));

        // Mid-conversion: an open range inside what is written is a 206 without a length.
        let r = serve(&e, Ask::Resume(Some((10, None)))).await;
        assert_eq!(r.status(), StatusCode::PARTIAL_CONTENT);
        assert_eq!(hdr(&r, "content-range").as_deref(), Some("bytes 10-*/*"));
        assert!(hdr(&r, "content-length").is_none());
        assert_eq!(hdr(&r, "x-as-transcoded").as_deref(), Some("1"));
        assert_eq!(hdr(&r, "x-clip-start-ms").as_deref(), Some("5"));
        // It waits for the rest, and ends when the conversion does.
        let e2 = e.clone();
        let d2 = data.clone();
        let w = tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(100)).await;
            e2.write(&d2[40..]);
            e2.finish();
        });
        assert_eq!(body(r).await, data[10..].to_vec());
        w.await.unwrap();

        // Finished: real Content-Range and length.
        let r = serve(&e, Ask::Resume(Some((90, None)))).await;
        assert_eq!(r.status(), StatusCode::PARTIAL_CONTENT);
        assert_eq!(hdr(&r, "content-range").as_deref(), Some("bytes 90-99/100"));
        assert_eq!(hdr(&r, "content-length").as_deref(), Some("10"));
        assert_eq!(body(r).await, data[90..].to_vec());
        let r = serve(&e, Ask::Resume(Some((20, Some(29))))).await;
        assert_eq!(hdr(&r, "content-range").as_deref(), Some("bytes 20-29/100"));
        assert_eq!(body(r).await, data[20..30].to_vec());
        // Past the end: 416 with the size.
        let r = serve(&e, Ask::Resume(Some((100, None)))).await;
        assert_eq!(r.status(), StatusCode::RANGE_NOT_SATISFIABLE);
        assert_eq!(hdr(&r, "content-range").as_deref(), Some("bytes */100"));
        // A repeat from the start: 200 with the length.
        let r = serve(&e, Ask::Start).await;
        assert_eq!((r.status(), hdr(&r, "content-length").as_deref()), (StatusCode::OK, Some("100")));
        assert_eq!(body(r).await, data);
        // A form not supported.
        assert_eq!(serve(&e, Ask::Resume(None)).await.status(), StatusCode::RANGE_NOT_SATISFIABLE);

        // A failed conversion: past what was written is 416 at once, not a hang.
        let f = c.create(&crate::transcode::cache::key(&["f"]), vec![]).unwrap();
        f.write(&data[..10]);
        f.fail();
        let t0 = std::time::Instant::now();
        let r = serve(&f, Ask::Resume(Some((50, None)))).await;
        assert_eq!(r.status(), StatusCode::RANGE_NOT_SATISFIABLE);
        assert!(t0.elapsed() < Duration::from_secs(2));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn answers_from_the_copy_take_ranges_and_never_wrap() {
        let dir = std::env::temp_dir().join(format!("amber-tcache-http2-{}", std::process::id()));
        let c = Cache::new(dir.clone(), Limits { entry_max: 1 << 20, total_max: 1 << 20, expire: Duration::from_secs(60) });
        let hs = vec![("accept-ranges".to_string(), "none".to_string()), ("x-as-transcoded".to_string(), "1".to_string())];
        let e = c.create(&crate::transcode::cache::key(&["r"]), hs).unwrap();
        let data: Vec<u8> = (0..50u8).collect();
        assert!(e.write(&data));
        // Answered from the copy: ranges work, and it says so.
        let r = serve(&e, Ask::Start).await;
        assert_eq!(hdr(&r, "accept-ranges").as_deref(), Some("bytes"));
        assert_eq!(r.headers().get_all("accept-ranges").iter().count(), 1);
        // A bounded range still being written, its end there: 206 with a length.
        let r = serve(&e, Ask::Resume(Some((0, Some(1))))).await;
        assert_eq!((r.status(), hdr(&r, "content-range").as_deref(), hdr(&r, "content-length").as_deref()), (StatusCode::PARTIAL_CONTENT, Some("bytes 0-1/*"), Some("2")));
        assert_eq!(body(r).await, vec![0, 1]);
        // The largest end there is: no wrap to 0 (the body runs to the end).
        let r = serve(&e, Ask::Resume(Some((10, Some(u64::MAX))))).await;
        assert_eq!((r.status(), hdr(&r, "content-range").as_deref()), (StatusCode::PARTIAL_CONTENT, Some("bytes 10-*/*")));
        let e2 = e.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(50)).await;
            e2.finish();
        });
        assert_eq!(body(r).await, data[10..].to_vec());
        let r = serve(&e, Ask::Resume(Some((10, Some(u64::MAX))))).await;
        assert_eq!(hdr(&r, "content-range").as_deref(), Some("bytes 10-49/50"));
        assert_eq!(body(r).await, data[10..].to_vec());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn a_range_waiting_for_its_bytes_keeps_the_conversion() {
        let dir = std::env::temp_dir().join(format!("amber-tcache-http3-{}", std::process::id()));
        let c = Cache::new(dir.clone(), Limits { entry_max: 1 << 20, total_max: 1 << 20, expire: Duration::from_secs(60) });
        let e = c.create(&crate::transcode::cache::key(&["w"]), vec![]).unwrap();
        e.set_first_wait(Duration::from_secs(60));
        // AVPlayer's first request while the first try is slow to start.
        let e2 = e.clone();
        let r = tokio::spawn(async move { serve(&e2, Ask::Resume(Some((0, Some(1))))).await });
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert!(!r.is_finished());
        assert!(!e.abandoned(Duration::ZERO), "a reader while it waits");
        let e3 = e.clone();
        tokio::task::spawn_blocking(move || assert!(e3.write(b"abc"))).await.unwrap();
        let r = r.await.unwrap();
        assert_eq!(r.status(), StatusCode::PARTIAL_CONTENT);
        assert!(!e.abandoned(Duration::ZERO), "the hold passed to the body");
        assert_eq!(body(r).await, b"ab".to_vec());
        assert!(e.abandoned(Duration::ZERO), "and ends with it");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn a_huge_range_end_does_not_lift_the_pacing() {
        let dir = std::env::temp_dir().join(format!("amber-tcache-http4-{}", std::process::id()));
        let c = Cache::new(dir.clone(), Limits { entry_max: 1 << 30, total_max: 1 << 30, expire: Duration::from_secs(60) });
        let e = c.create(&crate::transcode::cache::key(&["p"]), vec![]).unwrap();
        assert!(e.write(&[0; 10]));
        let r = serve(&e, Ask::Resume(Some((5, Some(u64::MAX))))).await;
        assert_eq!(r.status(), StatusCode::PARTIAL_CONTENT);
        assert_eq!(e.wanted(), 5, "from its first byte, not its end");
        drop(r);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn a_far_bounded_end_does_not_wait_out_the_time() {
        use crate::transcode::cache::AHEAD;
        let dir = std::env::temp_dir().join(format!("amber-tcache-http5-{}", std::process::id()));
        let c = Cache::new(dir.clone(), Limits { entry_max: 1 << 40, total_max: 1 << 40, expire: Duration::from_secs(60) });
        let e = c.create(&crate::transcode::cache::key(&["b"]), vec![]).unwrap();
        assert!(e.write(&[0; 10]));
        // bytes=5-(5 + 2 * AHEAD): answered once the writer is as far as pacing lets it
        // (5 + AHEAD), not after the wait.
        let e2 = e.clone();
        let r = tokio::spawn(async move { serve(&e2, Ask::Resume(Some((5, Some(5 + 2 * AHEAD))))).await });
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert!(!r.is_finished(), "waits for the bytes it can get");
        let e3 = e.clone();
        tokio::task::spawn_blocking(move || {
            let chunk = vec![0u8; 1 << 20];
            // Ends exactly where the pacing holds the writer (from + AHEAD).
            assert!(e3.write(&vec![0u8; (5 + AHEAD - 10) as usize % (1 << 20)]));
            while e3.progress().written < 5 + AHEAD {
                assert!(e3.write(&chunk));
            }
            assert_eq!(e3.progress().written, 5 + AHEAD);
        })
        .await
        .unwrap();
        let t0 = std::time::Instant::now();
        let r = tokio::time::timeout(Duration::from_secs(5), r).await.expect("answered without waiting out the time").unwrap();
        assert!(t0.elapsed() < Duration::from_secs(5));
        assert_eq!((r.status(), hdr(&r, "content-range").as_deref()), (StatusCode::PARTIAL_CONTENT, Some("bytes 5-*/*")));
        e.fail();
        drop(r);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn which_requests_start_a_conversion_and_which_sources_are_kept() {
        assert!(Ask::Start.starts());
        assert!(Ask::Resume(Some((0, Some(1)))).starts(), "AVPlayer's bytes=0-1");
        assert!(!Ask::Resume(Some((1304, None))).starts());
        assert!(!Ask::Resume(None).starts() && !Ask::Headers.starts());
        assert_eq!(Ask::Resume(Some((5, Some(9)))).range(), Some((5, Some(9))));
        assert_eq!(Ask::Start.range(), None);
        let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_millis() as i64;
        assert!(may_grow(now + 60_000, 10_000), "ends in the future");
        assert!(may_grow(now - 20_000, 10_000), "ended moments ago: its last segment may not be saved yet");
        assert!(!may_grow(now - 30_000, 10_000), "past a segment and the margin: kept");
        assert!(may_grow(now - 60_000, 60_000), "long segments wait longer");
    }

    #[test]
    fn the_key_ignores_caps_order_and_separates_accounts() {
        let a = Caps::parse("h264:1920x1080,hevc:3840x2160@30").unwrap();
        let b = Caps::parse("hevc:3840x2160@30,h264:1920x1080").unwrap();
        let s = ClipSrc::Event { id: "e1".into(), offset: 0 };
        assert_eq!(cache_key("bob", &s, &a, false), cache_key("bob", &s, &b, false));
        assert_ne!(cache_key("bob", &s, &a, false), cache_key("amy", &s, &a, false));
        assert_ne!(cache_key("bob", &s, &a, false), cache_key("bob", &s, &a, true));
    }
}
