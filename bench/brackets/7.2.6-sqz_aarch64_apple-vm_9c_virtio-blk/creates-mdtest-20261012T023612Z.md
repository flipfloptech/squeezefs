### creates-mdtest — SCOPING-VALID

instrument `mdtest (ior io500-sc19-605-g5528b36) -C -F -u -z 2 -b 8 -n <sized from the warm-up rate>, mpirun -np 4` · substrate `nvmet-loop: 3 kernel nvmet namespaces (resv_enable=1) connected through `nvmeof connect`, backed by sparse files on /build (/dev/vdc)` · venue `7.2.6-sqz_aarch64_apple-vm_9c_virtio-blk` · build `squeezefs 0.1.0 (585152f3a24d, gate profile, zig 0.17.0)` · posture `symmetric` · format `defaults` · mount `defaults` · 20261012T023612Z · 2 legs × 60 s (+10 s warm-up excluded) · order A→B

| leg | arm | creates/s | p50 µs | p99 µs | device÷user (stats) | device÷user (diskstats data / meta) | meta pad fraction / journal B/op / entries per window | wareq-sz B (stats / disk) | reclaim q+e=t | admission ×user | wire rpcs | tripwires | tick rescues | valid |
|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|
| 1 | A | 7556 | — | — | — | — / — | 0.904 / 2523 / 1.64 | 0 / — | 0+0=0 | — | 0 | 0 | 0 | yes |
| 2 | B | 9722 | — | — | — | — / — | 0.909 / 2669 / 1.55 | 0 / — | 0+0=0 | — | 0 | 0 | 0 | yes |

| leg | arm | lookups | getattrs | dirents | node reads | node bytes | node bytes/record | node reads/record | cache hits | cache misses |
|---|---|---|---|---|---|---|---|---|---|---|
| 1 | A | 535159 | 219 | 0 | 9960 | 2610954240 | 4877 | 0.019 | 7159725 | 0 |
| 2 | B | 705046 | 207 | 0 | 12844 | 3366977536 | 4774 | 0.018 | 9941661 | 0 |

A mean 7556 (spread 0.000), B mean 9722 (spread 0.000) → A/B = 0.777; all legs: min 7556 max 9722 spread 25.1 %.
A = B (same binary): the spread IS the venue's noise band for this row: **25.1 %**.
- leg 1/A `meta_pad_fraction_bound` ok: journal device bytes 1345327104 ≡ entries 120808152 + pad 1216636176 + headers 7882776 + holes 0 (closes); pad fraction 0.904 over 328449 windows (1.64 entries/window, 2522 B/op); pad-majority windows under the law 184935 (held 57631, unshadowed 266153, of which the gate declined 138849) vs the pacing bound 529447 (= span 70.6 s ÷ window rtt 147 µs × 1.1 + 442 deadline-ended holds + 1); holds 57631 (not worth 138849, probes 8730; live gain 0.250/hold), mean hold 93 µs, live bound 293 µs
- leg 1/A `meta_device_div_user` ok: META device bytes: stats 2655698944 (journal lane 1345327104 + io pool 1310371840) vs diskstats 2655698944 (×1.0000); ÷ user bytes: stats —, diskstats —; journal 2522 B/op, pad fraction 0.904
- leg 2/B `meta_pad_fraction_bound` ok: journal device bytes 1877282816 ≡ entries 159472570 + pad 1706810542 + headers 10999704 + holes 0 (closes); pad fraction 0.909 over 458321 windows (1.55 entries/window, 2669 B/op); pad-majority windows under the law 239776 (held 74021, unshadowed 378240, of which the gate declined 212485) vs the pacing bound 702770 (= span 72.3 s ÷ window rtt 113 µs × 1.1 + 461 deadline-ended holds + 1); holds 74021 (not worth 212485, probes 13370; live gain 0.250/hold), mean hold 66 µs, live bound 145 µs
- leg 2/B `meta_device_div_user` ok: META device bytes: stats 3573653504 (journal lane 1877282816 + io pool 1696370688) vs diskstats 3573653504 (×1.0000); ÷ user bytes: stats —, diskstats —; journal 2669 B/op, pad fraction 0.909

notes: 533304 files across 4 tasks × 73 dirs; elapsed 70.6 s (sized from the warm-up rate)
