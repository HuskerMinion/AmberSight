//! Graphics cards the built-in AI can run on (Windows: DirectML, which
//! numbers them the way DXGI lists them). Elsewhere the list is empty and the
//! AI runs on the CPU.

#[derive(Clone, Debug, serde::Serialize)]
pub struct Gpu {
    /// Its number for DirectML (DXGI adapter order).
    pub index: u32,
    pub name: String,
    /// Its own video memory, MB (0 for most integrated graphics).
    pub memory_mb: u64,
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
        out.push(Gpu { index: i, name: String::from_utf16_lossy(&d.Description[..len]).trim().to_string(), memory_mb: d.DedicatedVideoMemory as u64 / (1024 * 1024) });
    }
    out
}

#[cfg(not(windows))]
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
        let g = |index, name: &str, memory_mb| Gpu { index, name: name.into(), memory_mb };
        let gpus = vec![g(0, "Intel(R) UHD Graphics", 0), g(1, "Intel(R) Arc(TM) B580", 12000), g(2, "AMD Radeon", 8000)];
        assert_eq!(pick(&gpus, None).unwrap().index, 1);
        assert_eq!(pick(&gpus, Some("AMD Radeon")).unwrap().index, 2);
        assert_eq!(pick(&gpus, Some("gone")).unwrap().index, 1, "a card no longer there: the biggest");
        assert!(pick(&[], None).is_none());
    }
}
