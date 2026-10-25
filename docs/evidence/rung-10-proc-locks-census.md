# rung 10 — proc-locks-census: a `/proc/locks` census can skip a held lock

**The red (gate 137).** `meta.guard.proc` "an exclusive holder that is not this binary and
outlasts the bound … writerFlockHeld reads it as a writer" failed at `expect(held)`. The
readers' "is a writer here?" answered "no writer" while `flock -x <path> cat` held the lock
for the whole test.

**Mechanism.** `/proc/locks` is a seq_file over the per-CPU global lock list, walked in CPU
order with each CPU's list newest first. Each `read(2)` fills one page under `file_rwsem`
and then drops it. The next read restarts at the record INDEX it reached (`locks_start` →
`seq_hlist_start_percpu(…, *pos)`). So if a lock before that index is dropped between two
reads, every later record shifts down one and the record at the boundary is never printed.
The idle workstation's table is already past one page (5,132 B, two reads), so any lock
churn can tear it. `writerFlockHeld` re-read the census every 5 ms through its 5 s bound
(about 1,000 reads) and returned "no writer" on the first one that named nobody.
`flockExclusive`'s exiting wait had the same weakness: it read such a round as udev's, which
counted a shared wait and, past 5 s, refused BUSY early. Measured: a held flock was missed by
243 of 252,540 scans beside 4 `flock(1)` loops, and by 0 scans without churn.

**The law: a census names, it never clears.**
1. "No writer" is the kernel's word: a `LOCK_SH|LOCK_NB` on a fresh description, asked only
   when nothing is named, and dropped at once. A racing writer waits it out as a holder it
   cannot name and is never refused (decision 0002 row 295, amended).
2. A lock held through a description THIS process holds is named off its
   `/proc/self/fdinfo/<fd>` `lock:` lines. That covers its own live lock and the
   supervisor's replay hold. The lines print under the inode's lock-list lock, and
   `/proc/self/fd` is walked by fd number, so the naming is exact. "This process refuses at
   once" never rides the census. The cost per round is one `getdents64`, a `readlink` per fd,
   and an fdinfo read per path fd, on control paths only.
3. A round that names no exclusive locker keeps the last judgement. A round that saw no flock
   at all counts in no class (row 542, amended).

**Reds (deterministic).** The seam `test_seams.between_locks_pages` runs between a census's
first page and its next read. The rig `TornCensus` pins the holder's creator and 2.5 pages of
byte-range locks to one CPU, then drops the byte-range locks at the seam, so the holder's
record is skipped. A lock file keeps it to one rig per venue.
- `4d341f9`: `writerFlockHeld` read "no writer" while `flock(1)` held. The exiting wait
  counted a shared wait for the torn round.
- `86bb43e`: this process's live holder was waited a round (one shared wait), and
  `writerFlockHeld` also waited a round on it. A dead creator's lock on this process's
  description counted a shared wait. A first-round tear behind a supervisor-held description
  counted as udev's.

**Numbers (local, 7.3.0-1.7-mainline-sqz, 32 threads).**
- origin/dev `e5253be`, the gate-137 test alone, 4 in parallel: 1/200 failed beside 32 ×
  `yes`, and 16/200 beside 16 × `yes` + 2 `flock(1)` loops.
- The first fix: the gate-137 test alone, 8 in parallel, passed 500/500 in both shapes. The 55
  `meta.guard.proc` tests then gave 196/200 beside 16 + 2. The 4 failures were zero-wait
  assertions on a single census, which law 2 closes.
- Final: all 57 `meta.guard.proc` tests, 8 in parallel, passed 300/300 beside 16 × `yes` + 2
  `flock(1)` loops and 200/200 beside 24 + 4.
- Unit filters: guard 174/185, proc 118/133, flock 57/59; the skips are the smoke tier.
- Smoke rows: 47/47. They cover mount races umount (~300 ms waits, both arms), both BUSY rows,
  the `-o ro` reader of a live set, enable-symmetric MOUNTED, KILL ROWS (c), the D0 guard, and
  `--supervise --log-file`.
- Rails: 32/32. `zig fmt --check`: clean.
