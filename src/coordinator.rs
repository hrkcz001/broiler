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
use crate::config::settings::{self, Settings};
use crate::config::value::{percent_to_native, ControlValue};
use crate::ipc::{Failure, PROTOCOL_VERSION};
use crate::transport::{ControlReading, MonitorSummary, Transport, TransportError};

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
            return Err(Failure::new(
                "verification_failed",
                format!(
                    "{}: wrote {} and the device reports {}",
                    control, outcome.native_target, outcome.observed
                ),
            ));
        }

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
        Err(invalid(
            "a target is required: monitor <id> or all_selected",
        ))
    }

    /// Apply one control of a profile to one monitor and verify it.
    fn apply_control(
        &mut self,
        monitor: &str,
        control: &str,
        value: crate::config::value::ControlValue,
    ) -> Result<Value, Failure> {
        self.set(monitor, control, value)
    }

    /// Apply a whole profile to one monitor.
    ///
    /// Controls that this build cannot yet write are reported as `unsupported` instead of being
    /// silently dropped, and they make the result `partial` rather than `applied`.
    fn apply_one(&mut self, profile: &crate::config::Profile, monitor: &str) -> Value {
        let mut fields: Vec<Value> = Vec::new();
        let mut applied = 0usize;
        let mut unsupported = 0usize;
        let mut failed = 0usize;

        let mut supported: Vec<(&str, crate::config::value::ControlValue)> = Vec::new();
        if let Some(value) = profile.monitor.contrast {
            supported.push(("contrast", value));
        }
        if let Some(value) = profile.monitor.brightness {
            supported.push(("brightness", value));
        }

        for (control, value) in supported {
            match self.apply_control(monitor, control, value) {
                Ok(result) => {
                    applied += 1;
                    fields.push(json!({
                        "control": control,
                        "status": "applied",
                        "record": result,
                    }));
                }
                Err(failure) => {
                    failed += 1;
                    fields.push(json!({
                        "control": control,
                        "status": "failed",
                        "error": failure,
                    }));
                }
            }
        }

        // Everything else the profile asks for is known but not writable in this build.
        let mut deferred: Vec<&str> = Vec::new();
        if profile.monitor.color_temperature.is_some() {
            deferred.push("color_temperature");
        }
        if profile.monitor.color_preset.is_some() {
            deferred.push("color_preset");
        }
        if profile.monitor.rgb_gain.red.is_some() {
            deferred.push("rgb_gain.red");
        }
        if profile.monitor.rgb_gain.green.is_some() {
            deferred.push("rgb_gain.green");
        }
        if profile.monitor.rgb_gain.blue.is_some() {
            deferred.push("rgb_gain.blue");
        }
        if profile.monitor.volume.is_some() {
            deferred.push("volume");
        }
        if profile.monitor.mute.is_some() {
            deferred.push("mute");
        }
        for control in deferred {
            unsupported += 1;
            fields.push(json!({
                "control": control,
                "status": "unsupported",
                "reason": "this control has no write path in this build",
            }));
        }

        let status = if applied > 0 && unsupported == 0 && failed == 0 {
            "applied"
        } else if applied > 0 {
            "partial"
        } else {
            "not_applied"
        };

        json!({
            "monitor": monitor,
            "status": status,
            "fields": fields,
        })
    }

    fn apply_everywhere(&mut self, id: &str, targets: &[String]) -> Result<Value, Failure> {
        let profile = self
            .registry
            .find(id)
            .cloned()
            .ok_or_else(|| Failure::new("invalid", format!("unknown profile `{id}`")))?;

        let mut results: Vec<Value> = Vec::new();
        let mut any_applied = false;
        let mut any_partial = false;

        for monitor in targets {
            let outcome = self.apply_one(&profile, monitor);
            match outcome.get("status").and_then(Value::as_str) {
                Some("applied") => {
                    any_applied = true;
                    self.last_applied
                        .insert(monitor.clone(), profile.id.clone());
                }
                Some("partial") => any_partial = true,
                _ => {}
            }
            results.push(outcome);
        }

        let status = if any_applied && !any_partial {
            "applied"
        } else if any_applied || any_partial {
            "partial"
        } else {
            "not_applied"
        };

        Ok(json!({
            "profile": profile.id,
            "status": status,
            "monitors": results,
        }))
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
        let fields = result["monitors"][0]["fields"].as_array().expect("fields");
        let volume = fields
            .iter()
            .find(|field| field["control"] == "volume")
            .expect("volume row");
        assert_eq!(volume["status"], "unsupported");
        let brightness = fields
            .iter()
            .find(|field| field["control"] == "brightness")
            .expect("brightness row");
        assert_eq!(brightness["status"], "applied");
        assert_eq!(brightness["record"]["observed"], 100);
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
}
