# Rung 10 — `squeezefs umount` of a BUSY mount: refuse loud, serve on; `--lazy` serves to the end (2026-10-12)

The defect, the contract (decision 0003 §S; 0002 row 499), the before/after on the venue, the rows that
pin it. Landed red-first on `umount` (worktree `.wt-umount`), both gates green at the landing.

## 1. The defect (reproduced on the venue image with `scripts/dev_mount.sh`, then by hand)

An orderly `squeezefs umount <mnt>` while ANOTHER process holds a reference inside the mount (a shell
whose cwd is the mount root, an open fd), every time:

```
stop requested (SIGTERM/SIGINT): unmounting /build/tmp/repro-umount-1486537/mnt
transport: umount2(…/mnt) failed (BUSY); detaching lazily                                   (+2 s)
supervisor: daemon 1486561 missed a probe (5 s unresponsive; grace window 30 s)             … ×5
supervisor: daemon 1486561 unresponsive for 30 s (no answer to stat of the mount root) — dumping state, then SIGABRT → SIGKILL
supervisor: daemon 1486561 is dead (wait status 0x6)
umount rc=0 after 29.005 s
```

What happened, in order: the daemon's `Session.close` drained (every retained slot released, every owed
reply synthesized), then met EBUSY on its plain `umount2` for the 2 s window, then detached lazily and
wrote fusectl's `abort` — a silent no-op on the venue, where `/sys/fs/fuse/connections` is an empty
directory (fusectl is not mounted). The holder's cwd kept the superblock and the connection alive; the
workers sat in their 30 s residency answering every fresh delivery ENOTCONN — a daemon that had stopped
serving a connection it still owned (charter §1.1: a wedge). The supervisor's probe, a `stat` of
`<mnt>/.stats`, now resolved under the mountpoint directory (ENOENT): six misses, SIGABRT at 30 s — on a
healthy, defined state (detached but referenced). The verb polled "alive?" and, finding the daemon dead
and the path no longer a FUSE mount, returned 0.

## 2. The contract (0003 §S)

**Default — refuse loud.** `squeezefs umount <mnt>` runs the holder census (`src/cli/holders.zig`) before
it signals: every process's `cwd`/`root`/`exe` through `statx(STATX_MNT_ID, AT_STATX_DONT_SYNC)` on the
`/proc` magic links, every fd through `fdinfo`'s `mnt_id`, mappings through `maps` by device number,
mounts stacked inside by parent id — keyed by the kernel's MOUNT ID (`src/core/mountinfo.zig`), never by
path text. A held mount answers `error.Busy` (EBUSY through the ONE table) with the holders named, exit
1, nothing signalled; the mount keeps serving. The verb `chdir`s to `/` first so it is never its own
holder. The daemon runs the same census when SIGTERM/SIGINT lands (a raw `kill -TERM` meets it too) and
refuses the same way — logged with the holders, `transport_umount_busy_refusals` counted, serving on;
the verb reads that counter back through `.stats` so a holder that arrived between the two censuses is
still an EBUSY with a name.

**`--lazy` / `-l` — detach and serve to the end.** SIGUSR2: an unheld mount stops orderly (drain-first,
the plain `umount2`); a held one is detached NOW (`Session.detachLazily`: `MNT_DETACH`, `fusermount3
-u -z` unprivileged) and the daemon serves every request on the detached superblock until the kernel
ends the connection (the last holder leaves → the superblock dies → the sideband's `/dev/fuse` read
answers the disconnect → `on_shutdown` → the teardown: staged promotion, final checkpoint, claim
release). The verb returns at once naming the holders it leaves and waits for the exit only when there
are none.

**The verb's verdict is order-based on kernel facts** — the mount's id at the path, the daemon's
liveness, the census — never 0 after a kill: the entry gone while the daemon lived (or the daemon gone
with the mount detached and no holder left on its id) is its own unmount; the daemon gone with the
mount still attached, or with holders left on a lazily detached mount, is a death (exit 1, the dead
connection freed externally so it does not sit at the path as ENOTCONN).

**The supervisor probes by the mount's state.** The control-inode stat while the mount's id is in the
table at the mountpoint; the daemon's heartbeat once it is not — a `MAP_SHARED|MAP_ANONYMOUS` page the
intermediate maps before forking the daemon, stamped at `ready` and by every watchdog tick, stale after
two ticks, the same 30 s law to escalation. The watchdog runs to the end of the teardown now, so a long
dismount pass is never a wedge. The escalation ladder is unchanged for a real wedge.

**The race-case exit.** An EBUSY that outlives the 2 s window after the drain (a holder inside the
drain's milliseconds) is named in the log, detached lazily, and the workers' residency ends at the
detach (`Shared.detached`) — the straggler's next request answers ENOTCONN while the workers live and
ECONNABORTED at the exit; the daemon finishes in its normal time. The session's `umount2` by path first
proves the mount at the path is still its own (the id captured at `mount(2)`): a successor mounted at the
same path after an external detach is left alone.

## 3. After: the same repro on the venue (`scripts/.repro-umount-busy.sh`, the fixed binary)

Cycle 1 — the default, a `sleep 300` whose cwd is the mount root:

```
squeezefs umount: …/mnt is busy (1 holder(s): pid 1990257 (sleep) cwd) — BUSY (Busy): nothing unmounted, the mount keeps serving; retry when they are gone, or `squeezefs umount --lazy …/mnt` to detach now and let the daemon serve them to the end
umount rc=1 after .0057 s
stat .stats: …/mnt/.stats ok
write ok
mounted: 1
umount (holder gone) rc=0 after 1.413 s
mounted: 0
  stop requested (SIGTERM/SIGINT): unmounting …/mnt
  teardown: session closed; open_count_stranded=0 transport_requests_abandoned=0 tick_rescues=0/0
  meta volume …/meta.img: clean unmount (checkpoint seq 5, tail 11192, head 11192, healthy)
  supervisor: daemon 1990220 exited (wait status 0x0); done
```

Cycle 2 — `--lazy`, a holder that keeps reading its cwd (`ls` + `.stats`, 60 reads at 100 ms):

```
squeezefs umount: …/mnt detached; daemon 1990295 serves the 2 holder(s): pid 1990332 (bash) cwd, pid 1990361 (sleep) cwd until the last leaves, then tears down (its supervisor logs the exit)
umount --lazy rc=0 after .0335 s
mounted at path: 0
holder: 60 reads on the detached mount, all served
  lazy stop requested (SIGUSR2): …/mnt detached lazily with 2 holder(s): pid 1990332 (bash) cwd, pid 1990361 (sleep) cwd — serving them until the kernel ends the connection, then tearing down
  supervisor: …/mnt is no longer attached (the daemon's own unmount in progress, or a lazy detach serving its holders): liveness by the heartbeat from here
  the FUSE session ended (unmounted): tearing down
  meta volume …/meta.img: clean unmount (checkpoint seq 11, tail 14752, head 14752, healthy)
  supervisor: daemon 1990295 exited (wait status 0x0); done
daemon processes left: 0
```

No `detaching lazily` from the teardown, no `missed a probe`, no `unresponsive`, no `is dead`.

## 4. The rows

| Row | Where | What it pins |
|---|---|---|
| `smoke: umount of a BUSY mount (another process's cwd inside it) — …` | `src/cli/smoke.zig` | red on `33c2ee2` ("rc exited with code 0 after 30011 ms (expected exit 1 within a tick)"); green: exit 1 ≤ 5 s naming the holder's pid and `cwd`; `.stats` and a write served after; a bare SIGTERM refused, `transport_umount_busy_refusals` = 1; the supervisor silent; the holder leaves → `clean unmount`, `exited (wait status 0x0); done`, no mount, no guard |
| `smoke: \`umount --lazy\` of a BUSY mount — …` | `src/cli/smoke.zig` | the verb returns ≤ 5 s naming the holder; the path leaves the table; the holder's read count keeps growing on the detached superblock; `is no longer attached … heartbeat` logged, nothing else from the supervisor; the holder leaves → `the FUSE session ended (unmounted)`, `clean unmount`, `exited (wait status 0x0)`; `-l` on an unheld mount = `lazy stop requested (SIGUSR2): unmounting`, no lazy detach |
| the census over a real mount | `src/cli/holders.zig` | a tmpfs: a child's cwd found by cwd, our fd by fd, nobody after the child leaves, `umount2` EBUSY ↔ success agree with the census |
| a child holding ONLY a mapping (2026-10-18) | `src/cli/holders.zig` | red on the rung's code: `umount2` EBUSY, census `0` — `readFileAlloc` sizes a file by stat and a proc file reports size 0, so `/proc/<pid>/maps` read as EMPTY and the `mmap` arm never matched; a process holding only a mapping was invisible. Green: `maps` read to EOF (`readProcFileAlloc`), the child named `pid N (…) mmap`; past `MAPS_READ_LIMIT` a warning names the pid (never a silent miss). Every other proc read in the verb was already a fixed-buffer `readFile` |
| a tmpfs held by a child's cwd | `src/cli/umount.zig` | the verb's `Watched` census refuses by name; the refusal line reads `… (1 holder(s): pid N (…) cwd) — BUSY (Busy)` |
| the heartbeat machine | `src/cli/supervisor.zig` | fresh beats keep a detached daemon healthy; stopped beats escalate on the 30 s law; `probeKind` follows attachment; the first stamp is at ready (the lazy row found the 0 page: `missed a probe (the heartbeat; 5 s unresponsive)` when the detach came before the first tick) |
| the grammar | `src/cli/args.zig` | `umount [--lazy\|-l] <mountpoint>` |
| the verb's pure readers | `src/cli/umount.zig` | the pid, the refusal count (0 when absent), the build commit out of `.stats`; the exit wait = residency + one tick; the `.stats` poll rides the drain's lease budget |

**Twenty runs of both rows in a loop on the venue** (another agent's gate and smoke running alongside,
load 2–4): see §5.

## 5. The 20× loop

`for i in 1..20: zig build smoke -Dtest-filter="umount --lazy" -Dtest-filter="umount of a BUSY"` on the
venue, two other agents' builds and smokes running alongside (load 3.2 → 0.5): **20/20 green**, 20–21 s per
pass of both rows (runs 1–3 at 192–217 s include a rebuild — the tree was being edited under the loop).
Nothing in the rows waits on a timer: every wait is a bounded poll on a kernel fact (the mount table, a
pid's state, the holder's read count) or a log line the daemon or its supervisor writes.

The guard added last (`--lazy` only to a daemon of this build: SIGUSR2's default action terminates a
process without our handler, so the verb compares `.stats`' `build.commit` with its own first) rode runs
3–20.

## 6. What the full gate found (and the rows it changed)

- The two census unit rows passed alone and failed inside the full unit binary: their forked holder
  (fork without exec) inherited `PR_SET_DUMPABLE 0` from an earlier test (`transform`'s key hardening),
  and the magic links of a non-dumpable task need `CAP_SYS_PTRACE` — which the venue container does not
  grant root (`CapEff a82465fb`, bit 19 clear). The rows make the child dumpable; the census doc states
  the limit (0003 §S "Not walked"). On a host, root has the capability; the smoke's holders (`sleep`,
  `sh`) are exec'd and dumpable either way.
- `smoke: THE INTERCEPTION HOST` ran `umount` while its own process held `ring.dat` (and four screen fds)
  open on the mount — a holder under the new law. The row now asserts the refusal by name (`pid <self>
  (squeezefs-smoke) fd N`, exit 1, the mount still there), closes every fd, and only then runs the
  umount "with the live session" (the socket and its bindings, which the host poisons at its disarm).
- `smoke: ONLINE FSCK THROUGH THE ADMIN LANE` (landed on `dev` by the jobs rung during this work) failed
  once in the first full gate with a C9 "stated gap" finding on the two-daemon set (`invariant_tripwires`
  1) and passed on the re-run and in the second gate: not touched by this rung, noted for its owner.

## 7. Owed

- The census does not walk a per-thread cwd/root (`unshare(CLONE_FS)`) nor another mount namespace;
  mappings of a bind of the same superblock elsewhere match by device (an over-count the operator's retry
  or `--lazy` absorbs). The kernel's `umount2` stays the authority on "busy".
- The supervisor's heartbeat proves the meta lane schedules; a FUSE-path wedge on a DETACHED mount is the
  daemon's deadline watchdog's to log — no probe can reach a detached superblock from outside.
- A wedged-daemon smoke for the escalation ladder itself still needs a `SQUEEZEFS_TEST_*` park seam in
  the handler lane (`rung-03-fuse.md` §owed); the ladder is unchanged by this rung and tested as the
  pure machine.
