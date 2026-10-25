//! The transport boundary the coordinator talks to.
//!
//! The coordinator never calls Dxva2 itself. It issues typed operations to a [`Transport`], of which
//! there are two: the real one that supervises the isolated DDC worker, and a deterministic fake used
//! by tests. This is what lets every command be tested without hardware.

use std::collections::HashMap;
use std::path::PathBuf;

use serde_json::{json, Value};

use crate::supervisor::{Supervisor, SupervisorError, WorkerCommand};

/// Why a transport operation failed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TransportError {
    /// The monitor is not present in this session.
    UnknownMonitor(String),
    /// The display positively does not offer the control.
    Unsupported(String),
    /// No response within the bound; the hardware outcome is unknown.
    Timeout(String),
    /// The channel failed.
    Transport(String),
    /// The worker answered something the coordinator cannot use.
    Protocol(String),
}

impl TransportError {
    /// Machine-readable kind carried over the wire and in CLI messages.
    pub fn kind(&self) -> &'static str {
        match self {
            TransportError::UnknownMonitor(_) => "unknown_monitor",
            TransportError::Unsupported(_) => "unsupported",
            TransportError::Timeout(_) => "timeout",
            TransportError::Transport(_) => "transport_error",
            TransportError::Protocol(_) => "protocol_error",
        }
    }

    pub fn message(&self) -> &str {
        match self {
            TransportError::UnknownMonitor(message)
            | TransportError::Unsupported(message)
            | TransportError::Timeout(message)
            | TransportError::Transport(message)
            | TransportError::Protocol(message) => message,
        }
    }
}

/// One monitor as the coordinator sees it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MonitorSummary {
    pub id: String,
    pub friendly_name: String,
    pub connector: String,
    pub primary: bool,
}

/// Current value and reported range of one control.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ControlReading {
    pub minimum: Option<u64>,
    pub current: u64,
    pub maximum: Option<u64>,
    pub backend: String,
}

/// Result of one verified write.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WriteOutcome {
    pub native_target: u64,
    pub observed: u64,
    pub backend: String,
}

impl WriteOutcome {
    pub fn verified(&self) -> bool {
        self.native_target == self.observed
    }
}

/// What the coordinator needs from the hardware side.
pub trait Transport {
    fn monitors(&mut self) -> Result<Vec<MonitorSummary>, TransportError>;
    fn read(&mut self, monitor: &str, control: &str) -> Result<ControlReading, TransportError>;
    fn write(
        &mut self,
        monitor: &str,
        control: &str,
        value: u64,
    ) -> Result<WriteOutcome, TransportError>;

    /// Diagnostic hook: peak number of operations that were inside this transport at once.
    ///
    /// Only the fake transport implements it; a real transport returns `None` because the worker
    /// boundary already makes overlap impossible.
    fn peak_concurrency(&self) -> Option<u32> {
        None
    }
}

fn value_of<T: serde::de::DeserializeOwned>(value: Value, what: &str) -> Result<T, TransportError> {
    serde_json::from_value(value)
        .map_err(|error| TransportError::Protocol(format!("worker returned an invalid {what}: {error}")))
}

/// The production transport: every call goes through the supervised worker process.
pub struct WorkerTransport {
    supervisor: Supervisor,
}

impl WorkerTransport {
    pub fn new(tray_executable: PathBuf) -> WorkerTransport {
        WorkerTransport {
            supervisor: Supervisor::new(WorkerCommand::internal(tray_executable)),
        }
    }

    pub fn with_supervisor(supervisor: Supervisor) -> WorkerTransport {
        WorkerTransport { supervisor }
    }

    pub fn supervisor_mut(&mut self) -> &mut Supervisor {
        &mut self.supervisor
    }

    fn call(&mut self, method: &str, params: Value) -> Result<Value, TransportError> {
        self.supervisor.request(method, params).map_err(|error| match error {
            SupervisorError::Timeout { waited_ms } => TransportError::Timeout(format!(
                "the DDC worker did not answer within {waited_ms} ms; the hardware outcome is unknown"
            )),
            SupervisorError::Device(failure) => match failure.kind.as_str() {
                "unknown_monitor" => TransportError::UnknownMonitor(failure.message),
                "unsupported" => TransportError::Unsupported(failure.message),
                "timeout" => TransportError::Timeout(failure.message),
                other => TransportError::Transport(format!("{other}: {}", failure.message)),
            },
            other => TransportError::Transport(other.message()),
        })
    }
}

impl Transport for WorkerTransport {
    fn monitors(&mut self) -> Result<Vec<MonitorSummary>, TransportError> {
        let result = self.call("monitors", Value::Null)?;
        #[derive(serde::Deserialize)]
        struct Entry {
            id: String,
            friendly_name: String,
            connector: String,
            builtin: bool,
        }
        #[derive(serde::Deserialize)]
        struct Body {
            monitors: Vec<Entry>,
        }
        let body: Body = value_of(result, "monitor list")?;
        Ok(body
            .monitors
            .into_iter()
            .filter(|entry| !entry.builtin)
            .map(|entry| MonitorSummary {
                id: entry.id,
                friendly_name: entry.friendly_name,
                connector: entry.connector,
                primary: false,
            })
            .collect())
    }

    fn read(&mut self, monitor: &str, control: &str) -> Result<ControlReading, TransportError> {
        let result = self.call("read", json!({"monitor": monitor, "control": control}))?;
        Ok(ControlReading {
            minimum: result.get("minimum").and_then(Value::as_u64),
            current: result.get("value").and_then(Value::as_u64).unwrap_or(0),
            maximum: result.get("maximum").and_then(Value::as_u64),
            backend: result
                .get("backend")
                .and_then(Value::as_str)
                .unwrap_or("unknown")
                .to_string(),
        })
    }

    fn write(
        &mut self,
        monitor: &str,
        control: &str,
        value: u64,
    ) -> Result<WriteOutcome, TransportError> {
        let result = self.call(
            "write",
            json!({"monitor": monitor, "control": control, "value": value}),
        )?;
        Ok(WriteOutcome {
            native_target: value,
            observed: result.get("observed").and_then(Value::as_u64).unwrap_or(u64::MAX),
            backend: result
                .get("write_backend")
                .and_then(Value::as_str)
                .unwrap_or("unknown")
                .to_string(),
        })
    }
}

/// Deterministic transport for tests: no hardware, no processes.
#[derive(Default)]
pub struct FakeTransport {
    monitors: Vec<MonitorSummary>,
    values: HashMap<(String, String), u64>,
    maximum: u64,
    /// Number of operations currently inside the transport; used to prove serialization.
    pub in_flight: u32,
    pub max_in_flight: u32,
    pub writes: Vec<(String, String, u64)>,
    /// Fail the next write with this error.
    pub fail_next_write: Option<TransportError>,
    /// Report a different value than the one written.
    pub observe_offset: i64,
    /// Controls a monitor positively does not offer; reads and writes report `unsupported`.
    pub absent: Vec<(String, String)>,
    /// Writes that fail once with the given error, in registration order.
    pub failing_writes: Vec<(String, String, TransportError)>,
}

impl FakeTransport {
    pub fn with_monitor(id: &str, name: &str) -> FakeTransport {
        FakeTransport {
            monitors: vec![MonitorSummary {
                id: id.to_string(),
                friendly_name: name.to_string(),
                connector: String::from("hdmi"),
                primary: true,
            }],
            maximum: 100,
            ..FakeTransport::default()
        }
    }

    pub fn with_value(mut self, monitor: &str, control: &str, value: u64) -> FakeTransport {
        self.values
            .insert((monitor.to_string(), control.to_string()), value);
        self
    }

    /// Add a second or third monitor with a deterministic identity.
    pub fn add_monitor(mut self, id: &str, name: &str) -> FakeTransport {
        self.monitors.push(MonitorSummary {
            id: id.to_string(),
            friendly_name: name.to_string(),
            connector: String::from("hdmi"),
            primary: false,
        });
        self
    }

    /// Record that a monitor positively does not offer a control.
    pub fn without_control(mut self, monitor: &str, control: &str) -> FakeTransport {
        self.absent
            .push((monitor.to_string(), control.to_string()));
        self
    }

    /// Fail the first write of this control on this monitor with the given error.
    pub fn fail_write(
        mut self,
        monitor: &str,
        control: &str,
        error: TransportError,
    ) -> FakeTransport {
        self.failing_writes
            .push((monitor.to_string(), control.to_string(), error));
        self
    }

    fn enter(&mut self) {
        self.in_flight += 1;
        self.max_in_flight = self.max_in_flight.max(self.in_flight);
    }

    fn leave(&mut self) {
        self.in_flight = self.in_flight.saturating_sub(1);
    }
}

impl Transport for FakeTransport {
    fn monitors(&mut self) -> Result<Vec<MonitorSummary>, TransportError> {
        self.enter();
        let result = self.monitors.clone();
        self.leave();
        Ok(result)
    }

    fn peak_concurrency(&self) -> Option<u32> {
        Some(self.max_in_flight)
    }

    fn read(&mut self, monitor: &str, control: &str) -> Result<ControlReading, TransportError> {
        self.enter();
        if !self.monitors.iter().any(|entry| entry.id == monitor) {
            self.leave();
            return Err(TransportError::UnknownMonitor(format!(
                "monitor {monitor} is not present"
            )));
        }
        if self
            .absent
            .iter()
            .any(|(entry, field)| entry == monitor && field == control)
        {
            self.leave();
            return Err(TransportError::Unsupported(format!(
                "{control} is not offered by {monitor}"
            )));
        }
        let current = self
            .values
            .get(&(monitor.to_string(), control.to_string()))
            .copied()
            .unwrap_or(0);
        let result = ControlReading {
            minimum: Some(0),
            current: (current as i64 + self.observe_offset).max(0) as u64,
            maximum: Some(self.maximum),
            backend: String::from("fake"),
        };
        self.leave();
        Ok(result)
    }

    fn write(
        &mut self,
        monitor: &str,
        control: &str,
        value: u64,
    ) -> Result<WriteOutcome, TransportError> {
        self.enter();
        if let Some(error) = self.fail_next_write.take() {
            self.leave();
            return Err(error);
        }
        if let Some(index) = self
            .failing_writes
            .iter()
            .position(|(entry, field, _)| entry == monitor && field == control)
        {
            let (_, _, error) = self.failing_writes.remove(index);
            self.leave();
            return Err(error);
        }
        self.writes
            .push((monitor.to_string(), control.to_string(), value));
        self.values
            .insert((monitor.to_string(), control.to_string()), value);
        let observed = (value as i64 + self.observe_offset).max(0) as u64;
        let result = WriteOutcome {
            native_target: value,
            observed,
            backend: String::from("fake"),
        };
        self.leave();
        Ok(result)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fake_transport_validates_monitor_identity() {
        let mut transport = FakeTransport::with_monitor("mon-a", "Panel");
        assert!(transport.read("mon-a", "brightness").is_ok());
        assert_eq!(
            transport.read("mon-b", "brightness").unwrap_err().kind(),
            "unknown_monitor"
        );
    }

    #[test]
    fn fake_transport_reports_a_mismatch_when_the_device_drifts() {
        let mut transport = FakeTransport::with_monitor("mon-a", "Panel");
        transport.observe_offset = 1;
        let outcome = transport.write("mon-a", "brightness", 40).expect("write");
        assert_eq!(outcome.observed, 41);
        assert!(!outcome.verified());
    }
}
