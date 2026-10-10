### seq-write-1m-direct — SCOPING-INVALID

instrument `fio-3.36 libaio write bs=1m direct=1 iodepth=32 numjobs=4 size=512m/job` · substrate `file-backed sparse volumes on /build (ext4 on /dev/vdc, shared disk: diskstats ratio is an upper bound)` · venue `7.2.6-sqz_aarch64_apple-vm_9c_virtio-blk` · build `squeezefs 0.1.0 (ce0edf4f004d, gate profile, zig 0.17.0)` · posture `symmetric` · mount `defaults` · 20261010T170201Z · 4 legs × 60 s (+10 s warm-up excluded) · order A→B→B→A

| leg | arm | — | p50 µs | p99 µs | device÷user (stats) | device÷user (diskstats data / meta) | wareq-sz B (stats / disk) | reclaim q+e=t | admission ×user | wire rpcs | tripwires | tick rescues | valid |
|---|---|---|---|---|---|---|---|---|---|---|---|---|---|
| 1 | A | 0 | — | — | — | — / — | 1048018 / — | 6151+0=6151 | — | 0 | 0 | 7 | NO |
| 2 | B | 0 | — | — | — | — / — | 1047058 / — | 8701+0=8701 | — | 0 | 0 | 45 | NO |
| 3 | B | 0 | — | — | — | — / — | 1049014 / — | 6617+0=6617 | — | 0 | 0 | 44 | NO |
| 4 | A | 0 | — | — | — | — / — | 1049140 / — | 7956+0=7956 | — | 0 | 0 | 43 | NO |

A mean 0.000 (spread 0), B mean 0.000 (spread 0); all legs: min 0 max 0 spread 0.0 %.
A = B (same binary): the spread IS the venue's noise band for this row: **0.0 %**.
- INVALID leg 1/A: `instrument_ok` — instrument rc 4, 100 I/O errors (fio io_u errors (see measure.log)); daemon log: 100 `answered EAGAIN` line(s)
- INVALID leg 2/B: `instrument_ok` — instrument rc 4, 128 I/O errors (fio io_u errors (see measure.log)); daemon log: 128 `answered EAGAIN` line(s)
- INVALID leg 3/B: `instrument_ok` — instrument rc 4, 108 I/O errors (fio io_u errors (see measure.log)); daemon log: 108 `answered EAGAIN` line(s)
- INVALID leg 4/A: `instrument_ok` — instrument rc 4, 128 I/O errors (fio io_u errors (see measure.log)); daemon log: 128 `answered EAGAIN` line(s)
