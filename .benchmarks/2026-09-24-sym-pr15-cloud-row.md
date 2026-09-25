# Symmetric shared-disk metadata — PR 15 Phase B, the cloud row — run 1 (2026-09-24)

**Status: LAUNCHED with the owner's expressed approval; ASSEMBLED on 8 real
nodes on the second attempt; FAILED on its FIRST row; TORN DOWN at 26.5 min
≈ $5.2, nothing billing. The row is INCOMPLETE and UNMEASURED — no per-node
number exists. Evidence LOST with the dev machine on 2026-09-24.**

The full account is the acceptance record's §3.10
([`.benchmarks/2026-09-19-sym-acceptance.md`](2026-09-19-sym-acceptance.md)
— §3.10 the run, §4.4ar–at the three findings, §7 item 0 / item 20 and §9
the consequences); this note is the row's own page.

## The shape (approved 2026-09-24 14:45 wall — "S2 + 8 oss")

`PRESET=mw SYMMETRIC=1 N_MDS=1 N_OSS=8 N_CLIENT=8 MAX_CLUSTER_HOURS=4` —
**17 × i4i.2xlarge, us-east-1a, on-demand**, cluster placement group: 1
metadata storage node, 8 data storage nodes (one 1,875 GB instance-store
namespace each, nvmet-tcp, `resv_enable=1`), 8 client nodes = **one
symmetric writer per node** (the MANAGER on `client0`, a JOINED writer on
`client1..client7` through the join ladder over the real wire, a
`--read-only` token reader on `client0`); `format --symmetric` CACHE-LESS.
Cluster `sqzbench-20260924-150215`. Instrument: `tests/cloud_bench_cluster.sh`
+ `tests/cloud_sym_rows.sh` (`--venue=cloud --size-to-rt=auto --rt=60`), the
laws `tests/sym_rows_lib.sh`'s.

**Venue:** AMI `ami-0c40b68421a1fcd8e` (the `squeezefs-bench-base=mw`
bake), kernel `7.0.0-1011-aws` on every node (both kernel floors probed),
build **`aad50a1f` `release`** (`task build:ubuntu2604`) on every node —
`squeezefs --version` ≡ every mount's `build_commit`; node-to-node RTT
0.168 / 0.179 / 0.190 ms.

## Timeline (UTC)

| instant | event |
|---|---|
| 19:02:14 | `launch` — 17 instances, SSH-reachable, the deadline guard armed (+4 h) |
| ≈ 19:12 | `deploy` — the artifact sha256-verified on every node. Ubuntu's unattended apt fired on a fresh node during the session: ≈ 3 min of CPU on a writer, an `sshd` restart that killed the driver's preflight once |
| — | `assemble-sym` attempt 1 — **FAILED at the mounts**: the baked AMI's `/etc/machine-id` was CLONED on every node, and the daemon's node token is derived from it (`src/writer_scope.rs`), so every joiner carried the manager's `(node_token, mount_slot)` |
| — | `assemble-sym` attempt 2 — the clones regenerated, **8 real nodes ASSEMBLED**: `appenders_known 8`, `membership_writers 7`, `nvme resv-report -e` 8 / 8 distinct registrant Host IDs on every namespace (every joiner `joined_registrant_posture registrant`), the token reader up, `build_commit` verified on every mount. Attempt 2's `format` met attempt 1's superblock |
| ≈ 19:26 | `bench-sym` row 1 — **gate 2 `sym-tarx`, arm `sym-1`: FAILED** — the joined writer m60 (`client1`)'s `mkdir -p <mount>/s8a-sym-1` under the UNSTRIPED root answered **`EINVAL`**; the driver died; the per-node `.stats` and daemon logs were pulled (evidence before verdict) |
| 19:28:44 | `teardown` — instances terminated, SG / launch template / placement group deleted, the tag-scoped sweep clean; **nothing billing — verified ×3** |

## What failed (the contemporaneous reading — the only numbers that survive)

- **m60's log at the `mkdir`:** `appender 1's extent grant is exhausted (0
  unclaimed, 1 needed) and the manager has not refilled it — retry
  (EAGAIN)` — the retryable `GrantExhausted` — surfaced to `mkdir(2)` as
  `EINVAL` (through `Corrupt`).
- **m60 pre-row:** `extent_grant_granted 72 = claimed 72, unclaimed 0`;
  `joined_wire_verbs 356` (one ask per cadence tick since the join, every
  one a verbatim replay).
- **The manager pre-row:** `extent_grants 7` (one carve per joiner — the
  join's); `manager_verbs 2135`, of which **`manager_verb_replays 2083`**.
  All 7 joiners write-dead from their join; the manager replaying every ask.

## The three findings (each → PR 13i `fix/sym-shared-lun-coherence`; §4.4ar–at)

- **F-C1 — DESIGN-LEVEL, a flip blocker: cross-host page-cache incoherence
  on the shared metadata LUN.** Every metadata read/write is `uring_fs`
  BUFFERED (`O_CLOEXEC` only — `src/uring_fs.rs:1368/1586/1609`; the data
  path alone `O_DIRECT`, `src/nvme_dev.rs:744`) = each host's block-device
  page cache. Two hosts over nvme-tcp ⇒ a joiner reads ITS kernel's stale
  cache of a block the manager wrote (the appender page's two images:
  `Live` with the grant cleared at `backend.rs:10510–10514`, then the grant
  word via `write_wire_joiner_page_grant` ≈ `backend.rs:11096`, durable at
  the next barrier); the same for tree 0 / ring 0 projections, foreign
  slot-tree nodes, the directory, the ledger. Every co-located venue
  (laptop, squeeze-test, the 2026-09-12 cloud row) shared ONE cache and was
  structurally blind. **Remedy — the shared-LUN rule:** `O_DIRECT` (or
  explicit invalidation) on every shared-LUN metadata read + write on every
  host; sector-aligned staging for the ring's byte-positioned entries and
  the 4 KiB page / ledger writes. **Fixture:** two kernels on one block
  device — a qemu/KVM guest member over a laptop-exported nvmet-tcp
  namespace (network namespaces on one laptop share a page cache).
- **F-C2:** `open_joined_appender` (`src/meta_backend/kv/backend/joined.rs:751`)
  DISCARDS the `Joined` reply's `grant` word (`ManagerReply::Joined {
  appender_id, already, node_seq_base, .. }`) and rebuilds the RAM grant
  from the device page (`RegionGrant::recover(record.extents(),
  &page.grant)`, `backend.rs` ≈ 20450–20465) — the manager's own doc on
  `write_wire_joiner_page_grant` states the contract the joiner must honour
  (the grant's runs reach the joiner ON THE REPLY); `unclaimed_remainder_of`
  (`backend.rs:9286`) reads the page word for the §5.3.5 replay.
- **F-C3:** the conveyor batch-failure fan-out `clone_kv_error`
  (`backend.rs:23709–23727`) flattens every class but `Io` / `NoSpace` to
  `KvError::Corrupt(other.to_string())` → `InvalidOperation` → `EINVAL`;
  `SlotBusy`, `GrantDeferred`, `Busy`, `ManagerUnreachable` lose their class
  too.

## The four rig defects, FIXED on the redo branch `perf/sym-cloud-row-run`

1. `assemble-sym` asserts `/etc/machine-id` DISTINCT across the client nodes
   and regenerates a clone (`systemd-machine-id-setup` after truncating the
   file; re-read, re-asserted) — a new step before the host-identity step.
2. The re-assemble's `format` passes `--force` (the live-client refusal
   kept).
3. `deploy` stops `apt-daily.timer` + `apt-daily-upgrade.timer` and masks
   `unattended-upgrades` on every node.
4. The symmetric shape's `EST_CLUSTER_HOURLY` prices EVERY node
   (`N_MDS + N_OSS + N_CLIENT + N_SPARE` — the typed-YES line had read 11
   nodes / ~$7.55/hr for the 17 launched, ~$11.66/hr).

Each proven by `--dry-run` at the approved shape (red before, green after;
no aws call executes under `--dry-run`).

## Cost

≈ 26.5 min × 17 × ~$0.686/hr ≈ **$5.2**. On-demand throughout; no spot
interruption. Nothing billing after the teardown (the sweep, `status`, a
tag-key `describe-instances` across the region — ×3).

## Evidence — lost with the dev machine on 2026-09-24

The row pulled the per-node `.stats` snapshots and every node's daemon logs
to `.benchmarks/cloud/2026-09-24-152527/` before the teardown. **That
directory, the first redo branch (`perf/sym-cloud-row-run` @ `3ce1395a`,
never pushed) and its worktree were LOST when the dev machine was
reinstalled the same evening.** The conclusions above rest on the agent's
contemporaneous summary quoted in the run log
([`.benchmarks/2026-09-12-sym-pr-run.md`](2026-09-12-sym-pr-run.md), the
2026-09-24 15:55 row) and on the code sites, re-verified on this tree. No
per-node number of the run exists; gate 2 has no ratio, gate 3 no multiple,
gate 3b no reading. **The law the loss added: every review-stage branch is
pushed to `origin` as a backup ref.**

## Owed

Run 2 — after PR 13i lands (F-C3 → F-C2 → F-C1, red-first, F-C1 on the
two-kernel fixture), on the flip candidate's binary, **with a NEW expressed
owner approval for that specific run** (the owner rule of 2026-09-24 21:20:
nothing runs on AWS until PR 13i has landed): the three row sets from zero
on the approved shape, the per-NODE law of gate 3 read as written, the
evidence pulled and committed under `.benchmarks/cloud/<ts>/` BEFORE any
verdict.
