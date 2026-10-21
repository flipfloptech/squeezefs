# Porting spec — the `.stats` inode and the CLI (rung 5)

Scope: charter §10 (observability contract), §8 (the one CLI binary), §11 (bench smoke in
the gate; the measurement law), §1.6 (knob classes). Reference paths are relative to
`~/Source/squeezefs` (`R:`); port paths to `~/Source/squeezefs-zig` (`P:`). Every claim
carries `file:line`. The reference is evidence (decision 0002); §7 lists the quirks NOT
carried and what the port does instead.

Rung placement (charter §13): rung 5 = "clients/claims/heartbeats, the stats inode, the
CLI" ⇒ `.stats`/`.config`/`.trace`, `status`, `clients`, `df`, `bench`, `clone`, `tune`,
`config`, `claim clear`, `staging`. Rung 6: `appenders`, `appender clear`. Rung 7: the
admin lane (`op-trace on|off|status`, `job-throttle`, `volume-*` live verbs) and the
`user.squeezefs.il0` xattr. Rung 8: `volume {…}`, `fsck`/`scrub`, `defrag`, `job`,
`nvmeof`, `storage`.

## 0. The port today (what rung 5 builds on)

- The CLI is one typed `Verb` union — `version, help, format, mount, umount, status`
  (`P:src/cli/args.zig:78-85`), dispatched by `cli.run` with three exit codes: 0 ok, 1 a
  refused/failed verb (reason on stderr), 2 usage (`P:src/cli/cli.zig:27-31, 46-66`).
  Linux-only verbs are gated by `builtin.os.tag`, never stubbed (`P:src/cli/cli.zig:19-25`).
- `status` already reads the superblock, the newest ledger record, the set config and the
  per-volume REFERENCE census offline, O_DIRECT, beside a live daemon, and prints a text
  report whose last line is `summary: v4, …` (`P:src/cli/status.zig:1-11, 204-235`); the
  writer-guard probe is a shared non-blocking `flock` (`P:src/cli/status.zig:79-91`).
- Stats today = `SIGUSR1` → six log lines `stats transport/data/meta/exec/mem_budget/
  ops/daemon: {json}` (`P:src/cli/mount.zig:539-614`), each a fixed buffer (16 KiB; 64 KiB
  for `data`). The smoke reads must-stay-0 gauges and the closure ledgers out of that tail
  (`P:src/cli/smoke.zig:1306-1379`).
- JSON writers that exist: 31 `writeStatsJson` in `src/data/*` composed by
  `DataPlane.writeFamiliesJson` under `{plane, write, read, warm_ladder, layout, staging,
  staging_backing_phase_ns, pack, router, block_alloc, block_refs, reclaim, transform,
  nvme_dev, purge, hot_tier, nvme_read_cache, prefetch, placement, volumes[]}`
  (`P:src/data/plane.zig:1806-1852`); `Session.writeStatsJson` (transport,
  `P:src/fuse/transport/session.zig:1302`), `SqzIo.writeStatsJson` (exec,
  `P:src/exec/sqz_io.zig:507`), `MemBudget.writeStatsJson` (`P:src/mem/budget.zig:829`),
  `journal_lane.writeStatsJson` (`P:src/meta/journal_lane.zig:226`), `conveyor.Stats.
  writeJson/writePhasesJson` (`P:src/meta/conveyor.zig:380`), `uring_fs.writeStatsJson`
  (`P:src/io/uring_fs.zig:489`), `nvme_derive.writeStatsJson` (`P:src/io/nvme_derive.zig:403`),
  `op_trace.writeLatencyJson` (`P:src/fuse/handler/op_trace.zig:216-226`).
- Families with counters but NO writer yet (the inode must aggregate them): `meta/
  {alloc_ext 9, checkpoint 11 (partially hand-listed in mount.zig:566-573), dlm 8, economy
  5, heap_law 2, inos 2, journal 6, key 1, leaf_merge 8, ledger 1, node_cache 13,
  node_snapshot 4, node_state_core 2, node 9, record 3, smo 6, tree 2, tx 1}`, `fuse/
  handler/{kv_fs 19 (hand-listed), lifecycle 11, op_guard 16, open_table 3, watchdog 4}`,
  `sync/{arc_swap 1, tick 2}`, `exec/placement 1` (counts = `pub var …: counters.*`
  declarations per file, `rg` census 2026-10-07).
- Primitives: `counters.{ShardedCounter, Gauge, ShardedLatencyHistogram, Tripwire}`; the
  histogram export shape `{"buckets":{label:n…},"count","sum_ns","mean_ns"}` over 26 log2
  µs buckets (`P:src/core/counters.zig:101-150`); `invariantTripwires()`, `tripwireCount
  (site)`, `forEachTripwireSite(ctx, visit)` over a 512-slot static site table
  (`P:src/core/counters.zig:251, 289-315`).
- Knobs: `SQUEEZEFS_OP_PROFILE` registered (`ab_lever`, `P:src/core/knobs.zig:157`), armed
  once at mount (`P:src/fuse/handler/op_trace.zig:182-199`); `SQUEEZEFS_STATS_KEY_CENSUS`
  deliberately NOT registered until a reader exists (`P:src/core/knobs.zig:96-97`);
  `SQUEEZEFS_DIRECT_DEVICE_TRUE` registered (`P:src/core/knobs.zig:209`).
- `default_permissions` is the shipped posture and cannot be turned off by `-o`
  (`P:src/fuse/mount_options.zig:108, 173, 456-457`); the mount uid/gid are `geteuid/
  getegid` (`P:src/cli/mount.zig:180`). Writeback cache is OFF by default (`-o writeback`
  opts in; `P:src/fuse/mount_options.zig:130-132`) — the reference's default-on posture
  is what motivated its generation scheme (§1.3).
- `statfs` reports the data allocators' space when the set has data volumes, else the
  meta heap; `files = INO_SPACE`, `ffree = INO_SPACE − used_inos` from the watermark
  (`P:src/fuse/handler/kv_fs.zig:1688-1711`); `inos.stats.live_inodes` exists as the
  population gauge (`P:src/meta/inos.zig:47-54, 199-225`).
- `op_trace`: the stage vocabulary, `OpTrace` on the handler frame, the always-on
  `fuse_op_latency_ns[class]` and the armed `fuse_op_phase_ns[class][phase]` exist; the
  sampled RING and `.trace` are declared rung-5 (`P:src/fuse/handler/op_trace.zig:1-29`).
  The ring's geometry/derivation is already specified in
  `docs/porting/fuse_handlers_lanes.md` §7.1 (lines 778-852) — this spec covers only its
  exposure.
- The gate: `zig build gate` = fmt + unit tests + rails + smoke (`P:build.zig:157-162`);
  there is no bench step yet.

## 1. The `.stats` inode (charter §10)

### 1.1 Identity

| Item | Reference | Port |
|---|---|---|
| Canonical inos | `CONFIG_INODE = 0xffff_ffff_ffff_fffe`, `STATS_INODE = 0xffff_ffff_ffff_fffd` (`R:src/fuse_client.rs:19-20`) | NOT served: nothing predates the generation scheme here (§7 Q3). Reserve the two values so a real ino can never collide. |
| Generation range | `[0xffff_ffff_0000_0000, 0xffff_ffff_ffff_fff0]`; real inos monotonic-from-1, cap ≥ 100 M ≪ 2³², so the range starting at 2⁶⁴−2³² is disjoint (`R:21-41`) | Same range; the port's local ino space is `INO_SPACE` (`P:src/fuse/handler/kv_fs.zig:133`), far below. |
| Class in the ino | low `VIRTUAL_CLASS_BITS = 2`: Stats 0, Config 1, Trace 2 (`R:45-76`); survives registry eviction | Same encoding (`enum(u2)`), so a FORGET/GETATTR on an evicted generation still knows its class. |
| Mint | `ino = FIRST + (seq % slots) × 4 + tag`, `seq` one shared `AtomicU64` across handler clones — a split counter "could mint the SAME gen ino on two queues" (`R:14839-14862, 9414-9418`); wraps after ~1 G mints per class | One `std.atomic.Value(u64)` on the filesystem; the registry is a FIFO ring of generations (§7 Q16). |
| Registry cap | `VIRTUAL_GEN_REGISTRY_CAP = 256` — a safety rail against a kernel that never FORGETs: 256 × the field-measured ≈ 72 KiB payload ≈ 18 MiB worst case; "a fixed count (not a derived budget) is deliberate" (`R:117-125`) | Keep 256 with that WHY on the line; eviction order = mint order. |
| Retirement | FORGET / BATCH_FORGET remove the generation (`R:30322-30332, 30357-30369`); RELEASE drops the fh pin (`R:29953-29956`) | Same; `forgetShape`-class hooks already exist in the lifecycle. |

### 1.2 Attributes and visibility

`get_stats_attr(size)` (`R:10892-10916`): regular file, `perm = VIRTUAL_INODE_MODE = 0o400`,
`nlink 1`, `uid/gid = the mount identity`, `size = payload.len()`, `blocks = size.div_ceil
(512)`, `blksize 4096`, `rdev 0`, `atime = mtime = ctime = now`. `.config` is the twin
(`R:14806-14830`); `.trace` shares `.stats`' shape (`R:14865-14876`).

WHY 0400 (VAL-7a, `R:535-547`): they were 0444; the payload names every cached block key,
the read-cache census, `active_writes` keyed by inode, every backing device path and
staging directory — "a complete map of the daemon's private state and of every co-tenant's
I/O, readable by any local user on an `allow_other` mount". With `-o default_permissions`
always set (`R:31511`; port `P:src/fuse/mount_options.zig:108`) the kernel enforces the mode,
so `0400 + mount uid` is the access boundary; root still reads (the operator path).

Visibility: the root virtuals are LOOKUP-ONLY — never listed by readdir ("the .zfs/.lustre
hidden control-file pattern; fstests generic/062: recursive walks must not see fabricated
files", `R:29125-29129`). Their reserved readdir cookies `(1<<62)+3` (`.config`) and
`(1<<62)+4` (`.stats`) ride strictly above the real cookie space `[3, 2⁶²+2]` and are
honoured only as a stream terminator on resume (`R:166-177, 25238-25240`). Port: the
cookie codec is `key.encodeReaddirCookie` (`P:src/fuse/handler/kv_fs.zig:52-54`); add the
two reserved values as "offset ≥ RESERVED ⇒ empty page" and never emit them.

### 1.3 Read semantics — snapshot at LOOKUP, immutable per generation

The reference carries three mechanisms at once (history: `R:21-41`, `R:27501-27508`):

1. **Per-lookup generations** (the 2026-08-04 wb-cache torn-JSON fix, "third mechanism"):
   `lookup(1, ".stats")` generates the payload ONCE, mints a fresh gen ino and replies an
   entry with `ttl = 0` and `attr.size = payload.len()` (`R:27152-27161`). Under
   `FUSE_WRITEBACK_CACHE` the kernel OWNS `i_size` of a regular file and discards the size
   in every later attr reply — measured live: daemon GETATTR replies 71352→71350→71349 while
   the kernel served a frozen 71352, so `cat` (splice) clamped and tore the JSON mid-string
   39/40 times under counter churn (`R:24-29`). A fresh kernel inode's size authority
   initialises from THIS entry and never needs to change because the generation is
   immutable (`R:30-33`).
2. **The legacy fixed-ino protocol** — `latest_stats_json` / `latest_stats_size` cells:
   first touch generates and publishes, GETATTR reports the published size and never
   regenerates (`R:14917-14929`, pinned by `metrics_tests::stats_snapshot_getattr_size_
   matches_served_bytes_under_churn` — 9 of 10 mdstorm snapshots were torn prefixes before,
   `R:tests/metrics_tests.rs:60-70`); OPEN consumes-or-regenerates and republishes the pinned
   size, "concurrent readers race last-open-wins (bounded, documented residual)"
   (`R:27501-27517`).
3. **The fh pin**: OPEN inserts `(fh → Arc<payload>)` into `open_virtual_files`, replies
   `FOPEN_DIRECT_IO` ("the kernel trusts our read replies (short read = EOF) instead of a
   cached i_size"), RELEASE removes it (`R:27529-27546, 29953-29956`). READ serves the fh
   pin first, then the generation registry verbatim, then (fixed inos only) a fresh
   regenerate; a retired generation is `ESTALE` (`R:33779-33820`).

GETATTR on a generation ino reports the registry payload's length or `ESTALE` once
retired — "the zero entry TTL mints fresh on the next walk" (`R:14886-14900`). `lseek`
answers all-data over the frozen size, `ENXIO` at/after EOF (`R:29722-29755`). `flush` is a
no-op (`R:29859-29861`). Every mutation refuses: `setattr` EACCES for any virtual ino
(`R:28453-28458`), `unlink/rename` of `.config` EPERM (`R:28821-28823, 28882, 28994`),
`setxattr` EPERM (`R:30621-30625`), write-intent open falls through to the regular path only
for real inos.

**PORT (one mechanism, §7 Q3):** `.stats`/`.config`/`.trace` are generation inos only.
`lookup(1, name)` → build the payload into a fresh generation (one allocation of the final
byte length), mint, reply `{nodeid = gen_ino, entry_valid = 0, attr_valid = 0, attr.size =
len, mode 0100400, uid/gid = mount}`; `open` → `FOPEN_DIRECT_IO`, fh = the generation index
(no second table: the generation IS the pin — RELEASE drops one reference, FORGET another,
the ring eviction a third; the payload frees at zero); `read(off, len)` → the slice of the
generation's bytes (no copy, §7 Q15), empty past EOF; `getattr/lseek` → the frozen size;
`ESTALE` for a retired generation. The payload ends with ONE `\n` and no padding
(`finish_virtual_payload`, `R:8592-8610`; pinned by `phantom_backend0_tests::
test_stats_payload_exact_size_no_tail_padding`). The port's handler already answers
negative lookups and TTLs through `mount_options.Ttls`; the virtual arm bypasses them.

### 1.4 The JSON schema

Serialisation: the reference builds a `serde_json::Value` tree and pretty-prints it
(`R:14802`), so the family order is the macro's order. Top level, in order (`R:11229-12882`):

```
build_commit, build_tag, build_profile,                       # build identity (fleet mixed-version detector)
read_lru_key_count, write_lru_key_count,                      # census-FREE counts (always; what umount reads)
nvme_staged_write_file_count, nvme_read_cache_block_count, active_write_block_count,
inline_max_bytes, pack_max_slot_bytes, small_file_packing,    # the layout boundaries in force
stats_key_census,                                             # bool: the census armed?
read_lru_keys[], write_lru_keys[], nvme_staged_write_file_ids[], nvme_read_cache_block_keys[],
active_writes{ino: [block…]}, pack_open_blocks_census[],      # KEY CENSUS — empty unless armed; keys always present
active_leases_count, volume_states[{id, backing_dev, state, healthy, capacity_bytes, used_bytes, free_bytes}],
placement{backend_fill_spread, backends[{id, backend_placement_picks, backend_placement_weight, backend_fill_ratio, eligible}]},
metrics{ … ≈ 900 inline keys + 328 post-macro inserts … },
cache_capacities{read_lru_{current,max}_bytes, write_lru_{current,max}_bytes, nvme_staging_{current,max}_bytes, nvme_read_cache_{current,max}_bytes},
internal_caches{metadata_cache_size}
```

Inside `metrics` (`R:11277-12868`): the counters (`fuse_ops`, `fuse_op_watchdog_overdue`,
…), the R5 budget gauges (`mem_budget_*`, `mem_budget_components{name: {current, floor,
weight, sheds}}`, `R:11318-11354`), `detached_task_panics` and `invariant_tripwires`
(`R:11596-11600`), `lock_ticked_reregisters` (`R:11606`), the timer gauges
(`R:11615-11618`), the ALWAYS-ON phase histogram families (29 `*_ns` keys:
`write_pipeline_phase_ns, meta_op_phase_ns, lock_phase_ns, read_serve_phase_ns,
read_fill_phase_ns, ipc_direct_phase_ns, ipc_ingress_ns, read_transport_phase_ns,
write_transport_phase_ns, zc_bridge_phase_ns, fuse3_fused_timeline_ns, transport_reap_gap_ns,
publish_phase_ns, meta_txpass_phase_ns, uring_fs_write_phase_ns, transport_spin_ns,
ipc_session_admission_ns, ipc_drain_pass_ns, ipc_drain_flush_ns, meta_ship_*,
dlm_*_phase_ns, fsync_phase_ns, membership_renew_*`, `R:11953-12003` and the `_ns` census),
`daemon_cpu_ns` + `daemon_cpu_ns_by_class` (ONE sample, per-thread classes scanned before
the process total so `Σ classes ≤ total`, `R:11216-11218, 11963-11967`), the op-trace
gauges `op_trace_{armed,samples,dropped,divisor}` ("a stats read never drains",
`R:11219-11223, 11973-11976`).

Post-macro insertions (`R:12884-14800`): membership/free-grace gauges export `*_mode: "off"`
ALONE when unarmed "rather than a block of zeroes that would look like a broken plane"
(`R:12884-12907`); `alloc_reachable_blocks` only when the grace plane is engaged
(`R:12908-12913`); the D-3 stripe census flat (`R:12914-12920`, `stripe_census_entries`
`R:2287`); the whole `meta_kv_*` family only when a metadata volume is mounted (`has_meta`,
`R:11129-11134, 12932`); `writer_guard_{mode[],fenced[],pr_reacquires[],term[]}` per volume
(`R:14740-14773`); and the `SQUEEZEFS_OP_PROFILE` families ONLY when armed — "a disabled
mount's stats surface is byte-identical to pre-M2 (the zero-cost contract extends to the
JSON)": `fuse_op_phase_ns`, `fuse_create_under_lock_ns`, `fuse_op_profile_inflight`,
`fuse_write_phase_ns`, `block_lock_wait_by_site`, `block_lock_stripe_audit`,
`fuse_write_inflight` (`R:14775-14799`).

Histogram shape — THE one export every family uses (`histogram_json`, `R:5039-5064`;
`HIST_JSON_*` keys `R:5031-5034`): `{"buckets": {"<=1us": n, …, ">16s": n}, "count": c,
"sum_ns": s, "mean_ns": m}`; 26 power-of-two µs buckets PLUS exact `count`/`sum_ns` words
because "before them every quoted mean was a bucket-midpoint estimate with up to 2×
per-bucket error and no containment law was checkable to the ns" (`R:5007-5012`). Sharded
histograms fold stripe-wise at read (`R:5200-5225`). The queue-depth histogram is a flat
`{"0","1","2","<=4",…,">4096"}` map over 15 buckets (`R:5228-5298`). The port's
`LatencyHistogram.Snapshot.writeJson` is byte-compatible with the latency shape
(`P:src/core/counters.zig:143-150`).

Counters: the reference's `ShardedAtomic` sums stripes with relaxed loads — "exact for
counts that happened-before the read (the stats-snapshot contract); concurrent increments
may or may not be included" (`R:4941-4953`). Port: `ShardedCounter.load` is the same fold
(`P:src/core/counters.zig:71-78`).

Tripwires: the reference exports the TOTAL only; the per-site census
`INVARIANT_TRIPWIRE_SITES` is an in-process `scc::HashMap<&'static str, AtomicU64>` read by
tests to allow-list one routed site (`R:src/lib.rs:548-562`); `note_invariant_tripwire`
increments total + site and logs one line per site per second (`R:584-610`). The charter's
"`invariant_tripwires` with a per-site label" is therefore NOT met by the reference's JSON
(§7 Q2). **PORT:** `"invariant_tripwires": N, "invariant_tripwire_sites": {"site": n, …}`
(every site that ever fired, via `counters.forEachTripwireSite`,
`P:src/core/counters.zig:309-315`), plus `"tripwire_sites_overflow": n`
(`P:src/core/counters.zig:303-306`).

Closure identities (charter §10; already asserted by the smoke on the SIGUSR1 tail, `P:src/
cli/smoke.zig:1352-1379`): `admit_write_{overlay,inplace,accumulate,inline}_bytes Σ ≡
transport_zc_write_{extract,direct}_bytes + fuse_truncate_zero_tail_bytes + write_gap_zero_bytes`;
`read_user_bytes ≡ read_copy_dest_bytes + read_copy_bounce_bytes + read_dest_dma_bytes +
read_zc_serve_bytes + read_zc_pool_serve_bytes`; `terminal_frees ≡ block_free_reclaim_queued
+ block_free_reclaim_elided`; `unpublished_mint_abandons ≤
write_pipeline_discards` (a superseded upload publishes, decision 0003 §AJ.7); the transport's `parked_commits ≡ unparked_commits` and
`fast_dispatch_serves + fusions + demotes ≡ READs` (charter §4). The inode states nothing
new: it is the ONE place every term of every identity is readable in one snapshot.

Key census (VAL-7a, `R:554-568, 10918-10977, 11258-11273`): `stats_key_census_enabled()` is
read LIVE on every build ("this is the cold `.stats` open path, and a live-flip is exactly
how an operator uses it" — `std::env::var` per call is acceptable here, never on an op).
Armed: `read_lru_keys`, `write_lru_keys`, `nvme_read_cache_block_keys`,
`nvme_staged_write_file_ids`, `active_writes{ino: [block]}` (scope-tolerant parse of
`active_block:` keys, `R:10943-10966`), `pack_open_blocks_census` (device offsets). Disarmed:
the same keys with empty values "so an operator can always key on them"; the `*_count`
replacements export unconditionally — `active_write_block_count` and
`nvme_staged_write_file_count` are what `squeezefs umount` reads to decide whether staged
custody is unflushed (`R:src/main.rs:7531-7534`). The port's equivalents: the staging
ring's live records (`staging.zig`), the hot tier / NVMe read cache / read LRU key sets, the
pack's open blocks (`pack.zig`), the write pipeline's in-flight table — each gains a
`writeCensusJson(w)` the inode calls only when armed.

`SQUEEZEFS_OP_PROFILE`: already armed once at mount in the port (`P:src/fuse/handler/
op_trace.zig:182-199`); the inode adds `fuse_op_phase_ns{class: {phase: hist}}` and
`fuse_op_profile_inflight` under the same armed-only rule (the knob doc already promises a
byte-identical disarmed surface, `P:src/core/knobs.zig:157`). `fuse_op_latency_ns{class:
hist}` is ALWAYS on (`P:src/fuse/handler/op_trace.zig:201-203, 216-226`).

`op_trace` exposure: `op_trace_armed`, `op_trace_samples`, `op_trace_dropped`,
`op_trace_divisor` in `.stats` (`R:src/op_trace.rs:280-288`); the samples themselves only in
`.trace` (§1.6).

**PORT schema** (the SIGUSR1 families become ONE object; every family ALWAYS present,
zero-valued when its mechanism is inert — the reference's own "every-gauge-0-unarmed law",
with the ONE exception the knob doc already states, op_profile):

```
{ "build": {"train","commit","profile"},                                  # main.zig:15-24 Version
  "mount": {"uid","gid","mountpoint","sqmeta_uri","sqdata_uri","read_only","writeback","direct_device_true"},
  "stats_key_census": bool,
  "transport": {…Session.writeStatsJson…},                               # session.zig:1302
  "data": {…DataPlane.writeFamiliesJson…, "census": {…} iff armed},      # plane.zig:1806
  "meta": {"conveyor","conveyor_phases","meta_barrier","checkpoint","journal_lane",
           "node_cache","tree","smo","leaf_merge","alloc_ext","journal","ledger","record",
           "key","tx","economy","inos","dlm","heap_law","node","node_snapshot","node_state"},
  "fuse": {"lifecycle","op_guard","open_table","watchdog","kv_fs"},
  "exec": {…SqzIo.writeStatsJson…}, "sync": {"lock_ticked_reregisters","channel_ticked_reregisters","arc_swap"},
  "io": {"uring_fs","nvme_derive"}, "mem_budget": {…},
  "ops": {"fuse_op_latency_ns": {class: hist}, "fuse_op_phase_ns": {…} iff SQUEEZEFS_OP_PROFILE, "fuse_op_profile_inflight" iff},
  "op_trace": {"armed","samples","dropped","divisor","pool_bytes"},
  "cpu": {"daemon_cpu_ns","daemon_cpu_ns_by_class": {class: ns}},         # exec/cpu_class.zig
  "daemon": {"fuse_ops","fuse_ops_inflight","volume_failed","open_count_stranded",
             "invariant_tripwires","invariant_tripwire_sites": {site: n},"tripwire_sites_overflow"} }
```

Gauge NAMES inside each family stay the charter's/reference's (`transport_*`, `meta_kv_*`,
`read_*`, `write_pipeline_*`, …) — the smoke's `numberAfter("\"name\":")` reads are
name-keyed, not path-keyed, and keep working. The writers are generated, not hand-listed
(§7 Q1/Q4): `counters.writeNamespaceJson(comptime Ns: type, w)` walks the `pub var`
declarations of a `stats` namespace and emits each by type (`ShardedCounter`/`Gauge` →
number, `Tripwire` → number, `ShardedLatencyHistogram` → histogram, a `PhaseHistograms`
struct → object), so a counter declared is a counter exported; a static rail
(`tests/rails.zig`) asserts every `counters.*` declaration in `src/` is reachable from the
`.stats` tree (the inverse census, like the knob rail).

### 1.5 `.config`

`generate_config_json` (`R:10747-10888`): `{client_version, format{disk_cache_paths: "a,b"},
data_volumes{id: {id, backing_dev, uri: "sqdata://…", status: enabled|disabled, health}},
metadata_volumes{meta_volume_N: {backing_dev, uri, status, health}}, uid, gid, block_size,
sqmeta_uri, sqdata_uri}` — the joined-member URIs are "the exact strings the CLI verbs
accept" (`R:10870-10885`) and `meta_uri_of_live_mount` reads `sqmeta_uri` back to let a
verb accept a mountpoint (`R:src/main.rs:1983-2000`). The table lists EXACTLY the
registered volumes — a phantom `backend_0` alias row was a user-reported bug (`R:10761-10770`).

**PORT:** `.config` = `{build, mount{uid,gid,…}, set: set_config.Config.writeJson (the
rung-4 set config: set_uuid, block_size, compression, encryption, data[{vol_tag, state,
device_uuid, uri, capacity_blocks}], cache_paths[]), sqmeta_uri, sqdata_uri}` — the port's
`status` already renders this config offline (`P:src/cli/status.zig:224-228`); the inode is
its live twin. `clients`/`df`/`clone` accept a mountpoint by reading `.config`'s `sqmeta_uri`.

### 1.6 `.trace`

`lookup(1, ".trace")` DRAINS every ring into one immutable generation ("a second lookup
reads empty"; `R:27163-27176`); payload = `{"armed", "divisor", "dropped", "samples_total",
"clock": "CLOCK_MONOTONIC_ns", "stages": {"<id>": "<name>"…}, "samples": [[op_id, stage,
mono_ns]…]}` sorted by `(op_id, mono_ns)`; `samples_total`/`dropped` cumulative over the
ring's life (`R:src/op_trace.rs:247-278`; key set pinned by `op_trace_tests::
trace_json_shape_is_pinned_and_drains`, `R:tests/op_trace_tests.rs:638-660`). Owner-only like
`.stats` ("a trace names inos, offsets and op ids", `R:14865-14868`). The `op-trace on|off|
status` admin verb (`R:src/op_trace.rs:217-245`) is rung 7 (admin lane).

**PORT:** same payload; `stages` from `Stage.all` (`P:src/fuse/handler/op_trace.zig:123`);
`mono_ns` is `clock.monotonicNs` directly (L13). Known quirk kept: a `stat .trace` drains
(§7 Q17).

### 1.7 The reserved-xattr screen (hides `job:` / control records) and `il0`

`xattr_name_allowed` is an ALLOWLIST (`R:src/meta_backend/kv/backend.rs:700-744`): `user.*`
minus the `user.squeezefs.` family, `security.*`, `trusted.*`; everything else — `job:*`,
`system.symlink`, `layout`, `writer_claim`, `client:{id}`, the format config, "and anything
added later" — is EPERM on set/remove, absent on get, filtered from listings; refusals count
`fuse_reserved_xattr_refusals` (`R:30663-30668`). WHY an allowlist: the pre-RC denylist
(`job:` + `user.squeezefs.`) left `system.symlink` (the VFS performs NO permission check for
`system.*`), `layout` (a decodable foreign value redirects reads to another file's blocks),
`writer_claim` (removing it presents the set as unclaimed) and `client:{id}` writable from an
unprivileged shell (`R:706-722`). The ONE name admitted inside the reserved family is the
gather-mode opt-in `GATHER_XATTR` (rung 6, `R:734-737`); `user.squeezefs.stripes=<K>` is a
COMMAND on an armed symmetric mount (`R:30637-30662`, rung 6). POSIX ACL names are
`EOPNOTSUPP` (`R:30634-30636`). Virtual inos: `setxattr` EPERM, `getxattr` ENODATA.

`user.squeezefs.il0` (rung 7 — note only): synthesised at `getxattr` ONLY when the session
host is armed, size probe / ERANGE / data; disabled mounts answer ENODATA "the cheap
negative probe"; the reserved name never serves on-disk bytes (`R:30699-30717`); filtered
from listxattr; EPERM on set (charter §7).

**PORT:** `kv_fs.isReservedXattr` screens only `layout` and `system.symlink` today
(`P:src/fuse/handler/kv_fs.zig:394-396`). Rung 5 replaces it with the allowlist: `user.*`
minus `user.squeezefs.`, `security.*`, `trusted.*` (the port keeps control records in the
control tree, not as ino-1 xattrs, so the screen protects the two record names plus the
`client:`/`writer_claim` records rung 5 adds); the refusal is `error.PermissionDenied` on
set/remove, `error.NoAttribute` on get, skipped in list, counted by
`fuse_reserved_xattr_refusals` (a `ShardedCounter`, not a tripwire — a probing tenant is
not a cannot-happen).

### 1.8 Who reads the inode

`squeezefs umount` (`R:src/main.rs:7531-7534`: `nvme_staged_write_file_count`,
`active_write_block_count` → drain prompt), `squeezefs bench` (`R:src/bench.rs:1147-1162`:
every `metrics` key that is a u64, delta per iteration; `direct_device_true` for the
posture line, `R:1362-1377`), the supervisor (`<mountpoint>/.stats` is the probe inode,
`R:src/main.rs:516`; the port's supervisor stats the mount ROOT today —
`P:src/cli/supervisor.zig:13` — rung 5 points it at `.stats`, the charter's "control
inode"), the rigs' 1 Hz poll (`DRAIN_INTERVAL_S`, `R:src/op_trace.rs:62-64`), the smoke.

## 2. THE VERB TABLE

Grammar in the reference: clap derive, `Cli{--log-file (global), command}`
(`R:src/main.rs:12-26`), `Commands` enum (`R:29-852`). Exit codes: `main` returns
`Result<(), Box<dyn Error>>` → any `Err` prints `Error: …` and exits 1; explicit
`exit(1)` on the knob gate (`R:2784-2787`) and the mount preflights; `exit(130)` on Ctrl-C
in a guarded verb (`R:8896`); clap usage errors exit 2. Refusal text = the `Err` string.
"R#" = rung.

| Verb | Flags / positionals (reference) | Output | Refusals (text class) | R# |
|---|---|---|---|---|
| `format URIS… [--block-size 4M] [--capacity] [--inodes 1000000] [--mem-cache-size] [--disk-cache-size] [--read/write-cache-size] [--read/write-mem-cache-size] [--disk-cache-paths a,b] [--data-lv] [--meta-lv] [--ip/--port/--subnqn] [--multi-writer] [--single-writer] [--symmetric] [-f/--force] [--full] [--compression] [--encrypt-algo] [--encrypt-key PATH|-] [--dismount-wait] [--upload-delay] [--fuse-io-uring-sqpoll-idle-ms] [--meta-node-kib 256] [--meta-journal-mb] [--meta-slots (RETIRED, hidden)]` (`R:31-200`) | `Format complete.` (`R:4607`) | live client (format_preflight); oversubscription; retired flag | landed (port: `P:src/cli/args.zig:32-57`) |
| `mount URIS… MOUNTPOINT` + ≈ 35 flags (`R:430-664`) incl. `--daemon`, `--supervise` (requires daemon), `--uid/--gid`, `--no-writeback`, `--interception`, `--admin-uid`, `--allow-other`, `--read-only`, `-o OPTS`, `--job-cpu-limit 50`, `--write-verification[-sample]`, `--encrypt-key`, `--mem-budget` | daemon log | `--disk-cache-paths` → "cache paths are fixed at format; use `squeezefs config set-cache-paths`" exit 1 (`R:2821-2830`); `--fabric-endpoints` likewise | landed (port `-o`, `--daemon --log-file [--supervise]`) |
| `umount [-g URI] MOUNTPOINT [-f]` (`R:670-685`) | drain prompt from `.stats` counts, `All active write blocks drained cleanly!` | — | landed (port detach only; the staged-promote prompt lands with the inode) |
| `status [URI\|mountpoint]` (`R:202-205`) | pretty JSON `{"Setting": {Name, BlockSize, Capacity, Inodes, Compression, EncryptAlgo, [MemCacheSize, DiskCacheSize, DiskCachePaths], StorageBackends{id: {id, backing_dev?, status}}}, "Clients": [registration…], "Fabric"?}` (`R:32430-32539`) | "no metadata volume path specified or active mount found"; "Volume not formatted" | landed in the port as a text report (§0); rung 5 adds the `Clients` section |
| `clients URI\|path [--json]` (`R:211-217`) | §4.7 | "cannot probe metadata volume '{path}': {e}" | **5** |
| `df [-g URI\|path] [--json]` (`R:823-839`) | §4.6 | "no metadata volume specified — pass sqmeta://<meta_dev> …"; "carries no format config — volume not formatted"; "format config names no data volumes"; "cannot size data volume" | **5** |
| `bench MOUNTPOINT [-w] [-r] [--stat] [--del] [-t N] [-n N] [-s SIZE] [-b SIZE] [--rand] [--direct] [--time S] [-i N]` (`R:701-774`) | §3 | shape errors (§3.2) | **5** |
| `clone [-g URI] SRC DEST` (`R:782-799`) | `Cloning file from {src} to {dest} (metadata-only CoW)...` / `File cloned successfully.` (`R:7083-7088`) | no URI → "no metadata volume specified — pass sqmeta://… `clone` resolves both paths through the metadata set and takes the single-writer guard over it" (`R:7076-7081`); live mount; missing source; root as source; staged source; partial-mode map; contended map after 3 attempts (§4.1) | **5** |
| `tune` (`R:803`) | §4.2 | — (never fails) | **5** |
| `config [-g URI\|mountpoint] ACTION` (`R:805-817, 1545-1632`): `set-cache-paths URI PATHS… [-f/--yes]`, `get-cache-paths URI`, `set-fabric-endpoints URI vol=traddr:trsvcid:subnqn…`, `get-fabric-endpoints URI`, `data-volume {list,enable ID,disable ID}`, `metadata-volume {list,enable,disable}`; REMOVED stubs: `set KEY VALUE`, `list`, `fsck`, `disk-cache {add,remove,enable,disable,flush,list}`, `data-volume {add,remove,migrate}`, `metadata-volume {add,remove,migrate}` → "`squeezefs {verb}` was removed: {why} Superseded by `{successor}`" exit 1 (`removed_verb`, `R:2763-2769`) | §4.3 | "cannot change cache paths: {e}"; live mount; metadata-volume enable/disable offline → "needs a live mount (admin lane)" | **5** for cache paths; fabric endpoints 8; health overrides 7/8 |
| `claim clear URI` (`R:425-428, 855-872`) | per volume: "no writer claim present — nothing to clear" / "cleared stale writer claim (holder id=…, pid=…, boot=…, last heartbeat ts=…). This was an operator attestation…" (`R:5349-5387`) | flock held → "refusing to clear the writer claim — the volume is live-mounted"; fresh client registrations → "… has live client registrations: […]"; fresh claim → "… within the {ttl}s ttl and may be alive. Stop that writer (or wait …)" (`R:backend.rs:18879-18945`); exit 1 "claim clear refused/failed on {n} of {m} volume(s)" | **5** |
| `staging {adopt,discard} --slot HEX8 URI\|path` (`R:257-260, 1239-1270`) | §4.4 | pending write custody refuses adopt; live mount | **5** (the charter's `staging` verb; the reference has no list/promote/drain — see §4.4) |
| `appenders URI [--json]`; `appender clear URI ID [--volume N]` (`R:219-236, 875-897`) | one row per appender page | volume without incompat bit 17 | 6 |
| `volume {add-data TARGET DEV [--no-rebalance], list TARGET [--json], remove-data TARGET ID [--throttle], undrain TARGET ID, repair-set URI, add-meta URI DEV [--take-slots], enable-multi-writer (RETIRED), enable-symmetric URI [--resume\|--dry-run\|--abort], migrate-meta-slot MNT SLOT VOL, locate TARGET PATH [--json], remove-meta URI VICTIM}` (`R:1273-1498`) | | | 8 (`locate` 6) |
| `job {list T, status T ID, pause, resume, cancel, throttle T ID PCT, worker URI, rotate-enroll URI}` (`R:1501-1542`) | | | 8 |
| `fsck TARGET [reports…] [--online\|--offline] [--throttle 100] [--json] [--shards k/N] [--scrub] [--repair] [--apply] [--quarantine-dir]`; `scrub TARGET [--throttle] [--json]` (`R:292-360`) | nonzero exit when findings exist | | 8 |
| `defrag TARGET [--data] [--pack] [--volume ID] [--meta] [--fold] [--rebalance] [--report-only] [--throttle] [--json]` (`R:378-421`) | | | 8 |
| `nvmeof {share, unshare, list, restore, adopt, connect, disconnect, resv-report, target {setup,start,stop,status,systemd-unit,install (RETIRED)}}` (`R:1014-1236`) | | | 8 |
| `storage {pool {create,add,remove,delete,list}, volume {create,extend,delete,list}}` (`R:900-990`) | LVM wrappers | | 8 |
| `--version` / `-V` | `squeezefs <train> (<short>[-dirty] / <full>[-dirty][, tag T]) built <utc> profile <p>` (`R:src/version.rs:70-72`) | | landed (`P:src/main.zig:15-24`) |

Common helpers: `parse_block_uri(uri, "sqmeta://")` splits members on `,` (`R:1950`; the
port's `uri.parse` is the ONE parser and refuses relative paths, 0002 row 92);
`find_squeezefs_mounts()` scans `/proc/mounts` for `fuse*` and probes each
(`R:8205-8223`); `resolve_config_target` → `Offline(members)` for `sqmeta://`, `Live
(mountpoint)` otherwise, auto-picks the ONE live mount else refuses naming the count
(`R:2737-2758`); `admin_roundtrip(mnt, verb, arg)` reads the `il0` bootstrap xattr and
speaks the admin lane (`R:2002-2033`; rung 7).

**PORT grammar additions (rung 5)**, in the typed `Verb`: `clients: {meta_uri, json}`,
`df: {meta_uri, json}`, `bench: {mountpoint, write, read, stat, del, threads?, files?,
size?, block?, rand, direct, time?, iterations}`, `clone: {meta_uri, src, dest}`, `tune:
{apply}`, `config: union{set_cache_paths{meta_uri, paths, force}, get_cache_paths
{meta_uri}}`, `claim_clear: {meta_uri}`, `staging: union{list{meta_uri}, adopt{meta_uri,
slot}, discard{meta_uri, slot}}`. `meta_uri` is a positional everywhere and accepts a
mountpoint (resolved through `.config`'s `sqmeta_uri`); no `-g`, no `SQUEEZEFS_META_URI`,
no aliases, no retired stubs (§7 Q7/Q9). Exit codes stay 0/1/2.

## 3. `bench`

### 3.1 Model

"Simplified-elbencho": a persistent, reusable dataset at `<mountpoint>/squeezefs-bench/
t{tid}/f{fid}.bin`, one shape vocabulary `threads × files × size @ block`, sequential or
`--rand` (a shuffled FULL-COVERAGE permutation — every block exactly once), optional
`O_DIRECT` (`R:src/bench.rs:1-20, 32, 269-278, 359-365`). The engine drives the MOUNT
through ordinary POSIX file I/O — "bench is the load generator measuring the mounted
filesystem through the kernel, exactly like elbencho. Do not route bench I/O through
`crate::uring_fs`; the io_uring mandate governs the filesystem's own data paths, not this
measurement client" (`R:9-15`). It never drives the backend directly.

Write pattern: deterministic non-zero xorshift64* stream seeded per `(tid, fid, block)`
through splitmix64 of the tag `0x5351_4653_4245_4E43` ("SQFSBENC") "so compression cannot
fake numbers and reads could in principle verify content" (`R:16-18, 367-412`).

Phases in fixed order Write → Read → Stat → Del (`R:77-87, 774-792`). Write opens
`write|create` WITHOUT `O_TRUNC` ("iteration N+1 measures steady-state overwrites of the
same inodes"), per block `seek` + `write_all`, latency stamped per op, then `set_len(size)`
+ `sync_all()` INSIDE the timed phase — "honest durable write numbers" (`R:468-486, 493-546`).
Read = `read_exact` per block with an exact short tail (`R:548-587`). Stat = `metadata()`;
Del = `remove_file()` per file (`R:589-623`). Time box: checked AFTER each op "so every
worker completes at least one operation" (`R:523-532`); default 30 s for `--rand`,
unlimited (full coverage) for sequential, `--time 0` forces full coverage (`R:50-54,
969-977`).

### 3.2 Shape resolution and refusals

Auto: `threads = clamp(available_parallelism, 1, 16)` (`R:35, 818-820`); `total = max(16 GiB,
2 GiB × threads)` capped at `free/4` rounded down to 1 MiB, refused below 4 GiB
(`R:37-45, 826-841`); `files = 1`; `size = total / (threads × files)` rounded down to 1 MiB
"so `-s % -b == 0` holds for BOTH suite block sizes (4k divides 1m)" (`R:43-45, 846-864`);
free space for sizing = statvfs free + the existing dataset's bytes (`R:868-899`); explicit
flags always override (`R:920-967`). The header prints `(auto)`/`(explicit)` per value
(`R:1260-1352`).

`validate_shape` (`R:229-266`): threads/files/size/block ≥ 1; `--direct` requires `block %
4096 == 0` and `size % block == 0` ("O_DIRECT cannot write a partial EOF tail block").
`validate_dataset` (`R:300-350`) refuses a missing/wrong-size/stray-file dataset loudly and
"creates nothing". The bare invocation refuses `-b` and `--rand` ("the bare invocation runs
the fixed saturation suite", `R:1508-1528`); `--iterations 0` refused (`R:1504-1506`).
Permission denied on the mountpoint names the FUSE `allow_other` rule (`R:1530-1542`).

### 3.3 The suite and the rows

`suite_passes` (`R:995-1058`): write seq 1m direct → read seq 1m direct → read rand 4k
direct (boxed) → write rand 4k direct (boxed) → stat → del; the del pass "leaves the mount
clean". `SUITE_SEQ_BLOCK = 1 MiB`, `SUITE_RAND_BLOCK = 4096` (`R:46-49`). Explicit phases
share one shape; a set without Write validates the dataset before timing (`R:1064-1080`).

Header (`R:1278-1352`): mode, passes, `threads=… files/thread=… size=…/file (… B; total …)`,
block/access/io/time-box/iterations, and the **direct-I/O posture** sniffed from `.stats`'
`direct_device_true`: "device-true (-o direct_device_true): O_DIRECT rows measure the DEVICE
path" vs "hybrid (default): O_DIRECT rows may serve from the read tiers once warm"
(`R:1353-1377`) — a measurement note, never a gate.

Row (`R:1390-1454`): `| PASS | THROUGHPUT | IOPS | COV | LAT MIN | LAT AVG | LAT P99 | LAT
MAX |` with `pass_desc = "<Phase> <block> <seq|rand>"`, throughput MiB/s (byte-moving phases
only), coverage `ops/expected_ops` with a note line when a box expired, p99 = nearest-rank on
the sorted per-op latency vector (`R:626-632`). Per iteration a `.stats` delta table
(`R:1456-1495`): `fuse_ops, meta_updates, put_obj, get_obj, del_obj, cache_hits, cache_misses`
— and a note when `.stats` is unreadable.

### 3.4 What the measurement law requires that the reference's rows lack

Charter §11: a row states instrument/substrate/venue/profile; write rows carry `device
bytes ÷ user bytes`, `wareq-sz` vs block size, and the reclaim ledger; headline rows are
≥ 60 s; A-B-B-A brackets on an aging store. The reference's row has none of the amplification
terms (its delta table is seven counters), prints a profiling warning via
`profiling_build_warning` (`R:src/main.rs:7036-7046`), and colours cells by fixed thresholds
(`R:1399-1428`).

**PORT `bench` row** (per pass, machine-parseable and human): `pass, block, access, io,
threads×files×size, elapsed_s, ops, bytes, MiB/s, IOPS, coverage, lat{min, mean, p50, p99,
p999, max}` from a `counters.LatencyHistogram` per worker folded at the end (26 log2 buckets
+ exact count/sum; p-quantiles reported with the bucket's upper bound, never a midpoint —
§7 Q13), PLUS from the `.stats` delta: `user_bytes` (the pass's bytes), `device_write_bytes`
(`nvme_dev` write bytes), `amplification = device_write_bytes ÷ user_bytes`, `wareq_sz =
device_write_bytes ÷ device_writes` beside the block size, the reclaim ledger `{terminal_frees,
block_free_reclaim_queued, block_free_reclaim_elided, discard_bytes}`, `fuse_ops`,
`transport_ring_requests`, `transport_enters`, the engagement gauges of the mechanisms the
pass claims (`held_stores`, `patch_writes`, `hot_block_hits`, `fast_dispatch_serves`,
`transport_zc_*`), and the must-stay-0 census (`invariant_tripwires`, the smoke's list
`P:src/cli/smoke.zig:1306-1334`). Header: `instrument=squeezefs bench <train> <commit>
profile=<gate|dist>`, `substrate=` the data devices' transport read from
`/sys/block/<dev>/device/transport` (`loop`/`tcp`/`pcie`), `venue=` `uname -r` + hostname,
the direct-I/O posture, and `warning: profile gate is not a dist measurement` when the
build is not `dist` (the binary names its profile, `P:src/main.zig:15-24`). `--time` default
for headline rows: the operator passes `--time 60` for a claim row; the bench prints
`scoping-only (<60 s)` on any shorter data row.

### 3.5 Smoke mode (the gate)

The reference's gate runs `cargo bench --benches -- --test` — "Criterion smoke — one iteration
per bench, no measurement" (`R:Taskfile.yml:189-190`; gate.yml:8, 18-19: "Per-commit gets
bench SMOKE only"). Ten Criterion binaries (`R:benches/`: `copy_path`, `high_concurrency`,
`ipc_hop`, `membership`, `meta_lv`, `nvme_issue`, `read_path`, `squeezefs`, `write_path`,
`zcrx`) ≈ 200 bench functions (e.g. `meta_lv_bench`: `create_unlink_file`, `lookup_file`,
`set_get_xattr`, `readdir_page_100_of_1k`, `merge_8x256`, `fold_lookup_4src_delta_chain`,
`point_lookup_hot_100k`, `entry_encode_checksum_create`, `core_admit_reserve_advance`;
`read_path_bench`: `insert_serve4_retire_4m`, `dense_seek_hole_1024_blocks`, `oneshot_cross_
thread_hop`, `serve_phase_record_8threads`; `write_path_bench`: `flush_serial`,
`record_write_4x1m_inorder`, `park_16x4k`).

**PORT:** `zig build bench` builds `src/bench/*.zig` microbenches (one `pub fn main` per
family: `codec` (record/bset/node/key fold), `counters` (sharded inc, histogram record,
tripwire fire), `journal_core` (admit/reserve/advance), `node_cache` (hit/miss/evict), `lz4`,
`checksum`, `sync` (mutex uncontended/contended, notify hop), `exec` (fiber spawn/park/wake
hop), `data` (block_alloc mint/free, refs stage, layout merge)); `zig build bench-smoke`
runs every microbench for ONE iteration with `--smoke` (asserts it runs and prints its row)
and joins `gate` (`P:build.zig:157-162`). The in-binary `squeezefs bench` is NOT in the gate
(it needs a mount); its unit tests (§6) are.

## 4. The rung-5 verbs at the data-plane level

### 4.1 `clone` — instant CoW, offline, under the D0 guard

`clone_path_offline(meta_lvs, src, dest)` (`R:src/config_ops.rs:1322-1369`): per volume
`format_preflight(path, true)` → "clone refused: metadata volume '{path}' is not exclusively
claimable (a live writer may hold it — the single-writer guard forbids offline clones under a
live write mount)"; `open_routed_meta_set` takes the D0 claims (flock + PR + `writer_claim`)
"unconditionally, not only when a flag happens to be passed"; the mount-shaped offline
router runs allocator REFCOUNT RECOVERY ("without which every striped pin refuses as
untracked"); guards release on every path (`R:1362-1368`). WHY offline-only: "the verb was
a silent no-op before this function existed … printed success having created nothing"
(`R:1326-1330`); on a live mount the clone is `cp --reflink=always` → `copy_file_range`
(`R:1396-1400`).

Body (`R:1373-1407`): resolve the source FIRST (missing → "cannot be resolved"; ino 1 →
"resolves to the filesystem root — name a file"); a STAGED source refuses ("its acked payload
lives in the mount's isolated staging … cloning it would mint a zero-filled clone");
`router.clone_path` then `reclaim_drain()` before the claims release (the claim-release law).

`clone_path` (`R:src/routing.rs:23835-23881`): `create_with_rdev(parent, name, mode, uid,
gid, rdev)` a NEW inode, `setattr(size)`, then `clone_file(src_key, dest_key)`.
`clone_file` striped arm (`R:22042-22156`): refuse a block with an undrained staged
`active_block:` sibling or extent record ("ACKED CUSTODY the clone cannot carry by
refcount"); refuse a partial-mode map; then an all-or-nothing VALIDATED pin round over the
source map — `pin_block_validated` returns `Pinned` / `PinnedUnstable` (pin-CAS →
`fence(SeqCst)` → incarnation snapshot saw the block's word unstable: a W1 patch mid-DMA) /
`Refused` (freed since the fetch); any non-`Pinned` undoes the round (`free_block` = one
decrement), refetches the authoritative map under the meta lock and retries; `attempt ≥ 3`
is the bounded EBUSY-class refusal ("a point-in-time clone of a hot-mutating file has no
progress guarantee worth a new lock-order edge"). Then the DURABLE half (`R:22157-22176`):
one `+ref` record per dest map entry staged into the dest's layout commit "so a remount reads
refcount 2 for a shared block instead of re-deriving it from a tree walk. Without this the
§6.3 W1 hazard is exactly reproducible across a remount". On an armed forest set the
`MarkShared`/`ShareBlock` protocol runs BEFORE the dest publishes and the dest's refs carry
the SHARED bit, "a clone of a clone inherits it" (`R:22177-22211`; rung 6).

Test contract (`R:tests/cli_clone_tests.rs:338-409`): the dest is a NEW ino; `dest_map ==
src_map` (identical keys, nothing copied); every shared block's refcount goes 1 → 2; the
clone reads back the source's bytes; no URI / missing source / staged source refuse loudly
and never print "cloned successfully" (`R:411-470`).

**PORT (rung 4 makes it cheap):** `block_refs` already stages `taken`/`takenShared` ops
with the SHARED bit (`P:src/data/block_refs.zig:8-20, 133-143, 230-240`) and the W1 patch
class refuses a block whose refcount ≠ 1 (`write_shared_class_violation`). Offline clone =
open the set with the writer guard (`format`'s preflight), `KvFs.lookupPath(src)`, refuse
inline-only? NO: an inline source clones by copying the inline bytes into the new inode
(no blocks); a staged source refuses as above; a striped/packed source: read the map,
`+ref` every binding with `takenShared`, write the dest inode + `layout` record + map in ONE
transaction, no DMA. No RAM pin round is needed offline (the guard excludes every writer);
the live clone is rung 7's `copy_file_range`. Output: `cloned <src> → <dest>: <n> block(s)
shared, 0 bytes copied`.

### 4.2 `tune`

`tune_system` (`R:src/main.rs:8755-8859`) is a SYSCTL tuner for the client node, not a knob
retune: prints current then (as root) writes `vm.dirty_ratio = 40`, `vm.dirty_background_
ratio = 10`, `net.core.{rmem,wmem}_max = 67108864`; per `/sys/fs/fuse/connections/<id>`
RAISE-ONLY `max_background` to 256 / `congestion_threshold` to 192 when `< 256` ("mounts
negotiate `max_background` = ring capacity at INIT (often > 256 now) — `tune` must never
LOWER a live connection", `R:8821-8830`); `/sys/class/bdi/0:<id>/read_ahead_kb = 0`
(`R:8833-8851`). Non-root prints a WARNING and checks only (`R:8761-8767`). Every write is
`let _ =` (errors dropped); exit always 0. No test covers it.

The LIVE retune surface in the reference is the admin lane (rung 7): `job-throttle` (the
per-worker duty cycle `sleep t × (100−pct)/pct`, charter §8), `op-trace on|off|status`,
`volume-enable/disable`, `meta-volume-enable/disable` (`R:src/ipc_service.rs:1442-1766`).
Knobs themselves are NOT live-retunable (read once at mount, charter §1.6).

**PORT:** `tune [--apply]` — check mode by default (prints each parameter: current, target,
reason), `--apply` as root writes and REPORTS each outcome; raise-only on live FUSE
connections; `read_ahead_kb = 0` justified on the line ("the daemon prefetches; kernel
readahead doubles device reads"); exit 1 if any apply failed. The sysctl targets carry
their WHY (`dirty_ratio 40/10`: a write-through FUSE mount with `max_background` depth
needs dirty headroom ≥ ring capacity × `max_write`; `rmem/wmem 64 MiB`: nvme-tcp socket
buffers at 4 MiB blocks). The admin-lane retunes stay rung 7.

### 4.3 `config`

`get-cache-paths URI` → the recorded staging roots or `none (cache-less)` (`R:src/main.rs:
7205-7220`; `config_ops.rs:528-533`). `set-cache-paths URI PATHS… [-f]` (`R:7190-7204`;
`config_ops.rs:535-609`): live-client gate on EVERY volume FIRST, the set must be formatted,
the NEW dirs wiped + recreated + ownership-stamped behind the ENG-4 guard (system paths
hard-refused; a non-empty dir with no staging marker needs `--force`; "ALL dirs precheck
before ANY is wiped"), then ONE record rewrite on the config home volume through the
daemon's UNSCREENED internal setxattr (the FUSE allowlist would refuse it) and a clean
shutdown. `set/get-fabric-endpoints` (rung 8), `data-volume list/enable/disable` (durable
state offline, admin lane live; rung 7/8), `metadata-volume *` (live only). Twelve verbs
are `removed_verb` stubs (§7 Q7).

**PORT:** `config show [--json]` and the fabric-endpoint verbs over the rung-4 `set_config`
record in the control tree. `set-cache-paths`/`get-cache-paths` and `format --disk-cache-paths`
went with disk-based caching (2026-10-21, 0002 row 550): the staging ring is the WRITE CACHE
in RAM on every mount, `SQUEEZEFS_WRITE_CACHE_PCT` of system RAM; a `set_config` record that
names cache paths is refused by name (reformat).

### 4.4 `staging`

The reference's verbs are `staging adopt --slot HEX8 URI` and `staging discard --slot HEX8
URI` (`R:src/main.rs:1239-1270, 4660-4711`): a writer-scoped set binds staging roots to a
client slot; a remount at another path strands the old slot's residue. `adopt` re-binds the
root's generation marker (crash-safe two-phase) → "Adopted slot m{slot:08x}'s staging
residue to this node ({n} root(s)):" + roots; pending write custody refuses ("drain it first
by mounting with `-o client_slot=<hex8>`"). `discard` enumerates every destroyed custody key
("the attested-verb loudness class") → "DESTROYED slot m…'s staging residue: {n} root(s),
{k} live staged custody unit(s)." + `root:`/`freed:` lines + "Those staged bytes are gone by
design". `slot_residue_for_set` (`config_ops.rs:3664`) feeds `clients` (§4.7). The charter
names `staging` without subverbs; there is no `list/promote/drain` in the reference.

**PORT:** no `staging` verb (removed 2026-10-21 with the on-disk roots, 0002 row 550): the
write cache is RAM, a dead daemon's ring is held by its supervisor for the member's next mount
(`squeezefs mount` recovers it; `kill` the supervisor discards it), promotion of staged files
is `fsync`'s law and `umount`'s teardown (charter §6.1, §8), and a drain is `fsync`/`umount`,
never a verb.

### 4.5 `claim clear`

`KvMetaBackend::claim_clear(path)` (`R:backend.rs:18873-18960`): layer-A `flock` probe (held
⇒ live same-host mount ⇒ refuse); the live sweep over `client:*` records (any fresh heartbeat
⇒ refuse naming the keys); no `writer_claim` ⇒ `NoClaim`; a claim within `CLIENT_STALE_TTL_
SECS` ⇒ refuse ("may be alive. Stop that writer (or wait …)"); else remove under the writer
bring-up and make it durable → `Cleared(holder{id, pid, boot, ts})`. The ladder ahead of
the verb: same-host dead-pid auto-reclaim → PR preempt → claim TTL expiry; "No mount flag
bypasses the guard" (`R:src/main.rs:862-865`). The claim record is `writer_claim` =
`{"id","ts","pid","boot"[,"term"]}` on ino 1 (`R:backend.rs:746-751`).

**PORT:** the D0 guard lands in rung 5 with this verb; the claim is a control-tree record
(not an xattr), the staleness law ONE constant pair (§5).

### 4.6 `df` — the statfs rule

`run_df_report` (`R:src/main.rs:8491-8698`): read-only probes of every meta volume; the
format config from volume 0; retired records (id tombstones) excluded; the allocator
accounting REBUILT "exactly the way a mount does" by the inode-tree walk
(`recover_active_blocks_v3`, `R:8564-8577`); `total = formatted capacity (quota-aware)`,
`used = Σ allocated striped-block bytes`, `free = total − used`, `inodes.total = quota`,
`inodes.used = Σ live_inodes + 1 (root)` (`R:8579-8594`). JSON (`R:8633-8651`): `{name,
block_size, capacity_bytes, used_bytes, free_bytes, inodes{total, used, free},
data_volumes[{path, size_bytes, allocated_bytes}], meta_volumes[{path, size_bytes,
kv_heap_bytes, kv_heap_free_bytes, kv_heap_used_bytes, next_ino}]}` (schema pinned by
`cli_clients_df_tests::test_df_offline_empty_volume_reconciles_capacity_and_schema`;
`heap_used + heap_free == heap`, `R:tests/cli_clients_df_tests.rs:629-718`). Text: `Data:
capacity … used … (pct%) free …`, `Inodes: quota … used … free …`, a DATA VOLUME table and a
META VOLUME table (`R:8659-8697`).

The live `statfs` (`R:src/fuse_client.rs:29791-29850`): "served entirely from maintained
in-RAM state — statfs is called constantly, so no metadata transactions and no device I/O";
`bsize = frsize = 4096`, `blocks = capacity/4096`, `bfree = bavail = free/4096`, `files =
inode quota`, `ffree = quota − (Σ live_inodes + 1)` — "POSIX-1: the LIVE inode population,
never the monotonic watermark … derived straight it made `IUsed` rise forever"
(`R:29820-29838`). `used` per volume = `get_used_blocks` (`R:src/block_allocator.rs:
3028-3052`): on a grant-armed allocator whose allocation lease THIS process holds, "the
bitmap IS the free list … used = the bitmap's population — every referenced block plus every
open grant, the other writers' windows counted as reserved — MINUS THIS WRITER'S OWN
UNCONSUMED WINDOW"; the `--single-writer` arithmetic `highest − free_list` "read a used count
that only ever GREW on every armed default mount (PR 14: `df` never recovering after
deletes)". `allocated_bytes` dedups by allocator identity (`R:src/routing.rs:3319-3344`).

**PORT:** the live `statfs` already takes the data allocators' space (`P:src/fuse/handler/
kv_fs.zig:1688-1711`); rung 5 switches `ffree` to `INO_SPACE − inos.stats.live_inodes`
(`P:src/meta/inos.zig:47-54, 199-225`) and `used` to the charter rule `bitmap population −
own unconsumed window` once the rung-6 grant window exists (today the window is the whole
allocator, so `used = population`). Offline `df URI|mountpoint [--json]` reuses
`status.readRefCensus` (distinct blocks per volume, `P:src/cli/status.zig:111-158`) plus the
durable bitmap pages — never a full inode-tree walk (§7 Q10) — and the heap figures
`status` prints. Same JSON keys, `capacity_bytes = Σ (capacity_blocks − 1) × block` (the
port has no capacity quota: the set's capacity IS the devices', charter §8 "thin provisioning"
is a by-design exception).

### 4.7 `clients`

`run_clients_report` (`R:src/main.rs:8337-8490`): per meta volume `open_probe` (read-only,
nothing written) → `mount_registrations()`: every `client:{uuid}` record and the
`writer_claim` as `MountRegistration{key, kind: "client"|"writer", id, pid, boot,
heartbeat_ts, age_secs, heartbeat_fresh, holder_provably_dead, job_endpoint}`
(`R:backend.rs:866-897`); state ladder `dead` (same-host `kill(pid,0) == ESRCH`, scoped by
`boot`) > `live` (fresh heartbeat) > `stale` (`R:899-911`); the membership census adds
READERS on an armed plane (rung 6, `R:8353-8373`); this node's staging residue appended
(§4.4). Text: `KIND ID PID STATE AGE VOLUME` rows + "{n} registration(s): {live} live,
{stale} stale, {dead} dead (reclaimable)." or "No client registrations on {n} metadata
volume(s) — no mount (live or crashed) holds this filesystem." + the residue block
(`R:8457-8489`). JSON: `{volumes[], clients[{kind, key, id, pid, boot, heartbeat_ts,
age_secs, state, job_endpoint, volume}], live, stale, dead, staging_slots[{slot, scope,
dir, live_owner, live_units}]}` (`R:8396-8428`, `to_json` `R:backend.rs:913-927`).

Heartbeat law: `CLIENT_HEARTBEAT_INTERVAL_SECS = 10`, `CLIENT_STALE_TTL_SECS = 45`
("stale registrations do NOT block `format` and are reaped on next format attempt",
`R:src/fuse_client.rs:186-195`); `format_preflight`'s predicate is heartbeat freshness alone
(`R:builder.rs:1208-1228`). Tests: forged live/stale records classify; a live mount lists then
zero after a clean unmount; a `kill -9`'d daemon reads `stale`, never `live`
(`R:tests/cli_clients_df_tests.rs:389-627`).

**PORT:** the `client:` records are control-tree records `{client_uuid, pid, boot_id,
heartbeat_s, mount_slot, job_endpoint?}` written at mount and refreshed by a `sqz-timer`-
driven heartbeat fiber every 10 s; `clients` reads them offline at the newest checkpoint
(the `status` pattern) — a heartbeat committed since the checkpoint sits in the journal,
so the verb states "as of checkpoint seq N" (the port's `status` already notes this,
`P:src/cli/status.zig:5-8`); the `dead` arm is same-host only.

### 4.8 `status`

Reference: pretty JSON `{Setting, Clients, Fabric?}` (`R:src/fuse_client.rs:32430-32539`);
with no URI it finds the one mount and reads `.config` — but takes `format.disk_cache_paths`
as the META path (`R:src/main.rs:4616-4635`, §7 Q8). **PORT:** keep the text report; add a
`clients` line per registration and accept a mountpoint through `.config`.

## 5. Knobs, gauges, constants

### 5.1 Knobs (charter §1.6 classes; the port's `Class` set `P:src/core/knobs.zig:57-72`)

| Knob | Kind, default | Class | Reader | Notes |
|---|---|---|---|---|
| `SQUEEZEFS_STATS_KEY_CENSUS` | bool, off (`R:env_knobs.rs:313`) | `switch_` | the `.stats` builder, read live per build (`R:fuse_client.rs:566-568`) | Register in rung 5 (the registry note `P:knobs.zig:96-97`); the ONLY knob read after mount — a `.stats` build is cold, never an op. |
| `SQUEEZEFS_OP_PROFILE` | bool, off (`R:310`) | `ab_lever` (landed `P:knobs.zig:157`) | `op_trace.arm` at mount | armed-only JSON families (§1.4). |
| `SQUEEZEFS_OP_TRACE` | bool, off (`R:311`) | `switch_` | `op_trace.armFromEnv` at mount | geometry derived (`fuse_handlers_lanes.md` §7.1); `.trace` drains. |
| `SQUEEZEFS_DIRECT_DEVICE_TRUE` | bool, off (`R:218`) | `ab_lever` (landed `P:knobs.zig:209`) | the read classifier | bench prints the posture from `.stats`. |
| `SQUEEZEFS_SUPERVISE_INTERVAL_SECS` 5, `SQUEEZEFS_SUPERVISE_UNRESPONSIVE_SECS` 30 (`R:147-148`) | int | — | — | NOT carried: the port's supervisor has the two as constants with their WHY (`P:src/cli/supervisor.zig:13-18`); a knob on a constant is 0002 row 17's class. |
| `SQUEEZEFS_META_URI` | str (`R:141`), clap `env` default for `-g` | — | — | NOT carried (§7 Q9). |
| `SQUEEZEFS_TRANSPORT_DEBUG` (`R:270`) | bool | — | — | NOT carried: the reply-integrity instruments are always on (`R:fuse_client.rs:11031-11037`). |

A/B levers non-default are logged at mount (charter §1.6) — the port's knob gate already
does this; the `.stats` `mount` family also lists them (`"ab_levers": {key: value}`) so a
row's posture is in the row's own snapshot.

### 5.2 Gauges new in rung 5 (every other family's names are their modules')

| Gauge | Kind | Meaning / WHY |
|---|---|---|
| `invariant_tripwire_sites{site: n}` | map of counts | charter §10 per-site label (§7 Q2) |
| `tripwire_sites_overflow` | must-stay-0 | the 512-slot site table overflowed (`P:counters.zig:246-251`) |
| `stats_builds`, `stats_build_bytes_max`, `stats_generations_live`, `stats_generations_evicted` | counters/gauges | engagement of the inode itself; `evicted` ≈ 0 (a kernel that FORGETs) |
| `fuse_reserved_xattr_refusals` | counter | the screen's engagement (`R:fuse_client.rs:6272, 30663-30668`) |
| `client_heartbeats`, `client_heartbeat_failures` | counter / must-stay-0 | the heartbeat fiber; a failed commit is a stale-classified live mount |
| `op_trace_{armed,samples,dropped,divisor,pool_bytes}` | gauges | `R:op_trace.rs:280-288, 232-245` |
| `live_inodes`, `destroyed_inodes` | gauges (landed, `P:inos.zig:47-54`) | `statfs.ffree`, `df` |

### 5.3 Constants with WHY

| Constant | Value | WHY (source) |
|---|---|---|
| `VIRTUAL_INODE_MODE` | `0o400` | the payload maps the daemon's private state and co-tenants' I/O (`R:fuse_client.rs:535-547`) |
| `VIRTUAL_GEN_INO_FIRST/LAST` | `2⁶⁴−2³²` … `…fff0` | disjoint from monotonic real inos (cap ≥ 100 M ≪ 2³²) and below the two canonical inos (`R:36-41`) |
| `VIRTUAL_CLASS_BITS` | 2 | the class survives registry eviction (`R:43-58`) |
| `VIRTUAL_GEN_REGISTRY_CAP` | 256 | 256 × measured ≈ 72 KiB ≈ 18 MiB worst case under a never-FORGETting kernel; a safety rail, "never a tuning knob" (`R:117-125`) |
| `READDIR_VIRTUAL_{CONFIG,STATS}_COOKIE` | `(1<<62)+3/+4` | above the real cookie space `[3, 2⁶²+2]`, sign-bit clear for the i64 FUSE surface (`R:166-177`) |
| entry/attr TTL of the virtuals | 0 | every path walk revalidates ⇒ fresh generation per consumer, prompt FORGET (`R:27143-27146, 27157`) |
| `CLIENT_HEARTBEAT_INTERVAL_SECS` / `CLIENT_STALE_TTL_SECS` | 10 s / 45 s | 4.5 heartbeats of slack; stale never blocks `format` (`R:186-195`) |
| histogram buckets | 26 log2 µs + `count`/`sum_ns` | exact means; containment laws checkable to the ns (`R:5007-5012`) |
| tripwire log rate | 1 line / site / s | "a violation that fires per-op cannot itself become the outage" (`R:lib.rs:581-583`; port `P:counters.zig:207-213`) |
| bench `AUTO_THREADS_CAP` 16, `AUTO_TOTAL_FLOOR` 16 GiB, `AUTO_PER_THREAD` 2 GiB, `AUTO_MIN_TOTAL` 4 GiB, round 1 MiB, `SUITE_SEQ_BLOCK` 1 MiB, `SUITE_RAND_BLOCK` 4 KiB, rand box 30 s | | the one shape vocabulary; 1 MiB rounding so `-s % -b == 0` for both suite blocks (`R:bench.rs:35-54`) |
| bench seed tag | `0x5351_4653_4245_4E43` | reproducible across runs; xorshift so compression cannot fake rows (`R:376-412`) |
| `OPS_PER_CORE_PER_S` 40 000, `STAGES_PER_OP` 32, `DRAIN_INTERVAL_S` 1, `SAMPLE_BYTES` 24, `RING_MIN_CAPACITY` 1024 | | op-trace derivation (`R:op_trace.rs:52-73`; already in `fuse_handlers_lanes.md` §7.1) |
| `tune` targets | dirty 40/10, rmem/wmem 64 MiB, max_background 256 / congestion 192 raise-only, read_ahead_kb 0 | `R:main.rs:8779-8851` (the port writes the WHY on each line, §4.2) |
| `std::env::var` per op | ≈ 0.7 % of daemon cycles | why every knob is read once at mount (`R:fuse_client.rs:198-201`; port `P:op_trace.zig:182-185`) |

## 6. Tests (reference name → one sentence; the port's red-first contract)

`.stats` / virtual inodes (`R:tests/metrics_tests.rs`, `val7_access_control_tests.rs`,
`phantom_backend0_tests.rs`, `op_trace_tests.rs`, `cli_version_tests.rs`):
- `stats_snapshot_getattr_size_matches_served_bytes_under_churn` — LOOKUP → OPEN → churn →
  GETATTR → READ yields a size equal to the served bytes and a parseable JSON.
- `stats_snapshot_protocol_holds_across_handler_clones` — the protocol holds when each op
  lands on a different queue/clone.
- `stats_lookup_mints_fresh_generation_inos_wb_cache_face` — two lookups mint two distinct
  generation inos, each frozen; real inos never collide with the range.
- `stats_and_config_inodes_are_owner_only` — mode is `0o400`.
- `stats_key_census_is_opt_in` — off by default, `=1` arms, read live not memoised.
- `test_stats_payload_exact_size_no_tail_padding`, `test_config_payload_exact_size_no_tail_
  padding` — payload = pretty JSON + one `\n`, size exact.
- `test_config_lists_real_backing_devices_ids_and_uris`, `test_multi_volume_config_table_
  lists_exactly_named_volumes` — `.config` names exactly the registered volumes with real
  device paths and canonical URIs.
- `stats_surface_exports_build_commit_and_build_tag` — the build identity is in the payload.
- `transport_commit_batch_stats_surface`, `transport_wake_stats_surface`, `fabric_family_
  stats_surface_exports_zero_valued_without_fabric`, `fuse_zc_ledger_always_exports_under_
  metrics` — families export zero-valued when their mechanism is inert.
- `trace_inode_is_owner_only_and_stats_carry_the_gauges` — `.trace` is a Trace-class
  generation ino, 0400, size = payload; `.stats` carries `op_trace_*`.
- `trace_json_shape_is_pinned_and_drains` — the seven keys; a second drain is empty.
- `fuse_ops_single_increment_is_immediately_visible`, `fuse_ops_exact_across_live_parked_
  threads` — the sharded fold is exact for happened-before increments.

CLI (`R:tests/cli_clients_df_tests.rs`, `cli_clone_tests.rs`, `cli_version_tests.rs`,
`cli_help_hygiene_tests.rs`, `mount_writer_guard_tests.rs`, `refcount_clone_tests.rs`):
- `test_clients_classifies_forged_live_and_stale_registrations`; `test_clients_lists_live_
  mount_then_zero_after_clean_unmount`; `test_clients_kill9_daemon_classified_stale_not_
  active`.
- `test_df_offline_empty_volume_reconciles_capacity_and_schema` (keys pinned; `heap_used +
  heap_free == heap`; `allocated_bytes == 0`); `test_df_reflects_allocator_delta_after_
  durable_write` (64 MiB written ⇒ used rises by the allocated blocks); `test_df_answers_
  against_live_mounted_volume` (durable snapshot beside a live mount); `test_df_unformatted_
  volume_fails_loud` (nonzero, names "not formatted").
- `test_cli_clone_verb_produces_a_real_cow_clone` (new ino, identical map, refcounts 1→2,
  bytes equal); `test_cli_clone_without_meta_uri_refuses_loudly`; `test_cli_clone_missing_
  source_refuses_loudly`; `test_cli_clone_staged_source_refuses_loudly`; `test_clone_fails_
  loud_when_blocks_unpinnable`; `test_clone_retries_via_authoritative_map`; `test_clone_of_
  large_inline_v3_file_pins_every_block`.
- `test_claim_clear_refuses_fresh_claim`; `test_stale_cross_host_claim_on_non_pr_volume_
  refused_names_claim_clear` (the mount refusal names the verb).
- `version_flag_prints_one_train_first_line`, `version_line_names_the_build_profile`,
  `version_line_declares_a_profiling_build_and_only_then`.
- `test_help_walker_sees_top_level_verbs` (the 18-verb census), `test_commands_listings_are_
  one_line_summaries` (≤ 72 chars), `test_help_tree_contains_no_internal_design_anchors`,
  `…no_editorial_voice`, `…no_all_caps_emphasis` — the help register.

bench (`R:tests/bench_tests.rs`): `test_parse_size_accepts_human_units` / `_rejects_garbage`;
`test_bare_invocation_selects_the_saturation_suite`; `test_phase_selection_fixed_order`;
`test_auto_threads_clamp`; `test_auto_total_floor_and_per_thread_scaling`; `test_auto_total_
caps_at_quarter_of_free_space`; `test_auto_total_too_small_filesystem_is_loud`; `test_auto_
file_size_rounds_down_to_1mib`; `test_resolve_shape_explicit_flags_always_override`; `test_
auto_sizing_counts_existing_dataset_as_free`; `test_resolve_time_box`; `test_suite_pass_
sequence_and_shapes`; `test_suite_time_flag_overrides_rand_boxes_only`; `test_phase_passes_
inherit_rand_time_box_default`; `test_time_box_expiry_yields_partial_coverage`; `test_suite_
lifecycle_on_tiny_dataset`; `test_suite_refuses_explicit_block_and_rand`; `test_shape_
rejects_zero_*`; `test_direct_requires_block_multiple_of_4096`; `test_direct_requires_size_
multiple_of_block`; `test_dataset_layout_paths`; `test_validate_dataset_{missing_is_loud_and_
creates_nothing, wrong_size_is_loud, extra_files_is_loud, ok_after_write}`; `test_block_order_
{sequential_is_identity, rand_is_full_coverage_permutation}`; `test_block_count_partial_tail`;
`test_fill_block_deterministic_and_nonzero`.

**PORT tests** (portable unless noted): the generation ring (mint/evict/ESTALE/class
decode); the JSON tree parses (a tiny validating scanner in the test) and contains every
`counters.*` declaration (the inverse rail, static); `invariant_tripwire_sites` carries a
fired site; the key census keys are present-empty off and populated on; the op_profile
families are absent off / present on and the disarmed bytes are identical across a toggle
of unrelated state; `.trace` drains once; the xattr allowlist (`user.x` yes, `user.
squeezefs.x` no, `security.capability` yes, `trusted.x` yes, `layout`/`system.symlink`/
`job:x`/`client:x` no); `clients` classification over forged records (live/stale/dead);
`df` JSON schema and `heap_used + heap_free == heap`; clone: new ino, identical map, every
ref `shared`, refcount 2, inline source copies bytes, staged source refuses; `bench`: every
pure function above (parse_size, shapes, auto sizing, time box, suite composition, block
order, fill determinism, the histogram p99 law). Smoke (Linux): `cat .stats` on the live
mount parses, carries `volume_failed: false` and the must-stay-0 census (replacing the
SIGUSR1 tail read, `P:src/cli/smoke.zig:1306-1350`); `stat .stats` is 0400/mount-uid; `ls`
does not list it; `clients` shows the live mount then none; `df` before/after a 64 MiB write.

## 7. QUIRKS not carried (decision 0002 rows to add) and what the port does instead

| # | Reference quirk | Why poor | Port |
|---|---|---|---|
| Q1 | The `.stats` payload is a `serde_json::Value` TREE (`json!{…}` + `metrics.insert` + `to_string_pretty`, `R:fuse_client.rs:11229, 12928-12930, 14802`) — thousands of allocations per build for a ≈ 72 KiB payload, then `String → Vec<u8> → Arc` | A per-read allocation storm on a diagnostic the rigs poll at 1 Hz and the supervisor every 5 s; the family order is a macro's accident | Stream through ONE `std.Io.Writer` into one allocation of the final length (`Io.Writer.Allocating` sized from the last build's length); family writers are `comptime`-generated from the `stats` namespaces (§1.4) |
| Q2 | `invariant_tripwires` exported as ONE total; the per-site census in-process only (`R:lib.rs:548-562`) | Charter §10 requires the per-site label on the surface; an operator reading a nonzero total cannot name the site without the log | `invariant_tripwire_sites{site: n}` via `forEachTripwireSite` |
| Q3 | Three coexisting serve mechanisms — canonical fixed inos with `latest_*_json/size` publish cells, per-lookup generation inos, and the `open_virtual_files` fh pin (`R:27487-27546, 14886-14933, 9403-9431`) | Each is a patch over the previous torn-JSON fix; the fixed-ino arm still has "last-open-wins" | ONE mechanism: generation inos; the generation is the pin (RELEASE/FORGET/eviction are three references) |
| Q4 | `metrics` is a flat bag of ≈ 1,200 keys with format-scoped post-macro insertion (`has_meta`, `R:12923-12961`) and mode-only exports (`membership_mode: "off"` alone, `alloc_reachable_blocks` absent unless engaged, `R:12884-12913`) | A consumer must know which keys exist under which posture; flat names collide across planes | Named families, every family always present and zero-valued when inert; op_profile the one armed-only exception the knob doc already states |
| Q5 | Derived values in the export: `cache_hit_ratio` f64 (`R:11297`), `read_tier_admission_mode: format!("{:?}")` (`R:11366`) | A ratio is the consumer's arithmetic; `Debug` formatting as a wire string | counts only; enums by `@tagName` |
| Q6 | `detached_task_panics` (`R:11596`) | Zig has no unwinding (0002 row 9) | not carried |
| Q7 | String-typed verb plumbing: twelve `removed_verb` stubs that parse and then refuse (`R:main.rs:7181-7189, 7254-7294, 7361-7384, 7456-7498`), hidden RETIRED flags/verbs (`--meta-slots` `R:186-199`, `enable-multi-writer` `R:1384-1396`, `target install` `R:1177-1189`, `spdk` `R:992-1001`), `-g/--meta-uri/meta_uri` triplets, `--data-lv` with four aliases (`R:71-79`), `config` sub-aliases (`R:1547-1567`), admin verbs re-parsed from `arg.split_whitespace()` (`R:ipc_service.rs:1380-1388`) | Nothing predates the port, so no retired spelling needs a refusal; aliases are two names for one thing; a string arg re-parsed server-side is a second grammar | One typed `Verb`; one spelling per flag; the admin lane (rung 7) carries a typed wire struct |
| Q8 | `status` with no URI reads `.config`'s `format.disk_cache_paths` and treats its first member as the META volume path (`R:main.rs:4616-4635`) | A cache directory is not a metadata device; the verb then fails inside the probe with a misleading error | A mountpoint argument resolves through `.config`'s `sqmeta_uri` (the helper the reference already has, `R:1983-2000`) |
| Q9 | `-g URI` with `env = "SQUEEZEFS_META_URI"` as a clap default on `umount`/`clone`/`config`/`df` (`R:671-679, 786-794, 807-814, 825-832`); `df` takes the URI both as `-g` and positionally (`R:823-839`) | An environment knob that is a CLI default; two spellings of one positional | positional `URI|mountpoint` everywhere; knob not carried |
| Q10 | `df` rebuilds every allocator by the FULL inode-tree walk (`recover_active_blocks_v3`, `R:main.rs:8564-8577`) | O(files) I/O for a space report; the reference's `status` already read the config without it | the refs-kind range census per volume (`P:src/cli/status.zig:111-158`) + the durable bitmap population |
| Q11 | `tune` writes sysctls with `let _ =` (errors dropped), always exits 0, constants bare (`R:main.rs:8782-8783, 8799-8800, 8828-8829, 8849`) | A silent failure on an operator verb; §1.5 constants with no reason | check/`--apply` with per-write outcomes, WHY on each target, exit 1 on any failure |
| Q12 | bench sniffs `.stats` by substring (`contains("\"direct_device_true\": true")`, `R:bench.rs:1363-1375`) and `umount` by `serde_json` parse of the whole payload (`R:main.rs:7531-7534`) | string-matching a JSON surface; the smoke does the same (`numberAfter`) | ONE `core/json_scan.zig` (a non-allocating key-path reader over the `.stats` bytes) used by `umount`, `bench`, the smoke and the supervisor |
| Q13 | bench keeps EVERY per-op latency in a `Vec<u64>` and sorts for p99 (`R:bench.rs:501, 519, 626-632`) | a 60 s rand-4k row at 500 k IOPS is 30 M × 8 B = 240 MB of client RAM and an O(n log n) sort per pass | `counters.LatencyHistogram` per worker (26 buckets + exact count/sum), quantiles as bucket upper bounds |
| Q14 | bench colours cells by fixed thresholds (> 100 / 50 MiB/s, > 200 / 100 IOPS, `R:1399-1428`) and prints a seven-counter delta without amplification (`R:1479-1485`) | arbitrary thresholds pretend to judge; the charter's write-row terms are absent | no colour; the measurement-law fields (§3.4) |
| Q15 | virtual READ copies the slice (`slice.to_vec()`, `R:33810-33815`) | a copy per 128 KiB read of an immutable buffer | reply from the generation's bytes |
| Q16 | registry eviction is a linear `min_by_key(seq)` scan at every mint past the cap (`R:14845-14854`) | O(cap) per lookup at the cap | a FIFO ring: mint order IS eviction order |
| Q17 | `.trace` drains at LOOKUP (`R:27163-27176`) — a `stat .trace` consumes the ring | a side-effecting lookup | KEPT (the size must be known at lookup and the generation must be immutable); documented: "`stat .trace` drains; use `cat`" — a `.trace` lookup is never issued by the kernel on its own because the entry is lookup-only and TTL 0 |
| Q18 | `status` JSON uses PascalCase keys (`Setting`, `BlockSize`, `StorageBackends`, `R:32511-32534`) beside snake_case everywhere else | two casings on one surface | the port's text report; every JSON surface snake_case |
| Q19 | the supervisor probes `<mountpoint>/.stats` by `stat` (`R:main.rs:516`) while the port's probes the root (`P:src/cli/supervisor.zig:13`) | the charter says "a control inode"; a root `stat` is served from the kernel's dentry cache under a nonzero TTL and may not reach the daemon | the port's supervisor stats `.stats` (TTL 0 ⇒ every probe is a LOOKUP the daemon answers) |
| Q20 | the heartbeat/claim records are user-visible-namespace xattrs on ino 1 screened by the FUSE allowlist (`R:backend.rs:746-751`) | the screen is the only thing between a tenant and the guard (the reference's own VAL-2 finding, `R:706-722`) | control-tree records (rung-4's `set_config` pattern); the allowlist still screens the two layout/symlink names |
