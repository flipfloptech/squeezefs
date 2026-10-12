### seq-write-1m-direct — SCOPING-VALID

instrument `fio-3.36 libaio write bs=1m blockalign=1m direct=1 iodepth=32 numjobs=4 size=512m/job` · substrate `nvmet-loop: 3 kernel nvmet namespaces (resv_enable=1) connected through `nvmeof connect`, backed by sparse files on /build (/dev/vdc)` · venue `7.2.6-sqz_aarch64_apple-vm_9c_virtio-blk` · build `squeezefs 0.1.0 (585152f3a24d, gate profile, zig 0.17.0)` · posture `symmetric` · format `defaults` · mount `defaults` · 20261012T023335Z · 2 legs × 60 s (+10 s warm-up excluded) · order A→B

| leg | arm | MiB/s | p50 µs | p99 µs | device÷user (stats) | device÷user (diskstats data / meta) | meta pad fraction / journal B/op / entries per window | wareq-sz B (stats / disk) | reclaim q+e=t | admission ×user | wire rpcs | tripwires | tick rescues | valid |
|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|
| 1 | A | 1737 | 68682 | 333447 | 1.000 | 1.000 / 0.005 | 0.772 / 3904 / 1.37 | 1069874 / 524288 | 0+26060=26060 | ×1.0000 | 0 | 0 | 0 | yes |
| 2 | B | 1235 | 77070 | 583008 | 1.000 | 1.000 / 0.005 | 0.764 / 3785 / 1.43 | 1067801 / 524288 | 0+18663=18663 | ×1.0000 | 0 | 0 | 0 | yes |

| leg | arm | lookups | getattrs | dirents | node reads | node bytes | node bytes/record | node reads/record | cache hits | cache misses |
|---|---|---|---|---|---|---|---|---|---|---|
| 1 | A | 15 | 9 | 0 | 922 | 241696768 | 10070699 | 38.4 | 789610 | 0 |
| 2 | B | 15 | 9 | 0 | 668 | 175112192 | 7296341 | 27.8 | 566083 | 0 |

A mean 1737 (spread 0.000), B mean 1235 (spread 0.000) → A/B = 1.406; all legs: min 1235 max 1737 spread 33.7 %.
A = B (same binary): the spread IS the venue's noise band for this row: **33.7 %**.
- leg 1/A `write_amplification_bound` ok: device bytes 109295173632 ÷ user 109295173632 = ×1.000 vs the §J.1 bound ×1.000 (+0.1 slack) for bs 1048576; in-place folds 0 B over 0 DMAs
- leg 1/A `meta_pad_fraction_bound` ok: journal device bytes 406949888 ≡ entries 90585955 + pad 313979461 + headers 2384472 + holes 0 (closes); pad fraction 0.772 over 85719 windows (1.37 entries/window, 3904 B/op); pad-majority windows under the law 45141 (held 11712, unshadowed 62661, of which the gate declined 29232) vs the pacing bound 63749 (= span 60.0 s ÷ window rtt 1040 µs × 1.1 + 257 deadline-ended holds + 1); holds 11712 (not worth 29232, probes 1840; live gain 0.241/hold), mean hold 292 µs, live bound 290 µs
- leg 1/A `meta_device_div_user` ok: META device bytes: stats 527695872 (journal lane 406949888 + io pool 120745984) vs diskstats 527695872 (×1.0000); ÷ user bytes: stats 0.005, diskstats 0.005; journal 3904 B/op, pad fraction 0.772
- leg 2/B `write_amplification_bound` ok: device bytes 78272004096 ÷ user 78272004096 = ×1.000 vs the §J.1 bound ×1.000 (+0.1 slack) for bs 1048576; in-place folds 0 B over 0 DMAs
- leg 2/B `meta_pad_fraction_bound` ok: journal device bytes 282546176 ≡ entries 65096300 + pad 215794332 + headers 1655544 + holes 0 (closes); pad fraction 0.764 over 59001 windows (1.43 entries/window, 3785 B/op); pad-majority windows under the law 32226 (held 10147, unshadowed 41470, of which the gate declined 19391) vs the pacing bound 41524 (= span 60.4 s ÷ window rtt 1611 µs × 1.1 + 253 deadline-ended holds + 1); holds 10147 (not worth 19391, probes 1210; live gain 0.239/hold), mean hold 462 µs, live bound 325 µs
- leg 2/B `meta_device_div_user` ok: META device bytes: stats 370421760 (journal lane 282546176 + io pool 87875584) vs diskstats 370421760 (×1.0000); ÷ user bytes: stats 0.005, diskstats 0.005; journal 3785 B/op, pad fraction 0.764
