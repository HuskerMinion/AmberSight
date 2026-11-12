//! Listening to a camera's microphone: the loud-sound trigger.
//!
//! The camera's audio (AAC, or G.711 that the ingest turned into FLAC frames)
//! is decoded here to plain numbers, one channel. Every 100 ms the loudness
//! is measured (in dBFS: 0 is the loudest a microphone can report, quiet
//! rooms sit around -60) and compared with the usual background level of
//! that spot, which follows slowly. A sound that is clearly louder than the
//! background (by `margin_db`), loud in itself (above `min_db`) and lasts a
//! little while (`min_ms`) starts an event. See `labels` for naming the sound.

pub mod labels;

use anyhow::{anyhow, Result};
use std::collections::VecDeque;

/// How a camera's audio frames are coded.
#[derive(Clone, Debug, PartialEq)]
pub enum AudioCodec {
    /// AAC (raw frames), with its AudioSpecificConfig.
    Aac { config: Vec<u8> },
    /// Mono 16-bit FLAC frames as made from G.711 (crate::ingest::g711).
    Flac16 { rate: u32 },
}

/// Turns audio frames into mono samples (-1.0 to 1.0).
pub struct Decoder {
    aac: Option<symphonia_codec_aac::AacDecoder>,
    rate: u32,
    scratch: Vec<f32>,
}

impl Decoder {
    pub fn new(codec: &AudioCodec) -> Result<Decoder> {
        match codec {
            AudioCodec::Flac16 { rate } => Ok(Decoder { aac: None, rate: *rate, scratch: Vec::new() }),
            AudioCodec::Aac { config } => {
                use symphonia_core::codecs::audio::well_known::CODEC_ID_AAC;
                use symphonia_core::codecs::audio::{AudioCodecParameters, AudioDecoder, AudioDecoderOptions};
                let mut p = AudioCodecParameters::new();
                p.for_codec(CODEC_ID_AAC);
                p.extra_data = Some(config.clone().into_boxed_slice());
                let d = symphonia_codec_aac::AacDecoder::try_new(&p, &AudioDecoderOptions::default())
                    .map_err(|e| anyhow!("this camera's sound cannot be decoded: {e}"))?;
                let rate = d.codec_params().sample_rate.unwrap_or(0);
                if rate == 0 {
                    return Err(anyhow!("this camera's sound has no sample rate"));
                }
                Ok(Decoder { aac: Some(d), rate, scratch: Vec::new() })
            }
        }
    }

    /// Samples per second of what `decode` returns.
    pub fn rate(&self) -> u32 {
        self.rate
    }

    /// One frame to mono samples, appended to `out`.
    pub fn decode(&mut self, data: &[u8], out: &mut Vec<f32>) -> Result<()> {
        match self.aac.as_mut() {
            None => {
                let s = crate::ingest::g711::flac_frame_samples(data).ok_or_else(|| anyhow!("unreadable sound frame"))?;
                out.extend(s.iter().map(|&x| f32::from(x) / 32768.0));
            }
            Some(d) => {
                use symphonia_core::codecs::audio::AudioDecoder;
                use symphonia_core::packet::PacketRef;
                let pkt = PacketRef::new(0, 0i64.into(), 0u64.into(), data);
                let buf = d.decode_ref(&pkt).map_err(|e| anyhow!("sound frame: {e}"))?;
                let ch = buf.num_planes().max(1);
                self.scratch.clear();
                buf.copy_to_vec_interleaved::<f32>(&mut self.scratch);
                // Mix the channels down to one.
                out.extend(self.scratch.chunks(ch).map(|c| c.iter().sum::<f32>() / ch as f32));
            }
        }
        Ok(())
    }
}

/// Loudness of samples in dBFS (a full-scale sine is about -3).
#[cfg(test)]
pub fn level_db(samples: &[f32]) -> f32 {
    if samples.is_empty() {
        return SILENT_DB;
    }
    let ms = samples.iter().map(|&x| f64::from(x) * f64::from(x)).sum::<f64>() / samples.len() as f64;
    if ms <= 0.0 {
        return SILENT_DB;
    }
    ((10.0 * ms.log10()) as f32).max(SILENT_DB)
}

/// What digital silence reads as.
pub const SILENT_DB: f32 = -100.0;

/// When a sound counts as loud. Defaults: 20 dB over the background,
/// at least -45 dBFS, for 200 ms; then quiet for 10 s before another.
#[derive(Clone, Copy, Debug)]
pub struct TriggerSettings {
    pub margin_db: f32,
    pub min_db: f32,
    pub min_ms: u32,
    pub cooldown_ms: u32,
}

impl Default for TriggerSettings {
    fn default() -> Self {
        TriggerSettings { margin_db: 20.0, min_db: -45.0, min_ms: 200, cooldown_ms: 10_000 }
    }
}

/// What the latest audio amounted to.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Heard {
    Nothing,
    /// A new loud sound: start an event.
    Start,
    /// The loud sound goes on (keeps its event open).
    Still,
}

/// Measuring window (100 ms).
const WINDOW_MS: u32 = 100;
/// How quickly the background follows: quiet windows within ~8 s, loud
/// ones within ~60 s (so a sound that goes on and on, rain or a running
/// machine, becomes the new normal instead of an endless event).
const BG_QUIET_MS: f32 = 8_000.0;
const BG_LOUD_MS: f32 = 60_000.0;
/// Listen this long before judging anything (the background must settle).
const WARMUP_MS: u32 = 2_000;
/// A loud sound keeps its event open for at most this long.
const MAX_STILL_MS: u32 = 60_000;

/// The loud-sound detector for one camera.
pub struct Loudness {
    pub settings: TriggerSettings,
    win: usize,
    acc_sq: f64,
    acc_n: usize,
    /// Latest 100 ms level and the background (dBFS).
    pub level: f32,
    pub background: Option<f32>,
    heard_ms: u32,
    loud_ms: u32,
    /// Since the last loud window (counts up; large = long quiet).
    quiet_ms: u32,
    /// Since the current loud sound started (None: none going on).
    since_start: Option<u32>,
}

impl Loudness {
    pub fn new(rate: u32, settings: TriggerSettings) -> Loudness {
        Loudness {
            settings,
            win: ((rate as usize) * WINDOW_MS as usize / 1000).max(1),
            acc_sq: 0.0,
            acc_n: 0,
            level: SILENT_DB,
            background: None,
            heard_ms: 0,
            loud_ms: 0,
            quiet_ms: u32::MAX / 2,
            since_start: None,
        }
    }

    /// The level that counts as loud right now.
    pub fn threshold(&self) -> Option<f32> {
        self.background.map(|b| (b + self.settings.margin_db).max(self.settings.min_db))
    }

    /// Feed samples; Start if a new loud sound began in them.
    pub fn push(&mut self, samples: &[f32]) -> Heard {
        let mut out = Heard::Nothing;
        for &x in samples {
            self.acc_sq += f64::from(x) * f64::from(x);
            self.acc_n += 1;
            if self.acc_n < self.win {
                continue;
            }
            let ms = self.acc_sq / self.acc_n as f64;
            self.acc_sq = 0.0;
            self.acc_n = 0;
            let level = if ms > 0.0 { ((10.0 * ms.log10()) as f32).max(SILENT_DB) } else { SILENT_DB };
            match self.window(level) {
                Heard::Start => out = Heard::Start,
                Heard::Still if out == Heard::Nothing => out = Heard::Still,
                _ => {}
            }
        }
        out
    }

    /// One 100 ms window.
    fn window(&mut self, level: f32) -> Heard {
        self.level = level;
        self.heard_ms = self.heard_ms.saturating_add(WINDOW_MS);
        let Some(bg) = self.background else {
            self.background = Some(level);
            return Heard::Nothing;
        };
        let loud = self.heard_ms > WARMUP_MS && level >= bg + self.settings.margin_db && level >= self.settings.min_db;
        // The background follows slowly, and more slowly still while loud.
        let tau = if loud || level >= bg + self.settings.margin_db { BG_LOUD_MS } else { BG_QUIET_MS };
        let a = WINDOW_MS as f32 / tau;
        self.background = Some(bg + a * (level - bg));
        if loud {
            self.loud_ms += WINDOW_MS;
            self.quiet_ms = 0;
        } else {
            self.loud_ms = 0;
            self.quiet_ms = self.quiet_ms.saturating_add(WINDOW_MS);
        }
        if let Some(t) = self.since_start.as_mut() {
            *t = t.saturating_add(WINDOW_MS);
            if self.quiet_ms >= self.settings.cooldown_ms {
                self.since_start = None;
            } else {
                return if loud && *t <= MAX_STILL_MS { Heard::Still } else { Heard::Nothing };
            }
        }
        if self.loud_ms >= self.settings.min_ms.max(WINDOW_MS) {
            self.since_start = Some(0);
            return Heard::Start;
        }
        Heard::Nothing
    }
}

/// The last few seconds of sound (to name what was heard).
pub struct Recent {
    buf: VecDeque<f32>,
    cap: usize,
}

impl Recent {
    pub fn new(cap: usize) -> Recent {
        Recent { buf: VecDeque::with_capacity(cap), cap }
    }
    pub fn push(&mut self, s: &[f32]) {
        for &x in s {
            if self.buf.len() == self.cap {
                self.buf.pop_front();
            }
            self.buf.push_back(x);
        }
    }
    /// The newest `n` samples (fewer if not heard yet).
    pub fn last(&self, n: usize) -> Vec<f32> {
        self.buf.iter().skip(self.buf.len().saturating_sub(n)).copied().collect()
    }
}

/// Change the sample rate (straight-line between samples: plenty for
/// telling a dog from a doorbell).
pub fn resample(s: &[f32], from: u32, to: u32) -> Vec<f32> {
    if from == to || s.is_empty() || from == 0 {
        return s.to_vec();
    }
    let n = (s.len() as u64 * u64::from(to) / u64::from(from)) as usize;
    let step = f64::from(from) / f64::from(to);
    (0..n)
        .map(|i| {
            let p = i as f64 * step;
            let j = p as usize;
            let f = (p - j as f64) as f32;
            let a = s[j.min(s.len() - 1)];
            let b = s[(j + 1).min(s.len() - 1)];
            a + (b - a) * f
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    const RATE: u32 = 8000;

    /// Deterministic noise at about `db` dBFS.
    fn noise(ms: u32, db: f32, seed: &mut u32) -> Vec<f32> {
        let amp = 10f32.powf(db / 20.0) * 3f32.sqrt();
        (0..RATE * ms / 1000)
            .map(|_| {
                *seed = seed.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
                amp * ((*seed >> 8) as f32 / (1u32 << 24) as f32 * 2.0 - 1.0)
            })
            .collect()
    }

    fn run(l: &mut Loudness, s: &[f32]) -> Vec<Heard> {
        // In 20 ms packets, like a camera sends them.
        s.chunks((RATE / 50) as usize).map(|c| l.push(c)).filter(|h| *h != Heard::Nothing).collect()
    }

    #[test]
    fn levels() {
        assert_eq!(level_db(&[0.0; 100]), SILENT_DB);
        let sine: Vec<f32> = (0..8000).map(|i| (i as f32 * 0.1).sin()).collect();
        assert!((level_db(&sine) + 3.01).abs() < 0.1);
        let mut seed = 1;
        assert!((level_db(&noise(1000, -40.0, &mut seed)) + 40.0).abs() < 0.5);
    }

    #[test]
    fn silence_and_steady_noise_never_trigger() {
        let mut l = Loudness::new(RATE, TriggerSettings::default());
        assert!(run(&mut l, &vec![0.0; RATE as usize * 10]).is_empty());
        let mut l = Loudness::new(RATE, TriggerSettings::default());
        let mut seed = 7;
        assert!(run(&mut l, &noise(30_000, -30.0, &mut seed)).is_empty());
        assert!((l.background.unwrap() + 30.0).abs() < 1.0);
    }

    #[test]
    fn a_burst_triggers_once() {
        let mut l = Loudness::new(RATE, TriggerSettings::default());
        let mut seed = 3;
        let mut s = noise(5_000, -60.0, &mut seed);
        s.extend(noise(1_000, -20.0, &mut seed));
        s.extend(noise(5_000, -60.0, &mut seed));
        let h = run(&mut l, &s);
        assert_eq!(h.first(), Some(&Heard::Start));
        assert_eq!(h.iter().filter(|x| **x == Heard::Start).count(), 1);
        // Too short (100 ms < 200 ms): nothing.
        let mut l = Loudness::new(RATE, TriggerSettings::default());
        let mut s = noise(5_000, -60.0, &mut seed);
        s.extend(noise(100, -20.0, &mut seed));
        s.extend(noise(2_000, -60.0, &mut seed));
        assert!(run(&mut l, &s).is_empty());
        // Loud for the background, but under the floor: nothing.
        let mut l = Loudness::new(RATE, TriggerSettings::default());
        let mut s = noise(5_000, -90.0, &mut seed);
        s.extend(noise(1_000, -55.0, &mut seed));
        assert!(run(&mut l, &s).is_empty());
        // Nothing during warm-up.
        let mut l = Loudness::new(RATE, TriggerSettings::default());
        let mut s = noise(500, -60.0, &mut seed);
        s.extend(noise(1_000, -20.0, &mut seed));
        assert!(run(&mut l, &s).is_empty());
    }

    #[test]
    fn cooldown_and_long_noise() {
        let mut seed = 5;
        let quiet = |ms, seed: &mut u32| noise(ms, -60.0, seed);
        let bang = |seed: &mut u32| noise(400, -15.0, seed);
        // Two bangs 3 s apart are one sound; after 10 s of quiet, a new one.
        let mut l = Loudness::new(RATE, TriggerSettings::default());
        let mut s = quiet(5_000, &mut seed);
        s.extend(bang(&mut seed));
        s.extend(quiet(3_000, &mut seed));
        s.extend(bang(&mut seed));
        s.extend(quiet(11_000, &mut seed));
        s.extend(bang(&mut seed));
        let h = run(&mut l, &s);
        assert_eq!(h.iter().filter(|x| **x == Heard::Start).count(), 2, "{h:?}");
        assert!(h.contains(&Heard::Still));
        // A noise that goes on for minutes is one sound, and becomes the background.
        let mut l = Loudness::new(RATE, TriggerSettings::default());
        let mut s = quiet(5_000, &mut seed);
        s.extend(noise(240_000, -25.0, &mut seed));
        let h = run(&mut l, &s);
        assert_eq!(h.iter().filter(|x| **x == Heard::Start).count(), 1);
        assert!(l.background.unwrap() > -30.0, "{:?}", l.background);
    }

    #[test]
    fn margin_is_respected() {
        let mut seed = 9;
        let mut s = noise(5_000, -50.0, &mut seed);
        s.extend(noise(1_000, -35.0, &mut seed)); // 15 dB over
        let mut l = Loudness::new(RATE, TriggerSettings::default());
        assert!(run(&mut l, &s).is_empty());
        let mut l = Loudness::new(RATE, TriggerSettings { margin_db: 10.0, ..Default::default() });
        assert_eq!(run(&mut l, &s).first(), Some(&Heard::Start));
    }

    #[test]
    fn resampling() {
        let s: Vec<f32> = (0..8000).map(|i| (i as f32 / 8000.0 * 440.0 * std::f32::consts::TAU).sin()).collect();
        let r = resample(&s, 8000, 16000);
        assert_eq!(r.len(), 16000);
        assert!((level_db(&r) - level_db(&s)).abs() < 0.2);
        assert_eq!(resample(&s, 16000, 16000).len(), 8000);
        let mut rc = Recent::new(4);
        rc.push(&[1.0, 2.0, 3.0, 4.0, 5.0]);
        assert_eq!(rc.last(2), vec![4.0, 5.0]);
        assert_eq!(rc.last(9), vec![2.0, 3.0, 4.0, 5.0]);
    }

    /// A real AAC-LC stream (0.5 s of 1 kHz tone, 16 kHz mono, made with
    /// ffmpeg) decodes to the tone at the right level.
    #[test]
    fn decodes_aac() {
        let adts = include_bytes!("test_sine.aac");
        // ADTS header -> AudioSpecificConfig (object type, rate index, channels).
        let profile = (adts[2] >> 6) + 1;
        let rate_idx = (adts[2] >> 2) & 0x0f;
        let chans = ((adts[2] & 1) << 2) | (adts[3] >> 6);
        let asc = vec![(profile << 3) | (rate_idx >> 1), ((rate_idx & 1) << 7) | (chans << 3)];
        let mut d = Decoder::new(&AudioCodec::Aac { config: asc }).unwrap();
        assert_eq!(d.rate(), 16000);
        let mut out = Vec::new();
        let mut i = 0;
        while i + 7 <= adts.len() {
            let len = (usize::from(adts[i + 3] & 3) << 11) | (usize::from(adts[i + 4]) << 3) | usize::from(adts[i + 5] >> 5);
            let hdr = if adts[i + 1] & 1 == 0 { 9 } else { 7 };
            d.decode(&adts[i + hdr..i + len], &mut out).unwrap();
            i += len;
        }
        assert!(out.len() >= 7000, "{}", out.len());
        // The middle part: 1/8 full scale (ffmpeg's sine), less 6 dB = -27 dBFS RMS.
        let mid = &out[2000..6000];
        assert!((level_db(mid) + 27.1).abs() < 1.5, "{}", level_db(mid));
        // It is a 1 kHz tone: about 2 zero crossings per ms.
        let zc = mid.windows(2).filter(|w| (w[0] < 0.0) != (w[1] < 0.0)).count();
        assert!((480..=520).contains(&zc), "{zc}");
        // Loud enough to trigger after a quiet start.
        let mut l = Loudness::new(16000, TriggerSettings::default());
        l.push(&vec![0.00001; 16000 * 3]);
        assert_eq!(l.push(mid), Heard::Start);
    }

    #[test]
    fn decodes_g711_flac() {
        let s: Vec<i16> = (0..160).map(|i| if i % 2 == 0 { 16384 } else { -16384 }).collect();
        let mut d = Decoder::new(&AudioCodec::Flac16 { rate: 8000 }).unwrap();
        let mut out = Vec::new();
        d.decode(&crate::ingest::g711::flac_frame(&s, 0, 8000), &mut out).unwrap();
        assert_eq!(out.len(), 160);
        assert!((level_db(&out) + 6.02).abs() < 0.1);
        assert!(d.decode(&[0u8; 20], &mut out).is_err());
    }
}
