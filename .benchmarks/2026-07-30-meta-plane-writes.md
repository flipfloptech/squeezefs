# 2026-07-30 — Meta-plane write ceiling: parent-sticky file placement + per-block publish economy

Branch `perf/meta-plane-writes` (off dev `f16d5a9`, **unmerged — do not
merge/push without orchestrator review**). Commits: red instrument+contract
`a3b2190` (`tests/meta_plane_distribution_tests.rs` + per-volume journal
counters), fix `cc4f46a` (regular-file mint striping), red audit `36f5278`
(`tests/meta_write_economy_audit_tests.rs`), economy fix `15ee403`
(freeze-time shadow-fold), audit-print split `3a8b1ce`, fmt `(chore)`.

**Dev-box numbers in this note are SCOPING-ONLY** (isolated worktree,
thermal-governed 2.0–2.2 GHz box, debug-profile sandboxes). The evidence
class is **counter/distribution facts** plus the user's-cluster captures
(journaled on the client at `/scratch/tmp/agent_runs.log`; raw iostat
tapes under `/scratch/tmp/agent_mpw/`).

## 1. The field conviction (user's 4-node cluster, binary f16d5a9)

Writes are capped by the METADATA plane, not data:

| Phase | User B/W | Blocks/s | Meta device-writes/s (nvme0) | Meta writes/block |
|---|---|---|---|---|
| Fresh 4 MiB streaming | 10.4 GB/s | ~2,600 | ~21,000 (4 KiB each) | **~8** |
| Steady-state rewrite | 6.6 GB/s | ~1,650 | ~24,000 | **~14.5** (destroy+republish) |

Both phases sit at the same ~21–25 k meta-writes/s ceiling on **one**
device. Data heads at rewrite: aqu ~3, util ~92 % — capacity to spare.
Write pipeline: 39/40 admitted blocks parked awaiting commits (depth
governor probed and backed off — exonerated). **The smoking gun: `nvme2`
— the SECOND meta volume (mds1) of the `--meta-slots 8` pair
(`sqmeta:///dev/nvme0n1,/dev/nvme2n1`) — recorded 0.00 writes through
every capture for a week**; `meta_kv_free_extents` [32631, 32636].

Live-mount facts gathered this session (stats inode, non-destructive):
the benchmark tree is **32 regular files (`k1..k32`, 3 GiB each) directly
in the root directory**; `meta_kv_journal_full_stalls` = **[10, 0]**
(volume 0 stalls, volume 1 idle); `meta_kv_journal_bytes` 2.64 GB over
`meta_kv_journal_entries` 145,113 = **18.2 KiB mean journal entry**;
`meta_kv_commit_sites`: `backend.rs:5292` (= `set_layout_and_size`, the
block-publish commit) **123,722 of 145 k commits**; conveyor group-size
median 1 (38,654 of 57,398 passes were singletons); node writeback:
appends 11,500 / **compactions 18,814** (1.6:1 INVERTED against the
"> ~1:8 is mistuned" design note), append bytes 353 MB / **rewrite bytes
3.79 GB**; both meta volumes report health 999 (the placement band
cannot explain the exclusion).

## 2. Prong 1 — why volume 1 carries ZERO journal traffic

### Root cause

**Regular-file inode placement was parent-sticky.**
`RoutedMetaBackend::create_with_rdev` placed every non-directory inode on
its parent directory's volume (`target_v_idx = parent_v_idx`); only
directories striped (health-banded 90 %-band round-robin). The root
directory is pinned to slot 0 → volume 0 (`route_ino_width`: ino 1 → slot
0). Every data-plane meta commit — block publish (`set_layout_and_size`),
size flip, extent-record spill, destroy — routes by the **file's** ino
(`route_ino`), so the field's 32 root-directory files sent 100 % of
journal + conveyor + checkpoint traffic through volume 0's single
journal/pass-task pipeline while volume 1 idled (its only writes: D0
`writer_claim` heartbeats — the 0.00 in iostat rounding).

### Hypothesis adjudication

| Hypothesis | Verdict | Evidence |
|---|---|---|
| (a) slot→volume map collapses (all 8 slots → volume 0) | **RULED OUT** | `plan_meta_slot_set` writes the identity distribution `slot k → k mod N` ([0,1,0,1,0,1,0,1] for the field shape); `validate_slot_map` refuses any volume hosting no slot; `discover_meta_set` reconstructs per-slot highest-epoch-wins. Field behavioral probe (§2.3) shows volume 1 commits fine when an ino routes there. |
| (b) monotonic ino × routing lands every ino in volume-0 slots | **CONFIRMED (as placement, not arithmetic)** | Mints encode the target volume's mint slot (`make_global_ino_width(raw, mint, W)`) — the arithmetic is fine; it's the *choice of target volume* that was parent-sticky for files. Cargo repro: 32 root-dir creates → placement **[32, 0]**, journal entries **[288, 0]**. |
| (c) journal/commit routing ignores the ino's slot volume | **RULED OUT** | Per-volume `KvMetaBackend` owns its own journal/conveyor/checkpoint; every mutation routes `route_ino(ino)` first (`set_layout_and_size`, `destroy_inodes`, dentry ops). Field probe: dirs that landed on volume 1 drove nvme2 at 14.6–35 k w/s. |
| (d) format-time slot map recorded wrong | **RULED OUT** | Stamps carry `slots_hosted = {k : k mod N == pos}` at epoch 1; the live mount's `.config` shows both volumes enabled, health 999/999; `test_stamp_survives_mount_write_checkpoint_remount` pins stamp persistence. |

### The distribution contract (cargo repro, red → green)

`tests/meta_plane_distribution_tests.rs`, 2-volume file-backed stamped set,
`--meta-slots 8` (the exact field shape), driven through the routed
`Metadata` trait:

- **Instrument** (new): per-volume `JournalRing::written_{entries,bytes}`
  mirrors of the process-global `META_KV_JOURNAL_{ENTRIES,BYTES}` (which
  cannot attribute traffic to a volume — the blindness that hid this),
  surfaced on the stats inode as
  `meta_kv_journal_{entries,bytes}_per_volume` (arrays parallel to
  `meta_format_version`).
- **Contract**: 32 files created in ONE directory (root), 8 block-publish
  commits each (the exact `set_layout_and_size` shape the write path
  issues per published block). Tolerance stated: each volume of the
  healthy symmetric pair carries **≥ 30 %** of the total journal-entry
  delta (the parent volume legitimately carries every dentry-side entry
  on top of its mint/publish share, so 50/50 is not the contract; the
  bug's shape is ~1/99), and each volume hosts **≥ 25 %** of minted file
  inos.
- **Red at `a3b2190`**: placement `[32, 0]`, journal entries `[288, 0]`.
- **Green at `cc4f46a`**: both contracts pass; the pre-existing directory
  striping is pinned separately (`test_dir_placement_stripes_across_volumes_pin`).

### The fix

`cc4f46a` — one `pick_mint_volume()` helper (the directory branch's
health-banded 90 %-band round-robin, verbatim) now places **directories
AND regular files**. Cross-volume creates ride the existing mint+dentry
two-commit machinery (dirs have exercised it since VL5a). Economy: a
cross-volume regular create costs two whole-tx entries (mint on target +
dentry/parent-times on parent) instead of one — but they land on
DIFFERENT volumes, so per-volume entries/op stays ≈ 1 and the per-volume
journal ceiling is unchanged for create storms while the data plane gains
the full set's journal bandwidth. Single-volume sets short-circuit (no
health probe — the pre-VL5a create shape). `meta_entry_economy_tests`
(single-volume G4 pins) green.

### Field behavioral confirmation (pre-fix binary f16d5a9, live cluster)

Bounded create+4 KiB-write storms against the EXISTING mount (journaled;
tree created under `/scratch/tmp/test/agent_mpw`, removed after; iostat
1 s over both meta namespaces, first report excluded):

| Phase (30 s) | creates/s | nvme0n1 (meta vol 0) | nvme2n1 (meta vol 1) |
|---|---|---|---|
| 8 fresh dirs, files striped across them | 2,800 | **14,606 w/s** / 58.4 MB/s | **14,658 w/s** / 58.6 MB/s |
| ONE fresh dir, all files in it | 2,741 | 0.4 w/s | **24,757 w/s** / 99.0 MB/s |

The single-dir phase concentrated the whole meta plane on ONE volume
(volume 1 this run — the RR placed that dir there), reproducing the
week-long conviction with the roles swapped — which simultaneously proves
volume 1's journal/conveyor pipeline is fully functional (killing (a),
(c), (d) behaviorally) and that placement is the whole story. Note the
single-volume phase runs at ~24.8 k w/s — the same ceiling band the
field's streaming workload saturates.

## 3. Prong 2 — per-block meta device-write economy

### Audit (sandbox, exact; `audit_block_publish_meta_economy`)

One file streamed to 256 blocks through the exact per-block-publish
commit (`set_layout_and_size` with the re-serialized whole layout), then
a 256-publish full-map rewrite pass. Journal counters are the new
per-volume ring mirrors; single-volume sandbox:

```
window        entries  bytes      bytes/publish  layout_len@end
(  0,  64]        64      79543           1243            2138
( 64, 128]        64     214432           3350            4250
(128, 192]        65     349678           5464            6362
(192, 256]        66     485077           7579            8491
rewrite x256      260    2209848           8632            8491
node writeback: 198 append frames / 1748992 B, 7 compactions / 102400 B
rewrite, 512 shadow-dropped records   (post-15ee403 run)
```

**Where the field's ~8 writes/fresh block and ~14.5/rewrite go:**

| Component | Per-publish cost | Field arithmetic |
|---|---|---|
| Journal entry (ONE per publish — count economy is healthy) | **O(block_map)** bytes: `merge_block_mappings` → `save_metadata_to_backend` re-serializes the ENTIRE layout value per publish; entry ≈ 44 B × block-index + ~200 B | 3 GiB files = 768 blocks ⇒ mean entry ≈ 18 KiB (measured 18.2 KiB field-wide) ≈ 4.5 journal pages/publish |
| Node writeback (bset append of the SAME whole layout value, 4 KiB-padded) | O(block_map) again per surviving overlay version | appends + the compactions they force: 353 MB + **3.79 GB whole-node rewrites** (256 KiB logs fill in ~7 frames at rewrite-phase sizes ⇒ compaction:append 1.6:1 inverted) |
| Checkpoint (ledger A/B + bitmap pages) | amortized, small | 1,731 checkpoints / 145 k entries |
| Rewrite adder | full-map entries (8.6 KiB vs the fresh ramp) + displaced-key frees (refcount/reclaim rides data-plane BLKDISCARD, not meta) | 14.5 vs 8 writes/block |

Total for a fresh mid-file block: ~4 journal pages + ~4 node-writeback
pages ≈ **8 device writes** — the field number, reproduced from counters.
The **journal-bytes ceiling** interpretation: 24 k × 4 KiB ≈ 96 MB/s of
4 KiB meta writes; the plane is byte/IOPS-bound on ONE volume's
journal+writeback stream.

### Landed (red-first): freeze-time shadow-fold (`15ee403`)

`freeze_locked` froze the ENTIRE overlay, so W same-key commits inside
one checkpoint cadence appended W full layout values — every one but the
newest completely shadowed by the fold algebra (§4.2: a newer Put/Delete
terminates the fold). Now the frozen bset carries, per key, only the
newest base-establishing record (Put/Delete) plus Deltas newer than it;
delta-only runs keep everything (their base lives in older bsets).
Red: 16 superseding 3 KiB puts froze 17 records / ≥ 48 KiB appended;
green: 2 records / ≤ 9 KiB. Crash contract untouched (one tx = one
checksummed journal entry unchanged; a frozen bset either fully survives
— fold-identical — or is fully dropped by the §4.5 torn-tail classifier
while the journal window still covers every dropped seq; §4.4 pt 4
compensation detection preserved by the 4a-guard argument — a failed tx's
record is always the newest of its key run). Engagement gauge:
`meta_kv_node_freeze_shadow_dropped` (stats inode). Green across
`kv_fold_slimming_tests`, `kv_tree_tests`, `crash_contract_tests`,
`crash_kill_tests`, `kv_backend_tests`. Expected field effect: node
writeback bytes divide by the per-window supersession factor (the
streaming capture ran ~2,600 publishes/s over 32 hot layout keys ≈ 4
versions/key/50 ms window), which divides the compaction rate that
produced the 3.79 GB rewrite stream.

### FILED (not landed — format/design-level, needs its own program)

1. **The O(file_size)-per-publish layout representation** (the dominant
   term): per-block `block_map` entries re-serialize the whole map into
   every journal entry AND every node writeback. Fix directions, in
   ascending invasiveness: (i) **per-ino publish coalescing** — batch W
   consecutive block publishes into one merge commit (divides journal
   bytes by W; durability posture unchanged — publishes are already
   50 ms-cadence durable, and fsync must force-drain); (ii) **layout
   delta records** — a `Delta`-kind encoding for the layout xattr
   carrying only the changed `(block → key)` entries (O(1) per publish;
   new wire format under the §4.2 delta rules); (iii) **earlier indirect
   spill** — today the map spills to a data-plane blob only past ~60 KiB
   serialized (≈ 6 GiB file at 4 MiB blocks), so 3 GiB files pay inline
   O(k) meta bytes forever; spilling earlier moves the O(k) bytes to the
   data heads ("capacity to spare") at one blob write per publish, but
   the in-place blob rewrite's torn-write window needs its own analysis
   before widening its population.
2. **Conveyor group-size median 1** on the field capture (38,654/57,398
   singleton passes): the streaming shape commits serially per ino, so
   the group-commit machinery cannot batch — publish coalescing (1.i)
   is the same lever seen from the other side.

## 4. Prong 3 — mds-node dual duty (measure, file)

Field probe (journaled, cleaned up): a 4-dir create+4 KiB-write meta
storm (spreads over both meta volumes) alone vs concurrently with 4 × dd
1 MiB O_DIRECT streaming writers (~7.4 GB/s aggregate onto the 4 data
namespaces, incl. the mds-hosted `mds0-d0`/`mds1-d0`):

| Phase | creates/s | meta vols w/s | meta w_await | data heads |
|---|---|---|---|---|
| A: meta storm alone | 2,760 | 14.4 k + 14.4 k | 0.18–0.20 ms | idle |
| B: meta + 7.4 GB/s data | **816 (−70 %)** | 1.8 k + 35.1 k¹ | **0.18 ms (no inflation)** | 458 w/s ≈ 1.85 GB/s each, w_await 1.25–1.50 ms, util ~55 % |
| A2: meta storm alone (repeat) | 2,390 | 5.6 k + 5.6 k | 0.18–0.20 ms | idle |

¹ Phase B's dd files were fresh creates in one directory — pre-fix
parent-sticky placement put ALL their block publishes on volume 1
(nvme2 at 35 k w/s): an accidental second live confirmation of prong 1,
and a bonus observation that the meta plane reached **35 k w/s** on
nvme2 when the initiator pushed harder.

**Filing**: at this load level the mds targets' meta volumes show **no
device-latency inflation** under concurrent data duty (w_await flat at
0.18–0.20 ms; the data namespaces on the same mds nodes carry the same
~1.85 GB/s as the oss heads). The −70 % meta-op throughput under
combined load is **initiator-side** (shared client NIC/CPU + FUSE
pipeline contention), not mds journal latency. Caveat honestly: this is
one client at 7.4 GB/s; the field's 10.4 GB/s multi-client captures may
expose target-side CPU contention this probe cannot — re-measure
post-deploy with the per-volume counters if rewrite-phase w_await on
nvme0/nvme2 diverges from ~0.2 ms.

## 5. Projected field impact (PROJECTION until the field re-measure)

- **Placement striping** (cc4f46a): the 32-file streaming tree's
  data-plane meta traffic splits ~50/50 across both volumes ⇒ per-volume
  meta writes/block halve (fresh ~8 → ~4, rewrite ~14.5 → ~7.3). At the
  observed ~21–25 k w/s per-volume ceiling (and the 35 k w/s nvme2
  observation in §4), the meta-plane-permitted block rate roughly
  doubles: fresh 2,600 → ~5,200 blocks/s ⇒ **~20 GB/s meta-permitted**
  (10.4 measured today), rewrite 1,650 → ~3,300 blocks/s ⇒ **~13 GB/s
  meta-permitted** (6.6 today). Other binders (client NIC, data-head
  util at 92 %) are expected to intervene before those numbers — the
  honest claim is "the meta plane stops being the binding constraint at
  today's rates".
- **Shadow-fold** (15ee403): divides the node-writeback share (~half the
  meta device writes) by the per-window supersession factor (~4 at the
  captured streaming rate) ⇒ a further ~35–40 % cut in per-volume meta
  writes/block on this workload, multiplicative with the striping split.
- Both effects are counter-verifiable post-deploy via
  `meta_kv_journal_{entries,bytes}_per_volume` (balance) and
  `meta_kv_node_freeze_shadow_dropped` (engagement) with the same
  nvme0-vs-nvme2 iostat capture.

## 6. Gates

- Red→green chain: `a3b2190` (red: `[32,0]`/`[288,0]`) → `cc4f46a`
  (green); `36f5278` (red: 17-records freeze) → `15ee403` (green).
- `cargo clippy --all-targets --all-features -- -D warnings` clean;
  `cargo fmt --check` clean.
- `cargo test --all-features -- --test-threads=1` full suite from zero:
  see the campaign report (run on the throttled shared dev box).
- `cargo doc --no-deps`, `cargo bench --benches -- --test`: see report.
- statfs ×10 loaded soak: see report.
- Loom: not re-run — no lock-free core changed (`freeze_locked` runs
  under the node write lock; `journal_core`/`node_state_core`/
  `alloc_ext_core` untouched).

## 7. Orchestrator actions needed

1. **Review + merge** `perf/meta-plane-writes` (5 commits, unmerged, not
   pushed per campaign rules).
2. **Field deploy + remount window** for the 4-node cluster (binary with
   cc4f46a/15ee403 + the per-volume counters) — required before the §5
   projections can be re-measured; NOTE the fix changes placement for
   NEW files only (the existing k1..k32 inodes stay on volume 0; a
   rewrite-phase re-measure needs a fresh file set or tolerance for the
   legacy set's imbalance).
3. Decide the **prong-2 filed program** (§3: publish coalescing / layout
   delta records / earlier indirect spill) — the remaining O(file_size)
   journal term is the next meta-plane multiplier after striping.
