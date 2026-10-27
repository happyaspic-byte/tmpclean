//! The GUI's only `unsafe`: raw Win32 window/control FFI behind safe wrappers.
//!
//! The window procedure is stateless: button clicks and the title-bar close
//! are re-posted as `WM_APP` messages so [`Ui::next_event`] sees every input in
//! the message loop, together with `WM_TIMER` ticks and Enter/Esc key presses.

use std::mem::{size_of, zeroed};
use std::ptr::{null, null_mut};

use windows_sys::Win32::Foundation::{HWND, LPARAM, LRESULT, RECT, WPARAM};
use windows_sys::Win32::Graphics::Gdi::{
    GetStockObject, UpdateWindow, COLOR_BTNFACE, DEFAULT_GUI_FONT,
};
use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
use windows_sys::Win32::UI::Controls::{
    InitCommonControlsEx, ICC_PROGRESS_CLASS, INITCOMMONCONTROLSEX, PBM_SETMARQUEE, PBM_SETPOS,
    PBM_SETRANGE32, PBS_MARQUEE, PROGRESS_CLASSW,
};
use windows_sys::Win32::UI::Input::KeyboardAndMouse::{
    EnableWindow, SetFocus, VK_ESCAPE, VK_RETURN,
};
use windows_sys::Win32::UI::WindowsAndMessaging::{
    AdjustWindowRectEx, CreateWindowExW, DefWindowProcW, DispatchMessageW, GetMessageW,
    GetSystemMetrics, LoadCursorW, MessageBoxW, PostMessageW, PostQuitMessage, RegisterClassW,
    SendMessageW, SetForegroundWindow, SetTimer, SetWindowLongPtrW, SetWindowTextW, ShowWindow,
    TranslateMessage, BN_CLICKED, BS_DEFPUSHBUTTON, BS_PUSHBUTTON, GWL_STYLE, IDC_ARROW,
    MB_ICONERROR, MB_OK, MSG, SM_CXSCREEN, SM_CYSCREEN, SW_SHOW, WM_APP, WM_CLOSE, WM_COMMAND,
    WM_DESTROY, WM_KEYDOWN, WM_SETFONT, WM_TIMER, WNDCLASSW, WS_CAPTION, WS_CHILD, WS_DISABLED,
    WS_EX_TOPMOST, WS_MINIMIZEBOX, WS_OVERLAPPED, WS_SYSMENU, WS_TABSTOP, WS_VISIBLE,
};

/// Control id of the Cancel button.
pub const IDC_CANCEL: u16 = 101;
/// Control id of the Close button (UI Automation AutomationId "102").
pub const IDC_CLOSE: u16 = 102;
/// `WM_APP` payload for the title-bar close box.
const WINDOW_CLOSE: usize = 0xFFFF;
const TIMER_ID: usize = 1;
const SS_NOPREFIX: u32 = 0x80;
const CLIENT_W: i32 = 480;
const CLIENT_H: i32 = 380;

/// One input observed by the message loop.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Event {
    Tick,
    Cancel,
    Close,
    WindowClose,
    Enter,
    Escape,
    Quit,
}

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(Some(0)).collect()
}

unsafe extern "system" fn wndproc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    match msg {
        WM_COMMAND => {
            if ((wparam >> 16) & 0xFFFF) as u32 == BN_CLICKED {
                PostMessageW(hwnd, WM_APP, wparam & 0xFFFF, 0);
            }
            0
        }
        WM_CLOSE => {
            PostMessageW(hwnd, WM_APP, WINDOW_CLOSE, 0);
            0
        }
        WM_DESTROY => {
            PostQuitMessage(0);
            0
        }
        _ => DefWindowProcW(hwnd, msg, wparam, lparam),
    }
}

/// Load comctl32 v6 classes (progress bar). Requires the embedded manifest.
pub fn init_common_controls() {
    let icc = INITCOMMONCONTROLSEX {
        dwSize: size_of::<INITCOMMONCONTROLSEX>() as u32,
        dwICC: ICC_PROGRESS_CLASS,
    };
    // SAFETY: icc is a fully initialized struct with the correct dwSize.
    unsafe { InitCommonControlsEx(&icc) };
}

/// Modal message box (no owner window).
pub fn message_box(title: &str, text: &str, error: bool) {
    let (title, text) = (wide(title), wide(text));
    let style = if error { MB_OK | MB_ICONERROR } else { MB_OK };
    // SAFETY: both strings are NUL-terminated and outlive the call.
    unsafe { MessageBoxW(null_mut(), text.as_ptr(), title.as_ptr(), style) };
}

/// The fixed, always-on-top progress dialog. Not `Send`: Win32 windows
/// belong to the thread that created them.
pub struct Ui {
    hwnd: HWND,
    targets: HWND,
    counters: HWND,
    status: HWND,
    progress: HWND,
    cancel: HWND,
    close: HWND,
}

impl Ui {
    /// Create and show the dialog; `None` if any window could not be created.
    pub fn create(title: &str, cancel_text: &str, close_text: &str) -> Option<Ui> {
        let class = wide("tmpclean_gui_window");
        let title = wide(title);
        // SAFETY: every pointer passed below is either null (documented as
        // allowed) or a NUL-terminated buffer living until the call returns;
        // returned handles are checked for null before use.
        unsafe {
            let instance = GetModuleHandleW(null());
            let wc = WNDCLASSW {
                style: 0,
                lpfnWndProc: Some(wndproc),
                cbClsExtra: 0,
                cbWndExtra: 0,
                hInstance: instance,
                hIcon: null_mut(),
                hCursor: LoadCursorW(null_mut(), IDC_ARROW),
                hbrBackground: (COLOR_BTNFACE + 1) as usize as _,
                lpszMenuName: null(),
                lpszClassName: class.as_ptr(),
            };
            if RegisterClassW(&wc) == 0 {
                return None;
            }
            // Fixed size: no WS_THICKFRAME and no maximize box.
            let style = WS_OVERLAPPED | WS_CAPTION | WS_SYSMENU | WS_MINIMIZEBOX;
            let mut rect = RECT {
                left: 0,
                top: 0,
                right: CLIENT_W,
                bottom: CLIENT_H,
            };
            AdjustWindowRectEx(&mut rect, style, 0, WS_EX_TOPMOST);
            let (w, h) = (rect.right - rect.left, rect.bottom - rect.top);
            let x = (GetSystemMetrics(SM_CXSCREEN) - w).max(0) / 2;
            let y = (GetSystemMetrics(SM_CYSCREEN) - h).max(0) / 2;
            let hwnd = CreateWindowExW(
                WS_EX_TOPMOST,
                class.as_ptr(),
                title.as_ptr(),
                style,
                x,
                y,
                w,
                h,
                null_mut(),
                null_mut(),
                instance,
                null(),
            );
            if hwnd.is_null() {
                return None;
            }
            let font = GetStockObject(DEFAULT_GUI_FONT);
            let child = |class: *const u16, text: &str, style: u32, id: u16, r: [i32; 4]| {
                let text = wide(text);
                let h = CreateWindowExW(
                    0,
                    class,
                    text.as_ptr(),
                    WS_CHILD | WS_VISIBLE | style,
                    r[0],
                    r[1],
                    r[2],
                    r[3],
                    hwnd,
                    id as usize as _,
                    instance,
                    null(),
                );
                if !h.is_null() {
                    SendMessageW(h, WM_SETFONT, font as usize, 1);
                }
                h
            };
            let stat = wide("STATIC");
            let button = wide("BUTTON");
            let ui = Ui {
                hwnd,
                targets: child(stat.as_ptr(), "", SS_NOPREFIX, 0, [12, 12, 456, 64]),
                counters: child(stat.as_ptr(), "", SS_NOPREFIX, 0, [12, 82, 456, 190]),
                status: child(stat.as_ptr(), "", SS_NOPREFIX, 0, [12, 278, 456, 20]),
                progress: child(PROGRESS_CLASSW, "", PBS_MARQUEE, 0, [12, 302, 456, 18]),
                cancel: child(
                    button.as_ptr(),
                    cancel_text,
                    WS_TABSTOP | BS_PUSHBUTTON as u32,
                    IDC_CANCEL,
                    [288, 338, 86, 28],
                ),
                close: child(
                    button.as_ptr(),
                    close_text,
                    WS_TABSTOP | WS_DISABLED | BS_DEFPUSHBUTTON as u32,
                    IDC_CLOSE,
                    [382, 338, 86, 28],
                ),
            };
            if [
                ui.targets,
                ui.counters,
                ui.status,
                ui.progress,
                ui.cancel,
                ui.close,
            ]
            .iter()
            .any(|h| h.is_null())
            {
                return None;
            }
            ShowWindow(hwnd, SW_SHOW);
            UpdateWindow(hwnd);
            SetForegroundWindow(hwnd);
            SetFocus(ui.cancel);
            Some(ui)
        }
    }

    fn set_text(hwnd: HWND, text: &str) {
        let text = wide(text);
        // SAFETY: hwnd is a live window owned by this thread; text is terminated.
        unsafe { SetWindowTextW(hwnd, text.as_ptr()) };
    }

    pub fn set_title(&self, text: &str) {
        Self::set_text(self.hwnd, text);
    }

    pub fn set_targets(&self, text: &str) {
        Self::set_text(self.targets, text);
    }

    pub fn set_counters(&self, text: &str) {
        Self::set_text(self.counters, text);
    }

    pub fn set_status(&self, text: &str) {
        Self::set_text(self.status, text);
    }

    pub fn set_cancel_enabled(&self, enabled: bool) {
        // SAFETY: live child window of this thread.
        unsafe { EnableWindow(self.cancel, enabled.into()) };
    }

    /// Enable the Close button and give it keyboard focus.
    pub fn enable_close(&self) {
        // SAFETY: live child window of this thread.
        unsafe {
            EnableWindow(self.close, 1);
            SetFocus(self.close);
        }
    }

    pub fn progress_marquee(&self) {
        // SAFETY: live progress control; PBM_SETMARQUEE takes (on, interval ms).
        unsafe { SendMessageW(self.progress, PBM_SETMARQUEE, 1, 30) };
    }

    pub fn progress_full(&self) {
        // SAFETY: live progress control; plain integer messages and style bits.
        unsafe {
            SendMessageW(self.progress, PBM_SETMARQUEE, 0, 0);
            SetWindowLongPtrW(self.progress, GWL_STYLE, (WS_CHILD | WS_VISIBLE) as isize);
            SendMessageW(self.progress, PBM_SETRANGE32, 0, 100);
            SendMessageW(self.progress, PBM_SETPOS, 100, 0);
        }
    }

    /// Start the single UI timer; each period yields one [`Event::Tick`].
    pub fn start_timer(&self, period_ms: u32) {
        // SAFETY: live top-level window; no TIMERPROC, so WM_TIMER is queued.
        unsafe { SetTimer(self.hwnd, TIMER_ID, period_ms, None) };
    }

    /// Block until the next input event, dispatching everything else.
    pub fn next_event(&self) -> Event {
        loop {
            // SAFETY: MSG is plain data; GetMessageW fills it. Messages not
            // consumed here go through the standard Translate/Dispatch pair.
            unsafe {
                let mut msg: MSG = zeroed();
                if GetMessageW(&mut msg, null_mut(), 0, 0) <= 0 {
                    return Event::Quit;
                }
                match msg.message {
                    WM_TIMER if msg.hwnd == self.hwnd && msg.wParam == TIMER_ID => {
                        return Event::Tick
                    }
                    WM_APP if msg.hwnd == self.hwnd => match msg.wParam {
                        w if w == IDC_CANCEL as usize => return Event::Cancel,
                        w if w == IDC_CLOSE as usize => return Event::Close,
                        WINDOW_CLOSE => return Event::WindowClose,
                        _ => {}
                    },
                    WM_KEYDOWN if msg.wParam == VK_ESCAPE as usize => return Event::Escape,
                    WM_KEYDOWN if msg.wParam == VK_RETURN as usize => return Event::Enter,
                    _ => {
                        TranslateMessage(&msg);
                        DispatchMessageW(&msg);
                    }
                }
            }
        }
    }
}
