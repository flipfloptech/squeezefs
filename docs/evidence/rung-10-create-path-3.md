# Rung 10 — the create path, wave 3: the chain (2026-10-20)

Continues `rung-10-create-path.md` (waves 1–2). **The bar** (owner): ext4's order of magnitude on
the in-memory substrate — `mdtest -np 32 -n 10000 -u` file creation 711 k/s, removal 573 k. Venue:
this workstation (32 threads, `7.2.8-9.30-omarchy-sqz`), the RAM NVMe set (512-B grain), every phase
started at Tctl ≤ 70 °C with no other benchmark, build or test of another tree running; A = dev
`329a3c9` (the main tree's dist build), B = `perf/create-path-3`. Scratch `~/tmp/createpath2/`.

## The chain as it stood (A, `fc`, `SQUEEZEFS_OP_PROFILE=1`)

The create is 319 µs: 18 µs of handler before the commit call, **300 inside it**, 1 after. Inside:
`meta_tx_queue_wait` 36 µs (enqueue → the pass drains); the pass's HOLD on 56 % of passes, 42 µs
each (22 % the §L pad-majority hold — a 1–2-tx window is under the 512-B grain —, 33 % the §AW
pacing hold: the four appliers busy); the applier's window 55 µs with `leaf_lock_hold` 100 µs per
4.4-tx window (≈ 40 the apply — 9 µs per tx, as the one-applier arm below shows —, ≈ 60 the appliers'
waits for each other's leaves); the lane 55 (the write 19.5: device 12.6 + two hops; the queue
behind other windows; the acks' fan-out); the heap admission's resolve on the committer 12. Hops per
create: four cross-thread wakes (committer → lane for the pass, lane → applier lane, applier → lane
for the submit, lane → committer for the ack) around one device round trip. The ack spin never
engages (`ack_spin_busy` ≡ creates: a FUSE worker lane is never idle beside its caller).

## Lever 1 — the pass venue (`SQUEEZEFS_META_PASS_VENUE=committer`, measured, `lane` ships)

The election winner runs the pass and the apply on its own thread (the election serializes passes
whatever thread hosts them; only the submit and the ack need the lane's ring), no pass fiber on the
lane. Red test: the storm with no pass on the lane's thread, every ack present, the reopen replays.
One defect met on the way: the lane's fiber re-ran `runScheduledPass` on its 2 s tick while a
committer held leadership mid-pass — two passes over one batch buffer, a member drained twice, the
lane's second ack into a dead frame; the venue spawns no fiber now. Measured (`-u`): file creation
84.0 k against A's 89.8 k, removal 70.8, directory creation 63.7, removal 48.0; the create 327 µs
against 319 — the leader applies a 10-tx batch serially (`leaf_lock_hold` 90 µs per window, 9 µs per
tx) where four appliers overlap theirs; the lane's CPU 2.2 → 0.9 s per 320 k creates. A CPU
reshaping, not a shorter chain; the convoy on the leader's FUSE lane is the risk. Not shipped.

## Lever 2 — the §L pad hold derives from the grain (decision 0003 §BA.1; shipped)

| arm (`-np 32 -u`, k/s) | fc | fr | dc | dr | create µs | windows | device B/op | pad B/op | lane CPU |
|---|---|---|---|---|---|---|---|---|---|
| `both` (§L + pacing, was shipped) | 88.5 | 89.2 | 58.8 | 51.3 | 323 | 73.8 k | 248 | 65 | 1.88 s |
| `pacing` (pad hold off) | 96.9 | 100.4 | 64.0 | 58.8 | 293 | 95.6 k | 270 | 87 | 1.77 s |
| `none` | 99.8 | 93.7 | 60.1 | 58.8 | 283 | 170 k | 343 | 160 | 2.19 s |

Shared directory (A/B of the arms, k/s): `both` 18.9/17.6 and 14.2/13.8; `pacing` 18.9/18.3 and
14.7/14.1; `none` 18.4 and 14.3 — inside the spread. `derived` ships: `padHoldFor(grain) = grain ≥
4096` (the pad hold's gain is a page of ring per round trip at a page grain; at 512 B it saved 22 B a
create and cost every member of a held batch a landing). Tie test on the derivation and the knob's
arms.

## Lever 3 — the applier's resolve seeded from the committer's admission (§BA.2; shipped)

`Queued.seeds`: the heap admission's resolved leaves ride the queue entry; the applier's A3 takes a
seed still mapped and covering its key and descends otherwise. 100 % seeded on mdtest (645 k of
645 k); the applier lanes' CPU 3.37 → 3.01 s per 320 k creates; the rate unchanged (the resolve
precedes the lock). Red test: quiet ⇒ every record seeded, none descended; a storm with checkpoints
⇒ seeded + descended ≥ records.

## What was measured and not taken

- **Per-tx reservations / group commit in the lane**: every reservation pads to the device grain
  (the pad law is what keeps two windows out of one sector), so a reservation per committer costs a
  grain per create — 2.3× the ring at 512 B, 20× at 4 KiB. The pass stays the batcher. The lane
  coalescing byte-granular reservations into grain writes is the owner's §5.2 amendment (§BA.4).
- **Ack after the RAM apply** (ext4's posture): the only path to 711 k — but a daemon crash is a
  power cut for its metadata, the kill rows' law. The owner's, with the crash matrix re-judged.
- **The handler's remainder**: the gather xattr → an inode flag is a format change (0002 row); the
  mint's census (≈ 3.5 µs) → a per-lane ino window must follow the lease generation; the negative
  LOOKUP is 4.5 µs now (wave 2's RwLock) and the kernel issues it on every uncached `open(O_CREAT)` —
  the daemon cannot pre-populate a name it has not seen.

## A/B against dev (A B B A B A, one hour, every phase at Tctl ≤ 70 °C and a quiet host)

| row | A (dev `329a3c9`) | B (`8c88ce7`) | B ÷ A |
|---|---|---|---|
| `-u` file creation /s | 87.9–98.8 k (mean 91.7) | 104.4–108.1 k (106.1) | **1.16** |
| `-u` file removal /s | 83.0–94.6 k (87.4) | 98.4–110.5 k (106.3) | **1.22** |
| `-u` directory creation /s | 58.4–66.0 k (62.9) | 61.7–64.1 k (63.2) | 1.00 |
| `-u` directory removal /s | 51.7–52.0 k (51.8) | 57.1–59.0 k (57.7) | **1.11** |
| shared directory, file creation /s | 15.7–17.6 k | 17.3–18.4 k | 1.07 |
| shared directory, directory creation /s | 12.5–12.6 k | 11.4–13.2 k | 0.98 (inside A's spread) |
| the classic run: creation / removal / dir creation / dir removal | 79.4 / 82.2 / 71.5 / 54.0 k | 86.7 / 92.4 / 73.7 / 59.6 k | 1.09 / 1.12 / 1.03 / 1.10 |
| file stat /s (the classic run) | 8.35 M | 9.35 M | the kernel's dcache serves it |

Against ext4 / xfs: file creation 106 k against 711 k / 471 k (15 %), removal 106 k against
573 k / 183 k, directory creation 63 k against 246 k / 369 k, directory removal 58 k against
456 k / 158 k. The bars stand. The chain as it stands: four thread hops around one device round
trip per create, the pass's pacing hold when the appliers are full, and the appliers' waits for
each other's leaves; the create is 290 µs of which ≈ 42 µs is the handler's CPU.

The data rows (`scripts/perf/quiet.sh ram-sqz cr3-{A,B}-r1`, 20 s each): 4k randread QD32×4
1.736 → 1.745 M IOPS, 4k randwrite QD32×4 1.137 → 1.149 M, seq write 1 MiB QD16×4 52.7 → 52.7 GB/s.

## Suites

Unit families (the wave-2 set plus `tokens`, `sym`, `xv`; `-Dfresh=true`): 1096/1168 passed, 72
skipped; `rails` 29/29; `fmt` clean. Smoke: INVALIDATION COHERENCE ×5 and once under each arm
(`committer`, `none`, `both`), THE SHARED HOT DIRECTORY ACROSS DAEMONS, DEAD MANAGER HOLDING A
STRIPED, THE KILL MATRIX, A STRIPED DIRECTORY ON ONE DAEMON, kill -9 mid-tar, PROMOTION, TWO-DAEMON,
READER: 49/49 (`-Dfresh=true`). `fsck --offline` clean after every session (32). No full gate (the
train gates).
