# 2026-09-09 — 1.2.1 vs 1.2.2, shipped `dist` binaries, squeeze-test

Two purposes in one A-B-B-A: (1) the **field row the bounded-park fix
owes** (`.benchmarks/2026-09-08-generic-795-lookup-wedge.md` — landed on
in-process evidence during the release; a transport change with no
same-binary lever, so the arms are the two release binaries), and (2) the
campaign board's field rows re-measured on the **shipped profile** (the
2026-09-08 rows were `release`-profile dev builds).

## 1. Venue

| | |
|---|---|
| box | squeeze-test, 32-core Xeon, kernel `6.19.14-sqz` (patch 0031), the 5-node nvme-tcp fabric set (5 meta + 10 data namespaces), fresh `cluster_reset_v4.sh` format per arm |
| arms | **E** = `squeezefs 1.2.1 (27a396e1, tag stable-2026.09.1) profile dist`; **F** = `squeezefs 1.2.2 (e3635557, tag stable-2026.09.2) profile dist` — the rocky8 `dist` artifacts of each tag, same-tag shims |
| order | **E F F E**, 12:09–12:27Z, one boot, `write_BW` 40 s prep per arm |
| rows | the campaign-rows rig verbatim (`.benchmarks/rigs/2026-09-08-campaign-rows-abba.sh`): rr4k-kern, rw4k-kern, wdur-kern (`--end_fsync=1`), fsync-storm (24 × 256 KiB, fsync/write), rr4k-il control; 30 s each |
| reducer | `.benchmarks/rigs/2026-09-08-campaign-rows-reduce.py` — exact means, both orders shown |

Tripwires 0 on every row of both arms; `invariant_tripwires` 0; box busy
within 0.1–3 points across arms.

## 2. The rows (medians of 2, F/E)

| row | IOPS | p50 | p99.9 | daemon µs/op | notes |
|---|---|---|---|---|---|
| rr4k-kern (R-5) | **+6.3 %** (563.9 k → 599.7 k) | −2.9 % | −6.1 % | **−9.3 %** | both orders agree to 0.4 % |
| rw4k-kern (W-6) | **+13.3 %** (488.0 k → 552.8 k) | **−12.4 %** | +8.2 % | **−11.9 %** | every write the W1 patch (`patch_writes` ≡ ops), both orders agree to 0.3 % |
| fsync-storm (W-5) | **+24.4 %** fsyncs/s (2,605 → 3,242) | fsync p50 **−22.1 %** | sync p99 +2.4 % | +0.6 % per fsync | data sync REQUESTS 10 → 1 per fsync, physical 2.00 → 1.00; meta barrier one per fsync on both |
| wdur-kern (W-5) | +1.0 % (33.7 → 34.0 GB/s) | +2.1 % | −16.5 % | −5.3 % | PAR by construction (a 24-file stream touches every namespace) |
| rr4k-il (control) | +1.7 % | −3.8 % | −5.0 % | −3.7 % | the shim takes neither handler — bounds the free-standing term |

These reproduce the 2026-09-08 `release`-profile rows to within 1–2
points on every column (`.benchmarks/2026-09-08-campaign-rows-squeeze-test.md`:
+6.8 / +13.2 / +23.8 %), so the campaign's field claims hold on the
shipped `dist` profile.

## 3. The bounded park in the field (the fix's owed row)

The 1.2.2 arm carries the worker park that is EXT_ARG-bounded (100 ms)
while its drain group owes any reply. Its whole cost is the ticks it
takes and what they find:

| F row | `park_backstop_ticks` (30 s, 32 workers) | commit rescues | CQE rescues | slots overdue | `parked ≡ unparked` |
|---|---|---|---|---|---|
| rr4k-kern (pos 2 / 3) | 7 / 16 | 0 / 0 | 0 / 0 | 0 | ✓ (1,285,116 / 1,241,811) |
| rw4k-kern | 44 / 71 | 0 / 0 | 0 / 0 | 0 | ✓ |
| wdur-kern | 57 / 150 | 0 / 0 | 0 / 0 | 0 | ✓ |
| fsync-storm | 304 / 334 | 0 / 0 | 0 / 0 | 0 | ✓ |
| rr4k-il | 304 / 339 (the mount's cumulative count — the shim row parks nothing new) | 0 | 0 | 0 | ✓ |

Under load a worker almost never parks with a reply owed — at most ≈ 11
ticks/s across the 32 workers on the fsync storm, ≈ 0.2/s on rand-read —
and **every tick found nothing a wake should have delivered**: the two
rescue counters read 0 on all ten rows. That is the healthy posture the
fix's law predicts (nonzero IS the lost-wake tripwire), and the rows'
IOPS / CPU-per-op deltas above are indistinguishable from yesterday's
pre-fix `release` rows, so the fix's field cost is below this instrument's
resolution. The rows are 30 s brackets, not the ≥ 60 s sustained shape;
the wake-loss class this bounds has one field exposure in ~50 fstests
runs, so a sustained row cannot be expected to show a rescue either — the
counters exist for the exposure, whenever it comes.

## 4. Verdicts

* **The bounded-park fix's field row: MET** — cost unmeasurable, ledger
  clean, tripwires 0, parked/unparked closure exact on every row.
* **The campaign rows on the shipped profile: MET** — R-5 / W-6 / W-5
  reproduce within 1–2 points of the dev-build rows.
* 1.2.2 `dist` stays mounted on the box after the bracket (the rig's last
  arm is E; the box was returned to F by hand — verify with
  `.stats build_tag`).
