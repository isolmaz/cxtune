"""Linear-sweep disassembly of [start, end) with the romdis.py decoder. May be out of step in
data areas. Static: reads the two files only.

usage: python tools/romlin.py <rom.bin> <patch_1450.bin> <start_hex> <end_hex>
(the first two arguments are those of romdis.py)
"""
import os
import sys

HERE = os.path.dirname(os.path.abspath(__file__))
src = open(os.path.join(HERE, 'romdis.py')).read().split('\nENDS = ')[0]
rom_path, patch_path, start, end = sys.argv[1:5]
sys.argv = ['romdis', rom_path, patch_path, '']
exec(src)
a, end = int(start, 16), int(end, 16)
while a < end:
    r = decode(a)
    if r is None:
        print('%04x  %02x                .byte' % (a, mem[a]))
        a += 1
        continue
    mn, mode, n, t, tgt, absa = r
    print('%04x  %-17s %-6s %s' % (a, ' '.join('%02x' % x for x in mem[a:a + n]), mn, t))
    a += n
