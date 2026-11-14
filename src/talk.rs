//! Two-way audio ("talk") through the RTSP audio backchannel: a minimal
//! RTSP client over TCP that asks the camera for its `sendonly` audio track,
//! sets it up with interleaved transport and streams G.711 RTP to it, plus
//! the PCM -> 8 kHz -> G.711 conversion and the WebSocket entry point.

use std::collections::HashSet;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use anyhow::{anyhow, bail, Context, Result};
use axum::extract::ws::{CloseFrame, Message, WebSocket, WebSocketUpgrade};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use base64::Engine as _;
use md5::Digest as _;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::tcp::OwnedWriteHalf;
use tokio::net::TcpStream;
use tokio::sync::mpsc;
use tokio::time::{interval, timeout, Instant, MissedTickBehavior};

use crate::engine::Engine;

const REQUIRE: &str = "www.onvif.org/ver20/backchannel";
const USER_AGENT: &str = "AmberSight";
/// One RTP packet carries 20 ms of 8 kHz audio.
pub const SAMPLES_PER_PACKET: usize = 160;
const PACKET_INTERVAL: Duration = Duration::from_millis(20);
const KEEPALIVE_EVERY: Duration = Duration::from_secs(20);
/// Above this much queued audio (200 ms) the oldest is dropped ...
const MAX_QUEUED: usize = SAMPLES_PER_PACKET * 10;
/// ... down to this much (60 ms), keeping latency low.
const QUEUE_KEEP: usize = SAMPLES_PER_PACKET * 3;

// ---------------------------------------------------------------- G.711

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Codec {
    /// G.711 mu-law, static payload type 0.
    Pcmu,
    /// G.711 A-law, static payload type 8.
    Pcma,
}

impl Codec {
    pub fn name(self) -> &'static str {
        match self {
            Codec::Pcmu => "PCMU/8000",
            Codec::Pcma => "PCMA/8000",
        }
    }
    pub fn silence(self) -> u8 {
        self.encode(0)
    }
    pub fn encode(self, s: i16) -> u8 {
        match self {
            Codec::Pcmu => linear_to_ulaw(s),
            Codec::Pcma => linear_to_alaw(s),
        }
    }
}

pub fn linear_to_ulaw(sample: i16) -> u8 {
    const BIAS: i32 = 0x84;
    const CLIP: i32 = 32635;
    let mut v = sample as i32;
    let sign = if v < 0 {
        v = -v;
        0x80u8
    } else {
        0
    };
    v = v.min(CLIP) + BIAS;
    let mut exp = 7u8;
    let mut mask = 0x4000;
    while exp > 0 && v & mask == 0 {
        exp -= 1;
        mask >>= 1;
    }
    let mantissa = ((v >> (exp + 3)) & 0x0F) as u8;
    !(sign | (exp << 4) | mantissa)
}

pub fn linear_to_alaw(sample: i16) -> u8 {
    let (mag, mask) = if sample >= 0 { (sample as i32, 0xD5u8) } else { (-(sample as i32) - 1, 0x55u8) };
    let (exp, man) = if mag >= 256 {
        let exp = (32 - (mag as u32).leading_zeros()) as u8 - 8;
        (exp, ((mag >> (exp + 3)) & 0x0F) as u8)
    } else {
        (0, ((mag >> 4) & 0x0F) as u8)
    };
    ((exp << 4) | man) ^ mask
}

// ----------------------------------------------------------- resampling

/// Streaming box-filter resampler from an arbitrary rate to 8 kHz.
/// Rates below 8 kHz are repeated (zero-order hold).
pub struct Resampler {
    ratio: f64,
    filled: f64,
    sum: f64,
}

impl Resampler {
    pub fn new(rate: u32) -> Self {
        Resampler { ratio: rate.max(1) as f64 / 8000.0, filled: 0.0, sum: 0.0 }
    }

    pub fn push(&mut self, input: &[i16], out: &mut Vec<i16>) {
        for &s in input {
            let mut rem = 1.0f64;
            while rem > 1e-9 {
                let take = rem.min(self.ratio - self.filled);
                self.sum += s as f64 * take;
                self.filled += take;
                rem -= take;
                if self.filled >= self.ratio - 1e-9 {
                    out.push((self.sum / self.ratio).round().clamp(-32768.0, 32767.0) as i16);
                    self.sum = 0.0;
                    self.filled = 0.0;
                }
            }
        }
    }
}

/// PCM16LE bytes at some rate in, 160-byte G.711 packets out, with a bounded
/// queue so a stalled network never builds up latency.
pub struct Packetizer {
    codec: Codec,
    resampler: Resampler,
    carry: Option<u8>,
    queue: Vec<u8>,
    scratch: Vec<i16>,
    pub dropped: usize,
}

impl Packetizer {
    pub fn new(codec: Codec, rate: u32) -> Self {
        Packetizer { codec, resampler: Resampler::new(rate), carry: None, queue: Vec::new(), scratch: Vec::new(), dropped: 0 }
    }

    pub fn push_pcm(&mut self, bytes: &[u8]) {
        let mut all = Vec::with_capacity(bytes.len() + 1);
        all.extend(self.carry.take());
        all.extend_from_slice(bytes);
        let pairs = all.chunks_exact(2);
        self.carry = pairs.remainder().first().copied();
        let samples: Vec<i16> = pairs.map(|p| i16::from_le_bytes([p[0], p[1]])).collect();
        self.scratch.clear();
        self.resampler.push(&samples, &mut self.scratch);
        let codec = self.codec;
        self.queue.extend(self.scratch.iter().map(|&s| codec.encode(s)));
        if self.queue.len() > MAX_QUEUED {
            let drop = self.queue.len() - QUEUE_KEEP;
            self.queue.drain(..drop);
            self.dropped += drop;
        }
    }

    pub fn pop_packet(&mut self) -> Option<Vec<u8>> {
        if self.queue.len() < SAMPLES_PER_PACKET {
            return None;
        }
        Some(self.queue.drain(..SAMPLES_PER_PACKET).collect())
    }
}

/// Minimal WAV reader: 16-bit PCM, any rate; multi-channel is mixed to mono.
pub fn read_wav(data: &[u8]) -> Result<(u32, Vec<i16>)> {
    if data.len() < 12 || &data[0..4] != b"RIFF" || &data[8..12] != b"WAVE" {
        bail!("not a RIFF/WAVE file");
    }
    let (mut rate, mut channels, mut bits) = (0u32, 0usize, 0u16);
    let mut pos = 12;
    while pos + 8 <= data.len() {
        let id = &data[pos..pos + 4];
        let len = u32::from_le_bytes(data[pos + 4..pos + 8].try_into().unwrap()) as usize;
        let body = &data[pos + 8..(pos + 8).saturating_add(len).min(data.len())];
        if id == b"fmt " && body.len() >= 16 {
            let format = u16::from_le_bytes([body[0], body[1]]);
            channels = u16::from_le_bytes([body[2], body[3]]) as usize;
            rate = u32::from_le_bytes(body[4..8].try_into().unwrap());
            bits = u16::from_le_bytes([body[14], body[15]]);
            if format != 1 {
                bail!("only PCM WAV files are supported");
            }
        } else if id == b"data" {
            if bits != 16 || channels == 0 {
                bail!("only 16-bit PCM WAV files are supported");
            }
            let all: Vec<i16> = body.chunks_exact(2).map(|b| i16::from_le_bytes([b[0], b[1]])).collect();
            let mono = all.chunks_exact(channels).map(|f| (f.iter().map(|&s| s as i32).sum::<i32>() / channels as i32) as i16).collect();
            return Ok((rate, mono));
        }
        pos = pos.saturating_add(8).saturating_add(len).saturating_add(len & 1);
    }
    bail!("WAV file has no data chunk")
}

// ------------------------------------------------------------------ RTP

pub fn rtp_packet(pt: u8, seq: u16, timestamp: u32, ssrc: u32, payload: &[u8]) -> Vec<u8> {
    let mut p = Vec::with_capacity(12 + payload.len());
    p.push(0x80);
    p.push(pt & 0x7F);
    p.extend_from_slice(&seq.to_be_bytes());
    p.extend_from_slice(&timestamp.to_be_bytes());
    p.extend_from_slice(&ssrc.to_be_bytes());
    p.extend_from_slice(payload);
    p
}

/// RTSP interleaved binary frame: `$`, channel, 16-bit length, payload.
pub fn interleaved_frame(channel: u8, payload: &[u8]) -> Vec<u8> {
    let mut f = Vec::with_capacity(4 + payload.len());
    f.push(b'$');
    f.push(channel);
    f.extend_from_slice(&(payload.len() as u16).to_be_bytes());
    f.extend_from_slice(payload);
    f
}

// ------------------------------------------------------------------ SDP

#[derive(Debug, PartialEq, Eq)]
pub struct Track {
    pub control_url: String,
    pub payload_type: u8,
    pub codec: Codec,
}

/// Resolve a media `a=control` value against the Content-Base.
fn resolve_control(base: &str, control: Option<&str>) -> String {
    match control.map(str::trim) {
        None | Some("") | Some("*") => base.to_string(),
        Some(c) if c.to_ascii_lowercase().starts_with("rtsp://") => c.to_string(),
        Some(c) => {
            if base.ends_with('/') {
                format!("{base}{}", c.trim_start_matches('/'))
            } else {
                format!("{base}/{}", c.trim_start_matches('/'))
            }
        }
    }
}

/// Find the `sendonly` audio section of an SDP and pick a supported codec.
pub fn parse_backchannel(sdp: &str, base: &str) -> Result<Track> {
    struct Media {
        audio: bool,
        sendonly: bool,
        pts: Vec<u8>,
        rtpmap: Vec<(u8, String)>,
        control: Option<String>,
    }
    let mut medias: Vec<Media> = Vec::new();
    for line in sdp.lines().map(str::trim) {
        if let Some(m) = line.strip_prefix("m=") {
            let mut f = m.split_whitespace();
            let kind = f.next().unwrap_or("");
            let pts = f.skip(2).filter_map(|p| p.parse().ok()).collect();
            medias.push(Media { audio: kind == "audio", sendonly: false, pts, rtpmap: vec![], control: None });
        } else if let Some(cur) = medias.last_mut() {
            if line == "a=sendonly" {
                cur.sendonly = true;
            } else if let Some(r) = line.strip_prefix("a=rtpmap:") {
                if let Some((pt, enc)) = r.split_once(' ') {
                    if let Ok(pt) = pt.trim().parse() {
                        cur.rtpmap.push((pt, enc.trim().to_string()));
                    }
                }
            } else if let Some(c) = line.strip_prefix("a=control:") {
                cur.control = Some(c.trim().to_string());
            }
        }
    }
    let m = medias
        .iter()
        .find(|m| m.audio && m.sendonly)
        .ok_or_else(|| anyhow!("camera offers no audio backchannel (no sendonly audio track in its SDP)"))?;
    let mut offered = Vec::new();
    for &pt in &m.pts {
        let enc = m.rtpmap.iter().find(|(p, _)| *p == pt).map(|(_, e)| e.to_ascii_uppercase());
        let codec = match (pt, enc.as_deref()) {
            (_, Some(e)) if e.starts_with("PCMU/8000") => Some(Codec::Pcmu),
            (_, Some(e)) if e.starts_with("PCMA/8000") => Some(Codec::Pcma),
            (0, None) => Some(Codec::Pcmu),
            (8, None) => Some(Codec::Pcma),
            _ => None,
        };
        offered.push(enc.unwrap_or_else(|| format!("payload {pt}")));
        if let Some(codec) = codec {
            return Ok(Track { control_url: resolve_control(base, m.control.as_deref()), payload_type: pt, codec });
        }
    }
    bail!("backchannel codec not supported (camera offers {}); only G.711 PCMU/PCMA at 8000 Hz can be sent", offered.join(", "))
}

// --------------------------------------------------------- Digest / Basic

fn md5_hex(s: &str) -> String {
    md5::Md5::digest(s.as_bytes()).iter().map(|b| format!("{b:02x}")).collect()
}

/// RFC 2617 response value (MD5). `qop` = (nc, cnonce) when qop=auth.
pub fn digest_response(user: &str, pass: &str, realm: &str, nonce: &str, method: &str, uri: &str, qop: Option<(&str, &str)>) -> String {
    let ha1 = md5_hex(&format!("{user}:{realm}:{pass}"));
    let ha2 = md5_hex(&format!("{method}:{uri}"));
    match qop {
        Some((nc, cnonce)) => md5_hex(&format!("{ha1}:{nonce}:{nc}:{cnonce}:auth:{ha2}")),
        None => md5_hex(&format!("{ha1}:{nonce}:{ha2}")),
    }
}

#[derive(Debug, Clone)]
enum Auth {
    Digest { realm: String, nonce: String, opaque: Option<String>, qop: bool, nc: u32 },
    Basic,
}

fn parse_params(s: &str) -> Vec<(String, String)> {
    let mut out = Vec::new();
    let mut rest = s.trim();
    while let Some(eq) = rest.find('=') {
        let key = rest[..eq].trim().trim_start_matches(',').trim().to_ascii_lowercase();
        let after = rest[eq + 1..].trim_start();
        let (val, tail) = if let Some(q) = after.strip_prefix('"') {
            match q.find('"') {
                Some(end) => (q[..end].to_string(), &q[end + 1..]),
                None => (q.to_string(), ""),
            }
        } else {
            let end = after.find(',').unwrap_or(after.len());
            (after[..end].trim().to_string(), &after[end..])
        };
        out.push((key, val));
        rest = tail.trim_start().trim_start_matches(',').trim_start();
    }
    out
}

/// Pick Digest over Basic when the camera offers both.
fn parse_challenges(values: &[&str]) -> Option<Auth> {
    let mut basic = false;
    for v in values {
        let v = v.trim();
        if v.len() >= 6 && v[..6].eq_ignore_ascii_case("digest") {
            let p = parse_params(&v[6..]);
            let get = |k: &str| p.iter().find(|(n, _)| n == k).map(|(_, v)| v.clone());
            return Some(Auth::Digest {
                realm: get("realm").unwrap_or_default(),
                nonce: get("nonce")?,
                opaque: get("opaque"),
                qop: get("qop").is_some_and(|q| q.split(',').any(|x| x.trim() == "auth")),
                nc: 0,
            });
        }
        if v.len() >= 5 && v[..5].eq_ignore_ascii_case("basic") {
            basic = true;
        }
    }
    basic.then_some(Auth::Basic)
}

fn authorization(auth: &mut Option<Auth>, user: &str, pass: &str, method: &str, uri: &str) -> Option<String> {
    match auth.as_mut()? {
        Auth::Basic => {
            let tok = base64::engine::general_purpose::STANDARD.encode(format!("{user}:{pass}"));
            Some(format!("Basic {tok}"))
        }
        Auth::Digest { realm, nonce, opaque, qop, nc } => {
            *nc += 1;
            let mut h = format!("Digest username=\"{user}\", realm=\"{realm}\", nonce=\"{nonce}\", uri=\"{uri}\"");
            if *qop {
                let ncs = format!("{:08x}", *nc);
                let cnonce = format!("{:016x}", rand::random::<u64>());
                let resp = digest_response(user, pass, realm, nonce, method, uri, Some((&ncs, &cnonce)));
                h.push_str(&format!(", response=\"{resp}\", qop=auth, nc={ncs}, cnonce=\"{cnonce}\""));
            } else {
                h.push_str(&format!(", response=\"{}\"", digest_response(user, pass, realm, nonce, method, uri, None)));
            }
            if let Some(o) = opaque {
                h.push_str(&format!(", opaque=\"{o}\""));
            }
            Some(h)
        }
    }
}

// ----------------------------------------------------------------- RTSP

struct RtspResponse {
    status: u16,
    line: String,
    headers: Vec<(String, String)>,
    body: String,
}

impl RtspResponse {
    fn header(&self, name: &str) -> Option<&str> {
        self.headers.iter().find(|(n, _)| n == name).map(|(_, v)| v.as_str())
    }
}

/// (credential-free RTSP URL, user, password, host, port) from a camera URL.
/// Configured credentials win; otherwise those embedded in the URL are used.
pub fn split_url(raw: &str, user: &str, pass: &str) -> Result<(String, String, String, String, u16)> {
    let u = url::Url::parse(raw).context("invalid camera URL")?;
    if u.scheme() != "rtsp" {
        bail!("talk needs an rtsp:// camera URL");
    }
    let host = u.host_str().context("camera URL has no host")?.to_string();
    let port = u.port().unwrap_or(554);
    let dec = |s: &str| percent_encoding::percent_decode_str(s).decode_utf8_lossy().into_owned();
    let (user, pass) = if user.is_empty() { (dec(u.username()), dec(u.password().unwrap_or(""))) } else { (user.to_string(), pass.to_string()) };
    let host_part = if host.contains(':') { format!("[{host}]") } else { host.clone() };
    let mut req_url = format!("rtsp://{host_part}:{port}{}", u.path());
    if let Some(q) = u.query() {
        req_url.push('?');
        req_url.push_str(q);
    }
    Ok((req_url, user, pass, host, port))
}

/// Request/response phase of the connection, before streaming starts.
struct Handshake {
    stream: TcpStream,
    buf: Vec<u8>,
    cseq: u32,
    auth: Option<Auth>,
    user: String,
    pass: String,
    session: Option<String>,
}

impl Handshake {
    fn build(&mut self, method: &str, url: &str, extra: &[(&str, String)]) -> String {
        self.cseq += 1;
        let mut req = format!("{method} {url} RTSP/1.0\r\nCSeq: {}\r\nUser-Agent: {USER_AGENT}\r\nRequire: {REQUIRE}\r\n", self.cseq);
        if let Some(s) = &self.session {
            req.push_str(&format!("Session: {s}\r\n"));
        }
        for (k, v) in extra {
            req.push_str(&format!("{k}: {v}\r\n"));
        }
        if let Some(h) = authorization(&mut self.auth, &self.user, &self.pass, method, url) {
            req.push_str(&format!("Authorization: {h}\r\n"));
        }
        req.push_str("\r\n");
        req
    }

    /// Send a request; on a 401 with a challenge answer it once on the same
    /// connection (the Digest nonce is bound to it).
    async fn call(&mut self, method: &str, url: &str, extra: &[(&str, String)], log: &(dyn Fn(&str) + Sync)) -> Result<RtspResponse> {
        for attempt in 0..2 {
            let req = self.build(method, url, extra);
            self.stream.write_all(req.as_bytes()).await?;
            let resp = read_response(&mut self.stream, &mut self.buf).await?;
            log(&format!("{method} -> {}", resp.line));
            if resp.status == 401 && attempt == 0 {
                let ch: Vec<&str> = resp.headers.iter().filter(|(n, _)| n == "www-authenticate").map(|(_, v)| v.as_str()).collect();
                if self.user.is_empty() {
                    bail!("camera requires authentication but no credentials are configured");
                }
                self.auth = Some(parse_challenges(&ch).context("camera sent a 401 with no usable authentication challenge")?);
                continue;
            }
            return Ok(resp);
        }
        bail!("authentication rejected (wrong username or password?)")
    }
}

/// Read one RTSP response, skipping any interleaved `$` frames before it.
async fn read_response(stream: &mut TcpStream, buf: &mut Vec<u8>) -> Result<RtspResponse> {
    let fut = async {
        loop {
            while buf.first() == Some(&b'$') && buf.len() >= 4 {
                let n = 4 + u16::from_be_bytes([buf[2], buf[3]]) as usize;
                if buf.len() < n {
                    break;
                }
                buf.drain(..n);
            }
            if buf.first() != Some(&b'$') {
                if let Some(end) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
                    let head = String::from_utf8_lossy(&buf[..end]).into_owned();
                    let mut lines = head.split("\r\n");
                    let line = lines.next().unwrap_or("").to_string();
                    let status = line.split_whitespace().nth(1).and_then(|s| s.parse().ok()).ok_or_else(|| anyhow!("bad RTSP status line: {line}"))?;
                    let headers: Vec<(String, String)> =
                        lines.filter_map(|l| l.split_once(':')).map(|(k, v)| (k.trim().to_ascii_lowercase(), v.trim().to_string())).collect();
                    let clen: usize = headers.iter().find(|(k, _)| k == "content-length").and_then(|(_, v)| v.parse().ok()).unwrap_or(0);
                    if buf.len() >= end + 4 + clen {
                        let body = String::from_utf8_lossy(&buf[end + 4..end + 4 + clen]).into_owned();
                        buf.drain(..end + 4 + clen);
                        return Ok(RtspResponse { status, line, headers, body });
                    }
                }
            }
            let mut chunk = [0u8; 4096];
            let n = stream.read(&mut chunk).await?;
            if n == 0 {
                bail!("camera closed the connection");
            }
            buf.extend_from_slice(&chunk[..n]);
        }
    };
    timeout(Duration::from_secs(8), fut).await.map_err(|_| anyhow!("timed out waiting for the camera's RTSP response"))?
}

/// Read and discard everything the camera sends (RTCP etc.), reporting any
/// RTSP status lines it spots so keepalive and teardown replies are visible.
async fn drain(mut rd: tokio::net::tcp::OwnedReadHalf, tx: mpsc::UnboundedSender<String>) {
    let mut acc: Vec<u8> = Vec::new();
    let mut chunk = [0u8; 4096];
    while let Ok(n) = rd.read(&mut chunk).await {
        if n == 0 {
            break;
        }
        acc.extend_from_slice(&chunk[..n]);
        while let Some(i) = acc.windows(9).position(|w| w == b"RTSP/1.0 ") {
            match acc[i..].windows(2).position(|w| w == b"\r\n") {
                Some(e) => {
                    let _ = tx.send(String::from_utf8_lossy(&acc[i..i + e]).into_owned());
                    acc.drain(..i + e);
                }
                None => break,
            }
        }
        if acc.len() > 8192 {
            let keep = acc.len() - 16;
            acc.drain(..keep);
        }
    }
}

/// An established backchannel, ready to take G.711 payloads of 20 ms.
pub struct Backchannel {
    wr: OwnedWriteHalf,
    reader: tokio::task::JoinHandle<()>,
    statuses: mpsc::UnboundedReceiver<String>,
    pub codec: Codec,
    pub session: String,
    pub packets: u64,
    channel: u8,
    payload_type: u8,
    seq: u16,
    timestamp: u32,
    ssrc: u32,
    cseq: u32,
    url: String,
    auth: Option<Auth>,
    user: String,
    pass: String,
    last_keepalive: Instant,
}

impl Backchannel {
    /// Connect, DESCRIBE (with the backchannel Require header), SETUP the
    /// camera's `sendonly` audio track over interleaved TCP, and PLAY.
    pub async fn open(raw_url: &str, user: &str, pass: &str, log: &(dyn Fn(&str) + Sync)) -> Result<Backchannel> {
        let (url, user, pass, host, port) = split_url(raw_url, user, pass)?;
        let stream = timeout(Duration::from_secs(8), TcpStream::connect((host.as_str(), port)))
            .await
            .map_err(|_| anyhow!("timed out connecting to {host}:{port}"))?
            .with_context(|| format!("cannot connect to {host}:{port}"))?;
        stream.set_nodelay(true).ok();
        let mut hs = Handshake { stream, buf: Vec::new(), cseq: 0, auth: None, user: user.clone(), pass: pass.clone(), session: None };

        let d = hs.call("DESCRIBE", &url, &[("Accept", "application/sdp".into())], log).await?;
        if d.status != 200 {
            bail!("DESCRIBE failed: {}", d.line);
        }
        let base = d.header("content-base").map(str::to_string).unwrap_or_else(|| url.clone());
        let track = parse_backchannel(&d.body, &base)?;
        log(&format!("backchannel track: control={} payload_type={} codec={}", track.control_url, track.payload_type, track.codec.name()));

        let s = hs.call("SETUP", &track.control_url, &[("Transport", "RTP/AVP/TCP;unicast;interleaved=0-1".into())], log).await?;
        if s.status != 200 {
            bail!("SETUP failed: {}", s.line);
        }
        let session = s.header("session").map(|v| v.split(';').next().unwrap_or("").trim().to_string()).filter(|v| !v.is_empty());
        let transport = s.header("transport").unwrap_or("").to_string();
        log(&format!("  Session: {}  Transport: {}", session.as_deref().unwrap_or("(none)"), transport));
        let channel = transport
            .split(';')
            .find_map(|p| p.trim().strip_prefix("interleaved="))
            .and_then(|r| r.split('-').next())
            .and_then(|c| c.parse().ok())
            .unwrap_or(0u8);
        hs.session = session.clone();

        let p = hs.call("PLAY", &base, &[("Range", "npt=0.000-".into())], log).await?;
        if p.status != 200 {
            bail!("PLAY failed: {}", p.line);
        }

        let Handshake { stream, cseq, auth, .. } = hs;
        let (rd, wr) = stream.into_split();
        let (tx, statuses) = mpsc::unbounded_channel();
        Ok(Backchannel {
            wr,
            reader: tokio::spawn(drain(rd, tx)),
            statuses,
            codec: track.codec,
            session: session.unwrap_or_default(),
            packets: 0,
            channel,
            payload_type: track.payload_type,
            seq: rand::random(),
            timestamp: rand::random(),
            ssrc: rand::random(),
            cseq,
            url: base,
            auth,
            user,
            pass,
            last_keepalive: Instant::now(),
        })
    }

    /// Send one 20 ms G.711 payload (160 bytes).
    pub async fn send_packet(&mut self, payload: &[u8]) -> Result<()> {
        let rtp = rtp_packet(self.payload_type, self.seq, self.timestamp, self.ssrc, payload);
        self.wr.write_all(&interleaved_frame(self.channel, &rtp)).await.context("camera connection lost")?;
        self.seq = self.seq.wrapping_add(1);
        self.timestamp = self.timestamp.wrapping_add(payload.len() as u32);
        self.packets += 1;
        Ok(())
    }

    /// Fire-and-forget request on the streaming connection (the reply is
    /// consumed by the drain task).
    async fn simple_request(&mut self, method: &str) -> Result<()> {
        self.cseq += 1;
        let mut req = format!("{method} {} RTSP/1.0\r\nCSeq: {}\r\nUser-Agent: {USER_AGENT}\r\nRequire: {REQUIRE}\r\n", self.url, self.cseq);
        if !self.session.is_empty() {
            req.push_str(&format!("Session: {}\r\n", self.session));
        }
        if let Some(h) = authorization(&mut self.auth, &self.user, &self.pass, method, &self.url) {
            req.push_str(&format!("Authorization: {h}\r\n"));
        }
        req.push_str("\r\n");
        self.wr.write_all(req.as_bytes()).await.context("camera connection lost")?;
        Ok(())
    }

    /// Send a keepalive when one is due (every ~20 s) and notice a dead link.
    pub async fn keepalive_if_due(&mut self) -> Result<()> {
        if self.last_keepalive.elapsed() >= KEEPALIVE_EVERY {
            self.last_keepalive = Instant::now();
            self.simple_request("OPTIONS").await?;
        }
        while self.statuses.try_recv().is_ok() {}
        if self.reader.is_finished() {
            bail!("camera closed the connection");
        }
        Ok(())
    }

    /// TEARDOWN; returns the camera's status line if it answered.
    pub async fn close(mut self) -> Option<String> {
        while self.statuses.try_recv().is_ok() {}
        let line = if self.simple_request("TEARDOWN").await.is_ok() { timeout(Duration::from_secs(2), self.statuses.recv()).await.ok().flatten() } else { None };
        let _ = self.wr.shutdown().await;
        self.reader.abort();
        line
    }
}

// ------------------------------------------------------------- talk-test

/// CLI helper: connect, stream silence (or a WAV file) for `seconds`, tear down.
pub async fn run_test(url: &str, user: &str, pass: &str, seconds: u64, wav: Option<&str>) -> Result<()> {
    let log = |m: &str| println!("{m}");
    let mut bc = Backchannel::open(url, user, pass, &log).await?;
    println!("streaming {} for {seconds} s ({})", bc.codec.name(), if wav.is_some() { "WAV file" } else { "silence" });
    // Optional WAV source, fed 20 ms of input per tick; silence after it ends.
    let mut source = match wav {
        Some(path) => {
            let (rate, samples) = read_wav(&std::fs::read(path).with_context(|| format!("cannot read {path}"))?)?;
            let bytes: Vec<u8> = samples.iter().flat_map(|s| s.to_le_bytes()).collect();
            Some((Packetizer::new(bc.codec, rate), bytes, (rate as usize / 50) * 2))
        }
        None => None,
    };
    let silence = vec![bc.codec.silence(); SAMPLES_PER_PACKET];
    let mut tick = interval(PACKET_INTERVAL);
    tick.set_missed_tick_behavior(MissedTickBehavior::Delay);
    let mut result = Ok(());
    let mut offset = 0;
    for _ in 0..seconds * 50 {
        tick.tick().await;
        let pkt = match source.as_mut() {
            Some((pk, bytes, step)) => {
                let end = (offset + *step).min(bytes.len());
                pk.push_pcm(&bytes[offset..end]);
                offset = end;
                pk.pop_packet().unwrap_or_else(|| silence.clone())
            }
            None => silence.clone(),
        };
        if let Err(e) = bc.send_packet(&pkt).await {
            result = Err(e);
            break;
        }
        if let Err(e) = bc.keepalive_if_due().await {
            result = Err(e);
            break;
        }
    }
    println!("packets sent: {}", bc.packets);
    match bc.close().await {
        Some(l) => println!("TEARDOWN -> {l}"),
        None => println!("TEARDOWN sent (no reply seen)"),
    }
    result
}

// ------------------------------------------------------------- WebSocket

fn busy() -> &'static Mutex<HashSet<String>> {
    static BUSY: OnceLock<Mutex<HashSet<String>>> = OnceLock::new();
    BUSY.get_or_init(Default::default)
}

/// Holds the "one talker per camera" claim until dropped.
struct TalkSlot(String);

impl TalkSlot {
    fn claim(camera: &str) -> Option<TalkSlot> {
        busy().lock().unwrap().insert(camera.to_string()).then(|| TalkSlot(camera.to_string()))
    }
}

impl Drop for TalkSlot {
    fn drop(&mut self) {
        busy().lock().unwrap().remove(&self.0);
    }
}

async fn close_with(mut ws: WebSocket, code: u16, reason: &str) {
    let reason: String = reason.chars().take(100).collect();
    let _ = ws.send(Message::Close(Some(CloseFrame { code, reason: reason.into() }))).await;
}

/// `GET /api/cameras/:id/talk?rate=48000` (WebSocket upgrade). Admins, and operators with the talk right.
#[allow(clippy::too_many_arguments)]
pub async fn handle(
    engine: Arc<Engine>,
    id: usize,
    access: crate::web::Access,
    caller: crate::web::Caller,
    allowed: bool,
    ip: String,
    rate: Option<u32>,
    ws: WebSocketUpgrade,
) -> Response {
    // The session's own rights (a session can have fewer than its account,
    // e.g. one opened from a linked server).
    let _ = &caller;
    if !allowed {
        return (StatusCode::FORBIDDEN, "this account may not talk through cameras").into_response();
    }
    let Some(handle) = engine.camera(id) else {
        return (StatusCode::NOT_FOUND, "no such camera").into_response();
    };
    let (name, url, user, pass) = {
        let c = handle.cfg.read().unwrap();
        (c.name.clone(), c.url.clone(), c.username.clone(), c.password.clone())
    };
    if !access.can_name(&name) {
        return (StatusCode::NOT_FOUND, "not found").into_response();
    }
    let rate = rate.unwrap_or(48000);
    if !(8000..=96000).contains(&rate) {
        return (StatusCode::BAD_REQUEST, "rate must be between 8000 and 96000").into_response();
    }
    let slot = TalkSlot::claim(&name);
    // (A GET, so the sign-in layer does not log it: talking is logged here.)
    if slot.is_some() {
        let _ = engine.db.insert_audit(&caller.0, &ip, "talk through the camera's speaker", &name, 200);
    }
    let who = Some((engine.clone(), caller.0.clone(), caller.1));
    ws.on_upgrade(move |socket| crate::web::while_account_allowed(who, async move {
        match slot {
            Some(slot) => talk_session(socket, slot, name, url, user, pass, rate, caller.0).await,
            None => close_with(socket, 4409, "another talker is already connected to this camera").await,
        }
    }))
}

#[allow(clippy::too_many_arguments)]
async fn talk_session(mut socket: WebSocket, _slot: TalkSlot, name: String, url: String, user: String, pass: String, rate: u32, who: String) {
    let log = |m: &str| tracing::info!("talk {name}: {m}");
    let mut bc = match Backchannel::open(&url, &user, &pass, &log).await {
        Ok(bc) => bc,
        Err(e) => {
            tracing::warn!("talk {name}: backchannel failed: {e:#}");
            close_with(socket, 1011, &format!("{e:#}")).await;
            return;
        }
    };
    tracing::info!("talk {name}: backchannel opened for {who} ({}, session {}, input {rate} Hz)", bc.codec.name(), bc.session);
    let mut pk = Packetizer::new(bc.codec, rate);
    let mut tick = interval(PACKET_INTERVAL);
    tick.set_missed_tick_behavior(MissedTickBehavior::Skip);
    let (reason, client_gone) = loop {
        tokio::select! {
            msg = socket.recv() => match msg {
                Some(Ok(Message::Binary(b))) => pk.push_pcm(&b),
                Some(Ok(Message::Close(_))) | None => break ("client closed".to_string(), true),
                Some(Ok(_)) => {}
                Some(Err(e)) => break (format!("socket error: {e}"), true),
            },
            _ = tick.tick() => {
                if let Some(p) = pk.pop_packet() {
                    if let Err(e) = bc.send_packet(&p).await {
                        break (format!("camera error: {e:#}"), false);
                    }
                }
                if let Err(e) = bc.keepalive_if_due().await {
                    break (format!("camera error: {e:#}"), false);
                }
            }
        }
    };
    let (packets, dropped) = (bc.packets, pk.dropped);
    let td = bc.close().await;
    tracing::info!("talk {name}: closed ({reason}); {packets} packets sent, {dropped} samples dropped, TEARDOWN {}", td.as_deref().unwrap_or("no reply"));
    if client_gone {
        // Reading on lets the websocket layer flush its reply to the client's
        // close frame before the socket is dropped.
        let _ = timeout(Duration::from_secs(1), socket.recv()).await;
    } else {
        close_with(socket, 1011, &reason).await;
    }
}

// ----------------------------------------------------------------- tests

#[cfg(test)]
mod tests {
    use super::*;

    const SDP: &str = "v=0\r\ns=Session\r\nm=video 0 RTP/AVP 96\r\na=control:track1\r\na=recvonly\r\na=rtpmap:96 H264/90000\r\n\
m=audio 0 RTP/AVP 97\r\na=control:track2\r\na=recvonly\r\na=rtpmap:97 MPEG4-GENERIC/16000/1\r\n\
m=audio 0 RTP/AVP 0\r\na=control:track3\r\na=rtpmap:0 PCMU/8000\r\na=sendonly\r\n";

    #[test]
    fn sdp_relative_control_with_base() {
        let t = parse_backchannel(SDP, "rtsp://cam:554/Preview_02_main/").unwrap();
        assert_eq!(t, Track { control_url: "rtsp://cam:554/Preview_02_main/track3".into(), payload_type: 0, codec: Codec::Pcmu });
        let t = parse_backchannel(SDP, "rtsp://cam:554/Preview_02_main").unwrap();
        assert_eq!(t.control_url, "rtsp://cam:554/Preview_02_main/track3");
    }

    #[test]
    fn sdp_absolute_control_and_alaw() {
        let sdp = "m=audio 0 RTP/AVP 8\r\na=control:rtsp://cam/back\r\na=sendonly\r\n";
        let t = parse_backchannel(sdp, "rtsp://cam/x").unwrap();
        assert_eq!(t.control_url, "rtsp://cam/back");
        assert_eq!(t.codec, Codec::Pcma);
        assert_eq!(t.payload_type, 8);
    }

    #[test]
    fn sdp_refusals() {
        let none = "m=audio 0 RTP/AVP 97\r\na=rtpmap:97 MPEG4-GENERIC/16000/1\r\na=recvonly\r\n";
        assert!(parse_backchannel(none, "rtsp://c/").unwrap_err().to_string().contains("no audio backchannel"));
        let other = "m=audio 0 RTP/AVP 97\r\na=rtpmap:97 OPUS/48000/2\r\na=sendonly\r\n";
        let e = parse_backchannel(other, "rtsp://c/").unwrap_err().to_string();
        assert!(e.contains("not supported") && e.contains("OPUS"), "{e}");
    }

    #[test]
    fn g711_known_values() {
        assert_eq!(linear_to_ulaw(0), 0xFF);
        assert_eq!(linear_to_ulaw(-1), 0x7F);
        assert_eq!(linear_to_ulaw(32767), 0x80);
        assert_eq!(linear_to_ulaw(-32768), 0x00);
        assert_eq!(linear_to_alaw(0), 0xD5);
        assert_eq!(linear_to_alaw(-1), 0x55);
        assert_eq!(linear_to_alaw(32767), 0xAA);
        assert_eq!(linear_to_alaw(-32768), 0x2A);
        assert_eq!(Codec::Pcmu.silence(), 0xFF);
        assert_eq!(Codec::Pcma.silence(), 0xD5);
    }

    #[test]
    fn resampler_lengths() {
        let mut out = Vec::new();
        Resampler::new(48000).push(&vec![1000i16; 4800], &mut out);
        assert_eq!(out.len(), 800);
        assert!(out.iter().all(|&s| s == 1000));
        let mut out = Vec::new();
        Resampler::new(44100).push(&vec![0i16; 4410], &mut out);
        assert!((out.len() as i64 - 800).abs() <= 1, "{}", out.len());
        let mut out = Vec::new();
        Resampler::new(8000).push(&[1, 2, 3], &mut out);
        assert_eq!(out, vec![1, 2, 3]);
        let mut out = Vec::new();
        Resampler::new(16000).push(&[0, 100, 200, 400], &mut out);
        assert_eq!(out, vec![50, 300]);
    }

    #[test]
    fn packetizer_packets_and_limit() {
        let mut p = Packetizer::new(Codec::Pcmu, 48000);
        // 20 ms at 48 kHz = 960 samples = 1920 bytes, split at an odd byte
        let pcm = vec![0u8; 1920];
        p.push_pcm(&pcm[..1001]);
        p.push_pcm(&pcm[1001..]);
        let pkt = p.pop_packet().unwrap();
        assert_eq!(pkt.len(), 160);
        assert!(pkt.iter().all(|&b| b == 0xFF));
        assert!(p.pop_packet().is_none());
        // a huge burst is trimmed to bound latency
        p.push_pcm(&vec![0u8; 48000 * 2]);
        assert!(p.dropped > 0);
        let mut n = 0;
        while p.pop_packet().is_some() {
            n += 1;
        }
        assert!(n <= 3);
    }

    #[test]
    fn rtp_and_framing_bytes() {
        let p = rtp_packet(0, 0x0102, 0x03040506, 0x0708090A, &[0xFF, 0xFE]);
        assert_eq!(p, vec![0x80, 0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 0xFF, 0xFE]);
        let f = interleaved_frame(0, &p);
        assert_eq!(&f[..4], &[b'$', 0, 0, 14]);
        assert_eq!(&f[4..], &p[..]);
    }

    #[test]
    fn digest_known_answer() {
        // RFC 2617 section 3.5 example
        let r = digest_response("Mufasa", "Circle Of Life", "testrealm@host.com", "dcd98b7102dd2f0e8b11d0f600bfb0c093", "GET", "/dir/index.html", Some(("00000001", "0a4f113b")));
        assert_eq!(r, "6629fae49393a05397450978507c4ef1");
    }

    #[test]
    fn challenge_parsing() {
        let a = parse_challenges(&["Digest realm=\"IPC\", nonce=\"abc123\", qop=\"auth\", opaque=\"zz\""]).unwrap();
        match a {
            Auth::Digest { realm, nonce, opaque, qop, .. } => {
                assert_eq!((realm.as_str(), nonce.as_str(), opaque.as_deref(), qop), ("IPC", "abc123", Some("zz"), true));
            }
            _ => panic!("expected digest"),
        }
        assert!(matches!(parse_challenges(&["Basic realm=\"x\""]), Some(Auth::Basic)));
        assert!(matches!(parse_challenges(&["Basic realm=\"x\"", "Digest realm=\"x\", nonce=\"n\""]), Some(Auth::Digest { .. })));
    }

    #[test]
    fn url_splitting() {
        let (u, user, pass, host, port) = split_url("rtsp://adm%40:p%3Aw@10.0.0.5/a/b?x=1", "", "").unwrap();
        assert_eq!((u.as_str(), user.as_str(), pass.as_str(), host.as_str(), port), ("rtsp://10.0.0.5:554/a/b?x=1", "adm@", "p:w", "10.0.0.5", 554));
        let (_, user, pass, ..) = split_url("rtsp://u:p@h:8554/x", "cfg", "secret").unwrap();
        assert_eq!((user.as_str(), pass.as_str()), ("cfg", "secret"));
    }

    #[test]
    fn wav_reader() {
        let mut w = b"RIFF\0\0\0\0WAVEfmt ".to_vec();
        w.extend_from_slice(&16u32.to_le_bytes());
        w.extend_from_slice(&[1, 0, 1, 0]);
        w.extend_from_slice(&8000u32.to_le_bytes());
        w.extend_from_slice(&16000u32.to_le_bytes());
        w.extend_from_slice(&[2, 0, 16, 0]);
        w.extend_from_slice(b"data");
        w.extend_from_slice(&4u32.to_le_bytes());
        w.extend_from_slice(&[1, 0, 2, 0]);
        assert_eq!(read_wav(&w).unwrap(), (8000, vec![1, 2]));
    }
}
