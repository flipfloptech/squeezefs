#!/usr/bin/env bash
# Development aid: every `.stats` counter that moved between two snapshots
# (|Δ| > threshold), as `path Δ`. Used to find a row's ledger terms.
#   statsdiff.sh before.json after.json [threshold]
set -euo pipefail
export BRK_HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
python3 - "$1" "$2" "${3:-0}" <<'EOF'
import json, os, sys
sys.path.insert(0, os.environ["BRK_HERE"])
import brk
a = brk.flatten(json.load(open(sys.argv[1])))[0]
b = brk.flatten(json.load(open(sys.argv[2])))[0]
thr = float(sys.argv[3])
for k in sorted(b):
    if "/buckets/" in k or k.endswith("mean_ns"):
        continue
    va, vb = a.get(k), b.get(k)
    if isinstance(vb, (int, float)) and not isinstance(vb, bool) and isinstance(va, (int, float)) and abs(vb - va) > thr:
        print(k, vb - va)
EOF
