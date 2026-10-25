//! Command dispatch for both executables.
//!
//! The G1 skeleton owns argument dispatch, the version/help surface, exit codes and the
//! logger bootstrap. Every hardware or configuration command is listed in the help text
//! because it is part of the user-facing contract, and reports that it is not implemented
//! in this build instead of pretending to succeed.

use serde::Serialize;

use crate::discovery::{self, DiscoveryFailure};
use crate::logging::{self, Level};
use crate::model::{CapabilitiesReport, ControlEvidence, MonitorListReport, Reading};
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
const PLANNED_COMMANDS: &[&str] = &[
    "start",
    "stop",
    "status",
    "reload",
    "profiles",
    "profile",
    "get",
    "set",
    "settings",
    "startup",
    "repository",
    "icons",
    "flyout",
    "paths",
    "operations",
    "ping",
];

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
        other if PLANNED_COMMANDS.contains(&other) => report_unimplemented(other),
        other => {
            output::error_line(&format!("{}: unknown command: {}", PRODUCT_NAME, other));
            EXIT_USAGE
        }
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

fn handle_monitors(args: &[String]) -> i32 {
    match args.first().map(String::as_str) {
        Some("list") => monitors_list(&args[1..]),
        Some("capabilities") => monitors_capabilities(&args[1..]),
        Some(other) => usage_error(&format!("monitors: unknown subcommand: {other}")),
        None => usage_error("monitors: expected 'list' or 'capabilities'"),
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
        // These stay unimplemented, so the test never touches hardware.
        assert_eq!(dispatch_console(&argv(&["profiles", "list"])), EXIT_USAGE);
        assert_eq!(
            dispatch_console(&argv(&["set", "brightness", "42"])),
            EXIT_USAGE
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
