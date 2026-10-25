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
    request_on(&endpoint.name, storage, method, params)
}

/// Send one request to the coordinator answering on a named endpoint.
///
/// Production callers use [`request`], which resolves the one endpoint of this logon session. An
/// explicit name exists so a test can address its own fixture coordinator and prove that it never
/// reaches, and is never reached by, the owner's instance.
pub fn request_on(
    endpoint_name: &str,
    storage: &StorageContext,
    method: &str,
    params: serde_json::Value,
) -> Result<serde_json::Value, ClientError> {
    let deadline = Instant::now() + CLIENT_TIMEOUT;
    let client = Client::connect(endpoint_name, deadline).map_err(|error| {
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

    // Preserve means read, never apply: one observation of the controls the loaded profiles request,
    // before the first icon is shown, so the tray opens on evidence instead of a neutral guess. This
    // is a bounded read pass through the single worker and writes nothing.
    {
        let observed = match coordinator.lock() {
            Ok(mut guard) => guard.reconcile(&targets),
            Err(_) => Vec::new(),
        };
        log_info!(
            "observation completed for {} of {} monitor(s) before serving",
            observed.len(),
            targets.len()
        );
    }

    let specs: Vec<crate::tray::IconSpec> = targets
        .iter()
        .enumerate()
        .map(|(position, monitor)| crate::tray::IconSpec {
            monitor: monitor.clone(),
            // The icon starts on whatever the observation proved, and neutral when it proved nothing.
            glyph: initial_glyph(&coordinator, monitor),
            index: position + 1,
            badges: true,
        })
        .collect();

    let host = Box::new(TrayHost {
        coordinator: Arc::clone(&coordinator),
        storage: storage.clone(),
        targets: targets.clone(),
        names: resident_names(&coordinator, &targets),
        tray_window: std::sync::atomic::AtomicIsize::new(0),
        last_failure: Arc::new(Mutex::new(None)),
        applying: Arc::new(std::sync::atomic::AtomicBool::new(false)),
        left_click: {
            let stored = coordinator
                .lock()
                .map(|guard| guard.settings().left_click)
                .unwrap_or(crate::config::settings::LeftClick::Cycle);
            let cached = std::sync::atomic::AtomicU8::new(0);
            cache_click_behaviour(&cached, stored);
            Arc::new(cached)
        },
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
    /// The controlled monitors, in the order the tray slots use.
    targets: Vec<String>,
    /// Their display names, read once when the icons were built, so a tooltip refresh costs no
    /// transport call at all.
    names: Vec<String>,
    /// The tray owner window, so the coordinator thread can ask the UI thread for a flyout.
    tray_window: std::sync::atomic::AtomicIsize,
    /// Why the most recent apply failed, read by the row that reports it.
    last_failure: Arc<Mutex<Option<String>>>,
    /// The monitor a profile is being applied to, so a second click cannot start a second writer.
    applying: Arc<std::sync::atomic::AtomicBool>,
    /// The stored left-click behaviour, republished whenever settings are applied.
    ///
    /// The UI thread must never wait for the coordinator's lock: a DDC transaction holds it for the
    /// whole write, and a click that waited would freeze the popup for as long as the monitor takes.
    left_click: Arc<std::sync::atomic::AtomicU8>,
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

/// The icon name of the profile a monitor is currently in, from the shared snapshot.
///
/// `None` when no profile is certified, which is what makes the icon neutral instead of a guess.
fn current_icon<T: crate::transport::Transport>(
    coordinator: &Coordinator<T>,
    monitor: &str,
) -> Option<String> {
    let current = coordinator.current(monitor);
    let id = current.profile_id()?;
    coordinator
        .profile_icon(id)
        .map(str::to_string)
        .or_else(|| Some(id.to_string()))
}

/// The glyph a tray icon starts with: the certified profile's, or the neutral monitor.
fn initial_glyph(
    coordinator: &Arc<Mutex<Coordinator<crate::transport::WorkerTransport>>>,
    monitor: &str,
) -> crate::tray::Glyph {
    let name = coordinator
        .lock()
        .ok()
        .and_then(|guard| current_icon(&guard, monitor));
    name.as_deref()
        .map(crate::tray::Glyph::from_name)
        .unwrap_or(crate::tray::Glyph::Monitor)
}

/// The display name of each controlled monitor, in slot order.
///
/// Captured once, when the icons are built. A surface that needs a monitor name then reads memory
/// instead of the worker.
fn resident_names(
    coordinator: &Arc<Mutex<Coordinator<crate::transport::WorkerTransport>>>,
    targets: &[String],
) -> Vec<String> {
    let monitors = match coordinator.lock() {
        Ok(mut guard) => guard.monitors().unwrap_or_default(),
        Err(_) => Vec::new(),
    };
    targets
        .iter()
        .map(|id| {
            monitors
                .iter()
                .find(|monitor| &monitor.id == id)
                .map(|monitor| monitor.friendly_name.clone())
                .unwrap_or_else(|| id.clone())
        })
        .collect()
}

/// What a left click on a tray icon does right now.
///
/// Read from the coordinator on every activation, never captured when the icons were installed: the
/// settings window changes this value while the tray is running, and the point of that window is that
/// the change takes effect without a restart. A poisoned lock keeps the cycle behaviour, which cannot
/// open a window that would then be unresponsive.
fn click_behaviour(cached: &std::sync::atomic::AtomicU8) -> crate::config::settings::LeftClick {
    match cached.load(std::sync::atomic::Ordering::Relaxed) {
        1 => crate::config::settings::LeftClick::Flyout,
        _ => crate::config::settings::LeftClick::Cycle,
    }
}

/// Republish the cached behaviour from the coordinator's stored settings.
///
/// Called from every place the UI already reads settings, so a change made anywhere — this window or
/// `broiler settings set` on the command line — reaches the next click.
fn refresh_click_cache<T: crate::transport::Transport>(
    coordinator: &Arc<Mutex<Coordinator<T>>>,
    cached: &std::sync::atomic::AtomicU8,
) {
    let stored = coordinator
        .lock()
        .map(|guard| guard.settings().left_click)
        .unwrap_or(crate::config::settings::LeftClick::Cycle);
    cache_click_behaviour(cached, stored);
}

/// Store the behaviour so the next activation can read it without a lock.
fn cache_click_behaviour(cached: &std::sync::atomic::AtomicU8, value: crate::config::settings::LeftClick) {
    let code = match value {
        crate::config::settings::LeftClick::Cycle => 0,
        crate::config::settings::LeftClick::Flyout => 1,
    };
    cached.store(code, std::sync::atomic::Ordering::Relaxed);
}

/// The state label a tooltip or a flyout header shows for one monitor.
///
/// It is a pure function of the coordinator's one current snapshot, so the label cannot disagree with
/// the icon or with `status`. `matched` and `applied_verified` both name the profile the user sees;
/// the two states differ in provenance, which `status` reports and the tooltip does not need to.
fn state_label<T: crate::transport::Transport>(coordinator: &Coordinator<T>, monitor: &str) -> String {
    let current = coordinator.current(monitor);
    match current.profile_id() {
        Some(id) => coordinator.profile_name(id).unwrap_or(id).to_string(),
        None => match current.state {
            crate::reconcile::CurrentState::Checking => String::from("Checking"),
            crate::reconcile::CurrentState::Unknown => String::from("Unknown"),
            _ => String::from("Custom"),
        },
    }
}

unsafe fn host<'a>(user: *mut std::ffi::c_void) -> Option<&'a mut TrayHost> {
    (user as *mut TrayHost).as_mut()
}

/// The tooltip text for one icon, from cached state only.
///
/// Nothing here touches the hardware: the shell asks for a tooltip whenever the pointer rests on the
/// icon, and a DDC read at that moment would stall the UI thread and the message loop behind it. The
/// brightness shown is the last value this coordinator observed, and it is omitted while unknown.
fn tray_tooltip(user: *mut std::ffi::c_void, monitor: usize) -> String {
    let Some(host) = (unsafe { host(user) }) else {
        return String::from(PRODUCT_NAME);
    };
    let total = host.targets.len();
    let fallback = host
        .targets
        .get(monitor)
        .cloned()
        .unwrap_or_else(|| String::from(PRODUCT_NAME));
    let name = host
        .names
        .get(monitor)
        .cloned()
        .unwrap_or_else(|| fallback.clone());
    let (state, show_name) = match host.coordinator.lock() {
        Ok(guard) => {
            // The display name of the profile, not the file id the user never sees.
            (
                state_label(&guard, &fallback),
                guard.settings().always_show_monitor_name,
            )
        }
        Err(_) => (String::from("Custom"), false),
    };
    crate::tray::tooltip(monitor + 1, total, &name, show_name, &state)
}

fn tray_activate(user: *mut std::ffi::c_void, monitor: usize, _activation: crate::tray::Activation) {
    let Some(host) = (unsafe { host(user) }) else {
        return;
    };
    let Some(id) = host.targets.get(monitor).cloned() else {
        return;
    };
    // Read on every activation. Snapshotting this when the icons were installed is what made a change
    // in the settings window take effect only after a restart.
    match click_behaviour(&host.left_click) {
        crate::config::settings::LeftClick::Cycle => {
            // One cycle is a DDC transaction: it must not run on the UI thread, and its result becomes
            // visible through the same publication every other command uses.
            start_operation(
                host,
                id.clone(),
                Work::Direct {
                    what: String::from("cycle"),
                    method: "profile.next",
                    params: serde_json::json!({"monitor": id}),
                },
            );
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

/// The rows the flyout shows: every profile in registry order, with the certified one marked.
///
/// The marker comes from the coordinator's current snapshot, which is the same answer the icon and the
/// tooltip read; a monitor whose state is unknown shows no marker rather than a guess.
fn tray_flyout_content(
    user: *mut std::ffi::c_void,
    slot: usize,
) -> crate::win::flyout::Content {
    use crate::win::flyout::{Content, Item, State};

    let Some(host) = (unsafe { host(user) }) else {
        return Content {
            title: crate::win::flyout::heading(
                slot + 1,
                0,
                PRODUCT_NAME,
                false,
            ),
            items: Vec::new(),
        };
    };
    let total = host.targets.len();
    let Some(id) = host.targets.get(slot).cloned() else {
        return Content {
            title: crate::win::flyout::heading(slot + 1, total, PRODUCT_NAME, false),
            items: Vec::new(),
        };
    };
    let Ok(mut guard) = host.coordinator.lock() else {
        return Content {
            title: crate::win::flyout::heading(slot + 1, total, &id, false),
            items: Vec::new(),
        };
    };
    let name = host
        .names
        .get(slot)
        .cloned()
        .unwrap_or_else(|| id.clone());
    let show_name = guard.settings().always_show_monitor_name;
    // The marker comes from the same snapshot as the icon and the tooltip, so the flyout cannot claim
    // a profile the other surfaces do not.
    let current = guard.current(&id).profile_id().map(str::to_string);
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
            let icon = profile
                .get("icon")
                .and_then(serde_json::Value::as_str)
                .unwrap_or(&profile_id)
                .to_string();
            Some(Item {
                id: profile_id,
                name: label,
                icon,
                state,
            })
        })
        .collect();
    Content {
        title: crate::win::flyout::heading(slot + 1, total, &name, show_name),
        items,
    }
}

/// Apply the profile the user picked in the flyout.
///
/// The row index is resolved to a profile id here, on the UI thread, and the write itself is handed to
/// [`start_operation`]: a click must return to the message loop immediately.
fn tray_choose(user: *mut std::ffi::c_void, slot: usize, row: usize) {
    let Some(host) = (unsafe { host(user) }) else {
        return;
    };
    let Some(monitor) = host.targets.get(slot).cloned() else {
        return;
    };
    // Nothing is looked up here. The row is turned into a profile id inside the worker thread: this
    // callback runs on the UI thread, and a lookup would wait for the coordinator lock that a running
    // DDC transaction holds, which is exactly how the popup stopped responding under a slow worker.
    start_operation(host, monitor, Work::Row { row });
}

/// Run one coordinator command off the UI thread and report the outcome back to the tray.
///
/// Every hardware operation that a click can start goes through here — a flyout apply and a cycle
/// alike — for three reasons. A DDC transaction can take tens of milliseconds or time out, and the UI
/// thread must never wait for it. One serialized writer is the product invariant, so a second request
/// while one is in flight is refused with a reason instead of queued behind it. And the outcome is
/// reported through the coordinator's own publication, so the icon, the tooltip and the flyout all
/// learn about a completed cycle or apply instead of only the surface that started it.
/// What one operation applies.
///
/// The row case carries no profile id: it is resolved inside the worker thread, because resolving it
/// needs the coordinator lock and a DDC transaction holds that lock for the whole write. Doing the
/// lookup on the UI thread is what froze the popup when the worker was slow.
enum Work {
    Direct {
        what: String,
        method: &'static str,
        params: serde_json::Value,
    },
    Row {
        row: usize,
    },
}

fn start_operation(host: &TrayHost, monitor: String, request: Work) {
    let row = match &request {
        Work::Direct { .. } => None,
        Work::Row { row } => Some(*row),
    };
    let what = match &request {
        Work::Direct { what, .. } => what.clone(),
        Work::Row { .. } => String::from("profile"),
    };
    let window = host
        .tray_window
        .load(std::sync::atomic::Ordering::Relaxed);
    if host
        .applying
        .swap(true, std::sync::atomic::Ordering::SeqCst)
    {
        log_error!("{monitor}: {what} was ignored; another hardware operation is in progress");
        if let Ok(mut guard) = host.last_failure.lock() {
            *guard = Some(String::from("another operation is in progress"));
        }
        if let (Some(row), true) = (row, window != 0) {
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
    // A request that started on a row says so in the status area before the DDC transaction begins, so
    // a slow monitor shows that something is happening instead of looking unresponsive.
    if let (Some(row), true) = (row, window != 0) {
        unsafe {
            windows_sys::Win32::UI::WindowsAndMessaging::PostMessageW(
                window as windows_sys::Win32::Foundation::HWND,
                crate::win::flyout::RESULT,
                row,
                2,
            );
        }
    }
    std::thread::spawn(move || {
        let mut method = "profile.apply";
        let mut what = what;
        let outcome = match request {
            Work::Direct {
                what: direct_what,
                method: direct_method,
                params,
            } => {
                what = direct_what;
                method = direct_method;
                match coordinator.lock() {
                    Ok(mut guard) => guard.handle(method, &params),
                    Err(_) => Err(crate::ipc::Failure::new(
                        "internal",
                        "the coordinator lock was poisoned",
                    )),
                }
            }
            Work::Row { row } => match coordinator.lock() {
                Ok(mut guard) => {
                    let profile = guard
                        .handle("profiles.list", &serde_json::Value::Null)
                        .ok()
                        .and_then(|result| result.get("profiles").cloned())
                        .and_then(|value| value.as_array().cloned())
                        .unwrap_or_default()
                        .get(row)
                        .and_then(|profile| profile.get("id"))
                        .and_then(serde_json::Value::as_str)
                        .map(str::to_string);
                    match profile {
                        Some(profile) => {
                            what = profile.clone();
                            guard.handle(
                                "profile.apply",
                                &serde_json::json!({"id": profile, "monitor": monitor}),
                            )
                        }
                        None => Err(crate::ipc::Failure::new(
                            "internal",
                            "the chosen row no longer names a profile",
                        )),
                    }
                }
                Err(_) => Err(crate::ipc::Failure::new(
                    "internal",
                    "the coordinator lock was poisoned",
                )),
            },
        };
        let applied = matches!(
            &outcome,
            Ok(result) if result.get("status").and_then(serde_json::Value::as_str) == Some("applied")
        );
        match &outcome {
            Ok(result) if applied => {
                // A hardware write is only acceptable with a stated reason and recorded values, so
                // the log carries the requested value, the value written and what the device reported
                // for every control in the profile.
                log_info!("{monitor}: {what} applied ({method})");
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
                log_error!("{monitor}: {what} was not applied ({status})");
                if let Ok(mut guard) = failure.lock() {
                    *guard = Some(status.to_string());
                }
            }
            Err(error) => {
                log_error!("{monitor}: {what} failed: {}", error.message);
                if let Ok(mut guard) = failure.lock() {
                    *guard = Some(error.message.clone());
                }
            }
        }
        applying.store(false, std::sync::atomic::Ordering::SeqCst);
        // The coordinator has already published the change; this message only tells the flyout which
        // row it was, so an error can be shown against the row the user clicked.
        if let (Some(row), true) = (row, window != 0) {
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

/// The icon name of the profile this monitor is currently in, if one is certified.
fn tray_glyph(user: *mut std::ffi::c_void, slot: usize) -> Option<String> {
    let host = unsafe { host(user) }?;
    let id = host.targets.get(slot)?.clone();
    let guard = host.coordinator.lock().ok()?;
    current_icon(&guard, &id)
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
        always_show_monitor_name: false,
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
    // Anything that changes settings outside this window — `broiler settings set` on the command line,
    // for instance — reaches the UI through this snapshot, so the cached click behaviour is refreshed
    // here too. Without it a command-line change was obeyed by `status` and ignored by the next click.
    let left_click = guard.settings().left_click;
    // The cache is refreshed from the same read the window gets, so the visible control and the next
    // click can never disagree.
    cache_click_behaviour(&host.left_click, left_click);
    crate::win::settings::Snapshot {
        left_click,
        always_show_monitor_name: guard.settings().always_show_monitor_name,
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
            apply_setting(host, "tray.left_click", text)
        }
        Action::AlwaysShowMonitorName(show) => {
            apply_setting(
                host,
                "tray.always_show_monitor_name",
                if show { "true" } else { "false" },
            )
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
        Action::ReloadProfiles => {
            // A failed reload keeps the last good registry and reports why; the window says so through
            // its summary line rather than a modal dialog.
            let result = match host.coordinator.lock() {
                Ok(mut guard) => guard.handle("profiles.reload", &serde_json::Value::Null),
                Err(_) => return false,
            };
            match result {
                Ok(value) => {
                    log_info!(
                        "profiles reloaded: {} profile(s)",
                        value.get("count").and_then(serde_json::Value::as_u64).unwrap_or(0)
                    );
                    true
                }
                Err(error) => {
                    log_error!("profiles were not reloaded: {}", error.message);
                    if let Ok(mut guard) = host.last_failure.lock() {
                        *guard = Some(error.message.clone());
                    }
                    false
                }
            }
        }
        Action::OpenRepository => {
            crate::win::ui::open_target("https://github.com/hrkcz001/broiler").is_ok()
        }
        Action::Close => true,
    }
}

/// Perform one `settings.set` and report whether the coordinator accepted it.
///
/// The window shows what the coordinator holds, so a refused write must return false: the caller
/// reverts the control instead of leaving a value visible that was never stored.
fn apply_setting(host: &TrayHost, key: &str, value: &str) -> bool {
    let result = match host.coordinator.lock() {
        Ok(mut guard) => guard.handle(
            "settings.set",
            &serde_json::json!({"key": key, "value": value}),
        ),
        Err(_) => return false,
    };
    match result {
        Ok(_) => {
            log_info!("{key} is now {value}");
            if key == "tray.left_click" {
                // Republish the cached copy here rather than reading it on every click: the click path
                // must not wait for a coordinator lock that a DDC transaction may hold for seconds.
                refresh_click_cache(&host.coordinator, &host.left_click);
            }
            true
        }
        Err(error) => {
            log_error!("{key} was not saved: {}", error.message);
            false
        }
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
    fn capabilities(
        &mut self,
        monitor: &str,
    ) -> Result<Option<String>, crate::transport::TransportError> {
        self.inner.capabilities(monitor)
    }

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

    /// A coordinator over the fake transport, with `profiles` in its registry and its own root.
    fn fixture_host(
        name: &str,
        profiles: &[&str],
    ) -> (
        Arc<Mutex<Coordinator<crate::transport::FakeTransport>>>,
        PathBuf,
    ) {
        let (storage, base) = isolated(name);
        let transport = crate::transport::FakeTransport::with_monitor("mon-a", "Panel")
            .with_value("mon-a", "brightness", 40);
        let coordinator = Coordinator::new(
            transport,
            Settings::default(),
            registry(profiles),
            storage.settings_file(),
        )
        .with_profiles_dir(storage.profiles_dir())
        .with_storage(storage);
        (Arc::new(Mutex::new(coordinator)), base)
    }

    #[test]
    fn a_left_click_follows_the_setting_that_is_stored_now() {
        // The regression: the behaviour was captured when the icons were installed, so changing it in
        // the settings window did nothing until the process restarted. It now comes from a cache that
        // is republished whenever the setting is stored, and the click path never takes the
        // coordinator lock, which a slow DDC transaction can hold for seconds.
        use crate::config::settings::LeftClick;
        use std::sync::atomic::AtomicU8;

        let (coordinator, base) = fixture_host("live-click", &["day", "night"]);
        let cache = AtomicU8::new(0);
        let stored = coordinator
            .lock()
            .expect("lock")
            .settings()
            .left_click;
        cache_click_behaviour(&cache, stored);
        assert_eq!(click_behaviour(&cache), LeftClick::Flyout);

        for (value, expected) in [("cycle", LeftClick::Cycle), ("flyout", LeftClick::Flyout)] {
            coordinator
                .lock()
                .expect("lock")
                .handle(
                    "settings.set",
                    &serde_json::json!({"key": "tray.left_click", "value": value}),
                )
                .expect("settings.set");
            let stored = coordinator
                .lock()
                .expect("lock")
                .settings()
                .left_click;
            cache_click_behaviour(&cache, stored);
            assert_eq!(
                click_behaviour(&cache),
                expected,
                "a stored change must be what the next click obeys"
            );
        }
        let _ = std::fs::remove_dir_all(base);
    }

    #[test]
    fn a_click_behaviour_changed_on_the_command_line_reaches_the_next_click() {
        // The regression: the cache was refreshed only by the settings window, so `broiler settings set
        // tray.left_click cycle` was stored, reported by `status`, and still ignored by the next click.
        use crate::config::settings::LeftClick;
        use std::sync::atomic::AtomicU8;

        let (coordinator, base) = fixture_host("cli-click", &["day", "night"]);
        // Start from the opposite value, the way a process that has been running would.
        let cache = AtomicU8::new(1);
        assert_eq!(click_behaviour(&cache), LeftClick::Flyout);

        coordinator
            .lock()
            .expect("lock")
            .handle(
                "settings.set",
                &serde_json::json!({"key": "tray.left_click", "value": "cycle"}),
            )
            .expect("settings.set");

        refresh_click_cache(&coordinator, &cache);
        assert_eq!(
            click_behaviour(&cache),
            LeftClick::Cycle,
            "the snapshot refresh is what carries a command-line change to the next click"
        );
        let _ = std::fs::remove_dir_all(base);
    }

    #[test]
    fn the_tooltip_state_is_the_profile_display_name_and_not_its_id() {
        // The regression: the tooltip printed `applied_profile`, which is the file id.
        let (storage, base) = isolated("tooltip-name");
        let text = "schema_version = 1\nid = \"night\"\nname = \"Night mode\"\nicon = \"night\"\norder = 10\n\n[monitor]\nbrightness = 40\n";
        let profile = crate::config::profile::parse(text, std::path::Path::new("night.toml"))
            .expect("profile");
        let transport = crate::transport::FakeTransport::with_monitor("mon-a", "Panel");
        let mut coordinator = Coordinator::new(
            transport,
            Settings::default(),
            ProfileRegistry::new(vec![profile]).expect("registry"),
            storage.settings_file(),
        )
        .with_profiles_dir(storage.profiles_dir())
        .with_storage(storage);

        // Before any observation the honest answer is that nobody knows yet.
        assert_eq!(state_label(&coordinator, "mon-a"), "Checking");

        coordinator
            .handle("profile.apply", &serde_json::json!({"id": "night", "monitor": "mon-a"}))
            .expect("apply");
        assert_eq!(
            state_label(&coordinator, "mon-a"),
            "Night mode",
            "the tooltip must show the name the user wrote, not the file id"
        );
        assert_eq!(
            coordinator.current("mon-a").state,
            crate::reconcile::CurrentState::Applied
        );

        // A direct change is not the profile any more: the reads decide, and they say Custom.
        coordinator
            .handle(
                "parameters.set",
                &serde_json::json!({"monitor": "mon-a", "control": "brightness", "value": 60, "unit": "native"}),
            )
            .expect("direct set");
        assert_eq!(coordinator.current("mon-a").state, crate::reconcile::CurrentState::Custom);
        assert_eq!(state_label(&coordinator, "mon-a"), "Custom");

        // Back to the profile's own value, and a read-only observation certifies it as matched — not
        // applied, because nothing was written this time.
        coordinator
            .handle(
                "parameters.set",
                &serde_json::json!({"monitor": "mon-a", "control": "brightness", "value": 40, "unit": "native"}),
            )
            .expect("direct set");
        assert_eq!(coordinator.current("mon-a").state, crate::reconcile::CurrentState::Matched);
        assert_eq!(
            crate::tray::tooltip(1, 1, "Panel", true, &state_label(&coordinator, "mon-a")),
            "Panel - Night mode"
        );
        let _ = std::fs::remove_dir_all(base);
    }

    #[test]
    fn every_mutating_command_announces_a_change_and_a_read_does_not() {
        let (coordinator, base) = fixture_host("publish", &["day", "night"]);
        let mut guard = coordinator.lock().expect("lock");

        // A read or a query publishes nothing: no surface shows anything new.
        guard.handle("status", &serde_json::Value::Null).expect("status");
        assert!(guard.last_published().is_empty());
        guard.handle("profiles.list", &serde_json::Value::Null).expect("list");
        assert!(guard.last_published().is_empty());

        // Apply, cycle, reload, settings and selection each publish the monitor they changed. The
        // publication happens inside `handle`, so one command cannot be implemented without it.
        for (method, params) in [
            ("profile.apply", serde_json::json!({"id": "day", "monitor": "mon-a"})),
            ("profile.next", serde_json::json!({"monitor": "mon-a"})),
            (
                "settings.set",
                serde_json::json!({"key": "tray.left_click", "value": "cycle"}),
            ),
            (
                "monitors.select",
                serde_json::json!({"monitor_ids": ["mon-a"], "remember": true}),
            ),
            ("profiles.reload", serde_json::Value::Null),
        ] {
            guard.handle(method, &params).expect(method);
            assert_eq!(
                guard.last_published(),
                [String::from("mon-a")],
                "{method} must announce the monitor it changed"
            );
        }

        // A failed command still publishes nothing new: its own state did not change.
        guard
            .handle("settings.set", &serde_json::json!({"key": "nonsense.key", "value": "1"}))
            .expect_err("unknown key");
        assert!(guard.last_published().is_empty());
        drop(guard);
        let _ = std::fs::remove_dir_all(base);
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
        // No daemon is started for this fixture namespace; the endpoint simply has no server. The
        // owner's resident instance must not answer it either, which is why the namespace exists.
        let (storage, base) = isolated("no-coordinator");
        let name = crate::pipe::fixture::namespace("runtime-none");
        let answer = crate::pipe::fixture::with_namespace(&name, || {
            request(&storage, "ping", serde_json::Value::Null)
        });
        assert_eq!(answer, Err(ClientError::NotRunning));
        let _ = std::fs::remove_dir_all(base);
    }

    #[test]
    fn stopping_without_a_coordinator_is_idempotent() {
        let (storage, base) = isolated("stop");
        let name = crate::pipe::fixture::namespace("runtime-stop");
        let answer = crate::pipe::fixture::with_namespace(&name, || stop_resident(&storage));
        assert_eq!(answer, Ok(false));
        let _ = std::fs::remove_dir_all(base);
    }

    /// A fixture coordinator answering on its own namespace, so the next test can prove that a live
    /// instance is invisible to a namespace that is not its own.
    #[test]
    fn one_fixture_namespace_never_answers_another() {
        use crate::config::registry::ProfileRegistry;
        use crate::daemon::handle_connection;
        use crate::pipe::{fixture, local_endpoint, ServerConnection};
        use crate::transport::FakeTransport;

        let (storage, base) = isolated("namespaces");
        let serving = fixture::namespace("runtime-serving");
        let endpoint = fixture::with_namespace(&serving, || local_endpoint().expect("endpoint"));
        let server = ServerConnection::create(&endpoint, true).expect("fixture server");

        let transport = FakeTransport::with_monitor("mon-a", "Panel");
        let coordinator = Arc::new(Mutex::new(
            Coordinator::new(
                transport,
                Settings::default(),
                ProfileRegistry::default(),
                storage.settings_file(),
            )
            .with_storage(storage.clone()),
        ));
        let handle = std::thread::spawn(move || {
            server.connect().expect("client connected");
            handle_connection(server, coordinator);
        });

        // The same fixture instance answers on its own namespace ...
        let answered = request_on(&serving, &storage, "ping", serde_json::Value::Null);
        assert!(answered.is_ok(), "{answered:?}");
        handle.join().expect("fixture server thread");

        // ... and an unrelated namespace, which is where every other test lives, sees nothing.
        let other = fixture::namespace("runtime-other");
        let silent = request_on(&other, &storage, "ping", serde_json::Value::Null);
        assert_eq!(silent, Err(ClientError::NotRunning));
        let _ = std::fs::remove_dir_all(base);
    }
}
