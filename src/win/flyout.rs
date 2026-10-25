//! The modeless profile flyout.
//!
//! A compact popup anchored to one tray icon. Its rows are a standard themed list view driven by an
//! image list, not a hand-drawn button per profile: that is what gives selection, hover, keyboard
//! navigation, a visible focus rectangle and the screen-reader behaviour of a real control.
//!
//! Two cues are kept apart on purpose. Which profile the monitor is *in* is a leading state column —
//! an icon plus the word `Current` — while the row the user is pointing at or has moved to with the
//! arrow keys is the list view's own selection and focus. Marking the current profile by selecting
//! its row would make "what is true" and "what is focused" the same thing, and they are not.
//!
//! Pending and failed operations get a short status line at the bottom instead of being appended to
//! profile names, so a name never changes because an operation is running.
//!
//! There is deliberately no Settings control here: this surface picks a profile, and every other
//! action lives in the settings window or on the command line.

use std::ffi::c_void;

use windows_sys::Win32::Foundation::{HWND, LPARAM, POINT, RECT, WPARAM};
use windows_sys::Win32::Graphics::Gdi::{
    CreateDIBSection, DeleteObject, GetMonitorInfoW, GetStockObject, MonitorFromPoint,
    BITMAPINFO, BITMAPINFOHEADER, BI_RGB, DIB_RGB_COLORS, HBITMAP, HMONITOR, MONITORINFO,
    MONITOR_DEFAULTTONEAREST,
};
use windows_sys::Win32::UI:: Controls::{
    ImageList_Add, ImageList_Create, ImageList_Destroy, InitCommonControlsEx, ICC_LISTVIEW_CLASSES,
    INITCOMMONCONTROLSEX, LVCOLUMNW, LVITEMW, NMHDR, NMITEMACTIVATE, NMLVKEYDOWN,
};
use windows_sys::Win32::UI::HiDpi::GetDpiForSystem;
use windows_sys::Win32::UI::Input::KeyboardAndMouse::SetFocus;
use windows_sys::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DefWindowProcW, DestroyWindow, GetCursorPos, GetSystemMetrics,
    GetWindowLongPtrW, IsWindow, LoadCursorW, RegisterClassExW, SendMessageW, SetForegroundWindow,
    SetWindowLongPtrW, SetWindowPos, ShowWindow, CREATESTRUCTW, CS_DROPSHADOW, CS_HREDRAW,
    CS_VREDRAW, GWLP_USERDATA, IDC_ARROW, SM_CXSCREEN, SM_CYSCREEN, SWP_NOACTIVATE, SWP_NOZORDER,
    SW_SHOW, WNDCLASSEXW, WS_BORDER, WS_CHILD, WS_EX_TOOLWINDOW, WS_EX_TOPMOST, WS_POPUP,
    WS_TABSTOP, WS_VISIBLE,
};

use crate::win::wide::to_wide;

/// Controls the flyout creates.
const TITLE_ID: i32 = 1;
const LIST_ID: i32 = 2;
const STATUS_ID: i32 = 3;

/// `WC_LISTVIEW`.
const LISTVIEW_CLASS: &str = "SysListView32";

/// The private message range, spelled out because not every `windows-sys` version exports `WM_APP`.
const WM_APP: u32 = 0x8000;

/// Owner to tray: open a flyout for the monitor in `wParam`.
pub const OPEN: u32 = WM_APP + 40;

/// Apply result: `wParam` is the row index, `lParam` is 0 for success or 1 for failure.
pub const RESULT: u32 = WM_APP + 41;

// List view messages, from `commctrl.h`.
const LVM_SETIMAGELIST: u32 = 0x1003;
const LVM_SETITEMSTATE: u32 = 0x102B;
const LVM_SETEXTENDEDLISTVIEWSTYLE: u32 = 0x1036;
const LVM_GETNEXTITEM: u32 = 0x100C;
const LVM_SETITEMW: u32 = 0x104C;
const LVM_INSERTITEMW: u32 = 0x104D;
const LVM_INSERTCOLUMNW: u32 = 0x1061;
const LVIF_TEXT: u32 = 0x0001;
const LVIF_IMAGE: u32 = 0x0002;
const LVIF_PARAM: u32 = 0x0004;
const LVIF_STATE: u32 = 0x0008;
const LVIS_FOCUSED: u32 = 0x0001;
const LVS_REPORT: u32 = 0x0001;
const LVS_SINGLESEL: u32 = 0x0004;
const LVS_SHOWSELALWAYS: u32 = 0x0008;
const LVS_NOCOLUMNHEADER: u32 = 0x4000;
const LVS_EX_FULLROWSELECT: u32 = 0x0000_0020;
const LVS_EX_SUBITEMIMAGES: u32 = 0x0000_0002;
const LVS_EX_DOUBLEBUFFER: u32 = 0x0001_0000;
const LVNI_FOCUSED: u32 = 0x0001;
const LVSIL_SMALL: usize = 1;
/// `LVN_FIRST - 55`: the list view's key-down notification.
const LVN_KEYDOWN: i32 = -155;
/// `NM_CLICK`, as it arrives in `NMHDR.code`.
const NM_CLICK: i32 = -2;
const VK_RETURN: usize = 0x0D;
const VK_ESCAPE: usize = 0x1B;

/// The state one row can be in. `Pending` and `Failed` describe an operation, not a profile, so they
/// are shown in the status line instead of in the row caption.
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
    /// The profile's icon name, drawn in the row.
    pub icon: String,
    pub state: State,
}

/// What the owner hands over when it opens the flyout.
#[derive(Clone, Debug)]
pub struct Content {
    pub title: String,
    pub items: Vec<Item>,
}

/// The header of the flyout.
///
/// Several controlled monitors are always identified by index and name, because the popup belongs to
/// one icon and the user has to be able to tell which. A single monitor uses its name only when
/// `tray.always_show_monitor_name` is on; otherwise the heading is the neutral `Profiles`, which is
/// what "omit the monitor name" means for a window that still needs a title.
pub fn heading(index: usize, total: usize, name: &str, show_name: bool) -> String {
    if total > 1 {
        return format!("Monitor {index} \u{2014} {name}");
    }
    if show_name {
        return name.to_string();
    }
    String::from("Profiles")
}

/// The caption of one row: the profile's own name, never decorated with operation state.
pub fn item_text(name: &str, _state: &State) -> String {
    name.to_string()
}

/// The text of the leading state column: the accessible name of the current marker.
pub fn marker_text(state: &State) -> &'static str {
    match state {
        State::Current => "Current",
        _ => "",
    }
}

/// The short status line under the rows. Empty means "nothing to report".
pub fn status_text(state: &State, name: &str) -> String {
    match state {
        State::Normal | State::Current => String::new(),
        State::Pending => format!("Applying {name}\u{2026}"),
        State::Failed(reason) => format!("{name}: {reason}"),
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

/// Sizes for one DPI. Everything scales, so the popup stays readable at 200 %.
struct Metrics {
    width: i32,
    margin: i32,
    title_height: i32,
    row_height: i32,
    status_height: i32,
    gap: i32,
    /// Icon and marker size in pixels.
    icon: i32,
    /// The leading state column: wide enough for the marker icon and the word `Current`.
    state_column: i32,
}

impl Metrics {
    fn new(dpi: u32) -> Metrics {
        let scale = |px: i32| (px * dpi as i32 + 48) / 96;
        Metrics {
            width: scale(300),
            margin: scale(12),
            title_height: scale(24),
            row_height: scale(26),
            status_height: scale(20),
            gap: scale(6),
            icon: scale(16),
            state_column: scale(84),
        }
    }

    fn list_height(&self, rows: usize) -> i32 {
        (rows.max(1) as i32) * self.row_height + 2
    }

    fn title_top(&self) -> i32 {
        self.margin
    }

    fn list_top(&self) -> i32 {
        self.margin + self.title_height + self.gap
    }

    fn status_top(&self, rows: usize) -> i32 {
        self.list_top() + self.list_height(rows) + self.gap
    }

    fn height(&self, rows: usize) -> i32 {
        self.status_top(rows) + self.status_height + self.margin
    }
}

/// The open popup.
pub struct Flyout {
    window: HWND,
    list: HWND,
    status: HWND,
    /// The image list: 0 is blank, 1 is the current marker, then one per profile icon.
    images: isize,
    items: Vec<Item>,
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
            if self.images != 0 {
                ImageList_Destroy(self.images as isize);
                self.images = 0;
            }
        }
    }
}

/// Index 0 of the image list: nothing to show. Index 1: the current-profile marker.
const IMAGE_BLANK: i32 = 0;
const IMAGE_CURRENT: i32 = 1;
const IMAGES_FIXED: usize = 2;

unsafe extern "system" fn flyout_proc(
    window: HWND,
    message: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> isize {
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
        // Losing the foreground closes the popup: it is one click away, and a popup that stays behind
        // other windows is a stray window. `WM_ACTIVATE` with an inactive, non-minimised state is a
        // click elsewhere, which is the documented dismissal gesture for a tray flyout.
        0x0006 => {
            let activated = (wparam & 0xFFFF) as u32;
            if activated == 0 {
                DestroyWindow(window);
            }
            0
        }
        0x0100 => {
            // Escape dismisses without applying anything.
            if wparam == VK_ESCAPE {
                DestroyWindow(window);
            }
            0
        }
        0x0010 => {
            DestroyWindow(window);
            0
        }
        // WM_NOTIFY: a click, a double click or Enter on the focused row starts the apply.
        0x004E => {
            let header = &*(lparam as *const NMHDR);
            if header.idFrom as i32 != LIST_ID {
                return 0;
            }
            match header.code as i32 {
                NM_CLICK => {
                    let item = &*(lparam as *const NMITEMACTIVATE);
                    activ(flyout, item.iItem);
                    0
                }
                LVN_KEYDOWN => {
                    let key = &*(lparam as *const NMLVKEYDOWN);
                    if key.wVKey as usize == VK_RETURN {
                        let focused = SendMessageW(
                            flyout.list,
                            LVM_GETNEXTITEM,
                            usize::MAX, // -1: from the beginning
                            LVNI_FOCUSED as isize,
                        );
                        activ(flyout, focused as i32);
                    }
                    0
                }
                _ => 0,
            }
        }
        _ => DefWindowProcW(window, message, wparam, lparam),
    }
}

/// Report a chosen row to the owner. Out-of-range indices are ignored rather than guessed.
fn activ(flyout: &Flyout, index: i32) {
    if index < 0 || index as usize >= flyout.items.len() {
        return;
    }
    crate::log_info!("flyout row {index} chosen for slot {}", flyout.target);
    (flyout.choose)(flyout.user, flyout.target, index as usize);
}

const FLYOUT_CLASS: &str = "BroilerFlyout";

unsafe fn register_class() {
    let class_name = to_wide(FLYOUT_CLASS);
    let mut class: WNDCLASSEXW = std::mem::zeroed();
    class.cbSize = std::mem::size_of::<WNDCLASSEXW>() as u32;
    // CS_DROPSHADOW is the documented way for a popup to get the shell's shadow instead of a
    // hand-painted one.
    class.style = CS_HREDRAW | CS_VREDRAW | CS_DROPSHADOW;
    class.lpfnWndProc = Some(flyout_proc);
    class.hInstance = windows_sys::Win32::System::LibraryLoader::GetModuleHandleW(std::ptr::null());
    class.hCursor = LoadCursorW(std::ptr::null_mut(), IDC_ARROW as *const u16);
    // The dialog face, so the heading and the status line sit on the window's own colour.
    class.hbrBackground = (windows_sys::Win32::Graphics::Gdi::COLOR_WINDOW + 1) as usize as *mut c_void;
    class.lpszClassName = class_name.as_ptr();
    let _ = RegisterClassExW(&class);
    // The class name is copied into the registered class, so the local vector may go.
    std::mem::forget(class_name);
}

/// Ask for the list-view class before one is created.
pub fn init_common_controls() {
    unsafe {
        let mut settings: INITCOMMONCONTROLSEX = std::mem::zeroed();
        settings.dwSize = std::mem::size_of::<INITCOMMONCONTROLSEX>() as u32;
        settings.dwICC = ICC_LISTVIEW_CLASSES;
        InitCommonControlsEx(&settings);
    }
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
        id as usize as *mut c_void as windows_sys::Win32::UI::WindowsAndMessaging::HMENU,
        windows_sys::Win32::System::LibraryLoader::GetModuleHandleW(std::ptr::null()),
        std::ptr::null_mut(),
    );
    if !control.is_null() {
        let font = GetStockObject(windows_sys::Win32::Graphics::Gdi::DEFAULT_GUI_FONT as i32);
        SendMessageW(control, 0x0030, font as WPARAM, 1);
    }
    control
}

/// A bitmap and its pixels for the image list, from one rendered icon.
unsafe fn dib(bitmap: &crate::tray::Bitmap) -> Option<(HBITMAP, *mut c_void)> {
    let mut info: BITMAPINFO = std::mem::zeroed();
    info.bmiHeader.biSize = std::mem::size_of::<BITMAPINFOHEADER>() as u32;
    info.bmiHeader.biWidth = bitmap.width as i32;
    // A negative height requests a top-down bitmap, matching the pixel order of `Bitmap`.
    info.bmiHeader.biHeight = -(bitmap.height as i32);
    info.bmiHeader.biPlanes = 1;
    info.bmiHeader.biBitCount = 32;
    info.bmiHeader.biCompression = BI_RGB as u32;
    let mut bits: *mut c_void = std::ptr::null_mut();
    let colour = CreateDIBSection(
        std::ptr::null_mut(),
        &info,
        DIB_RGB_COLORS,
        &mut bits,
        std::ptr::null_mut(),
        0,
    );
    if colour.is_null() || bits.is_null() {
        return None;
    }
    std::ptr::copy_nonoverlapping(
        bitmap.pixels.as_ptr(),
        bits as *mut u8,
        bitmap.pixels.len(),
    );
    Some((colour, bits))
}

/// The marker: a filled dot on a transparent square, the same accent colour at every size.
fn marker_bitmap(size: u32) -> crate::tray::Bitmap {
    let mut bitmap = crate::tray::Bitmap {
        width: size,
        height: size,
        pixels: vec![0; (size * size * 4) as usize],
    };
    let centre = size as f32 / 2.0;
    let radius = size as f32 * 0.28;
    for y in 0..size as i32 {
        for x in 0..size as i32 {
            let dx = x as f32 + 0.5 - centre;
            let dy = y as f32 + 0.5 - centre;
            if dx * dx + dy * dy <= radius * radius {
                let offset = ((y as u32 * size + x as u32) * 4) as usize;
                // BGRA: the accent blue #2F6FEB, fully opaque.
                bitmap.pixels[offset] = 0xEB;
                bitmap.pixels[offset + 1] = 0x6F;
                bitmap.pixels[offset + 2] = 0x2F;
                bitmap.pixels[offset + 3] = 255;
            }
        }
    }
    bitmap
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
            init_common_controls();
            let dpi = GetDpiForSystem().max(96);
            let metrics = Metrics::new(dpi);
            let rows = content.items.len().max(1);
            let width = metrics.width;
            let height = metrics.height(rows);

            let mut flyout = Box::new(Flyout {
                window: std::ptr::null_mut(),
                list: std::ptr::null_mut(),
                status: std::ptr::null_mut(),
                images: 0,
                items: content.items.clone(),
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

            let list = add_control(
                window,
                LISTVIEW_CLASS,
                "",
                LVS_REPORT
                    | LVS_SINGLESEL
                    | LVS_SHOWSELALWAYS
                    | LVS_NOCOLUMNHEADER
                    | WS_BORDER
                    | WS_TABSTOP,
                metrics.margin,
                metrics.list_top(),
                width - 2 * metrics.margin,
                metrics.list_height(rows),
                LIST_ID,
            );
            if list.is_null() {
                crate::log_error!("the flyout list view could not be created");
                return None;
            }
            flyout.list = list;
            SendMessageW(
                list,
                LVM_SETEXTENDEDLISTVIEWSTYLE,
                0,
                (LVS_EX_FULLROWSELECT | LVS_EX_SUBITEMIMAGES | LVS_EX_DOUBLEBUFFER) as isize,
            );

            // One image list serves every row: a blank, the current marker, and one image per profile.
            let size = metrics.icon as u32;
            let images: isize = ImageList_Create(
                size as i32,
                size as i32,
                0x0020 | 0x0001, // ILC_COLOR32 | ILC_MASK
                (content.items.len() + IMAGES_FIXED) as i32,
                0,
            );
            if images == 0 {
                crate::log_error!("the flyout image list could not be created");
                return None;
            }
            flyout.images = images;
            for bitmap in [crate::tray::Bitmap {
                width: size,
                height: size,
                pixels: vec![0; (size * size * 4) as usize],
            }, marker_bitmap(size)] {
                if let Some((colour, _)) = dib(&bitmap) {
                    ImageList_Add(images as isize, colour, std::ptr::null_mut());
                    DeleteObject(colour as *mut c_void);
                }
            }
            for item in &content.items {
                let bitmap = crate::tray::render(
                    crate::tray::Glyph::from_name(&item.icon),
                    None,
                    size,
                );
                if let Some((colour, _)) = dib(&bitmap) {
                    ImageList_Add(images as isize, colour, std::ptr::null_mut());
                    DeleteObject(colour as *mut c_void);
                }
            }
            SendMessageW(list, LVM_SETIMAGELIST, LVSIL_SMALL, images);

            // Two columns: the state marker, then the profile with its icon.
            for (index, (label, width)) in [
                ("", metrics.state_column),
                ("Profile", width - 2 * metrics.margin - metrics.state_column - 4),
            ]
            .iter()
            .enumerate()
            {
                let mut column: LVCOLUMNW = std::mem::zeroed();
                column.mask = 0x0002; // LVCF_WIDTH
                column.cx = *width;
                let mut text = to_wide(label);
                if !label.is_empty() {
                    column.mask |= 0x0001; // LVCF_TEXT
                    column.pszText = text.as_mut_ptr();
                }
                SendMessageW(list, LVM_INSERTCOLUMNW, index, &mut column as *mut LVCOLUMNW as isize);
                // The label vector only has to live for the duration of the call.
                std::mem::forget(text);
            }

            for (index, item) in content.items.iter().enumerate() {
                let mut marker = to_wide(marker_text(&item.state));
                let mut row: LVITEMW = std::mem::zeroed();
                row.mask = LVIF_TEXT | LVIF_IMAGE | LVIF_PARAM;
                row.iItem = index as i32;
                row.pszText = marker.as_mut_ptr();
                row.iImage = if item.state == State::Current {
                    IMAGE_CURRENT
                } else {
                    IMAGE_BLANK
                };
                row.lParam = index as isize;
                SendMessageW(list, LVM_INSERTITEMW, 0, &row as *const LVITEMW as isize);
                std::mem::forget(marker);

                let mut name = to_wide(&item_text(&item.name, &item.state));
                let mut subitem: LVITEMW = std::mem::zeroed();
                subitem.mask = LVIF_TEXT | LVIF_IMAGE;
                subitem.iItem = index as i32;
                subitem.iSubItem = 1;
                subitem.pszText = name.as_mut_ptr();
                subitem.iImage = (IMAGES_FIXED + index) as i32;
                SendMessageW(list, LVM_SETITEMW, 0, &subitem as *const LVITEMW as isize);
                std::mem::forget(name);
            }

            // Focus the first row so the keyboard works immediately; the selection stays empty, which
            // keeps "focused" visibly distinct from "current".
            let mut state: LVITEMW = std::mem::zeroed();
            state.mask = LVIF_STATE;
            state.state = LVIS_FOCUSED;
            state.stateMask = LVIS_FOCUSED;
            SendMessageW(list, LVM_SETITEMSTATE, 0, &state as *const LVITEMW as isize);

            let status = add_control(
                window,
                "STATIC",
                "",
                0,
                metrics.margin,
                metrics.status_top(rows),
                width - 2 * metrics.margin,
                metrics.status_height,
                STATUS_ID,
            );
            flyout.status = status;

            // The anchor is the icon rectangle when the shell can report one, and the pointer
            // otherwise, which is where the user just clicked.
            let anchor_rect = anchor.unwrap_or_else(|| {
                let mut pointer = POINT { x: 0, y: 0 };
                GetCursorPos(&mut pointer);
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
            SetFocus(list);
            Some(flyout)
        }
    }

    pub fn window_id(&self) -> isize {
        self.window as isize
    }

    pub fn target(&self) -> usize {
        self.target
    }

    /// Replace one row's state: the leading marker and the status line, never the profile name.
    pub fn set_state(&mut self, index: usize, state: State) {
        let Some(item) = self.items.get_mut(index) else {
            return;
        };
        item.state = state.clone();
        unsafe {
            let mut marker = to_wide(marker_text(&state));
            let mut row: LVITEMW = std::mem::zeroed();
            row.mask = LVIF_TEXT | LVIF_IMAGE;
            row.iItem = index as i32;
            row.iSubItem = 0;
            row.pszText = marker.as_mut_ptr();
            row.iImage = if state == State::Current {
                IMAGE_CURRENT
            } else {
                IMAGE_BLANK
            };
            SendMessageW(self.list, LVM_SETITEMW, 0, &row as *const LVITEMW as isize);
            std::mem::forget(marker);
        }
        let line = status_text(&state, &item.name);
        self.set_status(&line);
    }

    /// The short status line: what is happening now, or why the last attempt failed.
    pub fn set_status(&mut self, text: &str) {
        if self.status.is_null() {
            return;
        }
        let text = to_wide(text);
        unsafe {
            windows_sys::Win32::UI::WindowsAndMessaging::SetWindowTextW(self.status, text.as_ptr());
        }
    }

    /// Bring the rows in line with state that was just re-read.
    ///
    /// Rows are matched by profile id, so the marker follows the coordinator's answer rather than the
    /// row the user last clicked. A registry that gained or lost a profile needs new rows, which this
    /// call refuses instead of half-applying: it returns false and leaves the window as it was.
    pub fn sync(&mut self, content: &Content) -> bool {
        if self.items.len() != content.items.len() {
            return false;
        }
        for (index, fresh) in content.items.iter().enumerate() {
            if self.items[index].id != fresh.id {
                return false;
            }
        }
        for (index, fresh) in content.items.iter().enumerate() {
            if self.items[index].state != fresh.state {
                self.set_state(index, fresh.state.clone());
            }
        }
        true
    }

    /// How many rows the popup shows.
    pub fn item_count(&self) -> usize {
        self.items.len()
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
        assert_eq!(heading(2, 2, "MSI MAG275R", false), "Monitor 2 \u{2014} MSI MAG275R");
        // One monitor with the name preference off: a neutral heading, no name to omit.
        assert_eq!(heading(1, 1, "MSI MAG275R", false), "Profiles");
        assert_eq!(heading(1, 1, "MSI MAG275R", true), "MSI MAG275R");
        // Several monitors are always identified, whatever the preference says.
        assert_eq!(heading(1, 3, "MSI MAG275R", false), "Monitor 1 \u{2014} MSI MAG275R");
    }

    #[test]
    fn a_row_caption_is_the_profile_name_and_nothing_else() {
        // Names stay stable: an operation in flight must not rewrite a row's text.
        for state in [
            State::Normal,
            State::Current,
            State::Pending,
            State::Failed(String::from("busy")),
        ] {
            assert_eq!(item_text("Night", &state), "Night", "{state:?}");
        }
    }

    #[test]
    fn the_current_marker_is_separate_from_focus_and_selection() {
        // The marker is a leading column with its own accessible name, not the list view's selection.
        assert_eq!(marker_text(&State::Current), "Current");
        for state in [State::Normal, State::Pending, State::Failed(String::from("x"))] {
            assert_eq!(marker_text(&state), "");
        }
    }

    #[test]
    fn pending_and_failed_operations_use_the_status_line() {
        assert_eq!(status_text(&State::Normal, "Night"), "");
        assert_eq!(status_text(&State::Current, "Night"), "");
        assert_eq!(status_text(&State::Pending, "Night"), "Applying Night\u{2026}");
        assert_eq!(
            status_text(&State::Failed(String::from("verification_failed")), "Night"),
            "Night: verification_failed"
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
        assert_eq!(low.width, 300);
        assert_eq!(high.width, 600);
        assert!(high.height(3) > low.height(3));
        // The rows fit inside the window at either scale, with room for the status line.
        for metrics in [Metrics::new(96), Metrics::new(144), Metrics::new(192)] {
            assert!(metrics.height(3) > metrics.status_top(3));
            assert!(metrics.status_top(3) > metrics.list_top());
            // The state column leaves room for the profile column.
            assert!(metrics.state_column < metrics.width - 2 * metrics.margin);
        }
    }

    #[test]
    fn the_marker_bitmap_is_a_transparent_square_with_one_solid_dot() {
        let marker = marker_bitmap(16);
        assert_eq!(marker.width, 16);
        assert_eq!(marker.pixels.len(), 16 * 16 * 4);
        // Corners stay transparent, the centre is opaque.
        let corner = 0usize;
        assert_eq!(marker.pixels[corner + 3], 0);
        let centre = ((8 * 16 + 8) * 4) as usize;
        assert_eq!(marker.pixels[centre + 3], 255);
        // And it is the accent colour, so it does not depend on a system colour that High Contrast
        // would replace with something invisible.
        assert_eq!(&marker.pixels[centre..centre + 3], &[0xEB, 0x6F, 0x2F]);
    }
}
