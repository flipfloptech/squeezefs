# Rung 10 — the create path, wave 6: the leaf set (2026-10-21)

Continues `rung-10-create-path-5.md`. Venue: this workstation (32 threads, `7.2.8-9.33-omarchy-sqz`),
the RAM NVMe set (512-B grain); A = dev `051c994`, B = `perf/create-path-6`. Scratch
`~/tmp/createpath2/` (`w2-{A,B}-w6*`), `~/tmp/w4cp/`.

## The leaf set, named (decision 0003 §BD)

A regular `mdtest -u` create stages **two records**: the child's inode `Put` (its rotor slot's tree)
and the parent's dentry `Put` (the parent's slot tree). Nothing else: the parent's stamp is parked
(§AV) and a regular child changes no parent nlink; no xattr, no gather, no stripe record. The "3.9
takes" were these two leaves taken **twice** — by the committer's heap admission (§AW put it before the
enqueue, under the leaves' write locks) and by the apply — less the odd parked-stamp drain. Red test: a
kernel create takes ≤ 2 leaves at the apply and none at the admission; charged ≡ released.

**Before (wave 5, `self`):** `leaf_wait_other` 1 235 128 takes / 320 000 creates = 3.86 per create,
41 µs each. **After (§BD):** admission takes 273 077 (0.85 per create: the 45 % of admissions on a
leaf at capacity, which keep the locked law) at 56 µs, apply takes 540 486 (1.7 per create) at 12 µs.

## Lever 1 — the lock-free heap admission (§BD.1; ships)

On a volume with room (`fastPathRoom`: free ≥ 2× the reserve + one extent per transaction the conveyor
can hold) and no heap-full posture, a leaf whose published log end (`CachedNode.log_hint`, stored at
every write unlock) plus the bytes admitted onto it without the lock (`inflight_admitted`) plus this
transaction's fits in place is admitted with no lock; the charge is released after the apply's unlock
published the bytes. 55 % of admissions take it under the storm; the rest meet a leaf at capacity — a
256 KiB node fills with 4 KiB appended frames and is rewritten (198 MB rewritten against 80 MB appended
per 320 k creates) — and keep the locked law, whose job (the promise, the compaction) is exactly then.

| `mdtest -n 10000 -u` | A (dev) | B (§BD, venue) | ratio |
|---|---|---|---|
| -np 32 file creation | 100.7 / 101.7 k | 102.3 / 104.8 k | 1.02× |
| -np 32 file removal | 106.6 / 107.8 k | 111.3 / 107.4 k | 1.02× |
| -np 32 dir creation / removal | 57.9 / 57.5 k · 59.3 / 59.7 k | 64.5 / 59.1 k · 60.0 / 59.3 k | 1.07× / 1.00× |
| -np 64 fc / fr / dc / dr | 108.4 / 102.5 / 55.6 / 55.1 k | 120.6 / 97.1 / 64.0 / 56.0 k | 1.11 / 0.95 / 1.15 / 1.02× |

(Loads 1–6 at every phase's end; interleaved A B A B.) Neutral to +11 %: the admission's takes are
gone from the committer, the rate barely moves. **The serializer is not the leaves' sharing but the
work under their locks:** the apply's hold is 116 µs per 4-tx window on the venue, of which 24 µs is
lock wait — the rest is the apply's `O(overlay)` remerge and the checkpointer's freeze and rewrite of
the same leaves under the same lock (`sqz-ckpt` 2–3 s of CPU per 2.7 s phase).

## Lever 2 — the lane's ino window (§BD.2; measured, not shipped)

One window of `node_size / 128` inos per lane per slot (one leaf of inode records), one `fetch_add` per
window, consecutive mints adjacent: file creation unchanged (96.5 k venue, 119.4 k `self`), and the
watermark-derived used-inode count the `statfs` row reads inflated by the windows' gaps (102 912 for
200 creates in the two-member row). The inode leaf's sharing is not the limit; reverted.

## `self` re-judged (§BC) with the smaller set

-np 32 `self`: 103.3 k creates, 61.4 k removals (one quiet leg; 119.4 k / 117.7 k on another). -np 64
`self`: **48.1 / 22.8 / 29.6 / 30.6 k** — the convoy stands (0.4× / 0.2×): with the leaf's work under
its lock unchanged, 64 committers taking their own leaves queue behind that work. `venue` stays the
default; `self` the lever.

## Kernel patch 0034 (atomic open): not drafted this wave.

## Tests

Unit `fmt test rails`: **2901/2991 passed, 90 skipped**, rails 29/29. Smoke (coherence, shared hot dir,
dead manager, kill rows (a)(b), kill matrix, kill -9 mid-tar ×2, replay proof, durability points):
**50/50**; INVALIDATION COHERENCE ×5. fsck clean after every session.

## Risks and what was not verified

- The fast path's room law is extent-based; a volume one extent shy of `2 × reserve + txs` takes the
  locked law for everything (safe, slower). An over-admission is bounded by the lanes' in-flight bytes
  and lands as a split from guaranteed extents.
- Not run: the shared-directory and stat shapes this wave (unchanged code paths), the data rows (no
  data path touched), the NVMe substrate. Patch 0034 not started. The next lever is named in §BD.3.
