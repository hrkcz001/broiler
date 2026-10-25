# Test evidence

Everything below was produced by a command that was actually run. Build and fake-backend
evidence is kept separate from hardware evidence. A gate is listed as evidenced only when its
own gate command produced real output; anything else is listed as not yet evidenced.

## Host

- Windows 11, x86-64 MSVC target.
- Rust 1.99.0, installed without rustup. Every build runs through `build.bat`, which calls
  `C:\Program Files (x86)\Microsoft Visual Studio\18\BuildTools\VC\Auxiliary\Build\vcvars64.bat`
  before `cargo`, so the MSVC linker and `rc.exe` are on `PATH`.
- From an MSYS bash shell the build command is
  `MSYS2_ARG_CONV_EXCL='*' cmd.exe /c build.bat --release`.
- Displays present on this machine: MSI MAG275R (external) and BOE0852 (internal panel).
  The internal panel must never be targeted.

## G1 — build skeleton

Evidenced on 2026-10-25.

| Check | Command | Observed |
|---|---|---|
| Release build produces both executables | `MSYS2_ARG_CONV_EXCL='*' cmd.exe /c build.bat --release` | exit 0, `Finished \`release\` profile [optimized] target(s)` |
| Artifacts | `ls -l target/release/broiler.exe target/release/broiler-tray.exe` | both present, 170496 bytes each |
| Hashes | `sha256sum target/release/*.exe` | `a880a04386a6985f833c8d965d7362dff063bcc04f65aea562410309c4222f6d` (broiler.exe), `57de465ae40509530873bf7ed142c142dba3cf3b398c265d689e5abc3ba4c9d1` (broiler-tray.exe) |
| Console version | `target/release/broiler.exe --version` | `broiler 0.1.0`, exit 0 |
| Redirected version output | `target/release/broiler.exe --version > ver.txt` | exit 0, file contains `broiler 0.1.0` |
| GUI executable, inherited console | `target/release/broiler-tray.exe --version` | `broiler 0.1.0`, exit 0 |
| GUI executable, no arguments | `target/release/broiler-tray.exe` | exit 0, one log record path created, message states no UI exists |
| Unimplemented command | `target/release/broiler.exe monitors list` | exit 2, `command is not implemented in this build: monitors` |
| Unknown command | `target/release/broiler.exe frobnicate` | exit 2, `unknown command: frobnicate` |
| Unit tests | `MSYS2_ARG_CONV_EXCL='*' cmd.exe /c build.bat test` | 11 passed, 0 failed |
| PE subsystem | PE optional header read | `broiler.exe` subsystem 3 (`WINDOWS_CUI`), `broiler-tray.exe` subsystem 2 (`WINDOWS_GUI`) |
| Per-Monitor V2 manifest | `mt.exe -inputresource:target\release\broiler-tray.exe;#1 -out:<file>` | exit 0, extracted XML contains `dpiAwareness` = `PerMonitorV2,PerMonitor`, `dpiAware` = `true/pm`, `requestedExecutionLevel` = `asInvoker` |
| Version resource | `(Get-Item target\release\broiler-tray.exe).VersionInfo` | `FileDescription` = `broiler tray and coordinator`, `OriginalFilename` = `broiler-tray.exe`, `FileVersion` = `0.1.0.0` |

Notes and limits:

- The "no console flash" property is evidenced by the PE subsystem being `WINDOWS_GUI` plus the
  code path in `src/output.rs`, which only calls `AttachConsole(ATTACH_PARENT_PROCESS)` when this
  process has no usable standard handle. A GUI-subsystem process never has a console allocated for
  it at creation, and the process never calls `AllocConsole`. It was not verified by watching a
  desktop with a stopwatch, so no claim is made about a specific visual observation.
- `broiler-tray.exe --internal-ddc-worker` exits 2 with a "not implemented" message. The isolated
  worker belongs to G4.

## Hardware evidence

No DDC/CI read or write has been performed by this project. No monitor model, firmware, cable or
path has been validated. Every control remains unverified on real hardware.

| Date | Monitor | Operation | Result |
|---|---|---|---|
| — | — | — | nothing performed yet |

## Gates not yet evidenced

G2 through G12 have no evidence of any kind. They are not implemented.
