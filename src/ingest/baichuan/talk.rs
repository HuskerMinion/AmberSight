//! Two-way audio ("talk") over a Reolink's Baichuan session (TCP 9000), for
//! cameras with no RTSP audio backchannel or whose video already comes over
//! Baichuan. The browser side is the same as RTSP talk (`crate::talk`); only
//! the way out differs. Written from public protocol descriptions (message
//! ids, element names, block layout) and nothing else: see the module notes.
//!
//! The pieces:
//!   * IMA/DVI ADPCM: the camera's talk codec. Each block is framed as a
//!     BcMedia ADPCM packet ("01wb"), with the "00 01" sub-header followed by
//!     the byte count of the packed nibbles (512 for 1024 samples; a wrong
//!     value sounds distorted) and the DVI-4 state (predictor, step index).
//!   * `Halfband`: 8 kHz -> 16 kHz (the camera's talk rate) by a proper
//!     interpolation filter, never by repeating samples.
//!   * `TalkEncoder`: browser PCM -> 8 kHz -> camera rate -> ADPCM blocks,
//!     each marked as sound or silence.
//!   * `Talker`: one talk on the session, as a small state machine:
//!       - digital silence opens nothing; the first sound sends TalkConfig
//!         (setup takes 1-2 s) and audio is dropped until the camera says yes,
//!         so nothing plays back in a burst;
//!       - sends are paced so no more than about LEAD of audio waits in the
//!         camera;
//!       - RELEASE_AFTER after the last sound the talk is let go, so the
//!         Reolink app can talk again;
//!       - a refused or failed talk (TalkConfig refused while the Reolink app
//!         talks, no answer) ends only the talk: back off BACKOFF, warn at
//!         most once per BACKOFF. Nothing here can end the session or its
//!         video: talk errors never leave this module.
//!   * `Talks`: the talks on one session, kept in the session's shared state
//!     (`wired::Shared`) and driven by its thread (one session per device:
//!     the firmware allows about two).
//!
//! Messages (ids and their XML from public protocol notes, see `bc`):
//!   10  TalkAbility  ask (Extension: channelId); answer: TalkAbility with
//!       duplexList, audioStreamModeList, audioConfigList/audioConfig
//!       (audioType, sampleRate, samplePrecision, lengthPerEncoder,
//!       soundTrack). Read-only.
//!   201 TalkConfig   Extension (channelId) + TalkConfig (channelId, duplex,
//!       audioStreamMode, audioConfig as above); the camera answers 200 or
//!       refuses.
//!   202 Talk         Extension (binaryData 1, channelId) + BcMedia ADPCM
//!       packets; no answer.
//! No talk-stop message is publicly documented: a talk is let go by no longer
//! sending (the camera frees it on its own a few seconds later).
//!
//! Battery cameras never talk here: the caller (`crate::talk`) uses this way
//! only for cameras not set up as battery cameras, and the session refuses a
//! device that reports a battery or a hub channel not shown as wired.

use super::bc;
use super::client::Session;
use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::SyncSender;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tracing::{info, warn};

// ---------------------------------------------------------------- IMA ADPCM

const STEPS: [i32; 89] = [
    7, 8, 9, 10, 11, 12, 13, 14, 16, 17, 19, 21, 23, 25, 28, 31, 34, 37, 41, 45, 50, 55, 60, 66, 73, 80, 88, 97, 107, 118, 130, 143, 157, 173, 190, 209, 230,
    253, 279, 307, 337, 371, 408, 449, 494, 544, 598, 658, 724, 796, 876, 963, 1060, 1166, 1282, 1411, 1552, 1707, 1878, 2066, 2272, 2499, 2749, 3024, 3327,
    3660, 4026, 4428, 4871, 5358, 5894, 6484, 7132, 7845, 8630, 9493, 10442, 11487, 12635, 13899, 15289, 16818, 18500, 20350, 22385, 24623, 27086, 29794,
    32767,
];
const INDEX_STEP: [i32; 8] = [-1, -1, -1, -1, 2, 4, 6, 8];

/// The coder's state (the same on both ends).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Ima {
    pub pred: i32,
    pub index: i32,
}

impl Ima {
    fn step_delta(step: i32, code: u8) -> i32 {
        let mut d = step >> 3;
        if code & 4 != 0 {
            d += step;
        }
        if code & 2 != 0 {
            d += step >> 1;
        }
        if code & 1 != 0 {
            d += step >> 2;
        }
        d
    }

    fn apply(&mut self, code: u8) -> i16 {
        let d = Self::step_delta(STEPS[self.index as usize], code);
        self.pred = if code & 8 != 0 { self.pred - d } else { self.pred + d }.clamp(-32768, 32767);
        self.index = (self.index + INDEX_STEP[(code & 7) as usize]).clamp(0, 88);
        self.pred as i16
    }

    pub fn encode(&mut self, s: i16) -> u8 {
        let step = STEPS[self.index as usize];
        let mut diff = s as i32 - self.pred;
        let mut code = 0u8;
        if diff < 0 {
            code = 8;
            diff = -diff;
        }
        if diff >= step {
            code |= 4;
            diff -= step;
        }
        if diff >= step >> 1 {
            code |= 2;
            diff -= step >> 1;
        }
        if diff >= step >> 2 {
            code |= 1;
        }
        self.apply(code);
        code
    }

    #[cfg_attr(not(test), allow(dead_code))] // the fake camera, the probe and tests
    pub fn decode(&mut self, code: u8) -> i16 {
        self.apply(code & 15)
    }
}

/// Nibble order in a packet: true = the first sample in the low nibble (the
/// IMA convention). Not yet checked against a camera's speaker; if the live
/// listen sounds like noise, flip this one line (the rig's
/// baichuan_talk_sim.py decodes low-first too).
pub const LOW_NIBBLE_FIRST: bool = true;

fn pack(first: u8, second: u8) -> u8 {
    if LOW_NIBBLE_FIRST { first | (second << 4) } else { (first << 4) | second }
}

fn unpack(b: u8) -> (u8, u8) {
    if LOW_NIBBLE_FIRST { (b & 15, b >> 4) } else { (b >> 4, b & 15) }
}

/// BcMedia magic of an ADPCM packet.
pub const ADPCM_MAGIC: &[u8; 4] = b"01wb";

/// One block as a BcMedia ADPCM packet: magic, the inner size twice, then
/// `00 01`, the packed-nibble byte count, the DVI-4 state before the block
/// (predictor i16, step index u8, 0), the nibbles (order: LOW_NIBBLE_FIRST),
/// and padding to a multiple of 8.
pub fn frame_block(samples: &[i16], st: &mut Ima) -> Vec<u8> {
    let nib = samples.len().div_ceil(2);
    let inner = 8 + nib;
    let mut v = Vec::with_capacity(8 + inner + 8);
    v.extend_from_slice(ADPCM_MAGIC);
    v.extend_from_slice(&(inner as u16).to_le_bytes());
    v.extend_from_slice(&(inner as u16).to_le_bytes());
    v.extend_from_slice(&[0x00, 0x01]);
    v.extend_from_slice(&(nib as u16).to_le_bytes());
    v.extend_from_slice(&(st.pred as i16).to_le_bytes());
    v.push(st.index as u8);
    v.push(0);
    for pair in samples.chunks(2) {
        let lo = st.encode(pair[0]);
        let hi = pair.get(1).map(|&s| st.encode(s)).unwrap_or(0);
        v.push(pack(lo, hi));
    }
    while v.len() % 8 != 0 {
        v.push(0);
    }
    v
}

/// Read one ADPCM packet from the front of `data`: its samples and the bytes
/// it took (padding included). For the fake camera in tests and the rig.
#[cfg_attr(not(test), allow(dead_code))] // the fake camera and tests
pub fn parse_block(data: &[u8]) -> Option<(Vec<i16>, usize)> {
    if data.len() < 16 || &data[..4] != ADPCM_MAGIC || data[8..10] != [0x00, 0x01] {
        return None;
    }
    let inner = u16::from_le_bytes([data[4], data[5]]) as usize;
    let nib = u16::from_le_bytes([data[10], data[11]]) as usize;
    if inner != 8 + nib || data.len() < 8 + inner {
        return None;
    }
    let mut st = Ima { pred: i16::from_le_bytes([data[12], data[13]]) as i32, index: (data[14] as i32).min(88) };
    let mut out = Vec::with_capacity(nib * 2);
    for &b in &data[16..16 + nib] {
        let (a, c) = unpack(b);
        out.push(st.decode(a));
        out.push(st.decode(c));
    }
    let used = (8 + inner).div_ceil(8) * 8;
    Some((out, used.min(data.len())))
}

// ---------------------------------------------------------------- 8 -> 16 kHz

/// Taps of the interpolation filter (4k+3, so the half-band zeros fall on
/// every other tap).
const HB_TAPS: usize = 51;

/// Doubles the sample rate: zeros between the input samples, then a
/// Kaiser-windowed half-band low-pass (cut-off at a quarter of the output
/// rate, about 60 dB down past it) with a gain of two. The image of a 3.4 kHz
/// tone (at 4.6 kHz) is well down; delay HB_TAPS/2 output samples.
pub struct Halfband {
    taps: [f32; HB_TAPS],
    line: [f32; HB_TAPS],
}

fn bessel_i0(x: f64) -> f64 {
    let (mut sum, mut term) = (1.0, 1.0);
    for k in 1..50 {
        term *= (x / 2.0) / k as f64;
        sum += term * term;
    }
    sum
}

impl Default for Halfband {
    fn default() -> Self {
        Self::new()
    }
}

impl Halfband {
    pub fn new() -> Self {
        let beta = 5.65; // about 60 dB of stop band
        let m = (HB_TAPS - 1) as f64 / 2.0;
        let mut taps = [0f32; HB_TAPS];
        for (n, t) in taps.iter_mut().enumerate() {
            let k = n as f64 - m;
            // Ideal half-band (cut-off at fs/4), times 2 for the zero stuffing.
            let ideal = if k == 0.0 { 1.0 } else { (std::f64::consts::PI * k / 2.0).sin() / (std::f64::consts::PI * k / 2.0) };
            let r = k / m;
            let w = bessel_i0(beta * (1.0 - r * r).max(0.0).sqrt()) / bessel_i0(beta);
            *t = (ideal * w) as f32;
        }
        Halfband { taps, line: [0.0; HB_TAPS] }
    }

    fn step(&mut self, x: f32) -> f32 {
        self.line.copy_within(0..HB_TAPS - 1, 1);
        self.line[0] = x;
        self.line.iter().zip(self.taps.iter()).map(|(a, b)| a * b).sum()
    }

    pub fn push(&mut self, input: &[i16], out: &mut Vec<i16>) {
        for &s in input {
            let a = self.step(s as f32);
            let b = self.step(0.0);
            out.push(a.round().clamp(-32768.0, 32767.0) as i16);
            out.push(b.round().clamp(-32768.0, 32767.0) as i16);
        }
    }
}

// ---------------------------------------------------------------- formats

/// What the camera takes for talk (from its TalkAbility answer).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TalkFormat {
    pub rate: u32,
    /// Samples per ADPCM block (lengthPerEncoder).
    pub block_samples: usize,
    pub duplex: String,
    pub stream_mode: String,
    pub precision: u32,
    pub sound_track: String,
}

impl TalkFormat {
    pub fn block_duration(&self) -> Duration {
        Duration::from_micros(self.block_samples as u64 * 1_000_000 / self.rate.max(1) as u64)
    }
    pub fn describe(&self) -> String {
        format!("ADPCM {} Hz, {} samples a block, {}", self.rate, self.block_samples, self.duplex)
    }
}

/// Keep only plain word characters of a value the camera sent before it is
/// sent back in XML.
fn word(s: &str) -> String {
    s.chars().filter(|c| c.is_ascii_alphanumeric() || *c == '_').take(32).collect()
}

/// Read the camera's TalkAbility answer: the ADPCM, mono, 16-bit entry of
/// its audioConfigList (the lowest priority value when it lists several).
pub fn parse_ability(xml: &str) -> Result<TalkFormat, String> {
    if !xml.contains("TalkAbility") {
        return Err("the camera's talk answer has no TalkAbility".into());
    }
    let duplex = bc::xml_tag(xml, "duplex").map(word).filter(|s| !s.is_empty()).unwrap_or_else(|| "FDX".into());
    let stream_mode = bc::xml_tag(xml, "audioStreamMode").map(word).filter(|s| !s.is_empty()).unwrap_or_else(|| "followVideoStream".into());
    let mut best: Option<(i64, TalkFormat)> = None;
    let mut offered = Vec::new();
    for part in xml.split("<audioConfig>").skip(1) {
        let part = part.split("</audioConfig>").next().unwrap_or("");
        let ty = bc::xml_tag(part, "audioType").unwrap_or("").to_string();
        let rate: u32 = bc::xml_tag(part, "sampleRate").and_then(|v| v.parse().ok()).unwrap_or(16000);
        let block: usize = bc::xml_tag(part, "lengthPerEncoder").and_then(|v| v.parse().ok()).unwrap_or(1024);
        let precision: u32 = bc::xml_tag(part, "samplePrecision").and_then(|v| v.parse().ok()).unwrap_or(16);
        let track = bc::xml_tag(part, "soundTrack").map(word).unwrap_or_else(|| "mono".into());
        let prio: i64 = bc::xml_tag(part, "priority").and_then(|v| v.parse().ok()).unwrap_or(0);
        offered.push(format!("{ty} {rate} Hz {block} {track}"));
        let ok = ty.eq_ignore_ascii_case("adpcm")
            && matches!(rate, 8000 | 16000)
            && precision == 16
            && track.eq_ignore_ascii_case("mono")
            && (64..=4096).contains(&block)
            && block % 2 == 0;
        if ok && best.as_ref().is_none_or(|(p, _)| prio < *p) {
            best = Some((prio, TalkFormat { rate, block_samples: block, duplex: duplex.clone(), stream_mode: stream_mode.clone(), precision, sound_track: track }));
        }
    }
    match best {
        Some((_, f)) => Ok(f),
        None if offered.is_empty() => Err("the camera lists no talk audio format".into()),
        None => Err(format!("the camera's talk audio is not one AmberSight can send (it takes {})", offered.join(", "))),
    }
}

pub fn ext_xml(channel: u8) -> String {
    format!("<?xml version=\"1.0\" encoding=\"UTF-8\" ?>\n<Extension version=\"1.1\">\n<channelId>{channel}</channelId>\n</Extension>\n")
}

pub fn data_ext_xml(channel: u8) -> String {
    format!("<?xml version=\"1.0\" encoding=\"UTF-8\" ?>\n<Extension version=\"1.1\">\n<binaryData>1</binaryData>\n<channelId>{channel}</channelId>\n</Extension>\n")
}

pub fn config_xml(channel: u8, f: &TalkFormat) -> String {
    format!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\" ?>\n<body>\n<TalkConfig version=\"1.1\">\n<channelId>{channel}</channelId>\n<duplex>{}</duplex>\n\
         <audioStreamMode>{}</audioStreamMode>\n<audioConfig>\n<audioType>adpcm</audioType>\n<sampleRate>{}</sampleRate>\n\
         <samplePrecision>{}</samplePrecision>\n<lengthPerEncoder>{}</lengthPerEncoder>\n<soundTrack>{}</soundTrack>\n</audioConfig>\n</TalkConfig>\n</body>\n",
        f.duplex, f.stream_mode, f.rate, f.precision, f.block_samples, f.sound_track
    )
}

// ---------------------------------------------------------------- encoder

/// A block peak at or above this counts as sound (about -54 dBFS); below it
/// is silence (digital silence is 0).
pub const SOUND_PEAK: i16 = 64;

/// One encoded block, ready for the camera.
pub struct Block {
    pub bytes: Vec<u8>,
    pub sound: bool,
}

/// Browser PCM16LE at `rate` -> 8 kHz -> the camera's rate -> ADPCM blocks.
pub struct TalkEncoder {
    down: crate::talk::Resampler,
    up: Option<Halfband>,
    carry: Option<u8>,
    pcm: Vec<i16>,
    block: usize,
    ima: Ima,
    s8: Vec<i16>,
}

impl TalkEncoder {
    pub fn new(input_rate: u32, f: &TalkFormat) -> Self {
        TalkEncoder {
            down: crate::talk::Resampler::new(input_rate),
            up: (f.rate == 16000).then(Halfband::new),
            carry: None,
            pcm: Vec::new(),
            block: f.block_samples,
            ima: Ima::default(),
            s8: Vec::new(),
        }
    }

    pub fn push_pcm(&mut self, bytes: &[u8]) -> Vec<Block> {
        let mut all = Vec::with_capacity(bytes.len() + 1);
        all.extend(self.carry.take());
        all.extend_from_slice(bytes);
        let pairs = all.chunks_exact(2);
        self.carry = pairs.remainder().first().copied();
        let samples: Vec<i16> = pairs.map(|p| i16::from_le_bytes([p[0], p[1]])).collect();
        self.push_samples(&samples)
    }

    pub fn push_samples(&mut self, samples: &[i16]) -> Vec<Block> {
        self.s8.clear();
        self.down.push(samples, &mut self.s8);
        match &mut self.up {
            Some(h) => h.push(&self.s8, &mut self.pcm),
            None => self.pcm.extend_from_slice(&self.s8),
        }
        let mut out = Vec::new();
        while self.pcm.len() >= self.block {
            let chunk: Vec<i16> = self.pcm.drain(..self.block).collect();
            let sound = chunk.iter().any(|s| s.unsigned_abs() >= SOUND_PEAK as u16);
            out.push(Block { bytes: frame_block(&chunk, &mut self.ima), sound });
        }
        out
    }
}

// ---------------------------------------------------------------- one talk

/// Audio waiting in the camera is kept at about this much (and never near
/// a second).
pub const LEAD: Duration = Duration::from_millis(500);
/// Audio queued here beyond this is dropped (oldest first): with LEAD in
/// the camera, a burst never adds more than about a second of delay.
pub const MAX_QUEUE: Duration = Duration::from_millis(500);
/// Let the talk go this long after the last sound.
pub const RELEASE_AFTER: Duration = Duration::from_secs(2);
/// After a refused or failed talk: wait this long, and warn at most this often.
pub const BACKOFF: Duration = Duration::from_secs(5);
/// The camera has this long to answer TalkConfig (it takes 1-2 s).
pub const OPEN_TIMEOUT: Duration = Duration::from_secs(5);
/// And this long to answer TalkAbility.
pub const ASK_TIMEOUT: Duration = Duration::from_secs(5);

/// Where a talk's messages go (the session; a recorder in tests).
pub trait TalkSink {
    /// Send TalkConfig; the message number the answer will carry.
    fn send_config(&mut self, channel: u8, xml: &str) -> Result<u16, String>;
    /// Send ADPCM packets (one Talk message).
    fn send_audio(&mut self, channel: u8, num: u16, packets: &[u8]) -> Result<(), String>;
}

impl TalkSink for Session {
    fn send_config(&mut self, channel: u8, xml: &str) -> Result<u16, String> {
        self.send_ext_on(bc::MSG_TALK_CONFIG, channel, None, &ext_xml(channel), xml.as_bytes(), true).map_err(|e| format!("{e:#}"))
    }
    fn send_audio(&mut self, channel: u8, num: u16, packets: &[u8]) -> Result<(), String> {
        self.send_ext_on(bc::MSG_TALK, channel, Some(num), &data_ext_xml(channel), packets, false).map(|_| ()).map_err(|e| format!("{e:#}"))
    }
}

/// What a talk did, for the log and the tests (shared with its handle).
#[derive(Default, Debug)]
pub struct TalkStats {
    pub blocks_sent: AtomicU64,
    pub blocks_dropped: AtomicU64,
    pub opens: AtomicU64,
    pub refusals: AtomicU64,
    pub warnings: AtomicU64,
    pub releases: AtomicU64,
    /// Blocks handed to the session thread and not yet taken in.
    pub in_flight: AtomicU64,
    /// The talk is over on the session side (the session ended).
    pub ended: AtomicBool,
    pub end_reason: Mutex<Option<String>>,
    pub last_problem: Mutex<Option<String>>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Phase {
    Idle,
    Opening { num: u16, since: Instant },
    Ready,
    Backoff { until: Instant },
}

pub struct Talker {
    pub id: u64,
    pub channel: u8,
    pub fmt: TalkFormat,
    pub phase: Phase,
    /// The TalkConfig's number: the talk's audio carries it.
    num: u16,
    queue: VecDeque<Vec<u8>>,
    /// When the audio sent so far ends playing in the camera.
    play_until: Option<Instant>,
    last_sound: Option<Instant>,
    last_warn: Option<Instant>,
    pub stats: Arc<TalkStats>,
    label: String,
}

impl Talker {
    pub fn new(id: u64, channel: u8, fmt: TalkFormat, stats: Arc<TalkStats>, label: &str) -> Self {
        Talker {
            id,
            channel,
            fmt,
            phase: Phase::Idle,
            num: 0,
            queue: VecDeque::new(),
            play_until: None,
            last_sound: None,
            last_warn: None,
            stats,
            label: label.to_string(),
        }
    }

    fn drop_block(&self) {
        self.stats.blocks_dropped.fetch_add(1, Ordering::Relaxed);
    }

    fn quiet_for(&self, now: Instant) -> bool {
        self.last_sound.is_none_or(|t| now.duration_since(t) >= RELEASE_AFTER)
    }

    /// A refused or failed talk: close it, wait BACKOFF, warn at most once
    /// per BACKOFF. Only the talk: never an error for the session.
    fn fail(&mut self, why: String, now: Instant) {
        self.phase = Phase::Backoff { until: now + BACKOFF };
        let n = self.queue.len() as u64;
        self.stats.blocks_dropped.fetch_add(n, Ordering::Relaxed);
        self.queue.clear();
        self.play_until = None;
        self.stats.refusals.fetch_add(1, Ordering::Relaxed);
        *self.stats.last_problem.lock().unwrap() = Some(why.clone());
        if self.last_warn.is_none_or(|t| now.duration_since(t) >= BACKOFF) {
            self.last_warn = Some(now);
            self.stats.warnings.fetch_add(1, Ordering::Relaxed);
            warn!("talk {}: {why}; trying again in {} s (video goes on)", self.label, BACKOFF.as_secs());
        }
    }

    pub fn push(&mut self, block: Vec<u8>, sound: bool, now: Instant, sink: &mut dyn TalkSink) {
        if sound {
            self.last_sound = Some(now);
        }
        if let Phase::Backoff { until } = self.phase {
            if now < until {
                return self.drop_block();
            }
            self.phase = Phase::Idle;
        }
        match self.phase {
            Phase::Idle => {
                if sound {
                    // Open in the background; audio until then is dropped.
                    match sink.send_config(self.channel, &config_xml(self.channel, &self.fmt)) {
                        Ok(num) => {
                            self.num = num;
                            self.phase = Phase::Opening { num, since: now };
                        }
                        Err(e) => self.fail(format!("sending the talk request failed: {e}"), now),
                    }
                }
                self.drop_block();
            }
            Phase::Opening { .. } | Phase::Backoff { .. } => self.drop_block(),
            Phase::Ready => {
                if !sound && self.quiet_for(now) {
                    return self.drop_block();
                }
                self.queue.push_back(block);
                let max = (MAX_QUEUE.as_micros() / self.fmt.block_duration().as_micros().max(1)) as usize;
                while self.queue.len() > max.max(1) {
                    self.queue.pop_front();
                    self.drop_block();
                }
            }
        }
    }

    /// The camera's answer to a message; true when it was this talk's.
    pub fn on_reply(&mut self, num: u16, status: u16, now: Instant) -> bool {
        match self.phase {
            Phase::Opening { num: n, .. } if n == num => {
                if status == 200 {
                    self.phase = Phase::Ready;
                    self.play_until = None;
                    self.stats.opens.fetch_add(1, Ordering::Relaxed);
                    info!("talk {}: the camera is ready ({})", self.label, self.fmt.describe());
                } else {
                    self.fail(format!("the camera refused to talk (Baichuan status {status}; the Reolink app may be talking)"), now);
                }
                true
            }
            _ => false,
        }
    }

    pub fn tick(&mut self, now: Instant, sink: &mut dyn TalkSink) {
        match self.phase {
            Phase::Opening { since, .. } if now.duration_since(since) >= OPEN_TIMEOUT => {
                self.fail(format!("the camera did not answer the talk request within {} s", OPEN_TIMEOUT.as_secs()), now);
            }
            Phase::Ready => {
                let dur = self.fmt.block_duration();
                while !self.queue.is_empty() {
                    let at = self.play_until.map_or(now, |p| p.max(now));
                    if at.duration_since(now) >= LEAD {
                        break;
                    }
                    let b = self.queue.pop_front().unwrap();
                    if let Err(e) = sink.send_audio(self.channel, self.num, &b) {
                        return self.fail(format!("sending talk audio failed: {e}"), now);
                    }
                    self.stats.blocks_sent.fetch_add(1, Ordering::Relaxed);
                    self.play_until = Some(at + dur);
                }
                if self.queue.is_empty() && self.quiet_for(now) {
                    // Let it go: the Reolink app can talk again.
                    self.phase = Phase::Idle;
                    self.play_until = None;
                    self.stats.releases.fetch_add(1, Ordering::Relaxed);
                    info!("talk {}: quiet for {} s, talk let go", self.label, RELEASE_AFTER.as_secs());
                }
            }
            _ => {}
        }
    }

    /// Audio sent but not yet played (for tests).
    #[cfg(test)]
    pub fn ahead(&self, now: Instant) -> Duration {
        self.play_until.map_or(Duration::ZERO, |p| p.saturating_duration_since(now))
    }
}

// ---------------------------------------------------------------- per session

/// A talk opened on a session (what its handle gets back).
pub struct TalkOpened {
    pub id: u64,
    pub format: TalkFormat,
    pub stats: Arc<TalkStats>,
}

pub enum TalkCmd {
    Open { channel: u8, reply: SyncSender<Result<TalkOpened, String>> },
    Data { id: u64, block: Vec<u8>, sound: bool },
    End { id: u64 },
}

/// A talk waiting for the hub's word on its channel.
struct HubWait {
    channel: u8,
    until: Instant,
    reply: SyncSender<Result<TalkOpened, String>>,
}

struct Ask {
    id: u64,
    channel: u8,
    num: u16,
    until: Instant,
    reply: SyncSender<Result<TalkOpened, String>>,
}

/// The talks on one session.
#[derive(Default)]
pub struct Talks {
    talkers: Vec<Talker>,
    asks: Vec<Ask>,
    hub_waits: Vec<HubWait>,
    next_id: u64,
    pub label: String,
}

impl Talks {
    pub fn new(label: &str) -> Self {
        Talks { label: label.to_string(), ..Default::default() }
    }

    /// A talk or a talk ability question is open (the session is wanted).
    pub fn busy(&self) -> bool {
        !self.talkers.is_empty() || !self.asks.is_empty() || !self.hub_waits.is_empty()
    }

    /// Hold a talk until the hub says what the channel is (see `resolve`).
    pub fn wait(&mut self, channel: u8, reply: SyncSender<Result<TalkOpened, String>>, until: Instant) {
        self.hub_waits.push(HubWait { channel, until, reply });
    }

    /// Talks waiting for the hub: `verdict` is Some(Ok) for a wired channel
    /// (asked on), Some(Err) to refuse, None while not known (refused once
    /// their time is up).
    pub(super) fn resolve(&mut self, s: &mut Session, verdict: impl Fn(u8) -> Option<Result<(), String>>, now: Instant) {
        let mut i = 0;
        while i < self.hub_waits.len() {
            let v = verdict(self.hub_waits[i].channel);
            if v.is_none() && now < self.hub_waits[i].until {
                i += 1;
                continue;
            }
            let w = self.hub_waits.remove(i);
            match v {
                Some(Ok(())) => self.ask(s, w.channel, w.reply, now),
                Some(Err(why)) => {
                    let _ = w.reply.send(Err(why));
                }
                None => {
                    let _ = w.reply.send(Err(format!("talk over Baichuan is not for battery cameras: the hub did not say in time whether channel {} is one", w.channel)));
                }
            }
        }
    }

    /// Ask the camera what talk it takes (read-only). The answer opens the
    /// talk (see `on_msg`).
    pub(super) fn ask(&mut self, s: &mut Session, channel: u8, reply: SyncSender<Result<TalkOpened, String>>, now: Instant) {
        match s.send_ext_on(bc::MSG_TALK_ABILITY, channel, None, &ext_xml(channel), b"", true) {
            Ok(num) => {
                self.next_id += 1;
                self.asks.push(Ask { id: self.next_id, channel, num, until: now + ASK_TIMEOUT, reply });
            }
            Err(e) => {
                let _ = reply.send(Err(format!("asking the camera about talk failed: {e:#}")));
            }
        }
    }

    pub fn data(&mut self, s: &mut dyn TalkSink, id: u64, block: Vec<u8>, sound: bool, now: Instant) {
        if let Some(t) = self.talkers.iter_mut().find(|t| t.id == id) {
            // (Only this thread takes away, so it never goes below zero.)
            if t.stats.in_flight.load(Ordering::Relaxed) > 0 {
                t.stats.in_flight.fetch_sub(1, Ordering::Relaxed);
            }
            t.push(block, sound, now, s);
        }
    }

    pub fn end(&mut self, id: u64) {
        self.talkers.retain(|t| t.id != id);
    }

    /// A talk message from the camera: true when it was one.
    pub fn on_msg(&mut self, xml: impl FnOnce() -> String, h: &bc::Header, now: Instant) -> bool {
        match h.msg_id {
            bc::MSG_TALK_ABILITY => {
                let Some(i) = self.asks.iter().position(|a| a.num == h.msg_num) else { return true };
                let a = self.asks.remove(i);
                let r = if h.status != 200 {
                    Err(format!("the camera does not offer talk over Baichuan (status {})", h.status))
                } else {
                    parse_ability(&xml())
                };
                match r {
                    Ok(format) => {
                        let stats = Arc::new(TalkStats::default());
                        let opened = TalkOpened { id: a.id, format: format.clone(), stats: stats.clone() };
                        if a.reply.send(Ok(opened)).is_ok() {
                            info!("talk {}: channel {} takes {}", self.label, a.channel, format.describe());
                            self.talkers.push(Talker::new(a.id, a.channel, format, stats, &self.label));
                        }
                    }
                    Err(e) => {
                        let _ = a.reply.send(Err(e));
                    }
                }
                true
            }
            bc::MSG_TALK_CONFIG => {
                for t in self.talkers.iter_mut() {
                    if t.on_reply(h.msg_num, h.status, now) {
                        break;
                    }
                }
                true
            }
            bc::MSG_TALK => true,
            _ => false,
        }
    }

    pub fn tick(&mut self, s: &mut dyn TalkSink, now: Instant) {
        self.asks.retain(|a| {
            if now >= a.until {
                let _ = a.reply.send(Err(format!("the camera did not say what talk it takes within {} s", ASK_TIMEOUT.as_secs())));
                false
            } else {
                true
            }
        });
        // A talk whose handle is gone (its opening answer was never read,
        // or its End was lost): only this list still holds its stats.
        self.talkers.retain(|t| Arc::strong_count(&t.stats) > 1);
        for t in self.talkers.iter_mut() {
            t.tick(now, s);
        }
    }

    /// The session ended: every talk is told.
    pub fn end_all(&mut self, why: &str) {
        for a in self.asks.drain(..) {
            let _ = a.reply.send(Err(why.to_string()));
        }
        for w in self.hub_waits.drain(..) {
            let _ = w.reply.send(Err(why.to_string()));
        }
        for t in self.talkers.drain(..) {
            *t.stats.end_reason.lock().unwrap() = Some(why.to_string());
            t.stats.ended.store(true, Ordering::Relaxed);
        }
    }
}

/// A talk on a camera's shared Baichuan session. Dropping it ends the talk.
pub struct TalkHandle {
    pub id: u64,
    pub format: TalkFormat,
    pub stats: Arc<TalkStats>,
    pub(super) send: Box<dyn Fn(TalkCmd) -> bool + Send + Sync>,
}

impl TalkHandle {
    /// Pass one block on; false once the session is gone.
    pub fn send(&self, b: Block) -> bool {
        if self.stats.ended.load(Ordering::Relaxed) {
            return false;
        }
        // No more than about a second of blocks on their way to a busy session
        // thread: past that they are dropped here.
        let max = (1_000_000 / self.format.block_duration().as_micros().max(1)).max(4) as u64;
        if self.stats.in_flight.load(Ordering::Relaxed) >= max {
            self.stats.blocks_dropped.fetch_add(1, Ordering::Relaxed);
            return true;
        }
        self.stats.in_flight.fetch_add(1, Ordering::Relaxed);
        (self.send)(TalkCmd::Data { id: self.id, block: b.bytes, sound: b.sound })
    }
    pub fn ended(&self) -> Option<String> {
        self.stats.ended.load(Ordering::Relaxed).then(|| self.stats.end_reason.lock().unwrap().clone().unwrap_or_else(|| "the Baichuan session ended".into()))
    }
}

impl Drop for TalkHandle {
    fn drop(&mut self) {
        (self.send)(TalkCmd::End { id: self.id });
    }
}

// ---------------------------------------------------------------- tests

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use std::f64::consts::PI;

    /// Power of `freq` in `x` (Goertzel), sample rate `fs`.
    fn power(x: &[f64], freq: f64, fs: f64) -> f64 {
        let w = 2.0 * PI * freq / fs;
        let c = 2.0 * w.cos();
        let (mut s1, mut s2) = (0.0, 0.0);
        for &v in x {
            let s0 = v + c * s1 - s2;
            s2 = s1;
            s1 = s0;
        }
        s1 * s1 + s2 * s2 - c * s1 * s2
    }

    fn tone(freq: f64, fs: f64, n: usize, amp: f64) -> Vec<i16> {
        (0..n).map(|i| (amp * (2.0 * PI * freq * i as f64 / fs).sin()).round() as i16).collect()
    }

    /// Signal-to-noise ratio of `x` against the best-fitting sine at `freq`
    /// (no alignment needed), in dB.
    pub(crate) fn sine_snr(x: &[i16], freq: f64, fs: f64) -> f64 {
        let w = 2.0 * PI * freq / fs;
        let (mut ss, mut sc, mut cc, mut xs, mut xc, mut m) = (0.0, 0.0, 0.0, 0.0, 0.0, 0.0);
        for (i, &v) in x.iter().enumerate() {
            let (s, c) = ((w * i as f64).sin(), (w * i as f64).cos());
            ss += s * s;
            sc += s * c;
            cc += c * c;
            xs += v as f64 * s;
            xc += v as f64 * c;
            m += v as f64;
        }
        let _ = m;
        let det = ss * cc - sc * sc;
        let a = (xs * cc - xc * sc) / det;
        let b = (xc * ss - xs * sc) / det;
        let (mut sig, mut noise) = (0.0, 0.0);
        for (i, &v) in x.iter().enumerate() {
            let fit = a * (w * i as f64).sin() + b * (w * i as f64).cos();
            sig += fit * fit;
            noise += (v as f64 - fit).powi(2);
        }
        10.0 * (sig / noise.max(1e-9)).log10()
    }

    #[test]
    fn the_8_to_16_khz_filter_puts_a_3_4_khz_tone_image_well_down() {
        let x = tone(3400.0, 8000.0, 8000, 12000.0);
        let mut y = Vec::new();
        Halfband::new().push(&x, &mut y);
        assert_eq!(y.len(), 16000);
        let y: Vec<f64> = y[200..].iter().map(|&v| v as f64).collect();
        let tone_p = power(&y, 3400.0, 16000.0);
        let image_p = power(&y, 4600.0, 16000.0);
        let down = 10.0 * (tone_p / image_p).log10();
        eprintln!("3.4 kHz tone: image at 4.6 kHz is {down:.1} dB down");
        assert!(down >= 50.0, "image only {down:.1} dB down");
        // The pass band keeps its level (1 kHz in = 1 kHz out at 0 dB).
        let x = tone(1000.0, 8000.0, 8000, 12000.0);
        let mut y = Vec::new();
        Halfband::new().push(&x, &mut y);
        let gin = power(&x[100..].iter().map(|&v| v as f64).collect::<Vec<_>>(), 1000.0, 8000.0) / (x.len() - 100) as f64;
        let gout = power(&y[200..].iter().map(|&v| v as f64).collect::<Vec<_>>(), 1000.0, 16000.0) / (y.len() - 200) as f64;
        // (Goertzel power grows with the square of the length: per-sample
        // power is power/len; the 16 kHz run is twice as long.)
        let db = 10.0 * ((gout / 2.0) / gin).log10();
        eprintln!("1 kHz pass band gain {db:.3} dB");
        assert!(db.abs() < 0.3, "pass band gain {db:.2} dB");
    }

    #[test]
    fn upsampling_interpolates_and_never_repeats_samples() {
        let x = tone(1000.0, 8000.0, 4000, 10000.0);
        let mut y = Vec::new();
        Halfband::new().push(&x, &mut y);
        // A clean 16 kHz sine (repeating samples would leave a strong 7 kHz image).
        let snr = sine_snr(&y[100..], 1000.0, 16000.0);
        assert!(snr > 45.0, "SNR {snr:.1} dB");
        let repeats = y[100..].chunks(2).filter(|p| p[0] == p[1]).count();
        assert!(repeats < 50, "{repeats} sample pairs repeated");
    }

    #[test]
    fn block_headers_carry_the_packed_nibble_byte_count() {
        let mut st = Ima::default();
        let b = frame_block(&tone(500.0, 16000.0, 1024, 8000.0), &mut st);
        assert_eq!(&b[..4], b"01wb");
        assert_eq!(u16::from_le_bytes([b[4], b[5]]), 520, "inner size: sub-header + state + 512");
        assert_eq!(u16::from_le_bytes([b[6], b[7]]), 520);
        assert_eq!(&b[8..10], &[0x00, 0x01]);
        assert_eq!(u16::from_le_bytes([b[10], b[11]]), 512, "512 for 1024 samples");
        assert_eq!(&b[12..16], &[0, 0, 0, 0], "first block starts from a zero state");
        assert_eq!(b.len(), 528);
        // The next block's header holds the state the first ended in.
        let st1 = st;
        let b2 = frame_block(&tone(500.0, 16000.0, 1024, 8000.0), &mut st);
        assert_eq!(i16::from_le_bytes([b2[12], b2[13]]) as i32, st1.pred);
        assert_eq!(b2[14] as i32, st1.index);
        // Another block size: the field follows it; padding to 8.
        let b3 = frame_block(&[0i16; 500], &mut Ima::default());
        assert_eq!(u16::from_le_bytes([b3[10], b3[11]]), 250);
        assert_eq!(b3.len() % 8, 0);
        assert_eq!(parse_block(&b3).unwrap().1, b3.len());
    }

    #[test]
    fn adpcm_decodes_back_close_to_the_input() {
        let x: Vec<i16> = (0..16000)
            .map(|i| {
                let t = i as f64 / 16000.0;
                (6000.0 * (2.0 * PI * 440.0 * t).sin() + 3000.0 * (2.0 * PI * 1250.0 * t).sin()) as i16
            })
            .collect();
        let mut st = Ima::default();
        let mut bytes = Vec::new();
        for c in x.chunks(1024) {
            bytes.extend(frame_block(c, &mut st));
        }
        let mut y = Vec::new();
        let mut at = 0;
        while let Some((s, used)) = parse_block(&bytes[at..]) {
            y.extend(s);
            at += used;
        }
        assert_eq!(y.len(), x.len() + x.len() % 2);
        let (mut sig, mut noise) = (0.0, 0.0);
        for (a, b) in x.iter().zip(&y).skip(100) {
            sig += (*a as f64).powi(2);
            noise += (*a as f64 - *b as f64).powi(2);
        }
        let snr = 10.0 * (sig / noise).log10();
        eprintln!("ADPCM round trip SNR {snr:.1} dB");
        assert!(snr > 20.0, "ADPCM SNR {snr:.1} dB");
    }

    const ABILITY: &str = "<?xml version=\"1.0\" encoding=\"UTF-8\" ?><body><TalkAbility version=\"1.1\"><duplexList><duplex>FDX</duplex></duplexList>\
<audioStreamModeList><audioStreamMode>followVideoStream</audioStreamMode></audioStreamModeList><audioConfigList><audioConfig><priority>0</priority>\
<audioType>adpcm</audioType><sampleRate>16000</sampleRate><samplePrecision>16</samplePrecision><lengthPerEncoder>1024</lengthPerEncoder>\
<soundTrack>mono</soundTrack></audioConfig></audioConfigList></TalkAbility></body>";

    pub(crate) fn ability_xml(rate: u32, block: usize) -> String {
        ABILITY.replace("16000", &rate.to_string()).replace("1024", &block.to_string())
    }

    #[test]
    fn the_format_is_read_from_the_camera() {
        let f = parse_ability(ABILITY).unwrap();
        assert_eq!((f.rate, f.block_samples, f.duplex.as_str(), f.stream_mode.as_str()), (16000, 1024, "FDX", "followVideoStream"));
        assert_eq!(f.block_duration(), Duration::from_millis(64));
        let f = parse_ability(&ability_xml(8000, 512)).unwrap();
        assert_eq!((f.rate, f.block_samples), (8000, 512));
        let x = config_xml(0, &parse_ability(ABILITY).unwrap());
        for want in ["<TalkConfig version=\"1.1\">", "<channelId>0</channelId>", "<duplex>FDX</duplex>", "<audioType>adpcm</audioType>", "<sampleRate>16000</sampleRate>", "<lengthPerEncoder>1024</lengthPerEncoder>", "<soundTrack>mono</soundTrack>"] {
            assert!(x.contains(want), "{want} in {x}");
        }
        let e = parse_ability(&ABILITY.replace("adpcm", "aac")).unwrap_err();
        assert!(e.contains("not one AmberSight can send") && e.contains("aac"), "{e}");
        assert!(parse_ability("<body><TalkAbility/></body>").is_err());
        // A value the camera sent goes back as plain word characters only.
        let f = parse_ability(&ABILITY.replace("<duplex>FDX</duplex>", "<duplex>F<x>DX</duplex>")).unwrap();
        assert!(!f.duplex.contains('<'));
    }

    #[test]
    fn digital_silence_is_silence_and_a_tone_is_sound() {
        let f = parse_ability(ABILITY).unwrap();
        let mut e = TalkEncoder::new(48000, &f);
        let b = e.push_samples(&vec![0i16; 48000]);
        assert_eq!(b.len(), 15, "1 s at 16 kHz in 1024-sample blocks");
        assert!(b.iter().all(|b| !b.sound));
        let b = e.push_samples(&tone(440.0, 48000.0, 48000, 3000.0));
        assert!(b.iter().filter(|b| b.sound).count() >= 14);
        // Odd-byte splits of PCM16LE are put back together.
        let pcm: Vec<u8> = tone(440.0, 8000.0, 2048, 3000.0).iter().flat_map(|s| s.to_le_bytes()).collect();
        let mut e = TalkEncoder::new(8000, &f);
        let n = e.push_pcm(&pcm[..1001]).len() + e.push_pcm(&pcm[1001..]).len();
        assert_eq!(n, 4);
    }

    /// Keeps what a talk sends.
    #[derive(Default)]
    pub(crate) struct Rec {
        pub configs: Vec<(Instant, u16)>,
        pub audio: Vec<(Instant, u16, Vec<u8>)>,
        pub refuse_send: bool,
        num: u16,
    }

    impl TalkSink for Rec {
        fn send_config(&mut self, _c: u8, xml: &str) -> Result<u16, String> {
            assert!(xml.contains("TalkConfig"));
            self.num += 1;
            self.configs.push((Instant::now(), self.num));
            Ok(self.num)
        }
        fn send_audio(&mut self, _c: u8, num: u16, p: &[u8]) -> Result<(), String> {
            if self.refuse_send {
                return Err("broken pipe".into());
            }
            self.audio.push((Instant::now(), num, p.to_vec()));
            Ok(())
        }
    }

    fn talker() -> Talker {
        Talker::new(1, 0, parse_ability(ABILITY).unwrap(), Arc::default(), "test")
    }

    fn blk(sound: bool) -> Vec<u8> {
        let s = if sound { tone(300.0, 16000.0, 1024, 5000.0) } else { vec![0; 1024] };
        frame_block(&s, &mut Ima::default())
    }

    #[test]
    fn silence_opens_nothing_and_the_talk_is_let_go_2_s_after_the_last_sound() {
        let mut t = talker();
        let mut rec = Rec::default();
        let t0 = Instant::now();
        let ms = |n: u64| t0 + Duration::from_millis(n);
        for k in 0..50 {
            t.push(blk(false), false, ms(k * 64), &mut rec);
            t.tick(ms(k * 64), &mut rec);
        }
        assert!(rec.configs.is_empty(), "digital silence opened a talk");
        assert_eq!(t.phase, Phase::Idle);
        // Sound: TalkConfig goes out; audio before the answer is dropped.
        let at = 4000;
        t.push(blk(true), true, ms(at), &mut rec);
        assert_eq!(rec.configs.len(), 1);
        t.push(blk(true), true, ms(at + 64), &mut rec);
        t.tick(ms(at + 64), &mut rec);
        assert!(rec.audio.is_empty(), "audio before the camera is ready is dropped");
        assert!(t.on_reply(rec.configs[0].1, 200, ms(at + 1500)));
        assert_eq!(t.phase, Phase::Ready);
        // Speech for 1 s, then silence: sent until 2 s after the last sound.
        let mut now = at + 1500;
        for _ in 0..16 {
            t.push(blk(true), true, ms(now), &mut rec);
            t.tick(ms(now), &mut rec);
            now += 64;
        }
        let last_sound = now - 64;
        while now < last_sound + 3000 {
            t.push(blk(false), false, ms(now), &mut rec);
            t.tick(ms(now), &mut rec);
            now += 64;
        }
        assert_eq!(t.phase, Phase::Idle, "let go after 2 s of quiet");
        assert_eq!(t.stats.releases.load(Ordering::Relaxed), 1);
        // The pauses inside the 2 s went out with the speech (16 + ~31).
        let sent = rec.audio.len();
        assert!((16 + 28..=16 + 33).contains(&sent), "{sent} blocks sent");
        assert!(rec.audio.iter().all(|a| a.1 == rec.configs[0].1), "audio carries the TalkConfig's number");
        // More silence opens nothing again; new sound opens a new talk.
        t.push(blk(false), false, ms(now), &mut rec);
        assert_eq!(rec.configs.len(), 1);
        t.push(blk(true), true, ms(now + 64), &mut rec);
        assert_eq!(rec.configs.len(), 2);
    }

    #[test]
    fn sends_are_paced_so_under_a_second_waits_in_the_camera() {
        let mut t = talker();
        let mut rec = Rec::default();
        let t0 = Instant::now();
        t.push(blk(true), true, t0, &mut rec);
        t.on_reply(rec.configs[0].1, 200, t0);
        // A burst: 3 s of audio arrives at once (a stalled browser catching up).
        for _ in 0..47 {
            t.push(blk(true), true, t0, &mut rec);
        }
        assert!(t.stats.blocks_dropped.load(Ordering::Relaxed) >= 47 - 8, "the queue is capped at 0.5 s");
        // Then steady speech in real time, the session ticking every 20-100 ms.
        let mut worst = Duration::ZERO;
        let mut sent_by: Vec<(Duration, usize)> = Vec::new();
        let mut el = Duration::ZERO;
        let mut next_block = Duration::ZERO;
        let mut step = 20;
        while el < Duration::from_secs(6) {
            let now = t0 + el;
            while next_block <= el {
                t.push(blk(true), true, now, &mut rec);
                next_block += Duration::from_millis(64);
            }
            t.tick(now, &mut rec);
            worst = worst.max(t.ahead(now));
            sent_by.push((el, rec.audio.len()));
            step = if step == 20 { 100 } else { 20 };
            el += Duration::from_millis(step);
        }
        assert!(worst <= LEAD + Duration::from_millis(64), "{worst:?} waiting in the camera");
        // Never more sent than real time plus the lead allows.
        for (el, n) in sent_by {
            let allowed = ((el + LEAD).as_millis() / 64 + 1) as usize;
            assert!(n <= allowed, "at {el:?}: {n} blocks sent, at most {allowed}");
        }
        // And the steady part keeps up (nothing piles up or is lost there).
        assert!(rec.audio.len() >= 6000 / 64 - 2, "{} blocks", rec.audio.len());
    }

    #[test]
    fn a_talk_whose_handle_is_gone_is_dropped_and_in_flight_blocks_are_bounded() {
        let mut talks = Talks::new("test");
        let mut rec = Rec::default();
        let kept = Arc::new(TalkStats::default());
        talks.talkers.push(Talker::new(1, 0, parse_ability(ABILITY).unwrap(), kept.clone(), "test"));
        talks.talkers.push(Talker::new(2, 0, parse_ability(ABILITY).unwrap(), Arc::default(), "test"));
        talks.tick(&mut rec, Instant::now());
        assert_eq!(talks.talkers.iter().map(|t| t.id).collect::<Vec<_>>(), vec![1], "the one nobody holds is gone");
        assert!(talks.busy());
        drop(kept);
        talks.tick(&mut rec, Instant::now());
        assert!(!talks.busy(), "the session may sign out");
        // A session thread that does not take blocks in: about 1 s (15
        // blocks of 64 ms) goes out, the rest is dropped on this side.
        let sent = Arc::new(std::sync::atomic::AtomicU64::new(0));
        let s2 = sent.clone();
        let h = TalkHandle {
            id: 1,
            format: parse_ability(ABILITY).unwrap(),
            stats: Arc::default(),
            send: Box::new(move |c| {
                if matches!(c, TalkCmd::Data { .. }) {
                    s2.fetch_add(1, Ordering::Relaxed);
                }
                true
            }),
        };
        for _ in 0..100 {
            assert!(h.send(Block { bytes: blk(true), sound: true }));
        }
        assert_eq!(sent.load(Ordering::Relaxed), 15);
        assert_eq!(h.stats.blocks_dropped.load(Ordering::Relaxed), 85);
    }

    #[test]
    fn nibble_order_is_one_switch() {
        // pack/unpack agree, and the first sample is where the switch says.
        assert_eq!(unpack(pack(3, 9)), (3, 9));
        assert_eq!(pack(3, 9), if LOW_NIBBLE_FIRST { 0x93 } else { 0x39 });
    }

    #[test]
    fn a_refused_talk_backs_off_5_s_and_warns_once_per_5_s() {
        let mut t = talker();
        let mut rec = Rec::default();
        let t0 = Instant::now();
        let ms = |n: u64| t0 + Duration::from_millis(n);
        t.push(blk(true), true, ms(0), &mut rec);
        assert!(t.on_reply(rec.configs[0].1, 400, ms(800)));
        assert!(matches!(t.phase, Phase::Backoff { .. }));
        assert_eq!(t.stats.warnings.load(Ordering::Relaxed), 1);
        // Within the back-off: sound opens nothing.
        let mut now = 800;
        while now < 5700 {
            t.push(blk(true), true, ms(now), &mut rec);
            t.tick(ms(now), &mut rec);
            now += 64;
        }
        assert_eq!(rec.configs.len(), 1, "nothing asked during the back-off");
        // After it: asked again; refused again, but no second warning yet
        // only if within 5 s of the first (here it is past 5 s: one more).
        t.push(blk(true), true, ms(5900), &mut rec);
        assert_eq!(rec.configs.len(), 2);
        t.on_reply(rec.configs[1].1, 400, ms(6000));
        assert_eq!(t.stats.refusals.load(Ordering::Relaxed), 2);
        assert_eq!(t.stats.warnings.load(Ordering::Relaxed), 2);
        // No answer at all is a failure too (after OPEN_TIMEOUT).
        t.push(blk(true), true, ms(11_100), &mut rec);
        assert_eq!(rec.configs.len(), 3);
        t.tick(ms(11_100 + 5_100), &mut rec);
        assert_eq!(t.stats.refusals.load(Ordering::Relaxed), 3);
        // A failed send ends only the talk.
        let mut t = talker();
        let mut rec = Rec { refuse_send: true, ..Default::default() };
        t.push(blk(true), true, t0, &mut rec);
        t.on_reply(1, 200, t0);
        t.push(blk(true), true, t0, &mut rec);
        t.tick(t0, &mut rec);
        assert!(matches!(t.phase, Phase::Backoff { .. }));
        // Warnings within 5 s of each other: only one.
        let mut t = talker();
        let mut rec = Rec::default();
        t.push(blk(true), true, ms(0), &mut rec);
        t.on_reply(1, 400, ms(0));
        t.phase = Phase::Idle; // (as if the back-off had passed early)
        t.push(blk(true), true, ms(1000), &mut rec);
        t.on_reply(2, 400, ms(1000));
        assert_eq!((t.stats.refusals.load(Ordering::Relaxed), t.stats.warnings.load(Ordering::Relaxed)), (2, 1));
    }
}
