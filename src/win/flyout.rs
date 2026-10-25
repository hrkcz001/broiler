//! The modeless profile flyout.
//!
//! One reusable popup owns the whole surface: a visible header naming the monitor, one native radio
//! button per profile, and nothing else. There is deliberately no Settings control here: the flyout is
//! a profile picker, and every other action lives in the settings window or on the command line.
//!
//! The flyout never applies anything itself. It reports the chosen index to its owner through a plain
//! function pointer and lets the caller decide, so the hardware policy stays in one place; the caller
//! then calls [`Flyout::set_state`] with the outcome, which is what keeps a pending or failed apply
//! visible instead of pretending the profile took effect.

use std::ffi::c_void;

use windows_sys::Win32::Foundation::{COLORREF, HWND, LPARAM, LRESULT, POINT, RECT, WPARAM};
use windows_sys::Win32::Graphics::Gdi::{
    DeleteObject, DrawFocusRect, DrawTextW, Ellipse, FillRect, GetMonitorInfoW, GetStockObject,
    GetSysColor, GetSysColorBrush, MonitorFromPoint, SelectObject, SetBkMode, SetTextColor,
    COLOR_GRAYTEXT, COLOR_HIGHLIGHT, COLOR_HIGHLIGHTTEXT, COLOR_WINDOW, COLOR_WINDOWTEXT,
    DEFAULT_GUI_FONT, DT_LEFT, DT_SINGLELINE, DT_VCENTER, HMONITOR, MONITORINFO,
    MONITOR_DEFAULTTONEAREST, TRANSPARENT,
};
use windows_sys::Win32::UI::Controls::{DRAWITEMSTRUCT, ODS_DISABLED, ODS_FOCUS, ODS_SELECTED};
use windows_sys::Win32::UI::HiDpi::GetDpiForSystem;
use windows_sys::Win32::UI::Input::KeyboardAndMouse::{EnableWindow, SetFocus};
use windows_sys::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DefWindowProcW, DestroyWindow, GetSystemMetrics, GetWindowLongPtrW, IsWindow,
    LoadCursorW, RegisterClassExW, SendMessageW, SetForegroundWindow, SetWindowLongPtrW,
    SetWindowPos, ShowWindow, CREATESTRUCTW, CS_HREDRAW, CS_VREDRAW, GWLP_USERDATA, HMENU,
    IDC_ARROW, SM_CXSCREEN, SM_CYSCREEN, SWP_NOACTIVATE, SWP_NOZORDER, SW_SHOW, WNDCLASSEXW,
    WS_BORDER, WS_CHILD, WS_EX_TOOLWINDOW, WS_EX_TOPMOST, WS_POPUP, WS_TABSTOP, WS_VISIBLE,
};

use crate::win::wide::to_wide;

/// Controls the flyout creates. `WM_COMMAND` reports these.
const TITLE_ID: i32 = 1;
const FIRST_ITEM_ID: i32 = 100;

/// The private message range, spelled out because not every `windows-sys` version exports `WM_APP`.
const WM_APP: u32 = 0x8000;

/// Owner to tray: open a flyout for the monitor in `wParam`.
pub const OPEN: u32 = WM_APP + 40;

/// Apply result: `wParam` is the row index, `lParam` is 0 for success or 1 for failure.
pub const RESULT: u32 = WM_APP + 41;

/// The state one row can be in. `Pending` and `Failed` exist so the surface never claims a profile
/// applied when the hardware has not confirmed it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum State {
    /// The profile this monitor is verified to be in.
    Current,
    /// Nothing known about this profile and this monitor.
    Normal,
    /// A request is in flight.
    Pending,
    /// The request failed; the reason is shown as given.
    Failed(String),
}

/// One row.
#[derive(Clone, Debug)]
pub struct Item {
    pub id: String,
    pub name: String,
    /// The profile's icon name, drawn to the left of the name.
    pub icon: String,
    pub state: State,
}

/// What the owner hands over when it opens the flyout.
#[derive(Clone, Debug)]
pub struct Content {
    pub title: String,
    pub items: Vec<Item>,
}

/// The header: the product calls it `Monitor 2 — MSI MAG275R`, using the same index the tray badge
/// and the tooltip use, so the three never disagree.
pub fn title_text(index: usize, name: &str) -> String {
    format!("Monitor {index} \u{2014} {name}")
}

/// One row's caption. The state is spelled out because a radio button alone cannot say "failed".
pub fn item_text(name: &str, state: &State) -> String {
    match state {
        // The marker to the left of the name says which profile is current; the caption does not
        // repeat it. A state that is not "nothing special" is still spelled out, because a dot cannot
        // express "this is being written right now" or why it failed.
        State::Current | State::Normal => name.to_string(),
        State::Pending => format!("{name} (applying\u{2026})"),
        State::Failed(reason) => format!("{name} (failed: {reason})"),
    }
}

/// Where the popup goes for an anchor rectangle, kept inside the work area.
///
/// Preference is below the icon and left-aligned with it, which is where the shell puts its own
/// flyouts. Above is used when there is no room below, and the result is clamped last, so an icon in
/// the bottom-right corner of a display with a taskbar still yields an entirely visible popup.
/// Negative desktop coordinates need no special case: the arithmetic is signed throughout.
pub fn place(anchor: RECT, width: i32, height: i32, work: RECT) -> (i32, i32) {
    let below = anchor.bottom;
    let above = anchor.top - height;
    let y = if below + height <= work.bottom {
        below
    } else if above >= work.top {
        above
    } else {
        below
    };
    let x = clamp(anchor.left, work.left, work.right - width);
    let y = clamp(y, work.top, work.bottom - height);
    (x, y)
}

fn clamp(value: i32, low: i32, high: i32) -> i32 {
    if high < low {
        return low;
    }
    value.max(low).min(high)
}

/// Sizes for one DPI. Rows and margins scale, so the popup does not become unreadable at 200 %.
struct Metrics {
    width: i32,
    margin: i32,
    title_height: i32,
    row_height: i32,
    gap: i32,
    /// Icon and marker size in pixels.
    icon: i32,
}

/// `BS_OWNERDRAW`: the row is drawn by `WM_DRAWITEM`.
const BS_OWNERDRAW: u32 = 0x0000000B;

impl Metrics {
    fn new(dpi: u32) -> Metrics {
        let scale = |px: i32| (px * dpi as i32 + 48) / 96;
        Metrics {
            width: scale(260),
            margin: scale(12),
            title_height: scale(30),
            // Rows are taller than the icon, so a 20 px glyph sits in a 34 px row instead of looking
            // lost: the notification area's own 16 px icon is the wrong reference for a menu.
            row_height: scale(34),
            gap: scale(4),
            icon: scale(20),
        }
    }

    fn height(&self, rows: usize) -> i32 {
        2 * self.margin + self.title_height + self.gap + rows as i32 * (self.row_height + self.gap)
    }

    fn title_top(&self) -> i32 {
        self.margin
    }

    fn row_top(&self, index: usize) -> i32 {
        self.margin + self.title_height + self.gap + index as i32 * (self.row_height + self.gap)
    }
}

/// The open popup.
pub struct Flyout {
    window: HWND,
    rows: Vec<HWND>,
    items: Vec<Item>,
    /// Icon size in pixels, scaled for the DPI the popup opened at.
    icon_size: i32,
    /// Index into the owner's monitor list, echoed back with every choice.
    target: usize,
    user: *mut c_void,
    choose: fn(*mut c_void, usize, usize),
}

impl Drop for Flyout {
    fn drop(&mut self) {
        unsafe {
            if !self.window.is_null() && IsWindow(self.window) != 0 {
                DestroyWindow(self.window);
            }
            self.window = std::ptr::null_mut();
        }
    }
}

unsafe extern "system" fn flyout_proc(
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
    let flyout = GetWindowLongPtrW(window, GWLP_USERDATA) as *mut Flyout;
    let Some(flyout) = flyout.as_ref() else {
        return DefWindowProcW(window, message, wparam, lparam);
    };

    match message {
        // Losing the foreground closes the popup. This is the documented behaviour for a tray flyout:
        // it is one click away, and a popup that stays behind other windows is a stray window.
        0x0006 => {
            let activated = (wparam & 0xFFFF) as u32;
            if activated == 0 {
                DestroyWindow(window);
            }
            0
        }
        0x0010 => {
            // Destroyed by the owner or by focus loss; nothing else is pending.
            0
        }
        0x0100 => {
            // Escape dismisses without applying anything.
            if wparam as u32 == 0x1B {
                DestroyWindow(window);
            }
            0
        }
        0x002B => {
            // WM_DRAWITEM: one row, drawn as marker + profile icon + name.
            let item = &*(lparam as *const DRAWITEMSTRUCT);
            if item.CtlType == 4 {
                let index = (item.CtlID as i32 - FIRST_ITEM_ID).max(0) as usize;
                if let Some(drawn) = flyout.items.get(index) {
                    draw_row(flyout, drawn, item);
                }
            }
            1
        }
        0x0111 => {
            let control = (wparam & 0xFFFF) as i32;
            let code = ((wparam >> 16) & 0xFFFF) as u16;
            // A click reports BN_CLICKED (0) with the control handle in `lParam`. Requiring the
            // handle is what distinguishes a real activation from a stray command: a notification
            // with no sender must never apply a profile.
            let sender = lparam as HWND;
            let matches_row = flyout.rows.iter().any(|row| *row == sender);
            if code == 0 && matches_row {
                if let Some(index) = control.checked_sub(FIRST_ITEM_ID) {
                    if (index as usize) < flyout.items.len() {
                        crate::log_info!(
                            "flyout row {index} chosen for slot {}",
                            flyout.target
                        );
                        (flyout.choose)(flyout.user, flyout.target, index as usize);
                    }
                }
            }
            0
        }
        _ => DefWindowProcW(window, message, wparam, lparam),
    }
}

const FLYOUT_CLASS: &str = "BroilerFlyout";

unsafe fn register_class() {
    let class_name = to_wide(FLYOUT_CLASS);
    let mut class: WNDCLASSEXW = std::mem::zeroed();
    class.cbSize = std::mem::size_of::<WNDCLASSEXW>() as u32;
    class.style = CS_HREDRAW | CS_VREDRAW;
    class.lpfnWndProc = Some(flyout_proc);
    class.hInstance = windows_sys::Win32::System::LibraryLoader::GetModuleHandleW(std::ptr::null());
    class.hCursor = LoadCursorW(std::ptr::null_mut(), IDC_ARROW as *const u16);
    class.lpszClassName = class_name.as_ptr();
    let _ = RegisterClassExW(&class);
    // The class name is copied into the registered class, so the local vector may go.
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

impl Flyout {
    /// Create and show the popup for `content`, anchored to the tray icon at `anchor`.
    ///
    /// `target` is the monitor slot the choice is reported for; `choose` runs on the UI thread and
    /// must not block.
    pub fn open(
        owner: HWND,
        anchor: Option<RECT>,
        content: Content,
        target: usize,
        user: *mut c_void,
        choose: fn(*mut c_void, usize, usize),
    ) -> Option<Box<Flyout>> {
        unsafe {
            register_class();
            let dpi = GetDpiForSystem().max(96);
            let metrics = Metrics::new(dpi);
            let rows = content.items.len().max(1);
            let width = metrics.width;
            let height = metrics.height(rows);

            let mut flyout = Box::new(Flyout {
                window: std::ptr::null_mut(),
                rows: Vec::new(),
                items: content.items.clone(),
                icon_size: metrics.icon, // placeholder until the geometry is known
                target,
                user,
                choose,
            });

            let class_name = to_wide(FLYOUT_CLASS);
            let title = to_wide(&content.title);
            let window = CreateWindowExW(
                WS_EX_TOOLWINDOW | WS_EX_TOPMOST,
                class_name.as_ptr(),
                title.as_ptr(),
                WS_POPUP | WS_BORDER,
                0,
                0,
                width,
                height,
                owner,
                std::ptr::null_mut(),
                windows_sys::Win32::System::LibraryLoader::GetModuleHandleW(std::ptr::null()),
                &mut *flyout as *mut Flyout as *const c_void,
            );
            if window.is_null() {
                return None;
            }
            flyout.window = window;

            add_control(
                window,
                "STATIC",
                &content.title,
                0,
                metrics.margin,
                metrics.title_top(),
                width - 2 * metrics.margin,
                metrics.title_height,
                TITLE_ID,
            );

            for (index, item) in content.items.iter().enumerate() {
                // Owner-drawn push buttons: a button gives click, focus, Enter and Space for free,
                // while the drawing puts the profile icon and the current marker where they belong.
                // Radio buttons are not an option: an auto-radio that takes focus sends BN_CLICKED on
                // this build, which applied a profile with no user action.
                let control = add_control(
                    window,
                    "BUTTON",
                    &item_text(&item.name, &item.state),
                    WS_TABSTOP | BS_OWNERDRAW,
                    metrics.margin,
                    metrics.row_top(index),
                    width - 2 * metrics.margin,
                    metrics.row_height,
                    FIRST_ITEM_ID + index as i32,
                );
                flyout.rows.push(control);
            }

            // The anchor is the icon rectangle when the shell can report one, and the pointer
            // otherwise, which is where the user just clicked.
            let anchor_rect = anchor.unwrap_or_else(|| {
                let mut pointer = POINT { x: 0, y: 0 };
                windows_sys::Win32::UI::WindowsAndMessaging::GetCursorPos(&mut pointer);
                RECT {
                    left: pointer.x,
                    top: pointer.y,
                    right: pointer.x,
                    bottom: pointer.y,
                }
            });
            let corner = POINT {
                x: (anchor_rect.left + anchor_rect.right) / 2,
                y: (anchor_rect.top + anchor_rect.bottom) / 2,
            };
            let work = work_area(MonitorFromPoint(corner, MONITOR_DEFAULTTONEAREST)).unwrap_or(RECT {
                left: 0,
                top: 0,
                right: GetSystemMetrics(SM_CXSCREEN),
                bottom: GetSystemMetrics(SM_CYSCREEN),
            });
            let (x, y) = place(anchor_rect, width, height, work);

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
            // The popup needs the foreground to receive the keyboard and to notice focus loss.
            SetForegroundWindow(window);
            if let Some(first) = flyout.rows.first() {
                SetFocus(*first);
            }
            Some(flyout)
        }
    }

    pub fn window_id(&self) -> isize {
        self.window as isize
    }

    pub fn target(&self) -> usize {
        self.target
    }

    /// Replace one row's state, keeping the caption in the form the user reads.
    pub fn set_state(&mut self, index: usize, state: State) {
        let Some(item) = self.items.get_mut(index) else {
            return;
        };
        item.state = state.clone();
        let Some(control) = self.rows.get(index).copied() else {
            return;
        };
        let text = to_wide(&item_text(&item.name, &state));
        unsafe {
            windows_sys::Win32::UI::WindowsAndMessaging::SetWindowTextW(control, text.as_ptr());
            // A row being applied cannot be clicked again; the others stay available.
            EnableWindow(control, i32::from(state != State::Pending));
        }
    }

    /// How many rows the popup shows.
    pub fn item_count(&self) -> usize {
        self.rows.len()
    }

    pub fn is_open(&self) -> bool {
        !self.window.is_null() && unsafe { IsWindow(self.window) != 0 }
    }
}

unsafe fn work_area(monitor: HMONITOR) -> Option<RECT> {
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

/// Draw one row: a filled marker when this profile is current, the profile icon, then the name.
///
/// Everything is painted on a stock background brush, so the row follows the system colours and the
/// selected state the button already tracks.
unsafe fn draw_row(flyout: &Flyout, item: &Item, draw: &DRAWITEMSTRUCT) {
    let rect = draw.rcItem;
    let selected = draw.itemState & ODS_SELECTED != 0;
    let disabled = draw.itemState & ODS_DISABLED != 0;
    let background = if selected {
        COLOR_HIGHLIGHT
    } else {
        COLOR_WINDOW
    };
    let foreground = if disabled {
        COLOR_GRAYTEXT
    } else if selected {
        COLOR_HIGHLIGHTTEXT
    } else {
        COLOR_WINDOWTEXT
    };

    FillRect(draw.hDC, &rect, GetSysColorBrush(background));

    let size = flyout.icon_size;
    let mark = (size / 2).max(3);
    let padding = (size / 2).max(4);
    let middle = rect.top + (rect.bottom - rect.top) / 2;
    let mark_centre = rect.left + padding + mark / 2;

    if item.state == State::Current {
        let brush = windows_sys::Win32::Graphics::Gdi::CreateSolidBrush(
            GetSysColor(foreground),
        );
        if !brush.is_null() {
            let previous = SelectObject(draw.hDC, brush as *mut c_void);
            Ellipse(
                draw.hDC,
                mark_centre - mark / 2,
                middle - mark / 2,
                mark_centre + mark / 2 + 1,
                middle + mark / 2 + 1,
            );
            SelectObject(draw.hDC, previous);
            DeleteObject(brush as *mut c_void);
        }
    }

    // The profile icon, pixel by pixel: the glyph already exists as a rendered bitmap, and at 16 to
    // 32 pixels this is cheaper than managing a mask.
    let icon_left = rect.left + padding + mark + padding;
    let icon_top = middle - size / 2;
    let glyph = crate::tray::render(
        crate::tray::Glyph::from_name(&item.icon),
        None,
        size as u32,
    );
    let colour: COLORREF = GetSysColor(foreground);
    for y in 0..glyph.height as i32 {
        for x in 0..glyph.width as i32 {
            let index = ((y as u32 * glyph.width + x as u32) * 4) as usize;
            if glyph.pixels.get(index + 3).copied().unwrap_or(0) == 0 {
                continue;
            }
            windows_sys::Win32::Graphics::Gdi::SetPixelV(
                draw.hDC,
                icon_left + x,
                icon_top + y,
                colour,
            );
        }
    }

    // The text follows the icon closely; a gap as wide as the icon reads as two columns.
    let text_left = icon_left + size + size / 3;
    let mut text = to_wide(&item_text(&item.name, &item.state));
    SetBkMode(draw.hDC, TRANSPARENT as i32);
    SetTextColor(draw.hDC, colour);
    let mut text_rect = RECT {
        left: text_left,
        top: rect.top,
        right: rect.right - padding,
        bottom: rect.bottom,
    };
    DrawTextW(
        draw.hDC,
        text.as_mut_ptr(),
        -1,
        &mut text_rect,
        DT_LEFT | DT_VCENTER | DT_SINGLELINE,
    );

    if draw.itemState & ODS_FOCUS != 0 {
        let mut focus = rect;
        focus.left += 1;
        focus.top += 1;
        focus.right -= 1;
        focus.bottom -= 1;
        DrawFocusRect(draw.hDC, &focus);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const WORK: RECT = RECT {
        left: 0,
        top: 0,
        right: 1920,
        bottom: 1080,
    };

    #[test]
    fn the_header_names_the_monitor_with_the_same_index_as_the_badge() {
        assert_eq!(title_text(2, "MSI MAG275R"), "Monitor 2 \u{2014} MSI MAG275R");
    }

    #[test]
    fn a_row_says_what_is_true_about_it() {
        assert_eq!(item_text("Night", &State::Normal), "Night");
        // The marker to the left of the name carries "current"; the caption does not repeat it.
        assert_eq!(item_text("Night", &State::Current), "Night");
        assert_eq!(item_text("Night", &State::Pending), "Night (applying\u{2026})");
        assert_eq!(
            item_text("Night", &State::Failed(String::from("busy"))),
            "Night (failed: busy)"
        );
    }

    #[test]
    fn a_popup_opens_below_the_icon_and_inside_the_work_area() {
        let anchor = RECT {
            left: 100,
            top: 20,
            right: 120,
            bottom: 40,
        };
        assert_eq!(place(anchor, 260, 200, WORK), (100, 40));
    }

    #[test]
    fn an_icon_at_the_bottom_right_flips_the_popup_above_the_taskbar() {
        // An overflow-tray icon: bottom-right corner, taskbar occupying the last 48 pixels.
        let work = RECT {
            left: 0,
            top: 0,
            right: 1920,
            bottom: 1032,
        };
        let anchor = RECT {
            left: 1860,
            top: 1044,
            right: 1880,
            bottom: 1064,
        };
        let (x, y) = place(anchor, 260, 200, work);
        // Entirely visible: neither off the right edge nor under the taskbar.
        assert!(x + 260 <= work.right, "x={x}");
        assert!(y + 200 <= work.bottom, "y={y}");
        assert!(y >= work.top);
        // Above the icon, and then pulled up again because the icon itself hangs below the work
        // area, so the popup ends flush with the bottom of it rather than under the taskbar.
        assert!(y < anchor.top, "y={y}");
        assert_eq!(y, work.bottom - 200);
    }

    #[test]
    fn negative_desktop_coordinates_are_handled_without_special_cases() {
        // A display to the left of the primary one has negative coordinates.
        let work = RECT {
            left: -1920,
            top: -200,
            right: 0,
            bottom: 880,
        };
        let anchor = RECT {
            left: -1900,
            top: -180,
            right: -1880,
            bottom: -160,
        };
        let (x, y) = place(anchor, 260, 200, work);
        assert_eq!((x, y), (-1900, -160));
    }

    #[test]
    fn a_popup_taller_than_the_work_area_is_clamped_to_its_top() {
        let work = RECT {
            left: 0,
            top: 0,
            right: 800,
            bottom: 600,
        };
        let anchor = RECT {
            left: 700,
            top: 500,
            right: 720,
            bottom: 520,
        };
        let (x, y) = place(anchor, 260, 900, work);
        assert_eq!(x, 800 - 260);
        assert_eq!(y, 0);
    }

    #[test]
    fn metrics_scale_with_dpi() {
        let low = Metrics::new(96);
        let high = Metrics::new(192);
        assert_eq!(low.width, 260);
        assert_eq!(high.width, 520);
        assert!(high.height(3) > low.height(3));
        // Rows never overlap at either scale.
        assert!(low.row_top(1) >= low.row_top(0) + low.row_height);
        assert!(high.row_top(1) >= high.row_top(0) + high.row_height);
    }
}
