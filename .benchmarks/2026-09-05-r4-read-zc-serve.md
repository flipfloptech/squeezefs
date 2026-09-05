# 2026-09-05 — R-4: the READ zc serve, per arm (`perf/read-zc-serve`)

E2e perf audit ladder row 11 / read board items #4 and #5 (the whole-box
CPU wall at 2.69 passes/byte cold whole-block, and the warm-serve pass
`read_copy_warm_serve_bytes` "no software lever left — only zc deletes
it"; lever named as "zc serve — `READ_FIXED` into folios — on the sqz
kernel"). Branch `perf/read-zc-serve` off dev tip `27a396e1`. Unprivileged
session on the 7.1.8-cachyos (sqz 7.1 track) box: no root tests, no
`task check`, no fstests — four sibling campaigns shared the box. The
field rows and the zc-capability gate are the PARENT's (§7).

Commits (in order): `deda3681` test / `49d14d4c` feat (the per-arm copy
split) · `2fa7b473` test / `abd3a798` feat (the R-2 fast-probe ledger gap)
· `35400e0f` test / `351760cf` feat (the lever) · rows + this note.

## 1. Verdict in one paragraph

**The lever is a DAEMON item, not a kernel item — and the board's
framing of the warm pass as "only zc deletes it" was half right.** On a
zc-armed session every out-paged READ reply body already reaches the
request's pages by `READ_FIXED(fd → slot)` — the kernel installed the
READ's folios as a sparse fixed buffer in the daemon's queue ring (patch
0024, `io_buffer_register_bvec`, `ITER_DEST`), and the fork names two
SOURCES for that op today: the device fd (the direct leg, `zc_device_fetch`
/ `ZcFetchMsg`) and the per-ent bounce memfd (every other paged reply —
`commit_ready_reply`'s `ZcPend::BounceFetch`). That op IS "`READ_FIXED`
into folios". What no kernel offers is a **VA**-sourced form — there is
no io_uring op that copies an anonymous daemon buffer into a fixed
buffer, which is exactly why `ZcBounce` exists ("an anonymous mapping
has no fd for the ring ops to name", `crates/fuse3/src/raw/connection/zc.rs`).
So the warm hot/hold serve and the cold whole-block slice-out on an
armed session paid TWO passes — the daemon's tier → bounce copy, then the
kernel's bounce → folios bridge — because the tier buffer had no fd. The
software lever is to GIVE it one: the whole-block READ fill pool as a
`MAP_SHARED` memfd slab (the bounce's own two-way-reachability law), so
the router hands the transport the tier buffer's `(fd, offset)` and the
worker bridges THAT into the folios. One kernel pass, the daemon copy
deleted, no kernel change. Landed behind `SQUEEZEFS_READ_ZC_SERVE`
(default off — a measurement lever until the field A-B-B-A adjudicates).

## 2. The per-arm ledger (from the code, on an ARMED session)

Terms: **RX** = the nvme-tcp receive copy (kernel softirq; 0.69 passes /
user byte at read_amp 0.69, 1.00 at amp 1 — interface-class, priced and
stopped on: `.benchmarks/2026-08-02-read-copy-count.md` §3.1); **D** =
a daemon CPU pass over payload bytes (the `read_copy_*` ledger);
**B** = the kernel's bridge pass `READ_FIXED(fd → slot)` — shmem/page
cache → the request's folios (the K1 commit copy's zc-era twin;
`fuse3_zc_replies` counts it, `zc_bridge_phase_ns` prices its hops).

| Arm | Path today (armed) | Passes/byte today | Site (code) | The pass a "`READ_FIXED` into folios" serve deletes | Kernel prerequisite? |
|---|---|---|---|---|---|
| (a) cold whole-block fill → slice-out (unaligned / transform / cohort-joined / lease-declined windows; the EXA row's ONE daemon pass) | device → pool fill [RX] → bounce [D: `serve_copy_to_dest`] → folios [B] | **RX + 1 D + 1 B = 2.69** (amp 0.69) | `routing.rs` fetch-loop `(Some(val), Some(dest))` slice and the stale-absent slice — now counted `read_copy_fill_slice_bytes` | **D** — bridge from the pool buffer instead of the bounce → RX + B = **1.69** | **No** — the fill pool must be fd-addressable (daemon substrate) |
| (a′) cold whole-block, ALIGNED passthrough (`zc_geometry`) | device → folios [RX, the direct leg] | **1.0** — already the floor (`read_zc_serve_bytes`) | `routing.rs` "FUSE-zc direct leg" → `zc_device_fetch` | nothing | n/a (shipped since 2026-08-06; device-true by design) |
| (b) dest-window lease (`read_dest_lease_bytes`) | device → dest [RX]; on an armed session `dest` IS the bounce → folios [B] | **2.0** armed; 1.0 + K1 on stock kernels | ranged leg with `RangedDest::with_lease` | superseded on armed sessions: the direct leg takes every cold aligned sub-block window first | n/a |
| (c-hot) warm hot-tier serve | hot `Bytes` (pool memory) → bounce [D] → folios [B] | **2.0** | handler arm `routing.rs` "R4 hot-block fast path"; R-2 fast probe `try_read_range_sync` leg 2 (`FastReadSink::Window`) | **D** → **1.0** | **No** — hot entries ARE fill-pool `Bytes` (refcount, never copied in) |
| (c-hold) warm read-lane-hold serve | held `Bytes` (pool memory) → bounce [D] → folios [B] | **2.0** | handler arm "Read-lane hold fast path"; fast probe leg 2b | **D** → **1.0** | **No** — same substrate |
| (c-cache) warm NVMe read-cache serve | mmap'd segment ring (a FILE) → bounce [D] → folios [B] | **2.0** | handler "Tier fast path with binding recheck"; fast probe leg 3 | **D** in principle (`READ_FIXED(segment fd)`), but the ring recycles under the reader — needs a post-CQE generation check the mmap guard does synchronously today; field-irrelevant (cache-less format) — **left on the copy path, counted** (`read_copy_cache_serve_bytes`) | No (daemon) |
| (d) transform volumes (compressed / encrypted) | device → pool [RX] → decode [D₁, unavoidable] → bounce [D₂] → folios [B] | RX + 2 D + B | `get_block_for_index` → `process_read` | **D₂** iff the decode output were pool-backed (it is a fresh buffer today) — next rung | No (daemon) |
| (e) multi-block assembly | per-block slices → dest [D] → folios [B] | 2.0 + RX share | `assembly_tasks` writers, counted `read_copy_dest_bytes` | D (a scatter bridge — one `READ_FIXED` per block slice) — rare on 1 MiB-aligned shapes, not built | No (daemon) |
| (f) il arena dests | pool/tier → arena [D, `ipc_arena_copy_bytes`] → client `slab_read` | 2.0 | `ArenaWindow::write_at` | none — no ring slot, no folios; the SDK's arena-native buffers are that program | n/a |

**Two ledger findings the audit did not have:**

1. **The R-2 fast-probe warm serve was OUTSIDE the READ copy ledger.**
   Since 2026-09-03 the reap thread's SYNC warm ladder
   (`Filesystem::read_fast_probe` → `DataRouter::try_read_range_sync`)
   copies hot/hold/read-cache bytes into the reply window
   (`FastReadSink::Window::write_at`) and is THE warm venue on an armed
   session — but no `read_copy_*` bucket counted it, so a warm armed row
   would have failed closure (`dest + bounce + dest_dma + zc_serve <
   served`) by the ledger's own validity rule. Fixed (`abd3a798`):
   `try_read_range_sync` reports the arm (`SyncServeArm::{Staged, Hot,
   Hold, Cache}`), the kernel probe counts dest (window) / bounce (heap
   stand-in) + warm + the arm; the il caller ignores the arm (its arena
   sink already counts `ipc_arena_copy_bytes`). Pinned:
   `read_copy_ledger_tests::warm_arms_and_the_cold_fill_slice_are_attributed_per_arm`
   phase E (red against the tree: dest delta 0).
2. **On an armed passthrough mount the aligned cold population never
   becomes warm.** The direct leg is device-true by design (no tier
   deposit) and dest-leaseable/zc-eligible traffic stands the lane and
   R2 down, so `r_warm` on an armed mount re-reads through the direct
   leg unless something else warmed the tier: cohort joins (a second
   reader hitting the in-flight fill — the EXA rows' 2 readers/file, the
   follower serves from the pooled fill: arm (a)), unaligned windows,
   ranged escalations. The lever's field population is therefore arms
   (a) + (c) on those shapes, not "every re-read".

## 3. The instrument (landed, always-on)

| Counter | Meaning | Law |
|---|---|---|
| `read_copy_hot_serve_bytes` / `read_copy_hold_serve_bytes` / `read_copy_cache_serve_bytes` | the three tier arms of the warm serve copy (handler arms AND the R-2 fast probe) | `hot + hold + cache ≡ read_copy_warm_serve_bytes` — an exact PARTITION, counted AT the copy |
| `read_copy_fill_slice_bytes` | the cold whole-block fill → dest slice-out (both faces) | `⊆ read_copy_dest_bytes − read_copy_warm_serve_bytes`; the remainder = ranged bounce-with-dest legs, ranged escalation slices, assembly slices, staged serves (unnamed by design) |
| `read_zc_pool_serve_bytes` | bytes served by fd-source (the lever engaged: pool slice handed back, no copy) | a NEW closure term: `zc_serve + zc_pool_serve + dest + bounce + dest_dma ≈ served` |
| `read_zc_pool_serve_warm_bytes` | the hot + hold subset | `total − warm` = the cold fill-slice share |
| `fuse3_zc_fd_body_replies` | transport vehicle count (fd-bridged commits) | ≈ fd-source serves; the difference is the composed-window residue (a parked-run compose rebuilt the body on the heap → ordinary reply), ≈ 0 |
| `fuse3_zc_fd_body_fallbacks` | an fd-source bridge failed and re-staged through the bounce | **must stay ≈ 0**; the reply is never lost either way |

All export under `metrics` (pinned in `metrics_tests::fuse_zc_ledger_always_exports_under_metrics`).

## 4. The lever — `SQUEEZEFS_READ_ZC_SERVE` (default ON since the 2026-09-05 field bracket, §7)

**Substrate** (`src/cache/pool.rs`): `AlignedBufPool::new_memfd_slabbed` —
one `memfd_create` + `ftruncate(capacity × 4 MiB)` + `mmap(MAP_SHARED)`;
slots carved like the PERF-4 (b) heap slab (same alignment, zeroed at
birth, committed on first touch — no `MAP_POPULATE`, like the heap pool);
`fd_offset_of(ptr, len)` answers `(fd, offset)` for any range inside the
slab (every `Bytes` slice of a handout qualifies), `None` for heap slabs,
fresh over-capacity backings and foreign memory. `ZC_FILL_POOL` is built
iff the knob is on (a memfd failure logs loud and declines — the lever
changes copy economics, never correctness); `read_bounce_pool(size)`
routes whole-block fills to it. A SEPARATE pool from `ALIGNED_BUF_POOL`
so the write path's `ActiveBlockBuf` substrate is untouched — the A/B
moves the READ serve only. Registered with R5 as `zc_fill_pool`. Same
derived capacity (`block_pool_capacity_from(cores)`): entries beyond it
are fresh heap backings and take the copy path — the pool capacity is
the lever's COVERAGE bound on a hot tier larger than the pool
(`aligned_pool_misses` prices it; a sparse memfd reserved to the R5
budget with hole-punch trim is the growth rung).

**Serve** (`src/routing.rs`, `src/fuse_client.rs`): `zc_fd_source(zc,
dest_addr, src)` gates each arm — a zc handle (armed ring slot, no arena),
a dest (the bounce — the copy this elides), and pool memory. The hot,
hold and both cold fill-slice arms hand back `src.slice(start..end)`
instead of `serve_copy_to_dest`; counted only on the proven serve (after
the binding check), so a rebind retry never leaves a phantom count. The
R-2 fast probe gets the same through `PayloadSink::offer_fd_body`
(default `false` — il arena sinks and heap stand-ins pay nothing, no
clone unless accepted): `FastReadSink::Window` stores the slice + fd
address and the probe answers `FastReadProbe::ServedFd`. The FUSE
handler re-probes the body at reply time (`ReplyData::zc_fd_body`) — a
parked-run compose rebuilt it on the heap → ordinary reply.

**Transport** (`crates/fuse3`): `CommitMsg::body_fd`;
`FuseConnection::submit_reply_fd_body` passes the body `Bytes` VERBATIM
(the `write_vectored` path would heap-copy a non-dest body — the copy
the lever exists to delete); `commit_ready_reply`'s zc out-paged arm
skips the staging copy and pushes `READ_FIXED(fd @ off → slot)` with the
body riding `ZcPend::BounceFetch { fd_source: true }` as the keepalive
until the CQE — **the buffer-lease law by construction**: a pool slot
recycles only when the last `Bytes` owner drops, and the pend owns one.
A failed fd-source bridge answers `PendDone::Restage(msg)`: the caller
re-runs `commit_ready_reply` with the fd dropped (stage → bounce →
bridge — the shipped path), re-stamping the bridge deadline the caller
had just cleared; teardown drains an fd-source pend exactly like a bounce
pend (header-only EIO). The fast-dispatch inline serve commits
`ServedFd` with `body_fd` set. Counters `fuse3_zc_fd_body_{replies,fallbacks}`.

**What the lever does NOT touch**: the direct leg, the dest lease, il
arena serves, the NVMe read-cache arm (§2 c-cache), transform decode
output, the assembly arm, the write path's pool, any kernel patch. Per-op
economics vs today: the SAME bridge op count (the bounce bridge is
replaced by the pool bridge, not added to), minus one memcpy per served
byte; the bridge's kernel-side cost profile is the shipped one (same
`READ_FIXED(memfd)` shape, 4 KiB shmem folios unless collapsed — the
bounce is not THP-collapsed either).

## 5. In-process rows (release build, unprivileged, file-backed sandbox)

`tests/read_zc_serve_rows.rs::warm_hot_loop_row` — 8 × 1 MiB blocks
warmed through the ordinary ladder, then 64 × 1 MiB hot reads through the
router's hot arm with a zc handle + a 4 KiB-aligned dest (the bounce
stand-in); one JSON line per process. A = lever off, B =
`SQUEEZEFS_READ_ZC_SERVE=1`. Instrument: `cargo test --release --test
read_zc_serve_rows -- --nocapture`. Not a perf claim (no transport, the
fake zc handle never fetches): it prices the DAEMON copy side and shows
the ledger closing on both legs.

Shape: 8 × 1 MiB striped blocks, warmed hot in a non-sequential order
(a sequential sweep is stream-classified and R1b skips the hot publish),
then 64 hot reads of an UNALIGNED 384 KiB window (`off = blk × 1 MiB +
1234` — above the ranged threshold, not `zc_geometry`-aligned, so only
the hot arm can serve it; `hot_block_hits` = 64 on every leg). Eight
processes, A-B-B-A twice (the order as run):

| Leg | lever | served B | `read_copy_dest_bytes` | `read_copy_warm_serve_bytes` | `read_copy_hot_serve_bytes` | `read_zc_pool_serve_bytes` | `read_zc_pool_serve_warm_bytes` | hot hits | handler ns/op |
|---|---|---|---|---|---|---|---|---|---|
| A1 | off | 25,165,824 | 25,165,824 | 25,165,824 | 25,165,824 | 0 | 0 | 64 | 10,452 |
| B1 | on | 25,165,824 | 0 | 0 | 0 | 25,165,824 | 25,165,824 | 64 | 1,733 |
| B2 | on | 25,165,824 | 0 | 0 | 0 | 25,165,824 | 25,165,824 | 64 | 1,232 |
| A2 | off | 25,165,824 | 25,165,824 | 25,165,824 | 25,165,824 | 0 | 0 | 64 | 10,543 |
| A3 | off | 25,165,824 | 25,165,824 | 25,165,824 | 25,165,824 | 0 | 0 | 64 | 9,074 |
| B3 | on | 25,165,824 | 0 | 0 | 0 | 25,165,824 | 25,165,824 | 64 | 2,212 |
| B4 | on | 25,165,824 | 0 | 0 | 0 | 25,165,824 | 25,165,824 | 64 | 1,757 |
| A4 | off | 25,165,824 | 25,165,824 | 25,165,824 | 25,165,824 | 0 | 0 | 64 | 11,649 |

Reading: closure is exact on every leg (`dest + zc_pool_serve ≡ served`,
asserted in the harness); with the lever on every served byte moves from
the copy buckets to the fd-source bucket (the per-arm split says which
arm: all hot, as the shape intends) and the handler-lane time per op drops
from 9.1–11.6 µs to 1.2–2.2 µs — the 384 KiB memcpy is the term that
left. The kernel bridge (`READ_FIXED(pool fd → slot)` vs `READ_FIXED(bounce
→ slot)`) is NOT in this loop: same op, different source fd, priced only
by the field row.

Contracts (all red-first, all green; counted runs in §6):
`read_zc_pool_serve_tests` (substrate two-way reachability; copy elision
on the fill-slice / hot / hold arms with exact bytes and fd addresses
that `pread` back to the served bytes; the read-cache arm and un-armed
requests keep the copy path; the fast probe answers `ServedFd`),
`read_copy_ledger_tests` (the per-arm partition + the fast-probe venue),
`cache::pool::memfd_slab_is_fd_addressable_and_keeps_the_slab_laws`.

## 6. Gate (this side)

Unprivileged, targeted (the box was shared by four campaigns; no `task
check`, no fstests, no root suites — the parent's):

| Check | Result |
|---|---|
| `cargo fmt --check` (root) + `cargo fmt --check` (fork) | clean |
| `cargo clippy --all-targets --all-features -- -D warnings` (root) | clean |
| `cargo clippy --all-targets -- -D warnings` (root, the SHIPPED config) | clean |
| `cargo clippy --all-targets --all-features -- -D warnings` (`crates/fuse3`) | clean |
| `RUSTDOCFLAGS="-D warnings" cargo doc --no-deps` (root + fork) | clean (exit 0 both) |
| `cargo test` (fork, `crates/fuse3`) | 235 passed |
| `cargo test --lib cache::pool` | 5 passed (incl. the new memfd-slab law test) |
| `read_zc_pool_serve_tests` (3) · `read_copy_ledger_tests` (4) · `read_dest_lease_tests` (1) · `read_fast_dispatch_tests` (6) · `rebind_starvation_tests` (5) · `read_lane_tests` (18) · `env_knob_convention_tests` (21) · `no_tokio_convention_tests` (2) · `metrics_tests` (9) · `fuse_zc_serve_tests` (2) · `ipc_hold_probe_tests` (5) · `ipc_op_economy_tests` (5) · `nt_read_serve_tests` (1) — `--test-threads=1` | all green |
| Counted ×20: `read_zc_pool_serve_tests` + `read_copy_ledger_tests` (the new contracts), each rep both binaries `--test-threads=1` | **20/20 pass, 0 fail** |
| `tests/check_markdown_links.sh` on the four touched docs | 156 links, 0 broken |

Red-first proofs kept in the commit record: the per-arm split (four
missing fields → compile-red, `deda3681`), the fast-probe venue (phase E
dest delta 0 against the tree, `2fa7b473` — proven by stash-and-run), the
lever (API absent → compile-red, `35400e0f`).

## 7. Field rows — MEASURED 2026-09-05 (dev box, tcp devsub, root, zc armed)

Venue: this box's `7.1.8-cachyos-lto` (sqz 7.1 patch track) — `fuse3_zc_
negotiated` = 1 on every leg; tcp devsub (zram OSS at memory-bus speed, so
the rows are CPU-per-byte rows, not device rows); same `release` binary
`d551f1ba` (the five-campaign stack); rig
`.benchmarks/rigs/2026-09-05-read-lever-abba-local.sh` (8 × 1 GiB minted
once from `/dev/urandom`; per leg a COLD remount, then `r_cold` and
`r_warm` = fio 3.42 libaio `direct=1`, 16 readers qd8 — readers j and j+8
share file j, the EXA follower shape — 1 MiB reads); **A =
`SQUEEZEFS_READ_ZC_SERVE=1` / B = `0`**, order A B B A; artifacts
`.benchmarks/rows-r4-read-zc-20260905/{aligned,unaligned}/`.

### 7.1 Aligned reads — the direct leg, a null row (`aligned/`)

Block-aligned 1 MiB `direct=1` on an armed session rides the pre-existing
direct zc serve: `read_zc_serve_bytes` = 16 GiB = every byte on every leg,
cold AND warm, every tier counter 0, `read_zc_pool_serve_bytes` 0 — §2
finding 2 verbatim (the aligned cold population never becomes warm; at
12 GiB/s from zram no follower ever finds a fill in flight to cohort on).
The lever has no population here; recorded as the null it is.

### 7.2 Unaligned reads — the lever's population, sustained 30 s (`unaligned/`)

`offset=4096`: every 1 MiB read spans two blocks → the pooled-fill
slice-out arm cold, the tier arms warm. 16 readers × qd8 × 30 s per row.

| row | leg | GiB/s | clat mean | daemon CPU ms/GiB | `fuse3-ur` ms/GiB | `zc_serve` | `zc_pool_serve` (warm subset) | `copy_dest` | `copy_fill_slice` | `copy_hot`+`hold`+`cache` | fd-body replies / fallbacks |
|---|---|---|---|---|---|---|---|---|---|---|---|
| `r_cold` | A1 | **12.42** | 10.0 ms | **386** | **59** | 122.9 G | 156.6 G (48.6 G) | 93.2 G | 0 | 0 | 160,354 / 0 |
| | B1 | 10.42 | 12.0 ms | 507 | 95 | 107.7 G | 0 | 205.0 G | 86.2 G | 40.7 G | 0 / 0 |
| | B2 | 8.45 | 14.8 ms | 725 | 112 | 86.5 G | 0 | 167.3 G | 75.5 G | 28.3 G | 0 / 0 |
| | A2 | **8.73** | 14.3 ms | **559** | **70** | 98.2 G | 98.3 G (33.2 G) | 65.5 G | 0 | 0 | 100,617 / 0 |
| `r_warm` | A1 | **12.94** | 9.6 ms | **369** | **58** | 119.9 G | 171.5 G (51.3 G) | 97.1 G | 0 | 0 | 175,618 / 0 |
| | B1 | 8.99 | 13.9 ms | 650 | 107 | 97.6 G | 0 | 172.3 G | 75.3 G | 29.5 G | 0 / 0 |
| | B2 | 8.13 | 15.3 ms | 756 | 118 | 83.0 G | 0 | 161.1 G | 72.8 G | 27.3 G | 0 / 0 |
| | A2 | **8.45** | 14.8 ms | **566** | **69** | 89.3 G | 101.0 G (33.0 G) | 63.4 G | 0 | 0 | 103,384 / 0 |

Closure exact on every leg: `zc_serve + zc_pool_serve + copy_dest ≡ bytes
served` (A1 cold: 122.9 + 156.6 + 93.2 = 372.7 G vs fio 372.6 G); with
the lever on the fill-slice and the three warm-tier copies go to ZERO and
their bytes reappear as `read_zc_pool_serve_bytes` (the warm subset = the
former hot+hold+cache sum), `fuse3_zc_fd_body_fallbacks` = 0 across
440 k fd-source replies, `transport_fast_dispatch_serves` unchanged in
kind. The box heated through the bracket (load 3 → 19; 12.4 → 8.5 GiB/s
leg to leg with identical knobs on the B pair), which is why the per-byte
columns are the verdict and the reversed bracket the honest throughput read.

**Verdict — LANDS, default ON (flipped from the campaign's off).** Daemon
CPU per GiB **−24 % cold / −25 % warm** on the clean reversed bracket
(A2 vs B2: 559 vs 725, 566 vs 756) and −24 % / −43 % on the first; the
transport-worker class (`fuse3-ur`, where the serve memcpy lived) **−37 %
to −46 %**; throughput A ≥ B on all four pairs (+3 % / +4 % reversed,
+19 % / +44 % first — the latter inflated by thermal drift); clat A ≤ B
on all four. On this venue the device is not the bound, so the CPU/byte
column is the one the board's "whole-box CPU wall" names — and it is the
one that moved. The aligned direct leg is untouched (7.1). The zc-
capability gate ran on this code under both postures (§7.3).
### 7.3 zc-capability gate under both postures (this box, root)

`sudo tests/run_zc_capability_gate.sh` with `SQUEEZEFS_READ_ZC_SERVE=1`
and again with `=0` on the five-campaign stack: the zero-copy surface is
**green both times** — `fuse_zc_write_fusion` 5/5, `zc_bridge_cqe_wedge`
3/3, `zc_bridge_phase` 2/2, `bench_tests` 95/95, zcrx lib probes 38 ×3,
skip ledger EMPTY (`/tmp/five/zc-gate-{on,off}.log`). The bundled live
NVMe-reservation leg (`wero_rtype_tests::live_wero_…`, 1 of 6) failed
under BOTH postures in 0.06 s at its own `nvme connect`: `nvme nvme1:
creating 26 I/O queues … Connect command failed, errno: -18 … failed to
connect queue: 15` — a kernel-level fault of this box's current state
(the 1.2.1 fstests `-g auto` run's CPU-hotplug test, `generic/650`, left
CPUs 8, 10, 20, 22, 28, 30 firmware-latched offline at 22:48 on 09-04 —
"failed to report alive state" on every re-online attempt — so the box
has 26 of 32 CPUs; 4-queue connects (the dev substrate's) succeed, the
default 26-queue connects fail at queue 15; the multi-writer fleet's
identity connect fails the same way). It passed on this box at 10:52
today (1.2.1 gate, `zc-gate-local.log`) and is untouched by any of the
five levers (none reach `nvme connect`). Re-run after the reboot the
latched cores need; recorded, not adjudicated, here.

**Venue caveat for every row in §7:** all of today's brackets ran on the
26-CPU box — identical for both arms of each bracket, so every A/B stands;
the absolute numbers are 26-core numbers.

## 8. Next rungs (not built)

1. **Coverage**: a sparse memfd reserved to the hot-tier budget (slot
   bitmap + `fallocate(PUNCH_HOLE)` trim) so every hot entry is
   fd-addressable, not just the first `cores × 16` slots.
2. **Transform volumes**: decode into a pool slot so arm (d) loses D₂.
3. **NVMe read cache**: `READ_FIXED(segment fd)` with a post-CQE
   generation re-check (the mmap guard's law moved after the bridge).
4. **THP for the pool memfd** (`MADV_COLLAPSE`, the `thp.rs` precedent) —
   also for the bounce; both bridge from 4 KiB shmem folios today.
5. **Kernel item (optional, the op-count rung)**: a COMMIT-time
   user-VA reply source on zc queues (`commit.payload_addr`, the
   non-zc `fuse_uring_copy_from_ring` path run at COMMIT) would delete
   the bridge OP too (the worker park + CQE), not just the copy — but
   pays `copy_from_user` faults per page instead of a shmem folio walk.
   Not a prerequisite for anything above.
