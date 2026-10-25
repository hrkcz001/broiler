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
use crate::lifecycle::StartupOutcome;
use crate::ipc::{Failure, Request, Response, PROTOCOL_VERSION};
use crate::pipe::{self, Client, Endpoint};
use crate::transport::Transport;
use crate::storage::{StorageContext, StorageMode};
use crate::{log_error, log_info, output, PRODUCT_NAME};

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
///
/// The request names the data root this process resolved. A coordinator that owns another root
/// refuses it, so a second writer is never created and no answer is given about data the caller
/// did not read.
pub fn request(
    storage: &StorageContext,
    method: &str,
    params: serde_json::Value,
) -> Result<serde_json::Value, ClientError> {
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
                storage_root: Some(storage.root().display().to_string()),
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
pub fn is_running(storage: &StorageContext) -> bool {
    matches!(request(storage, "ping", serde_json::Value::Null), Ok(_))
}

/// Actionable message for a portable first launch that still has to decide about older AppData
/// configuration. `None` means there is nothing to decide.
pub fn pending_import_message(storage: &StorageContext) -> Option<String> {
    let roots = crate::storage::appdata_roots()?;
    if !crate::migrate::pending_import(storage, &roots) {
        return None;
    }
    let source = crate::storage::appdata_of(&roots);
    Some(format!(
        "an older configuration exists in {} and {} is empty. Run `{} data migrate --from-appdata \
         --dry-run` to inspect it, `{} data migrate --from-appdata` to copy it, or `{} data migrate \
         --from-appdata --decline` to keep using only the new root",
        source.root().display(),
        storage.root().display(),
        PRODUCT_NAME,
        PRODUCT_NAME,
        PRODUCT_NAME
    ))
}

/// What normal launch does after the startup decision.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum StartupStep {
    /// Apply the one valid profile to these targets once, then exit; no tray, no residence.
    ApplyOnce {
        profile: String,
        targets: Vec<String>,
    },
    /// Report the outcome and exit with this code without touching hardware.
    Exit(i32),
    /// Stay resident and serve the pipe.
    Serve,
}

/// Map a startup outcome to an action. Pure, so every branch is testable without a monitor.
pub fn plan_startup(outcome: &StartupOutcome) -> StartupStep {
    match outcome {
        // Nothing to control: no dialog, no tray, no error.
        StartupOutcome::ExitNoMonitors => StartupStep::Exit(0),
        // A confirmed or cancelled empty selection is a decision, not a failure.
        StartupOutcome::ExitEmptySelection => StartupStep::Exit(0),
        StartupOutcome::ExitInvalidProfiles { .. } => StartupStep::Exit(2),
        // This build has no checkbox dialog. Guessing a target would write to a monitor the user
        // never chose, so the launch stops and says which command records the choice.
        StartupOutcome::ShowSelection { .. } => StartupStep::Exit(2),
        StartupOutcome::SetupWindow => StartupStep::Exit(2),
        StartupOutcome::OneShot { targets, profile } => StartupStep::ApplyOnce {
            profile: profile.clone(),
            targets: targets.clone(),
        },
        StartupOutcome::Resident { .. } => StartupStep::Serve,
    }
}

/// The message a launch prints for an outcome that stops it. `None` means exit silently.
pub fn startup_message(
    outcome: &StartupOutcome,
    storage: &StorageContext,
) -> Option<String> {
    match outcome {
        StartupOutcome::ExitNoMonitors => None,
        StartupOutcome::ExitEmptySelection => None,
        StartupOutcome::ExitInvalidProfiles { diagnostics } => Some(format!(
            "{diagnostics} profile file(s) failed validation, so nothing was applied; run `{} \
             profiles validate`",
            PRODUCT_NAME
        )),
        StartupOutcome::ShowSelection { reason, .. } => Some(format!(
            "a monitor selection is required ({}) and this build has no selection dialog. Record one \
             with `{} monitors select --monitor <id>` or `{} monitors select --none`; nothing was \
             changed",
            reason.as_str(),
            PRODUCT_NAME,
            PRODUCT_NAME
        )),
        StartupOutcome::SetupWindow => Some(format!(
            "there is no profile in {}, so there is nothing to apply. Put a profile there or run \
             `{} profiles open`",
            storage.profiles_dir().display(),
            PRODUCT_NAME
        )),
        StartupOutcome::OneShot { .. } | StartupOutcome::Resident { .. } => None,
    }
}

/// Decide what this launch should do and, for a one-shot outcome, apply it exactly once.
///
/// Hardware is only touched by the `ApplyOnce` branch, and that branch writes solely through the
/// tested planner inside the coordinator.
pub fn startup_step<C: crate::transport::Transport>(
    coordinator: &mut Coordinator<C>,
    storage: &StorageContext,
) -> StartupStep {
    let outcome = match coordinator.startup_outcome() {
        Ok(outcome) => outcome,
        Err(error) => {
            log_error!("startup decision failed: {}", error.message);
            output::error_line(&format!("{}: {}", PRODUCT_NAME, error.message));
            return StartupStep::Exit(6);
        }
    };
    log_info!("startup outcome: {}", outcome.as_str());

    let step = plan_startup(&outcome);
    if let Some(message) = startup_message(&outcome, storage) {
        log_info!("{message}");
        output::error_line(&format!("{}: {message}", PRODUCT_NAME));
    }
    match &step {
        StartupStep::ApplyOnce { profile, targets } => {
            log_info!(
                "applying profile {profile} to {} target(s) and exiting",
                targets.len()
            );
            let result = coordinator.handle(
                "profile.apply",
                &serde_json::json!({"id": profile, "monitor_ids": targets}),
            );
            match result {
                Ok(result) => return StartupStep::Exit(crate::cli::report_apply(&result, false)),
                Err(error) => {
                    log_error!("profile {} was not applied: {}", profile, error.message);
                    output::error_line(&format!("{}: {}", PRODUCT_NAME, error.message));
                    return StartupStep::Exit(6);
                }
            }
        }
        StartupStep::Exit(_) | StartupStep::Serve => {}
    }
    step
}

/// A coordinator over a transient worker, for the decision paths that must work without a resident
/// process. The error carries the exit code the caller should use.
fn transient_coordinator(
    storage: &StorageContext,
) -> Result<Coordinator<TransientTransport>, (i32, String)> {
    let settings = settings::load(&storage.settings_file())
        .map_err(|error| (2, error.render()))?;
    let registry = ProfileRegistry::load_dir(&storage.profiles_dir())
        .map_err(|error| (2, error.render()))?;
    let transport = TransientTransport::new().map_err(|reason| (6, reason))?;
    Ok(
        Coordinator::new(transport, settings, registry, storage.settings_file())
            .with_profiles_dir(storage.profiles_dir())
            .with_storage(storage.clone()),
    )
}

/// Run the startup decision for the CLI `start` command, which is also the logon entry point.
///
/// It builds a coordinator over a transient worker so the decision, and a one-shot application, do
/// not require a resident process. Nothing is spawned here.
pub fn startup_step_for_storage(storage: &StorageContext) -> StartupStep {
    let mut coordinator = match transient_coordinator(storage) {
        Ok(coordinator) => coordinator,
        Err((code, message)) => {
            log_error!("{message}");
            output::error_line(&format!("{}: {message}", PRODUCT_NAME));
            return StartupStep::Exit(code);
        }
    };
    startup_step(&mut coordinator, storage)
}

/// Run the resident coordinator until it is asked to stop. Returns the process exit code.
pub fn run_resident(storage: StorageContext) -> i32 {
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

    // A portable first launch must decide about the older AppData configuration before it starts
    // serving: silently ignoring the user's old profiles is not an option, and silently importing
    // them is worse.
    if let Some(reason) = pending_import_message(&storage) {
        log_error!("{reason}");
        output::error_line(&format!("{}: {reason}", PRODUCT_NAME));
        return 2;
    }

    let settings: Settings = match settings::load(&storage.settings_file()) {
        Ok(settings) => settings,
        Err(error) => {
            log_error!("settings refused: {}", error.render());
            output::error_line(&format!("{}: {}", PRODUCT_NAME, error.render()));
            return 2;
        }
    };

    let registry: ProfileRegistry = match ProfileRegistry::load_dir(&storage.profiles_dir()) {
        Ok(registry) => registry,
        Err(error) => {
            log_error!("profiles refused: {}", error.render());
            output::error_line(&format!("{}: {}", PRODUCT_NAME, error.render()));
            return 2;
        }
    };

    let tray = match tray_executable() {
        Ok(path) => path,
        Err(reason) => {
            output::error_line(&format!("{}: {reason}", PRODUCT_NAME));
            return 6;
        }
    };

    let transport = crate::transport::WorkerTransport::new(tray);
    let coordinator_state = Coordinator::new(
        transport,
        settings,
        registry,
        storage.settings_file(),
    )
    .with_profiles_dir(storage.profiles_dir())
    .with_storage(storage.clone());
    // Normal launch follows the documented lifecycle: no monitor means exit silently, one profile
    // means apply once and exit, and only a real multi-profile setup stays resident. Without this
    // the tray would become resident at logon with nothing to control and never apply anything.
    let mut coordinator_state = coordinator_state;
    if let StartupStep::Exit(code) = startup_step(&mut coordinator_state, &storage) {
        log_info!("startup finished with exit code {code}");
        return code;
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
///
/// An explicit root is handed to the child so it does not resolve one of its own; a portable or
/// AppData installation resolves the same root from the same executable and marker. The started
/// coordinator is then checked against the root this command used.
pub fn start_resident(storage: &StorageContext) -> Result<(), String> {
    let tray = tray_executable()?;
    let mut arguments: Vec<String> = Vec::new();
    if storage.mode() == StorageMode::Explicit {
        arguments.push(String::from("--config-dir"));
        arguments.push(storage.root().display().to_string());
    }
    let child = crate::win::process::Child::spawn_detached(&tray, &arguments)
        .map_err(|error| format!("cannot start {}: {error}", tray.display()))?;

    let deadline = Instant::now() + Duration::from_secs(5);
    while Instant::now() < deadline {
        if is_running(storage) {
            return verify_running_root(storage);
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
pub fn stop_resident(storage: &StorageContext) -> Result<bool, ClientError> {
    match request(storage, "shutdown", serde_json::Value::Null) {
        Ok(_) => Ok(true),
        Err(ClientError::NotRunning) => Ok(false),
        Err(other) => Err(other),
    }
}

/// Confirm that the coordinator now answering owns the root this command resolved.
fn verify_running_root(storage: &StorageContext) -> Result<(), String> {
    let ours = storage.root().display().to_string();
    match request(storage, "storage.info", serde_json::Value::Null) {
        Ok(info) => {
            let theirs = info
                .get("storage")
                .and_then(|storage| storage.get("root"))
                .and_then(serde_json::Value::as_str)
                .unwrap_or("");
            if theirs == ours {
                Ok(())
            } else {
                Err(format!(
                    "the coordinator answered from {theirs}, not from {ours}; stop it and retry"
                ))
            }
        }
        // A coordinator too old to report its root is not one this build can vouch for.
        Err(error) => Err(format!(
            "the coordinator did not report its data root: {}",
            error.message()
        )),
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

    /// An isolated root in the scratch directory: no test reads the user's real AppData.
    fn isolated(name: &str) -> (StorageContext, PathBuf) {
        let mut base = std::env::temp_dir();
        base.push(format!("broiler-runtime-{}-{}", std::process::id(), name));
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(&base).expect("scratch");
        (StorageContext::for_test(&base.join("data")), base)
    }

    fn profile_text(id: &str, order: i64) -> String {
        format!(
            "schema_version = 1\nid = \"{id}\"\nname = \"{id}\"\nicon = \"day\"\norder = {order}\n\n[monitor]\nbrightness = 40\n"
        )
    }

    fn registry(ids: &[&str]) -> ProfileRegistry {
        let profiles = ids
            .iter()
            .enumerate()
            .map(|(index, id)| {
                crate::config::profile::parse(
                    &profile_text(id, index as i64 * 10),
                    std::path::Path::new("test.toml"),
                )
                .expect("profile")
            })
            .collect();
        ProfileRegistry::new(profiles).expect("registry")
    }

    /// One launch decision with a fake transport. `name` keeps parallel tests off one directory.
    fn launch(
        name: &str,
        transport: crate::transport::FakeTransport,
        profiles: &[&str],
    ) -> (StartupStep, crate::transport::FakeTransport) {
        let (storage, base) = isolated(name);
        let mut coordinator = Coordinator::new(
            transport,
            Settings::default(),
            registry(profiles),
            storage.settings_file(),
        )
        .with_profiles_dir(storage.profiles_dir())
        .with_storage(storage.clone());
        let step = startup_step(&mut coordinator, &storage);
        let transport = coordinator.into_transport();
        let _ = std::fs::remove_dir_all(base);
        (step, transport)
    }

    #[test]
    fn a_launch_with_no_monitor_exits_silently_and_writes_nothing() {
        let (step, transport) = launch(
            "no-monitor",
            crate::transport::FakeTransport::new(),
            &["day", "night"],
        );
        assert_eq!(step, StartupStep::Exit(0));
        assert!(transport.writes.is_empty());
    }

    #[test]
    fn a_single_profile_is_applied_once_instead_of_becoming_resident() {
        let (step, transport) = launch(
            "one-shot",
            crate::transport::FakeTransport::with_monitor("mon-a", "Panel")
                .with_value("mon-a", "brightness", 10),
            &["day"],
        );
        assert_eq!(step, StartupStep::Exit(0));
        assert_eq!(
            transport.writes,
            vec![(String::from("mon-a"), String::from("brightness"), 40)]
        );
    }

    #[test]
    fn a_single_profile_that_the_monitor_refuses_does_not_report_success() {
        let (step, transport) = launch(
            "refused",
            crate::transport::FakeTransport::with_monitor("mon-a", "Panel")
                .without_control("mon-a", "brightness"),
            &["day"],
        );
        assert_eq!(step, StartupStep::Exit(5));
        assert!(transport.writes.is_empty());
    }

    #[test]
    fn several_profiles_stay_resident_without_writing_anything() {
        let (step, transport) = launch(
            "resident",
            crate::transport::FakeTransport::with_monitor("mon-a", "Panel"),
            &["day", "night"],
        );
        assert_eq!(step, StartupStep::Serve);
        assert!(transport.writes.is_empty());
    }

    #[test]
    fn several_monitors_without_a_remembered_selection_refuse_to_guess() {
        let (step, transport) = launch(
            "two-monitors",
            crate::transport::FakeTransport::with_monitor("mon-a", "Panel")
                .add_monitor("mon-b", "Second"),
            &["day"],
        );
        assert_eq!(step, StartupStep::Exit(2));
        assert!(transport.writes.is_empty());
    }

    #[test]
    fn no_profile_never_invents_one() {
        let (step, transport) = launch(
            "no-profile",
            crate::transport::FakeTransport::with_monitor("mon-a", "Panel"),
            &[],
        );
        assert_eq!(step, StartupStep::Exit(2));
        assert!(transport.writes.is_empty());
    }

    #[test]
    fn every_outcome_maps_to_one_action() {
        use crate::lifecycle::{SelectionReason, StartupOutcome};
        let (storage, base) = isolated("mapping");
        let cases = [
            (StartupOutcome::ExitNoMonitors, StartupStep::Exit(0)),
            (StartupOutcome::ExitEmptySelection, StartupStep::Exit(0)),
            (
                StartupOutcome::ExitInvalidProfiles { diagnostics: 2 },
                StartupStep::Exit(2),
            ),
            (
                StartupOutcome::ShowSelection {
                    reason: SelectionReason::MultipleMonitorsNoChoice,
                    remember: true,
                },
                StartupStep::Exit(2),
            ),
            (StartupOutcome::SetupWindow, StartupStep::Exit(2)),
            (
                StartupOutcome::OneShot {
                    targets: vec![String::from("mon-a")],
                    profile: String::from("day"),
                },
                StartupStep::ApplyOnce {
                    profile: String::from("day"),
                    targets: vec![String::from("mon-a")],
                },
            ),
            (
                StartupOutcome::Resident {
                    targets: vec![String::from("mon-a")],
                    profiles: 2,
                },
                StartupStep::Serve,
            ),
        ];
        for (outcome, expected) in &cases {
            assert_eq!(plan_startup(outcome), *expected, "{outcome:?}");
        }
        // Only outcomes that stop the launch carry a message; the silent ones stay silent.
        assert!(startup_message(&StartupOutcome::ExitNoMonitors, &storage).is_none());
        assert!(startup_message(&StartupOutcome::ExitEmptySelection, &storage).is_none());
        assert!(startup_message(&StartupOutcome::SetupWindow, &storage).is_some());
        let _ = std::fs::remove_dir_all(base);
    }

    #[test]
    fn a_request_without_a_coordinator_reports_not_running() {
        // No daemon is started in the test process; the endpoint simply has no server.
        let (storage, base) = isolated("no-coordinator");
        assert_eq!(
            request(&storage, "ping", serde_json::Value::Null),
            Err(ClientError::NotRunning)
        );
        let _ = std::fs::remove_dir_all(base);
    }

    #[test]
    fn stopping_without_a_coordinator_is_idempotent() {
        let (storage, base) = isolated("stop");
        assert_eq!(stop_resident(&storage), Ok(false));
        let _ = std::fs::remove_dir_all(base);
    }
}
