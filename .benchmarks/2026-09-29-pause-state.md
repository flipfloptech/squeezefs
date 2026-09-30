# Pause state — 2026-09-29 (machine move after the 1.3.0 release act)

Written across the day's last hours. The owner's first ruling — "pause
after this run even if it fails" — was superseded when attempt 14's only
red turned out to be the fuzz leg's BUILD after six green product legs:
"we only worry about fixing the fuzz leg and testing it again and
whatever is after it … if something fails from an actual code defect that
might break things then we pause." So the run finishes here — the fuzz
leg on the fix, then the release act up to the tag question — and the
move follows. `dev` = `origin/dev` is named in §"State at the move"
(filled at the end); every review-stage branch of the day landed ff-only
and was deleted, local and remote.

## Where the run stands (2026-09-29)

The 1.3.0 release gate has run FOURTEEN attempts since 2026-09-27; every
red or hang was attributed, fixed red-first, reviewed to LANDABLE and
landed before the next launch from zero (the record
`.benchmarks/2026-09-19-sym-acceptance.md` §4.4bp–§4.4ce; the run log
`.benchmarks/2026-09-12-sym-pr-run.md`, its last row growing per attempt).
Today's two:

- **§4.4cd — PRODUCT (attempt 12 HUNG 4.6 h in fstests generic/551)**: the
  rewrite program parked every displaced block of a random O_DIRECT
  overwrite in the file's open epoch until the volume read FULL, and the
  ENOSPC early-close (KD-1.7) existed at the write pipeline alone — the
  writeback flush unit retried `StorageFull` for ever. Landed `bc3764f1`
  (7 commits, 4 review rounds): the pressure close at the ONE allocation
  act (`allocate_placed_block`, non-parking 3.5, no hooked caller holds
  3.5 or 4a — `allocate_placed_block_under_guard` for the two that do, the
  census rail `tests/allocation_act_lock_law_tests.rs`), the armed
  allocator's post-drain re-ask (PR 8's inert-valve gap), the fsync
  ladder's own early close + retry, one epoch's per-file fence never
  failing the sweep; gauges `rewrite_shadow_pressure_closes` / `_busy`,
  `fsync_flush_enospc_retries`. Live proof: the scaled generic/551 repro
  on the round-2 binary — three fills, three closes, 209 / 209 writeback
  units at attempt 0, zero refusals. generic/551 at its real 24 GiB shape
  inside attempt 14's fstests leg is the acceptance.
- **§4.4ce — HARNESS (attempt 13 RED at `task check`, 29 min)**:
  `meta_entry_economy_tests`' destroy-batch law judged the process-global
  journal-entry counter exactly across a window two background actors
  write into (an SMO's `commit_entry` record; the 50 ms cadence drain
  commit's counter skew) — the TIMING-CONTRACT class of §4.4ca/cb/cc.
  Landed `b2cfb3e4` (3 commits, 2 review rounds): the law at the destroy
  tx's own commit SITE (the whole site map — one commit per batch and no
  other committer), the drain quiesced with its premise asserted, five
  full-fill batches on a 321st calibration corpse; red-first 320 vs 5.

## Attempt 14 outcome — six product legs GREEN; the fuzz leg red on a BUILD, fixed, re-run alone

`task check` GREEN (14:17, 395 suites, 53 min at the cap) → fstests
`-g auto` GREEN (16:32 — 791 ran / 788 clean / 3 expected-shape /
0 unexpected; **generic/551, the test that hung attempt 12 for 4.6 h,
PASSED in 76 s at its real 24 GiB shape — §4.4cd's acceptance is MET**) →
pjdfstests GREEN (16:35) → LTP GREEN (16:40) → require-mount GREEN
(17:09) → zc GREEN (17:11, the laptop's sqz kernel) → `fuzz END rc=1`
(17:20): `cargo +nightly fuzz build` failed with `E0275` on
`JoinedWire::with_client`'s recursive `impl Future` — every nightly
tried refuses it, stable 1.98 alone accepts it — a compile-shape defect
since PR 12b that no nightly had compiled since (record **§4.4cf**).
Fixed by erasing the one cold await on the re-dial's return edge to
`dyn Future` (`fix/joined-wire-redial-future-cycle`; red-first = the
nightly build; stable side: lints + `sym_n_daemon_tests` 108 green);
rails: `task check:fuzz-nightly` + the driver's pre-chain nightly build.
**Owner ruling**: the six green legs stand; the fuzz leg alone re-runs on
the fixed tip; the release act names both shas. Its result:
§"State at the move".

## State at the move

- `dev` = `origin/dev` = the release-act commit for **1.3.0** over
  `227ac373` (§4.4cf `585d7377` + fold, §4.4cg `1ce23c8e` + fold — every
  branch landed ff-only and deleted). The tag `stable-2026.09.5` is the
  OWNER's act on that commit (asked, not taken); `task dist:all` and the
  rocky8 pair to `squeeze-test` follow the tag.
- **The 1.3.0 release gate is GREEN as a composed chain**: six product
  legs on `b2cfb3e4` (attempt 14), the fuzz leg on `1ce23c8e` — 18 / 18
  targets, 0 artifacts, ≈ 5.5 × 10⁸ executions — after two fuzz-leg
  finds fixed on the way (§4.4cf the build, §4.4cg the oracles); record
  `.benchmarks/2026-09-29-1.3.0-release-gate.md`. No performance number
  in it.
- Owed on the box (the next machine drives them): the box brackets on the
  flip binary (gates 1 / 2 / 3 / 3b / 3c / 5 / 7); the cloud row under a
  NEW expressed approval.
- The board items above stand; two new ones from §4.4cf/§4.4cg's reviews:
  `check:fuzz-nightly` belongs on `nightly.yml` (a pure compile); the
  1.101 nightly deprecates `Atomic*::fetch_update` for `try_update` at
  ≥ 4 `routing.rs` sites — the next stable bump's clippy red; fsck C1's
  `record_schema_violation` has no kind-6/7 arms (pre-existing).
- Artefacts to carry: `~/tmp/sym-run-state/` (the plan JSON, the review
  trail through `grok-exec-review-4.4cg.md`, the driver with its new
  preflight, `attempts/attempt14-*` — the six legs' logs and both fuzz
  runs'), the memory notes; `~/tmp/g551-evidence/` may be deleted (its
  acceptance, generic/551, is green in the record).

## The machine

The Omarchy laptop (AMD Ryzen AI MAX+ PRO 395 "Strix Halo", 32 CPUs,
kernel `7.2.5-7-omarchy` carrying the sqz zc/kmbuf patches — the owner's
build — `fuse.enable_uring=Y` persisted) **hard-powered-off at 12:58:00
EDT** during attempt 13's aftermath (the §4.4ce review's parallel probes
beside a build): the journal simply stops, no shutdown sequence — a
firmware thermal trip. Installed the same day at the owner's request:
`sqz-thermal-governor.service` (root, enabled) — a stepped
`scaling_max_freq` cap over all 32 `amd-pstate-epp` policies driven by
k10temp Tctl (`/usr/local/sbin/sqz-thermal-governor`; overrides in
`/etc/default/sqz-thermal-governor`: ladder 76 / 82 / 88 / 94 °C →
4.0 / 3.2 / 2.4 / 1.6 GHz, 2 s period, step-down after 15 cool samples,
full speed restored on stop; `journalctl -u sqz-thermal-governor` is the
record). Verified under attempt 14's compile: load ≈ 100, Tctl held
73–78 °C at level 2 (84–89 °C spikes on the untuned ladder). The laptop
runs no benchmark rows (the venue law), so the cap costs nothing that
holds merit; it lengthens the gate's wall.

**`/tmp` is tmpfs here.** The reboot took `/tmp/grok-justin/*` (review
files, probe crates, the g551 repro script, commit drafts) and
`/tmp/release-1.3.0/*` (the driver's working copy, every attempt's
archived logs, `chain.txt`). What survived: the repo and its pushed
branches, the run-state dir (the plan mirror
`grok-exec-plan-dadee1dd.json`, the mirrored reviews, the driver's
canonical copy `release-1.3.0-driver.sh`), the release worktree
`~/Source/squeezefs-release-1.3.0`. **Owner ruling (2026-09-29): every
scratch and run-state file lives under `~/tmp/` — never `/tmp` (tmpfs)
and never the home directory's root.** Applied the same day: the run
state is `~/tmp/sym-run-state/` (moved from `~/sym-run-state/`, which
older record entries still cite), the chain's working dir is
`~/tmp/release-1.3.0/` (the driver's `REL`), agent scratch is
`~/tmp/grok-justin/`, and the g551 evidence is `~/tmp/g551-evidence/`;
attempt 14 alone finished under `/tmp/release-1.3.0` (it was launched
before the ruling), its logs copied into `~/tmp/sym-run-state/attempts/`.

## What the next machine needs (the release gate's preconditions)

- Toolchain as here: cargo/rustc 1.98.1 (stable), a nightly toolchain +
  `cargo-fuzz` 0.13 (the fuzz leg), `cargo-audit` 0.22 (`task audit`),
  `task` 3.53, `jq`, docker (the `task build:*`/`dist:*` distro builds;
  no podman — the Taskfile prefers docker).
- Root + FUSE: `/dev/fuse`, `fusermount3`, `fuse.enable_uring=Y`
  (`/etc/modprobe.d/squeezefs-fuse.conf` here) — every mount-class suite;
  the zc leg needs a kernel with the sqz zc/kmbuf patches
  (`docker/kernel-sqz/`; without it the zc leg self-skips and the release
  record says so, as 1.2.0's did).
- Limits the driver's preflight asserts: memlock hard unlimited
  (`/etc/security/limits.d/99-squeezefs-memlock.conf` here), nofile hard
  524288, RTTIME hard unlimited (the Omarchy launcher's 0/0 was §4.4bw —
  the harness lifts and refuses fail-closed; every tool shell that mounts
  runs `sudo -n prlimit --pid $$ --rttime=unlimited:unlimited
  --memlock=unlimited:unlimited; ulimit -l unlimited` first).
- Space: `/dev/shm` ≥ 50 GiB (the fstests scratch volumes are 18 + 24 GiB
  tmpfs files; the driver deletes them after every fstests leg — attempt
  8's lingered swapped and ENOMEM'd a live mount), `/home` with ≥ 400 GiB
  free (`target/` for the main checkout ≈ 230 GiB, the release worktree's
  ≈ 130 GiB; reclaim with `cargo clean` when short).
- The external suites at the durable prefix
  `/var/cache/squeezefs-suites/{xfstests-dev,ltp,ltp_install,pjdfstest}`
  (`tests/suite_tree.sh` builds them as root; `run_fstests.sh` /
  `run_ltp_syscalls.sh` / `run_pjdfstests.sh` find them there).
- The repo-local git identity in EVERY checkout and worktree before its
  first commit (`git -c user.email=109311040+flipfloptech@users.noreply.github.com
  -c user.name=flipfloptech`, or set it locally): the global identity is
  private and GitHub refuses it (GH007).
- The release chain: worktree `git worktree add --detach
  ~/Source/squeezefs-release-1.3.0 <sha>`; the driver at
  `~/tmp/sym-run-state/release-1.3.0-driver.sh` copied to
  `~/tmp/release-1.3.0/driver.sh` (its `REL` is `$HOME/tmp/release-1.3.0`);
  launch `cd ~/tmp/release-1.3.0 && rm -f chain.exit chain.txt;
  (nohup setsid bash ~/tmp/release-1.3.0/driver.sh <sha> > driver.out 2>&1 &)`,
  then verify the `task check` pid reads `Max realtime timeout unlimited`
  in `/proc/<pid>/limits`; a monitor on `chain.exit`; archive every
  attempt's `chain.txt` + the red leg's log under
  `~/tmp/sym-run-state/attempts/`.
- Carry from this machine (a tarball): `~/tmp/sym-run-state/` whole (the
  plan JSON, `grok-exec-review-4.4*.md`, the driver, the earlier resume
  note `RESUME-2026-09-24-omarchy.md`), and the agent memory topic
  `~/.grok/memory-v2/workspaces/squeezefs-a83e38f2/topics/laptop-venue-thermal-and-scratch.md`
  (the workspace hash is the checkout path's — same path, same hash).
  `~/tmp/g551-evidence/` (24 GiB real / 42 GiB apparent: the rescued attempt-12
  volume images, the daemon log, gdb stacks, `fsck.json`) is CITED by
  §4.4cd and can be deleted once attempt 14's generic/551 is green — its
  numbers are in the record.

## Next, in order (the next machine)

1. Whatever §"State at the move" names as open — then, if the release
   act did not complete here, the release act for **1.3.0**: bump 1.2.4 →
   1.3.0 across the root and `crates/{fuse3,squeezefs-ipc,
   squeezefs-preload,squeezefs-testkit}/Cargo.toml` + the three lockfiles
   + the doc lines (`AGENTS.md:940`, `README.md:96`, `QUICKSTART.md:22`,
   `docs/operations.md:79,88,90,92`) + the RELEASE_NOTES verification
   paragraph + the gate record `.benchmarks/2026-09-27-1.3.0-release-gate.md`
   — ONE `docs(release)` commit over the tested tree (the 1.2.4 pattern:
   product identical, the tag on it) → **ASK THE OWNER before the tag**
   (`stable-2026.09.N`; the last is `stable-2026.09.4`) → `task dist:all`
   → the rocky8 pair to `squeeze-test` (VPN) → the box brackets on the
   flip binary (gates 1 / 2 / 3 / 3b / 3c / 5 / 7 per the acceptance
   record's §9) → the cloud row ONLY under a NEW expressed approval.
2. The board (unchanged from the run log's last rows): §4.4cd's six stated
   residuals (a legacy indirect-blob file at the wall; a joiner's
   one-beat-late shipped frees; the write path's growth-arm `ENOSPC` on a
   `Busy` skip; contract 5's exact counts via a serialized write; the
   router-wide `pending` word's extra ask per pass; the armed-mount
   leave's two "could not withdraw writer … volume is shutting down"
   WARNs); §4.4ce's census — the exact-equality laws over
   `META_KV_JOURNAL_ENTRIES` in `tests/` (the drain-active class
   `extent_patch_tests:488`, `killpriv_v2_tests:388/:430`,
   `overlay_overwrite_tests:624`, `meta_entry_economy_tests:671`; 39
   SMO-only sites) and `meta_entry_economy_tests`' `--test-threads=1`
   premise; the standing TIMING-CONTRACTS-under-load item (§4.4ca/cb/cc/ce
   — a busy/thermal refusal in the gate, or a lint over `== N` laws on
   deferred-tail counters); item 24's Lows 29–31; F-R5 economy; the zc
   tail; F-R2; the quiescent reader's in-place join;
   `durable_block_refs_tests` serial-only.
3. Run hygiene left here: the `sqz-sym-run` inhibitor is gone with the
   reboot; `~/tmp` holds ≈ 3 GiB of older suites' `sqfs_*` scratch
   (regenerable); `dhat-heap.json` in the main checkout is gitignored
   residue; `~/tmp/g551-evidence/` as above.

## Standing hazards

- Every tool shell that mounts: the `prlimit` + `ulimit -l unlimited`
  line first; mount-class suites `--test-threads=1`.
- Never `git checkout -- <file>` on a tree with uncommitted work (back up
  with `cp` first); `pgrep -f` / `pkill -f` patterns must not match the
  tool shell (a pid file, or `ps -eo pid,args | awk`).
- Gates and `dist` builds ONLY in the pinned worktree; the laptop idle
  (no cargo) while a gate runs; measurement rows never on this machine
  (the venue law — squeeze-test A-B-B-A rows are the only numbers that
  hold merit).
- No AWS launch without the owner's EXPRESSED approval for that run.
