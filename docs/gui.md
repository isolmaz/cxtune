# The tray application

`cxtune-gui` (`gui/`, a workspace member) is the Windows 11 window and notification-area icon for
cxtune. It is a separate binary from the `cxtune` command-line tool: the CLI stays a console
program that prints and returns an exit code, while `cxtune-gui.exe` is built for the Windows
subsystem (no console window) and is what "Start with Windows" launches. Both use the same
library and the same engine ([engine.md](engine.md)), and only one of them can control the dongle
at a time (`Global\cxtune-device`): a command that finds the application running reports
`busy`, and the application shows the same refusal when a command or `watch` holds the lock.

```
cxtune-gui.exe              open the window
cxtune-gui.exe --minimized  start in the notification area only (what "Start with Windows" runs)
```

The interface toolkit is [Slint](https://slint.dev) 1.18.1 with the Fluent style and the software
renderer (no GPU, no web runtime). The window follows the light or dark scheme of Windows.

## Window, icon and process

- **Tabs.** Output (mode, indicators, start delay, headphone volume, "Apply recommended"),
  Microphone (boost, noise reduction, AGC, microphone volume), Sidetone, General (start with
  Windows, language, log folder) and About. The window has a fixed size of 480 x 348 logical
  pixels and no scrolling. A "?" next to a label explains it in a box while the mouse is over it.
- **Images.** All of them are the project's own work, made for it: the icon (`gui/assets/app.ico`,
  `tray.png` and `window.png`, a blue disc with three white bars) was drawn from plain shapes,
  and the drawing of the control box on the About tab is built from Slint shapes
  (`DeviceDrawing` in `gui/ui/components.slint`). Nothing is taken from an icon set, another
  program or a manufacturer.
- **Status.** The band at the bottom shows two lines: what is wrong (or how the dongle stands) and
  what to do about it, a coloured dot, an "Open log" link when the log holds technical details,
  and the Re-apply button. Nothing else is repeated elsewhere.
- **Closing** the window hides it; the application keeps running in the notification area.
  The icon's menu has Show, the four modes (the chosen one marked with a filled circle; the
  Windows menu of Slint 1.18.1 has no check marks), Re-apply, About and Quit. A click on the
  icon shows the window. The tooltip is the same first status line as the window's. Quit stops
  the engine (a mode change in progress finishes first, which takes seconds) and ends the
  process.
- **One instance.** The first process holds the named lock `Local\cxtune-gui` and a named
  event. A second launch signals the event and exits; the first shows and raises its window.
- **Start with Windows** writes `"<path of the exe>" --minimized` to the value `cxtune` under
  `HKCU\Software\Microsoft\Windows\CurrentVersion\Run`, and removes it again; nothing is
  installed or elevated.
- **Language.** Turkish or English. The default is Turkish if the Windows display language is
  Turkish (`tr-*`) and English otherwise; the choice is saved in `%APPDATA%\cxtune\gui.toml`.
  The texts are in the interface files; the Turkish catalogue is
  `gui/lang/tr/LC_MESSAGES/cxtune-gui.po` and a test checks that every text has a translation,
  that none is left over and that the placeholders match.

## Threads

- The **UI thread** runs the Slint event loop and the handlers of the window. It never reads or
  writes the dongle and never waits for it. The window and the icon are shown first, and the
  engine is started on a short-lived thread (`cxtune-start`), because making one enumerates the
  HID devices, which takes long when one of them is slow. Its result comes back through
  `slint::invoke_from_event_loop` with the generation number, and until then the status says
  that the application is starting. Re-apply starts a new engine the same way and stops the old
  one on that thread first. Quit hides the window and the icon at once and stops the engine on a
  thread of its own; an engine that is still being started is stopped when it arrives, and a
  start that does not return ends the program after 3 s.
- The **engine** (`cxtune-engine`, with its watchdog) does all HID and silent-stream work. Its
  events reach the UI thread through `slint::invoke_from_event_loop`, tagged with a generation
  number so that events of an engine that was replaced are ignored. The user's choices go to
  the engine as `Command`s; the engine merges those that arrive within 150 ms.
- The **volume thread** (`cxtune-volume`) owns the Windows audio endpoints of the dongle.

## Controls

A setting that the user has not chosen follows the settings file, then what the dongle holds,
then the vendor's value, so a control always shows what is true. A control that the user has
used keeps the user's value. Sliders send a command at once and then at most one per 100 ms,
with the last value always sent when the slider is released.

- **Mode and start delay.** The delay slider is enabled only in minimum hiss mode, with the range
  of the core (20-2000 ms in steps of 10).
- **Mode indicators** (Hiss, Pop, Delay) show only what the documentation records
  ([firmware-notes.md](firmware-notes.md), README): hiss as the README words it (constant, some,
  reduced), the pop as recorded (none in stock mode, low in quiet mode, loud in minimum hiss) and
  the time that the start of audio is muted, as measured (none in stock and quiet mode, about
  0.6 s in balanced mode, the start delay plus 100 ms in minimum hiss). The start pop of
  balanced mode was not measured and says so.
- **Microphone boost** 0-7 shows the gain compared with the vendor's step 4, 6 dB per step. The
  gain of step 0 itself is not documented, so no absolute gain is shown. Step 6 is marked
  recommended (best signal-to-noise ratio, in a single measurement, so only indicative); a hint
  says that the step from 6 to 7 raised the noise by 6 dB.
- **Noise reduction** (measured: about 12 dB less steady noise) and **AGC** (experimental: no
  effect on steady noise could be measured, speech not tested).
- **Sidetone** is experimental and its loudness was never measured. Switching it on while the
  level is still the vendor's untouched 947 first sets 200. The level slider is logarithmic over
  the register range 1-4096. A line advises against turning on the Windows setting "Listen to
  this device" at the same time (an echo).
- **Apply recommended** sets balanced mode, microphone boost 6, noise reduction on, AGC off and
  sidetone off, and leaves the start delay and the sidetone level as they are. Balanced mode is
  the recommendation because it reduces the idle hiss at the price of 0.6 s of muted audio after
  idle, without the loud start pop that was recorded for minimum hiss (the pop of balanced mode
  itself was not measured); stock and quiet mode keep the idle hiss.
- **Volume** (headphones and microphone, with a mute switch each) is the volume of the Windows
  audio endpoints of the dongle, found by the USB vendor and product id like the silent stream's
  endpoint (`cxtune::volume`, `IAudioEndpointVolume`). No HID report is involved. While the window
  is shown the volume thread reads both endpoints twice a second, so the sliders follow the
  buttons on the dongle and the Windows mixer; for 1.2 s after the user moves a slider what was
  read is ignored. Without a dongle the sliders are disabled.

## Status

`status::status` turns the engine's `Snapshot` (or the failure to start the engine) into the two
status lines. Only the most important problem is shown; the order is:

1. a problem of the engine, most specific first (`communication`, `audio`, `refused`,
   unsupported firmware, `verify`, `drift`, `unresponsive`, `fatal`; busy, an unreadable settings
   file or a failed start when the engine could not start), each with its action. A refused
   device lock is `busy` unless a worker of this program is still stuck in a call to the dongle
   (`engine::worker_left_behind`): that is the `unresponsive` problem, and the dongle has to be
   replugged;
2. the settings file that cannot be saved (the hint is to check that the file is not locked or
   read-only);
3. something unusual that the engine reported (a warning), which stays until Re-apply and
   points to the log;
4. the state of the dongle (not connected, waiting for its firmware, applying, reading, in sync,
   not applied, stopped) with how the settings stand (saved, saving).

Paths, hexadecimal bytes and error codes are never shown. The engine writes them to the log
(`%LOCALAPPDATA%\cxtune\cxtune.log`), and so does the application for what the engine does not
log itself: a failed start, a rejected value and an unresponsive dongle. "Open log" opens the
folder.

## The interface contract

Everything that is drawn is in `gui/ui/`, and nothing there decides anything: the Rust side
(`gui/src/app.rs`, `status.rs`, `controls.rs`) owns the state and the behaviour. The files are

| File | Content |
|---|---|
| `contract.slint` | the enums and the `Model` global: what Rust sets and what the window reports |
| `texts.slint` | the translated text of every enum value |
| `theme.slint`, `components.slint`, `tabs.slint` | the look: colours, grid, widgets, the five tabs |
| `app.slint` | the status band, `MainWindow` and the `Tray` component |

A different look is a different set of `theme`, `components`, `tabs`, `texts` and `app` files
that binds the same contract; `build.rs` compiles `ui/app.slint`, which must export everything
in `contract.slint` and the `MainWindow` and `Tray` components.

`Model` (read by the window; `in-out` properties are also written by it):

| Property | Meaning |
|---|---|
| `title`, `hint`, `tone`, `details`, `reapply-enabled`, `version` | the status lines (enums `Title`, `Hint`, `Tone`), whether the log has details, whether Re-apply has something to do |
| `tab` (in-out) | the shown tab: 0 output, 1 microphone, 2 sidetone, 3 general, 4 about |
| `controls-enabled` | whether the controls that change the dongle can be used |
| `mode` (in-out), `mode-recommended`, `hiss`, `pop`, `delay`, `delay-ms` | the mode (0 stock, 1 quiet start, 2 balanced, 3 minimum hiss) and its indicators |
| `settle-ms` (in-out), `settle-min`, `settle-max`, `settle-step`, `delay-enabled` | the start delay |
| `boost` (in-out), `boost-max`, `boost-db`, `boost-recommended`, `noise-reduction` (in-out), `agc` (in-out) | the microphone |
| `sidetone` (in-out), `level-position` (in-out), `level-positions`, `level` | sidetone: the slider position and the register value it stands for |
| `speaker-present`, `speaker-volume` (in-out), `speaker-muted` (in-out), `microphone-present`, `microphone-volume` (in-out), `microphone-muted` (in-out) | the Windows volume |
| `autostart` (in-out), `autostart-failed`, `language` (in-out; 0 Turkish, 1 English) | general |

Callbacks of `Model`: `mode-selected(int)`, `settle-changed(int)`, `boost-changed(int)`,
`noise-reduction-toggled(bool)`, `agc-toggled(bool)`, `sidetone-toggled(bool)`,
`level-changed(float)`, `slider-released()`, `volume-changed(side, percent)`,
`mute-toggled(side, bool)`, `autostart-toggled(bool)`, `language-selected(int)`,
`apply-recommended()`, `reapply()` and `open-log-folder()`.

`Tray` has the properties `status-title`, `mode`, `controls-enabled` and `reapply-enabled`, and the
callbacks `show-requested`, `about-requested`, `mode-requested(int)`, `reapply-requested`,
`quit-requested` and `icon-clicked`. (Slint's `SystemTrayIcon` has a `tooltip` and a `clicked`
that its Rust API does not expose, so the component binds the tooltip from `status-title` and
forwards the click.)

## Development

`cargo build -p cxtune-gui --features snapshot` adds `--snapshot <folder>`, which renders the
window to PNG files (every tab in both languages and both colour schemes, the other modes and the
status line of every problem) and quits. The problems in those pictures are simulated through
`status::status`; the rest is the real state of the dongle. The release profile uses `lto` and
`strip`, and `panic` stays `unwind`: the engine relies on `catch_unwind` to end with a last error
state.

## Not built

The microphone level meter and the 7.1 / stage enhancer need device work and are left for a later
milestone.
