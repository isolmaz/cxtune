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

Mode names: the modes are *default* (the vendor patch), *low click*, *balanced* and *lowest
noise* (`default`, `low-click`, `balanced`, `lowest-noise` on the command line and in the settings
file). The CLI and the settings file also accept the former names `stock`, `quiet`, `click-free`
and `min-hiss`, and the settings file the former key `min_hiss_ms` for `soft_start_ms`.

## HID protocol (proven)

The memory-access protocol of fwupd's `synaptics-cxaudio` plugin, on the HID collection with
usage page `0x000C` (interface 3).

- OUT report `0x04`, 37 bytes: `[0x04, flags, length, addrHi, addrLo, data...]`, `length` 1..32.
- IN report `0x05`, 33 bytes: `[0x05, data...]`, fetched with GET_REPORT after the request.
- `flags`: `0x00` read CPU memory, `0x40` write CPU memory. The only other value cxtune sends is
  `0x20` alone, an EEPROM read, from `cxtune dump`. `0x20|0x40` (an EEPROM write), `0x10` (address
  at or above 64 KiB) and `0x80` (DSP memory) are never used.
- A read needs about 5 ms between the request and GET_REPORT; a write needs about 50 ms before
  the readback. A read makes up to 5 attempts (4 retries) until the IN report is `0x05`.
  `cxtune dump` also reads every chunk twice and makes up to 5 attempts (4 retries) at the chunk
  until the two reads agree (`READ_ATTEMPTS` in `src/protocol.rs`).
- Requests carry no tag. Two programs talking to the dongle at once can receive each other's
  responses, so only one process may use the interface. cxtune enforces this between its own
  processes with the named mutex `Global\cxtune-device` (`src/lock.rs`).
- All patch RAM is reloaded from EEPROM by a replug or USB reset: every change is volatile.

## Registers and variables

| Address | Meaning | Status |
|---|---|---|
| `$0F18` | DAC channel enables, bit 0 left, bit 1 right. `0x03` while a playback stream runs, `0x00` at idle (unless the DAC-hold immediates force it on). | proven |
| `$0F1A` | Headphone driver power state. `0x00` fully active, `0x33` parked (ROM idle), `0x77` clamped. The values and their order of use below are observed. | proven |
| `$0F1B` | Second output driver. `0x00` while a stream plays, `0x33` at idle, in every mode. In default mode it is the only playback register that differs between idle and playing (the vendor patch forces `$0F18` and `$0F1A`, and the references stay on). "Playback active" below means `$0F18` bit 0 set and `$0F1B == 0x00`. | proven |
| `$0F15`, `$0F16` | DAC left/right gain; bit 7 is the DAC mute. Set at idle, cleared about 36 ms after the driver reaches `0x00`. | proven |
| `$0F3E` | Analog power-up reference. Bit 1 is the bias reference: `0x02` while active, cleared by balanced mode at idle. | proven |
| `$0F40` bit 1, `$0F4A` bit 4, `$0F4E` bit 7 | Further steps of the analog power-up sequence (`$0F4E` bit 7 is step 0, `$0F4A` bit 4 and `$0F3E` bit 1 step 1, `$0F40` bit 1 step 2). Read as set while active. | inferred |
| `$0F2C` | Codec power-down bits. Seen as `0x40` (default mode) and `0x60` (low-click idle). Shown by `status`, never written. | proven (values), inferred (name) |
| `$0F71` | Stream routing enables, bits 0/1 checked by the ROM driver update. | inferred |
| `$1001..$1005` | Firmware version `15 00 00 00` and chip id `03`. | proven |
| `$1017` | MIC_CONTROL. Bits 2:0 are the analog mic boost, 6 dB steps; vendor value `0x3C` (boost 4). cxtune writes `0x38..0x3F`. | proven |
| `$1013`, `$1014` | Volume-mute flags and output control, consulted by the ROM driver update (`$1014` bit 0 selects the 0x00/0x33 driver choice). | inferred |
| `$16CF`, `$17D0` | The `ORA #imm` immediates in the DAC-hold routines at `$16C9` and `$17CA`. `0x03` forces `$0F18` bits 0/1 on at every pass; `0x00` leaves them to the ROM. | proven |
| `$14B1..$14B3` | Hook 30: opcode and 16-bit operand of the entry in the hook table. `$14B4` is the next entry's `JMP`. | proven |
| `$1EFC..$1F26` | The 43-byte window: unreferenced patch code ending in the vendor hook-30 routine at `$1F1B`. Nothing else jumps into it (inferred from the cross-references). | proven (bytes), inferred (unreferenced) |
| `$1EF9` | `PLA PLA RTS`, the body of hook 29. Jumping here from hook 30 returns to the caller of `$D45E`, skipping the ROM driver update. | proven (bytes), inferred (effect) |
| `$9C`..`$9E`, `$C9`/`$CA`, `$D7`/`$D8` | Zero-page flags and timers. `$9D` bit 1 is "timer 1 (`$C9`/`$CA`) finished" and gates the DAC unmute; `$9E` bit 5 is "timer 8 (`$D7`/`$D8`) finished". Timers count 10 ms (600 ms = `0x3C`, 800 ms = `0x50` match what the ROM uses and what was measured). | inferred |
| `$A5` bit 0 | Capture (microphone) active. | inferred |
| `$1000` bit 7 | Set by the vendor patch only for chip ids below 3; clear here (`0x01`). | proven (value), inferred (meaning) |
| `$00A6` bit 0 | DSP bypass. Clear at idle and while streams run (`0x00`, `0x02`). | proven (values), inferred (meaning) |
| `$1151` | DSP enable bits. Bit 2 mic noise reduction (stock 1), bit 4 mic AGC (stock 0), bit 7 sidetone (stock 0). cxtune changes only these three. | proven |
| `$1154` | Config-change request and stream state. Bit 0 is NEWC (the ROM clears it within a pass). Bit 1 follows the capture stream and bit 4 the playback stream (observed); the ROM owns the other bits. | proven (bits 0, 1, 4) |
| `$1276/$1277` | Sidetone gain, little endian, stock `0x03B3`. cxtune writes `0x0001..0x1000`. In UAC 1/256 dB units per the ROM: `0x03B3` = +3.70 dB, `0x1000` = +16 dB. | proven (copied to `$112A/B` and `$1255/6`); the unit is the ROM's, not exposed to the host (no feature unit 11 in the descriptor), DSP loudness not measured, nothing listened to (see "Sidetone gain unit") |
| `$00A4` | Applier flags: bit 7 noise reduction applied, bit 5 AGC applied (both only while a capture stream is open), bit 4 virtual surround applied (only while a playback stream is open and `$1152` bit 3 is set: seen set within 0.1 s of a silent stream opening and cleared when it closed). | proven |
| `$00A8` | Mixer flags: bit 5 capture, bit 4 playback, bit 3 sidetone path running. | proven |
| `$13DC` | Capture interface alternate setting: `0x01` while a capture stream is open. `$13DD` is the same for playback. | proven |
| `$1152` bit 3 | Virtual surround (7.1) on, stock off. cxtune changes this bit only. | proven (the bit, its read-back and its effect on `$00A4`); the sound was not listened to |
| `$10D3` bits 3:0 | Virtual surround level, stock `0x04`. The ROM takes 0 as 1 and anything above 10 as 10, so cxtune writes 1..10. Bit 4 (a variant) and bits 7:5 are left as they are. | proven (value and read-back), inferred (the clamping) |
| `$1F0E` | The balanced routine's decay time: the immediate of `MOVI #imm,$D7`, units of 10 ms, stock `0x50` (800 ms). cxtune writes `0x0A..0xC8` in place. | proven |
| `$1F15` | The lowest-noise routine's soft-start time: the immediate of `MOVI #imm,$D7`, units of 10 ms, stock `0x3C` (600 ms). cxtune writes `0x02..0xC8` in place. In the other routines the byte is code. | proven |
| `$1152` bits 1:0 | EQ preset index. Not exposed: the vendor patch clears them on every pass (see the DSP section). | proven |

Not exposed and never written: `$0F4D`, `$040A` (EEPROM commit request), every bit of `$1152`
except 3, `$10D3` except its low nibble, every bit of `$1151` except 2, 4 and 7, EEPROM, DSP
memory.

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
(window), `$1017` (mic), `$1151` and `$1152` (DSP enable bits), `$10D3` (surround level),
`$1154` (NEWC request) and `$1276/$1277` (sidetone gain). Anything else is refused at the single
write function.

Guards checked there, right before each write, besides the before-bytes of the patch:

- every write: the ROM identity at `$1001..$1005` (`15 00 00 00 03`);
- every write that changes the hook or its routine (they are made with a stream open):
  playback active (`$0F18` bit 0 and `$0F1B == 0x00`);
- hook to RTS: the operands match the installed routine, and `$0F3E` bit 1 is set;
- operands and window parts: the hook is RTS;
- hook to JMP: the operands and the whole routine are in place; for a routine in the window,
  the `PLA PLA RTS` gadget at `$1EF9` (both routines leave through it); for lowest noise, the
  soft-start immediate is in `0x02..0xC8`, for balanced the decay immediate is in `0x0A..0xC8`;
- the soft-start and decay immediates: every other byte of the window matches the routine they
  belong to, and the value read and the value written are in the range above;
- every DSP write (`$1151`, `$1152`, `$10D3`, `$1154`, `$1276/7`): `$1000` bit 7 clear and
  `$00A6` bit 0 clear;
- `$1151`: the byte holds no bit outside `0x04`, `0x10`, `0x80` when read, and only the one
  bit of the patch changes; the byte is read again after the guards, right before the write;
- `$1152`: the byte holds no bit outside `0x0B` (surround and the two preset bits that the
  vendor patch clears) when read, only bit 3 changes, and the byte is read again after the guards,
  right before the write;
- `$10D3`: bits 7:5 are clear when read, the low nibble becomes 1..10 and the rest of the byte is
  kept; the byte is read again after the guards;
- `$1276/7`: the value read and the value written are in `0x0001..0x1000` (nothing with bit 15
  set);
- `$1154`: if NEWC (bit 0) is clear when read, the write is that byte, read again after the
  guards, with bit 0 set. If it is set (a request of the ROM's own), nothing is written. In
  both cases the request is then waited for (see below).

## The window images

Layout of every image: 43 bytes, written as two reports: part 1 is `$1EFC..$1F1B` (32 bytes),
part 2 is `$1F1C..$1F26` (11 bytes).

The vendor image, in the window of default and low-click mode:

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
  (playback), DAC unmute 36 ms later. Stop: references off, decay time, then `0x77`.
- The **decay time** is the immediate `#$50` at `$1F0E` (10 ms units, 800 ms). cxtune writes
  `0x0A..0xC8` (100..2000 ms) there in place while the hook is live: the byte is data of the
  routine and the write changes no instruction (see "The exception: a one-byte immediate
  operand"). The measured times are under "Balanced stop decay time".
- The **start dwell** (600 ms, timer 1) is set by the ROM routine at `$D671` that the routine
  calls; it is outside every region cxtune writes and is not tunable.
- Cost: the first ≈650 ms of audio after idle are muted (the 600 ms start dwell, the DAC unmute
  about 37 ms later and the poll resolution; `mode::mute_delay_ms` uses 650 ms); opening the
  microphone from full idle gives digital silence and a click first (see "Microphone capture in
  each mode").

### Lowest noise (v2, former `min-hiss`)

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
- **T1**, the soft-start time, is the immediate at `$1F15`: units of 10 ms, default `0x3C`
  (600 ms), allowed `0x02..0xC8` (20..2000 ms). `cxtune soft-start` writes it in place while the
  hook is live (see "The exception: a one-byte immediate operand"). **T2** at `$1F07` (default `0x0A`) is not changed.
- A multi-bit mask in `BRSET`/`BRCLR` does not test "all bits" as one would expect on this core;
  every test above uses a single-bit mask (proven by a failed earlier version, exact semantics
  not determined).
- Idle: `$0F1A = 0x77`, `$0F1B = 0x33`, references untouched. Start: `0x77 -> 0x33`, `0x00` after T1.
- Cost: loud start click (recorded); audio is muted for T1 plus T2 (735 ms measured with the
  defaults).

### Low click (former `quiet`, `click-free`)

Hook `60 1B 1F 4C`, DAC holds `0x00`, vendor window. The ROM parks the driver at `0x33` when
idle (proven), `$0F1B = 0x33`. Low click (recorded), some idle noise.

### Default (former `stock`)

Hook `4C 1B 1F 4C`, DAC holds `0x03`, vendor window. Driver `0x00`, `$0F18 = 0x03` and
references on at all times (proven). Constant noise floor, no start click.

## Measured on the device

Method: with each mode installed by `cxtune apply` and the device idle, `examples/trace.rs`
polls the playback registers over HID (one cycle of four reads takes about 36 ms, which is the
time resolution below) while it opens an exclusive-mode silent stream at 2 s and closes it at
6 s. Times are relative to the first poll that shows the stream. Columns are
`$0F18 / $0F1A / $0F1B / $0F3E`; "muted" is `$0F15`/`$0F16` bit 7.

| Mode | Idle | Start | Stop (first poll after the close) |
|---|---|---|---|
| default | `03 / 00 / 33 / 02`, not muted | `$0F1B` 33 -> 00 only | `$0F1B` back to 33 |
| low click | `00 / 33 / 33 / 02`, muted | `03 / 00 / 00 / 02` and unmuted at once | `00 / 33 / 33 / 02`, muted |
| balanced | `00 / 77 / 33 / 00`, muted | `03 / 33 / 33 / 02` at once; `$0F1A` and `$0F1B` 00 at +589 ms; unmuted 37 ms later | `00 / 33 / 33 / 00`, muted; `$0F1A` 77 at +812 ms |
| lowest noise | `00 / 77 / 33 / 02`, muted | `03 / 33 / 33 / 02` at once; `$0F1A` and `$0F1B` 00 at +588 ms; unmuted 148 ms later | `00 / 77 / 33 / 02`, muted |

- Lowest noise with the soft-start time set to `0x1E` (300 ms): `$0F1A` 33 -> 00 after 294 ms
  and the unmute 150 ms later. With the default `0x3C` (600 ms) the same step took 588 ms. T2
  (`0x0A`, 100 ms) is the other part of the unmute delay; the rest is the poll resolution.
- Balanced stop: the 800 ms timer of the power-down measured 812 ms (other decay times below).
- The mode tuples, the raw bytes (`$14B1..$14B4`, `$16C9`, `$17CA`, the 43-byte window,
  `$1017`) and the registers above were read back after installing each mode.
- Start and stop transients in the microphone recording (previous research, 20 ms peak of the
  high-passed signal against a median of -68 dBFS): the ROM default (low click) shows nothing more
  than 12 dB above the noise floor; the clamp routines (lowest noise and the clamp-only routine
  below) both show a transient of about -14 dBFS at the start and again at the stop.

## Balanced stop decay time

The balanced routine ends a stop with the timer `MOVI #imm,$D7` at `$1F0E` (units of 10 ms, `0x50`
= 800 ms stock). cxtune writes it in place within `0x0A..0xC8` (100..2000 ms) while the hook is
live, and puts the default back before the routine is replaced. Measured with a silent exclusive
stream closed at 4 s: the time from the close until `$0F1A` showed `0x77` (polls about 30 ms
apart).

| Decay byte | Decay time | Clamp after the close |
|---|---|---|
| `0x0A` | 100 ms | 0.13 to 0.2 s |
| `0x1E` | 300 ms | 0.33 s |
| `0x32` | 500 ms | between 0.03 and 0.53 s (the poll after the first change came 0.5 s later) |
| `0x50` | 800 ms | 0.8 s |
| `0xC8` | 2000 ms | 2.03 s |

Through the engine, the idle watch after a microphone close (100 ms polls, set-up of about 0.4 s
included) reported 402 ms for 100 ms, 403 ms for 300 ms, 806 to 1015 ms for 800 ms and
2032 ms for 2000 ms. A shorter time clamps the driver sooner; whether 100 ms is audible as a
click was not listened to. The start dwell of the routine (the 600 ms timer that `JSR $D671`
sets) is in the ROM and is not tunable.

## Microphone capture in each mode

Method: a shared-mode 48 kHz capture stream opened at 0.5 s and closed at 4.5 s while the
registers were polled (about 40 ms apart). Boost 6, noise reduction on, AGC and sidetone off,
headset not worn. The registers are state checks and do not depend on what the microphone picks
up. Proven.

| Mode | Idle `$0F18 / $0F1A / $0F1B / $0F3E` | Capture open | After the close |
|---|---|---|---|
| default | `03 / 00 / 33 / 02` | nothing changes except `$00A5` = 01 and `$13DC` = 01 | nothing changes |
| low click | `00 / 33 / 33 / 02` | the same | the same |
| lowest noise | `00 / 77 / 33 / 02` | the same | the same |
| balanced | `00 / 77 / 33 / 00` | `$0F1A` 77 -> 33 and `$0F3E` 00 -> 02 within 40 ms | `$0F3E` 00 within 41 ms; `$0F1A` stays 33 and shows 77 at the next poll 0.85 s after the close |

So only the balanced mode changes the output for a capture, and the output is idle again when
`$0F1A` is `0x77` and the references are off (`mode::Output::is_idle`); the engine watches that
after every microphone close in that mode. Through the engine the idle was reported 999 to
1202 ms after the close with the default decay time.

The level meter showed digital silence for the first 0.2 to 1.3 s of a capture started from
idle in balanced mode and for about 0.3 s in the other modes, and in balanced mode a stretch of
higher levels that ended at the latest 3.1 s after the first samples in 17 timed starts. These
captures were made while someone worked at the PC (keyboard and mouse), so no level from them is
used anywhere in the documentation and the 4 s that the Test tab leaves out at the start of
every measurement is provisional. A check in a quiet room is part of the attended listening
test.

## Test tab checks

Made through the engine on the real dongle (boost 6, noise reduction on, headset not worn). Only
what the engine and the registers report is recorded here; no level, noise floor or noise
reduction figure from a capture is, because the room was not quiet.

| Check | Result |
|---|---|
| Level meter | opened the stream, reported about 20 levels per second, closed it on request; stopping the engine with the meter on closed the capture stream (`$13DC` = 0, `$00A5` = 0) |
| Noise floor, recording | ran for the warm-up plus the requested time and reported a result; a 3 s recording was kept in memory |
| Noise reduction comparison | switched `$1151` bit 2 and requested a reload twice, and the setting read back as on afterwards |
| Playback of the recording, click test | ran (the output was idle first, the stream ended with the clip). They played a recording of the room (peak below -50 dBFS) and the 1 kHz tone at -40 dBFS; they were not listened to. |
| Waiting for idle | the click test and noise listening waited 806 to 1015 ms after the microphone close in balanced mode (default decay time) and 0 ms in the other modes; noise listening ended with no stream open |

The attended listening test covers what these cannot: the noise floor and the noise reduction
comparison in a quiet room, and how the click test tone, a played-back recording and the surround
sound.

## Virtual surround

`$1152` bit 3 switches the codec's virtual surround on and `$10D3` bits 3:0 hold its level (stock
off and `0x04`). The ROM reads them when a playback stream opens or at a NEWC pass and sets
`$00A4` bit 4 while the effect is applied. cxtune writes only that bit and the low nibble of the
level (1 to 10; the ROM takes 0 as 1 and more than 10 as 10, from the disassembly), through the
guarded writes described above, and applies the saved choice with the other DSP settings.

Observed on the device:

- `$1152` 00 -> 08 and back, read back after each write; `$10D3` set to 2, 4 and 8 and read back.
- With a silent playback stream opened, `$00A4` was `0x10` within 0.1 s with the surround on and
  stayed `0x00` with it off; it was cleared when the stream closed.
- A level change with a playback stream open made two writes (the level and a NEWC reload) and
  no warning; without a stream it made one, and the next stream picks the level up.
- The engine applied the saved surround again with one write after `$1152` was cleared by
  another program.

**The dongle's own control (inferred, not tested).** The vendor patch has code at
`$18BA..$18C5` that toggles `$1152` bit 3 and does not touch `$10D3` (found in the
cross-references of the patch by a reviewer). That a control on the dongle switches the virtual
surround is inferred from that code and was not tested: no button press was made on the device.
ROM `$DA30` (index 17, 0-based, of the jump tables at `$F083` and `$1885`) steps through three states, each
followed by a NEWC request: from off it sets bit 3 and clears `$10D3` bit 4; the next call sets
`$10D3` bit 4 as well; the call after that clears both (a reading of the ROM by a reviewer). Which
physical control, if any, calls either routine is not known. The engine follows a surround switch
that it did not make (the chosen setting takes the value the dongle holds) when the dongle held
the chosen settings at the last pass.

Not measured: what it sounds like (nothing but digital silence was played, and nothing was
listened to), whether the NEWC reload clicks while audio plays, and the effect on loudness. The
setting is shipped as experimental.

## Not offered: the clamp-only routine

An earlier 23-byte routine only clamped the driver at idle:

```
ad 18 0f 29 03 d0 0f 68 68 a9 77 8d 1a 0f ad 1b 0f 09 33 8d 1b 0f 60
```

(`LDA $0F18; AND #$03; BNE` to the final `RTS` while playing; otherwise `PLA PLA`,
`$0F1A = 0x77`, `$0F1B |= 0x33`, `RTS`.) It leaves the same idle state as lowest noise
(`$0F1A = 0x77`, `$0F1B = 0x33`, references untouched) and its recording shows the same
transients at the start and at the stop (-14.4 / -14.2 dBFS against -14.1 / -14.2 dBFS for
lowest noise). Lowest noise adds the explicit start sequence and the tunable soft-start time, so the
clamp-only routine is not offered as a mode. Its muted time at the start was not measured on
the device, so this is a judgment and not a proven dominance. A device that is found with this
routine installed is refused as unrecognised (nothing is written); a replug clears it.

## Changing between modes

A mode is the tuple (DAC holds, hook opcode, hook operands, window part 1, window part 2).
`mode::plan` moves any recognised tuple, including the half-changed ones an interrupted change
leaves behind, to any mode:

1. A soft-start time or decay time other than the default is first put back (`0x3C` or
   `0x50`, in place) when the routine that holds it is replaced. A tuned time is recognised only
   in a whole routine of its kind (both window parts), so this step is always possible. The
   chosen time is written again after the routine it belongs to is installed.
2. If the hook is live and anything it runs will change, it is turned to RTS first.
3. Window parts and operands change only while the hook is RTS (one exception, below).
   Installing a routine writes its parts, then the operands. Removing one writes the operands,
   then the parts.
4. The hook is turned to JMP last, and only if the window holds one whole routine and the
   operands point at it.
5. DAC holds are released before the hook changes and held again after it.

Unrecognised bytes anywhere (ROM identity, hook, window, DAC routines) are refused before the
first write, and before a stream is opened.

### The exception: a one-byte immediate operand

A single-byte immediate operand may be changed in place while the hook is live when every other
byte of the routine is guarded. cxtune does this for the balanced decay time at `$1F0E` and the
lowest-noise soft-start time at `$1F15`. Both bytes are the immediate of a `B2 imm D7`
(`MOVI #imm,$D7`) instruction that starts one byte earlier. The write is guarded by the whole
window (every other byte equals the routine's image) and by the allowed range of the value.
Why it is safe (a reading of the ROM and the patch by a reviewer):

1. The opcode and the zero-page operand of the instruction are guarded unchanged, and every
   allowed value gives the same 3-byte instruction, so no instruction boundary, opcode or branch
   target can change.
2. It is one byte, so the firmware performs it as one store. The HID handler (`b48d`, in the
   main-loop service `7ad8`) and the hook (`8e6d`, reaching `$D45E JSR $14B1`) run in the same
   main loop, so the routine never runs in the middle of the write. Without that ordering a
   1-byte store is atomic on the 8-bit core, and both the old and the new value are valid.
3. The immediate is loaded into timer `$D7` only at one transition of the routine, and a timer
   that is already running keeps its count. The decay immediate at `$1F0E` is loaded once per
   stop, at the reference-off transition. The soft-start immediate at `$1F15` is loaded once per
   start, at the `0x77 -> 0x33` transition (`$1F10 CLRB #$44`, `$1F14 MOVI #$3C,$D7`,
   `$1F17 RMB5 $9E`), not at a stop. A write therefore takes effect at the next stop (decay) or
   the next start (soft-start).

Before the first write of a change that needs the stream, the executor waits until the codec
shows it: `$0F18` bit 0, `$0F1A == 0x00`, `$0F1B == 0x00` and `$0F3E` bit 1. The stream is
only running on the host side when Windows reports it; the codec follows a moment later, and
in default mode `$0F1B` is the one register that tells. Each write that needs the stream is also
refused unless the stream is still running and the codec still shows playback, so a stream that
dies partway stops the change before the next such write (DAC-hold writes in between need
nothing and are made).

## The Windows audio engine and the output pin

The codec follows the USB stream of the dongle's playback endpoint: while Windows keeps it
running the codec shows playback (`$0F18 = 0x03`, `$0F1A = 0x00`, `$0F1B = 0x00`), and the idle
state, with its lower noise floor, starts when Windows ends it. Observed on this machine (Windows 11
build 26200):

- **Exclusive mode.** The USB stream ends when an exclusive-mode stream closes. `$0F18` was
  `0x00` at the first poll after the close in 12 of 12 trials (79-112 ms after the close began,
  which includes stopping the stream and one poll), and 14 mode changes in a row, each with
  its own stream, left the output idle every time.
- **Shared mode.** After a shared-mode stream closes, Windows often keeps the USB stream
  running: in about half of the trials the output stayed active, in one trial for more than
  7 minutes. A shared stream from Python (PortAudio WASAPI) does the same, so it is not
  specific to cxtune's stream. A WDM-KS stream (PortAudio) released it every time; a second
  short shared stream released a held pin in 1 of 4 trials.
- **Exclusive mode cuts other programs off.** Opening an exclusive stream while a shared-mode
  client had a session on the endpoint (playing silence, or merely open) stopped that client
  for good: its callbacks ended, and the idle one could not be started afterwards.

So `audio::open_silence` opens an exclusive 16-bit stereo stream (48, 44.1 or 96 kHz) only when
no program other than the system sounds and cxtune's own process has a session on the endpoint,
active or not, and a shared-mode stream otherwise (also when the endpoint offers no such
exclusive format). With a program's session open the output is in use anyway, and a shared
stream next to it left both an active and an idle Python client running. The buffer is filled
with zeros and flagged silent.

The format and the device period are probed first and the session check runs immediately
before the exclusive `Initialize`. A program that opens a session after the check but before
`Initialize` returns is still cut off: WASAPI offers no way to make the two one step. The
exclusive buffer is requested as four device periods and refilled every 20 ms; on this machine
the device period is 10 ms and the buffer came back as 3840 frames (80 ms at 48 kHz), so a late
refill cannot underrun. If the stream fails, the failure is recorded before the stream is
stopped.

Writes do not depend on the pin: the planner runs from either state. `status` shows
`$0F18`/`$0F1A`, which tells a held pin from a real idle.

## Microphone DSP settings

Settings: noise reduction, AGC, sidetone on/off and sidetone level. Static analysis of the ROM
and the patch listing found the mechanism; the measurements below were made on the device with
the HyperX microphone as the only capture source (headset not worn, quiet room) and digital
silence as the only playback.

### Mechanism

- The main loop runs the service routines (including the HID handler) and then applies the
  configuration. If `$1154` bit 0 (NEWC) is set, the ROM reloads the DSP settings and clears
  the bit; the appliers for noise reduction and AGC run only in that reload. The ROM sets
  NEWC itself when a stream opens. A change of `$1151` bit 2 or 4 therefore takes effect at
  the next NEWC, whoever requests it. (proven)
- The NEWC pass also loads the DAC filter (`cfc2`; from the static analysis, not observed on
  the device). Because the ROM sets NEWC on every stream open, the firmware runs this reload
  at each playback start as well. Whether a NEWC made while a playback stream is running
  causes a click in the headphones was not measured (see Hazards).
- The appliers run only while the microphone gate is satisfied; an open capture stream
  satisfies it (`$13DC = 0x01`). With no capture stream `$00A4` stays `0x00`. (proven for a
  48 kHz shared-mode capture stream; other formats were not tried)
- Sidetone needs no NEWC: the ROM evaluates `$1151` bit 7 on every pass and starts the path
  (`$00A8` bit 3) when a capture and a playback stream are both open. It copies `$1276/7` to
  `$112A/B` on every pass and, while the path is on, to the shadow `$1255/6` and the DSP.
  (proven)
- A value written to `$1151` or `$1276/7` while the streams are closed stays in RAM through
  stream opens and closes and is applied by the ROM's own reload at the next stream open.
  (proven: written at idle, then several capture and playback open/close cycles)

### NEWC request

`patches::newc` reads `$1154`. If bit 0 is set, the ROM has a request of its own pending.
Its reload runs after every write cxtune has made, so it applies them: cxtune waits for it
(5 ms polls, 500 ms limit) instead of making another request, and the setting counts as
applied. Otherwise it reads the guards, reads `$1154` again (a request that appeared in the
meantime is waited for in the same way), writes the byte with bit 0 set and polls until the
ROM clears the bit. The other bits are then compared with what was read before the write. If
they differ, a stream event coincided with the request. If they do not, bit 1 is compared with
`$13DC != 0` and bit 4 with `$13DD != 0` (the ROM keeps `$1154` bits 1 and 4 equal to the
capture and playback stream state): a disagreement means the request wrote back stale stream
bits over an update of the ROM's, and counts as a stream event too. The settings are written and the
reload was done, so this is reported as a stream event (a warning in the CLI and the engine,
which does not stop anything), not as a failure. The request may have overwritten the ROM's own
update of that byte, which cannot be told apart from here from an update that came after the
write, so the request is not repeated. A request the ROM does not consume in time is an error. The write
cannot be made atomic: the protocol has no read-modify-write, so the window between the read
and the write is one HID round trip (a few milliseconds). cxtune asks for NEWC only after it
changed noise reduction or AGC and a capture stream is open (`$13DC`); otherwise the next
stream applies the change and `$1154` is not touched.

Measured: 72 requests (66 with a capture stream open, 4 with only playback open, 2 with no
stream), 40 of them as 20 off/on pairs of `mic-nr` in a row. Each was consumed within the
limit, and in the research script before the first poll after the write (which itself takes
50 ms). In the 58 where `$1154` was compared before and after (including all 40 of the run
in a row) it was identical. A pass increments the
counters `$125C..$125F` and rewrites `$00C6`; in the compared ranges (`$0000..$00FF`,
`$0F00..$0F7F`, `$1000..$12FF`, `$13D0..$13EF`) the only other changes seen were the applier
flags and, once each, `$0044/$0045` and `$009D`.

With another process opening and closing the capture stream every 0.2 to 0.8 s (167 cycles in
two runs), 130 alternating `mic-nr` changes each made one NEWC request while the stream was
open: 0 errors and 11 stream events. In all 11, `$1154` went from `0x02` to `0x00` between the
read and the end of the request, which is the capture stream closing during it. Each was
reported as a stream event and the run went on. At the end `$1154` was `0x00` with the
microphone closed.

### Applying the saved settings

`settings::ensure` (used by the engine, see [engine.md](engine.md)) applies the microphone DSP settings first, then the mode,
then the mic boost. A saved sidetone level is written whether sidetone is on or off (switching
sidetone on writes it first; switching it off comes before the level, so the level then
jumps). A DSP reload request is awaited before the mode change opens its own
playback stream, so the request does not coincide with that stream's open and close, and the
ROM's reload at the start of that stream sees the settings already in place. A DSP setting
that is refused writes nothing; the mode and the mic boost are still applied and the refusal
is returned afterwards. Any other error in the DSP step (device gone, readback mismatch) stops
the sequence. The result always carries the report of what was done before the error (writes
made and stream events seen); only the writes of a mode change that fails part-way are not
counted. Measured with the capture stream open: changing to low click with noise reduction
off wrote `$1151` at 92 ms, `$1154` at 201 ms and the first mode byte (`$14B1`) at 529 ms;
changing back to balanced with noise reduction on wrote `$1151` at 94 ms, `$1154` at 206 ms
and the first mode byte (`$1EFC`) at 521 ms. A second run wrote nothing.

### Results

| Setting | Device result |
|---|---|
| Noise reduction (`$1151` bit 2, stock on) | `$00A4` bit 7 followed the bit after NEWC in all 12 changes. Stationary noise, median of 50 ms frames over 30 s: on -63.9 dBFS, off -52.0, on -63.9, off -52.0, on -63.9 dBFS, an 11.9 dB drop repeatable to 0.1 dB, 10.8 to 12.1 dB in every band from 20 Hz to 8 kHz. Effect on speech not measured. The click when the microphone opens reached -1.1 dBFS in the recording that started with NR off and -18 dBFS in the two that started with NR on. |
| AGC (`$1151` bit 4, stock off) | `$00A4` bit 5 followed the bit after NEWC in all 16 changes. **No effect on stationary noise**: 90 s per state at -64 dBFS (NR on), 60 s at -52 dBFS and 45 s at -46 dBFS (NR off, boost 7), each off/on/off (and on again); level differences at most 0.1 dB and no rise over time in the 5 s profiles. The firmware ran the AGC applier, but its behaviour on speech could not be measured without an audible test signal. Shipped as experimental. |
| Sidetone on/off (`$1151` bit 7, stock off) | With capture and playback open, `$00A8` bit 3 was set after the write (no NEWC) and cleared after the clear, (4 of 4 switches); `$1255/6` became `0x03B3` at the first switch-on; written at idle it was running as soon as both streams were open. With only one of the two streams open it stayed clear. Audible result not measured (no listening test). |
| Sidetone level (`$1276/7`, stock `0x03B3`) | `$112A/B` followed the register for `0x1000`, `0x0001`, `0x0200` (sidetone off) and `$1255/6` followed it for `0x0200`, `0x0001`, `0x0100` (sidetone on) within 0.3 s. The register is in UAC 1/256 dB units per the ROM and not exposed to the host (see "Sidetone gain unit"); how the DSP applies it was not measured and the loudness was never listened to. With sidetone on, cxtune moves the gain in 4 steps of equal size in dB, one guarded write each, about 93 ms apart (0.39 s in all); with sidetone off it is one write. The equal-dB steps are covered by the tests on the fake dongle only: the device runs of this ramp, made with steps of equal ratio of the register (947 to 200 wrote `0x0282`, `0x01B3`, `0x0127`, `0x00C8`), kept `$00A8 = 0x38` and the path running, and sidetone was not switched on again. |
| EQ preset (`$1152` bits 1:0) | **Inert, not shipped.** Writing 2, 1 or 3 was never readable back, even 8 ms later; `$1248` stayed `0x06` and a NEWC pass loaded nothing. The hook-5 routine (`$16B5`, `JSR $1650`) of the vendor patch rewrites the two bits to 0 on every pass; changing that would mean editing vendor routine code, which cxtune does not do. |
| Mic boost (`$1017`, existing) | One step more (6 to 7) raised the noise floor from -52.0 to -45.8 dBFS (+6.2 dB), consistent with 6 dB steps. |

Mic boost and signal-to-noise ratio, measured on the real dongle with an IEM microphone, one
take per boost value (speech level and noise floor in dBFS):

| Boost (`$1017`) | Speech | Noise | Speech minus noise |
|---|---|---|---|
| 4 (vendor) | -57 | -65 | 8 dB |
| 6 | -45.3 | -57.9 | 12.6 dB |
| 7 | -40.7 | -50.3 | 9.6 dB |

Boost 6 gave the largest difference of the three. The noise was dominated by mains hum at 50,
100 and 150 Hz, which is also present in the stock state, and each value is a single take, so
the ranking is indicative only. The vendor boost of 4 gives a speech level of about -57 dBFS in
this test.

In the recordings the largest 10 ms peaks were the same size at the toggle times as elsewhere
(-42 dBFS at boost 6 with NR off, -36 dBFS at boost 7), so no switching transient was seen in
the microphone recordings. Nothing was recorded or listened to on the playback side: a click
in the headphones when a NEWC pass reloads the DAC filter while audio plays is not measured.

Stock values on this device: `$1151 = 0x04`, `$1152 = 0x00` (idle and with streams open),
`$1248 = 0x06`, `$1276/7 = b3 03`, `$1255/6 = d5 40` until the sidetone path first runs (it
holds the last gain afterwards).

### Sidetone gain unit

`$1276/7` is encoded by the ROM's class-request handler as a USB audio class volume in 1/256 dB
(feature unit 11, which this unit's descriptor does not declare, so no host is told anything about
the register). How the DSP applies the value was not measured. The ROM copies the register
unconverted to DSP `$2634` via `$112A` (a reading of the ROM by a reviewer). Evidence for the
unit:

- The ROM's class request table (at `$EA84`) serves the sidetone feature unit's GET_CUR
  directly from `$1276/7`, and its MIN, MAX and RES from `$0081`, `$0083` and `$0085` (a reading
  of the ROM by a reviewer).
- Those three words, read from the device: `$0081/2 = 0x0000`, `$0083/4 = 0x1000`,
  `$0085/6 = 0x0001`, followed by `0x03B3` at `$0087/8`. In 1/256 dB that is a range of 0 to
  +16 dB in steps of 1/256 dB, and the stock value is +3.70 dB.

So the unit is ROM-evidenced. The DSP's own reading of the value was not measured,
and nothing was listened to, because sidetone was left off for the whole work on the device (the
register itself was only read). cxtune treats the value as dB because that is the conservative
reading for its warnings and ramp; the `0x1000` cap holds whichever way the DSP reads it. What
follows from the dB reading:

- Every register value in the range is a gain of 0 dB or more. There is no quiet end: the lowest
  value cxtune writes (`0x0001`, 0.004 dB) is only 3.7 dB below the vendor default, and `0x1000`
  is +16 dB. The only silent setting is sidetone off (bit 7 of `$1151`), which is the stock
  state. The class's silence value has the high bit set (`0x8000`); cxtune never writes it.
- The level is shown and set in dB (0.0 to 16.0; 0.0 is written as `0x0001`). A change while
  sidetone is on is made in 4 steps of equal size in dB, one guarded write each.
- A default for a user who has never chosen a level is the vendor default, +3.70 dB: it is what
  the dongle holds after it starts, and the next lower level that makes sense (unity) is only
  3.7 dB lower. Sidetone itself stays off until it is switched on.

Not tested: a listening test of sidetone, AGC or the virtual surround; the playback side of a
NEWC request (a click while audio plays); a very short decay time as a click; capture formats other than 48 kHz mono
shared mode (the gate compares a format value with `$115A`); a physical replug and a real sleep and resume (the saved
settings are applied by the same writes, which were exercised at idle and with streams open).

## Hazards

- **Hook off while the references are off.** Balanced mode turns the references (`$0F3E` bit 1)
  off at idle. The ROM never turns them back on, so switching the hook to RTS (or leaving a
  non-balanced routine in place) at that moment leaves them off until a replug. The hook is
  therefore never turned off unless `$0F3E` bit 1 is set (checked in the write guard), and
  every change that needs the hook off runs with a stream open, which makes balanced mode
  raise the references first (the plan waits for the codec to show the stream, with `$0F3E`
  bit 1 set, before the first write).
- **A driver transition while the hook is off.** With the hook at RTS at idle the ROM parks the
  driver at `0x33`, and the next routine then clamps it at `0x77`: two bias steps. With a
  silent stream open the driver is at `0x00` throughout, so the swap causes none.
- **A live hook with half a routine.** Any inconsistent window under a live hook runs garbage
  code. The hook is only turned on when both window parts and the operands match one routine.
- **A lost update of `$1154`.** NEWC is the only write to a byte that the ROM also changes
  (stream state). The read before the write is at most one HID round trip old, and a stream
  event inside that window would be overwritten; the check after the request only shows that
  such an event happened near the request, not that it was lost. It is reported as a stream
  event, which is a warning.
- **A NEWC request while audio plays.** The reload includes the DAC filter load. The ROM does
  the same at every stream open, but a request made during a call or other playback was not
  checked for a click in the headphones, because no playback was captured or listened to.
- **A NEWC for the surround level.** Changing the level while a playback stream is open and the
  surround is on sends a NEWC request, which reloads the DSP including the DAC filter. A click
  in the headphones was not checked (see the NEWC hazard above).
- **A long decay time.** The balanced routine keeps the driver at `0x33` (parked, not clamped)
  for the decay time after every stream and capture, up to 2 s.
- **Sidetone loudness.** The register is in UAC 1/256 dB units per the ROM and not exposed to the
  host; read that way every value amplifies (see "Sidetone gain unit"), so no level is quiet. DSP
  loudness was not measured. The level stays in `0x0001..0x1000` (0 to +16 dB), sidetone
  is off by default and switching it on applies the saved level first. Only sidetone off is
  silent; a level was never listened to.
- **One program at a time** (see the protocol above).
- A replug restores the vendor patch, so saved settings have to be applied again (`watch`).
