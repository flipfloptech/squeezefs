### seq-write-1m-buffered — SCOPING-INVALID

instrument `fio-3.36 libaio write bs=1m direct=0 iodepth=32 numjobs=4 size=1g/job` · substrate `nvmet-tcp on 127.0.0.1: 3 kernel nvmet namespaces (resv_enable=1) connected through `nvmeof connect`, backed by sparse files on /build (/dev/vdc)` · venue `7.2.6-sqz_aarch64_apple-vm_9c_virtio-blk` · build `A squeezefs 0.1.0 (53d05e10eeb6, gate profile, zig 0.17.0) / B squeezefs 0.1.0 (7f76af1a7b03, gate profile, zig 0.17.0)` · posture `symmetric` · mount `defaults` · 20261011T012426Z · 4 legs × 60 s (+10 s warm-up excluded) · order A→B→B→A

| leg | arm | MiB/s | p50 µs | p99 µs | device÷user (stats) | device÷user (diskstats data / meta) | wareq-sz B (stats / disk) | reclaim q+e=t | admission ×user | wire rpcs | tripwires | tick rescues | valid |
|---|---|---|---|---|---|---|---|---|---|---|---|---|---|
| 1 | A | 640.0 | 84410 | 2038432 | 0.972 | 0.974 / 0.009 | 1047994 / 1050708 | 0+9485=9485 | ×1.0000 | 0 | 0 | 0 | NO |
| 2 | B | 1002 | 101188 | 497025 | 0.996 | 1.000 / 0.010 | 1047543 / 1052146 | 0+15028=15028 | ×1.0000 | 0 | 0 | 0 | yes |
| 3 | B | 789.8 | 119013 | 692060 | 0.996 | 1.000 / 0.010 | 1047511 / 1052261 | 0+11850=11850 | ×1.0000 | 0 | 0 | 0 | yes |
| 4 | A | 522.4 | 45875 | 2071986 | 0.966 | 0.967 / 0.009 | 1048230 / 1049925 | 0+7588=7588 | ×1.0000 | 0 | 0 | 0 | NO |

A mean 581.2 (spread 117.5), B mean 895.8 (spread 211.9) → A/B = 0.649; all legs: min 522.4 max 1002 spread 64.9 %.
- INVALID leg 1/A: `instrument_ok` — instrument rc 3, 60 I/O errors (fio job error codes [28]); daemon log: 0 `answered EAGAIN` line(s)
- INVALID leg 4/A: `instrument_ok` — instrument rc 3, 66 I/O errors (fio job error codes [28]); daemon log: 0 `answered EAGAIN` line(s)
