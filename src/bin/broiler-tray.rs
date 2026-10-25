//! GUI-subsystem executable: `broiler-tray.exe`.
//!
//! Windows subsystem is set here so the process never allocates a console window when it is
//! started from Explorer or from the logon Run key. Console output is only produced when a
//! parent console already exists, which is how `broiler-tray.exe --version` reports itself
//! from an interactive shell without breaking redirection.

#![windows_subsystem = "windows"]

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();

    // The internal worker mode speaks the private protocol on inherited standard handles and must
    // never touch a console, a log file or the user interface.
    if args.first().map(String::as_str) == Some("--internal-ddc-worker") {
        std::process::exit(broiler::worker::serve());
    }

    // Everything else, including the resident start with no arguments, goes through the shared
    // entry point, so the one data root is resolved once for the whole process tree. The tray,
    // selection dialog and settings window of later gates live in this same process.
    std::process::exit(broiler::cli::run(broiler::cli::Program::Tray, &args));
}
