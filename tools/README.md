# Research tools

Tools for reading the firmware of the CX20773 in the HyperX Cloud II USB dongle with your own
unit. They contain no firmware: you produce the inputs from your own device and the outputs stay on
your machine. Background and open questions are in
[docs/firmware-research.md](../docs/firmware-research.md).

**Never commit or share the dump or anything generated from it** (ROM and EEPROM files, listings,
decompiler output, vendor update files). Share addresses, short readbacks, observations and your
own annotations instead (section 9 of the research document).

## 1. Dump your device

Close the tray app and `cxtune watch` (one program at a time may talk to the dongle), then:

```powershell
cxtune dump C:\path\to\out
```

This reads the ROM (CPU `$7000..$FFFF`, flag `0x00`) and the EEPROM (`0x0000..0x3FFF`, flag `0x20`
alone, a read) in 32-byte chunks, takes about 15 seconds, writes `rom.bin` (36864 bytes) and
`eeprom.bin` (16384 bytes) and prints their SHA-256 values next to the reference unit's. The
command cannot send a write: the request types it uses have no write flag. It uploads nothing.

If it prints `same firmware as reference`, the ROM and the patch stream are those the research
document describes. If it prints `different from the reference firmware`, open an issue with the
printed hashes (not the files).

## 2. Disassemble the ROM and the patch

Python 3, standard library only. The tools read files and never touch the device.

`romdis.py` takes the ROM and the patch as loaded into RAM at `$1450`, which is EEPROM bytes
`0x05BF..0x1F2C`:

```powershell
python -c "d = open(r'C:\path\to\out\eeprom.bin', 'rb').read(); open(r'C:\path\to\out\patch_1450.bin', 'wb').write(d[0x05BF:0x1F2D])"
python tools\romdis.py C:\path\to\out\rom.bin C:\path\to\out\patch_1450.bin C:\path\to\listing
python tools\romlin.py C:\path\to\out\rom.bin C:\path\to\out\patch_1450.bin f000 f100
```

- `romdis.py` writes `listing.txt` (all decoded code), `xrefs.txt` (data references per address),
  `calls.txt` (call graph), `jtables.txt`, `ptrseeds.txt` and `undef.txt` into the output directory.
  On the reference firmware it decodes about 11,000 instructions with no decode problems.
- `romlin.py` prints a linear sweep of an address range, which also shows the data between the
  routines.

`cxrun.py` and `cxlin.py` disassemble only the EEPROM patch, with the decoder in `cxdis.py` (CPU
address = EEPROM offset + `0x0E91`). They take `eeprom.bin` directly:

```powershell
python tools\cxrun.py C:\path\to\out\eeprom.bin dis
python tools\cxrun.py C:\path\to\out\eeprom.bin regs
python tools\cxlin.py C:\path\to\out\eeprom.bin 1450 1500
```

The instruction table (65C02 plus Rockwell bit operations plus the Conexant additions) is
described in section 3.1 of the research document. The seed addresses in `romdis.py` and
`cxrun.py` are those of ROM `CATP_V00.15.00.00` with patch rev `0010`; another firmware needs its
own seeds.

## 3. Ghidra

`ghidra/CX65C02/` is a Ghidra processor module with the language `CX65C02`: the 6502
specification of Ghidra plus the 65C02 instructions and the Conexant additions. It is the `.slaspec`
and `.ldefs` of the language only. It was compiled with Ghidra 12.1.4; the decompiler output is a
cross-check, not a source of facts.

1. Copy `tools\ghidra\CX65C02` to `<ghidra>\Ghidra\Processors\CX65C02`.
2. Copy `6502.slaspec`, `6502.pspec` and `6502.cspec` from
   `<ghidra>\Ghidra\Processors\6502\data\languages` into
   `<ghidra>\Ghidra\Processors\CX65C02\data\languages` (`cx65c02.slaspec` includes the first and the
   language definition uses the other two; they are Ghidra's files and are not repeated here).
3. Restart Ghidra. It compiles `cx65c02.slaspec` into `cx65c02.sla` when it loads the language; to
   compile by hand, run `<ghidra>\support\sleigh.bat -a <ghidra>\Ghidra\Processors\CX65C02\data\languages`.
4. Import `rom.bin` as a raw binary with language `CX65C02:LE:16:default`, base address `7000`, and
   add the patch (`patch_1450.bin`) as a second block at `1450`. The function seeds that
   `python tools\cxrun.py <eeprom.bin> seeds` prints are the entry points of the patch.
