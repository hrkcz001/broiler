// Resource and manifest wiring for both executables.
//
// The Per-Monitor V2 manifest and the version resource are compiled with the MSVC
// resource compiler (rc.exe), which is on PATH because build.bat runs vcvars64.bat
// first. If rc.exe is unavailable the build still succeeds, but the manifest is then
// embedded through the linker and no version resource is emitted, so the build prints
// a warning instead of silently losing DPI awareness.

use std::env;
use std::path::PathBuf;
use std::process::Command;

fn main() {
    println!("cargo:rerun-if-changed=res/broiler.rc");
    println!("cargo:rerun-if-changed=res/broiler-tray.rc");
    println!("cargo:rerun-if-changed=res/broiler.manifest");
    println!("cargo:rerun-if-changed=build.rs");

    let target = env::var("TARGET").unwrap_or_default();
    if !target.contains("msvc") {
        return;
    }

    let out_dir = PathBuf::from(env::var("OUT_DIR").expect("OUT_DIR is set by cargo"));
    let rc_exe = env::var("RC_EXE").unwrap_or_else(|_| String::from("rc.exe"));

    // One resource object per binary, so each executable carries its own VERSIONINFO.
    // Compile both before emitting any link argument: a half-compiled set would give one
    // binary an embedded manifest and the other a linker-generated duplicate.
    let binaries = [
        ("broiler", "res/broiler.rc"),
        ("broiler-tray", "res/broiler-tray.rc"),
    ];
    let mut compiled: Vec<(&str, PathBuf)> = Vec::new();

    for (bin, rc) in binaries {
        let res = out_dir.join(format!("{bin}.res"));
        let status = Command::new(&rc_exe)
            .arg("/nologo")
            .arg("/fo")
            .arg(&res)
            .arg(rc)
            .status();
        match status {
            Ok(status) if status.success() => compiled.push((bin, res)),
            _ => {
                compiled.clear();
                break;
            }
        }
    }

    if compiled.len() == binaries.len() {
        for (bin, res) in &compiled {
            println!("cargo:rustc-link-arg-bin={bin}={}", res.display());
        }
        println!("cargo:rustc-link-arg-bins=/MANIFEST:NO");
    } else {
        {
            println!(
                "cargo:warning=rc.exe failed or is missing; embedding the manifest through the \
                 linker instead (no version resource). Build through build.bat for the full \
                 resource build."
            );
            println!("cargo:rustc-link-arg-bins=/MANIFEST:EMBED");
            println!("cargo:rustc-link-arg-bins=/MANIFESTINPUT:res/broiler.manifest");
        }
    }
}
