//! Recordings and event clips transcoded on the fly: ffmpeg reads the clip
//! (piped from the recordings, or a saved clip file) and writes fragmented
//! MP4, streamed to the player as it comes. One ffmpeg per request; it
//! stops when the player goes.

use super::{Encoder, Slot, Target, UserLease};
use crate::ingest::splice::VirtualClip;
use std::io::{Read, Write};
use std::process::Stdio;
use std::sync::Arc;

pub enum Input {
    /// Assembled from the recordings: piped to ffmpeg.
    Clip(VirtualClip),
    /// A saved clip file.
    File(std::path::PathBuf),
}

/// The ffmpeg arguments for a recording: `seek_ms` into it, as fragmented
/// MP4 with a keyframe (and fragment) every two seconds.
pub fn args(e: &Encoder, hw_decode: bool, t: &Target, fps: f64, depth: u8, file: Option<&std::path::Path>, seek_ms: i64, limit_ms: Option<i64>) -> Vec<String> {
    let mut a: Vec<String> = ["-hide_banner", "-loglevel", "error"].iter().map(|s| s.to_string()).collect();
    a.extend(super::device_args(e, hw_decode));
    let ss = format!("{:.3}", seek_ms.max(0) as f64 / 1000.0);
    match file {
        Some(p) => {
            if seek_ms > 0 {
                a.extend(["-ss".to_string(), ss.clone()]);
            }
            a.extend(["-i".to_string(), p.to_string_lossy().into_owned()]);
        }
        None => {
            a.extend(["-f", "mp4", "-i", "pipe:0"].iter().map(|s| s.to_string()));
            // A pipe cannot seek: frames before the moment are decoded and
            // dropped (the clip starts at the keyframe before it).
            if seek_ms > 0 {
                a.extend(["-ss".to_string(), ss]);
            }
        }
    }
    if let Some(l) = limit_ms.filter(|l| *l > 0) {
        a.extend(["-t".to_string(), format!("{:.3}", l as f64 / 1000.0)]);
    }
    a.extend(["-map", "0:v:0", "-map", "0:a:0?", "-c:a", "copy"].iter().map(|s| s.to_string()));
    a.extend(["-vf".to_string(), super::filters(e, hw_decode, t, depth)]);
    a.extend(super::encoder_args(e, t, fps, false));
    a.extend(["-f", "mp4", "-movflags", "empty_moov+default_base_moof+frag_keyframe+skip_trailer", "pipe:1"].iter().map(|s| s.to_string()));
    a
}

pub struct Job {
    pub input: Input,
    pub target: Target,
    pub encoders: Vec<Encoder>,
    /// On the graphics card: software may be tried after it (it worked in
    /// the test encode), when it has a place then.
    pub software_after: bool,
    pub slot: Slot,
    pub lease: Option<UserLease>,
    pub fps: f64,
    pub depth: u8,
    /// The source's codec, for the memory of decoding that failed.
    pub src_codec: String,
    pub seek_ms: i64,
    /// How long the output lasts (ms), written into its header: ffmpeg's
    /// streamed header says 0 and players would show a 1 ms video.
    pub media_ms: Option<i64>,
    /// Stop after this much (ms) of video.
    pub limit_ms: Option<i64>,
    pub what: &'static str,
    pub camera: String,
}

/// An ffmpeg that has written nothing by then has failed (the next way is
/// tried).
const FIRST_OUTPUT: std::time::Duration = std::time::Duration::from_secs(15);

/// How long a reader may wait for the first byte: every way the job can try
/// (each given FIRST_OUTPUT), and a margin.
pub fn first_byte_wait(job: &Job) -> std::time::Duration {
    first_byte_wait_for(&job.encoders, job.software_after)
}

fn first_byte_wait_for(encoders: &[Encoder], software_after: bool) -> std::time::Duration {
    let ways = super::live::attempts(encoders).len() + usize::from(software_after);
    FIRST_OUTPUT * ways.max(1) as u32 + std::time::Duration::from_secs(15)
}

/// Where the converted bytes go: straight to the one response, or to a file
/// kept for a while (every response, the first included, reads from it).
enum Sink {
    Chan(tokio::sync::mpsc::Sender<Chunk>),
    Cache(Arc<super::cache::Entry>),
}

impl Sink {
    /// False: stop converting (the player went, or a limit was hit).
    fn send(&self, data: &[u8]) -> bool {
        match self {
            Sink::Chan(tx) => send_within(tx, Ok(data.to_vec()), STALLED),
            Sink::Cache(e) => e.write(data),
        }
    }

    /// Nobody wants the rest.
    fn gone(&self) -> bool {
        match self {
            Sink::Chan(tx) => tx.is_closed(),
            Sink::Cache(e) => e.abandoned(super::cache::ABANDON),
        }
    }

    fn is_cache(&self) -> bool {
        matches!(self, Sink::Cache(_))
    }

    fn end(&self, sent_any: bool, gone: bool, clean: bool) {
        match self {
            Sink::Chan(tx) => {
                if !sent_any && !gone {
                    let _ = tx.try_send(Err(std::io::Error::other("transcode failed")));
                }
            }
            Sink::Cache(e) => {
                if clean && sent_any && !gone {
                    e.finish();
                } else {
                    e.fail();
                }
            }
        }
    }
}

/// Fails the entry if the conversion never ends it: the thread did not
/// start, or it panicked (else readers would wait on it forever).
struct FailGuard(Arc<super::cache::Entry>);

impl Drop for FailGuard {
    fn drop(&mut self) {
        // (Nothing when it ended already: fail changes only one being written.)
        self.0.fail();
    }
}

/// Start a transcode whose output is written to the cache entry as it is made.
pub fn start_cached(job: Job, entry: Arc<super::cache::Entry>) {
    let spawn = |f: Box<dyn FnOnce() + Send>| std::thread::Builder::new().name("file-transcode".into()).spawn(f).map(|_| ());
    start_guarded(entry, spawn, move |sink| run(job, sink));
}

/// Run `work` on the thread `spawn` starts, the entry failed if it does not
/// end it.
fn start_guarded(entry: Arc<super::cache::Entry>, spawn: impl FnOnce(Box<dyn FnOnce() + Send>) -> std::io::Result<()>, work: impl FnOnce(Sink) + Send + 'static) {
    let guard = FailGuard(entry.clone());
    let started = spawn(Box::new(move || {
        let _guard = guard;
        work(Sink::Cache(entry));
    }));
    if let Err(e) = started {
        // The closure (and its guard) was dropped: the entry has failed.
        tracing::warn!("transcode: could not start: {e}");
    }
}

/// Start the transcode; the body streams its output.
pub fn stream(job: Job) -> axum::body::Body {
    let (tx, mut rx) = tokio::sync::mpsc::channel::<Chunk>(4);
    std::thread::Builder::new()
        .name("file-transcode".into())
        .spawn(move || run(job, Sink::Chan(tx)))
        .ok();
    let s = futures::stream::poll_fn(move |cx| rx.poll_recv(cx));
    axum::body::Body::from_stream(s)
}

fn run(job: Job, sink: Sink) {
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::Mutex;
    let Job { input, target, encoders, software_after, slot, lease, fps, depth, src_codec, seek_ms, media_ms, limit_ms, what, camera } = job;
    let input = Arc::new(input);
    let gone = Arc::new(AtomicBool::new(false));
    let g2 = gone.clone();
    let sink = Arc::new(sink);
    let mut clean = false;
    let listed = super::list_active(what, &camera, target, Box::new(move || if g2.load(Ordering::Relaxed) { 0 } else { 1 }));
    let mut slot = slot;
    let mut ways = super::live::attempts(&encoders);
    let mut i = 0;
    let mut sent_any = false;
    let mut tries = super::HwTries::default();
    loop {
        if i >= ways.len() {
            match (software_after && slot.hardware).then(super::reserve_software).flatten() {
                Some(sw) => {
                    slot = sw;
                    ways.push((Encoder::software(), false));
                }
                None => break,
            }
        }
        while i < ways.len() && ways[i].1 && super::hw_skipped(&super::HwMemory::key(&ways[i].0, target.codec, &src_codec, depth)) {
            i += 1;
        }
        if i >= ways.len() {
            continue;
        }
        let (e, hw_decode) = ways[i].clone();
        i += 1;
        *listed.0.encoder.lock().unwrap() = e.name(target.codec).to_string();
        let file = match input.as_ref() {
            Input::File(p) => Some(p.as_path()),
            Input::Clip(_) => None,
        };
        let a = args(&e, hw_decode, &target, fps, depth, file, seek_ms, limit_ms);
        let mut child = match crate::child::ffmpeg().args(&a).stdin(if file.is_some() { Stdio::null() } else { Stdio::piped() }).stdout(Stdio::piped()).stderr(Stdio::piped()).spawn() {
            Ok(c) => c,
            Err(err) => {
                tracing::warn!("transcode: {camera}: ffmpeg: {err}");
                break;
            }
        };
        crate::child::adopt(&child);
        *listed.0.pid.lock().unwrap() = Some(child.id());
        let errs = super::live::keep_tail(child.stderr.take().unwrap());
        if let (Some(mut stdin), Input::Clip(_)) = (child.stdin.take(), input.as_ref()) {
            let input = input.clone();
            std::thread::spawn(move || {
                let Input::Clip(c) = input.as_ref() else { return };
                const PIECE: u64 = 512 * 1024;
                let mut at = 0;
                while at < c.len() {
                    let to = (at + PIECE).min(c.len());
                    let Ok(b) = c.read_range(at, to) else { return };
                    if stdin.write_all(&b).is_err() {
                        return;
                    }
                    at = to;
                }
            });
        }
        let mut out = child.stdout.take().unwrap();
        let child = Arc::new(Mutex::new(child));
        let got = Arc::new(AtomicBool::new(false));
        let done = Arc::new(AtomicBool::new(false));
        // Watch: nothing written in time (failed), or the player went.
        {
            let (child, got, done, sink) = (child.clone(), got.clone(), done.clone(), sink.clone());
            let t0 = std::time::Instant::now();
            std::thread::spawn(move || {
                while !done.load(Ordering::SeqCst) {
                    std::thread::sleep(std::time::Duration::from_millis(250));
                    if sink.gone() || (!got.load(Ordering::SeqCst) && t0.elapsed() > FIRST_OUTPUT) {
                        let _ = child.lock().unwrap().kill();
                        return;
                    }
                }
            });
        }
        let mut buf = vec![0u8; 256 * 1024];
        let mut eof = false;
        let mut patch = super::mp4::DurationPatch::new(media_ms);
        loop {
            match out.read(&mut buf) {
                Ok(0) | Err(_) => {
                    eof = true;
                    let rest = patch.rest();
                    if !rest.is_empty() && !sink.send(&rest) {
                        gone.store(true, Ordering::Relaxed);
                    }
                    break;
                }
                Ok(n) => {
                    if !got.swap(true, Ordering::SeqCst) {
                        // Settled at the first output, not at the end: others need not
                        // wait out a failing try on the card meanwhile.
                        tries.settle(super::HwMemory::key(&e, target.codec, &src_codec, depth), format!("{src_codec} with {}", e.name(target.codec)), hw_decode, true, false);
                    }
                    let data = patch.push(&buf[..n]);
                    if !data.is_empty() && !sink.send(&data) {
                        // The player went, or stopped reading.
                        gone.store(true, Ordering::Relaxed);
                        break;
                    }
                }
            }
        }
        done.store(true, Ordering::SeqCst);
        // Ended by itself and well (not killed, not crashed): the kept copy is complete.
        if eof && sink.is_cache() {
            for _ in 0..50 {
                match child.lock().unwrap().try_wait() {
                    Ok(Some(st)) => {
                        clean = st.success() && got.load(Ordering::SeqCst);
                        break;
                    }
                    Ok(None) => {}
                    Err(_) => break,
                }
                std::thread::sleep(std::time::Duration::from_millis(100));
            }
        }
        super::live::stop_child(&mut child.lock().unwrap());
        if sink.gone() {
            gone.store(true, Ordering::Relaxed);
        }
        // Made nothing: pending, blamed on the card only when software decoding then works.
        if !got.load(Ordering::SeqCst) {
            tries.settle(super::HwMemory::key(&e, target.codec, &src_codec, depth), format!("{src_codec} with {}", e.name(target.codec)), hw_decode, false, gone.load(Ordering::Relaxed));
        }
        if got.load(Ordering::SeqCst) || gone.load(Ordering::Relaxed) {
            sent_any = got.load(Ordering::SeqCst);
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(30));
        let how = if hw_decode { "decoding on the graphics card" } else { "decoding in software" };
        super::warn_limited(&format!("file {camera} {} {hw_decode}", e.name(target.codec)), &format!("transcode: {camera}: {} ({how}) failed: {}", e.name(target.codec), super::live::tail(&errs)));
    }
    sink.end(sent_any, gone.load(Ordering::Relaxed), clean);
    drop(slot);
    drop(lease);
    drop(listed);
}

/// A player that takes nothing for this long is dropped (its transcode
/// stops and frees its place).
const STALLED: std::time::Duration = std::time::Duration::from_secs(45);

type Chunk = Result<Vec<u8>, std::io::Error>;

/// Hand a piece to the response; false when the player is gone or has not
/// taken anything for `limit`.
fn send_within(tx: &tokio::sync::mpsc::Sender<Chunk>, item: Chunk, limit: std::time::Duration) -> bool {
    use tokio::sync::mpsc::error::TrySendError;
    let t0 = std::time::Instant::now();
    let mut item = item;
    loop {
        match tx.try_send(item) {
            Ok(()) => return true,
            Err(TrySendError::Closed(_)) => return false,
            Err(TrySendError::Full(v)) => {
                if t0.elapsed() >= limit {
                    return false;
                }
                item = v;
                std::thread::sleep(std::time::Duration::from_millis(10));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use crate::transcode::cache::{Cache, Limits, Status};

    #[test]
    fn a_conversion_that_never_ends_its_entry_fails_it() {
        let dir = std::env::temp_dir().join(format!("amber-file-guard-{}", std::process::id()));
        let c = Cache::new(dir.clone(), Limits { entry_max: 1 << 20, total_max: 1 << 20, expire: std::time::Duration::from_secs(60) });
        // The thread could not be started.
        let e = c.create("a", vec![]).unwrap();
        super::start_guarded(e.clone(), |_| Err(std::io::Error::other("no threads")), |_| {});
        assert_eq!(e.progress().status, Status::Failed);
        // It panicked before it ended the entry.
        let e = c.create("b", vec![]).unwrap();
        let spawn = |f: Box<dyn FnOnce() + Send>| {
            let _ = std::thread::spawn(f).join();
            Ok(())
        };
        super::start_guarded(e.clone(), spawn, |sink| {
            assert!(sink.send(b"part"));
            panic!("a bug in the conversion");
        });
        assert_eq!(e.progress(), crate::transcode::cache::Progress { written: 4, status: Status::Failed });
        // One that ended well stays done.
        let e = c.create("c", vec![]).unwrap();
        super::start_guarded(e.clone(), spawn, |sink| {
            assert!(sink.send(b"all"));
            sink.end(true, false, true);
        });
        assert_eq!(e.progress().status, Status::Done);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_first_byte_wait_covers_every_try() {
        use crate::transcode::Encoder;
        let job = |encoders: Vec<Encoder>, software_after: bool| super::first_byte_wait_for(&encoders, software_after);
        let hw = Encoder { hw: Some(crate::transcode::Hw::Qsv), device: None };
        // Card decode + software decode, then software after: three tries.
        assert_eq!(job(vec![hw.clone()], true), std::time::Duration::from_secs(3 * 15 + 15));
        assert_eq!(job(vec![Encoder::software()], false), std::time::Duration::from_secs(15 + 15));
    }
    #[test]
    fn a_player_that_stops_reading_is_dropped() {
        let (tx, mut rx) = tokio::sync::mpsc::channel(1);
        let d = std::time::Duration::from_millis(50);
        assert!(super::send_within(&tx, Ok(vec![1]), d));
        assert!(!super::send_within(&tx, Ok(vec![2]), d), "full and not read: gives up");
        assert!(rx.try_recv().is_ok());
        assert!(super::send_within(&tx, Ok(vec![3]), d));
        drop(rx);
        assert!(!super::send_within(&tx, Ok(vec![4]), d));
    }
}
