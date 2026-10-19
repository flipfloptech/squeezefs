# Rung 10 — the overlay record outlives its last store's completion (the 2026-10-19 panic)

**The trace.** Perf session `fsr2-B-r2` (2026-10-19 06:42 local, kernel 7.2.8-7-omarchy-sqz, binary 5db6217 (the fsync-route agent's B leg), RAM NVMe,
the `sw1m-q16j4` row: four files, 1 MiB sequential O_DIRECT writes, QD16 each), 1 of 6 sessions;
`/var/lib/sqz-perf/daemon.log`:

```
thread 1040200 panic: reached unreachable code
std/Io.zig:1771  Mutex.unlock: switch (m.state.swap(.unlocked, .release)) { .unlocked => unreachable
src/data/device_overlay.zig:982  finishStore:  rec.inflight_change.notifyWaiters(io);
src/data/device_overlay.zig:931  land ← write_admit.admitPiece ← plane.write ← kv_fs.write ← lifecycle.run (a handler fiber)
```

A mutex unlocked that nobody held: the record's `inflight_change` notify had been re-initialised
under the thread inside it. The minutes before are a burst of `settle transient, attempt 1/2`
lines on all four inos — concurrent settlers (the detached drains, the fsync drains) meeting the
epoch's running save.

**The interleaving.** Since `b191efb` (2026-10-16) an overlay store claims its range under the
block guard (3) and lands its DMA and CQE holding NO guard; the record's `inflight` is the only
thing keeping it alive, and the registry is a fixed pool whose free list is LIFO (decision 0003
§AP). `finishStore` ran `Core.completeStore` — coverage, the freeze on full, the claim release and
`inflight −= 1` — and THEN `notifyWaiters` on the record:

| A — the CQE (a handler fiber, no (3)) | S — a settler under (3) of the same block; then any writer |
|---|---|
| `completeStore`: covered, frozen, claim cleared, `inflight 1 → 0` | |
| | `settleLocked` → `awaitInflight`: `inflightEmpty()` true (or registered, re-checked true, `cancel`) → proceed |
| | publish/feed → `teardown` → `registry.retire` (the slot on top of the free list) |
| | a writer's `installRecord(block M)` → `install`: `free.pop()` = that slot; `r.* = fill; r.inflight_change = .init` |
| `notifyWaiters`: `interior.lock` … | |
| | … the reset lands between the lock and the unlock … |
| `interior.unlock`: swap reads `.unlocked` → `unreachable` | |

A reset before the lock is silent: block M's record gets an epoch bump and a wake that are not its
own. A parked waiter cannot be the settler — it re-acquires `interior` before returning — so the
settler is one whose re-check caught the decrement without parking: the fsync's `drainForIno`, the
write-back timer's `drainBlock`, a sibling's steal in `claim`, the one-authority screen. The
ACK-early continuation had a second post-drop touch: `defer rec.acked_pending −= 1` after its
`finishStore` (a word nothing read).

**The fix** (`3f0b6c6`). THE COMPLETION'S LAST TOUCH (module doc): the hold drop and its
announcement are ONE critical section of the record's notify, and the settler quiesces on that
lock once after reading the set empty — the kernel's `complete()`/`wait_for_completion` shape.
`Core.completeStore` no longer releases; `Core.releaseStore` (the claim clear, then `inflight −= 1`
SeqCst) is the `HoldDrop` that `Notify.notifyWaitersAfter(io, change)` applies under `interior`
before the epoch bump and the wakes. `awaitInflight` ends with `Notify.tryQuiesce`/`quiesce`
(`overlay_teardown_quiesce_waits` counts a settle that found a completion still inside). The
ordering: the drop is a SeqCst RMW program-ordered after the lock's acquire CAS, so a settler that
reads it reads a lock that is or was held; its own acquire pairs with the completion's unlock
(`swap .release`), so every access the completion made to the record happens-before the teardown,
and the unlock is the completion's last access (`Mutex.unlock`'s contended-arm wake passes the
address only). `acked_pending` is deleted. Model 4 (`device_overlay_model.zig`,
`overlay_completion_last_touch_precedes_the_retire`) holds; both weakenings fail
`InvariantViolated`: `drop_outside_lock` (the shipped shape) and `no_quiesce`.

**The red test** (`832f962`, `device_overlay` "THE COMPLETION'S LAST TOUCH"). The in-process seam
`announce_hold` parks a completion after its drop, before its announcement (`announce_held` the
park observable). A whole-block store of block 0 parks there (frozen, `inflightEmpty`); a second
thread drains block 0 and installs block 1 — the LIFO slot; the main thread waits for the drain to
finish OR `overlay_teardown_quiesce_waits` to move, then releases. Unfixed: the settler got through
the window, and block 1's record carried the stale completion (`epoch` expected 1, found 2). Fixed:
the settler waited (one quiesce wait), block 0 stayed registered meanwhile, block 1's notify saw
exactly its own CQE. `sync.notify` gains the primitive's own pin (the change under the lock; a
quiescer finding a notifier inside waits it out, deterministic through the mutex word).

**Runs** (local, 7.2.8-9.30-omarchy-sqz). Units: device_overlay 63/63, notify 61/61, write_admit
78/78, data.plane 50/50, kv_fs 93/93. Smoke: the data plane WITH and WITHOUT a cache path, THE
KILL MATRIX (every fsck clean), `sync -f` IS A DURABILITY POINT — green; `e2e zc WRITE, the overlay
class` green. Perf: six quiet sessions `sudo MX_RUNTIME=20 MX_ROWS='sw1m|sw4m' scripts/perf/quiet.sh
ram-sqz ovl-rN` on this branch's dist binary (`3f0b6c6`), serialized with lever 5's sessions —
no panic, every daemon exit 0, `fsck --offline` clean ×6 (`/build/perf/mx/ram-sqz-rovl-r1..6`):

| row | r1 | r2 | r3 | r4 | r5 | r6 |
|---|---|---|---|---|---|---|
| sw1m-q16j1 MiB/s | 27955 | 29667 | 27611 | 27923 | 27962 | 28329 |
| sw1m-q16j4 MiB/s | 48743 | 39580* | 48893 | 49012 | 48907 | 49088 |
| sw4m-q4j4 MiB/s | 47064 | 46688 | 47579 | 47700 | 47803 | 38202* |

(* a neighbour's build landed mid-row: p99 7.9 / 7.6 ms; the other four within the fsync-route
note's spreads.) `overlay_teardown_quiesce_waits` 1 in each of r2, r4, r5 (≈ 600 K settles and
2.5 M stores per session): the window the panic needed — a settler reading the set empty while the
completion was still inside the notify — was hit three times in six sessions and absorbed.

**Found on the way.** r1's `sw4m-q4j4` row fired `block_claim_anomalies` once (`vol 1 block 192
found refcount 1 … forcing 1`), after 43 `overlay_superseded_dead_old_binding` supersessions and
43 `unpublished_mint_abandons` — the dead-capture belt (finding 51) handing dests back to the
grant window, and the allocator's give-back-vs-next-claim race `rung-10-pack-read-tls-teardown.md`
describes under `abandonUnpublished`. Neither path is touched here; across 715 recorded rows the
belt had never fired before and did not in r2–r6 (0 and 0). The allocator's residual window is
its owner's; the row's fsck was clean.

**Open.** `serve` borrows a record across its device reads holding nothing (no claim, no hold):
the pool keeps the memory valid and the terminal re-checks plus the generation equality guard the
bytes, but a slot re-filled mid-read reads as a fresh record (generation 0) — `birth` is not
re-checked. Not this panic's window; noted for the read path.
