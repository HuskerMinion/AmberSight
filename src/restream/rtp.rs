//! RTP packetization for the shared RTSP streams: H.264 (RFC 6184), H.265
//! (RFC 7798) and AAC (RFC 3640, AAC-hbr), plus RTCP sender reports and the
//! SDP that describes a stream. Everything here is pure (no I/O).

use base64::Engine as _;

use super::hub::StreamInfo;

/// Largest RTP payload we make: fits an Ethernet frame with headers, so a
/// stream relayed onward over UDP by another app is not fragmented.
pub const MAX_PAYLOAD: usize = 1400;
pub const VIDEO_PT: u8 = 96;
pub const AUDIO_PT: u8 = 97;

/// The NAL units of a frame stored with length prefixes (`len_size` bytes,
/// big endian). A broken length ends the list.
pub fn split_nals(data: &[u8], len_size: usize) -> Vec<&[u8]> {
    let mut out = Vec::new();
    let mut i = 0;
    while len_size > 0 && len_size <= 4 && i + len_size <= data.len() {
        let n = data[i..i + len_size].iter().fold(0usize, |a, b| (a << 8) | *b as usize);
        i += len_size;
        if n == 0 || i + n > data.len() {
            break;
        }
        out.push(&data[i..i + n]);
        i += n;
    }
    out
}

/// H.264 NAL type 7 / 8 (SPS, PPS), or H.265 32 / 33 / 34 (VPS, SPS, PPS).
pub fn is_param_set(nal: &[u8], h265: bool) -> bool {
    match nal.first() {
        None => false,
        Some(b) if h265 => matches!((b >> 1) & 0x3f, 32..=34),
        Some(b) => matches!(b & 0x1f, 7 | 8),
    }
}

/// RTP payloads for one H.264 access unit: single NAL unit packets, and
/// FU-A fragments for NAL units larger than `max`.
pub fn packetize_h264(nals: &[&[u8]], max: usize) -> Vec<Vec<u8>> {
    let mut out = Vec::new();
    for nal in nals.iter().filter(|n| !n.is_empty()) {
        if nal.len() <= max {
            out.push(nal.to_vec());
            continue;
        }
        let (ind, typ) = ((nal[0] & 0xe0) | 28, nal[0] & 0x1f);
        let body = &nal[1..];
        let chunks = body.chunks(max - 2).count();
        for (i, c) in body.chunks(max - 2).enumerate() {
            let flags = if i == 0 { 0x80 } else { 0 } | if i + 1 == chunks { 0x40 } else { 0 };
            let mut p = Vec::with_capacity(c.len() + 2);
            p.extend_from_slice(&[ind, flags | typ]);
            p.extend_from_slice(c);
            out.push(p);
        }
    }
    out
}

/// RTP payloads for one H.265 access unit: single NAL unit packets and FU
/// (type 49) fragments.
pub fn packetize_h265(nals: &[&[u8]], max: usize) -> Vec<Vec<u8>> {
    let mut out = Vec::new();
    for nal in nals.iter().filter(|n| n.len() > 2) {
        if nal.len() <= max {
            out.push(nal.to_vec());
            continue;
        }
        let typ = (nal[0] >> 1) & 0x3f;
        let (h0, h1) = ((nal[0] & 0x81) | (49 << 1), nal[1]);
        let body = &nal[2..];
        let chunks = body.chunks(max - 3).count();
        for (i, c) in body.chunks(max - 3).enumerate() {
            let flags = if i == 0 { 0x80 } else { 0 } | if i + 1 == chunks { 0x40 } else { 0 };
            let mut p = Vec::with_capacity(c.len() + 3);
            p.extend_from_slice(&[h0, h1, flags | typ]);
            p.extend_from_slice(c);
            out.push(p);
        }
    }
    out
}

/// The RTP payload for one AAC frame (one access unit, AAC-hbr: 13 bit
/// size, 3 bit index). None for a frame that does not fit the size field.
pub fn packetize_aac(frame: &[u8]) -> Option<Vec<u8>> {
    if frame.is_empty() || frame.len() > 0x1fff {
        return None;
    }
    let size = frame.len() as u16;
    let mut p = Vec::with_capacity(frame.len() + 4);
    // AU-headers-length: 16 bits (one header).
    p.extend_from_slice(&[0, 16, (size >> 5) as u8, ((size & 0x1f) << 3) as u8]);
    p.extend_from_slice(frame);
    Some(p)
}

/// A complete RTP packet.
pub fn rtp_packet(pt: u8, marker: bool, seq: u16, ts: u32, ssrc: u32, payload: &[u8]) -> Vec<u8> {
    let mut p = Vec::with_capacity(12 + payload.len());
    p.push(0x80);
    p.push(pt | if marker { 0x80 } else { 0 });
    p.extend_from_slice(&seq.to_be_bytes());
    p.extend_from_slice(&ts.to_be_bytes());
    p.extend_from_slice(&ssrc.to_be_bytes());
    p.extend_from_slice(payload);
    p
}

/// Append `pkt` to `out` as an interleaved RTSP frame (`$`, channel, length).
pub fn interleave(out: &mut Vec<u8>, channel: u8, pkt: &[u8]) {
    out.push(b'$');
    out.push(channel);
    out.extend_from_slice(&(pkt.len() as u16).to_be_bytes());
    out.extend_from_slice(pkt);
}

/// An RTCP compound packet: sender report plus source description (CNAME).
/// `wall` is the sender's clock as Unix seconds, `ts` the RTP timestamp that
/// belongs to it.
pub fn rtcp_sender_report(ssrc: u32, wall: f64, ts: u32, packets: u32, octets: u32) -> Vec<u8> {
    let secs = wall.max(0.0);
    let ntp_sec = (secs.floor() as u64 + 2_208_988_800) as u32;
    let ntp_frac = (secs.fract() * 4_294_967_296.0) as u32;
    let mut p = Vec::with_capacity(52);
    p.extend_from_slice(&[0x80, 200, 0, 6]);
    p.extend_from_slice(&ssrc.to_be_bytes());
    p.extend_from_slice(&ntp_sec.to_be_bytes());
    p.extend_from_slice(&ntp_frac.to_be_bytes());
    p.extend_from_slice(&ts.to_be_bytes());
    p.extend_from_slice(&packets.to_be_bytes());
    p.extend_from_slice(&octets.to_be_bytes());
    // SDES: one chunk, CNAME "ambersight", end marker, padded to a multiple of 4.
    p.extend_from_slice(&[0x81, 202, 0, 5]);
    p.extend_from_slice(&ssrc.to_be_bytes());
    p.extend_from_slice(&[1, 10]);
    p.extend_from_slice(b"ambersight");
    p.extend_from_slice(&[0, 0, 0, 0]);
    p
}

/// `profile-level-id` (hex) from an H.264 SPS NAL unit.
pub fn profile_level_id(sps: &[u8]) -> Option<String> {
    (sps.len() >= 4).then(|| format!("{:02x}{:02x}{:02x}", sps[1], sps[2], sps[3]))
}

/// The parameter sets and NAL length size in an AVCDecoderConfigurationRecord:
/// (SPS and PPS NAL units in order, length size).
pub fn avcc_param_sets(avcc: &[u8]) -> Option<(Vec<Vec<u8>>, usize)> {
    if avcc.len() < 7 || avcc[0] != 1 {
        return None;
    }
    let len_size = (avcc[4] & 3) as usize + 1;
    let mut i = 5;
    let mut out = Vec::new();
    let take = |i: &mut usize, count: usize, out: &mut Vec<Vec<u8>>| -> Option<()> {
        for _ in 0..count {
            let n = u16::from_be_bytes([*avcc.get(*i)?, *avcc.get(*i + 1)?]) as usize;
            out.push(avcc.get(*i + 2..*i + 2 + n)?.to_vec());
            *i += 2 + n;
        }
        Some(())
    };
    let n_sps = (avcc[i] & 0x1f) as usize;
    i += 1;
    take(&mut i, n_sps, &mut out)?;
    let n_pps = *avcc.get(i)? as usize;
    i += 1;
    take(&mut i, n_pps, &mut out)?;
    Some((out, len_size))
}

/// The parameter sets (VPS, SPS, PPS, in the order stored) and NAL length
/// size in an HEVCDecoderConfigurationRecord.
pub fn hvcc_param_sets(hvcc: &[u8]) -> Option<(Vec<Vec<u8>>, usize)> {
    if hvcc.len() < 23 {
        return None;
    }
    let len_size = (hvcc[21] & 3) as usize + 1;
    let arrays = hvcc[22] as usize;
    let mut i = 23;
    let mut out = Vec::new();
    for _ in 0..arrays {
        let typ = hvcc.get(i)? & 0x3f;
        let count = u16::from_be_bytes([*hvcc.get(i + 1)?, *hvcc.get(i + 2)?]) as usize;
        i += 3;
        for _ in 0..count {
            let n = u16::from_be_bytes([*hvcc.get(i)?, *hvcc.get(i + 1)?]) as usize;
            let nal = hvcc.get(i + 2..i + 2 + n)?;
            if matches!(typ, 32..=34) {
                out.push(nal.to_vec());
            }
            i += 2 + n;
        }
    }
    Some((out, len_size))
}

fn b64(d: &[u8]) -> String {
    base64::engine::general_purpose::STANDARD.encode(d)
}

fn hex(d: &[u8]) -> String {
    d.iter().map(|b| format!("{b:02x}")).collect()
}

/// A one-line session name safe for SDP (no line breaks or control characters).
fn clean_line(s: &str) -> String {
    let t: String = s.chars().map(|c| if c.is_control() { ' ' } else { c }).collect();
    let t = t.trim();
    if t.is_empty() { "AmberSight".to_string() } else { t.to_string() }
}

/// The SDP for a stream. Track controls are relative (`trackID=0` video,
/// `trackID=1` audio), resolved against the Content-Base of the DESCRIBE answer.
pub fn sdp(name: &str, info: &StreamInfo, session_id: u64) -> String {
    let v = &info.video;
    let mut s = String::new();
    s += "v=0\r\n";
    s += &format!("o=- {session_id} 1 IN IP4 0.0.0.0\r\n");
    s += &format!("s={}\r\n", clean_line(name));
    s += "c=IN IP4 0.0.0.0\r\nt=0 0\r\na=control:*\r\na=range:npt=now-\r\n";
    s += &format!("m=video 0 RTP/AVP {VIDEO_PT}\r\n");
    s += &format!("a=rtpmap:{VIDEO_PT} {}/{}\r\n", if v.h265 { "H265" } else { "H264" }, v.clock_rate);
    if v.h265 {
        let by = |t: u8| -> Option<String> {
            let l: Vec<String> = v.param_sets.iter().filter(|n| n.first().is_some_and(|b| (b >> 1) & 0x3f == t)).map(|n| b64(n)).collect();
            (!l.is_empty()).then(|| l.join(","))
        };
        let mut f = Vec::new();
        for (key, t) in [("sprop-vps", 32), ("sprop-sps", 33), ("sprop-pps", 34)] {
            if let Some(x) = by(t) {
                f.push(format!("{key}={x}"));
            }
        }
        if !f.is_empty() {
            s += &format!("a=fmtp:{VIDEO_PT} {}\r\n", f.join(";"));
        }
    } else {
        let sets: Vec<String> = v.param_sets.iter().map(|n| b64(n)).collect();
        let pli = v.param_sets.iter().find(|n| n.first().is_some_and(|b| b & 0x1f == 7)).and_then(|n| profile_level_id(n));
        let mut f = vec!["packetization-mode=1".to_string()];
        if let Some(p) = pli {
            f.push(format!("profile-level-id={p}"));
        }
        if !sets.is_empty() {
            f.push(format!("sprop-parameter-sets={}", sets.join(",")));
        }
        s += &format!("a=fmtp:{VIDEO_PT} {}\r\n", f.join(";"));
    }
    s += "a=control:trackID=0\r\n";
    if let Some(a) = &info.audio {
        s += &format!("m=audio 0 RTP/AVP {AUDIO_PT}\r\n");
        s += &format!("a=rtpmap:{AUDIO_PT} mpeg4-generic/{}/{}\r\n", a.rate, a.channels);
        s += &format!(
            "a=fmtp:{AUDIO_PT} streamtype=5;profile-level-id=1;mode=AAC-hbr;sizelength=13;indexlength=3;indexdeltalength=3;config={}\r\n",
            hex(&a.config)
        );
        s += "a=control:trackID=1\r\n";
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::restream::hub::{AudioInfo, StreamInfo, VideoInfo};

    fn lp(nals: &[&[u8]]) -> Vec<u8> {
        let mut v = Vec::new();
        for n in nals {
            v.extend_from_slice(&(n.len() as u32).to_be_bytes());
            v.extend_from_slice(n);
        }
        v
    }

    #[test]
    fn splits_length_prefixed_nals() {
        let d = lp(&[&[0x65, 1, 2], &[0x41, 9]]);
        assert_eq!(split_nals(&d, 4), vec![&[0x65u8, 1, 2][..], &[0x41, 9][..]]);
        // A length running past the data ends the list.
        let mut bad = d.clone();
        bad.extend_from_slice(&[0, 0, 0, 200, 1]);
        assert_eq!(split_nals(&bad, 4).len(), 2);
        assert!(split_nals(&[], 4).is_empty());
    }

    #[test]
    fn h264_small_nals_are_single_packets() {
        let p = packetize_h264(&[&[0x67, 1, 2, 3], &[0x68, 4]], 1400);
        assert_eq!(p, vec![vec![0x67, 1, 2, 3], vec![0x68, 4]]);
    }

    #[test]
    fn h264_large_nal_is_fragmented_and_reassembles() {
        let mut nal = vec![0x65u8];
        nal.extend((0..3000u32).map(|i| (i % 251) as u8));
        let p = packetize_h264(&[&nal], 1400);
        assert_eq!(p.len(), 3);
        assert!(p.iter().all(|x| x.len() <= 1400));
        // FU indicator keeps NRI (3) and has type 28.
        assert_eq!(p[0][0], (0x65 & 0xe0) | 28);
        assert_eq!(p[0][1], 0x80 | 5);
        assert_eq!(p[1][1], 5);
        assert_eq!(p[2][1], 0x40 | 5);
        let mut re = vec![(p[0][0] & 0xe0) | (p[0][1] & 0x1f)];
        for x in &p {
            re.extend_from_slice(&x[2..]);
        }
        assert_eq!(re, nal);
    }

    #[test]
    fn h265_large_nal_is_fragmented_and_reassembles() {
        // IDR_W_RADL = 19
        let mut nal = vec![19 << 1, 1u8];
        nal.extend((0..4000u32).map(|i| (i % 253) as u8));
        let p = packetize_h265(&[&nal], 1400);
        assert_eq!(p.len(), 3);
        assert_eq!((p[0][0] >> 1) & 0x3f, 49);
        assert_eq!(p[0][1], 1);
        assert_eq!(p[0][2], 0x80 | 19);
        assert_eq!(p[2][2], 0x40 | 19);
        let mut re = vec![(p[0][0] & 0x81) | ((p[0][2] & 0x3f) << 1), p[0][1]];
        for x in &p {
            re.extend_from_slice(&x[3..]);
        }
        assert_eq!(re, nal);
    }

    #[test]
    fn aac_payload_has_au_header() {
        let f = vec![7u8; 300];
        let p = packetize_aac(&f).unwrap();
        assert_eq!(&p[..2], &[0, 16]);
        let au = u16::from_be_bytes([p[2], p[3]]);
        assert_eq!(au >> 3, 300);
        assert_eq!(au & 7, 0);
        assert_eq!(&p[4..], &f[..]);
        assert!(packetize_aac(&[]).is_none());
        assert!(packetize_aac(&vec![0; 9000]).is_none());
    }

    #[test]
    fn rtp_header_and_interleave() {
        let p = rtp_packet(96, true, 0x1234, 0xdeadbeef, 0x01020304, &[9, 9]);
        assert_eq!(&p[..12], &[0x80, 0xe0, 0x12, 0x34, 0xde, 0xad, 0xbe, 0xef, 1, 2, 3, 4]);
        let mut o = Vec::new();
        interleave(&mut o, 2, &p);
        assert_eq!(&o[..4], &[b'$', 2, 0, 14]);
        assert_eq!(&o[4..], &p[..]);
    }

    #[test]
    fn rtcp_sender_report_layout() {
        let r = rtcp_sender_report(7, 1.5, 1000, 3, 400);
        assert_eq!(r.len() % 4, 0);
        assert_eq!(r[1], 200);
        // 28 byte SR + 24 byte SDES.
        assert_eq!(r.len(), 52);
        assert_eq!(u32::from_be_bytes(r[8..12].try_into().unwrap()), 2_208_988_801);
        assert_eq!(u32::from_be_bytes(r[12..16].try_into().unwrap()), 2_147_483_648);
        assert_eq!(r[29], 202);
        // SDES length field counts words after the first.
        assert_eq!(u16::from_be_bytes([r[30], r[31]]) as usize * 4 + 4, 24);
    }

    fn avcc() -> Vec<u8> {
        let sps = [0x67u8, 0x64, 0x00, 0x1f, 0xac];
        let pps = [0x68u8, 0xee, 0x3c, 0x80];
        let mut v = vec![1, 0x64, 0, 0x1f, 0xff, 0xe1];
        v.extend_from_slice(&(sps.len() as u16).to_be_bytes());
        v.extend_from_slice(&sps);
        v.push(1);
        v.extend_from_slice(&(pps.len() as u16).to_be_bytes());
        v.extend_from_slice(&pps);
        v
    }

    #[test]
    fn reads_avcc_parameter_sets() {
        let (sets, ls) = avcc_param_sets(&avcc()).unwrap();
        assert_eq!(ls, 4);
        assert_eq!(sets, vec![vec![0x67, 0x64, 0, 0x1f, 0xac], vec![0x68, 0xee, 0x3c, 0x80]]);
        assert!(avcc_param_sets(&[1, 2]).is_none());
        // Truncated record.
        assert!(avcc_param_sets(&avcc()[..10]).is_none());
    }

    #[test]
    fn reads_hvcc_parameter_sets() {
        let mut v = vec![0u8; 22];
        v[0] = 1;
        v[21] = 0xff;
        v.push(3);
        for (t, nal) in [(32u8, vec![0x40u8, 1, 0xaa]), (33, vec![0x42, 1, 0xbb]), (34, vec![0x44, 1, 0xcc])] {
            v.push(0x80 | t);
            v.extend_from_slice(&1u16.to_be_bytes());
            v.extend_from_slice(&(nal.len() as u16).to_be_bytes());
            v.extend_from_slice(&nal);
        }
        let (sets, ls) = hvcc_param_sets(&v).unwrap();
        assert_eq!(ls, 4);
        assert_eq!(sets.len(), 3);
        assert_eq!(sets[1], vec![0x42, 1, 0xbb]);
    }

    fn info(h265: bool, audio: bool) -> StreamInfo {
        StreamInfo {
            video: VideoInfo {
                h265,
                clock_rate: 90000,
                width: 1920,
                height: 1080,
                len_size: 4,
                param_sets: if h265 {
                    vec![vec![0x40, 1, 0xaa], vec![0x42, 1, 0xbb], vec![0x44, 1, 0xcc]]
                } else {
                    avcc_param_sets(&avcc()).unwrap().0
                },
            },
            audio: audio.then(|| AudioInfo { config: vec![0x12, 0x10], rate: 44100, channels: 2 }),
            generation: 1,
        }
    }

    #[test]
    fn sdp_for_h264_with_audio() {
        let s = sdp("Front Yard", &info(false, true), 42);
        assert!(s.starts_with("v=0\r\no=- 42 1 IN IP4 0.0.0.0\r\ns=Front Yard\r\n"));
        assert!(s.contains("m=video 0 RTP/AVP 96\r\na=rtpmap:96 H264/90000\r\n"));
        assert!(s.contains("a=fmtp:96 packetization-mode=1;profile-level-id=64001f;sprop-parameter-sets=Z2QAH6w=,aO48gA==\r\n"));
        assert!(s.contains("a=control:trackID=0\r\n"));
        assert!(s.contains("m=audio 0 RTP/AVP 97\r\na=rtpmap:97 mpeg4-generic/44100/2\r\n"));
        assert!(s.contains("mode=AAC-hbr;sizelength=13;indexlength=3;indexdeltalength=3;config=1210\r\n"));
        assert!(s.contains("a=control:trackID=1\r\n"));
        assert!(s.ends_with("\r\n"));
    }

    #[test]
    fn sdp_for_h265_video_only() {
        let s = sdp("x\r\ny", &info(true, false), 1);
        assert!(s.contains("s=x  y\r\n"));
        assert!(s.contains("a=rtpmap:96 H265/90000\r\n"));
        assert!(s.contains("a=fmtp:96 sprop-vps=QAGq;sprop-sps=QgG7;sprop-pps=RAHM\r\n"));
        assert!(!s.contains("m=audio"));
    }

    #[test]
    fn recognizes_parameter_set_nals() {
        assert!(is_param_set(&[0x67, 0], false));
        assert!(is_param_set(&[0x68, 0], false));
        assert!(!is_param_set(&[0x65, 0], false));
        assert!(is_param_set(&[0x42, 1], true));
        assert!(!is_param_set(&[0x26, 1], true));
        assert!(!is_param_set(&[], true));
    }
}
