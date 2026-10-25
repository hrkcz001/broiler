# Implementation handoff

> Superseded. The current handoff for the implementation worker is `docs/next-worker-handoff.md`. This file is kept for history; where they differ, the newer handoff and `docs/storage.md` win.

Target worker: DeepSeek V4.1 Flash 1017. This is a handoff document, not a claim that a worker has been launched or that a particular provider model ID exists.

Audience: the worker session that writes the Rust implementation. Read this first, then `docs/architecture.md` for reasoning and `docs/parameters.md` for the value model. `README.md` is the user-facing contract.

## Mission

Implement `broiler` in the existing repository `hrkcz001/broiler` (branch `main`, public, Apache-2.0) according to the approved design. Do not redesign scope. When reality contradicts a design assumption, stop, record the evidence, and change the document with a note rather than silently diverging.

## Starting state

- Present: `README.md`, `LICENSE`, `NOTICE`, `docs/architecture.md`, `docs/parameters.md`, `examples/*.toml`, `packaging/scoop/broiler.json.template`, `.gitignore`.
- Absent: `Cargo.toml`, any Rust source, any build script, any workflow, any release, any Scoop manifest in the bucket.
- No hardware write has ever been made by this project. All DDC claims are design-level.
- Host facts: Windows 11, Rust 1.99 installed **without rustup**, so a bare `cargo build` cannot find the MSVC linker. Create `build.bat` that calls
  `C:\Program Files (x86)\Microsoft Visual Studio\18\BuildTools\VC\Auxiliary\Build\vcvars64.bat` and then cargo, and use it for every build. In an MSYS bash shell invoke it as `MSYS2_ARG_CONV_EXCL='*' cmd.exe /c build.bat`.
- Development display: `MSI MAG275R` (external, HDMI/DP) plus an internal `BOE0852` panel that must never be selected.

## Ownership, tools, and delivery

- Repository: https://github.com/hrkcz001/broiler, local `C:\Users\hrkcz001\broiler`. Re-read actual state before acting; this document may predate commits from another session. Do not overwrite unexpected changes.
- The owner explicitly authorizes autonomous commits, pushes, repository configuration, releases and publication to the existing `hrkcz001/scoop_bucket`. No second bucket. Never publish a checksum or release URL before the real artifact exists.
- Copyright stays `hrkcz001 2026`; Apache-2.0. README credits Hermes Agent as author as implementation proceeds. Never add AI sign-offs or coauthor trailers to Git commits.
- Reports to the owner are Russian; project files, code, UI and commit messages are English. Keep checkpoints brief.
- In Hermes, load relevant Rust Win32, TDD, debugging, GitHub, Scoop and review skills before those phases. Follow the repository contract over stale skill examples; no NULL-DACL named pipe recipes.
- Tool shell on this host is MSYS bash, not PowerShell. Invoke PowerShell explicitly; give native programs `C:/...` paths instead of `/c/...`. Scratch files belong under the Hermes scratch directory, not system temp. Do not modify other Hermes profiles or configuration just to select a model.
- Requested model label is DeepSeek V4.1 Flash 1017. The owner will start that worker session; do not assume the label is an API ID or silently start another model.
- Hardware tests must target an unambiguously identified monitor. Begin read-only, record current values, then make a deliberate minimal reversible adjustment for a required test. Restore a test baseline only when the path is healthy; this is not permission to add automatic rollback/restore-on-exit product behavior. Record outcome-unknown writes and stop rather than replaying them. Request visual confirmation for color-temperature and audio validation instead of inventing it.

## Non-negotiable constraints

1. Use direct native Win32 through `windows-sys` as the planned platform stack. No Electron or WebView. Do not introduce alternative UI/event-loop stacks or an async runtime without measured evidence and an explicit documented architectural decision.
2. Two binaries: `broiler.exe` (console) and `broiler-tray.exe` (GUI subsystem, Per-Monitor V2 manifest). No Windows service, no kernel driver, no elevation anywhere.
3. One hardware writer. All DDC traffic flows through a single coordinator and a sequential executor. Blocking Dxva2 calls run in an on-demand child process (`broiler-tray.exe --internal-ddc-worker`) with a kill-on-close job object and confirmed termination before replacement.
4. TOML for settings and profiles, exactly the schema in `docs/parameters.md`. Unknown keys are errors. Typed values are percent | native | named intent. There is no separate discrete syntax: `{ raw = N }` is one native integer validated against whatever the device reports, range or enumerated set. A bare integer for a continuous control is percent even when written in hex. Preset indices are choices, never percentages, and VCP payloads are not assumed to be one byte.
5. Color temperature is required v1 scope with three forms: Kelvin intent, `{ vcp = 0x14, raw = N }`, `{ vcp = 0x0C, raw = N }`. It must not be approximated with RGB, gamma, or an overlay. Win32 `MC_COLOR_TEMPERATURE` enum ordinals must never be written as VCP values.
6. No arbitrary VCP write path, no vendor codes, no factory reset, no input switching, no power mode, no EEPROM save, no background polling, no automatic restore-on-exit.
7. Named pipes only: per-logon-SID DACL, `PIPE_REJECT_REMOTE_CLIENTS`, `FILE_FLAG_FIRST_PIPE_INSTANCE` on the first instance, length-prefixed JSON, 64 KiB frame cap. No TCP/HTTP.
8. English only in code, comments, docs, UI strings, commit messages, and release notes. No emoji in UI. No AI attribution in commits.
9. Never claim CI green, a passed hardware test, a published release, or a working Scoop install without fresh evidence from the command you ran.

## Ordered work packages

Execute in order. Each has a gate that must be evidenced before the next begins. Record gate evidence in `docs/tested-devices.md` or a narrowly scoped test report; do not use AI sign-off trailers.

**G1 — build skeleton.** `Cargo.toml` (choose and document an appropriate stable edition, 2021 is acceptable), `build.bat`, `.rc`/manifest with Per-Monitor V2, `main.rs` dispatch for both binaries, bounded file logger, `--version`. Gate: `build.bat --release` produces both exes; `broiler --version` prints to the console, and the GUI executable starts without a console flash. Do not require a GUI-subsystem executable to print to an unattached console.

**G2 — read-only discovery.** `QueryDisplayConfig` + `EnumDisplayMonitors` + physical monitor enumeration, topology-to-handle join, EDID-based identity with documented fallback to a connection-bound ID, internal-panel exclusion, bounded reads of `0x10`/`0x12` and the capabilities string. Gate: `broiler monitors list --json` and `broiler monitors capabilities --monitor <id> --json` return real data for the MSI and correctly exclude the internal panel; no writes exist in the binary yet.

**G3 — domain and config, tests first.** Typed value model (percent/native/Kelvin intent), including native values above 255 and set- versus range-validation chosen from device metadata, profile parsing and full-directory validation, ordering, settings load/save with comment preservation, lifecycle state machine, apply planning, partial/error results, fake backend. Gate: `cargo test` covers every startup branch (zero/one/many monitors, zero/one/many profiles, saved-missing selection, invalid file aborting activation) and all unit-conversion edge cases without hardware.

**G4 — bounded backend, brightness only.** Worker protocol, job object, sequential executor, 100 ms inter-transaction gap, 2 s watchdog, bounded retries for reads only, no replay of timed-out writes, circuit breaker, readback verification. Gate: a brightness write on the MSI is verified by readback, a deliberately hung fake worker is contained and reaped, and no UI thread blocks.

**G5 — coordinator and CLI.** Single instance, ACL pipe server, typed request/response, all commands from the README's CLI contract, exit codes 0/2/3/4/5/6/7, `--json` everywhere on read paths, transient headless execution when no daemon is running, `monitors validate-value`. Gate: two concurrent CLI clients share one executor; malformed frames, oversized frames, and a wrong-session client are rejected where a real second-session fixture is available; otherwise document that manual security check as untested rather than inventing it; every documented command has an integration test against the fake backend.

**G6 — required startup UX.** Selection dialog with checkboxes and Remember choice, shown for multiple monitors with no saved selection **including at logon**, zero/one/many-profile lifecycle, zero-profile setup window, settings reachable without normal residency. Gate: fake-backend lifecycle table plus one manual single-monitor one-shot run.

**G7 — tray and icons.** One `NOTIFYICONDATA` per target, `NOTIFYICON_VERSION_4`, event from `LOWORD(lParam)`, id from `HIWORD(lParam)`, `NIN_SELECT`/`WM_CONTEXTMENU`, `TaskbarCreated` re-add on a hidden top-level window, monochrome icon compositor with numeric badges, DPI-correct HICON caching, right-click menu with exactly Settings and Exit. Gate: icons survive an Explorer restart; no GDI/USER handle growth over 100 profile switches; two-digit badge renders legibly at 16 px.

**G8 — flyout and settings window.** Modeless popup with monitor title, profile rows, keyboard navigation, focus-loss dismissal, `Shell_NotifyIconGetRect` anchoring clamped to the work area, pending/partial/error states, no settings control inside the flyout. Settings window with launch-at-login, click behavior, monitor selection, open/reload profiles, repository link. Gate: exact CLI parity for every setting; Narrator reads title, rows, and states; no DDC call on the UI thread.

**G9 — remaining controls.** Contrast, color temperature (all three forms, increment-aware conversion only when validated), color presets, RGB gains with dependency re-read, volume, audio-only mute. Each control gated independently per device; document unsupported cases instead of widening scope. Gate: MSI capability report recorded in `docs/tested-devices.md` with exact commands and observed values; mute never blanks the screen on a mapping you enabled.

**G10 — hardening.** Wake/hotplug, debounced rediscovery, worker faults, saved-selection ambiguity, invalid reload, external OSD changes, log caps, measured resource report. Gate: `docs/tested-devices.md` holds real numbers for private bytes, working set, CPU time over a fixed interval, startup time, and per-operation latency.

**G11 — CI and release.** `ci.yml` (fmt, clippy `-D warnings`, test, release build) and `release.yml` (tag `v*` → zip `broiler-<version>-windows-x64.zip` with both exes, LICENSE, NOTICE, examples, plus SHA256SUMS). Gate: download the uploaded asset, verify its hash, and run the CLI smoke tests from the extracted copy. Do not claim code signing.

**G12 — Scoop.** Fill `packaging/scoop/broiler.json.template` with the real version and hash, add `bucket/broiler.json` to `hrkcz001/scoop_bucket`, wait for that bucket's CI, then `scoop install hrkcz001/broiler` in a clean shell, exercise CLI + tray + autostart + upgrade + uninstall. Gate: actual install transcript recorded in the release notes. Preserve user config on uninstall.

## Testing rules

- Fake backend tests are the default for CI; label them as such. Hardware evidence is separate and dated.
- Every timed or retried code path needs a deterministic test with a clock or injected delay, not a sleep in CI.
- Golden tests for icon/badge bitmaps at 16/24/32 px.
- No test may write to a real monitor in CI, and no test may assume a display is attached.

## Reporting back

Keep a running `docs/tested-devices.md`. When reporting to the owner, state plainly: which gates are evidenced, which are not, what commands produced the evidence, and what remains blocked. If a hardware operation is ambiguous (timeout, partial write), say so and stop rather than retrying.

## Definition of done for the first user-visible release

All gates G1 through G12 evidenced, or unavailable physical test cases explicitly declared as untested rather than fabricated; README status line replaced with the real version; no unresolved release/hash placeholders in the live manifest, while clearly labelled documentation placeholders remain acceptable; `broiler monitors capabilities` output for the MSI included in the release notes; known limitations listed honestly.
