# v0.1.0 — SqueezeFS against ext4 and xfs on the same RAM substrate (2026-10-26)

The tagged release (`v0.1.0` = `7f8cef6`, dist build), mainline Linux `7.3.0-1.7-mainline-sqz`, the
workstation (32 threads, 109 GiB), the venue held by `scripts/perf/quiet.sh`, AC power, `tctl_c`
43–79 °C, cooled ≤ 70 °C between targets. Same substrate and shape as
`rung-10-localfs-comparison.md` (10-19, kernel 9.30, dist `7a0fe2d`): RAM-backed NVMe namespaces
(brd through nvme-loop); ext4 and xfs on one namespace, default mkfs; SqueezeFS the perf mount
(2 MDT + 2 OST, supervised, write cache on). fio rows from `scripts/perf/matrix.sh`, 20 s each;
`mpirun -np 32 mdtest -n 10000 -u -i 1`. Rows in `/build/perf/mx/results.tsv` (`*-rv010*`);
mdtest outputs `/build/perf/localfs-v0.1.0/`.

## Data path — the FUSE mount (io_uring engine)

| row | ext4 | xfs | SqueezeFS | ÷ ext4 | SqueezeFS on 10-19 |
|---|---|---|---|---|---|
| 4k randread QD1 (IOPS) | 265,675 | 254,466 | 124,399 | 47 % | — |
| 4k randread QD32×4 (IOPS) | 2,204,063 | 2,186,451 | 1,312,391 | 60 % | 1,186,887 |
| 4k randwrite QD1 (IOPS) | 240,115 | 246,523 | 114,646 | 48 % | — |
| 4k randwrite QD32×4 (IOPS) | 2,027,404 | 1,980,111 | 1,142,277 | 56 % | 415,417 |
| 64k randread QD16×4 (IOPS) | 1,011,937 | 970,116 | 911,966 | 90 % | — |
| seq read 1 MiB QD16×4 (MiB/s) | 78,975 | 80,278 | 74,092 | 94 % | 72,693 |
| seq write 1 MiB QD16×4 (MiB/s) | 65,988 | 66,728 | 51,058 | 77 % | 49,709 |

## Data path — the interposer's direct arm (psync engine, `-o interception=direct`)

| row | ext4 (psync) | SqueezeFS direct arm | ÷ ext4 |
|---|---|---|---|
| 4k randread QD32×4 (IOPS) | 3,361,167 | 3,369,305 | 100.2 % |
| 4k randwrite QD32×4 (IOPS) | 3,093,388 | 3,063,762 | 99.0 % |
| seq read 1 MiB (MiB/s) | 96,351 | 96,734 | 100.4 % |
| seq write 1 MiB (MiB/s) | 80,799 | 80,897 | 100.1 % |

## Metadata — `mdtest -np 32` (through FUSE; ops/s)

| phase | ext4 | xfs | SqueezeFS | ÷ ext4 | ÷ xfs | SqueezeFS on 10-19 |
|---|---|---|---|---|---|---|
| file creation | 646,485 | 450,375 | 84,116 | 13 % | 19 % | 41,181 |
| file stat | 14.44 M | 12.28 M | 9.58 M | 66 % | 78 % | 10.93 M |
| file read (0 B) | 13.74 M | 6.93 M | 0.83 M | 6 % | 12 % | — |
| file removal | 588,632 | 169,010 | 88,887 | 15 % | 53 % | 36,653 |
| directory creation | 219,546 | 356,959 | 84,243 | 38 % | 24 % | 51,806 |
| directory stat | 13.34 M | 12.38 M | 10.37 M | 78 % | 84 % | — |
| directory removal | 439,223 | 127,180 | 57,048 | 13 % | 45 % | 40,377 |

## Reading

- **The interposer's direct arm meets the owner's bar on every data row it covers** — 99–100 % of
  ext4 on the same engine.
- **The FUSE path**: bandwidth rows near parity (64k reads 90 %, seq read 94 %, seq write 77 %);
  4k random I/O at 47–60 % — the per-request round trip on mainline 7.3 (`rung-10-fuse-mainline-uapi.md`
  §10: 9.33's kmbuf series measured 1.88 M on the same row; the next lever is re-expressing 0031 for
  mainline's `dev_uring.c`, owner's call on kernel work). 4k randwrite is 2.75× the 10-19 number.
- **Metadata through FUSE is the remaining gap**: creates and removals at 13–15 % of ext4 (19 % /
  53 % of xfs), 2.0–2.4× the 10-19 numbers; stat at 66–84 %. A zero-byte `open+read+close` at 6 %
  is the FUSE open round trip per file (served opens on 7.3 — §BE). The levers named in the create
  path's notes remain: the appliers' mutual leaf waits, atomic open (kernel), and the open round trip.
- ext4's own numbers moved since 10-19 (file stat 9.82 M → 14.44 M) — compare within this session.
