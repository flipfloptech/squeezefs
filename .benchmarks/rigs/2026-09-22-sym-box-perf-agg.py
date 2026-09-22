#!/usr/bin/env python3
"""The perf-phases rig's aggregators (2026-09-22-sym-box-perf-phases.sh).

    2026-09-22-sym-box-perf-agg.py flat    <perf.data> <comm-prefix> [top]
    2026-09-22-sym-box-perf-agg.py callers <perf.data> <comm-prefix> <leaf-substr> [depth]

Both read `perf script -F comm,ip,sym` (one sample = a comm line followed by
its chain, innermost first) and keep the samples whose comm STARTS WITH the
prefix — `perf report --comm` matches a comm EXACTLY, and the handler lanes
are `fuse3-tpc0`, `fuse3-tpc1`, … (the rig's first build filtered on
`fuse3-tpc` and wrote an empty table on every leg).

`flat`: the leaf symbols of the prefix's samples, most frequent first, with
the crate hashes (`Cs<hash>_`) folded so two binaries' tables line up.
`callers`: for samples whose LEAF contains `leaf-substr`, the caller chain
(the next `depth` frames, innermost first) counted — the question a
DWARF-unwound leg answers (who calls glibc's memmove/memcpy, which carries
no frame pointer). Runs on the box's Python 3.6.
"""
import collections
import re
import subprocess
import sys

CRATES = ("9squeezefs", "5fuse3", "3scc", "8arc_swap", "15futures_channel", "12futures_util",
          "4core", "3std", "4moka", "5alloc", "__mem", "_mem", "13tikv", "7_rjem", "5ahash",
          "12squeezefs_ipc", "15crossbeam_epoch")


def norm(sym):
    sym = re.sub(r"Cs[0-9A-Za-z]{9,14}_", "", sym)
    sym = re.sub(r"\.llvm\.\d+", "", sym)
    idx = min([sym.find(c) for c in CRATES if sym.find(c) >= 0] or [0])
    sym = sym[idx:]
    sym = re.sub(r"B[0-9a-z]*_", "", sym)
    return sym[:110]


def samples(data):
    p = subprocess.run(["perf", "script", "-i", data, "-F", "comm,ip,sym"],
                       stdout=subprocess.PIPE, stderr=subprocess.DEVNULL, universal_newlines=True)
    comm, chain = None, []
    for line in p.stdout.splitlines():
        if not line.strip():
            if comm is not None:
                yield comm, chain
            comm, chain = None, []
            continue
        parts = line.split()
        if not line.startswith((" ", "\t")):
            if comm is not None:
                yield comm, chain
            comm = parts[0]
            chain = [parts[2]] if len(parts) >= 3 else []
        elif len(parts) >= 2:
            chain.append(parts[1])
    if comm is not None:
        yield comm, chain


def main():
    mode, data, prefix = sys.argv[1], sys.argv[2], sys.argv[3]
    total = kept = 0
    if mode == "flat":
        top = int(sys.argv[4]) if len(sys.argv) > 4 else 400
        cnt = collections.Counter()
        for comm, chain in samples(data):
            total += 1
            if comm.startswith(prefix):
                kept += 1
                cnt[norm(chain[0]) if chain else "[unknown]"] += 1
        print("samples total %d, %s* %d" % (total, prefix, kept))
        for sym, c in cnt.most_common(top):
            print("%6.2f%% %6d  %s" % (100.0 * c / max(1, kept), c, sym))
    elif mode == "callers":
        leaf = sys.argv[4]
        depth = int(sys.argv[5]) if len(sys.argv) > 5 else 4
        cnt = collections.Counter()
        n = 0
        for comm, chain in samples(data):
            total += 1
            if not comm.startswith(prefix):
                continue
            kept += 1
            if chain and leaf in chain[0]:
                n += 1
                cnt[" <- ".join(norm(s) for s in chain[1:1 + depth])] += 1
        print("%s* samples %d (of %d); leaf %s: %d (%.2f %%)" % (prefix, kept, total, leaf, n, 100.0 * n / max(1, kept)))
        for k, c in cnt.most_common(30):
            print("%6d  %s" % (c, k))
    else:
        sys.exit("mode flat|callers")


if __name__ == "__main__":
    main()
