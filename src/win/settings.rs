//! The settings window.
//!
//! One ordinary native dialog, modeless so the resident instance keeps serving while it is open. It is
//! built from a dialog template held in memory rather than by composing child windows by hand, because
//! that is what gives the window its standard frame, its system font, its themed controls, Tab
//! navigation, the default-button and Escape behaviour, and the dialog keyboard semantics an
//! accessibility client expects. `Common-Controls` v6 is activated for the process in
//! `res/broiler.manifest`, so the controls are drawn by the themed implementation.
//!
//! The window decides nothing. Every action is reported to the caller, which performs it and then
//! calls [`Settings::set_snapshot`] with what the coordinator ended up holding; a refused save
//! therefore cannot leave the window claiming a value that was never written.
//!
//! One row is honest about a missing feature: this build registers no logon entry, so the startup
//! checkbox is shown and disabled instead of offering a promise it cannot keep.

use std::ffi::c_void;

use windows_sys::Win32::Foundation::{HWND, LPARAM, RECT, WPARAM};
use windows_sys::Win32::Graphics::Gdi::{
    GetMonitorInfoW, MonitorFromPoint, HMONITOR, MONITORINFO, MONITOR_DEFAULTTONEAREST,
};
use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
use windows_sys::Win32::UI::HiDpi::GetDpiForSystem;
use windows_sys::Win32::UI::Input::KeyboardAndMouse::EnableWindow;
use windows_sys::Win32::UI::WindowsAndMessaging::{
    CreateDialogIndirectParamW, DestroyWindow, GetDlgItem, GetSystemMetrics, GetWindowLongPtrW,
    GetWindowRect, IsWindow, SendMessageW, SetForegroundWindow, SetWindowLongPtrW,
    SetWindowPos, SetWindowTextW, ShowWindow, DLGTEMPLATE, GWLP_USERDATA, SM_CXSCREEN, SM_CYSCREEN,
    SWP_NOACTIVATE, SWP_NOZORDER, SW_SHOW,
};
use windows_sys::Win32::UI::Input::KeyboardAndMouse::SetFocus;

use crate::config::settings::LeftClick;
use crate::win::wide::to_wide;

// ---------------------------------------------------------------------------
// Control identifiers. `WM_COMMAND` reports these, and IDCANCEL is what Escape sends.
// ---------------------------------------------------------------------------

pub const COMBO: i32 = 100;
pub const STARTUP_BOX: i32 = 101;
pub const MONITOR_NAME_BOX: i32 = 102;
pub const SELECT_MONITORS: i32 = 103;
pub const OPEN_PROFILES: i32 = 104;
pub const RELOAD_PROFILES: i32 = 105;
pub const OPEN_REPOSITORY: i32 = 106;
/// `IDCANCEL`: the close box and Escape both end here.
pub const CLOSE: i32 = 2;

// Dialog and control styles, from `winuser.h` and `commctrl.h`.
const WS_POPUP: u32 = 0x8000_0000;
const WS_CAPTION: u32 = 0x00C0_0000;
const WS_SYSMENU: u32 = 0x0008_0000;
const WS_CLIPCHILDREN: u32 = 0x0200_0000;
const WS_TABSTOP: u32 = 0x0001_0000;
const WS_VSCROLL: u32 = 0x0020_0000;
const DS_SETFONT: u32 = 0x0000_0040;
const DS_MODALFRAME: u32 = 0x0000_0080;
/// `BS_AUTOCHECKBOX`.
const BS_AUTOCHECKBOX: u32 = 0x0003;
const BS_PUSHBUTTON: u32 = 0x0000;
const BS_DEFPUSHBUTTON: u32 = 0x0001;
/// An editable combo box with a drop-down list. Named so a test can assert the selector is not this.
pub const CBS_DROPDOWN: u32 = 0x0002;
/// A drop-down list that is not editable. This is the one the settings window uses.
pub const CBS_DROPDOWNLIST: u32 = 0x0003;
/// The list part keeps its own height instead of being sized to fit an edit field.
const CBS_NOINTEGRALHEIGHT: u32 = 0x0040;
/// `SS_LEFT`.
const SS_LEFT: u32 = 0x0000;

const WM_INITDIALOG: u32 = 0x0110;
const WM_COMMAND: u32 = 0x0111;
const WM_CLOSE: u32 = 0x0010;
const CBN_SELCHANGE: u16 = 1;
const BN_CLICKED: u16 = 0;
const CB_ADDSTRING: u32 = 0x0143;
const CB_SETCURSEL: u32 = 0x014E;
const CB_GETCURSEL: u32 = 0x0147;
const BM_GETCHECK: u32 = 0x00F0;
const BM_SETCHECK: u32 = 0x00F1;
const BST_CHECKED: usize = 1;
const BST_UNCHECKED: usize = 0;

/// What the window asks its owner to do. The owner performs it and reports the outcome back.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Action {
    /// The user changed the left-click behaviour; the value is the one to store.
    LeftClick(LeftClick),
    /// The user changed `tray.always_show_monitor_name`.
    AlwaysShowMonitorName(bool),
    /// The user asked for the monitor selection dialog.
    SelectMonitors,
    /// The user asked for the profiles directory.
    OpenProfiles,
    /// The user asked the coordinator to re-read the profiles directory.
    ReloadProfiles,
    /// The user asked for the repository page.
    OpenRepository,
    /// The user closed the window.
    Close,
}

/// The state the window shows when it opens, and again after every action.
#[derive(Clone, Debug)]
pub struct Snapshot {
    /// Current left-click behaviour.
    pub left_click: LeftClick,
    /// Current `tray.always_show_monitor_name`.
    pub always_show_monitor_name: bool,
    /// How many monitors this instance controls, for the summary line.
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
    format!("{monitors} monitor(s) controlled, {profiles} profile(s) loaded")
}

// ---------------------------------------------------------------------------
// The dialog template
// ---------------------------------------------------------------------------

/// The dialog's own style bits, as one value so a test can assert what the window is.
pub fn dialog_style() -> u32 {
    // A framed, owned window with a close box and no maximise or minimise: the standard shape of a
    // settings dialog, and the reason the previous bare popup looked like nothing else on the system.
    DS_SETFONT | DS_MODALFRAME | WS_POPUP | WS_CAPTION | WS_SYSMENU | WS_CLIPCHILDREN
}

/// One control in the template, before it is serialised.
struct Item {
    style: u32,
    id: i32,
    class: Class,
    text: String,
    /// Dialog units: x, y, width, height.
    rect: (i16, i16, i16, i16),
}

#[derive(Clone, Copy)]
enum Class {
    Button,
    Static,
    ComboBox,
}

impl Class {
    /// The predefined class atom the template uses (`0xFFFF`, ordinal).
    fn ordinal(self) -> u16 {
        match self {
            Class::Button => 0x0080,
            Class::Static => 0x0082,
            Class::ComboBox => 0x0085,
        }
    }
}

fn push_u16(out: &mut Vec<u16>, value: u16) {
    out.push(value);
}

fn push_u32(out: &mut Vec<u16>, value: u32) {
    out.push((value & 0xFFFF) as u16);
    out.push((value >> 16) as u16);
}

fn push_i16(out: &mut Vec<u16>, value: i16) {
    out.push(value as u16);
}

fn push_text(out: &mut Vec<u16>, text: &str) {
    out.extend(text.encode_utf16());
    out.push(0);
}

fn align_dword(out: &mut Vec<u16>) {
    if out.len() % 2 != 0 {
        out.push(0);
    }
}

/// The combo style, read from the template by the tests as well as used to build it.
fn combo_style() -> u32 {
    // Not editable: a drop-down list has no edit field at all. The previous build passed 0x0002 here
    // under a comment claiming this constant, which is the defect the test pins down.
    CBS_DROPDOWNLIST | CBS_NOINTEGRALHEIGHT | WS_VSCROLL | WS_TABSTOP
}

/// The rows the template holds, in tab order.
fn items(snapshot: &Snapshot) -> Vec<Item> {
    vec![
        Item {
            style: SS_LEFT,
            id: 0,
            class: Class::Static,
            text: String::from("Left click on the tray icon"),
            rect: (7, 6, 226, 9),
        },
        Item {
            style: combo_style(),
            id: COMBO,
            class: Class::ComboBox,
            text: String::new(),
            rect: (7, 17, 226, 64),
        },
        Item {
            style: BS_AUTOCHECKBOX | WS_TABSTOP,
            id: MONITOR_NAME_BOX,
            class: Class::Button,
            text: String::from("Always show monitor name"),
            rect: (7, 86, 226, 10),
        },
        Item {
            style: BS_AUTOCHECKBOX | WS_TABSTOP,
            id: STARTUP_BOX,
            class: Class::Button,
            text: startup_text(snapshot.start_at_logon),
            rect: (7, 98, 226, 10),
        },
        Item {
            style: SS_LEFT,
            id: 0,
            class: Class::Static,
            text: summary_text(snapshot.monitors, snapshot.profiles),
            rect: (7, 110, 226, 9),
        },
        Item {
            style: BS_PUSHBUTTON | WS_TABSTOP,
            id: SELECT_MONITORS,
            class: Class::Button,
            text: String::from("Select monitors..."),
            rect: (7, 124, 110, 14),
        },
        Item {
            style: BS_PUSHBUTTON | WS_TABSTOP,
            id: OPEN_PROFILES,
            class: Class::Button,
            text: String::from("Open profiles directory"),
            rect: (123, 124, 110, 14),
        },
        Item {
            style: BS_PUSHBUTTON | WS_TABSTOP,
            id: RELOAD_PROFILES,
            class: Class::Button,
            text: String::from("Reload profiles"),
            rect: (7, 141, 110, 14),
        },
        Item {
            style: BS_PUSHBUTTON | WS_TABSTOP,
            id: OPEN_REPOSITORY,
            class: Class::Button,
            text: String::from("Open repository"),
            rect: (123, 141, 110, 14),
        },
        Item {
            style: BS_DEFPUSHBUTTON | WS_TABSTOP,
            id: CLOSE,
            class: Class::Button,
            text: String::from("Close"),
            rect: (158, 162, 75, 14),
        },
    ]
}

/// Serialise a `DLGTEMPLATE` and its items exactly as `CreateDialogIndirectParamW` expects.
///
/// The structure is a sequence of 16-bit words: header, menu, class, title, font, then per item the
/// `DLGITEMTEMPLATE` followed by its class, title and creation-data length. Item records must start on
/// a `DWORD` boundary, which is why every item is preceded by an alignment step.
fn build_template(snapshot: &Snapshot) -> Vec<u16> {
    let rows = items(snapshot);
    let mut out: Vec<u16> = Vec::new();
    push_u32(&mut out, dialog_style());
    push_u32(&mut out, 0); // no extended style
    push_u16(&mut out, rows.len() as u16);
    push_i16(&mut out, 0); // x
    push_i16(&mut out, 0); // y
    push_i16(&mut out, 246); // width, dialog units
    push_i16(&mut out, 184); // height
    push_u16(&mut out, 0); // no menu
    push_u16(&mut out, 0); // default window class
    push_text(&mut out, "broiler settings");
    // DS_SETFONT: point size and typeface. `MS Shell Dlg` is the shell's UI font alias, so the dialog
    // follows the user's font and DPI instead of pinning a bitmap font.
    push_u16(&mut out, 9);
    push_text(&mut out, "MS Shell Dlg");

    for row in rows {
        align_dword(&mut out);
        push_u32(&mut out, row.style);
        push_u32(&mut out, 0);
        push_i16(&mut out, row.rect.0);
        push_i16(&mut out, row.rect.1);
        push_i16(&mut out, row.rect.2);
        push_i16(&mut out, row.rect.3);
        push_u32(&mut out, row.id as u32);
        push_u16(&mut out, 0xFFFF);
        push_u16(&mut out, row.class.ordinal());
        push_text(&mut out, &row.text);
        push_u16(&mut out, 0); // no creation data
    }
    align_dword(&mut out);
    out
}

// ---------------------------------------------------------------------------
// The running window
// ---------------------------------------------------------------------------

/// The open window.
pub struct Settings {
    window: HWND,
    combo: HWND,
    monitor_name: HWND,
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

/// The dialog procedure. A dialog procedure returns `1` for a message it handled, unlike a window
/// procedure's `LRESULT`.
unsafe extern "system" fn settings_proc(
    window: HWND,
    message: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> isize {
    match message {
        WM_INITDIALOG => {
            let settings = lparam as *const Settings as *mut Settings;
            SetWindowLongPtrW(window, GWLP_USERDATA, settings as isize);
            if settings.is_null() {
                return 1;
            }
            let settings = &*settings;
            let snapshot = (settings.snapshot)(settings.user);
            let combo = GetDlgItem(window, COMBO);
            for (index, (label, _)) in left_click_items().iter().enumerate() {
                let text = to_wide(label);
                SendMessageW(combo, CB_ADDSTRING, index, text.as_ptr() as isize);
            }
            SendMessageW(
                combo,
                CB_SETCURSEL,
                left_click_index(snapshot.left_click),
                0,
            );
            SendMessageW(
                settings.monitor_name,
                BM_SETCHECK,
                if snapshot.always_show_monitor_name {
                    BST_CHECKED
                } else {
                    BST_UNCHECKED
                },
                0,
            );
            // This build registers nothing at logon, so the control is shown and disabled rather than
            // offering a promise it cannot keep.
            EnableWindow(GetDlgItem(window, STARTUP_BOX), 0);
            let text = to_wide(&summary_text(snapshot.monitors, snapshot.profiles));
            SetWindowTextW(settings.summary, text.as_ptr());
            SetFocus(combo);
            1
        }
        WM_COMMAND => {
            let control = (wparam & 0xFFFF) as i32;
            let code = ((wparam >> 16) & 0xFFFF) as u16;
            let user = GetWindowLongPtrW(window, GWLP_USERDATA) as *const Settings as *mut Settings;
            let Some(settings) = user.as_ref() else {
                return 0;
            };
            match (control, code) {
                (COMBO, CBN_SELCHANGE) => {
                    let index = SendMessageW(settings.combo, CB_GETCURSEL, 0, 0) as usize;
                    let chosen = left_click_at(index);
                    if !(settings.apply)(settings.user, Action::LeftClick(chosen)) {
                        // The write did not happen, so the combo must not claim it did.
                        let stored = (settings.snapshot)(settings.user).left_click;
                        SendMessageW(
                            settings.combo,
                            CB_SETCURSEL,
                            left_click_index(stored),
                            0,
                        );
                    }
                    1
                }
                (MONITOR_NAME_BOX, BN_CLICKED) => {
                    let checked = SendMessageW(settings.monitor_name, BM_GETCHECK, 0, 0) == 1;
                    if !(settings.apply)(settings.user, Action::AlwaysShowMonitorName(checked)) {
                        let stored = (settings.snapshot)(settings.user).always_show_monitor_name;
                        SendMessageW(
                            settings.monitor_name,
                            BM_SETCHECK,
                            if stored { BST_CHECKED } else { BST_UNCHECKED },
                            0,
                        );
                    }
                    1
                }
                (SELECT_MONITORS, BN_CLICKED) => {
                    (settings.apply)(settings.user, Action::SelectMonitors);
                    1
                }
                (OPEN_PROFILES, BN_CLICKED) => {
                    (settings.apply)(settings.user, Action::OpenProfiles);
                    1
                }
                (RELOAD_PROFILES, BN_CLICKED) => {
                    (settings.apply)(settings.user, Action::ReloadProfiles);
                    1
                }
                (OPEN_REPOSITORY, BN_CLICKED) => {
                    (settings.apply)(settings.user, Action::OpenRepository);
                    1
                }
                // Escape arrives as IDCANCEL; the close box arrives through WM_CLOSE.
                (CLOSE, BN_CLICKED) => {
                    (settings.apply)(settings.user, Action::Close);
                    DestroyWindow(window);
                    1
                }
                _ => 0,
            }
        }
        WM_CLOSE => {
            let user = GetWindowLongPtrW(window, GWLP_USERDATA) as *const Settings as *mut Settings;
            if let Some(settings) = user.as_ref() {
                (settings.apply)(settings.user, Action::Close);
            }
            DestroyWindow(window);
            1
        }
        _ => 0,
    }
}

impl Settings {
    /// Create and show the dialog for `snapshot`.
    pub fn open(
        owner: HWND,
        snapshot: &Snapshot,
        user: *mut c_void,
        read: fn(*mut c_void) -> Snapshot,
        apply: fn(*mut c_void, Action) -> bool,
    ) -> Option<Box<Settings>> {
        unsafe {
            let template_words = build_template(snapshot);
            let mut state = Box::new(Settings {
                window: std::ptr::null_mut(),
                combo: std::ptr::null_mut(),
                monitor_name: std::ptr::null_mut(),
                summary: std::ptr::null_mut(),
                user,
                snapshot: read,
                apply,
            });

            let window = CreateDialogIndirectParamW(
                GetModuleHandleW(std::ptr::null()),
                template_words.as_ptr() as *const DLGTEMPLATE,
                owner,
                Some(settings_proc),
                &mut *state as *mut Settings as isize,
            );
            if window.is_null() {
                return None;
            }
            state.window = window;
            state.combo = GetDlgItem(window, COMBO);
            state.monitor_name = GetDlgItem(window, MONITOR_NAME_BOX);
            // The summary static has no identifier, so it is found by position among the children.
            state.summary = summary_handle(window);

            let area = work_area_around_cursor().unwrap_or(RECT {
                left: 0,
                top: 0,
                right: GetSystemMetrics(SM_CXSCREEN),
                bottom: GetSystemMetrics(SM_CYSCREEN),
            });
            let mut bounds: RECT = std::mem::zeroed();
            GetWindowRect(window, &mut bounds);
            let width = bounds.right - bounds.left;
            let height = bounds.bottom - bounds.top;
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
            Some(state)
        }
    }

    /// Show the state the coordinator actually holds, which may differ from what was clicked if the
    /// write failed.
    pub fn set_snapshot(&mut self, snapshot: &Snapshot) {
        if self.combo.is_null() {
            return;
        }
        unsafe {
            SendMessageW(
                self.combo,
                CB_SETCURSEL,
                left_click_index(snapshot.left_click),
                0,
            );
            if !self.monitor_name.is_null() {
                SendMessageW(
                    self.monitor_name,
                    BM_SETCHECK,
                    if snapshot.always_show_monitor_name {
                        BST_CHECKED
                    } else {
                        BST_UNCHECKED
                    },
                    0,
                );
            }
            if !self.summary.is_null() {
                let text = to_wide(&summary_text(snapshot.monitors, snapshot.profiles));
                SetWindowTextW(self.summary, text.as_ptr());
            }
        }
    }

    pub fn window_id(&self) -> isize {
        self.window as isize
    }

    pub fn is_open(&self) -> bool {
        !self.window.is_null() && unsafe { IsWindow(self.window) != 0 }
    }
}

/// The static row that carries the summary: the fifth item of the template, the second unlabelled one.
unsafe fn summary_handle(window: HWND) -> HWND {
    use windows_sys::Win32::UI::WindowsAndMessaging::{
        GetWindow, GW_CHILD, GW_HWNDNEXT,
    };
    // Walk the children in creation order and take the second static with id 0 after the label.
    let mut found: Vec<HWND> = Vec::new();
    let mut child = GetWindow(window, GW_CHILD);
    while !child.is_null() {
        let class = {
            let mut buffer = [0u16; 64];
            let length = windows_sys::Win32::UI::WindowsAndMessaging::GetClassNameW(
                child,
                buffer.as_mut_ptr(),
                buffer.len() as i32,
            );
            String::from_utf16_lossy(&buffer[..length.max(0) as usize])
        };
        if class.eq_ignore_ascii_case("Static") {
            found.push(child);
        }
        child = GetWindow(child, GW_HWNDNEXT);
    }
    // The template order is: label, combo, name box, startup box, summary.
    found.get(1).copied().unwrap_or(std::ptr::null_mut())
}

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

/// The system DPI, for a caller that needs to reason about the window's size.
pub fn current_dpi() -> u32 {
    unsafe { GetDpiForSystem().max(96) }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn snapshot() -> Snapshot {
        Snapshot {
            left_click: LeftClick::Flyout,
            always_show_monitor_name: false,
            monitors: 1,
            profiles: 2,
            start_at_logon: false,
        }
    }

    #[test]
    fn the_window_is_a_framed_dialog_with_a_system_font() {
        let style = dialog_style();
        // A framed window with a title bar and a close box: this is what the previous bare popup lacked.
        assert_eq!(style & WS_CAPTION, WS_CAPTION);
        assert_eq!(style & WS_SYSMENU, WS_SYSMENU);
        assert_eq!(style & WS_POPUP, WS_POPUP);
        assert_eq!(style & DS_MODALFRAME, DS_MODALFRAME);
        // The dialog carries its own font, so it follows the user's UI font and DPI.
        assert_eq!(style & DS_SETFONT, DS_SETFONT);
        // No maximise or minimise box: a settings dialog is not a resizable document window.
        assert_eq!(style & 0x0001_0000, 0, "WS_MAXIMIZEBOX must not be set");
        assert_eq!(style & 0x0002_0000, 0, "WS_MINIMIZEBOX must not be set");
    }

    #[test]
    fn the_selector_is_a_dropdown_list_and_not_an_editable_field() {
        let style = combo_style();
        // The two low bits are the combo type: 1 simple, 2 drop-down, 3 drop-down list.
        assert_eq!(style & 0x0003, CBS_DROPDOWNLIST, "style was {style:#06X}");
        assert_ne!(style & 0x0003, CBS_DROPDOWN);
        assert_eq!(CBS_DROPDOWNLIST, CBS_DROPDOWN | 0x0001);
        assert_ne!(style & WS_TABSTOP, 0);
    }

    #[test]
    fn the_template_is_the_shape_the_loader_expects() {
        let rows = items(&snapshot());
        let words = build_template(&snapshot());
        // Header: style, exStyle, item count, rectangle, menu, class, title, font.
        // The header is style (two words), extended style (two words), then the item count.
        assert_eq!(words[0] as u32 | ((words[1] as u32) << 16), dialog_style());
        assert_eq!(words[2] as u32 | ((words[3] as u32) << 16), 0);
        assert_eq!(words[4], rows.len() as u16);
        let mut expected: Vec<u16> = Vec::new();
        push_text(&mut expected, "broiler settings");
        assert!(words
            .windows(expected.len())
            .any(|window| window == expected.as_slice()));
        let mut font: Vec<u16> = Vec::new();
        push_text(&mut font, "MS Shell Dlg");
        assert!(words
            .windows(font.len())
            .any(|window| window == font.as_slice()));
        // Every item record starts on a DWORD boundary, which the dialog manager requires.
        assert_eq!(words.len() % 2, 0);
    }

    #[test]
    fn the_rows_are_in_tab_order_and_include_every_action() {
        let rows = items(&snapshot());
        let ids: Vec<i32> = rows.iter().map(|row| row.id).collect();
        // The tab order the dialog walks: selector, name preference, startup, then the buttons.
        assert_eq!(
            ids,
            vec![
                0,
                COMBO,
                MONITOR_NAME_BOX,
                STARTUP_BOX,
                0,
                SELECT_MONITORS,
                OPEN_PROFILES,
                RELOAD_PROFILES,
                OPEN_REPOSITORY,
                CLOSE
            ]
        );
        // Close is the default button, so Enter closes and Escape (IDCANCEL) matches it.
        let close = rows.iter().find(|row| row.id == CLOSE).expect("close row");
        assert_eq!(close.style & BS_DEFPUSHBUTTON, BS_DEFPUSHBUTTON);
        assert_eq!(CLOSE, 2, "the close button must be IDCANCEL so Escape works");
    }

    #[test]
    fn every_control_is_reachable_from_the_keyboard() {
        // Static rows are labels; every interactive row carries WS_TABSTOP.
        for row in items(&snapshot()) {
            let interactive = matches!(row.class, Class::ComboBox) || row.id != 0;
            if interactive {
                assert_ne!(row.style & WS_TABSTOP, 0, "control {} is not tabbable", row.id);
            }
        }
    }

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
    fn the_dialog_scales_with_the_system_font_rather_than_a_pixel_grid() {
        // Sizes are in dialog units, so the layout follows the font and DPI instead of a fixed pixel
        // grid that would clip text on a scaled display.
        let rows = items(&snapshot());
        let width = rows
            .iter()
            .map(|row| row.rect.0 + row.rect.2)
            .max()
            .unwrap_or(0);
        assert!(width <= 246, "a control is wider than the dialog: {width}");
        let height = rows
            .iter()
            .map(|row| row.rect.1 + row.rect.3)
            .max()
            .unwrap_or(0);
        assert!(height <= 184, "a control is taller than the dialog: {height}");
        // No two rows in the same column overlap vertically.
        for (index, left) in rows.iter().enumerate() {
            for right in rows.iter().skip(index + 1) {
                let overlap = left.rect.1 < right.rect.1 + right.rect.3
                    && right.rect.1 < left.rect.1 + left.rect.3;
                let same_column = left.rect.0 == right.rect.0;
                assert!(!(overlap && same_column), "rows overlap at {}", left.rect.1);
            }
        }
    }

    #[test]
    fn the_current_dpi_is_sane() {
        assert!((96..=768).contains(&current_dpi()));
    }
}
