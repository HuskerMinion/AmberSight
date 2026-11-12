//! HTTP-FLV live streams, as served by Reolink hubs and cameras at
//! `/flv?port=1935&app=bcs&stream=channel0_sub.bcs`.
//!
//! Some devices stream more reliably this way than over RTSP: a Reolink Home
//! Hub relaying a battery camera produced video on only part of its RTSP
//! sessions but on every FLV request. The reader turns the FLV tags into the
//! same frames the RTSP reader produces (length-prefixed NAL units, raw AAC),
//! plus sender-report-like timing marks so audio lines up with video.
//!
//! Credentials go in the query (`user`, `password`), as these devices expect;
//! they are added from the camera's settings and never logged.

use anyhow::{anyhow, bail, Context, Result};
use std::collections::VecDeque;
use std::sync::Arc;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

/// True for an `http(s)://host/flv?...` stream URL.
pub fn is_flv_url(url: &str) -> bool {
    match url::Url::parse(url) {
        Ok(u) => matches!(u.scheme(), "http" | "https") && u.path().trim_end_matches('/').ends_with("/flv"),
        Err(_) => false,
    }
}

// ---------------------------------------------------------------------------
// Tag parsing
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq)]
pub enum Tag {
    /// `onMetaData` picture size, when given, and whether audio is announced.
    Meta { width: Option<u32>, height: Option<u32>, audio: bool },
    /// AVC/HEVC decoder configuration record (`avcC` / `hvcC` contents).
    VideoConfig { h265: bool, record: Vec<u8> },
    /// One video frame: length-prefixed NAL units.
    Video { ms: i64, cts: i32, key: bool, data: Vec<u8> },
    /// AAC AudioSpecificConfig.
    AudioConfig(Vec<u8>),
    /// One raw AAC frame.
    Audio { ms: i64, data: Vec<u8> },
    /// Anything else (other codecs, sequence end, unknown tag types).
    Other,
}

/// Splits an FLV byte stream into tags. Feed bytes with [`TagReader::push`]
/// and take tags with [`TagReader::next_tag`].
#[derive(Default)]
pub struct TagReader {
    buf: Vec<u8>,
    pos: usize,
    header_done: bool,
}

impl TagReader {
    pub fn push(&mut self, data: &[u8]) {
        if self.pos > 0 && self.pos * 2 >= self.buf.len() {
            self.buf.drain(..self.pos);
            self.pos = 0;
        }
        self.buf.extend_from_slice(data);
    }

    fn avail(&self) -> &[u8] {
        &self.buf[self.pos..]
    }

    pub fn next_tag(&mut self) -> Result<Option<Tag>> {
        if !self.header_done {
            let b = self.avail();
            if b.len() < 9 {
                return Ok(None);
            }
            if &b[..3] != b"FLV" {
                bail!("not an FLV stream");
            }
            let header_len = u32::from_be_bytes([b[5], b[6], b[7], b[8]]) as usize;
            // The header is 9 bytes; a huge value would make us buffer forever.
            if !(9..=1024).contains(&header_len) {
                bail!("bad FLV header size {header_len}");
            }
            // Header, then PreviousTagSize0.
            if b.len() < header_len + 4 {
                return Ok(None);
            }
            self.pos += header_len + 4;
            self.header_done = true;
        }
        let b = self.avail();
        if b.len() < 11 {
            return Ok(None);
        }
        let typ = b[0] & 0x1f;
        let size = u32::from_be_bytes([0, b[1], b[2], b[3]]) as usize;
        let ms = (u32::from_be_bytes([b[7], b[4], b[5], b[6]])) as i64;
        if size > 16 << 20 {
            bail!("FLV tag of {size} bytes; stream is corrupt");
        }
        // Tag header, body, PreviousTagSize.
        if b.len() < 11 + size + 4 {
            return Ok(None);
        }
        let body = b[11..11 + size].to_vec();
        self.pos += 11 + size + 4;
        Ok(Some(match typ {
            8 => parse_audio(ms, &body),
            9 => parse_video(ms, &body)?,
            18 => parse_meta(&body),
            _ => Tag::Other,
        }))
    }
}

fn parse_video(ms: i64, b: &[u8]) -> Result<Tag> {
    if b.len() < 2 {
        return Ok(Tag::Other);
    }
    if b[0] & 0x80 != 0 {
        // Enhanced RTMP/FLV: [ex-header|frame type|packet type][FourCC]...
        if b.len() < 5 {
            return Ok(Tag::Other);
        }
        let key = (b[0] >> 4) & 0x07 == 1;
        let packet_type = b[0] & 0x0f;
        let h265 = match &b[1..5] {
            b"hvc1" => true,
            b"avc1" => false,
            _ => return Ok(Tag::Other),
        };
        return Ok(match packet_type {
            0 => Tag::VideoConfig { h265, record: b[5..].to_vec() },
            1 if b.len() >= 8 => Tag::Video { ms, cts: si24(&b[5..8]), key, data: b[8..].to_vec() },
            3 => Tag::Video { ms, cts: 0, key, data: b[5..].to_vec() },
            _ => Tag::Other,
        });
    }
    let key = b[0] >> 4 == 1;
    let h265 = match b[0] & 0x0f {
        7 => false,
        12 => true, // HEVC, as widely used before enhanced FLV
        _ => return Ok(Tag::Other),
    };
    if b.len() < 5 {
        return Ok(Tag::Other);
    }
    Ok(match b[1] {
        0 => Tag::VideoConfig { h265, record: b[5..].to_vec() },
        1 => Tag::Video { ms, cts: si24(&b[2..5]), key, data: b[5..].to_vec() },
        _ => Tag::Other,
    })
}

fn si24(b: &[u8]) -> i32 {
    let v = u32::from_be_bytes([0, b[0], b[1], b[2]]) as i32;
    if v & 0x80_0000 != 0 {
        v - 0x100_0000
    } else {
        v
    }
}

fn parse_audio(ms: i64, b: &[u8]) -> Tag {
    // Sound format 10 = AAC.
    if b.len() < 2 || b[0] >> 4 != 10 {
        return Tag::Other;
    }
    match b[1] {
        0 => Tag::AudioConfig(b[2..].to_vec()),
        1 => Tag::Audio { ms, data: b[2..].to_vec() },
        _ => Tag::Other,
    }
}

/// `onMetaData` (AMF0): only width and height are used.
fn parse_meta(b: &[u8]) -> Tag {
    let mut r = Amf { b, pos: 0 };
    match r.value() {
        Some(AmfValue::Str(s)) if s == "onMetaData" => {}
        _ => return Tag::Other,
    }
    let Some(AmfValue::Props(props)) = r.value() else { return Tag::Other };
    let num = |k: &str| {
        props.iter().find(|(n, _)| n == k).and_then(|(_, v)| match v {
            AmfValue::Num(x) if *x > 0.0 && *x < 65536.0 => Some(*x as u32),
            _ => None,
        })
    };
    let audio = props.iter().any(|(n, v)| n == "audiocodecid" && matches!(v, AmfValue::Num(x) if *x > 0.0));
    Tag::Meta { width: num("width"), height: num("height"), audio }
}

enum AmfValue {
    Num(f64),
    Str(String),
    Props(Vec<(String, AmfValue)>),
    Other,
}

struct Amf<'a> {
    b: &'a [u8],
    pos: usize,
}

impl Amf<'_> {
    fn take(&mut self, n: usize) -> Option<&[u8]> {
        let s = self.b.get(self.pos..self.pos + n)?;
        self.pos += n;
        Some(s)
    }
    fn u16(&mut self) -> Option<usize> {
        self.take(2).map(|s| u16::from_be_bytes([s[0], s[1]]) as usize)
    }
    fn short_str(&mut self) -> Option<String> {
        let n = self.u16()?;
        self.take(n).map(|s| String::from_utf8_lossy(s).into_owned())
    }
    fn props(&mut self) -> Option<Vec<(String, AmfValue)>> {
        let mut out = Vec::new();
        loop {
            let name = self.short_str()?;
            if name.is_empty() && self.b.get(self.pos) == Some(&9) {
                self.pos += 1;
                return Some(out);
            }
            let v = self.value()?;
            out.push((name, v));
        }
    }
    fn value(&mut self) -> Option<AmfValue> {
        let t = *self.take(1)?.first()?;
        Some(match t {
            0 => {
                let s = self.take(8)?;
                AmfValue::Num(f64::from_be_bytes(s.try_into().ok()?))
            }
            1 => {
                self.take(1)?;
                AmfValue::Other
            }
            2 => AmfValue::Str(self.short_str()?),
            3 => AmfValue::Props(self.props()?),
            5 | 6 => AmfValue::Other,
            8 => {
                self.take(4)?; // approximate count; the list ends with 0x000009
                AmfValue::Props(self.props()?)
            }
            _ => return None,
        })
    }
}

// ---------------------------------------------------------------------------
// Codec parameters
// ---------------------------------------------------------------------------

/// RFC 6381 codec string from an `avcC` / `hvcC` record.
pub fn codec_string(h265: bool, record: &[u8]) -> Option<String> {
    if !h265 {
        let r = record.get(1..4)?;
        return Some(format!("avc1.{:02X}{:02X}{:02X}", r[0], r[1], r[2]));
    }
    // HEVCDecoderConfigurationRecord: version, then profile_space(2)
    // tier(1) profile_idc(5), 32 compatibility flags, 48 constraint
    // flags, level.
    let r = record.get(..13)?;
    let space = ["", "A", "B", "C"][(r[1] >> 6) as usize];
    let tier = if r[1] & 0x20 != 0 { "H" } else { "L" };
    let idc = r[1] & 0x1f;
    let compat = u32::from_be_bytes([r[2], r[3], r[4], r[5]]).reverse_bits();
    let mut constraints = &r[6..12];
    while let [head @ .., 0] = constraints {
        if head.is_empty() {
            break;
        }
        constraints = head;
    }
    let mut s = format!("hvc1.{space}{idc}.{compat:X}.{tier}{}", r[12]);
    for b in constraints {
        s.push_str(&format!(".{b:02X}"));
    }
    Some(s)
}

fn mp4_box(out: &mut Vec<u8>, kind: &[u8; 4], body: impl FnOnce(&mut Vec<u8>)) {
    let start = out.len();
    out.extend_from_slice(&[0, 0, 0, 0]);
    out.extend_from_slice(kind);
    body(out);
    let len = (out.len() - start) as u32;
    out[start..start + 4].copy_from_slice(&len.to_be_bytes());
}

/// `avc1`/`hvc1` VisualSampleEntry carrying the decoder configuration.
pub fn video_sample_entry(h265: bool, record: &[u8], width: u16, height: u16) -> Vec<u8> {
    let mut out = Vec::new();
    mp4_box(&mut out, if h265 { b"hvc1" } else { b"avc1" }, |b| {
        b.extend_from_slice(&0u32.to_be_bytes()); // reserved
        b.extend_from_slice(&1u32.to_be_bytes()); // reserved + data_reference_index
        b.extend_from_slice(&[0; 16]);
        b.extend_from_slice(&width.to_be_bytes());
        b.extend_from_slice(&height.to_be_bytes());
        b.extend_from_slice(&[0x00, 0x48, 0x00, 0x00, 0x00, 0x48, 0x00, 0x00]); // 72 dpi
        b.extend_from_slice(&[0; 4]);
        b.extend_from_slice(&[0x00, 0x01]); // frame count
        b.extend_from_slice(&[0; 32]); // compressor name
        b.extend_from_slice(&[0x00, 0x18, 0xff, 0xff]); // depth, pre_defined
        mp4_box(b, if h265 { b"hvcC" } else { b"avcC" }, |b| b.extend_from_slice(record));
    });
    out
}

/// Sample rate and channel count from an AAC AudioSpecificConfig.
pub fn parse_asc(asc: &[u8]) -> Option<(u32, u8)> {
    const RATES: [u32; 13] = [96000, 88200, 64000, 48000, 44100, 32000, 24000, 22050, 16000, 12000, 11025, 8000, 7350];
    if asc.len() < 2 {
        return None;
    }
    let idx = ((asc[0] & 0x07) << 1) | (asc[1] >> 7);
    if idx == 15 {
        if asc.len() < 5 {
            return None;
        }
        let rate = (u32::from(asc[1] & 0x7f) << 17) | (u32::from(asc[2]) << 9) | (u32::from(asc[3]) << 1) | u32::from(asc[4] >> 7);
        let ch = (asc[4] >> 3) & 0x0f;
        return Some((rate, ch));
    }
    let rate = *RATES.get(idx as usize)?;
    let ch = (asc[1] >> 3) & 0x0f;
    Some((rate, ch))
}

/// `mp4a` AudioSampleEntry with an `esds` holding the AudioSpecificConfig.
pub fn aac_sample_entry(asc: &[u8]) -> Option<Vec<u8>> {
    let (rate, ch) = parse_asc(asc)?;
    if !(1..=2).contains(&ch) || rate > 65535 || asc.len() > 100 {
        return None;
    }
    let ncc = u32::from(ch);
    let desc = |b: &mut Vec<u8>, tag: u8, body: &[u8]| {
        b.push(tag);
        b.push(body.len() as u8); // all bodies here are < 128 bytes
        b.extend_from_slice(body);
    };
    let mut dsi = Vec::new();
    desc(&mut dsi, 0x05, asc);
    let mut dcd_body = vec![0x40, 0x15];
    let buffer = (6144 / 8) * ncc;
    dcd_body.extend_from_slice(&buffer.to_be_bytes()[1..]);
    dcd_body.extend_from_slice(&((6144 / 1024) * ncc * rate).to_be_bytes());
    dcd_body.extend_from_slice(&0u32.to_be_bytes());
    dcd_body.extend_from_slice(&dsi);
    let mut es_body = vec![0, 0, 0];
    desc(&mut es_body, 0x04, &dcd_body);
    desc(&mut es_body, 0x06, &[2]);
    let mut out = Vec::new();
    mp4_box(&mut out, b"mp4a", |b| {
        b.extend_from_slice(&[0, 0, 0, 0, 0, 0, 0, 1, 0, 0, 0, 0, 0, 0, 0, 0]);
        b.extend_from_slice(&(ch as u16).to_be_bytes());
        b.extend_from_slice(&[0x00, 0x10, 0, 0, 0, 0]);
        b.extend_from_slice(&(rate << 16).to_be_bytes());
        mp4_box(b, b"esds", |b| {
            b.extend_from_slice(&0u32.to_be_bytes());
            desc(b, 0x03, &es_body);
        });
    });
    Some(out)
}

// ---------------------------------------------------------------------------
// Frames
// ---------------------------------------------------------------------------

#[derive(Clone, Debug)]
pub struct VideoConfig {
    pub h265: bool,
    pub record: Vec<u8>,
    pub width: u32,
    pub height: u32,
}

/// What the reader hands on: frames in the RTSP reader's format, plus
/// timing marks (FLV timestamps share one clock, so every track gets a
/// mark mapping its ticks to the same wall-clock time).
#[derive(Debug)]
pub enum FlvItem {
    /// `ticks`: decode time (the FLV timestamp); `cts`: presentation time
    /// minus decode time (the tag's composition time; B-frames), both 90 kHz.
    Video { ticks: i64, cts: i32, data: Vec<u8>, key: bool, new_params: bool },
    Audio { ticks: i64, data: Vec<u8> },
    /// track 0 = video, 1 = audio.
    Timing { track: usize, ticks: i64, epoch_secs: f64 },
}

/// Turns tags into frames with 90 kHz video and sample-rate audio ticks.
#[derive(Default)]
pub struct FrameBuilder {
    size: (Option<u32>, Option<u32>),
    /// The metadata announced an audio track.
    pub audio_announced: bool,
    pending_video: Option<(bool, Vec<u8>)>,
    pub video: Option<VideoConfig>,
    pub audio_asc: Option<Vec<u8>>,
    audio_rate: u32,
    params_changed: bool,
    /// FLV milliseconds and local time of the first tag with a timestamp.
    base: Option<(i64, f64)>,
    timing_sent: [bool; 2],
}

impl FrameBuilder {
    fn now_secs() -> f64 {
        std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs_f64()).unwrap_or(0.0)
    }

    fn settle_video(&mut self) {
        let Some((h265, record)) = self.pending_video.take() else { return };
        let (Some(width), Some(height)) = self.size else {
            self.pending_video = Some((h265, record));
            return;
        };
        let cfg = VideoConfig { h265, record, width, height };
        let changed = self.video.as_ref().is_some_and(|v| v.record != cfg.record || v.width != width || v.height != height);
        if self.video.is_none() || changed {
            self.params_changed = true;
            self.video = Some(cfg);
        }
    }

    pub fn push(&mut self, tag: Tag, out: &mut VecDeque<FlvItem>) -> Result<()> {
        match tag {
            Tag::Meta { width, height, audio } => {
                self.size = (width.or(self.size.0), height.or(self.size.1));
                self.audio_announced |= audio;
                self.settle_video();
            }
            Tag::VideoConfig { h265, record } => {
                if codec_string(h265, &record).is_none() {
                    bail!("camera sent an unreadable video configuration");
                }
                self.pending_video = Some((h265, record));
                self.settle_video();
                if self.pending_video.is_some() {
                    bail!("camera did not say the picture size (no FLV metadata)");
                }
            }
            Tag::AudioConfig(asc) => {
                if let Some((rate, _)) = parse_asc(&asc) {
                    self.audio_rate = rate;
                    self.audio_asc = Some(asc);
                }
            }
            Tag::Video { ms, cts, key, data } => {
                if self.video.is_none() || data.is_empty() {
                    return Ok(());
                }
                let (base_ms, base_wall) = *self.base.get_or_insert((ms, Self::now_secs()));
                // FLV video timestamps are decode times; the composition time
                // offset says when the picture is shown (B-frames).
                let ticks = (ms - base_ms) * 90;
                let cts = cts.saturating_mul(90);
                if !self.timing_sent[0] {
                    self.timing_sent[0] = true;
                    out.push_back(FlvItem::Timing { track: 0, ticks, epoch_secs: base_wall + ticks as f64 / 90_000.0 });
                }
                let new_params = std::mem::take(&mut self.params_changed);
                out.push_back(FlvItem::Video { ticks, cts, data, key, new_params });
            }
            Tag::Audio { ms, data } => {
                if self.audio_asc.is_none() || self.audio_rate == 0 || data.is_empty() {
                    return Ok(());
                }
                let (base_ms, base_wall) = *self.base.get_or_insert((ms, Self::now_secs()));
                let ticks = (ms - base_ms) * i64::from(self.audio_rate) / 1000;
                if !self.timing_sent[1] {
                    self.timing_sent[1] = true;
                    out.push_back(FlvItem::Timing {
                        track: 1,
                        ticks,
                        epoch_secs: base_wall + ticks as f64 / f64::from(self.audio_rate),
                    });
                }
                out.push_back(FlvItem::Audio { ticks, data });
            }
            Tag::Other => {}
        }
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// HTTP
// ---------------------------------------------------------------------------

trait Io: AsyncRead + AsyncWrite + Unpin + Send {}
impl<T: AsyncRead + AsyncWrite + Unpin + Send> Io for T {}

/// Body framing of the HTTP response.
enum Body {
    Raw,
    /// Chunked: bytes left in the current chunk (None = expecting a size line).
    Chunked { left: Option<usize> },
}

/// A live FLV stream over HTTP(S).
pub struct FlvStream {
    io: Box<dyn Io>,
    body: Body,
    raw: Vec<u8>,
    tags: TagReader,
    frames: FrameBuilder,
    queue: VecDeque<FlvItem>,
    eof: bool,
}

impl FlvStream {
    /// Request the stream. `user`/`password` are added to the query unless
    /// the URL already carries them.
    pub async fn open(url: &str, user: &str, password: &str) -> Result<FlvStream> {
        let mut u = url::Url::parse(url).context("invalid stream URL")?;
        let has = |k: &str| u.query_pairs().any(|(n, _)| n == k);
        if !user.is_empty() && !has("user") {
            let (user_has, pass_has) = (has("user"), has("password"));
            let mut q = u.query_pairs_mut();
            if !user_has {
                q.append_pair("user", user);
            }
            if !pass_has {
                q.append_pair("password", password);
            }
        }
        let host = u.host_str().ok_or_else(|| anyhow!("stream URL has no host"))?.trim_matches(|c| c == '[' || c == ']').to_string();
        let tls = u.scheme() == "https";
        let port = u.port().unwrap_or(if tls { 443 } else { 80 });
        let tcp = tokio::net::TcpStream::connect((host.as_str(), port)).await.with_context(|| format!("cannot reach {host}:{port}"))?;
        tcp.set_nodelay(true).ok();
        let mut io: Box<dyn Io> = if tls {
            let connector = tokio_rustls::TlsConnector::from(tls_config());
            let name = rustls::pki_types::ServerName::try_from(host.clone()).map_err(|_| anyhow!("bad host name {host}"))?;
            Box::new(connector.connect(name, tcp).await.context("TLS handshake failed")?)
        } else {
            Box::new(tcp)
        };
        let path = match u.query() {
            Some(q) => format!("{}?{}", u.path(), q),
            None => u.path().to_string(),
        };
        let host_header = match u.port() {
            Some(p) => format!("{host}:{p}"),
            None => host.clone(),
        };
        let req = format!(
            "GET {path} HTTP/1.1\r\nHost: {host_header}\r\nUser-Agent: AmberSight/{}\r\nAccept: */*\r\nConnection: close\r\n\r\n",
            env!("CARGO_PKG_VERSION")
        );
        io.write_all(req.as_bytes()).await.context("sending the request failed")?;

        // Response head.
        let mut raw = Vec::new();
        let head_end = loop {
            if let Some(i) = raw.windows(4).position(|w| w == b"\r\n\r\n") {
                break i;
            }
            if raw.len() > 16 * 1024 {
                bail!("the camera's answer has no end of headers");
            }
            let mut chunk = [0u8; 4096];
            let n = io.read(&mut chunk).await.context("reading the answer failed")?;
            if n == 0 {
                bail!("the camera closed the connection without answering");
            }
            raw.extend_from_slice(&chunk[..n]);
        };
        let head = String::from_utf8_lossy(&raw[..head_end]).into_owned();
        raw.drain(..head_end + 4);
        let status = head.lines().next().unwrap_or("");
        let code = status.split_whitespace().nth(1).unwrap_or("");
        if code != "200" {
            bail!("camera answered {}", status.trim());
        }
        let header = |name: &str| {
            head.lines()
                .skip(1)
                .filter_map(|l| l.split_once(':'))
                .find(|(k, _)| k.trim().eq_ignore_ascii_case(name))
                .map(|(_, v)| v.trim().to_ascii_lowercase())
        };
        if let Some(ct) = header("content-type") {
            if !ct.contains("flv") && !ct.contains("octet-stream") {
                bail!("camera answered with {ct}, not a video stream (wrong URL or sign-in?)");
            }
        }
        let body = if header("transfer-encoding").is_some_and(|v| v.contains("chunked")) {
            Body::Chunked { left: None }
        } else {
            Body::Raw
        };
        Self::start(io, body, &raw).await
    }

    /// A pushed stream: plain FLV bytes from `io` (no HTTP framing).
    pub async fn from_io<T: AsyncRead + AsyncWrite + Unpin + Send + 'static>(io: T) -> Result<FlvStream> {
        Self::start(Box::new(io), Body::Raw, &[]).await
    }

    async fn start(io: Box<dyn Io>, body: Body, raw: &[u8]) -> Result<FlvStream> {
        let mut s = FlvStream {
            io,
            body,
            raw: Vec::new(),
            tags: TagReader::default(),
            frames: FrameBuilder::default(),
            queue: VecDeque::new(),
            eof: false,
        };
        s.feed(raw)?;
        // Codec setup comes in-band, audio's a little after the first video
        // frames. Read ahead (frames are queued, not lost) until the video
        // and any announced audio are known, so a recording made from the
        // first keyframe includes the audio track.
        let until = tokio::time::Instant::now() + std::time::Duration::from_secs(3);
        while !s.eof && !(s.frames.video.is_some() && (!s.frames.audio_announced || s.frames.audio_asc.is_some())) {
            let mut chunk = vec![0u8; 64 * 1024];
            match tokio::time::timeout_at(until, s.io.read(&mut chunk)).await {
                Err(_) => break,
                Ok(Ok(0)) => s.eof = true,
                Ok(Ok(n)) => s.feed(&chunk[..n])?,
                Ok(Err(e)) => bail!("connection lost: {e}"),
            }
        }
        Ok(s)
    }

    /// Decode body framing and parse whatever complete tags are there.
    fn feed(&mut self, data: &[u8]) -> Result<()> {
        match &mut self.body {
            Body::Raw => self.tags.push(data),
            Body::Chunked { left } => {
                self.raw.extend_from_slice(data);
                let mut pos = 0;
                loop {
                    match left {
                        Some(0) => {
                            // CRLF after a chunk.
                            if self.raw.len() - pos < 2 {
                                break;
                            }
                            pos += 2;
                            *left = None;
                        }
                        Some(n) => {
                            let take = (*n).min(self.raw.len() - pos);
                            if take == 0 {
                                break;
                            }
                            self.tags.push(&self.raw[pos..pos + take]);
                            pos += take;
                            *n -= take;
                        }
                        None => {
                            let Some(i) = self.raw[pos..].windows(2).position(|w| w == b"\r\n") else {
                                if self.raw.len() - pos > 1024 {
                                    bail!("bad chunked encoding from camera");
                                }
                                break;
                            };
                            let line = String::from_utf8_lossy(&self.raw[pos..pos + i]).into_owned();
                            let size = usize::from_str_radix(line.split(';').next().unwrap_or("").trim(), 16)
                                .map_err(|_| anyhow!("bad chunk size from camera"))?;
                            pos += i + 2;
                            if size == 0 {
                                self.eof = true;
                                break;
                            }
                            *left = Some(size);
                        }
                    }
                }
                self.raw.drain(..pos);
            }
        }
        while let Some(tag) = self.tags.next_tag()? {
            self.frames.push(tag, &mut self.queue)?;
        }
        Ok(())
    }

    /// Next item; None when the stream ended. Cancel-safe.
    pub async fn next(&mut self) -> Option<Result<FlvItem>> {
        loop {
            if let Some(item) = self.queue.pop_front() {
                return Some(Ok(item));
            }
            if self.eof {
                return None;
            }
            let mut chunk = vec![0u8; 64 * 1024];
            match self.io.read(&mut chunk).await {
                Ok(0) => {
                    self.eof = true;
                }
                Ok(n) => {
                    if let Err(e) = self.feed(&chunk[..n]) {
                        self.eof = true;
                        return Some(Err(e));
                    }
                }
                Err(e) => {
                    self.eof = true;
                    return Some(Err(anyhow!("connection lost: {e}")));
                }
            }
        }
    }

    pub fn video(&self) -> Option<&VideoConfig> {
        self.frames.video.as_ref()
    }

    pub fn audio_config(&self) -> Option<&[u8]> {
        self.frames.audio_asc.as_deref()
    }
}

/// TLS without certificate checks: devices on the LAN use self-signed
/// certificates, and this client only talks to the configured camera.
fn tls_config() -> Arc<rustls::ClientConfig> {
    static CFG: std::sync::OnceLock<Arc<rustls::ClientConfig>> = std::sync::OnceLock::new();
    CFG.get_or_init(|| {
        let provider = Arc::new(rustls::crypto::ring::default_provider());
        let cfg = rustls::ClientConfig::builder_with_provider(provider.clone())
            .with_safe_default_protocol_versions()
            .expect("ring supports the default TLS versions")
            .dangerous()
            .with_custom_certificate_verifier(Arc::new(AnyCert(provider)))
            .with_no_client_auth();
        Arc::new(cfg)
    })
    .clone()
}

#[derive(Debug)]
struct AnyCert(Arc<rustls::crypto::CryptoProvider>);

impl rustls::client::danger::ServerCertVerifier for AnyCert {
    fn verify_server_cert(
        &self,
        _end_entity: &rustls::pki_types::CertificateDer<'_>,
        _intermediates: &[rustls::pki_types::CertificateDer<'_>],
        _server_name: &rustls::pki_types::ServerName<'_>,
        _ocsp_response: &[u8],
        _now: rustls::pki_types::UnixTime,
    ) -> std::result::Result<rustls::client::danger::ServerCertVerified, rustls::Error> {
        Ok(rustls::client::danger::ServerCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        _message: &[u8],
        _cert: &rustls::pki_types::CertificateDer<'_>,
        _dss: &rustls::DigitallySignedStruct,
    ) -> std::result::Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        // The certificate itself is not trusted, so checking that the server
        // holds its key proves nothing; and some devices use X.509 v1
        // certificates the checker cannot read.
        Ok(rustls::client::danger::HandshakeSignatureValid::assertion())
    }

    fn verify_tls13_signature(
        &self,
        _message: &[u8],
        _cert: &rustls::pki_types::CertificateDer<'_>,
        _dss: &rustls::DigitallySignedStruct,
    ) -> std::result::Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        // The certificate itself is not trusted, so checking that the server
        // holds its key proves nothing; and some devices use X.509 v1
        // certificates the checker cannot read.
        Ok(rustls::client::danger::HandshakeSignatureValid::assertion())
    }

    fn supported_verify_schemes(&self) -> Vec<rustls::SignatureScheme> {
        self.0.signature_verification_algorithms.supported_schemes()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tag(typ: u8, ms: u32, body: &[u8]) -> Vec<u8> {
        let mut t = vec![typ];
        t.extend_from_slice(&(body.len() as u32).to_be_bytes()[1..]);
        t.extend_from_slice(&ms.to_be_bytes()[1..]);
        t.push((ms >> 24) as u8);
        t.extend_from_slice(&[0, 0, 0]);
        t.extend_from_slice(body);
        t.extend_from_slice(&((11 + body.len()) as u32).to_be_bytes());
        t
    }

    fn meta(w: f64, h: f64) -> Vec<u8> {
        let mut b = vec![2, 0, 10];
        b.extend_from_slice(b"onMetaData");
        b.extend_from_slice(&[8, 0, 0, 0, 2]);
        for (k, v) in [("width", w), ("height", h)] {
            b.extend_from_slice(&(k.len() as u16).to_be_bytes());
            b.extend_from_slice(k.as_bytes());
            b.push(0);
            b.extend_from_slice(&v.to_be_bytes());
        }
        b.extend_from_slice(&[0, 0, 9]);
        b
    }

    // avcC and AAC config as sent by a Reolink Home Hub (sub stream).
    const AVCC: &str = "01640033ffe1000b67640033ac1514a0600de401000468ee3cb0";

    fn hex(s: &str) -> Vec<u8> {
        (0..s.len()).step_by(2).map(|i| u8::from_str_radix(&s[i..i + 2], 16).unwrap()).collect()
    }

    fn sample_stream() -> Vec<u8> {
        let mut s = b"FLV\x01\x05\x00\x00\x00\x09\x00\x00\x00\x00".to_vec();
        s.extend(tag(18, 0, &meta(1536.0, 432.0)));
        let mut vc = vec![0x17, 0, 0, 0, 0];
        vc.extend(hex(AVCC));
        s.extend(tag(9, 0, &vc));
        s.extend(tag(9, 0, &[0x17, 1, 0, 0, 0, 0, 0, 0, 2, 0x65, 0x88]));
        s.extend(tag(9, 66, &[0x27, 1, 0, 0, 0, 0, 0, 0, 2, 0x41, 0x9a]));
        s.extend(tag(8, 66, &[0xae, 0, 0x14, 0x08]));
        s.extend(tag(8, 64, &[0xae, 1, 0x21, 0x10]));
        s
    }

    #[test]
    fn parses_reolink_style_stream_in_pieces() {
        let data = sample_stream();
        let mut r = TagReader::default();
        let mut fb = FrameBuilder::default();
        let mut out = VecDeque::new();
        // Feed in awkward 7-byte pieces.
        for piece in data.chunks(7) {
            r.push(piece);
            while let Some(t) = r.next_tag().unwrap() {
                fb.push(t, &mut out).unwrap();
            }
        }
        let v = fb.video.as_ref().unwrap();
        assert_eq!((v.width, v.height, v.h265), (1536, 432, false));
        assert_eq!(codec_string(false, &v.record).unwrap(), "avc1.640033");
        let items: Vec<_> = out.into_iter().collect();
        assert!(matches!(items[0], FlvItem::Timing { track: 0, ticks: 0, .. }));
        assert!(matches!(&items[1], FlvItem::Video { ticks: 0, key: true, new_params: true, data, .. } if data.len() == 6));
        assert!(matches!(&items[2], FlvItem::Video { ticks: 5940, key: false, new_params: false, .. }));
        // Audio at 64 ms, 16 kHz: 1024 ticks, timed on the same clock.
        assert!(matches!(items[3], FlvItem::Timing { track: 1, ticks: 1024, .. }));
        assert!(matches!(&items[4], FlvItem::Audio { ticks: 1024, .. }));
        if let (FlvItem::Timing { epoch_secs: v, .. }, FlvItem::Timing { epoch_secs: a, .. }) = (&items[0], &items[3]) {
            assert!((a - v - 0.064).abs() < 1e-6);
        }
    }

    #[test]
    fn composition_time_gives_decode_time_and_offset() {
        // An encoder with B-frames (ffmpeg/OBS): timestamps are decode times,
        // the signed 24-bit composition time says when each picture shows.
        assert_eq!(si24(&[0xff, 0xff, 0xdf]), -33);
        assert_eq!(si24(&[0, 0, 100]), 100);
        let mut s = b"FLV\x01\x01\x00\x00\x00\x09\x00\x00\x00\x00".to_vec();
        s.extend(tag(18, 0, &meta(640.0, 480.0)));
        let mut vc = vec![0x17, 0, 0, 0, 0];
        vc.extend(hex(AVCC));
        s.extend(tag(9, 0, &vc));
        // I (cts 33), P (cts 100), B (cts 0), B (cts 0): decode 0, 33, 67, 100 ms.
        s.extend(tag(9, 0, &[0x17, 1, 0, 0, 33, 0, 0, 0, 1, 0x65]));
        s.extend(tag(9, 33, &[0x27, 1, 0, 0, 100, 0, 0, 0, 1, 0x41]));
        s.extend(tag(9, 67, &[0x27, 1, 0, 0, 0, 0, 0, 0, 1, 0x01]));
        s.extend(tag(9, 100, &[0x27, 1, 0, 0, 0, 0, 0, 0, 1, 0x01]));
        // Enhanced FLV (avc1 FourCC), coded frames with a negative offset.
        let mut ex = vec![0x80 | 0x20 | 1];
        ex.extend_from_slice(b"avc1");
        ex.extend_from_slice(&[0xff, 0xff, 0xdf, 0, 0, 0, 1, 0x01]);
        s.extend(tag(9, 133, &ex));
        let mut r = TagReader::default();
        r.push(&s);
        let mut fb = FrameBuilder::default();
        let mut out = VecDeque::new();
        while let Some(t) = r.next_tag().unwrap() {
            fb.push(t, &mut out).unwrap();
        }
        let got: Vec<(i64, i32)> = out
            .iter()
            .filter_map(|i| match i {
                FlvItem::Video { ticks, cts, .. } => Some((*ticks, *cts)),
                _ => None,
            })
            .collect();
        assert_eq!(got, vec![(0, 2970), (2970, 9000), (6030, 0), (9000, 0), (11970, -2970)]);
    }

    #[test]
    fn hevc_codec_string_matches_hub_record() {
        // Start of the hvcC the hub sends for the 5120x1440 main stream.
        let r = hex("01016000000000000000000096f001fcfdf8f800000f03");
        assert_eq!(codec_string(true, &r).unwrap(), "hvc1.1.6.L150.00");
        assert_eq!(codec_string(false, &hex(AVCC)).unwrap(), "avc1.640033");
    }

    #[test]
    fn sample_entries_have_the_right_shape() {
        let v = video_sample_entry(false, &hex(AVCC), 1536, 432);
        assert_eq!(&v[4..8], b"avc1");
        assert_eq!(u32::from_be_bytes(v[0..4].try_into().unwrap()) as usize, v.len());
        assert_eq!(&v[90..94], b"avcC");
        let a = aac_sample_entry(&[0x14, 0x08]).unwrap();
        assert_eq!(&a[4..8], b"mp4a");
        assert_eq!(u32::from_be_bytes(a[0..4].try_into().unwrap()) as usize, a.len());
        assert_eq!(parse_asc(&[0x14, 0x08]), Some((16000, 1)));
        // esds: version, ES descriptor (tag 3) ending with SL config 06 01 02.
        assert_eq!(&a[a.len() - 3..], &[6, 1, 2]);
    }

    #[test]
    fn recognizes_flv_urls() {
        assert!(is_flv_url("https://192.168.1.5/flv?port=1935&app=bcs&stream=channel0_sub.bcs"));
        assert!(is_flv_url("http://cam/flv"));
        assert!(!is_flv_url("rtsp://cam/flv"));
        assert!(!is_flv_url("https://cam/cgi-bin/api.cgi"));
    }
}
