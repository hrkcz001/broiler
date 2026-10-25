//! Display topology discovery.
//!
//! `QueryDisplayConfig` supplies the active paths: one source (a GDI display device such as
//! `\\.\DISPLAY1`) and one target (the connector, with its EDID ids and monitor device path).
//! `EnumDisplayMonitors` supplies the `HMONITOR` values that the Dxva2 physical-monitor API
//! needs. The two are joined by the documented GDI device name, never by enumeration order.
//!
//! Nothing in this module writes to a monitor.

use serde::Serialize;
use windows_sys::Win32::Devices::Display::{
    DisplayConfigGetDeviceInfo, GetDisplayConfigBufferSizes, QueryDisplayConfig,
    DISPLAYCONFIG_DEVICE_INFO_GET_SOURCE_NAME, DISPLAYCONFIG_DEVICE_INFO_GET_TARGET_NAME,
    DISPLAYCONFIG_MODE_INFO, DISPLAYCONFIG_PATH_INFO, DISPLAYCONFIG_SOURCE_DEVICE_NAME,
    DISPLAYCONFIG_TARGET_DEVICE_NAME, DISPLAYCONFIG_VIDEO_OUTPUT_TECHNOLOGY, QDC_ALL_PATHS,
    QDC_ONLY_ACTIVE_PATHS,
};
use windows_sys::Win32::Foundation::{ERROR_SUCCESS, LPARAM, RECT, TRUE};
use windows_sys::Win32::Graphics::Gdi::{
    EnumDisplayMonitors, GetMonitorInfoW, HDC, HMONITOR, MONITORINFOEXW,
};
use windows_sys::Win32::UI::WindowsAndMessaging::MONITORINFOF_PRIMARY;

/// `DISPLAYCONFIG_TARGET_DEVICE_NAME_FLAGS::friendlyNameFromEdid`, bit 0 of the flag word.
///
/// `windows-sys` does not export the named constant because the C header exposes the flag
/// through an anonymous bitfield, so the documented bit value is used here.
const TARGET_FLAG_FRIENDLY_NAME_FROM_EDID: u32 = 0x0000_0001;

use super::wide::from_wide_field;

/// One active display target joined to its monitor handle.
#[derive(Clone, Debug, Serialize)]
pub struct DisplayTarget {
    /// GDI device name of the source, for example `\\.\DISPLAY1`.
    pub device_name: String,
    /// Friendly name reported by the target; from EDID when Windows says so.
    pub friendly_name: String,
    pub friendly_name_from_edid: bool,
    /// Monitor interface path, for example `\\?\DISPLAY#MSI3CA7#5&...#{guid}`.
    pub monitor_device_path: String,
    pub edid_manufacture_id: u16,
    pub edid_product_code: u16,
    pub output_technology: i32,
    pub output_technology_name: String,
    pub connector_instance: u32,
    /// Raw `HMONITOR` value. Routing data only; never persistent identity.
    pub hmonitor: isize,
    pub position: Rect,
    pub primary: bool,
    /// How many paths in this query share this source, which is one for an extended desktop
    /// and more than one for a cloned output.
    pub source_path_count: usize,
    /// Whether the target reported itself as available (`targetAvailable`). Paths returned by
    /// an all-paths query can include a connector whose monitor is not present.
    pub available: bool,
}

#[derive(Clone, Copy, Debug, Serialize)]
pub struct Rect {
    pub x: i32,
    pub y: i32,
    pub width: i32,
    pub height: i32,
}

/// Why discovery could not produce a topology.
#[derive(Clone, Debug)]
pub enum TopologyError {
    /// `GetDisplayConfigBufferSizes` or `QueryDisplayConfig` failed.
    Unavailable { win32_error: u32 },
    /// A monitor could not be joined to an active path.
    NoActivePaths,
}

impl TopologyError {
    pub fn describe(&self) -> String {
        match self {
            TopologyError::Unavailable { win32_error } => {
                format!("display topology query failed with Win32 error {win32_error}")
            }
            TopologyError::NoActivePaths => String::from("no active display paths are reported"),
        }
    }
}

/// Output technology name used in diagnostics.
pub fn output_technology_name(value: DISPLAYCONFIG_VIDEO_OUTPUT_TECHNOLOGY) -> String {
    use windows_sys::Win32::Devices::Display::*;
    let name = match value {
        DISPLAYCONFIG_OUTPUT_TECHNOLOGY_OTHER => "other",
        DISPLAYCONFIG_OUTPUT_TECHNOLOGY_HD15 => "vga",
        DISPLAYCONFIG_OUTPUT_TECHNOLOGY_SVIDEO => "s-video",
        DISPLAYCONFIG_OUTPUT_TECHNOLOGY_COMPOSITE_VIDEO => "composite",
        DISPLAYCONFIG_OUTPUT_TECHNOLOGY_COMPONENT_VIDEO => "component",
        DISPLAYCONFIG_OUTPUT_TECHNOLOGY_DVI => "dvi",
        DISPLAYCONFIG_OUTPUT_TECHNOLOGY_HDMI => "hdmi",
        DISPLAYCONFIG_OUTPUT_TECHNOLOGY_LVDS => "lvds",
        DISPLAYCONFIG_OUTPUT_TECHNOLOGY_D_JPN => "d-jpn",
        DISPLAYCONFIG_OUTPUT_TECHNOLOGY_SDI => "sdi",
        DISPLAYCONFIG_OUTPUT_TECHNOLOGY_DISPLAYPORT_EXTERNAL => "displayport",
        DISPLAYCONFIG_OUTPUT_TECHNOLOGY_DISPLAYPORT_EMBEDDED => "displayport-embedded",
        DISPLAYCONFIG_OUTPUT_TECHNOLOGY_UDI_EXTERNAL => "udi-external",
        DISPLAYCONFIG_OUTPUT_TECHNOLOGY_UDI_EMBEDDED => "udi-embedded",
        DISPLAYCONFIG_OUTPUT_TECHNOLOGY_SDTVDONGLE => "sdtv-dongle",
        DISPLAYCONFIG_OUTPUT_TECHNOLOGY_MIRACAST => "miracast",
        DISPLAYCONFIG_OUTPUT_TECHNOLOGY_INDIRECT_WIRED => "indirect-wired",
        DISPLAYCONFIG_OUTPUT_TECHNOLOGY_INDIRECT_VIRTUAL => "indirect-virtual",
        DISPLAYCONFIG_OUTPUT_TECHNOLOGY_DISPLAYPORT_USB_TUNNEL => "displayport-usb-tunnel",
        DISPLAYCONFIG_OUTPUT_TECHNOLOGY_INTERNAL => "internal",
        other => return format!("unknown-{}", other as u32),
    };
    String::from(name)
}

/// True when the output technology identifies a built-in panel.
///
/// Windows reports built-in laptop and all-in-one panels as `INTERNAL`, and some drivers use
/// `LVDS`, embedded DisplayPort or embedded UDI instead. This is a routing heuristic supported
/// by the connector type Windows itself reports; it does not probe the panel.
pub fn is_builtin_output(value: DISPLAYCONFIG_VIDEO_OUTPUT_TECHNOLOGY) -> bool {
    use windows_sys::Win32::Devices::Display::*;
    matches!(
        value,
        DISPLAYCONFIG_OUTPUT_TECHNOLOGY_INTERNAL
            | DISPLAYCONFIG_OUTPUT_TECHNOLOGY_LVDS
            | DISPLAYCONFIG_OUTPUT_TECHNOLOGY_DISPLAYPORT_EMBEDDED
            | DISPLAYCONFIG_OUTPUT_TECHNOLOGY_UDI_EMBEDDED
    )
}

fn source_device_name(path: &DISPLAYCONFIG_PATH_INFO) -> Option<String> {
    let mut name: DISPLAYCONFIG_SOURCE_DEVICE_NAME = unsafe { std::mem::zeroed() };
    name.header.r#type = DISPLAYCONFIG_DEVICE_INFO_GET_SOURCE_NAME;
    name.header.size = std::mem::size_of::<DISPLAYCONFIG_SOURCE_DEVICE_NAME>() as u32;
    name.header.adapterId = path.sourceInfo.adapterId;
    name.header.id = path.sourceInfo.id;
    let status = unsafe { DisplayConfigGetDeviceInfo(&mut name.header) };
    if status != ERROR_SUCCESS as i32 {
        return None;
    }
    let text = from_wide_field(&name.viewGdiDeviceName);
    (!text.is_empty()).then_some(text)
}

fn target_info(path: &DISPLAYCONFIG_PATH_INFO) -> Option<DISPLAYCONFIG_TARGET_DEVICE_NAME> {
    let mut target: DISPLAYCONFIG_TARGET_DEVICE_NAME = unsafe { std::mem::zeroed() };
    target.header.r#type = DISPLAYCONFIG_DEVICE_INFO_GET_TARGET_NAME;
    target.header.size = std::mem::size_of::<DISPLAYCONFIG_TARGET_DEVICE_NAME>() as u32;
    target.header.adapterId = path.targetInfo.adapterId;
    target.header.id = path.targetInfo.id;
    let status = unsafe { DisplayConfigGetDeviceInfo(&mut target.header) };
    if status != ERROR_SUCCESS as i32 {
        return None;
    }
    Some(target)
}

struct MonitorEnum {
    entries: Vec<(isize, String, Rect, bool)>,
}

unsafe extern "system" fn enum_monitor(
    hmonitor: HMONITOR,
    _hdc: HDC,
    _rect: *mut RECT,
    data: LPARAM,
) -> windows_sys::core::BOOL {
    let collected = &mut *(data as *mut MonitorEnum);
    let mut info: MONITORINFOEXW = std::mem::zeroed();
    info.monitorInfo.cbSize = std::mem::size_of::<MONITORINFOEXW>() as u32;
    if GetMonitorInfoW(hmonitor, &mut info.monitorInfo) != 0 {
        let device = from_wide_field(&info.szDevice);
        let rect = info.monitorInfo.rcMonitor;
        collected.entries.push((
            hmonitor as isize,
            device,
            Rect {
                x: rect.left,
                y: rect.top,
                width: rect.right - rect.left,
                height: rect.bottom - rect.top,
            },
            info.monitorInfo.dwFlags & MONITORINFOF_PRIMARY != 0,
        ));
    }
    TRUE
}

fn enum_monitors() -> Vec<(isize, String, Rect, bool)> {
    let mut collected = MonitorEnum { entries: Vec::new() };
    unsafe {
        EnumDisplayMonitors(
            std::ptr::null_mut(),
            std::ptr::null(),
            Some(enum_monitor),
            &mut collected as *mut MonitorEnum as LPARAM,
        );
    }
    collected.entries
}

/// Enumerate active display targets, each joined to its `HMONITOR` by GDI device name.
pub fn enumerate() -> Result<Vec<DisplayTarget>, TopologyError> {
    query(QDC_ONLY_ACTIVE_PATHS)
}

/// Enumerate every known path, including connectors whose monitor is not present.
///
/// Used only to report built-in panels that exist on the machine but are not part of the
/// active desktop. Nothing here is ever offered for control.
pub fn enumerate_all() -> Result<Vec<DisplayTarget>, TopologyError> {
    query(QDC_ALL_PATHS)
}

fn query(flags: windows_sys::Win32::Devices::Display::QUERY_DISPLAY_CONFIG_FLAGS) -> Result<Vec<DisplayTarget>, TopologyError> {
    let mut path_count: u32 = 0;
    let mut mode_count: u32 = 0;
    let status = unsafe { GetDisplayConfigBufferSizes(flags, &mut path_count, &mut mode_count) };
    if status != ERROR_SUCCESS {
        return Err(TopologyError::Unavailable {
            win32_error: status,
        });
    }
    if path_count == 0 {
        return Err(TopologyError::NoActivePaths);
    }

    let mut paths = vec![DISPLAYCONFIG_PATH_INFO::default(); path_count as usize];
    let mut modes = vec![DISPLAYCONFIG_MODE_INFO::default(); mode_count as usize];
    let status = unsafe {
        QueryDisplayConfig(
            flags,
            &mut path_count,
            paths.as_mut_ptr(),
            &mut mode_count,
            modes.as_mut_ptr(),
            std::ptr::null_mut(),
        )
    };
    if status != ERROR_SUCCESS {
        return Err(TopologyError::Unavailable {
            win32_error: status,
        });
    }
    paths.truncate(path_count as usize);

    let monitors = enum_monitors();
    let mut targets: Vec<DisplayTarget> = Vec::new();

    for path in &paths {
        let Some(device_name) = source_device_name(path) else {
            continue;
        };
        let Some(target) = target_info(path) else {
            continue;
        };

        let source_path_count = paths
            .iter()
            .filter(|other| source_device_name(other).as_deref() == Some(device_name.as_str()))
            .count();

        let joined = monitors
            .iter()
            .find(|(_, device, _, _)| device.eq_ignore_ascii_case(&device_name));

        let (hmonitor, position, primary) = match joined {
            Some((hmonitor, _, rect, primary)) => (*hmonitor, *rect, *primary),
            None => (0, Rect { x: 0, y: 0, width: 0, height: 0 }, false),
        };

        let flags = unsafe { target.flags.Anonymous.value };
        let output_technology = target.outputTechnology;
        let available = path.targetInfo.targetAvailable != 0;
        targets.push(DisplayTarget {
            device_name,
            friendly_name: from_wide_field(&target.monitorFriendlyDeviceName),
            friendly_name_from_edid: flags & TARGET_FLAG_FRIENDLY_NAME_FROM_EDID != 0,
            monitor_device_path: from_wide_field(&target.monitorDevicePath),
            edid_manufacture_id: target.edidManufactureId,
            edid_product_code: target.edidProductCodeId,
            output_technology,
            output_technology_name: output_technology_name(output_technology),
            connector_instance: target.connectorInstance,
            hmonitor,
            position,
            primary,
            source_path_count,
            available,
        });
    }

    if targets.is_empty() {
        return Err(TopologyError::NoActivePaths);
    }
    Ok(targets)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builtin_technologies_are_recognised() {
        use windows_sys::Win32::Devices::Display::*;
        assert!(is_builtin_output(DISPLAYCONFIG_OUTPUT_TECHNOLOGY_INTERNAL));
        assert!(is_builtin_output(DISPLAYCONFIG_OUTPUT_TECHNOLOGY_LVDS));
        assert!(is_builtin_output(
            DISPLAYCONFIG_OUTPUT_TECHNOLOGY_DISPLAYPORT_EMBEDDED
        ));
        assert!(!is_builtin_output(DISPLAYCONFIG_OUTPUT_TECHNOLOGY_HDMI));
        assert!(!is_builtin_output(
            DISPLAYCONFIG_OUTPUT_TECHNOLOGY_DISPLAYPORT_EXTERNAL
        ));
    }

    #[test]
    fn names_are_stable_strings() {
        use windows_sys::Win32::Devices::Display::*;
        assert_eq!(
            output_technology_name(DISPLAYCONFIG_OUTPUT_TECHNOLOGY_HDMI),
            "hdmi"
        );
        assert_eq!(
            output_technology_name(DISPLAYCONFIG_OUTPUT_TECHNOLOGY_DISPLAYPORT_EXTERNAL),
            "displayport"
        );
        assert_eq!(
            output_technology_name(DISPLAYCONFIG_OUTPUT_TECHNOLOGY_INTERNAL),
            "internal"
        );
        assert!(output_technology_name(1234).starts_with("unknown-"));
    }
}
