# Rung 10 — the direct arm's WRITES: a client patches the data device in place under a recallable write lease (2026-10-20)

Wave 3 of the client bypass (`rung-10-client-bypass-rows.md`, `rung-10-client-direct.md`): reads
reached ext4/xfs parity by the client reading the device itself; writes stayed on the ring at
39–51 % of ext4. The W1 patch is a shape a client can issue — one aligned DMA into a block the file
owns alone, no read-modify-write (`rung-10-write-iops.md`) — given two promises from the daemon:
nothing of its own shadows the device for the inode while the client writes, and the inode's
layout never changes under a write in flight. Charter §7 amended; decision 0003 §AY.

**The recall protocol.** A WRITE lease is the read lease plus a slot in the session's ledger
(`write_lease_core.Words{tag, inflight, recall}`, client-writable shared memory) and a grant that
holds only when every mapped block passes W1's own state half (`WritePath.probeDirectWriteLease`:
sole-owned, undecorated, passthrough, no custody — the fence pair run and the word re-stabilized).
At the grant the inode's and every block key's stripe is marked (`lease_epoch`): the read path
deposits nothing for the keys and the mover stays off them — the inode is device-true — and the
keys are purged from the four tiers. The lease is a token the daemon RECALLS before every mutation
of the inode, with nothing held: at a kernel write's admission (a Shared-class write may still land
an overlay), at an exclusive op guard (truncate, setattr, unlink, an extending or CoW write), at the
layout lock (every publish, any origin), at the forget. The recall is Dekker's — the daemon stores
`recall := 1` (SeqCst) then loads `inflight`; the client stores `inflight += 1` (SeqCst) then loads
`recall` and backs out on anything but LIVE — so a count of zero lets the daemon proceed AT ONCE (no
round trip) and a count above zero is a client `pwrite` genuinely in progress, which the daemon
polls out through its `Io` (a fiber parks; never a lane's thread in a kernel futex) up to 1 s, then
FORCES: `recall := 2`, `ipc_write_leases_forced` fires, the session is poisoned, and a write that
landed under force answers EIO — the one lossy case, loud on both sides. `WriteLeaseModel`
(`ipc/write_lease_model.zig`, the explorer with TSO store buffers) proves the invariant — never a
mutation with a write in flight; every client write landed before the mutation or refused — and
that dropping either side's fence breaks it (the store-buffering failure). The postlude is a
batched `WriteNote` (64 writes / 10 ms / the lease's drop): the dirty mark that stamps Δ(mtime, ctime)
at the open's flush (§L.6's law), KD-11's notify, the fsync touch; `fsync`/`fdatasync` are now
interposed (the census 67): the note acked, `fdatasync` on every device written, then the kernel's
FSYNC. A direct write's crash atomicity is the device's sector atomicity — ext4's O_DIRECT shape.

**Red first.** The core (`enterWrite`/`exitWrite`/`recall`/`force`/`grant`/`retire`, the tag
guarding a re-granted slot) and its model; the lease-epoch stripes (inode and key) and the recall
hook; the recall over the ledger (quiescent at once; drained when the client exits; FORCED at the
bound with the tripwire); the wire's write frames and fuzz rows; the device screen's write bit; the
e2e's write steps — a direct write byte-exact through an O_DIRECT kernel fd with `ipc_ops_write`
flat and the same lease serving the read, an extending write the ring's, fsync's acked note, **a
write held in flight making a kernel truncate WAIT at the recall** (150 ms observed, 0 forced, the
truncate landing on the exit) and the next write finding the lease dead. **The kill rows** (`shim
smoke`, a C writer under LD_PRELOAD on a `-o interception=direct` mount): kill -9 the WRITER
mid-run — 425,679 pages acked, every last-acked page intact (nothing acked is lost; the unsent
partial note's dirty mark is the only loss); kill -9 the DAEMON with writes in flight — 1,499,650
pages acked, the writer FAILS LOUD (rc 1, an error line within the heartbeat's second), every acked
page intact after the remount (the device holds only acked bytes, the layout unchanged); **the
dm-log-writes cut** after fsync'd writes (`src/shim/direct_cut.sh`: the data volume under
`log-writes`, every page fsync'd, the daemon killed mid-run, the mark, the replay onto a fresh
device to the LAST FLUSH before the mark, the remount) — every synced page survives, `fsck --offline`
clean. Counts: units 775 (ipc/lease/shim/sdk/wire/op_guard/write_admit/read_path/plane), rails
29 with the census 67/67, `sdk-smoke`, `shim-smoke` 41 incl. rows 5a–5c, smoke 516 incl. both
interception e2e rows; fmt clean.

**Measured** (the same substrate, psync shapes and rules; dist `39febad`; rows under
`/build/perf/mx/ram-sqz-il-direct-rw3b` and `-rmix`; the write rows 100 % direct — 32 write leases,
0 recalls, 0 rechecks refused, 0 forced; the daemon's whole part is `sqz-ipc-ctl` 0.2–0.3 cores
taking the notes):

| row (psync; 32 jobs unless QD1) | ext4 | xfs | FUSE | ring bypass | **direct** | ÷ ext4 | Tctl |
|---|---|---|---|---|---|---|---|
| 4k randwrite QD1 (IOPS, p50 µs) | 217,747 (4.6) | 215,840 (4.0) | 84,502 | 110,633 | **215,131** (4.5) | 99 % | 55→54 |
| 4k randwrite ×32 | 3,027,972 (9.0) | 2,791,974 (9.7) | 789,400 | 1,168,926 | **3,032,734** (8.8) · 3,069,995 (re-run) | 100–101 % | 54→81 |
| 64k randwrite ×32 (MiB/s) | 76,207 | 76,203 | 30,076 | 31,201 | **76,083** | 100 % | 58→86 |
| 1 MiB seq write ×32 | 85,076 | 84,865 | 53,165 | 51,287 | **84,239** | 99 % | 60→82 |
| 4k randrw 50/50 ×32 (one file read AND written per job) | 3,044,570 (8.8) | — | — | — | **3,006,371** (8.9) | 99 % | 54→82 |
| reads this build: 4k QD1 / ×32, 64k, 1 MiB | 268 k / 3.05 M, 79.2, 98.3 | | | | 259 k / 3.09 M, 81.0, 98.1 | 97–102 % | |

Host CPU per 4k write op at ×32: ext4 7.9 µs, direct 7.8 µs (23.7 cores ÷ 3.03 M). The mixed row's
44 lease grants over 32 fds are the read asks turning over into write leases once each (12 read
asks before the first write landed); no lease was recalled: a client's own direct reads and writes
move nothing in the daemon. (A first run of this session read 2.3–2.4 M across every row with the
host's 1-min load at 28 from another agent's compiles — the thermal rule now also gates on load.)

**Verdict.** With the write arm every data row the interposer serves — 4k random reads and writes
at QD1 and ×32, 64k, 1 MiB sequential both ways, mixed read/write — is **on par with ext4 and xfs**
(97–102 % of ext4; above xfs on every one), at ext4's CPU per op, with the daemon idle. The owner's
bar for the "fastest" gate is met on the shapes the arm serves; what stays on the ring is by law, not
cost: extending writes (the size-neutral law), holes, files not resident-striped-passthrough,
unaligned or multi-block requests, and every write of a file another writer admitted on since the
lease (a kernel-path writer evicts direct writers per write by design).

**Not verified; risks.** The forced-recall path end to end (only the unit rig forces it: a client
wedged for a second mid-`pwrite` is not a shape we could produce — the kill rows show a dying
client's syscall completes); the uid-refusal arm for writes (the rule's rows only); real NVMe and a
volatile write cache (the cut row is brd under dm-log-writes: the fdatasync-is-a-FLUSH law is
exercised, a device's own cache is not); a kernel-path writer and a direct writer on ONE file
thrash the lease per kernel write (by design; no row measures it); the unsent partial note at a
`close` whose ctl lock is held loses ≤ 63 writes' dirty mark (never a byte); the daemon's own
`PATCH_MAX` (CoW above block/8) does not bound a direct write — a cut mid-write tears at the
device's sector grain, as ext4; security posture unchanged in kind: the write bit on the device
nodes extends the whole-device trust a direct reader already holds.
