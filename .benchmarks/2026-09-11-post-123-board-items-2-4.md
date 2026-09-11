# 2026-09-11 — Post-1.2.3 board items 2–4: the full metadata heap, reclaim atomicity, the packed direct-drive arm

**Landed on `dev` `862b8077`; batch gate GREEN (09:48–10:52, pinned
worktree, from zero: 379 suites / 4,946 tests, 18 stages, both audits).**
Three parallel red-first campaigns off the 1.2.3 tip, merged narrow → wide
(reclaim `d7573e5b` → PK8 `445525e1` → metadata `862b8077`). Two of them
surfaced defects the 1.2.3 binary ships — both recorded here with their
repros, both fixed.

---

## 1. Item 2 — a full metadata volume is ENOSPC, never a fail-stop (`862b8077`; red `359afc39`)

**The defect** (`.benchmarks/2026-09-09-inline-raise-sweep-local.md` §3): a
full 1 GiB metadata volume produced `checkpoint: metadata heap exhausted
(free=0, reserve=80) … compaction deferred` on twelve nodes, then
`pending-free retirements wedged … 8 consecutive barriered checkpoint
cycles … volume marked FAILED`, then EIO on every op and `writeback error
latched`. A full heap is neither corruption nor fencing; the right answer
is ENOSPC on the metadata plane.

**The verified mechanism — why the reserve saved nothing.** The §4.7
user-side claim (`claim_user`, floor = reserve) **had no production
caller**. A user commit never claims an extent: its records land in leaf
overlays and the CHECKPOINT's flush pass claims for the SMOs they force —
through `claim_internal`, floor 0, like every SMO. So acked user growth
spent the reserve itself; the pass hit `free=0` mid-flush and
skip-and-deferred; the deferred nodes' floors pinned the tail BELOW every
parked retirement gate (the gates are the SMOs' entry seqs at the head, the
floors are older), so nothing could drain; the wedged-tail audit — built
for a tail nothing can discharge — fired. §4.7's "the reserve is
allocatable only by compaction/checkpoint internals, so no
write-to-free-space deadlock" was true only as a claim law; with no user
side it protected nothing.

**The fix — every acked record is flushable by construction.** The commit
pass (`run_batch_pipeline` step 3b, under the union leaf write locks,
before any reservation) projects each touched leaf's flush
(`NodeDirty::projected_log_end`): fits ⇒ in-place append, nothing owed;
overflows ⇒ the SMO's extents are **promised** on the node
(`NodeDirty::promise`, ledger `NodeCache::heap_promised`) against
`claimable − promised` — a fold that fits `fold_capacity` (estimated with
the pending records merged so a Delete/Put SHADOWS its key: a delete into a
full leaf projects as the compaction it is) is net-zero and admitted down to
the **compaction floor = reserve/2** (`alloc_ext::compaction_floor_extents`,
derived, tie-tested); a split (`smo_extents_for_parts` = the SMO's own
greedy ¾-fill part count + rounding + cascade, +1 root) is admitted only
above the whole reserve. A refused member fails ALONE, pre-reservation,
`KvError::NoSpace` → **`ENOSPC`** (`clone_kv_error` no longer flattens it
to `Corrupt` = EINVAL), after one retry behind two barriered cycles when
budget is returnable. Promises are consumed at the SMO, re-promised on
successors that inherit an overflowing leftover, released when an in-place
append leaves the remainder fitting, Drop-owned. An allocation-free fast
path keeps the pass service time at dev parity (the completion-hop suite
interleaved ×25: 23/25 vs dev 21/25).

**The two standstill classes** (`checkpoint_cycle`): a barriered cycle
whose flush pass deferred a node for `NoSpace` is the **space** class —
`heap_full_cycles++`, `heap_full` latched, one WARN per transition, the
wedge rung neither advanced nor reset, never terminal, cleared when a pass
defers nothing and the minimal split fits above the reserve. **FAILED** is
the **wedge** class only: nothing deferred for space, retirements parked,
no release, no tail advance for the bound's cycles.

**Contracts** (`tests/meta_volume_full_tests.rs`, 24 MiB volume, 64 KiB
nodes, 1 MiB ring): red on 1.2.3 = the field chain verbatim (`heap
exhausted … deferred` ×10 → `wedged … volume marked FAILED` → `got
Io("commit aborted while parked for ring space") (errno 5)`); green 6/6:
ENOSPC with the reserve intact (`free=9 claimable, 1 promised, reserve=8`
— free never reached 0), never FAILED across fill/refusals/deletes/remount,
reads serve, unlink + destroy commit, creates resume after deletes +
checkpoint, `pending_free` and `heap_promised` both 0 at quiesce, the
seam-driven space standstill runs 12 barriered cycles past the wedge bound
un-failed and clears. Gauges `meta_kv_heap_full` (0/1), `meta_kv_enospc_
refusals`, `meta_kv_heap_full_cycles`, `meta_kv_heap_promised`. No knob.

**Limitation stated:** the v1 tree never merges leaves, so deletes return
record space INSIDE leaves, never extents — after a fill, creates resume
into the room the deletes freed and hit ENOSPC again once a new leaf is
needed. Leaf merge is a separate item.

## 2. Item 3 — reclaim atomicity: the release rides the destroy (`d7573e5b`; red `2695aca0`)

**The two residuals** of the corpse-sweep fix
(`.benchmarks/2026-09-11-corpse-sweep-double-release.md` §7): (A) a FAILED
release commit followed by a SUCCESSFUL destroy orphaned the ino's durable
references forever (a permanent C8-visible leak); (B) a single corpse whose
own destroy exceeded the journal's whole-entry cap failed loud at every
mount.

**Finding beside them — a 1.2.3 shipped defect:** the standalone release
commit was NEVER chunked, so **every unlink of a file with ≳ 3,000 block
references (≈ 11.6 GiB at 4 MiB) deterministically orphaned its whole
ledger** — the release entry exceeded the cap, the destroy then succeeded,
and the references outlived their owner. Not an I/O-error corner: the
common large-file delete. Red: `4000 durable reference(s) name the DESTROYED
corpse … left: 4000 right: 0`.

**The fix — ONE transaction per ino.** `KvMetaBackend::destroy_inodes_
releasing` stages each ino's release `Delete`s into the SAME `KvTx` as its
inode + xattr `Delete`s under the exclusive 4a guards `lock_many` already
takes, witnessing every release first (the C9 repair's precedent; §4.10
makes both half-states unrepresentable). `reclaim_destroy` packs inos by
their priced footprint (`destroy_entry_bytes` + `release_records_bytes`)
and bisects a failed group to the failing ino, which is **refused** —
record, layout and references retained (`reclaim_destroy_refused_release_
failed`, one WARN) for the next pass. Peer-owned inos use refuse-and-retain
(the release ships first; the destroy only if it landed). **Journal entries
per reclaim: N block-owning corpses went N + 1 → 1** (pinned at N = 8).

**B — one over-cap corpse destroys across entries** (`destroy_inode_
chunked`): records packed greedily to the cap under one held 4a guard in
the order releases → every other xattr → the `layout` xattr + the inode
record LAST. Any committed prefix leaves the record WITH its layout, so the
next sweep recomputes the full release set; already-committed releases
witness no record (counted skipped — never a second decrement); the rest
witness and free; the remainder destroys. Contract: 4,603 xattrs, kill -9
between entries, the next mount finishes it (oracle 0). Deleted:
`destroy_batch_bisect`, `plan_destroy_chunks` (superseded).

Gauges `reclaim_destroy_refused_release_failed`, `reclaim_release_destroy_
joint_commits`, `reclaim_single_ino_chunked_destroys`; seams
`TEST_FAIL_RECLAIM_RELEASE`, `SQUEEZEFS_TEST_DESTROY_CHUNK_STOP_AFTER`.

## 3. Item 4 — PK8, the interposer's direct-drive arm for packed tenants (`445525e1`; red `89bfb9e3`)

A promoted packed tenant is a **`staged`-layout inode** (`file_type=staged`,
`block_map[0] = bk@inc:off:len`) — so the direct-drive prelude refused it at
the `file_type != "striped"` screen as `Meta`, never reaching the decorated-
mapping line; the shim's cold read of every packed small file took the
handler path. The arm: on a passthrough volume, a request window inside
the tenant plans ONE grain-aligned ranged read at `block base + off +
floor_grain(rel)`, bounded by the tenant's slot and the file size, on the
**mapping's** backend fd (the FIND-PK-0 law), with the whole-block arm's
custody snapshot + CQE revalidation; transformed volumes, overlays, ring-
resident newer images and tenant-crossing windows stay handler-served,
each named in the decision ledger. Lever `SQUEEZEFS_IPC_DD_PACKED` **ON**
(the same single DMA the handler issues — no tradeoff found); gauges
`ipc_direct_packed_serves` / `ipc_direct_packed_bytes` / `ipc_direct_
ineligible_packed_shape`. Local scoping row (20,000 × 16 KiB packed files,
shim randread): 99.93 % direct under ON, `packed_reads ≡ ipc_ops_read` on
both arms, IOPS par within dev-box noise — the box row is not owed (an arm,
not a lever decision). `tests/ipc_direct_packed_tests.rs` (10).

**Finding beside it — a 1.2.3 shipped wrong-bytes exposure (`445525e1`;
red `51fa0c47`):** the WHOLE-BLOCK direct-drive arm ran no dead-lifetime
screen. A key whose lifetime is already dead at plan time (the offset
freed and reissued while the RAM layout still names the old `@inc` key —
the stale-binding class the handler's rebind ladder exists for) reads a
STABLE fill word across plan → CQE, so the revalidation passed and the DMA
returned the reissued offset's bytes. Red: the striped arm planned
(`Ok(IpcDirectSnapshot{ key: "4194304@e13wu1oi" … })`), and with the
assertion neutralized the ring read completed with the wrong bytes
(`the REISSUED offset's bytes were served`). `block_key_lifetime_dead` now
screens both arms (one RAM lookup) → `Overlay` → handler, whose funnel
answers the handler's own law (EIO + the rebind ladder).

## 4. Release status

1.2.3 ships: the fail-stop on a full metadata volume (item 2), the ≳ 3,000-
reference unlink ledger orphan (item 3's finding), and the whole-block
direct-drive wrong-bytes exposure through the shim (item 4's finding). The
first is a robustness defect (EIO where ENOSPC belongs; recovery = the
volume stays FAILED until remount); the second a permanent C8-visible leak
on every large delete; the third serves wrong bytes to a shim client on a
stale binding after a free/re-mint — rare (the handler path's rebind
counters, `stale_binding_rebinds`, are its rate) but a correctness class.
All three fixes are on `dev` for 1.2.4.
