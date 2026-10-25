//! GUI-subsystem executable: `broiler-tray.exe`.
//!
//! Windows subsystem is set here so the process never allocates a console window when it is
//! started from Explorer or from the logon Run key. Console output is only produced when a
//! parent console already exists, which is how `broiler-tray.exe --version` reports itself
//! from an interactive shell without breaking redirection.

#![windows_subsystem = "windows"]

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let code = broiler::cli::run(broiler::cli::Program::Tray, &args);
    std::process::exit(code);
}
