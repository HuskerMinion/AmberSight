//! The siren's sound on Reolink cameras: send a custom sound, switch the
//! siren between it and the camera's built-in sound, and see which is used.
//! Always over Reolink port 9000 (Baichuan), also for a camera whose other
//! controls go over its web API (the web API has no such setting).
//!
//! The sound is what the official Reolink app sends (decoded from its
//! traffic and tried on the lab's cameras, 2026-11-27): IMA ADPCM, 4 bits,
//! mono, 16 kHz, the first sample in the low nibble, coded in one go (the
//! coder's state never starts again: starting it again every 512, 1024 or
//! 2048 bytes made the camera's sound clip), at most 39936 bytes (4.99 s,
//! the app's longest). The camera plays it as loud as the samples are, so
//! it is brought to near full scale first.
//!
//! An upload is untrusted: at most `MAX_UPLOAD` bytes, read by ffmpeg from a
//! file of its own in a private folder, with only the file protocol and a
//! short list of audio formats allowed (nothing in a file can make ffmpeg
//! open anything else), within a time limit; the folder goes afterwards.

use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use serde_json::{json, Value};
use tracing::warn;

use super::baichuan_drv::{BaichuanDriver, SoundState};
use super::{askable, audit, battery_behind_hub, ensure, handle, is_hub, know_device, not_done, refuse, siren_busy, target, Caller, Refused, Target};
use crate::engine::Engine;
use crate::ingest::baichuan::talk::Ima;

/// The sample rate the camera takes.
pub const RATE: usize = 16_000;
/// The longest sound, in samples (39936 bytes, as the app's longest).
pub const MAX_SAMPLES: usize = 79_872;
pub const MAX_BYTES: usize = MAX_SAMPLES / 2;
/// The shortest sound (a quarter second).
pub const MIN_SAMPLES: usize = RATE / 4;
/// Sizes are a whole number of these bytes (the app's are: 39936 = 39 x 1024).
pub const BLOCK_BYTES: usize = 1024;
/// The biggest upload taken (refused before anything reads it).
pub const MAX_UPLOAD: usize = 10 * 1024 * 1024;
/// ffmpeg may take this long to read an upload.
const DECODE_TIME: Duration = Duration::from_secs(20);
/// The loudest sample after bringing it up: about -1 dBFS.
const PEAK: f64 = 29_204.0;
/// Quieter than this all through (about -60 dBFS): no sound.
const SILENT: i32 = 33;
/// The formats ffmpeg may read an upload as (its demuxer names).
const FORMATS: &str = "mov,mp4,m4a,3gp,matroska,webm,wav,w64,ogg,mp3,aac,flac,caf,aiff";

/// One camera's sound is sent at most once in this long, and switched at
/// most once in `PICK_GAP` (counted from when the last one was done).
#[cfg(not(test))]
pub const SEND_GAP: Duration = Duration::from_secs(10);
#[cfg(test)]
pub const SEND_GAP: Duration = Duration::from_secs(3);
pub const PICK_GAP: Duration = Duration::from_secs(2);
/// One account sends at most this many sounds in `SEND_WINDOW`.
pub const SENDS_PER_USER: usize = 10;
pub const SEND_WINDOW: Duration = Duration::from_secs(10 * 60);
/// At most this many uploads are read by ffmpeg at once (server-wide).
const DECODES_AT_ONCE: usize = 2;
/// A private folder left by an earlier run (a crash) goes once this old.
const STALE_DIR: Duration = Duration::from_secs(60 * 60);
/// A camera's state read this recently is not read again.
const READ_FRESH: Duration = Duration::from_secs(5);

// ------------------------------------------------------------------ coding

/// `samples` (16 kHz mono) as the camera's ADPCM: coded in one go from a
/// state of zero, the first sample of each byte in the low nibble, padded
/// with silence to a whole number of `BLOCK_BYTES` (cut at `MAX_BYTES`).
pub fn encode(samples: &[i16]) -> Vec<u8> {
    let n = samples.len().min(MAX_SAMPLES);
    let bytes = n.div_ceil(2).div_ceil(BLOCK_BYTES) * BLOCK_BYTES;
    let bytes = bytes.min(MAX_BYTES);
    let mut st = Ima::default();
    let mut out = Vec::with_capacity(bytes);
    let at = |i: usize| if i < n { samples[i] } else { 0 };
    for b in 0..bytes {
        let lo = st.encode(at(2 * b));
        let hi = st.encode(at(2 * b + 1));
        out.push(lo | (hi << 4));
    }
    out
}

#[cfg(test)]
/// The camera's ADPCM back to samples (in one go, low nibble first).
pub fn decode(adpcm: &[u8]) -> Vec<i16> {
    let mut st = Ima::default();
    let mut out = Vec::with_capacity(adpcm.len() * 2);
    for &b in adpcm {
        out.push(st.decode(b & 15));
        out.push(st.decode(b >> 4));
    }
    out
}

/// Bring the loudest sample to about -1 dBFS. Err: no sound at all.
pub fn normalize(samples: &mut [i16]) -> Result<(), String> {
    let peak = samples.iter().map(|&s| (s as i32).abs()).max().unwrap_or(0);
    if peak <= SILENT {
        return Err("the sound is silent (or nearly): record or pick a louder one".into());
    }
    let gain = PEAK / f64::from(peak);
    for s in samples.iter_mut() {
        *s = (f64::from(*s) * gain).round().clamp(-32768.0, 32767.0) as i16;
    }
    Ok(())
}

/// Samples (16 kHz mono, from ffmpeg) to what is sent to the camera.
pub fn prepare(mut samples: Vec<i16>) -> Result<Vec<u8>, String> {
    samples.truncate(MAX_SAMPLES);
    if samples.len() < MIN_SAMPLES {
        return Err("the sound is too short: at least a quarter of a second".into());
    }
    normalize(&mut samples)?;
    Ok(encode(&samples))
}

/// A private folder for one upload, gone when dropped.
struct TempDir(PathBuf);

const DIR_PREFIX: &str = "ambersight-siren-";

/// Remove the upload folders in `dir` older than STALE_DIR (only folders of
/// that name, never a link).
fn remove_stale(dir: &Path) {
    let Ok(rd) = std::fs::read_dir(dir) else { return };
    for d in rd.flatten() {
        if !d.file_name().to_string_lossy().starts_with(DIR_PREFIX) {
            continue;
        }
        let Ok(m) = std::fs::symlink_metadata(d.path()) else { continue };
        if m.is_dir() && m.modified().ok().and_then(|t| t.elapsed().ok()).is_some_and(|a| a > STALE_DIR) {
            let _ = std::fs::remove_dir_all(d.path());
        }
    }
}

/// An upload being read by ffmpeg (DECODES_AT_ONCE at most, counted in `DECODING`).
struct DecodeSlot(&'static Mutex<usize>);

static DECODING: Mutex<usize> = Mutex::new(0);

impl DecodeSlot {
    fn take(count: &'static Mutex<usize>, most: usize) -> Option<DecodeSlot> {
        let mut n = count.lock().unwrap_or_else(|e| e.into_inner());
        (*n < most).then(|| {
            *n += 1;
            DecodeSlot(count)
        })
    }
}

impl Drop for DecodeSlot {
    fn drop(&mut self) {
        *self.0.lock().unwrap_or_else(|e| e.into_inner()) -= 1;
    }
}

impl TempDir {
    fn new() -> std::io::Result<TempDir> {
        use rand::Rng;
        static CLEANED: std::sync::Once = std::sync::Once::new();
        CLEANED.call_once(|| remove_stale(&std::env::temp_dir()));
        let name = format!("{DIR_PREFIX}{}-{:016x}", std::process::id(), rand::thread_rng().gen::<u64>());
        let p = std::env::temp_dir().join(name);
        #[cfg_attr(not(unix), allow(unused_mut))]
        let mut b = std::fs::DirBuilder::new();
        #[cfg(unix)]
        {
            use std::os::unix::fs::DirBuilderExt;
            b.mode(0o700);
        }
        b.create(&p)?;
        Ok(TempDir(p))
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// The arguments for ffmpeg to read `input` (only as a file, only as one of
/// `FORMATS`) to at most 5 s of 16 kHz mono 16-bit samples in `output`.
fn ffmpeg_args(input: &Path, output: &Path) -> Vec<std::ffi::OsString> {
    let mut a: Vec<std::ffi::OsString> = ["-nostdin", "-hide_banner", "-v", "error", "-y", "-protocol_whitelist", "file", "-format_whitelist", FORMATS, "-i"].iter().map(Into::into).collect();
    a.push(input.into());
    for x in ["-map", "0:a:0", "-vn", "-sn", "-dn", "-ac", "1", "-ar", "16000", "-t", "5", "-f", "s16le", "-c:a", "pcm_s16le"] {
        a.push(x.into());
    }
    a.push(output.into());
    a
}

/// An uploaded sound file (any format ffmpeg reads from `FORMATS`) as 16 kHz
/// mono samples, at most 5 s.
pub fn decode_upload(data: &[u8]) -> Result<Vec<i16>, String> {
    if data.is_empty() {
        return Err("no sound file was sent".into());
    }
    if data.len() > MAX_UPLOAD {
        return Err(format!("the sound file is too big: at most {} MB", MAX_UPLOAD / (1024 * 1024)));
    }
    let dir = TempDir::new().map_err(|e| format!("could not make a folder for the sound: {e}"))?;
    let input = dir.0.join("upload");
    let output = dir.0.join("sound.raw");
    let log = dir.0.join("ffmpeg.log");
    std::fs::write(&input, data).map_err(|e| format!("could not keep the sound for reading: {e}"))?;
    let log_file = std::fs::File::create(&log).map_err(|e| format!("could not make ffmpeg's log: {e}"))?;
    let mut cmd = crate::child::ffmpeg();
    cmd.args(ffmpeg_args(&input, &output)).stdin(std::process::Stdio::null()).stdout(std::process::Stdio::null()).stderr(log_file);
    let mut child = cmd.spawn().map_err(|e| format!("could not run ffmpeg: {e}"))?;
    crate::child::adopt(&child);
    let until = Instant::now() + DECODE_TIME;
    let status = loop {
        match child.try_wait() {
            Ok(Some(st)) => break st,
            Ok(None) if Instant::now() >= until => {
                let _ = child.kill();
                let _ = child.wait();
                return Err("reading the sound file took too long".into());
            }
            Ok(None) => std::thread::sleep(Duration::from_millis(20)),
            Err(e) => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(format!("waiting for ffmpeg: {e}"));
            }
        }
    };
    let said = std::fs::read_to_string(&log).unwrap_or_default();
    if !status.success() {
        if said.contains("matches no streams") {
            return Err("the file has no sound in it".into());
        }
        // (ffmpeg's words name the private folder: only the log gets them.)
        warn!("siren sound: ffmpeg could not read an upload: {}", said.lines().last().unwrap_or("").replace(&dir.0.display().to_string(), "<upload>"));
        return Err("the file could not be read as sound: send a WAV, MP3, M4A/AAC, WebM, Ogg or FLAC file".into());
    }
    let raw = std::fs::read(&output).map_err(|e| format!("could not read the decoded sound: {e}"))?;
    let samples: Vec<i16> = raw.chunks_exact(2).map(|b| i16::from_le_bytes([b[0], b[1]])).collect();
    if samples.is_empty() {
        return Err("the file has no sound in it".into());
    }
    Ok(samples)
}

// ------------------------------------------------------------- the camera

/// What was last read of each camera's siren sound (by camera name, with
/// the target key: a camera reached another way is read again).
#[derive(Clone)]
struct Known {
    key: String,
    state: SoundState,
    at: Instant,
}

fn known() -> &'static Mutex<std::collections::HashMap<String, Known>> {
    static K: std::sync::OnceLock<Mutex<std::collections::HashMap<String, Known>>> = std::sync::OnceLock::new();
    K.get_or_init(Default::default)
}

/// The last send and switch per camera, and the sends per account.
#[derive(Default)]
struct Limits {
    sent: std::collections::HashMap<String, Instant>,
    picked: std::collections::HashMap<String, Instant>,
    by_user: std::collections::VecDeque<(String, Instant)>,
}

fn limits() -> &'static Mutex<Limits> {
    static L: std::sync::OnceLock<Mutex<Limits>> = std::sync::OnceLock::new();
    L.get_or_init(Default::default)
}

fn driver(t: &Target) -> Result<BaichuanDriver, Refused> {
    BaichuanDriver::for_sound(&t.bc_base, &t.user, &t.pass, t.channel, is_hub(t)).map_err(|e| refuse(500, format!("{e:#}")))
}

/// Words added to a refusal from a battery camera behind a Home Hub.
pub const HUB_SIREN_HINT: &str = "This might be because the camera's siren is switched off: switch it on in the Reolink app, then try again.";

fn unsupported(why: &str) -> Value {
    json!({ "supported": false, "why": why })
}

/// The state's JSON (the same shape for GET, upload and switch).
fn state_json(t: &Target, s: &SoundState, error: Option<String>) -> Value {
    let using = match s.custom {
        Some(true) => "custom",
        Some(false) => "built_in",
        None => "unknown",
    };
    json!({
        "supported": s.takes,
        "why": (!s.takes).then_some("This camera does not take a sound of its own for its siren."),
        "using": using,
        // (Behind a hub the camera's answer is not reliable: not given.)
        "custom_bytes": s.stored_bytes,
        "hub": is_hub(t),
        "max_seconds": MAX_SAMPLES as f64 / RATE as f64,
        "max_upload_bytes": MAX_UPLOAD,
        "error": error,
    })
}

/// The camera may be asked about its siren's sound: a Reolink camera with a
/// siren, reached now (a battery camera only through a hub that reports it
/// awake). Err inside: it cannot, with the words why (as JSON for the GET).
/// The bool: a battery camera behind a Home Hub (a refusal may be its siren
/// setting).
fn reach(engine: &Engine, id: usize) -> Result<Result<(String, Target, bool), Value>, Refused> {
    let (h, cfg) = handle(engine, id)?;
    let t = match target(&cfg) {
        Ok(t) => t,
        Err(why) => return Ok(Err(unsupported(&why))),
    };
    know_device(engine, &cfg.name, &t, &h);
    if t.battery && !is_hub(&t) {
        return Ok(Err(unsupported("The siren's sound is set for battery cameras behind a Reolink Home Hub only.")));
    }
    if !askable(&h, &t) {
        let why = if t.battery { "This battery camera is asleep: open its live view to wake it, then try again." } else { "The hub does not report this camera awake: open its live view to wake it, then try again." };
        return Ok(Err(json!({ "supported": false, "asleep": true, "why": why })));
    }
    let ab = ensure(engine, &cfg.name, &t, &h, false);
    match ab {
        Some(a) if a.siren => {
            let hint = battery_behind_hub(&h, &t);
            Ok(Ok((cfg.name, t, hint)))
        }
        Some(_) => Ok(Err(unsupported("This camera has no siren."))),
        None => {
            let e = engine.controls.with(&cfg.name, |c| c.error.clone()).unwrap_or_else(|| "no answer".into());
            Ok(Err(unsupported(&format!("Could not find out what this camera has: {e}"))))
        }
    }
}

fn remember(name: &str, t: &Target, s: &SoundState) {
    known().lock().unwrap().insert(name.to_string(), Known { key: t.key.clone(), state: s.clone(), at: Instant::now() });
}

fn last_known(name: &str, t: &Target) -> Option<Known> {
    known().lock().unwrap().get(name).filter(|k| k.key == t.key).cloned()
}

/// Read the state from the camera (or what was read a moment ago).
fn read_state(name: &str, t: &Target, fresh_ok: bool) -> Result<SoundState, String> {
    if fresh_ok {
        if let Some(k) = last_known(name, t).filter(|k| k.at.elapsed() < READ_FRESH) {
            return Ok(k.state);
        }
    }
    let d = driver(t).map_err(|r| r.msg)?;
    match d.sound_state() {
        Ok(s) => {
            remember(name, t, &s);
            Ok(s)
        }
        Err(e) => Err(crate::engine::redact_url(format!("{e:#}"))),
    }
}

/// `GET`: whether the camera takes a sound of its own, and which it uses.
pub fn view(engine: &Engine, id: usize) -> Result<Value, Refused> {
    let (name, t, _) = match reach(engine, id)? {
        Ok(x) => x,
        Err(v) => return Ok(v),
    };
    match read_state(&name, &t, true) {
        Ok(s) => Ok(state_json(&t, &s, None)),
        Err(e) => match last_known(&name, &t) {
            // (Shown as last read, with the problem.)
            Some(k) => Ok(state_json(&t, &k.state, Some(e))),
            None => Ok(json!({ "supported": false, "why": format!("The camera did not answer on Reolink port 9000 (the siren's sound is set that way): {e}"), "error": e })),
        },
    }
}

/// Refused while the siren sounds or its stop is due: a send goes ahead of
/// everything on the camera's session, so the stop would wait behind it.
fn not_while_sounding(engine: &Engine, name: &str) -> Result<(), Refused> {
    if siren_busy(engine, name) {
        return Err(refuse(409, "The siren is sounding: try again when it has stopped."));
    }
    Ok(())
}

/// The camera to change: reached, its siren quiet, and it takes a sound (as just read).
fn for_change(engine: &Engine, id: usize) -> Result<(String, Target, SoundState, bool), Refused> {
    let (name, t, hint) = reach(engine, id)?.map_err(|v| refuse(409, v["why"].as_str().unwrap_or("not supported")))?;
    not_while_sounding(engine, &name)?;
    let s = read_state(&name, &t, true).map_err(|e| refuse(502, format!("The camera did not answer on Reolink port 9000: {e}")))?;
    if !s.takes {
        return Err(refuse(409, "This camera does not take a sound of its own for its siren."));
    }
    Ok((name, t, s, hint))
}

/// The words for a failed change (a refusal from a battery camera behind a
/// Home Hub, `hint`, says what may help).
fn failed(hint: bool, what: &str, e: &anyhow::Error) -> String {
    let words = crate::engine::redact_url(format!("{e:#}"));
    if hint && not_done(e) {
        format!("{what}: {words}. {HUB_SIREN_HINT}")
    } else {
        format!("{what}: {words}.")
    }
}

/// `POST`: send a sound file (decoded and checked first), then switch the siren to it.
pub fn upload(engine: &Engine, id: usize, who: &Caller, data: &[u8]) -> Result<Value, Refused> {
    // (Refused before the file is read, too, while the siren sounds.)
    let (_, cfg) = handle(engine, id)?;
    not_while_sounding(engine, &cfg.name)?;
    // (The file is read before anything is asked of the camera.)
    let slot = DecodeSlot::take(&DECODING, DECODES_AT_ONCE).ok_or_else(|| refuse(429, "Other siren sounds are being read now: try again in a moment."))?;
    let adpcm = decode_upload(data).and_then(prepare).map_err(|e| refuse(400, format!("The sound was not sent: {e}.")))?;
    drop(slot);
    let (name, t, _, hint) = for_change(engine, id)?;
    {
        let mut l = limits().lock().unwrap();
        if l.sent.get(&t.place).is_some_and(|at| at.elapsed() < SEND_GAP) {
            return Err(refuse(429, format!("A sound was sent to this camera less than {} seconds ago.", SEND_GAP.as_secs())));
        }
        while l.by_user.front().is_some_and(|(_, at)| at.elapsed() > SEND_WINDOW) {
            l.by_user.pop_front();
        }
        if l.by_user.iter().filter(|(u, _)| *u == who.user).count() >= SENDS_PER_USER {
            return Err(refuse(429, format!("Too many siren sounds sent from this account: at most {SENDS_PER_USER} in {} minutes.", SEND_WINDOW.as_secs() / 60)));
        }
        l.sent.insert(t.place.clone(), Instant::now());
        l.by_user.push_back((who.user.clone(), Instant::now()));
    }
    let secs = (adpcm.len() * 2) as f64 / RATE as f64;
    let op = engine.controls.op(&t.place);
    let _g = op.lock().unwrap();
    // (Again with the action lock: a siren started meanwhile is seen.)
    not_while_sounding(engine, &name)?;
    let d = driver(&t)?;
    let r = d.sound_send(&adpcm);
    // (The gap counts from when it is done: a slow camera does not shorten it.)
    limits().lock().unwrap().sent.insert(t.place.clone(), Instant::now());
    let what = format!("siren sound: custom sound sent ({secs:.1} s, {} bytes) and switched to", adpcm.len());
    let rr = r.as_ref().map(|_| ()).map_err(|e| crate::engine::redact_url(format!("{e:#}")));
    audit(engine, who, &name, &what, &rr);
    if let Err(e) = r {
        return Err(refuse(502, failed(hint, "The sound was not sent", &e)));
    }
    drop(_g);
    let s = read_state(&name, &t, false).map_err(|e| refuse(502, format!("The sound was sent, but the camera's state could not be read back: {e}")))?;
    Ok(state_json(&t, &s, None))
}

/// `PUT`: the siren's custom sound (`custom`) or its built-in one.
pub fn pick(engine: &Engine, id: usize, who: &Caller, custom: bool) -> Result<Value, Refused> {
    let (name, t, _, hint) = for_change(engine, id)?;
    {
        let mut l = limits().lock().unwrap();
        if l.picked.get(&t.place).is_some_and(|at| at.elapsed() < PICK_GAP) {
            return Err(refuse(429, "This camera's siren sound was switched a moment ago."));
        }
        l.picked.insert(t.place.clone(), Instant::now());
    }
    let op = engine.controls.op(&t.place);
    let _g = op.lock().unwrap();
    not_while_sounding(engine, &name)?;
    let d = driver(&t)?;
    let r = d.sound_pick(custom);
    limits().lock().unwrap().picked.insert(t.place.clone(), Instant::now());
    let what = if custom { "siren sound: switched to the custom sound" } else { "siren sound: switched to the built-in sound" };
    let rr = r.as_ref().map(|_| ()).map_err(|e| crate::engine::redact_url(format!("{e:#}")));
    audit(engine, who, &name, what, &rr);
    if let Err(e) = r {
        return Err(refuse(502, failed(hint, "The siren sound did not change", &e)));
    }
    drop(_g);
    let s = read_state(&name, &t, false).map_err(|e| refuse(502, format!("The siren sound changed, but the camera's state could not be read back: {e}")))?;
    Ok(state_json(&t, &s, None))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tone(n: usize, amp: f64, f: f64) -> Vec<i16> {
        (0..n).map(|i| (amp * (i as f64 * f * std::f64::consts::TAU / RATE as f64).sin()) as i16).collect()
    }

    #[test]
    fn coding_round_trips_close_to_the_input() {
        let x = tone(16_000, 20_000.0, 440.0);
        let a = encode(&x);
        assert_eq!(a.len(), 8192, "a whole number of blocks");
        let y = decode(&a);
        assert_eq!(y.len(), 16_384);
        // (After the coder settles: within a few percent of full scale.)
        let err = x.iter().zip(&y).skip(200).map(|(a, b)| (i32::from(*a) - i32::from(*b)).abs()).max().unwrap();
        assert!(err < 2_000, "worst error {err}");
        assert!(y[16_200..].iter().all(|s| s.abs() < 300), "padded with silence (once the coder settles)");
    }

    #[test]
    fn the_coder_never_starts_again() {
        // The same samples coded in one go and in two halves: only one go
        // carries its state across 1024 bytes (2048 samples).
        let x = tone(8192, 25_000.0, 1000.0);
        let whole = encode(&x);
        let mut st = Ima::default();
        let mut first: Vec<u8> = Vec::new();
        for p in x[..2048].chunks(2) {
            let lo = st.encode(p[0]);
            let hi = st.encode(p[1]);
            first.push(lo | (hi << 4));
        }
        assert_eq!(&whole[..1024], &first[..], "the start is the plain IMA coding");
        let restarted = encode(&x[2048..4096]);
        assert_ne!(&whole[1024..2048], &restarted[..1024], "the state goes on past 1024 bytes");
        // And decoding in one go gives the tone back past the block edge.
        let y = decode(&whole);
        let err = x[2040..2100].iter().zip(&y[2040..2100]).map(|(a, b)| (i32::from(*a) - i32::from(*b)).abs()).max().unwrap();
        assert!(err < 3_000, "no jump at the block edge: {err}");
        // Low nibble first.
        let mut st = Ima::default();
        let c0 = st.encode(x[0]);
        assert_eq!(whole[0] & 15, c0);
    }

    #[test]
    fn sizes_and_loudness() {
        assert_eq!(encode(&vec![100; 10 * RATE]).len(), MAX_BYTES, "cut at the longest");
        assert_eq!(MAX_BYTES, 39_936);
        assert_eq!(encode(&vec![0; 3]).len(), BLOCK_BYTES);
        let mut quiet = tone(4000, 1000.0, 440.0);
        normalize(&mut quiet).unwrap();
        let peak = quiet.iter().map(|s| i32::from(*s).abs()).max().unwrap();
        assert!((28_500..=29_300).contains(&peak), "{peak}");
        assert!(normalize(&mut [0i16; 100]).is_err(), "silence");
        assert!(normalize(&mut [5i16, -20, 30]).is_err(), "nearly silence");
        assert!(prepare(vec![1000; 100]).unwrap_err().contains("too short"));
        assert_eq!(prepare(tone(RATE * 7, 9000.0, 440.0)).unwrap().len(), MAX_BYTES);
    }

    #[test]
    fn ffmpeg_reads_only_a_file_as_audio() {
        let a: Vec<String> = ffmpeg_args(Path::new("/x/in"), Path::new("/x/out")).iter().map(|s| s.to_string_lossy().into_owned()).collect();
        let at = |k: &str| a.iter().position(|x| x == k).unwrap();
        assert_eq!(a[at("-protocol_whitelist") + 1], "file");
        assert!(!a[at("-format_whitelist") + 1].contains("hls") && !a[at("-format_whitelist") + 1].contains("concat"));
        assert!(at("-protocol_whitelist") < at("-i") && at("-format_whitelist") < at("-i"), "input options");
        assert_eq!(a[at("-ar") + 1], "16000");
        assert_eq!(a[at("-ac") + 1], "1");
        assert_eq!(a[at("-t") + 1], "5");
        assert!(a.contains(&"-nostdin".to_string()));
        assert!(decode_upload(&[]).is_err());
        assert!(decode_upload(&vec![0u8; MAX_UPLOAD + 1]).unwrap_err().contains("too big"));
    }

    #[test]
    fn at_most_two_reads_at_once() {
        static N: Mutex<usize> = Mutex::new(0);
        let a = DecodeSlot::take(&N, DECODES_AT_ONCE);
        let b = DecodeSlot::take(&N, DECODES_AT_ONCE);
        assert!(a.is_some() && b.is_some());
        assert!(DecodeSlot::take(&N, DECODES_AT_ONCE).is_none(), "a third waits its turn");
        drop(a);
        assert!(DecodeSlot::take(&N, DECODES_AT_ONCE).is_some(), "given back");
        drop(b);
        assert_eq!(*N.lock().unwrap(), 0);
    }

    #[cfg(unix)]
    #[test]
    fn old_upload_folders_go() {
        let root = std::env::temp_dir().join(format!("ambersight-stale-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        let old = root.join(format!("{DIR_PREFIX}1-old"));
        let new = root.join(format!("{DIR_PREFIX}2-new"));
        let other = root.join("someone-else");
        let file = root.join(format!("{DIR_PREFIX}file"));
        for d in [&old, &new, &other] {
            std::fs::create_dir(d).unwrap();
        }
        std::fs::write(&file, b"x").unwrap();
        let past = std::time::SystemTime::now() - STALE_DIR - Duration::from_secs(60);
        for p in [&old, &other, &file] {
            std::fs::File::open(p).unwrap().set_modified(past).unwrap();
        }
        remove_stale(&root);
        assert!(!old.exists(), "an old upload folder goes");
        assert!(new.exists() && other.exists() && file.exists(), "only old folders of that name");
        let _ = std::fs::remove_dir_all(&root);
    }

    /// With the system's ffmpeg: a WAV is read, a playlist that points
    /// elsewhere and a file with no sound are not.
    #[test]
    fn ffmpeg_decodes_an_upload() {
        if std::process::Command::new(crate::ffmpeg_bin::program()).arg("-version").output().is_err() {
            return;
        }
        // A WAV of 1 s at 8 kHz stereo.
        let mut wav = Vec::new();
        let n = 8000u32;
        wav.extend_from_slice(b"RIFF");
        wav.extend_from_slice(&(36 + n * 4).to_le_bytes());
        wav.extend_from_slice(b"WAVEfmt ");
        wav.extend_from_slice(&16u32.to_le_bytes());
        wav.extend_from_slice(&1u16.to_le_bytes());
        wav.extend_from_slice(&2u16.to_le_bytes());
        wav.extend_from_slice(&8000u32.to_le_bytes());
        wav.extend_from_slice(&(8000u32 * 4).to_le_bytes());
        wav.extend_from_slice(&4u16.to_le_bytes());
        wav.extend_from_slice(&16u16.to_le_bytes());
        wav.extend_from_slice(b"data");
        wav.extend_from_slice(&(n * 4).to_le_bytes());
        for i in 0..n {
            let v = (8000.0 * (f64::from(i) * 440.0 * std::f64::consts::TAU / 8000.0).sin()) as i16;
            wav.extend_from_slice(&v.to_le_bytes());
            wav.extend_from_slice(&v.to_le_bytes());
        }
        let s = decode_upload(&wav).unwrap();
        assert!((15_900..=16_100).contains(&s.len()), "{}", s.len());
        assert!(prepare(s).is_ok());
        let m3u = b"#EXTM3U\n#EXTINF:1,\n/etc/hostname\n";
        assert!(decode_upload(m3u).is_err(), "a playlist is not read");
        assert!(decode_upload(b"not a sound at all, just words").is_err());
    }
}
