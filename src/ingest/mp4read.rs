//! A small reader for ordinary MP4 files (a camera's own SD-card recordings):
//! where each video and audio sample is, when it is shown and whether it is a
//! keyframe, from the `moov` box alone. The sample bytes are then fetched
//! with byte ranges, so a short stretch of a long file costs only that
//! stretch. H.264/H.265 samples in MP4 are already length-prefixed and AAC is
//! raw, which is exactly what the recorder writes, so nothing is converted.

use super::fmp4::TrackKind;
use anyhow::{anyhow, bail, Result};

/// Where one sample is and when.
#[derive(Clone, Debug, PartialEq)]
pub struct SampleRef {
    pub offset: u64,
    pub size: u32,
    /// Decode time in the track's timescale.
    pub dts: u64,
    /// Shown this long after it is decoded (B-frames), in the timescale.
    pub cts_offset: i32,
    pub sync: bool,
}

#[derive(Clone, Debug)]
pub struct TrackIndex {
    pub kind: TrackKind,
    pub timescale: u32,
    /// The whole sample entry box (`avc1`, `hvc1`, `mp4a`...) as in the file.
    pub sample_entry: Vec<u8>,
    pub width: u32,
    pub height: u32,
    pub samples: Vec<SampleRef>,
}

impl Mp4Index {
    /// Every sample lies inside a file of `file_size` bytes and is of a
    /// sensible size: a damaged or made-up index never asks for more.
    pub fn check_within(&self, file_size: u64) -> Result<()> {
        for t in self.video.iter().chain(self.audio.iter()) {
            for s in &t.samples {
                if s.size > MAX_SAMPLE_BYTES || s.offset.checked_add(s.size as u64).is_none_or(|e| e > file_size) {
                    bail!("the recording's index points outside the file");
                }
            }
        }
        Ok(())
    }
}

impl TrackIndex {
    /// Seconds from the start of the file at which sample `i` is decoded.
    #[cfg(test)]
    pub fn secs(&self, i: usize) -> f64 {
        self.samples[i].dts as f64 / self.timescale.max(1) as f64
    }
}

#[derive(Clone, Debug, Default)]
pub struct Mp4Index {
    pub video: Option<TrackIndex>,
    pub audio: Option<TrackIndex>,
}

/// The largest index (`moov` box) read: a camera's two-minute recording has
/// one of well under a megabyte.
pub const MAX_MOOV_BYTES: u64 = 16 << 20;
/// The largest single sample taken: a picture bigger than this is damage.
pub const MAX_SAMPLE_BYTES: u32 = 16 << 20;

/// What the first bytes of a file say about where its `moov` is.
#[derive(Debug, PartialEq)]
pub enum MoovAt {
    /// The box starts at `offset` and is `size` bytes long.
    Found { offset: u64, size: u64 },
    /// Boxes seen so far end before it; more bytes are needed from here.
    NeedMore(u64),
}

fn be32(b: &[u8], at: usize) -> Option<u32> {
    Some(u32::from_be_bytes(b.get(at..at + 4)?.try_into().ok()?))
}
fn be64(b: &[u8], at: usize) -> Option<u64> {
    Some(u64::from_be_bytes(b.get(at..at + 8)?.try_into().ok()?))
}

/// A box header at `at`: (type, header length, total length).
fn box_at(b: &[u8], at: usize) -> Option<([u8; 4], u64, u64)> {
    let size = be32(b, at)? as u64;
    let kind: [u8; 4] = b.get(at + 4..at + 8)?.try_into().ok()?;
    match size {
        0 => Some((kind, 8, (b.len() - at) as u64)),
        1 => Some((kind, 16, be64(b, at + 8)?)),
        s if s >= 8 => Some((kind, 8, s)),
        _ => None,
    }
}

/// Find the `moov` box among the top-level boxes in `head`, the first bytes
/// of a file (the camera writes it before the media, with spare room after).
pub fn find_moov(head: &[u8]) -> Result<MoovAt> {
    let mut at = 0u64;
    let mut seen = 0;
    while (at as usize) < head.len() {
        let Some((kind, _hdr, len)) = box_at(head, at as usize) else {
            return Ok(MoovAt::NeedMore(at));
        };
        if &kind == b"moov" {
            if len < 8 || len > MAX_MOOV_BYTES {
                bail!("the recording's index is damaged or too large ({len} bytes)");
            }
            return Ok(MoovAt::Found { offset: at, size: len });
        }
        if &kind == b"mdat" {
            bail!("the recording has its index after the video (a file still being written, or not made for streaming)");
        }
        if len < 8 {
            bail!("damaged recording header");
        }
        at = at.checked_add(len).ok_or_else(|| anyhow!("damaged recording header"))?;
        if at > MAX_MOOV_BYTES {
            bail!("no index found at the start of the recording");
        }
        seen += 1;
        if seen > 64 {
            bail!("no index found at the start of the recording");
        }
    }
    Ok(MoovAt::NeedMore(at))
}

/// Child boxes of `b[from..to]`: (type, payload start, payload end).
fn children(b: &[u8], from: usize, to: usize) -> Vec<([u8; 4], usize, usize)> {
    let mut out = Vec::new();
    let mut at = from;
    while at + 8 <= to {
        let Some((kind, hdr, len)) = box_at(&b[..to], at) else { break };
        let end = at.saturating_add(len as usize);
        if len < hdr || end > to {
            break;
        }
        out.push((kind, at + hdr as usize, end));
        at = end;
    }
    out
}

fn find(b: &[u8], from: usize, to: usize, kind: &[u8; 4]) -> Option<(usize, usize)> {
    children(b, from, to).into_iter().find(|c| &c.0 == kind).map(|c| (c.1, c.2))
}

/// An HEVC sample entry (`entry`: the whole box) labeled `hev1`, ffmpeg's
/// label when it copies a camera's stream into MP4, relabeled `hvc1` in
/// place: Apple's players (iPhone, Safari, QuickTime) refuse `hev1`. `hvc1`
/// says the decoder setup is all in the entry, so only an entry holding a
/// VPS, an SPS and a PPS is relabeled, and those arrays are marked complete,
/// as ffmpeg's own `-tag:v hvc1` writes them (the same pictures, the same
/// size). True when it was changed.
pub fn hev1_entry_to_hvc1(entry: &mut [u8]) -> bool {
    let Some((kind, hdr, len)) = box_at(entry, 0) else { return false };
    let end = usize::try_from(len).unwrap_or(usize::MAX).min(entry.len());
    // VisualSampleEntry: 78 bytes of fields, then its boxes.
    let first = hdr as usize + 78;
    if &kind != b"hev1" || first > end {
        return false;
    }
    let Some((cs, ce)) = find(entry, first, end, b"hvcC") else { return false };
    // hvcC: 22 bytes, the number of arrays, then each array: a byte with the
    // NAL type (low 6 bits; the top one "complete"), a count, and the units,
    // each after its 2-byte length.
    let Some(&arrays) = entry.get(cs + 22) else { return false };
    let mut at = cs + 23;
    let mut marks = Vec::new();
    let mut have = [false; 3];
    for _ in 0..arrays {
        let (Some(&t), Some(n)) = (entry.get(at), entry.get(at + 1..at + 3)) else { return false };
        let n = u16::from_be_bytes([n[0], n[1]]);
        let mut p = at + 3;
        for _ in 0..n {
            let Some(l) = entry.get(p..p + 2) else { return false };
            p += 2 + u16::from_be_bytes([l[0], l[1]]) as usize;
        }
        if p > ce {
            return false;
        }
        // 32, 33, 34: VPS, SPS, PPS.
        if let Some(k) = ((t & 0x3f) as usize).checked_sub(32).filter(|&k| k < 3) {
            have[k] |= n > 0;
            marks.push(at);
        }
        at = p;
    }
    if !have.iter().all(|&h| h) {
        return false; // the setup only in the stream: hev1 is right
    }
    for m in marks {
        entry[m] |= 0x80;
    }
    entry[4..8].copy_from_slice(b"hvc1");
    true
}

/// Every video sample entry labeled `hev1` in `head` (whole top-level boxes
/// from a file's start, its `moov` among them), relabeled as
/// [`hev1_entry_to_hvc1`] says. True when one was.
pub fn hev1_to_hvc1(head: &mut [u8]) -> bool {
    let mut spots = Vec::new();
    for (k, ms, me) in children(head, 0, head.len()) {
        if &k != b"moov" {
            continue;
        }
        for (k, ts, te) in children(head, ms, me) {
            let Some((ds, de)) = (&k == b"trak").then(|| find(head, ts, te, b"mdia")).flatten() else { continue };
            let Some((is, ie)) = find(head, ds, de, b"minf") else { continue };
            let Some((ss, se)) = find(head, is, ie, b"stbl") else { continue };
            let Some((sd, sde)) = find(head, ss, se, b"stsd") else { continue };
            // stsd: version, flags and entry count, then the entries.
            let mut at = sd + 8;
            while let Some((kind, _, len)) = box_at(&head[..sde], at) {
                let Some(end) = usize::try_from(len).ok().and_then(|l| at.checked_add(l)).filter(|&e| e <= sde && len >= 8) else { break };
                if &kind == b"hev1" {
                    spots.push((at, end));
                }
                at = end;
            }
        }
    }
    let mut any = false;
    for (s, e) in spots {
        any |= hev1_entry_to_hvc1(&mut head[s..e]);
    }
    any
}

/// A finished MP4 file's `moov` box (where it starts, its bytes), found
/// among its first 64 top-level boxes; None when it has none or it is too big.
fn read_moov(f: &mut std::fs::File) -> std::io::Result<Option<(u64, Vec<u8>)>> {
    use std::io::{Read, Seek, SeekFrom};
    let file_len = f.metadata()?.len();
    let mut at = 0u64;
    let mut h = [0u8; 16];
    for _ in 0..64 {
        if at.saturating_add(8) > file_len {
            return Ok(None);
        }
        f.seek(SeekFrom::Start(at))?;
        let n = (file_len - at).min(16) as usize;
        f.read_exact(&mut h[..n])?;
        let (kind, _, len) = box_at(&h[..n], 0).ok_or_else(|| std::io::Error::other("damaged MP4 box"))?;
        // (Size 0: the box runs to the end of the file.)
        let len = if h[..4] == [0; 4] { file_len - at } else { len };
        if len < 8 {
            return Err(std::io::Error::other("damaged MP4 box"));
        }
        if &kind == b"moov" {
            if len > MAX_MOOV_BYTES || at + len > file_len {
                return Ok(None);
            }
            let mut moov = vec![0u8; len as usize];
            f.seek(SeekFrom::Start(at))?;
            f.read_exact(&mut moov)?;
            return Ok(Some((at, moov)));
        }
        at = at.checked_add(len).ok_or_else(|| std::io::Error::other("damaged MP4 box"))?;
    }
    Ok(None)
}

/// For a finished file on disk (a saved clip) with FLAC sound: where its
/// STREAMINFO block sizes are and the 4 bytes [`flac_fixed_block`] would
/// put there (the most samples one of its first 64 frames holds), for
/// serving it to Apple's players. None: nothing to change.
pub fn flac_file_fixed_block(path: &std::path::Path) -> std::io::Result<Option<(u64, [u8; 4])>> {
    use std::io::{Read, Seek, SeekFrom};
    let mut f = std::fs::File::open(path)?;
    let Some((at, moov)) = read_moov(&mut f)? else { return Ok(None) };
    let Some((si, ss, se)) = flac_track(&moov) else { return Ok(None) };
    if moov[si..si + 2] == moov[si + 2..si + 4] {
        return Ok(None); // one block size already (ffmpeg's own FLAC)
    }
    // The frames of its first chunk (at most 64): where the chunk starts
    // (stco or co64), how many it holds (stsc) and their sizes (stsz).
    let boxes = children(&moov, ss, se);
    let get = |k: &[u8; 4]| boxes.iter().find(|c| &c.0 == k).map(|c| (c.1, c.2));
    let chunk = match (get(b"stco"), get(b"co64")) {
        (Some((s, _)), _) if be32(&moov, s + 4).unwrap_or(0) > 0 => be32(&moov, s + 8).map(u64::from),
        (_, Some((s, _))) if be32(&moov, s + 4).unwrap_or(0) > 0 => be32(&moov, s + 8).zip(be32(&moov, s + 12)).map(|(h, l)| (u64::from(h) << 32) | u64::from(l)),
        _ => None,
    };
    let per_chunk = get(b"stsc").filter(|&(s, _)| be32(&moov, s + 4).unwrap_or(0) > 0).and_then(|(s, _)| be32(&moov, s + 12));
    let Some(((chunk, per_chunk), (zs, ze))) = chunk.zip(per_chunk).zip(get(b"stsz")) else { return Ok(None) };
    let fixed = be32(&moov, zs + 4).unwrap_or(0);
    let count = be32(&moov, zs + 8).unwrap_or(0).min(per_chunk).min(64) as usize;
    let mut frames = Vec::new();
    let mut at_frame = chunk;
    for i in 0..count {
        let size = if fixed > 0 { fixed } else { match be32(&moov, zs + 12 + i * 4) { Some(n) if zs + 16 + i * 4 <= ze => n, _ => break } };
        if size > 1 << 16 {
            break;
        }
        let mut b = vec![0u8; size as usize];
        f.seek(SeekFrom::Start(at_frame))?;
        if f.read_exact(&mut b).is_err() {
            break;
        }
        frames.push(b);
        at_frame += u64::from(size);
    }
    let Some(block) = flac_largest_block(frames) else { return Ok(None) };
    let b = block.to_be_bytes();
    Ok(Some((at + si as u64, [b[0], b[1], b[0], b[1]])))
}

/// Where the STREAMINFO of a FLAC audio track (`fLaC` sample entry, its
/// `dfLa`) starts in `head` (whole top-level boxes, the `moov` among them):
/// the offset of its two block-size fields (min, then max; 2 bytes each).
pub fn flac_streaminfo_at(head: &[u8]) -> Option<usize> {
    flac_track(head).map(|t| t.0)
}

/// [`flac_streaminfo_at`], and where that track's sample table (`stbl`
/// contents) is.
fn flac_track(head: &[u8]) -> Option<(usize, usize, usize)> {
    for (k, ms, me) in children(head, 0, head.len()) {
        if &k != b"moov" {
            continue;
        }
        for (k, ts, te) in children(head, ms, me) {
            let Some((ds, de)) = (&k == b"trak").then(|| find(head, ts, te, b"mdia")).flatten() else { continue };
            let Some((is, ie)) = find(head, ds, de, b"minf") else { continue };
            let Some((ss, se)) = find(head, is, ie, b"stbl") else { continue };
            let Some((sd, sde)) = find(head, ss, se, b"stsd") else { continue };
            let mut at = sd + 8;
            while let Some((kind, hdr, len)) = box_at(&head[..sde], at) {
                let Some(end) = usize::try_from(len).ok().and_then(|l| at.checked_add(l)).filter(|&e| e <= sde && len >= 8) else { break };
                // AudioSampleEntry: 28 bytes of fields, then its boxes.
                if &kind == b"fLaC" {
                    if let Some((fs, fe)) = find(head, at + hdr as usize + 28, end, b"dfLa") {
                        // dfLa: version and flags, then FLAC metadata blocks:
                        // the first, STREAMINFO (type 0), 34 bytes.
                        let t = *head.get(fs + 4)?;
                        let n = u32::from_be_bytes([0, *head.get(fs + 5)?, *head.get(fs + 6)?, *head.get(fs + 7)?]);
                        if t & 0x7f == 0 && n >= 34 && fs + 8 + 34 <= fe {
                            return Some((fs + 8, ss, se));
                        }
                    }
                }
                at = end;
            }
        }
    }
    None
}

/// A FLAC track's STREAMINFO set to say every frame holds `block` samples
/// (min = max): Apple's players (iPhone, Safari, QuickTime) drop a FLAC track
/// whose block sizes differ (AmberSight's say 16 to 65535, as a camera's
/// packets may be any size) and then end the video after 5 s. The frames
/// themselves are not changed. True when there was such a track.
pub fn flac_fixed_block(head: &mut [u8], block: u16) -> bool {
    let Some(at) = flac_streaminfo_at(head) else { return false };
    head[at..at + 2].copy_from_slice(&block.to_be_bytes());
    head[at + 2..at + 4].copy_from_slice(&block.to_be_bytes());
    true
}

/// The most samples one of the first `n` FLAC frames (MP4 samples) of a
/// track holds, read from the frames themselves.
pub fn flac_largest_block(frames: impl IntoIterator<Item = Vec<u8>>) -> Option<u16> {
    frames.into_iter().filter_map(|f| crate::ingest::g711::flac_block_size(&f)).max().and_then(|b| u16::try_from(b).ok())
}

/// [`hev1_to_hvc1`] for a finished file on disk (a saved clip, its `moov`
/// at the start or the end): only the changed bytes are written. True when
/// it was changed.
pub fn hev1_file_to_hvc1(path: &std::path::Path) -> std::io::Result<bool> {
    use std::io::{Seek, SeekFrom, Write};
    let mut f = std::fs::OpenOptions::new().read(true).open(path)?;
    let Some((at, mut moov)) = read_moov(&mut f)? else { return Ok(false) };
    let was = moov.clone();
    if !hev1_to_hvc1(&mut moov) {
        return Ok(false);
    }
    // (Its time is kept, 2 s later: the clip sweep and the offsite copy go
    // by it, and a player's cached copy, checked by that time, is then
    // seen to be out of date.)
    let modified = f.metadata()?.modified().ok();
    let mut w = std::fs::OpenOptions::new().write(true).open(path)?;
    // Each changed run in one write, so a reader never sees half a label.
    let mut i = 0;
    while i < moov.len() {
        if was[i] == moov[i] {
            i += 1;
            continue;
        }
        let start = i;
        while i < moov.len() && was[i] != moov[i] {
            i += 1;
        }
        w.seek(SeekFrom::Start(at + start as u64))?;
        w.write_all(&moov[start..i])?;
    }
    w.flush()?;
    if let Some(t) = modified {
        let _ = w.set_modified(t + std::time::Duration::from_secs(2));
    }
    Ok(true)
}

/// Read the sample tables of a complete `moov` box (header included).
pub fn parse_moov(moov: &[u8]) -> Result<Mp4Index> {
    let (_, hdr, len) = box_at(moov, 0).ok_or_else(|| anyhow!("empty header"))?;
    let end = (len as usize).min(moov.len());
    let mut out = Mp4Index::default();
    for (kind, s, e) in children(moov, hdr as usize, end) {
        if &kind != b"trak" {
            continue;
        }
        let Some(t) = parse_trak(moov, s, e)? else { continue };
        let slot = if t.kind == TrackKind::Video { &mut out.video } else { &mut out.audio };
        if slot.is_none() {
            *slot = Some(t);
        }
    }
    if out.video.as_ref().is_none_or(|v| v.samples.is_empty()) {
        bail!("no video in the recording");
    }
    Ok(out)
}

fn parse_trak(b: &[u8], from: usize, to: usize) -> Result<Option<TrackIndex>> {
    let Some((tk_s, tk_e)) = find(b, from, to, b"tkhd") else { return Ok(None) };
    let Some((md_s, md_e)) = find(b, from, to, b"mdia") else { return Ok(None) };
    let Some((hd_s, _)) = find(b, md_s, md_e, b"hdlr") else { return Ok(None) };
    let kind = match b.get(hd_s + 8..hd_s + 12) {
        Some(b"vide") => TrackKind::Video,
        Some(b"soun") => TrackKind::Audio,
        _ => return Ok(None),
    };
    // tkhd: display size is the last two 16.16 numbers.
    let (width, height) = if tk_e >= tk_s + 8 { (be32(b, tk_e - 8).unwrap_or(0) >> 16, be32(b, tk_e - 4).unwrap_or(0) >> 16) } else { (0, 0) };
    let (mh_s, _) = find(b, md_s, md_e, b"mdhd").ok_or_else(|| anyhow!("no media header"))?;
    let timescale = if b.get(mh_s) == Some(&1) { be32(b, mh_s + 20) } else { be32(b, mh_s + 12) }.filter(|&t| t > 0).ok_or_else(|| anyhow!("no time scale"))?;
    let (mi_s, mi_e) = find(b, md_s, md_e, b"minf").ok_or_else(|| anyhow!("no media info"))?;
    let (st_s, st_e) = find(b, mi_s, mi_e, b"stbl").ok_or_else(|| anyhow!("no sample table"))?;
    let boxes = children(b, st_s, st_e);
    let get = |k: &[u8; 4]| boxes.iter().find(|c| &c.0 == k).map(|c| (c.1, c.2));

    // stsd: the first entry, whole.
    let (sd_s, sd_e) = get(b"stsd").ok_or_else(|| anyhow!("no sample description"))?;
    let entries = sd_s + 8;
    let (ek, _, elen) = box_at(b, entries).ok_or_else(|| anyhow!("no sample entry"))?;
    let entry_end = usize::try_from(elen).ok().and_then(|l| entries.checked_add(l)).filter(|&e| e <= sd_e).ok_or_else(|| anyhow!("damaged sample description"))?;
    let mut sample_entry = b.get(entries..entry_end).ok_or_else(|| anyhow!("damaged sample description"))?.to_vec();
    // (A camera's own file may say hev1: copied on as hvc1, which Apple's players take.)
    hev1_entry_to_hvc1(&mut sample_entry);
    if kind == TrackKind::Video && !matches!(&ek, b"avc1" | b"avc3" | b"hvc1" | b"hev1") {
        bail!("video of a kind AmberSight does not copy ({})", String::from_utf8_lossy(&ek));
    }
    if kind == TrackKind::Audio && &ek != b"mp4a" {
        return Ok(None); // G.711 and the like: left out
    }

    // stts: (count, delta) runs.
    let (s, e) = get(b"stts").ok_or_else(|| anyhow!("no sample times"))?;
    let n = be32(b, s + 4).unwrap_or(0) as usize;
    let mut deltas = Vec::new();
    for i in 0..n.min((e - s) / 8) {
        deltas.push((be32(b, s + 8 + i * 8).unwrap_or(0) as u64, be32(b, s + 12 + i * 8).unwrap_or(0) as u64));
    }
    // stsz
    let (s, e) = get(b"stsz").ok_or_else(|| anyhow!("no sample sizes"))?;
    let fixed = be32(b, s + 4).unwrap_or(0);
    let count = be32(b, s + 8).unwrap_or(0) as usize;
    if count > 5_000_000 {
        bail!("implausible sample count");
    }
    if fixed == 0 && s + 12 + count * 4 > e {
        bail!("damaged sample sizes");
    }
    let size = |i: usize| if fixed != 0 { fixed } else { be32(b, s + 12 + i * 4).unwrap_or(0) };
    // stsc: (first chunk, samples per chunk)
    let (s, e) = get(b"stsc").ok_or_else(|| anyhow!("no chunk map"))?;
    let n = be32(b, s + 4).unwrap_or(0) as usize;
    let mut runs = Vec::new();
    for i in 0..n.min((e - s) / 12) {
        runs.push((be32(b, s + 8 + i * 12).unwrap_or(1) as usize, be32(b, s + 12 + i * 12).unwrap_or(0) as usize));
    }
    // chunk offsets
    let offsets: Vec<u64> = if let Some((s, e)) = get(b"stco") {
        let n = (be32(b, s + 4).unwrap_or(0) as usize).min((e - s) / 4);
        (0..n).map(|i| be32(b, s + 8 + i * 4).unwrap_or(0) as u64).collect()
    } else if let Some((s, e)) = get(b"co64") {
        let n = (be32(b, s + 4).unwrap_or(0) as usize).min((e - s) / 8);
        (0..n).map(|i| be64(b, s + 8 + i * 8).unwrap_or(0)).collect()
    } else {
        bail!("no chunk offsets");
    };
    // stss: sync samples (1-based); absent = all.
    let sync: Option<Vec<u32>> = get(b"stss").map(|(s, e)| {
        let n = (be32(b, s + 4).unwrap_or(0) as usize).min((e - s) / 4);
        (0..n).map(|i| be32(b, s + 8 + i * 4).unwrap_or(0)).collect()
    });
    // ctts: (count, offset); version 1 offsets are signed, version 0 are
    // unsigned but read the same way for any sane value.
    let ctts: Vec<(u64, i32)> = get(b"ctts")
        .map(|(s, e)| {
            let n = (be32(b, s + 4).unwrap_or(0) as usize).min((e - s) / 8);
            (0..n).map(|i| (be32(b, s + 8 + i * 8).unwrap_or(0) as u64, be32(b, s + 12 + i * 8).unwrap_or(0) as i32)).collect()
        })
        .unwrap_or_default();

    // Walk chunks to place each sample, the chunk map's runs in step (a run
    // holds from its first chunk until the next run's), stopping once every
    // sample is placed.
    let mut samples = Vec::with_capacity(count);
    let mut idx = 0usize;
    let mut run = 0usize;
    for (ci, &chunk_off) in offsets.iter().enumerate() {
        if idx >= count {
            break;
        }
        let chunk = ci + 1;
        while run + 1 < runs.len() && runs[run + 1].0 <= chunk {
            run += 1;
        }
        let per = runs.get(run).filter(|r| r.0 <= chunk).map_or(0, |r| r.1);
        let mut off = chunk_off;
        for _ in 0..per {
            if idx >= count {
                break;
            }
            let sz = size(idx);
            samples.push(SampleRef { offset: off, size: sz, dts: 0, cts_offset: 0, sync: false });
            off = off.checked_add(sz as u64).ok_or_else(|| anyhow!("damaged chunk offsets"))?;
            idx += 1;
        }
    }
    if samples.len() < count {
        bail!("the sample tables disagree ({} of {count} samples placed)", samples.len());
    }
    // Times.
    let mut t = 0u64;
    let mut i = 0usize;
    'outer: for (n, d) in &deltas {
        for _ in 0..*n {
            if i >= samples.len() {
                break 'outer;
            }
            samples[i].dts = t;
            t = t.saturating_add(*d);
            i += 1;
        }
    }
    if i < samples.len() {
        bail!("the sample times are short");
    }
    let mut i = 0usize;
    'o2: for (n, o) in &ctts {
        for _ in 0..*n {
            if i >= samples.len() {
                break 'o2;
            }
            samples[i].cts_offset = *o;
            i += 1;
        }
    }
    match &sync {
        None => samples.iter_mut().for_each(|s| s.sync = true),
        Some(list) => {
            for &n in list {
                if let Some(s) = (n as usize).checked_sub(1).and_then(|k| samples.get_mut(k)) {
                    s.sync = true;
                }
            }
        }
    }
    Ok(Some(TrackIndex { kind, timescale, sample_entry, width, height, samples }))
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    fn bx(kind: &[u8; 4], payload: &[u8]) -> Vec<u8> {
        let mut v = ((payload.len() + 8) as u32).to_be_bytes().to_vec();
        v.extend_from_slice(kind);
        v.extend_from_slice(payload);
        v
    }
    fn full(kind: &[u8; 4], body: &[u8]) -> Vec<u8> {
        let mut p = vec![0, 0, 0, 0];
        p.extend_from_slice(body);
        bx(kind, &p)
    }
    fn u32s(v: &[u32]) -> Vec<u8> {
        v.iter().flat_map(|x| x.to_be_bytes()).collect()
    }

    /// One track: `sizes` per sample, chunks of `per_chunk` samples laid out
    /// from `first_offset`, constant `delta`, keyframes at `sync` (1-based).
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn trak(handler: &[u8; 4], entry: &[u8; 4], timescale: u32, sizes: &[u32], per_chunk: u32, first_offset: u32, delta: u32, sync: Option<&[u32]>, ctts: Option<&[(u32, u32)]>) -> Vec<u8> {
        let mut tkhd = vec![0u8; 80];
        tkhd[76..80].copy_from_slice(&(360u32 << 16).to_be_bytes());
        tkhd[72..76].copy_from_slice(&(640u32 << 16).to_be_bytes());
        let tkhd = full(b"tkhd", &tkhd);
        let mut hd = vec![0u8; 4];
        hd.extend_from_slice(handler);
        hd.extend_from_slice(&[0; 12]);
        let hdlr = full(b"hdlr", &hd);
        let mut mh = u32s(&[0, 0, timescale, 0]);
        mh.extend_from_slice(&[0; 4]);
        let mdhd = full(b"mdhd", &mh);
        let entry_box = bx(entry, &[0; 20]);
        let stsd = full(b"stsd", &[u32s(&[1]), entry_box].concat());
        let n = sizes.len() as u32;
        let stts = full(b"stts", &u32s(&[1, n, delta]));
        let mut stsz = u32s(&[0, n]);
        stsz.extend(u32s(sizes));
        let stsz = full(b"stsz", &stsz);
        let stsc = full(b"stsc", &u32s(&[1, 1, per_chunk, 1]));
        let mut offs = Vec::new();
        let mut off = first_offset;
        for ch in sizes.chunks(per_chunk as usize) {
            offs.push(off);
            off += ch.iter().sum::<u32>();
        }
        let stco = full(b"stco", &[u32s(&[offs.len() as u32]), u32s(&offs)].concat());
        let mut stbl = [stsd, stts, stsz, stsc, stco].concat();
        if let Some(s) = sync {
            stbl.extend(full(b"stss", &[u32s(&[s.len() as u32]), u32s(s)].concat()));
        }
        if let Some(c) = ctts {
            let flat: Vec<u32> = c.iter().flat_map(|&(a, b)| [a, b]).collect();
            stbl.extend(full(b"ctts", &[u32s(&[c.len() as u32]), u32s(&flat)].concat()));
        }
        let minf = bx(b"minf", &bx(b"stbl", &stbl));
        let mdia = bx(b"mdia", &[mdhd, hdlr, minf].concat());
        bx(b"trak", &[tkhd, mdia].concat())
    }

    #[test]
    fn reads_samples_chunks_times_and_keyframes() {
        // Video: 5 samples in chunks of 2 (offsets 100, 100+10+20, 100+60...), 3000 ticks apart at 90 kHz.
        let v = trak(b"vide", b"avc1", 90_000, &[10, 20, 30, 40, 50], 2, 100, 3000, Some(&[1, 4]), Some(&[(2, 6000), (3, 0)]));
        let a = trak(b"soun", b"mp4a", 16_000, &[7, 8], 2, 1000, 1024, None, None);
        let moov = bx(b"moov", &[v, a].concat());
        let idx = parse_moov(&moov).unwrap();
        let v = idx.video.unwrap();
        assert_eq!((v.width, v.height, v.timescale), (640, 360, 90_000));
        assert_eq!(v.samples.len(), 5);
        assert_eq!(v.samples.iter().map(|s| s.offset).collect::<Vec<_>>(), vec![100, 110, 130, 160, 200]);
        assert_eq!(v.samples.iter().map(|s| s.dts).collect::<Vec<_>>(), vec![0, 3000, 6000, 9000, 12000]);
        assert_eq!(v.samples.iter().map(|s| s.sync).collect::<Vec<_>>(), vec![true, false, false, true, false]);
        assert_eq!(v.samples.iter().map(|s| s.cts_offset).collect::<Vec<_>>(), vec![6000, 6000, 0, 0, 0]);
        assert_eq!(&v.sample_entry[4..8], b"avc1");
        let a = idx.audio.unwrap();
        assert_eq!((a.timescale, a.samples.len()), (16_000, 2));
        assert!(a.samples.iter().all(|s| s.sync), "no sync table: every sample is one");
        assert_eq!(a.samples[1].offset, 1007);
    }

    #[test]
    fn finds_moov_after_other_boxes_or_asks_for_more() {
        let ftyp = bx(b"ftyp", &[0; 16]);
        let moov = bx(b"moov", &[0; 40]);
        let file = [ftyp.clone(), moov.clone(), bx(b"skip", &[0; 100]), bx(b"mdat", &[0; 10])].concat();
        assert_eq!(find_moov(&file[..64]).unwrap(), MoovAt::Found { offset: 24, size: 48 });
        // Only the moov's header is in hand: enough to know where it is.
        assert_eq!(find_moov(&file[..32]).unwrap(), MoovAt::Found { offset: 24, size: 48 });
        assert_eq!(find_moov(&file[..20]).unwrap(), MoovAt::NeedMore(24));
        // Media first: the index is at the end; refused.
        let late = [ftyp, bx(b"mdat", &[0; 10]), moov].concat();
        assert!(find_moov(&late).is_err());
    }

    /// A video track with the given chunk map runs and 64-bit chunk offsets,
    /// `count` samples of 10 bytes.
    fn crafted_trak(count: u32, runs: &[(u32, u32)], offsets: &[u64]) -> Vec<u8> {
        let mut tkhd = vec![0u8; 80];
        tkhd[76..80].copy_from_slice(&(360u32 << 16).to_be_bytes());
        let tkhd = full(b"tkhd", &tkhd);
        let mut hd = vec![0u8; 4];
        hd.extend_from_slice(b"vide");
        hd.extend_from_slice(&[0; 12]);
        let hdlr = full(b"hdlr", &hd);
        let mut mh = u32s(&[0, 0, 90_000, 0]);
        mh.extend_from_slice(&[0; 4]);
        let mdhd = full(b"mdhd", &mh);
        let stsd = full(b"stsd", &[u32s(&[1]), bx(b"avc1", &[0; 20])].concat());
        let stts = full(b"stts", &u32s(&[1, count, 3000]));
        let stsz = full(b"stsz", &u32s(&[10, count]));
        let flat: Vec<u32> = runs.iter().flat_map(|&(a, b)| [a, b, 1]).collect();
        let stsc = full(b"stsc", &[u32s(&[runs.len() as u32]), u32s(&flat)].concat());
        let mut co = u32s(&[offsets.len() as u32]);
        co.extend(offsets.iter().flat_map(|o| o.to_be_bytes()));
        let co64 = full(b"co64", &co);
        let stbl = [stsd, stts, stsz, stsc, co64].concat();
        let minf = bx(b"minf", &bx(b"stbl", &stbl));
        let mdia = bx(b"mdia", &[mdhd, hdlr, minf].concat());
        bx(b"trak", &[tkhd, mdia].concat())
    }

    #[test]
    fn a_huge_made_up_chunk_map_is_read_quickly() {
        // 1M chunks and 300k runs for one sample: walking every run for every
        // chunk would take hours; the runs are walked in step and the walk stops
        // once the sample is placed.
        let runs: Vec<(u32, u32)> = (0..300_000u32).map(|i| (i * 3 + 1, 1)).collect();
        let offsets: Vec<u64> = (0..1_000_000u64).map(|i| 1000 + i * 10).collect();
        let moov = bx(b"moov", &crafted_trak(1, &runs, &offsets));
        let t = std::time::Instant::now();
        let idx = parse_moov(&moov).unwrap();
        assert_eq!(idx.video.unwrap().samples.len(), 1);
        assert!(t.elapsed() < std::time::Duration::from_secs(5), "{:?}", t.elapsed());
    }

    #[test]
    fn chunk_offsets_past_the_end_are_refused_without_overflow() {
        // A chunk at the very top of the 64-bit range: the next sample's place wraps.
        let moov = bx(b"moov", &crafted_trak(2, &[(1, 2)], &[u64::MAX - 5]));
        assert!(parse_moov(&moov).is_err(), "wrapping offsets are damage");
        // In range for the numbers, but past the end of the file.
        let moov = bx(b"moov", &crafted_trak(2, &[(1, 2)], &[1_000_000]));
        let idx = parse_moov(&moov).unwrap();
        assert!(idx.check_within(500_000).is_err());
        assert!(idx.check_within(1_000_020).is_ok());
        assert!(idx.check_within(1_000_019).is_err());
    }

    #[test]
    fn made_up_box_sizes_neither_wrap_nor_panic() {
        // A 64-bit box size that wraps the position.
        let mut big = bx(b"ftyp", &[0; 8]);
        big.extend_from_slice(&[0, 0, 0, 1]);
        big.extend_from_slice(b"free");
        big.extend_from_slice(&(u64::MAX - 4).to_be_bytes());
        big.extend_from_slice(&[0; 32]);
        assert!(find_moov(&big).is_err());
        // One that does not wrap but puts the index far past where a camera writes it.
        assert!(find_moov(&big[16..]).is_err());
        // A moov said to be larger than any real index.
        let mut huge = vec![0, 0, 0, 1];
        huge.extend_from_slice(b"moov");
        huge.extend_from_slice(&(u64::MAX).to_be_bytes());
        assert!(find_moov(&huge).is_err());
        let mut huge = vec![0, 0, 0, 1];
        huge.extend_from_slice(b"moov");
        huge.extend_from_slice(&(MAX_MOOV_BYTES + 1).to_be_bytes());
        assert!(find_moov(&huge).is_err());
        // A sample entry whose size runs far past its box.
        let mut v = trak(b"vide", b"avc1", 90_000, &[10], 1, 100, 3000, None, None);
        let at = v.windows(4).position(|w| w == b"avc1").unwrap() - 4;
        v[at..at + 4].copy_from_slice(&u32::MAX.to_be_bytes());
        assert!(parse_moov(&bx(b"moov", &v)).is_err());
    }

    /// An hev1 entry: 78 bytes of fields, then an hvcC with arrays of the
    /// given NAL types (one 3-byte unit each).
    fn hev1(types: &[u8]) -> Vec<u8> {
        let mut c = vec![1u8; 22];
        c.push(types.len() as u8);
        for &t in types {
            c.extend_from_slice(&[t, 0, 1, 0, 3, 0x40, 1, 2]);
        }
        let mut body = vec![0u8; 78];
        body.extend_from_slice(&bx(b"hvcC", &c));
        bx(b"hev1", &body)
    }

    #[test]
    fn hev1_with_its_setup_in_the_entry_becomes_hvc1() {
        let mut e = hev1(&[32, 33, 34, 39]);
        assert!(hev1_entry_to_hvc1(&mut e));
        assert_eq!(&e[4..8], b"hvc1");
        // VPS, SPS and PPS marked complete; the SEI left as it was.
        let arrays: Vec<u8> = (0..4).map(|i| e[8 + 78 + 8 + 23 + i * 8]).collect();
        assert_eq!(arrays, vec![0x80 | 32, 0x80 | 33, 0x80 | 34, 39]);
        // Already hvc1: nothing to do.
        assert!(!hev1_entry_to_hvc1(&mut e));
        // No PPS in the entry (it comes in the stream): stays hev1.
        let mut e = hev1(&[32, 33]);
        let was = e.clone();
        assert!(!hev1_entry_to_hvc1(&mut e));
        assert_eq!(e, was);
        // A unit running past the box: left alone, no panic.
        let mut e = hev1(&[32, 33, 34]);
        let n = e.len();
        e[n - 4] = 0xff;
        let was = e.clone();
        assert!(!hev1_entry_to_hvc1(&mut e));
        assert_eq!(e, was);
        for cut in 0..e.len() {
            let mut short = e[..cut].to_vec();
            hev1_entry_to_hvc1(&mut short);
        }
    }

    #[test]
    fn a_header_is_relabeled_through_its_moov() {
        let stsd = |entry: &[u8]| {
            let mut b = vec![0u8, 0, 0, 0, 0, 0, 0, 1];
            b.extend_from_slice(entry);
            bx(b"stsd", &b)
        };
        let trak = |entry: &[u8]| bx(b"trak", &bx(b"mdia", &bx(b"minf", &bx(b"stbl", &stsd(entry)))));
        let moov = bx(b"moov", &[trak(&hev1(&[32, 33, 34])), trak(&bx(b"mp4a", &[0; 28]))].concat());
        let mut head = [bx(b"ftyp", b"isom\0\0\0\0"), moov].concat();
        let len = head.len();
        assert!(hev1_to_hvc1(&mut head));
        assert_eq!(head.len(), len);
        assert!(head.windows(4).any(|w| w == b"hvc1") && !head.windows(4).any(|w| w == b"hev1"));
        assert!(!hev1_to_hvc1(&mut head));
        for cut in 0..head.len() {
            let mut short = head[..cut].to_vec();
            hev1_to_hvc1(&mut short);
        }
    }

    #[test]
    fn a_video_of_another_kind_or_no_video_is_refused() {
        let v = trak(b"vide", b"mp4v", 90_000, &[10], 1, 100, 3000, None, None);
        assert!(parse_moov(&bx(b"moov", &v)).is_err());
        let a = trak(b"soun", b"mp4a", 16_000, &[7], 1, 100, 1024, None, None);
        assert!(parse_moov(&bx(b"moov", &a)).is_err());
        // G.711 audio is left out, the video kept.
        let v = trak(b"vide", b"hvc1", 90_000, &[10], 1, 100, 3000, None, None);
        let g = trak(b"soun", b"alaw", 8_000, &[7], 1, 120, 160, None, None);
        let idx = parse_moov(&bx(b"moov", &[v, g].concat())).unwrap();
        assert!(idx.audio.is_none() && idx.video.is_some());
    }
}
