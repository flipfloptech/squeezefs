# Rung 9 — the bracket harness and the venue's first SCOPING rows

Charter §11 ("Measurement law"), §13 (the bars), §10 (the ledgers), §7 (the
shim path), §8 (`bench`, the nvmet target). HEAD `ce0edf4` + this commit.

**Everything in this note is SCOPING.** The venue is the macOS `container`
VM (`7.2.6-sqz_aarch64_apple-vm_9c_virtio-blk`: 9 vCPU Apple-silicon guest,
16 GiB, one virtio disk that is a sparse image on the Mac's APFS), shared live
with two other agents' gates (load average 4–11 during these rows). None of
the numbers below is a bar; what this rung delivers is the HARNESS, the
ledgers closing (or being caught open), the noise band, and the defects the
harness surfaced on its first day. The acceptance procedure for the reference
box is `docs/gates/brackets.md`.

## 1. The harness: how a row is produced

`scripts/bracket/bracket.sh [--scoping] [--substrate S] [--lever binary|posture]
[--mount-opts O] <row> <A-binary> <B-binary>` (bash) + `scripts/bracket/brk.py`
(every number that is not the instrument's own). One invocation = one row:

1. **Identity.** `brk.py build <bin>` parses `squeezefs --version` (train,
   commit, profile, `-dirty`); `brk.py venue-id` composes
   `<kernel>_<arch>_<cpu>_<cores>c_<disk class>` from `uname`, `lscpu`,
   `/proc/meminfo`, `/sys/block`, `fuse.enable_uring`. Both ride the record's
   header with the instrument string, the substrate chain, the set geometry,
   the memory budget, the mount options, the leg timing and `loadavg`.
2. **Substrate** (`substrates/*.sh`), through the product's own verbs, zero
   mocks: `file` = sparse regular files on `/build`; `nvmet-loop` /
   `nvmet-tcp` = one meta + N data kernel-nvmet namespaces created by
   `squeezefs nvmeof share --subnqn … [--transport loop | --ip 127.0.0.1 --port P]
   --create-size …` and attached by `squeezefs nvmeof connect` (the fidelity
   tier's recipe), pid-tagged (`brk-<pid>-…`), own port-id slice (55000+), own
   share ledger dir, explicit host identity (the venue's `/etc/machine-id` is
   empty). The record names the chain: `nvmeXnY ← nvmet-{loop,tcp} ← sparse
   file on /dev/vdc`. The namespaces' `/proc/diskstats` rows are exclusive to
   the mount (exact); `/dev/vdc`'s are shared with everything on `/build`
   (an upper bound, said so).
3. **The aging store.** `squeezefs format [--single-writer] sqmeta://… sqdata://…`
   ONCE. `--lever posture` formats two sets (A symmetric default, B
   `--single-writer`) with one binary; `--lever binary` one set for two binaries.
4. **Legs A → B → B → A.** Each leg: `squeezefs mount --daemon --log-file …
   [-o opts] <uri> <mnt>` with the leg's binary (`SQUEEZEFS_MEM_BUDGET_MB`
   recorded; a shim row adds `-o interception` and runs the instrument under
   `LD_PRELOAD=<bin dir>/../lib/libsqueezefs_il.so`, `SQUEEZEFS_TEST_IPC_ALLOW_DIRTY=1`
   only for a `-dirty` build, recorded) → wait for `ready:` → the row's
   `row_prepare` on the FIRST leg of a set only (the dataset is laid down once
   and never recreated: a write leg overwrites the same inodes, a read leg reads
   the same bytes) → **warm-up** (`row_run … warmup`, default 10 s, output
   discarded) → `settle` (sync + wait for the reclaim ledger to quiesce) →
   snapshot `.stats` + `/proc/diskstats` + `loadavg` → **measured span**
   (default 60 s; shorter refused without `--scoping`) → `settle` → snapshot →
   `row_metrics` → `brk.py leg` → `row_cleanup` (outside the record) →
   `squeezefs umount` → wait for the daemon pid (from `.stats` `mount.pid`,
   the only process the harness may ever kill) → the daemon log copied beside
   the record.
5. **The leg record** (`brk.py leg`): the instrument's numbers (fio JSON:
   ops, bytes, MiB/s, IOPS, clat p50/p99/p99.9/max, fsync latency; mdtest's
   `File creation` rate; `renames.py`'s entries/op; tar's verbs/entry), and over
   the (before, after) `.stats` pair the ledgers:
   `device bytes ÷ user bytes` from the daemon's device-write arms
   (`overlay_store_bytes + patch_write_bytes + own_block_dma_bytes +
   durable_upload_bytes_{escalation,writeback}`; `rewrite_device_write_bytes`
   reported beside — see finding F9) AND from diskstats per data / meta
   namespace; `wareq-sz` from diskstats `sectors ÷ writes` and from the
   daemon's request counters (`put_obj + patch_writes + own_block_dmas`);
   the reclaim ledger `block_free_reclaim_queued + _elided ≡ terminal_frees`
   (delta and absolute at quiesce); the admission ledger
   `Σ admit_write_{overlay,inplace,accumulate,inline}_bytes ≡ user bytes`;
   the read copy ledger `Σ {read_copy_*, read_dest_dma_bytes, read_zc_serve_bytes,
   read_dest_lease_bytes, transport_zc_serve_bytes, transport_zc_bounce_bytes_bridged}
   ≈ user bytes` (F10); the solo mount's wire census
   (`cluster_wire_frames_sent + joined_wire_verbs + xv_cross_owner_steps_shipped +
   slot_resolve_rpcs + xv_cross_owner_guard_rpcs + dlm_token_grants + slot_ships +
   free_shipped_blocks` — the port has no single `dlm_rpcs`; this sum is its
   meaning); `invariant_tripwires` and nine other must-stay-0s; nine tick-rescue
   counters; the IPC family (`ipc_ops_*`, `ipc_lane_gate_kernel_routes`,
   admission refusals); the journal family; the transport family; the
   `fsync_phase_ns` family (count + sum per phase, exact-sum checked);
   `daemon_cpu_ns`; the posture (`direct_device_true`, `writer_guard_mode`,
   `symmetric_meta`, budget); `absent_keys` (a renamed counter can never zero
   a ledger silently).
6. **Verdicts per leg** (any false ⇒ the leg is INVALID with the reason, the
   row is INVALID, nothing is dropped): `instrument_ok` (rc 0, no I/O errors;
   the daemon's `answered EAGAIN` count beside), `must_stay_0`, `reclaim_ledger`,
   `admission_ledger` (write rows, ±0.1 %), `engagement_fuse`
   (`Δtransport_ring_requests ≥ ops` for direct rows — handler-lane `fuse_ops`
   does not see a zero-copy direct-leg read, F10) / `engagement_shim`
   (`Δ(ipc_ops_read + ipc_ops_write) + gate routes ≥ ops` and ≥ 1 session; a
   1 MiB op is above the hybrid lane gate and is judged on the kernel lane),
   `fsync_exact_sum`, `solo_wire_rpcs_0`, `mem_budget_green` (a leg that ran
   Yellow/Red sheds and parks: not a number), `read_copy_ledger` (±2 %),
   `daemon_alive` (a daemon dead mid-leg — the venue's other agents sweep
   `pkill -9 -x squeezefs` — is recorded and the bracket remounts the same store
   for the next leg).
7. **The fold** (`brk.py fold`): per arm mean / spread / min / max of the
   row's headline; `noise_band_pct` when A = B (the spread over four legs of
   the same binary IS the venue's noise band for that row); `a_over_b`
   otherwise; `invalid_reasons`. Output:
   `bench/brackets/<venue-id>/<row>-<date>.{json,md,log}` + the legs' daemon
   logs; `brk.py summary <dir>` renders the table in §4.
8. **Teardown**: `umount` (lazy-detach if the daemon is dead), `nvmeof
   disconnect` + `unshare` by manifest, then a residue sweep (adopt + unshare
   anything still pid-tagged ours), scratch removed; `scripts/venue trim` by
   hand after the matrix.

Instruments (`scripts/bracket/jobs/*.fio`, `rows/*.sh`): fio 3.36 `seq-1m.fio`
(1 MiB, QD32 × N, direct 0/1, libaio), `rand-4k.fio` (4 KiB, QD32 × N, direct,
norandommap), `fsync-4k.fio` (psync 4 KiB buffered + `fsync=1`),
`prepare-seq.fio` (the dataset, once); mdtest `-C -F -u -z 2 -b 8` (73
directories per task; the item count sized from the warm-up's rate because
mdtest's stonewall refuses a branching tree, F11); `renames.py` (rounds of
untimed creates → timed rename+unlink, Δ`meta_kv_journal_entries` ÷ ops over
the timed spans alone); `tar -x` of a 500 × 20 synthetic tree (1–4 KiB files)
repeated for the span, N extractions in flight; `io500.sh` (one io500 run
with the datadir on a mount, the ledgers around it). Shim rows use psync
(F3). Provenance: fio 3.36 (Ubuntu 24.04), OpenMPI 4.1.6, IOR/mdtest
`io500-sc19-605-g5528b36` (= io500's pinned `5528b3619236d475`), io500
`io500-sc26_v1` (34a596582715934d2f7695302a7c51e8f7e73d9a) with pfind
27427343bb66f525b93d225d1f124cbed440d0b5, gcc 13.3, built into
`/build/tools/{bin,src}` from the sources on the external disk
(`/build/ext/{ior,io500}`; io500's pinned IOR fetched from github).

## 2. The venue and its noise band

`7.2.6-sqz_aarch64_apple-vm_9c_virtio-blk`: kernel 7.2.6-sqz (fuse
`enable_uring=Y`, kmbuf + zero-copy + payload retention probed Present —
every mount here ran a `zero_copy` session, 9 queues × depth 28), aarch64
Apple (vendor 0x61, no model string) under the macOS `container` hypervisor,
9 vCPU, 16 GiB, `/build` = ext4 on `/dev/vdc` (virtio-blk, a sparse APFS
image: ~350–450 MiB/s sustained O_DIRECT writes, reads from the Mac's cache),
no NUMA, one virtio-net queue. Not a measurement venue by AGENTS.md; two other
agents ran gates, LTP and pjdfstest throughout (`loadavg` 4–11 on 9 CPUs,
recorded per leg).

Noise band = the spread over four legs of A = B = HEAD, per row (§4, column
"noise band"): **5 % on the quietest fabric row (seq-read-1m-direct, nvmet-tcp)
to 20–35 % on the IOPS and buffered rows while the other agents' suites
ran.** A venue whose own noise is 20 % cannot judge a 5 % A/B; the box's
noise band has to be measured first (gate §1 item 8).

The substrate chain hides the device entirely for reads: the nvmet file
backend and the file substrate both serve reads from the VM's page cache
(`seq-read-1m-direct` 8 GB/s through nvme-tcp loopback on a 4 GiB dataset;
the `file` substrate read a 512 MiB dataset at 18 GB/s in a probe). Every
read row here measures the transport + the TCP loopback copy, not a device.
Writes do reach `/dev/vdc` (`device ÷ user` from the namespaces' diskstats
≈ 1.0 on the valid write rows).

## 3. Scoping rows

@@TABLE@@

Every record (JSON, md, harness log, per-leg daemon logs) is under
`bench/brackets/7.2.6-sqz_aarch64_apple-vm_9c_virtio-blk/`; the io500 run
under `io500-<date>/` there; the wedge evidence under
`wedge-seq-write-1m-buffered-20261010T1232Z/`.

@@IO500@@

## 4. Findings (what the harness caught on its first day)

F1. **Commit conveyor wedge** (product, first-class — charter §1.1).
`seq-write-1m-buffered` on nvmet-tcp (libaio QD32 × 4, 1 MiB buffered
writes): the WARM-UP never returned. The FUSE op watchdog reported 4 writes
"in flight ≥ 75 s, parked past station `meta_backend_start`"; `.stats`:
`meta_conveyor_queued 6`, `meta_commit_parked 6`, `meta_conveyor_windows_inflight 0`,
`meta_conveyor_park_tick_rescues 0`, `journal_lane_parks 23005`; every daemon
thread (`sqz-jrnl0`, `sqz-meta0/1`, the lanes) parked in `io_cqring_wait`;
fio's threads in `request_wait_answer`; `invariant_tripwires 0`. Six
transactions enqueued on the conveyor with no leader pass running and no
window in flight = a lost wake / leader-exit race in the conveyor's enqueue
→ pass handoff (`src/meta/conveyor.zig`), under 128 outstanding 1 MiB
buffered writes. Evidence: `wedge-seq-write-1m-buffered-20261010T1232Z/`
(full `.stats`, daemon log with the watchdog's 880 overdue reports, every
task's kernel stack, fio's stacks). Needs a red-first repro in the conveyor
suite; the model check of the conveyor handoff should be revisited for the
weakening this shape exercises. The row stays un-run.

F2. **EAGAIN on a blocking write(2)** (product). `seq-write-1m-direct`
(libaio QD32 × 4) and `shim-seq-write-1m` on nvmet-tcp: fio aborts with
`EAGAIN` (104–128 per leg; 4 per leg on the shim row). The daemon:
`handler: write … answered EAGAIN: WouldBlock` — `write_admit.zig`'s
accumulation checkout at the pool cap ("backpressure (EAGAIN-class), never a
lost byte", `parked_gate_waits`). A blocking write on a regular file may not
return EAGAIN (POSIX reserves it for O_NONBLOCK); the admission must park
under the write-pipeline depth governor instead of refusing. All four legs of
both rows INVALID by `instrument_ok`; the numbers that fio did produce before
aborting are in the records, not in the table.

F3. **The shim does not intercept fio's libaio engine** (product).
`LD_DEBUG=bindings` shows fio binding `io_submit@LIBAIO_0.1` /
`io_getevents@LIBAIO_0.4` to `libsqueezefs_il.so`, yet `ipc_ops_*` stay 0:
fio creates its context with `io_queue_init()`, which libaio resolves
internally to its own `io_setup` — the shim never sees the context and its
`io_submit` passes an unknown context through. The engagement law reports it
(INVALID). Fix: interpose `io_queue_init` / `io_queue_release` (and treat any
unknown context as a passthrough that is COUNTED). The shim rows here use
psync (QD1 × N) and say so.

F4. **IL admission cap vs. a bounded budget** (sizing law). The daemon admits
IL sessions up to `budget/8` at a 64 MiB arena each; at `SQUEEZEFS_MEM_BUDGET_MB=2048`
only 3 sessions admit and fio's second process is refused ("session budget
exhausted (class 5)") and silently falls back to kernel FUSE — the engagement
law caught it (72 % of ops through the ring). On the box: 32 processes ×
2 sessions × 64 MiB = 4 GiB of arenas needs a ≥ 32 GiB budget, or the cap
must derive from the population.

F5. **The budget's unreclaimable floor** (sizing law). On this geometry
(9 queues × 32 MiB kmbuf + payload arenas + per-lane read slabs + fiber
stacks) `mem_budget_unreclaimable_bytes` ≈ 2.7 GiB: a 2 GiB budget runs Red
from its first tick ("HARD BACKSTOP … Red sheds escalate to component
floors"). The harness marks any leg not Green INVALID (`mem_budget_green`)
and defaults to 6 GiB; the 4 GiB read dataset still took
`seq-read-1m-buffered` to Yellow on its last leg (INVALID, recorded).

F6. **`journal_lane_park_tick_rescues` under write saturation** (product,
must-stay-≈0). 6–186 per 60 s leg on every saturated write row
(`seq-write-1m-direct` legs: 174/128/18/6; the shim write row 3–10; a read
row's umount-side flush 5), plus one `lock_ticked_reregisters`. The journal
lane's 2 s backstop found work a wake should have delivered: a lost-wake
class in the conveyor → journal-lane handoff, kin to F1.

F7. `transport_lease_overlong` 128–1412 per saturated write leg: FUSE_WRITE
payload leases ≥ 1 s because 128 MiB are queued ahead of a ~400 MiB/s
substrate (p50 196 ms, p99 1.7 s on the file substrate). Reporting, not a
defect, on this substrate.

F8. **SafeAllocator leak at exit** (product, small): 144 B from
`src/ops/nvmet/initiator.zig:875` (`sample`: `prev.live = cur.live` drops the
previous controller list) on every mount over nvme namespaces once `.stats`
has been rendered — one `error(` line per daemon lifetime on every nvmet row.

F9. **`rewrite_device_write_bytes` is not a second device write**
(accounting). On a 22.28 GB overwrite row: `overlay_store_bytes` 22.17 GB +
`rewrite_device_write_bytes` 6.09 GB vs diskstats 1.025 × user. The rewrite
epoch counts the same overlay DMA it was fed. `src/cli/bench.zig`'s
`device_write_keys` sums both and over-reports amplification (1.27 here);
the harness reports it beside. bench.zig should follow.

F10. **Reads on a zero-copy session never reach a handler lane**
(accounting). An O_DIRECT READ the warm ladder cannot answer is served at the
delivery CQE by the direct device leg (`transport_zc_direct_legs`,
`transport_zc_serve_bytes` = user bytes exactly); `fuse_ops` does not move,
nor do the data plane's `read_copy_*` arms. An engagement or copy ledger
built on `fuse_ops` / `read_copy_*` alone (as bench.zig's) reads 0 for every
read row; the harness uses `transport_ring_requests` and adds the transport's
serve arms.

F11. mdtest's `-W` stonewall "does only work with a branch factor ≤ 1": a
many-dirs creates row cannot be time-boxed by the instrument; the harness
sizes the count from the warm-up's rate (elapsed recorded).

F12. The file substrate's data volumes are opened buffered ("write cache
file-backed"): reads come from the VM page cache (18 GB/s); the nvmet file
backend likewise on the target side. The venue has no device-true read path;
`-o direct_device_true` here rules only the daemon's tiers out, not the
substrate's cache.

F13. Other agents' `pkill -9 -x squeezefs` sweeps kill this harness's daemons
and its CLI teardown verbs at random; the harness survives it (dead leg
INVALID, lazy-detach, residue sweep) but a shared venue cannot be a quiet one.

## 5. Gaps — what cannot be measured here and what the box run needs

- **Every bar.** 9 vCPU, one virtio disk, loopback TCP on one NIC queue: the
  venue's valid rows sit two to three orders of magnitude under the bars and
  carry a 5–35 % noise band. The bars need the 32-core client, five
  memory-backed nvme-tcp targets on a real NIC, the sqz kernel, the `dist`
  profile, a quiet box, and 16 GiB+ datasets (`BRK_FILE_SIZE=4g`,
  `--numjobs 32`, the default budget): `docs/gates/brackets.md` §2–3.
- **Reads** have no device behind them on this venue (F12); `device-true`
  read rows exist only on the box.
- **The shim's data-path bars** (942 k / 727 k IOPS) need libaio engagement
  (F3) and a population-aware IL admission cap (F4) before the box run can
  even be VALID at 32 processes.
- **The bandwidth bars** need F1 and F2 fixed first: at QD32 × 4 the write
  path either wedges (buffered) or refuses (direct). The box run at QD32 × 32
  will hit both harder.
- **The posture row** compares a solo symmetric mount with `--single-writer`
  at ~4.5 k creates/s; "within noise" at a 10–20 % noise band is a weak
  statement — the box's quiet band decides it.
- **io500** ran the bounded `io500-scoping.ini` (60 s stonewall, capped
  sizes — never submittable); the box runs `config-minimal.ini` unmodified.
- **A-B with A ≠ B**: every row here is A = B (the noise floor). The first
  real A/B is the fix for F1/F2 against HEAD, through this harness.

## 6. Owed

- Red-first repros for F1 (conveyor wedge), F2 (EAGAIN to a blocking write),
  F6 (journal lane tick rescues), F3 (shim `io_queue_init`), F8 (leak);
  bench.zig's device-write arms (F9) and read engagement (F10) aligned with
  `brk.py`'s.
- `brk.py`'s ledger term list is a second registry of `.stats` names; a rail
  that checks every name in `brk.ALL_COUNTERS` against the stats schema (the
  way `tests/rails.zig` checks knobs) would turn a renamed counter into a red
  gate instead of an `absent_keys` entry.
- The harness runs the A/B over binaries and over the format posture; the
  lever over a `SQUEEZEFS_*` A/B knob (same binary, knob on/off) is the next
  lever to add.
