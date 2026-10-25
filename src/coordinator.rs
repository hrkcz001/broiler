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
            shutdown_requested: false,
            observed_at: crate::clock::iso_local(),
        }
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

    #[test]
    fn shutdown_marks_the_coordinator() {
        let mut coordinator = coordinator();
        coordinator.handle("shutdown", &Value::Null).expect("shutdown");
        assert!(coordinator.shutdown_requested());
    }
}
