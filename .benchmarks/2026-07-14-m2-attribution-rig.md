# PR M2 acceptance — per-op attribution rig first light + journal-entry economy (OQ 1 settled)

| | |
|---|---|
| **Program** | metadata-throughput (`docs/design-metadata-throughput.md`), PR M2 |
| **Branch** | `test/meta-attribution-rig` @ `577afe7` (off dev `8e95ebb`) |
| **Box / rails** | same 3.5 GHz-capped box as the baseline; storms quiet-gated (3×15 s streak, Tctl < 80 °C), daemons caged (`systemd-run --user --scope -p MemoryMax=8G -p MemorySwapMax=0`), binary named `sqm2` (kill-pattern immunity), kills by PID only |
| **Substrate** | file-backed sandbox on btrfs-CoW home fs (`~/tmp/m2_rig_*/meta_*.img`) ≈ the baseline's **A cow** class; default cadence (50 ms) — the sanctioned substrate for non-barrier work (baseline bracket flat ±9 %) |
| **Shape** | mdstorm 8 threads × 100 k, one dir (`create → stat → rename → unlink`), per-phase `.stats` snapshots; separate 60 k create leg under bpftrace (untimed — instrumentation skews) |

## Verdicts up front

1. **OQ 1 is SETTLED, on a live mount, with named call sites.** The second journal
   entry per rename **and** per unlink is the **kernel's post-op ctime writeback**
   (`fuse_update_ctime` → `fuse_flush_times` → `FUSE_SETATTR(FATTR_MTIME|FATTR_CTIME)`)
   landing as a times-only `Metadata::setattr` commit at
   **`src/meta_backend/kv/backend.rs:3151` (`setattr_locked`)** — exactly 1.000/op in
   both storms. The engine paths are exonerated: `routed_rename_local`
   (`backend.rs:2948`) and `routed_unlink_local` (`backend.rs:2769`) are exactly
   1.000 entries/op.
2. **The unlink batch-fill prior is REFUTED.** Destroy-batch fill ≈ **62** (1,606
   batches/100 k unlinks; `meta_reclaim_batch_size` delta concentrated in the ≤64
   bucket), contributing **1/fill ≈ 0.016/op** — the *third* (small) term, not the
   second entry. The gather window is healthy: close reasons cap-dominated
   (create phase 1,556 cap vs 10 window; unlink phase 1,385 cap vs 221 window).
   D4.c's "fix the gather degeneration" is de-scoped by measurement; **G4's win
   lives in killing the SETATTR echo** (a whole 1.0 entries/op on both ops), which
   D4.b's one-tx rename (ctime staged in-tx) is already shaped to absorb.
3. **The §4/R1 serial-chain model HOLDS** (details below): 1/throughput = 159 µs/op,
   the under-`i_rwsem` estimator sees a median ≈ 90 µs (64–128 µs bucket; ~100 µs
   distribution mean) of it from userspace, and the kernel-side bpf cross-check
   (`down_write`→`up_write` wait+hold, comm=mdstorm) modes at **1–2 ms ≈ 8 threads
   × 159 µs** — the parent lock is continuously occupied with a full convoy.
   **R1's High rating is confirmed with numbers**: G2's ≥ 18 k/s needs ≤ ~55 µs
   under-lock; today's *handler-visible* span alone is ~90–100 µs, and ~60 µs more
   is kernel-side hand-off the daemon never sees.

## Journal-entry economy through the mount (rig OFF session, 100 k ops/phase)

| phase | entries/op | fuse_ops/op | meta_updates/op | committers (site → per-op) |
|---|---:|---:|---:|---|
| create | **1.0056** | 5.181 | 1.000 | `routed_create_local` (:2657) → 1.0000; heartbeat setxattr (:3171) ambient |
| stat | 0.0001 | 1.000 | 0 | — |
| rename | **2.0022** | 5.012 | **2.000** | `routed_rename_local` (:2948) → 1.0000; **`setattr_locked` (:3151) → 1.0000** |
| unlink | **2.0197** | 5.787 | **2.000** | `routed_unlink_local` (:2769) → 1.0000; **`setattr_locked` (:3151) → 1.0000**; `destroy_inodes` (:2541) → 0.0164 |

Baseline agreement is exact (1.006 / 2.002 / 2.020; 5.18 / 5.01 / 5.70 fuse_ops).
`meta_updates` = 2.000/op is the tell the baseline's own snapshots already carried:
a second FUSE **mutation handler** runs per rename/unlink. The rig-ON op mix names
it: **setattr = 1.00/op** in both phases. The model
`entries/unlink = 1 (unlink tx) + 1 (SETATTR echo) + 1/fill + ambient` closes to
2.0194 measured vs 2.020 baseline; rename closes at 2.0023 vs 2.002. The
`tests/meta_entry_economy_tests.rs` pins encode all of this against sandboxes
(fill = 64 → per-op 2.016; singleton-fill would read ≥ 3.0 — refuted).

## First rig light (SQUEEZEFS_OP_PROFILE=1 session, same substrate)

Timed rows moved ≤ noise with the rig ON (see disabled-cost below). Op mix +
phase medians (bucket-geometric estimates from `fuse_op_phase_ns`; h2b =
handler→backend, b2r = backend→reply):

**create phase** (6,291 ops/s):

| op | count/op | h2b µs | backend µs | b2r µs | total µs |
|---|---:|---:|---:|---:|---:|
| lookup | 1.00 | 0.7 | 2.8 | 0.7 | 2.8 |
| create | 1.00 | 0.7 | **90.5** | 1.4 | 90.5 |
| getattr | 1.17 | 0.7 | 0.7 | 0.7 | 0.7 |
| flush | 1.00 | 0.7 | 2.8 | 0.7 | 2.8 |
| release | 1.00 | 0.7 | 2.8 | 0.7 | 5.7 |

**rename phase** (6,442 ops/s): lookup 2.00/op @ 5.7 µs; rename 1.00 @ 22.6 µs
backend; **setattr 1.00 @ 22.6 µs backend**; getattr 1.01 @ 1.4–2.8 µs.
**unlink phase** (4,025 ops/s): lookup 1.00 @ 11.3 µs; unlink 1.00 @ **90.5 µs**
backend; setattr 1.00 @ 22.6 µs; **getattr 1.82/op @ 45.3 µs**; forget ~1.00.

D1-relevant findings the phase split hands the later PRs:

- **Create's wall is ~entirely inside `backend.create`** (h2b 0.7 µs, b2r 1.4 µs,
  backend 64–128 µs bucket): the handler glue (attr-cache insert, metadata_cache
  seed, dir-entry invalidate) is *not* where create's µs live — the routed layer +
  engine span is (mount-side 90 µs vs 37 µs engine-direct trait row = +53 µs of
  routed/DLM/glue + storm interference for D1.c/D7 to attribute further).
- **GETATTR is the biggest trailing-op population** (1.17/create, **1.82/unlink**
  at 45 µs each) — D2.c's attr-TTL audit target, now with numbers.
- FLUSH/RELEASE cost 2.8–5.7 µs each at zero dirty state (the D1.d/D2.a elision
  wins ~2 round trips + ~8 µs handler time per create).

**Under-`i_rwsem` span** (`fuse_create_under_lock_ns`, LOOKUP-arrival →
CREATE-reply, n = 100 k): median bucket **64–128 µs** (geometric ≈ 90.5 µs),
distribution 12.9 % ≤ 64 µs / **78.6 %** 64–128 µs / 7.3 % 128–256 µs, p99 ≈ 362 µs;
distribution mean ≈ 100 µs.

## §4/R1 arithmetic, re-derived from measurement

- Serial wall per op: `1/throughput` = 1/6,291 = **159 µs** (one-dir, 8 writers).
- Per-thread cycle: 8/6,291 = **1.27 ms** — and the **bpf cross-check**
  (`down_write`→`up_write` per (tid, rwsem), comm=mdstorm, 60 k-create leg) modes
  at **[1 ms, 2 ms)** (35.5 k of 60 k samples; 21.4 k in [512 µs, 1 ms)): each
  thread's wait-plus-hold ≈ 7 waiters × span + own span ≈ 8 × 159 µs. The parent
  `i_rwsem` is a full convoy — **one-dir throughput = 1/(under-lock span)**, as §4
  hypothesized. (Probe caveat: comm-filtered `down_write` includes non-i_rwsem
  rwsems (mmap etc.) — the storm loop makes those negligible (the sub-µs spike is
  them); the histogram is wait-inclusive by construction.)
- Decomposition of the 159 µs: ~90–100 µs is **daemon-visible** under-lock span
  (LOOKUP handler entry → CREATE reply enqueued; create handler backend 90 µs +
  LOOKUP 2.8 µs + glue). The **~60 µs residual** is kernel-side: lock hand-off,
  request dispatch before LOOKUP handler entry, reply→wake legs — under-lock time
  no daemon-side cut can reach directly (transport latency cuts (D3) and round-trip
  removal (D2.d atomic-open folding LOOKUP into CREATE) attack it instead).
- "×4.6" one-dir multiple: this box's one-dir 6.29 k/s vs the baseline's many-dirs
  27.8–28.3 k/s ≈ **4.4–4.5×**, consistent with the baseline's measured 3.9–4.9×
  band (mfcreate not re-run here; the baseline row stands).
- **G2 arithmetic check**: ≥ 18 k/s ⇔ ≤ ~55 µs under-lock. Today: ~159 µs total =
  ~90–100 µs daemon-visible (D1/D2/D7's territory: create backend 90 µs is the body)
  + ~60 µs kernel/transport legs (D2.d/D3's territory). The R1 escalation path
  (§5.8(c)) stays armed: if D1–D7 land and the kernel legs floor above ~25 µs, the
  one-dir row stalls in the 14–16 k band exactly as R1 warns.

## Disabled-cost proof (hard M2 criterion)

- **Gate**: memoized `OnceLock` (the `get_fuse_timeout` pattern), default OFF;
  pinned by `op_profile_gate_is_memoized_and_default_off` (post-launch env
  mutation must not arm it). Disabled per-handler cost = one atomic load + branch
  → `None`; stats JSON emits **no** rig fields when disabled (byte-identical
  surface).
- **Criterion rows, dev tip (8e95ebb) vs branch tip, same box** (the commit-hook
  population):

| row | dev | branch |
|---|---|---|
| `kv_meta_metadata/create_unlink_file` | 66.8 µs [64.9, 68.8] | 65.9 µs [64.5, 67.5] |
| `kv_meta_metadata/lookup_file` | 2.062 µs | 2.037 µs |
| `kv_meta_metadata/readdir_page_100_of_1k` | 57.8 µs | 53.0 µs |
| `high_concurrency_contention/dlm_lock_contention` | 818.7 ns | 816.5 ns |

  No row moved outside its CI (readdir moved *down* — box noise; `set_get_xattr`
  is CI-unstable on both tips: 62.5 µs dev vs 50.7–58.3 µs branch re-runs).
- **Mount rows**: rig-binary-with-env-unset one-dir create = **5,847–5,999/s**,
  inside the baseline band (5,868–6,490, A-cow class). Rig **enabled** rows
  (6,291/6,307 create) sit at the band's top — enabling the rig costs less than
  session-to-session thermal noise at this op cost (4 `Instant` reads + one slot
  CAS against a 159 µs op).

## Incident: the rig's first catch — torn `.stats` snapshots (fixed in-PR)

With the rig armed, the stats payload grew to ~41 KB and **9 of 10 storm phase
snapshots became unparseable JSON prefixes**. Two stacked causes, isolated live
(strace + daemon-side `Open virtual (pinned N bytes)` / `Read virtual (hit)`
debug pair):

1. **GETATTR regenerated the payload and republished a new size** while the open
   fh served the older pinned generation — fstat's `i_size` disagreed with the
   bytes the fh serves.
2. Even with a coherent fstat, **`cat` reads via splice, whose copy bound is a
   possibly one-generation-stale `i_size`** regardless of `FOPEN_DIRECT_IO`
   (plain `read(2)`/`dd` parsed while `cat` tore; 4 KiB quantization still tore
   whenever a storm grew the payload across a quantum — all five residual tears
   were exactly 40,960-byte prefixes).

Fix (commit `577afe7`): generation-publish points store the size beside the bytes
(`latest_{stats,config}_size`); GETATTR/readdirplus report the published size and
never regenerate; OPEN re-publishes the exact pinned generation's size; and
virtual payloads are **tail-padded to a constant size** (`.stats` 256 KiB floor,
`.config` 64 KiB; trailing whitespace is legal JSON; overflow degrades loud to
4 KiB quantization) so `i_size` never moves between generations. Real-kernel
verification: 3× `cat` + `dd` + plain-read all return complete parseable
262,144-byte snapshots; the final session's 10/10 phase snapshots parse.
Pinned by `metrics_tests::stats_snapshot_getattr_size_matches_served_bytes_under_churn`.

## Deferred / honest notes

- The bpf leg ran (bpftrace 0.26, `sudo -n`, non-interactive) and is recorded
  above as the calibration artifact; a finer-grained i_rwsem-only probe (struct
  offsets to filter the parent inode's rwsem specifically) is deferred — the
  comm-filtered wait+hold histogram was sufficient to confirm the convoy model.
- The OFF session ran at the rig commit (`1ec73c1`); the ON sessions at the
  snapshot-fix tip (`577afe7`). The fix touches only virtual-inode serving — op
  paths identical.
- Histogram medians are bucket-geometric estimates (power-of-2 buckets of the
  repo's `LatencyHistogram`); sub-µs phases saturate the ≤1 µs bucket by design.
- mfcreate (many-dirs) was not re-run; the ×4.6 comparison uses the baseline's
  many-dirs rows on the same box class.

## Artifacts

`~/tmp/m2_rig_2394317/`: `results.tsv` (all quiet-gated rows), `stats/`
(per-phase pre/post snapshots, `bpf_i_rwsem_hold.txt`), `logs/` (daemon logs).
Harness: `/tmp/m2_session{_lib}.sh`, analysis `/tmp/m2_analyze.py` (session-local,
not repo material).
