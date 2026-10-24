# Rung 10 — THE INTERCEPTION HOST under load: placement laws, the warm serve, the duplicate kernel invalidation

The e2e row (`ipc/host/e2e_linux.zig`) went red under load at a different assertion each time:
`ipc_placed_severs ≤ 3` after block 1 (10-23), `≥ 4` after block 2 (gate 115, the
`unknownlease` loop), the warm serve's `ipc_fast_path_serves + 1` (the `staged-lost` loop).
Beside the full smoke the row was 220 / 220 green; confined to 4 CPUs beside 6 busy loops (the
client preempted mid-staging) it went red 15 times in 340 runs before the fixes. Daemon
instrumentation (sever arrival per drain pass, adoptions, sideband notifies, GETATTR replies) named
three causes.

**Placement counts follow the drain passes (the row's).** The client pushes each 1 MiB flight
after its copy (`client.multiOp`), so a service park's rescan (5 ms bound) or a client preempted
between flights drains a prefix: `flush n=1 → adopt block 2, 1 MiB → chunks 2–4 why=table,
pooled`. `il_daemon.md` §3.2 states the arm: an entry-present block never adopts, later chunks
pay the 2-copy fallback; the bytes are whole. At the default op ceiling (`max(block, 1 MiB)`) a
block-aligned 4 MiB pwrite is one flight — the split needs the row's 1 MiB op class. The row now
judges the laws: one sever per chunk; the bytes; every assembly settled (`ipc_placed_assembly_bytes`
back to 0, adoptions + reaps ≥ 1 when one placed); the striped block's first chunk takes the placed
arm and its first merge resolves the assembly; four placed with no refusal is ONE adoption, no reap.

**The warm serve's intervenors (the row's).** The guard exempted only Red; at Yellow the hot
tier's insert is net-zero growth (an empty tier sheds the block it just took) and a contended
probe demotes. The serve is judged only at Green across the three samples with no tier eviction,
refused put or contended probe in the window (printed otherwise). Not reproduced in 880 runs.

**A ring write's size was undone by a duplicate invalidation (the product's, POSIX-8).** Seen in
a gate on 10-13 (`rung-10-gate-blockers.md`: the ticket pages read 0 of 32,768) and 13 times
here: eight ticket writes acked, each grow announced awaited (`inval.onWrite`); the kernel `pread`
then read 0 of 32 KiB. Timeline (µs): acks to 284880; `pread` 285440 → its GETATTR; four attrs-only
`INVAL_INODE` of the inode written 285916–285928; the daemon's reply (`size=32768`) 285932;
`pread` returns 0 at 285944. The four were `conveyor.KernelView` invalidations of the ring writes'
OWN commits (`!kernel_origin`), queued before each ack, written by the notifier ~1.5 ms later; one
landing inside the GETATTR bumps `attr_version` and the kernel discards the fresh reply
(`fuse_change_attributes_i`), keeping `i_size` 0. **Fix:** `KvTx.kernel_told` — the IPC host's
handoffs (`Ctx.kernel_told`) commit it; `noteKernel` skips such a member unless its window failed;
the ack posture is untouched (decision 0003 §AB.2 amended).

**Red rows:** conveyor "the kernel view: a change its own channel tells …" (`expected 0, found 1`
without the skip); kv_fs "a request the kernel sent is kernel-origin …" (extended: `kernel_told`
set, origin unchanged); the e2e row under the confined harness.

**Numbers** (`~/tmp/ih/`, 880 runs of the row): confined harness, the corrected row before the
fix 290 / 300 (10 stale-size reds at the ticket pages' `pread`; block 2's `pread` read 0 once
before), after it 240 / 240 (12 split passes judged green); beside the full smoke 80 / 80 (one
Dekker refusal + sealed fallback judged green); the warm serve judged on every run. The
interposer's matrix rows are not owed: the placed-write path is unchanged (the fix drops one
notifier write per ring-write commit).
