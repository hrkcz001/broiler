//! The notification-area icons.
//!
//! One icon per controlled monitor, with a stable identity GUID derived from the monitor id, so a
//! restarted coordinator or a restarted Explorer gets the same icon back instead of a duplicate. The
//! shell integration follows the documented rules: `NOTIFYICON_VERSION_4`, `NIM_SETVERSION` after
//! every addition, callbacks decoded from the packed version-4 `lParam`, a hidden top-level owner
//! window for broadcast lifecycle notifications, recovery when Explorer restarts, and every icon
//! handle destroyed exactly once.
//!
//! The icons are drawn here rather than loaded from a vendor asset: a monochrome glyph with an opaque
//! numeric badge in the bottom-right corner. Icon geometry, the badge and the tooltip text are pure
//! functions, so they are tested without a shell.

use std::ffi::c_void;

use sha2::{Digest, Sha256};
use windows_sys::Win32::Foundation::{HWND, LPARAM, LRESULT, POINT, RECT, WPARAM};
use windows_sys::Win32::Graphics::Gdi::{
    CreateDIBSection, DeleteObject, BITMAPINFO, BITMAPINFOHEADER, BI_RGB, DIB_RGB_COLORS, HBITMAP,
};
use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
use windows_sys::Win32::UI::Shell::{
    Shell_NotifyIconGetRect, Shell_NotifyIconW, NIF_GUID, NIF_ICON, NIF_MESSAGE, NIF_SHOWTIP,
    NIF_TIP, NIM_ADD, NIM_DELETE, NIM_MODIFY, NIM_SETVERSION, NOTIFYICONDATAW, NOTIFYICONIDENTIFIER,
    NOTIFYICON_VERSION_4,
};
use windows_sys::Win32::UI::WindowsAndMessaging::{
    AppendMenuW, CreateIconIndirect, CreatePopupMenu, CreateWindowExW, DefWindowProcW, DestroyIcon,
    DestroyMenu, DestroyWindow, DispatchMessageW, GetCursorPos, GetMessageW, GetSystemMetrics,
    InsertMenuW, LoadCursorW, PostMessageW, PostQuitMessage, RegisterClassExW, RegisterWindowMessageW,
    SetForegroundWindow, SetTimer, SetWindowLongPtrW, TrackPopupMenu, TranslateMessage,
    KillTimer, CREATESTRUCTW,
    GWLP_USERDATA, ICONINFO, MF_SEPARATOR, MF_STRING, SM_CXSMICON, TPM_RETURNCMD, TPM_RIGHTBUTTON,
    WM_APP, WM_CONTEXTMENU, WM_LBUTTONDBLCLK, WM_LBUTTONUP, WM_RBUTTONUP, WNDCLASSEXW,
    WS_EX_TOOLWINDOW, WS_POPUP,
};

use crate::win::wide::to_wide;

/// The private callback message the shell uses for every icon of this window.
const CALLBACK_MESSAGE: u32 = WM_APP + 1;
/// `NIN_SELECT` and `NIN_KEYSELECT` as the shell sends them under version 4.
const NIN_SELECT: u32 = 0x0400;
const NIN_KEYSELECT: u32 = 0x0401;

const MENU_SETTINGS: usize = 1;
const MENU_EXIT: usize = 2;

/// Tray to itself: show the settings window for the current state.
pub const SETTINGS_OPEN: u32 = WM_APP + 42;

/// Tray to itself: re-read the state and update the open settings window.
pub const SETTINGS_REFRESH: u32 = WM_APP + 43;

/// Timer id for the one-shot version-4 retry.
const VERSION_RETRY_TIMER: usize = 1;
const WM_TIMER: u32 = 0x0113;

const ICON_CLASS: &str = "BroilerTrayHost";

/// How the user activated an icon.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Activation {
    /// A left click or an Enter key on the focused icon.
    Primary,
    /// A double click, which some users reach for instead of a single click.
    Secondary,
}

/// The icon artwork a profile can ask for. Names follow the documented set.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Glyph {
    Monitor,
    Custom,
    Day,
    Night,
    Dawn,
    Dusk,
    Coding,
    Reading,
    Movie,
    Gaming,
    Work,
    Presentation,
    Photo,
    Eco,
    Focus,
}

impl Glyph {
    /// The glyph for a profile's icon name, falling back to the neutral monitor.
    pub fn from_name(name: &str) -> Glyph {
        match name {
            "custom" => Glyph::Custom,
            "day" => Glyph::Day,
            "night" => Glyph::Night,
            "dawn" => Glyph::Dawn,
            "dusk" => Glyph::Dusk,
            "coding" => Glyph::Coding,
            "reading" => Glyph::Reading,
            "movie" => Glyph::Movie,
            "gaming" => Glyph::Gaming,
            "work" => Glyph::Work,
            "presentation" => Glyph::Presentation,
            "photo" => Glyph::Photo,
            "eco" => Glyph::Eco,
            "focus" => Glyph::Focus,
            _ => Glyph::Monitor,
        }
    }
}

/// What one monitor's icon shows.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct IconSpec {
    /// Monitor identity, also the source of the stable GUID.
    pub monitor: String,
    pub glyph: Glyph,
    /// Stable 1-based index, shown as a badge only when more than one monitor is controlled.
    pub index: usize,
    pub badges: bool,
}

/// Stable icon identity: the first 16 bytes of the SHA-256 of the monitor id.
///
/// Deriving it instead of generating a random GUID is what makes the icon survive a restart: the same
/// monitor keeps the same identity, and no duplicate icon is left behind by an older process.
pub fn icon_identity(monitor: &str) -> [u8; 16] {
    let digest = Sha256::digest(monitor.as_bytes());
    let mut bytes = [0u8; 16];
    bytes.copy_from_slice(&digest[..16]);
    // Mark it as an RFC 4122 version 5 style name-based identifier. The value is not hashed MD5, so
    // it is only a marker of intent, never a claim of a standard UUID variant.
    bytes[6] = (bytes[6] & 0x0F) | 0x50;
    bytes[8] = (bytes[8] & 0x3F) | 0x80;
    bytes
}

/// The tooltip for one icon: index and name, verified state, brightness when it is known.
///
/// The contract asks for all four pieces, and for the tray tooltip to stay short enough for the
/// notification area.
pub fn tooltip(index: usize, total: usize, name: &str, state: &str, brightness: Option<u32>) -> String {
    let mut text = if total > 1 {
        format!("{index}/{total} {name}")
    } else {
        name.to_string()
    };
    if !state.is_empty() {
        text.push_str(" - ");
        text.push_str(state);
    }
    if let Some(value) = brightness {
        text.push_str(&format!(", {value}%"));
    }
    // The shell truncates at 127 characters plus a terminator.
    if text.chars().count() > 120 {
        text = text.chars().take(119).collect();
        text.push('…');
    }
    text
}

/// A 32-bit BGRA icon bitmap, top-down, premultiplied-free straight alpha.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Bitmap {
    pub width: u32,
    pub height: u32,
    /// Four bytes per pixel, `B, G, R, A`, rows top to bottom.
    pub pixels: Vec<u8>,
}

impl Bitmap {
    fn new(size: u32) -> Bitmap {
        Bitmap {
            width: size,
            height: size,
            pixels: vec![0; (size * size * 4) as usize],
        }
    }

    fn put(&mut self, x: i32, y: i32, value: u8) {
        if x < 0 || y < 0 || x >= self.width as i32 || y >= self.height as i32 {
            return;
        }
        let offset = ((y as u32 * self.width + x as u32) * 4) as usize;
        self.pixels[offset] = value;
        self.pixels[offset + 1] = value;
        self.pixels[offset + 2] = value;
        self.pixels[offset + 3] = 255;
    }

    /// Number of pixels that are not fully transparent.
    pub fn painted(&self) -> usize {
        self.pixels.chunks(4).filter(|pixel| pixel[3] != 0).count()
    }

    /// True when every painted pixel is either white or black, which is what "monochrome" means here.
    pub fn is_monochrome(&self) -> bool {
        self.pixels
            .chunks(4)
            .filter(|pixel| pixel[3] != 0)
            .all(|pixel| pixel[0] == pixel[1] && pixel[1] == pixel[2])
    }
}

/// 3x5 digits, so a two-digit badge needs no separate artwork and no cap at nine monitors.
const DIGITS: [[u8; 5]; 10] = [
    [0b111, 0b101, 0b101, 0b101, 0b111],
    [0b010, 0b110, 0b010, 0b010, 0b111],
    [0b111, 0b001, 0b111, 0b100, 0b111],
    [0b111, 0b001, 0b111, 0b001, 0b111],
    [0b101, 0b101, 0b111, 0b001, 0b001],
    [0b111, 0b100, 0b111, 0b001, 0b111],
    [0b111, 0b100, 0b111, 0b101, 0b111],
    [0b111, 0b001, 0b010, 0b010, 0b010],
    [0b111, 0b101, 0b111, 0b101, 0b111],
    [0b111, 0b101, 0b111, 0b001, 0b111],
];

/// Render one icon: a monochrome glyph, plus the numeric badge when several monitors are controlled.
///
/// Drawing instead of loading an asset keeps the icon set inside the source, which is what the
/// "no vendor logos, no emoji, no font dependency" rule needs, and it makes the geometry testable.
pub fn render(glyph: Glyph, badge: Option<u32>, size: u32) -> Bitmap {
    let mut bitmap = Bitmap::new(size);
    let unit = (size as f32) / 16.0;
    let centre = (size as f32) / 2.0;
    // A badge needs room, so the glyph shrinks into the upper-left three quarters.
    let scale = if badge.is_some() { 0.72 } else { 1.0 };
    let radius = centre * 0.42 * scale;
    let cx = centre - if badge.is_some() { unit * 1.5 } else { 0.0 };
    for y in 0..size as i32 {
        for x in 0..size as i32 {
            let fx = x as f32 + 0.5 - cx;
            let fy = y as f32 + 0.5 - centre;
            let inside = match glyph {
                Glyph::Day => fx * fx + fy * fy <= radius * radius,
                // A crescent: the disk minus a disk shifted up and right.
                Glyph::Night => {
                    let outer = fx * fx + fy * fy <= radius * radius;
                    let cut = (fx - radius * 0.55).powi(2) + (fy + radius * 0.35).powi(2)
                        <= radius * radius * 0.82;
                    outer && !cut
                }
                Glyph::Dawn | Glyph::Dusk => {
                    let disk = fx * fx + fy * fy <= radius * radius;
                    let horizon = fy > radius * 0.55;
                    disk || horizon && fx.abs() <= radius
                }
                Glyph::Monitor => {
                    let body = fx.abs() <= radius && fy.abs() <= radius * 0.72;
                    let stand = fx.abs() <= unit * 1.2 && fy > radius * 0.72 && fy <= radius;
                    let outline = fx.abs() > radius - unit * 1.2 || fy.abs() > radius * 0.72 - unit;
                    (body && outline) || stand
                }
                Glyph::Custom => {
                    // Sliders: two tracks with a knob each.
                    let track = |ymid: f32, knob: f32| {
                        (fy - ymid).abs() <= unit * 0.8 && fx.abs() <= radius
                            || (fx - knob).abs() <= unit * 1.4 && (fy - ymid).abs() <= unit * 1.6
                    };
                    track(-radius * 0.4, -radius * 0.3) || track(radius * 0.4, radius * 0.4)
                }
                Glyph::Coding => {
                    let arm = |sign: f32| {
                        ((fx - sign * radius * 0.5).abs() <= unit * 1.6
                            && (fy.abs() - (radius - fx * sign * 0.0)).abs() < radius * 0.2)
                            || (fx - sign * radius * 0.9).abs() <= unit * 1.6 && fy.abs() <= radius
                    };
                    arm(-1.0) || arm(1.0) || (fy.abs() <= unit && fx.abs() <= radius)
                }
                Glyph::Reading => {
                    // An open book: two pages that meet in the middle.
                    (fy.abs() <= radius * 0.7 && fx.abs() <= radius && (fx - fx.round()).abs() > 0.0)
                        && ((fx > 0.0) ^ (fy.abs() < radius * 0.1))
                }
                Glyph::Movie => {
                    let frame = fx.abs() <= radius && fy.abs() <= radius * 0.78;
                    let border = fx.abs() > radius - unit * 1.4 || fy.abs() > radius * 0.78 - unit;
                    let sprocket = ((fx / (radius * 0.5)).round() * radius * 0.5 - fx).abs()
                        <= unit * 0.8
                        && fy.abs() <= radius * 0.78;
                    frame && (border || sprocket)
                }
                Glyph::Gaming => {
                    // A controller: a rounded body with two grips.
                    let body = fx.abs() <= radius && fy.abs() <= radius * 0.5;
                    let grip = ((fx.abs() - radius * 0.8).powi(2) + (fy - radius * 0.6).powi(2))
                        <= (radius * 0.35).powi(2);
                    body || grip
                }
                Glyph::Work => {
                    let body = fx.abs() <= radius && fy > -radius * 0.35 && fy <= radius * 0.75;
                    let outline = fx.abs() > radius - unit * 1.4 || fy > radius * 0.75 - unit * 1.4;
                    let handle = fx.abs() <= radius * 0.35 && fy <= -radius * 0.35
                        && fy > -radius * 0.75;
                    (body && outline) || handle
                }
                Glyph::Presentation => {
                    let board = fx.abs() <= radius && fy.abs() <= radius * 0.6;
                    let border = fx.abs() > radius - unit * 1.4 || fy.abs() > radius * 0.6 - unit;
                    let leg = (fx * 0.8).abs() <= unit && fy > radius * 0.6 && fy <= radius;
                    (board && border) || leg
                }
                Glyph::Photo => {
                    let body = fx.abs() <= radius && fy.abs() <= radius * 0.7;
                    let lens = fx * fx + fy * fy <= (radius * 0.35).powi(2);
                    let ring = (fx * fx + fy * fy <= (radius * 0.5).powi(2))
                        && !(fx * fx + fy * fy <= (radius * 0.32).powi(2));
                    body && (lens || ring)
                }
                Glyph::Eco => {
                    // A leaf: an ellipse rotated by using a sheared coordinate.
                    let sx = fx * 0.8 - fy * 0.4;
                    let sy = fx * 0.4 + fy * 0.8;
                    (sx * sx) / (radius * radius) + (sy * sy) / (radius * radius * 0.45) <= 1.0
                }
                Glyph::Focus => {
                    // A target: two concentric rings.
                    let outer = fx * fx + fy * fy <= radius * radius;
                    let inner_hole = fx * fx + fy * fy <= (radius * 0.62).powi(2);
                    let core = fx * fx + fy * fy <= (radius * 0.28).powi(2);
                    (outer && !inner_hole) || core
                }
            };
            if inside {
                bitmap.put(x, y, 0xFF);
            }
        }
    }
    if let Some(value) = badge {
        draw_badge(&mut bitmap, value, unit);
    }
    bitmap
}

/// The opaque badge: a white plate with the index in black, in the bottom-right corner.
fn draw_badge(bitmap: &mut Bitmap, value: u32, unit: f32) {
    let digits: Vec<u32> = value
        .to_string()
        .chars()
        .filter_map(|character| character.to_digit(10))
        .collect();
    if digits.is_empty() {
        return;
    }
    let columns = (digits.len() * 4 - 1) as f32;
    let plate_width = (columns * unit + unit).max(unit * 4.0);
    let plate_height = 6.0 * unit;
    let x0 = bitmap.width as f32 - plate_width;
    let y0 = bitmap.height as f32 - plate_height;
    for y in y0 as i32..bitmap.height as i32 {
        for x in x0 as i32..bitmap.width as i32 {
            bitmap.put(x, y, 0xFF);
        }
    }
    for (position, digit) in digits.iter().enumerate() {
        let left = x0 + unit + position as f32 * 4.0 * unit;
        let top = y0 + unit * 0.5;
        for (row, bits) in DIGITS[*digit as usize].iter().enumerate() {
            for column in 0..3 {
                if bits & (0b100 >> column) != 0 {
                    let px = left + column as f32 * unit;
                    let py = top + row as f32 * unit;
                    for dy in 0..unit.ceil() as i32 {
                        for dx in 0..unit.ceil() as i32 {
                            bitmap.put(px as i32 + dx, py as i32 + dy, 0x00);
                        }
                    }
                }
            }
        }
    }
}

/// The callbacks the shell invokes. Function pointers keep the window procedure non-generic.
pub struct Callbacks {
    pub user: *mut c_void,
    /// Tooltip text for the icon at this monitor slot.
    pub tooltip: fn(*mut c_void, usize) -> String,
    /// The user activated the icon at this slot.
    pub activate: fn(*mut c_void, usize, Activation),
    /// The user chose `Settings`.
    pub settings: fn(*mut c_void),
    /// The user chose `Exit`: stop the whole instance.
    pub exit: fn(*mut c_void),
    /// The rows the flyout shows for the monitor at this slot.
    pub flyout: fn(*mut c_void, usize) -> crate::win::flyout::Content,
    /// The user picked row `row` for the monitor at this slot. Must return without blocking.
    pub choose: fn(*mut c_void, usize, usize),
    /// The icon name of the profile this monitor is verified to be in, if any.
    pub glyph: fn(*mut c_void, usize) -> Option<String>,
    /// Why the last apply failed, for the row that reports it.
    pub failure: fn(*mut c_void) -> Option<String>,
    /// The values the settings window shows when it opens.
    pub settings_snapshot: fn(*mut c_void) -> crate::win::settings::Snapshot,
    /// Perform one settings action. False means the change was refused and must not be shown.
    pub settings_apply: fn(*mut c_void, crate::win::settings::Action) -> bool,
}

/// One installed icon.
struct Slot {
    identity: [u8; 16],
    uid: u32,
    /// Index into the caller's monitor list.
    monitor: usize,
    glyph: Glyph,
    badge: Option<u32>,
    icon: isize,
    size: i32,
}

impl Drop for Slot {
    fn drop(&mut self) {
        unsafe {
            if self.icon != 0 {
                DestroyIcon(self.icon as *mut c_void);
                self.icon = 0;
            }
        }
    }
}

/// An installed set of icons and the hidden owner window behind them.
pub struct Tray {
    window: HWND,
    slots: Vec<Slot>,
    callbacks: Callbacks,
    taskbar_created: u32,
    installed: bool,
    /// True when `NIM_SETVERSION` was accepted, which is what makes the packed version-4 `lParam`
    /// meaningful. A shell that refuses it sends the legacy packing instead.
    version_four: bool,
    /// Monitor id per slot, used by the log lines so a failure names the monitor.
    monitors: Vec<String>,
    /// The settings window, while it is open.
    settings: Option<Box<crate::win::settings::Settings>>,
    /// The one reusable flyout. Clicking another icon closes this and opens a new one for that
    /// monitor, which is what "a single reusable flyout" means in practice.
    flyout: Option<Box<crate::win::flyout::Flyout>>,
}

impl Drop for Tray {
    fn drop(&mut self) {
        unsafe {
            for slot in &self.slots {
                self.delete(slot);
            }
            if !self.window.is_null() {
                DestroyWindow(self.window);
                self.window = std::ptr::null_mut();
            }
        }
    }
}

unsafe extern "system" fn tray_proc(
    window: HWND,
    message: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    // Adopt the owner.
    if message == 0x0081 {
        let create = &*(lparam as *const CREATESTRUCTW);
        SetWindowLongPtrW(window, GWLP_USERDATA, create.lpCreateParams as isize);
        return 1;
    }
    if message == 0x0082 {
        SetWindowLongPtrW(window, GWLP_USERDATA, 0);
        return 0;
    }
    // The window user data is the address of the owning `Tray`; reading it through any other type
    // would reinterpret the first bytes of that struct as a pointer.
    let tray = {
        let pointer = windows_sys::Win32::UI::WindowsAndMessaging::GetWindowLongPtrW(
            window,
            GWLP_USERDATA,
        ) as *mut Tray;
        pointer.as_mut()
    };
    let Some(tray) = tray else {
        return DefWindowProcW(window, message, wparam, lparam);
    };

    match message {
        m if m == tray.taskbar_created && m != 0 => {
            // Explorer restarted: every icon is gone, and the identity GUIDs are what let the shell
            // put them back in the same place instead of duplicating them.
            tray.readd_all();
            0
        }
        WM_TIMER => {
            // The one-shot retry for the version-4 behaviour, see `schedule_version_retry`.
            if wparam == VERSION_RETRY_TIMER {
                unsafe { KillTimer(window, VERSION_RETRY_TIMER) };
                tray.request_version_four();
            }
            0
        }
        CALLBACK_MESSAGE => {
            // Version 4 packs the event in the low word and the icon id in the high word; the legacy
            // packing puts the icon id in `wParam` and the mouse message in `lParam`.
            let packed = lparam as u32;
            let (event, uid) = if packed >> 16 != 0 {
                (packed & 0xFFFF, packed >> 16)
            } else {
                (packed, wparam as u32)
            };
            tray.dispatch(event, uid);
            0
        }
        SETTINGS_OPEN => {
            tray.open_settings();
            0
        }
        SETTINGS_REFRESH => {
            tray.refresh_settings();
            0
        }
        crate::win::flyout::OPEN => {
            // A request to show the flyout for the monitor at this slot, from a left click or from
            // the command line.
            tray.open_flyout(wparam as usize);
            0
        }
        crate::win::flyout::RESULT => {
            tray.flyout_result(wparam as usize, lparam == 0);
            0
        }
        0x0111 => {
            let command = (wparam & 0xFFFF) as usize;
            match command {
                MENU_SETTINGS => (tray.callbacks.settings)(tray.callbacks.user),
                MENU_EXIT => (tray.callbacks.exit)(tray.callbacks.user),
                _ => {}
            }
            0
        }
        0x0010 => {
            // The hidden window is never closed by the user, but a shutdown must still be clean.
            PostQuitMessage(0);
            0
        }
        _ => DefWindowProcW(window, message, wparam, lparam),
    }
}

impl Tray {
    /// Install one icon per spec. Returns `None` when the shell refuses the first addition, which is
    /// the honest signal that there is no notification area to talk to.
    pub fn install(specs: &[IconSpec], callbacks: Callbacks) -> Option<Box<Tray>> {
        crate::win::ui::init_dpi();
        let size = unsafe { GetSystemMetrics(SM_CXSMICON) }.max(16);
        let mut tray = Box::new(Tray {
            window: std::ptr::null_mut(),
            slots: Vec::new(),
            callbacks,
            taskbar_created: 0,
            installed: false,
            version_four: true,
            monitors: specs.iter().map(|spec| spec.monitor.clone()).collect(),
            settings: None,
            flyout: None,
        });
        unsafe {
            let class_name = to_wide(ICON_CLASS);
            let mut class: WNDCLASSEXW = std::mem::zeroed();
            class.cbSize = std::mem::size_of::<WNDCLASSEXW>() as u32;
            class.lpfnWndProc = Some(tray_proc);
            class.hInstance = GetModuleHandleW(std::ptr::null());
            class.hCursor = LoadCursorW(std::ptr::null_mut(), 32512 as *const u16);
            class.lpszClassName = class_name.as_ptr();
            let _ = RegisterClassExW(&class);

            let title = to_wide("broiler");
            // A hidden top-level window, not a message-only one: broadcast notifications such as
            // TaskbarCreated only reach top-level windows.
            let window = CreateWindowExW(
                WS_EX_TOOLWINDOW,
                class_name.as_ptr(),
                title.as_ptr(),
                WS_POPUP,
                0,
                0,
                0,
                0,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                GetModuleHandleW(std::ptr::null()),
                &*tray as *const Tray as *mut c_void,
            );
            if window.is_null() {
                std::mem::forget(class_name);
                crate::log_error!("the tray owner window could not be created");
                return None;
            }
            std::mem::forget(class_name);
            tray.window = window;

            let created = to_wide("TaskbarCreated");
            tray.taskbar_created = RegisterWindowMessageW(created.as_ptr());

            let total = specs.len();
            for (position, spec) in specs.iter().enumerate() {
                let badge = if total > 1 && spec.badges {
                    Some(spec.index as u32)
                } else {
                    None
                };
                let identity = icon_identity(&spec.monitor);
                let slot = Slot {
                    identity,
                    uid: (position + 1) as u32,
                    monitor: position,
                    glyph: spec.glyph,
                    badge,
                    icon: 0,
                    size,
                };
                tray.slots.push(slot);
                let index = tray.slots.len() - 1;
                if !tray.add(index) {
                    // A shell that refuses an icon must not leave a half-installed tray behind.
                    return None;
                }
            }
            tray.installed = true;
            if !tray.version_four {
                tray.schedule_version_retry();
            }
        }
        tray.installed.then_some(tray)
    }

    /// Add the icon at `index` and set the version-4 behaviour on it.
    unsafe fn add(&mut self, index: usize) -> bool {
        let slot = &self.slots[index];
        let icon = match render_icon(slot.glyph, slot.badge, slot.size as u32) {
            Some(icon) => icon,
            None => {
                crate::log_error!("the icon bitmap for slot {index} could not be created");
                return false;
            }
        };
        self.slots[index].icon = icon;
        let mut data = self.data(index, NIF_MESSAGE | NIF_ICON | NIF_TIP | NIF_GUID | NIF_SHOWTIP);
        data.uCallbackMessage = CALLBACK_MESSAGE;
        data.hIcon = icon as *mut c_void;
        let tooltip = (self.callbacks.tooltip)(self.callbacks.user, self.slots[index].monitor);
        fill(&mut data.szTip, &tooltip);
        if Shell_NotifyIconW(NIM_ADD, &data) == 0 {
            crate::log_error!("the shell refused icon {index} (NIM_ADD)");
            return false;
        }
        // Required after every addition, including after an Explorer restart. A shell that refuses
        // it still shows the icon, so the refusal downgrades the callback format instead of
        // discarding a working icon: `dispatch` decodes both packings.
        let mut version = self.data(index, 0);
        // `NIM_SETVERSION` is validated against the version-3 size of the structure; passing the
        // Windows 10 size is refused, which would silently leave legacy callbacks in place.
        version.cbSize = version_three_size() as u32;
        version.Anonymous.uVersion = NOTIFYICON_VERSION_4;
        if Shell_NotifyIconW(NIM_SETVERSION, &version) == 0 {
            let code = windows_sys::Win32::Foundation::GetLastError();
            self.version_four = false;
            crate::log_error!(
                "the shell refused the version-4 behaviour (Win32 error {code}); falling back to legacy callbacks"
            );
        }
        crate::log_info!(
            "icon {index} installed for {}",
            self.monitors.get(self.slots[index].monitor).cloned().unwrap_or_default()
        );
        true
    }

    /// Delete the icon of one slot, if it is installed.
    unsafe fn delete(&self, slot: &Slot) {
        if slot.icon == 0 {
            return;
        }
        let data = self.data_for(slot, NIF_GUID);
        Shell_NotifyIconW(NIM_DELETE, &data);
    }

    /// Re-add everything after an Explorer restart.
    fn readd_all(&mut self) {
        for index in 0..self.slots.len() {
            // The old handles were released by the shell; drop ours before composing again.
            self.slots[index].icon = 0;
            unsafe {
                self.add(index);
            }
        }
    }

    /// Ask again once, shortly after installation.
    ///
    /// The shell accepts `NIM_SETVERSION` only once it has registered the icon, and it refuses a call
    /// that arrives in the same instant as `NIM_ADD` on this Windows build. A single delayed retry
    /// costs nothing and either upgrades the callbacks or leaves the logged fallback in place.
    fn schedule_version_retry(&self) {
        unsafe {
            SetTimer(self.window, VERSION_RETRY_TIMER, 400, None);
        }
    }

    /// Retry the version-4 request for every installed icon.
    fn request_version_four(&mut self) {
        let mut accepted = 0;
        for index in 0..self.slots.len() {
            let mut version = self.data(index, 0);
            version.cbSize = version_three_size() as u32;
            version.Anonymous.uVersion = NOTIFYICON_VERSION_4;
            unsafe {
                if Shell_NotifyIconW(NIM_SETVERSION, &version) != 0 {
                    accepted += 1;
                }
            }
        }
        self.version_four = accepted == self.slots.len() && !self.slots.is_empty();
        if self.version_four {
            crate::log_info!("version-4 icon callbacks accepted after the retry");
        } else {
            crate::log_error!(
                "version-4 icon callbacks were refused {accepted}/{} time(s); legacy callbacks stay in effect",
                self.slots.len()
            );
        }
    }

    /// Update one tooltip, for a state change that matters to the user.
    pub fn set_tooltip(&mut self, monitor: usize, text: &str) {
        if let Some(index) = self.slots.iter().position(|slot| slot.monitor == monitor) {
            unsafe {
                let mut data = self.data(index, NIF_TIP | NIF_SHOWTIP | NIF_GUID);
                fill(&mut data.szTip, text);
                Shell_NotifyIconW(NIM_MODIFY, &data);
            }
        }
    }

    /// How many icons are installed. Used by tests and by the startup log.
    pub fn len(&self) -> usize {
        self.slots.len()
    }

    pub fn is_empty(&self) -> bool {
        self.slots.is_empty()
    }

    fn data(&self, index: usize, flags: u32) -> NOTIFYICONDATAW {
        self.data_for(&self.slots[index], flags)
    }

    fn data_for(&self, slot: &Slot, flags: u32) -> NOTIFYICONDATAW {
        let mut data: NOTIFYICONDATAW = unsafe { std::mem::zeroed() };
        data.cbSize = std::mem::size_of::<NOTIFYICONDATAW>() as u32;
        data.hWnd = self.window;
        data.uID = slot.uid;
        data.uFlags = flags;
        data.guidItem = guid(slot.identity);
        data
    }

    fn dispatch(&mut self, event: u32, uid: u32) {
        let Some(slot) = self.slots.iter().find(|slot| slot.uid == uid) else {
            return;
        };
        let monitor = slot.monitor;
        match event {
            WM_CONTEXTMENU | WM_RBUTTONUP => unsafe { self.show_menu() },
            NIN_SELECT | NIN_KEYSELECT | WM_LBUTTONUP => {
                (self.callbacks.activate)(self.callbacks.user, monitor, Activation::Primary)
            }
            WM_LBUTTONDBLCLK => {
                (self.callbacks.activate)(self.callbacks.user, monitor, Activation::Secondary)
            }
            _ => {}
        }
    }

    /// The right-click menu, with exactly the two documented commands.
    unsafe fn show_menu(&mut self) {
        let menu = CreatePopupMenu();
        if menu.is_null() {
            return;
        }
        let settings = to_wide("Settings");
        let exit = to_wide("Exit");
        AppendMenuW(menu, MF_STRING, MENU_SETTINGS, settings.as_ptr());
        InsertMenuW(menu, MENU_SETTINGS as u32, MF_SEPARATOR, 0, std::ptr::null());
        AppendMenuW(menu, MF_STRING, MENU_EXIT, exit.as_ptr());

        let mut point = POINT { x: 0, y: 0 };
        GetCursorPos(&mut point);
        // Without taking the foreground the menu survives the first click elsewhere.
        SetForegroundWindow(self.window);
        let command = TrackPopupMenu(
            menu,
            TPM_RETURNCMD | TPM_RIGHTBUTTON,
            point.x,
            point.y,
            0,
            self.window,
            std::ptr::null(),
        );
        let _ = DestroyMenu(menu);
        PostMessageW(self.window, 0x0000, 0, 0);
        match command as usize {
            MENU_SETTINGS => (self.callbacks.settings)(self.callbacks.user),
            MENU_EXIT => (self.callbacks.exit)(self.callbacks.user),
            _ => {}
        }
    }

    /// Run the message loop until the window quits.
    pub fn run(&mut self) -> i32 {
        let mut message: windows_sys::Win32::UI::WindowsAndMessaging::MSG =
            unsafe { std::mem::zeroed() };
        loop {
            let result = unsafe { GetMessageW(&mut message, std::ptr::null_mut(), 0, 0) };
            if result == 0 {
                return message.wParam as i32;
            }
            if result == -1 {
                return 1;
            }
            unsafe {
                TranslateMessage(&message);
                DispatchMessageW(&message);
            }
        }
    }

    /// Show the settings window, replacing one that is already open.
    ///
    /// Modeless, so the instance keeps serving while it is open; the window reports every action back
    /// through the callbacks and never writes a setting itself.
    pub fn open_settings(&mut self) {
        if let Some(open) = self.settings.as_ref() {
            if open.is_open() {
                // Bring the existing window forward instead of stacking a second one.
                unsafe {
                    windows_sys::Win32::UI::WindowsAndMessaging::SetForegroundWindow(open.window_id()
                        as windows_sys::Win32::Foundation::HWND)
                };
                return;
            }
        }
        let snapshot = (self.callbacks.settings_snapshot)(self.callbacks.user);
        self.settings = crate::win::settings::Settings::open(
            self.window,
            &snapshot,
            self.callbacks.user,
            self.callbacks.settings_snapshot,
            self.callbacks.settings_apply,
        );
        if self.settings.is_none() {
            crate::log_error!("the settings window could not be created");
        }
    }

    /// Update the open settings window from the current state.
    pub fn refresh_settings(&mut self) {
        let Some(window) = self.settings.as_mut() else {
            return;
        };
        if !window.is_open() {
            self.settings = None;
            return;
        }
        let snapshot = (self.callbacks.settings_snapshot)(self.callbacks.user);
        window.set_left_click(snapshot.left_click);
        window.set_summary(snapshot.monitors, snapshot.profiles);
    }

    /// Show the profile flyout for the monitor at `slot`.
    ///
    /// The popup is anchored to the icon itself through `Shell_NotifyIconGetRect`; when the shell
    /// cannot report a rectangle the flyout falls back to the pointer. A previously open flyout for
    /// another monitor is replaced, so at most one popup exists.
    pub fn open_flyout(&mut self, slot: usize) {
        let Some(entry) = self.slots.iter().find(|entry| entry.monitor == slot) else {
            return;
        };
        let anchor = self.icon_rect(entry);
        let content = (self.callbacks.flyout)(self.callbacks.user, slot);
        self.flyout = None;
        self.flyout = crate::win::flyout::Flyout::open(
            self.window,
            anchor,
            content,
            slot,
            self.callbacks.user,
            self.callbacks.choose,
        );
        if self.flyout.is_none() {
            crate::log_error!("the profile flyout could not be created for slot {slot}");
        }
    }

    /// Report the outcome of a click back into the open flyout, then refresh what the icon claims.
    pub fn flyout_result(&mut self, row: usize, applied: bool) {
        let Some(flyout) = self.flyout.as_mut() else {
            return;
        };
        if !flyout.is_open() {
            self.flyout = None;
            return;
        }
        if applied {
            let items = flyout.item_count();
            for index in 0..items {
                flyout.set_state(
                    index,
                    if index == row {
                        crate::win::flyout::State::Current
                    } else {
                        crate::win::flyout::State::Normal
                    },
                );
            }
        } else {
            let reason = (self.callbacks.failure)(self.callbacks.user)
                .unwrap_or_else(|| String::from("the coordinator refused the profile"));
            flyout.set_state(row, crate::win::flyout::State::Failed(reason));
        }
        let slot = flyout.target();
        self.refresh_icon(slot);
        self.refresh_tooltip(slot);
    }

    /// Redraw the icon for a slot from the profile the caller verifies.
    ///
    /// The glyph only changes after a verified apply, so the icon never advertises a profile the
    /// hardware did not confirm.
    pub fn refresh_icon(&mut self, slot: usize) {
        let Some(index) = self.slots.iter().position(|entry| entry.monitor == slot) else {
            return;
        };
        let name = (self.callbacks.glyph)(self.callbacks.user, slot);
        let glyph = name
            .as_deref()
            .map(Glyph::from_name)
            .unwrap_or(Glyph::Monitor);
        let (badge, size, uid, identity) = {
            let entry = &self.slots[index];
            (entry.badge, entry.size, entry.uid, entry.identity)
        };
        if self.slots[index].glyph == glyph {
            return;
        }
        let Some(icon) = render_icon(glyph, badge, size as u32) else {
            return;
        };
        let mut data: NOTIFYICONDATAW = unsafe { std::mem::zeroed() };
        data.cbSize = std::mem::size_of::<NOTIFYICONDATAW>() as u32;
        data.hWnd = self.window;
        data.uID = uid;
        data.uFlags = NIF_ICON;
        data.hIcon = icon as *mut c_void;
        data.guidItem = guid(identity);
        if unsafe { Shell_NotifyIconW(NIM_MODIFY, &data) } != 0 {
            unsafe { DestroyIcon(self.slots[index].icon as *mut c_void) };
            self.slots[index].icon = icon;
            self.slots[index].glyph = glyph;
        } else {
            unsafe { DestroyIcon(icon as *mut c_void) };
        }
    }

    /// Recompute the tooltip for a slot from the caller's view of the monitor.
    pub fn refresh_tooltip(&mut self, slot: usize) {
        let text = (self.callbacks.tooltip)(self.callbacks.user, slot);
        self.set_tooltip(slot, &text);
    }

    /// The icon's screen rectangle, for anchoring the flyout.
    fn icon_rect(&self, slot: &Slot) -> Option<RECT> {
        let mut identifier: NOTIFYICONIDENTIFIER = unsafe { std::mem::zeroed() };
        identifier.cbSize = std::mem::size_of::<NOTIFYICONIDENTIFIER>() as u32;
        identifier.hWnd = self.window;
        identifier.uID = slot.uid;
        identifier.guidItem = guid(slot.identity);
        let mut rect: RECT = unsafe { std::mem::zeroed() };
        let result = unsafe { Shell_NotifyIconGetRect(&identifier, &mut rect) };
        if result < 0 {
            return None;
        }
        Some(rect)
    }

    /// The owner window, as a number so another thread can post to it.
    ///
    /// Posting a message to a window is safe from any thread; a message loop only ever runs on the
    /// thread that owns it.
    pub fn window_id(&self) -> isize {
        self.window as isize
    }

    /// End the message loop from anywhere, including another thread.
    pub fn quit(&self) {
        unsafe {
            if !self.window.is_null() {
                PostMessageW(self.window, 0x0010, 0, 0);
            }
        }
    }
}

/// Compose one icon and hand it to the shell as an `HICON`.
fn render_icon(glyph: Glyph, badge: Option<u32>, size: u32) -> Option<isize> {
    let bitmap = render(glyph, badge, size);
    unsafe {
        let mut info: BITMAPINFO = std::mem::zeroed();
        info.bmiHeader.biSize = std::mem::size_of::<BITMAPINFOHEADER>() as u32;
        info.bmiHeader.biWidth = bitmap.width as i32;
        // A negative height requests a top-down bitmap, matching the pixel order above.
        info.bmiHeader.biHeight = -(bitmap.height as i32);
        info.bmiHeader.biPlanes = 1;
        info.bmiHeader.biBitCount = 32;
        info.bmiHeader.biCompression = BI_RGB as u32;

        let mut bits: *mut c_void = std::ptr::null_mut();
        let color: HBITMAP = CreateDIBSection(
            std::ptr::null_mut(),
            &info,
            DIB_RGB_COLORS,
            &mut bits,
            std::ptr::null_mut(),
            0,
        );
        if color.is_null() || bits.is_null() {
            return None;
        }
        std::ptr::copy_nonoverlapping(
            bitmap.pixels.as_ptr(),
            bits as *mut u8,
            bitmap.pixels.len(),
        );

        // A 32-bit colour bitmap with alpha needs no mask image beyond a zeroed one.
        let mask = windows_sys::Win32::Graphics::Gdi::CreateBitmap(
            bitmap.width as i32,
            bitmap.height as i32,
            1,
            1,
            std::ptr::null(),
        );
        let mut icon_info: ICONINFO = std::mem::zeroed();
        icon_info.fIcon = 1;
        icon_info.hbmColor = color;
        icon_info.hbmMask = mask;
        let icon = CreateIconIndirect(&icon_info);
        DeleteObject(color as *mut c_void);
        if !mask.is_null() {
            DeleteObject(mask as *mut c_void);
        }
        if icon.is_null() {
            return None;
        }
        Some(icon as isize)
    }
}

/// Size of the version-3 `NOTIFYICONDATAW`: everything up to the GUID-pair tail.
fn version_three_size() -> usize {
    std::mem::size_of::<NOTIFYICONDATAW>()
        - std::mem::size_of::<windows_sys::core::GUID>()
        - std::mem::size_of::<isize>()
}

fn guid(bytes: [u8; 16]) -> windows_sys::core::GUID {
    windows_sys::core::GUID {
        data1: u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]),
        data2: u16::from_le_bytes([bytes[4], bytes[5]]),
        data3: u16::from_le_bytes([bytes[6], bytes[7]]),
        data4: [
            bytes[8], bytes[9], bytes[10], bytes[11], bytes[12], bytes[13], bytes[14], bytes[15],
        ],
    }
}

/// Copy `text` into a fixed NUL-terminated field, truncating at a character boundary.
fn fill(field: &mut [u16], text: &str) {
    let encoded: Vec<u16> = text.encode_utf16().take(field.len() - 1).collect();
    field[..encoded.len()].copy_from_slice(&encoded);
    field[encoded.len()] = 0;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_icon_identity_is_stable_per_monitor_and_differs_between_monitors() {
        let first = icon_identity("mon-227062c49310910f");
        assert_eq!(first, icon_identity("mon-227062c49310910f"));
        assert_ne!(first, icon_identity("mon-other"));
        // A restarted process must derive the same identity, so nothing here may be random.
        assert_eq!(icon_identity("mon-227062c49310910f"), first);
        assert_eq!(first[6] & 0xF0, 0x50);
    }

    #[test]
    fn a_tooltip_carries_index_state_and_brightness() {
        assert_eq!(
            tooltip(2, 3, "MSI MAG275R", "Night", Some(0)),
            "2/3 MSI MAG275R - Night, 0%"
        );
        // With one monitor the index is redundant.
        assert_eq!(tooltip(1, 1, "MSI MAG275R", "Custom", None), "MSI MAG275R - Custom");
        assert!(tooltip(1, 1, "MSI MAG275R", "Custom", None).chars().count() <= 120);
    }

    #[test]
    fn a_long_tooltip_is_truncated_without_splitting_a_character() {
        let name = "М".repeat(200);
        let text = tooltip(1, 1, &name, "Night", None);
        assert!(text.chars().count() <= 120, "{}", text.chars().count());
        assert!(text.ends_with('…'));
    }

    #[test]
    fn an_icon_is_drawn_monochrome_with_content() {
        for glyph in [Glyph::Monitor, Glyph::Day, Glyph::Night, Glyph::Custom, Glyph::Eco] {
            for size in [16u32, 24, 32] {
                let icon = render(glyph, None, size);
                assert_eq!(icon.width, size);
                assert_eq!(icon.pixels.len(), (size * size * 4) as usize);
                assert!(icon.painted() > 10, "{glyph:?} at {size} painted nothing");
                assert!(icon.is_monochrome(), "{glyph:?} at {size} used colour");
            }
        }
    }

    #[test]
    fn a_badge_changes_the_icon_and_supports_two_digits() {
        let plain = render(Glyph::Night, None, 32);
        let badged = render(Glyph::Night, Some(1), 32);
        assert_ne!(plain.pixels, badged.pixels);
        let tenth = render(Glyph::Night, Some(10), 32);
        assert_ne!(badged.pixels, tenth.pixels, "10 must not look like 1");
        // The badge is opaque and touches the bottom-right corner.
        let corner = ((31 * 32 + 31) * 4) as usize;
        assert_eq!(tenth.pixels[corner + 3], 255);
    }

    #[test]
    fn a_glyph_name_falls_back_to_the_neutral_monitor() {
        assert_eq!(Glyph::from_name("night"), Glyph::Night);
        assert_eq!(Glyph::from_name("day"), Glyph::Day);
        assert_eq!(Glyph::from_name("nonsense"), Glyph::Monitor);
    }

    #[test]
    fn the_tooltip_field_is_nul_terminated_and_truncated() {
        let mut field = vec![0xFFFFu16; 8];
        fill(&mut field, "abcdef");
        assert_eq!(&field[..7], &[97, 98, 99, 100, 101, 102, 0]);
        let mut small = vec![0u16; 4];
        fill(&mut small, "abcdefgh");
        assert_eq!(small[3], 0);
    }
}
