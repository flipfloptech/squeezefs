# 2026-07-31 — Whole-application NUMA affinity: the N-topology map, the crossing instrument, stage-1 socket locality

Branch `perf/numa-affinity` off dev `3ea474b` (**unmerged — do not
merge/push without orchestrator review**). USER DIRECTIVES, verbatim:
the S1 flag "should be its own campaign for the entirety of the
application", and the standing amendment "anything like numa needs to
scale/work on its own on other topologies. not all hosts have just 2
domains."

Field facts driving the charter (the near-zero-copy §7 verdict's
client, 2× Xeon Gold 6426Y): node0 = even CPUs + ens1f0np0 (the .177
fabric), node1 = odd CPUs + ens2f0np0 (the .178 fabric) — one fabric
per socket; multipath round-robin ⇒ ~half of all payload bytes were
predicted to cross UPI between arena/buffer memory and the serving
NIC. The near-zero-copy ledger named S1-side NUMA placement the
shim-lead widening lever (§7.3) and priced nothing else on that axis.

Commits: red `ce9e406` (the four-synthetic-shape contracts), green
`edcdae1` (map + instrument + stage-1 placement), `06d7a10` (same-pid
sibling rotation, red-first), rig `b49c3a4`
(`tests/numa_field_verdict.sh`); docs = this note.

## 1. The N-topology-general nearest-resource map (the amendment's core)

`crates/squeezefs-ipc/src/numa_core.rs`, `#[path]`-shared into the
root crate AND the fuse3 fork (the `thp.rs`/`wake_core` precedent —
dense node indices agree across the crate boundary because both build
the map from the same sysfs through the same constructor):

- **Runtime-derived, injectable**: `/sys/devices/system/node/nodeN/
  {cpulist,distance}` → nodes (kernel id + CPUs, possibly none),
  square SLIT matrix, precomputed per-node distance rank orders.
  Tests feed synthetic descriptions through the SAME constructor.
  ANY sysfs failure degrades to the single-node identity map.
- **No node arithmetic anywhere**: "my nearest X" =
  `nearest(from, candidates)` (distance, then index); ties and
  resource-less nodes resolve by distance → load → index
  (`pick_owner`); CPU-less (CXL) nodes are first-class and never
  receive execution; `rotate_exec_from` spreads same-origin siblings
  nearest-first.
- **Structural single-node no-op**: `placement_applies` gates every
  action on `!is_single()` independent of the env lever; the owner
  partition degenerates to all-zeros; the admission pick reduces to
  exactly the pre-campaign `(load, index)`; contract-tested.
- **`SQUEEZEFS_NUMA=0`** disables placement/pinning ONLY — the
  instrument stays alive on both sides (that is what makes the field
  A/B attributable). Default ON.

### Behavior on the four synthetic shapes (contract-pinned, `tests/numa_affinity_tests.rs`)

1. **Single node**: `owner_nodes(8) == [0;8]`, `pick_owner` ==
   lightest-then-lowest (byte-identical policy to dev), no mbind, no
   pins, gauges classify everything local. The machinery is inert.
2. **Field 2×1:1**: even/odd CPU split resolves per-CPU; each node's
   nearest NIC is its own; owners alternate `[0,1,0,1…]`; a node-1
   session picks the lightest node-1 owner even when node-0 idles.
3. **NPS 4-node / 2-NIC**: node1 (no NIC) resolves its nearest NIC to
   its SOCKET-SHARED node0 (distance 12), never cross-socket (32);
   rank order from node1 = `[1,0,2,3]`; owners cover all four nodes
   round-robin; intra-socket non-self is still classified remote
   (12 > 10) — the distance rule, not a socket rule.
4. **NIC-less + CPU-less (CXL)**: owners never land on the CPU-less
   node; its nearest exec node is a distance lookup; memory
   deliberately placed there classifies remote from node0 (14 > 10)
   — the instrument stays honest about deliberate far-memory.
   Asymmetric CPU counts weight the owner partition (24:8 ⇒ 3:1).

## 2. The crossing instrument (stage 0)

Distance-based classification (`is_local_choice`: local iff the
memory node was a minimal-distance choice from the executing node —
reduces to same-node on ordinary shapes without encoding that rule;
unknown nodes NEVER enter the counters). Memory nodes are QUERIED
(`get_mempolicy(MPOL_F_NODE|MPOL_F_ADDR)`) where pages actually
landed — never assumed from policy intent.

| Gauge | Site | CPU pass |
|---|---|---|
| `numa_{local,remote}_bytes` | §5.5.2 ring-write sever (service thread), §5.5.1 arena completion serves | daemon copy passes over session arenas |
| `fuse3_numa_{local,remote}_bytes` | FUSE_WRITE lease delivery (exec ≈ qid CPU — the kernel's K1 `copy_from_user` estimate), reply-body serves into ent payloads | transport passes over payload arenas |
| `numa_nodes` | stats inode | the map's node count (1 = no-op posture) |

**Scope stated honestly:** the shim's S1 (app→arena) pass executes in
the client process and cannot reach the daemon stats inode — it is
NOT counted (its locality is what arena placement + app affinity
determine); the kernel-path M1 merge and the zero-copy
serve-into-payload elide/execute on lanes whose buffer node is not
cheaply known at that site — M1 is not counted in v1 (the K1 estimate
covers the same ent buffers from the delivery side). The instrument
is a crossing-rate ESTIMATE over the passes it names, not a total
UPI-byte meter.

## 3. Stage 1 — end-to-end socket locality (landed, gated)

- **Session→node at HELLO** (daemon-side only, zero wire/ABI change):
  the SO_PEERCRED pid's `/proc/<pid>/stat` processor field →
  `node_of_cpu`; same-pid fd-shard SIBLINGS rotate across exec nodes
  nearest-first (`rotate_exec_from` — one multi-threaded app must not
  pile every arena onto one socket; the ingest-economy
  drain-capacity-halving shape, contract-pinned).
- **Arena placement composed with THP**: `mbind(MPOL_PREFERRED,
  node)` BEFORE `MADV_POPULATE_WRITE` + `MADV_COLLAPSE` (pages fault
  on the bound node; the collapse keeps them there — a 2 MiB folio
  cannot span nodes). The instrument then queries where pages REALLY
  landed.
- **Owner→node partition + locality-first admission**: the partition
  is CPU-share-weighted largest-remainder interleave over the
  EXISTING service-thread ceiling (no new constants); admission picks
  `(distance from the arena's ACTUAL node, load, index)`; service
  threads pin to their partition node's CPU set ∩ the process mask
  (taskset never widened; refusal = free-running, exactly dev).
- **fuse3 transport**: per-queue payload arenas became one mmap span
  (the near-zero-copy OQ-3 shape) + `MADV_HUGEPAGE`, node-bound via
  `node_of_cpu(qid)` under the qid==cpu correspondence (the
  queue-per-possible-CPU default; the testing queue override disables
  placement rather than mis-derive). Queue workers fall back to a
  node pin when the exact-core pin refuses (taskset-restricted
  mounts). `tpc_spawn_on_node` prefers node-local handler lanes with
  global-rotation fallback; the dead-lane re-dispatch walk still
  covers every lane (locality is a preference, never an availability
  constraint).
- **ipc handoffs ride the payload's node**: writes (incl. deferred
  placed handoffs, which now carry their arena node to end-of-sweep),
  read handoffs, and the read-reply `ArenaWindow` serves.
- **Custody/security/failover untouched**: fd screen, sever laws,
  lease laws, kernel multipath failover — none of these surfaces
  changed.

## 4. Stage 2 — path preference (EVALUATED, FILED — not landed)

Field probes (read-only, journaled):

- Every subsystem `iopolicy=round-robin`; **all 22 nvme-tcp
  controllers report `numa_node = -1`**, and the attribute is
  **read-only** (`-r--r--r--`) on the field kernel (7.1.2-1.el8
  lineage). Kernel `iopolicy=numa` therefore has NOTHING to steer by:
  switching it on would degenerate to first-live-path (a bandwidth
  regression risk), not locality. Not viable without kernel-side
  ctrl-node derivation/labeling.
- Daemon-side per-IO path choice is not reachable either: path
  selection happens inside the kernel's multipath head
  (`nvme_find_path`); userspace submits to the head node only.
- Filed follow-ups, in preference order: (a) kernel lineage that
  derives nvme-tcp ctrl numa_node from the connected netdev (or makes
  the attr writable) + `iopolicy=numa` + stage-1's node-local ABBs —
  the complete byte journey; (b) per-node io_uring submit lanes in
  `NvmeBlockDev` routed by payload node (submission-CPU locality —
  useful the moment (a) exists; inert benefit before it);
  (c) abandoning native multipath for daemon-owned per-fabric
  namespaces — rejected for v1: it moves failover ownership into the
  daemon (charter: failover semantics unchanged).

## 5. Field verdict (venue, instrument, discipline)

**Venue (stated):** the 4-node cluster, client `memp-s3ds-aqs-37`,
cluster_reset v3 substrate (nullblk 4-wide ns=2, cache-less,
dual-200GbE). **Instrument (stated):** elbencho 3.1-11 dynamic, `t32
-b 4m`, 16 × 8 GiB set, O_DIRECT, sustained rows `--timelimit 60
--infloop`; il rows via LD_PRELOAD of the KD-7 same-commit shim.
**Discipline:** order **A-B-B-A-B-A + A0** (A = `06d7a10` pair,
placement ON; B = dev tip `3ea474b` pair built from the same box; A0
= A binary `SQUEEZEFS_NUMA=0` — the attribution control AND the
stage-0 baseline crossing capture), settle hygiene between rows
(reclaim `queue_bytes == 0` ×3), fill+order labels on every row,
standing deployed pair restored at the end. Raw artifacts: client
`/scratch/tmp/numa_verdict/`, journal `/scratch/tmp/agent_runs.log`.

### 5.1 The baseline crossing proof (A0 — placement OFF, instrument alive)

The campaign's premise measured, per row (fractions over the row's
instrumented bytes; `-1` = the row drives no such pass):

| A0 row | ipc arena passes (severs + serves) | fuse3 K1 estimate | bytes instrumented |
|---|---|---|---|
| fresh (kern) | — | **0.479 local** | 137 GB |
| wrkern | — | **0.481 local** | 1.06 TB |
| wril | **0.519 local** | (31 KB — noise, ignored) | 1.39 TB |
| rdil | **0.484 local** | (33 KB — noise) | 1.21 TB |

**The ~50 % crossing rate is real and measured on every high-volume
pass** — un-placed session arenas and payload buffers land where
accident put them, and half of all instrumented payload bytes cross
UPI. (A0 fuse3 buf-node labels ride the pre-populate zero-page query
— the fraction there reads as "exec-node vs node-0", which on this
even/odd topology is the same ~50 % statement; the A legs' labels are
post-populate and exact. Read-row fuse3 counts are KB-class because
the read reply serve is the zero-copy elided path — stated so nobody
reads signal into them.)

### 5.2 Throughput (MiB/s, medians of 3, per-rep order-labeled, A-B-B-A-B-A)

| Row | A (06d7a10, placement ON) | B (dev 3ea474b) | A/B | A reps | B reps | A0 |
|---|---|---|---|---|---|---|
| fresh 128 GiB (kern, 9 % fill) | 22,370 | 22,270 | +0.4 % (par) | 22370/22616/22340 | 22270/22416/22122 | 22,402 |
| wr-kern sustained 60 s | 16,766 | 16,699 | +0.4 % (par) | 16809/16766/16627 | 16642/16699/16751 | 16,765 |
| **wr-il sustained 60 s** | **22,126** | 17,884 | **+23.7 %** | 22126/22130/22040 | 18055/17884/17792 | 18,244 (≈ B) |
| rd-kern sustained 60 s | 16,979 | 16,920 | +0.3 % (par) | 17072/16912/16979 | 16934/16920/16842 | 16,941 |
| **rd-il sustained 60 s** | **19,156** | 16,995 | **+12.7 %** | 19160/19156/19004 | 16902/16995/17027 | 16,885 (≈ B) |

- **Every A il rep sits above every B il rep** (no overlap; the
  bracket is order-clean, both directions present on the par rows).
- **A0 ≈ B on every row** (wril 18,244 vs B 17,792–18,055; rdil
  16,885 in-spread) — binary drift nil, **the deltas are the
  placement**, attribution pinned by the same leg that proves the
  baseline crossing.
- **Bars:** no row regresses (kern write/read + fresh at par); the
  shim/kernel gap **re-widened exactly as the S1 flag predicted** —
  write 1.071 → **1.320**, read 1.005 → **1.129**.
- **Stretch:** wr-il sustained 22,126 MiB/s = **89 % of the 24.3
  GiB/s raw-fio ceiling** on this substrate (and above the pre-reset
  historical shim sustained 18.0 GB/s on this t32b4m shape).
- The kernel write path gained ~nothing from K1 becoming fully local
  — stated honestly: at ~16.7 GB/s that path is not crossing-bound
  (pipeline/device-bound); the shim path, which pays two userspace
  passes per byte, is where UPI locality was the binding term.

### 5.3 Engagement (exact, per row)

- A il rows: `numa_local_frac = 1.000` over 1.39 TB (wril) / 1.21 TB
  (rdil) per rep; A kern rows: `f3_local_frac = 1.000` over 1.06 TB
  (wrkern) / 137 GB (fresh). The locality gauges swing local
  EXACTLY when placement is on — the engagement law held on every
  run.
- `ShmemPmdMapped` = **512 MiB during every il row of every leg**
  (A, B, AND A0) — the arena `mbind` composes with the THP
  populate+collapse; placement did not cost the huge pages.
- `placed_merge_elides ≈ ipc_placed_severs` (1,327,628 vs 1,327,713
  on wril r1); `ipc_bytes_in/out` account for every il row's bytes;
  `write_path_seed_read_bytes` 0 throughout;
  `ipc_sessions_poisoned` / `ipc_descriptor_rejects` **0 on every
  leg**; 16 sessions per leg (8 per il row — spawn-on-bind intact,
  `svc_threads` 7–8 with the locality-first pick).
- Store hygiene: settle between rows, fill labeled (9 % fresh / 42 %
  sustained), standing dev pair (`b4edafc`) restored + store settled
  at end; journal `/scratch/tmp/agent_runs.log` carries every leg.

## 6. Gates (merge bar per operating model: long gates skipped)

- `cargo clippy --all-targets --all-features -- -D warnings` — clean
  (root; fuse3 fork clippy clean standalone).
- `cargo fmt --check` — clean (both).
- Contracts `tests/numa_affinity_tests.rs` 12/12 green (red commit
  `ce9e406` precedes green `edcdae1`).
- Touched-surface binaries green: `ipc_host_tests` 21, `preload_session_tests`
  24, `shim_parity_tests` 3, `ipc_op_economy_tests` 3,
  `ingest_economy_tests` 4, `arena_thp_tests` 3, `nt_copy_tests` 6,
  `tpc_tests`/`tpc_scheduler_tests` 2; fuse3 lib suite 45/45.
- rocky8 container pairs (KD-7): `06d7a10` and `3ea474b`, `--version`
  non-unknown, glibc ≤ 2.28 asserted in-container.

## 7. Open questions

1. **Shim-side S1 locality gauge** — needs a client-side counter
   surface (shim can compute it; nowhere to publish). Revisit if the
   verdict shows the un-instrumented S1 term dominating.
2. **M1 merge-site classification** — requires threading the ent
   buffer node through the lease `Bytes`; the K1-side estimate covers
   the same buffers, so only worth it if lane-affinity questions
   arise.
3. **Read-path tier buffer placement** (hot-block tier, severed pool,
   block pools are node-blind recycled) — per-node pool sharding is
   the natural stage 1b if the verdict shows read-side remote splits
   dominating.
