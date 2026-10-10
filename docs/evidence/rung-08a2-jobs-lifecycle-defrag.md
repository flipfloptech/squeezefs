# Rung 8 wave A2 — the job fabric, the data-volume lifecycle, defrag D1–D4

Spec: `docs/porting/ops_jobs_lifecycle.md`; decision 0003 §G; 0002 rows 408–418.
Modules: `src/ops/jobs/*` (+ `src/ops/jobs.zig`), `src/ops/lifecycle/*` (+ root),
`src/ops/defrag/*` (+ root), `src/cli/{job,defrag}.zig`, the `volume add-data /
remove-data / undrain` arms of `src/cli/volume.zig`.

## What landed (portable, red-first, gate-green on macOS; unit + rails on the venue)

| Piece | Where | Pinned by |
|---|---|---|
| `Tag.job = 13` / `Tag.job_shard = 14`; `JobRecordV1` 96 B, `JobShardV1` 56 B + ≤ 64 tuples; total decode; fuzz | `control_records.zig` (anchored hunk), `jobs/records.zig` | byte offsets, every refusal class, "never panic on arbitrary bytes; decodes re-encode to themselves" |
| The throttle law `t × (100 − pct)/pct` in 50 ms slices, live re-priced; `MOVER_PIPELINE_WIDTH = 4` | `jobs/throttle.zig` | the tie test at 25/50/75 (`3t, t, t/3`), the slice-bounded retune, the ±10 % duty gate over a noop run (`fabric.zig`) |
| Capacity preflight (every term derived) + the write-rate estimator | `jobs/preflight.zig` over `set_config`'s constants | the property "refuse iff `avail < needed + transient + headroom`" over fuzzed censuses |
| `plan_rebalance` in ppm, greedy legs, never below the mean | `jobs/rebalance.zig` | the 70/40/10 tie test |
| The fabric: submit (coordinator-class, durable Put first), adoption (Running/Queued → Queued, Paused stays), `claimNext` under the mover-scope law (queued LOUD once), the CAS claim word + RAII release (tripwire on a non-holder release), the terminal law (persist-then-flip, first terminal wins, regression tripwire), the park/resume Dekker with the `SQUEEZEFS_TEST_JOB_PARK_DELAY_MS` window, checkpoint cadence 256 tasks / 5 s, Memory Red shed → pause loudly, never self-resume | `jobs/fabric.zig` | 12 tests incl. crash-resume by adoption over the same store, an error-returning body releasing its claim |
| Shard leases: fencing from 0, renew/check/complete, sweeper `max(TTL/4, 25 ms)`, expiry → fencing bump → quarantine cohort under a fresh dead epoch → PR preempt (landed ⇒ release; else deferred-reclaim loud) → requeue; read shards take the fencing arm only; fresh destinations; the persisted fencing seeded at adoption | `jobs/lease.zig` | the lease/fencing matrix, the quarantined-destination tripwire, the remote closure |
| The `0x06xx` block on the ONE listener (`job_enroll, shard_assign, heartbeat, result_submit, read_shard_result, shard_abandon`), `Class.bulk` 16 MiB, class-aware receive cap, the screens (fencing before any device read, `n ≤ 64`, every tuple in the shard's window), 100 % verify-reads on plaintext (permille only under mTLS), the worker loop with the half-TTL heartbeat and `REVALIDATE_BATCH = 16` | `wire/{verbs,frame,session,service,loopback}.zig` (anchored hunks), `jobs/{frames,wire}.zig` | loopback end-to-end: enroll → pull → execute → propose → the job completes durably; lease expiry + the refused late submit + re-lease at fencing 1; the window/verify refusals |
| Models with weakenings: the claim word (drop the scope check), the park/resume Dekker (flip after the re-check), the fencing pair (compare outside the lock) | `jobs/models.zig` | `expectHolds` / `expectFails` |
| The published seams for fsck: `ShardLease`, `Throttle`, `AdminJob`, `FleetDispatch` | `jobs/seams.zig` | the fabric's own bindings |
| The lattice with the seven-step durability order over a RAM world; a crash after EVERY step ⇒ the remount converges; `vol-{16 hex}` never reused; undrain flips Active FIRST | `lifecycle/lattice.zig` | the crash matrix, the retire crash loop |
| The census (a shared block ONE task, refs `(ino, block_index)`, the decoration verbatim) | `lifecycle/census.zig` | |
| THE ONE MOVER `moveOne` in nine steps over a seam; the kill matrix at every step ⇒ never a lost/duplicated binding, bytes intact, the allocator reconciled to the bindings' population, the re-plan converges | `lifecycle/mover.zig` | 6 tests incl. the shared-block move-once, the supersession no-op, the verify-read mismatch |
| The drain + rebalance bodies (re-plan per pass, the capacity re-verify → `PausedCapacity`, retire on convergence, the undrain self-cancel); a crash mid-drain → a fresh fabric adopts and converges | `lifecycle/drain.zig` | 4 tests |
| D1–D4 definitions + the pack face + the ratio codec; `plan_defrag` (D2 then D1); `plan_pack_compaction`; the compactor with the raise/publish/release law; the bounded report | `defrag/{metrics,plan,pack_compact,report,bodies}.zig` | 13 tests incl. `--pack` on half-empty packs ⇒ fewer blocks, byte-exact; the lever-off refusal; `defrag --data` convergence |
| `plane.republishBinding` (the layout primitive under the (3.7) lock: expected-binding compare, ONE tx with `+dst −src`), `plane.moverQuiescent`, `plane.forEachBinding`; `KvBackend/MetaSet.stampVolumeLifecycle` | `data/plane.zig`, `meta/{backend,meta_set}.zig` (anchored hunks) | compiled on the venue; exercised through the offline verbs' walk |
| Linux binding: `TreeStore` (tree 0 of volume 0), `Options.fromEnv` (every knob row read), `DataWero` (rtype 3 over `pr_linux`), `AllocSink` (the allocator quarantine + dead epochs), `PlaneMover`, `PlaneWalk`, `PlaneWorld`, `PlaneDrain`, `Node` (fabric + table + wire side + the `job_copy_buffers` component + worker fibers) | `jobs/coordinator.zig` | `Options.fromEnv` test on the venue; the rest compiles and is NOT yet wired into the mount (below) |
| Verbs: `volume add-data` (bit → device → ONE tx with the queued rebalance → refused by identity on a re-add), `remove-data` (preflight over the open plane, Draining + queued evacuation), `undrain` (Active FIRST, cancels durably, refused after), `job list/status` (the durable range probe), `job rotate-enroll`, `defrag --report-only` (offline measure + bounded JSON), the mutating defrag arms as queued records | `cli/{volume,job,defrag}.zig`, `cli/args.zig` | run by hand on the venue (format 1 meta + 2 data → add-data d2 → list → report → remove-data → undrain → defrag queue → rotate-enroll refused) |

Measured on the venue by hand (nvmet-loop class, debug build): the offline add-data of a 2 GiB file
device: superblock write + read-back, the bit stamp, one tx — under 200 ms end to end.

## Not delivered in this wave (stated, not hidden)

- The daemon does NOT yet start `coordinator.Node` at mount: adopted jobs stay Queued on disk until
  a mount starts the fabric. The Linux binding is complete and compiled; the mount integration
  (manager predicate → `setManager`, the allocation-lease arm → `openAdmission`, the budget
  component registration, the data WERO's device list) is the next hunk in `cli/mount.zig`.
- No admin-lane dispatcher (`ipc/host/admin.zig`): `job pause/resume/cancel/throttle`, `job worker`
  and every mountpoint target are REFUSED by name. `job list/status` on a live mount is refused
  too (the read-only set open still takes the writer door).
- `enable-symmetric`, the fleet read-shard runner on the worker (fsck's engine), the mover's TCP
  leg (`job worker` over `wire/dialer.zig`), mTLS — not written.
- `republishBinding` refuses a block-map-tree (kvmap) head as `Superseded`: a kvmap file's blocks
  defer forever under a drain (the volume never retires with such a file; the window merge's own
  transaction shape is the sweep's). Striped inline maps and packed tenants republish.
- The Linux e2e kill matrix (kill -9 mid-drain → remount → the adopted job completes), the
  `df` baseline and the remote worker over TCP are owed with the mount integration; the RAM-world
  kill matrices (lattice, mover, drain) pin the laws meanwhile.
- The gate on the SHARED working tree (three waves landing concurrently) showed one `cli.args`
  usage-census failure from another wave's `fsck_merge` tag and a SIGKILLed IPC park-ladder test
  under build contention; this wave's staged tree passes unit + rails on both venues.
