# An fsync barriers only the metadata members its file depends on (2026-10-28)

**What changed** (decision 0003 §CH; charter §5.2, §5.3 amended). `MetaSet.fsync` looped every member's
trailing barrier, one after another, for every fsync/fdatasync/fsyncdir/syncfs and cross-owner intent.
Now each member's conveyor keeps `fsync_marks.Marks`: per inode stripe (W = 2^16 words) the padded end of
the last window an acked entry naming the inode was reserved in — records keyed by it, dentry `Put`s naming
it — stamped by the committer before its commit returns; plus `chain`, the last window holding a directory's
dentry `Put`, stamped by the pass. `MetaSet.fsyncIno` (kv_fs's fsync and the data plane's meta leg) asks a
member only when `max(stripe, chain, overflow)` is above the lane's durable floor, and asks every such member
before waiting on any. The lane's covering request (`requestBarrierCovering`) is answered at its admission
when the durable floor covers it, and JOINS the barrier in flight when that barrier's floor does. fsyncdir,
syncfs, the intents' barriers and the free hold's drain ask every member at once for everything it reserved,
skipping the durable ones. `SQUEEZEFS_META_FSYNC_SCOPE=set` is the control arm (the former serial law, its
requests classified). Counters: `meta.fsync.*` (`meta_fsync_{ino,set}_calls`, `_member_requests`,
`_member_skips`, `_requests_{own,chain,none}`, `_marks_overflows`, `meta_fsync_{ino,set}_ns`), per member
`volumes[i].fsync` (`lane_barriers`, `requests*`, `skips`, `joins`, `durable_at_admit`, `durable_floor`),
`journal_lane_fsync_{joins,durable_at_admit}`, `fuse_fsyncs` / `fuse_fsync_ns`.

**Why.** With §BZ every member carries load, so a file's fsync paid N serial device round trips where its
file's entries lay on one member. A transaction lands on one member; a span is an intent barriered on every
member before its op returns, so what an fsync can still owe is on the members its marks name.

**The red tests.** `meta_set` "an fsync of one inode barriers only the members holding its un-durable
entries": control arm +1 +1 per member for a file whose entry is on member 0; the rule +1 +0, then +0 +0,
member 1 alone after a directory made there, every-member +1 +1 then +0 +0. `journal_lane` "§CH: a covering
fsync JOINS the barrier in flight …": red with the join removed (`fsync_joins` 0). Smoke "AN fsync BARRIERS
ONLY THE MEMBERS ITS FILE DEPENDS ON, AND EVERY fsync'd FILE SURVIVES A POWER CUT": two members over one
dm-delay + dm-flakey device (`DmErrorSet.upWith(rig, 2)`); same-member files, root files whose dentry is on
member 0 and inode on member 1 (cross-owner creates), a renamed parent, a hard link, a new subdirectory on
member 1; then at 400 ms per metadata write/flush a file per member and a parent rename, cut at once. Red
with fsyncIno asking no member (`b/last`, `a/p3/g` lost) and with `chain` unstamped (`a/p3/g` lost). Model
`fsync_join_model.FsyncJoin`: 87 states hold; `join_by_ticket`, `skip_on_landed`, `stamp_after_return`
each fail `InvariantViolated`.

**The counters** (correctness runs, not rates: `~/tmp/fsync-barriers/rig.sh`, loop images under /build/tmp,
supervised mount, 2 OSTs, 10 s; fio psync 4 KiB randwrite `--fsync=1`/`--fdatasync=1` one job per top-level
directory, 2 × MDT jobs; `cwf` = create + 4 KiB write + fsync + close loops, a thread per directory; mdtest
`-F -n 400 -w 4096 -y -u`; `syncfs` = writers in every directory beside a syncfs loop. Lane barriers per
FUSE fsync (syncfs: per syncfs). dev = `7693bab` on a quiet host; control/rule = this branch's two arms back
to back under other agents' build load, so latencies compare only control vs rule; fsck clean, 0 error
lines, 0 tripwires on every run):

| workload | MDT | dev | control | rule | control: requests to a member holding nothing of the file | `fuse_fsync_ns` mean control → rule |
|---|---|---|---|---|---|---|
| fio fsync | 2 / 4 | 1.81 / 3.43 | 2.19 / 4.42 | **0.18 / 0.25** | 88 % / 94 % | 427 → 94 µs / 1,199 → 182 µs |
| fio fdatasync | 2 / 4 | 1.89 / 3.43 | 2.05 / 4.49 | **0.23 / 0.22** | 89 % / 93 % | 327 → 157 µs / 1,781 → 112 µs |
| create+write+fsync | 2 / 4 | 1.72 / 2.82 | 1.59 / 2.64 | **0.75 / 0.81** | 67 % / 89 % | 523 → 247 µs / 1,337 → 260 µs |
| mdtest -y | 2 / 4 | 1.73 / 2.64 | 1.54 / 2.62 | **0.81 / 0.89** | 66 % / 87 % | 468 → 323 µs / 1,548 → 378 µs |
| syncfs | 2 / 4 | 2.00 / 4.00 | 2.00 / 4.00 | 1.98 / 3.93 | — (every member holds writes) | syncfs meta leg 812 → 678 µs / 1,736 → 1,265 µs |

Under the rule `requests_none` is 0 everywhere; the fio rows' data-path meta leg (`fsync_phase_ns.meta_barrier`)
is a skip (mean ≤ 1.3 µs): an in-place overwrite owes no metadata, and only the times drain's Δtime (≈ 1 in 5
fsyncs) asks the file's member. Joins: 228 of 26,128 requests on fio, 1,267 of 22,698 on create+write+fsync.
`chain` requests and mark overflows 0 in every run (no workload names directories beside its fsyncs; the
smoke row's renamed parent is the chain's one asker).

**Checks.** See the report of the landing commit (unit filters, rails, fmt, smoke rows, xfstests).

**The A-B-B-A row for the owner.** `~/tmp/fsync-barriers/abba-fsync.fio`: psync 4 KiB randwrite, `fsync=1`,
8 jobs each in its own top-level directory `d0 … d7` (256 MiB files laid down first, 60 s time-based); arms
dev vs the branch, or `SQUEEZEFS_META_FSYNC_SCOPE=set` vs `ino` in one binary; the fdatasync form with
`--fdatasync=1 --fsync=0`; on 2 and 4 MDTs.

**Seen, not changed.** (a) fdatasync journals the size-neutral mtime stamp as fsync does (`kv_fs.fsync`
ignores `datasync`) — skipping it would leave most overwrite fdatasyncs with no metadata barrier: the owner's
call. (b) One FUSE fsync runs 1.2–1.4 per-inode meta rounds on fio (the epoch save's, then the times
drain's, §AL.5's order).
