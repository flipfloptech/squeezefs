# Rung 10 — the cost of epochcap's union cap (epochperf)

`631f83f` (`rung-10-epochcap.md`) caps the open shadow UNITED with the closing epoch's at
`max_displaced` (64): while a save runs, a record of a NEW block answers `save_in_flight` and
waits for it. Before, the next epoch took up to 64 new blocks during the save. Question: does the
wait cost throughput? **Verdict: no. Every difference is within noise. No product change.**

**Venue.** kamrui (x86_64, 12 threads, 61 GiB, 7.2.6-sqz). Each side runs
`scripts/perf_mount.sh` in native mode with the dist binary: A `f0f5ee48b5e9`, B `59349ee554b5`
(`631f83f`; the rest of the delta is tests and docs). The set is 2 MDT (4 GiB) + 2 OST
(32 GiB) LVs on the local NVMe, with default knobs (write-back posture `timer`). Each session:
`blkdiscard` the LVs, `--fresh` format, lay down 4 × 2 GiB files (1 MiB O_DIRECT), then OVERWRITE
them: `fio --direct=1 --ioengine=io_uring --time_based --runtime=60 --end_fsync=1`. Each session
queued like a full gate and held the gate lock and every build slot, so nothing slotted ran beside
it. A 2 s sampler flagged foreign processes. One row overlapped a 24 × `yes` storm (A block-d2,
rw-4m-qd1-nj1) and is excluded. Reps: 4 MiB / seq-1m 3 per side (d1–d3, order ABBAAB); 4k / 64k
2 per side (the orchestrator stopped the runs once r2 agreed with r1).

**4 KiB and 64 KiB random overwrites never reach the rewrite epoch.** The data block is 4 MiB
(`CHUNK_SIZE`), and a sub-block overwrite patches in place. Both sides count 0 epoch records and
0 swaps, except the 64k qd16-nj4 row, which counts ≤ 24 records over the run. So the cap cannot
cost anything here. Whole-block overwrites do fill epochs back to back: about 10 k records and
500–890 saves per 60 s. They go through the device overlay. Its answer to `save_in_flight` is a
settle retry (`overlay_settle_retries`, with a ladder backoff of 1 ms that caps at 50 ms). That
retry is the wait counter. `rewrite_shadow_save_waits` (the accumulate path) stayed 0 on both sides.

| workload (60 s) | A MiB/s median [min–max] | B MiB/s median [min–max] | B/A | p99 ms A / B | B settle retries per run (A: 0 everywhere) |
|---|---|---|---|---|---|
| rw-4k qd1 nj1 | 31.3 [31.1–31.5] | 31.4 [31.1–31.7] | 1.00 | 0.22 / 0.20 | 0 |
| rw-4k qd1 nj4 | 114.2 [114.0–114.5] | 115.2 [115.2–115.2] | 1.01 | 0.18 / 0.18 | 0 |
| rw-4k qd16 nj1 | 89.6 [88.6–90.5] | 90.6 [89.9–91.2] | 1.01 | 1.14 / 1.14 | 0 |
| rw-4k qd16 nj4 | 223.1 [218.6–227.5] | 227.8 [227.6–228.1] | 1.02 | 3.00 / 2.93 | 0 |
| seq-4k qd16 nj1 | 52.5 [49.9–55.0] | 56.3 [56.2–56.3] | 1.07 | 4.14 / 3.59 | 0 |
| rw-64k qd1 nj1 | 410.2 [406.1–414.4] | 406.0 [396.2–415.8] | 0.99 | 0.27 / 0.27 | 0 |
| rw-64k qd1 nj4 | 606.6 [527.8–685.5] | 662.5 [600.9–724.1] | 1.09 | 1.27 / 1.20 | 0 |
| rw-64k qd16 nj1 | 662.1 [645.2–678.9] | 685.7 [678.6–692.7] | 1.04 | 4.9 / 5.9 | 0 |
| rw-64k qd16 nj4 | 623.1 [581.2–665.1] | 545.8 [464.0–627.6] | 0.88 | 13.9 / 14.5 | 0 |
| rw-4m qd1 nj1 | 618.3 [512.7–723.9] (n=2) | 766.8 [373.4–773.2] | 1.24 | 19 / 12 | 15 / 4 / 15 |
| rw-4m qd1 nj4 | 824.1 [762.1–831.6] | 735.3 [711.4–745.8] | 0.89 | 48 / 71 | 0 / 2 / 3 |
| rw-4m qd16 nj1 | 651.1 [600.7–664.0] | 619.8 [579.7–728.3] | 0.95 | 165 / 190 | 0 / 0 / 0 |
| rw-4m qd16 nj4 | 761.4 [701.2–817.6] | 762.2 [753.0–789.1] | 1.00 | 1002 / 1011 | 383 / 78 / 138 |
| seq-1m qd16 nj1 | 689.9 [666.3–746.7] | 691.4 [670.4–721.9] | 1.00 | 55 / 54 | 148 / 90 / 102 |

**Why this is noise.** Where the wait fires most (rw-4m qd16 nj4, seq-1m: 78–383 retries per
run), the medians are equal. The two rows that differ by about 11% barely fire the wait, or not
at all. rw-4m qd1 nj4 fired 0–3 retries per run, which is at most 0.25% of the run at the 50 ms
cap. rw-64k qd16 nj4 fired none and recorded 5–24 epoch blocks. The 4 MiB rows are device-bound
(≈ 200 IOPS × 4 MiB ≈ 800 MB/s). The same row swings 373–773 MiB/s on ONE side with the session's
position in the run. A–B ranges that do not overlap at n = 3 on that scatter are not a signal.
The 4k rows, which are CPU/transport-bound and reproducible to ±1%, agree within 2%.

**Seen beside the measurement (both sides, not this change).** In three 64k sessions
(A r1, A r2, B r2) the teardown's first `squeezefs umount` failed, so the offline fsck refused (the
writer guard was still held). A second umount was clean. `daemon.log`: mdt1's writer-claim
heartbeat refresh failed `SlotBusy`, and the claim "could not be deleted at unmount". Every other
session's fsck: 0 findings.
