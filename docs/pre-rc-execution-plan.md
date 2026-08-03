# Pre-RC Execution Plan — Rev 3 (full scope)

**Input:** `docs/pre-rc-engineering-spec.md` (Rev 3; item census from the Rev 2 passes at `dev` @ `68e8474`, status deltas verified at `391dec2`) — 17 P0 / 46 P1 / 61 P2 / 38 P3, plus the §6.9 DLM staging plan S0–S11 and the §9 PERF board.
**Rev 3 delta:** the 2026-08-04 merge train (`68e8474..391dec2`, five campaigns) landed after the census — Phase 0.4 re-anchors against the new tip and sweeps the ~2,500 unreviewed merged lines; the spec's Rev 3 addendum records the item-status deltas (ENG-15 partial, TEST-8 wider, FUSE-1 escalated). One new ruling **D6** below (reset-v5 window vs the one-window rule, forced by the verified FUSE-1 × patch-0027 collision — **ruled: option (a)**, FUSE-1 pulled forward, kernel patch set frozen).
**Scope ruling (user):** **nothing is explicitly out of scope for this RC.** All 162 items, the full DLM program through S11, the full PERF board, and the zcrx lane are in the program. Items may still finish as a *written adjudication* where a fix is genuinely wrong to ship, but every item gets an explicit disposition in the RC manifest — none are silently deferred.
**Governing workflow:** AGENTS.md TDD cycle (tests first, red-first repros, tiered gates, branches off `dev`, `--ff-only` merges, repro-port mandate, multi-run discipline, two-substrate rule, sustained-state rows).

**Spot-verified against source before planning:** ENG-1 (clippy::all at both crate roots), ENG-7 (no CI), the never-incremented `lease_acquire_*` counters, DUR-2 (no Fsync op in `nvme_dev.rs`), TEST-1 (power-cut harness used only by its own self-test + one crash-contract test). All hold at `68e8474`.

---

## 0. Decisions recorded (user rulings, this session)

| # | Decision | Ruling | Plan consequence |
|---|---|---|---|
| D1 | Meaning of "15,000+ concurrent nodes" | **15 k nodes, all consistently reading AND writing, but rarely (maybe never) the same files — @-scale AI-training mixed workloads.** 15 k real concurrent writers cannot be tested directly. | The product claim is full multi-writer with low same-file overlap. This is the *best possible* fit for the §6.7 delegation-heavy design (uncontended acquire = zero network ops). Validation is three-legged: real measured multi-mount at achievable scale, a **simulated-client harness** for the 15 k membership/lease/revoke planes (the spec's R6 pattern: 1,875 simulated clients — extend to 15 k), and published scaling arithmetic tied to measured per-shard numbers. The docs claim exactly what the evidence shows, per tier. |
| D2 | Job-wire posture | **Configurable, default bind `0.0.0.0`; peers auto-discovered, never manually configured.** | Since the listener is open by default, the VAL-6 security gaps become non-negotiable P0s: zero-config mutual authentication derived from storage trust (the `job:enroll` meta-KV secret — possession of volume access = cluster membership), never an accept-everything verifier. New work item **DISC-1**: auto-discovery of squeezefs peers via the `client:{uuid}` records on the shared metadata volume (they already carry the job-wire endpoint). Discovery + authn design lands in S3 `cluster_wire`. |
| D3 | rsa RUSTSEC-2023-0071 | **Replace the primitive.** | VAL-3's key-handling redesign folds in a move off RSA key-wrap (X25519/HPKE-style or AES-KW under a KDF). On-disk format change rides the Phase-8 reformat window with its own incompat bit. `rsa` crate exits the tree; the advisory disappears rather than being adjudicated. |
| D4 | DUR-7 cross-volume atomicity | **Distributed transaction, not refusal.** | Build the intent-record/compensation machinery once, early in the DLM program (it is also what S8 function-shipped metadata needs for cross-volume ops); DUR-7 is its first consumer. Because DUR-7 is P0, the tx machinery is scheduled right after S2/S3 rather than waiting for S8. |
| D5 | zcrx read lane | **Default-on in the RC**, after MEM-3 (cancellation safety) + TEST-6 (test coverage) + Z3 (gather fusion), gated by the engagement laws (`gather ≡ fill`, `zcrx_lane_poisoned == 0`) and a sustained ≥ 60 s row. | PERF-1 graduates from opportunistic to a committed deliverable with a hard gate chain. |
| **D6** | Reset-v5 window vs the one-window rule, and the bit-62 disengagement | **RULED (2026-08-02): option (a) — mainline-correct, no new kernel patches.** User rationale verbatim: *"whatever doesn't require more kernel patches/maintenance, we already have some of that. lets make sure we are main-line correct."* Background: the staged reset-v5 window's generic/634-on-live-bit-62 row is structurally disengaged as staged — patch 0027 consumes bit 62 from the folded flags2 word, which the kernel discards against the fork's minor-31 / no-`FUSE_INIT_EXT` reply (spec FUSE-1, Rev 3 escalation — verified against the patch and the fork). The rejected alternatives (kernel-side unconditional fold, or a narrow private-bit read from `arg->flags2`) are recorded as NOT chosen: both mask a daemon defect FUSE-1 must close anyway (E1), ship protocol divergence into a fork kernel, and invalidate the staged/sha-verified v2 RPMs. The narrow variant remains the evidence-gated fallback ONLY if the FUSE-1 A/B surfaces a genuine minor-36 obstacle. | **FUSE-1 is pulled forward as the next work item** (ahead of its Phase-3 slot) — **DONE 2026-08-02** (`fix/fuse-init-ext`: `53b887d` red → `3049b6d` green, 31→36 audit empty; evidence `.benchmarks/2026-08-02-fuse1-init-ext.md`): the live leg ran on the REAL cluster (user directive: cluster_reset scripts, zero local files — `cluster_reset_v4.sh` first execution, two first-run fixes on `fix/cluster-reset-ssh-quoting`), fold + transport armed on the v1 sqz kernel (kmbuf-bufring, 32×32), A-B-B-A parity within noise vs the `391dec2` control, v1 timestamp "before" face recorded. NOTE the bit-62 CORRECTION vs this row's original text: patch 0027 is v2-only, so the full bit-62 engagement (`utimensat` clamped incore + generic/634 PASS) is the WINDOW's row after the v2 boot — the pre-window leg proves the fold and parity, which is what makes that row honest. Remaining: merge `fix/fuse-init-ext` + `fix/cluster-reset-ssh-quoting` to dev before the window opens (the window pair inherits FUSE-1; staged v2 RPMs stay valid; one-window rule survives). |

| **D7** | Execution priority (2026-08-02) | **Performance and the DLM program run as parallel tracks, ahead of everything else.** User rulings verbatim: *"I want to get our performance as optimized as we possibly can"* and *"outside of performance single most important thing IS the DISTRIBUTED LOCK MANAGER SO WE CAN HAVE MORE THAN ONE CLIENT AT A TIME."* Test-suite gates (the from-zero full suite, the three-suite release gate, the reset-v5 window) are DEFERRED — not cancelled — until these tracks land what they can; per-branch targeted tests and counted A/B brackets remain in force (perf work is invalid without its bracket; red-first stays the law for DLM stages). | Track 1 (DLM, the long pole): §6.11 repro → S0 → S1 → S2 → S3/S3.5 → **S4 solo gate** → **S5 read-only coherent mounts = the first multi-client ship**, then S6+. Track 2 (PERF board, magnitude × confidence): PERF-6 → PERF-2/5 → PERF-7 → MEM-3→Z3 (PERF-1), then PERF-4/13/16. Cluster brackets serialize through the orchestrator (one mount, one venue); the window and v2 boot stay parked (D6 sequencing unchanged when they resume). |

---

## 0b. Execution log — the parallel-agent program (2026-08-02)

Per D7, work runs as **file-partitioned parallel agents** under one law: red-first TDD (tests commit before implementation), a criterion microbench on every touched hot path, targeted gates only (no full suites — deferred), no cluster access (a live mount holds the reset-v5 venue), and the orchestrator does every merge. **D11 (2026-08-03) tightened the gate clause further**: for the remainder of the DLM push, agents write their tests and benches but do not *run* suites, measured benches, or brackets — build-level verification only (`check`/`clippy`/`fmt`) — and the entire deferred stack runs once, from zero, when N readers + N writers work. Disposition tracking moved to **`docs/rc-manifest.md`** (exit criterion E14, started now so it accumulates rather than requiring archaeology later).

Wave 1: DLM S0/S1 + §6.11 · PERF-2/5/6 · PERF-7 · MEM-3 + Z3 · MEM-1 + PERF-4 · MEM-2 · ENG-1/5/6/9/12/13/14 + MEM-6 · ENG-3/4.
Wave 2: VAL-1/2 · VAL-4/5 · VAL-6 · POSIX-1/2/3/4.

**Serialization the partition forces** (next wave, as owners release files): TEST-1 → DUR-2 → DUR-1 needs `nvme_dev.rs` (PERF-D); FUSE-2 ⊕ PERF-16 needs `fuse_over_uring.rs` (PERF-A); DUR-3..8 need TEST-1; VAL-3 + KW-1 need `config_ops.rs` (ENG-OPS); ENG-2 needs ENG-5's dependency deletions to be meaningful.

| **D8** | Concurrency target (2026-08-02, supersedes the D1 reading) | **N coherent WRITERS + N coherent readers, including concurrent writers to different regions of the SAME large file.** User statement verbatim: *"a file especially a large one could be getting read/written to different blocks by different applications and want locks on them."* | This is spec §6.9 **S9 + S11** (multi-writer data plane + byte-range custody), not S5. Scope clarification recorded: multiple applications on ONE mount already write disjoint blocks of one file concurrently (per-inode locking is meta-prep only on the striped path; block work rides per-block flush locks) — the ask is **cross-node**. S5 is NOT a detour: its two hard parts (node-cache revalidation, freed-offset grace period) are exactly what S9 requires underneath, and §6.3 says the grace period *is* the multi-writer prerequisite. The API already anticipates this — `acquire_lock` takes `range: Option<(u64,u64)>` — but there is **no range conflict logic**: ranges fold into the file's lock entry today. |
| **D9** | The multi-writer format work vs the reformat window | **RULED: it joins the existing batched window.** No separate window. | Every §6.2 durable-format change (durable block refcounts, per-writer journal rings, partitioned bitmap/ledger, per-writer ino cursors, `offset ‖ incarnation` block keys) lands behind incompat bits that are **built but NOT stamped**, exactly as KW-1 and DLM bit 7 were handled this wave. They stamp together in Phase 8 with the sharded indirect map, the AEAD AAD binding, and bit 6's field validation. |
| **D10** | R1 — function-shipped metadata's serial-latency risk | **ACCEPTED.** | S8 may take serial operations from ~9,100/s to 6.7–20 k/s before owner queueing at 50–150 µs RTT (`tar -x`, `make`, `rsync` are serial streams). Per §6.10 R1 the `tar -x` A/B is **published even if it regresses**, and S10 (subtree delegation + client-owned-slot placement) is the recovery. S10 does not gate S8's landing. |
| **D11** | Verification cadence during the DLM push (2026-08-03, sharpens D7) | **RULED, verbatim: *"no cargo test, benchs or release gate yet until we are done implementing the DLM and can have N Readers and Writers."*** | The deferral is now explicit and total for three classes: (1) **no suite runs** — not the from-zero `cargo test --all-features`, not `task check`'s test line, not the require-mount leg; (2) **no measured benches** — no criterion measurement rows, no `run_bench_baseline.sh` compare or `save`, no A/B brackets, and bench *smoke* only when a bench file itself changed shape; (3) **no release gate** — the three external suites, the scoreboard, and the reset-v5 window stay parked. What REMAINS in force, because merging code that does not build is pure damage: `cargo check`, `cargo clippy --all-targets --all-features -- -D warnings`, `cargo fmt --check`, the markdown link check, and — per the 2026-08-03 follow-up ruling — **one targeted test file per branch** (`cargo test --test <this_branch_only>`), which is exactly enough to keep red-first TDD real: a stage sees its own tests fail before the implementation and pass after, and nothing else runs. The deferred stack then runs once, from zero, on the finished binary — which is also the honest way to run it: a suite green against half a DLM proves nothing about the other half. |

---

## 1. Definition of RC-ready (exit criteria)

| # | Criterion |
|---|---|
| E1 | All 17 P0 items closed, each with a red-first cargo repro landed with its fix |
| E2 | Crate-root clippy suppressions removed entirely; `-D warnings` green with zero allows (full burn-down, style included) |
| E3 | `cargo audit` in the gate and clean — `rsa` replaced (D3), the EOL rustls 0.21 stack gone (ENG-5) |
| E4 | CI runs `task check` + audit on every PR (ENG-7) |
| E5 | Data-device power-loss harness exists; per-layout durability matrix green (TEST-1 → DUR-2 → DUR-1) |
| E6 | Three-suite release gate from zero on the RC binary (TEST-8), flakes fixed first (TEST-7) |
| E7 | DLM S0–S11 landed with each stage's §6.9 gate met; S4 solo-mode indistinguishable from today (`dlm_rpcs == 0`, scoreboard within noise) |
| E8 | Multi-writer validation per D1: real multi-mount rows at achievable scale + 15 k simulated-client membership/lease/revoke rows + S7 stop/resume-past-TTL device-rejection leg + S9 write fan-out with amplification columns |
| E9 | Job wire secure-by-default at `0.0.0.0` (VAL-6 closed on cluster_wire) + DISC-1 auto-discovery working with zero manual config |
| E10 | zcrx default-on with its gate chain (D5) |
| E11 | PERF board: every PERF-1..25 item either landed with its named instrument or closed with a counted A/B falsification recorded in `.benchmarks/` |
| E12 | Every P1/P2/P3 closed, or carrying a written adjudication in the RC manifest — no silent deferrals |
| E13 | Docs corrected to the D1 posture (§6.12): guarantee tables per evidence tier, AGENTS.md/README/`FOPEN_NOFLUSH` comment drift fixed |
| E14 | RC manifest: item-ID → fix-SHA → repro-test map, adjudications, guarantee classes, evidence index (closes the §11 evidence-practice items) |
| E15 | Fresh metadata baseline + scoreboard re-run on the RC binary; stale/retracted findings annotated in `.benchmarks/` |

---

## 2. Phase structure

Eight phases. 1/2/3 parallelize across workstreams; 4 is the long pole and starts as soon as Phase 0 lands; 5–7 backfill; 8 is strictly last on a frozen candidate.

### Phase 0 — Gates and knowledge-changers (first; changes what is known)

| Step | Item | Action |
|---|---|---|
| 0.1 | **ENG-1 (narrow)** | One commit narrows `#![allow(clippy::all)]` → `style, complexity, pedantic`; follow-up commits fix the ~14 correctness/suspicious findings. Re-engages `correctness`/`suspicious`/`perf` immediately. **Re-baseline the item list afterward** — new findings join the program. (Full style burn-down is Phase 6's last item → E2.) |
| 0.2 | **ENG-2** | Install cargo-audit, add to `task check` + full gate. Expected findings: `rsa` (D3 replaces it, Phase 1), rustls 0.21 (ENG-5 deletes it, Phase 5). |
| 0.3 | **ENG-7** | CI workflow running `task check` per PR. Everything after this is enforced. |
| 0.4 | **[A]-validation sweep** | Read-only source confirmation of the [A] items feeding Phases 1–4 (DUR-1/3/4/5/6/7, VAL-4/5/6, FUSE-2 rows, §6.11, MEM-3/5) before fix branches cut. **Rev 3:** re-anchor all line references against the post-merge-train tip (`391dec2`+) and extend the sweep over the five merged campaigns' ~2,500 unreviewed lines (SDK direct-link/session, derivation resolvers, bench harness, claim-release fix). |
| 0.5 | **Doc corrections (D1)** | §6.12 corrections to AGENTS.md/README per the D1 posture; RES-21 (lock-order level 3.5); ENG-15 drift. Docs-only gate. |

### Phase 1 — Security and reachability P0s (three parallel workstreams)

**A — input validation:** VAL-1 (checked ioctl arithmetic, clamped block loop, `gds` feature gate, fuzz seed) · VAL-2 (xattr allowlist mirrored into the backend, `listxattr` filtering, migrate the in-tree tests that write layouts through the surface) · VAL-3+D3 (key material off volume/argv; **replace RSA key-wrap** — new wrap format behind an incompat bit staged for the Phase-8 reformat window; `zeroize`, redacting Debug; also covers VAL-7h key hygiene).

**B — wire surfaces:** VAL-4 (shim peer/seal verification, socket-dir hardening) · VAL-5a–e (IPC control-plane bounds; 5d's `retain` pattern also closes RES-5) · VAL-6 interim hardening (frame bounds, connection caps, accept backoff, deferred body allocation, nonce freshness) — the authn/discovery redesign itself is S3 (Phase 4), but the memory/DoS bounds cannot wait for it given the `0.0.0.0` default.

**C — memory ownership + operability:** MEM-1 + MEM-2 in one branch (ownership for the zero-copy read destination; `JoinSet`-owned assembly tasks — also closes RES-9) · MEM-3 (zcrx cancellation drop-guard + CID return — first link of the D5 gate chain) · ENG-3 (audible daemon: default `info`, loud log-file failure) · ENG-4 (staging-wipe guard).

### Phase 2 — Durability spine (strictly ordered)

1. **TEST-1** — extend `arm_power_cut` to the `NvmeBlockDev` worker (track writes since last data-device barrier). Prerequisite for everything below.
2. **DUR-2** — `Fsync { DATASYNC }` op + VWC probe gauge + buffered-fallback policy (fail mount or real flush), coalesced per the `sync_coalescer` discipline.
3. **DUR-1** — fsync contract: pre-collect block set, staging leg escalates to `upload_active_block_bytes`, data barrier strictly before the metadata barrier (kill the `try_join!`). Red against `:9673` first.
4. **DUR-3 + DUR-4** (parallel once 1–2 land) — epoch-stamped `pending_reclaim`; transactional bitmap snapshot-and-clear + promoted generation guard.
5. **DUR-5** — A/B superblock (root-ledger pattern) + serialized `set_incompat_bit`.
6. **DUR-6 ⊕ PERF-9** — one design decision: checksum/CoW/barrier the existing blob, or land the sharded indirect map that obsoletes it. With PERF-9 in scope (nothing deferred), **the sharded format is the recommended single fix** — it closes DUR-6's three defects by construction and takes the incompat bit into the Phase-8 reformat window.
7. **DUR-8a–f** — ride the nearest branch per file, each with its own red-first test.
8. **Per-layout durability matrix** — parameterized suite on the TEST-1 harness: inline, staged, striped write-through, striped-via-staging, W1 patch, W2 fold, in-place overwrite, rewrite-shadow close, IPC ring write.

(DUR-7 moved to Phase 4 per D4 — it ships as the first consumer of the cross-volume transaction machinery.)

### Phase 3 — Transport conformance (parallel with Phase 2; disjoint files)

1. **FUSE-1** — minor ≥ 36 + `FUSE_INIT_EXT` together; negotiation test; live-mount A/B on the target kernel **before** landing. Must precede Phase-8 perf evidence — **and (Rev 3) gates the reset-v5 window's bit-62 row per D6**: as staged, the kernel discards the daemon's flags2 echo, so patch 0027's arm never engages. The A/B must additionally assert bit 62 lands (`sb->s_time_max` set ⇒ generic/634 single-test passes on the sqz kernel).
2. **FUSE-2 ⊕ PERF-16** — designed jointly: the per-`(qid, ent_idx)` slot state machine with `(qid, ent_idx, commit_id)` carried in the request replaces the `pending` map rather than wrapping it. One `fail_ent` helper on every non-reply exit; always-on `transport_requests_{failed_synthetic,abandoned}` counters; ungated stale-pending watchdog. Acceptance: per-row legs + `abandoned == 0` suite-wide.
3. **FUSE-3a–l** — batched into branches by file.
4. **FUSE-4** — 4a `FOPEN_KEEP_CACHE` (≡ PERF-6, with the warm double-read measurement); 4b real generation from the superblock uuid; 4c pairs with POSIX-3; 4d/4e as specified.

### Phase 4 — The DLM / multi-node program: S0–S11 + cross-volume tx + discovery (the long pole)

Stages land in §6.9 order, each behind its stated gate. Additions from D1/D2/D4:

| Stage | Ships | Gate | Additions |
|---|---|---|---|
| §6.11 first | Red-first stale-fencing remount test | red today, green at S2 | Independent; start immediately |
| S0 | `LockManager` trait; delete the mock family (closes most of ENG-14) | mdstorm + rand-4k within noise | Wire or delete `lease_acquire_*` counters honestly |
| S1 | Global `grant_seq` replaces `FENCING_MAP` (closes RES-2) | RSS flat over 10⁷-inode walk | — |
| S2 | Durable `WriterClaim.term`, composed tokens (incompat bit 7) | fencing remount-monotone | Bit 7 batched into the Phase-8 reformat window |
| S3 | **`cluster_wire`** — job_wire ported onto it (closes VAL-6 structurally) | job-wire fidelity legs unchanged | **D2 lands here:** zero-config mutual authn derived from the `job:enroll` meta-KV secret (possession of volume access = membership); **DISC-1** auto-discovery via `client:{uuid}` records (they already carry endpoints); default `0.0.0.0` is then safe-by-construction. Measure the fabric RTT here — it prices S8 (risk R1) |
| S3.5 (new, D4) | **Cross-volume transaction machinery** — **LANDED 2026-08-05** (`src/meta_backend/crossvol_tx.rs` + `KvMetaBackend::xv_apply_step`; normative `docs/design-cow-kv-metadata.md` §4.10a): the `SQZXTX01` intent record rides step 0's commit (atomic, zero extra entries), protocol barriers order it across devices, the retirement is synchronous before guard release, and mount recovery rolls every open intent FORWARD through the same witnessed applier the live path uses — so **compensation is never needed at recovery** (all validation is consumed at plan time; count steps are absolute post-images with a (pre, post) CAS witness). **DUR-7 CLOSED** for cross-volume unlink/rmdir, link, cross-parent rename (incl. dest-replace and `RENAME_WHITEOUT`) and `RENAME_EXCHANGE` | **MET**: `tests/crossvol_tx_tests.rs` — commit-boundary seam over EVERY enumerated window of all three shapes (pre- or post-state, never intermediate), recovered-inode reclaimability, single-volume zero-intent + one-entry-per-op non-regression, retirement-before-release, recovery idempotence, a power-cut leg, the foreign-witness leg, and concurrent inverse ops for the acquisition order. Red proof: with recovery neutered, 9 of 13 legs fail with the exact DUR-7 shapes | No incompat bit (an intent is an ordinary typed record under a reserved ino — no reformat window). Cost: +1 journal entry and ≤ 2 coalesced barriers per CROSS-volume op, zero on strict volumes, zero on single-volume sets. **S8's seam is named**: `crossvol_tx::execute` + the one per-volume applier call a remote participant replaces; a remote participant is NOT expressible until S8's wire carries `Metadata` verbs. Cross-volume `create` deliberately NOT wrapped (reason + what is owed in §4.10a) |
| S4 | Slot lock manager, solo mode — **LANDED 2026-08-03** (`src/dlm_slot.rs`; `tests/dlm_slot_lock_tests.rs`; design note `docs/design-dynamic-meta-routing.md` §9) | **mdstorm + rand-4k + scoreboard within noise** — DEFERRED by D11 (bench/rig freeze); `dlm_rpcs == 0` asserted in-suite across every acquire shape and exported on `.stats` | The go/no-go gate for everything after |
| S5 | Read-only coherent client mounts (§6.8: RO mount mode, root-ledger revalidation cadence, freed-offset grace period, TTL alignment, purge-on-revalidation, reader lockdown) | N readers × cached stat/s capability row | The freed-offset grace period is also the multi-writer prerequisite (§6.3) |
| S6 | Membership off the journal (lease-based liveness) — **LANDED 2026-08-05** (`src/membership.rs` + `src/membership_wire.rs` + `src/membership_sim.rs`; `tests/dlm_membership_tests.rs`; operator surface in `docs/operations.md` §Membership plane) | volume-0 journal tx/s → ~0 at 15 k **simulated** clients — **the MEASURED half is DEFERRED per D11**; the in-process contract is pinned instead (500 renewals move `meta_kv_journal_entries` by 0, with a contrast arm proving the counter is live), and the harness that produces the row is landed | Mechanism: durable registration written ONCE (the `membership_owner` rendezvous record; §6.2 item 7's `claim_set` on membership CHANGE, incompat **bit 14** built-not-stamped per D9) + liveness by lease renewal over `cluster_wire` with the lease table in RAM — *durability of liveness is a category error*, the rejected slab/gossip alternatives are argued in the module docs. Read side: ONE `getxattr` + a paged RAM census (readers visible for the first time, with zero metadata writes). Two clocks with the client's strictly stricter (`T_self = T_owner − 2·skew_max − D_purge`, unsafe configurations REFUSE). Owner failure = re-assertion + grace window. The D1 harness (`membership_sim`) lands here and runs at a few hundred clients; the **15 k row is deferred evidence** and reports volume-0 journal tx/s, renewal latency distribution, revoke fan-out and failover grace completion — `SimReport::render` prints exactly that. §6.8 item 3 is NOT built: its API is named (`ack_free_epoch` → `min_acked_free_epoch` / `members_behind_free_epoch` / `evict`) |
| S7 | Data-plane custody-epoch fence + dead-epoch quarantine + WERO on data namespaces (closes RES-6's cross-host face) | stop/resume-past-TTL leg shows **device rejection**, both stacks | Multi-writer refuses to arm on non-PR substrates; confirm PR on `SQZ_DEVSUB_TRANSPORT=tcp` (risk R7). **In-process half LANDED** (`src/data_custody.rs`, `tests/dlm_data_fence_tests.rs`): one authorization point, quarantine lifecycle, both refusals. The capability gate is **incompat bit 11** — built, never stamped (D9), so it joins the Phase-8 reformat window beside bit 7. The device-rejection gate is DEFERRED and specified verbatim in `docs/design-nvmeof-target-management.md` §6.8.1 |
| S8 | Function-shipped metadata (the `Metadata` verbs on the wire, pipelined) — **LANDED 2026-08-05** (`src/meta_ship/`; `tests/meta_ship_tests.rs`; operator surface `docs/operations.md` §Metadata function shipping) | serial `tar -x` A/B **published even if it regresses** (risk R1) — **DEFERRED by D11**, see the landed-reality note below | Cross-**owner** shapes REFUSE loud naming S3.5 (it is not built); one-owner cross-volume ops are unchanged |
| S9 | Multi-writer data plane (custody tokens, direct DMA) — **LANDED 2026-08-06** (`src/data_grant.rs` + `src/multi_writer.rs` + `src/meta_ship/publish.rs`; `tests/dlm_multi_writer_tests.rs`; operator surface `docs/operations.md` §Multi-writer data plane) | 15 k-**shaped** write fan-out row with write-amplification columns, at achievable real scale + simulated fan-out per D1 — **DEFERRED by D11**, see the landed-reality note below | The D1 workload shape (disjoint file sets) is the primary row; shared-file is a correctness row, not a perf row. **The arm cannot be reached on a field volume**: nothing stamps the six capability bits (D9), and the D0 Layer-B2 gate still refuses a fresh foreign claim, so the co-writer posture waits on §6.2 item 7's consumer half |
| S10 | Subtree delegation + client-owned-slot placement | `tar -x` recovered to the S0 baseline | The D1 low-overlap workload makes delegation the steady-state path |
| S11 | Byte-range custody + W1 `patch_ineligible_range_shared` seventh clause | MPI-IO-shaped row | Keeps the W1 decision ledger honest |

Loom models (`grant_table_core`, `token_cache_core`, `lease_clock_core`) land with their stages; the `dlm_*` counter family with S4; risks R1–R9 tracked per stage with the resolving evidence the spec names.

**D1 validation strategy (explicit):** three evidence tiers, each labeled in the guarantee table — (i) *measured real*: multi-mount rows at the scale the lab can mount (tens–hundreds); (ii) *measured simulated*: the 15 k simulated-client harness for membership, lease renewal, revoke fan-out, and owner-failover grace (extends the spec's R6 1,875-client pattern); (iii) *arithmetic on measured constants*: per-shard and per-volume ceilings from (i) composed to 15 k, published with the formula. The product claim cites its tier.

### Phase 5 — P1 sweep (grouped, parallel)

- **Resources:** RES-1, RES-3, RES-4, RES-5 (≡ VAL-5d pattern), RES-6 (local D0-latch face; cross-host face is S7), RES-7, RES-8 (unwind-counting `tpc_spawn` — closes ~15 sites).
- **POSIX:** POSIX-1..8 — with POSIX-6 (structured errno) **early**: it is a one-way door once applications depend on the substring mapping.
- **Test infra:** TEST-2 (`SQUEEZEFS_TEST_REQUIRE_MOUNT=1` + skip ledger), TEST-3 (sleeps → `poll_until`), TEST-4 (fuzz targets: bootstrap blob, ring headers, cluster_wire frames, five on-disk decoders; proptest never-panic for `kv/{record,node,bset,journal}` + `layout_wire`), TEST-5 (`fd_table_core` loom + sync_coalescer + node_cache), TEST-6 (zcrx uring coverage — D5 gate chain link 2).
- **ENG:** ENG-5 (delete four unused deps → EOL TLS stack gone), ENG-6 (delete `[build] target-cpu=native`), ENG-8 (default-features gate line + dhat-on measurement exclusion).

### Phase 6 — P2/P3 burn-down (complete, per the nothing-deferred ruling)

Grouped by subsystem so each group is one or two branches touching one area:

- **DUR/meta P2s:** the remaining §1 items not already ridden in Phase 2.
- **MEM-4/5/6/7:** unsound-API privatization, guard field order, `set_len` (moot if ENG-13 deletes the DHT — do ENG-13 first), SAFETY-comment backfill (~165 sites — concentrate on `routing.rs`/`nvme_dev.rs` payload-pointer sites first).
- **VAL-7a–i:** stats-inode modes, staging file modes, admin-lane ladder, quota posture statement (per D1: multi-tenant training clusters — implement per-uid accounting or document), copy_file_range bounds, GDS pid handling, NQN components, key hygiene remnants, SPDK checkout.
- **POSIX-9..18** including the operations.md deviation notes (POSIX-12, POSIX-17).
- **RES-10..22** including the `debug_assert`→loud-never-fatal conversion pass (RES-22).
- **ENG-9..17:** untrack artifacts, env-knob convention unification + collision rename + knob docs (ENG-10), dev-mode logging (ENG-11), delete `recovery.rs` stub (ENG-12), delete/feature-gate p2p-DHT (ENG-13 — **decision: delete**; discovery is DISC-1's job now), dead items (ENG-14 remnant), AGENTS drift (ENG-15), publish=false + toolchain + --locked (ENG-16), fuse3 fork test leg (ENG-17).
- **TEST-9** coverage gaps: prioritize `job_wire`→`cluster_wire` (moots the old file) and `sync_coalescer`.
- **Last:** full clippy style burn-down → crate-root allows deleted → E2.

### Phase 7 — PERF board (all 25; after correctness fixes in the same files)

Sequenced to follow the correctness work that touches the same code, so no fix is measured twice:

**Rev 3 infrastructure status:** the measured-compare instrument this phase depends on (E11) is now real — `tests/run_bench_baseline.sh` merged with the microbench program, paced mode + exec-bit fix pending on `test/bench-baseline-pacing`, inaugural `reference.json` save in progress on the dev box. Every Phase-7 landing refreshes the reference (`save`) as part of the landing.

| Cluster | Items | Notes |
|---|---|---|
| Fused with correctness (already landed by now) | PERF-6 (≡FUSE-4a), PERF-9 (≡DUR-6), PERF-16 (≡FUSE-2) | Instruments recorded at landing time |
| zcrx completion (D5) | PERF-1 → Z3 gather fusion → **default-on flip** | Gate chain: MEM-3 ✓ (Ph1) → TEST-6 ✓ (Ph5) → Z3 → engagement laws + sustained ≥ 60 s row |
| Transport | PERF-2 (over_uring mutex), PERF-5 (SINGLE_ISSUER/DEFER_TASKRUN), PERF-18 (header line layout), PERF-23 (clock reads), PERF-24 (depth/max_background headroom), PERF-25 (REAP_EVENT_PARK_MAX re-derive, after PERF-18) | `perf c2c` + phase histograms; PERF-25 explicitly after PERF-18 |
| Read path | PERF-4 (fill issue economy + `register_buffers`; re-measure first — NUMA may have moved it), PERF-11 (detach tier publish), PERF-15 (sub-block framing for transformed volumes) | Named acceptance bars from the spec |
| Write/publish | PERF-8 (O(file-size) saves), PERF-13 (5 ms admission-park tail), PERF-10 (third copy on transformed blocks), PERF-14 (env memoization) | `publish_phase_ns` / `write_pipeline_phase_ns` |
| IPC/shim | PERF-7 (lseek interposition — big: +30–60 % on read()-based drivers), PERF-12 (kernel-lane alloc-free), PERF-19 (sched_getcpu skip), PERF-20 (per-thread severed pools) | tcp substrate, engagement counters |
| Free | PERF-3 (shard counters), PERF-21 (SeqCst→Release), PERF-22 (min_distance precompute) | — |

Every row: named instrument, tcp substrate where fabric-sensitive, A-B-B-A, sustained ≥ 60 s. A counted falsification is a valid closure (recorded in `.benchmarks/`), per E11.

### Phase 8 — Freeze, evidence, release gate, manifest (strictly last)

1. **The one reformat window:** batch every on-disk change — D3 key-wrap format, DUR-6/PERF-9 sharded indirect map, S2 bit 7, plus field validation of the already-shipped bit 6 (dynamic routing) — into a single reformat, field-validated together. **Rev 3 (D6 ruled):** the staged reset-v5 window proceeds as the separate earlier window carrying the bit-6 validation and the five campaigns' owed rows, gated on FUSE-1 landing + its live A/B first; this phase's window carries only the format changes above.
2. Freeze the RC candidate SHA.
3. **TEST-7** — fix the two flakes first (counts restart post-fix, per the multi-run discipline).
4. **TEST-8** — three-suite from-zero fail-fast on the RC binary; any failure → repro-port → fix → re-freeze → restart from zero.
5. Fresh metadata baseline on a clean box; full scoreboard; S4/S9/S10 gate rows; zcrx sustained row; 15 k simulated-client rows; write-amplification columns everywhere the rules require.
6. Annotate stale/retracted findings in `.benchmarks/` (the serve-decomposition "5.3 ms prize" retraction, the zram-specific write-wall verdicts).
7. **RC manifest** (E14): item-ID → fix-SHA → repro-test map; every adjudication; the tiered guarantee table per D1; evidence index.

---

## 3. Dependency DAG (critical edges)

```
ENG-1(narrow) → re-baseline → all fix branches        ENG-7(CI) → enforcement of everything
TEST-1 → DUR-2 → DUR-1 ; TEST-1 → DUR-3/4/6-matrix
DUR-6 ≡ PERF-9 (one design: sharded map) → Phase-8 reformat window
FUSE-2 ≡ PERF-16 (one design: slot state machine replaces pending map)
FUSE-1 → before all Phase-7/8 perf evidence ; FUSE-1 → reset-v5 window bit-62 row (D6)
MEM-2 ≡ RES-9 ; VAL-5d ≡ RES-5 ; ENG-13(delete DHT) moots MEM-6
MEM-3 → TEST-6 → Z3 → zcrx default-on (D5)
§6.11(red) → S2(green) ; S1 closes RES-2 ; S3 closes VAL-6 + carries DISC-1 ; S3 RTT number prices S8
S3.5(cross-volume tx) → DUR-7 ; S3.5 → S8
S4 gate → S5..S11 ; S5 grace period → S9 ; S7 → S9 ; S9 → S10 → S11
D3 key-wrap + DUR-6 map + S2 bit7 + S7 bit10 + bit6 field validation → ONE reformat window (Phase 8)
TEST-7 → TEST-8 → manifest → tag
```

**Critical path:** Phase 0 → S0…S4 (gate) → S5–S11 with S3.5/DUR-7 inline — the DLM program is the long pole; the durability spine (Phase 2) and transport (Phase 3) run beside it and must finish before the S9 fan-out rows (durable multi-writer evidence needs DUR-2's barriers to mean anything).

---

## 4. Risks and mitigations

| Risk | Mitigation |
|---|---|
| ENG-1 re-baseline surfaces unknown correctness items | Phase 0 exists for this; item list is re-cut before Phase 1 branches |
| FUSE-1 minor bump changes kernel behavior beyond flags2 | Live-mount A/B on the target kernel pre-landing; lands before all perf evidence |
| S8 function shipping regresses serial latency (spec risk R1) | RTT measured at S3; `tar -x` A/B published even if it regresses; S10 delegation must recover it — and the D1 workload (low overlap) is delegation's best case |
| 15 k cannot be tested with real writers (D1) | Tiered evidence: real / simulated / arithmetic, each labeled; the claim cites its tier — never a number the evidence doesn't carry |
| `0.0.0.0` default before S3 authn lands (D2) | Phase 1B lands the DoS/memory bounds immediately; the authn/discovery redesign is S3; until S3 merges, the current HMAC-enrollment posture + bounds is the interim, recorded as such |
| Reformat-window pileup (bits 6, 7, key-wrap, indirect map) | Deliberately batched into ONE Phase-8 window, field-validated together — D6 (Rev 3) rules how the already-staged reset-v5 window composes with this |
| The reset-v5 window ships a disengaged bit-62 row (Rev 3, verified) | D6 ruled (a): FUSE-1 lands + passes its live A/B on squeeze-test's running v1 kernel BEFORE the window is scheduled; the row stays in the runbook |
| Flakes poison Phase-8 counted runs | TEST-7 first; counts restart post-fix |
| Style burn-down churn vs concurrent branches | Deferred to end of Phase 6 by design |
| Scope is very large for one RC | The manifest's adjudication mechanism is the honest pressure valve: an item that shouldn't ship gets a *written* disposition, never a silent drop — and the phase gates (S4 especially) give clean stopping points if the program must re-scope |

---

## 5. New work items introduced by this plan (not in the spec)

| ID | Item | Origin |
|---|---|---|
| DISC-1 | Peer auto-discovery via `client:{uuid}` meta-KV records + zero-config mutual authn from the `job:enroll` storage-trust secret; job wire safe at `0.0.0.0` by construction | D2 |
| TX-1 (=S3.5) | Cross-volume intent-record/compensation transaction machinery; consumers: DUR-7 now, S8 later | D4 |
| SIM-1 | 15 k simulated-client harness (membership, lease renewal, revoke fan-out, failover grace) | D1 |
| KW-1 | Post-RSA key-wrap format + migration path inside the Phase-8 reformat window | D3 |

---

## 6. Design briefs (Rev 2.1 addendum — expansions of the phases carrying the most design risk)

Phases 1, 5, and 6 are deliberately not expanded: they are checklist-shaped, and the spec's per-item text (anchors, required behavior, acceptance) already serves as the design brief. Phase 7's rows carry their instruments and acceptance bars in the plan table. The four briefs below cover the work where a wrong early decision is expensive.

### 6.1 Phase 2 — the durability spine

**TEST-1: the data-device power-cut harness.**
- What exists: `uring_fs::arm_power_cut`/`power_cut` journals every write since the last `fdatasync` and reverts the uncovered ones — a correct volatile-cache-loss simulator with the wrong coverage: `NvmeBlockDev` runs its own io_uring worker and never passes through it.
- Design: a test-only fault seam at the worker boundary (env-gated, zero cost when off — the `SQUEEZEFS_TEST_WRITE_STALL_MS` precedent). Armed, the worker journals `(offset, len, prior bytes)` per completed Write; `power_cut()` restores everything not covered by a completed barrier op.
- Bootstrapping subtlety, load-bearing for sequencing: until DUR-2 lands there **is** no barrier op, so the armed harness treats zero writes as durable — which is exactly the red state DUR-1/DUR-2's first tests require. Land the harness first; write the red tests immediately after; then build the primitive that turns them green.
- API mirrors the existing shim (`arm_power_cut(dev)` / `power_cut(dev)`) plus a barrier-epoch observer — DUR-3's checkpoint-concurrency leg needs to ask "which barrier covered this push?"
- Hardware leg: at least one release-gate run on a real VWC-enabled NVMe box. The entire in-tree fleet (zram, null_blk, tempfiles) has no volatile cache, so a green gate there carries no durability information. Added to the Phase-8 checklist.

**DUR-2: the flush primitive.**
- `UringRequest::Fsync { datasync }` on the worker (io_uring `Fsync` opcode + `FSYNC_DATASYNC`), fixed-file aware; exposed as a per-device `NvmeBlockDev::flush()` coalesced with the `SyncCoalescer` discipline — reuse the existing coalescer (registration atomic with the flushing flag), do not re-derive it.
- VWC probe at mount: sysfs `queue/write_cache` (+ NVMe identify where available), surfaced as a stats gauge beside `meta_volume_atomicity_physical`, logged at mount.
- The buffered-degrade arm (O_DIRECT open failure) needs ONE decision at design review: (a) fail the mount loudly — recommended, it matches the "fix uring or fail loud" house law — or (b) keep the degradation and prove the new flush path covers buffered writes too. The spec permits either; today's posture (degrade + nothing ever flushes) is the only prohibited one.
- Ordering: per touched data device, the flush completes inside `flush_inode_to_backend` strictly before `persist_dirty_layout_if_needed`/`sync_device_for_ino`.
- Cost: measured honestly (tcp substrate, A-B-B-A, sustained ≥ 60 s) and recorded in `.benchmarks/`. Correctness is not gated on the number; coalescing width is the mitigation lever.

**DUR-1: red-first sequence.**
1. Red: write 1 MiB into a 4 MiB block → `flush_inode_to_backend` → assert the block map names a published device key and `staged_writes_in_flight == 0` for the ino (red against the early return at `fuse_client.rs:9673`).
2. Fix: collect the block index set **before** `flush_memory_buffers_for_inode` (or union with `list_staged_files()` filtered by the ino's `active_block` prefix); the staging leg escalates to `upload_active_block_bytes` rather than `put_active_block` + enqueue; kill the `tokio::try_join!` — the data barrier completes before the metadata barrier that names it.
3. Then the TEST-1-powered legs: fsync + power-cut per layout, and the no-orphaned-mapping assertion (every key the durable map names reads back the bytes written).

**The acceptance matrix.** One parameterized suite (rstest): 9 layout rows — inline, staged, striped write-through, striped-via-staging, W1 patch, W2 fold, in-place overwrite, rewrite-shadow close, IPC ring write — × 2 legs (fsync-then-power-cut; power-cut-mid-write, asserting recovery refuses rather than serves torn state).

### 6.2 Phase 3 — the FUSE-2 ⊕ PERF-16 joint design

- One slot state machine per `(qid, ent_idx)`: `Registered → Delivered → Replied → Registered`, owned exclusively by the queue worker (single-owner by construction ⇒ no loom model required; debug-assert the ownership). The `pending` unique map is **deleted, not wrapped**: requests carry `(qid, ent_idx, commit_id)` so replies address their slot directly — this is simultaneously what removes PERF-16's three sharded-mutex ops per request and what closes FUSE-2 rows 4 and 10 structurally (there is no map to miss or collide in).
- `commit_id` rides `user_data`, distinguishing REGISTER from COMMIT CQEs — closes row 6 (the EAGAIN re-REGISTER-vs-re-commit confusion the code itself documents as a hazard).
- ONE `fail_ent(ent, errno)` helper; every non-reply exit routes through it. The machinery already exists inline twice (`fuse_over_uring.rs:2500-2515`, `:2706-2718`) — consolidate, then route the remaining nine rows.
- Always-on counters `transport_requests_failed_synthetic` / `transport_requests_abandoned` (must-stay-0 tripwire), and the `:1973` stale-pending scan promoted to an ungated watchdog. The exactly-one-reply invariant lands as the doc comment on the state enum.
- The in-tree exemplar to copy is `TpcScheduler::dispatch` (dead-lane detection, re-dispatch, counter, abort-rather-than-blackhole) — the spec's §8 names it as the model.
- Acceptance: one leg per in-process-reachable row; `abandoned == 0` across the full suite; the `fuse_resend` case on a live mount. Note TEST-7 flake #1 (`multi_queue_tests::storm::…_no_starvation`) lives in this code — expect its fix to ride this redesign, and apply the multi-run discipline when claiming it fixed. **Resolved separately, 2026-08-04** (pre-RC loose ends): it was a TEST bug — `transport_parked_commits == 0` asserted the ABSENCE of the §5.4 re-arm gate rather than its correctness, and the redesign's cheaper reply path makes the benign park race MORE likely. The leg now asserts the park ledger closes (`parked ≡ unparked`, new `transport_unparked_commits` counter); do not re-litigate it as a transport defect.

### 6.3 Phase 4 — cluster_wire, discovery, and the transaction machinery

**S3 `cluster_wire`.**
- Framing: length-prefixed, schema-versioned, binary (postcard/bincode — `serde_json`'s ~1 µs per frame fails the 10 µs custody budget); bounded frame sizes with streamed bodies; per-class deadlines; connection caps, accept backoff, finished-handle pruning. Landing this retires VAL-6's bounds and RES-5's job_wire half permanently rather than patching a file the program then deletes.
- Authn (D2): challenge-response possession proof of the `job:enroll` meta-KV secret with a **server-issued** nonce (today the worker picks its own nonce — the replay gap), then a session key derived from the secret (TLS-PSK, or exporter-bound per-frame HMAC). Storage trust is the root: whoever can read the shared metadata volume is definitionally inside the trust domain, which is what makes zero-config authn sound. The accept-everything certificate verifier is deleted; the verification-strength ladder keys on an authenticated channel, never on `transport == "tls"`.
- DISC-1: endpoint publication already exists — `client:{uuid}` records carry the job-wire endpoint. Discovery = enumerate the records; no multicast protocol, the shared volume IS the rendezvous. Interim form reads the records as they are today; final form rides S6's lease-based membership so discovery adds zero load to the ino-1 hotspot (§6.5 item 3).
- Deliverable beyond code: a counted RTT row on the target fabric. This single number prices S8 (risk R1) before S8 is designed.

**S3.5 / TX-1 cross-volume transactions.**
- Scope discipline: this is two-phase commit with a deterministic same-process coordinator (the D0 claim holder) over volumes it already exclusively owns — NOT distributed consensus. It becomes remote-capable at S8 by reusing the record format, not by redesign.
- Protocol: durable **intent record** on the coordinating volume (tx-id, op type, participant volumes, per-participant redo + compensation payloads, fencing term), barriered → participant commits applied in fixed volume order, each stamped with the tx-id for idempotency → intent completed/deleted. Mount replay scans open intents: roll forward when all participants prove applied, compensate otherwise.
- DUR-7 is consumer #1 (cross-volume link/unlink/dir-rename). Acceptance: the two-meta-volume commit-boundary seam test — pre- or post-state, never an intermediate one — plus one mount-replay leg per enumerated crash window.

**SIM-1: the 15 k simulated-client harness.**
- Each simulated client is a lease-plane state machine (register → heartbeat → renew → answer revokes) speaking cluster_wire against a real volume set — no mount, no data plane. 15 k clients = 15 k tasks on a few boxes.
- Rows it owns: S6's gate (volume-0 journal tx/s → ~0 at 15 k), the revoke fan-out latency distribution, and failover grace-window completion at 1,875 (the spec's R6 figure) and at 15 k.
- Labeling law: every SIM-1 number is tier-(ii) evidence (*measured simulated*) in the D1 guarantee table, never conflated with real-mount rows.

**The S4 gate, made precise.** "Within noise" = medians of 3, A-B-B-A wherever the store ages, both substrates per the two-substrate rule, scoreboard full run (not smoke); `dlm_rpcs == 0` asserted **inside** every existing perf rig row rather than as a separate test. S4 is the program's go/no-go: if solo mode is not free, S5+ does not start until it is.

**S9 landed reality (2026-08-06, `feat/dlm-s9-multi-writer-data`). The measured half is DEFERRED, not met — the gate is NOT satisfied, and the arm is not field-reachable.** §6.9's S9 gate is the 15 k-shaped write fan-out row with write-amplification columns; ruling **D11** freezes benches, rigs and suites, so it has not run and **no number about S9 exists**. What landed is the mechanism plus the attribution the deferred row will need (`dlm_custody_phase_ns` rtt/arbitrate/adopt/renew, always on).

What landed, and where each promise came from:

* **Remote write custody** (`src/data_grant.rs`) — JOIN mints a client lease whose epoch IS the custody epoch its grants ride (per *lease*: a client holding three grants has ONE custody, and it is custody that moves); ACQUIRE takes the authority's **own** local lease, so arbitration is S11's interval algebra and disjoint byte ranges of one file are two live grants (ruling **D8**, cross-node) while an overlapping exclusive span is refused inside the caller's wait budget; RENEW is the heartbeat **and** the only revocation channel, carrying the co-writer's declared in-flight destinations (the job wire's pre-allocated-destination law); REVOKE/EXPIRY retire the grants, mint ONE dead epoch per client and quarantine those offsets through S7; RECLAIM is the grace window's re-assertion in the successor's era. Release needs a `DrainProof` whose constructors demand evidence (`preempt_landed(0)` returns `None`), so "no release without a proof" is a type property. **Only custody travels — a co-writer's bytes go straight to the shared namespace.**
* **S4's foreign-home refusal became a round trip.** `dlm_slot` counted the RPC and refused *"until the remote arm ships"*; it now ships to the home's authority and adopts the answer (`dlm::adopt_remote_grant`: era adopt → floor raise → custody record with the OWNER's token), so the ~24 fencing reads, W1's seventh clause and the lease's liveness all answer from the authority's decision. Nothing on a client mints custody. `dlm_rpcs` keeps its S4 meaning exactly. The refusal survives where no owner can answer (no client armed — every shipped mount; a non-inode object).
* **S7's epoch gained its per-client half.** S7 reserved the low 40 bits and specified "advance the epoch, not poison it"; the epoch is now `(term << 40) | custody generation`, an advance retires every authorization minted under the previous generation (`data_dma_epoch_refusals`), and generation 0 — every single-writer mount — makes the epoch exactly S7's term base.
* **S8's publish gap** (`src/meta_ship/publish.rs`) — the eight non-trait verbs the daemon publishes through (`set_layout_and_size`, `merge_layout_and_size`, `commit_block_refs`, `park_write_times`, `destroy_inodes`, `create_with_rdev_size`, `xattr_value_cap`, `readdir_stream`), as an **additive** vocabulary on its own verb block so S8's pinned schema did not grow. The daemon's eight call sites route: one relaxed load unarmed, then today's call verbatim. A foreign-home publish with no client armed **refuses** — executing it locally would append to a peer's journal ring.
* **The arm** (`src/multi_writer.rs`) — S8's un-called `arm_ownership` finally has its caller, together with the S7 fence and the custody plane. Six-rung refusal ladder: reader / the **six** capability bits 7·9·10·11·13·14 (each a §6.2 assumption whose absence makes a second writer unsound; bits 8 and 12 deliberately NOT required — they express two appenders on one volume, which volume-granular ownership never produces) / non-PR substrate / membership off (an unseeable co-writer is an unevictable one) / no durable era / `SQUEEZEFS_MW_BIND=off`. **S9 takes no incompat bit** — a fifth parallel bit collision was available and declined.

**Not done, deliberately, and each one named rather than implied:** no **push revocation backchannel** (a revoked co-writer's belief can outlive the revoke by one renewal cadence — bounded by its own `T_self` self-fence and by the device, never by its belief; spec §6.6's server→client revocation and risk R5's thrash valve need it); no **data-plane allocation partition** (two writers' allocators would collide on fresh offsets — which is why a co-writer *declares* its destinations and why the job wire's pre-allocated-destination model is the sound shape today; the analogue of §6.2 item 3 for the DATA plane is unbuilt); no **retry** on the publish lane (no dedup window, and `create` is not idempotent); the quarantine wire carries **bare offsets** rather than `(volume, offset)`, so the router sink admits by capacity — over-quarantining at most one offset per volume, which is bounded and visible, versus under-quarantining, which is silent corruption; and the **D0 Layer-B2 gate is untouched**, which is what makes the co-writer posture field-unreachable and the metadata guarantee-class table unchanged.

**S8 landed reality (2026-08-05, `feat/dlm-s8-function-shipping`). The measured half is DEFERRED, not met — the gate is NOT satisfied.** §6.9's S8 gate is the serial `tar -x` A/B published even if it regresses; ruling **D11** freezes benches, rigs and suites until the DLM can serve N readers and N writers, so that row has not run and no number about S8 exists yet. Do not read this row as a met gate: what landed is the mechanism plus the *attribution* the deferred row will need (`meta_ship_phase_ns` route/queue_wait/encode/rtt/decode + `meta_ship_owner_phase_ns` admit/dispatch/execute/reply_encode/total, always on).

What landed: `src/meta_ship/` — (a) the **ownership plane**, per-VOLUME by construction (§6.10 R4), so an intra-volume split is unrepresentable rather than refused: one volume still has one journal ring, one bitmap, one root ledger (bit 8's partitioned append is built-not-stamped and nothing assigns appender ids) and one node cache (the third-gate-state residual in `kv/revalidate.rs`); (b) the **verb vocabulary** on the S3 wire — 13 verbs covering the trait's 13 required members, with `create` riding `create_with_rdev` and `lookup` composed from `LookupDentry` ⊕ `Getattr` because its two participants can home on different owners; (c) the **client router** — routing is `route_ino` plus one relaxed load, an unarmed mount takes today's path verbatim, and the pipelining unit is the BATCH (one conveyor + drain per owner; a serial stream pays one RTT per verb and the drain never *adds* delay, which is R1's cost made visible rather than hidden); (d) the **owner service** on the pinned `sqz-cluster-svc{n}` lanes with the batch handed to the runtime that owns the backend's tasks (`commit_tx` spawns the per-volume conveyor pass task on the committer's runtime — an inline lane execution would give a volume's whole conveyor a lane-lifetime current-thread runtime), a `(client_epoch, request_id)` dedup window for idempotency, and whole-frame era + grace gates that key on `mutating()` (reads are answered from current state and relearn the era for free); (e) the **client token cache**, which is how **S4's fencing-read contract is discharged** — the read is homed behind one relaxed load and a foreign home serves owners' own grants, never the local view; a miss serves the owner era's base and trips a must-stay-0 tripwire, because a miss means the intent-lock property was violated.

Also landed: `cluster_wire::RpcAsyncService` (the explicit handoff S3's synchronous contract named), `RoutedMetaBackend::lookup_dentry`, the mint constraint (a create mints only into an owned volume — the Lockify self-designating creator, free here because inos are monotonic), and the `meta_ship` stats family. **No incompat bit**: ownership needs no durable record, because each volume's D0 `writer_claim` already names its holder and its era, so the claim holder IS the owner — bit 11 stays free (pinned in-suite). **Not done, deliberately**: no production arm (the multi-writer mount is S9's — a mount that ships metadata but cannot ship data custody is not a product), no remote CUSTODY transfer (S9), no shipping of the non-trait capability surface the daemon needs (`create_with_rdev_size`, `readdir_stream`, `set_layout_and_size`, `merge_layout_and_size`, `commit_block_refs`, `park_write_times`, `destroy_inodes`), so the daemon is not switched onto the router — that wiring is an S9 deliverable, not a missing S8 line.

**S4 landed reality (2026-08-03, `feat/dlm-s4-slot-locks`).** The structure shipped; the *measured* half of the gate is deferred, not met, by ruling **D11** (no benches, rigs or suites until the DLM can serve N readers and writers) — S5 planning may proceed on the structure, but the go/no-go verdict is only adjudicated once the freeze lifts and the mdstorm / rand-4k / scoreboard rows run. What landed: `src/dlm_slot.rs` homes every lock object on the durable meta slot map (`route_ino_width` over the set's frozen width, published to the lock plane when a routed set is opened — design note `docs/design-dynamic-meta-routing.md` §9), asks a lock-free ownership question (`is_local_slot`, unconditionally local in solo mode) at the ONE acquire entry point, then delegates to the unchanged `LocalLockManager`, so solo acquisition is bit-for-bit S0–S2. `DlmClient` resolves to the new manager, which is how ~200 call sites got homing + ownership with no edit. A foreign home is refused loud and counted (`dlm_rpcs`) at the site the remote acquire will occupy — reachable today only through a documented test seam, which is what keeps the counter live rather than decorative. `.stats` gained `dlm_mode` (`solo`), `dlm_rpcs` (**must stay 0** on a single-node mount) and `dlm_term`. Fencing reads are deliberately NOT homed (~24 hot sites; the local view IS the authority in solo mode) — **the contract S6/S8 must honour**, stated on the function. Written-but-unrun per D11: two Criterion rows (`s4_lock_home_slot_ino_path`, `s4_is_local_slot_solo`) with the prediction ≲ 20 ns combined, i.e. < 2 % of the uncontended acquire row.

### 6.4 Phase 8 — the reformat window and the manifest

- **One format bump**, stamped once: KW-1's wrap format, the DUR-6/PERF-9 sharded indirect map, and S2's bit 7 land in code throughout the program but are stamped together in this window; bit 6 (already shipped on in-process evidence only) gets its field validation in the same pass. Rationale: the spec's evidence note — bit 6 already blocks all field validation behind one reformat; every additional window multiplies that cost across the fleet.
- Order inside the phase is strict and restarts on any fix: reformat → TEST-7 flakes fixed → freeze the candidate SHA → TEST-8 from zero (fail-fast; any failure → repro-port → fix → re-freeze → restart) → evidence rows → manifest → tag.
- Hardware durability leg (from §6.1): one release-gate run of the durability matrix on a real VWC-enabled NVMe box.
- **RC manifest skeleton** — write it at Phase 0 and fill it as items close, so E14 is an accumulation, not an archaeology project: (1) disposition table, every item ID → {fix SHA + repro test | written adjudication}; (2) the D1 tiered guarantee table (measured-real / measured-simulated / arithmetic-on-measured-constants); (3) evidence index into `.benchmarks/`; (4) known issues and declared deviations (the POSIX-12/17 class); (5) the format/incompat ledger — bits, stamp dates, migration story.
