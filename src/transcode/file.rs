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
    a.extend(["-f", "mp4", "-movflags", "empty_moov+default_base_moof+frag_keyframe", "pipe:1"].iter().map(|s| s.to_string()));
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
    pub seek_ms: i64,
    /// Stop after this much (ms) of video.
    pub limit_ms: Option<i64>,
    pub what: &'static str,
    pub camera: String,
}

/// An ffmpeg that has written nothing by then has failed (the next way is
/// tried).
const FIRST_OUTPUT: std::time::Duration = std::time::Duration::from_secs(15);

/// Start the transcode; the body streams its output.
pub fn stream(job: Job) -> axum::body::Body {
    let (tx, mut rx) = tokio::sync::mpsc::channel::<Chunk>(4);
    std::thread::Builder::new()
        .name("file-transcode".into())
        .spawn(move || run(job, tx))
        .ok();
    let s = futures::stream::poll_fn(move |cx| rx.poll_recv(cx));
    axum::body::Body::from_stream(s)
}

fn run(job: Job, tx: tokio::sync::mpsc::Sender<Chunk>) {
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::Mutex;
    let Job { input, target, encoders, software_after, slot, lease, fps, depth, seek_ms, limit_ms, what, camera } = job;
    let input = Arc::new(input);
    let gone = Arc::new(AtomicBool::new(false));
    let g2 = gone.clone();
    let listed = super::list_active(what, &camera, target, Box::new(move || if g2.load(Ordering::Relaxed) { 0 } else { 1 }));
    let mut slot = slot;
    let mut ways = super::live::attempts(&encoders);
    let mut i = 0;
    let mut sent_any = false;
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
            let (child, got, done, tx) = (child.clone(), got.clone(), done.clone(), tx.clone());
            let t0 = std::time::Instant::now();
            std::thread::spawn(move || {
                while !done.load(Ordering::SeqCst) {
                    std::thread::sleep(std::time::Duration::from_millis(250));
                    if tx.is_closed() || (!got.load(Ordering::SeqCst) && t0.elapsed() > FIRST_OUTPUT) {
                        let _ = child.lock().unwrap().kill();
                        return;
                    }
                }
            });
        }
        let mut buf = vec![0u8; 256 * 1024];
        loop {
            match out.read(&mut buf) {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    got.store(true, Ordering::SeqCst);
                    if !send_within(&tx, Ok(buf[..n].to_vec()), STALLED) {
                        // The player went, or stopped reading.
                        gone.store(true, Ordering::Relaxed);
                        break;
                    }
                }
            }
        }
        done.store(true, Ordering::SeqCst);
        super::live::stop_child(&mut child.lock().unwrap());
        if tx.is_closed() {
            gone.store(true, Ordering::Relaxed);
        }
        if got.load(Ordering::SeqCst) || gone.load(Ordering::Relaxed) {
            sent_any = got.load(Ordering::SeqCst);
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(30));
        let how = if hw_decode { "decoding on the graphics card" } else { "decoding in software" };
        super::warn_limited(&format!("file {camera} {} {hw_decode}", e.name(target.codec)), &format!("transcode: {camera}: {} ({how}) failed: {}", e.name(target.codec), super::live::tail(&errs)));
    }
    if !sent_any && !gone.load(Ordering::Relaxed) {
        let _ = tx.try_send(Err(std::io::Error::other("transcode failed")));
    }
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
