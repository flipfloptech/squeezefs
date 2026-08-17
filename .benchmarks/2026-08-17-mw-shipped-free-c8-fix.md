# 2026-08-17 — The co-writer tar+rm C8 drift: reclaim vs the open rewrite epoch

**Branch** `fix/mw-shipped-free-c8-drift` (worktree off dev `1536c4f1`).
**The finding**: `.benchmarks/2026-08-17-s10-update-intents.md` §FOUND —
the `s10-intents-tarx` leg's fsck oracle red with
`[C8] durable block-reference drift: 1 durable record vs 0 counted layout
references` after a co-writer tar-extract + `rm -rf` sweep (11 findings /
4 arms; 5 on the intents-OFF control — rung 13's machinery structurally
dark, so the leak pre-dates it). This note is that handoff's fix rung.

---

## The conviction

**Repro** (this campaign, live): one tar+rm pass of the real linux-src
`fs/` tree (2,286 files, 49 MB) on a fresh `mw_fleet.sh create 1
--cowriters=1` fleet, extract on the co-writer, `rm -rf`, fsck on the
authority → **6 C8 findings**, each `durable 1 vs derived 0` at map
index 0 of a destroyed ino. No netem, no intents, no unmount needed —
the finding's "intents irrelevant" claim confirmed. (The synthesized
tarx tree does NOT repro: its ≤2 KiB files store INLINE — zero blocks,
zero refs. The measured row's real tree is what makes files striped.)

**The counter closure that names the site** (stats-inode deltas across
the repro pass — every number exact):

| Ledger row | Δ | Reading |
|---|---|---|
| `patch_ineligible_shared` (m50) | **+6** | six sub-block overwrites-of-mapped-blocks hit `begin_patch_sole_owner`, whose `plane_gate` REFUSES on a co-writer (the six `W1 in-place sub-block patch refused` log lines, 07:58:13–18, mid-extract under lane-ENOSPC churn) |
| `overlay_overwrite_installs` / `overlay_epoch_feeds` (m50) | **+6 / +6** | each refused patch fell back to the B4 overlay arm: fresh CoW dest K′, gap-seed from the old K (`overlay_gap_seed_old_bytes` +25,141,248 = 6×4 MiB − 24,576 user bytes), **rewrite-epoch feed** |
| `rewrite_shadow_swaps` (m50) | **+6** | all six epochs "closed" — through `close_rewrite_epoch`'s `_ => Ok(())` arm (clean/VANISHED layout), because by then `rm -rf`'s reclaim had destroyed the ino |
| `meta_ship_publish.free_shipped_blocks` vs `free_served_blocks` | **+1024 vs +1018** | exactly 6 shipped frees answered **`NonTerminal`** — the epoch-close frees of the displaced Ks arriving while their durable records still lived |
| `meta_kv_block_refs_staged` vs `released` (m0) | **+1018 vs +1018** | 1,012 releases were real; **6 were no-op Deletes** of records never staged (the RAM map's K′ bindings) — leaving the 6 real K records orphaned |
| fsck | **6 × C8** `durable 1 vs derived 0` | the orphans, named with `ino … idx 0` owners in the drift log |

**The mechanism, per victim ino:**

1. Extract publishes block 0 → mapped K, record `(K, ino, 0)` staged
   (the delta rode the shipped layout publish — correct).
2. A later sub-block write of the same mapped block: the W1 patch is
   **refused by the co-writer plane gate** (patch is authority-only —
   §6.2 item 6), so the write rides the B4 overlay → **rewrite epoch**:
   the RAM map rebinds 0 → K′, K parks in `epoch.displaced`, and the
   `{release K, take K′}` pair parks in `pending_block_refs` — **the
   durable map still names K; nothing durable has moved** (correct: the
   epoch is the deferred-swap design).
3. `rm -rf` reclaims the ino BEFORE the epoch closes.
   `delete_file` computed the corpse's release set **from the RAM
   snapshot alone**: it released `(K′, ino, 0)` — never staged, a no-op
   Delete — and never released `(K, ino, 0)`, the record that WAS
   staged. It never drained `pending_block_refs` and never touched the
   epoch. `destroy_inodes` then erased the layout that justified K's
   record.
4. The idle sweeper's `close_rewrite_epoch` (≤ ~10 s later) found the
   layout VANISHED → freed the parked K **without draining anything**
   (the `Ok(())` arm's comment blesses the frees; nothing blesses the
   stranded notes). On the co-writer that free ships → the authority's
   ledger still holds `(K, ino, 0)` → **`NonTerminal`, nothing moves**.
5. Net, forever: one orphaned `TREE_BLOCK_REFS` record (fsck C8,
   `durable 1 vs derived 0`) **plus** a durably-referenced, unreachable
   block (honestly-unavailable space).

**Why solo tar+rm never leaks** (it demonstrably doesn't — weeks of
clean fsck): on the authority posture the SAME sub-block overwrite rides
the **W1 in-place patch** — one DMA, same key, no displacement, no
epoch, no accounting. The vulnerable composition (reclaim of an ino with
an OPEN rewrite epoch whose bindings never persisted) is only *reached*
by tar+rm through the co-writer's patch-plane refusal → B4/CoW
fallback. The CLASS itself is posture-blind — a solo whole-block
sequential rewrite + instant `rm` inside the epoch's idle horizon would
leak identically — which is why the fix lives in the reclaim path, not
in the co-writer.

**Why the oracle reads it as C8**: the derived census skips
`nlink == 0` (and destroyed) inodes by construction; the durable scan
counts every record. An orphaned record is exactly `durable N vs
derived 0`.

## The fix (`src/routing.rs`, `DataRouter::delete_file`)

One choke point — every destroy funnels through `delete_file`
(fuse reclaim, fsck C9 repair) — three composed moves, no new vehicle,
no new stats key, no knob:

1. **The reclaim's existing fence also tears down the ino's open
   rewrite epoch** (removed under the same `INODE_META_LOCKS` acquire
   that orders in-flight publishes). Race-free: reclaim admission proved
   no open handles (shadow records ride the write path), and a
   mid-flight `close_rewrite_epoch` re-registers under this same lock on
   a transient failure, so the removal always collects the epoch or
   finds it already closed-with-save (either is consistent). The
   sweeper's post-destroy close becomes a structural no-op (`Ok(false)`).
2. **The epoch's parked custody joins the corpse's frees**: guards drop
   first (`close_rewrite_epoch`'s order), then the displaced keys
   (disjoint from the snapshot map by construction) and any shadow
   values the snapshot map does not name (the RAM-evicted refetch loses
   the KD-1.9 compose once the epoch is removed — without this arm the
   shadow B-keys would strand allocated, fsck C2). FIND-M11-A's
   reclaimed-ino orphan-discard law: nlink 0, nothing open, never live
   acked data.
3. **The corpse's release commit drains `pending_block_refs`**: every
   deferred op maps to a RELEASE of its reference — a drained release
   names exactly the durable record the RAM map no longer shows; a
   drained take names one the map-derived set already covers or one
   never staged, and a Delete of an absent key is a no-op — so the
   union is conservative and exact. Dedup is O(pending), never
   O(file size) (the sparse-corpse law).

**Standing laws, checked**: the layout-publish delta still rides the
publish transaction untouched (this enlarges the ALREADY-sanctioned
standalone corpse-release commit, delete_file's own spec'd transaction);
one tx = one entry (one `CommitBlockRefs`, one KvTx); on a co-writer
that commit is the witnessed schema-5 vehicle verbatim — era gate +
`(lease_epoch, request_id)` dedup unchanged; release-before-free order
unchanged (which is what turns the 6 `NonTerminal`s into `Freed`s);
the terminal-free law unchanged (the durable effect IS the Delete —
now actually staged); C8 stays report-only.

## Red-first repro + pins

* **The repro** — `tests/durable_block_refs_tests.rs::`
  `reclaim_of_an_ino_with_an_open_rewrite_epoch_orphans_no_durable_reference`
  (Vector C): the FUSE-harness venue drives the REAL product path —
  striped fixture → whole-block displacing overwrite (epoch opens,
  notes park) → unlink → `delete_file` → `destroy_inodes` → the
  sweeper's close → oracle. **RED at `68ecd597` against dev with the
  exact field shape** (`drift [(vol, 0, durable 1, derived 0)]`), green
  with the fix; also pins the epoch teardown at reclaim and both
  blocks' space return.
* **The wire-face pin** — `tests/mw_cowriter_free_tests.rs::`
  `a_reclaim_shaped_release_batch_is_exact_idempotent_and_frees_read_freed`:
  the fix's co-writer output over the real wire — ONE witnessed
  `CommitBlockRefs` carrying a no-op Delete (never-staged shadow key)
  beside the real one; verbatim re-ship answers the winner's cached
  outcome at journal-entry equality (`replays` +1, finding-#6 law 2);
  the subsequent shipped frees read **`Freed`, never `NonTerminal`**;
  both offsets re-enter the authority's free supply.
* **Era arm**: unchanged and already pinned —
  `mw_publish_era_gate_tests` (a swept era's `commit_block_refs`
  refuses `PUBLISH_STALE_LEASE` with nothing applied, before the
  witness window).

## The standing repro: the FIXED class green from zero, every run

Venue: `mw_fleet.sh create 1 --cowriters=1` (`SQZ_MWFLEET_MW_PORT=54193`,
`SQZ_MWFLEET_OSS_GB=24`),
`SQZ_MWMATRIX_TAR_SRC=/home/justin/Source/linux/fs` (the measured row's
real tree — the synthesized tree is inline-only: 2,881
`layout_inline_writes`, zero blocks, zero refs, so it structurally
cannot exercise the oracle). Every row is teardown → create → leg, from
zero; the dev row is the same leg on the UNMODIFIED dev-tip binary — the
attribution control for both classes.

| Run (from zero) | Binary | **C8** | C10 | Leg exit |
|---|---|---|---|---|
| tarx, dev control | dev `1536c4f1` | **21** | 1 | red |
| tarx 1 | fix `88b8ea51` | **0** | 37 | red (C10 only) |
| tarx 2 | fix `88b8ea51` | **0** | 0 | **GREEN** (`PUBLISHED`, exit 0) |
| tarx 3 | fix `b1aeb6a8` (final, fmt-only delta) | **0** | 1 | red (C10 only) |
| tarx 4 | fix `b1aeb6a8` (final) | **0** | 6 | red (C10 only) |
| fanout | fix `b1aeb6a8` (final) | **0** | 0 | **GREEN** (`fsck findings:0, drift=0`, engagement exact) |

**The fixed class is dead**: 0 C8 findings and `meta_kv_block_refs_drift`
0 on EVERY fixed-binary run (plus the manual pre-fix repro's 6 → 0),
against 21 on the same-venue dev control and the rung-13 record's 11.
The A-B bracket also pins attribution: nothing about the venue changed
between the dev row and the fix rows but the binary.

**The full tarx exit is NOT claimable green ×2** and this note does not
claim it: three of four fixed-binary runs red **solely** on the
**pre-existing C10 class** (below) — fired by the UNMODIFIED dev binary
too, so per the multi-run discipline the reds are attributed to a
different standing bug, declared here as rate evidence (≥1 C10 in 4 of 5
tarx runs across both binaries; sizes 1–37), never counted as this fix's
acceptance debt.

## FOUND (pre-existing, now the tarx leg's remaining blocker): C10 under the co-writer rm -rf

The same first-oracle-on-this-workload dynamic that surfaced C8
surfaces a second pre-existing class: **C10** — `nlink 0 while a dentry
still names the ino` (directories included) and, on the big run,
whole-directory **dangling-dentry** sweeps (37 findings: `fs/afs`'s 36
children's records destroyed with their dentries still present, plus
`dir_nlink_underflows: 1` on the authority). fsck's own evidence text
names the shape: *"a cross-volume unlink whose count step committed and
whose name step did not"* — S3.5's documented un-built cross-volume
transaction window, reached live by the co-writer `rm -rf` storm
(MDS_COUNT=2: dentry on the parent's volume, record on the child's).

**Attribution: NOT this fix** — the dev-control row (unmodified
`1536c4f1`, identical venue) produces the same class (`C10ZeroNlinkNamed`
directory shape, verbatim), and this diff touches only `delete_file`'s
block-reference/epoch/free arms — no dentry, no nlink, no unlink, no
intent surface. Stochastic (0–37 findings per run, both binaries, both
intents arms). **This is this rung's stop-and-read handoff**, exactly as
rung 13's C8 was: repro = the tarx leg (real tree) on any co-writer
fleet, ~3 of 4 runs; the `dir_nlink_underflows` tripwire and
`fsck_dangling_dentries`/`fsck_nlink_zero_named` are the counters to
watch. Fix rung owns: the owner-side cross-volume unlink's count/name
window under the S8-shipped rm storm (and its interaction with the
rung-13 intent lane, if any — the 37-finding run was an intents-ON arm,
the dev control's single finding proves the class does not need one).

## Gates

* Repro suite + wire pin green; **touched suites serial, all green**:
  durable_block_refs (15), mw_cowriter_free (13), rewrite_shadow (7),
  rewrite_shadow_supersede (3), overlay_overwrite (29),
  statfs_live_accounting (1), sparse_write_bounded (7),
  parked_overlay_reclaim (2), fsck_c9 (10), writeback (11),
  mw_publish_era_gate (5), durability_matrix (12+2i),
  meta_lock_free_hoist (2), dlm_cowriter (18).
* `cargo clippy --all-targets --all-features -- -D warnings` AND the
  shipped config — clean. `cargo fmt --check` — clean. Markdown check —
  clean. No scripts touched (no shellcheck), no lock-free core touched
  (no loom).
* Full `task check` DEFERRED per the standing ruling for this ladder.
* Zero-residue teardown verified after every fleet cycle.

## Residuals (stated, not hidden)

1. **Field volumes already leaked stay leaked** — C8 is report-only by
   law (restating the ledger would erase the evidence). The 6 orphaned
   records on any pre-fix fleet volume remain fsck-visible until that
   volume is reformatted or a future repair class claims them; the fix
   stops NEW leaks only.
2. **The release-commit failure window is unchanged**: a corpse whose
   release commit fails (transport/era) still frees its blocks and
   leaves records for fsck C2/C8 — the pre-existing, documented
   conservative direction (delete_file's own crash-window rationale).
3. **`rewrite_shadow_swaps` no longer counts reclaim-torn epochs** (they
   never swapped anything — the counter was lying by +6 on the repro
   run); a dashboard keying on swaps-per-epoch gets more honest, not
   less.
4. **The pre-existing C10 class** (§FOUND above) keeps the tarx leg's
   FULL exit stochastic-red until its own fix rung lands — the C8 rows
   of that leg's oracle are green on every run of this binary, and
   `s9-fanout`'s whole oracle is green from zero.
