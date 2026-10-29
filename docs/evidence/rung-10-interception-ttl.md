# Rung 10 — the interception mount's TTLs: isolated, coherence by invalidation (2026-10-28)

The owner's ruling: "the interposer TTL should be 100 % done". §CB.5 (a): an `-o interception` mount
started PEERED (§AB.2), every kernel TTL 1 s against the plain mount's `T_owner` (45 s). Decision 0003
§CE. Branch `feat/interception-ttl` off `7693bab`; this workstation, `7.3.0-1.7-mainline-sqz`. **No
benchmark was run**: correctness runs with counters — the gate-profile binary, 2 MDT + 2 OST loop images
under `/build/tmp`, a mountpoint of the run's own, mdtest with a rank-0 phase hook that reads `.stats`
before and after every phase and pauses 1.5 s before each (every phase starts past the 1 s class),
`SQUEEZEFS_OP_PROFILE=1`, fsck after every run. Rig: `~/tmp/ittl/{arm.sh,phases.py,compare.py}`.

## What changed

- The TTL law starts ISOLATED on an interception mount (a reader or a joiner still starts peered; the
  first join still flips it). Every class is `T_owner`, except a regular file's SETATTR/LINK reply there
  (the kernel installs those without its `attr_version` check): the base attr class.
- The limiter (`ipc/host/inval.zig`) drives coherence by invalidation, never expiry: a bind fires
  nothing; the binding's first ring write and the first of every window fire WHOLE; a grow is announced
  awaited (as before); a suppressed write marks its binding row dirty, and the ctl poller's trailing
  sweep (once per window), the unbind and the teardown fire WHOLE for a dirty row; a write on a dead row
  fires itself; a parked ring atime fires attrs-only; a full sideband queue falls back to the reserved
  attrs-only cells. `FUSE_AUTO_INVAL_DATA` is never advertised (0002 row 446), so the old 1 s TTL never
  refreshed a page: a suppressed write stayed stale in a buffered reader until the binding's next fire;
  now ≤ 2 windows.
- A kernel size set lowers the announced high-water and moves a generation (`ipc/ring_inodes.zig`):
  a regrow below the old high-water is announced.

## Counts (LOOKUP + GETATTR per phase; b = `7693bab`, a = the branch)

| -np 32, `-n 2000 -u` | b plain | b icpt | b preload | a plain | a icpt | a preload |
|---|---|---|---|---|---|---|
| dir stat / rename / removal | 6 / 64,004 / 7 | 64,099 / 126,795 / 64,075 | 64,101 / 128,064 / 64,083 | 4 / 63,855 / 7 | 4 / 63,496 / 7 | 6 / 63,757 / 6 |
| file stat / read / removal | 4 / 7 / 4 | 64,063 / 64,076 / 63,679 | 63,959 / 63,968 / 64,096 | 7 / 4 / 7 | 7 / 4 / 7 | 9 / 6 / 9 |
| preload notifies: file create / read | | | 125,708 / 127,490 | | | 0 / 0 |

-np 64 the same shape at twice the counts (b icpt dir stat 125,861, rename 256,075; a every arm 4–9,
rename = the plain's negative LOOKUP of the new name). `-w 4096 -e 4096`: file stat is a GETATTR per
file in every arm (the kernel's write, or the ring's announced grow, invalidated the attrs; 111–128 k at
-np 64 in every arm, run to run); file read 4–9 in every arm after (b: 59–128 k LOOKUPs); preload
notifies per written file 3 → 2, per read file 2 → ≈ 0.2 (attrs-only atime). The per-kind alternative,
emulated on the base binary (entry classes 45 s, attrs 1 s): file stat 64,093, file read 64,097
GETATTRs. Every run: fsck 0 findings, 0 tripwires.

## Red tests

Smoke "THE INTERCEPTION TTL (decision 0003 §CE)" (`ipc/host/e2e_linux.zig`): on `7693bab` "48 stats past
1 s asked the daemon 49 LOOKUP/GETATTR"; on the branch 0, removals 0, a bind/unbind pair with no I/O 0
(`7693bab`: 1); then, with `T_owner` in force, a kernel reader on another fd sees a ring grow at once, an
overwrite's mtime and bytes (the trailing sweep; `7693bab`: bytes stale past 10 s), a kernel truncate's
regrow at once (`7693bab`: size 0), a ring read's atime, another process's `stat`; a dirty unbind fires.
Each step probed on `7693bab` with its assertion printed instead: stats, bind/unbind, overwrite bytes,
truncate regrow red; removals and the dirty unbind's bytes green. The per-inode-short-TTL design failed its bytes step (pages
stale past 10 s). Units: `inval` "the ladder" / "the ladder's edges", `ring_inodes`, `kv_fs` "an
interception mount is isolated", `session`. Models (`ipc/ring_inodes_model.zig`): `Unbind` holds and
fails without the writer's or the unbinder's fence; `Sweep` holds from a clean and a pre-marked row and
fails with the flag raised before the mark.

## Checks

`zig build rails fmt` 32/32; unit filters `ipc.` 154/158 (4 smoke skips), `fuse.` 367/386 (19
kernel-feature skips), `shim.` 79/82, `sdk.` 60/60; smoke `INTERCEPTION` 42/42, `DIRECT ARM` 40/40,
`INVALIDATION` 40/40; `zig build shim-smoke` 43/43 (rows 1–5c); xfstests generic/001, 013, 426, 604 on
`-o interception` mounts (12 daemons, host armed): Passed all 4, fsck 0, 0 alerts (the "did NOT pass"
is the subset's stale-ledger check; the harness has no preload hook).

## Gate 163's red (decision 0003 §CE.10)

Gate 163 failed the row at "49 LOOKUP/GETATTR" with the law ISOLATED: the row's names were unpinned, and a
host-wide `drop_caches` (the export-handles row, shard 5 of the same gate) or reclaim evicted them in the
window (fuse evicts an inode with its last dentry). By hand: one drop in the window = 49 asks, law isolated,
56 inodes evicted; pinned = 0; pinned on `7693bab` = 49 (the red stands). The gate's shard 2/16 beside a
drop every 5 s reproduced it on its first run (58 evicted). The row now pins every counted name with an
`O_PATH` fd, asserts the new `fuse_ttl_law_peered` gauge 0, prints the evictions; the ready line prints the
law's current classes and posture. 20 × green beside the full smoke's 16 shards and a drop every 5 s.

## Seen, not changed

§CE.9: a bind racing a kernel truncate of the same inode (its regrow announced by the leading fire,
not awaited); the harness's missing preload hook; finding 3's per-open bind round trip.

## The A-B-B-A (2026-10-29, quiet host on AC, the RAM-NVMe perf mount, 2 MDT + 2 OST, dist builds)

`dev` `d80407c` against the branch (`c7ccd5f`), legs A B B A, three arms per leg — a plain mount,
`-o interception`, and `-o interception` with the shim preloaded into every rank
(`mpirun -x LD_PRELOAD=…/libsqueezefs_il.so`) — `mdtest -n 10000 -u -i 1`, gate lock and every build slot
held, tctl ≤ 70 °C, fsck clean after all 40 runs; k ops/s, the mean of two legs per arm:

| | icpt -np 32 dev → branch | icpt -np 64 | preload -np 32 | preload -np 64 |
|---|---|---|---|---|
| directory stat | 4,371 → 13,782 (+215 %) | 1,481 → 8,378 (+466 %) | 4,922 → 11,555 (+135 %) | 1,379 → 8,465 (+514 %) |
| directory rename | 225 → 271 (+20 %) | 275 → 292 (+6 %) | 239 → 275 (+15 %) | 268 → 299 (+12 %) |
| directory removal | 239 → 290 (+21 %) | 250 → 300 (+20 %) | 243 → 289 (+19 %) | 250 → 301 (+21 %) |
| file creation | 256 → 251 (−2 %) | 258 → 256 (−1 %) | **63 → 140 (+122 %)** | **41 → 144 (+257 %)** |
| file stat | 4,137 → 10,050 (+143 %) | 1,822 → 8,207 (+350 %) | 2,620 → 11,875 (+353 %) | 1,711 → 11,545 (+575 %) |
| file read | 861 → 892 (+4 %) | 881 → 904 (+3 %) | 252 → 260 (+3 %) | 247 → 268 (+8 %) |
| file removal | 286 → 321 (+12 %) | 285 → 340 (+19 %) | 267 → 322 (+21 %) | 288 → 348 (+21 %) |

The plain arm (the code path this change leaves alone) over three rounds (`ittl-abba`, `-abba2`, `-abba3`;
one -np 64 branch run of round 2 lost a rank to the desktop's `RLIMIT_RTTIME 0` in the benchmark
script, since lifted): every row within ± 5 % of `dev` but the kernel-cached stat rows, whose legs swing
± 30 % on `dev` itself (file stat −3 % / +3 %, directory stat +9 % / −5 % at -np 32 / 64 over all legs).
Directory creation is +1–8 % in every interception arm. Landed (gate 165 green, 75/75). Sessions:
`/build/perf/ittl-abba/`, `ittl-abba2/`, `ittl-abba3/`.
