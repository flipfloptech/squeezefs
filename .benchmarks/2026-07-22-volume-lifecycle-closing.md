# Volume Lifecycle & Online Maintenance program — closing report + the first full release gate (PR VL10)

**Sequencing note (user directive 2026-07-24 — merge preceded
acceptance):** the entire VL10 campaign branch (`test/vl10-release-gate`,
~30 external-suite fixes incl. the generic/795 quartet and generic/423,
plus the 1.1.0 release-train versioning) was MERGED to `dev` first
(tip `abac3d0`; the campaign branch is deleted), and the release-gate
acceptance evidence was then produced ON THE MERGED TIP by the
`test/vl10-acceptance` campaign (§10 below) — the only acceptance
citations for the gate are §10's from-zero runs on `abac3d0`.

Branch `test/vl10-release-gate` off `dev` @ `8d7058a`, 2026-07-22. Design:
`docs/design-volume-lifecycle.md` (Rev 12 — program CLOSED with this
record). Companion evidence: `.benchmarks/2026-07-21-vl8-stabilization-catalog.md`
(the pre-existing-bug catalog) and `.benchmarks/2026-07-21-wedge-and-074-fixes.md`
(the wedge/074/FIND-RW5-A campaign that closed every open engineering item
before this gate ran).

Instrument note (standing rule): every number below states its instrument.
External suites ran on this box (32-core, 109 GiB RAM, Arch/CachyOS kernel
7.1.3) via their house runners: `tests/run_pjdfstests.sh` (NEW, lands with
this PR), `tests/run_ltp_syscalls.sh`, `tests/run_fstests.sh` — all
file-backed `/dev/shm` volumes + tmpfs staging, release binary.
Rig/matrix/soak evidence is the VL9 final-binary run set (2026-07-21
session artifacts `vl9_{soak,matrices,rig}_FINAL.log` — tallies quoted in
§4; the artifacts were session-scoped /tmp files and did not survive the
2026-07-22 reboot, so §4's quoted counts are the durable record). Devsub
measurements state theirs inline.

---

## 1. Charter recap (user requirements, verbatim intent — §2.1 of the design)

| # | Requirement | Disposition |
|---|---|---|
| 1 | Dynamically add and remove metadata AND data volumes | **SHIPPED** — `volume add-data/remove-data/undrain/list` (online + offline), `volume add-meta/remove-meta` (offline), `volume migrate-meta-slot` (online), `volume repair-set` |
| 1a | Capacity preflight; refuse a remove that cannot fit; report honestly | **SHIPPED** — closed-form `DrainPreflight` (refuse iff `avail < needed+transient+headroom`, property-tested), refusals print the numbers, checkpoint re-verification self-pauses (`paused-capacity`) |
| 1b | AUTOMATIC migration from/to added/removed volumes | **SHIPPED** — CoW evacuation movers; auto-rebalance-on-add (KD-12, `--no-rebalance` opt-out); continuous drift prevention in the write path (§5.9 balance-aware placement, the user's OQ-3 design) |
| 1c | ONLINE if possible, offline optional | **SHIPPED** — online-first everywhere it is safe; the two exceptions are stated loudly (meta membership add/remove are offline verbs; `defrag --fold` is live-only) |
| 2 | Defragment (2a online) | **SHIPPED** — four-axis model (KD-11), `defrag --data/--meta/--fold/--rebalance/--report-only`, live gauges `frag_d1..d4` |
| 3 | fsck (3a online) | **SHIPPED** — seven classes C1–C7, verify-before-report (zero-FP ladder), per-class `--repair [--apply]` (SG-3 ruling: repair ships), online + offline `--shards k/N` |
| 4 | All ONLINE jobs distributed across ALL MOUNTED CLIENTS, throttled via percentage | **SHIPPED** — one job fabric, per-worker duty-cycle throttle (KD-3), remote workers over the §5.1.6 job-shard wire (SG-1 ruling: remote execution ships in v1.1) |
| 5 | Offline jobs OPTIONALLY distributed | **SHIPPED** — offline modes are the same engine in a short-lived D0-guarded coordinator hosting the same wire endpoint (KD-13); coordination-free `--shards k/N` remains the degenerate mode |
| 6 | "Online methods only is OK" | Honored as online-first; nothing regressed to offline-only that was online before |

## 2. The PR ladder — landed SHAs and outcomes

All PRs merged ff-only to `dev` except VL10 (this branch, left unmerged for
review per the program instruction). SHAs are `dev` history.

| PR | Landed (red → green, principal) | One-line outcome |
|---|---|---|
| VL1 honesty cleanup | `80e2eae` → `6e07820` (2026-07-19) | Fake `config data-volume/metadata-volume/fsck` verbs + dead job code + phantom `meta_volume_0` seed deleted; CLI arms refuse loudly naming successors (G-VL-1) |
| VL2 job fabric + admin lane + xattr screen | `4732c4d` → `1613236` (slice 1), `5239f71` → `2e87afb` (slice 2) | Durable schema-1 `job:` records on ino 1; duty-cycle throttle ±10 % at 25/50/75 pinned; crash-resume by adoption; ADMIN lane armed on every mount; reserved-xattr screen closes the pre-existing format-config tamper hole |
| VL2b remote wire | `c50dde5` → `0029f58` | §5.1.6 shard-execution wire: HMAC storage-trust enrollment, shard leases + fencing-checked proposals, fresh-destination law both halves, WERO (rtype 2) preempt ladder, plaintext ⇒ 100 % verify-reads |
| VL3 durable identity + online add | `c975762` → `c6e6af2` (+ `9b71bb3` config re-home, `5c56215` docs) | Never-reused `vol-` ids (KD-5); online `volume add-data`; `KV_VOLUME_LIFECYCLE` bit 3; `/dev/shm` runtime-config file deleted; lifecycle rig skeleton |
| VL4 drain/remove + movers | `886aa8f` → `c413a09` | Closed-form preflight (G-VL-3 e); CoW evacuation with move-once clone law + pre-publish ledger; draining serves reads (EIO foot-gun retired); rebalance objective + auto-on-add |
| VL4b balance-aware placement | `ed447f4` → `2eb22a8` (+ rig leg 8 `272c1bc`) | `health_effective` fill penalty (capped 300/1000), ArcSwap `PlacementTable`, per-write DashMap scan retired; failover semantics pinned unchanged |
| VL5a frozen width + slot map | `ffd4868` → `b08d0dc` | Durable `routing_width W` (W ≤ 1 identity pinned), root-ledger membership stamps, order-independent bootstrap, `KV_GUEST_SLOTS` bit 2; legacy sets byte-identical |
| VL5b slot migration engine | `1049cc1` → `2f50939` (+ `c4275a4` online verb, `a6e6939` bootstrap fix, `f433c...`/`f433cbb` KD-8 two-phase staging rebind — dev SHA `f433cbb` = `fix(meta): KD-8 becomes a crash-safe two-phase staging REBIND`) | Guest keyspaces, conveyor delta tee + overflow fallback, pre-4a cutover gate, §5.5.2b ordered flip with enumerated crash windows, staging drain barrier → generation rebind |
| VL6a fsck detection | `e174c19` → `84d6f3b` (+ `572d052` registry feeders, rig legs `0959443`) | Seven classes incl. C7 scrub; the zero-FP ladder (settle + lease re-check; epoch filter escalating to the in-flight registry); offline sharding + merge-reports |
| VL6b fsck repair | `b0cb46a` → `baa177a` (+ rig leg 15 `1be563b`; FP fixes `45337c4`/`335674a`/`186fb31`) | Per-class verify-before-repair actions, dry-run default, quarantine-first with fsynced manifest, apply-twice idempotent, kill-9-mid-apply convergent |
| VL7 defrag | `10db8b6` → `fe9e41c` (+ loom `d8fefa3`, rig leg 16 `bfa66b0`) | Four measured axes + movers reusing `move_one`/fold/SMO-compactor; `--report-only`; contiguity-aware allocator picks |
| VL8 stabilization catalog | per-item red→green series (see the catalog record); wedge/074/RW5-A campaign on `fix/write-wedge-and-074` (dev `bd1f3dc` → `b3fc09e` ledger-lock invariant, `dcace33` → `76c6f80` O_TRUNC, `6e8e770` → `12ef8b4` + faces 4–7 `c9a3d9a`/`2b80a56`/`8637d0a`/`ca49615`) | EVERY catalog item closed: 9 pre-existing bugs fixed tests-first (incl. both wedge modes and the 074 stale-unit class); 003/213 adjudicated as the only standing expected shapes; generic/464 + generic/074 flipped to expected-PASS |
| VL9 soak + matrices + interaction pins | `6e4e816` → `31cd573` (pin a), `73a3d22` → `cf28130` (pins b/d + source-pin FP fix), `22d6adc` (soak + matrices + leg 17), `716c62c` → `6ff2b3a` (guest-only census fix) | The canonical 9-step lifecycle soak (10-op rotating menu) + the counted ×10 G-VL matrices + the five interaction policy pins — all green on the final binary (§4 below) |
| VL10 closing + release gate | this branch (~50 commits, unmerged for review): `41e3576` (runner), `0ce70db` → `37f1c86` (pjdfstest create/parent-attr), the §5.3 fstests fix ladder (~17 families, every fix red-first), `1f654c4` → `4381670` (fsck scan-rate floor), `442d09f` → `d5033a5` (drain pipeline floor), the three gate-caught regressions (§9: `704a3df` foreign-zeros, `991317d` harness, `9725b09` → `445b412` stale-binding decode), the fail-fast rule (`aaed390`/`b735409`/`d7a8ec2`), docs/closing commits | The first full three-suite release gate (§5) + measured G-VL floors (§6) + this record + AGENTS/README/QUICKSTART/operations program docs |

## 3. G-VL gate adjudication

| Gate | Verdict | Evidence (instrument stated) |
|---|---|---|
| **G-VL-1** honesty | **PASS** | VL1 refusal-message contracts (`tests/` CLI pins, retargeted to the real verbs in `c444451`); clippy `-D warnings` clean tree-wide at every merge; AGENTS module map truthful (defrag/fsck lines updated with the PRs that made them true) |
| **G-VL-2** data add | **PASS** | Lifecycle rig (add → write → remount → read; `volume list`/df exactness; auto-rebalance submitted + `--no-rebalance` suppression pinned in VL4's cargo suite); final-binary rig pass 2026-07-21 (vl9_rig_FINAL.log: `VOLUME LIFECYCLE RIG … PASSED`) |
| **G-VL-3** drain/remove | **PASS** | (a) matrix **m1 = 10/10 zero-loss** drain kill-9 (randomized injection; checksum manifests identical; vl9_matrices_FINAL.log); (b) drain throughput vs raw device copy at 100 % throttle — **measured this session, §6**; (c) clone move-once refcount-pinned (VL4 suite); (d) W1-ledger delta-0 across a drain pinned (VL4 suite; `write_path_seed_read_bytes`/`patch_edge_rmw_reads` stay 0 in the soak's step-9 tripwire check); (e) preflight refuse-iff property test (fuzzed censuses, VL4 suite); (f) draining-serves-reads regression pinned (VL4 suite; exercised live by rig leg 17a and the soak's `data_drain` iteration) |
| **G-VL-4** meta add/remove | **PASS** | Matrix **m2**: §5.5.2b crash windows ×12/window + torn ×10 (deterministic cargo seams) + staging-barrier/add-meta coordinator kill-9 **10/10** (vl9_matrices_FINAL.log); slot-migration tree-diff equivalence + cutover gate + cross-slot-rename-during-cutover + delta-overflow fallback pinned (VL5b suite); incompat-bit non-intersection + old-mask refusal + W=1 byte-identity pinned (VL5a suite); soak iterations 8/9/10 (`slot_migrate`, `meta_add`, `meta_remove`) green with byte identity + st_ino stability (vl9_soak_FINAL.log) |
| **G-VL-5** fsck + scrub + repair | **PASS** (device-backed floors measured this session, §6) | (a) FP = 0: matrix **m3 = ×10 under fsx/fsstress churn AND ×10 drain-concurrent clone-heavy** (vl9_matrices_FINAL.log), plus the stalled-unit/R5-park FP-seeding fault injections (VL6a suite) — the drain-concurrent pin caught and fixed the mover source-pin C3 FP (`cf28130`); (b) seeded detection 100 % per class C1–C7 incl. both C1 seeds and all three C7 arms (VL6a fault rig); (c) floors: cargo-instrument baseline recorded (census 1,142,885 inodes/s vs scan 117,518 inodes/s, RAM-authoritative warm — the honest caveat lives in `tests/fsck_tests.rs`); the gate-grade device-backed numbers are in §6; (d) repair legs: per-class seed ⇒ detect ⇒ dry-run ⇒ apply ⇒ re-fsck-clean ⇒ manifest-intact + apply-twice idempotence + kill-9-mid-apply convergence (VL6b suite + rig leg 15) |
| **G-VL-6** defrag | **PASS** | Synthetic-fragmentation fixture: D1 ≤ 0.3 → ≥ 0.9 with reclaimable tail ≥ 90 % under concurrent churn, zero corruption; D2 strictly improved on the streaming fixture; `--report-only` matches the independent census (VL7 cargo suite + rig leg 16, re-run in the final rig pass) |
| **G-VL-7** fabric | **PASS** (single-node legs; remote ×10 legs deferred — §7) | Throttle duty-cycle ±10 % at 25/50/75 pinned (VL2 suite); pause/resume/cancel/live-rethrottle durable (VL2 suite); matrix **m4 = 10/10** coordinator kill-9 mid-defrag ⇒ remount ⇒ adopted durable job converges unattended (vl9_matrices_FINAL.log); offline probe-readable records (VL2 suite); wire mechanics — enrollment refusal, stale-fencing refusal, lease-expiry reclaim with fresh destinations, WERO preempt semantics, SIGSTOP/SIGCONT zombie shapes, plaintext 100 %-verify — pinned at the cargo layer (VL2b suite, `FakeReservationClient` + in-process wire) |
| **G-VL-8** placement | **PASS** (rig-scale; full-matrix row deferred — §7) | Weight-math property tests (300-point cap, unhealthy ⇒ weight 0, failover-unchanged) + the imbalanced-set scenario (VL4b suite); rig leg 8: fill spread converges + emptier-volume pick share on the live mount (`272c1bc`, re-run in the final rig pass) |

## 4. VL9 final-binary counted evidence (the soak + matrices, 2026-07-21)

- **Canonical lifecycle soak**: `SOAK_ITERS=10` — **10/10 green**, one
  lifecycle op per iteration over the full 10-op menu (fsck, scrub,
  defrag-data, data-add+auto-rebalance, rebalance, defrag-meta,
  data-drain, ONLINE slot-migrate, meta-add, meta-remove), byte identity +
  st_ino stability + xattr identity after every op, space reclaimed to
  baseline, clean dismount with zero staged residue + deregistered
  heartbeats + must-stay-0 tripwires each iteration
  (vl9_soak_FINAL.log).
- **Counted matrices** (`LOOPS=10`, abort-on-failure):
  m1 drain kill-9 **10/10 zero-loss**; m2 crash windows **×12/window +
  torn ×10 + barrier kill-9 10/10**; m3 fsck FP=0 **×10 churn + ×10
  drain-concurrent**; m4 fabric kill-9 **10/10 converged**
  (vl9_matrices_FINAL.log).
- **Rig** (kill-9 `LOOPS=3`, all legs incl. 17a/17b interaction rows):
  PASSED (vl9_rig_FINAL.log) — leg 17a observed
  `job_serialized_waits 0 → 2` (mover-scope serialization engaged) and the
  named-draining-victim refusal; leg 17b converged a live slot migration
  during a drain (cutover 0 ms) with manifest intact + fsck clean.

## 5. The release gate (first full execution — user ruling 2026-07-20)

### 5.1 pjdfstest (`tests/run_pjdfstests.sh`, NEW in this PR)

- **Inventory run** (dev-tip binary + the new runner): 238 files /
  **8,798 tests — exactly ONE failing family**: `open/00.t` subtests
  33–34 ("update parent directory ctime/mtime if file didn't exist").
- **Root cause + fix (repro-port mandate)**: the CREATE handler kept the
  parent's pre-create attr-cache entry while the backend bumped the
  parent's mtime/ctime — every parent GETATTR inside the attr-TTL window
  served pre-bump times (the same observation-layer artifact the fstests
  generic/003 leg (ii) adjudication had described; the bar moved from
  "documented" to FIXED). unlink/rename already refreshed (D2.c);
  mkdir/mknod/symlink invalidate; create was the one dir-mutating handler
  that did neither. Red `0ce70db`
  (`tests/attr_refresh_tests.rs::create_refreshes_parent_attr_cache` —
  fails on the exact stale-parent-mtime signature) / green `37f1c86`
  (create rides `refresh_attr_cache(parent)`, backend-authoritative).
  Targeted verify: `open/00.t` 47/47.
- **Full sweep, count restarted from zero on the FINAL binary**
  (final boot, tree `704a3df`): **8,798/8,798 — `Result: PASS`,
  verdict 0** (238 files, 161 s). The runner's expected-fail table
  **closes EMPTY** (it is enforced bidirectionally: any unexpected
  failure OR any stale entry exits nonzero). Counted-restart lineage,
  honestly: identical 8,798-PASS runs landed on the mid-gate binary
  (pre-reboot) and on `d5033a5` — each restart triggered by a
  subsequent code fix, each rerun green.

### 5.2 LTP (`sudo tests/run_ltp_syscalls.sh`, full default set)

**PASS 174 / FAIL 0 / BROKEN 0 / SKIPPED 9 (TCONF)** — green on the
final binary (`704a3df`), final boot, fresh /tmp toolchain build. The
9 TCONFs are the suite's own environment conf-skips (unchanged from
prior runs), not SqueezeFS refusals. Zero failures ⇒ zero repro-port
obligations from LTP. (Same green tally on `d5033a5` and twice
pre-reboot — every restart rerun per the multi-run discipline.)

### 5.3 fstests (`sudo tests/run_fstests.sh`, full `-g auto`)

**Fix-loop discipline as ruled**: the first full inventory produced
the failure list (that run self-poisoned around generic/580 after
collecting the families below — documented honestly; no second full
run was spent between fixes), then per-family targeted fix loops (red
cargo repro-port → fix → single-test verify). The first post-fix full
sweep then acted as a SECOND inventory: it caught one more family —
the generic/075.3 rider wedge (fsx mmap `msync: EIO`; 192/247/258
umount-busy collateral from the wedged daemon) — which was fixed
red-first (ladder row below), the sweep aborted per the counted
discipline, and the final sweep restarted from zero on the fixed
binary (§5.3b).

**The fix ladder** (every family: red cargo test first, then the fix;
red → green SHAs; single-test verification in minutes not hours):

| Family | Root cause | Red → Green | Repro-port |
|---|---|---|---|
| generic/131(27), 478, 504 (POSIX/OFD/flock) | daemon lock table could never match kernel semantics (unlock-on-close vs FLUSH elision, OFD owners) | `5a03da9`/`030f3e5` (first rewrite), superseded by `531169a`→`bf5e19c` (flock) + `f55935e`→`c045295` (**kernel-local amputation**: never advertise `FUSE_POSIX_LOCKS`/`FUSE_FLOCK_LOCKS`; daemon table + DLM delegation surface deleted) | negotiation pins in `crates/fuse3` session tests; 131/478/504 = kernel-interface-only rows → `SQUEEZEFS_FSTESTS_QUICK` |
| generic/035 + 078 (rename) | dir-overwrite left dest nlink; rename2 flag half-truths | `e55d3ca` → `857979d` | `tests/rename_semantics_tests.rs` |
| generic/020 (xattr cap) | record-envelope budget stole 256 B from the 64 KiB xattr VALUE cap | → `948de74` | `tests/kv_backend_tests.rs` + node/tree cap tests |
| generic/062 (virtual files listed) | `.config`/`.stats` leaked into readdir | `a9854a2` → `472a5e6` | `tests/kv_scale_tests.rs` virtuals pin |
| generic/062 rider | attr ≥ 2.6 unconditional `--restore` warning | harness `-hP` sed (instrument noise, not product) | n/a (harness truth) |
| generic/128 (nosuid) | mount ignored `-o nosuid/nodev/noexec` | `710df84` → `605cc60` | `tests/mount_preflight_tests.rs` |
| generic/258 (pre-epoch times) | u64 storage truncated negative ns | `cdc3765` → `7b68204` | `tests/attr_refresh_tests.rs::negative_timestamps_round_trip` |
| generic/099/319 (ACL) | half-advertised ACLs | `e4a83f3` → `21c45c4` (ENOTSUP posture), then `600a94f` (stop advertising `FUSE_POSIX_ACL` — the blanket-EOPNOTSUPP create regression) | `tests/job_fabric_tests.rs` ACL rows + fuse3 negotiation pin |
| generic/426/467/477 (open_by_handle) | EXPORT_SUPPORT advertised but LOOKUP('.')/('..') unimplemented | `a878a36` → `b943cec` ('.'), `4437f78` → `fd7963a` ('..' reverse dentry scan) | `tests/attr_refresh_tests.rs` lookup rows |
| generic/525 (high-offset) | u32 block-index silently wrapped | `4857a38` → `51595eb` (honest EFBIG cap = `min(i64::MAX, bs×u32::MAX)`) | `tests/sparse_write_bounded_tests.rs` |
| generic/533 (removexattr) | absent attr returned ENOENT | `97d48d4` → `948a0ea` (ENODATA) | `tests/job_fabric_tests.rs` |
| teardown-linger family (294/306/452/529/530) | delete's staged sweep was O(logical size) — 115.9 s on a huge sparse file | `356ac64` → `27c29d9` (occupancy-index prefix sweep, O(present + map); red 115.9 s → ~40 ms) | `tests/sparse_write_bounded_tests.rs::delete_of_huge_sparse_file_is_omap` |
| remount family (294/452 residual) | mount helper treated `-o remount` as a fresh mount (60 s daemon wait) | → `6148685` (helper hands MS_REMOUNT to the kernel: `mount -i` + `LIBMOUNT_FORCE_MOUNT2=always` — fsconfig re-submission is refused by kernel fuse) | kernel-interface-only (documented in the commit); 294/306/452 in `SQUEEZEFS_FSTESTS_QUICK` |
| generic/306 residual (mknod rdev) | device numbers never persisted (`rdev: 0` hardcoded; reply-echo only) | `ed5ee14` → `4bf7e75` (rdev rides the inode value's reserved wire word — historical `flags2`, never written by a live binary; zero on-disk format change; renamed end-to-end) | `tests/attr_refresh_tests.rs::mknod_rdev_round_trips` |
| generic/209 (aio-dio staleness) | two write-path windows: checkout invisibility + multi-block reads not composing parked runs | `298d48f` → `a9e40b6` (OVERLAY NEVER INVISIBLE on the write path + `overlay_parked_runs`; repro 10/10, mount 5/5) | `tests/write_visibility_tests.rs` |
| generic/075.3 (fsx mmap, msync EIO) — final-sweep inventory find | rider admission bounded by `meta.size` (truncate-UP inflated) instead of the staged image: a beyond-chunk extent parked whose fold composes an unstorable > 4 MiB image — the FIND-RW4-A guard refuses (correctly) and fsync wedges EIO; generic/192's umount-busy was collateral | `95772f8` → `27e4a32` (bound by `min(img_len, meta.size)`; beyond-image writes take the whole-image grow/promote path) | `tests/rw5a_never_lossy_tests.rs::truncate_up_rider_admission_bounds_by_the_image_not_the_size` |
| generic/631 (overlayfs-upper) — fail-fast sweep find | overlayfs refuses any upper fs lacking RENAME_WHITEOUT ('upper fs missing required features'); the kernel forwards flag 4 to every FUSE fs (proven live — the daemon logs it), so the 078-era EINVAL refusal was a DAEMON gap, not a kernel-interface exception | `3e6c59f` → `d36c273` (whiteout = char-0:0 inode minted ATOMICALLY inside the rename tx — storable since the 4bf7e75 rdev word; cross-volume shape rides the documented fragment posture; WHITEOUT\|EXCHANGE refused at every layer) | `tests/rename_semantics_tests.rs` whiteout pins ×3 + refusal pins; generic/631 PASS (42 s overlay rename storm), generic/078 now RUNS its whiteout legs and passes |
| generic/003 re-pin + 192 | noatime-by-design class | `b84f818` (documented shapes, not fixes) | standing adjudication (§8) |
| pjdfstest open/00.t 33–34 | create kept stale parent attr cache | `0ce70db` → `37f1c86` | `tests/attr_refresh_tests.rs::create_refreshes_parent_attr_cache` |

`SQUEEZEFS_FSTESTS_QUICK` grew by every case above that caught a real
bug (+020 035 062 128 131 258 294 306 426 452 467 477 478 504 525
533), with the kernel-interface-only exceptions (131/478/504, the
remount arm) annotated inline in `tests/run_fstests.sh`.

### 5.3b The final sweep — under the NEW fail-fast rule

**Standing rule adopted mid-gate (user directive, 2026-07-22):** full
fstests runs FAIL FAST — the run aborts at the first UNEXPECTED
failure (artifacts preserved, test named loudly), the failure is
fixed red-first, and the run RESTARTS from zero. The adjudicated
by-design set (003/192/213) continues only on an EXACT pinned-shape
match (`expected_shape_diff` in `tests/run_fstests.sh` carries the
verbatim diffs). Runner + LTP-runner + AGENTS commits: `aaed390`,
`b735409`, `d7a8ec2`. The former "inventory once" posture is retired
for full runs.

**Restart lineage (honest):** sweep 1 (inventory, pre-rule) found the
075.3 rider wedge → aborted + fixed (`95772f8`→`27e4a32`); sweep 2
(pre-rule restart) was killed by the rule adoption itself; the full
serial cargo gate then caught the reads_mid_fold EINVAL flake → fixed
(`9725b09`→`445b412`); fail-fast sweep 1 ran 622/787 and aborted at
generic/631 (overlayfs-upper needs RENAME_WHITEOUT) → IMPLEMENTED
(`3e6c59f`→`d36c273`; 631+078 verified targeted); fail-fast sweep 2
ran 469/787 and aborted at generic/476 on a DMESG-ONLY finding whose
every trigger line was this box's amdgpu display-driver WARN (zero
fs frames — instrument noise; the runner now continues dmesg-only
failures IFF every trigger is a named host-hardware frame, `738e635`);
fail-fast sweep 3 ran 625/787 (generic/631 now PASSING in-sweep) and
aborted at generic/634 — the >year-2262 timestamp test, adjudicated
kernel-interface-only (`a1ac9b2`: the i64-ns ±292-year range is a
deliberate finite-range choice — the ext4-u34/xfs-bigtime class — and
FUSE has no protocol field to advertise `sb->s_time_max`, so incore
cannot clamp like ext4/xfs; the daemon's deterministic saturation is
cargo-pinned, the six saturated rows pinned byte-exact as 634's
expected shape); the FINAL sweep runs from zero. All adjudicated
shapes (003/192/213, then +634) matched their pins exactly and
continued in-run — the expected-shape mechanism verified live.

**Tally:** the pre-merge from-zero attempts never completed as the
acceptance citation — the 2026-07-24 attempt FAIL-FASTED at generic/423
(the coarse-clock ctime inversion; fixed red-first, see the lineage
addendum), after which the user merged the campaign to `dev` and moved
acceptance to the merged tip. **The from-zero acceptance citation for
the gate is §10's run on `abac3d0`: `FAIL-FAST SUMMARY: 787 ran, 783
clean, 4 expected-shape, 0 unexpected` (exit 0).** Pre-merge sweeps
(fail-fast legs + the 423→end resume) validated every position at least
once on pre-merge binaries and are debugging-efficiency evidence only.

Standing adjudications (the ONLY classes, all pinned as exact expected
shapes in `tests/run_fstests.sh`):

- **generic/003 + generic/192** — ONE class: noatime by design (no
  read-path atime write exists; the JuiceFS reference posture) +
  kernel-TTL attr observation artifacts. 003's expected shape is
  exactly SIX ERROR lines (re-pinned post-37f1c86); 192 is the
  atime-delta face of the same ruling (expected shape: exactly
  "delta1 has value of 0" + "delta1 is NOT in range 5 .. 7"; delta2 =
  mtime stays in range). Any different diff = regression.
- **generic/213** — thin provisioning by design (`fallocate(mode=0)`
  reserves nothing on a sparse/dynamic backend); the expected shape is
  exactly the one missing ENOSPC golden line. Any different diff =
  regression.

**generic/464 and generic/074 are expected-PASS** (the 2026-07-21
campaign closed both classes; any diff on them is a regression by
definition).

## 6. Closing measurements (devsub / device-backed instruments)

All rows measured on the FINAL binary (`704a3df`), final boot,
counted runs restarted after every subsequent code fix (multi-run
discipline; the `d5033a5`-era run showed the same floors: drain
median 0.648, scrub 0.751, scan 0.648). Every number states its
instrument.

**Two floors were initially MISSED and fixed tests-first, not
documented around** (the standing bar):

1. **G-VL-5(c) scan floor** — the C1–C6 scan measured **0.28×** the
   census rate at 1 M inodes: pass 1 ran its four independent
   read-only scans (3 C1 tree walks, census, C4/C5 staging) as a
   serial SUM. Red `1f654c4` (cargo tripwire census/20 → census/4 +
   32 k fixture) → green `4381670` (unthrottled pass 1 spawns the
   per-(volume, tree) C1 walks and the staging scan concurrent with
   the census; throttled runs stay serial — KD-3's duty cycle is per
   worker). Post-fix the RAM-warm cargo instrument shows the scan
   BEATING the bare census walk (1,815,738 vs 1,419,779 inodes/s —
   its walks overlap the census's xattr reads).
2. **G-VL-3(b) drain floor** — drain-vs-raw-copy sat AT the line
   (medians 0.497/0.512 across two boots): `move_one` was a strict
   serial per-block pipeline paying 3× the block's bytes in
   serialized device round-trips. Red `442d09f` (park-one-publish +
   copy-gauge overlap instrument) → green `d5033a5`
   (`MOVER_PIPELINE_WIDTH = 4` join_all window for unthrottled
   drain/rebalance; defrag and throttled passes stay width-1 by
   design — order-dependent D1/D2 floors, per-worker duty cycle).

**Measured floors (final binary, final boot):**

| Gate row | Instrument | Result | Floor | Verdict |
|---|---|---|---|---|
| G-VL-3(b) drain ≥ ½ raw copy | devsub (zram nvmet-loop), 4 GiB urandom striped over 2 volumes, victim 2 GiB; raw baseline = dd 4 MiB O_DIRECT of a **urandom-prefilled** namespace pair (the v1 instrument read unwritten zram — a 27 GB/s zero-page memcpy, discarded as dishonest); drain timed at throttle 100, poll 0.1 s, `evacuate_bytes_moved` delta accounts every victim byte | drain 2.17/2.16/2.16 GB/s vs raw 3.32/3.35/3.36 GB/s → ratios 0.653/0.642/0.642, **median 0.642** | ≥ 0.5 | **PASS** |
| G-VL-5(c) C7 scrub ≥ ½ seq read | bandwidth-capped null_blk (memory_backed, `mbps=2000` — the design §5.6 devsub band), 3 GiB urandom striped; seq baseline dd 4 MiB O_DIRECT same device | scrub 1.60 GB/s vs seq 2.12 GB/s → **0.752** (0.751/0.753 on prior runs — scrub was already parallel enough) | ≥ 0.5 | **PASS** |
| G-VL-5(c) scan ≥ ½ census | same capped null_blk, **1,001,014 inodes**, cold process per probe (`df` offline vs `fsck` offline) | **0.677** (census ~800 k inodes/s vs scan ~540 k; 0.648 on the `d5033a5` run; pre-fix: **0.283** — the miss that forced fix 1) | ≥ 0.5 | **PASS** |

Instrument notes: (a) the null_blk `mbps` cap is a per-device token
bucket — cross-device serial copies refill during the peer's op, so
the drain row uses the REAL devsub (zram) with a prefilled baseline
instead; (b) the capped-nullb drain ratios (0.485–0.496 pre-fix) are
retained in the session logs as the supplementary instrument that
first exposed the serial-mover shape.

## 7. Honest-scope deferrals (the 2-node-devsub board)

Recorded by VL9's rider and carried here verbatim — these rows need the
2-node devsub substrate (nvmet-loop namespaces cross-mounted, a second
enrolled client) and remain OPEN on the G-VL-7/G-VL-5(d)/G-VL-8 gate
board; the single-node forms of each are covered (VL2b/VL4b/VL6b cargo
suites, rig legs 8/13/15, matrices m1–m4):

1. **Remote-worker ×10 rows** (G-VL-7): 2 enrolled workers executing
   evacuation + scrub shards with engagement via `job_remote_shards`;
   remote-worker kill-9 mid-shard ⇒ TTL ⇒ reclaim ⇒ converge ×10;
   the SIGSTOP live-zombie leg ×10; the post-job-end SIGCONT PR variant.
2. **G-VL-8 under the full matrix**: the 70/40/10 % imbalanced-set
   convergence row under sustained mixed writes at matrix scale (the
   rig-scale convergence row is green — leg 8).
3. **Repair-under-concurrency** (G-VL-5(d) online row): `--repair
   --apply` under live churn at matrix scale (the offline and
   single-node forms are green ×10 — VL6b suite + rig leg 15).
4. **Distributed C7 scrub ≥ 3× single-worker** (G-VL-5(c) scaling
   clause): the §5.1.6 wire dispatches WHOLE jobs only in v1.1 —
   shipping scrub sub-shards over the read-shard seam is the stated
   follow-up, recorded honestly in `JobType::wire_executable`
   (`src/jobs.rs`) and the `src/fsck.rs` header rather than silently
   half-shipped. The single-worker scrub floor IS measured (§6).

No floor in this closing report depends on any deferred row; they are
acceptance breadth, not correctness gaps. The remote-worker wire's
correctness envelope (fencing, fresh destinations, WERO preemption,
verify-reads) is fully pinned at the cargo layer.

## 8. Standing adjudications after this program

**fstests generic/003 + generic/192 (one noatime class), generic/213
(thin provisioning), and generic/634 (i64-ns timestamp range;
kernel-interface-only — FUSE cannot advertise s_time_max) — nothing
else.** The pjdfstest
expected-fail table is empty; LTP carries no expected failures; every
other historical fstests adjudication was killed by fixing the code
(464 EIO class, 464 wedge modes, 074 all three families, 013 wedge).

## 9. Cargo gates (final tree `445b412`)

| Gate | Result |
|---|---|
| `cargo clippy --all-targets --all-features -- -D warnings` | clean |
| `cargo fmt --check` | clean |
| `cargo test --all-features -- --test-threads=1` | **133 suites / 1,367 tests / 0 failures** (serial, final boot) |
| `cargo doc --no-deps` | 0 warnings |
| `cargo bench --benches -- --test` (criterion smoke) | all benches pass, 0 failures |

**The gate earned its keep THREE times on this branch** — regressions
caught by the full serial run, each fixed tests-first before the final
external sweeps:

1. **Foreign-zeros read regression** (`tests/ranged_read_tests.rs::
   rebind_under_movement_never_foreign_bytes`, red 4/6): bisected to
   `a9e40b6` (the generic/209 never-invisible write path) — the
   published overlay entry's seed class keyed on THIS WRITE's
   complement shape (`needs_existing_data`), so a fully-covering
   overwrite of an existing block sat in the map as a Fresh
   (zeros-complement) buffer with an empty coverage union during the
   absorb/sibling awaits, and a concurrent read's Fresh-gap branch
   composed ZEROS over real old bytes. Fixed in `704a3df`: the seed
   class keys on whether the BLOCK holds existing bytes (deferred
   item-B seed — readers materialize old bytes under the block lock);
   the covering write still pays no seed read (`record_write`'s
   completion transition clears the deferral — structural). Repro
   10/10 green; the 209 repro (write_visibility 16/16) and the
   seed-read tripwires stay green/0.
2. **Contract-5 harness rot** (`tests/writeback_fencing_livelock_
   tests.rs`): the VL10 O(map) delete-sweep fix closed the leak shape
   the test used as its precondition. The worker-side discard ladder
   contract is unchanged; the harness now stages the fold-refused
   beyond-layout block (its unit queues), lets the sweep clear the
   entry (asserted), and rebuilds the staged source under the
   flush-era token — the crash-window analog (`991317d`). Suite 5/5.
3. **Stale-binding decode EINVAL** (`tests/extent_overlay_tests.rs::
   reads_mid_fold_serve_exact_bytes`, ~15 % flake with siblings,
   bisected to `a9e40b6`): the never-invisible fold keeps the overlay
   visible across its authority transfer, so readers' deferred base
   reads now overlap the displace/free/reuse window — and on a
   transformed volume the dead incarnation's bytes legally FAIL frame
   decode (LZ4 ExpectedAnotherByte), which `get_block_for_index`
   propagated before its binding recheck. Fixed in `445b412` (red
   `9725b09`, deterministic stale-key/undecodable-image repro): a
   fetch/decode error on a NON-current binding is a counted rebind
   loss; still-current ⇒ propagate (real corruption never masked).
   reads_mid_fold 15/15 post-fix; read-path blast radius green.

---

## SESSION NOTE (2026-07-24, generic/795 fix ladder — IN PROGRESS, resume here)

**Rule compliance:** the 2026-07-23 resume-from-failure rule (`--resume-from`,
persisted order, from-zero acceptance unchanged) is landed (`2119d47`).
The third from-zero pass was killed externally; under the rule the sweep
resumes from generic/795 once it is green (it is the current abort point,
found by the resume pass at position 63/163 of the 634-slice).

**generic/795 fix ladder this session (red repro `10f08b6`):**

| Face | Root cause (tape-proven) | Fix | Commit |
|---|---|---|---|
| SIZE LED DATA (in-process) | write/cfr size publishes ran pre-dispatch; readers clamped to a size no overlay backed and composed zeros | all size publishes (attr cache, router meta floor, cfr dest) moved strictly post-dispatch | `2991ddb` |
| stale-absent binding | entry-time map snapshot missed a mid-read publish; hole verdict served zeros | `get_block_for_index` re-resolves None bindings freshest-first; single-block stale-None leg funnels through it | `2991ddb` |
| overlay retired mid-read | write-through/spill retired the overlay inside the read window | handler pre+post `capture_parked_runs` compose + **BLOCK_CUSTODY_EPOCHS** (per-(ino,block) seqlock bumped at every overlay/sibling/record retire — `retire_parked_overlay`, `NvmeCache::remove_active_block`) with a fingerprint-retry read loop; repro went ~1/4 → 120/120 green | `2991ddb` |
| destroy-under-live-fd | reclaim admitted at open-count 0; racing OPEN was granted onto the inode being destroyed; kernel `fuse_short_read` zero-extended the dead ino's empty replies to its cached i_size (sticky page-cache zeros — the fstests cmp signature) | two-sided open/reclaim handshake (claim-then-recheck vs count-then-refuse) + read() no longer fabricates size 0 from a getattr error; repro-port `open_racing_reclaim_never_reads_a_destroyed_ino` (kernel zero-extension half documented kernel-interface-only; 795+631+683+732 joined `SQUEEZEFS_FSTESTS_QUICK`) | `04a51b4` |
| spurious ENOENT regression of the handshake | claim-before-nlink-gates let FORGET storms transiently claim LIVE inos | admission gates cheaply first, claims only provably-dead inos | `4a5a4d2` |
| cfr copies missed source overlays | cfr source read hit base tiers only; parked acked bytes copied as zeros into the dest | cfr source read now runs the handler's pre/post parked-run compose (incl. past the base's physical tail) | `4a5a4d2` |

**Transport exoneration (evidence, /tmp tapes + session log):** per-reply
digests over a full failing run — 1.37 M reads, ZERO zero-page-carrying
replies; deliver=reply=commit exactly 1:1 (1,530,475 each), no anomalies;
arena 0xAB sentinel never surfaced; NO_ZC (copied replies) still hit;
classical-transport diagnostic session still hit; fuse2fs control CLEAN.
The corruption is daemon-side by elimination — never the fuse3 fork or the
kernel ring.

**REMAINING OPEN FACE (fstests generic/795 still FAILS under its full
load; all my standalone mount rigs — recopy, read-only, fsstress-loaded,
fstests mount options — are now CLEAN on the fixed binary):** dense fsv
copies durably-ish serve zero 4K pages in their TAIL-BLOCK region (e.g.
[8.37M,8.9M) of a 10M file) and mid-block-0/1 offsets; per-ino R-digest
shows every logged read of the bad region with z=60/64 while the SAME
run's cfr source chunks were clean (CFR-SRC-Z: 0 for the failing pair)
and RMW-SHORT-BASE: 0 for the fsv ino. Leg probes attribute serves to
LEG-HOT (R4 hot-block tier) and LEG-RANGED (device ranged read,
binding-current + incarnation-valid) — note b=2 (tail-block) hot hits
are partially false-positive-prone (legal zero tails past EOF) but the
zeros land INSIDE the sub-10M real range. Next steps: (1) probe the
NVMe read-cache range leg (leg B, unprobed); (2) hot-tier fill
provenance for tail-block keys (who publishes a hot entry with mid-range
zeros — suspect: a fill racing the tail block's writeback flush/new-key
publish, or `slice_whole_for_ranged` over a SHORT whole-block value);
(3) check `load_striped_block_keys`'s `block_map_id` arm — it consults
`block_map_cache` which NOTHING ever populates (`.get`-only; every miss
resolves the block as a hole) — dead-or-broken arm, decide delete vs fix;
(4) re-run `sudo bash tests/run_fstests.sh generic/795` after each fix,
then `--resume-from generic/795`, then the from-zero acceptance pass.

Working-tree hygiene at handover: ALL temporary probes stripped
(SQZ_795_PROBE / 795p / sentinel / classical knob / runner env injection
all reverted); /tmp/xfstests-dev/tests/generic/795 restored pristine
from /tmp/795.orig; rigs live in /tmp/vl10_795_mount*.sh (3=buffered,
4=O_DIRECT, 9=pread-forensic, 11/12=fsstress+fstests-opts, 13=creation-
only, 14=read-only). Branch `test/vl10-release-gate` @ `4a5a4d2`,
UNMERGED, no tag.

### 2026-07-24 addendum — generic/795 CLOSED (the whole-file-clone face)

The remaining open face was run to ground and fixed; **fstests
generic/795 passes 3/3** (first passes ever recorded — the test sits past
the 634 abort position, territory no prior sweep reached).

| Face | Root cause (tape-proven) | Fix | Commit |
|---|---|---|---|
| dead `block_map_cache` | populate sites died with the Redis removal (`23ed315`); every miss fabricated a HOLE (fabricate-zeros engine; arm proven dead in-situ — 0 fires) | map-id-without-inline-map arm re-resolves authoritatively from the backend, fails loud if unresolvable; sync leg demotes; cache deleted (field/builder/gauge/mem-budget) | `72feca1` |
| whole-file clone vs parked custody | cfr's off 0→0 full-length empty-dest fast path (`clone_file`) refcount-cloned the striped source's DURABLE-ONLY map; the pre-clone freeze scanned RAM overlays only, so custody at the staged-sibling station (reader-flush/memory-pressure spill, merge queued — the busy-mount steady state) was invisible → full-size clone with unmerged blocks reading ZEROS durably (backtrace: `copy_file_range → clone_file → save_metadata_to_backend` minting striped/10M for an ino with zero writes; SAVE/MI/W/C attribution tape) | 3 layers: cfr pre-guard freeze drains staged siblings; clone fast path gated on a freeze-clean verify (any below-size unbound/overlaid/sibling'd/record'd block demotes to the chunked path, which composes parked runs since `4a5a4d2`); `clone_file` refuses loud on undrained custody | `72feca1` |

Repro-port: `whole_file_clone_carries_parked_source_custody`
(deterministic RED on the pre-fix tree — LOST WRITE at the parked tail
block — green with the fix). Investigation instruments (all stripped
before commit): per-op lifecycle tape (CR/W/C/C-ERR/SA/REL/OP/UN),
meta-insert/SAVE/FMB/MERGE attribution with backtrace-on-anomaly, and
per-leg zero-serve probes; the runner's env injection reverted; the
patched local xfstests test restored pristine. Also noted for the
record: generic/795 reformats+remounts scratch 5× per invocation
(`_scratch_mkfs` per run) and each dismount left ~200 unflushed staged
files — the staging generation binding (format wipes + marker) held
correctly throughout (0 mismatch adoptions; verified in-tape).

### 2026-07-24 — resume/acceptance lineage (honest record)

* `72feca1` landed → `generic/795` single ×3: **PASS, PASS, PASS** (first
  greens ever for this test).
* `--resume-from generic/795` (binary `72feca1`): ABORTED at test 1/3 —
  795 itself failed with the `cp … Resource temporarily unavailable`
  flake (cfr's temporary-lease 5s wait lost to a conveyor-stall guard
  hold; tape: `lock Ino(27064) still held after 5s wait budget`).
  Fix `998b619` (cfr cached leases + bounded stall retry; repro-port
  documented-exception — >5s stall race, in-QUICK regression net).
* Binary `998b619` (counted restart): `generic/795` ×3 **PASS**, then
  `--resume-from generic/795` → **end of list clean** (795 PASS,
  generic/796 PASS, generic/797 not-run [xfs_io fiemap unsupported —
  harness not-run, not a failure]). FAIL-FAST SUMMARY: 3 ran, 3 clean,
  0 expected-shape, 0 unexpected.
* FINAL from-zero `-g auto` acceptance pass on `998b619`: launched
  2026-07-24 (the sole acceptance citation; resume passes above were
  debugging efficiency only, per the 2026-07-23 rule).
* That pass **FAIL-FASTED at `generic/423`** (test 418/787; everything
  before it green or documented-expected-shape). The statx hard-link
  leg's `ts=C,c`: the linked file's ctime observed 162 µs BEHIND the
  reference socket created BEFORE the `ln` (`ctime.nsec is before
  ref_c.nsec (416747177 < 416909528)`).
  - Root cause (a LATENT ≤1-tick race, NOT a branch regression — the
    795 family and create-attr-refresh work never touched timestamp
    authoring): the daemon stamped inode times from fine
    `CLOCK_REALTIME` while the kernel authors wb-cache regular-file
    link-ctime locally from `CLOCK_REALTIME_COARSE`
    (`inode_set_ctime_current`), which lags fine by up to one tick
    (1.85 ms measured on this HZ=1000 host). Daemon-stamped socket
    (mknod) > kernel-stamped link ctime inside one tick ⇒ inversion.
    Earlier 423 greens were the window not firing.
  - Second face caught by the repro's chain test: `link()` (all three
    arms) and the setattr commit-arm AUTO-bump stamped ctime over the
    UNFOLDED inode base — a parked pending-times refinement (absorbed
    kernel echo) ahead of `now` regressed an already-served view.
  - Ladder: red repro `1d25ee8`
    (`daemon_inode_stamps_never_lead_the_kernel_coarse_clock` — red
    round 0, +624 µs lead; `link_ctime_is_monotone_against_prior_
    observations` — red round 2 post-clock-fix, the parked-echo face) →
    fix `1374429` (`coarse_realtime_ns()` becomes THE inode-timestamp
    clock across `now_ns()`/write-path attr publish/fuse3
    `FATTR_*_NOW`; fold→signed-monotone-bump→retire in link /
    routed_link_local / routed_nlink_adjust and the setattr auto-bump;
    strict time-ADVANCE tests cross a coarse tick, kv_backend's
    reference clock rides the same domain) → QUICK-set growth
    `f2818c0`. Repro green ×6.
  - Per the counted-restart rule the from-zero acceptance count
    RESTARTED on the fixed binary (423 single ×3 → `--resume-from
    generic/423` → fresh from-zero pass; rows below).
* **Full cargo suite enforcement caught a second regression** (take-2/3
  of `cargo test --all-features -- --test-threads=1` on the 423-fixed
  tree): `write_through_tests::test_one_shot_full_block_write_through`
  red 8/8 DETERMINISTIC — bisected to **my own `2991ddb`** (honest
  provenance): moving the size publish post-dispatch (correct) left the
  clean RAM floor entry as the SOLE carrier of a striped write's true
  end; any TTL refill / evict-and-refetch regressed size to the lagging
  durable value and reads clamped acked tail bytes away. Fix `d284334`:
  `update_metadata_cache_size` marks the grown entry `layout_dirty`
  (the existing local-authority rule; persisted+cleaned by
  `persist_dirty_layout_if_needed` on the fsync/release cadence); test
  pins the dirty floor + fsync-driven durable size. (Two earlier
  suite-run one-off failures — `read_prefetch_pipeline_tests::
  pipeline_phases`, `job_wire_tests::lease_expiry_*` — were attributed
  to a runaway `cgc watch` indexer holding **11,305 runnable threads /
  load 13k** on the box; killed, both tests green ×26 and ×green in the
  final full pass. Recorded as environment, not product.)
* **Full cargo suite (final tree `d284334`): 133/133 test binaries
  green, zero failures** (`--test-threads=1`, quiet box).
* Binary `d284334` (counted restart): `generic/423` single ×3 **PASS,
  PASS, PASS**.
* Binary `d284334` (pre-merge): `--resume-from generic/423` → end of
  the persisted expansion order — completed clean (debugging-efficiency
  leg; see §10 for the only acceptance citations).

---

## 10. Acceptance campaign on the MERGED tip (`test/vl10-acceptance`, 2026-07-24)

Per the sequencing note at the head of this report: all fixes merged to
`dev` first (`abac3d0` — includes the whole VL10 ladder, the 795
quartet, 423, wedge/074/RW5-A, CLI help/config/status, and the 1.1.0
release-train versioning), then this campaign produced the release-gate
acceptance evidence on that tip. Branch `test/vl10-acceptance` off
`dev` @ `abac3d0`; binary:

```
squeezefs 1.1.0 (abac3d00c98a / abac3d00c98aec56055055888aa06d0fbaa4f1e5) built 2026-07-24T22:16:41Z
```

Instruments: the house runners on this box (32-core, 109 GiB RAM,
Arch/CachyOS kernel 7.1.4, post-reboot boot), file-backed `/dev/shm`
volumes + tmpfs staging, release binary. Counted-restart discipline in
force; the from-zero runs below are the gate's acceptance citations.

| Suite | Run | Result |
|---|---|---|
| fstests full `-g auto` (from zero, fail-fast) | `sudo tests/run_fstests.sh` | **PASS** — exit 0, `FAIL-FAST SUMMARY: 787 ran, 783 clean, 4 expected-shape, 0 unexpected`; the four `.out.bad` artifacts are exactly the adjudicated set {003, 192, 213, 634}, each matching its pinned shape (the runner aborts on any other diff); generic/795 PASSED in-run (245 s), generic/423 clean; 8h07m wall (2026-07-24 18:20 → 2026-07-25 02:27 EDT) |
| LTP full (abort-on-first-unexpected) | `sudo tests/run_ltp_syscalls.sh` | **PASS** — exit 0: `PASS: 174, FAIL: 0, BROKEN: 0, SKIPPED: 9` (skips are TCONF, continue-by-rule); 6m50s wall |
| pjdfstest full | `sudo tests/run_pjdfstests.sh` | **PASS** — `Files=238, Tests=8798 … All tests successful, Result: PASS` (158 s wall; prove=0 verdict=0) — re-cited on this binary (matches the pre-merge §5.1 run) |

**Release-gate verdict on `abac3d0`:** all three suites pass from zero
on the merged tip — fstests full `-g auto` fail-fast-clean (783/787 +
the four pinned expected shapes, zero unexpected), LTP 174/174 (9
TCONF), pjdfstest 8798/8798. The user ruling 2026-07-20 ("every release
passes all three; known-failure exceptions require a documented
adjudication") is satisfied: the only exceptions are the §5.3b/§8
standing adjudications, each matched byte-exact in-run. **GO for a
`stable-2026.07` release tag** (tagging is the release act and is the
user's — not performed here).

The §9 cargo gates were last run in full on the pre-merge final tree
(`d284334`: 133/133 test binaries green serial, clippy `-D warnings`
clean, fmt clean, doc clean, bench smoke green); the merge to `dev` was
ff-only content-identical modulo the release-train versioning PRs, whose
own gate ran on their branch. This acceptance campaign changed no code
(docs/markdown only — this report), so per the tiered gate the
markdown check is its required gate.

**Program disposition: CLOSED.** The v1.1 volume-lifecycle charter is
shipped (§1), the ladder is merged (§2, tip `abac3d0`), every G-VL gate
is adjudicated green (§3, §6), the release gate has its first full
three-suite from-zero pass on the merged tip (§10), and the honest-scope
deferrals stand recorded on the 2-node board (§7).
