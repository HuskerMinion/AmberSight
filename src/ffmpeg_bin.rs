//! Finding the ffmpeg program, and fetching it once when the computer has
//! none.
//!
//! AmberSight runs ffmpeg for the cameras it cannot read itself, for clips,
//! snapshots and playback pictures. It looks for ffmpeg in this order:
//!
//! 1. next to the AmberSight program (a copy put there by hand),
//! 2. in `<data folder>/tools`, where AmberSight keeps the one it downloaded,
//! 3. on the system PATH (Homebrew, winget, apt, ...).
//!
//! The capture helper, run by an ordinary account on the computer of a
//! Windows service, cannot know the (private) data folder. It also reads
//! `ffmpeg-path.txt` beside the program, which the service writes (see
//! `lockdown::share_ffmpeg_in_background`), between steps 1 and 2. The
//! server never reads that note.
//!
//! When none of them works at start on a Mac (Apple silicon or Intel) or on
//! Windows, AmberSight downloads one fixed ffmpeg version, checks it against
//! the checksum written below (the same way the AI models are fetched) and
//! keeps only the ffmpeg program from the zip. That happens once; later
//! starts find it in the tools folder. On Linux the system's ffmpeg from the
//! package manager is used (it knows the computer's graphics drivers), and a
//! missing one gets a clear message instead.
//!
//! The downloads (ffmpeg 8.1.2, the same version on every computer):
//! - macOS: Martin Riedl's build server (ffmpeg.martin-riedl.de), which
//!   keeps every release it has built. GPL v3 build (it includes x264 and
//!   x265). The program is signed by its builder ("Developer ID
//!   Application: Martin Riedl") with Apple's hardened runtime.
//! - Windows: Gyan Doshi's "essentials" build, from the GyanD/codexffmpeg
//!   releases on GitHub (versioned releases stay up). GPL v3.
//!
//! The ffmpeg code is the FFmpeg project's (LGPL/GPL). AmberSight does not
//! ship it: the user's own computer downloads it from the builders above.
//!
//! Mac Gatekeeper: a file is only checked by Gatekeeper when it carries the
//! "quarantine" mark, which web browsers and other apps that ask for it add
//! to their downloads. AmberSight writes the file itself, so it has no mark
//! and macOS runs it without asking. (Even if it had one, these builds are
//! signed with a Developer ID, so they would not be blocked as "unknown".)
//! Apple silicon also needs every program to be signed; these are.

use anyhow::{anyhow, bail, Context, Result};
use std::path::{Path, PathBuf};
use std::sync::Mutex;

/// One fixed ffmpeg download: where it is, what it must hash to, and which
/// file inside the zip is the program.
pub struct Pinned {
    pub version: &'static str,
    pub url: &'static str,
    pub sha256: &'static str,
    pub bytes: u64,
    /// Path of the ffmpeg program inside the zip.
    pub member: &'static str,
    /// Where a person can download it by hand (shown when ours fails).
    pub page: &'static str,
}

/// The downloads, one per kind of computer: Mac with Apple silicon, Intel
/// Mac, Windows (64-bit Intel/AMD).
const DOWNLOADS: [Pinned; 3] = [
    Pinned {
        version: "8.1.2",
        url: "https://ffmpeg.martin-riedl.de/download/macos/arm64/1783011502_8.1.2/ffmpeg.zip",
        sha256: "ef1aa60006c7b77ce170c1608c08d8e4ba1c30c5746f2ac986ded932d0ac2c3c",
        bytes: 28_196_358,
        member: "ffmpeg",
        page: "https://ffmpeg.martin-riedl.de",
    },
    Pinned {
        version: "8.1.2",
        url: "https://ffmpeg.martin-riedl.de/download/macos/amd64/1783018342_8.1.2/ffmpeg.zip",
        sha256: "a52ef43883f44c219766d4b3bdde4e635b35465d0b704c01c3a0566b59775df9",
        bytes: 33_586_778,
        member: "ffmpeg",
        page: "https://ffmpeg.martin-riedl.de",
    },
    Pinned {
        version: "8.1.2",
        url: "https://github.com/GyanD/codexffmpeg/releases/download/8.1.2/ffmpeg-8.1.2-essentials_build.zip",
        sha256: "db580001caa24ac104c8cb856cd113a87b0a443f7bdf47d8c12b1d740584a2ec",
        bytes: 109_728_040,
        member: "ffmpeg-8.1.2-essentials_build/bin/ffmpeg.exe",
        page: "https://www.gyan.dev/ffmpeg/builds/",
    },
];

/// The download for this computer. None on Linux and others: they use the
/// system's ffmpeg.
const PINNED: Option<&Pinned> = if cfg!(all(target_os = "macos", target_arch = "aarch64")) {
    Some(&DOWNLOADS[0])
} else if cfg!(all(target_os = "macos", target_arch = "x86_64")) {
    Some(&DOWNLOADS[1])
} else if cfg!(all(windows, target_arch = "x86_64")) {
    Some(&DOWNLOADS[2])
} else {
    None
};

/// The ffmpeg program inside a zip is never bigger than this (they are
/// about 65-100 MB); a bigger one is not what we expect.
const MAX_PROGRAM_BYTES: u64 = 400 << 20;

/// What has been found so far.
struct State {
    tools: Option<PathBuf>,
    chosen: Option<PathBuf>,
    /// Shown in the status page, e.g. "8.1.2 (downloaded by AmberSight)".
    note: Option<String>,
}

static STATE: Mutex<State> = Mutex::new(State { tools: None, chosen: None, note: None });

fn exe_name() -> &'static str {
    if cfg!(windows) {
        "ffmpeg.exe"
    } else {
        "ffmpeg"
    }
}

/// The folder AmberSight keeps its own ffmpeg in.
pub fn tools_dir(data_dir: &Path) -> PathBuf {
    let d = data_dir.join("tools");
    std::path::absolute(&d).unwrap_or(d)
}

/// Tell the search where the downloaded copy lives (from the config's data
/// folder); commands that read the config call this before running ffmpeg.
pub fn set_data_dir(data_dir: &Path) {
    STATE.lock().unwrap().tools = Some(tools_dir(data_dir));
}

/// The ffmpeg program to run. Before `ensure` has checked one, this is the
/// first ffmpeg file found in the search order, else plain "ffmpeg".
pub fn program() -> PathBuf {
    let tools = {
        let st = STATE.lock().unwrap();
        if let Some(p) = &st.chosen {
            return p.clone();
        }
        st.tools.clone()
    };
    candidates(exe_dir().as_deref(), None, tools.as_deref(), std::env::var_os("PATH").as_deref()).into_iter().next().unwrap_or_else(|| PathBuf::from(exe_name()))
}

/// The note beside the program that tells the capture helper where the
/// server's ffmpeg is.
pub const HINT_FILE: &str = "ffmpeg-path.txt";

/// The path a note's text names, only when it is an absolute path to an
/// existing file called exactly ffmpeg.exe (ffmpeg on other systems), with no
/// `..` in it. Anything else is ignored.
fn hint_target(text: &str) -> Option<PathBuf> {
    let line = text.trim_start_matches('\u{feff}').lines().next()?.trim();
    let p = PathBuf::from(line);
    let plain = p.is_absolute() && !p.components().any(|c| matches!(c, std::path::Component::ParentDir));
    let named = p.file_name().is_some_and(|n| n.to_string_lossy().eq_ignore_ascii_case(exe_name()));
    (plain && named && p.is_file()).then_some(p)
}

/// The ffmpeg the note beside the program names, if usable.
fn read_hint(exe_dir: Option<&Path>) -> Option<PathBuf> {
    use std::io::Read;
    let f = std::fs::File::open(exe_dir?.join(HINT_FILE)).ok()?;
    let mut text = String::new();
    f.take(4096).read_to_string(&mut text).ok()?;
    hint_target(&text)
}

/// The ffmpeg for the capture helper: like `program`, plus the note the
/// service leaves beside the program. Only the helper calls this.
pub fn helper_program() -> PathBuf {
    let dir = exe_dir();
    let tools = STATE.lock().unwrap().tools.clone();
    let hint = read_hint(dir.as_deref());
    candidates(dir.as_deref(), hint.as_deref(), tools.as_deref(), std::env::var_os("PATH").as_deref()).into_iter().next().unwrap_or_else(|| PathBuf::from(exe_name()))
}

/// The ffmpeg file to run, only when one exists (for handing it to a
/// capture helper).
pub fn existing() -> Option<PathBuf> {
    Some(program()).filter(|p| p.is_file())
}

/// One line for the status page: the version and where it came from.
pub fn status() -> Option<String> {
    STATE.lock().unwrap().note.clone()
}

fn exe_dir() -> Option<PathBuf> {
    std::env::current_exe().ok().and_then(|e| e.parent().map(Path::to_path_buf))
}

/// Every ffmpeg file that exists, in search order: next to the program,
/// in the tools folder, then each PATH folder.
fn candidates(exe_dir: Option<&Path>, hint: Option<&Path>, tools: Option<&Path>, path_env: Option<&std::ffi::OsStr>) -> Vec<PathBuf> {
    let name = exe_name();
    let mut files: Vec<PathBuf> = exe_dir.into_iter().map(|d| d.join(name)).collect();
    files.extend(hint.map(Path::to_path_buf));
    files.extend(tools.map(|d| d.join(name)));
    if let Some(p) = path_env {
        files.extend(std::env::split_paths(p).map(|d| d.join(name)));
    }
    let mut out: Vec<PathBuf> = Vec::new();
    for f in files {
        if f.is_file() && !out.contains(&f) {
            out.push(f);
        }
    }
    out
}

/// The version an ffmpeg program reports, or None when it does not run.
fn version_of(prog: &Path) -> Option<String> {
    let mut c = std::process::Command::new(prog);
    c.arg("-version").stdin(std::process::Stdio::null()).stderr(std::process::Stdio::null());
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        c.creation_flags(0x0800_0000); // CREATE_NO_WINDOW
    }
    let out = c.output().ok().filter(|o| o.status.success())?;
    let text = String::from_utf8_lossy(&out.stdout);
    // "ffmpeg version 8.1.2-essentials_build-www.gyan.dev Copyright ..."
    let v = text.lines().next()?.split_whitespace().nth(2).unwrap_or("?");
    Some(v.split("-http").next().unwrap_or(v).chars().take(40).collect())
}

/// Make sure a working ffmpeg is there before the server starts: use the
/// first one that runs, else download the fixed version once (Mac and
/// Windows). Blocks while downloading.
pub fn ensure(data_dir: &Path) -> Result<()> {
    set_data_dir(data_dir);
    let tools = tools_dir(data_dir);
    let found = candidates(exe_dir().as_deref(), None, Some(&tools), std::env::var_os("PATH").as_deref());
    for p in &found {
        match version_of(p) {
            Some(v) => {
                let from = if p.starts_with(&tools) { "downloaded by AmberSight" } else { "installed" };
                choose(p.clone(), format!("{v} ({from})"));
                tracing::info!("using ffmpeg {v}: {}", p.display());
                return Ok(());
            }
            None => tracing::warn!("ffmpeg at {} does not run; skipping it", p.display()),
        }
    }
    let Some(pin) = PINNED else { bail!("{}", missing_help(None)) };
    let mb = pin.bytes as f64 / 1_048_576.0;
    println!("ffmpeg was not found; downloading ffmpeg {} ({mb:.0} MB), one time...", pin.version);
    tracing::info!("ffmpeg was not found; downloading ffmpeg {} ({mb:.0} MB), one time", pin.version);
    let step = std::sync::atomic::AtomicU64::new(0);
    let got = fetch(&tools, pin, &|done, total| {
        // A line every 25%.
        let quarter = done * 4 / total.max(1);
        if quarter > step.swap(quarter, std::sync::atomic::Ordering::Relaxed) && quarter < 4 {
            tracing::info!("downloading ffmpeg: {}%", quarter * 25);
        }
    })
    .and_then(|p| match version_of(&p) {
        Some(v) => Ok((p, v)),
        None => Err(anyhow!("the downloaded ffmpeg does not run on this computer")),
    });
    match got {
        Ok((p, v)) => {
            println!("ffmpeg {v} is ready ({})", p.display());
            tracing::info!("ffmpeg {v} downloaded and checked: {}", p.display());
            choose(p, format!("{v} (downloaded by AmberSight)"));
            Ok(())
        }
        Err(e) => bail!("{}", missing_help(Some(&format!("{e:#}")))),
    }
}

fn choose(p: PathBuf, note: String) {
    let mut st = STATE.lock().unwrap();
    st.chosen = Some(p);
    st.note = Some(note);
}

/// The message when there is no ffmpeg: what went wrong and how to fix it
/// by hand on this kind of computer.
fn missing_help(download_error: Option<&str>) -> String {
    let head = match download_error {
        Some(e) => format!("AmberSight needs ffmpeg and could not download it ({e})."),
        None => "AmberSight needs ffmpeg, and it was not found.".to_string(),
    };
    let how = match PINNED {
        Some(p) if cfg!(windows) => format!(
            "To fix: run  winget install ffmpeg  (then restart AmberSight), or download {} , unzip it, and copy bin\\ffmpeg.exe next to ambersight.exe. Downloads: {}",
            p.url, p.page
        ),
        Some(p) => format!(
            "To fix: run  brew install ffmpeg , or download {} , unzip it, and put the ffmpeg program next to AmberSight. Downloads: {}",
            p.url, p.page
        ),
        None if cfg!(target_os = "linux") => "To fix: install it with your package manager, then start AmberSight again. Ubuntu, Debian, Mint:  sudo apt install ffmpeg . Arch:  sudo pacman -S ffmpeg . Fedora: its own ffmpeg (ffmpeg-free) cannot play H.265 cameras, so first add RPM Fusion (https://rpmfusion.org/Configuration), then run  sudo dnf install ffmpeg --allowerasing .".to_string(),
        None => "To fix: install ffmpeg from https://ffmpeg.org/download.html and put the ffmpeg program next to AmberSight or on the PATH.".to_string(),
    };
    format!("{head} {how}")
}

/// Download the zip into `tools`, check it, and pull out the program.
fn fetch(tools: &Path, pin: &Pinned, progress: &dyn Fn(u64, u64)) -> Result<PathBuf> {
    std::fs::create_dir_all(tools).with_context(|| format!("could not make {}", tools.display()))?;
    let agent = ureq::Agent::config_builder()
        .timeout_connect(Some(std::time::Duration::from_secs(20)))
        .timeout_recv_response(Some(std::time::Duration::from_secs(60)))
        .timeout_recv_body(Some(std::time::Duration::from_secs(3600)))
        .build()
        .new_agent();
    let mut resp = agent.get(pin.url).call().map_err(|e| anyhow!("download failed: {e}"))?;
    // One byte over the expected size is enough to know it is wrong.
    let rd = resp.body_mut().with_config().limit(pin.bytes + 1).reader();
    let zip_path = tools.join(format!("ffmpeg-{}.zip", pin.version));
    save_verified(rd, &zip_path, pin.sha256, pin.bytes, progress)?;
    let dest = tools.join(exe_name());
    let r = extract(&zip_path, pin.member, &dest);
    // The zip is no longer needed once the program is out.
    let _ = std::fs::remove_file(&zip_path);
    r.map(|()| dest)
}

/// Write `rd` to `<dest>.part`, then rename it to `dest` only when its size
/// and checksum are the expected ones (otherwise the .part is removed).
fn save_verified(mut rd: impl std::io::Read, dest: &Path, sha256: &str, bytes: u64, progress: &dyn Fn(u64, u64)) -> Result<()> {
    use sha2::{Digest, Sha256};
    use std::io::Write;
    let tmp = dest.with_extension("part");
    let result = (|| -> Result<()> {
        let mut out = std::fs::File::create(&tmp).with_context(|| format!("could not write {}", tmp.display()))?;
        let mut h = Sha256::new();
        let mut buf = vec![0u8; 1 << 16];
        let mut done = 0u64;
        loop {
            let n = rd.read(&mut buf).context("download interrupted")?;
            if n == 0 {
                break;
            }
            done += n as u64;
            if done > bytes {
                bail!("the download is bigger than expected");
            }
            out.write_all(&buf[..n])?;
            h.update(&buf[..n]);
            progress(done, bytes);
        }
        out.sync_all()?;
        if done != bytes {
            bail!("the download is incomplete ({done} of {bytes} bytes)");
        }
        let got: String = h.finalize().iter().map(|b| format!("{b:02x}")).collect();
        if got != sha256 {
            bail!("the download did not match its checksum");
        }
        Ok(())
    })();
    if let Err(e) = result {
        let _ = std::fs::remove_file(&tmp);
        return Err(e);
    }
    std::fs::rename(&tmp, dest)?;
    Ok(())
}

/// Copy the one file `member` out of the zip to `dest` (through a .part
/// file), and make it runnable on Mac/Linux.
fn extract(zip_path: &Path, member: &str, dest: &Path) -> Result<()> {
    use std::io::Read;
    let f = std::fs::File::open(zip_path)?;
    let mut z = zip::ZipArchive::new(f).context("the download is not a readable zip")?;
    let mut e = z.by_name(member).with_context(|| format!("{member} is missing from the zip"))?;
    let size = e.size();
    if size > MAX_PROGRAM_BYTES {
        bail!("{member} in the zip is too big ({size} bytes)");
    }
    let tmp = dest.with_extension("part");
    let result = (|| -> Result<()> {
        let mut out = std::fs::File::create(&tmp)?;
        // The zip reader also checks each file's own checksum (CRC).
        let n = std::io::copy(&mut (&mut e).take(MAX_PROGRAM_BYTES + 1), &mut out).context("could not unzip ffmpeg")?;
        if n != size {
            bail!("{member} unzipped to the wrong size");
        }
        out.sync_all()?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o755))?;
        }
        Ok(())
    })();
    if let Err(e) = result {
        let _ = std::fs::remove_file(&tmp);
        return Err(e);
    }
    std::fs::rename(&tmp, dest)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn temp(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("ambersight-ffbin-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    fn sha(data: &[u8]) -> String {
        use sha2::{Digest, Sha256};
        Sha256::digest(data).iter().map(|b| format!("{b:02x}")).collect()
    }

    #[test]
    fn search_order_is_beside_program_then_tools_then_path() {
        let root = temp("order");
        let (beside, tools, p1, p2) = (root.join("exe"), root.join("tools"), root.join("p1"), root.join("p2"));
        for d in [&beside, &tools, &p1, &p2] {
            std::fs::create_dir_all(d).unwrap();
        }
        let path_env = std::env::join_paths([&p1, &p2]).unwrap();
        let find = || candidates(Some(&beside), None, Some(&tools), Some(&path_env));
        assert!(find().is_empty());
        std::fs::write(p2.join(exe_name()), b"x").unwrap();
        assert_eq!(find(), vec![p2.join(exe_name())]);
        std::fs::write(p1.join(exe_name()), b"x").unwrap();
        assert_eq!(find()[0], p1.join(exe_name()));
        std::fs::write(tools.join(exe_name()), b"x").unwrap();
        assert_eq!(find()[0], tools.join(exe_name()));
        std::fs::write(beside.join(exe_name()), b"x").unwrap();
        assert_eq!(find(), vec![beside.join(exe_name()), tools.join(exe_name()), p1.join(exe_name()), p2.join(exe_name())]);
        // A folder named ffmpeg is not the program.
        let _ = std::fs::remove_file(beside.join(exe_name()));
        std::fs::create_dir_all(beside.join(exe_name())).unwrap();
        assert_eq!(find()[0], tools.join(exe_name()));
        // No tools folder known yet (and the same folder twice counts once).
        let path_env = std::env::join_paths([&p1, &p1]).unwrap();
        assert_eq!(candidates(None, None, None, Some(&path_env)), vec![p1.join(exe_name())]);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn the_note_is_one_candidate_between_the_program_folder_and_path() {
        let root = temp("hint");
        let (beside, data, p1) = (root.join("exe"), root.join("data"), root.join("p1"));
        for d in [&beside, &data, &p1] {
            std::fs::create_dir_all(d).unwrap();
        }
        let path_env = std::env::join_paths([&p1]).unwrap();
        let (tool, on_path, next) = (data.join(exe_name()), p1.join(exe_name()), beside.join(exe_name()));
        std::fs::write(&tool, b"x").unwrap();
        std::fs::write(&on_path, b"x").unwrap();
        let find = |h: Option<&Path>| candidates(Some(&beside), h, None, Some(&path_env));
        assert_eq!(find(None), vec![on_path.clone()]);
        assert_eq!(find(Some(&tool)), vec![tool.clone(), on_path.clone()]);
        std::fs::write(&next, b"x").unwrap();
        assert_eq!(find(Some(&tool)), vec![next.clone(), tool.clone(), on_path.clone()]);
        // The server's own search never takes a note.
        std::fs::write(beside.join(HINT_FILE), format!("{}\r\n", tool.display())).unwrap();
        assert_eq!(read_hint(Some(&beside)), Some(tool.clone()));
        assert_eq!(candidates(Some(&beside), None, None, Some(&path_env)), vec![next, on_path]);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn the_note_is_ignored_unless_it_names_an_existing_ffmpeg_file() {
        let root = temp("hintcheck");
        let good = root.join(exe_name());
        std::fs::write(&good, b"x").unwrap();
        let other = root.join(if cfg!(windows) { "ffprobe.exe" } else { "ffprobe" });
        std::fs::write(&other, b"x").unwrap();
        let say = |p: &Path| hint_target(&format!("{}\r\nsecond line\r\n", p.display()));
        assert_eq!(say(&good), Some(good.clone()));
        assert_eq!(hint_target(&format!("\u{feff}  {}  ", good.display())), Some(good.clone()));
        // Another program, a folder, a missing file, a relative path, a path with .. in it, empty.
        assert_eq!(say(&other), None);
        assert_eq!(say(&root), None);
        assert_eq!(say(&root.join("nope").join(exe_name())), None);
        assert_eq!(hint_target(exe_name()), None);
        std::fs::create_dir_all(root.join("sub")).unwrap();
        assert_eq!(say(&root.join("sub").join("..").join(exe_name())), None);
        assert_eq!(hint_target(""), None);
        // A note that is missing or too long to be a path.
        assert_eq!(read_hint(Some(&root)), None);
        std::fs::write(root.join(HINT_FILE), vec![b'a'; 100_000]).unwrap();
        assert_eq!(read_hint(Some(&root)), None);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn checked_download_is_kept_and_a_wrong_one_removed() {
        let root = temp("verify");
        let data = vec![7u8; 200_000];
        let dest = root.join("ffmpeg-1.zip");
        let part = root.join("ffmpeg-1.part");
        let last = std::sync::atomic::AtomicU64::new(0);
        save_verified(&data[..], &dest, &sha(&data), data.len() as u64, &|d, _| last.store(d, std::sync::atomic::Ordering::Relaxed)).unwrap();
        assert_eq!(std::fs::read(&dest).unwrap(), data);
        assert_eq!(last.into_inner(), data.len() as u64);
        assert!(!part.exists());
        std::fs::remove_file(&dest).unwrap();

        // Wrong checksum: nothing kept.
        let e = save_verified(&data[..], &dest, &sha(b"other"), data.len() as u64, &|_, _| {}).unwrap_err();
        assert!(e.to_string().contains("checksum"), "{e}");
        assert!(!dest.exists() && !part.exists());
        // Bigger than expected: stopped, nothing kept.
        let e = save_verified(&data[..], &dest, &sha(&data), 1000, &|_, _| {}).unwrap_err();
        assert!(e.to_string().contains("bigger"), "{e}");
        assert!(!dest.exists() && !part.exists());
        // Cut short: nothing kept.
        let e = save_verified(&data[..100], &dest, &sha(&data), data.len() as u64, &|_, _| {}).unwrap_err();
        assert!(e.to_string().contains("incomplete"), "{e}");
        assert!(!dest.exists() && !part.exists());
        let _ = std::fs::remove_dir_all(&root);
    }

    fn make_zip(path: &Path, files: &[(&str, &[u8])]) {
        use zip::write::SimpleFileOptions;
        let mut w = zip::ZipWriter::new(std::fs::File::create(path).unwrap());
        let opts = SimpleFileOptions::default().compression_method(zip::CompressionMethod::Deflated);
        for (name, body) in files {
            w.start_file(*name, opts).unwrap();
            w.write_all(body).unwrap();
        }
        w.finish().unwrap();
    }

    #[test]
    fn unzip_takes_only_the_named_program() {
        let root = temp("unzip");
        let zip_path = root.join("a.zip");
        let prog: Vec<u8> = (0..100_000u32).map(|i| (i % 251) as u8).collect();
        make_zip(&zip_path, &[
            ("ffmpeg-9/bin/ffplay.exe", b"not this one"),
            ("ffmpeg-9/bin/ffmpeg.exe", &prog),
            ("ffmpeg-9/LICENSE", b"GPL"),
        ]);
        let dest = root.join("ffmpeg.exe");
        extract(&zip_path, "ffmpeg-9/bin/ffmpeg.exe", &dest).unwrap();
        assert_eq!(std::fs::read(&dest).unwrap(), prog);
        assert!(!root.join("ffmpeg.part").exists());
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(std::fs::metadata(&dest).unwrap().permissions().mode() & 0o777, 0o755);
        }
        // A name that is not in the zip is an error, and nothing is written.
        let other = root.join("other");
        assert!(extract(&zip_path, "ffmpeg", &other).is_err());
        assert!(!other.exists());
        // Not a zip at all.
        std::fs::write(&zip_path, b"hello").unwrap();
        assert!(extract(&zip_path, "ffmpeg-9/bin/ffmpeg.exe", &other).is_err());
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn pinned_download_is_sane() {
        for (i, p) in DOWNLOADS.iter().enumerate() {
            assert!(p.url.starts_with("https://") && p.url.contains(p.version) && p.url.ends_with(".zip"));
            assert_eq!(p.sha256.len(), 64);
            assert!(p.sha256.chars().all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase()));
            assert!(p.bytes > 10 << 20 && p.bytes < 200 << 20);
            // Windows is last; the Mac ones are the bare program.
            assert_eq!(p.member.ends_with("/bin/ffmpeg.exe"), i == 2);
            assert!(i == 2 || p.member == "ffmpeg");
        }
        if let Some(p) = PINNED {
            assert!(p.member.ends_with(exe_name()));
        }
        assert_eq!(PINNED.is_some(), cfg!(any(target_os = "macos", windows)) && cfg!(any(target_arch = "x86_64", target_arch = "aarch64")) && !cfg!(all(windows, target_arch = "aarch64")));
        assert!(missing_help(Some("offline")).contains("offline"));
        if cfg!(target_os = "linux") {
            // Fedora's own ffmpeg-free cannot decode H.265: the help says so.
            assert!(missing_help(None).contains("RPM Fusion"));
        }
    }
}
