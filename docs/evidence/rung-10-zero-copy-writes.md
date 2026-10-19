# Rung 10 — zero-copy big writes: the overlay class is held in its slot (lever (b) of `rung-10-seqwrite.md`)

**Diagnosis** (`rung-10-perf-baseline-workstation.md`): every WRITE larger than `block/8` was extracted
at delivery — `WRITE_FIXED(slot → bounce)` on io-wq, 0.77 ms/MiB — and the overlay arm then DMA'd the
bounce: two copies per written byte, io-wq 2.7 of the daemon's 5.5 cores on 1M seq write ×4, 14.7 GB/s
= 25 % of the raw 58.7 GB/s. Charter §6.2 item 1 says `WRITE_FIXED(device fd ← slot)`, zero copies. The
hold path existed end to end (`Payload.slot` → the overlay's slot vehicle → `storeHeld` → the
`direct_store` bridge on the ring that registered the slot) but only the W1 shape reached it: the
transport's `holdCandidate` (strict `payload/2`) and `w1Shape` (`≤ block/8`), and the plane's gate
(non-extending, `≤ patch_max`, not stream-adjacent) all excluded the streaming write.

**What changed** (`7ab7e68`). `zc.holdShape` proves the transport's half — 4 KiB-aligned, within one
block, non-empty, no bound of its own (a WRITE never exceeds `max_write ≤ payload`; a straddling write
splits into pieces that all materialize, so it keeps the at-delivery extraction). `zc.holdClass` splits
on `PATCH_MAX = block/8`: W1's ceiling IS the overlay's length floor, so the class costs no constant.
`plane.holdVerdict` (pure; the tie test restates it) holds the W1 class under the patch's own screens as
before (holding W1-ineligible shapes measured 0.45×) and the overlay class whenever the arm is on
(`SQUEEZEFS_DEVICE_OVERLAY` ∧ the new A/B lever `SQUEEZEFS_ZC_HOLD_OVERLAY` ∧ a passthrough volume) —
extending and stream-adjacent writes included, both payload classes: the port's overlay store takes the
slot synchronously (`commit_retain = false`, the CQE precedes the ack), so the O_DIRECT class needs no
snapshot and no late materialize (0002 row 544; the reference held page-cache deliveries only). The
held store's wait is the store cell's `Io.Event`: the fiber parks, its lane thread does not (the
overlay's `.bytes` vehicle parks the thread in `nvme_dev.writeBlock` — lever (a)'s problem, now paid
only by the fallback). A decline under the arm's guard (an epoch's save in flight, RAM custody on the
block) materializes lazily, counted. Counters: `transport_zc_write_hold_bytes` / `_lazy_extract_bytes`
(held bytes, and the one copy a hold can still pay), `transport_zc_write_hold_ns` (delivery → release),
`held_store_bytes`, `hold_gate_overlay_admits`, `transport_zc_write_direct_refusals`.

**The bound.** A held slot costs the transport nothing an extracted one does not: both stay Delivered
until the commit (the kernel keeps the app's pages in the slot until then either way); the hold swaps
the ent's bounce bridge for its device bridge — one pend per ent, the SQ sized `next_pow2(width × depth
+ 8)` for it — and the bytes in flight take the overlay's governor permit (a full pipe parks the holder
holding no lock, as before). No cap of its own was added. The one refusal the ring can answer, no SQE
after its flush, is `transport_zc_write_direct_refusals` and the write falls to the extraction vehicle
(`overlay_store_fallbacks`): 0 and 0 in every session below.

**Red → green.** `e2e zc WRITE, the overlay class`: a 1 MiB page-aligned write on a zero-copy session is
HELD and lands by `WRITE_FIXED(fd ← slot)` into the echo filesystem's memfd — byte-exact, directs +1 for
1 MiB, extractions +0; a 1 MiB write straddling the block edge and an unaligned write extract at
delivery; a declining filesystem extracts the 1 MiB at delivery. Red on the old predicate: the write was
extracted (holds +0; the scratch array answered EFBIG). Also pinned: `holdShape`/`holdClass` on every
shape, and `holdVerdict` by class (W1 declines extending / stream-adjacent, the overlay holds them, the
patch-OFF arm makes every shape the overlay's). Green: `transport` 138, `write_admit` 77, `data.plane`
50, `device_overlay` 61, `kv_fs` 91, the rails; smoke rows `the data plane` ×2 (ledgers closed, 243 /
251 held stores of which 45 W1), `KERNEL AIO`, `THE KILL MATRIX`, `kill -9 mid-tar -x` ×2, `THE ORDERED
LAW'S KILL ROW`, `sync -f` ×3, `THE WRITE-BACK TIMER` ×3 — every acked byte present, fsck clean.

**A/B on this workstation** (`scripts/perf/quiet.sh ram-sqz`, RAM-NVMe over nvme-loop, dist builds,
4 × 3 GiB lay-down, 20 s rows, the whole venue held; A = `dev` `887c334` from the main tree, B = this
branch; legs A B B A over the full matrix; mean B ÷ mean A; daemon cores with io-wq's share in
parentheses). Every session ended with `fsck --offline` clean.

| row | A (per leg) | B (per leg) | B ÷ A | daemon cores A → B (io-wq) |
|---|---|---|---|---|
| 1M seq write QD16×1 (MiB/s) | 5,108, 5,724 | 12,996, 13,916 | **2.48×** | 1.42 (0.59) → 2.00 (0.12) |
| 1M seq write QD16×4 | 13,758, 14,591 | 26,515, 27,475 | **1.90×** | 5.32 (2.53) → 5.75 (0.35) |
| 4M seq write QD4×4 | 8,273, 14,658 | 27,332, 28,935 | **2.45×** (1.92× against A's better leg) | 3.82 (1.94) → 5.51 (0.35) |
| 64k randwrite QD16×4 | 17,919, 18,748 | 19,169, 19,343 | 1.05× | 6.96 → 7.10 |
| 4k randwrite QD1 / QD32×1 / QD32×4 (IOPS) | 44.9 k / 213.9 k / 391.7 k | 43.6 k / 223.9 k / 400.5 k | 0.97× / 1.05× / 1.02× | — |
| 4k randread QD1 / QD32×1 / QD32×4 | 99.7 k / 294.7 k / 783.0 k | 104.7 k / 305.8 k / 845.3 k | 1.05× / 1.04× / 1.08× | — |
| 64k randread / 1M seq read ×1 / ×4 (MiB/s) | 31,574 / 39,101 / 63,834 | 32,426 / 41,323 / 66,038 | 1.03× / 1.06× / 1.03× | — |

The rows the change cannot reach (4k and 64k writes stay W1-class and held as before; the reads) move
−3…+8 %: the 4k randwrite QD1 pair (0.97×) reversed in the profiled pair (A 42.4 k, B 46.3 k; 1.01×
over the three pairs), and the same `dev` binary measured 57.4 k in the baseline session — the row's
session drift, not the lever. The iteration pair (`rb-A-r1`/`rb-B-r1`, 4 rows) read 5,800 → 14,159,
14,470 → 27,683, 14,259 → 28,648, 19,136 → 19,374. On sw1m ×4 (B): 530–550 k holds, every one the
overlay class, 99.7–99.9 % stored straight from the slot (`directs ≡ held_stores`), 0 direct failures,
0 refusals, 0 fallbacks; the 0.1–0.3 % lazy extractions are the rewrite epoch's save window
(`overlay_ineligible_shadow_bound`, paid by A as accumulation too); hold time mean 2.1 ms, mode 2–4 ms
(QD16×4 = 64 in flight at 27 GB/s); p50 3.9 → 2.3 ms, p99 13–18 → 6.5–8.2 ms.

**io-wq, by `perf record -a` mid-row (1M seq write ×4, share of all host samples):** io-wq 26.0 % →
2.6 %; the nvmet-loop target's kworkers (the substrate's one copy) 26.1 → 38.0 %; `sqz-fuse-ln` 8.4 →
16.8 %, `sqz-ckpt` 4.8 → 7.9 %, `sqz-jrnl` 2.7 → 6.3 %; `sqz-nvme` 5.8 % → below the top twelve (the
data DMAs ride the FUSE queue rings now). Sequential write stands at 27.5 GB/s = 47 % of raw (the
charter's bar is 36.4). What binds next, from B's inclusive daemon profile: the overlay settle's
`mapMerge` → `publishLayout` is 23 % of daemon samples, with `__munmap` 16 %, `fastMemset` 13 % and
`rwsem_down_write_slowpath` (the process mmap_lock) 12 % inside it — allocation churn on the per-block
publish path, 6.6 k publishes/s at this rate; the checkpoint and journal lanes each run at ~55 % of a
thread. Lever (a) is no longer on this row's critical path for the held store; it still is for every
`.bytes` DMA (the fallback, accumulation uploads, the gap seeds).

**Seen beside, not this lever's:** `data_alloc_population_drift` fired once on `rb-B-r1` sw1m ×4 and
three times on `rb-B-r3` sw4m ×4 (and once on another agent's `ra-B-r1`): `populationDriftCheck` runs
from the checkpoint cycle (`plane_arm.writeKvdaPages`) against a live bitmap with no exclusion from the
allocator's sets and clears, so at 6–7 k mints and frees per second the maintained count and the
popcount are read at different instants and the "re-sync" stores the torn count. A faster write path
makes it fire; it needs its own red test.
