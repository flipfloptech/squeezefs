# Cloud Benchmarking — occasional AWS spot clusters

`tests/cloud_bench_cluster.sh` stands up, exercises, and tears down an
occasional credible SqueezeFS benchmark cluster on AWS EC2 **spot**
instances, in the house style of `tests/cluster_reset.sh` (config block up
top, loud failures, typed-YES confirmations for costly/destructive steps,
verified state transitions, idempotent steps). This page is the operator
guide plus the labeling rules that make cloud rows admissible evidence.

## Prerequisites

- **aws cli v2**, configured (`aws sts get-caller-identity` works) with EC2
  permissions in the target region: create-fleet/run-instances, security
  groups, launch templates, placement groups, tags, and SSM parameter read
  (AMI lookup).
- **EC2 key pair** in the region (`KEY_NAME`) and its private key file
  (`SSH_KEY_FILE`) on the operator box.
- **Spot vCPU quota** — "All Standard (A, C, D, H, I, M, R, T, Z) Spot
  Instance Requests": the `i4i` preset needs 96 vCPUs, `i3en` needs 288.
- **Pre-built artifacts** in `ARTIFACT_DIR` (the script *deploys*, it never
  builds): `squeezefs` (glibc must fit the Ubuntu 24.04 AMI — use the
  `task build:ubuntu2404` dist output), optionally `libsqueezefs_il.so`, and
  a **dynamic** `elbencho` (house rule: the pinned static build cannot load
  the il shim, so any instrument that may run il must be dynamic).
- A **default VPC** in the target AZ (its default subnet's auto-assigned
  public IPs are how SSH reaches the nodes; the NVMe-oF fabric rides the
  private IPs inside the security group).

## Cost table (planning numbers — the max-spend guard is the protection)

| preset | instance        | cluster (×6) | instance store / node    | est. spot cost | venue |
|--------|-----------------|--------------|--------------------------|----------------|-------|
| `i4i`  | `i4i.4xlarge`   | 96 vCPU      | 1 × 3,750 GB Nitro NVMe  | ~$3–4/hr       | IOPS |
| `i3en` | `i3en.12xlarge` | 288 vCPU     | 4 × 7,500 GB NVMe        | ~$10–14/hr     | throughput |

Default roles: **2 mds + 2 oss + 1 client + 1 spare** (spare = job worker /
second load generator), counts configurable via `N_MDS`/`N_OSS`/`N_CLIENT`/
`N_SPARE`.

**No burst-class instances (t2/t3/t3a/t4g), ever** — the script refuses
them. CPU-credit throttling makes a median a function of the credit balance
rather than the code under test, their network baseline is burst-shaped the
same way, and they have no instance store. Any row measured on one would be
INVALID under the labeling discipline.

## The one-command happy path

```bash
MAX_CLUSTER_HOURS=3 tests/cloud_bench_cluster.sh full
```

`full` = launch → deploy → assemble → bench → teardown. Step-by-step
subcommands (`launch`, `deploy`, `assemble`, `bench`, `status`, `teardown`)
exist for iterating; every one of them takes `--dry-run`, which prints the
exact aws/ssh commands with canned query results and needs no credentials.

Guard rails:

- **Max-spend guard:** `launch`/`full` refuse without `MAX_CLUSTER_HOURS`
  (positive integer). Launch installs a detached **teardown-at-deadline**
  process the moment instances exist; `status` shows the countdown.
- **Failure trap:** a failure mid-`launch` or mid-`full` best-effort
  tears the cluster down before exiting.
- **Teardown is idempotent and tag-driven** (`squeezefs-bench=<cluster-id>`
  on every resource): it works from tags alone (`--cluster-id`), so a lost
  state dir never strands a billing resource. It ends with a **final sweep
  that fails loudly** listing anything still billing.
- **Typed YES** is required for launch (cost) and assemble/teardown
  (destructive); `--yes` exists for the deadline guard and traps.

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
3. Client instance store is formatted and mounted at `/scratch` (cache +
   staging dirs), then `squeezefs format … --disk-cache-paths`
   and `squeezefs mount … --daemon --interception --allow-other`.
4. **build_commit ritual:** artifact sha256 is verified on every node at
   deploy, and after mount the `.stats` `build_commit` must appear in the
   deployed binary's `--version` — a mismatch fails the assemble.

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

## Substrate-labeling rules (what makes a cloud row admissible)

- **Substrate label = `aws-spot/<instance-type>/<AZ>`** — instance type and
  AZ are part of the row, always, plus the spot market designation. Every
  row is also stamped with **instrument** (elbencho version, dynamic),
  **venue**, **order**, cluster id, and timestamp (the standing
  instrument-alignment lesson: every measurement states its instrument —
  and here, its substrate).
- Cloud rows are a **third substrate class**, next to the devsub `loop` and
  `tcp` substrates of the two-substrate rule. Never mix cloud numbers into
  devsub medians or vice versa; a cross-substrate delta is scoping evidence,
  not acceptance.
- **Spot interruption = restart the count.** The script polls instance
  states between rows and in a background monitor; any instance leaving
  `running` mid-battery ABORTS the run, writes
  `COUNT-ABORTED-SPOT-INTERRUPTION.txt` into the results dir, and exits
  nonzero. Per the multi-run discipline, the interrupted run is INVALID and
  the count restarts from zero on a fresh cluster — partial results are
  never silently spliced. (Launch uses capacity-optimized spot allocation
  precisely to make interruptions rare.)
- Placement is a **cluster placement group** (same-rack networking) — note
  it if you deviate, since fabric latency is part of the substrate.

## Teardown and billing hygiene

`teardown` terminates instances, deletes the security group (with
ENI-detach retries), launch template, and placement group — all located by
tag/name, safe to run twice — cancels the deadline guard, and finishes with
the loud sweep. If the sweep finds anything, the script **fails** and names
the resources; remove them and re-run teardown to re-verify. It also warns
about *other* `squeezefs-bench`-tagged clusters still running in the region.

Local state lives in `.cloud-bench/<cluster-id>/` (gitignored); torn-down
clusters are renamed `<cluster-id>.torn-down`.
