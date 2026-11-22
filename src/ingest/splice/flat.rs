//! Playback for Apple's players (iPhone, iPad, Safari, QuickTime): a plain MP4.
//!
//! AVFoundation reads every fragment of a fragmented MP4 before it says the
//! video is ready: a 10-minute window of a 4K camera meant ~200 MB and close
//! to a minute over a slow link before the first picture. Given one `moov`
//! with the whole sample table it starts at once and reads only what it plays.
//!
//! The same bytes, laid out for that: the header becomes an `ftyp` and a
//! `moov` with full sample tables (made from the fragments' `moof`s as they
//! are served: decode times, durations, sizes, keyframes, composition
//! offsets), each `moof` (and the `sidx`) becomes a `free` box of the same
//! size, and every `mdat` stays where it was. Nothing is re-encoded; only the
//! header grows, so every byte after it moves by the same amount.

use super::{Piece, VirtualClip};
use anyhow::{bail, Context, Result};

/// A top-level box of the clip: where it starts and how long it is.
struct TopBox {
    kind: [u8; 4],
    start: u64,
    len: u64,
}

/// One sample as it plays: decode time (track timescale), duration, size,
/// where its bytes are in the clip, keyframe, composition offset.
#[derive(Clone, Copy)]
struct Sample {
    dts: u64,
    dur: u32,
    size: u32,
    pos: u64,
    sync: bool,
    cto: i32,
}

#[derive(Clone, Copy, Default)]
struct Defaults {
    dur: u32,
    size: u32,
    flags: u32,
}

fn be32(b: &[u8], at: usize) -> Option<u32> {
    b.get(at..at + 4).map(|x| u32::from_be_bytes([x[0], x[1], x[2], x[3]]))
}
fn be64(b: &[u8], at: usize) -> Option<u64> {
    b.get(at..at + 8).map(|x| u64::from_be_bytes([x[0], x[1], x[2], x[3], x[4], x[5], x[6], x[7]]))
}

/// The boxes inside `b[from..to]`: (kind, start of the box, start of its
/// contents, end).
fn kids(b: &[u8], from: usize, to: usize) -> Vec<([u8; 4], usize, usize, usize)> {
    let mut out = Vec::new();
    let mut at = from;
    while at + 8 <= to {
        let Some(size) = be32(b, at) else { break };
        let kind = [b[at + 4], b[at + 5], b[at + 6], b[at + 7]];
        let (len, hdr) = match size {
            1 => match be64(b, at + 8) {
                Some(l) => (l as usize, 16),
                None => break,
            },
            0 => (to - at, 8),
            n => (n as usize, 8),
        };
        if len < hdr || at.checked_add(len).is_none_or(|e| e > to) {
            break;
        }
        out.push((kind, at, at + hdr, at + len));
        at += len;
    }
    out
}

fn kid(b: &[u8], from: usize, to: usize, kind: &[u8; 4]) -> Option<(usize, usize, usize)> {
    kids(b, from, to).into_iter().find(|k| &k.0 == kind).map(|k| (k.1, k.2, k.3))
}

/// A header box's duration (in box `t`, contents from `c`): at `v0` bytes in
/// (32 bits) or, for version 1, at `v1` (64 bits).
fn put_duration(t: &mut [u8], c: usize, v0: usize, v1: usize, d: u64) -> Result<()> {
    let at = |n: usize| t.len() >= c + n;
    match t.get(c) {
        Some(1) if at(v1 + 8) => t[c + v1..c + v1 + 8].copy_from_slice(&d.to_be_bytes()),
        Some(0) if at(v0 + 4) => t[c + v0..c + v0 + 4].copy_from_slice(&(d.min(u64::from(u32::MAX)) as u32).to_be_bytes()),
        _ => bail!("a header box too short or of an unknown version"),
    }
    Ok(())
}

fn bx(kind: &[u8; 4], body: &[u8]) -> Vec<u8> {
    let mut b = Vec::with_capacity(8 + body.len());
    b.extend_from_slice(&((8 + body.len()) as u32).to_be_bytes());
    b.extend_from_slice(kind);
    b.extend_from_slice(body);
    b
}

fn full(kind: &[u8; 4], version: u8, body: &[u8]) -> Vec<u8> {
    let mut v = vec![version, 0, 0, 0];
    v.extend_from_slice(body);
    bx(kind, &v)
}

/// Most samples one track may have here (a 10-minute window holds tens of
/// thousands): beyond it the clip is sent as it is.
const MAX_SAMPLES: usize = 2_000_000;

impl VirtualClip {
    /// Where each piece starts, for [`VirtualClip::read_at`].
    fn piece_starts(&self) -> Vec<u64> {
        let mut pos = 0u64;
        self.pieces.iter().map(|p| {
            let s = pos;
            pos += p.len();
            s
        }).collect()
    }

    /// `read_range` that finds the first piece by search (`starts` from
    /// `piece_starts`), not by walking every piece before it.
    fn read_at(&self, starts: &[u64], a: u64, b: u64) -> std::io::Result<Vec<u8>> {
        let i = starts.partition_point(|&s| s <= a).saturating_sub(1);
        self.read_range_from(i, starts.get(i).copied().unwrap_or(0), a, b)
    }

    /// The clip's top-level boxes, read from its pieces.
    fn top_boxes(&self, starts: &[u64]) -> Result<Vec<TopBox>> {
        let mut out = Vec::new();
        let mut at = 0u64;
        while at < self.total {
            let h = self.read_at(starts, at, (at + 16).min(self.total))?;
            if h.len() < 8 {
                bail!("a box header cut short at {at}");
            }
            let kind = [h[4], h[5], h[6], h[7]];
            let (len, hdr) = match be32(&h, 0).unwrap_or(0) {
                1 => (be64(&h, 8).context("a large box header cut short")?, 16),
                0 => (self.total - at, 8),
                n => (u64::from(n), 8),
            };
            if len < hdr || at.checked_add(len).is_none_or(|e| e > self.total) {
                bail!("a damaged box at {at}");
            }
            out.push(TopBox { kind, start: at, len });
            at += len;
            if out.len() > MAX_SAMPLES {
                bail!("too many boxes");
            }
        }
        Ok(out)
    }

    /// The clip's bytes `a..b` as pieces (the same files, nothing copied
    /// from them); `a` and `b` fall on box edges.
    fn sub_pieces(&self, a: u64, b: u64) -> Vec<Piece> {
        let mut out = Vec::new();
        let mut pos = 0u64;
        for p in &self.pieces {
            let (ps, pe) = (pos, pos + p.len());
            pos = pe;
            if pe <= a || ps >= b {
                continue;
            }
            let (la, lb) = (a.max(ps) - ps, b.min(pe) - ps);
            out.push(match p {
                Piece::Bytes(bytes) => Piece::Bytes(bytes[la as usize..lb as usize].to_vec()),
                Piece::Slice { path, offset, .. } => Piece::Slice { path: path.clone(), offset: offset + la, len: lb - la },
                Piece::Fragment { path, offset, patches, .. } => {
                    let kept: Vec<(u32, u64, u8)> = patches
                        .iter()
                        .filter(|&&(at, _, w)| u64::from(at) >= la && u64::from(at) + u64::from(w) <= lb)
                        .map(|&(at, v, w)| ((u64::from(at) - la) as u32, v, w))
                        .collect();
                    if kept.is_empty() {
                        Piece::Slice { path: path.clone(), offset: offset + la, len: lb - la }
                    } else {
                        Piece::Fragment { path: path.clone(), offset: offset + la, len: lb - la, patches: kept }
                    }
                }
                Piece::Free { .. } => Piece::Free { len: lb - la },
            });
        }
        out
    }

    /// This clip as a plain MP4 (see the module notes); an error when it is
    /// not a fragmented MP4 this can lay out (it is then sent as it is).
    pub(super) fn flattened(&self) -> Result<VirtualClip> {
        let starts = self.piece_starts();
        let boxes = self.top_boxes(&starts)?;
        let (true, Some(moov_at)) = (boxes.first().is_some_and(|b| &b.kind == b"ftyp"), boxes.iter().position(|b| &b.kind == b"moov")) else {
            bail!("no header at the start");
        };
        if !boxes.iter().any(|b| &b.kind == b"moof") {
            bail!("not fragmented");
        }
        let moov_box = &boxes[moov_at];
        let moov = self.read_at(&starts, moov_box.start, moov_box.start + moov_box.len)?;
        let old_head = moov_box.start + moov_box.len;

        // Tracks: id -> (index, timescale, defaults).
        let mut trex: std::collections::HashMap<u32, Defaults> = Default::default();
        if let Some((_, ms, me)) = kid(&moov, 8, moov.len(), b"mvex") {
            for (k, _, s, _) in kids(&moov, ms, me) {
                if &k == b"trex" {
                    let id = be32(&moov, s + 4).context("trex")?;
                    trex.insert(id, Defaults { dur: be32(&moov, s + 12).unwrap_or(0), size: be32(&moov, s + 16).unwrap_or(0), flags: be32(&moov, s + 20).unwrap_or(0) });
                }
            }
        }
        let traks: Vec<(usize, usize, usize)> = kids(&moov, 8, moov.len()).into_iter().filter(|k| &k.0 == b"trak").map(|k| (k.1, k.2, k.3)).collect();
        let mut ids = Vec::new();
        for &(_, s, e) in &traks {
            let (_, hs, _) = kid(&moov, s, e, b"tkhd").context("trak without tkhd")?;
            let id = if moov.get(hs) == Some(&1) { be32(&moov, hs + 20) } else { be32(&moov, hs + 12) }.context("tkhd")?;
            ids.push(id);
        }
        let mut samples: Vec<Vec<Sample>> = vec![Vec::new(); traks.len()];

        // The fragments, as served.
        for b in &boxes[moov_at + 1..] {
            if &b.kind != b"moof" {
                continue;
            }
            let moof = self.read_at(&starts, b.start, b.start + b.len)?;
            let mut prev_end: Option<u64> = None;
            for (k, _, ts, te) in kids(&moof, 8, moof.len()) {
                if &k != b"traf" {
                    continue;
                }
                let (_, hs, _) = kid(&moof, ts, te, b"tfhd").context("traf without tfhd")?;
                let tf = be32(&moof, hs).context("tfhd")? & 0x00ff_ffff;
                let id = be32(&moof, hs + 4).context("tfhd")?;
                let Some(ti) = ids.iter().position(|&x| x == id) else { continue };
                if tf & 0x1 != 0 {
                    bail!("a fragment with its own base offset");
                }
                let mut q = hs + 8;
                if tf & 0x2 != 0 {
                    q += 4;
                }
                let base = trex.get(&id).copied().unwrap_or_default();
                let mut d = base;
                if tf & 0x8 != 0 {
                    d.dur = be32(&moof, q).context("tfhd")?;
                    q += 4;
                }
                if tf & 0x10 != 0 {
                    d.size = be32(&moof, q).context("tfhd")?;
                    q += 4;
                }
                if tf & 0x20 != 0 {
                    d.flags = be32(&moof, q).context("tfhd")?;
                }
                // Data positions count from the moof (default-base-is-moof, or
                // the first traf), else from where the previous traf's ended.
                let base_at = if tf & 0x2_0000 != 0 { b.start } else { prev_end.unwrap_or(b.start) };
                let mut data = base_at;
                let mut dts = match kid(&moof, ts, te, b"tfdt") {
                    Some((_, s, _)) if moof.get(s) == Some(&1) => be64(&moof, s + 4).context("tfdt")?,
                    Some((_, s, _)) => u64::from(be32(&moof, s + 4).context("tfdt")?),
                    None => samples[ti].last().map(|l| l.dts + u64::from(l.dur)).unwrap_or(0),
                };
                for (rk, _, rs, _) in kids(&moof, ts, te) {
                    if &rk != b"trun" {
                        continue;
                    }
                    let v = *moof.get(rs).context("trun")?;
                    let fl = be32(&moof, rs).context("trun")? & 0x00ff_ffff;
                    let count = be32(&moof, rs + 4).context("trun")? as usize;
                    let mut p = rs + 8;
                    if fl & 0x1 != 0 {
                        data = base_at.wrapping_add_signed(i64::from(be32(&moof, p).context("trun")? as i32));
                        p += 4;
                    }
                    let first_flags = if fl & 0x4 != 0 {
                        p += 4;
                        be32(&moof, p - 4)
                    } else {
                        None
                    };
                    let stride = 4 * (fl & 0xf00).count_ones() as usize;
                    if count.checked_mul(stride).and_then(|n| n.checked_add(p)).is_none_or(|e| e > moof.len()) || samples[ti].len() + count > MAX_SAMPLES {
                        bail!("a damaged or very long trun");
                    }
                    for i in 0..count {
                        let mut s = Sample { dts, dur: d.dur, size: d.size, pos: data, sync: false, cto: 0 };
                        let mut flags = if i == 0 { first_flags.unwrap_or(d.flags) } else { d.flags };
                        if fl & 0x100 != 0 {
                            s.dur = be32(&moof, p).unwrap_or(0);
                            p += 4;
                        }
                        if fl & 0x200 != 0 {
                            s.size = be32(&moof, p).unwrap_or(0);
                            p += 4;
                        }
                        if fl & 0x400 != 0 {
                            flags = be32(&moof, p).unwrap_or(0);
                            p += 4;
                        }
                        if fl & 0x800 != 0 {
                            let raw = be32(&moof, p).unwrap_or(0);
                            s.cto = if v == 0 { raw.min(i32::MAX as u32) as i32 } else { raw as i32 };
                            p += 4;
                        }
                        // sample_is_non_sync_sample (bit 16) clear: a keyframe.
                        s.sync = flags & 0x0001_0000 == 0;
                        if s.pos.checked_add(u64::from(s.size)).is_none_or(|e| e > self.total) {
                            bail!("a sample past the clip's end");
                        }
                        data = s.pos + u64::from(s.size);
                        dts = dts.checked_add(u64::from(s.dur)).context("decode times past the end of time")?;
                        samples[ti].push(s);
                    }
                }
                prev_end = Some(data);
            }
        }

        // The new header, twice: its size first (positions do not change it),
        // then with every sample's position moved by how much it grew.
        let build = |shift: i64| -> Result<Vec<u8>> {
            let movie_ts = {
                let (_, hs, _) = kid(&moov, 8, moov.len(), b"mvhd").context("no mvhd")?;
                if moov.get(hs) == Some(&1) { be32(&moov, hs + 20) } else { be32(&moov, hs + 12) }.filter(|&t| t > 0).unwrap_or(1000)
            };
            let mut body = Vec::new();
            let mut movie_dur = 0u64;
            let mut trak_boxes = Vec::new();
            for (ti, &(_, s, e)) in traks.iter().enumerate() {
                let smp = &samples[ti];
                let (_, md0, mde) = kid(&moov, s, e, b"mdia").context("trak without mdia")?;
                let (_, mh, _) = kid(&moov, md0, mde, b"mdhd").context("no mdhd")?;
                let media_ts = if moov.get(mh) == Some(&1) { be32(&moov, mh + 20) } else { be32(&moov, mh + 12) }.filter(|&t| t > 0).unwrap_or(1000);
                // Durations from one decode time to the next (gaps in the
                // recording become a longer sample; never 0).
                let mut durs: Vec<u32> = Vec::with_capacity(smp.len());
                for (i, x) in smp.iter().enumerate() {
                    let d = match smp.get(i + 1) {
                        Some(n) if n.dts > x.dts => (n.dts - x.dts).min(u64::from(u32::MAX)) as u32,
                        // Decode times that stand still or go back: no plain
                        // timeline to give them (sent as they are).
                        Some(_) => bail!("decode times that go back"),
                        None => x.dur.max(1),
                    };
                    durs.push(d);
                }
                let first = smp.first().map(|x| x.dts).unwrap_or(0);
                let media_dur: u64 = durs.iter().map(|&d| u64::from(d)).sum();
                let to_movie = |t: u64| -> u64 { ((t as u128 * movie_ts as u128 + media_ts as u128 / 2) / media_ts as u128) as u64 };
                // The old edit list (an export's cut: a pause, then from a
                // media time for a set length), in the clip's media time,
                // which counted from 0; the new one counts from this track's
                // first sample.
                let (mut old_empty, mut old_len, mut old_mt) = (0u64, 0u64, 0i64);
                if let Some((_, es, ee)) = kid(&moov, s, e, b"edts") {
                    if let Some((_, ls, _)) = kid(&moov, es, ee, b"elst") {
                        let v = *moov.get(ls).context("elst")?;
                        let n = be32(&moov, ls + 4).unwrap_or(0).min(1000) as usize;
                        for k in 0..n {
                            let (d, mt) = if v == 1 {
                                (be64(&moov, ls + 8 + k * 20), be64(&moov, ls + 16 + k * 20).map(|x| x as i64))
                            } else {
                                (be32(&moov, ls + 8 + k * 12).map(u64::from), be32(&moov, ls + 12 + k * 12).map(|x| i64::from(x as i32)))
                            };
                            let (Some(d), Some(mt)) = (d, mt) else { break };
                            if mt < 0 {
                                old_empty = old_empty.saturating_add(d);
                            } else {
                                (old_len, old_mt) = (d, mt);
                                break;
                            }
                        }
                    }
                }
                let rel = old_mt - first.min(i64::MAX as u64) as i64;
                let pause = old_empty + if rel < 0 { to_movie(rel.unsigned_abs()) } else { 0 };
                let media_time = rel.max(0);
                let shown = if old_len > 0 { old_len.saturating_sub(if rel < 0 { to_movie(rel.unsigned_abs()) } else { 0 }) } else { to_movie(media_dur.saturating_sub(media_time as u64)) };
                let track_movie = pause.saturating_add(shown);
                movie_dur = movie_dur.max(track_movie);
                let mut trak = Vec::new();
                for (k, ks, kc, ke) in kids(&moov, s, e) {
                    match &k {
                        b"tkhd" => {
                            let mut t = moov[ks..ke].to_vec();
                            put_duration(&mut t, kc - ks, 20, 28, track_movie)?;
                            trak.extend_from_slice(&t);
                            // The edit list: none needed, or a pause (a track
                            // that starts later), then the track from a media
                            // time for a length (an export's cut).
                            if pause > 0 || media_time != 0 || old_len > 0 {
                                let mut el = Vec::new();
                                let entries: Vec<(u64, i64)> = if pause > 0 { vec![(pause, -1), (shown, media_time)] } else { vec![(shown, media_time)] };
                                el.extend_from_slice(&(entries.len() as u32).to_be_bytes());
                                for (d, m) in entries {
                                    el.extend_from_slice(&d.to_be_bytes());
                                    el.extend_from_slice(&m.to_be_bytes());
                                    el.extend_from_slice(&0x0001_0000u32.to_be_bytes());
                                }
                                trak.extend_from_slice(&bx(b"edts", &full(b"elst", 1, &el)));
                            }
                        }
                        b"edts" => {}
                        b"mdia" => {
                            let mut mdia = Vec::new();
                            for (mk, ms, mc, me) in kids(&moov, kc, ke) {
                                match &mk {
                                    b"mdhd" => {
                                        let mut t = moov[ms..me].to_vec();
                                        put_duration(&mut t, mc - ms, 16, 24, media_dur)?;
                                        mdia.extend_from_slice(&t);
                                    }
                                    b"minf" => {
                                        let mut minf = Vec::new();
                                        for (nk, ns, nc, ne) in kids(&moov, mc, me) {
                                            if &nk == b"stbl" {
                                                let (st, _, _) = kid(&moov, nc, ne, b"stsd").map(|(a, b, c)| (moov[a..c].to_vec(), b, c)).context("no stsd")?;
                                                minf.extend_from_slice(&bx(b"stbl", &stbl(&st, smp, &durs, shift)));
                                            } else {
                                                minf.extend_from_slice(&moov[ns..ne]);
                                            }
                                        }
                                        mdia.extend_from_slice(&bx(b"minf", &minf));
                                    }
                                    _ => mdia.extend_from_slice(&moov[ms..me]),
                                }
                            }
                            trak.extend_from_slice(&bx(b"mdia", &mdia));
                        }
                        _ => trak.extend_from_slice(&moov[ks..ke]),
                    }
                }
                trak_boxes.push(bx(b"trak", &trak));
            }
            for (k, ks, kc, ke) in kids(&moov, 8, moov.len()) {
                match &k {
                    b"mvhd" => {
                        let mut t = moov[ks..ke].to_vec();
                        put_duration(&mut t, kc - ks, 16, 24, movie_dur)?;
                        body.extend_from_slice(&t);
                    }
                    b"trak" => body.extend_from_slice(&trak_boxes.remove(0)),
                    b"mvex" => {}
                    _ => body.extend_from_slice(&moov[ks..ke]),
                }
            }
            let mut head = bx(b"ftyp", &[b"isom".as_slice(), &512u32.to_be_bytes(), b"isom".as_slice(), b"iso2".as_slice(), b"mp41".as_slice()].concat());
            head.extend_from_slice(&bx(b"moov", &body));
            Ok(head)
        };
        let probe = build(0)?;
        let shift = probe.len() as i64 - old_head as i64;
        let head = build(shift)?;
        if head.len() != probe.len() {
            bail!("the header changed size");
        }

        // The rest as it was, each moof (and anything else that is not
        // media) a free box of the same size.
        let mut pieces = vec![Piece::Bytes(head)];
        let mut run: Option<(u64, u64)> = None;
        for b in &boxes[moov_at + 1..] {
            let keep = matches!(&b.kind, b"mdat" | b"free" | b"skip");
            if keep {
                run = Some(match run {
                    Some((a, _)) => (a, b.start + b.len),
                    None => (b.start, b.start + b.len),
                });
            } else {
                if let Some((a, z)) = run.take() {
                    pieces.extend(self.sub_pieces(a, z));
                }
                pieces.push(Piece::Free { len: b.len });
            }
        }
        if let Some((a, z)) = run.take() {
            pieces.extend(self.sub_pieces(a, z));
        }
        let total = pieces.iter().map(|p| p.len()).sum::<u64>();
        if total as i64 != self.total as i64 + shift {
            bail!("the laid-out clip is not the right size");
        }
        Ok(VirtualClip { pieces, total, apple: Default::default(), frag_wall: Vec::new(), first_cto: 0, ..self.clone() })
    }
}

/// A track's sample table: its description, then times, keyframes, chunks
/// (runs of samples one after another in the file), sizes and positions.
fn stbl(stsd: &[u8], smp: &[Sample], durs: &[u32], shift: i64) -> Vec<u8> {
    let mut b = stsd.to_vec();
    // stts
    let mut runs: Vec<(u32, u32)> = Vec::new();
    for &d in durs {
        match runs.last_mut() {
            Some(r) if r.1 == d => r.0 += 1,
            _ => runs.push((1, d)),
        }
    }
    let mut t = (runs.len() as u32).to_be_bytes().to_vec();
    for (n, d) in &runs {
        t.extend_from_slice(&n.to_be_bytes());
        t.extend_from_slice(&d.to_be_bytes());
    }
    b.extend_from_slice(&full(b"stts", 0, &t));
    // ctts (version 1: offsets may be below 0)
    if smp.iter().any(|s| s.cto != 0) {
        let mut runs: Vec<(u32, i32)> = Vec::new();
        for s in smp {
            match runs.last_mut() {
                Some(r) if r.1 == s.cto => r.0 += 1,
                _ => runs.push((1, s.cto)),
            }
        }
        let mut t = (runs.len() as u32).to_be_bytes().to_vec();
        for (n, c) in &runs {
            t.extend_from_slice(&n.to_be_bytes());
            t.extend_from_slice(&c.to_be_bytes());
        }
        b.extend_from_slice(&full(b"ctts", u8::from(smp.iter().any(|s| s.cto < 0)), &t));
    }
    // stss (none: every sample is a keyframe)
    if smp.iter().any(|s| !s.sync) {
        let keys: Vec<u32> = smp.iter().enumerate().filter(|(_, s)| s.sync).map(|(i, _)| i as u32 + 1).collect();
        let mut t = (keys.len() as u32).to_be_bytes().to_vec();
        for k in keys {
            t.extend_from_slice(&k.to_be_bytes());
        }
        b.extend_from_slice(&full(b"stss", 0, &t));
    }
    // Chunks: samples that follow each other in the file.
    let mut chunks: Vec<(u64, u32)> = Vec::new();
    for (i, s) in smp.iter().enumerate() {
        let follows = i > 0 && smp[i - 1].pos + u64::from(smp[i - 1].size) == s.pos;
        match chunks.last_mut() {
            Some(c) if follows => c.1 += 1,
            _ => chunks.push((s.pos, 1)),
        }
    }
    let mut sc: Vec<(u32, u32)> = Vec::new();
    for (i, &(_, n)) in chunks.iter().enumerate() {
        if sc.last().is_none_or(|l| l.1 != n) {
            sc.push((i as u32 + 1, n));
        }
    }
    let mut t = (sc.len() as u32).to_be_bytes().to_vec();
    for (first, n) in &sc {
        t.extend_from_slice(&first.to_be_bytes());
        t.extend_from_slice(&n.to_be_bytes());
        t.extend_from_slice(&1u32.to_be_bytes());
    }
    b.extend_from_slice(&full(b"stsc", 0, &t));
    // stsz
    let mut t = 0u32.to_be_bytes().to_vec();
    t.extend_from_slice(&(smp.len() as u32).to_be_bytes());
    for s in smp {
        t.extend_from_slice(&s.size.to_be_bytes());
    }
    b.extend_from_slice(&full(b"stsz", 0, &t));
    // co64
    let mut t = (chunks.len() as u32).to_be_bytes().to_vec();
    for (pos, _) in &chunks {
        t.extend_from_slice(&((*pos as i64 + shift) as u64).to_be_bytes());
    }
    b.extend_from_slice(&full(b"co64", 0, &t));
    b
}
