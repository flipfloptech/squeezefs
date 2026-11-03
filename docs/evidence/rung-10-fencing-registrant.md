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
| device | one PR command per char node at a time; reports in per-call buffers; the holder's multi-command sequences under the guard's lock (§DG.15, §DG.20) |
| lapse | the re-acquire reads the device's claim again under the hold: not ours → fail-stop (§DG.19) |
| node | every metadata user refuses a fencer's node by name (`registerMetaDevice`); `scripts/udev/60-squeezefs-fencer.rules`, which deployments must ship (§DG.16, §DG.20) |
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
| the lapse claim window (re-review MAJOR) | the beat ran on over a successor's claim | `83c552d` / `ea2dc35` |
| `appender clear` on the fencer's node (re-review minor 1) | its stderr named no fencer | `1bfecd1` / `2a0af0e` |

## Models (`guard/host_fence_model.zig`)

`Member` (the manager's fencer, a co-located member, the manager's unmount): 15,481 states, depth 27, HOLDS under
spec and nvmet 7.3. `Election` (step-faithful for M1: the re-acquire, the fencer's re-register, the hold check, the
recovery thread under the fencer's lock, the lapse's claim re-read): 1,786,694 states, depth 34, HOLDS. Weakenings fail: no fencer on nvmet
→ `FencedMemberWrote`; a fencer key of its own → `FencedHostRegistered` / `FencerWrote` (multipath=Y) /
`FencerPreemptUnheld` (election); no own-key guard → `FencerFenced`; the fencer's preempts not keeping the hold →
`FencedMemberWrote`; no hold checks or no fencer lock → `FencerPreemptUnheld`; no claim re-read after the
lapse's re-acquire → `OverwroteAcked` (§DG.19).

## Checks (kernel.org 7.3.1, `nvme_core.multipath=N`)

Final round (`2a0af0e`): full `zig build test` 3259 pass / 187 skip / 0 fail; units (11 filters) 640/43/0;
nvmet tier ×3 idle 65 pass / 1 skip (multipath-tcp) / 0 fail each, ×1 under `cpu+smoke` load 65/1/0; the 27
eviction/re-join/KILL smoke rows ×3 71/71 each; rails 32/32; `zig fmt --check` clean. Earlier rounds: §DG.11,
§DG.18.

## Open

multipath=Y (the per-controller form, §DG.3); data-namespace PR fencing (§DG.12); the udev rule is installed by
provisioning, not yet on this venue; a killed manager's fencer controller on a host that never mounts again
(§DG.17 (3)); the issue lock held across a stuck command, bounded by the NVMe I/O timeout (§DG.15).
