# Rung 10 — a joined member's write to another member's file keeps its bytes (2026-10-24)

**The report** (the re-join agent): two mounts of one symmetric set, A the manager, B joined; A writes
`own/f`; B runs `echo x >> own/f` — the size grows, the appended bytes read back as zeros through B and
through A, and stay zeros after both remount (fsck clean: the zeros are written bytes, not a hole).

**Three mechanisms** (none of them a failed foreign layout lookup read as a hole):

1. **The holder served a write's size claim as a truncate.** A foreign-slot write lands its bytes on a
   block of the writer's grant, ships the layout publish (bytes and binding reach the holder together),
   then ships its size as `setattr{size}`. The holder ran every shipped size through its size path
   (decision 0003 §AL.6: `servedTruncate` → `setattr` → a striped grow's `zeroExtension(old, new)`), so the
   extension law wrote zeros over `[old size, new size)` — exactly the bytes the publish had just bound.
   Both postures; every append, every write past EOF, every append across a block boundary.
2. **A grant carried the stored record, not the holder's acked state.** The page is built from the
   store: a striped write's size claim parks (§L.6/§AL.5) and its custody stays in RAM until `T_wb`, and
   the grant prepare promoted only the write cache's residents. B's `>>` on a file A had just written
   (unsynced) took its EOF from the stale size, wrote over A's bytes, and its claim — served as a
   truncate (1.) — CUT A's file to B's end: foreground, A's 100 KiB file read back as B's 34 B and A's
   5 MiB file as B's 3,000 B; supervised, A's 5 MiB file as 4 MiB + 3,000 B.
3. **A displaced open-pack tenant drifted the holder's pin.** B's writes promote A's packed files; the
   displaced tenants' frees ship to A, whose served free ran the plain ladder without the packer's
   `released + 1` — `pack_pin_refcount_drift` fired at A's dismount seal (supervised arm).

**The laws** (charter §5.5 amended). THE CLAIM LAW: a foreign write's size is a CLAIM — wire bit
`SETATTR_SIZE_CLAIM` (`verbs.zig`, with `SETATTR_SIZE`; the record head now screens its setattr word):
the holder raises its size to `max(size, claim)` in the verb's entry and never runs its size path
(`record_ship.apply`, `record_size_claims`); a truncate still runs it. THE GRANT'S LANDING: before a
foreign grant's page is built the holder promotes its residents, lands every other write custody of the
object and journals its parked claim behind the data barrier (`KvFs.landForGrant`, `fuse_grant_landings`,
`fuse_grant_landing_drain_failures` 0). A shipped free of a tenant of the holder's OPEN pack leaves through
the packer (`DataPlane.releaseShippedOpenTenant`), as a local free does.

| red test | origin/dev (0661074) | branch |
|---|---|---|
| unit two_holders "THE CLAIM LAW" | — (no claim word) | size max'd, face never run, a lower claim moves nothing |
| unit verbs "cross-owner payloads" | an unknown setattr bit passes the record head | screened (and a claim without a size) |
| smoke "A JOINED MEMBER'S APPEND … BYTE-EXACT — supervised" | f1 f2 f3 f6 f7 zeros at B's bytes; f8 5,245,496 → 4,197,304 B | byte-exact at once and after both remount, fsck 0 |
| smoke "… — foreground" | f1 f2 f3 f6 zeros; f7 102,400 → 34 B; f8 5,245,496 → 3,000 B | byte-exact, fsck 0 |
| the same rows judging both logs (d4a5281) | `pack_pin_refcount_drift` on A at the seal | no tripwire on either member |

Under load: three concurrent loops × 7 = 21/21 runs of both rows (42 row runs) green beside other agents' gates and an xfstests subset (load average 13–17). The cross-owner and two-mount rows (the two-daemon matrix and its MutualTls twin, invalidation coherence, rejoin, member scope, the joiner rename matrix, the sideband, the write cache's recall, the handover's parked stamps, the merged census, block re-use, the busy writer, KVDA, the grant ledger, fallocate's setid strips, the `-o ro` reader, the online volume, the kill rows (a)): 61/61. Hand repro: `~/tmp/foreign-append/repro.sh` (both postures).

**Owed — the window** (seen, not fixed): a holder's extending striped write PARKS its claim and custody
with no commit, so a foreign token issued before it stands un-recalled; the reader's next append takes
the stale EOF. Alternating appends (B `>>`, A `>>`, …, six rounds) lost 45 of B's 54 bytes on
`~/tmp/foreign-append/window.sh` with this branch. The law it needs: a holder's write that parks a size
claim on an object a foreign token stands on lands and journals it before the ack (its commit's pass
recalls the token) — a per-object registration filter in the token table (one relaxed load per extending
write; the pending map's lock as the paired fence against a grant's register-then-prepare), modelled.
