# 2026-08-17 — The tarx C10 "cross-volume unlink window": a detector-plane mirage, convicted and closed

**Branch** `fix/mw-xv-unlink-c10` (worktree off dev `55d88138`).
**The finding**: `.benchmarks/2026-08-17-mw-shipped-free-c8-fix.md` §FOUND +
residual #4 — the co-writer tarx leg's rm sweep red on **C10**
(`nlink 0 while a dentry still names the ino`, whole-directory
dangling-dentry sweeps, sizes 1–37, ≥1-in-4 leg runs, fired by the
UNMODIFIED dev binary too), with `dir_nlink_underflows` /
`fsck_dangling_dentries` / `fsck_nlink_zero_named` the moving tripwires.
This note is that handoff's fix rung.

---

## The conviction (diagnosed against the live repro FIRST)

**The write path is innocent.** An NLINKTRACE-instrumented binary traced
every nlink mutation site (parent updates, xv SetNlink, unlink_local,
destroys) through red and green leg runs:

* `crossvol_tx_started == crossvol_tx_completed` (247/247 per pass),
  `crossvol_tx_steps_foreign == 0`, `crossvol_tx_midplan_escalations == 0`
  on every captured run — no cross-volume plan ever half-committed.
* The red run (leg2, 25 findings) shows every victim FULLY removed at
  10:48:11 — the count step, the name step AND the record destroy all
  committed and traced (`xv_setnlink pre=1 post=0` → `destroy nlink=0`;
  `unlink_local … is_dir=false nlink=1` → `destroy`) — hours of state
  before the red fsck.
* Same-volume unlink is ONE whole-tx entry; the cross-volume plan
  commits the NAME first by design (a skew could only land in the safe
  leak direction). No ordering fix exists to write, because no ordering
  bug exists.

**The findings were manufactured by the DETECTOR's fleet plane.** Online
fsck on a co-writer fleet is the KD-MW-16 fan-out: census shards are
dispatched to fleet members, and the leg2 red run's own job log shows the
dispatch (`census partitioned 2 ways — 1 worker shard(s)`) with the 25
findings arriving VERIFIED from the pass. The member (the co-writer)
reads through its S5 staleness-bounded reader view — **per-volume
checkpoint projections at different instants**. The rm storm is what
spreads those instants (mass deletes churning both volumes' checkpoint
cadence), so the member's shard sees volume A's dentry tree BEFORE the
sweep beside volume B's inode tree AFTER it:

* a dentry whose delete is past A's instant + a record whose
  `Put(nlink=0)` (the unlink's own commit) is inside B's =
  **C10ZeroNlinkNamed**, verbatim;
* a dentry whose delete is past A's instant + a record whose destroy is
  inside B's = **C10DanglingDentry**, whole leaves at a time (fs/9p's 16
  names in one cluster, the earlier capture's fs/afs 36 — dentry leaves
  are key-clustered per directory).

**Why the member's own zero-FP ladder cannot clear it**: the settle
window, the `(nlink, ctime)` witness bracket, the fresh dentry pass and
the 4a "lease" re-check all re-read the SAME time-shifted projection —
verification inside the bubble confirms the bubble. The open-intent
exemption cannot fire because no intent exists (nothing is in flight;
the skew is two healthy instants).

**The clincher pair**: (1) the red fleet re-fscked CLEAN ~19 minutes
later with zero repair — the reader's poll caught up (`fsck #1 red →
fsck later clean`, then a remount bisect confirmed the durable state was
always clean); (2) my single-arm rig (no leg, same storm) never
produced a C10 in 10 runs, while the leg (member shard dispatched at its
end-of-leg fsck) fired at the historical rate. Stochastic 0–37 by
whether the poll straddled the storm; "pre-existing" because KD-MW-16
pre-dates this branch; intents-arm-blind because the mirage needs only
the member dispatch.

**The third tripwire is a separate, also-convicted bug**: on EVERY tarx
run `dir_nlink_underflows` fired exactly once, on ino 1 — and the trace
showed `mkdir(dest)` reading **root nlink = 1 on a fresh volume**. The
image builder minted every directory (root included) at `nlink: 1` with
no parent bumps — one below the live `2 + subdirectories` law that the
parent-decrement floor (`pv.nlink > 2`) assumes — so the first full-tree
`rm -rf` sweep hit the floor on root's decrement, forever, the deficit
self-suppressing at 2 (which is why nothing else ever noticed).

## The fix shape (adjudicated per the mission's ladder)

The window IS "fsck reading between two legitimately-ordered commits" —
so the fix is detector-side, in the verifier's zero-FP ladder, at the
plane that violated the ladder's one-view assumption. The S3.5
cross-volume transaction plane is NOT needed (nothing in the write path
moved). The loss-direction teeth are never weakened — they are pinned.

1. **The inode plane is a ONE-VIEW plane** (`FsckOptions::inode_plane`,
   internal — no knob): C9/C10 verdicts are census-vs-dentry-pass
   AGREEMENT, meaningful only when both walks and every verification
   read share one authority's coherent instant. Fleet shards — member
   AND local — skip the classes (their refs still feed the block-plane
   census merge); `fleet_worker` sets the flag at the wire seam.
2. **`run_fleet`'s finalize judges the plane WHOLE on the coordinator**:
   one unsharded dentry pass + one census over the coordinator's
   RAM-authoritative view, then the UNCHANGED evaluate + `recheck_suspects`
   ladder (settle → witness bracket → fresh pass → 4a lease re-check →
   intent exemption) — shared with the allocator classes' finalize. Cost:
   one coordinator census + dentry pass per fleet pass — the stated
   Amdahl term of the fan-out (design-mw-fleet-jobs §4), paid so the
   teeth stay exact.
3. **`strip_inode_plane_proposals`**: a member proposing C9/C10 findings
   is an older/foreign binary's time-shifted verdict — dropped LOUDLY at
   merge with its inode-plane counters zeroed, so a mirage can never
   move the coordinator's `fsck_nlink_zero_named` /
   `fsck_dangling_dentries` stop-and-read tripwires.
   `fold_finalize_counters` now folds the finalize's inode-plane
   counters (they exist only there — exact, never a double count).
4. **The builder mints directories on the live law**: root and `add_dir`
   directories carry `nlink: 2` and bump their parent — a built image
   agrees with what live mkdirs would have left. New formats only;
   existing volumes keep their self-suppressed deficit (forward-only).

Non-fleet fsck (solo online, offline `--shards` operator probes) is
byte-identical to before: `inode_plane` defaults true, `run_fleet` with
zero capacity is still `run()` verbatim.

## Red-first pins

* `tests/mw_fleet_jobs_tests.rs::a_time_shifted_members_inode_plane_verdicts_never_merge`
  — a member proposing manufactured C9/C10 findings on a healthy tree:
  merged report stays `findings: 0`, tripwires stay flat. **RED at the
  pre-fix tree** (the forged findings merged verbatim), green with the fix.
* `tests/mw_fleet_jobs_tests.rs::the_inode_plane_loss_direction_teeth_survive_the_fleet_plane`
  — REAL planted damage (nlink-0-with-name + dangling name) is still
  found with the fleet plane armed and a member enrolled: the teeth pin.
* `tests/kv_backend_tests.rs::a_production_format_roots_nlink_is_two` —
  fresh `format_v3` root nlink == 2; a routed-layer mkdir/rmdir cycle
  returns it to 2 with `dir_nlink_underflows` flat. **RED pre-fix**
  (root read 1; the cycle underflowed).
* `tests/kv_backend_tests.rs::format_time_directories_carry_the_two_plus_subdirs_law`
  — built images carry the live law (root = 2 + subdirs, leaf dirs = 2).
  **RED pre-fix.**
* `tests/fsck_c10_tests.rs` (16) — the loss-direction detector suite,
  untouched and green: single-view detection, evidence, repair postures
  all intact.

## Counted live acceptance (from zero, fixed binary `5077323c`)

Venue: `mw_fleet.sh create 1 --cowriters=1` (SQZ_MWFLEET_MW_PORT=54193,
SQZ_MWFLEET_OSS_GB=24), `SQZ_MWMATRIX_TAR_SRC=/home/justin/Source/linux/fs`
(the measured row's real tree), the FULL `s10-intents-tarx` leg (4
A-B-B-A intents arms, netns co-writer at 250 µs RTT, end-of-leg fsck
oracle + drift + owner_panics checks). Every row is teardown → create →
leg, from zero. (A first count attempt aborted at its second run on the
leg's own quiet-box gate — `loadavg 5 > 4` residue from the release
build; the leg never executed, the abort is environmental, and per the
counted-restart discipline the count below restarted from zero on a
quiet box.)

| Run (from zero) | Leg exit | C10 | C8/drift | dir_nlink_underflows | dangling / zero_named |
|---|---|---|---|---|---|
| tarx 1 | **GREEN** (`PUBLISHED`) | 0 | 0 / 0 | 0 | 0 / 0 |
| tarx 2 | **GREEN** (`PUBLISHED`) | 0 | 0 / 0 | 0 | 0 / 0 |
| tarx 3 | **GREEN** (`PUBLISHED`) | 0 | 0 / 0 | 0 | 0 / 0 |
| tarx 4 | **GREEN** (`PUBLISHED`) | 0 | 0 / 0 | 0 | 0 / 0 |
| tarx 5 | **GREEN** (`PUBLISHED`) | 0 | 0 / 0 | 0 | 0 / 0 |

**5/5 green from zero** against the pre-fix ≥1-in-4 red rate (the same
venue red on run 2 of 8 with the pre-fix binary during diagnosis, 25
findings), with every tripwire flat on every run — `dir_nlink_underflows`
0 where the pre-fix binary fired it on effectively every full sweep.

Plus the no-regression legs on the same binary, same fleet shape:
`s10-intents` **GREEN**, `s9-fanout` **GREEN**.

## Gates

* Touched suites serial, all green: crossvol_tx (14),
  dynamic_meta_routing (16), fsck_c10 (16), fsck_c9 (10), fsck_repair
  (16), fsck (20), kv_backend (36), kv_scale (15), meta_ship (15),
  mw_delegation (14), mw_fleet_jobs (9), mw_intent_batch (20),
  posix_p2_semantics (7), posix_semantics (13), rename_semantics (6).
* `cargo clippy --all-targets --all-features -- -D warnings` AND the
  shipped config — clean. `cargo fmt --check` — clean. Markdown check —
  clean. No harness scripts touched (no shellcheck), no lock-free core
  touched (no loom). Full `task check` DEFERRED per the standing ruling
  for this ladder.
* Zero-residue teardown after every fleet cycle; the diagnostic
  NLINKTRACE instrumentation never entered a commit.

## Residuals (stated, not hidden)

1. **Field volumes formatted pre-fix keep root (and any builder-built
   directory) one below the 2+subdirs law** — self-suppressed at the
   parent-decrement floor, visible only as one `dir_nlink_underflows`
   per full sweep of a pre-fix volume and a `find -noleaf`-class
   pessimization. Forward-only by the standing law; no mount-time heal.
2. **The fleet fsck's inode plane is now coordinator-serial** — one
   census + one dentry pass on the coordinator per fleet pass (the
   block-plane fan-out is unchanged). If fleet inode-plane sharding is
   ever wanted, it needs a coherent-instant protocol (all shards pinned
   to ONE checkpoint epoch per volume), which is S10c's business, not a
   patch.
3. **A genuinely damaged volume observed THROUGH a member still reports
   nothing from that member** — by design: the coordinator's finalize
   judges the same damage from the authoritative view, so coverage is
   preserved (the teeth pin proves it); what a member loses is only the
   ability to manufacture verdicts.
4. **The C10 evidence text still names the S3.5 cross-volume window**
   as one origin of the shape — correct as a class description (a real
   crashed pre-S3.5 unlink still leaves it); this campaign adds the
   knowledge that a LIVE, healing, fleet-observed instance of the shape
   is a detector-plane artifact, now structurally impossible to report.
