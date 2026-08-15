# Overlay overwrite arm (B4) — B4d/B4e acceptance and the default adjudication

**Date:** 2026-08-15 (local B4d rows 2026-08-15; field B4e rows 2026-08-15)
**Design:** `docs/design-overlay-overwrite.md` (rev 4) — PRs B4a `2e7c63d3`, B4b `96bc360b`,
B4c-i `8e1be0d1`, B4c-ii `2bfa4704`; measured binaries `a2970b42` (local) / `1705f0d0` (field).
**Verdict:** `SQUEEZEFS_OVERLAY_OVERWRITE` ships **default ON** (KD-B4-9 confirmed), field-adjudicated
on the deciding venue. `SQUEEZEFS_OVERLAY_CLOSE_BARRIER` (OQ-5) stays **default OFF** by design
posture; its price is recorded (≈ 0 on both venues).

## The claim under test

Sustained mapped-block overwrite ingest paid one full daemon NT-memcpy pass over ~97 % of its
bytes (the 2026-08-14 field conviction: `nt_copy_bytes ≡ write_through_bytes` at 96.6 % of a
31.1 GiB/s row). The B4 arm routes those overwrites over the zero-copy slot→device overlay with
a fresh CoW dest, publishing by feeding the rewrite epoch — deleting the copy.

## Field rows (B4e — the deciding venue)

Box `memp-s3ds-aqs-37`: 32 CPUs, 2×200 GbE nvme-tcp, CPU-bound at the merge wall. Binary
`1705f0d0` both legs (KD-7). Row: 8 jobs × 8 GiB, bs=4M, qd4, libaio, direct; pre-fill pass then
60 s `time_based` overwrite; fresh mount per leg; A-B-B-A order ON→OFF→OFF→ON + one barrier leg.
Instrument: fio 3.x + `.stats` snapshot deltas (`/home/justin/Source/tmp/*_{before,after}.json`).

| Leg | Sustained | io (60 s) | daemon jiffies | jiffies/GiB |
|---|---|---|---|---|
| ON1 | **32.1 GiB/s** | 1927 GiB | 51,016 | **26.5** |
| OFF1 | 31.0 GiB/s | 1858 GiB | 98,581 | 53.1 |
| OFF2 | 30.9 GiB/s | 1854 GiB | 97,695 | 52.7 |
| ON2 | **31.6 GiB/s** | 1893 GiB | 51,692 | **27.3** |
| ONbar (OQ-5) | 31.6 GiB/s | 1897 GiB | 50,970 | 26.9 |

* **Throughput: ON wins both orders** (+3.5 % / +2.3 %) — materially above the control, order-independent.
* **Daemon CPU HALVED**: 26.5–27.3 vs 52.7–53.1 jiffies/GiB (−49 %; ≈ 7.9 cores freed at 32 GiB/s)
  — the efficiency-doctrine face, larger than the throughput face because the freed CPU is the
  fleet's headroom.
* **Engagement exact (row-validity)**: overwrite share **0.999** of user bytes
  (`overlay_overwrite_bytes` ≡ `overlay_store_bytes`); `nt_copy` share **1.000 → 0.001** — the
  96.6 % merge-share wall deleted, byte-exact; `overlay_ack_early_bytes ≈ overlay_store_bytes`
  (KD-B4-10's separate share check) to 4 decimal places; ~492 k/484 k epoch feeds;
  `overlay_feed_fallbacks` 0, `invariant_tripwires` 0, `rewrite_amp` **1.0000** both arms;
  `overlay_ineligible_shadow_bound` 541–972 per ~490 k feeds (the same-epoch re-overwrite
  residual is negligible at field fileset size).
* Fresh rows: only ON1's first pass is a true first touch (30.5 GiB/s); later "fresh" passes
  overwrite the prior leg's fileset and split by lever accordingly — labeled, not sentinels.
* **OQ-5 price: free on this venue** (31.6 GiB/s, same jiffies). The lever stays default OFF on
  the design's posture grounds (taking it for the overlay alone would fork the rewrite program's
  durability class — design §OW-8/OQ-5), but operators who want overlay-fed coverage closes out
  of the DUR-2 volatility class can take it at ≈ zero cost here.

## Local rows (B4d — tcp devsub, the venue split)

strixhalo, zram-backed nvmet-tcp devsub (`SQZ_DEVSUB_TRANSPORT=tcp`), binary `a2970b42`, fresh
substrate per leg, 8 jobs × 1 GiB bs=4M qd4, 60 s sustained; A-B-B-A ON→OFF→OFF→ON.

| Leg | Sustained | jiffies/GiB (sustained) | overwrite share |
|---|---|---|---|
| ON1 | 783 MiB/s | 34.6 | 0.807 |
| OFF1 | 1149 MiB/s | 50.0 | 0 |
| OFF2 | 1161 MiB/s | 48.8 | 0 |
| ON2 | 920 MiB/s | 34.0 | 0.793 |

The **above-control gate FAILED both orders locally** — the B4d falsifier fired, and the residual
was profiled and named per the honest-failure discipline:

1. **The depth term (dominant).** The OFF control's merge path decouples client pacing through the
   write-pipeline BDP governor (`write_pipeline_depth_target` converged **253 MB** in flight vs
   ON's client-shaped 100–119 MB) on a venue with ~144 ms per-4 MiB service — a device-bound
   venue rewards pipelining depth over copy deletion. Signature: ON at qd16 recovers to
   940 MiB/s. The field venue is CPU-bound; the term inverts there (the field tables above).
2. **The mid-row discard stream: priced ≈ 0.** qd4 ON legs issued ~2.1 k fabric discards
   (~9 GB trim — the manners law's idle catch-up in offered-load gaps; the qd16 leg issued zero).
   `SQUEEZEFS_RECLAIM_BATCH_MS=60000` leg: 921 vs 920 MiB/s — not the loss.
3. **`overlay_ineligible_shadow_bound` 9–14 k/row** = same-epoch re-overwrites (the 8 GiB local
   fileset re-passes every ~8 s) declining to the merge path — the whole ~20 % local residual
   merge share. Structurally small at field fileset size (0.1 %).

Local correctness columns matched the field: engagement exact, `rewrite_amp` 1.0000,
dev/user ≤ 1.002 (write-amp instrument), fallbacks/tripwires 0, ENOSPC declines 0, fresh-arm
first-touch sentinel at par (ON 875–982 vs OFF 797–931 MiB/s bands overlap). OQ-5 locally:
796 MiB/s, inside the ON band.

## Adjudication

* **Default ON** (registry line + `device_overlay::overlay_overwrite_enabled` + the
  `overwrite_knob_registry_defaults` drift pin flipped together, red-first). The deciding venue
  is the arm's premise — CPU-bound fabric ingest — and it won both orders on both doctrine faces.
  The **venue split is recorded on the registry line**: device-bound substrates (the local
  zram-tcp class) prefer `0`, and `0` remains the exact B2 control.
* The interim default-OFF posture (`1705f0d0`, user ruling 2026-08-15 pre-field + the
  falsified-lever rule on the local bracket) is superseded by this adjudication the same day.
* **OQ-5 stays OFF** (posture, not price). Recorded: ≈ free on the field venue.
* The `stress_recycled_keys_v3` sentinel pins the arm ON explicitly (it convicted the 2026-08-14
  compose fetch-vs-terminal wrong-serve and the double-owner free) — correctness coverage never
  follows the shipped default.
* Ladder: **B4d and B4e are DONE**; the parent `docs/design-device-overlay.md` B4 row flips to
  DONE via this note. The write-amp rig's overlay-overwrite engagement columns remain a rig
  follow-on (the columns exist on the stats inode; the B4d cells extracted them directly).

## Depth addendum (2026-08-15, same box/binary — the offered-load probes)

The +2–3.5 % headline above UNDERSTATED the arm: the qd4 row shape was offered-load-bound
(8 jobs × qd4 = 128 MiB in flight against a ~4 ms round trip), which masked the win. The probe
legs (fresh mount per leg, 60 s sustained, engagement columns from the same snapshot discipline):

| Leg | Shape | Sustained | daemon j/GiB | Engagement |
|---|---|---|---|---|
| ONref | 8×qd4 | 32.1 GiB/s | 26.7 | ow 0.999, amp 1.0000 |
| **ONq16** | 16×qd8 | **41.6 GiB/s** | 39.5 | ow 0.970, amp 1.0000, trips 0 |
| ONboth | 16×qd8 + fusion lever | 41.4 GiB/s | 40.5 | fusions **0** (label-only) |
| ONfuse | 8×qd4 + fusion lever | 31.9 GiB/s | 26.6 | fusions **0** (label-only) |
| **OFFq16** | 16×qd8 | **30.1 GiB/s** | 63.7 | control |

1. **The control is depth-FLAT** (31.0 at qd4 → 30.1 at 4× offered load): the B2 merge path is
   pinned at ~31 GiB/s by the daemon merge-copy CPU wall regardless of load — the original
   conviction, confirmed from the demand side. The sar capture during a control-class row shows
   both ports balanced at ~64 % util: a third of the wire idle while the control sits flat.
2. **The arm scales to the wires — CONFIRMED at the NIC**: ON at 16×qd8 = 41.6 GiB/s = 357 Gb/s
   payload; sar during the row (`ONwire`, a repeat leg at 41.6) shows BOTH ports at **90.3–90.7 %
   ifutil, balanced to < 2 %** (22.05–22.14 GB/s TX each ≈ 44.3 GB/s on the wire vs 44.7 GB/s
   payload — the delta is TCP/NVMe framing). ~90 % is practical nvme-tcp line rate on 200 GbE.
   At the deep shape the arm is **+38 % throughput at −38 % CPU/byte** vs the control. The
   remaining write wall on this hardware is the fabric itself — more bandwidth means more/faster
   ports (or an offloaded transport), not software.
3. **The fusion probes are label-only**: `SQUEEZEFS_FUSE_ZC_FUSION_MAX=4194304` turned but
   `fuse3_zc_write_fusions` stayed 0 — an upstream eligibility screen keeps whole-block 4 MiB
   stores off the fused path (the vehicle targets the W1/small population). The zc-write
   extraction (`fuse3_zc_write_extract_bytes` ≈ 100 % of row bytes, one memory pass/byte)
   remains a NAMED, unadjudicated CPU term — an efficiency-doctrine follow-on (extend fusion
   eligibility to whole-block held stores), not a bandwidth term at 89 % line rate.
4. Fresh-pass rows in all legs are 64 GiB/≈2.1 s bursts — label-only per the sustained rule; a
   real fresh-ingest number needs a time-based first-touch row over never-written files.
5. CPU/GiB rises with depth on the arm (26.7 → 39.5) — queue-worker economics at depth; the
   next "same for less effort" candidate once the wire wall is confirmed during a 41-class row.
