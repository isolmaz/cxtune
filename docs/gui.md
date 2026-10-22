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

- **Layout.** The window has a fixed size of 720 x 480 logical pixels and no scrolling. The
  header holds a photo of the dongle (`gui/assets/dongle.png`), its name, a pill with the state of
  the dongle (the one place that says how it stands) and the OUT and MIC level meters. A column
  of pages on the left (Output, Microphone, Sidetone, General, each with a glyph of Segoe Fluent
  Icons) leads to the page on the right, made of Windows 11 style cards. About is a page of
  General: a row of its card opens it and the breadcrumb at its top leads back. The layout does
  the explaining: rows have a title and a control and no sub-line; what more there is to say
  sits behind the "?" of the row, which opens with the mouse or, from the keyboard, with Enter
  or Space, and is also the accessible description of the control. A group of experimental
  settings carries one "Experimental" marker above it. Left and Right move between the mode
  cards from the keyboard, Up and Down between the pages, and Space or Enter chooses.
- **Meters.** The OUT and MIC meters take a peak level in dBFS (`out-db`, `mic-db`) and a flag
  (`meters-active`). While the flag is false they are dimmed, every segment is off and there is no
  value; with a level the value appears to the left of the bars. Nothing in the application
  delivers levels yet.
- **Colours.** The window follows the light or dark scheme and the accent colour of Windows. Text
  uses the Fluent tokens TextFillColorPrimary, -Secondary and -Disabled; every secondary text has
  at least 4.5:1 contrast on its card in both schemes.
- **Type** (`Type` in `theme.slint`). Texts are 20 px semibold (page title), 16 px semibold
  (device name), 14 px (titles of rows and cards; semibold on a card header), 13 px (body, values,
  buttons) and 12 px (captions, secondary text); glyphs are 16 px, chevrons and the glyphs of the
  mode traits 12 px. The font is Segoe UI, not Segoe UI Variable: its digits are all the same
  width and Slint cannot switch on the tabular figures of the variable font.
- **Grid** (`Grid` in `theme.slint`; 4 px base). The navigation is 160 px wide, the header 64 px
  and the footer 60 px (12 px above and below the two-line status, 14 px above and below the
  32 px button). The page has 24 px at its sides and 16 px above and below, so everything on it
  is 512 px wide: the title (32 px high, at y = 16 of the page), the rows, the info bars and the
  mode cards. The title and the groups are 12 px apart, the cards of a group 4 px, the mode cards
  8 px. A card has 12 px of padding and an info bar of one line is 32 px high. A row is 48 px high
  and has a 20 px glyph column, 12 px, the label (it starts 44 px from the left edge of the row
  on every page, as does the text of an info bar) and at the right edge, 12 px inside it, the
  control: a slider is 200 px wide and is followed by 8 px and a 64 px right-aligned value, a
  switch has the word for its state on its left, a combo box or a button is 160 px wide. The
  snapshot pictures in `grid` have a guide line at each of these x positions.
- **Icon.** The icon (`gui/assets/app.ico`, `tray.png` and `window.png`, a blue rounded square
  with a white headphone) is the project's own work, drawn from plain shapes. Nothing in it is
  taken from an icon set or another program.
- **Status.** The pill in the header is the state of the dongle. The band at the bottom adds
  what is going on and what needs an action, in two lines with a coloured dot (what is wrong or
  what is being done, and what to do about it) and an "Open log" link (a stop of Tab) when the
  log holds technical details; while all is well it holds only the Re-apply button.
- **Closing** the window hides it; the application keeps running in the notification area.
  The icon's menu has Show, the four modes (the chosen one marked with a filled circle; the
  Windows menu of Slint 1.18.1 has no check marks), Re-apply, About and Quit. A click on the
  icon shows the window. The tooltip is the same first status line as the window's. Quit stops
  the engine (a mode change in progress finishes first, which takes seconds) and ends the
  process. A launch that arrives while the engine is being stopped does not bring the window
  back.
- **One instance.** The first process holds the named lock `Local\cxtune-gui` and a named
  event. A second launch signals the event and exits; the first shows and raises its window.
- **Start with Windows** writes `"<path of the exe>" --minimized` to the value `cxtune` under
  `HKCU\Software\Microsoft\Windows\CurrentVersion\Run`, and removes it again; nothing is
  installed or elevated.
- **Language.** Turkish or English. The default is Turkish if the Windows display language is
  Turkish (`tr-*`) and English otherwise; the choice is saved in `%APPDATA%\cxtune\gui.toml`.
  The texts are in the interface files; the Turkish catalogue is
  `gui/lang/tr/LC_MESSAGES/cxtune-gui.po` and a test checks that every text has a translation,
  that none is left over and that the placeholders match. The same words are used everywhere:

  | English | Turkish | Meaning |
  |---|---|---|
  | Noise floor | Gürültü tabanı | the hiss of the headphone driver at idle |
  | Click | Tık | the pop when audio starts |
  | Mute delay | Açılış gecikmesi | how long the start of audio is muted |
  | Soft-start time | Yumuşak açılış süresi | the settle time of Lowest noise |
  | Default, Low click, Balanced, Lowest noise | Varsayılan, Az tıklı, Dengeli, En sessiz | the modes `default`, `low-click`, `balanced` and `lowest-noise` |

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

- **Mode and soft-start time.** The four modes are cards across the page; the chosen one has a
  border and a check mark and the recommended one a badge. The soft-start time slider is enabled
  only in Lowest noise, with the range of the core (20-2000 ms in steps of 10).
- **Mode indicators.** Every card has three lines in the same order: noise floor, click and mute
  delay, each with a glyph in the colour of its value (a grey glyph and a dash when it was not
  measured) and the value. A legend under the cards names the glyphs once, and each line has the
  same text as a tooltip, with what is known about the value ("not measured yet", "muted for the
  soft-start time plus about 150 ms"). The values are what the documentation records
  ([firmware-notes.md](firmware-notes.md), README): the noise floor as the README words it
  (constant, some, reduced), the click as recorded (none in `default`, low in `low-click`, loud in
  `lowest-noise`) and the time that the start of audio is muted, which is the figure of the engine
  (`mode::mute_delay_ms`: none in `default` and `low-click`, 650 ms in `balanced`, the soft-start
  time plus 150 ms in `lowest-noise`). The start click of balanced mode was not measured and
  says so.
- **Microphone boost** 0-7 shows the gain compared with the vendor's step 4, 6 dB per step, as
  "+12 dB"; the "?" says what it is relative to. The gain of step 0 itself is not documented, so
  no absolute gain is shown. The "?" also says that step 6 had the best signal-to-noise ratio (in
  a single measurement, so only indicative) and that step 7 raised the noise by 6 dB. In
  balanced mode a note says that opening the microphone from idle gives 1.26 s of silence and a
  click.
- **Noise reduction** (measured: about 12 dB less steady noise) and **AGC** (experimental: a
  measurement found no effect on steady noise, speech not tested).
- **Sidetone** is experimental and its loudness was never listened to. The level is shown in dB:
  the register is shown as 0 to +16 dB in the units of the ROM's USB audio request handler
  (1/256 dB), which this dongle does not expose to Windows (its descriptor declares no feature
  unit 11), and the vendor's untouched value 947 is +3.7 dB. What the DSP makes of the register
  and how loud sidetone is were not measured, so nothing says that a low value is quiet. Only the
  window divides the register by 256. Switching sidetone on while the level is still the vendor's
  untouched 947 first sets 200. The level slider is linear in the register, so linear in dB: it
  spans 1-4096 and a key press moves it by 128, half a dB. A line advises against turning on the
  Windows setting "Listen to this device" at the same time (an echo).
- **Apply recommended** (a subtle button next to the title of the Output page, with what it
  sets as its tooltip) sets balanced mode, microphone
  boost 6, noise reduction on, AGC off and sidetone off, and leaves the soft-start time and the
  sidetone level as they are. Balanced mode is
  the recommendation because it reduces the idle hiss at the price of 0.65 s of muted audio after
  idle, without the loud start pop that was recorded for lowest noise (the pop of balanced mode
  itself was not measured); default and low click keep the idle hiss.
- **Volume** (speakers on the Output page, microphone on the Microphone page; the glyph at the
  start of the row mutes) is the volume of the Windows audio endpoints of the dongle, found by the USB
  vendor and product id like the silent stream's endpoint (`cxtune::volume`,
  `IAudioEndpointVolume`). No HID report is involved. While the window is shown the volume thread
  reads both endpoints twice a second, so the sliders follow the buttons on the dongle and the
  Windows mixer; for 1.2 s after the user moves a slider what was read is ignored. Without a
  dongle the sliders are disabled.

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
| `theme.slint`, `components.slint`, `pages.slint` | the look: colours, grid, widgets, the five pages |
| `app.slint` | the header, the navigation, the status band, `MainWindow` and the `Tray` component |

A different look is a different set of `theme`, `components`, `pages`, `texts` and `app` files
that binds the same contract; `build.rs` compiles `ui/app.slint`, which must export everything
in `contract.slint` and the `MainWindow` and `Tray` components.

`Model` (read by the window; `in-out` properties are also written by it):

| Property | Meaning |
|---|---|
| `title`, `hint`, `tone`, `details`, `reapply-enabled`, `version` | the status lines (enums `Title`, `Hint`, `Tone`), whether the log has details, whether Re-apply has something to do |
| `tab` (in-out) | the shown page: 0 output, 1 microphone, 2 sidetone, 3 general, 4 about |
| `meters-active`, `out-db`, `mic-db` | the header meters: whether there are levels, and the peak levels of the output and the microphone in dBFS (-60 to 0) |
| `controls-enabled` | whether the controls that change the dongle can be used |
| `mode` (in-out), `recommended-mode`, `indicators` | the mode (0 `default`, 1 `low-click`, 2 `balanced`, 3 `lowest-noise`), the recommended one, and the `ModeIndicators` (`hiss`, `pop`, `delay`, `delay-ms`) of every mode by index |
| `settle-ms` (in-out), `settle-min`, `settle-max`, `settle-step`, `delay-enabled` | the soft-start time |
| `boost` (in-out), `boost-max`, `boost-db`, `noise-reduction` (in-out), `agc` (in-out) | the microphone |
| `sidetone` (in-out), `level` (in-out), `level-min`, `level-max`, `level-step` | sidetone: the register value (shown as dB, 256 per dB) and the range and key step of its slider |
| `speaker-present`, `speaker-volume` (in-out), `speaker-muted` (in-out), `microphone-present`, `microphone-volume` (in-out), `microphone-muted` (in-out) | the Windows volume |
| `autostart` (in-out), `autostart-failed`, `language` (in-out; 0 Turkish, 1 English) | general |

Callbacks of `Model`: `mode-selected(int)`, `settle-changed(int)`, `boost-changed(int)`,
`noise-reduction-toggled(bool)`, `agc-toggled(bool)`, `sidetone-toggled(bool)`,
`level-changed(float)`, `slider-released()`, `volume-changed(side, percent)`,
`mute-toggled(side, bool)`, `autostart-toggled(bool)`, `language-selected(int)`,
`apply-recommended()`, `reapply()` and `open-log-folder()`.

`Theme.scheme` (in-out) is the colour scheme, bound to the one of Windows. The `snapshot` feature
sets it to draw both schemes, so it is part of the contract and a different look keeps it.

`Tray` has the properties `status-title`, `mode`, `controls-enabled` and `reapply-enabled`, and the
callbacks `show-requested`, `about-requested`, `mode-requested(int)`, `reapply-requested`,
`quit-requested` and `icon-clicked`. (Slint's `SystemTrayIcon` has a `tooltip` and a `clicked`
that its Rust API does not expose, so the component binds the tooltip from `status-title` and
forwards the click.)

## Development

`cargo build -p cxtune-gui --features snapshot` adds `--snapshot <folder>`, which renders the
window to PNG files (every page in both languages and both colour schemes, the other modes, muted
volumes, sidetone on, the meters with made-up levels and the status line of every problem) and
quits. The folder `grid` has the same pictures with the guide lines, and the folder `focus`
the English pages with the keyboard focus on each control that Tab reaches, one after the other.
Every state in those pictures is simulated: the engine is not started and the dongle is never read
or written. A simulated engine state goes through the same code as a real one
(`status::status` and the controls); the volumes and the meter levels are set directly.
`SLINT_SCALE_FACTOR=1` makes the pictures 720 x 480 pixels, the size of the window at 100 %. The
release profile uses `lto` and `strip`, and `panic` stays `unwind`: the engine relies on
`catch_unwind` to end with a last error state.

## Not built

The microphone level meter and the 7.1 / stage enhancer need device work and are left for a later
milestone.
