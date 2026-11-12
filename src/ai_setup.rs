//! Getting the built-in AI model: the official YOLOX files, pinned by
//! checksum, downloaded with progress (Settings > Detection & alerts > Objects (AI), or `ambersight ai-model`).

use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};
use std::time::Duration;

use anyhow::{anyhow, bail, Context, Result};
use sha2::{Digest, Sha256};

pub struct ModelInfo {
    pub id: &'static str,
    pub file: &'static str,
    pub sha256: &'static str,
    pub name: &'static str,
    pub about: &'static str,
}

pub const MODELS: &[ModelInfo] = &[
    ModelInfo {
        id: "tiny",
        file: "yolox_tiny.onnx",
        sha256: "427cc366d34e27ff7a03e2899b5e3671425c262ea2291f88bb942bc1cc70b0f7",
        name: "Tiny",
        about: "About 20 MB. Fastest and lightest; can miss small or distant objects.",
    },
    ModelInfo {
        id: "s",
        file: "yolox_s.onnx",
        sha256: "c5c2d13e59ae883e6af3b45daea64af4833a4951c92d116ec270d9ddbe998063",
        name: "Small",
        about: "About 35 MB. More accurate, a little more CPU per check.",
    },
    ModelInfo {
        id: "l",
        file: "yolox_l.onnx",
        sha256: "7860ae79de6c89a3c1eb72ae9a2756c0ccfbe04b7791bb5880afabd97855a411",
        name: "Large",
        about: "About 217 MB. Most accurate; needs a graphics card to keep up with several cameras. About five times the work per check of Small.",
    },
];

const RELEASE: &str = "https://github.com/Megvii-BaseDetection/YOLOX/releases/download/0.1.1rc0";

pub fn find(id: &str) -> Option<&'static ModelInfo> {
    MODELS.iter().find(|m| m.id == id)
}

/// Which model a file is (by its name).
pub fn identify(path: &Path) -> Option<&'static ModelInfo> {
    let f = path.file_name()?.to_str()?;
    MODELS.iter().find(|m| m.file == f)
}

/// A path for config files and messages: forward slashes, no `\\?\` prefix.
pub fn clean_path(p: &Path) -> String {
    let s = p.display().to_string();
    let s = s.strip_prefix(r"\\?\").unwrap_or(&s);
    s.replace('\\', "/")
}

fn sha256_file(p: &Path) -> Result<String> {
    let mut f = std::fs::File::open(p)?;
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

/// Make sure `m` is in `dir` (download it if missing or damaged); returns its
/// path. `progress(bytes, total)` is called while downloading.
pub fn ensure(dir: &Path, m: &ModelInfo, progress: &dyn Fn(u64, Option<u64>)) -> Result<PathBuf> {
    std::fs::create_dir_all(dir).with_context(|| format!("cannot create {}", dir.display()))?;
    let dest = dir.join(m.file);
    if dest.is_file() && sha256_file(&dest)? == m.sha256 {
        return Ok(dest);
    }
    let url = format!("{RELEASE}/{}", m.file);
    let agent = ureq::Agent::config_builder().timeout_connect(Some(Duration::from_secs(20))).timeout_recv_response(Some(Duration::from_secs(60))).timeout_recv_body(Some(Duration::from_secs(3600))).build().new_agent();
    let mut resp = agent.get(&url).call().map_err(|e| anyhow!("download failed: {e}"))?;
    let total = resp.headers().get("content-length").and_then(|v| v.to_str().ok()).and_then(|v| v.parse::<u64>().ok());
    let part = dest.with_extension("part");
    let mut out = std::fs::File::create(&part).with_context(|| format!("cannot write {}", part.display()))?;
    let mut reader = resp.body_mut().with_config().limit(300 << 20).reader();
    let mut h = Sha256::new();
    let mut buf = vec![0u8; 1 << 16];
    let mut got = 0u64;
    loop {
        let n = reader.read(&mut buf).map_err(|e| anyhow!("download interrupted: {e}"))?;
        if n == 0 {
            break;
        }
        out.write_all(&buf[..n])?;
        h.update(&buf[..n]);
        got += n as u64;
        progress(got, total);
    }
    out.flush()?;
    drop(out);
    let sum: String = h.finalize().iter().map(|b| format!("{b:02x}")).collect();
    if sum != m.sha256 {
        let _ = std::fs::remove_file(&part);
        bail!("the downloaded file did not match the published checksum; not using it");
    }
    std::fs::rename(&part, &dest)?;
    Ok(dest)
}

/// One download at a time, reported to the admin page.
#[derive(Clone, Default, serde::Serialize)]
pub struct Job {
    /// "idle", "downloading", "loading", "ready", "error"
    pub state: &'static str,
    pub model: String,
    pub bytes: u64,
    pub total: Option<u64>,
    pub error: Option<String>,
}

pub fn job() -> &'static Mutex<Job> {
    static J: OnceLock<Mutex<Job>> = OnceLock::new();
    J.get_or_init(|| Mutex::new(Job { state: "idle", ..Default::default() }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn models_and_paths() {
        assert_eq!(find("s").unwrap().file, "yolox_s.onnx");
        assert_eq!(find("l").unwrap().file, "yolox_l.onnx");
        assert_eq!(identify(Path::new("x/yolox_l.onnx")).unwrap().id, "l");
        assert!(find("xl").is_none());
        assert_eq!(identify(Path::new("D:/x/models/yolox_tiny.onnx")).unwrap().id, "tiny");
        assert_eq!(clean_path(Path::new(r"\\?\D:\AmberSight\data\models\yolox_s.onnx")), "D:/AmberSight/data/models/yolox_s.onnx");
    }

    #[test]
    fn a_damaged_file_is_not_trusted() {
        let dir = std::env::temp_dir().join(format!("amber-ai-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join("x.bin");
        std::fs::write(&p, b"abc").unwrap();
        assert_eq!(sha256_file(&p).unwrap(), "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
