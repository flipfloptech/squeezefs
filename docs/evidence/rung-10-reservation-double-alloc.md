# Rung 10 — a fallocate'd lay-down held twice its size; a delete's blocks were no supply until the tick (2026-10-29; decision 0003 §CR)

**The report** (§CN.6 (c)). fio's lay-down — `fallocate(fd, 0, 0, 384 MiB)` per file, then 32 files × 1 MiB
O_DIRECT QD16 — transiently held twice its size; on 2 × 14 GiB OSTs, right after unlinking the previous pass,
the set latched "fresh supply EXHAUSTED" and the lay-down crawled until killed.

**Reproduced** (`~/tmp/resv/laydown.sh`: 2 MDT + 2 sparse OST images on the venue's disk, `--supervise`, a
sampler reading every volume's `used_blocks` from `.stats` each 100 ms, fio crc32c `verify_only` after every
pass; counts, never rates). Dev `5b86c9f`, 2 × 8 GiB, 32 × 128 MiB = 1,024 blocks: **peak 2,130 and 2,017
blocks**; `overlay_installs` 1,049, `overlay_epoch_feeds_fresh` 1,024, `alloc_fresh_mints` 2,073 (the 1,024
reservations + a fresh dest per block), `reserved_landings` 0. 2 × 5 GiB, 32 × 192 MiB, `rm` then the next
lay-down at once: fallocate reserved 1,008 of 1,536 blocks and 33–176 writes answered ENOSPC, with
`fuse_reclaim_destroys` 0 inside the pass. The crawl itself did not reproduce on disk-backed images (14 GiB ×4,
×6 with the §CN rows: latched in 2–3 samples, every pass completed); its precondition did, every time.

**Cause 1: the overlay's Hole arm minted over the reservation.** A reservation is an UNWRITTEN binding,
`lookup` a hole, so `claim` classed the piece a Hole and `installRecord` minted a fresh dest D; the settle FED
the epoch (R parked as the A key), and R was freed only after the save landed and its free's record was
durable. Only `accumulate.pickDest` took a reservation over; the overlay — the arm every ≥ 512 KiB aligned
piece takes — never did. **Cause 2: the corpse gather.** `rm` of 32 files leaves 32 corpses, under
`RECLAIM_BATCH` (64), so their destroy waits for the reclaim tick (5 s); until it runs their blocks are in no
word `freshSupplyVisible` reads, so the latch is terminal and a `.visible_supply` mint refuses at once.

**Red.** Unit `write_admit` "the fallocate'd lay-down through the device overlay" (`cfb5e5b`): `expected 3,
found 4` used blocks after the first half-block piece. Smoke "A FALLOCATE'D LAY-DOWN HOLDS ITS SIZE, AND A
DELETE'S BLOCKS ARE SUPPLY…" (`1899517`, on the first fix): after `rm` of four 64 MiB files, `f`, `g`, `h`
`falloc` ENOSPC (0 corpses destroyed), their writes ENOSPC.

**Fix 1** (`cc190f8`). A Hole record over a reservation this mount owns alone, on a passthrough volume, takes
the reserved block as its dest (`reservationVerdict`: W1's retire → sole verdict; no mint, no fsck guard). Its
bytes are nobody's — every reader serves the binding as zeros — so the overlay's "never in place" tear cannot
happen: a crash before the flip leaves R unwritten and the acked write absent. The settle re-checks the binding
under (3) (probe first; `overlay_reserved_landing_lost` must-stay-0), seeds the gaps with zeros in R, and
publishes the written twin durably at once — the same reference, never the epoch (its orphan discard would free
a block the layout names). A shared reservation still mints; a transformed volume's declines to accumulation,
which lands in place. **Fix 2** (`2463ce7`). `reclaim.Reclaimer.Upstream`: the corpse gather (gathered, or
inside a destroy pass until its frees reached the plane) joins `pending` — the latch stays inert, a
`.visible_supply` mint parks within its wall — and the ENOSPC valve kicks the cadence to destroy the tail now
(`block_free_upstream_kicks`, `fuse_reclaim_kicked_drains`); the kick never destroys on the caller's thread.

**Checks.** The lay-down (1,024 blocks): **peak 1,024 = size**, `overlay_reserved_landings` 1,024, epoch feeds
0, fresh mints = the reservations. 2 × 5 GiB fill → `rm` → lay-down ×10: every pass completed, 1,536 of 1,536
reserved, 0 ENOSPC, 0 latched samples, the rm's 32 corpses destroyed inside each pass by one kick, verified,
fsck clean. The §CN verified loop at 2 × 14 GiB with the rw4k/rw64k rows, no wait after `rm`, ×10: verified,
0 tripwires, 0 drift, fsck clean. Units `data.`, `fuse.handler.kv_fs`, `fallocate`: 724/726 (2 kernel skips);
`zig build fmt rails` green. Smoke: the new row, FALLOCATE — EVERY MODE'S LAW, SETID, the punch power cut, A
FULL DATA STORE ANSWERS ENOSPC, A FREED BLOCK IS NOT REUSED…, THE UNLINK STORM, A FULL METADATA VOLUME…, the
data plane with/without the write cache, the streaming overwrite, the KILL MATRIX, KILL -9 BETWEEN A
PROMOTION…, THE ORDERED LAW, kill -9 mid-tar: 55/56 (the looped row needs its knob) and the looped row ×24.
xfstests generic/001 013 074 075 112 127 263: pass, 0 alert lines; the fallocate set (008 033 059 071 086 091
092 103 213 214 228 286 299 311 315 347 391 422 436 439 445 446 469 512 539 567 568 586 590 610 627 703 749
758): 33 pass, 092 `_notrun`, fsck 0 on both sets.

**Seen, not fixed** (§CR.7). (a) generic/103 logs `dismount promotion: … 1 FAILED`: its `falloc` takes the block
a staged 64 KiB resident's claim counted on — a reservation ignores the write cache's claim (`storeCoversRing`);
on dev too. (b) generic/299's parked-gate ENOSPC lines (15; dev 38) and generic/347's thin-pool watchdog and
census lines (52; dev 36): the full-store regimes, on dev too. (c) `transport_lease_overlong` (a FUSE_WRITE
payload held ≥ 1 s) under the 512-deep lay-down at host load ~50, on dev too. (d) A block the write-back timer
settles partial is flipped written, so its later pieces CoW (36 of 1,536 blocks at most, freed after their
save). (e) `df` counts a gathered corpse's blocks used until its destroy (≤ one tick).

## The A-B-B-A (2026-10-29, quiet host on AC, the RAM-NVMe perf mount, 2 MDT + 2 OST, dist builds)

`dev` `6415dc1` against this branch (`72d1045`), two rounds of the fio matrix (one `baseline.sh ram-sqz`
psync session per leg; 4 legs per arm), mean of the legs: 4k randread ×32 −2.5 %, ×32×4 −1.0 %; 4k
randwrite QD1 −3.0 %, ×32 −0.6 %, ×32×4 −2.7 %; 64 KiB +2.0 / +3.8 %; 1 MiB seq read +0.3 / +0.2 %, write
+1.2 / +0.2 %; 4 MiB write −0.9 % (4k randread QD1 +8.7 % on one 89 k `dev` leg). mdtest (one round, both
shapes, -np 32/64): within the legs' spread. Neutral on throughput; the lay-down's peak blocks halve.
Landed after the rebase onto `1df4bdd` (drop-7.2 removed the overlay's early-ack counters; the conflict kept
this branch's reservation counters) — gate 184 green, 75/75. Sessions: `/build/perf/mx/results.tsv`
(`ram-sqz-rrc{A,B}*`, `ram-sqz-rrv2*`), `/build/perf/rc-md/`.
