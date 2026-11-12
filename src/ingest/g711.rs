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
}

impl ToFlac {
    pub fn new(law: Law, rate: u32) -> ToFlac {
        ToFlac { law, rate, next: 0 }
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
