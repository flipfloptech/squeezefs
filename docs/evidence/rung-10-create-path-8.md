# Rung 10 — the create path, wave 8: the mint window (2026-10-26)

Continues `rung-10-create-path-7.md`. Venue: this workstation (32 threads, `7.3.0-1.7-mainline-sqz`);
branch `perf/ino-windows` off dev `380866f`. Counters only, no benchmark (another agent shared the
host, load 3–13): `mpirun -np 32 mdtest -n 10000 -u` on a FILE-BACKED 2-MDT + 2-OST set under
`/build/tmp`, `.stats` around each phase, `fsck --offline --scrub` after every run (all clean, every
run 320,000 files). Scratch `~/tmp/ino-windows/` (`storm.sh`, `runs/*`). Decision 0003 §BV.

## The hot leaf, named

Wave 7 left 62 µs of every apply window waiting for leaves other appliers held. A temporary
classifier on the apply's takes (dev, unbound ranks): **2,932 of 3,270 ms of take waits per create
phase were on the RIGHTMOST leaf of an inode tree**; dentry leaves 308 ms. The rotor's headroom law
counts trees in nodes and its tie went to `rotor[rr % n]` or else the FIRST most-headroom tree, so
concurrent creates funnel into one tree's right edge until it splits, then into the next: 44 % of
mints landed in a slot one of the previous eight mints used (12 % for a uniform pick).

## Why not an ino range inside the slot

A slot tree is ordered by ino: a fresh range lies above the tree's last key, in its rightmost leaf,
until splits cut that leaf between ranges — and the headroom law leaves the tree at its first split.
Ranges sharing a tree share its hot leaf (§BD.2: no change). So the window is a TREE: a stream claims
one rotor tree's edge (the headroom pick among the trees no other stream claims) for
`windowMints(node_size)` mints (one node of inode records, 1,386 at 256 KiB) or one checkpoint age.
No ino is reserved — each mint stays the cursor's `fetch_add`: nothing returned or skipped, inos
dense, `statfs` and the recovery fold untouched.

## Who gets a window

| step (counters per apply window, mdtest create phase) | leaf waits | hold |
|---|---|---|
| dev, unbound ranks / ranks bound to hw threads | 58–66 / 74 µs | 111–128 / 136 µs |
| a THREAD's window, unbound / bound | 44 / 5.2 µs | 103 / 71 µs |
| a CALLER's window (the FUSE pid), unbound | 4.0 µs | 57 µs |
| **the final rule, unbound (load 13)** | **4.2 µs** | **55 µs** |

An unbound rank moves between CPUs, so one drain worker served two ranks and its window held both:
the window follows the caller (`Store.allocateIno` takes the pid; no caller = the thread). It opens
beside another ACTIVE stream (two decisions of its own): a one-off `mkdir` takes a tree of its own
but never makes a serial creator concurrent — THE TWO-MEMBER row's shell kept its 128 guest trees,
which every-decision-counts had cut to two. The affinity arm yields to another stream's window on the
parent's tree and makes it the stream's window: without it the one-off `mkdir`s funnelled the
32-stream row's directories into the smallest tree and 13–15 k affinity mints shared its edge (16–20
µs per window). The census's tie now goes to the first candidate at or after the rr cursor. The
removal phase: 55 → 14 µs per window. Inode-rightmost waits per create phase: 2,932 → 129 ms.

## The handover law (red first)

Red (`5d444c9`, the plane's mint seam): A's create decides its slot and parks; A hands the slot to
B; B mints off the cursor A carried; A resumes — `A's create answered ino 65539 (slot 1); B minted
ino 65539`. Dev has the race for every census mint (µs); a window caches its decision for a
thousand. Fix (`cd52a2c`): an ino is kept only if, after its `fetch_add`, the slot still names this
mount at the decision's lease generation and is not releasing; else it is burned (`mint_burns`) and
redone (`rotorCap(MINT_SPREAD)` burns in a row answer `Deferred`). The cursor's `fetch_add` and
snapshot are seq_cst, a Dekker pair with the release's gate store and cursor read. Model
`mint_window_model.MintRecheck` holds; no recheck, a recheck blind to the generation, and the gate
store drained after the cursor read each fail; `WindowClaim`: a check-then-store names two holders.

## Tests

Unit: the race (red → green, one burn); eight minters past two refills with a windowed tree handed
to B mid-run (8 claims on 8 trees, every ino distinct); two eight-thread sessions around a remount;
the window follows its caller across threads, a one-off caller beside a lone creator; `mintChoice`'s
claimed skip and cyclic tie; the derivations. Filters `kv_fs mint ino conveyor node_cache slot
handover migrate meta_set acquire rotor two-member` green; `rails` 32/32; `fmt` clean. Smoke THE
MINT WINDOWS (32 shell streams × 2,000 creates, a fresh set per arm): census 16.9, window 4.7 µs of
waits per window, hold 48 → 35 µs, affinity mints 0, 91 grants, no burn, fsck clean.

## What remains

The hold is work: ≈ 50 µs per 5.7-transaction window, ≈ 9 µs per create under the leaf locks. One
hot leaf per stream crosses the 4 KiB writeback threshold more often: freezes 80 k × 8.8 µs → 136 k ×
4.1 µs per create phase (the same ~0.6 s). Not run: the RAM NVMe set, `-np 64`, `self` apply.
