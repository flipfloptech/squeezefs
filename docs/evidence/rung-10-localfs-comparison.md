# Rung 10 — SqueezeFS against ext4 and xfs on the same in-memory substrate (2026-10-19)

Measurement only, at the owner's question: "we should be besting or on par with a normal local
filesystem when in memory." Same RAM-backed NVMe namespace(s) (brd through nvme-loop,
`scripts/perf/ramnvme.sh`), the workstation (32 threads, 109 GiB, `7.2.8-9.30-omarchy-sqz`), the
venue held, every phase started at k10temp Tctl ≤ 70 °C. ext4 and xfs: one namespace, default mkfs,
mounted directly. SqueezeFS: the perf mount (2 MDT + 2 OST, dist build `7a0fe2d`, two rings per
queue). `mdtest -np 32 -n 10000 -u -i 1` (no sync: ext4/xfs metadata stays in the page cache; our
flush interval is 50 ms, so creates are not durable at ack on either side); fio rows from
`scripts/perf/matrix.sh`, 20 s each. Raw outputs under `/build/perf/localfs/`, rows in
`/build/perf/mx/results.tsv` (`localfs-ext4`, `localfs-xfs`, `localfs-sqz`).

| | ext4 | xfs | SqueezeFS | SqueezeFS ÷ ext4 |
|---|---|---|---|---|
| mdtest file creation /s | 711,513 | 470,934 | 41,181 | 5.8 % |
| mdtest file stat /s | 9.82 M | 13.67 M | 10.93 M | 111 % |
| mdtest file removal /s | 572,822 | 182,711 | 36,653 | 6.4 % |
| mdtest directory creation /s | 246,469 | 368,881 | 51,806 | 21 % |
| mdtest directory removal /s | 455,717 | 157,789 | 40,377 | 8.9 % |
| 4k randread QD32×4 (IOPS) | 2,031,307 | 2,010,358 | 1,186,887 | 58 % |
| 4k randwrite QD32×4 (IOPS) | 1,963,126 | 1,940,972 | 415,417 | 21 % |
| seq read 1 MiB QD16×4 (MiB/s) | 81,833 | 82,903 | 72,693 | 89 % |
| seq write 1 MiB QD16×4 (MiB/s) | 70,480 | 70,563 | 49,709 | 70 % |

Host cores during the 4k read row: ext4 8.3, SqueezeFS 12.8 of which the daemon 8.0 (all
`sqz-fuse-ur`); the 4k write row: ext4 8.1, SqueezeFS 11.0 of which the daemon 9.2.

## Reading

- **On par or better**: stat (10.9 M/s; the cached lookup of lever 1 and two rings per queue), and
  sequential read at 89 %.
- **Data IOPS**: an in-kernel filesystem answers a 4 KiB I/O with no boundary crossing; SqueezeFS
  pays one FUSE-over-io_uring round trip per op (≈ 2.9 µs of kernel in `rung-10-read-cost.md`) plus
  its own ≈ 1.6 µs. Parity on 4k reads THROUGH FUSE is not a tuning matter; it is what the client
  bypass (charter §3, `libsqueezefs_il.so`) exists for. The write row's further gap is ours: three KV
  lookups and a journal entry per 4 KiB write.
- **Metadata creates/removes at 6 %** is the finding. 41 k creates/s over 32 ranks is ≈ 780 µs of
  wall per create per rank while the transport carries 1.19 M round trips/s: the cost is the
  metadata engine (the striped-directory verdict and its lock, the per-create journal entry and its
  KV puts, token-plane work on a solo mount, the forget/corpse path). The charter's ≥ 32 k bar is
  cleared; the owner's bar for an in-memory substrate is ext4's order of magnitude.

## Next, in order

1. The create path: profile `mdtest -np 32` the way the data levers were (the per-create split), then
   the levers that fall out — one KV transaction per create, the stripe verdict cached per worker, no
   token traffic on a solo mount, the journal entry's per-op allocations. Target ≥ 200 k creates/s
   on this box.
2. 4k writes: the same treatment on the write's KV lookups and journal entry.
3. The client bypass measured on this box against the same rows, as a matrix row.
