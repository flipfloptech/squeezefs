# DUR-3..8 — metadata-durability integrity (pre-RC spec §1)

**Branch:** `fix/dur-metadata-integrity` (off `dev` @ `7d1ec2e1`)
**Scope:** pre-RC engineering spec §1 items DUR-3, DUR-4, DUR-5, DUR-6 (P0)
and DUR-8a–f (P1); DUR-7 scope-checked per execution-plan ruling D4.
**Prerequisite:** TEST-1's data-device power-cut harness (`src/dev_power_cut.rs`)
plus the two `uring_fs` stalls this branch adds — the metadata-plane items are
not testable without an interleaving that can be BUILT rather than raced.

Every item is red-first. Redness was verified for each row by running the
committed tests against the unmodified `src/` (`git stash push -- src/`), not
by inspection.

---

## 1. Landed items

| Item | Red evidence (before) | Green (after) | Test |
|---|---|---|---|
| DUR-3 | `reusable_upto` 2025 → 3881 released by a barrier submitted before the ledger record existed | unchanged by the stale barrier; the next covering barrier releases it | `dur3_a_barrier_in_flight_before_the_push_never_drains_it`, `dur3_deferred_reclamation_releases_on_the_next_covering_barrier` |
| DUR-4 | a failed bitmap write's retry wrote `[]`; a same-generation retry tripped the `debug_assert` (release: silent A/B tie) | the retry rewrites page 0 and the claim reads set on disk; the two slots never tie | `dur4_a_failed_bitmap_write_keeps_its_dirty_bits`, `dur4_bitmap_generations_never_tie_on_disk` |
| DUR-5 | a torn sector 0 ⇒ `"unsupported metadata format version 2880154539"` (volume permanently unmountable); four concurrent bit setters lost the layout-deltas bit (`features_incompat=0x5f`) | the tail copy classifies and mounts, sector 0 self-heals at the next write mount; all four bits survive | `dur5_a_torn_primary_superblock_still_classifies`, `dur5_concurrent_incompat_bit_setters_never_lose_a_bit` |
| DUR-6 | a sector-boundary tear between two same-shape images decoded CLEANLY with 378/400 entries naming the predecessor's blocks; the blob was rewritten in place (same offset twice); the layout named an un-barriered blob | torn image refused (typed `IndirectMapFormat`, EIO); CoW with the predecessor freed after the commit; `covering_epoch` proves the barrier precedes the naming commit | `dur6_a_torn_indirect_blob_refuses_instead_of_decoding_wrong_keys`, `dur6_indirect_publish_is_cow_and_frees_the_predecessor`, `dur6_the_blob_is_barriered_before_the_layout_names_it`, `dur6_legacy_v1_blobs_still_rehydrate` |
| DUR-8a | a flipped `block_idx`/`fencing_token`/`flags`/`count` passed verification | every header field is inside the digest | `dur8a_extent_record_checksum_covers_the_header` |
| DUR-8b | the durable delta chain reached 5 with the cap at 4 (and 255/255 in the economy suite) | the backend re-bases at the cap in both commit paths | `dur8b_the_chain_cap_bounds_the_durable_delta_chain` |
| DUR-8c | `next_ino` came back 2 with the replay window mentioning inos 4 000 and 9 000 | the watermark folds dentry-value and xattr-key inos | `dur8c_replay_watermark_folds_dentry_and_xattr_inos` |
| DUR-8e | a 4 GiB declared plaintext allocated | refused before the allocation; zstd bounded at the reader | `dur8e_declared_plaintext_length_is_bounded_by_the_block_size` |
| DUR-8f | two `?` exits abandoned an allocated, unnamed block | `MintedBlockGuard` frees on every exit before custody transfer | covered by the existing spill suites + the guard's own contract (MEM-2) |

### DUR-5 — layout verdict

**A/B'ing sector 0 into two sectors is a format break; the tail copy is not.**
`SuperblockV3::plan` puts the root ledger at offset 4096 on every v3 volume ever
formatted, so there is no reserved space beside sector 0 to claim, and the
bitmap region's deliberate over-reserve is not guaranteed to spare a page pair
(`bitmap_region_len(volume/node_size)` vs `bitmap_pages_for(actual extents)`
usually round to the same page count). Splitting sector 0 into 512-byte A/B
slots is worse than a break: the checksum covers the WHOLE sector, so an older
binary would read a valid volume as corrupt.

Shipped instead (spec's second option, verbatim): a redundant copy at the last
aligned 4 KiB sector, `plan` reserving it out of the heap (≤ one extent) so
every fresh format has one. Pre-DUR-5 volumes whose heap runs to the end get no
copy — logged once, never scribbled over a live node. The `sb_generation` u64
lives in the sector's zero padding, which the existing whole-sector checksum
already covers, so an older binary verifies and decodes byte-identically and
ignores it: **no new incompat bit, no field moved, no format change.**

Write order: copy first (barriered), then sector 0 (barriered) — a torn sector 0
always recovers forward. The copy is consulted ONLY when the primary cannot
serve, and only when the recovered image itself reserves the slot it was read
from, so no stale or crafted tail sector can displace a readable primary.
`KvMetaBackend::open` repairs sector 0 from the copy under the writer flock
(write mounts only): the "no repair verb" gap closed as an automatic act.

### DUR-6 — sharded vs patched

**Patched (blob v2 = checksum + CoW + barrier), sharded deferred to PERF-9 in the
Phase-8 window.** Rationale, in the order it governed:

1. PERF-9's sharded map is a *performance* deliverable. AGENTS.md makes a
   counted A/B bracket mandatory for perf work; this branch has no venue (the
   cluster holds the reset-v5 mount) and the parallel-agent law forbids taking
   it. Landing the format without its bracket would be a program violation.
2. The execution plan's own §7 one-window rule batches the sharded map's
   incompat bit into the Phase-8 reformat, and this wave was told not to stamp
   one.
3. DUR-6 is P0 and cannot wait for that window. The three defects are closed
   NOW on the existing blob, in the way the header was designed for ("the NEXT
   encoding change is a version bump, not a format break"): the v3 sharded image
   slots in behind the same version byte with the same reader gate.

Compatibility: v1 blobs are still DECODED (a fleet migrates by rewriting — every
publish is CoW now — not by reformatting). A pre-DUR-6 binary meeting a v2 blob
refuses loud on the version gate; never a silent misread. That is the standing
forward-only posture, and the downgrade cost is stated here rather than
discovered in the field.

### DUR-8d — on-disk compatibility verdict: **BREAKS existing encrypted volumes; deferred to the batched format window**

`Aad::empty()` means a ciphertext block validates anywhere, so any stale or
relocated mapping decrypts cleanly instead of failing loud. Binding
`AAD = (ino, block_idx, dev_offset)` is the right fix and is free at runtime —
but it is **not** backward compatible: every block on an existing encrypted
volume was sealed with empty AAD, and a binary that opens with a bound AAD fails
EVERY such open. There is no in-band marker to discriminate: the encrypted
payload header is `wrapped_key_len(2) | nonce_len(1) | wrapped_key | nonce | ct`
with no version or flags field.

Consequences for sequencing:

* the change needs a **per-volume format-config flag or incompat bit** so a
  mount knows which AAD its blocks were sealed with — exactly what this wave was
  told not to stamp;
* it composes naturally with D3's key-wrap replacement (`rsa` → X25519/AES-KW),
  which is already scheduled for the same window with its own bit;
* half-building it (plumbing a context through `process_write`/`process_read`/
  `process_write_pooled` that constructs `Aad::empty()` today) would add a
  wide, dead parameter surface for zero present benefit.

**Recommendation for the window:** add a `payload_version` byte to the encrypted
payload header at the same time (so future AAD changes need no volume-level
flag), bind `AAD = version || ino || block_idx || dev_offset` (LE, fixed width),
and note that `dev_offset` binding makes a relocated-but-unmoved-block class
(defrag/mover republish) an explicit re-seal, not an accident — the mover must
re-encrypt on relocation, which is already what `move_one` does for transformed
volumes.

---

## 2. DUR-7 — scope call: **STOPPED at this design note (ruling D4)**

Per execution-plan ruling D4 cross-volume atomicity ships as a **distributed
transaction, not a refusal**, and the plan schedules the machinery as DLM stage
**S3.5** with DUR-7 as its first consumer. Building intent records +
compensation + crash recovery inside this branch would be a second, larger
program riding on a durability-fix branch, and it would have to interact with
machinery this branch does not own (the M7 conveyor, the §5.5.2a slot-cutover
gate, the writer-claim/fencing ladder, mount replay ordering). **Not built.**
What follows is the handoff.

### 2.1 The crash windows, enumerated

Cross-volume shapes exist only when `route_ino(parent) != route_ino(child)` on a
multi-volume metadata set. Each is TWO independent whole-tx commits, in this
order, with no intent record between them:

| Op | Commit A | Commit B | Crash between A and B |
|---|---|---|---|
| `link` (`mod.rs:~1690`) | child volume: `nlink += 1` + ctime | parent volume: dentry insert + parent times | `nlink = 2` with ONE dentry. `reclaim_orphaned_batch` skips `nlink > 0`, so the inode and every block it names leak permanently — no verb finds them. |
| `unlink` (`:~1591`) | parent volume: dentry removal + parent update | child volume: `nlink -= 1` (dir ⇒ 0) + ctime | `nlink = 1` with ZERO dentries: invisible to every path and unreclaimable. Same permanent leak, plus a `df` that never comes back. |
| `rmdir` (same path, `is_dir`) | parent: dentry removal + parent `nlink` bump | child: `nlink = 0` | parent link count drifts DOWN permanently; the directory inode is stranded. |
| dir `rename` across parents (`:~1982`) | old parent: dentry removal (+`nlink -= 1` when the child is a dir) | new parent: dentry insert (+`nlink += 1`), child ctime | parent `nlink` drifts: `rmdir` on the old parent then either succeeds with children present or refuses forever. The code's own comment says "best-effort". |
| `RENAME_WHITEOUT` cross-volume (`:~2060`) | the move (above) | whiteout inode mint + its dentry at the old name | the rename is done with no whiteout — the overlayfs upper layer loses its deletion marker. |

Two properties make these worse than a lost update: (a) the survivor state is
*self-consistent per volume*, so nothing in fsck's C1–C6 classes flags it as
torn (a positive `nlink` with no dentry is exactly what a hard link looks like
from the child volume's side); and (b) the whole class is **untested** — every
in-tree suite is single-volume, which is why the acceptance leg below is a
prerequisite, not a nicety.

### 2.2 Intent-record format (proposal for S3.5)

An intent is a **journaled record on the COORDINATOR volume** (the parent's
volume for every op above — it is the one whose commit orders the namespace),
written in the SAME whole-tx entry as commit A, so the intent and the first half
are atomic by construction (one checksummed journal entry — no new durability
primitive is needed):

```text
tree:  TREE_XATTRS on the coordinator's ino 1   (the `job:` record precedent, KD-2)
key:   xattr_key(1, hash56("xtx:" || tx_id), coll)
value: XattrValue { name: "xtx:{tx_id}", value: IntentRecord }

IntentRecord (LE):
  magic     [u8; 8]  "SQZXTX01"
  version   u32      1
  tx_id     u128     random (never a counter — no cross-volume clock exists)
  op        u8       1=link 2=unlink 3=rmdir 4=dir-rename 5=whiteout-mint
  flags     u32      bit0 = compensation-only (A already committed, B refused)
  coord_vol u16      slot id of the coordinator volume
  part_vol  u16      slot id of the participant volume
  parent    u64      GLOBAL ino
  child     u64      GLOBAL ino
  name_len  u16
  new_name_len u16
  fencing   u64      the op's fencing token (staleness gate at recovery)
  checksum  u64      xxh3 over the record with this field zeroed
  name      [u8]     old/only name
  new_name  [u8]     rename target name
```

Protocol (per op):

1. **A + intent in one tx** on the coordinator (atomic).
2. **B** on the participant (its own whole-tx entry).
3. **Intent retirement**: a tombstone Delete of the intent key on the
   coordinator. Retirement may batch (the M7 conveyor) — a duplicate retirement
   is a no-op.

Recovery at mount (before the volume serves): scan the coordinator's `xtx:`
records — bounded, and zero on a healthy set — and for each, decide with the
PARTICIPANT's current state, because B is idempotent-checkable:

* `link`: participant `nlink` already counts the new dentry ⇒ retire; else
  re-run B (roll FORWARD — the dentry is the user-visible half and A already
  committed it).
* `unlink`/`rmdir`: participant `nlink` already decremented ⇒ retire; else
  re-run B. Never roll back: A removed the name, which the caller was told
  succeeded.
* dir `rename`: both parents' `nlink` deltas are idempotent by comparing the
  dentry sets; re-run the missing half.
* whiteout mint: absent ⇒ mint; present ⇒ retire.

So the protocol is **roll-forward with idempotent participants**, and the
`flags` compensation bit exists only for the one case that cannot roll forward
(B refused *deterministically* — e.g. `nlink` overflow or a participant volume
retired mid-op), where recovery undoes A instead. That asymmetry is what makes
compensation small.

### 2.3 Constraints the implementer must honour

* **Ordering**: the intent must be durable before B — it rides A's entry, so
  this is free, but B's commit must NOT be batched into A's conveyor window on
  the coordinator (different volumes, different conveyors — already true).
* **Lock order**: intents add no new lock population; they stage into the tx
  that already holds the op's 4a guards (`Arc<[DlmGuard]>`), so §4.9 is
  unchanged.
* **Slot migration**: `xtx:` records live on ino 1 of the coordinator, which the
  VL5 slot machinery already treats as volume-local — a slot move must drain
  intents first (the KD-8 rebind pattern), or recovery will look on the wrong
  volume.
* **fsck**: an unretired intent older than one mount is a finding class (C8),
  not a repair — recovery owns it.
* **Acceptance (the plan's own gate)**: a two-meta-volume test with a
  commit-boundary seam asserting the pre- or post-state and never an
  intermediate one. The seam already exists in this branch:
  `uring_fs::arm_barrier_stall` / `arm_write_stall` hold one volume's commit at
  an exact point while the other proceeds, and `uring_fs::power_cut` reverts the
  unsynced half — no new harness is required.

---

## 3. Microbenches (mandatory, both commit-adjacent)

* `meta_lv_bench` → **`kv_superblock/{encode_sector, classify_sector0,
  sector_generation, backup_offset}`** — DUR-5 put sector 0 on a commit-adjacent
  path (the layout-deltas bit is stamped during ordinary write traffic) and now
  pays two encodes per stamp. Field shape: `SuperblockV3::plan` for the shipped
  256 KiB-node geometry on a 64 GiB volume (the `dev_substrate.sh` mds size).
* `write_path_bench` → **`write_indirect_map/{encode_checksum, verify_decode}_{700, 4096}`**
  — DUR-6's digest sits on every indirect publish, and CoW means every publish
  pays it. Field shapes from `tests/indirect_map_backend_keys_tests.rs`: 700
  entries is the real spill boundary at the 64 KiB-node inline cap; 4 096 ≈ a
  16 GiB file at the shipped 4 MiB block size.

Both smoke clean under `cargo bench --benches -- --test`. No measured numbers
are claimed here: this box is thermally capped and shared, and the baseline tier
(`tests/run_bench_baseline.sh`) is the only sanctioned measurement venue.

---

## 4. Durability-matrix cells

`tests/durability_matrix_tests.rs` leg B excludes `StripedWriteThrough`,
`InplaceOverwrite` and `RewriteShadowClose` as *red-today-by-design*, citing
DUR-6 §3 ("no ordering barrier") for the un-synced publish path, and carries
`unbarrried_publish_rows_are_a_known_gap` as the placeholder.

This branch closes that gap **for the indirect-blob publish only** — the layout
can no longer name an un-barriered blob. The three excluded leg-B rows are about
the DATA block publish (coverage-complete block DMA → block-map merge), which is
DUR-1/DUR-2's path, not DUR-6's: those cells stay excluded, and the placeholder
test's wording is still accurate. No matrix cell flips in this branch; the new
DUR-6 barrier leg (`dur6_the_blob_is_barriered_before_the_layout_names_it`) is
the first *green* row of that family and lives with its own suite.
