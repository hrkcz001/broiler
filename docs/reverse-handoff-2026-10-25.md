# Reverse handoff, 2026-10-25

Written at the owner's instruction: work stops here, and a review session takes over. This document
describes what was built, what is proven, what the owner found wrong with it, and what a reviewer must
decide before anyone writes more code. It is a record, not a plan.

Owner's words, which override anything below: **stop all work, the PM session will deal with what was
built**.

## State of the repository

- Branch `main`, working tree clean, head `8a0c497`, pushed to `origin`.
- Tests: `271 passed, 0 failed` (`MSYS2_ARG_CONV_EXCL='*' cmd.exe /c build.bat test`).
- Build: `MSYS2_ARG_CONV_EXCL='*' cmd.exe /c build.bat --release`.
- **A test run fails while an instance is running.** `start` leaves a coordinator holding the data
  root; six tests then report `storage_conflict`, `EXIT_USAGE` instead of the expected codes, and
  `instance::tests::the_second_acquisition_reports_an_existing_owner` fails. Run `broiler stop` first.
  This cost time twice today and is not a code defect.
- The instance started for the owner's test was stopped.

## Commits made today

| Commit | Subject |
| --- | --- |
| `ea10f84` | P0: plan every target before the first write, typed causes, exit codes |
| `5448655` | storage: one resolved root, portable marker, copy-only migration |
| `f0428e6` | docs: P0 and P1 gate results |
| `6da05df` | startup lifecycle: apply one profile once, exit on no monitor, serve only for many |
| `cef31a7` | monitor selection dialog and the zero-profile setup window |
| `ed1950d` | tray: one notification-area icon per controlled monitor |
| `7f37e7a` | docs: keyboard activation is a proposal, not a requirement |
| `55d810e` | flyout: modeless profile flyout |
| `f0a2d1a` | settings: native settings window |
| `8a0c497` | icons: fill the canvas, dot for the current profile, owner-drawn rows |

## What is proven, and how

Every claim below came from a command, not from reading code.

**P0, profile application.** `plan::plan` runs before the first write for every target; a control with
no write path reports `not_implemented`; an unknown write outcome stops the transaction instead of
being replayed. Proven by tests and by `docs/status-2026-10-25-p0-p1.md`.

**P1, storage.** One resolved root per process: `--config-dir`, then a marker-selected
executable-adjacent `data`, then AppData. `broiler paths --json` reports it; a second root is refused
with `storage_conflict`; migration is dry-run first, copy-only, and never touches hardware or
autostart. Proven live on a disposable portable root and by a live conflict test. The owner's real
`%APPDATA%\broiler` was read, never migrated.

**G6, startup lifecycle.** Zero monitors exits silently, exactly one profile applies once and exits,
several monitors without a saved selection open the dialog, several with a selection serve. Proven
live for each branch, with one DDC write recorded as `brightness 0 -> 0`.

**G7, tray.** One icon per controlled monitor, identity GUID derived from the monitor id, tooltip,
menu with `Settings`/`Exit`, Explorer-restart recovery, icons destroyed once. Proven: the shell
accepts the icon (`icon 0 installed for mon-227062c49310910f`), `stop` returns in 83 ms and the
process exits within a second. **`NIM_SETVERSION` is refused on this machine**, so version-4 callbacks
are not in effect and the decoder handles the legacy packing.

**G8, flyout and settings.** Both windows exist, are visible, and were inspected through the same
Win32 calls an accessibility client uses: the flyout is 260x126 with header `Monitor 1 — MSI MAG275R`
and rows `Day`/`Night`; the settings window is 380x216 with all controls and an honest disabled
startup row. Activating a row produced `brightness: 0% -> 0 (device reports 0)`.

## What the owner found wrong

Listed as reported, with the technical diagnosis for a reviewer. None of it is fixed; this document is
the only work done after the report.

1. **The settings window does not look like a Windows window.** It is a `WS_POPUP | WS_CAPTION |
   WS_SYSMENU` window with `WS_EX_TOOLWINDOW`, created without a dialog frame and without a themed
   template. The control look is whatever the process gets by default; there is no dark-mode handling
   and no `WM_CTLCOLOR*` response. Likely the largest visual gap.
2. **The left-click selector is an input field.** `src/win/settings.rs:347` passes
   `0x0002 | 0x0040` and the comment claims `CBS_DROPDOWNLIST`; `0x0002` is `CBS_DROPDOWN`, the
   editable style. The correct constant is `0x0003`. A one-line defect with a misleading comment.
3. **The tray icon is a monitor, not the current profile.** With no verified profile the glyph falls
   back to `Glyph::Monitor` (`src/tray.rs`, `refresh_icon`). Because nothing is verified at startup,
   this is the normal case, so the icon the owner sees is always the neutral monitor.
4. **Colour.** The artwork is monochrome by design (`Bitmap::is_monochrome`, asserted in tests) because
   `docs/architecture.md` asked for original monochrome artwork. The owner wants **coloured icons, like
   the emoji-style set used in the `paveconel` project**. This contradicts the design document, which
   must be amended, not quietly overridden.
5. **The flyout looks poor.** Rows are owner-drawn with plain GDI: stock colours, a 3x5 digit badge
   style, no theme, no hover state. The dotted rectangle the owner saw is `DrawFocusRect`
   (`src/win/flyout.rs:588`); it is the standard focus indicator drawn by hand, and it reads as a
   stray rectangle because nothing else in the row follows the system theme.
6. **`cycle` does not work.** `left_click` is read **once**, when the icons are installed
   (`src/runtime.rs:502`), so switching to `cycle` in the settings window changes the file and the
   window but not the behaviour until the process restarts. Additionally the cycle path never
   refreshes the icon or the tooltip, so even a successful cycle looks like nothing happened.
7. **The tooltip shows the profile's file id, not its name.** `src/runtime.rs:684` stores
   `applied_profile(&id)`, which is the profile id (`night`), and `tray::tooltip` prints it directly.
8. **The context menu has a stray separator first.** `src/tray.rs:829` inserts a separator at position
   `MENU_SETTINGS` (=1) while the menu is still being built, which lands in the wrong place.
9. **Nothing is selected at startup; the state reads `Custom`.** See the next section: this is a design
   decision, not an oversight.
10. **New requirement: `Always show monitor name`.** A settings checkbox; when off, the monitor name is
    not shown in the flyout header and not in the tooltip **if only one monitor is controlled**. Not
    implemented. Note that `settings::parse` is strict, so a new key needs a default and a test.

## Why "Custom" at startup, and why nothing is selected

`Coordinator::last_applied` records only what **this process** applied and verified. A resident
instance that starts and applies nothing (the owner's `profiles.startup = "preserve"`) therefore
knows no profile, and both surfaces say so instead of guessing:

- the tooltip reports `Custom`,
- the flyout draws no dot,
- the tray icon falls back to the neutral monitor glyph.

Nothing in the product inspects the monitor at startup to decide which profile matches, and nothing
persists "last applied" across restarts. `docs/next-worker-handoff.md` puts "truthful verified profile
state" in **G10**, so the gap is scheduled, but the decision it needs is a product one and belongs to
the review:

1. Show nothing until something is verified (today's behaviour, honest, feels broken to the owner).
2. Persist the last applied profile id in the state and show it marked as "last known, unverified".
3. Read brightness/contrast at startup and mark a profile whose values match.
4. Apply a chosen profile at startup instead of preserving hardware state.

Option 3 is a heuristic that can be wrong on monitors that round values; option 4 contradicts
`preserve`. Whichever is chosen changes the icon, the tooltip and the flyout together, and must be
stated in `docs/architecture.md` before implementation.

## Decisions a reviewer must revisit

- **Monochrome versus coloured artwork.** The design document and the owner disagree. The owner wins,
  but the document has to record the change with a date, and the icon tests that assert monochrome
  have to be replaced by colour tests.
- **The two windows are hand-built Win32.** `WS_POPUP` windows with stock controls, no dialog
  templates, no theme handling, no dark mode. If the owner expects ordinary Windows windows, this is
  the decision to reopen: either invest in themed drawing (`WM_CTLCOLOR*`, `DrawThemeBackground`,
  dark-mode attributes) or a dialog template, or accept a deliberately plain look.
- **Owner-drawn rows.** They exist to place a marker, an icon and a name in one row. That is the price
  of the dot; a plain list view with `LVS_EX_*` and image lists may look more native for the same
  result, and would give selection, keyboard and screen-reader behaviour for free.
- **The flyout communicates state with text.** `(applying…)` and `(failed: reason)` are honest but
  were themselves rejected as clutter once. Any redesign must keep a truthful pending/failed signal.
- **`profiles.startup = "preserve"`** is why nothing is applied at logon; every other choice here
  depends on whether that stays true.

## Work not verified, by anyone

- A real mouse click and real keyboard use in either window; Escape and focus loss; Narrator.
- `Select monitors…` was never pressed.
- The tray icon's appearance in the notification area; only that the shell accepted it.
- Explorer restart recovery, DPI changes while running, GDI or handle counts over time.
- Anything on a second display.
- Scoop installation, `persist: data` linking, upgrade and uninstall (G12, no work started).

## Rules that remain in force

- English sources, comments, UI, docs and commits. No AI attribution trailers. Apache-2.0,
  copyright hrkcz001 2026.
- Native Win32 through `windows-sys`; two binaries; one serialized hardware writer; one data root.
- No arbitrary numeric VCP writes, no vendor codes, no power mode, no input switching, no factory
  reset, no async runtime, no service, no elevation.
- **No keyboard hooks, hotkeys or global input.** The product may react only to messages the shell
  sends it. This was the owner's sharpest correction of the session.
- A hardware write needs a stated reason and recorded before/after values; an unknown outcome is
  reported, never replayed.
- The owner's temperature values are 5 at night and 8 at day; never interpolate, never link
  brightness to temperature.
- The owner's real `%APPDATA%\broiler` data must not be migrated or rewritten without explicit
  permission.
- Do not claim a gate, hardware result, release or install that a command did not produce. If reality
  contradicts a document, add a dated follow-up rather than rewriting the record.

## Suggested order for the review session

1. Fix the outright defects: `CBS_DROPDOWNLIST` (`0x0003`), the stale `left_click` snapshot, the
   profile *name* in the tooltip, the menu separator. All four are small and independent.
2. Decide the startup-state question above and write it into the architecture document; then implement
   it once, in the coordinator, so the icon, tooltip and flyout cannot disagree.
3. Re-do the artwork: coloured icons, own set, with tests that measure colour and coverage rather than
   monochrome coverage. Amend `docs/architecture.md` first.
4. Re-do the two windows' appearance against the owner's expectation of native windows; consider a
   dialog template and a list view instead of owner-drawn rows.
5. Add `Always show monitor name` with a default, a test, and the single-monitor rule.
6. Only then return to G9 (colour temperature) and the rest of the gate list.

## Where the evidence lives

- `docs/status-2026-10-25-p0-p1.md` — P0 and P1.
- `docs/status-2026-10-25-g6.md` — startup lifecycle and the windows.
- `docs/status-2026-10-25-g7.md` — tray, plus the `NIM_SETVERSION` refusal and the keyboard
  correction.
- `docs/status-2026-10-25-g8-flyout.md`, `docs/status-2026-10-25-g8-settings.md` — the two windows.
- The logs of the live runs are in the disposable roots under `%LOCALAPPDATA%\Temp`; the portable
  test root `broiler-g8` may still exist and can be deleted.
