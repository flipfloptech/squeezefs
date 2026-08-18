# The FIELD MPI-IO runbook — the rung-18 shared-vs-disjoint ior row over a real nvme-tcp fabric

This is the operator sequence for running the S11 MPI-IO acceptance row
(`s11-mpiio`, `tests/run_mw_matrix.sh`) on the FIELD cluster: client
`memp-s3ds-aqs-37` (32 CPU, 2×200GbE) + 5 storage nodes over nvme-tcp,
targets built by `tests/cluster_reset_v5_mw.sh` (the `cluster_reset_v4.sh`
lineage extended to the multi-writer fleet shape). The local verdict rows
this field row is read against are
[.benchmarks/2026-08-18-s11-mpiio-row.md](../.benchmarks/2026-08-18-s11-mpiio-row.md)
and the issued MET verdict in
[.benchmarks/2026-08-18-s11-widthn-refs-fix.md](../.benchmarks/2026-08-18-s11-widthn-refs-fix.md);
the prep record for this machinery is
[.benchmarks/2026-08-18-mw-field-mpiio-prep.md](../.benchmarks/2026-08-18-mw-field-mpiio-prep.md).

Field conventions apply throughout: **everything lives under
`/scratch/tmp`** (`/tmp` is banned on the field boxes) — binaries at
`/scratch/tmp/squeezefs`, the authority mount at `/scratch/tmp/test`,
co-writer mounts at `/scratch/tmp/test-cw1..8`, logs at
`/scratch/tmp/logs/sqz-mw-*.log`, rows at `/scratch/tmp/mwmatrix-rows`.

---

## Preflight 1 — CLIENT kernel (expected: NO change needed)

```
ls /sys/module/fuse/parameters/enable_uring
```

Expected output: the path itself
(`/sys/module/fuse/parameters/enable_uring`). The knob EXISTING is the
requirement — FUSE-over-io_uring is mandatory for every mount and the
daemon auto-enables `fuse.enable_uring=Y` when it can (mount fails loud
otherwise). The client already mounts today under `cluster_reset_v4.sh`,
so this is a confirmation, not a change.

**No client kernel swap is needed for the multi-writer fleet.** The
authority and every co-writer are CO-LOCATED on this one client and share
the box's default NVMe host identity (`/etc/nvme/hostnqn`) — the
`docs/operations.md` §Multi-writer co-writer mounts shape. The sqz-kernel
patch 0030 (host-scoped fabric subsystems) exists ONLY for
multi-IDENTITY mounts (two explicit hostnqn pairs on one box), which this
campaign does not use.

## Preflight 2 — STORAGE-NODE kernels (the one place a kernel update may be needed)

The S9 multi-writer arm **refuses on a non-PR substrate**, and nvme-tcp
Persistent Reservations require nvmet PR support on the TARGET kernels.
Probe each storage node (safe transient configfs probe; run as root on the
node):

```
modprobe nvmet
mkdir -p /sys/kernel/config/nvmet/subsystems/nqn.prprobe/namespaces/1
ls /sys/kernel/config/nvmet/subsystems/nqn.prprobe/namespaces/1/resv_enable
rmdir /sys/kernel/config/nvmet/subsystems/nqn.prprobe/namespaces/1 \
      /sys/kernel/config/nvmet/subsystems/nqn.prprobe
```

* Expected (capable node): the `ls` prints the `resv_enable` path.
* If `resv_enable` is **ABSENT**: that node's kernel lacks nvmet
  Persistent Reservations and **that is the one case needing a node
  kernel/module update**. nvmet PR shipped in **mainline v6.13**
  (`nvmet: support reservation feature`, `drivers/nvme/target/pr.c`; some
  distro kernels backport it into 6.12-based trees — the attribute probe
  above is the truth, not the version string). The sqz kernel series
  (`docker/kernel-sqz/`, 6.19.x-sqz) ships it and is the program's
  known-good option for the nodes. Storage nodes do NOT need patch 0030 —
  any nvmet-PR-capable kernel serves.

Once targets exist, the live-check form is:

```
ls /sys/kernel/config/nvmet/subsystems/*/namespaces/1/resv_enable
cat /sys/kernel/config/nvmet/subsystems/*/namespaces/1/resv_enable   # every line: 1
```

`cluster_reset_v5_mw.sh` asserts exactly this per node at target-build
time (the product `nvmeof share` verb writes `resv_enable=1` before
enable whenever the kernel offers the knob — on a knob-less kernel it
only prints a detection-grade note, which is why the v5 script turns the
absence into a build-time FATAL naming the node).

## Preflight 3 — CLIENT userspace

On the client (all needed by the row itself):

```
command -v mpirun mpicc curl gcc make python3 nvme
```

* `mpirun`/`mpicc` — Open MPI or MPICH distro packages (the local verdict
  ran Open MPI 5.0.10). The leg builds the PINNED ior 4.0.0
  (sha256-checked) on demand into `<checkout>/target/mw-ior`; GCC ≥ 15
  build nuance (`-std=gnu17`) is handled inside the leg.
* A **repo checkout on the client** — the paste line runs
  `tests/run_mw_matrix.sh` from it, and ior builds into its `target/`.
* `/scratch/tmp/squeezefs` on the client and every node — the SAME build
  (the paste line pins `SQZ_BIN` to it).
* Root mpirun (`--allow-run-as-root`), core-binding
  (`--bind-to none`), and PRRTE slot oversubscription
  (`--map-by :OVERSUBSCRIBE`) are handled inside the leg.

## Preflight 4 — PR end-to-end verify (after connect)

The v5 script runs this itself after step 3; the manual form, on the
client against any DATA namespace:

```
nvme resv-report /dev/nvmeXn1
```

Expected: exit 0 and a report (before any mount: `regctl: 0`, no
reservation). Failure here means the fabric does not transport PR
end-to-end — go back to Preflight 2. After the authority mounts, the same
command shows the WERO hold: `rtype: 3` (Write Exclusive — Registrants
Only) with `regctl >= 1`, and the authority's `.stats` reads
`data_plane_fence_mode: 1`.

---

## The run sequence

1. **Dry-run preflight on the field box** (prints every ssh/format/mount
   command, executes nothing; works unprivileged):

   ```
   tests/cluster_reset_v5_mw.sh --dry-run
   ```

2. **Reset + fleet bring-up** (DESTROYS all cluster data; type `YES`):

   ```
   sudo tests/cluster_reset_v5_mw.sh
   ```

   What it does beyond v4: asserts `resv_enable=1` per namespace at
   target build, verifies `nvme resv-report` per data namespace after
   connect, formats (the default format is multi-writer-capable), mounts
   the AUTHORITY at `/scratch/tmp/test` with the S9 arm
   (`SQUEEZEFS_MULTI_WRITER=1`, membership bind, stable MW port 45999,
   `SQUEEZEFS_FLEET_SHARE=9`), harvests each co-writer's durable
   enrollment id from its rung-3 refusal, re-arms the authority with the
   roster (a new era), then mounts the 8 co-writers at
   `/scratch/tmp/test-cw1..8` (`SQUEEZEFS_MW_ROLE=co-writer`,
   `SQUEEZEFS_MW_AUTHORITY=<parsed endpoint>`,
   `SQUEEZEFS_RANGE_CUSTODY=1`) — every mount readiness-gated on its own
   log lines (`data-plane WERO (rtype 3) acquired`,
   `CO-WRITER ADMITTED`) and `.stats` posture
   (`data_plane_fence_mode=1`, `membership_mode`, `mount_posture`).

3. **Run the row** — the script prints the exact paste line; it is:

   ```
   sudo SQZ_BIN=/scratch/tmp/squeezefs \
        SQZ_MWMATRIX_MOUNTS=/scratch/tmp/test,/scratch/tmp/test-cw1,/scratch/tmp/test-cw2,/scratch/tmp/test-cw3,/scratch/tmp/test-cw4,/scratch/tmp/test-cw5,/scratch/tmp/test-cw6,/scratch/tmp/test-cw7,/scratch/tmp/test-cw8 \
        SQZ_MWMATRIX_ROWDIR=/scratch/tmp/mwmatrix-rows \
        tests/run_mw_matrix.sh s11-mpiio --procs=4
   ```

   `SQZ_MWMATRIX_MOUNTS` is the external-mounts mode (first entry =
   authority, rest = co-writers): the leg verifies each mount's posture
   from its own `.stats`, convicts a missing `SQUEEZEFS_RANGE_CUSTODY`
   arm at the probe pass, and never reads the local-fleet MEMBERS table.
   8 mounts × 4 procs = 32 ranks — the local verdict geometry.

4. **Where rows land**: `/scratch/tmp/mwmatrix-rows/s11mpiio-<epoch>/` —
   ior outputs per phase (`probe.out`, `A1.out`, `B1.out`, `B2.out`,
   `A2.out`, `readcheck.out`), per-mount stats snapshots
   (`m*_p*.json`, taken by `cat .stats` — never cp), and `fsck.out`.
   The leg prints the A-B-B-A table, the ≥ 0.8× verdict, the exact
   engagement columns, and the fsck/C8 result; exit 0 = green row.

5. **Teardown / repeat**: re-run `sudo tests/cluster_reset_v5_mw.sh` —
   every counted run gets fresh backings and a fresh format (the
   counted-restart discipline: a run after any fix restarts the count
   from zero; pre-fix greens are never credited).

---

## Reading the row honestly

* **The tier**: the local verdict
  ([2026-08-18-s11-widthn-refs-fix.md](../.benchmarks/2026-08-18-s11-widthn-refs-fix.md):
  A-B-B-A min bracket **1.411×, MET**) is **measured-simulated** — one
  box, co-located members, nvmet-tcp on localhost, memory backings. The
  field row is a **real nvme-tcp fabric** (5 nodes, 2×200GbE): a
  different substrate class (`docs/rc-manifest.md` tiers). The GATE is
  the same (shared ≥ 0.8× disjoint in BOTH brackets); the absolute
  MiB/s numbers are **not comparable across tiers** — state both tiers
  in any note that cites both.
* **The inline-map cap**: the leg self-sizes the shared file by probed
  bandwidth and caps it at **5,120 MiB** — inside the inline-map domain
  (~6 GiB at 4 MiB blocks). Past that boundary a co-writer's spill takes
  the shared head `indirect:` and concurrent publishes refuse
  loud-and-fail-safe (the rung-20 residual #1,
  blob-aware owner-side composition). At 200GbE probe rates the cap WILL
  bind: the leg says so, raises its iteration ceiling to keep the ≥ 60 s
  sustained window, and warns (labels) if the probed rate makes even
  that window unreachable. A capped row is honest; an uncapped one would
  be an fsync-EIO refusal, not a result.
* **Quiet box**: keep the client otherwise idle for measured runs. The
  leg's quiet gate checks for foreign cargo work and labels the table
  `PROVISIONAL` if found — on the field the equivalent is: no other
  benchmark, no rsync, no competing mounts during the row. Every row
  states its instrument (pinned ior 4.0.0 + mpirun version — the leg
  prints both) and its substrate (this fleet: nvme-tcp, 5 nodes,
  nullblk-or-zram backing per the CONFIG block — record which).
* **A-B-B-A is internal to the leg**: shared/disjoint/disjoint/shared runs
  over ONE aging store, which is exactly why BOTH brackets gate (a
  single-order delta on an aging store is an ordering artifact). Across
  runs, the unit of repetition is the whole reset (fresh format), never
  a re-run over the aged fleet.
* **The oracle is WARM on the field**: external-mounts mode runs the fsck
  findings/C8-drift oracle on the LIVE authority (the leg has no remount
  recipe for an external fleet). The local fleet leg's oracle is
  cold-remounted; on the field, cold verification is the next reset's
  fresh mount reading the same volumes. `fsck findings: 0` and
  `meta_kv_block_refs_drift: 0` are still hard gates either way.
* **What a red row means**: the leg exits nonzero naming the failing gate
  (sustained-window decay, a bracket < 0.8×, an engagement column, or
  fsck/C8). Per the repro-port mandate, a field-found product failure
  comes home as a cargo repro with its fix — capture the whole rowdir
  plus `/scratch/tmp/logs/sqz-mw-*.log` before tearing down.
