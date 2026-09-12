# Cloud Benchmarking — occasional AWS clusters

`tests/cloud_bench_cluster.sh` stands up, exercises, and tears down an
occasional credible SqueezeFS benchmark cluster on AWS EC2 — **on-demand
instances by default** (`MARKET=spot` opts into the spot discount for
uncounted, exploratory sessions), in the house style of
`tests/cluster_reset.sh` (config block up top, loud failures, typed-YES
confirmations for costly/destructive steps, verified state transitions,
idempotent steps). This page is the operator guide plus the labeling rules
that make cloud rows admissible evidence.

**Every launch needs the project owner's expressed approval for that run**
(the standing order in [AGENTS.md](../AGENTS.md) → *Cloud runs require
EXPRESSED approval*): the free local fleet probe comes first, and the cloud
venue exists only for the final sustained verdict.

## Prerequisites

- **aws cli v2**, configured (`aws sts get-caller-identity` works) with EC2
  permissions in the target region (`AWS_REGION`, default `us-east-1`;
  `AWS_AZ`, default `us-east-1a`): create-fleet/run-instances, security
  groups, launch templates, placement groups, tags, and SSM parameter read
  (AMI lookup).
- **EC2 key pair** in the region (`KEY_NAME`, default `squeezefs-bench`)
  and its private key file (`SSH_KEY_FILE`, default
  `~/.ssh/squeezefs-bench.pem`) on the operator box. SSH runs with
  `IdentitiesOnly=yes`: an agent holding more keys than sshd's
  `MaxAuthTries` would otherwise never offer the bench key (2026-09-12: a
  launch stalled at 6/7 nodes on "Too many authentication failures").
- **vCPU quota** in the market you launch in — the `i4i` preset needs 96
  vCPUs, `i3en` 288, `mw` 32 (for `MARKET=spot` that is the "All Standard
  (A, C, D, H, I, M, R, T, Z) Spot Instance Requests" quota).
- **Pre-built artifacts** in `ARTIFACT_DIR` (the script *deploys*, it never
  builds): `squeezefs` (glibc must fit the AMI — `task build:ubuntu2404` →
  `dist/ubuntu2404/` for the Ubuntu 24.04 presets; the `mw` preset rides
  Ubuntu 26.04 and defaults to `dist/ubuntu2604/` from
  `task build:ubuntu2604`), optionally `libsqueezefs_il.so`, and a
  **dynamic** `elbencho` (`ELBENCHO_BIN`, default `$ARTIFACT_DIR/elbencho`;
  house rule: the pinned static build cannot load the il shim, so any
  instrument that may run il must be dynamic). The `mw` preset's instrument
  is the pinned ior the s11-mpiio leg builds on the client — elbencho is
  optional there.
- A **default VPC** in the target AZ (its default subnet's auto-assigned
  public IPs are how SSH reaches the nodes; the NVMe-oF fabric rides the
  private IPs inside the security group). SSH ingress is limited to
  `OPERATOR_CIDR` (auto-detected when unset).

## Presets and cost (planning numbers — the max-spend guard is the protection)

| preset | instance | cluster | instance store / node | est. cost | venue |
|--------|----------|---------|-----------------------|-----------|-------|
| `i4i` (default) | `i4i.4xlarge` | ×6 (96 vCPU) | 1 × 3,750 GB Nitro NVMe | ~$12–17/hr on-demand, ~$3–4/hr spot | IOPS |
| `i3en` | `i3en.12xlarge` | ×6 (288 vCPU) | 4 × 7,500 GB NVMe | ~$30–45/hr on-demand, ~$10–14/hr spot | throughput |
| `mw` | `i4i.2xlarge` | ×4 (32 vCPU) | 1 × 1,875 GB Nitro NVMe | ~$2.7–2.8/hr on-demand, ~$0.5–0.8/hr spot | multi-writer MPI-IO (`s11-mpiio`) |
| `custom` | `INSTANCE_TYPE` verbatim | — | — | — | (still burst-class-refused) |

Roles are preset-dependent (an explicit `N_MDS`/`N_OSS`/`N_CLIENT`/`N_SPARE`
always wins): `i4i`/`i3en`/`custom` = **2 mds + 2 oss + 1 client + 1 spare**
(spare = job worker / second load generator; the battery drives from
`client0`); `mw` = **1 mds + 2 oss + 1 client**, no spare — the whole
multi-writer fleet (1 authority + `MW_COWRITERS` co-writer daemons, default 2,
plus the ior ranks) is co-located on `client0`. The `mw` preset honours
`INSTANCE_TYPE` as an override (`i4i.4xlarge`/`i4i.8xlarge` for a bigger
co-located fleet; ~$11/hr on-demand at `i4i.8xlarge`).

The default flipped from spot to on-demand on 2026-08-19: a spot reclaim 20
minutes into a counted `mw` session aborted the count, and determinism is
worth the ~2–3× hourly premium at these cluster sizes. The market is stamped
into every row label either way.

**No burst-class instances (t2/t3/t3a/t4g), ever** — the script refuses
them. CPU-credit throttling makes a median a function of the credit balance
rather than the code under test, their network baseline is burst-shaped the
same way, and they have no instance store. Any row measured on one would be
INVALID under the labeling discipline.

## The one-command happy path

```bash
MAX_CLUSTER_HOURS=3 tests/cloud_bench_cluster.sh full            # i4i preset
MAX_CLUSTER_HOURS=3 PRESET=mw tests/cloud_bench_cluster.sh full  # multi-writer MPI-IO arm
```

`full` = launch → deploy → assemble → bench → teardown (with `PRESET=mw`:
launch → deploy → assemble-mw → bench-mw → teardown). Step-by-step
subcommands (`launch`, `deploy`, `assemble`, `assemble-mw`, `bench`,
`bench-mw`, `status`, `teardown`) exist for iterating; every one of them
takes `--dry-run`, which prints the exact aws/ssh commands with canned query
results and needs no credentials. `--cluster-id ID` targets a specific
cluster (default: the one recorded in `.cloud-bench/current`); `--preset P`
overrides `PRESET`. Pass the preset on **every** subcommand of an `mw`
cluster — it selects the role shape, the AMI and the artifact defaults, and
only the node list persists in cluster state.

Guard rails:

- **Max-spend guard:** `launch`/`full` refuse without `MAX_CLUSTER_HOURS`
  (positive integer). Launch installs a detached **teardown-at-deadline**
  process the moment instances exist; `status` shows the elapsed
  cluster-hours, the estimated spend and the countdown.
- **Failure trap:** a failure mid-`launch` or mid-`full` best-effort
  tears the cluster down before exiting.
- **Teardown is idempotent and tag-driven** (`squeezefs-bench=<cluster-id>`
  on every resource): it works from tags alone (`--cluster-id`), so a lost
  state dir never strands a billing resource. It ends with a **final sweep
  that fails loudly** listing anything still billing.
- **Typed YES** is required for launch (cost) and assemble/assemble-mw/
  teardown (destructive); `--yes` exists for the deadline guard and traps.

## What assemble builds

Mirror of `tests/cluster_reset.sh` over the product verbs, with the cloud
deltas stated where they happen:

1. Storage nodes discover their **instance-store** NVMe namespaces (model
   "Amazon EC2 NVMe Instance Storage"; EBS devices are excluded) and share
   them via `squeezefs nvmeof share --target-stack nvmet` on the private IP.
   mds nodes share one namespace; oss nodes share all of theirs.
2. The client connects each subsystem — **single-path** (cloud instances
   have one NIC), so the dual-path connect loop and the round-robin
   `iopolicy` write from `cluster_reset.sh` are deliberately absent.
3. Client instance store is formatted (ext4) and mounted at `/scratch`
   (cache + staging dirs), then `squeezefs format … --disk-cache-paths`
   and `squeezefs mount … --daemon --interception --allow-other`.
4. **build_commit ritual:** artifact sha256 is verified on every node at
   deploy, and after mount the `.stats` `build_commit` must appear in the
   deployed binary's `--version` — a mismatch fails the assemble.

`assemble-mw` (`PRESET=mw`) runs the same fabric steps and diverges at the
mount into the `tests/cluster_reset_v5_mw.sh` multi-writer recipe: 1
authority at `/scratch/mnt` plus `MW_COWRITERS` co-writer mounts
(`/scratch/mnt-cw1..K`) on `client0`, kernel FUSE (no `--interception`),
`SQUEEZEFS_RANGE_CUSTODY` armed on every co-writer (the s11-mpiio row
requires it; the product default stays off), a stable authority port
(`MW_PORT`, default 45999), an nvmet Persistent-Reservation assert per storage
node, and both MW kernel floors (client FUSE-over-io_uring, storage-node nvmet
PR) probed loud — the Ubuntu 24.04 GA kernel lacks both, which is why the
preset defaults to the 26.04 AMI.

## The benchmark battery

Runs from `client0`; results are captured (via ssh tee + per-row `.stats`
snapshots) into **`.benchmarks/cloud/<timestamp>/`** on the operator box,
with a `manifest.txt` and the daemon log.

| order | row | shape |
|---|---|---|
| 1–4 | `seq-write-1m-relaxed` vs `seq-write-1m-durable-sync` | **A-B-B-A bracket** (RW6 durability-leveled pair; also primes the f-files) |
| 5 | `seq-read-1m` | 16t, O_DIRECT |
| 6 | `rand-read-1m` | 16t, O_DIRECT |
| 7 | `rand-read-4k-t32qd32` | 32t, iodepth 32 |
| 8 | `seq-read-4k` | 16t |
| 9 | `rand-write-4k-overwrite-primed` | 32t qd32, **overwrites the primed files** (the sole-owner-patch/overwrite venue) |
| 10 | `seq-write-4k` | fresh files |
| 11 | `cleanup-delete` | elbencho `-F` |

An `abba_bracket` helper is exported by the battery for any additional A/B:
per the standing comparison rule, any A/B whose shared store ages across
runs must run **both orders** and cite both brackets.

`bench-mw` (`PRESET=mw`) runs one row instead: the **s11-mpiio
shared-vs-disjoint** ior row (`tests/run_mw_matrix.sh s11-mpiio` in
external-mounts mode, `MW_IOR_PROCS` ranks per mount, default 4) over the
assembled fleet, into the same results layout.

## Substrate-labeling rules (what makes a cloud row admissible)

- **Substrate label = `aws-<market>/<instance-type>/<AZ>/pg-<placement>`** —
  market (`on-demand`/`spot`), instance type, AZ and placement strategy are
  part of the row, always. Every row is also stamped with **instrument**
  (elbencho version, dynamic — or the pinned ior for the mw row), **venue**,
  **order**, cluster id, and timestamp (the standing instrument-alignment
  lesson: every measurement states its instrument — and here, its
  substrate).
- Cloud rows are a **third substrate class**, next to the devsub `loop` and
  `tcp` substrates of the two-substrate rule. Never mix cloud numbers into
  devsub medians or vice versa; a cross-substrate delta is scoping evidence,
  not acceptance.
- **An instance leaving `running` = restart the count.** The script polls
  instance states between rows and in a background monitor (20 s cadence);
  any instance leaving `running` mid-battery ABORTS the run, writes
  `COUNT-ABORTED-SPOT-INTERRUPTION.txt` into the results dir, and exits
  nonzero. Per the multi-run discipline, the interrupted run is INVALID and
  the count restarts from zero on a fresh cluster — partial results are
  never silently spliced. On-demand instances are not reclaimed, which is
  why they are the default; `MARKET=spot` launches use capacity-optimized
  allocation to make interruptions rare.
- Placement is a **cluster placement group** by default (same-rack
  networking; `PLACEMENT_STRATEGY=cluster`). `partition`, `spread` and
  `none` (no placement group — the fallback when the cluster pool is dry;
  same-AZ networking only) are accepted, and the row label carries the
  strategy in force, since fabric latency is part of the substrate.

## Teardown and billing hygiene

`teardown` cancels the deadline guard, terminates instances, deletes the
launch template, the security group (with ENI-detach retries) and the
placement group — all located by tag/name, safe to run twice — and finishes
with the loud sweep. If the sweep finds anything, the script **fails** and
names the resources; remove them and re-run teardown to re-verify. It also
warns about *other* `squeezefs-bench`-tagged clusters still running in the
region.

Local state lives in `.cloud-bench/<cluster-id>/` (gitignored); torn-down
clusters are renamed `<cluster-id>.torn-down`.
