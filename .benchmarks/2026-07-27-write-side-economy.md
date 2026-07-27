# 2026-07-27 — DIALED P3: the write side — matrix + large-op ring economy

Branch `perf/write-side-economy` (off dev `cca7858`, **unmerged pending
review**). Commits: red `49c01ef` (the large-op economy contract + the
write-matrix harness `tests/write_matrix.sh`), green `af86657` (multi-slab
max_op windows + pipelined flights + `SlotCore::release_claimed` + loom),
`979d003` (single-slab fast-path restore + gate leg 2h multi-slab kill
cycles), `732cacc` (aligned-stride claim pass — the packing fix). Contract
suite: `tests/preload_session_tests.rs` §DIALED-P3 (7 tests; 2 red at
`49c01ef`, 5 semantics pins).

**The charter (P3):** writes were UNTUNED territory. Build the write
matrix first (the campaign's map), then fix the biggest collapses
red-first. Field prior: 4k rand O_DIRECT healthy (shim 1.4× kernel, 100 %
W1 patch engagement); user report of shim writes "slow as dirt vs normal
writes" on an unreproduced shape — prime suspects (a) large-block shim
chunking, (b) buffered-vs-writeback comparison asymmetry (KD-11).

## 1. Substrate (labeled; the DIALED rig, verified knob-by-knob)

32-CPU box, 109 GiB RAM, kernel 7.1.4-1-cachyos. configfs null_blk
`sqzlat_oss0` (36 GiB memory-backed, `completion_nsec=235000`, `irqmode=2`,
bs 4096, 8 squeues, hw QD 128) → nvmet-loop → `/dev/nvme1n1` (data);
`sqzlat_mds0` (3 GiB) → `/dev/nvme2n1` (meta) — still up from P1.5
(§9 there), knobs re-verified this session.

Filesystem: fresh cache-less format per side (`sqmeta:///dev/nvme2n1
sqdata:///dev/nvme1n1`, 4 MiB blocks — no staged layout; beyond-inline
routes striped). Mounts: ARMED `--daemon --allow-other --interception
--mem-cache-size 1GB` (KD-11 forces kernel write-through),
`SQUEEZEFS_IPC_SERVICE_THREADS=8`; UNARMED = same minus `--interception`
(kernel writeback cache ON — the "normal writes" posture).

**Instrument (stated, the L1-A lesson): fio 3.42, `ioengine=psync`,
`--thread`, numjobs=16 (t1 rows noted), qd1 sync syscalls — fio
page-aligns its buffers.** Rand rows = 10 s time_based overwrites of 16 ×
1 GiB preallocated striped whole-block-mapped files (the W1 patch shape);
seq rows = fresh-file creates (`--fallocate=none`), size-scaled per bs
(4k:64 MiB/file … 1m:512 MiB/file), with an **RW6-convention durable
tail** (fsync-every-file + syncfs, timed separately) on every seq row.
Engagement per row from `.stats` deltas: a shim row is INVALID unless
`ipc_ops_write Δ` accounts for its ops (the harness exits nonzero);
`ring/op` = `ipc_ops_write Δ ÷ fio ops` is the chunking instrument.
Harness: `tests/write_matrix.sh` (medians of 3; per-rep full-metrics
diffs persisted).

## 2. The matrix (BASELINE, dev `cca7858` daemon+shim pair — the map)

Medians of 3, IOPS. `ring/op` in brackets on shim rows.

| Row (armed, t16) | kernel | shim | shim/kernel |
|---|---|---|---|
| rand-4k-odirect | 30,794 | 39,807 [1.00] | **1.29× — healthy (the field's W1 patch row)** |
| rand-64k-odirect | 30,849 | 33,603 [1.00] | 1.09× |
| rand-256k-odirect | 23,898 | **11,147 [4.00]** | **0.47× — collapse** |
| rand-1m-odirect | 4,439 | **2,816 [16.00]** | **0.63× — collapse** |
| rand-4k-buffered | 30,232 | 38,615 [1.00] | 1.28× |
| rand-64k-buffered | 29,149 | 33,507 [1.00] | 1.15× |
| rand-256k-buffered | 21,672 | **11,403 [4.00]** | **0.53×** |
| rand-1m-buffered | 3,821 | **2,744 [16.00]** | **0.72×** |
| seq-4k-odirect | 168,798 | 330,573 [1.00] | **1.96×** |
| seq-64k-odirect | 84,672 | 87,265 [1.00] | 1.03× |
| seq-256k-odirect | 29,309 | **21,744 [4.00]** | **0.74×** |
| seq-1m-odirect | 7,699 | **4,917 [16.00]** | **0.64×** |
| seq-4k-buffered | 178,938 | 318,136 [1.00] | 1.78× |
| seq-64k-buffered | 79,245 | 87,033 [1.00] | 1.10× |
| seq-256k-buffered | 26,947 | **21,473 [4.00]** | **0.80×** |
| seq-1m-buffered | 6,861 | **5,195 [16.00]** | **0.76×** |

Single-stream (t1, seq-1m — the dd/cp shape): kernel-armed 2,695 odirect /
1,829 buffered; **shim 1,018 / 1,045 = 0.38×/0.57×** (982 µs/op — 16
serial ring RTTs vs the kernel's one 1 MiB WRITE at 371 µs).

UNARMED kernel-buffered rows (writeback cache ON): rand 4k/64k/256k/1m =
62,141 / 14,822 / 3,424 / 1,012; seq = 289,982 / 25,196 / 5,916 / 1,533.

**Reading the map:**
1. **The collapse is exactly the >slab chunking** (suspect (a) confirmed):
   `ring/op` 4.00/16.00 on every 256k/1m row — the shim chunked ring
   writes at the per-slot arena slab (arena/slots = 64 MiB/1024 = 64 KiB)
   and round-tripped each chunk SERIALLY, while the daemon has validated
   `len ≤ max_op_bytes` (1 MiB) since PR L4-4. Worst single-stream: 0.38×.
2. **≤ 64 KiB rows are healthy everywhere** (1.03–1.96× kernel) — the 4k
   patch rows the charter protects.
3. **Suspect (b) does not reproduce as a shim loss on this rig** — the
   unarmed writeback mount is SLOWER than the armed write-through mount at
   every size ≥ 64 KiB (FUSE bdi dirty throttling: writers block on
   background writeback almost immediately at these rates), and at 4k seq
   the SHIM (318k) beats unarmed writeback (290k) even though the
   unintercepted armed kernel path pays the KD-11 tax there (179k vs
   290k ≈ the priced ~1.6× on this rig's shape). Adjudication in §5.

## 3. The fix — large-op ring economy (client-side only; no wire/ABI change)

Three pieces, all in `crates/squeezefs-preload/src/session.rs` +
`crates/squeezefs-ipc/src/slot_core.rs` (daemon untouched; KD-7 pairs
unchanged — the daemon's `len ≤ max_op_bytes` validation was always the
contract):

- **Multi-slab max_op windows** (`claim_run`): a contiguous, non-wrapping
  run of slots reserves a contiguous arena window (slot slabs are adjacent
  by layout); only the BASE slot is submitted — extension slots are
  CLAIMED **arena holds** the daemon never observes, returned via the new
  `SlotCore::release_claimed` (CLAIMED → FREE, client-exclusive, same
  Release edge as `release()`). A 1 MiB write is now ONE ring op at
  default geometry.
- **Pipelined flights**: every chunk of one `ring_pwrite` is submitted
  before any is waited on; flights are reaped in offset order. POSIX
  prefix semantics pinned: the first short/failed chunk in offset order
  ends the reported count, later flights drain ignored (their bytes may
  have landed — the same property as the kernel's split out-of-order
  O_DIRECT WRITE pipeline, converged by the caller's retry; first-chunk
  errors surface the errno, protocol-class EINVAL falls through, the
  §5.4.1 deadline still poisons and abandons the run — never-reuse law).
  Fragmentation degrades run length (never refuses); slot exhaustion
  keeps the old backpressure posture.
- **Aligned-stride claim pass** (`732cacc`): the greedy random-start scan
  fragmented the slot array under 16 concurrent large writers — measured
  **ring/op 1.86–1.91 instead of 1.00** on the t16 1 MiB rows (the
  first-counted-side lineage, §4). Pass 1 tries want-aligned bases first
  so packing claimants collide on whole windows; the greedy pass stays as
  the fragmentation fallback.
- **Single-slab ops keep the pre-P3 allocation-free serial path**
  (`979d003`) — every ≤ 64 KiB write (the whole W1 patch population) is
  byte-identical to baseline; the flight bookkeeping exists only for
  >slab ops. (The first counted side without this showed the healthy 4k
  odirect row drifting −9.7 %; with the restore it probes at base parity.)
- **Severance/custody unchanged**: sever-at-dequeue (§5.5.2), the one
  arena read, the zero-copy ledger (app copy → arena, severed copy,
  merge copy) — the fix removes ROUND TRIPS, not copies; no new buffer
  classes, no daemon-side change.

**Loom** (`tests/run_loom.sh`, 44/44 green): new model
`ipc_slot_claimed_hold_release_reuse_clean` — holds are daemon-invisible
(`try_begin_serve` refuses CLAIMED), a hold's generation never consumes a
later life's DONE (the ABA guard extended across the new CLAIMED→FREE
edge). No other ordering changed (`wait_consume` is `one_op`'s wait
factored verbatim).

## 4. A/B/A (armed rows, medians of 3; per-run CSVs in the session dirs)

Sides in one session window: **base** = dev `cca7858` pair → **final** =
`732cacc` pair → **base2 bracket** = the base pair RE-RUN after ship
(binary-vs-drift separation). Lineage (multi-run discipline, recorded
never credited): the `af86657` first counted side (4k drift → `979d003`)
and the `979d003` second side (ring/op 1.86 fragmentation → `732cacc`)
each aborted their counts; the acceptance count below is the final pair
from zero.

| Row (t16) | base | **final `732cacc`** | base2 bracket | vs kernel (final) | verdict |
|---|---|---|---|---|---|
| shim-rand-256k-odirect | 11,147 | **23,999 [1.00]** | 10,036 | 1.05× | **+115 % vs base, +139 % vs bracket** |
| shim-rand-1m-odirect | 2,816 | **4,025 [1.00]** | 2,690 | 0.84× | **+43 % / +50 %** |
| shim-rand-256k-buffered | 11,403 | **23,725 [1.00]** | 10,844 | 1.10× | **+108 % / +119 %** |
| shim-rand-1m-buffered | 2,744 | **4,015 [1.00]** | 2,717 | 1.03× | **+46 % / +48 %** |
| shim-seq-256k-odirect | 21,744 | **28,668 [1.00]** | 20,165 | 1.03× | **+32 % / +42 %** |
| shim-seq-1m-odirect | 4,917 | **7,308 [1.00]** | 5,295 | 1.00× | **+49 % / +38 %** |
| shim-seq-256k-buffered | 21,473 | **27,513 [1.00]** | 21,278 | 1.00× | +28 % / +29 % |
| shim-seq-1m-buffered | 5,195 | **7,237 [1.00]** | 5,191 | 1.05× | **+39 % / +39 %** |
| **shim-rand-4k-odirect (healthy row)** | 39,807 | 39,223 [1.00] | 38,934 | 1.28× | **parity (+0.7 % vs bracket)** |
| shim-rand-4k-buffered | 38,615 | 39,194 [1.00] | 38,985 | 1.30× | parity |
| shim-rand-64k-odirect | 33,603 | 33,868 [1.00] | 33,585 | 1.08× | parity |
| shim-seq-4k-odirect | 330,573 | 326,863 [1.00] | 326,050 | 3.05× | parity |
| shim-seq-64k-buffered | 87,033 | 89,530 [1.00] | 79,922 | 1.13× | parity+ |
| kernel context rows | — | — | — | — | untouched path; final ≡ bracket within the rig's drift band on every row |

Single-stream (t1 seq-1m, final pair): shim odirect **1,984** (base 1,018,
**+95 %**, 0.74× kernel-armed 2,681); shim buffered **2,040** (base 1,045,
**+95 %**, **1.12×** kernel-armed buffered 1,822). The "slow as dirt"
single-stream shape moved 0.38×/0.57× → 0.74×/1.12× kernel.

**Engagement**: every shim row engagement-exact (`ipc_ops_write Δ`
accounts for every op; kernel rows Δ = 0); `ring/op` = **1.00 on every
row** of the final pair (the 4.00/16.00 collapse instrument, and the
1.86–1.91 fragmentation lineage, both closed).

**Instrument note (honest)**: the armed-kernel-seq-4k-odirect row is
bimodal on EVERY side (71k–178k across reps/sides on an untouched path) —
a rig characteristic, not a binary effect; no verdict is drawn from it.

## 5. Suspect (b) adjudicated — buffered ring writes stay write-through

The charter asked whether the ring should ack buffered (non-sync) writes
at arena-accept. **Ruled out by the design, and the matrix shows it is
not needed on this rig:**

- **Design**: §5.6.3 durability ordering (fsync-through-FUSE soundness)
  requires the happens-before chain *app-program-order → ring-ack →
  fsync-issue → daemon flush* — a ring write may ack only after the
  daemon owns the bytes in the same pre-flush state kernel-FUSE writes
  reach. An arena-accept ack would ack bytes still in **client-writable
  shm** that no daemon state contains (the severed copy is made at
  dequeue, and sever-at-dequeue custody is load-bearing); a subsequent
  fsync could flush state missing the acked write, and an ENOSPC/EFBIG/
  fencing refusal would have already been acked — the never-lossy custody
  rules forbid both. **No early ack; the row IS the write-through
  contract.**
- **Empirically** the contract costs nothing through the shim here: armed
  shim buffered ≥ unarmed writeback at every size on this rig (4k seq:
  318k vs 290k; 1m seq: 7,237 vs 1,533 — FUSE bdi throttling makes
  writeback the slow path at these rates). The KD-11 tax exists only for
  UNINTERCEPTED buffered writes (armed kernel 4k seq 167–179k vs unarmed
  290k ≈ 1.6–1.7× here — the tax the L4 closing report priced at ~3× on
  its rig), and the shim is the designed escape for exactly those writes.
- The user's "slow as dirt vs normal writes" is therefore attributed to
  suspect (a) — large-block serial chunking (0.38× single-stream, the
  dd/cp shape) — now fixed; if the field shape returns post-fix, the
  matrix harness is the reproduction instrument.

## 6. Falsification duties (all on the final pair)

- **Cargo contract suite** (red at `49c01ef`): one-ring-op-per-max_op
  window; pipeline-before-wait (a serial client strands on a
  park-until-both-arrive sink); POSIX prefix on short first completion;
  first-chunk errno vs mid-stream prefix; every-slot-released (incl.
  extension holds); fragmented-slots degradation; multi-slab parity
  through the REAL DataPlaneSink (1 MiB + odd tail, both transports).
  18/18 in `preload_session_tests`.
- **Mid-drive verify**: fio randwrite `--verify=crc32c --verify_fatal`
  through the shim on the armed mount — bs=1m (multi-slab flights) and
  bsrange=4k-1m (slab-boundary-crossing sizes), 8 threads each:
  **err=0**, zero verify failures; tripwires after:
  `write_path_seed_read_bytes` 0, `patch_edge_rmw_reads` 0,
  `ipc_descriptor_rejects` 0, `ipc_sessions_poisoned` 0,
  `fsck_findings` 0. Per-row deltas across the whole final matrix:
  tripwires clean on every rep.
- **Preload gate** `sudo tests/run_preload_gate.sh`: both legs PASSED on
  the final pair — with **leg 2h extended** (this branch): kill-9 write
  soak alternates bs=1M cycles so multi-slab pipelined flights (runs +
  arena-extension holds) are in flight at SIGKILL; 5 cycles, zero
  session/arena residue, daemon alive; leg 2l direct-drive kill-9 soak
  engaged +15,360 serves, zero residue.
- **Umount promptness**: 0.002 s immediately after the verify rows.
- **Loom**: 44/44 (§3).

## 7. Gates

On `732cacc`: `cargo clippy --all-targets --all-features -- -D warnings`
clean; `cargo fmt --check` clean; `cargo doc --no-deps` — 3 warnings,
byte-identical to the pre-existing dev set (`handoff_spawn` ×2 +
`GhostTable` private-link); bench smoke `cargo bench --benches -- --test`
green (210 bench tests); `tests/run_loom.sh` 44/44. KD-7: every A/B side
measured as a same-commit daemon+shim pair.

`cargo test --all-features -- --test-threads=1`: the FIRST run failed one
test — `mount_writer_guard_tests::test_torn_claim_entry_recovers_and_
reclaims` ("another squeezefs process holds the writer lock" on the
remount-after-torn-claim leg) — **a cross-test timing flake on an
untouched path** (this branch's diff is entirely shim-side:
`crates/squeezefs-preload`, the additive `SlotCore::release_claimed`,
tests, gate script; no daemon/guard surface): the test passes 5/5
isolated and the WHOLE writer-guard binary passes 3/3 on this branch AND
3/3 on clean dev `cca7858` (declared attribution gathering, never
acceptance). Recorded here so it is not silently inherited — like the P1
note's `pending_free_at_cap` flake, it needs its own red-first loop in
its own program. Per the counted-run discipline the suite was then re-run
FROM ZERO on the final binary: complete pass, exit 0.

## 8. Residuals (recorded, not chased)

- **rand-1m-odirect at 0.84× kernel (t16) / seq-1m t1 odirect at 0.74×**:
  the remaining per-op gap is the ring's severed copy (+1 × 1 MiB memcpy
  + alloc vs the kernel path's zero-copy payload lease) plus the
  single-handoff-per-op serve (no intra-op overlap of arena copy with
  daemon serve). Named follow-ons, in order of expected yield: (i) a
  §5.5.3-class registered-buffer arena DMA for complete-block/W1-patch
  ring WRITE shapes (deletes the severed copy where the daemon never
  interprets payload bytes — rule-3 legal, needs its own ≥10 % A/B to
  stay); (ii) a write direct-drive prelude on the service thread
  mirroring P1's read engine (patch-shaped + complete-block writes are
  prelude-shaped; fallback-is-correctness for anything
  staged/overlay/meta-bearing) — a P1-sized program, filed with this
  matrix as its justification, NOT started here.
- **`ring_pread` large-op economy**: reads > slab still chunk serially at
  the slab (the read rows are DIALED via direct-drive for ≤64 KiB
  device-class shapes, but a 1 MiB shim read is 16 serial RTTs on the
  handler path). The same `claim_run`/flight machinery applies nearly
  verbatim — pre-agreed follow-on, kept out of this branch to keep the
  write A/B clean.
- **Unintercepted buffered small writes on armed mounts** keep the KD-11
  write-through tax (§5) — by design; the shim is the escape.
- **The armed-kernel-seq-4k-odirect bimodality** (§4 instrument note) is
  unexplained rig behavior on an untouched path; worth a look if it ever
  appears off-rig.
- fstests/LTP/pjdfstests: untouched kernel-FUSE paths (the daemon binary
  is byte-identical in behavior; all changes are shim-side) — release
  cadence per the test-tier table.

## 9. Substrate teardown

As the P1/P1.5 notes: disconnect the two nvmet-loop subsystems, unlink
the port, rmdir nvmet objects, power-off + rmdir the `sqzlat_*` configfs
null_blk items. Left up while the branch is under review (RAM-backed,
reboot-ephemeral).
