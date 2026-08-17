# 2026-08-17 — Rung 12: S10 LOOKUP-class SUBTREE DELEGATIONS (the engine behind the rung-11 brake)

**Branch** `feat/s10-subtree-delegation` (worktree off dev `152df2a9`).
Charter: `docs/design-full-multi-writer.md` §8.2 lever 1 + PR row 12 —
*"LOOKUP-class delegations: piggybacked grant, coherence law
(recall-before-conflicting-publish), grace re-assertion; red-first
stale-serve test; `SQUEEZEFS_DELEGATION` lever"* — consuming the frozen
rung-11 API verbatim (`.benchmarks/2026-08-17-s10-recall-valve.md`) and
discharging its residuals 1 (the wire half), 2 (eviction escalation →
`MembershipOwner::evict`) and 4 (demotion's remaining holders — decided:
the next conflicting mutation recalls them, unchanged; the storm rows
never priced a demote-recalls-now). Residual 3 (partial acks) stays: the
storm rows never priced them in either.

**Red-first discipline.** The whole suite
(`tests/mw_delegation_tests.rs`) landed first and fails to compile at
its commit (`4cbfba00` — the rung-11 shape). The spec-R5-style red HALF
is permanent: `red_half_without_the_law_a_delegated_holder_serves_stale`
runs the exact conflicting-mutation shape against the law-disabled seam
(`TEST_DELEG_COHERENCE_LAW = false`, a test-only static — deliberately
NOT a knob, the `RecallConfig{valve:false}` precedent) and proves the
stale dentry serve HAPPENS, with a control pin that the disabled law
issues zero recalls — the gate IS the mechanism.

---

## What landed

* **Wire (schema 1 → 2** — design §11 "schema +1"; KD-MW-11: no incompat
  bit, delegations are RAM): `MetaRequestFrame` carries the client's
  KD-MW-2 identity; `MetaOpResult` carries piggybacked [`DelegGrant`]s
  (the intent-lock law — acquisition never costs its own round trip;
  over-issue: a lookup earns the parent AND the resolved child) and
  reply-ridden `revokes` + `revoke_fence`. New verbs on their own block
  (`0x0400..=0x04FF`): **DelegRecall** — the holder's STANDING poll (the
  owner parks it and answers the instant a recall is enqueued: push over
  a dial-only wire without a second listener; acks ride the next round)
  — and **DelegReassert** (the NFSv4 reconstruction, KD-MW-5), admitted
  whether or not the grace window is open (the `VERB_RECLAIM` posture).
  `STATUS_DELEG_FENCED` refuses a fenced holder's poll AND re-assert.
* **The coherence law** (recall-before-conflicting-publish, OWNER-side):
  gated at the `RoutedMetaBackend` mutation surface — trait verbs AND the
  layout-publish funnels (`set_layout_and_size`/`merge_layout_and_size`,
  covering the authority's own writeback and the S9 shipped publishes) —
  BEFORE any 4a acquisition. **The M7-conveyor conflict the charter
  flagged as a STOP condition does not arise**: the gate completes before
  the tx enqueues, so recall-ack happens-before conveyor batching by
  construction; nothing reaches inside the conveyor. The
  grant-vs-mutation check-then-act race is closed structurally
  (in-flight registry → recall snapshot → grant path re-checks AFTER the
  lane records, retracting by `surrender`; the stamp is read only past
  that point). unlink/rename resolve the affected CHILD inos into the
  recall set (rmdir/rename-over of a delegated directory), paid only
  while delegations are outstanding (`deleg_gate_wants_children`).
* **The self-conflict surrender** (`RecallLane::surrender`, additive to
  the frozen API): the mutating holder's own grant dies WITH its
  mutation's reply (reply-revokes land before the caller's await returns
  — read-your-own-writes by construction), never a wire recall — the
  serial mutate-then-lookup stream pays zero added rounds, and
  self-churn never stamps the thrash valve (it protects WIRE fan-out;
  a surrender has none).
* **The client serve** (`MetaShipRouter` route arm: lookup/getattr/
  readdir): serves from the holder's reader-revalidation view under
  three gates — the **watermark** (below), **channel freshness**
  (a completed recall round within the derived window; fail-closed on
  any transport doubt; the pathological remainder is bounded by the S6
  eviction/T_self arithmetic — the S9 custody plane's documented
  posture, restated verbatim), and the **era** (entries never serve
  across a term). A delegated NEGATIVE is authoritative (the view's
  dentry set is exact while the grant is live and current). In-flight
  serves drain BEFORE the ack (the never-serve-after-ack law);
  `dlm_delegation_stale_serves` is the must-stay-0 tripwire and the
  drain makes it structurally unreachable.
* **Grant/recall reordering fence**: grants and recalls travel on
  different sessions (batch lane vs poll channel) with no
  cross-ordering; every recall carries the owner's grant-seq fence at
  frame build, seq is minted BEFORE the lane records, and the client
  tombstones at the fence — a dropped in-flight grant is only ever a
  lost optimization.
* **Timeout escalation** (rung-11 residual 2 discharged):
  `expire_overdue`'s dead recalls are LOUD
  (`dlm_delegation_recall_timeouts`), fence the holder on this plane
  (poll/reassert refuse; re-admission by remount), and evict it from an
  armed membership plane (minting the S7 dead epoch).
* **Reader-TTL stretch, attr face**: a delegated getattr reply
  stretches the kernel attr TTL to the channel-validity horizon, and
  the recall-drop pushes `notify_inval_inode` through the
  mount-installed sink — "the recall is what bounds staleness". The
  ENTRY-TTL face needs per-name tracking and belongs to rung 13's
  per-directory machinery (stated residual).
* **`SQUEEZEFS_DELEGATION`** (ENG-10 registry, `Kind::Bool`, **static
  default `on`**, read only when the mw ownership plane is armed — the
  `SQUEEZEFS_MW_ROLE` precedent; set-but-unarmed is announced-inert at
  mount, never a refusal; `=0` on an armed mount is the A/B control).
  The coherence law itself has only the test seam, never a knob.
* **Stats**: the `dlm_delegation` family (design §13 spellings —
  grants/hits/recalls/reasserts/entries/bytes + the two tripwires — plus
  engagement gauges: installs, reply_revokes, declines, tombstone_drops,
  channel_{suspends,rounds}, evictions), and
  `dlm_delegation_recall_phase_ns` {gate_wait, holder_drain} composing
  with the rung-11 lane's `dlm_revoke_phase_ns`; delegation bytes ride
  R5 as the sheddable `dlm_delegation_entries` component (floor 0,
  weight 1 — every shed entry costs one re-earned grant, never a wrong
  answer). Arm wiring: `arm_multi_writer` installs the delegation host
  beside the S8 owner service (rollback on listener failure);
  `disarm` uninstalls it.

## The four LIVE findings (every one caught by the fleet, invisible to the pre-fix cargo fixtures; each has its repro-port)

| # | Finding | How it presented | Fix + pin |
|---|---|---|---|
| 1 | **Delegated serves recursed through the daemon verb router** — the serve read the local view via trait verbs, whose bodies consult the hook; on an armed co-writer the hook routes the foreign ino back into the delegated arm | first co-writer mount: `fuse3-tpc` lane stack overflow, mount dead before its stats inode answered | hook-free local bodies (`getattr_local`/`readdir_local`; trait verbs now delegate to them) + every serve/stamp read uses them; the cargo fixture now INSTALLS the daemon verb router (the live shape) so the whole suite recursed pre-fix (`160a17a9`) |
| 2 | **The authority never routed the delegation verb block** — `with_meta` claimed 16..=17 only; every poll answered `RPC_UNKNOWN_VERB` | grants issued, recall undeliverable → timed out → the HEALTHY holder evicted (the escalation working, aimed at its own transport) | `with_meta` claims `VERB_DELEG_BASE..=VERB_DELEG_LAST` for the same service; the fixture serves through the PRODUCTION `AsyncVerbRouter` — an unrouted verb turns the suite red (`a06307a0`) |
| 3 | **The rung-11 deadline derivation had no delivery term** — `4×(rtt_p99+owner_p99)` priced drain-and-ack, but the wire that landed is a standing poll with up to two park floors of turnaround at ZERO load; live it derived **1 ms** | same eviction as #2 once the verbs routed: a healthy holder cannot possibly ack inside 1 ms of poll turnaround | `RECALL_DELIVERY_TERM = 2 × the 100 ms park floor` added to the live-evidence arm (the same constant the owner's park clamps at — the two derivations cannot drift); valve-suite arithmetic pins updated (`a06307a0`) |
| 4 | **The `(ctime, mtime, size)` stamp is not a sound dentry-set version** — attr triples alias under serial-create rates and the times economy | fleet `tar -x`: `fs/btrfs: Cannot utime: No such file or directory` — a stale AUTHORITATIVE NEGATIVE for a directory whose create had returned; `stale_serves` stayed 0 because by its own broken token the serve was "valid" | the currency token is the volume's **journal commit watermark** (`commit_watermark` = reservation frontier; serve law = the holder's adopted checkpoint tail `view_watermark` ≥ the grant's mint — checkpoint-prefix visibility makes the implication exact, and a journal position cannot alias); the attr compare is DELETED (it could only add false misses from parked-time folds); pin `the_currency_token_survives_attr_aliasing` builds the aliasing shape deterministically (`1df65642`) |

Finding #4 is the note's headline lesson: **the tripwire cannot catch a
lying token** — `dlm_delegation_stale_serves` guards the drain-before-ack
law, not the token's truth. Only the crucible (a real serial-create storm
over a real reader view) falsified the attr stamp, within minutes of the
first fleet run.

## The live leg — `tests/run_mw_matrix.sh s10-delegation` (GREEN from zero on the final binary)

Venue: `mw_fleet.sh create N=2 --multi-writer --cowriters=1`
(instance-suffixed tcp devsub, nvmet-tcp 127.0.0.1:54143;
`SQZ_MWFLEET_MW_PORT=54193`; co-located co-writer — the ops.md honest
residual), release binary `1df65642`, box 32-core / 117 GiB, kernel
7.1.6-1-cachyos-sqz. Counted-restart discipline held: every fix above
tore the fleet down and re-ran from zero; the row below is the final
binary's from-zero pass. Artifacts:
`/home/justin/Source/mwfleet-acceptance-rows/s10d-1786938629`.

| Phase | Verdict |
|---|---|
| **Engagement** (kernel caches dropped per pass so every stat reaches the daemon; instrument = `ls -1` + `stat` — `ls -l` is excluded because its per-entry getxattr/listxattr are the XATTR class, rows 13+'s, and ship by design: measured live, exactly its 25 xattr calls) | **26 delegated serves, 0 shipped verbs** across the 24-file stat pass (the point of S10); owner grants accounted |
| **Coherence, live** | the authority's `touch` in the delegated dir returned only after the holder's ack (`dlm_revokes_acked` +1, timeouts 0), and the holder saw the fresh name **immediately after the create returned** — no staleness window, no sleep |
| **Lever-off control** (`SQUEEZEFS_DELEGATION=0` co-writer remount) | 0 delegated serves, 50 shipped verbs over the same pass — the A/B is dark |
| **Authority kill-9** | successor up in 1 s; the holder re-admitted BY REMOUNT (the rung-10 documented posture) after the successor's membership grace window closed (33 s — a fresh acquire is refused in-window by design; the surviving-process in-place grace re-assert is the cargo suite's, where a holder can actually outlive its authority); delegations RE-EARNED under the successor era; one more coherence round green |
| **Oracle + tripwires** | fsck `findings: 0`, `meta_kv_block_refs_drift 0`, `owner_panics 0`, `stale_serves 0` both ends, `local_commit_refusals 0`, `invariant_tripwires 0` |
| **Teardown** | zero residue (0 mounts, 0 state dirs) |

## The informational tar-x row (NOT the row-14 gate)

`s8-serial-ab`, the EXACT rung-9 instrument (`/home/justin/Source/linux/fs`,
2,384 entries) on a fresh 24 GiB/oss fleet (`SQZ_MWFLEET_OSS_GB=24` — the
rung-9 venue size; the default 4 GiB ENOSPCs this instrument), medians of
one pass per arm, same binary both arms, single-order (informational; the
store ages ~90 MB/arm on a 24 GiB set):

| Venue | published S8 raw (rung 9) | delegation ON (`1df65642`) | lever-off control (same binary) |
|---|---|---|---|
| local | 995/s | 1200/s | 1302/s |
| ship rtt≈0 | 594/s | 628/s | 608/s |
| **ship +250 µs** | **143/s** | **151/s (+5.6 % vs published)** | 148/s |
| ship +1 ms | 46/s | 46/s | 46/s |

**Honest reading: a WASH within noise (151 vs 148 same-binary).** The
shipped-verb counts barely move (34,784 vs 34,919 at 250 µs) because a
serial CREATE storm structurally disengages LOOKUP delegations: every
own-create surrenders the parent's grant (correctly — read-your-writes),
and the re-earned grant's watermark chases a commit frontier the reader
view only reaches at checkpoint cadence, so the warming window never
closes mid-storm. That is the design's own assignment: LOOKUP delegation
is the **read-mostly** lever (proven by the leg's 26-hits/0-ships read
row); the tar-x recovery belongs to rung 13 (UPDATE intents — creates
answered locally) and rung 14 (placement), where the ≤1.10× gate lives.
The published-143 comparison is cross-binary context; the same-binary A/B
is the governing pair.

## Gates run

* `cargo test --test mw_delegation_tests` — **14/14** (red first:
  `4cbfba00` fails to compile by construction; the law-off red half and
  the finding-#4 aliasing pin are permanent).
* Touched suites serial (`--test-threads=1`): mw_delegation (14),
  mw_recall_valve (12), meta_ship (15), mw_arm_s8 (4),
  env_knob_convention (37), derivation_sweep (18), metrics (34),
  skip_ledger (21), readonly_mount (28), mw_publish_era_gate (12),
  mw_cowriter_free (23), mw_cowriter_lane (7 + 18), dlm_membership (34),
  dlm_cowriter (9/11 + 1 ignored), mw_fleet_jobs (5/28 per suite split) —
  all green.
* `cargo clippy --all-targets --all-features -- -D warnings` AND the
  shipped config `cargo clippy --all-targets -- -D warnings` — clean.
  `cargo fmt --check` — clean. `shellcheck -x tests/run_mw_matrix.sh` —
  clean. Markdown check on this note — clean.
* No new lock-free core (the delegation cache composes `scc` maps +
  atomics + the existing `sqz_notify`/`sqz_task` cores; the lane's mutex
  is rung-11's, control-plane class), so no new loom model per the
  charter's preference.
* Full `task check` DEFERRED per the standing ruling for this ladder.

## Decisions (the ones a future reader needs)

* **Delegation granularity is per-OBJECT over-issue** (parent + resolved
  child on a lookup), not a depth-budgeted subtree record: it keys the
  rung-11 lane verbatim (per-ino recall, no reverse child→parent index
  owner-side) and the subtree EFFECT emerges from the grant population.
  `DELEG_CLASS_LOOKUP` is the only defined capability bit — UPDATE/PERM/
  XATTR deliberately undefined so they cannot be granted by accident.
* **The recall transport is the holder's standing poll** on the existing
  dial-only `cluster_wire` — no second listener, no push backchannel
  invention; parked rounds answer instantly on enqueue, park bound =
  `clamp(deadline/2, 100 ms, 5 s)` (under the wire's 10 s call timeout),
  published on every reply so the two ends share one arithmetic.
* **Publishes gate as owner-local recalls** (no mutator identity on the
  publish path yet): a co-writer that writes a file it also holds a
  LOOKUP grant on pays a wire recall of its own grant per publish.
  Correct, priced, and named residual for rung 13 (the publish frame
  carries `client` — threading it into the gate is one seam).
* **`meta_ship_tests` pins the lever OFF**: its two-sandbox fixtures are
  the fencing-read venue (the client's inner is a DIFFERENT filesystem);
  delegation semantics live in the same-set reader-view suite.

## Residuals for rows 13/14 (stated, not hidden)

1. **UPDATE intents** (rung 13): the serial-create disengagement above is
   the measured motivation — creates must answer locally for tar-x to
   move; the per-directory machinery rung 13 builds is also where the
   ENTRY-TTL stretch face and per-name kernel invalidation belong.
2. **Publish-path mutator identity** (above) — reply-revoke for shipped
   publishes instead of self-recall round trips.
3. **Partial acks** (rung-11 residual 3) — still unpriced; the leg's
   storm shapes never queued deep enough to need them.
4. **Reassert-set bounding**: the re-assert frame rides the CONTROL cap
   (~16 k inos at the recall entry budget); a holder with more live
   delegations than that re-earns the tail. Fine at rung-12 populations;
   row 13's create-intent volumes should re-check.
5. **The valve note's storm table** still governs; the delegation-scoped
   counters compose with the lane's (`surrenders` joined
   `dlm_recall_*`). A counted retune of `RECALL_THRASH_CYCLES`/cooldown
   remains PR-13's, with the levers registered.
6. **Watermark spaces are per-volume solo-appender** (the shipped
   posture): under a future stamped bit-8 partitioned-append volume the
   grant/view comparison must name the partition; bit 8 is built-unstamped
   (D9), so nothing reaches this today.
