"""Recursive-descent disassembly of the EEPROM patch (rev 0010), using the decoder in cxdis.py.
Static: reads the EEPROM dump only.

usage: python tools/cxrun.py <eeprom.bin> [dis|problems|regs|seeds]
  dis       listing of all decoded code (default)
  problems  decode problems and the gaps in the code region
  regs      cross-references of the data addresses and the calls out of the patch
  seeds     function start addresses with names, for the Ghidra setup

The seed addresses below are those of the rev 0010 patch.
"""
import sys, os, collections
exec(open(os.path.join(os.path.dirname(os.path.abspath(__file__)), 'cxdis.py')).read())
hooks = []
EXTRA = {
 # JMP ($1885,X) dispatch table entries that point into the patch
 0x18B7:'tbl1885', 0x1C17:'tbl1885', 0x1C3F:'tbl1885', 0x18CE:'tbl1885', 0x198C:'tbl1885', 0x1A53:'tbl1885', 0x1A8B:'tbl1885',
 # 8-byte request records at $2073 (handler word at +6)
 0x20A2:'tbl2073', 0x208C:'tbl2073', 0x20B9:'tbl2073',
 # handler words in the table at $1E34
 0x1E21:'tbl1e34', 0x1E25:'tbl1e34', 0x1E29:'tbl1e34', 0x1D9C:'tbl1e34', 0x1DB1:'tbl1e34', 0x1DC6:'tbl1e34', 0x1DCA:'tbl1e34', 0x1D87:'tbl1e34', 0x1D72:'tbl1e34',
 # well-formed code after an RTS with no reference found in the image
 0x210D:'unref', 0x21E4:'unref', 0x1BAA:'unref', 0x2141:'unref', 0x1EFC:'unref', 0x1DCE:'tbl1e34', 0x1DD2:'tbl1e34', 0x1DD6:'tbl1e34',
}
for k, a in enumerate(range(img(0x1453), img(0x155B), 3)):
    if d[a] == 0x4c: hooks.append((k, a, d[a+1] | d[a+2] << 8))
seeds = set(t for _, _, t in hooks) | set(EXTRA)
seeds.add(0x161D)  # JMP target stored into RAM hook at $12EA
for p in (0x181F,0x18DE,0x197E,0x199B,0x1A24,0x1A5E,0x1AAC,0x1AA0,0x1C27,0x1C4F,0x1E04):
    seeds.add(p)  # code pointers stored into RAM callback slots $1205/6, $12E2/3, $15D8/9
code = {}; problems = []; calls = []; work = list(seeds); owner = {}
while work:
    c = work.pop(); i = img(c)
    while LO <= i < HI:
        if cpu(i) in code: break
        r = decode(i)
        if r is None: problems.append((cpu(i), 'undef op %02x' % d[i])); break
        code[cpu(i)] = r
        for k in range(1, r[2]):
            if cpu(i)+k in code: problems.append((cpu(i), 'overlap'))
        cont, tg = flow(i)
        for t in tg:
            calls.append((cpu(i), t, r[0]))
            if LO <= img(t) < HI: work.append(t)
        if not cont: break
        i += r[2]
mode = sys.argv[2] if len(sys.argv) > 2 else 'dis'
if mode == 'problems':
    for p in sorted(set(problems)): print('%04x %s' % p)
    print(len(code), 'insns')
    cov = set()
    for c, r in code.items():
        for k in range(r[2]): cov.add(c+k)
    # print uncovered ranges in code region
    s = None
    for c in range(cpu(LO), cpu(HI)+1):
        if c not in cov and s is None: s = c
        if c in cov and s is not None:
            print('gap %04x-%04x (%d)' % (s, c-1, c-s)); s = None
elif mode == 'dis':
    targets = collections.defaultdict(list)
    for a, t, k in calls: targets[t].append((a, k))
    hookt = {t: k for k, a, t in hooks}
    prev = None
    for c in sorted(code):
        mn, md, n, t, tgt, absa = code[c]
        if prev is not None and c != prev: print('        ; ---- gap ----')
        if c in hookt: print('\n; ===== hook[%d] (image %#x) target =====' % (hookt[c], hooks[[h[2] for h in hooks].index(c)][1]))
        if c in targets and any(k == 'JSR' for _, k in targets[c]): print('; sub_%04x  called from %s' % (c, ' '.join('%04x' % a for a, k in targets[c] if k == 'JSR')))
        lbl = 'L%04x:' % c if c in targets else ''
        raw = ' '.join('%02x' % x for x in d[img(c):img(c)+n])
        note = ''
        if mn in ('JSR', 'JMP') and tgt is not None and not (LO <= img(tgt) < HI): note = '   ; external (ROM/RAM)'
        print('%-7s %04x  %-15s %-5s %s%s' % (lbl, c, raw, mn, t, note))
        prev = c + n
if mode == 'regs':
    acc = collections.defaultdict(list)
    for c in sorted(code):
        mn, md, n, t, tgt, absa = code[c]
        if absa is None or mn in ('JSR', 'JMP'): continue
        if md in ('zp','zpx','zpy','zprel','immzp') : key = 'zp$%02x' % absa
        else: key = '$%04x' % absa
        acc[key].append('%04x:%s %s' % (c, mn, t))
    for k in sorted(acc, key=lambda s: (s.startswith('zp'), s)):
        print(k, '|', ' ; '.join(acc[k]))
    print('\n# external call/jump targets')
    ext = collections.defaultdict(list)
    for a, t, k in calls:
        if not (LO <= img(t) < HI): ext[t].append('%04x:%s' % (a, k))
    for t in sorted(ext): print('$%04x' % t, ' '.join(ext[t]))
if mode == 'seeds':
    out = {}
    for k, a, t in hooks: out[t] = 'hook%02d_img%03x' % (k, a)
    for t, why in EXTRA.items(): out.setdefault(t, '%s_%04x' % (why, t))
    for t in seeds: out.setdefault(t, 'cb_%04x' % t)
    for a, t, k in calls:
        if LO <= img(t) < HI and k in ('JSR', 'PATCH'): out.setdefault(t, ('sub_%04x' if k == 'JSR' else 'patchtgt_%04x') % t)
    for t in sorted(out): print('%04x %s' % (t, out[t]))
