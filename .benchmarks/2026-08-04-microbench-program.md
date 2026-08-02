# 2026-08-04 — The microbench program: hot-path Criterion coverage + baseline teeth

Branch `feat/microbench-program` (off dev `68e8474`). Closes the gap
between the AGENTS.md Phase-4 mandatory-benchmarking policy ("every
public function has a Criterion benchmark") and reality (3 bench
binaries + one recent group) — **for the performance-load-bearing
machinery specifically**, a pragmatic sweep, not a bureaucratic one —
and gives the benches teeth: a baseline-comparison harness so hot-path
regressions trip locally instead of at field windows.

## 1. What shipped

* **Phase 1 — bench groups.** Four new root bench binaries
  (`copy_path_bench`, `zcrx_bench`, `read_path_bench`,
  `write_path_bench`), new groups joined to two existing binaries
  (`meta_lv_bench` +`kv_journal`, `ipc_hop_bench` +`cqe_doorbell`), and
  the fuse3 fork's first bench target (`fuse3_hot_bench`, own
  workspace — criterion dev-dep + explicit `[[bench]]` since
  `autobenches = false`). House style throughout: `harness = false`,
  field-derived input shapes documented in-file with citations.
* **Phase 2 — teeth.** `tests/run_bench_baseline.sh` (save/check): runs
  the full set on both workspaces, extracts per-bench **median ns/op**
  from criterion's `estimates.json`, compares against the committed
  reference, exits nonzero past the per-group threshold. Reference
  lives at **`.benchmarks/criterion-baselines/reference.json`** (flat
  `{bench_id: median_ns}` + a `_meta` stamp of hostname/commit/date —
  chosen over committing criterion's whole baseline tree: one
  reviewable file, no binary sample blobs in git).
* **Docs.** AGENTS.md Criterion section rewritten (bench inventory +
  harness law); test-tiering table gains the **nightly / pre-merge
  (perf PRs)** bench-baseline row. Per-commit cadence stays
  **smoke-only** (`cargo bench --benches -- --test`, unchanged).

### Honesty box (stated in-script, restated here)

Baselines are **SAME-BOX RELATIVE tripwires** — the thermally-capped
dev box gives relative truth only. The harness pins to the quiet cores
(`taskset -c 8-15`, `nice 10`, 8 build jobs), **refuses to measure**
when Tctl/Tdie ≥ 80 °C or any foreign cargo/rustc work is running, and
refuses cross-box compares (stamped hostname;
`SQZ_BENCH_ALLOW_FOREIGN_BASELINE=1` is the explicit override). A
missing bench **fails** the compare (the meta_lv_bench
silently-broken-since-PR-8 lesson); new benches and >25 % improvements
warn toward a re-`save`.

## 2. Coverage table (module → group → shapes → threshold)

| Module / machinery | Bench group (binary) | Field shapes (citation) | Threshold |
|---|---|---|---|
| `src/nt_copy.rs` NT-store copy core | `copy_path_nt` (copy_path_bench) | NT-vs-std A/B at 4 KiB (sub-floor), 256 KiB (the `SQUEEZEFS_NT_COPY_MIN` floor), 1 MiB (merge-chunk class), 4 MiB (block); unaligned source; NT side includes the load-bearing `sfence` (`.benchmarks/2026-07-31-near-zero-copy.md`) | 10 % |
| §5.5.2 sever + `SeveredPool` | `copy_path_sever` (copy_path_bench) | 1 MiB (`DEFAULT_MAX_OP_BYTES`) pooled-hit get→copy→put vs the retired per-op-alloc miss (`.benchmarks/2026-07-28-ingest-economy.md`: 1.76 M minor faults/s conviction) | 10 % |
| Warm serve prelude (§5.5.1) | `copy_path_serve_prelude` (copy_path_bench) | zero-heap `StackKey` `active_block:` key vs heap `format!` — the ns/op face of the `SQZ_ALLOC_TRACE` allocs/op==0 law (`.benchmarks/2026-07-28-ipc-op-economy.md`; `tests/ipc_op_economy_tests.rs`) | 10 % |
| `src/zcrx_lane/area_core.rs` `SpanLedger` | `zcrx_span_ledger` (zcrx_bench) | grant/release steady cycle, non-crossing clone-drop, 4-thread contended clone-drop on one hot slot (design-zcrx-read-lane §4.3/§5; 200 M+ field recycles `.benchmarks/2026-08-03-zcrx-lane.md`) | 10 % (25 % contended) |
| `src/zcrx_lane/pdu_stream.rs` chunk parser | `zcrx_pdu_stream` (zcrx_bench) | page-grain chunks, C2HData hlen 24/pdo 24 digests-off: 32×4 KiB-payload PDU stream and one 128 KiB MDTS-face PDU, headers split across seams (`.benchmarks/2026-08-04-zcrx-z2.md`) | 10 % |
| `src/read_lane.rs` `ReadLaneHold` | `read_lane_hold` (read_path_bench) | EXA shape: 4 MiB block deposited, four 1 MiB credited serves → coverage retire; credit-0 probe; miss probe (`.benchmarks/2026-08-01-read-lane.md`) | 10 % |
| §5.3 classifier `StreamLanes::observe` | `read_classifier` (read_path_bench) | exact-contiguity match arm; classified-membership ±64×len qd-reorder pair (round-3 wedge fix); foreign random-4 KiB claim arm (`.benchmarks/2026-08-01-read-lane.md`; docs/design-read-path.md) | 10 % |
| Coverage union `ActiveBlockBuf::record_write` | `write_coverage_union` (write_path_bench) | 4×1 MiB in-order; 32×128 KiB even-then-odd kernel-split OOO (FIND-L1-A / instrument-alignment lesson; `tests/write_through_coverage_tests.rs`) | 10 % |
| W2 extent overlay park/fold | `write_extent_overlay` (write_path_bench) | 16×4 KiB parks (the `fold_fill` ≥ 16 gate shape); escalate+seed fold (`docs/design-random-small-writes.md` §5.2, `.benchmarks/2026-07-17-rand-write-program-closing.md`) | 10 % |
| Supersession snapshot/CoW + `BLOCK_FLUSH_LOCKS` | `write_supersession` (write_path_bench) | snapshot mint; snapshot-alive CoW `make_mut` (4 MiB); stripe route + uncontended trylock (Idea-2 stamp; P1-9 level 3) | 10 % |
| Layout publish encode (`src/layout_wire.rs`) | `write_layout_publish` (write_path_bench) | full-save 1,024-block map (the convicted O(file-size) term) vs delta-64 encode/decode/apply (`.benchmarks/2026-07-30-write-commit-economy.md`) | 10 % |
| KV journal codec + reservation core | `kv_journal` (meta_lv_bench) | D4 create-shape entry (dentry Put + inode Put + parent Δtime) encode+xxh3; lever-B publish-batch-64 entry; §4.4 pt 5 admit/reserve/advance + admit/release on 8 MiB production geometry (`docs/design-cow-kv-metadata.md` §4.1/§4.4, `.benchmarks/2026-07-30-meta-plane-writes.md`) | 10 % |
| KV bset/tree/fold/meta-trait, `route_ino` | `kv_bset`/`kv_tree`/`kv_fold`/`kv_meta_metadata` (meta_lv_bench), `dynamic_meta_routing` (high_concurrency_bench) | **pre-existing coverage** (PR K1/K5/K7/M9, VL5a) — unchanged, now under the baseline harness | 20 % (tree/meta), 10 % |
| IPC ring/slot/payload floors | `ipc_hop` (ipc_hop_bench) | **pre-existing** (PR L4-2) — unchanged, now under the harness | 10 % |
| `CqeDoorbell` (squeezefs-ipc `cqe_core`) | `cqe_doorbell` (ipc_hop_bench) | complete-unparked (elide steady state), complete-parked (wake decision), park_begin/park_end ceremony — both load-bearing Dekker fences priced (`.benchmarks/2026-07-28-ipc-op-economy.md`) | 10 % |
| fuse3 ent header codec | `fuse3_ent_codec` (fuse3_hot_bench) | `fuse_in_header` decode (40 B ingress); attr-out / entry-out reply serialize, capacity-exact `serialize_into` — byte-for-byte the session.rs shape (design-metadata-throughput D2/D3) | 10 % |
| fuse3 commit-batch drain accounting | `fuse3_commit_batch` (fuse3_hot_bench) | per-flush histogram record, batch sizes 1..=8 (the exact-bucket law; `.benchmarks/2026-07-18-l3-transport-economy.md`) | 10 % |
| fuse3 kmbuf attach/recycle law | `fuse3_kmbuf` (fuse3_hot_bench) | sim venue (`KmbufQueue::sim_anon`, depth 32 / 1 MiB payloads): flagged re-point, unflagged reuse, attached read (`.benchmarks/2026-08-03-sqz-kernel.md`) | 10 % |
| Crypto/compress, contention, bench-engine | pre-existing groups (squeezefs_bench, high_concurrency_bench) | unchanged, now under the harness | 15 % / 25 % / 10 % |

**Bench seams added** (each documented at its site — the benches must
measure shipping code, not lookalikes): `SeveredPool` +
`new/get/put` → `pub`; `StreamLanes::new`/`observe` + `LaneRef` (+
`is_streaming()`) → `pub`; fuse3 `abi` + `get_bincode_config` →
`#[doc(hidden)] pub`; `CommitBatchHistogram::new/record` → `pub`
(+`Default`); `KmbufQueue::sim_anon` (anon-mapped sim constructor —
never a product path).

## 3. Inaugural numbers (the reference baseline)

**Recorded 2026-08-02** (`tests/run_bench_baseline.sh save`, exit 0):
**99 medians** (92 root + 7 fuse3, all 28 groups present) →
`.benchmarks/criterion-baselines/reference.json`, stamped
`strixhalo` @ `3ce65cc`. Venue: the thermally-capped dev box in the
harness's **paced mode** (`SQZ_BENCH_PACED=1`, added post-merge on
`test/bench-baseline-pacing`: build all targets first, then one bench
binary per quiet window — resume < 65 °C and no foreign cargo/rustc,
poll-never-contend; 16 pacing events over the ~94 min run, so no
binary measured on a heat-soaked or contended box). Same-box relative
tripwire semantics unchanged — these figures are `check`-mode
reference points on this host, never absolute claims.

Headline primitives (medians, ns/op, read from `reference.json`):

| Primitive | Bench | Median |
|---|---|---|
| NT copy vs std, 256 KiB (the `SQUEEZEFS_NT_COPY_MIN` floor) | `copy_path_nt/{nt,std}/256k` | **6,173 vs 6,899** (NT −10.5 %) |
| NT copy vs std, 4 KiB (below-floor shape — std must win) | `copy_path_nt/{nt,std}/4k` | 218 vs **50** (std 4.4×, floor law confirmed) |
| Severed-pool hit vs alloc miss, 1 MiB | `copy_path_sever/{pooled_hit,alloc_miss}_1m` | 33,796 vs 33,914 (parity at 1 MiB — the pool's win is fault-rate under saturation, not single-op ns) |
| Serve-prelude key mint, stack vs heap | `copy_path_serve_prelude/{stack,heap}_key_active_block` | **58 vs 65** |
| Doorbell complete, parked vs unparked | `cqe_doorbell/complete_{parked,unparked}` | 18 vs 18 (park/begin/end 28) |
| KV journal entry encode+checksum, create shape | `kv_journal/entry_encode_checksum_create` | **75** |
| KV journal publish batch-64 encode / decode | `kv_journal/entry_{encode,decode}…publish_batch64` | 1,458 / 7,178 |
| Read-lane hold: miss probe / credit-0 serve probe | `read_lane_hold/{miss_probe,serve_credit0_probe}` | 43 / 83 |
| Read classifier: seq match arm / foreign-random claim / reorder pair | `read_classifier/…` | 65 / 54 / 96 |

## 4. Surprises / findings (filed, not fixed here)

**PENDING — lost with the authoring session.** The gate record in §6
was written referencing "the flake note in §4" (fuse3 lib suite run
×5 quiet-box), but the finding itself was never written down before
the session died, and its content is not reconstructible from the
committed work. Do not backfill from memory: if the fuse3 lib-suite
flake (or any other finding) reproduces during the §3 measurement
session, record it here fresh with its own evidence; otherwise this
section closes as "none reproduced".

## 5. What remains uncovered, with reasons

| Item | Reason |
|---|---|
| `DataRouter::hold_serve_admission` (the R1b ceremony) | private async method needing a fully-wired router + tiers + ghost table — integration scope; its serve legs' primitive costs are covered (hold serve, tier probes ride existing groups); a rig-level row exists in `tests/copy_census_rig.sh` |
| Shadow-epoch open/close (`RewriteEpoch`) | `pub(crate)`, constructible only through `DataRouter::rewrite_shadow_record` with allocator guards + metadata backend — integration scope; the swap-record ENCODE face is covered (`write_layout_publish`), and the epoch's contracts are pinned by `tests/rewrite_shadow_tests.rs` |
| Serve-into-arena `PayloadSink` impl (`ArenaWindow`) | constructible only from a live shm session map; the copy cost itself is covered by `copy_path_nt`/`payload_4k_each_way`, and the two-process truth is the ipc_hop rig (`tests/run_ipc_hop_bench.sh`) |
| fuse3 commit-batch DRAIN loop (`push_cmd_batched`/`flush_submit`) | requires a live io_uring + armed FUSE session; only its accounting arithmetic is microbenchable (covered); the end-to-end number is `transport_commit_batch*` on the stats inode under load |
| `read_serve_copy_raw` NT read-serve arm | same `copy_body` as `dma_copy_*` (already laddered); a separate bench would double-count one function |
| libaio-reap / futex wait paths (`REAP_EVENT_PARK_MAX` regimes) | syscall-bound (FUTEX_WAIT/WAKE) — criterion in-process numbers would be fiction; the rig owns them |

## 6. Gates

*(Provenance: the list below is the authoring session's record, written
before that session died. The resumption session that finished this
note independently re-verified `cargo fmt --check` clean on both roots
and re-ran the bench smoke — result stated at the end of this
section.)*

* `cargo clippy --all-targets --all-features -- -D warnings` — clean (root);
  fuse3 clippy (`--all-targets`, crate features) — clean.
* `cargo fmt --check` — clean, both roots.
* **Bench smoke** `cargo bench --benches -- --test` — green, both roots
  (every new bench runs one iteration; a panicking bench fails the gate).
* Targeted suites untouched-green (`--test-threads=1`): read_lane 8,
  ipc_host 23, ipc_op_economy 3, zcrx_lane 29, write_through_coverage 8,
  layout_delta_fold 8, publish_coalesce 6, kv_journal 21,
  extent_overlay 14, ingest_economy 4; fuse3 lib suite 60/60 (×5 quiet-box
  — see the flake note in §4).
* `cargo doc --no-deps` — builds; 3 pre-existing warnings
  (`ipc_service`/`AdmissionGovernor` private intra-doc links), none from
  this branch.

**Resumption-session re-verification (2026-08-02, at HEAD `193749b`):**
`cargo fmt --check` clean on both roots; bench smoke green on both
roots — root `cargo bench --benches -- --test` exit 0 with all 25
groups exercised (the full coverage-table set: `copy_path_{nt,sever,
serve_prelude}`, `zcrx_{span_ledger,pdu_stream}`, `read_lane_hold` /
`read_classifier` incl. the engagement asserts, `write_{coverage_union,
extent_overlay,supersession,layout_publish}`, `kv_journal`,
`cqe_doorbell`, plus every pre-existing group); fuse3
`cargo bench --benches --features tokio-runtime,unprivileged -- --test`
exit 0 (`fuse3_ent_codec`, `fuse3_commit_batch`, `fuse3_kmbuf` all
Success; lib suite 60/60 in the same invocation). Clippy/doc/targeted
suites above remain the authoring session's record.
