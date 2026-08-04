# 2026-08-04 — Derivation-debt audit: no bare numeric default without a derivation or an on-line reason

**Branch:** `audit/derivation-debt` off dev `7cfa7ce4` (post-derivation-sweep, post-population-fix HEAD `e1076bfc`).
**Ruling (user, 2026-08-04, verbatim):** "nothing should be a hard coded
set number maybe a percentage or calculation but never just 8..."
**Relation to the derivation sweep** (`.benchmarks/2026-08-04-derivation-sweep.md`):
the sweep converted the class-A caps and FILED the measured constants.
This audit applies the STRICTER reading to what remains: every numeric
default at its DEFINITION SITE must carry (A) a real derivation, or (B) a
documented physical/format/kernel/measured reason **on the line** — and a
number that *claims* a derivation whose input has no definition site
(`300_000 / 10` "= TTI ÷ 10" beside a bare `Duration::from_secs(300)`) is
a **C violation**, not a B.

**Grading key**
- **A** — derived (budget/cores/geometry/measured-bandwidth fraction; tie test).
- **B** — fixed, WITH a compliant on-line reason (physical / format / kernel-ABI / measured-with-citation / never-regress floor / time-horizon-not-capacity).
- **C** — violation: bare number, no reason, or a stale/circular/duplicated reason. All fixed in this audit.

---

## 1. The C list (violations found → fixed)

| # | Site (at base) | Violation | Fix (commit `fix(derivation): …`) | Tie test |
|---|---|---|---|---|
| C-1 | `src/routing.rs:643` `EPOCH_IDLE_HORIZON_MS = 300_000 / 10` + `src/routing.rs:5667` `.time_to_idle(Duration::from_secs(300))` | The horizon CLAIMS "metadata cache's TTI ÷ 10" while spelling 300 000 itself; the TTI it claims to derive from was a second, independent bare 300 in the cache builder — a derivation with no input definition site (the circular-reason class) | ONE definition: `pub routing::METADATA_CACHE_TTI_SECS = 300` (documented time-horizon class, deliberately separate from `DAEMON_CACHE_TTL_SECS` — distinct cache surfaces); builder and `EPOCH_IDLE_HORIZON_MS = TTI × 1000 / 10` both read it | `metadata_cache_tti_has_one_definition_and_derives_the_idle_horizon` |
| C-2 | `src/mem_budget.rs:770` `ipc_session_population_target` `cpus × 8` | The ×8 is honestly documented (the EXA canon qd slope, measured refusal ×2) but the `8` had THREE definition sites: this literal, `tests/fio/run_fio_row.sh`'s `IODEPTH="8"`, and the four `exa_client_perf.sh` battery rows — the exact "never just 8" shape the ruling names | `pub mem_budget::EXA_CANON_QD = 8` — ONE definition, documented as an EXTERNAL instrument fact (shape parity with the DDN exa-client kit's dims — an instrument's wire format, not tuning); the population slope multiplies by it; the tie test PARSES both fio canon files and reds on drift | `exa_canon_qd_has_one_definition_tied_to_the_fio_canon` |
| C-3 | `src/zcrx_lane/probe.rs:84-88` `(cpus/8).clamp(1,8)` io_queues / `(cpus*2).clamp(4,64)` depth | Cited "design §8" — whose ACTUAL derivation differs (§8's queue ceiling is `nic_queues/4`; §8's depth is BDP-derived): a stale citation over bare literals. Bonus finding: sized from the calling thread's `available_parallelism()` — the Hang-1 pinned-first-toucher poison the sweep fixed in `uring_fs` (A10) but missed here | Extracted pure `probe::lane_geometry(cpus)` reading `crate::cpu::process_parallelism()`; every literal now has its honest reason ON THE LINE: floor 1 = physical minimum; ceiling = named `LANE_IO_QUEUES_MAX = 8` (pre-steering want rail bounding pinned area RAM at 8 × 64 × 1 MiB = 512 MiB/device until steering's NIC-derived `nic_queues/4` — `lane_queue_picks` — can see the device; the largest Z2/Z3-exercised geometry); depth slope `cpus × 2` documented as the INTERIM stand-in for §8's unbuilt BDP probe (filed follow-on); floor 4 / cap 64 = §8's own `derived_inflight` clamp (floor = the `Q_DEPTH_FLOOR` never-regress class), MQES still clamping at connect | `zcrx_lane_geometry_is_pinned_on_canonical_shapes` |
| C-4 | `src/block_reclaim.rs:589-593` reclaim knob defaults 64 / 2 / 4096 / 32 / 1000 | Measured constants (correctly filed by the sweep) whose citations lived in the module doc (lanes) or nowhere (batch, queue cap, park bound) — bare at the definition site | Citations moved ONTO the lines: batch 64/2 ms = the async-reclaim coalesce shape (`.benchmarks/2026-07-27-async-block-reclaim.md`); 4096 = the write-wall rows' deferred thin-space budget with the filed capacity-derivation thesis named; 32 lanes = the width experiment verdict; 1 s park = liveness horizon. No behavior change | — (doc-only; the values were already pinned by `block_reclaim` suites) |
| C-5 | `src/zcrx_lane/probe.rs:122` `LANE_MAX_XFER_CAP_BYTES = 1024 * 1024` | The doc SAYS it is "the FUSE transport's payload face" — i.e. it restates fuse3's `PAYLOAD_BASE` as a second literal (the mirror-by-retyping class) | Defined FROM the one definition: `= fuse3::raw::connection::fuse_over_uring::PAYLOAD_BASE as u32`; the Z2 MDTS-face measured citation stays; the value pin makes a fuse3 payload-floor change surface here as a conscious lane re-derivation | `zcrx_lane_xfer_cap_is_the_transport_payload_face` |
| C-6 | `src/routing.rs:6156` + `:6290` `<= 256 * 1024` and `src/read_lane.rs:104` `READ_LANE_MIN_FILL_BYTES = 256 * 1024` | The read path's ONE size-class boundary (R1b always-admit arm, R4 RAM-LRU-vs-hot split, hold participation floor) as three retyped literals — read_lane's own doc said "mirrored", which is how the drift class is born | `pub routing::READ_SIZE_CLASS_BOUNDARY_BYTES = 256 KiB` (documented as a MEASURED read-path-program constant — the `READ_RANGED_THRESHOLD`/NT-floor 256 KiB class, re-bracket on the fio-gap venue to move it); both routing sites and `READ_LANE_MIN_FILL_BYTES` read it | `read_size_class_boundary_has_one_definition` |
| C-7 | `src/uring_fs.rs:39` `URING_FS_QUEUE_CAP = 4096` | The one bare const in a file where every neighbor carries its doc line | On-line reason: backpressure-not-custody bound (P1-6 class, the `nvme_dev::URING_REQ_QUEUE_CAP` posture), sweep-filed with its saturation-sweep thesis | — (doc-only) |
| C-8 | `src/routing.rs:5646` `max(10_000, total_memory / 200_000)` + `:5670` `stream_lanes` 100 000 / 30 s | RAM-derived capacity whose denominator had no meaning on the line; a leak-rail cap and idle TTL with no stated class | On-line reasons: one layout-cache entry per 200 KB of physical RAM (entry ≈ hundreds of B–few KiB ⇒ worst case ≪ 1 % of RAM), floor 10 k = shipped small-box posture, and WHY physical RAM not the R5 budget (constructor can run pre-resolution; moka self-evicts, R5 gauges separately); stream_lanes 100 k = leak rail over tiny lane structs (never a working-set budget), 30 s = stream-idle time horizon | — (doc-only) |

**Every C is fixed on this branch.** C-1/2/3/5/6 red-first (commit
`93dea175`, E0425/E0432/E0603 at base); C-4/7/8 are citation/doc moves
with no behavioral surface (the values were already suite-pinned).

## 2. The stricter-reading re-examination (the named list)

| Item | Verdict |
|---|---|
| `ipc_session_population_target` ×8 | **C-2 → A-tied.** The more honest derivation the ruling asked about EXISTS: the 8 is the EXA canon qd, and it now has ONE definition (`EXA_CANON_QD`) tie-tested against the fio canon files themselves. |
| `ctl_conn_cap_from` ×2 (`src/ipc_host.rs:249`) | **B.** Structural reason on the line: a connection exists to become a session; ×2 = every admitted session's held connection + a handshake in flight, plus the ADMIN lane; floor 32 = administrable-when-shed-to-zero; rail 4096 = OS-thread exhaustion rail. Derivation input (`arena_cap_bytes / session_footprint`) is budget-derived. |
| `il_sessions_default` `clamp(cpus/4, 2, 16)` | **A/B.** Slope = measured drain-thread saturation (2026-07-19 sweep); floor 2 documented (single-session serialization plateau); ceiling DERIVED from the structural binder `SESSION_REGISTRY_SLOTS/2` since the sweep — tie tests both sides. Nothing further. |
| zcrx probe geometry | **C-3 → fixed** (pure form + honest on-line reasons + process-mask sizing). The §8 BDP/NIC full derivation remains the FILED follow-on — building a link-speed × RTT probe now would be derivation theater ahead of the lane's own acceptance program. |
| `LANE_MAX_XFER_CAP_BYTES` 1 MiB | **C-5 → A-tied** to `PAYLOAD_BASE`. |
| Reclaim batch defaults | **C-4 → B** (citations on the lines). `RECLAIM_QUEUE_MAX_BLOCKS` keeps the sweep's filed derivation thesis (aggregate-capacity-derived thin-space debt) — conversion awaits its queue-cap sweep with `block_free_reclaim_cap_parks` as the verdict; inventing that derivation without the sweep would be theater. |
| `EPOCH_IDLE_HORIZON_MS` "TTI/10" | **Verified NOT actually tied — C-1 → fixed.** The division was real; the input was duplicated, not shared. |
| Virtual-gen registry cap 256 (`fuse_client.rs:101`) | **B.** The line documents the worst-case arithmetic (256 × field-measured ~72 KiB ≈ 18 MiB) AND why a fixed count is deliberate: a safety rail against a misbehaving (never-FORGETting) kernel, not a tuning knob. Compliant as written. |
| `Q_DEPTH_FLOOR` 4 / `PAYLOAD_BASE` / `MAX_BACKGROUND_FLOOR` 64 (fuse3) | **B.** Each documents the never-regress-below-shipped posture on the line; `Q_DEPTH_DESIRED = 32` keeps its measured L1 citation (316k) with the ≥ 96-CPU depth-sweep re-bracket filed. |
| Prefetch/read-lane constants | `READ_LANE_BUDGET_DIVISOR = 8` **B/A** (budget fraction, documented junior to the write pipeline's /4); `READ_LANE_MIN_FILL_BYTES` **C-6 → A-tied**; `derived_prefetch_window_cap` rails floor 4 / cap 4096 **B** (documented: AIMD start 2 cannot double once below 4; cap bounds a pathological budget/block ratio); `SHARE_PCT 50` / `FILL_PCT 5` **B** (percentage knobs — the directive's own preferred form); `FIFO_RECLAIM_SLACK 32` + `EVICTION_QUEUE_RECLAIM_SLACK 64` **B** (documented amortization floors, "not a resource cap — the bound that matters is the `2 × live` term, which scales"). |

## 3. The B census (fixed-with-reason — representative, by class)

Verified compliant at their definition sites this audit (beyond §2):

- **Wire/ABI/kernel facts:** fuse3 `abi.rs` FUSE opcodes/flags/struct sizes, kmbuf/zcrx uapi consts (`IORING_*`, `FUSE_URING_*`, `REQ_HEADER_SZ 288`), `IORING_OP_RECV_ZC = 58` (uapi ≥ 6.15, probed by value), `PATH_MAX_WITH_NUL`, `FUSE_NAME_MAX 255`, `FOPEN_*` bits, `PMD_SIZE 2 MiB`, `POOLED_BUF_ALIGN 4096`, LBA 4096 screens (`ipc_direct`, `placed_core::CLAIM_PAGE`), `nvmf` connect-data 1024 B.
- **On-disk format:** KV node/superblock geometry, journal ring min/max (B/C per the sweep's pushback §4 — format must stay portable to smaller mount hosts), staging/extent record headers + versions, `MAX_INLINE_SIZE 4096`, `CHUNK_SIZE 4 MiB`, routing width `2^16`, incompat-bit ledger budgets.
- **Structural binders:** `SESSION_REGISTRY_SLOTS 32` (alloc-free interposer static table; every il ceiling derives from it), `IPC_ARENA_DMA_ALIGN_BYTES 4 MiB` (slots × DMA LBA — the bounce-fix law), slot/ring layout consts.
- **Never-regress floors:** `IPC_ARENA_FLOOR_BYTES 64 MiB`, population floor 128 (= the retired /128 posture), parked 256, node-cache 512 MiB, batch floors 64 / 256 KiB, `MAX_BACKGROUND_FLOOR 64`, `Q_DEPTH_FLOOR 4`, uid-cap floor 64, `MIN_BLOCK_KEYS_PER_CALL` (documented PHYSICAL floor: 1 GiB span at the smallest 4 KiB geometry).
- **Measured-with-citation:** NT floors 256 KiB (DMA-destined 1 MiB-class chunks vs latency-bound small writes — on the line), `REAP_EVENT_PARK_MAX 2` (op-economy Phase B counted retune), `WAIT_SPINS_* 4096/256`, ranged threshold 256 KiB, `RANGED_BUF_SIZE 64 KiB` (ipc-miss-path fix, on-line), `SHARD_REPLACE_SLACK`, hot-tier min-shard, `FOLD_MAX_EXTENTS 64` (amortization trigger, `fold_fill ≥ 16` gate), il/service `cpus/4` slope, write-pipeline `FLOOR_BLOCKS_PER_LANE 8`/`HEADROOM 3`/`BUDGET_CAP_DIVISOR 4`/`WINDOW_MS 250` + probe consts (probe-up governor — the depth itself fully derived).
- **Time horizons (not capacity):** kernel TTLs 1 s, `DAEMON_CACHE_TTL_SECS`/`METADATA_CACHE_TTI_SECS` 300 s, heartbeat 10/45 (the ONE staleness law — membership TTL genuinely ties to `CLIENT_STALE_TTL_SECS`, verified), `NONCE_TTL 60 s`, `CTL_HANDSHAKE_TIMEOUT 10 s` (documented: matched to the shim's `CTL_RECV_TIMEOUT`), `SERVICE_PARK_MAX 5 ms` (the D18 ladder cap), `IDLE_CONFIRM_TICKS 20`, watchdog 5 s ticks, fsck settle 2 s.
- **Backpressure bounds (small structs, custody elsewhere; sweep-filed with sweep theses):** `URING_REQ_QUEUE_CAP 4096` (P1-6, on the line), `URING_FS_QUEUE_CAP 4096` (now on the line — C-7), `RING_ENTRIES 512` (documented ≫ observed governed depth), `ADMIT_CAP 256`, `WRITEBACK_QUEUE_CAP 4096`, `STAGING_MERGE_QUEUE_CAP 1024`, `EVICT_CHANNEL_BYTE_BOUND 256 MiB` (R5-registered since RES-4).
- **Job-fabric constants (maintenance plane, all documented on the line):** `CHECKPOINT_TASKS 256`/`CHECKPOINT_SECS 5` (bounded crash re-work), `MOVER_PIPELINE_WIDTH 4` (round-trip overlap vs R5 copy budget — G-VL-3 b), `EVACUATE_INFLIGHT_WINDOW_BLOCKS 64`, `DRAIN_HEADROOM_FLOOR_BYTES 1 GiB`, `DRAIN_FLOOR_RATE 512 MiB/s` (G-VL-3(b)-derived, replaced live by the measured job rate), `REBALANCE_DEFAULT_THROTTLE_PCT 25` (KD-12 conservative default), `DEFRAG_MAX_PASSES 16` (termination belt).
- **Diagnostic/contention geometry (RAM-trivial, sweep-filed):** `BLOCK_LOCK_STRIPES 4096`, `SHARDED_ATOMIC_STRIPES 64`, op-profile rings 256/512/1024, phase-table dims, `ERROR_SAMPLES 3`, readdir pages 4096/1024 (documented kernel-dirent-buffer reasoning), `DIR_ENTRY_CACHE_MAX_ENTRIES 10_000` (documented ~60 MB 1 M-entry arithmetic).

## 4. Graded B that a stricter ruling should revisit (flagged, unchanged)

1. **`placed_sever::assembly_cap_bytes` — `min(budget/8, 2 GiB)`**
   (`src/placed_sever.rs:51`). The SAME `min(fraction, 2 GiB)` shape the
   sweep deleted as class A at the transport and ipc caps. Kept B here
   because the line documents a real difference: assemblies are claimed
   to be STRUCTURALLY bounded by in-flight ring ops (≤ one block per
   live claim) and the rail's primary role is the pre-arm budget==0
   fallback. But on the 176 GiB field budget the armed arm clamps
   22 GiB → 2 GiB; if a placed-sever row ever shows `placed_assembly_bytes`
   pinned at the rail with `sever_fallbacks` growing, this is the A1/A2
   conversion re-run (delete the rail, trust the structural bound + R5).
2. **`bench.rs` auto-sizing consts** (`AUTO_THREADS_CAP 16`,
   `AUTO_*_BYTES`): instrument-side, not product path — but the 16-thread
   cap predates big-core boxes; a bench-fidelity pass could derive it
   from `process_parallelism` like the fio canon's njobs=nproc law.
3. **Pool sizing from `std::thread::available_parallelism()`**
   (`cache/pool.rs` ×3, `cache/lru.rs` shard count): the value shapes are
   fine (`max(cores × 16, 64)`, `max(next_pow2(cores), 16)`) but the
   SOURCE is the calling thread's mask — the Hang-1 pinned-first-toucher
   class fixed in `uring_fs` (A10) and `probe.rs` (C-3). These Lazys are
   plausibly first-touched from pinned runtime workers; migrating them to
   `crate::cpu::process_parallelism()` is a one-line-each follow-on that
   needs its own suite pass (pool geometry feeds fixed-buffer
   registration), so it is FILED rather than batched here.
4. **`SQUEEZEFS_FUSE_IO_URING_ENTRIES` default 1024** (classical
   INIT/notify rings only): harmless, but the only remaining fixed ring
   depth an operator sees in the registry; the sweep filed it C with "not
   the hot path".
5. **zcrx §8 full derivation** (BDP depth + NIC-derived queue want): the
   C-3 fix documents the interim honestly; the lane's acceptance program
   owns the real derivation.

## 5. The ruling's direct question

**"Does any surface still contain a bare `8` (or any bare small integer)
without a definition-site reason?"** — On the audited surfaces (the full
env-knob registry + fuse_client, routing, block_reclaim, mem_budget,
ipc_host/service/direct, write_pipeline(+core), read_lane, zcrx_lane,
nvme_dev, uring_fs, cache/, tiering/, bg_admit, jobs, placed_sever/core,
nt_copy, squeezefs-ipc sizing/layout/ring/slot cores, squeezefs-preload
session/interpose/aio_glue, fuse3 transport geometry): **no.** Every
remaining small integer at its definition site now either derives (A),
carries its physical/format/kernel/measured/never-regress/time-horizon
reason on the line (B), or was fixed in §1. The nearest residues are the
§4 flags: the `available_parallelism()` SOURCE question (values fine,
mask wrong in a corner), and the two `min(fraction, rail)` shapes
(`placed_sever` 2 GiB, `SeveredPool`-pattern rails) whose rails are
documented but displaceable by measurement.

## 6. Verification (ruling D12 — targeted, no full gate)

| Gate | Result |
|---|---|
| Red at base | `93dea175`'s five tie tests: E0425/E0432/E0603 (missing consts/fns) |
| `cargo test --test derivation_sweep_tests` | 20/20 green (15 sweep + 5 audit) |
| Touched suites (`--test-threads=1`, the house convention): `zcrx_lane_tests` 40/40, `read_lane_tests` 10/10, `mem_budget_tests` 16/16, `read_tier_admission_tests` 10/10, `read_admission_governor_tests` 2/2, `async_block_reclaim_tests` 17/17, `block_free_reclaim_tests` 6/6, `reclaim_batch_tests` 6/6, `ingest_economy_tests` 4/4, `ipc_op_economy_tests` 3/3 | green. (A parallel-threads `read_lane_tests` run flakes 3–4 tests IDENTICALLY at base `e1076bfc` — pre-existing process-global-state interference, which is why the gate runs `--test-threads=1`.) |
| `cargo clippy --all-targets --all-features -- -D warnings` | clean |
| `cargo clippy --all-targets -- -D warnings` (shipped config) | clean |
| `cargo fmt --check` | clean |

**AGENTS.md:** no edit — the law's wording already prefers honest
measured constants over derivation theater ("floors are legitimate only
as physical minima or the never-regress-below-shipped posture"; "fixed
clamps require a documented physical/format/kernel/measured reason on
the line"), which is exactly the grading this audit applied.
