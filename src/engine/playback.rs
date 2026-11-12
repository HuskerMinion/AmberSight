//! Playback of indexed recordings: timeline coverage, windows assembled into
//! one MP4, and full-resolution stills.

use super::*;
use crate::db::{Db, SegmentRow};
use crate::ingest::splice::{SourceSegment, VirtualClip};
use std::io::Write;
use std::path::PathBuf;

/// Segments whose gap is at most this are drawn as one span.
pub const SPAN_GAP_MS: i64 = 1500;
/// Longest window `/api/playback` assembles.
pub const MAX_WINDOW_MS: i64 = 2 * 60 * 60 * 1000;
/// Fast playback: longest window per request, and the clip's frame spacing.
pub const MAX_TRICK_WINDOW_MS: i64 = 24 * 60 * 60 * 1000;
/// When retention last moved or deleted recordings: planned clips older
/// than that may point at files that are gone, so they are planned again.
static LAST_MOVE: std::sync::Mutex<Option<std::time::Instant>> = std::sync::Mutex::new(None);

/// Retention moved or deleted recordings.
pub fn recordings_moved() {
    *LAST_MOVE.lock().unwrap() = Some(std::time::Instant::now());
}

/// A cached clip still good: under a minute old and planned after the last move.
fn fresh(at: &std::time::Instant) -> bool {
    at.elapsed() < std::time::Duration::from_secs(60) && LAST_MOVE.lock().unwrap().is_none_or(|m| *at > m)
}

/// What the AI saw in each frame: (time ms, objects).
pub type TrackFrames = Vec<(i64, Vec<crate::ai::Detection>)>;

/// The AI track of an event is made in parts this long (see Engine::event_track).
pub const TRACK_PART_MS: i64 = 5000;
/// The stretch of recording an event's AI track covers: from 2 s before the
/// event to 2 s after, at most 3 minutes (ms since the epoch).
pub fn track_window(start_s: i64, end_s: i64) -> (i64, i64) {
    let from = start_s * 1000 - 2000;
    (from, (end_s * 1000 + 2000).min(from + 180_000))
}
/// Longest window one reversed clip covers (it is decoded whole in memory).
pub const MAX_REVERSE_WINDOW_MS: i64 = 10_000;
/// Reversed clips are scaled down to at most this wide (memory, speed).
const REVERSE_MAX_WIDTH: u32 = 960;
pub const TRICK_FRAME_MS: u32 = 100;

/// Merge `[start, end]` intervals (any order) whose gap is <= `gap_ms`.
pub fn merge_spans(mut spans: Vec<(i64, i64)>, gap_ms: i64) -> Vec<(i64, i64)> {
    spans.sort_unstable();
    let mut out: Vec<(i64, i64)> = Vec::new();
    for (s, e) in spans {
        match out.last_mut() {
            Some(last) if s - last.1 <= gap_ms => last.1 = last.1.max(e),
            _ => out.push((s, e)),
        }
    }
    out
}

/// Coverage of a camera's recordings within [from_ms, to_ms].
#[derive(Debug, serde::Serialize)]
pub struct Coverage {
    pub camera: String,
    pub spans: Vec<(i64, i64)>,
    pub segments: usize,
    pub bytes: i64,
}

pub fn coverage(db: &Db, camera: &str, from_ms: i64, to_ms: i64) -> anyhow::Result<Coverage> {
    let rows = db.segments_overlapping(&slug(camera), from_ms, to_ms)?;
    let spans = rows.iter().map(|r| (r.start_ms.max(from_ms), r.end_ms.min(to_ms))).collect();
    Ok(Coverage {
        camera: camera.to_string(),
        spans: merge_spans(spans, SPAN_GAP_MS),
        segments: rows.len(),
        bytes: rows.iter().map(|r| r.bytes).sum(),
    })
}

/// Recordings in the native format, which play straight from the files.
pub fn is_native_kind(kind: &str) -> bool {
    matches!(kind, "continuous" | "triggered" | "sub")
}

/// A camera's native-format recordings over [from_ms, to_ms], for clips;
/// the sub stream's are marked low quality (used only where nothing else
/// was recorded). `resolve` maps an index row to its file.
pub fn sources(db: &Db, camera_slug: &str, from_ms: i64, to_ms: i64, resolve: impl Fn(&SegmentRow) -> PathBuf) -> anyhow::Result<Vec<SourceSegment>> {
    sources_q(db, camera_slug, from_ms, to_ms, false, resolve)
}

/// As [`sources`]; with `small`, the other way round: the sub stream is
/// preferred wherever it was recorded (data saver on a phone).
pub fn sources_q(
    db: &Db,
    camera_slug: &str,
    from_ms: i64,
    to_ms: i64,
    small: bool,
    resolve: impl Fn(&SegmentRow) -> PathBuf,
) -> anyhow::Result<Vec<SourceSegment>> {
    let mut rows: Vec<SegmentRow> = db.segments_overlapping(camera_slug, from_ms, to_ms)?.into_iter().filter(|r| is_native_kind(&r.kind)).collect();
    let small = small && rows.iter().any(|r| r.kind == "sub");
    // The other stream's files are only kept where they fill a gap in the
    // preferred one (each file costs disk seeks; where both were recorded
    // the other copy would never be shown).
    if rows.iter().any(|r| r.kind == "sub") && rows.iter().any(|r| r.kind != "sub") {
        let mut spans: Vec<(i64, i64)> = rows.iter().filter(|r| (r.kind == "sub") == small).map(|r| (r.start_ms, r.end_ms)).collect();
        spans.sort_unstable();
        let mut merged: Vec<(i64, i64)> = Vec::new();
        for (a, b) in spans {
            match merged.last_mut() {
                Some(m) if a <= m.1 + SPAN_GAP_MS => m.1 = m.1.max(b),
                _ => merged.push((a, b)),
            }
        }
        let covered = |a: i64, b: i64| {
            let i = merged.partition_point(|m| m.0 <= a);
            i > 0 && merged[i - 1].1 >= b
        };
        // (Only the part inside the window counts.)
        rows.retain(|r| (r.kind == "sub") == small || !covered(r.start_ms.max(from_ms), r.end_ms.min(to_ms)));
    }
    Ok(rows
        .into_iter()
        .map(|r| {
            let sub = r.kind == "sub";
            SourceSegment { path: resolve(&r), start_ms: r.start_ms, end_ms: r.end_ms, low: if small { !sub } else { sub }, sub }
        })
        .collect())
}

/// Plan a clip over [from_ms, to_ms] from native-format recordings only.
pub fn window_clip(
    db: &Db,
    camera: &str,
    from_ms: i64,
    to_ms: i64,
    resolve: impl Fn(&SegmentRow) -> PathBuf,
) -> anyhow::Result<Option<VirtualClip>> {
    VirtualClip::plan(&sources(db, &slug(camera), from_ms, to_ms, resolve)?, from_ms, to_ms)
}

/// Decode one frame of `clip` at `offset_s` seconds into a JPEG with ffmpeg,
/// optionally scaled to `width` pixels wide.
fn extract_jpeg(clip: VirtualClip, offset_s: f64, width: Option<u32>) -> anyhow::Result<Vec<u8>> {
    use std::process::Stdio;
    let mut cmd = crate::child::ffmpeg();
    cmd.args(["-hide_banner", "-loglevel", "error", "-i", "-"]).args(["-ss", &format!("{offset_s:.3}")]);
    if let Some(w) = width {
        cmd.args(["-vf", &format!("scale={w}:-2")]);
    }
    let mut child = cmd
        .args(["-frames:v", "1", "-f", "image2pipe", "-c:v", "mjpeg", "-q:v", if width.is_some() { "5" } else { "3" }, "-"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|e| anyhow::anyhow!("cannot run ffmpeg: {e}"))?;
    let mut stdin = child.stdin.take().expect("piped stdin");
    // Feed from a thread so a full stdout pipe cannot deadlock us.
    let feeder = std::thread::spawn(move || {
        let len = clip.len();
        let mut pos = 0;
        while pos < len {
            let end = (pos + (1 << 20)).min(len);
            match clip.read_range(pos, end) {
                Ok(b) if stdin.write_all(&b).is_ok() => pos = end,
                _ => break, // ffmpeg closed its input (it has its frame) or read error
            }
        }
    });
    let out = child.wait_with_output()?;
    let _ = feeder.join();
    if out.stdout.is_empty() {
        anyhow::bail!("ffmpeg produced no image");
    }
    Ok(out.stdout)
}

/// Frame at `at_ms` from a clip that starts at the preceding keyframe.
pub fn still_jpeg(
    db: &Db,
    camera: &str,
    at_ms: i64,
    resolve: impl Fn(&SegmentRow) -> PathBuf,
) -> anyhow::Result<Option<Vec<u8>>> {
    let Some(clip) = window_clip(db, camera, at_ms, at_ms + 1000, resolve)? else { return Ok(None) };
    let offset = ((at_ms - clip.start_ms) as f64 / 1000.0).max(0.0);
    extract_jpeg(clip, offset, None).map(Some)
}

/// A filmstrip: small pictures of a camera across [from, to] in one JPEG
/// grid, for scrubbing a timeline without asking for each moment.
pub struct Filmstrip {
    pub jpeg: Vec<u8>,
    /// Wall-clock time of each picture (ms), in grid order.
    pub times: Vec<i64>,
    pub cols: u32,
    pub w: u32,
    pub h: u32,
}

/// At most this many filmstrips are made at once (each is an ffmpeg run).
const FILMSTRIP_JOBS: usize = 2;

/// Playback windows being prepared now. Filmstrips (nice to have) wait while
/// any are: on a hard disk both compete for the same seeks, and a click on
/// the timeline must not wait behind fifteen filmstrips.
pub static PLAYBACK_BUSY: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

/// When a playback window was last asked for (epoch s): background disk
/// work (filling the layout index) waits while someone is watching.
pub static LAST_PLAYBACK: std::sync::atomic::AtomicI64 = std::sync::atomic::AtomicI64::new(0);

/// Note that someone is watching recordings now.
pub fn playback_now() {
    LAST_PLAYBACK.store(chrono::Utc::now().timestamp(), std::sync::atomic::Ordering::Relaxed);
}

fn yield_to_playback() {
    use std::sync::atomic::Ordering;
    let until = std::time::Instant::now() + std::time::Duration::from_secs(5);
    while PLAYBACK_BUSY.load(Ordering::Relaxed) > 0 && std::time::Instant::now() < until {
        std::thread::sleep(std::time::Duration::from_millis(40));
    }
}
pub const MAX_FILMSTRIP_FRAMES: u32 = 240;
const MAX_FILMSTRIP_STEP_MS: i64 = 2 * 3600 * 1000;

/// One picture every `step_ms` (keyframes only, the sub stream when there
/// is one), `w` pixels wide (16:9 cells), laid out 12 to a row.
pub fn filmstrip(
    db: &Db,
    camera: &str,
    from_ms: i64,
    step_ms: i64,
    n: u32,
    w: u32,
    resolve: impl Fn(&SegmentRow) -> PathBuf,
) -> anyhow::Result<Option<Filmstrip>> {
    use std::sync::{Mutex, OnceLock};
    type Key = (String, i64, i64, u32, u32);
    static CACHE: OnceLock<Mutex<std::collections::VecDeque<(Key, std::sync::Arc<Filmstrip>)>>> = OnceLock::new();
    static JOBS: Gate = Gate::new(FILMSTRIP_JOBS);
    let n = n.clamp(1, MAX_FILMSTRIP_FRAMES);
    let w = (w.clamp(64, 320) / 2) * 2;
    let h = ((w * 9 / 16) / 2) * 2;
    // At most a picture every two hours (a strip covers 20 days at most).
    let step_ms = step_ms.clamp(1000, MAX_FILMSTRIP_STEP_MS);
    let from_ms = from_ms.clamp(0, Utc::now().timestamp_millis() + 86_400_000);
    let to_ms = from_ms + step_ms * n as i64;
    let cam = slug(camera);
    let key = (cam.clone(), from_ms, step_ms, n, w);
    let cache = CACHE.get_or_init(Default::default);
    // Spans still being recorded change; only cache what is over.
    let settled = to_ms < Utc::now().timestamp_millis() - 120_000;
    if let Some((_, f)) = cache.lock().unwrap().iter().find(|(k, _)| *k == key) {
        return Ok(Some(Filmstrip { jpeg: f.jpeg.clone(), times: f.times.clone(), cols: f.cols, w: f.w, h: f.h }));
    }
    // Strips are a nice-to-have: a few at a time, and none while a timeline
    // click is being served; busy for long means "not now".
    let Some(_slot) = JOBS.enter(std::time::Duration::from_secs(10)) else { anyhow::bail!("busy") };
    yield_to_playback();
    // The sub stream where recorded: cheap to decode, and the same files a
    // click on the timeline opens first (so they are already read). Only the
    // recording at each step is opened, however wide the strip.
    let all = sources_q(db, &cam, from_ms, to_ms, true, resolve)?;
    let segs = one_per_step(&all, from_ms, step_ms, n);
    const FRAME_MS: u32 = 100;
    let speed = ((step_ms as f64) / FRAME_MS as f64).round().max(1.0) as u32;
    let Some(clip) = VirtualClip::plan_trick(&segs, from_ms, to_ms, speed, FRAME_MS)? else { return Ok(None) };
    let times = clip.frame_times.clone();
    if times.is_empty() {
        return Ok(None);
    }
    let k = times.len() as u32;
    let cols = k.min(12);
    let rows = k.div_ceil(cols);
    yield_to_playback();
    let vf = format!("scale={w}:{h}:force_original_aspect_ratio=decrease,pad={w}:{h}:(ow-iw)/2:(oh-ih)/2,tile={cols}x{rows}");
    let jpeg = run_ffmpeg_jpeg(clip, &vf)?;
    let f = Filmstrip { jpeg, times, cols, w, h };
    if settled {
        let mut c = cache.lock().unwrap();
        if c.len() >= 64 {
            c.pop_front();
        }
        c.push_back((key, std::sync::Arc::new(Filmstrip { jpeg: f.jpeg.clone(), times: f.times.clone(), cols, w, h })));
    }
    Ok(Some(f))
}

/// The recordings (in time order) that hold each step's moment, or the
/// first one starting within that step: one per step at most.
fn one_per_step(segs: &[SourceSegment], from_ms: i64, step_ms: i64, n: u32) -> Vec<SourceSegment> {
    let mut sorted: Vec<&SourceSegment> = segs.iter().collect();
    sorted.sort_by_key(|s| s.start_ms);
    let mut out: Vec<SourceSegment> = Vec::new();
    for i in 0..n as i64 {
        let t = from_ms + i * step_ms;
        // The last recording starting at or before t (it may hold t), else
        // the first one starting before the next step.
        let k = sorted.partition_point(|s| s.start_ms <= t);
        let pick = if k > 0 { Some(sorted[k - 1]) } else { None }
            .filter(|s| k == sorted.len() || t - s.start_ms < 2 * 3600 * 1000)
            .or_else(|| sorted.get(k).copied().filter(|s| s.start_ms < t + step_ms));
        if let Some(s) = pick {
            if !out.iter().any(|o| o.path == s.path) {
                out.push(s.clone());
            }
        }
    }
    out.sort_by_key(|s| s.start_ms);
    out
}

/// At most `max` callers at once; others wait up to a limit, then give up.
pub(crate) struct Gate {
    busy: std::sync::Mutex<usize>,
    cv: std::sync::Condvar,
    max: usize,
}

pub(crate) struct GateSlot<'a>(&'a Gate);

impl Drop for GateSlot<'_> {
    fn drop(&mut self) {
        *self.0.busy.lock().unwrap() -= 1;
        self.0.cv.notify_one();
    }
}

impl Gate {
    pub(crate) const fn new(max: usize) -> Self {
        Gate { busy: std::sync::Mutex::new(0), cv: std::sync::Condvar::new(), max }
    }

    pub(crate) fn enter(&self, wait: std::time::Duration) -> Option<GateSlot<'_>> {
        let until = std::time::Instant::now() + wait;
        let mut busy = self.busy.lock().unwrap();
        while *busy >= self.max {
            let left = until.checked_duration_since(std::time::Instant::now())?;
            busy = self.cv.wait_timeout(busy, left).unwrap().0;
        }
        *busy += 1;
        Some(GateSlot(self))
    }

    /// As `enter`, but gives up as soon as `cancelled` says so (the
    /// browser no longer waits for the result).
    pub(crate) fn enter_unless(&self, wait: std::time::Duration, cancelled: &dyn Fn() -> bool) -> Option<GateSlot<'_>> {
        let until = std::time::Instant::now() + wait;
        let mut busy = self.busy.lock().unwrap();
        while *busy >= self.max {
            if cancelled() {
                return None;
            }
            let left = until.checked_duration_since(std::time::Instant::now())?;
            busy = self.cv.wait_timeout(busy, left.min(std::time::Duration::from_millis(200))).unwrap().0;
        }
        *busy += 1;
        Some(GateSlot(self))
    }
}

/// Feed `clip` to ffmpeg with filter `vf` and return the single JPEG it makes.
fn run_ffmpeg_jpeg(clip: VirtualClip, vf: &str) -> anyhow::Result<Vec<u8>> {
    use std::process::Stdio;
    let mut child = crate::child::ffmpeg()
        .args(["-hide_banner", "-loglevel", "error", "-i", "-", "-vf", vf, "-frames:v", "1", "-f", "image2pipe", "-c:v", "mjpeg", "-q:v", "6", "-"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| anyhow::anyhow!("cannot run ffmpeg: {e}"))?;
    let mut stdin = child.stdin.take().expect("piped stdin");
    let feeder = std::thread::spawn(move || {
        let len = clip.len();
        let mut pos = 0;
        while pos < len {
            let end = (pos + (1 << 20)).min(len);
            match clip.read_range(pos, end) {
                Ok(b) if stdin.write_all(&b).is_ok() => pos = end,
                _ => break,
            }
        }
    });
    let out = child.wait_with_output()?;
    let _ = feeder.join();
    if out.stdout.is_empty() {
        anyhow::bail!("ffmpeg made no picture: {}", String::from_utf8_lossy(&out.stderr).lines().next().unwrap_or(""));
    }
    Ok(out.stdout)
}

/// Feed `clip` to ffmpeg and return it reversed, as an MP4 that starts
/// playing at once (index at the front), video only. Stops ffmpeg (with an
/// error) once `cancelled` says so: nobody is waiting for the result.
fn run_ffmpeg_reverse(clip: VirtualClip, cancelled: &dyn Fn() -> bool) -> anyhow::Result<Vec<u8>> {
    use std::process::Stdio;
    use std::sync::atomic::{AtomicU64, Ordering};
    static N: AtomicU64 = AtomicU64::new(0);
    let tmp = std::env::temp_dir().join(format!("ambersight-rev-{}-{}.mp4", std::process::id(), N.fetch_add(1, Ordering::Relaxed)));
    struct Gone(PathBuf);
    impl Drop for Gone {
        fn drop(&mut self) {
            let _ = std::fs::remove_file(&self.0);
        }
    }
    let _gone = Gone(tmp.clone());
    let vf = format!("scale='min({REVERSE_MAX_WIDTH},iw)':-2,reverse");
    let mut child = crate::child::ffmpeg()
        .args(["-hide_banner", "-loglevel", "error", "-i", "-", "-an", "-vf", &vf])
        .args(["-c:v", "libx264", "-preset", "ultrafast", "-tune", "fastdecode", "-crf", "26", "-g", "30", "-pix_fmt", "yuv420p"])
        .args(["-movflags", "+faststart", "-f", "mp4", "-y"])
        .arg(&tmp)
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| anyhow::anyhow!("cannot run ffmpeg: {e}"))?;
    let mut stdin = child.stdin.take().expect("piped stdin");
    let feeder = std::thread::spawn(move || {
        let len = clip.len();
        let mut pos = 0;
        while pos < len {
            let end = (pos + (1 << 20)).min(len);
            match clip.read_range(pos, end) {
                Ok(b) if stdin.write_all(&b).is_ok() => pos = end,
                _ => break,
            }
        }
    });
    // (Its messages are read on the side so it never stalls on a full pipe.)
    let mut stderr = child.stderr.take().expect("piped stderr");
    let errors = std::thread::spawn(move || {
        let mut b = Vec::new();
        let _ = std::io::Read::read_to_end(&mut stderr, &mut b);
        b
    });
    // Wait for ffmpeg, checking every 50 ms that someone still wants it.
    let status = loop {
        if let Some(st) = child.try_wait()? {
            break st;
        }
        if cancelled() {
            let _ = child.kill();
            let _ = child.wait();
            let _ = feeder.join();
            let _ = errors.join();
            anyhow::bail!("cancelled");
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
    };
    let _ = feeder.join();
    let err = errors.join().unwrap_or_default();
    let bytes = std::fs::read(&tmp).unwrap_or_default();
    if !status.success() || bytes.is_empty() {
        anyhow::bail!("ffmpeg could not reverse the clip: {}", String::from_utf8_lossy(&err).lines().next().unwrap_or(""));
    }
    Ok(bytes)
}

/// Feed `clip` to ffmpeg and hand each of `fps` JPEG frames a second (at
/// most `width` wide) to `each` as soon as it is decoded (the AI works on one
/// while the next decodes). Decoding on the graphics card where it can
/// (Windows D3D11, which also works for a service), else on the CPU.
/// Returns how many frames there were.
fn run_ffmpeg_frames(clip: VirtualClip, fps: u32, width: u32, each: &mut dyn FnMut(usize, &[u8]) -> anyhow::Result<()>) -> anyhow::Result<usize> {
    #[cfg(windows)]
    {
        use std::sync::atomic::{AtomicBool, Ordering};
        static HW_BROKEN: AtomicBool = AtomicBool::new(false);
        if !HW_BROKEN.load(Ordering::Relaxed) {
            match ffmpeg_frames(clip.clone(), fps, width, true, each) {
                Ok(n) => return Ok(n),
                // (Only a start that made nothing: frames already handed on
                // cannot be taken back.)
                Err(e) if e.to_string().starts_with("ffmpeg made no frames") => {
                    tracing::info!("AI track: decoding on the graphics card failed ({e:#}); using the CPU from now on");
                    HW_BROKEN.store(true, Ordering::Relaxed);
                }
                Err(e) => return Err(e),
            }
        }
    }
    ffmpeg_frames(clip, fps, width, false, each)
}

fn ffmpeg_frames(clip: VirtualClip, fps: u32, width: u32, hw: bool, each: &mut dyn FnMut(usize, &[u8]) -> anyhow::Result<()>) -> anyhow::Result<usize> {
    use std::io::Read;
    use std::process::Stdio;
    let vf = format!("fps={fps},scale='min({width},iw)':-2");
    let hwargs: &[&str] = if hw { &["-hwaccel", "d3d11va"] } else { &[] };
    let mut child = crate::child::ffmpeg()
        .args(["-hide_banner", "-loglevel", "error"])
        .args(hwargs)
        .args(["-i", "-", "-an", "-vf", &vf, "-f", "image2pipe", "-c:v", "mjpeg", "-q:v", "5", "-"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| anyhow::anyhow!("cannot run ffmpeg: {e}"))?;
    let mut stdin = child.stdin.take().expect("piped stdin");
    let feeder = std::thread::spawn(move || {
        let len = clip.len();
        let mut pos = 0;
        while pos < len {
            let end = (pos + (1 << 20)).min(len);
            match clip.read_range(pos, end) {
                Ok(b) if stdin.write_all(&b).is_ok() => pos = end,
                _ => break,
            }
        }
    });
    let mut stderr = child.stderr.take().expect("piped stderr");
    let errs = std::thread::spawn(move || {
        let mut s = String::new();
        let _ = stderr.read_to_string(&mut s);
        s
    });
    let mut stdout = child.stdout.take().expect("piped stdout");
    let mut buf: Vec<u8> = Vec::new();
    let mut chunk = vec![0u8; 256 * 1024];
    let mut n = 0usize;
    let mut failed = None;
    loop {
        let got = match stdout.read(&mut chunk) {
            Ok(0) | Err(_) => break,
            Ok(k) => k,
        };
        buf.extend_from_slice(&chunk[..got]);
        // Hand on every whole picture in the buffer.
        let (pics, used) = whole_jpegs(&buf);
        for p in pics {
            if let Err(e) = each(n, p) {
                failed = Some(e);
                break;
            }
            n += 1;
        }
        buf.drain(..used);
        if failed.is_some() {
            let _ = child.kill();
            break;
        }
    }
    let _ = child.wait();
    let _ = feeder.join();
    let err = errs.join().unwrap_or_default();
    if let Some(e) = failed {
        return Err(e);
    }
    if n == 0 {
        anyhow::bail!("ffmpeg made no frames: {}", err.lines().next().unwrap_or(""));
    }
    Ok(n)
}

/// The whole JPEGs at the start of `b` (ffmpeg image2pipe output) and how
/// many bytes they take: each runs from a start marker (FF D8) to an end
/// marker (FF D9); inside the data an FF byte is always followed by 00 or a
/// marker, so FF D9 only ends a picture.
fn whole_jpegs(b: &[u8]) -> (Vec<&[u8]>, usize) {
    let mut out = Vec::new();
    let mut i = 0;
    let mut used = 0;
    while i + 1 < b.len() {
        if b[i] == 0xFF && b[i + 1] == 0xD8 {
            let mut j = i + 2;
            while j + 1 < b.len() && !(b[j] == 0xFF && b[j + 1] == 0xD9) {
                j += 1;
            }
            if j + 1 >= b.len() {
                break;
            }
            out.push(&b[i..j + 2]);
            i = j + 2;
            used = i;
        } else {
            i += 1;
            used = i;
        }
    }
    (out, used)
}


/// Widest preview `/api/preview` makes.
pub const MAX_PREVIEW_WIDTH: u32 = 640;

/// A small picture for hovering or dragging along a timeline: the keyframe
/// at or before `at_ms` (no decoding up to the exact moment), from the sub
/// stream when it was recorded (much cheaper to decode), `width` wide.
/// Returns the picture and the keyframe's time.
pub fn preview_jpeg(
    db: &Db,
    camera: &str,
    at_ms: i64,
    width: u32,
    resolve: impl Fn(&SegmentRow) -> PathBuf,
) -> anyhow::Result<Option<(Vec<u8>, i64)>> {
    use std::sync::{Mutex, OnceLock};
    type Entry = ((String, i64, u32), Vec<u8>);
    static CACHE: OnceLock<Mutex<std::collections::VecDeque<Entry>>> = OnceLock::new();
    const CACHE_LEN: usize = 400;
    static PREVIEWS: Gate = Gate::new(3);
    let width = width.clamp(64, MAX_PREVIEW_WIDTH);
    let at_ms = at_ms.clamp(0, Utc::now().timestamp_millis());
    let cam = slug(camera);
    let segs = sources_q(db, &cam, at_ms, at_ms + 1000, true, resolve)?;
    let Some(clip) = VirtualClip::plan_trick(&segs, at_ms, at_ms + 1000, 1, 100)? else { return Ok(None) };
    let key = (cam, clip.start_ms, width);
    let cache = CACHE.get_or_init(Default::default);
    if let Some((_, jpeg)) = cache.lock().unwrap().iter().find(|(k, _)| *k == key) {
        return Ok(Some((jpeg.clone(), clip.start_ms)));
    }
    let start = clip.start_ms;
    // Each one is an ffmpeg run: a few at a time.
    let Some(_slot) = PREVIEWS.enter(std::time::Duration::from_secs(5)) else { anyhow::bail!("busy") };
    let jpeg = extract_jpeg(clip, 0.0, Some(width))?;
    let mut c = cache.lock().unwrap();
    if c.len() >= CACHE_LEN {
        c.pop_front();
    }
    c.push_back((key, jpeg.clone()));
    Ok(Some((jpeg, start)))
}

impl Engine {
    pub fn recording_coverage(&self, camera: &str, from_ms: i64, to_ms: i64) -> anyhow::Result<Coverage> {
        coverage(&self.db, camera, from_ms, to_ms)
    }

    /// Names of all configured cameras.
    pub fn camera_names(&self) -> Vec<String> {
        self.config.lock().unwrap().cameras.iter().map(|c| c.name.clone()).collect()
    }

    /// `small`: the sub stream wherever it was recorded (data saver).
    /// Kept for a minute: a player asks for the same window several times
    /// (where it starts, then byte ranges as it plays).
    pub fn playback_clip(&self, camera: &str, from_ms: i64, to_ms: i64, small: bool) -> anyhow::Result<Option<VirtualClip>> {
        use std::sync::{Mutex, OnceLock};
        type Key = (String, i64, i64, bool);
        static CACHE: OnceLock<Mutex<Vec<(Key, std::time::Instant, VirtualClip)>>> = OnceLock::new();
        let cache = CACHE.get_or_init(Default::default);
        let key = (camera.to_string(), from_ms, to_ms, small);
        {
            let mut c = cache.lock().unwrap();
            c.retain(|(_, at, _)| fresh(at));
            if let Some((_, _, clip)) = c.iter().find(|(k, _, _)| *k == key) {
                return Ok(Some(clip.clone()));
            }
        }
        let t0 = std::time::Instant::now();
        struct Busy;
        impl Drop for Busy {
            fn drop(&mut self) {
                PLAYBACK_BUSY.fetch_sub(1, std::sync::atomic::Ordering::Relaxed);
            }
        }
        PLAYBACK_BUSY.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let _busy = Busy;
        // Where this window's clip ended when it was last planned (kept
        // for half an hour after its last use): a browser still playing it
        // asks for more byte ranges, and a window still being recorded has
        // grown since. Planning it to the old end gives the very same
        // bytes; the browser asks for what came after from that end.
        type Layouts = Mutex<std::collections::VecDeque<(Key, i64, std::time::Instant)>>;
        static LAYOUTS: OnceLock<Layouts> = OnceLock::new();
        const LAYOUT_KEEP: std::time::Duration = std::time::Duration::from_secs(30 * 60);
        let layouts = LAYOUTS.get_or_init(Default::default);
        let earlier_end = {
            let mut l = layouts.lock().unwrap();
            l.retain(|(_, _, at)| at.elapsed() < LAYOUT_KEEP);
            l.iter().find(|(k, _, _)| *k == key).map(|(_, end, _)| *end)
        };
        let segs = sources_q(&self.db, &slug(camera), from_ms, to_ms, small, |r| self.segment_path(r))?;
        let clip = VirtualClip::plan_like(&segs, from_ms, to_ms, earlier_end)?;
        let took = t0.elapsed();
        if took > std::time::Duration::from_secs(1) {
            info!("playback: {camera}: {} recordings took {} ms to open", segs.len(), took.as_millis());
        }
        if let Some(c) = &clip {
            {
                let mut l = layouts.lock().unwrap();
                l.retain(|(k, _, _)| *k != key);
                if l.len() >= 256 {
                    l.pop_front();
                }
                l.push_back((key.clone(), c.end_ms, std::time::Instant::now()));
            }
            let mut cc = cache.lock().unwrap();
            if cc.len() >= 32 {
                cc.remove(0);
            }
            cc.push((key, std::time::Instant::now(), c.clone()));
        }
        Ok(clip)
    }

    /// The window [from_ms, to_ms] played backwards: browsers cannot play
    /// video in reverse, so ffmpeg decodes the window (the sub stream where
    /// it was recorded, scaled down) and encodes it in reverse order. The
    /// clip's time 0 is the window's end (`end_ms`). Kept for a minute.
    /// `cancelled`: the browser gave up on it; then ffmpeg is not started
    /// (or is stopped) and an error comes back.
    pub fn reverse_clip(&self, camera: &str, from_ms: i64, to_ms: i64, cancelled: &dyn Fn() -> bool) -> anyhow::Result<Option<VirtualClip>> {
        use std::sync::{Mutex, OnceLock};
        type Key = (String, i64, i64);
        static CACHE: OnceLock<Mutex<Vec<(Key, std::time::Instant, VirtualClip)>>> = OnceLock::new();
        static JOBS: Gate = Gate::new(2);
        let to_ms = to_ms.min(from_ms + MAX_REVERSE_WINDOW_MS);
        let cache = CACHE.get_or_init(Default::default);
        let key = (camera.to_string(), from_ms, to_ms);
        {
            let mut c = cache.lock().unwrap();
            c.retain(|(_, at, _)| fresh(at));
            if let Some((_, _, clip)) = c.iter().find(|(k, _, _)| *k == key) {
                return Ok(Some(clip.clone()));
            }
        }
        let Some(fwd) = self.playback_clip(camera, from_ms, to_ms, true)? else { return Ok(None) };
        let Some(_slot) = JOBS.enter_unless(std::time::Duration::from_secs(15), cancelled) else {
            anyhow::bail!(if cancelled() { "cancelled" } else { "busy" })
        };
        if cancelled() {
            anyhow::bail!("cancelled");
        }
        let (start, end, sub) = (fwd.start_ms, fwd.end_ms, fwd.sub);
        let t0 = std::time::Instant::now();
        let bytes = run_ffmpeg_reverse(fwd, cancelled)?;
        if t0.elapsed() > std::time::Duration::from_millis(700) {
            info!("playback: {camera}: reversing {} s took {} ms{}", (end - start) / 1000, t0.elapsed().as_millis(), if sub { " (sub stream)" } else { "" });
        }
        let clip = VirtualClip::from_bytes(bytes, start, end, sub);
        let mut c = cache.lock().unwrap();
        if c.len() >= 16 {
            c.remove(0);
        }
        c.push((key, std::time::Instant::now(), clip.clone()));
        Ok(Some(clip))
    }

    /// What the AI sees through part `part` (TRACK_PART_MS long) of an
    /// event's recording, 3 frames a second (the AI's own checks stop once it
    /// has confirmed the event, so they cover only its first moments):
    /// (parts in the event, (time ms, objects) per frame). In parts so the
    /// moment being watched is ready in a second or two. Kept in memory for
    /// the most recent.
    pub fn event_track(&self, id: &str, part: u32) -> anyhow::Result<Option<(u32, TrackFrames)>> {
        use std::sync::{Arc, Mutex, OnceLock};
        type Track = Arc<(u32, TrackFrames)>;
        type Cache = Mutex<std::collections::VecDeque<((String, u32), Track)>>;
        static CACHE: OnceLock<Cache> = OnceLock::new();
        static JOBS: Gate = Gate::new(1);
        const FPS: i64 = 3;
        let cache = CACHE.get_or_init(Default::default);
        let key = (id.to_string(), part);
        let cached = || cache.lock().unwrap().iter().find(|(k, _)| *k == key).map(|(_, t)| (t.0, t.1.clone()));
        if let Some(t) = cached() {
            return Ok(Some(t));
        }
        let Some(ev) = self.db.get_event(id)? else { return Ok(None) };
        let (from, to) = track_window(ev.start, ev.end);
        let parts = ((to - from + TRACK_PART_MS - 1) / TRACK_PART_MS) as u32;
        if part >= parts {
            return Ok(Some((parts, Vec::new())));
        }
        let (pf, pt) = (from + part as i64 * TRACK_PART_MS, (from + (part as i64 + 1) * TRACK_PART_MS).min(to));
        let Some(_slot) = JOBS.enter(std::time::Duration::from_secs(30)) else { anyhow::bail!("busy") };
        // (Another request may have made it while this one waited.)
        if let Some(t) = cached() {
            return Ok(Some(t));
        }
        // On a graphics card: the full-resolution recording, looked at whole
        // and in tiles (people far away are a few pixels once the picture
        // is scaled to the model's input). Else the sub stream, whole.
        let tiled = self.detector().filter(|d| d.gpu_name().is_some());
        let Some(clip) = self.playback_clip(&ev.camera, pf, pt, tiled.is_none())? else { return Ok(Some((parts, Vec::new()))) };
        let start = clip.start_ms;
        let t0 = std::time::Instant::now();
        let mut out = Vec::new();
        let mut ai_time = std::time::Duration::ZERO;
        run_ffmpeg_frames(clip, FPS as u32, if tiled.is_some() { 1920 } else { 640 }, &mut |n, jpeg| {
            // The clip starts at the keyframe before the part: only its own frames.
            let t = start + (n as i64 * 1000) / FPS;
            if t < pf || t >= pt {
                return Ok(());
            }
            let a = std::time::Instant::now();
            let seen = match &tiled {
                Some(d) => {
                    let img = image::load_from_memory(jpeg).map_err(|e| anyhow::anyhow!("frame: {e}"))?.to_rgb8();
                    d.detect_tiled(&img, 0.3)?
                }
                None => self.ai_detect(jpeg, 0.3)?,
            };
            ai_time += a.elapsed();
            out.push((t, seen));
            Ok(())
        })?;
        if t0.elapsed() > std::time::Duration::from_secs(3) {
            info!(
                "AI track: {}: part {part}: {} frames took {} ms (AI {} ms){}",
                ev.camera,
                out.len(),
                t0.elapsed().as_millis(),
                ai_time.as_millis(),
                if tiled.is_some() { ", full resolution, tiled" } else { "" }
            );
        }
        let t: Track = Arc::new((parts, out));
        let mut c = cache.lock().unwrap();
        if c.len() >= 400 {
            c.pop_front();
        }
        c.push_back((key, t.clone()));
        Ok(Some((t.0, t.1.clone())))
    }

    /// Keyframe-only fast-playback clip (see `VirtualClip::plan_trick`). Planned
    /// clips are kept for a minute: the browser asks for many byte ranges.
    pub fn trick_clip(&self, camera: &str, from_ms: i64, to_ms: i64, speed: u32, frame_ms: u32, small: bool, cancelled: &dyn Fn() -> bool) -> anyhow::Result<Option<VirtualClip>> {
        use std::sync::{Mutex, OnceLock};
        type Key = (String, i64, i64, u32, u32, bool);
        static CACHE: OnceLock<Mutex<Vec<(Key, std::time::Instant, VirtualClip)>>> = OnceLock::new();
        let cache = CACHE.get_or_init(Default::default);
        let key = (camera.to_string(), from_ms, to_ms, speed, frame_ms, small);
        {
            let mut c = cache.lock().unwrap();
            c.retain(|(_, at, _)| fresh(at));
            if let Some((_, _, clip)) = c.iter().find(|(k, _, _)| *k == key) {
                return Ok(Some(clip.clone()));
            }
        }
        let t0 = std::time::Instant::now();
        let st0 = crate::ingest::splice::LOAD_STATS.get();
        let segs = sources_q(&self.db, &slug(camera), from_ms, to_ms, small, |r| self.segment_path(r))?;
        let listed = t0.elapsed();
        let clip = VirtualClip::plan_trick_until(&segs, from_ms, to_ms, speed, frame_ms, cancelled)?;
        let took = t0.elapsed();
        if took > std::time::Duration::from_millis(500) {
            let st = crate::ingest::splice::LOAD_STATS.get();
            info!(
                "playback: {camera}: fast clip ({speed}x, {frame_ms} ms a picture, {} recordings) took {} ms (finding them {} ms; all cameras meanwhile: {} from the index in {} ms, {} read from disk in {} ms)",
                segs.len(),
                took.as_millis(),
                listed.as_millis(),
                st.0 - st0.0,
                st.1 - st0.1,
                st.2 - st0.2,
                st.3 - st0.3
            );
        }
        if let Some(c) = &clip {
            let mut cc = cache.lock().unwrap();
            if cc.len() >= 16 {
                cc.remove(0);
            }
            cc.push((key, std::time::Instant::now(), c.clone()));
        }
        Ok(clip)
    }

    /// The keyframe on screen at `at_ms` (fast playback shows these one by
    /// one, as fast as they arrive). `small`: the sub stream where recorded.
    pub fn picture_at(&self, camera: &str, at_ms: i64, small: bool) -> anyhow::Result<Option<crate::ingest::splice::KeyPicture>> {
        let segs = sources_q(&self.db, &slug(camera), at_ms.saturating_sub(20_000), at_ms.saturating_add(1), small, |r| self.segment_path(r))?;
        crate::ingest::splice::key_picture(&segs, at_ms)
    }

    pub fn filmstrip(&self, camera: &str, from_ms: i64, step_ms: i64, n: u32, w: u32) -> anyhow::Result<Option<Filmstrip>> {
        filmstrip(&self.db, camera, from_ms, step_ms, n, w, |r| self.segment_path(r))
    }

    pub fn preview_at(&self, camera: &str, at_ms: i64, width: u32) -> anyhow::Result<Option<(Vec<u8>, i64)>> {
        preview_jpeg(&self.db, camera, at_ms, width, |r| self.segment_path(r))
    }

    pub fn still_at(&self, camera: &str, at_ms: i64) -> anyhow::Result<Option<Vec<u8>>> {
        still_jpeg(&self.db, camera, at_ms, |r| self.segment_path(r))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn jpeg_streams_split_into_pictures() {
        let a = [0xFF, 0xD8, 1, 0xFF, 0x00, 2, 0xFF, 0xD9];
        let b = [0xFF, 0xD8, 3, 0xFF, 0xD9];
        let joined: Vec<u8> = a.iter().chain(b.iter()).copied().chain([0xFF, 0xD8, 9]).collect();
        let (parts, used) = whole_jpegs(&joined);
        assert_eq!(parts, vec![&a[..], &b[..]], "a torn last picture waits for the rest");
        assert_eq!(used, a.len() + b.len());
    }
    use crate::ingest::fmp4::{TrackInfo, TrackKind};
    use crate::ingest::recorder::SegmentWriter;

    #[test]
    fn the_other_stream_fills_gaps_unless_one_covers_the_window() {
        let dir = std::env::temp_dir().join(format!("amber-sources-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let db = Db::open(&dir.join("t.db")).unwrap();
        let row = |path: &str, a: i64, b: i64, kind: &str| SegmentRow { path: path.into(), start_ms: a, end_ms: b, bytes: 1, video_frames: 1, kind: kind.into(), folder: String::new() };
        // Main 0-10 s; the sub-stream copy only from 5 s on.
        db.insert_segment("cam", &row("m1.mp4", 0, 10_000, "continuous")).unwrap();
        db.insert_segment("cam", &row("s1.mp4", 5_000, 15_000, "sub")).unwrap();
        let names = |v: Vec<SourceSegment>| v.iter().map(|s| (s.path.file_name().unwrap().to_string_lossy().into_owned(), s.low)).collect::<Vec<_>>();
        let resolve = |r: &SegmentRow| dir.join(&r.path);
        // Data saver over 0-15 s: the sub stream preferred, main fills 0-5 s.
        assert_eq!(names(sources_q(&db, "cam", 0, 15_000, true, resolve).unwrap()), vec![("m1.mp4".into(), true), ("s1.mp4".into(), false)]);
        // 6-9 s: the sub stream covers it, main is not opened.
        assert_eq!(names(sources_q(&db, "cam", 6_000, 9_000, true, resolve).unwrap()), vec![("s1.mp4".into(), false)]);
        // Full quality over 6-9 s: main covers it, the sub copy is not opened.
        assert_eq!(names(sources_q(&db, "cam", 6_000, 9_000, false, resolve).unwrap()), vec![("m1.mp4".into(), false)]);
        // Full quality over 0-15 s: main preferred, sub fills 10-15 s.
        assert_eq!(names(sources_q(&db, "cam", 0, 15_000, false, resolve).unwrap()), vec![("m1.mp4".into(), false), ("s1.mp4".into(), true)]);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn merge_spans_cases() {
        assert!(merge_spans(vec![], 1500).is_empty());
        // gap exactly at threshold merges, above does not
        assert_eq!(merge_spans(vec![(0, 1000), (2500, 3000)], 1500), vec![(0, 3000)]);
        assert_eq!(merge_spans(vec![(0, 1000), (2501, 3000)], 1500), vec![(0, 1000), (2501, 3000)]);
        assert_eq!(merge_spans(vec![(0, 1000), (1200, 3000)], 1500), vec![(0, 3000)]);
        // unsorted, and a contained span does not shrink the merged end
        assert_eq!(
            merge_spans(vec![(9000, 9500), (0, 5000), (100, 200), (20000, 21000)], 1500),
            vec![(0, 5000), (9000, 9500), (20000, 21000)]
        );
    }

    #[test]
    fn window_across_two_segments_is_keyframe_aligned() {
        let dir = std::env::temp_dir().join(format!("amber-playback-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let mut entry = vec![0, 0, 0, 16];
        entry.extend_from_slice(b"avc1");
        entry.extend_from_slice(&[0; 8]);
        let track = TrackInfo { kind: TrackKind::Video, timescale: 90000, sample_entry: entry, width: 64, height: 48 };
        let mut w = SegmentWriter::new(&dir, "front_door-1", vec![track], 3).unwrap();
        for i in 0..100i64 {
            w.push_video(i * 9000, vec![(i % 251) as u8; 40], i % 10 == 0).unwrap();
        }
        w.finish().unwrap();
        let closed = w.take_closed();
        assert!(closed.len() >= 3);

        let db = Db::open(&dir.join("t.db")).unwrap();
        let mut t0 = 0;
        for (i, s) in closed.iter().enumerate() {
            if i == 0 {
                t0 = s.start_ms;
            }
            let name = s.path.file_name().unwrap().to_string_lossy().into_owned();
            db.insert_segment(
                "front_door",
                &SegmentRow {
                    path: name,
                    start_ms: s.start_ms,
                    end_ms: s.end_ms,
                    bytes: std::fs::metadata(&s.path).unwrap().len() as i64,
                    video_frames: 0,
                    kind: "continuous".into(),
                    folder: String::new(),
                },
            )
            .unwrap();
        }
        let resolve = |r: &SegmentRow| dir.join(&r.path);

        let cov = coverage(&db, "Front Door", t0 - 5000, t0 + 60_000).unwrap();
        assert_eq!(cov.segments, closed.len());
        assert_eq!(cov.spans.len(), 1);
        assert!(cov.bytes > 0);

        // 2.5 s .. 6.2 s crosses the 3 s and 6 s segment boundaries.
        let clip = window_clip(&db, "Front Door", t0 + 2500, t0 + 6200, resolve).unwrap().unwrap();
        assert_eq!(clip.start_ms - t0, 2000);
        assert_eq!(clip.end_ms - t0, 7000);
        assert!(window_clip(&db, "Front Door", t0 + 90_000, t0 + 95_000, resolve).unwrap().is_none());
        assert!(window_clip(&db, "Other", t0, t0 + 5000, resolve).unwrap().is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
