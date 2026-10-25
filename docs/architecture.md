# broiler: research, architecture, and implementation plan

Status: design document, amended 2026-10-25 against the running implementation. The repository now contains Rust source, two binaries, a resident coordinator and a native tray, flyout and settings window; see `docs/status-2026-10-25-*.md` for gate evidence and `docs/handoff-v2-2026-10-25.md` for the work that remains. Sections marked **Amended 2026-10-25** supersede earlier text; where this document and the implementation disagree, the amendment wins and the code is the thing to change.

## Approved UX amendment, 2026-10-25

`docs/ux-contract-2026-10-25.md` supersedes the older monochrome artwork, process-local-only current-profile state and unconditional single-monitor-name rules below. The owner approved read-only exact profile matching under preserve; colored icons, native themed windows, live settings and the monitor-name preference are required. Read `docs/handoff-v2-2026-10-25.md` before further implementation. Older paragraphs are historical rationale where they conflict with this amendment.

## 1. Decision summary

- Target Windows 10 22H2 and Windows 11 desktop sessions, initially x86-64 MSVC. ARM64 is a later verified target, not an implied supported build.
- Use TOML for both user settings and one-file-per-profile storage.
- Ship `broiler.exe`, a console CLI, and `broiler-tray.exe`, a GUI-subsystem user-session daemon. The daemon includes the native tray, settings, selection dialog, and profile flyout. It is not a Windows service.
- Use direct Win32 bindings through `windows-sys`, with narrow safe wrappers and RAII for handles. Use Dxva2 monitor configuration APIs, preferring high-level controls where available and limiting low-level VCP operations to explicit needs, not a custom I2C driver or GPU-vendor API.
- Keep a single command coordinator and a single active DDC executor. Isolate potentially blocking DDC calls in an on-demand child process using the tray executable's internal worker mode.
- Use local named pipes for CLI-to-daemon IPC, with explicit logon-SID ACLs and remote clients rejected. Do not use TCP or HTTP.
- Storage follows `docs/storage.md`: one unified data root chosen by an explicit `--config-dir`, an executable-adjacent portable marker for Scoop/portable installs, and AppData only for unmarked installs. Scoop `persist: data` is required; AppData-only storage is superseded.
- Support brightness and contrast as the baseline control set. Color temperature is required in v1, with discrete hexadecimal preset values and native temperature-request values, not just percentage sliders. Expose RGB gains, color presets, volume, and audio-only mute conditionally. Never equate a standardized VCP code with a universally reliable implementation.
- Do not implement automatic startup profile writes in the multi-profile path, background DDC polling, animation, vendor hacks, arbitrary raw VCP writes, input switching, power-off, or factory resets in v1.

## 2. Evidence and reliability boundary

Microsoft explicitly warns that many monitors do not fully implement MCCS and that these APIs can cause undefined monitor behavior. It recommends physical validation before using them on arbitrary monitors.[3] Therefore no responsible implementation can promise that a list of VCP codes will never lock up any monitor. The product promise is conservative commands, bounded application-side waits, truthful status, and a published tested-device matrix, not universal firmware safety.

Microsoft recommends high-level monitor configuration functions unless a needed feature is available only at the low level.[21] Accordingly, prefer Get/SetMonitorBrightness, Get/SetMonitorContrast, and Get/SetMonitorRedGreenOrBlueGain when the device supports them. Normalize high-level controls using their reported minimum and maximum, not an assumed zero minimum. Low-level VCP is an explicit allowlisted fallback with separately validated metadata, not an automatic second write attempt after an uncertain high-level failure. High-level settings still use arbitrary manufacturer-dependent units, and their ranges may change with monitor state.[22]

Windows provides monitor enumeration, physical-monitor handles, capabilities reads, VCP reads, and VCP writes. Continuous controls have vendor-dependent maxima; noncontinuous controls have vendor-dependent allowed values.[2] A successful `GetVCPFeatureAndVCPFeatureReply` is evidence of readability, not proof that writing is implemented safely. Matching readback verifies a reported value, not necessarily visible brightness, calibrated luminance, or audible output; physical validation must check the actual effect. Its `MC_VCP_CODE_TYPE` describes set-parameter versus momentary behavior, not the continuous/noncontinuous classification; maintain the latter in the version-aware control registry. The returned maximum is undefined for noncontinuous features.[14]

The capabilities string is advisory. ddcutil documents omitted supported codes, inaccurate replies, and slow/error-prone multi-exchange capabilities requests.[18] Discovery should not reject an otherwise readable brightness control merely because the capabilities string omits it. Conversely, advertised presence must not authorize a write automatically.

Microsoft gives typical call durations of about 40 ms for a VCP read and 50 ms for a VCP write; these are not maximum-duration or cancellation guarantees.[14][3] Coalescing rapid clicks and keeping hardware I/O off the UI thread are necessary even with healthy hardware.

No DDC write has been made during this research. A read-only PnP inventory found an MSI MAG275R external display and a BOE0852 integrated display on the development machine. PnP presence proves neither DDC support nor parameter safety. This is an initial test candidate, not a compatibility claim.

HDR, monitor picture modes, GPU drivers, cloned outputs, USB-C adapters, MST chains, docks, sleep, and reconnection belong in the acceptance matrix. Do not infer Windows behavior from Linux-specific ddcutil transport workarounds or quietly implement vendor fallbacks; docks and MST hubs are a documented source of DDC transport differences.[13]

## 3. Configuration format and storage

### Format choice

TOML is the proposed format: explicit key/value syntax, comments, typed scalars, and readable sections. The specification defines UTF-8 input and supports both LF and CRLF.[1] This suits small hand-edited configuration files without introducing a bespoke language.

Alternatives considered:

- JSON: useful for machine output and IPC, unnecessarily punctuation-heavy for hand-edited profiles.
- YAML: readable, but indentation-sensitive nested structures and implicit typing add avoidable configuration ambiguity.
- KDL: clean syntax, but less familiar to the likely Windows CLI audience and an unnecessary new convention.
- RON: convenient for Rust-native structures but leaks implementation vocabulary into a user-facing format.
- INI: easy for trivial settings but weak for typed nested data and schema evolution.

Use the TOML 1.0-compatible subset even if the current parser supports a newer specification. Use `serde` for typed parsing and `toml_edit` for comment-preserving settings edits; avoid pulling two full TOML parser stacks without a measured need. Freeze dependency versions in Cargo.lock when implementation starts.

### Paths

Resolve the roaming and local AppData directories with known-folder APIs rather than the working directory or executable directory.

```text
%APPDATA%\broiler\settings.toml
%APPDATA%\broiler\profiles\day.toml
%APPDATA%\broiler\profiles\night.toml
%LOCALAPPDATA%\broiler\state.json
%LOCALAPPDATA%\broiler\capabilities.json
%LOCALAPPDATA%\broiler\logs\broiler.log
```

This AppData layout applies only to an unmarked install. The Scoop and portable layout, precedence rules and AppData migration are specified in `docs/storage.md`, which supersedes this section for packaged installations.

Settings and profiles are user-maintained. State, capabilities, last commanded/observed values, and monitor alias mappings are application-owned. Cache writes must not rewrite profiles. Machine-local remembered selections in roaming settings remain harmless because unmatched monitor IDs are never rebound to unrelated monitors.

Support an explicit `--config-dir PATH` for testing and alternate deployments. One coordinator per user/session remains the default: starting a second daemon with a different config root must report the existing root rather than run a competing writer. Portable/Scoop mode is now required per `docs/storage.md`, selected by the explicit marker rather than inferred from an executable move. Resolve and propagate one storage context to every process.

### Global settings example

```toml
schema_version = 1

[startup]
launch_at_login = false

[tray]
left_click = "flyout"
badges = "when_multiple"

[selection]
remember = false
monitor_ids = []

[profiles]
startup = "preserve"
unsupported = "skip"

[logging]
level = "warn"
```

Semantics:

- `left_click`: `cycle` or `flyout`, live-changeable.
- `badges`: `when_multiple` in v1; required multi-monitor identification is not a freely disableable option.
- `always_show_monitor_name`: default `false`, live-changeable through `broiler settings set
  tray.always_show_monitor_name` and the settings window. With exactly one controlled monitor and the
  preference off, the monitor name is omitted from the tooltip and the flyout heading. With several
  controlled monitors the name and index are always shown, whatever the preference says: the popup
  belongs to one icon and the user has to be able to tell which monitor it controls.
- `remember`: whether the selection persists across application launches. With false, `monitor_ids` must be empty and the current choice exists only in memory. Turning it off clears the persisted IDs.
- `monitor_ids`: opaque application IDs returned by `broiler monitors list --json`, never user-authored `DISPLAY1` guesses. Display examples do not invent real IDs.
- `profiles.startup`: `preserve` in v1. Multiple profiles do not imply permission to change the monitor at every logon. Single-profile startup is a separate mandatory one-shot rule.
- `profiles.unsupported`: `skip` or `fail`; invalid values and real transport failures are never treated as unsupported.
- `logging.level`: `off`, `error`, `warn`, `info`, `debug`; rotate logs with a fixed small size cap. No telemetry.
- `launch_at_login`: desired registration state. Status separately reports the actual registry registration and any failure or externally disabled startup condition that can be observed reliably. Do not claim that an HKCU Run value alone proves Windows will launch the app.

Startup registration is a quoted absolute path to `broiler-tray.exe`, stored in `HKCU\Software\Microsoft\Windows\CurrentVersion\Run`. It is user logon startup, not boot-time service startup; Windows does not guarantee immediate execution.[11] Reconcile only on an explicit settings change/reload, not on every autostart launch. Report and retain pending reconciliation errors; do not silently flip a checkbox after a failed registry write. Standard-length command-line limits must be validated before registration.

### Minimal profile

```toml
schema_version = 1
id = "night"
name = "Night"
icon = "night"
order = 20

[monitor]
brightness = 15
```

### Complete control-schema reference

```toml
# Reference only. Each optional feature requires monitor support.
schema_version = 1
id = "coding-reference"
name = "Coding"
icon = "coding"
order = 30

[monitor]
brightness = 35
contrast = { percent = 70 }
color_preset = 0x0B # Native user1 preset, subject to device validation
volume = 20
mute = false

[monitor.rgb_gain]
red = 100
green = 95
blue = 85
```

This is not a recommended universal calibration. In particular, do not modify contrast, gains, audio, or preset merely because the example contains them. The shipped initial sample profiles should change brightness only. An icon named `night` does not imply warm color, gamma correction, or any extra write.

Schema rules:

1. `schema_version = 1` is required; unsupported future versions fail clearly.
2. IDs are immutable lowercase ASCII slugs, unique across the directory; filenames may change independently. `name` is a nonempty display label; `icon` must be a known built-in name; `order` defaults to 100.
3. Sort by `order`, then ID. Cycle uses exactly this order. Profile state is per monitor, never one global current index.
4. All controls are optional, but a profile must contain at least one control. Omitted controls are untouched; no guessed default writes.
5. Use typed values, not a universal 0..100 representation. Brightness, contrast, RGB gains, and volume accept a bare integer percentage 0..100, an explicit `{ percent = 35 }`, or an explicit `{ raw = 0x80 }` native value. Exactly one unit key is allowed. Native is a single syntax regardless of whether the control accepts a range or an enumerated set: that classification comes from device metadata at runtime, not from the config author. TOML hexadecimal integers are integers, so a bare `brightness = 0x40` means 64 percent, not native value 64; native values always require the `raw` tag. Raw continuous values must fit the validated device range and permitted subrange; never normalize, clamp, or reinterpret them as percentages. Volume excludes reserved values in both modes. If its safe subrange cannot be established, volume is unsupported. Hexadecimal is presentation syntax, not a distinct runtime integer type. Preserve syntax during comment-preserving edits where practical and print both decimal/hex in diagnostics.
6. RGB gain channels may be independently omitted. If gains and `color_preset` coexist, only a validated user preset is permitted: names `user1`/`user2`/`user3`, or a raw value with established equivalent semantics. An explicit non-user or semantically unknown preset plus gains is a validation error, not an ordering trick. Gains alone require the current mode to support user adjustment; do not silently switch picture modes. `color_temperature` cannot coexist with `color_preset` or RGB gains in one profile; these are alternative white-point mechanisms, not ordered duplicate writes.
7. `color_preset` accepts a named string, a bare native integer such as `0x05`, or `{ raw = 0x05 }`; never percentages. Named color presets: `srgb`, `native`, `4000k`, `5000k`, `6500k`, `7500k`, `8200k`, `9300k`, `10000k`, `11500k`, `user1`, `user2`, `user3`. These are requested standard meanings, not guarantees of color accuracy. Accept only a known mapping for the reported MCCS version and a value supported by that device.
8. `color_temperature` is an optional profile field whose implementation is required in v1. Accept `"6500k"` or `{ kelvin = 6500 }` for a supported actual temperature setting, `{ vcp = 0x14, raw = 5 }` for a native preset value, or `{ vcp = 0x0C, raw = 35 }` for a native request value. Both raw forms are one unchanged native integer; only the receiving code and its validation differ, and the numbers above are illustrative rather than claimed encodings for this monitor. Require exactly one of these forms; reject bare integers, percentages, and ambiguous mixed unit tables. Never synthesize temperature through RGB ratios, gamma ramps, or a dimming overlay. See the temperature section below for backend choice, support evidence, and validation.
9. Unknown keys, duplicate IDs, out-of-range values, unknown icon names, empty profiles, and conflicting modes are errors with file/line/column diagnostics.
10. At startup parse every `profiles/*.toml` file before applying anything. Invalid files abort profile activation rather than reducing a multi-profile folder to one valid profile and unexpectedly applying it. Reload is transactional: keep the last good registry if any new file fails.
11. Ignore unrelated extensions and temporary editor files. No scripts, includes, environment interpolation, or executable hooks in profiles.
12. Apply settings changes via a serialized coordinator operation with atomic same-directory file replacement. Preserve user comments and reject stale edits rather than overwriting unrelated changes. A failed save or registry reconciliation is visible.

## 4. Monitor controls and safety policy

The control codes and version distinctions below are drawn from the ddcutil MCCS feature reference; a real example capabilities response shows brightness, contrast, presets, and gains together, but not universal presence.[5][4]

| Profile field | VCP | v1 policy |
|---|---|---|
| `brightness` | `0x10` | Baseline; targeted read probe even if capabilities omit it. Validate native maximum before write. |
| `contrast` | `0x12` | Baseline but optional; may be unavailable in particular picture/HDR modes. Never adjust by default. |
| `color_temperature` | High-level temperature, `0x14`, or `0x0C` | Required v1 feature; Kelvin, discrete hex preset, or native request forms. Query actual device support. |
| `color_preset` | `0x14` | Conditional; named or native decimal/hex value; version-aware noncontinuous mapping and confirmed device values. |
| `rgb_gain.red` | `0x16` | Conditional; current or explicitly selected user color mode must permit gains. |
| `rgb_gain.green` | `0x18` | Same policy. |
| `rgb_gain.blue` | `0x1A` | Same policy. |
| `volume` | `0x62` | Conditional monitor-speaker control, not Windows master volume; version-aware range. |
| `mute` | `0x8D` | Conditional audio-only semantics; not a generic boolean write to unknown firmware. |

Mute deserves special caution. The feature is named Audio mute/Screen blank; MCCS 2.2 introduces complex screen-blanking semantics. The old simple values are 1 for mute and 2 for unmute, not a universal 0/1 convention.[5] In v1, allow `mute` only for a confirmed audio-only encoding and validated device/version combination. Otherwise report unsupported; never guess, write 0, or risk screen blanking. The same caution applies when the version report is missing or contradictory.

Temperature request `0x0C` is required v1 scope; increment `0x0B` is read-only.

Standardized but deliberately excluded from the ordinary v1 profile schema:


- `0x87` sharpness: classification differs across MCCS versions; lower practical value than the core controls and not a universally continuous slider.[5]
- `0x60` input source and `0xD6` power mode: can remove the control channel or visible image; out of scope for routine safe profile switching.[5]
- Factory reset, restore defaults, DDC disable, save-current-settings/EEPROM writes, geometry, orientation, overdrive, dynamic contrast, HDR toggles, gamma tables, vendor-specific codes, and arbitrary numeric `set-vcp` writes: excluded.
- `0xDC` application/display mode is not the meaning of the profile name or icon. A `gaming` icon does not switch an unknown firmware game mode.[5]

Conditional features are part of the common config schema but not automatically enabled for every display. Status distinguishes `advertised`, `readable`, `write_verified`, `unsupported`, `blocked_by_mode`, `unverified`, and `unavailable`; evidence can overlap and must not be compressed into a misleading single supported boolean.

### Color temperature: required v1 support

The user already uses monitor-side temperature control. This is a requirement and user-reported working behavior, not proof of the VCP code or values used on the MSI. Read the actual settings and capabilities; do not infer encoding from an OSD label.

- **Kelvin intent**: `color_temperature = "6500k"` or `{ kelvin = 6500 }`. Prefer `Get/SetMonitorColorTemperature` when the requested value is supported. Win32 `MC_COLOR_TEMPERATURE` enum ordinals are NOT native VCP preset values.[24][25] When high-level support is absent, choose a validated matching `0x14` preset or a validated `0x0C` request mapping before any write. Never fall back after an uncertain write or silently round to another temperature.
- **Native preset value**: `color_temperature = { vcp = 0x14, raw = 5 }`. This writes value `5` to VCP code `0x14`; it is not code `0x05`, five percent, or a Win32 enum ordinal. The owner uses two endpoint values in Twinkle Tray: brightness 0 (night) selects native preset 5, and brightness 100 selects native preset 8 of VCP `0x14`. These are user-selected endpoints, NOT a discovered supported range or evidence that 6 and 7 are used/supported. Standard simple labels for 5 and 8 are 6500 K and 9300 K; actual device labels remain unverified.[5] `color_preset = 5` is the alternative general-preset spelling; the two fields cannot coexist. A discrete preset is an exact choice, not a percentage. The application must not interpolate intermediate preset values from the two configured endpoints.
- **Native request value**: `color_temperature = { vcp = 0x0C, raw = 0x23 }`. Same value kind as above, different receiving code, so it is validated against that monitor's request range instead of an enumerated set. The value is illustrative, not a claim about this monitor, 35 percent, or necessarily 6500 K. `0x0B` supplies read-only increment metadata.[5] A Kelvin conversion such as `3000 + request * increment` must be validated against the actual device/version baseline, increment, range and resolution before use. Unknown conversion does not prohibit a validated explicit native request: report native units without inventing Kelvin.

Required `broiler monitors capabilities --monitor <id> --json` diagnostics: known temperatures, native discrete values in decimal and hex, labels where known, current native value, min/max or discrete set as appropriate, increment metadata, evidence provenance, identity/path context and chosen backend. Do not invent finite preset sets for a continuous request. Advertised, observed and user/model-validated evidence remain distinct. A missing capabilities entry is not proof of unsupported control. Provide an explicit supervised known-value validation path (`broiler monitors validate-value color-temperature 0x05 --vcp 0x14 --monitor <id> --confirm-write`) that performs one selected allowlisted write/readback, records observed evidence and prompts for actual visual confirmation before marking it user-validated. No enumeration of values, force flag, arbitrary VCP write, or inferred Kelvin labels. Confirmation failure or unavailable interaction must leave validation incomplete. Existing positive range/discrete/mode contradictions fail closed; incomplete capability data can be supplemented by this explicit known-value test, not an automatic probe. Temperature changes may modify gains, which must be re-read.[24]

Raw values do not bypass range checks or permit arbitrary VCP access. Only the named control registry and temperature VCP selectors `0x14`/`0x0C` are writable. Never synthesize temperature with RGB ratios, gamma ramps or software overlays. Required implementation tests cover exact integer forwarding above 100, hex/decimal equality, backend enum separation, conflicting white-point controls, range/allowed-set validation, and conversion representability.

### Execution rules

- Discover identity and probe only `0x10`/`0x12` initially. Read capabilities and optional fields only when requested by diagnostics or an actual profile. Do not scan all 256 codes or use no-op writes for discovery.
- Cache capability evidence by stable identity and connection/mode context. Revalidate after wake, HDR/picture mode changes when detectable, reconnection, or a contradictory reply. Cache is a hint, not authorization.
- Build an apply plan first. For `unsupported = fail`, preflight every requested control on every explicit target before the first write. This catches missing support but cannot make a multi-monitor hardware operation atomic.
- For `skip`, skip only positively unsupported controls and return their names. Unknown transport failures stop that monitor's operation. If nothing can be applied, report `not_applied`, not success.
- Serialize DDC globally at first; no overlapping monitor transactions and no concurrent CLI writer. Start with 100 ms minimum between completed transactions, one bounded retry for transient reads; no automatic replay of a timed-out or outcome-unknown write, and an initial 2 s worker-operation watchdog. These are proposed conservative defaults to tune against real measurements, not protocol guarantees.
- Order: explicitly requested color-temperature operation OR user preset, settle/re-read relevant ranges and dependent color state, gains when permitted, contrast, brightness, volume, mute. Re-read after a preset because it may reset dependent values. Stop on a dependency failure; never write gains assuming a failed user-mode change succeeded.
- Coalesce queued writes for the same monitor/field and rapid cycle requests to their latest intended target; do not enqueue an unbounded number of profile applications. Retain one in-flight transaction plus bounded pending intent.
- Read back after settling. Keep the requested typed value and unit, effective native target and VCP/backend, observed native value, timestamp, and verification status separate. Kelvin and discrete settings are not labelled percentages. Match rounded native values, not an unrepresentable requested percentage. Any further tolerance must be backed by observed model behavior, never silently widened.
- Report success only for verified applied fields. A partial operation is `partial`; a readback mismatch is not a successful profile. Preserve the last fully applied profile ID separately from any pending/partial intent.
- No rollback on every error: rollback creates additional writes and cannot restore a locked firmware. Record what changed and leave a clear partial result.
- Never periodically save to monitor nonvolatile memory, spam retries, fight another control program, or restore an old snapshot on exit. Settings remain as last commanded.

### Blocking-call isolation

Run all Dxva2 calls, including slow enumeration/capabilities operations, inside a child launched by `broiler-tray.exe --internal-ddc-worker`. The worker opens and owns its physical-monitor handles; never send raw handles across the public protocol. Use an inherited anonymous channel with an explicit handle inheritance list for the private parent/worker protocol, not a public unauthenticated worker pipe. Associate children with a kill-on-close job object.

Create the worker on demand and retire it after approximately 30 seconds idle. It holds no tray/UI ownership. This keeps ordinary steady-state residency to one GUI process, with temporary isolation when hardware is touched. The exact idle policy is tunable after memory and launch-latency measurement.

TerminateProcess is asynchronous and a worker cannot exit until pending I/O completes or cancels; always confirm termination before replacement.[23] A parent-side timeout stops waiting and can request termination of the worker process; it does not cancel a command already sent to monitor firmware, fix a wedged GPU driver, or prove the hardware state unchanged. Record a timed-out write as outcome unknown, and reconcile with a read after recovery rather than blindly replaying it. Do not start another writer until the old worker is confirmed dead; if termination/reaping cannot complete, quarantine that connection and report a degraded backend. Use bounded circuit breaking after repeated failures, with explicit refresh/wake/reconnect recovery rather than tight automatic restarts. Bound discovery as a whole as well as individual operations. A proposed 8 s discovery budget must be tested; silent startup does not mean mathematically instantaneous detection.

## 5. Monitor identity and selection

Enumerate active display topology with `QueryDisplayConfig`, request target/source device information, and join it to `EnumDisplayMonitors` and physical-monitor handles using documented device names, not enumeration order. Windows supplies friendly target names, output technology, EDID manufacturer/product IDs, connector instance, and a monitor device path.[9][19]

Stable identity strategy:

1. Manufacturer, product, and a valid unique EDID serial when available; namespace and hash these into an opaque ID.
2. If serial is absent/duplicated, use monitor device path/connection binding and expose `identity_confidence = connection_bound`.
3. Identical serial-less monitors cannot be reliably distinguished after arbitrary port swaps. Show this limitation and ask again when ambiguous. Never fall back from an unmatched saved ID to the first monitor.
4. `HMONITOR`, physical handle, adapter LUID, enumeration offset, and `DISPLAY1` are runtime routing data, not persistent identity.
5. The mapping between display topology and physical handles is an early research gate, especially clone/mirror and tiled-monitor cases. If a physical handle cannot be associated unambiguously, omit writes and report the ambiguity rather than guess.

Maintain an application-assigned positive `display_index`, shown consistently in selection, flyout title, tray badge, tooltip, and CLI. It is not promised to equal the Windows Settings display number. Persist index associations locally so reconnecting a monitor does not renumber other active monitors. Reassignment requires a deliberate topology/selection decision, not incidental enumeration order.

Remembered selection behavior:

- No remembered choice and exactly one controllable external monitor: select it automatically.
- No remembered choice and more than one: show selection dialog with checkboxes and `Remember choice`, including at logon/autostart. Do not silently select all monitors in an autostart-only exception.
- A valid saved selection: control only those IDs currently present; new displays are never silently enrolled.
- Some selected devices missing: continue with remaining selected devices; retain missing IDs for reconnection.
- All selected devices missing but other controllable displays present: show selection again, with an explicit explanation; do not retarget automatically.
- No controllable devices: silent normal-start exit, including when all detected devices fail bounded probes. A diagnostic CLI command can explain failures.
- Confirming an empty selection deliberately exits without tray icons. Cancel changes nothing; during initial startup it exits.

The selection dialog lists model/name, short identity, connector, and controllability. Unsupported displays may appear disabled with a reason when the dialog was explicitly opened; the built-in laptop panel is not selected or controlled via WMI as a hidden fallback.

## 6. Lifecycle and edge cases

Normal startup sequence:

1. Establish per-user/session coordinator ownership. If one already exists, activate it or return its status; never spawn a second hardware writer.
2. Enumerate and probe controllable external displays within the discovery budget. Zero: exit 0, no dialog, tray, or notification. Do not seed profiles or change autostart as a side effect of a zero-monitor launch.
3. Resolve remembered selection or show the required multi-monitor checkbox dialog. No selected target: exit.
4. Read/validate settings and the entire profiles directory before any profile write. A malformed explicit settings file is not silently replaced with defaults.
5. Zero profiles: show an actionable, minimal setup window with `Open profiles directory`, `Reload`, and `Exit`. Do not fabricate profiles or apply arbitrary values. The settings window remains explicitly available through the CLI even without monitors; explicit management commands bypass normal-start auto-exit.
6. Exactly one valid profile and no invalid profile files: apply it to all selected targets, await bounded results, close the worker/coordinator, and exit. Never create a tray icon. Failure still ends this one-shot run; record diagnostics and return a nonzero result to the launching CLI.
7. More than one: keep running and create one tray icon for each controlled, connected monitor. Preserve current hardware values. Initially show a neutral `monitor` icon unless the current state was actually verified against a profile. A persisted last-profile ID alone does not prove the monitor still has those values.

`broiler start` follows this exact behavior; it returns `one_shot_complete` rather than falsely reporting a running daemon when only one profile exists. There is no hidden `start` exception that forces residency.

For an already running multi-profile daemon, reload replaces only the registry, not the live hardware state. Changing to one profile does not immediately write and terminate the daemon; the one-profile rule is a startup rule. The UI can explain that a restart will use one-shot behavior. Reload to zero leaves the daemon available for settings/setup. This avoids a filesystem edit unexpectedly changing all monitors or closing management windows.

During residency, handle display/device notifications and resume events with debounced rediscovery. Remove disconnected-device icons; re-add selected monitors on return without automatic profile reapplication. If the selected set becomes empty, retain a short proposed 15 s recovery grace to absorb sleep/display-mode churn, then exit if it remains empty. No-monitor-at-launch remains an immediate silent exit after bounded discovery. Because the app then no longer runs, it cannot notice a future hotplug; the user must launch it again or wait for the next configured logon. A hotplug watcher mode would be an explicit future requirement, not silently substituted behavior.

In cycle mode an unknown/custom state goes to the first ordered profile. Direct successful parameter changes mark the monitor as `Custom`. Do not claim an old profile remains fully active merely because brightness still matches it.

## 7. Native UI and tray

### Toolkit decision

Use Win32/User32 standard controls and Common Controls, with a single UI message thread. No browser, WebView2, renderer process, Tokio runtime, eframe, or continuously repainted GPU surface. A Windows-only app already needs native control of icon identity, popup focus, pipes, topology, and startup. `tray-icon`/`muda` are reasonable cross-platform libraries but do not solve the profile flyout and add layers without a v1 portability requirement. Reconsider only if a spike shows a concrete accessibility/maintenance advantage.

Use a hidden top-level owner window rather than only a message-only window when relying on broadcast lifecycle notifications. Register for shell restart recovery and re-add icons after Explorer restarts. Explicitly delete icons and release HICON/GDI/window handles at shutdown.

### Tray

Use `Shell_NotifyIconW` with one stable GUID per monitor identity and `NOTIFYICON_VERSION_4`. Call `NIM_SETVERSION` after every icon addition, including shell restart recovery.[6] Decode callback event from `LOWORD(lParam)` and icon ID from `HIWORD(lParam)`, and support keyboard activation as well as mouse activation.[17]

> Follow-up, 2026-10-25. Keyboard activation in the sentence above means only this: Windows+B focuses
> the notification area, the arrow keys move between icons, and Enter or Space activates the focused
> one, at which point the shell sends the application `NIN_KEYSELECT`. Nothing reads or intercepts the
> keyboard; there is no hook, no hotkey and no global input in the product.
>
> It was proposed here during the research phase and is **not part of the owner's requirements**, so
> v1 does not claim it. The implementation maps the message to the same action as a left click and
> does nothing else with it.
>
> Corrected 2026-10-25, repair gate R1. The earlier version of this note recorded that
> `NIM_SETVERSION` "is refused on this machine" and treated the refusal as an OS property. That was
> wrong: the call was made with the version-3 structure size and with `uFlags = 0`, identifying the
> icon by `uID` although it had been added under `NIF_GUID`. The documented contract is narrower than
> the workaround assumed. `cbSize` is the size of this SDK's `NOTIFYICONDATAW`; the version-2 and
> version-3 sizes are documented for older `Shell32.dll` versions. `NIM_SETVERSION` "must be called
> every time a notification area icon is added", immediately, with no delay. And an icon is identified
> by the member its addition used — `guidItem` here — so a version call without `NIF_GUID` cannot find
> it. With the size and the identifier corrected, the current build's version-4 request is accepted on
> the same machine: the log of the 2026-10-25 R1 run shows `icon 0 installed for
> mon-227062c49310910f` with no refusal, where the previous build logged a refusal. Negotiation is per
> icon, and both documented callback encodings are routed by each icon's own result. See
> `docs/status-2026-10-25-r1.md`.

- Left activation: cycle this monitor only, or show its flyout according to settings. Exactly one
  callback event per click starts an activation: the shell was observed delivering `WM_LBUTTONDOWN`,
  `WM_LBUTTONUP` and `NIN_SELECT` for one physical press of a version-4 icon, so the decoder accepts
  one event per negotiated encoding and ignores the rest.
- Right activation: native `HMENU` with exactly `Settings` and `Exit`. Exit stops the whole broiler instance, not merely one icon.
- Tooltip: monitor index/name, verified profile or Custom, observed brightness when known, and short operation/error state.
- Windows may initially place icons in notification overflow; do not hack shell preferences or promise forced pinning.

### Built-in icons and badges

Initial named set: `day`, `night`, `dawn`, `dusk`, `coding`, `reading`, `movie`, `gaming`, `work`, `presentation`, `photo`, `eco`, `focus`, `monitor`, `custom`.

Use original coloured artwork: sun, crescent, sunrise, sunset, code brackets, open book, film frame, controller, briefcase, screen/lectern, camera, leaf, target, monitor, and sliders. These are semantic names, not emoji, vendor logos, or external font glyph dependencies. UI icons carry no hidden settings behavior.

Embed pre-rasterized RGBA assets at 16/20/24/32/40/48 px. Compose a bottom-right opaque high-contrast badge into the icon bitmap because the tray API takes a whole HICON, not a separately managed numeric overlay. Use hand-tuned bitmap digits and reserved icon/badge geometry so the badge does not erase the entire symbol. Single controlled monitor: no badge. Multiple controlled monitors: stable indices on every icon. Two-digit indices require an expanded badge and dedicated 16 px visual tests; do not cap supported monitors at nine or silently replace 10 with 0. Badge layout and tooltip are both required for accessibility.

Cache HICON objects by `(icon_name, display_index, DPI, theme, status)`. Recompose only when one of those changes. Update colors for taskbar/light/dark and high-contrast settings; verify actual appearance on supported Windows builds rather than assuming app theme equals taskbar theme. Profile icons remain recognizable by shape, not color: colour is a second cue, never the only one, because a colour-blind user and a High Contrast theme both remove it.

### Profile flyout

A modeless owned top-level `WS_POPUP`/tool window, not a dialog box and not a context menu. No taskbar button, no nested modal loop. Visible title/header and accessible window name: `Monitor 2 — MSI MAG275R`. Native list/list-view items show icon, profile name, and current/pending state. No Settings button, gear, footer link, or settings row in this surface.

Anchor with `Shell_NotifyIconGetRect`, which returns the icon's screen rectangle.[7] Clamp to the nearest monitor work area; fall back to the activation point if the rectangle is unavailable. Respect negative desktop coordinates, overflow tray placement, taskbar edge, and per-monitor DPI. Enable Per-Monitor V2 DPI awareness and handle DPI changes without stretching a 16 px bitmap.

Allow arrow keys, Enter, Space, Tab, and Escape; restore focus on dismissal. Close on outside activation/focus loss without fighting focus acquisition. Clicking another monitor's icon retargets the single reusable flyout. A click starts an asynchronous apply; mark pending and only commit the tray's profile icon after successful verification. Surface partial/failure state without pretending it applied. Use native controls for baseline keyboard/screen-reader accessibility; any owner drawing must preserve accessible names, selection, and focus states and be checked with Narrator.

### Implemented 2026-10-25 (appearance gate R3)

- The settings window is a native dialog created from an in-memory `DLGTEMPLATEEX`
  (`CreateDialogIndirectParamW`) with `DS_SETFONT` and `MS Shell Dlg`, so it follows the user's font
  and DPI. Tab, `Escape` and the default button come from `IsDialogMessageW` in the message loop.
  The template opens with `dlgVer` and then `signature`, and every control carries `WS_CHILD`; both
  details are silent when wrong — the loader returns null with no error, or creates an empty body.
- The flyout is a modeless `WS_POPUP` tool window holding a standard `SysListView32` with an image
  list. The current profile is a leading column with the text `Current` and a marker dot; focus and
  selection remain the control's own, so "what is true" and "what is focused" are never the same
  signal. Pending and failed operations appear in a short status line, never in a profile name.
- Profile names in the popup are the profiles' own names; nothing appends operation state to them.
- Artwork is original coloured RGBA with antialiased edges and an opaque badge plate, replacing the
  monochrome symbols. See `docs/status-2026-10-25-r3.md` for what was observed and what was not.

### Settings and monitor selection

Settings is one ordinary native window containing:

- `Launch at Windows startup` checkbox, showing desired versus detected registration errors.
- `Left-click behavior` selector: `Cycle profiles` / `Open profile selection`.
- `Select monitors...` action opening the checkbox dialog with `Remember choice`.
- `Open profiles directory` action.
- `Reload profiles` action and actionable parse diagnostics.
- `GitHub repository` link to the broiler repository.

Every action has a CLI equivalent. Settings may show current monitor values or errors read-only, but there is no scope expansion into a complex calibration application. All product UI, logs, source comments, example files, and repository documentation are English.

## 8. Process, module, and dependency architecture

```text
broiler.exe
  CLI parsing / output / daemon bootstrap
                 |
        ACL-protected local named pipe
                 |
broiler-tray.exe
  UI thread: HWND + tray + native windows
  coordinator: settings + profiles + selection + operations + state
  bounded IPC executor: commands -> coordinator, no direct writes
                 |
     inherited private IPC, on demand
                 |
broiler-tray.exe --internal-ddc-worker
  topology / physical handles / capabilities / VCP I/O
```

Start with one package, shared library modules, and two binary targets; do not create a micro-crate for every module. Suggested boundaries:

- `model`: typed profiles, settings, monitor identities, control registry, operation results.
- `config`: parse/validate, transactional registry, comment-preserving settings updates.
- `controller`: lifecycle state machine, target resolution, apply plans, coalescing.
- `backend`: transport trait, fake backend, Windows worker protocol and supervision.
- `windows`: topology joins, Dxva2 RAII, pipe ACLs, autostart, known folders.
- `ui`: tray, selection, settings, flyout, icons, DPI and accessibility.
- `ipc`: versioned public request/response types and framing.
- `cli`: command parsing, human/JSON formatting, exit mapping.

All state-changing GUI and CLI paths issue the same typed commands to the coordinator. The UI never owns DDC handles. Avoid holding shared-state locks across I/O; publish immutable snapshots or posted messages to the UI thread.

Initial dependencies: `windows-sys` with explicit features, `serde`, `serde_json`, `toml_edit`, and `lexopt` or `clap` after measuring binary-size/help ergonomics. Prefer `clap` if it materially reduces error-prone CLI work; do not hand-roll an entire parser to save an unmeasured amount of memory. Choose either a small logging facade or a minimal bounded file logger, not a broad async stack. Use an established hash implementation for persistent opaque identities; do not use randomized `DefaultHasher` as a stable ID format. No runtime SVG parser is needed.

The current `windows-sys` display module exposes the required Dxva2 physical-monitor and VCP functions directly.[20] This avoids treating a third-party DDC crate as required infrastructure. `ddc-winapi` remains a reference implementation to compare with during the discovery spike, not an adopted dependency without review.

## 9. Public IPC contract

Endpoint example:

```text
\\.\pipe\broiler.v1.<user-sid-hash>.<session-id>
```

Use a per-logon local mutex and explicit security descriptors. Endpoint naming is routing, not authentication. Microsoft documents that the default pipe ACL includes read access for Everyone and anonymous, and recommends a logon SID to prevent access from other terminal sessions.[8] Therefore:

- Explicit DACL scoped to the current logon SID, optionally SYSTEM for administration; never a NULL DACL or Everyone grant.
- `PIPE_REJECT_REMOTE_CLIENTS`; `FILE_FLAG_FIRST_PIPE_INSTANCE` on the first server endpoint to detect an unexpected existing server.[16]
- Authenticate/validate server/client session and identity where applicable, bound pipe instances and request size, reject malformed input, and time out idle clients. Do not grant generic write/create-instance rights casually; Microsoft notes their overlapping rights.[8]
- Use overlapped pipe I/O consistently to support cancellation and deadlines. Do not mix overlapped handles with synchronous calls lacking an OVERLAPPED structure.
- No arbitrary file-writing paths or shell execution verbs in IPC. `open-profiles` and `open-repository` refer only to known fixed targets. Launch worker/executable from a resolved absolute path, not PATH search.

Wire format: 32-bit little-endian payload length followed by UTF-8 JSON, maximum 64 KiB per frame initially. `protocol_version`, `request_id`, `method`, and typed `params` are required. Each connection is bounded; normal CLI uses one request per connection. Large monitor diagnostics use explicit truncation/pagination rather than unlimited allocation.

Methods: `ping`, `status`, `monitors.list`, `monitors.refresh`, `monitors.select`, `profiles.list`, `profiles.reload`, `profiles.validate`, `profiles.apply`, `profile.next`, `parameters.get`, `parameters.set`, `monitors.capabilities`, `monitors.validate_value`, `settings.get`, `settings.update`, `startup.status`, `ui.settings`, `ui.monitors`, `ui.flyout`, `paths.profiles`, `open.profiles`, `open.repository`, `shutdown`, and `operations.get`.

Commands return either a completed result or `accepted` with an operation ID; `--wait` waits using bounded polling of `operations.get`. Return per-monitor/per-control outcomes, skipped fields, verification, and typed errors such as `unsupported`, `invalid_config`, `ambiguous_monitor`, `transport_error`, `timeout`, `busy`, `partial`, and `protocol_mismatch`. A pipe connection timeout does not imply a DDC operation was rolled back. Operation IDs plus a bounded recent-ID cache prevent accidental retrying of an already accepted request. No strict exactly-once guarantee across a daemon crash is claimed.

`status` is a cached snapshot by default and does not perform fresh DDC reads. `status --refresh` and explicit parameter gets request bounded fresh I/O. Return observed-value age so stale data is not mistaken for a live reading.

## 10. Proposed CLI contract

```text
broiler start
broiler stop
broiler status [--json] [--refresh]
broiler reload
broiler monitors list [--json] [--refresh]
broiler monitors select --monitor <id> --monitor <id> --remember
broiler monitors select --none --remember
broiler monitors select --dialog
broiler profiles list [--json]
broiler profiles validate
broiler profile apply night --monitor <id> [--wait]
broiler profile apply night --all-selected [--wait]
broiler profile next --monitor <id>
broiler monitors capabilities --monitor <id> [--json]
broiler monitors validate-value color-temperature 0x05 --vcp 0x14 --monitor <id> --confirm-write
broiler get brightness --monitor <id>
broiler get color-temperature --monitor <id>
broiler set brightness 42 --monitor <id> [--wait]
broiler set rgb-gain 100,95,85 --monitor <id> [--wait]
broiler set brightness 0x80 --raw --monitor <id> [--wait]
broiler set color-preset 0x05 --monitor <id> [--wait]
broiler set color-temperature 6500 --kelvin --monitor <id> [--wait]
broiler set color-temperature 0x05 --vcp 0x14 --raw --monitor <id> [--wait]
broiler set color-temperature 0x23 --vcp 0x0C --raw --monitor <id> [--wait]
broiler settings get [key] [--json]
broiler settings set tray.left_click cycle
broiler settings set profiles.unsupported fail
broiler settings set startup.launch_at_login true
broiler startup status
broiler settings open
broiler profiles open
broiler repository open
broiler icons list
broiler flyout --monitor <id>
```

`set` supports each field in the control registry, not only brightness. Profile/setting keys, values, and errors must be identical across GUI and CLI. Schema/version and invariant keys such as badge policy are not mutable application preferences. Provide batch settings updates to set remember/monitor IDs coherently rather than accepting invalid intermediate states.

Target selection: require an explicit target when multiple are available; never silently broadcast a direct set command. `--monitor` accepts a stable ID or an explicitly labelled current application index; scripts should use IDs. `--all-selected` deliberately applies to the current controlled set and errors if that set is empty.

When the daemon is running, hardware commands always route through it. When absent, read-only configuration queries need no daemon. Hardware CLI operations can launch a transient coordinator with an explicit command, run it without selection dialogs, await the result, and exit. This explicit headless execution does not invoke normal startup's profile-count rules or auto-apply a sole profile. Fail with an actionable ambiguous-target error instead of opening UI from an unattended shell. Offline settings updates use the same serialization/atomic-save rules.

`stop` is idempotent. `start` returns the actual lifecycle outcome: already running, resident, one-shot complete, no controllable monitors, or error. Explicit `settings open` works even with zero monitors or one profile so setup remains reachable. GUI-subsystem and console binaries avoid console flashes and unreliable redirected-output behavior from a single hybrid executable.

Exit codes: 0 success/idempotent stop/no-monitor normal start; 2 command/config validation error; 3 daemon unavailable for a daemon-only command; 4 missing/ambiguous target; 5 unsupported requested control under strict policy; 6 timeout/transport/verification failure; 7 partial application. A skipped field is present in JSON and yields partial status/code 7, even if useful fields were applied. `status --json` reports `running: false` with code 0; process-state predicates should use that field. No fabricated asynchronous success before verification.

## 11. Test strategy and performance targets

Before implementation, separate verified evidence from objectives. No binary size, working set, idle CPU, startup time, or hardware success rate has been measured for broiler.

Proposed targets: idle daemon private bytes below 15 MiB, no periodic DDC traffic, no recurring repaint loop, effectively event-driven idle CPU, release payload below 10 MiB excluding documentation, and immediate UI pending feedback below 50 ms. Report private bytes, working set, CPU time over a documented interval, GDI/USER handles, and worker-active versus worker-idle costs separately. Reject targets only after measured evidence, not optimistic estimates.

Automated tests:

- TOML syntax and schema, unknown keys, profile conflicts, duplicate IDs, deterministic sort, comment-preserving settings writes.
- Typed percent/raw/Kelvin/discrete parsing, decimal/hex equivalence, raw values above 100, explicit VCP routing, temperature/preset/gain conflicts, unsupported discrete values, correct Kelvin backend and increment mapping, no fake RGB-temperature fallback, percentage/native mapping, zero/bogus maxima, volume reserved ranges, noncontinuous preset maps, version-dependent mute, unsupported policy, missing RGB dependencies.
- Every lifecycle branch: 0/1/many monitors and profiles, invalid profile abort, empty selection, saved missing monitors, reload transitions, hotplug grace.
- Fake backend fault injection: delayed/hung calls, partial writes, mismatch, disconnect mid-profile, no rollback assumptions, worker crash, operation deduplication.
- IPC framing limits, malformed JSON, idle clients, version mismatch, concurrent clients, shutdown, ACL/session/remote rejection, server-name collision.
- Snapshot/golden checks for icon/badge combinations and DPI; two-digit badges, monochrome/high-contrast assets, no GDI leaks over repeated icon replacement.
- Native UI smoke tests and manual Narrator/keyboard checks; overflow tray, taskbar edges, mixed DPI/negative coordinates, Explorer restart, lost focus and Escape.

Physical acceptance matrix:

- MSI MAG275R on the current machine as a first candidate, then at least one different vendor/model before broad claims; record exact model, firmware when obtainable, GPU/driver, cable, connector, and every tested control.
- Direct HDMI and DP; one supported dock/MST path if available, with failures documented rather than patched by guessing.
- SDR/HDR where available, picture modes, external OSD changes, monitor sleep/resume, suspend/resume, unplug/replug, cable/port swap, two identical serial-less displays, multi-monitor mixed support.
- Verify that brightness-only profiles do not change any unrelated control, and that mute cannot blank the screen on supported mappings.
- Test coexistence as an explicit unsupported-concurrent-writer condition: broiler cannot serialize unrelated utilities like Twinkle Tray. Advise disabling competing automation rather than silently fighting it.

GitHub-hosted CI cannot validate physical DDC behavior. Label fake-backend tests and real-monitor evidence separately.

## 12. Implementation sequence and gates

1. **Publish design-only foundation.** Public repo `hrkcz001/broiler`, Apache-2.0 license, NOTICE `Copyright hrkcz001 2026`, README crediting Hermes Agent as design author, this architecture, and sample configs. No pretend release; document future install commands with an explicit unavailable-until-release warning. Gate: sample TOML parses and source citations resolve.
2. **Read-only topology/DDC spike.** Establish x86-64 MSVC build, raw Windows topology-to-physical-handle mapping, friendly names/EDID identity, allowlisted reads, capabilities evidence, blocking behavior, worker bootstrap. No write tests until monitor/target identity is unambiguous. Gate: stable selection survives reconnect or explicitly reports ambiguity.
3. **Domain/config tests first.** Implement typed settings/control registry, profile validation and sorting, normalization, fake backend, lifecycle state machine, partial results, and transactional reload. Gate: tests cover all startup branches before enabling hardware writes.
4. **Bounded DDC backend.** RAII, worker/job supervision, private channel, sequential executor, operation deadlines, coalescing, circuit breaker, reads/range validation, then brightness-only writes/readback on a deliberate target. Gate: verified brightness operation, no UI involvement, and successful hung-worker recovery tests. Conditional controls remain disabled until individually validated.
5. **Secure coordinator/CLI.** Single instance, explicit ACL local pipes, framing, typed errors, public commands, detached GUI bootstrap, transient headless operations, status snapshots. Gate: concurrent clients share one executor; negative security and timeout tests pass.
6. **Required startup UX.** Selection dialog, remembered identities and zero/one/many-profile behavior; settings remain reachable without normal residency. Gate: full lifecycle table with fake backend plus manual single-monitor one-shot test, including the selection dialog at interactive launch AND logon whenever more than one monitor is available without a remembered choice.
7. **Tray and icon pipeline.** One icon per target, named monochrome assets, badges, per-monitor state, cycle mode, native Settings/Exit menu, shell restart recovery. Gate: mixed DPI, overflow and multi-monitor identity checks pass without leaked GDI handles.
8. **Flyout and settings.** Modeless profile list, correct monitor title, keyboard/Narrator behavior, focus/position handling, pending/partial/error states; all requested settings/actions and exact CLI parity. Gate: no settings control in flyout and no DDC blocking on UI thread.
9. **Remaining control validation.** Required color temperature (including exact hex preset and native request paths), contrast, presets/gains, speaker volume, audio-only mute. Temperature must not be deferred out of v1 or replaced by RGB approximation. Gate each device/version combination independently; document unsupported cases. Do not widen support just because a code appears in capabilities.
10. **Lifecycle and diagnostics hardening.** Wake/hotplug, worker faults, saved-selection ambiguity, invalid reloads, external changes, concurrent app behavior, log caps/privacy, performance measurement. Gate: measured resource report and actual device matrix.
11. **CI and release.** Windows formatting/lint/tests/build, schema/icon checks, locked dependencies, MSVC x64 release ZIP with both EXEs, LICENSE/NOTICE and examples; SHA256SUMS, release notes, and GitHub assets. Pin workflow actions to reviewed revisions. Gate: download the uploaded archive and verify hashes and CLI smoke tests; do not claim code signing without a signing identity.
12. **Scoop publication.** Add the verified release manifest to the existing bucket, wait for its real CI, install from it, exercise CLI/GUI/autostart and upgrade/uninstall. Gate: successful actual Scoop installation and no configuration loss. Publish only then.

## 13. Repository, licensing, releases, and existing Scoop workflow

The existing `hrkcz001/scoop_bucket` contains `bucket/paveconel.json` with GitHub release ZIP URLs, a real SHA-256, `license: Apache-2.0`, `bin`, `checkver: github`, and a versioned autoupdate URL.[10] Its current workflow validates bucket manifests under Windows PowerShell and PowerShell. Reuse that bucket and packaging convention; do not create a second bucket or copy paveconel's service/driver instructions.

Planned broiler manifest fields, not a publishable manifest yet:

- Path: `bucket/broiler.json`.
- `homepage`: `https://github.com/hrkcz001/broiler`.
- `license`: `Apache-2.0`.
- `architecture.64bit.url`: `.../releases/download/v<version>/broiler-<version>-windows-x64.zip`.
- `architecture.64bit.hash`: hash of the exact uploaded ZIP, never a placeholder promoted to the live bucket.
- `bin`: `broiler.exe`; optionally a Start Menu shortcut to `broiler-tray.exe`.
- `checkver`: `github`; matching `autoupdate` URL and released checksum metadata.
- Required `persist: data`, per `docs/storage.md`. Scoop links the application's `data` directory to persistent backing storage; ordinary nonportable installs retain AppData. Existing AppData migration is explicit, copy-only, conflict-safe and tested before UI work.
- No admin hook, driver/service installation, automatic autostart enablement, or hardware profile application during install.
- Before upgrade/uninstall, gracefully stop a running daemon so locked binaries do not break extraction. Validate hook behavior and decide whether to resume a previously running instance without violating the single-profile startup rule.
- Startup registration must use Scoop's stable `current` path, not an expiring version path or an ambiguous shim. Record and test actual resolved paths.
- On uninstall remove only broiler's own autostart registration if it points to this installation; never remove unrelated values or user profiles. Do not run an unintended one-profile apply as an install/uninstall hook.

Future user commands, valid only after publication:

```text
scoop bucket add hrkcz001 https://github.com/hrkcz001/scoop_bucket
scoop install hrkcz001/broiler
```

The repository license is Apache-2.0; copyright holder is exactly `hrkcz001 2026`. Original icon art can use the same license. Preserve third-party notices for any subsequently adopted assets or dependencies. The initial README credits Hermes Agent for the research/design and explicitly says implementation has not begun; update authorship wording only when code exists.

## 14. Decisions not to hide

- Zero profiles need a setup path; default profiles are not silently fabricated.
- Multiple-profile startup preserves monitor state unless a future explicit startup-apply feature is added.

> Implemented 2026-10-25 (repair gate R2). "Preserve" now means *read*: at startup the coordinator
> reads once, per monitor and control, the union of the controls the loaded profiles explicitly
> request, and compares the device's native values with the native targets the planner itself would
> write. A complete agreement is reported as `matched` and never as applied; complete disagreement is
> `custom`; anything less than complete evidence is `unknown`; a monitor that has not been observed is
> `checking`. The result is one coordinator-owned snapshot that the icon, the tooltip, the flyout and
> `status` all read, with the evidence timestamp next to it. A persisted per-monitor record — profile
> id plus content fingerprint — only breaks a tie between candidates the reads already proved, and is
> dropped when the write that produced it did not verify. No write, no polling and no
> brightness-temperature linking takes place during an observation. `src/reconcile.rs` holds the
> comparison, `src/state.rs` the tie-break history, and `docs/status-2026-10-25-r2.md` the evidence.
- A reload is not an automatic hardware write or exit trigger.
- A stable monitor identity is conditional on usable identity evidence, not guaranteed by a hash.
- A silent zero-monitor exit means the app cannot later detect hotplug without being restarted.
- A worker watchdog protects application responsiveness, not arbitrary monitor firmware.
- Color temperature and explicit native decimal/hex values are required v1 scope, not optional future enhancements.
- The multi-monitor selection dialog appears at logon too; there is no silent select-all exception.
- No published Scoop package exists until an actual binary release, checksum, bucket CI run, and install test exist.

Sources:
[1] https://toml.io/en/v1.0.0
[2] https://learn.microsoft.com/en-us/windows/win32/monitor/using-the-low-level-monitor-configuration-functions
[3] https://learn.microsoft.com/en-us/windows/win32/api/lowlevelmonitorconfigurationapi/nf-lowlevelmonitorconfigurationapi-setvcpfeature
[4] https://www.ddcutil.com/cap_u3011_verbose_output
[5] https://www.ddcutil.com/vcpinfo_output
[6] https://learn.microsoft.com/en-us/windows/win32/api/shellapi/nf-shellapi-shell_notifyiconw
[7] https://learn.microsoft.com/en-us/windows/win32/api/shellapi/nf-shellapi-shell_notifyicongetrect
[8] https://learn.microsoft.com/en-us/windows/win32/ipc/named-pipe-security-and-access-rights
[9] https://learn.microsoft.com/en-us/windows/win32/api/winuser/nf-winuser-displayconfiggetdeviceinfo
[10] https://github.com/hrkcz001/scoop_bucket/blob/master/bucket/paveconel.json
[11] https://learn.microsoft.com/en-us/windows/win32/setupapi/run-and-runonce-registry-keys
[13] https://www.ddcutil.com/faq
[14] https://learn.microsoft.com/en-us/windows/win32/api/lowlevelmonitorconfigurationapi/nf-lowlevelmonitorconfigurationapi-getvcpfeatureandvcpfeaturereply
[16] https://learn.microsoft.com/en-us/windows/win32/api/winbase/nf-winbase-createnamedpipea
[17] https://learn.microsoft.com/en-us/windows/win32/api/shellapi/ns-shellapi-notifyicondataw
[18] https://www.ddcutil.com/command_capabilities
[19] https://learn.microsoft.com/en-us/windows/win32/api/wingdi/ns-wingdi-displayconfig_target_device_name
[20] https://docs.rs/windows-sys/0.61.2/windows_sys/Win32/Devices/Display/index.html
[21] https://learn.microsoft.com/en-us/windows/win32/monitor/about-monitor-configuration
[22] https://learn.microsoft.com/en-us/windows/win32/monitor/using-the-high-level-monitor-configuration-functions
[23] https://learn.microsoft.com/en-us/windows/win32/api/processthreadsapi/nf-processthreadsapi-terminateprocess
[24] https://learn.microsoft.com/en-us/windows/win32/api/highlevelmonitorconfigurationapi/nf-highlevelmonitorconfigurationapi-setmonitorcolortemperature
[25] https://learn.microsoft.com/en-us/windows/win32/api/highlevelmonitorconfigurationapi/ne-highlevelmonitorconfigurationapi-mc_color_temperature
