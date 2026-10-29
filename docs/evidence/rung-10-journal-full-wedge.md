# Rung 10 — the journal-full wedge: not reproduced; the watchdog now names the metadata wait (2026-10-29; decision 0003 §CM)

The owner's A-B-B-A on `dev` `0778bb4` (run record `/var/lib/sqz-perf/runs/20261029T064343Z`: the dev arm's
second leg, `mpirun -np 32 mdtest -n 10000 -u -i 1 -w 4096 -e 4096`, a fresh 2 MDT + 2 OST RAM-namespace set,
dist binary, `--supervise`) wedged the daemon 7.4 s into the create phase; the supervisor SIGABRTed it 175 s
later. One of about six rw4k `-np 32` runs of that binary that day; the perf mount's whole log holds no other
`meta_backend_start` wedge of it. Evidence: `~/tmp/journal-wedge/` (the daemon log, the run record, mdtest's).

## What the evidence proves (`daemon-window.log`, the run record's fsck)

- **Every volume-0 rank, at one instant.** The 32 stuck ops are one per rank: the 16 ranks whose directory
  routes to volume 0 sit past `meta_backend_start` (7 creates, 7 inline first writes, uniques
  4025856–4026058) or `write_locked` (2); the 16 volume-1 ranks sit past `write_locked`/`handler_entry`.
  `lock_ticked_reregisters` climbs +38/+57 per 5 s: 19 ticked lock waiters whose 2 s phases agree within 1 s
  — they parked together at ≈ 231 s. Volume 1 is collateral (its writes queue on the (3.7)/guard stripes the
  stuck volume-0 ops hold); volume 1's checkpointer cycled on (seq 39 → 71 at its idle cadence).
- **Nothing of volume 0 moved after its checkpoint 39.** The supervisor's tail replay at the death: 0 windows,
  "nothing held" (no tail slot written: no window was between its turns and its ack); the fsck mount: ledger
  seq 39, 0 entries replayed at the tail. So no reservation landed after cycle 39 and no window was in
  flight: volume 0's committers were queued or before their queue, not inside a window.
- **Not the ring admission park.** That park iterates every 10 ms and logs at 30 s; no such line.
  `channel_ticked_reregisters` 0: no ticked channel wait found its condition met.
- **Not the §CK class.** The wedge outlived the kernel's connection abort at 376.9 s (every kernel-side
  sleep released) and the dump shows every lane in `io_cqring_wait`, no lane thread in a futex.

The wait itself is not in the evidence: no `.stats`, no thread or fiber stacks. By reading, every pass-side
wait is ticked or gated on counters that read 0 with nothing in flight; the waits that are neither (an
`Io.Mutex` — the conveyor's queue mutex, a Notify's interior —, an untimed completion) leave no counter.

## Reproduction (no wedge)

`~/tmp/journal-wedge/jwrig-ram.sh` (the owner's shape on `scripts/perf/ramnvme.sh` namespaces, a fresh
format per run, its own mountpoint, gdb's fiber walk armed at the first watchdog line) and `jwrig-seq.sh`
(the owner's leg: plain and rw4k at `-np 32` and `64`): about 390 runs, 0 wedges — the `0778bb4` dist binary
at the owner's budget (26,235 MiB) and quiet (45–60 k creates/s) or loaded (12–30 k), `nice -20`, pass claims
`never`; loop images; a chaos build parking the commit path's points at random (1/100 to 1/50,000, ≤ 50 ms);
a hold on the checkpoint cycle (0.4 s, 2.5 s); `-n 4000` four runs a mount; a continuous create/mkdir/rmdir
storm. Journal-full stalls 12–223 per run, every stall ended.

## The census (this change)

The watchdog's wedge census (`fuse/handler/watchdog.zig`, every 5 s while an op is overdue) logged the drain
workers alone. It now logs one `meta census` line per volume and the executor's lost-wake absorbers:

- the conveyor: the leader latch; the pass's site — `idle`, `running`, or the wait it parks in (`hold`,
  `window_gate`, `tail_gate`, `recall`, `admission` with its step `try_admit`/`kick`/`wait`, `yield`) and since
  when, entered on each wait's slow path only; the queue's `Io.Mutex` state; committers queued, inside `build` (the door, the heap admission;
  two sharded counters), parked; room waiters; windows and tail members in flight; parked answers; the ring's
  head and reusable mark; the registry's open count, completed and resident prefixes, and its OLDEST open
  reservation — a window (members, claimed prefix, pending, helper class) or an entry no window holds;
- stage B's phase, pending windows, prefix target and runs on the device; the SMO structural tail slot;
- the checkpoint side's step (`waiting`, `serial` — the SMO lock —, `decide`, `drain`, C2…C13) and since when,
  its last record's seq and tail;
- `exec_lane_tick_rescues`, `exec_driven_owner_queued_parks`.

The owner's wedge with this line reads its class at the first watchdog tick: a pass `running` behind the
leader latch with committers queued (a park no tick covers), a gate or the admission step with its age, or a
window or an untracked entry holding the prefixes. Unit: `meta.backend` "the wedge census (the watchdog's
per-volume line)…" (a quiet volume after a commit and a cycle: idle, nothing open).

Checks on the head rebased onto `b959c79`: `zig build fmt rails` 32/32; units `meta.backend`, `meta.conveyor`,
`meta.checkpoint`, `meta.journal_lane`, `meta.meta_set` 222/223 (1 skip), and before the rebase with
`meta.journal_tail`, `meta.node_cache` and the `_model` checks 413/414 (1 skip); smoke `300 CONCURRENT FSYNCS
AND 300 CONCURRENT CREATES`, `THE REPLAY PROOF`, the data plane with and without the write cache, `THE SHARED
HOT DIRECTORY ACROSS DAEMONS`, `THE KILL ROWS` (a)–(c): green. No lock-free core changed.

## Seen, not fixed

A mount reused for several mdtest runs slows run over run, the profile in `kv_fs.forgetParent` /
`rememberParent` (the `parents` memo map, tombstone-heavy after a run's removals) — §CM.5.
