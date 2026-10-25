//! Console executable: `broiler.exe`.
//!
//! Thin wrapper. All dispatch lives in the library so both executables share one
//! implementation and one test surface.

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let code = broiler::cli::run(broiler::cli::Program::Console, &args);
    std::process::exit(code);
}
