//! Foscam cameras' native HTTP streaming protocol (the non-standard
//! `SERVERPUSH` method + `FOSC` framing).
//!
//! These cameras do not use RTSP for live video. They answer an HTTP request
//! whose method is the non-standard `SERVERPUSH` with a continuous binary
//! stream. The request path is just `/`; the target camera and its sign-in
//! travel in a `FOSC`-framed login block in the request body:
//!
//! ```text
//! SERVERPUSH / HTTP/1.1
//! Host: <host>:<port>
//! ...
//!
//! [u32 prefix] "FOSC" [u32 blocklen] [block]
//! ```
//!
//! The video login block is 161 bytes: a one-byte flag, a 64-byte
//! null-padded user name, a 64-byte null-padded password, a 4-byte session
//! token, and null padding. The reply is a raw byte stream (no HTTP status
//! line) of `FOSC` header records interleaved with **Annex-B** H.264 (NAL
//! units delimited by start codes). This module demuxes the H.264, converts
//! it to AVCC through the shared [`crate::ingest::baichuan::avcc::AvccBridge`],
//! and emits [`crate::ingest::recorder::MediaItem::Video`] frames.
//!
//! Clean-room, written from captured traffic. The session token's exact
//! semantics, and the exact layout of the reply's FOSC header records, are
//! best-effort (see `token` and `H264Demux`): the demuxer deliberately skips
//! the FOSC records and reads only the Annex-B NAL units, which is enough to
//! recover the picture.

use crate::ingest::baichuan::avcc::{split_annexb, AvccBridge};
use crate::ingest::recorder::{MediaItem, VideoParams};
use anyhow::{anyhow, Context, Result};
use std::sync::{Arc, Mutex};
use std::time::Instant;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::sync::mpsc::UnboundedReceiver;
use tokio::sync::watch;

/// URL scheme for a Foscam camera using its native SERVERPUSH protocol:
/// `foscam://host[:port]`.
pub const SCHEME: &str = "foscam://";
/// Foscam's HTTP (and native streaming) port.
pub const DEFAULT_PORT: u16 = 88;

/// Whether this URL names a Foscam (SERVERPUSH) stream.
pub fn is_foscam_url(url: &str) -> bool {
    url.starts_with(SCHEME)
}

/// Parse `foscam://host[:port]` into (host, port). A missing port is 88.
pub fn parse_url(url: &str) -> Option<(String, u16)> {
    let rest = url.strip_prefix(SCHEME)?;
    let rest = rest.trim_end_matches('/');
    if rest.is_empty() {
        return None;
    }
    let (host, port) = if let Some(b) = rest.strip_prefix('[') {
        let (h, tail) = b.split_once(']')?;
        let port = tail.strip_prefix(':').and_then(|p| p.parse().ok()).unwrap_or(DEFAULT_PORT);
        (h.to_string(), port)
    } else if let Some((h, p)) = rest.rsplit_once(':') {
        if p.is_empty() || !p.bytes().all(|b| b.is_ascii_digit()) {
            (rest.to_string(), DEFAULT_PORT)
        } else {
            (h.to_string(), p.parse().ok()?)
        }
    } else {
        (rest.to_string(), DEFAULT_PORT)
    };
    if host.is_empty() {
        return None;
    }
    Some((host, port))
}

/// A per-connection session token. Its meaning is not fully known; the captures
/// show a small (a few million) per-connection value that is echoed in the
/// reply's FOSC records. A random value is used here.
fn token() -> u32 {
    rand::random::<u32>()
}

/// Append `bytes` truncated to 64 bytes and null-padded to exactly 64.
fn push_field64(buf: &mut Vec<u8>, bytes: &[u8]) {
    let n = bytes.len().min(64);
    buf.extend_from_slice(&bytes[..n]);
    buf.resize(buf.len() + (64 - n), 0);
}

/// Build the `SERVERPUSH` video request body (the FOSC login block).
fn video_request_body(username: &str, password: &str) -> Vec<u8> {
    let mut block = Vec::with_capacity(161);
    block.push(0x00); // flag byte (video stream)
    push_field64(&mut block, username.as_bytes());
    push_field64(&mut block, password.as_bytes());
    block.extend_from_slice(&token().to_le_bytes());
    block.resize(161, 0);
    debug_assert_eq!(block.len(), 161);

    let mut body = Vec::with_capacity(4 + 4 + 4 + 161);
    body.extend_from_slice(&0u32.to_le_bytes()); // prefix: 0 = video stream
    body.extend_from_slice(b"FOSC");
    body.extend_from_slice(&(block.len() as u32).to_le_bytes());
    body.extend_from_slice(&block);
    body
}

/// The full HTTP request (headers + body) sent to open a video stream.
fn request(host: &str, port: u16, username: &str, password: &str) -> Vec<u8> {
    let mut req = format!(
        "SERVERPUSH / HTTP/1.1\r\nHost: {host}:{port}\r\nAccept:*/*\r\nConnection: Close\r\n\r\n"
    )
    .into_bytes();
    req.extend_from_slice(&video_request_body(username, password));
    req
}

/// A playing Foscam stream. Dropping it closes the read task.
pub struct FoscamStream {
    pub rx: UnboundedReceiver<Result<MediaItem, String>>,
    params: Arc<Mutex<Option<VideoParams>>>,
    cancel: watch::Sender<bool>,
}

impl FoscamStream {
    /// Connect, send `SERVERPUSH`, and start demuxing the reply. Sign-in and
    /// the read loop run on a spawned task; this returns once the socket is
    /// connected and the request is written.
    pub async fn open(url: &str, username: &str, password: &str) -> Result<FoscamStream> {
        let (host, port) = parse_url(url).ok_or_else(|| anyhow!("bad foscam URL"))?;
        let addr = format!("{host}:{port}");
        let mut sock = TcpStream::connect(&addr)
            .await
            .with_context(|| format!("Foscam SERVERPUSH connect to {addr} failed"))?;
        sock.write_all(&request(&host, port, username, password))
            .await
            .context("writing the SERVERPUSH request failed")?;

        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        let params = Arc::new(Mutex::new(None));
        let params2 = params.clone();
        let (cancel_tx, cancel_rx) = watch::channel(false);

        tokio::spawn(async move {
            run(sock, tx, params2, cancel_rx).await;
        });

        Ok(FoscamStream { rx, params, cancel: cancel_tx })
    }

    /// Video parameters, once an SPS+PPS have been read.
    pub fn video_params(&self) -> Option<VideoParams> {
        self.params.lock().unwrap().clone()
    }
}

impl Drop for FoscamStream {
    fn drop(&mut self) {
        let _ = self.cancel.send(true);
    }
}

/// Read, demux and convert until the socket closes or the handle drops.
async fn run(
    mut sock: TcpStream,
    tx: tokio::sync::mpsc::UnboundedSender<Result<MediaItem, String>>,
    params: Arc<Mutex<Option<VideoParams>>>,
    mut cancel: watch::Receiver<bool>,
) {
    let mut demux = H264Demux::new();
    let mut bridge = AvccBridge::new(false, 0, 0);
    let start = Instant::now();
    let mut buf = [0u8; 32 * 1024];
    loop {
        let n = tokio::select! {
            r = sock.read(&mut buf) => match r {
                Ok(0) => break, // stream ended: flush the trailing frame below
                Ok(n) => n,
                Err(e) => {
                    let _ = tx.send(Err(format!("Foscam read failed: {e}")));
                    return;
                }
            },
            _ = cancel.changed() => return,
        };
        for au in demux.push(&buf[..n]) {
            if !convert(&mut bridge, params.as_ref(), &start, au, &tx) {
                return;
            }
        }
    }
    for au in demux.finish() {
        if !convert(&mut bridge, params.as_ref(), &start, au, &tx) {
            return;
        }
    }
}

/// Turn one Annex-B access unit into a `MediaItem::Video` and send it.
/// Returns false when the receiver is gone (stop reading).
fn convert(
    bridge: &mut AvccBridge,
    params: &Mutex<Option<VideoParams>>,
    start: &Instant,
    au: Vec<u8>,
    tx: &tokio::sync::mpsc::UnboundedSender<Result<MediaItem, String>>,
) -> bool {
    // Pick up picture size from the SPS when a new one arrives.
    for nal in split_annexb(&au) {
        if nal.first().map(|b| b & 0x1f) == Some(NAL_SPS) {
            if let Some((w, h)) = sps_dimensions(nal) {
                bridge.set_dimensions(w, h);
            }
        }
    }
    // The reply has no usable timestamps: time frames by arrival on this
    // machine's clock, 90 kHz, monotonic from the first frame.
    let ticks = start.elapsed().as_micros() as i64 * 9 / 100;
    let Some(bf) = bridge.push_frame(&au) else { return true };
    if bf.new_params {
        *params.lock().unwrap() = bridge.video_params();
    }
    let item = MediaItem::Video { ticks, cts: 0, data: bf.data, key: bf.key, new_params: bf.new_params };
    tx.send(Ok(item)).is_ok()
}

// ---------------------------------------------------------------------------
// H.264 Annex-B demux
// ---------------------------------------------------------------------------

/// NAL unit types this reader accepts (the ones a Foscam stream sends).
const NAL_NON_IDR: u8 = 1;
const NAL_IDR: u8 = 5;
const NAL_SEI: u8 = 6;
const NAL_SPS: u8 = 7;
const NAL_PPS: u8 = 8;
const NAL_AUD: u8 = 9;

/// Splits a mixed byte stream (FOSC headers + Annex-B H.264) into H.264
/// access units. FOSC headers are skipped: only bytes between Annex-B start
/// codes that look like real NAL units are kept.
struct H264Demux {
    /// Bytes not yet consumed.
    buf: Vec<u8>,
    /// SPS/PPS/SEI/AUD NALs (with 4-byte start codes) awaiting a picture.
    prefix: Vec<u8>,
}

impl H264Demux {
    fn new() -> Self {
        H264Demux { buf: Vec::new(), prefix: Vec::new() }
    }

    /// Feed bytes; returns complete access units (Annex-B, start codes
    /// included) ready for [`AvccBridge::push_frame`].
    fn push(&mut self, data: &[u8]) -> Vec<Vec<u8>> {
        self.buf.extend_from_slice(data);
        let mut out = Vec::new();
        loop {
            let first = match find_start_code(&self.buf, 0) {
                Some((s, _)) => s,
                None => {
                    self.trim_junk();
                    break;
                }
            };
            if first > 0 {
                // Bytes before the first start code are FOSC headers / junk.
                self.buf.drain(..first);
            }
            let (_, body) = match find_start_code(&self.buf, 0) {
                Some(sc) => sc,
                None => break,
            };
            let Some((next, _)) = find_start_code(&self.buf, body) else { break };
            let mut end = next;
            while end > body && self.buf[end - 1] == 0 {
                end -= 1;
            }
            if end > body {
                let nal = self.buf[body..end].to_vec();
                self.ingest_nal(nal, &mut out);
            }
            self.buf.drain(..next);
        }
        out
    }

    /// No start code in the buffer: keep only the tail of a possible partial
    /// start code; drop the rest (FOSC header bytes).
    fn trim_junk(&mut self) {
        if self.buf.len() > 3 {
            let keep = self.buf.len() - 3;
            self.buf.drain(..keep);
        }
    }

    /// Flush the trailing NAL (the stream ended: no next start code will
    /// delimit it) and drop any lone parameter sets.
    fn finish(&mut self) -> Vec<Vec<u8>> {
        let mut out = Vec::new();
        if let Some((start, _)) = find_start_code(&self.buf, 0) {
            if start > 0 {
                self.buf.drain(..start);
            }
            if let Some((_, body)) = find_start_code(&self.buf, 0) {
                let mut end = self.buf.len();
                while end > body && self.buf[end - 1] == 0 {
                    end -= 1;
                }
                if end > body {
                    let nal = self.buf[body..end].to_vec();
                    self.ingest_nal(nal, &mut out);
                }
            }
        }
        self.buf.clear();
        self.prefix.clear();
        out
    }

    fn ingest_nal(&mut self, nal: Vec<u8>, out: &mut Vec<Vec<u8>>) {
        let ty = nal.first().map(|b| b & 0x1f);
        match ty {
            Some(NAL_SPS) | Some(NAL_PPS) | Some(NAL_SEI) | Some(NAL_AUD) => {
                self.prefix.extend_from_slice(&[0, 0, 0, 1]);
                self.prefix.extend_from_slice(&nal);
            }
            Some(NAL_IDR) | Some(NAL_NON_IDR) => {
                let mut au = std::mem::take(&mut self.prefix);
                au.extend_from_slice(&[0, 0, 0, 1]);
                au.extend_from_slice(&nal);
                out.push(au);
            }
            _ => {} // unknown type: not picture data
        }
    }
}

/// Find the next Annex-B start code at or after `from`, as (start, NAL-body
/// offset just past the code). Matches `00 00 01`; a 4-byte `00 00 00 01` is
/// found at its second zero, its leading zero left as the previous NAL's
/// trailing zero (see the trim in `push`).
fn find_start_code(data: &[u8], from: usize) -> Option<(usize, usize)> {
    let mut i = from;
    while i + 3 <= data.len() {
        if data[i] == 0 && data[i + 1] == 0 && data[i + 2] == 1 {
            return Some((i, i + 3));
        }
        i += 1;
    }
    None
}

// ---------------------------------------------------------------------------
// SPS picture size
// ---------------------------------------------------------------------------

/// Remove H.264 emulation-prevention bytes (`00 00 03` -> `00 00`).
fn unescape(data: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(data.len());
    let mut zeros = 0;
    for &b in data {
        if zeros >= 2 && b == 3 {
            zeros = 0;
            continue;
        }
        zeros = if b == 0 { zeros + 1 } else { 0 };
        out.push(b);
    }
    out
}

/// H.264 profiles that carry chroma/bit-depth fields before the picture size.
const HIGH_PROFILES: [u8; 13] = [100, 110, 122, 244, 44, 83, 86, 118, 128, 138, 139, 134, 135];

/// A tiny big-endian bit reader with Exp-Golomb codes.
struct Bits<'a> {
    b: &'a [u8],
    pos: usize,
}

impl<'a> Bits<'a> {
    fn new(b: &'a [u8]) -> Self {
        Bits { b, pos: 0 }
    }
    fn bit(&mut self) -> Option<u32> {
        let v = (self.b.get(self.pos / 8)? >> (7 - self.pos % 8)) & 1;
        self.pos += 1;
        Some(v as u32)
    }
    fn flag(&mut self) -> Option<bool> {
        self.bit().map(|b| b == 1)
    }
    fn skip(&mut self, n: usize) -> Option<()> {
        self.pos += n;
        (self.pos <= self.b.len() * 8).then_some(())
    }
    fn ue(&mut self) -> Option<u32> {
        let mut zeros = 0;
        while self.bit()? == 0 {
            zeros += 1;
            if zeros > 31 {
                return None;
            }
        }
        Some((1u32 << zeros) - 1 + self.bits(zeros)?)
    }
    fn se(&mut self) -> Option<i32> {
        let v = self.ue()? as i32;
        Some((v + 1) / 2 * if v & 1 == 0 { -1 } else { 1 })
    }
    fn bits(&mut self, n: u32) -> Option<u32> {
        let mut v = 0;
        for _ in 0..n {
            v = (v << 1) | self.bit()?;
        }
        Some(v)
    }
}

/// Picture size (width, height) from an H.264 SPS NAL (header byte included).
/// Returns `None` when the SPS is unusual (e.g. carries scaling matrices).
fn sps_dimensions(nal: &[u8]) -> Option<(u16, u16)> {
    if nal.first().map(|b| b & 0x1f) != Some(NAL_SPS) {
        return None;
    }
    let rbsp = unescape(&nal[1..]);
    let r = &mut Bits::new(&rbsp);
    let profile = r.bits(8)? as u8;
    r.skip(16)?; // constraint flags + level_idc
    r.ue()?; // seq_parameter_set_id
    if HIGH_PROFILES.contains(&profile) {
        let chroma_format_idc = r.ue()?;
        if chroma_format_idc == 3 {
            r.skip(1)?;
        }
        r.ue()?; // bit_depth_luma_minus8
        r.ue()?; // bit_depth_chroma_minus8
        r.skip(1)?; // qpprime_y_zero_transform_bypass_flag
        if r.flag()? {
            return None; // scaling matrices: unsupported here
        }
    }
    r.ue()?; // log2_max_frame_num_minus4
    let poc = r.ue()?;
    match poc {
        0 => {
            r.ue()?;
        }
        1 => {
            r.skip(1)?;
            r.se()?;
            r.se()?;
            let n = r.ue()?;
            for _ in 0..n {
                r.se()?;
            }
        }
        _ => {}
    }
    r.ue()?; // max_num_ref_frames
    r.skip(1)?; // gaps_in_frame_num_value_allowed_flag
    let width_mbs = r.ue()?;
    let height_mbs = r.ue()?;
    let frame_mbs_only = r.flag()?;
    if !frame_mbs_only {
        r.skip(1)?;
    }
    r.skip(1)?; // direct_8x8_inference_flag
    let cropping = r.flag()?;
    let (mut cl, mut cr, mut ct, mut cb) = (0u32, 0u32, 0u32, 0u32);
    if cropping {
        cl = r.ue()?;
        cr = r.ue()?;
        ct = r.ue()?;
        cb = r.ue()?;
    }
    let mut width = (width_mbs + 1) * 16;
    let mut height = (2 - frame_mbs_only as u32) * (height_mbs + 1) * 16;
    if cropping {
        // 4:2:0 (the common case): crop units are 2 luma samples wide, and
        // (2 - frame_mbs_only) * 2 tall.
        let cu_x = 2u32;
        let cu_y = (2 - frame_mbs_only as u32) * 2;
        width = width.saturating_sub((cl + cr) * cu_x);
        height = height.saturating_sub((ct + cb) * cu_y);
    }
    Some((width.min(u16::MAX as u32) as u16, height.min(u16::MAX as u32) as u16))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_urls() {
        assert!(is_foscam_url("foscam://cam.example"));
        assert!(!is_foscam_url("rtsp://cam.example"));
        assert_eq!(parse_url("foscam://cam.example"), Some(("cam.example".into(), 88)));
        assert_eq!(parse_url("foscam://cam.example:554"), Some(("cam.example".into(), 554)));
        assert_eq!(parse_url("foscam://cam.example/"), Some(("cam.example".into(), 88)));
        assert_eq!(parse_url("foscam://[fd00::5]:88"), Some(("fd00::5".into(), 88)));
        assert_eq!(parse_url("foscam://"), None);
        assert_eq!(parse_url("rtsp://x"), None);
    }

    #[test]
    fn request_body_layout() {
        let body = video_request_body("jeff", "99rm250");
        assert_eq!(body.len(), 4 + 4 + 4 + 161);
        assert_eq!(&body[0..4], &0u32.to_le_bytes());
        assert_eq!(&body[4..8], b"FOSC");
        let block_len = u32::from_le_bytes(body[8..12].try_into().unwrap()) as usize;
        assert_eq!(block_len, 161);
        let block = &body[12..];
        assert_eq!(block[0], 0x00, "flag byte");
        // username field: "jeff" then nulls to 64
        assert_eq!(&block[1..5], b"jeff");
        assert!(block[5..65].iter().all(|&b| b == 0));
        // password field starts at offset 65
        assert_eq!(&block[65..72], b"99rm250");
        assert!(block[72..129].iter().all(|&b| b == 0));
    }

    fn annexb(nals: &[&[u8]]) -> Vec<u8> {
        let mut v = Vec::new();
        for n in nals {
            v.extend_from_slice(&[0, 0, 0, 1]);
            v.extend_from_slice(n);
        }
        v
    }

    #[test]
    fn demux_skips_junk_and_groups_keyframes() {
        let sps = [0x67u8, 0x64, 0x00, 0x1f];
        let pps = [0x68u8, 0xeb, 0xe3];
        let idr = [0x65u8, 0x88, 0x84];
        let p = [0x41u8, 0x9a, 0x02];
        // A fake FOSC header followed by SPS+PPS+IDR, then a P-frame.
        let junk = b"junkjunkjunkjunk\x00\x00\x00\x1aFOSC";
        let mut stream = Vec::new();
        stream.extend_from_slice(junk);
        stream.extend_from_slice(&annexb(&[&sps, &pps, &idr]));
        stream.extend_from_slice(&annexb(&[&p]));

        let mut d = H264Demux::new();
        let mut aus = d.push(&stream);
        // The trailing P-frame has no following start code: flushed at end.
        aus.extend(d.finish());
        assert_eq!(aus.len(), 2, "one keyframe AU + one P-frame AU");
        // First AU holds SPS+PPS+IDR; second holds the P-frame.
        assert!(aus[0].windows(3).any(|w| w == idr));
        assert!(aus[1].windows(3).any(|w| w == p));
    }

    #[test]
    fn demux_emits_a_frame_once_the_next_start_code_arrives() {
        let idr = [0x65u8, 0x88, 0x84];
        let p = [0x41u8, 0x9a, 0x02];
        let full = annexb(&[&idr, &p]);
        let mut d = H264Demux::new();
        let mut aus = Vec::new();
        for chunk in full.chunks(4) {
            aus.extend(d.push(chunk));
        }
        aus.extend(d.finish());
        assert_eq!(aus.len(), 2);
        assert!(aus[0].windows(3).any(|w| w == idr));
        assert!(aus[1].windows(3).any(|w| w == p));
    }

    #[test]
    fn demux_flushes_the_trailing_frame_on_finish() {
        let p = [0x41u8, 0x9a, 0x02];
        let full = annexb(&[&p]);
        let mut d = H264Demux::new();
        assert!(d.push(&full).is_empty(), "no next start code yet");
        let aus = d.finish();
        assert_eq!(aus.len(), 1);
        assert!(aus[0].windows(3).any(|w| w == p));
    }
}
