//! H.264 sequence parameter sets: letting decoders show each frame as soon
//! as it is decoded.
//!
//! Many cameras (Reolink, for one) send an SPS that says level 5.1 and has
//! no VUI `bitstream_restriction`. A decoder must then assume frames may be
//! reordered and may hold up to the level's whole picture buffer (16 frames
//! for a small picture) before it shows the first one. Hardware decoders
//! (Edge/Chrome on Windows) do: a live picture lags 1-2 s behind, and a
//! recording shows its first picture only once 16 more frames have arrived.
//!
//! For a stream whose frames are shown in the order they are decoded (no
//! B-frames), `low_delay_init` adds the restriction (`max_num_reorder_frames
//! = 0`) to the SPS in an MP4 header's `avcC`, and the decoder outputs each
//! frame at once. `ReorderCheck` tells, from the frames themselves, whether
//! a stream is such a stream. Anything not understood is left as it is.

/// Reads bits, most significant first, from an RBSP.
struct Bits<'a> {
    b: &'a [u8],
    p: usize,
}

impl<'a> Bits<'a> {
    fn new(b: &'a [u8]) -> Self {
        Bits { b, p: 0 }
    }
    fn u(&mut self, n: u32) -> Option<u32> {
        let mut v = 0u64;
        for _ in 0..n {
            let byte = *self.b.get(self.p >> 3)?;
            v = (v << 1) | ((byte >> (7 - (self.p & 7))) & 1) as u64;
            self.p += 1;
        }
        u32::try_from(v).ok()
    }
    fn flag(&mut self) -> Option<bool> {
        self.u(1).map(|v| v == 1)
    }
    fn skip(&mut self, n: usize) -> Option<()> {
        (self.p + n <= self.b.len() * 8).then(|| self.p += n)
    }
    fn ue(&mut self) -> Option<u32> {
        let mut zeros = 0;
        while self.u(1)? == 0 {
            zeros += 1;
            if zeros > 31 {
                return None;
            }
        }
        if zeros == 0 {
            return Some(0);
        }
        Some(((1u64 << zeros) - 1 + self.u(zeros)? as u64).min(u32::MAX as u64) as u32)
    }
    fn se(&mut self) -> Option<i64> {
        let k = self.ue()? as i64;
        Some(if k & 1 == 1 { (k + 1) / 2 } else { -k / 2 })
    }
}

/// Writes bits, most significant first.
#[derive(Default)]
struct BitOut {
    out: Vec<u8>,
    n: usize,
}

impl BitOut {
    fn bit(&mut self, b: bool) {
        if self.n.is_multiple_of(8) {
            self.out.push(0);
        }
        if b {
            *self.out.last_mut().unwrap() |= 0x80 >> (self.n % 8);
        }
        self.n += 1;
    }
    fn u(&mut self, n: u32, v: u32) {
        for i in (0..n).rev() {
            self.bit((v >> i) & 1 == 1);
        }
    }
    fn ue(&mut self, v: u32) {
        let x = v as u64 + 1;
        let len = 64 - x.leading_zeros();
        for _ in 0..len - 1 {
            self.bit(false);
        }
        for i in (0..len).rev() {
            self.bit((x >> i) & 1 == 1);
        }
    }
}

/// NAL unit payload with emulation prevention bytes removed.
fn unescape(nal: &[u8]) -> Vec<u8> {
    let mut o = Vec::with_capacity(nal.len());
    let mut zeros = 0;
    for &x in nal {
        if zeros >= 2 && x == 3 {
            zeros = 0;
            continue;
        }
        o.push(x);
        zeros = if x == 0 { zeros + 1 } else { 0 };
    }
    o
}

/// RBSP with emulation prevention bytes added.
fn escape(rbsp: &[u8]) -> Vec<u8> {
    let mut o = Vec::with_capacity(rbsp.len() + 4);
    let mut zeros = 0;
    for &x in rbsp {
        if zeros >= 2 && x <= 3 {
            o.push(3);
            zeros = 0;
        }
        o.push(x);
        zeros = if x == 0 { zeros + 1 } else { 0 };
    }
    o
}

/// What a slice header needs from its SPS, and what the low-delay rewrite
/// needs to know.
#[derive(Clone, Debug, PartialEq)]
pub struct Sps {
    pub profile: u8,
    /// Luma bit depth (8 unless a High profile says otherwise).
    pub bit_depth: u8,
    pub separate_colour_plane: bool,
    pub log2_max_frame_num: u32,
    pub poc_type: u32,
    pub log2_max_poc_lsb: u32,
    pub max_num_ref_frames: u32,
    pub frame_mbs_only: bool,
    pub vui: bool,
    /// `max_num_reorder_frames`, when the VUI has a bitstream_restriction.
    pub max_num_reorder: Option<u32>,
    /// Bit position (in the RBSP after the NAL header byte) of the VUI
    /// flag, and, with a VUI, of its bitstream_restriction flag.
    vui_flag_at: usize,
    restriction_flag_at: usize,
}

const HIGH_PROFILES: [u8; 13] = [100, 110, 122, 244, 44, 83, 86, 118, 128, 138, 139, 134, 135];

fn skip_hrd(r: &mut Bits) -> Option<()> {
    let cnt = r.ue()?;
    if cnt > 31 {
        return None;
    }
    r.skip(8)?;
    for _ in 0..=cnt {
        r.ue()?;
        r.ue()?;
        r.skip(1)?;
    }
    r.skip(20)
}

/// Reads an SPS NAL unit (header byte included).
pub fn parse_sps(nal: &[u8]) -> Option<Sps> {
    if nal.first()? & 0x1f != 7 {
        return None;
    }
    let rbsp = unescape(&nal[1..]);
    let r = &mut Bits::new(&rbsp);
    let profile = r.u(8)? as u8;
    r.skip(16)?;
    if r.ue()? > 31 {
        return None;
    }
    let mut separate_colour_plane = false;
    let mut bit_depth = 8u8;
    if HIGH_PROFILES.contains(&profile) {
        let chroma = r.ue()?;
        if chroma == 3 {
            separate_colour_plane = r.flag()?;
        }
        bit_depth = 8u8.saturating_add(r.ue()?.min(8) as u8);
        r.ue()?;
        r.skip(1)?;
        if r.flag()? {
            for i in 0..if chroma != 3 { 8 } else { 12 } {
                if !r.flag()? {
                    continue;
                }
                let (mut last, mut next) = (8i64, 8i64);
                for _ in 0..if i < 6 { 16 } else { 64 } {
                    if next != 0 {
                        next = (last + r.se()? + 256).rem_euclid(256);
                    }
                    if next != 0 {
                        last = next;
                    }
                }
            }
        }
    }
    let log2_max_frame_num = r.ue()?.checked_add(4)?;
    let poc_type = r.ue()?;
    let mut log2_max_poc_lsb = 0;
    match poc_type {
        0 => log2_max_poc_lsb = r.ue()?.checked_add(4)?,
        1 => {
            r.skip(1)?;
            r.se()?;
            r.se()?;
            let n = r.ue()?;
            if n > 255 {
                return None;
            }
            for _ in 0..n {
                r.se()?;
            }
        }
        2 => {}
        _ => return None,
    }
    if log2_max_frame_num > 16 || log2_max_poc_lsb > 16 {
        return None;
    }
    let max_num_ref_frames = r.ue()?;
    r.skip(1)?;
    r.ue()?;
    r.ue()?;
    let frame_mbs_only = r.flag()?;
    if !frame_mbs_only {
        r.skip(1)?;
    }
    r.skip(1)?;
    if r.flag()? {
        for _ in 0..4 {
            r.ue()?;
        }
    }
    let vui_flag_at = r.p;
    let vui = r.flag()?;
    let mut max_num_reorder = None;
    let mut restriction_flag_at = 0;
    if vui {
        if r.flag()? && r.u(8)? == 255 {
            r.skip(32)?;
        }
        if r.flag()? {
            r.skip(1)?;
        }
        if r.flag()? {
            r.skip(4)?;
            if r.flag()? {
                r.skip(24)?;
            }
        }
        if r.flag()? {
            r.ue()?;
            r.ue()?;
        }
        if r.flag()? {
            r.skip(65)?;
        }
        let nal_hrd = r.flag()?;
        if nal_hrd {
            skip_hrd(r)?;
        }
        let vcl_hrd = r.flag()?;
        if vcl_hrd {
            skip_hrd(r)?;
        }
        if nal_hrd || vcl_hrd {
            r.skip(1)?;
        }
        r.skip(1)?;
        restriction_flag_at = r.p;
        if r.flag()? {
            r.skip(1)?;
            for _ in 0..4 {
                r.ue()?;
            }
            max_num_reorder = Some(r.ue()?);
            r.ue()?;
        }
    }
    // What follows must be the stop bit: anything else means the SPS was
    // not read the way it was written. (A few zero bits before it are
    // let through: some cameras, a Reolink E1 Outdoor Pro for one, write
    // one; there is no field after the VUI they could belong to.)
    let last_one = (0..rbsp.len() * 8).rev().find(|&i| rbsp[i >> 3] & (0x80 >> (i & 7)) != 0)?;
    if last_one < r.p || last_one - r.p > 7 || (r.p..last_one).any(|i| rbsp[i >> 3] & (0x80 >> (i & 7)) != 0) {
        return None;
    }
    Some(Sps { profile, bit_depth, separate_colour_plane, log2_max_frame_num, poc_type, log2_max_poc_lsb, max_num_ref_frames, frame_mbs_only, vui, max_num_reorder, vui_flag_at, restriction_flag_at })
}

/// True when the SPS already tells a decoder it never has to wait for
/// later frames: a bitstream_restriction, or picture order type 2 (output
/// order is decode order).
pub fn sps_is_low_delay(s: &Sps) -> bool {
    s.max_num_reorder.is_some() || s.poc_type == 2
}

/// The SPS NAL unit (header byte included) with a bitstream_restriction
/// saying no frame is ever reordered, or None when it has one already,
/// needs none, or cannot be read. Only for streams known to have no
/// reordering (see `ReorderCheck`).
pub fn low_delay_sps(nal: &[u8]) -> Option<Vec<u8>> {
    let s = parse_sps(nal)?;
    if sps_is_low_delay(&s) {
        return None;
    }
    let rbsp = unescape(&nal[1..]);
    let src = &mut Bits::new(&rbsp);
    let mut out = BitOut::default();
    let copy = |out: &mut BitOut, src: &mut Bits, n: usize| -> Option<()> {
        for _ in 0..n {
            out.bit(src.flag()?);
        }
        Some(())
    };
    if !s.vui {
        // No VUI: add one that holds only the restriction.
        copy(&mut out, src, s.vui_flag_at)?;
        out.bit(true);
        // aspect ratio, overscan, signal type, chroma location, timing,
        // NAL HRD, VCL HRD, pic_struct: all absent.
        out.u(8, 0);
    } else {
        copy(&mut out, src, s.restriction_flag_at)?;
    }
    out.bit(true); // bitstream_restriction_flag
    out.bit(true); // motion_vectors_over_pic_boundaries_flag
    out.ue(2); // max_bytes_per_pic_denom (the default)
    out.ue(1); // max_bits_per_mb_denom (the default)
    out.ue(15); // log2_max_mv_length_horizontal (the default)
    out.ue(15); // log2_max_mv_length_vertical (the default)
    out.ue(0); // max_num_reorder_frames
    out.ue(s.max_num_ref_frames.max(1)); // max_dec_frame_buffering
    out.bit(true); // rbsp_stop_one_bit, then zeros to the byte
    let mut nal_out = vec![nal[0]];
    nal_out.extend_from_slice(&escape(&out.out));
    // Check the result reads back as the same SPS with the restriction.
    let back = parse_sps(&nal_out)?;
    let same = Sps { max_num_reorder: None, vui: s.vui, vui_flag_at: s.vui_flag_at, restriction_flag_at: s.restriction_flag_at, ..back.clone() };
    (back.max_num_reorder == Some(0) && same == s).then_some(nal_out)
}

/// The SPSs of an avcC record.
fn avcc_sps(avcc: &[u8]) -> Option<(Vec<&[u8]>, usize)> {
    if avcc.len() < 7 || avcc[0] != 1 {
        return None;
    }
    let n = (avcc[5] & 0x1f) as usize;
    let mut at = 6;
    let mut out = Vec::with_capacity(n);
    for _ in 0..n {
        let len = u16::from_be_bytes([*avcc.get(at)?, *avcc.get(at + 1)?]) as usize;
        out.push(avcc.get(at + 2..at + 2 + len)?);
        at += 2 + len;
    }
    Some((out, at))
}

/// An avcC record with low-delay SPSs (see `low_delay_sps`), or None when
/// none of its SPSs changes.
pub fn low_delay_avcc(avcc: &[u8]) -> Option<Vec<u8>> {
    let (sps, rest) = avcc_sps(avcc)?;
    let fixed: Vec<Option<Vec<u8>>> = sps.iter().map(|s| low_delay_sps(s)).collect();
    if fixed.iter().all(Option::is_none) {
        return None;
    }
    let mut out = avcc[..6].to_vec();
    for (s, f) in sps.iter().zip(&fixed) {
        let n: &[u8] = f.as_deref().unwrap_or(s);
        out.extend_from_slice(&u16::try_from(n.len()).ok()?.to_be_bytes());
        out.extend_from_slice(n);
    }
    out.extend_from_slice(&avcc[rest..]);
    Some(out)
}

/// Boxes on the way from an MP4 header to the avcC, with where their
/// children start.
fn child_offset(kind: &[u8]) -> Option<usize> {
    Some(match kind {
        b"moov" | b"trak" | b"mdia" | b"minf" | b"stbl" => 8,
        b"stsd" => 16,
        b"avc1" | b"avc3" => 86,
        _ => return None,
    })
}

/// Where the avcC box is in `b`: (offset, size, offsets of the boxes
/// holding it).
fn find_avcc(b: &[u8], from: usize, to: usize, parents: &mut Vec<usize>) -> Option<(usize, usize)> {
    let mut at = from;
    while at + 8 <= to {
        let size = u32::from_be_bytes(b[at..at + 4].try_into().ok()?) as usize;
        if size < 8 || at + size > to {
            return None;
        }
        let kind = &b[at + 4..at + 8];
        if kind == b"avcC" {
            return Some((at, size));
        }
        if let Some(c) = child_offset(kind).filter(|c| *c < size) {
            parents.push(at);
            if let Some(f) = find_avcc(b, at + c, at + size, parents) {
                return Some(f);
            }
            parents.pop();
        }
        at += size;
    }
    None
}

/// The avcC record inside an MP4 header (ftyp + moov), if any.
pub fn init_avcc(init: &[u8]) -> Option<&[u8]> {
    let (at, size) = find_avcc(init, 0, init.len(), &mut Vec::new())?;
    Some(&init[at + 8..at + size])
}

/// The MP4 header (ftyp + moov, or a moov) with its avcC's SPSs made low
/// delay and every box around it resized, or None when there is nothing to
/// change (no avcC, already low delay, not understood).
pub fn low_delay_init(init: &[u8]) -> Option<Vec<u8>> {
    let mut parents = Vec::new();
    let (at, size) = find_avcc(init, 0, init.len(), &mut parents)?;
    let body = low_delay_avcc(&init[at + 8..at + size])?;
    let new_size = body.len() + 8;
    let delta = new_size as i64 - size as i64;
    let mut out = Vec::with_capacity(init.len() + body.len());
    out.extend_from_slice(&init[..at]);
    out.extend_from_slice(&u32::try_from(new_size).ok()?.to_be_bytes());
    out.extend_from_slice(b"avcC");
    out.extend_from_slice(&body);
    out.extend_from_slice(&init[at + size..]);
    for p in parents {
        let old = u32::from_be_bytes(out[p..p + 4].try_into().ok()?) as i64;
        out[p..p + 4].copy_from_slice(&u32::try_from(old + delta).ok()?.to_be_bytes());
    }
    Some(out)
}

/// Watches a stream's frames (in decode order) for anything shown out of
/// the order it is decoded in: B slices, composition offsets, or picture
/// order counts that do not rise. Only streams it understands can pass:
/// one SPS, frame (not field) coding, picture order type 0 (type 2 never
/// reorders and needs no help).
pub struct ReorderCheck {
    sps: Option<Sps>,
    len_size: usize,
    prev_msb: i64,
    prev_lsb: i64,
    last_poc: Option<i64>,
    /// frame_num of the last reference picture.
    prev_ref_frame_num: Option<u32>,
    /// Breaks in frame_num (lost frames: the order starts over after one).
    pub breaks: u32,
    /// Frames looked at, and keyframes among them.
    pub frames: u32,
    pub keyframes: u32,
    /// Something was shown out of decode order, or could not be read.
    pub reorders: bool,
}

impl ReorderCheck {
    /// For a stream with this avcC record.
    pub fn new(avcc: &[u8]) -> Self {
        let parsed = avcc_sps(avcc).filter(|(s, _)| s.len() == 1).and_then(|(s, _)| parse_sps(s[0]));
        let sps = parsed.filter(|s| s.poc_type == 0 && s.frame_mbs_only && !sps_is_low_delay(s));
        let len_size = avcc.get(4).map_or(4, |b| (b & 3) as usize + 1);
        ReorderCheck { reorders: sps.is_none(), sps, len_size, prev_msb: 0, prev_lsb: 0, last_poc: None, prev_ref_frame_num: None, breaks: 0, frames: 0, keyframes: 0 }
    }

    /// One frame: length-prefixed NAL units, and how much later than its
    /// decode time it is shown.
    pub fn frame(&mut self, data: &[u8], composition_offset: i32) {
        if self.reorders {
            return;
        }
        self.frames += 1;
        if composition_offset != 0 || self.slice(data).is_none() {
            self.reorders = true;
        }
    }

    fn slice(&mut self, data: &[u8]) -> Option<()> {
        let sps = self.sps.as_ref()?;
        let mut at = 0;
        // The first slice of the picture.
        let nal = loop {
            let len = data.get(at..at + self.len_size)?.iter().fold(0usize, |a, &b| (a << 8) | b as usize);
            let nal = data.get(at + self.len_size..at + self.len_size + len)?;
            at += self.len_size + len;
            if matches!(nal.first()? & 0x1f, 1 | 5) {
                break nal;
            }
        };
        let idr = nal[0] & 0x1f == 5;
        let reference = nal[0] & 0x60 != 0;
        // A slice header is short: its start is enough.
        let rbsp = unescape(&nal[1..nal.len().min(64)]);
        let r = &mut Bits::new(&rbsp);
        r.ue()?;
        let slice_type = r.ue()?;
        if slice_type % 5 == 1 || slice_type > 9 {
            return None; // B slice
        }
        r.ue()?;
        if sps.separate_colour_plane {
            r.skip(2)?;
        }
        let frame_num = r.u(sps.log2_max_frame_num)?;
        if idr {
            r.ue()?;
        }
        let lsb = r.u(sps.log2_max_poc_lsb)? as i64;
        let max = 1i64 << sps.log2_max_poc_lsb;
        if idr {
            self.prev_msb = 0;
            self.prev_lsb = 0;
            self.last_poc = None;
            self.keyframes += 1;
        } else if self.prev_ref_frame_num.is_some_and(|p| frame_num != (p + 1) % (1 << sps.log2_max_frame_num)) {
            // Frames went missing (a keyframe lost on the network, say): the
            // picture order count cannot be followed across the break, and
            // a lower one after it is not reordering. Start over from here.
            self.breaks += 1;
            self.prev_msb = 0;
            self.prev_lsb = lsb;
            self.last_poc = None;
        }
        if reference {
            self.prev_ref_frame_num = Some(frame_num);
        }
        // Picture order count (8.2.1.1).
        let msb = if lsb < self.prev_lsb && self.prev_lsb - lsb >= max / 2 {
            self.prev_msb + max
        } else if lsb > self.prev_lsb && lsb - self.prev_lsb > max / 2 {
            self.prev_msb - max
        } else {
            self.prev_msb
        };
        let poc = msb + lsb;
        if self.last_poc.is_some_and(|p| poc <= p) {
            return None;
        }
        self.last_poc = Some(poc);
        if reference {
            self.prev_msb = msb;
            self.prev_lsb = lsb;
        }
        Some(())
    }

    /// True once enough frames were seen (`min_frames`, a whole group of
    /// pictures included) and none was out of order.
    pub fn never_reorders(&self, min_frames: u32) -> bool {
        !self.reorders && self.frames >= min_frames && self.keyframes >= 1
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    fn hex(s: &str) -> Vec<u8> {
        (0..s.len()).step_by(2).map(|i| u8::from_str_radix(&s[i..i + 2], 16).unwrap()).collect()
    }

    /// Writes an SPS from parts, for tests.
    pub(crate) struct SpsSpec {
        profile: u8,
        level: u8,
        poc_type: u32,
        refs: u32,
        w_mbs: u32,
        h_mbs: u32,
        frame_mbs_only: bool,
        crop: Option<[u32; 4]>,
        /// None: no VUI. Some: timing (num_units, time_scale), with an
        /// aspect ratio and a NAL HRD, and a restriction (reorder frames).
        vui: Option<(Option<(u32, u32)>, bool, bool, Option<u32>)>,
        scaling: bool,
    }

    pub(crate) fn write_sps(s: &SpsSpec) -> Vec<u8> {
        let mut o = BitOut::default();
        o.u(8, s.profile as u32);
        o.u(8, 0);
        o.u(8, s.level as u32);
        o.ue(0);
        if HIGH_PROFILES.contains(&s.profile) {
            o.ue(1);
            o.ue(0);
            o.ue(0);
            o.bit(false);
            o.bit(s.scaling);
            if s.scaling {
                for i in 0..8 {
                    o.bit(i % 2 == 0);
                    if i % 2 == 0 {
                        for j in 0..if i < 6 { 16 } else { 64 } {
                            // deltas: +1 then 0 (non-zero scale) ...
                            let d: i64 = if j == 0 { 1 } else { 0 };
                            let k = if d > 0 { 2 * d - 1 } else { -2 * d } as u32;
                            o.ue(k);
                        }
                    }
                }
            }
        }
        o.ue(0); // log2_max_frame_num - 4
        o.ue(s.poc_type);
        if s.poc_type == 0 {
            o.ue(2);
        } else if s.poc_type == 1 {
            o.bit(false);
            o.ue(0);
            o.ue(0);
            o.ue(1);
            o.ue(0);
        }
        o.ue(s.refs);
        o.bit(false);
        o.ue(s.w_mbs - 1);
        o.ue(s.h_mbs - 1);
        o.bit(s.frame_mbs_only);
        if !s.frame_mbs_only {
            o.bit(false);
        }
        o.bit(true);
        o.bit(s.crop.is_some());
        if let Some(c) = s.crop {
            for v in c {
                o.ue(v);
            }
        }
        o.bit(s.vui.is_some());
        if let Some((timing, aspect, hrd, restriction)) = s.vui {
            o.bit(aspect);
            if aspect {
                o.u(8, 255);
                o.u(16, 4);
                o.u(16, 3);
            }
            o.bit(false);
            o.bit(true);
            o.u(3, 5);
            o.bit(false);
            o.bit(true);
            o.u(24, 0x010101);
            o.bit(false);
            o.bit(timing.is_some());
            if let Some((n, t)) = timing {
                o.u(32, n);
                o.u(32, t);
                o.bit(false);
            }
            o.bit(hrd);
            if hrd {
                o.ue(0);
                o.u(4, 2);
                o.u(4, 3);
                o.ue(1000);
                o.ue(2000);
                o.bit(false);
                o.u(20, 0xfffff);
            }
            o.bit(false);
            if hrd {
                o.bit(false);
            }
            o.bit(false);
            o.bit(restriction.is_some());
            if let Some(n) = restriction {
                o.bit(true);
                o.ue(2);
                o.ue(1);
                o.ue(16);
                o.ue(16);
                o.ue(n);
                o.ue(s.refs.max(n));
            }
        }
        o.bit(true);
        let mut nal = vec![0x67];
        nal.extend_from_slice(&escape(&o.out));
        nal
    }

    pub(crate) fn spec() -> SpsSpec {
        SpsSpec { profile: 100, level: 51, poc_type: 0, refs: 1, w_mbs: 40, h_mbs: 23, frame_mbs_only: true, crop: Some([0, 0, 0, 4]), vui: Some((Some((1, 30)), false, false, None)), scaling: false }
    }

    /// An `avc1` sample entry whose avcC has `spec()`'s SPS.
    pub(crate) fn h264_sample_entry() -> Vec<u8> {
        let avcc = avcc_of(&write_sps(&spec()));
        let mut body = vec![0u8; 78];
        body.extend_from_slice(&((avcc.len() + 8) as u32).to_be_bytes());
        body.extend_from_slice(b"avcC");
        body.extend_from_slice(&avcc);
        let mut entry = ((body.len() + 8) as u32).to_be_bytes().to_vec();
        entry.extend_from_slice(b"avc1");
        entry.extend_from_slice(&body);
        entry
    }

    pub(crate) fn avcc_of(sps: &[u8]) -> Vec<u8> {
        let mut a = vec![1, sps[1], sps[2], sps[3], 0xff, 0xe1];
        a.extend_from_slice(&(sps.len() as u16).to_be_bytes());
        a.extend_from_slice(sps);
        a.extend_from_slice(&[1, 0, 4, 0x68, 0xee, 0x3c, 0xb0]);
        a
    }

    #[test]
    fn camera_spss_get_a_restriction() {
        // Reolink E1 Pro sub stream and doorbell main stream (High 5.1, VUI
        // with timing, no restriction).
        for (hex_sps, profile, vui) in [("67640033ac1514a0a02ff961000004f60000457404", 100, true), ("67640033ac1514a02800f1a1000004f60000c67004", 100, true)] {
            let sps = hex(hex_sps);
            let p = parse_sps(&sps).expect("reads");
            assert_eq!((p.profile, p.poc_type, p.max_num_ref_frames, p.vui, p.max_num_reorder), (profile, 0, 1, vui, None));
            let fixed = low_delay_sps(&sps).expect("patched");
            let q = parse_sps(&fixed).unwrap();
            assert_eq!(q.max_num_reorder, Some(0));
            // Everything before the restriction is as it was.
            let keep = if vui { p.restriction_flag_at } else { p.vui_flag_at };
            assert_eq!(&unescape(&fixed[1..])[..keep / 8], &unescape(&sps[1..])[..keep / 8]);
            // Once is enough.
            assert_eq!(low_delay_sps(&fixed), None);
        }
        // Reolink E1 Outdoor Pro: a stray zero bit before the stop bit.
        let stray = hex("67640033ac1514a0a02ff961000004f60000633802");
        assert_eq!(parse_sps(&stray).unwrap().max_num_reorder, None);
        let fixed = low_delay_sps(&stray).unwrap();
        assert_eq!(parse_sps(&fixed).unwrap().max_num_reorder, Some(0));
        assert_eq!(fixed, hex("67640033ac1514a0a02ff961000004f6000063380da08042a0"));
        // Amcrest IP8M sub stream (Main 3.0, no VUI): picture order type 2,
        // never reordered, nothing to add.
        let amcrest = parse_sps(&hex("674d401ea680b03d90")).unwrap();
        assert_eq!((amcrest.profile, amcrest.poc_type, amcrest.vui), (77, 2, false));
        assert_eq!(low_delay_sps(&hex("674d401ea680b03d90")), None);
    }

    #[test]
    fn same_bytes_as_the_live_players_patch() {
        // What static/js/lowdelay.js makes of these camera SPSs (verified in
        // Edge): the server must make the same.
        for (from, to) in [
            ("67640033ac1514a0a02ff961000004f60000457404", "67640033ac1514a0a02ff961000004f6000045740da08042a0"),
            ("67640033ac1514a02800f1a1000004f60000c67004", "67640033ac1514a02800f1a1000004f60000c6700da08042a0"),
            ("67640033ac1514a0e010684000013d8000253501", "67640033ac1514a0e010684000013d8000253503682010a8"),
        ] {
            assert_eq!(low_delay_sps(&hex(from)), Some(hex(to)), "{from}");
        }
    }

    #[test]
    fn many_kinds_of_sps_read_back_patched() {
        let cases: Vec<SpsSpec> = vec![
            spec(),
            SpsSpec { vui: None, ..spec() },
            SpsSpec { profile: 77, vui: None, crop: None, ..spec() },
            SpsSpec { profile: 66, refs: 3, ..spec() },
            SpsSpec { vui: Some((None, true, true, None)), ..spec() },
            SpsSpec { vui: Some((Some((1001, 60000)), true, false, None)), scaling: true, ..spec() },
            SpsSpec { profile: 77, refs: 0, frame_mbs_only: false, ..spec() },
            SpsSpec { poc_type: 1, ..spec() },
            // Picture sizes and timing whose bytes need emulation prevention.
            SpsSpec { w_mbs: 1, h_mbs: 1, crop: Some([0, 0, 0, 0]), vui: Some((Some((0, 0x300)), false, false, None)), ..spec() },
        ];
        for (i, c) in cases.iter().enumerate() {
            let sps = write_sps(c);
            let p = parse_sps(&sps).unwrap_or_else(|| panic!("case {i} reads"));
            assert_eq!((p.profile, p.poc_type, p.max_num_ref_frames, p.frame_mbs_only, p.vui), (c.profile, c.poc_type, c.refs, c.frame_mbs_only, c.vui.is_some()), "case {i}");
            let fixed = low_delay_sps(&sps).unwrap_or_else(|| panic!("case {i} patched"));
            let q = parse_sps(&fixed).unwrap();
            assert_eq!(q.max_num_reorder, Some(0), "case {i}");
            assert!(q.vui);
            // No three-byte start code pattern without emulation prevention.
            assert!(!fixed.windows(3).any(|w| w[0] == 0 && w[1] == 0 && w[2] <= 2), "case {i}");
        }
    }

    #[test]
    fn emulation_prevention_bytes_are_kept_and_added() {
        let c = SpsSpec { w_mbs: 1, h_mbs: 1, crop: Some([0, 0, 0, 0]), vui: Some((Some((0, 0x300)), false, false, None)), ..spec() };
        let sps = write_sps(&c);
        assert!(sps.windows(3).any(|w| w == [0, 0, 3]), "the test SPS has an emulation prevention byte");
        let fixed = low_delay_sps(&sps).unwrap();
        assert!(fixed.windows(3).any(|w| w == [0, 0, 3]));
        assert_eq!(parse_sps(&fixed).unwrap().max_num_reorder, Some(0));
    }

    #[test]
    fn restricted_or_order_type_2_or_unreadable_sps_are_left_alone() {
        assert_eq!(low_delay_sps(&write_sps(&SpsSpec { vui: Some((Some((1, 30)), false, false, Some(2))), ..spec() })), None);
        assert_eq!(low_delay_sps(&write_sps(&SpsSpec { poc_type: 2, ..spec() })), None);
        let sps = write_sps(&spec());
        assert_eq!(low_delay_sps(&sps[..sps.len() - 3]), None);
        assert_eq!(low_delay_sps(&[0x68, 0xee, 0x3c, 0xb0]), None);
        assert_eq!(low_delay_sps(&[]), None);
        // A trailing byte the parser does not expect: not understood.
        let mut odd = sps.clone();
        let last = odd.len() - 1;
        odd[last] ^= 0x0f;
        assert!(parse_sps(&odd).is_none() || low_delay_sps(&odd).is_some_and(|f| parse_sps(&f).is_some()));
    }

    fn init_with(avcc: &[u8]) -> Vec<u8> {
        fn bx(kind: &[u8], body: &[u8]) -> Vec<u8> {
            let mut b = ((body.len() + 8) as u32).to_be_bytes().to_vec();
            b.extend_from_slice(kind);
            b.extend_from_slice(body);
            b
        }
        let mut entry = vec![0u8; 78];
        entry.extend(bx(b"avcC", avcc));
        entry.extend(bx(b"pasp", &[0, 0, 0, 1, 0, 0, 0, 1]));
        let mut stsd = vec![0, 0, 0, 0, 0, 0, 0, 1];
        stsd.extend(bx(b"avc1", &entry));
        let mut stbl = bx(b"stsd", &stsd);
        stbl.extend(bx(b"stts", &[0; 8]));
        let minf = bx(b"stbl", &stbl);
        let mdia = [bx(b"mdhd", &[0; 24]), bx(b"minf", &minf)].concat();
        let trak = [bx(b"tkhd", &[0; 84]), bx(b"mdia", &mdia)].concat();
        let moov = [bx(b"mvhd", &[0; 100]), bx(b"trak", &trak), bx(b"mvex", &[0; 32])].concat();
        [bx(b"ftyp", b"iso6\0\0\0\0"), bx(b"moov", &moov)].concat()
    }

    fn box_sizes_add_up(b: &[u8], from: usize, to: usize) -> bool {
        let mut at = from;
        while at + 8 <= to {
            let size = u32::from_be_bytes(b[at..at + 4].try_into().unwrap()) as usize;
            if size < 8 || at + size > to {
                return false;
            }
            if let Some(c) = child_offset(&b[at + 4..at + 8]) {
                if !box_sizes_add_up(b, at + c, at + size) {
                    return false;
                }
            }
            at += size;
        }
        at == to
    }

    #[test]
    fn init_segments_get_the_patched_avcc_and_correct_box_sizes() {
        let sps = write_sps(&spec());
        let avcc = avcc_of(&sps);
        let init = init_with(&avcc);
        let out = low_delay_init(&init).expect("patched");
        assert!(out.len() > init.len());
        assert!(box_sizes_add_up(&out, 0, out.len()));
        let a = init_avcc(&out).unwrap();
        let (s, rest) = avcc_sps(a).unwrap();
        assert_eq!(parse_sps(s[0]).unwrap().max_num_reorder, Some(0));
        // PPS and the rest of the record unchanged; header bytes too.
        assert_eq!(&a[rest..], &avcc[6 + 2 + sps.len()..]);
        assert_eq!(&a[..6], &avcc[..6]);
        // Patched once; the box after the avcC still there.
        assert_eq!(low_delay_init(&out), None);
        assert!(out.windows(4).any(|w| w == b"pasp"));
        // Nothing to do for an H.265 header or garbage.
        assert_eq!(low_delay_init(b"\0\0\0\x08moov"), None);
        assert_eq!(low_delay_init(&init[..init.len() - 10]), None);
    }

    /// Length-prefixed slice NAL: type, ref, slice_type, frame_num, poc lsb.
    pub(crate) fn slice(idr: bool, reference: bool, slice_type: u32, frame_num: u32, lsb: u32) -> Vec<u8> {
        let mut o = BitOut::default();
        o.ue(0);
        o.ue(slice_type);
        o.ue(0);
        o.u(4, frame_num & 15);
        if idr {
            o.ue(0);
        }
        o.u(6, lsb);
        o.u(8, 0xff); // the rest of the slice
        let mut nal = vec![if idr { 0x65 } else if reference { 0x41 } else { 0x01 }];
        nal.extend_from_slice(&escape(&o.out));
        let sei = [0x06u8, 0x05, 0x01, 0x00, 0x80];
        let mut b = (sei.len() as u32).to_be_bytes().to_vec();
        b.extend_from_slice(&sei);
        b.extend_from_slice(&(nal.len() as u32).to_be_bytes());
        b.extend_from_slice(&nal);
        b
    }

    #[test]
    fn p_only_streams_pass_the_reorder_check() {
        let avcc = avcc_of(&write_sps(&spec()));
        let mut c = ReorderCheck::new(&avcc);
        // Two groups of pictures, POC lsb wrapping at 64.
        for g in 0..2 {
            for i in 0..40u32 {
                let lsb = (i * 2) % 64;
                c.frame(&slice(i == 0, true, if i == 0 { 7 } else { 5 }, i, lsb), 0);
            }
            assert!(!c.reorders, "group {g}");
        }
        assert!(c.never_reorders(60));
        assert!(!c.never_reorders(100));
    }

    #[test]
    fn b_frames_offsets_and_falling_order_fail_the_reorder_check() {
        let avcc = avcc_of(&write_sps(&spec()));
        let run = |frames: &[(bool, u32, u32, i32)]| {
            let mut c = ReorderCheck::new(&avcc);
            for (n, &(idr, st, lsb, cto)) in frames.iter().enumerate() {
                c.frame(&slice(idr, true, st, n as u32, lsb), cto);
            }
            c.reorders
        };
        assert!(!run(&[(true, 7, 0, 0), (false, 5, 2, 0), (false, 5, 4, 0)]));
        // A B slice.
        assert!(run(&[(true, 7, 0, 0), (false, 5, 4, 0), (false, 6, 2, 0)]));
        // P frames shown out of decode order.
        assert!(run(&[(true, 7, 0, 0), (false, 5, 4, 0), (false, 5, 2, 0)]));
        // A composition offset.
        assert!(run(&[(true, 7, 0, 0), (false, 5, 2, 3000)]));
        // A lost keyframe: frame_num and the order count start again
        // without an IDR. Not reordering; the order is followed again after.
        let mut c = ReorderCheck::new(&avcc);
        for i in 0..20u32 {
            c.frame(&slice(i == 0, true, if i == 0 { 7 } else { 5 }, i, i * 2), 0);
        }
        for i in 1..20u32 {
            c.frame(&slice(false, true, 5, i, i * 2), 0);
        }
        assert!(!c.reorders);
        assert_eq!(c.breaks, 1);
        c.frame(&slice(false, true, 5, 4, 30), 0);
        assert!(c.reorders, "out of order after the break");
        // Not a slice at all.
        let mut c = ReorderCheck::new(&avcc);
        c.frame(&[0, 0, 0, 2, 0x09, 0xf0], 0);
        assert!(c.reorders);
        // Streams the check does not follow: never pass.
        for s in [SpsSpec { poc_type: 1, ..spec() }, SpsSpec { frame_mbs_only: false, ..spec() }, SpsSpec { poc_type: 2, ..spec() }] {
            assert!(ReorderCheck::new(&avcc_of(&write_sps(&s))).reorders);
        }
    }
}
