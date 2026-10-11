### seq-write-1m-direct — SCOPING-INVALID

instrument `fio-3.36 libaio write bs=1m direct=1 iodepth=32 numjobs=4 size=1g/job` · substrate `nvmet-tcp on 127.0.0.1: 3 kernel nvmet namespaces (resv_enable=1) connected through `nvmeof connect`, backed by sparse files on /build (/dev/vdc)` · venue `7.2.6-sqz_aarch64_apple-vm_9c_virtio-blk` · build `A squeezefs 0.1.0 (53d05e10eeb6, gate profile, zig 0.17.0) / B squeezefs 0.1.0 (7f76af1a7b03, gate profile, zig 0.17.0)` · posture `symmetric` · mount `defaults` · 20261011T011807Z · 4 legs × 60 s (+10 s warm-up excluded) · order A→B→B→A

| leg | arm | — | p50 µs | p99 µs | device÷user (stats) | device÷user (diskstats data / meta) | wareq-sz B (stats / disk) | reclaim q+e=t | admission ×user | wire rpcs | tripwires | tick rescues | valid |
|---|---|---|---|---|---|---|---|---|---|---|---|---|---|
| 1 | A | 0 | — | — | — | — / — | 1048309 / — | 0+7889=7889 | — | 0 | 0 | 1850 | NO |
| 2 | B | 1247 | 95945 | 333447 | 0.996 | 1.000 / 0.010 | 1047635 / 1052635 | 0+18736=18736 | ×1.0000 | 0 | 0 | 0 | yes |
| 3 | B | 1263 | 96993 | 270533 | 0.996 | 1.000 / 0.010 | 1047537 / 1052304 | 0+18979=18979 | ×1.0000 | 0 | 0 | 0 | yes |
| 4 | A | 0 | — | — | — | — / — | 1048328 / — | 0+8463=8463 | — | 0 | 0 | 1750 | NO |

A mean 0.000 (spread 0), B mean 1255 (spread 16.2); all legs: min 0 max 1263 spread 201.3 %.
- INVALID leg 1/A: `instrument_ok` — instrument rc 3, 96 I/O errors (fio io_u errors (see measure.log)); daemon log: 0 `answered EAGAIN` line(s)
- INVALID leg 4/A: `instrument_ok` — instrument rc 3, 96 I/O errors (fio io_u errors (see measure.log)); daemon log: 0 `answered EAGAIN` line(s)
