//! G.711 (A-law / mu-law) camera audio, made playable in browsers.
//!
//! Browsers' Media Source Extensions cannot play G.711, but they do play
//! FLAC in fragmented MP4. G.711 is 8-bit companded PCM, so each RTP packet
//! is expanded to 16-bit samples and wrapped as one FLAC frame with a
//! VERBATIM (uncompressed) subframe: no encoder library, a few dozen bytes of
//! framing, and the files and live view then carry the camera's audio like
//! any AAC camera (8 kHz mono = 128 kbit/s).

/// Which G.711 companding law the camera uses.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Law {
    A,
    Mu,
}

impl Law {
    /// From an RTSP encoding name ("pcma" / "pcmu").
    pub fn from_encoding(name: &str) -> Option<Law> {
        match name.to_ascii_lowercase().as_str() {
            "pcma" => Some(Law::A),
            "pcmu" => Some(Law::Mu),
            _ => None,
        }
    }
}

/// ITU-T G.711 A-law byte to a 16-bit linear sample.
pub fn alaw_to_linear(a: u8) -> i16 {
    let a = a ^ 0x55;
    let seg = (a & 0x70) >> 4;
    let mut t = i32::from(a & 0x0f) << 4;
    t = match seg {
        0 => t + 8,
        1 => t + 0x108,
        _ => (t + 0x108) << (seg - 1),
    };
    (if a & 0x80 != 0 { t } else { -t }) as i16
}

/// ITU-T G.711 mu-law byte to a 16-bit linear sample.
pub fn ulaw_to_linear(u: u8) -> i16 {
    let u = !u;
    let mut t = ((i32::from(u & 0x0f) << 3) + 0x84) << ((u & 0x70) >> 4);
    t -= 0x84;
    (if u & 0x80 != 0 { -t } else { t }) as i16
}

/// FLAC sample-rate code for the frame header; 0b1101 = rate follows as u16 Hz.
fn rate_code(rate: u32) -> u8 {
    match rate {
        8000 => 0b0100,
        16000 => 0b0101,
        22050 => 0b0110,
        24000 => 0b0111,
        32000 => 0b1000,
        44100 => 0b1001,
        48000 => 0b1010,
        _ => 0b1101,
    }
}

fn crc8(data: &[u8]) -> u8 {
    let mut c: u8 = 0;
    for &b in data {
        c ^= b;
        for _ in 0..8 {
            c = if c & 0x80 != 0 { (c << 1) ^ 0x07 } else { c << 1 };
        }
    }
    c
}

fn crc16(data: &[u8]) -> u16 {
    let mut c: u16 = 0;
    for &b in data {
        c ^= u16::from(b) << 8;
        for _ in 0..8 {
            c = if c & 0x8000 != 0 { (c << 1) ^ 0x8005 } else { c << 1 };
        }
    }
    c
}

/// FLAC's UTF-8-style coding of a frame/sample number (up to 36 bits).
fn push_coded_number(out: &mut Vec<u8>, n: u64) {
    if n < 0x80 {
        out.push(n as u8);
        return;
    }
    // Payload bits for 2..=7 bytes: 11, 16, 21, 26, 31, 36.
    let bytes = (2..=7u32).find(|&k| n < (1u64 << (5 * k + 1))).unwrap_or(7);
    let first_bits = 7 - bytes; // payload bits in the first byte
    let prefix: u8 = !(0xffu8 >> bytes); // `bytes` ones
    out.push(prefix | ((n >> (6 * (bytes - 1))) as u8 & ((1u8 << first_bits) - 1)));
    for i in (0..bytes - 1).rev() {
        out.push(0x80 | ((n >> (6 * i)) as u8 & 0x3f));
    }
}

/// One FLAC frame (variable-blocksize strategy, mono, 16-bit, VERBATIM)
/// holding `samples`, which start at sample number `first`.
pub fn flac_frame(samples: &[i16], first: u64, rate: u32) -> Vec<u8> {
    let mut f = Vec::with_capacity(16 + samples.len() * 2);
    f.extend_from_slice(&[0xff, 0xf9]); // sync + variable blocksize
    let rc = rate_code(rate);
    f.push((0b0111 << 4) | rc); // blocksize: 16-bit (n-1) follows
    f.push(0b0000_1000); // mono, 16 bits per sample
    push_coded_number(&mut f, first);
    f.extend_from_slice(&((samples.len().clamp(1, 65536) - 1) as u16).to_be_bytes());
    if rc == 0b1101 {
        f.extend_from_slice(&(rate.min(65535) as u16).to_be_bytes());
    }
    let h = crc8(&f);
    f.push(h);
    f.push(0b0000_0010); // subframe: VERBATIM, no wasted bits
    for s in samples {
        f.extend_from_slice(&s.to_be_bytes());
    }
    let c = crc16(&f);
    f.extend_from_slice(&c.to_be_bytes());
    f
}

/// The samples of a frame made by `flac_frame` (mono, 16-bit, VERBATIM);
/// None for anything else. Used to listen to G.711 cameras (sound trigger).
pub fn flac_frame_samples(f: &[u8]) -> Option<Vec<i16>> {
    if f.len() < 10 || f[0] != 0xff || f[1] != 0xf9 || f[2] >> 4 != 0b0111 || f[3] != 0b0000_1000 {
        return None;
    }
    // Coded sample number: as many bytes as the first byte has leading ones.
    let lead = f[4].leading_ones() as usize;
    let mut i = 4 + if lead == 0 { 1 } else { lead };
    let n = usize::from(u16::from_be_bytes([*f.get(i)?, *f.get(i + 1)?])) + 1;
    i += 2;
    if f[2] & 0x0f == 0b1101 {
        i += 2;
    }
    i += 1; // header CRC-8
    if f.get(i) != Some(&0b0000_0010) {
        return None;
    }
    let body = f.get(i + 1..i + 1 + 2 * n)?;
    Some(body.chunks_exact(2).map(|b| i16::from_be_bytes([b[0], b[1]])).collect())
}

/// The number of samples in a frame made by `flac_frame`, from its header
/// alone (None for anything else).
pub fn flac_block_size(f: &[u8]) -> Option<u32> {
    if f.len() < 10 || f[0] != 0xff || f[1] != 0xf9 || f[2] >> 4 != 0b0111 || f[3] != 0b0000_1000 {
        return None;
    }
    let lead = f[4].leading_ones() as usize;
    let i = 4 + if lead == 0 { 1 } else { lead };
    Some(u32::from(u16::from_be_bytes([*f.get(i)?, *f.get(i + 1)?])) + 1)
}

fn mp4_box(out: &mut Vec<u8>, kind: &[u8; 4], body: impl FnOnce(&mut Vec<u8>)) {
    let start = out.len();
    out.extend_from_slice(&[0, 0, 0, 0]);
    out.extend_from_slice(kind);
    body(out);
    let len = (out.len() - start) as u32;
    out[start..start + 4].copy_from_slice(&len.to_be_bytes());
}

/// The `fLaC` MP4 sample entry (with its `dfLa` STREAMINFO) for mono
/// 16-bit FLAC at `rate` Hz.
pub fn flac_sample_entry(rate: u32) -> Vec<u8> {
    let mut si = Vec::with_capacity(34);
    si.extend_from_slice(&16u16.to_be_bytes()); // min block size
    si.extend_from_slice(&65535u16.to_be_bytes()); // max block size
    si.extend_from_slice(&[0; 6]); // min/max frame size unknown
    // rate (20 bits) | channels-1 (3) | bits-1 (5) | total samples (36, 0 = unknown)
    let packed: u64 = (u64::from(rate & 0xf_ffff) << 44) | (15u64 << 36);
    si.extend_from_slice(&packed.to_be_bytes());
    si.extend_from_slice(&[0; 16]); // MD5 unknown
    let mut out = Vec::new();
    mp4_box(&mut out, b"fLaC", |b| {
        b.extend_from_slice(&[0, 0, 0, 0, 0, 0, 0, 1]); // reserved, data_reference_index 1
        b.extend_from_slice(&[0; 8]);
        b.extend_from_slice(&1u16.to_be_bytes()); // channels
        b.extend_from_slice(&16u16.to_be_bytes()); // sample size
        b.extend_from_slice(&[0, 0, 0, 0]);
        b.extend_from_slice(&((rate.min(65535)) << 16).to_be_bytes());
        mp4_box(b, b"dfLa", |b| {
            b.extend_from_slice(&[0, 0, 0, 0]); // version, flags
            b.push(0x80); // last metadata block, type 0 = STREAMINFO
            b.extend_from_slice(&(si.len() as u32).to_be_bytes()[1..]);
            b.extend_from_slice(&si);
        });
    });
    out
}

/// Turns a camera's G.711 packets into FLAC frames, numbering samples.
pub struct ToFlac {
    law: Law,
    rate: u32,
    next: u64,
    /// Timeline: a packet whose first sample is number `n` is placed at
    /// `base + n`.
    base: i64,
    /// RTP time and sample count of the previous packet.
    last: Option<(i64, u64)>,
    /// The previous packet's timeline position.
    last_pos: i64,
    /// Where the timeline was last set from the RTP clock: (RTP time, sample
    /// number) of that packet.
    anchor: (i64, u64),
    /// What the camera's clock is, once learnt.
    clock: Clock,
    /// While learning: each step's RTP time per sample.
    steps: Vec<f64>,
    /// The window of steps being gathered (RTP time, samples), and the RTP
    /// time per sample of the last windows.
    win: (i64, i64),
    windows: std::collections::VecDeque<f64>,
    /// Big steps (sound left out) not yet placed: RTP time, samples carried.
    big: (i64, i64),
    told: bool,
}

#[derive(Clone, Copy, PartialEq)]
enum Clock {
    Learning,
    /// Its RTP steps match its samples: timed by its clock.
    Exact,
    /// RTP time per sample is this (not 1): timed by its samples.
    Odd(f64),
}

/// A window of steps, in seconds of samples.
const WINDOW_SECS: i64 = 2;
/// The clock is learnt over this many windows (6 s).
const LEARN_WINDOWS: usize = 3;
/// An odd clock's rate is the median of this many last windows (10 minutes:
/// slow, so jittery steps do not move it).
const RATE_WINDOWS: usize = 301;

/// The median of `v` (sorted in place): with an even count, the mean of the
/// two middle values (timestamps in pairs, 0 then twice the step, give 1).
fn median(v: &mut [f64]) -> Option<f64> {
    if v.is_empty() {
        return None;
    }
    v.sort_by(f64::total_cmp);
    let m = v.len() / 2;
    Some(if v.len() % 2 == 0 { (v[m - 1] + v[m]) / 2.0 } else { v[m] })
}

impl ToFlac {
    pub fn new(law: Law, rate: u32) -> ToFlac {
        ToFlac {
            law,
            rate,
            next: 0,
            base: 0,
            last: None,
            last_pos: 0,
            anchor: (0, 0),
            clock: Clock::Learning,
            steps: Vec::new(),
            win: (0, 0),
            windows: Default::default(),
            big: (0, 0),
            told: false,
        }
    }

    /// The camera's RTP ticks per sample, once learnt.
    fn ticks_per_sample(&self) -> Option<f64> {
        match self.clock {
            Clock::Learning => None,
            Clock::Exact => Some(1.0),
            Clock::Odd(r) => Some(r),
        }
    }

    /// Like `convert`, with the packet's time in the stream's clock (the
    /// sample rate). The camera's clock is learnt over its first 6 s:
    ///
    /// - Most cameras' RTP steps match their samples (the middle 2-second
    ///   window is within 0.5% of them, or the middle step is and the windows
    ///   are within 3%): they are timed by
    ///   their clock, as all audio was before, so sound left out (silence,
    ///   lost packets) keeps its place. A difference of more than 30 ms (or a
    ///   packet) between the clock and the samples moves the timeline on.
    /// - Some cameras stamp G.711 packets with RTP steps that do not match the
    ///   samples (600 for 320 on average, with jitter), and audio placed by
    ///   them runs far too long. They are timed by their samples. Only an
    ///   unmistakable gap moves the timeline: one step of more than 8 times the
    ///   previous packet's samples, by the time it shows at the camera's rate
    ///   (the median of its last 2-second windows). Small losses are not
    ///   looked for (jitter must never make holes).
    ///
    /// A camera whose rate changes mid-stream (reconfigured) is not followed:
    /// it reconnects, and the clock is learnt again. When packets arrive does
    /// not matter. An RTP clock that goes back by more than a second is a
    /// camera restart: the timeline starts again from it.
    pub fn convert_timed(&mut self, rtp: i64, data: &[u8]) -> (i64, Vec<u8>) {
        let one_sec = i64::from(self.rate);
        let n = self.next;
        match self.last {
            None => self.restart(rtp, n),
            Some((last_rtp, last_n)) => {
                let step = rtp.wrapping_sub(last_rtp);
                let last_n = last_n.min(i64::MAX as u64 / 8) as i64;
                if step < -one_sec {
                    self.restart(rtp, n);
                } else {
                    let big = step > last_n * 8;
                    if big {
                        self.big = (self.big.0.saturating_add(step), self.big.1 + last_n);
                    } else {
                        if self.clock == Clock::Learning && last_n > 0 {
                            self.steps.push(step as f64 / last_n as f64);
                        }
                        self.win = (self.win.0.saturating_add(step), self.win.1 + last_n);
                        if self.win.1 >= WINDOW_SECS * one_sec {
                            let w = std::mem::take(&mut self.win);
                            self.add_window(w);
                        }
                    }
                    match self.clock {
                        Clock::Learning => {}
                        Clock::Exact => {
                            // Timed by the clock: from the anchor, its time against the samples.
                            let err = rtp.wrapping_sub(self.anchor.0).saturating_sub(n.saturating_sub(self.anchor.1).min(i64::MAX as u64) as i64);
                            let threshold = (one_sec * 3 / 100).max(last_n);
                            if err > threshold {
                                self.base = self.base.saturating_add(err);
                                self.anchor = (rtp, n);
                            } else if err < -one_sec {
                                self.anchor = (rtp, n);
                            }
                            self.big = (0, 0);
                        }
                        Clock::Odd(r) => {
                            // A rate under 1 comes from steps too wild to learn from
                            // (big steps left out, the short ones after them kept): never
                            // place gaps from it.
                            if self.big.1 > 0 && r < 1.005 {
                                self.big = (0, 0);
                            } else if self.big.1 > 0 {
                                let b = std::mem::take(&mut self.big);
                                let left_out = b.0 as f64 / r - b.1 as f64;
                                if left_out >= 1.0 {
                                    self.base = self.base.saturating_add(left_out.round() as i64);
                                }
                            }
                        }
                    }
                }
            }
        }
        let pos = self.base.saturating_add(n.min(i64::MAX as u64) as i64);
        let frame = self.convert(data);
        self.last = Some((rtp, data.len() as u64));
        self.last_pos = pos;
        (pos, frame)
    }

    fn add_window(&mut self, w: (i64, i64)) {
        if w.1 <= 0 {
            return;
        }
        self.windows.push_back(w.0 as f64 / w.1 as f64);
        while self.windows.len() > RATE_WINDOWS {
            self.windows.pop_front();
        }
        match self.clock {
            Clock::Learning if self.windows.len() >= LEARN_WINDOWS => {
                // Matching: the middle window within 0.5% of 1 (paired stamps, 0 then
                // 2x, make windows of exactly 1), or the middle step within 0.5% with
                // windows within 3% (lost packets raise a window a little). A camera
                // stamping most packets right and then catching up in one step
                // (320, 320, 320, 1600) has a middle step of 1 but windows of 2: odd.
                let step = median(&mut std::mem::take(&mut self.steps)).unwrap_or(0.0);
                let win = median(&mut self.windows.iter().copied().collect::<Vec<_>>()).unwrap_or(0.0);
                let exact = (win - 1.0).abs() < 0.005 || ((step - 1.0).abs() < 0.005 && (win - 1.0).abs() < 0.03);
                self.clock = if exact { Clock::Exact } else { Clock::Odd(win) };
            }
            Clock::Odd(_) => {
                if let Some(m) = median(&mut self.windows.iter().copied().collect::<Vec<_>>()) {
                    self.clock = Clock::Odd(m);
                }
            }
            _ => {}
        }
    }

    /// The first packet, or a camera restart: the timeline starts at its RTP
    /// time, and the clock is learnt again.
    fn restart(&mut self, rtp: i64, n: u64) {
        self.base = rtp.saturating_sub(n.min(i64::MAX as u64) as i64);
        self.anchor = (rtp, n);
        self.clock = Clock::Learning;
        self.steps.clear();
        self.win = (0, 0);
        self.windows.clear();
        self.big = (0, 0);
    }

    /// A time in the camera's RTP clock (an RTCP sender report) on this
    /// timeline: from the last packet, the RTP time between at the camera's
    /// rate of RTP ticks per sample. None until that rate is learnt, or
    /// further than a second from the last packet (it cannot be placed well).
    pub fn timeline_of(&self, rtp: i64) -> Option<i64> {
        let (last_rtp, _) = self.last?;
        let d = rtp.wrapping_sub(last_rtp);
        if d.abs() > i64::from(self.rate) {
            return None;
        }
        let scaled = (d as f64 / self.ticks_per_sample()?).round() as i64;
        Some(self.last_pos.saturating_add(scaled))
    }

    /// Once, when the camera's RTP steps and the samples it sent disagree by
    /// more than 10% (once the rate is known): RTP time passed per sample.
    pub fn take_mismatch(&mut self) -> Option<f64> {
        if self.told {
            return None;
        }
        let r = self.ticks_per_sample()?;
        if (r - 1.0).abs() > 0.1 {
            self.told = true;
            Some(r)
        } else {
            None
        }
    }

    pub fn rate(&self) -> u32 {
        self.rate
    }
    /// One G.711 packet (one byte per sample) to one FLAC frame.
    pub fn convert(&mut self, data: &[u8]) -> Vec<u8> {
        let dec = match self.law {
            Law::A => alaw_to_linear,
            Law::Mu => ulaw_to_linear,
        };
        let pcm: Vec<i16> = data.iter().map(|&b| dec(b)).collect();
        let f = flac_frame(&pcm, self.next, self.rate);
        self.next += pcm.len() as u64;
        f
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn g711_reference_values() {
        // Silence and full scale per ITU-T G.711.
        assert_eq!(alaw_to_linear(0xd5), 8);
        assert_eq!(alaw_to_linear(0x55), -8);
        assert_eq!(alaw_to_linear(0xaa), 32256);
        assert_eq!(alaw_to_linear(0x2a), -32256);
        assert_eq!(ulaw_to_linear(0xff), 0);
        assert_eq!(ulaw_to_linear(0x80), 32124);
        assert_eq!(ulaw_to_linear(0x00), -32124);
        // Monotonic over each half of the code space.
        let mut v: Vec<i16> = (0..=255u8).map(ulaw_to_linear).collect();
        v.sort();
        v.dedup();
        assert!(v.len() >= 255);
    }

    #[test]
    fn coded_numbers() {
        let enc = |n| {
            let mut v = Vec::new();
            push_coded_number(&mut v, n);
            v
        };
        assert_eq!(enc(0x7f), vec![0x7f]);
        assert_eq!(enc(0x80), vec![0xc2, 0x80]);
        assert_eq!(enc(0x7ff), vec![0xdf, 0xbf]);
        assert_eq!(enc(0x800), vec![0xe0, 0xa0, 0x80]);
        assert_eq!(enc(0xffff), vec![0xef, 0xbf, 0xbf]);
        assert_eq!(enc(0x10000), vec![0xf0, 0x90, 0x80, 0x80]);
    }

    #[test]
    fn frame_layout_and_crcs() {
        let f = flac_frame(&[0, 1, -1], 160, 8000);
        assert_eq!(&f[..4], &[0xff, 0xf9, 0x74, 0x08]);
        // 160 = 0xa0 -> two-byte coded number.
        assert_eq!(&f[4..6], &[0xc2, 0xa0]);
        assert_eq!(&f[6..8], &[0, 2]); // blocksize - 1
        assert_eq!(f[8], crc8(&f[..8]));
        assert_eq!(f[9], 0x02);
        assert_eq!(&f[10..16], &[0, 0, 0, 1, 0xff, 0xff]);
        let n = f.len();
        assert_eq!(u16::from_be_bytes([f[n - 2], f[n - 1]]), crc16(&f[..n - 2]));
        // CRC-8 check value for "123456789" (poly 0x07) and CRC-16/UMTS (0x8005).
        assert_eq!(crc8(b"123456789"), 0xf4);
        assert_eq!(crc16(b"123456789"), 0xfee8);
    }

    #[test]
    fn sample_entry_shape() {
        let e = flac_sample_entry(8000);
        assert_eq!(&e[4..8], b"fLaC");
        assert_eq!(u32::from_be_bytes([e[0], e[1], e[2], e[3]]) as usize, e.len());
        let d = e.windows(4).position(|w| w == b"dfLa").unwrap();
        assert_eq!(e[d + 8], 0x80);
        assert_eq!(&e[d + 9..d + 12], &[0, 0, 34]);
        // STREAMINFO sample rate field.
        let si = &e[d + 12..];
        let packed = u64::from_be_bytes(si[10..18].try_into().unwrap());
        assert_eq!(packed >> 44, 8000);
        assert_eq!((packed >> 41) & 7, 0);
        assert_eq!((packed >> 36) & 31, 15);
    }

    #[test]
    fn converter_numbers_samples() {
        let mut c = ToFlac::new(Law::A, 8000);
        let a = c.convert(&[0xd5; 160]);
        let b = c.convert(&[0xd5; 160]);
        assert_eq!(a[4], 0); // first sample 0
        assert_eq!(&b[4..6], &[0xc2, 0xa0]); // second frame starts at 160
        assert_eq!(a.len(), 8 + 1 + 320 + 2); // header, subframe byte, samples, CRC-16
    }

    /// Feed packets of `n` samples with these RTP steps (cycled), `count` of
    /// them; returns (first position, last position, RTP time of the last).
    fn feed(c: &mut ToFlac, rtp0: i64, n: usize, steps: &[i64], count: usize) -> (i64, i64, i64) {
        let mut rtp = rtp0;
        let (first, _) = c.convert_timed(rtp, &vec![0xff; n]);
        let mut last = first;
        for i in 0..count {
            rtp = rtp.wrapping_add(steps[i % steps.len()]);
            last = c.convert_timed(rtp, &vec![0xff; n]).0;
        }
        (first, last, rtp)
    }

    /// Feed `count` packets of `n` samples, the steps from `step(i)`; fails on
    /// any position step other than `n` (a hole).
    fn no_holes(c: &mut ToFlac, n: usize, count: usize, mut step: impl FnMut(usize) -> i64) {
        let mut rtp = 1_000i64;
        let mut prev = c.convert_timed(rtp, &vec![0xff; n]).0;
        for i in 0..count {
            rtp += step(i);
            let t = c.convert_timed(rtp, &vec![0xff; n]).0;
            assert_eq!(t - prev, n as i64, "packet {i}");
            prev = t;
        }
    }

    /// A small deterministic random number generator.
    fn noise(seed: &mut u64, span: i64) -> i64 {
        *seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
        ((*seed >> 33) as i64 % (2 * span + 1)) - span
    }

    #[test]
    fn audio_is_timed_by_its_samples_not_the_cameras_steps() {
        let mut c = ToFlac::new(Law::Mu, 8000);
        // 320 samples per packet, RTP steps 1.875 times that (600 on average).
        let steps = [800i64, 400, 784, 16, 48, 800, 800, 1200, 600, 552];
        let mut rtp = 1_000_000i64;
        let mut first = None;
        for i in 0..400 {
            let (t, f) = c.convert_timed(rtp, &[0xff; 320]);
            let first = *first.get_or_insert(t);
            assert_eq!(t - first, i * 320, "packet {i}: 320-tick steps");
            assert_eq!(flac_frame_samples(&f).unwrap().len(), 320);
            if i == 10 {
                assert!(c.timeline_of(rtp).is_none(), "the rate is not known yet");
            }
            rtp += steps[i as usize % steps.len()];
        }
        assert_eq!(first, Some(1_000_000));
        let r = c.take_mismatch().expect("a mismatch is reported");
        assert!((r - 1.875).abs() < 0.1, "{r}");
        assert!(c.take_mismatch().is_none(), "only once");
        // A sender report at the camera's RTP time lands on this timeline, scaled.
        let (last_rtp, last_pos) = (rtp - steps[399 % steps.len()], 1_000_000 + 399 * 320);
        assert_eq!(c.timeline_of(last_rtp), Some(last_pos));
        let p = c.timeline_of(last_rtp + 600).unwrap();
        assert!((p - (last_pos + 320)).abs() <= 20, "{p}");
        assert!(c.timeline_of(last_rtp + 100_000).is_none(), "too far to place");
        // 3 s left out at that camera's rate (45000 RTP ticks): 3 s on the timeline.
        let (t, _) = c.convert_timed(last_rtp + 45_000 + 320, &[0xff; 320]);
        let moved = t - (last_pos + 320);
        assert!((moved - 24_000).abs() < 800, "{moved}");
    }

    #[test]
    fn jitter_at_an_odd_rate_never_makes_holes() {
        // The rev3 pattern, and one of period 7 (straddling windows).
        no_holes(&mut ToFlac::new(Law::Mu, 8000), 320, 25_000, |i| [1400, 100, 300, 600, 600, 600, 600, 600, 600, 600][i % 10]);
        no_holes(&mut ToFlac::new(Law::Mu, 8000), 320, 25_000, |i| [1400, 100, 300, 600, 600, 600, 600][i % 7]);
        // Random noise around 600 for 320, an hour of it.
        for span in [300, 700, 1000] {
            let mut seed = span as u64;
            let mut c = ToFlac::new(Law::Mu, 8000);
            no_holes(&mut c, 320, 90_000, |_| 600 + noise(&mut seed, span));
            let r = c.ticks_per_sample().unwrap();
            assert!((r - 1.875).abs() < 0.02, "±{span}: {r}");
        }
        // Odd steps from the very start: nothing moves.
        let mut c = ToFlac::new(Law::Mu, 8000);
        let (first, last, _) = feed(&mut c, 0, 320, &[1400, 100], 24);
        assert_eq!(last - first, 24 * 320);
    }

    #[test]
    fn a_long_gap_at_an_odd_rate_is_kept() {
        let mut c = ToFlac::new(Law::Mu, 8000);
        let mut seed = 7u64;
        let mut rtp = 0i64;
        let mut last = c.convert_timed(rtp, &[0xff; 320]).0;
        for _ in 0..2000 {
            rtp += 600 + noise(&mut seed, 300);
            last = c.convert_timed(rtp, &[0xff; 320]).0;
        }
        // 3 s left out: 45000 RTP ticks at 1.875, plus the packet's own 600.
        rtp += 45_600;
        let t = c.convert_timed(rtp, &[0xff; 320]).0;
        assert!(((t - last) - (320 + 24_000)).abs() < 800, "{} s", (t - last - 320) as f64 / 8000.0);
    }

    #[test]
    fn a_camera_whose_clock_matches_keeps_lost_packets() {
        // 1% single losses over 1000 s: within 50 ms of its RTP clock.
        let mut c = ToFlac::new(Law::Mu, 8000);
        let mut rtp = 0i64;
        let (first, _) = c.convert_timed(rtp, &[0xff; 160]);
        let mut last = first;
        for i in 1..50_000i64 {
            rtp += 160;
            if i % 100 == 0 {
                continue;
            }
            last = c.convert_timed(rtp, &[0xff; 160]).0;
        }
        let short = (rtp - first) - (last - first);
        assert!(short.abs() <= 400, "{} ms short", short / 8);
        assert!(c.take_mismatch().is_none());
        assert_eq!(c.timeline_of(rtp), Some(last), "known, and 1");
        // Timestamps in pairs (0, then twice the step): taken as matching, no holes.
        no_holes(&mut ToFlac::new(Law::Mu, 8000), 160, 50_000, |i| if i % 2 == 0 { 0 } else { 320 });
        let mut c = ToFlac::new(Law::Mu, 8000);
        feed(&mut c, 0, 160, &[0, 320], 1000);
        assert!(c.clock == Clock::Exact);
    }

    #[test]
    fn a_camera_three_percent_off_gets_no_holes() {
        no_holes(&mut ToFlac::new(Law::Mu, 8000), 160, 50_000, |i| [165, 165, 164, 165, 165][i % 5]);
        no_holes(&mut ToFlac::new(Law::Mu, 8000), 160, 50_000, |i| if i % 2 == 0 { 159 } else { 160 });
        no_holes(&mut ToFlac::new(Law::Mu, 8000), 320, 20_000, |_| 603);
    }

    #[test]
    fn a_camera_that_catches_up_in_one_step_is_not_taken_as_matching() {
        // Right stamps, then one step catching up: rate 2, middle step 1.
        no_holes(&mut ToFlac::new(Law::Mu, 8000), 320, 20_000, |i| [320, 320, 320, 1600][i % 4]);
    }

    #[test]
    fn wild_jitter_on_a_matching_clock_never_inserts_time() {
        // +-1000 ticks (125 ms) on 160-sample packets: big steps get left out of
        // the windows and the rate comes out far under 1; no gap may be placed from it.
        let mut c = ToFlac::new(Law::Mu, 8000);
        let mut seed = 7u64;
        let mut rtp = 1_000i64;
        let first = c.convert_timed(rtp, &[0xff; 160]).0;
        let mut last = first;
        let count = 30 * 60 * 50;
        for i in 0..count {
            rtp = 1_000 + (i as i64 + 1) * 160 + noise(&mut seed, 1000);
            last = c.convert_timed(rtp, &[0xff; 160]).0;
        }
        let inserted = (last - first) - count as i64 * 160;
        assert!(inserted.abs() < 8000 * 5, "inserted {} s", inserted as f64 / 8000.0);
    }

    #[test]
    fn gaps_and_restarts_on_a_matching_clock() {
        let mut c = ToFlac::new(Law::Mu, 8000);
        let (_, t, mut rtp) = feed(&mut c, 5000, 160, &[160], 400);
        assert_eq!(t, 5000 + 400 * 160);
        // Nothing sent for 3 s, and the RTP clock shows it: a 3 s jump.
        rtp += 160 + 24_000;
        let (t2, _) = c.convert_timed(rtp, &[0xff; 160]);
        assert_eq!(t2, t + 160 + 24_000, "the gap is kept");
        rtp += 160;
        let (t3, _) = c.convert_timed(rtp, &[0xff; 160]);
        assert_eq!(t3, t2 + 160);
        // Gaps of 0.5 s: each kept.
        for _ in 0..3 {
            rtp += 160;
            let (before, _) = c.convert_timed(rtp, &[0xff; 160]);
            rtp += 4160;
            let (after, _) = c.convert_timed(rtp, &[0xff; 160]);
            assert_eq!(after - before, 160 + 4000);
        }
        // A restart: the clock goes back, and is learnt again.
        let (t4, _) = c.convert_timed(100, &[0xff; 160]);
        assert_eq!(t4, 100);
        assert!(c.timeline_of(100).is_none());
        let (t5, _) = c.convert_timed(260, &[0xff; 160]);
        assert_eq!(t5, 260);
        assert!(c.take_mismatch().is_none(), "gaps do not count as a mismatch");
        // A burst after a network stall: RTP is contiguous, so nothing moves
        // (when packets arrive is not looked at at all).
        let mut c = ToFlac::new(Law::Mu, 8000);
        let (first, last, _) = feed(&mut c, 0, 160, &[160], 600);
        assert_eq!(last - first, 600 * 160);
    }

    #[test]
    fn rtp_times_near_the_ends_do_not_overflow() {
        let mut c = ToFlac::new(Law::A, 8000);
        let (a, _) = c.convert_timed(i64::MAX - 100, &[0; 160]);
        assert_eq!(a, i64::MAX - 100);
        let (b, _) = c.convert_timed(i64::MIN + 60, &[0; 160]);
        let (d, _) = c.convert_timed(i64::MIN + 220_000, &[0; 160]);
        assert!(d >= b);
        feed(&mut c, i64::MAX - 1_000_000, 160, &[160, 1_000_000_000], 2000);
        feed(&mut c, 0, 320, &[600, 1_000_000_000, 600, 600], 2000);
        let _ = c.timeline_of(i64::MAX);
        let _ = c.timeline_of(i64::MIN);
    }

    #[test]
    fn frames_read_back() {
        let s: Vec<i16> = (0..300).map(|i| (i * 97 % 4000 - 2000) as i16).collect();
        for (first, rate) in [(0u64, 8000u32), (5_000_000, 8000), (160, 11025)] {
            assert_eq!(flac_frame_samples(&flac_frame(&s, first, rate)).as_deref(), Some(&s[..]));
        }
        assert_eq!(flac_frame_samples(&[1, 2, 3]), None);
    }
}

#[cfg(test)]
mod ffmpeg_check {
    /// Writes a raw .flac stream (1 s, 440 Hz A-law tone) and has ffmpeg decode it.
    /// `cargo test g711 -- --ignored` (needs ffmpeg on PATH).
    #[test]
    #[ignore]
    fn ffmpeg_decodes_our_flac() {
        use super::*;
        let law = |x: i16| -> u8 {
            // Find the A-law code whose decoded value is closest (test only).
            (0..=255u8).min_by_key(|&c| (i32::from(alaw_to_linear(c)) - i32::from(x)).abs()).unwrap()
        };
        let mut c = ToFlac::new(Law::A, 8000);
        let entry = flac_sample_entry(8000);
        let d = entry.windows(4).position(|w| w == b"dfLa").unwrap();
        let mut file = b"fLaC".to_vec();
        file.extend_from_slice(&entry[d + 8..]); // metadata block header + STREAMINFO
        let mut want = Vec::new();
        for p in 0..50 {
            let pkt: Vec<u8> = (0..160)
                .map(|i| {
                    let n = (p * 160 + i) as f64;
                    law((10000.0 * (n * 440.0 * std::f64::consts::TAU / 8000.0).sin()) as i16)
                })
                .collect();
            want.extend(pkt.iter().map(|&b| alaw_to_linear(b)));
            file.extend_from_slice(&c.convert(&pkt));
        }
        let dir = std::env::temp_dir().join(format!("g711flac-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let src = dir.join("t.flac");
        let out = dir.join("t.raw");
        std::fs::write(&src, &file).unwrap();
        let st = std::process::Command::new(crate::ffmpeg_bin::program())
            .args(["-v", "error", "-y", "-i"]).arg(&src)
            .args(["-f", "s16le", "-acodec", "pcm_s16le"]).arg(&out)
            .status().unwrap();
        assert!(st.success());
        let raw = std::fs::read(&out).unwrap();
        let got: Vec<i16> = raw.chunks(2).map(|b| i16::from_le_bytes([b[0], b[1]])).collect();
        assert_eq!(got, want);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Writes an audio-only fragmented MP4 (our fLaC entry, 2 s of tone) to
    /// $G711_MP4_OUT for browser (MSE) and ffprobe checks.
    #[test]
    #[ignore]
    fn write_fmp4_sample() {
        use super::*;
        use crate::ingest::fmp4::{fragment, init_segment, Sample, TrackInfo, TrackKind, TrackRun};
        let Ok(path) = std::env::var("G711_MP4_OUT") else { return };
        let track = TrackInfo { kind: TrackKind::Audio, timescale: 8000, sample_entry: flac_sample_entry(8000), width: 0, height: 0 };
        let mut out = init_segment(&[track]);
        let mut c = ToFlac::new(Law::A, 8000);
        let mut n = 0u64;
        for frag in 0..4u32 {
            let mut samples = Vec::new();
            let base = n;
            for _ in 0..25 {
                let pkt: Vec<u8> = (0..160)
                    .map(|_| {
                        n += 1;
                        let v = (8000.0 * (n as f64 * 440.0 * std::f64::consts::TAU / 8000.0).sin()) as i16;
                        (0..=255u8).min_by_key(|&k| (i32::from(alaw_to_linear(k)) - i32::from(v)).abs()).unwrap()
                    })
                    .collect();
                samples.push(Sample::new(160, c.convert(&pkt), true));
            }
            out.extend_from_slice(&fragment(frag + 1, &[TrackRun { track_index: 0, base_decode_time: base, samples }]));
        }
        std::fs::write(path, out).unwrap();
    }
}
