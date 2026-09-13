#!/usr/bin/env python3
"""Gate-1 tables for the sym-pr1 solo re-gate rig (`2026-09-13-sym-pr1-solo-regate.sh`).

    2026-09-13-sym-pr1-solo-regate-reduce.py <out-dir>...

Tags are `<arm><pos>-<row>` (`A1-rr4k-kern`, `B2-wfresh-kern`), the timed
mount legs `<arm><pos>.mount.time` / `.remount.time` / `.umount.time`, the
mdstorm legs `<arm><pos>-mdstorm.row`. Arms: A = before PR 1, B = PR 1,
S = B with the forest stamped (SCOPING, compared against B, never against A).

Per row the four A-B-B-A values (positions 1..4), the per-arm MEDIANS, the
B/A ratio of the medians, the NOISE BAND from the two same-arm positions
(max(|A1−A4|/mean(A), |B2−B3|/mean(B))) and a VERDICT: `within noise` iff
|B/A − 1| ≤ max(band, 3 %), else `DELTA` (a single bracket never convicts —
a DELTA row is re-run once before the note's verdict, per the rig header).
The primary metric per row class: IOPS (rand-4k), MiB/s (w_fresh), seconds
(mount / remount / umount — lower is better, the ratio still B/A), ops/s per
phase (mdstorm). Beside it: daemon µs per fio op, box busy %, bw-log
flatness (first vs last third), clat p50/p99, the tripwire sum, and the
gate reads per position (dlm_rpcs absolute, Δfsck_findings,
Δmeta_kv_{journal_entries,checkpoints,node_appends}). Runs on the box's
Python 3.6.
"""
import glob
import json
import os
import re
import statistics
import sys

TRIPWIRES = ("invariant_tripwires", "fuse_op_watchdog_overdue", "transport_cq_overflows",
             "transport_lease_overlong", "write_pipeline_fence_drops", "data_dma_fence_refusals",
             "writeback_errors_latched", "detached_task_panics", "job_worker_panics")
FOREST = ("meta_kv_forest_slot_trees_minted", "meta_kv_forest_root_publishes",
          "meta_kv_forest_key_violations", "meta_kv_forest_reader_window_skips",
          "meta_kv_forest_reader_unpublished_children")
NOISE_FLOOR = 0.03


def load_metrics(path):
    return json.load(open(path))["metrics"]


def delta(s0, s1, k):
    a, b = s0.get(k), s1.get(k)
    if isinstance(a, (int, float)) and isinstance(b, (int, float)):
        return b - a
    return None


def busy(res, label):
    try:
        a = [int(x) for x in open("%s/%s.procstat0" % (res, label)).readline().split()[1:]]
        b = [int(x) for x in open("%s/%s.procstat1" % (res, label)).readline().split()[1:]]
    except OSError:
        return None
    dd = [y - x for x, y in zip(a, b)]
    tot = sum(dd)
    idle = dd[3] + dd[4]
    return 100 * (tot - idle) / tot if tot else None


def flatness(res, label):
    series = {}
    for f in glob.glob("%s/%s_bw.*.log" % (res, label)):
        for line in open(f):
            t, v = line.split(",")[:2]
            t = int(t) // 1000
            series[t] = series.get(t, 0) + int(v)
    ts = sorted(series)
    if len(ts) < 9:
        return None
    n = len(ts) // 3
    a = statistics.mean(series[t] for t in ts[:n])
    c = statistics.mean(series[t] for t in ts[-n:])
    return (c - a) / a * 100 if a else None


def thermal_max(res, label, which):
    best = None
    try:
        for line in open("%s/%s.thermal%s" % (res, label, which)):
            m = re.search(r"=(\d+)$", line.strip())
            if m:
                v = int(m.group(1)) / 1000.0
                best = v if best is None else max(best, v)
    except OSError:
        return None
    return best


def fio_row(res, label):
    s0 = load_metrics("%s/%s.stats0" % (res, label))
    s1 = load_metrics("%s/%s.stats1" % (res, label))
    jobs = json.load(open("%s/%s.fio.json" % (res, label)))["jobs"]
    side = "read" if sum(j["read"]["total_ios"] for j in jobs) else "write"
    ios = sum(j[side]["total_ios"] for j in jobs)

    def pct(j, p):
        return j[side]["clat_ns"]["percentile"][p]

    r = {
        "iops": sum(j[side]["iops"] for j in jobs),
        "bw": sum(j[side]["bw_bytes"] for j in jobs) / 2 ** 20,
        "gib": sum(j[side]["io_bytes"] for j in jobs) / 2 ** 30,
        "p50": statistics.mean(pct(j, "50.000000") for j in jobs) / 1000,
        "p99": max(pct(j, "99.000000") for j in jobs) / 1000,
        "cpu_us": (delta(s0, s1, "daemon_cpu_ns") or 0) / ios / 1000 if ios else None,
        "busy": busy(res, label),
        "flat": flatness(res, label),
        "trip": sum(delta(s0, s1, k) or 0 for k in TRIPWIRES),
        "dlm_rpcs": s1.get("dlm_rpcs"),
        "fsck": delta(s0, s1, "fsck_findings"),
        "jentries": delta(s0, s1, "meta_kv_journal_entries"),
        "ckpts": delta(s0, s1, "meta_kv_checkpoints"),
        "appends": delta(s0, s1, "meta_kv_node_appends"),
        "therm0": thermal_max(res, label, 0),
        "therm1": thermal_max(res, label, 1),
        "forest": {k: s1.get(k) for k in FOREST if k in s1},
    }
    if side == "write":
        r["wt_blocks"] = delta(s0, s1, "write_through_blocks")
        r["patch"] = delta(s0, s1, "patch_writes")
    return r


def time_row(res, tag, label):
    try:
        line = open("%s/%s.%s.time" % (res, tag, label)).read()
    except OSError:
        return None
    r = {}
    for k in ("mount_s", "stats_s", "umount_s"):
        m = re.search(r"\b%s=([0-9.]+)" % k, line)
        if m:
            r[k] = float(m.group(1))
    m = re.search(r"\brc=(\d+)", line)
    if m:
        r["rc"] = int(m.group(1))
    stats = "%s/%s.%s.stats" % (res, tag, label)
    if os.path.exists(stats):
        s = load_metrics(stats)
        r["dlm_rpcs"] = s.get("dlm_rpcs")
        r["forest"] = {k: s.get(k) for k in FOREST if k in s}
    return r


def mdstorm_row(res, tag):
    path = "%s/%s-mdstorm.row" % (res, tag)
    if not os.path.exists(path):
        return None
    phases = {}
    for line in open(path):
        m = re.match(r"^(\w+) ops=(\d+) wall_s=([0-9.]+) ops_s=([0-9.]+)", line.strip())
        if m:
            phases[m.group(1)] = {"ops": int(m.group(2)), "wall": float(m.group(3)), "ops_s": float(m.group(4))}
    r = {"phases": phases}
    post = "%s/%s-mdstorm.post.json" % (res, tag)
    pre = "%s/%s-mdstorm.pre.json" % (res, tag)
    if os.path.exists(post) and os.path.exists(pre):
        s0, s1 = load_metrics(pre), load_metrics(post)
        r["dlm_rpcs"] = s1.get("dlm_rpcs")
        r["jentries"] = delta(s0, s1, "meta_kv_journal_entries")
        r["ckpts"] = delta(s0, s1, "meta_kv_checkpoints")
        r["appends"] = delta(s0, s1, "meta_kv_node_appends")
        r["trip"] = sum(delta(s0, s1, k) or 0 for k in TRIPWIRES)
        r["forest"] = {k: s1.get(k) for k in FOREST if k in s1}
    return r


def fmt(v, f):
    if v is None:
        return "—"
    if isinstance(v, dict):
        return " ".join("%s=%s" % (k.replace("meta_kv_forest_", ""), x) for k, x in sorted(v.items())) or "—"
    return f.format(v)


def verdict(arms, key, base="A", test="B", lower_is_better=False):
    """(median_base, median_test, ratio, band, label) for one metric."""
    xs = [r[key] for _, r in sorted(arms.get(base, [])) if r.get(key) is not None]
    ys = [r[key] for _, r in sorted(arms.get(test, [])) if r.get(key) is not None]
    if not xs or not ys:
        return None
    mx, my = statistics.median(xs), statistics.median(ys)
    band = 0.0
    for vals in (xs, ys):
        if len(vals) >= 2 and statistics.mean(vals):
            band = max(band, abs(max(vals) - min(vals)) / statistics.mean(vals))
    ratio = my / mx if mx else None
    if ratio is None:
        return mx, my, None, band, "n/a"
    dev = abs(ratio - 1)
    label = "within noise" if dev <= max(band, NOISE_FLOOR) else "DELTA"
    if test == "S":
        label = "SCOPING " + ("(within noise of B)" if dev <= max(band, NOISE_FLOOR) else "(delta vs B)")
    return mx, my, ratio, band, label


def positions_line(arms, key, f, letters):
    cells = []
    for arm in letters:
        for pos, r in sorted(arms.get(arm, [])):
            cells.append("%s%d=%s" % (arm, pos, fmt(r.get(key), f)))
    return " ".join(cells)


def print_verdict_table(title, rows, primary, unit, base, test, lower_is_better=False):
    """rows: name -> arms dict; primary: metric key; one line per row."""
    print("\n### %s — %s vs %s (%s)" % (title, test, base, unit))
    print("| row | %s positions | median %s | median %s | %s/%s | noise band | verdict |" % (primary, base, test, test, base))
    print("|---|---|---|---|---|---|---|")
    for name in rows:
        arms = rows[name]
        v = verdict(arms, primary, base, test, lower_is_better)
        if v is None:
            continue
        mx, my, ratio, band, label = v
        print("| %s | %s | %s | %s | %s | %.1f %% | **%s** |" % (
            name, positions_line(arms, primary, "{:,.0f}" if unit != "s" else "{:.3f}", [base, test]),
            fmt(mx, "{:,.0f}" if unit != "s" else "{:.3f}"), fmt(my, "{:,.0f}" if unit != "s" else "{:.3f}"),
            "—" if ratio is None else "%.3f" % ratio, 100 * band, label))


def print_detail_table(title, arms, cols, letters):
    cols = [c for c in cols if any(r.get(c[0]) is not None for a in letters for _, r in arms.get(a, []))]
    print("\n#### %s" % title)
    print("| arm | pos | " + " | ".join(h for _, h, _ in cols) + " |")
    print("|" + "---|" * (len(cols) + 2))
    for arm in letters:
        for pos, r in sorted(arms.get(arm, [])):
            print("| %s | %d | " % (arm, pos) + " | ".join(fmt(r.get(k), f) for k, _, f in cols) + " |")


FIO_COLS = [("iops", "IOPS", "{:,.0f}"), ("bw", "MiB/s", "{:,.0f}"), ("gib", "GiB moved", "{:.1f}"),
            ("p50", "clat p50 µs", "{:.1f}"), ("p99", "clat p99 µs", "{:.0f}"), ("cpu_us", "daemon µs/op", "{:.2f}"),
            ("busy", "box busy %", "{:.1f}"), ("flat", "flat %", "{:+.1f}"), ("therm0", "°C start", "{:.0f}"),
            ("therm1", "°C end", "{:.0f}"), ("trip", "tripwires Δ", "{:.0f}"), ("dlm_rpcs", "dlm_rpcs", "{}"),
            ("fsck", "Δfsck_findings", "{}"), ("jentries", "Δjournal entries", "{:,.0f}"),
            ("ckpts", "Δcheckpoints", "{:,.0f}"), ("appends", "Δnode appends", "{:,.0f}"),
            ("wt_blocks", "write_through", "{:,.0f}"), ("patch", "patch_writes", "{:,.0f}"),
            ("forest", "forest gauges", "{}")]
TIME_COLS = [("mount_s", "mount → return s", "{:.3f}"), ("stats_s", "mount → first .stats s", "{:.3f}"),
             ("umount_s", "clean unmount s", "{:.3f}"), ("rc", "rc", "{}"), ("dlm_rpcs", "dlm_rpcs", "{}"),
             ("forest", "forest gauges", "{}")]
MD_COLS = [("dlm_rpcs", "dlm_rpcs", "{}"), ("trip", "tripwires Δ", "{:.0f}"), ("jentries", "Δjournal entries", "{:,.0f}"),
           ("ckpts", "Δcheckpoints", "{:,.0f}"), ("appends", "Δnode appends", "{:,.0f}"), ("forest", "forest gauges", "{}")]
MD_PHASES = ("mkdir", "create", "stat", "rename", "unlink", "manydirs", "rmdir")


def census_diff(res, letters_present):
    """Keys present in the census of one arm and absent in another (top-level metrics keys +
    one nesting level), with every position's value of the added keys."""
    census = {}
    for f in sorted(glob.glob("%s/*.mount.stats" % res)):
        m = re.match(r"^([A-Z])(\d+)\.mount\.stats$", os.path.basename(f))
        if not m:
            continue
        s = load_metrics(f)
        keys = set()
        for k, v in s.items():
            keys.add(k)
            if isinstance(v, dict):
                for kk in v:
                    keys.add("%s.%s" % (k, kk))
        census.setdefault(m.group(1), []).append((int(m.group(2)), keys, s))
    if "A" not in census or "B" not in census:
        return
    ka = set.union(*[k for _, k, _ in census["A"]])
    kb = set.union(*[k for _, k, _ in census["B"]])
    print("\n### `.stats` field census — B vs A (first mount of each position)")
    print("* keys: A %d, B %d; in B not in A: %d; in A not in B: %d" % (len(ka), len(kb), len(kb - ka), len(ka - kb)))
    for k in sorted(kb - ka):
        vals = []
        for arm in ("B", "S"):
            for pos, _, s in sorted(census.get(arm, [])):
                top = k.split(".")[0]
                v = s.get(top)
                if "." in k and isinstance(v, dict):
                    v = v.get(k.split(".", 1)[1])
                vals.append("%s%d=%s" % (arm, pos, json.dumps(v) if not isinstance(v, (int, float, str)) else v))
        print("  * `%s`: %s" % (k, " ".join(vals)))
    for k in sorted(ka - kb):
        print("  * REMOVED in B: `%s`" % k)


def main():
    runs = sys.argv[1:]
    if not runs:
        sys.exit(__doc__)
    fio = {}     # row -> arm -> [(pos, r)]
    times = {}   # label -> arm -> [(pos, r)]
    md = {}      # arm -> [(pos, r)]
    for res in runs:
        for jf in sorted(glob.glob("%s/*.fio.json" % res)):
            label = os.path.basename(jf)[: -len(".fio.json")]
            m = re.match(r"^([A-Z])(\d+)-(.+)$", label)
            if not m or not os.path.exists("%s/%s.stats1" % (res, label)):
                continue
            fio.setdefault(m.group(3), {}).setdefault(m.group(1), []).append((int(m.group(2)), fio_row(res, label)))
        for tf in sorted(glob.glob("%s/*.time" % res)):
            m = re.match(r"^([A-Z])(\d+)\.(mount|remount|umount)\.time$", os.path.basename(tf))
            if not m:
                continue
            r = time_row(res, "%s%s" % (m.group(1), m.group(2)), m.group(3))
            if r:
                times.setdefault(m.group(3), {}).setdefault(m.group(1), []).append((int(m.group(2)), r))
        for mf in sorted(glob.glob("%s/*-mdstorm.row" % res)):
            m = re.match(r"^([A-Z])(\d+)-mdstorm\.row$", os.path.basename(mf))
            if not m:
                continue
            r = mdstorm_row(res, "%s%s" % (m.group(1), m.group(2)))
            if r:
                md.setdefault(m.group(1), []).append((int(m.group(2)), r))

    letters = sorted({a for d in list(fio.values()) + list(times.values()) + [md] for a in d})
    has_s = "S" in letters
    ab = [a for a in ("A", "B") if a in letters]

    # ---- the gate-1 verdict table (A-B-B-A) ----
    print("## Gate 1 verdict table — B (PR 1) vs A (before PR 1), A-B-B-A on default-format (flat) volumes")
    fio_primary = {"rr4k-kern": ("iops", "IOPS"), "rw4k-kern": ("iops", "IOPS"), "wfresh-kern": ("bw", "MiB/s")}
    rows = {}
    for name in ("wfresh-kern", "rr4k-kern", "rw4k-kern"):
        if name in fio:
            rows[name] = fio[name]
    for name in sorted(fio):
        if name not in rows:
            rows[name] = fio[name]
    for name, arms in rows.items():
        key, unit = fio_primary.get(name, ("iops", "IOPS"))
        print_verdict_table("fio row %s" % name, {name: arms}, key, unit, "A", "B")
    for label in ("mount", "remount", "umount"):
        if label in times:
            key = "umount_s" if label == "umount" else "mount_s"
            print_verdict_table("%s time" % label, {label: times[label]}, key, "s", "A", "B", lower_is_better=True)
            if label != "umount":
                print_verdict_table("%s → first .stats" % label, {label: times[label]}, "stats_s", "s", "A", "B", lower_is_better=True)
    if md:
        md_rows = {}
        for ph in MD_PHASES:
            arms = {}
            for arm, lst in md.items():
                arms[arm] = [(pos, {"ops_s": r["phases"].get(ph, {}).get("ops_s")}) for pos, r in lst]
            md_rows["mdstorm %s" % ph] = arms
        print_verdict_table("mdstorm (file-backed /dev/shm substrate, 8 threads, 100 % scale)", md_rows, "ops_s", "ops/s", "A", "B")

    # ---- the stamped SCOPING leg vs B ----
    if has_s:
        print("\n## SCOPING — S (B with the forest STAMPED, bit 17 via SQUEEZEFS_TEST_STAMP_SYMMETRIC=1 at format) vs B (flat)")
        print("Not the gate: the default format is flat at PR 1; this prices the forest's cost on the same box, one leg against B's two.")
        for name, arms in rows.items():
            key, unit = fio_primary.get(name, ("iops", "IOPS"))
            print_verdict_table("fio row %s" % name, {name: arms}, key, unit, "B", "S")
        for label in ("mount", "remount", "umount"):
            if label in times:
                key = "umount_s" if label == "umount" else "mount_s"
                print_verdict_table("%s time" % label, {label: times[label]}, key, "s", "B", "S", lower_is_better=True)
        if md:
            print_verdict_table("mdstorm", md_rows, "ops_s", "ops/s", "B", "S")

    # ---- detail tables ----
    print("\n## Per-position detail")
    for name, arms in rows.items():
        print_detail_table("fio %s" % name, arms, FIO_COLS, letters)
    for label in ("mount", "remount", "umount"):
        if label in times:
            print_detail_table("%s legs" % label, times[label], TIME_COLS, letters)
    if md:
        print("\n#### mdstorm ops/s per phase")
        print("| arm | pos | " + " | ".join(MD_PHASES) + " | " + " | ".join(h for _, h, _ in MD_COLS) + " |")
        print("|" + "---|" * (len(MD_PHASES) + len(MD_COLS) + 2))
        for arm in letters:
            for pos, r in sorted(md.get(arm, [])):
                print("| %s | %d | " % (arm, pos) + " | ".join(fmt(r["phases"].get(ph, {}).get("ops_s"), "{:,.0f}") for ph in MD_PHASES)
                      + " | " + " | ".join(fmt(r.get(k), f) for k, _, f in MD_COLS) + " |")

    for res in runs:
        census_diff(res, letters)


if __name__ == "__main__":
    main()
