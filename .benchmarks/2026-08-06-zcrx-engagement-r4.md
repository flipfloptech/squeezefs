# 2026-08-06 — zcrx engagement round 4: THE VERDICT — the stride/chunk incompatibility hypothesis is FALSE; the park-class discriminator and the live-armed census land

Branch `perf/zcrx-stride-verdict` (worktree off `integrate/zcrx-wave`
tip `ee2e7456`, **unmerged — the orchestrator merges**). Charter: the
216 MiB area deployed and the field STILL starves (starved_ms = 8433 =
9 windows, structural = 4 of 10 — one vote short of the sweep, bypasses
318k/seqread row); adjudicate the BINARY hypothesis at the source, no
product code unless the answer is arithmetical. Source venue: the sqz
**linux-6.19.14** tree (the exact field kernel; mlx5/net-core/io_uring
files pristine stable except the io_uring/FUSE series, which does not
touch zcrx.c's provider ops or mlx5). Design:
`docs/design-zcrx-read-lane.md` Rev 4c.

Commits: red `94cc75ed` + amendment `ccd5e7c2` (see §3 — the first red
premise self-falsified in the same source walk; the amended contracts
are the honest ones) · green `426acb7c`.

## 1. THE VERDICT: (a) is FALSE — the lane is NOT structurally incompatible

**MPWQE strides never demand physically-contiguous memory.** The
per-WQE page path (`mlx5e_alloc_rx_mpwqe`, en_rx.c:773–855): allocate
`pages_per_wqe` INDIVIDUAL netmems — one `page_pool_dev_alloc_netmems`
per page (`mlx5e_page_alloc_fragmented`, en_rx.c:277–293) — and write
each page's DMA address into the WQE's inline MTTs (en_rx.c:804–808).
The UMR makes 64 discontiguous order-0 pages ONE contiguous IOVA range;
the 16 KiB "linear stride" of Rev 4b is the ethtool frames↔WQE
conversion unit (params.c:292–301), not a memory shape. The provider
contract closes exactly:

* non-XSK `page_shift` IS `PAGE_SHIFT` — "Regular RQ uses order-0
  pages, the NIC must be able to map them" (params.c:24–34);
* the UMR mode falls through to ALIGNED/MTT (params.c:37+ — every
  other arm is XSK-only);
* the provider-backed pool is created `pp_params.order = 0`
  (en_main.c:994);
* the zcrx provider REFUSES anything except
  `pp order + PAGE_SHIFT == niov_shift` (io_pp_zc_init, zcrx.c:1018) —
  the lane's 4 KiB chunks pass, and the field's 0.6 GB of
  pattern-correct zero-copy fills is the empirical co-proof (the whole
  path executed end-to-end, repeatedly).
* SHAMPO composes, it does not tax: the unreadable-MP branch gives
  headers a SEPARATE kernel-page pool (en_main.c:826–846); payload
  strides ride the provider pool (en_main.c:1005–1007).

**MTU has no bearing on compatibility** (it only moves the standing
term's stride bucket, Rev 4b). The campaign-ending stop is NOT filed;
no MTU-1500 trade needs pricing.

## 2. The free-pool stranding theories — also closed at the source

* The pool alloc path drains alloc-cache → ptr_ring → provider IN
  ORDER (`page_pool_alloc_netmems` → `__page_pool_get_cached` →
  `mp_ops->alloc_netmems`, page_pool.c:654–668) — chunks parked in pp
  caches are allocable, never stranded.
* A full ptr_ring overflows driver recycles to the provider freelist
  (`page_pool_return_netmem` → `io_pp_zc_release_netmem` →
  `io_zcrx_return_niov_freelist`, page_pool.c:753–773 +
  zcrx.c:994–1005).
* User refill returns wait in the rqe ring until the slow path pulls
  them (`io_pp_zc_alloc_netmems` → `io_zcrx_ring_refill`,
  zcrx.c:920–991; bounded pulls of `PP_ALLOC_CACHE_REFILL = 64`,
  types.h:55) — visible, not lost.
* Copy-fallback allocs draw provider niovs (`io_zcrx_copy_chunk` →
  `io_alloc_fallback_niov`, zcrx.c:1188–1290) — bounded by transit,
  and a term the class split below makes visible as `pool_dry`
  pressure with low engagement.

Every enumerable holder is funded at 216 MiB: posted WQEs (128 MiB —
Rev 4b, `frames × stride`, log_wqe-independent), in-flight payload
(≤ the 64 MiB admitted window; socket transit ⊆ issued ⊆ admitted),
partial-WQE tails (≤ 2 WQEs ≈ 0.5 MiB). **The residual starvation is a
LIVE-INPUT question the source cannot answer** — and it was unanswerable
in the field because of an instrument gap WE own (§3).

## 3. The red self-falsification (recorded per the counted-run
discipline) and the honest contracts

The first red commit (`94cc75ed`) contracted funding the pp ptr-ring
"float" (`min(pool_size, 16384)` entries, page_pool.c:213–214) as a
fourth area component. The SAME source walk falsified it before green:
ring-parked chunks are ALLOCABLE (§2, alloc-order citation), so they
are free supply, not demand — funding them would be a 64 MiB constant
dressed as a derivation. The amended red (`ccd5e7c2`) replaced it with
the instrument the tape actually lacks:

* **The park errno-class split** — `classify_recv_end` files THREE
  different starvation terms under ONE Park verdict and one counter:
  `-ENOMEM` = provider pool dry (the area/pool arithmetic),
  `-ENOBUFS` = RQ ring starved (the refill-posting path),
  `-ENOSPC` = the lane's own CQ full (the reap term — round 7's class,
  zcrx.c:1280/:1317). Rounds 2–4 could not tell WHICH term starved.
  Landed: `park_class_name` + `zcrx_parks_{pool_dry,rq_empty,cq_full}`
  (stats inode + rig columns; closure `parks ≡ dry + rq + cq` per row;
  the episode-starting errno is the counted class and the park log
  names it). Note the round-2/3 "every window starved" reading is a
  DIFFERENT bug in each class — e.g. `cq_full` would indict the reap
  cadence under the 10-session fan-in, not the area at all.
* **The live-armed census denominator** (the structural=4-of-10 miss):
  `nic_census::note_released` on EVERY teardown (voter, poison,
  shutdown) — a torn session holds no RSS exclusion, so it carries no
  rent into the majority denominator; the voter votes while still
  counted live. The round-4 tape's 4th vote fires (2×4 ≥ 10 − 3) where
  ever-armed idled one vote short for the whole row. The round-3
  no-release pins hold unchanged (denominator identical when nothing
  tears).

## 4. Verification

Amended red `ccd5e7c2` compile-red at base (`park_class_name`, the
three counters, `note_released` — absent). Green: `zcrx_lane_tests`
**70** (67 + park-class + counters-export + live-denominator tape
contract) — ×10; in-module `--lib zcrx` **38**; steering 23; derivation
sweep; read-path spot battery; both clippys `-D warnings`; fmt;
markdown links — all clean/green.

## 5. THE FIELD ROW THAT DECIDES (one row, no more source campaigns)

Same A-B-B-A instrument; beside it, per-queue ethtool taps that need no
binary change: `rx[i]_pp_alloc_empty/slow` and pp inflight
(`hold_cnt − release_cnt`) on the lane-leased queues = the pool's
ACTUAL holding per row.

* `parks(dry=…, rq=…, cq=…)` — the class names the term:
  **dry-dominated** ⇒ the pool arithmetic still misses a live input —
  read pp inflight directly against the 216 MiB area and the missing
  MiB is a NUMBER, not a theory; **cq-dominated** ⇒ the reap cadence /
  CQ sizing under fan-in is the term (the round-7 arithmetic gets its
  own bracket) and the AREA was never the problem; **rq-dominated** ⇒
  the refill-posting path (rqe ring pull cadence) is the term.
* The live-armed census bounds the cost of whichever answer: the 4th
  vote sweeps the NIC (round-4's tape shape), so a starving geometry
  now costs ~2 s of rent, with `starved_ms`/`structural`/`bypasses`/
  park classes as the tape.
* If the row instead runs clean at 216 MiB (the round-3 fix + some
  transient the classes now bound), the round-1 engagement laws take
  over: share ≥ 0.5, `gather ≡ fill`, and the composed row against the
  35.5 bar.

## PROGRAM PARKED (user ruling 2026-08-06, verbatim): "if we can reach the
## numbers we want without zcrx I would rather do that since it seems much
## more portable if we can" + "it also doesn't seem like it's giving us the
## performance we expected on these servers anyway"

Adjudication: the ruling matches the portable-by-default law — the lane
requires kernel ≥ 6.15 zcrx, driver/provider cooperation, ntuple state and
RSS manipulation, and after 12 field rounds on this exact rail it has
served ~0.6 GB of ~1,600 GB rows while paying measurable RSS rent. The
machinery REMAINS (opt-in `SQUEEZEFS_ZCRX_LANE=1`, safe-by-construction:
no-harm economics arm + live-armed whole-NIC release + the park-class
discriminator make any future engagement attempt a one-row diagnosis).
The read-throughput program continues on the PORTABLE road: whole-block
cohort dest-DMA (the dest-lease's (C) extension), the warm-serve zc
follow-on (§8.1), and transport-ingress economy — no NIC or kernel
dependencies beyond what the product already ships on.

## The program's closing data point (the park-class row, run post-ruling)

`parks=5(dry=5,rq=0,cq=0)` on both A-sides — **pool_dry-dominated**: the
provider pool's real holding exceeds even the 216 MiB derived area; the
refill-posting and CQ arms are clean. The named residual for any future
resumption: read the lane-leased queues' pp inflight (`hold_cnt −
release_cnt`, free ethtool taps — no binary change) during one armed row
and the missing MiB becomes a number. Parked here.
