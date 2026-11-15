//! macOS Local Network privacy (macOS 15 and later).
//!
//! macOS asks before a program connects to devices on the Mac's own
//! networks. It only asks for a program that is code-signed: an unsigned one
//! (an Intel build: Apple's linker signs only Apple-silicon programs) is
//! blocked without asking, gets "No route to host" for every camera on the
//! local network, and never shows up in System Settings > Privacy & Security
//! > Local Network. A per-user LaunchAgent and a LaunchDaemon that runs as a
//! user (`UserName`) are both blocked; only a daemon running as root is
//! exempt. An ad-hoc signature (no Apple account needed) is enough for macOS
//! to ask and to list the program; the answer is kept across updates as long
//! as the identifier stays the same.

use std::path::Path;

/// The code-signing identifier of the program (also used by
/// scripts/package.sh). macOS keeps the Local Network answer for it, so it
/// must never change.
pub const SIGNING_ID: &str = "com.ambersight.ambersight";

/// Start of the camera error macOS's block produces (health.rs keeps it as is).
pub const BLOCKED_PREFIX: &str = "macOS blocked AmberSight from the local network";

const LC_CODE_SIGNATURE: u32 = 0x1d;

/// Whether a Mach-O program carries a code signature: `None` when the bytes
/// are not a single-architecture Mach-O header (a universal file, or too
/// short to tell).
pub fn macho_signed(bytes: &[u8]) -> Option<bool> {
    let u32_at = |o: usize| bytes.get(o..o + 4).map(|b| u32::from_le_bytes([b[0], b[1], b[2], b[3]]));
    let header = match u32_at(0)? {
        0xfeed_facf => 32,
        0xfeed_face => 28,
        _ => return None,
    };
    let ncmds = u32_at(16)?;
    let mut off = header;
    for _ in 0..ncmds {
        let cmd = u32_at(off)?;
        let size = u32_at(off + 4)? as usize;
        if cmd == LC_CODE_SIGNATURE {
            return Some(true);
        }
        if size < 8 {
            return None;
        }
        off += size;
    }
    Some(false)
}

/// The slices of a universal ("fat") file: (offset, size) of each
/// architecture's Mach-O. `None` when it is not a universal file.
pub fn fat_slices(head: &[u8]) -> Option<Vec<(u64, u64)>> {
    let be32 = |o: usize| head.get(o..o + 4).map(|b| u32::from_be_bytes([b[0], b[1], b[2], b[3]]) as u64);
    let be64 = |o: usize| head.get(o..o + 8).map(|b| u64::from_be_bytes([b[0], b[1], b[2], b[3], b[4], b[5], b[6], b[7]]));
    let wide = match be32(0)? {
        0xcafe_babe => false,
        0xcafe_babf => true,
        _ => return None,
    };
    let n = be32(4)? as usize;
    // (Java class files share 0xcafebabe; they have a version there, far above any arch count.)
    if n == 0 || n > 16 {
        return None;
    }
    let mut out = Vec::with_capacity(n);
    for i in 0..n {
        if wide {
            let o = 8 + i * 32;
            out.push((be64(o + 8)?, be64(o + 16)?));
        } else {
            let o = 8 + i * 20;
            out.push((be32(o + 8)?, be32(o + 12)?));
        }
    }
    Some(out)
}

/// Whether the single-architecture Mach-O starting at `at` in `f` is signed.
fn slice_signed(f: &mut std::fs::File, at: u64) -> Option<bool> {
    use std::io::{Read, Seek, SeekFrom};
    f.seek(SeekFrom::Start(at)).ok()?;
    let mut head = [0u8; 32];
    f.read_exact(&mut head).ok()?;
    let sizeofcmds = u32::from_le_bytes([head[20], head[21], head[22], head[23]]) as usize;
    // (Load commands are a few KB; refuse a header claiming more than 1 MB.)
    if sizeofcmds > 1 << 20 {
        return None;
    }
    let mut all = head.to_vec();
    all.resize(32 + sizeofcmds, 0);
    f.read_exact(&mut all[32..]).ok()?;
    macho_signed(&all)
}

/// Whether the program file at `path` is code-signed (`None`: cannot tell).
/// A universal file counts as signed only when every architecture in it is
/// (codesign signs them all at once).
pub fn file_signed(path: &Path) -> Option<bool> {
    use std::io::Read;
    let mut f = std::fs::File::open(path).ok()?;
    let mut head = vec![0u8; 8 + 16 * 32];
    let got = f.read(&mut head).ok()?;
    head.truncate(got);
    match fat_slices(&head) {
        Some(slices) => {
            let mut all = true;
            for (at, _) in slices {
                all &= slice_signed(&mut f, at)?;
            }
            Some(all)
        }
        None => slice_signed(&mut f, 0),
    }
}

/// This program's file, with symbolic links followed (a link to it is not
/// what macOS checks or what codesign must change).
fn program_file() -> Option<std::path::PathBuf> {
    std::env::current_exe().ok().map(|p| std::fs::canonicalize(&p).unwrap_or(p))
}

/// Whether this running program is code-signed (checked once).
fn program_signed() -> Option<bool> {
    static SIGNED: std::sync::OnceLock<Option<bool>> = std::sync::OnceLock::new();
    *SIGNED.get_or_init(|| program_file().and_then(|p| file_signed(&p)))
}

/// Whether a camera host is on a local network as macOS counts it: a
/// private, link-local or unique-local address, or a local name.
pub fn local_host(host: &str) -> bool {
    let host = host.trim_start_matches('[').trim_end_matches(']');
    if let Ok(ip) = host.parse::<std::net::IpAddr>() {
        return match ip {
            std::net::IpAddr::V4(v4) => v4.is_private() || v4.is_link_local(),
            std::net::IpAddr::V6(v6) => {
                let s = v6.segments()[0];
                (s & 0xfe00) == 0xfc00 || (s & 0xffc0) == 0xfe80
            }
        };
    }
    let h = host.to_ascii_lowercase();
    let h = h.trim_end_matches('.');
    !h.is_empty() && (!h.contains('.') || [".local", ".lan", ".home.arpa", ".internal", ".home"].iter().any(|s| h.ends_with(s)))
}

/// The camera error to show when macOS's Local Network privacy is the likely
/// cause: on a Mac, "No route to host" to a camera on the local network.
/// `signed` is whether this program is code-signed (`None`: unknown).
pub fn blocked_text(error: &str, url: &str, mac: bool, signed: Option<bool>) -> Option<String> {
    if !mac || !error.to_ascii_lowercase().contains("no route to host") {
        return None;
    }
    if !local_host(&crate::engine::newcams::host_of(url)) {
        return None;
    }
    Some(if signed == Some(false) {
        format!(
            "{BLOCKED_PREFIX} (or the camera is off): this copy of the program is not code-signed, so macOS blocks it without asking. \
             Run `ambersight setup` (or `ambersight service install`) again, which signs it, then click Allow when macOS asks"
        )
    } else {
        format!("{BLOCKED_PREFIX} (or the camera is off): allow ambersight in System Settings > Privacy & Security > Local Network")
    })
}

/// [`blocked_text`] for this computer and program.
pub fn blocked(error: &str, url: &str) -> Option<String> {
    let mac = cfg!(target_os = "macos");
    blocked_text(error, url, mac, if mac { program_signed() } else { None })
}

/// The command that signs a program by hand (the path quoted for a shell,
/// a `'` in it too).
pub fn sign_command(exe: &Path) -> String {
    format!("codesign --force --sign - --identifier {SIGNING_ID} '{}'", exe.display().to_string().replace('\'', "'\\''"))
}

/// At start on a Mac: say so in the log when the program is not signed.
pub fn startup_note() {
    if cfg!(target_os = "macos") && program_signed() == Some(false) {
        let exe = program_file().map(|p| sign_command(&p)).unwrap_or_default();
        tracing::warn!(
            "this program is not code-signed: macOS 15 and later block it from cameras on the local network without asking. \
             Run `ambersight setup` or `ambersight service install` again (it signs the program), or sign it by hand: {exe}"
        );
    }
}

/// Sign an unsigned program ad hoc so macOS asks for Local Network access
/// (macOS only). A symbolic link is followed: the program file itself is
/// signed. The signed copy is written beside it (a new file of its own
/// name, never through a link) and renamed over it, so a running program is
/// not changed under itself. A universal file is signed too (codesign signs
/// every architecture in it). Returns whether it signed anything.
#[cfg(target_os = "macos")]
pub fn sign_if_unsigned(exe: &Path) -> anyhow::Result<bool> {
    use anyhow::{bail, Context};
    use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt};
    let exe = &std::fs::canonicalize(exe).with_context(|| format!("could not find {}", exe.display()))?;
    if file_signed(exe) != Some(false) {
        return Ok(false);
    }
    let meta = std::fs::metadata(exe).with_context(|| format!("could not read {}", exe.display()))?;
    let nanos = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.subsec_nanos()).unwrap_or(0);
    let name = exe.file_name().and_then(|n| n.to_str()).unwrap_or("ambersight");
    let tmp = exe.with_file_name(format!(".{name}.signing-{}-{nanos}", std::process::id()));
    let copied = (|| -> std::io::Result<()> {
        let mut out = std::fs::OpenOptions::new().write(true).create_new(true).mode(0o700).custom_flags(libc::O_NOFOLLOW).open(&tmp)?;
        std::io::copy(&mut std::fs::File::open(exe)?, &mut out)?;
        out.sync_all()?;
        std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(meta.mode() & 0o7777))
    })();
    if let Err(e) = copied {
        let _ = std::fs::remove_file(&tmp);
        return Err(e).with_context(|| format!("could not copy {} to {}", exe.display(), tmp.display()));
    }
    let out = std::process::Command::new("/usr/bin/codesign")
        .args(["--force", "--sign", "-", "--identifier", SIGNING_ID])
        .arg(&tmp)
        .output();
    let ok = match out {
        Ok(o) if o.status.success() => Ok(()),
        Ok(o) => Err(String::from_utf8_lossy(&o.stderr).trim().to_string()),
        Err(e) => Err(e.to_string()),
    };
    if let Err(e) = ok {
        let _ = std::fs::remove_file(&tmp);
        bail!("codesign failed: {e}");
    }
    // Keep the owner (a root `--system` install signing a user's program).
    let _ = std::os::unix::fs::chown(&tmp, Some(meta.uid()), Some(meta.gid()));
    if let Err(e) = std::fs::rename(&tmp, exe) {
        let _ = std::fs::remove_file(&tmp);
        return Err(e).with_context(|| format!("could not replace {}", exe.display()));
    }
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A 64-bit Mach-O header with the given load commands (cmd, size).
    fn macho(cmds: &[(u32, u32)]) -> Vec<u8> {
        let mut b = Vec::new();
        b.extend(0xfeed_facfu32.to_le_bytes());
        b.extend(0x0100_0007u32.to_le_bytes()); // x86_64
        b.extend(3u32.to_le_bytes());
        b.extend(2u32.to_le_bytes()); // executable
        b.extend((cmds.len() as u32).to_le_bytes());
        b.extend(cmds.iter().map(|c| c.1).sum::<u32>().to_le_bytes());
        b.extend(0u32.to_le_bytes());
        b.extend(0u32.to_le_bytes());
        for &(cmd, size) in cmds {
            b.extend(cmd.to_le_bytes());
            b.extend(size.to_le_bytes());
            b.resize(b.len() + size as usize - 8, 0);
        }
        b
    }

    #[test]
    fn finds_the_code_signature() {
        assert_eq!(macho_signed(&macho(&[(0x19, 72), (0x2, 24)])), Some(false));
        assert_eq!(macho_signed(&macho(&[(0x19, 72), (0x1d, 16), (0x2, 24)])), Some(true));
        // Not a single-architecture Mach-O, cut short, or a broken command.
        assert_eq!(macho_signed(&0xcafe_babeu32.to_be_bytes()), None);
        assert_eq!(macho_signed(b"\x7fELF\x02\x01\x01\0"), None);
        assert_eq!(macho_signed(&macho(&[(0x19, 72), (0x1d, 16)])[..36]), None);
        assert_eq!(macho_signed(&macho(&[(0x19, 4)])), None);
    }

    #[test]
    fn reads_the_signature_from_a_file() {
        let dir = std::env::temp_dir().join(format!("amb-macnet-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let (a, b) = (dir.join("unsigned"), dir.join("signed"));
        std::fs::write(&a, macho(&[(0x19, 72)])).unwrap();
        std::fs::write(&b, macho(&[(0x19, 72), (0x1d, 16)])).unwrap();
        assert_eq!(file_signed(&a), Some(false));
        assert_eq!(file_signed(&b), Some(true));
        assert_eq!(file_signed(&dir.join("missing")), None);
        // A universal file: signed only when every architecture is.
        let fat = |slices: &[Vec<u8>]| {
            let mut b = Vec::new();
            b.extend(0xcafe_babeu32.to_be_bytes());
            b.extend((slices.len() as u32).to_be_bytes());
            let mut at = 4096u32;
            let mut body = Vec::new();
            for s in slices {
                b.extend([0u8; 8]); // cputype, cpusubtype
                b.extend(at.to_be_bytes());
                b.extend((s.len() as u32).to_be_bytes());
                b.extend(12u32.to_be_bytes());
                body.push((at, s.clone()));
                at += 4096;
            }
            for (o, s) in body {
                b.resize(o as usize, 0);
                b.extend(s);
            }
            b
        };
        let (fs, fu) = (dir.join("fat-signed"), dir.join("fat-half"));
        std::fs::write(&fs, fat(&[macho(&[(0x1d, 16)]), macho(&[(0x19, 72), (0x1d, 16)])])).unwrap();
        std::fs::write(&fu, fat(&[macho(&[(0x1d, 16)]), macho(&[(0x19, 72)])])).unwrap();
        assert_eq!(file_signed(&fs), Some(true));
        assert_eq!(file_signed(&fu), Some(false));
        assert_eq!(fat_slices(&fat(&[macho(&[]), macho(&[])])), Some(vec![(4096, 32), (8192, 32)]));
        // A Java class file shares the magic; its "count" is a version far above 16.
        assert_eq!(fat_slices(&[0xca, 0xfe, 0xba, 0xbe, 0, 0, 0, 52]), None);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn sign_command_quotes_the_path() {
        assert_eq!(
            sign_command(Path::new("/Users/o'brien/AmberSight/ambersight")),
            "codesign --force --sign - --identifier com.ambersight.ambersight '/Users/o'\\''brien/AmberSight/ambersight'"
        );
    }

    #[test]
    fn local_hosts() {
        for h in ["192.168.1.87", "10.0.2.2", "172.20.1.1", "169.254.1.2", "[fe80::1]", "fd00::5", "cam.local", "doorbell", "nvr.lan", "cam.home.arpa"] {
            assert!(local_host(h), "{h}");
        }
        for h in ["8.8.8.8", "100.64.1.2", "2001:db8::1", "camera.example.com", ""] {
            assert!(!local_host(h), "{h}");
        }
    }

    #[test]
    fn blocked_message_only_for_local_cameras_on_a_mac() {
        let e = "DESCRIBE failed: Unable to connect to RTSP server: No route to host (os error 65)";
        let url = "rtsp://admin:pw@192.168.1.50:554/main";
        let signed = blocked_text(e, url, true, Some(true)).unwrap();
        assert!(signed.starts_with(BLOCKED_PREFIX));
        assert!(signed.contains("Privacy & Security > Local Network"));
        let unsigned = blocked_text(e, url, true, Some(false)).unwrap();
        assert!(unsigned.contains("not code-signed") && unsigned.contains("ambersight setup"));
        assert!(blocked_text(e, url, true, None).unwrap().contains("Local Network"));
        // ffmpeg's wording too.
        assert!(blocked_text("Error opening input files: No route to host", url, true, Some(true)).is_some());
        // Not a Mac, another error, or a camera off the local network.
        assert!(blocked_text(e, url, false, Some(true)).is_none());
        assert!(blocked_text("Connection refused (os error 61)", url, true, Some(true)).is_none());
        assert!(blocked_text(e, "rtsp://camera.example.com/live", true, Some(true)).is_none());
        // Never the camera's password.
        assert!(!signed.contains("pw") && !unsigned.contains("admin"));
    }

    #[test]
    fn plain_error_keeps_the_message() {
        let m = blocked_text("No route to host", "rtsp://10.0.0.5/x", true, Some(true)).unwrap();
        assert_eq!(crate::engine::health::plain_error(&m), m);
    }
}
