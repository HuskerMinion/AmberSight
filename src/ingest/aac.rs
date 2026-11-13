//! Diagnostics for a camera's AAC audio in live view.
//!
//! A browser's audio decoder stops the whole video on one frame it rejects.
//! To learn what such a camera really sends, the first frames of each
//! camera's audio are summarised in one log line per camera per run: the
//! stream header's facts next to what the frames look like.
//!
//! `raw_frame` is the one thing here that changes what is kept: it turns a
//! camera's AAC frame into the raw frame MP4 expects.

use std::collections::HashSet;
use std::sync::Mutex;

/// The raw AAC frame to record and stream, or None to drop it.
///
/// Some cameras (seen on a Kuna camera) send every frame with its ADTS
/// header (the framing of .aac files) and an empty frame before each one. In
/// an MP4 neither is valid: players and ffmpeg refuse the whole file. An
/// empty frame is dropped; a frame that is exactly one ADTS frame with one
/// raw block loses its header. Anything else is kept as it is.
pub fn raw_frame(data: &[u8]) -> Option<&[u8]> {
    if data.is_empty() {
        return None;
    }
    // Syncword 0xFFF and layer 00.
    if data.len() >= 7 && data[0] == 0xFF && data[1] & 0xF6 == 0xF0 {
        let header = if data[1] & 1 == 0 { 9 } else { 7 }; // CRC present: 2 more bytes
        let frame_len = (usize::from(data[3] & 3) << 11) | (usize::from(data[4]) << 3) | usize::from(data[5] >> 5);
        let blocks = data[6] & 3; // raw data blocks in the frame, minus one
        if frame_len == data.len() && blocks == 0 {
            // A header with nothing after it is an empty frame too.
            return if data.len() > header { Some(&data[header..]) } else { None };
        }
    }
    Some(data)
}

/// The AudioSpecificConfig inside an `mp4a` sample entry's `esds`.
pub fn asc_of_entry(entry: &[u8]) -> Option<&[u8]> {
    let mut i = entry.windows(4).position(|w| w == b"esds")? + 4 + 4; // name, version/flags
    // ES_Descriptor (3): 2 bytes ES_ID + 1 byte flags; DecoderConfig (4):
    // 13 bytes; then DecoderSpecificInfo (5) is the config.
    for (tag, skip) in [(0x03u8, 3usize), (0x04, 13), (0x05, 0)] {
        if *entry.get(i)? != tag {
            return None;
        }
        i += 1;
        let mut len = 0usize;
        for _ in 0..4 {
            let b = *entry.get(i)?;
            i += 1;
            len = (len << 7) | usize::from(b & 0x7f);
            if b & 0x80 == 0 {
                break;
            }
        }
        if tag == 0x05 {
            return entry.get(i..i + len);
        }
        i += skip;
    }
    None
}

/// Frames looked at before the line is written.
const LOOK: u32 = 60;

/// Cameras already reported in this run.
static REPORTED: Mutex<Option<HashSet<String>>> = Mutex::new(None);

/// Collects facts about the first audio frames of one camera stream.
pub struct Diag {
    name: String,
    /// "AAC object type N, R Hz, C channel(s)" from the stream header.
    header: String,
    frames: u32,
    min: usize,
    max: usize,
    adts: u32,
    /// How many frames began with each syntax element id (0 single channel,
    /// 1 channel pair, 2 coupling, 3 LFE, 4 data stream, 5 program config,
    /// 6 fill, 7 end).
    first: [u32; 8],
    done: bool,
}

impl Diag {
    /// None when the sample entry holds no readable AAC config, or this
    /// camera was reported already.
    pub fn new(name: &str, entry: &[u8]) -> Option<Self> {
        let asc = asc_of_entry(entry)?;
        let header = match super::flv::parse_asc(asc) {
            Some((rate, ch)) => format!("AAC object type {}, {} Hz, {} channel config", asc[0] >> 3, rate, ch),
            None => format!("unreadable config ({} bytes)", asc.len()),
        };
        let mut seen = REPORTED.lock().unwrap();
        if !seen.get_or_insert_with(HashSet::new).insert(name.to_string()) {
            return None;
        }
        Some(Diag { name: name.to_string(), header, frames: 0, min: usize::MAX, max: 0, adts: 0, first: [0; 8], done: false })
    }

    pub fn frame(&mut self, data: &[u8]) {
        if self.done {
            return;
        }
        self.frames += 1;
        self.min = self.min.min(data.len());
        self.max = self.max.max(data.len());
        if data.len() > 1 && data[0] == 0xFF && data[1] & 0xF0 == 0xF0 {
            self.adts += 1;
        }
        if let Some(b) = data.first() {
            self.first[usize::from(b >> 5)] += 1;
        }
        if self.frames >= LOOK {
            self.done = true;
            tracing::warn!("{}", self.report());
        }
    }

    pub fn report(&self) -> String {
        let firsts: Vec<String> = self.first.iter().enumerate().filter(|(_, n)| **n > 0).map(|(id, n)| format!("element {id}: {n}")).collect();
        format!(
            "live audio of camera '{}': header says {}; first {} frames: {} start with an ADTS sync, first syntax element [{}], sizes {} to {} bytes",
            self.name,
            self.header,
            self.frames,
            self.adts,
            firsts.join(", "),
            if self.frames == 0 { 0 } else { self.min },
            self.max,
        )
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn camera_frames_become_raw_aac() {
        use super::raw_frame;
        // Empty frames (sent before each real one) are dropped.
        assert_eq!(raw_frame(&[]), None);
        // One whole ADTS frame (no CRC, 130 bytes): the 7-byte header goes.
        let mut f = vec![0xFF, 0xF9, 0x4C, 0x40, 0x10, 0x42, 0xFC];
        f.extend((0..123).map(|i| i as u8));
        assert_eq!(f.len(), 130);
        assert_eq!(raw_frame(&f), Some(&f[7..]));
        // With a CRC the header is 9 bytes.
        let mut c = f.clone();
        c[1] = 0xF8;
        assert_eq!(raw_frame(&c), Some(&c[9..]));
        // A length that does not match, or several raw blocks: left alone.
        let short = &f[..100];
        assert_eq!(raw_frame(short), Some(short));
        let mut multi = f.clone();
        multi[6] = 0xFD;
        assert_eq!(raw_frame(&multi), Some(&multi[..]));
        // A header-only frame is dropped like an empty one.
        assert_eq!(raw_frame(&[0xFF, 0xF9, 0x4C, 0x40, 0x00, 0xFF, 0xFC]), None);
        // Ordinary raw AAC is untouched.
        let raw = [0x21, 0x10, 0x05, 0x00];
        assert_eq!(raw_frame(&raw), Some(&raw[..]));
    }

    use super::*;

    /// AAC-LC, 48 kHz, `ch` channels.
    fn entry(ch: u8) -> Vec<u8> {
        crate::ingest::flv::aac_sample_entry(&[0x11, 0x80 | (ch << 3)]).unwrap()
    }

    #[test]
    fn config_is_found_in_the_sample_entry() {
        assert_eq!(asc_of_entry(&entry(1)).unwrap(), &[0x11, 0x88]);
        assert_eq!(asc_of_entry(&entry(2)).unwrap(), &[0x11, 0x90]);
        assert!(asc_of_entry(&entry(2)[..30]).is_none());
    }

    #[test]
    fn report_has_the_facts_and_no_address() {
        let mut d = Diag::new("unit-test-cam-a", &entry(1)).unwrap();
        let pair = [0x21u8; 141];
        let adts = [0xFFu8, 0xF1, 0x4C, 0x80, 0, 0, 0, 1, 2];
        for _ in 0..3 {
            d.frame(&pair);
        }
        d.frame(&adts);
        d.frame(&[]);
        let r = d.report();
        assert!(r.contains("'unit-test-cam-a'"), "{r}");
        assert!(r.contains("object type 2, 48000 Hz, 1 channel config"), "{r}");
        assert!(r.contains("5 frames: 1 start with an ADTS sync"), "{r}");
        assert!(r.contains("element 1: 3"), "{r}");
        assert!(r.contains("element 7: 1"), "{r}");
        assert!(r.contains("sizes 0 to 141 bytes"), "{r}");
        assert!(!r.contains("://"));
    }

    #[test]
    fn a_camera_is_reported_once_per_run() {
        assert!(Diag::new("unit-test-cam-b", &entry(2)).is_some());
        assert!(Diag::new("unit-test-cam-b", &entry(2)).is_none());
        assert!(Diag::new("unit-test-cam-c", &entry(2)).is_some());
        // Not AAC config: nothing to say.
        assert!(Diag::new("unit-test-cam-d", &[0; 10]).is_none());
    }

    #[test]
    fn the_line_is_written_after_enough_frames_and_only_then() {
        let mut d = Diag::new("unit-test-cam-e", &entry(2)).unwrap();
        for _ in 0..LOOK - 1 {
            d.frame(&[0x21; 10]);
            assert!(!d.done);
        }
        d.frame(&[0x21; 10]);
        assert!(d.done);
        d.frame(&[0x21; 10]);
        assert_eq!(d.frames, LOOK, "later frames are not counted");
    }
}
