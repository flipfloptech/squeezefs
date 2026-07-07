# Mount-bench baseline: `pre_wal_removal`

Committed per design-wal-crash-consistency Rollout §3 (review Issue 6): the
auditable baseline for PR 4's "no regression" gates and PR 5's
**Metadata Delete ≥ 2×** gate.

## Provenance

| | |
|---|---|
| Commit | `9b02b3a` (dev; includes design PRs 1–3: superblock validation, ino quarantine, crash harness — **pre WAL deletion**) |
| Date | 2026-07-07 |
| Binary | `cargo build --release` (release profile keeps `debug = true`) |
| Machine | AMD RYZEN AI MAX+ PRO 395 w/ Radeon 8060S, 32 hw threads, 94 GiB RAM |
| Kernel | 7.1.3-1-cachyos |
| Meta volume | file-backed 256 MiB on tmpfs (`sqmeta:///tmp/sqfs_bl/meta.bin`) |
| Data volume | file-backed 16 GiB on tmpfs (`sqdata:///tmp/sqfs_bl/data.bin`) |
| Staging | `/tmp/sqfs_bl_staging` (tmpfs) |
| Mount | `squeezefs mount sqmeta:///tmp/sqfs_bl/meta.bin <mnt> --disk-cache-paths <staging> --daemon --allow-others` — FUSE-over-io_uring armed (queues=32, depth=4) |
| Flush knob | unset (default: 50 ms deferred) |
| Command | `sudo squeezefs bench <mnt> -t 10 --small-size 4096 --large-size 1024` |

tmpfs-backed volumes mean the numbers isolate the **software** path (FUSE +
commit protocol + WAL worker) from device latency — exactly the costs PR 4/5
change. The meta/data cost split for the Delete row (§4.5): the bench's
small files are staged/inline layouts whose `delete_file` data-path teardown
is cheap relative to the two metadata transactions each delete pays at this
commit (unlink + destroy, each with a WAL worker round-trip).

## Results (T=10, Large=1024 MB/thread, Small=4096 KB × 100/thread)

| Workload | Throughput | IOPS | Avg latency |
|---|---|---|---|
| Write (Large Seq) | 1242.09 MiB/s | 1242.09 ops/s | 0.81 ms |
| Read (Large Seq) | 1921.68 MiB/s | 1921.68 ops/s | 0.52 ms |
| Write (Large Rand) | 688.57 MiB/s | 688.57 ops/s | 1.45 ms |
| Read (Large Rand) | 1135.05 MiB/s | 1135.05 ops/s | 0.88 ms |
| Write (Small Seq) | 441.03 MiB/s | 110.26 ops/s | 9.07 ms |
| Read (Small Seq) | 2022.55 MiB/s | 505.64 ops/s | 1.98 ms |
| Write (Small Rand) | 395.43 MiB/s | 98.86 ops/s | 10.12 ms |
| Read (Small Rand) | 455.35 MiB/s | 113.84 ops/s | 8.78 ms |
| **Metadata Stat** | — | **69489.91 ops/s** | 0.01 ms |
| **Metadata Mkdir** | — | **18230.49 ops/s** | 0.05 ms |
| **Metadata Readdir** | — | **10028.21 ops/s** | 0.10 ms |
| **Metadata Rmdir** | — | **16737.35 ops/s** | 0.06 ms |
| **Metadata Delete** | — | **1516.73 ops/s** | 0.66 ms |

## Gates keyed on this table

- **PR 4** (WAL deletion): metadata rows at-or-better; no `lookup`/Stat
  regression. Criterion companion baseline: `pre_wal_removal`
  (`target/criterion`, saved same commit): `create_unlink_file` 207.35 µs,
  `lookup_file` 7.69 µs, `set_get_xattr` 154.40 µs (medians).
- **PR 5** (reclaim group-commit): **Metadata Delete ≥ 2× ⇒ ≥ 3033 ops/s**
  on this machine/profile.
