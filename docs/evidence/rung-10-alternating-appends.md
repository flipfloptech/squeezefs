# Rung 10 — alternating appends, unservable foreign bindings, the full-store wall's line (2026-10-24)

**1. Alternating appends lost data.** A holder's extending striped write PARKS its size claim and its
custody (§L.6/§AL.5): nothing commits, so no pass recalls a member's grant. B `>>` (its own publish
recalls B), B `stat` (a fresh grant), A `>>` (parked), B `>>` — B took the stale EOF and wrote over A's
line. **The law (charter §5.5 amended): REGISTER-THEN-CHECK.** The token holder counts its registered
objects in a filter (`Holder.watch`, a cell per object of the cap; `mayHold` is one relaxed load, exact
when 0; `held` confirms under the lock). A holder's write that parked a claim reads it AFTER the park
released the pending stripe's lock; a member's grant lands its custody and journals its claim before the
ack (`KvFs.landIfWatched`, `fuse_write_watched_landings`), and the journal's pass recalls the grant. A grant
checks for a claim parked since its prepare AFTER it registered, under that stripe lock
(`PrepareSeam.pending` = `KvFs.claimParked`, never the map's relaxed count; `dlm_token_grant_late_landings`)
and lands it registered (its own pass recalls it; the reader installs nothing under the generation law
and re-asks). Model `tokens/extension_watch_model.zig` (explicit store buffers, a release drains): holds;
**check-then-register fails, the check off the lock fails**.

**3. Resolve errors read as holes.** A foreign layout or block-map window the token could not serve came
back from the seams as null — a hole: a read served zeros, and a write seeded its block from it (its bytes
then sat in custody its foreign tree-mapped publish refuses). **The error channel:** the failure bumps the
ino's resolve-fault epoch before the seam returns (`DataPlane.resolve_faults`, 256 rows); a read that
resolved under a moved row answers EIO (`foreign_resolve_eio_reads`), a write piece whose lookup did
refuses EIO before any custody (`MapSeam.resolveFault`, `write_resolve_fault_refusals`).

**4.** The full-store wall said its ENOSPC per refused write (844 lines in a subset): latched once per
episode (`full_wall_said`, closed by the gate's next capacity), every refusal counted.

| red test | origin/dev (8bb2f94) | branch |
|---|---|---|
| smoke ALTERNATING APPENDS … supervised | `log` 5,243,024 B mirror vs B 5,242,970 / A 5,242,979; `small` 150 vs 87 / 96 | byte-exact both mounts, after remount; 16 watched landings |
| smoke ALTERNATING APPENDS … foreground | `log` likewise; `small` 150 vs 78 / 87 | byte-exact; 16 watched landings |
| smoke A FOREIGN BLOCK MAP THE TOKEN CANNOT SERVE (seam `SQUEEZEFS_TEST_FOREIGN_MAP_FAIL`) | B's read rc 0 (holes as zeros); B's write acked | both EIO; A's file intact; fsck 0 |
| unit write_admit "THE RESOLVE FAULT" | — (no channel) | EIO, no custody; then seeded from the real block |
| unit write_admit "the wall … says ENOSPC ONCE per episode" | 2 error lines | 1 |
| model extension_watch | — | holds; 2 weakenings fail |

Under load: three concurrent loops × 7 = 21/21 runs of the three rows (63 row runs) beside other agents' builds; the cross-owner and two-mount rows (the 61-row set, the block-map reader, the access stamp) 63/63; unit filters 477/477; rails 32/32. xfstests generic/409 732 013 127 299: Passed all 5, fsck 0 findings on both sets, no tripwire; generic/299's filled scratch said the wall's ENOSPC in 18 lines — 18 episodes, each closed by returned capacity (844 lines before).

**Owed.** (2) A truncate racing a joiner's claim: `max(size, claim)` regrows over a cut its publish
predates. The design: the grant page carries the holder's size epoch, read BEFORE the records; the
joiner's write captures it before its probe and its claim carries it; the holder refuses a claim under an
older epoch and runs its size path at its current size (clipping blocks the late publish re-bound). The
hot-path A-B-B-A (`sw1m-q16j4`, `rw4k-q32j4`) of the extending write's one check: owed to a quiet host.
