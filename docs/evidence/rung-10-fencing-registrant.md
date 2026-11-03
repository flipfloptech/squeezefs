# Rung 10 — the fencing registrant (2026-11-03, decision 0003 §DG)

**The ruling** (the owner, on §DF.11): a dead member's host is device-fenced on nvmet 7.3 today, with no
kernel change, by a fencing registrant. nvmet 7.3 answers a reservation HOLDER's preempt of another key by
re-typing its own hold (`nvmet_pr_preempt`: `holder == reg` before PRKEY is read); a NON-holder's preempt of a
key that is not the holder's unregisters every registrant of that key and keeps the reservation
(`nvmet_pr_unreg_all_host_by_prkey`) — the spec's rule too.

## What changed

| | change |
|---|---|
| registrant | `guard/fencer.zig`: a second host identity of the manager's host (hostid `uuid4(sha256(domain ‖ wire host id ‖ 0 ‖ set))`), a connect of the metadata subsystem under it (address off the metadata controller, namespace uuid verified, shared through the fabric registry), its char node only; registered at the manager's hold after the stale-sibling rule, re-registered at a lapse's re-acquire, given back by its last user |
| key | the HOST's own key: a takeover's or a death's preempt removes the host and its fencer in one command; the fencer can never take the reservation (§DG.2 — a deliberate departure from "never equal to any member's key") |
| recovery | step 1 and the re-fence issued by the fencer (`Mount.fencer`); the own-key guard kept; the report still judges (`.ignored` / `.failed`) |
| hold | a hold under which the fencer preempted stays at the last clean unmount (§DF.6) |
| multipath=Y | refused by name (a second identity joins the head; no per-path char node) — the holder's posture, warned |
| counters | `pr_fencer_registrations`, `pr_fencer_preempts`, `pr_fencer_unavailable`; per volume `pr_fencer {…}` / `pr_fencer_absent` |
| fake | the holder is a registrant (host id + key, as nvmet); preempts follow F2/F3 |

## The model (`guard/host_fence_model.zig`)

`Member` (+ the manager's fencer, a member co-located with the manager, the manager's unmount): 15,481 states,
depth 27, HOLDS under spec AND nvmet 7.3 semantics. `Election` (+ every manager's fencer, the superseded
manager's resumed fencer preempt): 308,427 states, depth 29, HOLDS. New invariants: `FencerHeld`,
`FencerFenced`, `ManagerUnreserved`, `FencedHostRegistered`, `FencerWrote`. Weakenings, each failing: no fencer
on nvmet → `FencedMemberWrote` (§DF.11's limit); a fencer key of its own → `FencedHostRegistered`, under
multipath=Y `FencerWrote`, in the election `FencerHeld`; no own-key guard → `FencerFenced`; the fencer's
preempts not keeping the hold → `FencedMemberWrote`; every earlier member weakening fails under nvmet + fencer.

## The reds (committed alone, red on dev + the seam)

- "a member's death on nvmet 7.3 is FENCED by the manager's fencing registrant" (the flipped pinned row):
  `appender_recovery_preempts` expected 1, found 0. Green: preempts 1, `pr_fencer_preempts` 1, ignored 0,
  re-fences 0; the report: host A's key gone, rtype 3, the manager's key holding beside its fencer (regctl 2);
  after the manager's umount regctl 1 (the hold kept), its fencer controller gone.
- "a frozen member fenced through the manager's fencing registrant never lands its in-flight writes": X's
  windows held (`SQUEEZEFS_TEST_LANDING_HOLD_AT_WRITE`), X stopped, evicted and fenced, the hold released
  (`SQUEEZEFS_TEST_LANDING_HOLD_RELEASE`), X resumed. Dev: the windows LANDED (no reservation-conflict line in
  30 s). Green: X's journal latches the fence class (`journal: reservation-conflict errno at …`).

## Checks (kernel.org 7.3.1, `nvme_core.multipath=N`)

The device-laws row (`87d5bac`): the same-key fencer fences a member (its write `-EBADE`, the reservation kept, the
holder the host, not the fencer); a takeover removes the host and its fencer in one command; the superseded
fencer's preempt conflicts — ×3 green. Nvmet tier: ×3 idle at `8a76998` 63 pass / 1 skip (multipath-tcp) / 0
fail, at `87d5bac` 64/1/0, ×2 under `cpu+smoke` load (load ≈ 31) 64/1/0. Smoke: the 27 eviction/re-join/KILL
rows ×3, 71/71 each. Units (11 filters): 634 pass, 41 skip, 0 fail. Rails 32/32; `zig fmt --check` clean.

## Open

multipath=Y (the per-controller form, §DG.3); data-namespace PR fencing (§DG.12); `pr_linux.Device` serializes
nothing between its users (§DG.13(d)).
