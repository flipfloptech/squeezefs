### seq-write-1m-buffered — SCOPING-INVALID

instrument `fio-3.36 libaio write bs=1m direct=0 iodepth=32 numjobs=4 size=512m/job` · substrate `nvmet-tcp on 127.0.0.1: 3 kernel nvmet namespaces (resv_enable=1) connected through `nvmeof connect`, backed by sparse files on /build (/dev/vdc)` · venue `7.2.6-sqz_aarch64_apple-vm_9c_virtio-blk` · build `squeezefs 0.1.0 (ce0edf4f004d, gate profile, zig 0.17.0)` · posture `symmetric` · mount `defaults` · 20261010T143514Z · 4 legs × 60 s (+10 s warm-up excluded) · order A→B→B→A

| leg | arm | — | p50 µs | p99 µs | device÷user (stats) | device÷user (diskstats data / meta) | wareq-sz B (stats / disk) | reclaim q+e=t | admission ×user | wire rpcs | tripwires | tick rescues | valid |
|---|---|---|---|---|---|---|---|---|---|---|---|---|---|
| 1 | A | 0 | — | — | — | — / — | 1049224 / — | 0+8149=8149 | — | 0 | 0 | 9 | NO |
| 2 | B | 0 | — | — | — | — / — | 1047842 / — | 0+5040=5040 | — | 0 | 0 | 0 | NO |
| 3 | B | 406.4 | 287310 | 893387 | 0.997 | 1.006 / 0.006 | 1049784 / 1060241 | 0+6135=6135 | ×1.0000 | 0 | 0 | 18 | yes |
| 4 | A | 0 | — | — | — | — / — | 0 / — | 0+0=0 | — | 0 | 0 | 0 | NO |

A mean 0.000 (spread 0), B mean 203.2 (spread 406.4); all legs: min 0 max 406.4 spread 300.0 %.
A = B (same binary): the spread IS the venue's noise band for this row: **300.0 %**.
- INVALID leg 1/A: `instrument_ok` — instrument rc 3, 96 I/O errors (fio io_u errors (see measure.log)); daemon log: 96 `answered EAGAIN` line(s)
- INVALID leg 2/B: `instrument_ok` — instrument rc 4, 128 I/O errors (fio io_u errors (see measure.log)); daemon log: 128 `answered EAGAIN` line(s)
- INVALID leg 4/A: `daemon_alive` — daemon pid 454025 died during the measured span (instrument rc 4) (see the leg's daemon.log and dmesg.tail; the venue's other agents run `pkill -9 -x squeezefs` sweeps)
