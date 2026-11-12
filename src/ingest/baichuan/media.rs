//! BcMedia framing inside reassembled preview payloads. All integers are
//! little-endian. Magics on the wire (bytes in order):
//!   InfoV1 "1001", InfoV2 "1002"
//!   I-frame: [chan '0'..'9', '0', 'd', 'c'] then "H264"/"H265"
//!   P-frame: [chan '0'..'9', '1', 'd', 'c'] then "H264"/"H265"
//!   AAC "05wb", ADPCM "01wb"
//! Video header: magic(4) type(4) payload_size u32 addl_header u32
//!   microseconds u32 unknown u32  = 24 bytes, then addl_header bytes, then
//!   payload_size bytes of raw Annex-B data, then padding to a multiple of 8.
//! Info header: 32 bytes (width @8, height @12, fps @17), no payload.

use anyhow::Result;

#[derive(Default, Debug)]
pub struct Stats {
    pub iframes: usize,
    pub pframes: usize,
    pub audio: usize,
    pub info: usize,
    pub skipped_bytes: usize,
    pub codec: Option<String>,
    pub width: u32,
    pub height: u32,
    pub incomplete_tail: usize,
}

fn u32le(b: &[u8], i: usize) -> Option<u32> {
    b.get(i..i + 4).map(|s| u32::from_le_bytes(s.try_into().unwrap()))
}
fn u16le(b: &[u8], i: usize) -> Option<u16> {
    b.get(i..i + 2).map(|s| u16::from_le_bytes(s.try_into().unwrap()))
}
fn pad8(n: usize) -> usize {
    (8 - n % 8) % 8
}

fn is_video(m: &[u8]) -> Option<bool> {
    // returns Some(true)=iframe, Some(false)=pframe
    if m.len() >= 4 && m[2] == b'd' && m[3] == b'c' && (b'0'..=b'9').contains(&m[0]) {
        match m[1] {
            b'0' => Some(true),
            b'1' => Some(false),
            _ => None,
        }
    } else {
        None
    }
}

/// Given ONE media message payload, return its raw H.264/H.265 bytes if it is a
/// video frame (BcMedia header stripped), or None for audio/info frames. These
/// battery cameras send one frame per UDP message and an unreliable size field,
/// so we strip at the authoritative message boundary instead of trusting size.
/// A payload with no known magic is treated as a continuation (whole payload).
pub fn frame_video(payload: &[u8]) -> Option<&[u8]> {
    if payload.len() < 4 { return None; }
    let m = &payload[..4];
    if is_video(m).is_some() {
        let addl = u32le(payload, 12)? as usize;
        payload.get(24 + addl..)
    } else if m == b"1001" || m == b"1002" || m == b"05wb" || m == b"01wb" {
        None // info or audio
    } else {
        Some(payload) // continuation of a spanning video frame
    }
}

/// A VALIDATED frame magic at `i` — not just the 4 magic bytes, but the extra
/// check that proves it's a real frame (so coincidental byte patterns inside
/// video data don't desync us): video must carry an "H264"/"H265" type, audio
/// must carry the ADTS syncword, info must be 1001/1002.
fn valid_magic(data: &[u8], i: usize) -> bool {
    match data.get(i..i + 4) {
        Some(m) if is_video(m).is_some() => {
            matches!(data.get(i + 4..i + 8), Some(b"H264") | Some(b"H265"))
        }
        Some(b"05wb") => matches!(data.get(i + 8), Some(0xff)) // ADTS sync 0xFFFx
            && data.get(i + 9).is_some_and(|&b| b & 0xf0 == 0xf0),
        Some(b"01wb") => data.get(i + 8..i + 10) == Some(&[0x00, 0x01]),
        Some(b"1001") | Some(b"1002") => true,
        _ => false,
    }
}

/// Find the next validated frame magic strictly after `from`.
fn next_magic(data: &[u8], from: usize) -> Option<usize> {
    ((from + 1)..data.len().saturating_sub(3)).find(|&i| valid_magic(data, i))
}

/// Where the video frame at `i` (payload starting at `start`) ends:
/// `(payload_end, next_frame)`, or None while it is not provably complete.
///
/// The header's size field is trusted when the frame it describes ends exactly
/// at the end of the data or at the next validated magic. That matters: an idle,
/// motion-gated camera sends one keyframe and goes quiet, and waiting for a next
/// magic held that frame back forever (main stream: 175 KB in, 0 frames out).
/// The size field is sometimes wrong on these cameras, so a validated magic
/// before the claimed end wins, and otherwise the next magic delimits the frame.
fn video_end(data: &[u8], i: usize, start: usize) -> Option<(usize, usize)> {
    let nm = next_magic(data, start);
    if let Some(size) = u32le(data, i + 8) {
        let end = start.saturating_add(size as usize);
        for next in [end, end.saturating_add(pad8(size as usize))] {
            if next <= data.len() && (next == data.len() || valid_magic(data, next)) {
                if nm.is_some_and(|m| m < next) {
                    break; // a real boundary inside the claimed size: the size is wrong
                }
                return Some((end, next));
            }
        }
    }
    nm.map(|m| (m, m))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Build a BcMedia video frame: magic(4)+type(4)+size+addl(0)+micros+unknown
    /// then `payload`. The size field is intentionally present but we don't rely
    /// on it (the parser delimits by the next magic).
    fn vframe(chan_ic: &[u8; 4], payload: &[u8]) -> Vec<u8> {
        let mut v = Vec::new();
        v.extend_from_slice(chan_ic);          // e.g. b"00dc"
        v.extend_from_slice(b"H264");
        v.extend_from_slice(&(payload.len() as u32).to_le_bytes()); // size @8
        v.extend_from_slice(&0u32.to_le_bytes());                   // addl_header @12
        v.extend_from_slice(&0u32.to_le_bytes());                   // micros @16
        v.extend_from_slice(&0u32.to_le_bytes());                   // unknown @20
        v.extend_from_slice(payload);
        v
    }

    #[test]
    fn emits_complete_holds_back_trailing_partial() {
        // Two full frames back-to-back, then a third frame that is cut off
        // mid-payload (the capture ended). The first two are bounded by a
        // following magic and must be emitted; the third must be held back.
        let mut data = Vec::new();
        data.extend_from_slice(&vframe(b"00dc", &[0xAA; 40])); // I-frame, complete
        data.extend_from_slice(&vframe(b"01dc", &[0xBB; 40])); // P-frame, complete
        let mut cut = vframe(b"00dc", &[0xCC; 40]);            // I-frame, truncated
        cut.truncate(24 + 10);                                 // header + 10 of 40
        data.extend_from_slice(&cut);

        let mut video = Vec::new();
        let st = extract(&data, &mut video).unwrap();
        assert_eq!(st.iframes, 1, "only the first (complete) I-frame is emitted");
        assert_eq!(st.pframes, 1);
        assert_eq!(video.len(), 80, "exactly the two complete payloads, no partial tail");
        assert!(st.incomplete_tail > 0, "the truncated final frame is held back");
        assert_eq!(st.skipped_bytes, 0);
    }
}

/// Walk `data`, appending raw Annex-B video (I- and P-frame payloads) to `video`.
pub fn extract(data: &[u8], video: &mut Vec<u8>) -> Result<Stats> {
    let mut st = Stats::default();
    let mut i = 0;
    while i + 8 <= data.len() {
        if !valid_magic(data, i) {
            // Not a real frame boundary; scan to the next validated magic.
            match next_magic(data, i) {
                Some(m) => { st.skipped_bytes += m - i; i = m; continue; }
                None => break,
            }
        }
        let magic = &data[i..i + 4];
        if let Some(is_i) = is_video(magic) {
            // The size field is unreliable on these cameras, so delimit the
            // frame by the next VALIDATED magic. Data starts after the header.
            let Some(addl) = u32le(data, i + 12) else { break };
            let start = i + 24 + addl as usize;
            if start > data.len() { st.incomplete_tail = data.len() - i; break; }
            // End it at its declared size when that lands on the end of the
            // data or the next validated magic, else at the next magic. If
            // neither proves where it ends, the capture or stream was cut
            // mid-frame: HOLD IT BACK rather than emit a truncated frame that
            // decodes to a gray/garbled tail (ffmpeg "bytestream -12").
            let Some((end, next)) = video_end(data, i, start) else {
                st.incomplete_tail = data.len() - i;
                break;
            };
            if st.codec.is_none() {
                st.codec = Some(String::from_utf8_lossy(&data[i + 4..i + 8]).trim_end_matches('\0').to_string());
            }
            video.extend_from_slice(&data[start..end]);
            if is_i { st.iframes += 1 } else { st.pframes += 1 }
            i = next;
        } else if magic == b"1001" || magic == b"1002" {
            st.info += 1;
            st.width = u32le(data, i + 8).unwrap_or(0);
            st.height = u32le(data, i + 12).unwrap_or(0);
            i += 32;
        } else {
            // audio: trust the size, but if it doesn't land on a magic, resync.
            let Some(size) = u16le(data, i + 4) else { break };
            st.audio += 1;
            let adv = 8 + size as usize + pad8(size as usize);
            let nxt = i + adv;
            i = if nxt <= data.len() && (nxt == data.len() || valid_magic(data, nxt)) {
                nxt
            } else {
                next_magic(data, i).unwrap_or(data.len())
            };
        }
    }
    Ok(st)
}

/// One demuxed video access unit: its raw Annex-B bytes (start codes included),
/// whether it is a keyframe, and the BcMedia per-frame timestamp in microseconds.
#[derive(Debug, Clone, PartialEq)]
pub struct Frame {
    pub annexb: Vec<u8>,
    pub key: bool,
    pub micros: u32,
}

/// A streaming BcMedia demuxer. Feed reassembled preview bytes with [`Demux::push`]
/// and get back complete video frames as the stream delimits them. It is the
/// incremental form of [`extract`]: a frame is emitted only once the next magic
/// proves where it ends, so a partial frame at the tail of a read is held back
/// (never a truncated frame). Audio frames are dropped; Info frames update
/// `width`/`height`/`h265`.
#[derive(Default)]
pub struct Demux {
    buf: Vec<u8>,
    pub width: u16,
    pub height: u16,
    pub h265: bool,
    codec_seen: bool,
    /// When the stream's first info header says it starts, in the camera's
    /// local time. For an SD-card recording that is when the clip began (its
    /// PIR trigger); for live preview, the moment preview started.
    pub clip_start: Option<chrono::NaiveDateTime>,
}

/// The start time in an info header at `i` (bytes 18..24: years since 1900,
/// month, day, hour, minute, second), in the camera's local time.
pub fn info_start_time(data: &[u8], i: usize) -> Option<chrono::NaiveDateTime> {
    let b = data.get(i + 18..i + 24)?;
    chrono::NaiveDate::from_ymd_opt(1900 + b[0] as i32, b[1] as u32, b[2] as u32)?.and_hms_opt(b[3] as u32, b[4] as u32, b[5] as u32)
}

/// Most unparsed bytes a Demux keeps between pushes (a 4K keyframe is well under).
const MAX_HELD: usize = 16 << 20;
/// Largest extra-header length a video frame may declare (real ones are 0 or a few bytes).
const MAX_ADDL: u32 = 1024;

impl Demux {
    pub fn new() -> Self {
        Self::default()
    }

    /// Append freshly reassembled bytes and return any newly-complete frames.
    pub fn push(&mut self, bytes: &[u8]) -> Vec<Frame> {
        let mut data = std::mem::take(&mut self.buf);
        data.extend_from_slice(bytes);
        let mut out = Vec::new();
        let mut i = 0;
        while i + 8 <= data.len() {
            if !valid_magic(&data, i) {
                match next_magic(&data, i) {
                    Some(m) => {
                        i = m;
                        continue;
                    }
                    None => break,
                }
            }
            if let Some(is_i) = is_video(&data[i..i + 4]) {
                let Some(addl) = u32le(&data, i + 12) else { break };
                if addl > MAX_ADDL {
                    // Not a real frame header (a size no camera sends): resync.
                    match next_magic(&data, i) {
                        Some(m) => {
                            i = m;
                            continue;
                        }
                        None => {
                            i = data.len().saturating_sub(7);
                            break;
                        }
                    }
                }
                let micros = u32le(&data, i + 16).unwrap_or(0);
                let start = i + 24 + addl as usize;
                if start > data.len() {
                    break;
                }
                // Complete once its size (or the next magic) proves where it
                // ends; until then keep the bytes for the next push.
                let Some((end, next)) = video_end(&data, i, start) else { break };
                if !self.codec_seen {
                    self.h265 = &data[i + 4..i + 8] == b"H265";
                    self.codec_seen = true;
                }
                out.push(Frame { annexb: data[start..end].to_vec(), key: is_i, micros });
                i = next;
            } else if &data[i..i + 4] == b"1001" || &data[i..i + 4] == b"1002" {
                if i + 32 > data.len() {
                    break; // the rest of this header comes with the next push
                }
                if let Some(w) = u32le(&data, i + 8) {
                    if w > 0 {
                        self.width = w as u16;
                    }
                }
                if let Some(h) = u32le(&data, i + 12) {
                    if h > 0 {
                        self.height = h as u16;
                    }
                }
                if self.clip_start.is_none() {
                    self.clip_start = info_start_time(&data, i);
                }
                i += 32;
            } else {
                // Audio: advance by its size field, resyncing if it misses.
                let Some(size) = u16le(&data, i + 4) else { break };
                let nxt = i + 8 + size as usize + pad8(size as usize);
                i = if nxt <= data.len() && (nxt == data.len() || valid_magic(&data, nxt)) {
                    nxt
                } else {
                    match next_magic(&data, i) {
                        Some(m) => m,
                        None => break,
                    }
                };
            }
        }
        if i > 0 {
            data.drain(..i);
        }
        // Never hold more than one oversized frame's worth: data that will not
        // parse (a broken stream) is dropped instead of growing without end.
        if data.len() > MAX_HELD {
            data.clear();
        }
        self.buf = data;
        out
    }
}

#[cfg(test)]
mod demux_tests {
    use super::*;

    // A BcMedia I/P video frame: magic + "H264" + size + addl(0) + micros + unk,
    // then `payload` Annex-B bytes.
    fn vframe(iframe: bool, micros: u32, payload: &[u8]) -> Vec<u8> {
        let mut v = Vec::new();
        v.extend_from_slice(if iframe { b"00dc" } else { b"01dc" });
        v.extend_from_slice(b"H264");
        v.extend_from_slice(&(payload.len() as u32).to_le_bytes());
        v.extend_from_slice(&0u32.to_le_bytes());
        v.extend_from_slice(&micros.to_le_bytes());
        v.extend_from_slice(&0u32.to_le_bytes());
        v.extend_from_slice(payload);
        v
    }

    fn info(w: u32, h: u32) -> Vec<u8> {
        let mut v = b"1002".to_vec();
        v.extend_from_slice(&0u32.to_le_bytes()); // 4..8
        v.extend_from_slice(&w.to_le_bytes()); // width @8
        v.extend_from_slice(&h.to_le_bytes()); // height @12
        v.resize(32, 0);
        v
    }

    #[test]
    fn emits_complete_frames_and_holds_back_the_tail() {
        let mut d = Demux::new();
        let mut stream = info(640, 360);
        stream.extend_from_slice(&vframe(true, 1000, &[0xAA; 30]));
        stream.extend_from_slice(&vframe(false, 2000, &[0xBB; 30]));
        // A third frame, only its header + a few payload bytes so far.
        let mut partial = vframe(true, 3000, &[0xCC; 30]);
        partial.truncate(24 + 5);
        stream.extend_from_slice(&partial);

        let frames = d.push(&stream);
        assert_eq!(frames.len(), 2, "two complete frames, the third held back");
        assert!(frames[0].key && frames[0].micros == 1000 && frames[0].annexb == vec![0xAA; 30]);
        assert!(!frames[1].key && frames[1].micros == 2000 && frames[1].annexb == vec![0xBB; 30]);
        assert_eq!((d.width, d.height), (640, 360));
        assert!(!d.h265);

        // Completing the third frame yields it; the fourth, whole and last, too
        // (its size ends it exactly at the end of the data).
        let rest = &vframe(true, 3000, &[0xCC; 30])[24 + 5..];
        let mut more = rest.to_vec();
        more.extend_from_slice(&vframe(false, 4000, &[0xDD; 30]));
        let frames = d.push(&more);
        assert_eq!(frames.len(), 2);
        assert_eq!(frames[0].annexb, vec![0xCC; 30]);
        assert_eq!(frames[0].micros, 3000);
        assert_eq!(frames[1].annexb, vec![0xDD; 30]);
    }

    #[test]
    fn a_header_cut_short_waits_for_the_rest_instead_of_panicking() {
        let mut d = Demux::new();
        let h = info(1920, 1080);
        assert!(d.push(&h[..20]).is_empty(), "first 20 bytes of the 32-byte header");
        let mut rest = h[20..].to_vec();
        rest.extend_from_slice(&vframe(true, 1000, &[0xAA; 10]));
        let frames = d.push(&rest);
        assert_eq!(frames.len(), 1);
        assert_eq!((d.width, d.height), (1920, 1080));
    }

    #[test]
    fn a_frame_header_with_an_impossible_extra_length_is_skipped() {
        let mut bad = vframe(true, 1000, &[0xAA; 10]);
        bad[12..16].copy_from_slice(&0x7fff_0000u32.to_le_bytes());
        let mut stream = bad;
        stream.extend_from_slice(&vframe(true, 2000, &[0xBB; 10]));
        let mut d = Demux::new();
        let frames = d.push(&stream);
        assert_eq!(frames.len(), 1, "the good frame after it still comes out");
        assert_eq!(frames[0].micros, 2000);
    }

    #[test]
    fn the_info_header_gives_the_clip_start_time() {
        // A real SD download's info header: 1920x1080, 15 fps, start
        // 2026-11-11 10:40:22, end 10:40:52 (camera local time).
        let h = [
            0x31, 0x30, 0x30, 0x32, 0x20, 0, 0, 0, 0x80, 0x07, 0, 0, 0x38, 0x04, 0, 0, 0x00, 0x0f, 0x7e, 0x0b, 0x0b, 0x0a, 0x28,
            0x16, 0x7e, 0x0b, 0x0b, 0x0a, 0x28, 0x34, 0, 0,
        ];
        let mut d = Demux::new();
        assert!(d.push(&h).is_empty());
        assert_eq!((d.width, d.height), (1920, 1080));
        assert_eq!(d.clip_start.unwrap().to_string(), "2026-11-11 10:40:22");
        assert_eq!(info_start_time(&h, 0).unwrap().to_string(), "2026-11-11 10:40:22");
        assert!(info_start_time(&h[..20], 0).is_none(), "a short header gives no time");
    }

    #[test]
    fn a_lone_keyframe_is_emitted_without_waiting_for_a_next_magic() {
        // An idle, motion-gated camera sends one keyframe and goes quiet: it
        // must come out now, not wait forever (this was 175 KB in, 0 out).
        let mut d = Demux::new();
        let mut stream = info(1920, 1080);
        stream.extend_from_slice(&vframe(true, 1000, &[0x65; 120_000]));
        let frames = d.push(&stream);
        assert_eq!(frames.len(), 1);
        assert!(frames[0].key && frames[0].annexb.len() == 120_000);
        let mut video = Vec::new();
        let st = extract(&stream, &mut video).unwrap();
        assert_eq!((st.iframes, st.incomplete_tail, video.len()), (1, 0, 120_000));
    }

    #[test]
    fn a_wrong_size_field_falls_back_to_the_next_magic() {
        // Size claims far more than the frame really holds: the next magic wins.
        let mut f1 = vframe(true, 10, &[1, 2, 3, 4, 5, 6, 7, 8]);
        f1[8..12].copy_from_slice(&28_549u32.to_le_bytes());
        let stream = [f1, vframe(false, 20, &[9, 9, 9, 9])].concat();
        let frames = Demux::new().push(&stream);
        assert_eq!(frames.len(), 2);
        assert_eq!(frames[0].annexb, vec![1, 2, 3, 4, 5, 6, 7, 8]);
        assert_eq!(frames[1].annexb, vec![9, 9, 9, 9]);
        // Size claims more than has arrived and no magic follows: still held.
        let mut part = vframe(true, 30, &[7; 50]);
        part.truncate(24 + 20);
        assert!(Demux::new().push(&part).is_empty());
    }

    #[test]
    fn split_across_pushes_reassembles() {
        let mut d = Demux::new();
        let f1 = vframe(true, 10, &[1, 2, 3, 4, 5, 6, 7, 8]);
        let f2 = vframe(false, 20, &[9, 10, 11, 12]);
        let whole = [f1.as_slice(), f2.as_slice()].concat();
        // Feed one byte at a time; each frame comes out once its last byte lands.
        let mut got = Vec::new();
        for b in &whole {
            got.extend(d.push(std::slice::from_ref(b)));
        }
        assert_eq!(got.len(), 2, "both frames, each as soon as it is complete");
        assert_eq!(got[0].annexb, vec![1, 2, 3, 4, 5, 6, 7, 8]);
        assert_eq!(got[1].annexb, vec![9, 10, 11, 12]);
    }
}
