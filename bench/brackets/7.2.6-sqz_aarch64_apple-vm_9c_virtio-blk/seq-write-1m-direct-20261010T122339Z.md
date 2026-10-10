### seq-write-1m-direct — SCOPING-INVALID

instrument `fio-3.36 libaio write bs=1m direct=1 iodepth=32 numjobs=4 size=1g/job` · substrate `nvmet-tcp on 127.0.0.1: 3 kernel nvmet namespaces (resv_enable=1) connected through `nvmeof connect`, backed by sparse files on /build (/dev/vdc)` · venue `7.2.6-sqz_aarch64_apple-vm_9c_virtio-blk` · build `squeezefs 0.1.0 (ce0edf4f004d, gate profile, zig 0.17.0)` · posture `symmetric` · mount `defaults` · 20261010T122339Z · 4 legs × 60 s (+10 s warm-up excluded) · order A→B→B→A

| leg | arm | — | p50 µs | p99 µs | device÷user (stats) | device÷user (diskstats data / meta) | wareq-sz B (stats / disk) | reclaim q+e=t | admission ×user | wire rpcs | tripwires | tick rescues | valid |
|---|---|---|---|---|---|---|---|---|---|---|---|---|---|
| 1 | A | 0 | — | — | — | — / — | 1049372 / — | 0+6275=6275 | — | 0 | 0 | 174 | NO |
| 2 | B | 0 | — | — | — | — / — | 1049695 / — | 0+5875=5875 | — | 0 | 0 | 129 | NO |
| 3 | B | 0 | — | — | — | — / — | 1048829 / — | 0+5192=5192 | — | 0 | 0 | 18 | NO |
| 4 | A | 0 | — | — | — | — / — | 1049256 / — | 0+3091=3091 | — | 0 | 0 | 6 | NO |

A mean 0.000 (spread 0), B mean 0.000 (spread 0); all legs: min 0 max 0 spread 0.0 %.
A = B (same binary): the spread IS the venue's noise band for this row: **0.0 %**.
- INVALID leg 1/A: `instrument_ok` — instrument rc 4, 0 I/O errors (?); daemon log: 124 `answered EAGAIN` line(s)
- INVALID leg 2/B: `instrument_ok` — instrument rc 4, 0 I/O errors (?); daemon log: 128 `answered EAGAIN` line(s)
- INVALID leg 3/B: `instrument_ok` — instrument rc 4, 0 I/O errors (?); daemon log: 104 `answered EAGAIN` line(s)
- INVALID leg 4/A: `instrument_ok` — instrument rc 4, 0 I/O errors (?); daemon log: 108 `answered EAGAIN` line(s)
