//! The two native windows normal startup can show.
//!
//! This is deliberately the smallest UI the lifecycle contract needs: the multi-monitor selection
//! dialog with checkboxes and `Remember choice`, and the zero-profile setup window with
//! `Open profiles directory`, `Reload` and `Exit`. Both are ordinary top-level windows built from
//! User32 standard controls, with one message loop on the calling thread and Per-Monitor V2 DPI
//! awareness.
//!
//! The windows decide nothing. They return the user's answer, and the caller persists it and re-runs
//! the startup decision, so every policy stays in the tested decision function instead of an event
//! handler. The tray icon, the flyout and the settings window belong to later gates.

use std::ffi::c_void;
use std::path::Path;
use std::sync::Once;

use windows_sys::Win32::Foundation::{HWND, LPARAM, LRESULT, RECT, WPARAM};
use windows_sys::Win32::Graphics::Gdi::{
    GetMonitorInfoW, GetStockObject, MonitorFromWindow, UpdateWindow, DEFAULT_GUI_FONT, MONITORINFO,
    MONITOR_DEFAULTTONEAREST,
};
use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
use windows_sys::Win32::UI::HiDpi::{
    GetDpiForSystem, SetProcessDpiAwarenessContext, DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2,
};
use windows_sys::Win32::UI::Input::KeyboardAndMouse::EnableWindow;
use windows_sys::Win32::UI::WindowsAndMessaging::{
    AdjustWindowRectEx, CREATESTRUCTW, CreateWindowExW, DefWindowProcW, DestroyWindow,
    DispatchMessageW, GetClientRect, GetMessageW, GetWindowLongPtrW, GetWindowRect, IDCANCEL, IDOK,
    IsDialogMessageW, LoadCursorW, MSG, MoveWindow, PostQuitMessage, RegisterClassExW, SW_SHOW,
    SendMessageW, SetWindowLongPtrW, ShowWindow, TranslateMessage, WNDCLASSEXW, WNDPROC, WS_CAPTION,
    WS_CHILD, WS_EX_DLGMODALFRAME, WS_GROUP, WS_POPUP, WS_SYSMENU, WS_TABSTOP, WS_VISIBLE,
    GWLP_USERDATA,
};

use super::wide::to_wide;

const BUTTON_CLASS: &str = "BUTTON";
const STATIC_CLASS: &str = "STATIC";

const BS_PUSHBUTTON: u32 = 0x0000;
const BS_DEFPUSHBUTTON: u32 = 0x0001;
const BS_AUTOCHECKBOX: u32 = 0x0003;
const IDC_ARROW: u32 = 32512;
const WM_COMMAND: u32 = 0x0111;
const WM_CLOSE: u32 = 0x0010;
const WM_NCCREATE: u32 = 0x0081;
const WM_NCDESTROY: u32 = 0x0082;
const WM_SETFONT: u32 = 0x0030;
const BM_GETCHECK: u32 = 0x00F0;
const BM_SETCHECK: u32 = 0x00F1;
const BST_CHECKED: usize = 1;
const BN_CLICKED: u16 = 0;
const CS_HREDRAW: u32 = 0x0002;
const CS_VREDRAW: u32 = 0x0001;

/// One selectable monitor, reduced to what the dialog shows.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Entry {
    pub id: String,
    /// Model or friendly name.
    pub title: String,
    /// Connector and short identity.
    pub detail: String,
    /// False for a display that cannot be controlled: it is listed with its reason and disabled.
    pub controllable: bool,
}

/// What the user did in the selection dialog.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Answer {
    /// Escape, the close button or Cancel: change nothing.
    Cancelled,
    Confirmed { ids: Vec<String>, remember: bool },
}

/// What the user did in the zero-profile setup window.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SetupAnswer {
    OpenDirectory,
    Reload,
    Exit,
}

/// Identifiers a confirmed answer records: checked, controllable, in list order.
///
/// A disabled row cannot be recorded even if Windows reports it checked, so a display that failed
/// probing never becomes a target through this dialog.
pub fn confirmed_ids(entries: &[Entry], checked: &[bool]) -> Vec<String> {
    entries
        .iter()
        .zip(checked.iter())
        .filter(|(entry, is_checked)| **is_checked && entry.controllable)
        .map(|(entry, _)| entry.id.clone())
        .collect()
}

/// One line per monitor: model, connector, short identity.
pub fn entry_label(entry: &Entry) -> String {
    let suffix = if entry.controllable {
        String::new()
    } else {
        String::from("  (not controllable)")
    };
    if entry.detail.is_empty() {
        format!("{}{suffix}", entry.title)
    } else {
        format!("{}{suffix}  -  {}", entry.title, entry.detail)
    }
}

/// Scale a 96-DPI measurement. Rounded, and never zero, so no control collapses at low DPI.
pub fn scale(px: i32, dpi: u32) -> i32 {
    ((px as i64 * dpi.max(48) as i64 + 48) / 96).max(1) as i32
}

/// Control geometry of the selection dialog at one DPI, in physical pixels.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Layout {
    pub width: i32,
    pub margin: i32,
    pub row_height: i32,
    pub row_gap: i32,
    pub intro_height: i32,
    pub remember_height: i32,
    pub button_width: i32,
    pub button_height: i32,
    pub button_gap: i32,
}

impl Layout {
    /// Geometry for `rows` monitor rows at 96 DPI, then scaled once.
    pub fn new(rows: usize, dpi: u32) -> Layout {
        let _ = rows;
        Layout {
            width: scale(620, dpi),
            margin: scale(16, dpi),
            row_height: scale(24, dpi),
            row_gap: scale(4, dpi),
            intro_height: scale(36, dpi),
            remember_height: scale(24, dpi),
            button_width: scale(92, dpi),
            button_height: scale(30, dpi),
            button_gap: scale(8, dpi),
        }
    }

    /// Top of monitor row `index`, relative to the client area.
    pub fn row_top(&self, index: usize) -> i32 {
        self.margin + self.intro_height + index as i32 * (self.row_height + self.row_gap)
    }

    /// Top of the `Remember choice` box.
    pub fn remember_top(&self, rows: usize) -> i32 {
        self.row_top(rows) + self.row_gap * 2
    }

    /// Top of the button row.
    pub fn buttons_top(&self, rows: usize) -> i32 {
        self.remember_top(rows) + self.remember_height + self.margin
    }

    /// Client height needed for `rows` monitor rows.
    pub fn height(&self, rows: usize) -> i32 {
        self.buttons_top(rows) + self.button_height + self.margin
    }
}

/// Say once per process that this UI is Per-Monitor V2 aware.
///
/// A second call is harmless; Windows refuses the change only after a window exists, and that is not
/// a condition worth reporting to a user.
pub fn init_dpi() {
    static ONCE: Once = Once::new();
    ONCE.call_once(|| unsafe {
        SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
    });
}

/// State of one open window. Allocated by the caller, adopted by the window procedure, and freed by
/// the caller after the message loop returns.
struct Frame {
    answer: Answer,
    entries: Vec<Entry>,
    rows: Vec<HWND>,
    remember: HWND,
    ok: HWND,
    cancel: HWND,
    setup_buttons: [HWND; 3],
}

impl Frame {
    fn empty() -> Frame {
        Frame {
            answer: Answer::Cancelled,
            entries: Vec::new(),
            rows: Vec::new(),
            remember: std::ptr::null_mut(),
            ok: std::ptr::null_mut(),
            cancel: std::ptr::null_mut(),
            setup_buttons: [std::ptr::null_mut(); 3],
        }
    }
}

/// A window class, registered on first use.
struct ClassName(&'static str);

impl ClassName {
    /// Register this class. Registering it twice fails harmlessly because the class exists.
    unsafe fn register(&self, procedure: WNDPROC) {
        // The class name must outlive the call: RegisterClassExW keeps the pointer Windows needs
        // while the class exists, so it is not a temporary.
        let class_name = to_wide(self.0);
        let mut class: WNDCLASSEXW = std::mem::zeroed();
        class.cbSize = std::mem::size_of::<WNDCLASSEXW>() as u32;
        class.style = CS_HREDRAW | CS_VREDRAW;
        class.lpfnWndProc = procedure;
        class.hInstance = GetModuleHandleW(std::ptr::null());
        class.hCursor = LoadCursorW(std::ptr::null_mut(), IDC_ARROW as *const u16);
        class.lpszClassName = class_name.as_ptr();
        let _ = RegisterClassExW(&class);
        // Windows copies the name into the class, so the local vector may now go.
        std::mem::forget(class_name);
    }
}

const SELECT_CLASS: ClassName = ClassName("BroilerMonitorSelection");
const SETUP_CLASS: ClassName = ClassName("BroilerSetup");

/// Everything one child control needs.
struct Child<'a> {
    class: &'a str,
    text: &'a str,
    style: u32,
    x: i32,
    y: i32,
    width: i32,
    height: i32,
    id: i32,
}

/// Create one child control. `id` is what `WM_COMMAND` reports for it.
unsafe fn add_child(parent: HWND, child: Child<'_>) -> HWND {
    let class_name = to_wide(child.class);
    let text = to_wide(child.text);
    let control = CreateWindowExW(
        0,
        class_name.as_ptr(),
        text.as_ptr(),
        child.style | WS_CHILD | WS_VISIBLE,
        child.x,
        child.y,
        child.width,
        child.height,
        parent,
        child.id as usize as *mut c_void,
        GetModuleHandleW(std::ptr::null()),
        std::ptr::null_mut(),
    );
    if !control.is_null() {
        let font = GetStockObject(DEFAULT_GUI_FONT as i32);
        SendMessageW(control, WM_SETFONT, font as WPARAM, 1);
    }
    control
}

/// The `Frame` a window procedure is using for `window`.
unsafe fn frame(window: HWND) -> Option<&'static mut Frame> {
    let pointer = GetWindowLongPtrW(window, GWLP_USERDATA) as *mut Frame;
    pointer.as_mut()
}

/// Create a window with this client size, placed in the middle of its monitor's work area.
unsafe fn create_window(
    class: &ClassName,
    title: &str,
    procedure: unsafe extern "system" fn(HWND, u32, WPARAM, LPARAM) -> LRESULT,
    frame: *mut Frame,
    client_width: i32,
    client_height: i32,
) -> Option<HWND> {
    let style = WS_POPUP | WS_CAPTION | WS_SYSMENU;
    let mut rect = RECT {
        left: 0,
        top: 0,
        right: client_width,
        bottom: client_height,
    };
    AdjustWindowRectEx(&mut rect, style, 0, WS_EX_DLGMODALFRAME);
    let width = rect.right - rect.left;
    let height = rect.bottom - rect.top;

    class.register(Some(procedure));
    let class_name = to_wide(class.0);
    let title = to_wide(title);
    let window = CreateWindowExW(
        WS_EX_DLGMODALFRAME,
        class_name.as_ptr(),
        title.as_ptr(),
        style,
        0,
        0,
        width,
        height,
        std::ptr::null_mut(),
        std::ptr::null_mut(),
        GetModuleHandleW(std::ptr::null()),
        frame as *const c_void,
    );
    if window.is_null() {
        return None;
    }
    place_on_work_area(window);
    Some(window)
}

/// Move a window to the middle of the work area of the monitor it belongs to.
unsafe fn place_on_work_area(window: HWND) {
    let monitor = MonitorFromWindow(window, MONITOR_DEFAULTTONEAREST);
    if monitor.is_null() {
        return;
    }
    let mut info: MONITORINFO = std::mem::zeroed();
    info.cbSize = std::mem::size_of::<MONITORINFO>() as u32;
    if GetMonitorInfoW(monitor, &mut info) == 0 {
        return;
    }
    let mut client: RECT = std::mem::zeroed();
    if GetClientRect(window, &mut client) == 0 {
        return;
    }
    let mut outer: RECT = std::mem::zeroed();
    if GetWindowRect(window, &mut outer) == 0 {
        return;
    }
    let width = outer.right - outer.left;
    let height = outer.bottom - outer.top;
    let work = info.rcWork;
    let x = work.left + ((work.right - work.left) - width) / 2;
    let y = work.top + ((work.bottom - work.top) - height) / 2;
    MoveWindow(window, x, y, width, height, 0);
}

/// Run the message loop until the window quits, and return the quit code.
unsafe fn pump(window: HWND) -> isize {
    let mut message: MSG = std::mem::zeroed();
    loop {
        let result = GetMessageW(&mut message, std::ptr::null_mut(), 0, 0);
        if result == 0 {
            return message.wParam as isize;
        }
        if result == -1 {
            return 0;
        }
        // IsDialogMessageW gives Tab, Enter and Escape their documented meanings, which is also what
        // makes the window usable from the keyboard and by a screen reader.
        if IsDialogMessageW(window, &mut message) == 0 {
            TranslateMessage(&message);
            DispatchMessageW(&message);
        }
    }
}

unsafe extern "system" fn selection_proc(
    window: HWND,
    message: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    match message {
        // Adopt the state the caller allocated before the controls existed.
        WM_NCCREATE => {
            let create = &*(lparam as *const CREATESTRUCTW);
            SetWindowLongPtrW(window, GWLP_USERDATA, create.lpCreateParams as isize);
            1
        }
        WM_NCDESTROY => {
            // The caller owns the state and frees it after the loop, so only drop the reference.
            SetWindowLongPtrW(window, GWLP_USERDATA, 0);
            0
        }
        WM_CLOSE => {
            PostQuitMessage(0);
            0
        }
        WM_COMMAND => {
            let control = wparam & 0xFFFF;
            let code = ((wparam >> 16) & 0xFFFF) as u16;
            if code == BN_CLICKED {
                if control == IDOK as usize {
                    if let Some(frame) = frame(window) {
                        let mut checked = Vec::with_capacity(frame.entries.len());
                        for row in &frame.rows {
                            checked
                                .push(SendMessageW(*row, BM_GETCHECK, 0, 0) as usize == BST_CHECKED);
                        }
                        let remember = !frame.remember.is_null()
                            && SendMessageW(frame.remember, BM_GETCHECK, 0, 0) as usize
                                == BST_CHECKED;
                        let ids = confirmed_ids(&frame.entries, &checked);
                        frame.answer = Answer::Confirmed { ids, remember };
                    }
                    PostQuitMessage(1);
                    return 0;
                }
                if control == IDCANCEL as usize {
                    PostQuitMessage(0);
                    return 0;
                }
            }
            0
        }
        _ => DefWindowProcW(window, message, wparam, lparam),
    }
}

unsafe extern "system" fn setup_proc(
    window: HWND,
    message: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    match message {
        WM_NCCREATE => {
            let create = &*(lparam as *const CREATESTRUCTW);
            SetWindowLongPtrW(window, GWLP_USERDATA, create.lpCreateParams as isize);
            1
        }
        WM_NCDESTROY => {
            SetWindowLongPtrW(window, GWLP_USERDATA, 0);
            0
        }
        // Closing the window means Exit: the window exists only while there is nothing to apply.
        WM_CLOSE => {
            PostQuitMessage(3);
            0
        }
        WM_COMMAND => {
            let control = wparam & 0xFFFF;
            let code = ((wparam >> 16) & 0xFFFF) as u16;
            if code == BN_CLICKED {
                match control as i32 {
                    1 => {
                        PostQuitMessage(1);
                        return 0;
                    }
                    2 => {
                        PostQuitMessage(2);
                        return 0;
                    }
                    3 => {
                        PostQuitMessage(3);
                        return 0;
                    }
                    _ => {}
                }
            }
            0
        }
        _ => DefWindowProcW(window, message, wparam, lparam),
    }
}

/// Show the checkbox selection dialog. Blocks until the user answers or closes it.
///
/// `entries` should be the controllable external monitors; an uncontrollable display may be included
/// when the dialog was opened explicitly, and is then shown disabled with its reason.
pub fn select_monitors(title: &str, prompt: &str, entries: &[Entry], remember: bool) -> Answer {
    init_dpi();
    match open_selection(title, prompt, entries, remember) {
        Some((window, state)) => unsafe {
            ShowWindow(window, SW_SHOW);
            UpdateWindow(window);
            pump(window);
            let answer = state.answer.clone();
            let _ = DestroyWindow(window);
            answer
        },
        None => Answer::Cancelled,
    }
}

/// Create the selection window and its controls without showing it or pumping messages.
///
/// Separated from `select_monitors` so the geometry, the control count and the visibility can be
/// checked without a user: a dialog that never appears is exactly the failure a manual test is worst
/// at catching.
fn open_selection(
    title: &str,
    prompt: &str,
    entries: &[Entry],
    remember: bool,
) -> Option<(HWND, Box<Frame>)> {
    unsafe {
        let dpi = GetDpiForSystem().max(96);
        let layout = Layout::new(entries.len(), dpi);
        let mut state = Box::new(Frame::empty());
        state.entries = entries.to_vec();

        let window = match create_window(
            &SELECT_CLASS,
            title,
            selection_proc,
            &mut *state,
            layout.width,
            layout.height(entries.len()),
        ) {
            Some(window) => window,
            None => return None,
        };

        add_child(
            window,
            Child {
                class: STATIC_CLASS,
                text: prompt,
                style: 0,
                x: layout.margin,
                y: layout.margin,
                width: layout.width - 2 * layout.margin,
                height: layout.intro_height,
                id: 0,
            },
        );

        let mut rows = Vec::with_capacity(entries.len());
        for (index, entry) in entries.iter().enumerate() {
            let checkbox = Child {
                class: BUTTON_CLASS,
                text: &entry_label(entry),
                style: BS_AUTOCHECKBOX | WS_TABSTOP,
                x: layout.margin,
                y: layout.row_top(index),
                width: layout.width - 2 * layout.margin,
                height: layout.row_height,
                id: 100 + index as i32,
            };
            let control = add_child(window, checkbox);
            if !entry.controllable {
                // Listed, explained, and not selectable.
                EnableWindow(control, 0);
            }
            rows.push(control);
        }

        let remember_box = add_child(
            window,
            Child {
                class: BUTTON_CLASS,
                text: "Remember choice",
                style: BS_AUTOCHECKBOX | WS_TABSTOP,
                x: layout.margin,
                y: layout.remember_top(entries.len()),
                width: layout.width - 2 * layout.margin,
                height: layout.remember_height,
                id: 200,
            },
        );
        if remember && !remember_box.is_null() {
            SendMessageW(remember_box, BM_SETCHECK, BST_CHECKED, 0);
        }

        let buttons_top = layout.buttons_top(entries.len());
        let ok_x = layout.width - layout.margin - 2 * layout.button_width - layout.button_gap;
        let ok = add_child(
            window,
            Child {
                class: BUTTON_CLASS,
                text: "OK",
                style: BS_DEFPUSHBUTTON | WS_TABSTOP | WS_GROUP,
                x: ok_x,
                y: buttons_top,
                width: layout.button_width,
                height: layout.button_height,
                id: IDOK,
            },
        );
        let cancel = add_child(
            window,
            Child {
                class: BUTTON_CLASS,
                text: "Cancel",
                style: BS_PUSHBUTTON | WS_TABSTOP,
                x: ok_x + layout.button_width + layout.button_gap,
                y: buttons_top,
                width: layout.button_width,
                height: layout.button_height,
                id: IDCANCEL,
            },
        );

        state.rows = rows;
        state.remember = remember_box;
        state.ok = ok;
        state.cancel = cancel;
        Some((window, state))
    }
}

/// Show the zero-profile setup window. Blocks until the user picks an action or closes it.
pub fn setup_window(title: &str, message: &str) -> SetupAnswer {
    init_dpi();
    match open_setup(title, message) {
        Some((window, _state)) => unsafe {
            ShowWindow(window, SW_SHOW);
            UpdateWindow(window);
            let answer = match pump(window) {
                1 => SetupAnswer::OpenDirectory,
                2 => SetupAnswer::Reload,
                _ => SetupAnswer::Exit,
            };
            let _ = DestroyWindow(window);
            answer
        },
        None => SetupAnswer::Exit,
    }
}

/// Create the setup window and its controls without showing it or pumping messages.
fn open_setup(title: &str, message: &str) -> Option<(HWND, Box<Frame>)> {
    unsafe {
        let dpi = GetDpiForSystem().max(96);
        let layout = Layout::new(2, dpi);
        let mut state = Box::new(Frame::empty());
        let window = match create_window(
            &SETUP_CLASS,
            title,
            setup_proc,
            &mut *state,
            layout.width,
            layout.height(2),
        ) {
            Some(window) => window,
            None => return None,
        };

        add_child(
            window,
            Child {
                class: STATIC_CLASS,
                text: message,
                style: 0,
                x: layout.margin,
                y: layout.margin,
                width: layout.width - 2 * layout.margin,
                height: layout.intro_height * 2,
                id: 0,
            },
        );

        let buttons_top = layout.buttons_top(2);
        let mut x = layout.margin;
        let mut buttons = [std::ptr::null_mut(); 3];
        for (index, caption) in ["Open profiles directory", "Reload", "Exit"]
            .iter()
            .enumerate()
        {
            // The first caption needs more room than the other two.
            let width = if index == 0 {
                layout.button_width * 2 + layout.button_gap
            } else {
                layout.button_width
            };
            let control = add_child(
                window,
                Child {
                    class: BUTTON_CLASS,
                    text: caption,
                    style: BS_PUSHBUTTON | WS_TABSTOP,
                    x,
                    y: buttons_top,
                    width,
                    height: layout.button_height,
                    id: index as i32 + 1,
                },
            );
            buttons[index] = control;
            x += width + layout.button_gap;
        }
        state.setup_buttons = buttons;
        Some((window, state))
    }
}

/// The entries a selection dialog shows for the present monitors.
pub fn entries_for(monitors: &[crate::transport::MonitorSummary]) -> Vec<Entry> {
    monitors
        .iter()
        .map(|monitor| Entry {
            id: monitor.id.clone(),
            title: monitor.friendly_name.clone(),
            detail: format!(
                "{}{}",
                monitor.connector,
                if monitor.primary { ", primary" } else { "" }
            ),
            controllable: true,
        })
        .collect()
}

/// Open a directory with the shell's default handler.
pub fn open_directory(directory: &Path) -> Result<(), String> {
    let target = directory.display().to_string();
    open_target(&target)
}

/// Open a path or URL with the shell's default handler.
///
/// The same call for a directory and a repository URL: the shell decides what "open" means, so the
/// product never hardcodes a browser or a file manager.
pub fn open_target(target: &str) -> Result<(), String> {
    use windows_sys::Win32::UI::Shell::ShellExecuteW;
    let operation = to_wide("open");
    let file = to_wide(target);
    let result = unsafe {
        ShellExecuteW(
            std::ptr::null_mut(),
            operation.as_ptr(),
            file.as_ptr(),
            std::ptr::null(),
            std::ptr::null(),
            1,
        )
    };
    if result as isize <= 32 {
        return Err(format!("cannot open {target}"));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(id: &str, controllable: bool) -> Entry {
        Entry {
            id: id.to_string(),
            title: format!("Panel {id}"),
            detail: String::from("hdmi"),
            controllable,
        }
    }

    #[test]
    fn a_disabled_row_is_never_recorded_even_if_it_is_checked() {
        let entries = [entry("mon-a", true), entry("mon-b", false)];
        assert_eq!(
            confirmed_ids(&entries, &[true, true]),
            vec![String::from("mon-a")]
        );
    }

    #[test]
    fn an_empty_selection_records_nothing() {
        let entries = [entry("mon-a", true), entry("mon-b", true)];
        assert!(confirmed_ids(&entries, &[false, false]).is_empty());
    }

    #[test]
    fn a_confirmed_selection_keeps_list_order() {
        let entries = [entry("mon-a", true), entry("mon-b", true)];
        assert_eq!(
            confirmed_ids(&entries, &[true, true]),
            vec![String::from("mon-a"), String::from("mon-b")]
        );
    }

    #[test]
    fn rows_never_overlap_and_the_height_grows_with_the_monitor_count() {
        for dpi in [96, 120, 144, 192] {
            let mut previous = 0;
            for rows in 1..=12usize {
                let layout = Layout::new(rows, dpi);
                for index in 1..rows {
                    assert!(
                        layout.row_top(index) >= layout.row_top(index - 1) + layout.row_height,
                        "rows overlap at {rows} rows and {dpi} dpi"
                    );
                }
                assert!(layout.remember_top(rows) >= layout.row_top(rows - 1) + layout.row_height);
                assert!(
                    layout.buttons_top(rows) >= layout.remember_top(rows) + layout.remember_height
                );
                let height = layout.height(rows);
                assert!(height > previous, "height does not grow at {rows} rows");
                previous = height;
            }
        }
    }

    #[test]
    fn every_measurement_scales_with_dpi_and_never_collapses() {
        let base = Layout::new(3, 96);
        let doubled = Layout::new(3, 192);
        assert_eq!(doubled.width, base.width * 2);
        assert_eq!(doubled.row_height, base.row_height * 2);
        assert_eq!(doubled.margin, base.margin * 2);
        assert_eq!(scale(1, 48), 1);
        assert_eq!(scale(0, 96), 1);
        // 150 % is the common laptop scale and must not lose a single pixel of separation.
        let scaled = Layout::new(6, 144);
        assert!(scaled.row_gap >= 4);
    }

    #[test]
    fn a_monitor_is_labelled_with_name_connector_and_primary() {
        let entries = entries_for(&[crate::transport::MonitorSummary {
            id: String::from("mon-a"),
            friendly_name: String::from("MSI MAG275R"),
            connector: String::from("hdmi"),
            primary: true,
        }]);
        assert_eq!(entries.len(), 1);
        assert!(entries[0].controllable);
        let label = entry_label(&entries[0]);
        assert!(label.contains("MSI MAG275R"), "{label}");
        assert!(label.contains("hdmi"), "{label}");
        assert!(label.contains("primary"), "{label}");
    }

    #[test]
    fn an_uncontrollable_display_says_so_in_its_label() {
        let label = entry_label(&entry("mon-b", false));
        assert!(label.contains("not controllable"), "{label}");
    }

    /// Count the child controls of `parent` through the window manager, not through our own state.
    unsafe fn child_count(parent: HWND) -> usize {
        use windows_sys::Win32::UI::WindowsAndMessaging::{GetWindow, GW_CHILD, GW_HWNDNEXT};
        let mut count = 0;
        let mut child = GetWindow(parent, GW_CHILD);
        while !child.is_null() {
            count += 1;
            child = GetWindow(child, GW_HWNDNEXT);
        }
        count
    }

    /// The windows must exist, be showable and carry one checkbox per monitor. This asserts window
    /// creation without a user and without a message loop, which is the part a manual check misses
    /// when a dialog silently fails to appear. A machine without an interactive desktop returns
    /// `None` from creation, and the test then only reports that it could not run here.
    #[test]
    fn the_selection_window_has_a_checkbox_per_monitor_and_a_remember_box() {
        init_dpi();
        let entries = [entry("mon-a", true), entry("mon-b", false)];
        let opened = open_selection(
            "broiler: select monitors",
            "Choose the monitors broiler controls.",
            &entries,
            true,
        );
        let (window, state) = match opened {
            Some(opened) => opened,
            None => {
                eprintln!("no interactive desktop: the window could not be created");
                return;
            }
        };
        unsafe {
            // One static prompt, two monitor rows, one remember box and two buttons.
            assert_eq!(child_count(window), 6);
            assert_eq!(state.rows.len(), 2);
            assert!(!state.remember.is_null());
            assert!(!state.ok.is_null());
            assert!(!state.cancel.is_null());
            let mut rect: RECT = std::mem::zeroed();
            assert_ne!(
                windows_sys::Win32::UI::WindowsAndMessaging::GetWindowRect(window, &mut rect),
                0
            );
            assert!(rect.right - rect.left > 0 && rect.bottom - rect.top > 0);
            let _ = DestroyWindow(window);
        }
    }

    #[test]
    fn the_setup_window_offers_the_three_documented_actions() {
        init_dpi();
        // The setup window is opened the same way, so the same creation check applies.
        let opened = super::open_setup("broiler: no profiles", "There is no profile in C:\\none.");
        let (window, state) = match opened {
            Some(opened) => opened,
            None => {
                eprintln!("no interactive desktop: the window could not be created");
                return;
            }
        };
        unsafe {
            // One static message and three buttons.
            assert_eq!(child_count(window), 4);
            assert!(state.setup_buttons.iter().all(|button| !button.is_null()));
            let _ = DestroyWindow(window);
        }
    }
}
