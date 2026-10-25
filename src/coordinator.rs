//! The command coordinator.
//!
//! One coordinator per user session owns every hardware operation. Both the CLI and, later, the
//! tray issue the same typed commands here; the coordinator is the only place that decides what a
//! command means. It holds the settings, the profile registry and the selected monitors, and it
//! serialises access to the transport.
//!
//! The coordinator does not render anything and does not block on hardware outside the transport's
//! own bounded calls.

use std::path::PathBuf;

use serde_json::{json, Value};

use crate::config::registry::ProfileRegistry;
use crate::config::settings::{self, Settings, UnsupportedPolicy};
use crate::config::value::{percent_to_native, ControlValue, DeviceValueSupport};
use crate::config::Profile;
use crate::ipc::{Failure, PROTOCOL_VERSION};
use crate::plan::{self, PlanError};
use crate::transport::{ControlReading, MonitorSummary, Transport, TransportError};

/// Controls with an executable write path in this build, in the transport's own spelling.
///
/// The planner knows every control in the registry; this list is the narrower set the current
/// backend can actually send. A requested control outside it is reported as `not_implemented`
/// rather than silently dropped or misrepresented as a device limitation.
const WRITE_PATH_CONTROLS: &[&str] = &["brightness", "contrast"];

fn build_writes(control: &str) -> bool {
    WRITE_PATH_CONTROLS.contains(&control)
}

/// Controls a profile requests that this build cannot write at all.
fn unimplemented_controls(profile: &Profile) -> Vec<&'static str> {
    profile
        .requested_control_names()
        .into_iter()
        .filter(|name| !build_writes(name))
        .collect()
}

/// The part of a profile this build can execute.
///
/// Planning only sees controls that have a write path, so missing backend support can never be
/// confused with missing device support: the former is reported as `not_implemented`, the latter
/// comes back from the plan as `unsupported`.
fn writable_part(profile: &Profile) -> Profile {
    let mut restricted = profile.clone();
    let monitor = &mut restricted.monitor;
    if !build_writes("color_temperature") {
        monitor.color_temperature = None;
    }
    if !build_writes("color_preset") {
        monitor.color_preset = None;
    }
    if !build_writes("rgb_gain.red") {
        monitor.rgb_gain.red = None;
    }
    if !build_writes("rgb_gain.green") {
        monitor.rgb_gain.green = None;
    }
    if !build_writes("rgb_gain.blue") {
        monitor.rgb_gain.blue = None;
    }
    if !build_writes("contrast") {
        monitor.contrast = None;
    }
    if !build_writes("brightness") {
        monitor.brightness = None;
    }
    if !build_writes("volume") {
        monitor.volume = None;
    }
    if !build_writes("mute") {
        monitor.mute = None;
    }
    restricted
}

/// The single cause to report for a monitor, or for a whole apply, that did not fully succeed.
///
/// Ordered by how much the result can be trusted: an outcome-unknown write outranks a positive
/// `unsupported` reply, which outranks a build limitation.
const CAUSE_PRIORITY: &[&str] = &[
    "timeout",
    "transport_error",
    "protocol_error",
    "verification_failed",
    "unsupported",
    "blocked_by_mode",
    "not_implemented",
    "invalid_value",
];

fn dominant_cause(fields: &Value) -> &'static str {
    let Some(fields) = fields.as_array() else {
        return "not_applied";
    };
    for cause in CAUSE_PRIORITY {
        if fields.iter().any(|field| field["state"] == *cause) {
            return cause;
        }
    }
    "not_applied"
}

fn row_causes(rows: &Value) -> &'static str {
    let Some(rows) = rows.as_array() else {
        return "not_applied";
    };
    for cause in CAUSE_PRIORITY {
        if rows.iter().any(|row| row["cause"] == *cause) {
            return cause;
        }
    }
    "not_applied"
}

/// Map a planning failure onto the coordinator's typed failure.
fn plan_failure(monitor: Option<&str>, error: &PlanError) -> Failure {
    let kind = match error {
        PlanError::Unsupported { .. } => "unsupported",
        PlanError::Invalid { .. } => "invalid",
        PlanError::Conflict(_) => "invalid_config",
    };
    let message = match monitor {
        Some(monitor) => format!("{monitor}: {error}"),
        None => error.to_string(),
    };
    Failure::new(kind, message)
}

/// Why a command was refused.
fn invalid(message: impl Into<String>) -> Failure {
    Failure::new("invalid", message)
}

fn unknown_method(method: &str) -> Failure {
    Failure::new(
        "unknown_method",
        format!("`{method}` is not a coordinator method in this build"),
    )
}

fn transport_failure(error: TransportError) -> Failure {
    Failure::new(error.kind(), error.message().to_string())
}

fn string_param(params: &Value, key: &str) -> Result<String, Failure> {
    params
        .get(key)
        .and_then(Value::as_str)
        .map(|value| value.to_string())
        .ok_or_else(|| invalid(format!("`{key}` must be a string")))
}

fn control_param(params: &Value) -> Result<&'static str, Failure> {
    let control = string_param(params, "control")?;
    match control.as_str() {
        "brightness" => Ok("brightness"),
        "contrast" => Ok("contrast"),
        other => Err(Failure::new(
            "unsupported",
            format!("`{other}` is not a coordinator control in this build; brightness and contrast exist"),
        )),
    }
}

fn reading_json(reading: &ControlReading) -> Value {
    json!({
        "value": reading.current,
        "minimum": reading.minimum,
        "maximum": reading.maximum,
        "backend": reading.backend,
    })
}

/// Commands over one transport, plus the configuration they act on.
pub struct Coordinator<T: Transport> {
    transport: T,
    settings: Settings,
    registry: ProfileRegistry,
    settings_path: PathBuf,
    profiles_dir: Option<PathBuf>,
    /// Last fully applied profile per monitor. A profile id is not a claim about current hardware
    /// state; it only records what this coordinator last applied and verified.
    last_applied: std::collections::HashMap<String, String>,
    shutdown_requested: bool,
    observed_at: String,
}

impl<T: Transport> Coordinator<T> {
    pub fn new(transport: T, settings: Settings, registry: ProfileRegistry, settings_path: PathBuf) -> Coordinator<T> {
        Coordinator {
            transport,
            settings,
            registry,
            settings_path,
            profiles_dir: None,
            last_applied: std::collections::HashMap::new(),
            shutdown_requested: false,
            observed_at: crate::clock::iso_local(),
        }
    }

    /// Where the profile registry came from, so `reload` can re-read it.
    pub fn with_profiles_dir(mut self, directory: PathBuf) -> Coordinator<T> {
        self.profiles_dir = Some(directory);
        self
    }

    pub fn shutdown_requested(&self) -> bool {
        self.shutdown_requested
    }

    /// Peak concurrent transport operations, when the transport reports it.
    pub fn transport_peak_concurrency(&self) -> Option<u32> {
        self.transport.peak_concurrency()
    }

    pub fn settings(&self) -> &Settings {
        &self.settings
    }

    pub fn monitor_summaries(&mut self) -> Result<Vec<MonitorSummary>, TransportError> {
        self.transport.monitors()
    }

    pub fn read(&mut self, monitor: &str, control: &str) -> Result<ControlReading, TransportError> {
        self.transport.read(monitor, control)
    }

    /// Resolve a requested value and write it, verifying the readback.
    ///
    /// A successful direct change makes the monitor `Custom`: an older profile id must not stay
    /// labelled active merely because one of its values still happens to match.
    pub fn set(
        &mut self,
        monitor: &str,
        control: &str,
        value: ControlValue,
    ) -> Result<Value, Failure> {
        let reading = self
            .transport
            .read(monitor, control)
            .map_err(transport_failure)?;

        let native = match value {
            ControlValue::Percent(percent) => {
                let (Some(minimum), Some(maximum)) = (reading.minimum, reading.maximum) else {
                    return Err(invalid(
                        "the display reported no range for this control, so a percentage cannot be mapped",
                    ));
                };
                if maximum < minimum {
                    return Err(invalid("the display reported an inverted range for this control"));
                }
                percent_to_native(percent, minimum, maximum)
            }
            ControlValue::Native(native) => {
                if let (Some(minimum), Some(maximum)) = (reading.minimum, reading.maximum) {
                    if native < minimum || native > maximum {
                        return Err(invalid(format!(
                            "native value {native} is outside the reported range {minimum}..={maximum}; nothing was written"
                        )));
                    }
                }
                native
            }
        };

        let outcome = self
            .transport
            .write(monitor, control, native)
            .map_err(transport_failure)?;

        if !outcome.verified() {
            // The monitor is in an unverified state, so no profile is claimed for it either.
            self.last_applied.remove(monitor);
            return Err(Failure::new(
                "verification_failed",
                format!(
                    "{}: wrote {} and the device reports {}",
                    control, outcome.native_target, outcome.observed
                ),
            ));
        }

        self.last_applied.remove(monitor);
        Ok(json!({
            "monitor": monitor,
            "control": control,
            "native_target": outcome.native_target,
            "observed": outcome.observed,
            "verified": true,
            "backend": outcome.backend,
        }))
    }

    fn persist_settings(&mut self) -> Result<(), Failure> {
        crate::config::settings::save(&self.settings_path, &self.settings)
            .map_err(|error| Failure::new("io_error", error.render()))
    }

    /// Which monitors a profile command targets. Broadcasts are never implicit.
    fn resolve_targets(&mut self, params: &Value) -> Result<Vec<String>, Failure> {
        if params
            .get("all_selected")
            .and_then(Value::as_bool)
            .unwrap_or(false)
        {
            if self.settings.monitor_ids.is_empty() {
                return Err(invalid(
                    "all_selected was requested but the controlled set is empty",
                ));
            }
            return Ok(self.settings.monitor_ids.clone());
        }
        if let Some(monitor) = params.get("monitor").and_then(Value::as_str) {
            return Ok(vec![monitor.to_string()]);
        }
        if let Some(list) = params.get("monitor_ids").and_then(Value::as_array) {
            if list.is_empty() {
                return Err(invalid("monitor_ids is empty; a target is required"));
            }
            let mut ids: Vec<String> = Vec::new();
            for entry in list {
                let id = entry
                    .as_str()
                    .ok_or_else(|| invalid("monitor_ids entries must be strings"))?;
                // A repeated target would write the same monitor twice in one apply.
                if !ids.iter().any(|seen| seen == id) {
                    ids.push(id.to_string());
                }
            }
            return Ok(ids);
        }
        Err(invalid(
            "a target is required: monitor <id>, monitor_ids or all_selected",
        ))
    }

    /// Read the device metadata one write path needs.
    ///
    /// Only the controls the profile actually requests are read: discovery stays bounded to the
    /// codes 0x10 and 0x12 that this build can write. An `unsupported` reply is positive evidence
    /// about the display; any other transport error aborts the preflight.
    fn read_metadata(
        &mut self,
        monitor: &str,
        controls: &[&str],
    ) -> Result<crate::plan::DeviceMetadata, TransportError> {
        let mut metadata = crate::plan::DeviceMetadata::default();
        for control in controls {
            let support = match self.transport.read(monitor, control) {
                Ok(reading) => match (reading.minimum, reading.maximum) {
                    (Some(minimum), Some(maximum)) if maximum >= minimum => {
                        DeviceValueSupport::Range { minimum, maximum }
                    }
                    // A getter that reports no usable range cannot validate a percentage.
                    _ => DeviceValueSupport::Unknown,
                },
                Err(TransportError::Unsupported(_)) => DeviceValueSupport::Unsupported,
                Err(error) => return Err(error),
            };
            match *control {
                "brightness" => metadata.brightness = support,
                "contrast" => metadata.contrast = support,
                _ => {}
            }
        }
        Ok(metadata)
    }

    /// Apply a whole profile to every target, after planning all of them.
    ///
    /// Nothing is written until every target has been planned, so the strict policy cannot modify
    /// one monitor and then discover that another cannot be served. Execution then stops on a
    /// monitor as soon as a write leaves the hardware outcome uncertain; a timed-out write is
    /// reported, never replayed.
    fn apply_everywhere(&mut self, id: &str, targets: &[String]) -> Result<Value, Failure> {
        let profile = self
            .registry
            .find(id)
            .cloned()
            .ok_or_else(|| Failure::new("invalid", format!("unknown profile `{id}`")))?;

        let policy = self.settings.unsupported;

        // White-point conflicts are a property of the profile, so they are refused before any
        // monitor is even read.
        plan::check_conflicts(&profile).map_err(|error| plan_failure(None, &error))?;

        // A build limitation is not a device property: it is reported as such, and it is part of the
        // preflight under the strict policy.
        let unimplemented = unimplemented_controls(&profile);

        // Every target must exist before the first write; otherwise a typo would write half a set.
        let present: Vec<String> = self
            .transport
            .monitors()
            .map_err(transport_failure)?
            .into_iter()
            .map(|monitor| monitor.id)
            .collect();
        for monitor in targets {
            if !present.iter().any(|id| id == monitor) {
                return Err(Failure::new(
                    "unknown_monitor",
                    format!("monitor {monitor} is not present in this session"),
                ));
            }
        }

        if policy == UnsupportedPolicy::Fail && !unimplemented.is_empty() {
            return Err(Failure::new(
                "unsupported",
                format!(
                    "profiles.unsupported = fail: {} {} not written by this build; nothing was written",
                    unimplemented.join(", "),
                    if unimplemented.len() == 1 { "is" } else { "are" }
                ),
            ));
        }

        // Preflight: plan every target before executing any of them.
        let writable = writable_part(&profile);
        let requested = writable.requested_control_names();
        let mut prepared: Vec<(String, plan::ApplyPlan)> = Vec::new();
        for monitor in targets {
            let metadata = self
                .read_metadata(monitor, &requested)
                .map_err(transport_failure)?;
            let target_plan = plan::plan(&writable, &metadata, policy)
                .map_err(|error| plan_failure(Some(monitor), &error))?;
            prepared.push((monitor.clone(), target_plan));
        }

        // Execute. Each monitor is a transaction: once a write is outcome-unknown, the remaining
        // planned writes are not attempted.
        let mut results: Vec<Value> = Vec::new();
        for (monitor, target_plan) in prepared {
            let mut fields: Vec<Value> = Vec::new();
            let mut applied = 0usize;
            let mut stopped: Option<Failure> = None;

            for write in &target_plan.writes {
                let control = write.control.name();
                if let Some(failure) = &stopped {
                    fields.push(json!({
                        "control": control,
                        "status": "not_attempted",
                        "requested": write.requested.to_string(),
                        "native": write.native,
                        "state": "transaction_stopped",
                        "reason": format!(
                            "not attempted: the previous write on this monitor failed with {}",
                            failure.kind
                        ),
                    }));
                    continue;
                }

                match self.transport.write(&monitor, control, write.native) {
                    Ok(outcome) if outcome.verified() => {
                        applied += 1;
                        fields.push(json!({
                            "control": control,
                            "status": "applied",
                            "requested": write.requested.to_string(),
                            "native": outcome.native_target,
                            "observed": outcome.observed,
                            "backend": outcome.backend,
                        }));
                    }
                    Ok(outcome) => {
                        // A readback mismatch is a verification failure, not success.
                        let failure = Failure::new(
                            "verification_failed",
                            format!(
                                "{}: wrote {} and the device reports {}",
                                control, outcome.native_target, outcome.observed
                            ),
                        );
                        fields.push(json!({
                            "control": control,
                            "status": "failed",
                            "requested": write.requested.to_string(),
                            "native": outcome.native_target,
                            "observed": outcome.observed,
                            "state": "verification_failed",
                            "error": failure,
                        }));
                        stopped = Some(failure);
                    }
                    Err(TransportError::Unsupported(message)) => {
                        // Positive evidence from the device; the path is still healthy.
                        fields.push(json!({
                            "control": control,
                            "status": "skipped",
                            "requested": write.requested.to_string(),
                            "native": write.native,
                            "state": "unsupported",
                            "reason": message,
                        }));
                    }
                    Err(error) => {
                        let failure = transport_failure(error);
                        fields.push(json!({
                            "control": control,
                            "status": "failed",
                            "requested": write.requested.to_string(),
                            "native": write.native,
                            "state": failure.kind,
                            "error": failure,
                        }));
                        stopped = Some(failure);
                    }
                }
            }

            for skip in &target_plan.skipped {
                fields.push(json!({
                    "control": skip.control.name(),
                    "status": "skipped",
                    "state": skip.state,
                    "reason": skip.reason,
                }));
            }
            for control in &unimplemented {
                fields.push(json!({
                    "control": control,
                    "status": "skipped",
                    "state": "not_implemented",
                    "reason": format!("this build has no write path for {control}"),
                }));
            }

            let not_applied_fields = fields
                .iter()
                .filter(|field| field["status"] != "applied")
                .count();
            let status = if applied > 0 && not_applied_fields == 0 {
                "applied"
            } else if applied > 0 {
                "partial"
            } else {
                "not_applied"
            };

            if status == "applied" {
                self.last_applied
                    .insert(monitor.clone(), profile.id.clone());
            } else {
                // A partial or failed apply leaves the monitor in an unverified state; an older
                // profile id must not stay labelled active.
                self.last_applied.remove(&monitor);
            }

            let mut row = json!({
                "monitor": monitor,
                "status": status,
                "fields": fields,
            });
            if status != "applied" {
                row["cause"] = json!(dominant_cause(&row["fields"]));
            }
            results.push(row);
        }

        let applied_targets = results
            .iter()
            .filter(|row| row["status"] == "applied")
            .count();
        let partial_targets = results
            .iter()
            .filter(|row| row["status"] == "partial")
            .count();
        let status = if applied_targets == results.len() {
            "applied"
        } else if applied_targets > 0 || partial_targets > 0 {
            "partial"
        } else {
            "not_applied"
        };

        let mut result = json!({
            "profile": profile.id,
            "status": status,
            "monitors": results,
        });
        if status != "applied" {
            result["cause"] = json!(row_causes(&result["monitors"]));
        }
        Ok(result)
    }

    /// Dispatch one request. The returned value is the `result` field of the response.
    pub fn handle(&mut self, method: &str, params: &Value) -> Result<Value, Failure> {
        match method {
            "ping" => Ok(json!({"protocol": PROTOCOL_VERSION})),
            "status" => {
                let monitors = self
                    .transport
                    .monitors()
                    .map_err(transport_failure)?
                    .into_iter()
                    .map(|monitor| {
                        json!({
                            "id": monitor.id,
                            "friendly_name": monitor.friendly_name,
                            "connector": monitor.connector,
                        })
                    })
                    .collect::<Vec<Value>>();
                Ok(json!({
                    "running": true,
                    "protocol": PROTOCOL_VERSION,
                    "observed_at": self.observed_at,
                    "monitors": monitors,
                    "profiles": self.registry.len(),
                    "profile_ids": self.registry.profiles().iter().map(|profile| profile.id.clone()).collect::<Vec<String>>(),
                    "selection": self.settings.monitor_ids,
                    "settings_path": self.settings_path.display().to_string(),
                }))
            }
            "monitors.list" => {
                let monitors = self.transport.monitors().map_err(transport_failure)?;
                Ok(json!({
                    "monitors": monitors.iter().map(|monitor| json!({
                        "id": monitor.id,
                        "friendly_name": monitor.friendly_name,
                        "connector": monitor.connector,
                        "primary": monitor.primary,
                    })).collect::<Vec<Value>>()
                }))
            }
            "parameters.get" => {
                let monitor = string_param(params, "monitor")?;
                let control = control_param(params)?;
                let reading = self
                    .transport
                    .read(&monitor, control)
                    .map_err(transport_failure)?;
                Ok(json!({"control": control, "reading": reading_json(&reading)}))
            }
            "parameters.set" => {
                let monitor = string_param(params, "monitor")?;
                let control = control_param(params)?;
                let value = params
                    .get("value")
                    .and_then(Value::as_u64)
                    .ok_or_else(|| invalid("`value` must be a non-negative integer"))?;
                let unit = params
                    .get("unit")
                    .and_then(Value::as_str)
                    .unwrap_or("native");
                let typed = match unit {
                    "native" => ControlValue::Native(value),
                    "percent" if value <= 100 => ControlValue::Percent(value as u32),
                    "percent" => {
                        return Err(invalid(format!(
                            "{value} is not a percentage; percentages range 0..=100"
                        )))
                    }
                    other => return Err(invalid(format!("unknown unit `{other}`"))),
                };
                self.set(&monitor, control, typed)
            }
            "profiles.list" => Ok(json!({
                "profiles": self.registry.profiles().iter().map(|profile| json!({
                    "id": profile.id,
                    "name": profile.name,
                    "icon": profile.icon,
                    "order": profile.order,
                    "controls": profile.requested_control_names(),
                })).collect::<Vec<Value>>()
            })),
            "profiles.validate" => Ok(json!({
                "valid": true,
                "count": self.registry.len(),
                "profiles": self.registry.profiles().iter().map(|profile| profile.id.clone()).collect::<Vec<String>>(),
            })),
            "settings.get" => match params.get("key").and_then(Value::as_str) {
                None => Ok(settings_json(&self.settings)),
                Some(key) => settings_value(&self.settings, key)
                    .ok_or_else(|| invalid(format!("unknown settings key `{key}`"))),
            },
            "settings.set" => {
                let key = string_param(params, "key")?;
                let value = string_param(params, "value")?;
                let text = std::fs::read_to_string(&self.settings_path).unwrap_or_default();
                let updated = settings::set_key(&self.settings_path, &text, &key, &value)
                    .map_err(|error| Failure::new("invalid_config", error.render()))?;
                let parsed = settings::parse(&updated, &self.settings_path)
                    .map_err(|error| Failure::new("invalid_config", error.render()))?;
                if let Some(parent) = self.settings_path.parent() {
                    let _ = std::fs::create_dir_all(parent);
                }
                std::fs::write(&self.settings_path, updated).map_err(|error| {
                    Failure::new("io_error", format!("cannot write settings: {error}"))
                })?;
                self.settings = parsed;
                Ok(json!({"key": key, "value": settings_value(&self.settings, &key).unwrap_or(Value::Null)}))
            }
            "monitors.select" => {
                let requested = params
                    .get("monitor_ids")
                    .and_then(Value::as_array)
                    .ok_or_else(|| invalid("`monitor_ids` must be an array of identifiers"))?;
                let mut ids: Vec<String> = Vec::new();
                for entry in requested {
                    let id = entry
                        .as_str()
                        .ok_or_else(|| invalid("`monitor_ids` entries must be strings"))?;
                    if !self
                        .transport
                        .monitors()
                        .map_err(transport_failure)?
                        .iter()
                        .any(|monitor| monitor.id == id)
                    {
                        return Err(Failure::new(
                            "unknown_monitor",
                            format!("monitor {id} is not present in this session"),
                        ));
                    }
                    ids.push(id.to_string());
                }
                let remember = params
                    .get("remember")
                    .and_then(Value::as_bool)
                    .unwrap_or(self.settings.remember);
                if !remember && !ids.is_empty() {
                    self.settings.remember = false;
                    self.settings.monitor_ids = Vec::new();
                } else {
                    self.settings.remember = remember;
                    self.settings.monitor_ids = ids;
                }
                self.persist_settings()?;
                Ok(json!({
                    "remember": self.settings.remember,
                    "monitor_ids": self.settings.monitor_ids,
                }))
            }
            "profiles.reload" => {
                let Some(directory) = self.profiles_dir.clone() else {
                    return Err(invalid("this coordinator has no profiles directory configured"));
                };
                match ProfileRegistry::load_dir(&directory) {
                    Ok(registry) => {
                        self.registry = registry;
                        Ok(json!({
                            "reloaded": true,
                            "count": self.registry.len(),
                            "profiles": self.registry.profiles().iter().map(|profile| profile.id.clone()).collect::<Vec<String>>(),
                        }))
                    }
                    // Keep the last good registry: a reload must not silently reduce the set.
                    Err(error) => Err(Failure::new("invalid_config", error.render())),
                }
            }
            "profile.apply" => {
                let id = string_param(params, "id")?;
                let targets = self.resolve_targets(params)?;
                self.apply_everywhere(&id, &targets)
            }
            "profile.next" => {
                let monitor = string_param(params, "monitor")?;
                let current = self.last_applied.get(&monitor).cloned();
                let next = match current.as_deref() {
                    Some(id) => self
                        .registry
                        .next_after(id)
                        .map(|profile| profile.id.clone()),
                    None => self.registry.profiles().first().map(|profile| profile.id.clone()),
                };
                let Some(next) = next else {
                    return Err(invalid("there is no profile to select"));
                };
                self.apply_everywhere(&next, &[monitor])
            }
            "startup.status" => {
                let monitors: Vec<String> = self
                    .transport
                    .monitors()
                    .map_err(transport_failure)?
                    .into_iter()
                    .map(|monitor| monitor.id)
                    .collect();
                let state = crate::lifecycle::StartupState {
                    monitors: &monitors,
                    registry: Ok(&self.registry),
                    settings: &self.settings,
                    selection_confirmed_empty: false,
                };
                let outcome = crate::lifecycle::decide(&state);
                Ok(json!({
                    "outcome": outcome.as_str(),
                    "monitors": monitors,
                    "profiles": self.registry.profiles().iter().map(|profile| profile.id.clone()).collect::<Vec<String>>(),
                    "selection": self.settings.monitor_ids,
                }))
            }
            "icons.list" => Ok(json!({"icons": crate::config::profile::ICON_NAMES})),
            "monitors.validate_value" | "monitors.validate-value" => {
                let monitor = string_param(params, "monitor")?;
                let control = control_param(params)?;
                let value = params
                    .get("value")
                    .and_then(Value::as_u64)
                    .ok_or_else(|| invalid("`value` must be a non-negative integer"))?;
                let confirmed = params
                    .get("confirm_write")
                    .and_then(Value::as_bool)
                    .unwrap_or(false);
                if !confirmed {
                    return Err(invalid(
                        "validate-value performs one supervised write and requires confirm_write",
                    ));
                }
                let outcome = self
                    .transport
                    .write(&monitor, control, value)
                    .map_err(transport_failure)?;
                Ok(json!({
                    "monitor": monitor,
                    "control": control,
                    "native_target": outcome.native_target,
                    "observed": outcome.observed,
                    "verified": outcome.verified(),
                    "backend": outcome.backend,
                    "evidence": "write_observed",
                }))
            }
            "paths.profiles" => Ok(json!({
                "profiles": self.profiles_dir.as_ref().map(|path| path.display().to_string()),
                "settings": self.settings_path.display().to_string(),
            })),
            "shutdown" => {
                self.shutdown_requested = true;
                Ok(json!({"stopping": true}))
            }
            other => Err(unknown_method(other)),
        }
    }
}

fn settings_json(settings: &Settings) -> Value {
    json!({
        "schema_version": settings.schema_version,
        "startup": {"launch_at_login": settings.launch_at_login},
        "tray": {"left_click": settings.left_click.as_str(), "badges": "when_multiple"},
        "selection": {"remember": settings.remember, "monitor_ids": settings.monitor_ids},
        "profiles": {"startup": "preserve", "unsupported": settings.unsupported.as_str()},
        "logging": {"level": settings.log_level.as_str()},
    })
}

fn settings_value(settings: &Settings, key: &str) -> Option<Value> {
    match key {
        "startup.launch_at_login" => Some(json!(settings.launch_at_login)),
        "tray.left_click" => Some(json!(settings.left_click.as_str())),
        "tray.badges" => Some(json!("when_multiple")),
        "selection.remember" => Some(json!(settings.remember)),
        "selection.monitor_ids" => Some(json!(settings.monitor_ids)),
        "profiles.startup" => Some(json!("preserve")),
        "profiles.unsupported" => Some(json!(settings.unsupported.as_str())),
        "logging.level" => Some(json!(settings.log_level.as_str())),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::transport::FakeTransport;

    fn coordinator() -> Coordinator<FakeTransport> {
        let transport = FakeTransport::with_monitor("mon-a", "Panel").with_value("mon-a", "brightness", 40);
        Coordinator::new(
            transport,
            Settings::default(),
            ProfileRegistry::default(),
            PathBuf::from("C:\\test\\settings.toml"),
        )
    }

    #[test]
    fn ping_reports_the_protocol_version() {
        let mut coordinator = coordinator();
        let result = coordinator.handle("ping", &Value::Null).expect("ping");
        assert_eq!(result["protocol"], PROTOCOL_VERSION);
    }

    #[test]
    fn status_reports_runtime_state() {
        let mut coordinator = coordinator();
        let result = coordinator.handle("status", &Value::Null).expect("status");
        assert_eq!(result["running"], true);
        assert_eq!(result["profiles"], 0);
        assert!(result["monitors"].as_array().is_some());
    }

    #[test]
    fn percent_set_uses_the_reported_range() {
        let mut coordinator = coordinator();
        let result = coordinator
            .handle("parameters.set", &json!({"monitor": "mon-a", "control": "brightness", "value": 50, "unit": "percent"}))
            .expect("set");
        assert_eq!(result["native_target"], 50);
        assert_eq!(result["verified"], true);
    }

    #[test]
    fn native_set_is_bounds_checked_without_writing() {
        let mut coordinator = coordinator();
        let error = coordinator
            .handle("parameters.set", &json!({"monitor": "mon-a", "control": "brightness", "value": 200, "unit": "native"}))
            .expect_err("out of range");
        assert_eq!(error.kind, "invalid");
        assert!(coordinator.transport.writes.is_empty());
    }

    #[test]
    fn a_write_that_does_not_verify_is_a_failure_not_success() {
        let mut coordinator = coordinator();
        coordinator.transport.observe_offset = 2;
        let error = coordinator
            .handle("parameters.set", &json!({"monitor": "mon-a", "control": "brightness", "value": 40, "unit": "native"}))
            .expect_err("mismatch");
        assert_eq!(error.kind, "verification_failed");
    }

    #[test]
    fn unsupported_controls_and_unknown_methods_are_typed_errors() {
        let mut coordinator = coordinator();
        let control = coordinator
            .handle("parameters.get", &json!({"monitor": "mon-a", "control": "sharpness"}))
            .expect_err("unsupported");
        assert_eq!(control.kind, "unsupported");

        let method = coordinator
            .handle("set-vcp", &json!({"code": 0xD6, "value": 5}))
            .expect_err("unknown method");
        assert_eq!(method.kind, "unknown_method");
    }

    fn sample_profile(id: &str, order: i64, body: &str) -> crate::config::Profile {
        let text = format!(
            "schema_version = 1\nid = \"{id}\"\nname = \"{id}\"\nicon = \"day\"\norder = {order}\n\n[monitor]\n{body}\n"
        );
        crate::config::profile::parse(&text, std::path::Path::new("test.toml")).expect("profile")
    }

    fn scratch(name: &str) -> PathBuf {
        let mut directory = std::env::temp_dir();
        directory.push(format!("broiler-coordinator-{}-{}", std::process::id(), name));
        let _ = std::fs::remove_dir_all(&directory);
        std::fs::create_dir_all(&directory).expect("scratch directory");
        directory
    }

    fn coordinator_with(
        transport: FakeTransport,
        profiles: Vec<crate::config::Profile>,
        directory: &std::path::Path,
    ) -> Coordinator<FakeTransport> {
        Coordinator::new(
            transport,
            Settings::default(),
            ProfileRegistry::new(profiles).expect("registry"),
            directory.join("settings.toml"),
        )
        .with_profiles_dir(directory.to_path_buf())
    }

    #[test]
    fn selecting_a_monitor_persists_only_present_identifiers() {
        let directory = scratch("select");
        let transport = FakeTransport::with_monitor("mon-a", "Panel");
        let mut coordinator = coordinator_with(transport, Vec::new(), &directory);

        let result = coordinator
            .handle(
                "monitors.select",
                &json!({"monitor_ids": ["mon-a"], "remember": true}),
            )
            .expect("select");
        assert_eq!(result["remember"], true);
        assert_eq!(result["monitor_ids"][0], "mon-a");
        assert!(directory.join("settings.toml").exists());

        let error = coordinator
            .handle(
                "monitors.select",
                &json!({"monitor_ids": ["mon-ghost"], "remember": true}),
            )
            .expect_err("unknown monitor");
        assert_eq!(error.kind, "unknown_monitor");
        let _ = std::fs::remove_dir_all(&directory);
    }

    #[test]
    fn applying_a_profile_writes_supported_controls_and_marks_the_rest_partial() {
        let directory = scratch("apply");
        let transport = FakeTransport::with_monitor("mon-a", "Panel");
        let profile = sample_profile("day", 10, "brightness = 100\ncontrast = 80\nvolume = 20");
        let mut coordinator = coordinator_with(transport, vec![profile], &directory);

        let result = coordinator
            .handle("profile.apply", &json!({"id": "day", "monitor": "mon-a"}))
            .expect("apply");

        assert_eq!(result["status"], "partial");
        // Volume is not a device limitation here: this build has no write path for it.
        let volume = field_row(&result, "mon-a", "volume");
        assert_eq!(volume["status"], "skipped");
        assert_eq!(volume["state"], "not_implemented");
        let brightness = field_row(&result, "mon-a", "brightness");
        assert_eq!(brightness["status"], "applied");
        assert_eq!(brightness["observed"], 100);
        assert_eq!(brightness["native"], 100);
        assert_eq!(field_row(&result, "mon-a", "contrast")["status"], "applied");
        // Plan order puts contrast before brightness, and both were sent once.
        assert_eq!(
            coordinator.transport.writes,
            vec![
                (String::from("mon-a"), String::from("contrast"), 80),
                (String::from("mon-a"), String::from("brightness"), 100),
            ]
        );
        let _ = std::fs::remove_dir_all(&directory);
    }

    #[test]
    fn applying_to_all_selected_requires_a_selection() {
        let directory = scratch("all-selected");
        let transport = FakeTransport::with_monitor("mon-a", "Panel");
        let profile = sample_profile("day", 10, "brightness = 50");
        let mut coordinator = coordinator_with(transport, vec![profile], &directory);

        let error = coordinator
            .handle("profile.apply", &json!({"id": "day", "all_selected": true}))
            .expect_err("empty selection");
        assert_eq!(error.kind, "invalid");
        assert!(coordinator.transport.writes.is_empty());
        let _ = std::fs::remove_dir_all(&directory);
    }

    #[test]
    fn profile_next_cycles_in_registry_order() {
        let directory = scratch("next");
        let transport = FakeTransport::with_monitor("mon-a", "Panel");
        let profiles = vec![
            sample_profile("day", 10, "brightness = 100"),
            sample_profile("night", 20, "brightness = 0"),
        ];
        let mut coordinator = coordinator_with(transport, profiles, &directory);

        let first = coordinator
            .handle("profile.next", &json!({"monitor": "mon-a"}))
            .expect("first");
        assert_eq!(first["profile"], "day");
        let second = coordinator
            .handle("profile.next", &json!({"monitor": "mon-a"}))
            .expect("second");
        assert_eq!(second["profile"], "night");
        let third = coordinator
            .handle("profile.next", &json!({"monitor": "mon-a"}))
            .expect("third");
        assert_eq!(third["profile"], "day");
        let _ = std::fs::remove_dir_all(&directory);
    }

    #[test]
    fn validate_value_needs_an_explicit_confirmation() {
        let directory = scratch("validate");
        let transport = FakeTransport::with_monitor("mon-a", "Panel");
        let mut coordinator = coordinator_with(transport, Vec::new(), &directory);

        let error = coordinator
            .handle(
                "monitors.validate_value",
                &json!({"monitor": "mon-a", "control": "brightness", "value": 5}),
            )
            .expect_err("no confirmation");
        assert_eq!(error.kind, "invalid");
        assert!(coordinator.transport.writes.is_empty());

        let result = coordinator
            .handle(
                "monitors.validate_value",
                &json!({"monitor": "mon-a", "control": "brightness", "value": 5, "confirm_write": true}),
            )
            .expect("confirmed");
        assert_eq!(result["verified"], true);
        assert_eq!(coordinator.transport.writes.len(), 1);
        let _ = std::fs::remove_dir_all(&directory);
    }

    #[test]
    fn startup_status_reports_residency_for_two_profiles() {
        let directory = scratch("startup");
        let transport = FakeTransport::with_monitor("mon-a", "Panel");
        let profiles = vec![
            sample_profile("day", 10, "brightness = 100"),
            sample_profile("night", 20, "brightness = 0"),
        ];
        let mut coordinator = coordinator_with(transport, profiles, &directory);

        let result = coordinator
            .handle("startup.status", &Value::Null)
            .expect("startup");
        assert_eq!(result["outcome"], "resident");
        let _ = std::fs::remove_dir_all(&directory);
    }

    #[test]
    fn reload_keeps_the_last_good_registry_when_a_file_is_invalid() {
        let directory = scratch("reload");
        std::fs::write(
            directory.join("day.toml"),
            "schema_version = 1\nid = \"day\"\nname = \"Day\"\nicon = \"day\"\n\n[monitor]\nbrightness = 50\n",
        )
        .expect("write profile");
        let prefix_errors = ProfileRegistry::load_dir(&directory).expect("initial registry");
        let transport = FakeTransport::with_monitor("mon-a", "Panel");
        let mut coordinator = Coordinator::new(
            transport,
            Settings::default(),
            prefix_errors,
            directory.join("settings.toml"),
        )
        .with_profiles_dir(directory.clone());

        std::fs::write(directory.join("broken.toml"), "schema_version = 1\nid = \"x\"\n")
            .expect("write broken profile");
        let error = coordinator
            .handle("profiles.reload", &Value::Null)
            .expect_err("invalid file");
        assert_eq!(error.kind, "invalid_config");
        // The last good registry is still in service.
        let listed = coordinator
            .handle("profiles.list", &Value::Null)
            .expect("list");
        assert_eq!(listed["profiles"].as_array().map(|items| items.len()), Some(1));
        let _ = std::fs::remove_dir_all(&directory);
    }

    #[test]
    fn shutdown_marks_the_coordinator() {
        let mut coordinator = coordinator();
        coordinator.handle("shutdown", &Value::Null).expect("shutdown");
        assert!(coordinator.shutdown_requested());
    }

    // -----------------------------------------------------------------------
    // P0: strict preflight, failure boundaries and multi-target aggregation
    // -----------------------------------------------------------------------

    use crate::config::settings::UnsupportedPolicy;
    use crate::transport::TransportError;

    fn strict() -> Settings {
        Settings {
            unsupported: UnsupportedPolicy::Fail,
            ..Settings::default()
        }
    }

    fn coordinator_with_settings(
        transport: FakeTransport,
        profiles: Vec<crate::config::Profile>,
        directory: &std::path::Path,
        settings: Settings,
    ) -> Coordinator<FakeTransport> {
        Coordinator::new(
            transport,
            settings,
            ProfileRegistry::new(profiles).expect("registry"),
            directory.join("settings.toml"),
        )
        .with_profiles_dir(directory.to_path_buf())
    }

    fn monitor_row<'a>(result: &'a Value, monitor: &str) -> &'a Value {
        result["monitors"]
            .as_array()
            .expect("monitors array")
            .iter()
            .find(|entry| entry["monitor"] == monitor)
            .expect("monitor row")
    }

    fn field_row<'a>(result: &'a Value, monitor: &str, control: &str) -> &'a Value {
        monitor_row(result, monitor)["fields"]
            .as_array()
            .expect("fields array")
            .iter()
            .find(|entry| entry["control"] == control)
            .expect("field row")
    }

    #[test]
    fn strict_policy_refuses_a_profile_with_an_unwritable_control_before_any_write() {
        let directory = scratch("strict-unwritable");
        let transport = FakeTransport::with_monitor("mon-a", "Panel");
        let profile = sample_profile("night", 10, "brightness = 0\nvolume = 20");
        let mut coordinator =
            coordinator_with_settings(transport, vec![profile], &directory, strict());

        let error = coordinator
            .handle("profile.apply", &json!({"id": "night", "monitor": "mon-a"}))
            .expect_err("strict policy refuses the unwritable control");

        assert_eq!(error.kind, "unsupported");
        assert!(coordinator.transport.writes.is_empty(), "nothing may be written");
        let _ = std::fs::remove_dir_all(&directory);
    }

    #[test]
    fn strict_policy_writes_nothing_on_either_target_when_one_target_lacks_a_control() {
        let directory = scratch("strict-targets");
        let transport = FakeTransport::with_monitor("mon-a", "Panel")
            .add_monitor("mon-b", "Second")
            .without_control("mon-b", "contrast");
        let profile = sample_profile("day", 10, "brightness = 100\ncontrast = 80");
        let mut coordinator =
            coordinator_with_settings(transport, vec![profile], &directory, strict());

        let error = coordinator
            .handle(
                "profile.apply",
                &json!({"id": "day", "monitor_ids": ["mon-a", "mon-b"]}),
            )
            .expect_err("one target cannot provide the requested control");

        assert_eq!(error.kind, "unsupported");
        assert!(
            coordinator.transport.writes.is_empty(),
            "the healthy target must not be written when preflight fails anywhere"
        );
        let _ = std::fs::remove_dir_all(&directory);
    }

    #[test]
    fn skip_policy_applies_the_writable_fields_and_names_what_it_skipped() {
        let directory = scratch("skip-unwritable");
        let transport = FakeTransport::with_monitor("mon-a", "Panel");
        let profile = sample_profile("day", 10, "brightness = 40\nvolume = 20");
        let mut coordinator = coordinator_with(transport, vec![profile], &directory);

        let result = coordinator
            .handle("profile.apply", &json!({"id": "day", "monitor": "mon-a"}))
            .expect("apply");

        assert_eq!(result["status"], "partial");
        assert_eq!(field_row(&result, "mon-a", "brightness")["status"], "applied");
        let volume = field_row(&result, "mon-a", "volume");
        assert_eq!(volume["status"], "skipped");
        assert_eq!(volume["state"], "not_implemented");
        assert_eq!(coordinator.transport.writes.len(), 1);
        let _ = std::fs::remove_dir_all(&directory);
    }

    #[test]
    fn a_timeout_stops_the_rest_of_that_monitors_transaction() {
        let directory = scratch("timeout-stop");
        let transport = FakeTransport::with_monitor("mon-a", "Panel").fail_write(
            "mon-a",
            "contrast",
            TransportError::Timeout(String::from(
                "the DDC worker did not answer; the hardware outcome is unknown",
            )),
        );
        let profile = sample_profile("day", 10, "contrast = 70\nbrightness = 50");
        let mut coordinator = coordinator_with(transport, vec![profile], &directory);

        let result = coordinator
            .handle("profile.apply", &json!({"id": "day", "monitor": "mon-a"}))
            .expect("apply reports an outcome rather than aborting");

        assert_eq!(result["status"], "not_applied");
        assert_eq!(result["cause"], "timeout");
        assert_eq!(field_row(&result, "mon-a", "contrast")["status"], "failed");
        assert_eq!(
            field_row(&result, "mon-a", "brightness")["status"],
            "not_attempted"
        );
        assert!(
            coordinator.transport.writes.is_empty(),
            "no write may follow an outcome-unknown write in the same transaction"
        );
        let _ = std::fs::remove_dir_all(&directory);
    }

    #[test]
    fn one_failed_target_can_never_turn_a_mixed_result_into_applied() {
        let directory = scratch("mixed");
        let transport = FakeTransport::with_monitor("mon-a", "Panel")
            .add_monitor("mon-b", "Second")
            .fail_write(
                "mon-b",
                "brightness",
                TransportError::Timeout(String::from("no answer from the worker")),
            );
        let profile = sample_profile("day", 10, "brightness = 60");
        let mut coordinator = coordinator_with(transport, vec![profile], &directory);

        let result = coordinator
            .handle(
                "profile.apply",
                &json!({"id": "day", "monitor_ids": ["mon-a", "mon-b"]}),
            )
            .expect("apply");

        assert_eq!(result["status"], "partial");
        assert_eq!(result["cause"], "timeout");
        assert_eq!(monitor_row(&result, "mon-a")["status"], "applied");
        assert_eq!(monitor_row(&result, "mon-b")["status"], "not_applied");
        let _ = std::fs::remove_dir_all(&directory);
    }

    #[test]
    fn every_target_failing_is_not_applied_with_a_typed_cause() {
        let directory = scratch("all-failed");
        let transport = FakeTransport::with_monitor("mon-a", "Panel").fail_write(
            "mon-a",
            "brightness",
            TransportError::Transport(String::from("the pipe closed")),
        );
        let profile = sample_profile("day", 10, "brightness = 60");
        let mut coordinator = coordinator_with(transport, vec![profile], &directory);

        let result = coordinator
            .handle("profile.apply", &json!({"id": "day", "monitor": "mon-a"}))
            .expect("apply");

        assert_eq!(result["status"], "not_applied");
        assert_eq!(result["cause"], "transport_error");
        let _ = std::fs::remove_dir_all(&directory);
    }

    #[test]
    fn an_unknown_target_is_refused_before_planning() {
        let directory = scratch("unknown-target");
        let transport = FakeTransport::with_monitor("mon-a", "Panel");
        let profile = sample_profile("day", 10, "brightness = 60");
        let mut coordinator = coordinator_with(transport, vec![profile], &directory);

        let error = coordinator
            .handle("profile.apply", &json!({"id": "day", "monitor": "mon-ghost"}))
            .expect_err("target is not present");

        assert_eq!(error.kind, "unknown_monitor");
        assert!(coordinator.transport.writes.is_empty());
        let _ = std::fs::remove_dir_all(&directory);
    }

    #[test]
    fn a_direct_set_clears_the_applied_profile_marker() {
        let directory = scratch("direct-set");
        let transport = FakeTransport::with_monitor("mon-a", "Panel");
        let profile = sample_profile("day", 10, "brightness = 60");
        let mut coordinator = coordinator_with(transport, vec![profile], &directory);

        coordinator
            .handle("profile.apply", &json!({"id": "day", "monitor": "mon-a"}))
            .expect("apply");
        assert_eq!(
            coordinator.last_applied.get("mon-a").map(String::as_str),
            Some("day")
        );

        coordinator
            .handle(
                "parameters.set",
                &json!({"monitor": "mon-a", "control": "brightness", "value": 40, "unit": "percent"}),
            )
            .expect("direct set");

        assert!(
            !coordinator.last_applied.contains_key("mon-a"),
            "a direct change makes the monitor Custom, not still on the old profile"
        );
        let _ = std::fs::remove_dir_all(&directory);
    }

    #[test]
    fn strict_policy_refuses_conflicting_white_point_controls_before_any_write() {
        let directory = scratch("strict-conflict");
        let transport = FakeTransport::with_monitor("mon-a", "Panel");
        let mut profile = sample_profile("day", 10, "brightness = 40");
        profile.monitor.color_temperature =
            Some(crate::config::profile::ColorTemperature::Kelvin(6500));
        profile.monitor.rgb_gain.red = Some(crate::config::value::ControlValue::Percent(100));
        let mut coordinator =
            coordinator_with_settings(transport, vec![profile], &directory, strict());

        let error = coordinator
            .handle("profile.apply", &json!({"id": "day", "monitor": "mon-a"}))
            .expect_err("conflicting white-point mechanisms");

        assert!(coordinator.transport.writes.is_empty());
        assert!(error.message.contains("RGB gains"), "{}", error.message);
        let _ = std::fs::remove_dir_all(&directory);
    }
}
