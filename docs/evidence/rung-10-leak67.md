# rung 10 — leak67: an open pack minted on another volume than its key

**Symptom.** The smoke row "the data plane WITH a cache path" failed its `fsck --online` after `rm`
with `[C2] vol-… block N — leaked: tracked by the allocator, referenced by nothing`. It showed in 1 VM
train run on `e1c334f`. The same signature is in a run on `979d920`, which is older than
`flushBornBy`, so `flushBornBy` is not the cause.

**Root cause.** `promoteLocked` picks ONE volume (`rt.pickVolume`), transforms the image for it, and
keys the pack as `PackKey{ tag, scope }`. The refill's `packAllocatePlaced` ignored the key and asked
the router again. With two volumes and the round-robin band, key A's pack was minted on B. fsck's
open-pack face looked up `openPack({ block's volume, scope })`, got the other key's row, and confirmed
the pack's own pin as leaked. That pin is refcount 1 once the pack's tenants have left, so a failing run
showed one finding per volume. The data side of the same defect: an image transformed for A landed on
B. Today this is latent only. The set has one transform (`transformWrite` ignores the volume), so no
cheap red exists for it.

**Fix (`ba8e2a5`).** `pack.AllocSeam.allocate_placed(ctx, PackKey)` now returns `Mint{ offset,
incarnation }`. The seam mints on `k.vol_tag` and has no way to name another volume, so the
`OpenPack`'s volume IS the key's by construction. Every exemption face (fsck's runner, C8 at mount,
the mover) asks one `Packer.isOpenBlock(vol, base)`, which matches the pack's own placement
(`openPackAt`). The faces no longer go through `openPack`, which claims a row as a side effect. Both
seam callers already key by their one pick: the plane's promotion and `defrag --pack`'s prepare. The
porting spec's §5.4 records the detail. Nothing durable changed: no format change, no fsck contract
change.

**Coverage (`15d0857`).** The row's coverage depended on the venue's RAM. The staging ring is
`clamp(free/4, 64 MiB, budget/8)`. On kamrui's derived budget (a 61 GiB host) the ring was large,
nothing packed online, and the row went green without judging a pack. On the VM the free-space
share gave a 204–234 MiB ring (the train's logs), and the 400 MiB storm reached high water only
sometimes, hence the low rate. The row now pins `SQUEEZEFS_MEM_BUDGET_MB=1024` for its daemon, which
caps the ring at 128 MiB on every venue. A comptime check restates why: ring + small files < storm.
The row also asserts `layout_promoted_packed > 0` before `rm`.

**Red → green.**

| Check | Before the fix | After |
|---|---|---|
| unit "a packed promotion's pack opens on the volume the promotion picked …" (`c875b8e`) | `expected 10, found 11` (key 0xA's pack on 0xB) | green; the faces find the block on its own volume and not on the other |
| smoke row, kamrui, pinned budget | 2/2 red with the fix reverted, 2 C2 findings each (6 and 8 files packed online); the diagnostic repro was 14/14 | **20/20 green** (15 + 5 in two venue calls), 5–11 files packed online per run, 0 findings |
| unit filters `pack`, `fsck`, `mover`, `defrag`, `job` (kamrui) | — | green |

**Gate** (kamrui, `-Dfresh=true`, on `18e7e5a`): `53/53 steps succeeded; 2974/3057 tests passed (83
skipped)`. The row ran inside it with 9 files packed online and 0 findings.

**Found by reading (separate, not this row; not re-checked after `baf17ef`).**
- `write_epoch.Epochs.closeEpoch` on `error.NotFound` frees only the current shadows. An intermediate
  displaced key (`.epoch` origin) is dropped with its guard and leaks.
- `write_epoch.Epochs.supersede`: orphans past 8 are removed from `displaced` and never freed.
- `plane.foldInserts` drops displaced bindings past `max_displaced` silently. Reading found no caller
  that exceeds it.
- fsck's C8 `isOpenPackAny` asks every volume for a block index, so an open pack on B at index N also
  exempts A's block N. This is over-broad, not a miss.
- `RefillCtx.compute` leaks its fresh mint if `gpa.create(OpenPack)` fails (OOM only).
