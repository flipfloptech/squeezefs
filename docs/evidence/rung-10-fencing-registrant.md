# Rung 10 — the fencing registrant (2026-11-03, decision 0003 §DG)

**The ruling** (the owner, on §DF.11): a dead member's host is device-fenced on nvmet 7.3 today, with no
kernel change. nvmet 7.3 answers a reservation HOLDER's preempt of another key by re-typing its own hold
(`nvmet_pr_preempt`: `holder == reg` before PRKEY is read); a NON-holder's preempt of a key that is not the
holder's unregisters every registrant of that key and keeps the reservation — the spec's rule too.

## What changed

| | change |
|---|---|
| registrant | `guard/fencer.zig`: a second host identity of the manager's host (hostid derived from the wire host id and the set; hostnqn `nqn.2026-10.io.squeezefs:fencer:<uuid>`), connected to the metadata subsystem (shared through the fabric registry, one user line per namespace), its char node only; registered at the manager's hold after the stale-sibling rule, re-registered at a lapse's re-acquire, given back by its last user |
| key | the HOST's own: a takeover's or a death's preempt removes the host and its fencer in one command (§DG.2) |
| preempts | recovery's step 1 and the re-fence through the fencer, only after a report shows THIS host (key + host id) holding, under the fencer's lock; a fencer whose host does not hold after its registration is taken back (§DG.14) |
| device | one PR command per char node at a time; reports in per-call buffers (§DG.15) |
| node | a metadata mount/format on a fencer's node refused by name; `scripts/udev/60-squeezefs-fencer.rules` (§DG.16) |
| hold | a hold under which the fencer preempted stays at the last clean unmount (§DF.6) |
| multipath=Y | refused by name — the holder's posture, warned (§DG.3) |

## Reds (each committed alone, shown failing first)

| row | on the code before the fix | commit (red / fix) |
|---|---|---|
| a member's death on nvmet 7.3 is FENCED (the pinned row flipped) | `appender_recovery_preempts` expected 1, found 0 | `be35d5e` / `8a76998` |
| a frozen member's released in-flight windows | they LANDED (no reservation-conflict line in 30 s) | `be35d5e` / `8a76998` |
| M2: two issuers on one Device | A returned B's -EBADF | `7ba6145` / `3e28493` |
| M1: a fencer re-registered after a takeover | its preempt landed (`.fenced`) | `b75c1be` / `4c6325f` |
| M3: a mount on the fencer's node | it became ready | `8941970` / `3f9362d` |

## Models (`guard/host_fence_model.zig`)

`Member` (the manager's fencer, a co-located member, the manager's unmount): 15,481 states, depth 27, HOLDS under
spec and nvmet 7.3. `Election` (step-faithful for M1: the re-acquire, the fencer's re-register, the hold check, the
recovery thread under the fencer's lock): 1,589,758 states, depth 33, HOLDS. Weakenings fail: no fencer on nvmet
→ `FencedMemberWrote`; a fencer key of its own → `FencedHostRegistered` / `FencerWrote` (multipath=Y) /
`FencerPreemptUnheld` (election); no own-key guard → `FencerFenced`; the fencer's preempts not keeping the hold →
`FencedMemberWrote`; no hold checks or no fencer lock → `FencerPreemptUnheld`. Reachable, pre-existing: the
lapse's claim probe before its re-acquire (§DG.13(h)).

## Checks (kernel.org 7.3.1, `nvme_core.multipath=N`)

First round (`8a76998`/`87d5bac`): the device-laws row ×3 green; nvmet tier ×3 idle 63–64 pass / 1 skip
(multipath-tcp) / 0 fail, ×2 under `cpu+smoke` load 64/1/0; the 27 eviction/re-join/KILL smoke rows ×3 71/71.
Review round (`3f9362d`, `3873ce6`): full `zig build test` 3258 pass / 187 skip / 0 fail; units (11 filters)
639/43/0; nvmet tier ×3 idle 65/1/0; ×1 under load 64/1/1 — roundtrip-loop's harness-port assertion, a
benchmark's loop shares holding port 53176 on the venue then (every fencing row passed); the 27 rows ×3
71/71; rails 32/32; `zig fmt --check` clean.

## Open

multipath=Y (the per-controller form, §DG.3); data-namespace PR fencing (§DG.12); the udev rule is installed by
provisioning, not yet on this venue; a killed manager's fencer controller on a host that never mounts again
(§DG.17 (3)).
