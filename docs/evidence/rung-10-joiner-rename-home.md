# Rung 10 — a joiner's intent whose step 0 is foreign was written into the MANAGER's slot (F8)

**Seen.** `rung-10-adjudication-audit.md` §2.4: xfstests generic/732 under the shipped symmetric
default (`SUITE_SINGLE_WRITER=0`). The second `_scratch_mount` JOINED; `mkdir A B; touch A/f; mv
A/f B/` through mount 1 and `cat B/f` through mount 2 passed; `mv mount2/B/f mount2/A/` answered
EIO after 90 s — the joiner logged `move gate: rename met a slot move that did not resolve in
90000 ms (last answer SlotMoved)`. Reproduced by hand on a two-daemon pair (`f1d0d21`'s message has
the counters): 513 move-gate re-issues in 20 s, 513 refusals at the JOINER's own door, 0 steps
shipped, 0 intents minted, 0 `ResolveSlot` calls, 0 handovers, 0 offers.

**Root cause.** Not a handover and not the move gate. A rename whose every object the manager holds
has a FOREIGN step 0, so the executor writes the intent ALONE "into the mount's own rotor slot"
(`xv/ops.zig` `executeInner`, charter §5.5) and took that slot from the resolver's `localSlots`,
whose first entry `xv/set_bind.zig` hard-wired to slot 0 — volume 0's native slot, the MANAGER's.
On a joiner the commit met its own door's foreign-lease verdict (`SlotBusy` → `SlotMoved`),
`leaveOpen` forgot the never-durable record, the gate parked on an epoch nothing would bump, re-polled
and re-issued the identical commit until `T_park_max`. The manager's renames into the joiner's
directories pass (slot 0 is its own); a create or mkdir from the joiner passes (step 0 is the
child's record in the joiner's rotor, the rider rides it). The mirror case's 10 s hang in the hand
repro was the kernel's `s_vfs_rename_mutex` held by the parked rename — one mechanism.

**Fix (`f1d0d21`).** `ops.Resolver.ownHome`: the home of an intent whose step 0 is foreign is a slot
this mount LEASES — the manager's native slot 0, a joiner's own rotor slot
(`Plane.ownRotorSlot` = `LeaseTable.firstRotorHeldBy(me)`, the lowest rotor lease not releasing; a
lazy grant's tree is minted by the commit). Null (a joiner between its grants) defers the op; it is
never written into another appender's slot. `localSlots` lists the roll-forward homes only — local
slots with a tree — so a joiner no longer scans the manager's slot 0 for intents to adopt at its
mount (a latent second hazard of the same line). No lock-free core changed: no model.

**Red → green.**
- Unit, `handover_moves` "A JOINER'S RENAME WHOSE EVERY OBJECT IS THE MANAGER'S": the in-process
  two-member rig behind the product's move gate; generic/732's shape through B, back through A,
  B again; the mirror (B's directories and file, A moving it, B back); each member's file between the
  other's directories both ways; each rename ≤ 2 s; the joiner's `ownHome` its own rotor lease and
  never 0, its roll-forward homes its own; minted ≡ retired as deltas; both members read one tree.
  Red on dev at the first assertion (the home was slot 0).
- Smoke, "A RENAME THROUGH A JOINED MOUNT INTO THE MANAGER'S DIRECTORY RESOLVES ITS SLOT MOVE"
  (`daaf506`): two daemons, 12 renames (11 cross-owner) each under `timeout 2 s`, the tree sha-equal
  and nlink-exact through both mounts, `df -i` equal, minted ≡ retired on both, umount B then A, fsck
  clean. Red on dev: `rename B/f A/ did not resolve within 2 s`, the daemon's 48 s deadline line (at
  the smoke's T_owner). Green: the slowest rename 6 ms alone, 5–14 ms in 20/20 runs beside the full
  smoke tier (28/28 steps, 117/121, 4 venue skips).

**Numbers.** Unit filters `sym` 420/425 (5 skips), `handover` 68/70 (2 skips), `xv` 85/85,
`move_gate` 41/41; rails 32/32; `zig fmt --check` clean.
