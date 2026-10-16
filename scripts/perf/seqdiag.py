#!/usr/bin/env python3
# seqdiag.py <dir> — seqdiag.sh's per-row decomposition: fio, the daemon's write latency, block-lock waits by
# site, device legs, the daemon's CPU by thread class and the handler lanes' syscall shares.
import collections, glob, json, os, re, sys

D = sys.argv[1]
SITES = ["write_checkout", "pipeline_upload", "writeback_flush", "fold", "overlay_prune", "settle", "fsync", "read"]


def flat(d, p=""):
    o = {}
    if isinstance(d, dict):
        if "count" in d and "sum_ns" in d:
            o[p] = (d["count"], d["sum_ns"])
            return o
        for k, v in d.items():
            o.update(flat(v, p + "." + k if p else k))
    elif isinstance(d, list):
        for i, v in enumerate(d):
            o.update(flat(v, "%s[%d]" % (p, i)))
    else:
        o[p] = d
    return o


def tasks(f):
    t = {}
    for line in open(f):
        tid, rest = line.split(" ", 1)
        comm, ticks = rest.rsplit(" ", 1)
        t[tid] = (comm, int(ticks))
    return t


for js in sorted(glob.glob(os.path.join(D, "*.json"))):
    if js.endswith(".stats0.json") or js.endswith(".stats1.json"):
        continue
    n = os.path.basename(js)[:-5]
    j = json.load(open(js))["jobs"][0]["write"]
    a = flat(json.load(open(os.path.join(D, n + ".stats0.json"))))
    b = flat(json.load(open(os.path.join(D, n + ".stats1.json"))))

    def h(k):
        x, y = a.get(k, (0, 0)), b.get(k, (0, 0))
        return y[0] - x[0], y[1] - x[1]

    def c(k):
        return b.get(k, 0) - a.get(k, 0)

    nw, sw = h("ops.fuse_op_latency_ns.write")
    print("== %s  %.0f MiB/s  fio clat %.0f us  daemon write %.0f us (n=%d)" % (n, j["bw_bytes"] / 2**20, j["clat_ns"]["mean"] / 1e3, sw / max(nw, 1) / 1e3, nw))
    for i, s in enumerate(SITES):
        cn, t = h("data.write_seams.block_lock_wait[%d]" % i)
        if cn:
            print("  block_lock_wait %-16s n=%7d mean %7.0f us  per write %7.0f us" % (s, cn, t / cn / 1e3, t / max(nw, 1) / 1e3))
    for k in ("io.nvme_derive.write_pipeline_phase_ns.dev_queue", "io.nvme_derive.write_pipeline_phase_ns.dev_service", "fuse.op_guard.write_lock_wait_shared", "meta.dlm.lock_phase_dlm_guard_wait"):
        cn, t = h(k)
        if cn:
            print("  %-55s n=%7d mean %7.0f us" % (k, cn, t / cn / 1e3))
    print("  block_lock acquires %d contended %d  overlay stores %d  extractions %d" % (c("data.write_seams.block_lock_acquires"), c("data.write_seams.block_lock_contended"), c("data.families.write.overlay.overlay_stores"), c("transport.transport_zc_write_extractions")))
    t0f, t1f = os.path.join(D, n + ".t0"), os.path.join(D, n + ".t1")
    if os.path.exists(t1f):
        wall = float(open(os.path.join(D, n + ".wall")).read())
        hz = os.sysconf("SC_CLK_TCK")
        x, y = tasks(t0f), tasks(t1f)
        cls = collections.defaultdict(lambda: [0.0, 0.0, 0])
        for tid, (comm, tk) in y.items():
            cc = (tk - x.get(tid, (comm, 0))[1]) / hz / wall
            kk = re.sub(r"\d+$", "", comm)
            cls[kk][0] += cc
            cls[kk][1] = max(cls[kk][1], cc)
            cls[kk][2] += 1
        tot = sum(v[0] for v in cls.values())
        print("  daemon %.2f cores: " % tot + " ".join("%s×%d:%.2f(max %.0f%%)" % (k, v[2], v[0], 100 * v[1]) for k, v in sorted(cls.items(), key=lambda kv: -kv[1][0])[:7]))
    sy = os.path.join(D, n + ".sys")
    if os.path.exists(sy):
        cnt = collections.Counter()
        for line in open(sy):
            k, _, v = line.split()
            cnt[{"202": "futex", "426": "io_uring_enter"}.get(v, v)] += int(k)
        tot = sum(cnt.values()) or 1
        print("  lanes: " + " ".join("%s %.0f%%" % (k, 100 * v / tot) for k, v in cnt.most_common(4)))
