//! Domain records for monitor discovery.
//!
//! Two rules shape these types:
//!
//! * Identity is separate from routing. A stable identifier is derived from EDID evidence and
//!   hashes into an opaque string; `HMONITOR`, the GDI device name and the display index are
//!   routing data that may change between sessions.
//! * Evidence is never compressed into a single boolean. "Advertised in the capabilities
//!   string", "a read succeeded", "a write was observed" and "the user confirmed the effect"
//!   are different claims with different proof.

use serde::Serialize;
use sha2::{Digest, Sha256};

use crate::caps::Capabilities;
use crate::win::ddc::{DdcError, HighLevelReading, VcpCodeType, VcpReading};
use crate::win::topology::Rect;

/// Namespace mixed into every derived identifier, so a future identity scheme cannot collide
/// with the current one.
pub const ID_NAMESPACE: &str = "broiler.monitor.v1";

/// Length of the hex body in an identifier.
const ID_HEX_CHARS: usize = 16;

/// Build the opaque identifier for a monitor.
///
/// The input is the identity evidence: manufacturer, product code and whatever serial evidence
/// exists. Callers that fall back to a connection binding must pass the device path instead of
/// a serial, which is why the caller also records [`IdentityConfidence`].
pub fn monitor_id(manufacturer: Option<&str>, product_code: Option<u16>, identity_evidence: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(ID_NAMESPACE.as_bytes());
    hasher.update(b"|");
    hasher.update(manufacturer.unwrap_or("").as_bytes());
    hasher.update(b"|");
    hasher.update(
        product_code
            .map(|code| format!("{code:04x}"))
            .unwrap_or_default()
            .as_bytes(),
    );
    hasher.update(b"|");
    hasher.update(identity_evidence.as_bytes());
    let digest = hasher.finalize();
    let hex: String = digest.iter().take(ID_HEX_CHARS / 2).map(|b| format!("{b:02x}")).collect();
    format!("mon-{hex}")
}

/// How strong the identity evidence is.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum IdentityConfidence {
    /// Manufacturer, product and a serial from the EDID.
    EdidSerial,
    /// No usable serial; the identifier is bound to this connection.
    ConnectionBound,
    /// Another active monitor produced the same evidence, so the two cannot be told apart.
    Ambiguous,
}

/// One observed control value or failure.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum Reading {
    /// A read returned a value.
    Readable {
        backend: String,
        vcp_code: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        minimum: Option<u32>,
        current: u32,
        #[serde(skip_serializing_if = "Option::is_none")]
        maximum: Option<u32>,
        #[serde(skip_serializing_if = "Option::is_none")]
        code_type: Option<VcpCodeType>,
    },
    /// No response within the bound; the hardware outcome is unknown.
    Timeout { waited_ms: u64, message: String },
    /// The transport failed. Distinct from unsupported.
    TransportError {
        #[serde(skip_serializing_if = "Option::is_none")]
        call: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        win32_error: Option<u32>,
        message: String,
    },
    /// The handle reports no DDC-capable physical monitor.
    Unavailable { message: String },
    /// More than one physical monitor answers for one handle.
    Ambiguous { count: u32, message: String },
    /// The control was not read in this operation.
    NotRequested,
}

impl Reading {
    /// High-level Dxva2 getter result.
    pub fn from_high_level(result: &Result<HighLevelReading, DdcError>, vcp_code: u8) -> Reading {
        match result {
            Ok(reading) => Reading::Readable {
                backend: String::from("dxva2-high-level"),
                vcp_code: format!("0x{vcp_code:02X}"),
                minimum: Some(reading.minimum),
                current: reading.current,
                maximum: Some(reading.maximum),
                code_type: None,
            },
            Err(error) => Reading::from_error(error),
        }
    }

    /// Low-level VCP getter result.
    pub fn from_vcp(result: &Result<VcpReading, DdcError>) -> Reading {
        match result {
            Ok(reading) => Reading::Readable {
                backend: String::from("dxva2-vcp"),
                vcp_code: format!("0x{:02X}", reading.vcp_code),
                minimum: None,
                current: reading.current,
                maximum: Some(reading.maximum),
                code_type: Some(reading.code_type),
            },
            Err(error) => Reading::from_error(error),
        }
    }

    /// Failure record for a DDC error.
    pub fn from_error(error: &DdcError) -> Reading {
        match error {
            DdcError::Timeout { waited_ms } => Reading::Timeout {
                waited_ms: *waited_ms,
                message: error.message(),
            },
            DdcError::Ambiguous { count } => Reading::Ambiguous {
                count: *count,
                message: error.message(),
            },
            DdcError::NoPhysicalMonitor => Reading::Unavailable {
                message: error.message(),
            },
            DdcError::Api { call, win32_error } => Reading::TransportError {
                call: Some((*call).to_string()),
                win32_error: Some(*win32_error),
                message: error.message(),
            },
            DdcError::Crashed => Reading::TransportError {
                call: None,
                win32_error: None,
                message: error.message(),
            },
        }
    }

    /// Machine-readable state name, matching the documented evidence vocabulary.
    pub fn state(&self) -> &'static str {
        match self {
            Reading::Readable { .. } => "readable",
            Reading::Timeout { .. } => "timeout",
            Reading::TransportError { .. } => "transport_error",
            Reading::Unavailable { .. } => "unavailable",
            Reading::Ambiguous { .. } => "ambiguous",
            Reading::NotRequested => "not_requested",
        }
    }

    pub fn is_readable(&self) -> bool {
        matches!(self, Reading::Readable { .. })
    }
}

/// Brightness and contrast evidence read during discovery.
#[derive(Clone, Debug, Serialize)]
pub struct ProbeReport {
    pub brightness: Reading,
    pub contrast: Reading,
    pub vcp_brightness: Reading,
    pub vcp_contrast: Reading,
    pub elapsed_ms: u64,
}

/// A monitor that can be selected and controlled.
#[derive(Clone, Debug, Serialize)]
pub struct MonitorRecord {
    pub id: String,
    pub display_index: u32,
    pub identity_confidence: IdentityConfidence,
    pub identity_evidence: Vec<String>,
    pub friendly_name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub manufacturer: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub product_code: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub edid_serial: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub edid_name: Option<String>,
    pub device_name: String,
    pub monitor_device_path: String,
    pub connector: String,
    pub output_technology: String,
    pub connector_instance: u32,
    pub physical_monitor: Vec<String>,
    pub primary: bool,
    pub position: Rect,
    pub controllable: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub probe: Option<ProbeReport>,
}

/// A display that was detected and deliberately not offered for control.
#[derive(Clone, Debug, Serialize)]
pub struct ExcludedRecord {
    pub friendly_name: String,
    pub device_name: String,
    pub connector: String,
    pub reason: String,
    pub detail: String,
}

/// Full result of `monitors list`.
#[derive(Clone, Debug, Serialize)]
pub struct MonitorListReport {
    pub backend: String,
    pub observed_at: String,
    pub elapsed_ms: u64,
    pub monitors: Vec<MonitorRecord>,
    pub excluded: Vec<ExcludedRecord>,
    pub notes: Vec<String>,
}

/// Capabilities evidence for one monitor.
#[derive(Clone, Debug, Serialize)]
pub struct CapabilitiesSection {
    pub state: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub raw: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub declared_length: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub parsed: Option<Capabilities>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
}

/// Per-control evidence rows.
#[derive(Clone, Debug, Serialize)]
pub struct ControlEvidence {
    pub name: String,
    pub vcp_code: String,
    /// `None` when no capabilities string was available.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub advertised: Option<bool>,
    pub reading: Reading,
    pub provenance: String,
}

/// Full result of `monitors capabilities`.
#[derive(Clone, Debug, Serialize)]
pub struct CapabilitiesReport {
    pub backend: String,
    pub observed_at: String,
    pub monitor: MonitorRecord,
    pub capabilities: CapabilitiesSection,
    pub controls: Vec<ControlEvidence>,
    pub notes: Vec<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn identifiers_are_stable_and_namespaced() {
        let first = monitor_id(Some("MSI"), Some(0x3CA7), "ABC123");
        let second = monitor_id(Some("MSI"), Some(0x3CA7), "ABC123");
        assert_eq!(first, second);
        assert!(first.starts_with("mon-"));
        assert_eq!(first.len(), 4 + ID_HEX_CHARS);
    }

    #[test]
    fn different_evidence_yields_different_identifiers() {
        let a = monitor_id(Some("MSI"), Some(0x3CA7), "ABC123");
        let b = monitor_id(Some("MSI"), Some(0x3CA7), "ABC124");
        let c = monitor_id(Some("DEL"), Some(0x3CA7), "ABC123");
        assert_ne!(a, b);
        assert_ne!(a, c);
    }

    #[test]
    fn missing_evidence_still_produces_an_identifier() {
        let id = monitor_id(None, None, "\\\\?\\DISPLAY#XYZ");
        assert!(id.starts_with("mon-"));
        assert_eq!(id.len(), 4 + ID_HEX_CHARS);
    }

    #[test]
    fn reading_states_match_the_evidence_vocabulary() {
        assert_eq!(Reading::NotRequested.state(), "not_requested");
        assert_eq!(
            Reading::from_error(&DdcError::Timeout { waited_ms: 2000 }).state(),
            "timeout"
        );
        assert_eq!(
            Reading::from_error(&DdcError::Ambiguous { count: 2 }).state(),
            "ambiguous"
        );
        assert!(!Reading::from_error(&DdcError::NoPhysicalMonitor).is_readable());
    }

    #[test]
    fn high_level_and_vcp_readings_keep_their_backend() {
        let high = Reading::from_high_level(
            &Ok(HighLevelReading {
                minimum: 10,
                current: 55,
                maximum: 100,
            }),
            0x10,
        );
        match high {
            Reading::Readable {
                backend,
                minimum,
                current,
                ..
            } => {
                assert_eq!(backend, "dxva2-high-level");
                assert_eq!(minimum, Some(10));
                assert_eq!(current, 55);
            }
            other => panic!("unexpected reading {other:?}"),
        }

        let vcp = Reading::from_vcp(&Ok(VcpReading {
            vcp_code: 0x12,
            current: 70,
            maximum: 100,
            code_type: VcpCodeType::SetParameter,
        }));
        match vcp {
            Reading::Readable {
                backend,
                vcp_code,
                code_type,
                ..
            } => {
                assert_eq!(backend, "dxva2-vcp");
                assert_eq!(vcp_code, "0x12");
                assert_eq!(code_type, Some(VcpCodeType::SetParameter));
            }
            other => panic!("unexpected reading {other:?}"),
        }
    }
}
