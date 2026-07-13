# Staged zeros-LOSS leg 5 — CLOSED (v2: lock-serialized promotion release + write-side binding revalidation)

Date: 2026-07-13 · Branch `fix/staged-zeros-leg5-v2` off dev @ `91bb1e8` · Rails: 16-core
cap + `CARGO_BUILD_JOBS=12`, 3.5 GHz cap, 8G cages, unique sandbox `~/tmp/leg5v2`.
Artifacts: `~/tmp/leg5v2/fail1..2/` (recops/models/live images/tapes), probe logs.

## Why the withheld candidate regressed 074/112/616 — the taped answer

The promote-outcome probe (one aged roll each, identical protocol):

| Build | promote ok | promote refused | refusal rate |
|---|---|---|---|
| withheld candidate (touch+adopt) | 6,246 | 7,254 | 54 % |
| v2 (no touch) | 7,038 | 15,962 | **69 %** |

This **overturned** the starvation hypothesis: high promotion-refusal is the *baseline*
under RMW storms (every re-stage advances the generation; queued promotions of
superseded images refuse by design — each refusal is an allocate→write→free cycle).
The candidate's regression was therefore not starvation but its **adoption protocol
racing the very ABA it couldn't see** (below) — the same reason its clean build stayed
red. The 074/112/616 re-flakes and the aged residual were one defect.

## The residual, finally taped WITH logging active (no masking)

The zero-count + free-attribution tape on the v2-precursor build caught it:
one device offset (`29657923584`) **ping-ponging promote→free→realloc at storm rate
for a single file** — a promotion publishes `block_map[0]`; the next RMW commit
*legally* releases it as superseded; the allocator hands the offset straight to the
next promotion of the same file. The write-side consumers of that mapping read it
with **no binding revalidation**:

- the **RMW ring-miss seed fallback** (routing.rs) resolved the mapping and
  device-read it once, unvalidated — a supersede-free-retenant landing mid-read
  poisoned the whole-image seed (punched zeros / another incarnation's bytes) and the
  re-stage **codified** it (tape: `zseed` jumping to 100 % zeros; the aged
  GOOD→0x0000 signature);
- the **truncate durable clip** revalidated only on fetch *error* — the poisoned
  *success* (freed+rewritten bytes reading back fine) passed.

This is the read path's 074/127/616 promoted-mapping ABA **on the write side** —
which is also the serialization point the withheld candidate's logging accidentally
provided (rail c): the stderr mutex ordered seed reads vs superseding frees. v2
encodes that ordering deliberately.

## Mechanism shipped (`833d92c`, tests `c238c70`)

1. **Lock-serialized destructive step**: `promote_staged_file` releases the ring entry
   INSIDE its commit's `INODE_META_LOCKS` section — the staged-arm commit's
   ring-residency check is now exact (present means present until the lock drops).
   No generation touch ⇒ no mass promotion invalidation.
2. **Residency check + adopt**: the staged-arm commit publishes ring-authoritative
   meta only when the entry is resident; a consumed entry ⇒ adopt the promotion's
   published mapping (dirty — refill-immune under the leg-1 dirty-authority rule),
   free nothing.
3. **Promotion size floor**: persist `max(current.size, image_len)` — the image IS
   acked content; a lagging size resurrected `{old size, new image}` through the TTL
   refill as a zero tail (leg-5 fail3 tape: size=306494 with a 425111-byte image).
4. **Write-side binding-revalidated fetches** (the core): the RMW seed fallback and
   the truncate durable clip use the read path's serve rule — fetch, re-resolve the
   freshest identity, serve only a still-bound fetch; on movement re-resolve
   (bounded 64, then loud EIO — never a silent zeros seed). `staged_identity_retries`
   now counts write-side rebinds (1,422 in one clean aged roll — each a
   would-have-been poisoned seed).

## Clean-build verdict table (the finish line)

| Build | Aged protocol | QUICK |
|---|---|---|
| dev 91bb1e8 (base) | 3/3 red | {003,213} ×3 |
| v2 + tape (logging ON) | 3/3 clean | — |
| **v2 CLEAN build (all diagnostics stripped)** | **3/3 CLEAN** | **{003,213} ×3 — zero re-flakes of 074/112/616** |

`staged_payload_lost_reads = 0`; daemon anon ≤ ~1.0 GB in the 8G cage.

## Acceptance

| Gate | Result |
|---|---|
| Aged protocol (clean build) | **3/3 clean** (was 3/3 red on every prior tip since the family opened) |
| QUICK ×3 | **{003,213} only, 3/3** (074/112/616 green — the withheld candidate's regression absent) |
| Leg-1..4 seeded suites + hammer | all green (refill 4, ABA 2, pressure 7 incl. leg-5 hammer, truncate-stale 6, visibility 7, crash-recovery 6, rmw-alloc 1) |
| Full serial gate | **670 passed / 0 failed**; clippy `-D warnings` clean; fmt clean; doc 0 warnings; bench smoke 118 ok |
| Crash contract | kill9 deep churn (`SQUEEZEFS_CRASH_ROUNDS=60`) green; unmount-kill soak **PASS 30 cycles**; loom 19/19 |
| LTP | **174 PASS / 0 FAIL / 0 BROKEN / 9 SKIPPED** |
| Perf rows 1–3 (order-controlled pair) | write 1120 vs 1145 MiB/s, read 4229 vs 4555 (run spread), rand-4k 141.0k vs 71–117k IOPS — flat / better |

## Design-pass note (for the record)

Legs 2/3/4/5 and the earlier staged-identity family were all promotion-lifecycle
races. v2 closes the family's last reproducible member by making the lifecycle's one
destructive step lock-serialized and every durable-mapping consumer
binding-revalidated — the two invariants a designed promotion state machine would
enforce structurally. If another member ever surfaces, a design-level pass over the
staged lifecycle (explicit states: ring-authoritative / promoted / adopted /
superseded, with single-owner transitions) is the recommended shape; the invariants
above are its first two theorems.
