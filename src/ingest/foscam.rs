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
//!
//! H.264 only: a camera that sends H.265 is told apart and refused with a
//! plain error (its RTSP address still works). Video only: no audio, and
//! the foscam:// reader has no sub stream.

use crate::ingest::baichuan::avcc::{split_annexb, AvccBridge};
use crate::ingest::recorder::{MediaItem, VideoParams};
use crate::ingest::sps::sps_display_size;
use anyhow::{anyhow, Context, Result};
use std::sync::{Arc, Mutex};
use std::time::Instant;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::sync::mpsc::{Receiver, Sender};
use tokio::sync::watch;

/// Frames held between the read task and the recorder. Past it the read
/// task waits, so a camera sending faster than they are taken is slowed by
/// TCP instead of filling memory.
const CHANNEL_FRAMES: usize = 256;

/// URL scheme for a Foscam camera using its native SERVERPUSH protocol:
/// `foscam://host[:port]`.
pub const SCHEME: &str = "foscam://";
/// Foscam's HTTP (and native streaming) port.
pub const DEFAULT_PORT: u16 = 88;

/// Whether this URL names a Foscam (SERVERPUSH) stream. The scheme is
/// matched in any case ("Foscam://" too), as push.rs does.
pub fn is_foscam_url(url: &str) -> bool {
    strip_scheme(url).is_some()
}

/// What follows `foscam://` (matched in any case), or None.
fn strip_scheme(url: &str) -> Option<&str> {
    let url = url.trim();
    let head = url.get(..SCHEME.len())?;
    head.eq_ignore_ascii_case(SCHEME).then(|| &url[SCHEME.len()..])
}

/// Parse `foscam://host[:port]` into (host, port). A missing port is 88.
/// Only a host and port are accepted: no path, query or user info (the
/// sign-in goes in the camera's user name and password fields), an IPv6
/// address must be in brackets, and the host may not hold a space or a
/// control character (it goes into the request's Host header).
pub fn parse_url(url: &str) -> Option<(String, u16)> {
    let rest = strip_scheme(url)?;
    let rest = rest.trim_end_matches('/');
    if rest.is_empty() || rest.contains(['/', '?', '#', '@']) {
        return None;
    }
    let port_of = |p: &str| -> Option<u16> {
        if p.is_empty() || !p.bytes().all(|b| b.is_ascii_digit()) {
            return None;
        }
        p.parse().ok().filter(|&p| p > 0)
    };
    let (host, port) = if let Some(b) = rest.strip_prefix('[') {
        let (h, tail) = b.split_once(']')?;
        let port = match tail {
            "" => DEFAULT_PORT,
            t => port_of(t.strip_prefix(':')?)?,
        };
        (h, port)
    } else {
        match rest.split_once(':') {
            None => (rest, DEFAULT_PORT),
            // More than one colon: an IPv6 address without brackets.
            Some((_, p)) if p.contains(':') => return None,
            Some((h, p)) => (h, port_of(p)?),
        }
    };
    if host.is_empty() || host.contains(['[', ']']) || host.chars().any(|c| c == ' ' || c.is_ascii_control()) {
        return None;
    }
    Some((host.to_string(), port))
}

/// A per-connection session token. Its meaning is not fully known; the captures
/// show a small (a few million) per-connection value that is echoed in the
/// reply's FOSC records. A random value is used here.
fn token() -> u32 {
    rand::random::<u32>()
}

/// Longest user name or password the login block can carry: its 64-byte
/// fields are null-padded, so one byte is kept for the terminating NUL.
const MAX_CREDENTIAL: usize = 63;

/// Refuse a credential the 64-byte field cannot carry as written. The error
/// names the field (`what`), never its value.
fn check_credential(what: &str, value: &str) -> Result<()> {
    if value.len() > MAX_CREDENTIAL {
        return Err(anyhow!("the Foscam {what} is longer than {MAX_CREDENTIAL} bytes"));
    }
    if value.contains('\0') {
        return Err(anyhow!("the Foscam {what} contains a NUL character"));
    }
    Ok(())
}

/// Append `bytes` (at most 63, checked by `check_credential`) null-padded to
/// exactly 64.
fn push_field64(buf: &mut Vec<u8>, bytes: &[u8]) {
    let n = bytes.len().min(MAX_CREDENTIAL);
    buf.extend_from_slice(&bytes[..n]);
    buf.resize(buf.len() + (64 - n), 0);
}

/// Build the `SERVERPUSH` video request body (the FOSC login block).
fn video_request_body(username: &str, password: &str) -> Result<Vec<u8>> {
    check_credential("user name", username)?;
    check_credential("password", password)?;
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
    Ok(body)
}

/// `host:port`, with an IPv6 host in brackets.
fn host_port(host: &str, port: u16) -> String {
    if host.contains(':') {
        format!("[{host}]:{port}")
    } else {
        format!("{host}:{port}")
    }
}

/// The full HTTP request (headers + body) sent to open a video stream.
fn request(host: &str, port: u16, username: &str, password: &str) -> Result<Vec<u8>> {
    let body = video_request_body(username, password)?;
    let mut req = format!(
        "SERVERPUSH / HTTP/1.1\r\nHost: {}\r\nAccept:*/*\r\nConnection: Close\r\n\r\n",
        host_port(host, port)
    )
    .into_bytes();
    req.extend_from_slice(&body);
    Ok(req)
}

/// A playing Foscam stream. Dropping it closes the read task.
pub struct FoscamStream {
    pub rx: Receiver<Result<MediaItem, String>>,
    params: Arc<Mutex<Option<VideoParams>>>,
    cancel: watch::Sender<bool>,
}

impl FoscamStream {
    /// Connect, send `SERVERPUSH`, and start demuxing the reply. Sign-in and
    /// the read loop run on a spawned task; this returns once the socket is
    /// connected and the request is written.
    pub async fn open(url: &str, username: &str, password: &str) -> Result<FoscamStream> {
        let (host, port) =
            parse_url(url).ok_or_else(|| anyhow!("bad foscam URL (expected foscam://host or foscam://host:port)"))?;
        // Built before connecting, so a user name or password that cannot be
        // sent is refused without touching the camera.
        let req = request(&host, port, username, password)?;
        let addr = host_port(&host, port);
        let mut sock = TcpStream::connect(&addr)
            .await
            .with_context(|| format!("Foscam SERVERPUSH connect to {addr} failed"))?;
        sock.write_all(&req)
            .await
            .context("writing the SERVERPUSH request failed")?;

        let (tx, rx) = tokio::sync::mpsc::channel(CHANNEL_FRAMES);
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

/// Read, demux and convert until the socket closes, the stream fails, or the
/// handle drops.
async fn run(
    mut sock: TcpStream,
    tx: Sender<Result<MediaItem, String>>,
    params: Arc<Mutex<Option<VideoParams>>>,
    mut cancel: watch::Receiver<bool>,
) {
    // A full channel waits in `send`; the handle dropping ends that wait too.
    tokio::select! {
        _ = read_loop(&mut sock, &tx, params.as_ref()) => {}
        _ = cancel.changed() => {}
    }
}

async fn read_loop(sock: &mut TcpStream, tx: &Sender<Result<MediaItem, String>>, params: &Mutex<Option<VideoParams>>) {
    let mut demux = H264Demux::new();
    let mut sign_in = SignInCheck::default();
    let mut conv = Converter { bridge: AvccBridge::new(false, 0, 0), start: Instant::now(), last_ticks: None };
    let mut buf = vec![0u8; 32 * 1024];
    loop {
        let n = match sock.read(&mut buf).await {
            Ok(0) => break, // stream ended
            Ok(n) => n,
            Err(e) => {
                let _ = tx.send(Err(format!("Foscam read failed: {e}"))).await;
                return;
            }
        };
        if let Err(e) = sign_in.push(&buf[..n]) {
            let _ = tx.send(Err(e)).await;
            return;
        }
        let aus = match demux.push(&buf[..n]) {
            Ok(aus) => aus,
            Err(e) => {
                let _ = tx.send(Err(e)).await;
                return;
            }
        };
        for au in aus {
            if let Some(item) = conv.convert(params, au) {
                if tx.send(Ok(item)).await.is_err() {
                    return; // the receiver is gone
                }
            }
        }
    }
    for au in demux.finish() {
        if let Some(item) = conv.convert(params, au) {
            if tx.send(Ok(item)).await.is_err() {
                return;
            }
        }
    }
}

/// 90 kHz ticks for a frame that arrived `micros` after the first: arrival
/// time, but always at least one tick past the previous frame, so frames
/// that arrive together never share a time.
fn arrival_ticks(micros: u128, last: Option<i64>) -> i64 {
    let ticks = i64::try_from(micros.saturating_mul(9) / 100).unwrap_or(i64::MAX);
    match last {
        Some(l) => ticks.max(l.saturating_add(1)),
        None => ticks,
    }
}

/// Turns Annex-B access units into `MediaItem::Video` frames.
struct Converter {
    bridge: AvccBridge,
    start: Instant,
    last_ticks: Option<i64>,
}

impl Converter {
    /// One access unit as a frame, or None while the bridge has no
    /// parameter sets for it yet.
    fn convert(&mut self, params: &Mutex<Option<VideoParams>>, au: Vec<u8>) -> Option<MediaItem> {
        // Pick up picture size from the SPS when a new one arrives.
        for nal in split_annexb(&au) {
            if nal.first().map(|b| b & 0x1f) == Some(NAL_SPS) {
                if let Some((w, h)) = sps_display_size(nal) {
                    if let (Ok(w), Ok(h)) = (u16::try_from(w), u16::try_from(h)) {
                        self.bridge.set_dimensions(w, h);
                    }
                }
            }
        }
        let bf = self.bridge.push_frame(&au)?;
        // The reply has no usable timestamps: time frames by arrival on this
        // machine's clock, 90 kHz, strictly increasing from the first frame.
        let ticks = arrival_ticks(self.start.elapsed().as_micros(), self.last_ticks);
        self.last_ticks = Some(ticks);
        if bf.new_params {
            *params.lock().unwrap() = self.bridge.video_params();
        }
        Some(MediaItem::Video { ticks, cts: 0, data: bf.data, key: bf.key, new_params: bf.new_params })
    }
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

/// Most bytes held for one NAL unit or one picture (as flv.rs limits a
/// tag): past it the stream is not one this reader understands.
const MAX_BUF: usize = 16 << 20;

/// The camera's reply is a series of records, `[u32 type]["FOSC"][u32
/// length][payload]` (little-endian). Before any video it sends the sign-in
/// result as a record of this type, whose payload starts with a u32 result:
/// 0 when the user name and password were accepted, 1 when they were not.
/// (From a capture of an FI9800P V3's reply, one wrong and one right.)
/// Other values are not known; they are taken as the camera not starting
/// the stream, not as a wrong password.
const SIGN_IN_RECORD: u32 = 100;
/// The sign-in result code for a wrong user name or password.
const SIGN_IN_REFUSED: u32 = 1;
/// The sign-in result is looked for this far into the reply.
const SIGN_IN_WITHIN: usize = 64 << 10;
/// A record header: `[u32 type]["FOSC"][u32 length]`.
const RECORD_HEADER: usize = 12;

/// Watches the start of the reply for the sign-in result, so a wrong
/// password is reported as one ("sign-in refused": not retried in a loop that
/// would lock the camera's account) instead of a stream with no video.
///
/// The reply's records are walked from its first byte by their lengths.
/// Anything that is not a record header where one should be, or a record
/// that would end past `SIGN_IN_WITHIN`, ends the check with no result (the
/// stream is left alone).
#[derive(Default)]
struct SignInCheck {
    /// Offset in the reply of the record header looked at next.
    next: usize,
    /// Bytes of the reply read so far.
    read: usize,
    /// The reply's bytes from `next` on (only those read so far).
    held: Vec<u8>,
    done: bool,
}

impl SignInCheck {
    fn push(&mut self, data: &[u8]) -> Result<(), String> {
        if self.done {
            return Ok(());
        }
        let end = self.read.saturating_add(data.len());
        if self.next < end {
            // `held` is empty unless `next` was already read.
            let skip = self.next.saturating_sub(self.read);
            self.held.extend_from_slice(&data[skip..]);
        }
        self.read = end;
        let r = self.walk();
        if self.done {
            self.held = Vec::new();
        }
        r
    }

    /// Walk the records in `held`; drains what was walked past once.
    fn walk(&mut self) -> Result<(), String> {
        let u32_at = |b: &[u8], i: usize| u32::from_le_bytes(b[i..i + 4].try_into().unwrap());
        let mut at = 0; // `next`'s place in `held`
        let r = loop {
            if self.next >= SIGN_IN_WITHIN {
                self.done = true;
                break Ok(());
            }
            let h = &self.held[at.min(self.held.len())..];
            if h.len() < RECORD_HEADER {
                break Ok(());
            }
            if &h[4..8] != b"FOSC" {
                self.done = true;
                break Ok(());
            }
            let ty = u32_at(h, 0);
            if ty == SIGN_IN_RECORD {
                if self.next.saturating_add(RECORD_HEADER + 4) > SIGN_IN_WITHIN {
                    self.done = true;
                    break Ok(());
                }
                if h.len() < RECORD_HEADER + 4 {
                    break Ok(()); // the result is in a later read
                }
                self.done = true;
                break match u32_at(h, RECORD_HEADER) {
                    0 => Ok(()),
                    SIGN_IN_REFUSED => Err(format!(
                        "Foscam sign-in refused (the camera answered with code {SIGN_IN_REFUSED}): check the user name and password"
                    )),
                    code => Err(format!("Foscam camera would not start the stream (code {code})")),
                };
            }
            let len = u32_at(h, 8) as usize;
            let Some(rec_end) = self.next.checked_add(RECORD_HEADER).and_then(|n| n.checked_add(len)) else {
                self.done = true;
                break Ok(());
            };
            if rec_end > SIGN_IN_WITHIN {
                self.done = true;
                break Ok(());
            }
            at += rec_end - self.next;
            self.next = rec_end;
        };
        self.held.drain(..at.min(self.held.len()));
        r
    }
}
/// Most bytes of parameter sets, SEI and delimiters held before a picture.
const MAX_PREFIX: usize = 1 << 20;
/// NAL units read before a stream with no H.264 SPS or slice is refused.
const PROBE_NALS: usize = 64;

/// The error for a camera that sends H.265.
const H265_ERROR: &str =
    "this Foscam sends H.265 video, which the foscam:// reader does not support yet; try its RTSP address";

/// Splits a mixed byte stream (FOSC headers + Annex-B H.264) into H.264
/// access units. FOSC headers are skipped: only bytes between Annex-B start
/// codes that look like real NAL units are kept, and a NAL unit is cut where
/// a FOSC record begins (4 bytes before its `FOSC` magic; what follows it is
/// skipped up to the next start code). A `FOSC` counts as a record's magic
/// only when the two bytes before it are zero (the top of a small record
/// type, see `is_record_magic`); elsewhere it is ordinary NAL data.
struct H264Demux {
    /// Bytes read. `buf[pos..]` are not yet consumed: while `in_nal`, they
    /// start with the body of the NAL unit being read; otherwise they are
    /// junk before a start code. `buf[..pos]` is dropped once per `push`, so
    /// bytes are not moved for every NAL unit.
    buf: Vec<u8>,
    /// Where the unconsumed bytes in `buf` begin.
    pos: usize,
    /// Where the next search in `buf` starts (at or past `pos`): everything
    /// before it was already searched, so each byte is looked at about once.
    scan: usize,
    /// A start code has been seen and its NAL unit's end is not yet known.
    in_nal: bool,
    /// The access unit being built (Annex-B, 4-byte start codes).
    au: Vec<u8>,
    /// `au` holds a slice of its picture.
    au_has_slice: bool,
    /// NAL units seen before the first H.264 SPS or slice (H.265 check).
    probed: usize,
    /// An H.264 SPS or slice has been seen.
    h264: bool,
}

/// What ends the NAL unit being read.
#[derive(Debug, PartialEq)]
enum Boundary {
    /// An Annex-B start code `00 00 01` at this offset.
    StartCode(usize),
    /// The `FOSC` magic of a record header at this offset.
    Fosc(usize),
}

impl H264Demux {
    fn new() -> Self {
        H264Demux { buf: Vec::new(), pos: 0, scan: 0, in_nal: false, au: Vec::new(), au_has_slice: false, probed: 0, h264: false }
    }

    /// Feed bytes; returns the access units completed by them (Annex-B,
    /// start codes included) ready for [`AvccBridge::push_frame`]. An access
    /// unit is complete once the next one starts. An error ends the stream:
    /// it is not one this reader can follow.
    fn push(&mut self, data: &[u8]) -> Result<Vec<Vec<u8>>, String> {
        self.buf.extend_from_slice(data);
        let mut out = Vec::new();
        let r = self.walk(&mut out);
        // Drop the consumed bytes, once for this read.
        self.buf.drain(..self.pos);
        self.scan -= self.pos;
        self.pos = 0;
        r.map(|_| out)
    }

    fn walk(&mut self, out: &mut Vec<Vec<u8>>) -> Result<(), String> {
        loop {
            if !self.in_nal {
                match find_start_code(&self.buf, self.scan) {
                    Some((_, body)) => {
                        // Bytes before the start code are FOSC headers / junk.
                        self.pos = body;
                        self.scan = body;
                        self.in_nal = true;
                    }
                    None => {
                        // Keep only what could begin a start code.
                        self.pos = self.pos.max(self.buf.len().saturating_sub(2));
                        self.scan = self.pos;
                        return Ok(());
                    }
                }
            }
            let base = self.pos;
            match find_boundary(&self.buf[base..], self.scan - base) {
                Some(Boundary::StartCode(at)) => {
                    self.ingest_nal(base + at, out)?;
                    self.pos = base + at + 3;
                    self.scan = self.pos;
                }
                Some(Boundary::Fosc(at)) => {
                    // The record starts 4 bytes before its magic.
                    self.ingest_nal(base + at - 4, out)?;
                    self.pos = base + at + 4;
                    self.scan = self.pos;
                    self.in_nal = false;
                }
                None => {
                    if self.buf.len() - self.pos > MAX_BUF {
                        return Err(format!("the Foscam stream sent a NAL unit over {} MB; stopping", MAX_BUF >> 20));
                    }
                    // A start code or magic may straddle this read and the next.
                    self.scan = self.buf.len().saturating_sub(3).max(self.pos);
                    return Ok(());
                }
            }
        }
    }

    /// The stream ended. The NAL unit being read has no end and may be cut
    /// short, so it is dropped. The access unit before it holds only
    /// complete NAL units and is returned when it has a picture.
    fn finish(&mut self) -> Vec<Vec<u8>> {
        self.buf.clear();
        self.pos = 0;
        self.scan = 0;
        self.in_nal = false;
        let au = std::mem::take(&mut self.au);
        if std::mem::take(&mut self.au_has_slice) {
            vec![au]
        } else {
            Vec::new()
        }
    }

    /// Take `buf[pos..end]` (less the trailing zeros a 4-byte start code or
    /// padding leaves) as one NAL unit. One whose first byte has the
    /// forbidden_zero_bit set is not H.264 (junk from a record's payload)
    /// and is dropped.
    fn ingest_nal(&mut self, end: usize, out: &mut Vec<Vec<u8>>) -> Result<(), String> {
        let start = self.pos;
        let mut end = end;
        while end > start && self.buf[end - 1] == 0 {
            end -= 1;
        }
        if end == start || self.buf[start] & 0x80 != 0 {
            return Ok(());
        }
        let nal = &self.buf[start..end];
        let ty = nal[0] & 0x1f;
        if !self.h264 {
            if is_h265_parameter_set(nal) {
                return Err(H265_ERROR.to_string());
            }
            if matches!(ty, NAL_SPS | NAL_IDR | NAL_NON_IDR) {
                self.h264 = true;
            } else {
                self.probed += 1;
                if self.probed >= PROBE_NALS {
                    return Err(format!(
                        "no H.264 video in the first {PROBE_NALS} units from this Foscam; it may send H.265, which the foscam:// reader does not support yet; try its RTSP address"
                    ));
                }
            }
        }
        match ty {
            // These come before a picture's first slice: after a slice they
            // begin the next access unit (H.264 7.4.1.2.3).
            NAL_SEI | NAL_SPS | NAL_PPS | NAL_AUD => {
                if self.au_has_slice {
                    out.push(std::mem::take(&mut self.au));
                    self.au_has_slice = false;
                }
            }
            NAL_IDR | NAL_NON_IDR => {
                if nal.len() < 2 {
                    return Ok(());
                }
                // first_mb_in_slice == 0 (its ue(v) code is a single 1 bit)
                // is the first slice of a new picture; other slices join it.
                if nal[1] & 0x80 != 0 && self.au_has_slice {
                    out.push(std::mem::take(&mut self.au));
                }
                self.au_has_slice = true;
            }
            _ => return Ok(()), // unknown type: not picture data
        }
        let limit = if self.au_has_slice { MAX_BUF } else { MAX_PREFIX };
        if self.au.len() + 4 + nal.len() > limit {
            return Err(format!("the Foscam stream sent over {} MB without a complete picture; stopping", limit >> 20));
        }
        self.au.extend_from_slice(&[0, 0, 0, 1]);
        self.au.extend_from_slice(nal);
        Ok(())
    }
}

/// An H.265 VPS, SPS or PPS NAL header (`40 01`, `42 01`, `44 01`).
fn is_h265_parameter_set(nal: &[u8]) -> bool {
    matches!(nal, [0x40 | 0x42 | 0x44, 0x01, ..])
}

/// Find the next Annex-B start code at or after `from`, as (start, NAL-body
/// offset just past the code). Matches `00 00 01`; a 4-byte `00 00 00 01` is
/// found at its second zero, its leading zero left as the previous NAL's
/// trailing zero (see the trim in `ingest_nal`).
fn find_start_code(data: &[u8], from: usize) -> Option<(usize, usize)> {
    let i = data.get(from..)?.windows(3).position(|w| w == [0, 0, 1])? + from;
    Some((i, i + 3))
}

/// Whether the `FOSC` at `at` can be a record header's magic: its record's
/// u32 type (little-endian, a small number) fits before it in `data`, so its
/// top two bytes are zero. Otherwise it is ordinary data in the NAL unit.
fn is_record_magic(data: &[u8], at: usize) -> bool {
    at >= 4 && data[at - 2] == 0 && data[at - 1] == 0
}

/// The first start code or record-header `FOSC` magic at or after `from`.
fn find_boundary(data: &[u8], from: usize) -> Option<Boundary> {
    let mut i = from;
    while i + 3 <= data.len() {
        match data[i] {
            0 if data[i + 1] == 0 && data[i + 2] == 1 => return Some(Boundary::StartCode(i)),
            b'F' if data.get(i + 1..i + 4) == Some(b"OSC") && is_record_magic(data, i) => return Some(Boundary::Fosc(i)),
            _ => {}
        }
        i += 1;
    }
    None
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
        assert_eq!(parse_url("foscam://192.0.2.5:88/"), Some(("192.0.2.5".into(), 88)));
        assert_eq!(parse_url("foscam://[fd00::5]:88"), Some(("fd00::5".into(), 88)));
        assert_eq!(parse_url("foscam://[fd00::5]"), Some(("fd00::5".into(), 88)));
        assert_eq!(parse_url("foscam://"), None);
        assert_eq!(parse_url("rtsp://x"), None);
        // A path, query or user info after the host.
        assert_eq!(parse_url("foscam://cam.example:88/x"), None);
        assert_eq!(parse_url("foscam://cam.example/videoMain"), None);
        assert_eq!(parse_url("foscam://cam.example?x=1"), None);
        assert_eq!(parse_url("foscam://u:p@cam.example"), None);
        // IPv6 without brackets, or with junk after them.
        assert_eq!(parse_url("foscam://fd00::5"), None);
        assert_eq!(parse_url("foscam://fd00::5:88"), None);
        assert_eq!(parse_url("foscam://[fd00::5]88"), None);
        assert_eq!(parse_url("foscam://[fd00::5]:"), None);
        // Ports that are not ports.
        assert_eq!(parse_url("foscam://cam.example:"), None);
        assert_eq!(parse_url("foscam://cam.example:0"), None);
        assert_eq!(parse_url("foscam://cam.example:70000"), None);
        assert_eq!(parse_url("foscam://cam.example:8x"), None);
        assert_eq!(parse_url("foscam://:88"), None);
    }

    #[test]
    fn the_scheme_matches_in_any_case() {
        assert!(is_foscam_url("Foscam://cam.example"));
        assert!(is_foscam_url("FOSCAM://cam.example"));
        assert!(!is_foscam_url("foscam:/cam.example"));
        assert!(!is_foscam_url("fos"));
        assert_eq!(parse_url("Foscam://cam.example:8088"), Some(("cam.example".into(), 8088)));
        assert_eq!(parse_url("FOSCAM://[fd00::5]"), Some(("fd00::5".into(), 88)));
    }

    #[test]
    fn a_host_with_a_space_or_control_character_is_refused() {
        for url in [
            "foscam://cam example",
            "foscam://cam.example\r\nX-Evil: 1",
            "foscam://cam.example\n:88",
            "foscam://cam\t.example",
            "foscam://cam\x7f.example",
            "foscam://[fd00::5 ]:88",
            "foscam://cam\0.example",
        ] {
            assert_eq!(parse_url(url), None, "{url:?}");
        }
    }

    #[test]
    fn request_body_layout() {
        let body = video_request_body("user", "secret").unwrap();
        assert_eq!(body.len(), 4 + 4 + 4 + 161);
        assert_eq!(&body[0..4], &0u32.to_le_bytes());
        assert_eq!(&body[4..8], b"FOSC");
        let block_len = u32::from_le_bytes(body[8..12].try_into().unwrap()) as usize;
        assert_eq!(block_len, 161);
        let block = &body[12..];
        assert_eq!(block[0], 0x00, "flag byte");
        // username field: "user" then nulls to 64
        assert_eq!(&block[1..5], b"user");
        assert!(block[5..65].iter().all(|&b| b == 0));
        // password field starts at offset 65
        assert_eq!(&block[65..71], b"secret");
        assert!(block[71..129].iter().all(|&b| b == 0));
    }

    #[test]
    fn request_brackets_an_ipv6_host() {
        let req = request("fd00::5", 88, "user", "secret").unwrap();
        assert!(req.starts_with(b"SERVERPUSH / HTTP/1.1\r\nHost: [fd00::5]:88\r\n"));
        let req = request("cam.example", 88, "user", "secret").unwrap();
        assert!(req.starts_with(b"SERVERPUSH / HTTP/1.1\r\nHost: cam.example:88\r\n"));
    }

    #[test]
    fn credentials_the_block_cannot_carry_are_refused() {
        let max = "a".repeat(63);
        let body = video_request_body(&max, &max).unwrap();
        // 63 bytes, then the NUL that ends each field.
        assert_eq!(&body[13..76], max.as_bytes());
        assert_eq!(body[76], 0);
        assert_eq!(&body[77..140], max.as_bytes());
        assert_eq!(body[140], 0);

        let long_name = "n".repeat(64);
        let long_pass = "p".repeat(64);
        let e = video_request_body(&long_name, "secret").unwrap_err().to_string();
        assert_eq!(e, "the Foscam user name is longer than 63 bytes");
        let e = video_request_body("user", &long_pass).unwrap_err().to_string();
        assert_eq!(e, "the Foscam password is longer than 63 bytes");
        assert!(!e.contains(&long_pass));
        // A multi-byte character counts in bytes: 32 x 2 = 64.
        assert!(video_request_body("user", &"\u{e9}".repeat(32)).is_err());
        assert!(video_request_body("user", &"\u{e9}".repeat(31)).is_ok());

        let e = video_request_body("us\0er", "secret").unwrap_err().to_string();
        assert_eq!(e, "the Foscam user name contains a NUL character");
        let e = video_request_body("user", "sec\0ret").unwrap_err().to_string();
        assert_eq!(e, "the Foscam password contains a NUL character");
        assert!(!e.contains("sec"));
        // The whole request is refused the same way, before any connect.
        assert!(request("cam.example", 88, "user", &long_pass).is_err());
        assert!(request("cam.example", 88, "", "").is_ok());
    }

    #[test]
    fn ticks_follow_arrival_and_always_step() {
        assert_eq!(arrival_ticks(0, None), 0);
        assert_eq!(arrival_ticks(1_000_000, Some(0)), 90_000);
        // Two frames read at once: the second is still one tick later.
        assert_eq!(arrival_ticks(1_000_000, Some(90_000)), 90_001);
        assert_eq!(arrival_ticks(1_000_000, Some(90_001)), 90_002);
        // Arrival time again once the clock is past the last frame.
        assert_eq!(arrival_ticks(2_000_000, Some(90_002)), 180_000);
        assert_eq!(arrival_ticks(u128::MAX, Some(i64::MAX)), i64::MAX);
    }

    fn annexb(nals: &[&[u8]]) -> Vec<u8> {
        let mut v = Vec::new();
        for n in nals {
            v.extend_from_slice(&[0, 0, 0, 1]);
            v.extend_from_slice(n);
        }
        v
    }

    fn run_all(d: &mut H264Demux, stream: &[u8], chunk: usize) -> Vec<Vec<u8>> {
        let mut aus = Vec::new();
        for c in stream.chunks(chunk) {
            aus.extend(d.push(c).unwrap());
        }
        aus
    }

    /// A reply record as the camera frames it.
    fn record(ty: u32, payload: &[u8]) -> Vec<u8> {
        let mut r = ty.to_le_bytes().to_vec();
        r.extend_from_slice(b"FOSC");
        r.extend_from_slice(&(payload.len() as u32).to_le_bytes());
        r.extend_from_slice(payload);
        r
    }

    /// The sign-in result record (made-up contents after the result).
    fn sign_in_reply(result: u32) -> Vec<u8> {
        let mut p = result.to_le_bytes().to_vec();
        p.resize(2219, 0);
        p[8..15].copy_from_slice(b"Preset1");
        record(SIGN_IN_RECORD, &p)
    }

    #[test]
    fn a_refused_sign_in_is_reported_as_one() {
        // A device-info record first, as the camera sends it, then the result;
        // fed in pieces of every size so the record can be split anywhere.
        let mut reply = record(200, b"\x01\x02\x00\x00TEST-MODEL\x00\x00");
        reply.extend(sign_in_reply(1));
        reply.extend(record(157, &[0, 100]));
        for chunk in 1..=reply.len() {
            let mut c = SignInCheck::default();
            let got: Result<Vec<()>, String> = reply.chunks(chunk).map(|p| c.push(p)).collect();
            let e = got.unwrap_err();
            assert!(crate::engine::native::is_refused_password(&e) && crate::engine::native::is_auth_error(&e), "chunk {chunk}: {e}");
            assert!(c.done && c.held.is_empty(), "chunk {chunk}");
        }
        // The result record first, as the camera may send it.
        let mut c = SignInCheck::default();
        assert!(c.push(&sign_in_reply(1)).is_err());
    }

    #[test]
    fn another_sign_in_code_is_a_plain_error() {
        let mut reply = record(200, &[7; 342]);
        reply.extend(sign_in_reply(2));
        for chunk in [1, 5, 13, 4096] {
            let mut c = SignInCheck::default();
            let got: Result<Vec<()>, String> = reply.chunks(chunk).map(|p| c.push(p)).collect();
            let e = got.unwrap_err();
            assert_eq!(e, "Foscam camera would not start the stream (code 2)");
            assert!(!crate::engine::native::is_refused_password(&e) && !crate::engine::native::is_auth_error(&e), "{e}");
        }
    }

    #[test]
    fn the_sign_in_result_may_straddle_reads() {
        let mut reply = record(200, &[7; 342]);
        let at = reply.len(); // where the type-100 record begins
        reply.extend(sign_in_reply(1));
        for cut in at..at + RECORD_HEADER + 4 {
            for cut2 in [cut, cut + 1, cut + 3] {
                let mut c = SignInCheck::default();
                assert!(c.push(&reply[..cut]).is_ok(), "cut {cut}");
                let cut2 = cut2.min(at + RECORD_HEADER + 4);
                let r1 = c.push(&reply[cut..cut2]);
                let r2 = c.push(&reply[cut2..]);
                assert!(r1.is_err() ^ r2.is_err(), "cut {cut}/{cut2}: told once");
                // Only the bytes from the record being read on are held.
                assert!(c.held.is_empty());
            }
        }
    }

    #[test]
    fn records_are_walked_by_length_without_rescanning() {
        // A "FOSC" with type 100 inside another record's payload is not a
        // record: the walk steps over the payload by its length.
        let mut payload = vec![0u8; 64];
        payload[8..20].copy_from_slice(&record(SIGN_IN_RECORD, &[])[..12]);
        payload[20..24].copy_from_slice(&1u32.to_le_bytes());
        let mut reply = record(200, &payload);
        reply.extend(sign_in_reply(0));
        let mut c = SignInCheck::default();
        for p in reply.chunks(7) {
            c.push(p).unwrap();
            assert!(c.held.len() < 7 + RECORD_HEADER + 4, "holds only from the next record on");
        }
        assert!(c.done);
    }

    #[test]
    fn a_huge_record_length_ends_the_check() {
        let mut reply = 200u32.to_le_bytes().to_vec();
        reply.extend_from_slice(b"FOSC");
        reply.extend_from_slice(&u32::MAX.to_le_bytes());
        let mut c = SignInCheck::default();
        c.push(&reply).unwrap();
        assert!(c.done && c.held.is_empty());
        c.push(&sign_in_reply(1)).unwrap();

        // A record ending past the limit, or at it (no room for another).
        for len in [SIGN_IN_WITHIN - RECORD_HEADER + 1, SIGN_IN_WITHIN - RECORD_HEADER] {
            let mut h = 200u32.to_le_bytes().to_vec();
            h.extend_from_slice(b"FOSC");
            h.extend_from_slice(&(len as u32).to_le_bytes());
            let mut c = SignInCheck::default();
            c.push(&h).unwrap();
            assert!(c.done && c.held.is_empty(), "len {len}");
        }
        // One ending just before it: the next header is still read.
        let mut reply = record(200, &vec![0u8; SIGN_IN_WITHIN - 2 * RECORD_HEADER - 4]);
        reply.extend(sign_in_reply(1));
        let mut c = SignInCheck::default();
        assert!(reply.chunks(1000).map(|p| c.push(p)).any(|r| r.is_err()));
        // And one byte later, the result would be past the limit.
        let mut reply = record(200, &vec![0u8; SIGN_IN_WITHIN - 2 * RECORD_HEADER - 3]);
        reply.extend(sign_in_reply(1));
        let mut c = SignInCheck::default();
        assert!(reply.chunks(1000).all(|p| c.push(p).is_ok()));
        assert!(c.done);
    }

    #[test]
    fn a_reply_that_does_not_start_with_a_record_is_left_alone() {
        let mut c = SignInCheck::default();
        let mut reply = annexb(&[&SPS, &PPS, &IDR]);
        reply.extend(sign_in_reply(1));
        for p in reply.chunks(5) {
            c.push(p).unwrap();
        }
        assert!(c.done && c.held.is_empty());
        // Too short to tell: waits.
        let mut c = SignInCheck::default();
        c.push(b"\x64\x00\x00\x00FOS").unwrap();
        assert!(!c.done);
        c.push(b"X\x00\x00\x00\x00").unwrap();
        assert!(c.done, "not a record header");
    }

    #[test]
    fn an_accepted_sign_in_and_the_video_after_it_pass() {
        let mut reply = sign_in_reply(0);
        reply.extend(record(157, &[0, 100]));
        reply.extend(annexb(&[&SPS, &PPS, &IDR]));
        let mut c = SignInCheck::default();
        for p in reply.chunks(5) {
            c.push(p).unwrap();
        }
        assert!(c.done);
        // Nothing is looked at after the result.
        c.push(&sign_in_reply(1)).unwrap();
    }

    #[test]
    fn a_reply_without_a_sign_in_record_is_left_alone() {
        // Small records only, past the limit: the check gives up.
        let small = record(157, &[0, 100]);
        let mut c = SignInCheck::default();
        for _ in 0..(SIGN_IN_WITHIN / small.len() + 2) {
            c.push(&small).unwrap();
        }
        assert!(c.done && c.held.is_empty(), "stops looking after {SIGN_IN_WITHIN} bytes");
        // A FOSC magic with too little before it is not read out of bounds.
        let mut c = SignInCheck::default();
        c.push(b"FOSCxx").unwrap();
        c.push(b"\x64\x00\x00\x00FOSC").unwrap();
        assert!(c.done);
    }

    const SPS: [u8; 4] = [0x67, 0x64, 0x00, 0x1f];
    const PPS: [u8; 3] = [0x68, 0xeb, 0xe3];
    const IDR: [u8; 3] = [0x65, 0x88, 0x84];
    const P: [u8; 3] = [0x41, 0x9a, 0x02];
    const P2: [u8; 3] = [0x41, 0x9a, 0x04];

    #[test]
    fn demux_skips_junk_and_groups_keyframes() {
        // A fake FOSC header followed by SPS+PPS+IDR, then P-frames.
        let junk = b"junkjunkjunkjunk\x00\x00\x00\x1aFOSC";
        let mut stream = Vec::new();
        stream.extend_from_slice(junk);
        stream.extend_from_slice(&annexb(&[&SPS, &PPS, &IDR]));
        stream.extend_from_slice(&annexb(&[&P, &P2]));

        let mut d = H264Demux::new();
        let mut aus = d.push(&stream).unwrap();
        aus.extend(d.finish());
        // The trailing P2 has no following start code: dropped at the end.
        assert_eq!(aus, vec![annexb(&[&SPS, &PPS, &IDR]), annexb(&[&P])]);
    }

    #[test]
    fn demux_emits_a_frame_once_the_next_one_starts() {
        let full = annexb(&[&IDR, &P, &P2]);
        for chunk in [1, 2, 3, 4, 5, 64] {
            let mut d = H264Demux::new();
            let aus = run_all(&mut d, &full, chunk);
            // IDR is complete once P starts; P is only known complete at
            // the end, and P2 (no end) is dropped.
            assert_eq!(aus, vec![annexb(&[&IDR])], "chunk {chunk}");
            assert_eq!(d.finish(), vec![annexb(&[&P])], "chunk {chunk}");
        }
    }

    #[test]
    fn demux_drops_the_trailing_nal_on_finish() {
        let full = annexb(&[&P]);
        let mut d = H264Demux::new();
        assert!(d.push(&full).unwrap().is_empty(), "no next start code yet");
        assert!(d.finish().is_empty(), "the P-frame may be cut short");
    }

    #[test]
    fn demux_groups_the_slices_of_one_picture() {
        // Two slices of an IDR picture (the second's first_mb_in_slice is
        // not 0: its first bit is 0), then a one-slice P picture, then AUDs.
        let idr_a = [0x65u8, 0x88, 0x84];
        let idr_b = [0x65u8, 0x40, 0x84];
        let aud = [0x09u8, 0xf0];
        let stream = annexb(&[&aud, &SPS, &PPS, &idr_a, &idr_b, &aud, &P, &aud, &aud]);
        let mut d = H264Demux::new();
        let aus = run_all(&mut d, &stream, 7);
        assert_eq!(aus, vec![annexb(&[&aud, &SPS, &PPS, &idr_a, &idr_b]), annexb(&[&aud, &P])]);

        // Without delimiters, a slice with first_mb_in_slice 0 starts the next.
        let p_a = [0x41u8, 0x9a, 0x02];
        let p_b = [0x41u8, 0x20, 0x02];
        let stream = annexb(&[&SPS, &PPS, &idr_a, &idr_b, &p_a, &p_b, &P2, &P2]);
        let mut d = H264Demux::new();
        let aus = run_all(&mut d, &stream, 1000);
        assert_eq!(aus, vec![annexb(&[&SPS, &PPS, &idr_a, &idr_b]), annexb(&[&p_a, &p_b])]);
        // An SPS after a picture begins the next access unit too.
        let stream = annexb(&[&SPS, &PPS, &IDR, &SPS, &PPS, &IDR, &P, &P]);
        let mut d = H264Demux::new();
        let aus = run_all(&mut d, &stream, 1000);
        assert_eq!(aus, vec![annexb(&[&SPS, &PPS, &IDR]), annexb(&[&SPS, &PPS, &IDR])]);
    }

    #[test]
    fn demux_cuts_a_nal_where_a_fosc_record_begins() {
        // IDR, then a FOSC record ([u32][FOSC][u32 len][block]) stuck to its
        // end with no start code, then P-frames.
        let mut stream = annexb(&[&IDR]);
        stream.extend_from_slice(&0x10u32.to_le_bytes());
        stream.extend_from_slice(b"FOSC");
        stream.extend_from_slice(&8u32.to_le_bytes());
        stream.extend_from_slice(&[0x12, 0x34, 0x56, 0x78, 0x9a, 0xbc, 0x00, 0x00]);
        stream.extend_from_slice(&annexb(&[&P, &P2]));
        for chunk in [1, 3, 4, 5, 1000] {
            let mut d = H264Demux::new();
            let mut aus = run_all(&mut d, &stream, chunk);
            aus.extend(d.finish());
            // The IDR ends where the record's prefix begins.
            assert_eq!(aus, vec![annexb(&[&IDR]), annexb(&[&P])], "chunk {chunk}");
        }
        // A record whose prefix ends in a start code's bytes.
        let mut stream = annexb(&[&IDR]);
        stream.extend_from_slice(&[0x00, 0x00, 0x01, 0x02]);
        stream.extend_from_slice(b"FOSC\x08\x00\x00\x00abcdefgh");
        stream.extend_from_slice(&annexb(&[&P, &P2]));
        let mut d = H264Demux::new();
        let mut aus = run_all(&mut d, &stream, 1000);
        aus.extend(d.finish());
        assert_eq!(aus, vec![annexb(&[&IDR]), annexb(&[&P])]);
    }

    #[test]
    fn fosc_inside_a_slice_is_data() {
        // "FOSC" in a slice's bytes, after nonzero bytes (or too near its
        // start to have a record type before it): not a record, so the
        // slice is kept whole.
        let idr = [0x65u8, 0x88, 0x84, 0x12, 0x34, b'F', b'O', b'S', b'C', 0x56, 0x78];
        let p = [0x41u8, b'F', b'O', b'S', b'C', 0x02];
        let p2 = [0x41u8, 0x9a, 0x00, 0x01, b'F', b'O', b'S', b'C', 0x02];
        let stream = annexb(&[&SPS, &PPS, &idr, &p, &p2, &P]);
        for chunk in [1, 2, 3, 5, 1000] {
            let mut d = H264Demux::new();
            let mut aus = run_all(&mut d, &stream, chunk);
            aus.extend(d.finish());
            // p (first_mb_in_slice not 0) joins the IDR picture.
            assert_eq!(aus, vec![annexb(&[&SPS, &PPS, &idr, &p]), annexb(&[&p2])], "chunk {chunk}");
        }
    }

    #[test]
    fn a_nal_with_the_forbidden_bit_is_dropped() {
        let junk = [0x85u8, 0x88, 0x84]; // an "IDR" with bit 7 set
        let stream = annexb(&[&SPS, &PPS, &IDR, &junk, &P, &junk, &P2, &P]);
        let mut d = H264Demux::new();
        let mut aus = run_all(&mut d, &stream, 4);
        aus.extend(d.finish());
        assert_eq!(aus, vec![annexb(&[&SPS, &PPS, &IDR]), annexb(&[&P]), annexb(&[&P2])]);
        // Before any H.264, it does not count toward the H.265 probe either.
        let many: Vec<&[u8]> = vec![&junk[..]; PROBE_NALS + 2];
        let mut d = H264Demux::new();
        assert!(d.push(&annexb(&many)).unwrap().is_empty());
    }

    #[test]
    fn consumed_bytes_are_dropped_once_per_read() {
        // Many NAL units in one read: all of them are consumed, and what is
        // left is only the unfinished last one.
        let mut nals: Vec<&[u8]> = vec![&SPS, &PPS, &IDR];
        nals.extend(std::iter::repeat_n(&P[..], 500));
        let stream = annexb(&nals);
        let mut d = H264Demux::new();
        let aus = d.push(&stream).unwrap();
        assert_eq!(aus.len(), 499);
        assert_eq!(d.pos, 0);
        assert_eq!(d.buf, P.to_vec());
        assert!(d.scan <= d.buf.len());
    }

    #[test]
    fn demux_refuses_h265() {
        let vps = [0x40u8, 0x01, 0x0c, 0x01];
        let sps = [0x42u8, 0x01, 0x01, 0x01];
        let pps = [0x44u8, 0x01, 0xc1, 0x72];
        let idr = [0x26u8, 0x01, 0xaf, 0x00];
        let mut d = H264Demux::new();
        let e = d.push(&annexb(&[&vps, &sps, &pps, &idr])).unwrap_err();
        assert_eq!(e, H265_ERROR);

        // No parameter sets seen, but no H.264 either: refused after a while.
        let trail = [0x02u8, 0x01, 0xd0, 0x10];
        let many: Vec<&[u8]> = vec![&trail[..]; PROBE_NALS + 2];
        let mut d = H264Demux::new();
        let e = d.push(&annexb(&many)).unwrap_err();
        assert!(e.contains("H.265") && e.contains("RTSP"), "{e}");

        // H.264 is not mistaken for it, and once H.264 is seen the check stops.
        let mut d = H264Demux::new();
        let mut nals: Vec<&[u8]> = vec![&SPS, &PPS, &IDR];
        nals.extend(std::iter::repeat_n(&trail[..], PROBE_NALS + 2));
        nals.push(&P);
        nals.push(&P);
        assert_eq!(d.push(&annexb(&nals)).unwrap(), vec![annexb(&[&SPS, &PPS, &IDR])]);
    }

    #[test]
    fn demux_memory_is_bounded() {
        // One NAL unit that never ends.
        let mut d = H264Demux::new();
        d.push(&annexb(&[&P])).unwrap();
        let chunk = vec![0xaau8; 1 << 20];
        let mut failed = None;
        for i in 0..20 {
            if let Err(e) = d.push(&chunk) {
                failed = Some((i, e));
                break;
            }
        }
        let (i, e) = failed.expect("stopped");
        assert!(i >= 15, "not before 16 MB");
        assert!(e.contains("16 MB"), "{e}");

        // Parameter sets and SEI with no picture.
        let sei = [0x06u8, 0x05, 0xff, 0xaa, 0xaa, 0xaa, 0x80];
        let mut d = H264Demux::new();
        d.push(&annexb(&[&SPS])).unwrap();
        let block = annexb(&vec![&sei[..]; 1000]);
        let e = (0..400).find_map(|_| d.push(&block).err()).expect("stopped");
        assert!(e.contains("1 MB"), "{e}");
        assert!(d.au.len() <= MAX_PREFIX);

        // Junk with no start code is not kept.
        let mut d = H264Demux::new();
        for _ in 0..32 {
            d.push(&chunk).unwrap();
        }
        assert!(d.buf.len() <= 2);
    }

    #[test]
    fn boundaries_are_found_in_order() {
        assert_eq!(find_boundary(b"ab\x10\x00\x00\x00FOSC\x00\x00\x01", 0), Some(Boundary::Fosc(6)));
        assert_eq!(find_boundary(b"ab\x00\x00\x01\x10\x00\x00\x00FOSC", 0), Some(Boundary::StartCode(2)));
        // Not a record header: too near the start, or a big type before it.
        assert_eq!(find_boundary(b"abFOSC\x00\x00\x01", 0), Some(Boundary::StartCode(6)));
        assert_eq!(find_boundary(b"ab\x10\x00\x01\x00FOSC\x00\x00\x01", 0), Some(Boundary::StartCode(10)));
        assert_eq!(find_boundary(b"ab\x10\x00\x00\x7fFOSC\x00\x00\x01", 0), Some(Boundary::StartCode(10)));
        assert_eq!(find_boundary(b"abFOS", 0), None);
        assert_eq!(find_boundary(b"\x00\x00\x01", 1), None);
        assert_eq!(find_start_code(b"\x00\x00\x01", 4), None);
    }
}
