//! Downloads of a stretch of recording (the timeline's export):
//!
//! * Exact start and end: a clip has to begin at a keyframe, often seconds
//!   before the moment asked for. An edit list in each track tells players to
//!   start showing at the asked moment and to stop at the asked end, so the
//!   file plays exactly that stretch while keeping the frames it needs.
//! * Time captions: a subtitle track (3GPP timed text, which QuickTime, VLC,
//!   mpv, ffmpeg and most editors read) with the camera and the date and time
//!   of every second, interleaved with the video so it stays in step.
//!
//! The clip index (sidx) is dropped: it is for streaming to a browser, and its
//! byte counts would no longer match.

use super::{be32, children, find, parse_moov, Piece, VirtualClip};
use anyhow::{bail, Context, Result};

fn bx(kind: &[u8; 4], body: &[u8]) -> Vec<u8> {
    let mut b = Vec::with_capacity(8 + body.len());
    b.extend_from_slice(&((8 + body.len()) as u32).to_be_bytes());
    b.extend_from_slice(kind);
    b.extend_from_slice(body);
    b
}

fn full(kind: &[u8; 4], version: u8, flags: u32, body: &[u8]) -> Vec<u8> {
    let mut v = Vec::with_capacity(4 + body.len());
    v.push(version);
    v.extend_from_slice(&flags.to_be_bytes()[1..]);
    v.extend_from_slice(body);
    bx(kind, &v)
}

/// An edit list: show `duration` (movie timescale) from `media_time` (track timescale).
fn edts(duration: u64, media_time: i64) -> Vec<u8> {
    let mut e = Vec::new();
    e.extend_from_slice(&1u32.to_be_bytes());
    e.extend_from_slice(&duration.to_be_bytes());
    e.extend_from_slice(&media_time.to_be_bytes());
    e.extend_from_slice(&1i16.to_be_bytes());
    e.extend_from_slice(&0i16.to_be_bytes());
    bx(b"edts", &full(b"elst", 1, 0, &e))
}

/// The caption track: 3GPP timed text, timescale 1000.
fn text_trak(id: u32, width: u32, height: u32, edit: Option<Vec<u8>>) -> Vec<u8> {
    // tkhd v0: enabled, in the movie.
    let mut t = Vec::new();
    t.extend_from_slice(&[0; 8]); // created, modified
    t.extend_from_slice(&id.to_be_bytes());
    t.extend_from_slice(&[0; 4]);
    t.extend_from_slice(&[0; 4]); // duration (fragments carry it)
    t.extend_from_slice(&[0; 8]);
    t.extend_from_slice(&[0; 4]); // layer, alternate group
    t.extend_from_slice(&[0; 4]); // volume, reserved
    for m in [0x0001_0000u32, 0, 0, 0, 0x0001_0000, 0, 0, 0, 0x4000_0000] {
        t.extend_from_slice(&m.to_be_bytes());
    }
    t.extend_from_slice(&(width << 16).to_be_bytes());
    t.extend_from_slice(&(height << 16).to_be_bytes());
    let tkhd = full(b"tkhd", 0, 3, &t);

    let mut m = Vec::new();
    m.extend_from_slice(&[0; 8]);
    m.extend_from_slice(&1000u32.to_be_bytes());
    m.extend_from_slice(&0u32.to_be_bytes());
    m.extend_from_slice(&0x55C4u16.to_be_bytes()); // "und"
    m.extend_from_slice(&0u16.to_be_bytes());
    let mdhd = full(b"mdhd", 0, 0, &m);
    let mut h = Vec::new();
    h.extend_from_slice(&[0; 4]);
    h.extend_from_slice(b"sbtl");
    h.extend_from_slice(&[0; 12]);
    h.extend_from_slice(b"Time\0");
    let hdlr = full(b"hdlr", 0, 0, &h);

    // tx3g sample entry: centered at the bottom, white on translucent black.
    let font = (height / 24).clamp(12, 255) as u8;
    let mut e = Vec::new();
    e.extend_from_slice(&[0; 6]);
    e.extend_from_slice(&1u16.to_be_bytes()); // data reference
    e.extend_from_slice(&0u32.to_be_bytes()); // display flags
    e.push(1); // horizontal: center
    e.push(0xFF); // vertical: bottom (-1)
    e.extend_from_slice(&[0, 0, 0, 0x99]); // background
    e.extend_from_slice(&0i16.to_be_bytes()); // box: top, left, bottom, right
    e.extend_from_slice(&0i16.to_be_bytes());
    e.extend_from_slice(&(height.min(i16::MAX as u32) as i16).to_be_bytes());
    e.extend_from_slice(&(width.min(i16::MAX as u32) as i16).to_be_bytes());
    e.extend_from_slice(&[0, 0, 0, 0, 0, 1, 0, font, 0xFF, 0xFF, 0xFF, 0xFF]); // style: chars 0-0, font 1, size, white
    let name = b"Sans-Serif";
    let mut f = Vec::new();
    f.extend_from_slice(&1u16.to_be_bytes());
    f.extend_from_slice(&1u16.to_be_bytes());
    f.push(name.len() as u8);
    f.extend_from_slice(name);
    e.extend_from_slice(&bx(b"ftab", &f));
    let mut sd = 1u32.to_be_bytes().to_vec();
    sd.extend_from_slice(&bx(b"tx3g", &e));
    let empty = 0u32.to_be_bytes();
    let stbl = bx(
        b"stbl",
        &[
            full(b"stsd", 0, 0, &sd),
            full(b"stts", 0, 0, &empty),
            full(b"stsc", 0, 0, &empty),
            full(b"stsz", 0, 0, &[0; 8]),
            full(b"stco", 0, 0, &empty),
        ]
        .concat(),
    );
    let mut dref = 1u32.to_be_bytes().to_vec();
    dref.extend_from_slice(&full(b"url ", 0, 1, &[]));
    let minf = bx(b"minf", &[full(b"nmhd", 0, 0, &[]), bx(b"dinf", &full(b"dref", 0, 0, &dref)), stbl].concat());
    let mdia = bx(b"mdia", &[mdhd, hdlr, minf].concat());
    bx(b"trak", &[tkhd, edit.unwrap_or_default(), mdia].concat())
}

/// One fragment of the caption track: `samples` (duration ms, text) from
/// `base` ms after the clip's start.
fn text_fragment(seq: u32, track: u32, base: u64, samples: &[(u32, String)]) -> Vec<u8> {
    let mfhd = full(b"mfhd", 0, 0, &seq.to_be_bytes());
    let tfhd = full(b"tfhd", 0, 0x02_0000, &track.to_be_bytes()); // default-base-is-moof
    let tfdt = full(b"tfdt", 1, 0, &base.to_be_bytes());
    let mut data = Vec::new();
    let mut r = Vec::new();
    r.extend_from_slice(&(samples.len() as u32).to_be_bytes());
    r.extend_from_slice(&0u32.to_be_bytes()); // data offset, below
    for (dur, text) in samples {
        let t = text.as_bytes();
        let len = t.len().min(u16::MAX as usize);
        r.extend_from_slice(&dur.to_be_bytes());
        r.extend_from_slice(&((len + 2) as u32).to_be_bytes());
        data.extend_from_slice(&(len as u16).to_be_bytes());
        data.extend_from_slice(&t[..len]);
    }
    let trun = full(b"trun", 0, 0x000001 | 0x000100 | 0x000200, &r);
    let trun_at = 8 + mfhd.len() + 8 + tfhd.len() + tfdt.len();
    let mut moof = bx(b"moof", &[mfhd, bx(b"traf", &[tfhd, tfdt, trun].concat())].concat());
    let offset = (moof.len() + 8) as u32;
    moof[trun_at + 16..trun_at + 20].copy_from_slice(&offset.to_be_bytes());
    [moof, bx(b"mdat", &data)].concat()
}

/// Caption samples for wall-clock [from, to): one per second of the clock.
fn captions(from: i64, to: i64, label: &str, clock: &dyn Fn(i64) -> String) -> Vec<(u32, String)> {
    let mut out = Vec::new();
    let mut t = from;
    while t < to {
        let next = ((t.div_euclid(1000) + 1) * 1000).min(to);
        out.push(((next - t) as u32, format!("{label}  {}", clock(t))));
        t = next;
    }
    out
}

impl VirtualClip {
    /// This clip as a file to download: `cut` (ms since the epoch) starts
    /// and ends playback exactly there; `label` and `clock` (wall time to
    /// text) make the time captions. Clips not made by `plan` come back as
    /// they are.
    pub fn for_download(self, cut: Option<(i64, i64)>, label: &str, clock: &dyn Fn(i64) -> String) -> Result<VirtualClip> {
        let Some(Piece::Bytes(init)) = self.pieces.first() else { return Ok(self) };
        if self.frag_wall.is_empty() {
            return Ok(self);
        }
        let (ms, me) = find(init, 0, init.len(), b"moov")?.context("clip without moov")?;
        let moov = &init[ms..me];
        let tracks = parse_moov(moov)?;
        if !tracks.iter().any(|t| t.video) {
            bail!("clip without video");
        }
        let (hs, _) = find(moov, 8, moov.len(), b"mvhd")?.context("moov without mvhd")?;
        let movie_ts = be32(moov, hs + 12 + if moov[hs + 8] == 1 { 16 } else { 8 })? as u64;
        let text_id = tracks.iter().map(|t| t.id).max().unwrap_or(1) + 1;
        let cut = cut.map(|(a, b)| (a.max(self.start_ms), b.max(a + 1)));
        let edit = |ts: u32, extra: i64| {
            cut.map(|(a, b)| {
                let dur = ((b - a) as u64 * movie_ts) / 1000;
                let media = ((a - self.start_ms) * ts as i64) / 1000 + extra;
                edts(dur, media)
            })
        };
        let (mut width, mut height) = (0u32, 0u32);
        let mut body = Vec::new();
        let mut text_done = false;
        for (kind, s, e) in children(moov, 8, moov.len())? {
            match &kind {
                b"mvhd" => {
                    let mut m = moov[s..e].to_vec();
                    let n = m.len();
                    m[n - 4..].copy_from_slice(&(text_id + 1).to_be_bytes());
                    body.extend_from_slice(&m);
                }
                b"trak" => {
                    let (ts, te) = find(moov, s + 8, e, b"tkhd")?.context("trak without tkhd")?;
                    let id = be32(moov, ts + 12 + if moov[ts + 8] == 1 { 16 } else { 8 })?;
                    let meta = tracks.iter().find(|t| t.id == id);
                    if meta.is_some_and(|m| m.video) {
                        width = be32(moov, te - 8)? >> 16;
                        height = be32(moov, te - 4)? >> 16;
                    }
                    let mut t = Vec::new();
                    for (k2, s2, e2) in children(moov, s + 8, e)? {
                        if &k2 == b"edts" {
                            continue;
                        }
                        t.extend_from_slice(&moov[s2..e2]);
                        if &k2 == b"tkhd" {
                            if let Some(m) = meta {
                                // Video starts at its first sample's presentation time.
                                let extra = if m.video { self.first_cto as i64 } else { 0 };
                                t.extend_from_slice(&edit(m.timescale, extra).unwrap_or_default());
                            }
                        }
                    }
                    body.extend_from_slice(&bx(b"trak", &t));
                }
                b"mvex" => {
                    body.extend_from_slice(&text_trak(text_id, width, height, edit(1000, 0)));
                    text_done = true;
                    let mut x = moov[s + 8..e].to_vec();
                    let mut trex = Vec::new();
                    trex.extend_from_slice(&text_id.to_be_bytes());
                    trex.extend_from_slice(&1u32.to_be_bytes());
                    trex.extend_from_slice(&[0; 12]);
                    x.extend_from_slice(&full(b"trex", 0, 0, &trex));
                    body.extend_from_slice(&bx(b"mvex", &x));
                }
                _ => body.extend_from_slice(&moov[s..e]),
            }
        }
        if !text_done {
            bail!("clip without mvex (not fragmented)");
        }
        let new_init = [&init[..ms], &bx(b"moov", &body)[..], &init[me..]].concat();

        // Fragments, renumbered, each followed by its captions.
        let mut pieces = vec![Piece::Bytes(new_init)];
        let mut seq = 0u32;
        let mut frag = 0usize;
        for p in self.pieces.into_iter().skip(1) {
            match p {
                Piece::Fragment { path, offset, len, mut patches } => {
                    seq += 1;
                    if let Some(first) = patches.first_mut() {
                        first.1 = seq as u64; // (the sequence number is the first patch)
                    }
                    pieces.push(Piece::Fragment { path, offset, len, patches });
                    if let Some(&(a, b)) = self.frag_wall.get(frag) {
                        let caps = captions(a, b, label, clock);
                        if !caps.is_empty() {
                            seq += 1;
                            pieces.push(Piece::Bytes(text_fragment(seq, text_id, (a - self.start_ms).max(0) as u64, &caps)));
                        }
                    }
                    frag += 1;
                }
                // (The index: for streaming only, and it would no longer match.)
                Piece::Bytes(b) if b.get(4..8) == Some(b"sidx") => {}
                other => pieces.push(other),
            }
        }
        let total = pieces.iter().map(|p| p.len()).sum();
        let (start_ms, end_ms) = cut.unwrap_or((self.start_ms, self.end_ms));
        Ok(VirtualClip { pieces, total, start_ms, end_ms, frame_times: Vec::new(), sub: self.sub, frag_wall: Vec::new(), first_cto: 0 })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn download_has_exact_cut_and_a_caption_track() {
        let dir = std::env::temp_dir().join(format!("amber-export-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let segs = super::super::tests::record(&dir);
        let t0 = segs[0].start_ms;
        let clip = VirtualClip::plan(&segs, t0 + 2500, t0 + 6200).unwrap().unwrap();
        let frags = clip.frag_wall.len();
        let out = clip.for_download(Some((t0 + 2500, t0 + 6200)), "Porch", &|t| (t / 1000).to_string()).unwrap();
        assert_eq!((out.start_ms, out.end_ms), (t0 + 2500, t0 + 6200));
        let path = dir.join("export.mp4");
        out.write_file(&path).unwrap();
        let p = super::super::parse_segment_file(&path).unwrap();
        assert_eq!(p.tracks.len(), 2, "video and captions");
        assert_eq!(p.fragments.len(), 2 * frags, "each video fragment followed by its captions");
        // Sequence numbers run 1, 2, 3 ... across both.
        let bytes = std::fs::read(&path).unwrap();
        for (k, f) in p.fragments.iter().enumerate() {
            assert_eq!(be32(&bytes, f.offset as usize + f.seq_pos).unwrap(), k as u32 + 1);
        }
        // The video track starts 500 ms into its first (2 s keyframe) fragment.
        let (ms, me) = find(&bytes, 0, bytes.len(), b"moov").unwrap().unwrap();
        let elst = bytes[ms..me].windows(4).position(|w| w == b"elst").unwrap() + ms;
        let media_time = i64::from_be_bytes(bytes[elst + 4 + 4 + 4 + 8..elst + 4 + 4 + 4 + 16].try_into().unwrap());
        assert_eq!(media_time, 45_000, "0.5 s at 90 kHz");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn captions_split_on_clock_seconds() {
        let c = captions(1_500, 3_200, "Porch", &|t| format!("{}", t / 1000));
        assert_eq!(c, vec![(500, "Porch  1".to_string()), (1000, "Porch  2".to_string()), (200, "Porch  3".to_string())]);
    }

    #[test]
    fn caption_fragment_points_at_its_data() {
        let f = text_fragment(3, 9, 1234, &[(1000, "a".into()), (500, "bc".into())]);
        let moof_len = u32::from_be_bytes(f[0..4].try_into().unwrap()) as usize;
        assert_eq!(&f[moof_len + 4..moof_len + 8], b"mdat");
        // The data offset leads to the first sample: length 1, "a".
        let (ts, _) = find(&f, 8, moof_len, b"traf").unwrap().unwrap();
        let (rs, _) = find(&f, ts + 8, moof_len, b"trun").unwrap().unwrap();
        let off = be32(&f, rs + 16).unwrap() as usize;
        assert_eq!(&f[off..off + 3], &[0, 1, b'a']);
    }
}
