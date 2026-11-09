# Handoff — 2026-11-09

## Current state and next action

**`origin/dev` is `5b6eaab`.** PR-4's main path (member re-admission, held
journal resume, `umount --force`, data-plane `Held`, `classifyForData`,
`data_resolution[d]`, writer `awaitPast`, and the gate fixes that followed)
landed there after a green full venue gate: **75/75 steps, 3861 passed, 218
skipped, exit 0**. The exclusive class ran; both host-wide `sync` rows skipped
(`no_kernel_feature`: this 7.3 kernel does not send `FUSE_SYNCFS`, upstream
from 7.4).

**In flight:** branch `feat/pr4`, off `dev` at `5b6eaab`. It has not landed.

| Commit | What |
|---|---|
| `fbd7fe8` | C2 post-check on every joined volume |
| `02c5eb2` | `MetaReadmit` on the `uring_fs` pool waiter |

**Next work:** **DC22** — a withdrawal of a fresh registration drains this
process's in-flight commands, with a bounded wait, before unregistering
(decision 0004 §4.5.4). Start from `shouldWithdraw` / `runSuccessionRegister`
and the late-`cleared` refusal path.

Read `AGENTS.md`, `docs/charter.md`, and decision
`0004-metadata-wear-preempt-all.md` before changing core logic. PR-5
(metadata WEAR) stays behind PR-4. Product metadata is still WERO (type 3);
data namespaces already use WEAR (type 5).

## Resume

```
git checkout feat/pr4
git status -sb          # should be clean and tracking origin/feat/pr4
# then implement DC22
```

Landing `feat/pr4` is a full venue gate, then `git merge --ff-only` onto `dev`
in the main tree (`/home/justin/Source/squeezefs-zig`). The post-merge hook
publishes `origin/dev`. Do not commit directly on `dev`.

## What landed on `dev` in this session (PR-4 main path)

From `e2b91d9` to `5b6eaab`. Highlights:

- Member-fence classifier, census beat, succession re-register after DO.33 write-drain.
- Held journal writes and barriers resume (`awaitPast`, re-issue on `cleared` or `.retry`).
- KD20 dismount wait; `squeezefs umount --force` (admin verb `0x0707`).
- Data-plane `Held`: three-way verdict, `DmaError`/`mapDma` plumbing, writers wait on
  `data_resolution[d]`, a metadata `cleared` kicks the data beat.
- Gate fixes that were required to land: verb `0x0707` in the numbers rail; Held
  arity/switches; no second `shutdown` on a freed resolution word; `awaitDismount`
  stores `terminal` when nothing is in flight; census beat and succession park stay
  down while `SQUEEZEFS_TEST_MEMBERSHIP_BEAT_HOLD` is active, and the hold arms when
  its FIFO appears; a leaving member is not fenced because its page is already Free.

## What is on `feat/pr4` (not yet on `dev`)

- **C2 on every joined volume.** `currencyOnVolumes` / `liveOnEveryJoined`: Live
  under writer id, node token and term on every joined volume, before and after
  a succession re-register. A death recovered on one volume first does not let
  the host re-register on another that still reads it Live. An unreadable page
  is not a death. `MetaSet` installs `JoinedSet.peers` after open and after a
  successor publish.
- **`uring_fs` pool waiter `MetaReadmit`.** `writeAt` / `writeAtBatch` /
  `fdatasync` / `readAt` consult a hook bound for that path before fence
  classification. `held` and `retry` return `error.Held`. `latch` returns the
  original error. Nested consults are bypassed; `readAtRaw` skips the hook. A
  joined `DeviceLoader` binds its member-fence hook at open and unbinds on close.

## Still on PR-4, in this order

- **DC22.** Withdrawal drains in-flight commands, bounded wait, then unregister.
- **The coalesced re-check cache** per `(word, generation)`.
- **C1's grant-term half** on the successor page.
- **A data read that hits EIO during a drain** issues a data `drainBarrier` and
  retries the read once.
- **Fidelity F10:** a busy co-located joiner writing and fsyncing across a
  takeover, with no data latch (`data_pr_write_refusals` stays 0).

## Venue and rulings

- Local venue, stock Linux ≥ 7.3, no kernel patches. This host: Omarchy-flavoured
  7.3.1 (and stock `linux-mainline-sqz` as the second boot entry). `FUSE_SYNCFS`
  is 7.4; the two `[venue-exclusive]` host-wide `sync` smokes skip here.
- Scratch under `~/tmp/` (owner ruling), not `/tmp`.
- No KASAN fleet, no `SQUEEZEFS_TEST_NVMET_FATAL_TEARDOWN`, no upstream mail.
  The disposable-lab branch was a proposal only and has been deleted.
- Leftover local worktrees and feature branches from PR-2/PR-3/stats/fiemap/edge-split
  were deleted after their work was already on `dev` or deliberately not landed.
  Do not recreate them.

## Owner decisions still owed (unchanged)

- Install the fencer udev rule on this venue if not already
  (`scripts/dev/provision-local-venue --apply`).
- The nvmet PR-preempt upstream mail stays unsent unless the owner sends it.
- Production `multipath=Y` metadata fencing remains detection-only until the
  per-controller ioctl form or the upstream nvmet fix.
