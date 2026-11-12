//! Windows notification-area icon shown while captures are wanted:
//! "AmberSight is recording this screen", with Pause all / Resume / Quit.
//! It runs on its own thread with a hidden window that receives the icon's
//! messages; the async side talks to it with posted messages.

use super::win::wide;
use std::cell::RefCell;
use tokio::sync::mpsc::UnboundedSender;
use windows_sys::Win32::Foundation::{HWND, LPARAM, LRESULT, POINT, WPARAM};
use windows_sys::Win32::UI::Shell::*;
use windows_sys::Win32::UI::WindowsAndMessaging::*;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TrayCmd {
    Pause,
    Resume,
    Quit,
}

const WM_UPDATE: u32 = WM_APP + 1;
const WM_ICON: u32 = WM_APP + 2;
const WM_CLOSE_TRAY: u32 = WM_APP + 3;
const NIN_KEYSELECT: u32 = NIN_SELECT | 1; // NIN_SELECT | NINF_KEY
const ID_PAUSE: usize = 1;
const ID_RESUME: usize = 2;
const ID_QUIT: usize = 3;
const TIP: &str = "AmberSight is recording this screen";

struct ThreadState {
    cmds: UnboundedSender<TrayCmd>,
    icon: HICON,
    added: bool,
    visible: bool,
    paused: bool,
    taskbar_created: u32,
}

thread_local! {
    static STATE: RefCell<Option<ThreadState>> = const { RefCell::new(None) };
}

pub struct Tray {
    hwnd: HWND,
}

impl Tray {
    /// Start the tray thread (the icon stays hidden until `update(true, ..)`).
    pub fn spawn(cmds: UnboundedSender<TrayCmd>) -> Option<Tray> {
        let (tx, rx) = std::sync::mpsc::channel::<HWND>();
        std::thread::Builder::new()
            .name("tray".into())
            .spawn(move || unsafe { thread_main(cmds, tx) })
            .ok()?;
        let hwnd = rx.recv().ok()?;
        (hwnd != 0).then_some(Tray { hwnd })
    }

    pub fn update(&self, visible: bool, paused: bool) {
        unsafe {
            PostMessageW(self.hwnd, WM_UPDATE, visible as usize, paused as isize);
        }
    }
}

impl Drop for Tray {
    fn drop(&mut self) {
        // Synchronous, so the icon is gone before the process exits.
        unsafe {
            SendMessageW(self.hwnd, WM_CLOSE_TRAY, 0, 0);
        }
    }
}

unsafe fn thread_main(cmds: UnboundedSender<TrayCmd>, ready: std::sync::mpsc::Sender<HWND>) {
    use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
    let hinst = GetModuleHandleW(std::ptr::null());
    let class = wide("AmberSightCaptureTray");
    let wc = WNDCLASSW {
        style: 0,
        lpfnWndProc: Some(wndproc),
        cbClsExtra: 0,
        cbWndExtra: 0,
        hInstance: hinst,
        hIcon: 0,
        hCursor: 0,
        hbrBackground: 0,
        lpszMenuName: std::ptr::null(),
        lpszClassName: class.as_ptr(),
    };
    RegisterClassW(&wc);
    let title = wide("AmberSight Capture");
    // A hidden ordinary window (never shown): popup menus need a real owner
    // window to close properly when the user clicks elsewhere.
    let hwnd = CreateWindowExW(WS_EX_TOOLWINDOW, class.as_ptr(), title.as_ptr(), WS_POPUP, 0, 0, 0, 0, 0, 0, hinst, std::ptr::null());
    if hwnd == 0 {
        let _ = ready.send(0);
        return;
    }
    STATE.with(|s| {
        *s.borrow_mut() = Some(ThreadState {
            cmds,
            icon: make_icon(hinst),
            added: false,
            visible: false,
            paused: false,
            taskbar_created: RegisterWindowMessageW(wide("TaskbarCreated").as_ptr()),
        })
    });
    let _ = ready.send(hwnd);
    let mut msg: MSG = std::mem::zeroed();
    while GetMessageW(&mut msg, 0, 0, 0) > 0 {
        TranslateMessage(&msg);
        DispatchMessageW(&msg);
    }
}

/// The app icon (embedded PNG) at 32x32, or the stock application icon.
unsafe fn make_icon(hinst: isize) -> HICON {
    const SIZE: u32 = 32;
    if let Ok(img) = image::load_from_memory(include_bytes!("../../static/icons/icon-192.png")) {
        let img = image::imageops::resize(&img.to_rgba8(), SIZE, SIZE, image::imageops::FilterType::Lanczos3);
        let mut bgra = Vec::with_capacity((SIZE * SIZE * 4) as usize);
        for p in img.pixels() {
            bgra.extend_from_slice(&[p[2], p[1], p[0], p[3]]);
        }
        let and_mask = vec![0u8; (SIZE * SIZE / 8) as usize];
        let icon = CreateIcon(hinst, SIZE as i32, SIZE as i32, 1, 32, and_mask.as_ptr(), bgra.as_ptr());
        if icon != 0 {
            return icon;
        }
    }
    LoadIconW(0, IDI_APPLICATION)
}

unsafe fn notify_data(hwnd: HWND, icon: HICON, paused: bool) -> NOTIFYICONDATAW {
    let mut nid: NOTIFYICONDATAW = std::mem::zeroed();
    nid.cbSize = std::mem::size_of::<NOTIFYICONDATAW>() as u32;
    nid.hWnd = hwnd;
    nid.uID = 1;
    nid.uFlags = NIF_MESSAGE | NIF_ICON | NIF_TIP | NIF_SHOWTIP;
    nid.uCallbackMessage = WM_ICON;
    nid.hIcon = icon;
    let tip = if paused { format!("{TIP} (paused)") } else { TIP.to_string() };
    for (i, c) in tip.encode_utf16().take(nid.szTip.len() - 1).enumerate() {
        nid.szTip[i] = c;
    }
    nid
}

/// Add, update or remove the icon to match the wanted state.
unsafe fn sync_icon(hwnd: HWND, st: &mut ThreadState, force_add: bool) {
    let mut nid = notify_data(hwnd, st.icon, st.paused);
    if st.visible {
        if !st.added || force_add {
            if Shell_NotifyIconW(NIM_ADD, &nid) != 0 {
                nid.Anonymous.uVersion = NOTIFYICON_VERSION_4;
                Shell_NotifyIconW(NIM_SETVERSION, &nid);
                st.added = true;
            }
        } else {
            Shell_NotifyIconW(NIM_MODIFY, &nid);
        }
    } else if st.added {
        Shell_NotifyIconW(NIM_DELETE, &nid);
        st.added = false;
    }
}

unsafe fn show_menu(hwnd: HWND, paused: bool) -> Option<TrayCmd> {
    let menu = CreatePopupMenu();
    if menu == 0 {
        return None;
    }
    let item = |id: usize, text: &str, enabled: bool| {
        let t = wide(text);
        AppendMenuW(menu, MF_STRING | if enabled { 0 } else { MF_GRAYED }, id, t.as_ptr());
    };
    item(ID_PAUSE, "Pause all", !paused);
    item(ID_RESUME, "Resume", paused);
    AppendMenuW(menu, MF_SEPARATOR, 0, std::ptr::null());
    item(ID_QUIT, "Quit", true);
    let mut pt = POINT { x: 0, y: 0 };
    GetCursorPos(&mut pt);
    SetForegroundWindow(hwnd);
    let id = TrackPopupMenu(menu, TPM_RIGHTBUTTON | TPM_RETURNCMD | TPM_NONOTIFY, pt.x, pt.y, 0, hwnd, std::ptr::null()) as usize;
    PostMessageW(hwnd, WM_NULL, 0, 0);
    DestroyMenu(menu);
    match id {
        ID_PAUSE => Some(TrayCmd::Pause),
        ID_RESUME => Some(TrayCmd::Resume),
        ID_QUIT => Some(TrayCmd::Quit),
        _ => None,
    }
}

unsafe extern "system" fn wndproc(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    match msg {
        WM_UPDATE => {
            STATE.with(|s| {
                if let Some(st) = s.borrow_mut().as_mut() {
                    st.visible = wp != 0;
                    st.paused = lp != 0;
                    sync_icon(hwnd, st, false);
                }
            });
            0
        }
        WM_ICON => {
            // NOTIFYICON_VERSION_4: the event is in the low word of lParam.
            let ev = (lp as u32) & 0xffff;
            if ev == WM_CONTEXTMENU || ev == NIN_SELECT || ev == NIN_KEYSELECT {
                let paused = STATE.with(|s| s.borrow().as_ref().map(|st| st.paused).unwrap_or(false));
                if let Some(cmd) = show_menu(hwnd, paused) {
                    STATE.with(|s| {
                        if let Some(st) = s.borrow().as_ref() {
                            let _ = st.cmds.send(cmd);
                        }
                    });
                }
            }
            0
        }
        WM_CLOSE_TRAY => {
            STATE.with(|s| {
                if let Some(st) = s.borrow_mut().as_mut() {
                    st.visible = false;
                    sync_icon(hwnd, st, false);
                }
            });
            DestroyWindow(hwnd);
            0
        }
        WM_DESTROY => {
            PostQuitMessage(0);
            0
        }
        _ => {
            let restart = STATE.with(|s| s.borrow().as_ref().is_some_and(|st| st.taskbar_created != 0 && msg == st.taskbar_created));
            if restart {
                // Explorer restarted: the icon must be added again.
                STATE.with(|s| {
                    if let Some(st) = s.borrow_mut().as_mut() {
                        st.added = false;
                        sync_icon(hwnd, st, true);
                    }
                });
                return 0;
            }
            DefWindowProcW(hwnd, msg, wp, lp)
        }
    }
}
