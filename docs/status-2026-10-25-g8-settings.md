# Status, 2026-10-25: G8 part two, the settings window

Fifth record for the day, covering the settings half of G8. [The flyout record](status-2026-10-25-g8-flyout.md)
stays as written.

Starting point: `55d810e`, 266 tests green.

## What was added

`src/win/settings.rs`, opened from the tray menu as `Settings`, from `broiler settings window`, and by
the `ui.settings` request. One ordinary modeless window, `WS_POPUP | WS_CAPTION | WS_SYSMENU` with
`WS_EX_TOOLWINDOW`, so the instance keeps serving while it is open.

What it shows, and what it deliberately does not:

- `Left click on the tray icon`: a `CBS_DROPDOWNLIST` with `Cycle profiles` / `Open the profile
  flyout`. A change writes `tray.left_click` through the coordinator, which keeps the user's comments
  in the file. If the write fails the combo snaps back to the stored value, so the window never shows
  a setting that is not in the file.
- `Launch at Windows startup`: present, unchecked, disabled, and captioned
  `(not registered by this build)`. This build writes no logon entry, and a checkbox that silently
  does nothing would be worse than an honest disabled row.
- A line with the counts: `1 monitor(s) controlled, 2 profile(s) loaded`, refreshed after a selection.
- `Select monitors…` opens the existing checkbox dialog and stores the answer through
  `monitors.select`.
- `Open profiles directory`, `Close`.

No settings control appears in the flyout, and none of these actions is reachable only from the
window: every one has a CLI equivalent (`settings set tray.left_click …`, `monitors select …`,
`repository open`, `broiler settings window`).

## Evidence

```text
MSYS2_ARG_CONV_EXCL='*' cmd.exe /c build.bat test
test result: ok. 270 passed; 0 failed
```

Four new tests: the combo's fixed order and a stray index never selecting the flyout, the honest
startup caption, the summary wording, and a layout that stacks without overlapping at 96/120/144/192
DPI.

Live run, release binaries, disposable portable root, `left_click = "flyout"`. The window was read
through the same Win32 calls an accessibility client uses:

```text
window   visible=True size=380x216
id=0   Static    enabled=1  'Left click on the tray icon'
id=10  ComboBox  enabled=1  items=['Cycle profiles', 'Open the profile flyout'] selected=1
id=11  Button    enabled=0  'Launch at Windows startup (not registered by this build)'
id=0   Static    enabled=1  '1 monitor(s) controlled, 2 profile(s) loaded'
id=12  Button    enabled=1  'Select monitors…'
id=13  Button    enabled=1  'Open profiles directory'
id=14  Button    enabled=1  'Close'
```

Choosing `Cycle profiles` with the notification the combo itself sends:

```text
after select: ['Cycle profiles', 'Open the profile flyout'] selected = 0
[tray] left_click = "cycle"          (settings.toml, comments and the logging section intact)
[info] tray.left_click is now cycle
```

Closing with the `Close` button destroyed the window (`FindWindowW` afterwards returns nothing), and
`broiler stop` ended the instance with no process left.

## Not verified

- `Select monitors…` was not pressed: it opens a modal dialog that needs a person to answer it. The
  code path is the same one `monitors select --dialog` already used.
- A real click and real keyboard use in the window.
- The startup row after a real logon registration, which this build cannot produce.
- Narrator announcing the window and its controls.
