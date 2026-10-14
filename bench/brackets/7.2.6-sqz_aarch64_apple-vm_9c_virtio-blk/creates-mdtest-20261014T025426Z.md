### creates-mdtest — SCOPING-VALID

instrument `mdtest (ior io500-sc19-605-g5528b36) -C -F -u -z 2 -b 8 -n <sized from the warm-up rate>, mpirun -np 4` · substrate `file-backed sparse volumes on /build (ext4 on /dev/vdc, shared disk: the data diskstats ratio is an upper bound); metadata on a dedicated loop device per set (/dev/loop0, exclusive diskstats)` · venue `7.2.6-sqz_aarch64_apple-vm_9c_virtio-blk` · build `A squeezefs 0.1.0 (d65a300db7cf-dirty, dist profile, zig 0.17.0) / B squeezefs 0.1.0 (f246494d8c39-dirty, dist profile, zig 0.17.0)` · posture `symmetric` · format `defaults` · mount `defaults` · 20261014T025426Z · 4 legs × 20 s (+5 s warm-up excluded) · order A→B→B→A

| leg | arm | creates/s | p50 µs | p99 µs | device÷user (stats) | device÷user (diskstats data / meta) | meta pad fraction / journal B/op / entries per window | wareq-sz B (stats / disk) | reclaim q+e=t | admission ×user | wire rpcs | tripwires | tick rescues | valid |
|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|
| 1 | A | 14877 | — | — | — | — / — | 0.409 / 386.6 / 1.42 | 0 / — | 0+0=0 | — | 0 | 0 | 0 | yes |
| 2 | B | 14115 | — | — | — | — / — | 0.414 / 389.9 / 1.40 | 0 / — | 0+0=0 | — | 0 | 0 | 0 | yes |
| 3 | B | 14133 | — | — | — | — / — | 0.394 / 377.2 / 1.47 | 0 / — | 0+0=0 | — | 0 | 0 | 0 | yes |
| 4 | A | 15481 | — | — | — | — / — | 0.395 / 377.2 / 1.47 | 0 / — | 0+0=0 | — | 0 | 0 | 0 | yes |

| leg | arm | lookups | getattrs | dirents | node reads | node bytes | node bytes/record | node reads/record | cache hits | cache misses |
|---|---|---|---|---|---|---|---|---|---|---|
| 1 | A | 323546 | 12 | 0 | 5659 | 1483472896 | 4585 | 0.017 | 4102777 | 0 |
| 2 | B | 294921 | 53 | 0 | 4398 | 1152909312 | 3909 | 0.015 | 4150644 | 0 |
| 3 | B | 397543 | 65 | 0 | 6335 | 1660682240 | 4177 | 0.016 | 5596052 | 0 |
| 4 | A | 326468 | 15 | 0 | 5227 | 1370226688 | 4197 | 0.016 | 4601786 | 0 |

A mean 15179 (spread 603.9), B mean 14124 (spread 17.9) → A/B = 1.075; all legs: min 14115 max 15481 spread 9.3 %.
- leg 1/A `meta_pad_fraction_bound` ok: journal device bytes 124956672 ≡ entries 73135726 + pad 51088778 + headers 732168 + holes 0 (closes); pad fraction 0.409 over 229277 windows (1.42 entries/window, 386 B/op); pad-majority windows under the law 97419 (held 48880, unshadowed 118674, of which the gate declined 70135) vs the pacing bound 294541 (= span 21.7 s ÷ window rtt 81 µs × 1.1 + 329 deadline-ended holds + 1); holds 48880 (not worth 70135, probes 4410; live gain 0.225/hold), mean hold 52 µs, live bound 81 µs
- leg 1/A `meta_device_div_user` ok: META device bytes: stats 862576640 (journal lane 124956672 + io pool 737619968) vs diskstats 862576640 (×1.0000); ÷ user bytes: stats —, diskstats —; journal 386 B/op, pad fraction 0.409
- leg 2/B `meta_pad_fraction_bound` ok: journal device bytes 114672128 ≡ entries 66519819 + pad 47480381 + headers 671928 + holes 0 (closes); pad fraction 0.414 over 211096 windows (1.40 entries/window, 389 B/op); pad-majority windows under the law 84524 (held 38752, unshadowed 115443, of which the gate declined 69671) vs the pacing bound 266917 (= span 20.8 s ÷ window rtt 86 µs × 1.1 + 215 deadline-ended holds + 1); holds 38752 (not worth 69671, probes 4380; live gain 0.250/hold), mean hold 53 µs, live bound 114 µs
- leg 2/B `meta_device_div_user` ok: META device bytes: stats 706507264 (journal lane 114672128 + io pool 591835136) vs diskstats 706507264 (×1.0000); ÷ user bytes: stats —, diskstats —; journal 389 B/op, pad fraction 0.414
- leg 3/B `meta_pad_fraction_bound` ok: journal device bytes 149599744 ≡ entries 89710043 + pad 59013149 + headers 876552 + holes 0 (closes); pad fraction 0.394 over 271770 windows (1.47 entries/window, 377 B/op); pad-majority windows under the law 121258 (held 64487, unshadowed 127937, of which the gate declined 71166) vs the pacing bound 343237 (= span 28.1 s ÷ window rtt 90 µs × 1.1 + 494 deadline-ended holds + 1); holds 64487 (not worth 71166, probes 4470; live gain 0.218/hold), mean hold 57 µs, live bound 163 µs
- leg 3/B `meta_device_div_user` ok: META device bytes: stats 992200192 (journal lane 149599744 + io pool 842600448) vs diskstats 992200192 (×1.0000); ÷ user bytes: stats —, diskstats —; journal 377 B/op, pad fraction 0.394
- leg 4/A `meta_pad_fraction_bound` ok: journal device bytes 123072512 ≡ entries 73793940 + pad 48557444 + headers 721128 + holes 0 (closes); pad fraction 0.395 over 223012 windows (1.47 entries/window, 377 B/op); pad-majority windows under the law 83261 (held 36207, unshadowed 112738, of which the gate declined 65684) vs the pacing bound 291827 (= span 21.1 s ÷ window rtt 80 µs × 1.1 + 232 deadline-ended holds + 1); holds 36207 (not worth 65684, probes 4130; live gain 0.221/hold), mean hold 50 µs, live bound 73 µs
- leg 4/A `meta_device_div_user` ok: META device bytes: stats 814727168 (journal lane 123072512 + io pool 691654656) vs diskstats 814727168 (×1.0000); ÷ user bytes: stats —, diskstats —; journal 377 B/op, pad fraction 0.395

notes: 323260 files across 4 tasks × 73 dirs; elapsed 21.7 s (sized from the warm-up rate)
