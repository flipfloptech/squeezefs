# Rung 10 — the full Linux gate in under eight minutes (2026-10-13)

The owner: "Is it possible for us to make the full linux gate faster? It seems awfully slow in the
beginning; we just hang out with `sdk c smoke: ok …` on screen for a while." A green quiet-venue
gate took 17–19 min (`/build/tmp/ho-gateR1.log`; R1→R2→R3 finished 19 min apart; 19 m 16 s
measured on this branch's base, §5.1), with nothing on screen while its two long serial lanes
ran. Target: ≤ 8 min warm, the same pass/skip totals, at least as deterministic as before.
**Result: 6 m 11 s / 6 m 09 s / 5 m 59 s warm, 7 m 29 s cold, the same totals (+8 new tests), and
macOS 7 m 15 s → 2 m 44–2 m 52 s** (§5).

Every number below is the venue (`sqz-zig`, 9 vCPU / 16 GiB, kernel 7.2.6-sqz) unless it says
the dev box (macOS, 11 cores). "Warm" is the per-commit condition the venue recipe uses: the Zig
caches hold std and the vendored C, a touch to `src/core/clock.zig` recompiles the library, every
test reruns. Measurement scripts are not committed (they wrap `zig build` with a 2 s sampler of
`/proc/stat`, `MemAvailable` and per-comm RSS); their raw outputs are under `/build/gs/` on the venue.

## 1. Measured before

### 1.1 The step tree (ho-gateR1, the owner's green gate)

| lane | step | wall | peak RSS |
|---|---|---:|---:|
| unit | compile test squeezefs-unit | 1 m | 5 GB |
| unit | run test squeezefs-unit (2,633 tests, ONE process, sequential) | **10 m** | 1 GB |
| product | compile exe squeezefs (ReleaseSafe, thin LTO) | 2 m | 4 GB |
| product | run test squeezefs-smoke (89 rows, ONE process) | **11 m** | 0.9 GB |
| product | → bench-smoke → shim-smoke | 9 s + 11 s | |
| product | → nvmet-smoke | 2 m | |
| product | → suites-smoke (pjdfstest subset) | 2 m | |

`build.zig` chained smoke → bench → shim → nvmet → suites with `dependOn` ("one mount on the venue at
a time"); the critical path was compile exe → smoke → bench → shim → nvmet → suites ≈ 17.5 min.

### 1.2 CPU and memory during a gate

The baseline run (`/build/gs/base1`: this tree's runner, the old structure — unit ∥ smoke, one
process each; 904 s for compile + both) used **1.06 cores of 9 on average (11.7 %)**. The compile
minutes ran 1.3–2.3 cores (the compiler is mostly one thread); the test minutes 0.2–1.6. Memory
never bound: ≥ 7.4 GiB available while both compilers ran (6 GiB + 4 GiB peaks), 13–14 GiB during
the tests (≤ 5 daemons, ≤ 1.7 GiB of daemon RSS, ≤ 1.1 GiB of test processes).

### 1.3 Per test (the unit binary, unsharded, `base1`)

2,642 tests, Σ 617 s. p50 0.1 ms · p75 2 ms · p90 50 ms · p95 289 ms · p99 5.5 s · max 31.5 s;
107 tests ≥ 1 s carry 95 % of the time, 13 tests ≥ 10 s carry 41 %.

| # | wall s | cum % | test |
|---:|---:|---:|---|
| 1 | 31.5 | 5 | meta.sym.membership.volume_bind.test.linux: every crash window of the §5.9 procedure over the r… |
| 2 | 30.7 | 10 | meta.sym.handover_moves.test.linux: THE MOVE MATRIX — offer pushed, stand raised (0003 §X.1): B… |
| 3 | 27.2 | 15 | ipc.host.service.test.the park ladder has no lost wake: 20 k ops from a client thread with rand… |
| 4 | 27.2 | 19 | meta.ledger.test.property: slot decode is total on arbitrary and restamped bytes; a decoded rec… |
| 5 | 25.4 | 23 | meta.routing.offline.test.add-meta grows a 2-set to 3 (two loaded slots taken, child-first clai… |
| 6 | 22.0 | 27 | meta.routing.migrate.test.preflight refusals: slot 0 is pinned; a target past the set; and the … |
| 7 | 19.6 | 30 | wire.tls_e2e_linux.test.tls e2e: SCOPING — connect+handshake, record throughput and CPU per GB … |
| 8 | 14.8 | 32 | meta.smo.test.scale: ≥ 100k keys at 64 KiB nodes ⇒ depth ≤ 3 (the fanout tie), every key found,… |
| 9 | 11.4 | 34 | meta.sym.membership.volume_bind.test.linux: THE PRIMING LAW, red then green — a writer whose fi… |
| 10 | 10.6 | 36 | meta.sym.handover_moves.test.linux: THE MOVE MATRIX — step 0 drained, A's door closed (0003 §X.… |
| 11 | 10.6 | 37 | meta.sym.handover_moves.test.linux: THE MOVE MATRIX — door re-opened (0003 §X.1): B's five ops … |
| 12 | 10.5 | 39 | meta.sym.handover_moves.test.linux: THE MOVE MATRIX — grant landed, B's install mid-way (0003 §… |
| 13 | 10.0 | 41 | fuse.transport.session.test.e2e: a 10 s random-op storm from 4 threads never needs the 100 ms b… |
| 14 | 9.7 | 42 | meta.guard.gate.test.the recovery ladder step 2 on PR: a TTL-stale foreign claim with a foreign… |
| 15 | 8.7 | 44 | meta.sym.bind.test.the handover's kill matrix: a death after the page named the slot Releasing … |
| 16 | 8.4 | 45 | meta.guard.gate.test.THE FENCE LATCH on the device path: an injected reservation conflict on (a… |
| 17 | 8.2 | 46 | meta.leaf_merge.test.cross_parent_underfull_leaves_shrink_through_interior_merges_to_a_fixed_po… |
| 18 | 7.9 | 48 | ipc.client.test.e2e: no-wait tickets + the completion-doorbell park over 100 k ops with randomi… |
| 19 | 7.6 | 49 | meta.guard.gate.test.a stale cross-host claim on a NON-PR volume refuses naming `claim clear`; … |
| 20 | 7.1 | 50 | meta.sym.enable_symmetric_bind.test.linux: enable-symmetric end to end — a --single-writer volu… |
| 21 | 7.0 | 51 | meta.sym.enable_symmetric_bind.test.linux: enable-symmetric refuses a MOUNTED volume (the flock… |
| 22 | 6.9 | 52 | meta.backend.test.kill -9: a child mid-commit-stream is SIGKILLed; the reopened volume holds ev… |
| 23 | 6.9 | 54 | meta.sym.membership.volume_bind.test.linux: a dead in-process region recovered over the real ba… |
| 24 | 6.8 | 55 | meta.guard.gate.test.the recovery ladder step 1: a FRESH claim of a provably dead same-host pid… |
| 25 | 6.7 | 56 | meta.sym.membership.volume_bind.test.linux: THE COHERENCE HOLD — a write into a recovered slot … |
| 26 | 6.1 | 57 | meta.backend.test.the D4 faces on a device-backed volume: one xattr band rewritten across check… |
| 27 | 5.5 | 58 | io.uring_fs.test.fd cache churn under load: 16 threads over 200 distinct files with a 16-entry … |
| 28 | 5.1 | 58 | meta.leaf_merge.test.height_three_tree_collapses_to_a_root_leaf_gap_free_with_readers_mid_walk:… |
| 29 | 5.1 | 59 | wire.tls_e2e_linux.test.wire e2e over MutualTls, the kernel record layer's law: the kTLS probe … |
| 30 | 4.6 | 60 | meta.sym.enable_symmetric_bind.test.linux: enable-symmetric's resume over the device — a run ki… |

### 1.4 Per row (the smoke binary, unsharded, `base1`)

90 tests (47 rows + 43 anonymous `test {}` blocks every filtered binary carries), Σ 705 s;
p50 2.2 s · p90 21 s · max 123 s; 20 rows ≥ 10 s carry 78 %, 6 rows ≥ 30 s carry 47 %.

| # | wall s | cum % | test |
|---:|---:|---:|---|
| 1 | 123.0 | 17 | THE SHARED HOT DIRECTORY ACROSS DAEMONS — A (the manager) mkdirs `hot`; B JOINED storms creates… |
| 2 | 49.5 | 24 | THE JOBS MATRIX — format (1 MiB blocks, a cache path) → fill (packed, striped, a 1.5 GiB block-… |
| 3 | 45.5 | 31 | ONLINE FSCK THROUGH THE ADMIN LANE — the fill (dirs, 200 packed files, hard links, three fsync'… |
| 4 | 44.9 | 37 | FSCK AND SCRUB — format (a cache path) → fill (dirs, 200 packed small files, hard links, three … |
| 5 | 35.7 | 42 | THE FLEET READ SHARD — mount A (the coordinator) + B JOINED with the fleet on: B enrolls at A a… |
| 6 | 33.6 | 47 | THE BLOCK-MAP TREE — a 1.5 GiB file at 1 MiB blocks crosses the inline cap: kill -9 mid-crossin… |
| 7 | 27.3 | 51 | THE KILL MATRIX — kill -9 at 50/100/150 MiB of a 200 MiB dd and mid 8-way cp storm → fsck --off… |
| 8 | 20.8 | 54 | DEFRAG --fold AND --meta ON A LIVE MOUNT — format (64 KiB meta nodes, 1 MiB blocks, a cache pat… |
| 9 | 20.6 | 57 | THE FLEET'S MERGED BLOCK CENSUS — mount A + B JOINED with the fleet on and B under the `SQUEEZE… |
| 10 | 19.6 | 60 | A FROZEN DAEMON NEVER HANGS THE HOST'S `sync` (0003 §Z) — format → mount under the request-time… |
| 11 | 17.0 | 62 | THE KILL ROWS (c) — `appender clear` refuses a live-mounted set; after A's clean umount parks B… |
| 12 | 15.5 | 64 | `umount --lazy` of a BUSY mount — the verb returns at once naming the holders it leaves, the mo… |
| 13 | 15.4 | 66 | THE REKEY ROW — format (1 MiB blocks, lz4 + aes-256-gcm, key A, a cache path) → fill (40 packed… |
| 14 | 13.5 | 68 | the data plane WITH a cache path — dd 200 MiB sha round trip, cp 100 MiB + cmp, truncate, 4 KiB… |
| 15 | 13.4 | 70 | the data plane WITHOUT a cache path — staging and the NVMe tier OFF, every write beyond inline … |
| 16 | 12.6 | 72 | THE KVDA PAGE SINK — A holds the leases with its pages on volume 0 and every carve's delta in r… |
| 17 | 12.1 | 74 | THE KILL ROWS (a) — kill -9 B mid-`tar -x` into A's directory ⇒ A evicts B at T_owner, records … |
| 18 | 11.1 | 75 | a SET format — meta + 2 data files + lz4 + aes-256-gcm + a cache path → both data superblocks w… |
| 19 | 10.4 | 77 | A STRIPED DIRECTORY ON ONE DAEMON — mkdir hot → the explicit flip (`user.squeezefs.stripes` = 6… |
| 20 | 10.1 | 78 | THE ONLINE VOLUME ROW ACROSS DAEMONS — mount A (the manager) + B JOINED → `volume add-data --no… |
| 21 | 9.2 | 80 | a TWO-MEMBER metadata set through the binary — format sqmeta://a,b → mount → 2000 files over 8 … |
| 22 | 9.0 | 81 | TWO MOUNTS OF ONE SET ON ONE HOST, one cache path — each member binds its OWN staging root (000… |
| 23 | 8.5 | 82 | THE KVDA PAGE SINK's fallback — the same row with the dying holder's page writes SKIPPED (the s… |
| 24 | 8.2 | 83 | THE TWO-DAEMON MATRIX over MutualTls on KERNEL TLS — `key tls-init` + `tls-issue` ×2 → mount A … |
| 25 | 8.1 | 84 | THE N-DAEMON POSTURE, the manager half — format (the symmetric default; meta + 2 data, cache pa… |
| 26 | 8.1 | 86 | THE TWO-DAEMON MATRIX — mount A (the manager) → mount B as a SECOND PROCESS joined over TCP (no… |
| 27 | 6.9 | 87 | THE `-o ro` READER OF A LIVE SET — C mounts `-o ro` beside A (the manager) and B (joined): no f… |
| 28 | 6.8 | 88 | A JOINER'S CLEAN UNMOUNT AND REJOIN while the manager stays up — mount A (the manager) → B JOIN… |
| 29 | 6.6 | 88 | `volume enable-symmetric` — format --single-writer (meta + 2 data, a cache path) → mount, a tre… |
| 30 | 6.1 | 89 | A MEMBER'S CACHED COPY OF ANOTHER MEMBER'S BLOCKS NEVER OUTLIVES THE BLOCKS' RE-USE (0003 §W.2)… |

### 1.5 The dev box

`zig build gate` on macOS: **7 m 15 s**; the unit run 6 m (2,116 tests, one process), its compile
39 s. Its unit Σ is 394 s of CPU-bound Debug work; one test, `meta.smo.test.scale: ≥ 100k keys at
64 KiB nodes`, is **132 s** (33.5 %) there and 15 s on the venue.

## 2. What dominated, and what was done about it

### 2.1 Every clean dismount waited out the 2 s liveness tick (fixed)

Timing an `fsck --offline` line by line put 1.97 s between "dismount teardown completed" and
"clean unmount" — on a volume with three files. The fsck's own work was 4 ms. Sampling the
process during the gap: the checkpoint and lane threads idle, the executor lane waiting.

`Guard.stopHeartbeat` stored the stop word (`hb_stop.store(1)`) and then called `tick.wake`,
whose release swap delivers the token and wakes the futex only if the word held none — the store
had already put one there, so the futex wake was skipped and the heartbeat's park (its 10 s beat)
ended only when the 2 s tick rescued it. Every clean unmount, every `fsck --offline`, every
offline verb that opens a volume read-write paid up to 2 s.

- Red: `meta.guard.gate.test.a clean dismount wakes the parked heartbeat …` — "the dismount took
  2026 ms (the liveness tick is 2000 ms)" (`5cee56c`).
- Fix: the stop goes through `tick.wake` alone (`aceafd9`). `squeezefs umount` **1.97 s → 40 ms**,
  `fsck --offline` **2.14 s → 0.22 s**; the unit binary's Σ 617 s → 321 s (the guard, membership,
  bind and crash-window tests open and close volumes dozens of times: `volume_bind … every crash
  window` 31.5 s → 4 s); the smoke's Σ 705 s → 494 s.

### 2.2 The rows and tests that remain long

With the CPU time per test now in the table (§3.2), the long ones split cleanly:

- **Compute (CPU ≈ wall)**: `meta.ledger.test.property` 28 s, `meta.smo.test.scale` 18 s (132 s on
  the dev box), the leaf-merge suites, `THE SHARED HOT DIRECTORY ACROSS DAEMONS` (76 s, 94 %),
  `THE BLOCK-MAP TREE` (35 s, 97 %), `THE JOBS MATRIX` (44 s, 80 %). Kept — they are the work they
  test; sharding gives each its own process.
- **The hot-directory row's fsck is superlinear (a finding, not fixed here).** Before the
  dismount fix the row took 123 s, of which its closing `fsck --offline --scrub` took **83.5 s for
  52,994 entries** in one 64-stripe directory. A plain directory measures 0.04 s / 0.10 / 0.31 /
  1.13 s at 2 k / 4 k / 8 k / 16 k entries (×2.4–3.6 per doubling, near-quadratic); the striped
  one costs ~17× more per entry (15 k striped entries 19.1 s in another run). The row's storms run
  until its T_owner evictions land, so its file count — and the fsck's cost — floats with the
  venue's speed (15 k–53 k entries). Owed to the fsck owner: the per-entry check of a large (and a
  striped) directory.
- **By design**: `ipc.host.service.test.the park ladder has no lost wake: 20 k ops … randomised
  gaps` (23 s at 2 % CPU — its sleeps ARE its stimulus), `fuse.transport.session.test.e2e: a 10 s
  random-op storm` (10 s), `wire.tls_e2e_linux.test.tls e2e: SCOPING …` (22 s, a throughput
  scoping run inside the unit gate). Kept.
- **Waiting on real timers a seam could drive (owed, not on the critical path now)**:
  `umount --lazy of a BUSY mount` (15.5 s at 7 % CPU) waits up to `3 × PROBE_INTERVAL_NS` (5 s,
  no seam) for the supervisor's next probe; `THE KILL ROWS (a)/(c)` and the fleet rows (11–21 s at
  12–19 % CPU) wait on T_owner evictions and renewal cadences already under the T_owner seam;
  `sync.notify`/`sync.rwlock` tick tests (4.5 s, 0 % CPU) wait one real 2 s tick each. With the
  smoke sharded these shorten the sum, not the wall: the shards balance at ~2 min each (§5).

### 2.3 Hidden coupling sharding exposed (fixed)

A shard runs a subsequence of the unsharded order, so a test whose verdict depended on what ran
before it in the same process changed verdict with the partition; and the gate's host is now
busier (3.3 cores on average instead of 1.1), so a test that assumed an idle host met a loaded
one. Probing the unit binary under 2–9 shards on both hosts, the gates themselves and 8 × `yes`
found:

| test | coupling | fix |
|---|---|---|
| `ipc.doorbell` submit / service-park (dev box) | the parked daemon's wake is a raw Linux futex trap; on macOS it ran whatever syscall a previous test left in x16 — SIGSYS under one plan | skip `no_linux` before the wake (`e22c7fd`) |
| `fuse.handler.watchdog … never cancels` (dev box) | the fsync class's threshold derives from the process-wide latency histograms earlier tests fed | tick past the derived threshold (`7842cbb`) |
| `meta.checkpoint_cycle … the tick` (dev box, 11 shards) | asserted the lateness < 200 ms — real time between two statements, i.e. the host's speed | assert the term in whichever regime the lateness fell (`a16f5f1`) |
| `io.uring_fs … misaligned direct writes refuse` (venue, 3/3 gates on the regenerated plan) | requires the refusal tripwire's log, which logs once a second per site — the loop-device refusal test ran just before it | `resetLogBudgetForTest()` first (`4239c0f`) |
| `exec.lane … DROP_WAKE` (venue gate) | timed the healed wake from the spawn; a lane that had not reached its park took the spawn from its inbox scan and needed no tick | wait for the lane IN its park (its first two enters), time from before the lane exists (`cf1bbe1`) |
| `data.reclaim … the debt drainer` (venue gate) | started the drainer between two frees it expected coalesced, and read the command list when the debt hit zero — the pass cancels the debt before it issues the command | land both frees first, wait for the claims' return (`cf1bbe1`) |
| `sync.mpmc … a pool free list at its population` (venue gate) | asserts the race it provokes happened within 200 k rounds — two threads ON CPU together, which a loaded host can deny | cycle until a refusal is seen, ≤ 50× the rounds (`cf1bbe1`); under 8 × `yes` these three failed 8/20 before, passed 30/30 after |
| uring128 scratch files, the flock probe, the key stash | fixed `/build/tmp` names | pid-tagged (`9a73535`) |

One assertion is load-sensitive beyond the derived shard count and is left to its owner:
`meta.backend.test.ONE thread per volume … fiber parks per window ≤ 2` failed once with **8**
unit shards on 9 vCPUs (`parks ≤ 2 × lane_writes + 1`: uncoalesced admit wakes under preemption
add parks); it passed in every run at the derived 4 (both hosts, every gate below).

## 3. The design

### 3.1 The project test runner (`tests/test_runner.zig`)

Zig 0.17's `lib/compiler/test_runner.zig`, kept whole — the build-runner protocol over
`--listen=-`, the error-log rule (`log_err_count`), the leak check, `error.SkipZigTest` (so
`testkit.skip`, its ledger and `-Drelease-gate=true` behave exactly as before), the fuzzer hooks —
plus:

- `--shard=I/N | --shard=exclusive`: in server mode the runner answers `query_test_metadata` with
  its part only and maps `run_test` indices onto it, so a step's counts are its part's and a set's
  parts sum to the unsharded run. A crashed part the build runner respawns recomputes the same plan.
- `--costs=PATH`: the committed table the plan balances by; `--costs-out=PATH`: what this process
  measured — wall and CPU (`getrusage` self + reaped children: a smoke row's daemons) per test.
- `--no-exclusive-step`: a `[venue-exclusive]` test in a binary without an exclusive step refuses
  the run by name (it would otherwise run nowhere).
- **The heartbeat.** The build runner buffers a test process's stderr until the step ends and
  draws progress only on a TTY. When its own stderr (`/proc/<ppid>/fd/2`) is a pipe, a thread
  writes there every 30 s — `[squeezefs-smoke 3/4] 46/55, 1m00s; running "cli.smoke.test.smoke:
  THE JOBS MATRIX — format (1 MiB blocks, a cache path) → fill (packed, …" for 13s` — and one line
  at the end: `[squeezefs-unit 2/4] done 660/660 in
  1m17s (the table's plan 1m22s); 24 skipped, 0 failed`. A regular-file redirect gets none (a
  reopened file has its own offset); a terminal has the live tree (`scripts/venue` now passes `-t`
  when its stdin is one).

### 3.2 The plan and the table (`tests/shard_plan.zig`, `tests/costs/`)

Greedy LPT: sort by cost descending (index ascending on ties), give each test to the least-loaded
part (lowest-numbered on ties); each part runs its tests in index order. A test without a row
costs the median of the matched rows; with no table every test costs alike and LPT is round-robin.
`tests/costs/<binary>.<os>.tsv` holds `<name>\t<wall µs>\t<cpu µs>`; `zig build costs` regenerates
the unit and smoke tables from this venue's gate runs (byte-sorted, one stable diff). **The rail**:
the runner refuses a table for which more than a tenth of the binary's tests have no row, or more
than a tenth of the table's cost sits on rows naming no test (a renamed heavy row) — deterministic,
names only, never a timing. The rails binary tests the plan (seven tests).

### 3.3 The derived shard counts (`build.zig` `HostFacts`)

| set | formula | venue (9 cpu, 15.6 GiB) | dev box (11 cpu, 36 GiB) |
|---|---|---:|---:|
| unit | `min(cpus / 2, RAM / 2 GiB)` | **4** | **5** |
| smoke | `min(cpus / 2, RAM / 3 GiB)` | **4** | 1 (off Linux every row skips) |

Why: a unit part's resident peak is ≤ 1 GiB (the whole unsharded run's), ×2; a smoke part's is the
test process (≤ 0.8 GiB) plus a three-daemon row's daemons (≤ 0.6 GiB each), rounded to 3 GiB. The
cores bound is measured, not assumed: 11 unit parts on the 11-core dev box ran its longest test
2.3× slower than alone (5 m for the 132 s test; the gate took 6 m 40 s against 7 m 15 s unsharded)
and met timing-bounded tests with an oversubscribed host; at 5 parts the dev box's gate is 2 m 45 s.
On the venue the compile of the binary is the critical path's head and keeps its core. `-Dunit-shards`
/ `-Dsmoke-shards` override; a `-Dtest-filter` run is one process (a developer's loop).

### 3.4 The gate's graph

Every test step of the gate now runs side by side; the only orderings left are real resources:

| dependency | the shared resource |
|---|---|
| smoke / bench / nvmet parts → compile exe | the binary under test |
| shim smoke, shim census → `zig build-lib` of the shim | the staged `.so` |
| unit parts → compile test squeezefs-unit; smoke parts → compile test squeezefs-smoke | the one test binary each set runs |
| **the exclusive class → every other test step of the gate** | the host itself: the frozen-daemon row's host-wide `sync` visits every FUSE mount on the host, and it SIGSTOPs a daemon (another stopped task voids it, `otherStoppedTask`) |

The removed chain (smoke → bench → shim → nvmet → suites) was "one mount on the venue at a time";
every rig was audited for what it shares (§4). The exclusive step comes twice: in the gate (after
everything) and in the binary's own step (`zig build smoke`, after the smoke's parts).

## 4. Shard isolation audit

| hazard | where | verdict |
|---|---|---|
| fixed paths | uring128 scratch files, the flock probe, the key stash | pid-tagged (`9a73535`); every rig dir and mountpoint was already `-<pid>` (smoke, bench, shim, nvmet, guard, IL host, suites) |
| fixed ports | wire/TLS e2e, the daemons' listeners | `127.0.0.1:0` throughout; nvmet ports run-scoped since `48a8dc2`/`a9fa84e` |
| nvmet configfs | the fidelity tier | run-scoped names and censuses (`a9fa84e`); one nvmet step per gate |
| global cgroup census | `mem/budget.zig censusFleetShare` (daemons count same-comm peers) | unit tests read fixture roots (`fe1ba10`); smoke daemons divide their roots by the co-located daemon count — up to 12 at once now (was ≤ 5): budgets shrink, no row asserts a budget-derived size, every row green at 4 parts |
| `/build/tmp` sweeps | none in any rig (only `scripts/venue trim`, by hand) | — |
| host-wide `sync` | the frozen-daemon row only (the rig sync rail forbids the rest) | `[venue-exclusive]` |
| SIGSTOP | the frozen-daemon row; nvmet `sigstop-preempt` | the row exclusive; the nvmet leg runs no host-wide operation while its daemon is stopped (`sync -f` only) |
| physically contiguous memory | every daemon's kmbuf ring (order-8 pages); the gate now runs up to 12 daemons at once | one `-o ro` reader's mount was refused `kmbuf ring registration ENOMEM` once, beside a foreign 9.9 GiB compile (a gate on an earlier tree); never without foreign memory pressure |
| `drop_caches` | suites' mount retry after a kmbuf ENOMEM | a fragmentation retry path, not taken in any run here |
| process-global test state | histograms, tripwire log budgets, signal-free syscalls | §2.3 |

## 5. Results

Every gate below ran the per-commit recipe (the clock touch, every test rerun) through the
measuring wrapper; "foreign" counts the 2 s samples in which another agent's venue command ran.

### 5.1 The Linux gate, final tree (`f827c9d`; rebased to `cf1bbe1` over dev's doc and script commits — the same code)

| run | wall | result | foreign samples | note |
|---|---:|---|---:|---|
| G1 | **6 m 11 s** (371 s) | 49/49 steps · 2,827/2,899 · 72 skipped | 20/184 | |
| G2 | **6 m 09 s** (369 s) | 49/49 · 2,827/2,899 · 72 skipped | 35/183 | |
| G3 | **5 m 59 s** (359 s) | 49/49 · 2,827/2,899 · 72 skipped | 161/178 | a foreign compile beside it (13.4 GiB, MemAvailable 1.9 GiB) |
| F1 | 5 m 36 s (336 s) | green | 0/167 | the same tree, earlier |
| F2 | 5 m 31 s (331 s) | green | 36/165 | |
| F3 | 5 m 56 s (356 s) | `THE KILL ROWS (b)`: A's teardown past `umount`'s 32 s | 70/177 | a foreign compile (12.8 GiB) left 2.6 GiB available; the row passes 5/5 alone on the tree |
| cold | **7 m 29 s** (449 s) | 49/49 · 2,827/2,899 · 72 skipped | 173/224 | empty `--cache-dir` and `ZIG_GLOBAL_CACHE_DIR` |
| cold (earlier) | 7 m 05 s (425 s) | nvmet `roundtrip-loop`: the loop listener's port id outside the harness slice | 181/211 | pristine `dev` failed the same leg at 02:52 (below) — a loop port another run held |

G1–G3 are three consecutive clean runs (each started only once the venue had no other build
runner and a load under 3).

**Before**, the same recipe and wrapper on pristine `dev` (`f246494`, the base of this branch):
**19 m 16 s** (1,156 s; 46/46 steps, 2,819/2,891, 72 skipped; unit 10 m in one process beside
smoke 11 m in one process, then bench → shim → nvmet → suites; 1.5 cores busy on average). An
earlier attempt ran 16 m 46 s to the nvmet `roundtrip-loop` failure below. The owner's R1 → R2 → R3
finished 19 m 04 s and 19 m 03 s apart (`/build/tmp/ho-gateR*.log`). **After: 6 m 11 s, 6 m 09 s,
5 m 59 s — 3.2× — and 7 m 29 s from cold caches.**

Where the 6 minutes go (G1): compile the binary 0–3 m (the unit binary's compile ends at 1 m;
the unit parts run 1–2.5 m beside the binary's compile, 1 m 20 s each against a 1 m 20 s plan); the
smoke parts, bench (8 s), shim (13 s), nvmet (1 m 39 s) and suites (2 m 19 s) from 3 m, the smoke
parts 1 m 58 s – 2 m 19 s against a 1 m 59 s plan; the exclusive frozen-daemon row last (15–31 s).
CPU: 3.3 cores busy on average (36 %; it was 1.1), peaking at 6; memory ≥ 7.3 GiB available
without foreign load; up to 12 daemons at once.

### 5.2 Totals: the same suite

Per binary, pristine `dev` → this tree: unit 2,576 pass / 70 skip / 2,646 → 2,577 / 70 / 2,647
(+1: the guard's red repro, now green); smoke 88 / 2 / 90; bench-smoke 41; shim-smoke 40;
shim-census 3; nvmet-smoke 53; rails 18 → 25 (+7: the shard plan's tests). **72 skipped on both.**
No test lost an assertion (§2.3's fixes widen what a test waits for or which regime it asserts).

### 5.3 The partition does not change the results

| run | unit parts | smoke parts | wall | unit | smoke | bench / shim / nvmet |
|---|---:|---:|---:|---|---|---|
| G1 (derived) | 4 | 4 | 6 m 11 s | 2,577 / 70 / 2,647 | 88 / 2 / 90 | 41 / 40 / 53 |
| V1 | 2 | 3 | 7 m 23 s | 2,577 / 70 / 2,647 | 88 / 2 / 90 | 41 / 40 / 53 |
| V2 | 7 | 5 | 7 m 29 s | 2,577 / 70 / 2,647 | 88 / 2 / 90 | 41 / 40 / 53 |

Both off-derivation counts are slower — fewer parts leave the long chains long, more parts stretch
one another — which is what the derivation says. The unit binary alone also ran at 2, 3, 5, 6 and
8 parts on the venue and 2, 3, 5, 7, 9 on the dev box: identical totals, the one failure being §2.3's
8-part `fiber parks per window` (under the derived 4: never). The whole unit binary in ONE process
under 8 × `yes` fails one test, `exec.lane … 1 ms timer parks under a foreign spawn storm count 0
tick rescues` (a producer starved past the 2 s tick between its push and its wake reads as a
rescue); it passed in every gate.

### 5.4 The dev box

`zig build gate` on macOS: **7 m 15 s → 2 m 52 s and 2 m 44 s** on the final tree (2 m 49 s on the
tree before §2.3's last three fixes), 2,222 / 2,251, 29 skipped: 5 unit parts, the 132 s
`meta.smo.test.scale` on its own part bounds it.

### 5.5 What the screen shows now

`scripts/venue zig build gate` from a terminal draws the build runner's live tree (`-t`). From a
script or an agent (no TTY) the heartbeat arrives as the gate runs:

```
[squeezefs-smoke 3/4] 42/55, 30s; running "cli.smoke.test.smoke: the data plane WITH a cache path — dd 200 MiB sha round trip, cp 100 MiB…" for 9s
[squeezefs-unit 2/4] done 662/662 in 1m23s (the table's plan 1m20s); 16 skipped, 0 failed
[squeezefs-smoke exclusive] done 1/1 in 15s (the table's plan 15s); 0 skipped, 0 failed
```

## 6. Not done / owed

- The offline fsck's superlinear cost on large (and striped) directories (§2.2).
- Seams for the remaining real-timer waits (§2.2): the supervisor probe interval, the 2 s tick in
  the `sync.*` tick tests.
- `meta.backend … fiber parks per window ≤ 2` is scheduling-sensitive past the derived shard count (§2.3).
- The compile of the binary (2–3 min, one LLVM thread under thin LTO) now heads the critical path.
- Seen only beside another agent's heavy run, attributed to it, not fixed here: nvmet
  `roundtrip-loop` asserts its loop listener's port id is in the harness slice, but `share
  --transport loop` reuses a loop port another run holds (pristine `dev` failed the same leg);
  `THE KILL ROWS (b)`'s manager teardown outlasted `umount`'s 32 s with 2.6 GiB available; the
  kmbuf ENOMEM of §4. And `exec.lane … 1 ms timer parks … count 0 tick rescues` under a saturated
  host (8 × `yes`, §5.3).
