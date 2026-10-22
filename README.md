# cxtune

**Silence the hiss of the HyperX Cloud II USB sound card. No firmware flashing, nothing permanent.**

Unofficial, use at your own risk. Changes are RAM-only and a replug undoes them; see
[Disclaimer](#disclaimer).

## The problem

The USB sound card of the HyperX Cloud II keeps its headphone driver powered at all times. With
sensitive in-ear monitors (IEMs) that is a constant hiss, even when nothing is playing. Turning the
driver down trades the hiss for pops when audio starts and stops. The vendor mic boost is low
as well: speech sits around -57 dBFS in our test.

## What cxtune does

cxtune is a small Windows 11 program (Rust) for the dongle's CX20773 codec: a command-line tool
and a tray application.

- **Output modes** with an explicit trade-off between idle hiss and the noise when a stream
  starts (`stock`, `quiet`, `balanced`, `min-hiss`; details below).
- **Mic boost** in eight analog steps (boost 6 gave 12.6 dB speech-to-noise against 8 dB at the
  vendor setting in a single measurement; see `docs/firmware-notes.md`).
- **Mic noise reduction** switch. Measured on the device: stationary noise 11.9 dB lower with it
  on. Speech quality was not measured.
- **Mic AGC and sidetone** (experimental): the firmware applies them, but their effect is not
  fully measured.
- **Automatic re-apply** (`cxtune watch`): the settings are applied again when the dongle is
  replugged or the computer wakes from sleep.
- **Tray application** (English and Turkish): a window and a notification-area icon for all of
  the above and for the Windows volume of the dongle; see
  [The tray application](#the-tray-application).

| Mode | Idle | Stream start | Cost |
|---|---|---|---|
| `stock` | Vendor behaviour: driver always on. Constant hiss. | No pops. | None. |
| `quiet` | ROM default idle power-down. Some hiss. | Low pop. | None. |
| `balanced` | Driver clamped, analog references off. | Follows the ROM power-up order. | The first 600 ms of audio after idle are muted; opening the microphone from full idle gives 1.26 s of silence and a click. |
| `min-hiss` | Driver clamped. | Loud pop. | Audio is muted for the settle time (default 600 ms) plus 100 ms. |

## How it works

The dongle's firmware patch lives in RAM and is loaded from the EEPROM at every plug-in. cxtune
reads and changes a few bytes of that patch RAM over HID, using the memory-access protocol of
fwupd's `synaptics-cxaudio` plugin. Nothing is written to the EEPROM or to DSP memory, so every
change is volatile: unplug the dongle and it is stock again.

## Disclaimer

cxtune is an independent project. It is not affiliated with, endorsed by or supported by
HyperX, HP or Synaptics (Conexant). Use it at your own risk; the software is provided without
warranty and without liability, as stated in the [license](LICENSE). The changes it makes are
temporary (RAM only), and replugging the dongle restores the vendor behaviour.

## Quick start

Requires Windows 11 and the Rust MSVC toolchain, version 1.92 or newer (1.88 for the
command-line tool alone, `cargo build --release -p cxtune`; the Visual Studio C++ build tools
are needed for the bundled hidapi).

```
cargo build --release
target\release\cxtune.exe status
target\release\cxtune.exe apply balanced
target\release\cxtune.exe mic-boost 6
target\release\cxtune.exe watch
```

`status` and `dump` only read. Every other command except `watch` changes the dongle and remembers the
choice in `%APPDATA%\cxtune\config.toml`. `watch` keeps the remembered settings applied until you stop
it.

```
cxtune status                   read and show the device state and detected mode
cxtune apply <mode>             apply stock | quiet | balanced | min-hiss and remember it
cxtune tune <ms>                set the min-hiss settle time (20-2000 ms in steps of 10)
cxtune mic-boost <0-7>          set mic boost (vendor value 4; 6 had the best SNR in our measurement)
cxtune mic-nr <on|off>          set microphone noise reduction
cxtune mic-agc <on|off>         set microphone AGC (experimental)
cxtune sidetone <on|off>        set sidetone (experimental)
cxtune sidetone-level <1-4096>  set the sidetone gain register (experimental; start low)
cxtune dump <out-dir>           read the ROM and EEPROM (read-only) into rom.bin and eeprom.bin
                                and compare their SHA-256 with the reference unit's
cxtune watch                    keep the remembered settings applied across replugs and
                                wake-ups (holds the dongle; other commands are refused)
```

## The tray application

`target\release\cxtune-gui.exe` is a small window and notification-area icon on top of the same
engine (Windows 11, Turkish and English). It follows the light or dark scheme of Windows, has no
console window and closes to the notification area.

- **Output**: the four modes with Hiss, Pop and Delay indicators that show only what was measured
  (and say so when something was not), the start delay of minimum hiss, the headphone volume and
  **Apply recommended** (balanced mode, microphone boost 6, noise reduction on, AGC and sidetone
  off).
- **Microphone**: boost with the gain compared with the vendor's setting, noise reduction, AGC
  (experimental) and the microphone volume.
- **Sidetone** (experimental, with a note not to turn on Windows' "Listen to this device" as
  well) and **General**: start with Windows, language, the log folder.
- **About**: version, the supported device and the disclaimer.
- The **notification-area icon** switches the mode, re-applies the settings and shows the
  window; its tooltip is the short status. `cxtune-gui.exe --minimized` starts it in the
  notification area, and only one copy runs at a time.
- The **status** is one line for what is wrong and one for what to do; technical details are only
  in the log (`%LOCALAPPDATA%\cxtune\cxtune.log`).
- The volume sliders are the Windows volume of the dongle's playback and capture endpoints
  (the same as the Windows mixer and the buttons on the dongle); no HID report is involved.

How it works, the status rules and the interface contract are in [docs/gui.md](docs/gui.md).
The window is built with [Slint](https://slint.dev) under its Royalty-free license.

[![Made with Slint](https://raw.githubusercontent.com/slint-ui/slint/master/logo/MadeWithSlint-logo-whitebg.png)](https://slint.dev)

## Supported device

| | |
|---|---|
| USB ID | `0951:16A4` (HyperX Cloud II USB sound card) |
| Codec | Conexant/Synaptics CX20773 |
| ROM | `CATP_V00.15.00.00` |
| System | Windows 11 |

Any other device, or a ROM or patch RAM that does not match, is refused before anything is
written.

## Details

- [docs/firmware-notes.md](docs/firmware-notes.md): protocol, registers, patch images,
  measurements and hazards.
- [docs/engine.md](docs/engine.md): the background engine behind `watch`.
- [docs/gui.md](docs/gui.md): the tray application.
- [docs/firmware-research.md](docs/firmware-research.md): how the CX20773 firmware works, what
  is still unknown and how to help (contributions welcome).
- [tools/README.md](tools/README.md): `cxtune dump` and the disassembler and Ghidra tools for
  researching your own unit's firmware.

### Configuration

`%APPDATA%\cxtune\config.toml`. Keys that are absent leave that setting as the device has it.
The file is written to a temporary file and renamed over the old one. A file with unrecognised
keys is rejected with an error that names the file, before anything is written to the device.

```toml
mode = "balanced"
mic_boost = 6
min_hiss_ms = 600
mic_nr = true
mic_agc = false
sidetone = false
sidetone_level = 947
```

### Safety design

- Every write is built in `src/patches.rs` from a closed set of known firmware images and
  constants: address, accepted before-bytes, new bytes, and guard bytes or bits elsewhere in
  memory that must match. Patches have private fields, so no other code can make one.
- `patches::apply_patch` is the only function that sends a write. It reads the target and the
  guards, refuses on any mismatch, writes, reads back, and fails on a readback mismatch. It
  refuses any address outside the known writable patch-RAM regions and any device whose
  firmware version and chip id are not the ones the routines were written for.
- Only the flags `0x00` (read CPU memory) and `0x40` (write CPU memory) are sent, plus `0x20`
  alone (an EEPROM read) by `cxtune dump`. EEPROM writes (`0x20|0x40`), DSP (`0x80`) and
  high-address (`0x10`) flags are never used.
- A mode change is planned in full before the first write, from any recognised state
  (including every half-changed state an interrupted change can leave). An unrecognised state
  is refused without writing anything.
- The hook at `$14B1` is turned to RTS before the routine or its operands change and to JMP
  last.
- Routine changes are made while a playback stream of digital silence is open on the dongle,
  so the headphone driver does not switch at the moment the hook is off.
- Only one cxtune process controls the dongle at a time (machine-wide mutex
  `Global\cxtune-device`), because the read protocol has no request tag. The lock does not keep
  other programs that read codec memory over HID away.

### Limitations and unverified items

- When another program has an audio session on the dongle's output, the silent stream is a
  shared-mode stream, and Windows sometimes keeps the output active after it closes. Until it
  releases, the codec still sees playback and the idle hiss reduction has not started.
- Patch RAM that does not match a known mode (for example from another tool) is refused;
  replugging the dongle restores the vendor patch.
- Mic AGC and sidetone are experimental; their effect on speech and loudness was not measured.
  Sidetone can be loud in IEMs.
- Noise reduction was measured on stationary noise only.
- A click in the headphones when a noise reduction or AGC change reloads the running stream
  while audio plays has not been measured.
- Re-apply after a physical replug or a real sleep and resume has only been tested with
  simulated restarts and absences, not on hardware.
- The mic boost comparison is a single take per value, with mains hum dominating the noise.
- Tested with one dongle and ROM version.

### Building from source

The minimum supported Rust version is 1.88 for the command-line tool and 1.92 for the tray
application, which Slint requires (`rust-version` in the `Cargo.toml` of each).

```
cargo build --release
cargo test
cargo clippy --all-targets -- -D warnings
```

The binaries are `target\release\cxtune.exe` and `target\release\cxtune-gui.exe`. Tests run
against an in-memory fake of the dongle and never touch real hardware. With the dongle connected,
`cargo run --release --example trace -- <open_s> <close_s> <total_s>` opens and closes a silent
stream and prints every change of the playback registers with a timestamp; it only reads and,
like the commands, refuses to run while another cxtune process controls the dongle.
`cargo run --example volume -- get` reads the volume and mute state of the dongle's Windows
audio endpoints (`set` and `mute` change them).

### License

[MIT](LICENSE), except `tools/ghidra/` (Apache-2.0, derived from Ghidra; see
[LICENSE](tools/ghidra/CX65C02/LICENSE) and [NOTICE](tools/ghidra/CX65C02/NOTICE)).
