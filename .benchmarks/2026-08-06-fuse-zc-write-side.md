# 2026-08-06 — D14 write-side zc: track-keyed mirror fix + WRITE_FIXED(device ← slot)

Branch `perf/fuse-zc-write-side` (off wave tip `aca64de1`, **unmerged —
the orchestrator merges**). Charter: ruling **D14** (rc-manifest §3f)
— the write-side zc leg supersedes the extraction-economy campaign:
patch 0024 already registers WRITE payload pages `ITER_SOURCE` in the
sparse slot; build the daemon half (`WRITE_FIXED(device fd ← slot)` —
zero daemon copies for eligible WRITE shapes), extraction demoted to
the ineligible-shape fallback. Part 0 first: the 7.1 out-paged-mirror
divergence blocked the local venue (readdir EIO on every armed mount).

Commits: `3bc16bda` (red: track-keyed mirror + zc-ledger stats
contract) · `cecc318b` (Part 0: track-keyed mirror, guards name
op+track, fuse3 tracing audible, write-direct ledger pair) ·
`0cd4420e` (red: the D14 leg's contracts) · `efcfc2f5` (Part 1:
dispatch-before-extraction + the direct DMA leg) · `b0bd0a20`
(FUSE-3g held-length accounting — the live-found blocker) ·
`75c8ead1` (the A-B-B-A rig + table).

## Part 0 — the 7.1 out-paged-mirror divergence (root cause + fix)

**Live repro** (local dev box, `7.1.6-1-cachyos-sqz`, armed mount):
every `getdents64` returned EIO (`ls` unusable); create/write/read
fine. `fuse3_zc_fallbacks`/`slot_payload_skips` flat 0 — and **zero
log lines anywhere** (see "loudness" below).

**Root cause — the mirror is a property of the running kernel's FUSE
TREE, not of the zc series.** `7eccd53a` pinned READDIR[PLUS] out of
the out-paged mirror on the 6.19.14-sqz field measurement. Both
verified from source:

* **6.19-sqz** (elrepo EL8 base): vanilla fuse readdir —
  `fuse_readdir_uncached` uses a **kvmalloc buffer**
  (`args->out_args[0].value = buf`), `out_pages` never set → the
  kernel attaches a kmbuf and COPIES readdir replies (exactly what the
  field probe measured).
* **7.1-sqz** (CachyOS 7.1.6 base, `/home/justin/.cache/cachyos-km/…/
  cachyos-7.1.6-1.tar.gz` — the shipping package's own tree): the
  Cachy sauce carries a **page-buffer readdir** —
  `fuse_readdir_alloc_buf()` bulk-allocates pages and sets
  `ap->args.out_pages = true`. On a zc queue `can_zero_copy_req` is
  then true for READDIR[PLUS]: no kmbuf attach, folio copy skipped at
  COMMIT → the daemon's kmbuf-bodied reply hit the no-attachment
  guard → EIO (loud-never-corrupt, as designed — now also correct).
  Vanilla 7.1 readdir is UNCHANGED from 6.19 (verified `git diff
  v6.19..v7.1 -- fs/fuse/` — the readdir.c delta is cosmetic); the
  divergence is the distro patchset, which is why the mirror keys on
  the resolved TRACK, not a version.

**The fix (track-keyed mirror, `cecc318b`):**

| track (opcode ladder) | out-paged | in-paged |
|---|---|---|
| 6.19-sqz (37/38) | {READ, READLINK} — field-measured `7eccd53a` | {WRITE} |
| 7.1-sqz (38/39) | {READ, READLINK, READDIR, READDIRPLUS} — source-verified + live-proven | {WRITE} |

`zc::out_paged(op, KmbufTrack)` / `in_paged(op, KmbufTrack)`;
`KmbufOpcodes` carries its `KmbufTrack` id; the worker captures the
ladder-resolved track once. Uncertainty degrades toward INCLUSION: an
op in the mirror the kernel serves copyable pays one clean counted
fallback (a vanilla-fuse 7.1 build = +1 fallback per readdir, output
correct); an op missing from the mirror hits the EIO guard — which now
**names the opcode + resolved track** in both miss directions and
counts `fuse3_zc_fallbacks` on the reverse miss too.

**Loudness was structural**: the daemon logs through `log`/env_logger
and installs no tracing subscriber; fuse3's `tracing` lacked the `log`
feature — every transport `error!`/`warn!` (including the guards whose
whole design is "loud") was invisible. The fork's tracing now carries
`features = ["log"]`; guard lines reach `--log-file` (verified live);
`debug!`/`trace!` hot-path macros stay short-circuited by the level
filter.

**Stats visibility (the second Part-0 item)**: the zc keys were never
missing — they live under the stats JSON's **`metrics` object** like
every counter family (the bracket rig reads `d.get('metrics', d)`; the
orchestrator's top-level scan was the error). Pinned by
`fuse_zc_ledger_always_exports_under_metrics` (metrics_tests): the
full family (`fuse3_kmbuf_negotiated`, `fuse3_zc_{negotiated,replies,
fallbacks,slot_payload_skips}`, `fuse3_zc_write_{extractions,
extract_bytes,directs,direct_bytes}`, `read_zc_serve_bytes`) always
exports as u64s under `metrics` and never at the top level.

**Part-0 verification (local armed 7.1 mount, post-fix):** `ls`/`find`
work; create/readdir/readlink/md5 round trip exact (buffered AND
O_DIRECT); 5×`ls` = +6 `fuse3_zc_replies` (readdirplus riding the
bounce bridge), fallbacks/skips 0 — the empirically-determined 7.1
mirror confirmed live. Fork suite 149 green; targeted root suites
green; clippy both configs + fmt clean.

## Part 1 — the D14 write-side leg (design ledger)

**The shipped architecture — dispatch-before-extraction + the W1
direct leg:**

1. **Held-slot delivery**: an armed WRITE no longer extracts at
   delivery. The payload stays in the sparse slot; the pool's
   `ZcHeldTable` publishes the held length per (qid, ent); the request
   dispatches IMMEDIATELY with an empty placeholder (oversize refused
   loud as before). FUSE-3g's body accounting counts the held length
   (the `b0bd0a20` fix — the live venue found the placeholder reading
   as Overdeclared and EINVAL-ing every write before `handle_write`).
2. **The slot source** (`ZcWriteSlot`, the `ZcReadServe` precedent):
   minted by the WRITE handler from the connection's held table;
   `store(fd, dev_off)` = `WorkerMsg::ZcStore` →
   `WRITE_FIXED(device fd ← slot)` on the queue ring
   (`ZcPend::HandlerStore`, raw CQE result forwarded);
   `materialize()` = `WorkerMsg::ZcExtract` → the old
   slot→memfd bridge on demand (`ZcPend::LazyExtract` — the CQE mints
   the §5.4 bounce lease and answers the parked handler), **memoized**
   so the fencing-retry loop never re-extracts.
3. **Eligibility ladder v1 = the W1 sole-owner patch class** (charter
   shape (a)): every existing predicate verbatim (aligned, sub-block,
   non-extending, non-adjacent, sole-owner via the §5.1 fence
   protocol, passthrough, undecorated whole-block mapping) — the
   direct leg only swaps the DMA VEHICLE inside the same
   begin/publish window: `authorize_zc_store()` (the RES-6/S7
   `fence_gate` the `write_block` worker runs — a fence refusal fails
   loud and never falls back) → `store()` sourcing the caller's
   registered pages → full-length success counts
   `fuse3_zc_write_directs/_bytes` (the engagement pair). Any
   decline/failure falls back to the pooled vehicle under the SAME
   sole-owner window — a partial direct store is fully overwritten by
   the pooled DMA (self-healing). `--write-verification` mounts
   decline the direct leg (the pooled path's window-exact read-back is
   the verifier).
4. **Shape (b) fresh-block streaming: NOT BUILT — structurally unsafe
   as charterable, stated per the charter.** A direct-DMA'd segment's
   acked bytes would live ONLY in an allocated-but-unpublished device
   block. The write path's own law (src/fuse_client.rs, the
   write-checkout comment): *"OVERLAY NEVER INVISIBLE … the entry now
   stays in the map at every instant"* — acked bytes must be
   composable by a concurrent read at every instant, and pre-publish
   the `ActiveBlockBuf` overlay is their ONLY authority. Direct
   streaming deletes the RAM copy, so a read between segment-DMA and
   publish composes zeros over acked bytes (the fstests generic/209
   class the overlay law exists for). Making it safe needs a
   DEVICE-BACKED overlay compose (coverage runs + unpublished device
   offset served by the read path) — its own campaign. Sequential
   rows therefore ride the lazy-extraction vehicle (which
   dispatch-before-extraction already improves: the extraction wait
   moved off the delivery path and overlaps the handler prelude).
5. **ACK semantics**: unchanged for the direct leg BY CONSTRUCTION —
   the W1 patch always held the reply across its in-place DMA; the
   direct leg swaps the vehicle, not the timing, so the charter's
   "reply-held-to-DMA collapses buffered streaming" concern does not
   arise (streaming never takes the direct leg in v1).
6. **Invariants**: `authorize_dma` custody/fence law runs verbatim at
   the new submission point; coverage-union `record_write`, killpriv,
   lock order (3/3.5), the §5.4 lease-severance law (the bounce lease
   stays bounded to one handler invocation; the memoizing handle drops
   with the handler) all untouched by construction — the direct leg
   lives INSIDE `try_sole_owner_patch`, which takes no pipeline permit
   today and still doesn't (in-handler synchronous DMA, ent-bounded;
   in-flight bytes are the CALLER'S pages — zero daemon bytes, no R5
   component owed). Slot-state contracts, tail-loss ×10, coverage,
   never-lossy suites all green post-change.

**Live local proof (7.1-sqz armed mount, `b0bd0a20` binary):**
16 MiB buffered write+fsync = 22 lazy extractions / 16.78 MB extract
bytes (write_through 3 blocks); aligned 4 KiB O_DIRECT overwrite =
`fuse3_zc_write_directs` **+1** / 4096 B / `patch_writes` +1 / ZERO
extraction — the slot→device WRITE_FIXED live on the real kernel; md5
exact buffered + O_DIRECT; P0 `cp && sync file` ×3 clean; readdir
green throughout.

## Verification (targeted, both workspaces)

* fork: 149 green (`cd crates/fuse3 && cargo test`), incl. the
  track-keyed mirror pin, the ladder-track identity, the ZcHeldTable
  contract; clippy `-D warnings` + fmt clean.
* root: `fuse_zc_write_tests` (injected slot source — direct
  engagement + ledger + device-honest read-back, materialize
  fallbacks, failed-store fallback under the same sole-owner window,
  memoized retry) + `extent_patch_tests` (20) +
  `write_through_coverage_tests` + `write_pipeline_tests` +
  `rw5a_never_lossy_tests` + `fsync_writeback_tail_loss_tests` +
  `metrics_tests` + `fuse_zc_serve_tests` +
  `transport_lease_overlong_tests` + `multi_queue_tests` all green
  single-threaded; ×10 loop on {fuse_zc_write, fsync_writeback_tail_loss,
  write_through_coverage, rw5a_never_lossy}: 10/10 green (restarted
  from zero after the `b0bd0a20` fix per the multi-run discipline);
  clippy `--all-features` AND shipped-config `-D warnings` clean; fmt
  clean.

## Found-latent hand-off: post-close GETATTR can adopt a mid-writeback size floor

The first bracket attempts flushed out a class the (intended)
loudness fix had armed: 43 of the fork session's 45 `#[instrument]`
handler spans carried no level → the tracing `log` feature made every
FUSE op write an INFO line through env_logger's mutex to the log file.
Under that per-op tax the P0 smoke (`cp 32MiB f && sync f` → md5)
failed **20/20 on the field** (fabric venue; local loop venue 0/10) on
BOTH zc postures — and the counted A/B pinned it: base `aca64de1`
0/20, Part-0-only binary 20/20, same venue, same shape.

The signature is NOT data loss: `stat` right after `sync f` reads the
file SHORT by exactly its tail block (29360128 = 28 of 32 MiB), the
stored bytes are intact (md5 exact after the kernel attr TTL), and the
op tape (now readable thanks to the same loudness fix) shows the
mechanism: cp's 32 WRITEs dispatch → RELEASE → **GETATTR** (u=16004) →
LOOKUP×2 → OPEN → FSYNC → RELEASE. The GETATTR runs between cp's
close and `sync`'s fsync while the LAST WRITEs are still unreplied —
the daemon legally serves the attr floor as of that instant (28 MiB),
the post-close kernel adopts the smaller size, and the 1 s kernel attr
TTL then serves the stale i_size to every stat/read until it expires.
The per-op log tax stretched the write-handler window enough to select
this schedule deterministically; base's timing never selects it on
this venue. Classification: a LATENT attr-floor-vs-in-flight-acks
race, pre-existing on the timing axis — the daemon could defend by
flooring served attr sizes at the ino's in-flight write high-water
mark. HAND-OFF: needs its own red-first campaign (the op tape above is
the repro recipe; a `SQUEEZEFS_TEST_WRITE_STALL_MS`-class seam selects
the schedule deterministically in-process).

The fix here (`a1ff7782`): handler spans are per-op DEBUG surface and
now say so — `level = "debug"` on all 45; `warn!`/`error!` (what the
loudness fix was FOR) stay audible. Post-fix field probe: 0/20 (below).

## Field acceptance (squeeze-test: EL8, 6.19.14-sqz — the 6.19 track)

Rig `.benchmarks/rigs/2026-08-06-zc-write-side-rig.sh` (extends the
DO-NOT-FLIP pricing rig; FATAL require-mount/arm-proof/correctness/
engagement gates + the per-armed-leg direct-DMA micro-engagement smoke;
armed rows gauge the direct+extract VEHICLE SPLIT; per-row amp =
device÷user bytes and wareq-sz per the standing law). Rows per write
leg: seqwr 1M (48 GiB fileset) / dur (fsync_on_close, 16 GiB — see the
placement-skew hand-off below) / rand4k (HOLE regime — the previous
bracket's shape, kept for comparability) / rand4kow (prewritten
durably-published fileset — the overwrite regime where W1/D14 engage).
Instrument: fio 3.36 libaio direct=1, 60 s + 10 s ramp, nvme-tcp
fabric substrate (stated per the standing rule). Reference
DO-NOT-FLIP bracket: 0.998×/0.909×/0.936×
(`.benchmarks/2026-08-06-fuse-zc-write-bracket.md`). NOTE: `exa_perf`
had been deleted from the volume since that bracket; it was recreated
(128 GiB prefilled, fsync_on_close) before any counted run — venue
stated because the read-sentinel ABSOLUTE level did not reproduce
(control 24.0 vs the earlier 27.3–27.9 GB/s; the A/B ratios are this
campaign's claims, absolutes are not).

### Venue hand-off #2: close-storm placement skew (found by the rig, counted)

Three counted failures with one signature before any valid bracket:
the dur row's fsync_on_close storm concentrates the ENTIRE fileset
onto 1–2 of the 5 data volumes until one refuses StorageFull (48 GiB
fileset → nvme18n1 12288/12288 full; 32 GiB → nvme12n1+nvme14n1 both
47.9/48 GiB; 24 GiB on the fourth-leg aged store → nvme12n1 47.8 GiB
while every sibling held 26 GiB — the whole fileset on ONE volume),
with reclaim healthy (queue 0, discards flowing) and set-level free
space plentiful (63–87 GiB). VL4b's balance-aware placement is
defeated by the close-time durable-upload burst on an aged store.
HAND-OFF: its own campaign (the `squeezefs df` tapes are in
zcws-{2,6,7}.log on the host). The dur row shrank to 16 GiB (fits one
volume's share even under total concentration) so it prices the WRITE
VEHICLE, not the skew wall — deviation stated.

### zcws-8 — the ALL-LAZY design's counted rows (the falsified arm)

The first complete bracket ran the all-lazy dispatch (every armed
WRITE held; extraction on demand from the handler task). Engagement
exact on every row; correctness green ×6 legs. The table FALSIFIED
lazy-everything for the streaming population:

| row | armed med | control med | ratio | brackets |
|---|---|---|---|---|
| seqwr | 20.898 | 21.788 | 0.959× | 0.992× / 0.929× |
| dur | 19.175 | 31.357 | **0.612×** | 0.566× / 0.657× |
| rand4k (hole) | 1.302 GB/s (318k IOPS) | 1.457 | 0.894× | 0.904× / 0.884× |
| rand4kow (overwrite) | 1.164 (284k IOPS) | 1.235 | 0.943× | 0.973× / 0.915× |

* dur armed legs ran at 38–42 % box busy vs 74–75 % control — the
  per-request handler-task round trip (WorkerMsg + worker wake +
  oneshot wake per 1 MiB extraction) starved the delivery pipeline
  that the at-delivery BATCHED extraction (0.998×/0.909× in the
  pricing bracket) never paid.
* rand4kow: direct share **99.4–99.5 %** of vehicle bytes (the D14
  leg carried the row; `patch_writes` ≈ every op; amp 1.00,
  wareq-sz 4 KiB both sides) — yet armed still lost 5.7 % at median:
  at 4 KiB the two task hops of the store round trip
  (handler→worker→WRITE_FIXED→oneshot) outweigh the copy+extraction
  they delete. armed p50 was BETTER (0.31 vs 0.55–0.58 ms); p99 worse
  (9.4 vs 3.9–4.4 ms).
* rand4k (hole): direct 37–38 % (the patch-eligible slice), extraction
  the rest; 0.894×.
* Read sentinel: armed 33.3 vs control 24.0 GB/s (+39 %, daemon CPU
  −69 %) — the K1-kill read win reproduced RELATIVELY; both absolutes
  sit below the earlier venue's (see the venue note).

### The hybrid (b4bcbb20) and the zcws-9 run — INCOMPLETE, wedge found

`zc::hold_candidate` (aligned, nonzero, < payload/2 — the
geometry-derived streaming bound) now splits delivery: candidates
HOLD (direct-DMA reachable), everything else extracts AT DELIVERY on
the worker's batched drain pass (the zcws-6-era vehicle restored —
`ZcPend::WriteExtract`). zcws-9 W1 (armed): seqwr 21.524, **dur
32.949 GB/s** (controls 31.3–31.4 — the 0.612× collapse REPAIRED, the
armed durable row at/above control for the first time in any bracket),
rand4k 1.307 (direct 37.5 %) — all gates PASS. W2/W3 (controls) clean.
**W4 (armed, the most-aged leg) WEDGED mid-seqwr**: uniques delivered
and never replied across qids 23–30 (oldest ≈ 57 min), every queue
worker parked healthy in `io_cqring_wait`, `.stats` reads hang, fio in
uninterruptible sleep — the transport watchdog named every slot
(`transport_slots_overdue`, the FUSE-2 instrument doing its job). A
load-selected lost-bridge-CQE class on the armed hybrid: FIRST-CLASS
product bug by repo law, and by itself a stronger DO-NOT-FLIP than any
slow row. HAND-OFF: the wedge tape (overdue-slot census per qid,
worker wchan states, the W4 leg log) — root-cause with a red-first
cargo repro before any further armed write acceptance; the venue
recipe is {aged store, saturated 1 MiB seq-write, armed hybrid,
6.19-sqz}.

## Flip verdict

**DO NOT FLIP — `SQUEEZEFS_FUSE_ZC` default stays OFF** (`src/env_knobs.rs`
untouched; no default-pinning test names it ON).

* The flip rule required all write rows ≥ 0.97× and the read sentinel
  ≥ 39.5 GB/s. The only complete bracket (zcws-8, all-lazy) measured
  0.959× / 0.612× / 0.894× / 0.943× and a 33.3 GB/s sentinel (venue
  regressed — see note). The hybrid repaired the dur row (W1 armed
  32.9 vs 31.4 control) but its acceptance run is INCOMPLETE: the W4
  armed-leg wedge is disqualifying on its own terms.
* What a future flip needs: (1) the wedge root-caused + repro-ported;
  (2) a cheaper small-write store round trip (the 4 KiB rows lose on
  task hops at 99.5 % direct engagement — batching store CQE wakes
  through the doorbell discipline is the obvious lever); (3) a
  re-baselined venue (exa_perf recreate + the placement-skew fix) so
  the 39.5 GB/s sentinel is even reachable — control read measured
  24.0 GB/s on today's venue.
* The knob remains live, loud, engagement-gauged measurement surface;
  armed WRITE rows are now fully attributable (direct vs extracted
  bytes exact on every counted row).

## Postures left + artifacts

* **Field** (squeeze-test): mounted on `b4bcbb20` in the DEFAULT
  posture — `fuse3_zc_negotiated=0`, both write-vehicle ledgers 0,
  P0 cp+sync-file clean, verified post-remount (the zcws-9 wedge was
  recovered via the FUSE-connection abort; fio/daemon drained clean).
* **Local** (7.1.6-1-cachyos-sqz): mounted ARMED on the same commit's
  dev build — readdir green (the Part-0 fix live), streaming rides the
  at-delivery extraction arm (21 extractions/16 MiB), the aligned
  overwrite rides the direct DMA (+1 direct, +1 patch, zero
  extraction), md5 exact, P0 ×3 clean.
* Artifacts on the field host: `/scratch/tmp/zcws-8-keep/` (the
  complete all-lazy bracket — the table above), `/scratch/tmp/zcws-9/`
  (the hybrid's W1–W3 rows + the W4 wedge tape),
  `/scratch/tmp/zcws-{8,9}.log`, per-leg mount logs
  `/scratch/tmp/logs/sqz-W*-zc*.log` (the W4 log carries the
  `transport_slots_overdue` census), the ENOSPC/skew tapes in
  `/scratch/tmp/zcws-{2,6,7}.log` (removed dirs; logs retained).
  Analysis: `.benchmarks/rigs/2026-08-06-zc-write-side-table.py`.
