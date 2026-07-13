# Finding A closed — the node-seq mint watermark (2026-07-13)

Branch `fix/kv-finding-a` off dev @ `7ce1800` (the beta-release-gate note's merge).
Root-causes and fixes the gate's **NOT-READY driver**: persistent `corrupt KV encoding`
decode errors on checksum-valid frames of the fstests scratch volume
(`.benchmarks/2026-07-13-beta-release-gate.md` §Finding A). Rails: taskset 0-15 +
JOBS=12, 3.5 GHz cap untouched, Tctl 44–73 °C, unique sandboxes `~/tmp/findingA_*`,
gate-run artifacts reused from `~/tmp/relgate_20260713/`.

## Verdict in one line

**Software, fixed:** node incarnation seqs minted from a per-volume counter that was
never persisted; clean-shutdown remounts collapsed its reseed floor and re-minted the
stamped domain, so recycled extents still holding old appended frames could satisfy the
§4.5 `node_seq_at_write == node_seq` admission on a new equal-seq node — chaining the
previous incarnation's checksummed records into the wrong tree/level. The ledger now
persists a **node-seq watermark**; mounts reseed at-or-above it; the exact gate error is
**reproduced verbatim from pure software state** in a pinned test, closing the
software-vs-hardware cleave on the software side.

## Forensic timeline (gate artifacts)

| Evidence | Finding |
|---|---|
| Scratch daemon log (1,452 error lines) | TWO distinct corruption events, not a stream: `dentry value: 9 trailing byte(s)` first at 11:40:54 (then 1×/harness-cycle), `interior value must be 16 bytes, got 75` first at **12:01:35** (~21 min later, then ~5×/min) — each a persistent damaged node re-served on every scratch mount until the 12:59 generic/515 device-poison erased the volume |
| Journal run markers | Event 1 born during the generic/207–221 window (aio-dio races, ENOSPC-mmap 211, unwritten-extent 213/214); event 2 during generic/311–318 — both on an aged tree (~350 mount cycles) |
| Exclusions (gate + this session) | no daemon kill, no scope overlap (single scratch scope 11:30:29→11:57:18), no raw-device writer before 12:58:58, TEST volume clean 4 h, xxh3 passes ⇒ bytes were written as-checksummed |
| Seq-domain probe (offline walk tool, this branch) | **record (LWW) seqs are journal-domain and persistent** — the fold is safe across remounts (falsified the first hypothesis; three LWW red tests came back green and were replaced); **node seqs are counter-domain**: uuid-based huge values, root seqs floor the mount, non-root mints float above |

## The hole (exact)

- `KvTree::next_seq` mints node incarnation seqs from `Arc<AtomicU64>` shared per
  volume; **never persisted**.
- Mount reseeds it from `max(ledger.seq /*checkpoint ordinal*/, the 3 root node seqs,
  replay-window interior child pointers)` (`backend.rs` open).
- Leaf compactions/splits under a stable root mint successors ABOVE every root seq
  (`smo_replace` appends the parent pointer; the root keeps its seq). A clean shutdown
  empties the replay window ⇒ next mount's floor < those mints ⇒ **the stamped domain is
  re-minted**.
- The freed source extents of those SMOs keep their appended frames (stamped with the
  now-re-mintable seqs); the A/B allocator legitimately reuses the extents after the
  next checkpoint. Equal stamp + reuse ⇒ §4.5 admits the residue: cross-tree/-level
  records under the wrong decoder. A 75-byte dentry (10 + 65-char name — the fstests
  long-name shape) under the interior decoder IS the gate's second signature;
  event 1 is the same admission with a foreign 19-byte value under the dentry decoder.
- fstests scratch is the perfect incubator: hundreds of clean remounts, repetitive mint
  patterns (similar arithmetic sequences every session), tombstone-desert compactions,
  tiny volume (high extent-reuse pressure). Matches: n=2 events in 80 min, scratch-only,
  never on fresh volumes, never in QUICK-era logs.

## The fix (three legs, forward-only, no shims)

1. **`LedgerRecord.node_seq_watermark`** (checkpoint.rs): the mint counter captured at
   record build (after the flush loop's own SMO mints); mount reseeds
   `max(ledger.seq, watermark)` with the existing root/replay `fetch_max` floors kept as
   crash-window belt-and-braces (SMO pointer records journal `child_seq`; extents freed
   in a crash window are not reusable before the next checkpoint — the chain argument is
   on the field's doc). Builder stamps the bootstrap ledger with its final mint value.
2. **Superblock incompat bit 1 `NODE_SEQ_WATERMARK`**: pre-watermark v3 volumes refuse
   loud — "reformat required" (the v2-removal precedent; their ledger slots no longer
   decode anyway: the fixed payload prefix widened 8 B).
3. **Debug-tier write-side audit** at the typed boundary (`commit_tx` /
   `commit_compensation`): every staged record must decode under its own tree's typed
   decoder before any byte persists — a malformed encode (or in-RAM corruption of staged
   records) now fails on the WRITER with a backtrace instead of surfacing as a
   reader-side decode error on a checksum-valid frame. **Cost, honestly:** debug builds
   only (`cfg(debug_assertions)` — the cargo gate, QUICK and soak tiers all run it);
   release builds pay zero. A release-sampled variant was considered and not taken: the
   software hole is closed by leg 1, and the tree/node layers are contents-agnostic by
   contract (their tests forge synthetic payloads), so the audit's correct home is the
   backend staging boundary.

**Evaluated and REJECTED — loud higher-stamp tripwire** in the frame walk ("a residue
stamp above the live node seq is impossible under monotonic mints ⇒ fail loud"): the
existing `v3_quick_reformat_buries_previous_generation_records` pin caught it failing
legitimate mounts — dead-generation residue after a quick reformat carries stamps from a
foreign uuid-derived base that is HIGHER on a coin flip and must stay silently buried.
A sound tripwire needs a generation tag in the frame header (the reserved u16 is the
candidate slot); noted on `FrameProbe::StaleIncarnation`, not taken.

## Pins (tests/kv_finding_a_tests.rs — red-first, final text re-verified red on the pre-fix tree)

| Test | Red on pre-fix | Green on fix |
|---|---|---|
| `node_seq_mints_stay_above_every_persisted_stamp_across_remount` — 4 remount cycles, offline live-walk vs the running stamp ceiling + the mounted ledger watermark covers it | 4/5 rolls (live-only sampling escapes when a late root SMO re-floors near the ceiling — noted in-test) | 3/3 + every later gate roll (unconditional by design) |
| `equal_stamp_recycled_extent_reproduces_the_gate_signature` — forged re-mint state ⇒ frame admitted ⇒ fold ⇒ **verbatim** `corrupt KV encoding: interior value must be 16 bytes, got 75` | (mechanism demo — documents WHY; bypasses mints) | green |
| `future_stamped_residue_frame_is_buried_never_admitted` — higher-stamp residue: not admitted, load not failed | red pre-fix in its tripwire form; re-aimed at the burial contract after the reformat false-positive | green |
| §6.1 gate (`kv_backend_tests`): pre-watermark superblock refuses naming watermark + reformat | n/a (new bit) | green |
| `crash_contract_tests` stale-incarnation forge re-aimed at the higher-stamp direction with the reasoning on record | — | green |

## Acceptance (fix tip `fab8c04` + pins)

| Gate | Result |
|---|---|
| Finding-A suite ×3 | green 3/3 (and ×N across every later roll) |
| KV family serial (`kv_node/tree/scale/backend/journal/alloc` + `crash_contract`) | all green (kv_scale 15 passed/2 ignored-by-default, backend 32, node 12, tree 17… full rolls in the log) |
| fstests generic/**340 344 345 346 354** (the Finding-A faces) | **Passed all 5**, zero `corrupt KV encoding` in either daemon log |
| QUICK ×5 (`MEMMAX=8G`) | rolls 1,2,4,5 = **{003, 213} only**; roll 3 added the documented 074 fstest.2 transient family — **A/B-cleared**: standalone 074 3/3 green on this tip AND 3/3 green on pre-branch dev@7ce1800 same session, and the same 074 failure fired twice today on the parallel session's unrelated branch (box-lineage flake, provenance table rates) |
| kill9 deep churn (`SQUEEZEFS_CRASH_ROUNDS=60`, batch shape) | green rolls incl. 60-round standalone; the documented `test_kill9_remount_soak_v3` shutdown-drain flake (lineage `91e9883`) reproduced at **2/13 on this tip vs 1/8 on pre-branch dev** — pre-existing at parity, same empty-replay-window assert, not aggravated |
| unmount kill soak (root, 30 cycles) | **PASS** — 0 coredumps, 0 SIGABRT, 0 panics |
| clippy `-D warnings` / fmt / doc | clean / clean / 0 warnings |
| full serial suite | **693 passed / 0 failed** (74 bins) |
| bench smoke | green |
| loom | not re-run — no commit-path atomic changes (a plain ledger field, one `load` snapshot, reseed of the same counter; no new cross-word invariants per the standing §5.7 scope note) |

## Release-gate consequence

`.benchmarks/2026-07-13-beta-release-gate.md`'s verdict line is amended by this note:
Finding A is **software-root-caused and fixed** (not hardware; the box's crash history
and the `CPU8` kernel line are no longer needed to explain anything — the mechanism
reproduces bit-exactly in a test). The gate's remaining open classes are unchanged
(documented platform set, KV-flood cage-kill class, drain-timeout class, aio-dio /
DIO-hole-pread pre-existing families, generic/515 harness exposure). Verdict moves to
**READY-WITH-DOCUMENTED-CLASSES** contingent on the standing nightly obligations; the
recommended re-gate is one full `-g auto` on a tip that includes this fix (scratch
integrity now holds by invariant — note the 515 poison still costs ~90 tests of
inventory until the harness guard is added).

**Operator note:** v3 volumes formatted before this fix refuse to mount loud
("reformat required") — reformat with `squeezefs format --force`. This is the
forward-only policy working as designed.
