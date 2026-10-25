# UX and observed profile-state contract

Amended 2026-10-25 after the owner reviewed the G7/G8 build. This is the authoritative amendment to the original architecture's monochrome artwork and process-local-only profile-state decisions. The owner approved preserve-mode read-only matching. This document specifies future fixes; it does not claim they exist.

## 1. Preserve means read, never apply

Keep `profiles.startup = "preserve"`. On resident startup, identify selected monitors and asynchronously read the union of controls explicitly requested by loaded profiles, once per monitor/control through the single serialized worker. This is observational reconciliation, not profile application. Do not write, switch modes, issue factory/reset/save requests or emulate temperature. Show a neutral icon and no profile dot while reconciliation is pending. Do not block the UI message loop on DDC.

Match only on complete, fresh, validated evidence for every explicitly requested profile field. Unspecified fields do not participate. Compare device-native targets resolved by the same metadata/conversion code used by application planning: percentage to reported range with the planner's rounding, raw as the exact integer, named/Kelvin intent only through a validated mapping. No broad epsilon or guessed equivalence. A device-specific rounding tolerance may be added only with recorded device evidence and tests, never globally to make a profile match.

An unsupported, not-implemented, unknown, stale, timed-out or unreadable field makes that candidate unverified; a brightness-only match must not certify a profile also containing temperature. Conflicting white-point mechanisms remain invalid. A profile with no hardware fields is not a vacuous match. No additional write path is required just to observe a supported readable control, but guessing a mapping is prohibited.

On a complete match, report `matched`, not `applied`. If multiple profiles match, prefer the persisted last successfully applied profile ID only when it is among the current complete matches and its current content fingerprint matches the recorded fingerprint. Otherwise choose the first in existing deterministic registry order (`order`, then `id`). The winning current profile supplies its display name and icon. Persisted history by itself never establishes current state.

Use one coordinator-owned snapshot per monitor for every UI/CLI surface. Separate:

- current evidence: `checking`, `matched`, `applied_verified`, `custom`, `unknown`;
- pending operation and any operation error;
- historical last-applied ID/fingerprint, which is a tie-breaker/history only;
- observation timestamp, registry generation/content fingerprint and connection generation.

`custom`: complete observations establish that no candidate matches. `unknown`: observations are insufficient to establish a match or confidently exclude every candidate. Neither selects a row or claims a profile icon. Tooltip may say `Custom` or `Unknown`; the transient checking state is `Checking`. Do not represent an incomplete read as a verified mismatch.

After a successful fully verified profile apply, publish `applied_verified`. On direct writes, partial/failed writes or timeout, invalidate the old current claim immediately and reconcile by reads when safe. Unknown-outcome writes must never be retried. Reconcile after explicit refresh, valid registry reload and reconnect/wake; clear old evidence before connection-generation changes. Ordinary status, tooltip generation and flyout opening consume a cached snapshot, not synchronous hardware reads. Do not add a continuous DDC polling loop just to maintain icons.

Observation evidence is truthful as of its timestamp, not a guarantee against someone changing the monitor OSD later. Do not invent continuous monitoring. A cached last-applied label must not survive registry edits or reconnection as a verified current label. Tests cover changes between observation generations.

The flyout dot means current observed/verified match, not keyboard focus or merely the last clicked row. All surfaces consume the same selected profile ID and resolve its name/icon from the same registry generation. Cycle starts after that selected profile; if none is known, starts at the first profile. Do not temporarily mark a requested profile active before success.

## 2. Appearance and native interaction

Settings is an ordinary owned modeless Windows settings window with standard close affordance and standard themed controls, not a custom bare popup. Keep native Win32/windows-sys; do not switch to Electron, WebView, egui or a new GUI runtime. Use a common-controls v6 activation manifest, system GUI font/metrics and proper dialog navigation/message handling. A resource dialog template or a correctly styled modeless window is acceptable; window flags alone do not establish native quality. Use `CBS_DROPDOWNLIST`, not editable `CBS_DROPDOWN`.

Flyout remains a compact non-dialog popup anchored to the selected tray icon. Use a standard themed single-selection list view and image list rather than a bespoke owner-drawn button for each row. It contains profile icon, profile name and a separate current-state marker; focus/hot/selection state must remain distinguishable from that marker. Prefer a dedicated leading state column/icon with an accessible name. Standard list view gives a better base for accessibility, but does not automatically prove Narrator support: test actual labels and states. No Settings button in the flyout.

Provide hover and visible keyboard focus through native control behavior; do not remove focus indicators to hide a drawing defect. Escape and outside focus loss dismiss the flyout. Do not steal focus repeatedly. Within the open UI, arrows/Enter/Tab work normally; no global keyboard hooks, registered hotkeys or input polling. Shell keyboard activation callbacks are allowed, not a reason to install hooks. Real mouse and keyboard interaction must be tested, not only window enumeration or synthetic WM_COMMAND.

Support Per-Monitor V2 layout and WM_DPICHANGED, including icon/font recreation. High Contrast uses system colors and readable focus. Prefer a correctly themed light control surface to a partially dark, unreadable hybrid. Dark title bars/control rendering must use supported, tested paths; do not depend on undocumented uxtheme ordinals. If full native dark control support is unavailable, record that limitation rather than claiming support or guessing.

Pending/failed operation state is a short separate status area, not text appended to every profile name. Keep names stable, no parenthetical applying/failed decorations. Show useful errors without silent failures or repeated modal dialogs. Modeless UI remains responsive while one serialized writer handles operations. Coalesce/refuse conflicting repeated requests clearly; never start a second writer.

## 3. Artwork

The owner's requirement overrides the original monochrome decision for this product: curated built-in profile glyphs are colored, recognizable at actual tray size, and have the visual role of the colored emoji-style set in paveconel. Inspect that project's actual assets and licensing before choosing the art direction; do not copy a proprietary emoji font or assume its rendered glyphs are redistributable. Prefer original small transparent RGBA artwork. Stable config names remain unchanged.

One shared asset/rasterization source serves tray and flyout. Sun/day, moon/night and the other existing names must be visually distinct. Badge numerals have opaque contrasting backing and remain readable without obscuring the identifying shape. Use stable per-monitor indices and badges only when multiple monitors are controlled. Generate DPI-appropriate sizes; tests cover color content, alpha edges, coverage, distinct silhouettes and badge readability, not the old monochrome assertion. Check actual tray screenshots, not only pixel coverage numbers. High Contrast may use a deliberate monochrome/system-color fallback; color must not be the only state cue.

## 4. Live settings and monitor labels

New setting, additive within schema version 1:

```toml
[tray]
always_show_monitor_name = false
```

GUI checkbox label: `Always show monitor name`. CLI get/set and settings serialization expose the same key; missing key defaults to false. Preserve comments and use existing atomic/stale-editor-safe persistence. This is a label preference, not a target-selection change.

If exactly one monitor is controlled and the setting is false: omit the monitor name from tooltip and use `Profiles` as the flyout heading. If true, show its name. For multiple controlled monitors, always show the monitor index and name in tooltip/flyout heading regardless of the preference. This explicitly supersedes the original unconditional monitor-name requirement for the single-monitor case. Selection dialog always identifies every monitor.

Changing left-click behavior or this label setting from GUI or CLI takes effect for the running tray immediately after successful commit, without restart. Do not keep startup-only cached behavior in TrayHost. UI reads a versioned cached settings snapshot, not hardware state. CLI/GUI profile apply, cycle, reload, selection change and observation completion must all publish updates to the affected tray icon, tooltip and any open flyout through one notification path. Failed save must not update effective UI state.

## 5. Shell callback correctness

Investigate the reported NIM_SETVERSION refusal as an implementation/API-contract issue before attributing it to this Windows machine. Use current SDK declarations and official Shell_NotifyIconW/NOTIFYICONDATAW documentation. Verify cbSize, NIF_GUID/guidItem and identifier consistency, union initialization, return handling and ownership. Remove unsupported statements that an arbitrary version-3 structure size or delayed call is universally required. GetLastError may be stale when the API does not promise to set it.

Callback negotiation is per tray icon. One global boolean cannot safely decode a mix where one icon accepts version 4 and another does not. Use per-slot negotiation and route both documented encodings without ambiguous or duplicate activation. Test synthetic decoder fixtures plus real shell acceptance and actual mouse/keyboard activation. Keep legacy fallback when negotiation genuinely fails, but do not claim version-4 validation on a legacy run. Explorer recovery and icon lifetime need a real test, not just TaskbarCreated code existing.
