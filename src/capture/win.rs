//! Windows-only pieces: monitor/window enumeration, console, DPI, the Run key,
//! and the job object that ties ffmpeg to the helper.

use super::proto::{MonitorInfo, WindowInfo};
use windows_sys::Win32::Foundation::{CloseHandle, BOOL, HWND, LPARAM, RECT};
use windows_sys::Win32::Graphics::Gdi::{EnumDisplayMonitors, GetMonitorInfoW, HDC, HMONITOR, MONITORINFO, MONITORINFOEXW};
use windows_sys::Win32::UI::WindowsAndMessaging as wm;

pub fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

fn from_wide(b: &[u16]) -> String {
    let end = b.iter().position(|&c| c == 0).unwrap_or(b.len());
    String::from_utf16_lossy(&b[..end])
}

/// Report physical pixels (what ddagrab/gdigrab capture), not scaled ones.
pub fn dpi_aware() {
    use windows_sys::Win32::UI::HiDpi::{SetProcessDpiAwarenessContext, DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2};
    unsafe {
        SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
    }
}

pub fn monitors() -> Vec<MonitorInfo> {
    unsafe extern "system" fn cb(h: HMONITOR, _dc: HDC, _r: *mut RECT, data: LPARAM) -> BOOL {
        let out = &mut *(data as *mut Vec<MonitorInfo>);
        let mut mi: MONITORINFOEXW = std::mem::zeroed();
        mi.monitorInfo.cbSize = std::mem::size_of::<MONITORINFOEXW>() as u32;
        if GetMonitorInfoW(h, &mut mi as *mut MONITORINFOEXW as *mut MONITORINFO) != 0 {
            let r = mi.monitorInfo.rcMonitor;
            out.push(MonitorInfo {
                index: out.len() as u32,
                name: from_wide(&mi.szDevice).trim_start_matches(r"\\.\").to_string(),
                x: r.left,
                y: r.top,
                w: (r.right - r.left).max(0) as u32,
                h: (r.bottom - r.top).max(0) as u32,
                primary: mi.monitorInfo.dwFlags & wm::MONITORINFOF_PRIMARY != 0,
            });
        }
        1
    }
    let mut out: Vec<MonitorInfo> = Vec::new();
    unsafe {
        EnumDisplayMonitors(0, std::ptr::null(), Some(cb), &mut out as *mut _ as LPARAM);
    }
    out
}

/// Visible, titled, top-level application windows.
pub fn windows() -> Vec<WindowInfo> {
    unsafe extern "system" fn cb(h: HWND, data: LPARAM) -> BOOL {
        let out = &mut *(data as *mut Vec<WindowInfo>);
        if let Some(w) = window_info(h) {
            out.push(w);
        }
        1
    }
    let mut out: Vec<WindowInfo> = Vec::new();
    unsafe {
        wm::EnumWindows(Some(cb), &mut out as *mut _ as LPARAM);
    }
    out
}

unsafe fn window_info(h: HWND) -> Option<WindowInfo> {
    use windows_sys::Win32::Graphics::Dwm::{DwmGetWindowAttribute, DWMWA_CLOAKED};
    if wm::IsWindowVisible(h) == 0 || wm::GetWindow(h, wm::GW_OWNER) != 0 {
        return None;
    }
    let ex = wm::GetWindowLongPtrW(h, wm::GWL_EXSTYLE) as u32;
    if ex & wm::WS_EX_TOOLWINDOW != 0 {
        return None;
    }
    // Hidden store apps and windows on other virtual desktops are "cloaked".
    let mut cloaked: u32 = 0;
    if DwmGetWindowAttribute(h, DWMWA_CLOAKED as u32, &mut cloaked as *mut u32 as *mut _, 4) == 0 && cloaked != 0 {
        return None;
    }
    let mut buf = [0u16; 512];
    let n = wm::GetWindowTextW(h, buf.as_mut_ptr(), buf.len() as i32);
    if n <= 0 {
        return None;
    }
    let title = String::from_utf16_lossy(&buf[..n as usize]);
    if title.trim().is_empty() {
        return None;
    }
    let mut r: RECT = std::mem::zeroed();
    wm::GetWindowRect(h, &mut r);
    let (w, hh) = ((r.right - r.left).max(0) as u32, (r.bottom - r.top).max(0) as u32);
    let mut pid = 0u32;
    wm::GetWindowThreadProcessId(h, &mut pid);
    let exe = exe_name(pid).unwrap_or_default();
    if exe.eq_ignore_ascii_case("explorer.exe") && title == "Program Manager" {
        return None;
    }
    Some(WindowInfo { title, exe, w, h: hh, xid: Some(h as u64) })
}

/// (the DPI a window draws at, its screen's DPI) when the window draws
/// smaller than it shows: an older program the system scales up. Such a
/// window's own picture (what gdigrab copies) is the smaller one.
pub fn drawn_scale(xid: u64) -> Option<(u32, u32)> {
    use windows_sys::Win32::Graphics::Gdi::{MonitorFromWindow, MONITOR_DEFAULTTONEAREST};
    use windows_sys::Win32::UI::HiDpi::{GetDpiForMonitor, GetDpiForWindow, MDT_EFFECTIVE_DPI};
    let h = xid as HWND;
    unsafe {
        let win = GetDpiForWindow(h);
        let mon = MonitorFromWindow(h, MONITOR_DEFAULTTONEAREST);
        let (mut x, mut y) = (0u32, 0u32);
        if win == 0 || mon == 0 || GetDpiForMonitor(mon, MDT_EFFECTIVE_DPI, &mut x, &mut y) != 0 {
            return None;
        }
        (x > win).then_some((win, x))
    }
}

fn exe_name(pid: u32) -> Option<String> {
    use windows_sys::Win32::System::Threading::{OpenProcess, QueryFullProcessImageNameW, PROCESS_NAME_WIN32, PROCESS_QUERY_LIMITED_INFORMATION};
    unsafe {
        let h = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid);
        if h == 0 {
            return None;
        }
        let mut buf = [0u16; 1024];
        let mut len = buf.len() as u32;
        let ok = QueryFullProcessImageNameW(h, PROCESS_NAME_WIN32, buf.as_mut_ptr(), &mut len);
        CloseHandle(h);
        if ok == 0 {
            return None;
        }
        let path = String::from_utf16_lossy(&buf[..len as usize]);
        path.rsplit(['\\', '/']).next().map(str::to_string)
    }
}

/// Set on a helper started without a console window (see `hide_console`).
pub const HIDDEN_ENV: &str = "AMBERSIGHT_CAPTURE_HIDDEN";

/// The exe is a console program, so Windows opens a console window for it
/// when it starts at sign-in or from Explorer/Win+R. In that case (the helper
/// is the only process on its console) let go of the console, which closes
/// the window, and carry on as the same process. (An earlier version started
/// a hidden copy of itself instead; that copy could be killed along with the
/// launcher's job.) Started from a terminal, the shell shares the console and
/// nothing changes.
pub fn hide_console() {
    use windows_sys::Win32::Storage::FileSystem::{CreateFileW, FILE_GENERIC_READ, FILE_GENERIC_WRITE, FILE_SHARE_READ, FILE_SHARE_WRITE, OPEN_EXISTING};
    use windows_sys::Win32::System::Console::{FreeConsole, GetConsoleProcessList, SetStdHandle, STD_ERROR_HANDLE, STD_INPUT_HANDLE, STD_OUTPUT_HANDLE};
    if std::env::var_os(HIDDEN_ENV).is_some() {
        return;
    }
    let mut pids = [0u32; 4];
    if unsafe { GetConsoleProcessList(pids.as_mut_ptr(), pids.len() as u32) } != 1 {
        return;
    }
    unsafe {
        FreeConsole();
        // Point stdin/stdout/stderr at NUL so later prints go nowhere quietly.
        let nul = wide("NUL");
        let h = CreateFileW(nul.as_ptr(), FILE_GENERIC_READ | FILE_GENERIC_WRITE, FILE_SHARE_READ | FILE_SHARE_WRITE, std::ptr::null(), OPEN_EXISTING, 0, 0);
        if h != -1 {
            SetStdHandle(STD_INPUT_HANDLE, h);
            SetStdHandle(STD_OUTPUT_HANDLE, h);
            SetStdHandle(STD_ERROR_HANDLE, h);
        }
    }
    // Marks this process as hidden (shown in the log, inherited by nothing that matters).
    std::env::set_var(HIDDEN_ENV, "1");
    tracing::info!("capture: let go of the console window; continuing in the background");
}

// ------------------------------------------------------------ Run key

pub const RUN_KEY: &str = r"Software\Microsoft\Windows\CurrentVersion\Run";

pub fn run_key_set(name: &str, command: &str) -> anyhow::Result<()> {
    use windows_sys::Win32::System::Registry::{RegSetKeyValueW, HKEY_CURRENT_USER, REG_SZ};
    let data = wide(command);
    let rc = unsafe {
        RegSetKeyValueW(
            HKEY_CURRENT_USER,
            wide(RUN_KEY).as_ptr(),
            wide(name).as_ptr(),
            REG_SZ,
            data.as_ptr() as *const _,
            (data.len() * 2) as u32,
        )
    };
    anyhow::ensure!(rc == 0, "could not set the start-at-sign-in entry (error {rc})");
    Ok(())
}

pub fn run_key_get(name: &str) -> Option<String> {
    use windows_sys::Win32::System::Registry::{RegGetValueW, HKEY_CURRENT_USER, RRF_RT_REG_SZ};
    let mut buf = vec![0u16; 2048];
    let mut size = (buf.len() * 2) as u32;
    let rc = unsafe {
        RegGetValueW(
            HKEY_CURRENT_USER,
            wide(RUN_KEY).as_ptr(),
            wide(name).as_ptr(),
            RRF_RT_REG_SZ,
            std::ptr::null_mut(),
            buf.as_mut_ptr() as *mut _,
            &mut size,
        )
    };
    (rc == 0).then(|| from_wide(&buf))
}

pub fn run_key_delete(name: &str) -> anyhow::Result<()> {
    use windows_sys::Win32::System::Registry::{RegDeleteKeyValueW, HKEY_CURRENT_USER};
    let rc = unsafe { RegDeleteKeyValueW(HKEY_CURRENT_USER, wide(RUN_KEY).as_ptr(), wide(name).as_ptr()) };
    // 2 = not found: nothing to remove.
    anyhow::ensure!(rc == 0 || rc == 2, "could not remove the start-at-sign-in entry (error {rc})");
    Ok(())
}

// ------------------------------------------------------------ job object

/// Put a child in a job that kills it when the helper exits, however it exits.
pub fn adopt(child: &tokio::process::Child) {
    use std::sync::OnceLock;
    use windows_sys::Win32::System::JobObjects::*;
    static JOB: OnceLock<usize> = OnceLock::new();
    let job = *JOB.get_or_init(|| unsafe {
        let job = CreateJobObjectW(std::ptr::null(), std::ptr::null());
        if job == 0 {
            return 0;
        }
        let mut info: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = std::mem::zeroed();
        info.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
        SetInformationJobObject(
            job,
            JobObjectExtendedLimitInformation,
            &info as *const _ as *const _,
            std::mem::size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
        );
        job as usize
    });
    if let (true, Some(h)) = (job != 0, child.raw_handle()) {
        unsafe {
            AssignProcessToJobObject(job as isize, h as isize);
        }
    }
}
