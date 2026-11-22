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
/// The description of an assembled clip (its first fragments).
pub(super) fn probe_clip(c: &crate::ingest::splice::VirtualClip) -> Option<mp4::VideoDesc> {
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
#[allow(clippy::too_many_arguments)]
/// `apple`: the request is from an Apple player (AVFoundation): a converted
/// answer goes only once complete, with its real size (see [`serve`]).
pub async fn handle(engine: Arc<Engine>, src: ClipSrc, caps: Option<Caps>, info: bool, ask: Ask, low: bool, user: String, apple: bool) -> Outcome {
    let probe = ask == Ask::Headers;
    super::set_config(&engine.config.lock().unwrap().transcode);
    // A converted answer kept from before (this account, this recording, these caps): a repeat
    // or a Range is read from the copy; it needs no encoder and no place.
    let cache = super::cache::global().filter(|_| caps.is_some() && !info && !probe);
    let ckey = cache.map(|c| {
        c.set_limits(super::cache::Limits::from_config(&super::config()));
        cache_key(&user, &src, &caps.clone().unwrap_or_default(), low, apple)
    });
    // Found, or claimed: a second request for the same answer at the same
    // time waits for this one and reads its copy (one conversion, not two).
    let mut claim = None;
    if let (Some(c), Some(k)) = (cache, &ckey) {
        // (An Apple player's Range from further in is only ever answered from
        // the copy it began on: never made again, which need not give the same
        // bytes, and never the recording's own bytes.)
        let may_start = ask.starts();
        let mut found = c.lookup(k, ask.range(), may_start).await;
        if let super::cache::Lookup::Found(e) = &found {
            // A conversion that failed (or was given up) is made again for an
            // Apple player, which can only play a whole one.
            if apple && e.progress().status == super::cache::Status::Failed {
                c.forget(e);
                found = c.lookup(k, ask.range(), may_start).await;
            }
        }
        match found {
            super::cache::Lookup::Found(e) => return Outcome::Done(serve(&e, ask, apple).await),
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
    // (Apple's players need the whole answer's size, so theirs is kept and
    // answered once made, growing or not: each of their Ranges then reads
    // the same copy. Theirs are kept apart from other players' answers (see
    // `cache_key`), so no one else is given a copy of a recording that has
    // grown since.)
    if o.growing && !apple {
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
    let decided = caps.as_ref().and_then(|c| super::decide(c, &o.desc));
    let Some((target, why)) = decided else {
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
    // Busy for now (the encoders not yet tested, or no place on them): an
    // Apple player's Range is told to ask again. The other reasons hold for
    // good, and the answer sent instead (as it is) is the same every time.
    let mut transient = false;
    let busy = if super::is_unconverted(&user) {
        // (The demo account: never converted, as when the encoders are busy.)
        Some(Why::BusyFallback)
    } else if !super::usable(&target) {
        // Too small to be worth making: the best there is instead.
        Some(why)
    } else if encoders.is_none() {
        // The test encodes have not finished yet.
        transient = true;
        Some(Why::BusyFallback)
    } else if encoders.as_ref().is_some_and(|e| e.is_empty()) {
        Some(Why::Off)
    } else if apple && !info && !probe && !super::cache::global().is_some_and(|c| c.keeps()) {
        // No copies kept at all: an Apple player could not be answered whole
        // and then by Ranges from the same copy; the recording as it is, the
        // same each time, is sent instead.
        Some(Why::BusyFallback)
    } else if apple && matches!(ask, Ask::Resume(None)) {
        return Outcome::Done(range_not_satisfiable(None));
    } else if apple && matches!(ask, Ask::Resume(_)) && claim.is_none() {
        // From byte 0: another request is making it (this one waited, then
        // missed). Further in: the copy it began on is gone. Ask again.
        return Outcome::Done(self::busy("this video is being converted for the device; ask again", 2));
    } else if matches!(ask, Ask::Resume(_)) && claim.is_none() && !apple {
        // This answer would be converted, a conversion has no byte positions
        // and no copy is kept to answer from. Sending the untouched file's
        // bytes from that offset would break a player that began on the
        // converted stream (it would read another file's bytes), so 416: the
        // player fails cleanly and the app asks again with start=.
        return Outcome::Done(no_byte_positions());
    } else if !super::user_has_room(&user) || encoders.as_deref().and_then(super::would_reserve).is_none() {
        transient = true;
        Some(Why::BusyFallback)
    } else {
        None
    };
    let encoders = encoders.unwrap_or_default();
    if let Some(w) = busy {
        if apple && transient && !info && !probe {
            // (An Apple player is never sent the answer as it is while busy
            // for now: a later request could get the converted copy and mix
            // the two. Busy for good, it is sent as it is every time.)
            return Outcome::Done(self::busy("the encoders are busy; ask again", 2));
        }
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
        if apple {
            return Outcome::Done(self::busy("the encoders are busy; ask again", 2));
        }
        let (low, plan, sub) = fallback(Why::BusyFallback).await;
        return untouched(low, plan, sub.as_ref().unwrap_or(&o));
    };
    let enc_name = encoders.first().map(|e| e.name(codec).to_string());
    let plan = Plan { source: o.desc.clone(), target: Some(target), encoder: enc_name, why: Some(why), sent: None };
    let clip_headers = o.clip_headers(true);
    let (_, sent_ms) = o.media(true);
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
    let entry = claim.and_then(|cl| cl.create(hs.clone(), &source));
    if apple && entry.is_none() {
        // No room for a copy now (every kept answer in use): busy for now, so
        // ask again (sent as it is, a later try could get the converted copy
        // and mix the two).
        drop((lease, encoders, slot));
        return Outcome::Done(self::busy("the server is busy converting; ask again", 2));
    }
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
        caption: None,
        download: None,
        subs: None,
        keep_times: false,
    };
    if let Some(entry) = entry {
        entry.set_first_wait(super::file::first_byte_wait(&job));
        if apple {
            entry.keep();
        }
        super::file::start_cached(job, entry.clone());
        return Outcome::Done(serve(&entry, ask, apple).await);
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

/// Where a download's conversion reads: (ms into the clip to start at, ms of
/// video to make), for the window `from`..`to` of a clip that runs
/// `clip_start`..`clip_end` (it starts at the keyframe before `from`).
fn download_cut(clip_start: i64, clip_end: i64, from: i64, to: i64) -> (i64, i64) {
    let a = from.clamp(clip_start, clip_end);
    let b = to.clamp(a, clip_end);
    (a - clip_start, b - a)
}

/// How long an Apple player's request waits for its conversion to finish.
const APPLE_WHOLE_WAIT: std::time::Duration = std::time::Duration::from_secs(120);

/// Busy: 503, and when to ask again.
fn busy(why: &'static str, retry_s: u32) -> Response {
    (StatusCode::SERVICE_UNAVAILABLE, [(header::RETRY_AFTER, retry_s.to_string())], why).into_response()
}

/// What else a download carries besides fitting the device.
#[derive(Default)]
pub struct DownloadOpts {
    /// Burn the camera's text (its `overlay_text`) into the picture.
    pub text: Option<crate::config::CameraConfig>,
}

impl DownloadOpts {
    /// Something must be burned in: the download is converted even when it
    /// fits the device.
    pub fn burns(&self) -> bool {
        self.text.is_some()
    }
}

/// A download fitted to the device (`download=1` with `caps`): when the
/// window does not fit the device, it is converted to the codec and size
/// `decide` picks, the camera and time burned into the picture (one encode).
/// With text to burn in (`opts`), it is always converted (to the
/// recording's own codec and size when it fits the device). A converted
/// download also has the time subtitle track.
/// `Untouched`: it fits and nothing is burned in, or it cannot be converted
/// here (conversion off, too small): the caller sends
/// today's download with these headers.
/// `offset_s`: the local time zone's offset from UTC (seconds) at an epoch
/// ms, for the captions' clock.
#[allow(clippy::too_many_arguments)]
pub async fn download(engine: Arc<Engine>, camera: String, from: i64, to: i64, caps: Option<Caps>, ask: Ask, low: bool, user: String, offset_s: fn(i64) -> i64, opts: DownloadOpts) -> Outcome {
    super::set_config(&engine.config.lock().unwrap().transcode);
    let (e, cam) = (engine.clone(), camera.clone());
    let opened = tokio::task::spawn_blocking(move || e.playback_clip(&cam, from, to, low).map(|c| c.and_then(|c| probe_clip(&c).map(|d| (c, d))))).await;
    let (clip, desc) = match opened {
        Ok(Ok(Some(o))) => o,
        // (Nothing there: the caller answers as it does today.)
        Ok(Ok(None)) => return Outcome::Untouched { low, headers: Vec::new() },
        Ok(Err(e)) => return Outcome::Done(crate::web::internal_error("download", &e)),
        Err(_) => return Outcome::Done((StatusCode::INTERNAL_SERVER_ERROR, "download error").into_response()),
    };
    let as_is = |why: Option<Why>| Outcome::Untouched { low, headers: Plan { source: desc.clone(), target: None, encoder: None, why, sent: None }.headers() };
    // Fitted to the device (with the camera and time burned in, as always), or only
    // something burned in: the recording's own codec and size.
    let fitted = caps.as_ref().and_then(|c| super::decide(c, &desc)).filter(|(t, _)| super::usable(t) || !opts.burns());
    let (target, why, fit) = match fitted {
        Some((t, w)) if !super::usable(&t) => return as_is(Some(w)),
        Some((t, w)) => (t, w, true),
        None if opts.burns() => (super::burn::forced_target(&desc), Why::Text, false),
        None => return as_is(None),
    };
    let codec = target.codec;
    let encoders = match tokio::task::spawn_blocking(move || super::encoders_for(codec)).await.ok().flatten() {
        None => return Outcome::Done(busy("the video encoders are still being checked; try again in a few seconds", 3)),
        Some(v) if v.is_empty() => return as_is(Some(Why::Off)),
        Some(v) => v,
    };
    let (seek_ms, media_ms) = download_cut(clip.start_ms, clip.end_ms, from, to);
    let start = clip.start_ms + seek_ms;
    let mut hs: Vec<(String, String)> = vec![
        ("content-type".into(), "video/mp4".into()),
        ("content-disposition".into(), format!("attachment; filename=\"{}-{start}.mp4\"", crate::config::slug(&camera))),
        ("cache-control".into(), "no-store".into()),
        ("accept-ranges".into(), "none".into()),
        ("x-clip-start-ms".into(), start.to_string()),
        ("x-clip-end-ms".into(), (start + media_ms).to_string()),
        ("x-clip-sub".into(), (clip.sub as u8).to_string()),
        ("x-as-media-duration-ms".into(), media_ms.to_string()),
    ];
    let response = |hs: Vec<(String, String)>, body: axum::body::Body| {
        let mut b = Response::builder();
        for (k, v) in hs {
            b = b.header(k, v);
        }
        b.body(body).unwrap()
    };
    match ask {
        Ask::Headers => {
            let plan = Plan { source: desc.clone(), target: Some(target), encoder: super::would_reserve(&encoders).map(|e| e.name(codec).to_string()), why: Some(why), sent: None };
            hs.extend(plan.headers().into_iter().map(|(k, v)| (k.to_string(), v)));
            hs.push(("content-length".into(), "0".into()));
            return Outcome::Done(response(hs, axum::body::Body::empty()));
        }
        Ask::Resume(r) if r.is_none_or(|(a, _)| a > 0) => return Outcome::Done(no_byte_positions()),
        _ => {}
    }
    // Two at a time on the server, within the account's limit and the encoders' places; the
    // account's start is counted only once it has them all.
    let Some(hold) = super::reserve_download() else { return Outcome::Done(busy("two downloads are being converted already; try again shortly", 10)) };
    let Some(lease) = super::user_lease(&user) else { return Outcome::Done(busy("this account is converting as many videos as it may; try again shortly", 10)) };
    let all_encoders = encoders.clone();
    let Some((encoders, slot)) = super::reserve(&encoders) else { return Outcome::Done(busy("the video encoders are busy; try again shortly", 10)) };
    if !super::may_start(&user) {
        return Outcome::Done(busy("too many conversions started in the last minute; try again shortly", 20));
    }
    let plan = Plan { source: desc.clone(), target: Some(target), encoder: encoders.first().map(|e| e.name(codec).to_string()), why: Some(why), sent: None };
    hs.extend(plan.headers().into_iter().map(|(k, v)| (k.to_string(), v)));
    // The captions' clock: ffmpeg counts each frame's time from the first frame
    // sent (`start`: the seek is a trim filter before the burned-in text).
    // (One base per stretch of one UTC offset: see clock_text.)
    let clock = |make: &dyn Fn(f64) -> String| crate::engine::clock_text(start, media_ms, &offset_s, make);
    let caption = [fit.then(|| clock(&|b| crate::engine::caption_drawtext(&camera, b))), opts.text.as_ref().map(|c| clock(&|b| crate::engine::overlay_download_drawtext(c, b)))].into_iter().flatten().collect::<Vec<_>>().join(",");
    let subs = super::burn::subs_file(&engine, start, media_ms, &camera);
    let job = Job {
        target,
        software_after: slot.hardware && all_encoders.iter().any(|e| e.hw.is_none()),
        encoders,
        slot,
        lease: Some(lease),
        fps: desc.fps.unwrap_or(25.0),
        depth: desc.bit_depth,
        src_codec: format!("{:?}", desc.codec),
        seek_ms,
        media_ms: Some(media_ms),
        limit_ms: Some(media_ms),
        what: "download",
        camera,
        caption: (!caption.is_empty()).then_some(caption),
        download: Some(hold),
        input: Input::Clip(clip),
        subs,
        keep_times: false,
    };
    Outcome::Done(response(hs, super::file::stream(job)))
}

/// An event clip downloaded with something burned in (`opts.burns()`): the
/// camera's text, and the time subtitle track.
pub async fn download_event(engine: Arc<Engine>, id: String, user: String, opts: DownloadOpts) -> Response {
    let e = engine.clone();
    let id2 = id.clone();
    let text = opts.text.clone();
    let built = tokio::task::spawn_blocking(move || -> anyhow::Result<Option<super::burn::Burn>> {
        let Some(ev) = e.db.get_event(&id2)? else { return Ok(None) };
        super::burn::event_burn(&e, &ev, text.as_ref())
    })
    .await;
    let b = match built {
        Ok(Ok(Some(b))) => b,
        Ok(Ok(None)) => return (StatusCode::NOT_FOUND, "clip not found").into_response(),
        Ok(Err(e)) => return crate::web::internal_error("clip", &e),
        Err(_) => return (StatusCode::INTERNAL_SERVER_ERROR, "clip error").into_response(),
    };
    let b = super::burn::Burn { download: true, ..b };
    let mut hs = vec![("content-disposition".to_string(), format!("attachment; filename=\"{id}.mp4\""))];
    if let Some(ms) = b.media_ms {
        hs.push(("x-as-media-duration-ms".into(), ms.to_string()));
    }
    super::burn::stream(b, &user, hs).await
}

/// Everything that changes the converted output, hashed: the account, the recording, the
/// caps (in a fixed order), the quality; and whether it is for an Apple player, whose
/// answers are kept apart (made whole first, kept even of a recording still growing).
fn cache_key(user: &str, src: &ClipSrc, caps: &Caps, low: bool, apple: bool) -> String {
    let what = match src {
        ClipSrc::Window { camera, from, to, start } => format!("w|{camera}|{from}|{to}|{start}"),
        ClipSrc::Event { id, offset } => format!("e|{id}|{offset}"),
    };
    let mut c: Vec<String> = caps.0.iter().map(|c| format!("{:?}", c)).collect();
    c.sort();
    super::cache::key(&[user, &what, &c.join(";"), if low { "low" } else { "main" }, if apple { "apple" } else { "" }])
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
///
/// `apple`: Apple's players (AVFoundation) cannot play a ranged answer whose
/// total size is not given (they never load it), nor a 200 that ignores
/// their Range: they are answered once the conversion is done, as from a
/// finished file, with the conversion not held back meanwhile.
async fn serve(e: &std::sync::Arc<super::cache::Entry>, ask: Ask, apple: bool) -> Response {
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
    let hold = e.hold(if apple { u64::MAX / 2 } else { from });
    let p = if apple {
        let p = e.wait_past(u64::MAX, APPLE_WHOLE_WAIT).await;
        if p.status != Status::Done {
            return busy("still converting this video for the device", 2);
        }
        p
    } else if ranged {
        e.wait_past(need, e.wait_limit(e.progress().written)).await
    } else {
        e.progress()
    };
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
mod download_tests {
    use super::download_cut;

    #[test]
    fn a_download_is_cut_to_the_window_asked_for() {
        // The clip starts at the keyframe 1.5 s before the window.
        assert_eq!(download_cut(10_000, 80_000, 11_500, 71_500), (1_500, 60_000));
        // A window running past the recording: up to its end.
        assert_eq!(download_cut(10_000, 40_000, 11_500, 71_500), (1_500, 28_500));
        // One starting before it: from its start.
        assert_eq!(download_cut(10_000, 40_000, 5_000, 20_000), (0, 10_000));
        assert_eq!(download_cut(10_000, 40_000, 50_000, 60_000), (30_000, 0));
    }
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
    async fn apple_players_get_a_conversion_once_it_is_whole_with_its_size() {
        let dir = std::env::temp_dir().join(format!("amber-tcache-apple-{}", std::process::id()));
        let c = Cache::new(dir.clone(), Limits { entry_max: 1 << 20, total_max: 1 << 20, expire: Duration::from_secs(60) });
        let hs = vec![("content-type".to_string(), "video/mp4".to_string()), ("accept-ranges".to_string(), "none".to_string())];
        let data: Vec<u8> = (0..100u8).collect();
        let finish_later = |e: std::sync::Arc<crate::transcode::cache::Entry>, d: Vec<u8>| {
            // In 10-byte writes, held back by the pacing unless a reader wants it all.
            tokio::task::spawn_blocking(move || {
                for ch in d.chunks(10) {
                    std::thread::sleep(Duration::from_millis(5));
                    assert!(e.write(ch));
                }
                e.finish();
            })
        };
        // AVPlayer's first probe, mid-conversion: answered when done, with the total.
        let e = c.create(&crate::transcode::cache::key(&["a"]), hs.clone()).unwrap();
        e.set_ahead(10);
        let w = finish_later(e.clone(), data.clone());
        let r = serve(&e, Ask::Resume(Some((0, Some(1)))), true).await;
        w.await.unwrap();
        assert_eq!(r.status(), StatusCode::PARTIAL_CONTENT);
        assert_eq!(hdr(&r, "content-range").as_deref(), Some("bytes 0-1/100"));
        assert_eq!(hdr(&r, "content-length").as_deref(), Some("2"));
        assert_eq!(hdr(&r, "accept-ranges").as_deref(), Some("bytes"));
        assert_eq!(body(r).await, vec![0, 1]);
        // Then the rest, and the whole: from the same copy.
        let r = serve(&e, Ask::Resume(Some((2, None))), true).await;
        assert_eq!((hdr(&r, "content-range").as_deref(), hdr(&r, "content-length").as_deref()), (Some("bytes 2-99/100"), Some("98")));
        assert_eq!(body(r).await, data[2..].to_vec());
        let r = serve(&e, Ask::Start, true).await;
        assert_eq!((r.status(), hdr(&r, "content-length").as_deref()), (StatusCode::OK, Some("100")));
        // A conversion that fails: no half answer.
        let f = c.create(&crate::transcode::cache::key(&["b"]), hs.clone()).unwrap();
        assert!(f.write(&data[..20]));
        f.fail();
        assert_eq!(serve(&f, Ask::Resume(Some((0, Some(1)))), true).await.status(), StatusCode::SERVICE_UNAVAILABLE);
        let _ = std::fs::remove_dir_all(&dir);
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
        let r = serve(&e, Ask::Resume(Some((10, None))), false).await;
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
        let r = serve(&e, Ask::Resume(Some((90, None))), false).await;
        assert_eq!(r.status(), StatusCode::PARTIAL_CONTENT);
        assert_eq!(hdr(&r, "content-range").as_deref(), Some("bytes 90-99/100"));
        assert_eq!(hdr(&r, "content-length").as_deref(), Some("10"));
        assert_eq!(body(r).await, data[90..].to_vec());
        let r = serve(&e, Ask::Resume(Some((20, Some(29)))), false).await;
        assert_eq!(hdr(&r, "content-range").as_deref(), Some("bytes 20-29/100"));
        assert_eq!(body(r).await, data[20..30].to_vec());
        // Past the end: 416 with the size.
        let r = serve(&e, Ask::Resume(Some((100, None))), false).await;
        assert_eq!(r.status(), StatusCode::RANGE_NOT_SATISFIABLE);
        assert_eq!(hdr(&r, "content-range").as_deref(), Some("bytes */100"));
        // A repeat from the start: 200 with the length.
        let r = serve(&e, Ask::Start, false).await;
        assert_eq!((r.status(), hdr(&r, "content-length").as_deref()), (StatusCode::OK, Some("100")));
        assert_eq!(body(r).await, data);
        // A form not supported.
        assert_eq!(serve(&e, Ask::Resume(None), false).await.status(), StatusCode::RANGE_NOT_SATISFIABLE);

        // A failed conversion: past what was written is 416 at once, not a hang.
        let f = c.create(&crate::transcode::cache::key(&["f"]), vec![]).unwrap();
        f.write(&data[..10]);
        f.fail();
        let t0 = std::time::Instant::now();
        let r = serve(&f, Ask::Resume(Some((50, None))), false).await;
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
        let r = serve(&e, Ask::Start, false).await;
        assert_eq!(hdr(&r, "accept-ranges").as_deref(), Some("bytes"));
        assert_eq!(r.headers().get_all("accept-ranges").iter().count(), 1);
        // A bounded range still being written, its end there: 206 with a length.
        let r = serve(&e, Ask::Resume(Some((0, Some(1)))), false).await;
        assert_eq!((r.status(), hdr(&r, "content-range").as_deref(), hdr(&r, "content-length").as_deref()), (StatusCode::PARTIAL_CONTENT, Some("bytes 0-1/*"), Some("2")));
        assert_eq!(body(r).await, vec![0, 1]);
        // The largest end there is: no wrap to 0 (the body runs to the end).
        let r = serve(&e, Ask::Resume(Some((10, Some(u64::MAX)))), false).await;
        assert_eq!((r.status(), hdr(&r, "content-range").as_deref()), (StatusCode::PARTIAL_CONTENT, Some("bytes 10-*/*")));
        let e2 = e.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(50)).await;
            e2.finish();
        });
        assert_eq!(body(r).await, data[10..].to_vec());
        let r = serve(&e, Ask::Resume(Some((10, Some(u64::MAX)))), false).await;
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
        let r = tokio::spawn(async move { serve(&e2, Ask::Resume(Some((0, Some(1)))), false).await });
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
        let r = serve(&e, Ask::Resume(Some((5, Some(u64::MAX)))), false).await;
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
        let r = tokio::spawn(async move { serve(&e2, Ask::Resume(Some((5, Some(5 + 2 * AHEAD)))), false).await });
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
        assert_eq!(cache_key("bob", &s, &a, false, false), cache_key("bob", &s, &b, false, false));
        assert_ne!(cache_key("bob", &s, &a, false, false), cache_key("amy", &s, &a, false, false));
        assert_ne!(cache_key("bob", &s, &a, false, false), cache_key("bob", &s, &a, true, false));
        assert_ne!(cache_key("bob", &s, &a, false, false), cache_key("bob", &s, &a, false, true), "Apple players' answers are kept apart");
    }
}
