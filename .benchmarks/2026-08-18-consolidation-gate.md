# 2026-08-18 — the consolidation `task check`: the deferral lifted, GREEN from zero

**Context.** The user ruling of 2026-08-12 ("don't run the gates until we
get performance up") deferred the authoritative `task check` through the
perf campaigns and the entire S8→S11 full-multi-writer program — ~60
ff-merges landed under the per-rung tiered gates (touched suites serial +
clippy both configs + fmt + shellcheck + markdown) but never the
end-to-end pass. On the program's close (`3e38af43`) the user lifted the
deferral ("go ahead"). This note records the consolidation: six runs,
five catches, every catch convicted with controls and fixed on its own
ff-merged branch, and the final run **GREEN FROM ZERO** on `5f40a1c1`'s
parent tip (`GATE-EXIT=0`, `/tmp/gate6.log` preserved this session).

## The verdict (run 6, from zero, every leg)

| Leg | Result |
|---|---|
| `cargo clippy --all-targets --all-features -- -D warnings` | clean |
| `cargo clippy --all-targets -- -D warnings` (shipped config) | clean |
| `cargo fmt --check` | clean |
| `cargo test --all-features -- --test-threads=1` (full serial battery, live-mount suites executing for real on this box) | **0 failures** |
| `cargo doc --no-deps` | clean |
| `cargo bench --benches -- --test` (smoke, root workspace) | clean |
| `task check:fuse3` (fork clippy/fmt/suite/bench) | clean |
| `task check:docs` (markdown, 268 files) | 0 broken |
| `task audit` (both lockfiles, `--deny unsound --deny yanked`) | pass — residue = the two rc-manifest §5-adjudicated `unmaintained` warnings (`bincode 1.3.3`, `number_prefix 0.4.0`), nothing new |

## The catches (counted-restart honored: each fix restarted the gate from zero)

| Run | Catch | Class | Conviction | Fix (ff-merged) |
|---|---|---|---|---|
| 1 | `fsync_durability_contract_tests::test_data_barrier_precedes_the_metadata_barrier` red | Harness race | The 50 ms checkpoint tick lands in the SAME `meta_device_syncs` funnel the assertion reads; bare 0/3 vs timer-parked 5/5, and the fsync path provably early-returns before any meta barrier on a failed data barrier. **Product ordering law intact.** Retires the carried residual-board item 12 | `07456d21` — park-the-timer pin (the registry's own idiom); teeth sharpened |
| 2 | `job_wire_tests::reassigned_shard_gets_fresh_destinations_and_expired_ones_quarantined` red | Harness race | Coordinator `shutdown()` hard-kills sockets by design; worker 2's LOCAL ledger legally raced an in-flight accept ack under battery load (every durable law had already passed; 10/10 green isolated) | `9e0346ec` — `WorkerOptions.acks_received` test observable (the `hold_submission` precedent's shape), polled before shutdown |
| 3 | `every_tracked_shell_script_is_executable` red | Convention rail | The rung-16 tax rig committed `100644` (`core.fileMode=false` hid the chmod) | `5380d45f` — `update-index --chmod=+x` |
| 4 | `mw_fabric_identity_tests::cli_mount_pair_or_neither_refusal_is_instant` red | Wall-clock guess | The fixed `< 5 s` "instant" bound blew at 6.7 s under battery load; the refusal itself was correct and loud | `5c102a04` — all three bounds in the suite become `max(quiet_floor, K × yardstick)` where the yardstick is the same binary's `--version` spawn under the same load (the bench-baseline same-box-relative discipline applied to test bounds) |
| 5 | `posix_mount_semantics_tests::mount_exports_holes_to_lseek_and_st_blocks` red | Settle-free read | `st_blocks` legitimately counts the durable blocks PLUS the staged copy until its asynchronous retirement; under load the retire lagged past the stat (10/10 isolated; every hole-geometry assertion held) | this note's sibling commit — bounded poll to the EXACT steady state (terminal value keeps full teeth) |

## The lesson (standing)

The full serial battery is the first venue where every timing-sensitive
live-mount assertion runs under sustained load. **Every product law
held** — the catches were exclusively wall-clock guesses, settle-free
reads of asynchronously-converging gauges, harness/shutdown races, and
one convention rail. The fixes are the corresponding house patterns
(park-the-timer, completion observable, same-box-relative yardstick,
poll-to-exact-steady-state) — never widened guesses, never weakened
assertions. Environment note for future gate sessions: reading a gate's
verdict through `| tail` masks the exit status (bit twice); capture
`GATE-EXIT=$?` explicitly. Rig hygiene: a worktree `target/` filled the
59 G tmpfs mid-session (42 GiB) — external `CARGO_TARGET_DIR` for
worktree verification builds.

## Standing debts after this note

The release battery (pjd → LTP → full fstests `-g auto`) remains OWED
before any release tag (unchanged posture), and the quiet-box sustained
rows for the perf-era headline claims remain owed (todo 21).
