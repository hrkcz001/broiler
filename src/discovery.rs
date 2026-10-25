//! Read-only monitor discovery.
//!
//! Discovery joins three sources of evidence, in this order:
//!
//! 1. the active display topology, which names each source device and connector;
//! 2. the EDID that Windows cached for that monitor device, used only for identity;
//! 3. bounded Dxva2 reads of `0x10` and `0x12`, and the capabilities string on request.
//!
//! Built-in panels are excluded from the controllable list and reported separately with the
//! reason. Nothing here writes to a monitor.

use std::time::{Duration, Instant};

use serde::Serialize;

use crate::caps::{self, Capabilities};
use crate::clock;
use crate::model::{
    self, CapabilitiesReport, CapabilitiesSection, ControlEvidence, ExcludedRecord, IdentityConfidence,
    MonitorListReport, MonitorRecord, ProbeReport, Reading,
};
use crate::win::ddc::{self, DdcError, ProbeOutcome, VCP_BRIGHTNESS, VCP_CONTRAST};
use crate::win::topology::{self, DisplayTarget};
use crate::win::{edid, registry};

/// Default bound for a single monitor probe. The architecture starts with a 2 s worker
/// watchdog; discovery uses the same bound per monitor so one wedged monitor cannot stall a
/// list operation.
pub const DEFAULT_PROBE_BUDGET: Duration = Duration::from_millis(2000);

/// Capabilities replies are slower than single VCP reads, so they get a larger bound while
/// the whole command stays inside the documented discovery budget.
pub const CAPABILITIES_PROBE_BUDGET: Duration = Duration::from_millis(6000);

/// Why discovery produced no usable report.
#[derive(Clone, Debug)]
pub enum DiscoveryFailure {
    /// The display topology could not be queried.
    Topology(topology::TopologyError),
    /// `--monitor` named an identifier this session does not know.
    UnknownMonitor {
        requested: String,
        available: Vec<String>,
    },
    /// The named identifier matched more than one present monitor.
    AmbiguousMonitor { requested: String },
}

impl DiscoveryFailure {
    pub fn message(&self) -> String {
        match self {
            DiscoveryFailure::Topology(error) => error.describe(),
            DiscoveryFailure::UnknownMonitor {
                requested,
                available,
            } => {
                if available.is_empty() {
                    format!(
                        "monitor {requested} is not present and no controllable monitor was found"
                    )
                } else {
                    format!(
                        "monitor {requested} is not present; available monitors: {}",
                        available.join(", ")
                    )
                }
            }
            DiscoveryFailure::AmbiguousMonitor { requested } => format!(
                "monitor {requested} matches more than one present monitor; select it again after \
                 the topology settles"
            ),
        }
    }
}

/// A monitor candidate before any DDC probe.
#[derive(Clone, Debug)]
struct Candidate {
    target: DisplayTarget,
    manufacturer: Option<String>,
    product_code: Option<u16>,
    serial: Option<String>,
    edid_name: Option<String>,
    identity_evidence: Vec<String>,
    confidence: IdentityConfidence,
    id: String,
    builtin: bool,
}

fn split_device_path(path: &str) -> Option<(String, String)> {
    let parts: Vec<&str> = path.split('#').collect();
    if parts.len() < 3 {
        return None;
    }
    let hardware_id = parts[1].trim();
    let instance = parts[2].trim();
    if hardware_id.is_empty() || instance.is_empty() {
        return None;
    }
    Some((hardware_id.to_string(), instance.to_string()))
}

fn read_edid(target: &DisplayTarget) -> Option<(edid::EdidIdentity, String)> {
    let (hardware_id, instance) = split_device_path(&target.monitor_device_path)?;
    let key = format!("SYSTEM\\CurrentControlSet\\Enum\\DISPLAY\\{hardware_id}\\{instance}\\Device Parameters");
    let blob = registry::read_binary(&key, "EDID")?;
    let identity = edid::parse(&blob)?;
    Some((identity, key))
}

fn candidate_from_target(target: DisplayTarget) -> Candidate {
    let mut identity_evidence = Vec::new();
    let mut manufacturer = None;
    let mut product_code = None;
    let mut serial = None;
    let mut edid_name = None;
    let mut confidence = IdentityConfidence::ConnectionBound;

    if let Some((identity, key_path)) = read_edid(&target) {
        manufacturer = identity.manufacturer.clone();
        product_code = identity.product_code;
        serial = identity.serial_for_identity();
        edid_name = identity.monitor_name.clone();
        identity_evidence.push(format!("edid: {key_path}\\EDID"));
        if serial.is_some() {
            confidence = IdentityConfidence::EdidSerial;
        }
    } else {
        identity_evidence.push(String::from(
            "edid: no cached EDID blob for this monitor device",
        ));
    }

    if manufacturer.is_none() {
        manufacturer = edid::decode_manufacturer(target.edid_manufacture_id);
        if manufacturer.is_some() {
            identity_evidence.push(String::from(
                "manufacturer: display target edidManufactureId",
            ));
        }
    }
    if product_code.is_none() || product_code == Some(0) {
        let from_target = target.edid_product_code;
        if from_target != 0 {
            product_code = Some(from_target);
            identity_evidence.push(String::from(
                "product: display target edidProductCodeId",
            ));
        }
    }

    let (id, evidence_value) = match serial.as_ref() {
        Some(serial) => (
            model::monitor_id(manufacturer.as_deref(), product_code, serial),
            format!("serial:{serial}"),
        ),
        None => {
            let binding = if target.monitor_device_path.is_empty() {
                target.device_name.clone()
            } else {
                target.monitor_device_path.clone()
            };
            confidence = IdentityConfidence::ConnectionBound;
            identity_evidence.push(format!("identity bound to connection: {binding}"));
            (
                model::monitor_id(manufacturer.as_deref(), product_code, &binding),
                format!("connection:{binding}"),
            )
        }
    };
    identity_evidence.push(format!("identity input: {evidence_value}"));

    Candidate {
        builtin: topology::is_builtin_output(target.output_technology),
        target,
        manufacturer,
        product_code,
        serial,
        edid_name,
        identity_evidence,
        confidence,
        id,
    }
}

fn scan() -> Result<Vec<Candidate>, DiscoveryFailure> {
    let targets = topology::enumerate().map_err(DiscoveryFailure::Topology)?;
    let mut candidates: Vec<Candidate> = targets.into_iter().map(candidate_from_target).collect();

    // Identical evidence on two active monitors means neither identifier is trustworthy:
    // the two cannot be told apart after an arbitrary port swap.
    let ids: Vec<String> = candidates.iter().map(|candidate| candidate.id.clone()).collect();
    for candidate in &mut candidates {
        let duplicates = ids.iter().filter(|id| **id == candidate.id).count();
        if duplicates > 1 && candidate.confidence != IdentityConfidence::Ambiguous {
            candidate.confidence = IdentityConfidence::Ambiguous;
            candidate.identity_evidence.push(format!(
                "identity is ambiguous: {duplicates} active monitors share the same identity evidence"
            ));
        }
    }
    Ok(candidates)
}

fn probe_budget(read_capabilities: bool) -> Duration {
    if read_capabilities {
        CAPABILITIES_PROBE_BUDGET
    } else {
        DEFAULT_PROBE_BUDGET
    }
}

fn probe_report(outcome: &ProbeOutcome) -> ProbeReport {
    ProbeReport {
        brightness: Reading::from_high_level(&outcome.brightness_high_level, VCP_BRIGHTNESS),
        contrast: Reading::from_high_level(&outcome.contrast_high_level, VCP_CONTRAST),
        vcp_brightness: Reading::from_vcp(&outcome.brightness_vcp),
        vcp_contrast: Reading::from_vcp(&outcome.contrast_vcp),
        elapsed_ms: outcome.elapsed_ms,
    }
}

fn probe_reading(outcome: &Result<ProbeOutcome, DdcError>) -> ProbeReport {
    match outcome {
        Ok(outcome) => probe_report(outcome),
        Err(error) => ProbeReport {
            brightness: Reading::from_error(error),
            contrast: Reading::from_error(error),
            vcp_brightness: Reading::from_error(error),
            vcp_contrast: Reading::from_error(error),
            elapsed_ms: 0,
        },
    }
}

fn build_record(
    candidate: &Candidate,
    display_index: u32,
    probe: Option<ProbeReport>,
    physical: Vec<String>,
) -> MonitorRecord {
    let controllable = probe
        .as_ref()
        .map(|report| report.brightness.is_readable() || report.vcp_brightness.is_readable())
        .unwrap_or(false)
        && candidate.confidence != IdentityConfidence::Ambiguous;

    MonitorRecord {
        id: candidate.id.clone(),
        display_index,
        identity_confidence: candidate.confidence,
        identity_evidence: candidate.identity_evidence.clone(),
        friendly_name: if candidate.target.friendly_name.is_empty() {
            candidate
                .edid_name
                .clone()
                .unwrap_or_else(|| candidate.target.device_name.clone())
        } else {
            candidate.target.friendly_name.clone()
        },
        manufacturer: candidate.manufacturer.clone(),
        product_code: candidate.product_code.map(|code| format!("0x{code:04X}")),
        edid_serial: candidate.serial.clone(),
        edid_name: candidate.edid_name.clone(),
        device_name: candidate.target.device_name.clone(),
        monitor_device_path: candidate.target.monitor_device_path.clone(),
        connector: candidate.target.output_technology_name.clone(),
        output_technology: candidate.target.output_technology_name.clone(),
        connector_instance: candidate.target.connector_instance,
        physical_monitor: physical,
        primary: candidate.target.primary,
        position: candidate.target.position,
        controllable,
        probe,
    }
}

/// Externally controllable candidates in deterministic session order: left to right, then
/// top to bottom. The resulting position gives the session `display_index`.
fn ordered_externals(candidates: &[Candidate]) -> Vec<&Candidate> {
    let mut external: Vec<&Candidate> = candidates
        .iter()
        .filter(|candidate| !candidate.builtin)
        .collect();
    external.sort_by_key(|candidate| (candidate.target.position.x, candidate.target.position.y));
    external
}

fn excluded_record(candidate: &Candidate) -> ExcludedRecord {
    ExcludedRecord {
        friendly_name: if candidate.target.friendly_name.is_empty() {
            candidate
                .edid_name
                .clone()
                .unwrap_or_else(|| candidate.target.device_name.clone())
        } else {
            candidate.target.friendly_name.clone()
        },
        device_name: candidate.target.device_name.clone(),
        connector: candidate.target.output_technology_name.clone(),
        reason: String::from("internal_panel"),
        detail: format!(
            "output technology is {}; built-in panels are never offered for control",
            candidate.target.output_technology_name
        ),
    }
}

/// Built-in panels that exist on the machine but are not part of the active desktop.
///
/// A laptop panel stays enumerated while the lid is closed, so a user who later opens the lid
/// should already see why broiler does not offer it. This query is read-only and its results are
/// never probed or controlled.
fn dormant_builtin_panels(active: &[Candidate]) -> Vec<ExcludedRecord> {
    let Ok(all_paths) = topology::enumerate_all() else {
        return Vec::new();
    };

    let mut seen: Vec<String> = active
        .iter()
        .map(|candidate| candidate.target.monitor_device_path.clone())
        .collect();
    let mut records = Vec::new();

    for target in all_paths {
        if !topology::is_builtin_output(target.output_technology) || !target.available {
            continue;
        }
        if seen.iter().any(|path| path == &target.monitor_device_path) {
            continue;
        }
        seen.push(target.monitor_device_path.clone());

        // Built-in panels often ship an EDID with no name descriptor and no friendly name, so the
        // last resort is the vendor and product code from the blob.
        let edid_name = read_edid(&target).and_then(|(identity, _)| {
            identity.monitor_name.clone().or_else(|| {
                let manufacturer = identity.manufacturer?;
                let product = identity.product_code?;
                Some(format!("{manufacturer}{product:04X}"))
            })
        });
        let friendly_name = if target.friendly_name.is_empty() {
            edid_name.unwrap_or_else(|| target.device_name.clone())
        } else {
            target.friendly_name.clone()
        };
        records.push(ExcludedRecord {
            friendly_name,
            device_name: target.device_name.clone(),
            connector: target.output_technology_name.clone(),
            reason: String::from("internal_panel"),
            detail: format!(
                "built-in panel is present but not part of the active desktop; output technology is {}",
                target.output_technology_name
            ),
        });
    }
    records
}

/// Enumerate monitors and probe brightness and contrast read-only.
pub fn list() -> Result<MonitorListReport, DiscoveryFailure> {
    let started = Instant::now();
    let candidates = scan()?;

    let mut excluded: Vec<ExcludedRecord> = candidates
        .iter()
        .filter(|candidate| candidate.builtin)
        .map(excluded_record)
        .collect();
    excluded.extend(dormant_builtin_panels(&candidates));
    let external = ordered_externals(&candidates);

    let mut monitors = Vec::new();
    let mut notes = Vec::new();
    for (offset, candidate) in external.iter().enumerate() {
        let outcome = ddc::probe(candidate.target.hmonitor, false, probe_budget(false));
        let (report, physical) = match &outcome {
            Ok(outcome) => (Some(probe_report(outcome)), outcome.descriptions.clone()),
            Err(error) => (Some(probe_reading(&outcome)), {
                notes.push(format!(
                    "monitor {} could not be read: {}",
                    candidate.id,
                    error.message()
                ));
                Vec::new()
            }),
        };
        if candidate.confidence == IdentityConfidence::Ambiguous {
            notes.push(format!(
                "monitor {} has ambiguous identity and is not controllable",
                candidate.id
            ));
        }
        monitors.push(build_record(candidate, offset as u32 + 1, report, physical));
    }

    if monitors.is_empty() {
        notes.push(String::from("no controllable external monitor was discovered"));
    }
    notes.push(String::from(
        "display_index is assigned per session from left to right; persisted numbering arrives with the state store",
    ));

    Ok(MonitorListReport {
        backend: String::from("dxva2"),
        observed_at: clock::iso_local(),
        elapsed_ms: clock::elapsed_ms_since(started),
        monitors,
        excluded,
        notes,
    })
}

/// A routing handle for one discovered monitor.
///
/// This is what the isolated worker keeps: the opaque identifier plus the `HMONITOR` it currently
/// resolves to. The handle is routing data and is re-resolved if the topology changes.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct TargetHandle {
    pub id: String,
    pub friendly_name: String,
    pub hmonitor: isize,
    pub builtin: bool,
    pub connector: String,
}

/// Enumerate monitor identifiers with their current handles, without any DDC traffic.
pub fn target_handles() -> Result<Vec<TargetHandle>, DiscoveryFailure> {
    let candidates = scan()?;
    Ok(candidates
        .iter()
        .map(|candidate| TargetHandle {
            id: candidate.id.clone(),
            friendly_name: if candidate.target.friendly_name.is_empty() {
                candidate
                    .edid_name
                    .clone()
                    .unwrap_or_else(|| candidate.target.device_name.clone())
            } else {
                candidate.target.friendly_name.clone()
            },
            hmonitor: candidate.target.hmonitor,
            builtin: candidate.builtin,
            connector: candidate.target.output_technology_name.clone(),
        })
        .collect())
}

fn find_candidate(candidates: &[Candidate], requested: &str) -> Result<usize, DiscoveryFailure> {
    let matches: Vec<usize> = candidates
        .iter()
        .enumerate()
        .filter(|(_, candidate)| !candidate.builtin && candidate.id == requested)
        .map(|(index, _)| index)
        .collect();
    match matches.len() {
        0 => Err(DiscoveryFailure::UnknownMonitor {
            requested: requested.to_string(),
            available: candidates
                .iter()
                .filter(|candidate| !candidate.builtin)
                .map(|candidate| candidate.id.clone())
                .collect(),
        }),
        1 => Ok(matches[0]),
        _ => Err(DiscoveryFailure::AmbiguousMonitor {
            requested: requested.to_string(),
        }),
    }
}

fn capabilities_section(outcome: &Result<ProbeOutcome, DdcError>) -> (CapabilitiesSection, Option<Capabilities>) {
    match outcome {
        Ok(outcome) => match &outcome.capabilities {
            Ok(raw) => {
                let parsed = caps::parse(raw);
                (
                    CapabilitiesSection {
                        state: String::from("readable"),
                        raw: Some(raw.text.clone()),
                        declared_length: Some(raw.declared_length),
                        parsed: Some(parsed.clone()),
                        message: None,
                    },
                    Some(parsed),
                )
            }
            Err(error) => (
                CapabilitiesSection {
                    state: error.state().to_string(),
                    raw: None,
                    declared_length: None,
                    parsed: None,
                    message: Some(error.message()),
                },
                None,
            ),
        },
        Err(error) => (
            CapabilitiesSection {
                state: error.state().to_string(),
                raw: None,
                declared_length: None,
                parsed: None,
                message: Some(error.message()),
            },
            None,
        ),
    }
}

fn control_evidence(name: &str, vcp_code: u8, reading: Reading, capabilities: Option<&Capabilities>) -> ControlEvidence {
    ControlEvidence {
        name: name.to_string(),
        vcp_code: format!("0x{vcp_code:02X}"),
        advertised: capabilities.map(|parsed| parsed.mentions(vcp_code)),
        provenance: match &capabilities {
            Some(_) => String::from("dxva2 read, capabilities string parsed"),
            None => String::from("dxva2 read only"),
        },
        reading,
    }
}

/// Read capabilities and control evidence for one monitor.
pub fn capabilities(requested: &str) -> Result<CapabilitiesReport, DiscoveryFailure> {
    let candidates = scan()?;
    let index = find_candidate(&candidates, requested)?;
    let candidate = candidates[index].clone();

    let display_index = ordered_externals(&candidates)
        .iter()
        .position(|other| other.id == candidate.id)
        .map(|offset| offset as u32 + 1)
        .unwrap_or(1);

    let outcome = ddc::probe(candidate.target.hmonitor, true, probe_budget(true));
    let (section, parsed) = capabilities_section(&outcome);
    let (probe, physical) = match &outcome {
        Ok(outcome) => (Some(probe_report(outcome)), outcome.descriptions.clone()),
        Err(_) => (Some(probe_reading(&outcome)), Vec::new()),
    };

    let record = build_record(&candidate, display_index, probe.clone(), physical);
    let mut controls = Vec::new();
    if let Some(report) = probe.as_ref() {
        controls.push(control_evidence(
            "brightness",
            VCP_BRIGHTNESS,
            report.brightness.clone(),
            parsed.as_ref(),
        ));
        controls.push(control_evidence(
            "contrast",
            VCP_CONTRAST,
            report.contrast.clone(),
            parsed.as_ref(),
        ));
        controls.push(control_evidence(
            "brightness (raw vcp)",
            VCP_BRIGHTNESS,
            report.vcp_brightness.clone(),
            parsed.as_ref(),
        ));
        controls.push(control_evidence(
            "contrast (raw vcp)",
            VCP_CONTRAST,
            report.vcp_contrast.clone(),
            parsed.as_ref(),
        ));
    }

    Ok(CapabilitiesReport {
        backend: String::from("dxva2"),
        observed_at: clock::iso_local(),
        monitor: record,
        capabilities: section,
        controls,
        notes: vec![
            String::from(
                "the capabilities string is advisory: a missing code is not proof of unsupported",
            ),
            String::from(
                "color temperature and the remaining controls arrive with their own gate; no write path exists in this build",
            ),
        ],
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn device_path_splitting_requires_three_parts() {
        assert_eq!(
            split_device_path("\\\\?\\DISPLAY#MSI3CA7#5&2f0b4a2d&0&UID4352#{guid}"),
            Some((String::from("MSI3CA7"), String::from("5&2f0b4a2d&0&UID4352")))
        );
        assert_eq!(split_device_path("\\\\?\\DISPLAY#MSI3CA7"), None);
        assert_eq!(split_device_path(""), None);
    }

    #[test]
    fn unknown_monitor_lists_available_identifiers() {
        let failure = DiscoveryFailure::UnknownMonitor {
            requested: String::from("mon-deadbeefdeadbeef"),
            available: vec![String::from("mon-0011223344556677")],
        };
        let message = failure.message();
        assert!(message.contains("mon-deadbeefdeadbeef"));
        assert!(message.contains("mon-0011223344556677"));
    }
}
