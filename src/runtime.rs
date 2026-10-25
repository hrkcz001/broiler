//! Process wiring: the resident coordinator and the CLI's connection to it.
//!
//! `broiler-tray.exe` with no arguments becomes the resident coordinator: it takes the per-logon
//! instance lock, loads settings and the profile registry, and serves the endpoint until a shutdown
//! command arrives. The tray icons and windows of later gates attach to this same process.

use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use crate::config::registry::ProfileRegistry;
use crate::config::settings::{self, Settings};
use crate::coordinator::Coordinator;
use crate::daemon::Daemon;
use crate::instance::InstanceLock;
use crate::ipc::{Failure, Request, Response, PROTOCOL_VERSION};
use crate::pipe::{self, Client, Endpoint};
use crate::transport::Transport;
use crate::{log_error, log_info, output, paths, PRODUCT_NAME};

/// Bound for a CLI request to the coordinator.
pub const CLIENT_TIMEOUT: Duration = Duration::from_secs(30);

/// The absolute path of the tray executable that hosts both the coordinator and the DDC worker.
pub fn tray_executable() -> Result<PathBuf, String> {
    let executable = std::env::current_exe()
        .map_err(|error| format!("cannot resolve the current executable: {error}"))?;
    let directory = executable
        .parent()
        .ok_or_else(|| String::from("the current executable has no parent directory"))?;
    let candidate = directory.join("broiler-tray.exe");
    if candidate.exists() {
        return Ok(candidate);
    }
    // When running as broiler-tray.exe itself, the executable is already the right one.
    if executable.file_name().and_then(|name| name.to_str()) == Some("broiler-tray.exe") {
        return Ok(executable);
    }
    Err(format!(
        "the tray executable is missing next to this binary: {}",
        candidate.display()
    ))
}

/// Why a coordinator request did not produce a result.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ClientError {
    /// No coordinator is running for this logon session.
    NotRunning,
    /// The coordinator answered with a typed failure.
    Failure(Failure),
    /// The channel failed or the response was unusable.
    Channel(String),
}

impl ClientError {
    pub fn message(&self) -> String {
        match self {
            ClientError::NotRunning => {
                String::from("no broiler coordinator is running in this session")
            }
            ClientError::Failure(failure) => format!("{}: {}", failure.kind, failure.message),
            ClientError::Channel(reason) => reason.clone(),
        }
    }
}

/// Send one request to the resident coordinator, if there is one.
pub fn request(method: &str, params: serde_json::Value) -> Result<serde_json::Value, ClientError> {
    let endpoint = pipe::local_endpoint().map_err(|error| ClientError::Channel(error.to_string()))?;
    let deadline = Instant::now() + CLIENT_TIMEOUT;
    let client = Client::connect(&endpoint.name, deadline).map_err(|error| {
        if error.kind() == std::io::ErrorKind::TimedOut
            || error.kind() == std::io::ErrorKind::NotFound
        {
            ClientError::NotRunning
        } else {
            ClientError::Channel(error.to_string())
        }
    })?;

    let response: Response = client
        .request(
            &Request {
                id: 1,
                method: method.to_string(),
                params,
            },
            deadline,
        )
        .map_err(|error| {
            // A pipe that closed without an answer means the coordinator went away mid-command.
            if error.kind() == std::io::ErrorKind::UnexpectedEof {
                ClientError::NotRunning
            } else {
                ClientError::Channel(error.to_string())
            }
        })?;

    if response.ok {
        Ok(response.result.unwrap_or(serde_json::Value::Null))
    } else {
        Err(ClientError::Failure(
            response
                .error
                .unwrap_or_else(|| Failure::new("unknown", "the coordinator reported a failure")),
        ))
    }
}

/// True when a coordinator answers a ping.
pub fn is_running() -> bool {
    matches!(request("ping", serde_json::Value::Null), Ok(_))
}

fn settings_path() -> Option<PathBuf> {
    paths::settings_file()
}

fn profiles_dir() -> Option<PathBuf> {
    paths::profiles_dir()
}

/// Run the resident coordinator until it is asked to stop. Returns the process exit code.
pub fn run_resident() -> i32 {
    let endpoint: Endpoint = match pipe::local_endpoint() {
        Ok(endpoint) => endpoint,
        Err(error) => {
            log_error!("cannot build the local endpoint: {error}");
            output::error_line(&format!("{}: {error}", PRODUCT_NAME));
            return 6;
        }
    };

    let (_lock, already_running) = match InstanceLock::acquire(&endpoint) {
        Ok(result) => result,
        Err(error) => {
            log_error!("cannot take the instance lock: {error}");
            output::error_line(&format!("{}: {error}", PRODUCT_NAME));
            return 6;
        }
    };
    if already_running {
        // A second coordinator would be a second writer: report the existing one instead.
        output::error_line(&format!(
            "{}: a coordinator is already running in this session at {}",
            PRODUCT_NAME, endpoint.name
        ));
        log_info!("refused to start a second coordinator");
        return 3;
    }

    let settings: Settings = match settings_path() {
        Some(path) => match settings::load(&path) {
            Ok(settings) => settings,
            Err(error) => {
                log_error!("settings refused: {}", error.render());
                output::error_line(&format!("{}: {}", PRODUCT_NAME, error.render()));
                return 2;
            }
        },
        None => Settings::default(),
    };

    let registry: ProfileRegistry = match profiles_dir() {
        Some(directory) => match ProfileRegistry::load_dir(&directory) {
            Ok(registry) => registry,
            Err(error) => {
                log_error!("profiles refused: {}", error.render());
                output::error_line(&format!("{}: {}", PRODUCT_NAME, error.render()));
                return 2;
            }
        },
        None => ProfileRegistry::default(),
    };

    let tray = match tray_executable() {
        Ok(path) => path,
        Err(reason) => {
            output::error_line(&format!("{}: {reason}", PRODUCT_NAME));
            return 6;
        }
    };

    let transport = crate::transport::WorkerTransport::new(tray);
    let mut coordinator_state = Coordinator::new(
        transport,
        settings,
        registry,
        settings_path().unwrap_or_else(|| PathBuf::from("settings.toml")),
    );
    if let Some(directory) = profiles_dir() {
        coordinator_state = coordinator_state.with_profiles_dir(directory);
    }
    let coordinator = Arc::new(Mutex::new(coordinator_state));

    log_info!(
        "coordinator listening on {} (protocol {})",
        endpoint.name,
        PROTOCOL_VERSION
    );

    let daemon = Daemon::new(endpoint, coordinator);
    match daemon.run() {
        Ok(served) => {
            log_info!("coordinator stopped after {served} connection(s)");
            0
        }
        Err(error) => {
            log_error!("coordinator failed: {error}");
            output::error_line(&format!("{}: {error}", PRODUCT_NAME));
            6
        }
    }
}

/// Start the resident coordinator as a detached process and wait until it answers.
pub fn start_resident() -> Result<(), String> {
    let tray = tray_executable()?;
    let child = crate::win::process::Child::spawn_detached(&tray, &[])
        .map_err(|error| format!("cannot start {}: {error}", tray.display()))?;

    let deadline = Instant::now() + Duration::from_secs(5);
    while Instant::now() < deadline {
        if is_running() {
            return Ok(());
        }
        if !child.still_running() {
            return Err(format!(
                "the coordinator exited immediately with code {:?}; check the log",
                child.exit_code().unwrap_or(1)
            ));
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    Err(String::from(
        "the coordinator did not answer within 5 s after starting",
    ))
}

/// Ask the resident coordinator to stop. Idempotent.
pub fn stop_resident() -> Result<bool, ClientError> {
    match request("shutdown", serde_json::Value::Null) {
        Ok(_) => Ok(true),
        Err(ClientError::NotRunning) => Ok(false),
        Err(other) => Err(other),
    }
}

/// A transport that works without a resident coordinator: one transient worker per command.
pub struct TransientTransport {
    inner: crate::transport::WorkerTransport,
}

impl TransientTransport {
    pub fn new() -> Result<TransientTransport, String> {
        let tray = tray_executable()?;
        Ok(TransientTransport {
            inner: crate::transport::WorkerTransport::new(tray),
        })
    }
}

impl Transport for TransientTransport {
    fn monitors(
        &mut self,
    ) -> Result<Vec<crate::transport::MonitorSummary>, crate::transport::TransportError> {
        self.inner.monitors()
    }

    fn read(
        &mut self,
        monitor: &str,
        control: &str,
    ) -> Result<crate::transport::ControlReading, crate::transport::TransportError> {
        self.inner.read(monitor, control)
    }

    fn write(
        &mut self,
        monitor: &str,
        control: &str,
        value: u64,
    ) -> Result<crate::transport::WriteOutcome, crate::transport::TransportError> {
        self.inner.write(monitor, control, value)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_request_without_a_coordinator_reports_not_running() {
        // No daemon is started in the test process; the endpoint simply has no server.
        assert_eq!(
            request("ping", serde_json::Value::Null),
            Err(ClientError::NotRunning)
        );
    }

    #[test]
    fn stopping_without_a_coordinator_is_idempotent() {
        assert_eq!(stop_resident(), Ok(false));
    }
}
