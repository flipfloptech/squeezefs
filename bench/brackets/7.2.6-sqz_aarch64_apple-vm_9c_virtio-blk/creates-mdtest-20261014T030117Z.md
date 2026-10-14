### creates-mdtest — SCOPING-VALID

instrument `mdtest (ior io500-sc19-605-g5528b36) -C -F -u -z 2 -b 8 -n <sized from the warm-up rate>, mpirun -np 4` · substrate `nvmet-loop: 3 kernel nvmet namespaces (resv_enable=1), the metadata connected through `nvmeof connect`, the data volumes by the DAEMON from their fabric_endpoint records; backed by sparse files on /build (/dev/vdc)` · venue `7.2.6-sqz_aarch64_apple-vm_9c_virtio-blk` · build `A squeezefs 0.1.0 (d65a300db7cf-dirty, dist profile, zig 0.17.0) / B squeezefs 0.1.0 (f246494d8c39-dirty, dist profile, zig 0.17.0)` · posture `symmetric` · format `defaults` · mount `defaults` · 20261014T030117Z · 4 legs × 20 s (+5 s warm-up excluded) · order A→B→B→A

| leg | arm | creates/s | p50 µs | p99 µs | device÷user (stats) | device÷user (diskstats data / meta) | meta pad fraction / journal B/op / entries per window | wareq-sz B (stats / disk) | reclaim q+e=t | admission ×user | wire rpcs | tripwires | tick rescues | valid |
|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|
| 1 | A | 16110 | — | — | — | — / — | 0.912 / 2752 / 1.50 | 0 / — | 0+0=0 | — | 0 | 0 | 0 | yes |
| 2 | B | 14694 | — | — | — | — / — | 0.916 / 2879 / 1.43 | 0 / — | 0+0=0 | — | 0 | 0 | 0 | yes |
| 3 | B | 16037 | — | — | — | — / — | 0.911 / 2731 / 1.51 | 0 / — | 0+0=0 | — | 0 | 0 | 0 | yes |
| 4 | A | 15659 | — | — | — | — / — | 0.912 / 2764 / 1.49 | 0 / — | 0+0=0 | — | 0 | 0 | 0 | yes |

| leg | arm | lookups | getattrs | dirents | node reads | node bytes | node bytes/record | node reads/record | cache hits | cache misses |
|---|---|---|---|---|---|---|---|---|---|---|
| 1 | A | 376106 | 14 | 0 | 6965 | 1825832960 | 4854 | 0.019 | 4865185 | 0 |
| 2 | B | 304572 | 63 | 0 | 4893 | 1282670592 | 4211 | 0.016 | 4287955 | 0 |
| 3 | B | 383473 | 69 | 0 | 6108 | 1601175552 | 4175 | 0.016 | 5399423 | 0 |
| 4 | A | 355082 | 13 | 0 | 5732 | 1502609408 | 4232 | 0.016 | 5005208 | 0 |

A mean 15884 (spread 450.9), B mean 15366 (spread 1343) → A/B = 1.034; all legs: min 14694 max 16110 spread 9.1 %.
- leg 1/A `meta_pad_fraction_bound` ok: journal device bytes 1034752000 ≡ entries 85051786 + pad 943637214 + headers 6063000 + holes 0 (closes); pad fraction 0.912 over 252625 windows (1.50 entries/window, 2752 B/op); pad-majority windows under the law 141815 (held 54122, unshadowed 195246, of which the gate declined 107553) vs the pacing bound 324083 (= span 23.3 s ÷ window rtt 79 µs × 1.1 + 594 deadline-ended holds + 1); holds 54122 (not worth 107553, probes 6760; live gain 0.231/hold), mean hold 46 µs, live bound 236 µs
- leg 1/A `meta_device_div_user` ok: META device bytes: stats 1948745728 (journal lane 1034752000 + io pool 913993728) vs diskstats 1948745728 (×1.0000); ÷ user bytes: stats —, diskstats —; journal 2752 B/op, pad fraction 0.912
- leg 2/B `meta_pad_fraction_bound` ok: journal device bytes 874385408 ≡ entries 68720980 + pad 800541076 + headers 5123352 + holes 0 (closes); pad fraction 0.916 over 213473 windows (1.43 entries/window, 2878 B/op); pad-majority windows under the law 114066 (held 43273, unshadowed 167873, of which the gate declined 97080) vs the pacing bound 269728 (= span 20.7 s ÷ window rtt 84 µs × 1.1 + 371 deadline-ended holds + 1); holds 43273 (not worth 97080, probes 6110; live gain 0.125/hold), mean hold 52 µs, live bound 78 µs
- leg 2/B `meta_device_div_user` ok: META device bytes: stats 1529610240 (journal lane 874385408 + io pool 655224832) vs diskstats 1529610240 (×1.0000); ÷ user bytes: stats —, diskstats —; journal 2878 B/op, pad fraction 0.916
- leg 3/B `meta_pad_fraction_bound` ok: journal device bytes 1045364736 ≡ entries 86537382 + pad 952702170 + headers 6125184 + holes 0 (closes); pad fraction 0.911 over 255216 windows (1.51 entries/window, 2731 B/op); pad-majority windows under the law 135710 (held 45934, unshadowed 206402, of which the gate declined 116626) vs the pacing bound 336761 (= span 23.9 s ÷ window rtt 78 µs × 1.1 + 508 deadline-ended holds + 1); holds 45934 (not worth 116626, probes 7340; live gain 0.225/hold), mean hold 47 µs, live bound 67 µs
- leg 3/B `meta_device_div_user` ok: META device bytes: stats 1854652416 (journal lane 1045364736 + io pool 809287680) vs diskstats 1854652416 (×1.0000); ÷ user bytes: stats —, diskstats —; journal 2731 B/op, pad fraction 0.911
- leg 4/A `meta_pad_fraction_bound` ok: journal device bytes 981069824 ≡ entries 80270114 + pad 895051254 + headers 5748456 + holes 0 (closes); pad fraction 0.912 over 239519 windows (1.49 entries/window, 2764 B/op); pad-majority windows under the law 132018 (held 48076, unshadowed 188734, of which the gate declined 104792) vs the pacing bound 305570 (= span 22.7 s ÷ window rtt 82 µs × 1.1 + 436 deadline-ended holds + 1); holds 48076 (not worth 104792, probes 6590; live gain 0.375/hold), mean hold 46 µs, live bound 91 µs
- leg 4/A `meta_device_div_user` ok: META device bytes: stats 1743585280 (journal lane 981069824 + io pool 762515456) vs diskstats 1743585280 (×1.0000); ÷ user bytes: stats —, diskstats —; journal 2764 B/op, pad fraction 0.912

notes: 375936 files across 4 tasks × 73 dirs; elapsed 23.3 s (sized from the warm-up rate)
