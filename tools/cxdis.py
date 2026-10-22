"""Decoder for the CX2077x EEPROM patch (65C02 + Rockwell bit ops + Conexant custom ops).

Not run by itself: cxrun.py and cxlin.py load it. It reads the EEPROM dump given as their first
argument (eeprom.bin from `cxtune dump`); static, never talks to the device.
CPU address = image offset + DELTA, where DELTA follows from the patch pointer in the EEPROM header.
"""
import sys, struct, collections
IMG = sys.argv[1]
d = open(IMG, 'rb').read()
# The code segment of the patch (EEPROM pointer at 0x15) is assumed to load at CPU $1450, as in the Rev 0010 image (0x5BF + 0xE91).
# The hook table segment is 4 bytes higher in RAM; hooks are read from $1453 here (cxrun.py).
PATCH = d[0x15] | d[0x16] << 8
DELTA = 0x1450 - PATCH
LO, HI = PATCH + 0x18F, d[0x20] | d[0x21] << 8   # code region; HI = version string address
M = {}
def add(op, mn, mode):
    M[op] = (mn, mode)
# standard 6502/65C02 table
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
b0 BCS rel;b1 LDA izy;b2 LDA izp;b4 LDY zpx;b5 LDA zpx;b6 LDX zpy;b8 CLV imp;b9 LDA aby;ba TSX imp;bc LDY abx;bd LDA abx;be LDX aby
c0 CPY imm;c1 CMP izx;c4 CPY zp;c5 CMP zp;c6 DEC zp;c8 INY imp;c9 CMP imm;ca DEX imp;cc CPY abs;cd CMP abs;ce DEC abs
d0 BNE rel;d1 CMP izy;d2 CMP izp;d5 CMP zpx;d6 DEC zpx;d8 CLD imp;d9 CMP aby;da PHX imp;dd CMP abx;de DEC abx
e0 CPX imm;e1 SBC izx;e4 CPX zp;e5 SBC zp;e6 INC zp;e8 INX imp;e9 SBC imm;ea NOP imp;ec CPX abs;ed SBC abs;ee INC abs
f0 BEQ rel;f1 SBC izy;f2 SBC izp;f5 SBC zpx;f6 INC zpx;f8 SED imp;f9 SBC aby;fa PLX imp;fd SBC abx;fe INC abx
04 TSB zp;0c TSB abs;14 TRB zp;1c TRB abs
"""
for item in std.replace('\n', ';').split(';'):
    item = item.strip()
    if not item: continue
    op, mn, mode = item.split()
    add(int(op, 16), mn, mode)
for n in range(8):
    add(0x07 + n*0x10, 'RMB%d' % n, 'zp'); add(0x87 + n*0x10, 'SMB%d' % n, 'zp')
    add(0x0f + n*0x10, 'BBR%d' % n, 'zprel'); add(0x8f + n*0x10, 'BBS%d' % n, 'zprel')
# Conexant custom ops (hypotheses, overriding standard meanings); set via CUSTOM below
CUSTOM = {
    0xd2: ('SETB', 'mabs'),     # d2 mask lo hi : [abs] |= mask
    0xc2: ('CLRB', 'mabs'),     # c2 mask lo hi : [abs] &= ~mask
    0xe2: ('BRCLR', 'absmrel'), # e2 lo hi mask rel : branch if ([abs]&mask)==0 (polarity inferred from busy-wait loops)
    0xf2: ('BRSET', 'absmrel'), # f2 lo hi mask rel : branch if ([abs]&mask)!=0
    0xb2: ('MOVI', 'immzp'),    # b2 imm zp : [zp] = imm
    0x6b: ('PATCH', 'slot'),    # 6b slot from16 to16 : ROM patch slot (inferred: redirect ROM PC 'from' -> 'to')
    0xcb: ('UNK_CB', 'imp'),    # unknown 1-byte op, seen only directly after PATCH #1 at $2016
    0xdb: ('UNK_DB', 'imp'),    # unknown 1-byte op, seen only directly after PATCH #11 at $201d
}
M.update(CUSTOM)
SZ = {'imp':1,'acc':1,'imm':2,'zp':2,'zpx':2,'zpy':2,'izx':2,'izy':2,'izp':2,'rel':2,'abs':3,'abx':3,'aby':3,'ind':3,'iax':3,'zprel':3,'mabs':4,'mabsrel':5,'absmrel':5,'immzp':3,'slot':6,'imm2':3,'zpimm':3}
def cpu(i): return i + DELTA
def img(c): return c - DELTA
def s8(x): return x - 256 if x > 127 else x
def decode(i):
    op = d[i]
    if op not in M: return None
    mn, mode = M[op]; n = SZ[mode]; b = d[i:i+n]
    w = lambda k: b[k] | b[k+1] << 8
    tgt = None; absa = None
    if mode in ('imp',): t = ''
    elif mode == 'acc': t = 'A'
    elif mode == 'imm': t = '#$%02x' % b[1]
    elif mode == 'zp': t = '$%02x' % b[1]; absa = b[1]
    elif mode == 'zpx': t = '$%02x,X' % b[1]; absa = b[1]
    elif mode == 'zpy': t = '$%02x,Y' % b[1]; absa = b[1]
    elif mode == 'izx': t = '($%02x,X)' % b[1]
    elif mode == 'izy': t = '($%02x),Y' % b[1]
    elif mode == 'izp': t = '($%02x)' % b[1]
    elif mode == 'rel': tgt = cpu(i) + 2 + s8(b[1]); t = '$%04x' % tgt
    elif mode in ('abs','abx','aby'):
        absa = w(1); t = '$%04x' % absa + {'abs':'','abx':',X','aby':',Y'}[mode]
        if mn in ('JSR','JMP'): tgt = absa
    elif mode == 'ind': t = '($%04x)' % w(1); absa = w(1)
    elif mode == 'iax': t = '($%04x,X)' % w(1); absa = w(1)
    elif mode == 'zprel': tgt = cpu(i) + 3 + s8(b[2]); t = '$%02x,$%04x' % (b[1], tgt); absa = b[1]
    elif mode == 'mabs': absa = w(2); t = '#$%02x,$%04x' % (b[1], absa)
    elif mode == 'mabsrel': absa = w(2); tgt = cpu(i) + 5 + s8(b[4]); t = '#$%02x,$%04x,$%04x' % (b[1], absa, tgt)
    elif mode == 'absmrel': absa = w(1); tgt = cpu(i) + 5 + s8(b[4]); t = '$%04x,#$%02x,$%04x' % (absa, b[3], tgt)
    elif mode == 'immzp': absa = b[2]; t = '#$%02x,$%02x' % (b[1], b[2])
    elif mode == 'slot': t = '#%d, $%04x -> $%04x' % (b[1], w(2), w(4)); tgt = w(4)
    elif mode == 'imm2': t = '#$%02x,$%02x' % (b[1], b[2])
    elif mode == 'zpimm': t = '#$%02x,$%02x' % (b[1], b[2])
    return mn, mode, n, t, tgt, absa
ENDS = {'RTS','RTI','JMP','BRA'}
def flow(i):
    r = decode(i)
    mn, mode, n, t, tgt, absa = r
    cont = mn not in ENDS
    tg = [tgt] if tgt is not None else []
    if mn == 'JMP' and mode != 'abs': tg = []
    return cont, tg
