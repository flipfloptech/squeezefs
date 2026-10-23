# Rung 10 — fallocate under fsx and fsstress: the stale edge size and the reservation under custody (2026-10-23; decision 0003 §BJ)

**The symptom.** The full xfstests `-g auto` run on `925cb1d` stopped at generic/127 (fsx `-q -l 262144
-o 65536 -S 191110531 -N 100000 -R -W`): READ BAD DATA — zeros from 0xf4e0 where data was written. 127
passed alone and in the hour-old subset. The TEST set's daemon had fired `rewrite_shadow_displaced_drift`
twice under generic/013's fsstress, 114 tests earlier.

**The reproduction.** Not the 2-hour run: fsx's own shape by hand. Seeds 12 and 21 fail within a minute
on a fresh mount, each time right after a COPY (copy_file_range → the kernel's splice fallback: plain
FUSE writes) at the offset the zeros start. With `-X` (verify after every op) the first bad verify
follows a TRUNCATE DOWN or a PUNCH, and the daemon's trace (temporary TRACE lines at the write
admission, the extension law, the landings, the handler's fallocate and setattr) shows the mechanism:

```
TRACE fallocate ino 2: mode 10 [244699, 254623) size 245522      ← fzero past EOF; the claimed size 245522
TRACE zeroExtension ino 2: [48793, 244699)                        ← !!! from the durable size 48793 (the last truncate's)
TRACE write ino 2: [48793, 244699) class entire_op file.size 48793
TRACE write ino 2: [244699, 245522) class entire_op file.size 48793  ← the edge's zero write, size 48793
```

The ZERO_RANGE's in-bounds edge `[244699, 245522)` went to `zeroRange` with the file size computed in the
striped arm AFTER `settleCustody` from the handler's ENTRY probe plus the parked claim — and the settle
(the times drain racing it) had just journaled the claim into the record and retired it, so the probe's
durable size (48793, the last TRUNCATE DOWN's) plus a claim of 0 was the "size". A write that names a
size below its offset runs the extension law: `zeroExtension [48793, 244699)` — every byte written since
the truncate (the COPYs, fsx's writes) zeroed. Exactly 127's "zeros where data was written, check HOLE
and EXTEND ops".

**The drift.** fsstress `-p 20 -r -n 1000` by hand fires the tripwire in round 1; the diagnostic line
(now beside the tripwire) names the displaced key: `vol … unwritten true` — a reservation — with 0 or 1
parked key. A whole-block write into a hole writes through and rides the rewrite epoch; the map has no
binding at that block until the save; a later fallocate of the range reserved the "hole"; the save
displaced the reservation unparked. (`pickDest` declines a takeover of a block the epoch binds, so the
reservation was only freed once — the law was broken, not the data.)

**The laws** (decision 0003 §BJ). (1) The striped arm reads the size AFTER the settle, off the record as
it stands (`claimedSize(local, readInode)`): a size read before a wait is stale by whatever the wait
journaled. (2) A reservation fills holes of the write-side AUTHORITY: a block a parked buffer holds, an
overlay record covers or an epoch shadows is bound by its landing and is left alone
(`fallocate_custody_skips`); ZERO_RANGE over it zeroes through the admission, never a flip the map cannot
see; the custody check precedes the reservation check.

**Red first.** kv_fs "a PUNCH/ZERO edge carries the file's size as it stands AFTER the settle" (the data
face's settle hook journals and retires the claim as the drain does): on dev the edge's write named size
0; fixed, 100 000. write_admit "a reservation leaves the write side's custody alone": dev reserved the
epoch-shadowed and the parked holes (the drift follows); fixed, two of four reserved, no drift, the bytes
the writes', ZERO_RANGE over a parked hole lands zeros.

**The fix** — `01159e6`.

**Counts.** fsx `-X` seeds 21 12 31 32 33 34 35 36: all pass (21 and 12 failed before). fsstress
`-p 20 -r` ×4 rounds: no tripwire, fsck clean. Filters `fuse.handler.kv_fs` (109), `data.write_admit`
(82), `data.write_epoch` (58), `data.accumulate`, `data.layout`, `fallocate` green; rails; fmt. The
new tests **100/100** under the venue's load; the smoke rows FALLOCATE — EVERY MODE'S LAW, the data plane WITH the
write cache, the cache-path overwrite and storm stages LOOPED, A STREAMING OVERWRITE NEVER WEDGES, THE ORDERED LAW'S
KILL ROW — all PASS; the xfstests subset `generic/012 013 127 075 091 112 263`: **Passed all 7** (1 `_notrun`), 0 alert
lines in every daemon log, fsck clean on both sets (the "did NOT pass" is the ledger's stale-row rule of a subset).

**Seen, not fixed.** `writeZeros` and `zeroExtension` ignore a short `written` (the admission answers
whole pieces today). The drift tripwire's `eql` match means a same-reference FLIP under a parked key
would read as drift; no path flips under a shadow now (the handler settles first, the reservation skips
a shadowed block) — a `sameRef` match would make the parked set robust to it, owner's call.
