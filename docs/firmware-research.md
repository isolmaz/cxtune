# CX20773 firmware research (HyperX Cloud II USB dongle)

Background for contributors who want to build on cxtune's reverse engineering of the dongle's
firmware. Registers, modes, patch images and measurements are in
[firmware-notes.md](firmware-notes.md); this document does not repeat them. It covers how the
firmware is put together, how it was analysed, what is established and what is not.

Device: USB `0951:16A4`, Conexant/Synaptics CX20773, mask ROM `CATP_V00.15.00.00`, EEPROM patch
`Foreshot_HyperX_Patch_23-4E-0A_Rev_0010_NVM-CATP_dp_00.02.05.00`. One unit, one ROM and one patch
revision were examined.

## 1. Overview and how to read this document

Labels:

- **PROVEN**: observed on a real device, or a byte-exact fact of a dump or of a source file we
  read (including what a sequence of instructions literally does).
- **INFERRED**: purpose or behaviour deduced from code or from consistency between sources, not
  exercised on hardware.
- **UNKNOWN**: not determined.

Conventions: `$xxxx` is a CPU address (64 KiB space); `EEPROM 0xxxxx` is an offset in the
EEPROM; `img 0xxxxx` is an offset in the EEPROM image (same numbers as EEPROM offsets).

### Safety policy

cxtune changes volatile state only. It sends HID memory requests with flags `0x00` (read CPU
memory) and `0x40` (write CPU memory) and never `0x20` (EEPROM access), `0x80` (DSP memory) or `0x10`
(address at or above 64 KiB). The one exception is `cxtune dump` (section 9), a read-only path
that sends flag `0x20` without `0x40` to read the EEPROM; its request type has constant flags and
no data, and the request builder accepts only the flag values `0x00`, `0x20` and `0x40`, so a
request with `0x20|0x40` cannot be built. Every write passes one guarded function (see
[Safety design](../README.md#safety-design) and [Writable regions](firmware-notes.md#writable-regions)).
Reasons:

- A replug reloads patch RAM from the EEPROM, so a bad RAM write is undone. An EEPROM write is
  not, and the recovery behaviour of a damaged EEPROM is not established (section 5.4).
- The device-side handler of the memory protocol has no access control: it performs a write to
  any CPU address, and an EEPROM write when flags `0x20` and `0x40` are set (section 4). Nothing in the
  protocol protects the EEPROM header or the patch.
- The update method used by fwupd for this chip family has no rollback and no checksum
  (section 5.3).
- The DSP program is loaded from the EEPROM patch at boot, and the effect of a DSP write is not
  documented.

Contributions should keep to the same rule (section 9).

## 2. Hardware and firmware architecture

### 2.1 Components

| Part | What | Status |
|---|---|---|
| CPU | 8-bit core, 65C02 plus Rockwell bit instructions plus Conexant extensions (section 3) | PROVEN |
| ROM | 36864 bytes at `$7000..$FFFF`, mask ROM, read with ordinary HID reads | PROVEN |
| EEPROM | 16 KiB serial EEPROM on a bit-banged I2C bus (device address `0xA0`), holds patch, config, USB descriptors | PROVEN (contents), INFERRED (bus) |
| RAM | Config RAM `$1000..$11E8` and patch RAM `$1460..$2253` are loaded from the EEPROM at boot; the rest is working RAM | PROVEN |
| DSP | Voice/audio DSP behind two mailbox ports, program/data memory loaded from the EEPROM patch | INFERRED |

The ROM contains complete default behaviour (USB device descriptor `0572:1430` at `$E395`, a
default HID report descriptor with the memory-access reports `0x04`/`0x05` at `$E56A`, default
configuration at `$F117..$F368`, a UART console and download strings at `$E24B..$E354`). The
EEPROM replaces and extends it: identity, descriptors, configuration, a code patch and DSP code.
That the ROM defaults are used when no usable EEPROM is present follows from the code (section
5.2) and has not been tried.

### 2.2 CPU address map

As modelled in our Ghidra project (only the ROM boundary is certain, the rest are working
assumptions): zero page and stack `$0000..$01FF`; UART ring buffer `$0200..$023F`
(indices `$0240`/`$0241`); loader scratch `$0300`; hardware registers `$0400..$04xx`, `$0B00`,
`$0C00..$0D0x` (DSP mailboxes, USB), `$0F00..$0F7F` (codec); config RAM `$1000..$11E8`; working
variables `$1200..$13FF`; patch hook table and patch RAM `$1457..$2253` (the EEPROM loads from `$1460`); I/O `$3000..$30FF`;
ROM `$7000..$FFFF`.

### 2.3 ROM layout

| Range | Content | Status |
|---|---|---|
| `$7000` | Reset code; `$7003` is both the IRQ vector and the start of the boot sequence | PROVEN |
| `$E24B..$E354` | `(C) CONEXANT`, UART download strings, `CATP_V00.15.00.00` at `$E320`, `DSP LOCKUP` | PROVEN |
| `$E395`, `$E56A`, `$E8C4..` | default device descriptor, default HID report descriptor, USB audio class request table | PROVEN |
| `$F117..$F368` | default configuration: 80 B for zero page `$4C`, 25 B for `$0F50`, 489 B for `$1000` | PROVEN |
| `$F369..$FFDB` | pseudo-random fill, see below | PROVEN |
| `$FFDC..$FFDF` | 4 bytes `db a2 2f 67`, not the output of the fill generator | UNKNOWN |
| `$FFE0..$FFFF` | extended vector table and the 6502 vectors | PROVEN |

The ROM version string is parsed into `$1001..$1004` at boot (`$8C1B`): the digits of
`00.15.00.00` become `15 00 00 00`, which is what fwupd formats as `00.15.00.00`.

**The tail is a generated fill, not hidden data.** `$F369..$FFDB` (3187 bytes) is reproduced
exactly by `x' = 1664525 * x + 1013904223 (mod 2^24)`, output `(x >> 16) & 0xFF`, with state
`0x1B355F` at `$F369`. It has no code references, no compression or transform structure and no
overlap with other firmware we compared. The 4 bytes after it are not a CRC or sum of the
definitions we tried (CRC-32 variants, CRC-32C, Adler-32, 8/16/32-bit sums over several
ranges). The field's purpose is open (section 8).

### 2.4 Boot flow

INFERRED from the static disassembly, with addresses in the ROM listing.

1. `RESET $7000` calls `$70D0` (prints the stack bytes to the UART console) and continues at
   `$7003`: stack, hardware init, `$1000 = 0`, the hook table `$1460..$155E` is filled with `RTS`
   (`$70B8`).
2. EEPROM probe: `$7DC8` clears the bus, `$7B4F` addresses device `0xA0` (`$A1` bit 0 = EEPROM
   present). `$7C15` determines the addressing mode and size (section 5.2); `$8072` checks the
   header and rewrites it where it does not match (on every boot with an EEPROM, section 5.2);
   `$7B9C` checks whether EEPROM `0x02..0x0E` holds the string
   `(C) CONEXANT` and, if so, takes a different path (`$7BB8`) that copies a code record from
   the EEPROM into RAM and jumps to it. This path is not used by the Cloud II image.
3. Both DSP ports are started and awaited with a timeout (`$704C..$7093`).
4. `$7A11`: if the EEPROM is present, its size is known and EEPROM `0x14` is `'P'` (`0x50`), the
   patch stream at the address in EEPROM `0x15..0x16` is loaded (`$82FC`, section 2.5).
5. `$7A50` calls hook 3, then `$8BB7`, which first calls hook 23 and then copies the ROM
   defaults into zero page, `$0F50` and `$1000..$11E8`. `$8C09` calls `$8CFD`, which overlays the
   EEPROM configuration if EEPROM `0x11` is exactly `0x81` (config RAM address = EEPROM offset
   `+ 0x0E29`); back in `$8C09` the version bytes are written, `NEWC` (`$1154` bit 0) is set and
   `$1000 = 1`.
6. Main loop `$7A84` (section 3.4).

Hook 23 of the vendor patch (`$1EA3`) runs before the defaults and the configuration are loaded.
It arms two patch slots that fire later, inside the load, and adjust it: `$8D66 -> $1EC7`
(at the end of the EEPROM config copy) reads 81 further EEPROM bytes (`0x3C0..0x410`) into
`$1581..$15D1`, and `$8C74 -> $1EBA` seeds the sidetone gain `$1276/7` from zero-page `$87/$88`
(the stored default `0x03B3`) instead of the ROM's `$81/$82` (the minimum). Hook 25 (`$1EDA`)
arms `$8E55 -> $1EE1`, which writes `$1581..` back when the ROM saves its configuration.

### 2.5 The EEPROM patch stream

The stream starts at the address in EEPROM `0x15..0x16` (`0x05BF`). Segments are parsed by the
state machine at `$82FC` (jump table `$830F`). In the code segment, CPU address = image offset
`+ 0x0E91`; in the hook table segment it is `+ 0x0E95` (the 5-byte segment header sits in
between). In the loader (`$8327`) flag bit 7 selects DSP or CPU memory and bit 6 selects "run
after loading"; other bits are not tested. Layouts as implemented by our image parser
(`cximg.py`; PROVEN for this image):

| Flag | Layout | Meaning |
|---|---|---|
| `0x00` | `flag, startLo, startHi, endLo, endHi, data[end-start+1]` | load into CPU RAM |
| `0x40` | same | load into CPU RAM, then run it |
| `0x80` | `flag, bank, dstLo, dstHi, ?, endLo, endHi, data[end-dst]` | load into DSP memory (bank `01`) |

There is no end-of-stream flag: the driver loop (`$8300`) parses segments until a run segment
leaves the loader. (`cximg.py` stops at a flag with other bits set; that is the parser's
convention, not the loader's.)

This unit's stream (image offsets):

| Offset | Flag | Target | Bytes | Content |
|---|---|---|---|---|
| `0x05BF` | `0x40` | RAM `$00E8..$00E9` | 2 | `78 60` (`SEI`, `RTS`), run |
| `0x05C6` | `0x00` | RAM `$1460..$15D1` | 370 | hook table and patch variables |
| `0x073D` | `0x00` | RAM `$15D3..$2253` | 3201 | patch code and tables |
| `0x13C3..0x1593` | `0x80` | DSP bank 01 `$8000..$8B48` (5 segments) | 2812 | DSP program/data |
| `0x1EE2` | `0x40` | RAM `$0300..$0302` | 3 | `JMP $8530`, run; `$8530` ends the serial read and unwinds the loader |

The stream ends only because its last segment is a run segment whose code (`JMP $8530`) unwinds
the loader. The patch name string follows at `0x1EEA`; it would be parsed as a CPU segment if
the loader ever got there. No checksum or length field covers the stream.

### 2.6 Patch mechanisms

- **Hook table (PROVEN).** 3-byte entries at `$1457 + 3n`, each `4C lo hi` (`JMP`) or `60 60 60`
  (`RTS`). The ROM calls 73 of the entries from 75 `JSR` sites (main loop, config load, output
  driver, mixer, USB class handlers). 26 entries are populated in this image.
  Hook 30 (`$14B1`, called from `$D45E`) is the one cxtune controls; see
  [firmware-notes.md](firmware-notes.md#hook-table-and-the-driver-update).
- **`PATCH` slots (INFERRED).** `6B slot from to` redirects execution of the ROM address `from` to
  `to`. Targets are either `from + N` (skip N bytes: hook 30's `PATCH #0, $D4BA -> $D4BF` skips
  the ROM's 5-byte store of the idle-park value `0x33` to `$0F1A`) or patch code (for example
  `$947B -> $1F62`). The vendor patch uses slot numbers 0, 1 and 6 to 11 and reuses a number for
  several routines. The number of hardware slots, and what rearming a slot does, are UNKNOWN.
- **RAM trampolines and pointer slots (PROVEN as code).** The ROM keeps `JMP` trampolines in RAM
  (`$12EA`, `$11EF`) whose operands code stores at run time, and callback pointers (`$11EA`,
  `$11F0`, `$12E2`, `$1205`, `$11FC`, `$11FF`); the patch overwrites some of these.
- **Config overlay (PROVEN).** Hook 23 runs first in `$8BB7`, before the ROM defaults and the
  EEPROM configuration are copied. A patch adjusts the loaded values by arming `PATCH` slots
  there that fire inside the load (section 2.4).

## 3. The CPU

### 3.1 Instruction set

Base: 65C02 (BRA, STZ, PHX/PHY/PLX/PLY, TSB/TRB, `JMP (abs,X)`) with the Rockwell bit
instructions RMB/SMB/BBR/BBS. The Conexant extensions replace several 65C02 opcodes. Encodings
are from the dump; the semantics column is what the disassemblers and Ghidra model use.

| Opcode | Mnemonic | Bytes | Semantics | Status |
|---|---|---|---|---|
| `D2` | `SETB #mask,abs` | `D2 mask lo hi` | `[abs] \|= mask` | PROVEN (modes built on it, readbacks) |
| `C2` | `CLRB #mask,abs` | `C2 mask lo hi` | `[abs] &= ~mask` | PROVEN |
| `F2` | `BRSET abs,#mask,rel` | `F2 lo hi mask rel` | branch if `[abs] & mask != 0`; target = address + 5 + rel | PROVEN for single-bit masks |
| `E2` | `BRCLR abs,#mask,rel` | `E2 lo hi mask rel` | branch if `[abs] & mask == 0` | PROVEN for single-bit masks |
| `B2` | `MOVI #imm,zp` | `B2 imm zp` | `[zp] = imm` | PROVEN (timer immediates changed live) |
| `6B` | `PATCH #slot,from,to` | 6 | arm a ROM patch slot | INFERRED |
| `7B` | DSP mailbox | `7B sub [operands]` | software trap; sub-op `0,2,..,0x16`, length 2/4/6; handlers at `$7550` table write `$0C16/17`, `$0C1A/1B` (port 0) or `$0C96/97`, `$0C9A/9B` (port 1), start the transfer and wait for the strobe | INFERRED |
| `22` / `32` | push / pull A,X,Y | 1 | ISR and trap prologue/epilogue pair; `32` precedes `RTI` | INFERRED |
| `23` / `33` | bracket pair | 1 | used once around a call inside the `$FFF4` handler | UNKNOWN |
| `AB` | fetch inline operand | 1 | `A` = next byte of the trapped instruction's inline operands (advances the saved PC) | INFERRED |
| `BB` | skip inline operand | 1 | advances the saved PC by one | INFERRED |
| `9B` | indirect operand | 1 | used before `AB` in the memory-operand form of `7B` (`WRM`); the following `BB BB` skip the two pointer bytes | INFERRED |
| `03` | trap return | 1 | ends trap handlers after `32` (like `RTI`) | INFERRED |
| `CB` / `DB` | bracket pair | 1 | `DB` is the first instruction of the `$FFEC` and `$FFEE` trap handlers; `CB`..`DB` surround the operand fetches in the `7B` handlers and the ISR at `$7162`; `CB DB NOP NOP RTS` is the bit-pulse helper (`$8061`) called eight times per EEPROM byte by `$8046` | UNKNOWN (semantics) |
| `13` | one use | 1 | `$DFDA`, inside a short delay routine | UNKNOWN |
| `64` | `ADD zp` | 2 | `A += [zp]`, no carry in (replaces 65C02 `STZ zp`) | INFERRED (idiom) |
| `89` | `ADD #imm` | 2 | `A += imm`, no carry in (replaces `BIT #imm`) | INFERRED (idiom) |
| `1A` | `NEG A` | 1 | `A = -A` (replaces `INC A`) | PROVEN once: DSP volume shadow `$1260 = 0x0E` for USB volume code `0x41` |
| `3A` | `ASR A` | 1 | arithmetic shift right (replaces `DEC A`) | INFERRED (idiom) |

Notes:

- A multi-bit mask in `BRSET`/`BRCLR` does not test "all bits" as one would expect; every
  routine cxtune installs uses a single-bit mask (see firmware-notes).
- `JSR`/`JMP`/branches are standard. The 65C02 `(zp)` forms for `LDA`, `CMP`, `SBC` and `AND`
  share opcodes with `MOVI`, `SETB`, `BRSET` and `CX32`; whether the other `(zp)` forms
  (`12`, `52`, `72`, `92`) are intact was not checked.
- The `$FFEC` trap handler (`$8450`, 24-entry table at `$845D`, copies inline words into
  `$0B00..$0B05` and `$1440..$1445`) is entered by an opcode we have not identified.

### 3.2 Vectors

| Vector | Target | Role |
|---|---|---|
| `$FFE0..$FFEA` | `$7530` | `RTS` stub (unused) |
| `$FFEC` | `$8450` | trap with inline index (see above) |
| `$FFEE` | `$7549` | `7B` DSP mailbox trap |
| `$FFF0` | `$719C` | drains the ring buffer at `$0200` (UART output); hook at `$146F` (INFERRED) |
| `$FFF2`, `$FFF8` | `$7531`, `$753B` | short flag setters |
| `$FFF4` | `$7162` | handler that saves `$40..$47` around a call to `$11E9` |
| `$FFF6` | `$70F9` | periodic tick, see 3.3 |
| `$FFFA` NMI | `$70E3` | latches `$3006/$3009`, sets `$BF` bit 3 |
| `$FFFC` / `$FFFE` | `$7000` / `$7003` | reset / IRQ (boot code) |

### 3.3 Timers

The tick handler (`$70F9`) decrements the software delays `$C4`/`$C5`, tracks button state
(`$E7`, `$12E9`), calls the hook entries at `$1553` (if `$4F` bit 0) and `$1556`, and, when `$9C`
bit 3 is set, runs one step of a rotating chain through the RAM trampoline `$11EF`: each step
is a 16-bit down counter (low byte first)
that sets a flag bit when it reaches zero, then points the trampoline at the next step. The
chain is (PROVEN as code):

| Step | Counter | Flag |
|---|---|---|
| `$7216` | `$C7/$C8` | `$9D` bit 0 |
| `$7234` | `$C9/$CA` ("timer 1") | `$9D` bit 1 |
| `$7252` | `$CB/$CC` | `$9D` bit 2 |
| `$7270` | `$CD/$CE` | `$9D` bit 3 |
| `$728E` | `$CF/$D0` | `$9D` bit 4 |
| `$72AC` | `$D1/$D2` | `$9E` bit 2 |
| `$72CA` | `$D3/$D4` | `$9E` bit 3 |
| `$72E8` | `$D5/$D6` | `$9E` bit 4 |
| `$7306` | `$D7/$D8` ("timer 8") | `$9E` bit 5 |
| `$7324` | housekeeping step, wraps to `$7216` | |

A counter is decremented once per chain pass. One count is about 10 ms (measured: `0x3C` took
588 to 589 ms, `0x1E` 294 ms, `0x50` 812 ms with 36 ms poll resolution). Ten steps per pass
would put the tick at about 1 ms (INFERRED; the tick source is not identified). A routine
starts a timer by writing the counter (`MOVI`) and clearing the flag (`RMB`), then polls the
flag. Flag bits stay set until cleared by code.

### 3.4 Main loop and where hooks run

`$7A84` (PROVEN as code):

1. hook 5 (`$1466`), `$96CA`, `$853D`, `$98AA`
2. `$8D77`: hook 25 (`$14A2`), then the EEPROM-save request handler (`$040A`, section 5.2)
3. `$7AD8`: service routines, among them the HID memory handler `$B48D` (when `$B1` bit 7 is set),
   `$B602`, `$B954`, the response stager `$B5CE`, `$B6AE`; hooks `$14F6` (if `$9C` bit 2) and
   `$14F9` (if `$BB` bit 3)
4. `$A9B0`, hook `$14A5` if `$9C` bit 0
5. **park test:** if `$1000` bit 7 is set the loop restarts here (`$7AA2`). This is the "run only
   the basic functionality" state fwupd uses while writing the EEPROM. Everything below is
   skipped, including the configuration apply.
6. if `$9C` bit 6 is set the loop runs hook `$14A8` and the RAM slot `$11F8`, then branches back
   to `$7A84` (`$7AAA..$7AB0`); everything below, including the configuration apply, is skipped
   in that state too. Otherwise: `$BC5B`, `$E1BD`, `$D7E8`, `$D8B3`, hook `$154A` if `$9C`
   bit 1, `$D906`, `$D9D7`, **`$8E6D` (apply configuration: NEWC reload, then the per-pass
   updates, [firmware-notes](firmware-notes.md#mechanism))**, `$E081`, `$DCF4`; repeat.

The output driver update (`$D45E`, hook 30) is called from `$8E9A` inside `$8E6D`.

## 4. HID memory protocol

Framing, flags, timing and the single-process rule are in
[firmware-notes.md](firmware-notes.md#hid-protocol-proven). The device side (INFERRED from
`$B48D..$B5CE`):

- The OUT report lands in `$12EE` (flags), `$12EF` (length), `$12F0` (address high), `$12F1`
  (address low), `$12F2..` (data). The main loop handles it, not an interrupt. The length is
  clamped to 32 (`$B534`). The response is built at `$128A..` and staged to `$13E9..` for the
  IN report.
- Flag `0x20` selects the EEPROM, `0x40` makes the request a write, `0x10` sets a 17-bit
  address flag for EEPROM access, `0x80` selects the DSP path (port chosen by bit 0 of the
  first data byte). Without `0x20`/`0x80`, the request reads or writes `($40),Y` with no range
  check and no write protection.
- EEPROM reads and writes go through the same bit-banged routines the boot code uses
  (`$82CB` read, `$7E83` write, `$7EB4` page commit).

fwupd documents the same framing in its `synaptics-cxaudio` plugin. The buffer sizes in its
source (39 and 35 bytes) differ from the 37 and 33 bytes cxtune sends and receives; the
protocol works with ours, the reason is not established.

## 5. EEPROM

### 5.1 Map

PROVEN from the dump, from fwupd's structure definitions and from the ROM code that reads each
field. The vendor update image (the S-record file of the Cloud II updater) has no record below
`0x02`; it covers 7774 of the 7979 bytes in `0x02..0x1F2C` (the gaps are `0xA2..0x16D` and
`0x411`) and equals the dump wherever it covers. `0x00..0x01` were therefore written by the
ROM (INFERRED; it relies on the size-probe evidence in section 5.2).

| EEPROM | Content |
|---|---|
| `0x00` | `0x4C` (`'L'`), validity signature |
| `0x01` | `0x06`, size code: size = `1 << (code + 8)` = 16 KiB |
| `0x02` | `0x28` (`'('`), checked by the ROM next to `'L'`; `$7B9C` also compares from here against `(C) CONEXANT` |
| `0x03..0x04` | `A3 02`, unexplained |
| `0x05..0x06` | storage size 0; fwupd's storage area at `size - 0 - 4` = `0x3FFC` is empty |
| `0x07..0x10` | zero |
| `0x11` | `0x81`, "config valid" marker; the loader requires exactly this value |
| `0x12..0x13` | `6E 01`: config pointer `0x016E` |
| `0x14..0x16` | `'P'`, patch address `0x05BF` |
| `0x1F` | `0x8D`, unexplained |
| `0x20..0x39` | vendor info: version string pointer `0x1EEA`; CPU patch version `23 4E 0A`; DSP patch version `00 02 05 00`; layout `'S'` (`0x29`), layout version 4, application status 0; VID `0951`, PID `16A4`, revision `0010`; string pointers: language 0, manufacturer `0x54`, product `0x66`, serial `0x42` |
| `0x3A..0x41` | zero |
| `0x42..0xA1` | three USB string descriptors (`length, 0x03, UTF-16LE`): serial `00000000` at `0x42`, `Kingston` at `0x54`, `HyperX Virtual Surround Sound` at `0x66` |
| `0xA2..0x16D` | free (204 bytes, `0xFF`) |
| `0x16E..0x3BF` | configuration block, 594 bytes: `0x16E..0x1BD` to zero page `$4C..$9B`, `0x1BE..0x1D6` to `$0F50..$0F68`, `0x1D7..0x3BF` to `$1000..$11E8` |
| `0x3C0..0x410` | 81 bytes mirrored at `$1581..$15D1` by the vendor patch (INFERRED); `0x410` is `05` |
| `0x411` | `FF`, unexplained |
| `0x412..0x50A` | USB configuration descriptor (249 bytes, 4 interfaces) |
| `0x50B..0x5BE` | HID report descriptor (180 bytes) |
| `0x5BF..0x1EE9` | patch stream (section 2.5) |
| `0x1EEA` | `Foreshot_HyperX_Patch_23-4E-0A_Rev_0010_NVM-CATP_dp_00.02.05.00` |
| `0x1F2A..0x1F2C` | `CX3`, device-kind tag (CX2077x in fwupd) |
| `0x1F2D..0x3FFF` | free (about 8.3 KB, `0xFF`) except `0x1FFD = 0x06` and `0x3FFD = 0x07` |

There is no USB device descriptor in the EEPROM; the identity comes from the vendor info block,
and the ROM supplies the rest.

### 5.2 How the ROM looks at the EEPROM

Static analysis of the ROM listing (the code is PROVEN as bytes; the behaviours are INFERRED, none
was exercised, because that needs a damaged EEPROM):

- **Signature.** `$7B7E` accepts the EEPROM only if byte `0x00` is `0x4C` and byte `0x02` is
  `0x28`. `$7C15` tries both addressing modes (two address bytes, then one), several times each,
  and then reads byte `0x01`: a value `4..8` belongs to two-byte addressing, `0..3` to one-byte.
- **Header check and rewrite.** `$8072` returns early only when `$123B == 0xFF` (no EEPROM
  found). Otherwise, including with a valid signature, it branches to `$80BF`, so the checks
  below run on every boot with an EEPROM. It writes `4C`, the size code and `28` to
  `0x00..0x02` and zeroes `0x03..0x28` when the signature does not match, and it writes `'S' 04`
  to `0x29..0x2A` and zeroes `0x2B..0x41` when those two bytes do not already read `'S' 04`.
  This matches `0x29..0x2A` reading `'S' 04` (layout version 4) and `0x3A..0x41` being zero.
  **A corrupt `0x29` or `0x2A` alone would make the ROM write `'S' 04` and zero `0x2B..0x41`**
  (application status, VID/PID/revision and the string pointers). **A corrupt byte 0 or 2 would
  make it zero `0x03..0x28`**: the config marker and pointer (`0x11..0x13`), the patch pointer
  (`0x14..0x16`) and `0x20..0x28` of the vendor info. `0x2B..0x41` survive if `0x29..0x2A` still
  read `'S' 04`. The size probe below also writes single bytes, one of them inside the patch
  stream (`0x0FFD`).
- **Boot hang (INFERRED; whether a watchdog resets it is UNKNOWN).** In `$7C17..$7C64` the retry
  counter in `Y` decrements only when both signature checks fail. A valid signature with
  byte `0x01` outside `4..8` loops forever (`$7C64 BNE $7C17`, `$7C47 BRA $7C17`), so a damaged
  size code can hang the boot instead of triggering a re-initialisation.
- **Size probe (`$7C6D..$7DBE`; the two-byte part is `$7D03..$7DB0`).** With no usable signature
  the ROM first writes a test byte `0x5A` to `0x0FFD` (to `0x00FD` in one-byte mode) and then
  the marker bytes `9, 8, 7, 6, 5` to `0xFFFD, 0x7FFD, 0x3FFD, 0x1FFD, 0x0FFD` (in that order;
  addresses past the real size alias downwards), reads `0x0FFD` back and then the others upwards,
  and takes the first marker that does not read back as the size code (`0x1FFD` wrong: 4;
  `0x3FFD`: 5; `0x7FFD`: 6; `0xFFFD`: 7; all right: 8). On a 16 KiB part this leaves
  `0x1FFD = 06` and `0x3FFD = 07`, and `0x0FFD = 05`, which the patch later overwrote. That
  matches the dump byte for byte, so the stray bytes are the ROM's own size-probe markers from
  when the part was blank. The one-byte attempt on a two-byte part is not analysed.
- **Config load.** `$8CFD` (called from `$8C09`) requires `0x11 == 0x81`, then reads the config
  pointer from `0x12..0x13` and copies 80 + 25 + 489 bytes. No checksum.
- **Patch load.** `$7A11` requires `0x14 == 'P'` (`0x50`), reads the address from `0x15..0x16`
  and starts the segment loader. No checksum, no length check beyond the segment headers. A
  wrong address or a damaged segment header makes the loader write RAM at addresses taken from
  the stream (PROVEN as code; effect untested). There is no end-of-stream marker (section 2.5):
  a stream that loses its final run segment keeps being parsed, and writes RAM at addresses
  taken from whatever follows it.
- **ROM-initiated writes.** Besides the header initialisation, the main loop handles a save
  request in `$040A` (`$8D77..$8E6B`): bit 7 is the request. With bit 6 set and the low nibble
  `1` the ROM writes the live zero page, `$0F50` and `$1000` areas back to `0x16E..`, writes
  `0x11 = 0x01` first and `0x11 = 0x81` last (validity last; the vendor patch hooks the end of
  this at `$8E55`); with bit 6 clear it only updates the low nibble of byte `0x11`. cxtune never
  writes `$040A`.

### 5.3 How fwupd writes this chip family

Read from the plugin sources (`fu-synaptics-cxaudio-device.c`, `-firmware.c`; PROVEN):

1. Park the firmware: set bit 7 of `$1000`, wait 10 ms.
2. For a full-firmware file only, when moving a device from layout version 1 to 0: zero the
   layout signature and version.
3. Write the file's S3 records in file order, at most 32 bytes each. Each write is followed by a
   read request and a compare of the returned bytes; a mismatch aborts. The HID transfers
   themselves are retried on transport failure (100 ms delay), but a failed or mismatched record
   is not retried and not rolled back.
4. For a full-firmware file only: if `0x14` is `'P'`, write `0x14..0x16` to zero.
5. Unpark (clear bit 7 of `$1000`) and reset the device by writing `0x40` to `$0400`; the write is
   expected to fail because the device disappears.

The parser splits records so that `0x00..0x02`, the test mark `0xBC..0xBD`, the application status
byte `0x2B`, the serial number pointer and the serial string are never written (layout version
1 and above). There is no checksum, no two-stage commit and no power-loss handling in the
plugin. The copy of fwupd's quirk file we read has no entry for `0951:16A4`.

The order is carried by the file. The Cloud II update file (S-record) writes `0x14 = 0x00` first
and the body next, and ends with `0x1F29`, `0x20..0x21`, `0x25..0x28`, the `CX3` tag and finally
`0x14 = 'P'` (`0x50`). Because the ROM only loads a patch when `0x14` is `'P'`, a power loss
during such an update should leave a device that boots without its patch, not one with a
half-written patch. This is INFERRED from the ROM code and the file order; it is not a
property fwupd enforces.

The guard covers the patch only, not the configuration. In the same file, record 2 follows
right after `0x14 = 0` and writes `0x02..0x13`, including `0x11 = 0x81` and the config pointer;
the config block itself comes later (records 9 to 27) and nothing checksums it. A power loss in
between can leave a valid marker over a partly written configuration, which the ROM loads as is.

### 5.4 Recovery: what is open

Not established, and the reason cxtune never writes the EEPROM:

- Whether a device with a damaged or blank EEPROM enumerates with the HID memory interface.
  The ROM has a default device descriptor (`0572:1430`) and a default HID report descriptor with
  reports `0x04`/`0x05` (usage page `0x0C`), which suggests it does; whether those descriptors
  are used when the EEPROM is present but its patch is invalid is UNKNOWN.
- What the ROM does when the patch stream is present (`0x14 == 'P'`) and corrupt (UNKNOWN; no
  checksum was found, so it is probably not detected). The loader has no end-of-stream flag: a
  stream that loses its final run segment keeps being parsed and writes RAM at addresses taken
  from whatever follows it (section 2.5). Whether a watchdog resets a hung loader is UNKNOWN.
- A valid signature with a size code (`0x01`) outside `4..8` appears to hang the boot in the
  retry loop at `$7C17..$7C64` (section 5.2; INFERRED, watchdog UNKNOWN).
- Whether another Cloud II unit with different firmware revisions behaves the same.

A contributor with a spare unit can help here (section 9); an EEPROM test must never be run on
the only device someone owns.

## 6. Config RAM and the DSP

The mechanism (NEWC, appliers, gates) is documented with measurements in
[firmware-notes.md](firmware-notes.md#microphone-dsp-settings); the static analysis behind it
is summarised here with its labels.

**Config RAM.** `$1000..$11E8` is loaded from EEPROM `0x1D7..0x3BF` (section 5.1). Writes to it
are RAM-only. Parts used by the DSP appliers: `$1151` (DSP enables), `$1152` (second enable
set, bits 1:0 EQ preset), `$1153`, `$1154` (NEWC), `$1156..$115A` (rate gates), `$1104..$110E`
(AGC parameters), `$10F6..$10FA`, `$1197`, `$1199` (noise reduction), `$1112..$1129` (12-word
voice table), `$1015`/`$104C`/`$1083` (EQ presets, five 11-byte sections each, Q14 assumed).

**NEWC.** `$8E6D` runs the reload (`$8EAA`, the DAC filter load `$CFC2`) when `$1154` bit 0 is set
and clears the bit; the NR and AGC appliers (`$9231`, `$9307`) are called only from that reload
(PROVEN). The ROM sets NEWC itself on stream and rate events.

**Applier state flags.** Zero-page readbacks (PROVEN for the first three; the others INFERRED
from the code):

| Flag | Meaning |
|---|---|
| `$A4` bit 7 / 5 | noise reduction / AGC applied (DSP commands `0x24`/`0x25`, `0x2A`/`0x2B`) |
| `$A8` bit 3 | sidetone path running (commands `0x30`/`0x31`) |
| `$A4` bits 3, 4, 6, 0 | AEC, stage enhancer, DRC, loudness |
| `$A8` bits 0, 4, 5, 7 | inbound NR, playback mix 0, voice-in, mix 1 |
| `$A6` bit 0 | DSP bypass (also `$1153` bit 7) |

**Gates.** The NR and AGC appliers run only while `$9558` passes: the capture interface alt
setting `$13DC` is non-zero and the stream format value matches `$115A` (`6` here); alternative
sources `$1158`, `$1159`, `$1156`, `$1157` are selected by `$1154` bits 5, 6, 7 and `$1153` bit 0.
Who sets `$1154` bits 5 and 6 is UNKNOWN. On gate failure the applier disables and re-applies at
the next NEWC. Interface state: `$13DB` / `$13DC` / `$13DD` for interface 0 / capture / playback.

**DSP interface.** Two mailbox ports (`$0C12..$0C1B` and `$0C92..$0C9B` in our labels; same
layout, offset `0x80`). Parameter write: id to `$0C1A`/`$0C9A`, value to `$0C16/17` or
`$0C96/97`, strobe bit 0 of `$0C13`/`$0C93`. Command: `$0C96` = command, `$0C97 = 0`, set bit 2 of
`$0C93`, wait for the strobe to clear (`$767C`). The two ports' roles (for example voice versus
playback) are UNKNOWN. About 100 call sites are listed in our analysis folder with the id loaded
before each call; the parameter semantics are not decoded.

**What is inert or unsafe.**

- *EQ preset* (`$1152` bits 1:0): the vendor patch's hook 5 routine (`$16B5`, `JSR $1650`)
  rewrites the bits to 0 on every pass, so writes are not readable back and the loader never sees
  them (PROVEN). Presets 1..3 exist as coefficient tables.
- *Do not touch* (from the static analysis; cxtune does not): `$1151` bits 0, 1, 3, 5, 6; `$1152`
  bit 5 (tone generator); `$1153` bit 7 (DSP bypass); `$1145` bit 7; `$1014` bit 5 and `$1017`
  bit 6 (cleared at every NEWC); `$040A`.
- *Other host-side routes.* Windows audio APIs cannot reach AGC (the descriptor has no AGC
  control in this unit's feature unit 2) or sidetone (no feature unit 11 in the descriptor), so
  RAM writes are the only route here. The Linux driver `cx2070x` (GPL, Conexant, 2012) sets
  `DSP_PROCESSING_ENABLE_1/2` and NEWC the same way.
- *Stage enhancer / `$1152` bit 3.* One read showed `$1152 = 0x08`; other readings (idle and
  with streams) show `0x00` (firmware-notes). What sets the bit and when is UNKNOWN.

**Sidetone gain unit.** See section 8, item 2: the evidence points to USB audio class units,
in which case small register values are not necessarily quiet.

## 7. Methods and tools

All analysis was static unless stated. No dump is published (section 9).

**Disassembly.** `tools/romdis.py` is a recursive-descent disassembler for the ROM
(`$7000..$FFFF`) and the patch (RAM from `$1460`). It seeds from the vectors, the populated hook entries,
`JMP (table,X)` tables and code pointers stored into RAM slots, and writes a listing, data
cross-references, a call graph and the jump tables (about 11,000 instructions decoded; the
remaining gaps are data tables and the fill). `tools/cxdis.py` (run through `tools/cxrun.py`) decodes the EEPROM patch image with the
same instruction table (CPU address = image offset `+ 0x0E91`, the code segment's offset; the
hook table segment is at `+ 0x0E95`, so the tools, which place the whole patch with the code
offset, see the hook table 4 bytes below its address in RAM). Routines we install are
assembled with a small assembler and decoded by both tools, which must agree, then run through
a behavioural model of the ROM paths they touch before any write.

**Ghidra.** A custom SLEIGH language `CX65C02` (derived from Ghidra's 6502 specification, plus
65C02 and the extensions of section 3.1; opaque pcode operations for `CX22`, `CX9B`, `CXAB`,
`7B` and similar) with a headless script that creates the memory blocks, labels the registers and
seeds functions. Ghidra 12.1.4 was used. The decompiler output was a cross-check, not a source of
facts. The language spec is in `tools/ghidra/` (see [tools/README.md](../tools/README.md)); the
headless scripts and the register labels are not published.

**Reading memory safely.** A read-only HID reader: `cxtune dump`, whose request types are a CPU
memory read and an EEPROM read and have no write flag (the research used a Python reader whose
single send function refused any packet that was not a plain read). Reads of at most 32
bytes, with 5 ms between the request and `GET_REPORT`, and a pause between chunks for dumps;
one process at a time (the protocol has no request tag). Dumps were re-read in four places and
compared.

**Measuring.** `examples/trace.rs` polls the playback registers (a cycle of four reads takes
about 36 ms). For pops, the microphone is recorded on the same clock as a register poller, the
signal is high-passed, and the peak of the first 200 ms after each register transition is
compared with the median of the second before it. Playback is digital silence only. The Windows
audio behaviour that matters is in
[firmware-notes.md](firmware-notes.md#the-windows-audio-engine-and-the-output-pin).

**Sibling firmware.** 28 vendor EEPROM patch images for 14 Jabra UC VOICE
150a/250a/550a/750a products (two versions each) and one HP-branded Cloud II image were parsed
with the same tools
(configuration blocks, hook tables, USB descriptors, request tables) and compared with ours. The
30 images (with ours) fall into 15 distinct configuration blocks. Ours (Kingston, `0951:16A4`
rev `0010`) and the HP one (`03F0:0B92` rev `4103`) share one; the two Cloud II patches differ
in identity strings, pointers, a few bytes of the USB configuration descriptor, the patch
version byte (image `0x06CC`), the HID report descriptor (ours has an extra 109-byte
telephony/LED collection with reports `0x08`, `0x09` and `0x17`) and one code immediate
(`MOVI #$B4,$44` against `MOVI #$47,$44` at `$204D`, which looks like the HID report descriptor
length, 180 against 71 bytes). The Jabra
patches use the same ROM hook sites with different assignments, so they are the
same ROM family; their ROM version is unknown. Useful because they expose controls our unit's
descriptor hides (sidetone feature unit 11, default `$1151 = 0x81`).

**Other sources.** fwupd `plugins/synaptics-cxaudio` (LGPL) for the memory protocol and the EEPROM
structures; the Linux `cx2070x` codec driver for register names and the NEWC procedure; the
record order of the Cloud II update file for the write discipline. The strings "invalidating
header" and "validating header" are also in the HP-branded Cloud II (rev `4103`)
`DEVICEFIRMWAREUPGRADE-32.EXE`. The S-record file used in sections 5.1 and 5.3 comes from the
rev `0010` updater, whose `DEVICEFIRMWAREUPGRADE-32.EXE` contains both strings and is
byte-identical to the HP one.

## 8. Open questions, ranked by where help is most valuable

1. **EEPROM boot validation and recovery.** Section 5.2 answers part of it from the code; what
   remains is behaviour: corrupted patch stream, corrupted byte 0, 2, `0x29` or `0x2A`, a size
   code outside `4..8` (a likely boot hang, section 5.2), an interrupted update (section 5.3),
   descriptor selection without a valid patch. It needs a sacrificial unit and is the main obstacle to ever
   offering a persistent (EEPROM) mode.
2. **Sidetone gain unit.** The host-facing unit is a fact of the ROM code (PROVEN): its request
   table serves the feature unit 11 GET_CUR from `$1276` itself (`$EA84`) and MIN/MAX/RES from
   `$0081`, `$0083` and `$0085`, so a host to which feature unit 11 is exposed sees USB audio class 1/256 dB units, 0 to +16 dB
   in 1/256 dB steps (this unit's descriptor does not declare feature unit 11), with default `+3.70 dB` (`0x03B3`). Further evidence: zero page `$81..$88`
   is the same four-word layout (minimum, maximum, resolution, default) as the speaker and
   microphone volume ranges; the Jabra patches set the defaults `0x0A00` (+10 dB) and `0x0600`
   (+6 dB); `$D173` treats a high byte of `0x80` as mute, which is the class's "silence" code.
   What the DSP does with the value (`$112A` to DSP `$2634`) is not measured. The one point for
   the linear Q12 reading (`0x1000` as unity) is a resolution of 1. If the dB reading holds,
   small register values would not mean a quiet sidetone (`0x0001` would be about 0 dB of
   gain) and `0x1000` would be +16 dB, the top of the range. A
   measurement settles it: with sidetone on, a fixed input and the output measured at the
   headphone jack (headset not worn, or a loopback), the level difference between `0x0800` and
   `0x1000` is 6 dB if linear and 8 dB if dB; start at the default and move in small steps.
3. **DSP parameter semantics.** Command and parameter ids to blocks (NR, AGC, sidetone, EQ,
   DRC, loudness, stage enhancer), units of the NR/AGC parameter tables, what the two mailbox
   ports serve.
4. **`$0F1B` role.** Second output driver: `0x00` while playing, `0x33` idle (`$D494` writes
   `0x00`, or `0x88` when `$1014` bit 5 is set; `$D4A1` ORs in `0x33`). Its effect on hiss and
   on the stock-mode behaviour is unmeasured.
5. **Individual bits of `$0F1A`** (`0x33`, `0x77`, `0x00` are the observed values; `0x44` is what
   differs). Which bit powers which stage of the headphone driver, and which is responsible
   for hiss versus pop.
6. **Stage enhancer and `$1152` bit 3.** Who sets it, what it does audibly, and whether the
   `0x08` read is a real state.
7. **Remaining ROM structure.** The 4-byte field at `$FFDC`; the opcode that enters the
   `$FFEC` trap; `CB`/`DB`/`13`/`23`/`33` semantics; the number and behaviour of `PATCH` slots;
   the source of the tick; the `$2073` request records and the `$1E34` handler table.
8. **Other firmware revisions and units.** Please send the `cxtune status` output (section 9) of
   units with a different `Rev`, a different variant (for example the HP-branded `03F0:0B92`),
   or a different ROM version. It prints the `$1001` ROM identity, the mode, hook 30 (`$14B1`),
   the DAC holds (`$16C9`, `$17CA`), the window routine (`$1EFC`) and `$1017`, `$0F18`, `$0F1A`,
   `$0F1B`, `$0F2C`, `$0F3E`, `$1151`, `$1276`, `$00A4`, `$00A8` and `$13DC`; differences in
   those are the useful part. It does not print `$1152` or the other hooks.
9. **Sleep, replug and real-use behaviour** of `cxtune watch` on more hardware (not verified on
   hardware beyond simulated restarts; see README).

## 9. How to contribute safely

**Never share firmware dumps.** Do not attach, paste or link ROM or EEPROM dumps, full
disassembly listings, or vendor update files in issues, pull requests or discussions, and do not
add them to the repository. Why: the firmware is the vendor's copyrighted work; a disclaimer
does not give anyone the right to distribute it; a takedown request can disable the whole
repository, not one file; and emulator and firmware-research projects commonly work the same
way, asking contributors to dump their own device. This is a practical rule, not legal advice.

What is welcome: addresses, short readbacks (a few bytes, as in firmware-notes), observations,
measurements, and our own annotations (address maps, register names, tooling).

**Compare your unit against ours, without exchanging dumps.** Read your own device and compare
SHA-256 values with ours: `cxtune dump <out-dir>` (see [tools/README.md](../tools/README.md))
reads the ROM with flag `0x00` and the EEPROM with flag `0x20` alone, a read, writes `rom.bin` and
`eeprom.bin` into the directory you name, prints their hashes and compares them with the values
below. It never sends `0x20|0x40` (the request builder cannot build it), reads every chunk twice and
compares, refuses to overwrite an existing dump, and uploads nothing. Keep the files to yourself and report only the hashes. Reference values:

| Item | Range read | Size | SHA-256 |
|---|---|---|---|
| ROM | CPU `$7000..$FFFF` | 36864 | `07029dfb4676ce28c1ece6252d45e298a1140b092cbf981b071a30372f08438a` |
| EEPROM | `0x0000..0x3FFF` | 16384 | `f5d9ce77bc35db8c9e9df7f03e3b2eaf1d5f03cb2b9fa01d95fe25301a2b781c` |
| EEPROM from byte 2 to the end of the patch name area | `0x0002..0x1F2C` | 7979 | `20a7e010ccdb0a3221dd09ca200125b8abb4c8e6bb533cde512e00d3a1fad8d3` |
| Patch stream with the name string and the `CX3` tag | `0x05BF..0x1F2C` | 6510 | `b7fe5e12a17804ff84417368d9a66a7c007a4f9fc402419124985e14e4c4b864` |
| Patch stream only | `0x05BF..0x1EE9` | 6443 | `bd386d7e51090be1cdf3b9e3458c64a36fdfbf4169e33ec80fd7ac53f90788fc` |

The ROM was read with ordinary CPU-space reads and four ranges were re-read and compared. The
reference EEPROM hash was read during research with flag `0x20` (EEPROM read; `0x20` without
`0x40` is a read, and an EEPROM write is `0x20|0x40`). `cxtune dump` uses `0x20` for reads
only, and cxtune never sends an EEPROM write. A matching ROM hash plus a matching patch-stream
hash means the same firmware as ours. The free areas, the size-probe bytes, `0x00..0x01`, the serial string and the
config block (the ROM's save request rewrites it) may legitimately differ per unit, so a
full-EEPROM mismatch alone does not mean different firmware; if only those differ, say so in the
report.

**Shareable tooling.** The disassemblers and the Ghidra SLEIGH CPU specification are in
[`tools/`](../tools/README.md). They contain no firmware bytes. The address and register maps are
in [firmware-notes.md](firmware-notes.md).

**Rules for any experiment**

- Read first. Do not write anything until the same value has been read twice and the address
  appears in this document or in firmware-notes.
- One program talks to the dongle at a time. Stop `cxtune watch` before using another tool.
- Never write the EEPROM (flag `0x20|0x40`), DSP memory (`0x80`) or `$040A`, and never write
  outside the writable regions listed in firmware-notes. A replug restores the vendor state; the
  EEPROM has no undo.
- Use digital silence for playback tests. Do not play audible signals; keep headphones or IEMs
  off your ears for any test that could be loud (sidetone in particular).
- If the device stops responding, stop and replug. Do not retry in a loop.
- Do not experiment with EEPROM recovery on the only unit you own.

**Report template** (an issue is fine):

```
Unit: USB ID, product string, purchase region
OS and HID library
cxtune version and the output of `cxtune status` (reads only)
What was done: addresses read or written, in order, with the values
What was observed: values, timings, measurements (method, not recordings of copyrighted audio)
What is not verified
```
