# Rung 10 — the create path, wave 9: the work under the leaf lock and around it (2026-10-26)

Continues `rung-10-create-path-8.md` and `rung-10-self-apply-followups.md`. Venue: this workstation
(`7.3.0-1.7-mainline-sqz`), branch `perf/create-work` off dev `6992a4c`. **No benchmark**: correctness
runs only (rig `~/tmp/create-work/mdrig.sh`: 2 MDT + 2 OST on 512-B loop files, supervised,
`mpirun -np 32 mdtest -n 40000 -u`, `SQUEEZEFS_OP_PROFILE=1`, `.stats` around the phase, `perf record -g`
for 3 s mid-phase; A = dev, B = the branch, legs back to back on a host checked quiet; fsck clean after
every run). Per-create numbers are `.stats` deltas; CPU splits are perf's call-graph shares × the daemon's
CPU per create. The host was 35–45 % busy in every create phase: the rate is latency-bound.

## Where a create's µs went (dev)

Daemon CPU 82.8 µs per create. The create handler 124 µs (perf attached): 15.9 before the commit
(the gather opt-in's xattr probe 2.4–3.0, the parent's read 2.8, the EEXIST scan 1.8, the mint 1.0,
the guards 0.8, the staging 0.7), the queue wait 57, the apply 10.8 (the hold 9.9). Outside it the
negative LOOKUP 6.2 µs. Under the hold: the chunk copy 2.4, the pre-images 2.1, the snapshot publish 1.2,
the run edit's spine 0.8, the owned record copies 0.4. Across all of it the record index's binary
searches were ≈ 10 % of the daemon's CPU, 5.4 µs of it key compares whose cost was the key bytes'
cache misses (a second dependent load per probe into the 256 KiB source image). The turn spin
(`spinForAck`, §BC) burns 8.6 µs per create in clock reads — CPU, not work; untouched.

## The cuts (each red-first where a law is involved)

| commit | change | per create |
|---|---|---|
| `d501e34` | `key.order` eight bytes a step; a key's group end walked, not searched (`GROUP_WALK` 8) | group searches −2.5 µs; the word compare unmeasurable (miss-bound) |
| `7b0339c` red, `b083526` | no pre-images for a member answered at its apply (§BB.5: an answered window never rolls back); `meta_conveyor_undo_keys` | −2.1 µs, all under the hold; kernel creates/unlinks capture 0, leafwise destroys keep theirs |
| `7aeda7d` | the conveyor applies staged records borrowed (`applyLockedRefs`) | −0.4 µs and 6 allocations + 6 frees under the hold |
| `b0f8740` | a chunk with no dead bytes copied verbatim (two memcpys) | −1.0 µs under the hold |
| `78fb2f8` red, `540fefc` | an inode read decodes the borrowed record (`lookupView`) | one allocation + free per inode read |
| `1ce0a29` | the record index orders entries on a 3-byte key discriminator in `IdxRec`'s padding (still 24 B) | key compares 5.4 → 1.7 µs; LOOKUP 6.2 → 4.8 µs wall |

All six against dev (back-to-back legs): the hold 9.9 → 6.4 µs per create, the apply 10.8 → 7.2,
the handler's pre-commit 15.9 → 12.7, LOOKUP 6.2 → 4.8, daemon CPU 82.8 → 79.1 µs per create (some of
the cut misses moved to colder later readers: a pre-image lookup no longer warms the next LOOKUP's
probes). The queue wait read 57 → 47 µs in that pair (53 → 48, 58 → 43 in two earlier pairs).

## Laws and tests

`key.order` ≡ `std.mem.order` (every length pair 0..40 × a difference at every position; 20,000
random pairs). Group bounds ≡ the two partition points (groups of 0–37 records across chunks). The
pre-image test: a kernel create answered at its apply captures none; a commit acked at the landing
captures its three keys and its failed write still rolls back. The node_cache property test (stragglers,
freezes, rollbacks vs the by-seq fold) runs both apply forms. A verbatim chunk copy reads like the
compacted one; a chunk with dead bytes compacts. The discriminator search ≡ the full-key order over 48
random indexes (one directory, mixed kinds, all-tie discriminators; built whole and extended, prefix
kept or moved); an inverted discriminator order fails it. An inode read allocates nothing.

Unit filters `kv_fs` 122, `node_cache` 77, `economy` 71, `record` 459, `conveyor` 111, `meta.backend` 83,
`node_snapshot` 55, `tree`/`node`/`bset` green; rails 32/32; fmt clean. Smoke 49/49 (300 concurrent
creates, two mounts on one host, the mint windows, the unlink storm, fsck and scrub, online fsck, file
handles, the striped directory, RENAME_EXCHANGE nlink, the replay proof). xfstests generic/001 013 426
604 passed, fsck clean. mdtest -np 32 / 64 -n 10000, all four phases: complete, fsck clean.

## Seen, not cut

The gather opt-in's per-create xattr probe (2.4 µs): a memo validated by `ino_witness` also needs the
slot-move epoch (a slot that leaves and returns reloads leaves without a bump); not plumbed into KvFs.
The publish's old-snapshot destroy (1.1 µs) and the run edit's spine retains (0.8 µs) are cross-core
refcount traffic under the hold — a deferred release needs an `ArcSwap` exchange (a modelled core).
The journal thread (pass + lane) is ≈ 70 % busy per create phase — the queue wait's server.
