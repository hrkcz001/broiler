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

## G3 — domain and configuration, tests first

Evidenced on 2026-10-25. No hardware involved: this gate is pure domain logic and parsing.

| Check | Command | Observed |
|---|---|---|
| Whole test suite | `MSYS2_ARG_CONV_EXCL='*' cmd.exe /c build.bat test` | 138 passed, 0 failed |
| Release build unchanged | `MSYS2_ARG_CONV_EXCL='*' cmd.exe /c build.bat --release` | exit 0 |
| Shipped examples still parse | test `config::profile::tests::repository_examples_parse_with_the_current_schema` | `examples/day.toml`, `night.toml`, `full-profile.toml`, `color-temperature.toml` accepted by the parser |

Startup branches, each a named test in `src/lifecycle.rs`:

| Branch | Test |
|---|---|
| zero monitors | `zero_monitors_exits_silently_even_with_profiles`, `zero_monitors_wins_over_an_invalid_registry` |
| one monitor, one profile | `one_monitor_and_one_profile_is_a_one_shot_run` |
| one monitor, many profiles | `one_monitor_and_many_profiles_is_resident` |
| many monitors, no saved choice | `many_monitors_without_a_saved_choice_require_the_dialog` |
| saved selection partly missing | `a_saved_selection_controls_only_the_present_members` |
| saved selection entirely missing | `a_fully_missing_saved_selection_asks_again_instead_of_retargeting` |
| zero profiles | `zero_profiles_with_monitors_shows_the_setup_path` |
| invalid profile file | `an_invalid_profile_aborts_activation_instead_of_using_the_valid_ones` |
| confirmed empty selection | `a_confirmed_empty_selection_exits_without_tray_icons` |

Value model and conversion edge cases are covered in `src/config/value.rs`
(`bare integer is percent`, `hexadecimal bare integer is still percent`, `raw table is native and hex
equals decimal`, `native values above 255 are preserved exactly`, `percent and raw together are
rejected`, `percent mapping respects a nonzero minimum`, `percent mapping rounds half up`,
`native values are validated against a range without clamping`, `native values are validated against
an enumerated set`, `percentages are rejected for enumerated and unknown controls`), profile schema
and conflict rules in `src/config/profile.rs`, and planning/verification/timing in `src/plan.rs` and
`src/backend.rs` (including `a_timeout_is_reported_as_an_unknown_outcome_and_stops_the_monitor` and
`transactions_are_separated_by_the_configured_gap`, which asserts the 100 ms gap through an injected
clock instead of sleeping).

Limits recorded honestly:

- `cargo fmt` and `cargo clippy` are unavailable on this host: Rust was installed without rustup, so
  neither component is present. Formatting and lint gates therefore have no local evidence yet; they
  belong to G11's CI, which installs a normal toolchain.
- Planning uses device metadata supplied by tests. Real per-control metadata for the MSI is still
  unread beyond brightness, contrast and the capabilities string.

## G4 — bounded backend, brightness only

Evidenced on 2026-10-25. This is the first gate with real hardware writes.

| Check | Command | Observed |
|---|---|---|
| Unit and integration tests | `MSYS2_ARG_CONV_EXCL='*' cmd.exe /c build.bat test` | 155 passed, 0 failed |
| Verified brightness write | `broiler set brightness 5 --monitor mon-227062c49310910f --wait --json` | exit 0, `{"native_target": 5, "observed": 5, "verified": true, "backend": "dxva2-high-level"}` |
| Independent readback | `broiler get brightness --monitor mon-227062c49310910f` in a fresh process | `brightness 5 (min 0, max 100, dxva2-high-level)` |
| Baseline restored | `broiler set brightness 0 ...` then `broiler get brightness ...` | `set to 0 and verified by readback`, readback `0` |
| Verified contrast write | `broiler set contrast 51 ...` then `get contrast` | verified, `contrast 51`; restored to `50` and verified |
| Unknown monitor | `broiler get brightness --monitor mon-0000000000000000` | exit 4, `unknown_monitor` |
| Percentage above 100 | `broiler set brightness 200 ...` | exit 2, nothing written |
| Native value out of range | `broiler set brightness 0x200 --raw ...` | exit 2, `outside the reported range 0..=100; nothing was written` |
| Unlisted control | `broiler get sharpness ...` | exit 2, only brightness and contrast exist |
| Implicit broadcast refused | `broiler set brightness 10` (no `--monitor`) | exit 2 |
| No worker left behind | `Get-Process broiler-tray` before and after each operation | 0 processes before and after |
| Hung worker contained and reaped | test `supervisor::tests::a_hung_worker_is_contained_and_reaped` | a silent child that never answers is killed after the operation bound; `Termination { confirmed: true }`; supervisor not quarantined |
| Circuit breaker | test `supervisor::tests::a_dead_worker_is_reported_and_the_circuit_breaker_opens` | after three consecutive failures the backend refuses further traffic with `Quarantined` |
| Frame limits | `src/ipc.rs` tests | oversized, truncated, length-mismatched and malformed frames are rejected, never panicked on |

Honest limits:

- "No UI thread blocks" cannot be tested because no UI exists yet: both binaries are console-driven at
  this point. What is evidenced is that the calling process performs no Dxva2 call at all — every read
  and write goes through `broiler-tray.exe --internal-ddc-worker` over inherited pipes, and the parent
  only waits on that pipe with a deadline.
- The 100 ms inter-transaction gap, retry policy and verification are exercised against the fake
  backend and a scripted clock; on hardware each CLI invocation performs one write and one readback, so
  the gap between two transactions has not been observed yet.
- Only `0x10` and `0x12` have a write path. Every other control is refused by the worker's control
  allowlist, which is tested (`control_names_are_an_allowlist`).
- The competing-automation condition was checked: no Twinkle Tray or other DDC automation process was
  running during these writes (`Get-Process` filtered for monitor-control tools and `NVDisplay.Container`
  only).

## G5 — coordinator and CLI

Evidenced on 2026-10-25. Every command in the README contract answers except the two that need the
user interface (`flyout`, `monitors select --dialog`), and one security check has no fixture: a client
from another terminal session. That check is declared untested rather than assumed. Nothing below is
claimed beyond what the commands printed.

| Check | Command | Observed |
|---|---|---|
| Test suite | `MSYS2_ARG_CONV_EXCL='*' cmd.exe /c build.bat test` | 175 passed, 0 failed |
| Two concurrent clients, one executor | test `daemon::tests::two_concurrent_clients_share_one_executor` | both clients answered on their own connection; the fake transport recorded peak concurrency 1 |
| Malformed frame | test `daemon::tests::a_malformed_frame_gets_a_protocol_error_instead_of_a_panic` | typed `protocol_error` response, no panic |
| Oversized frame | test `daemon::tests::an_oversized_frame_is_refused_without_allocating_it` | refused before allocating the payload |
| Single instance | test `instance::tests::the_second_acquisition_reports_an_existing_owner` | second acquisition reports an existing owner; `broiler start` printed `already running` |
| Endpoint naming | test `pipe::tests::the_endpoint_name_is_stable_and_scoped_to_the_logon_sid` | name is `\\.\pipe\broiler.v1.<16 hex>`, derived from a hash of the logon SID |
| First-instance flag | test `pipe::tests::first_instance_flag_detects_an_existing_server` | a second first-instance creation fails |
| Pipe DACL, read back from the kernel | test `pipe::tests::the_pipe_dacl_names_only_the_logon_sid` (`--nocapture`) | `D:(A;;0x12019f;;;S-1-5-5-0-1517454)` — exactly one ACE, one logon-session SID, no `WD`/`AN`/`AU`/`BU`, not a null DACL |
| Start and status | `broiler start`, `broiler status --json` | `coordinator started`; JSON reported `running: true`, one monitor, zero profiles, settings path |
| Read through the coordinator | `broiler get brightness --monitor mon-227062c49310910f` | `brightness 0 (min 0, max 100, dxva2-high-level)` from the resident process |
| Two concurrent CLI clients | two `broiler get` invocations started together | both exit 0 with correct values from the one coordinator |
| Write through the coordinator | `broiler set brightness 2 --wait`, then `broiler get` | `set to 2 and verified by readback ... via the coordinator`, readback `2`; restored to `0` and verified |
| Settings through the coordinator | `broiler settings get/set tray.left_click` | `"flyout"` → `cycle` → `flyout`, file written with `schema_version = 1` |
| Stop and status | `broiler stop`, `broiler status --json` | `coordinator stopped`; status `{"running": false}`; zero `broiler-tray` processes left |
| Exit codes | `broiler get brightness --monitor mon-0000000000000000` etc. | 4 for an absent target, 2 for a value outside the contract, 0 for idempotent stop |
| Profile validation and listing | `broiler profiles validate`, `broiler profiles list` | `2 profile(s) valid in C:\Users\hrkcz001\AppData\Roaming\broiler\profiles`; ordered `10 day`, `20 night` |
| Real profile application | `broiler profile apply day --monitor mon-227062c49310910f --wait` | exit 0, `day applied`, contrast then brightness both `applied`; readbacks `brightness 100`, `contrast 80` |
| Profile cycling | `broiler profile next --monitor <id>` twice | `day` then `night` applied in registry order; readbacks returned to `brightness 0`, `contrast 50` |
| Monitor selection | `broiler monitors select --monitor <id> --remember` | selection persisted; `settings set selection.remember false` cleared the stored ids again |
| Startup outcome | `broiler startup status` | `{"outcome": "resident", "monitors": ["mon-227062c49310910f"], "profiles": ["day","night"]}` |
| Icons | `broiler icons list` | the fifteen built-in names, no emoji |
| Supervised validation refusals | `monitors validate-value brightness 50 --vcp 0x10 --monitor <id>` without `--confirm-write`; with `--vcp 0xD6` | exit 2 `requires --confirm-write`; exit 5 `brightness is carried by 0x10, not 0xD6; no write was made` |
| Profile target rules | `broiler profile apply day` (no target), `profile apply nonexistent --monitor <id>` | exit 2 for a missing target; exit 2 for an unknown profile id |

Not evidenced, and therefore not claimed:

- **`flyout` and `monitors select --dialog`** need the user interface of G8 and G6 and are reported as
  not implemented. Every other command in the README contract answers.
- **Integration tests use the fake transport for the coordinator commands**, including
  `profile.apply`, `profile.next`, `monitors.select`, `profiles.reload`, `startup.status` and
  `monitors.validate_value`; `monitors list`, `monitors capabilities` and `profiles` are covered by
  their own tests. Argument validation for each command is unit-tested through the dispatcher.
- **Multi-monitor behaviour is exercised only against the fake transport.** This machine has one
  physical monitor on one HDMI port, so selection, apply-to-all-selected and identity ambiguity across
  two real displays were never observed. They are covered by fake transports with two and three
  monitors in the lifecycle and coordinator tests, and are marked untested on hardware.
- **A wrong-session client.** The kernel-side DACL has been read back and contains exactly one ACE for
  this logon session's SID, with no `Everyone`, `Anonymous`, `Authenticated Users` or `Users` entry
  (see the SDDL row above). What remains untested is a positive attempt from a *different* Windows
  logon session: this machine has one interactive logon, so no such client can be created without a
  second account or a second session. Treat the cross-session rejection as untested; the scoping it
  relies on is verified.
- **`--refresh`** is accepted and ignored; status is a snapshot taken when the coordinator started,
  so observation age is not yet reported.
- **`startup.launch_at_login` records intent only.** The Run key is not written yet, so the setting
  must not be read as "Windows will start broiler".
- Settings writes through the coordinator are not atomic (the offline path is), and `broiler start`
  does not yet apply the single-profile one-shot rule.
- Profile application in this build writes only brightness and contrast; other requested controls are
  reported as `unsupported` and make the result `partial`, never a silent success.

## Hardware evidence

Every write below was a deliberate, minimal, reversible test adjustment with a recorded baseline that was restored and verified. There is no automatic restore-on-exit behaviour in the product.

| Date | Monitor | Operation | Result |
|---|---|---|---|
| 2026-10-25 | MSI MAG275R (HDMI, `\\.\DISPLAY4`) | `GetMonitorBrightness`, `GetMonitorContrast`, VCP read `0x10`/`0x12`, capabilities string | brightness 0 (min 0, max 100), contrast 50 (min 0, max 100), VCP `0x10` current 0 max 100, VCP `0x12` current 50 max 100, code type `set_parameter`, capabilities 238 chars MCCS 2.1 |
| 2026-10-25 | BOE0852 (internal panel) | all-paths topology query, EDID registry read | detected, classified built-in, excluded from control, never probed |
| 2026-10-25 | MSI MAG275R | `set brightness 5` (baseline 0) | write accepted, readback 5, verified inside the worker and by a fresh process, baseline restored to 0 and verified |
| 2026-10-25 | MSI MAG275R | `set contrast 51` (baseline 50) | write accepted, readback 51, restored to 50 and verified |

## Gates not yet evidenced

G6 through G12 have no evidence of any kind. They are not implemented. G5 is partially evidenced above and is not claimed as complete.
