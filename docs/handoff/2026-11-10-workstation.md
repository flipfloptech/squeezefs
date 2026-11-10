# Handoff — 2026-11-10

## Current state and next action

**Branch `feat/pr4` is at `a489076`** (5 commits ahead of `origin/feat/pr4`, 7 ahead of `dev`).
All core items listed in `docs/handoff/2026-11-09-workstation.md` ("Still on PR-4, in this order")
have been implemented, red-first unit tested, and committed:

| Commit | What |
|---|---|
| `4aae66b` | **DC22**: in-flight command drain before fresh registration withdrawal (`WithdrawDrain`, `withdrawRegistration`) |
| `2bbfadc` | **Coalesced pre-sleep re-check cache** per `(word, generation)` and three-valued R-1 |
| `96ec6e4` | **C1 grant-term half** on successor page and succession park lifecycle |
| `009be17` | **Data read EIO `drainBarrier`** and retry once (`Device.awaitReadDrainBarrier`, `data_pr_read_retries_after_drain`) |
| `a489076` | **Synchronous data re-admission** when metadata settled (`data_pr_succession_reregistrations`, busy F10 unit test) |

**Next action:**
1. Add the **Fidelity F10 row** in `src/ops/nvmet/fidelity.zig` per Decision 0004 §8.2: a busy co-located
   joiner JA on host A writing and fsyncing continuously across takeover by successor S on host B;
   assert `data_pr_refusals_held >= 1`, `data_pr_write_refusals == 0`, no data device fence latch,
   `data_pr_succession_reregistrations >= 1`, all fsynced data whole, and clean fsck.
2. Run the targeted nvmet smoke:
   `scripts/venue 'zig build nvmet-smoke -Dnvmet-filter="F10" --summary all'`
3. Run the **Full Venue Gate** on local:
   `scripts/venue 'timeout 3900 zig build --cache-dir /build/zig-cache-v2 gate --summary all'`
4. Fast-forward merge `feat/pr4` to `dev` (`git merge --ff-only feat/pr4` on `dev`).

Read `AGENTS.md`, `docs/charter.md`, and `docs/decisions/0004-metadata-wear-preempt-all.md`.
PR-5 (metadata WEAR, markers, registration lock) stays strictly behind PR-4. Product metadata
remains WERO (rtype 3); data namespaces use WEAR (rtype 5).

---

## What was implemented & committed on `feat/pr4` in this session

### 1. DC22 — In-flight command drain before withdrawal (`4aae66b`)
- Decision 0004 §4.5.4: when withdrawing a fresh registration (due to a failed post-check, re-judge, or
  late-cleared refusal), the process must drain its own in-flight commands before issuing unregister.
- Added `WITHDRAW_DRAIN_MS` (100 ms) and `T_WITHDRAW_NS` (30 s) bounds to `src/meta/guard/member_fence.zig`.
- Introduced `WithdrawDrain` interface (`count`, `waitZero`, `wakeZero`, `awaitDrain`).
- Implemented `withdrawRegistration` with bounded wait against `WithdrawDrain` before `client.unregister`.
- Added `late_cleared_refused` and `unregistered_seen` states in `member_fence.Readmit` and wired
  `finishReregister` across `JoinedSet`, `DeviceLoader`, `JournalLane`, `KvBackend`, and `uring_fs.Pool`.

### 2. Coalesced Pre-Sleep Re-check Cache & Three-valued R-1 (`2bbfadc`)
- Decision 0004 §4.5.3, §4.5.7 (round-5 issue 1, round-7 issue 3, round-8 issue 1):
- Added `RecheckCache` per `(word, generation)` to prevent thundering herd of raw PR reports/page reads
  when multiple fiber waiters wake or enter pre-sleep re-checks concurrently.
- Supports negative caching for unreadable reads (`unreadable@(g, tick)`), resolved at next periodic tick.
- Implemented three-valued R-1 (`registered`, `unregistered`, `dead`, `unreadable`) and propagated across
  `member_fence.zig`, `joined.zig`, and `data_fence.zig`.

### 3. C1 Grant-Term Half on Successor Page & Park Lifecycle (`96ec6e4`)
- Decision 0004 §4.5.2, §4.5.3, §4.5.4, §4.5.6:
- C1 grant-term check: a successor page is only recognized if `p.appender_id == 0`, `p.state == .live`,
  `p.term > who.mgr_term`, and if `who.grant_term != 0`, `p.term == who.grant_term`.
- Enforced clean park state transitions: engagement on `metadata_pending` / `Arm.succession`, disengagement
  and word clearing on `Arm.registered`, deadline extension on first observed successor bind.

### 4. Data Read EIO `drainBarrier` + Retry Once (`009be17`)
- Decision 0004 §4.5.3, §4.5.5, P9b:
- A data read that hits `EIO` (`error.InputOutput`) during an active controller preempt-and-abort drain
  issues a non-mutating `drainBarrier` on the data namespace.
- Pure classifier `readAfterEio(answer, unknowns_already, within_claim_ttl)`:
  - `.registered` or `.unregistered` -> `.retry_read` (retries the read once).
  - `.unknown` -> backoff & re-issue (`READ_BARRIER_BASE_MS = 10`, `READ_BARRIER_CAP_MS = 1000`) up to
    `READ_BARRIER_ATTEMPTS = 5` and `READ_BARRIER_TTL_NS = 45 s`.
  - Exceeding attempts or TTL -> surfaces the original `EIO`.
- Wired `drainBarrier` in `src/io/nvme_derive.zig`, `src/io/nvme_dev.zig` (`awaitReadDrainBarrier`),
  and `src/meta/guard/data_fence.zig` (`drainBarrierFn`, `drainBarrierAfterReadEio`).
- Tracked via sharded counter `data_pr_read_retries_after_drain` in `nvme_derive.stats`.

### 5. Synchronous Data Re-admission When Metadata Settled (`a489076`)
- Decision 0004 §4.5.5 (round-4 issue 1, busy F10):
- Fixed critical bug where `DataFence.readmitAfterRefusal` rejected data refusals when metadata was
  settled with `if (!v.lapse_pending) return self.declined(...)`, latching the data fence and poisoning
  custody for a busy co-located joiner JA whose metadata beat re-registered host A before data writes finished.
- In Decision 0004 §4.5.5, `metadata_settled` (registered under reservation on every joined metadata volume
  and current) triggers §DL.3's synchronous data re-admission through the data rule.
- Added `data_pr_succession_reregistrations` counter to `data_fence.stats`.
- When `readmitAfterRefusal` re-registers on data while `v.pending_since_ns != 0` or `v.resolution.state == .pending`,
  or when `DataFence.resolvePendingWord` re-registers on the beat, it increments `data_pr_succession_reregistrations`.
- Added unit test `"decision 0004 §4.5.5 (round-4 issue 1, the busy F10): a busy joiner's data refusal that arrives after its metadata re-registration but before its data beat..."` in `src/meta/guard/data_fence.zig`.

---

## Resume

```bash
git checkout feat/pr4
git status -sb          # should be clean, ahead of origin/feat/pr4 by 5 commits

# 1. Run targeted unit test suite:
scripts/venue 'zig build --cache-dir /build/zig-cache-v2 test -Dtest-filter="busy F10" -Dtest-filter="drainBarrier" -Dtest-filter="readAfterEio" --summary line'

# 2. Add fidelity row F10 in src/ops/nvmet/fidelity.zig and verify:
scripts/venue 'zig build nvmet-smoke -Dnvmet-filter="F10" --summary all'

# 3. Full venue gate before landing on dev:
scripts/venue 'timeout 3900 zig build --cache-dir /build/zig-cache-v2 gate --summary all'

# 4. Fast-forward merge onto dev:
git checkout dev
git merge --ff-only feat/pr4
```

## Venue and Rules
- Local venue on Linux ≥ 7.3 (`SQZ_VENUE=local`).
- Remember: `scripts/venue` and git commands that alter `.git` require `BypassSandbox: true` because `.git` and host `/build` are outside the standard sandbox.
- Branch from `dev`; conventional commits; fast-forward merges only.
