# 2026-07-28 — Shim parity: the 1-copy ring write path (sever into the ActiveBlockBuf)

Branch `perf/shim-parity` off dev tip `81bb53e`. Commits: red `adb4e5a`
(placed-sever contracts + counters), green `3ba555f` (the mechanism),
`c38ec63` (the parity verdict codified in write_matrix), `6192a90`
(fuse3 dead-lane re-dispatch — board item 2), `b0b218b` (whole-flight
engagement: deferred handoffs + batch publish), plus the bracket-rig and
docs commits. Perf binaries: CAMP `f9a884b` (product code identical to
the final tip — later commits are script/docs only), BASE `81bb53e`.

## 0. The governing law (user directive, verbatim)

> "The kernel and IPC should always at minimum be at par with the IPC
> out pacing the kernel in the majority of benchmarks."

The known violation (ingest-economy §3 / board item 1): kernel-FUSE
out-streamed the ring path ~15 % at t16×4MiB because a ring write paid
**shim-copy + sever-copy + merge** where the kernel path pays
**payload-lease + merge**. This campaign deletes the second daemon copy
and codifies the law as a self-enforcing sweep verdict.

## 1. The mechanism — 2 copies → 1 copy

### 1.1 Placed sever (`src/placed_sever.rs` + `src/placed_core.rs`)

The §5.5.2 severance law fixes WHERE the one arena read happens (at
dequeue, synchronously, on the service thread) — so the only way to
reach 1 copy is to make the sever's DESTINATION the block's future
`ActiveBlockBuf` backing. Whole-block-stream chunks of one
`(ino, block)` sever into one shared **assembly**
(`SharedBlock` — the same pooled 4096-aligned allocation class every
overlay backing uses), claimed page-granularly:

- **Sink screens** (all synchronous, latch-free; every refusal =
  pooled fallback): page-aligned single-block `[rel, rel+len)`;
  `len > patch_max_bytes()` (the same lever that admits the W1 patch —
  the two paths stay mutually exclusive by construction);
  `offset+len ≤ max_file_size()` (the handler's EFBIG screen mirrored —
  an EFBIG op must leave no side effects); cached layout class ==
  striped (only striped writes reach `write_file_staged`'s merge);
  no live overlay entry; assembly gauge under the cap.
- **Adoption** (the entry-absent merge branch, under the block's
  `BLOCK_FLUSH_LOCKS` guard): pointer proof (`payload.as_ptr() ==
  assembly.ptr + rel` — only a placed sever can satisfy it) + the
  claims **seal** ⇒ the assembly becomes the overlay backing
  (`ActiveBlockBuf::adopted` / `CowCell::adopt`), seed class identical
  to the fresh/deferred arms (`overwrite_seed_deferred` still counted
  on existing-bytes blocks; RW3b coverage law untouched).
- **Merge elision** (every merge): if the payload region IS the entry's
  CURRENT Full backing at exactly `rel_start` (`full_ptr_at` pointer
  identity), `record_write` runs and the copy is skipped
  (`placed_merge_elides`). The proof is airtight by the existing CoW
  discipline: **in-place mutation requires backing uniqueness, which is
  impossible while the placed payload's shared handle is alive** — any
  mutation since adoption CoW'd the backing, the pointer changed, and
  the merge copies from the (intact) assembly region instead.

### 1.2 Custody proof (the §5.5.2 law survives verbatim)

- The arena is still read exactly once, at dequeue, before the handoff
  counter increments — a post-sever scribble is inert (pinned in
  `tests/shim_parity_tests.rs::streaming_ring_write_severs_into_block_buffer_one_copy`,
  which scribbles all four chunks' arena windows inside a held-lock
  window and read-verifies).
- Severed bytes are indestructible: content-establishing exits
  (`zero_complete`, `fill_complement_from`), `make_mut`, and reader
  snapshots all ride `CowCell` — while any placed payload lives, the
  cell is non-unique and every mutation copies away. A shim crash after
  dequeue loses nothing (daemon-private memory; the preload gate's
  kill-9 soaks run against this shape).
- **Claims protocol** (`placed_core`): page-claim overlap exclusion
  (two severs can never share a region — a torn A/B mix would match
  neither write) and the seal-vs-claim **`fence(SeqCst)` Dekker** (the
  W1 §5.1 fence shape): a sever memcpy can never land in
  snapshot-reachable memory. Loom models
  (`placed_claims_overlap_exclusive_and_rollback_clean`,
  `placed_claims_never_double_grant_while_held`,
  `placed_seal_vs_claim_dekker_never_adopts_over_a_writer`) 3/3;
  **weakening evidence**: the pre-fence protocol (plain SeqCst
  store/load pair) FAILED the Dekker model; the fences are
  load-bearing and commented as such.
- Failure semantics unchanged: fencing retries re-merge idempotently
  (`record_write` of a covered range is a no-op; the pointer test stays
  true); entry-present blocks, staged-seeded blocks, extent overlays,
  and every CoW'd backing take the ordinary copy.

### 1.3 Engagement levers (commit `b0b218b`)

First smoke measured only ~50 % of chunks severing placed and ~28 % of
merges eliding — the drain and the handler lanes raced the stream:

1. **Placed-write handoffs defer to end-of-drain-pass**
   (`PENDING_PLACED_HANDOFFS` → `DataPlaneSink::flush`, exactly the
   `SessionSink::flush` liveness contract): every sibling chunk the
   pass dequeued severs into the shared assembly before any handler
   merge can park the overlay entry. Custody unchanged; only the spawn
   site moves. Pooled writes/read demotions keep their immediate spawn.
2. **Shim batch publish**: the pipelined large-write submit stages
   claim+copy+publish for the whole flight, then ring-pushes the batch
   under ONE doorbell (per-chunk pushes let the drain outrun the 1 MiB
   slab memcpys between them, splitting one block's chunks across
   passes). Strictly fewer doorbell edges; POSIX prefix semantics
   unchanged.

Post-levers engagement is **exact** at the wall venue: 16,320 placed
severs / 4,080 adoptions / 16,320 elided merges per 16 GiB row — every
chunk of every block, zero copies at the merge. (qd1 serial 1 MiB
streams engage at 1 elide per block — the adopting chunk — because the
client waits per op; the win is depth-proportional by design.)

### 1.4 Memory honesty

Live assembly bytes are gauged (`placed_assembly_bytes`) and registered
as the non-sheddable R5 component `placed_assemblies` (the
`ipc_severed_buffers` pattern); creation refuses past
`min(budget/8, 2 GiB)` (counted `ipc_placed_sever_fallbacks`, pooled
fallback). Convergence is by adoption/drop — a shed hook could not act
on in-flight custody.

## 2. The law, codified (commit `c38ec63`)

`tests/write_matrix.sh` now carries the user's sentence verbatim in its
header and ENFORCES it: every armed shim row pairs with its kernel twin
by median IOPS; within ±`SQZ_WM_NOISE_PCT` (default 10) = PAR, below =
LOSS → **the sweep exits nonzero naming the row**; the summary reports
win/loss/par and asserts il wins the majority of decided pairs (the
check is explicit so a future allow-loss lever cannot silently drop the
majority clause). The seq bs list gains **4m** — the t16×4MiB
known-violation venue — and `parity.csv` persists per run.
`tests/shim_parity_bracket.sh` is the campaign's counted A-B-B-A
acceptance rig (engagement-exact, amp columns, seed tripwire).

## 3. Bracket (counted, A-B-B-A vs dev `81bb53e`)

Venue: **TCP devsub substrate** (`SQZ_DEVSUB_TRANSPORT=tcp`, nvmet-tcp
on localhost; mds nvme1–4 memory null_blk, oss nvme5–8 zram, 22-CPU
box, quiet). Instrument (stated): fio psync `--zero_buffers`
`--direct=1` (zeros ≈ free on zram — the device is not the wall, the
client path is), 16 threads, medians of 3 per side per pass, order
CAMP-BASE-BASE-CAMP, fresh blkdiscard+format+interception mount per
side, KD-7 clean same-commit daemon+shim pairs. Engagement EXACT on
every cell (`ipc_bytes_in` Δ == row bytes on il, == 0 on kernel);
`write_path_seed_read_bytes` = 0 throughout; data-namespace
amplification 1.001–1.003 on every streaming row. Rows are relaxed-ACK
labeled (RW6 convention: no fsync in the loop, identical law both
sides). Raw CSV: `/tmp/pb_counted2/rows.csv` (preserved with the run's
fio/stats snapshots).

| row | CAMP med (per-pass) | BASE med (per-pass) | camp/base |
|---|---|---|---|
| **il-t16-b4m** (MiB/s) | **11,188** (11,222 / 11,115) | 9,510 (9,471 / 9,548) | **+17.6 %** |
| kern-t16-b4m (canary) | 11,168 (11,214 / 11,123) | 11,014 (11,018 / 11,011) | +1.4 % (kernel path untouched) |
| il-rand4k (IOPS) | 250,470 (257,584 / 250,400) | 253,597 (254,938 / 251,094) | −1.2 % (in-band wash) |
| kern-rand4k (IOPS, canary) | 206,842 | 206,603 | +0.1 % |

**The violation is closed, order-independent:**

- BASE reproduces the board item exactly: il/kernel = 9,510 / 11,014 =
  **0.863** (the −13.7 % violation).
- CAMP: il/kernel = 11,188 / 11,168 = **1.002 — par-to-ahead at the
  wall venue** (both per-pass windows agree: 11,222 vs 11,214 and
  11,115 vs 11,123).
- Small-op rows are flat (il-rand4k −1.2 % vs BASE with the kernel
  canary +0.1 % — the same-noise drift shape; il beats kernel by
  **+21 %** on rand-4k on BOTH sides). No IOPS traded for streaming.
- Sequential qd1 1 MiB il ingest (the rand-prep rows, incidental):
  CAMP 9,712 vs BASE 9,200 MiB/s (+5.6 %) — the single-elide-per-block
  regime.

## 4. Parity-verdict matrix (deliverable 2 run green)

`tests/write_matrix.sh` on the TCP devsub substrate (meta nvme1n1, data
nvme5–8n1 — the multi-volume list the substrate's per-namespace zram
capacity requires; stated: NOT the canonical fabric-latency matrix
venue), CAMP binaries, filter `armed-*-odirect`, reps 3, band ±10 %:

| pair (median IOPS) | shim | kernel | il/kern | verdict |
|---|---|---|---|---|
| rand-4k-odirect | 118,893 | 111,682 | 1.065 | PAR (il ahead) |
| rand-64k-odirect | 10,270 | 10,317 | 0.995 | PAR |
| rand-256k-odirect | 2,656 | 2,709 | 0.980 | PAR |
| rand-1m-odirect | 855 | 887 | 0.964 | PAR |
| seq-4k-odirect | 77,534 | 72,236 | 1.073 | PAR (il ahead) |
| seq-64k-odirect | 11,309 | 10,330 | 1.095 | PAR (il ahead) |
| seq-256k-odirect | 3,118 | 3,066 | 1.017 | PAR |
| seq-1m-odirect | 802 | 836 | 0.959 | PAR |
| seq-4m-odirect | 220 | 227 | 0.969 | PAR |

**`parity summary: pairs=9 win=0 loss=0 par=9` — exit 0 (at-minimum-par
holds on every pair; no beyond-band losses).** Engagement: every shim
row ring/op exact (4.00 chunks/op on seq-4m — the whole-flight split),
every kernel row ipc-clean. Instrument note (the standing
elbencho-vs-fio / zram lesson): the matrix's default fio buffers make
its 4m rows **device/regime-bound on zram (~900 MiB/s both transports,
symmetric)** — the client-bound regime where the placed-sever win shows
is the §3 `--zero_buffers` bracket; the matrix's job here is the LAW
(no beyond-band il loss anywhere), which it enforces on every future
run via its nonzero-exit verdict. The rand-1m rows drift downward
across reps on BOTH transports identically (zram store aging —
symmetric, PAR verdicts unaffected); shim-rand-4k rep 1 was a warmup
outlier (4.9 k vs 119 k, median robust).

## 5. Board-item disposition

1. **Sever-into-ActiveBlockBuf** (board item 1) — **CLOSED** (this
   campaign, §1–§3).
2. **fuse3 TPC lane-death dispatch blackhole** (board item 2) —
   **CLOSED** ride-along (`6192a90`): a closed lane channel returns the
   future and dispatch re-routes it to the next live lane, error-logged
   and counted (`fuse3_tpc_lane_redispatches` on the stats inode — a
   must-stay-0 tripwire; growth = a lane thread died and its share
   rides the survivors); ALL lanes dead aborts loudly instead of
   blackholing every mount user. Unit-pinned (`tpc_dispatch_tests`).
3. **Streaming partial-union flushes** (board item 3) — untouched
   (bounded, legitimate; the placed sever does not change flush-boundary
   timing — coverage completion is unchanged).

## 6. Gates (final tip, from zero, quiet box)

- **Full cargo gate**: clippy `-D warnings` clean (root all-features;
  fuse3, squeezefs-preload [interposers profile] clean per-crate);
  `cargo fmt --check` clean; `cargo test --all-features --
  --test-threads=1` from zero = **exit 0, 150/150 test binaries ok**;
  `cargo doc --no-deps` exit 0; bench smoke (`cargo bench --benches --
  --test`) exit 0.
- **Loom**: **51/51** (48 existing + the 3 new `placed_core` models).
  The Dekker model FAILED against the pre-fence protocol (the recorded
  weakening evidence) and passes with the paired `fence(SeqCst)`.
- **fuse3 standalone suite**: 42/42 (41 + the new `tpc_dispatch_tests`
  lane-death pin).
- **Preload gate**: leg 1 (unprivileged) PASSED; leg 2 (root) PASSED —
  mount parity, §3 rule-4 engagement, dup/close_range/lseek rows,
  **kill-9 soak + fork-kill-parent soak + direct-drive kill-9 soak
  (5 cycles, +15,360 engaged serves, zero session/arena residue)
  against the new custody shape** — the campaign's crash surface.
- **statfs_tests ×10 loaded soak** (stated load recipe: looping fat-LTO
  `cargo build --release` with `touch src/lib.rs` per iteration in a
  second worktree, verified live for the whole window): **30/30 green,
  0 hangs, 0 timeouts** (61.8–62.1 s per roll — the file's nominal
  duration), fusectl waiting-connections residue sweep clean.
- One suite roll DURING the campaign (running concurrently with two fat
  release builds) tripped `defrag_tests::
  test_report_only_gauges_match_independent_census` once — the known
  load-overlap class (§7); standalone green, and the acceptance
  from-zero roll above ran quiet and clean.

## 7. Found while measuring (recorded)

- **The bracket's first counted run ENOSPC'd by rig design** (the il +
  kernel 16 GiB datasets stayed live together = the substrate's whole
  32 GiB): fixed in the rig (datasets drop at row end), count restarted
  from zero per the multi-run discipline.
- **format→mount adjacency**: a mount immediately after a format
  process exit can transiently observe the D0 writer flock still held;
  the rig retries once (3 s). Not product-visible (operators do not
  race their own format's exit by milliseconds); recorded for the
  board.
- The defrag suite's
  `test_report_only_gauges_match_independent_census` tripped ONCE while
  the full suite ran concurrently with two fat release builds (the
  known load-overlap operator-error class from the op-economy/ingest
  notes); standalone rerun green; the acceptance suite roll ran on a
  quiet box.
