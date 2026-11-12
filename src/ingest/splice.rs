//! Virtual clips: play any time window straight from the recorded segments.
//!
//! Segments written by the native recorder are fragmented MP4 files that share
//! one codec header (`ftyp`+`moov`) and contain self-contained `moof`+`mdat`
//! fragments, each starting at a video keyframe (or continuing a long GOP).
//! A clip is the header of the first segment followed by the fragments that
//! cover the window, with fragment sequence numbers and decode times
//! rewritten so the clip starts at zero. Nothing is copied to disk; bytes are
//! produced on request, including arbitrary byte ranges for seeking.

use anyhow::{bail, Context, Result};
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};

/// Pictures at least this far apart (ms of recording) are planned from
/// each recording's first keyframe where its full layout is not at hand.
const QUICK_STEP_MS: f64 = 2500.0;
/// ... and a stretch recorded with another header this long (ms) or less
/// is passed over when the clip's header carries on after it.
const MAX_SKIP_OTHER_HEADER_MS: f64 = 120_000.0;

/// A recorded segment and its wall-clock start (ms since the Unix epoch).
#[derive(Clone, Debug)]
pub struct SourceSegment {
    pub path: PathBuf,
    pub start_ms: i64,
    /// Its wall-clock end as the index has it (ms since the Unix epoch).
    pub end_ms: i64,
    /// A lower-priority recording of the same camera: only used where no
    /// other recording covers the time.
    pub low: bool,
    /// The camera's sub stream (smaller picture).
    pub sub: bool,
}

/// Merge wall-clock spans (ms) that touch or overlap.
fn merged(mut spans: Vec<(f64, f64)>) -> Vec<(f64, f64)> {
    spans.sort_by(|a, b| a.0.total_cmp(&b.0));
    let mut out: Vec<(f64, f64)> = Vec::new();
    for (a, b) in spans {
        match out.last_mut() {
            Some(l) if a <= l.1 => l.1 = l.1.max(b),
            _ => out.push((a, b)),
        }
    }
    out
}

/// Whether `t` falls inside one of the merged `spans`.
fn inside(spans: &[(f64, f64)], t: f64) -> bool {
    let i = spans.partition_point(|s| s.1 <= t);
    spans.get(i).is_some_and(|s| s.0 <= t)
}

/// A `sidx` box (version 1) for fragments that directly follow it: per
/// fragment (size, video decode time, video duration, starts at a keyframe).
fn sidx(track_id: u32, timescale: u32, frags: &[(u64, u64, u64, bool)], first_cto: i32) -> Vec<u8> {
    let mut b = Vec::with_capacity(44 + 12 * frags.len());
    b.extend_from_slice(&0u32.to_be_bytes()); // size, filled in below
    b.extend_from_slice(b"sidx");
    b.extend_from_slice(&[1, 0, 0, 0]); // version 1, no flags
    b.extend_from_slice(&track_id.to_be_bytes());
    b.extend_from_slice(&timescale.to_be_bytes());
    // Earliest presentation time: the first sample's decode time plus its
    // composition offset (B-frames are shown after they are decoded).
    let ept = frags[0].1.saturating_add_signed(first_cto as i64);
    b.extend_from_slice(&ept.to_be_bytes());
    b.extend_from_slice(&0u64.to_be_bytes()); // first fragment right after this box
    b.extend_from_slice(&0u16.to_be_bytes());
    b.extend_from_slice(&(frags.len() as u16).to_be_bytes());
    for (i, &(size, dts, dur, key)) in frags.iter().enumerate() {
        // Up to the next fragment (gaps included), so times add up.
        let span = frags.get(i + 1).map(|n| n.1.saturating_sub(dts)).unwrap_or(dur).min(u32::MAX as u64) as u32;
        b.extend_from_slice(&((size as u32) & 0x7fff_ffff).to_be_bytes());
        b.extend_from_slice(&span.to_be_bytes());
        b.extend_from_slice(&(if key { 0x9000_0000u32 } else { 0 }).to_be_bytes());
    }
    let n = b.len() as u32;
    b[0..4].copy_from_slice(&n.to_be_bytes());
    b
}

/// Index of each segment's codec header among the distinct ones seen.
fn header_ids(parsed: &[Option<std::sync::Arc<ParsedSegment>>]) -> Vec<usize> {
    let mut seen: Vec<&[u8]> = Vec::new();
    parsed
        .iter()
        .map(|p| match p {
            Some(p) => seen.iter().position(|h| *h == p.init.as_slice()).unwrap_or_else(|| {
                seen.push(&p.init);
                seen.len() - 1
            }),
            None => usize::MAX,
        })
        .collect()
}

#[derive(Clone, Debug)]
struct TrackMeta {
    id: u32,
    timescale: u32,
    video: bool,
}

#[derive(Clone, Debug)]
struct TrafInfo {
    track_id: u32,
    /// Offset of the 8-byte tfdt value within the moof.
    tfdt_pos: usize,
    tfdt: u64,
    duration: u64,
    first_is_sync: bool,
    /// First sample: offset from the moof start (default-base-is-moof) and size.
    first_sample: Option<(u64, u32)>,
    /// First sample's composition offset (shown this long after it is
    /// decoded: B-frames), in the track's timescale.
    first_cto: i32,
}

#[derive(Clone, Debug)]
struct FragmentInfo {
    offset: u64,
    /// moof + mdat length.
    len: u64,
    /// Offset of the 4-byte sequence number within the moof.
    seq_pos: usize,
    trafs: Vec<TrafInfo>,
}

/// The largest single picture we will read from a recording (a 4K keyframe
/// is a few MB at most). A bigger number in a file means the file (or its
/// saved layout) is damaged, and trusting it could use up all the memory.
const MAX_PICTURE_BYTES: u32 = 16 * 1024 * 1024;

impl FragmentInfo {
    /// Forget where a fragment's first picture is when that place does not
    /// make sense: empty, larger than any real picture, or reaching past the
    /// end of the fragment (a damaged file, or a saved layout that no longer
    /// matches). Such a fragment can still be played whole; it just is not
    /// used for the one-picture-at-a-time fast playback.
    fn checked(mut self) -> Self {
        let len = self.len;
        for t in &mut self.trafs {
            if let Some((off, size)) = t.first_sample {
                let fits = off.checked_add(size as u64).is_some_and(|end| end <= len);
                if size == 0 || size > MAX_PICTURE_BYTES || !fits {
                    t.first_sample = None;
                }
            }
        }
        self
    }
}

struct ParsedSegment {
    init: Vec<u8>,
    tracks: Vec<TrackMeta>,
    fragments: Vec<FragmentInfo>,
}

mod export;
mod index;
pub use index::{forget, moved, open_index, remember};

fn be32(b: &[u8], at: usize) -> Result<u32> {
    Ok(u32::from_be_bytes(b.get(at..at + 4).context("truncated box")?.try_into().unwrap()))
}
fn be64(b: &[u8], at: usize) -> Result<u64> {
    Ok(u64::from_be_bytes(b.get(at..at + 8).context("truncated box")?.try_into().unwrap()))
}

/// Child boxes of a container payload: (type, start of box, end of box).
fn children(b: &[u8], from: usize, to: usize) -> Result<Vec<([u8; 4], usize, usize)>> {
    let mut out = Vec::new();
    let mut i = from;
    while i + 8 <= to {
        let size = be32(b, i)? as usize;
        if size < 8 || i + size > to {
            bail!("bad box size");
        }
        out.push((b[i + 4..i + 8].try_into().unwrap(), i, i + size));
        i += size;
    }
    Ok(out)
}

fn find(b: &[u8], from: usize, to: usize, kind: &[u8; 4]) -> Result<Option<(usize, usize)>> {
    Ok(children(b, from, to)?.into_iter().find(|(k, _, _)| k == kind).map(|(_, s, e)| (s, e)))
}

fn parse_moov(moov: &[u8]) -> Result<Vec<TrackMeta>> {
    let mut tracks = Vec::new();
    for (kind, s, e) in children(moov, 8, moov.len())? {
        if &kind != b"trak" {
            continue;
        }
        let (ts, _) = find(moov, s + 8, e, b"tkhd")?.context("trak without tkhd")?;
        // tkhd: version(1) flags(3), then v0: ctime(4) mtime(4) track_id(4).
        let version = moov[ts + 8];
        let id = be32(moov, ts + 8 + 4 + if version == 1 { 16 } else { 8 })?;
        let (ms, me) = find(moov, s + 8, e, b"mdia")?.context("trak without mdia")?;
        let (hs, _) = find(moov, ms + 8, me, b"mdhd")?.context("mdia without mdhd")?;
        let mv = moov[hs + 8];
        let timescale = be32(moov, hs + 8 + 4 + if mv == 1 { 16 } else { 8 })?;
        let (ds, _) = find(moov, ms + 8, me, b"hdlr")?.context("mdia without hdlr")?;
        let video = &moov[ds + 16..ds + 20] == b"vide";
        tracks.push(TrackMeta { id, timescale, video });
    }
    Ok(tracks)
}

fn parse_moof(moof: &[u8]) -> Result<(usize, Vec<TrafInfo>)> {
    let (ms, _) = find(moof, 8, moof.len(), b"mfhd")?.context("moof without mfhd")?;
    let seq_pos = ms + 12;
    let mut trafs = Vec::new();
    for (kind, s, e) in children(moof, 8, moof.len())? {
        if &kind != b"traf" {
            continue;
        }
        let (hs, _) = find(moof, s + 8, e, b"tfhd")?.context("traf without tfhd")?;
        let track_id = be32(moof, hs + 12)?;
        let (ds, _) = find(moof, s + 8, e, b"tfdt")?.context("traf without tfdt")?;
        if moof[ds + 8] != 1 {
            bail!("unsupported tfdt version");
        }
        let tfdt_pos = ds + 12;
        let tfdt = be64(moof, tfdt_pos)?;
        let tfhd_flags = be32(moof, hs + 8)? & 0x00ff_ffff;
        // tfhd optional fields after track_id: base offset(8), desc index(4),
        // default duration(4), default size(4), default flags(4).
        let mut q = hs + 16;
        let explicit_base = tfhd_flags & 0x1 != 0;
        for (bit, n) in [(0x1u32, 8usize), (0x2, 4), (0x8, 4)] {
            if tfhd_flags & bit != 0 {
                q += n;
            }
        }
        let default_size = if tfhd_flags & 0x10 != 0 { Some(be32(moof, q)?) } else { None };
        let (rs, _) = find(moof, s + 8, e, b"trun")?.context("traf without trun")?;
        let flags = be32(moof, rs + 8)? & 0x00ff_ffff;
        let count = be32(moof, rs + 12)? as usize;
        let mut p = rs + 16;
        let mut data_offset = None;
        if flags & 0x1 != 0 {
            data_offset = Some(be32(moof, p)? as i32);
            p += 4;
        }
        let mut size0 = default_size;
        let mut first_flags = None;
        if flags & 0x4 != 0 {
            first_flags = Some(be32(moof, p)?);
            p += 4;
        }
        let mut duration = 0u64;
        let mut sample0_flags = None;
        let mut first_cto = 0i32;
        for k in 0..count {
            if flags & 0x100 != 0 {
                duration += be32(moof, p)? as u64;
                p += 4;
            }
            if flags & 0x200 != 0 {
                if k == 0 {
                    size0 = Some(be32(moof, p)?);
                }
                p += 4;
            }
            if flags & 0x400 != 0 {
                if k == 0 {
                    sample0_flags = Some(be32(moof, p)?);
                }
                p += 4;
            }
            if flags & 0x800 != 0 {
                if k == 0 {
                    // Signed in trun version 1; version 0 offsets are unsigned
                    // but in practice small.
                    first_cto = be32(moof, p)? as i32;
                }
                p += 4;
            }
        }
        let f0 = first_flags.or(sample0_flags).unwrap_or(0);
        let first_is_sync = f0 & 0x0001_0000 == 0; // is_non_sync_sample bit clear
        // Only the layout our recorder writes (offsets relative to the moof).
        let first_sample = match (explicit_base, data_offset, size0) {
            (false, Some(off), Some(sz)) if off > 0 && count > 0 => Some((off as u64, sz)),
            _ => None,
        };
        trafs.push(TrafInfo { track_id, tfdt_pos, tfdt, duration, first_is_sync, first_sample, first_cto });
    }
    Ok((seq_pos, trafs))
}

/// A recording's layout (its header and where each fragment is), read once:
/// a finished recording never changes, and reading it means a disk seek per
/// fragment, which on a big hard disk makes every clip slow to start. Kept
/// for the most recently used files; a file that changed (its size or time)
/// is read again.
/// How recordings not in memory were opened: from the saved index (hits)
/// or by reading the file's headers (misses), with the time each took.
pub struct LoadStats {
    hits: std::sync::atomic::AtomicU64,
    hit_us: std::sync::atomic::AtomicU64,
    misses: std::sync::atomic::AtomicU64,
    miss_us: std::sync::atomic::AtomicU64,
}
impl LoadStats {
    fn hit(&self, d: std::time::Duration) {
        use std::sync::atomic::Ordering::Relaxed;
        self.hits.fetch_add(1, Relaxed);
        self.hit_us.fetch_add(d.as_micros() as u64, Relaxed);
    }
    fn miss(&self, d: std::time::Duration) {
        use std::sync::atomic::Ordering::Relaxed;
        self.misses.fetch_add(1, Relaxed);
        self.miss_us.fetch_add(d.as_micros() as u64, Relaxed);
    }
    /// (index hits, their ms, misses, their ms) so far.
    pub fn get(&self) -> (u64, u64, u64, u64) {
        use std::sync::atomic::Ordering::Relaxed;
        (self.hits.load(Relaxed), self.hit_us.load(Relaxed) / 1000, self.misses.load(Relaxed), self.miss_us.load(Relaxed) / 1000)
    }
}
pub static LOAD_STATS: LoadStats = LoadStats {
    hits: std::sync::atomic::AtomicU64::new(0),
    hit_us: std::sync::atomic::AtomicU64::new(0),
    misses: std::sync::atomic::AtomicU64::new(0),
    miss_us: std::sync::atomic::AtomicU64::new(0),
};

fn parse_segment(path: &Path) -> Result<std::sync::Arc<ParsedSegment>> {
    parse_segment_opt(path, true)?.context("recording layout unavailable")
}

/// A recording's layout from memory or the saved index; with `disk`, read
/// from the file when neither has it (else None).
fn parse_segment_opt(path: &Path, disk: bool) -> Result<Option<std::sync::Arc<ParsedSegment>>> {
    use std::collections::{HashMap, VecDeque};
    use std::sync::{Arc, Mutex, OnceLock};
    type Stamp = (u64, Option<std::time::SystemTime>);
    struct Cache {
        map: HashMap<PathBuf, (Stamp, Arc<ParsedSegment>)>,
        order: VecDeque<PathBuf>,
        bytes: usize,
    }
    // An hour of 10-second recordings of 15 cameras is ~5,400 files; the
    // memory budget is what normally limits it (long recordings hold more).
    const KEEP: usize = 20_000;
    const BUDGET: usize = 96 * 1024 * 1024;
    fn size_of(p: &ParsedSegment) -> usize {
        p.init.len() + p.fragments.iter().map(|f| f.trafs.len() * 64 + 64).sum::<usize>() + 128
    }
    static CACHE: OnceLock<Mutex<Cache>> = OnceLock::new();
    let meta = std::fs::metadata(path).with_context(|| format!("open {}", path.display()))?;
    let stamp: Stamp = (meta.len(), meta.modified().ok());
    let cache = CACHE.get_or_init(|| Mutex::new(Cache { map: HashMap::new(), order: VecDeque::new(), bytes: 0 }));
    if let Some((s, p)) = cache.lock().unwrap().map.get(path) {
        if *s == stamp {
            return Ok(Some(p.clone()));
        }
    }
    // The saved index (made when the recording was closed) saves reading
    // the file's headers; a recording that changed since is read again.
    let t0 = std::time::Instant::now();
    let parsed = Arc::new(match index::load(path, stamp) {
        Some(p) => {
            LOAD_STATS.hit(t0.elapsed());
            p
        }
        None if !disk => return Ok(None),
        None => {
            let p = parse_segment_file(path)?;
            LOAD_STATS.miss(t0.elapsed());
            index::store(path, stamp, &p);
            p
        }
    });
    let mut c = cache.lock().unwrap();
    let size = size_of(&parsed);
    match c.map.insert(path.to_path_buf(), (stamp, parsed.clone())) {
        Some((_, old)) => c.bytes = c.bytes - size_of(&old).min(c.bytes) + size,
        None => {
            c.order.push_back(path.to_path_buf());
            c.bytes += size;
        }
    }
    while c.order.len() > KEEP || c.bytes > BUDGET {
        let Some(old) = c.order.pop_front() else { break };
        if let Some((_, p)) = c.map.remove(&old) {
            c.bytes -= size_of(&p).min(c.bytes);
        }
    }
    Ok(Some(parsed))
}

/// The header and first fragment of a recording (its first keyframe: a
/// recording starts with one), read from the start of the file: one short
/// read on a hard disk instead of a seek per fragment. Kept in memory.
fn segment_head(path: &Path) -> Result<std::sync::Arc<ParsedSegment>> {
    use std::collections::{HashMap, VecDeque};
    use std::sync::{Arc, Mutex, OnceLock};
    type Stamp = (u64, Option<std::time::SystemTime>);
    const KEEP: usize = 8_000;
    type Heads = (HashMap<PathBuf, (Stamp, Arc<ParsedSegment>)>, VecDeque<PathBuf>);
    static CACHE: OnceLock<Mutex<Heads>> = OnceLock::new();
    let meta = std::fs::metadata(path).with_context(|| format!("open {}", path.display()))?;
    let stamp: Stamp = (meta.len(), meta.modified().ok());
    let cache = CACHE.get_or_init(Default::default);
    if let Some((s, p)) = cache.lock().unwrap().0.get(path) {
        if *s == stamp {
            return Ok(p.clone());
        }
    }
    let p = Arc::new(parse_segment_file_upto(path, Some(1))?);
    let mut c = cache.lock().unwrap();
    if c.0.insert(path.to_path_buf(), (stamp, p.clone())).is_none() {
        c.1.push_back(path.to_path_buf());
    }
    while c.1.len() > KEEP {
        let Some(old) = c.1.pop_front() else { break };
        c.0.remove(&old);
    }
    Ok(p)
}

fn parse_segment_file(path: &Path) -> Result<ParsedSegment> {
    parse_segment_file_upto(path, None)
}

/// Put a recording's layout in the saved index if it is not there yet
/// (true if it was added). Does not fill the memory cache.
pub fn index_recording(path: &Path) -> Result<bool> {
    if !index::available() {
        return Ok(false);
    }
    let meta = std::fs::metadata(path).with_context(|| format!("open {}", path.display()))?;
    let stamp = (meta.len(), meta.modified().ok());
    if index::has(path, stamp) {
        return Ok(false);
    }
    let p = parse_segment_file(path)?;
    index::store(path, stamp, &p);
    Ok(true)
}

/// Read a recording's layout; with `max_frags`, stop after that many fragments.
fn parse_segment_file_upto(path: &Path, max_frags: Option<usize>) -> Result<ParsedSegment> {
    let mut f = std::fs::File::open(path).with_context(|| format!("open {}", path.display()))?;
    let file_len = f.metadata()?.len();
    let mut init = Vec::new();
    let mut tracks = Vec::new();
    let mut fragments = Vec::new();
    let mut pos = 0u64;
    let mut hdr = [0u8; 8];
    while pos + 8 <= file_len {
        f.seek(SeekFrom::Start(pos))?;
        f.read_exact(&mut hdr)?;
        let size = u32::from_be_bytes(hdr[0..4].try_into().unwrap()) as u64;
        let kind: [u8; 4] = hdr[4..8].try_into().unwrap();
        if size < 8 || pos + size > file_len {
            break; // torn tail after a crash: keep what is complete
        }
        // Header boxes are small; a damaged size must not make us allocate
        // the whole file.
        const MAX_HEADER_BOX: u64 = 16 * 1024 * 1024;
        if matches!(&kind, b"ftyp" | b"moov" | b"moof") && size > MAX_HEADER_BOX {
            break;
        }
        match &kind {
            b"ftyp" | b"moov" => {
                let mut b = vec![0u8; size as usize];
                f.seek(SeekFrom::Start(pos))?;
                f.read_exact(&mut b)?;
                if &kind == b"moov" {
                    tracks = parse_moov(&b)?;
                }
                init.extend_from_slice(&b);
            }
            b"moof" => {
                let mut moof = vec![0u8; size as usize];
                f.seek(SeekFrom::Start(pos))?;
                f.read_exact(&mut moof)?;
                // The paired mdat follows directly.
                let mdat_pos = pos + size;
                if mdat_pos + 8 > file_len {
                    break;
                }
                f.seek(SeekFrom::Start(mdat_pos))?;
                f.read_exact(&mut hdr)?;
                let mdat_len = u32::from_be_bytes(hdr[0..4].try_into().unwrap()) as u64;
                if &hdr[4..8] != b"mdat" || mdat_len < 8 || mdat_pos + mdat_len > file_len {
                    break;
                }
                let (seq_pos, trafs) = parse_moof(&moof)?;
                fragments.push(FragmentInfo { offset: pos, len: size + mdat_len, seq_pos, trafs }.checked());
                pos = mdat_pos + mdat_len;
                if max_frags.is_some_and(|m| fragments.len() >= m) {
                    break;
                }
                continue;
            }
            _ => {}
        }
        pos += size;
    }
    if tracks.is_empty() {
        bail!("{} is not a fragmented MP4 recording", path.display());
    }
    Ok(ParsedSegment { init, tracks, fragments })
}

/// The video samples of a fragment (moof + mdat as recorded): each one's
/// bytes and composition offset. None for a layout the recorder does not
/// write.
fn video_samples(frag: &[u8], video_id: u32) -> Option<Vec<(&[u8], i32)>> {
    let moof_len = u32::from_be_bytes(frag.get(0..4)?.try_into().ok()?) as usize;
    let moof = frag.get(..moof_len)?;
    for (kind, s, e) in children(moof, 8, moof_len).ok()? {
        if &kind != b"traf" {
            continue;
        }
        let (hs, _) = find(moof, s + 8, e, b"tfhd").ok()??;
        let tfhd_flags = be32(moof, hs + 8).ok()? & 0x00ff_ffff;
        if be32(moof, hs + 12).ok()? != video_id {
            continue;
        }
        // Only default-base-is-moof with per-sample sizes, as recorded.
        if tfhd_flags & 0x1 != 0 {
            return None;
        }
        let (rs, _) = find(moof, s + 8, e, b"trun").ok()??;
        let flags = be32(moof, rs + 8).ok()? & 0x00ff_ffff;
        if flags & 0x1 == 0 || flags & 0x200 == 0 {
            return None;
        }
        let count = be32(moof, rs + 12).ok()? as usize;
        let mut data = be32(moof, rs + 16).ok()? as usize;
        let mut p = rs + 20 + if flags & 0x4 != 0 { 4 } else { 0 };
        let mut out = Vec::with_capacity(count.min(10_000));
        for _ in 0..count {
            if flags & 0x100 != 0 {
                p += 4;
            }
            let size = be32(moof, p).ok()? as usize;
            p += 4;
            if flags & 0x400 != 0 {
                p += 4;
            }
            let mut cto = 0;
            if flags & 0x800 != 0 {
                cto = be32(moof, p).ok()? as i32;
                p += 4;
            }
            out.push((frag.get(data..data + size)?, cto));
            data += size;
        }
        return Some(out);
    }
    None
}

/// Frames a recording must show in decode order before its header is made
/// low delay.
const LOW_DELAY_MIN_FRAMES: u32 = 10;

/// The header to serve for a clip of a recording that starts with fragment
/// `frag`: the recording's own, or, when its frames are never reordered (no
/// B-frames), one whose H.264 SPS says so, so that the browser's decoder
/// shows each frame at once instead of holding up to 16 (see `sps.rs`).
/// Decided from the frames of that fragment (one group of pictures, which
/// the browser reads first anyway), so the same window always gets the same
/// header. B-frame streams keep theirs.
fn low_delay_header(p: &ParsedSegment, path: &Path, frag: &FragmentInfo) -> Vec<u8> {
    type Verdicts = std::collections::HashMap<(PathBuf, u64), bool>;
    static SEEN: std::sync::OnceLock<std::sync::Mutex<Verdicts>> = std::sync::OnceLock::new();
    let Some(patched) = crate::ingest::sps::low_delay_init(&p.init) else {
        return p.init.clone(); // H.265, or nothing to add
    };
    let seen = SEEN.get_or_init(Default::default);
    let key = (path.to_path_buf(), frag.offset);
    let known = seen.lock().unwrap().get(&key).copied();
    let ok = known.unwrap_or_else(|| {
        let ok = never_reorders(p, path, frag);
        let mut s = seen.lock().unwrap();
        if s.len() >= 1024 {
            s.clear();
        }
        s.insert(key, ok);
        ok
    });
    if ok {
        patched
    } else {
        p.init.clone()
    }
}

/// True when fragment `frag` of the recording (up to 8 MB) shows each of
/// its frames in decode order, and has enough of them to tell.
fn never_reorders(p: &ParsedSegment, path: &Path, frag: &FragmentInfo) -> bool {
    let Some(video) = p.tracks.iter().find(|t| t.video) else { return false };
    let Some(avcc) = crate::ingest::sps::init_avcc(&p.init) else { return false };
    if frag.len > 8 * 1024 * 1024 {
        return false;
    }
    let mut b = vec![0u8; frag.len as usize];
    let read = std::fs::File::open(path).and_then(|mut f| {
        f.seek(SeekFrom::Start(frag.offset))?;
        f.read_exact(&mut b)
    });
    let Some(samples) = read.ok().and_then(|_| video_samples(&b, video.id)) else { return false };
    let mut check = crate::ingest::sps::ReorderCheck::new(avcc);
    for (data, cto) in samples {
        check.frame(data, cto);
    }
    check.never_reorders(LOW_DELAY_MIN_FRAMES)
}

/// `ftyp` + a `moov` holding only the video track (and its `trex`).
fn video_only_init(init: &[u8], video_id: u32) -> Result<Vec<u8>> {
    let track_id_of = |b: &[u8], s: usize, e: usize| -> Result<u32> {
        let (ts, _) = find(b, s + 8, e, b"tkhd")?.context("trak without tkhd")?;
        let v = b[ts + 8];
        be32(b, ts + 8 + 4 + if v == 1 { 16 } else { 8 })
    };
    let mut out = Vec::new();
    for (kind, s, e) in children(init, 0, init.len())? {
        if &kind != b"moov" {
            out.extend_from_slice(&init[s..e]);
            continue;
        }
        let mut body = Vec::new();
        for (k, cs, ce) in children(init, s + 8, e)? {
            match &k {
                b"trak" if track_id_of(init, cs, ce)? != video_id => {}
                b"mvex" => {
                    let mut mv = Vec::new();
                    for (k2, xs, xe) in children(init, cs + 8, ce)? {
                        // trex: header(8) version/flags(4) track_id(4)
                        if &k2 == b"trex" && be32(init, xs + 12)? != video_id {
                            continue;
                        }
                        mv.extend_from_slice(&init[xs..xe]);
                    }
                    body.extend_from_slice(&((mv.len() + 8) as u32).to_be_bytes());
                    body.extend_from_slice(b"mvex");
                    body.extend_from_slice(&mv);
                }
                _ => body.extend_from_slice(&init[cs..ce]),
            }
        }
        out.extend_from_slice(&((body.len() + 8) as u32).to_be_bytes());
        out.extend_from_slice(b"moov");
        out.extend_from_slice(&body);
    }
    Ok(out)
}

/// `moof` for one keyframe sample plus the `mdat` header; the sample data follows.
fn trick_fragment(seq: u32, track_id: u32, dts: u64, duration: u32, size: u32) -> Vec<u8> {
    let mut traf = Vec::new();
    // tfhd: default-base-is-moof
    traf.extend_from_slice(&16u32.to_be_bytes());
    traf.extend_from_slice(b"tfhd");
    traf.extend_from_slice(&0x0002_0000u32.to_be_bytes());
    traf.extend_from_slice(&track_id.to_be_bytes());
    traf.extend_from_slice(&20u32.to_be_bytes());
    traf.extend_from_slice(b"tfdt");
    traf.extend_from_slice(&0x0100_0000u32.to_be_bytes());
    traf.extend_from_slice(&dts.to_be_bytes());
    // trun: data-offset, duration, size, flags; one sample
    traf.extend_from_slice(&32u32.to_be_bytes());
    traf.extend_from_slice(b"trun");
    traf.extend_from_slice(&0x0000_0701u32.to_be_bytes());
    traf.extend_from_slice(&1u32.to_be_bytes());
    let off_pos = traf.len();
    traf.extend_from_slice(&0u32.to_be_bytes());
    traf.extend_from_slice(&duration.to_be_bytes());
    traf.extend_from_slice(&size.to_be_bytes());
    traf.extend_from_slice(&0x0200_0000u32.to_be_bytes()); // sync, depends on nothing
    let moof_len = 8 + 16 + 8 + traf.len();
    let data_offset = (moof_len + 8) as u32;
    traf[off_pos..off_pos + 4].copy_from_slice(&data_offset.to_be_bytes());
    let mut b = Vec::with_capacity(moof_len + 8);
    b.extend_from_slice(&(moof_len as u32).to_be_bytes());
    b.extend_from_slice(b"moof");
    b.extend_from_slice(&16u32.to_be_bytes());
    b.extend_from_slice(b"mfhd");
    b.extend_from_slice(&0u32.to_be_bytes());
    b.extend_from_slice(&seq.to_be_bytes());
    b.extend_from_slice(&((traf.len() + 8) as u32).to_be_bytes());
    b.extend_from_slice(b"traf");
    b.extend_from_slice(&traf);
    b.extend_from_slice(&(size + 8).to_be_bytes());
    b.extend_from_slice(b"mdat");
    b
}

#[derive(Clone)]
enum Piece {
    Bytes(Vec<u8>),
    /// Raw bytes copied from `path`.
    Slice { path: PathBuf, offset: u64, len: u64 },
    /// A fragment copied from `path`, with numbers in its moof replaced:
    /// (offset in the fragment, big-endian value, width 4 or 8 bytes).
    Fragment { path: PathBuf, offset: u64, len: u64, patches: Vec<(u32, u64, u8)> },
}

impl Piece {
    fn len(&self) -> u64 {
        match self {
            Piece::Bytes(b) => b.len() as u64,
            Piece::Slice { len, .. } => *len,
            Piece::Fragment { len, .. } => *len,
        }
    }
}

/// A playable MP4 assembled on request from recorded segments.
#[derive(Clone)]
pub struct VirtualClip {
    pieces: Vec<Piece>,
    total: u64,
    /// Wall-clock span actually covered (keyframe-aligned), ms since epoch.
    #[allow(dead_code)]
    pub start_ms: i64,
    #[allow(dead_code)]
    pub end_ms: i64,
    /// Fast-playback clips: the wall-clock time of each picture, in order.
    pub frame_times: Vec<i64>,
    /// The clip is the sub stream.
    pub sub: bool,
    /// Wall-clock span (ms) of each fragment piece, in order (plan() clips).
    frag_wall: Vec<(i64, i64)>,
    /// The first video sample's composition offset (B-frames), track timescale.
    first_cto: i32,
}

impl VirtualClip {
    /// Build a clip covering [from_ms, to_ms] from `segments`. Starts at the
    /// keyframe at or before `from_ms`. Low-quality (sub stream) fragments
    /// are dropped where another recording covers the time. Stops early if
    /// the codec header changes (the camera changed resolution, or playback
    /// passes between the sub and the main stream): the caller asks again
    /// from the clip's end. Returns None when nothing overlaps the window.
    pub fn plan(segments: &[SourceSegment], from_ms: i64, to_ms: i64) -> Result<Option<VirtualClip>> {
        // Gather every fragment with its wall-clock time.
        struct Cand {
            seg: usize,
            frag: FragmentInfo,
            start_ms: f64,
            end_ms: f64,
            sync: bool,
        }
        let mut parsed = Vec::new();
        let mut cands: Vec<Cand> = Vec::new();
        for (i, s) in segments.iter().enumerate() {
            let p = match parse_segment(&s.path) {
                Ok(p) => p,
                Err(e) => {
                    tracing::warn!("clip: skipping {}: {e:#}", s.path.display());
                    parsed.push(None);
                    continue;
                }
            };
            let Some(video) = p.tracks.iter().find(|t| t.video).cloned() else {
                parsed.push(None);
                continue;
            };
            for frag in &p.fragments {
                let Some(vt) = frag.trafs.iter().find(|t| t.track_id == video.id) else { continue };
                let ts = video.timescale.max(1) as f64;
                let start = s.start_ms as f64 + vt.tfdt as f64 * 1000.0 / ts;
                let end = start + vt.duration as f64 * 1000.0 / ts;
                cands.push(Cand { seg: i, frag: frag.clone(), start_ms: start, end_ms: end, sync: vt.first_is_sync });
            }
            parsed.push(Some(p));
        }
        if segments.iter().any(|s| s.low) {
            let high = merged(cands.iter().filter(|c| !segments[c.seg].low).map(|c| (c.start_ms, c.end_ms)).collect());
            cands.retain(|c| !segments[c.seg].low || !inside(&high, (c.start_ms + c.end_ms) / 2.0));
        }
        cands.sort_by(|a, b| a.start_ms.total_cmp(&b.start_ms));
        let hid = header_ids(&parsed);
        // (A fragment ending within 1 ms of `from` is where the previous clip
        // ended, its end rounded: start after it.)
        let Some(mut first) = cands.iter().position(|c| c.end_ms > from_ms as f64 + 1.0 && c.start_ms < to_ms as f64) else {
            return Ok(None);
        };
        // A clip must begin at a keyframe recorded with the same header.
        while first > 0 && !cands[first].sync && hid[cands[first - 1].seg] == hid[cands[first].seg] {
            first -= 1;
        }
        if !cands[first].sync {
            let h = hid[cands[first].seg];
            match cands.iter().skip(first).position(|c| c.sync && hid[c.seg] == h) {
                Some(k) if cands[first + k].start_ms < to_ms as f64 => first += k,
                _ => return Ok(None),
            }
        }
        let base_seg = cands[first].seg;
        let header = parsed[base_seg].as_ref().unwrap();
        let tracks = header.tracks.clone();
        let clip_start = cands[first].start_ms;

        let mut pieces = vec![Piece::Bytes(low_delay_header(header, &segments[base_seg].path, &cands[first].frag))];
        let mut running_end: Vec<u64> = vec![0; tracks.len()];
        // Per (segment, track): output decode time of that track's first
        // fragment in the segment, and its source tfdt.
        let mut seg_base: std::collections::HashMap<(usize, u32), (u64, u64)> = Default::default();
        let mut seq = 0u32;
        let mut end_ms = clip_start;
        let vid = tracks.iter().position(|t| t.video);
        // Per fragment written: (size, video decode time, video duration, starts at a keyframe), for the index.
        let mut index: Vec<(u64, u64, u64, bool)> = Vec::new();
        let mut first_cto = 0i32;
        let mut frag_wall: Vec<(i64, i64)> = Vec::new();
        for c in cands.iter().skip(first) {
            if c.start_ms >= to_ms as f64 {
                break;
            }
            if parsed[c.seg].as_ref().unwrap().init != header.init {
                break; // different codec header: cannot continue in one file
            }
            let mut patches = Vec::with_capacity(1 + c.frag.trafs.len());
            seq += 1;
            patches.push((c.frag.seq_pos as u32, seq as u64, 4u8));
            for t in &c.frag.trafs {
                let Some(ti) = tracks.iter().position(|m| m.id == t.track_id) else { continue };
                let ts = tracks[ti].timescale.max(1) as f64;
                let (out_base, src_base) = *seg_base.entry((c.seg, t.track_id)).or_insert_with(|| {
                    // Place the segment by wall clock (keeps audio and video
                    // aligned and preserves real gaps), never overlapping
                    // what was already written for this track.
                    let wall = segments[c.seg].start_ms as f64 + t.tfdt as f64 * 1000.0 / ts;
                    let want = ((wall - clip_start).max(0.0) * ts / 1000.0).round() as u64;
                    (want.max(running_end[ti]), t.tfdt)
                });
                let out = (out_base + t.tfdt.saturating_sub(src_base)).max(running_end[ti]);
                patches.push((t.tfdt_pos as u32, out, 8u8));
                running_end[ti] = out + t.duration;
                if Some(ti) == vid {
                    if index.is_empty() {
                        first_cto = t.first_cto;
                    }
                    index.push((c.frag.len, out, t.duration, c.sync));
                }
            }
            end_ms = c.end_ms;
            frag_wall.push((c.start_ms.round() as i64, c.end_ms.round() as i64));
            pieces.push(Piece::Fragment {
                path: segments[c.seg].path.clone(),
                offset: c.frag.offset,
                len: c.frag.len,
                patches,
            });
        }
        // A segment index right after the header: where each fragment is and
        // how long it plays, so a player can jump to the moment it wants
        // instead of reading through the clip fragment by fragment (each a
        // separate request over the network).
        if let (Some(v), true) = (vid, index.len() == pieces.len() - 1 && !index.is_empty() && index.len() < 65_536) {
            pieces.insert(1, Piece::Bytes(sidx(tracks[v].id, tracks[v].timescale, &index, first_cto)));
        }
        let total = pieces.iter().map(|p| p.len()).sum();
        let sub = segments[base_seg].sub;
        Ok(Some(VirtualClip { pieces, total, start_ms: clip_start.round() as i64, end_ms: end_ms.round() as i64, frame_times: Vec::new(), sub, frag_wall, first_cto }))
    }

    /// As `plan`, for a window planned before whose clip then ended at
    /// `earlier_end_ms`: the same pieces again, so byte positions the
    /// browser already holds still mean the same thing. (While the window is
    /// still being recorded, every new piece makes the clip's table of
    /// contents at the front longer, which would move all later bytes.) The
    /// browser asks for the rest from the clip's end, as usual.
    pub fn plan_like(segments: &[SourceSegment], from_ms: i64, to_ms: i64, earlier_end_ms: Option<i64>) -> Result<Option<VirtualClip>> {
        if let Some(end) = earlier_end_ms.filter(|e| *e - 1 > from_ms && *e - 1 < to_ms) {
            // (Up to just before the old end: the next piece starts at that
            // end, and the end was rounded to the millisecond.)
            if let Some(c) = Self::plan(segments, from_ms, end - 1)? {
                return Ok(Some(c));
            }
        }
        Self::plan(segments, from_ms, to_ms)
    }

    /// Fast-playback ("trick play") clip: only keyframes, about one per
    /// `speed * frame_ms` of recording (the keyframe nearest to when each is
    /// due, never closer than half that), retimed so the clip plays at normal
    /// speed while covering the window `speed` times faster. Video only.
    /// Clip time t (s) shows the recording at `start_ms + t * speed * 1000`.
    pub fn plan_trick(segments: &[SourceSegment], from_ms: i64, to_ms: i64, speed: u32, frame_ms: u32) -> Result<Option<VirtualClip>> {
        Self::plan_trick_until(segments, from_ms, to_ms, speed, frame_ms, &|| false)
    }

    /// As `plan_trick`; stops (with an error) once `cancelled` says so, e.g.
    /// when the browser no longer waits for the clip.
    pub fn plan_trick_until(segments: &[SourceSegment], from_ms: i64, to_ms: i64, speed: u32, frame_ms: u32, cancelled: &dyn Fn() -> bool) -> Result<Option<VirtualClip>> {
        let speed = speed.max(1) as f64;
        let step = speed * frame_ms as f64;
        // Every keyframe in the window: (wall ms, file, data offset, size, header, low).
        struct Key {
            wall: f64,
            path: PathBuf,
            off: u64,
            size: u32,
            header: usize,
            low: bool,
        }
        let mut headers: Vec<(Vec<u8>, TrackMeta)> = Vec::new();
        let mut keys: Vec<Key> = Vec::new();
        let mut high: Vec<(f64, f64)> = Vec::new();
        // Far apart pictures (fast playback, filmstrips, time-lapse): a
        // recording whose layout is not known yet gives only its first
        // keyframe, read from the start of the file, and recordings that
        // would add no picture are not opened at all. A hard disk can then
        // keep up with many cameras at once (one short read per picture
        // instead of a seek per fragment of every recording).
        let quick = step >= QUICK_STEP_MS;
        let mut order: Vec<&SourceSegment> = segments.iter().collect();
        order.sort_by_key(|s| s.start_ms);
        // Where the main recordings are, from the index: where the sub
        // stream's pictures must not be used. (Not only from the parts
        // actually opened: far apart pictures open few of them.)
        if quick {
            high.extend(order.iter().filter(|s| !s.low && s.end_ms > s.start_ms).map(|s| (s.start_ms as f64, s.end_ms as f64)));
        }
        // Per recording: when the next one of the same priority starts.
        let mut next_start = vec![None; order.len()];
        let mut later: [Option<f64>; 2] = [None; 2];
        for (j, s) in order.iter().enumerate().rev() {
            next_start[j] = later[s.low as usize];
            later[s.low as usize] = Some(s.start_ms as f64);
        }
        // Per priority (main/sub): the start of the last recording used.
        let mut last_used: [Option<f64>; 2] = [None; 2];
        for (j, s) in order.into_iter().enumerate() {
            if cancelled() {
                bail!("cancelled");
            }
            let known = if quick { parse_segment_opt(&s.path, false).ok().flatten() } else { None };
            let p = match known {
                Some(p) => Ok(p),
                None if quick => {
                    // Only this recording's first picture is at hand. It is
                    // not needed when it comes too soon after the last one,
                    // or when the next recording starts nearer to where the
                    // next picture is due (the same choice as below).
                    if let Some(last) = last_used[s.low as usize] {
                        let at = s.start_ms as f64;
                        let due = last + step;
                        let nearer_next = next_start[j].is_some_and(|n: f64| (n - due).abs() <= (at - due).abs());
                        if at - last < step * 0.5 || nearer_next {
                            continue;
                        }
                    }
                    segment_head(&s.path)
                }
                None => parse_segment(&s.path),
            };
            let p = match p {
                Ok(p) => p,
                Err(e) => {
                    tracing::warn!("trick clip: skipping {}: {e:#}", s.path.display());
                    continue;
                }
            };
            last_used[s.low as usize] = Some(s.start_ms as f64);
            let Some(video) = p.tracks.iter().find(|t| t.video).cloned() else { continue };
            let header = headers.iter().position(|(h, _)| *h == p.init).unwrap_or_else(|| {
                headers.push((p.init.clone(), video.clone()));
                headers.len() - 1
            });
            let ts = video.timescale.max(1) as f64;
            for frag in &p.fragments {
                let Some(vt) = frag.trafs.iter().find(|t| t.track_id == video.id) else { continue };
                let wall = s.start_ms as f64 + vt.tfdt as f64 * 1000.0 / ts;
                if wall >= to_ms as f64 {
                    break;
                }
                if !s.low {
                    high.push((wall, wall + vt.duration as f64 * 1000.0 / ts));
                }
                if let (true, Some((off, size))) = (vt.first_is_sync, vt.first_sample) {
                    keys.push(Key { wall, path: s.path.clone(), off: frag.offset + off, size, header, low: s.low });
                }
            }
        }
        if keys.iter().any(|k| k.low) {
            let high = merged(high);
            keys.retain(|k| !k.low || !inside(&high, k.wall));
        }
        keys.sort_by(|a, b| a.wall.total_cmp(&b.wall));
        // The keyframe at or before `from` starts the clip; after that one
        // keyframe per `step` of recording, while the header stays the same.
        // (Within 1 ms counts: a clip that ended at a hand-over reports its
        // end rounded, and asking from there must not pick the old keyframe.)
        let Some(first) = keys.iter().rposition(|k| k.wall <= from_ms as f64 + 1.0).or((!keys.is_empty()).then_some(0)) else {
            return Ok(None);
        };
        let header_id = keys[first].header;
        let mut picks: Vec<(PathBuf, u64, u32, f64)> = Vec::new();
        // When the next picture is due, and the last keyframe seen before
        // then (it is taken instead when it is nearer to the time due).
        let mut next = f64::MIN;
        let mut before: Option<usize> = None;
        // Where a different header takes over: the next clip starts there.
        let mut switch_at: Option<f64> = None;
        // Where the same header is next seen after each key (for skipping
        // short stretches recorded with another one, below).
        let mut next_same = vec![f64::INFINITY; keys.len()];
        let mut seen = f64::INFINITY;
        for i in (first..keys.len()).rev() {
            next_same[i] = seen;
            if keys[i].header == header_id {
                seen = keys[i].wall;
            }
        }
        for (i, k) in keys.iter().enumerate().skip(first) {
            if k.header != header_id {
                // Far apart pictures: a short stretch with another header (a
                // reconnect, the other stream filling a gap) is passed over
                // when this header carries on soon after; one clip then
                // spans it instead of ending there.
                if step >= QUICK_STEP_MS && next_same[i] - k.wall <= MAX_SKIP_OTHER_HEADER_MS {
                    continue;
                }
                switch_at = Some(k.wall);
                break;
            }
            if k.wall < next {
                // Not yet due; may still be the nearest (but never one that
                // comes less than half a step after the last picture).
                if picks.last().is_none_or(|p| k.wall - p.3 >= step * 0.5) {
                    before = Some(i);
                }
                continue;
            }
            // Due: this keyframe or the one just before, whichever is nearer.
            if let Some(b) = before.take().filter(|&b| next - keys[b].wall < k.wall - next) {
                let kb = &keys[b];
                picks.push((kb.path.clone(), kb.off, kb.size, kb.wall));
                next = kb.wall + step;
                if k.wall < next {
                    before = Some(i).filter(|_| k.wall - kb.wall >= step * 0.5);
                    continue;
                }
            }
            next = k.wall + step;
            picks.push((k.path.clone(), k.off, k.size, k.wall));
        }
        let (src_init, video) = headers.swap_remove(header_id);
        let init = video_only_init(&src_init, video.id)?;
        // Keyframes only: shown in the order they are decoded, whatever the
        // stream, so the decoder can show each one at once.
        let init = crate::ingest::sps::low_delay_init(&init).unwrap_or(init);
        if picks.is_empty() {
            return Ok(None);
        }
        let ts = video.timescale.max(1) as f64;
        let t0 = picks[0].3;
        let out_time = |wall: f64| ((wall - t0) / speed * ts / 1000.0).round() as u64;
        let mut pieces = vec![Piece::Bytes(init)];
        let mut index: Vec<(u64, u64, u64, bool)> = Vec::with_capacity(picks.len());
        for (i, (path, off, size, wall)) in picks.iter().enumerate() {
            let dts = out_time(*wall);
            // The last frame shows until the hand-over to another header, if any.
            let last_end = switch_at.map_or(dts + (frame_ms as f64 * ts / 1000.0) as u64, |w| out_time(w).max(dts + 1));
            let end = picks.get(i + 1).map(|n| out_time(n.3)).unwrap_or(last_end);
            let dur = end.saturating_sub(dts).clamp(1, u32::MAX as u64) as u32;
            let frag = trick_fragment(i as u32 + 1, video.id, dts, dur, *size);
            index.push((frag.len() as u64 + *size as u64, dts, dur as u64, true));
            pieces.push(Piece::Bytes(frag));
            pieces.push(Piece::Slice { path: path.clone(), offset: *off, len: *size as u64 });
        }
        // A segment index after the header (as in plan()): the browser knows
        // the clip's length and where each picture is without first reading
        // every fragment's header through the clip (each a separate request
        // over the network).
        if index.len() < 65_536 {
            pieces.insert(1, Piece::Bytes(sidx(video.id, video.timescale, &index, 0)));
        }
        let last = picks.last().unwrap().3;
        let total = pieces.iter().map(|p| p.len()).sum();
        // (Up to the hand-over, so the next clip starts with the new header.)
        let end_ms = switch_at.map_or(last + step, |w| w.max(last + 1.0));
        let frame_times = picks.iter().map(|p| p.3.round() as i64).collect();
        let sub = segments.iter().any(|s| s.path == keys[first].path && s.sub);
        Ok(Some(VirtualClip { pieces, total, start_ms: t0.round() as i64, end_ms: end_ms.round() as i64, frame_times, sub, frag_wall: Vec::new(), first_cto: 0 }))
    }

    pub fn len(&self) -> u64 {
        self.total
    }

    /// A clip made elsewhere (a reversed copy), served like the others.
    pub fn from_bytes(bytes: Vec<u8>, start_ms: i64, end_ms: i64, sub: bool) -> VirtualClip {
        let total = bytes.len() as u64;
        VirtualClip { pieces: vec![Piece::Bytes(bytes)], total, start_ms, end_ms, frame_times: Vec::new(), sub, frag_wall: Vec::new(), first_cto: 0 }
    }

    /// Bytes [start, end) of the clip.
    pub fn read_range(&self, start: u64, end: u64) -> std::io::Result<Vec<u8>> {
        let end = end.min(self.total);
        let mut out = Vec::with_capacity(end.saturating_sub(start) as usize);
        let mut pos = 0u64;
        for p in &self.pieces {
            let (ps, pe) = (pos, pos + p.len());
            pos = pe;
            if pe <= start || ps >= end {
                continue;
            }
            let (a, b) = (start.max(ps) - ps, end.min(pe) - ps);
            match p {
                Piece::Bytes(bytes) => out.extend_from_slice(&bytes[a as usize..b as usize]),
                Piece::Slice { path, offset, .. } => {
                    let mut f = std::fs::File::open(path)?;
                    f.seek(SeekFrom::Start(offset + a))?;
                    let mut buf = vec![0u8; (b - a) as usize];
                    f.read_exact(&mut buf)?;
                    out.extend_from_slice(&buf);
                }
                Piece::Fragment { path, offset, len: _, patches } => {
                    let mut f = std::fs::File::open(path)?;
                    f.seek(SeekFrom::Start(offset + a))?;
                    let mut buf = vec![0u8; (b - a) as usize];
                    f.read_exact(&mut buf)?;
                    apply_patches(&mut buf, a, patches);
                    out.extend_from_slice(&buf);
                }
            }
        }
        Ok(out)
    }

    /// Write the whole clip to a file (export / protect).
    pub fn write_file(&self, dest: &Path) -> std::io::Result<()> {
        use std::io::Write;
        let tmp = dest.with_extension("part");
        {
            let mut f = std::io::BufWriter::new(std::fs::File::create(&tmp)?);
            let mut pos = 0;
            while pos < self.total {
                let next = (pos + 4 * 1024 * 1024).min(self.total);
                f.write_all(&self.read_range(pos, next)?)?;
                pos = next;
            }
            f.flush()?;
        }
        std::fs::rename(&tmp, dest)
    }
}

/// One keyframe, on its own: what fast playback shows (the browser decodes
/// it by itself, no clip around it).
pub struct KeyPicture {
    /// When it was recorded (ms since the epoch).
    pub wall_ms: i64,
    /// RFC 6381 codec string, e.g. "avc1.640028".
    pub codec: String,
    /// The decoder configuration (`avcC` / `hvcC` contents).
    pub desc: Vec<u8>,
    /// The picture: length-prefixed NAL units, as stored.
    pub data: Vec<u8>,
    pub sub: bool,
    /// The picture's size as recorded (width, height), when the header says.
    pub size: Option<(u16, u16)>,
}

/// The picture size in an MP4 header's video sample entry (avc1/avc3/
/// hvc1/hev1: width and height sit 24 bytes into the entry's body).
fn picture_size(init: &[u8]) -> Option<(u16, u16)> {
    for tag in [b"avc1", b"avc3", b"hvc1", b"hev1"] {
        let Some(at) = init.windows(4).position(|w| w == tag) else { continue };
        let b = init.get(at + 4 + 24..at + 4 + 28)?;
        let (w, h) = (u16::from_be_bytes([b[0], b[1]]), u16::from_be_bytes([b[2], b[3]]));
        if w > 0 && h > 0 {
            return Some((w, h));
        }
    }
    None
}

/// The decoder configuration record in an MP4 header: (H.265, contents).
fn codec_record(init: &[u8]) -> Option<(bool, &[u8])> {
    for (tag, h265) in [(b"avcC", false), (b"hvcC", true)] {
        let Some(at) = init.windows(4).position(|w| w == tag).and_then(|i| i.checked_sub(4)) else { continue };
        let len = u32::from_be_bytes(init.get(at..at + 4)?.try_into().ok()?) as usize;
        if let Some(rec) = init.get(at + 8..at + len.max(8)).filter(|r| !r.is_empty()) {
            return Some((h265, rec));
        }
    }
    None
}

/// The picture on screen at `at_ms`: the last keyframe recorded at or
/// before it. Where a recording's layout is not known yet, its first
/// keyframe stands for all of it (one short read from the start of the
/// file instead of a seek per fragment).
pub fn key_picture(segments: &[SourceSegment], at_ms: i64) -> Result<Option<KeyPicture>> {
    let at = at_ms as f64 + 1.0;
    let mut order: Vec<&SourceSegment> = segments.iter().filter(|s| s.start_ms as f64 <= at).collect();
    // The recording that started last before `at` holds it.
    order.sort_by_key(|s| std::cmp::Reverse(s.start_ms));
    for s in order {
        let p = match parse_segment_opt(&s.path, false).ok().flatten() {
            Some(p) => p,
            None => match segment_head(&s.path) {
                Ok(p) => p,
                Err(e) => {
                    tracing::debug!("picture: skipping {}: {e:#}", s.path.display());
                    continue;
                }
            },
        };
        let Some(video) = p.tracks.iter().find(|t| t.video).cloned() else { continue };
        let ts = video.timescale.max(1) as f64;
        let mut pick: Option<(f64, u64, u32)> = None;
        for frag in &p.fragments {
            let Some(vt) = frag.trafs.iter().find(|t| t.track_id == video.id) else { continue };
            let wall = s.start_ms as f64 + vt.tfdt as f64 * 1000.0 / ts;
            if wall > at {
                break;
            }
            if let (true, Some((off, size))) = (vt.first_is_sync, vt.first_sample) {
                pick = Some((wall, frag.offset + off, size));
            }
        }
        let Some((wall, off, size)) = pick else { continue };
        // (Already checked when the layout was read; checked again here so a
        // damaged number can never make us set aside gigabytes of memory.)
        if size == 0 || size > MAX_PICTURE_BYTES {
            bail!("picture of {size} bytes in {} is not believable", s.path.display());
        }
        let Some((h265, rec)) = codec_record(&p.init) else { bail!("no decoder configuration in {}", s.path.display()) };
        let codec = crate::ingest::flv::codec_string(h265, rec).context("codec")?;
        let mut f = std::fs::File::open(&s.path)?;
        f.seek(SeekFrom::Start(off))?;
        let mut data = vec![0u8; size as usize];
        f.read_exact(&mut data)?;
        return Ok(Some(KeyPicture { wall_ms: wall.round() as i64, codec, desc: rec.to_vec(), data, sub: s.sub, size: picture_size(&p.init) }));
    }
    Ok(None)
}

/// Write `patches` (offsets within a fragment) into `buf`, which holds the
/// fragment's bytes from offset `at`.
fn apply_patches(buf: &mut [u8], at: u64, patches: &[(u32, u64, u8)]) {
    let end = at + buf.len() as u64;
    for &(pos, value, width) in patches {
        let bytes = value.to_be_bytes();
        let bytes = &bytes[8 - width as usize..];
        for (k, byte) in bytes.iter().enumerate() {
            let p = pos as u64 + k as u64;
            if p >= at && p < end {
                buf[(p - at) as usize] = *byte;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ingest::fmp4::{TrackInfo, TrackKind};
    use crate::ingest::recorder::SegmentWriter;

    fn video_track() -> TrackInfo {
        let mut entry = vec![0, 0, 0, 16];
        entry.extend_from_slice(b"avc1");
        entry.extend_from_slice(&[0; 8]);
        TrackInfo { kind: TrackKind::Video, timescale: 90000, sample_entry: entry, width: 64, height: 48 }
    }

    /// Record 10 s at 10 fps (1 s GOP) into 3 s segments; return them with
    /// the writer's wall-clock starts.
    pub(super) fn record(dir: &Path) -> Vec<SourceSegment> {
        record_track(dir, video_track())
    }

    fn record_track(dir: &Path, track: TrackInfo) -> Vec<SourceSegment> {
        let mut w = SegmentWriter::new(dir, "cam-1", vec![track], 3).unwrap();
        for i in 0..100i64 {
            w.push_video(i * 9000, vec![(i % 251) as u8; 40 + (i as usize % 7)], i % 10 == 0).unwrap();
        }
        w.finish().unwrap();
        w.take_closed().into_iter().map(|s| SourceSegment { path: s.path, start_ms: s.start_ms, end_ms: s.end_ms, low: false, sub: false }).collect()
    }

    #[test]
    fn clip_spans_segments_starts_at_keyframe_and_ranges_match() {
        let dir = std::env::temp_dir().join(format!("amber-splice-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let segs = record(&dir);
        assert!(segs.len() >= 3);
        let t0 = segs[0].start_ms;
        // 2.5 s .. 6.2 s: starts at the 2 s keyframe, crosses two segment
        // boundaries (3 s, 6 s), ends after the 6 s fragment.
        let clip = VirtualClip::plan(&segs, t0 + 2500, t0 + 6200).unwrap().unwrap();
        assert_eq!(clip.start_ms - t0, 2000);
        assert_eq!(clip.end_ms - t0, 7000);
        let all = clip.read_range(0, clip.len()).unwrap();
        assert_eq!(all.len() as u64, clip.len());
        // Any sub-range equals the same slice of the whole.
        for (a, b) in [(0u64, 10u64), (100, 900), (clip.len() - 50, clip.len()), (7, clip.len() - 3)] {
            assert_eq!(clip.read_range(a, b).unwrap(), all[a as usize..b as usize].to_vec());
        }
        // Parse the output back: one header, 5 fragments (2..7 s), sequence
        // numbers 1..5, decode times 0, 1 s, 2 s, ... contiguous.
        let out = dir.join("out.mp4");
        clip.write_file(&out).unwrap();
        let p = parse_segment(&out).unwrap();
        assert_eq!(p.fragments.len(), 5);
        let bytes = std::fs::read(&out).unwrap();
        let mut expect_dt = 0u64;
        for (k, f) in p.fragments.iter().enumerate() {
            assert_eq!(be32(&bytes, f.offset as usize + f.seq_pos).unwrap(), k as u32 + 1);
            assert_eq!(f.trafs[0].tfdt, expect_dt);
            assert!(f.trafs[0].first_is_sync);
            expect_dt += f.trafs[0].duration;
        }
        assert_eq!(expect_dt, 5 * 90000);
        // A window outside the recording yields nothing.
        assert!(VirtualClip::plan(&segs, t0 + 60_000, t0 + 70_000).unwrap().is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// An H.264 track whose header has a real SPS (High 5.1, no
    /// bitstream_restriction); frames are slices of type `slice_type(i)`,
    /// shown `cto(i)` after they are decoded.
    fn record_h264(dir: &Path, slice_type: impl Fn(u32) -> u32, cto: impl Fn(u32) -> i32) -> Vec<SourceSegment> {
        use crate::ingest::sps::tests::{h264_sample_entry, slice};
        let track = TrackInfo { kind: TrackKind::Video, timescale: 90000, sample_entry: h264_sample_entry(), width: 640, height: 360 };
        let mut w = SegmentWriter::new(dir, "cam-1", vec![track], 3).unwrap();
        for i in 0..100u32 {
            let k = i % 10;
            w.push_video_cts(i as i64 * 9000, cto(i), slice(k == 0, true, if k == 0 { 7 } else { slice_type(i) }, k, (k * 2) % 64), k == 0).unwrap();
        }
        w.finish().unwrap();
        w.take_closed().into_iter().map(|s| SourceSegment { path: s.path, start_ms: s.start_ms, end_ms: s.end_ms, low: false, sub: false }).collect()
    }

    fn served_sps(clip: &VirtualClip) -> crate::ingest::sps::Sps {
        let head = clip.read_range(0, 4096).unwrap();
        let avcc = crate::ingest::sps::init_avcc(&head).unwrap();
        let n = u16::from_be_bytes([avcc[6], avcc[7]]) as usize;
        crate::ingest::sps::parse_sps(&avcc[8..8 + n]).unwrap()
    }

    #[test]
    fn clips_of_streams_without_b_frames_get_a_low_delay_header() {
        let dir = std::env::temp_dir().join(format!("amber-splice-lowdelay-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let segs = record_h264(&dir.join("p"), |_| 5, |_| 0);
        let t0 = segs[0].start_ms;
        let clip = VirtualClip::plan(&segs, t0 + 2500, t0 + 6200).unwrap().unwrap();
        assert_eq!(served_sps(&clip).max_num_reorder, Some(0));
        // The file itself is untouched, and the clip still parses whole.
        assert_eq!(crate::ingest::sps::low_delay_init(&parse_segment(&segs[0].path).unwrap().init).is_some(), true);
        let out = dir.join("out.mp4");
        clip.write_file(&out).unwrap();
        assert_eq!(parse_segment(&out).unwrap().fragments.len(), 5);
        // Fast clips (keyframes only) too.
        let fast = VirtualClip::plan_trick(&segs, t0, t0 + 9000, 4, 100).unwrap().unwrap();
        assert_eq!(served_sps(&fast).max_num_reorder, Some(0));

        // B-frames: slices of type B, or composition offsets: the header
        // stays as recorded (except for fast clips: keyframes only).
        for (name, segs) in [("b", record_h264(&dir.join("b"), |i| if i % 3 == 2 { 6 } else { 5 }, |_| 0)), ("cto", record_h264(&dir.join("cto"), |_| 5, |i| if i % 10 == 0 { 0 } else { 3000 }))] {
            let t0 = segs[0].start_ms;
            let clip = VirtualClip::plan(&segs, t0 + 2500, t0 + 6200).unwrap().unwrap();
            assert_eq!(served_sps(&clip).max_num_reorder, None, "{name}");
            let fast = VirtualClip::plan_trick(&segs, t0, t0 + 9000, 4, 100).unwrap().unwrap();
            assert_eq!(served_sps(&fast).max_num_reorder, Some(0), "{name}");
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A camera recording the sub stream all the time (0..10 s) and the
    /// main stream (another codec header) from 3 s to 6 s.
    fn record_mixed(dir: &Path) -> (Vec<SourceSegment>, i64) {
        let mut segs = record(dir);
        for s in &mut segs {
            s.low = true;
        }
        let t0 = segs[0].start_ms;
        let mut main = video_track();
        main.width = 1280;
        main.height = 720;
        main.sample_entry[8] = 1; // a different header
        let mut w = SegmentWriter::new(dir, "cam-main", vec![main], 60).unwrap();
        for i in 0..30i64 {
            w.push_video(i * 9000, vec![200; 90], i % 10 == 0).unwrap();
        }
        w.finish().unwrap();
        for s in w.take_closed() {
            segs.push(SourceSegment { path: s.path, start_ms: t0 + 3000, end_ms: t0 + 3000 + (s.end_ms - s.start_ms), low: false, sub: false });
        }
        segs.sort_by_key(|s| s.start_ms);
        (segs, t0)
    }

    #[test]
    fn fast_clips_pass_over_a_short_stretch_with_another_header() {
        let dir = std::env::temp_dir().join(format!("amber-skiphdr-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let (segs, t0) = record_mixed(&dir);
        // 30x (a picture per 3 s): the 3-6 s stretch with the other header
        // does not end the clip; it carries on to the end.
        let c = VirtualClip::plan_trick(&segs, t0, t0 + 10_000, 30, 100).unwrap().unwrap();
        assert_eq!(c.frame_times.iter().map(|t| t - t0).collect::<Vec<_>>(), vec![0, 6000, 9000]);
        assert!(c.end_ms - t0 >= 9000);
        // Close pictures (2x) still hand over at the change, as before.
        let c = VirtualClip::plan_trick(&segs, t0, t0 + 10_000, 2, 100).unwrap().unwrap();
        assert_eq!(c.end_ms - t0, 3000);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn main_stream_is_preferred_and_clips_hand_over_between_streams() {
        let dir = std::env::temp_dir().join(format!("amber-mixed-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let (segs, t0) = record_mixed(&dir);
        // From 0: the sub stream up to where the main stream starts.
        let a = VirtualClip::plan(&segs, t0, t0 + 10_000).unwrap().unwrap();
        assert_eq!((a.start_ms - t0, a.end_ms - t0), (0, 3000));
        // From its end: the main stream, 3..6 s, never the sub stream's copy.
        let b = VirtualClip::plan(&segs, a.end_ms, t0 + 10_000).unwrap().unwrap();
        assert_eq!((b.start_ms - t0, b.end_ms - t0), (3000, 6000));
        let out = dir.join("b.mp4");
        b.write_file(&out).unwrap();
        assert_eq!(parse_segment(&out).unwrap().fragments.len(), 3);
        // Inside the main recording, playback starts at its keyframe.
        let mid = VirtualClip::plan(&segs, t0 + 4500, t0 + 10_000).unwrap().unwrap();
        assert_eq!((mid.start_ms - t0, mid.end_ms - t0), (4000, 6000));
        // Then the sub stream again to the end.
        let c = VirtualClip::plan(&segs, b.end_ms, t0 + 10_000).unwrap().unwrap();
        assert_eq!((c.start_ms - t0, c.end_ms - t0), (6000, 10_000));
        // Fast playback hands over the same way.
        let t = VirtualClip::plan_trick(&segs, t0, t0 + 10_000, 2, 100).unwrap().unwrap();
        assert_eq!((t.start_ms - t0, t.end_ms - t0), (0, 3000));
        let t2 = VirtualClip::plan_trick(&segs, t.end_ms, t0 + 10_000, 2, 100).unwrap().unwrap();
        assert_eq!((t2.start_ms - t0, t2.end_ms - t0), (3000, 6000));
        let t3 = VirtualClip::plan_trick(&segs, t2.end_ms, t0 + 10_000, 2, 100).unwrap().unwrap();
        assert_eq!(t3.start_ms - t0, 6000);
        // Without the main stream the sub stream plays straight through.
        let subs: Vec<SourceSegment> = segs.iter().filter(|s| s.low).cloned().collect();
        let all = VirtualClip::plan(&subs, t0, t0 + 10_000).unwrap().unwrap();
        assert_eq!((all.start_ms - t0, all.end_ms - t0), (0, 10_000));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn far_apart_pictures_use_each_recordings_first_keyframe_and_skip_unneeded_ones() {
        let dir = std::env::temp_dir().join(format!("amber-quick-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        // Recordings of 3 s (keyframes every 1 s) never opened before: not in memory.
        let segs = record(&dir);
        let t0 = segs[0].start_ms;
        let rel = |c: &VirtualClip| c.frame_times.iter().map(|t| t - t0).collect::<Vec<_>>();
        // 30x, 100 ms pictures = one per 3 s: each recording's first keyframe.
        let c = VirtualClip::plan_trick(&segs, t0, t0 + 10_000, 30, 100).unwrap().unwrap();
        assert_eq!(rel(&c), vec![0, 3000, 6000, 9000]);
        // 60x = one per 6 s: every other recording is not even opened.
        let c = VirtualClip::plan_trick(&segs, t0, t0 + 10_000, 60, 100).unwrap().unwrap();
        assert_eq!(rel(&c), vec![0, 6000]);
        let out = dir.join("quick.mp4");
        c.write_file(&out).unwrap();
        let p = parse_segment_file(&out).unwrap();
        assert_eq!(p.fragments.len(), 2);
        assert!(p.fragments.iter().all(|f| f.trafs[0].first_is_sync));
        // Once a recording's full layout is known, its inner keyframes count.
        for s in &segs {
            parse_segment(&s.path).unwrap();
        }
        let c = VirtualClip::plan_trick(&segs, t0, t0 + 10_000, 20, 100).unwrap().unwrap();
        assert_eq!(rel(&c), vec![0, 2000, 4000, 6000, 8000]);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn single_pictures_are_the_last_keyframe_at_or_before_the_moment() {
        let dir = std::env::temp_dir().join(format!("amber-picture-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        // A real decoder configuration (H.264 High, level 4.0).
        let avcc = [1u8, 0x64, 0, 0x28, 0xff, 0xe0, 0];
        let entry = crate::ingest::flv::video_sample_entry(false, &avcc, 64, 48);
        let segs = record_track(&dir, TrackInfo { sample_entry: entry, ..video_track() });
        let t0 = segs[0].start_ms;
        let at = |ms: i64| key_picture(&segs, t0 + ms).unwrap().map(|k| k.wall_ms - t0);
        // Layouts not known yet: each recording's first keyframe stands for it.
        assert_eq!(at(4500), Some(3000));
        assert_eq!(at(-10), None);
        for s in &segs {
            parse_segment(&s.path).unwrap();
        }
        assert_eq!(at(4500), Some(4000));
        assert_eq!(at(3000), Some(3000));
        let k = key_picture(&segs, t0 + 4500).unwrap().unwrap();
        assert_eq!(k.codec, "avc1.640028");
        assert_eq!(k.size, Some((64, 48)));
        assert_eq!(k.desc, avcc);
        assert!(!k.data.is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn trick_clip_keeps_one_keyframe_per_step_and_retimes() {
        let dir = std::env::temp_dir().join(format!("amber-trick-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let segs = record(&dir);
        let t0 = segs[0].start_ms;
        // 2x with 1 s output frames: one keyframe per 2 s of recording, from the
        // keyframe at or before 1.5 s (1 s) up to 9.5 s: 1, 3, 5, 7, 9 s.
        let clip = VirtualClip::plan_trick(&segs, t0 + 1500, t0 + 9500, 2, 1000).unwrap().unwrap();
        assert_eq!(clip.start_ms - t0, 1000);
        let out = dir.join("trick.mp4");
        clip.write_file(&out).unwrap();
        let p = parse_segment(&out).unwrap();
        assert_eq!(p.tracks.len(), 1);
        assert!(p.tracks[0].video);
        assert_eq!(p.fragments.len(), 5);
        for (k, f) in p.fragments.iter().enumerate() {
            let t = &f.trafs[0];
            assert!(t.first_is_sync);
            assert_eq!(t.tfdt, k as u64 * 90000, "2 s of recording per 1 s of clip");
            assert_eq!(t.duration, 90000);
            // The sample is the source keyframe's bytes (40 + i%7 bytes of value i%251).
            let (off, size) = t.first_sample.unwrap();
            let i = 10 + 20 * k;
            assert_eq!(size as usize, 40 + i % 7);
            let bytes = std::fs::read(&out).unwrap();
            let at = (f.offset + off) as usize;
            assert!(bytes[at..at + size as usize].iter().all(|&b| b == (i % 251) as u8));
        }
        // A segment index right before the first fragment: one entry per
        // picture, sizes reaching exactly to the end of the file, 2 s each.
        let bytes = std::fs::read(&out).unwrap();
        let first = p.fragments[0].offset as usize;
        let sx = bytes[..first].windows(4).rposition(|w| w == b"sidx").unwrap() - 4;
        let be32 = |at: usize| u32::from_be_bytes(bytes[at..at + 4].try_into().unwrap());
        assert_eq!(sx + be32(sx) as usize, first, "sidx directly before the fragments");
        let n = u16::from_be_bytes(bytes[sx + 38..sx + 40].try_into().unwrap()) as usize;
        assert_eq!(n, 5);
        let sizes: usize = (0..n).map(|k| (be32(sx + 40 + 12 * k) & 0x7fff_ffff) as usize).sum();
        assert_eq!(first + sizes, bytes.len());
        assert!((0..n).all(|k| be32(sx + 44 + 12 * k) == 90000));
        let _ = std::fs::remove_dir_all(&dir);
    }
    #[test]
    fn damaged_picture_sizes_are_ignored_not_read() {
        // Sizes that do not fit inside their fragment are forgotten.
        let traf = |off: u64, size: u32| TrafInfo { track_id: 1, tfdt_pos: 0, tfdt: 0, duration: 1, first_is_sync: true, first_sample: Some((off, size)), first_cto: 0 };
        let f = FragmentInfo { offset: 0, len: 1000, seq_pos: 0, trafs: vec![traf(100, 900), traf(100, 901), traf(u64::MAX, 10), traf(0, 0), traf(8, u32::MAX)] }.checked();
        let kept: Vec<_> = f.trafs.iter().map(|t| t.first_sample).collect();
        assert_eq!(kept, vec![Some((100, 900)), None, None, None, None]);

        // A recording whose first picture claims to be ~4 GB: that keyframe
        // is skipped (no huge memory), the others still work.
        let dir = std::env::temp_dir().join(format!("amber-badsize-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let avcc = [1u8, 0x64, 0, 0x28, 0xff, 0xe0, 0];
        let entry = crate::ingest::flv::video_sample_entry(false, &avcc, 64, 48);
        let segs = record_track(&dir, TrackInfo { sample_entry: entry, ..video_track() });
        let t0 = segs[0].start_ms;
        let path = &segs[1].path;
        let mut bytes = std::fs::read(path).unwrap();
        let first = parse_segment_file(path).unwrap().fragments[0].clone();
        let moof = first.offset as usize;
        let (ts, te) = find(&bytes, moof + 8, moof + be32(&bytes, moof).unwrap() as usize, b"traf").unwrap().unwrap();
        let (rs, _) = find(&bytes, ts + 8, te, b"trun").unwrap().unwrap();
        let flags = be32(&bytes, rs + 8).unwrap();
        assert!(flags & 0x200 != 0, "the recorder writes each sample's size");
        let mut p = rs + 16;
        for bit in [0x1, 0x4, 0x100] {
            if flags & bit != 0 {
                p += 4;
            }
        }
        bytes[p..p + 4].copy_from_slice(&0xFFFF_FFF0u32.to_be_bytes());
        std::fs::write(path, &bytes).unwrap();
        for s in &segs {
            parse_segment(&s.path).unwrap();
        }
        let p = parse_segment(path).unwrap();
        assert_eq!(p.fragments[0].trafs[0].first_sample, None);
        assert!(p.fragments[1].trafs[0].first_sample.is_some());
        // The moment just after the damaged keyframe (3 s): the one before it.
        let k = key_picture(&segs, t0 + 3500).unwrap().unwrap();
        assert_eq!(k.wall_ms - t0, 2000);
        assert_eq!(key_picture(&segs, t0 + 4500).unwrap().unwrap().wall_ms - t0, 4000);
        // Fast playback passes over it as well.
        let c = VirtualClip::plan_trick(&segs, t0, t0 + 10_000, 10, 100).unwrap().unwrap();
        assert!(!c.frame_times.contains(&(t0 + 3000)));
        assert!(c.read_range(0, c.len()).is_ok());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_window_planned_again_keeps_its_bytes_while_recording_goes_on() {
        let dir = std::env::temp_dir().join(format!("amber-replan-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let segs = record(&dir);
        let t0 = segs[0].start_ms;
        let (from, to) = (t0 + 500, t0 + 60_000);
        // While only the first two recordings (0..6 s) exist.
        let first = VirtualClip::plan(&segs[..2], from, to).unwrap().unwrap();
        assert_eq!(first.end_ms - t0, 6000);
        let before = first.read_range(0, first.len()).unwrap();
        // Later more was recorded: planned plainly, the clip (and its table
        // of contents at the front) grows, so old byte positions move.
        let grown = VirtualClip::plan(&segs, from, to).unwrap().unwrap();
        assert!(grown.end_ms > first.end_ms);
        assert_ne!(grown.read_range(0, before.len() as u64).unwrap(), before);
        // Planned like before: the very same bytes, ending where it did.
        let again = VirtualClip::plan_like(&segs, from, to, Some(first.end_ms)).unwrap().unwrap();
        assert_eq!((again.start_ms, again.end_ms, again.len()), (first.start_ms, first.end_ms, first.len()));
        assert_eq!(again.read_range(0, again.len()).unwrap(), before);
        // The rest comes from the old end, as usual.
        let rest = VirtualClip::plan(&segs, first.end_ms, to).unwrap().unwrap();
        assert_eq!((rest.start_ms - t0, rest.end_ms - t0), (6000, 10_000));
        // Nothing remembered (or a remembered end outside the window): plain.
        assert_eq!(VirtualClip::plan_like(&segs, from, to, None).unwrap().unwrap().len(), grown.len());
        assert_eq!(VirtualClip::plan_like(&segs, from, to, Some(t0)).unwrap().unwrap().len(), grown.len());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn far_apart_pictures_stay_near_the_spacing_asked_for() {
        let dir = std::env::temp_dir().join(format!("amber-spacing-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        // Recordings of 3 s, layouts not known: one keyframe per 3 s at hand.
        let segs = record(&dir);
        let t0 = segs[0].start_ms;
        let rel = |c: &VirtualClip| c.frame_times.iter().map(|t| t - t0).collect::<Vec<_>>();
        // A picture per 3.3 s asked for: every recording's (3 s apart), not
        // every other one's (6 s apart, nearly twice what was asked). (The
        // 9 s one is not due before the clip's end, 9.3 s: the next clip
        // starts with it.)
        let c = VirtualClip::plan_trick(&segs, t0, t0 + 10_000, 33, 100).unwrap().unwrap();
        assert_eq!(rel(&c), vec![0, 3000, 6000]);
        assert_eq!(c.end_ms - t0, 9300);
        // A picture per 5 s: 6 s apart is the nearest there is.
        let c = VirtualClip::plan_trick(&segs, t0, t0 + 10_000, 50, 100).unwrap().unwrap();
        assert_eq!(rel(&c), vec![0, 6000]);
        // Layouts known (a keyframe every second), a picture per 2.6 s: about
        // that far apart, never twice it.
        for s in &segs {
            parse_segment(&s.path).unwrap();
        }
        let c = VirtualClip::plan_trick(&segs, t0, t0 + 10_000, 26, 100).unwrap().unwrap();
        let r = rel(&c);
        assert!(r.windows(2).all(|w| (2000..=3000).contains(&(w[1] - w[0]))), "{r:?}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn far_apart_pictures_never_use_the_sub_stream_where_the_main_one_recorded() {
        let dir = std::env::temp_dir().join(format!("amber-quickcover-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        // Main stream: 3 s recordings over 0..10 s.
        let mut segs = record(&dir);
        let t0 = segs[0].start_ms;
        // Sub stream (another header): 5 s recordings, also 0..10 s.
        let mut small = video_track();
        small.sample_entry[8] = 1;
        let mut w = SegmentWriter::new(&dir, "cam-sub", vec![small], 5).unwrap();
        for i in 0..100i64 {
            w.push_video(i * 9000, vec![7; 30], i % 10 == 0).unwrap();
        }
        w.finish().unwrap();
        let subs = w.take_closed();
        let s0 = subs[0].start_ms;
        for s in subs {
            let (a, b) = (t0 + s.start_ms - s0, t0 + s.end_ms - s0);
            segs.push(SourceSegment { path: s.path, start_ms: a, end_ms: b, low: true, sub: true });
        }
        // A picture per 6 s from 5.5 s: the main recording of 3..6 s is not
        // opened, but it was recorded, so the sub stream's 5 s keyframe is
        // not used; the main stream's picture before it is.
        let c = VirtualClip::plan_trick(&segs, t0 + 5500, t0 + 10_000, 60, 100).unwrap().unwrap();
        assert!(!c.sub);
        assert_eq!(c.frame_times[0] - t0, 0);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
