# 2026-09-08 — the S11 assembler contracts D-5 turned red: the accept tick was holding the standing notice poll off

| | |
|---|---|
| **Branch** | `fix/assembler-contracts-notice-poll` off `dev` `e0bdd35f` |
| **Commits** | `53629a94` (red: the pre-ack contracts hold the poll off, finding 27's ledger contract, the ask tie test, the failure gauge contract) · `ef8e7f39` (fix: the seam, the derived ask, the failure gauge, `call_reply_bound`) · this record |
| **Trigger** | gate red on `e0bdd35f`: `tests/mw_authority_assembler_tests.rs` `the_renewal_carries_the_notice_and_the_ack_releases_the_parked_grant` ("the pending mark landed") and `an_unacked_demotion_resolves_at_lease_expiry_on_the_owners_clock` ("B parks behind the barrier"), deterministic; bisected to D-5's accept-tick commit `863a4304` (green at its parent `80bcd162`) |
| **Class** | **an in-process fixture artifact the accept tick had been hiding, not a product regression** — plus one latent product hazard found while establishing it (§5) and one vacuous sibling contract (§4) |
| **Fix** | `data_grant::test_set_notice_poll(Option<bool>)` — a seam read once by `connect`, never a knob — held off in exactly the contracts that pin a pre-ack state or a specific notice carrier; every demotion sampler a delta; the poll's ask derived as half the wire's reply bound; `dlm_custody_notice_poll_failures` |
| **Venue** | dev box, debug test binaries, `--test-threads=1`; every number here is a timestamp from a temporary probe (reverted, nothing of it committed) — scoping evidence per the venue rule; the claims are laws, not numbers |

## 1. The bisect

`863a4304`'s parent is `80bcd162`; its diff is `src/cluster_wire.rs` only —
`accept_loop`'s `Err(WouldBlock)` arm went from `std::thread::sleep(ACCEPT_POLL_TICK)`
to `wait_for_accept(&listener, ACCEPT_POLL_TICK)` (a `poll(2)` on the listening
fd). Both contracts were green at `80bcd162`, red at `863a4304`, red on the
tip. Nothing else moved between the two.

## 2. The probe

Temporary `eprintln!` timestamps (one monotonic origin per process) at: the
dial (`RpcClient::connect_sync` — TCP connected / admitted), the accept
(`accept_loop`'s `Ok`), the client's poll round start/done
(`notice_poll_run`), the owner's poll park (`VERB_CUSTODY_NOTICE_POLL`
handler), and the test's own instants (authority up, `arm_client` returned,
B spawned, sampler exit, `renew_all`). The OLD accept-loop shape was
re-created with an env-gated `std::thread::sleep(ACCEPT_POLL_TICK)` in place
of `wait_for_accept` — same binary, both shapes.

**OLD shape (sleep tick) — `the_renewal_carries_the_notice_…`, green:**

```
0.001 dial: tcp connected node-a (workload)       ← accept thread asleep in its tick
0.100 accept                                      ← the WHOLE tick paid
0.101 dial: admitted node-a
0.102 test: arm_client returned
0.102 poll: round start → dial: tcp connected     ← the notice session's LAZY first dial;
                                                    the accept thread re-polled at 0.100,
                                                    found nothing (2 ms too early), slept again
0.104 test: B spawned
0.115 test: sampler exit marked=true  demotions=1 acks=0 polls=0 poll_notices=0
0.115 test: renew_all start → dial: tcp connected (lease session; also queued)
0.201 accept ×2 (notice session, lease session)   ← one tick later, one burst
0.202 owner: poll parked (park=733 ms)  →  poll: round done ok  (notice already pending)
0.203 test: renew_all done quiesced=2 ; end polls=1 poll_notices=1
```

**NEW shape (`poll(2)`) — same contract, red:**

```
0.001 dial: tcp connected node-a → 0.001 accept → 0.002 admitted   ← immediate
0.002 test: arm_client returned ; poll: round start → dial → accept → admitted (all 0.002)
0.002 owner: poll parked (park=733 ms)
0.004 test: B spawned
0.004 poll: round done ok                          ← the mark woke the park; absorb → quiesce → ack
0.004 poll: round start (next park)
…
3.335 test: sampler exit marked=false demotions=1 acks=1 polls=5 poll_notices=1  → panic
```

## 3. The mechanism

`WriteCustodyClient::connect_with_clock` dials the WORKLOAD session, joins,
builds the client and spawns `notice_poll_run`. The poll's own session
(`notice_session`, finding 27b) is **dialed lazily** by the first round's
`call_once_on` — i.e. a few hundred microseconds after the workload
session's accept. The old accept loop did one `accept()` per iteration,
found `WouldBlock` (the second dial had not happened yet) and **slept the
full 100 ms tick**; the poll's connection sat in the kernel backlog until
the next wake. During that window the poll was structurally unreachable:
no parked RPC on the authority, so a demotion marked in it could only be
heard at the incumbent's next verb.

The two contracts' whole observation — B's acquire parks, the mark lands,
the 10 ms sampler sees the pending mark and `!b_task.is_finished()` — took
~13 ms from `arm_client`'s return, inside the tick. Then `renew_all` dialed
the lease session, which queued behind the poll's dial; the tick expired;
both were accepted in one burst; the poll's RPC parked with the notice
already pending and answered at once (`quiesced=2`: the renewal reply AND
the poll both absorbed it, the second ack a ledger no-op). So the poll was
not dead for the test — it was dead for the first 100 ms, which was the
only 100 ms the contracts looked at. The task brief's probe reading (polls 0,
notices 0, quiesced 0 "during the test") is the reading at the first
assertion instant; by the end of the run the poll had completed one round.

With `poll(2)` the notice session's dial is accepted at once, the poll is
parked ~1 ms after `connect` returns, and B's mark (2 ms later) wakes it:
absorb → quiesce hook → `ack_demotion` → B's grant issues, all before the
sampler's first 10 ms tick. That is finding 27's designed behaviour
verbatim. The contracts were written 2026-08-17 (`270cee26`); finding 27
landed 2026-08-28 (`4d1d4b0f`); in the eleven days between, the tick hid
the pre-emption on every run.

**Also convicted while auditing the siblings:** `mw13_authority_death_mid_demotion_…`
read the process-global demotion ledger against an ABSOLUTE threshold
(`demotions >= 1`). In the full suite two alphabetically-earlier tests had
already moved it, so the sampler exited at its FIRST check — before B had
even been scheduled — and `!b_task.is_finished()` held vacuously; in
isolation the contract failed 3/3 on the tip for exactly the poll reason
above (`b1.is_err()` would invert too: A's poll acks, B's grant ISSUES
before the kill). `the_renewal_carries_the_notice_…` had the same absolute
threshold; `d0` in both was captured AFTER B was spawned.

## 4. What was vacuous, and the audit

Every test in `tests/mw_authority_assembler_tests.rs`,
`tests/mw_ranged_lease_ladder_tests.rs` and `tests/dlm_range_custody_tests.rs`
was read for the assumption "a pre-ack state (or a specific notice carrier)
is observable while a live `WriteCustodyClient` holds custody":

| Contract | Client live? | Verdict |
|---|---|---|
| `the_renewal_carries_the_notice_and_the_ack_releases_the_parked_grant` | yes | **pre-ack state + renewal-carried arm** — poll held off; `d0` before spawn; delta sampler |
| `an_unacked_demotion_resolves_at_lease_expiry_on_the_owners_clock` | yes | **"never acks" + owner-clock expiry arm** — poll held off |
| `mw13_authority_death_mid_demotion_a_reasserts_and_the_demotion_restarts` | yes (both eras) | **pre-ack at the kill + renewal-resolved restart; vacuous absolute sampler** — poll held off; `d0` before spawn; delta sampler with a `marked` pin |
| `shrink_notices_ride_acquire_and_release_replies_not_just_renewals` (ladder, f16a) | yes | **carrier-specific** — the probe showed the poll completing two answered rounds in the test's 60 ms, i.e. both "the acquire / release reply carried it" assertions were satisfied by the poll's ack; poll held off, and the contract passes on the f16a carriers alone |
| `the_demotion_barrier_withholds_…`, `an_unacked_demotion_resolves_when_the_incumbents_grant_dies` (assembler §2), `a_foreign_required_over_the_stretch_tail_shrinks_…`, `a_written_tail_still_demotes_honestly`, `the_tail_shrink_ledger_closes_…` (range custody) | no — `LocalLockManager` only | not affected |
| `the_learned_ceiling_stops_repeat_stretch_collisions`, `zero_true_sharing_keeps_the_shared_clauses_silent` (ladder) | yes | their headline is the learned ceiling / zero demotions, not the carrier; `renew_all` still runs and the ledger's `+1 ack` holds whichever carrier delivered (acks are idempotent — the probe's `quiesced=2` closed as one ack). Left as they are; their "rides the renewal reply" comments describe the pre-f27 world |
| `a_quiet_incumbents_demotion_resolves_at_poll_latency_not_renewal` (ladder, f27) | yes | **not vacuous**: withholding the poll makes it red (verified — the asker burned its 3 s budget) |
| `the_notice_polls_park_never_starves_the_clients_own_verbs` (ladder, f27b) | yes | pins the workload session's independence from the parked poll; it does not claim the poll heard anything, so it has no vacuity to check against the poll — and it is the fixture that showed the quiet round's timing in §5 |
| `dlm_multi_writer_tests::a_revoke_during_a_parked_acquire_never_commits_custody` | no (raw frames) | whole-file arbitration park, no demotion |

The new coverage, `tests/mw_ranged_lease_ladder_tests.rs`:

* `the_standing_poll_is_the_quiet_incumbents_only_carrier_and_its_ledger_moves`
  — two arms in one fixture (the f27 quiet-clock authority, 10 s derived
  cadence). **Armed:** a quiet incumbent's poll completes a round, CARRIES
  the demotion (`dlm_custody_notice_polls` and `dlm_custody_notice_poll_notices`
  both move), the quiesce hook runs, the ledger closes through the ACK
  column, the peer is granted in < 2 s. **Withheld** (the seam): the same
  shape burns the peer's whole 1 s budget, gauges flat, nobody acks — the
  pre-f27 dip, verbatim. The second arm is what makes the first
  non-vacuous and what proves the seam the three S11 contracts stand on
  actually holds the poll off.
* `the_standing_polls_ask_sits_inside_the_wires_reply_bound` — §5's tie
  test (verified red with the ask set back to the bound).
* `a_poll_round_that_dies_is_counted_not_silent` — §5's gauge (red before:
  the key did not exist).

This new contract is green on BOTH accept-loop shapes (on the old one the
poll answered at ~100 ms, still far inside its 2 s bound); its red is the
poll withheld / pre-f27. The contracts D-5 made honest are the three
assembler ones, which is the intended division: the fixture artifact is
fixed where it lived.

## 5. The product finding: a zero-margin race on every quiet poll round

Not the bisected mechanism — found while weighing the brief's candidate
"the park's 10 s vs the 10 s read timeout". `notice_poll_run` asked
`park_ms: 10_000`; the authority clamps the park to
`min(park_ms, renew_interval)`, and on the fleet `renew_interval =
min(10 s, T_self/3)` is **exactly 10 s** (45 s TTL); the client's reply wait
is the socket read timeout installed at dial time, `DIAL_TIMEOUT` =
**10 s**. So a QUIET round's reply lands at `park_start + 10 s + slice
overshoot + RTT/2`, and the client's timeout fires at `recv_start + 10 s`
where `recv_start` precedes `park_start` by the request's RTT/2 + decode.
The client loses by ≈ RTT + the owner's wake latency — unless the kernel's
`SO_RCVTIMEO` jiffy rounding covers it.

Measured here (the f27b fixture, 10 s park, loopback): `owner: poll parked
0.002 → poll: round done ok 10.002` — the reply won by the rounding, and
the field's 48–51 answered rounds per fleet row (single-box nvmet-tcp, a
loopback RTT) are the same shape. On any venue whose RTT plus owner wake
exceeds the rounding (a fabric, a busy authority) the client times out
first: `call` errs, `call_once_on` drops the notice session, the loop
backs off 500 ms and re-dials — a 500 ms window per quiet round with **no
parked poll** (a demotion landing in it waits for the re-dial), one
connection churned per 10 s per co-writer, and **no ledger entry**: the
loop counted only answered rounds, so the field would have read "the
channel lives, fewer notices".

Two changes, both small:

* **The ask derives from the bound with margin** —
  `data_grant::notice_poll_park() = cluster_wire::call_reply_bound() / 2`
  (5 s at the shipped bound). The authority's clamp is unchanged, so the
  assembler fixture's 733 ms parks are byte-identical; a quiet co-writer
  sends one small frame per 5 s instead of per 10 s. Never a constant of
  its own: the tie test makes drift red.
* **`dlm_custody_notice_poll_failures`** — rounds that ended without an
  answered park. Steady growth on a quiet co-writer is session churn.

What this does NOT change: the poll's semantics, the §9.3 ledger law
(`demotions ≡ acks + fence_resolves`), the renewal as the worst-case
carrier, the 500 ms backoff, or any wire schema.

## 6. Reachability of the accept-tick mechanism itself on real postures

* **Pre-D-5 binaries in the field:** every notice-session dial (the first
  round, and every re-dial after a failed round) paid ≤ 100 ms before the
  poll could park; a demotion marked in that window was heard at ≤ 100 ms
  — still poll-class against the 10 s cadence. A latency term D-5 already
  removed, not a dark channel. No product regression.
* **TLS peer:** the custody client dials plaintext (`security: None`);
  N/A.
* **Reconnect:** on the tip the re-dial is accepted at once; the dark
  window is the 500 ms backoff by design ("a flapping authority is not
  hammered"), now counted.
* **A single-connection client:** the poll's session is its own by
  finding 27b; nothing shares it.

## 7. Not claimed

* No fleet row, no field number; the tie test and the failure gauge land
  on in-process evidence only. Whether any field row ever lost the
  reply-bound race is unknowable retroactively (the gauge did not exist);
  the first fleet row on this binary should read
  `dlm_custody_notice_poll_failures` ≈ 0 on every quiet co-writer.
* The ladder's `the_learned_ceiling_…` / `zero_true_sharing_…` comments
  that attribute the shrink notice to the renewal reply were not rewritten
  (their pins hold whichever carrier delivers).
* The 10 ms sampler loops in the S11 contracts remain sampler loops (they
  wait for the mark, which is legitimate — the mark lands on the authority's
  lane, not the test's); what changed is that they read deltas and that
  the state they observe can no longer be raced away by the product.

## 8. Suites (all `--test-threads=1`, debug)

`mw_authority_assembler_tests` 21/21 (the three formerly red/vacuous ones
also 3/3 each in isolation) · `mw_ranged_lease_ladder_tests` 18/18 (three
new) · `dlm_range_custody_tests` 41/41 · `dlm_multi_writer_tests` 16/16 ·
`dlm_cowriter_tests` 18/18 · `cluster_wire_tests` 25/25 (+1 ignored) ·
`owner_dispatch_hop_tests` 8/8 (+1 ignored) · `audit_instruments_tests`
27/27 · `env_knob_convention_tests` 21/21 (no knob was added);
`cargo fmt --check`, `cargo clippy --all-targets --all-features -- -D warnings`,
`cargo clippy --all-targets -- -D warnings`,
`RUSTDOCFLAGS="-D warnings" cargo doc --no-deps`, `tests/check_markdown_links.sh`.
No `AGENTS.md` change: no product law moved (the poll's semantics, the
§9.3 ledger law and the carriers are unchanged; the ask's derivation and
the gauge are mechanism).
