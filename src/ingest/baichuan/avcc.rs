//! The bridge from what the camera sends to what the ingest pipeline expects.
//!
//! The Baichuan media stream carries **Annex-B** H.264 (start-code-delimited
//! NAL units). AmberSight's whole pipeline — `recorder::MediaItem::Video`,
//! the MP4 writer, the live muxer, the decoder — expects **AVCC**: each access
//! unit as length-prefixed NAL units (4-byte big-endian lengths), with the SPS
//! and PPS pulled out into an AVCDecoderConfigurationRecord (`VideoParams`).
//! This module converts one and builds the other. It is pure and synchronous,
//! so it unit-tests offline against real captures with no camera.
//!
//! Width and height come from the stream's own SPS when it can be read, else
//! from the BcMedia info message, which the caller supplies via
//! [`AvccBridge::set_dimensions`].
//!
//! H.265 (wired cameras' main streams, e.g. a Duo Floodlight's 4608x1728)
//! works the same way: VPS/SPS/PPS go into an HEVCDecoderConfigurationRecord
//! (`build_hevc_config`, written from ISO/IEC 14496-15 and the H.265 SPS
//! syntax) and IRAP pictures are keyframes.

use crate::ingest::flv::{codec_string, video_sample_entry};
use crate::ingest::recorder::VideoParams;

/// H.264 NAL unit types we care about.
const NAL_IDR: u8 = 5;
const NAL_SPS: u8 = 7;
const NAL_PPS: u8 = 8;
const NAL_AUD: u8 = 9;

/// The H.264 video clock (90 kHz), the MP4/RTP convention for H.264/H.265.
pub const VIDEO_CLOCK: u32 = 90_000;

/// One converted access unit, ready for `MediaItem::Video { data, key, new_params, .. }`.
#[derive(Debug, Clone, PartialEq)]
pub struct BridgedFrame {
    /// Length-prefixed (AVCC) NAL units for this frame, parameter sets removed.
    pub data: Vec<u8>,
    /// The frame contains an IDR slice.
    pub key: bool,
    /// The configuration record changed at or before this frame (first frame
    /// after a new/changed SPS or PPS). The pipeline re-signals parameters.
    pub new_params: bool,
}

/// Split an Annex-B buffer into NAL units (without their start codes). Handles
/// both 3-byte (`00 00 01`) and 4-byte (`00 00 00 01`) start codes, and a
/// buffer that does not begin exactly on a start code (leading bytes before the
/// first start code are ignored).
pub fn split_annexb(data: &[u8]) -> Vec<&[u8]> {
    let mut nals = Vec::new();
    // Find the first start code.
    let mut i = match find_start_code(data, 0) {
        Some((s, _)) => s,
        None => return nals,
    };
    while i < data.len() {
        let (_sc_start, body_start) = match find_start_code(data, i) {
            Some(sc) => sc,
            None => break,
        };
        let (next_start, _) = find_start_code(data, body_start).unwrap_or((data.len(), data.len()));
        if body_start < next_start {
            // Trim any trailing zero byte that belongs to the next start code
            // (a 4-byte start code looks like a trailing 0 on the prior NAL).
            let mut end = next_start;
            while end > body_start && data[end - 1] == 0 {
                end -= 1;
            }
            if end > body_start {
                nals.push(&data[body_start..end]);
            }
        }
        i = next_start;
    }
    nals
}

/// Find the next start code at or after `from`. Returns (start-code offset,
/// NAL-body offset just past the start code).
fn find_start_code(data: &[u8], from: usize) -> Option<(usize, usize)> {
    let mut i = from;
    while i + 3 <= data.len() {
        if data[i] == 0 && data[i + 1] == 0 && data[i + 2] == 1 {
            return Some((i, i + 3));
        }
        i += 1;
    }
    None
}

fn h264_type(nal: &[u8]) -> Option<u8> {
    nal.first().map(|b| b & 0x1f)
}

/// Append one NAL as a 4-byte-length-prefixed (AVCC) unit.
fn push_avcc(out: &mut Vec<u8>, nal: &[u8]) {
    out.extend_from_slice(&(nal.len() as u32).to_be_bytes());
    out.extend_from_slice(nal);
}

/// Build an AVCDecoderConfigurationRecord from one SPS and one PPS NAL (each
/// including its header byte). Layout per ISO/IEC 14496-15. `None` for an SPS
/// too short to carry the profile and level, or a set too long to record.
pub fn build_avc_config(sps: &[u8], pps: &[u8]) -> Option<Vec<u8>> {
    if sps.len() < 4 || pps.is_empty() || sps.len() > u16::MAX as usize || pps.len() > u16::MAX as usize {
        return None;
    }
    // configurationVersion=1, then AVCProfileIndication / profile_compatibility
    // / AVCLevelIndication copied from the SPS, 0xff = (reserved|lengthSize-1=3),
    // 0xe1 = (reserved|numOfSPS=1).
    let mut a = vec![1, sps[1], sps[2], sps[3], 0xff, 0xe1];
    a.extend_from_slice(&(sps.len() as u16).to_be_bytes());
    a.extend_from_slice(sps);
    a.push(1); // numOfPPS
    a.extend_from_slice(&(pps.len() as u16).to_be_bytes());
    a.extend_from_slice(pps);
    Some(a)
}

/// H.265 NAL unit types we care about.
const HEVC_VPS: u8 = 32;
const HEVC_SPS: u8 = 33;
const HEVC_PPS: u8 = 34;
const HEVC_AUD: u8 = 35;

fn h265_type(nal: &[u8]) -> Option<u8> {
    (nal.len() >= 2).then(|| (nal[0] >> 1) & 0x3f)
}

/// A NAL unit's payload without its emulation-prevention bytes
/// (`00 00 03` -> `00 00`).
fn rbsp(nal: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(nal.len());
    let mut zeros = 0;
    for &b in nal {
        if zeros >= 2 && b == 3 {
            zeros = 0;
            continue;
        }
        zeros = if b == 0 { zeros + 1 } else { 0 };
        out.push(b);
    }
    out
}

/// A big-endian bit reader with Exp-Golomb codes.
struct Bits<'a> {
    b: &'a [u8],
    pos: usize,
}

impl Bits<'_> {
    fn bit(&mut self) -> Option<u32> {
        let v = (self.b.get(self.pos / 8)? >> (7 - self.pos % 8)) & 1;
        self.pos += 1;
        Some(v as u32)
    }
    fn bits(&mut self, n: u32) -> Option<u32> {
        let mut v = 0;
        for _ in 0..n {
            v = (v << 1) | self.bit()?;
        }
        Some(v)
    }
    fn skip(&mut self, n: usize) -> Option<()> {
        self.pos += n;
        (self.pos <= self.b.len() * 8).then_some(())
    }
    fn ue(&mut self) -> Option<u32> {
        let mut zeros = 0;
        while self.bit()? == 0 {
            zeros += 1;
            if zeros > 31 {
                return None;
            }
        }
        Some((1u32 << zeros) - 1 + self.bits(zeros)?)
    }
}

/// What the HEVC configuration record copies from the SPS: the 12 bytes
/// of general profile/tier/level, the sub-layer count, the nesting flag,
/// the chroma format and the bit depths.
struct HevcSpsInfo {
    ptl: [u8; 12],
    max_sub_layers: u8,
    temporal_id_nesting: bool,
    chroma_format_idc: u8,
    bit_depth_luma_minus8: u8,
    bit_depth_chroma_minus8: u8,
    /// The picture size, the conformance window taken off.
    width: u32,
    height: u32,
}

fn parse_hevc_sps(nal: &[u8]) -> Option<HevcSpsInfo> {
    let r = rbsp(nal.get(2..)?); // after the 2-byte NAL header
    let mut b = Bits { b: &r, pos: 0 };
    b.bits(4)?; // sps_video_parameter_set_id
    let max_sub_layers_minus1 = b.bits(3)? as usize;
    let temporal_id_nesting = b.bit()? == 1;
    // profile_tier_level: 96 bits of general profile, tier and level.
    let mut ptl = [0u8; 12];
    ptl.copy_from_slice(r.get(1..13)?);
    b.skip(96)?;
    let mut present = Vec::with_capacity(max_sub_layers_minus1);
    for _ in 0..max_sub_layers_minus1 {
        present.push((b.bit()?, b.bit()?));
    }
    if max_sub_layers_minus1 > 0 {
        b.skip(2 * (8 - max_sub_layers_minus1))?;
    }
    for (profile, level) in present {
        if profile == 1 {
            b.skip(88)?;
        }
        if level == 1 {
            b.skip(8)?;
        }
    }
    b.ue()?; // sps_seq_parameter_set_id
    let chroma_format_idc = b.ue()?;
    if chroma_format_idc == 3 {
        b.bit()?; // separate_colour_plane_flag
    }
    let pic_w = b.ue()?; // pic_width_in_luma_samples
    let pic_h = b.ue()?; // pic_height_in_luma_samples
    let (mut crop_w, mut crop_h) = (0u32, 0u32);
    if b.bit()? == 1 {
        // Conformance window offsets (left, right, top, bottom), in chroma units.
        let (sub_w, sub_h) = match chroma_format_idc {
            1 => (2, 2),
            2 => (2, 1),
            _ => (1, 1),
        };
        let (l, r, t, bt) = (b.ue()?, b.ue()?, b.ue()?, b.ue()?);
        crop_w = l.saturating_add(r).saturating_mul(sub_w);
        crop_h = t.saturating_add(bt).saturating_mul(sub_h);
    }
    let luma = b.ue()?;
    let chroma = b.ue()?;
    if chroma_format_idc > 3 || luma > 8 || chroma > 8 {
        return None;
    }
    Some(HevcSpsInfo {
        ptl,
        max_sub_layers: max_sub_layers_minus1 as u8 + 1,
        temporal_id_nesting,
        chroma_format_idc: chroma_format_idc as u8,
        bit_depth_luma_minus8: luma as u8,
        bit_depth_chroma_minus8: chroma as u8,
        width: pic_w.saturating_sub(crop_w),
        height: pic_h.saturating_sub(crop_h),
    })
}

/// Build an HEVCDecoderConfigurationRecord (ISO/IEC 14496-15) from one VPS,
/// SPS and PPS (each with its 2-byte NAL header), 4-byte NAL lengths.
/// `None` when the SPS cannot be read or a set is too long to record.
pub fn build_hevc_config(vps: &[u8], sps: &[u8], pps: &[u8]) -> Option<Vec<u8>> {
    if [vps, sps, pps].iter().any(|n| n.len() < 3 || n.len() > u16::MAX as usize) {
        return None;
    }
    let info = parse_hevc_sps(sps)?;
    let mut a = vec![1u8]; // configurationVersion
    a.extend_from_slice(&info.ptl); // profile space/tier/idc, compat flags, constraint flags, level
    a.extend_from_slice(&[0xf0, 0x00]); // reserved + min_spatial_segmentation_idc 0
    a.push(0xfc); // reserved + parallelismType 0 (unknown)
    a.push(0xfc | info.chroma_format_idc);
    a.push(0xf8 | info.bit_depth_luma_minus8);
    a.push(0xf8 | info.bit_depth_chroma_minus8);
    a.extend_from_slice(&[0, 0]); // avgFrameRate unknown
    // constantFrameRate 0, numTemporalLayers, temporalIdNested, lengthSizeMinusOne 3.
    a.push(((info.max_sub_layers & 7) << 3) | (u8::from(info.temporal_id_nesting) << 2) | 3);
    a.push(3); // numOfArrays
    for (t, nal) in [(HEVC_VPS, vps), (HEVC_SPS, sps), (HEVC_PPS, pps)] {
        a.push(0x80 | t); // array_completeness 1, NAL type
        a.extend_from_slice(&1u16.to_be_bytes());
        a.extend_from_slice(&(nal.len() as u16).to_be_bytes());
        a.extend_from_slice(nal);
    }
    Some(a)
}

/// Converts the camera's Annex-B access units into AVCC frames and tracks the
/// current parameter sets to produce a `VideoParams`. One per stream.
pub struct AvccBridge {
    h265: bool,
    width: u16,
    height: u16,
    vps: Option<Vec<u8>>,
    sps: Option<Vec<u8>>,
    pps: Option<Vec<u8>>,
    config: Option<Vec<u8>>,
    /// Set when the config record changes; cleared when reported on a frame.
    params_dirty: bool,
    /// The size the SPS gives: it wins over the info message's.
    sps_size: Option<(u16, u16)>,
}

impl AvccBridge {
    pub fn new(h265: bool, width: u16, height: u16) -> Self {
        AvccBridge { h265, width, height, vps: None, sps: None, pps: None, config: None, params_dirty: false, sps_size: None }
    }

    /// Update the picture size (from a BcMedia `InfoV2` message). A later Info
    /// with a real size corrects a bridge created before one arrived.
    pub fn is_h265(&self) -> bool {
        self.h265
    }

    /// The SPS's size, when it was read, wins over this.
    pub fn set_dimensions(&mut self, width: u16, height: u16) {
        if self.sps_size.is_some() {
            return;
        }
        self.resize(width, height);
    }

    fn resize(&mut self, width: u16, height: u16) {
        if width > 0 && height > 0 && (width != self.width || height != self.height) {
            self.width = width;
            self.height = height;
            // The sample entry carries the size, so signal fresh parameters.
            if self.config.is_some() {
                self.params_dirty = true;
            }
        }
    }

    /// Take the picture size from the new SPS (when it reads as a size a
    /// recording can have).
    fn size_from_sps(&mut self) {
        let Some(sps) = &self.sps else { return };
        let size = if self.h265 {
            parse_hevc_sps(sps).map(|s| (s.width, s.height))
        } else {
            crate::ingest::sps::sps_display_size(sps)
        };
        let Some((w, h)) = size else { return };
        let (Ok(w), Ok(h)) = (u16::try_from(w), u16::try_from(h)) else { return };
        if (16..=16384).contains(&w) && (16..=16384).contains(&h) {
            self.sps_size = Some((w, h));
            self.resize(w, h);
        }
    }

    fn rebuild_config(&mut self) {
        self.size_from_sps();
        if let (Some(sps), Some(pps)) = (&self.sps, &self.pps) {
            let next = if self.h265 {
                let Some(vps) = &self.vps else { return };
                build_hevc_config(vps, sps, pps)
            } else {
                build_avc_config(sps, pps)
            };
            let Some(next) = next else { return };
            if self.config.as_deref() != Some(next.as_slice()) {
                self.config = Some(next);
                self.params_dirty = true;
            }
        }
    }

    /// Feed one access unit's Annex-B bytes (one BcMedia video frame). Returns a
    /// converted frame, or `None` when the unit carried only parameter sets (or
    /// nothing usable, or H.265 which is not yet supported).
    pub fn push_frame(&mut self, annexb: &[u8]) -> Option<BridgedFrame> {
        let mut out = Vec::new();
        let mut key = false;
        if self.h265 {
            for nal in split_annexb(annexb) {
                let Some(t) = h265_type(nal) else { continue };
                let slot = match t {
                    HEVC_VPS => &mut self.vps,
                    HEVC_SPS => &mut self.sps,
                    HEVC_PPS => &mut self.pps,
                    HEVC_AUD => continue,
                    _ => {
                        // IRAP pictures (BLA, IDR, CRA: types 16..=21) are keyframes.
                        if (16..=21).contains(&t) {
                            key = true;
                        }
                        push_avcc(&mut out, nal);
                        continue;
                    }
                };
                if slot.as_deref() != Some(nal) {
                    *slot = Some(nal.to_vec());
                    self.rebuild_config();
                }
            }
            if out.is_empty() {
                return None;
            }
            let new_params = self.params_dirty && self.config.is_some();
            if new_params {
                self.params_dirty = false;
            }
            return Some(BridgedFrame { data: out, key, new_params });
        }
        for nal in split_annexb(annexb) {
            match h264_type(nal) {
                Some(NAL_SPS) => {
                    if self.sps.as_deref() != Some(nal) {
                        self.sps = Some(nal.to_vec());
                        self.rebuild_config();
                    }
                }
                Some(NAL_PPS) => {
                    if self.pps.as_deref() != Some(nal) {
                        self.pps = Some(nal.to_vec());
                        self.rebuild_config();
                    }
                }
                Some(NAL_AUD) => {} // access-unit delimiter: drop
                Some(NAL_IDR) => {
                    key = true;
                    push_avcc(&mut out, nal);
                }
                Some(_) => push_avcc(&mut out, nal), // non-IDR VCL, SEI, etc.
                None => {}
            }
        }
        if out.is_empty() {
            return None; // parameter-set-only AU: params updated, no frame
        }
        // Only report new parameters once we actually have a full config record.
        let new_params = self.params_dirty && self.config.is_some();
        if new_params {
            self.params_dirty = false;
        }
        Some(BridgedFrame { data: out, key, new_params })
    }

    /// The current video parameters, once an SPS+PPS have been seen.
    pub fn video_params(&self) -> Option<VideoParams> {
        let record = self.config.as_ref()?;
        Some(VideoParams {
            codec: codec_string(self.h265, record)?,
            h265: self.h265,
            extra_data: record.clone(),
            sample_entry: video_sample_entry(self.h265, record, self.width, self.height),
            width: self.width as u32,
            height: self.height as u32,
            clock_rate: VIDEO_CLOCK,
        })
    }

}

#[cfg(test)]
mod tests {

    #[test]
    fn a_too_short_sps_is_refused_not_a_panic() {
        assert_eq!(build_avc_config(&[0x67], &[0x68, 1]), None);
        assert_eq!(build_avc_config(&[0x67, 0x64, 0, 0x28], &[]), None);
        assert!(build_avc_config(&[0x67, 0x64, 0, 0x28], &[0x68, 1]).is_some());
    }

    use super::*;
    use crate::ingest::decode::{length_prefixed_to_annexb, parameter_sets_annexb, Codec};

    // Minimal but structurally valid NAL units (header byte + a little payload).
    // profile_idc 0x64 (High) so codec_string/sample_entry are exercised.
    fn sps_2560x1920() -> Vec<u8> {
        vec![
            0x67, 0x64, 0x00, 0x32, 0xac, 0xd9, 0x40, 0x28, 0x00, 0xf1, 0xb0, 0x11, 0x00, 0x00, 0x03, 0x00, 0x01, 0x00, 0x00, 0x03,
            0x00, 0x32, 0x0f, 0x18, 0x31, 0x96,
        ]
    }
    fn sps() -> Vec<u8> {
        vec![0x67, 0x64, 0x00, 0x1f, 0xac, 0xd9, 0x40, 0x50, 0x05, 0xbb]
    }
    fn pps() -> Vec<u8> {
        vec![0x68, 0xeb, 0xe3, 0xcb, 0x22, 0xc0]
    }
    fn idr() -> Vec<u8> {
        vec![0x65, 0x88, 0x84, 0x00, 0x10, 0xff]
    }
    fn nonidr() -> Vec<u8> {
        vec![0x41, 0x9a, 0x00, 0x20, 0x55]
    }

    fn annexb(nals: &[&[u8]], four_byte: bool) -> Vec<u8> {
        let mut v = Vec::new();
        for n in nals {
            if four_byte {
                v.extend_from_slice(&[0, 0, 0, 1]);
            } else {
                v.extend_from_slice(&[0, 0, 1]);
            }
            v.extend_from_slice(n);
        }
        v
    }

    #[test]
    fn splits_both_start_code_lengths() {
        let s = sps();
        let p = pps();
        for four in [false, true] {
            let buf = annexb(&[&s, &p], four);
            let nals = split_annexb(&buf);
            assert_eq!(nals, vec![s.as_slice(), p.as_slice()], "four_byte={four}");
        }
    }

    #[test]
    fn split_handles_mixed_start_codes_and_leading_junk() {
        // 4-byte SPS, 3-byte PPS, then a 4-byte IDR, with a stray byte in front.
        let mut buf = vec![0xAA];
        buf.extend_from_slice(&[0, 0, 0, 1]);
        buf.extend_from_slice(&sps());
        buf.extend_from_slice(&[0, 0, 1]);
        buf.extend_from_slice(&pps());
        buf.extend_from_slice(&[0, 0, 0, 1]);
        buf.extend_from_slice(&idr());
        assert_eq!(split_annexb(&buf), vec![sps().as_slice(), pps().as_slice(), idr().as_slice()]);
    }

    #[test]
    fn config_record_layout_round_trips_through_decode() {
        let config = build_avc_config(&sps(), &pps()).unwrap();
        // Fixed prefix per the spec.
        assert_eq!(&config[..6], &[1, sps()[1], sps()[2], sps()[3], 0xff, 0xe1]);
        // decode.rs must be able to pull the same SPS and PPS back out.
        let sets = parameter_sets_annexb(Codec::H264, &config).unwrap();
        let got = split_annexb(&sets);
        assert_eq!(got, vec![sps().as_slice(), pps().as_slice()]);
    }

    #[test]
    fn keyframe_au_yields_avcc_params_and_round_trips_to_annexb() {
        let mut b = AvccBridge::new(false, 640, 360);
        let au = annexb(&[&sps(), &pps(), &idr()], false);
        let f = b.push_frame(&au).expect("a frame");
        assert!(f.key, "IDR -> keyframe");
        assert!(f.new_params, "first full config -> new_params");
        // The frame data is exactly the IDR, length-prefixed; converting back to
        // Annex-B must reproduce the IDR NAL.
        let mut back = Vec::new();
        length_prefixed_to_annexb(&f.data, &mut back).unwrap();
        assert_eq!(split_annexb(&back), vec![idr().as_slice()]);

        let vp = b.video_params().expect("params after SPS+PPS");
        assert!(vp.codec.starts_with("avc1"), "codec {}", vp.codec);
        assert!(!vp.h265);
        assert_eq!((vp.width, vp.height, vp.clock_rate), (640, 360, 90_000));
        assert!(!vp.sample_entry.is_empty());
        assert_eq!(vp.extra_data, build_avc_config(&sps(), &pps()).unwrap());
    }

    #[test]
    fn later_pframe_is_not_key_and_not_new_params() {
        let mut b = AvccBridge::new(false, 640, 360);
        b.push_frame(&annexb(&[&sps(), &pps(), &idr()], false)).unwrap();
        let p = b.push_frame(&annexb(&[&nonidr()], false)).expect("a frame");
        assert!(!p.key);
        assert!(!p.new_params, "unchanged params -> no re-signal");
        let mut back = Vec::new();
        length_prefixed_to_annexb(&p.data, &mut back).unwrap();
        assert_eq!(split_annexb(&back), vec![nonidr().as_slice()]);
    }

    #[test]
    fn parameter_set_only_au_updates_config_but_yields_no_frame() {
        let mut b = AvccBridge::new(false, 640, 360);
        assert!(b.push_frame(&annexb(&[&sps(), &pps()], false)).is_none());
        assert!(b.video_params().is_some(), "config assembled from the param-only AU");
        // The next real frame carries the pending new_params.
        let f = b.push_frame(&annexb(&[&idr()], false)).unwrap();
        assert!(f.new_params);
    }

    #[test]
    fn aud_and_leading_params_are_stripped_from_frame_data() {
        let mut b = AvccBridge::new(false, 640, 360);
        let aud = [0x09u8, 0x10];
        let au = {
            let mut v = Vec::new();
            for n in [aud.as_slice(), &sps(), &pps(), &idr()] {
                v.extend_from_slice(&[0, 0, 0, 1]);
                v.extend_from_slice(n);
            }
            v
        };
        let f = b.push_frame(&au).unwrap();
        // Only the IDR survives in the frame data (AUD dropped, SPS/PPS to config).
        let mut back = Vec::new();
        length_prefixed_to_annexb(&f.data, &mut back).unwrap();
        assert_eq!(split_annexb(&back), vec![idr().as_slice()]);
    }

    #[test]
    fn the_sps_size_wins_over_the_info_message() {
        // A real camera SPS: 2560x1920 High profile (built by ffmpeg/x264).
        let sps = sps_2560x1920();
        assert_eq!(crate::ingest::sps::sps_display_size(&sps), Some((2560, 1920)));
        let mut b = AvccBridge::new(false, 2560, 1920);
        b.push_frame(&annexb(&[&sps, &pps(), &idr()], false)).unwrap();
        // A bad info size (issue #13) is ignored once the SPS gave one.
        b.set_dimensions(45941, 22513);
        assert_eq!(b.video_params().map(|v| (v.width, v.height)), Some((2560, 1920)));
        // The bridge made before any info header gets the SPS's size.
        let mut b = AvccBridge::new(false, 0, 0);
        b.push_frame(&annexb(&[&sps, &pps(), &idr()], false)).unwrap();
        assert_eq!(b.video_params().map(|v| (v.width, v.height)), Some((2560, 1920)));
        // H.265: the SPS's size too.
        let mut h = AvccBridge::new(true, 640, 480);
        let (vps, pps, idr) = (vec![0x40, 0x01, 0x0c, 0x01, 0xff, 0xff], vec![0x44, 0x01, 0xc1, 0x72, 0xb4], vec![0x26, 0x01, 0xaf, 0x10, 0x20]);
        h.push_frame(&annexb(&[&vps, &hevc_sps(), &pps, &idr], true)).unwrap();
        assert_eq!(parse_hevc_sps(&hevc_sps()).map(|s| (s.width, s.height)), Some((4608, 1728)));
        h.set_dimensions(45941, 22513);
        assert_eq!(h.video_params().map(|v| (v.width, v.height)), Some((4608, 1728)));
    }

    #[test]
    fn set_dimensions_resizes_and_resignals_params() {
        let mut b = AvccBridge::new(false, 0, 0);
        b.push_frame(&annexb(&[&sps(), &pps(), &idr()], false)).unwrap();
        // A later InfoV2 brings the real size; the next frame re-signals params.
        b.set_dimensions(640, 360);
        let f = b.push_frame(&annexb(&[&nonidr()], false)).unwrap();
        assert!(f.new_params, "a size change re-signals parameters");
        assert_eq!(b.video_params().map(|v| (v.width, v.height)), Some((640, 360)));
    }

    /// Offline end-to-end proof against a REAL camera capture (no camera at run
    /// time). Reads an extracted Annex-B `.h264` at `BRIDGE_TEST_H264`, regroups
    /// it into access units, runs the bridge, and writes a reconstruction
    /// (config parameter sets + each frame's AVCC converted back to Annex-B) to
    /// `BRIDGE_TEST_OUT`. `#[ignore]`d — run explicitly, then decode the output
    /// with ffmpeg and compare the frame count to the original.
    #[test]
    #[ignore]
    fn offline_real_capture_reconstructs_decodable_stream() {
        let path = std::env::var("BRIDGE_TEST_H264").expect("set BRIDGE_TEST_H264");
        let data = std::fs::read(&path).unwrap();
        // Regroup the elementary stream into access units: a new AU starts at
        // each VCL NAL (these cameras send one slice per frame).
        let mut aus: Vec<Vec<u8>> = Vec::new();
        let mut cur: Vec<u8> = Vec::new();
        let mut cur_has_vcl = false;
        for nal in split_annexb(&data) {
            let t = h264_type(nal).unwrap_or(0);
            let is_vcl = t == NAL_IDR || t == 1;
            if is_vcl && cur_has_vcl {
                aus.push(std::mem::take(&mut cur));
                cur_has_vcl = false;
            }
            cur.extend_from_slice(&[0, 0, 0, 1]);
            cur.extend_from_slice(nal);
            cur_has_vcl |= is_vcl;
        }
        if !cur.is_empty() {
            aus.push(cur);
        }

        let mut b = AvccBridge::new(false, 640, 360);
        let mut recon = Vec::new();
        let (mut frames, mut keys) = (0, 0);
        for au in &aus {
            if let Some(f) = b.push_frame(au) {
                if f.new_params {
                    let rec = b.video_params().unwrap().extra_data;
                    recon.extend_from_slice(&parameter_sets_annexb(Codec::H264, &rec).unwrap());
                }
                length_prefixed_to_annexb(&f.data, &mut recon).unwrap();
                frames += 1;
                keys += f.key as i32;
            }
        }
        let out = std::env::var("BRIDGE_TEST_OUT").unwrap_or_else(|_| "/tmp/bridge_recon.h264".into());
        std::fs::write(&out, &recon).unwrap();
        eprintln!("AUs={} frames={frames} keys={keys} recon_bytes={} -> {out}", aus.len(), recon.len());
        assert!(keys >= 1 && frames > keys, "expected several frames incl. a keyframe");
    }

    /// The whole AmberSight video path on a REAL raw camera capture: BcMedia
    /// bytes (as the probe saves them, `.bcm`) fed to `Demux` in UDP-sized
    /// pieces, each frame through `AvccBridge` (what the recorder receives), then
    /// back to Annex-B for ffmpeg to decode. Set `BCM_TEST_FILE` (and optionally
    /// `BCM_TEST_OUT`). `#[ignore]`d: the capture is camera footage and stays out
    /// of the repo.
    #[test]
    #[ignore]
    fn offline_raw_bcmedia_through_demux_and_bridge_decodes() {
        let path = std::env::var("BCM_TEST_FILE").expect("set BCM_TEST_FILE");
        let raw = std::fs::read(&path).unwrap();
        let mut demux = crate::ingest::baichuan::media::Demux::new();
        let mut frames_in = Vec::new();
        for piece in raw.chunks(1400) {
            frames_in.extend(demux.push(piece));
        }
        assert!(demux.width > 0 && demux.height > 0, "info header gave the size");
        let mut b = AvccBridge::new(demux.h265, demux.width, demux.height);
        let mut recon = Vec::new();
        let (mut frames, mut keys) = (0, 0);
        for f in &frames_in {
            if let Some(bf) = b.push_frame(&f.annexb) {
                if bf.new_params {
                    let rec = b.video_params().unwrap().extra_data;
                    recon.extend_from_slice(&parameter_sets_annexb(Codec::H264, &rec).unwrap());
                }
                length_prefixed_to_annexb(&bf.data, &mut recon).unwrap();
                frames += 1;
                keys += bf.key as i32;
            }
        }
        let out = std::env::var("BCM_TEST_OUT").unwrap_or_else(|_| "/tmp/bcm_recon.h264".into());
        std::fs::write(&out, &recon).unwrap();
        eprintln!(
            "raw={} demuxed={} bridged={frames} keys={keys} {}x{} recon_bytes={} -> {out}",
            raw.len(), frames_in.len(), demux.width, demux.height, recon.len()
        );
        assert!(keys >= 1, "at least one keyframe reaches the recorder");
    }

    // An H.265 SPS (Main profile, level 5.1, 4:2:0, 8-bit, 4608x1728),
    // built here bit by bit so the test needs no capture.
    fn hevc_sps() -> Vec<u8> {
        struct W {
            v: Vec<u8>,
            n: u32,
        }
        impl W {
            fn bit(&mut self, b: u32) {
                if self.n % 8 == 0 {
                    self.v.push(0);
                }
                let l = self.v.len() - 1;
                self.v[l] |= ((b & 1) as u8) << (7 - self.n % 8);
                self.n += 1;
            }
            fn bits(&mut self, v: u32, k: u32) {
                for i in (0..k).rev() {
                    self.bit(v >> i);
                }
            }
            fn ue(&mut self, v: u32) {
                let x = v + 1;
                let k = 32 - x.leading_zeros();
                self.bits(0, k - 1);
                self.bits(x, k);
            }
        }
        let mut w = W { v: vec![], n: 0 };
        w.bits(0, 4); // vps id
        w.bits(0, 3); // max_sub_layers_minus1
        w.bit(1); // temporal_id_nesting
        w.bits(0, 2);
        w.bit(0);
        w.bits(1, 5); // profile space, tier, idc = Main
        w.bits(0x6000_0000, 32); // compatibility flags 1 and 2
        w.bits(0x9000, 16);
        w.bits(0, 32); // constraint flags (progressive, frame-only)
        w.bits(153, 8); // level 5.1
        w.ue(0); // sps id
        w.ue(1); // chroma 4:2:0
        w.ue(4608);
        w.ue(1728);
        w.bit(0); // no conformance window
        w.ue(0);
        w.ue(0); // 8-bit
        w.bit(1); // (more syntax follows in a real SPS)
        let mut nal = vec![0x42, 0x01];
        // Emulation prevention, as a real SPS needs it.
        let mut zeros = 0;
        for b in w.v {
            if zeros >= 2 && b <= 3 {
                nal.push(3);
                zeros = 0;
            }
            zeros = if b == 0 { zeros + 1 } else { 0 };
            nal.push(b);
        }
        nal
    }

    #[test]
    fn h265_frames_get_an_hevc_record_and_irap_keyframes() {
        let vps = vec![0x40, 0x01, 0x0c, 0x01, 0xff, 0xff];
        let sps = hevc_sps();
        let pps = vec![0x44, 0x01, 0xc1, 0x72, 0xb4];
        let idr = vec![0x26, 0x01, 0xaf, 0x10, 0x20]; // IDR_W_RADL (19)
        let trail = vec![0x02, 0x01, 0xd0, 0x09]; // TRAIL_R (1)
        let mut b = AvccBridge::new(true, 4608, 1728);
        let f = b.push_frame(&annexb(&[&vps, &sps, &pps, &idr], true)).unwrap();
        assert!(f.key && f.new_params);
        assert_eq!(f.data, [&(idr.len() as u32).to_be_bytes()[..], &idr].concat(), "parameter sets taken out");
        let vp = b.video_params().unwrap();
        assert!(vp.h265);
        assert_eq!(vp.codec, "hvc1.1.6.L153.90");
        let rec = &vp.extra_data;
        assert_eq!(rec[0], 1);
        assert_eq!(rec[16] & 3, 1, "chroma 4:2:0");
        assert_eq!(rec[21] & 3, 3, "4-byte NAL lengths");
        assert_eq!(rec[22], 3, "VPS, SPS and PPS arrays");
        // decode.rs must be able to pull the sets back out.
        let sets = parameter_sets_annexb(Codec::H265, rec).unwrap();
        assert_eq!(split_annexb(&sets), vec![vps.as_slice(), sps.as_slice(), pps.as_slice()]);
        let p = b.push_frame(&annexb(&[&trail], false)).unwrap();
        assert!(!p.key && !p.new_params);
    }

    #[test]
    fn an_unreadable_hevc_sps_gives_no_record() {
        assert!(build_hevc_config(&[0x40, 1, 0], &[0x42, 1], &[0x44, 1, 0]).is_none());
        let mut b = AvccBridge::new(true, 1920, 1080);
        let f = b.push_frame(&annexb(&[&[0x40, 1, 0], &[0x42, 1, 0], &[0x44, 1, 0], &[0x26, 1, 0xaf]], false)).unwrap();
        assert!(f.key && !f.new_params);
        assert!(b.video_params().is_none());
    }
}
