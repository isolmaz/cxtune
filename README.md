# cxtune

A small Windows tool that removes the constant idle hiss of the **HyperX Cloud II USB sound
card dongle** (USB `0951:16A4`, Conexant/Synaptics CX20773 codec, ROM `CATP_V00.15.00.00`) on
sensitive IEMs, and sets its analog microphone boost.

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
| `min-hiss` | Driver clamped at `0x77`. | Loud pop. | Audio is muted for the settle time (default 600 ms); `tune` changes it. |

The exact bytes, registers and measurements behind each mode are in
[docs/firmware-notes.md](docs/firmware-notes.md).

`mic-boost <0-7>` sets `$1017` MIC_CONTROL to `0x38 + n` (analog boost in 6 dB steps; the
vendor value is 4, best measured SNR was 6).

Because RAM is reset on every replug, `watch` re-applies the remembered settings whenever the
dongle appears.

## Usage

```
cxtune status               read and show the device state and detected mode (read-only)
cxtune apply <mode>         apply stock | quiet | balanced | min-hiss and remember it
cxtune tune <ms>            set the min-hiss settle time (20-2000 ms in steps of 10)
cxtune mic-boost <0-7>      set mic boost and remember it
cxtune watch                keep the remembered settings applied across replugs
```

Settings are stored in `%APPDATA%\cxtune\config.toml`:

```toml
mode = "balanced"
mic_boost = 6
min_hiss_ms = 600
```

Do not run cxtune at the same time as another tool that reads codec memory over HID (for
example a second cxtune or the vendor software poking the same interface): the read protocol
has no request tag, so concurrent readers can receive each other's responses. The safety checks
make this fail closed (a refusal), not write wrong data.

## Safety design

- Every write is built in `src/patches.rs` from a closed set of known firmware images and
  constants: address, accepted before-bytes, new bytes, and guard bytes or bits elsewhere in
  memory that must match. Patches have private fields, so no other code can make one.
- `patches::apply_patch` is the only function that sends a write. It reads the target and the
  guards, refuses on any mismatch, writes, reads back, and fails loudly on a readback mismatch.
  It also refuses any address outside the known writable patch-RAM regions.
- A write report can only be built from a patch, and only with flag `0x40` (CPU memory). The
  EEPROM (`0x20`), DSP (`0x80`) and high-address (`0x10`) flags are never used.
- Mode changes are planned in full before the first write, from any recognised state: the four
  modes and every half-changed state an interrupted change can leave. An unrecognised state is
  refused without writing anything.
- The hook at `$14B1` is turned to RTS before the routine or its operands change and to JMP
  last, and it is only turned on when the whole routine is in place.
- Changes to the hook or its routine are made while a playback stream of digital silence is
  open on the dongle (WASAPI shared mode, `src/audio.rs`). The stream holds the headphone driver
  fully on, so the moment the hook is off causes no driver transition, and it brings the analog
  references up before balanced mode is left. Balanced mode turns them off at idle and the ROM
  never turns them back on, so turning the hook off while they are off is refused (a replug
  restores them).
- `$1151`, `$0F4D` and the sidetone bytes are not exposed.

## Known limitations

- Windows sometimes keeps the dongle's output stream active after the last shared-mode stream
  closes, including the silent one cxtune opens for a mode change. While it does, the codec
  still sees playback (`status` shows `$0F18 = 0x03`, `$0F1A = 0x00`) and the idle hiss
  reduction has not started yet. Closing a WDM-KS stream releases it. See the audio engine
  section of [docs/firmware-notes.md](docs/firmware-notes.md).
- Patch RAM that does not match a known mode (for example from another tool) is refused and
  nothing is written; replugging the dongle restores the vendor patch.

## Re-applying on replug

`watch` polls HID enumeration once per second (no device I/O while polling). When the dongle
appears it waits 2 s for the firmware to load its patch, then applies the saved settings. While
connected it re-verifies every 60 s, which also catches a USB reset that happened between two
polls. Applying is idempotent: if the device already matches, nothing is written and no stream
is opened. A readback mismatch stops `watch`.

Polling was chosen over Windows device notifications (`CM_Register_Notification` /
`WM_DEVICECHANGE`): it needs no window or callback threading, cannot miss an event because of a
registration race, and the cost of enumerating HID devices once a second is negligible.

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
opens and closes a silent stream and prints every change of the playback registers with a
timestamp. It only reads.

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
(`cxtune` library) plugs in unchanged; the `watch` loop would move to a background thread.
Check Slint's license terms (GPLv3, royalty-free, or commercial) before publishing.
