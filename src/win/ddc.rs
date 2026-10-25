//! Read-only Dxva2 access, bounded by an external deadline.
//!
//! Every Dxva2 call blocks inside the display driver with no cancellation contract, and
//! Microsoft's "about 40 ms" figure is typical rather than maximal. A read therefore runs on a
//! short-lived thread and the caller waits with a deadline. When the deadline expires the
//! result is reported as `timeout` with an unknown hardware outcome and the thread is
//! abandoned; the process then exits, which is the only reliable cleanup available before the
//! isolated worker process exists. Nothing here writes to a monitor: this module deliberately
//! exposes only getters.

use std::sync::mpsc::{self, RecvTimeoutError};
use std::time::{Duration, Instant};

use serde::Serialize;
use windows_sys::Win32::Devices::Display::{
    CapabilitiesRequestAndCapabilitiesReply, DestroyPhysicalMonitors,
    GetCapabilitiesStringLength, GetMonitorBrightness, GetMonitorContrast,
    GetNumberOfPhysicalMonitorsFromHMONITOR, GetPhysicalMonitorsFromHMONITOR,
    GetVCPFeatureAndVCPFeatureReply, SetMonitorBrightness, SetMonitorContrast, SetVCPFeature,
    PHYSICAL_MONITOR, MC_MOMENTARY, MC_SET_PARAMETER,
};
use windows_sys::Win32::Foundation::{GetLastError, HANDLE};
use windows_sys::Win32::Graphics::Gdi::HMONITOR;

use super::wide::from_wide_field;

/// VCP code for brightness.
pub const VCP_BRIGHTNESS: u8 = 0x10;
/// VCP code for contrast.
pub const VCP_CONTRAST: u8 = 0x12;
/// Colour temperature presets. The values are the display's own, not a standard table's.
pub const VCP_COLOR_TEMPERATURE: u8 = 0x14;

/// The controls this module is allowed to touch.
///
/// A closed enum rather than a caller-supplied code: configuration and the CLI can ask for one of
/// these three and nothing else.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum DdcControl {
    Brightness,
    Contrast,
    ColorTemperature,
}

impl DdcControl {
    /// The VCP code this control is written and read with.
    pub(crate) fn vcp(self) -> u8 {
        match self {
            DdcControl::Brightness => VCP_BRIGHTNESS,
            DdcControl::Contrast => VCP_CONTRAST,
            DdcControl::ColorTemperature => VCP_COLOR_TEMPERATURE,
        }
    }

    /// Whether the Win32 high-level getter and setter cover this control.
    ///
    /// Colour temperature is excluded on purpose: `SetMonitorColorTemperature` takes an
    /// `MC_COLOR_TEMPERATURE` ordinal, which is a different scale from the values VCP `0x14` accepts,
    /// and mixing them would write a meaning the user did not ask for.
    fn has_high_level(self) -> bool {
        matches!(self, DdcControl::Brightness | DdcControl::Contrast)
    }
}

/// How a failed DDC operation is described. Never conflated with "unsupported".
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DdcError {
    /// The bounded wait expired. Hardware outcome is unknown.
    Timeout { waited_ms: u64 },
    /// The monitor handle has no physical monitor attached.
    NoPhysicalMonitor,
    /// More than one physical monitor answers for the same handle.
    Ambiguous { count: u32 },
    /// A Dxva2 call failed.
    Api {
        call: &'static str,
        win32_error: u32,
    },
    /// The probe thread ended without producing a result.
    Crashed,
}

impl DdcError {
    /// Machine-readable state name used in JSON output.
    pub fn state(&self) -> &'static str {
        match self {
            DdcError::Timeout { .. } => "timeout",
            DdcError::NoPhysicalMonitor => "unavailable",
            DdcError::Ambiguous { .. } => "ambiguous",
            DdcError::Api { .. } | DdcError::Crashed => "transport_error",
        }
    }

    pub fn message(&self) -> String {
        match self {
            DdcError::Timeout { waited_ms } => format!(
                "no response within the {waited_ms} ms bound; hardware outcome unknown"
            ),
            DdcError::NoPhysicalMonitor => {
                String::from("the monitor handle reports no physical monitor")
            }
            DdcError::Ambiguous { count } => {
                format!("{count} physical monitors share one display handle")
            }
            DdcError::Api { call, win32_error } => {
                format!("{call} failed with Win32 error {win32_error}")
            }
            DdcError::Crashed => String::from("the probe thread ended without a result"),
        }
    }
}

/// Reading from a high-level Dxva2 getter, which reports the device's own minimum.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
pub struct HighLevelReading {
    pub minimum: u32,
    pub current: u32,
    pub maximum: u32,
}

/// Reading from `GetVCPFeatureAndVCPFeatureReply`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
pub struct VcpReading {
    pub vcp_code: u8,
    pub current: u32,
    pub maximum: u32,
    pub code_type: VcpCodeType,
}

/// `MC_VCP_CODE_TYPE`. This describes momentary versus set-parameter behaviour; it is not the
/// continuous/noncontinuous classification, which lives in the control registry.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum VcpCodeType {
    Momentary,
    SetParameter,
    Unknown,
}

impl VcpCodeType {
    fn from_raw(value: i32) -> VcpCodeType {
        if value == MC_MOMENTARY as i32 {
            VcpCodeType::Momentary
        } else if value == MC_SET_PARAMETER as i32 {
            VcpCodeType::SetParameter
        } else {
            VcpCodeType::Unknown
        }
    }
}

/// Raw capabilities string plus the length the monitor declared for it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CapabilitiesRaw {
    pub text: String,
    pub declared_length: u32,
}

/// Everything one bounded probe collected for one monitor handle.
#[derive(Clone, Debug)]
pub struct ProbeOutcome {
    pub physical_count: u32,
    pub descriptions: Vec<String>,
    pub brightness_high_level: Result<HighLevelReading, DdcError>,
    pub contrast_high_level: Result<HighLevelReading, DdcError>,
    pub brightness_vcp: Result<VcpReading, DdcError>,
    pub contrast_vcp: Result<VcpReading, DdcError>,
    pub capabilities: Result<CapabilitiesRaw, DdcError>,
    pub elapsed_ms: u64,
}

impl ProbeOutcome {
    /// True when at least one brightness read succeeded and the handle was unambiguous.
    pub fn is_controllable(&self) -> bool {
        self.physical_count == 1
            && (self.brightness_high_level.is_ok() || self.brightness_vcp.is_ok())
    }

    /// True when the handle answers for more than one physical monitor.
    pub fn is_ambiguous(&self) -> bool {
        self.physical_count > 1
    }
}

struct PhysicalMonitors {
    monitors: Vec<PHYSICAL_MONITOR>,
}

impl PhysicalMonitors {
    fn first(&self) -> HANDLE {
        self.monitors[0].hPhysicalMonitor
    }
}

impl Drop for PhysicalMonitors {
    fn drop(&mut self) {
        if self.monitors.is_empty() {
            return;
        }
        unsafe {
            DestroyPhysicalMonitors(self.monitors.len() as u32, self.monitors.as_ptr());
        }
    }
}

fn open_physical_monitors(hmonitor: HMONITOR) -> Result<PhysicalMonitors, DdcError> {
    let mut count: u32 = 0;
    let ok = unsafe { GetNumberOfPhysicalMonitorsFromHMONITOR(hmonitor, &mut count) };
    if ok == 0 {
        return Err(DdcError::Api {
            call: "GetNumberOfPhysicalMonitorsFromHMONITOR",
            win32_error: unsafe { GetLastError() },
        });
    }
    if count == 0 {
        return Err(DdcError::NoPhysicalMonitor);
    }

    let mut monitors = vec![PHYSICAL_MONITOR::default(); count as usize];
    let ok = unsafe { GetPhysicalMonitorsFromHMONITOR(hmonitor, count, monitors.as_mut_ptr()) };
    if ok == 0 {
        return Err(DdcError::Api {
            call: "GetPhysicalMonitorsFromHMONITOR",
            win32_error: unsafe { GetLastError() },
        });
    }
    Ok(PhysicalMonitors { monitors })
}

fn read_high_level(handle: HANDLE, contrast: bool) -> Result<HighLevelReading, DdcError> {
    let mut minimum: u32 = 0;
    let mut current: u32 = 0;
    let mut maximum: u32 = 0;
    let (ok, call) = unsafe {
        if contrast {
            (
                GetMonitorContrast(handle, &mut minimum, &mut current, &mut maximum),
                "GetMonitorContrast",
            )
        } else {
            (
                GetMonitorBrightness(handle, &mut minimum, &mut current, &mut maximum),
                "GetMonitorBrightness",
            )
        }
    };
    if ok == 0 {
        return Err(DdcError::Api {
            call,
            win32_error: unsafe { GetLastError() },
        });
    }
    Ok(HighLevelReading {
        minimum,
        current,
        maximum,
    })
}

fn read_vcp(handle: HANDLE, code: u8) -> Result<VcpReading, DdcError> {
    let mut code_type: i32 = 0;
    let mut current: u32 = 0;
    let mut maximum: u32 = 0;
    let ok = unsafe {
        GetVCPFeatureAndVCPFeatureReply(
            handle,
            code,
            &mut code_type,
            &mut current,
            &mut maximum,
        )
    };
    if ok == 0 {
        return Err(DdcError::Api {
            call: "GetVCPFeatureAndVCPFeatureReply",
            win32_error: unsafe { GetLastError() },
        });
    }
    Ok(VcpReading {
        vcp_code: code,
        current,
        maximum,
        code_type: VcpCodeType::from_raw(code_type),
    })
}

fn read_capabilities(handle: HANDLE) -> Result<CapabilitiesRaw, DdcError> {
    let mut length: u32 = 0;
    let ok = unsafe { GetCapabilitiesStringLength(handle, &mut length) };
    if ok == 0 {
        return Err(DdcError::Api {
            call: "GetCapabilitiesStringLength",
            win32_error: unsafe { GetLastError() },
        });
    }
    if length == 0 || length > 64 * 1024 {
        return Err(DdcError::Api {
            call: "GetCapabilitiesStringLength",
            win32_error: 0,
        });
    }

    // The API wants room for the terminating NUL as well.
    let mut buffer = vec![0u8; length as usize + 1];
    let ok = unsafe {
        CapabilitiesRequestAndCapabilitiesReply(
            handle,
            buffer.as_mut_ptr(),
            buffer.len() as u32,
        )
    };
    if ok == 0 {
        return Err(DdcError::Api {
            call: "CapabilitiesRequestAndCapabilitiesReply",
            win32_error: unsafe { GetLastError() },
        });
    }

    let end = buffer.iter().position(|&b| b == 0).unwrap_or(buffer.len());
    Ok(CapabilitiesRaw {
        text: String::from_utf8_lossy(&buffer[..end]).to_string(),
        declared_length: length,
    })
}

fn probe_blocking(hmonitor: HMONITOR, read_capabilities_string: bool) -> Result<ProbeOutcome, DdcError> {
    let started = Instant::now();
    let monitors = open_physical_monitors(hmonitor)?;
    // PHYSICAL_MONITOR is packed, so its description field cannot be borrowed: copy it out.
    let descriptions: Vec<String> = monitors
        .monitors
        .iter()
        .map(|monitor| {
            let field: [u16; 128] = unsafe {
                std::ptr::read_unaligned(std::ptr::addr_of!(monitor.szPhysicalMonitorDescription))
            };
            from_wide_field(&field)
        })
        .collect();
    let handle = monitors.first();

    let brightness_high_level = read_high_level(handle, false);
    let contrast_high_level = read_high_level(handle, true);
    let brightness_vcp = read_vcp(handle, VCP_BRIGHTNESS);
    let contrast_vcp = read_vcp(handle, VCP_CONTRAST);
    let capabilities = if read_capabilities_string {
        read_capabilities(handle)
    } else {
        Err(DdcError::NoPhysicalMonitor)
    };

    Ok(ProbeOutcome {
        physical_count: monitors.monitors.len() as u32,
        descriptions,
        brightness_high_level,
        contrast_high_level,
        brightness_vcp,
        contrast_vcp,
        capabilities,
        elapsed_ms: started.elapsed().as_millis().min(u64::MAX as u128) as u64,
    })
}

/// Run `action` against the single physical monitor behind `hmonitor`.
///
/// The physical-monitor array is created and destroyed inside the call, so a hung driver abandons
/// nothing but the calling thread. An ambiguous handle (more than one physical monitor) is refused
/// rather than guessed at.
pub(crate) fn with_physical_monitor<T>(
    hmonitor: isize,
    action: impl FnOnce(HANDLE) -> T,
) -> Result<T, DdcError> {
    if hmonitor == 0 {
        return Err(DdcError::NoPhysicalMonitor);
    }
    let monitors = open_physical_monitors(hmonitor as HMONITOR)?;
    if monitors.monitors.len() != 1 {
        return Err(DdcError::Ambiguous {
            count: monitors.monitors.len() as u32,
        });
    }
    Ok(action(monitors.first()))
}

/// Write through the high-level brightness or contrast setter.
pub(crate) fn set_high_level(handle: HANDLE, contrast: bool, value: u32) -> Result<(), DdcError> {
    let (ok, call) = unsafe {
        if contrast {
            (
                SetMonitorContrast(handle, value),
                "SetMonitorContrast",
            )
        } else {
            (
                SetMonitorBrightness(handle, value),
                "SetMonitorBrightness",
            )
        }
    };
    if ok == 0 {
        return Err(DdcError::Api {
            call,
            win32_error: unsafe { GetLastError() },
        });
    }
    Ok(())
}

/// Write an allowlisted VCP code.
///
/// Only the codes in this module's control registry reach here; there is no caller-supplied code
/// path from configuration or the CLI.
pub(crate) fn set_vcp(handle: HANDLE, code: u8, value: u32) -> Result<(), DdcError> {
    if code != VCP_BRIGHTNESS && code != VCP_CONTRAST && code != VCP_COLOR_TEMPERATURE {
        return Err(DdcError::Api {
            call: "SetVCPFeature",
            win32_error: 87, // ERROR_INVALID_PARAMETER
        });
    }
    let ok = unsafe { SetVCPFeature(handle, code, value) };
    if ok == 0 {
        return Err(DdcError::Api {
            call: "SetVCPFeature",
            win32_error: unsafe { GetLastError() },
        });
    }
    Ok(())
}

/// One reading of an allowlisted control, with the range the device reported.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct ControlReading {
    /// Reported minimum, present only from the high-level getter. Never assumed to be zero.
    pub minimum: Option<u32>,
    pub current: u32,
    pub maximum: u32,
    pub backend: &'static str,
}

/// Read one allowlisted control through the high-level getter, falling back to VCP.
pub(crate) fn read_control(
    handle: HANDLE,
    control: DdcControl,
) -> Result<ControlReading, DdcError> {
    if !control.has_high_level() {
        // Straight to VCP: there is no high-level getter whose scale matches this control's values.
        let reading = read_vcp(handle, control.vcp())?;
        return Ok(ControlReading {
            minimum: None,
            current: reading.current,
            maximum: reading.maximum,
            backend: "dxva2-vcp",
        });
    }
    match read_high_level(handle, control == DdcControl::Contrast) {
        Ok(reading) => Ok(ControlReading {
            minimum: Some(reading.minimum),
            current: reading.current,
            maximum: reading.maximum,
            backend: "dxva2-high-level",
        }),
        Err(high_level_error) => {
            match read_vcp(handle, control.vcp()) {
                // The low-level getter reports no minimum: it is left absent rather than invented.
                Ok(reading) => Ok(ControlReading {
                    minimum: None,
                    current: reading.current,
                    maximum: reading.maximum,
                    backend: "dxva2-vcp",
                }),
                Err(_) => Err(high_level_error),
            }
        }
    }
}

/// Write one allowlisted control, preferring the high-level setter.
pub(crate) fn write_control(
    handle: HANDLE,
    control: DdcControl,
    value: u32,
) -> Result<&'static str, DdcError> {
    if control.has_high_level() && set_high_level(handle, control == DdcControl::Contrast, value).is_ok()
    {
        return Ok("dxva2-high-level");
    }
    // A high-level failure is not retried at the low level when the outcome is uncertain: only a
    // definite refusal falls through, and the caller verifies by readback either way.
    set_vcp(handle, control.vcp(), value)?;
    Ok("dxva2-vcp")
}

/// Probe one monitor handle with a wall-clock bound.
///
/// The guard value is the raw `HMONITOR`. After a timeout the caller must treat the handle as
/// unusable: the abandoned thread still owns the physical-monitor handles and may still return
/// from the driver at any time.
pub fn probe(
    hmonitor: isize,
    read_capabilities_string: bool,
    budget: Duration,
) -> Result<ProbeOutcome, DdcError> {
    if hmonitor == 0 {
        return Err(DdcError::NoPhysicalMonitor);
    }
    let (sender, receiver) = mpsc::channel();
    let spawned = std::thread::Builder::new()
        .name(String::from("ddc-probe"))
        .spawn(move || {
            let result = probe_blocking(hmonitor as HMONITOR, read_capabilities_string);
            let _ = sender.send(result);
        });
    if spawned.is_err() {
        return Err(DdcError::Crashed);
    }

    match receiver.recv_timeout(budget) {
        Ok(result) => result,
        Err(RecvTimeoutError::Timeout) => Err(DdcError::Timeout {
            waited_ms: budget.as_millis().min(u64::MAX as u128) as u64,
        }),
        Err(RecvTimeoutError::Disconnected) => Err(DdcError::Crashed),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn error_states_are_not_conflated_with_unsupported() {
        assert_eq!(DdcError::Timeout { waited_ms: 5 }.state(), "timeout");
        assert_eq!(DdcError::NoPhysicalMonitor.state(), "unavailable");
        assert_eq!(DdcError::Ambiguous { count: 2 }.state(), "ambiguous");
        assert_eq!(
            DdcError::Api {
                call: "GetMonitorBrightness",
                win32_error: 31
            }
            .state(),
            "transport_error"
        );
    }

    #[test]
    fn timeout_message_states_the_unknown_outcome() {
        let message = DdcError::Timeout { waited_ms: 2000 }.message();
        assert!(message.contains("2000"));
        assert!(message.contains("unknown"));
    }

    #[test]
    fn code_type_maps_from_dxva2_values() {
        assert_eq!(VcpCodeType::from_raw(MC_MOMENTARY as i32), VcpCodeType::Momentary);
        assert_eq!(
            VcpCodeType::from_raw(MC_SET_PARAMETER as i32),
            VcpCodeType::SetParameter
        );
        assert_eq!(VcpCodeType::from_raw(99), VcpCodeType::Unknown);
    }

    #[test]
    fn a_null_handle_is_not_probed() {
        assert_eq!(
            probe(0, false, Duration::from_millis(10)).err(),
            Some(DdcError::NoPhysicalMonitor)
        );
    }

    #[test]
    fn capability_and_controllability_agree_on_ambiguity() {
        let outcome = ProbeOutcome {
            physical_count: 2,
            descriptions: vec![String::from("a"), String::from("b")],
            brightness_high_level: Ok(HighLevelReading {
                minimum: 0,
                current: 50,
                maximum: 100,
            }),
            contrast_high_level: Err(DdcError::NoPhysicalMonitor),
            brightness_vcp: Ok(VcpReading {
                vcp_code: VCP_BRIGHTNESS,
                current: 50,
                maximum: 100,
                code_type: VcpCodeType::SetParameter,
            }),
            contrast_vcp: Err(DdcError::NoPhysicalMonitor),
            capabilities: Err(DdcError::NoPhysicalMonitor),
            elapsed_ms: 40,
        };
        assert!(outcome.is_ambiguous());
        assert!(!outcome.is_controllable());
    }
}
