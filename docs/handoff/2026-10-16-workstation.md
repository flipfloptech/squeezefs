# Handoff — 2026-10-16: pause on the Mac, resume on the Linux workstation

**Checkpoint:** `dev` at the commit that adds this note (code = `286152b`, the clock-cut revert,
plus docs/scripts). Everything is pushed to `origin` (`git@github.com:flipfloptech/squeezefs-zig.git`).
No agent is running; no worktree but the gate train's; kamrui and the Mac VM are cleaned.

## Where things stand

**Gates at the checkpoint.** `286152b` passed the full fresh gate 4 times in a row on the VM
(aarch64, 51/51 steps, 3,000/3,085 tests) and 2 of 4 times on kamrui (x86_64, 55/55, 3,002/3,085);
kamrui's two reds are two different pre-existing intermittent defects in the multi-daemon rows
(open items 2 and 3 below). Earlier heads reached three consecutive greens on both venues many
times (`6183f59`, `5816c33`, `c11f3f9`, `b64c149`, `e36aa06`, `d7c4121`, `b4dd322`).

**Release gate (charter §11).** Per-commit gate: met repeatedly (above). Class C — pjdfstest,
full LTP, xfstests `-g auto`, 3 clean runs each — has NOT run: owner rule, it runs only in the
window right before a release tag. The owner then moved performance ahead of the tag.

**Performance** (kamrui, RAM-backed NVMe via `scripts/perf/ramnvme.sh`; `docs/evidence/rung-10-perf-baseline.md`
is the baseline; numbers are IOPS or MiB/s, % of the raw RAM device):

| row | baseline | now (`286152b`) | raw RAM | lever(s) |
|---|---|---|---|---|
| 4k randread QD32×4 | 32.6 k (5 %) | ≈ 251 k (40 %) | 630 k | layoutlookup |
| 4k randread QD1 | 8.3 k | ≈ 19.4 k (47 µs) | 63 k | layoutlookup |
| 4k randwrite QD32×4 | 11.6 k (2 %) | ≈ 79 k (13 %) | 614 k | layoutlookup, rangelock |
| 64k randread / randwrite | 1761 / 720 | ≈ 6300 / 3500 | 12845 / 9236 | layoutlookup, rangelock |
| 1M seq read ×4 | 6697 (83 %) | ≈ 7800 (97 %) | 8080 | layoutlookup |
| 1M seq write ×4 | 2359 (34 %) | ≈ 3250 (47 %) | 6889 | slabcopy, rangelock |

The clock-read cuts (`57d9090`, `fa3fe60`; +25–35 % on the 4k rows) are **reverted** in `286152b`
(open item 1); `docs/evidence/rung-10-clockreads.md` keeps their numbers. Evidence notes, in order:
`rung-10-perf-baseline`, `-layoutlookup`, `-slabcopy`, `-rangelock`, `-seqwrite`, `-clockreads`.

## Open items — the first work on the workstation

1. **The two-daemon teardown wedge** (VM only; `docs/gates/flakes.md` "THE TWO-DAEMON MATRIX").
   `umount A` gives up after 32 s while A's teardown hangs; 3 of ~9 VM gates with the clock cuts,
   0 of ~88 before; never on kamrui. `lanepark` found no stall in the clock arithmetic: the cuts
   most likely expose an older race. The capture (`12eeae1`) shows A past both data volumes'
   dismount with the main thread, sqz-ckpt, the journal, the meta lanes and the timer alive —
   a later teardown step waits, probably on the joined peer. **Next:** add USER stacks to the
   capture (a SIGQUIT dump in the daemon), name the step, fix it red-first, then re-land the clock
   cuts with `git revert 286152b`. Needs an aarch64 venue (the Mac VM) to reproduce.
2. **Interception writes acknowledged before the kernel's attrs invalidation** (flakes row "row 4b").
   A kernel-path `fstat`/`pread` right after ring writes sees the old size (shim smoke; IPC host
   smoke `e2e_linux.zig:542` on kamrui). Likely fix: deliver `FUSE_NOTIFY_INVAL_INODE` synchronously
   before completing a ring write that grows the file.
3. **A cross-owner insert rejected after a manager failover** (flakes row "A DEAD MANAGER HOLDING A
   STRIPED HOT DIRECTORY"). After A dies and remounts as manager, B fires `xv_cross_owner_steps_rejected`
   ("a served insert names a child with no record") dozens of times and its `.stats` goes unreadable.
   First seen on the checkpoint's kamrui gate.
4. **Owner rulings pending** (from `rung-10-seqwrite.md`; recommended: both, (a) first):
   (a) fiber-aware device waits — revise decision 0001 item 2 so a device write parks only its
   fiber (≈ +50 % on big writes; today ~4.5 device writes in flight, bounded by blocked lane threads);
   (b) zero-copy big writes — hold overlay-sized writes and write from the slot (charter §6.2 item 1;
   with (a) ≈ 2×).
5. **Smaller items:** THE JOBS MATRIX `job pause` failed once (matrixwedge's full smoke, unchased);
   the exec lane can starve its end-of-pass flush while a fiber yield-loops (no product fiber does);
   `format`'s refusal message does not name the lock holder; the post-release list in
   `docs/memory/progress-ledger.md`.

## Resuming on the workstation

1. `git clone git@github.com:flipfloptech/squeezefs-zig.git && cd squeezefs-zig && git checkout dev`,
   then `scripts/dev/install-hooks` (origin/dev follows every landing; pushes log to `.git/sqz-push.log`).
2. The Rust reference at `~/Source/squeezefs` (read-only evidence; also the source of kernel patches
   0001–0026 — or point `SQZ_REFERENCE` at a checkout).
3. `scripts/dev/provision-local-venue` — fix every `MISSING` it prints (`--apply` installs packages;
   it never touches a kernel, a filesystem or sudoers). Needs: Zig 0.17.0, passwordless sudo, a
   build root `SQZ_BUILD_ROOT` (default `/build`; ext4, sparse files, ≥ 200 GB; `/build` must exist,
   as a link if the root lives elsewhere — the product's test paths use it).
4. Kernel: `scripts/kernel/build-host-kernel --local`, install, reboot into `7.2.6-sqz` (patches
   0001–0029); `scripts/venue status` must show `fuse.enable_uring` and the nvmet modules.
5. First gate: `scripts/venue 'timeout 3900 zig build --cache-dir "$SQZ_BUILD_ROOT"/zig-cache-v2 -Dfresh=true gate --summary all'`
   (`SQZ_VENUE=local` is the default on Linux); then `scripts/gate-train` (defaults to `local`).
6. aarch64 coverage needs a second venue: the Mac VM (`sqz-zig`, kernel 0001–0029 set as its
   default) or an arm64 box over ssh (`SQZ_VENUE=<host>` like kamrui). kamrui (x86_64, Ryzen 5 5500U,
   HPET clock — its absolute numbers read slow) stays usable over ssh.
7. Memory: `docs/memory/` is the agent's notes (AGENTS.md points there); importing them into Grok's
   workspace memory is optional (`docs/memory/README.md`).
