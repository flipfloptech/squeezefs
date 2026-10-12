### creates-mdtest — VALID

instrument `mdtest (ior io500-sc19-605-g5528b36) -C -F -u -z 2 -b 8 -n <sized from the warm-up rate>, mpirun -np 4` · substrate `nvmet-loop: 3 kernel nvmet namespaces (resv_enable=1) connected through `nvmeof connect`, backed by sparse files on /build (/dev/vdc)` · venue `7.2.6-sqz_aarch64_apple-vm_9c_virtio-blk` · build `A squeezefs 0.1.0 (585152f3a24d, gate profile, zig 0.17.0) / B squeezefs 0.1.0 (585152f3a24d-dirty, gate profile, zig 0.17.0)` · posture `symmetric` · format `defaults` · mount `defaults` · 20261012T031433Z · 4 legs × 60 s (+10 s warm-up excluded) · order A→B→B→A

| leg | arm | creates/s | p50 µs | p99 µs | device÷user (stats) | device÷user (diskstats data / meta) | meta pad fraction / journal B/op / entries per window | wareq-sz B (stats / disk) | reclaim q+e=t | admission ×user | wire rpcs | tripwires | tick rescues | valid |
|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|
| 1 | A | 12659 | — | — | — | — / — | 0.902 / 2458 / 1.68 | 0 / — | 0+0=0 | — | 0 | 0 | 0 | yes |
| 2 | B | 10555 | — | — | — | — / — | 0.908 / 2621 / 1.58 | 0 / — | 0+0=0 | — | 0 | 0 | 0 | yes |
| 3 | B | 10946 | — | — | — | — / — | 0.905 / 2557 / 1.62 | 0 / — | 0+0=0 | — | 0 | 0 | 0 | yes |
| 4 | A | 10302 | — | — | — | — / — | 0.907 / 2610 / 1.58 | 0 / — | 0+0=0 | — | 0 | 0 | 0 | yes |

| leg | arm | lookups | getattrs | dirents | node reads | node bytes | node bytes/record | node reads/record | cache hits | cache misses |
|---|---|---|---|---|---|---|---|---|---|---|
| 1 | A | 861464 | 223 | 0 | 16319 | 4277927936 | 4965 | 0.019 | 11892836 | 0 |
| 2 | B | 779660 | 240 | 0 | 13996 | 3668967424 | 4704 | 0.018 | 11001042 | 0 |
| 3 | B | 773682 | 241 | 0 | 13714 | 3595042816 | 4645 | 0.018 | 10916640 | 38 |
| 4 | A | 780554 | 229 | 0 | 13923 | 3649830912 | 4675 | 0.018 | 11008447 | 8 |

A mean 11480 (spread 2356), B mean 10750 (spread 390.2) → A/B = 1.068; all legs: min 10302 max 12659 spread 21.2 %.
- leg 1/A `meta_pad_fraction_bound` ok: journal device bytes 2113089536 ≡ entries 195036600 + pad 1905671552 + headers 12381384 + holes 0 (closes); pad fraction 0.902 over 515891 windows (1.68 entries/window, 2458 B/op); pad-majority windows under the law 271944 (held 69303, unshadowed 438924, of which the gate declined 236283) vs the pacing bound 812663 (= span 67.9 s ÷ window rtt 92 µs × 1.1 + 745 deadline-ended holds + 1); holds 69303 (not worth 236283, probes 14860; live gain 0.229/hold), mean hold 46 µs, live bound 70 µs
- leg 1/A `meta_device_div_user` ok: META device bytes: stats 4268720128 (journal lane 2113089536 + io pool 2155630592) vs diskstats 4268720128 (×1.0000); ÷ user bytes: stats —, diskstats —; journal 2458 B/op, pad fraction 0.902
- leg 2/B `meta_pad_fraction_bound` ok: journal device bytes 2038513664 ≡ entries 176408572 + pad 1850160676 + headers 11944416 + holes 0 (closes); pad fraction 0.908 over 497684 windows (1.58 entries/window, 2621 B/op); pad-majority windows under the law 253287 (held 71328, unshadowed 419744, of which the gate declined 237785) vs the pacing bound 777124 (= span 73.7 s ÷ window rtt 104 µs × 1.1 + 357 deadline-ended holds + 1); holds 71328 (not worth 237785, probes 14950; live gain 0.218/hold), mean hold 55 µs, live bound 79 µs
- leg 2/B `meta_device_div_user` ok: META device bytes: stats 3884367872 (journal lane 2038513664 + io pool 1845854208) vs diskstats 3884367872 (×1.0000); ÷ user bytes: stats —, diskstats —; journal 2621 B/op, pad fraction 0.908
- leg 3/B `meta_pad_fraction_bound` ok: journal device bytes 1974149120 ≡ entries 175043445 + pad 1787538395 + headers 11567280 + holes 0 (closes); pad fraction 0.905 over 481970 windows (1.62 entries/window, 2557 B/op); pad-majority windows under the law 246801 (held 65304, unshadowed 410218, of which the gate declined 228721) vs the pacing bound 762743 (= span 70.5 s ÷ window rtt 102 µs × 1.1 + 391 deadline-ended holds + 1); holds 65304 (not worth 228721, probes 14388; live gain 0.250/hold), mean hold 53 µs, live bound 78 µs
- leg 3/B `meta_device_div_user` ok: META device bytes: stats 3779956736 (journal lane 1974149120 + io pool 1805807616) vs diskstats 3779956736 (×1.0000); ÷ user bytes: stats —, diskstats —; journal 2557 B/op, pad fraction 0.905
- leg 4/A `meta_pad_fraction_bound` ok: journal device bytes 2032627712 ≡ entries 176575935 + pad 1844141849 + headers 11909928 + holes 0 (closes); pad fraction 0.907 over 496247 windows (1.58 entries/window, 2610 B/op); pad-majority windows under the law 256326 (held 73616, unshadowed 416064, of which the gate declined 233354) vs the pacing bound 785382 (= span 75.6 s ÷ window rtt 106 µs × 1.1 + 355 deadline-ended holds + 1); holds 73616 (not worth 233354, probes 14680; live gain 0.225/hold), mean hold 60 µs, live bound 210 µs
- leg 4/A `meta_device_div_user` ok: META device bytes: stats 3868000256 (journal lane 2032627712 + io pool 1835372544) vs diskstats 3868000256 (×1.0000); ÷ user bytes: stats —, diskstats —; journal 2610 B/op, pad fraction 0.907

notes: 859664 files across 4 tasks × 73 dirs; elapsed 67.9 s (sized from the warm-up rate)
