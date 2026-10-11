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
with the datadir on a mount, the ledgers around it). Shim rows used psync
on this first day (F3; libaio since `613b1c6`). Provenance: fio 3.36 (Ubuntu 24.04), OpenMPI 4.1.6, IOR/mdtest
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

Noise band = the spread over four legs of A = B = `ce0edf4`, per row (§3,
column "noise band"): **0.8 % on the one row that ran while the venue was
briefly quiet (`rand-4k-write` on `file`), 15–30 % on the IOPS, mdtest and
direct-read rows, 45–112 % on the shim and buffered rows while the other
agents' suites ran** (`loadavg` 8–20). A venue whose own noise is 20 %
cannot judge a 5 % A/B; the box's noise band has to be measured first (gate
§1 item 8).

The substrate chain hides the device entirely for reads: the nvmet file
backend and the file substrate both serve reads from the VM's page cache
(`seq-read-1m-direct` 8 GB/s through nvme-tcp loopback on a 4 GiB dataset;
the `file` substrate read a 512 MiB dataset at 18 GB/s in a probe). Every
read row here measures the transport + the TCP loopback copy, not a device.
Writes do reach `/dev/vdc` (`device ÷ user` from the namespaces' diskstats
≈ 1.0 on the valid write rows).

## 3. Scoping rows

All rows: A = B = `ce0edf4f004d` (gate profile; sha256 `3429843f…9b94`, shim
`732e26b8…4f48`, snapshotted into the scratch per row), 4 legs × 60 s + 10 s
warm-up, `numjobs 4`, `iodepth 32`, 4 × 512 MiB files (2 GiB datasets),
`SQUEEZEFS_MEM_BUDGET_MB=7168`, sets of 2 GiB meta + 2 × 6 GiB data (sparse),
2026-10-10 14:27–17:54 UTC, `loadavg` 4–20 on 9 CPUs (two other agents' gates,
LTP, pjdfstest and builds ran throughout). Headline = the A arm's mean over
its two legs; "noise band" = the spread over all four legs of the same
binary. Every INVALID row keeps its numbers in the record; the table's
headline column shows them only where the leg produced one.

(The venue's bars, for orientation, not judgement: the 4-task mdtest row sits
at 13 % of the 32 k creates/s bar; the kernel-FUSE rand-4k rows at 5 % of
555 k IOPS; the shim rand-4k rows at 1–2 % of the 942 k / 727 k bars; the
seq rows at 1–20 % of the GB/s bars — on 9 vCPU, loopback TCP and one virtio
disk, as expected.)

| row | substrate | mount / lever | instrument | legs (A,B,B,A) | A mean | noise band | p50 / p99 µs | device÷user stats / disk | wareq-sz disk B | reclaim q+e=t | admission ×user | wire rpcs | tripwires | tick rescues | verdict | record |
|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|
| creates-mdtest | nvmet-loop | defaults | mdtest | 3719, 3906, 3488, 4623 | **4171 creates/s** | 28.9 % | — / — | — / — | — | 0+0=0 | — | 0 | 0 | 0 | SCOPING-VALID | `creates-mdtest-20261010T160749Z.json` |
| creates-mdtest | nvmet-loop | lever posture (A symmetric / B single-writer) | mdtest | 4599, 4736, 4783, 4080 | **4339 creates/s** | 15.5 % | — / — | — / — | — | 0+0=0 | — | 0 | 0 | 0 | SCOPING-VALID | `creates-mdtest-20261010T164606Z.json` |
| fsync-4k | nvmet-loop | defaults | fio-3.36 psync randwrite bs=4k direct=0 QD1×numjobs=4 size=2 | 999.2, 2636, 1591, 2508 | **1754 IOPS** | 84.7 % | 982.0 / 6398 | 1.022 / 1.022 | 2093 | 0+10=10 | ×1.0000 | 0 | 0 | 6 | SCOPING-VALID | `fsync-4k-20261010T160230Z.json` |
| rand-4k-read | nvmet-loop | defaults | fio-3.36 libaio randread bs=4k direct=1 iodepth=32 numjobs=4 | 22226, 24088, 23362, 29415 | **25820 IOPS** | 29.0 % | 4420 / 14909 | — / — | — | 0+0=0 | — | 0 | 0 | 0 | SCOPING-VALID | `rand-4k-read-20261010T153505Z.json` |
| rand-4k-read | nvmet-loop | direct_device_true | fio-3.36 libaio randread bs=4k direct=1 iodepth=32 numjobs=4 | 30538, 29067, 27079, 26279 | **28408 IOPS** | 15.1 % | 4108 / 12976 | — / — | — | 0+0=0 | — | 0 | 0 | 0 | SCOPING-VALID | `rand-4k-read-20261010T154023Z.json` |
| rand-4k-write | nvmet-loop | defaults | fio-3.36 libaio randwrite bs=4k direct=1 iodepth=32 numjobs= | 4139, 0, 5260, 1890 | **3014 IOPS** | 89.6 % | 36176 / 127052 | 1.017 / 1.023 | 4192 | 0+13=13 | ×1.0000 | 0 | 0 | 3 | SCOPING-INVALID | `rand-4k-write-20261010T154534Z.json` |
| rand-4k-write | file | defaults | fio-3.36 libaio randwrite bs=4k direct=1 iodepth=32 numjobs= | 7359, 7399, 7343, 7342 | **7351 IOPS** | 0.8 % | 15466 / 25952 | 1.035 / 1.159 | 3545 | 77+0=77 | ×1.0000 | 0 | 0 | 0 | SCOPING-VALID | `rand-4k-write-20261010T170604Z.json` |
| rename-unlink | nvmet-loop | defaults | renames.py pool=20000 threads=4 | 1.012, 1.012, 1.012, 1.013 | **1.012 journal entries/op** | 0.1 % | — / — | — / — | — | 0+0=0 | — | 0 | 0 | 0 | SCOPING-INVALID | `rename-unlink-20261010T162255Z.json` |
| seq-read-1m-buffered | nvmet-tcp | defaults | fio-3.36 libaio read bs=1m direct=0 iodepth=32 numjobs=4 siz | 1419, 2984, 2930, 2141 | **1780 MiB/s** | 66.1 % | 48103 / 232129 | — / — | — | 0+0=0 | — | 0 | 0 | 15 | SCOPING-VALID | `seq-read-1m-buffered-20261010T151933Z.json` |
| seq-read-1m-direct | nvmet-tcp | defaults | fio-3.36 libaio read bs=1m direct=1 iodepth=32 numjobs=4 siz | 7955, 8529, 7262, 7799 | **7877 MiB/s** | 16.1 % | 14746 / 41943 | — / — | — | 0+0=0 | — | 0 | 0 | 0 | SCOPING-INVALID | `seq-read-1m-direct-20261010T143010Z.json` |
| seq-write-1m-buffered | nvmet-tcp | defaults | fio-3.36 libaio write bs=1m direct=0 iodepth=32 numjobs=4 si | 0, 0, 406.4, 0 | **0.000 —** | 300.0 % | — / — | 0.997 / 1.006 | 1060241 | 0+19324=19324 | ×1.0000 | 0 | 0 | 27 | SCOPING-INVALID | `seq-write-1m-buffered-20261010T143514Z.json` |
| seq-write-1m-direct | nvmet-tcp | defaults | fio-3.36 libaio write bs=1m direct=1 iodepth=32 numjobs=4 si | 0, 0, 0, 0 | **0.000 —** | 0.0 % | — / — | — / — | — | 0+17099=17099 | — | 0 | 0 | 83 | SCOPING-INVALID | `seq-write-1m-direct-20261010T142713Z.json` |
| seq-write-1m-direct | file | defaults | fio-3.36 libaio write bs=1m direct=1 iodepth=32 numjobs=4 si | 0, 0, 0, 0 | **0.000 —** | 0.0 % | — / — | — / — | — | 29425+0=29425 | — | 0 | 0 | 139 | SCOPING-INVALID | `seq-write-1m-direct-20261010T170201Z.json` |
| shim-rand-4k-read | nvmet-loop | defaults | fio-3.36 psync randread bs=4k direct=1 QD1×numjobs=4 size=51 | 13619, 13332, 11051, 7761 | **10690 IOPS** | 51.2 % | 294.9 / 1853 | — / — | — | 0+0=0 | — | 0 | 0 | 0 | SCOPING-VALID | `shim-rand-4k-read-20261010T174908Z.json` |
| shim-rand-4k-write | nvmet-loop | defaults | fio-3.36 psync randwrite bs=4k direct=1 QD1×numjobs=4 size=5 | 4514, 13950, 13932, 3786 | **4150 IOPS** | 112.4 % | 284.7 / 6181 | 1.048 / 1.053 | 4328 | 0+183=183 | ×1.0000 | 0 | 0 | 3 | SCOPING-VALID | `shim-rand-4k-write-20261010T155710Z.json` |
| shim-seq-read-1m | nvmet-tcp | defaults | fio-3.36 psync read bs=1m direct=1 QD1×numjobs=4 size=512m/j | 6187, 3958, 5232, 4646 | **5416 MiB/s** | 44.5 % | 741.4 / 2466 | — / — | — | 0+0=0 | — | 0 | 0 | 1 | SCOPING-VALID | `shim-seq-read-1m-20261010T152947Z.json` |
| shim-seq-write-1m | nvmet-tcp | defaults | fio-3.36 psync write bs=1m direct=1 QD1×numjobs=4 size=512m/ | 308.4, 523.1, 473.1, 537.7 | **423.1 MiB/s** | 49.8 % | 3422 / 139821 | 0.969 / 0.977 | 1057584 | 0+24967=24967 | ×1.0000 | 0 | 0 | 646 | SCOPING-INVALID | `shim-seq-write-1m-20261010T152439Z.json` |
| tar-x | nvmet-loop | defaults | tar -x of a 500×20 synthetic tree | 0.000, 0.000, 0.000, 0 | **0.000 —** | 0.0 % | — / — | — / — | — | 0+0=0 | — | 0 | 0 | 0 | SCOPING-INVALID | `tar-x-20261010T163116Z.json` |

INVALID legs and their reasons (from the records):

- `rand-4k-write` leg 2/B INVALID `daemon_wedged`: instrument outlived its bound after 140 s; fuse_ops_inflight 128, fuse_op_watchdog_overdue 3456, meta_conveyor_queued 5, meta_commit_parked 5; daemon pid 611535 killed by the harness; evidence: warmup.wedge.*
- `rename-unlink` leg 1/A INVALID `mem_budget_green`: mem_budget_level 2 at the after-snapshot (0 = Green; budget 7168 MiB; top components exec_fiber_stacks 828 MiB, transport_zc_bounce 288 MiB, transport_kmbuf_region 288 MiB, block_alloc_table 0 MiB)
- `seq-read-1m-direct` leg 1/A INVALID `mem_budget_green`: mem_budget_level 2 at the after-snapshot (0 = Green; budget 7168 MiB; top components exec_fiber_stacks 703 MiB, transport_zc_bounce 288 MiB, transport_kmbuf_region 288 MiB, block_alloc_table 0 MiB)
- `seq-write-1m-buffered` leg 1/A INVALID `instrument_ok`: instrument rc 3, 96 I/O errors (fio io_u errors (see measure.log)); daemon log: 96 `answered EAGAIN` line(s)
- `seq-write-1m-buffered` leg 2/B INVALID `instrument_ok`: instrument rc 4, 128 I/O errors (fio io_u errors (see measure.log)); daemon log: 128 `answered EAGAIN` line(s)
- `seq-write-1m-buffered` leg 4/A INVALID `daemon_alive`: daemon pid 454025 died during the measured span (instrument rc 4) (see the leg's daemon.log and dmesg.tail; the venue's other agents run `pkill -9 -x squeezefs` sweeps)
- `seq-write-1m-direct` leg 1/A INVALID `instrument_ok`: instrument rc 4, 106 I/O errors (fio io_u errors (see measure.log)); daemon log: 106 `answered EAGAIN` line(s)
- `seq-write-1m-direct` leg 1/A INVALID `mem_budget_green`: mem_budget_level 2 at the after-snapshot (0 = Green; budget 7168 MiB; top components exec_fiber_stacks 717 MiB, transport_zc_bounce 288 MiB, transport_kmbuf_region 288 MiB, block_alloc_table 0 MiB)
- `seq-write-1m-direct` leg 2/B INVALID `instrument_ok`: instrument rc 4, 128 I/O errors (fio io_u errors (see measure.log)); daemon log: 128 `answered EAGAIN` line(s)
- `seq-write-1m-direct` leg 2/B INVALID `mem_budget_green`: mem_budget_level 2 at the after-snapshot (0 = Green; budget 7168 MiB; top components exec_fiber_stacks 718 MiB, transport_zc_bounce 288 MiB, transport_kmbuf_region 288 MiB, block_alloc_table 0 MiB)
- `seq-write-1m-direct` leg 3/B INVALID `instrument_ok`: instrument rc 4, 126 I/O errors (fio io_u errors (see measure.log)); daemon log: 126 `answered EAGAIN` line(s)
- `seq-write-1m-direct` leg 3/B INVALID `mem_budget_green`: mem_budget_level 2 at the after-snapshot (0 = Green; budget 7168 MiB; top components exec_fiber_stacks 725 MiB, transport_zc_bounce 288 MiB, transport_kmbuf_region 288 MiB, block_alloc_table 0 MiB)
- `seq-write-1m-direct` leg 4/A INVALID `instrument_ok`: instrument rc 4, 128 I/O errors (fio io_u errors (see measure.log)); daemon log: 128 `answered EAGAIN` line(s)
- `seq-write-1m-direct` leg 4/A INVALID `mem_budget_green`: mem_budget_level 2 at the after-snapshot (0 = Green; budget 7168 MiB; top components exec_fiber_stacks 718 MiB, transport_zc_bounce 288 MiB, transport_kmbuf_region 288 MiB, block_alloc_table 0 MiB)
- `seq-write-1m-direct` leg 1/A INVALID `instrument_ok`: instrument rc 4, 100 I/O errors (fio io_u errors (see measure.log)); daemon log: 100 `answered EAGAIN` line(s)
- `seq-write-1m-direct` leg 2/B INVALID `instrument_ok`: instrument rc 4, 128 I/O errors (fio io_u errors (see measure.log)); daemon log: 128 `answered EAGAIN` line(s)
- `seq-write-1m-direct` leg 3/B INVALID `instrument_ok`: instrument rc 4, 108 I/O errors (fio io_u errors (see measure.log)); daemon log: 108 `answered EAGAIN` line(s)
- `seq-write-1m-direct` leg 4/A INVALID `instrument_ok`: instrument rc 4, 128 I/O errors (fio io_u errors (see measure.log)); daemon log: 128 `answered EAGAIN` line(s)
- `shim-seq-write-1m` leg 2/B INVALID `instrument_ok`: instrument rc 4, 4 I/O errors (fio job error codes [11]); daemon log: 4 `answered EAGAIN` line(s)
- `shim-seq-write-1m` leg 4/A INVALID `instrument_ok`: instrument rc 4, 4 I/O errors (fio job error codes [11]); daemon log: 4 `answered EAGAIN` line(s)
- `tar-x` leg 2/B INVALID `mem_budget_green`: mem_budget_level 1 at the after-snapshot (0 = Green; budget 7168 MiB; top components exec_fiber_stacks 894 MiB, transport_zc_bounce 288 MiB, transport_kmbuf_region 288 MiB, block_alloc_table 0 MiB)
- `tar-x` leg 4/A INVALID `daemon_wedged`: instrument outlived its bound after 240 s; fuse_ops_inflight 4, fuse_op_watchdog_overdue 172, meta_conveyor_queued 5, meta_commit_parked 5; daemon pid 707670 killed by the harness; evidence: measure.wedge.*

Per-row notes (from the records):

- **rename-unlink**: 1.012 journal entries per rename/unlink op on every leg
  (bar ≤ 1.05 — met at this venue's resolution; PAD entries 0.85/op beside, the
  sector-pad law's framing), 2.6–3.6 k ops/s; the row is INVALID only because
  leg 1 ran Red (F5).
- **tar-x**: 0 wire verbs per entry on every live leg (bar met; the solo wire
  census is 0 everywhere), 1.0–1.2 k entries/s, 5.21 journal entries per tar
  entry (create + write + the setattr pair + the parent's Δtime); leg 2 Yellow,
  leg 4 WEDGED (F1).
- **creates-mdtest**: 3.5–4.8 k creates/s with 4 MPI tasks × 73 dirs, 215–370 k
  files per leg, journal entries ≈ 1.01 per create; Green on every leg.
- **posture (`--lever posture`)**: A (symmetric default) 4 599 / 4 080 vs B
  (`--single-writer`) 4 736 / 4 783 creates/s → A/B 0.91 inside the row's
  15–29 % noise band; `symmetric_meta` 1 vs 0 confirms the postures,
  `writer_guard_mode` `flock+pr` on both (PR armed on the nvmet namespaces),
  wire census 0 on all four legs. The bar ("within noise, `dlm_rpcs == 0`")
  holds at this resolution; the box's band decides it.
- **fsync-4k**: 60 k fsyncs per leg, `Σ phases ≡ total` to the ns
  (80 548 290 054 = 80 548 290 054 ns); mean 1 343 µs = data_barrier 824 +
  meta_barrier 517 + 2 µs of everything else; fio's own fsync latency mean
  1.58 ms, p99 6.9 ms; `device ÷ user` 1.022 (stats = diskstats); reclaim
  0 + 10 = 10.
- **rand-4k-write (nvmet-loop, W1 patches)**: `wareq-sz` 4 096–4 279 B on the
  data namespaces = exactly one 4 KiB device write per 4 KiB user write
  (`device ÷ user` data 1.00–1.04), and **0.73–0.75 × user bytes on the META
  namespace**: the journal's sector-pad law lands ≈ 3 KiB of journal page per
  4 KiB patch (one 4 KiB-aligned run per conveyor window). On the box's 727 k
  IOPS shape that is ≈ 2.2 GB/s of metadata-device writes (F14). Reclaim
  0 + 13 = 13 closes; leg 2 WEDGED (F1).
- **rand-4k-write (file)**: the quietest row of the day (noise 0.8 %) at
  7.3 k IOPS; `device ÷ user` 1.13–1.21 on the SHARED /dev/vdc (an upper
  bound); 77 terminal frees all queued (file substrate: punches, no elision).
- **shim rows**: `ipc_ops_{read,write}` ≡ fio's ops on every leg (100 %
  engagement, 4 sessions); the read copy ledger closes at ×1.0001 through
  `read_dest_dma_bytes` (the handoff DMAs into the arena); p50 285–300 µs.
  The shim write row's 4.5 k / 13.9 k / 13.9 k / 3.8 k legs (noise 112 %) are
  the venue's load swings, not the daemon's: `loadavg` 8–20 during the row.
- **rand-4k-read**: hybrid 25.8 k vs the `direct_device_true` ruler 28.4 k IOPS
  (the ruler is faster here: no tier probe before a cache that has nothing;
  both inside the 15–29 % band); `Δtransport_ring_requests` ≡ ops, every read
  served by the direct device leg (`transport_zc_direct_legs`), `fuse_ops` ≈ 0.
- **seq-read-1m-direct**: 7.3–8.5 GB/s through nvme-tcp loopback from the
  target's page cache (F12); INVALID only by leg 1's Red (F5).
- **seq-write rows**: every leg INVALID — EAGAIN storms (F2) and the wedge
  (F1); the bytes they did move closed their ledgers (admission ×1.0000,
  `device ÷ user` 0.97–1.01 on the data namespaces, `wareq-sz` 1 057–1 060 KB
  = the 1 MiB block).


Every record (JSON, md, harness log, per-leg daemon logs) is under
`bench/brackets/7.2.6-sqz_aarch64_apple-vm_9c_virtio-blk/`; the io500 run
under `io500-<date>/` there; the wedge evidence under
`wedge-seq-write-1m-buffered-20261010T1232Z/`.

### io500

`scripts/bracket/io500.sh --scoping --substrate nvmet-tcp --budget-mb 7168`
(`io500-scoping.ini`: the distribution's `config-minimal.ini` + a 60 s
stonewall and capped sizes, np 4) **wedged in its first phase, ior-easy-write**
(4 ranks × 2 MiB transfers): `meta_conveyor_queued 7`, `meta_commit_parked 7`,
no window in flight, 4 FUSE writes overdue for 36 minutes — the fifth
occurrence of F1. Evidence: `wedge-io500-ior-easy-write-20261010T1711Z/`
(`.stats`, daemon log, task stacks, io500's config). No summary exists. The
pipeline itself was validated on a 5 s / 64 MiB configuration against a file
substrate (every phase ran, `[SCORE] Bandwidth 0.31 GiB/s : IOPS 6.0 kiops :
TOTAL 1.36 [INVALID]` — the io500 marks a sub-300 s stonewall INVALID, as
intended); the box runs `config-minimal.ini` unmodified once F1 is fixed.


## 4. Findings (what the harness caught on its first day)

F1. **Commit conveyor wedge** (product, first-class — charter §1.1; FIVE
occurrences in one day across every write shape: `seq-write-1m-buffered`
twice (warm-up of leg 1 at 12:32; measured span of leg 4 at 14:39),
`rand-4k-write` twice (leg 3 at 13:18; leg 2 at 15:47), `tar -x` leg 4 at
16:42, io500's ior-easy-write at 17:12). The first: `seq-write-1m-buffered`
on nvmet-tcp (libaio QD32 × 4, 1 MiB buffered writes): the WARM-UP never
returned. The FUSE op watchdog reported 4 writes
"in flight ≥ 75 s, parked past station `meta_backend_start`"; `.stats`:
`meta_conveyor_queued 6`, `meta_commit_parked 6`, `meta_conveyor_windows_inflight 0`,
`meta_conveyor_park_tick_rescues 0`, `journal_lane_parks 23005`; every daemon
thread (`sqz-jrnl0`, `sqz-meta0/1`, the lanes) parked in `io_cqring_wait`;
fio's threads in `request_wait_answer`; `invariant_tripwires 0`. Five to
seven transactions enqueued on the conveyor (`meta_conveyor_queued` =
`meta_commit_parked` = 5–7) with no leader pass running and no window in
flight, every time = a lost wake / leader-exit race in the conveyor's enqueue
→ pass handoff (`src/meta/conveyor.zig`). In the rand-4k occurrence the 5
parked committers held their 4a inode guards, so 116 further writes parked on
the lock behind them: `lock_ticked_reregisters` 109 536 (the 2 s ticked
backstop re-registering forever — it self-heals a lost lock wake, not a
holder that never returns) with the watchdog station `handler_entry`; in the
buffered and tar occurrences the station is `meta_backend_start`.
`meta_conveyor_park_tick_rescues` 0 in all five: the conveyor's own backstop
never fires for this park. Evidence dirs:
`wedge-seq-write-1m-buffered-20261010T1232Z/`, `wedge-rand-4k-write-leg3-20261010T1318Z/`,
`wedge-seq-write-1m-buffered-leg4-20261010T1439Z/`, `wedge-io500-ior-easy-write-20261010T1711Z/`,
plus `rand-4k-write-20261010T154534Z` leg 2's and `tar-x-20261010T163116Z`
leg 4's `*.wedge.*` captures (full `.stats`, daemon logs with the watchdog's
overdue reports and the wedge census, every task's kernel stack, the
instrument's stacks). The harness now bounds every instrument at
`2 × span + 120 s`, captures, kills the daemon and records the leg
`daemon_wedged`. Needs a red-first repro in the conveyor suite; the
conveyor-handoff model check should be re-run for the weakening this shape
exercises (an enqueue that observes "leader running" while the leader is on
its way out). Until it is fixed no sustained write row is VALID.

*Resolution* (ONE fix, two repros: `638e75f` from the external-suites agent
— fstests generic/074, fsstress -p 20 — and `7b8ea4e` from this harness;
decision 0002 row 451). Root cause: the handoff theorem hands a won
election to "the winner's pass"; on the product venue that pass is ONE
fiber and the handoff is one `TaskState.wake`, which the word ABSORBS when
the fiber is already SCHEDULED (law 2). `pending()` — the pass's re-check
after `leader.store(false)` — takes the queue mutex, and a committer
holding that lock in its enqueue parks the pass fiber on it; the
committer's unlock is a futex wake of the same word (IDLE → SCHEDULED), its
CAS wins the released leadership, its `wakePass` is a no-op, and the fiber's
poll loses `tryLead` to a live leader, returns and parks IDLE with the latch
set and one entry queued. Every later committer loses the CAS and parks:
`meta_conveyor_queued` = `meta_commit_parked`, no window in flight, no pass.
Model: `conveyor_core_model.zig` model 7 `PassFiber` (the delivery word,
the parkable mutex, the one wake); the weakening that fails is
`.park_without_leader_recheck` — the venue parking as soon as `pass()`
returns (the shipped `if (leader) pass()` + `f.park()`); this harness's
equivalent model found the same schedule in 139 states and was dropped as a
strict duplicate at the reconcile. Red test on the real venue (`7b8ea4e`):
`backend.zig` "the venue delivers every election: a committer whose enqueue
parked the retiring pass on the queue mutex is drained, never left with the
leader latched and the pass fiber parked" — the committer held inside its
lock hold and the leader between its release and its re-check through
`conveyor_core.test_seams` (Io-parked, never a thread spin on the lane); it
observed the wedge state (fiber IDLE ∧ latch set ∧ one entry queued) on the
pre-fix venue and rescued it for teardown. Fix: `passEntry` re-reads the
latch (`leaderHeld`) after every pass and runs again while it is set (no
suspension point between the re-read and the park), and the venue park now
rides the 2 s liveness tick — a tick that finds the latch set counts
`meta_conveyor_park_tick_rescues` and runs the pass (the dropped-wake seam
test "the venue's tick backstop" counts exactly one; `Fiber.timerFired`
keeps a timer that popped beside the scheduling wake from reading as a
tick). Before/after on `seq-write-1m-buffered` (nvmet-tcp, QD32 × 4): see
the measured rows at the end of this section.

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

*Resolution* (`2603169` red, `3e898b1` fix; decision 0002 row 453). Root cause: the accumulation arms (F extent checkout and
escalation, G checkout/adopt, J merge and seed) answered `error.WouldBlock`
at the parked-buffer cap (`parked_pool_refusals` ≡ `parked_gate_waits` ≡
`answered EAGAIN` = 115/107 on the 512m/job re-run), and the handler's one
errno table maps it to EAGAIN. The pool was full of COLD partials (250
parked, 36 uploads at the end of a leg: an overwrite stream whose
shadow-bound pieces take the accumulation arm and never complete their
block), so a pure park would have wedged. Red tests (`write_admit.zig`): "the
parked-buffer gate, cold set" and "the parked-buffer gate, hot set" (both
answered `WouldBlock`). Fix: THE PARKED-BUFFER GATE — holding no lock,
register on the table's settle notify, re-check for room; a set whose
coldest parked block sat untouched ≥ `PARKED_COLD_NS` (the 2 s liveness
tick) is drained by the writer itself (`parked_gate_cold_flushes`: one
bounded upload, zero-completed or seeded from the write-side authority);
otherwise wait for the next upload's settle, bounded by the moment the
coldest turns cold — or at once when no upload is in flight (nothing else
can return a cell; `34dedf3`). The gate wakes on the table's CAPACITY
notify (an entry retired), never on an upload's exit (`5db4540`: a failed
flush settles `upload_settled` too, and three writers at the cap riding it
woke one another's failed flushes forever — 7.7 M, then 3.75 M gate rounds
in four minutes on the harness's first legs); a flush the plane refuses
(terminal `StorageFull`, a failed DMA) retires nothing and is retried once
per tick, and once the refusals span the allocation layer's 1 s wall with
no capacity returned the write answers ENOSPC, counted
(`parked_gate_wall_refusals`, must stay 0 on a healthy set) and logged
once — the plane is broken, not slow (`34dedf3`; the store state behind it
is F17). The op's watchdog station is `write_parked_pool`;
`write_admit.Error` has no `WouldBlock` member and the handler has no EAGAIN
arm for a write. Legitimately non-blocking callers: none on the write path —
the IPC placed-sever registry's `PoolExhausted` is its own copy fallback, not
an answer to the application. Tests: "the parked-buffer gate, cold set" /
"hot set" (red: `WouldBlock`), "dead victim" (three writers; red: 130 flush
attempts in a few thousand yields), "the wall" (one error line, one
refusal, the parked set intact). Before/after on `seq-write-1m-direct`
(512m/job): see the measured rows at the end of this section.

F3. **The shim does not intercept fio's libaio engine** (product).
`LD_DEBUG=bindings` shows fio binding `io_submit@LIBAIO_0.1` /
`io_getevents@LIBAIO_0.4` to `libsqueezefs_il.so`, yet `ipc_ops_*` stay 0:
fio creates its context with `io_queue_init()`, which libaio resolves
internally to its own `io_setup` — the shim never sees the context and its
`io_submit` passes an unknown context through. The engagement law reports it
(INVALID). Fix: interpose `io_queue_init` / `io_queue_release` (and treat any
unknown context as a passthrough that is COUNTED). The shim rows here use
psync (QD1 × N) and say so.
Resolution: `613b1c6` — libaio binds its internal calls locally (Ubuntu's
`-Bsymbolic-functions`; `objdump -R libaio.so.1` has no `io_*` relocation), so
the shim now interposes libaio's queue face (`io_queue_init`/`release`/
`run`/`wait`, 65 interpositions + the API) with libaio's own bodies over its
wrappers; an unregistered context is a counted passthrough
(`aio_unknown_ctx_op` per bound iocb → the daemon's
`ipc_client_aio_unknown_ctx_ops`; `aio_unknown_ctx_call` per
getevents/cancel). Tests: `interpose` "io_queue_init is libaio's body over the
shim's io_setup…", `aio` "io_queue_run's body…", `e2e` "fio's shape — a
context from libaio's io_queue_init rides the ring…". Verified with fio 3.36
libaio QD32 × 4 rand-4k reads through the shim: file substrate
`Δipc_ops_read 194,299 ≡ fio ops 194,299`, nvmet-loop `166,973 ≡ 166,973`,
`ipc_client_aio_unknown_ctx_ops 0`, `engagement_shim` VALID. The shim rows run
libaio again (`BRK_IOENGINE=psync` is the QD1 control); `docs/porting/il_shim.md`
§6.9; 0002 row 443.

F4. **IL admission cap vs. a bounded budget** (sizing law). The daemon admits
IL sessions up to `budget/8` at a 64 MiB arena each; at `SQUEEZEFS_MEM_BUDGET_MB=2048`
only 3 sessions admit and fio's second process is refused ("session budget
exhausted (class 5)") and silently falls back to kernel FUSE — the engagement
law caught it (72 % of ops through the ring). On the box: 32 processes ×
2 sessions × 64 MiB = 4 GiB of arenas needs a ≥ 32 GiB budget, or the cap
must derive from the population.
Resolution: `613b1c6` — the charter's arena `max(64 MiB, align_down(cap /
population_target, 4 MiB))` yields to a population floor of `2 × cpus`
sessions (one client process per CPU at the shim's per-mount session floor):
the largest 4 MiB-aligned arena at which the floor's sessions (arena + the
layout's 148 KiB overhead each) fit the cap, never below the 4 MiB DMA grain
(`mem/sizing.resolveIpcArenaBytes`, `ipcSessionPopulationFloor`,
`ipcAdmittedPopulation`; the field shape is byte-identical); an arena under
64 MiB carries fewer slots of the shipped 64 KiB slab (`ipcSlotsForArena`),
so the async per-op ceiling and the lane gate's floor survive the yield (the
first cut shrank the slab and the interception smoke's 32 KiB tickets no
longer fit a slot: `79438de`; the client's multi-slot flight is capped at
`max_op` whatever the slab: `cdbaecc`). The arm line
prints the census and warns under the floor; a client open the refusal's
backoff window kernel-serves counts `establish_refused` (it read `not_ours`).
Tests: `mem/sizing` "the arena yields to the population floor…" (2 GiB ⇒
12 MiB arenas, 21 sessions; 6144 MiB ⇒ 40 MiB, 19), `ipc/sizing` "the HELLO
ladder on a bounded budget admits the population floor…" (admitHello admits
≥ 2 × cpus at every shape), `shim/classify` "…every open the backoff
kernel-serves is the refusal's fallback". Venue: the arm line read `arena
12 MiB × cap 256 MiB = 21 session(s) (population floor 18)` at 2048 MiB,
`28 MiB × 512 MiB = 18` at 4096, `40 MiB × 768 MiB = 19` at 6144; at 6144 all
four fio processes paired (`ipc_admission_refusals 0`). At 2048 the shared
cgroup still reads Red (F5) and the R5 shed target refused 3 of 4 sessions
(`ipc_shed_refusals 3`) — the budget authority's row, not the cap's.
Decision 0003 §H; 0002 rows 442, 444.

F5. **The budget authority reads the whole container** (sizing law). R5's
`pressure = max(Σ gauges, rss_max, unreclaim_max)` with `unreclaim` = the
cgroup's `memory.stat` (anon + file_dirty + file_writeback + shmem +
unevictable + slab_unreclaimable) ÷ the co-located-daemon share. The venue
container is ONE root cgroup for three agents: its `anon` read 8.8 GB while
this harness's daemon owned 1.3 GiB of gauges (fiber stacks 0.7 GiB, kmbuf
0.3, zc bounce 0.3) — so legs went Yellow/Red at a 7 GiB budget on the
neighbours' zig builds and daemons (and a 2 GiB budget sat Red from the first
tick behind the daemon's own ≈ 2.7 GiB `unreclaimable` floor: 9 queues ×
32 MiB kmbuf rings + arenas + slabs + stacks). Red is a real posture change
(sheds, parked caps halve, disk-tier publishes pause), so the harness marks
any non-Green leg INVALID (`mem_budget_green`, with the top components in
the detail) — 7 rows here carry at least one such leg. On the box (own
cgroup, memory-backed targets) the arm reads the daemon alone.

F6. **`journal_lane_park_tick_rescues` under write saturation** (product,
must-stay-≈0). 6–186 per 60 s leg on every saturated write row
(`seq-write-1m-direct` legs: 174/128/18/6; the shim write row 3–10; a read
row's umount-side flush 5), plus one `lock_ticked_reregisters`. The journal
lane's 2 s backstop found work a wake should have delivered: a lost-wake
class in the conveyor → journal-lane handoff, kin to F1.

*Resolution* (`c9ae924` red, `9774031` fix; decision 0002 row 452). NOT a
lost wake: the lane's backstop is 100 ms (not 2 s) and the shipped verdict
counted every backstop-ended park as a rescue. On the 512m/job re-run the
rescues track the journal windows that outlasted 100 ms one for one (leg 1:
8 rescues, 8 windows ≥ 128 ms; leg 2: 46 rescues, 27 windows ≥ 128 ms and
55 ≥ 64 ms; one checkpoint barrier ≥ 512 ms) — the venue's one shared virtio
disk under 400 MB/s of data writes, the FSYNC and the 4 KiB journal write
behind it. Every out-of-lane completer kicks (`completeAbandoned` →
`smo_journal.device.completed`), and `meta_conveyor_park_tick_rescues`,
`channel_ticked_reregisters` and `exec_lane_tick_rescues` stayed 0
throughout. Red test (`journal_lane.zig`): "a slow device: a window whose
runs are owed past the backstop costs backstop ticks and ZERO tick rescues"
(a `delay_issue` seam holds the runs un-issued; the shipped verdict read
"expected 0, found 1"); the companion "a lost kick" pins the rescue
reachable (a completer that forgets to kick: exactly one). Fix: the verdict
is the next poll's re-check (`sync/tick.zig`'s channel-class contract) —
progress with no wake delivered is `journal_lane_park_tick_rescues`
(must-stay-0), nothing moved is the new `journal_lane_park_backstop_ticks`
(reported beside, outside the harness's rescue sum); `Fiber.timerFired()`
keeps a timer that popped in the same lane pass as the scheduling CQE from
reading as a tick (the same phantom class the lane's own
`exec_lane_tick_rescues` had on 2026-10-07). Before/after: see the measured
rows at the end of this section.

F7. `transport_lease_overlong` 128–1412 per saturated write leg: FUSE_WRITE
payload leases ≥ 1 s because 128 MiB are queued ahead of a ~400 MiB/s
substrate (p50 196 ms, p99 1.7 s on the file substrate). Reporting, not a
defect, on this substrate.

F8. **SafeAllocator leak at exit** (product, small): 144 B from
`src/ops/nvmet/initiator.zig:875` (`sample`: `prev.live = cur.live` drops the
previous controller list) on every mount over nvme namespaces once `.stats`
has been rendered — one `error(` line per daemon lifetime on every nvmet row.
Resolution: fixed by `9949e79` (landed the same morning, after the measured
`ce0edf4`): the retained controller identities live on the process allocator,
never a render's (`sampleWith`), with the red-first test "the process
sampler's retained identities live on the process allocator: a sample through
a per-render allocator leaves NOTHING live in it…" (two renders, controllers
present). Verified at `613b1c6` on nvmet-loop (3 fabric controllers,
`stats_builds 12`): the daemon log carries 0 `leaked`/`SafeAllocator` lines
through the clean unmount.

F9. **`rewrite_device_write_bytes` is not a second device write**
(accounting). On a 22.28 GB overwrite row: `overlay_store_bytes` 22.17 GB +
`rewrite_device_write_bytes` 6.09 GB vs diskstats 1.025 × user. The rewrite
epoch counts the same overlay DMA it was fed. `src/cli/bench.zig`'s
`device_write_keys` sums both and over-reports amplification (1.27 here);
the harness reports it beside. bench.zig should follow.
Resolution: `613b1c6` — `bench.StatsTerms.device_write_keys` is brk.py's
`DEVICE_WRITE_ARMS` (`overlay_store_bytes, patch_write_bytes,
own_block_dma_bytes, durable_upload_bytes_escalation,
durable_upload_bytes_writeback`), `rewrite_device_write_bytes` is read beside;
`device_write_req_keys` = `DEVICE_WRITE_REQS` (`patch_writes, own_block_dmas,
put_obj`). The rail "bracket ledger pin…" (`tests/rails.zig`) holds the four
lists equal to brk.py's and the two gauges beside in both. (`d41eec7`: the
escalation counter is `write_through_bytes`' subset and moved beside; see the
write amplification rows in §4.)

F10. **Reads on a zero-copy session never reach a handler lane**
(accounting). An O_DIRECT READ the warm ladder cannot answer is served at the
delivery CQE by the direct device leg (`transport_zc_direct_legs`,
`transport_zc_serve_bytes` = user bytes exactly); `fuse_ops` does not move,
nor do the data plane's `read_copy_*` arms. An engagement or copy ledger
built on `fuse_ops` / `read_copy_*` alone (as bench.zig's) reads 0 for every
read row; the harness uses `transport_ring_requests` and adds the transport's
serve arms.
Resolution: `613b1c6` — bench's read ledger is brk.py's `READ_COPY_ARMS`
(11 terms: the plane's `read_copy_*`/`read_dest_dma_bytes`/`read_zc_serve_bytes`
plus `transport_zc_serve_bytes` and `transport_zc_bounce_bytes_bridged`;
`read_dest_lease_bytes` beside, F15), its engagement term is
`transport_ring_requests` with `fuse_ops` printed as the handler-lane share
and `transport_zc_direct_legs`/`transport_zc_serve_bytes` beside; the rail
pins the list. The §6 owed rail landed with it: "stats name census: every
name in brk.py's ALL_COUNTERS is a `.stats` key the product emits" (116 names
against the source census of instrument declarations and the family writers'
keys) — its first run caught `transport_wake_elided` (the key is
`transport_wakes_elided`; it had read 0 under `absent_keys`) and the
client-page-only `lane_gate_threshold_bytes`.

F11. mdtest's `-W` stonewall "does only work with a branch factor ≤ 1": a
many-dirs creates row cannot be time-boxed by the instrument; the harness
sizes the count from the warm-up's rate (elapsed recorded).

F12. The file substrate's data volumes are opened buffered ("write cache
file-backed"): reads come from the VM page cache (18 GB/s); the nvmet file
backend likewise on the target side. The venue has no device-true read path;
`-o direct_device_true` here rules only the daemon's tiers out, not the
substrate's cache.

F13. Other agents' `pkill -9 -x squeezefs` sweeps kill this harness's daemons
and its CLI teardown verbs at random, and a rebuild of `zig-out-linux/`
under a running matrix swapped the binary AND the shim mid-row (the shim then
refused to pair: `ipc_sessions_total 0`, four rows INVALID for the wrong
reason — the first pass's records were discarded). The harness now snapshots
the binaries into its scratch (sha256 recorded) and measured the
`.wt-brackets` worktree build of `ce0edf4`; it survives a kill (dead leg
INVALID, lazy-detach, residue sweep) — but a shared venue cannot be a quiet
one.

F14. **The W1 patch shape writes ≈ 0.74 × user bytes to the METADATA
device** (`rand-4k-write` on nvmet-loop, the namespaces' diskstats: data
1.00–1.04 ×, meta 0.73–0.75 ×; `wareq-sz` 4 096–4 279 B on data): every
4 KiB patch is one journal transaction, and the sector-pad law frames each
conveyor window as a 4 KiB-aligned run. At the box's 727 k IOPS bar that is
≈ 2.2 GB/s into the metadata namespace — a term the bar's substrate (five
memory-backed targets) must carry and the record now shows per namespace.

F15. `read_dest_lease_bytes` is the dest-lease mechanism's gauge over the
same bytes `read_dest_dma_bytes` lands (1:1 on the shim read row); summing
both reads the copy ledger at ×2.0. Reported beside, not summed (the shim's
`ipc_bytes_out` likewise: the handoff DMAs into the arena through the data
plane's arm).

F16. Metadata rows cost ≈ 1.01 journal entries per create and 5.21 per
`tar` entry (create, write, the setattr pair, the parent's Δtime); the
fsync phase ledger is dominated by the two device barriers (data 824 µs +
meta 517 µs of 1 343 µs on nvmet-loop over a file backend).

F17. **The aging store starves its own flushes** (product, data plane —
surfaced by F2's gate; owner: the allocator/reclaim). On the harness's
default 6 GiB × 2 set, every sustained overwrite row ends with the
allocator refusing the WRITE-THROUGH and the gate's cold flushes
`StorageFull` while the volumes read 50 %/18 % (buffered) and even
25 %/8 % (direct, 512m/job) full: `alloc_valve_passes` 3.6–3.9 k per
leg, `overlay_enospc_declines` 1.1–1.2 k, `write_through_fallbacks`
580–650 — the reclaim returns supply at the device's pace
(`block_free_elided_debt_bytes` 6.3 GB per leg, the debt drains at
1 Hz) and the hot overlay writers mint every block the moment it lands
(`alloc_fresh_mints` 6–7.7 k per leg succeed), so a block that COMPLETED
in RAM cannot upload (`write_pipeline_stay_parked` ≡ the fallbacks) and
sits parked; the pool fills with those, and the gate's victim flush loses
the same race once per tick until its wall. The pre-fix binary showed the
same store state as 100–128 EAGAIN per leg (F2); the gate shows it as
57–96 `parked_gate_wall_refusals` (ENOSPC, one loud line each) per leg
and, on the QD32 direct row, as `lock_ticked_reregisters` 1.75 k (the
siblings queued on the inode guard behind each gate-parked writer for the
two ticks the wall allows). `write_fresh_block_enospc_refusals` and
`alloc_fresh_supply_exhausted` stay 0 throughout — the allocation layer
never calls the set exhausted, it just refuses each attempt after the
valve drained. The same row on a 24 GiB set (`--data-size 25769803776`,
the fresh frontier never exhausted within a leg) is VALID: fio rc 0, 0
EAGAIN, 0 wall refusals, `parked_pool_refusals` 0 (uploads land, the pool
never fills). Owed: a supply wait for the completed block's upload (the
grace-bounded form, or priority for a flush that frees RAM over a fresh
overlay mint), and the reclaim's debt drain paced to the device, not 1 Hz.

*Resolution* (`36acb58` the ledger, `09a1014` the supply wait and the
gate's law, `a8ee356` the paced debt drain; decision 0002 rows 455–458).
Root cause — NOT the reclaim and NOT the hot overlay writers: the grant
plane's `Ledger` is a 1,024-slot table of OPEN grants and nothing ever
retired the holder's own CONSUMED grants (the reference's `Vec` kept them
too, unbounded; the port's bound turned the growth into `table_full ⇒
Deferred`). Once the frontier wraps, carves truncate at the first set bit
(≈ 9 blocks each on the fragmented 6 GiB set), so the holder's own window
carved > 1,024 times in one 60 s leg; every top-up then answered
`deferred`, the window never refilled, and every mint on that volume read
an empty window as `StorageFull` while the bitmap was 25 % populated
(`alloc_fresh_supply_exhausted` stayed [0, 2] because the clear bits were
in sight). The repro at `53d05e1` (direct row, 512m/job, two legs):
`block_grant_deferred` 7,669 / 7,457 ≡ `block_grant_topup_deferred`,
`block_grant_exhausted` 7,876 / 7,900, the holder's word on vol 0 at the
end of both legs `open_grants 1,024, own_window_remaining 0`, vol 1 216 /
156 — and the stripe-unit affinity kept each file's blocks on its volume,
so the files on vol 0 failed every mint, parked, met the gate, and the
gate's victim flushes failed the same way until the wall. The 24 GiB set
never wrapped inside a leg (every carve a full 64-block run), hence VALID.
Fixes: (1) an open grant is what the writer still HOLDS — `Ledger.
syncWriter` retires every slot the writer's ask no longer names
(`block_grant_consumed_retired`), the in-process sink is bound to its
window and syncs before every ask; the ledger takes its own lock (the wire
service's threads and the holder's own top-up mutated it unserialized);
(2) `block_alloc.MintAnswer`: an empty window answers its CLASS — the
holder's `full` is `StorageFull`, a top-up owed by a deferred/unreachable
holder is `PlaneRefused` (never a latch); `WriterArm.mintBlock` is the ONE
grant source, and a racer that lost the single-flight latch at an EMPTY
window waits for the in-process carve (`block_grant_topup_waits`) instead
of reading the gap as exhaustion (over the wire: `retry`); (3) the
RAM-freeing upload's mint parks grace-bounded for VISIBLE supply
(`enospc.SupplyWait.visible_supply`: slices on the supply edge, which every
landed top-up now rings, up to the 1 s wall) while the fresh overlay mint
declines to accumulation at once — the fresh mint yields scarce supply to
the flushes that return RAM; (4) the gate's wall answers ENOSPC iff
`enospc.freshSupplyExhausted` over the placement candidates (the charter's
terminal law), else fires the must-stay-0 tripwire
`parked_gate_wall_supply_visible`, restarts the wall and keeps the write
parked at its watchdog station; (5) the debt drainer is paced to the
device: its trim ranges feed their own completion meter
(`block_free_debt_drain_rate`), a pressure pass claims one tick's worth of
completions (`debtBatchBlocks`) and rests one batch-time at the measured
rate (`debtPaceMs`, `block_free_debt_pace_ms`) — the shipped `progressed ⇒
at once` loop ran 110–145 serialized passes per leg behind the write lane
on the wrapped volume (virgin tail 0 ⇒ every byte of debt "past the
watermark"), its fruitless backoff a fixed 1 Hz ladder. Red tests:
`block_grant` "the aging store (rung 9 F17)…" (a 64-slot table over a
fragmented 256-block volume, 40 laps of scattered overwrite: without the
sync it fails at lap 6 mint 810 — "PlaneRefused with 128 clear bits, 64
open grants, window 0") and "an empty window with another minter's INLINE
top-up in flight…"; `enospc` "the write path's grace-bounded mint…" (the
three classes); `write_admit` "the wall on an EXHAUSTED set" / "the wall
with supply VISIBLE"; `reclaim` "the debt drainer's pace is the device's…"
(the tie) and "the debt drainer under a FOREGROUND stream…" (16 blocks of
debt: two single-batch passes then the cold rest with 8 still owed; the
shipped loop drained all 16 at once).

**The first A/B through the harness** (A = `53d05e1`, B = `7f76af1`,
nvmet-tcp, the DEFAULT 6 GiB × 2 set, 1g/job, QD32 × 4, A→B→B→A,
record `seq-write-1m-direct-20261011T011807Z`): both A legs INVALID — fio
rc 3, 96 ENOSPC (error 28) each, `parked_gate_wall_refusals` 96,
`alloc_valve_passes` 3,916 / 3,924, `overlay_enospc_declines` 1,216 /
1,218, `write_through_fallbacks` 651–654 ≡ `write_pipeline_stay_parked`,
`block_grant_deferred` 7,589 / 7,685, `block_grant_exhausted` 7,832 /
7,848, `alloc_fresh_mints` 7,889 / 8,463, `open_grants` 1,024 on vol 0 at
the end, `block_free_elided_debt_bytes` 5.23 / 4.76 GB at the end
(`block_free_trim_discards` 2,274 / 2,682 absolute, 110 / 145 pressure
passes); both B legs VALID — **1,247 / 1,263 MiB/s**, fio rc 0, 0
ENOSPC, `parked_gate_wall_refusals` 0, `parked_gate_wall_supply_visible`
0, `parked_gate_waits` 0 (the pool never fills), `alloc_valve_passes` 0,
`overlay_enospc_declines` 0, `write_through_fallbacks` 0,
`alloc_fresh_supply_exhausted` [0, 2], `block_grant_deferred` 0,
`block_grant_exhausted` 0, `block_grant_consumed_retired` 7,152 / 8,942,
`open_grants` 2–4 at every leg's end, `alloc_fresh_mints` 18,736 / 18,979
(2.3 × the A legs: the store was writing the whole leg), tripwires 0,
tick rescues 0 (A: 1,850 / 1,750 `lock_ticked_reregisters`), mem budget
Green; device ÷ user **0.996** (stats) / **1.000** data + **0.010** meta
(diskstats), `wareq-sz` 1,047,635 / 1,052,635 B; the debt at the end 4.11 /
4.73 GB (1,776 / 1,778 trim discards absolute at 15–19 blocks/s, pace
105–133 ms, 0 fruitless passes) — the debt is `⊆ free` by construction and
the drain is hygiene paced to the device, not supply.
`seq-write-1m-buffered` (the same set and shape, record
`seq-write-1m-buffered-20261011T012426Z`): A legs INVALID — fio rc 3, 60 /
66 ENOSPC, `parked_gate_wall_refusals` 60 / 66, `alloc_valve_passes`
3,628 / 3,684, `overlay_enospc_declines` 1,144 / 1,150,
`write_through_fallbacks` 578 / 591, `block_grant_deferred` 6,925 /
6,947, `open_grants` 1,024 at the end, 640 / 522 MiB/s for what fio did
write; B legs VALID — **1,002 / 790 MiB/s**, fio rc 0, 0 ENOSPC, every
F17 counter 0, `block_grant_consumed_retired` 6,139 / 4,996,
`open_grants` 2–10, tripwires 0, tick rescues 0 (A: 0 too on this row),
device ÷ user 0.996 / 1.000 + 0.010 (A: 0.972 / 0.974 — the A legs'
device bytes are the bytes they refused). The debt at the end: A 7.31 /
8.44 GB, B 4.48 / 8.29 GB — and legs 3 (B) and 4 (A) show WHY the second
pair sits high: 0 trim discards and 0 pressure passes on BOTH binaries,
the drainer asleep for the whole leg. Its only wake was `debt_target`'s
first false → true swap per volume; once reuse cancelled every debt block
between two passes, the idle park (no deadline) outlived the mount.
`8b40238`: the elision arm notifies on every 0 → > 0 transition of a
volume's debt (`notifyOne` keeps one permit); red: the "idle drains elided
debt…" test extended with a later free of the same volume ("expected 0,
found 4096").

### Measured rows after the F1/F2/F6 fixes (binary `34dedf3`, nvmet-tcp, QD32 × 4)

`seq-write-1m-buffered` (1g/job, 4 legs × 60 s): **0 wedges** (pre-fix:
2 of 4 legs wedged on this row, 5 wedges in the day); 483–519 MiB/s;
`meta_conveyor_park_tick_rescues` 0 / `journal_lane_park_tick_rescues` 0
on every leg (pre-fix 9 and 18 on this row's valid legs);
`journal_lane_park_backstop_ticks` 2–20 (the slow device, reported
beside); `answered EAGAIN` 0; `lock_ticked_reregisters` 0;
`parked_gate_waits` 109–131 ≡ `parked_gate_cold_flushes`, of which
57–69 ended in `parked_gate_wall_refusals` (fio error 28 — F17's store).
`seq-write-1m-direct` (512m/job, 2 legs): 0 wedges, 0 EAGAIN (pre-fix
100–128 per leg), 0 rescues, 96 wall refusals per leg (F17), fio rc 3.
`seq-write-1m-direct` (512m/job, **24 GiB set**, 2 legs): VALID — fio
rc 0, 373 / 456 MiB/s, 0 EAGAIN, 0 wedges, 0 rescues, `parked_pool_refusals`
0, `journal_lane_park_backstop_ticks` 119 / 141 (p99 0.9–2.0 s on the
saturated substrate — F7's class, not a lost wake).

### The write amplification rows (2026-10-11, decision 0003 §J; binaries A = the fix, B = `c36b531`)

Two rows added for the two measured defects of `docs/evidence/rung-09-external-suites.md`
(generic/074's 128× W2 fold, generic/069's whole-image re-stage per 4-byte append), and two
laws the harness now asserts on every leg: `write_amplification_bound` (data-write rows:
`device bytes ÷ user bytes ≤ pages(bs, gcd(bs, 4 KiB)) × 4 KiB / bs`, + 0.1 slack — 8× at
512 B, 1× at a page multiple) and, on the `data-append` kind, `staged_append_bound` (ring
bytes moved ≤ 11 × bytes appended) with `journal_entries_per_append ≤ 1.05`. `fold_inplace_
write_bytes` / `fold_inplace_dmas` joined the device-write arms (`bench.zig` pinned equal by
the rail). Both rows ran A-B-B-A, 4 legs × 60 s (+10 s warm-up), `file` substrate, numjobs 4,
budget 6144 MiB, this venue; the `mem_budget_green` INVALIDs below are the shared cgroup's
pressure signal (other agents' daemons), not the rows'.

| row | leg/arm | headline | p50 / p99 µs | device÷user (stats) | device÷user (diskstats) | the law | record |
|---|---|---|---|---|---|---|---|
| frag-512b-write (fio psync randwrite bs=512 direct=0, 256m/job × 4; the 074 shape over mapped sole-owner blocks) | 1/A | **9831 IOPS** | 239 / 1794 | **7.059** | 8.323 | `write_amplification_bound` ok (bound 8×; in-place folds 2.13 GB over 393,982 DMAs) | `frag-512b-write-20261011T171228Z.json` |
| | 2/B | 5349 IOPS | 494 / 6128 | **129.7** | 130.7 | FAILED (bound 8×; 21.3 GB for 164 MB; in-place folds 0) | |
| | 3/B | 4621 IOPS | 494 / 6849 | **130.3** | 136.2 | FAILED | |
| | 4/A | **9412 IOPS** | 237 / 1991 | **6.805** | 8.139 | ok | |
| append-4b (appends.py, 4 writers × 4 B O_APPEND integers, read back in order; a staging root formatted by the harness) | 1/A | **8110 appends/s** | — | 0 (the ring) | 170 | `staged_append_bound` ok: moved ×9.264 (484,036 in place, 56 re-stages); entries 1.001/op | `append-4b-20261011T171837Z.json` |
| | 2/B | 2624 appends/s | — | 0 | 4986 | (c36b531 has no ring ledger: every append re-staged the image); entries 1.001/op | |
| | 3/B | 2597 appends/s | — | 0 | 15677 | | |
| | 4/A | **7597 appends/s** | — | 0 | 3513 | ok: moved ×9.829 (452,804 in place, 56 re-stages); 1.001/op | |

A/B: `frag-512b-write` **1.93×** the IOPS at **7× instead of 130×** the device bytes (the
daemon's own arms; diskstats on the shared disk 8.1–8.3× vs 131–136×), p50 halved, p99 a
third; `append-4b` **3.0×** the appends/s, ring bytes moved 9.3–9.8× the appended bytes
against the 11× bound (the geometric re-stage chain at a quarter slack; 56 re-stages for
≈ 480 k appends per leg) where c36b531 moved the whole image per append. The appends/s
floor on both arms is the per-write journal entry (one Δ record per write, durable at ack):
≈ 120 µs per append across 4 writers on this substrate. Scoping runs at 2 × 15–20 s
(`append-4b-20261011T164101Z`, `frag-512b-write-20261011T164244Z`) agree. An earlier
cut of the fix seeded a deferred overlay's page at FIRST TOUCH (one 4 KiB device read on the
write path): the frag row's p50 rose 317 → 502 µs; the seed moved to the fold (one ranged
read per run of unseeded pages) and the fold itself off the write (detached), and the row's
p50 settled at 237–239 µs against c36b531's 494.

F9's arm list moved once more (`d41eec7`): `write_through_bytes` — EVERY parked full
buffer's upload (the sequential accumulation, the W2 escalation at its RAM trigger, the
flush-time complement) — is a device-write arm, and `durable_upload_bytes_escalation`, its
flush-time-complement subset, is read beside. Found on generic/074's growing-file shape
through the fixed fold: 825 write-through blocks (3.5 GB on the device) and 0 escalation
bytes — the ledger summed the subset and read 0 device bytes. The two bracket rows above
are unaffected (the `frag-512b-write` row writes into mapped blocks, which fold in place;
`append-4b` lives in the ring); the sequential rows were overlay-eligible (arm B) and
carried no write-through bytes.

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
  real A/B is the fix for F1/F2 against `ce0edf4`, through this harness.
- **Noise**: 5–112 % bands on a venue whose load average ran 4–20 on 9 CPUs;
  the one quiet row (`rand-4k-write` on `file`, 0.8 %) shows what the harness
  resolves when nothing else runs.

## 6. Owed

- Red-first repros for F1 (conveyor wedge), F2 (EAGAIN to a blocking write),
  F6 (journal lane tick rescues). (F3, F4, F8, F9, F10 are resolved above:
  `613b1c6`; the `brk.ALL_COUNTERS` rail landed with them.)
- The harness runs the A/B over binaries and over the format posture; the
  lever over a `SQUEEZEFS_*` A/B knob (same binary, knob on/off) is the next
  lever to add.
