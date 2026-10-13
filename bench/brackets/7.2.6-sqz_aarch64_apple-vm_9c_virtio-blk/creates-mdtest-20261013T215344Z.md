### creates-mdtest — SCOPING-INVALID

instrument `mdtest (ior io500-sc19-605-g5528b36) -C -F -u -z 2 -b 8 -n <sized from the warm-up rate>, mpirun -np 4` · substrate `file-backed sparse volumes on /build (ext4 on /dev/vdc, shared disk: diskstats ratio is an upper bound)` · venue `7.2.6-sqz_aarch64_apple-vm_9c_virtio-blk` · build `A squeezefs 0.1.0 (e83715389b42, gate profile, zig 0.17.0) / B squeezefs 0.1.0 (b652702df09c, gate profile, zig 0.17.0)` · posture `symmetric` · format `defaults` · mount `defaults` · 20261013T215344Z · 4 legs × 60 s (+10 s warm-up excluded) · order A→B→B→A

| leg | arm | creates/s | p50 µs | p99 µs | device÷user (stats) | device÷user (diskstats data / meta) | meta pad fraction / journal B/op / entries per window | wareq-sz B (stats / disk) | reclaim q+e=t | admission ×user | wire rpcs | tripwires | tick rescues | valid |
|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|
| 1 | A | 14755 | — | — | — | — / — | 0.381 / 370.0 / 1.54 | 0 / — | 0+0=0 | — | 0 | 0 | 0 | NO |
| 2 | B | 12425 | — | — | — | — / — | 0.381 / 369.8 / 1.54 | 0 / — | 0+0=0 | — | 0 | 0 | 0 | yes |
| 3 | B | 11494 | — | — | — | — / — | 0.390 / 375.7 / 1.49 | 0 / — | 0+0=0 | — | 0 | 0 | 0 | yes |
| 4 | A | 12014 | — | — | — | — / — | 0.397 / 379.7 / 1.47 | 0 / — | 0+0=0 | — | 0 | 0 | 0 | yes |

| leg | arm | lookups | getattrs | dirents | node reads | node bytes | node bytes/record | node reads/record | cache hits | cache misses |
|---|---|---|---|---|---|---|---|---|---|---|
| 1 | A | 951293 | 222 | 0 | 16803 | 4404805632 | 4629 | 0.018 | 13140127 | 0 |
| 2 | B | 951989 | 252 | 0 | 15900 | 4168089600 | 4377 | 0.017 | 13406253 | 3 |
| 3 | B | 913514 | 265 | 0 | 15710 | 4118282240 | 4507 | 0.017 | 12862913 | 1 |
| 4 | A | 1055344 | 289 | 0 | 17720 | 4645191680 | 4400 | 0.017 | 14851733 | 7 |

A mean 13384 (spread 2741), B mean 11959 (spread 931.1) → A/B = 1.119; all legs: min 11494 max 14755 spread 25.7 %.
- INVALID leg 1/A: `meta_device_div_user` — META device bytes: stats 2558358016 (journal lane 351351296 + io pool 2207006720) vs diskstats 2725868032 (×0.9385); ÷ user bytes: stats —, diskstats —; journal 369 B/op, pad fraction 0.381
- leg 1/A `meta_pad_fraction_bound` ok: journal device bytes 351351296 ≡ entries 215396540 + pad 133896060 + headers 2058696 + holes 0 (closes); pad fraction 0.381 over 621551 windows (1.54 entries/window, 369 B/op); pad-majority windows under the law 164035 (held 25951, unshadowed 328179, of which the gate declined 190095) vs the pacing bound 918683 (= span 64.4 s ÷ window rtt 77 µs × 1.1 + 256 deadline-ended holds + 1); holds 25951 (not worth 190095, probes 11950; live gain 0.239/hold), mean hold 39 µs, live bound 66 µs
- leg 1/A `meta_device_div_user` FAILED: META device bytes: stats 2558358016 (journal lane 351351296 + io pool 2207006720) vs diskstats 2725868032 (×0.9385); ÷ user bytes: stats —, diskstats —; journal 369 B/op, pad fraction 0.381
- leg 2/B `meta_pad_fraction_bound` ok: journal device bytes 351328256 ≡ entries 215450308 + pad 133819396 + headers 2058552 + holes 0 (closes); pad fraction 0.381 over 621958 windows (1.54 entries/window, 369 B/op); pad-majority windows under the law 166697 (held 25988, unshadowed 329342, of which the gate declined 188633) vs the pacing bound 978808 (= span 76.5 s ÷ window rtt 86 µs × 1.1 + 176 deadline-ended holds + 1); holds 25988 (not worth 188633, probes 11860; live gain 0.250/hold), mean hold 43 µs, live bound 60 µs
- leg 2/B `meta_device_div_user` ok: META device bytes: stats 2448427008 (journal lane 351328256 + io pool 2097098752) vs diskstats 2488211456 (×0.9840); ÷ user bytes: stats —, diskstats —; journal 369 B/op, pad fraction 0.381
- leg 3/B `meta_pad_fraction_bound` ok: journal device bytes 342438400 ≡ entries 206730488 + pad 133701440 + headers 2006472 + holes 0 (closes); pad fraction 0.390 over 615448 windows (1.49 entries/window, 375 B/op); pad-majority windows under the law 174033 (held 32477, unshadowed 339953, of which the gate declined 198397) vs the pacing bound 961104 (= span 79.3 s ÷ window rtt 91 µs × 1.1 + 233 deadline-ended holds + 1); holds 32477 (not worth 198397, probes 12470; live gain 0.225/hold), mean hold 47 µs, live bound 204 µs
- leg 3/B `meta_device_div_user` ok: META device bytes: stats 2421850624 (journal lane 342438400 + io pool 2079412224) vs diskstats 2460639744 (×0.9842); ÷ user bytes: stats —, diskstats —; journal 375 B/op, pad fraction 0.390
- leg 4/A `meta_pad_fraction_bound` ok: journal device bytes 399926784 ≡ entries 238880726 + pad 158702746 + headers 2343312 + holes 0 (closes); pad fraction 0.397 over 721588 windows (1.47 entries/window, 379 B/op); pad-majority windows under the law 204424 (held 41045, unshadowed 410271, of which the gate declined 246892) vs the pacing bound 1086961 (= span 87.7 s ÷ window rtt 89 µs × 1.1 + 281 deadline-ended holds + 1); holds 41045 (not worth 246892, probes 15530; live gain 0.375/hold), mean hold 47 µs, live bound 194 µs
- leg 4/A `meta_device_div_user` ok: META device bytes: stats 2744505856 (journal lane 399926784 + io pool 2344579072) vs diskstats 2788185600 (×0.9843); ÷ user bytes: stats —, diskstats —; journal 379 B/op, pad fraction 0.397

notes: 949624 files across 4 tasks × 73 dirs; elapsed 64.4 s (sized from the warm-up rate)
