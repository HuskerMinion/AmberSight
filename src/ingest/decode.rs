//! Decoding of camera video for motion analysis.
//!
//! Recording consumes retina frames as-is, but motion detection needs decoded
//! pictures. [`FrameDecoder`] feeds the length-prefixed access units produced
//! by retina (`FrameFormat::MP4`) into a single ffmpeg process as an Annex B
//! elementary stream and yields downscaled, rate-limited JPEG frames.

use anyhow::{anyhow, bail, Context, Result};
use std::io::{Read, Write};
use std::process::{Child, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver, SyncSender, TrySendError};
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

const START_CODE: [u8; 4] = [0, 0, 0, 1];
/// Queue depth between `push` and the ffmpeg stdin writer thread.
const INPUT_QUEUE: usize = 64;
/// Queue depth of decoded JPEGs awaiting the consumer.
const OUTPUT_QUEUE: usize = 32;
/// An ffmpeg that dies within this long after start is treated as a failed
/// hardware-decode setup and replaced by a software decoder.
const HW_FALLBACK_WINDOW: Duration = Duration::from_secs(1);
/// Input chunks kept for replay into the software fallback decoder.
const REPLAY_LIMIT: usize = 64;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Codec {
    H264,
    H265,
}

impl Codec {
    fn ffmpeg_format(self) -> &'static str {
        match self {
            Codec::H264 => "h264",
            Codec::H265 => "hevc",
        }
    }
}

/// Convert one access unit of 4-byte length-prefixed NAL units into Annex B,
/// appending to `out`.
pub fn length_prefixed_to_annexb(data: &[u8], out: &mut Vec<u8>) -> Result<()> {
    let mut pos = 0;
    while pos < data.len() {
        let Some(hdr) = data.get(pos..pos + 4) else {
            bail!("truncated NAL length at offset {pos}");
        };
        let len = u32::from_be_bytes([hdr[0], hdr[1], hdr[2], hdr[3]]) as usize;
        pos += 4;
        let end = pos
            .checked_add(len)
            .filter(|&e| e <= data.len())
            .ok_or_else(|| anyhow!("NAL of {len} bytes overruns access unit at offset {pos}"))?;
        out.extend_from_slice(&START_CODE);
        out.extend_from_slice(&data[pos..end]);
        pos = end;
    }
    Ok(())
}

struct Reader<'a> {
    data: &'a [u8],
    pos: usize,
}

impl<'a> Reader<'a> {
    fn take(&mut self, n: usize) -> Result<&'a [u8]> {
        let s = self
            .data
            .get(self.pos..self.pos + n)
            .ok_or_else(|| anyhow!("codec record truncated at offset {}", self.pos))?;
        self.pos += n;
        Ok(s)
    }
    fn u8(&mut self) -> Result<u8> {
        Ok(self.take(1)?[0])
    }
    fn u16(&mut self) -> Result<u16> {
        let b = self.take(2)?;
        Ok(u16::from_be_bytes([b[0], b[1]]))
    }
}

/// Extract the parameter sets from an avcC (H.264) or hvcC (H.265) record and
/// return them as Annex B (each NAL preceded by a start code). For H.264 these
/// are all SPS then all PPS; for H.265 the VPS/SPS/PPS arrays in record order.
pub fn parameter_sets_annexb(codec: Codec, extra_data: &[u8]) -> Result<Vec<u8>> {
    let mut r = Reader { data: extra_data, pos: 0 };
    let mut out = Vec::new();
    let mut emit = |nal: &[u8]| {
        out.extend_from_slice(&START_CODE);
        out.extend_from_slice(nal);
    };
    match codec {
        Codec::H264 => {
            if r.u8()? != 1 {
                bail!("unsupported avcC configuration version");
            }
            r.take(4)?; // profile, compat, level, length size
            let n_sps = r.u8()? & 0x1f;
            for _ in 0..n_sps {
                let len = r.u16()? as usize;
                emit(r.take(len)?);
            }
            let n_pps = r.u8()?;
            for _ in 0..n_pps {
                let len = r.u16()? as usize;
                emit(r.take(len)?);
            }
        }
        Codec::H265 => {
            if r.u8()? != 1 {
                bail!("unsupported hvcC configuration version");
            }
            r.take(21)?; // profile/tier/level/chroma/etc. up to numOfArrays
            let n_arrays = r.u8()?;
            for _ in 0..n_arrays {
                let nal_type = r.u8()? & 0x3f;
                let n = r.u16()?;
                for _ in 0..n {
                    let len = r.u16()? as usize;
                    let nal = r.take(len)?;
                    if matches!(nal_type, 32..=34) {
                        emit(nal);
                    }
                }
            }
        }
    }
    if out.is_empty() {
        bail!("codec record contains no parameter sets");
    }
    Ok(out)
}

/// Counters shared with the worker threads.
#[derive(Default)]
struct Counters {
    frames_in: AtomicU64,
    frames_dropped: AtomicU64,
    real_dropped: AtomicU64,
    jpegs_out: AtomicU64,
    jpegs_dropped: AtomicU64,
}

/// Snapshot of decoder statistics.
#[derive(Clone, Copy, Debug, Default)]
pub struct DecoderStats {
    /// Access units accepted by `push` (after the first keyframe).
    pub frames_in: u64,
    /// Access units discarded: waiting for a keyframe, queue full, or malformed.
    pub frames_dropped: u64,
    /// The part of `frames_dropped` that was a real loss: queue full, malformed,
    /// ffmpeg gone, and frames skipped while waiting for a keyframe after such
    /// a gap. Excludes the frames before the first keyframe of a new decoder.
    pub real_dropped: u64,
    /// JPEG frames produced by ffmpeg.
    pub jpegs_out: u64,
    /// JPEG frames discarded because the consumer was too slow.
    pub jpegs_dropped: u64,
}

/// One running ffmpeg process plus its two I/O threads.
struct Pipeline {
    child: Child,
    input: Option<SyncSender<Vec<u8>>>,
    writer: Option<JoinHandle<()>>,
    reader: Option<JoinHandle<()>>,
    started: Instant,
    /// Milliseconds after `started` at which stdout closed; `u64::MAX` while alive.
    exited_ms: Arc<AtomicU64>,
    hw: bool,
}

pub struct FrameDecoder {
    codec: Codec,
    fps: u32,
    scale: u32,
    param_sets: Vec<u8>,
    pipe: Option<Pipeline>,
    out_tx: Option<SyncSender<Vec<u8>>>,
    frames: Receiver<Vec<u8>>,
    counters: Arc<Counters>,
    seen_key: bool,
    /// Input was lost after a gap and is skipped until the next keyframe.
    gap: bool,
    /// Chunks sent while a hardware decoder is still unproven.
    replay: Vec<Vec<u8>>,
    hw_failed: bool,
}

/// Split `hwaccel` ("vaapi", "vaapi:/dev/dri/renderD129", "qsv") into ffmpeg input args.
fn hwaccel_args(spec: &str) -> Result<Vec<String>> {
    let (kind, device) = match spec.split_once(':') {
        Some((k, d)) => (k, Some(d.to_string())),
        None => (spec, None),
    };
    match kind {
        "vaapi" => Ok(vec![
            "-hwaccel".into(),
            "vaapi".into(),
            "-hwaccel_device".into(),
            device.unwrap_or_else(|| "/dev/dri/renderD128".into()),
        ]),
        "qsv" => {
            let mut v = vec!["-hwaccel".to_string(), "qsv".into()];
            if let Some(d) = device {
                v.extend(["-hwaccel_device".to_string(), d]);
            }
            Ok(v)
        }
        "cuda" | "vulkan" | "videotoolbox" | "d3d11va" | "dxva2" => {
            Ok(vec!["-hwaccel".into(), kind.into()])
        }
        other => bail!("unsupported hwaccel '{other}'"),
    }
}

fn ffmpeg_args(codec: Codec, fps: u32, scale: u32, hw: Option<&str>) -> Result<Vec<String>> {
    let mut a: Vec<String> = ["-hide_banner", "-loglevel", "error"].map(String::from).into();
    if let Some(spec) = hw {
        a.extend(hwaccel_args(spec)?);
    }
    // Raw elementary streams carry no timing; stamp with arrival time so the
    // fps filter sees real time regardless of the camera frame rate.
    a.extend(
        [
            "-flags",
            "low_delay",
            "-probesize",
            "4096",
            "-analyzeduration",
            "0",
            "-use_wallclock_as_timestamps",
            "1",
            "-f",
            codec.ffmpeg_format(),
            "-i",
            "-",
        ]
        .map(String::from),
    );
    let mut vf = format!("fps={}", fps.max(1));
    if scale > 0 {
        vf.push_str(&format!(",scale={scale}:-2"));
    }
    a.extend(["-vf".to_string(), vf]);
    a.extend(["-f", "image2pipe", "-c:v", "mjpeg", "-q:v", "8", "-"].map(String::from));
    Ok(a)
}

fn find_marker(buf: &[u8], from: usize) -> Option<usize> {
    if buf.len() < 3 || from >= buf.len() {
        return None;
    }
    (from..buf.len() - 2).find(|&i| buf[i] == 0xFF && buf[i + 1] == 0xD8 && buf[i + 2] == 0xFF)
}

/// Length of the complete JPEG at the start of `buf` (which begins with
/// SOI), or None if it is not complete yet. Walks the marker segments to the
/// start of scan, then scans entropy-coded data for EOI: there, 0xFF is
/// always followed by 0x00 (stuffing) or a restart marker, so FF D9 cannot
/// appear by accident. Header payloads are skipped by length for the same
/// reason.
fn find_jpeg_end(buf: &[u8]) -> Option<usize> {
    let mut i = 2; // after SOI
    loop {
        // Marker segment: FF xx, then a 2-byte big-endian length (incl. itself).
        while i < buf.len() && buf[i] == 0xFF && buf.get(i + 1) == Some(&0xFF) {
            i += 1; // fill bytes
        }
        if i + 4 > buf.len() {
            return None;
        }
        if buf[i] != 0xFF {
            return None; // not at a marker: wait for more data / resync later
        }
        let marker = buf[i + 1];
        if marker == 0xD9 {
            return Some(i + 2);
        }
        let len = u16::from_be_bytes([buf[i + 2], buf[i + 3]]) as usize;
        i += 2 + len;
        if marker == 0xDA {
            break; // start of scan: entropy-coded data follows
        }
    }
    while i + 1 < buf.len() {
        if buf[i] == 0xFF {
            match buf[i + 1] {
                0xD9 => return Some(i + 2),
                0x00 | 0xD0..=0xD7 | 0xFF => i += 1,
                // Another marker inside the scan area (e.g. a further SOS in
                // progressive JPEG): skip its segment.
                _ if i + 3 < buf.len() => {
                    let len = u16::from_be_bytes([buf[i + 2], buf[i + 3]]) as usize;
                    i += 2 + len;
                    continue;
                }
                _ => return None,
            }
        }
        i += 1;
    }
    None
}

impl Pipeline {
    fn spawn(
        codec: Codec,
        fps: u32,
        scale: u32,
        hw: Option<&str>,
        out_tx: SyncSender<Vec<u8>>,
        counters: Arc<Counters>,
    ) -> Result<Self> {
        let args = ffmpeg_args(codec, fps, scale, hw)?;
        let mut child = crate::child::ffmpeg()
            .args(&args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .context("failed to start ffmpeg")?;
        crate::child::adopt(&child);
        let mut stdin = child.stdin.take().context("ffmpeg stdin unavailable")?;
        let mut stdout = child.stdout.take().context("ffmpeg stdout unavailable")?;
        let started = Instant::now();
        let exited_ms = Arc::new(AtomicU64::new(u64::MAX));

        let (in_tx, in_rx) = mpsc::sync_channel::<Vec<u8>>(INPUT_QUEUE);
        let writer = std::thread::spawn(move || {
            while let Ok(chunk) = in_rx.recv() {
                if stdin.write_all(&chunk).is_err() {
                    break;
                }
            }
            // Dropping stdin signals EOF to ffmpeg; dropping in_rx makes later
            // sends fail so the owner notices.
        });

        let exited = exited_ms.clone();
        let reader = std::thread::spawn(move || {
            let mut buf: Vec<u8> = Vec::with_capacity(256 * 1024);
            let mut chunk = [0u8; 64 * 1024];
            loop {
                match stdout.read(&mut chunk) {
                    Ok(0) | Err(_) => break,
                    Ok(n) => buf.extend_from_slice(&chunk[..n]),
                }
                // Emit each JPEG as soon as its end-of-image marker arrives
                // (waiting for the next frame's start would add a frame of delay).
                while let Some(start) = find_marker(&buf, 0) {
                    if start > 0 {
                        buf.drain(..start);
                    }
                    let Some(end) = find_jpeg_end(&buf) else { break };
                    let jpeg = buf[..end].to_vec();
                    buf.drain(..end);
                    counters.jpegs_out.fetch_add(1, Ordering::Relaxed);
                    match out_tx.try_send(jpeg) {
                        Ok(()) => {}
                        Err(TrySendError::Full(_)) => {
                            counters.jpegs_dropped.fetch_add(1, Ordering::Relaxed);
                        }
                        Err(TrySendError::Disconnected(_)) => return,
                    }
                }
                if buf.len() > 32 * 1024 * 1024 {
                    buf.clear(); // runaway garbage; resynchronize on the next SOI
                }
            }
            exited.store(started.elapsed().as_millis() as u64, Ordering::SeqCst);
        });

        Ok(Pipeline {
            child,
            input: Some(in_tx),
            writer: Some(writer),
            reader: Some(reader),
            started,
            exited_ms,
            hw: hw.is_some(),
        })
    }

    fn alive(&self) -> bool {
        self.exited_ms.load(Ordering::SeqCst) == u64::MAX
    }

    /// Close stdin, give ffmpeg a moment to finish, then kill it.
    fn shutdown(mut self) {
        self.input.take(); // writer sees a closed queue and drops stdin
        let deadline = Instant::now() + Duration::from_millis(1500);
        loop {
            match self.child.try_wait() {
                Ok(Some(_)) => break,
                Ok(None) if Instant::now() < deadline => std::thread::sleep(Duration::from_millis(20)),
                _ => {
                    let _ = self.child.kill();
                    let _ = self.child.wait();
                    break;
                }
            }
        }
        // The writer can be stuck in write_all only while ffmpeg lives; it is
        // dead now, so both threads finish promptly.
        if let Some(h) = self.writer.take() {
            let _ = h.join();
        }
        if let Some(h) = self.reader.take() {
            let _ = h.join();
        }
    }
}

impl FrameDecoder {
    /// Start ffmpeg. `extra_data` is the avcC/hvcC record from
    /// `VideoParameters::extra_data()`. `hwaccel` is e.g. `"vaapi"`,
    /// `"vaapi:/dev/dri/renderD129"` or `"qsv"`; if that ffmpeg exits within a
    /// second of starting, a software decoder takes over transparently.
    pub fn start(codec: Codec, extra_data: &[u8], fps: u32, scale: u32, hwaccel: Option<String>) -> Result<Self> {
        let param_sets = parameter_sets_annexb(codec, extra_data)?;
        let (out_tx, frames) = mpsc::sync_channel(OUTPUT_QUEUE);
        let counters = Arc::new(Counters::default());
        let pipe = Pipeline::spawn(codec, fps, scale, hwaccel.as_deref(), out_tx.clone(), counters.clone())?;
        Ok(FrameDecoder {
            codec,
            fps,
            scale,
            param_sets,
            pipe: Some(pipe),
            out_tx: Some(out_tx),
            frames,
            counters,
            seen_key: false,
            gap: false,
            replay: Vec::new(),
            hw_failed: false,
        })
    }

    /// Decoded JPEG frames (SOI..next SOI), at most `fps` per second.
    pub fn frames(&self) -> &Receiver<Vec<u8>> {
        &self.frames
    }

    /// True if hardware decoding was requested but ffmpeg rejected it.
    pub fn hw_fallback_used(&self) -> bool {
        self.hw_failed
    }

    /// True while the current ffmpeg process is running with hardware decode.
    pub fn using_hwaccel(&self) -> bool {
        self.pipe.as_ref().is_some_and(|p| p.hw && p.alive())
    }

    pub fn stats(&self) -> DecoderStats {
        let c = &self.counters;
        DecoderStats {
            frames_in: c.frames_in.load(Ordering::Relaxed),
            frames_dropped: c.frames_dropped.load(Ordering::Relaxed),
            real_dropped: c.real_dropped.load(Ordering::Relaxed),
            jpegs_out: c.jpegs_out.load(Ordering::Relaxed),
            jpegs_dropped: c.jpegs_dropped.load(Ordering::Relaxed),
        }
    }

    /// Replace the stored parameter sets (stream parameters changed mid-session).
    pub fn set_extra_data(&mut self, extra_data: &[u8]) -> Result<()> {
        self.param_sets = parameter_sets_annexb(self.codec, extra_data)?;
        Ok(())
    }

    /// Queue one length-prefixed access unit. Never blocks: if ffmpeg cannot
    /// keep up the frame is dropped and counted. Frames before the first
    /// keyframe are dropped. Returns an error only if ffmpeg is gone and
    /// cannot be restarted.
    pub fn push(&mut self, data: &[u8], is_keyframe: bool) -> Result<()> {
        if !self.seen_key {
            if !is_keyframe {
                self.counters.frames_dropped.fetch_add(1, Ordering::Relaxed);
                if self.gap {
                    self.counters.real_dropped.fetch_add(1, Ordering::Relaxed);
                }
                return Ok(());
            }
            self.seen_key = true;
            self.gap = false;
        }
        let mut chunk = Vec::with_capacity(data.len() + self.param_sets.len() + 16);
        if is_keyframe {
            chunk.extend_from_slice(&self.param_sets);
        }
        if let Err(e) = length_prefixed_to_annexb(data, &mut chunk) {
            self.drop_until_keyframe();
            return Err(e);
        }
        self.counters.frames_in.fetch_add(1, Ordering::Relaxed);

        self.maybe_fall_back()?;
        let pipe = self.pipe.as_mut().context("decoder stopped")?;
        if pipe.hw && pipe.started.elapsed() < HW_FALLBACK_WINDOW {
            if self.replay.len() < REPLAY_LIMIT {
                self.replay.push(chunk.clone());
            }
        } else if !self.replay.is_empty() {
            // Past the fallback window: the replay copy is no longer needed.
            self.replay = Vec::new();
        }
        let Some(tx) = pipe.input.as_ref() else { bail!("decoder stopped") };
        match tx.try_send(chunk) {
            Ok(()) => Ok(()),
            Err(TrySendError::Full(_)) => {
                self.drop_until_keyframe();
                Ok(())
            }
            Err(TrySendError::Disconnected(_)) => {
                self.drop_until_keyframe();
                if self.maybe_fall_back()? {
                    return Ok(());
                }
                bail!("ffmpeg exited")
            }
        }
    }

    /// Count a dropped frame and skip input until the next keyframe. H.264/
    /// H.265 frames reference earlier ones, so decoding past a gap produces
    /// smeared pictures that the motion detector would read as motion.
    fn drop_until_keyframe(&mut self) {
        self.counters.frames_dropped.fetch_add(1, Ordering::Relaxed);
        self.counters.real_dropped.fetch_add(1, Ordering::Relaxed);
        self.seen_key = false;
        self.gap = true;
    }

    /// If a hardware-decode ffmpeg died within the grace window, restart in
    /// software and replay what it was sent. Returns whether it switched.
    fn maybe_fall_back(&mut self) -> Result<bool> {
        let Some(pipe) = self.pipe.as_ref() else { return Ok(false) };
        if !pipe.hw {
            return Ok(false);
        }
        let died_ms = pipe.exited_ms.load(Ordering::SeqCst);
        let early_exit = died_ms != u64::MAX && Duration::from_millis(died_ms) < HW_FALLBACK_WINDOW;
        let writer_dead = pipe.writer.as_ref().is_some_and(|w| w.is_finished())
            && pipe.started.elapsed() < HW_FALLBACK_WINDOW;
        if !(early_exit || writer_dead) {
            return Ok(false);
        }
        if let Some(old) = self.pipe.take() {
            old.shutdown();
        }
        self.hw_failed = true;
        let out_tx = self.out_tx.clone().context("decoder stopped")?;
        let new = Pipeline::spawn(self.codec, self.fps, self.scale, None, out_tx, self.counters.clone())?;
        if let Some(tx) = new.input.as_ref() {
            for c in self.replay.drain(..) {
                let _ = tx.try_send(c);
            }
        }
        self.pipe = Some(new);
        Ok(true)
    }

    /// Close ffmpeg's input, wait briefly for it to flush, then kill it.
    pub fn stop(&mut self) {
        self.out_tx.take();
        if let Some(p) = self.pipe.take() {
            p.shutdown();
        }
    }

    /// Whether the ffmpeg process is still running.
    pub fn is_running(&self) -> bool {
        self.pipe.as_ref().is_some_and(|p| p.alive())
    }

    /// Process id of the ffmpeg child (for diagnostics).
    pub fn pid(&self) -> Option<u32> {
        self.pipe.as_ref().map(|p| p.child.id())
    }
}

impl Drop for FrameDecoder {
    fn drop(&mut self) {
        self.stop();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tiny_jpeg(shade: u8) -> Vec<u8> {
        let img = image::GrayImage::from_fn(32, 24, |x, y| image::Luma([shade ^ ((x * 7 + y * 3) as u8)]));
        let mut out = Vec::new();
        image::DynamicImage::ImageLuma8(img)
            .write_to(&mut std::io::Cursor::new(&mut out), image::ImageFormat::Jpeg)
            .unwrap();
        out
    }

    #[test]
    fn jpeg_end_found_exactly_and_waits_for_partial() {
        let a = tiny_jpeg(0x10);
        let b = tiny_jpeg(0xF0);
        let mut both = a.clone();
        both.extend_from_slice(&b);
        assert_eq!(find_jpeg_end(&both), Some(a.len()));
        assert_eq!(find_jpeg_end(&b), Some(b.len()));
        // Every truncation of a JPEG is reported as incomplete.
        for cut in 2..a.len() {
            assert_eq!(find_jpeg_end(&a[..cut]), None, "cut at {cut}");
        }
    }

    fn lp(nals: &[&[u8]]) -> Vec<u8> {
        let mut v = Vec::new();
        for n in nals {
            v.extend_from_slice(&(n.len() as u32).to_be_bytes());
            v.extend_from_slice(n);
        }
        v
    }

    #[test]
    fn annexb_single_nal() {
        let mut out = Vec::new();
        length_prefixed_to_annexb(&lp(&[&[0x65, 1, 2, 3]]), &mut out).unwrap();
        assert_eq!(out, [0, 0, 0, 1, 0x65, 1, 2, 3]);
    }

    #[test]
    fn annexb_multiple_nals() {
        let mut out = Vec::new();
        length_prefixed_to_annexb(&lp(&[&[0x06, 9], &[0x65, 1, 2], &[0x41]]), &mut out).unwrap();
        assert_eq!(out, [0, 0, 0, 1, 0x06, 9, 0, 0, 0, 1, 0x65, 1, 2, 0, 0, 0, 1, 0x41]);
    }

    #[test]
    fn annexb_empty_and_errors() {
        let mut out = Vec::new();
        length_prefixed_to_annexb(&[], &mut out).unwrap();
        assert!(out.is_empty());
        assert!(length_prefixed_to_annexb(&[0, 0, 1], &mut out).is_err());
        assert!(length_prefixed_to_annexb(&[0, 0, 0, 9, 1, 2], &mut out).is_err());
        assert!(length_prefixed_to_annexb(&[0xff, 0xff, 0xff, 0xff, 1], &mut out).is_err());
    }

    fn avcc(sps: &[&[u8]], pps: &[&[u8]]) -> Vec<u8> {
        let mut v = vec![1, 0x64, 0, 0x1f, 0xff, 0xe0 | sps.len() as u8];
        for s in sps {
            v.extend_from_slice(&(s.len() as u16).to_be_bytes());
            v.extend_from_slice(s);
        }
        v.push(pps.len() as u8);
        for p in pps {
            v.extend_from_slice(&(p.len() as u16).to_be_bytes());
            v.extend_from_slice(p);
        }
        v
    }

    #[test]
    fn avcc_multiple_sps_pps() {
        let rec = avcc(&[&[0x67, 1, 2], &[0x67, 3]], &[&[0x68, 4], &[0x68, 5, 6]]);
        let out = parameter_sets_annexb(Codec::H264, &rec).unwrap();
        assert_eq!(
            out,
            [0, 0, 0, 1, 0x67, 1, 2, 0, 0, 0, 1, 0x67, 3, 0, 0, 0, 1, 0x68, 4, 0, 0, 0, 1, 0x68, 5, 6]
        );
    }

    #[test]
    fn avcc_errors() {
        assert!(parameter_sets_annexb(Codec::H264, &[]).is_err());
        assert!(parameter_sets_annexb(Codec::H264, &[2, 0, 0, 0, 0xff, 0xe1]).is_err());
        let mut rec = avcc(&[&[0x67, 1, 2]], &[&[0x68]]);
        rec.truncate(rec.len() - 1);
        assert!(parameter_sets_annexb(Codec::H264, &rec).is_err());
        assert!(parameter_sets_annexb(Codec::H264, &avcc(&[], &[])).is_err());
    }

    fn hvcc(arrays: &[(u8, &[&[u8]])]) -> Vec<u8> {
        let mut v = vec![1];
        v.extend_from_slice(&[0u8; 21]);
        v.push(arrays.len() as u8);
        for (t, nals) in arrays {
            v.push(0x80 | t);
            v.extend_from_slice(&(nals.len() as u16).to_be_bytes());
            for n in *nals {
                v.extend_from_slice(&(n.len() as u16).to_be_bytes());
                v.extend_from_slice(n);
            }
        }
        v
    }

    #[test]
    fn hvcc_vps_sps_pps() {
        let rec = hvcc(&[
            (32, &[&[0x40, 1]]),
            (33, &[&[0x42, 2, 3], &[0x42, 4]]),
            (39, &[&[0x4e, 9]]), // SEI: not a parameter set
            (34, &[&[0x44, 5]]),
        ]);
        let out = parameter_sets_annexb(Codec::H265, &rec).unwrap();
        assert_eq!(
            out,
            [0, 0, 0, 1, 0x40, 1, 0, 0, 0, 1, 0x42, 2, 3, 0, 0, 0, 1, 0x42, 4, 0, 0, 0, 1, 0x44, 5]
        );
    }

    #[test]
    fn hvcc_errors() {
        assert!(parameter_sets_annexb(Codec::H265, &[1, 0, 0]).is_err());
        assert!(parameter_sets_annexb(Codec::H265, &hvcc(&[])).is_err());
    }

    #[test]
    fn marker_search() {
        let b = [1, 0xff, 0xd8, 0xff, 0xe0, 2, 0xff, 0xd8, 0xff];
        assert_eq!(find_marker(&b, 0), Some(1));
        assert_eq!(find_marker(&b, 4), Some(6));
        assert_eq!(find_marker(&b, 7), None);
    }

    #[test]
    fn hwaccel_flags() {
        assert_eq!(
            hwaccel_args("vaapi").unwrap(),
            ["-hwaccel", "vaapi", "-hwaccel_device", "/dev/dri/renderD128"]
        );
        assert_eq!(hwaccel_args("vaapi:/dev/dri/renderD129").unwrap()[3], "/dev/dri/renderD129");
        assert_eq!(hwaccel_args("qsv").unwrap(), ["-hwaccel", "qsv"]);
        assert!(hwaccel_args("bogus").is_err());
    }

    #[test]
    fn push_drops_until_keyframe_and_stop_is_clean() {
        // Constructing the decoder needs ffmpeg; skip quietly if absent.
        if std::process::Command::new(crate::ffmpeg_bin::program()).arg("-version").output().is_err() {
            return;
        }
        let rec = avcc(&[&[0x67, 0x64, 0, 0x1f]], &[&[0x68, 0xee]]);
        let mut d = FrameDecoder::start(Codec::H264, &rec, 2, 320, None).unwrap();
        d.push(&lp(&[&[0x41, 1]]), false).unwrap();
        assert_eq!(d.stats().frames_dropped, 1);
        assert_eq!(d.stats().real_dropped, 0, "waiting for the first keyframe is not a real drop");
        assert_eq!(d.stats().frames_in, 0);
        d.stop();
        assert!(!d.is_running());
    }
}
