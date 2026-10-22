# cxtune

A small Windows tool that removes the constant idle hiss of the **HyperX Cloud II USB sound
card dongle** (USB `0951:16A4`, Conexant/Synaptics CX20773 codec, ROM `CATP_V00.15.00.00`) on
sensitive IEMs, and sets its analog microphone boost and a few microphone DSP settings.

> **Warning.** This project is not affiliated with HyperX, HP or Synaptics/Conexant. Use it at
> your own risk. It only changes the codec's **volatile RAM**: it never writes EEPROM or DSP
> memory, and unplugging/replugging the dongle (or any USB reset) restores the stock vendor
> behaviour.

## What it does

The vendor firmware patch keeps the DACs and the analog headphone driver powered at all times
to avoid pops, which leaves an audible hiss when nothing is playing. cxtune talks to the codec
over HID (the memory-access protocol used by fwupd's `synaptics-cxaudio` plugin) and changes a
few bytes of patch RAM. Every mode is a different trade between idle hiss and the noise when a
stream starts:

| Mode | Idle | Stream start | Cost |
|---|---|---|---|
| `stock` | Vendor behaviour: driver always on. Constant hiss. | No pops. | None. |
| `quiet` | ROM default idle power-down: driver parked at `0x33`. Some hiss. | Low pop. | None. |
| `balanced` | Driver clamped at `0x77`, analog references off. | Follows the ROM power-up order. | The first 600 ms of audio are muted after idle; opening the microphone from full idle gives 1.26 s of silence and a click. |
| `min-hiss` | Driver clamped at `0x77`. | Loud pop. | Audio is muted for the settle time (default 600 ms) plus 100 ms; `tune` changes the settle time. |

The exact bytes, registers and measurements behind each mode are in
[docs/firmware-notes.md](docs/firmware-notes.md).

`mic-boost <0-7>` sets `$1017` MIC_CONTROL to `0x38 + n` (analog boost in 6 dB steps; the
vendor value is 4, best measured SNR was 6).

Microphone DSP settings (what was measured on the device is in
[docs/firmware-notes.md](docs/firmware-notes.md)):

| Command | Setting | Measured |
|---|---|---|
| `mic-nr on\|off` | Noise reduction, device default on | Stationary noise 11.9 dB lower with it on. Speech not measured. |
| `mic-agc on\|off` | AGC, device default off, **experimental** | The firmware applies it, but no effect on stationary noise could be measured and speech was not tested. |
| `sidetone on\|off` | Microphone mixed into the headphones, device default off, **experimental** | The firmware starts the path (only while a microphone and a playback stream are open). Loudness not measured. |
| `sidetone-level <1-4096>` | Sidetone gain register, device default 947, **experimental** | The unit is not known; start low. Sidetone can be loud in IEMs. |

The playback EQ preset is not offered: the vendor patch resets it on every pass, so it has no
effect on this firmware.

Because RAM is reset on every replug, `watch` re-applies the remembered settings whenever the
dongle appears (see "Keeping the settings applied").

## Usage

```
cxtune status                   read and show the device state and detected mode (read-only)
cxtune apply <mode>             apply stock | quiet | balanced | min-hiss and remember it
cxtune tune <ms>                set the min-hiss settle time (20-2000 ms in steps of 10)
cxtune mic-boost <0-7>          set mic boost and remember it
cxtune mic-nr <on|off>          set microphone noise reduction and remember it
cxtune mic-agc <on|off>         set microphone AGC (experimental) and remember it
cxtune sidetone <on|off>        set sidetone (experimental) and remember it
cxtune sidetone-level <1-4096>  set the sidetone gain register (experimental) and remember it
cxtune watch                    keep the remembered settings applied across replugs and
                                wake-ups (holds the dongle; other commands are refused)
```

A noise reduction or AGC change made while a microphone stream is open is followed by a
reload request to the firmware, which applies it to the running stream at once; otherwise the
next microphone stream applies it. If the firmware already has a reload pending, that one is
waited for. If a stream opens or closes at the moment of the request, cxtune prints a warning
(the setting is written and the reload was done, but the request may have overwritten the
firmware's own update of its stream state, which is also reported when the stream bits of
`$1154` do not match the capture and playback streams after the request); the warning does not
stop `watch`. The reload also
reloads the firmware's DAC filter, as it does at every stream open; a click in the headphones
when a request is made while audio plays has not been measured.

Switching sidetone on applies the remembered level first. Changing the sidetone level while
sidetone is on moves the gain to the new value in four steps (about 0.4 s). The commands refuse
to change anything if the `$1151` byte holds bits other than the three they manage.

Settings are stored in `%APPDATA%\cxtune\config.toml`:

```toml
mode = "balanced"
mic_boost = 6
min_hiss_ms = 600
mic_nr = true
mic_agc = false
sidetone = false
sidetone_level = 947
```

Keys that are absent leave that setting as the device has it. The file is written to a temporary
file next to it and renamed over it, so it is never half-written.

A config file with keys from an earlier version (`profile`, `idle`, `clamp`) is rejected with
an error that names the file, before anything is written to the device; delete it or rewrite it
with the keys above.

Only one cxtune process controls the dongle at a time, because the read protocol has no
request tag and concurrent readers can receive each other's responses. Every command holds the
machine-wide named mutex `Global\cxtune-device` while it runs, and `watch` holds it for as long
as it runs; a command that finds it held prints `busy: another cxtune process is controlling
the dongle ...` and exits with status 1 before it reads the config or touches the device. The
mutex is released when the holder exits, also when it is killed.

The lock does not keep other programs that read codec memory over HID (for example vendor
software poking the same interface) away. The checks are meant to turn crossed responses into
a refusal rather than a wrong write, but untagged responses cannot guarantee it (a crossed
response that happens to satisfy a single-bit check passes).

## Safety design

- Every write is built in `src/patches.rs` from a closed set of known firmware images and
  constants: address, accepted before-bytes, new bytes, and guard bytes or bits elsewhere in
  memory that must match. Patches have private fields, so no other code can make one.
- `patches::apply_patch` is the only function that sends a write. It reads the target and the
  guards, refuses on any mismatch, writes, reads back, and fails loudly on a readback mismatch.
  It also refuses any address outside the known writable patch-RAM regions, and any device whose
  firmware version and chip id (`$1001..$1005`) are not the ones the routines were written for.
- A write report can only be built from a patch, and only with flag `0x40` (CPU memory). The
  EEPROM (`0x20`), DSP (`0x80`) and high-address (`0x10`) flags are never used.
- Mode changes are planned in full before the first write, from any recognised state: the four
  modes and every half-changed state an interrupted change can leave. An unrecognised state is
  refused without writing anything.
- The hook at `$14B1` is turned to RTS before the routine or its operands change and to JMP
  last, and it is only turned on when the whole routine is in place.
- A routine is only switched on when the `PLA PLA RTS` gadget it leaves through (`$1EF9`) is in
  place and, for min-hiss, the settle time is in its allowed range.
- Changes to the hook or its routine are made while a playback stream of digital silence is
  open on the dongle (`src/audio.rs`). The stream holds the headphone driver fully on, so the
  moment the hook is off causes no driver transition, and it brings the analog references up
  before balanced mode is left. Nothing is written until the codec shows the stream, and each
  of these writes is refused unless the codec still shows playback (`$0F18` bit 0 set and
  `$0F1B = 0x00`), so a stream that stops partway ends the change before the next write.
  Balanced mode turns the references off at idle and the ROM never turns them back on, so
  turning the hook off while they are off is refused (a replug restores them).
- The stream is a WASAPI exclusive-mode stream when no program has an audio session on the
  dongle's output, because closing it releases the output at once. Otherwise, or when exclusive
  mode is not available, it is a shared-mode stream: taking the output exclusively would stop
  the other programs' sessions for good.
- The DSP writes (`$1151` bits 2, 4 and 7, the sidetone gain `$1276/7`, and the NEWC request
  in `$1154`) are guarded like the rest: they need the chip-mode and DSP-bypass flags clear,
  accept only the bit subsets and range listed in the firmware notes, and read the byte again
  right before writing. A NEWC request that the ROM already has pending is waited for instead
  of repeated. After a NEWC request the ROM must clear it; if it leaves other bits of `$1154`
  different from before, or its stream bits do not match the capture and playback streams
  (`$13DC`, `$13DD`), that is reported as a stream event (a warning), because it means a stream
  opened or closed at that moment. `$1152`, `$0F4D` and `$040A` are never written.
- A sidetone level change while sidetone is on goes through four guarded writes of equal
  ratio, and the level stays in `0x0001..0x1000`.

## Known limitations

- When the silent stream has to be a shared-mode stream (another program has a session on the
  output, or exclusive mode is off for the device), Windows sometimes keeps the dongle's output
  active after it closes. While it does, the codec still sees playback (`status` shows
  `$0F18 = 0x03`, `$0F1A = 0x00`) and the idle hiss reduction has not started yet. Closing an
  exclusive-mode stream releases it. See the audio engine section of
  [docs/firmware-notes.md](docs/firmware-notes.md).
- Patch RAM that does not match a known mode (for example from another tool) is refused and
  nothing is written; replugging the dongle restores the vendor patch.

## Keeping the settings applied (`watch` and the engine)

`watch` runs the background engine (`cxtune::engine`, described in
[docs/engine.md](docs/engine.md)) without a user interface and prints what it does. The
engine is also what the tray application will use. It owns the dongle: one worker thread does
all HID and audio-stream access, and it holds the device lock for as long as it runs.

- **Arrival.** HID enumeration is polled once per second (no device I/O). A dongle that
  appears is left alone for 2 s so that its firmware can load the patch, then read and, if it
  differs from the saved settings, brought to them: the microphone DSP settings first, then the
  mode, then the mic boost. The DSP settings come first so that a reload request is finished
  before cxtune opens its own playback stream for the mode change; a DSP setting that is
  refused does not keep the mode from being applied. A dongle that disappears again within the
  2 s is never touched, so a flapping cable causes no writes and no errors.
- **Drift.** A connected dongle is read every 15 s and compared with the saved settings; only
  a difference leads to writes. A dongle that shows the vendor's settings again has restarted
  between two polls (a USB reset): it is handled like an arrival, with the 2 s wait, however
  often that happens. Any other difference, such as another tool changing the patch RAM, is
  corrected and reported as a warning. Because a correction of a microphone DSP setting reloads
  the codec's DSP, a setting that something keeps changing is corrected three times within ten
  minutes and then left alone, with an error that says so, until a setting is changed,
  re-applied or the dongle is restarted or replugged.
- **Sleep.** The computer waking from sleep or hibernation is noticed as a gap of more than
  5 s in the engine's wall-clock rhythm. The dongle may have restarted, so it is left alone for
  2 s and then verified.
- **Failures.** A dongle that is connected but does not answer is retried with growing pauses
  (2 s up to 30 s); a failed silent stream is retried (5 s up to 60 s); a refusal by a safety
  check is retried three times (5 s, 15 s, 60 s) because a patch that is still loading looks
  unrecognised; a write that does not stick twice (10 s, 60 s). A failure that persists is
  reported once. After the last retry the engine waits for a new command, a re-apply or a
  replug. A dongle that disappears while it is being used is a disconnect, not an error.
- **Commands.** Setting commands change the saved settings at once and the dongle at most
  150 ms later, with the latest values: a slider moved for a second causes a handful of
  writes, not one per step. The settings file is rewritten once per burst. If it cannot be
  written (another program holds it open), the state says the settings are not saved, the write
  is tried again with growing pauses and once more when the engine stops.
- **Failure of the engine.** A request to the dongle that never returns (hidapi has no time
  limit and cannot cancel it) is reported after 10 s as an error that asks for a replug, and
  stopping the engine does not wait for it. A panic ends the engine with a last error state;
  the user interface can ask whether the engine is still running.

Applying is idempotent: if the device already matches, nothing is written and no stream is
opened. A stream event near a DSP reload request is printed as a warning and does not stop
anything.

Polling and a wall-clock gap were chosen over Windows notifications
(`RegisterDeviceNotification` / `CM_Register_Notification` for the dongle,
`PowerRegisterSuspendResumeNotification` / `WM_POWERBROADCAST` for sleep): they need no window,
message pump or callback threading, cannot miss an event through a registration race, and
cannot be lost when a notification is dropped; enumerating HID devices once a second and
comparing two clocks cost next to nothing. A missed or late detection costs at most one
verification interval, because the periodic read finds the difference anyway.

The engine writes a diagnostic log to `%LOCALAPPDATA%\cxtune\cxtune.log` (arrivals, passes,
writes made, warnings, errors; no settings beyond the saved ones). At 256 KiB it is renamed to
`cxtune.log.1`, replacing the older one. If another program holds either file open so that the
rename is refused, the current file is emptied instead, so the two files stay within 512 KiB.

## Building

Requires the Rust MSVC toolchain (the `hidapi` crate compiles the C hidapi library, so the
Visual Studio C++ build tools must be installed).

```
cargo build --release -j 10
cargo test -- --test-threads=10
cargo clippy --all-targets -- -D warnings
```

The binary is `target\release\cxtune.exe`. Tests run against an in-memory fake of the device
and never touch real hardware. The fake also models which routine the hook runs, the playback
state and the analog references, and panics on any write that breaks the rules above, so a
planner or guard bug fails the test that triggers it.

With the dongle connected, `cargo run --release --example trace -- <open_s> <close_s> <total_s>`
opens and closes a silent stream (and says whether it is exclusive or shared) and prints every
change of the playback registers with a timestamp. It only reads.

## GUI recommendation

The next step is a small tray/settings window (mode picker, mic boost slider, status,
run-at-login). Options considered:

- **windows-rs (raw Win32 or WinUI via COM)**: smallest binary and truly native controls, but
  WinUI 3 from Rust is still not a supported, ergonomic path, and raw Win32 means hand-writing
  layout, DPI handling and dark mode for every control.
- **egui (eframe)**: fastest to build and very simple, but it is immediate-mode with its own
  look; it does not look like a Windows 11 app, redraws continuously unless tuned, and has no
  built-in tray support.
- **Tauri**: polished UI with web tech and good tray support, but pulls in WebView2 and a JS
  front end for what is a handful of controls.
- **Slint**: declarative UI, ships a native-looking Fluent (Windows 11) style, low memory, no
  web runtime, good DPI support; tray icons need a small extra crate (e.g. `tray-icon`).

**Recommendation: Slint with its Fluent style, plus `tray-icon` for the notification-area
icon.** It gives a Windows 11 look with modest code and dependencies, and the existing core
(`cxtune` library) plugs in unchanged: the user interface sends `engine::Command`s through a
`Handle` and shows the `Snapshot`s that the engine's event sink delivers.
Check Slint's license terms (GPLv3, royalty-free, or commercial) before publishing.
