# 2026-08-20 — The §9.3a tail-shrink fabric sessions: 24× probe confirmation + three new findings

Follow-on to `.benchmarks/2026-08-19-blob-aware-merge-and-fabric-venue.md`.
Venue: the mw cheap preset (4 × i4i.2xlarge on-demand, us-east-1b,
nvmet-tcp over the real network), binary `37de58ba` (dev with the
§9.3a tail shrink `a4d893c0`). Two sessions (4 and 5); session 5 ran on
the baked base AMI and cost ~$0.55 end-to-end (11.6 min including
teardown). **Standing user ruling (2026-08-20): NO further AWS sessions
without explicit approval; bugs get fixed locally.**

## 1. The tail-shrink fix, on the venue that convicted the fabrication

| Metric (probe pass, identical shape both days) | 2026-08-19 (pre-fix) | 2026-08-20 (fixed) |
|---|---|---|
| probe aggregate | **42.58 MiB/s** | **1,017.55 / 937.80 MiB/s** (sessions 4/5) |
| self-sized file | 928 MiB (inline domain) | **10,240 MiB — the indirect domain, on fabric** (sizer wanted 22 GiB) |
| `range_custody_demotions` | 9 (fabricated) | **0** |
| `dlm_custody_conflicts` | 822 | **0** |
| `range_custody_desired_trims` | 2,367 | 41 (the benign clip arm) |

The fabrication class is gone at real RTT and fabric ingest moved
**24×**; the fabric venue now clears the ≥ 750 MiB/s acceptance bar it
could not approach pre-fix. The FORMAL engagement row (later-phase
snapshot deltas: `tail_shrinks > 0`, gate GREEN end-to-end) remains
open — both sessions died mid-row on finding 1 below, and further
fabric sessions await user approval. Probe-phase ledgers and both
sessions' harvested artifacts: `.benchmarks/cloud/2026-08-19-234051/`
(session 4, probe 1,017) and `.benchmarks/cloud/2026-08-19-235559/`
(session 5 — full `mw-rows/` with per-phase ior outputs, daemon logs,
p0 snapshots; the evidence-before-verdict pull's first catch).

## 2. Finding 1 — the checkpoint AlreadyFreezing wedge (authority; ROOT of the cascade)

At 03:56:25 ONE maintenance pass failed:
`record value length 66121 exceeds the per-volume cap 65792` — the
10 GiB shared file's composed INLINE layout map: the owner-side chained
merge (sites a/b) materializes `delta.apply_to(inline head)` with **no
spill decision at the cap crossing** (the spill exists on the router
and in the scoped-Put site only — the residual-1 compose landed the
indirect-HEAD arm, not the inline→oversize CROSSING arm). The oversize
record then failed its node's freeze mid-encode, the error escaped
`freeze_locked` AFTER `begin_freeze` with `abort_freeze()` wired to no
production caller, and node `0x23c0000` stayed FREEZING forever: every
subsequent checkpoint tick refused `AlreadyFreezing` (mislabeled
"corrupt KV encoding" by the error-class shortcut), the journal tail
pinned, and the conveyor degraded until the fleet cascaded. A sibling
window: `compact_node_forced`'s straight-line `end_freeze` cleanup dies
with a dropped future (no drop guard). Fix campaign:
`fix/kv-freeze-wedge` (trigger spill arm + both latch windows + the
honest error class + a self-heal tripwire + one admission choke point).

## 3. Finding 2 — membership renewal starved past a 45 s TTL (co-writer)

cw2's log shows **zero** per-attempt renewal-failure warnings before
`membership renew refused: … not in owner's census` → the designed §6.7
self-fence → custody poisoned → ior fsync EIO. Zero warnings means the
renewal tick NEVER RAN for > 45 s (hypothesis 2 of the briefing): the
member renewal loop rides the 2-thread `sqz-meta` lane pool beside all
meta-plane client work, and under the write storm (plus meta-plane
futures wedged behind finding 1's parked authority conveyor) the
cadence lost its liveness. The self-fence machinery itself worked
exactly as specified — the bug class is renewal-liveness isolation, a
first-class load-dependent product bug. Red-repro design (occupy the
lanes past `T_self`, assert the member is never swept) is in hand;
campaign queued behind findings 1/3.

## 4. Finding 3 — co-writer posture economy (pre-fence W1 spam; post-fence free storm)

Pre-fence: every eligible aligned overwrite probed W1's
`begin_patch_sole_owner`, hit `plane_gate` and logged ERROR while
polluting `cowriter_accounting_refusals` (a documented must-stay-≈0
tripwire) — the write itself correctly fell back to CoW + shipped free.
Post-fence: every in-flight pipeline upload whose shipped publish was
(correctly) refused ran the never-published-offset cleanup
`allocator.free_block(offset)` — an allocator-direct arm the co-writer
refusal text claims "no product path uses", falsified by the
error-cleanup arms (`fuse_client.rs:17493`, `:16505`,
`assembly_tasks.rs:385`, jobs undo arms). Leak-safe both ways (nothing
destructive executed; derivation recovers the offsets) — the bugs are
posture-blind hot arms, ERROR-level log storms, and tripwire-counter
pollution. Fix campaign: `fix/cowriter-posture-economy`
(`patch_ineligible_posture` decision bucket; the quiet
`cowriter_unpublished_abandons` arm on the cleanup class; the corrected
defense-in-depth text).

## 5. Venue/rig lessons landed this session (all on dev)

`16931b02` (cloud-init package bake — the wedged regional Ubuntu mirror
cost 40+ billed minutes twice; `us-east-1.ec2.archive.ubuntu.com` was
black-holed over v4 AND v6 while `archive.ubuntu.com` answered in
0.2 s), `10cfae77` (baked-base AMI preference — `squeezefs-mw-base-v2`
`ami-0c40b68421a1fcd8e`, packages + fixed mirror baked in, session
fixed overhead → ~4 min; plus evidence-before-verdict: bench-mw pulls
the on-cluster row artifacts UNCONDITIONALLY before dying — session 4's
failure evidence was destroyed by its own teardown, session 5's came
home). Cloud spend, both days total: **~$8.5**.
