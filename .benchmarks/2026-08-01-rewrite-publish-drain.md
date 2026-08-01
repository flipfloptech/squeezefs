# 2026-08-01 — Rewrite publish drain: the 7.7× tax attributed (commit-rate coupling, not rewrite mechanics), the commit machinery collapsed, the row honestly unmoved

Branch `perf/rewrite-publish-drain` (off dev tip `31c89c5`, **unmerged —
the orchestrator merges**). Charter: the write-in-handler campaign's #2
residual (`.benchmarks/2026-08-01-write-in-handler.md` §9) — Phase 1
decompose the **publish 6.36 ms/block rewrite vs 0.83 fresh** anomaly
into named constituents with CLOSED residue before anything is built
(five hypotheses died to instruments this month); Phase 2 build what
the numbers name; rider diagnose the il `ipc_bind_refused_budget`
posture.

Commits: red `fd2a0e6` (publish-decomposition contracts) · green
`19a1dde` (`publish_phase_ns` + base-provenance/full-save ledgers) ·
`11d8050` (meta_commit interior split) · `5ee266d`
(`meta_txpass_phase_ns` — the M7 pass interior) · red `48864bf`
(publish-drain economy contracts) · green `e2efe52` (Lever A
era-guarded RAM base + Lever B aggregated publish commits) · this
note. Field windows 2026-08-01T07:5xZ–11:0xZ, journaled SESSION
START/END + every row in `/scratch/tmp/agent_runs.log`; artifacts
`/scratch/tmp/rpd_campaign/`.

## 1. Venue (labeled once; applies to every row)

Client squeeze-test (32 CPU / 2 NUMA / dual-200GbE), out of
production; substrate reset-v3 — nullblk 4-wide over **nvme-tcp**
(8 × 48 GiB data namespaces `nvme{4,6,8,10,12,14,16,18}n1`, meta
`nvme{0,2}n1`), cache-less format, 4 MiB blocks, store at 51 % fill +
the 32 GiB row fileset (**aging store ⇒ A-B-B-A mandatory on every
rewrite comparison**). Instrument fio-3.36 via `tests/fio/run_fio_row.sh`
(NUMA fan-out; amplification columns on every write row per the
standing rule); EXA write shape = `exa_write_bw.job` libaio 1M qd8
nj32 60 s + 10 s ramp over 32 × 1 GiB files — time_based ⇒ every pass
after the fileset exists is a REWRITE pass. Settle hygiene before
every row: `block_free_reclaim_queue_bytes = 0` AND
`meta_kv_pending_free = [0,0]` AND `mem_budget_level = 0`, observed
×3 at 1 Hz (`/scratch/tmp/rpd/settle.sh`). Raw write ceiling on this
fabric: 49.7 GB/s NIC-line-rate
(`.benchmarks/2026-07-31-raw-write-ceiling-resweep.md`).

Pairs (rocky8 container builds, KD-7 verified): instrument pairs
`19a1dde` → `11d8050` → `5ee266d` (Phase 1, deployed `.rpd` in
sequence); **C** = `e2efe52` (`.rpd`), **T** = `/scratch/tmp/*.wih`
(`946abc3` — code-identical to dev tip `31c89c5`, which is docs-only
on top of it).

## 2. Phase 1 — the instrument (shipped, ALWAYS-ON)

Three families (the `write_pipeline_phase_ns` cost contract — one
`Instant` read + one relaxed `fetch_add` per boundary; ungated on the
stats inode), `tests/publish_phase_tests.rs` 5/5:

* **`publish_phase_ns`** — the pipeline `publish` span decomposed:
  `queue_wait` (per op: ino-conveyor residence) → `lock_wait` /
  `base_fetch` / `apply` (per pass) → `save_encode` / `blob_write` /
  `meta_commit` (per publish-class save) + the meta_commit interior
  `commit_guard` / `commit_inode_read` / `commit_slot_probe` /
  `commit_tx_wait` → `total` (per op).
* **`meta_txpass_phase_ns`** — the M7 journal-conveyor pass interior:
  `tx_queue_wait` (per tx) + `pass_admission` / `pass_leaf_locks` /
  `pass_journal_write` / `pass_total` (per pass).
* **Ledgers**: `publish_base_{dirty_serves,fetches,ram_serves}` (the
  base-provenance ledger — closes EXACTLY against the pass count),
  `publish_full_save_{indirect,chain_cap,other}` (why a publish-class
  save fell off the O(batch) delta path),
  `layout_indirect_map_read{s,_bytes}` (whole-block map rehydrates),
  `publish_indirect_blob_bytes`.

## 3. Phase 1 — THE TAX, CLOSED (field, EXA rewrite, tip-code instrument pair)

Row `rpd-attr-rw3` (**32.44 GB/s, clat 8.173 ms, amp 1.149** — matches
the wall; 533,333 blocks, 327,532 publish passes, residue closed):

```
publish/block            4.796 ms   (the "6.36" term at this venue's shape)
├─ queue_wait            1.923      (ino-conveyor residence — echo of pass latency)
└─ own pass              ~2.4
   ├─ base_fetch         0.172      (backend refetch EVERY pass — ledger: fetches
   │                                 == passes, dirty/ram serves == 0)
   ├─ apply/save_encode  0.026
   └─ meta_commit        2.227  ⊃ commit_tx_wait 2.198 (98.7 %) =
      ├─ tx_queue_wait   0.741      (M7 conveyor queueing)
      ├─ pass_total      0.777      (journal_write 0.599 ⊃ ~0.3 device;
      │                              leaf_locks 0.174; admission ~0)
      └─ wake residue    ~0.68      (fan-out onto the saturated runtime)
      └─ guard/iread/slot 0.023
```

The discriminators that CLOSED the attribution:

1. **NOT the journal device**: meta namespaces at 0.22–0.44 ms
   `w_await`, 390–630 w/s, **1.25–1.75 % util** during the row.
2. **NOT rewrite mechanics**: the low-load probe (`rpd-attr-rwlow`,
   nj8 qd2 — SAME rewrite shape, 30.30 GB/s) runs publish at
   **0.287 ms/block** (commit_tx 0.153, queue 0.048). The 7.7× tax is
   **commit-rate-coupled queueing/scheduling at saturation**: the M7
   conveyor pass is a serialized ~0.78 ms server at **ρ ≈ 0.92**
   (166 k passes / 70 s across 2 volumes) while the client CPU runs
   ~94 % busy (usr 30 / sys 49 / iowait 15) — every wake hop in the
   commit chain (uring completion → pass task → publish pass →
   upload task) inflates to ~0.3–0.7 ms.
3. **The rewrite-vs-fresh asymmetry is the BASE PROVENANCE**: fresh
   streams publish on the dirty-RAM authority
   (`publish_base_dirty_serves` == passes) — rewrites refetched from
   the backend EVERY pass (`publish_base_fetches` == passes): a
   getxattr that folds the ino's ENTIRE unrebased delta chain
   (`layout_delta_folds` 6.32–6.44 M/row ≈ **19.5 folds per pass**)
   plus a full map decode + clone — and the refetch RESET the
   caller-half chain accounting to 0, so the on-disk chain **never
   re-based** (`publish_full_save_chain_cap = 0` under rewrite vs 99
   on the fresh row).
4. Indirect-map arms structurally quiet at this shape (1 GiB files =
   inline maps): `blob_write` = 0, `layout_indirect_map_reads` = 0 —
   the instrument stands ready for the >6 GiB-file venue where they
   are the predicted dominant face.

Fresh contrast (`rpd-attr-fresh2`, pass-bound burst label): publish
0.34 ms/block — meta_commit 0.284 (commit_tx 0.248, journal_write
0.111), queue_wait 0.015, base 100 % dirty-RAM serves.

## 4. Phase 2 — the build (both engagement-gauged, red-first)

`tests/publish_drain_economy_tests.rs` 7/7 (+ the Phase 1 contracts
moved with the law, as their red text promised):

1. **Lever A — era-guarded RAM-coherent publish base.** The coalesced
   publish pass serves its RMW base from a CLEAN cached entry whose
   new `layout_base_token` equals the ino's CURRENT fencing token
   (stamped by `fetch_metadata_from_backend` and by every save's
   republish; every layout mutation republishes the cache under
   `INODE_META_LOCKS`, so a same-era clean entry is coherent by
   construction). Foreign-era/unknown entries refetch exactly as
   before — the lease-loss stale-map hazard stays closed (pinned:
   `foreign_era_ram_base_is_refused_and_refetched` plants a poisoned
   foreign-era map and proves it never leaks). Kills the per-pass
   fold/decode/clone storm AND restores the chain-cap re-base cadence
   (`rewrite_chain_rebases_at_the_cap`).
2. **Lever B — per-volume aggregated publish commits.** Delta-class
   layout saves enqueue on a per-volume layout-merge conveyor
   (`ConveyorCore` reuse; leader-elect detached pass; M7 lifecycle:
   Weak between batches, panic guard fails loud, enqueue-then-elect
   no-lost-wakeup). One pass = `lock_many` over the batch's inos
   (deduped ascending — the `destroy_inodes` precedent, deadlock-free
   by ordering) + ONE multi-ino KvTx + ONE `commit_tx`: one journal
   entry, one ring write, one fan-out per drained window instead of
   per ino. Whole-tx atomicity per ino preserved (each ino's size+map
   ride one record set inside one checksummed entry — the generic/795
   law, pinned through drop-without-shutdown replay); per-op isolation
   preserved (a NotFound member fails ALONE with the never-lossy
   ladder's classification — `aggregated_batch_member_fails_alone`);
   drain caps derive from the existing
   `SQUEEZEFS_META_COMMIT_BATCH_{TXS,BYTES}` (no new constants);
   **`SQUEEZEFS_PUBLISH_COMMIT_GROUP_MAX=1` is the A/B lever** (the
   pre-campaign per-save path, verbatim — pinned byte-identical).

No lock-free core changed (`ConveyorCore` reused, not modified; new
counters are relaxed atomics) — no loom owed. Lock order: 4a
`lock_many` ascending-deduped before 4b leaf locks; the layout pass
takes no `INODE_META_LOCKS`, the M7 pass takes no 4a guards — the
lock populations stay acyclic (§P1-9).

## 5. The counted A-B-B-A (EXA rewrite, aging store, both orders)

| leg (order) | GB/s | clat ms | p99 ms | write_amp | engagement |
|---|---|---|---|---|---|
| C1 | 32.49 | 8.145 | 43.3 | 1.162 | ram_serves 266,111 / fetches **16** / dirty 0; groups 123,171 carrying 262,042 saves (**2.13 saves/commit**); chain-cap re-bases 4,085 ≈ deltas/64 EXACT; journal entries **131,089** |
| T1 | 32.67 | 8.114 | 73.9 | 1.152 | fetches-only base (pre-campaign); folds 6,439,907; journal entries 332,615; chain-cap 0 |
| T2 | 32.57 | 8.139 | 63.2 | 1.162 | — |
| C2 | 32.66 | 8.119 | 81.3 | 1.136 | ram_serves 351,110 / fetches 16; groups 167,180 / saves 345,732 (2.07); chain-cap 5,394; journal entries 176,632 |

**Throughput verdict: PARITY, order-independent** (C med 32.58 vs T
med 32.62, −0.1 % — inside the venue's leg spread). **The TERM
collapsed and the ledgers prove it on every C leg, engagement exact:**

| term (per-block/per-save means) | T (tip) | C (campaign) |
|---|---|---|
| `commit_tx_wait` (the named dominant) | 2.198 ms | **0.70–0.97 ms (−56…−68 %)** |
| journal entries / row | 332,615 | **131,089–176,632 (−47…−61 %)** |
| delta fold applies / row | 6.44 M | **1.86–2.41 M (−63…−71 %)** |
| publish base backend fetches | == passes (328,987) | **16** (first-touch only) |
| chain-cap re-bases | 0 (chain unbounded) | deltas/64, EXACT |
| publish/block (leg-dependent equilibrium) | 4.80–5.17 | 3.93–7.75 (see below) |
| admit_wait (leg-dependent equilibrium) | 17.1–18.8 | 5.2–19.4 |

**Little honesty (the charter demand, stated plainly):** the
commit machinery got measurably ~2× cheaper and the depth governor
did open the gate by derivation on legs where the equilibrium settled
that way (C1: admit 18.8 → 5.2 ms, in-pipe depth grew ~100 → ~155
blocks) — and the ROW DID NOT MOVE, in either order. The freed drain
time re-pooled into the device leg and the wider publish windows
(C1: dma 5.4 → 6.7, publish-span 4.6 → 8.5 at constant delivery;
C2 settled at a T-like equilibrium with publish 3.93 and admit 19.4 —
same 32.6 GB/s both ways). The row's binder is now measured, not
inferred: **data devices at 85–89 % util / aqu 3.3–3.7 with
write_amp 1.13–1.16** (user 32.5 ⇒ device ~37.5 of the 49.7 raw
ceiling) **plus client CPU at ~94 %** (usr 30 / sys 49 / iowait 15).
A qd16 probe (C, 31.46 GB/s at clat 16.06) confirms offered-load
scaling converts nothing. Per the in-handler campaign's re-pooling
lesson this is queueing conservation working as designed — the
publish drain is simply no longer the top tractable term (§9).

**Trade-off, stated honestly:** Lever A's restored re-base cadence
buys bounded on-disk chains (cold-read fold chains ≤ 64, fold applies
−65 %) at the price of periodic O(map) full saves — C journal BYTES
rose (73 → 93–118 MB/row) and meta-node writeback rose (204 → 253–
313 MB/row) while journal ENTRIES halved. Both planes stay ≪ 2 % meta-
device util; the CPU-side fold economy is the paying face at this
file size, and the byte face inverts on the big-file venues where the
unbounded chain is the catastrophic side (every reader folds the
whole chain).

### 5.1 Sustained + flatness (the ≥120 s law)

C sustained rewrite, 120 s + 10 ramp: **32.42 GB/s, flat** — per-10 s
device-byte series 314.8–329.1 GB with first-third 1,286.0 vs
last-third 1,290.8 (no decay). Amplification 1.082 on the row.

## 6. No-regression set (C vs T, same procedure both sides)

| row | T | C | verdict |
|---|---|---|---|
| EXA rewrite A-B-B-A | 32.62 med | 32.58 med | **parity** (the honest headline) |
| fresh-ingest pass (32×1g, pass-bound burst label) | 31.79 | 31.67 | par (−0.4 %); amp 0.551/0.556 (pipeline still draining at snapshot — label-only row) |
| rand-4k write (fresh dir, 30 s) | 341.4 k IOPS | 338.3 k | par (−0.9 %); amp 0.944/0.946 |
| read 1M qd8 cold (32×1g fileset, fresh remount) | 26.34 | 26.38 | **par** (amp 0.714 both — the read law holds) |
| tripwires (every C leg) | — | `write_path_seed_read_bytes` 0 · `patch_edge_rmw_reads` 0 · `write_pipeline_fence_drops` 0 · `block_free_reclaim_fence_halts` 0 · `sync_drains` 0 | clean |

## 7. Loaded soak (the merge-bar leg)

C pair, **600 s**: fio write 1M qd8 nj16 time_based + the 8-worker
metadata storm (create / 4k-fsync-dd / stat / rename / unlink + dir
churn) + `syncfs` every 10 s; wedge indicators sampled every 30 s
(`fuse_op_watchdog_overdue`, `transport_lease_overlong`,
`tpc_lane_redispatches`, `ipc_sessions_poisoned`,
`writer_guard_fenced`, `ipc_descriptor_rejects`,
`write_pipeline_fence_drops`, `block_free_reclaim_fence_halts`).
Result: **PASS — 31.62 GB/s sustained for the full window** (clat
4.146 ms, p99 18.2 ms — matches the wih soak reference 31.41); every
wedge indicator **ALL-ZERO across all 21 30-s samples**; all 8 storm
workers alive at stop (~16.3 k iterations each); zero non-kernel
D-state processes after quiesce; row rc = 0; amplification 0.999;
the subsequent standing-pair remount clean (over-uring armed,
transport_queues = 32).

## 8. Rider — il bind-budget diagnosis (VERIFIED, report-only)

**The binder is the R5 `ipc_session_arenas` admission cap doing its
design-intent job, not a mis-derivation.** Arithmetic on this venue:
mem budget 176 GiB ⇒ cap = min(budget/8, **2 GiB ceiling**) = 2 GiB
(`IPC_ARENA_CAP_CEILING`); session footprint = 64 MiB default arena +
ring/slots ≈ 64.3 MiB ⇒ **~31 concurrent sessions**; the 32-process
psync fleet at `il_sessions_default = clamp(32/4,2,16) = 8` shards
demands ~48 HELLOs. Reproduced on the pair (default mount): 48 binds
→ **13 refused** (`ipc_bind_refused_budget`), engagement 0.663
(INVALID label — exactly the standing venue posture). The per-uid
session cap (64) is NOT the binder; the shed path never engaged.

**Verified posture** (the existing lever, no code change): remounting
with `SQUEEZEFS_IPC_MEM_MAX=8192` lifted the cap — same row: 48/48
binds admitted, `ipc_bind_refused_budget` **0**, engagement 1.156
(≥ 0.90 VALID). Recommendation for this venue's il rows: mount with
`SQUEEZEFS_IPC_MEM_MAX=8192` (or run fleets at
`SQUEEZEFS_IL_SESSIONS=1` — 32 × 64 MiB fits under the default cap).
Charter note (not this campaign): the 2 GiB ceiling is one of the two
remaining fixed constants in the arena budget (`transport_buffer_cap`
shares the shape); if large-RAM il fleets become a standing venue, the
ceiling deserves a derivation. Not trivially fixable here without a
design round on the DoS posture it implements — report-only.

## 9. Residuals, ranked (for charter revision)

1. **The EXA rewrite row is now data-plane + client-CPU bound**
   (devices 85–89 % util at amp ~1.15; CPU ~94 % with sys 49 %):
   client-side drain surgery has no further conversion currency at
   this shape. The tractable remainders are CPU-side (the sys-heavy
   wake/softirq economy) and amplitude-side (write_amp 1.13–1.16 —
   displaced-block CoW + reclaim discards ride the same fabric; the
   in-place-overwrite lever exists for substrates where slot-replace
   is cheap, measured wrong for zram targets).
2. **The commit chain's wake hops** (~0.68 ms fan-out residue + ~0.3
   submission/completion inside `pass_journal_write` at saturation) —
   the doorbell/pinned-lane class; now amortized 2× by Lever B but
   still the largest per-commit term. Build bar per the read-fill
   precedent: ≥ 2 ms and a proven pattern — currently below it.
3. **Aggregation factor 2.1 is arrival-limited** (batch = what queues
   during ~0.9 ms commits at ~2.4 k commits/s). It self-scales with
   load (the jbd2/no-timer law); forcing it higher needs timers —
   banned, and pointless while the row binds elsewhere.
4. **`displaced_free` 0.95–1.78 ms/block** in-pipe (reclaim ENQUEUE
   bookkeeping under contention, not commands — `cap_parks` grew on C
   legs). Worth a look if the data-plane wall moves.
5. **The indirect-map faces** (`blob_write`, `layout_indirect_map_
   reads`) are instrumented but structurally quiet at 1 GiB files —
   they are the PREDICTED dominant publish face for >6 GiB files
   (per-pass whole-blob rewrite + 4 MiB rehydrate read). Measure
   before building; the venue needs a big-file fileset.

## 10. Gates

- clippy `-D warnings` PASS; fmt PASS (root; fuse3 untouched).
- Targeted suites (merge bar): `publish_drain_economy_tests` (7),
  `publish_phase_tests` (5), `publish_coalesce_tests` (6),
  `write_commit_economy_tests` (2 — call sites moved to `Bytes`),
  `write_commit_crash_tests` (kill-9 soak), `write_through_coverage_
  tests` (8), `write_pipeline_tests` + `write_pipeline_phase_tests`,
  `layout_delta_fold_tests` (8), `inplace_overwrite_tests`,
  `indirect_map_backend_keys_tests`, `conveyor_tests` (10),
  `fsync_coalescing_tests` (25), `crash_contract_tests`,
  `extent_{patch,overlay}_tests`, read family (`read_serve_phase`,
  `hot_block_tier`, `hybrid_io`, `read_admission_governor`,
  `read_lane`, `ranged_read`, `read_prefetch_pipeline`,
  `data_path_correctness`, `write_through_tests`) — ALL GREEN,
  `--test-threads=1`.
- `cargo test --all-features -- --test-threads=1` full suite from
  zero: **PASS** (exit clean; the run completed through its final
  phase — doc-tests — with zero failures; a failing binary aborts
  the serialized run before that phase, ~27 min on the throttled
  box).
- `cargo doc --no-deps`: builds; only the 4 pre-existing intra-doc-
  link warnings on dev-tip surfaces this branch does not touch
  (`SeveredPool`, `ipc_service` ×2, `AdmissionGovernor`) — none
  introduced.
- `cargo bench --benches -- --test` bench smoke: PASS.
- Loom: no lock-free core changed (`ConveyorCore` reused, not
  modified — verified by diff); not owed per the tier table.
- External POSIX suites: per the tier table these are the release
  gate, not per-PR; not run here.

## 11. Client state

Standing mount (`b4edafc` pair) restored + verified at SESSION END;
campaign pair retained at
`/scratch/tmp/{squeezefs,libsqueezefs_il.so}.rpd` (`e2efe52`).
Artifacts (per-row job/json/stats before-after/meta, flatness series,
soak wedge samples) under `/scratch/tmp/rpd_campaign/`; field scripts
under `/scratch/tmp/rpd/` (`settle.sh`, `field_deploy.sh`,
`phase_means.py`, `mdstorm.sh`, `wedge_sample.sh`). Bench filesets
`wr_rpd/`, `soak_rpd/` left on the store (normal bench artifacts).
No resets, no reformats, no raw-device writes, no storage-node
changes. Incidents journaled: the deploy script's over-uring
ready-grep false alarm on window 1 (mount verified armed via the
transport gauges — the log wording differs from the grep); one
accidental duplicate background dispatch of the window-2 deploy
command killed before execution (journal shows a single deploy).
