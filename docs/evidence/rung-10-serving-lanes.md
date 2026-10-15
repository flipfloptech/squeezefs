# Rung 10 — a served verb never parks holding the session; the lease gauge is one exact sum

Owed by `rung-10-gate-blockers-2.md` §7 (decision 0003 §AM). Agent `movematrix`.

## 1. The move matrix's offered leg: B's unlink parked 218 s, then `HolderUnresolved`

**What the row does.** A offers its slot S to B; B's five ops run into A's directory while the
offer stands. Instrumented on the venue, B's unlink and rename answer `SlotMoved` every 0.5 ms
attempt for ~10 s (B's door refuses B's own intent rider in a slot A still leases) until B's
renewal carries the offer and B's member thread accepts (`AcquireSlot` on B's manager link).
That accept is SERVED by A on the session that also carries B's renewals, its re-resolutions and
its own first touches, and the serve runs the whole transfer: the step-0 times drain takes 4a
locks with the blocking `lockPlan`, the door drain is unbounded, the checkpoint cycles wait on a
loaded device.

**The cycle.** The listener served each session one frame at a time while the dialer multiplexes
up to `MUX_IN_FLIGHT` (64) calls of every thread of a daemon onto that one session. A serve
that parks therefore holds every other call of the peer behind it — B's renewals included — and
when what it waits for is B's progress (a stripe B's unlink parked at A, B's member thread
blocked in the accept, B parked once its renewals stop), the reply bound only turns the
deadlock into retries until the lattice's deadline: one move-gate attempt of 218 s, then
`HolderUnresolved`. §Q had removed the shape for one verb (the guard ask takes the TRY form); the
accept and every other parking serve still had it.

**The law (0003 §AM.1).** A served verb never parks holding the session's serving capacity: the
connection serves on leader/follower lanes (`src/wire/listener.zig` `Lanes`) — one lane reads,
copies the call, hands the reader role on and serves on its own buffers; replies are sealed and
written under the session's send lock (both MAC sequences stay per direction). Capacity =
`SERVE_LANES_MAX` = the dialer's `MUX_IN_FLIGHT`: every call the peer can have in flight has a
lane, so none waits behind another's serve; lanes are spawned only as concurrency asks (a
sequential peer costs two threads per session; `cluster_wire_serve_lanes` counts them). An idle
read timeout ends a session only while no serve is in flight. Bounding every park below the
reply bound was rejected (K bounded parks still sum past it), as were per-class lanes (the
accept and the unlink's own first touch are one class).

**Red → green.** `tcp_e2e_linux` "A SERVED VERB NEVER PARKS HOLDING THE SESSION": a call parked in
its handler; a `ping` on the same session. On `dev`: the ping answered `HolderUnreachable` after
the 10 s reply bound (`tcp_e2e_linux.zig:296`, both calls' "got no reply within 10000 ms"). With
the lanes: answered at once beside the parked call, which answers its own reply when released.

**What the lanes exposed.** The first loop of the matrix row on the lanes fired
`lock_order_ledger_overflows` in every run: a served `XvGuards` records its 4a set in the
serving thread's lock-order ledger, the set lives on in the parked scope, and the `XvRelease`
now runs on another lane (a release on a thread that never recorded it is a no-op), so the
first lane's ledger only grew. The parked set is handed off like the conveyor's
(`lock_order.handedOff(.object)` once taken). Pin: `xv.guards` "a served XvGuards hands its set
to the parked scope" — red (`holds(.object)` after the serve) without the hand-off.

## 2. `slot_leases_held` wrapped to −164 on A

The plane added the table's whole count at the manager's arm, nothing for a wire grant at the
manager, subtracted one per release of ANY holder's slot, and a joiner's close subtracted its
table's count including the foreign views it loaded. **Law (0003 §AM.2):** each plane folds
`own leases − last share` into the gauge at every lease change it makes and takes its share out
at deinit; a share counts the leases its OWN regions hold (a wire lessee's are its process's).
**Red → green.** `handover_moves` "THE LEASE GAUGE IS ONE EXACT SUM" (arm → join → accept →
release all → close → re-join → close): on `dev` `expected 64, found 18446744073709551615`;
green with the fix, the gauge back at its start after both closes.

## 3. Loops and gates

On kamrui (x86_64, the 7.2.6-sqz series), the tree as landed:

| Run | Result |
|---|---|
| `handover_moves` "THE MOVE MATRIX — offer pushed" (the flaked row), 4 × `yes` | 200 / 200, at most 35 s a run (the first 67 on the lanes without the ledger hand-off: every run logged the overflow tripwire — §1) |
| smoke "THE HANDOVER DRAINS PARKED STAMPS" (two daemons, the accept served while B's ships run) | 159 / 160; the one failure (run 4 of the first 20) read `meta_kv_times_handover_drained` 0 at `smoke.zig:4073` with nothing pending or dropped after — not met again in 140 runs; `dev` 80 / 80. Owed if it recurs: whether a stamp can leave the slot before step 0 by a path the row does not count |
| red → green rows (§1, §2, the guards pin) | red on `dev` as quoted, green here |
