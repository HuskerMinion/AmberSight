//! Fragmented MP4 (ISO BMFF) writer for direct-to-disk recording.
//!
//! A segment file is: `ftyp` + `moov` (track descriptions, no samples) followed
//! by one `moof`+`mdat` pair per fragment. Each fragment starts on a video
//! keyframe and carries that GOP's video frames plus the audio that falls in
//! the same time span. Because every fragment is self-contained, a file that
//! is cut off by a crash or power loss still plays up to its last complete
//! fragment. The same layout is what browsers need for low-latency playback.
//!
//! Codec sample entries (`avc1`/`hvc1`/`mp4a`) come from retina; frames must
//! be in `retina::codec::FrameFormat::MP4` (length-prefixed NALs, raw AAC).

/// Description of one track for the init segment.
#[derive(Clone, Debug)]
pub struct TrackInfo {
    pub kind: TrackKind,
    /// Timescale in units per second (RTP clock rate: 90000 for video,
    /// the sample rate for AAC).
    pub timescale: u32,
    /// Full sample entry box (e.g. `avc1`, `hvc1`, `mp4a`) from retina.
    pub sample_entry: Vec<u8>,
    /// Display size for video tracks.
    pub width: u32,
    pub height: u32,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TrackKind {
    Video,
    Audio,
}

/// One media sample ready to go into a fragment.
#[derive(Clone, Debug)]
pub struct Sample {
    /// Duration in the track's timescale: decode time to the next sample.
    pub duration: u32,
    pub data: Vec<u8>,
    pub is_sync: bool,
    /// Presentation time minus decode time, in the track's timescale
    /// (non-zero only for streams with B-frames).
    pub composition_offset: i32,
}

impl Sample {
    /// A sample shown at its decode time (audio, video without B-frames).
    pub fn new(duration: u32, data: Vec<u8>, is_sync: bool) -> Self {
        Sample { duration, data, is_sync, composition_offset: 0 }
    }
}

/// Samples for one track within a fragment.
#[derive(Clone, Debug)]
pub struct TrackRun {
    /// Index into the init segment's track list (track_id = index + 1).
    pub track_index: usize,
    /// Decode time of the first sample, in the track's timescale, relative
    /// to the start of this file.
    pub base_decode_time: u64,
    pub samples: Vec<Sample>,
}

fn put_u16(b: &mut Vec<u8>, v: u16) {
    b.extend_from_slice(&v.to_be_bytes());
}
fn put_u32(b: &mut Vec<u8>, v: u32) {
    b.extend_from_slice(&v.to_be_bytes());
}
fn put_u64(b: &mut Vec<u8>, v: u64) {
    b.extend_from_slice(&v.to_be_bytes());
}

/// Append a box: 32-bit size, 4-char type, then whatever `body` writes.
fn boxed(b: &mut Vec<u8>, kind: &[u8; 4], body: impl FnOnce(&mut Vec<u8>)) {
    let start = b.len();
    put_u32(b, 0);
    b.extend_from_slice(kind);
    body(b);
    let size = (b.len() - start) as u32;
    b[start..start + 4].copy_from_slice(&size.to_be_bytes());
}

/// Full box: box header plus version (8 bits) and flags (24 bits).
fn full(b: &mut Vec<u8>, kind: &[u8; 4], version: u8, flags: u32, body: impl FnOnce(&mut Vec<u8>)) {
    boxed(b, kind, |b| {
        put_u32(b, ((version as u32) << 24) | (flags & 0x00ff_ffff));
        body(b);
    });
}

const MATRIX: [u32; 9] = [0x0001_0000, 0, 0, 0, 0x0001_0000, 0, 0, 0, 0x4000_0000];

/// Build `ftyp` + `moov` for the given tracks.
pub fn init_segment(tracks: &[TrackInfo]) -> Vec<u8> {
    let mut b = Vec::with_capacity(1024);
    boxed(&mut b, b"ftyp", |b| {
        b.extend_from_slice(b"iso6"); // major brand
        put_u32(b, 0);
        for brand in [b"iso6", b"mp41", b"isom", b"dash"] {
            b.extend_from_slice(brand);
        }
    });
    boxed(&mut b, b"moov", |b| {
        full(b, b"mvhd", 0, 0, |b| {
            put_u32(b, 0); // creation
            put_u32(b, 0); // modification
            put_u32(b, 1000); // timescale
            put_u32(b, 0); // duration unknown (fragmented)
            put_u32(b, 0x0001_0000); // rate 1.0
            put_u16(b, 0x0100); // volume 1.0
            b.extend_from_slice(&[0; 10]);
            for m in MATRIX {
                put_u32(b, m);
            }
            b.extend_from_slice(&[0; 24]);
            put_u32(b, tracks.len() as u32 + 1); // next track id
        });
        for (i, t) in tracks.iter().enumerate() {
            trak(b, i as u32 + 1, t);
        }
        boxed(b, b"mvex", |b| {
            for i in 0..tracks.len() {
                full(b, b"trex", 0, 0, |b| {
                    put_u32(b, i as u32 + 1); // track id
                    put_u32(b, 1); // default sample description index
                    put_u32(b, 0); // default duration
                    put_u32(b, 0); // default size
                    put_u32(b, 0); // default flags
                });
            }
        });
    });
    b
}

fn trak(b: &mut Vec<u8>, id: u32, t: &TrackInfo) {
    let video = t.kind == TrackKind::Video;
    boxed(b, b"trak", |b| {
        // flags: enabled | in movie | in preview
        full(b, b"tkhd", 0, 7, |b| {
            put_u32(b, 0);
            put_u32(b, 0);
            put_u32(b, id);
            put_u32(b, 0); // reserved
            put_u32(b, 0); // duration
            b.extend_from_slice(&[0; 8]);
            put_u16(b, 0); // layer
            put_u16(b, if video { 0 } else { 1 }); // alternate group
            put_u16(b, if video { 0 } else { 0x0100 }); // volume
            put_u16(b, 0);
            for m in MATRIX {
                put_u32(b, m);
            }
            put_u32(b, if video { t.width << 16 } else { 0 });
            put_u32(b, if video { t.height << 16 } else { 0 });
        });
        boxed(b, b"mdia", |b| {
            full(b, b"mdhd", 0, 0, |b| {
                put_u32(b, 0);
                put_u32(b, 0);
                put_u32(b, t.timescale);
                put_u32(b, 0);
                put_u16(b, 0x55c4); // language "und"
                put_u16(b, 0);
            });
            full(b, b"hdlr", 0, 0, |b| {
                put_u32(b, 0);
                b.extend_from_slice(if video { b"vide" } else { b"soun" });
                b.extend_from_slice(&[0; 12]);
                b.extend_from_slice(if video { b"VideoHandler\0" } else { b"SoundHandler\0" });
            });
            boxed(b, b"minf", |b| {
                if video {
                    full(b, b"vmhd", 0, 1, |b| b.extend_from_slice(&[0; 8]));
                } else {
                    full(b, b"smhd", 0, 0, |b| put_u32(b, 0));
                }
                boxed(b, b"dinf", |b| {
                    full(b, b"dref", 0, 0, |b| {
                        put_u32(b, 1);
                        full(b, b"url ", 0, 1, |_| {}); // self-contained
                    });
                });
                boxed(b, b"stbl", |b| {
                    full(b, b"stsd", 0, 0, |b| {
                        put_u32(b, 1);
                        b.extend_from_slice(&t.sample_entry);
                    });
                    full(b, b"stts", 0, 0, |b| put_u32(b, 0));
                    full(b, b"stsc", 0, 0, |b| put_u32(b, 0));
                    full(b, b"stsz", 0, 0, |b| {
                        put_u32(b, 0);
                        put_u32(b, 0);
                    });
                    full(b, b"stco", 0, 0, |b| put_u32(b, 0));
                });
            });
        });
    });
}

// Sample flags (ISO/IEC 14496-12 8.8.3.1): depends_on (2 bits at 24..25),
// is_non_sync_sample (bit 16).
const FLAGS_SYNC: u32 = 0x0200_0000; // depends on nothing
const FLAGS_NON_SYNC: u32 = 0x0101_0000; // depends on others, non-sync
/// trun flags: data-offset, sample-duration, sample-size, sample-flags.
const TRUN_FLAGS: u32 = 0x000001 | 0x000100 | 0x000200 | 0x000400;

/// Build one `moof` + `mdat` fragment. `sequence` starts at 1 and increases
/// by one per fragment within a file.
pub fn fragment(sequence: u32, runs: &[TrackRun]) -> Vec<u8> {
    let runs: Vec<&TrackRun> = runs.iter().filter(|r| !r.samples.is_empty()).collect();
    let mut b = Vec::new();
    // trun data_offset fields are patched once the moof size is known.
    let mut offset_slots: Vec<usize> = Vec::new();
    boxed(&mut b, b"moof", |b| {
        full(b, b"mfhd", 0, 0, |b| put_u32(b, sequence));
        for r in &runs {
            boxed(b, b"traf", |b| {
                // default-base-is-moof: data offsets are relative to moof start.
                full(b, b"tfhd", 0, 0x02_0000, |b| put_u32(b, r.track_index as u32 + 1));
                full(b, b"tfdt", 1, 0, |b| put_u64(b, r.base_decode_time));
                // flags: data-offset, sample-duration, sample-size, sample-flags;
                // plus composition offsets (version 1: signed) only when a
                // sample has one, so streams without B-frames are unchanged.
                let cto = r.samples.iter().any(|s| s.composition_offset != 0);
                let (version, flags) = if cto { (1, TRUN_FLAGS | 0x000800) } else { (0, TRUN_FLAGS) };
                full(b, b"trun", version, flags, |b| {
                    put_u32(b, r.samples.len() as u32);
                    offset_slots.push(b.len());
                    put_u32(b, 0); // data_offset placeholder
                    for s in &r.samples {
                        put_u32(b, s.duration);
                        put_u32(b, s.data.len() as u32);
                        put_u32(b, if s.is_sync { FLAGS_SYNC } else { FLAGS_NON_SYNC });
                        if cto {
                            put_u32(b, s.composition_offset as u32);
                        }
                    }
                });
            });
        }
    });
    let moof_len = b.len();
    let payload: usize = runs.iter().flat_map(|r| r.samples.iter()).map(|s| s.data.len()).sum();
    // mdat header is 8 bytes; each track's data follows the previous one.
    let mut data_pos = moof_len + 8;
    for (slot, r) in offset_slots.iter().zip(runs.iter()) {
        b[*slot..*slot + 4].copy_from_slice(&(data_pos as u32).to_be_bytes());
        data_pos += r.samples.iter().map(|s| s.data.len()).sum::<usize>();
    }
    put_u32(&mut b, (payload + 8) as u32);
    b.extend_from_slice(b"mdat");
    for r in &runs {
        for s in &r.samples {
            b.extend_from_slice(&s.data);
        }
    }
    b
}

/// The picture-only form of a fragment built by `fragment` with a video run
/// first and an audio run after it: the same fragment without the audio
/// run. None when it has no second run (nothing to take out), or is not
/// one of ours.
pub fn video_only(f: &[u8]) -> Option<Vec<u8>> {
    let be = |i: usize| -> Option<usize> { Some(u32::from_be_bytes(f.get(i..i + 4)?.try_into().ok()?) as usize) };
    if f.get(4..8)? != b"moof" || be(0)? > f.len() {
        return None;
    }
    let moof_len = be(0)?;
    // Child boxes of the moof: mfhd, then one traf per track.
    let mut trafs: Vec<(usize, usize)> = Vec::new();
    let mut i = 8;
    while i + 8 <= moof_len {
        let n = be(i)?;
        if n < 8 || i + n > moof_len {
            return None;
        }
        if &f[i + 4..i + 8] == b"traf" {
            trafs.push((i, n));
        }
        i += n;
    }
    if trafs.len() != 2 {
        return None;
    }
    // The data offset of a traf's trun: 12 bytes after the `trun` tag.
    let offset_at = |t: (usize, usize)| -> Option<usize> { Some(f[t.0..t.0 + t.1].windows(4).position(|w| w == b"trun")? + t.0 + 12) };
    let (o1, o2) = (offset_at(trafs[0])?, offset_at(trafs[1])?);
    let (d1, d2) = (be(o1)?, be(o2)?);
    let vlen = d2.checked_sub(d1)?;
    let video = f.get(d1..d1 + vlen)?;
    let new_moof = trafs[0].0 + trafs[0].1;
    let mut out = Vec::with_capacity(new_moof + 8 + vlen);
    out.extend_from_slice(&f[..new_moof]);
    out[..4].copy_from_slice(&(new_moof as u32).to_be_bytes());
    out[o1..o1 + 4].copy_from_slice(&((new_moof + 8) as u32).to_be_bytes());
    out.extend_from_slice(&((vlen + 8) as u32).to_be_bytes());
    out.extend_from_slice(b"mdat");
    out.extend_from_slice(video);
    Some(out)
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    #[test]
    fn picture_only_form_is_the_video_run_alone() {
        let video = TrackRun { track_index: 0, base_decode_time: 900, samples: vec![Sample::new(3000, vec![0xAA; 5], true), Sample::new(3000, vec![0xBB; 7], false)] };
        let audio = TrackRun { track_index: 1, base_decode_time: 160, samples: vec![Sample::new(1024, vec![0xCC; 3], true)] };
        let both = fragment(7, &[video.clone(), audio]);
        let only = video_only(&both).unwrap();
        assert_eq!(only, fragment(7, &[video.clone()]), "same bytes as building it without audio");
        // Nothing to take out of a fragment without audio, or from junk.
        assert!(video_only(&fragment(7, &[video])).is_none());
        assert!(video_only(&[0; 20]).is_none());
        assert!(video_only(&both[..30]).is_none());
    }

    fn box_types(buf: &[u8]) -> Vec<String> {
        let mut out = Vec::new();
        let mut i = 0;
        while i + 8 <= buf.len() {
            let size = u32::from_be_bytes(buf[i..i + 4].try_into().unwrap()) as usize;
            out.push(String::from_utf8_lossy(&buf[i + 4..i + 8]).into_owned());
            assert!(size >= 8 && i + size <= buf.len(), "bad box size at {i}");
            i += size;
        }
        assert_eq!(i, buf.len(), "trailing bytes");
        out
    }

    #[test]
    fn fragment_offsets_point_at_sample_data() {
        let runs = vec![
            TrackRun {
                track_index: 0,
                base_decode_time: 900,
                samples: vec![
                    Sample::new(3000, vec![0xAA; 5], true),
                    Sample::new(3000, vec![0xBB; 7], false),
                ],
            },
            TrackRun {
                track_index: 1,
                base_decode_time: 160,
                samples: vec![Sample::new(1024, vec![0xCC; 3], true)],
            },
        ];
        let f = fragment(1, &runs);
        assert_eq!(box_types(&f), vec!["moof", "mdat"]);
        // Find each trun data_offset and check it lands on the right bytes.
        let mut found = Vec::new();
        for i in 0..f.len() - 4 {
            if &f[i..i + 4] == b"trun" {
                let off = u32::from_be_bytes(f[i + 12..i + 16].try_into().unwrap()) as usize;
                found.push(f[off]);
            }
        }
        assert_eq!(found, vec![0xAA, 0xCC]);
        // The mdat holds video then audio, back to back.
        let tail = &f[f.len() - 15..];
        assert_eq!(&tail[..5], &[0xAA; 5]);
        assert_eq!(&tail[12..], &[0xCC; 3]);
    }

    /// (version, flags, per-sample (duration, size, flags, offset)) of the
    /// first trun in a fragment.
    pub(crate) fn parse_trun(f: &[u8]) -> (u8, u32, Vec<(u32, u32, u32, i32)>) {
        let at = f.windows(4).position(|w| w == b"trun").unwrap() - 4;
        let be = |i: usize| u32::from_be_bytes(f[i..i + 4].try_into().unwrap());
        let (version, flags) = (f[at + 8], be(at + 8) & 0x00ff_ffff);
        let count = be(at + 12) as usize;
        let mut p = at + 20; // after the data offset
        let mut out = Vec::new();
        for _ in 0..count {
            let (d, s, fl) = (be(p), be(p + 4), be(p + 8));
            p += 12;
            let o = if flags & 0x800 != 0 {
                p += 4;
                be(p - 4) as i32
            } else {
                0
            };
            out.push((d, s, fl, o));
        }
        assert_eq!(p, at + be(at) as usize, "trun size matches its samples");
        (version, flags, out)
    }

    #[test]
    fn composition_offsets_are_written_signed_only_when_present() {
        let mk = |offs: &[i32]| {
            let samples = offs
                .iter()
                .enumerate()
                .map(|(i, &o)| Sample { duration: 3000, data: vec![i as u8; 4], is_sync: i == 0, composition_offset: o })
                .collect();
            fragment(1, &[TrackRun { track_index: 0, base_decode_time: 0, samples }])
        };
        // B-frames: version 1, offsets present, negative values survive.
        let f = mk(&[3000, 9000, 0, -3000]);
        assert_eq!(box_types(&f), vec!["moof", "mdat"]);
        let (version, flags, samples) = parse_trun(&f);
        assert_eq!((version, flags), (1, 0x000f01));
        let offs: Vec<i32> = samples.iter().map(|s| s.3).collect();
        assert_eq!(offs, vec![3000, 9000, 0, -3000]);
        assert_eq!(samples[0].2, FLAGS_SYNC);
        assert_eq!(samples[1].2, FLAGS_NON_SYNC);
        let off = u32::from_be_bytes(f[f.windows(4).position(|w| w == b"trun").unwrap() + 12..][..4].try_into().unwrap()) as usize;
        assert_eq!(&f[off..off + 4], &[0; 4], "data offset still lands on the first sample");
        // No offsets: the box is exactly what it always was (version 0, no field).
        let f = mk(&[0, 0, 0]);
        let (version, flags, samples) = parse_trun(&f);
        assert_eq!((version, flags, samples.len()), (0, 0x000701, 3));
        assert_eq!(f, fragment(1, &[TrackRun { track_index: 0, base_decode_time: 0, samples: (0..3).map(|i| Sample::new(3000, vec![i as u8; 4], i == 0)).collect() }]));
    }

    #[test]
    fn empty_runs_are_skipped() {
        let runs = vec![
            TrackRun { track_index: 0, base_decode_time: 0, samples: vec![Sample::new(1, vec![1], true)] },
            TrackRun { track_index: 1, base_decode_time: 0, samples: vec![] },
        ];
        let f = fragment(3, &runs);
        assert_eq!(f.windows(4).filter(|w| w == b"traf").count(), 1);
    }

    #[test]
    fn init_segment_box_layout() {
        let entry = {
            let mut e = Vec::new();
            boxed(&mut e, b"avc1", |b| b.extend_from_slice(&[0; 78]));
            e
        };
        let init = init_segment(&[TrackInfo {
            kind: TrackKind::Video,
            timescale: 90000,
            sample_entry: entry,
            width: 640,
            height: 480,
        }]);
        assert_eq!(box_types(&init), vec!["ftyp", "moov"]);
        assert!(init.windows(4).any(|w| w == b"mvex"));
        assert!(init.windows(4).any(|w| w == b"avc1"));
    }
}
