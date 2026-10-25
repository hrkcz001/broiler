# Status, 2026-10-25: G8 part one, the profile flyout

Fourth record for the day, covering the flyout half of G8. The settings window is the other half and
is not in this record. [G7](status-2026-10-25-g7.md) and the earlier records stay as written.

Starting point: `7f37e7a`, 259 tests green.

## What was added

`src/win/flyout.rs` and the wiring that lets the tray and the command line open it.

- One reusable modeless popup per instance, `WS_POPUP` with `WS_EX_TOOLWINDOW`: no taskbar button, no
  modal loop, no dialog box. Clicking another monitor's icon replaces it rather than adding a second.
- A visible header naming the monitor exactly as the badge and the tooltip do: `Monitor 1 — MSI
  MAG275R`.
- One native push button per profile, caption carrying the state: `Night`, `Night (current)`,
  `Night (applying…)`, `Night (failed: …)`. No Settings control, no gear, no footer link.
- Anchoring through `Shell_NotifyIconGetRect`, with the pointer as the fallback when the shell cannot
  report a rectangle. Placement prefers below the icon, flips above when there is no room, then clamps
  into the work area, so an overflow-tray icon still yields a fully visible popup in signed
  coordinates.
- The flyout decides nothing. A click reports `(slot, row)` to the caller, the caller applies the
  profile on a worker thread, and the answer comes back as a posted message. Rebuilding a row's state
  is the caller's decision, which is what keeps "pending" and "failed" truthful.
- The tray icon now repaints only after a verified apply, from the profile the coordinator itself
  verified; the tooltip is refreshed at the same moment.
- `broiler flyout [--monitor <id>]` asks the running instance to show it. The window belongs to the
  process that owns the icons, so the command is a request and reports the instance's answer.

## Bugs found while proving it

1. Radio buttons applied a profile with no user action. On this build an `BS_AUTORADIOBUTTON` that
   takes focus sends `BN_CLICKED`, and the popup was applying row 0 within milliseconds of opening.
   Rows are plain push buttons now: a caption states the current profile, and only a click or Enter
   activates one. The handler additionally requires the notification's `lParam` to be a row it
   created, so a command with no sender cannot apply anything.
2. The left-click path posted messages to a window handle that was never recorded, so `flyout` as the
   configured left-click behaviour silently did nothing. The handle is stored once the icons exist.

## Evidence

```text
MSYS2_ARG_CONV_EXCL='*' cmd.exe /c build.bat test
test result: ok. 266 passed; 0 failed
```

Seven new tests: the header wording, all four row captions, placement below the icon and inside the
work area, an overflow-tray icon flipping the popup above the taskbar, negative desktop coordinates,
a popup taller than its work area being clamped, and DPI scaling with non-overlapping rows. The
command is covered too: without a coordinator it reports `EXIT_DAEMON_UNAVAILABLE`, and a missing or
unknown argument is `EXIT_USAGE`.

Live run with the release binaries, a disposable portable root, two profiles, `left_click = flyout`,
`logging.level = info`. The window was inspected through the same Win32 calls an accessibility client
uses:

```text
window      visible=True
rect        left=1575 top=906 right=1835 bottom=1032 size=260x126
child[0]  Static   'Monitor 1 — MSI MAG275R'
child[1]  Button   'Day'
child[2]  Button   'Night'
```

Activating the `Night` row with `BM_CLICK`, the documented programmatic equivalent of a click:

```text
[info] flyout row 1 chosen for slot 0
[info] mon-227062c49310910f: night applied from the flyout
[info] mon-227062c49310910f: brightness: 0% -> 0 (device reports 0)
rows after: ['Day', 'Night (current)']
```

One hardware write, with its reason and its before/after values in the log: this monitor was left at
brightness 0 by the earlier test in this session and the profile writes that same value back.

## Not verified

- A real mouse click and real keyboard use in the popup: the activation above is the button's own
  notification path, not a pointer event.
- `Escape`, focus loss, and the popup's disappearance when another window takes the foreground: the
  code handles `WM_ACTIVATE` and `VK_ESCAPE`, and no test or run exercised either.
- Narrator announcing the header and rows.
- The popup on a second display, on a vertical taskbar, and at 150/200 % after a DPI change.
- The window title is empty when read from another process, because a `WS_POPUP` window has no caption
  to read; the visible header is the accessible name in practice.
