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
# `write_through_bytes` is EVERY parked full buffer's upload (the sequential
# accumulation, the W2 escalation at its RAM trigger, the flush-time
# complement); `durable_upload_bytes_escalation` is its flush-time-complement
# SUBSET (measured on generic/074's growing-file shape: 825 write-through
# blocks, 0 escalation bytes — summing the subset alone read 0 device bytes
# against 3.5 GB on diskstats) and is reported beside. The two in-place arms
# land pages at a mapping the file owns alone: the W2 fold's (§J.1 landing 2)
# and the deferred full buffer's drain (§AK: an extending append's drain) —
# a content-valid buffer's dirty pages ride the same arm (§AJ.7). A superseded
# upload's DMA publishes its snapshot (§AJ.7), so `write_through_bytes` counts
# it (§AK.6: each DMA's stored length).
DEVICE_WRITE_ARMS = [
    "overlay_store_bytes", "patch_write_bytes", "own_block_dma_bytes",
    "write_through_bytes", "durable_upload_bytes_writeback",
    "fold_inplace_write_bytes", "drain_inplace_write_bytes",
]
# `drain_inplace_dmas` is beside: the drain's page DMAs ride the block layer's
# put (`put_obj` counts each one — the e2e row pins `put_obj + 1` per drain).
DEVICE_WRITE_BESIDE = ["rewrite_device_write_bytes", "rewrite_shadow_bytes", "rewrite_user_bytes", "durable_upload_bytes_escalation",
                       "drain_inplace_dmas"]
# The device-write REQUEST counters beside the byte arms (the stats-side
# `wareq-sz`; the substrate's diskstats is the ruler — these are the daemon's view).
# `put_obj` is the block device layer's put (an overlay store IS a put — not
# summed twice); a W1 patch DMA and a W2 in-place page DMA bypass it and are
# counted by `patch_writes` / `fold_inplace_dmas`.
DEVICE_WRITE_REQS = ["patch_writes", "own_block_dmas", "put_obj", "fold_inplace_dmas"]
# The staged arm's ring ledger (decision 0003 §J.2): bytes an append moved
# into the ring in place, and bytes a re-stage moved (out of the ring and
# back in) when the record's slack was exhausted.
STAGED_APPEND = ["staging_append_bytes", "staging_restage_bytes", "staging_restages", "staging_appends"]

# ---------------------------------------------------------------------------
# The write amplification law (decision 0003 §J.1; `extent_overlay.zig`
# `inPlaceBoundPpm` restates it): a write of `bs` bytes at `bs`-aligned
# offsets (fio's) touches at most `pages(bs, a)` pages, each written once per
# in-place fold, so `device ÷ user ≤ pages × PAGE / bs`; a page multiple is
# W1's (or the overlay's) 1×.
# ---------------------------------------------------------------------------
PAGE = 4096
# The ruler's slack: 10 % over the bound (the brackets gate's own 1.0–1.1
# band for whole-block rows) — the daemon's housekeeping writes land on the
# META device, never in the data arms.
AMP_SLACK = 0.1
# §J.2: `1 + 2(1 + ¼)/¼` over the geometric re-stage chain at a quarter slack.
STAGED_APPEND_BOUND = 11.0
# §AK: an append drained at its close lands its touched pages in place, and
# each block the appends reach lands whole ONCE at its first drain (write-
# before-publish; at most one block per block of appended bytes, so ≤ 1 per
# appended byte), plus the promotion's own-block DMA of the inline image
# (≤ one page per file): `device ≤ (pages(bs, a) × P / bs + 1) × appended +
# files × P`.
APPEND_DRAIN_HOLE_TERM = 1.0


def pages_worst(length, offset_align):
    return 1 + (length + PAGE - offset_align - 1) // PAGE


def append_drain_bound(bs, appended, files):
    """Device bytes an append-per-drain row may write (§AK), or None."""
    b = write_amp_bound(bs)
    if b is None or not appended:
        return None
    return (b + APPEND_DRAIN_HOLE_TERM) * appended + files * PAGE


def write_amp_bound(bs):
    if bs <= 0:
        return None
    if bs % PAGE == 0:
        return 1.0
    import math
    a = math.gcd(bs, PAGE)
    return pages_worst(bs, a) * PAGE / bs
ADMISSION_ARMS = [
    "admit_write_overlay_bytes", "admit_write_inplace_bytes",
    "admit_write_accumulate_bytes", "admit_write_inline_bytes",
]
# The read copy ledger (charter §6.3 `dest + bounce + dest_dma + zc_serve ≈ user
# bytes`): the PARTITION of the user's bytes by how they reached the reply —
# a daemon copy into the dest (`read_copy_dest_bytes`: every handler-side
# serve, the warm ladder's included), a device DMA straight into the dest
# (`read_dest_dma_bytes`: the lease and the raw full-block leg), the plane's
# own zc leg, and the transport's direct device leg (`transport_zc_direct_leg_
# serve_bytes`: served at the delivery CQE on a zero-copy session, never
# reaching a handler lane — so `fuse_ops` does not move and the engagement
# term for kernel-FUSE rows is `transport_ring_requests`). Each user byte in
# exactly one arm. The warm/hot/hold/cache/fill-slice terms are SUBSETS of
# `read_copy_dest_bytes` (reported beside: summing them double-counted every
# tier serve — the first ledger's lists were pinned on direct-leg rows where
# they read 0); `read_copy_bounce_bytes` is an intermediate pass (an NVMe-tier
# copy into a serve slot, an over-capacity fill backing), not a user byte;
# `transport_zc_serve_bytes` (direct leg + fd-source bridges) and
# `transport_zc_bounce_bytes_bridged` say how a body reached the slot, not
# whose bytes it was; `read_dest_lease_bytes` is the dest-lease MECHANISM's
# gauge over bytes `read_dest_dma_bytes` lands; a shim read's bytes land
# through the data plane into the arena, so `ipc_bytes_out` is not an arm.
READ_COPY_ARMS = [
    "read_copy_dest_bytes", "read_dest_dma_bytes", "read_zc_serve_bytes", "transport_zc_direct_leg_serve_bytes",
]
READ_COPY_BESIDE = ["read_copy_warm_serve_bytes", "read_copy_hot_serve_bytes", "read_copy_hold_serve_bytes",
                    "read_copy_cache_serve_bytes", "read_copy_fill_slice_bytes", "read_copy_bounce_bytes",
                    "transport_zc_serve_bytes", "transport_zc_bounce_bytes_bridged", "read_dest_lease_bytes",
                    "ipc_bytes_out", "ipc_read_demote_bytes"]
# The device-READ byte arms (decision 0003 §K — the read-amplification
# ledger): every path that moves bytes OFF a data device, each byte in exactly
# one arm, partitioned by PURPOSE so `device ÷ user` names its source. The
# plane's nine arms close EXACTLY against its one read choke point
# (`data_device_read_bytes`, read BESIDE: the `device_read_ledger` verdict);
# the transport's direct device leg, the zcrx lane and fsck's scrub DMA
# outside the plane and are summed in. The metadata plane's node reads are
# a separate ledger (`META_READ`), never summed into a data row's ratio.
DEVICE_READ_ARMS = [
    "read_dma_ranged_bytes", "read_dma_block_demand_bytes", "read_dma_block_prefetch_bytes",
    "read_dma_block_lane_bytes", "read_dma_image_bytes", "read_dma_window_bytes",
    "write_side_read_dma_bytes", "compose_read_dma_bytes", "pack_read_dma_bytes",
    "transport_zc_direct_leg_dma_bytes", "zcrx_fill_bytes", "fsck_scrub_read_bytes",
]
# The arms the plane's choke point sees (the closure's left side).
DEVICE_READ_PLANE_ARMS = DEVICE_READ_ARMS[:9]
# Beside: the plane's total, the WHERE partition (`pooled + registered ≡ the
# read path's six purpose arms`), the request count (`get_obj` = the stats-side
# `rareq-sz` denominator), and the user side — `read_user_bytes` (every byte the
# data plane answered, warm ladder included) + `transport_zc_direct_leg_serve_
# bytes` (the direct leg's validated serves never reach the plane).
DEVICE_READ_BESIDE = ["data_device_read_bytes", "read_fill_dma_bytes", "read_dest_dma_bytes", "get_obj",
                      "ranged_read_bytes", "read_lane_fetch_bytes", "prefetch_issued", "prefetch_wasted",
                      "prefetch_evicted_unconsumed", "read_streams_classified", "ranged_read_ghost_escalations",
                      "read_user_bytes", "transport_zc_direct_leg_serve_bytes"]
# The metadata read ledger: node bytes the metadata plane read off its device
# per record the client was answered (lookups, getattrs, dirents).
META_READ = ["node_read_bytes", "node_reads", "meta_kv_node_cache_hits", "meta_kv_node_cache_misses",
             "fuse_readdir_entries"]
RECLAIM = ["terminal_frees", "block_free_reclaim_queued", "block_free_reclaim_elided",
           "block_free_discard_bytes", "block_free_punch_bytes", "pack_terminal_frees"]
# The solo-mount wire census: every path that puts a verb on the cluster wire.
# The reference's `dlm_rpcs` has no single twin here; its meaning (a wire RPC a
# solo mount must never issue) is the SUM of these.
WIRE_RPCS = ["cluster_wire_frames_sent", "joined_wire_verbs", "xv_cross_owner_steps_shipped",
             "slot_resolve_rpcs", "xv_cross_owner_guard_rpcs", "dlm_token_grants",
             "slot_ships", "free_shipped_blocks"]
# The tick ledgers. RESCUES: work a backstop tick found that a wake should
# have delivered — each stays 0 on a healthy leg (the transport's two halves
# are lost-wake tripwires: a park owed a completed wake write no READ had
# consumed; completions a tick's own wait left in a CQ that was empty at the
# park). LATE: replies published and completions landed after a tick's park
# began — their wakes in flight (the kernel's -ETIME says the CQ was empty
# when the wait gave up), so they are late, never lost; read beside, never a
# rescue (`bench.zig` `tick_rescue_keys` / `tick_late_keys` name the same).
TICK_RESCUES = ["transport_park_tick_commit_lost_wakes", "transport_park_tick_cqe_lost_wakes",
                "journal_lane_park_tick_rescues", "meta_conveyor_park_tick_rescues",
                "block_free_reclaim_park_tick_wakes", "exec_lane_tick_rescues",
                "lock_ticked_reregisters", "channel_ticked_reregisters",
                "write_pipeline_admission_tick_wakes"]
TICK_LATE = ["transport_park_tick_late_commits", "transport_park_tick_late_cqes"]
MUST_STAY_0 = ["invariant_tripwires", "tripwire_sites_overflow", "meta_io_unaligned_refusals",
               "meta_io_buffered_fallback", "fuse_replies_synthesized", "fuse_reply_twice",
               "fuse_replies_oversize", "ipc_sessions_poisoned", "journal_lane_write_failures",
               "journal_lane_barrier_failures"]
IPC = ["ipc_ops_read", "ipc_ops_write", "ipc_bytes_in", "ipc_bytes_out", "ipc_sessions_total",
       "ipc_lane_gate_kernel_routes", "ipc_lane_gate_kernel_bytes", "ipc_fast_path_serves",
       "ipc_read_handoffs", "ipc_dd_inline_serves", "ipc_write_handoffs", "ipc_binds_dev_override",
       "ipc_admission_refusals", "ipc_bind_refused_budget", "ipc_shed_refusals", "ipc_binds",
       "ipc_client_aio_unknown_ctx_ops"]
JOURNAL = ["meta_kv_journal_entries", "meta_kv_journal_pad_entries", "meta_kv_journal_bytes",
           "meta_conveyor_drained_txs", "meta_conveyor_acks", "journal_lane_barriers_strict",
           "journal_lane_barriers_coalesced",
           # The write-stamp park (decision 0003 §L.6): size-neutral writes that journaled
           # nothing, the batched drains that made their stamps durable, the Δs they carried.
           "fuse_write_times_parked", "meta_kv_times_echo_drain_commits", "meta_kv_times_echo_drained"]
# The metadata-WRITE ledger (decision 0003 §L). The two device-write arms of
# the META device — the journal lane's own ring writes and the io pool's
# (node appends and images, ledger slots, bitmap and appender pages) — sum
# to the daemon's view of the META namespace's diskstats (`bench.zig`
# `meta_write_keys` names the same two). The journal's exact-sum terms:
# `meta_kv_journal_device_bytes ≡ meta_kv_journal_bytes + _pad_bytes +
# _header_bytes + _hole_bytes`; `pad ÷ device` is the pad fraction. The
# conveyor's window classes and hold outcomes carry the pacing law.
META_WRITE_ARMS = ["journal_lane_write_bytes", "meta_io_write_bytes"]
META_WRITE = ["meta_kv_journal_device_bytes", "meta_kv_journal_pad_bytes", "meta_kv_journal_header_bytes",
              "meta_kv_journal_hole_bytes", "meta_kv_journal_windows", "meta_kv_journal_windows_pad_majority",
              "meta_conveyor_holds", "meta_conveyor_hold_landings", "meta_conveyor_hold_deadlines", "meta_conveyor_holds_not_worth", "meta_conveyor_hold_probes", "meta_conveyor_hold_gain_milli",
              "meta_conveyor_windows_held", "meta_conveyor_windows_unshadowed", "meta_conveyor_windows_pipelined",
              "meta_conveyor_hold_bound_ns", "meta_kv_node_append_bytes", "meta_kv_node_rewrite_bytes",
              "meta_kv_ledger_writes"]
FUSE = ["fuse_ops", "transport_ring_requests", "transport_ring_reads", "transport_enters", "transport_fast_dispatch_serves",
        "transport_fast_dispatch_demotes", "transport_zc_direct_legs", "transport_zc_write_directs", "transport_payload_leases",
        "transport_wake_writes", "transport_wakes_elided", "transport_lease_overlong",
        "transport_requests_abandoned", "transport_cq_overflows"]
# (The hybrid lane gate's threshold is the CLIENT's — derived per process from
# its memBW probe onto the shim's stats page — and never a `.stats` key; the
# shim rows judge it by the block size, §4 of `leg`.)
MISC = ["put_obj", "held_stores", "patch_writes", "own_block_dmas", "overlay_stores", "hot_block_hits", "ranged_reads",
        "ranged_read_bytes", "read_lane_fetches", "read_lane_fetch_bytes", "prefetch_issued",
        "journal_lane_park_backstop_ticks", "parked_gate_waits", "parked_gate_cold_flushes",
        "daemon_cpu_ns", "mem_budget_bytes", "mem_budget_level", "direct_device_true",
        "manager_verbs", "writer_guard_mode", "symmetric_meta"]
FSYNC_PHASES = ["intent_barrier", "data_flush", "staged_promote", "data_barrier",
                "meta_publish", "meta_barrier", "extent_barrier"]
# `fuse_op_latency_ns`'s classes (`src/fuse/handler/op_trace.zig` OpClass).
OP_CLASSES = ["lookup", "getattr", "setattr", "mkdir", "create", "unlink", "rename", "readdir", "read", "write", "other"]

ALL_COUNTERS = (DEVICE_WRITE_ARMS + DEVICE_WRITE_BESIDE + DEVICE_WRITE_REQS + ADMISSION_ARMS + READ_COPY_ARMS + READ_COPY_BESIDE + RECLAIM
                + WIRE_RPCS + TICK_RESCUES + TICK_LATE + MUST_STAY_0 + IPC + JOURNAL + FUSE + MISC + STAGED_APPEND + DEVICE_READ_ARMS + DEVICE_READ_BESIDE + META_READ
                + META_WRITE_ARMS + META_WRITE)


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
    # The per-class op counts (the metadata read ledger's denominators).
    ops = {}
    for cls in OP_CLASSES:
        c = paths.get("/ops/fuse_op_latency_ns/%s/count" % cls)
        ops[cls] = num(c) if c is not None else 0
    out["fuse_op_counts"] = ops
    # The conveyor's window round trip and the §L hold: exact-sum histograms
    # (count, Σ ns) under member 0's `conveyor_phases`.
    cp = {}
    for ph in ("meta_window_total", "meta_pass_hold", "meta_pass_total"):
        c = paths.get("/meta/conveyor_phases/%s/count" % ph)
        s = paths.get("/meta/conveyor_phases/%s/sum_ns" % ph)
        cp[ph] = {"count": num(c) if c is not None else 0, "sum_ns": num(s) if s is not None else 0}
    out["conveyor_phase_ns"] = cp
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
        if k in ("fsync_phase_ns", "conveyor_phase_ns"):
            d[k] = {ph: {"count": v[ph]["count"] - before[k][ph]["count"],
                         "sum_ns": v[ph]["sum_ns"] - before[k][ph]["sum_ns"]} for ph in v}
        elif k == "fuse_op_counts":
            d[k] = {cls: v[cls] - before[k][cls] for cls in v}
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
    device_read_stats = sum(d[k] for k in DEVICE_READ_ARMS)
    device_read_plane = sum(d[k] for k in DEVICE_READ_PLANE_ARMS)
    user_read_stats = d["read_user_bytes"] + d["transport_zc_direct_leg_serve_bytes"]
    op_counts = d["fuse_op_counts"]
    meta_records = op_counts["lookup"] + op_counts["getattr"] + d["fuse_readdir_entries"]
    wire = sum(d[k] for k in WIRE_RPCS)
    ticks = {k: d[k] for k in TICK_RESCUES}
    ticks_late = {k: d[k] for k in TICK_LATE}
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
    if kind in ("data-write", "fsync", "data-append") and user_w:
        ratio = admission / user_w
        ok = abs(admission - user_w) <= max(1 << 20, user_w // 1000)  # 0.1 % or 1 MiB: the instrument's own housekeeping writes
        verdicts.append(("admission_ledger", ok, "Σ admit_write_* %d vs user %d (×%.4f)" % (admission, user_w, ratio)))
    # 3b. the write amplification law (§J.1): device bytes ≤ bound(bs) × user bytes on a data-write row.
    bound = write_amp_bound(int(metrics.get("bs", 0) or 0))
    if kind == "data-write" and user_w and bound is not None:
        ratio = device_write_stats / user_w
        ok = ratio <= bound + AMP_SLACK
        verdicts.append(("write_amplification_bound", ok, "device bytes %d ÷ user %d = ×%.3f vs the §J.1 bound ×%.3f (+%.1f slack) for bs %d; in-place folds %d B over %d DMAs" % (
            device_write_stats, user_w, ratio, bound, AMP_SLACK, int(metrics.get("bs", 0) or 0), d["fold_inplace_write_bytes"], d["fold_inplace_dmas"])))
    # 3c. the staged append law (§J.2): ring bytes moved ≤ 11 × bytes appended on an append row.
    if kind == "data-append" and user_w:
        moved = d["staging_append_bytes"] + d["staging_restage_bytes"]
        ratio = moved / user_w
        ok = ratio <= STAGED_APPEND_BOUND
        verdicts.append(("staged_append_bound", ok, "ring bytes moved %d (appends %d B over %d, re-stages %d B over %d) ÷ appended %d = ×%.3f vs the §J.2 bound ×%.0f" % (
            moved, d["staging_append_bytes"], d["staging_appends"], d["staging_restage_bytes"], d["staging_restages"], user_w, ratio, STAGED_APPEND_BOUND)))
        entries = d["meta_kv_journal_entries"]
        per_op = entries / ops if ops else 0.0
        verdicts.append(("journal_entries_per_append", per_op <= 1.05, "journal entries %d ÷ appends %d = %.3f/op (gate ≤ 1.05)" % (entries, ops, per_op)))
        # 3d. the append drain law (§AK): on a set without staging every
        #     append lands at its drain — the touched pages in place, each
        #     block once whole at its first; a staged set moves ring bytes.
        files = int(metrics.get("writers", 1) or 1)
        cap = append_drain_bound(int(metrics.get("bs", 0) or 0), user_w, files)
        if cap is not None:
            ok = device_write_stats <= cap * (1 + AMP_SLACK)
            verdicts.append(("append_drain_bound", ok, "device bytes %d ÷ appended %d = ×%.3f vs the §AK bound ×%.3f (+%.0f %% slack) for bs %d over %d file(s); in-place drains %d B over %d DMAs, whole-block uploads %d B" % (
                device_write_stats, user_w, device_write_stats / user_w, cap / user_w, AMP_SLACK * 100, int(metrics.get("bs", 0) or 0), files,
                d["drain_inplace_write_bytes"], d["drain_inplace_dmas"], d["write_through_bytes"])))
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
            verdicts.append(("engagement_shim", ok, "Δ(ipc_ops_read %d + ipc_ops_write %d) + gate routes %d vs instrument ops %d; sessions total %d, admission refusals Δ%d (budget %d; a refused client process is kernel-served for its backoff window); aio iocbs on unknown contexts Δ%d; kernel-lane Δtransport_ring_requests %d" % (
                d["ipc_ops_read"], d["ipc_ops_write"], d["ipc_lane_gate_kernel_routes"], ops, after["ipc_sessions_total"],
                d["ipc_admission_refusals"], d["ipc_bind_refused_budget"], d["ipc_client_aio_unknown_ctx_ops"], d["transport_ring_requests"])))
    elif kind.startswith("data") or kind == "fsync":
        direct = metrics.get("direct", 1)
        ring = d["transport_ring_requests"]
        ok = ring >= ops if direct else ring > 0
        if kind == "data-append":
            ok = ring >= ops  # every append is its own FUSE_WRITE (O_APPEND passes through)
        verdicts.append(("engagement_fuse", ok, "Δtransport_ring_requests %d (handler-lane fuse_ops %d, direct-leg serves %d, zc write directs %d) vs instrument ops %d (%s)" % (
            ring, d["fuse_ops"], d["transport_zc_direct_legs"], d["transport_zc_write_directs"], ops,
            "direct: ≥ one ring request per I/O" if direct else "buffered: the page cache coalesces and splits")))
    # 5. fsync exact sum (whenever the family moved).
    fs = d["fsync_phase_ns"]
    if fs["total"]["count"]:
        s = sum(fs[ph]["sum_ns"] for ph in FSYNC_PHASES)
        ok = s == fs["total"]["sum_ns"]
        verdicts.append(("fsync_exact_sum", ok, "Σ phases %d ns vs total %d ns over %d fsyncs" % (s, fs["total"]["sum_ns"], fs["total"]["count"])))
    # 5b. The metadata-WRITE ledger (decision 0003 §L), whenever the journal moved.
    meta_write_stats = sum(d[k] for k in META_WRITE_ARMS)
    jdev = d["meta_kv_journal_device_bytes"]
    jpad = d["meta_kv_journal_pad_bytes"]
    jent = d["meta_kv_journal_bytes"]
    jwin = d["meta_kv_journal_windows"]
    cp = d["conveyor_phase_ns"]
    rtt_ns = (cp["meta_window_total"]["sum_ns"] / cp["meta_window_total"]["count"]) if cp["meta_window_total"]["count"] else 0
    hold_ns = (cp["meta_pass_hold"]["sum_ns"] / cp["meta_pass_hold"]["count"]) if cp["meta_pass_hold"]["count"] else 0
    # The windows the pacing law governs: pad-majority ones the worth gate did
    # not decline (a declined window is issued beside the flight by design).
    paced = d["meta_conveyor_windows_held"] + d["meta_conveyor_windows_unshadowed"] - d["meta_conveyor_holds_not_worth"]
    elapsed_ns = (metrics.get("elapsed_s") or 0) * 1e9
    if jwin:
        # The journal's exact sum closes by construction; the harness restates it.
        jsum = jent + jpad + d["meta_kv_journal_header_bytes"] + d["meta_kv_journal_hole_bytes"]
        pad_fraction = jpad / jdev if jdev else 0.0
        # The pacing law: pad-majority windows at most one per window round
        # trip (+ the deadline-ended holds, + the first); 10 % slack on the
        # round trip's mean against the hold's own clock.
        paced_bound = (elapsed_ns / rtt_ns * 1.1 if rtt_ns else float("inf")) + d["meta_conveyor_hold_deadlines"] + 1
        # The strict cadence arms no hold (its committers declared latency over bytes).
        strict = d["journal_lane_barriers_strict"] > 0
        ok = jsum == jdev and (strict or paced <= paced_bound)
        verdicts.append(("meta_pad_fraction_bound", ok,
                         "journal device bytes %d ≡ entries %d + pad %d + headers %d + holes %d (%s); pad fraction %.3f over %d windows (%.2f entries/window, %d B/op); pad-majority windows under the law %d (held %d, unshadowed %d, of which the gate declined %d) vs the pacing bound %.0f (= span %.1f s ÷ window rtt %.0f µs × 1.1 + %d deadline-ended holds + 1); holds %d (not worth %d, probes %d; live gain %.3f/hold), mean hold %.0f µs, live bound %d µs" % (
                             jdev, jent, jpad, d["meta_kv_journal_header_bytes"], d["meta_kv_journal_hole_bytes"], "closes" if jsum == jdev else "DOES NOT CLOSE",
                             pad_fraction, jwin, (d["meta_kv_journal_entries"] / jwin), (jdev // ops) if ops else 0,
                             paced, d["meta_conveyor_windows_held"], d["meta_conveyor_windows_unshadowed"], d["meta_conveyor_holds_not_worth"], paced_bound, elapsed_ns / 1e9, rtt_ns / 1e3, d["meta_conveyor_hold_deadlines"],
                             d["meta_conveyor_holds"], d["meta_conveyor_holds_not_worth"], d["meta_conveyor_hold_probes"], after["meta_conveyor_hold_gain_milli"] / 1000.0, hold_ns / 1e3, after["meta_conveyor_hold_bound_ns"] // 1000)))
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
    # 9. the device-read ledger closes EXACTLY at the plane's one read choke
    #    point (every row: writes read seed images too). An old binary without
    #    the arms reads 0 ≡ 0 and lists them under absent_keys.
    verdicts.append(("device_read_ledger", device_read_plane == d["data_device_read_bytes"],
                     "Σ plane read arms %d vs data_device_read_bytes %d (%s)" % (
                         device_read_plane, d["data_device_read_bytes"],
                         ", ".join("%s %d" % (k, d[k]) for k in DEVICE_READ_PLANE_ARMS if d[k]) or "no device reads")))
    # 10. the read-amplification bound (decision 0003 §K): the row's shape
    #     derives `device ÷ user ≤ bound`; the substrate's namespaces are the
    #     ruler when the data devices are exclusive to the mount (nvmet), the
    #     daemon's arms otherwise. A row without a bound reports the ratio.
    data_devs = meta.get("data_disk_devices", [])
    meta_devs = meta.get("meta_disk_devices", [])
    disk_read = sum(ds[x]["read_bytes"] for x in data_devs if ds.get(x, {}).get("present"))
    disk_reads = sum(ds[x]["reads"] for x in data_devs if ds.get(x, {}).get("present"))
    exclusive = bool(meta.get("data_devices_exclusive", False))
    amp_stats = (device_read_stats / user_r) if user_r else None
    amp_disk = (disk_read / user_r) if (user_r and data_devs) else None
    bound = metrics.get("read_amp_bound")
    excluded = [k for k in metrics.get("read_amp_exclude", []) if k in DEVICE_READ_ARMS]
    if kind == "data-read" and user_r and bound is not None:
        if excluded:
            # An arm another mechanism owns (the fold's seed images behind a
            # read-after-write row) is taken out of the bounded ratio: only
            # the daemon's arms can tell purposes apart.
            ruler = (device_read_stats - sum(d[k] for k in excluded)) / user_r
            ruler_name = "stats arms minus %s" % "+".join(excluded)
        elif exclusive and amp_disk is not None:
            ruler, ruler_name = amp_disk, "diskstats (exclusive namespaces)"
        else:
            ruler, ruler_name = amp_stats, "stats arms"
        verdicts.append(("read_amplification_bound", ruler <= bound,
                         "device ÷ user %.4f by %s (stats %.4f, diskstats %s) vs bound %.4f — %s; arms: %s" % (
                             ruler, ruler_name, amp_stats, ("%.4f" % amp_disk) if amp_disk is not None else "—", bound,
                             metrics.get("read_amp_bound_why", "?"),
                             ", ".join("%s %d" % (k, d[k]) for k in DEVICE_READ_ARMS if d[k]) or "none")))

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
            "write_amplification_bound": bound,
            "staged_append": {k: d[k] for k in STAGED_APPEND},
            "admission_bytes": admission, "admission_arms": {k: d[k] for k in ADMISSION_ARMS},
            "read_copy_bytes": read_copy, "read_copy_arms": {k: d[k] for k in READ_COPY_ARMS},
            "read_copy_beside": {k: d[k] for k in READ_COPY_BESIDE},
            "device_read_bytes_stats": device_read_stats,
            "device_read_arms": {k: d[k] for k in DEVICE_READ_ARMS},
            "device_read_beside": {k: d[k] for k in DEVICE_READ_BESIDE},
            "user_read_bytes_stats": user_read_stats,
            "device_div_user_read_stats": amp_stats,
            "device_div_user_read_diskstats_data": amp_disk,
            "rareq_sz_stats": (device_read_stats // d["get_obj"]) if d["get_obj"] else 0,
            "rareq_sz_diskstats_data": (disk_read // disk_reads) if disk_reads else 0,
            "read_amp_bound": bound,
            "meta_read": {k: d[k] for k in META_READ},
            "fuse_op_counts": op_counts,
            "meta_records": meta_records,
            "meta_node_bytes_per_record": (d["node_read_bytes"] / meta_records) if meta_records else None,
            "meta_node_reads_per_record": (d["node_reads"] / meta_records) if meta_records else None,
            "reclaim": reclaim, "reclaim_absolute": reclaim_abs,
            "wire_rpcs": wire, "wire_arms": {k: d[k] for k in WIRE_RPCS},
            "tick_rescues": ticks, "tick_late": ticks_late, "must_stay_0": must0,
            "ipc": {k: d[k] for k in IPC},
            "journal": {k: d[k] for k in JOURNAL},
            "fuse": {k: d[k] for k in FUSE},
            "fsync_phase_ns": fs,
            "conveyor_phase_ns": cp,
            "meta_write_bytes_stats": meta_write_stats,
            "meta_write_arms": {k: d[k] for k in META_WRITE_ARMS},
            "meta_write": {k: d[k] for k in META_WRITE},
            "meta_pad_fraction": (jpad / jdev) if jdev else None,
            "meta_journal_bytes_per_op": (jdev / ops) if ops else None,
            "meta_window_rtt_ns": rtt_ns,
            "meta_hold_ns": hold_ns,
            "daemon_cpu_ns": d["daemon_cpu_ns"],
            "misc": {k: d[k] for k in ("held_stores", "patch_writes", "hot_block_hits", "ranged_reads", "ranged_read_bytes", "read_lane_fetches", "read_lane_fetch_bytes", "prefetch_issued", "manager_verbs")},
            "posture": {"direct_device_true": after["direct_device_true"], "writer_guard_mode": after["writer_guard_mode"],
                        "symmetric_meta": after["symmetric_meta"], "mem_budget_bytes": after["mem_budget_bytes"],
                        "mem_budget_level": after["mem_budget_level"],
                        "mem_budget_components_top": dict(sorted(after["mem_budget_components"].items(), key=lambda kv: -kv[1])[:8])},
            "absent_keys": d["absent_keys"],
        },
        "diskstats": ds,
        "verdicts": [{"check": n, "ok": ok, "detail": det} for (n, ok, det) in verdicts],
        "valid": all(ok for (_, ok, _) in verdicts),
    }
    # device ÷ user from the substrate: the data devices' written bytes over the user bytes.
    if user_w:
        dw = sum(ds[x]["write_bytes"] for x in data_devs if ds.get(x, {}).get("present"))
        mw = sum(ds[x]["write_bytes"] for x in meta_devs if ds.get(x, {}).get("present"))
        rec["ledgers"]["device_div_user_diskstats_data"] = dw / user_w if data_devs else None
        rec["ledgers"]["device_div_user_diskstats_meta"] = mw / user_w if meta_devs else None
        rec["ledgers"]["meta_device_div_user_stats"] = meta_write_stats / user_w
        writes = sum(ds[x]["writes"] for x in data_devs if ds.get(x, {}).get("present"))
        rec["ledgers"]["wareq_sz_diskstats_data"] = (dw // writes) if writes else 0
    # 9. The META device's ledger closes against its namespace (§L): the two
    #    device-write arms account for the META diskstats within 2 % (the
    #    kernel's own flushes carry no bytes), reported as `÷ user bytes`.
    meta_present = [x for x in meta_devs if ds.get(x, {}).get("present")]
    if meta_present and meta_write_stats:
        mw = sum(ds[x]["write_bytes"] for x in meta_present)
        closure = meta_write_stats / mw if mw else 0.0
        ok = 0.98 <= closure <= 1.02
        rec["verdicts"].append({"check": "meta_device_div_user", "ok": ok, "detail": "META device bytes: stats %d (journal lane %d + io pool %d) vs diskstats %d (×%.4f); ÷ user bytes: stats %s, diskstats %s; journal %d B/op, pad fraction %s" % (
            meta_write_stats, d["journal_lane_write_bytes"], d["meta_io_write_bytes"], mw, closure,
            ("%.3f" % (meta_write_stats / user_w)) if user_w else "—", ("%.3f" % (mw / user_w)) if user_w else "—",
            (jdev // ops) if ops else 0, ("%.3f" % (jpad / jdev)) if jdev else "—")})
        rec["valid"] = rec["valid"] and ok
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
    lines.append("instrument `%s` · substrate `%s` · venue `%s` · build `%s` · posture `%s` · format `%s` · mount `%s` · %s · %d legs × %s s (+%s s warm-up excluded) · order %s" % (
        h.get("instrument"), h.get("substrate_desc") or h.get("substrate"), h.get("venue_id"),
        (h.get("build_a") or {}).get("raw") if h.get("a_equals_b") else "A %s / B %s" % ((h.get("build_a") or {}).get("raw"), (h.get("build_b") or {}).get("raw")), h.get("posture"),
        h.get("format_opts") or "defaults", h.get("mount_opts") or "defaults", h.get("date"), len(legs), h.get("duration_s"), h.get("warmup_s"), "→".join(l["arm"] for l in legs)))
    lines.append("")
    lines.append("| leg | arm | %s | p50 µs | p99 µs | device÷user (stats) | device÷user (diskstats data / meta) | meta pad fraction / journal B/op / entries per window | wareq-sz B (stats / disk) | reclaim q+e=t | admission ×user | wire rpcs | tripwires | tick rescues | valid |" % hl["name"])
    lines.append("|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|")
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
        mw = L.get("meta_write", {})
        jwin = mw.get("meta_kv_journal_windows", 0)
        per_window = ("%.2f" % (L["journal"]["meta_kv_journal_entries"] / jwin)) if jwin else "—"
        lines.append("| %s | %s | %s | %s | %s | %s | %s / %s | %s / %s / %s | %s / %s | %d+%d=%d | %s | %d | %d | %d | %s |" % (
            l["leg"], l["arm"], fmt(v), fmt(lat.get("clat_p50_ns", 0) / 1000.0) if lat else "—",
            fmt(lat.get("clat_p99_ns", 0) / 1000.0) if lat else "—",
            fmt(L["device_div_user_stats"]), fmt(dsd), fmt(dsm),
            fmt(L.get("meta_pad_fraction")), fmt(L.get("meta_journal_bytes_per_op")), per_window,
            L["wareq_sz_stats"], L.get("wareq_sz_diskstats_data", "—"),
            rc["block_free_reclaim_queued"], rc["block_free_reclaim_elided"], rc["terminal_frees"], adm, L["wire_rpcs"],
            L["must_stay_0"]["invariant_tripwires"], ticks, "yes" if l["valid"] else "NO"))
    # The read-amplification ledger (decision 0003 §K) for every leg that read.
    if any(l["metrics"].get("user_read_bytes") for l in legs):
        lines.append("")
        lines.append("| leg | arm | read device÷user (stats) | read device÷user (diskstats data) | bound | rareq-sz B (stats / disk) | ranged | demand blk | prefetch blk | lane blk | image | window | compose | write-side | direct leg | plane ledger |")
        lines.append("|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|")
        for l in legs:
            L = l["ledgers"]
            a = L.get("device_read_arms", {})
            closed = next((v["ok"] for v in l["verdicts"] if v["check"] == "device_read_ledger"), None)
            lines.append("| %s | %s | %s | %s | %s | %s / %s | %s | %s | %s | %s | %s | %s | %s | %s | %s | %s |" % (
                l["leg"], l["arm"], fmt(L.get("device_div_user_read_stats")), fmt(L.get("device_div_user_read_diskstats_data")),
                fmt(L.get("read_amp_bound")), L.get("rareq_sz_stats", "—"), L.get("rareq_sz_diskstats_data", "—"),
                a.get("read_dma_ranged_bytes", 0), a.get("read_dma_block_demand_bytes", 0), a.get("read_dma_block_prefetch_bytes", 0),
                a.get("read_dma_block_lane_bytes", 0), a.get("read_dma_image_bytes", 0), a.get("read_dma_window_bytes", 0),
                a.get("compose_read_dma_bytes", 0), a.get("write_side_read_dma_bytes", 0), a.get("transport_zc_direct_leg_dma_bytes", 0),
                {True: "closed", False: "OPEN", None: "—"}[closed]))
    # The metadata read ledger for every leg that answered metadata.
    if any((l["ledgers"].get("meta_records") or 0) for l in legs):
        lines.append("")
        lines.append("| leg | arm | lookups | getattrs | dirents | node reads | node bytes | node bytes/record | node reads/record | cache hits | cache misses |")
        lines.append("|---|---|---|---|---|---|---|---|---|---|---|")
        for l in legs:
            L = l["ledgers"]
            oc = L.get("fuse_op_counts", {})
            mr = L.get("meta_read", {})
            lines.append("| %s | %s | %d | %d | %d | %d | %d | %s | %s | %d | %d |" % (
                l["leg"], l["arm"], oc.get("lookup", 0), oc.get("getattr", 0), mr.get("fuse_readdir_entries", 0),
                mr.get("node_reads", 0), mr.get("node_read_bytes", 0), fmt(L.get("meta_node_bytes_per_record")), fmt(L.get("meta_node_reads_per_record")),
                mr.get("meta_kv_node_cache_hits", 0), mr.get("meta_kv_node_cache_misses", 0)))
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
    # The §J and §L laws, per leg, when the row engaged them.
    for l in legs:
        for v in l["verdicts"]:
            if v["check"] in ("write_amplification_bound", "staged_append_bound", "append_drain_bound", "journal_entries_per_append", "meta_pad_fraction_bound", "meta_device_div_user"):
                lines.append("- leg %s/%s `%s` %s: %s" % (l["leg"], l["arm"], v["check"], "ok" if v["ok"] else "FAILED", v["detail"]))
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
        if h.get("format_opts"):
            opts = "%s; format %s" % (opts, h["format_opts"])
        if h.get("lever") == "posture":
            opts = "lever posture (A symmetric / B single-writer)"
        band = ("%.1f %%" % hl["noise_band_pct"]) if hl.get("noise_band_pct") is not None else "—"
        lines.append("| %s | %s | %s | %s | %s | **%s %s** | %s | %s / %s | %s / %s | %s | %s | %s | %d | %d | %d | %s | `%s` |" % (
            r["row"], h["substrate"], opts, h["instrument"].split(" (")[0][:60], vals, fmt(hl["A"]["mean"]), hl["name"], band, p50, p99,
            fmt(dvu), fmt(dvd), fmt(war) if war is not None else "—", reclaim, ("×%.4f" % adm) if adm is not None else "—",
            wire, trip, ticks, r["verdict"], name))
    print("\n".join(lines))
    # The read-amplification table: every record whose legs read.
    read_lines = ["", "| row | substrate | mount | A read device÷user stats / disk | B read device÷user stats / disk | bound | node bytes/record A / B | verdict | record |",
                  "|---|---|---|---|---|---|---|---|---|"]
    n_read = 0
    for name, r in recs:
        h = r["header"]
        legs = [l for l in r["legs"] if l.get("elapsed_s") is not None]
        if not any(l["metrics"].get("user_read_bytes") or l["ledgers"].get("meta_records") for l in legs):
            continue
        n_read += 1
        def arm_avg(arm, key):
            xs = [l["ledgers"].get(key) for l in legs if l["arm"] == arm]
            xs = [x for x in xs if x is not None]
            return (sum(xs) / len(xs)) if xs else None
        bound = next((l["ledgers"].get("read_amp_bound") for l in legs if l["ledgers"].get("read_amp_bound") is not None), None)
        read_lines.append("| %s | %s | %s | %s / %s | %s / %s | %s | %s / %s | %s | `%s` |" % (
            r["row"], h["substrate"], (h.get("mount_opts") or "defaults") + (("; format " + h["format_opts"]) if h.get("format_opts") else ""),
            fmt(arm_avg("A", "device_div_user_read_stats")), fmt(arm_avg("A", "device_div_user_read_diskstats_data")),
            fmt(arm_avg("B", "device_div_user_read_stats")), fmt(arm_avg("B", "device_div_user_read_diskstats_data")),
            fmt(bound), fmt(arm_avg("A", "meta_node_bytes_per_record")), fmt(arm_avg("B", "meta_node_bytes_per_record")),
            r["verdict"], name))
    if n_read:
        print("\n".join(read_lines))
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
                       "reclaim_absolute": {}, "wire_rpcs": 0, "wire_arms": {}, "tick_rescues": {}, "tick_late": {}, "must_stay_0": {"invariant_tripwires": 0},
                       "ipc": {}, "journal": {}, "fuse": {}, "fsync_phase_ns": {"total": {"count": 0, "sum_ns": 0}}, "daemon_cpu_ns": 0,
                       "misc": {}, "posture": {}, "absent_keys": [],
                       "device_read_bytes_stats": 0, "device_read_arms": {}, "device_read_beside": {}, "user_read_bytes_stats": 0,
                       "device_div_user_read_stats": None, "device_div_user_read_diskstats_data": None, "rareq_sz_stats": 0,
                       "rareq_sz_diskstats_data": 0, "read_amp_bound": None, "meta_read": {}, "fuse_op_counts": {}, "meta_records": 0,
                       "meta_node_bytes_per_record": None, "meta_node_reads_per_record": None},
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
