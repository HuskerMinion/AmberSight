//! Diagnostics for a camera's AAC audio in live view.
//!
//! A browser's audio decoder stops the whole video on one frame it rejects.
//! To learn what such a camera really sends, the first frames of each
//! camera's audio are summarised in one log line per camera per run: the
//! stream header's facts next to what the frames look like. Nothing here
//! changes what is recorded or streamed.

use std::collections::HashSet;
use std::sync::Mutex;

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
