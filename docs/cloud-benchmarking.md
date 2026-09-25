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
  vCPUs, `i3en` 288, `mw` 32, `mw` + `SYMMETRIC=1` 8 × every node =
  8 × (N_MDS + N_OSS + N_CLIENT + N_SPARE): at the default 1 mds + 2 oss +
  0 spare S1 (`N_CLIENT=3`) 48, S2 (`N_CLIENT=8`) 88; 136 at S2 +
  `N_OSS=8` (for `MARKET=spot` that is the "All Standard (A, C, D, H, I, M,
  R, T, Z) Spot Instance Requests" quota).
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
| `mw` + `SYMMETRIC=1 N_CLIENT=n` | `i4i.2xlarge` | ×(N_MDS + N_OSS + n + N_SPARE) — at the default 1 mds + 2 oss + 0 spare: S1 n=3: ×6 (48 vCPU), S2 n=8: ×11 (88 vCPU); S2 + `N_OSS=8` (the 2026-09-24 approved shape): ×17 (136 vCPU) | 1 × 1,875 GB Nitro NVMe | ~$0.686/hr per node on-demand, **every node bills**: S1 ~$4.1/hr, S2 ~$7.5/hr, S2 + 8 oss ~$11.7/hr (the rig's `EST_CLUSTER_HOURLY` prices `N_MDS + N_OSS + N_CLIENT + N_SPARE` — the 2026-09-24 run's typed-YES line had priced 11 of its 17 nodes) | symmetric gates 2 / 3 / 3b on N real nodes (PR 15) |
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
launch → deploy → assemble-mw → bench-mw → teardown; with `PRESET=mw
SYMMETRIC=1`: … → assemble-sym → bench-sym → teardown). Step-by-step
subcommands (`launch`, `deploy`, `assemble`, `assemble-mw`, `assemble-sym`,
`bench`, `bench-mw`, `bench-sym`, `status`, `teardown`) exist for iterating; every one of them
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
5. **Apt hygiene (deploy):** on every node `deploy` takes Ubuntu's
   unattended apt off the session — `systemctl stop unattended-upgrades
   .service` first (its stop handler waits for a running upgrade child: the
   graceful drain — a refused stop is reported loud and the hygiene
   continues), then the `apt-daily*` timers stopped, disabled and masked and
   the upgrader masked; a LIVE apt/dpkg transaction is detected by its LOCKS
   (`fuser` on `/var/lib/dpkg/lock*` + the apt locks), by the oneshot units'
   `activating` state, and by the UPGRADER's command line (`pgrep -f` on
   `unattended-upgrade` — never the 15-char comm, which the always-running
   `unattended-upgrade-shutdown --wait-for-signal` waiter shares, and never
   `unattended-upgrades.service`'s state, which is `active` on every booted
   Ubuntu node) — and waited for, bounded, then the deploy dies loud naming
   the node; dpkg is never killed. **The bound, honestly:** the drain's
   `systemctl stop` blocks for up to the unit's own `TimeoutStopSec`
   (1800 s on Ubuntu's `--wait-for-signal` unit) while a live upgrade
   finishes, THEN the lock loop's `APT_UPGRADE_WAIT_MAX_S` (600 s) — a
   worst case of 40 min per node, paid only when an upgrade is genuinely
   mid-flight; seconds per quiet node. The 2026-09-24 run met an unattended
   apt run during the session (the resume note lists the fix; what the run
   itself did is in no surviving source). The node script and its pin
   (every arm of the busy probe pinned on its own — a deleted arm goes RED):
   `tests/cloud_bench_node_scripts.sh`, `tests/cloud_bench_cluster_units.sh`.
6. **Idempotent re-assemble:** the fabric script's `format` passes
   `--force` — a re-assemble meets the previous assemble's superblock
   (`wipefs -a` on the storage node does not know the METALV01 magic) and
   every assemble is a declared reformat; `--force` keeps `format`'s
   live-client refusal. **That refusal has a second, correct face:** a
   mount that died WITHOUT a clean leave (a kill, a crashed node) keeps a
   heartbeat-fresh `writer_claim` / `client:` record for
   `CLIENT_STALE_TTL_SECS` (45 s, `src/fuse_client.rs`), and a re-assemble
   inside that window is refused "actively mounted by clients" — by design,
   not the superblock problem above. Wait the TTL out (the prologue's
   unmount + disconnect does not shorten it) and re-run.

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

## The symmetric fleet shape (`PRESET=mw SYMMETRIC=1` — PR 15, the program's only multi-node venue)

Every symmetric row so far ran **co-located** (one box, N daemons sharing
its cores — the laptop and squeeze-test), so design §8 gate 3's law
("aggregate create/s and ingest scale with N, **bounded by no node**") was
judged on creates per daemon-CPU-second
([design §8 row 3](design-symmetric-metadata.md#8-performance-gates--evidence-tiers),
acceptance record §3.9.3). The cloud row puts **one symmetric writer per
node** on a real nvme-tcp fabric and reads the law as written. `SYMMETRIC=1`
(or `--symmetric`) under `PRESET=mw` makes `N_CLIENT` the **writer node
count** (default 2; `N_MDS=1 N_OSS=2` as today; `INSTANCE_TYPE` override
honoured; the burst-class refusal untouched). Pass `PRESET=mw SYMMETRIC=1
N_CLIENT=<n>` on **every** subcommand:

```bash
MAX_CLUSTER_HOURS=4 PRESET=mw SYMMETRIC=1 N_CLIENT=3 tests/cloud_bench_cluster.sh launch
PRESET=mw SYMMETRIC=1 N_CLIENT=3 ARTIFACT_DIR=dist/ubuntu2604 tests/cloud_bench_cluster.sh deploy
PRESET=mw SYMMETRIC=1 N_CLIENT=3 tests/cloud_bench_cluster.sh assemble-sym
PRESET=mw SYMMETRIC=1 N_CLIENT=3 SYM_TAR_SRC=<linux>/fs tests/cloud_bench_cluster.sh bench-sym
tests/cloud_bench_cluster.sh teardown
```

| shape | `N_CLIENT` | nodes | rows | est. on-demand |
|---|---|---|---|---|
| **S1** | 3 | 6 × i4i.2xlarge | gate 2 + gate 3b (+ `-ls`) + gate 3 at N ≤ 2 | ~$4.1/hr |
| S1' | 2 | 5 × i4i.2xlarge | gates 2 + 3 (N ≤ 2) only — `SYM_ROWS=tarx,scale` (gate 3b needs ≥ 2 **joined** writers beside the manager: the flip triggers on foreign creates from more than one creator) | ~$3.4/hr |
| **S2** | 8 | 11 × i4i.2xlarge | gate 3's full N = 1/2/4/8 ladder + gates 2 / 3b | ~$7.5/hr |

**`assemble-sym`** runs `assemble-mw`'s fabric steps (the FUSE-over-io_uring
floor on **every** client node, the prologue on every client node, the
storage shares + the nvmet PR assert) and diverges at two points: the
manager's node formats with **`format --symmetric`, CACHE-LESS** (no
`--disk-cache-paths` — the shape `tests/mw_fleet.sh` formats the box and
local fleets with, so every beyond-inline file is a whole striped block at
its publish, visible to every node, and gate 2 prices the same whole-block
DMA the box priced; the mw preset's staged format is the co-located
`s11-mpiio` fleet's, not this one's; the other client nodes connect only —
the meta URI is the set's on every node); and the mount is
**one symmetric writer per client node** through the join ladder
(`SQUEEZEFS_SYMMETRIC_META=1`, no posture knob — `SQUEEZEFS_MULTI_WRITER` /
`MW_ROLE` / `MW_AUTHORITY` / `MW_MEMBERS` are retired spellings under the
plane): the **manager** on `client0` (the D0 winner; membership shard at
`auto`, listener at `<private ip>:SYM_PORT` — an explicit bind, advertised
verbatim into its claim-set entry, which every joiner's
`resolve_holder_endpoint` reads), a **joined writer** on `client1..N-1`
(the fifth door over the wire — its own ring, page, slot leases and
checkpoint task), and, with `SYM_TOKEN_READER=1` (default), a `--read-only`
**token reader** at `/scratch/mnt-ro` on `client0` (the `-ls` half of gate
3b). Every node is its **own identity**, asserted before any identity-bearing
step: **`/etc/machine-id` distinct across the client nodes** — the daemon's
node token, half of the `(node_token, mount_slot)` identity every appender
page, claim-set entry and membership record carries, is derived from it
(`src/writer_scope.rs`); a baked AMI clones the file onto every node, and
the 2026-09-24 run's first assemble "failed on the cloned `/etc/machine-id`
(= the node token)" — every joiner carried the manager's identity (derived
from the identity law; where in the join ladder it failed is not known) — a
clone is regenerated (a regular-file `/var/lib/dbus/machine-id` removed
first — `systemd-machine-id-setup` seeds from it — then the file truncated
and the setup run, the dbus copy recreated), re-read and re-asserted, a
duplicate still standing dies loud. Every node is also its
**own registrant** — the REMOTE posture: the node's
nvme-cli host identity (`/etc/nvme/hostnqn` + `hostid`), generated where
missing and **asserted distinct across the client nodes — both words**
(nvmet keys a PR registrant by the Host ID; a baked AMI clones both files
onto every node; a duplicate of either regenerates both), and after the
mounts the DEVICE's own answer is gated: **`nvme resv-report -e` on every
namespace of the set lists exactly N distinct Host IDs** (the manager holds
and registers, each REMOTE joiner registers under its own host identity,
the reader registers nothing — two nodes aliasing as one host, or a joiner
that adopted instead of registering, read N − 1); the manager's
`pr_registrants_per_namespace` (its own REGCTL read, refreshed on the 10 s
guard heartbeat for the metadata namespaces) is printed beside it as the
record. No `SQUEEZEFS_FLEET_SHARE` —
one daemon per node owns its machine, which is the point of the venue. The
mw preset's node packages gain `attr` (`getfattr` reads the striped
directory's `user.squeezefs.stripes`); the deploy resolves every tool the
rows invoke by name and the driver preflights them on every node before
any row runs — a missing tool dies loud, never a false RED on a paid row.

The posture gates are read from every node's `.stats` (a silently-degraded
mount is contractually impossible): the manager — `mount_posture writer`,
`manager_lease held` + `symmetric_meta 1` + `writer_guard_mode flock+pr` on
every volume, `data_plane_fence_mode 1`, `membership_mode owner`,
`symmetric_join.endpoint` on the node's private ip, the ladder's
`SYMMETRIC WRITER JOINED` log line; each joiner — the joined door's log
line, `mount_posture writer`, `joined_appender_id ≥ 1`,
**`joined_registrant_posture registrant`** (`adopted` would mean the joiner
believed itself co-located with the manager — a product finding on this
venue; `detection` a non-PR namespace), `manager_lease peer:…`,
`slot_leases_held ≥ 1`, `membership_mode member`, its own endpoint
published; fleet-wide at the manager — `appenders_known == N` (the
appender directory's Live-page count; a daemon's `appenders_live` counts
only the regions it joined itself, 1 everywhere) and `membership_writers
== N − 1` (every joiner a writer member of the manager's shard;
`membership_members` also counts the token reader, so it is not the gate);
the token reader —
`reader_staleness_bound_ms == 0`; then the build_commit ritual on every
node.

**`bench-sym`** drives the three row sets from the operator's box over ssh
with `tests/cloud_sym_rows.sh` — one row driver whose laws are
`tests/sym_rows_lib.sh`, the **same file `tests/run_mw_matrix.sh` sources
for its `sym-tarx` / `sym-scale` / `sym-shared-dir` legs**, so the cloud row
and the box row read one law (the matrix's `.stats` keys and verdict text;
change a threshold there and both venues move together). The row set:

| row | shape on the fleet | law (the lib's) |
|---|---|---|
| gate 2 `sym-tarx` | `tar -x` of the shipped corpus (`SYM_TAR_SRC=<linux>/fs` — the box used `linux-7.2.3/fs`, 2,468 entries; ship the same tarball to keep rows comparable) by **client1's joined writer** into a directory it created, vs **S0 = the manager's own extract on client0** (the same binary, the same node class and fabric, the same session — the matrix's `local_arm` shape: the joiners and the token reader stay **mounted and idle**, so the manager serves their renewals and token planes during S0; NOT a solo mount, and the row says `manager-local-S0(joiners-idle-mounted)`); A-B-B-A `sym-1 local-1 local-2 sym-2`; ONE extraction per arm — the box's exact shape (gate 2 is a wall ratio, not a rate; the set is cache-less, so every beyond-inline file is a whole 4 MiB block and one `fs/` extraction is ≈ 7 GiB of data blocks — `--tarx-reps=0` fills `--rt` with fresh-subdir extractions where the 1,875 GB data namespaces hold them); the row label states the format posture; the measured node-to-node RTT stated in the row (replaces the box's netem 250 µs) | ≤ 1.10× S0; `wire_verbs_per_entry` < 0.05; `slot_handovers == 0`; `dlm_rpcs == 0` |
| gate 3 `sym-scale` | N ∈ {1,2,4,8} ∩ [1, N_CLIENT] writer **nodes** (the manager + N−1 joiners) each create `SYM_FILES` files (`SYM_THREADS` threads) in its own directory, then ingest `SYM_INGEST_MB` (4 MiB blocks, `conv=fsync`); the idle writers **leave** (`sym-hook`) so exactly N appenders are live; `C/CPU-S` beside the wall multiple; the ingest row's amplification columns from `/proc/diskstats` on the storage nodes' data namespaces (`device ÷ user bytes`, `wareq-sz`) | ≥ 0.7 × N × the N=1 rate on both rows; `appenders_known == N`; handovers 0; `slot_ships ≤ N`; `dlm_rpcs` 0; the must-stay-0 deltas 0; deleted stays deleted through the manager after every writer's clean leave and through a remounted writer |
| gate 3b `sym-shared-dir` (+ `-ls`) | every writer node creates `SYM_FILES / N` files into **one** directory the first joiner made; then the token reader's cold `ls -l` of it | `dir_stripe_flips == 1` at the holder; `dir_striped_dirs ≥ 1`; `dir_stripe_ships > 0`; `xv shipped ≡ served`; `slot_handovers == 0`; `-ls`: `dlm_token_grants ∈ [K_D + K_root + C, K_D + K_root + C + 4]` (design §8 row 3b as adjudicated 2026-09-22, PR 13d — `K_D` = the directory's stripes at the holder, `K_root` = the mount ROOT's stripes as the manager reports it: 0 while `/` is unstriped; a token reader's first `stat /` folds the root's stripes, one records-only grant each, once per token lifetime; the lib reads both through one die-loud `getfattr` and the row prints both), 0 data-leaf reads (net of the poll's re-reads + one tree-0 read per stripe slot of D and of the root), `dir_stripe_readdir_merges ≥ 1` |

Every row also carries the **acked-writes-present** law (the lib's, so the
matrix's legs read it too): the tree a writer acked — entries and bytes —
read back **identical through another mount** (the manager for a joiner's
`tar -x`, the first joiner for the manager's; another writer of the row, or
the token reader, for `sym-scale`'s trees; the manager for the shared
directory) and every fsynced `ingest.bin` read back whole through another
mount, all zero. After every row set: `squeezefs fsck <manager mount>
--json` (findings 0), `meta_kv_block_refs_drift == 0`,
`data_alloc_bitmap_drift == 0` and the must-stay-0 set on every writer. **Every RATE phase runs ≥ `SYM_RT` seconds**
(default 60 — the sustained-state rule): on the cloud venue the driver's
`--size-to-rt=auto` pilot (an N = 1 create storm + a 256 MiB ingest on the
manager, then a small shared-dir wave) sizes `--files`, `--ingest-mb` (up
to `SYM_INGEST_CAP_MB`) and the shared row's per-creator count so each
phase fills RT with 25 % headroom, and a phase that still reads shorter is
**INVALID** (the row's verdict word; the driver exits nonzero after its row
sets, every other law's evidence kept) — a burst row is a failed row, never
a warning. The local scoping pass runs `--rt=10 --size-to-rt=off` (a warn). The design's
"vs today" **A arm** (the shipped authority + co-writers at the same N on
the same binary) is **not built** in PR 15 — this rig has no per-node
co-writer recipe (the v5-mw recipe is co-located on client0) — and
`SYM_ARM_A=1` refuses loud; the B-only law rows are the minimum.

Results land in `.benchmarks/cloud/<ts>/` (the manifest) and
`.benchmarks/cloud/<ts>/sym-rows/` (`rows.txt` = the labelled rows +
verdicts; per-node `.stats` snapshots `m<idx>_p<label>{0,1}.json`; the
fsck transcripts; every node's daemon logs under `logs/<node>/`). **Every
row carries the cloud label**: instrument, `substrate=aws-<market>/<instance>/<az>/pg-<placement>`,
venue + cluster id, the measured RTT, and **per node** its kernel and
the mounted daemon's `build_commit`. A cloud row is a third substrate class
— never spliced into devsub or squeeze-test medians. Every launch needs the
owner's expressed approval for **that** run; the free local pass
(`tests/cloud_sym_rows.sh … manager=local:… writer=local:…` over a
`tests/mw_fleet.sh create N=2 --symmetric --writers=3` fleet, with
`--mount-hook="tests/cloud_sym_rows.sh fleet-hook"`) comes first.

### The 2026-09-24 symmetric run (S2 + 8 oss) — what it cost, what it found, what it taught

The first Phase B run (`.benchmarks/2026-09-24-sym-pr15-cloud-row.md`; the
acceptance record's §3.10) launched the owner-approved shape
`PRESET=mw SYMMETRIC=1 N_MDS=1 N_OSS=8 N_CLIENT=8 MAX_CLUSTER_HOURS=4` —
17 × i4i.2xlarge, us-east-1a, on-demand — at 19:02:14 UTC, deployed
`aad50a1f`, assembled 8 real nodes on its second attempt (`appenders_known
8`, `membership_writers 7`, 8 / 8 device registrants), **failed on its first
row** (gate 2's `sym-1` arm: the joined writer's `mkdir` under the root
answered `EINVAL`) and was torn down at 19:28:44 UTC — **≈ 26.5 min,
≈ $5.2, nothing billing (verified ×3)**.

The run's pulled evidence was LOST with the dev machine the same evening;
every reading below is from the agent's contemporaneous summary (the run
log's 2026-09-24 15:55 row; the resume note) or derived from the rig's own
asserts — the record's §3.10 keeps the two apart line by line. Nothing that
is in neither (the node kernel, the node RTT, the intermediate wall times)
is stated.

| item | reading |
|---|---|
| cluster / shape | `sqzbench-20260924-150215`; 1 mds + 8 oss + 8 client (one symmetric writer per client node + a token reader on `client0`) |
| venue | AMI `ami-0c40b68421a1fcd8e` (the newest `squeezefs-bench-base=mw` bake the rig prefers — recalled as the run's; the bake is named in `.benchmarks/2026-08-20-fabric-confirm-sessions.md`); build `aad50a1f` `release` on every node (the deploy's sha256 + `build_commit` asserts) |
| cost | ≈ **$5.2** (the contemporaneous summary; 26.5 min × 17 × ~$0.686/hr agrees); the typed-YES line had read ~$7.55/hr for 11 nodes (fixed: every node is priced) |
| result | **INCOMPLETE — no per-node number**; three product findings routed to PR 13i (F-C1 cross-host page-cache incoherence on the shared metadata LUN — design-level; F-C2 the joiner discarding the `Joined` reply's grant word; F-C3 the conveyor's fan-out flattening every retryable class to `EINVAL`); the evidence directory `.benchmarks/cloud/2026-09-24-152527/` lost |

**Lessons, each landed in the rig:**

- **A baked AMI must be identity-scrubbed.** The bake clones
  `/etc/machine-id` (and `/etc/nvme/hostnqn` + `hostid`) onto every node;
  the daemon's node token is derived from the former and the PR registrant
  keyed on the latter. `assemble-sym` asserts all three distinct across the
  client nodes and regenerates a clone before any identity-bearing step —
  the machine-id first (`systemd-machine-id-setup`), then the nvme
  identity. A re-bake should scrub them (`truncate -s0 /etc/machine-id`,
  remove `/etc/nvme/host{nqn,id}`) before `create-image`; the rig's
  assertion is the belt either way.
- **Unattended apt is off for the session** (`deploy` — the graceful drain
  first, a live transaction waited for by its lock and never killed; item 5
  above), and **the re-assemble's `format` passes `--force`** — see "What
  assemble builds".
- **The estimate prices every node** — the max-spend guard is still the
  protection, but the typed-YES line now says what the fleet costs.
- **Two kernels on one LUN is a different venue.** Every co-located venue
  (the laptop, squeeze-test, the 2026-09-12 cloud `mw` row) shares one
  page cache; the first two-kernel venue found a design-level class on its
  first user mutation. The rig's job is exactly that.

**The rule for the re-run (the owner, 2026-09-24 21:20):** nothing runs on
AWS until PR 13i (`fix/sym-shared-lun-coherence` — F-C3 → F-C2 → F-C1, the
two-kernel fixture a qemu/KVM guest member over a laptop-exported nvmet-tcp
namespace) has landed, and **the re-run needs a new expressed owner approval
for that specific run** — the standing per-launch mandate with the landing
as its precondition. Run 2 pulls and commits its evidence under
`.benchmarks/cloud/<ts>/` before any verdict is written.

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
