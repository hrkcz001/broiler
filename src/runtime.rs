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
use crate::win::ui::Answer;
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

/// Whether startup may show a window to resolve an outcome.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Ask {
    /// Never open a window: stop with the message instead. This is what an unattended command and a
    /// test need, and it keeps a decision path free of UI.
    Never,
    /// Ask the user when the outcome needs an answer.
    WhenNeeded,
}

/// Decide what this launch should do and, for a one-shot outcome, apply it exactly once.
///
/// Hardware is only touched by the `ApplyOnce` branch, and that branch writes solely through the
/// tested planner inside the coordinator.
pub fn startup_step<C: crate::transport::Transport>(
    coordinator: &mut Coordinator<C>,
    storage: &StorageContext,
    ask: Ask,
) -> StartupStep {
    // Two outcomes are resolvable by asking, not by giving up: a required selection and a directory
    // with no profiles. Both can legitimately come back unchanged, so the loop is bounded instead of
    // trusting the user to make progress.
    for attempt in 0..=MAX_ASK_ROUNDS {
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

        if let StartupStep::ApplyOnce { profile, targets } = &step {
            log_info!(
                "applying profile {profile} to {} target(s) and exiting",
                targets.len()
            );
            let result = coordinator.handle(
                "profile.apply",
                &serde_json::json!({"id": profile, "monitor_ids": targets}),
            );
            return match result {
                Ok(result) => StartupStep::Exit(crate::cli::report_apply(&result, false)),
                Err(error) => {
                    log_error!("profile {} was not applied: {}", profile, error.message);
                    output::error_line(&format!("{}: {}", PRODUCT_NAME, error.message));
                    StartupStep::Exit(6)
                }
            };
        }

        if matches!(step, StartupStep::Serve) {
            return step;
        }

        let code = match step {
            StartupStep::Exit(code) => code,
            _ => unreachable!("handled above"),
        };
        if attempt == MAX_ASK_ROUNDS || ask == Ask::Never {
            report_stop(&outcome, storage);
            return StartupStep::Exit(code);
        }

        match &outcome {
            StartupOutcome::ShowSelection { remember, .. } => {
                match ask_for_selection(coordinator, *remember) {
                    // Cancel changes nothing, and a startup that has no selection simply exits.
                    Some(Answer::Cancelled) | None => return StartupStep::Exit(0),
                    Some(Answer::Confirmed { ids, remember }) => {
                        if let Err(error) = coordinator.handle(
                            "monitors.select",
                            &serde_json::json!({"monitor_ids": ids, "remember": remember}),
                        ) {
                            log_error!("the selection was not recorded: {}", error.message);
                            output::error_line(&format!("{}: {}", PRODUCT_NAME, error.message));
                            return StartupStep::Exit(6);
                        }
                    }
                }
            }
            StartupOutcome::SetupWindow => match ask_for_profiles(storage) {
                None | Some(crate::win::ui::SetupAnswer::Exit) => {
                    report_stop(&outcome, storage);
                    return StartupStep::Exit(code);
                }
                Some(crate::win::ui::SetupAnswer::OpenDirectory) => {
                    if let Err(reason) = crate::win::ui::open_directory(&storage.profiles_dir()) {
                        log_error!("{reason}");
                    }
                }
                Some(crate::win::ui::SetupAnswer::Reload) => {
                    if let Err(error) = coordinator.handle("reload", &serde_json::Value::Null) {
                        log_error!("profiles were not reloaded: {}", error.message);
                    }
                }
            },
            _ => {
                report_stop(&outcome, storage);
                return StartupStep::Exit(code);
            }
        }
    }
    // The loop can only leave through one of the returns above.
    StartupStep::Exit(0)
}

/// How many times startup asks instead of stopping, so a persistent answer cannot loop forever.
const MAX_ASK_ROUNDS: usize = 2;

fn report_stop(outcome: &StartupOutcome, storage: &StorageContext) {
    if let Some(message) = startup_message(outcome, storage) {
        log_info!("{message}");
        output::error_line(&format!("{}: {message}", PRODUCT_NAME));
    }
}

/// Show the checkbox dialog and return the answer, or `None` when no window could be shown.
fn ask_for_selection<C: crate::transport::Transport>(
    coordinator: &mut Coordinator<C>,
    remember: bool,
) -> Option<Answer> {
    let monitors = coordinator.monitors().ok()?;
    if monitors.is_empty() {
        return None;
    }
    let entries = crate::win::ui::entries_for(&monitors);
    Some(crate::win::ui::select_monitors(
        "broiler: select monitors",
        "Choose the monitors broiler controls.",
        &entries,
        remember,
    ))
}

/// Show the zero-profile setup window, or `None` when no window could be shown.
fn ask_for_profiles(storage: &StorageContext) -> Option<crate::win::ui::SetupAnswer> {
    let message = format!(
        "There is no profile in {}.\n\nAdd a profile file there, then choose Reload. {} never \
         creates a profile for you.",
        storage.profiles_dir().display(),
        PRODUCT_NAME
    );
    Some(crate::win::ui::setup_window("broiler: no profiles", &message))
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
    startup_step(&mut coordinator, storage, Ask::WhenNeeded)
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
    if let StartupStep::Exit(code) = startup_step(&mut coordinator_state, &storage, Ask::WhenNeeded) {
        log_info!("startup finished with exit code {code}");
        return code;
    }

    let coordinator = Arc::new(Mutex::new(coordinator_state));

    log_info!(
        "coordinator listening on {} (protocol {})",
        endpoint.name,
        PROTOCOL_VERSION
    );

    let targets = resident_targets(&coordinator);
    let specs: Vec<crate::tray::IconSpec> = targets
        .iter()
        .enumerate()
        .map(|(position, monitor)| crate::tray::IconSpec {
            monitor: monitor.clone(),
            // A persisted profile id proves nothing about the current hardware state, so the icon
            // starts neutral.
            glyph: crate::tray::Glyph::Monitor,
            index: position + 1,
            badges: true,
        })
        .collect();

    let host = Box::new(TrayHost {
        coordinator: Arc::clone(&coordinator),
        storage: storage.clone(),
        targets: targets.clone(),
        left_click: left_click_behaviour(&coordinator),
        tray_window: std::sync::atomic::AtomicIsize::new(0),
        last_failure: Arc::new(Mutex::new(None)),
        applying: Arc::new(std::sync::atomic::AtomicBool::new(false)),
    });
    let user = Box::into_raw(host) as *mut std::ffi::c_void;

    let tray = crate::tray::Tray::install(
        &specs,
        crate::tray::Callbacks {
            user,
            tooltip: tray_tooltip,
            activate: tray_activate,
            settings: tray_settings,
            exit: tray_exit,
            flyout: tray_flyout_content,
            choose: tray_choose,
            glyph: tray_glyph,
            failure: tray_failure,
            settings_snapshot: tray_settings_snapshot,
            settings_apply: tray_settings_apply,
        },
    );

    let Some(mut tray) = tray else {
        // Without a notification area the resident process would be invisible; serve anyway, because
        // the pipe still works, and say so instead of pretending there is an icon.
        log_info!("no notification area: serving without tray icons");
        spawn_stop_watchdog(Arc::clone(&coordinator), storage.clone(), None);
        let result = Daemon::new(endpoint, Arc::clone(&coordinator)).run();
        unsafe { drop(Box::from_raw(user as *mut TrayHost)) };
        return report_daemon(result);
    };

    // A `stop` from the command line must end this process, not just its coordinator. The watcher
    // polls the shutdown flag, ends this message loop, and releases the daemon's blocked accept.
    let quit_window = tray.window_id();
    unsafe {
        if let Some(host) = (user as *mut TrayHost).as_ref() {
            host.tray_window
                .store(quit_window, std::sync::atomic::Ordering::Relaxed);
        }
    }
    if let Ok(mut guard) = coordinator.lock() {
        guard.set_ui_window(quit_window);
    }
    spawn_stop_watchdog(Arc::clone(&coordinator), storage.clone(), Some(quit_window));
    let daemon_coordinator = Arc::clone(&coordinator);
    let daemon = std::thread::spawn(move || Daemon::new(endpoint, daemon_coordinator).run());
    tray.run();
    log_info!("tray message loop finished");

    // The daemon's accept loop is blocked; one request on the pipe wakes it so it can notice the
    // shutdown flag and return.
    let _ = stop_resident(&storage);
    let result = daemon.join().unwrap_or_else(|_| Ok(0));
    drop(tray);
    unsafe { drop(Box::from_raw(user as *mut TrayHost)) };
    report_daemon(result)
}

fn report_daemon(result: std::io::Result<u64>) -> i32 {
    match result {
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

/// End the process once a shutdown has been requested.
///
/// The accept loop can be blocked in its next `ConnectNamedPipe` when the shutdown flag is set by a
/// handler thread, so a flag check alone is not enough: the watcher polls the flag, then releases the
/// blocked accept with one real request and, when a tray exists, ends its message loop. Without this
/// `broiler stop` returns success while the process stays alive.
fn spawn_stop_watchdog(
    coordinator: Arc<Mutex<Coordinator<crate::transport::WorkerTransport>>>,
    storage: StorageContext,
    quit_window: Option<isize>,
) {
    std::thread::spawn(move || loop {
        std::thread::sleep(Duration::from_millis(50));
        let stopping = coordinator
            .lock()
            .map(|guard| guard.shutdown_requested())
            .unwrap_or(true);
        if !stopping {
            continue;
        }
        if let Some(window) = quit_window {
            unsafe {
                windows_sys::Win32::UI::WindowsAndMessaging::PostMessageW(
                    window as windows_sys::Win32::Foundation::HWND,
                    0x0010,
                    0,
                    0,
                );
            }
        }
        // Release the accept loop; the request itself does nothing.
        let _ = request(&storage, "ping", serde_json::Value::Null);
        return;
    });
}

/// Everything the tray callbacks need. Reached through the `user` pointer, always on the UI thread.
struct TrayHost {
    coordinator: Arc<Mutex<Coordinator<crate::transport::WorkerTransport>>>,
    storage: StorageContext,
    targets: Vec<String>,
    left_click: crate::config::settings::LeftClick,
    /// The tray owner window, so the coordinator thread can ask the UI thread for a flyout.
    tray_window: std::sync::atomic::AtomicIsize,
    /// Why the most recent apply failed, read by the row that reports it.
    last_failure: Arc<Mutex<Option<String>>>,
    /// The monitor a profile is being applied to, so a second click cannot start a second writer.
    applying: Arc<std::sync::atomic::AtomicBool>,
}

/// Which monitors a resident instance controls.
///
/// The list lives on the coordinator so the pipe answers `ui.flyout` with the same slot indices the
/// tray icons use; two implementations would drift.
fn resident_targets(
    coordinator: &Arc<Mutex<Coordinator<crate::transport::WorkerTransport>>>,
) -> Vec<String> {
    match coordinator.lock() {
        Ok(mut guard) => guard.resident_targets(),
        Err(_) => Vec::new(),
    }
}

fn left_click_behaviour(
    coordinator: &Arc<Mutex<Coordinator<crate::transport::WorkerTransport>>>,
) -> crate::config::settings::LeftClick {
    coordinator
        .lock()
        .map(|guard| guard.settings().left_click)
        .unwrap_or(crate::config::settings::LeftClick::Cycle)
}

unsafe fn host<'a>(user: *mut std::ffi::c_void) -> Option<&'a mut TrayHost> {
    (user as *mut TrayHost).as_mut()
}

fn tray_tooltip(user: *mut std::ffi::c_void, monitor: usize) -> String {
    let Some(host) = (unsafe { host(user) }) else {
        return String::from(PRODUCT_NAME);
    };
    let total = host.targets.len();
    let Some(id) = host.targets.get(monitor).cloned() else {
        return String::from(PRODUCT_NAME);
    };
    let (name, brightness, state) = match host.coordinator.lock() {
        Ok(mut guard) => {
            let name = guard
                .monitors()
                .unwrap_or_default()
                .into_iter()
                .find(|seen| seen.id == id)
                .map(|seen| seen.friendly_name)
                .unwrap_or_else(|| id.clone());
            // A read here is bounded and read-only; an unknown value is simply not shown.
            let brightness = guard
                .handle(
                    "parameters.get",
                    &serde_json::json!({"monitor": id, "control": "brightness"}),
                )
                .ok()
                .and_then(|result| {
                    result
                        .get("reading")
                        .and_then(|reading| reading.get("current"))
                        .and_then(serde_json::Value::as_u64)
                })
                .map(|value| value as u32);
            let state = match guard.applied_profile(&id) {
                Some(profile) => profile.to_string(),
                None => String::from("Custom"),
            };
            (name, brightness, state)
        }
        Err(_) => (id.clone(), None, String::from("Custom")),
    };
    crate::tray::tooltip(monitor + 1, total, &name, &state, brightness)
}

fn tray_activate(user: *mut std::ffi::c_void, monitor: usize, _activation: crate::tray::Activation) {
    let Some(host) = (unsafe { host(user) }) else {
        return;
    };
    let Some(id) = host.targets.get(monitor).cloned() else {
        return;
    };
    match host.left_click {
        crate::config::settings::LeftClick::Cycle => {
            let result = host.coordinator.lock().ok().map(|mut guard| {
                guard.handle("profile.next", &serde_json::json!({"monitor": id}))
            });
            match result {
                Some(Ok(_)) => log_info!("cycled the profile of {id}"),
                Some(Err(error)) => log_error!("the profile of {id} was not changed: {}", error.message),
                None => log_error!("the coordinator lock was poisoned"),
            }
        }
        crate::config::settings::LeftClick::Flyout => {
            // The UI thread owns the window; this callback runs on it, so the message is queued and
            // handled by the tray procedure with the rest of the window work.
            let window = host
                .tray_window
                .load(std::sync::atomic::Ordering::Relaxed);
            if window != 0 {
                unsafe {
                    windows_sys::Win32::UI::WindowsAndMessaging::PostMessageW(
                        window as windows_sys::Win32::Foundation::HWND,
                        crate::win::flyout::OPEN,
                        monitor,
                        0,
                    );
                }
            } else {
                log_error!("the flyout was requested before the tray window existed");
                output::error_line(&format!(
                    "{}: the profile flyout is not available yet",
                    PRODUCT_NAME
                ));
            }
        }
    }
}

/// The rows the flyout shows: every profile in registry order, with the verified one marked.
///
/// The current marker comes from `applied_profile`, which only reports what this process verified;
/// an edited file or a monitor that was never touched shows no marker rather than a guess.
fn tray_flyout_content(
    user: *mut std::ffi::c_void,
    slot: usize,
) -> crate::win::flyout::Content {
    use crate::win::flyout::{Content, Item, State};

    let Some(host) = (unsafe { host(user) }) else {
        return Content {
            title: crate::win::flyout::title_text(slot + 1, PRODUCT_NAME),
            items: Vec::new(),
        };
    };
    let Some(id) = host.targets.get(slot).cloned() else {
        return Content {
            title: crate::win::flyout::title_text(slot + 1, PRODUCT_NAME),
            items: Vec::new(),
        };
    };
    let Ok(mut guard) = host.coordinator.lock() else {
        return Content {
            title: crate::win::flyout::title_text(slot + 1, &id),
            items: Vec::new(),
        };
    };
    let name = guard
        .monitors()
        .unwrap_or_default()
        .into_iter()
        .find(|seen| seen.id == id)
        .map(|seen| seen.friendly_name)
        .unwrap_or_else(|| id.clone());
    let current = guard.applied_profile(&id).map(str::to_string);
    let listed = guard
        .handle("profiles.list", &serde_json::Value::Null)
        .ok()
        .and_then(|result| result.get("profiles").cloned())
        .and_then(|value| value.as_array().cloned())
        .unwrap_or_default();
    let items = listed
        .iter()
        .filter_map(|profile| {
            let profile_id = profile.get("id")?.as_str()?.to_string();
            let label = profile
                .get("name")
                .and_then(serde_json::Value::as_str)
                .unwrap_or(&profile_id)
                .to_string();
            let state = if current.as_deref() == Some(profile_id.as_str()) {
                State::Current
            } else {
                State::Normal
            };
            Some(Item {
                id: profile_id,
                name: label,
                state,
            })
        })
        .collect();
    Content {
        title: crate::win::flyout::title_text(slot + 1, &name),
        items,
    }
}

/// Apply the profile the user picked, off the UI thread, and report the outcome back to the tray.
///
/// The UI thread must never wait for a DDC transaction, so the work happens here and the answer
/// arrives as a posted message. A second click while one apply is in flight is refused with a
/// reason, because two writers to the same monitor is exactly what the product forbids.
fn tray_choose(user: *mut std::ffi::c_void, slot: usize, row: usize) {
    let Some(host) = (unsafe { host(user) }) else {
        return;
    };
    let Some(monitor) = host.targets.get(slot).cloned() else {
        return;
    };
    let window = host
        .tray_window
        .load(std::sync::atomic::Ordering::Relaxed);
    let profile = match host.coordinator.lock() {
        Ok(mut guard) => guard
            .handle("profiles.list", &serde_json::Value::Null)
            .ok()
            .and_then(|result| result.get("profiles").cloned())
            .and_then(|value| value.as_array().cloned())
            .unwrap_or_default()
            .get(row)
            .and_then(|profile| profile.get("id"))
            .and_then(serde_json::Value::as_str)
            .map(str::to_string),
        Err(_) => None,
    };
    let Some(profile) = profile else {
        return;
    };

    if host
        .applying
        .swap(true, std::sync::atomic::Ordering::SeqCst)
    {
        log_error!("a profile is already being applied; the click was ignored");
        if let Ok(mut guard) = host.last_failure.lock() {
            *guard = Some(String::from("another profile is being applied"));
        }
        if window != 0 {
            unsafe {
                windows_sys::Win32::UI::WindowsAndMessaging::PostMessageW(
                    window as windows_sys::Win32::Foundation::HWND,
                    crate::win::flyout::RESULT,
                    row,
                    1,
                );
            }
        }
        return;
    }

    let coordinator = Arc::clone(&host.coordinator);
    let failure = Arc::clone(&host.last_failure);
    let applying = Arc::clone(&host.applying);
    std::thread::spawn(move || {
        let outcome = match coordinator.lock() {
            Ok(mut guard) => guard.handle(
                "profile.apply",
                &serde_json::json!({"id": profile, "monitor": monitor}),
            ),
            Err(_) => Err(crate::ipc::Failure::new(
                "internal",
                "the coordinator lock was poisoned",
            )),
        };
        let applied = match &outcome {
            Ok(result) => {
                let status = result
                    .get("status")
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or("failed");
                status == "applied"
            }
            Err(_) => false,
        };
        match &outcome {
            Ok(result) if applied => {
                // A hardware write is only acceptable with a stated reason and recorded values, so
                // the log carries the requested value, the value written and what the device reported
                // for every control in the profile.
                log_info!("{monitor}: {profile} applied from the flyout");
                for field in applied_fields(result) {
                    log_info!("{monitor}: {field}");
                }
                if let Ok(mut guard) = failure.lock() {
                    *guard = None;
                }
            }
            Ok(result) => {
                let status = result
                    .get("status")
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or("failed");
                log_error!("{monitor}: {profile} was not applied ({status})");
                if let Ok(mut guard) = failure.lock() {
                    *guard = Some(format!("{status}"));
                }
            }
            Err(error) => {
                log_error!("{monitor}: {profile} failed: {}", error.message);
                if let Ok(mut guard) = failure.lock() {
                    *guard = Some(error.message.clone());
                }
            }
        }
        applying.store(false, std::sync::atomic::Ordering::SeqCst);
        if window != 0 {
            unsafe {
                windows_sys::Win32::UI::WindowsAndMessaging::PostMessageW(
                    window as windows_sys::Win32::Foundation::HWND,
                    crate::win::flyout::RESULT,
                    row,
                    if applied { 0 } else { 1 },
                );
            }
        }
    });
}

/// One line per applied control: what was requested, what was written, what the device reports.
fn applied_fields(result: &serde_json::Value) -> Vec<String> {
    let mut lines = Vec::new();
    let Some(monitors) = result.get("monitors").and_then(serde_json::Value::as_array) else {
        return lines;
    };
    for monitor in monitors {
        let Some(fields) = monitor.get("fields").and_then(serde_json::Value::as_array) else {
            continue;
        };
        for field in fields {
            if field.get("status").and_then(serde_json::Value::as_str) != Some("applied") {
                continue;
            }
            lines.push(format!(
                "{}: {} -> {} (device reports {})",
                field
                    .get("control")
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or("?"),
                field
                    .get("requested")
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or("?"),
                field
                    .get("native")
                    .map(|value| value.to_string())
                    .unwrap_or_else(|| String::from("?")),
                field
                    .get("observed")
                    .map(|value| value.to_string())
                    .unwrap_or_else(|| String::from("?")),
            ));
        }
    }
    lines
}

/// The icon name of the profile the coordinator verified for this monitor, if any.
fn tray_glyph(user: *mut std::ffi::c_void, slot: usize) -> Option<String> {
    let host = unsafe { host(user) }?;
    let id = host.targets.get(slot)?.clone();
    let mut guard = host.coordinator.lock().ok()?;
    let profile_id = guard.applied_profile(&id)?.to_string();
    let listed = guard
        .handle("profiles.list", &serde_json::Value::Null)
        .ok()?;
    let profiles = listed.get("profiles")?.as_array()?;
    let icon = profiles
        .iter()
        .find(|profile| profile.get("id").and_then(serde_json::Value::as_str) == Some(&profile_id))?
        .get("icon")
        .and_then(serde_json::Value::as_str)
        .unwrap_or(&profile_id)
        .to_string();
    Some(icon)
}

/// The reason a row reports as failed.
fn tray_failure(user: *mut std::ffi::c_void) -> Option<String> {
    let host = unsafe { host(user) }?;
    host.last_failure.lock().ok().and_then(|guard| guard.clone())
}

/// What the settings window shows: what the coordinator holds now, not what a file said at startup.
fn tray_settings_snapshot(user: *mut std::ffi::c_void) -> crate::win::settings::Snapshot {
    let fallback = crate::win::settings::Snapshot {
        left_click: crate::config::settings::LeftClick::Cycle,
        monitors: 0,
        profiles: 0,
        start_at_logon: false,
    };
    let Some(host) = (unsafe { host(user) }) else {
        return fallback;
    };
    let Ok(mut guard) = host.coordinator.lock() else {
        return fallback;
    };
    let monitors = guard.resident_targets().len();
    let profiles = guard
        .handle("profiles.list", &serde_json::Value::Null)
        .ok()
        .and_then(|result| result.get("profiles").cloned())
        .and_then(|value| value.as_array().map(Vec::len))
        .unwrap_or(0);
    crate::win::settings::Snapshot {
        left_click: guard.settings().left_click,
        monitors,
        profiles,
        start_at_logon: false,
    }
}

/// Perform one action from the settings window. The return value says whether it took effect.
fn tray_settings_apply(
    user: *mut std::ffi::c_void,
    action: crate::win::settings::Action,
) -> bool {
    use crate::win::settings::Action;
    let Some(host) = (unsafe { host(user) }) else {
        return false;
    };
    match action {
        Action::LeftClick(value) => {
            let text = match value {
                crate::config::settings::LeftClick::Cycle => "cycle",
                crate::config::settings::LeftClick::Flyout => "flyout",
            };
            let result = match host.coordinator.lock() {
                Ok(mut guard) => guard.handle(
                    "settings.set",
                    &serde_json::json!({"key": "tray.left_click", "value": text}),
                ),
                Err(_) => return false,
            };
            match result {
                Ok(_) => {
                    log_info!("tray.left_click is now {text}");
                    true
                }
                Err(error) => {
                    log_error!("tray.left_click was not saved: {}", error.message);
                    false
                }
            }
        }
        Action::SelectMonitors => {
            // The dialog is modal and owns its own loop; it belongs on the UI thread, which is where
            // this callback runs.
            let entries = match host.coordinator.lock() {
                Ok(mut guard) => crate::win::ui::entries_for(&guard.monitors().unwrap_or_default()),
                Err(_) => return false,
            };
            let answer = crate::win::ui::select_monitors(
                "broiler: select monitors",
                "Control these monitors. A display that cannot be controlled is listed and disabled.",
                &entries,
                false,
            );
            if let crate::win::ui::Answer::Confirmed { ids, remember } = answer {
                let written = match host.coordinator.lock() {
                    Ok(mut guard) => guard.handle(
                        "monitors.select",
                        &serde_json::json!({"monitor_ids": ids, "remember": remember}),
                    ),
                    Err(_) => return false,
                };
                if let Err(error) = written {
                    log_error!("the selection was not saved: {}", error.message);
                    return false;
                }
                if host.tray_window.load(std::sync::atomic::Ordering::Relaxed) != 0 {
                    unsafe {
                        windows_sys::Win32::UI::WindowsAndMessaging::PostMessageW(
                            host.tray_window.load(std::sync::atomic::Ordering::Relaxed)
                                as windows_sys::Win32::Foundation::HWND,
                            crate::tray::SETTINGS_REFRESH,
                            0,
                            0,
                        );
                    }
                }
                true
            } else {
                // Cancelling changes nothing and is not a failure of the window.
                true
            }
        }
        Action::OpenProfiles => crate::win::ui::open_directory(&host.storage.profiles_dir()).is_ok(),
        Action::Close => true,
    }
}

fn tray_settings(user: *mut std::ffi::c_void) {
    let Some(host) = (unsafe { host(user) }) else {
        return;
    };
    let window = host.tray_window.load(std::sync::atomic::Ordering::Relaxed);
    if window == 0 {
        log_error!("the settings window was requested before the tray window existed");
        return;
    }
    unsafe {
        windows_sys::Win32::UI::WindowsAndMessaging::PostMessageW(
            window as windows_sys::Win32::Foundation::HWND,
            crate::tray::SETTINGS_OPEN,
            0,
            0,
        );
    }
}

fn tray_exit(user: *mut std::ffi::c_void) {
    let Some(host) = (unsafe { host(user) }) else {
        return;
    };
    // Exit stops the whole instance: the coordinator flag ends the daemon, and the quit message ends
    // this message loop. Both, or the process would outlive its icons.
    if let Ok(mut guard) = host.coordinator.lock() {
        let _ = guard.handle("shutdown", &serde_json::Value::Null);
    }
    unsafe { windows_sys::Win32::UI::WindowsAndMessaging::PostQuitMessage(0) };
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
        let step = startup_step(&mut coordinator, &storage, Ask::Never);
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
