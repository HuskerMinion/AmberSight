//! Just enough MP4 reading for transcoding: what a stream's video is (codec,
//! bit depth, size, frame rate) from its header and first fragments, and
//! cutting ffmpeg's fragmented output into an init segment and fragments.

/// The boxes in `buf`, in order: (type, body). Stops at the first box that
/// does not fit.
pub fn boxes(buf: &[u8]) -> Vec<([u8; 4], &[u8])> {
    let mut out = Vec::new();
    let mut p = 0usize;
    while p + 8 <= buf.len() {
        let size = u32::from_be_bytes(buf[p..p + 4].try_into().unwrap()) as u64;
        let kind: [u8; 4] = buf[p + 4..p + 8].try_into().unwrap();
        let (hdr, size) = match size {
            0 => (8u64, (buf.len() - p) as u64),
            1 => {
                if p + 16 > buf.len() {
                    break;
                }
                (16, u64::from_be_bytes(buf[p + 8..p + 16].try_into().unwrap()))
            }
            s => (8, s),
        };
        if size < hdr || (p as u64).checked_add(size).is_none_or(|end| end > buf.len() as u64) {
            break;
        }
        out.push((kind, &buf[p + hdr as usize..p + size as usize]));
        p += size as usize;
    }
    out
}

/// The first box of this type directly inside `buf`.
pub fn child<'a>(buf: &'a [u8], kind: &[u8; 4]) -> Option<&'a [u8]> {
    boxes(buf).into_iter().find(|(k, _)| k == kind).map(|(_, b)| b)
}

fn path<'a>(buf: &'a [u8], kinds: &[&[u8; 4]]) -> Option<&'a [u8]> {
    let mut cur = buf;
    for k in kinds {
        cur = child(cur, k)?;
    }
    Some(cur)
}

fn u32_at(b: &[u8], at: usize) -> Option<u32> {
    Some(u32::from_be_bytes(b.get(at..at + 4)?.try_into().ok()?))
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "lowercase")]
pub enum VideoCodec {
    H264,
    Hevc,
    Other,
}

/// A video stream as recorded or received.
#[derive(Clone, Debug, PartialEq)]
pub struct VideoDesc {
    pub codec: VideoCodec,
    pub bit_depth: u8,
    pub width: u32,
    pub height: u32,
    /// Frames a second (None: not known yet).
    pub fps: Option<f64>,
    /// RFC 6381 string of the video (avc1.* / hvc1.*).
    pub codec_string: Option<String>,
    /// The video track's number and time scale, and default sample flags
    /// (from `trex`), for reading fragments.
    pub track_id: u32,
    pub timescale: u32,
    pub trex_flags: u32,
    pub trex_duration: u32,
    /// Length from the header (mvhd/mehd), ms, when it says.
    pub duration_ms: Option<i64>,
}

/// Bit depth from an `avcC` record: High 10 and up carry it after the
/// parameter sets; without those bytes, from the SPS itself.
fn avcc_depth(r: &[u8]) -> u8 {
    let profile = r.get(1).copied().unwrap_or(0);
    if ![110u8, 122, 244].contains(&profile) {
        return 8;
    }
    let mut p = 5usize;
    let Some(n) = r.get(p).map(|b| b & 0x1f) else { return 8 };
    p += 1;
    let mut first_sps: Option<&[u8]> = None;
    for _ in 0..n {
        let Some(l) = r.get(p..p + 2) else { return 8 };
        let len = u16::from_be_bytes([l[0], l[1]]) as usize;
        if first_sps.is_none() {
            first_sps = r.get(p + 2..p + 2 + len);
        }
        p += 2 + len;
    }
    let from_sps = || first_sps.and_then(crate::ingest::sps::parse_sps).map(|s| s.bit_depth).unwrap_or(8);
    let Some(n) = r.get(p).copied() else { return from_sps() };
    p += 1;
    for _ in 0..n {
        let Some(l) = r.get(p..p + 2) else { return from_sps() };
        p += 2 + u16::from_be_bytes([l[0], l[1]]) as usize;
    }
    match r.get(p + 1) {
        Some(b) => (b & 7) + 8,
        None => from_sps(),
    }
}

/// The video track of a `moov` box's body.
pub fn video_desc(moov: &[u8]) -> Option<VideoDesc> {
    let mut duration_ms = None;
    if let Some(mvhd) = child(moov, b"mvhd") {
        let (ts, dur) = if mvhd.first() == Some(&1) {
            (u32_at(mvhd, 20)? as u64, u64::from_be_bytes(mvhd.get(24..32)?.try_into().ok()?))
        } else {
            (u32_at(mvhd, 12)? as u64, u32_at(mvhd, 16)? as u64)
        };
        if ts > 0 && dur > 0 && dur != u32::MAX as u64 {
            duration_ms = dur.checked_mul(1000).map(|v| (v / ts).min(i64::MAX as u64) as i64);
        }
    }
    for (kind, trak) in boxes(moov) {
        if &kind != b"trak" {
            continue;
        }
        let hdlr = path(trak, &[b"mdia", b"hdlr"])?;
        if hdlr.get(8..12) != Some(b"vide") {
            continue;
        }
        let tkhd = child(trak, b"tkhd")?;
        let track_id = if tkhd.first() == Some(&1) { u32_at(tkhd, 20)? } else { u32_at(tkhd, 12)? };
        let mdhd = path(trak, &[b"mdia", b"mdhd"])?;
        let timescale = if mdhd.first() == Some(&1) { u32_at(mdhd, 20)? } else { u32_at(mdhd, 12)? };
        let stbl = path(trak, &[b"mdia", b"minf", b"stbl"])?;
        let stsd = child(stbl, b"stsd")?;
        let entry = boxes(stsd.get(8..)?).into_iter().next()?;
        let body = entry.1;
        let width = u16::from_be_bytes(body.get(24..26)?.try_into().ok()?) as u32;
        let height = u16::from_be_bytes(body.get(26..28)?.try_into().ok()?) as u32;
        let inner = body.get(78..).unwrap_or(&[]);
        let (codec, bit_depth, codec_string) = match &entry.0 {
            b"avc1" | b"avc3" => {
                let r = child(inner, b"avcC");
                (VideoCodec::H264, r.map(avcc_depth).unwrap_or(8), r.and_then(|r| crate::ingest::flv::codec_string(false, r)))
            }
            b"hvc1" | b"hev1" => {
                let r = child(inner, b"hvcC");
                (VideoCodec::Hevc, r.and_then(|r| r.get(17)).map(|b| (b & 7) + 8).unwrap_or(8), r.and_then(|r| crate::ingest::flv::codec_string(true, r)))
            }
            _ => (VideoCodec::Other, 8, None),
        };
        // Frame rate of a plain MP4: its sample times.
        let mut fps = None;
        if let Some(stts) = child(stbl, b"stts") {
            let n = u32_at(stts, 4).unwrap_or(0) as usize;
            let (mut count, mut ticks) = (0u64, 0u64);
            for i in 0..n.min(10_000) {
                let (Some(c), Some(d)) = (u32_at(stts, 8 + i * 8), u32_at(stts, 12 + i * 8)) else { break };
                count += c as u64;
                ticks += c as u64 * d as u64;
            }
            if count > 1 && ticks > 0 && timescale > 0 {
                fps = Some(count as f64 * timescale as f64 / ticks as f64);
            }
        }
        let (mut trex_flags, mut trex_duration) = (0, 0);
        if let Some(mvex) = child(moov, b"mvex") {
            for (k, b) in boxes(mvex) {
                if &k == b"trex" && u32_at(b, 4) == Some(track_id) {
                    trex_duration = u32_at(b, 12).unwrap_or(0);
                    trex_flags = u32_at(b, 20).unwrap_or(0);
                }
                if &k == b"mehd" && duration_ms.is_none() {
                    let mvts = child(moov, b"mvhd").and_then(|m| if m.first() == Some(&1) { u32_at(m, 20) } else { u32_at(m, 12) }).unwrap_or(0) as u64;
                    let d = if b.first() == Some(&1) { b.get(4..12).and_then(|x| x.try_into().ok()).map(u64::from_be_bytes) } else { u32_at(b, 4).map(|x| x as u64) };
                    if let (Some(d), true) = (d, mvts > 0) {
                        duration_ms = d.checked_mul(1000).map(|v| (v / mvts).min(i64::MAX as u64) as i64);
                    }
                }
            }
        }
        return Some(VideoDesc { codec, bit_depth, width, height, fps, codec_string, track_id, timescale, trex_flags, trex_duration, duration_ms });
    }
    None
}

/// What one `moof` says about the video track: whether it has video, if its
/// first video sample is a keyframe, and the video sample durations.
#[derive(Debug, Default, PartialEq)]
pub struct FragVideo {
    pub has_video: bool,
    pub first_sync: bool,
    pub durations: Vec<u32>,
    /// A video sample is shown later than decoded (B-frames: reordering).
    pub reorders: bool,
}

pub fn moof_video(moof: &[u8], track_id: u32, trex_flags: u32, trex_duration: u32) -> FragVideo {
    let mut out = FragVideo::default();
    for (k, traf) in boxes(moof) {
        if &k != b"traf" {
            continue;
        }
        let Some(tfhd) = child(traf, b"tfhd") else { continue };
        if u32_at(tfhd, 4) != Some(track_id) {
            continue;
        }
        let tf = u32_at(tfhd, 0).unwrap_or(0) & 0xff_ffff;
        let mut p = 8;
        if tf & 1 != 0 {
            p += 8;
        }
        if tf & 2 != 0 {
            p += 4;
        }
        let def_dur = if tf & 8 != 0 {
            let v = u32_at(tfhd, p).unwrap_or(trex_duration);
            p += 4;
            v
        } else {
            trex_duration
        };
        if tf & 0x10 != 0 {
            p += 4;
        }
        let def_flags = if tf & 0x20 != 0 { u32_at(tfhd, p).unwrap_or(trex_flags) } else { trex_flags };
        for (k, trun) in boxes(traf) {
            if &k != b"trun" {
                continue;
            }
            let fl = u32_at(trun, 0).unwrap_or(0) & 0xff_ffff;
            let n = u32_at(trun, 4).unwrap_or(0) as usize;
            let mut p = 8;
            if fl & 1 != 0 {
                p += 4;
            }
            let first_flags = if fl & 4 != 0 {
                let v = u32_at(trun, p);
                p += 4;
                v
            } else {
                None
            };
            for i in 0..n.min(100_000) {
                let mut d = def_dur;
                let mut f = def_flags;
                if fl & 0x100 != 0 {
                    d = u32_at(trun, p).unwrap_or(def_dur);
                    p += 4;
                }
                if fl & 0x200 != 0 {
                    p += 4;
                }
                if fl & 0x400 != 0 {
                    f = u32_at(trun, p).unwrap_or(def_flags);
                    p += 4;
                }
                if fl & 0x800 != 0 {
                    if u32_at(trun, p).is_some_and(|c| c != 0) {
                        out.reorders = true;
                    }
                    p += 4;
                }
                if i == 0 {
                    if let Some(ff) = first_flags {
                        f = ff;
                    }
                    if !out.has_video {
                        out.first_sync = f & 0x0001_0000 == 0;
                    }
                }
                out.has_video = true;
                out.durations.push(d);
            }
        }
    }
    out
}

/// The video of the start of a stream or file: its header plus, for a
/// fragmented one, the frame rate from the first fragments.
pub fn probe(buf: &[u8]) -> Option<VideoDesc> {
    let all = boxes(buf);
    let moov = all.iter().find(|(k, _)| k == b"moov")?.1;
    let mut d = video_desc(moov)?;
    if d.fps.is_none() {
        let (mut n, mut ticks) = (0u64, 0u64);
        for (_, moof) in all.iter().filter(|(k, _)| k == b"moof").take(20) {
            let v = moof_video(moof, d.track_id, d.trex_flags, d.trex_duration);
            n += v.durations.len() as u64;
            ticks += v.durations.iter().map(|x| *x as u64).sum::<u64>();
        }
        if n > 1 && ticks > 0 && d.timescale > 0 {
            d.fps = Some(n as f64 * d.timescale as f64 / ticks as f64);
        }
    }
    Some(d)
}

/// The start of a file and its `moov` wherever it is: a file whose header
/// is at its end (a plain MP4 written in one go) is read box by box.
pub fn probe_file(path: &std::path::Path) -> std::io::Result<Option<VideoDesc>> {
    use std::io::{Read, Seek, SeekFrom};
    let mut f = std::fs::File::open(path)?;
    let len = f.metadata()?.len();
    let mut head = vec![0u8; len.min(4 << 20) as usize];
    f.read_exact(&mut head)?;
    if let Some(d) = probe(&head) {
        return Ok(Some(d));
    }
    // Walk the top-level boxes for the moov.
    let mut pos = 0u64;
    while pos + 8 <= len {
        let mut h = [0u8; 16];
        f.seek(SeekFrom::Start(pos))?;
        f.read_exact(&mut h[..8])?;
        let mut size = u32::from_be_bytes(h[..4].try_into().unwrap()) as u64;
        let mut hdr = 8;
        if size == 1 {
            f.read_exact(&mut h[8..16])?;
            size = u64::from_be_bytes(h[8..16].try_into().unwrap());
            hdr = 16;
        } else if size == 0 {
            size = len - pos;
        }
        if size < hdr {
            break;
        }
        if &h[4..8] == b"moov" && size < (64 << 20) {
            let mut b = vec![0u8; (size - hdr) as usize];
            f.read_exact(&mut b)?;
            return Ok(video_desc(&b));
        }
        let Some(next) = pos.checked_add(size) else { break };
        pos = next;
    }
    Ok(None)
}

/// Like `probe`, reading only box headers, the header boxes and the first
/// few `moof` boxes (not the pictures) through `read(start, end)` from a
/// stream of `len` bytes.
pub fn probe_with(len: u64, read: &dyn Fn(u64, u64) -> Option<Vec<u8>>) -> Option<VideoDesc> {
    let mut buf = Vec::new();
    let (mut pos, mut moofs) = (0u64, 0);
    while pos.checked_add(8)? <= len && moofs < 8 {
        let h = read(pos, (pos + 16).min(len))?;
        let mut size = u32::from_be_bytes(h.get(..4)?.try_into().ok()?) as u64;
        if size == 1 {
            size = u64::from_be_bytes(h.get(8..16)?.try_into().ok()?);
        } else if size == 0 {
            size = len - pos;
        }
        if size < 8 {
            return None;
        }
        let kind = &h[4..8];
        if kind == b"ftyp" || kind == b"moov" || kind == b"moof" {
            if size > (16 << 20) {
                return None;
            }
            buf.extend(read(pos, pos.checked_add(size)?)?);
            moofs += (kind == b"moof") as u32;
        }
        pos = pos.checked_add(size)?;
    }
    probe(&buf)
}

/// Cuts a fragmented MP4 byte stream (ffmpeg's output) into the init
/// segment (ftyp + moov) and fragments (moof + mdat, with any boxes that
/// came between).
#[derive(Default)]
pub struct Splitter {
    buf: Vec<u8>,
    init: Vec<u8>,
    frag: Vec<u8>,
    have_init: bool,
}

pub enum Piece {
    Init(Vec<u8>),
    Fragment(Vec<u8>),
}

/// Largest box accepted from ffmpeg (a 4K keyframe is a few MB).
const MAX_BOX: u64 = 256 << 20;

impl Splitter {
    pub fn push(&mut self, data: &[u8], out: &mut Vec<Piece>) -> Result<(), &'static str> {
        self.buf.extend_from_slice(data);
        let mut p = 0usize;
        loop {
            let rest = &self.buf[p..];
            if rest.len() < 8 {
                break;
            }
            let mut size = u32::from_be_bytes(rest[..4].try_into().unwrap()) as u64;
            if size == 1 {
                if rest.len() < 16 {
                    break;
                }
                size = u64::from_be_bytes(rest[8..16].try_into().unwrap());
            }
            if size < 8 || size > MAX_BOX {
                return Err("bad box in the transcoder's output");
            }
            if (rest.len() as u64) < size {
                break;
            }
            let b = &rest[..size as usize];
            let kind: [u8; 4] = b[4..8].try_into().unwrap();
            if !self.have_init {
                self.init.extend_from_slice(b);
                if &kind == b"moov" {
                    self.have_init = true;
                    out.push(Piece::Init(std::mem::take(&mut self.init)));
                }
            } else {
                self.frag.extend_from_slice(b);
                if &kind == b"mdat" {
                    out.push(Piece::Fragment(std::mem::take(&mut self.frag)));
                }
            }
            p += size as usize;
        }
        self.buf.drain(..p);
        Ok(())
    }
}

#[cfg(test)]
mod tests {

    fn bx(kind: &[u8; 4], body: &[u8]) -> Vec<u8> {
        [&((body.len() + 8) as u32).to_be_bytes()[..], kind, body].concat()
    }

    /// A header like ffmpeg's empty_moov one: movie timescale 1000, video 10240.
    fn empty_moov() -> Vec<u8> {
        let mut mvhd = vec![0u8; 100];
        mvhd[12..16].copy_from_slice(&1000u32.to_be_bytes());
        let tkhd = vec![0u8; 84];
        let mut mdhd = vec![0u8; 24];
        mdhd[12..16].copy_from_slice(&10240u32.to_be_bytes());
        let mdia = bx(b"mdia", &bx(b"mdhd", &mdhd));
        let trak = bx(b"trak", &[bx(b"tkhd", &tkhd), mdia].concat());
        let mvex = bx(b"mvex", &bx(b"trex", &[0u8; 24]));
        bx(b"moov", &[bx(b"mvhd", &mvhd), trak, mvex, bx(b"udta", &[1, 2, 3])].concat())
    }

    #[test]
    fn the_length_is_written_into_the_header() {
        let m = with_duration(&empty_moov(), 50_405).unwrap();
        assert_eq!(m.len(), empty_moov().len() + 16);
        assert_eq!(u32::from_be_bytes(m[0..4].try_into().unwrap()) as usize, m.len());
        let body = &m[8..];
        assert_eq!(u32_at(child(body, b"mvhd").unwrap(), 16), Some(50_405));
        let trak = child(body, b"trak").unwrap();
        assert_eq!(u32_at(child(trak, b"tkhd").unwrap(), 20), Some(50_405));
        assert_eq!(u32_at(path(trak, &[b"mdia", b"mdhd"]).unwrap(), 16), Some(50_405 * 10240 / 1000));
        let mvex = child(body, b"mvex").unwrap();
        assert_eq!(u32_at(child(mvex, b"mehd").unwrap(), 4), Some(50_405));
        assert!(child(mvex, b"trex").is_some() && child(body, b"udta").is_some());
        // A second time: mehd is set, not added again.
        let again = with_duration(&m, 1000).unwrap();
        assert_eq!(again.len(), m.len());
        assert_eq!(u32_at(child(child(&again[8..], b"mvex").unwrap(), b"mehd").unwrap(), 4), Some(1000));
    }

    #[test]
    fn the_header_is_patched_however_the_output_arrives() {
        let ftyp = bx(b"ftyp", b"isom\0\0\0\0");
        let moof = bx(b"moof", &[7u8; 20]);
        let all = [ftyp.clone(), empty_moov(), moof.clone(), bx(b"mdat", &[9u8; 50])].concat();
        for step in [1usize, 7, 64, all.len()] {
            let mut p = DurationPatch::new(Some(2000));
            let mut out = Vec::new();
            for c in all.chunks(step) {
                out.extend(p.push(c));
            }
            out.extend(p.rest());
            assert_eq!(out.len(), all.len() + 16, "step {step}");
            assert_eq!(&out[..ftyp.len()], &ftyp[..]);
            let moov = &out[ftyp.len()..];
            let mlen = u32::from_be_bytes(moov[0..4].try_into().unwrap()) as usize;
            assert_eq!(u32_at(child(&moov[8..mlen], b"mvhd").unwrap(), 16), Some(2000));
            assert_eq!(&out[ftyp.len() + mlen..], &all[ftyp.len() + empty_moov().len()..]);
        }
        // No length, or output that is not a header first: passed on unchanged.
        let mut p = DurationPatch::new(None);
        assert_eq!(p.push(&all), all);
        let odd = [moof, bx(b"mdat", &[1])].concat();
        let mut p = DurationPatch::new(Some(5));
        assert_eq!(p.push(&odd), odd);
        assert!(with_duration(b"not a moov", 5).is_none());
    }

    use super::*;
    use crate::ingest::fmp4::{self, Sample, TrackInfo, TrackKind, TrackRun};

    pub fn hevc_record(depth_minus8: u8) -> Vec<u8> {
        let mut r = vec![1, 0x01, 0x60, 0, 0, 0, 0x90, 0, 0, 0, 0, 0, 153, 0xf0, 0, 0xfc, 0xfd, 0xf8 | depth_minus8, 0xf8 | depth_minus8, 0, 0, 0x0f, 0];
        if depth_minus8 > 0 {
            r[1] = 0x02;
        }
        r
    }

    fn stream(h265: bool, depth: u8, w: u16, h: u16) -> Vec<u8> {
        let rec = if h265 { hevc_record(depth - 8) } else { vec![1, 0x64, 0, 0x33, 0xff, 0xe0, 0] };
        let entry = crate::ingest::flv::video_sample_entry(h265, &rec, w, h);
        let t = TrackInfo { kind: TrackKind::Video, timescale: 90000, sample_entry: entry, width: w as u32, height: h as u32 };
        let mut out = fmp4::init_segment(&[t]);
        for i in 0..3u32 {
            let samples = (0..10).map(|j| Sample::new(4500, vec![1, 2, 3], j == 0)).collect();
            out.extend(fmp4::fragment(i + 1, &[TrackRun { track_index: 0, base_decode_time: i as u64 * 45000, samples }]));
        }
        out
    }

    #[test]
    fn reads_codec_depth_size_and_rate() {
        let d = probe(&stream(true, 8, 4608, 1728)).unwrap();
        assert_eq!((d.codec, d.bit_depth, d.width, d.height), (VideoCodec::Hevc, 8, 4608, 1728));
        assert!((d.fps.unwrap() - 20.0).abs() < 0.01, "{:?}", d.fps);
        assert!(d.codec_string.unwrap().starts_with("hvc1.1."));
        let d = probe(&stream(true, 10, 3840, 2160)).unwrap();
        assert_eq!(d.bit_depth, 10);
        let d = probe(&stream(false, 8, 1920, 1080)).unwrap();
        assert_eq!((d.codec, d.bit_depth), (VideoCodec::H264, 8));
        assert_eq!(d.codec_string.as_deref(), Some("avc1.640033"));
    }

    #[test]
    fn huge_box_sizes_do_not_wrap() {
        let mut b = vec![0, 0, 0, 1];
        b.extend_from_slice(b"moov");
        b.extend_from_slice(&u64::MAX.to_be_bytes());
        b.extend_from_slice(&[0; 32]);
        assert!(boxes(&b).is_empty());
        assert!(probe(&b).is_none());
    }

    #[test]
    fn keyframes_of_fragments_and_splitting() {
        let s = stream(false, 8, 640, 360);
        let d = probe(&s).unwrap();
        let mut sp = Splitter::default();
        let mut out = Vec::new();
        // In awkward pieces.
        for c in s.chunks(7) {
            sp.push(c, &mut out).unwrap();
        }
        assert_eq!(out.len(), 4);
        let Piece::Init(init) = &out[0] else { panic!("init first") };
        assert_eq!(&init[4..8], b"ftyp");
        for p in &out[1..] {
            let Piece::Fragment(f) = p else { panic!("fragment") };
            let moof = boxes(f).into_iter().find(|(k, _)| k == b"moof").unwrap().1;
            let v = moof_video(moof, d.track_id, d.trex_flags, d.trex_duration);
            assert!(v.has_video && v.first_sync && !v.reorders);
            assert_eq!(v.durations.len(), 10);
        }
        // B-frames: composition offsets.
        let mut s = Sample::new(3000, vec![1], false);
        s.composition_offset = 6000;
        let f = fmp4::fragment(9, &[TrackRun { track_index: 0, base_decode_time: 0, samples: vec![Sample::new(3000, vec![1], true), s] }]);
        let moof = boxes(&f).into_iter().find(|(k, _)| k == b"moof").unwrap().1;
        assert!(moof_video(moof, d.track_id, d.trex_flags, d.trex_duration).reorders);
    }
}

/// ffmpeg's `empty_moov` header says the movie lasts 0, and players that read
/// the length from it (ExoPlayer: mehd, else the movie header) then report a
/// 1 ms video. This writes `ms` into mvhd, every tkhd and mdhd, and adds (or
/// sets) mvex/mehd. `moov` is the whole box with its header; the result is
/// the new box (16 or 20 bytes longer when mehd was added). None: not a moov
/// this can read. Byte offsets after the header move: fragments must be
/// relative to their moof (default_base_moof) and there must be no mfra.
pub fn with_duration(moov: &[u8], ms: i64) -> Option<Vec<u8>> {
    if moov.len() < 8 || &moov[4..8] != b"moov" || u32::from_be_bytes(moov[0..4].try_into().ok()?) as usize != moov.len() || ms < 0 {
        return None;
    }
    let mut m = moov.to_vec();
    let scaled = |ts: u32| (ms as u128 * ts as u128 / 1000) as u64;
    // (offset of the full box's version byte, field offset in v0, in v1, timescale)
    let mut fields: Vec<(usize, usize, usize, u32)> = Vec::new();
    let body = &moov[8..];
    let mvhd = child(body, b"mvhd")?;
    let movie_ts = full_ts(mvhd, 12, 20)?;
    fields.push((off_in(moov, mvhd)?, 16, 24, movie_ts));
    for (k, trak) in boxes(body) {
        if &k != b"trak" {
            continue;
        }
        let tkhd = child(trak, b"tkhd")?;
        fields.push((off_in(moov, tkhd)?, 20, 28, movie_ts));
        let mdhd = path(trak, &[b"mdia", b"mdhd"])?;
        let ts = full_ts(mdhd, 12, 20)?;
        fields.push((off_in(moov, mdhd)?, 16, 24, ts));
    }
    for (at, v0, v1, ts) in fields {
        let d = scaled(ts);
        match m.get(at)? {
            0 => m.get_mut(at + v0..at + v0 + 4)?.copy_from_slice(&(d.min(u32::MAX as u64) as u32).to_be_bytes()),
            1 => m.get_mut(at + v1..at + v1 + 8)?.copy_from_slice(&d.to_be_bytes()),
            _ => return None,
        }
    }
    let d = scaled(movie_ts);
    let mvex = child(body, b"mvex")?;
    let mvex_at = off_in(moov, mvex)? - 8;
    if u32::from_be_bytes(moov[mvex_at..mvex_at + 4].try_into().ok()?) as usize != mvex.len() + 8 {
        return None;
    }
    if let Some(mehd) = child(mvex, b"mehd") {
        let at = off_in(moov, mehd)?;
        match m.get(at)? {
            0 => m.get_mut(at + 4..at + 8)?.copy_from_slice(&(d.min(u32::MAX as u64) as u32).to_be_bytes()),
            1 => m.get_mut(at + 4..at + 12)?.copy_from_slice(&d.to_be_bytes()),
            _ => return None,
        }
        return Some(m);
    }
    let mehd: Vec<u8> = if d <= u32::MAX as u64 {
        [&16u32.to_be_bytes()[..], b"mehd", &[0, 0, 0, 0], &(d as u32).to_be_bytes()].concat()
    } else {
        [&20u32.to_be_bytes()[..], b"mehd", &[1, 0, 0, 0], &d.to_be_bytes()].concat()
    };
    let grow = mehd.len();
    let mut out = Vec::with_capacity(m.len() + grow);
    out.extend_from_slice(&m[..mvex_at + 8]);
    out.extend_from_slice(&mehd);
    out.extend_from_slice(&m[mvex_at + 8..]);
    let set = |out: &mut Vec<u8>, at: usize| {
        let s = u32::from_be_bytes(out[at..at + 4].try_into().unwrap()) + grow as u32;
        out[at..at + 4].copy_from_slice(&s.to_be_bytes());
    };
    set(&mut out, 0);
    set(&mut out, mvex_at);
    Some(out)
}

/// Offset of `inner`'s body (a slice of `outer`) in `outer`.
fn off_in(outer: &[u8], inner: &[u8]) -> Option<usize> {
    let o = (inner.as_ptr() as usize).checked_sub(outer.as_ptr() as usize)?;
    (o + inner.len() <= outer.len()).then_some(o)
}

/// A full box's timescale: at `v0` (version 0) or `v1` (version 1) in its body.
fn full_ts(b: &[u8], v0: usize, v1: usize) -> Option<u32> {
    let at = match b.first()? {
        0 => v0,
        1 => v1,
        _ => return None,
    };
    Some(u32::from_be_bytes(b.get(at..at + 4)?.try_into().ok()?)).filter(|t| *t > 0)
}

/// Holds ffmpeg's output until its header (ftyp and moov) is complete,
/// writes the length into it, then passes everything on unchanged.
pub struct DurationPatch {
    ms: Option<i64>,
    held: Vec<u8>,
}

impl DurationPatch {
    pub fn new(ms: Option<i64>) -> Self {
        DurationPatch { ms: ms.filter(|m| *m > 0), held: Vec::new() }
    }

    /// The bytes to send now (maybe none while the header is incomplete).
    pub fn push(&mut self, data: &[u8]) -> Vec<u8> {
        let Some(ms) = self.ms else { return data.to_vec() };
        self.held.extend_from_slice(data);
        // Find moov among the leading boxes.
        let mut p = 0usize;
        loop {
            if p + 8 > self.held.len() {
                if self.held.len() > 1 << 20 {
                    return self.give_up();
                }
                return Vec::new();
            }
            let size = u32::from_be_bytes(self.held[p..p + 4].try_into().unwrap()) as usize;
            let kind = &self.held[p + 4..p + 8];
            if size < 8 || kind == b"moof" || kind == b"mdat" {
                return self.give_up();
            }
            if p + size > self.held.len() {
                if size > 1 << 20 {
                    return self.give_up();
                }
                return Vec::new();
            }
            if kind == b"moov" {
                let mut out = self.held[..p].to_vec();
                match with_duration(&self.held[p..p + size], ms) {
                    Some(m) => out.extend_from_slice(&m),
                    None => out.extend_from_slice(&self.held[p..p + size]),
                }
                out.extend_from_slice(&self.held[p + size..]);
                self.ms = None;
                self.held = Vec::new();
                return out;
            }
            p += size;
        }
    }

    /// Not a header this can change: send it as it is.
    fn give_up(&mut self) -> Vec<u8> {
        self.ms = None;
        std::mem::take(&mut self.held)
    }

    /// Whatever is still held when the output ends.
    pub fn rest(&mut self) -> Vec<u8> {
        std::mem::take(&mut self.held)
    }
}
