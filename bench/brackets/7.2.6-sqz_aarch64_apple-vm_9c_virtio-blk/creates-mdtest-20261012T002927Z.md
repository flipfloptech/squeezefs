### creates-mdtest — VALID

instrument `mdtest (ior io500-sc19-605-g5528b36) -C -F -u -z 2 -b 8 -n <sized from the warm-up rate>, mpirun -np 4` · substrate `nvmet-loop: 3 kernel nvmet namespaces (resv_enable=1) connected through `nvmeof connect`, backed by sparse files on /build (/dev/vdc)` · venue `7.2.6-sqz_aarch64_apple-vm_9c_virtio-blk` · build `A squeezefs 0.1.0 (f27c5018e635, gate profile, zig 0.17.0) / B squeezefs 0.1.0 (855589c5c947, gate profile, zig 0.17.0)` · posture `symmetric` · mount `defaults` · 20261012T002927Z · 4 legs × 60 s (+10 s warm-up excluded) · order A→B→B→A

| leg | arm | creates/s | p50 µs | p99 µs | device÷user (stats) | device÷user (diskstats data / meta) | meta pad fraction / journal B/op / entries per window | wareq-sz B (stats / disk) | reclaim q+e=t | admission ×user | wire rpcs | tripwires | tick rescues | valid |
|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|
| 1 | A | 12283 | — | — | — | — / — | — / 0.000 / — | 0 / — | 0+0=0 | — | 0 | 0 | 0 | yes |
| 2 | B | 12754 | — | — | — | — / — | 0.899 / 2381 / 1.73 | 0 / — | 0+0=0 | — | 0 | 0 | 0 | yes |
| 3 | B | 12235 | — | — | — | — / — | 0.901 / 2439 / 1.69 | 0 / — | 0+0=0 | — | 0 | 0 | 0 | yes |
| 4 | A | 9389 | — | — | — | — / — | — / 0.000 / — | 0 / — | 0+0=0 | — | 0 | 0 | 0 | yes |

A mean 10836 (spread 2895), B mean 12494 (spread 519.6) → A/B = 0.867; all legs: min 9389 max 12754 spread 28.8 %.
- leg 2/B `meta_pad_fraction_bound` ok: journal device bytes 2256928768 ≡ entries 215010142 + pad 2028694434 + headers 13224192 + holes 0 (closes); pad fraction 0.899 over 551008 windows (1.73 entries/window, 2381 B/op); pad-majority windows under the law 293033 (held 67548, unshadowed 475431, of which the gate declined 249946) vs the pacing bound 896859 (= span 74.3 s ÷ window rtt 91 µs × 1.1 + 724 deadline-ended holds + 1); holds 67548 (not worth 249946, probes 15720; live gain 0.375/hold), mean hold 44 µs, live bound 76 µs
- leg 2/B `meta_device_div_user` ok: META device bytes: stats 4504150016 (journal lane 2256928768 + io pool 2247221248) vs diskstats 4504150016 (×1.0000); ÷ user bytes: stats —, diskstats —; journal 2381 B/op, pad fraction 0.899
- leg 3/B `meta_pad_fraction_bound` ok: journal device bytes 2131488768 ≡ entries 198253996 + pad 1920745580 + headers 12489192 + holes 0 (closes); pad fraction 0.901 over 520383 windows (1.69 entries/window, 2438 B/op); pad-majority windows under the law 276849 (held 71674, unshadowed 441514, of which the gate declined 236339) vs the pacing bound 833547 (= span 71.4 s ÷ window rtt 94 µs × 1.1 + 830 deadline-ended holds + 1); holds 71674 (not worth 236339, probes 14870; live gain 0.125/hold), mean hold 45 µs, live bound 351 µs
- leg 3/B `meta_device_div_user` ok: META device bytes: stats 4149829632 (journal lane 2131488768 + io pool 2018340864) vs diskstats 4149829632 (×1.0000); ÷ user bytes: stats —, diskstats —; journal 2438 B/op, pad fraction 0.901

notes: 884816 files across 4 tasks × 73 dirs; elapsed 72.0 s (sized from the warm-up rate)
