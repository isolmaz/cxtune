# CX20773 firmware notes (HyperX Cloud II USB dongle)

The canonical reference for everything cxtune knows about the dongle. Only our own findings are
recorded here; no firmware, ROM or EEPROM dumps. Each statement is marked:

- **proven**: read back from, or observed on, a real device;
- **inferred**: derived from the ROM disassembly or from a recording, not confirmed by a direct
  experiment.

Device: USB `0951:16A4`, Conexant/Synaptics CX20773, ROM `CATP_V00.15.00.00` (proven: `$1000..$1008`
reads `01 15 00 00 00 03 0f e0 1f`, firmware bytes `15 00 00 00`, chip id `03`). The CPU is a
6502-derived 8-bit core with extra instructions (`BRSET`/`BRCLR`/`BBRn`/`BBSn`/`SETB`/`CLRB`/
`MOVI`/`PATCH`). All addresses below are in the 64 KiB CPU space.

## HID protocol (proven)

The memory-access protocol of fwupd's `synaptics-cxaudio` plugin, on the HID collection with
usage page `0x000C` (interface 3).

- OUT report `0x04`, 37 bytes: `[0x04, flags, length, addrHi, addrLo, data...]`, `length` 1..32.
- IN report `0x05`, 33 bytes: `[0x05, data...]`, fetched with GET_REPORT after the request.
- `flags`: `0x00` read CPU memory, `0x40` write CPU memory. cxtune sends nothing else. The other
  bits (`0x10` address at or above 64 KiB, `0x20` EEPROM, `0x80` DSP memory) are never used.
- A read needs about 5 ms between the request and GET_REPORT; a write needs about 50 ms before
  the readback. Reads are retried up to five times when the IN report is not `0x05`.
- Requests carry no tag. Two programs talking to the dongle at once can receive each other's
  responses, so only one process may use the interface.
- All patch RAM is reloaded from EEPROM by a replug or USB reset: every change is volatile.

## Registers and variables

| Address | Meaning | Status |
|---|---|---|
| `$0F18` | DAC channel enables, bit 0 left, bit 1 right. `0x03` while a playback stream runs, `0x00` at idle (unless the DAC-hold immediates force it on). | proven |
| `$0F1A` | Headphone driver power state. `0x00` fully active, `0x33` parked (ROM idle), `0x77` clamped. The values and their order of use below are observed. | proven |
| `$0F1B` | Second output driver. `0x00` while playing, `0x33` at idle. | proven |
| `$0F15`, `$0F16` | DAC left/right gain; bit 7 is the DAC mute. Set at idle, cleared about 36 ms after the driver reaches `0x00`. | proven |
| `$0F3E` | Analog power-up reference. Bit 1 is the bias reference: `0x02` while active, cleared by balanced mode at idle. | proven |
| `$0F40` bit 1, `$0F4A` bit 4, `$0F4E` bit 7 | Further steps of the analog power-up sequence (`$0F4E` bit 7 is step 0, `$0F4A` bit 4 and `$0F3E` bit 1 step 1, `$0F40` bit 1 step 2). Read as set while active. | inferred |
| `$0F2C` | Codec power-down bits. Seen as `0x40` (stock) and `0x60` (quiet idle). Shown by `status`, never written. | proven (values), inferred (name) |
| `$0F71` | Stream routing enables, bits 0/1 checked by the ROM driver update. | inferred |
| `$1017` | MIC_CONTROL. Bits 2:0 are the analog mic boost, 6 dB steps; vendor value `0x3C` (boost 4). cxtune writes `0x38..0x3F`. | proven |
| `$1013`, `$1014` | Volume-mute flags and output control, consulted by the ROM driver update (`$1014` bit 0 selects the 0x00/0x33 driver choice). | inferred |
| `$16CF`, `$17D0` | The `ORA #imm` immediates in the DAC-hold routines at `$16C9` and `$17CA`. `0x03` forces `$0F18` bits 0/1 on at every pass; `0x00` leaves them to the ROM. | proven |
| `$14B1..$14B3` | Hook 30: opcode and 16-bit operand of the entry in the hook table. `$14B4` is the next entry's `JMP`. | proven |
| `$1EFC..$1F26` | The 43-byte window: unreferenced patch code ending in the vendor hook-30 routine at `$1F1B`. Nothing else jumps into it (inferred from the cross-references). | proven (bytes), inferred (unreferenced) |
| `$1EF9` | `PLA PLA RTS`, the body of hook 29. Jumping here from hook 30 returns to the caller of `$D45E`, skipping the ROM driver update. | proven (bytes), inferred (effect) |
| `$9C`..`$9E`, `$C9`/`$CA`, `$D7`/`$D8` | Zero-page flags and timers. `$9D` bit 1 is "timer 1 (`$C9`/`$CA`) finished" and gates the DAC unmute; `$9E` bit 5 is "timer 8 (`$D7`/`$D8`) finished". Timers count 10 ms (600 ms = `0x3C`, 800 ms = `0x50` match what the ROM uses and what was measured). | inferred |
| `$A5` bit 0 | Capture (microphone) active. | inferred |

Not exposed and never written: `$1151`, `$0F4D`, the sidetone bytes, EEPROM, DSP memory.

## Hook table and the driver update

The vendor patch redirects ROM code through a table of 3-byte entries (`JMP target` or `RTS`);
entry `n` is at `$1457 + 3n`. **Hook 30 (`$14B1`)** is called by the ROM's driver update:

```
$D45E  JSR $14B1        ; hook 30
$D461  BBS5 $9D,...     ; ROM path: decide $0F1A / $0F1B from $0F18, $0F71, $1013, $1014
```

(inferred from the disassembly; confirmed by behaviour: with the hook at `RTS` the ROM parks the
driver at `0x33` at idle.)

- **Vendor hook 30** is `4C 1B 1F` (`JMP $1F1B`). The routine at `$1F1B` starts with a `PATCH`
  instruction (opcode `0x6B`, `PATCH #0, $D4BA -> $D4BF`), then `LDA #$00; STA $0F1A; RTS`. The
  driver is forced to `0x00` on every pass. The effect of the `PATCH` on the ROM's park store at
  `$D4BA` is inferred, not proven. (proven: stock holds `$0F1A = 0x00` at idle.)
- **RTS** (`60`) returns to `$D461` and lets the ROM decide. This is the ROM default idle
  power-down.
- **Our routines** are entered with `4C FC 1E`-style operands pointing at `$1EFC`. A routine
  ends with `RTS` (continue into the ROM path) or `BRA $1EF9` (the `PLA PLA RTS` gadget: skip the
  ROM path).
- **Hook 5** (`4C B5 16`) and **hook 7** (`4C CA 17`) lead to routines that contain the DAC-hold
  blocks at `$16C9` and `$17CA`: `LDA $0F18; AND #$DF; ORA #imm; STA $0F18`.

ROM sequences that the routines call or imitate (inferred from the disassembly):

- `$D671` is the second step of the analog power-up state machine: `SETB #$10,$0F4A`,
  `LDA #$33; STA $0F1A`, `SETB #$02,$0F3E`, timer 1 = `0x3C` (600 ms), then the machine continues
  on its own (`$0F40` bit 1, ...).
- `$D6D4` is the analog power-down: `$0F3E` bit 1 and `$0F40` bit 1 cleared, timer 8 = `0x50`
  (800 ms), then `$0F1A = 0x77`, `$0F4A` bit 4 and `$0F4E` bit 7 cleared.
- `$D4A1` is `$0F1B |= 0x33` (idle park of the second driver).

## Writable regions

cxtune writes only to: `$14B1..$14B3` (hook), `$16CF`, `$17D0` (DAC holds), `$1EFC..$1F26`
(window) and `$1017` (mic). Anything else is refused at the single write function.

## The window images

Layout of every image: 43 bytes, written as two reports: part 1 is `$1EFC..$1F1B` (32 bytes),
part 2 is `$1F1C..$1F26` (11 bytes).

The vendor image, in the window of stock and quiet mode:

```
2f bf 1b cd 85 12 f0 16 8d 85 12 8d ea 13 a9 01 8d 84 12 8d e9 13 a9 02
8d e8 13 20 1f ab 60 6b 00 ba d4 bf d4 a9 00 8d 1a 0f 60
```

### Balanced (R6)

```
f2 18 0f 01 1a 8f a5 17 e2 3e 0f 02 09 c2 02 3e 0f b2 50 d7 57 9e 5f 9e e6 d2 77 1a 0f 80 de
f2 3e 0f 02 03 20 71 d6 1f 9d d3 60
```

The byte sequences are proven; the annotations are inferred from the ROM disassembly.

```
1EFC  BRSET $0F18,#$01 -> ACT      ; playback running
1F01  BBS0  $A5        -> ACT      ; capture running
1F04  BRCLR $0F3E,#$02 -> DWELL    ; references already off
1F09  CLRB  #$02,$0F3E             ; references off (ROM power-down order, at the 0x33 park)
1F0D  MOVI  #$50,$D7               ; timer 8 = 800 ms
1F10  RMB5  $9E                    ; start timer 8
1F12  DWELL: BBR5 $9E  -> ROM path ; decaying: ROM keeps 0x33
1F15  SETB  #$77,$0F1A             ; decay finished: clamp
1F19  BRA   $1EF9                  ; skip the ROM path
1F1B  ACT:   BRSET $0F3E,#$02 -> WAIT
1F20  JSR   $D671                  ; references off: 0x33, references on, timer 1 = 600 ms
1F23  WAIT:  BBR1 $9D -> $1EF9     ; settling: hold the driver, skip the ROM path
1F26  RTS                          ; settled: the ROM path writes 0x00 (playback) or keeps 0x33
```

- Hook `4C FC 1E`, DAC holds `0x00`.
- Idle: `$0F1A = 0x77`, references (`$0F3E` bit 1) off, `$0F18 = 0x00`.
- Start (playback or capture) from idle: `0x33` and references on at once, `0x00` after 600 ms
  (playback), DAC unmute 36 ms later. Stop: references off, 800 ms decay, then `0x77`.
- Cost: the first 600 ms of audio after idle are muted; opening the microphone from full idle
  gives 1.26 s of silence and a click (recorded by the previous research).

### Minimum hiss (v2)

```
e2 18 0f 01 1c e2 1a 0f 01 f5 b2 0a c9 17 9d e2 1a 0f 04 09 c2 44 1a 0f b2 3c d7 57 9e 5f 9e
dd 60 d2 77 1a 0f d2 33 1b 0f 80 d2
```

The byte sequences are proven; the annotations are inferred from the ROM disassembly.

```
1EFC  BRCLR $0F18,#$01 -> IDLE     ; no playback
1F01  BRCLR $0F1A,#$01 -> ROM path ; driver already 0x00: active
1F06  MOVI  #$0A,$C9               ; T2: timer 1 = 100 ms, the DAC unmute gate
1F09  RMB1  $9D                    ;   (start it)
1F0B  BRCLR $0F1A,#$04 -> WAIT     ; bit 2 clear: already 0x33
1F10  CLRB  #$44,$0F1A             ; 0x77 -> 0x33
1F14  MOVI  #$3C,$D7               ; T1: timer 8 = 600 ms
1F17  RMB5  $9E                    ;   (start it)
1F19  WAIT: BBR5 $9E -> $1EF9      ; settling: hold 0x33, skip the ROM path
1F1C  RTS                          ; settled: the ROM path writes 0x00
1F1D  IDLE: SETB #$77,$0F1A
1F21  SETB  #$33,$0F1B             ; what ROM $D4A1 does at idle
1F25  BRA   $1EF9
```

- Hook `4C FC 1E`, DAC holds `0x00`.
- **T1**, the settle time, is the immediate at `$1F15`: units of 10 ms, default `0x3C`
  (600 ms), allowed `0x02..0xC8` (20..2000 ms). cxtune's `tune` writes it in place while the
  hook is live. **T2** at `$1F07` (default `0x0A`) is not changed.
- A multi-bit mask in `BRSET`/`BRCLR` does not test "all bits" as one would expect on this core;
  every test above uses a single-bit mask (proven by a failed earlier version, exact semantics
  not determined).
- Idle: `$0F1A = 0x77`, `$0F1B = 0x33`, references untouched. Start: `0x77 -> 0x33`, `0x00` after T1.
- Cost: loud start pop (recorded); audio is muted for T1 plus T2 (735 ms measured with the
  defaults).

### Quiet

Hook `60 1B 1F 4C`, DAC holds `0x00`, vendor window. The ROM parks the driver at `0x33` when
idle (proven), `$0F1B = 0x33`. Low pop (recorded), some idle hiss.

### Stock

Hook `4C 1B 1F 4C`, DAC holds `0x03`, vendor window. Driver `0x00`, `$0F18 = 0x03` and
references on at all times (proven). Constant hiss, no start pop.

## Measured on the device

Method: the playback registers are polled over HID (one cycle of four reads takes about 36 ms,
which is the time resolution below) while a silent stream is opened and closed on the dongle.
The stream is a WDM-KS stream (so that it really ends at the close, see the audio engine
section), kept open for 5 s from an idle start. Times are relative to the first change.
`examples/trace.rs` is the poller. Columns are `$0F18 / $0F1A / $0F1B / $0F3E`, "mute" is
`$0F15`/`$0F16` bit 7.

| Mode | Idle | Start | Stop (5.1-5.2 s after the start) |
|---|---|---|---|
| stock | `03 / 00 / 33 / 02`, not muted | `$0F1B` 33 -> 00 only | `$0F1B` back to 33 |
| quiet | `00 / 33 / 33 / 02`, muted | `$0F1A` 00 and `$0F18` 03 together, DAC unmuted in the next cycle | `$0F18` 00, `$0F1A` 33, muted |
| balanced | `00 / 77 / 33 / 00`, muted | `$0F18` 03, `$0F1A` 33, `$0F3E` 02 together; `$0F1A` 00 and unmute at +626 ms | `$0F1A` 33, `$0F3E` 00, `$0F18` 00, muted; `$0F1A` 77 at +811 ms |
| min-hiss | `00 / 77 / 33 / 02`, muted | `$0F18` 03, `$0F1A` 33 together; `$0F1A` 00 at +627 ms; unmute at +735 ms | `$0F1A` 77, `$0F18` 00, muted |

- Min-hiss with the settle time set to `0x1E` (300 ms): `$0F1A` 33 -> 00 after 295 ms. The
  defaults (`0x3C`, 600 ms) measured 627 ms; T2 (`0x0A`, 100 ms) measured 108 ms.
- Balanced stop: the 800 ms timer of the power-down measured 811 ms.
- The mode tuples, the raw bytes (`$14B1..$14B4`, `$16C9`, `$17CA`, the 43-byte window,
  `$1017`) and the registers above were read back after installing each mode.
- Start and stop transients in the microphone recording (previous research, 20 ms peak of the
  high-passed signal against a median of -68 dBFS): the ROM default (quiet) shows nothing more
  than 12 dB above the noise floor; the clamp routines (min-hiss and the clamp-only routine
  below) both show a transient of about -14 dBFS at the start and again at the stop.

## Not offered: the clamp-only routine

An earlier 23-byte routine only clamped the driver at idle:

```
ad 18 0f 29 03 d0 0f 68 68 a9 77 8d 1a 0f ad 1b 0f 09 33 8d 1b 0f 60
```

(`LDA $0F18; AND #$03; BNE` to the final `RTS` while playing; otherwise `PLA PLA`,
`$0F1A = 0x77`, `$0F1B |= 0x33`, `RTS`.) It leaves the same idle state as min-hiss
(`$0F1A = 0x77`, `$0F1B = 0x33`, references untouched) and its recording shows the same
transients at the start and at the stop (-14.4 / -14.2 dBFS against -14.1 / -14.2 dBFS for
min-hiss). Min-hiss adds the explicit start sequence and the tunable settle time, so the
clamp-only routine is not offered as a mode. Its muted time at the start was not measured on
the device, so this is a judgment and not a proven dominance. A device that is found with this
routine installed is refused as unrecognised (nothing is written); a replug clears it.

## Changing between modes

A mode is the tuple (DAC holds, hook opcode, hook operands, window part 1, window part 2).
`mode::plan` moves any recognised tuple, including the half-changed ones an interrupted change
leaves behind, to any mode:

1. A minimum-hiss settle time other than the default is first put back to `0x3C` (in place).
2. If the hook is live and anything it runs will change, it is turned to RTS first.
3. Window parts and operands change only while the hook is RTS. Installing a routine writes
   its parts, then the operands. Removing one writes the operands, then the parts.
4. The hook is turned to JMP last, and only if the window holds one whole routine and the
   operands point at it.
5. DAC holds are released before the hook changes and held again after it.

Unrecognised bytes anywhere (hook, window, DAC routines) are refused before the first write.

## The Windows audio engine and the output pin

cxtune opens its silent stream in WASAPI shared mode. After the last shared-mode client closes,
Windows often keeps the endpoint's USB alternate setting active, so the codec keeps seeing a
playback stream (`$0F18 = 0x03`, `$0F1A = 0x00`) and the idle hiss reduction does not take
effect. Observed on this machine (Windows 11 build 26200):

- It happens in about half of the trials, and a held pin was unchanged after more than
  7 minutes. A stream opened from Python (PortAudio WASAPI shared mode) behaves the same, so
  it is not specific to cxtune's stream.
- Closing a WDM-KS stream releases the pin every time; an exclusive-mode stream mostly does.
- A second short shared-mode stream released a held pin in 1 of 4 trials, so it is not a
  reliable remedy.

Writes do not depend on the pin: the planner runs from either state. `status` shows
`$0F18`/`$0F1A`, which tells a held pin from a real idle.

## Hazards

- **Hook off while the references are off.** Balanced mode turns the references (`$0F3E` bit 1)
  off at idle. The ROM never turns them back on, so switching the hook to RTS (or leaving a
  non-balanced routine in place) at that moment leaves them off until a replug. The hook is
  therefore never turned off unless `$0F3E` bit 1 is set (checked in the write guard), and
  every change that needs the hook off runs with a stream open, which makes balanced mode
  raise the references first (the plan waits for `$0F18` bit 0, `$0F1A = 0x00` and `$0F3E`
  bit 1 before the first write).
- **A driver transition while the hook is off.** With the hook at RTS at idle the ROM parks the
  driver at `0x33`, and the next routine then clamps it at `0x77`: two bias steps. With a
  silent stream open the driver is at `0x00` throughout, so the swap causes none.
- **A live hook with half a routine.** Any inconsistent window under a live hook runs garbage
  code. The hook is only turned on when both window parts and the operands match one routine.
- **One program at a time** (see the protocol above).
- A replug restores the vendor patch, so saved settings have to be applied again (`watch`).
