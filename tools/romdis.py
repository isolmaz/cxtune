"""Recursive-descent disassembler for the CX20773 ROM (0x7000-0xFFFF) + EEPROM patch (code from RAM 0x1450).
Static only: reads the two files, never talks to the device.

usage: python tools/romdis.py <rom.bin> <patch_1450.bin> <outdir>
  rom.bin        ROM dump from `cxtune dump` (36864 bytes, CPU $7000..$FFFF)
  patch_1450.bin eeprom.bin bytes 0x05BF..0x1F2C (6510 bytes), placed at $1450 with the offset of
                 the code segment (+0x0E91). The hook table segment is at +0x0E95 in RAM, so
                 here its entries are read from $1453 and are 4 bytes below their RAM addresses.
writes: listing.txt (all decoded code in address order), xrefs.txt (data refs per address),
        calls.txt (call graph), undef.txt (decode problems), jtables.txt, ptrseeds.txt
"""
import collections
import os
import sys

ROM_BASE, PATCH_BASE = 0x7000, 0x1450
rom = open(sys.argv[1], 'rb').read()
patch = open(sys.argv[2], 'rb').read()
OUT = sys.argv[3]
mem = bytearray(0x10000)
known = bytearray(0x10000)
mem[ROM_BASE:ROM_BASE + len(rom)] = rom
known[ROM_BASE:ROM_BASE + len(rom)] = b'\1' * len(rom)
mem[PATCH_BASE:PATCH_BASE + len(patch)] = patch
known[PATCH_BASE:PATCH_BASE + len(patch)] = b'\1' * len(patch)
PATCH_CODE = (0x15DF, 0x2254)  # code part of the patch; the rest is hook table / tables / strings

M = {}
std = """
00 BRK imp;01 ORA izx;05 ORA zp;06 ASL zp;08 PHP imp;09 ORA imm;0a ASL acc;0d ORA abs;0e ASL abs
10 BPL rel;11 ORA izy;12 ORA izp;15 ORA zpx;16 ASL zpx;18 CLC imp;19 ORA aby;1a INC acc;1d ORA abx;1e ASL abx
20 JSR abs;21 AND izx;24 BIT zp;25 AND zp;26 ROL zp;28 PLP imp;29 AND imm;2a ROL acc;2c BIT abs;2d AND abs;2e ROL abs
30 BMI rel;31 AND izy;32 AND izp;34 BIT zpx;35 AND zpx;36 ROL zpx;38 SEC imp;39 AND aby;3a DEC acc;3c BIT abx;3d AND abx;3e ROL abx
40 RTI imp;41 EOR izx;45 EOR zp;46 LSR zp;48 PHA imp;49 EOR imm;4a LSR acc;4c JMP abs;4d EOR abs;4e LSR abs
50 BVC rel;51 EOR izy;52 EOR izp;55 EOR zpx;56 LSR zpx;58 CLI imp;59 EOR aby;5a PHY imp;5d EOR abx;5e LSR abx
60 RTS imp;61 ADC izx;64 STZ zp;65 ADC zp;66 ROR zp;68 PLA imp;69 ADC imm;6a ROR acc;6c JMP ind;6d ADC abs;6e ROR abs
70 BVS rel;71 ADC izy;72 ADC izp;74 STZ zpx;75 ADC zpx;76 ROR zpx;78 SEI imp;79 ADC aby;7a PLY imp;7c JMP iax;7d ADC abx;7e ROR abx
80 BRA rel;81 STA izx;84 STY zp;85 STA zp;86 STX zp;88 DEY imp;89 BIT imm;8a TXA imp;8c STY abs;8d STA abs;8e STX abs
90 BCC rel;91 STA izy;92 STA izp;94 STY zpx;95 STA zpx;96 STX zpy;98 TYA imp;99 STA aby;9a TXS imp;9c STZ abs;9d STA abx;9e STZ abx
a0 LDY imm;a1 LDA izx;a2 LDX imm;a4 LDY zp;a5 LDA zp;a6 LDX zp;a8 TAY imp;a9 LDA imm;aa TAX imp;ac LDY abs;ad LDA abs;ae LDX abs
b0 BCS rel;b1 LDA izy;b4 LDY zpx;b5 LDA zpx;b6 LDX zpy;b8 CLV imp;b9 LDA aby;ba TSX imp;bc LDY abx;bd LDA abx;be LDX aby
c0 CPY imm;c1 CMP izx;c4 CPY zp;c5 CMP zp;c6 DEC zp;c8 INY imp;c9 CMP imm;ca DEX imp;cc CPY abs;cd CMP abs;ce DEC abs
d0 BNE rel;d1 CMP izy;d5 CMP zpx;d6 DEC zpx;d8 CLD imp;d9 CMP aby;da PHX imp;dd CMP abx;de DEC abx
e0 CPX imm;e1 SBC izx;e4 CPX zp;e5 SBC zp;e6 INC zp;e8 INX imp;e9 SBC imm;ea NOP imp;ec CPX abs;ed SBC abs;ee INC abs
f0 BEQ rel;f1 SBC izy;f5 SBC zpx;f6 INC zpx;f8 SED imp;f9 SBC aby;fa PLX imp;fd SBC abx;fe INC abx
04 TSB zp;0c TSB abs;14 TRB zp;1c TRB abs
"""
for item in std.replace('\n', ';').split(';'):
    item = item.strip()
    if item:
        op, mn, mode = item.split()
        M[int(op, 16)] = (mn, mode)
for n in range(8):
    M[0x07 + n * 0x10] = ('RMB%d' % n, 'zp')
    M[0x87 + n * 0x10] = ('SMB%d' % n, 'zp')
    M[0x0f + n * 0x10] = ('BBR%d' % n, 'zprel')
    M[0x8f + n * 0x10] = ('BBS%d' % n, 'zprel')
M.update({
    0xd2: ('SETB', 'mabs'),      # d2 mask lo hi : [abs] |= mask
    0xc2: ('CLRB', 'mabs'),      # c2 mask lo hi : [abs] &= ~mask
    0xe2: ('BRCLR', 'absmrel'),  # e2 lo hi mask rel : branch if ([abs]&mask)==0
    0xf2: ('BRSET', 'absmrel'),  # f2 lo hi mask rel : branch if ([abs]&mask)!=0
    0xb2: ('MOVI', 'immzp'),     # b2 imm zp : [zp] = imm
    0x6b: ('PATCH', 'slot'),     # 6b slot from16 to16
    0x22: ('CX22', 'imp'),       # 1-byte; prologue of ISRs/critical sections (inferred)
    0x32: ('CX32', 'imp'),       # 1-byte; matching epilogue before RTS/RTI (inferred)
    0x13: ('CX13', 'imp'),       # 1-byte; used inside a short delay routine at $dfd9
    0x03: ('CX03', 'imp'),       # 1-byte; ends ISR at $8459 (after CX32), treated as return
    0x23: ('CX23', 'imp'),       # 1-byte; seen in ISR at $717c
    0xab: ('CXAB', 'imp'),
    0x33: ('CX33', 'imp'),       # 1-byte; pairs with CX23 in the ISR at $7176
    0x9b: ('CX9B', 'imp'),       # 1-byte; stack/parameter ops used by the software-interrupt handlers ($7569, $75e1)
    0xbb: ('CXBB', 'imp'),       # 1-byte; loads an interrupt source index into A before JMP (tbl,X)
    0x64: ('ADD', 'zp'),         # A += [zp], no carry in (16-bit add idiom TXA/ADD $40/LDA #0/ADC $41)
    0x89: ('ADD', 'imm'),        # A += imm, no carry in (negate idiom EOR #$ff/ADD #1)
    0x1a: ('NEG', 'acc'),        # A = -A (confirmed by live DSP-volume shadow $1260 = 0x0e for code 0x41)
    0x3a: ('ASR', 'acc'),        # arithmetic shift right (16-bit ASR idiom LDA hi/ASR/STA hi/ROR lo)
    0xcb: ('UNK_CB', 'imp'),
    0xdb: ('UNK_DB', 'imp'),
})
SZ = {'imp': 1, 'acc': 1, 'imm': 2, 'zp': 2, 'zpx': 2, 'zpy': 2, 'izx': 2, 'izy': 2, 'izp': 2, 'rel': 2, 'abs': 3,
      'abx': 3, 'aby': 3, 'ind': 3, 'iax': 3, 'zprel': 3, 'mabs': 4, 'absmrel': 5, 'immzp': 3, 'slot': 6}
WRITES = {'STA', 'STX', 'STY', 'STZ', 'INC', 'DEC', 'ASL', 'LSR', 'ROL', 'ROR', 'TSB', 'TRB', 'SETB', 'CLRB', 'MOVI'} | \
         {'RMB%d' % n for n in range(8)} | {'SMB%d' % n for n in range(8)}


def s8(x):
    return x - 256 if x > 127 else x


def w(a):
    return mem[a] | mem[(a + 1) & 0xFFFF] << 8


DSPK = ['MEMX', 'WRI', 'WRM', 'RDIND', 'RDI', 'RDIND2']  # kinds of the 7b DSP-bridge op (semantics inferred)
DSPL = [6, 4, 6, 2, 4, 2]


def decode_7b(a):
    sub = mem[a + 1]
    if sub & 1 or sub >= 0x18:
        return None
    port, kind = divmod(sub, 0x0c)
    kind //= 2
    n = DSPL[kind]
    t = 'p%d.%s' % (port, DSPK[kind])
    absa = None
    if n == 4:
        t += ' dsp:$%04x' % w(a + 2)
    elif n == 6:
        absa = w(a + 2)
        t += ' $%04x, dsp:$%04x' % (absa, w(a + 4))
    return 'DSP', 'dsp%d' % n, n, t, None, absa


def decode(a):
    op = mem[a]
    if op == 0x7b and known[a]:
        return decode_7b(a)
    if op not in M or not known[a]:
        return None
    mn, mode = M[op]
    n = SZ[mode]
    if a + n > 0x10000:
        return None
    b = mem[a:a + n]
    tgt = absa = None
    t = ''
    if mode == 'acc':
        t = 'A'
    elif mode == 'imm':
        t = '#$%02x' % b[1]
    elif mode in ('zp', 'zpx', 'zpy'):
        t = '$%02x' % b[1] + {'zp': '', 'zpx': ',X', 'zpy': ',Y'}[mode]
        absa = b[1]
    elif mode == 'izx':
        t = '($%02x,X)' % b[1]
    elif mode == 'izy':
        t = '($%02x),Y' % b[1]
    elif mode == 'izp':
        t = '($%02x)' % b[1]
    elif mode == 'rel':
        tgt = (a + 2 + s8(b[1])) & 0xFFFF
        t = '$%04x' % tgt
    elif mode in ('abs', 'abx', 'aby'):
        absa = w(a + 1)
        t = '$%04x' % absa + {'abs': '', 'abx': ',X', 'aby': ',Y'}[mode]
        if mn in ('JSR', 'JMP'):
            tgt = absa
    elif mode == 'ind':
        absa = w(a + 1)
        t = '($%04x)' % absa
    elif mode == 'iax':
        absa = w(a + 1)
        t = '($%04x,X)' % absa
    elif mode == 'zprel':
        tgt = (a + 3 + s8(b[2])) & 0xFFFF
        absa = b[1]
        t = '$%02x,$%04x' % (b[1], tgt)
    elif mode == 'mabs':
        absa = w(a + 2)
        t = '#$%02x,$%04x' % (b[1], absa)
    elif mode == 'absmrel':
        absa = w(a + 1)
        tgt = (a + 5 + s8(b[4])) & 0xFFFF
        t = '$%04x,#$%02x,$%04x' % (absa, b[3], tgt)
    elif mode == 'immzp':
        absa = b[2]
        t = '#$%02x,$%02x' % (b[1], b[2])
    elif mode == 'slot':
        t = '#%d, $%04x -> $%04x' % (b[1], w(a + 2), w(a + 4))
        tgt = w(a + 4)
    return mn, mode, n, t, tgt, absa


ENDS = {'RTS', 'RTI', 'JMP', 'BRA', 'BRK', 'CX03'}


def in_code_area(a):
    return (ROM_BASE <= a <= 0xFFFF) or (PATCH_CODE[0] <= a < PATCH_CODE[1])


code = {}
problems = []
xcalls = []  # (from, to, kind)
jtabs = []
work = []
seed_why = {}


def seed(a, why):
    if in_code_area(a):
        work.append(a)
        seed_why.setdefault(a, why)


for name, va in (('NMI', 0xFFFA), ('RESET', 0xFFFC), ('IRQ', 0xFFFE)):
    seed(w(va), name)
for va in range(0xFFE0, 0xFFFA, 2):  # extended interrupt vector table
    seed(w(va), 'VEC_%04x' % va)
# patch hook table targets (3-byte JMP entries)
for k, a in enumerate(range(0x1453, 0x155B, 3)):
    if mem[a] == 0x4C:
        seed(w(a + 1), 'hook%02d' % k)
# code pointers that the patch stores into RAM callback slots
for p in (0x161D, 0x181F, 0x18DE, 0x197E, 0x199B, 0x1A24, 0x1A5E, 0x1AAC, 0x1AA0, 0x1C27, 0x1C4F, 0x1E04,
          0x18B7, 0x1C17, 0x1C3F, 0x18CE, 0x198C, 0x1A53, 0x1A8B, 0x20A2, 0x208C, 0x20B9, 0x1E21, 0x1E25, 0x1E29,
          0x1D9C, 0x1DB1, 0x1DC6, 0x1DCA, 0x1D87, 0x1D72, 0x1DCE, 0x1DD2, 0x1DD6):
    seed(p, 'patchptr')
extra = [int(x, 16) for x in os.environ.get('EXTRA_SEEDS', '').split()]
for x in extra:
    seed(x, 'extra')


def run():
    while work:
        a = work.pop()
        while in_code_area(a):
            if a in code:
                break
            r = decode(a)
            if r is None:
                problems.append((a, 'undef op %02x' % mem[a]))
                break
            code[a] = r
            mn, mode, n, t, tgt, absa = r
            for k in range(1, n):
                if a + k in code:
                    problems.append((a, 'overlap'))
            if tgt is not None and not (mn == 'JMP' and mode != 'abs'):
                kind = mn if mn in ('JSR', 'JMP', 'PATCH') else 'BR'
                xcalls.append((a, tgt, kind))
                seed(tgt, kind)
            if mn == 'JMP' and mode == 'iax':
                jtabs.append(a)
            if mn in ENDS:
                break
            a += n


run()
# JMP ($tbl,X) tables: take consecutive words that point into the ROM / patch code area
for j in list(jtabs):
    tbl = code[j][5]
    ents = []
    for k in range(64):
        e = w(tbl + 2 * k)
        if not known[tbl + 2 * k] or not in_code_area(e) or tbl <= e < tbl + 2 * k + 2 or 0xF000 <= e < 0xF200:
            break
        if k and (tbl + 2 * k) in code:
            break
        if ents and abs(e - ents[-1]) > 0x800:
            break
        ents.append(e)
    jtabs.append((j, tbl, ents))
    for e in ents:
        xcalls.append((j, e, 'JTAB'))
        seed(e, 'jtab_%04x' % tbl)
run()
jtabs = [x for x in jtabs if isinstance(x, tuple)]
# code pointers built with LDA #lo / STA p ... LDA #hi / STA p+1 into known code-vector slots:
# operands of JMP (abs) and the address fields of RAM trampolines (JSR/JMP to a RAM address holding 4C lo hi)
ptr_seeds = {}
for _ in range(4):
    slots = set()
    for a, r in code.items():
        if r[0] == 'JMP' and r[1] == 'ind':
            slots.add(r[5])
        if r[0] in ('JSR', 'JMP') and r[1] == 'abs' and r[4] is not None and r[4] < ROM_BASE and not in_code_area(r[4]):
            slots.add(r[4] + 1)
    lo_st = {}
    hi_st = {}
    order = sorted(code)
    for i, a in enumerate(order[1:], 1):
        p = order[i - 1]
        mn, mode, n, t, tgt, absa = code[a]
        pm, pmode = code[p][0], code[p][1]
        if mn == 'STA' and mode in ('abs', 'zp') and pm == 'LDA' and pmode == 'imm' and p + code[p][2] == a:
            v = mem[p + 1]
            lo_st.setdefault(absa, []).append((a, v))
            hi_st.setdefault(absa - 1, []).append((a, v))
        if mn == 'MOVI':
            lo_st.setdefault(absa, []).append((a, mem[a + 1]))
            hi_st.setdefault(absa - 1, []).append((a, mem[a + 1]))
    new = 0
    for dst in slots:
        for a_lo, v_lo in lo_st.get(dst, []):
            cand = sorted((abs(a_hi - a_lo), a_hi, v_hi) for a_hi, v_hi in hi_st.get(dst, []) if abs(a_hi - a_lo) < 16)
            for _, a_hi, v_hi in cand[:1]:
                    t = v_lo | v_hi << 8
                    if in_code_area(t) and t not in code and t not in ptr_seeds and t not in (0x2073, 0xFFFD):  # data pointers
                        ptr_seeds[t] = (dst, a_lo)
                        xcalls.append((a_lo, t, 'PTR'))
                        seed(t, 'ptr@%04x' % dst)
                        new += 1
    run()
    if not new:
        break
jtabs = [x for x in jtabs if isinstance(x, tuple)]
callers = collections.defaultdict(list)
for f, t, k in xcalls:
    callers[t].append((f, k))
subs = {t for f, t, k in xcalls if k in ('JSR', 'JTAB', 'PATCH', 'PTR')} | set(seed_why)

os.makedirs(OUT, exist_ok=True)
with open(os.path.join(OUT, 'listing.txt'), 'w') as fo:
    prev = None
    for a in sorted(code):
        mn, mode, n, t, tgt, absa = code[a]
        if prev is not None and a != prev:
            fo.write('        ; ---- gap %04x-%04x ----\n' % (prev, a - 1))
        if a in subs:
            cs = ' '.join('%04x%s' % (f, '' if k == 'JSR' else ':' + k) for f, k in callers.get(a, []) if k != 'BR')
            fo.write('\n; ===== sub_%04x %s callers: %s\n' % (a, seed_why.get(a, ''), cs))
        lbl = 'L%04x:' % a if a in callers else ''
        raw = ' '.join('%02x' % x for x in mem[a:a + n])
        fo.write('%-7s %04x  %-17s %-6s %s\n' % (lbl, a, raw, mn, t))
        prev = a + n
with open(os.path.join(OUT, 'xrefs.txt'), 'w') as fo:
    acc = collections.defaultdict(list)
    for a in sorted(code):
        mn, mode, n, t, tgt, absa = code[a]
        if absa is None or mn in ('JSR', 'JMP') and mode == 'abs':
            continue
        key = absa if mode not in ('zp', 'zpx', 'zpy', 'zprel', 'immzp') else absa  # zp shown as $00xx
        acc[key].append('%04x:%s%s %s' % (a, 'W:' if mn in WRITES else '', mn, t))
    for k in sorted(acc):
        fo.write('$%04x | %s\n' % (k, ' ; '.join(acc[k])))
with open(os.path.join(OUT, 'calls.txt'), 'w') as fo:
    for t in sorted(callers):
        fo.write('$%04x <- %s\n' % (t, ' '.join('%04x:%s' % (f, k) for f, k in callers[t] if k != 'BR')))
with open(os.path.join(OUT, 'undef.txt'), 'w') as fo:
    for a, why in sorted(set(problems)):
        fo.write('%04x %s\n' % (a, why))
with open(os.path.join(OUT, 'jtables.txt'), 'w') as fo:
    for j, tbl, ents in jtabs:
        fo.write('%04x JMP ($%04x,X): %s\n' % (j, tbl, ' '.join('%04x' % e for e in ents)))
cov = sum(code[a][2] for a in code if a >= ROM_BASE)
with open(os.path.join(OUT, 'ptrseeds.txt'), 'w') as fo:
    for t, (dst, a) in sorted(ptr_seeds.items()):
        fo.write('%04x stored to $%04x at %04x\n' % (t, dst, a))
print('insns', len(code), 'rom bytes covered', cov, 'of', len(rom), 'problems', len(set(problems)), 'jtabs', len(jtabs))
