# Directories spread by depth: both MDTs under one subtree (2026-10-27)

**What changed.** Charter §5.4's mint spread, amended under the owner's ruling (decision 0003 §BZ): a new
DIRECTORY at depth ≤ `D` (the root's children are depth 1) is homed by the member pick over the healthy band
at its ordinal under its parent (the parent's member + the directories placed under it: the first child
beside its parent, the siblings split evenly — §BZ.1); a directory deeper than `D`, and every file,
follows its parent's member as §AA.1 says. `D` is
the set's: `squeezefs format --dir-spread-depth N` (default 3, range 0 … 254; 0 = §AA.1 alone), stored in the
`set_config` record (byte 39), changed by `squeezefs config set-dir-spread-depth N sqmeta://…` (offline; in
force at the next mount of each member; existing directories never move), read by every mount of the set and
shown by `config show`, `status`, `format`, the mount summary and `.stats` (`routing_dir_spread_depth`). A
directory records its depth on its inode (`flags2` bits 0..7: the parent's + 1, saturating at 255; a
pre-field directory has none and its descendants record 255 and stay on its member); a rename keeps it.
Counters: `routing_dir_place_{rotor,remote,local}` and, per member, `mint_files` / `mint_dirs`.

**Why.** §AA.1 (2026-10-13) pinned every child — files AND directories — to the directory's member, so a set
spread only by top-level subtree: any workload under one tree ran on one metadata volume while the second
MDT, with its own journal lane, conveyor and leaf locks, sat idle. §AA.1's reason stands (half of every
directory's children were intents before it). The depth rule needs no per-mkdir load guess, which both
traps rule out: mdtest's rank directories are made at setup on an idle system, and a rule that spreads
mkdirs "when hot" would make every one of the 320 k timed mkdirs an intent. The Lustre analogue the owner
runs: `lfs setdirstripe -D -i -1 -c 1 --max-inherit-rr=N` on `/` (Lustre's root default and compiled-in
`LMV_INHERIT_RR_ROOT` are 3; the owner sets 10) — our `D` is that `N`, with `--max-inherit` = `N` (local below
it; the health weighting lives inside the round-robin). The losers — load-aware and creator-affine placement,
cross-volume striping — are stated in §BZ.5.

**The red test.** `meta_set` "mdtest's shape on a two-member set": `md/#test-dir.0/mdtest_tree.N.0` — under
§AA.1 alone 100 % of the 32 rank directories sit on one member (the test's own `D = 0` arm shows it); with
the rule 16/16, every rank's files and depth-4 directories on ITS member, 0 intents over the timed phases,
every record carrying its depth. "The spread depth is the set's dial": 3 → 10 on a live tree spreads new
depth-4 … 10 directories and leaves old homes alone; a legacy subtree saturates. Ties: `dir_place` at
`D` = 0, 1, 3, 10, 254; the record's byte; the `set_config` field, its compat byte and verb; the arg grammar;
the rename law in `economy`.

**The counters** (the 2 MDT + 2 OST correctness rig, `mdtest -n 10000 -u`, phases dir-create / dir-remove /
file-create / file-remove, one `.stats` snapshot per phase; a correctness run, not a measurement; fsck
clean and `invariant_tripwires` 0 on every run):

| | volume 0 / 1 share of commits, per phase | intents per phase (minted = retired) |
|---|---|---|
| before, -np 32 | 100 / 0 %, 100 / 0, 100 / 0, 100 / 0 (vol 1: 0–1 commits) | 0, 0, 0, 0 |
| before, -np 64 | 100 / 0 % in every phase | 0, 0, 0, 0 |
| after, -np 32 | 50.0 / 50.0 % in every phase (dirs 160,017 / 160,017; files 160,000 / 160,000) | 17, 17, 16, 16 — the setup's `#test-dir.0` + 16 of 32 rank dirs and their teardown; 0 of the 320 k timed ops |
| after, -np 64 | 50.0 / 50.0 % in every phase (dirs 320,033 / 320,033; files 320,000 / 320,000) | 33, 33, 32, 32; 0 timed |

`routing_dir_place_rotor` 34 (`md`, `#test-dir.0`, 32 ranks), `_remote` 17, `_local` 320,017 at -np 32.
A single-process `tar -x` of linux `fs/` (2,452 entries, 99 directories, 76 at depth 3): before, 100 % on
volume 0 and 0 intents; after, 39 intents (the depth-≤-3 directories whose pick differed from the parent's;
78 rotor decisions), files 52 / 48 % over the members, `steps_shipped` 0. Rig: `~/tmp/dir-placement/`
(`mdrig.sh`, `cmdrig.sh`, `vshare.py`, `runs/`).

**Checks** (on the head rebased onto `f2b6029`, the maintenance-queue landing). Unit filters (`§BZ`,
two-member set, mint, set_config, format, config, intent, step, applier, kv_fs, economy, inode, member,
routing, rename, stripe, mount, offline, create, args, maintenance) green; `zig build rails` green; `zig fmt
--check` clean; the smoke rows (the two-member set, the mint windows, the striped directory on one daemon,
300 concurrent creates, two mounts of one set on one host, the two-daemon matrix, the joiner's rename,
RENAME_EXCHANGE, the shared hot directory, the dead manager's striped directory, the kill rows (a) (b), the
replay proof, the unlink storm) each green. Through the binary: `format --dir-spread-depth 0` places a
4-level tree whole on one member; `config set-dir-spread-depth` refuses a live mount, rewrites the record
offline (0 → 10 → 3; 255 refused by name), `config show` and the mount summary print it, `.stats` reads it
after the remount; a 12-level chain at `D = 10` spreads levels 1 … 10 (73 rotor decisions, 36 remote) and
keeps 11–12 local; a tree made under `D = 0` carries its depth bytes, so its new depth-4 children spread once
`D` is 10 while nothing of it moved; fsck clean. Two mounts of one set (A the manager, B joined over
loopback) both read `D = 4`; B's own tree spreads at depth ≤ 4 with ONE shipped step (its top directory
under A's root), A's mkdirs under B's directories are the foreign arm's intents as §5.5 says, both see one
tree, fsck clean, 0 tripwires. xfstests generic/001 013 025 078 245 323 426 604 through
`scripts/suites/fstests.sh`: all 8 passed on a quiet host, both sets fsck clean (the harness's "did NOT
pass" on a subset run is its stale-ledger check alone). Seen, not fixed: an unintended `-g auto` pass on
this binary (the subset variable did not cross `scripts/venue`'s sudo) passed its first 322 tests and
stopped at generic/323 — the daemon unresponsive 41 s under 100 AIO threads WHILE the -np 64 rig and the
smoke rows ran beside it; alone, 323 passed in 148 s as it does on `dev` (122 s on 2026-10-24's runs). A
load sensitivity of the supervisor's 41 s stat watchdog, not a placement defect.

**The follow-up (§BZ.9).** The owner's 1 MDT + 2 OST A-B-B-A read directory creation at -np 32 at
−2.3 / −4.6 / −3.0 % with every other row flat: the first build read the logical parent's record before
every mkdir for its depth. Now a set that cannot spread (`dirsMaySpread`: one healthy member or `D = 0`)
does nothing for the rule on the mkdir path — the child's byte comes off the guarded parent read every
create does; a set that can spread takes the parent's depth and ordinal from the directory memo (one
record read per parent per mount, `/` included). Counters `fuse_dir_depth_memo_hits` / `_reads`; the
`kv_fs` test holds the reads at 0 on a one-member set. Found on the way: the shared round-robin word
correlates with a tree's creation order (`r0, r0/s, r1, r1/s, …` put every `r` on one member and every
`s` on the other — the two-member smoke row's files 2000 / 0); the pick is now the per-parent ordinal
(`dir_place.pickOrdinal`; tie test with the interleaving 4/4 per level). Re-measured with the ordinal
pick (fsck clean, tripwires 0): -np 32 50.0 / 50.0 % in every phase, intents 16 / 16 / 17 / 17 (the
remote rank directories at setup and their teardown; `#test-dir.0` now sits beside `md`), 0 over the
timed ops; -np 64 50.0 / 50.0 %, intents 32 / 32 / 33 / 33, 0 timed; `tar -x` of `fs/`: 38 intents of
2,452 entries, files 55 / 45 % (its subtrees are uneven).

**Follow-ups (not in this task).** A per-directory override (Lustre's `lfs setdirstripe -D` on one
directory: `user.squeezefs.dir_spread_depth` on the parent, inherited); cross-volume striping of ONE hot
directory (§BZ.5 (C)); the `CreateInode` wire word's new meaning is a wire change the train's full gate
judges (never run here).

## The A-B-B-A (2026-10-27/28, quiet host on AC, the RAM-NVMe perf mount, dist builds)

`dev` `f2b6029` against `81469b3` (D = 3, the set formatted fresh by `perf_mount.sh`), legs A B B A,
`mpirun mdtest -n 10000 -u -i 1 -d <mount>/mdtest` (the rank directories at depth 3), gate lock and every
build slot held, tctl ≤ 58 °C, fsck clean after every run; k ops/s (leg 1 / leg 2).

**2 MDT + 2 OST** (the second member idle before; 50 / 50 % of every phase after):

| | -np 32 dev | -np 32 D=3 | Δ | -np 64 dev | -np 64 D=3 | Δ |
|---|---|---|---|---|---|---|
| file creation | 197 / 184 | 257 / 256 | +34.8 % | 197 / 205 | 261 / 239 | +24.4 % |
| file removal | 270 / 261 | 327 / 329 | +23.5 % | 253 / 204 | 361 / 345 | +54.5 % |
| directory creation | 223 / 214 | 277 / 291 | +30.0 % | 217 / 210 | 250 / 274 | +22.9 % |
| directory removal | 244 / 242 | 283 / 282 | +16.3 % | 221 / 235 | 297 / 294 | +29.8 % |

Two earlier rounds on the pre-§BZ.9 head `4eb35ee` read the same shape (+19 … +47 % on every row).

**1 MDT + 2 OST** (one healthy member: every directory local — the rule's cost must be nil):

| | -np 32 dev | -np 32 D=3 | Δ | -np 64 dev | -np 64 D=3 | Δ |
|---|---|---|---|---|---|---|
| file creation | 226 / 206 | 216 / 217 | +0.3 % | 221 / 208 | 212 / 216 | −0.3 % |
| file removal | 291 / 281 | 292 / 279 | −0.1 % | 262 / 265 | 241 / 269 | −3.3 % |
| directory creation | 212 / 207 | 222 / 208 | +2.4 % | 211 / 215 | 220 / 221 | +3.6 % |
| directory removal | 222 / 225 | 227 / 240 | +4.3 % | 229 / 237 | 244 / 242 | +4.4 % |

Before §BZ.9 the 1-MDT directory creation at -np 32 read −2.3 / −4.6 / −3.0 % over three rounds (one with
the leg order reversed) with every other row flat — the parent-record read before every mkdir. Also seen:
`dev` on 1 MDT is faster than `dev` on 2 MDT (file creation 213 k vs 181–200 k at -np 32): an idle member
still costs its checkpoint and barrier work; with the rule that member carries half the load instead.
tarx (`scripts/suites/tarx.sh`, 2,452 entries): PASSED on `4eb35ee` — B/S0 1.05–1.08 (law ≤ 1.10), wire
0.0003–0.0012 verbs per entry (law < 0.05). Landed (gates 156 and 157 green, 75/75). Sessions:
`/build/perf/dp-abba/`, `dp1-abba/`, `dp1b-abba/`, `dp1c-abba/` (4eb35ee), `dp2-abba/`, `dp3-abba/` (81469b3).
