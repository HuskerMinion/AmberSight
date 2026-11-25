//! Graphics cards the built-in AI can run on: Windows (DirectML, which
//! numbers them the way DXGI lists them), 64-bit Linux PCs (NVIDIA cards
//! with CUDA, as nvidia-smi lists them, Intel graphics with OpenVINO and
//! AMD graphics with WebGPU in a helper program, from the kernel's list of
//! cards) and Apple silicon Macs (Core ML: one
//! entry for the chip). Elsewhere the list is empty and the AI runs on the
//! CPU.

#[derive(Clone, Debug, serde::Serialize)]
pub struct Gpu {
    /// Its number for DirectML (DXGI adapter order); on Linux nvidia-smi's
    /// number (PCI order), which CUDA may count differently: `bus` finds it.
    pub index: u32,
    /// As the system names it; on Linux, cards of the same model get " #<index>".
    pub name: String,
    /// Its own video memory, MB (0 for most integrated graphics).
    pub memory_mb: u64,
    /// Linux: its PCI address ("00000000:01:00.0" from nvidia-smi,
    /// "0000:03:00.0" for Intel), by which CUDA or OpenVINO is asked for its
    /// own number for the card; empty elsewhere.
    #[serde(skip)]
    #[cfg_attr(not(all(target_os = "linux", target_arch = "x86_64", target_env = "gnu")), allow(dead_code))]
    pub bus: String,
    /// What the AI runs it with: "CUDA", "OpenVINO", "WebGPU" (AMD, in the
    /// helper program of ai_amd.rs), "DirectML" or "Core ML".
    pub api: &'static str,
    /// Linux, Intel and AMD: its render node ("/dev/dri/renderD128").
    #[serde(skip)]
    #[cfg_attr(not(all(target_os = "linux", target_arch = "x86_64", target_env = "gnu")), allow(dead_code))]
    pub render: String,
}

/// The graphics cards in this computer (not Windows' software renderer).
#[cfg(windows)]
pub fn list() -> Vec<Gpu> {
    use windows::Win32::Graphics::Dxgi::{CreateDXGIFactory1, IDXGIFactory1, DXGI_ADAPTER_FLAG_SOFTWARE};
    let mut out = Vec::new();
    let Ok(f) = (unsafe { CreateDXGIFactory1::<IDXGIFactory1>() }) else { return out };
    for i in 0..16u32 {
        let Ok(a) = (unsafe { f.EnumAdapters1(i) }) else { break };
        let Ok(d) = (unsafe { a.GetDesc1() }) else { continue };
        if d.Flags & DXGI_ADAPTER_FLAG_SOFTWARE.0 as u32 != 0 {
            continue;
        }
        let len = d.Description.iter().position(|c| *c == 0).unwrap_or(d.Description.len());
        out.push(Gpu {
            index: i,
            name: String::from_utf16_lossy(&d.Description[..len]).trim().to_string(),
            memory_mb: d.DedicatedVideoMemory as u64 / (1024 * 1024),
            bus: String::new(),
            api: "DirectML",
            render: String::new(),
        });
    }
    out
}

/// The NVIDIA cards (none without NVIDIA's driver), then the Intel ones,
/// then the AMD ones.
/// nvidia-smi also gives the NVIDIA cards' memory; without it, the driver's
/// own list gives the names. Asked at most once a minute (Settings asks on
/// each visit); the cache is not held while nvidia-smi runs.
#[cfg(all(target_os = "linux", target_arch = "x86_64", target_env = "gnu"))]
pub fn list() -> Vec<Gpu> {
    use std::sync::Mutex;
    use std::time::{Duration, Instant};
    static CACHE: Mutex<Option<(Instant, Vec<Gpu>)>> = Mutex::new(None);
    if let Some((at, v)) = CACHE.lock().unwrap().as_ref() {
        if at.elapsed() < Duration::from_secs(60) {
            return v.clone();
        }
    }
    let mut v = nvidia_smi().map(|t| parse_nvidia_smi(&t)).filter(|v| !v.is_empty()).unwrap_or_else(nvidia_proc);
    number_same_names(&mut v);
    let mut intel = intel_cards(std::path::Path::new("/sys/class/drm"), &crate::openvino::known_cards());
    number_same_names(&mut intel);
    v.extend(intel);
    let ids: Vec<&std::path::Path> = PCI_IDS.iter().map(std::path::Path::new).collect();
    let amd_ids: Vec<&std::path::Path> = AMDGPU_IDS.iter().map(std::path::Path::new).collect();
    let mut amd = amd_cards(std::path::Path::new("/sys/class/drm"), &amd_ids, &ids);
    number_same_names(&mut amd);
    v.extend(amd);
    *CACHE.lock().unwrap() = Some((Instant::now(), v.clone()));
    v
}

/// `nvidia-smi --query-gpu=...` output, or None (not installed, failed, or
/// no answer within 5 seconds: a stuck driver must not hold Settings up).
#[cfg(all(target_os = "linux", target_arch = "x86_64", target_env = "gnu"))]
fn nvidia_smi() -> Option<String> {
    use std::process::{Command, Stdio};
    use std::time::{Duration, Instant};
    let mut child = Command::new("nvidia-smi")
        .args(["--query-gpu=index,name,memory.total,pci.bus_id", "--format=csv,noheader,nounits"])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .ok()?;
    let t0 = Instant::now();
    loop {
        match child.try_wait() {
            Ok(Some(st)) if st.success() => break,
            Ok(Some(_)) | Err(_) => return None,
            Ok(None) if t0.elapsed() > Duration::from_secs(5) => {
                let _ = child.kill();
                let _ = child.wait();
                return None;
            }
            Ok(None) => std::thread::sleep(Duration::from_millis(20)),
        }
    }
    let mut out = String::new();
    std::io::Read::read_to_string(&mut child.stdout.take()?, &mut out).ok()?;
    Some(out)
}

/// "0, NVIDIA GeForce RTX 5060 Ti, 16311, 00000000:01:00.0" per line.
#[cfg_attr(not(all(target_os = "linux", target_arch = "x86_64", target_env = "gnu")), allow(dead_code))]
fn parse_nvidia_smi(text: &str) -> Vec<Gpu> {
    text.lines()
        .filter_map(|l| {
            let mut p = l.split(',').map(str::trim);
            let index = p.next()?.parse().ok()?;
            let name = p.next().filter(|n| !n.is_empty())?.to_string();
            let memory_mb = p.next().and_then(|m| m.parse().ok()).unwrap_or(0);
            let bus = p.next().filter(|b| b.contains(':')).unwrap_or("").to_string();
            Some(Gpu { index, name, memory_mb, bus, api: "CUDA", render: String::new() })
        })
        .collect()
}

/// The cards from the driver's own files (no memory sizes there), in PCI
/// order as nvidia-smi numbers them.
#[cfg(all(target_os = "linux", target_arch = "x86_64", target_env = "gnu"))]
fn nvidia_proc() -> Vec<Gpu> {
    let Ok(rd) = std::fs::read_dir("/proc/driver/nvidia/gpus") else { return Vec::new() };
    let mut cards: Vec<(String, String)> = rd
        .flatten()
        .filter_map(|e| {
            let text = std::fs::read_to_string(e.path().join("information")).ok()?;
            Some((e.file_name().to_string_lossy().to_lowercase(), proc_model(&text)?))
        })
        .collect();
    cards.sort();
    cards.into_iter().enumerate().map(|(i, (bus, name))| Gpu { index: i as u32, name, memory_mb: 0, bus, api: "CUDA", render: String::new() }).collect()
}

/// The Intel graphics the kernel runs (i915 or xe driver) with a render
/// node, in PCI order, named as OpenVINO last named them (`known`, matched
/// by PCI address) or plainly until it has.
#[cfg(all(target_os = "linux", target_arch = "x86_64", target_env = "gnu"))]
fn intel_cards(drm: &std::path::Path, known: &[crate::openvino::Card]) -> Vec<Gpu> {
    let Ok(rd) = std::fs::read_dir(drm) else { return Vec::new() };
    let read = |p: std::path::PathBuf| std::fs::read_to_string(p).map(|t| t.trim().to_string()).unwrap_or_default();
    let mut found: Vec<(String, String)> = rd
        .flatten()
        .filter(|e| e.file_name().to_string_lossy().starts_with("renderD"))
        .filter_map(|e| {
            let dev = e.path().join("device");
            if read(dev.join("vendor")) != "0x8086" || !read(dev.join("class")).starts_with("0x03") {
                return None;
            }
            let driver = std::fs::read_link(dev.join("driver")).ok()?.file_name()?.to_string_lossy().into_owned();
            if driver != "i915" && driver != "xe" {
                return None;
            }
            let bus = std::fs::canonicalize(&dev).ok()?.file_name()?.to_string_lossy().into_owned();
            Some((bus, format!("/dev/dri/{}", e.file_name().to_string_lossy())))
        })
        .collect();
    found.sort();
    found
        .into_iter()
        .enumerate()
        .map(|(i, (bus, render))| {
            let k = known.iter().find(|k| k.bus.eq_ignore_ascii_case(&bus));
            let name = match k {
                Some(k) => k.name.clone(),
                None if built_in(&bus) => "Intel built-in graphics".to_string(),
                None => "Intel graphics card".to_string(),
            };
            Gpu { index: i as u32, name, memory_mb: k.map_or(0, |k| k.memory_mb), bus, api: "OpenVINO", render }
        })
        .collect()
}

/// Where the PCI database that names chips may be (the first that exists).
#[cfg(all(target_os = "linux", target_arch = "x86_64", target_env = "gnu"))]
const PCI_IDS: [&str; 3] = ["/usr/share/hwdata/pci.ids", "/usr/share/misc/pci.ids", "/usr/share/pci.ids"];

/// Where AMD's own list of product names may be (libdrm's; the first that
/// exists). It names chips by device and revision ("AMD Radeon 780M
/// Graphics"), where the PCI database often has only a code name.
#[cfg(all(target_os = "linux", target_arch = "x86_64", target_env = "gnu"))]
const AMDGPU_IDS: [&str; 2] = ["/usr/share/libdrm/amdgpu.ids", "/opt/amdgpu/share/libdrm/amdgpu.ids"];

/// The AMD graphics the kernel runs (amdgpu driver) with a render node, in
/// PCI order. The name is the product's in AMD's list (`amdgpu_ids`, by
/// device and revision), else the chip's in the PCI database (`pci_ids`, the
/// first file there), else "AMD graphics" (a name that does not change, so
/// the one chosen in Settings stays the one; Settings shows the name the
/// graphics driver gives while the AI runs on it). Video memory is what the driver reports (an integrated
/// chip's is the part of main memory set aside for it).
#[cfg(all(target_os = "linux", target_arch = "x86_64", target_env = "gnu"))]
fn amd_cards(drm: &std::path::Path, amdgpu_ids: &[&std::path::Path], pci_ids: &[&std::path::Path]) -> Vec<Gpu> {
    let Ok(rd) = std::fs::read_dir(drm) else { return Vec::new() };
    let read = |p: std::path::PathBuf| std::fs::read_to_string(p).map(|t| t.trim().to_string()).unwrap_or_default();
    let mut found: Vec<(String, String, String, String, u64)> = rd
        .flatten()
        .filter(|e| e.file_name().to_string_lossy().starts_with("renderD"))
        .filter_map(|e| {
            let dev = e.path().join("device");
            if read(dev.join("vendor")) != "0x1002" || !read(dev.join("class")).starts_with("0x03") {
                return None;
            }
            let driver = std::fs::read_link(dev.join("driver")).ok()?.file_name()?.to_string_lossy().into_owned();
            if driver != "amdgpu" {
                return None;
            }
            let bus = std::fs::canonicalize(&dev).ok()?.file_name()?.to_string_lossy().into_owned();
            let memory_mb = read(dev.join("mem_info_vram_total")).parse::<u64>().unwrap_or(0) / (1024 * 1024);
            Some((bus, format!("/dev/dri/{}", e.file_name().to_string_lossy()), read(dev.join("device")), read(dev.join("revision")), memory_mb))
        })
        .collect();
    found.sort();
    found
        .into_iter()
        .enumerate()
        .map(|(i, (bus, render, id, revision, memory_mb))| {
            let name = amdgpu_name(amdgpu_ids, &id, &revision)
                .or_else(|| pci_name(pci_ids, &id))
                .unwrap_or_else(|| "AMD graphics".to_string());
            Gpu { index: i as u32, name, memory_mb, bus, api: "WebGPU", render }
        })
        .collect()
}

/// An AMD product's name from libdrm's amdgpu.ids: lines of
/// "15BF,\tC1,\tAMD Radeon 780M Graphics" (device, revision, name, in hex);
/// "#" lines are comments and the first other line is the file's version.
/// The name gets "AMD " in front if it does not start with it.
#[cfg(all(target_os = "linux", target_arch = "x86_64", target_env = "gnu"))]
fn amdgpu_name(files: &[&std::path::Path], device: &str, revision: &str) -> Option<String> {
    let hex = |s: &str| u32::from_str_radix(s.trim().trim_start_matches("0x").trim_start_matches("0X"), 16).ok();
    let (dev, rev) = (hex(device)?, hex(revision)?);
    let text = files.iter().find_map(|f| std::fs::read_to_string(f).ok())?;
    text.lines().filter(|l| !l.trim_start().starts_with('#')).find_map(|l| {
        let mut parts = l.splitn(3, ',');
        let (d, r, name) = (parts.next()?, parts.next()?, parts.next()?.trim());
        if hex(d)? != dev || hex(r)? != rev || name.is_empty() || name.chars().any(char::is_control) {
            return None;
        }
        let name: String = name.chars().take(NAME_MAX).collect();
        let name = name.trim_end();
        Some(if name.starts_with("AMD ") { name.to_string() } else { format!("AMD {name}") })
    })
}

/// The longest AMD product name kept from a name file (characters).
#[cfg(all(target_os = "linux", target_arch = "x86_64", target_env = "gnu"))]
const NAME_MAX: usize = 100;

/// An AMD chip's name from the PCI database: its entry under vendor 1002
/// ("<tab>1638  Cezanne [Radeon Vega Series / Radeon Vega Mobile Series]"),
/// the part in brackets when there is one, as "AMD <that>".
#[cfg(all(target_os = "linux", target_arch = "x86_64", target_env = "gnu"))]
fn pci_name(files: &[&std::path::Path], device: &str) -> Option<String> {
    let id = device.trim().strip_prefix("0x")?.to_ascii_lowercase();
    if id.len() != 4 || !id.chars().all(|c| c.is_ascii_hexdigit()) {
        return None;
    }
    let text = files.iter().find_map(|f| std::fs::read_to_string(f).ok())?;
    let mut in_amd = false;
    for l in text.lines() {
        if !l.starts_with('\t') && !l.starts_with('#') {
            in_amd = l.starts_with("1002 ");
            continue;
        }
        if in_amd && l.starts_with('\t') && !l.starts_with("\t\t") {
            if let Some((i, name)) = l.trim_start().split_once("  ") {
                if i.eq_ignore_ascii_case(&id) {
                    let name = name.trim();
                    let short = match (name.find('['), name.rfind(']')) {
                        (Some(a), Some(b)) if b > a + 1 => name[a + 1..b].trim(),
                        _ => name,
                    };
                    // (Shown in Settings and the log: one plain line, as from amdgpu_name.)
                    if short.is_empty() || short.chars().any(char::is_control) {
                        return None;
                    }
                    let short: String = short.chars().take(NAME_MAX).collect();
                    return Some(format!("AMD {}", short.trim_end()));
                }
            }
        }
    }
    None
}

/// Intel graphics built into the processor (always at PCI 00:02.0).
pub(crate) fn built_in(bus: &str) -> bool {
    bus.ends_with(":00:02.0")
}

/// The "Model:" line of /proc/driver/nvidia/gpus/<bus>/information.
#[cfg_attr(not(all(target_os = "linux", target_arch = "x86_64", target_env = "gnu")), allow(dead_code))]
fn proc_model(text: &str) -> Option<String> {
    text.lines().find_map(|l| l.strip_prefix("Model:")).map(|m| m.trim().to_string()).filter(|m| !m.is_empty())
}

/// Cards of the same model get their number after the name, so each can be
/// chosen ("NVIDIA GeForce RTX 3060 #1").
#[cfg_attr(not(all(target_os = "linux", target_arch = "x86_64", target_env = "gnu")), allow(dead_code))]
pub(crate) fn number_same_names(v: &mut [Gpu]) {
    let names: Vec<String> = v.iter().map(|g| g.name.clone()).collect();
    for g in v.iter_mut() {
        if names.iter().filter(|n| **n == g.name).count() > 1 {
            g.name = format!("{} #{}", g.name, g.index);
        }
    }
}

/// Apple silicon: the chip itself ("Apple M2"), which Core ML runs on (its
/// Neural Engine and GPU share the computer's memory).
#[cfg(all(target_os = "macos", target_arch = "aarch64"))]
pub fn list() -> Vec<Gpu> {
    let name = std::process::Command::new("sysctl")
        .args(["-n", "machdep.cpu.brand_string"])
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .filter(|n| !n.is_empty())
        .unwrap_or_else(|| "Apple silicon".to_string());
    vec![Gpu { index: 0, name, memory_mb: 0, bus: String::new(), api: "Core ML", render: String::new() }]
}

#[cfg(not(any(windows, all(target_os = "linux", target_arch = "x86_64", target_env = "gnu"), all(target_os = "macos", target_arch = "aarch64"))))]
pub fn list() -> Vec<Gpu> {
    Vec::new()
}

/// The card to use: the one named `want` when it is there, else an NVIDIA
/// card over an Intel one over an AMD one (Linux), and then the one with the most video
/// memory (a separate graphics card over built-in graphics; Intel cards
/// have no size until OpenVINO has looked at them, so a card not built
/// into the processor comes first there).
#[cfg_attr(all(target_os = "macos", target_arch = "x86_64"), allow(dead_code))]
pub fn pick(gpus: &[Gpu], want: Option<&str>) -> Option<Gpu> {
    if let Some(w) = want.filter(|w| !w.is_empty()) {
        if let Some(g) = gpus.iter().find(|g| g.name == w) {
            return Some(g.clone());
        }
    }
    gpus.iter().max_by_key(|g| (rank(g.api), g.memory_mb, !built_in(&g.bus), std::cmp::Reverse(g.index))).cloned()
}

/// Which kind of card comes first when the owner has not chosen (Linux has
/// all three; the other systems have one kind).
pub(crate) fn rank(api: &str) -> u8 {
    match api {
        "CUDA" => 2,
        "OpenVINO" => 1,
        _ => 0,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn named_card_else_the_biggest() {
        let g = |index, name: &str, memory_mb| Gpu { index, name: name.into(), memory_mb, bus: String::new(), api: "DirectML", render: String::new() };
        let gpus = vec![g(0, "Intel(R) UHD Graphics", 0), g(1, "Intel(R) Arc(TM) B580", 12000), g(2, "AMD Radeon", 8000)];
        assert_eq!(pick(&gpus, None).unwrap().index, 1);
        assert_eq!(pick(&gpus, Some("AMD Radeon")).unwrap().index, 2);
        assert_eq!(pick(&gpus, Some("gone")).unwrap().index, 1, "a card no longer there: the biggest");
        assert!(pick(&[], None).is_none());
    }

    #[test]
    fn nvidia_smi_lines() {
        let g = parse_nvidia_smi("0, NVIDIA GeForce RTX 5060 Ti, 16311, 00000000:01:00.0\n1, NVIDIA GeForce RTX 3060, 12288, 00000000:02:00.0\n\n");
        assert_eq!(g.len(), 2);
        assert_eq!((g[0].index, g[0].name.as_str(), g[0].memory_mb, g[0].bus.as_str()), (0, "NVIDIA GeForce RTX 5060 Ti", 16311, "00000000:01:00.0"));
        assert_eq!((g[1].index, g[1].memory_mb), (1, 12288));
        assert_eq!(pick(&g, None).unwrap().index, 0);
        assert_eq!(pick(&g, Some("NVIDIA GeForce RTX 3060")).unwrap().index, 1);
        // Memory not known ("[N/A]") still lists the card; other lines are skipped.
        let g = parse_nvidia_smi("0, NVIDIA Tesla T4, [N/A]\nNVIDIA-SMI has failed because it couldn't communicate with the NVIDIA driver.\n");
        assert_eq!(g.len(), 1);
        assert_eq!((g[0].name.as_str(), g[0].memory_mb, g[0].bus.as_str()), ("NVIDIA Tesla T4", 0, ""));
        assert!(parse_nvidia_smi("").is_empty());
    }

    #[test]
    fn same_model_cards_get_numbers() {
        let mut g = parse_nvidia_smi("0, NVIDIA GeForce RTX 5060 Ti, 16311, a:1\n1, NVIDIA GeForce RTX 5060 Ti, 16311, a:2\n2, NVIDIA T400, 2048, a:3\n");
        number_same_names(&mut g);
        let names: Vec<&str> = g.iter().map(|g| g.name.as_str()).collect();
        assert_eq!(names, ["NVIDIA GeForce RTX 5060 Ti #0", "NVIDIA GeForce RTX 5060 Ti #1", "NVIDIA T400"]);
        assert_eq!(pick(&g, Some("NVIDIA GeForce RTX 5060 Ti #1")).unwrap().index, 1);
    }

    #[cfg(all(target_os = "linux", target_arch = "x86_64", target_env = "gnu"))]
    #[test]
    fn nvidia_before_intel_and_intel_cards_from_the_kernel() {
        let mut v = parse_nvidia_smi("0, NVIDIA T400, 2048, 00000000:01:00.0\n");
        // A fake /sys/class/drm: an Intel card (xe), the processor's Intel
        // graphics (i915), an AMD card and an Intel card on another driver.
        let d = std::env::temp_dir().join(format!("ambersight-drm-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        let card = |node: &str, bus: &str, vendor: &str, driver: &str| {
            let dev = d.join("devices").join(bus);
            std::fs::create_dir_all(&dev).unwrap();
            std::fs::create_dir_all(d.join("drivers").join(driver)).unwrap();
            std::fs::write(dev.join("vendor"), format!("{vendor}\n")).unwrap();
            std::fs::write(dev.join("class"), "0x030000\n").unwrap();
            std::os::unix::fs::symlink(d.join("drivers").join(driver), dev.join("driver")).unwrap();
            std::fs::create_dir_all(d.join("drm").join(node)).unwrap();
            std::os::unix::fs::symlink(&dev, d.join("drm").join(node).join("device")).unwrap();
        };
        card("renderD128", "0000:03:00.0", "0x8086", "xe");
        card("renderD129", "0000:00:02.0", "0x8086", "i915");
        card("renderD130", "0000:13:00.0", "0x1002", "amdgpu");
        card("renderD131", "0000:05:00.0", "0x8086", "vfio-pci");
        let intel = intel_cards(&d.join("drm"), &[]);
        assert_eq!(intel.iter().map(|g| (g.name.as_str(), g.bus.as_str(), g.render.as_str())).collect::<Vec<_>>(), [
            ("Intel built-in graphics", "0000:00:02.0", "/dev/dri/renderD129"),
            ("Intel graphics card", "0000:03:00.0", "/dev/dri/renderD128"),
        ]);
        assert!(intel.iter().all(|g| g.api == "OpenVINO"));
        // Not sized yet: the separate card over the built-in graphics.
        assert_eq!(pick(&intel, None).unwrap().bus, "0000:03:00.0");
        // Named as OpenVINO found them.
        let known = [crate::openvino::Card { id: "GPU.1".into(), name: "Intel(R) Arc(TM) B580 Graphics".into(), bus: "0000:03:00.0".into(), memory_mb: 12000, integrated: false }];
        let named = intel_cards(&d.join("drm"), &known);
        assert_eq!((named[1].name.as_str(), named[1].memory_mb), ("Intel(R) Arc(TM) B580 Graphics", 12000));
        // An NVIDIA card goes first, even with less memory; a named card wins.
        v.extend(named);
        assert_eq!(pick(&v, None).unwrap().api, "CUDA");
        assert_eq!(pick(&v, Some("Intel(R) Arc(TM) B580 Graphics")).unwrap().api, "OpenVINO");
        let _ = std::fs::remove_dir_all(&d);
    }

    #[cfg(all(target_os = "linux", target_arch = "x86_64", target_env = "gnu"))]
    #[test]
    fn amd_cards_from_the_kernel_come_after_nvidia_and_intel() {
        let d = std::env::temp_dir().join(format!("ambersight-drm-amd-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        let card = |node: &str, bus: &str, vendor: &str, driver: &str, id: &str, vram: Option<&str>| {
            let rev = if id == "0x15bf" { "0xc1" } else { "0x00" };
            let dev = d.join("devices").join(bus);
            std::fs::create_dir_all(&dev).unwrap();
            std::fs::create_dir_all(d.join("drivers").join(driver)).unwrap();
            std::fs::write(dev.join("vendor"), format!("{vendor}\n")).unwrap();
            std::fs::write(dev.join("device"), format!("{id}\n")).unwrap();
            std::fs::write(dev.join("class"), "0x030000\n").unwrap();
            std::fs::write(dev.join("revision"), format!("{rev}\n")).unwrap();
            if let Some(v) = vram {
                std::fs::write(dev.join("mem_info_vram_total"), format!("{v}\n")).unwrap();
            }
            std::os::unix::fs::symlink(d.join("drivers").join(driver), dev.join("driver")).unwrap();
            std::fs::create_dir_all(d.join("drm").join(node)).unwrap();
            std::os::unix::fs::symlink(&dev, d.join("drm").join(node).join("device")).unwrap();
        };
        card("renderD128", "0000:0c:00.0", "0x1002", "amdgpu", "0x1638", Some("536870912"));
        card("renderD129", "0000:00:02.0", "0x8086", "i915", "0x46a6", None);
        card("renderD130", "0000:05:00.0", "0x1002", "vfio-pci", "0x73bf", None);
        card("renderD131", "0000:06:00.0", "0x10de", "nvidia", "0x2504", None);
        let ids = d.join("pci.ids");
        std::fs::write(
            &ids,
            "# list\n8086  Intel Corporation\n\t46a6  AlderLake-P GT2 [Iris Xe Graphics]\n1002  Advanced Micro Devices, Inc. [AMD/ATI]\n\t1638  Cezanne [Radeon Vega Series / Radeon Vega Mobile Series]\n\t\t1002 1638  sub\n\t73bf  Navi 21 [Radeon RX 6800/6800 XT / 6900 XT]\n",
        )
        .unwrap();
        let nowhere = d.join("nowhere");
        let amd = amd_cards(&d.join("drm"), &[], &[nowhere.as_path(), ids.as_path()]);
        assert_eq!(amd.len(), 1, "only an amdgpu card counts");
        assert_eq!(
            (amd[0].name.as_str(), amd[0].api, amd[0].render.as_str(), amd[0].bus.as_str(), amd[0].memory_mb),
            ("AMD Radeon Vega Series / Radeon Vega Mobile Series", "WebGPU", "/dev/dri/renderD128", "0000:0c:00.0", 512)
        );
        // No database: a plain name.
        assert_eq!(amd_cards(&d.join("drm"), &[], &[])[0].name, "AMD graphics");
        // AMD's own list wins over the PCI database's code name, by device and revision.
        card("renderD132", "0000:0d:00.0", "0x1002", "amdgpu", "0x15bf", None);
        let amd_ids = d.join("amdgpu.ids");
        std::fs::write(
            &amd_ids,
            "# list\n1.0.0\n1638,\t00,\tAMD Radeon Graphics\n15BF,\t00,\tAMD Radeon 780M Graphics\n15BF,\tC1,\tRadeon 780M Graphics\n",
        )
        .unwrap();
        std::fs::write(&ids, format!("{}\t15bf  Phoenix1\n", std::fs::read_to_string(&ids).unwrap())).unwrap();
        let named = amd_cards(&d.join("drm"), &[nowhere.as_path(), amd_ids.as_path()], &[ids.as_path()]);
        // 1638 is revision 00 here (in the list); 15bf is c1, named without "AMD ".
        assert_eq!(
            named.iter().map(|g| g.name.as_str()).collect::<Vec<_>>(),
            ["AMD Radeon Graphics", "AMD Radeon 780M Graphics"]
        );
        // Not in AMD's list: the PCI database's name.
        std::fs::write(&amd_ids, "1.0.0\n15BF,\t00,\tAMD Radeon 780M Graphics\n").unwrap();
        let named = amd_cards(&d.join("drm"), &[amd_ids.as_path()], &[ids.as_path()]);
        assert_eq!(named[1].name, "AMD Phoenix1");
        std::fs::remove_dir_all(d.join("drm").join("renderD132")).unwrap();
        // Order: NVIDIA, then Intel, then AMD, unless one is named; AMD alone is used.
        let mut all = parse_nvidia_smi("0, NVIDIA T400, 2048, 00000000:06:00.0\n");
        all.extend(intel_cards(&d.join("drm"), &[]));
        all.extend(amd.clone());
        assert_eq!(pick(&all, None).unwrap().api, "CUDA");
        assert_eq!(pick(&all[1..], None).unwrap().api, "OpenVINO");
        assert_eq!(pick(&all[2..], None).unwrap().api, "WebGPU");
        assert_eq!(pick(&all, Some("AMD Radeon Vega Series / Radeon Vega Mobile Series")).unwrap().api, "WebGPU");
        // A discrete AMD card with lots of memory still comes after Intel graphics.
        let mut big = amd[0].clone();
        big.memory_mb = 16384;
        let mut mixed = intel_cards(&d.join("drm"), &[]);
        mixed.push(big);
        assert_eq!(pick(&mixed, None).unwrap().api, "OpenVINO");
        let _ = std::fs::remove_dir_all(&d);
    }

    #[cfg(all(target_os = "linux", target_arch = "x86_64", target_env = "gnu"))]
    #[test]
    fn amd_names_with_control_characters_are_refused_and_long_ones_cut() {
        let d = std::env::temp_dir().join(format!("ambersight-amd-names-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        let (ids, amd_ids) = (d.join("pci.ids"), d.join("amdgpu.ids"));
        let long = "R".repeat(500);
        std::fs::write(&ids, format!("1002  AMD\n\t1638  Cezanne [Radeon\x1b[31m Vega]\n\t1639  Bad\rname\n\t163a  {long}\n\t163b  [ ]\n")).unwrap();
        let pci = |id: &str| pci_name(&[ids.as_path()], id);
        assert_eq!(pci("0x1638"), None, "an escape sequence");
        assert_eq!(pci("0x1639"), None, "a carriage return");
        assert_eq!(pci("0x163a").unwrap().chars().count(), 4 + NAME_MAX);
        assert_eq!(pci("0x163b"), None, "nothing in the brackets");
        std::fs::write(&amd_ids, format!("1.0.0\n1638,\t00,\t{long}\n")).unwrap();
        assert_eq!(amdgpu_name(&[amd_ids.as_path()], "0x1638", "0x00").unwrap().chars().count(), 4 + NAME_MAX);
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn driver_model_line() {
        let text = "Model: \t\t NVIDIA GeForce RTX 5060 Ti\nIRQ:   \t\t 180\nBus Location: \t 0000:01:00.0\n";
        assert_eq!(proc_model(text).as_deref(), Some("NVIDIA GeForce RTX 5060 Ti"));
        assert_eq!(proc_model("IRQ: 1\n"), None);
    }
}
