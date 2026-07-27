# 2026-07-27 — OQ-1: the "overwrite issues 2× the FUSE ops" finding — instrument artifact, convicted and fixed

| | |
|---|---|
| **Question** | `.benchmarks/2026-07-27-async-block-reclaim.md` OQ-1: why does a 1 MiB O_DIRECT overwrite stream read `fuse_ops` 7,936 vs the fresh-write stream's 3,968 for the same 3 GiB (elbencho 16t × 192 MiB × 1 MiB, nvmet-tcp devsub)? |
| **Verdict** | **Instrument artifact.** Kernel-side truth (`fuse:fuse_request_send` tracepoint hist, keyed `connection,opcode`) shows fresh and overwrite streams issue the **same requests within 0.2 %** (6,249 vs 6,238/6,244 for the 3 GiB row). The doubling lived in the `fuse_ops` counter itself: `ProbabilisticAtomic`'s per-thread 128-increment batches flush only at **thread death**, but handler lanes live for the daemon's lifetime — every stats-row delta carried a ±(lanes × 127) residue band in 128-op quanta. A **fresh**-write row read 7,936 once the residue phase shifted (the clinching experiment). |
| **Fix (landed, this branch)** | `ShardedAtomic` — exact, contention-striped counter (thread-owned cache-line stripes, one uncontended relaxed RMW per op, `load()` sums). Post-fix `fuse_ops` tracks the kernel hist within ±1 and fresh == overwrite. |
| **Bonus finding (real, NOT fixed here)** | Every `write(2)` syscall on this mount pays **one `GETXATTR("security.capability")` round trip** (kernel killpriv probe — bpftrace-confirmed name + stack via `fuse_getxattr`). 3,072 of the fresh row's 6,249 requests — half the stream — are this probe, on fresh AND overwrite equally. Proposed fix: negotiate `FUSE_HANDLE_KILLPRIV_V2` (§5 below). |
| **Substrate / instrument** | nvmet-tcp devsub (`SQZ_DEVSUB_TRANSPORT=tcp tests/dev_substrate.sh`), 4 meta + 4 data namespaces, cache-less format; elbencho 3.1-10 (dynamic), `-w -t 16 -s 192m -b 1m --direct`; kernel 7.1.4-1-cachyos `fuse_request_send` hist trigger (exact, kernel-side, per-connection) + `.stats` deltas. Box shared-quiet; **op-count rows only** (no latency/throughput medians recorded — contention-tolerant per campaign rule). |
| **Branch** | `perf/oq1-overwrite-op-economy` off dev `78b9498`; RED test `4ef4139`, fix `97a1f54`. |

## 1. The instrument that settles it

`/sys/kernel/tracing/events/fuse/fuse_request_send` with
`hist:key=connection,opcode` counts every request the kernel dispatches
to the daemon — transport-agnostic (fires on the FUSE-over-io_uring
path), exact, and independent of daemon bookkeeping. `.stats` snapshots
bracket each row; the mount's connection id disambiguates other FUSE
mounts on the box.

## 2. Opcode table — fresh vs overwrite (pre-fix binary, 78b9498)

3 GiB per row: elbencho 16t × 192 MiB files × 1 MiB O_DIRECT. `fw` =
fresh files, `ow1`/`ow2` = overwrites of the same files, `fw2` = fresh
files in a new dir **after** the overwrite rows.

| opcode (kernel-side) | fw | ow1 | ow2 | fw2 |
|---|---|---|---|---|
| WRITE | 3072 | 3072 | 3072 | 3072 |
| GETXATTR | 3072 | 3072 | 3072 | 3072 |
| GETATTR | 34 | 23 | 29 | 42 |
| LOOKUP | 18 | 18 | 18 | 17 |
| OPEN / CREATE | 1 / 16 | 17 / 0 | 17 / 0 | 1 / 16 |
| RELEASE | 17 | 17 | 17 | 17 |
| SETATTR | 16 | 16 | 16 | 16 |
| READ (stats snaps) | 3 | 3 | 3 | 3 |
| **kernel total** | **6249** | **6238** | **6244** | **6256** |
| **`fuse_ops` delta (the artifact)** | **3968** | **7936** | **3968** | **7936** |

* **Kernel totals are flat across fresh and overwrite** (±0.2 %). There
  is no per-op overwrite tax at the FUSE layer. The original report's
  fw→ow residual is carried by `meta_kv_journal_bytes` (737 KB → 1,197 KB,
  CoW republish of existing block maps) and displaced-block work — not by
  op count.
* **`fuse_ops` moves in exact 128-op quanta** (3968 = 31 × 128,
  7936 = 62 × 128; the buffered row below read 23,808 = 186 × 128):
  `ProbabilisticAtomic` increments by 1 and flushes exactly 128 at a
  time, so the global is always a multiple of 128 and each row's delta
  is `true_ops + residue_start − residue_end` with residue ranging over
  ±(live threads × 127) ≈ ±4k here — the size of an entire row.
* **The clincher:** `fw2` — a fresh write of brand-new files — read
  `fuse_ops = 7936`, the "overwrite" number, on kernel-identical traffic
  (6,256). The 2× tracked residue phase, not workload shape. The original
  report's reproducibility across binaries was scheduling determinism of
  the same artifact.

## 3. Discriminating rows (pre-fix binary)

| row | kernel total | WRITE | GETXATTR | SETATTR | `fuse_ops` |
|---|---|---|---|---|---|
| ow 4 MiB blocks, O_DIRECT | 3933 | 3072 | **768** | 16 | 3968 |
| ow 1 MiB blocks, buffered | 23174 | 17039 | **3072** | 2958 | 23808 |

* 4 MiB O_DIRECT: kernel splits each 4 MiB syscall into 4 × 1 MiB FUSE
  WRITEs (max_write/max_pages), but GETXATTR drops to 768 = **one per
  `write(2)` syscall**, not per FUSE WRITE — the `file_remove_privs`
  signature.
* Buffered: WRITE inflates to 17k (writethrough page batching at
  ~185 KiB avg) + a SETATTR-per-MiB times-echo stream; GETXATTR stays
  1:1 with syscalls. `fuse_ops` ≈ kernel total on both rows — the
  residue band is only *visible* when it is large relative to the row.

## 4. The GETXATTR("security.capability") tax — real, both streams

bpftrace on `kprobe:fuse_getxattr` during the O_DIRECT stream:
`@name[security.capability] = 8` for 8 writes, kernel stack =
writev-path `file_remove_privs` (killpriv probe). Mechanism: FUSE
cannot set `SB_NOSEC`, and the INIT negotiation advertises neither
`FUSE_HANDLE_KILLPRIV` (fork supports it; the daemon's MountOptions
never enables it) nor `FUSE_HANDLE_KILLPRIV_V2` (not in the fork), so
`security_inode_need_killpriv` → `__vfs_getxattr` issues one GETXATTR
round trip per write syscall, answered ENODATA by the daemon every
time. **Half of every write stream's requests are this probe.**

## 5. Proposed fix for the GETXATTR tax (follow-up — NOT landed here)

Negotiate **`FUSE_HANDLE_KILLPRIV_V2`** in the fuse3 fork + daemon:
the kernel then skips the per-write killpriv probe entirely and sends
`FUSE_WRITE`/`FUSE_SETATTR` with `FUSE_WRITE_KILL_SUIDGID` /
`FATTR_KILL_SUIDGID` where clearing is required, making the daemon
responsible for dropping setuid/setgid (and capability xattrs) on
write, truncate, chown.

* **Estimated win:** −3,072 of 6,249 requests on the 3 GiB O_DIRECT row
  (**≈ 49 % of FUSE requests on any write-syscall-bound stream**); one
  fabric-RTT-class round trip removed per write syscall. Wall-clock win
  on this rig's 16-lane 1 MiB stream is modest (the probe rides
  parallel lanes); largest on latency-bound / qd1 / small-write-syscall
  streams and the interception ring's unintercepted siblings.
* **Blast radius:** INIT negotiation in `crates/fuse3` (uapi flag bit,
  `flags2`-independent), write/setattr handlers' suid/sgid-clearing
  law (the daemon already implements the killpriv chmod series for
  SETATTR — the 683-golden setgid-series law), truncate/fallocate
  paths, and pjdfstest/fstests killpriv coverage (fstests generic/193,
  314, 355, 673, 683-class) must gate it. It is a **semantics**
  change, not a counter change — deliberately out of scope for this
  diagnosis branch and flagged instead of rushed.
* Reference clients: JuiceFS/libfuse default `HANDLE_KILLPRIV_V2` on
  supporting kernels for exactly this reason (docs/reference-clients-survey.md
  per-write round-trip economy class, D2-family).

## 6. The landed fix (the OQ-1 artifact itself)

`src/fuse_client.rs`: `ProbabilisticAtomic` + its `ThreadLocalState`
deleted; `fuse_ops` is now a `ShardedAtomic` — 64 cache-line-aligned
stripes, each thread round-robin-assigned one on first use (TLS-cached),
`fetch_add` = one uncontended relaxed RMW on the owned stripe (no locks,
no cross-lane cache-line bouncing — latch-free posture unchanged),
`load()` sums stripes (stats reads are rare). Contract pinned red-first
in `tests/metrics_counter_tests.rs`:

* `fuse_ops_single_increment_is_immediately_visible` — one increment
  moves `load()` by exactly 1 (RED: probabilistic read 0).
* `fuse_ops_exact_across_live_parked_threads` — 16 **live, parked**
  threads × 100 sub-threshold increments all visible before any thread
  exits (RED: read 1 of 1,600; thread-death flush must not be what makes
  the counter right).

### Post-fix rig verification (fixed binary, same rig, fresh format)

| row | kernel total | `fuse_ops` delta | match |
|---|---|---|---|
| fix_fw | 6279 | 6279 | ±0 |
| fix_ow1 | 6266 | 6267 | ±1 (snapshot ordering: the hist read precedes the `.stats` read, whose own READ op lands between them) |
| fix_ow2 | 6265 | 6266 | ±1 |

Fresh == overwrite; `fuse_ops` is now a per-row-exact engagement
instrument (the preload gate's `lseek SEEK_CUR is FUSE-free` pin and
`write_matrix.sh` deltas inherit the exactness — the old counter could
spuriously flush ≥128 mid-window or hide an entire small row).

Quiet-box A-B-B-A throughput brackets were **not** recorded: the box
hosted sibling campaigns during this window, so per campaign rule the
evidence is op-count deltas only (contention-tolerant). TODO: a quiet
A-B-B-A bracket if anyone wants a throughput claim for the counter swap
(none is expected — the swap is metrics-plane only).

## 7. Consequences for the standing record

* `.benchmarks/2026-07-27-async-block-reclaim.md` §3's "fuse_ops
  doubles on overwrite (~1.6 extra FUSE ops per 1 MiB write)" line is
  **retracted by this report**: op counts are equal; the residual fw→ow
  gap attribution reduces to journal-byte growth (CoW republish) +
  displaced-block costs already named there. OQ-1 is closed.
* Any historical analysis that leaned on small `fuse_ops` deltas
  (< ~4k) from multi-lane runs should be re-read with the 128-quantum
  residue band in mind.

## 8. Repro

```bash
SQZ_DEVSUB_TRANSPORT=tcp sudo tests/dev_substrate.sh create
squeezefs format "sqmeta:///dev/nvme1n1,...4n1" "sqdata:///dev/nvme5n1,...8n1" --force
SQUEEZEFS_OP_PROFILE=1 squeezefs mount "sqmeta://..." /mnt/oq1 --daemon --allow-others --mem-cache-size 1GB
echo 'hist:key=connection,opcode' > /sys/kernel/tracing/events/fuse/fuse_request_send/trigger
# per row: snapshot hist + /mnt/oq1/.stats, run
#   elbencho -w -t 16 -s 192m -b 1m --direct /mnt/oq1/d/f{1..16}
# then snapshot again; diff per (connection, opcode).
# killpriv name/stack: bpftrace -e 'kprobe:fuse_getxattr { @[str(arg1)]=count(); @s[kstack(6)]=count(); }'
```
