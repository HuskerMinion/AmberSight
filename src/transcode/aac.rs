//! Downloads whose sound is FLAC (cameras with G.711 sound are recorded with
//! FLAC): the sound converted to AAC by ffmpeg, the video and the time
//! subtitles copied, as a plain MP4 (`+faststart`). QuickTime (and every
//! player of Apple's), Windows' own players and many phones do not play
//! FLAC in MP4. The sound is small (8 or 16 kHz, one channel), so this takes
//! a moment even for a long window.
//!
//! The file is made in `<data_dir>/tmp` and sent from there with its length,
//! removed once it has been sent (or the download stopped). When it cannot
//! be made (no ffmpeg, ffmpeg failed, too busy), the caller sends the plain
//! MP4 with the FLAC header fixed as before.

use crate::ingest::splice::VirtualClip;
use axum::body::Body;
use axum::http::{header, HeaderValue, StatusCode};
use axum::response::Response;
use std::io::Write;
use std::process::Stdio;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

/// What is converted.
pub enum Src {
    /// Assembled from the recordings (fragmented MP4): piped to ffmpeg, as
    /// a plain MP4 (see [`VirtualClip::for_apple`]).
    Clip(VirtualClip),
    /// A saved clip file.
    File(std::path::PathBuf),
}

impl Src {
    /// Whether its sound is FLAC (only those are converted).
    pub fn has_flac(&self) -> bool {
        match self {
            Src::Clip(c) => c.has_flac(),
            Src::File(p) => crate::ingest::mp4read::file_has_flac(p).unwrap_or(false),
        }
    }

    fn bytes(&self) -> u64 {
        match self {
            Src::Clip(c) => c.len(),
            Src::File(p) => std::fs::metadata(p).map(|m| m.len()).unwrap_or(0),
        }
    }
}

/// The ffmpeg arguments: `file` (None: the clip on stdin) to `out`, video
/// and subtitles copied, every sound track to AAC, a plain MP4 with its
/// header first. `limit_ms`: it ends there (ffmpeg keeps the frames to the
/// next keyframe past the end of a clip's edit list).
pub fn args(file: Option<&std::path::Path>, out: &std::path::Path, limit_ms: Option<i64>) -> Vec<String> {
    let mut a: Vec<String> = ["-hide_banner", "-loglevel", "error", "-nostdin", "-y"].iter().map(|s| s.to_string()).collect();
    match file {
        Some(p) => a.extend(["-i".to_string(), p.to_string_lossy().into_owned()]),
        None => a.extend(["-f", "mp4", "-i", "pipe:0"].iter().map(|s| s.to_string())),
    }
    if let Some(l) = limit_ms.filter(|l| *l > 0) {
        a.extend(["-t".to_string(), format!("{:.3}", l as f64 / 1000.0)]);
    }
    a.extend(
        ["-map", "0:v", "-map", "0:a?", "-map", "0:s?", "-c:v", "copy", "-c:s", "copy", "-c:a", "aac", "-b:a", "64k", "-movflags", "+faststart", "-f", "mp4"]
            .iter()
            .map(|s| s.to_string()),
    );
    a.push(out.to_string_lossy().into_owned());
    a
}

/// How long a conversion may take: a minute, and a second for every 2 MB
/// of the clip (the video is only copied).
pub fn time_limit(bytes: u64) -> std::time::Duration {
    std::time::Duration::from_secs(60 + bytes / (2 << 20))
}

/// Make `out` from `src` (see the module). Blocking. `cancelled` is asked
/// as it runs: true (the download was stopped) ends ffmpeg. Err: no ffmpeg,
/// it failed, it took longer than [`time_limit`], or it was stopped; `out`
/// is then incomplete (the caller removes it).
pub fn convert(src: &Src, out: &std::path::Path, cancelled: &dyn Fn() -> bool) -> std::io::Result<()> {
    let limit = time_limit(src.bytes());
    let (file, clip) = match src {
        Src::File(p) => (Some(p.as_path()), None),
        // Laid out as a plain MP4 first: ffmpeg follows a fragmented file's
        // edit lists only in part, and a download's are where it is cut to
        // the stretch asked for (a pipe is read in file order: the plain
        // layout keeps the fragments' order).
        Src::Clip(c) => (None, Some(c.clone().for_apple())),
    };
    let mut child = crate::child::ffmpeg()
        .args(args(file, out, clip.as_ref().map(|c| c.end_ms - c.start_ms)))
        .stdin(if file.is_some() { Stdio::null() } else { Stdio::piped() })
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()?;
    crate::child::adopt(&child);
    let errs = super::live::keep_tail(child.stderr.take().unwrap());
    // (A recording that could not be read to its end: ffmpeg ends well, on
    // less. Only a failed read counts: ffmpeg stops reading at `-t`, before
    // the rest of the clip's last fragments, and a write after that fails.)
    let input_cut = Arc::new(AtomicBool::new(false));
    let feeder = match (child.stdin.take(), clip) {
        (Some(mut stdin), Some(c)) => {
            let cut = input_cut.clone();
            Some(std::thread::spawn(move || {
                const PIECE: u64 = 512 * 1024;
                let mut at = 0;
                while at < c.len() {
                    let to = (at + PIECE).min(c.len());
                    let Ok(b) = c.read_range(at, to) else {
                        cut.store(true, Ordering::SeqCst);
                        return;
                    };
                    if stdin.write_all(&b).is_err() {
                        // ffmpeg stopped reading: its exit status says whether it is done or failed.
                        return;
                    }
                    at = to;
                }
            }))
        }
        _ => None,
    };
    let t0 = std::time::Instant::now();
    let status = loop {
        match child.try_wait() {
            Ok(Some(st)) => break st,
            Ok(None) => {}
            Err(e) => {
                super::live::stop_child(&mut child);
                return Err(e);
            }
        }
        if cancelled() {
            super::live::stop_child(&mut child);
            return Err(std::io::Error::new(std::io::ErrorKind::Interrupted, "the download was stopped"));
        }
        if t0.elapsed() > limit {
            super::live::stop_child(&mut child);
            return Err(std::io::Error::new(std::io::ErrorKind::TimedOut, format!("took longer than {} s", limit.as_secs())));
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
    };
    if let Some(f) = feeder {
        let _ = f.join();
    }
    if !status.success() {
        return Err(std::io::Error::other(format!("ffmpeg: {}", super::live::tail(&errs))));
    }
    if input_cut.load(Ordering::SeqCst) {
        return Err(std::io::Error::other("the recording could not be read to its end"));
    }
    // (ffmpeg labels copied HEVC hev1: hvc1 plays on Apple's players.)
    let _ = crate::ingest::mp4read::hev1_file_to_hvc1(out);
    Ok(())
}

/// How long a download waits for one of the server's download places.
const PLACE_WAIT: std::time::Duration = std::time::Duration::from_secs(30);

/// Set when dropped: the response was dropped (the download stopped) while
/// the file was still being made.
struct Stop(Arc<AtomicBool>);

impl Drop for Stop {
    fn drop(&mut self) {
        self.0.store(true, Ordering::Relaxed);
    }
}

/// A temporary file's name in `<data_dir>/tmp` (swept after an hour if a
/// crash leaves it).
pub fn temp_path(data_dir: &std::path::Path) -> std::path::PathBuf {
    static N: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let dir = crate::engine::storage::temp_dir(data_dir);
    dir.join(format!("aac-{}-{}.mp4", std::process::id(), N.fetch_add(1, Ordering::Relaxed)))
}

/// Downloads bigger than this (bytes of the clip), or longer (ms), are not
/// converted: they are sent at once as they are (a plain MP4 with the FLAC
/// header fixed). Converting first would send nothing for minutes (a
/// reverse proxy gives up), and need as much room again on the disk.
pub const MAX_BYTES: u64 = 1 << 30;
pub const MAX_MS: i64 = 20 * 60_000;

/// Room kept free besides what a conversion writes.
const MARGIN: u64 = 256 << 20;

/// Whether writing `need` bytes leaves the drive with `free` of `total`
/// bytes free still above the margin and the share recordings keep free
/// (`min_free_pct`, see the storage settings).
fn room_ok(free: u64, total: u64, need: u64, min_free_pct: u64) -> bool {
    let floor = total / 100 * min_free_pct.min(90);
    free.checked_sub(need.saturating_add(MARGIN)).is_some_and(|left| left >= floor)
}

/// Why `src` is not converted here (None: it may be): too big or long, or
/// too little room in `<data_dir>/tmp`.
fn skip_why(data_dir: &std::path::Path, src: &Src, min_free_pct: u64) -> Option<String> {
    let bytes = src.bytes();
    if bytes > MAX_BYTES {
        return Some(format!("{} MB is more than the {} MB converted", bytes >> 20, MAX_BYTES >> 20));
    }
    if let Src::Clip(c) = src {
        if c.end_ms - c.start_ms > MAX_MS {
            return Some(format!("{} min is longer than the {} min converted", (c.end_ms - c.start_ms) / 60_000, MAX_MS / 60_000));
        }
    }
    let dir = crate::engine::storage::temp_dir(data_dir);
    match (fs4::available_space(&dir), fs4::total_space(&dir)) {
        (Ok(free), Ok(total)) if room_ok(free, total, bytes, min_free_pct) => None,
        (Ok(free), Ok(_)) => Some(format!("too little free disk space ({} MB free, {} MB needed, the recordings' free share kept)", free >> 20, bytes >> 20)),
        _ => Some("the free disk space could not be read".into()),
    }
}

/// `src` converted into a temporary file (see [`convert`]), the file kept
/// until the last holder drops it. None: it could not be, or it is not
/// converted (too big, too long or too little room: see [`MAX_BYTES`]);
/// logged. `min_free_pct`: the share of the drive the recordings keep free.
pub fn convert_to_temp(data_dir: &std::path::Path, src: &Src, what: &str, min_free_pct: u64, cancelled: &dyn Fn() -> bool) -> Option<crate::engine::storage::TempInUse> {
    if let Some(why) = skip_why(data_dir, src, min_free_pct) {
        tracing::info!("download {what}: sent with its FLAC sound, not converted to AAC: {why}");
        return None;
    }
    crate::engine::storage::sweep_temp_files(data_dir, false);
    let held = crate::engine::storage::TempInUse::new(temp_path(data_dir));
    match convert(src, held.path(), cancelled) {
        Ok(()) => Some(held),
        Err(e) if e.kind() == std::io::ErrorKind::Interrupted => None,
        Err(e) => {
            super::warn_limited("aac download", &format!("download {what}: the sound could not be converted to AAC, sent as FLAC: {e}"));
            None
        }
    }
}

/// How a download was asked for: only a plain GET is converted.
#[derive(Clone, Copy, PartialEq, Debug)]
pub enum Ask {
    Get,
    /// The headers only: what a GET would send, nothing converted.
    Head,
    /// A `Range`: never converted (the caller answers it from the plain
    /// MP4, with byte ranges).
    Range,
}

impl Ask {
    pub fn of(method: &axum::http::Method, headers: &axum::http::HeaderMap) -> Ask {
        if method == axum::http::Method::HEAD {
            Ask::Head
        } else if headers.contains_key(header::RANGE) {
            Ask::Range
        } else {
            Ask::Get
        }
    }
}

/// A reader that takes nothing for this long is given up: the response
/// ends and its file is removed.
pub const SEND_IDLE: std::time::Duration = std::time::Duration::from_secs(120);

/// A download of `src` with its FLAC sound converted to AAC (see the module),
/// with these headers besides the type and length (its file name among
/// them). None: its sound is not FLAC, it could not be converted, or `ask`
/// is a Range: the caller sends it as it would otherwise. A HEAD gets the
/// headers a GET would have (no length: the file is not made), nothing run.
pub async fn download(data_dir: std::path::PathBuf, min_free_pct: u64, src: Src, what: String, headers: Vec<(String, String)>, ask: Ask) -> Option<Response> {
    if ask == Ask::Range {
        return None;
    }
    let (d, w) = (data_dir.clone(), what.clone());
    let (src, go) = tokio::task::spawn_blocking(move || {
        let go = src.has_flac()
            && match skip_why(&d, &src, min_free_pct) {
                None => true,
                Some(why) => {
                    tracing::info!("download {w}: sent with its FLAC sound, not converted to AAC: {why}");
                    false
                }
            };
        (src, go)
    })
    .await
    .ok()?;
    if !go {
        return None;
    }
    if ask == Ask::Head {
        return head_response(headers);
    }
    // One of the server's few download places (shared with the downloads
    // converted for a device), waited for a while.
    let t0 = std::time::Instant::now();
    let place = loop {
        if let Some(p) = super::reserve_download() {
            break p;
        }
        if t0.elapsed() > PLACE_WAIT {
            super::warn_limited("aac download busy", &format!("download {what}: other downloads are being converted, sent with FLAC sound"));
            return None;
        }
        tokio::time::sleep(std::time::Duration::from_millis(250)).await;
    };
    let stop = Stop(Arc::new(AtomicBool::new(false)));
    let flag = stop.0.clone();
    let made = tokio::task::spawn_blocking(move || convert_to_temp(&data_dir, &src, &what, min_free_pct, &|| flag.load(Ordering::Relaxed))).await.ok().flatten();
    drop(stop);
    drop(place);
    let held = made?;
    let file = tokio::fs::File::open(held.path()).await.ok()?;
    let len = file.metadata().await.ok()?.len();
    builder(headers).header(header::CONTENT_LENGTH, len).body(send_file(file, held, SEND_IDLE)).ok()
}

/// The answer's headers: those of a converted download, and `headers`.
fn builder(headers: Vec<(String, String)>) -> axum::http::response::Builder {
    let mut r = Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, "video/mp4")
        .header(header::CACHE_CONTROL, "no-store")
        .header(header::ACCEPT_RANGES, "none")
        .header("x-as-audio", "aac");
    for (k, v) in headers {
        if let (Ok(k), Ok(v)) = (header::HeaderName::try_from(k.as_str()), HeaderValue::from_str(&v)) {
            r = r.header(k, v);
        }
    }
    r
}

/// A HEAD of a download that would be converted: its headers, without a
/// length (it is known only once the file is made).
fn head_response(headers: Vec<(String, String)>) -> Option<Response> {
    // (A body of no known size: HTTP adds no `Content-Length: 0` to it.)
    builder(headers).body(Body::from_stream(futures::stream::empty::<std::io::Result<axum::body::Bytes>>())).ok()
}

/// The body that sends `file` (the converted download, removed with `held`
/// once sent). A reader that takes nothing for `idle` is given up: the file
/// is closed and removed, and the response ends unfinished.
fn send_file(file: tokio::fs::File, held: crate::engine::storage::TempInUse, idle: std::time::Duration) -> Body {
    let (tx, mut rx) = tokio::sync::mpsc::channel::<std::io::Result<axum::body::Bytes>>(2);
    tokio::spawn(async move {
        let mut rs = tokio_util::io::ReaderStream::with_capacity(file, 64 * 1024);
        while let Some(chunk) = futures::StreamExt::next(&mut rs).await {
            let failed = chunk.is_err();
            match tokio::time::timeout(idle, tx.send(chunk)).await {
                Ok(Ok(())) if !failed => {}
                Ok(_) => break,
                Err(_) => {
                    tracing::info!("download: the reader took nothing for {} s; ended", idle.as_secs());
                    break;
                }
            }
        }
        drop(rs); // close the file before it is removed
        drop(held);
    });
    Body::from_stream(futures::stream::poll_fn(move |cx| rx.poll_recv(cx)))
}

#[cfg(test)]
mod tests {
    #[test]
    fn video_and_subtitles_are_copied_and_the_sound_made_aac_in_a_plain_mp4() {
        let out = std::path::Path::new("/tmp/o.mp4");
        let a = super::args(None, out, Some(60_000));
        let at = |k: &str| a.iter().position(|x| x == k).map(|i| a[i + 1].as_str());
        assert_eq!(at("-i"), Some("pipe:0"));
        assert_eq!((at("-c:v"), at("-c:s"), at("-c:a")), (Some("copy"), Some("copy"), Some("aac")));
        assert_eq!(at("-movflags"), Some("+faststart"));
        assert!(!a.iter().any(|x| x.contains("frag")), "{a:?}");
        assert_eq!(a.last().map(String::as_str), Some("/tmp/o.mp4"));
        // An assembled clip ends where it was cut; a saved clip is whole.
        assert_eq!(at("-t"), Some("60.000"));
        let a = super::args(Some(std::path::Path::new("/c/clip.mp4")), out, None);
        assert!(a.windows(2).any(|w| w[0] == "-i" && w[1] == "/c/clip.mp4"));
        assert!(!a.iter().any(|x| x == "-t"));
        assert!(!a.iter().any(|x| x == "pipe:0"));
    }

    /// A conversion that fails (here: not a video) leaves nothing behind;
    /// the caller then sends the download as it would otherwise.
    #[test]
    fn a_failed_conversion_leaves_no_file() {
        let dir = std::env::temp_dir().join(format!("amber-aac-fail-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let bad = dir.join("bad.mp4");
        std::fs::write(&bad, b"not a video").unwrap();
        let src = super::Src::File(bad);
        assert!(!src.has_flac());
        assert!(super::convert_to_temp(&dir, &src, "test", 0, &|| false).is_none());
        // Stopped: ended at once.
        assert!(super::convert_to_temp(&dir, &super::Src::Clip(crate::ingest::splice::VirtualClip::from_bytes(vec![0; 64], 0, 1000, false)), "test", 0, &|| true).is_none());
        let left: Vec<_> = std::fs::read_dir(dir.join("tmp")).unwrap().flatten().collect();
        assert!(left.is_empty(), "{left:?}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_conversion_needs_room_and_leaves_the_recordings_their_share() {
        const MB: u64 = 1 << 20;
        let total = 100_000 * MB;
        // 10% kept free (10 000 MB), 256 MB margin.
        assert!(super::room_ok(20_000 * MB, total, 5_000 * MB, 10));
        assert!(!super::room_ok(15_000 * MB, total, 5_000 * MB, 10), "it would eat into the recordings' share");
        assert!(!super::room_ok(100 * MB, total, 50 * MB, 0), "the margin");
        assert!(super::room_ok(400 * MB, total, 50 * MB, 0));
        // Too big or too long: not converted (sent as it is, at once).
        let dir = std::env::temp_dir().join(format!("amber-aac-big-{}", std::process::id()));
        let long = super::Src::Clip(crate::ingest::splice::VirtualClip::from_bytes(vec![0; 64], 0, super::MAX_MS + 1, false));
        assert!(super::skip_why(&dir, &long, 0).is_some_and(|w| w.contains("longer")));
        let short = super::Src::Clip(crate::ingest::splice::VirtualClip::from_bytes(vec![0; 64], 0, 60_000, false));
        assert_eq!(super::skip_why(&dir, &short, 0), None);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A clip cut by `-t` well before its end (a download's window ends inside
    /// its last fragments): ffmpeg stops reading and ends well; the unread
    /// rest is not taken for a recording that could not be read.
    #[test]
    fn a_window_cut_before_the_clips_end_converts() {
        let dir = std::env::temp_dir().join(format!("amber-aac-cut-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let src = dir.join("in.mp4");
        // 20 s at 4 Mbps (noise does not compress), FLAC sound, fragmented.
        let ok = crate::child::ffmpeg()
            .args(["-v", "error", "-y", "-f", "lavfi", "-i", "nullsrc=s=320x240:r=10,geq=random(1)*255:128:128", "-f", "lavfi", "-i", "sine=sample_rate=8000", "-t", "20"])
            .args(["-c:v", "libx264", "-preset", "ultrafast", "-b:v", "4M", "-g", "20", "-c:a", "flac", "-ac", "1", "-strict", "-2", "-movflags", "frag_keyframe+empty_moov+default_base_moof", "-f", "mp4"])
            .arg(&src)
            .status()
            .is_ok_and(|s| s.success());
        if !ok {
            eprintln!("skipped: no ffmpeg with libx264");
            let _ = std::fs::remove_dir_all(&dir);
            return;
        }
        let bytes = std::fs::read(&src).unwrap();
        assert!(bytes.len() > 4 << 20, "{} bytes", bytes.len());
        // The first 3 s of it.
        let clip = super::Src::Clip(crate::ingest::splice::VirtualClip::from_bytes(bytes, 0, 3000, false));
        assert!(clip.has_flac());
        let out = dir.join("out.mp4");
        super::convert(&clip, &out, &|| false).expect("converted");
        let info = String::from_utf8_lossy(&crate::child::ffmpeg().args(["-hide_banner", "-i"]).arg(&out).output().unwrap().stderr).into_owned();
        assert!(info.contains("Audio: aac") && info.contains("Duration: 00:00:03"), "{info}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A reader that stops taking the converted file is given up: the file
    /// is removed; one that reads gets it all, and then it is removed too.
    #[tokio::test]
    async fn a_stalled_reader_lets_the_file_go() {
        let dir = std::env::temp_dir().join(format!("amber-aac-idle-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        for read in [false, true] {
            let held = crate::engine::storage::TempInUse::new(super::temp_path(&dir));
            let p = held.path().to_path_buf();
            std::fs::write(&p, vec![7u8; 2 << 20]).unwrap();
            let body = super::send_file(tokio::fs::File::open(&p).await.unwrap(), held, std::time::Duration::from_millis(200));
            if read {
                let b = axum::body::to_bytes(body, 4 << 20).await.unwrap();
                assert_eq!(b.len(), 2 << 20);
            } else {
                tokio::time::sleep(std::time::Duration::from_millis(800)).await;
                assert!(!p.exists(), "removed while the response is still held");
                drop(body);
            }
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
            assert!(!p.exists());
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_conversion_has_a_time_limit_by_size() {
        assert_eq!(super::time_limit(0), std::time::Duration::from_secs(60));
        assert_eq!(super::time_limit(200 << 20), std::time::Duration::from_secs(160));
    }
}
