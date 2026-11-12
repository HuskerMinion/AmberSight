//! Naming a loud sound: Google's YAMNet (Apache-2.0), which knows the 521
//! sound classes of AudioSet, as ONNX (a straight conversion published at
//! huggingface.co/audiomagic/yamnet-onnx, pinned by revision and checksum).
//! It is downloaded only when sound names are turned on, and runs on this
//! server's processor in a few milliseconds per sound. Its classes are
//! folded into a short list of names people care about.

// Intel Macs have no built-in AI runtime, so most of this goes unused there.
#![cfg_attr(all(target_os = "macos", target_arch = "x86_64"), allow(dead_code))]

use anyhow::{anyhow, bail, Result};
use std::path::{Path, PathBuf};

pub const MODEL_FILE: &str = "yamnet.onnx";
pub const MODEL_URL: &str = "https://huggingface.co/audiomagic/yamnet-onnx/resolve/f25b741c2f0bdc6d7e6db24b5fddda23347dbafd/yamnet.onnx";
pub const MODEL_SHA256: &str = "d3835ffbbd4a1bb3e777f0ca217b5007907f5171dd5d17c4236b95b2af8f908e";
pub const MODEL_BYTES: u64 = 16_093_355;

/// YAMNet listens at 16 kHz.
pub const RATE: u32 = 16_000;

/// A name counts when YAMNet is at least this sure of one of its classes.
pub const MIN_SCORE: f32 = 0.2;

/// The names we use, and the AudioSet classes (YAMNet's class numbers, from
/// its yamnet_class_map.csv) that make up each.
pub const GROUPS: &[(&str, &[usize])] = &[
    // Glass, Shatter.
    ("glass break", &[435, 437]),
    // Dog, Bark, Howl, Bow-wow, Whimper (dog), Canidae.
    ("dog bark", &[69, 70, 72, 73, 75, 117]),
    // Car alarm, Alarm, Siren, Civil defense siren, Smoke detector, Fire
    // alarm, police / ambulance / fire engine sirens.
    ("alarm", &[304, 382, 390, 391, 393, 394, 317, 318, 319]),
    // Shout, Yell, Children shouting, Screaming.
    ("shouting", &[6, 9, 10, 11]),
    // Explosion, Gunshot, Machine gun, Fusillade, Artillery fire, Cap gun,
    // Burst/pop, Boom, Bang, Smash/crash.
    ("bang", &[420, 421, 422, 423, 424, 425, 428, 430, 460, 463]),
    // Doorbell, Ding-dong.
    ("doorbell", &[349, 350]),
    // Knock.
    ("knock", &[353]),
    // Vehicle horn, Toot, Air horn, Train horn.
    ("car horn", &[302, 303, 312, 325]),
    // Crying/sobbing, Baby cry.
    ("crying", &[19, 20]),
];

/// The names a person can choose from.
pub fn names() -> Vec<&'static str> {
    GROUPS.iter().map(|g| g.0).collect()
}

/// Our names for YAMNet's scores (one row of 521 per 0.96 s; the best row
/// counts for each class), surest first.
pub fn name_scores(rows: &[Vec<f32>]) -> Vec<(&'static str, f32)> {
    let best = |i: usize| rows.iter().filter_map(|r| r.get(i)).copied().fold(0.0f32, f32::max);
    let mut out: Vec<(&'static str, f32)> = GROUPS
        .iter()
        .map(|(name, classes)| (*name, classes.iter().map(|&i| best(i)).fold(0.0f32, f32::max)))
        .filter(|(_, s)| *s >= MIN_SCORE)
        .collect();
    out.sort_by(|a, b| b.1.total_cmp(&a.1));
    out
}

pub fn model_path(dir: &Path) -> PathBuf {
    dir.join(MODEL_FILE)
}

pub fn model_present(dir: &Path) -> bool {
    model_path(dir).metadata().is_ok_and(|m| m.len() == MODEL_BYTES)
}

/// Download the model into `dir`, checked against its checksum.
/// `progress` gets (bytes so far, total).
pub fn download(dir: &Path, progress: &dyn Fn(u64, u64)) -> Result<()> {
    use sha2::{Digest, Sha256};
    use std::io::{Read, Write};
    std::fs::create_dir_all(dir)?;
    let dest = model_path(dir);
    if model_present(dir) {
        progress(MODEL_BYTES, MODEL_BYTES);
        return Ok(());
    }
    let agent = ureq::Agent::config_builder().timeout_connect(Some(std::time::Duration::from_secs(20))).timeout_recv_response(Some(std::time::Duration::from_secs(60))).timeout_recv_body(Some(std::time::Duration::from_secs(3600))).build().new_agent();
    let mut resp = agent.get(MODEL_URL).call().map_err(|e| anyhow!("download {MODEL_FILE}: {e}"))?;
    let mut rd = resp.body_mut().with_config().limit(MODEL_BYTES + (1 << 20)).reader();
    let tmp = dest.with_extension("part");
    let mut f = std::fs::File::create(&tmp)?;
    let mut h = Sha256::new();
    let mut buf = vec![0u8; 1 << 16];
    let mut done = 0u64;
    loop {
        let n = rd.read(&mut buf)?;
        if n == 0 {
            break;
        }
        f.write_all(&buf[..n])?;
        h.update(&buf[..n]);
        done += n as u64;
        progress(done.min(MODEL_BYTES), MODEL_BYTES);
    }
    drop(f);
    let got: String = h.finalize().iter().map(|b| format!("{b:02x}")).collect();
    if got != MODEL_SHA256 {
        let _ = std::fs::remove_file(&tmp);
        bail!("{MODEL_FILE} did not match its checksum");
    }
    std::fs::rename(&tmp, &dest)?;
    Ok(())
}

#[cfg(not(all(target_os = "macos", target_arch = "x86_64")))]
pub struct SoundModel {
    session: std::sync::Mutex<ort::session::Session>,
    _env: ort::environment::Environment,
}

#[cfg(not(all(target_os = "macos", target_arch = "x86_64")))]
impl SoundModel {
    pub fn load(dir: &Path) -> Result<Self> {
        let path = model_path(dir);
        if file_sha256(&path)? != MODEL_SHA256 {
            // Removed, so turning sound names off and on downloads it again.
            let _ = std::fs::remove_file(&path);
            bail!("{MODEL_FILE} does not match its checksum; turn sound names off and on to download it again");
        }
        let env = crate::ai::runtime()?;
        let session = ort::session::Session::builder(&env)
            .map_err(|e| anyhow!("AI runtime: {e}"))?
            .with_intra_threads(1)
            .map_err(|e| anyhow!("AI runtime: {e}"))?
            .commit_from_file(&path)
            .map_err(|e| anyhow!("could not load {MODEL_FILE}: {e}"))?;
        Ok(SoundModel { session: std::sync::Mutex::new(session), _env: env })
    }

    /// YAMNet's 521 scores per 0.96 s of `wave` (16 kHz mono, -1..1).
    pub fn scores(&self, wave: &[f32]) -> Result<Vec<Vec<f32>>> {
        let n = wave.len().max(1);
        let mut w = wave.to_vec();
        w.resize(n, 0.0);
        let t = ort::value::Tensor::from_array(ndarray::Array1::from_vec(w)).map_err(|e| anyhow!("sound input: {e}"))?;
        let mut s = self.session.lock().unwrap();
        let input = s.inputs().first().map(|i| i.name().to_string()).unwrap_or_else(|| "waveform".into());
        let out = s.run(ort::inputs![input => t]).map_err(|e| anyhow!("sound names: {e}"))?;
        let a = out[0].try_extract_array::<f32>().map_err(|e| anyhow!("sound output: {e}"))?;
        let v: Vec<f32> = a.iter().copied().collect();
        if v.is_empty() || v.len() % 521 != 0 {
            bail!("unexpected sound output ({} numbers)", v.len());
        }
        Ok(v.chunks(521).map(|c| c.to_vec()).collect())
    }

    /// Our names for a sound (16 kHz mono), surest first.
    pub fn name(&self, wave: &[f32]) -> Result<Vec<(&'static str, f32)>> {
        Ok(name_scores(&self.scores(wave)?))
    }
}

fn file_sha256(p: &Path) -> Result<String> {
    use sha2::{Digest, Sha256};
    use std::io::Read;
    let mut f = std::fs::File::open(p).map_err(|e| anyhow!("open {}: {e}", p.display()))?;
    let mut h = Sha256::new();
    let mut buf = vec![0u8; 1 << 16];
    loop {
        let n = f.read(&mut buf)?;
        if n == 0 {
            break;
        }
        h.update(&buf[..n]);
    }
    Ok(h.finalize().iter().map(|b| format!("{b:02x}")).collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn groups_are_sane() {
        let mut seen = std::collections::HashSet::new();
        for (name, classes) in GROUPS {
            assert!(!classes.is_empty(), "{name}");
            for &c in *classes {
                assert!(c < 521, "{name}: {c}");
                assert!(seen.insert(c), "class {c} is in two groups");
            }
        }
        assert!(names().contains(&"glass break") && names().contains(&"dog bark"));
    }

    #[test]
    fn maps_scores_to_names() {
        let mut a = vec![0.0f32; 521];
        let mut b = vec![0.0f32; 521];
        a[70] = 0.6; // Bark
        b[437] = 0.8; // Shatter
        b[0] = 0.9; // Speech: not one of ours
        a[349] = 0.1; // Doorbell, too unsure
        let got = name_scores(&[a, b]);
        assert_eq!(got.iter().map(|x| x.0).collect::<Vec<_>>(), vec!["glass break", "dog bark"]);
        assert!(name_scores(&[]).is_empty());
    }

    /// With the model at $AMBERSIGHT_YAMNET_DIR: silence is nothing we name,
    /// and the model gives one row per 0.96 s (hop 0.48 s).
    #[cfg(not(all(target_os = "macos", target_arch = "x86_64")))]
    #[test]
    fn model_runs() {
        let Some(dir) = std::env::var_os("AMBERSIGHT_YAMNET_DIR").map(PathBuf::from) else { return };
        let m = SoundModel::load(&dir).unwrap();
        let rows = m.scores(&vec![0.0; 16_000]).unwrap();
        assert_eq!(rows.len(), 2);
        assert!(name_scores(&rows).is_empty());
        // A two-tone chime (like a doorbell) should not be a dog or glass.
        let chime: Vec<f32> = (0..16_000)
            .map(|i| {
                let t = i as f32 / 16_000.0;
                let f = if t < 0.5 { 660.0 } else { 528.0 };
                0.4 * (t * f * std::f32::consts::TAU).sin() * (-(t % 0.5) * 4.0).exp()
            })
            .collect();
        let got = m.name(&chime).unwrap();
        eprintln!("chime -> {got:?}");
        assert!(!got.iter().any(|x| x.0 == "dog bark" || x.0 == "glass break"));
    }
}
