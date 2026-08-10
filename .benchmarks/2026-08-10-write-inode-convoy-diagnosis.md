# 2026-08-10 — Write-Inode-Convoy Diagnosis (rand-4k write ceiling)

| | |
|---|---|
| **Binary** | dev `e4f756a9` (`cargo build --release`, default features — measurement-valid) |
| **Substrate** | Production fabric rig `memp-s3ds-aqs-37` ("squeeze-test"): 32 CPUs, 5 meta vols (`/dev/nvme{0,2,4,6,8}n1`) + 10 data vols (`/dev/nvme{10..28 even}n1`), mount `--interception --allow-other` |
| **Instrument** | elbencho 3.1-11 (dynamic; libaio via `--iodepth 32`), shim `libsqueezefs_il.so` same commit; 32 × 2 GiB files preconditioned seq-1m |
| **Row shape** | rand-4k overwrite, `-t 32 -b 4k --iodepth 32 --rand --direct --lat --infloop --timelimit 60` (f8 probe: 30 s) |
| **Ordering** | il → kern → il → il(sessions) → kern(f8). il repeated after kern reproduced within 1.5 % (205k→208k), excluding store-aging artifacts for the il number; the kern row is single-order **diagnosis-grade** — campaign gate rows re-run full A-B-B-A |
| **Engagement** | exact on every row (charter rule 4): `ipc_ops_write` / `patch_writes` deltas vs row ops below; write amp = device÷user bytes |

## Rows

| Row | IOPS | avg lat | ops (fuse/ipc) | patch_writes | W1 share | device written | write amp | data-dev qd (Σ) |
|---|---|---|---|---|---|---|---|---|
| il 32×32 | 204,972 | 4.99 ms | 12,300,127 / 12,299,992 | 12,295,008 | 99.96 % | 48,114 MiB | 1.027 | — |
| kern 32×32 | 391,439 | 2.62 ms | 23,488,293 / — | 23,481,052 | 99.97 % | 91,769 MiB | 1.024 | ~1.5/dev (15.1) |
| il 32×32 (repeat) | 208,060 | 4.92 ms | — / 12,485,170 | 12,477,621 | 99.94 % | — | — | ~1.0/dev (10.0) |
| il + `SQUEEZEFS_IL_SESSIONS=16` | 206,335 | 4.96 ms | — / 12,381,629 | 12,372,498 | 99.93 % | — | — | ~1.0/dev (9.9) |
| kern, 8 files (30 s) | 266,381 | 3.84 ms | — | — | — | — | — | (4.3) |

Patch decision-ledger residue (kern row): `patch_ineligible_overlay` +7,093,
`patch_ineligible_adjacent` +9 — noise against 23.5M ops. Meta plane quiet:
`meta_kv_journal_entries` +6,058 (~101/s). `ipc_severed_pool_hits` ≡
`ipc_ops_write` on il rows (sever pool 100 %).

## Phase decomposition (kern row, n = 23,488,154; always-on histograms)

| Term | approx mean | top buckets (delta) |
|---|---|---|
| **`write_lock_wait`** | **≈ 1,120 µs** | ≤1 µs: 9,954,614 · ≤2 ms: 6,240,753 · ≤1024 µs: 3,542,972 · ≤4 ms: 2,231,370 |
| `write_transport_phase_ns.transport_total` | ≈ 1,715 µs | ≤2 ms: 7,419,896 · ≤1024 µs: 6,643,534 |
| `write_transport_phase_ns.queue_wait` | 60 µs | |
| `block_lock_wait` | 24 µs | 95 % ≤1 µs |
| `lease_lock_wait` | n = **32** total | one lease per file for the whole row |
| dispatch_lag / reply_commit | ~1 µs | |

## Model + verdict

Little's law closes on every row: kern 391k × 2.62 ms ≈ 1,024 = offered
depth; il 205k × 4.99 ms ≈ 1,022. Per-inode: 391k ÷ 32 ≈ 12.2k/s/inode ≈
1 ÷ 82 µs — one write per inode per (wake + cache-hit meta-prep) interval.
The ~1.12 ms `write_lock_wait` mean is the serialized tokio-wake chain of
~32 queued writers per inode's **write** guard (held time is the µs-class
meta-prep; the cost is the handoff wake, ~15–30 µs × queue length). The
devices idle at qd ≈ 1–1.5 (Σ 10–15 against 1,024 offered): the next
same-inode writer is not admitted until the previous op's DMA already
finished. Session lever (=16) is a no-op; file-width probe (8 files) scales
sub-linearly — both consistent with the guard convoy and nothing else.

il trails kern (205k vs 391k) — a write_matrix parity violation on this
shape; the handoff venue joins the same per-inode queue. Re-adjudicate after
the convoy falls (campaign PR 4 if residual).

**Named ceiling: the per-inode exclusive write-guard convoy.** Fix program:
`docs/design-write-inode-convoy.md`.
