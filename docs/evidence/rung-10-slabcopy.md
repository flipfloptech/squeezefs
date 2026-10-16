# Rung 10 — the zc bounce slab, one memfd per queue (slabcopy; perf-baseline lever 2)

**What changed.** `worker.Slab` (`src/fuse/transport/worker.zig`) was ONE memfd for every queue's
ent windows. It is now one VA span (reserved `PROT_NONE`), and each queue's `depth × stride` windows
are a memfd of their own, mapped `MAP_SHARED|MAP_FIXED` over that queue's share. The VA face (serves,
leases, `window()`) is unchanged. The fd face is now the queue's file at `ent × stride` (`fdOf`,
`fdOffsetOf`); the three bounce bridges and the idle-trim punch use it.

**Why this design, not a VA memcpy.** A paged WRITE's bytes sit in the app's pages, installed as the
zc slot (`imu->ubuf = 0`). The daemon has no address for them, and the only way out is a fixed-buffer
op into an fd (`WRITE_FIXED(slot → fd)`). So the copy cannot move to the worker; only its target file
can change. An extraction is a shmem write under that file's `i_rwsem`. io_uring already hashes one
ring's writes to one inode into one io-wq chain, so the wait was cross-ring: every ring's chain met
on the one inode. A file per queue removes that meeting point. It keeps the zero-copy/retention
contract (held slot, lazy extract, bounce leg, trim unchanged) and allocates nothing per op (the fd
array is allocated once at bring-up); it costs `nqueues` fds and VMAs. This was the reference's
`ZcBounce` shape: the port's one-slab merge had no stated reason.

**Red → green.** `the zc bounce slab: one memfd per queue mapped in place …`: queue 2's window, read
through its fd face at `ent × stride`, must hold the VA write; a write through the fd face must show
in the VA; three queues must have three inodes. Red (`53afb21`, today's one file behind the new
faces): `expected 171, found 0`. Green: `fuse.transport` 123/123, and the smoke + bench-smoke rows
(KILL MATRIX, JOBS MATRIX, KERNEL AIO, CACHED COPY) 84/84 on kamrui.

**A/B on kamrui RAM-backed NVMe** (`baseline.sh ram-sqz`, dist builds; legs interleaved A B B A, then
B A A B on the seq-write rows; median (spread, n)). **I**: A = `d7c4121`, B = the same plus this
change. **II**: after `layoutlookup` (lever 1) landed, A = `9205013`, B = this branch rebased.

| row | I: A | I: B | II: A | II: B |
|---|---|---|---|---|
| 1M seq write QD16×4 (MiB/s) | 2324 (12 %, 4) | 2318 (24 %, 4) | 2630 (11 %, 4) | **3254** (13 %, 4) **+24 %** |
| 4M seq write QD4×4 | 2187 (13 %, 4) | 2251 (11 %, 4) | 2641 (15 %, 4) | 2888 (25 %, 4) +9 % |
| 1M seq write QD16×1 | 1229 (3 %, 4) | 1262 (3 %, 4) | 1973 (11 %, 4) | 1913 (15 %, 4) |
| 4k randread QD1 / QD32×1 / QD32×4 (IOPS) | 8390 / 13627 / 33529 | 8278 / 13304 / 33250 | 19440 / 80870 / 250227 | 19598 / 81570 / 249080 |
| 4k randwrite QD1 / QD32×1 / QD32×4 | 3918 / 6228 / 11550 | 3977 / 6171 / 11422 | 9284 / 32151 / 74546 | 9432 / 32280 / 74062 |
| 64k randread / randwrite (MiB/s) | 1780 / 720 | 1772 / 723 | 6271 / 3340 | 6169 / 3261 |
| 1M seq read ×1 / ×4 | 5447 / 6678 | 5450 / 6673 | 6793 / 7755 | 6796 / 7776 |

On 1M ×4 in II the four runs do not overlap (A 2465–2743, B 2925–3359). Small-block and read rows
stay within ±2 % (n = 2–4; I's 4k rows QD1 and QD32×1 at n = 4).

**CPU and lock, 1M seq write ×4** (`/proc/<pid>/task` deltas, medians; `perf record -a -g` 10 s
mid-row).

| | I: A | I: B | II: A | II: B |
|---|---|---|---|---|
| io-wq cores · ms/MiB | 2.97 · 1.32 | 1.45 · 0.62 | 4.21 · 1.60 | 2.50 · 0.77 |
| daemon ms/MiB | 2.50 | 1.95 | 2.23 | 1.50 |
| io-wq samples in `down_write` (i_rwsem) | 67.2 % | 0.03 % | 70.6 % | 0.01 % |
| … in `memcpy` | 16 % | 58 % | 16 % | 63 % |

In A, the lock time is optimistic spinning (`osq_lock` + `rwsem_spin_on_owner`).

**Reading.** Before lever 1, the spinning burnt CPU (io-wq −53 % per MiB) but did not set the
ceiling: freed requests queued one stage later on the per-file DLM guard (7–10 % → 14–19 % of
writes waited), whose critical section was the layout walk. With lever 1 landed, the slab became
the ceiling and this change gives +24 % on 1M ×4. The ≈ 2× hypothesis still falls short. Next, 51 % of writes wait on the
per-file DLM guard (98,939 writes, 50,103 waits, mean 10.3 ms, 1M ×4 in II B): writes from one
job's QD16 to disjoint ranges of one file serialize. The extraction's memcpy (io-wq 23 % of all host
samples) is the largest CPU term after that. Every session ended with a clean `fsck --offline`.

**Gate** (kamrui, `-Dfresh=true`, rebased head): 55/55 steps succeeded; 2994/3077 tests passed (83 skipped).
