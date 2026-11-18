//! Intel graphics for the built-in AI on 64-bit Linux PCs: ONNX Runtime's
//! OpenVINO provider with OpenVINO's GPU plugin, from the Intel add-on (see
//! gpu_addon.rs), on Intel's compute driver (intel-opencl-icd), which the
//! owner installs.
//!
//! - The provider library needs OpenVINO's own libraries (libopenvino,
//!   its ONNX reader, oneTBB). They are loaded first from the add-on
//!   folder by their full paths, for the whole process (`preload`):
//!   the dynamic loader then finds them by name for the provider. OpenVINO
//!   loads its GPU plugin from beside its own library.
//! - The cards are found through OpenVINO's C API (libopenvino_c, in the
//!   add-on): their names, PCI addresses and memory, and OpenVINO's own
//!   number for each ("GPU.1"), so the card chosen in Settings is the one
//!   used. What was found is kept in `<data folder>/cache/openvino-<version>/
//!   devices.json` for Settings (`known_cards`).
//! - Compiled models are kept in the same folder (OpenVINO's `cache_dir`):
//!   the first start compiles the model for the card (seconds), later ones
//!   load it.
//! - Only OpenVINO's default precision for the card is used.

use std::ffi::{c_char, c_int, c_void, CStr, CString};
use std::path::{Path, PathBuf};
use std::sync::Mutex;

/// The OpenVINO version the add-on carries (ONNX Runtime 1.31's OpenVINO
/// provider is built against 2026.4.1), as its libraries' version suffix.
pub const SO_VERSION: &str = "2641";

/// The add-on's libraries: loaded by `preload` in this order (the first
/// is ONNX Runtime's bridge, the last its OpenVINO provider, both loaded
/// by ONNX Runtime itself).
pub const LIBS: [&str; 7] = [
    "libonnxruntime_providers_shared.so",
    "libtbb.so.12",
    "libopenvino.so.2641",
    "libopenvino_c.so.2641",
    "libopenvino_onnx_frontend.so.2641",
    "libopenvino_intel_gpu_plugin.so",
    "libonnxruntime_providers_openvino.so",
];

/// The ones `preload` loads (OpenVINO's own; the GPU plugin is loaded by OpenVINO).
const PRELOAD: [&str; 4] = ["libtbb.so.12", "libopenvino.so.2641", "libopenvino_c.so.2641", "libopenvino_onnx_frontend.so.2641"];

/// An Intel card as OpenVINO sees it.
#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct Card {
    /// OpenVINO's name for it ("GPU.0").
    pub id: String,
    /// "Intel(R) Arc(TM) B580 Graphics".
    pub name: String,
    /// "0000:03:00.0" (empty when OpenVINO does not say).
    pub bus: String,
    pub memory_mb: u64,
    /// Built into the processor (shares its memory).
    pub integrated: bool,
}

// ---------------------------------------------------------------------
// Direct submission off
// ---------------------------------------------------------------------

// Intel's compute driver ("NEO") by default lets the card read work from a
// ring in memory it keeps polling ("direct submission"). When a process
// dies by a signal while work is in flight, that memory is freed while the
// card still reads it, and the card faults (an Arc card on the xe driver,
// even at OpenVINO's default precision); after a fault the card's
// scheduler sometimes does not reset it, and the next job on it from any
// process hangs until that process is killed. With direct submission off
// (work goes through the kernel driver) a process killed mid-check left
// the card fine in every test, at about 0.3 ms more per check.
//
// The driver reads these settings from the environment when it starts, so
// they are set before main, while the program is still one thread and
// nothing is loaded: not when the owner set EnableDirectSubmission
// themselves. Child programs (ffmpeg) inherit them; they matter only to
// Intel's OpenCL / Level Zero driver, not to VA-API or Quick Sync decoding.
#[used]
#[link_section = ".init_array"]
static DIRECT_SUBMISSION_OFF: extern "C" fn() = direct_submission_off;

extern "C" fn direct_submission_off() {
    // SAFETY: runs before main, with no other thread; the strings are static C strings.
    unsafe {
        if libc::getenv(c"EnableDirectSubmission".as_ptr()).is_null() {
            libc::setenv(c"NEOReadDebugKeys".as_ptr(), c"1".as_ptr(), 1);
            libc::setenv(c"EnableDirectSubmission".as_ptr(), c"0".as_ptr(), 1);
        }
    }
}

// ---------------------------------------------------------------------
// The compute driver
// ---------------------------------------------------------------------

/// What this computer lacks for the AI on an Intel card, as a reason to show
/// (None: the compute driver is there and this user may use a card).
/// `render`: the Intel card's render node ("/dev/dri/renderD128").
pub fn missing_driver(render: &[String]) -> Option<String> {
    // OpenVINO's GPU plugin talks to the card through OpenCL.
    // SAFETY: a plain library load; kept loaded (the GPU plugin needs it).
    if unsafe { libc::dlopen(c"libOpenCL.so.1".as_ptr(), libc::RTLD_NOW | libc::RTLD_GLOBAL) }.is_null() {
        return Some(driver_missing(DRIVER_MISSING[0], crate::update::in_container()));
    }
    if !intel_icd_present() {
        return Some(driver_missing(&format!("{} (no Intel OpenCL driver in /etc/OpenCL/vendors)", DRIVER_MISSING[1]), crate::update::in_container()));
    }
    if !render.is_empty() && !render.iter().any(|n| may_use(n)) {
        let who = std::env::var("USER").ok().filter(|u| !u.is_empty()).unwrap_or_else(|| "the user AmberSight runs as".into());
        return Some(format!(
            "this user may not use the Intel card ({} needs the render group): add {who} to it (sudo usermod -aG render {who}) and restart AmberSight; in a container, pass the card in with --device /dev/dri and add the render group",
            render.join(", ")
        ));
    }
    None
}

/// Whether `why` (from `missing_driver`) is the compute driver missing.
pub fn is_driver_missing(why: &str) -> bool {
    why.starts_with(DRIVER_MISSING[0]) || why.starts_with(DRIVER_MISSING[1])
}

const DRIVER_MISSING: [&str; 2] = ["libOpenCL.so.1 not found", "Intel's compute driver is not installed"];

/// What to do about a missing compute driver: in a container, use the image
/// that has it; else install it.
fn driver_missing(what: &str, container: bool) -> String {
    if container {
        format!("{what}: this image has no Intel compute driver; use the ghcr.io/huskerminion/ambersight:openvino image (with --device /dev/dri) for the AI on Intel graphics")
    } else {
        format!("{what}: install Intel's compute driver (Ubuntu 25.04 or newer: intel-opencl-icd; Fedora: intel-compute-runtime; Debian: Intel's compute-runtime packages), see the install guide")
    }
}

/// An OpenCL driver list entry naming Intel's driver (libigdrcl), in the
/// folder the OpenCL loader reads (OCL_ICD_VENDORS, else /etc/OpenCL/vendors).
fn intel_icd_present() -> bool {
    let dir = std::env::var_os("OCL_ICD_VENDORS").map(PathBuf::from).filter(|p| p.is_dir()).unwrap_or_else(|| PathBuf::from("/etc/OpenCL/vendors"));
    let Ok(rd) = std::fs::read_dir(&dir) else { return false };
    rd.flatten().any(|e| e.path().extension().is_some_and(|x| x == "icd") && std::fs::read_to_string(e.path()).is_ok_and(|t| icd_is_intel_gpu(&t)))
}

/// Whether an .icd file's text names Intel's GPU driver.
fn icd_is_intel_gpu(text: &str) -> bool {
    text.lines().any(|l| l.trim().rsplit('/').next().is_some_and(|f| f.starts_with("libigdrcl")))
}

fn may_use(node: &str) -> bool {
    let Ok(c) = CString::new(node) else { return false };
    // SAFETY: access(2) with a valid C string.
    unsafe { libc::access(c.as_ptr(), libc::R_OK | libc::W_OK) == 0 }
}

// ---------------------------------------------------------------------
// Loading OpenVINO
// ---------------------------------------------------------------------

static LOADED: Mutex<Option<PathBuf>> = Mutex::new(None);

/// Load OpenVINO's libraries from the add-on folder `dir` (once per
/// process; a library cannot be unloaded and loaded from elsewhere).
pub fn preload(dir: &Path) -> Result<(), String> {
    let mut g = LOADED.lock().unwrap_or_else(|e| e.into_inner());
    if let Some(d) = g.as_ref() {
        return if d == dir { Ok(()) } else { Err(format!("OpenVINO was already loaded from {} (restart AmberSight to use {})", d.display(), dir.display())) };
    }
    for l in PRELOAD {
        let p = CString::new(dir.join(l).as_os_str().as_encoded_bytes()).map_err(|_| format!("bad path {}", dir.display()))?;
        // SAFETY: loading a library by its full path; it stays loaded.
        let h = unsafe { libc::dlopen(p.as_ptr(), libc::RTLD_NOW | libc::RTLD_GLOBAL) };
        if h.is_null() {
            // SAFETY: dlerror's text is valid until the next dl call on this thread.
            let e = unsafe { CStr::from_ptr(libc::dlerror()) }.to_string_lossy().into_owned();
            return Err(format!("could not load {l} from the Intel graphics add-on: {}", short_dlerror(&e)));
        }
    }
    *g = Some(dir.to_path_buf());
    Ok(())
}

/// dlerror's text without the folder ("libOpenCL.so.1: cannot open shared object file").
fn short_dlerror(e: &str) -> String {
    match e.split_once(": ") {
        Some((path, rest)) if path.contains('/') => rest.to_string(),
        _ => e.to_string(),
    }
}

// The C API (openvino/c/ov_core.h), looked up in the loaded libopenvino_c.
type CoreCreate = unsafe extern "C" fn(*mut *mut c_void) -> c_int;
type CoreFree = unsafe extern "C" fn(*mut c_void);
type CoreGetProperty = unsafe extern "C" fn(*const c_void, *const c_char, *const c_char, *mut *mut c_char) -> c_int;
type Free = unsafe extern "C" fn(*const c_char);

struct Api {
    create: CoreCreate,
    free_core: CoreFree,
    property: CoreGetProperty,
    free: Free,
}

fn api() -> Result<Api, String> {
    let lib = CString::new(format!("libopenvino_c.so.{SO_VERSION}")).unwrap();
    // SAFETY: the library was loaded by `preload` (RTLD_NOLOAD: never from
    // anywhere else); the symbols are OpenVINO's documented C functions.
    unsafe {
        let h = libc::dlopen(lib.as_ptr(), libc::RTLD_NOW | libc::RTLD_NOLOAD);
        if h.is_null() {
            return Err("OpenVINO is not loaded".into());
        }
        let sym = |n: &CStr| {
            let p = libc::dlsym(h, n.as_ptr());
            if p.is_null() { Err(format!("OpenVINO has no {}", n.to_string_lossy())) } else { Ok(p) }
        };
        Ok(Api {
            create: std::mem::transmute::<*mut c_void, CoreCreate>(sym(c"ov_core_create")?),
            free_core: std::mem::transmute::<*mut c_void, CoreFree>(sym(c"ov_core_free")?),
            property: std::mem::transmute::<*mut c_void, CoreGetProperty>(sym(c"ov_core_get_property")?),
            free: std::mem::transmute::<*mut c_void, Free>(sym(c"ov_free")?),
        })
    }
}

/// The Intel cards OpenVINO's GPU plugin finds (after `preload`). Starts
/// the card's driver: call it where a stuck driver cannot hold anything up
/// (the AI loads cards on a thread of their own, with a time limit).
pub fn cards() -> Result<Vec<Card>, String> {
    let api = api()?;
    let mut core: *mut c_void = std::ptr::null_mut();
    // SAFETY: documented C API; the core is freed below.
    let st = unsafe { (api.create)(&mut core) };
    if st != 0 || core.is_null() {
        return Err(format!("OpenVINO could not start (status {st})"));
    }
    struct Core<'a>(&'a Api, *mut c_void);
    impl Drop for Core<'_> {
        fn drop(&mut self) {
            // SAFETY: made by ov_core_create, freed once.
            unsafe { (self.0.free_core)(self.1) }
        }
    }
    let core = Core(&api, core);
    let prop = |dev: &str, key: &str| -> Result<String, String> {
        let (d, k) = (CString::new(dev).unwrap(), CString::new(key).unwrap());
        let mut out: *mut c_char = std::ptr::null_mut();
        // SAFETY: documented C API; the returned text is freed with ov_free.
        let st = unsafe { (api.property)(core.1, d.as_ptr(), k.as_ptr(), &mut out) };
        if st != 0 || out.is_null() {
            return Err(format!("OpenVINO could not read {key} of {dev} (status {st})"));
        }
        // SAFETY: a C string from OpenVINO.
        let s = unsafe { CStr::from_ptr(out) }.to_string_lossy().into_owned();
        unsafe { (api.free)(out) };
        Ok(s)
    };
    // "0 1" (the GPU plugin's own list; no other plugin is asked).
    let ids = prop("GPU", "AVAILABLE_DEVICES").map_err(|_| "OpenVINO finds no Intel card (is Intel's compute driver installed, and may this user use the card?)".to_string())?;
    let mut out = Vec::new();
    for n in ids.split([' ', ',']).map(str::trim).filter(|n| !n.is_empty()) {
        let id = format!("GPU.{n}");
        let name = prop(&id, "FULL_DEVICE_NAME").unwrap_or_else(|_| "Intel graphics".into());
        let bus = prop(&id, "DEVICE_PCI_INFO").ok().and_then(|p| pci_address(&p)).unwrap_or_default();
        let memory_mb = prop(&id, "GPU_DEVICE_TOTAL_MEM_SIZE").ok().and_then(|m| m.trim().parse::<u64>().ok()).unwrap_or(0) >> 20;
        let integrated = prop(&id, "DEVICE_TYPE").is_ok_and(|t| t.to_ascii_lowercase().contains("integrated"));
        out.push(Card { id, name: clean_name(&name), bus, memory_mb: if integrated { 0 } else { memory_mb }, integrated });
    }
    if out.is_empty() {
        return Err("OpenVINO finds no Intel card".into());
    }
    Ok(out)
}

/// OpenVINO's PCI text ("{domain: 0 bus: 3 device: 0x0 function: 0}") as
/// "0000:03:00.0".
fn pci_address(text: &str) -> Option<String> {
    let num = |key: &str| -> Option<u32> {
        let i = text.find(&format!("{key}:"))?;
        let v = text[i + key.len() + 1..].trim_start().split(|c: char| c.is_whitespace() || c == '}' || c == ',').next()?;
        match v.strip_prefix("0x") {
            Some(h) => u32::from_str_radix(h, 16).ok(),
            None => v.parse().ok(),
        }
    };
    Some(format!("{:04x}:{:02x}:{:02x}.{:x}", num("domain")?, num("bus")?, num("device")?, num("function")?))
}

/// "Intel(R) Arc(TM) B580 Graphics (dGPU)" -> "Intel(R) Arc(TM) B580 Graphics".
fn clean_name(n: &str) -> String {
    let n = n.trim();
    n.strip_suffix("(dGPU)").or_else(|| n.strip_suffix("(iGPU)")).unwrap_or(n).trim().to_string()
}

// ---------------------------------------------------------------------
// Where its files go
// ---------------------------------------------------------------------

/// `<data folder>/cache/openvino-<version>`.
pub fn cache_dir(data_dir: &Path) -> PathBuf {
    data_dir.join("cache").join(format!("openvino-{}", crate::update::CURRENT))
}

/// The cards last found, from `devices.json` (empty when none were found
/// yet), for naming them before OpenVINO is loaded.
pub fn known_cards() -> Vec<Card> {
    let Some(d) = crate::gpu_addon::data_dir() else { return Vec::new() };
    std::fs::read(cache_dir(&d).join("devices.json")).ok().and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or_default()
}

/// Keep what `cards` found for `known_cards`; other versions' cache folders
/// are removed (their compiled models are of no use to this one).
pub fn remember(cards: &[Card]) {
    let Some(d) = crate::gpu_addon::data_dir() else { return };
    let dir = cache_dir(&d);
    if crate::gpu_addon::private_dir(&dir).is_err() {
        return;
    }
    if let Ok(j) = serde_json::to_vec_pretty(cards) {
        let tmp = dir.join("devices.json.tmp");
        if std::fs::write(&tmp, j).is_ok() {
            let _ = std::fs::rename(&tmp, dir.join("devices.json"));
        }
    }
    crate::gpu_addon::remove_other_versions(&d.join("cache"), "openvino-", &dir);
}

/// The provider's options: the card, and the cache of compiled models when
/// its folder is this user's own (a model compiled by someone else is
/// never loaded). Default precision.
pub fn provider_options(card_id: &str) -> Vec<(String, String)> {
    let mut o = vec![("device_type".to_string(), card_id.to_string())];
    if let Some(d) = crate::gpu_addon::data_dir() {
        let dir = cache_dir(&d);
        match crate::gpu_addon::private_dir(&dir) {
            Ok(()) => o.push(("cache_dir".into(), dir.to_string_lossy().into_owned())),
            Err(e) => tracing::warn!("AI: compiled models for the Intel card are not kept ({e}); each start compiles the model again"),
        }
    }
    o
}

/// Add the OpenVINO provider with `options` to a session being made.
pub fn append_provider(b: &mut ort::session::builder::SessionBuilder, options: &[(String, String)]) -> Result<(), String> {
    use ort::AsPointer;
    let ks: Vec<CString> = options.iter().map(|o| CString::new(o.0.as_str())).collect::<Result<_, _>>().map_err(|_| "bad OpenVINO option".to_string())?;
    let vs: Vec<CString> = options.iter().map(|o| CString::new(o.1.as_str())).collect::<Result<_, _>>().map_err(|_| "bad OpenVINO option".to_string())?;
    let kp: Vec<*const c_char> = ks.iter().map(|c| c.as_ptr()).collect();
    let vp: Vec<*const c_char> = vs.iter().map(|c| c.as_ptr()).collect();
    // SAFETY: ONNX Runtime's documented C function, with the session
    // options this builder owns and arrays of `len` valid C strings that
    // outlive the call (it copies them).
    let st = unsafe { (ort::api().SessionOptionsAppendExecutionProvider_OpenVINO_V2)(b.ptr_mut(), kp.as_ptr(), vp.as_ptr(), kp.len()) };
    if st.0.is_null() {
        return Ok(());
    }
    // SAFETY: a status from ONNX Runtime: its message, then freed.
    let msg = unsafe { CStr::from_ptr((ort::api().GetErrorMessage)(st.0)) }.to_string_lossy().into_owned();
    unsafe { (ort::api().ReleaseStatus)(st.0) };
    Err(msg)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn direct_submission_is_off_from_the_start() {
        // (Set before main in this test program too, unless the environment had it.)
        assert!(std::env::var_os("EnableDirectSubmission").is_some());
        if std::env::var("EnableDirectSubmission").as_deref() == Ok("0") {
            assert_eq!(std::env::var("NEOReadDebugKeys").as_deref(), Ok("1"));
        }
    }

    #[test]
    fn pci_text_becomes_an_address() {
        assert_eq!(pci_address("{domain: 0 bus: 3 device: 0x0 function: 0}").as_deref(), Some("0000:03:00.0"));
        assert_eq!(pci_address("{domain: 0 bus: 19 device: 0x2 function: 1}").as_deref(), Some("0000:13:02.1"));
        assert_eq!(pci_address("nothing"), None);
    }

    #[test]
    fn names_lose_the_kind() {
        assert_eq!(clean_name("Intel(R) Arc(TM) B580 Graphics (dGPU)"), "Intel(R) Arc(TM) B580 Graphics");
        assert_eq!(clean_name("Intel(R) UHD Graphics 770 (iGPU) "), "Intel(R) UHD Graphics 770");
        assert_eq!(clean_name("Intel(R) Graphics"), "Intel(R) Graphics");
    }

    #[test]
    fn a_missing_driver_says_what_to_do() {
        let host = driver_missing("libOpenCL.so.1 not found", false);
        assert!(host.contains("intel-opencl-icd") && host.contains("Ubuntu 25.04") && host.contains("intel-compute-runtime") && !host.contains("Debian 13"), "{host}");
        let docker = driver_missing("libOpenCL.so.1 not found", true);
        assert!(docker.contains(":openvino") && !docker.contains("apt"), "{docker}");
        // Told apart from other reasons (the log is quieter for it on built-in graphics).
        assert!(is_driver_missing(&host) && is_driver_missing(&driver_missing(DRIVER_MISSING[1], false)));
        assert!(!is_driver_missing("this user may not use the Intel card"));
    }

    #[test]
    fn intel_icd_files() {
        assert!(icd_is_intel_gpu("/usr/lib/x86_64-linux-gnu/intel-opencl/libigdrcl.so\n"));
        assert!(icd_is_intel_gpu("libigdrcl.so"));
        assert!(!icd_is_intel_gpu("/opt/intel/oneapi/compiler/latest/lib/libintelocl.so"));
        assert!(!icd_is_intel_gpu("libamdocl64.so"));
    }

    #[test]
    fn dlerror_without_the_folder() {
        assert_eq!(short_dlerror("/x/y/libopenvino.so.2641: cannot open shared object file"), "cannot open shared object file");
        assert_eq!(short_dlerror("libOpenCL.so.1: cannot open shared object file"), "libOpenCL.so.1: cannot open shared object file");
    }

    #[test]
    fn the_libraries_match_the_version() {
        for l in LIBS.iter().chain(PRELOAD.iter()) {
            if l.contains(".so.2") {
                assert!(l.ends_with(SO_VERSION), "{l}");
            }
        }
        assert!(PRELOAD.iter().all(|p| LIBS.contains(p)));
    }
}
