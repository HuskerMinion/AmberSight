//! Filling a hole in a camera's recordings from the camera's own card.
//!
//! When a camera drops off (Wi-Fi, a router restart, AmberSight restarting),
//! its recordings have a hole while the camera itself kept recording to its
//! card. This module works out the holes, asks a Reolink camera for the card's
//! recordings of that time, downloads only the part that fits the hole, and
//! writes it in AmberSight's own recording format. Nothing here knows about
//! the engine: the caller hands in the hole and gets the new recordings back.
//!
//! Camera clocks: recordings are dated by this computer's clock, the card by
//! the camera's (a local time without zone). The zone is the camera's own
//! reading of its clock rounded to a quarter hour, and what is left over is
//! its clock error, which is taken off again.

use crate::ingest::fmp4::{TrackInfo, TrackKind};
use crate::ingest::mp4read::{self, MoovAt, Mp4Index, SampleRef, TrackIndex};
use crate::ingest::recorder::{SegmentInfo, SegmentWriter};
use crate::reolink::{CardFile, ReolinkClient};
use anyhow::{anyhow, bail, Context, Result};
use chrono::NaiveDateTime;
use std::path::Path;
use std::time::Duration;

/// Holes shorter than this are the usual seam between two recordings.
pub const MIN_GAP_MS: i64 = 20_000;
/// Longer holes are not filled: the camera was off or switched off on purpose.
pub const MAX_GAP_MS: i64 = 2 * 3600 * 1000;
/// How far back holes are looked for.
pub const LOOK_BACK_MS: i64 = 12 * 3600 * 1000;
/// The most downloaded for one hole (indexes included), and the longest one
/// hole may take; what was fetched by then is kept.
pub const MAX_GAP_BYTES: u64 = 1_200_000_000;
pub const MAX_GAP_TIME: Duration = Duration::from_secs(20 * 60);
/// The most card files used for one hole (a two-hour hole is about 60 to 120).
const MAX_FILES_PER_GAP: usize = 200;
/// A card file the camera says is larger than this is not believed.
pub const MAX_FILE_BYTES: u64 = 4 << 30;
/// Each download request must bring at least this much (or what is left):
/// a camera trickling a few bytes at a time is given up on.
const MIN_PROGRESS: u64 = 4096;
/// A camera clock further off than this is not trusted for placing footage.
const MAX_CLOCK_ERROR_MS: i64 = 5 * 60_000;
/// Most one download request asks for, and the pause after each: the card's
/// recordings come at a gentle pace that leaves a Wi-Fi camera room for its
/// live streams.
const REQUEST_BYTES: u64 = 1 << 20;
const PACE: Duration = Duration::from_millis(120);
/// How much video is fetched and written at a time.
const GROUP_BYTES: u64 = 8 << 20;
/// Samples this close together are fetched in one stretch; one stretch is
/// at most MAX_SPAN, and one group's stretches at most MAX_GROUP_FETCH.
const MERGE_GAP: u64 = 64 * 1024;
const MAX_SPAN: u64 = 2 * GROUP_BYTES;
const MAX_GROUP_FETCH: u64 = 4 * GROUP_BYTES;
/// A seam between two card files longer than this starts a new recording.
const SEAM_MS: f64 = 2_500.0;

/// A stretch of time with no recording.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Gap {
    pub from_ms: i64,
    pub to_ms: i64,
}

/// The holes between `spans` (recordings as (start, end) ms, any order) that
/// begin at or after `window_from_ms` and last between `min_ms` and `max_ms`.
/// Time before the first recording and after the last is not a hole (the camera
/// has not begun, or is still being recorded).
pub fn find_gaps(spans: &[(i64, i64)], window_from_ms: i64, min_ms: i64, max_ms: i64) -> Vec<Gap> {
    let mut s: Vec<(i64, i64)> = spans.to_vec();
    s.sort_unstable();
    let mut out = Vec::new();
    let mut reach: Option<i64> = None;
    for (a, b) in s {
        if let Some(r) = reach {
            let g = a - r;
            if g >= min_ms && g <= max_ms && r >= window_from_ms {
                out.push(Gap { from_ms: r, to_ms: a });
            }
        }
        reach = Some(reach.map_or(b, |r| r.max(b)));
    }
    out
}

/// How a camera's card clock maps onto this computer's.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CardClock {
    /// Its zone: local time minus UTC, in ms (a quarter-hour multiple).
    pub zone_ms: i64,
    /// How far ahead of this computer its clock runs, in ms.
    pub skew_ms: i64,
}

impl CardClock {
    /// From the camera's clock reading `camera_now` taken at `server_now_ms`.
    pub fn from_reading(camera_now: NaiveDateTime, server_now_ms: i64) -> Result<CardClock> {
        let diff = camera_now.and_utc().timestamp_millis() - server_now_ms;
        let quarter = 15 * 60_000;
        let zone_ms = ((diff as f64) / quarter as f64).round() as i64 * quarter;
        let skew_ms = diff - zone_ms;
        if skew_ms.abs() > MAX_CLOCK_ERROR_MS {
            bail!("the camera's clock is {} min off this computer's, too far to place its recordings", skew_ms.abs() / 60_000);
        }
        Ok(CardClock { zone_ms, skew_ms })
    }

    /// The card's time for this computer's `ms`.
    pub fn to_card(&self, ms: i64) -> NaiveDateTime {
        chrono::DateTime::from_timestamp_millis(ms + self.zone_ms + self.skew_ms).unwrap_or_default().naive_utc()
    }

    /// This computer's time (ms) for a card time.
    pub fn to_server_ms(&self, t: NaiveDateTime) -> i64 {
        t.and_utc().timestamp_millis() - self.zone_ms - self.skew_ms
    }
}

/// The card recordings that open or run into [from, to] (this computer's
/// ms), the unfinished one (its end reads 00:00:00 in its name) left out.
pub fn files_for(files: &[CardFile], clock: &CardClock, from_ms: i64, to_ms: i64) -> Vec<CardFile> {
    let mut v: Vec<CardFile> = files
        .iter()
        .filter(|f| !is_unfinished(&f.name) && f.size > 0 && f.size <= MAX_FILE_BYTES && f.end >= f.start)
        .filter(|f| clock.to_server_ms(f.end) > from_ms && clock.to_server_ms(f.start) < to_ms)
        .cloned()
        .collect();
    v.sort_by_key(|f| f.start);
    v
}

/// A card file still being written has no end yet: its name ends 000000.
pub fn is_unfinished(name: &str) -> bool {
    let leaf = name.rsplit('/').next().unwrap_or(name);
    let parts: Vec<&str> = leaf.split('_').collect();
    // RecM03_20261116_033425_000000_...: type, date, start, end, ...
    parts.len() >= 5 && parts[3] == "000000" && parts[2] != "000000"
}

/// Which samples of a card file fall in [lo_ms, hi_ms] (this computer's time),
/// given where the file starts: video from the first keyframe at or after
/// `lo_ms` up to the last picture that ends by `hi_ms`, and the audio over that.
#[derive(Debug, PartialEq)]
pub struct Pick {
    pub video: std::ops::Range<usize>,
    pub audio: std::ops::Range<usize>,
}

fn sample_ms(t: &TrackIndex, i: usize, file_start_ms: f64) -> f64 {
    file_start_ms + t.samples[i].dts as f64 * 1000.0 / t.timescale.max(1) as f64
}

/// Duration of sample `i` in ms (the last one takes the one before's).
fn sample_len_ms(t: &TrackIndex, i: usize) -> f64 {
    let n = t.samples.len();
    let d = if i + 1 < n { t.samples[i + 1].dts.saturating_sub(t.samples[i].dts) } else if i > 0 { t.samples[i].dts.saturating_sub(t.samples[i - 1].dts) } else { 0 };
    d as f64 * 1000.0 / t.timescale.max(1) as f64
}

pub fn pick_samples(idx: &Mp4Index, file_start_ms: i64, lo_ms: i64, hi_ms: i64) -> Option<Pick> {
    let v = idx.video.as_ref()?;
    let fs = file_start_ms as f64;
    let first = (0..v.samples.len()).find(|&i| v.samples[i].sync && sample_ms(v, i, fs) >= lo_ms as f64)?;
    let mut end = first;
    while end < v.samples.len() && sample_ms(v, end, fs) + sample_len_ms(v, end) <= hi_ms as f64 + 0.5 {
        end += 1;
    }
    if end <= first {
        return None;
    }
    // A picture's own time is its decode time here; audio is cut to the same span.
    let (a0, a1) = (sample_ms(v, first, fs), sample_ms(v, end - 1, fs) + sample_len_ms(v, end - 1));
    let audio = match &idx.audio {
        Some(a) => {
            let s = (0..a.samples.len()).find(|&j| sample_ms(a, j, fs) >= a0).unwrap_or(a.samples.len());
            let mut e = s;
            while e < a.samples.len() && sample_ms(a, e, fs) + sample_len_ms(a, e) <= a1 + 0.5 {
                e += 1;
            }
            s..e
        }
        None => 0..0,
    };
    Some(Pick { video: first..end, audio })
}

/// What the camera was asked for and what came back.
#[derive(Debug, Default)]
pub struct Filled {
    pub segments: Vec<SegmentInfo>,
    /// Seconds of video written.
    pub seconds: f64,
    pub bytes: u64,
    /// The hole was not covered all the way (the card ends, a file is still
    /// being written, the size limit was reached).
    pub partial: bool,
}

/// What to fill and how.
pub struct Job<'a> {
    pub gap: Gap,
    pub channel: u32,
    /// Prefer this card stream ("main" or "sub"); the other is used when the
    /// card has nothing of it for this time.
    pub stream: &'a str,
    pub audio: bool,
    /// The camera's recordings folder, and the camera's file-name stem
    /// (`{stem}-card-{ms}`), which is also what marks them as from the card.
    /// The new recordings are written next to it (see [`staging_dir`]) and
    /// moved in by [`commit_piece`] once the caller keeps them.
    pub dir: &'a Path,
    pub stem: &'a str,
    pub segment_seconds: u64,
    /// The longest the work may take (MAX_GAP_TIME).
    pub time_limit: Duration,
}

/// Where a fill writes its recordings until they are kept: a folder beside
/// the camera's recordings folder, which nothing that lists recordings looks
/// in. What is left there (a fill stopped by a crash) is never a recording.
pub fn staging_dir(segments_dir: &Path) -> std::path::PathBuf {
    segments_dir.with_file_name("card-part")
}

/// Delete what a fill left in its staging folder.
pub fn clear_staging(segments_dir: &Path) {
    let d = staging_dir(segments_dir);
    if d.exists() {
        if let Err(e) = std::fs::remove_dir_all(&d) {
            tracing::warn!("could not delete the unfinished card recordings in {}: {e}", d.display());
        }
    }
}

/// Keep a recording written by [`fill`]: date the file by its end (so a
/// later folder walk places it right even before it is indexed) and move it
/// into the recordings folder. Returns it with its new path.
pub fn commit_piece(mut seg: SegmentInfo, segments_dir: &Path) -> std::io::Result<SegmentInfo> {
    let end = std::time::UNIX_EPOCH + Duration::from_millis(seg.end_ms.max(0) as u64);
    std::fs::OpenOptions::new().write(true).open(&seg.path)?.set_modified(end)?;
    let name = seg.path.file_name().ok_or_else(|| std::io::Error::other("no file name"))?.to_owned();
    let to = segments_dir.join(name);
    std::fs::create_dir_all(segments_dir)?;
    std::fs::rename(&seg.path, &to)?;
    seg.path = to;
    Ok(seg)
}

/// Whether this computer's offset from UTC (`offset_at`, seconds, for an
/// epoch ms) is the same at all of `times`: the card's clock is read now, so
/// a hole from before a daylight-saving change would be placed an hour off.
pub fn same_offset(offset_at: impl Fn(i64) -> Option<i32>, times: &[i64]) -> bool {
    let mut it = times.iter().map(|&t| offset_at(t));
    let Some(first @ Some(_)) = it.next() else { return false };
    it.all(|o| o == first)
}

/// This computer's offset from UTC (seconds) at `ms`.
pub fn local_offset(ms: i64) -> Option<i32> {
    use chrono::{Offset, TimeZone};
    chrono::Local.timestamp_millis_opt(ms).single().map(|t| t.offset().fix().local_minus_utc())
}

/// What one hole may still take.
struct Budget {
    bytes_left: u64,
    deadline: std::time::Instant,
}

impl Budget {
    fn out_of_time(&self) -> bool {
        std::time::Instant::now() >= self.deadline
    }

    fn spend(&mut self, n: u64) {
        self.bytes_left = self.bytes_left.saturating_sub(n);
    }
}

/// The hole's time or byte allowance ran out during a download.
#[derive(Debug)]
struct OutOfBudget;

impl std::fmt::Display for OutOfBudget {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("the time allowed for this hole ran out")
    }
}

impl std::error::Error for OutOfBudget {}

/// Why nothing was filled.
#[derive(Debug)]
pub enum Outcome {
    /// Done (maybe in part); see [`Filled`].
    Filled(Filled),
    /// The card has nothing for this time.
    NothingOnCard(String),
}

/// The camera's clock against this computer's. The camera tells its time to
/// the second, cut off (never rounded), so a single reading is up to a second
/// behind. Reading until the second changes catches it at the start of a
/// second, to within one round trip.
pub fn measure_clock(client: &mut ReolinkClient) -> Result<CardClock> {
    let first = client.camera_clock()?;
    let mut last = (first, chrono::Utc::now().timestamp_millis());
    for _ in 0..14 {
        std::thread::sleep(Duration::from_millis(80));
        let t0 = chrono::Utc::now().timestamp_millis();
        let t = client.camera_clock()?;
        let t1 = chrono::Utc::now().timestamp_millis();
        if t != first {
            // It turned over somewhere in the last 80 ms plus this round trip: take the middle.
            last = (t, t0 - 40 + (t1 - t0) / 4);
            return CardClock::from_reading(last.0, last.1);
        }
        last = (t, t1);
    }
    // The camera's seconds never changed (a stuck clock?): the cut-off reading, shifted half a second.
    CardClock::from_reading(last.0, last.1 - 500)
}

/// `stop()` is asked between steps: true ends the work early (the camera was
/// turned off, the server is stopping). The recordings come back in the
/// staging folder; on an error or a stop nothing is left there.
pub fn fill(client: &mut ReolinkClient, job: &Job, stop: &dyn Fn() -> bool) -> Result<Outcome> {
    clear_staging(job.dir);
    let r = fill_staged(client, job, stop);
    if !matches!(r, Ok(Outcome::Filled(_))) {
        clear_staging(job.dir);
    }
    r
}

fn fill_staged(client: &mut ReolinkClient, job: &Job, stop: &dyn Fn() -> bool) -> Result<Outcome> {
    let mut budget = Budget { bytes_left: MAX_GAP_BYTES, deadline: std::time::Instant::now() + job.time_limit };
    let clock = measure_clock(client)?;
    let (from, to) = (job.gap.from_ms, job.gap.to_ms);
    // A little room each side: files are listed by their own times.
    let (a, b) = (clock.to_card(from - 5_000), clock.to_card(to + 5_000));
    let other = if job.stream == "main" { "sub" } else { "main" };
    let mut listed = Vec::new();
    for s in [job.stream, other] {
        let l = client.search(job.channel, s, a, b)?;
        let l = files_for(&l, &clock, from, to);
        if !l.is_empty() {
            listed = l;
            break;
        }
    }
    if listed.is_empty() {
        return Ok(Outcome::NothingOnCard("the card has no recording for this time".into()));
    }
    let sub = listed[0].stream == "sub";
    let mut out = Filled::default();
    if listed.len() > MAX_FILES_PER_GAP {
        listed.truncate(MAX_FILES_PER_GAP);
        out.partial = true;
    }
    let mut run: Option<Run> = None;
    let mut last_end_ms = from;
    for (n, f) in listed.iter().enumerate() {
        if stop() {
            bail!("stopped");
        }
        if budget.bytes_left == 0 || budget.out_of_time() {
            out.partial = true;
            break;
        }
        let file_start_ms = clock.to_server_ms(f.start);
        // A file that starts after where the last one ended leaves a seam.
        if run.as_ref().is_some() && (file_start_ms as f64) > last_end_ms as f64 + SEAM_MS {
            finish_run(&mut run, &mut out)?;
        }
        // Where this file takes over from the one before (neighboring card
        // files overlap by about a second: the later one starts on a keyframe,
        // so the earlier one is cut where it begins), and where it hands over.
        let lo = if run.is_some() { (last_end_ms - 200).max(from) } else { last_end_ms.max(from) };
        let hi = listed.get(n + 1).map_or(to, |next| to.min(clock.to_server_ms(next.start).max(file_start_ms)));
        let idx = match read_index(client, f, &mut budget) {
            Ok(i) => i,
            Err(e) => {
                // (The name is the camera's: shown escaped.)
                tracing::debug!("card file {:?}: {e:#}", f.name);
                out.partial = true;
                continue;
            }
        };
        let Some(pick) = pick_samples(&idx, file_start_ms, lo, hi) else { continue };
        let (end, more) = feed_file(client, f, &idx, &pick, file_start_ms, job, sub, &mut run, &mut out, &mut budget, stop)?;
        if let Some(e) = end {
            last_end_ms = e;
        }
        if !more {
            out.partial = true;
            break;
        }
    }
    finish_run(&mut run, &mut out)?;
    if out.segments.is_empty() {
        return Ok(Outcome::NothingOnCard("the card's recordings for this time could not be used".into()));
    }
    if (last_end_ms as f64) < to as f64 - 15_000.0 {
        out.partial = true;
    }
    Ok(Outcome::Filled(out))
}

/// The sample tables of a card file, from the first bytes of it; checked to
/// stay inside the file. What is read counts toward the hole's allowance.
fn read_index(client: &mut ReolinkClient, f: &CardFile, budget: &mut Budget) -> Result<Mp4Index> {
    let mut want = 96 * 1024u64;
    let mut head = Vec::new();
    for _ in 0..4 {
        if budget.out_of_time() || budget.bytes_left < want.min(f.size) {
            bail!("the allowance for this hole ran out");
        }
        let to = want.min(f.size).saturating_sub(1);
        let before = client.bytes_read();
        head = client.download(&f.name, Some((0, to))).context("reading the start of the recording")?;
        // (All that came: a device that ignores Range sends the whole file.)
        budget.spend(client.bytes_read() - before);
        match mp4read::find_moov(&head)? {
            MoovAt::Found { offset, size } => {
                let end = offset.checked_add(size).filter(|&e| e <= f.size).ok_or_else(|| anyhow!("the recording's index runs past its end"))?;
                if end <= head.len() as u64 {
                    let idx = mp4read::parse_moov(&head[offset as usize..end as usize])?;
                    idx.check_within(f.size)?;
                    return Ok(idx);
                }
                want = end;
            }
            MoovAt::NeedMore(at) => want = at.saturating_add(64 * 1024).max(want.saturating_mul(2)),
        }
        if want > 2 * mp4read::MAX_MOOV_BYTES || (head.len() as u64) >= f.size {
            break;
        }
    }
    bail!("the recording's index is too large or missing ({} bytes read)", head.len())
}

/// The byte stretches [lo, hi) to fetch for these samples: neighbors
/// joined, each stretch at most MAX_SPAN, all together at most
/// MAX_GROUP_FETCH (more is a damaged or made-up index).
fn plan_ranges(refs: &[&SampleRef]) -> Result<Vec<(u64, u64)>> {
    let mut v: Vec<(u64, u64)> = refs.iter().map(|s| (s.offset, s.offset.saturating_add(s.size as u64))).collect();
    v.sort_unstable();
    let mut out: Vec<(u64, u64)> = Vec::new();
    for (a, b) in v {
        if let Some(last) = out.last_mut() {
            if a <= last.1.saturating_add(MERGE_GAP) && b.max(last.1) - last.0 <= MAX_SPAN {
                last.1 = last.1.max(b);
                continue;
            }
        }
        out.push((a, b));
    }
    let total = out.iter().fold(0u64, |t, r| t.saturating_add(r.1 - r.0));
    if total > MAX_GROUP_FETCH {
        bail!("the recording's index asks for {total} bytes at once");
    }
    Ok(out)
}

/// One recording being written: a run of continuous card footage.
struct Run {
    writer: SegmentWriter,
    /// Server time (ms, fractional) of tick 0.
    start_ms: f64,
    /// Video ticks (90 kHz) per second of file time; audio uses its own rate.
    audio_rate: u32,
}

fn finish_run(run: &mut Option<Run>, out: &mut Filled) -> Result<()> {
    if let Some(mut r) = run.take() {
        r.writer.finish().context("finishing the recording")?;
        out.segments.extend(r.writer.take_closed());
    }
    Ok(())
}

/// Fetch the picked samples in groups and write them. Returns where the
/// footage written ends (this computer's ms), and false when the hole's
/// allowance ran out (what was written is kept).
#[allow(clippy::too_many_arguments)]
fn feed_file(
    client: &mut ReolinkClient,
    f: &CardFile,
    idx: &Mp4Index,
    pick: &Pick,
    file_start_ms: i64,
    job: &Job,
    sub: bool,
    run: &mut Option<Run>,
    out: &mut Filled,
    budget: &mut Budget,
    stop: &dyn Fn() -> bool,
) -> Result<(Option<i64>, bool)> {
    let v = idx.video.as_ref().unwrap();
    let fs = file_start_ms as f64;
    let mut end_ms = None;
    let mut i = pick.video.start;
    while i < pick.video.end {
        if stop() {
            bail!("stopped");
        }
        if budget.out_of_time() {
            return Ok((end_ms, false));
        }
        // A group of pictures, up to GROUP_BYTES (at least one).
        let mut j = i;
        let mut bytes = 0u64;
        while j < pick.video.end && (j == i || bytes + v.samples[j].size as u64 <= GROUP_BYTES) {
            bytes += v.samples[j].size as u64;
            j += 1;
        }
        // Its audio: from the group's first picture to the next one's.
        let t0 = sample_ms(v, i, fs);
        let t1 = if j < pick.video.end { sample_ms(v, j, fs) } else { f64::INFINITY };
        let audio: Vec<usize> = match (&idx.audio, job.audio) {
            (Some(a), true) => pick.audio.clone().filter(|&k| sample_ms(a, k, fs) >= t0 && sample_ms(a, k, fs) < t1).collect(),
            _ => Vec::new(),
        };
        let mut refs: Vec<&SampleRef> = v.samples[i..j].iter().collect();
        if let Some(a) = &idx.audio {
            refs.extend(audio.iter().map(|&k| &a.samples[k]));
        }
        let ranges = plan_ranges(&refs)?;
        let want = ranges.iter().map(|r| r.1 - r.0).sum::<u64>();
        if want > budget.bytes_left {
            return Ok((end_ms, false));
        }
        let mut bufs: Vec<(u64, Vec<u8>)> = Vec::with_capacity(ranges.len());
        for &(lo, hi) in &ranges {
            match fetch(client, &f.name, lo, hi, budget, stop) {
                Ok(b) => bufs.push((lo, b)),
                Err(e) if e.is::<OutOfBudget>() => return Ok((end_ms, false)),
                Err(e) => return Err(e),
            }
        }
        out.bytes += bufs.iter().map(|b| b.1.len() as u64).sum::<u64>();
        let slice = |s: &SampleRef| -> Result<Vec<u8>> {
            bufs.iter()
                .find(|(lo, b)| s.offset >= *lo && s.offset - lo + s.size as u64 <= b.len() as u64)
                .map(|(lo, b)| {
                    let a = (s.offset - lo) as usize;
                    b[a..a + s.size as usize].to_vec()
                })
                .ok_or_else(|| anyhow!("the camera's answer was short"))
        };
        if run.is_none() {
            let start_ms = sample_ms(v, i, fs);
            let mut tracks = vec![TrackInfo {
                kind: TrackKind::Video,
                timescale: 90_000,
                sample_entry: v.sample_entry.clone(),
                width: v.width,
                height: v.height,
            }];
            let mut audio_rate = 0;
            if let (Some(a), true) = (&idx.audio, job.audio) {
                tracks.push(TrackInfo { kind: TrackKind::Audio, timescale: a.timescale, sample_entry: a.sample_entry.clone(), width: 0, height: 0 });
                audio_rate = a.timescale;
            }
            let prefix = if sub { format!("{}-sub-card-{}", job.stem, start_ms.round() as i64) } else { format!("{}-card-{}", job.stem, start_ms.round() as i64) };
            let writer = SegmentWriter::new_recorded_at(&staging_dir(job.dir), &prefix, tracks, job.segment_seconds, start_ms / 1000.0).context("starting the recording file")?;
            *run = Some(Run { writer, start_ms, audio_rate });
        }
        let r = run.as_mut().unwrap();
        // Written in time order, audio between the pictures.
        let mut ai = 0;
        for k in i..j {
            let s = &v.samples[k];
            let t = sample_ms(v, k, fs);
            while ai < audio.len() && sample_ms(idx.audio.as_ref().unwrap(), audio[ai], fs) <= t {
                let sa = &idx.audio.as_ref().unwrap().samples[audio[ai]];
                let ta = sample_ms(idx.audio.as_ref().unwrap(), audio[ai], fs);
                let ticks = ((ta - r.start_ms) * r.audio_rate as f64 / 1000.0).round() as i64;
                if ticks >= 0 {
                    r.writer.push_audio(ticks, slice(sa)?);
                }
                ai += 1;
            }
            let ticks = ((t - r.start_ms) * 90.0).round() as i64;
            let cts = (s.cts_offset as i64 * 90_000 / v.timescale.max(1) as i64) as i32;
            r.writer.push_video_cts(ticks, cts, slice(s)?, s.sync).context("writing the recording")?;
        }
        while ai < audio.len() {
            let a = idx.audio.as_ref().unwrap();
            let ticks = ((sample_ms(a, audio[ai], fs) - r.start_ms) * r.audio_rate as f64 / 1000.0).round() as i64;
            if ticks >= 0 {
                r.writer.push_audio(ticks, slice(&a.samples[audio[ai]])?);
            }
            ai += 1;
        }
        out.seconds += (sample_ms(v, j - 1, fs) + sample_len_ms(v, j - 1) - t0) / 1000.0;
        end_ms = Some((sample_ms(v, j - 1, fs) + sample_len_ms(v, j - 1)).round() as i64);
        i = j;
    }
    Ok((end_ms, true))
}

/// Bytes [lo, hi) of a card file, in gentle requests, each retried once.
/// Fails with [`OutOfBudget`] when the hole's time runs out first.
fn fetch(client: &mut ReolinkClient, name: &str, lo: u64, hi: u64, budget: &mut Budget, stop: &dyn Fn() -> bool) -> Result<Vec<u8>> {
    // (Grown as the bytes come: the size asked for is the camera's word.)
    let mut buf = Vec::new();
    let mut at = lo;
    while at < hi {
        if stop() {
            bail!("stopped");
        }
        if budget.out_of_time() {
            return Err(OutOfBudget.into());
        }
        let to = at.saturating_add(REQUEST_BYTES).min(hi) - 1;
        let before = client.bytes_read();
        let part = match client.download(name, Some((at, to))) {
            Ok(p) => p,
            Err(_) => {
                std::thread::sleep(Duration::from_secs(2));
                client.download(name, Some((at, to)))?
            }
        };
        budget.spend(client.bytes_read() - before);
        if (part.len() as u64) < (to - at + 1).min(MIN_PROGRESS) {
            bail!("the camera sent too little ({} bytes)", part.len());
        }
        at += part.len() as u64;
        buf.extend_from_slice(&part);
        std::thread::sleep(PACE);
    }
    Ok(buf)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ingest::mp4read::tests::trak;

    fn dt(s: &str) -> NaiveDateTime {
        NaiveDateTime::parse_from_str(s, "%Y-%m-%d %H:%M:%S").unwrap()
    }

    #[test]
    fn gaps_are_the_holes_between_recordings() {
        let s = |a: i64, b: i64| (a * 1000, b * 1000);
        // Recorded 0-100, hole 100-160, recorded 160-300 (an overlap with the
        // one before, and a 5 s seam), 305-400, then a 3 h hole, then 11000-.
        let spans = [s(160, 300), s(0, 100), s(250, 280), s(305, 400), s(11_200, 11_300)];
        let g = find_gaps(&spans, 0, MIN_GAP_MS, MAX_GAP_MS);
        assert_eq!(g, vec![Gap { from_ms: 100_000, to_ms: 160_000 }], "the 3 h hole is too long and the 5 s seam too short");
        // Holes before the window are not looked at; a window start inside one skips it.
        assert!(find_gaps(&spans, 101_000, MIN_GAP_MS, MAX_GAP_MS).is_empty());
        // Nothing before the first or after the last recording is a hole.
        assert!(find_gaps(&[s(10, 20)], 0, MIN_GAP_MS, MAX_GAP_MS).is_empty());
        assert!(find_gaps(&[], 0, MIN_GAP_MS, MAX_GAP_MS).is_empty());
        // A recording that runs through the hole closes it.
        assert!(find_gaps(&[s(0, 100), s(50, 400), s(160, 200)], 0, MIN_GAP_MS, MAX_GAP_MS).is_empty());
    }

    #[test]
    fn the_card_clock_is_split_into_zone_and_error() {
        // Camera in UTC-7, clock 12 s ahead; this computer says 17:00:00 UTC.
        let now = dt("2026-11-16 17:00:00").and_utc().timestamp_millis();
        let c = CardClock::from_reading(dt("2026-11-16 10:00:12"), now).unwrap();
        assert_eq!((c.zone_ms, c.skew_ms), (-7 * 3_600_000, 12_000));
        // Round trip, and a known instant: card time 10:05:12 is 17:05:00 here.
        let t = dt("2026-11-16 10:05:12");
        assert_eq!(c.to_server_ms(t), dt("2026-11-16 17:05:00").and_utc().timestamp_millis());
        assert_eq!(c.to_card(c.to_server_ms(t)), t);
        // A half-hour zone, a clock running behind.
        let c = CardClock::from_reading(dt("2026-11-16 22:29:20"), now).unwrap();
        assert_eq!((c.zone_ms, c.skew_ms), (5 * 3_600_000 + 1_800_000, -40_000));
        // Too far off to trust.
        assert!(CardClock::from_reading(dt("2026-11-16 10:09:00"), now).is_err());
    }

    #[test]
    fn card_files_are_chosen_by_time_and_unfinished_ones_left_out() {
        let clock = CardClock { zone_ms: -7 * 3_600_000, skew_ms: 0 };
        let f = |n: &str, a: &str, b: &str| CardFile { name: n.into(), start: dt(a), end: dt(b), size: 1, stream: "sub".into() };
        let files = vec![
            f("Mp4Record/2026-11-16/RecS03_20261116_030000_030159_a_b.mp4", "2026-11-16 03:00:00", "2026-11-16 03:01:59"),
            f("Mp4Record/2026-11-16/RecS03_20261116_030200_030359_a_b.mp4", "2026-11-16 03:02:00", "2026-11-16 03:03:59"),
            f("Mp4Record/2026-11-16/RecS03_20261116_030400_000000_a_0.mp4", "2026-11-16 03:04:00", "2026-11-16 03:05:30"),
        ];
        let from = clock.to_server_ms(dt("2026-11-16 03:01:00"));
        let to = clock.to_server_ms(dt("2026-11-16 03:02:30"));
        let got = files_for(&files, &clock, from, to);
        assert_eq!(got.len(), 2);
        // The open one is out even where it overlaps.
        let to = clock.to_server_ms(dt("2026-11-16 03:05:00"));
        assert_eq!(files_for(&files, &clock, from, to).len(), 2);
        assert!(is_unfinished("/mnt/sda/X-Cam/Mp4Record/2026-11-16/RecS05_20261116_033425_000000_0_280_1E0_x_0.mp4"));
        assert!(!is_unfinished("Mp4Record/2026-11-16/RecS03_20261116_025823_030023_8B28808_2EF850.mp4"));
        // A file that began at midnight sharp has a start of 000000, not an open end.
        assert!(!is_unfinished("Mp4Record/2026-11-16/RecM03_20261116_000000_000159_7D28838_4B2E80E.mp4"));
    }

    /// Video: 20 pictures a second, a keyframe every 2 s (40 pictures), 10 s
    /// long; audio: 1024-sample frames at 16 kHz.
    fn index(sizes: usize) -> Mp4Index {
        let vs = vec![100u32; 200];
        let moov = {
            let v = trak(b"vide", b"avc1", 90_000, &vs, 5, 1000, 4500, Some(&(0..5).map(|k| k * 40 + 1).collect::<Vec<u32>>()), None);
            let a = trak(b"soun", b"mp4a", 16_000, &vec![20u32; 157], 5, 100_000, 1024, None, None);
            let mut b = ((v.len() + a.len() + 8) as u32).to_be_bytes().to_vec();
            b.extend_from_slice(b"moov");
            b.extend_from_slice(&v);
            b.extend_from_slice(&a);
            b
        };
        let _ = sizes;
        mp4read::parse_moov(&moov).unwrap()
    }

    #[test]
    fn only_the_part_inside_the_hole_is_picked_from_a_keyframe() {
        let idx = index(0);
        let start = 1_000_000i64; // the file begins at this ms
        // Hole from 3.0 s to 7.3 s into the file: the keyframe at 4 s, pictures up to 7.3 s.
        let p = pick_samples(&idx, start, start + 3_000, start + 7_300).unwrap();
        assert_eq!(p.video, 80..146, "from the keyframe at 4 s to the last whole picture ending by 7.3 s");
        // Audio covers the same span, no more.
        let a = idx.audio.as_ref().unwrap();
        let (s, e) = (p.audio.start, p.audio.end);
        assert!(a.secs(s) >= 4.0 - 1e-9 && a.secs(s) < 4.0 + 0.065);
        assert!(a.secs(e - 1) + 1024.0 / 16_000.0 <= 7.3 + 0.001);
        // The whole file; then a hole in the last stretch with no keyframe left in it.
        assert_eq!(pick_samples(&idx, start, start - 500, start + 60_000).unwrap().video, 0..200);
        assert!(pick_samples(&idx, start, start + 8_100, start + 9_900).is_none(), "the last keyframe, at 8 s, is before the hole");
    }

    #[test]
    fn a_hole_starting_between_keyframes_waits_for_the_next_one() {
        let idx = index(0);
        let p = pick_samples(&idx, 0, 4_050, 10_000).unwrap();
        assert_eq!(p.video.start, 120, "the keyframe at 6 s, none at 4.05 s or later before it");
        assert_eq!(pick_samples(&idx, 0, 0, 1_000).unwrap().video, 0..20);
    }

    // The keyframe test above: keyframes are at 0, 2, 4, 6, 8 s (pictures
    // 0, 40, 80, 120, 160), so from 8.1 s the one at 8 s is already past.
    #[test]
    fn nothing_after_the_last_keyframe_is_taken() {
        let idx = index(0);
        assert!(pick_samples(&idx, 0, 8_100, 10_000).is_none());
        assert_eq!(pick_samples(&idx, 0, 8_000, 10_000).unwrap().video, 160..200);
    }

    // ---- a fake camera ----

    fn bx(kind: &[u8; 4], payload: &[u8]) -> Vec<u8> {
        let mut v = ((payload.len() + 8) as u32).to_be_bytes().to_vec();
        v.extend_from_slice(kind);
        v.extend_from_slice(payload);
        v
    }

    /// A card recording of 10 s: 200 pictures (a keyframe every 40, picture
    /// `i` is 100 bytes of the value `i`) and 157 audio frames.
    fn card_mp4() -> Vec<u8> {
        card_mp4_at(0)
    }

    /// [`card_mp4`] with its index pointing `extra` bytes further on.
    fn card_mp4_at(extra: u32) -> Vec<u8> {
        let keys: Vec<u32> = (0..5).map(|k| k * 40 + 1).collect();
        let build = |off: u32| {
            let v = trak(b"vide", b"avc1", 90_000, &[100u32; 200], 5, off, 4500, Some(&keys), None);
            let a = trak(b"soun", b"mp4a", 16_000, &[20u32; 157], 5, off + 20_000, 1024, None, None);
            bx(b"moov", &[v, a].concat())
        };
        let ftyp = bx(b"ftyp", &[0; 16]);
        let moov_len = build(0).len();
        let moov = build((ftyp.len() + moov_len + 8) as u32 + extra);
        let mut data: Vec<u8> = (0..200u32).flat_map(|i| vec![i as u8; 100]).collect();
        data.extend(vec![0xEE; 157 * 20]);
        [ftyp, moov, bx(b"mdat", &data)].concat()
    }

    struct Fake {
        port: u16,
        requests: std::sync::Arc<std::sync::Mutex<Vec<String>>>,
    }

    /// How a fake camera misbehaves.
    #[derive(Clone, Default)]
    struct Quirks {
        /// The size its search gives for each file (default: the real one).
        size: Option<String>,
        /// Ranges that do not start at 0 get one byte.
        trickle: bool,
        /// After this many downloads, it stops answering them.
        drop_after: Option<usize>,
        /// Ignores Range and sends the whole file (a Reolink hub does).
        whole_only: bool,
    }

    /// A camera's web interface on a local port: sign-in, clock, search and
    /// download (with byte ranges) of card files that all hold `mp4`.
    fn fake_camera(zone_ms: i64, skew_ms: i64, files: Vec<(NaiveDateTime, NaiveDateTime, String)>, mp4: Vec<u8>) -> Fake {
        fake_camera_with(zone_ms, skew_ms, files, mp4, Quirks::default())
    }

    fn fake_camera_with(zone_ms: i64, skew_ms: i64, files: Vec<(NaiveDateTime, NaiveDateTime, String)>, mp4: Vec<u8>, q: Quirks) -> Fake {
        use std::io::{Read, Write};
        let mut downloads = 0usize;
        let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = l.local_addr().unwrap().port();
        let requests = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let log = requests.clone();
        std::thread::spawn(move || {
            for s in l.incoming() {
                let Ok(mut s) = s else { break };
                let mut buf = Vec::new();
                let mut tmp = [0u8; 4096];
                let (head_end, body_len) = loop {
                    let n = s.read(&mut tmp).unwrap_or(0);
                    if n == 0 {
                        break (0, 0);
                    }
                    buf.extend_from_slice(&tmp[..n]);
                    if let Some(p) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
                        let head = String::from_utf8_lossy(&buf[..p]).to_lowercase();
                        let cl = head.lines().find_map(|l| l.strip_prefix("content-length:")).and_then(|v| v.trim().parse::<usize>().ok()).unwrap_or(0);
                        break (p + 4, cl);
                    }
                };
                if head_end == 0 {
                    continue;
                }
                while buf.len() < head_end + body_len {
                    let n = s.read(&mut tmp).unwrap_or(0);
                    if n == 0 {
                        break;
                    }
                    buf.extend_from_slice(&tmp[..n]);
                }
                let head = String::from_utf8_lossy(&buf[..head_end]).to_string();
                let body = String::from_utf8_lossy(&buf[head_end..]).to_string();
                let line = head.lines().next().unwrap_or("").to_string();
                log.lock().unwrap().push(line.clone());
                let range = head.lines().find_map(|l| l.to_lowercase().strip_prefix("range: bytes=").map(|v| v.trim().to_string()));
                let mut reply = |status: &str, ctype: &str, extra: &str, b: &[u8]| {
                    let mut out = format!("HTTP/1.1 {status}\r\nContent-Type: {ctype}\r\n{extra}Content-Length: {}\r\nConnection: close\r\n\r\n", b.len()).into_bytes();
                    out.extend_from_slice(b);
                    let _ = s.write_all(&out);
                };
                let tj = |t: NaiveDateTime| {
                    use chrono::{Datelike, Timelike};
                    serde_json::json!({"year": t.year(), "mon": t.month(), "day": t.day(), "hour": t.hour(), "min": t.minute(), "sec": t.second()})
                };
                let json = |v: serde_json::Value| v.to_string().into_bytes();
                if line.contains("cmd=Login") {
                    reply("200 OK", "application/json", "", &json(serde_json::json!([{"cmd": "Login", "code": 0, "value": {"Token": {"leaseTime": 3600, "name": "tok"}}}])));
                } else if line.contains("cmd=GetTime") {
                    let now = chrono::DateTime::from_timestamp_millis(chrono::Utc::now().timestamp_millis() + zone_ms + skew_ms).unwrap().naive_utc();
                    reply("200 OK", "application/json", "", &json(serde_json::json!([{"cmd": "GetTime", "code": 0, "value": {"Time": tj(now)}}])));
                } else if line.contains("cmd=Search") {
                    let main = !body.contains("\"sub\"");
                    let list: Vec<serde_json::Value> = if main {
                        files.iter().map(|(a, b, n)| serde_json::json!({"name": n, "StartTime": tj(*a), "EndTime": tj(*b), "size": q.size.clone().unwrap_or_else(|| mp4.len().to_string()), "type": "main"})).collect()
                    } else {
                        Vec::new()
                    };
                    reply("200 OK", "application/json", "", &json(serde_json::json!([{"cmd": "Search", "code": 0, "value": {"SearchResult": {"File": list, "channel": 0}}}])));
                } else if line.contains("cmd=Download") {
                    downloads += 1;
                    if q.drop_after.is_some_and(|n| downloads > n) {
                        continue; // hangs up without an answer
                    }
                    match range.filter(|_| !q.whole_only).and_then(|r| r.split_once('-').map(|(a, b)| (a.parse::<usize>().unwrap(), b.parse::<usize>().unwrap()))) {
                        Some((a, b)) => {
                            let b = b.min(mp4.len() - 1);
                            let b = if q.trickle && a > 0 { a } else { b };
                            reply("206 Partial Content", "application/octet-stream", &format!("Content-Range: bytes {a}-{b}/{}\r\n", mp4.len()), &mp4[a..=b]);
                        }
                        None => reply("200 OK", "application/octet-stream", "", &mp4),
                    }
                } else {
                    reply("404 Not Found", "text/plain", "", b"");
                }
            }
        });
        Fake { port, requests }
    }

    #[test]
    fn a_hole_is_filled_from_two_overlapping_card_files() {
        let zone = -7 * 3_600_000i64;
        let skew = 4_000i64;
        let t0 = (chrono::Utc::now().timestamp() - 600) * 1000; // whole second, ten minutes ago
        let card = |ms: i64| chrono::DateTime::from_timestamp_millis(ms + zone + skew).unwrap().naive_utc();
        // Two 10 s files; the second starts 9 s into the first (the camera overlaps them by a second).
        let files = vec![
            (card(t0), card(t0 + 10_000), "Mp4Record/2026-11-16/RecM03_20261116_010000_010010_x_y.mp4".to_string()),
            (card(t0 + 9_000), card(t0 + 19_000), "Mp4Record/2026-11-16/RecM03_20261116_010009_010019_x_y.mp4".to_string()),
        ];
        let cam = fake_camera(zone, skew, files, card_mp4());
        let dir = std::env::temp_dir().join(format!("amber-cardfill-{}", std::process::id())).join("segments");
        let _ = std::fs::remove_dir_all(dir.parent().unwrap());
        let mut client = ReolinkClient::with_base(&format!("http://127.0.0.1:{}", cam.port), "u", "p");
        // The hole: 3 s into the first file to half a second into the second.
        let gap = Gap { from_ms: t0 + 3_000, to_ms: t0 + 9_500 };
        let job = Job { gap, channel: 0, stream: "main", audio: true, dir: &dir, stem: "porch", segment_seconds: 60, time_limit: MAX_GAP_TIME };
        let out = match fill(&mut client, &job, &|| false).unwrap() {
            Outcome::Filled(f) => f,
            Outcome::NothingOnCard(w) => panic!("nothing: {w}"),
        };
        // 4 s to 9 s of the first file, then the second from its start to 0.5 s: one recording.
        assert_eq!(out.segments.len(), 1, "{:?}", out.segments);
        let seg = &out.segments[0];
        assert!((seg.start_ms - (t0 + 4_000)).abs() <= 250, "starts at the keyframe: {}", seg.start_ms - t0);
        assert!((seg.end_ms - (t0 + 9_500)).abs() <= 250, "ends where the hole does: {}", seg.end_ms - t0);
        assert!((106..=114).contains(&seg.video_frames), "{} pictures (the clock reading is good to ~100 ms)", seg.video_frames);
        assert!(seg.start_ms >= gap.from_ms - 250 && seg.end_ms <= gap.to_ms + 250, "never outside the hole");
        assert!(!out.partial);
        assert!((out.seconds - 5.5).abs() < 0.1, "{}", out.seconds);
        let name = seg.path.file_name().unwrap().to_string_lossy().to_string();
        assert!(name.starts_with("porch-card-") && crate::engine::card_fill::is_card_piece(&name), "{name}");
        // The pictures from the keyframe on are in the file, the ones before it are not.
        let bytes = std::fs::read(&seg.path).unwrap();
        let has = |v: u8| bytes.windows(100).any(|w| w.iter().all(|&b| b == v));
        assert!(has(80) && has(179) && has(5) && !has(79) && !has(40) && !has(20), "only the wanted pictures");
        // Only part of each file was asked for, in byte ranges.
        let asked = cam.requests.lock().unwrap().iter().filter(|l| l.contains("cmd=Download")).count();
        assert!(asked >= 4, "{asked} downloads");
        let _ = std::fs::remove_dir_all(dir.parent().unwrap());
    }

    /// A device that ignores Range (a Reolink hub) and sends the whole file:
    /// filled all the same, each file downloaded once.
    #[test]
    fn a_device_that_sends_whole_files_fills_too() {
        let zone = -7 * 3_600_000i64;
        let t0 = (chrono::Utc::now().timestamp() - 600) * 1000;
        let card = |ms: i64| chrono::DateTime::from_timestamp_millis(ms + zone).unwrap().naive_utc();
        let files = vec![(card(t0), card(t0 + 10_000), "Mp4Record/2026-11-22/RecM05_20261122_101721_101731_0_1400_x.mp4".to_string())];
        let cam = fake_camera_with(zone, 0, files, card_mp4(), Quirks { whole_only: true, ..Default::default() });
        let dir = std::env::temp_dir().join(format!("amber-cardfill-whole-{}", std::process::id())).join("segments");
        let _ = std::fs::remove_dir_all(dir.parent().unwrap());
        let mut client = ReolinkClient::with_base(&format!("http://127.0.0.1:{}", cam.port), "u", "p");
        let job = Job { gap: Gap { from_ms: t0, to_ms: t0 + 10_000 }, channel: 0, stream: "main", audio: true, dir: &dir, stem: "side", segment_seconds: 60, time_limit: MAX_GAP_TIME };
        let out = match fill(&mut client, &job, &|| false).unwrap() {
            Outcome::Filled(f) => f,
            Outcome::NothingOnCard(w) => panic!("nothing: {w}"),
        };
        assert_eq!(out.segments.len(), 1);
        assert!(out.segments[0].video_frames >= 150, "{}", out.segments[0].video_frames);
        let asked = cam.requests.lock().unwrap().iter().filter(|l| l.contains("cmd=Download")).count();
        assert_eq!(asked, 1, "downloaded once, the parts served from it");
        let _ = std::fs::remove_dir_all(dir.parent().unwrap());
    }

    #[test]
    fn a_camera_with_nothing_on_its_card_for_the_time_says_so() {
        let cam = fake_camera(3_600_000, 0, Vec::new(), card_mp4());
        let dir = std::env::temp_dir().join(format!("amber-cardfill-none-{}", std::process::id())).join("segments");
        let mut client = ReolinkClient::with_base(&format!("http://127.0.0.1:{}", cam.port), "u", "p");
        let t0 = chrono::Utc::now().timestamp_millis() - 600_000;
        let job = Job { gap: Gap { from_ms: t0, to_ms: t0 + 60_000 }, channel: 0, stream: "main", audio: false, dir: &dir, stem: "porch", segment_seconds: 60, time_limit: MAX_GAP_TIME };
        assert!(matches!(fill(&mut client, &job, &|| false).unwrap(), Outcome::NothingOnCard(_)));
        assert!(!dir.exists(), "nothing written");
    }

    #[test]
    fn a_camera_turned_off_meanwhile_stops_the_work() {
        let zone = 0i64;
        let t0 = (chrono::Utc::now().timestamp() - 600) * 1000;
        let card = |ms: i64| chrono::DateTime::from_timestamp_millis(ms + zone).unwrap().naive_utc();
        let cam = fake_camera(zone, 0, vec![(card(t0), card(t0 + 10_000), "Mp4Record/a.mp4".to_string())], card_mp4());
        let dir = std::env::temp_dir().join(format!("amber-cardfill-stop-{}", std::process::id())).join("segments");
        let mut client = ReolinkClient::with_base(&format!("http://127.0.0.1:{}", cam.port), "u", "p");
        let job = Job { gap: Gap { from_ms: t0, to_ms: t0 + 10_000 }, channel: 0, stream: "main", audio: false, dir: &dir, stem: "porch", segment_seconds: 60, time_limit: MAX_GAP_TIME };
        assert!(fill(&mut client, &job, &|| true).is_err());
        let _ = std::fs::remove_dir_all(dir.parent().unwrap());
    }

    /// Two 10 s card files back to back, ten minutes ago; the hole is the
    /// whole of them.
    fn two_files(zone: i64) -> (i64, Vec<(NaiveDateTime, NaiveDateTime, String)>) {
        let t0 = (chrono::Utc::now().timestamp() - 600) * 1000;
        let card = |ms: i64| chrono::DateTime::from_timestamp_millis(ms + zone).unwrap().naive_utc();
        let files = vec![
            (card(t0), card(t0 + 10_000), "Mp4Record/2026-11-16/RecM03_20261116_010000_010010_x_y.mp4".to_string()),
            (card(t0 + 10_000), card(t0 + 20_000), "Mp4Record/2026-11-16/RecM03_20261116_010010_010020_x_y.mp4".to_string()),
        ];
        (t0, files)
    }

    fn scratch(name: &str) -> std::path::PathBuf {
        let d = std::env::temp_dir().join(format!("amber-cardfill-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        d.join("segments")
    }

    /// Nothing at all under the camera's folder but empty folders.
    fn no_files_under(dir: &Path) -> bool {
        fn walk(d: &Path) -> usize {
            std::fs::read_dir(d).map(|r| r.flatten().map(|e| if e.path().is_dir() { walk(&e.path()) } else { 1 }).sum()).unwrap_or(0)
        }
        walk(dir.parent().unwrap()) == 0
    }

    fn tag() -> u64 {
        use std::sync::atomic::{AtomicU64, Ordering};
        static N: AtomicU64 = AtomicU64::new(0);
        N.fetch_add(1, Ordering::Relaxed)
    }

    #[test]
    fn a_fill_cut_short_leaves_no_files_behind() {
        // The camera stops answering after the first file's index and video.
        let (t0, files) = two_files(0);
        let cam = fake_camera_with(0, 0, files, card_mp4(), Quirks { drop_after: Some(3), ..Default::default() });
        let dir = scratch("drop");
        let mut client = ReolinkClient::with_base(&format!("http://127.0.0.1:{}", cam.port), "u", "p");
        let job = Job { gap: Gap { from_ms: t0, to_ms: t0 + 20_000 }, channel: 0, stream: "main", audio: true, dir: &dir, stem: "porch", segment_seconds: 60, time_limit: MAX_GAP_TIME };
        assert!(fill(&mut client, &job, &|| false).is_err());
        assert!(no_files_under(&dir), "a half-written recording was left");
        assert!(!staging_dir(&dir).exists());
        // Stopped part way (the server stopping): nothing left either.
        let (t0, files) = two_files(0);
        let cam = fake_camera(0, 0, files, card_mp4());
        let mut client = ReolinkClient::with_base(&format!("http://127.0.0.1:{}", cam.port), "u", "p");
        let job = Job { gap: Gap { from_ms: t0, to_ms: t0 + 20_000 }, ..job };
        let asked = std::cell::Cell::new(0);
        let stop = || {
            asked.set(asked.get() + 1);
            asked.get() > 4
        };
        assert!(fill(&mut client, &job, &stop).is_err());
        assert!(no_files_under(&dir), "a half-written recording was left");
        // What an earlier fill left (the server was killed) goes at the next one.
        std::fs::create_dir_all(staging_dir(&dir)).unwrap();
        std::fs::write(staging_dir(&dir).join("porch-card-1794148581609-seg000000.mp4"), b"x").unwrap();
        let cam = fake_camera(0, 0, Vec::new(), card_mp4());
        let mut client = ReolinkClient::with_base(&format!("http://127.0.0.1:{}", cam.port), "u", "p");
        assert!(matches!(fill(&mut client, &job, &|| false).unwrap(), Outcome::NothingOnCard(_)));
        assert!(no_files_under(&dir));
        let _ = std::fs::remove_dir_all(dir.parent().unwrap());
    }

    #[test]
    fn a_kept_piece_is_dated_by_its_end_and_moved_in() {
        let dir = scratch("commit");
        let staged = staging_dir(&dir);
        std::fs::create_dir_all(&staged).unwrap();
        let p = staged.join("porch-card-1794148581609-seg000000.mp4");
        std::fs::write(&p, b"video").unwrap();
        let end_ms = 1_794_148_700_123i64;
        let seg = SegmentInfo { path: p.clone(), start_ms: 1_794_148_581_609, end_ms, bytes: 5, video_frames: 1, fragments: 1 };
        let kept = commit_piece(seg, &dir).unwrap();
        assert_eq!(kept.path, dir.join("porch-card-1794148581609-seg000000.mp4"));
        assert!(!p.exists() && kept.path.exists());
        let m = std::fs::metadata(&kept.path).unwrap().modified().unwrap();
        let ms = m.duration_since(std::time::UNIX_EPOCH).unwrap().as_millis() as i64;
        assert!((ms - end_ms).abs() < 2_000, "dated {ms}, ends {end_ms}");
        let _ = std::fs::remove_dir_all(dir.parent().unwrap());
    }

    #[test]
    fn a_daylight_saving_change_since_the_hole_skips_it() {
        // Offset -6 h before ms 1_000_000, -5 h from then on.
        let tz = |t: i64| Some(if t < 1_000_000 { -21_600 } else { -18_000 });
        // Hole before the change, filled after it: skipped.
        assert!(!same_offset(tz, &[100_000, 200_000, 2_000_000]));
        // Hole across the change: skipped.
        assert!(!same_offset(tz, &[900_000, 1_100_000, 2_000_000]));
        // Hole and now both after it, or both before it: filled.
        assert!(same_offset(tz, &[1_100_000, 1_200_000, 2_000_000]));
        assert!(same_offset(tz, &[100_000, 200_000, 900_000]));
        // A time this computer cannot place: skipped.
        assert!(!same_offset(|_| None, &[1, 2, 3]));
        assert!(local_offset(chrono::Utc::now().timestamp_millis()).is_some());
    }

    #[test]
    fn samples_far_apart_are_fetched_apart_and_too_much_is_refused() {
        let s = |offset: u64, size: u32| SampleRef { offset, size, dts: 0, cts_offset: 0, sync: false };
        let (a, b, c) = (s(1000, 500), s(1600, 400), s(3_000_000_000, 20));
        assert_eq!(plan_ranges(&[&c, &a, &b]).unwrap(), vec![(1000, 2000), (3_000_000_000, 3_000_000_020)]);
        let big: Vec<SampleRef> = (0..5).map(|k| s(k * 100_000_000, 15 << 20)).collect();
        assert!(plan_ranges(&big.iter().collect::<Vec<_>>()).is_err());
    }

    #[test]
    fn a_made_up_or_damaged_card_is_given_up_on_quickly() {
        let started = std::time::Instant::now();
        let run = |mp4: Vec<u8>, q: Quirks, limit: Duration| {
            let (t0, files) = two_files(0);
            let cam = fake_camera_with(0, 0, files, mp4, q);
            let dir = scratch(&format!("bad{}", tag()));
            let mut client = ReolinkClient::with_base(&format!("http://127.0.0.1:{}", cam.port), "u", "p");
            let job = Job { gap: Gap { from_ms: t0, to_ms: t0 + 20_000 }, channel: 0, stream: "main", audio: true, dir: &dir, stem: "porch", segment_seconds: 60, time_limit: limit };
            let r = fill(&mut client, &job, &|| false);
            assert!(no_files_under(&dir));
            let downloads = cam.requests.lock().unwrap().iter().filter(|l| l.contains("cmd=Download")).count();
            let _ = std::fs::remove_dir_all(dir.parent().unwrap());
            (r, downloads)
        };
        // A file size no card holds: not used, nothing downloaded.
        let (r, n) = run(card_mp4(), Quirks { size: Some(u64::MAX.to_string()), ..Default::default() }, MAX_GAP_TIME);
        assert!(matches!(r, Ok(Outcome::NothingOnCard(_))) && n == 0, "{r:?} {n}");
        // An index that points past the end of the file: not fetched from.
        let (r, n) = run(card_mp4_at(50_000_000), Quirks::default(), MAX_GAP_TIME);
        assert!(matches!(r, Ok(Outcome::NothingOnCard(_))) && n == 2, "{r:?} {n}: only the two indexes");
        // A camera that trickles a byte at a time: given up on at once.
        let (r, n) = run(card_mp4(), Quirks { trickle: true, ..Default::default() }, MAX_GAP_TIME);
        assert!(r.is_err() && n <= 4, "{r:?} {n}");
        // No time left: nothing asked.
        let (r, n) = run(card_mp4(), Quirks::default(), Duration::ZERO);
        assert!(matches!(r, Ok(Outcome::NothingOnCard(_))) && n == 0, "{r:?} {n}");
        assert!(started.elapsed() < Duration::from_secs(60));
    }
}
