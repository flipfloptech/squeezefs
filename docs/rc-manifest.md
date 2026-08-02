# SqueezeFS RC Manifest

**Purpose.** The release-candidate's accountability record: every engineering-spec item → its disposition (fix SHA + repro test, or a written adjudication), the guarantee table by evidence tier, the format/incompat ledger, and the evidence index. Plan exit criterion **E14**; started at Phase 0 by design so it accumulates instead of becoming archaeology.

**Release intent.** Public RC — a gift to the AI community. That intent raises two bars above internal-use software: (a) untrusted-input surfaces get first-class treatment (any process in the namespace reaches the IPC socket; the job-wire listener binds `0.0.0.0` by ruling D2), and (b) every claim in user-facing docs must cite the evidence tier it actually has.

**Inputs.** `docs/pre-rc-engineering-spec.md` (Rev 3) · `docs/pre-rc-execution-plan.md` (Rev 3) · rulings D1–D7.

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
| **§6.12 doc honesty** | **LANDED** — README and AGENTS.md now state the shipped concurrency scope (one write mount per volume set, enforced), mark TTL/renewal/global-fencing as unimplemented with their landing stages, and require scale claims to cite an evidence tier | `9ed7b3e` |

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
| ENG-2 (`cargo audit` in the gate) | ENG-5 landing | The `rsa` and rustls-0.21 findings are what the gate will report |

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
| — | Post-RSA key wrap (KW-1) | — | Ruling D3; same window |
| — | Sharded indirect map (DUR-6 ⊕ PERF-9) | — | Same window |

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

## 5. Supply chain (ENG-2 census, `cargo audit` 2026-08-02 @ `402ca77`)

`cargo-audit` installed and run for the first time in the project's history — the AGENTS.md Phase-5 claim "no known vulnerabilities" had never been verified. **5 vulnerabilities, 6 warnings.**

| Advisory | Crate | Disposition |
|---|---|---|
| RUSTSEC-2026-0098 (URI name constraints) | `rustls-webpki 0.101.7` | **Dies with ENG-5** — this is the EOL rustls 0.21 stack that only `hyper-rustls 0.24` (zero references) pulls in |
| RUSTSEC-2026-0099 (wildcard name constraints) | `rustls-webpki 0.101.7` | Same — ENG-5 |
| RUSTSEC-2026-0104 (reachable panic in CRL parsing) | `rustls-webpki 0.101.7` | Same — ENG-5 |
| RUSTSEC-2026-0204 (invalid pointer deref in `fmt::Pointer`) | `crossbeam-epoch 0.9.18` | Patched ≥ 0.9.20 — a lockfile bump; do it in the same pass as ENG-5 |
| RUSTSEC-2023-0071 (Marvin attack, timing key recovery) | `rsa 0.9.10` | **No patched release exists in the 0.9 line.** Ruling D3: replace the primitive (KW-1 post-RSA key wrap) rather than adjudicate. Until KW-1 lands this is the one advisory that requires a written RC disposition |

**Warnings worth a decision, not just a note:**

- `scc 3.8.3` — **unsound**: `Array::insert` violates exception safety if the compare function panics, risking a double free (RUSTSEC-2026-0205). `scc::HashMap` is load-bearing throughout the latch-free hot paths (this is not a leaf dependency). Check for a fixed release and bump; if none exists, audit whether any comparator we pass can panic.
- `event-listener 5.4.1` — unsound: `!Send` tags crossing thread boundaries via `StackSlot` (RUSTSEC-2026-0221). Transitive; confirm the reachable path.
- `spin 0.9.8` — yanked. `number_prefix`, `rustls-pemfile 1.0.4` — unmaintained (the pemfile one likely also dies with ENG-5).

**Gate action (ENG-2 proper):** once ENG-5 lands, add `cargo audit` to `task check` and the full gate, then re-run and record the residue here. A public RC ships with this table filled in, not with the claim unverified.

---

## 6. Evidence index

Populated as campaigns close. Current: `.benchmarks/2026-08-02-fuse1-init-ext.md` (FUSE-1 live A/B, reset-v5 venue) · `.benchmarks/criterion-baselines/reference.json` (99-median bench reference) · `.benchmarks/2026-08-04-*.md` (the five recovered campaigns).

**Open flakes** (TEST-7 class — every one poisons future counted runs; the multi-run discipline requires counts restart after each fix):

| Flake | Rate | Shape | Owner |
|---|---|---|---|
| `multi_queue_tests::storm::…_no_starvation` | 2/5 on untouched tip, 5/6 under load | spec TEST-7 #1 | Expected to ride the FUSE-2 redesign (same code) |
| `volume_drain_tests::test_offline_remove_data_drains_to_retired` | was 6/10 | **FIXED** 2026-08-02 (claim-release law); ×10 acceptance owed on the merged binary | — |
| `test_sqpoll_one_shared_poller_across_queue_rings` | 2/15 on clean base AND on branch (identical) | `/proc/self/task` comm scan racing async `iou-sqp` thread-spawn visibility; spikes under CPU load | Found by PERF-A 2026-08-02; assigned to the FUSE-2 agent (wait-for-count with deadline) |

**Standing corrections owed** (spec §11 evidence-practice): annotate the retracted serve-decomposition "5.3 ms pre-handler prize"; label the zram-specific write-wall §6.6 verdicts as substrate-bound; refresh the metadata baseline (current headline figures are from a box labeled DIRTY with a co-tenant at loadavg 19–25); re-run the competitive scoreboard (predates the reset-v3→v4 epoch change and ~20 campaigns).
