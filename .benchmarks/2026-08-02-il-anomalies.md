# 2026-08-02 — il-path anomalies: both prime suspects EXONERATED by count; the real terms named; the perf table made fair

Branch `fix/exa-perf-and-il-anomalies` (off dev tip `29e964d`,
**unmerged — the orchestrator merges**). Charter: name the il write
sever-ACK anomaly (~1.2 ms vs the §5.5.2 cost model) and the il
randread deficit (+110 µs/op vs kernel) with counted A/Bs; fix the
exa_client_perf fairness flaws. Field window 2026-08-01T21:5xZ–22:2xZ,
journaled SESSION START/END; artifacts `/scratch/tmp/ilfix/rows/` +
`/scratch/tmp/ilfix/v2smoke/`.

## 0. Venue (labeled once — DIAGNOSIS epoch, no baseline exists)

squeeze-test (32 CPU / 2 NUMA / dual-200GbE), **5-wide post-oss2
epoch**: 10 × 48 GiB data ns (`nvme{4..22}n1` even) over nvme-tcp,
meta `nvme{0,2}n1`, cache-less, 480 G store. Pair `109d7bc` both sides
(derived arena cap — no `SQUEEZEFS_IPC_MEM_MAX`). Fill: the USER's own
fileset (`exa_perf/` 16 × 1 GiB + `f1..16`) — the charter's numbers
came from their run; A/B rows are REWRITES of that set, so absolute
numbers age across the sequence (`block_free_reclaim_cap_parks` ≈ 13 k
per write row) and every verdict below is an adjacent-leg comparison,
never a cross-sequence absolute. Instrument fio-3.36 via
`run_fio_row.sh`; 30 s + 10 s ramp rows; env A/Bs via standard
redeploys of the SAME pair (default env restored after; verified).

## 1. Anomaly 1 — il write ACK ~1.2 ms: the service-thread ceiling is EXONERATED

The A/B the charter ordered (`SQUEEZEFS_IPC_SERVICE_THREADS` 8/16/24,
psync 1M nj32 shim writes, svc-thread pidstat per row):

| leg (order) | svc threads | GB/s | clat | svc busy (pidstat, per thread) | ipc_service_parks |
|---|---|---|---|---|---|
| A1 | 8 (default) | 36.61 | 0.879 | **4.4–8.0 %** | 321 k |
| B1 | 16 | 36.43 | 0.884 | 0.3–17.1 % | 807 k |
| C1 | 24 | 35.75 | 0.899 | 0.6–18.8 % | 1.08 M |
| A2 (close) | 8 | 30.99 | 1.045 | 1.2–3.2 % | 363 k |
| D (nj64, 8 thr) | 8 | 31.43 | 2.060 | 14.6–19.5 % | 213 k |

* **The ceiling is not the term**: at the default 8, threads run
  4–8 % busy (never saturated); widening to 16/24 moves NOTHING
  (36.6 → 36.4 → 35.8, adjacent-leg noise) while parks GROW (more
  threads, same work). The [2,16] clamp constant is NOT convicted — no
  derivation fix ships.
* **Sever-path health clean**: `ipc_severed_pool_misses` 32/row
  (≈ once per session at birth), placed adoptions ≡ merge elides
  (~320 k), `write_pipeline_admission_waits` 3–10 k/row (amortized),
  NUMA local ≡ bytes in.
* **The ~1.2 ms named**: the cost model's premise was wrong — the ring
  write ACK is NOT sever-bounded. `serve_write` severs at dequeue, but
  `completion.complete` fires after the FULL write handler (lease +
  merge + admission), so ACK ≈ in-handler residence at the offered
  load, and the rows Little-close exactly: 32 ÷ 0.879 ms = 36.4 k ops/s
  ≡ 36.6 GB/s (A1); the nj64 leg doubles in-flight and clat doubles
  (2.06 ms) at flat delivery — textbook drain-bound. The user's 1.214 ms
  / 26.75 GB/s row is the same law at their store state.
* **Parity verdict**: il writes ≥ kernel at matched venue state
  (A1 36.61 vs the kernel reference 32.24 GB/s libaio qd8 nj32; even
  the aged A2 leg at 30.99 sits at kernel-class). The apparent anomaly
  was store aging between the user's kernel and il rows, not a path
  tax. No build-class residual filed for writes.

## 2. Anomaly 2 — il randread deficit: the reap park-max is EXONERATED; the term is WARMTH ASYMMETRY

**The lever bracket** (fio libaio 4k nj32 shim; `SQUEEZEFS_IL_REAP_PARK_MAX`
client env — regime flip PROVEN by `ipc_cqe_wake_writes`):

| leg (order) | qd | park_max | kIOPS | clat | cqe wake writes / elided |
|---|---|---|---|---|---|
| K (kernel ref) | 16 | — | 264.3 | 1.819 | — |
| A1 | 16 | 2 (default) | 225.8 | 2.267 | 20 / 8.97 M |
| B1 | 16 | 16 | 223.8 | 2.287 | **7.74 M** / 1.16 M |
| B2 | 16 | 16 | 225.4 | 2.271 | 7.84 M / 1.12 M |
| A2 | 16 | 2 | 223.1 | 2.294 | 13 / 8.87 M |
| A/B | 32 | 2 / 64 | 230.1 / 226.2 | 4.448 / 4.526 | flip proven |
| A/B | 1 | 2 / 16 | 161.6 / 162.9 | 0.197 / 0.195 | (qd1 already event-driven) |

**Flat at every depth with the regime demonstrably flipped** (event
parking re-enabled = 7.7 M wake syscalls, zero throughput change) — the
50 µs batch quantum is NOT the +110 µs, and the 2026-07-28 retune to 2
neither helps nor hurts this fleet shape (the herd cost and the batch
saving are both ≈ 0 here). No retune ships; the qd1 pair re-confirms
the latency contract the original threshold protected.

**The real term (composition rows, per-30 s deltas):**

| | il (223 k IOPS, clat 2.29) | kernel (249 k, clat 1.93) |
|---|---|---|
| device-true ranged serves | 8.80 M (`ipc_direct_drive_serves`, DMA into arena — `read_dest_dma_bytes` ≡ 36.0 GB exact) | 9.65 M (into ring ent, 39.5 GB) |
| warm RAM serves | **72 k** (sync fast-path hot) + 11 tier | **82 k hot + 294 k** hold/tier serves (`read_copy_dest_bytes` 1.21 GB ≡ 294 k × 4 KiB exact) |
| whole-block fills | ~900 (`fill_dma` 3.8 GB) | ~909 (3.8 GB) |
| tier admissions | **597** | **206 k** (hold-serve re-landing ceremony: ~909 held 4 MiB blocks × ~226 4 KiB serves each) |
| governor denials | 7.80 M | 8.24 M |

Both paths escalate ~900 whole blocks under the governor trickle. The
KERNEL path then serves ~294 k follow-on 4 KiB reads (~3 % of ops) from
the read-lane HOLD + hot tier at ~µs — the handler's probe ladder runs
before the ranged dispatch. The IL path's DIALED P1.5 direct-drive
serves governor-DENIED misses device-true from the service thread and
**never probes the read-lane hold** — its only warm source is the sync
fast path's hot probe (72 k). ~220 k missing µs-class serves per row ≈
the whole −8 %/+110 µs gap (both rows Little-close: 249 k × 1.93 ms ≈
480 in-flight ≡ 223 k × 2.29 ms ≈ 511).

**Disposition: FILED as a build-class residual** (not a retune): teach
the direct-drive prelude (`ipc_direct_read_probe` / the P1.5 ladder) a
latch-free read-lane-hold probe before submitting device-true — the
hold's `serve_with_provenance` is already lock-free and the §5.5.1
serve legs are the precedent, but the prelude runs on a foreign service
thread and the hold serve carries the R1b ledger ceremony (an async
arm), so it needs its own red-first contracts + field bracket. Expected
win is bounded ≈ the gap (~8 % on this row); evidence = the composition
table above.

## 3. The exa_client_perf fairness fixes (shipped, `d4542e3`)

The two table flaws convicted and fixed, visible by construction:

1. **Matched offered concurrency**: psync shim passes scale njobs to
   `njobs × qd` by default (32→256 on the BW rows); the per-pass
   **in-flight figure prints in the table** (`--no-match-inflight`
   restores raw njobs, still labeled). The old table compared 32
   in-flight psync against 256 in-flight libaio and called it a path
   delta.
2. **Cold-read discipline**: `--allow-remount` = cold-by-remount
   (captured daemon cmdline + `SQUEEZEFS_*` env, SAME binary,
   restore-verified: commit unchanged + transport armed) — proven live
   in the smoke; cold-by-overflow auto-detected (set ≥ 2× R5
   `mem_budget_bytes`); anything else prints **`WARM (label-only)`**,
   never a bare number.
3. The 1M-libaio shim engine law is now MEASURED, not assumed: a probe
   pass this window read **engagement 0.000** (31.96 GB/s of pure
   kernel-lane traffic wearing a shim label) — exactly the
   silent-passthrough class the verdict column exists for.

**The corrected kernel-vs-il verdict on this venue** (v2 smoke,
matched in-flight 256/256, cold-remount reads, 12 s SMOKE-labeled —
directionally informative, not baseline-grade): write_bw 31.87 k /
30.18 il psync-256 (−5.3 %; the 30 s diagnosis rows at nj32 read il
AHEAD 36.6 vs 32.2 — the psync-256 posture trades context-switch
pressure for concurrency, both labeled); read_bw 29.05 / 24.67
(−15.1 %, cold); randwrite 296 k / 428 k (+44.5 %); randread 231 k /
212 k (−8.3 %, cold — anomaly 2's term, §2). il parity law holds on
writes; the read-side gaps are the filed §2 residual plus the psync-BW
posture, both now labeled in the table instead of silent.

## 4. Gates + hygiene

Shell-only branch (no `.rs`/`.toml`) — no cargo gates owed; the
one-heavy-thing rule respected (nothing built on the dev box).
`bash -n` + shellcheck 0.11.0 clean. Field: env restored to default
(final redeploy verified: queues armed, spawned svc threads 0 =
session-less default posture; ceiling re-derives on next admission),
user's fileset restored (the matched-inflight psync-256 smoke pass had
ADDED sqzfio.16-127.0 — removed, journaled as a correction; the
original 16 files remain, content rewritten in place), helpers
+ artifacts under `/scratch/tmp/ilfix/`, SESSION START/END + every row
journaled. No resets, no reformats, no raw-device writes, no
storage-node changes.
