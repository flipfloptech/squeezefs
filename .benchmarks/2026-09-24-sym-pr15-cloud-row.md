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
+ `tests/cloud_sym_rows.sh` at the lost branch's head `3ce1395a` (the
driver's flags are the rig's `bench-sym` defaults — `--venue=cloud`,
`--size-to-rt=auto`, `--rt=60`), the laws `tests/sym_rows_lib.sh`'s.

## Known vs inferred

The pulled evidence is lost (below). Every statement here is one of: **(S)
SOURCED** — in the resume note `~/sym-run-state/RESUME-2026-09-24-omarchy.md`
(the agent's contemporaneous summary of the pulled `.stats` + logs) or the
run log's 15:55 / 20:55 rows (`b88bfa54` / `b87d3f57`); **(D) DERIVED** —
what the rig's scripts or the code make necessarily true of a run that
reached the stated point; **(R) RECALLED** — in no surviving source,
unverifiable. The sourced set: the cluster id, the shape and the approval,
the two instants `19:02:14` / `19:28:44` UTC, ≈ $5.2, "nothing billing
(verified ×3)", "attempt 1 failed on the cloned `/etc/machine-id` (= the
node token)", attempt 2's `appenders_known 8` / `membership_writers 7` /
"device registrants 8/8", row 1's identity and failing op, the daemon line,
the four gauges + "one ask per cadence tick, every one a verbatim replay",
the three findings with their code sites, the four rig fixes as a list, the
lost directory's path and the lost branch's sha. **The node kernel, the
node-to-node RTT, the deploy's and row 1's wall times, and what the
unattended apt did during the run are in NO surviving source and are not
stated.**

**Venue:** AMI `ami-0c40b68421a1fcd8e` — the newest `squeezefs-bench-base=mw`
bake the rig prefers (`squeezefs-mw-base-v2`, named in
`.benchmarks/2026-08-20-fabric-confirm-sessions.md` §5); that this run
launched from it is R. Build **`aad50a1f` `release`** (`task
build:ubuntu2604`) on every node — S that the launch was "on `aad50a1f`'s
artifact", D that every node carried it (the deploy's sha256 + `--version`
and the assemble's `build_commit` ritual are asserts).

## Timeline (UTC)

| instant | event | class |
|---|---|---|
| 19:02:14 | `launch` — 17 instances; the deadline guard the rig arms at +4 h | S; D (the guard) |
| — | `deploy` — the artifact sha256-verified on every node (the rig's assert). The apt-hygiene fix is in the resume note's list of the four rig fixes; what the unattended apt DID during the run is in no source and is not stated | D; S (the fix's existence) |
| — | `assemble-sym` attempt 1 — **"failed on the cloned `/etc/machine-id` (= the node token)"** (S). D: the daemon's node token is derived from the file (`src/writer_scope.rs`), so every joiner carried the manager's `(node_token, mount_slot)`, and the failure was at the mounts (nothing before them reads the file). Where in the join ladder it failed is NOT known | S + D |
| — | `assemble-sym` attempt 2 — **"assembled 8 real nodes (`appenders_known 8`, `membership_writers 7`, device registrants 8/8)"** (S), after the clones were regenerated live (the 15:55 row). D: `joined_registrant_posture registrant` on every joiner, the token reader's posture, `build_commit` on every mount — the assemble's own asserts. D: attempt 2's `format` met attempt 1's superblock (`format_preflight` refuses a formatted volume without `--force`) | S + D |
| — | `bench-sym` row 1 — **gate 2 `sym-tarx`, arm `sym-1`: FAILED** — the joined writer m60 (`client1`, D — the driver's node table)'s `mkdir -p <mount>/s8a-sym-1` under the unstriped root → **`EINVAL`** (S); the per-node `.stats` and daemon logs were pulled (S — the 20:55 row names the directory) | S |
| 19:28:44 | `teardown` — "torn down … nothing billing (verified ×3)" (S); the three checks are the rig's teardown shape, the methods not recorded (D) | S + D |

No other instant survives; ≈ 26.5 min is the two sourced instants' difference.

## What failed (S — the contemporaneous reading, the only numbers that survive; the readings beside them D from the code)

- **m60's log at the `mkdir`:** `appender 1's extent grant is exhausted (0
  unclaimed, 1 needed) and the manager has not refilled it — retry
  (EAGAIN)` — the retryable `GrantExhausted` — "surfaced through `Corrupt`"
  to `mkdir(2)` as `EINVAL` (S; `Corrupt` → `InvalidOperation` → `EINVAL` is
  D, F-C3).
- **m60 pre-row:** `extent_grant_granted 72 = claimed 72, unclaimed 0` (S).
  The reading (D): `RegionGrant::recover(record.extents(), &page.grant)`
  with an EMPTY page word classifies every extent the tree-0 record names
  as CLAIMED with no mint at all (`backend.rs:20437–20441`'s own comment) —
  the stale-empty page word's classification, F-C1 ⊕ F-C2's shape,
  consistent with the 15:55 row's "rebuilds its grant from the (stale,
  empty) page"; `joined_wire_verbs 356` — "one ask per cadence tick, every
  one a verbatim replay" (S).
- **The manager pre-row:** `extent_grants 7`; `manager_verbs 2135`, of
  which **`manager_verb_replays 2083`** (S). D: one carve per joiner at its
  join; the replays are §5.3.5's idempotency answering the page's grant word
  (`unclaimed_remainder_of`). "All 7 joiners write-dead from their join,
  the manager replaying every ask verbatim" (S, the 15:55 row).

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

The list is S (the resume note names the four); what each fix DOES is this
branch's, pinned where a `--dry-run` cannot reach the logic:

1. `assemble-sym` asserts `/etc/machine-id` DISTINCT across the client nodes
   and regenerates a clone — a regular-file dbus id removed BEFORE
   `systemd-machine-id-setup` (its first source) and recreated after; the
   file truncated; re-read, re-asserted — a new step before the
   host-identity step (`tests/cloud_bench_node_scripts.sh`).
2. The re-assemble's `format` passes `--force` (the live-client refusal
   kept).
3. `deploy` takes Ubuntu's unattended apt off the session on every node:
   `unattended-upgrades` drained (its stop handler waits for a running
   child) then masked, the timers off; a LIVE apt/dpkg transaction is
   waited for by its LOCK, bounded, never killed.
4. The symmetric shape's `EST_CLUSTER_HOURLY` prices EVERY node
   (`N_MDS + N_OSS + N_CLIENT + N_SPARE` — the typed-YES line had read 11
   nodes / ~$7.55/hr for the 17 launched, ~$11.66/hr; D from the base
   script).

Fixes 2 and 4 are proven by `--dry-run` at the approved shape (red before,
green after); fixes 1 and 3 by the shell pin
`tests/cloud_bench_cluster_units.sh` (fake `systemctl` / `fuser` / `remote`
/ `systemd-machine-id-setup`; RED on the first build, GREEN now). No aws
call executes under `--dry-run`.

## Cost

≈ **$5.2** (S). D: 26.5 min × 17 × ~$0.686/hr ≈ $5.2 — the arithmetic
agrees. On-demand (S — the shape); no spot interruption (D — an
interruption aborts the count and the run's row reports none). "Nothing
billing (verified ×3)" (S) — the rig's three teardown checks by shape, the
methods not recorded.

## Evidence — lost with the dev machine on 2026-09-24

The row pulled the per-node `.stats` snapshots and every node's daemon logs
to `.benchmarks/cloud/2026-09-24-152527/` before the teardown. **That
directory, the first redo branch (`perf/sym-cloud-row-run` @ `3ce1395a`,
never pushed) and its worktree were LOST when the dev machine was
reinstalled the same evening** (the run log's 20:55 row). The conclusions
above rest on the agent's contemporaneous summary — quoted in the run log
([`.benchmarks/2026-09-12-sym-pr-run.md`](2026-09-12-sym-pr-run.md), the
2026-09-24 15:55 row) and carried verbatim in the resume note — and on the
code sites, re-verified on this tree. No per-node number of the run exists;
gate 2 has no ratio, gate 3 no multiple, gate 3b no reading. **The law the
loss added: every review-stage branch is pushed to `origin` as a backup
ref.**

## Owed

Run 2 — after PR 13i lands (F-C3 → F-C2 → F-C1, red-first, F-C1 on the
two-kernel fixture), on the flip candidate's binary, **with a NEW expressed
owner approval for that specific run** (the owner rule of 2026-09-24 21:20:
nothing runs on AWS until PR 13i has landed): the three row sets from zero
on the approved shape, the per-NODE law of gate 3 read as written, the
evidence pulled and committed under `.benchmarks/cloud/<ts>/` BEFORE any
verdict.
