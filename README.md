# broiler

Brightness Controller: a lightweight native Windows monitor utility planned in Rust.

**Status: early implementation. Working today: both executables; one resolved data root for the CLI, the coordinator and migration (`paths`, `data migrate --from-appdata`); a resident coordinator with a logon-SID-scoped pipe; `start`/`stop`/`status`/`reload`; `monitors list`, `capabilities`, `select`; `profiles list`/`validate`, `profile apply`, `profile next`; `get`/`set` for brightness and contrast with readback verification; `settings get`/`set`; `startup status`; `icons list`. Profile application now plans every target before the first write. Profiles in this build write only brightness and contrast; colour temperature and the remaining controls, the tray, the flyout, every window and startup registration are not implemented yet.**

The tray menu's `Settings` opens one ordinary window with the left-click behaviour, the monitor selection, the profiles directory and an honest, disabled row for logon startup, which this build does not register. `broiler settings window` opens the same window.

`broiler flyout --monitor <id>` opens the profile flyout of the running instance. The flyout is a single reusable modeless popup with one native button per profile, the monitor's name in its header, and no settings control: a click applies that profile through the coordinator, and the tray icon only changes once the hardware confirms it.

A resident instance shows one notification-area icon per controlled monitor, with a numeric badge when there is more than one, a tooltip carrying the verified profile or `Custom` plus the observed brightness, and a native menu with `Settings` and `Exit`. The tray icon, its menu and its tooltip are drawn by broiler itself.

A normal launch, including at logon, follows the documented lifecycle: with no controllable monitor it exits silently, with one profile it applies that profile once and exits without a tray icon, with several profiles it stays resident, and with several monitors and no recorded choice it stops and names the command that records one. The checkbox selection dialog is not implemented.

Research and architecture by **Hermes Agent** (Nous Research) at the direction of **hrkcz001**. See [architecture and acceptance gates](docs/architecture.md), [parameter reference](docs/parameters.md), [the storage contract](docs/storage.md), and [the next worker handoff](docs/next-worker-handoff.md).

## Review status, 2026-10-25

The tray and windows exist but have not passed interactive acceptance. Known defects include live Cycle changes, stale icon/tooltip updates and window styling. Next work is [the repair handoff](docs/handoff-v2-2026-10-25.md), not a claim of a finished release. The approved [UX contract](docs/ux-contract-2026-10-25.md) requires colored icons, read-only profile matching under preserve and `tray.always_show_monitor_name = false`; these amendments are not implemented yet. Existing config examples remain valid; do not add the new key to the current strict-parser build until supported.

## What it controls

External HDMI and DisplayPort monitors over DDC/CI: brightness, contrast, color temperature, color presets, RGB gains, monitor-speaker volume and audio-only mute where supported. Color temperature, including native hexadecimal preset values, is required v1 functionality, not a future feature. Support remains device- and mode-dependent.

Two executables: console `broiler.exe` and native GUI/tray `broiler-tray.exe`. No Electron, Node.js, WebView, Windows service, kernel driver, administrator installation or cloud account. Hardware commands are serialized through one user-session coordinator.

## Where data lives

Implemented as specified in [docs/storage.md](docs/storage.md): one storage context is resolved per process and passed to the CLI, the coordinator and migration. The order is an explicit `--config-dir`, then a `broiler.portable` marker beside the executable, then AppData for an unmarked install. A root that cannot be written is an error, never a reason to fall back.

A Scoop or portable install gets a data directory beside the binaries, linked by Scoop to its persistent store, so profiles and settings survive every upgrade:

```text
<install>\data\settings.toml
<install>\data\profiles\*.toml
<install>\data\state.json
<install>\data\logs\broiler.log
```

A normal unmarked installation keeps using AppData. Existing AppData data is carried over explicitly, never by a silent first-run import:

```text
broiler paths --json
broiler data migrate --from-appdata --dry-run
broiler data migrate --from-appdata
```

`--config-dir <absolute-path>` selects one unified data root, mainly for tests. Migration copies and validates; it never moves, never overwrites a conflict and never deletes the originals.

## Install through Scoop

**These commands become usable only after the first release and bucket publication. They do not install broiler today.** With Scoop already installed:

```powershell
scoop bucket add hrkcz001 https://github.com/hrkcz001/scoop_bucket
scoop install hrkcz001/broiler
```

If the bucket is already added for paveconel, do not add it twice. broiler uses the existing [hrkcz001/scoop_bucket](https://github.com/hrkcz001/scoop_bucket), not a new bucket. Afterwards:

```powershell
scoop update
scoop update broiler
scoop uninstall broiler
```

Uninstall preserves profiles and settings and removes only this installation's own startup registration. Installation does not enable startup and does not apply a monitor profile. The release ZIP will also be attached to [Releases](https://github.com/hrkcz001/broiler/releases): extract both executables together, then run `broiler.exe` from a shell or `broiler-tray.exe` interactively. No ZIP asset exists yet.

## First setup

1. Enable DDC/CI in the monitor OSD if needed. A visible image does not prove a working DDC channel. Disable competing monitor-control automation while validating.
2. Run `broiler monitors list --json` to obtain real monitor IDs. `<monitor-id>` below is a placeholder, never a literal value.
3. Run `broiler monitors capabilities --monitor <monitor-id> --json` to inspect values, units, native codes, ranges, discrete choices and evidence.
4. Run `broiler profiles open`, or create `%APPDATA%\broiler\profiles` in Explorer. Put one TOML file per profile there. Repository examples are not installed or activated automatically.
5. Run `broiler profiles validate`, then `broiler start`.

With one profile, startup applies it and exits without a tray icon. Two or more valid profiles give a resident profile switcher. A malformed file aborts activation instead of being ignored and accidentally reducing the count to one.

## Startup and tray behavior

- No controllable external monitors: silent normal-start exit.
- One controllable monitor and no saved selection: select it automatically.
- Multiple controllable monitors and no saved choice: show a checkbox selection dialog with **Remember choice**, including at Windows logon. Never silently select every monitor at autostart.
- Zero profiles: show a minimal setup path instead of generating settings.
- Exactly one valid profile: apply it to the selected monitors and exit.
- Multiple profiles: stay resident, preserve current monitor values, create one tray icon per controlled monitor with stable numeric badges when there is more than one.
- Left-click follows settings: cycle profiles, or open a modeless profile flyout whose title identifies the controlled monitor. The flyout contains no Settings button.
- Right-click offers **Settings** and **Exit**. Settings covers startup, click behavior, monitor selection, open/reload profiles and the repository link.
- Reload validates and replaces the registry without writing monitor parameters or unexpectedly exiting. The one-profile rule applies at startup.

## Configuration locations

The following describes the current implementation and the fallback for an ordinary unmarked install. For the required Scoop/portable `data` layout and safe migration, see [Where data lives](#where-data-lives) and [the storage contract](docs/storage.md).

```text
%APPDATA%\broiler\settings.toml
%APPDATA%\broiler\profiles\*.toml
%LOCALAPPDATA%\broiler\state.json
%LOCALAPPDATA%\broiler\capabilities.json
%LOCALAPPDATA%\broiler\logs\broiler.log
```

User-facing settings and profiles are UTF-8 TOML. State and capability evidence are application-owned. `--config-dir PATH` selects an alternate config root for testing but never permits a second writer in the same user session.

### Global settings

```toml
schema_version = 1

[startup]
launch_at_login = false

[tray]
left_click = "flyout" # "cycle" or "flyout"
badges = "when_multiple"

[selection]
remember = false
monitor_ids = []

[profiles]
startup = "preserve"
unsupported = "skip" # "skip" or "fail"

[logging]
level = "warn"
```

`remember = false` requires an empty persisted `monitor_ids`; session-only choices stay in memory. The selection dialog or `monitors select` saves real IDs atomically. `profiles.startup = "preserve"` concerns multi-profile startup and does not disable the single-profile one-shot rule. `skip` reports positively unsupported fields as partial application rather than success; `fail` preflights every requested control and refuses before writing. Neither hides transport or verification errors. Logging levels: `off`, `error`, `warn`, `info`, `debug`.

Launch at login is opt-in, per user and unelevated. The UI and CLI must distinguish desired startup configuration from a failed or externally disabled registration. A Run registry entry is not proof that Windows actually launched the process.

### Two minimal profiles

Save as `profiles/day.toml`:

```toml
schema_version = 1
id = "day"
name = "Day"
icon = "day"
order = 10

[monitor]
brightness = 100
contrast = 80
```

Save as `profiles/night.toml`:

```toml
schema_version = 1
id = "night"
name = "Night"
icon = "night"
order = 20

[monitor]
brightness = 0
contrast = 50
```

Those numbers are examples, not a calibration recommendation. Contrast belongs to a profile the same way brightness does: it is written only when the file names it, and a display that reports it unavailable in the current picture or HDR mode reports `blocked_by_mode` instead of skipping silently. Omitted parameters stay untouched. IDs are unique lowercase ASCII slugs; `name` is the visible label; sort order is `order`, then ID; renaming a file does not change its ID. Unknown keys, duplicate IDs, unsupported schema versions, empty profiles and invalid units are errors.

### Percentages, native integers and hexadecimal choices

Values do not all share a 0..100 range. There is exactly one native syntax, `{ raw = N }`, whether the monitor accepts a range or a short list of values:

```toml
[monitor]
brightness = 35
contrast = { raw = 0x80 }
color_preset = 0x0B
volume = { percent = 20 }

[monitor.rgb_gain]
red = { raw = 0xFF }
green = 95
blue = { percent = 85 }
```

Reference example only, not a safe universal profile. Native values must fall in the monitor's actual valid range or discrete set; nothing is clamped or guessed. Decimal and hexadecimal spellings are the same integer: `{ raw = 255 }` equals `{ raw = 0xFF }`.

For continuous controls a **bare integer always means percent**, even in hex: `brightness = 0x40` is 64 percent. Use `{ raw = 0x40 }` for native 64. Discrete presets never use percentages. TOML does not retain a runtime "hex integer" type.

### Color temperature

Choose exactly one mechanism per profile:

```toml
[monitor]
color_temperature = "6500k"
```

Equivalent Kelvin intent: `{ kelvin = 6500 }`. This requests a supported temperature, not an approximated RGB transformation.

Monitor with a short list of preset values:

```toml
[monitor]
color_temperature = { vcp = 0x14, raw = 0x05 }
```

This means VCP code `0x14`, native value `0x05`. The standard simple mapping labels it 6500 K; check your device's actual values and labels. Equivalent general-preset spelling: `color_preset = 0x05`. Never include both fields.

Monitor driven by the temperature-request control instead:

```toml
[monitor]
color_temperature = { vcp = 0x0C, raw = 0x23 }
```

`0x23` is an illustrative native request, **not a verified value for this monitor and not a universal 6500 K encoding**. Read the real request range from your display. `0x0B` is read-only increment metadata, not a writable parameter. Kelvin conversion becomes available only when that mapping is validated. A bare integer temperature is rejected as ambiguous; neither temperatures nor discrete presets are percentages.

Temperature cannot coexist with `color_preset` or RGB gains in one profile. Preset plus gains is allowed only with an established user-adjustable preset. Temperature support must never be faked through gamma ramps, overlays or guessed RGB ratios.

## Parameter overview

| Field under `[monitor]` | Accepted forms | Control |
|---|---|---|
| `brightness` | percent integer, `{ percent = N }`, `{ raw = N }` | high-level brightness or allowlisted `0x10` |
| `contrast` | same continuous forms | high-level contrast or `0x12` |
| `color_temperature` | Kelvin string/table, `{ vcp = 0x14, raw = N }`, `{ vcp = 0x0C, raw = N }` | actual monitor white point |
| `color_preset` | named string, native decimal/hex integer, `{ raw = N }` | `0x14` preset choice |
| `rgb_gain.red/green/blue` | continuous forms, independently | high-level gains or `0x16`/`0x18`/`0x1A` |
| `volume` | continuous forms, validated audio subrange only | `0x62`, monitor speakers |
| `mute` | boolean, confirmed audio-only encoding | `0x8D`, conditional |

Full unit rules and native mappings: [docs/parameters.md](docs/parameters.md). These rows are implementation requirements, not a promise that every monitor supports every control. Raw values apply only to named allowlisted controls; there is no arbitrary `set-vcp` escape hatch.

Built-in icon names: `day`, `night`, `dawn`, `dusk`, `coding`, `reading`, `movie`, `gaming`, `work`, `presentation`, `photo`, `eco`, `focus`, `monitor`, `custom`. Icons choose artwork only. `gaming` does not switch a firmware picture mode and `night` does not imply a warmer temperature.

## CLI examples

Planned v1 interface; unavailable until implementation.

```text
broiler start
broiler stop
broiler status --json
broiler status --refresh
broiler reload
broiler monitors list --json
broiler monitors capabilities --monitor <monitor-id> --json
broiler monitors select --dialog
broiler monitors select --monitor <monitor-id> --remember
broiler profiles list --json
broiler profiles validate
broiler profile apply night --monitor <monitor-id> --wait
broiler profile next --monitor <monitor-id>
broiler get color-temperature --monitor <monitor-id>
broiler set brightness 42 --monitor <monitor-id> --wait
broiler set brightness 0x80 --raw --monitor <monitor-id> --wait
broiler set color-preset 0x05 --monitor <monitor-id> --wait
broiler set color-temperature 6500 --kelvin --monitor <monitor-id> --wait
broiler set color-temperature 0x05 --vcp 0x14 --raw --monitor <monitor-id> --wait
broiler set color-temperature 0x23 --vcp 0x0C --raw --monitor <monitor-id> --wait
broiler paths --json
broiler data migrate --from-appdata --dry-run
broiler settings get --json
broiler settings set tray.left_click cycle
broiler settings set startup.launch_at_login true
broiler startup status
broiler settings open
broiler profiles open
broiler repository open
broiler icons list
```

With multiple monitors, target explicitly, or deliberately use `--all-selected` for profile application. `--raw` is a unit tag, not an unsafe-force flag. `--wait` returns a verified, partial or error result instead of treating acceptance as success. Cached status reports observation age; `--refresh` requests new bounded I/O.

When known-working values are missing from a capabilities string, the implementation must offer the supervised `monitors validate-value` path described in the parameter reference: one explicit named control and value, observed evidence recorded, no scanning and no arbitrary VCP access.

## Troubleshooting and safety

- A visible display may have no usable DDC/CI transport through a dock, adapter, KVM or MST path. Test a direct connection and report model, firmware, GPU/driver and exact path.
- Picture modes, HDR, monitor OSD changes or sleep can change availability and ranges. Refresh before trusting cached values.
- Capabilities strings are advisory. Advertised, readable, write-observed and user-validated are distinct evidence.
- A timeout has an unknown hardware outcome. Never blindly repeat a write or start concurrent replacement workers for a stuck one.
- `mute` is especially conditional: `0x8D` can carry screen-blanking semantics and must not rely on a guessed 0/1 encoding.
- Monitor power-off, input switching, factory reset, EEPROM-save, vendor-specific writes and automatic restore-on-exit are outside the v1 API.
- A matching readback does not prove calibrated luminance or a visible/audible effect. Physical test results stay separate from fake-backend CI.

No broiler hardware-control test has been run: the project has never written to a monitor. Read-only discovery on the development machine's MSI MAG275R returns brightness, contrast and capability data, and the built-in BOE0852 panel is excluded from control. That is an initial candidate, not a support certification. The owner reports existing monitor-side temperature control; its actual VCP mapping still has to be read and validated. Evidence and exact commands: [docs/tested-devices.md](docs/tested-devices.md).

## Build and implementation

A Cargo project with two binaries exists. Rust on the development host is installed without rustup, so cargo cannot find the MSVC linker on its own; build through the wrapper that calls `vcvars64.bat` first:

```bat
build.bat --release
build.bat test
```

From an MSYS bash shell, invoke it as `MSYS2_ARG_CONV_EXCL='*' cmd.exe /c build.bat --release`. Artifacts are `target\release\broiler.exe` and `target\release\broiler-tray.exe`. The tray executable is a GUI-subsystem binary with a Per-Monitor V2 manifest and an `asInvoker` execution level; the manifest and the version resource are compiled from `res\*.rc` by `rc.exe` during the build.

Implementation follows the ordered gates in [the architecture](docs/architecture.md): read-only topology and DDC discovery, test-driven typed configuration, bounded backend, CLI, native UI, release and Scoop verification. Gate evidence lives in [docs/tested-devices.md](docs/tested-devices.md). Do not replace these instructions with invented successful test results.

## License

Apache License 2.0. Copyright hrkcz001 2026. See [LICENSE](LICENSE) and [NOTICE](NOTICE).
