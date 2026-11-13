//! Graphics cards the built-in AI can run on: Windows (DirectML, which
//! numbers them the way DXGI lists them), 64-bit Linux PCs (NVIDIA cards
//! with CUDA, as nvidia-smi lists them) and Apple silicon Macs (Core ML: one
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
    /// Linux: its PCI address ("00000000:01:00.0"), by which CUDA is asked
    /// for its own number for the card; empty elsewhere.
    #[serde(skip)]
    #[cfg_attr(not(all(target_os = "linux", target_arch = "x86_64", target_env = "gnu")), allow(dead_code))]
    pub bus: String,
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
        });
    }
    out
}

/// The NVIDIA cards (none without NVIDIA's driver). nvidia-smi also gives
/// their memory; without it, the driver's own list gives the names. Asked
/// at most once a minute (Settings asks on each visit); the cache is not
/// held while nvidia-smi runs.
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
            Some(Gpu { index, name, memory_mb, bus })
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
    cards.into_iter().enumerate().map(|(i, (bus, name))| Gpu { index: i as u32, name, memory_mb: 0, bus }).collect()
}

/// The "Model:" line of /proc/driver/nvidia/gpus/<bus>/information.
#[cfg_attr(not(all(target_os = "linux", target_arch = "x86_64", target_env = "gnu")), allow(dead_code))]
fn proc_model(text: &str) -> Option<String> {
    text.lines().find_map(|l| l.strip_prefix("Model:")).map(|m| m.trim().to_string()).filter(|m| !m.is_empty())
}

/// Cards of the same model get their number after the name, so each can be
/// chosen ("NVIDIA GeForce RTX 3060 #1").
#[cfg_attr(not(all(target_os = "linux", target_arch = "x86_64", target_env = "gnu")), allow(dead_code))]
fn number_same_names(v: &mut [Gpu]) {
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
    vec![Gpu { index: 0, name, memory_mb: 0, bus: String::new() }]
}

#[cfg(not(any(windows, all(target_os = "linux", target_arch = "x86_64", target_env = "gnu"), all(target_os = "macos", target_arch = "aarch64"))))]
pub fn list() -> Vec<Gpu> {
    Vec::new()
}

/// The card to use: the one named `want` when it is there, else the one with
/// the most video memory (a separate graphics card over built-in graphics).
#[cfg_attr(all(target_os = "macos", target_arch = "x86_64"), allow(dead_code))]
pub fn pick(gpus: &[Gpu], want: Option<&str>) -> Option<Gpu> {
    if let Some(w) = want.filter(|w| !w.is_empty()) {
        if let Some(g) = gpus.iter().find(|g| g.name == w) {
            return Some(g.clone());
        }
    }
    gpus.iter().max_by_key(|g| (g.memory_mb, std::cmp::Reverse(g.index))).cloned()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn named_card_else_the_biggest() {
        let g = |index, name: &str, memory_mb| Gpu { index, name: name.into(), memory_mb, bus: String::new() };
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

    #[test]
    fn driver_model_line() {
        let text = "Model: \t\t NVIDIA GeForce RTX 5060 Ti\nIRQ:   \t\t 180\nBus Location: \t 0000:01:00.0\n";
        assert_eq!(proc_model(text).as_deref(), Some("NVIDIA GeForce RTX 5060 Ti"));
        assert_eq!(proc_model("IRQ: 1\n"), None);
    }
}
