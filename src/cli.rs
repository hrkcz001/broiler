//! Command dispatch for both executables.
//!
//! The G1 skeleton owns argument dispatch, the version/help surface, exit codes and the
//! logger bootstrap. Every hardware or configuration command is listed in the help text
//! because it is part of the user-facing contract, and reports that it is not implemented
//! in this build instead of pretending to succeed.

use std::path::PathBuf;

use serde::Serialize;
use serde_json::json;

use crate::config::value::percent_to_native;
use crate::discovery::{self, DiscoveryFailure};
use crate::logging::{self, Level};
use crate::model::{CapabilitiesReport, ControlEvidence, MonitorListReport, Reading};
use crate::supervisor::{Supervisor, SupervisorError, WorkerCommand};
use crate::{log_info, output, paths, PRODUCT_NAME, VERSION};

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

/// Commands that belong to the v1 contract but are not implemented in this build.
const PLANNED_COMMANDS: &[&str] = &["flyout"];

fn help_text() -> String {
    format!(
        "{name} {version} - native Windows DDC/CI monitor controller\n\
         \n\
         Usage:\n\
         \x20 {name} <command> [options]\n\
         \n\
         Commands defined by the v1 contract:\n\
         \x20 start | stop | status [--json] [--refresh] | reload\n\
         \x20 monitors list [--json] | monitors capabilities --monitor <id> [--json]\n\
         \x20 monitors select --monitor <id> [--remember] | monitors select --none | --dialog\n\
         \x20 monitors validate-value <parameter> <value> --vcp <code> --monitor <id> --confirm-write\n\
         \x20 profiles list [--json] | profiles validate | profiles open\n\
         \x20 profile apply <id> (--monitor <id> | --all-selected) [--wait] | profile next --monitor <id>\n\
         \x20 get <parameter> --monitor <id> | set <parameter> <value> --monitor <id> [--wait]\n\
         \x20 settings get [key] [--json] | settings set <key> <value> | settings open\n\
         \x20 startup status | repository open | icons list | flyout --monitor <id>\n\
         \n\
         General:\n\
         \x20 --version, -V   print the version\n\
         \x20 --help, -h      print this help\n\
         \n\
         Status: this build is the build skeleton. Command dispatch, the bounded log and the\n\
         two executables exist; hardware access, configuration and user interface do not.\n\
         Every command above except --version and --help reports that it is not implemented.\n\
         Exit codes: 0 success, 2 validation error, 3 daemon unavailable, 4 missing target,\n\
         5 unsupported control, 6 transport or verification failure, 7 partial application.",
        name = PRODUCT_NAME,
        version = VERSION
    )
}

/// Default log level until settings are implemented.
const DEFAULT_LOG_LEVEL: Level = Level::Warn;

fn init_logging() {
    if let Some(path) = paths::log_file() {
        logging::init(DEFAULT_LOG_LEVEL, path);
    }
}

fn print_version() {
    output::line(&format!("{} {}", PRODUCT_NAME, VERSION));
}

fn report_unimplemented(command: &str) -> i32 {
    output::error_line(&format!(
        "{}: command is not implemented in this build: {}",
        PRODUCT_NAME, command
    ));
    EXIT_USAGE
}

fn dispatch_console(args: &[String]) -> i32 {
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
        "monitors" => handle_monitors(&args[1..]),
        "start" => handle_start(&args[1..]),
        "stop" => handle_stop(&args[1..]),
        "status" => handle_status(&args[1..]),
        "profiles" => handle_profiles(&args[1..]),
        "settings" => handle_settings(&args[1..]),
        "reload" => handle_reload(&args[1..]),
        "profile" => handle_profile(&args[1..]),
        "startup" => handle_startup(&args[1..]),
        "icons" => handle_icons(&args[1..]),
        "repository" => handle_repository(&args[1..]),
        "get" => handle_get(&args[1..]),
        "set" => handle_set(&args[1..]),
        other if PLANNED_COMMANDS.contains(&other) => report_unimplemented(other),
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
            "partial" => EXIT_PARTIAL,
            _ => EXIT_TRANSPORT,
        },
    }
}

fn report_client_error(error: &crate::runtime::ClientError) -> i32 {
    output::error_line(&format!("{}: {}", PRODUCT_NAME, error.message()));
    client_exit_code(error)
}

fn handle_status(args: &[String]) -> i32 {
    let mut json_output = false;
    for argument in args {
        match argument.as_str() {
            "--json" => json_output = true,
            "--refresh" => {}
            other => return usage_error(&format!("status: unexpected argument: {other}")),
        }
    }

    match crate::runtime::request("status", json!(null)) {
        Ok(result) => {
            if json_output {
                emit_json(&result);
            } else {
                let monitors = result
                    .get("monitors")
                    .and_then(|value| value.as_array())
                    .map(|list| list.len())
                    .unwrap_or(0);
                let profiles = result.get("profiles").and_then(serde_json::Value::as_u64).unwrap_or(0);
                output::line(&format!(
                    "running, {monitors} monitor(s), {profiles} profile(s)"
                ));
            }
            EXIT_OK
        }
        Err(crate::runtime::ClientError::NotRunning) => {
            if json_output {
                emit_json(&json!({"running": false}));
            } else {
                output::line("not running");
            }
            EXIT_OK
        }
        Err(error) => {
            // A coordinator that has answered `stop` is not a running coordinator any more.
            if matches!(&error, crate::runtime::ClientError::Failure(failure) if failure.kind == "shutting_down") {
                if json_output {
                    emit_json(&json!({"running": false}));
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

fn handle_stop(args: &[String]) -> i32 {
    if let Some(argument) = args.first() {
        return usage_error(&format!("stop: unexpected argument: {argument}"));
    }
    match crate::runtime::stop_resident() {
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

fn handle_start(args: &[String]) -> i32 {
    if let Some(argument) = args.first() {
        return usage_error(&format!("start: unexpected argument: {argument}"));
    }
    if crate::runtime::is_running() {
        output::line("already running");
        return EXIT_OK;
    }
    match crate::runtime::start_resident() {
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

fn handle_profiles(args: &[String]) -> i32 {
    let Some(subcommand) = args.first() else {
        return usage_error("profiles: expected 'list', 'validate' or 'open'");
    };

    let directory = match paths::profiles_dir() {
        Some(directory) => directory,
        None => return usage_error("profiles: the roaming AppData directory is unavailable"),
    };

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

fn handle_settings(args: &[String]) -> i32 {
    let Some(subcommand) = args.first() else {
        return usage_error("settings: expected 'get', 'set' or 'open'");
    };
    let path = match paths::settings_file() {
        Some(path) => path,
        None => return usage_error("settings: the roaming AppData directory is unavailable"),
    };

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
            match crate::runtime::request(
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
            match crate::runtime::request("settings.set", json!({"key": key, "value": value})) {
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

fn handle_reload(args: &[String]) -> i32 {
    if let Some(argument) = args.first() {
        return usage_error(&format!("reload: unexpected argument: {argument}"));
    }
    match crate::runtime::request("profiles.reload", json!(null)) {
        Ok(result) => {
            let count = result.get("count").and_then(serde_json::Value::as_u64).unwrap_or(0);
            output::line(&format!("reloaded {count} profile(s)"));
            EXIT_OK
        }
        Err(error) => report_client_error(&error),
    }
}

fn handle_profile(args: &[String]) -> i32 {
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
            match crate::runtime::request("profile.apply", params) {
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
            match crate::runtime::request("profile.next", json!({"monitor": monitor})) {
                Ok(result) => report_apply(&result, json_output),
                Err(error) => report_client_error(&error),
            }
        }
        other => usage_error(&format!("profile: unknown subcommand: {other}")),
    }
}

/// Print an apply result and map its status to an exit code.
fn report_apply(result: &serde_json::Value, json_output: bool) -> i32 {
    let status = result
        .get("status")
        .and_then(serde_json::Value::as_str)
        .unwrap_or("not_applied");

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
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or("");
                output::line(&format!("  {control:<20} {field_status} {detail}"));
            }
        }
    }

    match status {
        "applied" => EXIT_OK,
        "partial" => EXIT_PARTIAL,
        _ => EXIT_TRANSPORT,
    }
}

fn handle_startup(args: &[String]) -> i32 {
    match args.first().map(String::as_str) {
        Some("status") => match crate::runtime::request("startup.status", json!(null)) {
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

fn handle_get(args: &[String]) -> i32 {
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
    match crate::runtime::request(
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

fn handle_set(args: &[String]) -> i32 {
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
    match crate::runtime::request(
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

fn monitors_select(args: &[String]) -> i32 {
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
        return usage_error(
            "monitors select: the selection dialog arrives with the user-interface gate; \
             use --monitor <id> or --none",
        );
    }
    if none && !ids.is_empty() {
        return usage_error("monitors select: --none cannot be combined with --monitor");
    }
    if !none && ids.is_empty() {
        return usage_error("monitors select: choose --monitor <id> (repeatable) or --none");
    }

    match crate::runtime::request(
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

fn monitors_validate_value(args: &[String]) -> i32 {
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

    match crate::runtime::request(
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

fn handle_monitors(args: &[String]) -> i32 {
    match args.first().map(String::as_str) {
        Some("list") => monitors_list(&args[1..]),
        Some("capabilities") => monitors_capabilities(&args[1..]),
        Some("select") => monitors_select(&args[1..]),
        Some("validate-value") => monitors_validate_value(&args[1..]),
        Some(other) => usage_error(&format!("monitors: unknown subcommand: {other}")),
        None => usage_error(
            "monitors: expected 'list', 'capabilities', 'select' or 'validate-value'",
        ),
    }
}

fn dispatch_tray(args: &[String]) -> i32 {
    match args.first().map(String::as_str) {
        None => {
            init_logging();
            log_info!("broiler-tray: starting without a user interface (build skeleton)");
            output::error_line(&format!(
                "{}: this build has no tray, no selection dialog and no coordinator yet",
                PRODUCT_NAME
            ));
            EXIT_OK
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
            init_logging();
            output::error_line(&format!(
                "{}: the internal DDC worker is not implemented in this build",
                PRODUCT_NAME
            ));
            EXIT_USAGE
        }
        Some(other) => {
            output::error_line(&format!("{}: unknown option: {}", PRODUCT_NAME, other));
            EXIT_USAGE
        }
    }
}

/// Entry point shared by both binaries.
pub fn run(program: Program, args: &[String]) -> i32 {
    match program {
        Program::Console => dispatch_console(args),
        Program::Tray => {
            // A GUI-subsystem process has no console unless one is inherited.
            output::attach_console_if_needed();
            dispatch_tray(args)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn argv(items: &[&str]) -> Vec<String> {
        items.iter().map(|s| s.to_string()).collect()
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
    fn no_arguments_is_a_usage_error_for_the_console_program() {
        assert_eq!(dispatch_console(&[]), EXIT_USAGE);
    }

    #[test]
    fn planned_commands_report_not_implemented_rather_than_success() {
        // The flyout is the only contract command with no implementation in this build.
        assert_eq!(dispatch_console(&argv(&["flyout"])), EXIT_USAGE);
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
}
