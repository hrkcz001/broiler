# Reverse handoff: worker report to the validating session

Audience: the PM / validation session that checks this work and then writes the handoff for the next
implementation worker. Written by the implementation worker that produced gates G1–G5.

Date: 2026-10-25. Repository: `C:\Users\hrkcz001\dev\broiler` (moved here from
`C:\Users\hrkcz001\broiler` during this session), branch `main`, head `de7b40c`.

This document states what was built, what a command actually printed, what must not be trusted
without re-running it, and what the next worker still has to do. It is deliberately more pessimistic
than the summary in `docs/tested-devices.md`: where the two differ, this document wins.

## 1. Repository state

- 54 tracked files, 12 760 lines of Rust under `src/`, one Cargo package with two binaries
  (`broiler.exe`, `broiler-tray.exe`), no third-party UI or async dependency.
- Dependencies: `serde`, `serde_json`, `sha2`, `toml_edit`, `windows-sys` 0.61 with an explicit
  feature list. `Cargo.lock` is committed.
- Working tree is clean. Nothing is stashed, nothing is untracked.
- Build and test, from an MSYS bash shell (Rust here has no rustup, so `vcvars64.bat` must run first):

  ```text
  MSYS2_ARG_CONV_EXCL='*' cmd.exe /c build.bat --release
  MSYS2_ARG_CONV_EXCL='*' cmd.exe /c build.bat test
  MSYS2_ARG_CONV_EXCL='*' cmd.exe /c build.bat clean      # after moving the tree
  ```

- `build.bat clean` exists because cargo fingerprints embed absolute paths: after the tree moved, the
  build reported success in 0.09 s and a test still resolved the *old* `CARGO_MANIFEST_DIR`.
- No CI, no release, no Scoop manifest, no code signing.

## 2. Gate status as claimed

| Gate | Status | Evidence location |
|---|---|---|
| G1 build skeleton | evidenced | `docs/tested-devices.md` §G1 |
| G2 read-only discovery | evidenced | §G2 |
| G3 domain and configuration | evidenced (unit tests only) | §G3 |
| G4 bounded backend, brightness | evidenced on hardware | §G4 |
| G5 coordinator and CLI | evidenced except `flyout`, `monitors select --dialog`, and a cross-session client | §G5 |
| G6 startup UX | not started | — |
| G7 tray and icons | not started | — |
| G8 flyout and settings window | not started | — |
| G9 remaining controls (incl. colour temperature) | not started | — |
| G10 hardening and measurement | not started | — |
| G11 CI and release | not started | — |
| G12 Scoop | not started | — |

Test count at head: **184 passed, 0 failed**, 1.25 s.

## 3. Validation checklist

Run these in order; each line states what a correct result looks like. Anything else means the
corresponding claim is wrong.

1. `build.bat --release` → exit 0, both executables in `target\release`, tray subsystem is GUI
   (`2`), console subsystem is CUI (`3`).
2. `build.bat test` → 184 passed.
3. `broiler.exe --version` → `broiler 0.1.0`; `broiler.exe --help` lists the contract commands.
4. `broiler monitors list --json` → one monitor, `mon-227062c49310910f`, `identity_confidence`
   `edid_serial`, brightness/contrast readings present, and `excluded` contains `BOE0852`
   with reason `internal_panel`.
5. `broiler monitors capabilities --monitor <id> --json` → `capabilities.state = readable`, 238
   characters, `mccs_version` 2.1, `cmds 01 02 03 07 0C 4E F3 E3`.
6. `broiler start` → `coordinator started`; `broiler status --json` → `running: true`, one monitor;
   a second `broiler start` → `already running` and no second `broiler-tray.exe` process.
7. `broiler get brightness --monitor <id>` → a value with min/max/backend. Two such commands started
   together both exit 0 (one coordinator, one executor).
8. `broiler profile apply day --monitor <id> --wait` → `applied`, contrast and brightness rows
   `applied`, readbacks match the profile; `broiler profile next --monitor <id>` cycles to `night`.
9. `broiler stop` → `coordinator stopped`; `broiler status --json` → `{"running": false}`; no
   `broiler-tray.exe` remains.
10. Negative cases: unknown monitor → exit 4; percentage 200 → exit 2, nothing written;
    `--raw 0x200` outside range → exit 2, `nothing was written`; `monitors validate-value` without
    `--confirm-write` → exit 2; `--vcp 0xD6` for brightness → exit 5 with no write.
11. Pipe DACL: `build.bat test --lib "--" --nocapture` and read the line
    `pipe DACL: D:(A;;0x12019f;;;S-1-5-5-<n>)` — one ACE, a logon-session SID, no `WD`/`AN`/`AU`/`BU`.

## 4. Claims that must not be trusted without re-running

- **Cross-session rejection.** Never observed from a second logon session. The kernel-side DACL is
  verified (item 11); the positive refusal is not. Do not let this be recorded as tested.
- **Multi-monitor behaviour on hardware.** This machine has one monitor on one HDMI port. Selection,
  apply-to-all-selected and identity ambiguity exist only in fake-transport tests. Real
  clone/MST/dock/two-identical-panels behaviour is unknown.
- **Colour temperature.** Not implemented at all. `examples/color-temperature.toml` parses and
  validates, and the planner has unit tests, but no write path and no device mapping exists.
- **`startup.launch_at_login`.** Records intent in `settings.toml` only; no `HKCU\...\Run` value is
  written and no status reconciliation exists.
- **Settings written through the coordinator** are not atomic (temp file + rename is used only in the
  offline CLI path).
- **`--refresh`** on `status` is accepted and ignored; `status` is a snapshot taken at coordinator
  start, so observation age is not reported.
- **One-shot startup rule.** `broiler start` always becomes resident; it does not apply a sole profile
  and exit.
- **Narrator/accessibility, icons, DPI, tray resilience**: nothing exists, so nothing is claimed.
- Anything about memory, CPU, startup latency or payload size: never measured.

## 5. Deviations and document edits

- `README.md` status line was rewritten three times as gates landed; it currently lists exactly what
  works and marks the rest as missing. It still contains no release claim.
- `examples/day.toml` and `examples/night.toml` now carry `contrast = 80` and `contrast = 50`, and day
  brightness is `100`, night brightness is `0`, at the owner's instruction. `README.md` examples were
  updated to match.
- `docs/tested-devices.md` was created and holds dated gate evidence and a hardware table.
- `build.bat` grew `test` and `clean` subcommands.
- No change was made to `docs/architecture.md`, `docs/parameters.md`, or `docs/implementation-handoff.md`:
  the design was not contradicted by reality, only narrowed by scope that is still to be built.
- Architectural decisions the next worker inherits and should not silently reverse:
  - the DDC worker is a child process launched with two inherited anonymous pipes and stdin/stdout
    redirection, not a second named pipe;
  - profile application in the current build writes only brightness and contrast, and reports every
    other requested control as `unsupported` with an overall `partial` status;
  - `display_index` is assigned per session (left to right); persistence needs the state store.

## 6. Constraint conformance (handoff §"Non-negotiable constraints")

1. Native Win32 through `windows-sys` only; no WebView, no async runtime, no alternative event loop.
2. Two binaries; tray is GUI subsystem with Per-Monitor V2 manifest and `asInvoker`; no service, no
   driver, no elevation.
3. One hardware writer: a single coordinator behind a mutex, a single supervised worker process, job
   object with kill-on-close, confirmed termination, circuit breaker.
4. TOML with typed percent/native/intent values; unknown keys are errors; no separate discrete syntax;
   bare integer is percent for continuous controls even in hex; presets are choices.
5. Colour temperature is **not yet implemented**; when it is, the three required forms and the ban on
   Win32 enum ordinals are already encoded in the parser and planner tests.
6. No arbitrary VCP write, no vendor codes, no factory reset, no input switching, no power mode, no
   EEPROM save, no polling, no restore-on-exit. The worker's control allowlist rejects anything except
   brightness and contrast and is tested.
7. Named pipe with logon-SID DACL, `PIPE_REJECT_REMOTE_CLIENTS`, `FILE_FLAG_FIRST_PIPE_INSTANCE`,
   length-prefixed JSON, 64 KiB cap. No TCP/HTTP anywhere.
8. English artifacts, no emoji, no AI attribution in commits.
9. No CI-green, hardware-pass or release claim is made anywhere that a command did not produce.

## 7. Known defects and gaps, in the order the next worker should care

1. `flyout` and `monitors select --dialog` are unimplemented (G6/G8). They are the only contract
   commands that answer "not implemented".
2. `startup.launch_at_login` has no registry side effect; `startup status` reports the desired value
   only.
3. Coordinator settings writes are not atomic; reuse `config::settings::save` there.
4. `status --refresh` does nothing; add a bounded re-read and an observation-age field.
5. `broiler start` ignores the lifecycle outcome; it should honour `StartupOutcome` (one-shot,
   no monitors, selection required, zero profiles).
6. `profile.apply` is limited to brightness/contrast; the planning layer for the rest exists
   (`src/plan.rs`) but is not wired to a transport that can carry those codes.
7. `monitors capabilities` reports only brightness/contrast evidence; colour temperature diagnostics
   (known values, increments, native discrete values in decimal and hex) are missing.
8. No state store: `%LOCALAPPDATA%\broiler\state.json` is unused, so `display_index` and last-applied
   profile are per-process.
9. No debounced rediscovery, no wake handling, no hotplug handling.
10. No CI, no release, no Scoop manifest.

## 8. Environment facts and side effects outside the repository

- Development machine: Windows 11, one external MSI MAG275R on HDMI as `\\.\DISPLAY4`, one built-in
  BOE0852 panel that is present but not part of the active desktop. Single interactive logon.
- No Twinkle Tray or other DDC automation ran during the write tests; `NVDisplay.Container` only.
- Files created outside the repo by this session:
  - `%APPDATA%\broiler\settings.toml` (defaults, `remember = false`, empty `monitor_ids`);
  - `%APPDATA%\broiler\profiles\day.toml` and `night.toml` (owner's values: 100/80 and 0/50);
  - `%LOCALAPPDATA%\broiler\logs\broiler.log` (empty: default level is `warn`).
- No autostart registration, no service, no driver, no registry writes were made by the product.
- Monitor state after the session: brightness 0, contrast 50 — the owner's night profile. Every write
  test restored its baseline and verified the restore.
- Batch files used for ad-hoc checks live under the Hermes scratch directory, not in the repo; the
  commands themselves are in `docs/tested-devices.md`.

## 9. Technical pitfalls worth inheriting

Recorded because each cost real time and will bite again:

- `windows-sys` feature gating: `ReadFile`/`WriteFile` need `Win32_System_IO`; `MONITORINFOF_PRIMARY`
  lives in `Win32_UI_WindowsAndMessaging`; `CreateProcessW` needs `Win32_System_Threading`; named pipes
  need `Win32_Security` and `Win32_System_IO` as well as `Win32_System_Pipes`.
- `PHYSICAL_MONITOR` is packed: its description field cannot be borrowed, copy it with
  `std::ptr::read_unaligned`.
- A pipe server that disconnects right after writing can discard unread data: call
  `FlushFileBuffers` before `DisconnectNamedPipe`.
- A `CreateFileW` failure with `ERROR_FILE_NOT_FOUND` means "no server has ever created this endpoint";
  retrying it until a deadline turns a fast negative into a 30 s hang. Only `ERROR_PIPE_BUSY` is worth
  retrying.
- Wrapping a raw handle in a temporary RAII type to test validity closes it immediately; that bug
  silently defeated `FILE_FLAG_FIRST_PIPE_INSTANCE` and made `ConnectNamedPipe` fail with error 6.
- The worker's control names are an allowlist ("brightness", "contrast"); there is no numeric VCP
  parameter anywhere in the protocol. Keep it that way when adding G9 controls.
- Percentage mapping needs the device-reported minimum: the VCP getter reports no minimum, so a
  percentage over the low-level fallback must be refused rather than assumed to start at zero.
- The MSI's capabilities string uses separator-free hex pairs (`cmds(010203070C4EF3E3)`); a
  whitespace-splitting parser silently finds nothing. A golden test uses the exact observed string.
- `0x10` and `0x12` are absent from `cmds(...)` while present in `vcp(...)` and readable: the
  capabilities string must never gate a read.
- Verifying the pipe ACL is possible on a single-session machine by reading the live handle's DACL
  back as SDDL (`GetSecurityInfo` + `ConvertSecurityDescriptorToStringSecurityDescriptorW`).

## 10. What the next worker must do

Follow the gate order. Concretely:

- **G6** — selection dialog (checkbox list, Remember choice, shown at logon too), zero-profile setup
  window, settings reachable without residency. All of this is native Win32; the decision logic already
  exists in `src/lifecycle.rs` and is fully tested, so the gate's work is UI plus wiring
  `StartupOutcome` into `broiler start` and the tray entry point.
- **G7** — tray: one `NOTIFYICONDATA` per target, `NOTIFYICON_VERSION_4`, `LOWORD/HIWORD(lParam)`,
  `TaskbarCreated` re-add, monochrome icon compositor with numeric badges, HICON cache keyed by
  (icon, index, DPI, theme, status).
- **G8** — modeless flyout and the settings window, with exact CLI parity.
- **G9** — remaining controls, colour temperature first: extend the worker allowlist, add per-device
  metadata reads (ranges, enumerated sets, `0x0B` increment), then wire `src/plan.rs` to real writes.
  Required forms: Kelvin intent, `{ vcp = 0x14, raw = N }`, `{ vcp = 0x0C, raw = N }`.
- **G10** — wake/hotplug, debounced rediscovery, worker faults, measured resource report.
- **G11** — CI (fmt, clippy `-D warnings`, test, release build) and the release workflow. Note that
  `cargo fmt` and `cargo clippy` are **not installed** on this host; CI must provide them.
- **G12** — Scoop manifest with a real hash, bucket CI, install transcript.

## 11. Suggested structure for the next worker handoff

1. What already exists and works, with the file to start from for each gate.
2. The exact reproduction commands, copied from §3 here.
3. The list from §4 of claims not to trust.
4. The gate order and the evidence each gate must produce.
5. Environment facts: one monitor, one HDMI, no rustup, AppData state, single logon session.
6. The pitfalls in §9.
7. The reporting contract: Russian to the owner, English artifacts, no AI trailers, ping Telegram at
   each gate boundary, and never claim a gate that did not produce its own evidence.

## 12. Attestation

Every number, string and exit code in this document came from a command run in this session against
this machine. No hardware behaviour, test result, release or install is described that was not
observed. Where something is unknown it is written as unknown, and the corresponding entry in
`docs/tested-devices.md` carries the same qualification.
