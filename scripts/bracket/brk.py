#!/usr/bin/env python3
"""The bracket harness's arithmetic (charter §11 "Measurement law", §10).

Everything a row records that is not the instrument's own number is computed
here from the product's `.stats` documents and `/proc/diskstats`, so the shell
driver (`bracket.sh`) never does arithmetic:

  venue-id                 the venue identity string (kernel, arch, cpu, cores, disk class)
  build  <squeezefs>       the build identity (`squeezefs --version`) as JSON
  get    <stats.json> KEY  one leaf of a `.stats` document (by basename)
  leg    ...               one leg's record: instrument metrics + the ledgers over
                           the (before, after) `.stats` pair + the diskstats deltas,
                           with the closure verdicts
  fold   ...               the bracket record over the four legs + the markdown row
  summary <venue-dir>      one markdown table over every record in a venue directory
  fio-metrics <fio.json>   the instrument's numbers from fio's JSON output
  mdtest-metrics <out>     the instrument's numbers from mdtest's text output

Keys are looked up by BASENAME in the `.stats` tree (the schema nests families;
the names are unique where it matters — `fuse_ops` is exported twice with one
value). An absent key reads 0 and is listed under `absent_keys` so a renamed
counter can never silently zero a ledger.
"""
import json
import os
import re
import subprocess
import sys

SECTOR = 512

# ---------------------------------------------------------------------------
# The ledger terms (the `.stats` keys a row reads; `src/cli/bench.zig` names
# the device-write arms and the admission arms the same way).
# ---------------------------------------------------------------------------

# The device-write byte arms: every path that lands user bytes on a data
# device ONCE. `rewrite_device_write_bytes` is the rewrite epoch's accounting
# of the SAME overlay DMA (measured: overlay 22.17 GB + rewrite 6.09 GB against
# diskstats 1.025 × user on a 22.28 GB row) and is reported beside, not summed.
DEVICE_WRITE_ARMS = [
    "overlay_store_bytes", "patch_write_bytes", "own_block_dma_bytes",
    "durable_upload_bytes_escalation", "durable_upload_bytes_writeback",
]
DEVICE_WRITE_BESIDE = ["rewrite_device_write_bytes", "rewrite_shadow_bytes", "rewrite_user_bytes"]
# The device-write REQUEST counters beside the byte arms (the stats-side
# `wareq-sz`; the substrate's diskstats is the ruler — these are the daemon's view).
# `put_obj` is the block device layer's put (an overlay store IS a put — not
# summed twice); a W1 patch DMA bypasses it and is counted by `patch_writes`.
DEVICE_WRITE_REQS = ["patch_writes", "own_block_dmas", "put_obj"]
ADMISSION_ARMS = [
    "admit_write_overlay_bytes", "admit_write_inplace_bytes",
    "admit_write_accumulate_bytes", "admit_write_inline_bytes",
]
# The read copy ledger (charter §6.3 `dest + bounce + dest_dma + zc_serve ≈ user
# bytes`): the data plane's arms plus the transport's — on a zero-copy session a
# READ the warm ladder cannot answer is served at the delivery CQE by the direct
# device leg (`READ_FIXED(device fd → slot)`, counted `transport_zc_serve_bytes`)
# and never reaches a handler lane (so `fuse_ops` does not move: the engagement
# term for kernel-FUSE rows is `transport_ring_requests`).
# `read_dest_lease_bytes` is the dest-lease MECHANISM's gauge over the same
# bytes `read_dest_dma_bytes` lands (measured 1:1 on the shim read row) and is
# reported beside, not summed; a shim read's bytes land through the data
# plane's dest DMA into the arena, so `ipc_bytes_out` is not an arm either.
READ_COPY_ARMS = [
    "read_copy_dest_bytes", "read_copy_bounce_bytes", "read_dest_dma_bytes",
    "read_zc_serve_bytes", "read_copy_warm_serve_bytes", "read_copy_hot_serve_bytes",
    "read_copy_hold_serve_bytes", "read_copy_cache_serve_bytes", "read_copy_fill_slice_bytes",
    "transport_zc_serve_bytes", "transport_zc_bounce_bytes_bridged",
]
READ_COPY_BESIDE = ["read_dest_lease_bytes", "ipc_bytes_out", "ipc_read_demote_bytes"]
RECLAIM = ["terminal_frees", "block_free_reclaim_queued", "block_free_reclaim_elided",
           "block_free_discard_bytes", "block_free_punch_bytes", "pack_terminal_frees"]
# The solo-mount wire census: every path that puts a verb on the cluster wire.
# The reference's `dlm_rpcs` has no single twin here; its meaning (a wire RPC a
# solo mount must never issue) is the SUM of these.
WIRE_RPCS = ["cluster_wire_frames_sent", "joined_wire_verbs", "xv_cross_owner_steps_shipped",
             "slot_resolve_rpcs", "xv_cross_owner_guard_rpcs", "dlm_token_grants",
             "slot_ships", "free_shipped_blocks"]
TICK_RESCUES = ["transport_park_tick_commit_rescues", "transport_park_tick_cqe_rescues",
                "journal_lane_park_tick_rescues", "meta_conveyor_park_tick_rescues",
                "block_free_reclaim_park_tick_wakes", "exec_lane_tick_rescues",
                "lock_ticked_reregisters", "channel_ticked_reregisters",
                "write_pipeline_admission_tick_wakes"]
MUST_STAY_0 = ["invariant_tripwires", "tripwire_sites_overflow", "meta_io_unaligned_refusals",
               "meta_io_buffered_fallback", "fuse_replies_synthesized", "fuse_reply_twice",
               "fuse_replies_oversize", "ipc_sessions_poisoned", "journal_lane_write_failures",
               "journal_lane_barrier_failures"]
IPC = ["ipc_ops_read", "ipc_ops_write", "ipc_bytes_in", "ipc_bytes_out", "ipc_sessions_total",
       "ipc_lane_gate_kernel_routes", "ipc_lane_gate_kernel_bytes", "ipc_fast_path_serves",
       "ipc_read_handoffs", "ipc_dd_inline_serves", "ipc_write_handoffs", "ipc_binds_dev_override",
       "ipc_admission_refusals", "ipc_bind_refused_budget", "ipc_shed_refusals", "ipc_binds"]
JOURNAL = ["meta_kv_journal_entries", "meta_kv_journal_pad_entries", "meta_kv_journal_bytes",
           "meta_conveyor_drained_txs", "meta_conveyor_acks", "journal_lane_barriers_strict",
           "journal_lane_barriers_coalesced"]
FUSE = ["fuse_ops", "transport_ring_requests", "transport_ring_reads", "transport_enters", "transport_fast_dispatch_serves",
        "transport_fast_dispatch_demotes", "transport_zc_direct_legs", "transport_zc_write_directs", "transport_payload_leases",
        "transport_wake_writes", "transport_wake_elided", "transport_lease_overlong",
        "transport_requests_abandoned", "transport_cq_overflows"]
MISC = ["put_obj", "held_stores", "patch_writes", "own_block_dmas", "overlay_stores", "hot_block_hits", "ranged_reads",
        "ranged_read_bytes", "read_lane_fetches", "read_lane_fetch_bytes", "prefetch_issued",
        "daemon_cpu_ns", "mem_budget_bytes", "mem_budget_level", "direct_device_true",
        "lane_gate_threshold_bytes", "manager_verbs", "writer_guard_mode", "symmetric_meta"]
FSYNC_PHASES = ["intent_barrier", "data_flush", "staged_promote", "data_barrier",
                "meta_publish", "meta_barrier", "extent_barrier"]

ALL_COUNTERS = (DEVICE_WRITE_ARMS + DEVICE_WRITE_BESIDE + ADMISSION_ARMS + READ_COPY_ARMS + READ_COPY_BESIDE + RECLAIM + WIRE_RPCS
                + TICK_RESCUES + MUST_STAY_0 + IPC + JOURNAL + FUSE + MISC)


def flatten(doc):
    """path → leaf value; basename → first leaf value (stable first-wins)."""
    paths = {}
    names = {}

    def walk(o, p):
        if isinstance(o, dict):
            for k, v in o.items():
                walk(v, p + "/" + k)
        elif isinstance(o, list):
            for i, v in enumerate(o):
                walk(v, p + "/" + str(i))
        else:
            paths[p] = o
            base = p.rsplit("/", 1)[1]
            names.setdefault(base, o)

    walk(doc, "")
    return paths, names


def load_stats(path):
    with open(path) as f:
        return json.load(f)


def num(v):
    if isinstance(v, bool):
        return int(v)
    if isinstance(v, (int, float)):
        return v
    return 0


def terms(doc):
    """The ledger terms of one `.stats` document (+ the fsync phase sums)."""
    paths, names = flatten(doc)
    out = {}
    absent = []
    for k in ALL_COUNTERS:
        if k in names:
            out[k] = names[k] if isinstance(names[k], str) else num(names[k])
        else:
            out[k] = 0
            absent.append(k)
    # fsync_phase_ns: exact-sum family (Σ phases ≡ total to the ns).
    fs = {}
    for ph in FSYNC_PHASES + ["total"]:
        c = paths.get("/data/families/write/fsync/fsync_phase_ns/%s/count" % ph)
        s = paths.get("/data/families/write/fsync/fsync_phase_ns/%s/sum_ns" % ph)
        fs[ph] = {"count": num(c) if c is not None else 0, "sum_ns": num(s) if s is not None else 0}
    out["fsync_phase_ns"] = fs
    # The R5 components' current bytes (which gauge took the budget to Yellow/Red).
    comps = {}
    for k, v in paths.items():
        if k.startswith("/mem_budget/mem_budget_components/") and k.endswith("/current"):
            comps[k.split("/")[3]] = num(v)
    out["mem_budget_components"] = comps
    out["absent_keys"] = absent
    return out


def delta(after, before):
    d = {}
    for k, v in after.items():
        if k in ("absent_keys",):
            continue
        if k == "mem_budget_components":
            d[k] = v
            continue
        if k == "fsync_phase_ns":
            d[k] = {ph: {"count": v[ph]["count"] - before[k][ph]["count"],
                         "sum_ns": v[ph]["sum_ns"] - before[k][ph]["sum_ns"]} for ph in v}
        elif isinstance(v, str):
            d[k] = v
        else:
            d[k] = v - before.get(k, 0)
    d["absent_keys"] = sorted(set(after["absent_keys"]) | set(before["absent_keys"]))
    return d


# ---------------------------------------------------------------------------
# /proc/diskstats
# ---------------------------------------------------------------------------

def parse_diskstats(path):
    """name → {reads, read_sectors, writes, write_sectors, discards, discard_sectors, flushes}."""
    out = {}
    with open(path) as f:
        for line in f:
            p = line.split()
            if len(p) < 14:
                continue
            name = p[2]
            e = {"reads": int(p[3]), "read_sectors": int(p[5]), "writes": int(p[7]),
                 "write_sectors": int(p[9]), "ms_io": int(p[12])}
            if len(p) >= 18:
                e["discards"] = int(p[14])
                e["discard_sectors"] = int(p[16])
            if len(p) >= 20:
                e["flushes"] = int(p[18])
            out[name] = e
    return out


def diskstats_delta(before, after, devices):
    out = {}
    for dev in devices:
        b = before.get(dev)
        a = after.get(dev)
        if not a or not b:
            out[dev] = {"present": False}
            continue
        d = {k: a[k] - b[k] for k in a if k in b}
        d["present"] = True
        d["read_bytes"] = d["read_sectors"] * SECTOR
        d["write_bytes"] = d["write_sectors"] * SECTOR
        d["wareq_sz"] = (d["write_bytes"] // d["writes"]) if d["writes"] else 0
        d["rareq_sz"] = (d["read_bytes"] // d["reads"]) if d["reads"] else 0
        if "discard_sectors" in d:
            d["discard_bytes"] = d["discard_sectors"] * SECTOR
        out[dev] = d
    return out


# ---------------------------------------------------------------------------
# Instruments.
# ---------------------------------------------------------------------------

def fio_metrics(path):
    with open(path) as f:
        doc = json.load(f)
    jobs = doc["jobs"]
    m = {"instrument": "fio-" + doc.get("fio version", "?").replace("fio-", ""), "jobs": len(jobs)}
    for direction in ("read", "write"):
        ios = sum(j[direction]["total_ios"] for j in jobs)
        if ios == 0:
            continue
        io_bytes = sum(j[direction]["io_bytes"] for j in jobs)
        runtime_ms = max(j[direction]["runtime"] for j in jobs)
        bw_bytes = sum(j[direction]["bw_bytes"] for j in jobs)
        iops = sum(j[direction]["iops"] for j in jobs)
        clat = [j[direction]["clat_ns"] for j in jobs]
        # Mean over jobs weighted by ios; percentiles as the max over jobs (conservative).
        mean_ns = sum(c["mean"] * j[direction]["total_ios"] for c, j in zip(clat, jobs)) / ios
        pct = {}
        for q in ("50.000000", "99.000000", "99.900000"):
            vals = [c.get("percentile", {}).get(q) for c in clat]
            vals = [v for v in vals if v is not None]
            pct[q] = max(vals) if vals else 0
        m[direction] = {
            "ops": ios, "bytes": io_bytes, "runtime_ms": runtime_ms,
            "bw_bytes_per_s": bw_bytes, "mib_per_s": bw_bytes / (1 << 20), "iops": iops,
            "clat_mean_ns": mean_ns, "clat_p50_ns": pct["50.000000"], "clat_p99_ns": pct["99.000000"],
            "clat_p999_ns": pct["99.900000"], "clat_max_ns": max(c["max"] for c in clat),
        }
        if any("sync" in j and j["sync"].get("total_ios", 0) for j in jobs):
            sync_ios = sum(j["sync"]["total_ios"] for j in jobs)
            lat = [j["sync"].get("lat_ns", {}) for j in jobs]
            m["fsync"] = {"ops": sync_ios,
                          "lat_mean_ns": sum(l.get("mean", 0) * j["sync"]["total_ios"] for l, j in zip(lat, jobs)) / sync_ios,
                          "lat_p99_ns": max(l.get("percentile", {}).get("99.000000", 0) for l in lat)}
    errs = [j.get("error", 0) for j in jobs if j.get("error", 0)]
    m["instrument_errors"] = len(errs)
    if errs:
        m["instrument_error_text"] = "fio job error codes %s" % sorted(set(errs))
    direction = "write" if "write" in m else "read"
    m["direction"] = direction
    m["ops"] = m[direction]["ops"]
    m["bytes"] = m[direction]["bytes"]
    m["elapsed_s"] = m[direction]["runtime_ms"] / 1000.0
    m["user_write_bytes"] = m["write"]["bytes"] if "write" in m else 0
    m["user_read_bytes"] = m["read"]["bytes"] if "read" in m else 0
    return m


def mdtest_metrics(path):
    """mdtest's text: the `File creation` rate (ops/s) and the stonewall census."""
    text = open(path).read()
    m = {"instrument": "mdtest"}
    v = re.search(r"mdtest-([\d.]+\S*)", text)
    if v:
        m["instrument"] = "mdtest-" + v.group(1)
    for row in ("File creation", "Directory creation", "File stat", "File removal", "Directory removal",
                "Tree creation", "Tree removal"):
        mm = re.search(r"^\s*%s\s*:?\s+([\d.]+)\s+([\d.]+)\s+([\d.]+)\s+([\d.]+)" % re.escape(row), text, re.M)
        if mm:
            m[row.lower().replace(" ", "_")] = {"max": float(mm.group(1)), "min": float(mm.group(2)),
                                               "mean": float(mm.group(3)), "stddev": float(mm.group(4))}
    sw = re.search(r"Continue stonewall hit min:\s*(\d+)\s*max:\s*(\d+)\s*avg:\s*([\d.]+)", text)
    if sw:
        m["stonewall_items_min"] = int(sw.group(1))
        m["stonewall_items_max"] = int(sw.group(2))
        m["stonewall_items_avg"] = float(sw.group(3))
    np_ = re.search(r"(\d+) tasks, (\d+) files", text)
    if np_:
        m["tasks"] = int(np_.group(1))
        m["items"] = int(np_.group(2))
    m["ops"] = int(m.get("stonewall_items_avg", 0) * m.get("tasks", 0)) or m.get("items", 0)
    m["bytes"] = 0
    fc = m.get("file_creation")
    m["creates_per_s"] = fc["mean"] if fc else 0.0
    return m


# ---------------------------------------------------------------------------
# The leg record and its closure verdicts.
# ---------------------------------------------------------------------------

def leg(args):
    (before_p, after_p, dsb_p, dsa_p, metrics_p, meta_p) = args[:6]
    before = terms(load_stats(before_p))
    after = terms(load_stats(after_p))
    d = delta(after, before)
    meta = json.load(open(meta_p))
    metrics = json.load(open(metrics_p))
    devices = meta.get("disk_devices", [])
    ds = diskstats_delta(parse_diskstats(dsb_p), parse_diskstats(dsa_p), devices)
    kind = meta.get("row_kind", "data")
    ops = metrics.get("ops", 0)
    user_w = metrics.get("user_write_bytes", 0) or (metrics.get("bytes", 0) if metrics.get("direction") == "write" else 0)
    user_r = metrics.get("user_read_bytes", 0) or (metrics.get("bytes", 0) if metrics.get("direction") == "read" else 0)

    device_write_stats = sum(d[k] for k in DEVICE_WRITE_ARMS)
    device_write_reqs = sum(d[k] for k in DEVICE_WRITE_REQS)
    admission = sum(d[k] for k in ADMISSION_ARMS)
    read_copy = sum(d[k] for k in READ_COPY_ARMS)
    wire = sum(d[k] for k in WIRE_RPCS)
    ticks = {k: d[k] for k in TICK_RESCUES}
    must0 = {k: d[k] for k in MUST_STAY_0}
    reclaim = {k: d[k] for k in RECLAIM}
    reclaim_abs = {k: after[k] for k in RECLAIM}
    ipc_ops = d["ipc_ops_read"] + d["ipc_ops_write"]

    verdicts = []  # (name, ok, detail)
    # 0. the instrument itself completed (fio rc 0, every job error-free); an
    #    instrument that saw EAGAIN/EIO from the mount is a finding, not a number.
    irc = metrics.get("instrument_rc", 0)
    ierr = metrics.get("instrument_errors", 0)
    if irc or ierr:
        verdicts.append(("instrument_ok", False, "instrument rc %s, %s I/O errors (%s); daemon log: %s `answered EAGAIN` line(s)" % (
            irc, ierr, metrics.get("instrument_error_text", "?"), meta.get("daemon_eagain_lines", "?"))))
    # 1. must-stay-0 tripwires over the leg.
    bad0 = {k: v for k, v in must0.items() if v}
    verdicts.append(("must_stay_0", not bad0, "every tripwire delta 0" if not bad0 else "non-zero: %s" % bad0))
    # 2. reclaim ledger: queued + elided ≡ terminal frees (the delta AND the absolute, at quiesce).
    q_e = reclaim["block_free_reclaim_queued"] + reclaim["block_free_reclaim_elided"]
    q_e_abs = reclaim_abs["block_free_reclaim_queued"] + reclaim_abs["block_free_reclaim_elided"]
    ok = (q_e == reclaim["terminal_frees"]) and (q_e_abs == reclaim_abs["terminal_frees"])
    verdicts.append(("reclaim_ledger", ok, "Δ queued %d + elided %d = terminal %d; absolute %d = %d" % (
        reclaim["block_free_reclaim_queued"], reclaim["block_free_reclaim_elided"], reclaim["terminal_frees"],
        q_e_abs, reclaim_abs["terminal_frees"])))
    # 3. admission ledger: Σ admit_write_* ≡ user bytes at ACK (write rows).
    if kind in ("data-write", "fsync") and user_w:
        ratio = admission / user_w
        ok = abs(admission - user_w) <= max(1 << 20, user_w // 1000)  # 0.1 % or 1 MiB: the instrument's own housekeeping writes
        verdicts.append(("admission_ledger", ok, "Σ admit_write_* %d vs user %d (×%.4f)" % (admission, user_w, ratio)))
    # 4. engagement: the ops reach the mechanism under test.
    if meta.get("shim"):
        # The hybrid lane gate (charter §7): ops above `memBW × 2.3 µs / 2 copies`
        # (≈ 140–164 KiB; the threshold lives in the shim's client stats page, not
        # in `.stats`) route to kernel FUSE by design — a 1 MiB op is above any
        # derivation of it, so the kernel lane's ring requests are the engagement.
        bs = metrics.get("bs", 0)
        if bs >= (1 << 20):
            ok = d["transport_ring_requests"] >= ops
            verdicts.append(("engagement_shim_kernel_lane", ok,
                             "bs %d B is above the hybrid lane gate: ops route to kernel FUSE by design; Δtransport_ring_requests %d vs ops %d (ipc ops %d, gate routes %d)" % (
                                 bs, d["transport_ring_requests"], ops, ipc_ops, d["ipc_lane_gate_kernel_routes"])))
        else:
            ok = ipc_ops + d["ipc_lane_gate_kernel_routes"] >= ops and after["ipc_sessions_total"] >= 1
            verdicts.append(("engagement_shim", ok, "Δ(ipc_ops_read %d + ipc_ops_write %d) + gate routes %d vs instrument ops %d; sessions total %d, admission refusals Δ%d (budget %d); kernel-lane Δtransport_ring_requests %d" % (
                d["ipc_ops_read"], d["ipc_ops_write"], d["ipc_lane_gate_kernel_routes"], ops, after["ipc_sessions_total"],
                d["ipc_admission_refusals"], d["ipc_bind_refused_budget"], d["transport_ring_requests"])))
    elif kind.startswith("data") or kind == "fsync":
        direct = metrics.get("direct", 1)
        ring = d["transport_ring_requests"]
        ok = ring >= ops if direct else ring > 0
        verdicts.append(("engagement_fuse", ok, "Δtransport_ring_requests %d (handler-lane fuse_ops %d, direct-leg serves %d, zc write directs %d) vs instrument ops %d (%s)" % (
            ring, d["fuse_ops"], d["transport_zc_direct_legs"], d["transport_zc_write_directs"], ops,
            "direct: ≥ one ring request per I/O" if direct else "buffered: the page cache coalesces and splits")))
    # 5. fsync exact sum (whenever the family moved).
    fs = d["fsync_phase_ns"]
    if fs["total"]["count"]:
        s = sum(fs[ph]["sum_ns"] for ph in FSYNC_PHASES)
        ok = s == fs["total"]["sum_ns"]
        verdicts.append(("fsync_exact_sum", ok, "Σ phases %d ns vs total %d ns over %d fsyncs" % (s, fs["total"]["sum_ns"], fs["total"]["count"])))
    # 6. the solo mount's wire census.
    if meta.get("solo", True):
        verdicts.append(("solo_wire_rpcs_0", wire == 0, "wire rpcs Δ %d (%s)" % (wire, {k: d[k] for k in WIRE_RPCS if d[k]})))
    # 7. the daemon ran Green: a Yellow/Red leg sheds and parks (charter §6.2 R5) — not a number.
    lvl = after["mem_budget_level"]
    top = sorted(after["mem_budget_components"].items(), key=lambda kv: -kv[1])[:4]
    verdicts.append(("mem_budget_green", lvl == 0, "mem_budget_level %s at the after-snapshot (0 = Green; budget %d MiB; top components %s)" % (
        lvl, after["mem_budget_bytes"] >> 20, ", ".join("%s %d MiB" % (k, v >> 20) for k, v in top))))
    # 8. read copy ledger (read rows): dest + bounce + dest_dma + zc_serve + tier serves ≈ user bytes.
    if kind == "data-read" and user_r:
        ratio = read_copy / user_r
        verdicts.append(("read_copy_ledger", 0.98 <= ratio <= 1.02 or metrics.get("direct", 1) == 0,
                         "Σ read copy arms %d vs user %d (×%.4f)%s" % (read_copy, user_r, ratio, "" if metrics.get("direct", 1) else " (buffered: readahead moves more than the user asked)")))

    rec = {
        "leg": meta.get("leg"), "arm": meta.get("arm"), "binary": meta.get("binary"), "build": meta.get("build"),
        "started_at": meta.get("started_at"), "elapsed_s": metrics.get("elapsed_s"),
        "loadavg_before": meta.get("loadavg_before"), "loadavg_after": meta.get("loadavg_after"),
        "metrics": metrics,
        "ledgers": {
            "device_write_bytes_stats": device_write_stats,
            "device_write_arms": {k: d[k] for k in DEVICE_WRITE_ARMS},
            "device_write_beside": {k: d[k] for k in DEVICE_WRITE_BESIDE},
            "device_writes_stats": device_write_reqs,
            "device_write_reqs": {k: d[k] for k in DEVICE_WRITE_REQS},
            "wareq_sz_stats": (device_write_stats // device_write_reqs) if device_write_reqs else 0,
            "device_div_user_stats": (device_write_stats / user_w) if user_w else None,
            "admission_bytes": admission, "admission_arms": {k: d[k] for k in ADMISSION_ARMS},
            "read_copy_bytes": read_copy, "read_copy_arms": {k: d[k] for k in READ_COPY_ARMS},
            "read_copy_beside": {k: d[k] for k in READ_COPY_BESIDE},
            "reclaim": reclaim, "reclaim_absolute": reclaim_abs,
            "wire_rpcs": wire, "wire_arms": {k: d[k] for k in WIRE_RPCS},
            "tick_rescues": ticks, "must_stay_0": must0,
            "ipc": {k: d[k] for k in IPC},
            "journal": {k: d[k] for k in JOURNAL},
            "fuse": {k: d[k] for k in FUSE},
            "fsync_phase_ns": fs,
            "daemon_cpu_ns": d["daemon_cpu_ns"],
            "misc": {k: d[k] for k in ("held_stores", "patch_writes", "hot_block_hits", "ranged_reads", "ranged_read_bytes", "read_lane_fetches", "read_lane_fetch_bytes", "prefetch_issued", "manager_verbs")},
            "posture": {"direct_device_true": after["direct_device_true"], "writer_guard_mode": after["writer_guard_mode"],
                        "symmetric_meta": after["symmetric_meta"], "mem_budget_bytes": after["mem_budget_bytes"],
                        "mem_budget_level": after["mem_budget_level"],
                        "mem_budget_components_top": dict(sorted(after["mem_budget_components"].items(), key=lambda kv: -kv[1])[:8]),
                        "lane_gate_threshold_bytes": after["lane_gate_threshold_bytes"]},
            "absent_keys": d["absent_keys"],
        },
        "diskstats": ds,
        "verdicts": [{"check": n, "ok": ok, "detail": det} for (n, ok, det) in verdicts],
        "valid": all(ok for (_, ok, _) in verdicts),
    }
    # device ÷ user from the substrate: the data devices' written bytes over the user bytes.
    data_devs = meta.get("data_disk_devices", [])
    meta_devs = meta.get("meta_disk_devices", [])
    if user_w:
        dw = sum(ds[x]["write_bytes"] for x in data_devs if ds.get(x, {}).get("present"))
        mw = sum(ds[x]["write_bytes"] for x in meta_devs if ds.get(x, {}).get("present"))
        rec["ledgers"]["device_div_user_diskstats_data"] = dw / user_w if data_devs else None
        rec["ledgers"]["device_div_user_diskstats_meta"] = mw / user_w if meta_devs else None
        writes = sum(ds[x]["writes"] for x in data_devs if ds.get(x, {}).get("present"))
        rec["ledgers"]["wareq_sz_diskstats_data"] = (dw // writes) if writes else 0
    json.dump(rec, sys.stdout, indent=1, default=str)
    sys.stdout.write("\n")


# ---------------------------------------------------------------------------
# The bracket fold: A-B-B-A over the four leg records.
# ---------------------------------------------------------------------------

def headline(rec):
    """(name, value) of the leg's headline number by the row's kind."""
    m = rec["metrics"]
    h = m.get("headline")
    if h:
        return h["name"], h["value"]
    return "ops/s", (m.get("ops", 0) / m["elapsed_s"]) if m.get("elapsed_s") else 0


def fold(args):
    header_p = args[0]
    leg_ps = args[1:]
    header = json.load(open(header_p))
    legs = [json.load(open(p)) for p in leg_ps]
    name = None
    by_arm = {"A": [], "B": []}
    for l in legs:
        name, v = headline(l)
        if l.get("elapsed_s") is None:
            continue  # a dead/aborted leg has no number; it is INVALID on its own
        by_arm[l["arm"]].append(v)
    import statistics

    def stats(vs):
        if not vs:
            return {"mean": 0, "spread": 0, "min": 0, "max": 0}
        return {"mean": statistics.fmean(vs), "spread": max(vs) - min(vs), "min": min(vs), "max": max(vs),
                "spread_pct": (100.0 * (max(vs) - min(vs)) / statistics.fmean(vs)) if statistics.fmean(vs) else 0}

    a = stats(by_arm["A"])
    b = stats(by_arm["B"])
    allv = by_arm["A"] + by_arm["B"]
    band = stats(allv)
    same = header.get("a_equals_b", False)
    invalid = [l for l in legs if not l["valid"]]
    verdict = "VALID" if not invalid else "INVALID"
    if header.get("scoping"):
        verdict = "SCOPING-" + verdict
    rec = {
        "schema": "squeezefs-bracket/1",
        "row": header["row"], "date": header["date"], "verdict": verdict, "scoping": header.get("scoping", False),
        "header": header,
        "headline": {"name": name, "A": a, "B": b, "all": band,
                     "noise_band_pct": band.get("spread_pct", 0) if same else None,
                     "a_over_b": (a["mean"] / b["mean"]) if b["mean"] else None},
        "legs": legs,
        "invalid_reasons": [{"leg": l["leg"], "arm": l["arm"], "failed": [v for v in l["verdicts"] if not v["ok"]]} for l in invalid],
    }
    json.dump(rec, sys.stdout, indent=1, default=str)
    sys.stdout.write("\n")
    # The markdown row.
    md = open(args[0] + ".md", "w") if False else None
    print(markdown(rec), file=sys.stderr)


def fmt(v, unit=""):
    if v is None:
        return "—"
    if isinstance(v, float):
        if abs(v) >= 1000:
            return "%.0f%s" % (v, unit)
        if abs(v) >= 10:
            return "%.1f%s" % (v, unit)
        return "%.3f%s" % (v, unit)
    return "%s%s" % (v, unit)


def markdown(rec):
    h = rec["header"]
    hl = rec["headline"]
    legs = rec["legs"]
    lines = []
    lines.append("### %s — %s" % (rec["row"], rec["verdict"]))
    lines.append("")
    lines.append("instrument `%s` · substrate `%s` · venue `%s` · build `%s` · posture `%s` · mount `%s` · %s · %d legs × %s s (+%s s warm-up excluded) · order %s" % (
        h.get("instrument"), h.get("substrate_desc") or h.get("substrate"), h.get("venue_id"),
        (h.get("build_a") or {}).get("raw") if h.get("a_equals_b") else "A %s / B %s" % ((h.get("build_a") or {}).get("raw"), (h.get("build_b") or {}).get("raw")), h.get("posture"),
        h.get("mount_opts") or "defaults", h.get("date"), len(legs), h.get("duration_s"), h.get("warmup_s"), "→".join(l["arm"] for l in legs)))
    lines.append("")
    lines.append("| leg | arm | %s | p50 µs | p99 µs | device÷user (stats) | device÷user (diskstats data / meta) | wareq-sz B (stats / disk) | reclaim q+e=t | admission ×user | wire rpcs | tripwires | tick rescues | valid |" % hl["name"])
    lines.append("|---|---|---|---|---|---|---|---|---|---|---|---|---|---|")
    for l in legs:
        m = l["metrics"]
        L = l["ledgers"]
        _, v = headline(l)
        direction = m.get("direction")
        lat = m.get(direction, {}) if direction else {}
        rc = L["reclaim"]
        adm = ("×%.4f" % (L["admission_bytes"] / m["user_write_bytes"])) if m.get("user_write_bytes") else "—"
        ticks = sum(L["tick_rescues"].values())
        dsd = L.get("device_div_user_diskstats_data")
        dsm = L.get("device_div_user_diskstats_meta")
        lines.append("| %s | %s | %s | %s | %s | %s | %s / %s | %s / %s | %d+%d=%d | %s | %d | %d | %d | %s |" % (
            l["leg"], l["arm"], fmt(v), fmt(lat.get("clat_p50_ns", 0) / 1000.0) if lat else "—",
            fmt(lat.get("clat_p99_ns", 0) / 1000.0) if lat else "—",
            fmt(L["device_div_user_stats"]), fmt(dsd), fmt(dsm), L["wareq_sz_stats"], L.get("wareq_sz_diskstats_data", "—"),
            rc["block_free_reclaim_queued"], rc["block_free_reclaim_elided"], rc["terminal_frees"], adm, L["wire_rpcs"],
            L["must_stay_0"]["invariant_tripwires"], ticks, "yes" if l["valid"] else "NO"))
    lines.append("")
    lines.append("A mean %s (spread %s), B mean %s (spread %s)%s; all legs: min %s max %s spread %.1f %%." % (
        fmt(hl["A"]["mean"]), fmt(hl["A"]["spread"]), fmt(hl["B"]["mean"]), fmt(hl["B"]["spread"]),
        (" → A/B = %.3f" % hl["a_over_b"]) if hl.get("a_over_b") else "",
        fmt(hl["all"]["min"]), fmt(hl["all"]["max"]), hl["all"].get("spread_pct", 0)))
    if hl.get("noise_band_pct") is not None:
        lines.append("A = B (same binary): the spread IS the venue's noise band for this row: **%.1f %%**." % hl["noise_band_pct"])
    for r in rec["invalid_reasons"]:
        for f in r["failed"]:
            lines.append("- INVALID leg %s/%s: `%s` — %s" % (r["leg"], r["arm"], f["check"], f["detail"]))
    fs = legs[0]["ledgers"]["fsync_phase_ns"] if legs else None
    if fs and fs["total"]["count"]:
        lines.append("")
        lines.append("fsync phases (leg 1, mean ns/op): " + ", ".join("%s %d" % (ph, fs[ph]["sum_ns"] // max(1, fs[ph]["count"])) for ph in FSYNC_PHASES + ["total"]))
    extra = legs[0]["metrics"].get("notes") if legs else None
    if extra:
        lines.append("")
        lines.append("notes: " + extra)
    return "\n".join(lines)


# ---------------------------------------------------------------------------
# Venue and build identity.
# ---------------------------------------------------------------------------

def sh(cmd):
    try:
        return subprocess.run(cmd, shell=True, capture_output=True, text=True, timeout=20).stdout.strip()
    except Exception:
        return ""


def venue_id(args):
    kernel = os.uname().release
    arch = os.uname().machine
    lscpu = sh("lscpu")
    model = ""
    vendor = ""
    hyper = ""
    for line in lscpu.splitlines():
        if line.startswith("Model name:"):
            model = line.split(":", 1)[1].strip()
        elif line.startswith("Vendor ID:"):
            vendor = line.split(":", 1)[1].strip()
        elif line.startswith("Hypervisor vendor:"):
            hyper = line.split(":", 1)[1].strip()
    cpu = model if model and model != "-" else vendor or "unknown-cpu"
    if hyper or os.path.exists("/sys/devices/virtual/dmi/id/sys_vendor") and "QEMU" in sh("cat /sys/devices/virtual/dmi/id/sys_vendor"):
        cpu += "-vm"
    elif os.path.exists("/dev/vda") or os.path.exists("/dev/vdc"):
        cpu += "-vm"
    cores = os.cpu_count() or 0
    disk = args[0] if args else "vdc"
    disk_class = "unknown"
    if disk.startswith("vd"):
        disk_class = "virtio-blk"
    elif disk.startswith("nvme"):
        t = sh("cat /sys/block/%s/device/transport 2>/dev/null" % disk)
        disk_class = "nvme-" + (t or "pcie")
    elif disk.startswith("sd"):
        disk_class = "scsi"
    mem_gib = 0
    for line in open("/proc/meminfo"):
        if line.startswith("MemTotal:"):
            mem_gib = int(line.split()[1]) // (1 << 20)
    ident = "_".join([kernel, arch, re.sub(r"[^A-Za-z0-9.-]+", "-", cpu).strip("-").lower(), "%dc" % cores, disk_class])
    doc = {"venue_id": ident, "kernel": kernel, "arch": arch, "cpu": cpu, "cores": cores, "mem_gib": mem_gib,
           "disk": disk, "disk_class": disk_class, "hostname": os.uname().nodename,
           "fuse_enable_uring": sh("cat /sys/module/fuse/parameters/enable_uring")}
    json.dump(doc, sys.stdout)
    sys.stdout.write("\n")


def build(args):
    out = sh("%s --version" % args[0])
    m = re.match(r"squeezefs (\S+) \(([0-9a-f]+(?:-dirty)?), (\w+) profile, zig ([\d.]+)\)", out)
    import hashlib
    doc = {"raw": out, "binary": os.path.realpath(args[0]), "source": args[1] if len(args) > 1 else None,
           "sha256": hashlib.sha256(open(args[0], "rb").read()).hexdigest()}
    so = os.path.join(os.path.dirname(args[0]), "..", "lib", "libsqueezefs_il.so")
    if os.path.exists(so):
        doc["shim_sha256"] = hashlib.sha256(open(so, "rb").read()).hexdigest()
    if m:
        doc.update({"train": m.group(1), "commit": m.group(2), "profile": m.group(3), "zig": m.group(4),
                    "dirty": m.group(2).endswith("-dirty")})
    json.dump(doc, sys.stdout)
    sys.stdout.write("\n")


def summary(args):
    """One markdown table over every bracket record in a venue directory."""
    import glob
    recs = []
    for p in sorted(glob.glob(os.path.join(args[0], "*.json"))):
        try:
            r = json.load(open(p))
        except Exception:
            continue
        if r.get("schema") != "squeezefs-bracket/1":
            continue
        recs.append((os.path.basename(p), r))
    lines = ["| row | substrate | mount / lever | instrument | legs (A,B,B,A) | A mean | noise band | p50 / p99 µs | device÷user stats / disk | wareq-sz disk B | reclaim q+e=t | admission ×user | wire rpcs | tripwires | tick rescues | verdict | record |",
             "|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|"]
    for name, r in recs:
        h = r["header"]
        hl = r["headline"]
        legs = r["legs"]
        live = [l for l in legs if l.get("elapsed_s") is not None]
        vals = ", ".join(fmt(headline(l)[1]) for l in legs)
        p50 = p99 = "—"
        if live and live[0]["metrics"].get("direction"):
            lat = [l["metrics"][l["metrics"]["direction"]] for l in live]
            p50 = fmt(sum(x["clat_p50_ns"] for x in lat) / len(lat) / 1000.0)
            p99 = fmt(sum(x["clat_p99_ns"] for x in lat) / len(lat) / 1000.0)
        def avg(key_fn):
            xs = [key_fn(l) for l in live]
            xs = [x for x in xs if x is not None]
            return (sum(xs) / len(xs)) if xs else None
        dvu = avg(lambda l: l["ledgers"].get("device_div_user_stats"))
        dvd = avg(lambda l: l["ledgers"].get("device_div_user_diskstats_data"))
        war = avg(lambda l: l["ledgers"].get("wareq_sz_diskstats_data"))
        rc = [l["ledgers"]["reclaim"] for l in live]
        reclaim = "%d+%d=%d" % (sum(x["block_free_reclaim_queued"] for x in rc), sum(x["block_free_reclaim_elided"] for x in rc), sum(x["terminal_frees"] for x in rc)) if rc else "—"
        adm = avg(lambda l: (l["ledgers"]["admission_bytes"] / l["metrics"]["user_write_bytes"]) if l["metrics"].get("user_write_bytes") else None)
        wire = sum(l["ledgers"]["wire_rpcs"] for l in live)
        trip = sum(l["ledgers"]["must_stay_0"].get("invariant_tripwires", 0) for l in live)
        ticks = sum(sum(l["ledgers"]["tick_rescues"].values()) for l in live)
        opts = h.get("mount_opts") or "defaults"
        if h.get("lever") == "posture":
            opts = "lever posture (A symmetric / B single-writer)"
        band = ("%.1f %%" % hl["noise_band_pct"]) if hl.get("noise_band_pct") is not None else "—"
        lines.append("| %s | %s | %s | %s | %s | **%s %s** | %s | %s / %s | %s / %s | %s | %s | %s | %d | %d | %d | %s | `%s` |" % (
            r["row"], h["substrate"], opts, h["instrument"].split(" (")[0][:60], vals, fmt(hl["A"]["mean"]), hl["name"], band, p50, p99,
            fmt(dvu), fmt(dvd), fmt(war) if war is not None else "—", reclaim, ("×%.4f" % adm) if adm is not None else "—",
            wire, trip, ticks, r["verdict"], name))
    print("\n".join(lines))
    for name, r in recs:
        for reason in r["invalid_reasons"]:
            for f in reason["failed"]:
                print("- `%s` leg %s/%s INVALID `%s`: %s" % (r["row"], reason["leg"], reason["arm"], f["check"], f["detail"]))


def dead_leg(args):
    """A leg whose daemon died: INVALID with the reason, no ledgers (env D_*).
    A `wedged` marker beside the leg (written by lib.sh's bounded_run) names
    the wedge the harness broke by killing the daemon."""
    e = os.environ
    marker = os.path.join(os.path.dirname(os.path.abspath(args[0])), "wedged")
    wedged = open(marker).read().strip() if os.path.exists(marker) else None
    rec = {"leg": int(e["D_LEG"]), "arm": e["D_ARM"], "binary": e["D_BIN"], "build": json.loads(e["D_BUILD"]),
           "started_at": e.get("D_START", ""), "elapsed_s": None,
           "metrics": {"ops": 0, "bytes": 0, "headline": {"name": "—", "value": 0}},
           "ledgers": {"device_write_bytes_stats": 0, "device_write_arms": {}, "device_writes_stats": 0, "wareq_sz_stats": 0,
                       "device_div_user_stats": None, "admission_bytes": 0, "admission_arms": {}, "read_copy_bytes": 0,
                       "read_copy_arms": {}, "reclaim": {"terminal_frees": 0, "block_free_reclaim_queued": 0, "block_free_reclaim_elided": 0},
                       "reclaim_absolute": {}, "wire_rpcs": 0, "wire_arms": {}, "tick_rescues": {}, "must_stay_0": {"invariant_tripwires": 0},
                       "ipc": {}, "journal": {}, "fuse": {}, "fsync_phase_ns": {"total": {"count": 0, "sum_ns": 0}}, "daemon_cpu_ns": 0,
                       "misc": {}, "posture": {}, "absent_keys": []},
           "diskstats": {},
           "verdicts": [{"check": "daemon_wedged" if wedged else e.get("D_CHECK", "daemon_alive"), "ok": False,
                         "detail": wedged if wedged else (
                             ("daemon pid %s died %s (see the leg's daemon.log and dmesg.tail; the venue's other agents run `pkill -9 -x squeezefs` sweeps)" % (e["D_PID"], e["D_WHEN"]))
                             if e.get("D_CHECK", "daemon_alive") == "daemon_alive" else "the instrument failed %s (see the leg's warmup.log)" % e["D_WHEN"])}],
           "valid": False}
    json.dump(rec, open(args[0], "w"), indent=1)


def get(args):
    doc = load_stats(args[0])
    _, names = flatten(doc)
    v = names.get(args[1])
    print("" if v is None else v)


def main():
    if len(sys.argv) < 2:
        print(__doc__)
        sys.exit(2)
    cmd, rest = sys.argv[1], sys.argv[2:]
    if cmd == "venue-id":
        venue_id(rest)
    elif cmd == "build":
        build(rest)
    elif cmd == "get":
        get(rest)
    elif cmd == "dead-leg":
        dead_leg(rest)
    elif cmd == "summary":
        summary(rest)
    elif cmd == "leg":
        leg(rest)
    elif cmd == "fold":
        fold(rest)
    elif cmd == "fio-metrics":
        json.dump(fio_metrics(rest[0]), sys.stdout)
        sys.stdout.write("\n")
    elif cmd == "mdtest-metrics":
        json.dump(mdtest_metrics(rest[0]), sys.stdout)
        sys.stdout.write("\n")
    elif cmd == "markdown":
        print(markdown(json.load(open(rest[0]))))
    else:
        print("unknown command %s" % cmd, file=sys.stderr)
        sys.exit(2)


if __name__ == "__main__":
    main()
