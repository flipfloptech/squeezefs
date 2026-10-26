# Rung 10 — self-apply follow-ups: the rmdir's prelude, and answers inside the resident prefix (2026-10-26)

Continues `rung-10-destroy-path.md`. Decision 0003 §BX. Venue: this workstation, `7.3.0-1.7-mainline-sqz`,
branch `perf/self-apply-followups` off `ca26920`. **No benchmark was run**: correctness runs only (rig
`~/tmp/destroy-self/mdrig.sh`, 2 MDT + 2 OST on 512-B loop devices, `mdtest -n 10000 -u`, settled phases,
a `.stats` delta per phase, fsck clean after every run); the owner's A-B-B-A judges the rates.

## 1. Directory removal under `self` (0.82–0.89× v0.1.0 in the owner's A-B-B-A)

The op profile (`SQUEEZEFS_OP_PROFILE=1`) put 610 of the rmdir's 633 µs BEFORE the commit (venue: 393 of
522); the commit itself was 22 µs (venue 128). Stage stamps around the prelude, sampled op trace, -np 32:

| rmdir span | self | venue |
|---|---|---|
| entry → the parent's stripe verdict | 189 µs | 128 µs |
| around the VICTIM's stripe verdict (`mapOf`) | 373 µs | 251 µs |
| guards, reads, ENOTEMPTY probe | 9 µs | 11 µs |
| the commit | 23 µs | 132 µs |

`mapOf(victim)` missed every time (`dir_stripe_verdict_reads` 1.0 per rmdir: a directory is never looked up
through the cache before it dies), read the inode and the STRIPED marker, then INSERTED the dying directory's
plain verdict under the cache's exclusive lock. The write-preferring `RwLock` queued every rank's shared
probe behind those inserts — the parent's verdict, a hit, waited 189 µs. Fix: the victim's verdict is read,
never remembered (`victimMapOf`); a striped victim still installs its map. Red `ce09dfe` (a reader inside the
cache: the rmdir joined its queue), fix `e23cad3`. After it the rmdir is 120 µs at -np 32 (prelude 25 µs).

| directory removal, correctness runs (before: `ca26920`) | self | venue |
|---|---|---|
| -np 32 | 49 → 198 k | 59 → 103 k |
| -np 64 | 45 → 194 k | 51 → 110 k |

Smoke: the striped-directory rows, RENAME_EXCHANGE AND THE PARENTS' NLINK, the unlink storm, THE REPLAY PROOF,
the kill rows, two mounts, the shared hot directory, the 32-stream mint storm — 10/10 green.

## 2. The death-replay hole (§BX)

Red `6956c0a`: A held before anything of it reaches the journal tail, B in a later window answered at the
apply; `judgedReplay` → `past_hole` 2 (self), 1 (venue): the supervisor drops B. Law: an answer ahead of the
landing is released only when every reservation below its own entry is landed or in the tail (the
registry's `resident_upto`, model 14). Fix `888d709`: `past_hole` 0 in both arms. Unit 3066/3231 (165
skipped), rails, fmt; smoke 14/14 (THE REPLAY PROOF, the kill rows and matrix, both tar kills, the unlink
storm, two mounts, 300 fsyncs, the idle-RSS rows); xfstests generic/001 013 074 753 pass.

**The cost is not a fence and a cursor.** Clean interleaved A/B (`e23cad3` → `888d709`, `self`, quiet host):

| | -np 32 before → after | -np 64 before → after |
|---|---|---|
| file creation | 228 → 152 k | 211 → 128 k |
| file removal | 238 → 135 k | 221 → 122 k |
| answers that waited / mean wait | 0 → 56 % / 115 µs | 0 → 67 % / 345 µs |
| daemon CPU per op | 63 → 83 µs | 65 → 100 µs |

It is a convoy: every answer waits for EVERY earlier member's claim, and one member slow to run (64 ranks on
32 CPUs) holds all later answers. The venue arm waits on 45–50 % of its windows (100–450 µs): a window that is
not all this kernel's ops never reaches the tail before its landing. The options are in §BX.5.
