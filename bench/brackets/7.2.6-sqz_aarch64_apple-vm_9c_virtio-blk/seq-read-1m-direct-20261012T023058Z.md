### seq-read-1m-direct — SCOPING-VALID

instrument `fio-3.36 libaio read bs=1m blockalign=1m direct=1 iodepth=32 numjobs=4 size=512m/job` · substrate `nvmet-loop: 3 kernel nvmet namespaces (resv_enable=1) connected through `nvmeof connect`, backed by sparse files on /build (/dev/vdc)` · venue `7.2.6-sqz_aarch64_apple-vm_9c_virtio-blk` · build `squeezefs 0.1.0 (585152f3a24d, gate profile, zig 0.17.0)` · posture `symmetric` · format `defaults` · mount `defaults` · 20261012T023058Z · 2 legs × 60 s (+10 s warm-up excluded) · order A→B

| leg | arm | MiB/s | p50 µs | p99 µs | device÷user (stats) | device÷user (diskstats data / meta) | meta pad fraction / journal B/op / entries per window | wareq-sz B (stats / disk) | reclaim q+e=t | admission ×user | wire rpcs | tripwires | tick rescues | valid |
|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|
| 1 | A | 18347 | 6521 | 14090 | — | — / — | 0.955 / 0.022 / 1.00 | 0 / — | 0+0=0 | — | 0 | 0 | 0 | yes |
| 2 | B | 16597 | 7242 | 15794 | — | — / — | 0.955 / 0.025 / 1.00 | 0 / — | 0+0=0 | — | 0 | 0 | 0 | yes |

| leg | arm | read device÷user (stats) | read device÷user (diskstats data) | bound | rareq-sz B (stats / disk) | ranged | demand blk | prefetch blk | lane blk | image | window | compose | write-side | direct leg | plane ledger |
|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|
| 1 | A | 1.000 | 1.000 | 1.050 | 0 / 524288 | 0 | 0 | 0 | 0 | 0 | 0 | 0 | 0 | 1154416115712 | closed |
| 2 | B | 1.000 | 1.000 | 1.050 | 0 / 524288 | 0 | 0 | 0 | 0 | 0 | 0 | 0 | 0 | 1044306198528 | closed |

| leg | arm | lookups | getattrs | dirents | node reads | node bytes | node bytes/record | node reads/record | cache hits | cache misses |
|---|---|---|---|---|---|---|---|---|---|---|
| 1 | A | 15 | 9 | 0 | 0 | 0 | 0.000 | 0.000 | 7706601 | 0 |
| 2 | B | 15 | 9 | 0 | 0 | 0 | 0.000 | 0.000 | 6971538 | 0 |

A mean 18347 (spread 0.000), B mean 16597 (spread 0.000) → A/B = 1.105; all legs: min 16597 max 18347 spread 10.0 %.
A = B (same binary): the spread IS the venue's noise band for this row: **10.0 %**.
- leg 1/A `meta_pad_fraction_bound` ok: journal device bytes 24576 ≡ entries 954 + pad 23478 + headers 144 + holes 0 (closes); pad fraction 0.955 over 6 windows (1.00 entries/window, 0 B/op); pad-majority windows under the law 6 (held 0, unshadowed 6, of which the gate declined 0) vs the pacing bound 15482 (= span 60.0 s ÷ window rtt 4264 µs × 1.1 + 0 deadline-ended holds + 1); holds 0 (not worth 0, probes 0; live gain 0.250/hold), mean hold 0 µs, live bound 12709 µs
- leg 1/A `meta_device_div_user` ok: META device bytes: stats 147456 (journal lane 24576 + io pool 122880) vs diskstats 147456 (×1.0000); ÷ user bytes: stats —, diskstats —; journal 0 B/op, pad fraction 0.955
- leg 2/B `meta_pad_fraction_bound` ok: journal device bytes 24576 ≡ entries 954 + pad 23478 + headers 144 + holes 0 (closes); pad fraction 0.955 over 6 windows (1.00 entries/window, 0 B/op); pad-majority windows under the law 6 (held 0, unshadowed 6, of which the gate declined 0) vs the pacing bound 35456 (= span 60.0 s ÷ window rtt 1862 µs × 1.1 + 0 deadline-ended holds + 1); holds 0 (not worth 0, probes 0; live gain 0.000/hold), mean hold 0 µs, live bound 5459 µs
- leg 2/B `meta_device_div_user` ok: META device bytes: stats 147456 (journal lane 24576 + io pool 122880) vs diskstats 147456 (×1.0000); ÷ user bytes: stats —, diskstats —; journal 0 B/op, pad fraction 0.955
