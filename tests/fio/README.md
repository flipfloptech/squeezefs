# tests/fio — the house fio canon

**fio is the house instrument for all field rows** (user directive
2026-07-31: "The lustre benchmarks are ran with fio so fio makes the
most sense to do all testing with from here out"). elbencho remains
informal-only. Every field/benchmark row cites the job file it ran, the
dims it ran it with, and the venue labels below — that is what makes a
`.benchmarks/` note reproducible.

## The engine policy (user ruling 2026-08-07)

`.benchmarks/2026-08-07-fio-engine-policy.md` is the record; the rules:

1. **Every throughput/IOPS row is `ioengine=libaio` + `direct=1` + a
   stated iodepth — BOTH lanes** (kernel and il/shim). il libaio rows
   ride the v1.1 aio interposers
   (`.benchmarks/2026-07-19-v1.1-libaio-interposers.md`); each row's
   engagement gate checks the counters ITS lane actually moves.
2. **Any A/B comparison uses the SAME engine both sides** (the
   matched-instrument law) — never kernel-libaio vs shim-psync.
3. **psync survives only as explicitly-labeled sync-lane coverage rows**
   (the §5.5.1 sync fast path as measurand; e.g. the session-FLEET rigs,
   whose 256-process shape the single-slot aio lane cannot express) —
   never a headline, never cross-lane compared.
4. **Buffered rows never silently use libaio** (without O_DIRECT it
   degrades to sync) — a buffered row states its engine choice.
5. **`ioengine=io_uring` is a labeled kernel-lane-only extra** — the
   shim cannot interpose io_uring, so it never appears on il rows.

Shim-lane geometry law: the v1.1 aio interposers are single-slot — an
iocb with `nbytes >` the session slot slab (`min(arena/slots,
max_op_bytes)`; 64 KiB at the derived arena floor, slots = 1024) rides
the kernel lane by design. A bs=1M il libaio row therefore needs the
daemon mounted with `SQUEEZEFS_IPC_ARENA_MB >= 1024` (slab = 1 MiB); the
engagement verdict makes any silent passthrough exit nonzero.

## The runner

`tests/fio/run_fio_row.sh` turns a `[global]`-shape job file into a
labeled row:

* **Topology-general NUMA fan-out** — discovers the box's NUMA nodes at
  runtime and appends one fio `[section]` per node with
  `numa_cpu_nodes=`/`numa_mem_policy=bind:` (works on N domains, not a
  hardcoded 2; a single-node box gets one section and **no** numa
  lines). `--njobs` is the TOTAL across nodes, split evenly.
  `--no-numa` disables the fan-out (e.g. fio built without libnuma).
* **Path selection** — kernel FUSE (`--dir` only), interception shim
  (`--shim <libsqueezefs_il.so>`: LD_PRELOAD applied to the fio process
  only), or raw devices (`--devices /dev/a:/dev/b`: one section per
  device, `SQZ_FIO_NJOBS_PER_DEV` jobs each, default 8).
* **Engagement verification** (charter rule 4) — a `--shim` row with
  `--mount <mountpoint>` diffs the stats inode around the run;
  `ipc_ops_read + ipc_ops_write` must account for ≥
  `SQZ_FIO_ENGAGE_MIN` (default 0.90) of the row's ios or the runner
  **exits nonzero** — a silent-passthrough row can never be quoted.
* **Destructive guard** — a write-class job pointed at `--devices`
  refuses to run without `--i-know-this-destroys-data`.
* **Artifacts** — the GENERATED job file (with the emitted sections),
  fio JSON, stats before/after/delta, and `meta.json` (all labels +
  dims + instrument version) under `--results` (default
  `/tmp/fio_rows/<timestamp>_<label>`). `--journal <file>` appends
  START/DONE lines (client etiquette: on shared boxes journal to the
  agreed log).

## The jobs

| Job | Shape | Purpose |
|---|---|---|
| `exa_write_bw.job` | seq write, libaio, bs=1M, qd=8, direct=1, time_based 30s + 10s ramp, size=1g, njobs=nproc NUMA-split | The EXAScaler client-validation write-BW shape — the reference-comparable "what does a Lustre validation run see" row |
| `exa_read_bw.job` | seq read, same dims | The EXA read-BW shape; prefill with `exa_write_bw.job` first (same `filename_format` ⇒ same files) |
| `exa_randwrite_iops.job` | randwrite, bs=4k fixed, otherwise same | The EXA random-write IOPS shape |
| `exa_randread_iops.job` | randread, bs=4k fixed, otherwise same | The EXA random-read IOPS shape (prefill first) |
| `gap_probe_write.job` | seq write, ALL dims parametrized | The gap-accounting sweep probe: `iodepth {1,4,8,32} × bs {1M,4M} × njobs {8,16,32}` — the dimension that recovers throughput names the bottleneck class (depth ⇒ latency chain; njobs ⇒ per-stream/queue concentration; bs ⇒ per-op overhead) |
| `gap_probe_read.job` | seq read, ALL dims parametrized | Read-side sibling of the sweep probe |
| `fresh_write_pass.job` | seq write, ONE pass (not time_based), dims parametrized | Fresh-ingest face (raw-ceiling-comparable); rm + settle before each rep, label pass-bound |
| `raw_ceiling_write.job` | seq write, bs=4m, qd=16, 8 jobs/device, `filename=` per device | **DESTRUCTIVE** device-sweep raw write ceiling — runner refuses without `--i-know-this-destroys-data`; never against a live volume set |
| `raw_ceiling_read.job` | seq read, same dims | Device-sweep raw read ceiling (read-only) |

Defaults (override per row): `--engine libaio --bs 1M --iodepth 8
--size 1g --runtime 30 --ramp 10 --njobs $(nproc)`. The EXA-parity dims
are the defaults on purpose — a bare invocation of an `exa_*.job` IS
the EXA shape.

## Venue-labeling rules (a row without these is not evidence)

Every quoted row states, per the standing measurement rules in
AGENTS.md:

1. **Instrument** — fio + version (the runner prints/persists it).
   fio libaio through the shim rides the il libaio interposers; verify
   engagement (the runner does).
2. **Shape** — the job file + dims (`bs/qd/njobs/engine/runtime`), and
   the path (kernel / shim / raw).
3. **Substrate** — `--substrate`: which box, which devices, loop vs tcp
   (two-substrate rule: loop-only results on fabric-sensitive rows are
   scoping, never acceptance), thermal/governor posture if relevant.
4. **Fill** — `--fill`: fresh format vs prefilled vs rewrite-aged; for
   reads: what wrote the data and when; for raw devices: thin/zeroed
   state.
5. **Order** — `--order`: rep number and bracket position. Comparisons
   over an aging store run A-B-B-A (both orders); headline claims
   include a ≥ 60 s sustained row (`--runtime 60` or more, throughput
   flat across the window).

Write rows additionally carry the standing amplification columns:
`--data-devs nvme4n1:nvme6n1:..` makes the runner diff
`/proc/diskstats` across the row and print `write_amp`/`read_amp`
(device bytes ÷ user bytes). Caveat: with `ramp_time > 0` the fio JSON
excludes ramp I/O while diskstats includes it, so the printed ratio
overestimates by roughly `(runtime+ramp)/runtime` — use `--ramp 0` when
the amp column is the row's verdict, or quote `tests/write_amp_rig.sh`
(the packaged instrument, incl. `wareq-sz` and `block_free_*`). On
PASS-BOUND write rows (`fresh_write_pass.job`) the window closes at fio
exit while the ACK-before-DMA pipeline is still draining — quote amp
from sustained (time_based) rows only.

## How `.benchmarks` notes cite rows

Quote the row line the runner prints and name the job file + dims +
venue labels, e.g.:

> wril-exa: `tests/fio/exa_write_bw.job` (libaio bs=1M qd=8 njobs=32
> NUMA-split 2×16, 60 s sustained), shim path, engagement 1.00,
> substrate "field 4-node 2×200GbE nvme-tcp", fill fresh, order
> r2-of-ABBA — **write: 20.1 GB/s**.

The persisted `meta.json` + generated job file under `--results` are
the reproducibility record; copy them (or their paths) into the
evidence note's raw-artifacts line.

## Example incantations

```bash
M=/scratch/tmp/test   # the mounted filesystem
SO=/usr/local/lib/libsqueezefs_il.so

# EXA write-BW, kernel path, 60 s sustained:
tests/fio/run_fio_row.sh --job tests/fio/exa_write_bw.job \
  --dir "$M/fio" --mount "$M" --runtime 60 --label wrkern-exa \
  --substrate "field 4-node nvme-tcp" --fill fresh --order r1

# Same shape via the shim (engagement-verified):
tests/fio/run_fio_row.sh --job tests/fio/exa_write_bw.job \
  --dir "$M/fio" --mount "$M" --shim "$SO" --runtime 60 --label wril-exa

# The gap sweep (write side, shim):
for qd in 1 4 8 32; do for bs in 1M 4M; do for nj in 8 16 32; do
  tests/fio/run_fio_row.sh --job tests/fio/gap_probe_write.job \
    --dir "$M/gap" --mount "$M" --shim "$SO" \
    --iodepth "$qd" --bs "$bs" --njobs "$nj" \
    --label "gap-w-qd${qd}-bs${bs}-nj${nj}"
done; done; done

# Raw read ceiling on two idle devices:
tests/fio/run_fio_row.sh --job tests/fio/raw_ceiling_read.job \
  --devices /dev/nvme1n1:/dev/nvme2n1 --label raw-read

# Raw WRITE ceiling (destroys the devices' contents!):
tests/fio/run_fio_row.sh --job tests/fio/raw_ceiling_write.job \
  --devices /dev/nvme1n1:/dev/nvme2n1 --label raw-write \
  --i-know-this-destroys-data
```

## The EXA client perf battery (`exa_client_perf.sh`)

Our version of the EXA client perf script: one command runs the four
EXA-parity rows kernel-path first, then through the shim, and prints
the results side-by-side (aligned table + venue header; persisted to a
timestamped report under the artifacts dir). Execution rides
`run_fio_row.sh` verbatim — NUMA fan-out, engagement verification, amp
columns, and labels all come from the runner.

* **KD-7 screen**: refuses loud before any row when the shim does not
  embed the mounted daemon's `build_commit`.
* **Fairness, visible by construction** (2026-08-02 il-anomalies
  window): psync is structurally qd=1, so psync shim passes scale njobs
  to `njobs x qd` by DEFAULT so offered in-flight matches the kernel
  pass (`--no-match-inflight` restores raw njobs); the per-pass
  **in-flight figure prints in the table** either way. Read passes are
  COLD only via `--allow-remount` (captured daemon cmdline+env, same
  binary, restore-verified) or when the fileset >= 2x the R5
  `mem_budget_bytes` (cold-by-overflow); un-guaranteed read passes
  print `WARM (label-only)` — never a bare number.
* **Per-pass engine law** (labeled in the table): kernel pass = libaio
  qd=8 (EXA canon); shim pass = psync for bs=1M rows / libaio for bs=4k
  rows (measured 2026-08-02: bs=1M libaio through the shim = engagement
  0.000 — those ops ride the kernel lane). A shim pass failing the
  runner's engagement check is printed `INVALID (passthrough)`.
* **Budget hint**: `ipc_bind_refused_budget` growth during a shim pass
  prints the fix (pre-eb94f0c binaries: raise `SQUEEZEFS_IPC_MEM_MAX`
  on the daemon; later binaries derive the cap).
* `--sustain` lifts rows to 60 s per the sustain law (the report states
  which mode ran); `--rows` subsets; `--emit-only` prints the plan.

Sample table (field smoke, 12 s SMOKE-labeled rows):

```
row        bs  kernel (engine)          shim (engine)              delta  shim engagement        verdict
--------------------------------------------------------------------------------------------------------
write_bw   1M    31.87 GB/s (libaio)      30.18 GB/s (psync)       -5.3%  0.935 (>=0.90 OK)      shim OK
               kernel: in-flight 256 (njobs 32) | clat_mean 8.334 ms | write_amp 1.804 (...)
               shim: in-flight 256 (njobs 256) | clat_mean 8.781 ms | write_amp 1.714 (...)
randread   4k    231.3 kIOPS (libaio)     212.2 kIOPS (libaio)     -8.3%  1.812 (>=0.90 OK)      shim OK
               kernel: in-flight 256 (njobs 32) | clat_mean 1.103 ms | cold (remount, restore-verified) | ...
               shim: in-flight 256 (njobs 32) | clat_mean 1.206 ms | cold (remount, restore-verified) | ...
```

```bash
# Full battery, sustained, cold reads via remount, amp columns, journaled:
tests/fio/exa_client_perf.sh --mount "$M" --shim "$SO" --sustain \
  --allow-remount --data-devs nvme4n1:nvme6n1:nvme8n1:nvme10n1 \
  --substrate "field 5-wide nvme-tcp" --fill "fresh set this session" \
  --journal /scratch/tmp/agent_runs.log

# Just the BW pair, quick:
tests/fio/exa_client_perf.sh --mount "$M" --shim "$SO" --rows write_bw,read_bw
```
