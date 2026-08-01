# 2026-08-02 — Derived `ipc_session_arenas` cap: the fixed 2 GiB ceiling deleted

**Branch:** `fix/ipc-arena-cap-derivation` off dev `1b936bf`.
**Ruling (user, 2026-08-01, verbatim intent):** "`SQUEEZEFS_IPC_MEM_MAX=8192`
looks like a hardcoded default... we were trying to stay away from things
like that and having it calculated based on the system. maybe a percentage
is better than a set value, or the ability for both with a default as a
percentage." This closes the 2026-08-01 rewrite-publish-drain rider §8
charter note ("the ceiling deserves a derivation").

## 1. The law change

Old: `ipc_arena_cap(budget) = min(budget/8, IPC_ARENA_CAP_CEILING = 2 GiB)`
(`src/mem_budget.rs`), env-overridable only as the absolute
`SQUEEZEFS_IPC_MEM_MAX` (MiB).

New (`mem_budget::resolve_ipc_arena_cap`, pure — the `resolve_budget_from`
pattern; plumbed at the single mount-side call in `src/fuse_client.rs`):

| Precedence | Form | Semantics |
|---|---|---|
| 1 (highest) | `SQUEEZEFS_IPC_MEM_MAX` (MiB) | absolute, **explicit-wins-verbatim** (incl. 0) — compat spelling, semantics unchanged |
| 2 | `SQUEEZEFS_IPC_MEM_PCT` (percent of the resolved R5 budget) | **the preferred spelling**; clamp (0,100] — >100 clamps to 100 with a warning; garbage / ≤0 / non-finite warns and falls through (knob-family convention: a bad env string never fails a mount) |
| 3 (default) | derived | `budget/8` = **12.5 %** (`IPC_ARENA_CAP_DEFAULT_PCT`) of the R5 memory budget — **no absolute ceiling** |

**Why the ceiling is deleted rather than re-derived:** the budget/8 fraction
was already machine-derived (the R5 budget resolves flag → env → cgroup
`memory.max`×0.8 → 70 % RAM), so the fraction is scale-free on every box
size. The pool is bounded without a constant: per-uid session caps + idle
reap (`SQUEEZEFS_IPC_IDLE_SECS`) bound the population, and R5 Red shedding
refuses new sessions under pressure (shed = refuse-new + reap-idle, never
tearing live sessions). A second absolute bound duplicated R5's job with a
number — and in the field it refused legitimate work (below).

Contracts (red-first, observed red at base `1b936bf`):
`tests/ipc_host_tests.rs::ipc_arena_cap_is_budget_fraction_no_fixed_ceiling`
(rewrites the retired `ipc_arena_cap_mirrors_transport_payload_cap_shape`
pin; red at base: `2147483648 != 21962500000` — the ceiling doing exactly
the field harm), `…_resolution_precedence_absolute_pct_default` and
`…_pct_clamps_and_ignores_garbage` (red at base: E0432, no
`resolve_ipc_arena_cap`). Green post-fix; suites `ipc_host_tests` (23),
`ipc_op_economy_tests` (3), `mem_budget_tests` (16),
`preload_session_tests` (24) all pass `--test-threads=1`; clippy
`--all-targets --all-features -D warnings` + fmt clean.

## 2. Derivation table across box sizes

Default posture (no flag/env/cgroup ⇒ budget = 70 % RAM); session
footprint ≈ 64.3 MiB (64 MiB default arena + ring/slots — the rider's
measured figure). "Sessions" = admission headroom at that footprint.

| Box RAM | R5 budget | OLD cap (min(b/8, 2 GiB)) | OLD sessions | NEW cap (b/8) | NEW sessions |
|---|---|---|---|---|---|
| 8 GiB | 5.6 GiB | 716.8 MiB | ~11 | 716.8 MiB | ~11 (unchanged) |
| 16 GiB | 11.2 GiB | 1.4 GiB | ~22 | 1.4 GiB | ~22 (unchanged) |
| 32 GiB | 22.4 GiB | **2 GiB (ceiling)** | ~31 | 2.8 GiB | ~44 |
| 64 GiB | 44.8 GiB | 2 GiB | ~31 | 5.6 GiB | ~89 |
| 128 GiB | 89.6 GiB | 2 GiB | ~31 | 11.2 GiB | ~178 |
| **251 GiB (field client)** | **176 GiB** | **2 GiB** | **~31** | **22 GiB** | **~350** |
| 1 TiB | 716.8 GiB | 2 GiB | ~31 | 89.6 GiB | ~1,427 |

Boxes at/below ~23 GiB budget (≈ 32 GiB RAM at default resolution) are
byte-identical to the old law — the ceiling never bound there. The field
shape that motivated the ruling: 251 GiB client, 32-process psync fleet at
`il_sessions_default = clamp(32/4,2,16) = 8` ⇒ ~48 HELLOs ≈ 3.02 GiB
demand; old cap 2 GiB ⇒ 13/48 refused (`ipc_bind_refused_budget`),
engagement 0.663 = INVALID il rows, forcing the `SQUEEZEFS_IPC_MEM_MAX=8192`
posture lever. New default cap 22 GiB admits the fleet with ~7× headroom —
no lever. (The per-uid session cap, 64, remains the population bound the
rider verified was NOT the binder.)

## 3. The transport sibling (`transport_buffer_cap`) — evaluated, FILED, not fixed

`transport_buffer_cap(budget) = min(budget/8, TRANSPORT_BUFFER_CAP_CEILING
= 2 GiB)` has the identical arithmetic shape. Analysis of whether the fixed
ceiling harms big boxes there:

- **Failure mode differs in class.** The IPC cap **refuses admission**
  (hard functional harm: sessions bounce, engagement-INVALID rows). The
  transport cap **degrades gracefully**: `TransportGeometry::plan` clamps
  per-queue depth `cap/(nqueues × payload_sz)` into [4, 32] — it never
  refuses work, and the floor (depth 4) is by construction the pre-L1
  shipped posture ("never a regression").
- **Where the ceiling binds.** payload_sz = max(max_write, 8 KiB,
  256 pages) = 1 MiB typical; desired depth 32 fits under 2 GiB up to
  **64 possible CPUs exactly** (64×32×1 MiB = 2 GiB). Depth at the
  ceiling: 96 CPUs → 21, 128 → 16, 192 → 10, 256 → 8, 512 → 4. The
  standing field client is **32 possible CPUs** (`transport_queues=32` in
  the live journal): 32×32×1 MiB = 1 GiB < 2 GiB — **the ceiling is not
  binding on the current field venue at all.**
- **Entangled with measured L1 evidence.** `max_background` is clamped to
  the measured ceiling 256 (`MAX_BACKGROUND_CEILING` — "raising this
  requires new evidence, not a bigger constant"). At 128 CPUs, lifting the
  payload ceiling to restore depth 32 would pin 4 GiB of registered
  buffers while delivered background concurrency stays kernel-gated at
  256 (128×16 = 2048 already clamps) — the win is unproven; the
  44k→316k L1 decomposition was measured on a ≤64-CPU box.
- **Structural blast radius.** The constant is duplicated in the fuse3
  fork (`crates/fuse3/…/fuse_over_uring.rs::TRANSPORT_BUFFER_CAP_CEILING`,
  the embedder-fallback `default_buffer_cap`), and the depth policy is the
  documented AGENTS.md FUSE-uring-knobs law with its own pinned tests
  (`tests/mem_budget_tests.rs::transport_buffer_cap_is_budget_eighth_ceilinged`,
  `tests/transport_concurrency_tests.rs`). Changing it is a transport
  geometry change.

**Verdict: NOT identical in class — filed as a follow-up, not blind-fixed.**
Suggested follow-up scope (needs its own field window on a >64-possible-CPU
box): A/B depth-32-vs-degraded at 96–256 CPUs with `max_background` held at
256, plus a re-derivation proposal (e.g. percentage with a
queues×desired-depth×payload demand cap instead of a byte constant).
Transport tests and both ceiling constants left untouched here.

## 4. Field leg — DEFERRED (baseline window still open)

Coordination check at 2026-08-01T~15:02Z: `/scratch/tmp/agent_runs.log`
(cluster client, read-only over ssh) shows the sibling `rcc agent:`
baseline session **still active** — fio rows (`rdk8-*`/`rdk32-*` gap-probe
reads) and standing-mount deploy swaps at 1b936bf as of 15:02:14Z, **no
SESSION END journaled**. Per the campaign's hard rule the cluster was not
touched: no deploy, no mount changes, no journal entries written.

**The field spot-check rides the next window** (default posture, no
lever): remount the pair at this branch, one il row, assert **48/48
sessions admitted**, `ipc_bind_refused_budget` **delta 0**, engagement
**≥ 0.90**; restore the standing pair after; journal SESSION START/END.
Expected arithmetic on that venue: budget 176 GiB ⇒ derived cap 22 GiB ⇒
48 × 64.3 MiB ≈ 3.02 GiB demand admits with ~7× headroom.

## 5. Verification summary

| Gate | Result |
|---|---|
| Red at base (behavioral) | law test FAILED `2147483648 != 21962500000` at `1b936bf` |
| Red at base (API) | E0432 `resolve_ipc_arena_cap` ×2 |
| Green post-fix | 3/3 new cap tests |
| `cargo clippy --all-targets --all-features -- -D warnings` | clean |
| `cargo fmt --check` | clean |
| `ipc_host_tests` / `ipc_op_economy_tests` / `mem_budget_tests` / `preload_session_tests` (`--test-threads=1`) | 23 / 3 / 16 / 24 — all pass |
| `cargo doc --no-deps` | clean |
| Field spot-check | deferred to the next window (§4) |
