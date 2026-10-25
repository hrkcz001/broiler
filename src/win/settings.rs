//! The settings window.
//!
//! One ordinary native window, modeless like the flyout, so the resident instance keeps answering
//! while it is open. It shows only what this build can actually do, and says so where it cannot: the
//! startup row is present and disabled rather than pretending to toggle a registry value this build
//! never writes.
//!
//! The window decides nothing either. Every action is reported to the caller, which performs it and
//! then calls [`Settings::set_left_click`] with the value the coordinator ended up with.

use std::ffi::c_void;

use windows_sys::Win32::Foundation::{HWND, LPARAM, LRESULT, RECT, WPARAM};
use windows_sys::Win32::Graphics::Gdi::{
    GetMonitorInfoW, GetStockObject, MonitorFromPoint, DEFAULT_GUI_FONT, HMONITOR, MONITORINFO,
    MONITOR_DEFAULTTONEAREST,
};
use windows_sys::Win32::UI::HiDpi::GetDpiForSystem;
use windows_sys::Win32::UI::Input::KeyboardAndMouse::EnableWindow;
use windows_sys::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DefWindowProcW, DestroyWindow, GetSystemMetrics, GetWindowLongPtrW, IsWindow,
    LoadCursorW, RegisterClassExW, SendMessageW, SetForegroundWindow,
    SetWindowLongPtrW, SetWindowPos, SetWindowTextW, ShowWindow, CREATESTRUCTW, CS_HREDRAW,
    CS_VREDRAW, GWLP_USERDATA, HMENU, IDC_ARROW, SM_CXSCREEN, SM_CYSCREEN, SWP_NOACTIVATE,
    SWP_NOZORDER, SW_SHOW, WNDCLASSEXW, WS_CHILD, WS_EX_TOOLWINDOW, WS_TABSTOP, WS_VISIBLE,
};

use crate::config::settings::LeftClick;
use crate::win::wide::to_wide;

/// Control identifiers. `WM_COMMAND` reports these.
pub const COMBO: i32 = 10;
pub const STARTUP_BOX: i32 = 11;
pub const SELECT_MONITORS: i32 = 12;
pub const OPEN_PROFILES: i32 = 13;
pub const CLOSE: i32 = 14;

/// What the window asks its owner to do. The owner performs it and reports the outcome back.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Action {
    /// The user changed the left-click behaviour; the value is the one to store.
    LeftClick(LeftClick),
    /// The user asked for the monitor selection dialog.
    SelectMonitors,
    /// The user asked for the profiles directory.
    OpenProfiles,
    /// The user closed the window.
    Close,
}

/// The state the window shows when it opens.
#[derive(Clone, Debug)]
pub struct Snapshot {
    /// Current left-click behaviour.
    pub left_click: LeftClick,
    /// How many monitors this instance controls, for the selection line.
    pub monitors: usize,
    /// How many profiles are loaded.
    pub profiles: usize,
    /// True when this build registers itself to start at logon.
    pub start_at_logon: bool,
}

/// The two combo entries, in the order the combo shows them.
pub fn left_click_items() -> [(&'static str, LeftClick); 2] {
    [
        ("Cycle profiles", LeftClick::Cycle),
        ("Open the profile flyout", LeftClick::Flyout),
    ]
}

/// The index of a behaviour in the combo.
pub fn left_click_index(value: LeftClick) -> usize {
    left_click_items()
        .iter()
        .position(|(_, item)| *item == value)
        .unwrap_or(0)
}

/// The behaviour at a combo index. An index outside the list keeps the cycle behaviour, which is the
/// safe default: a popup can never appear because of a stray index.
pub fn left_click_at(index: usize) -> LeftClick {
    left_click_items()
        .get(index)
        .map(|(_, value)| *value)
        .unwrap_or(LeftClick::Cycle)
}

/// The caption of the startup row. This build does not write a logon entry, and the row says so
/// instead of offering a checkbox that would do nothing.
pub fn startup_text(registered: bool) -> String {
    if registered {
        String::from("Launch at Windows startup")
    } else {
        String::from("Launch at Windows startup (not registered by this build)")
    }
}

/// The summary line under the controls.
pub fn summary_text(monitors: usize, profiles: usize) -> String {
    format!(
        "{monitors} monitor(s) controlled, {profiles} profile(s) loaded"
    )
}

struct Metrics {
    width: i32,
    margin: i32,
    label: i32,
    row: i32,
    gap: i32,
}

impl Metrics {
    fn new(dpi: u32) -> Metrics {
        let scale = |px: i32| (px * dpi as i32 + 48) / 96;
        Metrics {
            width: scale(380),
            margin: scale(12),
            label: scale(20),
            row: scale(26),
            gap: scale(8),
        }
    }

    fn combo_top(&self) -> i32 {
        self.margin + self.label + self.gap
    }

    fn startup_top(&self) -> i32 {
        self.combo_top() + self.row + self.gap * 2
    }

    fn summary_top(&self) -> i32 {
        self.startup_top() + self.row + self.gap
    }

    fn buttons_top(&self) -> i32 {
        self.summary_top() + self.label + self.gap
    }

    fn height(&self) -> i32 {
        self.buttons_top() + 2 * self.row + self.gap + self.margin
    }
}

/// The open window.
pub struct Settings {
    window: HWND,
    combo: HWND,
    summary: HWND,
    user: *mut c_void,
    snapshot: fn(*mut c_void) -> Snapshot,
    apply: fn(*mut c_void, Action) -> bool,
}

impl Drop for Settings {
    fn drop(&mut self) {
        unsafe {
            if !self.window.is_null() && IsWindow(self.window) != 0 {
                DestroyWindow(self.window);
            }
            self.window = std::ptr::null_mut();
        }
    }
}

const SETTINGS_CLASS: &str = "BroilerSettings";

unsafe extern "system" fn settings_proc(
    window: HWND,
    message: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    if message == 0x0081 {
        let create = &*(lparam as *const CREATESTRUCTW);
        SetWindowLongPtrW(window, GWLP_USERDATA, create.lpCreateParams as isize);
        return 1;
    }
    if message == 0x0082 {
        SetWindowLongPtrW(window, GWLP_USERDATA, 0);
        return 0;
    }
    let settings = GetWindowLongPtrW(window, GWLP_USERDATA) as *mut Settings;
    let Some(settings) = settings.as_ref() else {
        return DefWindowProcW(window, message, wparam, lparam);
    };

    match message {
        0x0010 => {
            // The close box behaves like the Close button.
            (settings.apply)(settings.user, Action::Close);
            DestroyWindow(window);
            0
        }
        0x0111 => {
            let control = (wparam & 0xFFFF) as i32;
            let code = ((wparam >> 16) & 0xFFFF) as u16;
            let sender = lparam as HWND;
            // 1 = CBN_SELCHANGE, 0 = BN_CLICKED.
            if control == COMBO && code == 1 {
                let index = SendMessageW(settings.combo, 0x0147, 0, 0) as usize; // CB_GETCURSEL
                let chosen = left_click_at(index);
                if !(settings.apply)(settings.user, Action::LeftClick(chosen)) {
                    // The write did not happen, so the combo must not claim it did.
                    let stored = (settings.snapshot)(settings.user).left_click;
                    SendMessageW(settings.combo, 0x014E, left_click_index(stored), 0);
                }
                return 0;
            }
            if code == 0 && sender == settings.combo {
                return 0;
            }
            if code == 0 {
                match control {
                    SELECT_MONITORS => {
                        (settings.apply)(settings.user, Action::SelectMonitors);
                    }
                    OPEN_PROFILES => {
                        (settings.apply)(settings.user, Action::OpenProfiles);
                    }
                    CLOSE => {
                        (settings.apply)(settings.user, Action::Close);
                        DestroyWindow(window);
                    }
                    _ => {}
                }
            }
            0
        }
        _ => DefWindowProcW(window, message, wparam, lparam),
    }
}

unsafe fn register_class() {
    let class_name = to_wide(SETTINGS_CLASS);
    let mut class: WNDCLASSEXW = std::mem::zeroed();
    class.cbSize = std::mem::size_of::<WNDCLASSEXW>() as u32;
    class.style = CS_HREDRAW | CS_VREDRAW;
    class.lpfnWndProc = Some(settings_proc);
    class.hInstance = windows_sys::Win32::System::LibraryLoader::GetModuleHandleW(std::ptr::null());
    class.hCursor = LoadCursorW(std::ptr::null_mut(), IDC_ARROW as *const u16);
    class.lpszClassName = class_name.as_ptr();
    let _ = RegisterClassExW(&class);
    std::mem::forget(class_name);
}

unsafe fn add_control(
    parent: HWND,
    class: &str,
    text: &str,
    style: u32,
    x: i32,
    y: i32,
    width: i32,
    height: i32,
    id: i32,
) -> HWND {
    let class_name = to_wide(class);
    let text = to_wide(text);
    let control = CreateWindowExW(
        0,
        class_name.as_ptr(),
        text.as_ptr(),
        style | WS_CHILD | WS_VISIBLE,
        x,
        y,
        width,
        height,
        parent,
        id as usize as *mut c_void as HMENU,
        windows_sys::Win32::System::LibraryLoader::GetModuleHandleW(std::ptr::null()),
        std::ptr::null_mut(),
    );
    if !control.is_null() {
        let font = GetStockObject(DEFAULT_GUI_FONT as i32);
        SendMessageW(control, 0x0030, font as WPARAM, 1);
    }
    control
}

impl Settings {
    /// Create and show the window for `snapshot`.
    pub fn open(
        owner: HWND,
        snapshot: &Snapshot,
        user: *mut c_void,
        read: fn(*mut c_void) -> Snapshot,
        apply: fn(*mut c_void, Action) -> bool,
    ) -> Option<Box<Settings>> {
        unsafe {
            register_class();
            let dpi = GetDpiForSystem().max(96);
            let metrics = Metrics::new(dpi);
            let width = metrics.width;
            let height = metrics.height();

            let mut window_state = Box::new(Settings {
                window: std::ptr::null_mut(),
                combo: std::ptr::null_mut(),
                summary: std::ptr::null_mut(),
                user,
                snapshot: read,
                apply,
            });

            let class_name = to_wide(SETTINGS_CLASS);
            let title = to_wide("broiler settings");
            let window = CreateWindowExW(
                WS_EX_TOOLWINDOW,
                class_name.as_ptr(),
                title.as_ptr(),
                windows_sys::Win32::UI::WindowsAndMessaging::WS_POPUP
                    | windows_sys::Win32::UI::WindowsAndMessaging::WS_CAPTION
                    | windows_sys::Win32::UI::WindowsAndMessaging::WS_SYSMENU,
                0,
                0,
                width,
                height,
                owner,
                std::ptr::null_mut(),
                windows_sys::Win32::System::LibraryLoader::GetModuleHandleW(std::ptr::null()),
                &mut *window_state as *mut Settings as *const c_void,
            );
            if window.is_null() {
                return None;
            }
            window_state.window = window;

            let inner = width - 2 * metrics.margin;
            add_control(
                window,
                "STATIC",
                "Left click on the tray icon",
                0,
                metrics.margin,
                metrics.margin,
                inner,
                metrics.label,
                0,
            );
            let combo = add_control(
                window,
                "COMBOBOX",
                "",
                0x0002 | 0x0040 | WS_TABSTOP, // CBS_DROPDOWNLIST | CBS_NOINTEGRALHEIGHT
                metrics.margin,
                metrics.combo_top(),
                inner,
                metrics.row * 6,
                COMBO,
            );
            for (index, (label, _)) in left_click_items().iter().enumerate() {
                let text = to_wide(label);
                SendMessageW(combo, 0x0143, 0, text.as_ptr() as isize); // CB_ADDSTRING
                let _ = index;
            }
            SendMessageW(combo, 0x014E, left_click_index(snapshot.left_click), 0); // CB_SETCURSEL

            let startup = add_control(
                window,
                "BUTTON",
                &startup_text(snapshot.start_at_logon),
                BS_AUTOCHECKBOX,
                metrics.margin,
                metrics.startup_top(),
                inner,
                metrics.row,
                STARTUP_BOX,
            );
            // This build registers nothing at logon, so the control is shown and disabled rather
            // than offering a promise it cannot keep.
            EnableWindow(startup, 0);

            let summary = add_control(
                window,
                "STATIC",
                &summary_text(snapshot.monitors, snapshot.profiles),
                0,
                metrics.margin,
                metrics.summary_top(),
                inner,
                metrics.label,
                0,
            );

            let half = (inner - metrics.gap) / 2;
            add_control(
                window,
                "BUTTON",
                "Select monitors\u{2026}",
                WS_TABSTOP,
                metrics.margin,
                metrics.buttons_top(),
                half,
                metrics.row,
                SELECT_MONITORS,
            );
            add_control(
                window,
                "BUTTON",
                "Open profiles directory",
                WS_TABSTOP,
                metrics.margin + half + metrics.gap,
                metrics.buttons_top(),
                half,
                metrics.row,
                OPEN_PROFILES,
            );

            let button_width = (inner - 2 * metrics.gap) / 3;
            add_control(
                window,
                "BUTTON",
                "Close",
                WS_TABSTOP | 0x0001, // BS_DEFPUSHBUTTON
                metrics.margin,
                metrics.buttons_top() + metrics.row + metrics.gap,
                button_width,
                metrics.row,
                CLOSE,
            );

            let area = work_area_around_cursor().unwrap_or(RECT {
                left: 0,
                top: 0,
                right: GetSystemMetrics(SM_CXSCREEN),
                bottom: GetSystemMetrics(SM_CYSCREEN),
            });
            let x = area.left + ((area.right - area.left) - width) / 2;
            let y = area.top + ((area.bottom - area.top) - height) / 2;
            SetWindowPos(
                window,
                std::ptr::null_mut(),
                x,
                y,
                width,
                height,
                SWP_NOZORDER | SWP_NOACTIVATE,
            );
            ShowWindow(window, SW_SHOW);
            SetForegroundWindow(window);
            window_state.combo = combo;
            window_state.summary = summary;
            Some(window_state)
        }
    }

    /// Show the behaviour the coordinator actually stored, which may differ from what was clicked if
    /// the write failed.
    pub fn set_left_click(&mut self, value: LeftClick) {
        if self.combo.is_null() {
            return;
        }
        unsafe {
            SendMessageW(self.combo, 0x014E, left_click_index(value), 0);
        }
    }

    /// Update the counts after a selection change.
    pub fn set_summary(&mut self, monitors: usize, profiles: usize) {
        if self.summary.is_null() {
            return;
        }
        let text = to_wide(&summary_text(monitors, profiles));
        unsafe { SetWindowTextW(self.summary, text.as_ptr()) };
    }

    pub fn window_id(&self) -> isize {
        self.window as isize
    }

    pub fn is_open(&self) -> bool {
        !self.window.is_null() && unsafe { IsWindow(self.window) != 0 }
    }
}

/// `BS_AUTOCHECKBOX`. The style bits are spelled out because the crate exports them only through the
/// controls module on some versions.
const BS_AUTOCHECKBOX: u32 = 0x0003;

unsafe fn work_area_around_cursor() -> Option<RECT> {
    let mut point = windows_sys::Win32::Foundation::POINT { x: 0, y: 0 };
    windows_sys::Win32::UI::WindowsAndMessaging::GetCursorPos(&mut point);
    let monitor: HMONITOR = MonitorFromPoint(point, MONITOR_DEFAULTTONEAREST);
    if monitor.is_null() {
        return None;
    }
    let mut info: MONITORINFO = std::mem::zeroed();
    info.cbSize = std::mem::size_of::<MONITORINFO>() as u32;
    if GetMonitorInfoW(monitor, &mut info) == 0 {
        return None;
    }
    Some(info.rcWork)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_combo_offers_both_behaviours_in_a_fixed_order() {
        let items = left_click_items();
        assert_eq!(items[0].1, LeftClick::Cycle);
        assert_eq!(items[1].1, LeftClick::Flyout);
        assert_eq!(left_click_index(LeftClick::Cycle), 0);
        assert_eq!(left_click_index(LeftClick::Flyout), 1);
        assert_eq!(left_click_at(0), LeftClick::Cycle);
        assert_eq!(left_click_at(1), LeftClick::Flyout);
        // A stray index must never open a popup.
        assert_eq!(left_click_at(99), LeftClick::Cycle);
    }

    #[test]
    fn the_startup_row_admits_that_this_build_registers_nothing() {
        assert_eq!(
            startup_text(false),
            "Launch at Windows startup (not registered by this build)"
        );
        assert_eq!(startup_text(true), "Launch at Windows startup");
    }

    #[test]
    fn the_summary_counts_what_the_instance_controls() {
        assert_eq!(
            summary_text(2, 3),
            "2 monitor(s) controlled, 3 profile(s) loaded"
        );
    }

    #[test]
    fn the_layout_stacks_rows_without_overlapping_them() {
        for dpi in [96u32, 120, 144, 192] {
            let metrics = Metrics::new(dpi);
            assert!(metrics.combo_top() >= metrics.margin + metrics.label);
            assert!(metrics.startup_top() >= metrics.combo_top() + metrics.row);
            assert!(metrics.summary_top() >= metrics.startup_top() + metrics.row);
            assert!(metrics.buttons_top() >= metrics.summary_top() + metrics.label);
            assert!(metrics.height() > metrics.buttons_top() + metrics.row);
        }
    }
}
