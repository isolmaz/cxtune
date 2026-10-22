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
few bytes of patch RAM:

| Profile | Effect |
|---|---|
| `stock` | Vendor behaviour: DACs held on (`$16CF`/`$17D0` = `0x03`), hook `$14B1` = `JMP`. |
| `idle` | ROM default idle power-down: DAC hold off (`0x00`), hook `$14B1` = `RTS`. The headphone driver parks at `0x33` when idle. |
| `clamp` | `idle` plus a 23-byte routine at `$1EFC` that clamps the driver at `0x77` when both DACs are idle. Experimental: louder pop when a stream starts. |

`mic-boost <0-7>` sets `$1017` MIC_CONTROL to `0x38 + n` (analog boost in 6 dB steps; the
vendor value is 4, best measured SNR was 6).

Because RAM is reset on every replug, `watch` re-applies the remembered settings whenever the
dongle appears.

## Usage

```
cxtune status               read and show the device state (read-only)
cxtune apply <profile>      apply stock | idle | clamp and remember it
cxtune mic-boost <0-7>      set mic boost and remember it
cxtune watch                keep the remembered settings applied across replugs
```

Settings are stored in `%APPDATA%\cxtune\config.toml`:

```toml
profile = "idle"
mic_boost = 6
```

Do not run cxtune at the same time as another tool that reads codec memory over HID (for
example a second cxtune or the vendor software poking the same interface): the read protocol
has no request tag, so concurrent readers can receive each other's responses. The safety checks
make this fail closed (a refusal), not write wrong data.

## Safety design

- Every write is an entry in a static allow-list (`src/patches.rs`): address, accepted
  before-bytes, new bytes, and guard bytes elsewhere in memory that must match.
- `patches::apply_patch` is the only function that sends a write. It reads the target and the
  guards, refuses on any mismatch, writes, reads back, and fails loudly on a readback mismatch.
  It also refuses any address outside the known writable patch-RAM regions.
- A write report can only be built from an allow-listed patch, and only with flag `0x40`
  (CPU memory). The EEPROM (`0x20`), DSP (`0x80`) and high-address (`0x10`) flags are never used.
- Profile changes are planned in full before the first write; an unrecognised state is refused
  without writing anything. The clamp routine is installed body first (while the hook is
  `RTS`), then the hook operands, then the opcode, and removed in reverse order.
- `$1151`, `$0F4D` and the sidetone bytes are not exposed.

## Re-applying on replug

`watch` polls HID enumeration once per second (no device I/O while polling). When the dongle
appears it waits 2 s for the firmware to load its patch, then applies the saved settings. While
connected it re-verifies every 60 s, which also catches a USB reset that happened between two
polls. Applying is idempotent: if the device already matches, nothing is written. A readback
mismatch stops `watch`.

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
and never touch real hardware.

## GUI recommendation

The next step is a small tray/settings window (profile picker, mic boost slider, status,
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
