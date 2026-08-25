# The width-8 s9-fanout re-grade — residual 5 retired, finding 14 fixed

**Date:** 2026-08-25 · **Binary:** `a7ac0062` (green row; the first pass ran
`69674d40`) · **Venue:** 1 authority + 7 co-writers (width 8), one box,
nvmet-tcp devsub, `SQZ_MWFLEET_OSS_GB=32` · **Instrument:**
`tests/run_mw_matrix.sh s9-fanout` (dd conv=fsync, phase-W 1024 MiB +
phase-R 512 MiB per member, durable rows)

## Why this row ran

The rung-20 residual board's **item 5** (the multi-writer program's last
live wedge): *"s9-fanout at width 8 on a custody-armed fleet wedged in
shipped-free double-release churn"* (recorded at rungs 18/19 on the
2026-08-18 binary; the proven venue was width 2). Since that capture, two
programs rewrote the paths under the wedge: the free-grace
pressure-coupled release valve (2026-08-21) and finding 13's
owner-partitioned shipped-free ledger read (`bebe2a78`, publish schema 7).
The counted-run law made the old wedge row evidence about a retired
binary; this re-grade is its from-zero re-run.

## Verdict: the wedge DID NOT REPRODUCE — GATE GREEN at width 8

From-zero counted row (`rows-s9a-width8-1787625423/`, final binary):

| member | role | W MB/s | R MB/s | fresh MiB | rw MiB | free_ship |
|---|---|---|---|---|---|---|
| m0 | authority | 1141 | 2532 | 1020 | 515 | 0 |
| m50–m56 | co-writers | 884–1030 | 866–1250 | 968–1024 | 462–515 | 129–162 each |

- **Engagement exact**: every co-writer's publishes and displaced frees
  shipped and were served (`free_ship` per member ≈ the rewritten blocks;
  authority ships 0); `local_commit_refusals` 0, `enospc` 0, harvests 0.
- **Amplification 1.037×** (12,288 user MiB → 12,744 device MiB on the
  data namespaces; meta 4 MiB, separate). Tripwires flat.
- **Oracle clean**: fsck findings 0, `meta_kv_block_refs_drift` 0.
- Both phases ran to completion at ~1 GB/s per member × 8 concurrent
  writers — the shipped-free funnel (the per-volume design's named
  "most likely relocated wall") carried 8-way fan-in without wedging.
  Evidence tier: measured-simulated (one box, co-located identities).

Attribution honesty: the wedge was not bisected to a single fix — the
old binary is two programs behind, and both the free-grace valve and the
finding-13 verdict rework sit on the double-release churn's paths. What
this row proves is the CURRENT binary's behavior at width 8, which is
what the residual board tracks.

## Finding 14 (found by this re-grade's second pass, fixed red-first)

**The one-attempt wire classes burned their only attempt on idle-reaped
sessions.** The warm-fleet second pass opened an existing file
(`dd` O_TRUNC) after a ~13 min quiet spell and failed **EINVAL on a
healthy fleet**: `cluster wire: the coordinator closed the session`. The
write-open's custody acquire rides the pooled workload session
(`data_grant::call_once` — one attempt, no reconnect, deliberately: an
acquire storm must not double-park arbitration), and the wire's 60 s
idle-session reaper had closed that socket minutes earlier. The
un-witnessed publish mutators (`ParkWriteTimes`/`DestroyInodes`/
`CreateWithRdevSize`, one attempt by the no-double-apply law) shared the
exposure. Every co-writer fleet hits this on the first write after any
minute of quiet.

**Fix** (`a7ac0062`; red tests `eb3059a3`,
`tests/mw_cowriter_lane_tests.rs` beside the rung-18 precedent):
`RpcClient::dead_on_arrival` — a non-blocking `MSG_PEEK` on the pooled
session's socket (EOF = reaped; unsolicited readable bytes on a
request/reply wire = desynchronized; `WouldBlock` = alive). Consumed at
the two pooled-checkout sites whose verbs get ONE attempt
(`PublishClient::ship`, `data_grant::call_once_on`): a session proven
dead BEFORE the send is replaced at zero attempts. The law is
unchanged — one-attempt refusal exists for the TRUE ambiguity (a frame
SENT whose reply was lost), and that arm refuses verbatim; the
resend-safe classes keep their rung-18 reconnect.

## The stale vehicle census (fixed in the same branch)

The first from-zero pass at width 8 ran BOTH phases clean and then
refused as an INVALID ROW: the leg's write-vehicle accounting read
`write_through_bytes` ≈ 0 on every member *including the plain-writer
authority*. The **device overlay** (`SQUEEZEFS_DEVICE_OVERLAY`,
default ON — the B2 one-path write store) replaced complete-block
write-through as the fresh-write vehicle after the leg was written. The
fresh column is now `write_through + overlay_store − overlay_overwrite`
(labeled `fresh_MiB_d`; the overwrite-arm subset rides the rewrite
column's plane), under which the row's bytes close exactly
(1020 fresh + 515 rewrite of 1536 written per member). The
`SQUEEZEFS_DEVICE_OVERLAY=0` A/B posture keeps its old accounting
verbatim. (`tests/run_mw_matrix.sh` only; the pv legs' shared column
table still REPORTS raw `write_through_bytes` — a true statement of that
counter, not a gate.)

## Gates

- Red tests first (both RED with the live error verbatim, rung-18
  precedent green beside them), fix, suites: mw_cowriter_lane 26/26,
  cluster_wire 23, dlm_cowriter 18, mw_cowriter_free 17,
  dlm_multi_writer 16, mw_publish_era_gate 5.
- Full `task check` from zero on a quiet box: PASS (an earlier gate
  attempt failed in `il_direct_write_tests` because the fleet teardown's
  daemon sweep ran DURING the gate and killed the suite's test daemons —
  operator error, both suites clean in isolation and on the quiet
  re-run; nothing else may touch the box while the gate runs).
- Zero-residue teardown after the row, twice (first pass + the green row).

## Residual board effect

`docs/design-full-multi-writer.md` residual **item 5 is RETIRED** by this
row: the width-N family's remaining live shape is green from zero at
width 8 on the current binary. What remains open in that family is
capacity arithmetic, not a wedge: the funnel's cost at K > 8 stays
unmeasured (this venue's per-member rates are box-bound, not
funnel-bound), and the per-volume program's K-owner funnel row (leg (c),
0.92× at K = 2) is the standing instrument for it.
