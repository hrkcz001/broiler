//! Command dispatch for both executables.
//!
//! This module owns argument dispatch, the version/help surface, exit codes and the logger
//! bootstrap. The help text is part of the user-facing contract, so it lists what this build
//! actually does and names what it does not; it must be updated in the same commit as a capability,
//! because a help screen that denies a working feature is as wrong as one that promises a missing one.

use std::path::PathBuf;

use serde::Serialize;
use serde_json::json;

use crate::config::value::percent_to_native;
use crate::discovery::{self, DiscoveryFailure};
use crate::logging::{self, Level};
use crate::model::{CapabilitiesReport, ControlEvidence, MonitorListReport, Reading};
use crate::supervisor::{Supervisor, SupervisorError, WorkerCommand};
use crate::storage::StorageContext;
use crate::{output, PRODUCT_NAME, VERSION};

/// Command completed, or an idempotent stop, or a normal start with no monitors.
pub const EXIT_OK: i32 = 0;
/// Command or configuration validation error.
pub const EXIT_USAGE: i32 = 2;
/// A daemon-only command was issued and no daemon is running.
pub const EXIT_DAEMON_UNAVAILABLE: i32 = 3;
/// The requested target is missing or ambiguous.
pub const EXIT_TARGET: i32 = 4;
/// A requested control is unsupported under the strict policy.
pub const EXIT_UNSUPPORTED: i32 = 5;
/// Timeout, transport error or verification failure.
pub const EXIT_TRANSPORT: i32 = 6;
/// Partial application: some requested fields were applied, others were not.
pub const EXIT_PARTIAL: i32 = 7;

/// Which executable is running.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum Program {
    Console,
    Tray,
}

fn help_text() -> String {
    format!(
        "{name} {version} - native Windows DDC/CI monitor controller\n\
         \n\
         Usage:\n\
         \x20 {name} [--config-dir <path>] <command> [options]\n\
         \n\
         Data:\n\
         \x20 paths [--json]                       where settings, profiles and logs live\n\
         \x20 data migrate --from-appdata [--dry-run | --decline] [--json]\n\
         \x20                                     copy an older AppData configuration here\n\
         \n\
         Coordinator:\n\
         \x20 start | stop | status [--json] [--refresh] | reload\n\
         \n\
         Monitors and parameters:\n\
         \x20 monitors list [--json] | monitors capabilities --monitor <id> [--json]\n\
         \x20 monitors select --monitor <id> [--remember] | --none | --dialog\n\
         \x20 flyout [--monitor <id>]             open the profile flyout of the running instance\n\
         \x20 settings window                     open the settings window of the running instance\n\
         \x20 monitors validate-value <parameter> <value> --vcp <code> --monitor <id> --confirm-write\n\
         \x20 get <parameter> --monitor <id> | set <parameter> <value> --monitor <id> [--wait]\n\
         \n\
         Profiles and settings:\n\
         \x20 profiles list [--json] | profiles validate | profiles open\n\
         \x20 profile apply <id> (--monitor <id> | --all-selected) [--wait] | profile next --monitor <id>\n\
         \x20 settings get [key] [--json] | settings set <key> <value> | settings open\n\
         \x20 startup status | repository open | icons list\n\
         \n\
         General:\n\
         \x20 --config-dir <path>   use one data root instead of AppData; must be writable\n\
         \x20 --version, -V         print the version\n\
         \x20 --help, -h            print this help\n\
         \n\
         Implemented: storage resolution and copy-only migration, monitor discovery, brightness and\n\
         contrast reads and writes, profile parsing and planning, the resident coordinator, the\n\
         startup lifecycle, the monitor selection dialog, the zero-profile setup window, one tray\n\
         icon per controlled monitor with a Settings/Exit menu and tooltips, the modeless profile\n\
         flyout, the native settings window with a live left-click selector, and the bounded log.\n\
         Not implemented in this build: colour temperature and the remaining conditional controls,\n\
         speaker volume and mute writes, and launch-at-logon registration. A command that needs one\n\
         of those reports it and exits 2; nothing writes a value it cannot verify.\n\
         Exit codes: 0 success, 2 validation error, 3 daemon unavailable, 4 missing target,\n\
         5 unsupported control, 6 transport or verification failure, 7 partial application.",
        name = PRODUCT_NAME,
        version = VERSION
    )
}

/// Default log level until settings are implemented.
const DEFAULT_LOG_LEVEL: Level = Level::Warn;

fn init_logging(storage: &StorageContext) {
    // The level the user configured, not a constant: `settings get logging.level` must not claim a
    // verbosity the log file does not honour. An unreadable settings file keeps the default.
    let level = crate::config::settings::load(&storage.settings_file())
        .map(|settings| settings.log_level)
        .unwrap_or(DEFAULT_LOG_LEVEL);
    logging::init(level, storage.log_file());
}

fn print_version() {
    output::line(&format!("{} {}", PRODUCT_NAME, VERSION));
}


fn dispatch_console(args: &[String], storage: &StorageContext) -> i32 {
    let Some(first) = args.first() else {
        output::error_line(&help_text());
        return EXIT_USAGE;
    };

    match first.as_str() {
        "--version" | "-V" | "version" => {
            print_version();
            EXIT_OK
        }
        "--help" | "-h" | "help" => {
            output::line(&help_text());
            EXIT_OK
        }
        "monitors" => handle_monitors(&args[1..], storage),
        "start" => handle_start(&args[1..], storage),
        "stop" => handle_stop(&args[1..], storage),
        "status" => handle_status(&args[1..], storage),
        "profiles" => handle_profiles(&args[1..], storage),
        "settings" => handle_settings(&args[1..], storage),
        "reload" => handle_reload(&args[1..], storage),
        "profile" => handle_profile(&args[1..], storage),
        "startup" => handle_startup(&args[1..], storage),
        "icons" => handle_icons(&args[1..]),
        "flyout" => handle_flyout(&args[1..], storage),
        "paths" => handle_paths(&args[1..], storage),
        "data" => handle_data(&args[1..], storage),
        "repository" => handle_repository(&args[1..]),
        "get" => handle_get(&args[1..], storage),
        "set" => handle_set(&args[1..], storage),
        other => {
            output::error_line(&format!("{}: unknown command: {}", PRODUCT_NAME, other));
            EXIT_USAGE
        }
    }
}

// ---------------------------------------------------------------------------
// coordinator lifecycle: start, stop, status
// ---------------------------------------------------------------------------

fn client_exit_code(error: &crate::runtime::ClientError) -> i32 {
    use crate::runtime::ClientError;
    match error {
        ClientError::NotRunning => EXIT_DAEMON_UNAVAILABLE,
        ClientError::Channel(_) => EXIT_TRANSPORT,
        ClientError::Failure(failure) => match failure.kind.as_str() {
            "invalid" | "invalid_config" => EXIT_USAGE,
            "unsupported" => EXIT_UNSUPPORTED,
            "unknown_monitor" | "ambiguous_monitor" => EXIT_TARGET,
            "storage_conflict" => EXIT_USAGE,
            "partial" => EXIT_PARTIAL,
            _ => EXIT_TRANSPORT,
        },
    }
}

fn report_client_error(error: &crate::runtime::ClientError) -> i32 {
    output::error_line(&format!("{}: {}", PRODUCT_NAME, error.message()));
    client_exit_code(error)
}

fn handle_status(args: &[String], storage: &StorageContext) -> i32 {
    let mut json_output = false;
    let mut refresh = false;
    for argument in args {
        match argument.as_str() {
            "--json" => json_output = true,
            // `--refresh` asks the coordinator for one fresh read-only observation. Without it,
            // status answers from the cached snapshot and touches no hardware.
            "--refresh" => refresh = true,
            other => return usage_error(&format!("status: unexpected argument: {other}")),
        }
    }

    match crate::runtime::request(storage, "status", json!({"refresh": refresh})) {
        Ok(result) => {
            if json_output {
                emit_json(&result);
            } else {
                let monitors = result
                    .get("monitors")
                    .and_then(|value| value.as_array())
                    .cloned()
                    .unwrap_or_default();
                let profiles = result.get("profiles").and_then(serde_json::Value::as_u64).unwrap_or(0);
                output::line(&format!(
                    "running, {} monitor(s), {profiles} profile(s)",
                    monitors.len()
                ));
                for monitor in &monitors {
                    let name = monitor
                        .get("friendly_name")
                        .and_then(serde_json::Value::as_str)
                        .unwrap_or("?");
                    let current = monitor.get("current").cloned().unwrap_or(json!(null));
                    let state = current
                        .get("state")
                        .and_then(serde_json::Value::as_str)
                        .unwrap_or("unknown");
                    let profile = current
                        .get("profile_name")
                        .or_else(|| current.get("profile"))
                        .and_then(serde_json::Value::as_str);
                    match profile {
                        Some(profile) => output::line(&format!("  {name}: {profile} ({state})")),
                        None => output::line(&format!("  {name}: {state}")),
                    }
                }
            }
            EXIT_OK
        }
        Err(crate::runtime::ClientError::NotRunning) => {
            if json_output {
                // The roots are known without a coordinator, so they are reported rather than
                // omitted: a script should not have to guess where this build keeps its data.
                emit_json(&json!({"running": false, "storage": storage.describe()}));
            } else {
                output::line("not running");
            }
            EXIT_OK
        }
        Err(error) => {
            // A coordinator that has answered `stop` is not a running coordinator any more.
            if matches!(&error, crate::runtime::ClientError::Failure(failure) if failure.kind == "shutting_down") {
                if json_output {
                    emit_json(&json!({"running": false, "storage": storage.describe()}));
                } else {
                    output::line("not running");
                }
                EXIT_OK
            } else {
                report_client_error(&error)
            }
        }
    }
}

fn handle_stop(args: &[String], storage: &StorageContext) -> i32 {
    if let Some(argument) = args.first() {
        return usage_error(&format!("stop: unexpected argument: {argument}"));
    }
    match crate::runtime::stop_resident(storage) {
        Ok(true) => {
            output::line("coordinator stopped");
            EXIT_OK
        }
        Ok(false) => {
            output::line("not running");
            EXIT_OK
        }
        Err(error) => report_client_error(&error),
    }
}

fn handle_start(args: &[String], storage: &StorageContext) -> i32 {
    if let Some(argument) = args.first() {
        return usage_error(&format!("start: unexpected argument: {argument}"));
    }
    if crate::runtime::is_running(storage) {
        output::line("already running");
        return EXIT_OK;
    }
    // Report the same actionable message the resident process would, instead of starting a child
    // that exits immediately.
    if let Some(reason) = crate::runtime::pending_import_message(storage) {
        output::error_line(&format!("{}: {reason}", PRODUCT_NAME));
        return EXIT_USAGE;
    }
    // `start` is also the logon entry point, so it follows the same lifecycle: nothing to control
    // exits silently, one profile applies once and exits, and only a multi-profile setup stays
    // resident. Without this the tray became resident even when it had nothing to apply.
    match crate::runtime::startup_step_for_storage(storage) {
        crate::runtime::StartupStep::Serve => {}
        crate::runtime::StartupStep::ApplyOnce { .. } => {
            unreachable!("the startup step applies before it returns")
        }
        crate::runtime::StartupStep::Exit(code) => return code,
    }
    match crate::runtime::start_resident(storage) {
        Ok(()) => {
            output::line("coordinator started");
            EXIT_OK
        }
        Err(reason) => {
            output::error_line(&format!("{}: {reason}", PRODUCT_NAME));
            EXIT_TRANSPORT
        }
    }
}

/// The checkbox dialog, opened explicitly. Recording the answer goes through the coordinator when one
/// is running so its in-memory settings stay current, and through the file otherwise.
///
/// A selection of nothing is a deliberate answer: it records an empty selection and stops controlling
/// anything, which is different from Cancel.
fn select_with_dialog(storage: &StorageContext, remember: bool) -> i32 {
    let transport = match crate::runtime::TransientTransport::new() {
        Ok(transport) => transport,
        Err(reason) => {
            output::error_line(&format!("{}: {reason}", PRODUCT_NAME));
            return EXIT_TRANSPORT;
        }
    };
    use crate::transport::Transport;
    let mut transport = transport;
    let monitors = match transport.monitors() {
        Ok(monitors) => monitors,
        Err(error) => {
            output::error_line(&format!("{}: {}", PRODUCT_NAME, error.message()));
            return EXIT_TRANSPORT;
        }
    };
    if monitors.is_empty() {
        output::error_line(&format!(
            "{}: there is no controllable monitor to select",
            PRODUCT_NAME
        ));
        return EXIT_TARGET;
    }
    let entries = crate::win::ui::entries_for(&monitors);
    match crate::win::ui::select_monitors(
        "broiler: select monitors",
        "Choose the monitors broiler controls.",
        &entries,
        remember,
    ) {
        crate::win::ui::Answer::Cancelled => {
            output::line("selection unchanged");
            EXIT_OK
        }
        crate::win::ui::Answer::Confirmed { ids, remember } => {
            record_selection(storage, &ids, remember)
        }
    }
}

/// Store the confirmed selection, through the coordinator when it is running.
fn record_selection(storage: &StorageContext, ids: &[String], remember: bool) -> i32 {
    if crate::runtime::is_running(storage) {
        return match crate::runtime::request(
            storage,
            "monitors.select",
            json!({"monitor_ids": ids, "remember": remember}),
        ) {
            Ok(result) => {
                emit_json(&result);
                EXIT_OK
            }
            Err(error) => report_client_error(&error),
        };
    }
    // No coordinator: write the two settings fields and leave everything else exactly as it is.
    let mut settings = match crate::config::settings::load(&storage.settings_file()) {
        Ok(settings) => settings,
        Err(error) => {
            output::error_line(&format!("{}: {}", PRODUCT_NAME, error.render()));
            return EXIT_USAGE;
        }
    };
    settings.monitor_ids = ids.to_vec();
    settings.remember = remember;
    match crate::config::settings::save(&storage.settings_file(), &settings) {
        Ok(()) => {
            emit_json(&json!({
                "selection": settings.monitor_ids,
                "remember": settings.remember,
                "storage": storage.describe(),
            }));
            EXIT_OK
        }
        Err(error) => {
            output::error_line(&format!("{}: {}", PRODUCT_NAME, error.render()));
            EXIT_TRANSPORT
        }
    }
}

/// Open a path with the shell's default handler.
fn open_path(path: &std::path::Path) -> Result<(), String> {
    use windows_sys::Win32::UI::Shell::ShellExecuteW;
    let operation: Vec<u16> = "open\0".encode_utf16().collect();
    let file: Vec<u16> = path
        .display()
        .to_string()
        .encode_utf16()
        .chain(std::iter::once(0))
        .collect();
    let result = unsafe {
        ShellExecuteW(
            std::ptr::null_mut(),
            operation.as_ptr(),
            file.as_ptr(),
            std::ptr::null(),
            std::ptr::null(),
            1,
        )
    };
    if result as isize <= 32 {
        return Err(format!("cannot open {}", path.display()));
    }
    Ok(())
}

fn handle_profiles(args: &[String], storage: &StorageContext) -> i32 {
    let Some(subcommand) = args.first() else {
        return usage_error("profiles: expected 'list', 'validate' or 'open'");
    };

    let directory = storage.profiles_dir();

    match subcommand.as_str() {
        "list" => {
            let mut json_output = false;
            for argument in &args[1..] {
                match argument.as_str() {
                    "--json" => json_output = true,
                    other => return usage_error(&format!("profiles list: unexpected argument: {other}")),
                }
            }
            match crate::config::ProfileRegistry::load_dir(&directory) {
                Ok(registry) => {
                    if json_output {
                        emit_json(&json!({
                            "directory": directory.display().to_string(),
                            "profiles": registry.profiles().iter().map(|profile| json!({
                                "id": profile.id,
                                "name": profile.name,
                                "icon": profile.icon,
                                "order": profile.order,
                                "controls": profile.requested_control_names(),
                            })).collect::<Vec<serde_json::Value>>(),
                        }));
                    } else {
                        output::line(&format!("{}", directory.display()));
                        for profile in registry.profiles() {
                            output::line(&format!(
                                "{:<4} {:<12} {:<16} {}",
                                profile.order,
                                profile.id,
                                profile.name,
                                profile.requested_control_names().join(", ")
                            ));
                        }
                    }
                    EXIT_OK
                }
                Err(error) => {
                    output::error_line(&format!("{}: {}", PRODUCT_NAME, error.render()));
                    EXIT_USAGE
                }
            }
        }
        "validate" => match crate::config::ProfileRegistry::load_dir(&directory) {
            Ok(registry) => {
                output::line(&format!(
                    "{} profile(s) valid in {}",
                    registry.len(),
                    directory.display()
                ));
                EXIT_OK
            }
            Err(error) => {
                output::error_line(&format!("{}: {}", PRODUCT_NAME, error.render()));
                EXIT_USAGE
            }
        },
        "open" => match open_path(&directory) {
            Ok(()) => EXIT_OK,
            Err(reason) => usage_error(&reason),
        },
        other => usage_error(&format!("profiles: unknown subcommand: {other}")),
    }
}

fn handle_settings(args: &[String], storage: &StorageContext) -> i32 {
    // `settings window` opens the running instance's settings window; every other form is a key.
    if args.first().map(String::as_str) == Some("window") {
        if args.len() > 1 {
            return usage_error(&format!("settings window: unexpected argument: {}", args[1]));
        }
        return match crate::runtime::request(storage, "ui.settings", serde_json::Value::Null) {
            Ok(_) => {
                output::line("settings window requested");
                EXIT_OK
            }
            Err(error) => report_client_error(&error),
        };
    }

    let Some(subcommand) = args.first() else {
        return usage_error("settings: expected 'get', 'set' or 'open'");
    };
    let path = storage.settings_file();

    match subcommand.as_str() {
        "get" => {
            let mut key: Option<&str> = None;
            let mut json_output = false;
            for argument in &args[1..] {
                match argument.as_str() {
                    "--json" => json_output = true,
                    other if key.is_none() => key = Some(other),
                    other => return usage_error(&format!("settings get: unexpected argument: {other}")),
                }
            }
            match crate::runtime::request(storage, 
                "settings.get",
                match key {
                    Some(key) => json!({ "key": key }),
                    None => json!(null),
                },
            ) {
                Ok(result) => {
                    emit_json(&result);
                    EXIT_OK
                }
                Err(crate::runtime::ClientError::NotRunning) => {
                    // No coordinator: read the file directly, with the same schema rules.
                    match crate::config::settings::load(&path) {
                        Ok(settings) => {
                            let value = match key {
                                Some(key) => {
                                    let text = std::fs::read_to_string(&path).unwrap_or_default();
                                    let _ = text;
                                    settings_key_value(&settings, key)
                                }
                                None => serde_json::to_value(format!("{settings:?}")).ok(),
                            };
                            match value {
                                Some(value) => {
                                    if json_output || key.is_some() {
                                        emit_json(&value);
                                    } else {
                                        output::line(&format!("{value}"));
                                    }
                                    EXIT_OK
                                }
                                None => usage_error(&format!("unknown settings key `{}`", key.unwrap_or(""))),
                            }
                        }
                        Err(error) => {
                            output::error_line(&format!("{}: {}", PRODUCT_NAME, error.render()));
                            EXIT_USAGE
                        }
                    }
                }
                Err(error) => report_client_error(&error),
            }
        }
        "set" => {
            let Some(key) = args.get(1) else {
                return usage_error("settings set: a key is required");
            };
            let Some(value) = args.get(2) else {
                return usage_error("settings set: a value is required");
            };
            match crate::runtime::request(storage, "settings.set", json!({"key": key, "value": value})) {
                Ok(result) => {
                    emit_json(&result);
                    EXIT_OK
                }
                Err(crate::runtime::ClientError::NotRunning) => {
                    let text = std::fs::read_to_string(&path).unwrap_or_default();
                    match crate::config::settings::set_key(&path, &text, key, value) {
                        Ok(updated) => {
                            let parsed = match crate::config::settings::parse(&updated, &path) {
                                Ok(parsed) => parsed,
                                Err(error) => {
                                    output::error_line(&format!("{}: {}", PRODUCT_NAME, error.render()));
                                    return EXIT_USAGE;
                                }
                            };
                            if let Err(error) = crate::config::settings::save(&path, &parsed) {
                                output::error_line(&format!("{}: {}", PRODUCT_NAME, error.render()));
                                return EXIT_TRANSPORT;
                            }
                            output::line(&format!("{key} updated"));
                            EXIT_OK
                        }
                        Err(error) => {
                            output::error_line(&format!("{}: {}", PRODUCT_NAME, error.render()));
                            EXIT_USAGE
                        }
                    }
                }
                Err(error) => report_client_error(&error),
            }
        }
        "open" => match open_path(&path) {
            Ok(()) => EXIT_OK,
            Err(reason) => usage_error(&reason),
        },
        other => usage_error(&format!("settings: unknown subcommand: {other}")),
    }
}

fn settings_key_value(
    settings: &crate::config::Settings,
    key: &str,
) -> Option<serde_json::Value> {
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

// ---------------------------------------------------------------------------
// profiles, monitor selection, startup, icons
// ---------------------------------------------------------------------------

fn handle_reload(args: &[String], storage: &StorageContext) -> i32 {
    if let Some(argument) = args.first() {
        return usage_error(&format!("reload: unexpected argument: {argument}"));
    }
    match crate::runtime::request(storage, "profiles.reload", json!(null)) {
        Ok(result) => {
            let count = result.get("count").and_then(serde_json::Value::as_u64).unwrap_or(0);
            output::line(&format!("reloaded {count} profile(s)"));
            EXIT_OK
        }
        Err(error) => report_client_error(&error),
    }
}

fn handle_profile(args: &[String], storage: &StorageContext) -> i32 {
    let Some(subcommand) = args.first() else {
        return usage_error("profile: expected 'apply' or 'next'");
    };

    match subcommand.as_str() {
        "apply" => {
            let Some(id) = args.get(1) else {
                return usage_error("profile apply: a profile id is required");
            };
            let mut monitor: Option<String> = None;
            let mut all_selected = false;
            let mut json_output = false;
            let mut index = 2usize;
            while index < args.len() {
                match args[index].as_str() {
                    "--monitor" => {
                        index += 1;
                        match args.get(index) {
                            Some(value) => monitor = Some(value.clone()),
                            None => return usage_error("profile apply: --monitor requires an identifier"),
                        }
                    }
                    "--all-selected" => all_selected = true,
                    "--wait" => {}
                    "--json" => json_output = true,
                    other => {
                        return usage_error(&format!("profile apply: unexpected argument: {other}"))
                    }
                }
                index += 1;
            }
            if monitor.is_some() && all_selected {
                return usage_error("profile apply: use either --monitor or --all-selected");
            }
            let params = match (&monitor, all_selected) {
                (Some(monitor), _) => json!({"id": id, "monitor": monitor}),
                (None, true) => json!({"id": id, "all_selected": true}),
                (None, false) => {
                    return usage_error(
                        "profile apply: a target is required: --monitor <id> or --all-selected",
                    )
                }
            };
            match crate::runtime::request(storage, "profile.apply", params) {
                Ok(result) => report_apply(&result, json_output),
                Err(error) => report_client_error(&error),
            }
        }
        "next" => {
            let mut monitor: Option<String> = None;
            let mut json_output = false;
            let mut index = 1usize;
            while index < args.len() {
                match args[index].as_str() {
                    "--monitor" => {
                        index += 1;
                        match args.get(index) {
                            Some(value) => monitor = Some(value.clone()),
                            None => return usage_error("profile next: --monitor requires an identifier"),
                        }
                    }
                    "--json" => json_output = true,
                    other => return usage_error(&format!("profile next: unexpected argument: {other}")),
                }
                index += 1;
            }
            let Some(monitor) = monitor else {
                return usage_error("profile next: --monitor <id> is required");
            };
            match crate::runtime::request(storage, "profile.next", json!({"monitor": monitor})) {
                Ok(result) => report_apply(&result, json_output),
                Err(error) => report_client_error(&error),
            }
        }
        other => usage_error(&format!("profile: unknown subcommand: {other}")),
    }
}

/// Print an apply result and map its status to an exit code.
pub(crate) fn report_apply(result: &serde_json::Value, json_output: bool) -> i32 {
    let status = result
        .get("status")
        .and_then(serde_json::Value::as_str)
        .unwrap_or("not_applied");
    let cause = result
        .get("cause")
        .and_then(serde_json::Value::as_str)
        .unwrap_or("");

    if json_output {
        emit_json(result);
    } else {
        let profile = result
            .get("profile")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("");
        for monitor in result
            .get("monitors")
            .and_then(serde_json::Value::as_array)
            .into_iter()
            .flatten()
        {
            let id = monitor
                .get("monitor")
                .and_then(serde_json::Value::as_str)
                .unwrap_or("");
            let monitor_status = monitor
                .get("status")
                .and_then(serde_json::Value::as_str)
                .unwrap_or("");
            output::line(&format!("{id}: {profile} {monitor_status}"));
            for field in monitor
                .get("fields")
                .and_then(serde_json::Value::as_array)
                .into_iter()
                .flatten()
            {
                let control = field
                    .get("control")
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or("");
                let field_status = field
                    .get("status")
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or("");
                let detail = field
                    .get("reason")
                    .or_else(|| field.get("state"))
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or("");
                output::line(&format!("  {control:<20} {field_status} {detail}"));
            }
        }
    }

    match status {
        "applied" => EXIT_OK,
        "partial" => EXIT_PARTIAL,
        // A result that applied nothing still carries a typed cause: an unsupported or unwritable
        // control is exit 5, an outcome-unknown or broken path is exit 6, a bad value is exit 2.
        _ => match cause {
            "unsupported" | "not_implemented" | "blocked_by_mode" => EXIT_UNSUPPORTED,
            "invalid_value" => EXIT_USAGE,
            _ => EXIT_TRANSPORT,
        },
    }
}

fn handle_startup(args: &[String], storage: &StorageContext) -> i32 {
    match args.first().map(String::as_str) {
        Some("status") => match crate::runtime::request(storage, "startup.status", json!(null)) {
            Ok(result) => {
                emit_json(&result);
                EXIT_OK
            }
            Err(error) => report_client_error(&error),
        },
        Some(other) => usage_error(&format!("startup: unknown subcommand: {other}")),
        None => usage_error("startup: expected 'status'"),
    }
}

/// `broiler paths [--json]`: where this installation actually keeps its data.
///
/// A running coordinator reports its own root as well, so a mismatch between the CLI and the
/// resident writer is visible instead of silently splitting the configuration.
fn handle_paths(args: &[String], storage: &StorageContext) -> i32 {
    let mut json_output = false;
    for argument in args {
        match argument.as_str() {
            "--json" => json_output = true,
            other => return usage_error(&format!("paths: unexpected argument: {other}")),
        }
    }

    let mut described = storage.describe();
    match crate::runtime::request(storage, "storage.info", json!(null)) {
        Ok(info) => {
            if let Some(daemon) = info.get("storage") {
                described["coordinator"] = serde_json::json!({
                    "running": true,
                    "root": daemon.get("root").cloned().unwrap_or(serde_json::Value::Null),
                    "mode": daemon.get("mode").cloned().unwrap_or(serde_json::Value::Null),
                });
            }
        }
        Err(crate::runtime::ClientError::NotRunning) => {
            described["coordinator"] = serde_json::json!({ "running": false });
        }
        Err(error) => return report_client_error(&error),
    }

    if json_output {
        emit_json(&described);
    } else {
        output::line(&format!("mode      {}", storage.mode().as_str()));
        output::line(&format!("root      {}", storage.root().display()));
        output::line(&format!("settings  {}", storage.settings_file().display()));
        output::line(&format!("profiles  {}", storage.profiles_dir().display()));
        output::line(&format!("state     {}", storage.state_file().display()));
        output::line(&format!("logs      {}", storage.log_dir().display()));
        if let Some(marker) = storage.marker() {
            output::line(&format!("marker    {}", marker.display()));
        }
    }
    EXIT_OK
}

/// `broiler data migrate --from-appdata [--dry-run|--decline] [--json]`
///
/// Copy-only and explicit: it never deletes the source, never overwrites a different destination,
/// and never touches hardware, the coordinator or startup registration.
fn handle_data(args: &[String], storage: &StorageContext) -> i32 {
    let Some(roots) = crate::storage::appdata_roots() else {
        return usage_error("data migrate: the AppData known folders are unavailable");
    };
    run_data(args, storage, &roots)
}

/// The migration command with its source roots supplied explicitly, so the whole command surface
/// can be exercised against disposable directories.
fn run_data(
    args: &[String],
    storage: &StorageContext,
    roots: &crate::storage::AppDataRoots,
) -> i32 {
    let Some(subcommand) = args.first() else {
        return usage_error("data: expected 'migrate'");
    };
    if subcommand != "migrate" {
        return usage_error(&format!("data: unknown subcommand: {subcommand}"));
    }

    let mut from_appdata = false;
    let mut dry_run = false;
    let mut decline = false;
    let mut json_output = false;
    for argument in &args[1..] {
        match argument.as_str() {
            "--from-appdata" => from_appdata = true,
            "--dry-run" => dry_run = true,
            "--decline" => decline = true,
            "--json" => json_output = true,
            other => return usage_error(&format!("data migrate: unexpected argument: {other}")),
        }
    }
    if !from_appdata {
        return usage_error("data migrate: --from-appdata is required; there is no implicit source");
    }
    if dry_run && decline {
        return usage_error("data migrate: --dry-run and --decline contradict each other");
    }

    if decline {
        return match crate::migrate::decline(storage, roots) {
            Ok(record) => {
                if json_output {
                    emit_json(&json!({
                        "declined": true,
                        "root": storage.root().display().to_string(),
                        "record": record,
                    }));
                } else {
                    output::line(&format!(
                        "recorded that the existing AppData configuration will not be imported; \
                         nothing was copied into {}",
                        storage.root().display()
                    ));
                }
                EXIT_OK
            }
            Err(error) => {
                output::error_line(&format!("{}: {error}", PRODUCT_NAME));
                EXIT_USAGE
            }
        };
    }

    match crate::migrate::migrate(storage, roots, dry_run) {
        Ok(report) => {
            if json_output {
                emit_json(&report.describe());
            } else {
                if report.unnecessary {
                    output::line(
                        "this installation already uses AppData; there is nothing to migrate",
                    );
                }
                output::line(&format!("from      {}", report.source_root.display()));
                output::line(&format!("to        {}", report.destination_root.display()));
                for line in &report.recovered {
                    output::line(&format!("recovered {line}"));
                }
                for item in &report.items {
                    output::line(&format!(
                        "{:<9} {}",
                        item.state.as_str(),
                        item.relative
                    ));
                }
                for (path, reason) in &report.skipped {
                    output::line(&format!("skipped   {path} ({reason})"));
                }
                if report.dry_run {
                    output::line("dry run: nothing was copied");
                } else if report.committed {
                    if report.written.is_empty() {
                        output::line("nothing to copy; the destination already matched");
                    } else {
                        output::line(&format!("copied {} file(s)", report.written.len()));
                        output::line("the original AppData files were left untouched");
                    }
                }
            }
            EXIT_OK
        }
        Err(error) => {
            output::error_line(&format!("{}: {error}", PRODUCT_NAME));
            EXIT_USAGE
        }
    }
}

fn handle_icons(args: &[String]) -> i32 {
    match args.first().map(String::as_str) {
        Some("list") => {
            for icon in crate::config::profile::ICON_NAMES {
                output::line(icon);
            }
            EXIT_OK
        }
        Some(other) => usage_error(&format!("icons: unknown subcommand: {other}")),
        None => usage_error("icons: expected 'list'"),
    }
}

/// Ask the resident instance to show a profile flyout.
///
/// The window belongs to the process that owns the icons, so this is a request, not a window this
/// command creates: the answer comes from the instance that will actually show it.
fn handle_flyout(args: &[String], storage: &StorageContext) -> i32 {
    let mut monitor: Option<String> = None;
    let mut index = 0;
    while index < args.len() {
        match args[index].as_str() {
            "--monitor" => {
                index += 1;
                match args.get(index) {
                    Some(value) => monitor = Some(value.clone()),
                    None => return usage_error("flyout: --monitor needs a value"),
                }
            }
            other => return usage_error(&format!("flyout: unexpected argument: {other}")),
        }
        index += 1;
    }
    let params = match &monitor {
        Some(id) => serde_json::json!({ "monitor": id }),
        None => serde_json::Value::Null,
    };
    match crate::runtime::request(storage, "ui.flyout", params) {
        Ok(result) => {
            let opened = result
                .get("opened")
                .and_then(serde_json::Value::as_bool)
                .unwrap_or(false);
            if opened {
                output::line("flyout requested");
                EXIT_OK
            } else {
                output::error_line(&format!("{}: the flyout was not requested", PRODUCT_NAME));
                EXIT_TRANSPORT
            }
        }
        Err(error) => report_client_error(&error),
    }
}

fn handle_repository(args: &[String]) -> i32 {
    match args.first().map(String::as_str) {
        Some("open") => match open_path(std::path::Path::new("https://github.com/hrkcz001/broiler")) {
            Ok(()) => EXIT_OK,
            Err(reason) => usage_error(&reason),
        },
        Some(other) => usage_error(&format!("repository: unknown subcommand: {other}")),
        None => usage_error("repository: expected 'open'"),
    }
}

// ---------------------------------------------------------------------------
// get / set, routed through the isolated DDC worker
// ---------------------------------------------------------------------------

/// Controls reachable from the command line without a profile.
fn control_from_name(name: &str) -> Option<&'static str> {
    match name {
        "brightness" => Some("brightness"),
        "contrast" => Some("contrast"),
        _ => None,
    }
}

fn tray_executable() -> Result<PathBuf, String> {
    let executable = std::env::current_exe()
        .map_err(|error| format!("cannot resolve the current executable: {error}"))?;
    let directory = executable
        .parent()
        .ok_or_else(|| String::from("the current executable has no parent directory"))?;
    Ok(directory.join("broiler-tray.exe"))
}

fn open_supervisor() -> Result<Supervisor, String> {
    let tray = tray_executable()?;
    if !tray.exists() {
        return Err(format!(
            "the DDC worker executable is missing next to this binary: {}",
            tray.display()
        ));
    }
    Ok(Supervisor::new(WorkerCommand::internal(tray)))
}

fn supervisor_exit_code(error: &SupervisorError) -> i32 {
    match error {
        SupervisorError::Device(failure) => match failure.kind.as_str() {
            "unknown_monitor" | "ambiguous" => EXIT_TARGET,
            "unsupported" => EXIT_UNSUPPORTED,
            _ => EXIT_TRANSPORT,
        },
        _ => EXIT_TRANSPORT,
    }
}

fn report_supervisor_error(error: &SupervisorError) -> i32 {
    output::error_line(&format!("{}: {}", PRODUCT_NAME, error.message()));
    supervisor_exit_code(error)
}

fn parse_number(text: &str) -> Option<u64> {
    let trimmed = text.trim();
    match trimmed.strip_prefix("0x").or_else(|| trimmed.strip_prefix("0X")) {
        Some(hex) => u64::from_str_radix(hex, 16).ok(),
        None => trimmed.parse::<u64>().ok(),
    }
}

fn handle_get(args: &[String], storage: &StorageContext) -> i32 {
    let mut control: Option<&'static str> = None;
    let mut monitor: Option<String> = None;
    let mut json_output = false;
    let mut index = 0usize;

    while index < args.len() {
        match args[index].as_str() {
            "--json" => json_output = true,
            "--monitor" => {
                index += 1;
                match args.get(index) {
                    Some(value) => monitor = Some(value.clone()),
                    None => return usage_error("get: --monitor requires an identifier"),
                }
            }
            other if control.is_none() => match control_from_name(other) {
                Some(name) => control = Some(name),
                None => {
                    return usage_error(&format!(
                        "get: `{other}` is not a readable control; brightness and contrast exist"
                    ))
                }
            },
            other => return usage_error(&format!("get: unexpected argument: {other}")),
        }
        index += 1;
    }

    let Some(control) = control else {
        return usage_error("get: a control name is required, for example `broiler get brightness --monitor <id>`");
    };
    let Some(monitor) = monitor else {
        return usage_error("get: --monitor <id> is required; run 'broiler monitors list --json' first");
    };

    // A resident coordinator owns the hardware: route the read through it when it exists.
    match crate::runtime::request(storage, 
        "parameters.get",
        json!({"monitor": monitor, "control": control}),
    ) {
        Ok(result) => {
            let reading = result.get("reading").cloned().unwrap_or(serde_json::Value::Null);
            if json_output {
                emit_json(&json!({
                    "monitor": monitor,
                    "control": control,
                    "value": reading.get("value"),
                    "minimum": reading.get("minimum"),
                    "maximum": reading.get("maximum"),
                    "backend": reading.get("backend"),
                }));
            } else {
                output::line(&format!(
                    "{} {} (min {}, max {}, {})",
                    control,
                    reading.get("value").and_then(serde_json::Value::as_u64).unwrap_or(0),
                    reading
                        .get("minimum")
                        .and_then(serde_json::Value::as_u64)
                        .map(|value| value.to_string())
                        .unwrap_or_else(|| String::from("?")),
                    reading
                        .get("maximum")
                        .and_then(serde_json::Value::as_u64)
                        .map(|value| value.to_string())
                        .unwrap_or_else(|| String::from("?")),
                    reading
                        .get("backend")
                        .and_then(serde_json::Value::as_str)
                        .unwrap_or("unknown")
                ));
            }
            return EXIT_OK;
        }
        Err(crate::runtime::ClientError::NotRunning) => {}
        Err(error) => return report_client_error(&error),
    }

    let mut supervisor = match open_supervisor() {
        Ok(supervisor) => supervisor,
        Err(reason) => return usage_error(&reason),
    };

    match supervisor.request("read", json!({"monitor": monitor, "control": control})) {
        Ok(result) => {
            if json_output {
                emit_json(&json!({
                    "monitor": monitor,
                    "control": control,
                    "value": result.get("value"),
                    "minimum": result.get("minimum"),
                    "maximum": result.get("maximum"),
                    "backend": result.get("backend"),
                }));
            } else {
                let value = result.get("value").and_then(serde_json::Value::as_u64).unwrap_or(0);
                let minimum = result
                    .get("minimum")
                    .and_then(serde_json::Value::as_u64)
                    .map(|value| value.to_string())
                    .unwrap_or_else(|| String::from("?"));
                let maximum = result
                    .get("maximum")
                    .and_then(serde_json::Value::as_u64)
                    .map(|value| value.to_string())
                    .unwrap_or_else(|| String::from("?"));
                let backend = result
                    .get("backend")
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or("unknown");
                output::line(&format!(
                    "{control} {value} (min {minimum}, max {maximum}, {backend})"
                ));
            }
            EXIT_OK
        }
        Err(error) => report_supervisor_error(&error),
    }
}

fn handle_set(args: &[String], storage: &StorageContext) -> i32 {
    let mut control: Option<&'static str> = None;
    let mut raw_value: Option<String> = None;
    let mut monitor: Option<String> = None;
    let mut raw = false;
    let mut wait = false;
    let mut json_output = false;
    let mut index = 0usize;

    while index < args.len() {
        match args[index].as_str() {
            "--raw" => raw = true,
            "--wait" => wait = true,
            "--json" => json_output = true,
            "--monitor" => {
                index += 1;
                match args.get(index) {
                    Some(value) => monitor = Some(value.clone()),
                    None => return usage_error("set: --monitor requires an identifier"),
                }
            }
            other if control.is_none() => match control_from_name(other) {
                Some(name) => control = Some(name),
                None => {
                    return usage_error(&format!(
                        "set: `{other}` is not a writable control; brightness and contrast exist"
                    ))
                }
            },
            other if raw_value.is_none() => raw_value = Some(other.to_string()),
            other => return usage_error(&format!("set: unexpected argument: {other}")),
        }
        index += 1;
    }

    let Some(control) = control else {
        return usage_error("set: a control name is required, for example `broiler set brightness 42 --monitor <id>`");
    };
    let Some(value_text) = raw_value else {
        return usage_error("set: a value is required");
    };
    let Some(monitor) = monitor else {
        return usage_error("set: --monitor <id> is required; multi-monitor broadcasts are not implicit");
    };
    let Some(value) = parse_number(&value_text) else {
        return usage_error(&format!("set: `{value_text}` is not a number"));
    };

    // A resident coordinator owns the hardware: route the write through it when it exists.
    let unit = if raw { "native" } else { "percent" };
    match crate::runtime::request(storage, 
        "parameters.set",
        json!({"monitor": monitor, "control": control, "value": value, "unit": unit}),
    ) {
        Ok(result) => {
            let observed = result
                .get("observed")
                .and_then(serde_json::Value::as_u64)
                .unwrap_or(u64::MAX);
            if json_output {
                emit_json(&json!({
                    "monitor": monitor,
                    "control": control,
                    "requested": value,
                    "unit": unit,
                    "native_target": result.get("native_target"),
                    "observed": observed,
                    "verified": result.get("verified"),
                    "backend": result.get("backend"),
                    "waited": wait,
                    "via": "coordinator",
                }));
            } else {
                output::line(&format!(
                    "{control} set to {observed} and verified by readback ({}) via the coordinator",
                    result
                        .get("backend")
                        .and_then(serde_json::Value::as_str)
                        .unwrap_or("unknown")
                ));
            }
            return EXIT_OK;
        }
        Err(crate::runtime::ClientError::NotRunning) => {}
        Err(error) => return report_client_error(&error),
    }

    let mut supervisor = match open_supervisor() {
        Ok(supervisor) => supervisor,
        Err(reason) => return usage_error(&reason),
    };

    // Read first: a percentage needs the device's own range, and the range must not be assumed.
    let reading = match supervisor.request("read", json!({"monitor": monitor, "control": control})) {
        Ok(result) => result,
        Err(error) => return report_supervisor_error(&error),
    };
    let minimum = reading.get("minimum").and_then(serde_json::Value::as_u64);
    let maximum = reading.get("maximum").and_then(serde_json::Value::as_u64).unwrap_or(0);

    let target = if raw {
        if let Some(maximum) = reading.get("maximum").and_then(serde_json::Value::as_u64) {
            let minimum = minimum.unwrap_or(0);
            if value < minimum || value > maximum {
                return usage_error(&format!(
                    "set: native value {value} is outside the reported range {minimum}..={maximum}; nothing was written"
                ));
            }
        }
        value
    } else {
        if value > 100 {
            return usage_error(&format!(
                "set: {value} is not a percentage; percentages range 0..=100, use --raw for a native value"
            ));
        }
        match minimum {
            Some(minimum) => percent_to_native(value as u32, minimum, maximum),
            None => {
                return usage_error(
                    "set: the display reported no minimum for this control, so a percentage cannot be mapped; use --raw",
                )
            }
        }
    };

    match supervisor.request(
        "write",
        json!({"monitor": monitor, "control": control, "value": target}),
    ) {
        Ok(result) => {
            let observed = result
                .get("observed")
                .and_then(serde_json::Value::as_u64)
                .unwrap_or(u64::MAX);
            let backend = result
                .get("write_backend")
                .and_then(serde_json::Value::as_str)
                .unwrap_or("unknown");

            if json_output {
                emit_json(&json!({
                    "monitor": monitor,
                    "control": control,
                    "requested": value,
                    "unit": if raw { "native" } else { "percent" },
                    "native_target": target,
                    "observed": observed,
                    "verified": observed == target,
                    "backend": backend,
                    "waited": wait,
                }));
            }

            if observed == target {
                if !json_output {
                    output::line(&format!(
                        "{control} set to {target} and verified by readback ({backend})"
                    ));
                }
                EXIT_OK
            } else {
                output::error_line(&format!(
                    "{}: {control} readback mismatch: wrote {target}, device reports {observed}",
                    PRODUCT_NAME
                ));
                EXIT_TRANSPORT
            }
        }
        Err(error) => report_supervisor_error(&error),
    }
}

// ---------------------------------------------------------------------------
// monitors
// ---------------------------------------------------------------------------

fn usage_error(message: &str) -> i32 {
    output::error_line(&format!("{}: {}", PRODUCT_NAME, message));
    EXIT_USAGE
}

fn emit_json<T: Serialize>(value: &T) {
    match serde_json::to_string_pretty(value) {
        Ok(text) => output::line(&text),
        Err(error) => output::error_line(&format!(
            "{}: cannot encode the report as JSON: {}",
            PRODUCT_NAME, error
        )),
    }
}

fn discovery_exit_code(failure: &DiscoveryFailure) -> i32 {
    match failure {
        DiscoveryFailure::Topology(_) => EXIT_TRANSPORT,
        DiscoveryFailure::UnknownMonitor { .. } | DiscoveryFailure::AmbiguousMonitor { .. } => {
            EXIT_TARGET
        }
    }
}

fn reading_summary(reading: &Reading) -> String {
    match reading {
        Reading::Readable {
            backend,
            current,
            maximum,
            minimum,
            ..
        } => {
            let min = minimum
                .map(|value| value.to_string())
                .unwrap_or_else(|| String::from("?"));
            let max = maximum
                .map(|value| value.to_string())
                .unwrap_or_else(|| String::from("?"));
            format!("{current} (min {min}, max {max}, {backend})")
        }
        Reading::Timeout { waited_ms, .. } => format!("timeout after {waited_ms} ms"),
        Reading::TransportError { message, .. } => format!("transport error: {message}"),
        Reading::Unavailable { message } => format!("unavailable: {message}"),
        Reading::Ambiguous { count, .. } => {
            format!("ambiguous: {count} physical monitors on one handle")
        }
        Reading::NotRequested => String::from("not read"),
    }
}

fn print_monitor_list(report: &MonitorListReport) {
    output::line(&format!(
        "backend {}, observed {}, {} ms",
        report.backend, report.observed_at, report.elapsed_ms
    ));
    if report.monitors.is_empty() {
        output::line("no controllable external monitors");
    }
    for monitor in &report.monitors {
        let role = if monitor.primary { "primary" } else { "extended" };
        let control = if monitor.controllable {
            "controllable"
        } else {
            "not controllable"
        };
        output::line(&format!(
            "{}  {}  {}  {}  {}  {}",
            monitor.display_index,
            monitor.id,
            monitor.friendly_name,
            monitor.connector,
            role,
            control
        ));
        if let Some(probe) = monitor.probe.as_ref() {
            output::line(&format!(
                "   brightness {}   contrast {}   ({} ms)",
                reading_summary(&probe.brightness),
                reading_summary(&probe.contrast),
                probe.elapsed_ms
            ));
        }
        output::line(&format!(
            "   identity {}  device {}",
            match monitor.identity_confidence {
                crate::model::IdentityConfidence::EdidSerial => "edid_serial",
                crate::model::IdentityConfidence::ConnectionBound => "connection_bound",
                crate::model::IdentityConfidence::Ambiguous => "ambiguous",
            },
            monitor.device_name
        ));
    }
    for excluded in &report.excluded {
        output::line(&format!(
            "excluded  {}  {}  {}  ({})",
            excluded.friendly_name, excluded.connector, excluded.reason, excluded.detail
        ));
    }
    for note in &report.notes {
        output::line(&format!("note: {note}"));
    }
}

fn print_control_evidence(control: &ControlEvidence) {
    let advertised = match control.advertised {
        Some(true) => "advertised",
        Some(false) => "not advertised",
        None => "no capability data",
    };
    output::line(&format!(
        "  {:<20} {}  {:<18} {}",
        control.name,
        control.vcp_code,
        advertised,
        reading_summary(&control.reading)
    ));
}

fn print_capabilities(report: &CapabilitiesReport) {
    output::line(&format!(
        "monitor   {}  ({})",
        report.monitor.friendly_name, report.monitor.id
    ));
    output::line(&format!(
        "connector {}  device {}  index {}",
        report.monitor.connector, report.monitor.device_name, report.monitor.display_index
    ));
    output::line(&format!("observed  {}  backend {}", report.observed_at, report.backend));

    let section = &report.capabilities;
    let declared = section
        .declared_length
        .map(|value| format!("{value} chars"))
        .unwrap_or_else(|| String::from("unknown length"));
    let version = section
        .parsed
        .as_ref()
        .and_then(|parsed| parsed.mccs_version.clone())
        .unwrap_or_else(|| String::from("not reported"));
    output::line(&format!(
        "capabilities {}  {}  mccs {}",
        section.state, declared, version
    ));
    if let Some(message) = section.message.as_ref() {
        output::line(&format!("  {message}"));
    }
    if let Some(parsed) = section.parsed.as_ref() {
        let commands: Vec<String> = parsed
            .commands
            .iter()
            .map(|code| format!("{code:02X}"))
            .collect();
        output::line(&format!("  cmds {}", commands.join(" ")));
    }

    output::line("controls");
    for control in &report.controls {
        print_control_evidence(control);
    }
    for note in &report.notes {
        output::line(&format!("note: {note}"));
    }
}

fn monitors_list(args: &[String]) -> i32 {
    let mut json = false;
    for arg in args {
        match arg.as_str() {
            "--json" => json = true,
            other => return usage_error(&format!("monitors list: unexpected argument: {other}")),
        }
    }

    match discovery::list() {
        Ok(report) => {
            if json {
                emit_json(&report);
            } else {
                print_monitor_list(&report);
            }
            EXIT_OK
        }
        Err(failure) => {
            output::error_line(&format!("{}: {}", PRODUCT_NAME, failure.message()));
            discovery_exit_code(&failure)
        }
    }
}

fn monitors_capabilities(args: &[String]) -> i32 {
    let mut json = false;
    let mut monitor: Option<String> = None;
    let mut index = 0usize;

    while index < args.len() {
        match args[index].as_str() {
            "--json" => json = true,
            "--monitor" => {
                index += 1;
                match args.get(index) {
                    Some(value) => monitor = Some(value.clone()),
                    None => return usage_error("monitors capabilities: --monitor requires an identifier"),
                }
            }
            other => {
                return usage_error(&format!(
                    "monitors capabilities: unexpected argument: {other}"
                ))
            }
        }
        index += 1;
    }

    let Some(monitor) = monitor else {
        return usage_error(
            "monitors capabilities: --monitor <id> is required; run 'broiler monitors list --json' first",
        );
    };

    match discovery::capabilities(&monitor) {
        Ok(report) => {
            if json {
                emit_json(&report);
            } else {
                print_capabilities(&report);
            }
            EXIT_OK
        }
        Err(failure) => {
            output::error_line(&format!("{}: {}", PRODUCT_NAME, failure.message()));
            discovery_exit_code(&failure)
        }
    }
}

fn monitors_select(args: &[String], storage: &StorageContext) -> i32 {
    let mut ids: Vec<String> = Vec::new();
    let mut remember = false;
    let mut none = false;
    let mut dialog = false;
    let mut index = 0usize;

    while index < args.len() {
        match args[index].as_str() {
            "--monitor" => {
                index += 1;
                match args.get(index) {
                    Some(value) => ids.push(value.clone()),
                    None => return usage_error("monitors select: --monitor requires an identifier"),
                }
            }
            "--remember" => remember = true,
            "--none" => none = true,
            "--dialog" => dialog = true,
            other => return usage_error(&format!("monitors select: unexpected argument: {other}")),
        }
        index += 1;
    }

    if dialog {
        if !ids.is_empty() || none {
            return usage_error(
                "monitors select: --dialog cannot be combined with --monitor or --none",
            );
        }
        return select_with_dialog(storage, remember);
    }
    if none && !ids.is_empty() {
        return usage_error("monitors select: --none cannot be combined with --monitor");
    }
    if !none && ids.is_empty() {
        return usage_error("monitors select: choose --monitor <id> (repeatable) or --none");
    }

    match crate::runtime::request(storage, 
        "monitors.select",
        json!({"monitor_ids": ids, "remember": remember}),
    ) {
        Ok(result) => {
            emit_json(&result);
            EXIT_OK
        }
        Err(error) => report_client_error(&error),
    }
}

fn monitors_validate_value(args: &[String], storage: &StorageContext) -> i32 {
    let mut parameter: Option<&'static str> = None;
    let mut value: Option<u64> = None;
    let mut vcp: Option<u64> = None;
    let mut monitor: Option<String> = None;
    let mut confirm_write = false;
    let mut index = 0usize;

    while index < args.len() {
        match args[index].as_str() {
            "--confirm-write" => confirm_write = true,
            "--vcp" => {
                index += 1;
                match args.get(index).and_then(|text| parse_number(text)) {
                    Some(code) => vcp = Some(code),
                    None => return usage_error("monitors validate-value: --vcp requires a code"),
                }
            }
            "--monitor" => {
                index += 1;
                match args.get(index) {
                    Some(value) => monitor = Some(value.clone()),
                    None => {
                        return usage_error("monitors validate-value: --monitor requires an identifier")
                    }
                }
            }
            other if parameter.is_none() => match control_from_name(other) {
                Some(name) => parameter = Some(name),
                None => {
                    return usage_error(&format!(
                        "monitors validate-value: `{other}` has no supervised validation path in this build; brightness and contrast exist"
                    ))
                }
            },
            other if value.is_none() => match parse_number(other) {
                Some(parsed) => value = Some(parsed),
                None => return usage_error(&format!("monitors validate-value: `{other}` is not a number")),
            },
            other => {
                return usage_error(&format!(
                    "monitors validate-value: unexpected argument: {other}"
                ))
            }
        }
        index += 1;
    }

    let Some(parameter) = parameter else {
        return usage_error("monitors validate-value: a control name is required");
    };
    let Some(value) = value else {
        return usage_error("monitors validate-value: a native value is required");
    };
    let Some(monitor) = monitor else {
        return usage_error("monitors validate-value: --monitor <id> is required");
    };
    if !confirm_write {
        return usage_error(
            "monitors validate-value: this performs one real write and requires --confirm-write",
        );
    }

    // The code must be the one this control actually uses; no arbitrary VCP access exists.
    let expected = match parameter {
        "brightness" => 0x10u64,
        "contrast" => 0x12u64,
        _ => 0,
    };
    if let Some(code) = vcp {
        if code != expected {
            output::error_line(&format!(
                "{}: {} is carried by 0x{expected:02X}, not 0x{code:02X}; no write was made",
                PRODUCT_NAME, parameter
            ));
            return EXIT_UNSUPPORTED;
        }
    }

    match crate::runtime::request(storage, 
        "monitors.validate_value",
        json!({
            "monitor": monitor,
            "control": parameter,
            "value": value,
            "confirm_write": true,
        }),
    ) {
        Ok(result) => {
            emit_json(&result);
            if result.get("verified").and_then(serde_json::Value::as_bool) == Some(true) {
                EXIT_OK
            } else {
                EXIT_TRANSPORT
            }
        }
        Err(error) => report_client_error(&error),
    }
}

fn handle_monitors(args: &[String], storage: &StorageContext) -> i32 {
    match args.first().map(String::as_str) {
        Some("list") => monitors_list(&args[1..]),
        Some("capabilities") => monitors_capabilities(&args[1..]),
        Some("select") => monitors_select(&args[1..], storage),
        Some("validate-value") => monitors_validate_value(&args[1..], storage),
        Some(other) => usage_error(&format!("monitors: unknown subcommand: {other}")),
        None => usage_error(
            "monitors: expected 'list', 'capabilities', 'select' or 'validate-value'",
        ),
    }
}

fn dispatch_tray(args: &[String], storage: &StorageContext) -> i32 {
    match args.first().map(String::as_str) {
        None => {
            init_logging(storage);
            crate::runtime::run_resident(storage.clone())
        }
        Some("--version") | Some("-V") | Some("version") => {
            print_version();
            EXIT_OK
        }
        Some("--help") | Some("-h") | Some("help") => {
            output::line(&help_text());
            EXIT_OK
        }
        Some("--internal-ddc-worker") => {
            // The worker mode belongs to the tray executable, which enters it before this module is
            // reached; it performs no file I/O, so it receives no data root and can never create a
            // second copy of the user's configuration. Reaching this arm means the console binary was
            // asked for it, which is a usage error rather than a missing feature.
            output::error_line(&format!(
                "{}: --internal-ddc-worker is the tray executable's private worker mode, not a {} command",
                PRODUCT_NAME, PRODUCT_NAME
            ));
            EXIT_USAGE
        }
        Some(other) => {
            output::error_line(&format!("{}: unknown option: {}", PRODUCT_NAME, other));
            EXIT_USAGE
        }
    }
}

/// Split a global `--config-dir <path>` off the argument list.
///
/// The root is resolved here, once, and every later step receives the absolute result. A second
/// `--config-dir` is a usage error rather than a silent last-one-wins.
fn take_config_dir(args: &[String]) -> Result<(Option<PathBuf>, Vec<String>), String> {
    let mut explicit: Option<PathBuf> = None;
    let mut rest: Vec<String> = Vec::new();
    let mut index = 0;
    while index < args.len() {
        match args[index].as_str() {
            "--config-dir" => {
                if explicit.is_some() {
                    return Err(String::from("--config-dir was given more than once"));
                }
                let Some(value) = args.get(index + 1) else {
                    return Err(String::from("--config-dir requires a path"));
                };
                explicit = Some(PathBuf::from(value));
                index += 2;
            }
            other => {
                rest.push(other.to_string());
                index += 1;
            }
        }
    }
    Ok((explicit, rest))
}

/// Entry point shared by both binaries.
pub fn run(program: Program, args: &[String]) -> i32 {
    let (explicit, rest) = match take_config_dir(args) {
        Ok(parsed) => parsed,
        Err(reason) => return usage_error(&reason),
    };
    let storage = match crate::storage::resolve(explicit.as_deref()) {
        Ok(storage) => storage,
        Err(error) => {
            // No root means no work: an unmarked installation whose known folders are unavailable
            // cannot guess one, and it must never invent a location.
            output::error_line(&format!("{}: {error}", PRODUCT_NAME));
            return EXIT_USAGE;
        }
    };

    match program {
        Program::Console => dispatch_console(&rest, &storage),
        Program::Tray => {
            // A GUI-subsystem process has no console unless one is inherited.
            output::attach_console_if_needed();
            dispatch_tray(&rest, &storage)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::OnceLock;

    fn argv(items: &[&str]) -> Vec<String> {
        items.iter().map(|s| s.to_string()).collect()
    }

    /// One isolated data root for the whole test binary.
    ///
    /// No test may read, write or depend on the developer's real AppData directory, and no test may
    /// depend on a user's running coordinator.
    fn test_root() -> &'static StorageContext {
        static STORAGE: OnceLock<StorageContext> = OnceLock::new();
        STORAGE.get_or_init(|| {
            let mut base = std::env::temp_dir();
            base.push(format!("broiler-cli-{}-storage", std::process::id()));
            StorageContext::for_test(&base)
        })
    }

    // The handlers take the resolved root explicitly; these local wrappers keep each test focused on
    // the command it exercises while still routing through the isolated root.
    /// An isolated root and its base directory, for tests that write settings.
    fn fresh(name: &str) -> (StorageContext, PathBuf) {
        let mut base = std::env::temp_dir();
        base.push(format!("broiler-cli-{}-{}", std::process::id(), name));
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(&base).expect("scratch");
        (StorageContext::for_test(&base.join("data")), base)
    }

    /// The fixture endpoint every CLI test talks to.
    ///
    /// A CLI command that needs a coordinator asks one on this namespace, which no process outside
    /// the test binary can name. Without it, a resident instance the developer is running would answer
    /// with its own root and turn an expected `daemon unavailable` into a `storage_conflict`.
    fn endpoint_namespace() -> &'static str {
        static NAME: OnceLock<String> = OnceLock::new();
        NAME.get_or_init(|| crate::pipe::fixture::stable("cli"))
    }

    fn dispatch_console(args: &[String]) -> i32 {
        crate::pipe::fixture::with_namespace(endpoint_namespace(), || {
            super::dispatch_console(args, test_root())
        })
    }

    fn dispatch_tray(args: &[String]) -> i32 {
        crate::pipe::fixture::with_namespace(endpoint_namespace(), || {
            super::dispatch_tray(args, test_root())
        })
    }

    #[test]
    fn version_is_reported_for_the_console_program() {
        assert_eq!(dispatch_console(&argv(&["--version"])), EXIT_OK);
    }

    #[test]
    fn help_is_success_for_both_programs() {
        assert_eq!(dispatch_console(&argv(&["--help"])), EXIT_OK);
        assert_eq!(dispatch_tray(&argv(&["--help"])), EXIT_OK);
    }

    #[test]
    fn the_help_does_not_deny_a_feature_this_build_implements() {
        // The regression: the help screen described tray icons, the flyout and the settings window as
        // unimplemented long after they shipped, so the owner read a false status from the product.
        let text = super::help_text();
        for implemented in [
            "tray",
            "flyout",
            "settings window",
            "left-click",
            "monitors select",
        ] {
            assert!(
                text.contains(implemented),
                "the help must mention `{implemented}`, which exists"
            );
            assert!(
                !text.contains(&format!("not implemented in this build: {implemented}")),
                "the help must not deny `{implemented}`"
            );
        }
        // And it must still name what is genuinely missing, instead of implying a complete build.
        assert!(text.contains("colour temperature"));
        assert!(text.contains("launch-at-logon"));
    }

    #[test]
    fn no_arguments_is_a_usage_error_for_the_console_program() {
        assert_eq!(dispatch_console(&[]), EXIT_USAGE);
    }

    #[test]
    fn the_flyout_command_asks_a_running_instance_and_says_so_when_there_is_none() {
        // No coordinator runs in the test process, so this reports the unavailable exit code and
        // never creates a window of its own.
        assert_eq!(dispatch_console(&argv(&["flyout"])), EXIT_DAEMON_UNAVAILABLE);
        assert_eq!(
            dispatch_console(&argv(&["flyout", "--monitor", "mon-a"])),
            EXIT_DAEMON_UNAVAILABLE
        );
        // A missing value is a usage error, not a request about an empty monitor.
        assert_eq!(dispatch_console(&argv(&["flyout", "--monitor"])), EXIT_USAGE);
        assert_eq!(dispatch_console(&argv(&["flyout", "--nonsense"])), EXIT_USAGE);
    }

    #[test]
    fn daemon_only_commands_report_the_unavailable_exit_code_without_a_coordinator() {
        // No coordinator runs in the test process, so these must be exit 3 and never touch hardware.
        assert_eq!(dispatch_console(&argv(&["reload"])), EXIT_DAEMON_UNAVAILABLE);
        assert_eq!(
            dispatch_console(&argv(&["profile", "apply", "day", "--monitor", "mon-a"])),
            EXIT_DAEMON_UNAVAILABLE
        );
        assert_eq!(
            dispatch_console(&argv(&["startup", "status"])),
            EXIT_DAEMON_UNAVAILABLE
        );
    }

    #[test]
    fn monitors_subcommands_reject_bad_arguments_without_touching_hardware() {
        assert_eq!(dispatch_console(&argv(&["monitors"])), EXIT_USAGE);
        assert_eq!(dispatch_console(&argv(&["monitors", "frobnicate"])), EXIT_USAGE);
        assert_eq!(
            dispatch_console(&argv(&["monitors", "capabilities"])),
            EXIT_USAGE
        );
        assert_eq!(
            dispatch_console(&argv(&["monitors", "capabilities", "--monitor"])),
            EXIT_USAGE
        );
        assert_eq!(
            dispatch_console(&argv(&["monitors", "list", "--bogus"])),
            EXIT_USAGE
        );
    }

    #[test]
    fn unknown_command_is_a_usage_error() {
        assert_eq!(dispatch_console(&argv(&["frobnicate"])), EXIT_USAGE);
        assert_eq!(dispatch_tray(&argv(&["--frobnicate"])), EXIT_USAGE);
    }

    #[test]
    fn exit_codes_match_the_documented_contract() {
        assert_eq!(EXIT_OK, 0);
        assert_eq!(EXIT_USAGE, 2);
        assert_eq!(EXIT_DAEMON_UNAVAILABLE, 3);
        assert_eq!(EXIT_TARGET, 4);
        assert_eq!(EXIT_UNSUPPORTED, 5);
        assert_eq!(EXIT_TRANSPORT, 6);
        assert_eq!(EXIT_PARTIAL, 7);
    }

    #[test]
    fn paths_describes_the_resolved_root_without_a_coordinator() {
        assert_eq!(dispatch_console(&argv(&["paths"])), EXIT_OK);
        assert_eq!(dispatch_console(&argv(&["paths", "--json"])), EXIT_OK);
        assert_eq!(dispatch_console(&argv(&["paths", "--bogus"])), EXIT_USAGE);
    }

    #[test]
    fn the_config_dir_option_is_taken_once_and_removed_from_the_arguments() {
        let parsed = take_config_dir(&argv(&["--config-dir", "C:\\chosen", "paths", "--json"]))
            .expect("parsed");
        assert_eq!(parsed.0, Some(PathBuf::from("C:\\chosen")));
        assert_eq!(parsed.1, argv(&["paths", "--json"]));

        assert!(take_config_dir(&argv(&["--config-dir"])).is_err());
        assert!(take_config_dir(&argv(&[
            "--config-dir",
            "a",
            "--config-dir",
            "b"
        ]))
        .is_err());
        assert_eq!(
            take_config_dir(&argv(&["paths"])).expect("parsed"),
            (None, argv(&["paths"]))
        );
    }

    #[test]
    fn a_confirmed_selection_is_recorded_without_touching_the_other_settings() {
        let (storage, base) = fresh("select-record");
        std::fs::create_dir_all(storage.root()).expect("root");
        std::fs::write(
            storage.settings_file(),
            "# hand written\nschema_version = 1\n\n[tray]\nleft_click = \"cycle\"\n\n[selection]\nremember = false\nmonitor_ids = []\n",
        )
        .expect("settings");

        assert_eq!(
            record_selection(&storage, &[String::from("mon-a"), String::from("mon-b")], true),
            EXIT_OK
        );

        let text = std::fs::read_to_string(storage.settings_file()).expect("settings");
        assert!(text.starts_with("# hand written\n"), "{text}");
        assert!(text.contains("left_click = \"cycle\""), "{text}");
        let settings = crate::config::settings::load(&storage.settings_file()).expect("reload");
        assert_eq!(
            settings.monitor_ids,
            vec![String::from("mon-a"), String::from("mon-b")]
        );
        assert!(settings.remember);
        let _ = std::fs::remove_dir_all(base);
    }

    #[test]
    fn an_empty_confirmed_selection_is_recorded_as_empty() {
        let (storage, base) = fresh("select-empty");
        std::fs::create_dir_all(storage.root()).expect("root");
        std::fs::write(
            storage.settings_file(),
            "schema_version = 1\n\n[selection]\nremember = true\nmonitor_ids = [\"mon-a\"]\n",
        )
        .expect("settings");

        assert_eq!(record_selection(&storage, &[], true), EXIT_OK);
        let settings = crate::config::settings::load(&storage.settings_file()).expect("reload");
        assert!(settings.monitor_ids.is_empty());
        let _ = std::fs::remove_dir_all(base);
    }

    #[test]
    fn data_migrate_requires_an_explicit_source_and_consistent_flags() {
        assert_eq!(dispatch_console(&argv(&["data"])), EXIT_USAGE);
        assert_eq!(dispatch_console(&argv(&["data", "frobnicate"])), EXIT_USAGE);
        assert_eq!(dispatch_console(&argv(&["data", "migrate"])), EXIT_USAGE);
        assert_eq!(
            dispatch_console(&argv(&[
                "data",
                "migrate",
                "--from-appdata",
                "--dry-run",
                "--decline"
            ])),
            EXIT_USAGE
        );
        assert_eq!(
            dispatch_console(&argv(&["data", "migrate", "--from-appdata", "--bogus"])),
            EXIT_USAGE
        );
    }

    /// A disposable AppData source and a disposable portable destination.
    struct Migration {
        base: PathBuf,
        roots: crate::storage::AppDataRoots,
        storage: StorageContext,
    }

    impl Drop for Migration {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.base);
        }
    }

    fn migration(name: &str) -> Migration {
        let mut base = std::env::temp_dir();
        base.push(format!("broiler-cli-migrate-{}-{}", std::process::id(), name));
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(&base).expect("scratch");
        let roots = crate::storage::AppDataRoots {
            roaming: base.join("Roaming"),
            local: base.join("Local"),
        };
        let storage = StorageContext::for_test(&base.join("app").join("data"));
        Migration {
            base,
            roots,
            storage,
        }
    }

    impl Migration {
        fn source_file(&self, relative: &str, text: &str) {
            let source = crate::storage::appdata_of(&self.roots);
            let path = source
                .root()
                .join(relative.replace('/', &std::path::MAIN_SEPARATOR.to_string()));
            std::fs::create_dir_all(path.parent().expect("parent")).expect("source dir");
            std::fs::write(path, text).expect("source file");
        }
    }

    #[test]
    fn data_migrate_dry_run_then_copy_then_decline_through_the_command_surface() {
        let fixture = migration("surface");
        fixture.source_file(
            "settings.toml",
            "# hand written\nschema_version = 1\n\n[tray]\nleft_click = \"cycle\"\n",
        );
        fixture.source_file(
            "profiles/night.toml",
            "schema_version = 1\nid = \"night\"\nname = \"Night\"\nicon = \"night\"\n\n[monitor]\nbrightness = 0\n",
        );

        // Dry run: reports the work, changes nothing.
        assert_eq!(
            run_data(
                &argv(&["migrate", "--from-appdata", "--dry-run"]),
                &fixture.storage,
                &fixture.roots
            ),
            EXIT_OK
        );
        assert!(!fixture.storage.settings_file().exists());

        // The real copy, through the same command surface.
        assert_eq!(
            run_data(
                &argv(&["migrate", "--from-appdata"]),
                &fixture.storage,
                &fixture.roots
            ),
            EXIT_OK
        );
        assert!(std::fs::read_to_string(fixture.storage.settings_file())
            .expect("settings")
            .starts_with("# hand written\n"));
        // The original AppData files are still there.
        let source = crate::storage::appdata_of(&fixture.roots);
        assert!(source.settings_file().exists());

        // A second run is a no-op, and declining after an import is refused rather than recorded.
        assert_eq!(
            run_data(
                &argv(&["migrate", "--from-appdata"]),
                &fixture.storage,
                &fixture.roots
            ),
            EXIT_OK
        );
        assert_eq!(
            run_data(
                &argv(&["migrate", "--from-appdata", "--decline"]),
                &fixture.storage,
                &fixture.roots
            ),
            EXIT_USAGE
        );
    }

    #[test]
    fn data_migrate_declines_explicitly_without_copying() {
        let fixture = migration("decline");
        fixture.source_file("settings.toml", "schema_version = 1\n");

        assert_eq!(
            run_data(
                &argv(&["migrate", "--from-appdata", "--decline"]),
                &fixture.storage,
                &fixture.roots
            ),
            EXIT_OK
        );
        assert!(!fixture.storage.settings_file().exists());
        let record = crate::migrate::recorded_decision(&fixture.storage).expect("record");
        assert_eq!(record.appdata_import, "declined");
    }

    #[test]
    fn data_migrate_stops_on_a_conflicting_destination() {
        let fixture = migration("conflict");
        fixture.source_file("settings.toml", "schema_version = 1\n\n[tray]\nleft_click = \"cycle\"\n");
        std::fs::create_dir_all(fixture.storage.root()).expect("root");
        std::fs::write(fixture.storage.settings_file(), "different\n").expect("conflict");

        assert_eq!(
            run_data(
                &argv(&["migrate", "--from-appdata"]),
                &fixture.storage,
                &fixture.roots
            ),
            EXIT_USAGE
        );
        assert_eq!(
            std::fs::read_to_string(fixture.storage.settings_file()).expect("kept"),
            "different\n"
        );
    }

    #[test]
    fn a_failed_apply_maps_its_typed_cause_to_an_exit_code() {
        let result = |status: &str, cause: &str| {
            json!({"profile": "day", "status": status, "cause": cause, "monitors": []})
        };
        assert_eq!(report_apply(&result("applied", ""), true), EXIT_OK);
        assert_eq!(report_apply(&result("partial", "timeout"), true), EXIT_PARTIAL);
        assert_eq!(
            report_apply(&result("not_applied", "unsupported"), true),
            EXIT_UNSUPPORTED
        );
        assert_eq!(
            report_apply(&result("not_applied", "not_implemented"), true),
            EXIT_UNSUPPORTED
        );
        assert_eq!(
            report_apply(&result("not_applied", "invalid_value"), true),
            EXIT_USAGE
        );
        assert_eq!(
            report_apply(&result("not_applied", "timeout"), true),
            EXIT_TRANSPORT
        );
        assert_eq!(
            report_apply(&result("not_applied", "verification_failed"), true),
            EXIT_TRANSPORT
        );
        // An undocumented cause must still not look like success.
        assert_eq!(report_apply(&result("not_applied", ""), true), EXIT_TRANSPORT);
    }
}
