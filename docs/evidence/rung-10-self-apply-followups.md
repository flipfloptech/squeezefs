# Rung 10 — self-apply follow-ups: the rmdir's prelude, and answers inside the resident prefix (2026-10-26)

Continues `rung-10-destroy-path.md`. Decision 0003 §BX. Venue: this workstation, `7.3.0-1.7-mainline-sqz`,
branch `perf/self-apply-followups` off `e23cad3`. **No benchmark was run**: correctness runs only (rig
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
registry's `resident_upto`, model 14). The first fix (`888d709`) claimed each entry after its apply; every
answer then waited for every earlier member's apply — a convoy (clean A/B, `self`: file creation 228 → 152 k
at -np 32, 56–67 % of answers parked 115–345 µs). The owner's ruling: keep the law, cut the convoy.

- **The entry at the turn** (red `7702840`, fix `24f8eaa`): a member claims its entry at its turn, before its
  apply; a failing member claims a PAD, releases its entry, then answers its error; the venue claims its
  image at the window's start, for every window. Parked: 26–36 % / 41–46 % (-np 32 / 64), 110–356 µs — a
  turn delivered is not a turn run.
- **The help** (red `86bcfc0`, fix `33c9d61`): each entry (image) is claimed once by CAS, by its owner or by
  a parked answer, which claims what holds the prefix below it before it parks. Targeted wakes; the waiter
  list under the executor's mutex (a spin lock wedged THE MINT WINDOWS row on the first cut).

| correctness runs (counters per phase) | self -np 32 | self -np 64 | venue -np 32 / 64 |
|---|---|---|---|
| answers that park | 10–17 % | 19–29 % | 6–15 % |
| mean wait (help included) | 24–36 µs | 68–127 µs | 5–16 µs |
| file creation / removal | 220 / 232 k | 191 / 190 k | 152–146 / 132–135 k |
| directory creation / removal | 173 / 200 k | 129 / 181 k | 84–89 / 87–94 k |

`past_hole` 0 in both arms; models 14 and 15 hold with every weakening failing; unit 3070/3235 (165
skipped), rails, fmt; smoke 14/14; xfstests generic/001 013 074 753 pass; fsck clean after every run. What
still parks at -np 64: an owner preempted inside its claim (64 ranks on 32 CPUs), a window the pass is still
handing out, an SMO entry (it reaches the device only by its landing).
