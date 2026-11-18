//! Live video in Apple Home: the SetupEndpoints and
//! SelectedRTPStreamConfiguration messages (TLV8, HAP chapter 11), and the
//! ffmpeg process that sends the camera to the controller over SRTP.
//!
//! The video is always encoded (H.264 at the size, frame rate, bit rate and
//! profile Home asks for), with the encoder the transcode module picked at
//! start (graphics card first, libx264 last) and under its limits. Copying
//! the camera's own stream is not done: Home needs its own size, bit rate,
//! packet size and keyframes, and a copied stream would break whenever the
//! camera differs. The source is the camera's live stream inside AmberSight
//! (the same fragments the live view uses), so the camera gets no new
//! connection. Sound: Opus (libopus), when the camera sends sound.
//!
//! Defences: RTP goes only to the address the controller gave, and that must
//! be the address of the HAP session itself (a controller cannot point video
//! at someone else). Few streams at once (per camera and in all), few
//! prepared sessions, and every stream stops when its controller's
//! connection closes, when it says end, or when no RTCP report from it
//! arrives for 30 s (see `Watch`: SRTCP under the session's key, each packet
//! once, or plain reports from its address when it sends no SRTCP; never
//! our own reports sent back).
//! The SRTP keys are never logged (they are on ffmpeg's command line, which
//! other accounts on the same computer can read, as for any ffmpeg use).

use std::collections::HashMap;
use std::io::{BufRead, BufReader, Write};
use std::net::{IpAddr, UdpSocket};
use std::process::Stdio;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use base64::Engine as _;
use rand::Rng;
use zeroize::Zeroize;

use super::server::status;
use super::tlv::{Builder, Tlv};
use crate::ingest::live::{LiveHub, LiveMsg};
use crate::transcode::{Encoder, Hw, OutCodec, Target};

/// The SRTP crypto suite (the only one offered and taken).
pub const SUITE_AES128: u8 = 0;

/// Streams of one camera at once, and of all HomeKit cameras.
pub const PER_CAMERA: usize = 2;
pub const MAX_STREAMS: usize = 4;
/// Prepared (SetupEndpoints) but not started sessions: per connection, in
/// all, and how long one waits for its start.
const PENDING_PER_CONN: usize = 2;
const PENDING_TOTAL: usize = 16;
const PENDING_TTL: Duration = Duration::from_secs(60);
/// A stream with no RTCP from the controller for this long stops.
pub const RTCP_TIMEOUT: Duration = Duration::from_secs(30);
/// An encode that has made no frame in this long failed.
const FIRST_FRAME: Duration = Duration::from_secs(12);
/// The RTP sequence numbers of a session's ffmpeg runs (all with Home's one
/// SRTP key and SSRC) start this far apart: a run that sent no frame (the
/// only kind followed by another) sent far fewer packets than this (sound
/// for at most `FIRST_FRAME`, 50 a second), so no two runs use the same
/// SRTP packet index, which with the same key and SSRC would be the same
/// AES keystream: the RTP (the video and sound) is protected. 8 runs at most
/// (the first starts below 0x1000), all below 65536 before they count up.
/// Not covered: ffmpeg's SRTCP index starts at 0 in every run, and Home
/// fixes our SSRC in the SetupEndpoints answer, so a retried run's sender
/// reports may repeat SRTCP indexes of the failed run's few ones. Those
/// carry only timestamps and packet counts: negligible.
const SEQ_STEP: u16 = 0x2000;
const MAX_RUNS: u16 = 8;

static ACTIVE: AtomicUsize = AtomicUsize::new(0);

/// The UDP ports RTCP comes back to (`[homekit] rtp_ports`).
static RTP_PORTS: Mutex<(u16, u16)> = Mutex::new((51900, 51999));

pub fn set_rtp_ports(r: (u16, u16)) {
    *RTP_PORTS.lock().unwrap() = r;
}

/// Two free UDP ports from the range (any address of this family), from a
/// random start so streams do not all try the same ones first.
fn bind_pair(any: IpAddr) -> Option<[UdpSocket; 2]> {
    let (a, b) = *RTP_PORTS.lock().unwrap();
    let n = (b - a) as u32 + 1;
    let start = rand::thread_rng().gen_range(0..n);
    let mut got: Vec<UdpSocket> = Vec::new();
    for i in 0..n {
        let port = a + ((start + i) % n) as u16;
        if let Ok(s) = UdpSocket::bind((any, port)) {
            if s.set_read_timeout(Some(Duration::from_millis(250))).is_ok() {
                got.push(s);
                if got.len() == 2 {
                    let b = got.pop()?;
                    let a = got.pop()?;
                    return Some([a, b]);
                }
            }
        }
    }
    None
}

/// Streams running now (all HomeKit accessories).
pub fn active() -> usize {
    ACTIVE.load(Ordering::SeqCst)
}

/// SRTP parameters of one direction.
#[derive(Clone)]
pub struct Srtp {
    pub suite: u8,
    pub key: Vec<u8>,
    pub salt: Vec<u8>,
}

impl Drop for Srtp {
    fn drop(&mut self) {
        self.key.zeroize();
        self.salt.zeroize();
    }
}

impl Srtp {
    fn parse(b: &[u8]) -> Option<Srtp> {
        let t = Tlv::decode(b)?;
        let suite = t.byte(1)?;
        let (key, salt) = (t.get(2).unwrap_or(&[]).to_vec(), t.get(3).unwrap_or(&[]).to_vec());
        // Only the suite we offer (SupportedRTPConfiguration): never unencrypted.
        (suite == SUITE_AES128 && key.len() == 16 && salt.len() == 14).then_some(Srtp { suite, key, salt })
    }

    fn tlv(&self) -> Vec<u8> {
        Builder::new().u8(1, self.suite).bytes(2, &self.key).bytes(3, &self.salt).done()
    }

    /// ffmpeg's -srtp_out_params: base64 of key then salt.
    fn ffmpeg_params(&self) -> String {
        let mut b = self.key.clone();
        b.extend_from_slice(&self.salt);
        let s = base64::engine::general_purpose::STANDARD.encode(&b);
        b.zeroize();
        s
    }
}

/// What a controller's SetupEndpoints write says.
pub struct Endpoints {
    pub session: [u8; 16],
    pub ip: IpAddr,
    pub video_port: u16,
    pub audio_port: u16,
    pub video: Srtp,
    pub audio: Srtp,
    /// For an IPv6 link-local address: the interface (the HAP connection's
    /// scope id), which SetupEndpoints itself does not carry.
    pub scope: u32,
}

fn u16le(b: Option<&[u8]>) -> Option<u16> {
    b.and_then(|v| <[u8; 2]>::try_from(v).ok()).map(u16::from_le_bytes)
}

fn address_tlv(ip: IpAddr, video: u16, audio: u16) -> Vec<u8> {
    Builder::new().u8(1, u8::from(ip.is_ipv6())).bytes(2, ip.to_string().as_bytes()).u16(3, video).u16(4, audio).done()
}

/// Parse SetupEndpoints; None for anything malformed or an SRTP suite we
/// do not send (only AES_CM_128_HMAC_SHA1_80).
pub fn parse_endpoints(b: &[u8]) -> Option<Endpoints> {
    let t = Tlv::decode(b)?;
    let session: [u8; 16] = t.get(1)?.try_into().ok()?;
    let a = Tlv::decode(t.get(3)?)?;
    let v6 = a.byte(1)? == 1;
    let ip: IpAddr = std::str::from_utf8(a.get(2)?).ok()?.trim().parse().ok()?;
    if ip.is_ipv6() != v6 {
        return None;
    }
    let (video_port, audio_port) = (u16le(a.get(3))?, u16le(a.get(4))?);
    if video_port == 0 || audio_port == 0 {
        return None;
    }
    Some(Endpoints { session, ip, video_port, audio_port, video: Srtp::parse(t.get(4)?)?, audio: Srtp::parse(t.get(5)?)?, scope: 0 })
}

/// The SetupEndpoints answer: our address and ports (where RTCP comes
/// back), the SRTP parameters we send with (the controller's, as HAP-NodeJS
/// and HAP-python do), and our SSRCs.
#[allow(clippy::too_many_arguments)]
pub fn endpoints_answer(session: &[u8; 16], local: IpAddr, video_port: u16, audio_port: u16, video: &Srtp, audio: &Srtp, video_ssrc: u32, audio_ssrc: u32) -> Vec<u8> {
    Builder::new()
        .bytes(1, session)
        .u8(2, 0)
        .bytes(3, &address_tlv(local, video_port, audio_port))
        .bytes(4, &video.tlv())
        .bytes(5, &audio.tlv())
        .bytes(6, &video_ssrc.to_le_bytes())
        .bytes(7, &audio_ssrc.to_le_bytes())
        .done()
}

/// SelectedRTPStreamConfiguration commands.
pub mod cmd {
    pub const END: u8 = 0;
    pub const START: u8 = 1;
    pub const SUSPEND: u8 = 2;
    pub const RESUME: u8 = 3;
    pub const RECONFIGURE: u8 = 4;
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct VideoSel {
    pub profile: u8,
    pub level: u8,
    pub width: u16,
    pub height: u16,
    pub fps: u8,
    pub pt: u8,
    pub max_kbps: u16,
    pub mtu: u16,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct AudioSel {
    pub codec: u8,
    pub channels: u8,
    /// 0 8 kHz, 1 16 kHz, 2 24 kHz.
    pub rate: u8,
    pub pt: u8,
    pub max_kbps: u16,
    pub packet_ms: u8,
}

impl AudioSel {
    pub fn hz(&self) -> u32 {
        match self.rate {
            0 => 8000,
            2 => 24000,
            _ => 16000,
        }
    }
}

#[derive(Debug, PartialEq)]
pub struct Selected {
    pub session: [u8; 16],
    pub command: u8,
    pub video: Option<VideoSel>,
    pub audio: Option<AudioSel>,
}

/// Parse SelectedRTPStreamConfiguration. A start needs H.264 video with its
/// size, rate, payload type and bit rate; a reconfigure may carry only the
/// size, rate and bit rate.
pub fn parse_selected(b: &[u8]) -> Option<Selected> {
    let t = Tlv::decode(b)?;
    let ctl = Tlv::decode(t.get(1)?)?;
    let session: [u8; 16] = ctl.get(1)?.try_into().ok()?;
    let command = ctl.byte(2)?;
    if command > cmd::RECONFIGURE {
        return None;
    }
    let video = match t.get(2) {
        Some(v) => {
            let v = Tlv::decode(v)?;
            if v.byte(1).is_some_and(|c| c != 0) {
                return None; // not H.264
            }
            let p = v.get(2).and_then(Tlv::decode).unwrap_or_default();
            let a = Tlv::decode(v.get(3)?)?;
            let r = v.get(4).and_then(Tlv::decode).unwrap_or_default();
            let (width, height, fps) = (u16le(a.get(1))?, u16le(a.get(2))?, a.byte(3)?);
            if !(64..=4096).contains(&width) || !(64..=4096).contains(&height) || !(1..=60).contains(&fps) {
                return None;
            }
            Some(VideoSel {
                profile: p.byte(1).unwrap_or(1).min(2),
                level: p.byte(2).unwrap_or(2).min(2),
                width,
                height,
                fps,
                pt: r.byte(1).unwrap_or(99),
                max_kbps: u16le(r.get(3)).unwrap_or(1000),
                mtu: u16le(r.get(5)).unwrap_or(1378).clamp(400, 1500),
            })
        }
        None => None,
    };
    let audio = match t.get(3) {
        Some(a) => {
            let a = Tlv::decode(a)?;
            let p = a.get(2).and_then(Tlv::decode).unwrap_or_default();
            let r = a.get(3).and_then(Tlv::decode).unwrap_or_default();
            Some(AudioSel {
                codec: a.byte(1)?,
                channels: p.byte(1).unwrap_or(1).clamp(1, 2),
                rate: p.byte(3).unwrap_or(1),
                pt: r.byte(1).unwrap_or(110),
                max_kbps: u16le(r.get(3)).unwrap_or(24).clamp(8, 64),
                packet_ms: p.byte(4).unwrap_or(20),
            })
        }
        None => None,
    };
    if command == cmd::START && video.is_none() {
        return None;
    }
    // RTP payload types are 7 bits.
    if video.is_some_and(|v| v.pt > 127) || audio.is_some_and(|a| a.pt > 127) {
        return None;
    }
    Some(Selected { session, command, video, audio })
}

/// Hold a start to what was offered: the largest offered size inside the
/// one asked for (else the smallest offered), its frame rate at most, and
/// 100 to 6000 kbps.
pub fn clamp_video(v: VideoSel) -> VideoSel {
    let offered = &super::accessories::OFFERED_SIZES;
    let (w, h, fps) = offered
        .iter()
        .filter(|(w, h, _)| *w <= v.width && *h <= v.height)
        .max_by_key(|(w, h, _)| *w as u32 * *h as u32)
        .or_else(|| offered.iter().min_by_key(|(w, h, _)| *w as u32 * *h as u32))
        .copied()
        .unwrap_or((640, 360, 30));
    VideoSel { width: w, height: h, fps: v.fps.clamp(1, fps), max_kbps: v.max_kbps.clamp(100, 6000), ..v }
}

/// Where a stream's picture comes from.
#[derive(Clone)]
pub enum Feed {
    /// The camera's live stream inside AmberSight (built-in reader): its
    /// fragments go to ffmpeg's stdin, so the camera gets no new connection.
    Hub(Arc<LiveHub>),
    /// A camera read by ffmpeg (burned-in text, a capture source, ...):
    /// ffmpeg reads it itself with these input arguments and draws `picture`
    /// (the text the recordings get, `engine::recording_filters`; empty when
    /// none) before scaling.
    Direct { input: Vec<String>, picture: String },
}

/// A camera's stream to send from (picked by the host).
#[derive(Clone)]
pub struct Source {
    pub feed: Feed,
    pub fps: f64,
    pub audio: bool,
    /// What the picture looks like (`picture_fingerprint`): a stream whose
    /// camera's fingerprint changes (its burned-in text or address changed) ends.
    pub fingerprint: String,
    /// The camera's name (for the log).
    pub name: String,
}

/// The fingerprint of what a camera's stream shows: its burned-in text
/// (the recording filters), whether it is read by ffmpeg, and its addresses.
/// (Hashed: no address with a login is kept around.)
pub fn picture_fingerprint(picture: &str, native: bool, url: &str, sub_url: Option<&str>) -> String {
    use sha2::{Digest, Sha256};
    let mut h = Sha256::new();
    for part in [picture, if native { "native" } else { "ffmpeg" }, url, sub_url.unwrap_or("")] {
        h.update(part.as_bytes());
        h.update([0u8]);
    }
    super::state::hex(&h.finalize()[..16])
}

/// ffmpeg's error output as it may be logged: logins in addresses and SRTP
/// keys ("inline:...") hidden.
pub fn scrub(text: &str) -> String {
    crate::engine::redact_text(text)
        .split(' ')
        .map(|t| match t.find("inline:") {
            Some(i) => format!("{}inline:***", &t[..i]),
            None => t.to_string(),
        })
        .collect::<Vec<_>>()
        .join(" ")
}

/// A prepared session: SetupEndpoints done, waiting for its start.
struct Pending {
    conn: u64,
    aid: u64,
    ep: Endpoints,
    socks: [UdpSocket; 2],
    ssrc: [u32; 2],
    made: Instant,
}

/// A running session's controls. One session is one stream to Home: when
/// it ends, for whatever reason, the session ends and Home sets up a new
/// one. Its SRTP key, salt and SSRC (the ones Home was told) are used by a
/// second ffmpeg only when the first sent no frame (another encoder takes
/// over), and then with RTP packet numbers far from the first one's (see
/// `SEQ_STEP`): the same packet number again would reuse the AES keystream.
struct Ctl {
    conn: u64,
    aid: u64,
    /// The camera's picture fingerprint when it started (unset while the
    /// start is still asking the camera for its stream).
    fingerprint: OnceLock<String>,
    stop: AtomicBool,
    child: Mutex<Option<std::process::Child>>,
}

impl Ctl {
    fn new(conn: u64, aid: u64) -> Ctl {
        Ctl { conn, aid, fingerprint: OnceLock::new(), stop: AtomicBool::new(false), child: Mutex::new(None) }
    }
    fn kill_child(&self) {
        if let Some(c) = self.child.lock().unwrap().as_mut() {
            let _ = c.kill();
        }
    }
    fn end(&self) {
        self.stop.store(true, Ordering::SeqCst);
        self.kill_child();
    }
}

/// A live stream's place under the limits: on the video encoder, and among
/// the streams of all accessories (`MAX_STREAMS`). Given back when dropped.
struct Place {
    encoders: Vec<Encoder>,
    slot: Option<crate::transcode::Slot>,
    software_after: bool,
    active: Release,
}

/// Runs its function once, when dropped.
struct Release(Option<Box<dyn FnOnce() + Send>>);

impl Drop for Release {
    fn drop(&mut self) {
        if let Some(f) = self.0.take() {
            f();
        }
    }
}

/// Takes a place for a live stream (BUSY when there is none; nothing is
/// asked of the camera here).
fn reserve_place() -> Result<Place, i64> {
    if !crate::transcode::probed() {
        // (The test encodes run once, shortly after start.)
        let _ = std::thread::spawn(|| crate::transcode::encoders_for(OutCodec::H264));
        return Err(status::BUSY);
    }
    let encoders = crate::transcode::encoders_for(OutCodec::H264).unwrap_or_default();
    let software_after = encoders.iter().any(|e| e.hw.is_none());
    let Some((encoders, slot)) = crate::transcode::reserve(&encoders) else {
        tracing::warn!("HomeKit: no place on the video encoder for a live stream (see [transcode] max_hardware / max_software)");
        return Err(status::BUSY);
    };
    // The global cap (the encoder slot is given back when refused).
    let mut n = ACTIVE.load(Ordering::SeqCst);
    loop {
        if n >= MAX_STREAMS {
            return Err(status::BUSY);
        }
        match ACTIVE.compare_exchange(n, n + 1, Ordering::SeqCst, Ordering::SeqCst) {
            Ok(_) => break,
            Err(now) => n = now,
        }
    }
    Ok(Place {
        encoders,
        slot: Some(slot),
        software_after,
        active: Release(Some(Box::new(|| {
            ACTIVE.fetch_sub(1, Ordering::SeqCst);
        }))),
    })
}

/// The streams of one HAP accessory server.
///
/// Locks: one, `tables`, for the prepared and running sessions and the
/// answers together (two locks taken in opposite orders by SetupEndpoints
/// and a start once deadlocked the whole server). Taken last of the HomeKit
/// locks (see the lock order in mod.rs); while it is held only a session's
/// own `Ctl::child` lock is taken (to kill its ffmpeg), and nothing slow is
/// done: the camera is asked for its stream, and ffmpeg started, without it.
#[derive(Default)]
pub struct Streams {
    tables: Mutex<Tables>,
}

#[derive(Default)]
struct Tables {
    pending: HashMap<[u8; 16], Pending>,
    /// Running sessions, and starts still asking the camera for its stream
    /// (they count against the limits from the moment they are taken).
    running: HashMap<[u8; 16], Arc<Ctl>>,
    /// The SetupEndpoints answer of each connection (and accessory): what
    /// that connection reads back, never another's.
    answers: HashMap<(u64, u64), Vec<u8>>,
}

fn random_ssrc() -> u32 {
    // (Positive: ffmpeg takes it as a signed number.)
    rand::thread_rng().gen_range(1..0x7fff_ffff)
}

/// An IPv4 address as itself, also when it came as ::ffff:a.b.c.d.
pub fn plain_ip(ip: IpAddr) -> IpAddr {
    match ip {
        IpAddr::V6(v6) => v6.to_ipv4_mapped().map(IpAddr::V4).unwrap_or(IpAddr::V6(v6)),
        v4 => v4,
    }
}

/// An IPv6 link-local address (fe80::/10): it needs the interface, which
/// SetupEndpoints does not carry (it is taken from the HAP connection).
pub fn link_local(ip: IpAddr) -> bool {
    matches!(ip, IpAddr::V6(v6) if v6.segments()[0] & 0xffc0 == 0xfe80)
}

impl Streams {
    /// Streams running for this accessory.
    pub fn count(&self, aid: u64) -> usize {
        Self::count_in(&self.tables.lock().unwrap().running, aid)
    }

    fn count_in(running: &HashMap<[u8; 16], Arc<Ctl>>, aid: u64) -> usize {
        running.values().filter(|c| c.aid == aid && !c.stop.load(Ordering::SeqCst)).count()
    }

    /// What connection `conn` reads back from this accessory's SetupEndpoints.
    pub fn answer(&self, conn: u64, aid: u64) -> Vec<u8> {
        self.tables.lock().unwrap().answers.get(&(conn, aid)).cloned().unwrap_or_default()
    }

    /// A SetupEndpoints write from connection `conn` (whose peer is `peer`,
    /// with `scope` its IPv6 scope id, reached on our `local` address): the
    /// answer to read back.
    #[allow(clippy::too_many_arguments)]
    pub fn setup(&self, conn: u64, aid: u64, peer: IpAddr, scope: u32, local: IpAddr, value: &[u8]) -> Result<Vec<u8>, i64> {
        let mut ep = parse_endpoints(value).ok_or(status::INVALID_VALUE)?;
        // (IpAddr carries no scope, so "fe80::1%en0" and "fe80::1" compare equal.)
        if plain_ip(ep.ip) != plain_ip(peer) {
            tracing::warn!("HomeKit: a controller at {} asked to send video to {}, not its own address; refused", plain_ip(peer), ep.ip);
            return Err(status::INVALID_VALUE);
        }
        if link_local(ep.ip) {
            // The video goes out on the interface the controller reached us on.
            if scope == 0 {
                tracing::warn!("HomeKit: a controller asked for live video to an IPv6 link-local address ({}) but its connection has no interface; refused", ep.ip);
                return Err(status::INVALID_VALUE);
            }
            ep.scope = scope;
        }
        let mut guard = self.tables.lock().unwrap();
        let t = &mut *guard;
        t.pending.retain(|_, p| p.made.elapsed() < PENDING_TTL);
        if t.pending.contains_key(&ep.session) || t.running.contains_key(&ep.session) {
            return Err(status::INVALID_VALUE);
        }
        if t.pending.len() >= PENDING_TOTAL || t.pending.values().filter(|p| p.conn == conn).count() >= PENDING_PER_CONN {
            return Err(status::OUT_OF_RESOURCES);
        }
        let local = plain_ip(local);
        let any: IpAddr = if local.is_ipv6() { IpAddr::from([0u16; 8]) } else { IpAddr::from([0u8; 4]) };
        let Some([v, a]) = bind_pair(any) else {
            tracing::warn!("HomeKit: no free UDP port for live video in [homekit] rtp_ports");
            return Err(status::OUT_OF_RESOURCES);
        };
        let (vp, ap) = (v.local_addr().map(|x| x.port()).unwrap_or(0), a.local_addr().map(|x| x.port()).unwrap_or(0));
        let ssrc = [random_ssrc(), random_ssrc()];
        let answer = endpoints_answer(&ep.session, local, vp, ap, &ep.video, &ep.audio, ssrc[0], ssrc[1]);
        t.pending.insert(ep.session, Pending { conn, aid, ep, socks: [v, a], ssrc, made: Instant::now() });
        t.answers.insert((conn, aid), answer.clone());
        Ok(answer)
    }

    /// A SelectedRTPStreamConfiguration write. `source`: the camera's
    /// stream for a size (asked only for a start). `muted`: the camera's
    /// Microphone is muted in Home (no sound is sent).
    pub fn select(&self, conn: u64, aid: u64, value: &[u8], muted: bool, source: &dyn Fn(u32, u32) -> Option<Source>) -> Result<(), i64> {
        let sel = parse_selected(value).ok_or(status::INVALID_VALUE)?;
        match sel.command {
            cmd::START => self.start(conn, aid, sel, muted, source),
            cmd::END | cmd::SUSPEND => {
                // (A suspended session is not kept: resuming it would need the
                // same SRTP state in a new ffmpeg. Home sets up a new one.)
                let mut t = self.tables.lock().unwrap();
                t.pending.retain(|k, p| !(*k == sel.session && p.conn == conn));
                match t.running.get(&sel.session) {
                    Some(c) if c.conn == conn => {
                        c.end();
                        t.running.remove(&sel.session);
                        Ok(())
                    }
                    Some(_) => Err(status::INSUFFICIENT_PRIVILEGES),
                    None => Ok(()),
                }
            }
            cmd::RESUME | cmd::RECONFIGURE => {
                let t = self.tables.lock().unwrap();
                match t.running.get(&sel.session) {
                    Some(c) if c.conn == conn && !c.stop.load(Ordering::SeqCst) => {
                        // Keeps running as it is (a new ffmpeg would reuse the session's SRTP state).
                        tracing::debug!("HomeKit: {} of a running live stream ignored", if sel.command == cmd::RESUME { "resume" } else { "reconfigure" });
                        Ok(())
                    }
                    // Ended (suspended, stopped): Home sets up a new session.
                    _ => Err(status::NOT_FOUND),
                }
            }
            _ => Err(status::INVALID_VALUE),
        }
    }

    fn start(&self, conn: u64, aid: u64, sel: Selected, muted: bool, source: &dyn Fn(u32, u32) -> Option<Source>) -> Result<(), i64> {
        self.start_with(conn, aid, sel, muted, source, &reserve_place)
    }

    /// A start, with its place under the limits taken by `reserve` first:
    /// the camera's stream is asked for (which wakes a battery camera) only
    /// once the stream will run, and the place is given back when there is
    /// no stream after all.
    fn start_with(&self, conn: u64, aid: u64, sel: Selected, muted: bool, source: &dyn Fn(u32, u32) -> Option<Source>, reserve: &dyn Fn() -> Result<Place, i64>) -> Result<(), i64> {
        let video = clamp_video(sel.video.ok_or(status::INVALID_VALUE)?);
        // The session is taken under the lock, from the count to the insert
        // (two starts at once cannot both take the camera's last place); the
        // rest is done without it, with the session already listed as running.
        let (p, ctl) = {
            let mut t = self.tables.lock().unwrap();
            match t.pending.get(&sel.session) {
                Some(p) if p.conn == conn && p.aid == aid => {}
                _ => return Err(status::INVALID_VALUE),
            }
            let p = t.pending.remove(&sel.session).unwrap();
            if Self::count_in(&t.running, aid) >= PER_CAMERA {
                return Err(status::BUSY);
            }
            let ctl = Arc::new(Ctl::new(conn, aid));
            t.running.insert(sel.session, ctl.clone());
            (p, ctl)
        };
        let given_up = |code: i64| {
            self.forget(&sel.session, &ctl);
            Err(code)
        };
        let place = match reserve() {
            Ok(place) => place,
            Err(code) => return given_up(code),
        };
        // (Dropping `place` on the way out gives it back.)
        let Some(src) = source(video.width as u32, video.height as u32) else {
            tracing::warn!("HomeKit: no live stream to send (the camera is offline, asleep, or read by ffmpeg)");
            return given_up(status::UNABLE_TO_COMMUNICATE);
        };
        let _ = ctl.fingerprint.set(src.fingerprint.clone());
        if ctl.stop.load(Ordering::SeqCst) {
            // Ended meanwhile (its connection closed, the camera was hidden, ...).
            return given_up(status::UNABLE_TO_COMMUNICATE);
        }
        let audio = sel.audio.filter(|a| a.codec == 3 && src.audio && !muted);
        let c2 = ctl.clone();
        let spawned = std::thread::Builder::new().name("homekit-stream".into()).spawn(move || {
            let Place { encoders, slot, software_after, active } = place;
            match slot {
                Some(slot) => run(c2, p, src, (video, audio), encoders, slot, software_after),
                // (Only tests make a place without an encoder slot.)
                None => c2.end(),
            }
            drop(active);
        });
        if spawned.is_err() {
            return given_up(status::OUT_OF_RESOURCES);
        }
        Ok(())
    }

    /// A start that did not happen: its session goes (only that one: the
    /// same id may have been set up again since).
    fn forget(&self, session: &[u8; 16], ctl: &Arc<Ctl>) {
        ctl.stop.store(true, Ordering::SeqCst);
        let mut t = self.tables.lock().unwrap();
        if t.running.get(session).is_some_and(|c| Arc::ptr_eq(c, ctl)) {
            t.running.remove(session);
        }
    }

    /// Drop finished sessions from the list.
    pub fn tidy(&self) {
        let mut t = self.tables.lock().unwrap();
        t.running.retain(|_, c| !c.stop.load(Ordering::SeqCst));
        t.pending.retain(|_, p| p.made.elapsed() < PENDING_TTL);
    }

    /// This accessory's camera now shows something else (its burned-in text
    /// changed, another address): its live views end, so none goes on with
    /// the old picture or address. Home sets up a new one.
    pub fn end_changed(&self, aid: u64, fingerprint: &str) {
        self.tables.lock().unwrap().running.retain(|_, c| {
            // (A start still asking the camera has no fingerprint yet: the
            // next look, half a second later, compares it.)
            let keep = c.aid != aid || c.fingerprint.get().is_none_or(|f| f == fingerprint);
            if !keep {
                tracing::info!("HomeKit: a camera's picture settings changed; its live view ended");
                c.end();
            }
            keep
        });
    }

    #[cfg(test)]
    pub fn fake_running(&self, aid: u64, fingerprint: &str) {
        let c = Arc::new(Ctl::new(1, aid));
        let _ = c.fingerprint.set(fingerprint.into());
        self.tables.lock().unwrap().running.insert(rand::random(), c);
    }

    /// The sessions of accessories not in this list end (bridged cameras
    /// hidden or removed).
    pub fn keep_only(&self, aids: &[u64]) {
        let mut t = self.tables.lock().unwrap();
        t.pending.retain(|_, p| aids.contains(&p.aid));
        t.answers.retain(|(_, a), _| aids.contains(a));
        t.running.retain(|_, c| {
            let keep = aids.contains(&c.aid);
            if !keep {
                c.end();
            }
            keep
        });
    }

    /// The controller's connection closed: its sessions end.
    pub fn end_conn(&self, conn: u64) {
        let mut t = self.tables.lock().unwrap();
        t.pending.retain(|_, p| p.conn != conn);
        t.answers.retain(|(c, _), _| *c != conn);
        t.running.retain(|_, c| {
            if c.conn == conn {
                c.end();
                false
            } else {
                true
            }
        });
    }

    /// Everything ends (HomeKit off, settings changed, reset).
    pub fn end_all(&self) {
        let mut t = self.tables.lock().unwrap();
        t.pending.clear();
        t.answers.clear();
        for (_, c) in t.running.drain() {
            c.end();
        }
    }
}

/// The ffmpeg arguments for one session: the camera's fragments on stdin
/// (or the camera itself, for `Feed::Direct`), video (and sound) to the
/// controller over SRTP; progress on stdout.
#[allow(clippy::too_many_arguments)]
pub fn ffmpeg_args(e: &Encoder, hw_decode: bool, v: &VideoSel, a: Option<&AudioSel>, ep: &Endpoints, ssrc: [u32; 2], seq: u16, src_fps: f64, feed: &Feed) -> Vec<String> {
    let s = |x: &[&str]| x.iter().map(|y| y.to_string()).collect::<Vec<String>>();
    let mut out = s(&["-hide_banner", "-loglevel", "error", "-nostats", "-progress", "pipe:1"]);
    // (Burned-in text is drawn on decoded frames in memory: never decoded on the card then.)
    let picture = match feed {
        Feed::Direct { picture, .. } => picture.as_str(),
        Feed::Hub(_) => "",
    };
    let hw_decode = hw_decode && picture.is_empty();
    out.extend(crate::transcode::device_args(e, hw_decode));
    match feed {
        Feed::Hub(_) => out.extend(s(&["-probesize", "65536", "-analyzeduration", "0", "-f", "mp4", "-i", "pipe:0"])),
        Feed::Direct { input, .. } => out.extend(input.iter().cloned()),
    }
    let fps = (v.fps as f64).min(if src_fps > 0.0 { src_fps.max(1.0) } else { 60.0 }).round().max(1.0) as u32;
    // The bit rate Home gives is a ceiling: aim lower (encoder_args lets peaks reach 1.5x).
    let t = Target { codec: OutCodec::H264, width: (v.width as u32) & !1, height: (v.height as u32) & !1, fps: Some(fps), bitrate_kbps: (v.max_kbps as u32 * 2 / 3).max(64) };
    out.extend(s(&["-map", "0:v:0", "-vf"]));
    let scale = crate::transcode::filters(e, hw_decode, &t, 8);
    out.push(if picture.is_empty() { scale } else { format!("{picture},{scale}") });
    out.extend(crate::transcode::encoder_args(e, &t, fps as f64, true));
    let profile = match (v.profile, e.hw) {
        (0, Some(Hw::Vaapi) | Some(Hw::Amf)) => "constrained_baseline",
        (0, _) => "baseline",
        (1, _) => "main",
        _ => "high",
    };
    out.extend(s(&["-profile:v", profile]));
    if e.hw.is_none() {
        out.extend(s(&["-level:v", ["3.1", "3.2", "4.0"][v.level.min(2) as usize]]));
    }
    out.push("-an".into());
    out.extend(rtp_out(v.pt, ssrc[0], seq, &ep.video, ep.ip, ep.scope, ep.video_port, v.mtu));
    if let Some(a) = a {
        out.extend(s(&["-map", "0:a:0", "-vn", "-c:a", "libopus", "-application", "lowdelay"]));
        // Opus frames of the packet time Home chose (20 ms is all we offer;
        // 40 and 60 are also Opus frame sizes, anything else gets 20). The RTP
        // clock of Opus is always 48 kHz (RFC 7587), whatever the sample rate;
        // ffmpeg's RTP muxer does that.
        let frame = match a.packet_ms {
            m @ (20 | 40 | 60) => m,
            _ => 20,
        };
        out.extend(s(&["-frame_duration", &frame.to_string(), "-ac", "1", "-ar", &a.hz().to_string(), "-b:a", &format!("{}k", a.max_kbps)]));
        out.extend(rtp_out(a.pt, ssrc[1], seq, &ep.audio, ep.ip, ep.scope, ep.audio_port, 188));
    }
    out
}

#[allow(clippy::too_many_arguments)]
fn rtp_out(pt: u8, ssrc: u32, seq: u16, srtp: &Srtp, ip: IpAddr, scope: u32, port: u16, mtu: u16) -> Vec<String> {
    // A link-local target carries its interface as a numeric zone
    // ("[fe80::1%2]"): getaddrinfo takes that on Linux, macOS and Windows,
    // and ffmpeg passes it through (tested: ffmpeg 8 on Linux).
    let host = match ip {
        IpAddr::V6(v6) if scope != 0 && link_local(ip) => format!("[{v6}%{scope}]"),
        IpAddr::V6(v6) => format!("[{v6}]"),
        v4 => v4.to_string(),
    };
    // (Always SRTP: SetupEndpoints takes nothing else.)
    let mut v: Vec<String> = vec!["-payload_type".into(), pt.to_string(), "-ssrc".into(), (ssrc as i32).to_string(), "-seq".into(), seq.to_string(), "-f".into(), "rtp".into()];
    v.extend(["-srtp_out_suite".to_string(), "AES_CM_128_HMAC_SHA1_80".to_string(), "-srtp_out_params".to_string(), srtp.ffmpeg_params()]);
    v.push(format!("srtp://{host}:{port}?rtcpport={port}&pkt_size={mtu}"));
    v
}

/// SRTCP's message authentication (RFC 3711, AES_CM_128_HMAC_SHA1_80) under
/// one master key and salt: the RTCP that counts as the controller being
/// there is RTCP only it can make.
struct SrtcpAuth {
    key: ring::hmac::Key,
    /// The highest SRTCP index taken (an older packet sent again is not new).
    last: Option<u32>,
}

/// RFC 3711's key derivation (4.3.1, key derivation rate 0): `out.len()`
/// bytes of AES-CM keystream under the master key, from the master salt
/// with `label` in its 8th byte.
fn srtp_kdf(master_key: &[u8], master_salt: &[u8], label: u8, out: &mut [u8]) -> Option<()> {
    use aes::cipher::{BlockEncrypt, KeyInit};
    let aes = aes::Aes128::new_from_slice(master_key).ok()?;
    if master_salt.len() != 14 {
        return None;
    }
    for (i, chunk) in out.chunks_mut(16).enumerate() {
        let mut block = [0u8; 16];
        block[..14].copy_from_slice(master_salt);
        block[7] ^= label;
        block[14..].copy_from_slice(&(i as u16).to_be_bytes());
        let mut b = block.into();
        aes.encrypt_block(&mut b);
        chunk.copy_from_slice(&b[..chunk.len()]);
        block.zeroize();
    }
    Some(())
}

impl SrtcpAuth {
    fn new(srtp: &Srtp) -> Option<SrtcpAuth> {
        // (Label 4: SRTCP's authentication key, 160 bits.)
        let mut k = [0u8; 20];
        srtp_kdf(&srtp.key, &srtp.salt, 4, &mut k)?;
        let key = ring::hmac::Key::new(ring::hmac::HMAC_SHA1_FOR_LEGACY_USE_ONLY, &k);
        k.zeroize();
        Some(SrtcpAuth { key, last: None })
    }

    /// Whether `pkt` is a new SRTCP packet under this key: its 80-bit tag is
    /// right and its index is above every one taken before.
    fn take(&mut self, pkt: &[u8]) -> bool {
        use subtle::ConstantTimeEq;
        // Header and sender SSRC, the E flag and index, the tag.
        if pkt.len() < 8 + 4 + 10 || pkt[0] >> 6 != 2 {
            return false;
        }
        let (signed, tag) = pkt.split_at(pkt.len() - 10);
        let mac = ring::hmac::sign(&self.key, signed);
        if !bool::from(mac.as_ref()[..10].ct_eq(tag)) {
            return false;
        }
        let e_index = &signed[signed.len() - 4..];
        let index = u32::from_be_bytes([e_index[0], e_index[1], e_index[2], e_index[3]]) & 0x7fff_ffff;
        if self.last.is_some_and(|l| index <= l) {
            return false;
        }
        self.last = Some(index);
        true
    }
}

/// A stream that has had no SRTCP under its keys after this long, but RTCP
/// reports from the controller's address, takes those instead (`Watch`).
const UNCHECKED_AFTER: Duration = Duration::from_secs(10);
/// Packets read from one socket before the other gets its turn.
const DRAIN_MAX: usize = 4096;

/// What a stream's watcher has to say once.
#[derive(Debug, PartialEq)]
enum Note {
    /// A report under the session's keys came: they are checked.
    Checked,
    /// None did in `UNCHECKED_AFTER`, but unchecked ones from the
    /// controller's address did: those count from now on.
    Unchecked,
}

/// Whether the controller is still there: the RTCP reports it sends to our
/// two ports. Counted: receiver reports (and the other RTCP kinds but sender
/// reports: Home sends no media, so it sends none) from the controller's
/// address, never with one of our SSRCs (our own ffmpeg's reports sent back
/// to us), and SRTCP under the session's keys (the video's or the sound's,
/// on either port), each packet once. A stream that got none of those in
/// its first `UNCHECKED_AFTER` but got reports from the controller's address
/// all the same (a controller whose RTCP is not SRTCP under the keys it
/// gave, if there is one) counts those from then on; ours sent back still
/// never count.
struct Watch {
    peer: IpAddr,
    ours: [u32; 2],
    auth: Vec<SrtcpAuth>,
    start: Instant,
    /// The last report that counted.
    last: Instant,
    /// The last one from the controller's address that would count unchecked.
    last_plain: Option<Instant>,
    checked: bool,
    unchecked: bool,
    /// Authentication tags computed (for the tests: junk costs none).
    #[cfg_attr(not(test), allow(dead_code))]
    macs: u64,
}

impl Watch {
    fn new(peer: IpAddr, ours: [u32; 2], auth: Vec<SrtcpAuth>, now: Instant) -> Watch {
        Watch { peer: plain_ip(peer), ours, auth, start: now, last: now, last_plain: None, checked: false, unchecked: false, macs: 0 }
    }

    /// Whether `pkt` looks like an RTCP report the controller could have
    /// sent: version 2, an RTCP packet type (200-204) but no sender report
    /// (200), a first packet that fits, and a sender SSRC that is not ours.
    /// (The first 8 bytes of SRTCP are not encrypted.)
    fn report_shaped(&self, pkt: &[u8]) -> bool {
        if pkt.len() < 8 || pkt[0] >> 6 != 2 || !(201..=204).contains(&pkt[1]) {
            return false;
        }
        let words = u16::from_be_bytes([pkt[2], pkt[3]]) as usize;
        let ssrc = u32::from_be_bytes([pkt[4], pkt[5], pkt[6], pkt[7]]);
        (words + 1) * 4 <= pkt.len() && !self.ours.contains(&ssrc)
    }

    /// One packet to our ports.
    fn packet(&mut self, from: IpAddr, pkt: &[u8], now: Instant) -> Option<Note> {
        if plain_ip(from) != self.peer || !self.report_shaped(pkt) {
            return None;
        }
        self.last_plain = Some(now);
        self.macs += 1;
        if self.auth.iter_mut().any(|a| a.take(pkt)) {
            self.last = now;
            if !self.checked {
                self.checked = true;
                return Some(Note::Checked);
            }
        } else if self.unchecked {
            self.last = now;
        }
        None
    }

    /// Time passed: whether the stream now takes unchecked reports.
    fn tick(&mut self, now: Instant) -> Option<Note> {
        if self.checked || self.unchecked || now.duration_since(self.start) < UNCHECKED_AFTER {
            return None;
        }
        let plain = self.last_plain?;
        self.unchecked = true;
        self.last = self.last.max(plain);
        Some(Note::Unchecked)
    }

    fn timed_out(&self, now: Instant) -> bool {
        now.duration_since(self.last) > RTCP_TIMEOUT
    }
}

fn say(name: &str, note: Option<Note>) {
    match note {
        Some(Note::Checked) => tracing::info!("Apple Home live view of '{name}': the viewer's reports are checked"),
        Some(Note::Unchecked) => tracing::warn!("Apple Home live view of '{name}': the viewer's reports could not be checked, so any report from its address keeps the view going"),
        None => {}
    }
}

/// Watch for RTCP from the controller on our two ports (`Watch`); end the
/// session after `RTCP_TIMEOUT` without any. Both ports are read until empty
/// on every pass (without waiting on either), so a flood of other packets
/// on one does not hold back the controller's reports.
fn rtcp_watch(ctl: Arc<Ctl>, socks: [UdpSocket; 2], mut w: Watch, name: String) {
    for s in &socks {
        if s.set_nonblocking(true).is_err() {
            // (Cannot happen on a bound UDP socket; the read timeout stays.)
            tracing::debug!("HomeKit: an RTCP socket could not be made non-blocking");
        }
    }
    let mut buf = [0u8; 1500];
    while !ctl.stop.load(Ordering::SeqCst) {
        let mut full = false;
        for s in &socks {
            let mut n = 0;
            while let Ok((len, from)) = s.recv_from(&mut buf) {
                say(&name, w.packet(from.ip(), &buf[..len], Instant::now()));
                n += 1;
                if n >= DRAIN_MAX {
                    full = true;
                    break;
                }
            }
        }
        let now = Instant::now();
        say(&name, w.tick(now));
        if w.timed_out(now) {
            tracing::info!("Apple Home live view of '{name}': no report from the viewer for {} s; stopped", RTCP_TIMEOUT.as_secs());
            ctl.end();
            return;
        }
        if !full {
            std::thread::sleep(Duration::from_millis(20));
        }
    }
}

enum Outcome {
    /// It sent frames, then ended (stopped, the camera's stream restarted,
    /// ffmpeg ended): the session is over.
    Ran,
    /// We stopped it before it sent anything.
    Stopped,
    /// It ended without sending a frame (no packet left it): another
    /// encoder may take the session.
    Failed(String),
    NoSource,
}

fn run(ctl: Arc<Ctl>, p: Pending, src: Source, mut params: (VideoSel, Option<AudioSel>), encoders: Vec<Encoder>, slot: crate::transcode::Slot, software_after: bool) {
    let Pending { ep, socks, ssrc, .. } = p;
    {
        let (c, name) = (ctl.clone(), src.name.clone());
        let auth: Vec<SrtcpAuth> = [&ep.video, &ep.audio].into_iter().filter_map(SrtcpAuth::new).collect();
        let w = Watch::new(ep.ip, ssrc, auth, Instant::now());
        let _ = std::thread::Builder::new().name("homekit-rtcp".into()).spawn(move || rtcp_watch(c, socks, w, name));
    }
    let mut slot = slot;
    let mut tries = crate::transcode::live::attempts(&encoders);
    let mut i = 0;
    // (Each ffmpeg run's first sequence number: see SEQ_STEP.)
    let seq0: u16 = rand::thread_rng().gen_range(0..0x1000);
    let mut runs: u16 = 0;
    loop {
        if ctl.stop.load(Ordering::SeqCst) {
            break;
        }
        if runs >= MAX_RUNS {
            tracing::warn!("HomeKit: a live stream did not start after {MAX_RUNS} tries; stopped");
            break;
        }
        if i >= tries.len() {
            // Every way on the graphics card failed: software, if it has a place.
            match (software_after && slot.hardware).then(crate::transcode::reserve_software).flatten() {
                Some(sw) => {
                    slot = sw;
                    tries.push((Encoder::software(), false));
                }
                None => break,
            }
        }
        let (e, hw_decode) = &tries[i];
        let seq = seq0 + runs * SEQ_STEP;
        runs += 1;
        match attempt(&ctl, &ep, ssrc, seq, &src, &params, e, *hw_decode) {
            // A camera read by ffmpeg may have no sound after all: once more
            // without (nothing was sent, so the session is still unused).
            Outcome::Failed(_) if params.1.is_some() && matches!(src.feed, Feed::Direct { .. }) => {
                params.1 = None;
            }
            Outcome::Failed(why) => {
                crate::transcode::warn_limited(&format!("homekit {}", e.name(OutCodec::H264)), &format!("HomeKit: live stream with {} did not start: {}", e.name(OutCodec::H264), scrub(&why)));
                i += 1;
            }
            Outcome::Ran | Outcome::Stopped | Outcome::NoSource => break,
        }
    }
    ctl.end();
    drop(slot);
    tracing::info!("HomeKit: a live stream ended");
}

#[allow(clippy::too_many_arguments)]
fn attempt(ctl: &Arc<Ctl>, ep: &Endpoints, ssrc: [u32; 2], seq: u16, src: &Source, params: &(VideoSel, Option<AudioSel>), e: &Encoder, hw_decode: bool) -> Outcome {
    let (v, a) = *params;
    let fed = match &src.feed {
        Feed::Hub(hub) => match hub.subscribe_for(a.is_none()) {
            Some(f) => Some(f),
            None => return Outcome::NoSource,
        },
        Feed::Direct { .. } => None,
    };
    let args = ffmpeg_args(e, hw_decode, &v, a.as_ref(), ep, ssrc, seq, src.fps, &src.feed);
    let stdin_kind = if fed.is_some() { Stdio::piped() } else { Stdio::null() };
    let mut child = match crate::child::ffmpeg().args(&args).stdin(stdin_kind).stdout(Stdio::piped()).stderr(Stdio::piped()).spawn() {
        Ok(c) => c,
        Err(err) => return Outcome::Failed(format!("ffmpeg: {err}")),
    };
    crate::child::adopt(&child);
    let (stdin, stdout, stderr) = (child.stdin.take(), child.stdout.take(), child.stderr.take());
    *ctl.child.lock().unwrap() = Some(child);
    if ctl.stop.load(Ordering::SeqCst) {
        ctl.kill_child();
    }
    let errs = stderr.map(crate::transcode::live::keep_tail);
    let frames = Arc::new(AtomicU64::new(0));
    // Set once ffmpeg is gone: the feeder stops even while the camera sends nothing.
    let dead = Arc::new(AtomicBool::new(false));
    if let (Some(mut stdin), Some((init, gop, mut rx))) = (stdin, fed) {
        let quiet = a.is_none();
        let dead = dead.clone();
        std::thread::spawn(move || {
            use tokio::sync::broadcast::error::TryRecvError;
            for b in std::iter::once(init).chain(gop) {
                if stdin.write_all(&b).is_err() {
                    return;
                }
            }
            while !dead.load(Ordering::SeqCst) {
                match rx.try_recv() {
                    Ok(LiveMsg::Fragment(f)) => {
                        let f = if quiet { crate::ingest::fmp4::video_only(&f).unwrap_or_else(|| f.to_vec()) } else { f.to_vec() };
                        if stdin.write_all(&f).is_err() {
                            return;
                        }
                    }
                    Err(TryRecvError::Empty) => std::thread::sleep(Duration::from_millis(5)),
                    Err(TryRecvError::Lagged(_)) => continue,
                    Ok(LiveMsg::Reset) | Err(TryRecvError::Closed) => return,
                }
            }
        });
    }
    // Progress: frames sent so far (the last count comes before ffmpeg exits).
    let progress = stdout.map(|out| {
        let f = frames.clone();
        std::thread::spawn(move || {
            for line in BufReader::new(out).lines() {
                let Ok(line) = line else { return };
                if let Some(n) = line.strip_prefix("frame=").and_then(|n| n.trim().parse::<u64>().ok()) {
                    f.store(n, Ordering::SeqCst);
                }
            }
        })
    });
    let t0 = Instant::now();
    let mut last = (0u64, Instant::now());
    let mut logged = false;
    let mut we_stopped = false;
    loop {
        std::thread::sleep(Duration::from_millis(200));
        let done = ctl.child.lock().unwrap().as_mut().is_none_or(|c| matches!(c.try_wait(), Ok(Some(_)) | Err(_)));
        if done {
            break;
        }
        let n = frames.load(Ordering::SeqCst);
        if n > 0 && !logged {
            logged = true;
            tracing::info!("HomeKit: live stream {}x{} at {} fps, {} kbps, on {}{} after {} ms", v.width, v.height, v.fps, v.max_kbps, e.name(OutCodec::H264), if a.is_some() { " with Opus sound" } else { "" }, t0.elapsed().as_millis());
        }
        if n != last.0 {
            last = (n, Instant::now());
        }
        let stuck = if n == 0 { t0.elapsed() > FIRST_FRAME } else { last.1.elapsed() > FIRST_FRAME };
        if ctl.stop.load(Ordering::SeqCst) {
            we_stopped = true;
            ctl.kill_child();
        } else if stuck {
            ctl.kill_child();
        }
    }
    dead.store(true, Ordering::SeqCst);
    // (Taken out first: the lock is not held while waiting for it to exit.)
    let child = ctl.child.lock().unwrap().take();
    if let Some(mut c) = child {
        let _ = c.kill();
        let _ = c.wait();
    }
    if let Some(p) = progress {
        let _ = p.join();
    }
    we_stopped |= ctl.stop.load(Ordering::SeqCst);
    if frames.load(Ordering::SeqCst) > 0 {
        Outcome::Ran
    } else if we_stopped {
        Outcome::Stopped
    } else if matches!(&src.feed, Feed::Hub(h) if h.info().is_none()) {
        Outcome::NoSource
    } else {
        Outcome::Failed(errs.map(|e| crate::transcode::live::tail(&e)).unwrap_or_default())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The "no SRTP" suite, which is refused.
    const NONE: u8 = 2;

    fn hex(s: &str) -> Vec<u8> {
        (0..s.len()).step_by(2).map(|i| u8::from_str_radix(&s[i..i + 2], 16).unwrap()).collect()
    }

    /// A SetupEndpoints write as HAP-python's camera tests build it.
    pub fn setup_bytes(session: [u8; 16], ip: &str, vport: u16, aport: u16, suite: u8) -> Vec<u8> {
        let ipa: IpAddr = ip.parse().unwrap();
        let addr = address_tlv(ipa, vport, aport);
        let srtp = |k: u8| if suite == SUITE_AES128 { Builder::new().u8(1, 0).bytes(2, &[k; 16]).bytes(3, &[k; 14]).done() } else { Builder::new().u8(1, 2).bytes(2, &[]).bytes(3, &[]).done() };
        Builder::new().bytes(1, &session).bytes(3, &addr).bytes(4, &srtp(1)).bytes(5, &srtp(2)).done()
    }

    #[test]
    fn setup_endpoints_both_ways() {
        let s = [7u8; 16];
        let ep = parse_endpoints(&setup_bytes(s, "192.168.1.20", 50000, 50002, SUITE_AES128)).unwrap();
        assert_eq!(ep.session, s);
        assert_eq!(ep.ip, "192.168.1.20".parse::<IpAddr>().unwrap());
        assert_eq!((ep.video_port, ep.audio_port), (50000, 50002));
        assert_eq!(ep.video.key, vec![1; 16]);
        assert_eq!(ep.audio.salt, vec![2; 14]);
        // ffmpeg's params: base64 of key then salt (30 bytes).
        assert_eq!(base64::engine::general_purpose::STANDARD.decode(ep.video.ffmpeg_params()).unwrap().len(), 30);
        let ans = endpoints_answer(&s, "192.168.1.5".parse().unwrap(), 40000, 40002, &ep.video, &ep.audio, 0x01020304, 9);
        let t = Tlv::decode(&ans).unwrap();
        assert_eq!(t.get(1).unwrap(), &s);
        assert_eq!(t.byte(2), Some(0));
        let a = Tlv::decode(t.get(3).unwrap()).unwrap();
        assert_eq!((a.byte(1), a.get(2).unwrap()), (Some(0), &b"192.168.1.5"[..]));
        assert_eq!(a.get(3).unwrap(), &40000u16.to_le_bytes());
        assert_eq!(t.get(6).unwrap(), &[4, 3, 2, 1]);
        // Our answer parses as a request would (same layout).
        assert!(parse_endpoints(&ans).is_some());
        // IPv6 and "no SRTP".
        let e6 = parse_endpoints(&setup_bytes(s, "2001:db8::1", 1, 2, SUITE_AES128)).unwrap();
        assert!(e6.ip.is_ipv6());
        // No SRTP (not offered) is refused.
        assert!(parse_endpoints(&setup_bytes(s, "192.168.1.20", 1, 2, NONE)).is_none());
    }

    #[test]
    fn hap_python_setup_bytes() {
        // HAP-python's encoding (pyhap.tlv.encode with pyhap.camera's type
        // numbers) of a SetupEndpoints request: session 0x00..0f, IPv4
        // 192.168.1.114, ports 50000/50002, AES_CM_128 with key 0x11.., salt
        // 0x22.. (video) and 0x33/0x44 (audio). Our builder makes the same bytes.
        let want = hex("0110000102030405060708090a0b0c0d0e0f031a010100020d3139322e3136382e312e313134030250c3040252c30425010100021011111111111111111111111111111111030e22222222222222222222222222220525010100021033333333333333333333333333333333030e4444444444444444444444444444");
        let ep = parse_endpoints(&want).unwrap();
        assert_eq!(ep.ip.to_string(), "192.168.1.114");
        assert_eq!((ep.video_port, ep.audio_port), (50000, 50002));
        assert_eq!((ep.video.key[0], ep.video.salt[0], ep.audio.key[0], ep.audio.salt[0]), (0x11, 0x22, 0x33, 0x44));
        let ours = {
            let srtp = |k: u8, s: u8| Builder::new().u8(1, 0).bytes(2, &[k; 16]).bytes(3, &[s; 14]).done();
            Builder::new().bytes(1, &ep.session).bytes(3, &address_tlv(ep.ip, 50000, 50002)).bytes(4, &srtp(0x11, 0x22)).bytes(5, &srtp(0x33, 0x44)).done()
        };
        assert_eq!(ours, want);
    }

    #[test]
    fn bad_setup_is_refused() {
        let s = [7u8; 16];
        let good = setup_bytes(s, "192.168.1.20", 50000, 50002, SUITE_AES128);
        // AES-256 (suite 1), short keys, wrong address family, zero ports, junk.
        let a256 = {
            let addr = address_tlv("192.168.1.20".parse().unwrap(), 1, 2);
            let srtp = Builder::new().u8(1, 1).bytes(2, &[1; 32]).bytes(3, &[1; 14]).done();
            Builder::new().bytes(1, &s).bytes(3, &addr).bytes(4, &srtp).bytes(5, &srtp).done()
        };
        assert!(parse_endpoints(&a256).is_none());
        let short = {
            let addr = address_tlv("192.168.1.20".parse().unwrap(), 1, 2);
            let srtp = Builder::new().u8(1, 0).bytes(2, &[1; 15]).bytes(3, &[1; 14]).done();
            Builder::new().bytes(1, &s).bytes(3, &addr).bytes(4, &srtp).bytes(5, &srtp).done()
        };
        assert!(parse_endpoints(&short).is_none());
        let fam = {
            let addr = Builder::new().u8(1, 1).bytes(2, b"192.168.1.20").u16(3, 1).u16(4, 2).done();
            let srtp = Builder::new().u8(1, 2).done();
            Builder::new().bytes(1, &s).bytes(3, &addr).bytes(4, &srtp).bytes(5, &srtp).done()
        };
        assert!(parse_endpoints(&fam).is_none());
        assert!(parse_endpoints(&setup_bytes(s, "192.168.1.20", 0, 2, SUITE_AES128)).is_none());
        use rand::{Rng, SeedableRng};
        let mut r = rand::rngs::StdRng::seed_from_u64(5);
        for _ in 0..5000 {
            let mut b = good.clone();
            let i = r.gen_range(0..b.len());
            b[i] = r.gen();
            b.truncate(r.gen_range(0..=b.len()));
            let _ = parse_endpoints(&b);
            let _ = parse_selected(&b);
        }
    }

    /// A SelectedRTPStreamConfiguration as Home sends it.
    pub fn selected_bytes(session: [u8; 16], command: u8, w: u16, h: u16, fps: u8, kbps: u16, audio: bool) -> Vec<u8> {
        let ctl = Builder::new().bytes(1, &session).u8(2, command).done();
        let params = Builder::new().u8(1, 2).u8(2, 2).u8(3, 0).done();
        let attrs = Builder::new().u16(1, w).u16(2, h).u8(3, fps).done();
        let rtp = Builder::new().u8(1, 99).bytes(2, &0x1234u32.to_le_bytes()).u16(3, kbps).bytes(4, &0.5f32.to_le_bytes()).u16(5, 1378).done();
        let video = Builder::new().u8(1, 0).bytes(2, &params).bytes(3, &attrs).bytes(4, &rtp).done();
        let mut b = Builder::new().bytes(1, &ctl).bytes(2, &video);
        if audio {
            let ap = Builder::new().u8(1, 1).u8(2, 0).u8(3, 2).u8(4, 20).done();
            let ar = Builder::new().u8(1, 110).bytes(2, &0x99u32.to_le_bytes()).u16(3, 24).bytes(4, &5.0f32.to_le_bytes()).done();
            b = b.bytes(3, &Builder::new().u8(1, 3).bytes(2, &ap).bytes(3, &ar).u8(4, 0).done());
        }
        b.done()
    }

    #[test]
    fn selected_configuration() {
        let s = [3u8; 16];
        let sel = parse_selected(&selected_bytes(s, cmd::START, 1280, 720, 30, 1000, true)).unwrap();
        assert_eq!(sel.command, cmd::START);
        let v = sel.video.unwrap();
        assert_eq!((v.width, v.height, v.fps, v.max_kbps, v.pt, v.mtu, v.profile, v.level), (1280, 720, 30, 1000, 99, 1378, 2, 2));
        let a = sel.audio.unwrap();
        assert_eq!((a.codec, a.hz(), a.pt, a.max_kbps, a.packet_ms), (3, 24000, 110, 24, 20));
        // End needs no video; a start without it is refused; H.265 is refused.
        let end = Builder::new().bytes(1, &Builder::new().bytes(1, &s).u8(2, cmd::END).done()).done();
        assert_eq!(parse_selected(&end).unwrap().command, cmd::END);
        let start_bare = Builder::new().bytes(1, &Builder::new().bytes(1, &s).u8(2, cmd::START).done()).done();
        assert!(parse_selected(&start_bare).is_none());
        let mut h265 = selected_bytes(s, cmd::START, 1280, 720, 30, 1000, false);
        // (The video codec byte follows the control item and the video item's header.)
        let pos = 2 + 2 + 16 + 3 + 2 + 2;
        assert_eq!(h265[pos], 0);
        h265[pos] = 1;
        assert!(parse_selected(&h265).is_none());
        assert!(parse_selected(&selected_bytes(s, cmd::START, 10, 720, 30, 1000, false)).is_none());
        assert!(parse_selected(&selected_bytes(s, 9, 1280, 720, 30, 1000, false)).is_none());
        // Payload types are 7 bits.
        let mut big_pt = selected_bytes(s, cmd::START, 1280, 720, 30, 1000, false);
        let i = big_pt.windows(3).position(|w| w == [1, 1, 99]).unwrap();
        big_pt[i + 2] = 200;
        assert!(parse_selected(&big_pt).is_none());
    }

    #[test]
    fn starts_are_held_to_what_was_offered() {
        let v = |w, h, fps, kbps| VideoSel { profile: 2, level: 2, width: w, height: h, fps, pt: 99, max_kbps: kbps, mtu: 1378 };
        let c = clamp_video(v(3840, 2160, 60, 20000));
        assert_eq!((c.width, c.height, c.fps, c.max_kbps), (1920, 1080, 30, 6000));
        let c = clamp_video(v(1300, 740, 24, 50));
        assert_eq!((c.width, c.height, c.fps, c.max_kbps), (1280, 720, 24, 100));
        let c = clamp_video(v(400, 300, 30, 300));
        assert_eq!((c.width, c.height, c.fps), (320, 240, 15));
        let c = clamp_video(v(100, 100, 30, 300));
        assert_eq!((c.width, c.height), (320, 180), "smaller than anything offered: the smallest");
    }

    fn hub_feed() -> Feed {
        Feed::Hub(Arc::new(LiveHub::new()))
    }

    #[test]
    fn ffmpeg_errors_are_scrubbed() {
        let e = "[rtsp @ 0x1] method DESCRIBE failed: 401 rtsp://admin:hunter2@192.0.2.5:554/main a=crypto:1 AES_CM_128_HMAC_SHA1_80 inline:QUJDREVGR0hJSktMTU5PUA==";
        let s = scrub(e);
        assert!(!s.contains("hunter2") && !s.contains("QUJDREVG") && s.contains("inline:***") && s.contains("401"), "{s}");
    }

    #[test]
    fn a_changed_picture_ends_the_cameras_streams() {
        let fp = |text: &str| picture_fingerprint(text, false, "rtsp://cam/main", Some("rtsp://cam/sub"));
        assert_eq!(fp(""), fp(""));
        assert_ne!(fp(""), fp("drawtext=text='A'"));
        assert_ne!(picture_fingerprint("", false, "rtsp://a", None), picture_fingerprint("", false, "rtsp://b", None));
        assert_ne!(picture_fingerprint("", true, "rtsp://a", None), picture_fingerprint("", false, "rtsp://a", None));
        let st = Streams::default();
        st.fake_running(2, &fp(""));
        st.fake_running(3, &fp(""));
        st.end_changed(2, &fp(""));
        assert_eq!((st.count(2), st.count(3)), (1, 1), "unchanged: kept");
        st.end_changed(2, &fp("drawtext=text='A'"));
        assert_eq!((st.count(2), st.count(3)), (0, 1), "text added: that camera's view ended");
    }

    #[test]
    fn burned_in_text_is_drawn_before_scaling() {
        let s = [7u8; 16];
        let ep = parse_endpoints(&setup_bytes(s, "192.168.1.20", 50000, 50002, SUITE_AES128)).unwrap();
        let sel = parse_selected(&selected_bytes(s, cmd::START, 1280, 720, 30, 1000, false)).unwrap();
        let feed = Feed::Direct { input: vec!["-rtsp_transport".into(), "tcp".into(), "-i".into(), "rtsp://cam/x".into()], picture: "drawtext=text='Cam':x=10:y=10".into() };
        // Even with a graphics card that decodes, the text needs the frames in memory.
        let e = Encoder { hw: Some(Hw::Vaapi), device: None };
        let a = ffmpeg_args(&e, true, &sel.video.unwrap(), None, &ep, [1, 2], 0, 25.0, &feed);
        let j = a.join(" ");
        assert!(j.contains("-rtsp_transport tcp -i rtsp://cam/x") && !j.contains("pipe:0") && !j.contains("-hwaccel "), "{j}");
        let vf = &a[a.iter().position(|x| x == "-vf").unwrap() + 1];
        assert!(vf.starts_with("drawtext=") && vf.contains("scale="), "{vf}");
    }

    #[test]
    fn srtp_key_derivation_known_answer() {
        // RFC 3711 B.3.
        let (key, salt) = (hex("E1F97A0D3E018BE0D64FA32C06DE4139"), hex("0EC675AD498AFEEBB6960B3AABE6"));
        let mut k = [0u8; 16];
        srtp_kdf(&key, &salt, 0, &mut k).unwrap();
        assert_eq!(k.to_vec(), hex("C61E7A93744F39EE10734AFE3FF7A087"));
        let mut s = [0u8; 14];
        srtp_kdf(&key, &salt, 2, &mut s).unwrap();
        assert_eq!(s.to_vec(), hex("30CBBC08863D8C85D49DB34A9AE1"));
    }

    #[test]
    fn only_authentic_new_srtcp_counts() {
        // An SRTCP sender report from ffmpeg 8 (its own SRTP code), sent with
        // -srtp_out_params zRCambWjgYkLXq3UJKqYfIcPDm8SUFsJ3tQ86g0K.
        let inline = base64::engine::general_purpose::STANDARD.decode("zRCambWjgYkLXq3UJKqYfIcPDm8SUFsJ3tQ86g0K").unwrap();
        let srtp = Srtp { suite: SUITE_AES128, key: inline[..16].to_vec(), salt: inline[16..].to_vec() };
        let pkt = hex("80c80006000012343975d62cc4140491d2c4d5fcb557a9e19dff678d80000000338bd5a43ddcc1849bee");
        let mut a = SrtcpAuth::new(&srtp).unwrap();
        assert!(a.take(&pkt), "ffmpeg's packet authenticates");
        assert!(!a.take(&pkt), "the same packet again does not count");
        // Plain RTCP (what anyone on the network can send), a changed byte,
        // another key: no.
        let mut b = SrtcpAuth::new(&srtp).unwrap();
        assert!(!b.take(&[0x80, 0xc9, 0, 1, 0, 0, 4, 0xd2]));
        let mut bad = pkt.clone();
        bad[5] ^= 1;
        assert!(!b.take(&bad));
        let other = Srtp { suite: SUITE_AES128, key: vec![7; 16], salt: vec![7; 14] };
        assert!(!SrtcpAuth::new(&other).unwrap().take(&pkt));
        assert!(b.take(&pkt));
    }

    /// An SRTCP receiver report from `ssrc` with this index, under `srtp`.
    fn srtcp_rr(srtp: &Srtp, ssrc: u32, index: u32) -> Vec<u8> {
        let mut k = [0u8; 20];
        srtp_kdf(&srtp.key, &srtp.salt, 4, &mut k).unwrap();
        let key = ring::hmac::Key::new(ring::hmac::HMAC_SHA1_FOR_LEGACY_USE_ONLY, &k);
        let mut p = vec![0x80, 0xc9, 0, 1];
        p.extend_from_slice(&ssrc.to_be_bytes());
        p.extend_from_slice(&(0x8000_0000 | index).to_be_bytes());
        let tag = ring::hmac::sign(&key, &p);
        p.extend_from_slice(&tag.as_ref()[..10]);
        p
    }

    fn lab_srtp() -> Srtp {
        let inline = base64::engine::general_purpose::STANDARD.decode("zRCambWjgYkLXq3UJKqYfIcPDm8SUFsJ3tQ86g0K").unwrap();
        Srtp { suite: SUITE_AES128, key: inline[..16].to_vec(), salt: inline[16..].to_vec() }
    }

    fn watch(t0: Instant) -> Watch {
        let srtp = lab_srtp();
        Watch::new("192.168.1.20".parse().unwrap(), [0x1234, 0x5678], vec![SrtcpAuth::new(&srtp).unwrap()], t0)
    }

    #[test]
    fn our_own_reports_sent_back_never_count() {
        let t0 = Instant::now();
        let home: IpAddr = "192.168.1.20".parse().unwrap();
        let mut w = watch(t0);
        // ffmpeg's own sender report (SSRC 0x1234, ours), sent back from the
        // controller's address: signed with the right key, but refused.
        let sr = hex("80c80006000012343975d62cc4140491d2c4d5fcb557a9e19dff678d80000000338bd5a43ddcc1849bee");
        assert_eq!(w.packet(home, &sr, t0 + Duration::from_secs(1)), None);
        // A receiver report signed under the key but with our SSRC: refused too.
        let ours = srtcp_rr(&lab_srtp(), 0x5678, 1);
        assert_eq!(w.packet(home, &ours, t0 + Duration::from_secs(2)), None);
        // A signed sender report with another SSRC: Home sends none.
        let mut sr2 = srtcp_rr(&lab_srtp(), 777, 2);
        sr2[1] = 200;
        assert_eq!(w.packet(home, &sr2, t0 + Duration::from_secs(3)), None);
        assert_eq!(w.macs, 0, "refused before any tag is computed");
        assert!(w.timed_out(t0 + RTCP_TIMEOUT + Duration::from_secs(1)));
        // And none of them makes the stream take unchecked reports later.
        for _ in 0..3 {
            assert_eq!(w.packet(home, &sr, t0 + Duration::from_secs(4)), None);
        }
        assert_eq!(w.tick(t0 + Duration::from_secs(20)), None);
        assert!(!w.unchecked);
    }

    #[test]
    fn junk_is_dropped_before_the_tag() {
        let t0 = Instant::now();
        let home: IpAddr = "192.168.1.20".parse().unwrap();
        let mut w = watch(t0);
        let good = srtcp_rr(&lab_srtp(), 777, 1);
        let mut junk: Vec<Vec<u8>> = vec![vec![], vec![0x80, 0xc9, 0, 1, 0, 0]];
        for (i, v) in [(0, 0x40u8), (1, 199), (1, 205), (1, 96)] {
            let mut p = good.clone();
            p[i] = v;
            junk.push(p);
        }
        // An RTCP length past the end of the packet.
        let mut long = good.clone();
        long[3] = 50;
        junk.push(long);
        for j in &junk {
            assert_eq!(w.packet(home, j, t0), None);
        }
        // From another address: not even looked at.
        assert_eq!(w.packet("192.168.1.66".parse().unwrap(), &good, t0), None);
        assert_eq!(w.macs, 0);
        // A well-shaped but forged one costs one tag and does not count.
        let mut forged = good.clone();
        forged[20] ^= 1;
        assert_eq!(w.packet(home, &forged, t0 + Duration::from_secs(29)), None);
        assert_eq!(w.macs, 1);
        assert!(w.timed_out(t0 + RTCP_TIMEOUT + Duration::from_secs(1)));
    }

    #[test]
    fn checked_reports_keep_a_stream_and_say_so_once() {
        let t0 = Instant::now();
        // (An IPv4-mapped address of the controller is the same address.)
        let home: IpAddr = "::ffff:192.168.1.20".parse().unwrap();
        let mut w = watch(t0);
        let s = Duration::from_secs;
        assert_eq!(w.packet(home, &srtcp_rr(&lab_srtp(), 777, 1), t0 + s(1)), Some(Note::Checked));
        assert_eq!(w.packet(home, &srtcp_rr(&lab_srtp(), 777, 2), t0 + s(25)), None, "said once");
        assert_eq!(w.packet(home, &srtcp_rr(&lab_srtp(), 777, 2), t0 + s(40)), None, "a replay does not count");
        assert!(!w.timed_out(t0 + s(50)));
        assert!(w.timed_out(t0 + s(56)));
        // Unchecked ones never count once checked ones came.
        assert_eq!(w.tick(t0 + s(20)), None);
        assert!(!w.unchecked);
    }

    #[test]
    fn unchecked_reports_count_only_after_ten_seconds_without_checked_ones() {
        let t0 = Instant::now();
        let home: IpAddr = "192.168.1.20".parse().unwrap();
        let s = Duration::from_secs;
        let plain = [0x80u8, 0xc9, 0, 1, 0, 0, 3, 9];
        // Nothing from the controller at all: no switch, the stream ends.
        let mut w = watch(t0);
        assert_eq!(w.tick(t0 + s(15)), None);
        assert!(w.timed_out(t0 + s(31)));
        // Plain reports: not before 10 s.
        let mut w = watch(t0);
        for i in 1..=9 {
            assert_eq!(w.packet(home, &plain, t0 + s(i)), None);
            assert_eq!(w.tick(t0 + s(i)), None);
        }
        assert!(!w.unchecked);
        assert_eq!(w.tick(t0 + s(10)), Some(Note::Unchecked));
        assert_eq!(w.tick(t0 + s(11)), None, "said once");
        assert_eq!(w.packet(home, &plain, t0 + s(35)), None);
        assert!(!w.timed_out(t0 + s(60)), "unchecked reports keep it going");
        // Ours sent back still do not.
        let sr = hex("80c80006000012343975d62cc4140491d2c4d5fcb557a9e19dff678d80000000338bd5a43ddcc1849bee");
        let mut ours_rr = plain;
        ours_rr[4..8].copy_from_slice(&0x1234u32.to_be_bytes());
        assert_eq!(w.packet(home, &sr, t0 + s(64)), None);
        assert_eq!(w.packet(home, &ours_rr, t0 + s(64)), None);
        assert!(w.timed_out(t0 + s(66)));
        // A later checked one is still said.
        assert_eq!(w.packet(home, &srtcp_rr(&lab_srtp(), 777, 1), t0 + s(70)), Some(Note::Checked));
    }

    #[test]
    fn runs_of_one_session_never_share_packet_numbers() {
        // A run before another sent no frame: at most sound, 50 packets a
        // second, for FIRST_FRAME (and a few video packets the last progress
        // line had not counted yet). Far below the step between runs, and the
        // last run still starts below 65536.
        let most = 50 * FIRST_FRAME.as_secs() as u32 + 1000;
        assert!(most < u32::from(SEQ_STEP), "{most}");
        assert!(0x0fff + u32::from(MAX_RUNS - 1) * u32::from(SEQ_STEP) < 65536);
    }

    #[test]
    fn ffmpeg_arguments() {
        let s = [7u8; 16];
        let ep = parse_endpoints(&setup_bytes(s, "192.168.1.20", 50000, 50002, SUITE_AES128)).unwrap();
        let sel = parse_selected(&selected_bytes(s, cmd::START, 1280, 720, 30, 1000, true)).unwrap();
        let a = ffmpeg_args(&Encoder::software(), false, &sel.video.unwrap(), sel.audio.as_ref(), &ep, [1111, 2222], 0x2345, 25.0, &hub_feed());
        let j = a.join(" ");
        assert!(j.contains("-c:v libx264") && j.contains("-profile:v high") && j.contains("-level:v 4.0") && j.contains("-bf 0") && j.contains("-tune zerolatency"), "{j}");
        assert!(j.contains("scale=1280:720") && j.contains("fps=25"), "the camera's own rate caps it: {j}");
        assert!(j.contains("-b:v 666k"), "{j}");
        assert!(j.contains("-payload_type 99 -ssrc 1111 -seq 9029 -f rtp -srtp_out_suite AES_CM_128_HMAC_SHA1_80"), "{j}");
        assert!(j.contains("-payload_type 110 -ssrc 2222 -seq 9029 -f rtp"), "{j}");
        assert!(j.contains("srtp://192.168.1.20:50000?rtcpport=50000&pkt_size=1378"), "{j}");
        assert!(j.contains("-c:a libopus") && j.contains("-ar 24000") && j.contains("srtp://192.168.1.20:50002?rtcpport=50002&pkt_size=188"), "{j}");
        // Without sound, no audio output; IPv6 in brackets.
        let ep6 = parse_endpoints(&setup_bytes(s, "2001:db8::1", 50000, 50002, SUITE_AES128)).unwrap();
        let j = ffmpeg_args(&Encoder::software(), false, &sel.video.unwrap(), None, &ep6, [1, 2], 0, 30.0, &hub_feed()).join(" ");
        assert!(j.contains("srtp://[2001:db8::1]:50000") && !j.contains("libopus"), "{j}");
        // A link-local target gets the connection's interface as its zone.
        let mut ll = parse_endpoints(&setup_bytes(s, "fe80::1", 50000, 50002, SUITE_AES128)).unwrap();
        ll.scope = 2;
        let j = ffmpeg_args(&Encoder::software(), false, &sel.video.unwrap(), None, &ll, [1, 2], 0, 30.0, &hub_feed()).join(" ");
        assert!(j.contains("srtp://[fe80::1%2]:50000?rtcpport=50000"), "{j}");
    }

    #[test]
    fn a_refused_start_never_wakes_the_camera() {
        let st = Streams::default();
        let me: IpAddr = "192.168.1.20".parse().unwrap();
        let local: IpAddr = "192.168.1.5".parse().unwrap();
        let asked = std::sync::atomic::AtomicUsize::new(0);
        let source = |_: u32, _: u32| {
            asked.fetch_add(1, Ordering::SeqCst);
            None
        };
        let given_back = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let g = given_back.clone();
        let place = move || {
            let g = g.clone();
            Ok(Place { encoders: Vec::new(), slot: None, software_after: false, active: Release(Some(Box::new(move || { g.fetch_add(1, Ordering::SeqCst); }))) })
        };
        let start = |n: u8| parse_selected(&selected_bytes([n; 16], cmd::START, 640, 360, 30, 500, false)).unwrap();
        let prepare = |n: u8| st.setup(1, 2, me, 0, local, &setup_bytes([n; 16], "192.168.1.20", 5000, 5002, SUITE_AES128)).unwrap();
        // No place (encoder or stream cap): busy, and the camera is not asked.
        prepare(1);
        assert_eq!(st.start_with(1, 2, start(1), false, &source, &|| Err(status::BUSY)).err(), Some(status::BUSY));
        assert_eq!(asked.load(Ordering::SeqCst), 0);
        // The camera's place taken: busy, not asked, no place taken either.
        st.fake_running(2, "x");
        st.fake_running(2, "x");
        prepare(2);
        let refused = || -> Result<Place, i64> { panic!("no place is taken for a camera at its limit") };
        assert_eq!(st.start_with(1, 2, start(2), false, &source, &refused).err(), Some(status::BUSY));
        assert_eq!(asked.load(Ordering::SeqCst), 0);
        // A place, but no stream from the camera: the place is given back.
        st.end_all();
        prepare(3);
        assert_eq!(st.start_with(1, 2, start(3), false, &source, &place).err(), Some(status::UNABLE_TO_COMMUNICATE));
        assert_eq!((asked.load(Ordering::SeqCst), given_back.load(Ordering::SeqCst)), (1, 1));
    }

    /// SetupEndpoints on one connection and starts on another, with the
    /// supervisor's tidy and the API's count, all at once for a few
    /// seconds: every one keeps going (two locks taken in opposite orders
    /// by setup and start once stopped them both, and the whole server).
    #[test]
    fn setup_and_start_at_once_never_stop_each_other() {
        let st = Arc::new(Streams::default());
        let done = Arc::new(AtomicBool::new(false));
        let counts: Arc<[AtomicU64; 3]> = Arc::new(Default::default());
        let me: IpAddr = "127.0.0.1".parse().unwrap();
        // The session the setup thread prepared last, for the start thread.
        let last_setup = Arc::new(AtomicU64::new(0));
        let mut threads = Vec::new();
        for which in 0..3usize {
            let (st, done, counts, last_setup) = (st.clone(), done.clone(), counts.clone(), last_setup.clone());
            threads.push(std::thread::spawn(move || {
                let mut n = 0u64;
                while !done.load(Ordering::SeqCst) {
                    n += 1;
                    match which {
                        0 => {
                            // Fresh session ids (refused after two for the
                            // per-connection limit: no ports held).
                            let mut sess = [0u8; 16];
                            sess[..8].copy_from_slice(&n.to_le_bytes());
                            if st.setup(1, 2, me, 0, me, &setup_bytes(sess, "127.0.0.1", 5000, 5002, SUITE_AES128)).is_ok() {
                                last_setup.store(n, Ordering::SeqCst);
                            }
                        }
                        1 => {
                            // A start from another connection (refused), and
                            // one of the session prepared last: taken, given
                            // a place, no stream from the camera, given up.
                            let _ = st.select(9, 2, &selected_bytes([0xEE; 16], cmd::START, 1280, 720, 30, 1000, false), false, &|_, _| None);
                            let mut sess = [0u8; 16];
                            sess[..8].copy_from_slice(&last_setup.load(Ordering::SeqCst).to_le_bytes());
                            let place = || Ok(Place { encoders: Vec::new(), slot: None, software_after: false, active: Release(None) });
                            let _ = st.start_with(1, 2, parse_selected(&selected_bytes(sess, cmd::START, 1280, 720, 30, 1000, false)).unwrap(), false, &|_, _| None, &place);
                        }
                        _ => {
                            st.tidy();
                            let _ = st.count(2);
                            let _ = st.answer(1, 2);
                        }
                    }
                    counts[which].fetch_add(1, Ordering::SeqCst);
                }
            }));
        }
        let t0 = Instant::now();
        let mut last = [0u64; 3];
        while t0.elapsed() < Duration::from_secs(3) {
            std::thread::sleep(Duration::from_millis(500));
            let now: Vec<u64> = counts.iter().map(|c| c.load(Ordering::SeqCst)).collect();
            for i in 0..3 {
                assert!(now[i] > last[i], "thread {i} stopped making progress after {:?}: {now:?}", t0.elapsed());
                last[i] = now[i];
            }
        }
        done.store(true, Ordering::SeqCst);
        for t in threads {
            t.join().unwrap();
        }
        st.end_all();
    }

    #[test]
    fn sessions_only_to_the_controller_and_limited() {
        let st = Streams::default();
        trait T {
            fn setup_t(&self, conn: u64, aid: u64, peer: IpAddr, local: IpAddr, v: &[u8]) -> Result<Vec<u8>, i64>;
        }
        impl T for Streams {
            fn setup_t(&self, conn: u64, aid: u64, peer: IpAddr, local: IpAddr, v: &[u8]) -> Result<Vec<u8>, i64> {
                self.setup(conn, aid, peer, 0, local, v)
            }
        }
        let me: IpAddr = "192.168.1.20".parse().unwrap();
        let local: IpAddr = "192.168.1.5".parse().unwrap();
        // Another address than the session's peer is refused.
        assert_eq!(st.setup_t(1, 2, "192.168.1.99".parse().unwrap(), local, &setup_bytes([1; 16], "192.168.1.20", 5000, 5002, SUITE_AES128)).err(), Some(status::INVALID_VALUE));
        // The peer seen as ::ffff:192.168.1.20 is the same address.
        let ans = st.setup_t(1, 2, "::ffff:192.168.1.20".parse().unwrap(), local, &setup_bytes([2; 16], "192.168.1.20", 5000, 5002, SUITE_AES128)).unwrap();
        let t = Tlv::decode(&ans).unwrap();
        let a = Tlv::decode(t.get(3).unwrap()).unwrap();
        assert_eq!(a.get(2).unwrap(), b"192.168.1.5");
        let vp = u16le(a.get(3)).unwrap();
        let (lo, hi) = *RTP_PORTS.lock().unwrap();
        assert!((lo..=hi).contains(&vp), "RTCP comes back to a port of the range: {vp}");
        // The same session again, and too many per connection.
        assert!(st.setup_t(1, 2, me, local, &setup_bytes([2; 16], "192.168.1.20", 5000, 5002, SUITE_AES128)).is_err());
        assert!(st.setup_t(1, 2, me, local, &setup_bytes([3; 16], "192.168.1.20", 5000, 5002, SUITE_AES128)).is_ok());
        assert_eq!(st.setup_t(1, 2, me, local, &setup_bytes([4; 16], "192.168.1.20", 5000, 5002, SUITE_AES128)).err(), Some(status::OUT_OF_RESOURCES));
        // A start from another connection, or for another accessory, is refused.
        let none = |_: u32, _: u32| None;
        assert!(st.select(9, 2, &selected_bytes([2; 16], cmd::START, 640, 360, 30, 500, false), false, &none).is_err());
        assert!(st.select(1, 3, &selected_bytes([2; 16], cmd::START, 640, 360, 30, 500, false), false, &none).is_err());
        // No source: unable to communicate (and the session is used up).
        let place = || Ok(Place { encoders: Vec::new(), slot: None, software_after: false, active: Release(None) });
        let start = parse_selected(&selected_bytes([2; 16], cmd::START, 640, 360, 30, 500, false)).unwrap();
        assert_eq!(st.start_with(1, 2, start, false, &none, &place).err(), Some(status::UNABLE_TO_COMMUNICATE));
        assert!(st.start_with(1, 2, parse_selected(&selected_bytes([2; 16], cmd::START, 640, 360, 30, 500, false)).unwrap(), false, &none, &place).is_err());
        // Each connection reads its own answer back.
        assert!(!st.answer(1, 2).is_empty() && st.answer(5, 2).is_empty());
        let other = st.setup_t(5, 2, me, local, &setup_bytes([9; 16], "192.168.1.20", 6000, 6002, SUITE_AES128)).unwrap();
        assert_eq!(st.answer(5, 2), other);
        assert_ne!(st.answer(1, 2), other);
        // IPv6 link-local: the interface comes from the connection; without one it is refused.
        let ll: IpAddr = "fe80::1".parse().unwrap();
        assert!(st.setup(7, 2, ll, 0, "fe80::2".parse().unwrap(), &setup_bytes([8; 16], "fe80::1", 5000, 5002, SUITE_AES128)).is_err());
        assert!(st.setup(7, 2, ll, 3, "fe80::2".parse().unwrap(), &setup_bytes([8; 16], "fe80::1", 5000, 5002, SUITE_AES128)).is_ok());
        assert_eq!(st.tables.lock().unwrap().pending.get(&[8; 16]).unwrap().ep.scope, 3);
        st.end_conn(7);
        // Resume or reconfigure of a session that is not running: not found (Home sets up a new one).
        let ctl = |c: u8| Builder::new().bytes(1, &Builder::new().bytes(1, &[3; 16]).u8(2, c).done()).done();
        assert_eq!(st.select(1, 2, &ctl(cmd::RESUME), false, &none).err(), Some(status::NOT_FOUND));
        assert_eq!(st.select(1, 2, &selected_bytes([3; 16], cmd::RECONFIGURE, 640, 360, 30, 500, false), false, &none).err(), Some(status::NOT_FOUND));
        assert!(st.select(1, 2, &ctl(cmd::SUSPEND), false, &none).is_ok());
        // Hidden cameras' prepared sessions go.
        st.keep_only(&[3]);
        assert!(st.tables.lock().unwrap().pending.is_empty() && st.answer(5, 2).is_empty());
        // Closing the connection drops what it prepared.
        assert!(st.setup_t(1, 3, me, local, &setup_bytes([10; 16], "192.168.1.20", 5000, 5002, SUITE_AES128)).is_ok());
        st.end_conn(1);
        assert!(st.tables.lock().unwrap().pending.is_empty() && st.answer(1, 3).is_empty());
    }
}
