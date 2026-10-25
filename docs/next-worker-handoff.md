# Next implementation worker handoff

## 1. Mission and actual starting point

Requirement update after initial validation: Scoop installation must use `persist: data`, not AppData-only storage. Read `docs/storage.md` immediately after this handoff; it overrides old storage/path and no-persist instructions. The current executable has NOT been migrated by this planning change.

Continue broiler in `C:\Users\hrkcz001\dev\broiler`, public repository https://github.com/hrkcz001/broiler, branch main. Do not use the former `C:\Users\hrkcz001\broiler` location. Validation baseline: commit `8d3e60b`, independently checked on 2026-10-25. Re-read current HEAD and working tree before editing.

Read in order:

1. This file, including corrections to the reverse handoff.
2. `docs/storage.md`, then `docs/handoff-reverse.md`, sections 3, 4, 7, 9 and 11.
3. `docs/architecture.md` and `docs/parameters.md` for the intended contract.
4. `docs/tested-devices.md` for worker-reported evidence, not independent certification.
5. The actual source. Tests passing is not proof the production path uses the tested planner.

Two Rust binaries exist; native Windows discovery and high-level brightness/contrast reads work. A supervised DDC worker, pipe coordinator, typed profile parser, planner and lifecycle logic exist. UI, temperature writes and remaining gates do not exist. No release, CI or Scoop publication has been independently verified or claimed here.

Primary entry points:

- CLI and formatting: `src/cli.rs`.
- Startup/bootstrap: `src/runtime.rs`, `src/bin/broiler-tray.rs`.
- Lifecycle decisions: `src/lifecycle.rs`.
- Production command dispatch and profile apply: `src/coordinator.rs`.
- Intended plan/policy engine: `src/plan.rs`; do not assume it is wired into production.
- Transport/worker: `src/transport.rs`, `src/worker.rs`, `src/supervisor.rs`, `src/win/ddc.rs`.
- Pipe and accept loop: `src/pipe.rs`, `src/daemon.rs`, `src/ipc.rs`, `src/instance.rs`.
- Configuration persistence: `src/config/settings.rs`, `src/config/registry.rs`.

## 2. Independent reproduction results

Run from the new repository directory using an explicit tool working directory. A tool may retain a stale old working directory and fail before executing the command; an explicit `workdir` avoids this.

```text
MSYS2_ARG_CONV_EXCL='*' cmd.exe /c build.bat --release
MSYS2_ARG_CONV_EXCL='*' cmd.exe /c build.bat test
MSYS2_ARG_CONV_EXCL='*' cmd.exe /c build.bat test --lib pipe::tests::the_pipe_dacl_names_only_the_logon_sid -- --nocapture
```

Observed independently:

- Release build succeeded, 20.83 s in this run.
- 184 library tests passed, 0 failed, 1.50 s; binary/doc targets have 0 tests.
- PE subsystem: `broiler.exe` 3 (CUI), `broiler-tray.exe` 2 (GUI). File sizes approximately 1.09 and 1.28 MiB, not resident memory measurements.
- `broiler --version`: `broiler 0.1.0`.
- `broiler monitors list --json`: one external MSI MAG275R, ID `mon-227062c49310910f`, EDID serial 601, HDMI, identity confidence `edid_serial`; BOE0852 excluded as `internal_panel`.
- Current read values during validation: brightness 0, contrast 50; both high-level ranges 0..100. No hardware write was performed by the validating session.
- Capabilities readable, declared length 238, MCCS 2.1; raw response contains `14(0506080B)`. Therefore advertised preset values are 5, 6, 8, 11, not the numerical interval 5..8. This is advertised evidence, not verified writes, Kelvin labels or a completeness guarantee.
- `start` => `coordinator started`; second `start` => `already running`; resident status reports running true and two profiles. Selection is empty in the observed status.
- Two concurrently launched `get brightness --monitor mon-227062c49310910f` commands both returned exit 0 and brightness 0.
- `stop` => `coordinator stopped`; subsequent status => running false. The session leaves no intentionally started coordinator running.
- Pipe test returned `D:(A;;0x12019f;;;S-1-5-5-0-1517454)`: logon SID only. This is not an actual second-session rejection test or comprehensive ACL/ownership attack review.
- `flyout --monitor x` and `monitors select --dialog`: exit 2, unimplemented.
- Percentage 200 and native brightness `0x200 --raw`: exit 2, explicit rejection. Unknown monitor get: exit 4.
- `monitors validate-value brightness 40 --vcp 0x10 --monitor <id>` without confirmation: exit 2, confirmation required.
- Temperature commands exit 2 because the control is not writable in this build.

The reverse checklist's claim that adding `--vcp 0xD6` to `set brightness` yields exit 5 is not reproduced: actual result is exit 2, `unexpected argument: --vcp`. The allowlist still rejects it, but the report's exact command/exit claim must be corrected rather than copied.

Do not rerun write/profile tests casually. The reviewer deliberately did not run day/night application because it changes the owner's monitor. Write-test evidence in the earlier worker report remains earlier-worker evidence, not independently revalidated.

## 3. Claims not certified

- Full completion of G1-G5: build and components are present, but production contract gaps remain.
- Cross-session or remote-client rejection on a real adversarial connection; same-logon endpoint impersonation resistance.
- Any real multi-monitor, clone, MST, dock, identical-panel, reconnect or hotplug result.
- Temperature writes, RGB gains, audio, preset readback or physical white-point confirmation.
- UI, tray icons, accessibility, mixed-DPI, Explorer restart.
- Correct lifecycle at startup; one-profile/no-monitor behavior is not wired into the resident entry point.
- Autostart registry effects, stale-edit protection, production request deduplication, bounded accepted-operation queue or asynchronous operation status.
- Idle CPU, private bytes, startup time or memory budget. The executable sizes above are file sizes only.
- Profile apply hardware results from the reverse checklist section 3 item 8: not repeated by the validator.

## 4. Repair production correctness before extending UI

Add failing fake-backend/CLI tests first, then fix these without hardware writes. Do not report G5 complete merely because its unit suite stays green.

### P0: strict preflight, failure boundaries and aggregation

`src/coordinator.rs:223-342` manually applies contrast/brightness instead of executing the tested plan:

- `apply_one` writes supported controls before marking remaining ones unsupported and does not inspect `settings.unsupported`. Under `profiles.unsupported = fail`, a profile containing brightness and an unwritable optional control can still modify brightness. Preflight all requested fields on all explicit targets before any strict-mode write.
- A control failure does not stop subsequent controls on the same monitor. Stop on timeout/outcome-unknown or transport failure according to the design; do not continue a transaction on an unhealthy path.
- `apply_everywhere` aggregates `any_applied` and `any_partial` but does not count `not_applied` targets. One fully applied target plus one failed target can be reported as overall `applied`. Aggregate every target and retain typed error causes; test applied+failed, partial+failed and all-failed cases.
- Preserve truthful per-monitor profile state. Direct sets and partial changes must not leave an unverified old profile labelled active.

Gate: deterministic tests prove strict-mode preflight makes zero writes on every target when any requested field/target fails; mixed outcomes are never full success; timeout stops remaining writes.

### P1: single-owner, responsiveness and shutdown review

The pipe handler takes `Arc<Mutex<Coordinator>>` through transport calls (`src/daemon.rs:75-104`). Check request latency and shutdown under a blocked worker, accepted-connection bounds, and idle-client behavior. The accept loop and even its stopping checks also lock the coordinator. Current two-client tests manually create server instances rather than exercising `Daemon::run` end to end; test the actual loop, not only `handle_connection`.

Review transient CLI operations (`src/cli.rs` and `runtime::TransientTransport`) for an ownership race between absent-daemon detection, transient writes and simultaneous daemon start/another CLI. These are review targets, not asserted reproduced races. Prove the one-writer invariant across all entry paths, not only resident requests behind one mutex. Audit `FlushFileBuffers` for an unbounded wait when a connected client never consumes its response; a read deadline alone does not bound every I/O operation. Verify consistent overlapped cancellation or other bounded transport behavior.

Gate: end-to-end fake-worker process tests demonstrate bounded concurrent clients, stop latency and no overlapping writers. Record unavailable second-session tests honestly.

### P1: unified storage and migration before UI

Implement the complete resolver/persistence contract in `docs/storage.md` before G6-G8, not only at final Scoop packaging. Centralize `src/paths.rs` into a shared storage context, with explicit-root precedence, marker-selected executable-adjacent `data` for Scoop/portable use and AppData fallback for ordinary installs. Add `broiler paths --json` and dry-run/idempotent AppData migration. Propagate the root through CLI-to-daemon bootstrap, autostart registration and worker launch; reject conflicting live roots without creating another writer. Keep migration and packaging free of profile writes.

Gate: isolated resolver/child-root/conflict/migration tests pass, source AppData files remain untouched, and no selected root silently falls back on write failure. Actual Scoop linking/upgrade retention is still a later real install gate, not simulated evidence.

### P1: persistence, CLI and status

- `settings.set` in `src/coordinator.rs:436-450` writes directly with `std::fs::write`; `persist_settings` exists but this branch does not use it. Use comment-preserving atomic save with errors propagated and stale-edit rules, not a superficial call substitution.
- `settings get --json` while stopped prints a JSON string containing Rust `Settings { ... }` Debug text, not a settings object. Reproduced by CLI. Align schema online/offline and add integration tests.
- `--help` still claims this is only a build skeleton and all commands except help/version are unimplemented. Update help/status from actual capability information.
- Reverse report calls status a startup snapshot, but production `status` calls `transport.monitors()` on each request (`coordinator.rs:349`). Observation timestamp is still stale and `--refresh` is ignored. Fix cached versus refreshed semantics and age, rather than implementing the wrong assumed baseline.
- Add actual user autostart registry behavior and actual/desired reconciliation; do not label intent as effective enabled state.
- `profiles.validate` while resident currently returns the loaded registry as valid; review whether edited on-disk files are actually validated. Test dirty/invalid disk contents before claiming full CLI validation.

Gate: online/offline structured output matches; saved file remains valid under failure; default status is bounded cached state and refresh really refreshes; startup toggle reports real registration failures.

## 5. Continue G6-G12 in the existing design order

After the prerequisite repair gate above:

1. G6: integrate `StartupOutcome` into normal CLI/GUI launch. Zero controllable monitors exits silently; exactly one profile applies once and exits; multiple monitors without saved selection show the checkbox dialog including at logon; zero-profile setup and explicit settings remain reachable.
2. G7: native tray, one icon per controlled monitor, stable numeric badges, Per-Monitor V2, hidden top-level owner, version-4 callbacks, Explorer recovery, no GDI leaks.
3. G8: modeless monitor-titled flyout with profiles only, no Settings control; separate Settings window and full CLI parity, focus/keyboard/Narrator checks.
4. G9: temperature first, then remaining controls. Extend named allowlists and metadata instead of adding arbitrary numeric VCP writes. Support Kelvin intent and explicit `{ vcp = 0x14, raw = N }` / `{ vcp = 0x0C, raw = N }`. The owner specifically uses 5 for night brightness 0 and 8 for day brightness 100. These are two chosen values; do not interpolate, infer other supported choices from endpoints, or create automatic brightness-temperature linking.
5. G10: persist display indices, truthful verified profile state, topology/wake rediscovery, fault and resource measurements.
6. G11: fmt/clippy/tests/release workflows. Check formatter/linter availability; report what was not run. Release requires actual assets and checksum verification.
7. G12: required `persist: data` linking, custom Scoop-root support, real migration/upgrade/uninstall preservation tests, plus real `bucket/broiler.json` in existing https://github.com/hrkcz001/scoop_bucket, real uploaded artifact hash, bucket CI and actual installation/upgrade/uninstall test. No placeholder publication.

Native raw values use one syntax whether validated against a range or a set. Do not introduce a separate user-facing discrete type or truncate all payloads to one byte. Win32 color-temperature enum ordinals are not VCP values. Native value does not authorize arbitrary code writes or dangerous controls.

## 6. Environment and side effects

- MSYS bash tool shell on Windows; native programs need `C:/...` paths. Rust installed without rustup; wrapper must call real vcvars64. Use Hermes scratch for temporary probes.
- One active external MSI on HDMI; built-in BOE panel excluded. Current validation observed brightness 0/contrast 50 before and during reads, without writes.
- Existing AppData settings/profiles were created by the earlier worker; do not overwrite them to simplify a test. Read and preserve them. The worker report says day 100/80, night 0/50, at the owner's instruction; verify actual files before relying on this.
- No product registry write was performed during independent validation. Only start/stop and read/negative tests were exercised.
- Existing contract documents and reverse report remain reference material. Correct inaccurate reports with dated follow-ups rather than silently rewriting historical observations.

## 7. Pitfalls to retain

- Physical monitor arrays/packed fields need correct unaligned handling and RAII lifetimes.
- `cmds(...)` contains protocol command opcodes, not the VCP feature list. `vcp(...)` contains feature codes and nested value lists. Preserve nested values such as `14(0506080B)`; flat `vcp_codes` is insufficient for temperature.
- Do not use `MC_VCP_CODE_TYPE` to classify continuous versus enumerated controls: it is set-parameter versus momentary metadata. Use the version-aware registry and validated device evidence.
- High-level readings supply min/max; do not infer low-level range metadata without a defined policy. Never silently reuse unrelated mode/range data.
- Pipe DACL inspection does not prove all runtime authorization, ownership or cancellation properties.
- A named-pipe flush can wait for a reader; framing and read timeouts alone do not establish bounded shutdown.
- Worker process termination is asynchronous; no replacement writer before confirmed exit. A timeout means unknown hardware outcome, not permission to replay.
- Tests that assume the real endpoint is absent are not isolated from a running user daemon. Fix fixtures so test success does not depend on stopping a user's app.

## 8. Ownership and reporting

Autonomous commits/pushes and repository/releases/bucket management were authorized by the owner. Preserve Apache-2.0, copyright `hrkcz001 2026`, and Hermes authorship credit in the README. No AI commit trailers. Russian owner-facing reports; English source, comments, UI, documentation and commits.

Keep changes focused, use tests before fixes, inspect existing work before editing, and update the user-facing docs to actual implementation. Send brief Telegram gate checkpoints only if an authorized connected delivery path is available; do not invent delivery or alter messaging/profile configuration. Code and large logs remain in the main session.

Final report must distinguish: independently rechecked results, inherited worker evidence, fake-backend tests, physical writes, visual confirmation, unavailable test environments and unresolved gates. Any blocker is reported plainly; no fabricated evidence or full-completion claim based only on test count.
