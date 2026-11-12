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
//! Width and height come from the BcMedia `InfoV2` message (the SPS parser in
//! `super::super::sps` does not expose picture size), so the caller supplies
//! them via [`AvccBridge::set_dimensions`].
//!
//! H.265 is not implemented here yet: every battery Argus we have seen streams
//! H.264. [`AvccBridge::push_frame`] returns `None` for an H.265 bridge until
//! the HEVCDecoderConfigurationRecord builder is written (tracked for the
//! reader phase).

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

/// Converts the camera's Annex-B access units into AVCC frames and tracks the
/// current parameter sets to produce a `VideoParams`. One per stream.
pub struct AvccBridge {
    h265: bool,
    width: u16,
    height: u16,
    sps: Option<Vec<u8>>,
    pps: Option<Vec<u8>>,
    config: Option<Vec<u8>>,
    /// Set when the config record changes; cleared when reported on a frame.
    params_dirty: bool,
}

impl AvccBridge {
    pub fn new(h265: bool, width: u16, height: u16) -> Self {
        AvccBridge { h265, width, height, sps: None, pps: None, config: None, params_dirty: false }
    }

    /// Update the picture size (from a BcMedia `InfoV2` message). A later Info
    /// with a real size corrects a bridge created before one arrived.
    pub fn is_h265(&self) -> bool {
        self.h265
    }

    pub fn set_dimensions(&mut self, width: u16, height: u16) {
        if width > 0 && height > 0 && (width != self.width || height != self.height) {
            self.width = width;
            self.height = height;
            // The sample entry carries the size, so signal fresh parameters.
            if self.config.is_some() {
                self.params_dirty = true;
            }
        }
    }

    fn rebuild_config(&mut self) {
        if let (Some(sps), Some(pps)) = (&self.sps, &self.pps) {
            let Some(next) = build_avc_config(sps, pps) else { return };
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
        if self.h265 {
            return None; // TODO(reader phase): HEVCDecoderConfigurationRecord
        }
        let mut out = Vec::new();
        let mut key = false;
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

    #[test]
    fn h265_bridge_is_not_yet_supported() {
        let mut b = AvccBridge::new(true, 1920, 1080);
        assert!(b.push_frame(&annexb(&[&sps(), &pps(), &idr()], false)).is_none());
        assert!(b.video_params().is_none());
    }
}
