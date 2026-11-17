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
/// A hardware decoder that sends no picture for this long while frames go
/// in is taken for stuck.
const HW_STALL: Duration = Duration::from_secs(10);
/// Frames that must have gone in without a picture coming out before a
/// hardware decoder counts as stuck.
const HW_STALL_FRAMES: u64 = 40;
/// This many error lines within a minute from a hardware decoder: it is
/// replaced by software decoding.
const HW_ERROR_BURST: u32 = 30;

/// Whether a hardware decoder that has run this long and given this many
/// pictures has run well enough to forget the camera's earlier failures.
fn healthy(ran: Duration, pictures: u64) -> bool {
    ran >= crate::hwdecode::HEALTHY_RUN && pictures > 0
}

/// What went wrong with a hardware decoder.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Trouble {
    /// It exited.
    Stopped,
    /// Many error lines: more often this camera's stream than the card.
    ErrorBurst,
    /// Frames went in, no pictures came out.
    Stalled,
}

impl Trouble {
    /// Whether the card may be blamed (and skipped for every camera with
    /// that codec for an hour) once software decoding then works. An error
    /// burst only moves this camera to the processor.
    fn blames_card(self) -> bool {
        !matches!(self, Trouble::ErrorBurst)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Codec {
    H264,
    H265,
}

impl Codec {
    pub fn ffmpeg_format(self) -> &'static str {
        match self {
            Codec::H264 => "h264",
            Codec::H265 => "hevc",
        }
    }
}

/// An H.265 access unit (4-byte length-prefixed NAL units) whose first
/// picture slice is a RASL picture (types 8 and 9): a leading picture of a
/// CRA or BLA that may refer to pictures before it, so it cannot be decoded
/// when decoding starts at that CRA or BLA.
pub fn is_rasl(data: &[u8]) -> bool {
    let mut pos = 0;
    while let Some(hdr) = data.get(pos..pos + 4) {
        let len = u32::from_be_bytes([hdr[0], hdr[1], hdr[2], hdr[3]]) as usize;
        let Some(&b) = data.get(pos + 4) else { return false };
        let t = (b >> 1) & 0x3f;
        if t < 32 {
            return matches!(t, 8 | 9);
        }
        pos = pos + 4 + len;
    }
    false
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

/// What a hardware decoder printed: the first error line, and whether
/// errors came in a burst.
#[derive(Default)]
struct ErrWatch {
    first: std::sync::Mutex<Option<String>>,
    burst: std::sync::atomic::AtomicBool,
    /// It said the system would not start another thread (a process limit,
    /// not the card).
    spawn_limit: std::sync::atomic::AtomicBool,
}

impl ErrWatch {
    /// One line of the decoder's stderr (not empty).
    fn see(&self, line: &str) {
        let mut f = self.first.lock().unwrap();
        if f.is_none() {
            *f = crate::hwdecode::reason_from_stderr(line);
        }
        if crate::proclimit::is_spawn_limit_line(line) {
            self.spawn_limit.store(true, Ordering::SeqCst);
        }
    }

    /// Why it failed when that was the process limit (in words), else None.
    fn limit_reason(&self) -> Option<String> {
        self.spawn_limit.load(Ordering::SeqCst).then(crate::proclimit::limit_words_now)
    }
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
    /// The hardware decoder it runs with (None: software).
    hw: Option<String>,
    errs: Arc<ErrWatch>,
}

pub struct FrameDecoder {
    codec: Codec,
    fps: u32,
    scale: u32,
    /// Privacy mask filter drawn on every picture (empty: none).
    masks: String,
    param_sets: Vec<u8>,
    pipe: Option<Pipeline>,
    out_tx: Option<SyncSender<Vec<u8>>>,
    frames: Receiver<Vec<u8>>,
    counters: Arc<Counters>,
    seen_key: bool,
    /// Input was lost after a gap and is skipped until the next keyframe.
    gap: bool,
    /// H.265: decoding (re)started at a keyframe, so RASL pictures right
    /// after it are skipped (they refer to pictures before a CRA).
    skip_rasl: bool,
    /// Chunks sent while a hardware decoder is still unproven.
    replay: Vec<Vec<u8>>,
    hw_failed: bool,
    /// Why the hardware decoder was replaced.
    hw_reason: Option<String>,
    /// The Status note for it: how often that way failed for this camera
    /// and when it is tried again.
    hw_note: Option<String>,
    /// The hardware decoder that was replaced, to blame once software
    /// decoding of the same stream works: (spec, why, pictures out then).
    blame: Option<(String, String, u64)>,
    /// Pictures out at the last look, when that number last changed, and
    /// frames in since (stuck-decoder check).
    seen_out: u64,
    seen_out_at: Instant,
    in_since_out: u64,
    /// The camera, for the log and Status.
    camera: String,
    /// The hardware decoder has run `hwdecode::HEALTHY_RUN` well, and the
    /// camera's earlier failures of that way were forgotten.
    hw_ok_noted: bool,
}

/// The filter that brings decoded pictures from the graphics card into
/// memory (8-bit or 10-bit), for the CPU filters after it.
pub const HW_DOWNLOAD: &str = "hwdownload,format=nv12|p010le";

/// ffmpeg input arguments for a hardware decoder `spec` ("vaapi:/dev/dri/renderD129",
/// "qsv:/dev/dri/renderD128", "d3d11va", ...). The pictures stay on the card
/// (`-hwaccel_output_format`), so a decoder that cannot start fails instead of
/// quietly decoding on the processor; `HW_DOWNLOAD` brings them back. Quick
/// Sync needs its decoder named, so the codec when it is known.
pub fn hw_input_args(spec: &str, codec: Option<Codec>) -> Result<Vec<String>> {
    let (kind, device) = match spec.split_once(':') {
        Some((k, d)) if !d.is_empty() => (k, Some(d.to_string())),
        Some((k, _)) => (k, None),
        None => (spec, None),
    };
    let s = |v: &[&str]| v.iter().map(|x| x.to_string()).collect::<Vec<String>>();
    match kind {
        "vaapi" => {
            let d = device.unwrap_or_else(|| "/dev/dri/renderD128".into());
            Ok(s(&["-hwaccel", "vaapi", "-hwaccel_device", &d, "-hwaccel_output_format", "vaapi"]))
        }
        "qsv" => {
            let mut v = Vec::new();
            if let Some(d) = device {
                v.extend(["-qsv_device".to_string(), d]);
            }
            v.extend(s(&["-hwaccel", "qsv", "-hwaccel_output_format", "qsv"]));
            match codec {
                Some(Codec::H264) => v.extend(s(&["-c:v", "h264_qsv"])),
                Some(Codec::H265) => v.extend(s(&["-c:v", "hevc_qsv"])),
                None => {}
            }
            Ok(v)
        }
        "cuda" => Ok(s(&["-hwaccel", "cuda", "-hwaccel_output_format", "cuda"])),
        "vulkan" => Ok(s(&["-hwaccel", "vulkan", "-hwaccel_output_format", "vulkan"])),
        "videotoolbox" => Ok(s(&["-hwaccel", "videotoolbox", "-hwaccel_output_format", "videotoolbox_vld"])),
        "d3d11va" => Ok(s(&["-hwaccel", "d3d11va", "-hwaccel_output_format", "d3d11"])),
        "dxva2" => Ok(s(&["-hwaccel", "dxva2", "-hwaccel_output_format", "dxva2_vld"])),
        other => bail!("unsupported hwaccel '{other}'"),
    }
}

/// `masks`: privacy mask filter ([`crate::privacy::mask_filter`]), drawn on
/// the analysed pictures (empty: none).
fn ffmpeg_args(codec: Codec, fps: u32, scale: u32, hw: Option<&str>, masks: &str) -> Result<Vec<String>> {
    let mut a: Vec<String> = ["-hide_banner", "-loglevel", "error"].map(String::from).into();
    if let Some(spec) = hw {
        a.extend(hw_input_args(spec, Some(codec))?);
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
    // On the card: drop frames there first, so only the analysed ones are
    // copied back.
    let mut vf = format!("fps={}", fps.max(1));
    if hw.is_some() {
        vf.push(',');
        vf.push_str(HW_DOWNLOAD);
    }
    if scale > 0 {
        vf.push_str(&format!(",scale={scale}:-2"));
    }
    // (In memory, after the card's pictures came back, and on the small
    // picture: the boxes are sized from it.)
    if !masks.is_empty() {
        vf.push(',');
        vf.push_str(masks);
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
        masks: &str,
        out_tx: SyncSender<Vec<u8>>,
        counters: Arc<Counters>,
    ) -> Result<Self> {
        let args = ffmpeg_args(codec, fps, scale, hw, masks)?;
        let mut child = crate::child::ffmpeg()
            .args(&args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(if hw.is_some() { Stdio::piped() } else { Stdio::null() })
            .spawn()
            .context("failed to start ffmpeg")?;
        crate::child::adopt(&child);
        let mut stdin = child.stdin.take().context("ffmpeg stdin unavailable")?;
        let mut stdout = child.stdout.take().context("ffmpeg stdout unavailable")?;
        let started = Instant::now();
        let exited_ms = Arc::new(AtomicU64::new(u64::MAX));
        let errs = Arc::new(ErrWatch::default());
        // A hardware decoder's complaints: the first says why it failed; many
        // in a short time mean it decodes badly.
        if let Some(e) = child.stderr.take() {
            let w = errs.clone();
            std::thread::spawn(move || {
                use std::io::BufRead;
                let mut window = Instant::now();
                let mut n = 0u32;
                for line in std::io::BufReader::new(e).lines() {
                    let Ok(line) = line else { break };
                    if line.trim().is_empty() {
                        continue;
                    }
                    w.see(&line);
                    if window.elapsed() > Duration::from_secs(60) {
                        window = Instant::now();
                        n = 0;
                    }
                    n += 1;
                    if n >= HW_ERROR_BURST {
                        w.burst.store(true, Ordering::SeqCst);
                    }
                }
            });
        }

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
            hw: hw.map(String::from),
            errs,
        })
    }

    fn alive(&self) -> bool {
        self.exited_ms.load(Ordering::SeqCst) == u64::MAX
    }

    /// Whether it ended because it was told to from outside: AmberSight is
    /// stopping, or ffmpeg got a signal (a service stop signals every process
    /// of the service). Not a failure of the decoder.
    fn stopped_from_outside(&mut self) -> bool {
        if crate::child::stopping() {
            return true;
        }
        if self.alive() {
            return false;
        }
        // (Its output closed a moment before it exits.)
        let deadline = Instant::now() + Duration::from_millis(200);
        loop {
            match self.child.try_wait() {
                Ok(Some(st)) => return crate::child::ended_by_signal(&st),
                Ok(None) if Instant::now() < deadline => std::thread::sleep(Duration::from_millis(10)),
                _ => return false,
            }
        }
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
        Self::start_masked(codec, extra_data, fps, scale, hwaccel, String::new())
    }

    /// As [`FrameDecoder::start`], with privacy masks (`masks`, a filter
    /// from [`crate::privacy::mask_filter`]) blacked out in every picture.
    pub fn start_masked(codec: Codec, extra_data: &[u8], fps: u32, scale: u32, hwaccel: Option<String>, masks: String) -> Result<Self> {
        let param_sets = parameter_sets_annexb(codec, extra_data)?;
        let (out_tx, frames) = mpsc::sync_channel(OUTPUT_QUEUE);
        let counters = Arc::new(Counters::default());
        let pipe = Pipeline::spawn(codec, fps, scale, hwaccel.as_deref(), &masks, out_tx.clone(), counters.clone())?;
        Ok(FrameDecoder {
            codec,
            fps,
            scale,
            masks,
            param_sets,
            pipe: Some(pipe),
            out_tx: Some(out_tx),
            frames,
            counters,
            seen_key: false,
            gap: false,
            skip_rasl: false,
            replay: Vec::new(),
            hw_failed: false,
            hw_reason: None,
            hw_note: None,
            blame: None,
            seen_out: 0,
            seen_out_at: Instant::now(),
            in_since_out: 0,
            camera: String::new(),
            hw_ok_noted: false,
        })
    }

    /// Name the camera (the log, and what a failing card is blamed for).
    pub fn with_camera(mut self, name: &str) -> Self {
        self.camera = name.to_string();
        self
    }

    /// Why the hardware decoder was replaced by software decoding.
    pub fn hw_reason(&self) -> Option<&str> {
        self.hw_reason.as_deref()
    }

    /// The Status note after a fallback: "VAAPI on ... failed twice (...);
    /// trying it again at 10:42".
    pub fn hw_note(&self) -> Option<&str> {
        self.hw_note.as_deref()
    }

    /// Decoded JPEG frames (SOI..next SOI), at most `fps` per second.
    pub fn frames(&self) -> &Receiver<Vec<u8>> {
        &self.frames
    }

    /// True if hardware decoding was requested but did not work.
    pub fn hw_fallback_used(&self) -> bool {
        self.hw_failed
    }

    /// True while the current ffmpeg process is running with hardware decode.
    pub fn using_hwaccel(&self) -> bool {
        self.pipe.as_ref().is_some_and(|p| p.hw.is_some() && p.alive())
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
            self.skip_rasl = self.codec == Codec::H265;
        } else if self.skip_rasl && !is_keyframe {
            if is_rasl(data) {
                // Refers to pictures before the keyframe this decode started at.
                self.counters.frames_dropped.fetch_add(1, Ordering::Relaxed);
                return Ok(());
            }
            self.skip_rasl = false;
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

        self.in_since_out += 1;
        let fell_back = self.maybe_fall_back()?;
        if fell_back == Some(false) && !is_keyframe {
            // A software decoder started mid-stream: from the next keyframe.
            self.seen_key = false;
            self.counters.frames_dropped.fetch_add(1, Ordering::Relaxed);
            return Ok(());
        }
        if fell_back == Some(false) {
            // It starts at this keyframe: H.265 RASL pictures after it refer
            // to pictures it never had.
            self.skip_rasl = self.codec == Codec::H265;
        }
        self.settle_blame();
        let pipe = self.pipe.as_mut().context("decoder stopped")?;
        if pipe.hw.is_some() && pipe.started.elapsed() < HW_FALLBACK_WINDOW {
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
                if self.maybe_fall_back()?.is_some() {
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

    /// What is wrong with the hardware decoder, if anything: it stopped, it
    /// reported a burst of errors, or no picture came out for a while as
    /// frames went in. The flag: whether the card may be blamed for it (an
    /// exit or a stall; a burst of errors is more likely this camera's stream).
    fn hw_trouble(&mut self) -> Option<(String, bool)> {
        let out = self.counters.jpegs_out.load(Ordering::Relaxed);
        if out != self.seen_out {
            self.seen_out = out;
            self.seen_out_at = Instant::now();
            self.in_since_out = 0;
        }
        let pipe = self.pipe.as_ref()?;
        pipe.hw.as_ref()?;
        let first = || pipe.errs.first.lock().unwrap().clone();
        let writer_dead = pipe.writer.as_ref().is_some_and(|w| w.is_finished());
        if !pipe.alive() || writer_dead {
            // (Give the stderr reader a moment to catch the reason.)
            std::thread::sleep(Duration::from_millis(30));
            return Some((first().unwrap_or_else(|| "the decoder stopped".into()), Trouble::Stopped.blames_card()));
        }
        if pipe.errs.burst.load(Ordering::SeqCst) {
            return Some((format!("many decoding errors: {}", first().unwrap_or_default()), Trouble::ErrorBurst.blames_card()));
        }
        if self.in_since_out >= HW_STALL_FRAMES && self.seen_out_at.elapsed() >= HW_STALL && pipe.started.elapsed() >= HW_STALL {
            return Some(("no pictures came out".into(), Trouble::Stalled.blames_card()));
        }
        None
    }

    /// If the hardware decoder is in trouble, restart in software (replaying
    /// what it was sent when it failed at once). Returns whether it switched
    /// at once (`Some(true)`: the input was replayed) or mid-stream.
    fn maybe_fall_back(&mut self) -> Result<Option<bool>> {
        let Some((why, blamable)) = self.hw_trouble() else {
            self.note_healthy();
            return Ok(None);
        };
        // Stopped from outside (AmberSight is stopping, or ffmpeg was sent a
        // signal): nothing to blame or switch; the caller sees the decoder gone.
        if self.pipe.as_mut().is_some_and(|p| p.stopped_from_outside()) {
            return Ok(None);
        }
        let Some(old) = self.pipe.take() else { return Ok(None) };
        let early = old.started.elapsed() < HW_FALLBACK_WINDOW;
        let spec = old.hw.clone().unwrap_or_default();
        // Out of threads: the system's limit, not the card. Not counted
        // against the way (the camera's move back is looked at as usual).
        let limit = old.errs.limit_reason();
        let why = limit.clone().unwrap_or(why);
        old.shutdown();
        self.hw_failed = true;
        tracing::info!(
            "camera '{}': hardware decoding ({}) stopped working ({why}); decoding on the processor",
            self.camera,
            crate::hwdecode::describe(&spec)
        );
        self.hw_reason = Some(why.clone());
        // This camera leaves that way alone for a while (15 minutes at
        // first, longer each time it fails again); the card itself is blamed
        // for every camera only once software decoding works (`settle_blame`).
        self.hw_note = Some(match &limit {
            Some(words) => words.clone(),
            None => crate::hwdecode::note_camera_failure(&self.camera, &spec, Some(self.codec), &why),
        });
        let out_tx = self.out_tx.clone().context("decoder stopped")?;
        let new = Pipeline::spawn(self.codec, self.fps, self.scale, None, &self.masks, out_tx, self.counters.clone())?;
        if early {
            if let Some(tx) = new.input.as_ref() {
                for c in self.replay.drain(..) {
                    let _ = tx.try_send(c);
                }
            }
        } else {
            self.replay.clear();
        }
        // (A burst of errors moves only this camera to the processor; the
        // card stays in use for the others. So does a card that was out of
        // memory: it may have room again soon, and the headroom guard keeps
        // Auto off a full NVIDIA card.)
        // (Out-of-memory wording on any way, Quick Sync included, is never blamed on the whole
        // card; whether it counts toward this camera giving the way up is memory_trouble.)
        let blamable = blamable && limit.is_none() && !crate::hwdecode::memory_words(&why);
        self.blame = blamable.then(|| (spec, why, self.counters.jpegs_out.load(Ordering::Relaxed)));
        self.pipe = Some(new);
        Ok(Some(early))
    }

    /// A hardware decoder that has run `hwdecode::HEALTHY_RUN` and still gives
    /// pictures: the camera's earlier failures of that way are forgotten, so
    /// only failures in a row lead to giving the way up.
    fn note_healthy(&mut self) {
        if self.hw_ok_noted || self.camera.is_empty() {
            return;
        }
        let Some(pipe) = self.pipe.as_ref() else { return };
        let Some(spec) = pipe.hw.as_deref() else { return };
        if healthy(pipe.started.elapsed(), self.counters.jpegs_out.load(Ordering::Relaxed)) {
            self.hw_ok_noted = true;
            crate::hwdecode::note_camera_ok(&self.camera, spec, Some(self.codec));
        }
    }

    /// Software decoding of the same stream works where the card failed:
    /// the card is to blame (other cameras skip it for that codec a while).
    /// When software fails too, the stream was at fault, not the card.
    fn settle_blame(&mut self) {
        let Some((spec, why, base)) = &self.blame else { return };
        if self.counters.jpegs_out.load(Ordering::Relaxed) >= base + 2 {
            crate::hwdecode::note_failure(spec, Some(self.codec), &self.camera, why);
            self.blame = None;
        }
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
            hw_input_args("vaapi", None).unwrap(),
            ["-hwaccel", "vaapi", "-hwaccel_device", "/dev/dri/renderD128", "-hwaccel_output_format", "vaapi"]
        );
        assert_eq!(hw_input_args("vaapi:/dev/dri/renderD129", None).unwrap()[3], "/dev/dri/renderD129");
        assert_eq!(hw_input_args("qsv", None).unwrap(), ["-hwaccel", "qsv", "-hwaccel_output_format", "qsv"]);
        assert_eq!(
            hw_input_args("qsv:/dev/dri/renderD128", Some(Codec::H265)).unwrap(),
            ["-qsv_device", "/dev/dri/renderD128", "-hwaccel", "qsv", "-hwaccel_output_format", "qsv", "-c:v", "hevc_qsv"]
        );
        assert!(hw_input_args("bogus", None).is_err());
        // Pictures are copied back after the frame-rate filter.
        let a = ffmpeg_args(Codec::H264, 5, 640, Some("vaapi:/dev/dri/renderD129"), "").unwrap();
        let vf = &a[a.iter().position(|x| x == "-vf").unwrap() + 1];
        assert_eq!(vf, "fps=5,hwdownload,format=nv12|p010le,scale=640:-2");
        let a = ffmpeg_args(Codec::H264, 5, 640, None, "").unwrap();
        assert!(!a.iter().any(|x| x.contains("hw")), "{a:?}");
    }

    /// The test sample's access units (length-prefixed) and its avcC record.
    fn sample_h264() -> (Vec<u8>, Vec<(Vec<u8>, bool)>) {
        let data = include_bytes!("../hwdecode/sample.h264");
        let mut starts = Vec::new();
        let mut i = 0;
        while i + 3 <= data.len() {
            if data[i] == 0 && data[i + 1] == 0 && data[i + 2] == 1 {
                starts.push(i + 3);
                i += 3;
            } else {
                i += 1;
            }
        }
        let mut nals: Vec<&[u8]> = Vec::new();
        for (n, &st) in starts.iter().enumerate() {
            let mut end = starts.get(n + 1).map(|&e| e - 3).unwrap_or(data.len());
            while end > st && data[end - 1] == 0 {
                end -= 1;
            }
            nals.push(&data[st..end]);
        }
        let sps: Vec<&[u8]> = nals.iter().copied().filter(|n| n[0] & 0x1f == 7).collect();
        let pps: Vec<&[u8]> = nals.iter().copied().filter(|n| n[0] & 0x1f == 8).collect();
        let frames = nals.iter().filter(|n| matches!(n[0] & 0x1f, 1 | 5)).map(|n| (lp(&[n]), n[0] & 0x1f == 5)).collect();
        (avcc(&sps, &pps), frames)
    }

    #[test]
    fn a_hardware_decoder_that_cannot_start_is_replaced_by_software() {
        if std::process::Command::new(crate::ffmpeg_bin::program()).arg("-version").output().is_err() {
            return;
        }
        let (rec, frames) = sample_h264();
        assert!(frames.len() >= 8 && frames[0].1, "{}", frames.len());
        let mut d = FrameDecoder::start(Codec::H264, &rec, 10, 160, Some("vaapi:/dev/dri/renderD999".into())).unwrap().with_camera("Test");
        let mut got = 0;
        for round in 0..6 {
            for (f, key) in &frames {
                d.push(f, *key).unwrap();
                std::thread::sleep(std::time::Duration::from_millis(40));
                while d.frames().try_recv().is_ok() {
                    got += 1;
                }
            }
            if got > 2 && round > 1 {
                break;
            }
        }
        assert!(d.hw_fallback_used(), "fell back");
        assert!(!d.using_hwaccel());
        assert!(d.hw_reason().is_some());
        assert!(d.hw_note().is_some_and(|n| n.contains("trying it again at")), "{:?}", d.hw_note());
        assert!(got > 0, "software decoding made pictures");
        d.stop();
    }

    #[test]
    fn privacy_masks_are_drawn_after_the_card_and_the_scaling() {
        let m = crate::privacy::mask_filter(&[crate::config::ZoneRect { x: 0.5, y: 0.5, w: 0.25, h: 0.25 }]);
        let a = ffmpeg_args(Codec::H264, 5, 640, Some("vaapi:/dev/dri/renderD129"), &m).unwrap();
        let vf = &a[a.iter().position(|x| x == "-vf").unwrap() + 1];
        assert_eq!(vf, &format!("fps=5,hwdownload,format=nv12|p010le,scale=640:-2,{m}"));
        let a = ffmpeg_args(Codec::H264, 5, 0, None, &m).unwrap();
        assert_eq!(a[a.iter().position(|x| x == "-vf").unwrap() + 1], format!("fps=5,{m}"));
    }

    /// Real ffmpeg: every picture of a masked decoder is black in the mask
    /// (the sample is ffmpeg's colour-bar test picture), also after a
    /// hardware decoder that cannot start was replaced.
    #[test]
    fn a_masked_decoder_gives_masked_pictures() {
        if std::process::Command::new(crate::ffmpeg_bin::program()).arg("-version").output().is_err() {
            return;
        }
        let (rec, frames) = sample_h264();
        let masks = crate::privacy::mask_filter(&[crate::config::ZoneRect { x: 0.5, y: 0.6, w: 0.25, h: 0.3 }]);
        for hw in [None, Some("vaapi:/dev/dri/renderD999".to_string())] {
            let mut d = FrameDecoder::start_masked(Codec::H264, &rec, 10, 160, hw.clone(), masks.clone()).unwrap();
            let mut got: Vec<Vec<u8>> = Vec::new();
            for _ in 0..6 {
                for (f, key) in &frames {
                    d.push(f, *key).unwrap();
                    std::thread::sleep(std::time::Duration::from_millis(30));
                    while let Ok(j) = d.frames().try_recv() {
                        got.push(j);
                    }
                }
                if got.len() > 2 {
                    break;
                }
            }
            d.stop();
            assert!(!got.is_empty(), "pictures ({hw:?})");
            for j in &got {
                let img = image::load_from_memory(j).unwrap().to_luma8();
                let (w, h) = img.dimensions();
                assert_eq!(w, 160);
                let inside = img.get_pixel(w * 5 / 8, h * 3 / 4).0[0];
                let outside = img.get_pixel(w / 16, h / 2).0[0];
                assert!(inside < 25 && outside > 40, "inside {inside}, outside {outside} ({hw:?})");
            }
        }
    }

    #[test]
    fn a_decoder_out_of_threads_is_not_a_card_failure() {
        let w = ErrWatch::default();
        w.see("[h264 @ 0x1] hwaccel initialisation returned error.");
        assert!(w.limit_reason().is_none(), "a card complaint alone");
        w.see("pthread_create() failed: Resource temporarily unavailable");
        w.see("[vist#0:0/h264 @ 0x1] Error while opening decoder: Resource temporarily unavailable");
        let why = w.limit_reason().expect("the process limit");
        assert!(why.starts_with("the system would not start another process or thread"), "{why}");
        assert!(w.first.lock().unwrap().is_some(), "the first complaint is still kept");
        let other = ErrWatch::default();
        other.see("[tcp @ 0x1] Resource temporarily unavailable");
        assert!(other.limit_reason().is_none());
    }

    #[test]
    fn only_exits_and_stalls_blame_the_card() {
        assert!(Trouble::Stopped.blames_card());
        assert!(Trouble::Stalled.blames_card());
        assert!(!Trouble::ErrorBurst.blames_card());
        // Forgetting earlier failures: only after a long run that gave pictures.
        assert!(healthy(crate::hwdecode::HEALTHY_RUN, 1));
        assert!(!healthy(crate::hwdecode::HEALTHY_RUN - Duration::from_secs(1), 1000));
        assert!(!healthy(crate::hwdecode::HEALTHY_RUN * 2, 0));
    }

    #[test]
    fn rasl_pictures_are_recognized() {
        let nal = |t: u8| vec![t << 1, 1, 0xaa];
        assert!(is_rasl(&lp(&[&nal(8)])));
        assert!(is_rasl(&lp(&[&nal(9)])));
        // Prefix SEI and AUD before the slice are passed over.
        assert!(is_rasl(&lp(&[&nal(35), &nal(39), &nal(8)])));
        for t in [0, 1, 6, 7, 19, 21] {
            assert!(!is_rasl(&lp(&[&nal(t)])), "type {t}");
        }
        assert!(!is_rasl(&[]));
        assert!(!is_rasl(&[0, 0, 0, 9]), "truncated");
    }

    #[test]
    fn h265_decoding_skips_rasl_pictures_after_its_first_keyframe() {
        if std::process::Command::new(crate::ffmpeg_bin::program()).arg("-version").output().is_err() {
            return;
        }
        // A minimal hvcC: 22 header bytes, then one VPS/SPS/PPS array each.
        let mut rec = vec![1u8];
        rec.extend_from_slice(&[0; 21]);
        rec.push(3);
        for t in [32u8, 33, 34] {
            rec.extend_from_slice(&[t, 0, 1, 0, 2, t << 1, 1]);
        }
        let nal = |t: u8| lp(&[&[t << 1, 1, 0xaa]]);
        let mut d = FrameDecoder::start(Codec::H265, &rec, 2, 320, None).unwrap();
        d.push(&nal(21), true).unwrap(); // CRA
        d.push(&nal(9), false).unwrap(); // RASL_R: skipped
        d.push(&nal(8), false).unwrap(); // RASL_N: skipped
        assert_eq!((d.stats().frames_in, d.stats().frames_dropped, d.stats().real_dropped), (1, 2, 0));
        d.push(&nal(1), false).unwrap(); // TRAIL_R: decoded
        d.push(&nal(21), true).unwrap(); // the next CRA, mid-decode
        d.push(&nal(8), false).unwrap(); // its RASL can be decoded now
        assert_eq!((d.stats().frames_in, d.stats().frames_dropped), (4, 2));
        d.stop();
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
