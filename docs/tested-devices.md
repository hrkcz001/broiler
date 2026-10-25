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

## G2 — read-only discovery

Evidenced on 2026-10-25 on the development machine. Read-only: the build contains no write call.

| Check | Command | Observed |
|---|---|---|
| Monitor list, human | `target/release/broiler.exe monitors list` | one controllable monitor, `mon-227062c49310910f`, MSI MAG275R, hdmi, primary, brightness 0, contrast 50 |
| Monitor list, JSON | `target/release/broiler.exe monitors list --json` | exit 0, valid JSON, `elapsed_ms` 250-262, `excluded` contains BOE0852 |
| Internal panel exclusion | same command | `excluded: [{"friendly_name": "BOE0852", "connector": "internal", "reason": "internal_panel", "detail": "built-in panel is present but not part of the active desktop; output technology is internal"}]` |
| Identity | `monitors list --json` | `identity_confidence: "edid_serial"`, `edid_serial: "601"`, `product_code: "0x3CB5"`, EDID read from `SYSTEM\CurrentControlSet\Enum\DISPLAY\MSI3CB5\5&16ba635d&0&UID4352\Device Parameters\EDID` |
| Identity stability across runs | two successive `monitors list --json` | identical id `mon-227062c49310910f` |
| Capabilities, human | `broiler monitors capabilities --monitor mon-227062c49310910f` | exit 0, 238 characters, `mccs_ver(2.1)`, `cmds 01 02 03 07 0C 4E F3 E3` |
| Capabilities, JSON | same with `--json` | exit 0, raw string recorded below, brightness and contrast `advertised: true` from the `vcp(...)` list |
| Unknown identifier | `monitors capabilities --monitor mon-0000000000000000` | exit 4, message lists the available identifier |
| Unit tests | `MSYS2_ARG_CONV_EXCL='*' cmd.exe /c build.bat test` | 42 passed, 0 failed |
| No write path | `grep -rn "SetVCPFeature\|SetMonitorBrightness\|SetMonitorContrast\|SetMonitorColor\|SetMonitorRedGreenOrBlue" src/` | no matches |

Raw capabilities reply from the MSI MAG275R, unchanged:

```text
(prot(monitor)type(lcd)model(MAG275R)cmds(010203070C4EF3E3)vcp(020405080B0C101214(0506080B)16181A6C6E70ACAEB6C0C6C8C9CACC(0102030405060708090A0C0D0F18192325)D6(0104)DF60(11120F)628D(0102)FF)mswhql(1)mccs_ver(2.1)asset_eep(32)mpu_ver(01))
```

Observations that matter for later gates:

- The reply uses hexadecimal pairs with no separator. A whitespace-only parser finds nothing; the
  parser in `src/caps.rs` splits separator-free runs into two-digit codes and keeps a golden test
  built from this exact reply.
- `0x10` and `0x12` are absent from `cmds(...)` but present in `vcp(...)`, and both read back
  successfully. This is the documented case where the capabilities string is advisory and must not
  gate a read.
- The internal panel `BOE0852` was present on the machine but not part of the active desktop during
  this run, so the active-path join never returned it. The excluded report is produced from a
  separate read-only all-paths query that never probes or controls the panel. Exclusion of a built-in
  panel that *is* part of the active desktop was not exercised on hardware; the classifier that
  decides it is unit-tested for `INTERNAL`, `LVDS`, embedded DisplayPort and embedded UDI.
- `physical_monitor` description is `Generic PnP Monitor`; the friendly name comes from the target
  device name, which Windows reports as coming from the EDID.
- `display_index` is assigned per session from left to right. Persistence is not implemented.

## Hardware evidence

Only reads have been performed. No write of any kind has been made by this project.

| Date | Monitor | Operation | Result |
|---|---|---|---|
| 2026-10-25 | MSI MAG275R (HDMI, `\\.\DISPLAY4`) | `GetMonitorBrightness`, `GetMonitorContrast`, VCP read `0x10`/`0x12`, capabilities string | brightness 0 (min 0, max 100), contrast 50 (min 0, max 100), VCP `0x10` current 0 max 100, VCP `0x12` current 50 max 100, code type `set_parameter`, capabilities 238 chars MCCS 2.1 |
| 2026-10-25 | BOE0852 (internal panel) | all-paths topology query, EDID registry read | detected, classified built-in, excluded from control, never probed |

## Gates not yet evidenced

G3 through G12 have no evidence of any kind. They are not implemented.
