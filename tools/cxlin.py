"""Linear-sweep disassembly of the EEPROM patch from CPU address <start> up to <end>, using the
decoder in cxdis.py. May be out of step in data areas. Static: reads the EEPROM dump only.

usage: python tools/cxlin.py <eeprom.bin> <start_hex> <end_hex>
"""
import sys, os
a0, a1 = int(sys.argv[2], 16), int(sys.argv[3], 16)
sys.argv = sys.argv[:2]
exec(open(os.path.join(os.path.dirname(os.path.abspath(__file__)), 'cxdis.py')).read())
c = a0
while c < a1:
    i = img(c); r = decode(i)
    if r is None:
        print('%04x  %02x        .byte' % (c, d[i])); c += 1; continue
    mn, md, n, t, tgt, absa = r
    print('%04x  %-17s %-5s %s' % (c, ' '.join('%02x' % x for x in d[i:i+n]), mn, t)); c += n
