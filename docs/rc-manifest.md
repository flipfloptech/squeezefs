# SqueezeFS RC Manifest

**Purpose.** The release-candidate's accountability record: every engineering-spec item → its disposition (fix SHA + repro test, or a written adjudication), the guarantee table by evidence tier, the format/incompat ledger, and the evidence index. Plan exit criterion **E14**; started at Phase 0 by design so it accumulates instead of becoming archaeology.

**Release intent.** Public RC — a gift to the AI community. That intent raises two bars above internal-use software: (a) untrusted-input surfaces get first-class treatment (any process in the namespace reaches the IPC socket; the job-wire listener binds `0.0.0.0` by ruling D2), and (b) every claim in user-facing docs must cite the evidence tier it actually has.

**Inputs.** `docs/pre-rc-engineering-spec.md` (Rev 3) · `docs/pre-rc-execution-plan.md` (Rev 3) · rulings D1–D7.

---

## 0a. Status: the document is fully dispositioned — 220 commits

Every section of `docs/pre-rc-engineering-spec.md` has been worked, and every item has a disposition: landed, adjudicated in writing, falsified by measurement, or deferred to a named stage with its rationale. No item is silently open.

**Closed with code:** all 4 memory-safety P0s · all 6 security/validation P0s · the durability spine + DUR-3/4/5/6 · FUSE-1, FUSE-2 (all eleven lost-reply rows) + FUSE-3a/b/d/e/f/g/h/i/j/k/l + FUSE-4a/b/c/d/e · POSIX-1..13, 15..18 · RES-1..15, 17..22 · ENG-1..17 · TEST-1..7, 9 · PERF-1..14, 16, 19, 21, 22 (PERF-3 partial) · DLM S0, S1, S2 · the three multi-writer format foundations.

**Closed by counted falsification** (plan E11 — a measured "no" closes an item): **PERF-18**. Built in full, kept green, then measured: 0.8 % in the field shape and **2.46× WORSE** in the symmetric shape. The spec counted the writes and ignored the reads — same-line keeps the hot side's RMW *and* its per-op read of the partner word inside one exclusively-held line. Reverted, `IPC_ABI` stays 3, recorded with a standing `ipc_wake_pair_lines` bench group so nobody re-derives it.

**Closed by written adjudication** (a fix would have been wrong): FUSE-3c (unimplementable as specified — a REGISTER's CQE fires on request *delivery*, impossible before arm; submission-counting is structurally necessary) · RES-13's two maps (`block_allocator.incarnations` **must not** have a removal path — a reclaimed entry restarts at generation 0 and lets a stale in-flight fill pass its after-check) · RES-18 (bounding the sideband would be actively harmful: it carries INTERRUPT, so a full channel stalls the reader and delays the very INTERRUPTs that unstick requests) · RES-10's second clause (does not reproduce — both budgets scale with the queue that holds the tombstones) · VAL-7d (single-tenant posture stated explicitly rather than building speculative per-uid accounting) · POSIX-12, POSIX-17 (declared deviations).

**Deferred to the batched Phase-8 reformat window** (ruling D9 — bits built, not stamped): the sharded indirect map (DUR-6 ⊕ PERF-9), AEAD AAD binding (DUR-8d — would fail every existing encrypted open), KW-1's incompat bit, DLM S2's bit 7, partitioned append's bit 8, durable block refs' bit 9, writer-scoped staging's bit 10 (which must be stamped on EVERY member of a set — engagement requires unanimity), S7's multi-writer-data bit 11, PERF-15's sub-block framing, and bit 6's field validation.

**Deferred to a named stage:** DUR-7 → S3.5 (design note delivered: crash windows enumerated, the `SQZXTX01` intent-record wire specified, and the finding that its two-volume seam gate needs *no new harness*) · RES-16's remaining half → S3 · POSIX-14's handle allocation → assessed and argued sound under FUSE-2 + 3k.

**Owed to a venue we do not have:** PERF-24 (depth × max_background sweep — and `Q_DEPTH_DESIRED` is not a free constant: it bounds the registered pinned arena, so a sweep must report the R5 component alongside IOPS or it will read memory pressure as a depth effect) · PERF-25 (its re-derivation trigger never fired, since PERF-18 was falsified) · TEST-8 (the three-suite release gate) · the multi-writer field rows.

**One actionable finding not yet acted on:** PERF-23 established that the spec's "invisible at any credible op rate" claim for phase instrumentation is **false** — `Instant::now()` 19.25 ns, one recorded span 45 ns, so **~135 ns/transport op and ~225 ns/read serve = 13.5 %/22.5 % of a warm 1 µs op**. A free, semantics-preserving fix exists (adjacent phases each mint a fresh `Instant` where one read could end A and start B — 19 sites in `routing.rs`, 31 in `fuse_over_uring.rs`, ~2× reduction with identical numbers). Worth a follow-up branch.

---

## 0. Program summary (live)

**Dev has taken 162 commits since the program began** (`402ca77` → present). Every composed tip was gated: `cargo check`, `cargo clippy --all-targets --all-features -- -D warnings` (a gate that inspected *nothing* before ENG-1), `cargo fmt`, targeted suites, and bench smoke. The full `cargo test` and the three external POSIX suites are deliberately **not yet run** — deferred by ruling D7, and they are the remaining step before "RC-ready" is a true statement.

| Metric | Then | Now |
|---|---|---|
| P0 items closed | 0 / 17 | **24** (the census grew as agents found items the spec missed) |
| `cargo audit` vulnerabilities | 5, one with **no upstream fix** | **1** (a lockfile bump) |
| Clippy coverage | 0 lints across 142 k LOC | full gate, `-D warnings` green |
| Loom models | 55 | **61** |
| Fuzz targets | 0 | **9** (~75 M executions, 1 real find) |
| Dead subsystems | p2p/DHT (~500 LOC), recovery stub, mock family | deleted (**−1,148 lines** from the mock family alone) |

### Bugs found that the specification did not contain

The strongest evidence the program worked is what it discovered *while* implementing:

| Found | By | Severity |
|---|---|---|
| `BsetView::parse` sized a Vec from an attacker-controlled `u32` — a 32-byte checksum-valid image requests **128 GiB**; under any `RLIMIT_AS` it is `SIGABRT` | fuzzing (TEST-4) | Daemon kill from one corrupt sector, in a decoder contracted to *detect* corruption |
| A dropped fsync leader left `flushing = true` **forever**, wedging every subsequent barrier on that meta volume — and DUR-3's reclamation watermarks ride those barriers | contract tests (TEST-9) | Data-loss-adjacent. **The spec's §8 lists `SyncCoalescer` as an invariant "examined for defects and none found"** |
| `squeezefs clone` was a silent no-op **and actively destructive** — its throwaway wiring pointed a `TieredCache` at the user's default staging dir, discarding 26 GB of another session's read cache before doing nothing | DLM S2 + loose-ends | A documented CLI verb that destroyed data and reported success |
| `KvError::NoSpace` returned **EINVAL** to `write(2)` — the spec cited a substring rule that never matched (the message reads lowercase `no space`) | POSIX-6 | Out-of-space reported as an invalid argument |
| `StorageFull` fell through to **EIO** | POSIX-6 | A full filesystem reported as a device failure |
| `statfs` missed ~63 of every 64 creates (dynamic routing mints from guest cursors; the in-RAM test constructor's `W ≤ 1` identity hid it) | POSIX-1 | `df -i` unusable; the in-process fix passed while the mount leg failed |
| readdir's `..` synthesis scanned the whole dentry tree **per directory** — 15.5 ms at 16 k dentries, linear in the volume's total | POSIX-4 | Every `ls`/`find`/`du`/`rsync`/`tar` walk. Fixed: **56,600×** |

### Places the specification was wrong, and implementation proved it

| Item | Spec said | Reality |
|---|---|---|
| FUSE-3c | Count a queue registered only after `depth` REGISTER **CQEs** reap | **Would deadlock the mount** — a REGISTER's CQE fires on request *delivery*, impossible before arm. Submission-counting is structurally necessary |
| DUR-4 | Promote the generation guard to **refuse** the write | **Would wedge forever** — a cycle that wrote its pages then failed leaves `checkpoint_seq` unadvanced, so the retry arrives with the same seq. It raises instead, loudly |
| MEM-2 | A `JoinSet` **or** an owning handle | **Both are required** — tokio abort is cooperative, so a task mid-`memcpy` finishes its poll segment and still races the pool recycle |
| RES-13 | Add removal paths to four unbounded maps | Two are correct as-is: `block_allocator.incarnations` **must not** have one (a reclaimed entry restarts at generation 0 and lets a stale in-flight fill pass its after-check — the generic/074 family) |
| RES-10 | The tombstone leak lets the R5 Red clamp fail to clamp | Does not reproduce — both budgets scale with the queue that holds the tombstones. The real leak underneath was found and fixed |
| POSIX-7 | The shim "cannot refuse mmap" | Half-stale — mmap has been interposed since the L4 wave; the real gap was that the unbind was point-in-time |
| DLM §6.11 | A durable term closes it | Insufficient — clean unmount **deletes** the claim, so a claim-only term re-issues a crashed predecessor's tokens. A second never-deleted `writer_term` record is load-bearing |

Two spec `[A]` suspicions were also verified **benign** rather than "fixed" (the `sync_key` hash-coincidence reading does not hold; `MS_ASYNC` is covered by the shard `sync_all`), and every §7 line anchor had drifted — several by thousands of lines.

---

## 1. Disposition table

Status values: **LANDED** (fix + repro merged) · **IN FLIGHT** (agent working) · **OPEN** · **ADJUDICATED** (written decision not to fix, with rationale).

### Landed before the wave program (2026-08-02, dev `402ca77`)

| Item | Disposition | Evidence |
|---|---|---|
| FUSE-1 | **LANDED** — minor 36 + `FUSE_INIT_EXT`; kernel now folds the reply's flags2 | `53b887d` (red) → `3049b6d` (green) → `91b310a`; `.benchmarks/2026-08-02-fuse1-init-ext.md`; 31→36 audit found zero kernel `fc->minor` gates in (23, 45] |
| ENG-15 (bench-count half) | **LANDED** | microbench merge rewrote the AGENTS.md Criterion section |
| TEST/E11 instrument | **LANDED** — `tests/run_bench_baseline.sh` + paced mode + committed 99-median reference | `d04512f`, `755e5ce`, `402ca77`; `.benchmarks/criterion-baselines/reference.json` (strixhalo @ `3ce65cc`) |
| TEST-7 (drain flake) | **LANDED** — claim-release reclaim law; ×10 acceptance owed on the merged binary | `391dec2` lineage; `.benchmarks/2026-08-04-volume-drain-flake.md` |
| **ENG-3** (P0) | **LANDED** — default `info` filter (explicit `RUST_LOG` still wins); a failed `--log-file` open now fails the command loudly at three layers instead of silently discarding all logging; six invisible-today messages re-triaged, three promoted to `error!` (O_DIRECT→buffered degradation, checkpoint-tick failure carrying DUR-4, meta-volume teardown failure) | `e0c351c` (red) → `0efaa6f` (green); `tests/daemon_logging_tests.rs` |
| **ENG-4** (P0) | **LANDED** — `stamp_staging_dir`'s unguarded root `remove_dir_all` now requires a recognized staging marker or explicit consent, hard-refuses a seven-root denylist (canonicalized, prefix-aware), prints the deletion plan, and prechecks every declared dir so a refusal never leaves a partial wipe | `e0c351c` (red) → `0efaa6f` (green); `tests/staging_wipe_guard_tests.rs`, `STAGING_WIPE_DENYLIST` |
| **VAL-1** (P0) | **LANDED** — GDS ioctl arithmetic range-checked (`checked_add` ⇒ EINVAL, `end_offset == 0` early return, clamped end block), the key-resolve loop bounded, and the arm `gds`-feature-gated out of the default build | `d63045d` (red) → `6645c05` (green) |

### Landed by the wave program

| Item | Disposition | Evidence |
|---|---|---|
| **ENG-1** (P0) | **LANDED** — crate-root `#![allow(clippy::all)]` narrowed to `style, complexity, pedantic`; the re-engaged correctness/suspicious/perf groups surfaced **10 findings, all fixed** (not allowed away). `-D warnings` is now a real gate on every commit. Notable: the two `not_unsafe_ptr_arg_deref` sites became `unsafe fn` with `# Safety` contracts and SAFETY comments at all 9 call sites — **partially closes MEM-4** honestly. **Re-baseline verdict: zero NEW correctness items surfaced** — the debt was exactly the spec's named set | `906ecad` |
| **ENG-5** | **LANDED** — four unused deps deleted; the entire EOL TLS stack (rustls 0.21.12, webpki 0.101, sct, tokio-rustls 0.24, hyper 0.14, h2 0.3, +18 more) is gone. **This closes 3 of the 5 `cargo audit` vulnerabilities** (§5). rustls 0.23 is now the only TLS stack | `de5381f`; cargo tree 261→231 |
| **ENG-6** | **LANDED** — `[build] target-cpu=native` deleted (Portable-by-default) | `d287b55` |
| **ENG-9** | **LANDED** — non-source files untracked. **Spec correction:** `github.jpeg` was NOT unreferenced (README banner) — untracked per directive with the dangling reference removed | `45f1ff8` |
| **ENG-12** | **LANDED** — `recovery.rs` stub deleted; AGENTS.md ×2 and a doc comment corrected to name the real machinery (`NvmeStaging::new`) | `2b9ba5f` |
| **ENG-13 + MEM-6** | **LANDED** — the unreachable p2p/DHT subsystem deleted (incl. the quinn dependency, ~10 crates, and the accept-everything peer verifier). The shared rustls core the job wire genuinely uses was preserved intact as `src/tiering/cluster_tls.rs` with its mTLS admit/refuse pins ported. MEM-6's `set_len`-over-uninitialized died with it | `a686c7d` |
| **ENG-14** | **LANDED** — fuse3's zero-caller `dispatch()` deleted (with its banned `#[allow(dead_code)]`); 17 item-level `abi.rs` allows consolidated into one documented module-level allow. The `dlm.rs` mock family correctly left to DLM S0 | `da8d87f` |
| **PERF-6 / FUSE-4a** | **LANDED** — `FOPEN_KEEP_CACHE` set; the kernel no longer drops the page cache on every open (safe because `AUTO_INVAL_DATA` is negotiated + D0 single-writer + the L4 W1 notify handoff) | `02288f9` (red) → `d436a51` |
| **PERF-2 ⊕ part of RES-22** | **LANDED** — the process-global `over_uring` mutex (taken 4× per READ on one cache line) replaced by `Arc<OnceLock<Arc<Pool>>>`: per-op sites are now a plain acquire load with **zero RMWs**. Verdict: the mutex hid no real race, but a check-then-install TOCTOU that could double-start pools was found and closed structurally. Measured on the sim venue: reply-venue probe **−75 %/−66 %**, payload-buffer probe **−23 %** | `14303e7` (red) → `0d1c2fa` |
| **PERF-5** | **LANDED** — `SINGLE_ISSUER + DEFER_TASKRUN` on the FUSE queue rings behind a memoized runtime probe (never a version check — Portable-by-default); Modern-refusal degrades to today's setup with a warning, probe-miss is silent, Plain refusal stays loud. SQPOLL builders deliberately excluded (DEFER_TASKRUN is SQPOLL-incompatible) | `a2f3216` (red) → `3a28eb7` |
| **MEM-3** (D5 gate link 1) | **LANDED** — the zcrx classic lane's cancellation hole closed: `CidSlot` (RAII CID + owned depth permit) lives in the pending entry and returns at entry destruction, classic entries hold a destination keep-alive, and the poison drain only runs when the destination writer is provably done. **Documented deviation from the spec's literal prescription:** cancellation is deliberately NOT a poison event (or the `zcrx_lane_poisoned` must-stay-0 tripwire rots once D5 flips the lane default-on), and a sync `Drop` cannot join an aborted task so the prescribed drop-guard shape could not have closed the in-flight-poll race. Same acceptance met: no recycled-buffer write, no CID leak | `50ef713` (red) → `650c7a0` |
| **PERF-1 / Z3** | **LANDED** — gather fusion: `dest_addr` funnel reads serve via one completion gather straight into the registered destination, deleting Z2's bounce+serve intermediate. Area-backend-only by law (classic's foreign-task reader is the MEM-1 hazard class). Every Z2 law re-pinned; Z2 loom models re-attested 2/2. Sim-venue bench: **−40.4 %** at 128 KiB, **−74.6 %** at the 4 MiB EXA cold-block shape. New gauge `zcrx_dest_gather_bytes` | `ebc95a0` (red) → `ddeec96`; `.benchmarks/2026-08-04-zcrx-z3.md` |
| Integration fix (cross-agent) | **LANDED** — ENG-1's `gather_into` → `unsafe fn` (MEM-4) vs Z3's new bench call sites: the rebase merged textually but not semantically; caught by the post-merge clippy gate, fixed with SAFETY-commented unsafe blocks | orchestrator |
| **VAL-6** (P0) + RES-5 (wire half) + RES-16 (half) | **LANDED** — interim hardening of a port that every write mount opens on `0.0.0.0`: body memory is now committed in 64 KiB rounds as bytes arrive (a lying 16 MiB length prefix went from a 16 MiB zeroed allocation per connection to an **813 ns refusal**), per-class caps (8 KiB pre-enrollment / 16 MiB post) and per-class deadlines, an RAII connection cap claimed before any task exists, exponential accept backoff (the EMFILE busy loop is gone), and `handles.retain` bounding retention by live connections instead of connections-ever-accepted. **Replay closed**: `WIRE_SCHEMA` 1→2, the coordinator now speaks first with a server-issued nonce + freshness window + single-use registry (a v1 self-nonce worker refuses loud rather than enrolling on a replayable proof). **Ladder re-keyed**: verification strength now requires CA-pinned mTLS (`authenticated ≡ ca_cert ∧ ca_key`), never the mere presence of a TLS object; plaintext and TLS-unauthenticated are both plaintext-class ⇒ mandatory 100 % verify-reads; a CA cert with no key now refuses the listener loud instead of reaching an `.unwrap()`. New `SQUEEZEFS_JOB_WIRE_*` env family (env rather than clap flags purely from wave file-ownership; promoting them is a 2-hunk follow-up). `mac_eq` untouched, as the spec requires | `10476ae` (red) → `52faedd` → `f9573ce`; 27 tests |
| **VAL-2** (P0) | **LANDED** — the xattr denylist became a positive allowlist (`user.*` minus `user.squeezefs.`, `security.*`, `trusted.*`), enforced at BOTH the FUSE handlers and `KvMetaBackend`'s `Metadata` impl so the FUSE layer is no longer the only barrier. Screened `getxattr` returns **ENODATA, not EPERM** — deliberately, because EPERM would contradict the `listxattr` filtering and hand a prober a census of internal records. Daemon-internal writers moved to explicit unscreened `*_internal` methods. **Flagged, not forced:** `RoutedMetaBackend`'s internal plane stays unscreened (unreachable from FUSE; closing it is a mechanical cross-file migration), and relocating the four internal records out of the xattr keyspace remains an on-disk format change | `298d822`, `23d4c50`; 6 tests + `kv_xattr_screen` bench |
| **MEM-2** (P0) + RES-9 + part of DUR-8f | **LANDED** — assembly tasks are now held in an `OwnedTaskSet` (join-all reports the first error only after the last join; drop aborts) **composed with a co-owned `Arc<AssemblyDest>`**, because the agent established that abort-on-drop alone is insufficient: tokio abort is cooperative, so a task mid-`memcpy` finishes its poll segment and would still race the pool recycle. With each task co-owning the destination the buffer recycles only when the last owner drops. Pointer routed through the reviewed `unsafe impl Send` convention with SAFETY comments at all five copy sites; the retired `try_join_all` footgun is permanently pinned by a deterministic test. RES-9: a `MintedBlockGuard` + salvage reaper closes the allocate→publish leak window, which **incidentally closes 2 of DUR-8f's 4 sites**. Bench: par at N=2 (the kernel-FUSE shape), +0.38 µs/task at N=16 ≈ 0.01 % of a warm serve — priced honestly | `5cb5625` (red) → `63c5106` → `5a51def`; 7 + 179 tests |
| **PERF-7** | **LANDED** — `lseek`/`lseek64` interposed and the per-op syscall pair deleted from the offsetful ring paths. **Engagement proven exactly: `lseek` per `dd` row 4104 → 8** (dd's own startup seeks), deterministic in both A-B-B-A orders; the mirrored-offset cycle measures ~73 ns against ~1.48 µs for the deleted syscall pair (~20×). Dup verdict: dup'd bound fds share one refcounted `BindingCell`, so the mirror is exact intra-process — and this **fixed a pre-existing race**, since the old per-fd stripe split dup siblings across different locks. Fork handled by an AS-safe atfork prepare handler (epoch bump + one flush per armed cell); the authority predicate and every demote trigger are documented in design Rev 19 §5.4.3. **Honest limit:** the syscall-census venue is not a bandwidth venue — the +30–60 % throughput expectation is explicitly left for the fabric-latency rig rather than claimed | `90a4820` (red) → `b0086a1` → `747a88a`; 13 tests; full sudo preload gate both legs PASSED |
| **VAL-4** (P0) | **LANDED** — the shim now authenticates the daemon before trusting it, in three rungs: the socket's parent directory must be owned by root-or-the-mount-uid and not group/other-writable (checked **before `connect(2)`**); `SO_PEERCRED` must match root-or-the-mount-uid (checked **before the credential fd is sent**; a `getsockopt` failure is a refusal, never an assumption); and `F_GET_SEALS` must show `SEAL|SHRINK|GROW` (checked **before the mapping is trusted**). Daemon half binds through a validated dirfd and **refuses** rather than falling back to `/tmp`. Deviation flagged: rung 1 accepts `{0, mount_uid}` rather than the spec's root-only, since root-only would refuse every non-root mount's own runtime dir | `2595cae` (red, 4/9 legs failing) → `2ec0798`; `tests/preload_authn_tests.rs` 10/10 |
| **VAL-5a–e** (P0) + RES-5 (ipc half) | **LANDED** — all five control-plane bounds, each with behavioral red evidence: SCM_RIGHTS fd count now derives from `cmsg_len` with `MSG_CTRUNC` refusal (was: 7 of 11 extra fds installed and leaked, both daemon and shim sides); `SO_RCVTIMEO` + a registry covering **every** accepted connection (was: `shutdown()` wedged the full 15 s on one silent peer); a per-session in-flight ledger that poisons on protocol violation (was: 20 forged in-flight ops accepted); a derived control-thread cap + handle pruning (was: 48 threads against a cap of 32, 1070 handles retained); and a per-pass drain budget with round-robin (was: a sibling starved 10 s while a greedy session served 35,993 ops). Ledger release happens **before** `core.complete()` so an honest back-to-back submit cannot read as over-admission. Bench: budget compare +0.9 ns/op, ledger pair 25.7 ns/op — noise against a µs-scale serve | `db27f14`…`9056160` (7 red→green pairs); `ipc_host_tests` 33/33 |
| **DLM S0 + S1** + RES-2 + §6.11 repro | **LANDED** — `LockManager` trait + `LocalLockManager` (call sites unchanged behind a type alias, so S4 swaps in mode dispatch without touching ~200 sites); the dead mock family deleted (**−1,148 lines / 118 files**, incl. a `MockMessageStream::next()` that slept 999,999 s). The permanently-zero `lease_acquire_ok/fail` counters were **wired, not deleted** — they now measure real acquisitions, which matters because a 2026-07-14 decision to demote lease batching cited evidence those gauges were structurally incapable of producing. S1 deleted `FENCING_MAP` outright (RES-2 closed by deletion) for one global `AtomicU64` + an 8 KiB stripe floor: RSS on a 3 M-ino walk went **+303 MiB → flat**. The ~24-site fencing census **falsified the naive design** (existing tests pin post-release and held-range-visible reads), which is why the entry-fold + mint-time floor structure exists. §6.11 repro is a real self-exec process-death child, `#[ignore]`d naming S2 as the green-maker | `8bac392`, `33429f8`→`38a7561`, `9fbc117`→`277dd65` |
| **MEM-1** (P0) | **LANDED** — the zero-copy read destination now conveys ownership: a worker-held token on the transport's **existing** §5.4 `EntLeaseState` word, chosen over a new epoch protocol precisely because it adds **zero new protocol words** — the loom-verified Dekker pairing, parked-commit wake, and shutdown drain all transfer verbatim. An abandoned future now leaves the ent's COMMIT_AND_FETCH parked until the DMA can no longer land (bounded stall, self-healing) instead of re-arming the buffer under an in-flight SQE. New loom model weakening-verified: removing `release()`'s SeqCst fence reproduces the missed wake deterministically. Full 55-model suite green | `908dd41` (red) → `2fbc434` |
| **PERF-4** | **LANDED** — probe-gated `SINGLE_ISSUER`/`DEFER_TASKRUN` ladder, one `io_uring_enter` per pass, and the ≤64 KiB bounce pool made slab-backed and **registered as one fixed buffer** (`ReadFixed`, no per-op page-pinning). Honest geometry verdict: the 4 MiB pool is **deliberately not registered** — pinning GiBs to save a cost already amortized across 4 MiB DMAs is a bad trade. Local bench: `read_64k_qd1` **179.9 → 23.8 µs (−87 %)** with variance collapsing 129–244 → 22.5–25.0 µs; `read_4k_qd16` **−78 %** | `0b5ff3c`, `7cdca19`; `benches/nvme_issue_bench.rs` |
| **TEST-1 + DUR-2 + DUR-1** (3× P0) | **LANDED** — the durability spine. **TEST-1**: `src/dev_power_cut.rs`, a real data-device power-cut harness at the `NvmeBlockDev` worker (journals prior bytes per write, reverts everything uncovered by a completed barrier) plus the barrier-epoch observer DUR-3 needs. **DUR-2**: `UringRequest::Fsync{DATASYNC}` + `NvmeBlockDev::flush()` coalesced through the existing `SyncCoalescer`, and a VWC probe published as `data_volume_write_cache` (sysfs-derived, `unknown` treated as volatile). **Buffered-degrade resolved as (a): fail loud** — the O_DIRECT→buffered fallback is deleted, since it was a data-plane mode nobody measured in which nothing ever flushed. **DUR-1**: the block set is collected *before* the memory flush (union of RAM buffers + the ino's staged `active_block:` prefix), the fsync staging leg escalates to a durable upload, and `try_join!` is gone so the data barrier strictly precedes the metadata barrier. **The matrix was falsified before landing**: stubbing `flush_data_devices` to a no-op fails all six full-exposure rows on lost acked bytes while both zero-exposure controls still pass. Two spec [A] findings verified **benign** (the `sync_key` hash-coincidence reading does not hold; `MS_ASYNC` is covered by the shard `sync_all`). One existing test asserted the defect verbatim and was corrected | `d9cba84`→`fc16ebb`, `3501c65`→`e79acb4`, `68b3f98`→`0770acc`, `a1f8baf` |
| Integration fixes (cross-agent) | **LANDED** — five seams none of which the rebase caught, all found by post-merge `cargo check`: `over_uring.lock()` vs PERF-2's `OnceLock`; `QueueHandle` missing MEM-1's lease fields in PERF-5's sim venue; 4× `pool.recycle()` after ENG-1 made it `unsafe`; `ActiveReq` missing `dest_token` in DUR-2's Fsync arm; VAL-1/VAL-2 tests on the pre-S0 `DlmClient` API | orchestrator |
| Integration fix (cross-agent) | **LANDED** — VAL-6's tests referenced `tiering::dht::ClusterSecurityConfig`, which ENG-13 relocated to `tiering::cluster_tls`; repointed at merge | orchestrator |
| **ENG-2** (P0) + **ENG-7/8/16/17** | **LANDED** — the gate regime got its enforcement points. `cargo audit` is in `task check` over **both** lockfiles (the excluded fuse3 fork's own lock had never been audited and still carried a vulnerable `event-listener`) with `--deny unsound --deny yanked`; the three open advisories were closed **by version, not adjudication** (scc 3.8.3→3.8.6 with the REQUIREMENT moved to the patched 3.8.4 floor, crossbeam-epoch→0.9.20, event-listener→5.4.2 in both locks) leaving **0 vulnerabilities, 2 unmaintained warnings** (§5). ENG-7: the repo had NO CI at all — `gate.yml` (task check, path-ignoring docs), `docs.yml` (the docs-class tier, which had never had an implementation either — `tests/check_markdown_links.sh`), `nightly.yml` (audit on a schedule, because advisories land without commits); the TEST-2 skip ledger is uploaded and summarized so a hosted runner says "NO live-mount evidence" instead of a phantom green, and `require-mount` runs where the mechanism exists. ENG-8: the SHIPPED default-features configuration is linted for the first time, and a `dhat-on` build now says so in `--version`, at mount, and on `squeezefs bench` (it has no jemalloc and no `dirty_decay_ms`). ENG-16/17: `publish = false`, `rust-toolchain.toml`, `--locked` on the host build, and `task check:fuse3` — the fork's 105-test suite ran by hand at merge time before this | `99a2affc`, `9e895b98` |
| **ENG-10 + ENG-11** | **LANDED** — one env-knob convention over ~117 knobs, enforced rather than described: a shared pure parser (`crates/squeezefs-ipc/src/env_knob_core.rs`, `#[path]`-shared into the root crate, the fuse3 fork and the shim) plus a REGISTRY + startup refusal gate (`src/env_knobs.rs`) called from `main` before anything is parsed or mounted. A malformed, out-of-range or retired knob refuses the process naming every offender; an unregistered `SQUEEZEFS_*` name is announced as a probable typo but never refuses (mixed-version fleets and the shim share the environment). The 19 presence-based booleans (`SQUEEZEFS_FREE_FORENSICS=0` *enabled* forensics), the 4 `panic!` sites (= `abort` under the release profile — a typo killed the mount) and the 3 loud-error sites now agree; every DEFAULT is unchanged and pinned knob-by-knob. Inode reclaim renamed out of the block-reclaim prefix (`SQUEEZEFS_INODE_RECLAIM_{BATCH,WINDOW_MS,CONCURRENCY}`), old spellings refusing loudly with their successors. **The census test is the durable part**: a knob literal that is not registered FAILS the gate, so the next knob cannot ship undocumented. ENG-11: `SQUEEZEFS_IPC_ALLOW_DEV` announces on both ends instead of relaxing the KD-7 skew gate silently | `04ad8bbd`; 20 tests |
| **ENG-15** | **LANDED** — the remaining AGENTS.md drift: the stale L4 `clamp(cpus/4,2,8)` service-thread figure (the constant a perf campaign ran to DELETE, still contradicting the correct `…,2,16)` 400 words later), the `src/crypto_compress.rs` claim of "RSA-wrapped symmetric encryption" (KW-1 replaced RSA with HKDF-SHA-256 → the volume's AEAD — a security-relevant claim, now naming `docs/design-key-handling.md` and the do-not-reintroduce rule), the `cargo audit` mandate with no enforcement point, the full-gate block (both copies) which listed neither the shipped-config clippy line nor the fork leg nor audit nor CI, and **this wave's undocumented stats families** — 12 counters verified against the stats JSON one by one (`data_volume_write_cache`, `data_dma_fence_refusals`, `job_worker_panics`, `detached_task_panics`, `invariant_tripwires`, `lease_retry_{waits,exhaustions}`, `writeback_errors_latched`, `transport_unparked_commits`, `lseek_holes_reported`, `meta_parent_scans`, `readdir_parent_memo_hits`) plus the two R5 eviction-channel components | (this branch) |
| **§6.12 doc honesty** | **LANDED** — README and AGENTS.md now state the shipped concurrency scope (one write mount per volume set, enforced), mark TTL/renewal/global-fencing as unimplemented with their landing stages, and require scale claims to cite an evidence tier | `9ed7b3e` |
| **FUSE-3g** (correctness) | **LANDED** — `data_ref` was sized from kernel-supplied `in_header.len`: below 40 the subtraction UNDERFLOWED (release profile ⇒ a ~2^64 slice length and a panic on the DISPATCH task, i.e. every in-flight request on the session losing its reply at once), and above `40 + filled` the slice ran past the delivered bytes into whatever the previous request left in the reused session buffer. `validated_body(header_len, filled, payload_len, buf_len)` is now the only sizing path (`ReadResult::Request` carries `filled`), refusing with EINVAL on the dispatch path and a failed mount on INIT. The §5.4 zero-copy WRITE shape is explicit rather than incidental: lease bytes count toward `available`, the returned slice stays bounded by `filled`. **Live case found:** a short-delivered `fuse_write_in` used to deserialize its size/offset/fh out of the previous request's tail | `b3ba77b0` (red) → `6803afbe`; 4 legs + `delivery_bounds` bench group |
| **FUSE-3f** | **LANDED** — `IORING_FEAT_NODROP` probed per queue ring (probe, never a version check) and `cq.overflow()` read with the same `cq.sync()` that publishes the tail, because a dropped completion is by definition invisible in the CQEs. `CqDropWatch` tracks a WRAPPING delta (the kernel field is a plain-store u32 — a wrap must not report 4 G phantom drops), reports each loss once, and charges the must-stay-0 `transport_cq_overflows`; `transport_cq_nodrop` records the kernel capability. The geometry (`cqsize = sq*2`, worst pass `depth*2+1`) is why it should never fire — the counter is what makes that an observation instead of an assumption, and it composes with FUSE-2's `transport_slots_overdue` (a dropped REGISTER/COMMIT stalls its ent) | `04cf32c5` (red) → `065cd27d`; 4 legs |
| **FUSE-3j** | **LANDED** — the teardown lease drain was `1 ms` sleep-polling up to `100 ms` PER PARKED ENT, SERIALLY: 3.2 s per queue at depth 32, on every umount, for a signal the lease drop already delivers. `drain_await_leases` waits ONCE for the whole queue on the queue eventfd (`DRAIN_LEASE_BUDGET`), keeping the loop's loom-verified drain→disarm→scan order; never-parked messages go through `try_commit` first so `parked` is published for every ent waited on. The reply-BODY discard on budget expiry is structural, not a wait-shape choice — §5.4 forbids writing a leased payload region at shutdown as anywhere else | `04cf32c5` (red) → `065cd27d`; 3 legs (shared-budget, eventfd-driven, no-wait) |
| **FUSE-3c** | **ADJUDICATED — UNIMPLEMENTABLE AS WRITTEN, concern covered elsewhere.** The spec asks the REGISTER barrier to count `depth` successful REGISTER CQEs before a queue is "registered". A REGISTER's CQE is not a registration ack: the kernel completes it only when it DELIVERS a request on that ent (`fs/fuse/dev_uring.c` — `fuse_uring_cmd` parks the ent in `FRPS_AVAILABLE` and the CQE is posted with the delivered request), and requests are only routed to the ring once `is_ring_ready()` sees every queue armed. Counting completions would therefore deadlock the arm: no readiness ⇒ no delivery ⇒ no CQE ⇒ no readiness. Submission-counting is structurally necessary. The real concern — a REGISTER the kernel REFUSED going unnoticed — is covered by FUSE-3a's error path: a refusal arrives as a negative-result CQE, is retried with bounded exponential backoff, retires the ent after K failures (`transport_ents_retired`), and fails the session when all ents retire. Confirmed independently by this agent against the current tree | reasoning first recorded by the FUSE-2 agent; re-derived + recorded here |
| **FUSE-3k** | **LANDED** — wire half: `fuse_forget_one.nlookup` was parsed into a field literally named `_nlookup` and discarded, and `Filesystem::batch_forget` took `&[Inode]`; both now carry `(inode, nlookup)`. Daemon half: `OpenEntry.lookups`, incremented at every entry reply (LOOKUP incl. the `.`/`..` arm, CREATE, MKNOD, MKDIR, SYMLINK, LINK, all three READDIRPLUS arms) and returned by `return_lookups`, with eviction of daemon-side per-inode state (attr cache, inode lock, RES-13 side maps, reclaim enqueue) at ZERO instead of on every forget — which is the kernel's own contract (`fuse_evict_inode` returns the whole accumulated count; `fuse_force_forget(1)` returns one reference and keeps the inode). Deliberate fallback: an UNTRACKED ino still evicts on its first forget, so a miscount can only cost cache retention. The entry is removed at zero, so RES-13's growth class does not return | `b34a7626` (red) → `e3132a8d`; 4 legs |
| **POSIX-14** | **LANDED (verdict: no handle table).** `fh = inode` is not the defect: the daemon keeps ZERO per-open state to key — write custody is per-inode by design (DLM leases per ino, `BLOCK_FLUSH_LOCKS` per block, the D1.d dirty bit per inode) — so allocating handles would add a table needing the same reconciliation plus a lookup per op and buy nothing. The defect is the `open_count` VETO: its only decrement is a RELEASE, so one lost RELEASE (the handler-task panic FUSE-2 now answers with a synthesized reply) vetoes `queue_reclaim_inode` for the mount's life. FUSE-3k gives the proof point: reaching zero lookup references is the kernel certifying it holds no reference to the inode, which it cannot do while any `struct file` on it is open — so a nonzero open count there is a PROVEN lost RELEASE. Now reported loud + counted on the must-stay-0 `open_count_stranded`. Deliberately NOT zeroed: reclaiming on bookkeeping we already know is wrong is exactly how generic/795 destroyed a live fd's data. **Residual (assigned to POSIX-15's sweep):** the stranded orphan's space is reclaimed at the next mount, not at detection | `b34a7626` (red) → `e3132a8d` |
| **FUSE-4b** | **LANDED** — `FUSE_EXPORT_SUPPORT` is advertised, so the kernel encodes `(nodeid, generation)` into NFS handles and ESTALEs on mismatch; every reply hardcoded `1`, so a handle minted before a `format` resolved against the same ino in the NEW filesystem — a different file. The generation is now derived at mount from the volume-set generation identity (v3 superblock uuids joined in volume order), folded to the `u32` the kernel stores and never 0 (`fuse_get_dentry` SKIPS the check for 0), and carried by all 12 entry-reply sites. Ino reuse cannot break it from the other side (v3 allocates monotonically). In-RAM/test mounts keep `1`. The `..`-fabrication half was already closed by POSIX-4 | `fea1d117` (red) → `c63d99c8`; 2 legs |
| **FUSE-4c** | **ADJUDICATED + GUARDED** — `FUSE_CACHE_SYMLINKS` needs no invalidation path because a symlink target can never change: it is written exactly once in the POSIX-3 create transaction, POSIX has no retarget call, the VAL-2 allowlist refuses `system.symlink` through setxattr/removexattr, and v3 never reuses an ino (so a cached page cannot be re-pointed at another file's target either). Recorded at the negotiation site and enforced by a grep guard (the R-6 unified-purge precedent): a second writer of that record fails a test instead of silently making the flag a lie | `fea1d117` (red) → `c63d99c8` |
| **FUSE-4d** | **LANDED (documented independence)** — `negotiate_max_readahead` echoes the kernel's limit VERBATIM (a clamped echo would cap kernel readahead for the mount's life, invisibly) and publishes it as `transport_max_readahead`. The independence is recorded on both sides: `max_readahead` bounds the KERNEL's per-file readahead requests; R2's prefetch window is a device-side pipeline depth (measured bandwidth × latency, R5-clamped) — different resources in different units, so coupling them would throttle one plane by the other's unrelated bound. The dead `max_readahead=4194304` mount-string token (stripped by fuse3's own filter before the kernel saw it) is deleted | `fea1d117` (red) → `c63d99c8` |
| **FUSE-4e** (correctness) | **LANDED** — `get_payload_buffer` always returned `(ptr, len)` and the read handler dropped the length (`.map(|(ptr, _sz)| ptr)`); `RangedDest.cap` was set from the REQUESTED slice length, so the bound was a copy of the thing it bounded. The only thing between a serve and a heap overrun was the kernel honoring the `max_pages` the INIT reply advertised. `ReadDest { addr, cap }` now carries the transport's own length (and the il arena override's), and `checked_ptr(len)` is the only way to the writable pointer: once at serve entry against the request `size` (which bounds every leg — each writes its own request-clamped length) and again at the two legs that hand the window to a writer that is not the serve itself (the §5.6 `RangedDest` DMA offer, the MEM-2 assembly fan-out). A non-fitting serve is refused, not truncated and not fatal: the dest drops to `None`, the copy path serves the read, and `read_dest_overruns` (must stay 0) records the breach | `7bd747d3` (red) → `2a4fa891`; 3 legs + `read_dest_bound` bench group |
| **MEM-4** | **LANDED (completed)** — ENG-1 had closed the two clippy-flagged sites; the three `pub` types the spec names now all refuse safe construction: `cache::pool::UringBufOwner` (private fields + `unsafe fn new`, with every read-path view funneled through ONE constructor — `routing::dest_bytes`, 17 struct literals collapsed into it), `routing::RangedDest` (private fields + `unsafe fn new` + accessors), `zcrx_lane::area::AreaSlice` (`unsafe fn new` — its `as_slice()` dereferences the caller's pointer). The pin is compile-time: `tests/unsound_api_surface_tests.rs` constructs each the sanctioned way under `#![deny(unused_unsafe)]`, so a re-exposed safe constructor fails `-D warnings` | `7bd747d3` (red) → `2a4fa891`; 3 legs |
| **MEM-7a–e** | **LANDED** — **7a**: `sever`'s page-alignment/in-bounds precondition is documented AND enforced in every build (it was a `debug_assert!` plus a screen inside the single caller, i.e. nothing shipped); violation is loud-never-fatal per RES-22 and falls back to the pooled sever. **7b**: `map_shared_pmd_aligned` does its slack arithmetic in `round_up(len, page)`, so the trim `munmap` address is always page-aligned — the unaligned form was refused by the kernel and leaked up to 2 MiB of address space per mapping (the spec's "unmaps the last page" framing is what a round-DOWN fix would produce; both are the same missing precondition). **7c**: `ipc_direct`'s reaper no longer exits on an enter error with ops in flight (every pending op pins its session mapping, so exiting released memory kernel DMA could still land in); failures retry loudly on `ipc_direct_reap_stalls` (must stay 0), and after 5 000 consecutive failures it LEAKS the pending destinations deliberately rather than unmap under DMA or hang the shutdown join. **7d**: `read_unaligned` for the alignment-1 GDS args buffer. **7e**: `routing.rs` 23/32 → 0/32 and `nvme_dev.rs` 5/12 → 0/12 missing SAFETY comments; **tree residue 198/671**, concentrated in `crates/squeezefs-preload/src/interpose.rs` (68), fuse3 `tokio.rs` (33) and `fuse_over_uring.rs` (33), `src/cache/gds.rs` (13), `src/main.rs` (13) — reported rather than padded | `cc627d27` (red) → `1804742f`; 6 legs |

### Wave 1 (in flight)

| Item(s) | Agent | Branch |
|---|---|---|
| §6.11 repro, DLM **S0**, **S1** (closes RES-2) | DLM-1 | `feat/dlm-s0-s1` |
| PERF-6/FUSE-4a, PERF-2, PERF-5 | PERF-A | `perf/transport-economy` |
| PERF-7 (shim lseek) | PERF-B | `perf/shim-lseek` |
| MEM-3 (D5 gate link 1), PERF-1/Z3 gather fusion | PERF-C | `perf/zcrx-z3` |
| MEM-1 (P0), PERF-4 | PERF-D | `perf/read-fill-economy` |
| MEM-2 (P0), RES-9 | MEM-2 | `fix/assembly-task-ownership` |
| ENG-1 (gate re-engage), ENG-5/6/9/12/13/14, MEM-6 | ENG-HYGIENE | `chore/eng-hygiene` |
| ENG-3, ENG-4 | ENG-OPS | `fix/audible-daemon-and-wipe-guard` |

### Wave 2 (in flight)

| Item(s) | Agent | Branch |
|---|---|---|
| VAL-1 (P0), VAL-2 (P0) | VAL-INPUT | `fix/val-ioctl-and-xattr-allowlist` |
| VAL-4 (P0), VAL-5a–e (P0), RES-5 (ipc half) | VAL-IPC | `fix/val-ipc-bounds-and-shim-authn` |
| VAL-6 (P0), RES-5 (wire half), RES-16 | VAL-WIRE | `fix/val-job-wire-bounds` |
| POSIX-1, POSIX-2, POSIX-3, POSIX-4 | POSIX-A | `fix/posix-statfs-sparse-symlink` |

### Blocked by file ownership (next wave)

| Item(s) | Blocked on | Why |
|---|---|---|
| TEST-1 → DUR-2 → DUR-1 (the durability spine, strictly ordered) | PERF-D releasing `src/nvme_dev.rs` | The power-cut harness seam and the `Fsync{DATASYNC}` op both live in the NvmeBlockDev worker |
| FUSE-2 ⊕ PERF-16 (exactly-one-reply + delete the pending map) | PERF-A releasing `fuse_over_uring.rs` | PERF-5's ring-setup change touches the same file; the joint design replaces the map wholesale |
| DUR-3, DUR-4, DUR-5, DUR-6, DUR-7, DUR-8a–f | TEST-1 | Untestable without a data-device power-loss harness |
| VAL-3 + KW-1 (key off-volume, post-RSA wrap) | ENG-OPS releasing `config_ops.rs` | Format-config persistence path; also carries an on-disk change for the batched window |

---

## 2. Guarantee table by evidence tier (ruling D1)

Every claim cites its tier. **(i) measured-real** — rows from real mounts at lab scale. **(ii) measured-simulated** — the SIM-1 harness (15 k client state machines, no data plane). **(iii) arithmetic-on-measured-constants** — per-shard/per-volume ceilings composed, formula published.

| Capability | Shipped guarantee | Tier | Status |
|---|---|---|---|
| Single writer, single mount | Full POSIX; whole-tx atomic metadata; torn-write immune | (i) | Shipped today |
| Concurrent mounts of one volume set | **Refused** (D0 flock + PR where available) | (i) | Shipped today — the honest current statement |
| One writer + N coherent readers | Target: readers take no leases; bounded staleness = one checkpoint interval | — | DLM **S5** — the first multi-client ship |
| Multi-writer, disjoint file sets | Target per D1 (AI-training mixed workloads) | (i)+(ii)+(iii) | DLM S8–S11 |
| Cross-host arbitration on non-PR substrates | Detection-grade only; multi-writer refuses to arm | (i) | Design law, enforced |

**Note for the public RC:** the current cluster's namespaces expose no `resv_enable` knob, so the reset-v5 venue itself runs PR-less — every multi-writer claim must therefore cite a PR-capable substrate, and the refusal path is what the lab venue exercises.

---

## 3. Format / incompat ledger

| Bit | Name | Stamped | Notes |
|---|---|---|---|
| 0 | `KV_V3` | at format | The only metadata format |
| 2 | `KV_GUEST_SLOTS` | on use | VL5b |
| 3 | `KV_VOLUME_LIFECYCLE` | at format | VL3 |
| 5 | `KV_LAYOUT_DELTAS` | first delta-class save | Write-commit economy |
| 6 | `KV_DYNAMIC_ROUTING` | at format, presence-REQUIRED | Field validation still owed — reset-v5 window. Verified in the field 2026-08-02: pre-bit-6 binaries refuse loud, as designed |
| 7 | `WriterClaim.term` | — | DLM **S2**, batched into the one reformat window |
| 8 | `KV_PARTITIONED_APPEND` | — | Multi-writer §6.2 items 2/3/4; built, never stamped (ruling D9) |
| 9 | `KV_BLOCK_REFCOUNTS` | — | Multi-writer §6.2 item 1; built, never stamped (D9). Renumbered from 8 after a same-wave collision — two definitions of one bit is silent aliasing, so every claim now carries a disjointness assertion |
| 10 | `KV_WRITER_SCOPED_STAGING` | — | Multi-writer §6.2 items 8/10 (keys **and** the node-scoped generation stamp share one bit: a half-engaged state is unsound in both directions); built, never stamped (D9) |
| 12 | `KV_INO_LANES` | — | Multi-writer §6.2 item 5 — per-writer ino lanes; built, never stamped (D9) |
| 13 | `KV_BLOCK_KEY_INCARNATION` | — | Multi-writer §6.2 item 6 — `offset ‖ incarnation` block keys; built, never stamped (D9). Both authored themselves at 10/11 and renumbered at integration: the **third** parallel claim, and the second the union pin caught on contact |
| 11 | `KV_MULTI_WRITER_DATA` | — | DLM **S7**'s capability gate — `SQUEEZEFS_MULTI_WRITER=1` refuses a format without it. Renumbered from 10 at integration (the second parallel claim; this one was caught by the disjointness pin's union clause going red, not by reading a diff). Built, never stamped (D9), so the knob currently refuses on every real volume — the honest posture |
| — | Post-RSA key wrap (KW-1) | — | Ruling D3; same window |
| — | Sharded indirect map (DUR-6 ⊕ PERF-9) | — | Same window |

Stamping is unanimous per volume SET for bit 10 (a half-stamped set stays
unscoped) and requires the Phase-8 window for all four unstamped bits.

---

## 3b1. DLM stage board (spec §6.9)

| Stage | Ships | State | Note |
|---|---|---|---|
| S0 | `LockManager` trait; mock family deleted | **LANDED** | Closed most of ENG-14 |
| S1 | Global `grant_seq` replaces `FENCING_MAP` | **LANDED** | Closed the unbounded-growth item |
| S2 | Durable `WriterClaim.term`, composed tokens (bit 7) | **LANDED** | Fencing is remount-monotone |
| S3 | `cluster_wire`; `job_wire` ported onto it | **LANDED** | Closed VAL-6 structurally (§3a). Mover-shard decode 32.6 µs → 771 ns; the measured RTT floor is what prices S8 |
| S3.5 | Cross-volume transaction machinery (ruling D4) | in flight | DUR-7 (a **P0**: link/unlink/dir-rename across volumes leak the inode *and all its blocks* permanently on a crash between the two commits) is its first consumer. S8 now depends on it twice: for cross-owner verbs and for durable exactly-once across an owner failover |
| S4 | Slot lock manager, solo mode | **LANDED** | `is_local_slot` is the ownership extension point; `dlm_rpcs == 0` by construction in solo. **Measured half of the gate deferred per D11** |
| S5 | Read-only coherent client mounts | **LANDED** (metadata half complete) | `-o ro`; three layered write gates; metadata is **bounded staleness** (≤ poll interval + the writer's ≤1 s checkpoint ceiling, published as `reader_staleness_bound_ms`), not a snapshot. **The DATA half is bounded, not eliminated** — within one interval a freed-and-reallocated block can serve another file's bytes, loud under AEAD but **silent on passthrough, the default**. That window is §6.8 item 3 |
| S6 | Membership off the journal (lease-based liveness) | in flight | Also carries §6.2 item 7 and the non-write reader→writer channel item 3 needs |
| S7 | Data-plane custody-epoch fence + dead-epoch quarantine + WERO | **LANDED** (in-process half) | One authorization point; quarantine release needs a drain proof; pressure answer is ENOSPC. **Device-rejection gate DEFERRED** and specified as an executable leg in `docs/design-nvmeof-target-management.md` §6.8.1 — a counter alone does not meet it |
| S8 | Metadata function shipping | **LANDED, not armed** | `src/meta_ship/` — verbs, ownership routing, batch pipelining, owner-side dedup. **No production arm on purpose**: `arm_ownership` has no caller, because a mount that ships metadata but cannot ship *data* custody is not a product (the data path correctly refuses a foreign-home lease at the S4 gate). S9 owns the arm and the non-trait publish surface the FUSE daemon actually uses. S4's contract #2 resolved by a **client token cache fed only by grants piggybacked on shipped replies** (a per-read round trip was excluded by §6.5 item 1's ≥ 99.5 % locally-served requirement against ~24 reads per write); a miss serves the owner era's base — same as a fresh local mount returns for an ungranted object, so stale still classifies stale — and trips a must-stay-0 counter, because a miss *means the intent-lock property was violated*. Cross-**owner** shapes refuse `EXDEV` naming S3.5; one-owner cross-volume ops are unchanged. Idempotency is request-id + owner-side dedup window (a duplicate awaits the winner's own outcome), **exactly-once within an era, at-least-once across a failover** — durable exactly-once needs S3.5's intent records and was not faked. Ownership granularity is the **volume**; an intra-volume split is unrepresentable until bit 8 is stamped and the node cache gains its third gate state |
| S9 | Multi-writer data plane | **OPEN** | Builds on S7's `authorize_dma` epoch contract |
| S10 | Subtree delegation + client-owned-slot placement | **OPEN** | Ruling D10's recovery for S8's serial latency |
| S11 | Byte-range custody + the W1 seventh clause | **LANDED** | `patch_ineligible_range_shared` keeps the W1 ledger honest |

Reader status, stated honestly: **one writer plus N readers is shipped for
metadata**; the remaining reader work is the data-plane grace period (item 3,
blocked on S6), reader visibility in `squeezefs clients` (S6), and the measured
capability row (deferred per D11 — it needs the tcp substrate or a real fabric
and a sustained ≥ 60 s window).

---

## 3b2. Multi-writer §6.2 item board — 8 of 10, plus the runtime item

Spec §6.2 ranks ten durable single-writer assumptions, plus one runtime item it
calls "arguably harder than any of the ten". Status after the 2026-08-03/05
wave (this board exists because the work is partitioned across agents, and an
item nobody claims is indistinguishable from an item nobody needs):

| # | Assumption | Status | Landed as |
|---|---|---|---|
| 1 | Block refcounts/free list have no on-disk representation | **LANDED** | Per-reference backpointer records (`TREE_BLOCK_REFS`), bit 9. Chosen over a count (RMW race — two inodes cloning one block share no lock) and over a delta (cannot express the first reference); fsck class **C8** ungated as a must-stay-0 tripwire |
| 2 | One journal ring head per volume | **LANDED** | Per-appender sub-rings, appender id in two of the four zero pad bytes §4.1 already reserved (inside the existing checksum), bit 8 |
| 3 | One A/B extent bitmap + one `advance_durable` tail | **LANDED** | Page-partitioned interleaved bitmap with per-partition durable clocks, bit 8 |
| 4 | One A/B root ledger, `slot = seq % 32` | **LANDED** | Per-writer slot ranges (≥ 2 slots each ⇒ appenders cap at 16), bit 8 |
| 5 | `next_ino` is a per-mount atomic over a shared namespace | **LANDED** | Per-writer ino lanes (bit 12) on the VL5b `slot_cursors` precedent; `next_ino()` is now the lane-dominating accessor and the raw atomic is private, so no path can under-declare a lane |
| 6 | Block keys are bare reusable device offsets | **LANDED** | `offset ‖ incarnation` (bit 13) — a stale process-local binding is now structurally detectable. The lifetime is minted at exactly ONE site (`claim_block_idx`, because an allocation is the only event that starts a new lifetime) and the stamp is deliberately NOT cleared on free, so a freed-but-unreclaimed offset still validates its own key |
| 7 | `writer_claim` is singular (expresses exclusion, not membership) | **OPEN** | Claim-set record + NVMe registrants. Pairs with S6 (membership) and must not collide with S7's WERO work on `reservation.rs` |
| 8 | `active_block:`/`active_block_ext:`/`mapping:` keys have no writer scope | **LANDED** | Trailing `:w_{16 hex}` component *after* every identity component, so historical scan prefixes keep their exact meaning — a foreign record must be SEEN to be classified. Keys carry identity, values carry currency (the fencing token stays in the value) |
| 9 | Layout-delta chains name their base with a process-local token | **OPEN** | Durable per-ino layout version. Touches the `routing.rs` publish path, so it waits for item 5/6 to land |
| 10 | Staging generation is the volume-set uuids only | **LANDED** | `{set}@node:{16 hex}` from a host-stable identity (machine-id, app-specific-hashed so the raw id never lands on disk). The D0 claim id was rejected: a successor mount would classify its own predecessor's crash residue as foreign, inverting staged-crash recovery into data loss |
| — | KV node cache is load-once RAM-authoritative | **LANDED** | *Partitioning, not cache coherence*: `apply_locked` is the one choke point, non-authority structural mutation refuses loud, and a peer's append into a cached tail is detected at `append_frozen` rather than silently overwriting acked records. Named residual: a leaf a peer wrote in an *earlier* window that the authority cached before that window closed leaves evidence nowhere — closing it needs a third gate state ("reader for structure, appender for my own leaves") |

One item remains — **9**, a writer-side publish concern (durable per-ino layout
version). Item 7 (the claim-set record) is being built inside S6, where the
membership primitive belongs. Neither gates a reader.

Two consequences of items 5/6 that later stages must honour: a **reader mount
never engages minting** (it requires `writer_term() > 0`), so an S5 reader grows
`block_key_incarnation_unknown` by construction — that counter is the measured
size of the S9 gap, **not** a fault, and must be exempted from any alarm, while
`refusals`/`exhausted` stay genuine must-stay-0 tripwires. And the writer scope
(a meta key NAME component) must never ride a block-key VALUE, nor the lifetime
a key name: the W1 predicate classifies by counting `:`-components after `://`,
so a stray scope suffix on a value silently flips whole-block eligibility off.

---

## 3c. Known-red at dev during the DLM push (D11 bookkeeping)

Ruling D11 parks suite runs until N readers + N writers work, so failures found
incidentally by agents are recorded here rather than fixed on sight — with one
exception class: a red caused by our own landing gets fixed immediately, since
it is merge debt, not pre-existing debt.

| Test | Cause | Disposition |
|---|---|---|
| `kv_backend_tests::v3_unknown_incompat_bit_refuses_naming_it_and_unknown_ro_does_not` | The case hardcoded `1 << 9` as "a bit this binary does not know"; bit 9 became `KV_BLOCK_REFCOUNTS` | **FIXED** (`9268d961`) — the probe bit now derives from `FEATURES_INCOMPAT_KNOWN`, so it pins the property and cannot rot as later stages claim bits |
| `mem_budget_tests::advisory_integration_phases` | "the paused victim must NOT reach the tier" — reported identical at base by two independent agents | Open; triage in the deferred stack. Suspect the R5 shed-channel semantics landed with the eviction-channel components, not a DLM regression |
| `writeback_tests::test_write_block_from_staging_transform_leg_drops_guard_before_dma` | An LZ4 image declares 65,536 B against a 4,096 B block bound — a process-global block-size ordering effect inside that suite | Open; triage in the deferred stack. If the global is genuinely order-dependent it is a test-isolation bug, and the fix belongs with TEST-3's `poll_until` sweep |

---

## 3a. VAL-6 closed structurally by DLM S3 (2026-08-05)

VAL-6's interim hardening was, by its own framing, bounds on a transport that should not have existed. **S3 `cluster_wire` replaced it and `job_wire` was ported onto the new wire**, so the item is now closed by construction rather than by patching:

- **The codec** — `serde_json` → bincode. The 64-checksum mover-shard decode VAL-6 measured at 32.6 µs against §6.5's 10 µs custody budget is now **771 ns (21.6×)**, 7.7 % of budget. Frame bytes 4,043 → 1,542. The A/B lives *inside* the bench group (`*_json_control` rows run the retired codec over identical frames in the same process), so the ratio reproduces from one `cargo bench` rather than resting on a remembered number.
- **Authentication now survives the handshake** — VAL-6 explicitly left per-frame authentication to S3. Every session frame carries `HMAC-SHA256(session_key, direction ‖ sequence ‖ length ‖ body)`; tamper, reorder, replay and reflection are each refused mid-session and each pinned. Under mTLS the key mixes in the RFC 5705 exporter, so it is channel-bound. The nonce is consumed **after** the MAC check, so a peer that cannot produce a valid proof can never spend an honest connection's challenge.
- **The accept-everything verifier is now UNREPRESENTABLE**, not merely deleted: `cluster_tls` constructs rustls configs only from a complete CA, so "TLS that authenticates nobody" cannot be built. **This is why exactly one VAL-6 leg changed outcome** — `unauthenticated_tls_is_plaintext_class_for_the_ladder` *described the verifier existing*, so keeping it green would have required keeping the verifier. It became `ca_less_tls_is_refused_not_admitted_as_a_lesser_class`. The other 26 legs kept their original assertions; 27/27 green.
- **Honest cost accounting:** S3 also *added* work, and reporting only the win would misrepresent it. The full authenticated custody frame (encode → MAC → verify → decode, both directions) is **2.97 µs** — where the retired JSON *decode alone* cost 16.6–32.6 µs.
- **DISC-1 landed** (ruling D2): peers auto-discovered from `client:{uuid}` records — the shared volume is the rendezvous, no multicast and no seed list. The interim form's cost is stated: `listxattr(1)` + one `getxattr` per record per volume, against an ino-1 hotspot §6.5 item 3 measures as saturating at ~4,550 clients; **S6** moves it onto lease-based liveness.
- **Knob:** `SQUEEZEFS_CLUSTER_WIRE_SVC_THREADS` (registered per ENG-10, derived `clamp(cpus/8, 1, 8)`, clamped to the core count). The listener family deliberately keeps its `SQUEEZEFS_JOB_WIRE_*` spelling — same listener, and docs/evidence key on it.

### The RTT row — the number that prices S8 (ruling D10's input)

Loopback **floor**, qd1, engagement exact: **9.33 µs** median at 0 B (2,000 samples), 12.58 µs at 5,000, **31.94 µs** at 4 KiB. Substituted into §6.5 item 1's own formula:

| Added RTT | Create throughput | Cost |
|---|---|---|
| 11 µs (this floor) | 9,090 → 8,264/s | **~9 %** |
| 50 µs | — | 31 % |
| 150 µs | — | 58 % |
| 250 µs (§6.5's figure) | 9,090 → 2,778/s | 69 % |

**Verdict: the wire's own overhead is not what decides S8 — the fabric is.** R1's fallback statement ("remote clients are throughput-oriented; latency-sensitive metadata work runs on the owner") becomes correct somewhere between the 50 and 150 µs rows. No fabric number is fabricated: the instrument ships in-tree (`cluster_wire::measure_rtt`, plus an `--ignored` harness that authenticates as any peer does), and the evidence note enumerates what the real row needs — two real hosts on the production NIC, both channel classes, medians of 3, A-B-B-A, a sustained ≥ 60 s row, exact counter closure with `mac_failures == 0`, and a pipelined row for S10 to beat.

---

## 3b. Deferred with a recorded rationale (not dropped)

### Peer-to-peer block fetch — deferred to post-S5, revisit with evidence

**Status:** the subsystem was deleted 2026-08-02 (ENG-13) because it was structurally unreachable — `P2pServer::run` was never called, `dht_node` was never set, and it carried an accept-everything peer certificate verifier. Its shared TLS core survives as `src/tiering/cluster_tls.rs`. All doc references are now marked historical.

**Why it is not simply dead (user question, 2026-08-02).** Peer fetch is a *bandwidth-aggregation and target-offload* mechanism, not a latency optimization. The measured economics on the current venue argue against it, and the reason is specific: nvme-tcp read RTT is **235–280 µs** against **memory-backed null_blk targets** — so the "storage node" is already a peer's RAM, reached by the fastest available path (kernel nvmet, no userspace hop). A peer fetch would ride the same wire and add a remote userspace daemon wake plus a copy. On a flat fabric with RAM-backed targets it cannot win.

**The conditions under which it does win**, all absent from the current rig:
1. **A topology gradient** — peer on the same ToR vs. target across an oversubscribed spine. Requires multi-rack; the venue's two IPs are two ports into one fabric, not two distance classes.
2. **Shared hot data** — N clients reading the same blocks: the target's NIC saturates at line rate while peers fan out. **This is the strongest argument and the user's ruling (2026-08-02) is that AI-training workloads do read substantially the same data** (datasets re-read every epoch across every worker; checkpoints write-once-read-many), which qualifies the D1 "rarely the same files" assumption for *block-level* reuse.
3. **Slower target media** — real SSDs with deep queues under load, where a peer's RAM copy beats a congested device queue even paying the extra hop.

**Reference class:** JuiceFS ships a peer cache, but over **object storage** (tens of ms, egress-priced), where any local peer wins trivially. Against 235 µs RAM-backed NVMe-oF the economics invert — the precedent does not transfer without re-measurement.

**Blocking costs.** (a) *Coherence*: §6.3's serve proof rests on process-local incarnation words and block-key bindings; peer-served bytes add a cross-node staleness vector where none exists today, and on a passthrough volume that failure is **silent** (transformed volumes fail loudly on the AEAD tag). This wants DLM **S5**'s freed-offset grace period and revalidation cadence underneath it — building before S5 means building twice. (b) *Security*: needs real authentication, though this cost has dropped sharply — `cluster_tls.rs` plus VAL-6's storage-trust enrollment (possession of the `job:enroll` secret ⇒ membership) is now a usable foundation.

**The experiment that would settle it** (hours, no product code): on a rig with a real topology gradient, measure a raw userspace TCP block-serve hop against the nvme-tcp target hop for the same block, plus an N-client shared-hot-block bandwidth row. If peer-hop ≥ target-hop on a flat fabric — the prediction — the feature is definitionally a topology-and-bandwidth play and must be justified on a multi-rack venue, never on this one.

---

## 4. Declared deviations (carry into user-facing docs)

| Deviation | Rationale | Reference |
|---|---|---|
| `noatime` semantics | By design; affects Maildir detection, `tmpwatch --atime`, `updatedb`, HSM agents, `find -atime` | fstests generic/003 + 192 adjudication; POSIX-17 requires naming the tool classes in `docs/operations.md` |
| Thin provisioning | By design | fstests generic/213 adjudication |
| Timestamp range ±9,223,372,036 s | i64 nanoseconds — the ext4-u34/xfs-bigtime finite-range class; daemon clamps durably. Incore clamping requires `s_time_max`, which mainline FUSE cannot advertise | fstests generic/634; kernel-sqz patch 0027 closes it on sqz kernels (v2, window-owed) |
| POSIX advisory locks are kernel-local | Full POSIX semantics per mount via `posix_lock_file`; cross-mount exclusion is D0's job | VL10 |
| Writable shared `mmap` across nodes | Not supported; must refuse rather than silently incohere | Spec §6.10 R9 |

---

## 5. Supply chain (ENG-2 — closed 2026-08-03)

`cargo-audit` was installed and run for the first time in the project's history on 2026-08-02 (`402ca77`): **5 vulnerabilities, 6 warnings**, and the AGENTS.md Phase-5 claim "no known vulnerabilities" had never been verified by anything. ENG-2 is now closed on both halves — the advisories are gone **by version, not by adjudication**, and the check has an enforcement point.

### The gate (the half that was missing)

`task audit` runs `cargo audit --deny unsound --deny yanked` over **both** lockfiles — the root workspace and the excluded `crates/fuse3` fork, whose own lock nothing had ever audited — and `task check` calls it, so it runs on every code-class commit and in CI (`.github/workflows/gate.yml`, plus a nightly `nightly.yml` pass because advisories land without commits). `unmaintained` deliberately stays a warning; the two current ones are adjudicated below.

### Residue at close (2026-08-03, `--deny unsound --deny yanked`)

**Root workspace: 0 vulnerabilities, 2 warnings. fuse3 fork: 0 vulnerabilities, 1 warning.**

| Advisory | Crate | Disposition |
|---|---|---|
| RUSTSEC-2025-0141 (unmaintained) | `bincode 1.3.3` | **Accepted for the RC, warning only.** Not a vulnerability. It is a session/wire and on-disk-adjacent codec: moving to `bincode 2.x` changes an encoding surface, which is a format decision with its own gate (fuzz targets + the decoder property mirror), not a dependency bump. Tracked as post-RC. |
| RUSTSEC-2025-0119 (unmaintained) | `number_prefix 0.4.0` | **Accepted, warning only.** Pulled by `indicatif` (progress-bar humanization) — leaf, no untrusted input, no security surface. Dies whenever `indicatif` drops it. |

### What was fixed to get there

| Advisory | Crate | Fix |
|---|---|---|
| RUSTSEC-2026-0098 / 0099 / 0104 (webpki name constraints ×2 + CRL panic) | `rustls-webpki 0.101.7` | **Died with ENG-5** — the EOL rustls 0.21 stack existed only because `hyper-rustls 0.24` (zero references) pulled it in. |
| RUSTSEC-2023-0071 (Marvin timing key recovery) | `rsa 0.9.10` | **Primitive replaced, not adjudicated** (ruling D3 / KW-1): the key wrap is now HKDF-SHA-256 → the volume's own AEAD, both from `ring` (`docs/design-key-handling.md`). `rsa` is no longer a dependency, and `Cargo.toml` carries a do-not-reintroduce note. |
| RUSTSEC-2026-0204 (invalid pointer deref in `fmt::Pointer`) | `crossbeam-epoch 0.9.18` | Lockfile bump to **0.9.20**. |
| RUSTSEC-2026-0221 (`!Send` tags across threads via `StackSlot`) | `event-listener 5.4.1` | Lockfile bump to **5.4.2** in **both** lockfiles (the fork's copy is why auditing one lockfile was not enough — it still carried 5.4.1). |
| RUSTSEC-2026-0205 (`Array::insert` not exception-safe ⇒ potential double free) | `scc 3.8.3` | **Bumped to 3.8.6, and the requirement moved to the patched floor `3.8.4`** so a future `cargo update` cannot regress below it. The upstream fix (3.8.4) replaced `mem::forget` with `ManuallyDrop` and split fallible from infallible work during a node split. **Reachability finding, recorded because `scc::HashMap` is load-bearing across the latch-free hot paths, not a leaf:** the unsoundness requires a **panicking `K::compare`**, and every scc key type in the tree is a primitive or a derive: `u64` (×20 maps), `String` (×11), `usize`, `u32`, `u16`, `&'static str`, `(u64, …)`, `Bytes`, `Ino` (= `u64`), `node_addr`, and `dlm::ObjectKey` (`#[derive(Clone, PartialEq, Eq, Hash)]` over `u64`/`Box<str>`). There is **no hand-written `Ord`/`PartialEq` comparator anywhere in the tree**, so the panicking-comparator precondition was unreachable even before the bump — which is why this was a warning, not an incident. |
| `spin 0.9.8` yanked; `rustls-pemfile 1.0.4` unmaintained | — | Gone with the 2026-08-02 dependency work (ENG-5 + the KW-1 stack change); neither appears in either lockfile now. |

**Standing rule:** a public RC ships with this table filled in. If a new advisory appears, it is fixed by version, replaced like `rsa`, or gets a written disposition here — the gate does not have an ignore list.

---

## 6. Evidence index

Populated as campaigns close. Current: `.benchmarks/2026-08-02-fuse1-init-ext.md` (FUSE-1 live A/B, reset-v5 venue) · `.benchmarks/criterion-baselines/reference.json` (99-median bench reference) · `.benchmarks/2026-08-04-*.md` (the five recovered campaigns).

**Open flakes** (TEST-7 class — every one poisons future counted runs; the multi-run discipline requires counts restart after each fix):

| Flake | Rate | Shape | Owner |
|---|---|---|---|
| `multi_queue_tests::storm::…_no_starvation` | was 2/5 on untouched tip, 5/6 under load | spec TEST-7 #1 — **TEST BUG, not a product bug**: it asserted `transport_parked_commits == 0`, but a parked commit is the §5.4 payload-lease re-arm gate WORKING (the COMMIT arrived while the request's lease still held a reference); engagement is a benign race the FUSE-2 reply-path work made MORE likely (cheaper reply ⇒ COMMITs win it) | **FIXED** 2026-08-04 (pre-RC loose ends): the assertion is now the park LEDGER's closure — `transport_parked_commits ≡ transport_unparked_commits` (new counter, stats inode), which stays red under a genuinely wedged gate and is immune to the race. Evidence: ×10 quiet-box green (engagement 0) + ×4 under 24-way CPU load green with engagement 23/25/32/35 parks, all closed — every one of those four would have FAILED the old assertion |
| `volume_drain_tests::test_offline_remove_data_drains_to_retired` | was 6/10 | **FIXED** 2026-08-02 (claim-release law); ×10 acceptance owed on the merged binary | — |
| `test_sqpoll_one_shared_poller_across_queue_rings` | 2/15 on clean base AND on branch (identical) | `/proc/self/task` comm scan racing async `iou-sqp` thread-spawn visibility; spikes under CPU load | Found by PERF-A 2026-08-02; assigned to the FUSE-2 agent (wait-for-count with deadline) |

**Standing corrections owed** (spec §11 evidence-practice): annotate the retracted serve-decomposition "5.3 ms pre-handler prize"; label the zram-specific write-wall §6.6 verdicts as substrate-bound; refresh the metadata baseline (current headline figures are from a box labeled DIRTY with a co-tenant at loadavg 19–25); re-run the competitive scoreboard (predates the reset-v3→v4 epoch change and ~20 campaigns).
