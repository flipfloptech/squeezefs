# Rung 10 — directory operations under `-o interception`: the fold's verdict insert (2026-10-28)

Finding 2 of `rung-10-perf-matrix-9811f84.md`: mdtest `-n 10000 -u -np 32` on the interception mount
with the shim preloaded read directory stat 78 k/s (plain mount 8,693 k), rename 34 k (271 k), removal
37 k (279 k); file rows and directory creation unmoved. Decision 0003 §CB. Branch `fix/interception-dirs`
off `8b14376`; this workstation, `7.3.0-1.7-mainline-sqz`. **No benchmark was run**: correctness runs
with counters (the gate-profile binary, 2 MDT + 2 OST loop images under `/build/tmp`, a mountpoint of
the run's own; `.stats` before and after every mdtest phase from rank 0's phase hook; `SQUEEZEFS_OP_PROFILE=1`;
`perf record -g` of the daemon; fsck after every run). Rig: `~/tmp/idirs/{arm.sh,prof.sh,probe.sh,phases.py}`.

## The split

Three arms, `-n 2000` then `-n 6000 -D`: (a) plain, (b) `-o interception`, (c) `-o interception` +
`mpirun -x LD_PRELOAD=…libsqueezefs_il.so` (with `SQUEEZEFS_TEST_IPC_ALLOW_DIRTY=1` on both ends — the
`-dirty` pairing refusal had made the first (c) kernel-served; engaged: 32 sessions, 128,000 binds,
64,000 ring reads + writes). The directory phases' FUSE op counts in (c) equal (b)'s: the shim interposes
no `stat`, `rename`, `rmdir` or `opendir`. The mount option is the whole difference, through the TTL law
(§AB.2): an interception mount is PEERED — every kernel TTL 1 s where the plain mount's is `T_owner`
(45 s) — so a directory a second old is re-asked (LOOKUP) at every stat, rename and rmdir. Plain (a),
`-n 6000`: the stat phase reaches the daemon 7 times; rename = 191,922 negative LOOKUPs (the new name)
+ 191,916 RENAMEs; removal = 192,000 RMDIRs, 0 LOOKUPs. Interception (b): stat 34,016 LOOKUPs (the dirs
past the TTL), rename 350,040 LOOKUPs, removal 192,081 LOOKUPs + 192,000 RMDIRs.

## The cause

A round trip is not the cost; its price was. Isolated phases (`-D -C`, 2.5 s, `-T`, 2.5 s, `-r`; -n 4000;
`SQUEEZEFS_OP_PROFILE=1`): the stat phase's 128,189 LOOKUPs had `backend` 512 µs–1 ms each (an idle
single-threaded directory LOOKUP after the TTL: 8 µs, a file's the same); the rmdir phase's RMDIRs waited
256 µs–1 ms on their lane (`handler_to_backend`) behind them with a 16–64 µs backend. Counters per phase:
`dir_stripe_verdict_reads` 128,090 ≈ `dir_stripe_verdict_try_refusals` 128,186 ≈ LOOKUPs, one cache
flush. `perf record -g`: every futex park under `kv_fs_stripes.verdict ← foldIfStriped ← lookup` (and
`← findChild`, `← lockCache`). The positive LOOKUP (and GETATTR) of a DIRECTORY folds it if striped:
`mapOf` → `verdict(.remember)` — a plain-hint MISS for every directory never asked as a parent (mkdir asks
the parent's verdict, not the child's) — and the miss inserted the child's hint under the verdict cache's
EXCLUSIVE lock. The write-preferring `RwLock` queued all 32 workers' shared probes behind each insert, the
drain workers' try-only probes were refused and re-asked on the handler lanes, which parked: a scheduler
wake per op. §BX.1 removed the same shape from rmdir's victim; the lookup's object had it still. A file's
lookup folds nothing — hence files unaffected; the plain mount never LOOKUPs a cached directory — hence
unaffected there.

## The fix and the red test

`Stripes.foldMapOf(global, v)`: the verdict with the record the lookup already holds (`verdictRecord`,
no second inode read) and a plain miss FORGOTTEN; `KvFs.foldIfStriped` asks it. A striped directory
still installs its map; a directory asked as a parent remembers as before — the first op under a
directory fills the hint its children's ops need. Red: kv_fs_stripes "a positive lookup or a stat of a
plain directory reads its verdict and never remembers it" — with a reader inside the cache, `lookup`,
`getattr`, `probe_lookup`, `probe_getattr` complete with nobody queued and no try-refusal, +1 verdict read
each, the hint cache unchanged; the first op under the child remembers it. Red on `8b14376` at
`expect(!queued)`.

## After (counters; the same rig)

| phase (interception, -n 4000, isolated) | before | after |
|---|---|---|
| directory stat, 128 k LOOKUPs | 49 k/s; backend 512 µs–1 ms; try-refusals 128,186 | 968 k/s; backend 4–16 µs; try-refusals 0 |
| directory removal | 28 k/s; rmdir lane wait 256 µs–1 ms | 143 k/s; lane wait 8–32 µs; backend 64–256 µs as plain |

Full arms, `-n 6000 -D` (stat / rename / removal): plain 2,350 k / 158 k / 186 k; `-o interception`
1,689 k / 147 k / 155 k; + preload 1,733 k / 142 k / 155 k — the residue is the LOOKUP round trip the 1 s
TTL buys. File rows (`-n 2000 -w 4096 -e 4096`) equal across the arms within the TTL's LOOKUPs. fsck 0
findings, 0 tripwires, every run.

## Checks

`zig build fmt rails`; unit filters `fuse.` 363/382 (19 kernel-feature skips), `ipc.` 151/154, `shim.`
79/82, `sdk.` 60/60; smoke rows THE INTERCEPTION HOST, THE DIRECT ARM, INVALIDATION COHERENCE ACROSS
DAEMONS (43 pass); `zig build shim-smoke` 43/43 (rows 1–5c); xfstests generic/001, 013, 426, 604 on
`-o interception` mounts (12 daemons, host armed): Passed all 4, 0 alerts, fsck 0 (the "did NOT pass" is
the subset run's stale-ledger check; the harness has no preload hook).

## Seen, not taken (§CB.5)

The TTL posture for directories under interception alone (a per-kind TTL: an owner call on §AB.2); 32
ranks reading `.stats` at once wedged a plain-mount daemon (one `sqz-fuse-ur` running 30 s, the
control-inode stat unanswered, SIGABRT; `~/tmp/idirs/out-a` first run); finding 3's per-open bind.
