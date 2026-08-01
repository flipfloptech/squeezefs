# 2026-08-01 — Write in-handler economy: the 5.2 ms closed, the futile-work pair killed, and the wall's residual honestly re-pooled kernel-side

Branch `perf/write-in-handler` (off dev tip `076fe81`, **unmerged — the
orchestrator merges**). Charter: the transport-ingress campaign's #1
ranked successor (`.benchmarks/2026-08-01-transport-ingress.md` §9) —
Phase 1 attribute the ~5.2 ms IN-HANDLER leg of the EXA write wall to
named sub-phases with CLOSED residue before anything is built; Phase 2
build what the numbers name; rider re-measure the read fill-issue term
post-affinity-fix before anyone builds the doorbell.

Commits: red `5e70600` (in-handler sub-phase contracts) · green
`514a78e` (lease_acquire/extent_probe/admit_gate phases) · red
`4a5f441` (work-removal contracts) · green `946abc3` (sibling-hop
elision + cache-less spill short-circuit) · this note. Field windows
2026-08-01T06:0xZ–08:3xZ, journaled SESSION START/END + every row in
`/scratch/tmp/agent_runs.log`; artifacts `/scratch/tmp/wih_campaign/`.

## 1. Phase 1 — the instrument (shipped, `SQUEEZEFS_OP_PROFILE`-gated)

`fuse_write_phase_ns` grew three in-handler spans (12 keys total,
`tests/write_in_handler_phase_tests.rs` 3/3): **lease_acquire**
(`get_or_acquire_lease`, split out of route_classify),
**extent_probe** (the per-block `try_extent_park` call), and
**admit_gate** (the pipeline admission park awaited in-handler before
the completing write's ACK — the per-op twin of the always-on
`admit_wait`, reusing its start instant: no extra clock read). Rig-off
cost unchanged (one memoized load + branch;
`rand_write_rig_off_tests` still green).

## 2. Venue (labeled once; applies to every row)

Client squeeze-test (32 CPU / 2 NUMA / dual-200GbE), out of
production; substrate reset-v3 — nullblk 4-wide over **nvme-tcp**
(8 × 48 GiB data namespaces `nvme{4,6,8,10,12,14,16,18}n1`), meta
`nvme{0,2}n1`, cache-less format, 4 MiB blocks. Instrument fio-3.36
via `tests/fio/run_fio_row.sh` (NUMA fan-out, amp columns; every write
row carries device-÷-user amplification per the standing rule); EXA
write shape = `exa_write_bw.job` libaio 1M qd8 nj32, 60 s + 10 s ramp,
`rm` + settle (reclaim queue AND `meta_kv_pending_free` AND mem level
0 ×3) + fresh dir per leg; time_based ⇒ pass-1 fresh + ~6 rewrite
passes (the SUSTAINED mixed face). Histogram counts are ramp-inclusive
(totals ≈ 1.17× a 60 s window — the standing caveat); **means are
unaffected and are what the tables cite**. Local scoping ran on
devsub-**tcp** (24-CPU box) but is instrument-verification only — that
venue is zram-write-bound at ~0.5 GB/s and cannot see the term.

Pairs (rocky8 container builds, KD-7 identity verified): window 1
instrument pair `514a78e` (= dev tip + phases, deployed `.wih`);
window 2 **C** = `946abc3` (`.wih`), **T** = dev tip `076fe81`
(`.tip076`).

## 3. Phase 1 — THE 5.2 ms, CLOSED (field row, OP_PROFILE=1)

Row `wih-attr-w1`: **32.13 GB/s, clat 8.248 ms, amp 1.131** (matches
the wall). 2,144,531 WRITEs; engagement exact (`write_transport` n ==
WRITE count; `nt_copy_bytes` 2.25 TB ≈ every merge byte NT-engaged —
the M1 copy is NT here, answering the charter question).

```
fio clat                 8.248 ms
└─ kernel-side           1.73   (clat − transport_total)
└─ transport_total       6.515
   ├─ queue_wait         0.646   ├ dispatch_lag 0.920   ├ reply_commit 0.006
   └─ handler            4.943  ≈ fuse_op write.total 4.931 (closes to 11 µs)
      ├─ prelude 0.001 · reply tail 0.009 (ACK fires ≈ immediately after
      │   handler return — in-place WRITE reply, no post-merge ACK gap)
      └─ backend         4.920 — THE 5.2 ms TERM, decomposed:
         ├─ admit_gate        3.065/op  (12.64 ms/block × 520k/2.14M) 62 %
         ├─ block_lock_wait   0.910/op  (stripe convoy @ write_checkout) 18 %
         ├─ sibling_remove    0.446/op  (spawn_blocking hop, UNDER the lock) 9 %
         ├─ merge_copy        0.217/op  (M1, NT — load-bearing) 4.4 %
         ├─ park_spill        0.151/op  (R5 pass; staging_put 0.110 ⊂ it)
         ├─ route_classify    0.092/op  (⊃ inode lock 0.088 + lease 0.001)
         ├─ extent_probe 0.022 · checkout 0.002 · seed_fetch 0.004
         └─ residue           0.011/op  (0.2 % — CLOSED)
```

Structural findings the numbers name:

1. **admit_gate (62 %) is CONSERVED queueing, not work** — the
   serve-decomposition's depth-pin A/B already proved opening the gate
   moves delivery ±2 % (w3: admit 10.3 ms → 4 µs, flat). NOT a build
   target; deliberately untouched.
2. **The staged-sibling `spawn_blocking` hop is pure waste on this
   venue**: 2.14 M blocking-pool round trips (0.446 ms/op mean at
   saturation, executed UNDER the held block lock — it also feeds the
   0.910 ms convoy term) for ZERO siblings found
   (`restage_churn_removes` 0). The RW1 H1 pin anticipated the flip.
3. **The cache-less spill engine is structurally futile**: 980 k
   staging puts REFUSED (`put_active_block` refuses unconditionally on
   empty `staging_dirs`) + 1,898 futile 4 MiB victim seed READS
   (~8 GB of device reads bought nothing) + victim locks held against
   live writers — driven by pipeline-resident parked custody
   (~230 blocks ≈ 0.93 GiB) riding above the 1 GiB parked cap.
4. ACK emission timing (charter question): the reply fires at handler
   return + 6 µs reply_commit — merge → admission park → detach spawn
   → attr/times publish → ACK. The only pre-ACK waits after the merge
   are admit_gate (conserved) and the ~1 ms of futile work above.

## 4. Phase 2 — the build (work removal, both engagement-gauged)

`tests/write_in_handler_economy_tests.rs` 5/5 (red-first):

1. **Sibling-hop elision** — the probe is now the latch-free occupancy
   index read; the `spawn_blocking` remove dispatches only when
   present. Index-absent is EXACT under the held block lock (the index
   is conservative-present — indexed before ring write, un-indexed
   strictly after removal — and every staging put site for a key holds
   that key's `BLOCK_FLUSH_LOCKS` guard). A planted sibling is still
   found and removed (contract 2 — one-authority law untouched).
   Gauge: `staging_sibling_hops_elided`.
2. **Cache-less spill short-circuit** — `spill_parked_toward_cap`
   returns immediately on cache-less volumes (format-time-immutable
   fact); the cap stays soft exactly as the refusal path always left
   it; the R5 Red DURABLE self-flush (not a staging arm) untouched.
   Gauge: `spill_pass_cacheless_skips`.

No lock-free core changed (no loom owed); zero-copy/latch-free laws
and lock order §P1-9 untouched; no new locks, no constants.

## 5. The counted A-B-B-A (EXA write, binary-vs-binary, unprofiled)

| leg (order) | GB/s | clat | p99 | write_amp | engagement |
|---|---|---|---|---|---|
| T1w | 31.31 | 8.473 | 78.1 | 1.122 | probes 2.10 M, `spill_seed_reads` 2,883 |
| C1w | 32.48 | 8.164 | 77.1 | 1.122 | elided == probes == 2,177,119; skips 1.07 M; seeds 0 |
| C2w | 32.37 | 8.189 | 68.7 | 1.146 | elided == probes == 2,166,434; skips 790 k |
| T2w | 31.84 | 8.334 | 88.6 | 1.090 | probes 2.14 M, seeds 1,147 |

Side medians **32.43 vs 31.58 GB/s = +2.7 %**, clat −3.2 %,
order-independent (C > T in both adjacent pairs). Engagement EXACT on
every C leg. **Sustained headline (the ≥120 s law):** C flat row
**32.27 GB/s over 120 s**, per-10 s device-byte series flat (first
third 312.6 vs last third 318.8 GB/10 s — no decay); T 120 s reference
31.57 flat (+2.2 %). (One sequencing slip journaled: the first flat120
ran on T's mount — kept as the T reference, C re-run from remount.)

### 5.1 The after-table (C profiled row `wih-attr-w2`: 32.39 GB/s, clat 8.181)

| term (ms/op means) | before | after |
|---|---|---|
| backend (in-handler total) | 4.920 | **3.880** (−1.04) |
| sibling_remove | 0.446 | **0.001** (killed) |
| park_spill (+staging_put ⊂) | 0.151 | **0.001** (killed; puts 980 k → 40, seeds → 0) |
| block_lock_wait | 0.910 | **0.538** (−0.37 — the in-lock hop fed the convoy) |
| merge_copy (M1, NT) | 0.217 | 0.216 (load-bearing floor) |
| admit_gate | 3.065 | 2.820 (conserved, as predicted) |
| route_classify (⊃ inode lock) | 0.092 | 0.228 (inode-gate convoy absorbed wait) |
| residue | 0.011 | 0.040 (1.0 % — still closed) |
| kernel-side (clat − transport_total) | 1.73 | **2.58** (+0.85 — the conservation) |

**Little honesty (the charter demand):** −1.04 ms of in-handler time
was REMOVED and measured gone, but the row converted +2.7 %, not the
naive +4 GB/s/ms — the freed time re-pooled into the kernel-side
residue (+0.85 ms) and the inode-gate/classify (+0.14), exactly the
queueing-conservation face the write-wall w2 leg documented. The op
chain still Little-closes on every leg (256 × 1 MiB ÷ clat ≈
delivery). What converted is the ~work fraction (the hop + futile
spill + their convoy echo); what cannot convert by in-handler surgery
is now measured, not inferred: admit_gate 2.82 (conserved) + kernel
2.58 + transport ingress 1.68 = 7.08 of the 8.18 ms RTT.

## 6. No-regression set

| row | T | C | verdict |
|---|---|---|---|
| EXA write 60 s (A-B-B-A medians) | 31.58 | 32.43 | **+2.7 % WIN** |
| EXA write 120 s sustained | 31.57 flat | 32.27 flat | +2.2 %, no decay |
| fresh-ingest pass (32×2 g, pass-bound label) | 32.19 | 32.69 | +1.6 % |
| rand-4k write IOPS (medians of 3) | 325.5 k (316.2–327.6, 3.5 % spread) | 319.5 k (319.4–319.6, byte-stable) | −1.9 % on medians — par-band: C reps byte-stable, T reps disagree by 3.5 % (the transport campaign's warm-4k adjudication shape). Ledger note: mix shift, C classifies more writes `patch_ineligible_overlay` (8.78 M vs 7.84 M) with more overlays live at its higher accumulation rate; C also ran MORE total FUSE write ops; futile spill seed reads 62 → 0 |
| read qd8 1M cold | 26.15 (window-1 rider, instrument pair) | 26.11 | par (amp 0.694 vs 0.691) — read law holds |
| read qd32 1M cold | 22.86 (transport-campaign anchor) | 22.72 | par (−0.6 %, amp 0.985) |
| il psync 1M nj32 | 39.07 / 36.40 GB/s ≥ kernel C 32.4 — parity law holds on raw numbers; **both cells labeled INVALID** (engagement 0.822 / 0.795 < 0.90): `ipc_bind_refused_budget` = 10 of 80 binds — the 32-process psync shape vs the session budget, a posture the transport campaign also recorded on the TIP side (0.759 INVALID) — not campaign-attributed (the diff touches no ipc surface); flagged as a standing venue item |

Loaded soak (the merge-bar leg): C pair, **600 s** fio write 1M qd8
nj16 time_based + 8-worker metadata storm (create/write-4k/stat/
rename/unlink + dir churn) + `syncfs` every 10 s; wedge indicators
sampled every 30 s (`fuse_op_watchdog_overdue`,
`transport_lease_overlong`, `tpc_lane_redispatches`,
`ipc_sessions_poisoned`, `writer_guard_fenced`,
`ipc_descriptor_rejects`, `write_pipeline_fence_drops`,
`block_free_reclaim_fence_halts`) — result recorded in §8.

## 7. Rider — read fill-issue re-measure (report-only verdict)

Same pair, cold remount, the decomposition's exact EXA cold-read shape
(gap_probe_read 1M qd8 nj32 on the 16×8 g fileset): **26.15 GB/s,
clat 10.235 ms, amp 0.691** (row itself +2.6 % over the decomp anchor
25.48 — the affinity fix's read gain, reproduced). The fill table:

```
fill_total 8.199 ms          (decomp anchor: 7.77)
├─ dev_service 5.480         (4.70 — deeper device load at the higher row)
├─ dev_queue   1.607         (1.37 — submission-side slot/channel wait)
├─ wake residue 1.025        (1.63 — fetch_dma − dev_queue − dev_service)
└─ decode/admission/deposit 0.087
```

The 3.0 ms issue-side term is now **2.63 ms**, and the leg the
established `cqe_core` doorbell pattern could address — the oneshot
completion wake — **shrank 1.63 → 1.02 ms** (the transport campaign's
§9 suspicion confirmed: those wakes rode the un-pinned lanes). Under
the charter rule (build only if ≥ 2 ms AND the fix is the doorbell
pattern): the doorbell-eligible slice is 1.0 ms ⇒ **report-only, do
not build**. The larger residual is `dev_queue` (submission-side, a
different mechanism — wider worker submission), ranked in §9.

## 8. Loaded-soak result (PASS)

C pair, **600 s**: fio write 1M qd8 nj16 time_based + the 8-worker
metadata storm + `syncfs` every 10 s. Result: **31.41 GB/s sustained
for the full window** (clat 4.174 ms, p99 18.2 ms); every wedge
indicator **ALL-ZERO across all 21 30-s samples**
(`fuse_op_watchdog_overdue`, `transport_lease_overlong`,
`tpc_lane_redispatches`, `ipc_sessions_poisoned`,
`writer_guard_fenced` [0,0], `ipc_descriptor_rejects`,
`write_pipeline_fence_drops`, `block_free_reclaim_fence_halts`);
zero non-kernel D-state processes after quiesce; all 8 storm workers
alive; clean rc = 0; the subsequent umount + standing-pair remount
clean.

## 9. Residuals, ranked (for charter revision)

1. **Kernel-side residue, now the largest term: 2.58 ms/op** (K1
   FUSE-ingress copy at 32 GB/s + kernel request queueing above the
   256-slot appetite). Kernel-interface class — per the charter this
   is a STOP-on-term: document, do not build. Priced by the
   near-zero-copy census; any movement needs max_pages/payload
   geometry or protocol work.
2. **admit_gate 2.82 ms/op conserved queueing** — displaceable only by
   a faster drain, i.e. the in-pipe legs BEHIND the ACK: publish
   6.36 ms/block (coalesce 2.4 vs cap 64 under sustained rewrite — the
   write-commit-economy §6.4 surface) and dma 5.8 ms/block (reclaim
   discard contention). The decomposition ranked this #4; it is now
   the write wall's top TRACTABLE term.
3. **Transport ingress 1.68 ms/op** (queue_wait 0.77 + dispatch 0.92)
   — the transport campaign's counted probes bounded its EXA-shape
   payoff at ≈ 0; unchanged.
4. **Block-lock convoy 0.54 ms/op residual** — now mostly the merge
   serialization itself; parallel disjoint merges (placed-sever-style
   shared assembly for the kernel path) is the only further cut, and
   it fights the §5.4 lease-severance law for leased payloads — not
   recommended without a design round.
5. **Read fill dev_queue 1.6 ms** (rider) — NvmeBlockDev submission
   width/channel economy; the doorbell-pattern wake leg is 1.0 ms and
   below the build bar.
6. rand-4k mix shift (−1.9 % par-band) — watch on the next scoreboard
   pass; the overlay-ineligible share is throughput-coupled.

## 10. Client state

Standing mount (`b4edafc` pair) restored + verified at SESSION END;
campaign pair retained at `/scratch/tmp/{squeezefs,libsqueezefs_il.so}.wih`
(`946abc3`), tip pair at `.tip076` (`076fe81`); window-1 instrument
binary superseded by `.wih` (journaled). Artifacts (per-row
job/json/stats before-after/meta, flatness series, phase tables, soak
indicators) under `/scratch/tmp/wih_campaign/`; analyzer at
`/scratch/tmp/fio_wih/phase_means_wih.py`. Bench filesets `wr_wih/`,
`soak_wih/` left on the store (normal bench artifacts); `rw4k_wih/`,
`fresh_wih/`, `wr_wih_il/`, `mdstorm_wih/` removed. No
resets, no reformats, no raw-device writes, no storage-node changes.
Incidents journaled: one remount refused LOUD by the D0 writer guard
then retried clean (working as designed); the flat120 sequencing slip
(§5).
