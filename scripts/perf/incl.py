#!/usr/bin/env python3
# incl.py [comm-prefix] [top] < perf-script-output — inclusive and self % per symbol from `perf script` (with -g).
import sys, re, collections
pref = sys.argv[1] if len(sys.argv) > 1 else ""
top = int(sys.argv[2]) if len(sys.argv) > 2 else 60
incl = collections.Counter(); self_ = collections.Counter(); n = 0
cur = None; frames = []
def flush():
    global n
    if cur is None: return
    if pref and not cur.startswith(pref): return
    n += 1
    seen = set()
    for i, f in enumerate(frames):
        if i == 0: self_[f] += 1
        if f not in seen:
            incl[f] += 1; seen.add(f)
for line in sys.stdin:
    if not line.strip():
        flush(); cur = None; frames = []; continue
    if not line.startswith((' ', '\t')):
        cur = line.split()[0]; frames = []; continue
    parts = line.strip().split(None, 1)
    if len(parts) < 2: continue
    sym = parts[1]
    sym = re.sub(r'\+0x[0-9a-f]+', '', sym)
    sym = re.sub(r' \(.*\)$', '', sym)
    if sym.startswith('[unknown]'): sym = '[k]' if parts[0].startswith('ffff') else sym
    frames.append(sym)
flush()
print("samples", n)
for s, c in incl.most_common(top):
    print("%6.2f%% incl %6.2f%% self  %s" % (100.0 * c / n, 100.0 * self_[s] / n, s))
