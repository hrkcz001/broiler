# Reverse handoff — repair gates R1–R3 and the start of R4

Written by the implementation worker for the PM session. Scope: everything after the reviewed
baseline. Read this as a state report, not as a work order; the work order remains
`docs/handoff-v2-2026-10-25.md` and the appearance contract remains
`docs/ux-contract-2026-10-25.md`.

## Where the tree is

- Branch `main`, pushed. Baseline `9268ffe`, head `1b21014`.
- 329 unit tests pass, release builds. `MSYS2_ARG_CONV_EXCL='*' cmd.exe /c build.bat test` and
  `build.bat --release` on this machine.
- Commits, oldest first: `8240bc7` R1, `719d893` R2, `ff09985` R3 appearance, `05639b1` R3 silent
  defects, `9302171` UI-thread lock, `16c5288` settings window ownership, `aa02846` click cache,
  `679ad01` tooltip and checkbox, `3c19eed` R3 evidence, `0ee07eb` capabilities value lists,
  `ddebc91` VCP 0x14 write path, `8407c91` unverifiable 0x14 write, `1b21014` `applied_unverified`.
- Status documents: `status-2026-10-25-r1.md`, `-r2.md`, `-r3.md`. Screenshots in `docs/evidence-r3/`.

## Gate by gate

**R1 — closed.** Known defects fixed with regression tests: editable combo (`CBS_DROPDOWNLIST`), the
click setting read live instead of captured at install, one publication path for apply/cycle/reload/
settings/selection with DDC work off the UI thread, display name in the tooltip instead of the profile
id, menu built from one ordered list, help text that no longer denies features this build has, and
tests isolated with their own root and their own endpoint namespace through a `#[cfg(test)]` fixture
that does not exist in the release binary.

`NIM_SETVERSION`: the earlier refusal was our bug, not a property of the machine — a version-3 `cbSize`,
`uFlags = 0`, and identification by `uID` while the icon was added with `NIF_GUID`. Version negotiation
is now per icon, `cbSize` is the size of this SDK's structure, and both callback encodings are decoded
by the result each icon actually achieved.

**R2 — closed.** `preserve` reads only. It reads the union of controls the loaded profiles request,
compares against canonical native targets using the planner's own conversion, and reports `matched`
only on a complete match of every requested field. Custom only when complete observations exclude every
candidate; Unknown when a read is unsupported, failed or incomplete. Several matches are resolved by a
history hint keyed on a profile fingerprint and then by registry order. One coordinator-owned snapshot
feeds icon, tooltip, flyout and status. No polling, no writes at startup, no brightness-to-temperature
linking.

Live evidence: with the display at brightness 100 / contrast 80 the state was `Day (matched)` from a
read, no write happened, and `state.json` was not created. Editing the profile turned it into `custom`
with the exact disagreeing field named.

**R3 — closed with an explicit list of what was not observed** (below). Delivered: a native settings
dialog created from an in-memory `DLGTEMPLATEEX` with `DS_SETFONT` and `MS Shell Dlg`, themed controls,
Tab/Escape/default button through `IsDialogMessageW`; a modeless flyout that is a standard themed
`SysListView32` over an image list, with the current profile as its own leading column and the word
`Current`, focus and selection left to the control, pending and failed states in a short status line,
Escape and click-away dismissal, no Settings button; original coloured artwork with antialiased edges
and an opaque badge plate; `tray.always_show_monitor_name` with CLI parity, default `false`, and the
single-monitor rule.

**R4 — started, not closed.** VCP 0x14 temperature: a closed control enum in the DDC layer, 0x14 on the
VCP path only, worker support, a capabilities-derived preset table, and validation that refuses a value
the panel does not advertise, before any write. The remaining R4 items — autostart with an
upgrade-stable path, topology hardening, release gates, Scoop persist — are untouched.

## Decisions the PM has to ratify

1. **The tooltip no longer shows a brightness percentage.** It printed the cached reading, which is a
   hardware claim a stale cache can get wrong, and the owner asked for it to go. The tooltip now names
   the monitor per the name rule and the profile. `docs/architecture.md` was amended so the contract
   does not demand it back.
2. **A new state, `applied_unverified`.** The owner confirmed visually that a 0x14 write changes the
   colour of this panel while the readback keeps its old value. Reporting that as `failed` was wrong
   (the colour changed) and reporting `applied_verified` would be a lie (nothing was measured). A
   profile whose every write was sent but at least one is unreadable is now named `applied_unverified`:
   it is named on the icon, in the tooltip and in `status`, and it is the base the next cycle advances
   from. It is **not** in `docs/ux-contract-2026-10-25.md` yet. This is the one thing that most needs a
   PM ruling.
3. **The settings window is created unowned.** Owned by the tray's hidden `WS_EX_TOOLWINDOW` host it had
   no taskbar button and was raised every time the owner window came forward, which a right click does.
4. **One activation per physical click.** The shell was observed delivering `WM_LBUTTONDOWN`,
   `WM_LBUTTONUP` and `NIN_SELECT` for one press of a version-4 icon. Handling each opened the flyout
   three times and would have applied a profile three times. Activation is now decided per negotiated
   encoding, with a regression test over the observed triple.
5. **The click behaviour is cached and republished**, because reading it live on the UI thread waits for
   the coordinator lock that a DDC transaction holds. A change made from the command line reaches the
   next click through the same settings read the window uses.
6. **Test isolation** relies on a `#[cfg(test)]` module that constructs endpoint names and mutex names
   in a thread-local namespace. There is no production path to name another endpoint: the mutex is
   derived from the full endpoint name with SHA-256.

## Defects found and fixed, with the reason each was silent

- The dialog template wrote the signature before `dlgVer`; the loader read it as a standard template and
  `CreateDialogIndirectParamW` returned null with `GetLastError` 0. Established with a throwaway probe
  program before touching the product.
- Controls were created without `WS_CHILD`, so the dialog appeared with an empty body.
- An all-transparent bitmap came out of the image list as an opaque black square; rows without the
  marker now ask for `I_IMAGENONE`.
- The marker dot had a hard edge and read as a square at 16 px; it is coverage-blended now.
- Escape in the flyout was delivered to the list view, so the popup's window procedure never saw it; the
  message loop dismisses the popup when the key belongs to one of its children.
- The checkbox and the combo were filled from `WM_INITDIALOG`, where the control handles are still
  null: the window always opened unchecked and turning the preference off took two clicks.
- Cycle applied a profile and then forgot it, so the next cycle started from the first profile again.

## Known debt, deliberately not fixed tonight

- `monitors validate-value` still refuses `color_temperature` with a hardcoded two-control message. It is
  a third place with its own list of controls and should read the same registry the coordinator uses.
- A flyout opened by `broiler flyout` on the command line has no focus until it is clicked; the owner
  considers the click behaviour normal, so this is recorded as a minor entry-point difference, not a
  defect.
- The `0x14` readback question is open: why this MSI MAG275R accepts a preset and keeps reporting the
  old one. A hypothesis (the read reports the active preset while the write lands in a user slot that
  only acts in one picture mode) is a hypothesis, not a finding.
- `applied_unverified` needs a paragraph in the appearance contract and in `docs/parameters.md`, and the
  history rule that a tie-breaker is only ever decided by verified evidence needs to say so explicitly.

## Hardware writes performed in this session, with before and after

Everything below happened on the live test root, never on the owner's profiles. The display is
`MSI MAG275R`, id `mon-227062c49310910f`.

- Brightness and contrast through profile applies and cycles: the display moved between 0/50 (a state it
  was found in, most likely left by Twinkle Tray, which the owner then shut down), 100/80 and 20/50.
  Every write is in the log with requested value, native value and the device's readback.
- VCP 0x14 written twice with the requested `8`, on the owner's instruction to set day temperature to 8.
  Readback stayed at `5`, the owner reports the colour visibly changed. No replay: the second attempt
  was a separate, deliberate probe.

## Not observed, and therefore not claimed

Narrator, High Contrast, dark mode, display scaling above 100 %, a second DDC-capable display, Explorer
restart. The machine has one DDC-capable monitor at a single DPI in the light theme, and Explorer restart
was not coordinated. Nothing in this session is evidence about any of them.

## What the next worker should do first

1. Get a ruling on `applied_unverified` and write it into the contract, then make `status`, the tooltip
   and the docs agree on one word for it.
2. Settle the `0x14` readback question with a read-only experiment if one exists; if not, decide whether
   temperature stays write-only and say so in `docs/parameters.md`.
3. Unify the three control lists (planner, coordinator whitelist, `validate-value`) into one registry.
4. Then the rest of R4: autostart with an upgrade-stable path, topology hardening, release gates, and a
   real Scoop `persist` cycle (install, upgrade, uninstall) against `hrkcz001/scoop_bucket` with real
   hashes.
