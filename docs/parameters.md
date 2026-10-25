# Parameters, units, and native values

Reference for the required v1 control surface. Source rationale and the numbered Microsoft/ddcutil source list are in [the architecture](architecture.md). Implementation status: nothing in this document is implemented yet. Every claim about a specific display's behavior is either marked as standard intent or left explicitly unverified.

## Value model

Three distinct kinds of value, never interchangeable:

| Kind | Written as | Meaning |
|---|---|---|
| Percent | `35`, `{ percent = 35 }` | Fraction of the monitor's usable range, 0..100. Continuous controls only. |
| Native | `{ raw = 0x80 }`, `{ raw = 5 }` | One exact byte for that control, validated against the device's range or enumerated set |
| Named intent | `"6500k"`, `"srgb"`, `{ kelvin = 6500 }` | A requested standard meaning that the daemon resolves to one native value |

Rules:

1. A bare integer for a continuous control means percent, including hexadecimal spelling. `brightness = 0x40` is 64 percent.
2. `{ raw = N }` is always the exact native value, never normalized. Decimal and hex are the same integer.
3. A native value outside the validated range or discrete set is rejected. No clamping, no substitution, no silent fallback to percent.
4. Percent and raw cannot be combined in one control. `{ percent = 35, raw = 0x40 }` is an error.
5. Discrete controls never accept percentages.
6. There is no separate "discrete value" syntax. The native value is an integer payload; it is not universally one byte. Standard non-table VCP values can use 16 bits, and each control still requires its own encoding/range checks. Whether a control is a range or an enumerated set is a property of the monitor, discovered at runtime from its capabilities and code type; the config author never declares it. `{ raw = 5 }` is therefore the same syntax for an enumerated `0x14` preset and for a range-limited continuous value. Percentage is rejected for enumerated controls, because a preset index is a choice, not a point on a percentage scale.
7. Every applied write records requested value and unit, resolved native value, backend used, observed readback, timestamp and verification state.
8. TOML has no distinct hexadecimal runtime type; hex is presentation syntax. Diagnostics print both decimal and hex for native values.

Percent mapping uses the monitor's reported minimum and maximum, not an assumed 0..100 range:

```
native = min + round(requested_percent * (max - min) / 100)
```

Readback comparison uses the resolved native value, not the unrepresentable percent. A percent request that maps outside the device range is a configuration or capability error, not a clamp.

## Control registry

| Field | VCP | Kind | Backend preference |
|---|---|---|---|
| `brightness` | `0x10` | continuous | `Get/SetMonitorBrightness` first, else `0x10` |
| `contrast` | `0x12` | continuous | `Get/SetMonitorContrast` first, else `0x12` |
| `color_temperature` | `0x14` (enumerated) or `0x0C` (request) | native integer, set- or range-validated per device | `Get/SetMonitorColorTemperature` first, else validated VCP path |
| `color_preset` | `0x14` | enumerated | `0x14` only |
| `rgb_gain.red` | `0x16` | continuous | `Get/SetMonitorRedGreenOrBlueGain` first, else `0x16` |
| `rgb_gain.green` | `0x18` | continuous | same, else `0x18` |
| `rgb_gain.blue` | `0x1A` | continuous | same, else `0x1A` |
| `volume` | `0x62` | continuous, restricted | `0x62` |
| `mute` | `0x8D` | enumerated, audio only | `0x8D` with confirmed encoding |

Not writable in v1, in any form: `0x0B` (read-only increment metadata), `0x04`, `0x05`, `0x06`, `0x08` (factory restores), `0x0A`, `0x60` (input source), `0x87` (sharpness), `0xD6` (power mode), `0xDC` (display mode), `0xDF`/`0xAC`/`0xAE` (read-only telemetry), table controls, and any vendor code. There is no generic `set-vcp` command.

## Brightness and contrast

- Percent or native continuous value.
- High-level Dxva2 functions take precedence because Microsoft recommends them and designed them so an application cannot put a monitor into an unusable state.
- High-level minimum is not assumed to be zero. Use the reported minimum and maximum from the getter's outputs.
- Contrast unavailable in a picture/HDR mode is reported as `blocked_by_mode`, distinct from unsupported hardware.
- Never adjust contrast automatically to compensate for brightness or vice versa.

## Color temperature

Exactly one per profile, in one of two shapes: a named intent, or a native value tagged with the VCP code that carries it.

```toml
color_temperature = "6500k"                        # named intent
color_temperature = { kelvin = 6500 }              # named intent, explicit
color_temperature = { vcp = 0x14, raw = 5 }        # native integer for the preset code
color_temperature = { vcp = 0x0C, raw = 35 }       # native integer for the request code
```

Both `raw` forms are the same kind of value: one native integer, exactly as sent. They differ only in which code receives it and therefore in how it is validated. `vcp = 0x14` is the enumerated preset code: the byte must be one of the values that monitor accepts, and `5` is a preset index, not five percent. `vcp = 0x0C` is the request code: the byte is checked against that monitor's reported request range. The literal `0x14`/`0x0C` is a code, the literal after `raw =` is a value — never mix those roles.

Accepted named temperatures (named intent only; a native value is not a temperature in Kelvin): `4000k`, `5000k`, `6500k`, `7500k`, `8200k`, `9300k`, `10000k`, `11500k`. This list mirrors the Windows high-level enumeration and the standard simple preset labels. It is a request vocabulary, not a claim that every display implements every entry.

Resolution order for Kelvin intent:

1. High-level temperature support advertised for that exact value: use it.
2. Otherwise, a validated `0x14` preset whose established meaning matches: use it.
3. Otherwise, a validated `0x0C` request mapping that yields the requested value within the reported resolution: use it.
4. Otherwise: fail with `unsupported` and list what the device does offer.

The backend is chosen before the first write. Never retry a different mechanism after an uncertain or timed-out write.

Preset-code semantics: the native value is the VCP payload, not a VCP code, a percentage, or a Windows enum ordinal. This project's own reference display is configured this way today: The owner configured two endpoints in Twinkle Tray: brightness 0 (night) selects native value 5 of `0x14`, and brightness 100 selects value 8. This does NOT establish the full supported set, imply support for 6 or 7, or request interpolation. Standard simple labels for 5 and 8 are 6500 K and 9300 K; their meaning on this device remains to be verified. broiler represents this use case as two profiles with explicit native choices, not an automatic brightness-to-temperature curve. Standard simple values include `0x01` sRGB, `0x02` native, `0x03` 4000 K, `0x04` 5000 K, `0x05` 6500 K, `0x06` 7500 K, `0x07` 8200 K, `0x08` 9300 K, `0x09` 10000 K, `0x0A` 11500 K, `0x0B` to `0x0D` user presets. A device may implement a different or smaller set; only values it actually accepts are writable. Named Kelvin strings map onto these labels only when the device confirms that meaning.

Temperature request semantics: `0x0C` is continuous in the standard definition, with device-specific baseline, increment and range. `0x0B` reports the increment as read-only metadata. A conversion of the form `baseline + request * increment` may be offered only after validating the actual baseline/increment for that display. Without a validated conversion, an explicit native request is still writable but is shown in native units, never labelled with an invented Kelvin value.

Color temperature changes can modify RGB gains. Dependencies are handled explicitly: re-read gains and any dependent value after the temperature operation settles. A profile may not combine `color_temperature` with RGB gains; preset-plus-gains is permitted only for a validated user-adjustable preset.

## Color presets

```toml
color_preset = "srgb"
color_preset = 0x05
color_preset = { raw = 0x05 }
```

Named values: `srgb`, `native`, `4000k`, `5000k`, `6500k`, `7500k`, `8200k`, `9300k`, `10000k`, `11500k`, `user1`, `user2`, `user3`. Names resolve to standard simple values; a name the device does not offer is rejected rather than substituted.

`color_preset` and `color_temperature` are mutually exclusive. Gains may accompany only a validated user preset. A non-user preset plus gains is a configuration error, because the firmware may ignore or overwrite the gains.

## RGB gains

- Each channel independent, percent or native continuous.
- Requires the current mode, or an explicitly requested user preset, to permit gain adjustment.
- No gamma correction, ICC profile manipulation, or software color transform.
- A profile that sets gains without a user preset is accepted only when the current mode already allows adjustment; otherwise the field is reported `blocked_by_mode`.

## Volume and mute

- `volume` is monitor-speaker volume over DDC, never the Windows master volume and never an audio-endpoint property.
- Version-aware ranges: MCCS 2.0/2.1 describe a normal continuous control, 3.0/2.2 describe a noncontinuous control with a continuous subrange. Reserved encodings inside that subrange are not writable.
- If the safe subrange cannot be established, volume reports `unsupported` rather than writing an uncertain value.
- `mute` is not a universal boolean. Standard simple values are `0x01` mute and `0x02` unmute; later definitions attach screen-blank semantics to the same code.
- Mute is writable only when the audio-only encoding is confirmed for that display and MCCS version. Otherwise it is reported `unsupported`; writing `0`, `0x01` on an unconfirmed device, or inferring support from working volume is forbidden.

## Profile schema summary

```toml
schema_version = 1            # required
id = "night"                  # required, unique lowercase slug
name = "Night"                # required, display label
icon = "night"                # required, built-in icon name
order = 20                    # optional, default 100

[monitor]
brightness = 15

[monitor.rgb_gain]
red = { percent = 100 }
```

Rules: at least one control is required; unknown keys are errors; duplicate IDs are errors; the whole directory is validated before any application; unsupported schema versions fail; a file that fails validation blocks activation of the entire registry instead of being silently dropped.

## Monitoring support and evidence

`broiler monitors capabilities --monitor <id> --json` reports, per control: whether it appeared in the capabilities string, whether a read succeeded, the reported minimum/maximum or discrete set, the current value, any named labels, whether a controlled write was ever observed, and whether the user confirmed the visible/audible effect. These are separate booleans with a timestamp and provenance, because they can disagree.

`broiler monitors validate-value` is the supervised path for a known-working value that capabilities omit:

```text
broiler monitors validate-value color-temperature 0x05 --vcp 0x14 --monitor <id> --confirm-write
```

It performs exactly one allowlisted write, reads back, records observed evidence, and asks for explicit confirmation of the visible effect before upgrading the evidence to user-validated. It never enumerates values, never loops over ranges, and never accepts vendor codes.

## Diagnostics and error vocabulary

| State | Meaning |
|---|---|
| `advertised` | Present in the capabilities string |
| `readable` | A get operation returned a value |
| `write_observed` | A write was accepted and the readback matched |
| `user_validated` | The owner confirmed the visible or audible effect |
| `blocked_by_mode` | Unavailable in the current picture/HDR/input mode |
| `unsupported` | Positively not offered by this display |
| `unverified` | No reliable evidence either way |
| `timeout` | No response within the bounded wait; hardware outcome unknown |
| `transport_error` | DDC channel failure, distinct from unsupported |
| `partial` | Some requested fields applied and verified, others did not |

A readback mismatch is a verification failure, not success. A timeout is never retried blindly and never reported as applied.
