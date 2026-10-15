# Rung 10 — kamrui: the failing daemon's fail-stop order, the matrix `df` at rest, the §L row on its law (2026-10-15)

## 1. A daemon failing its bring-up deadlocked with a host-wide `sync` (decision 0003 §AN)

**Cause.** After the INIT reply the kernel blocks every request in `fuse_get_req` until every
queue registers. A host-wide `sync` parked its FUSE_SYNCFS there, holding the superblock's
`s_umount` for read; the failing daemon's `umount2(MNT_DETACH)` dropped the mount's last reference
in its own task (`cleanup_mnt` → `deactivate_super` → `s_umount` for write) and only THEN wrote
fusectl's `abort`. Neither could proceed. The request timeout times queued requests only.

**Red** (`smoke: [venue-exclusive] A DAEMON WHOSE BRING-UP FAILS NEVER HANGS THE HOST'S sync`;
seams `SQUEEZEFS_TEST_FUSE_FAIL_AFTER_INIT_REPLY` and `SQUEEZEFS_TEST_FUSE_STARTUP_FAIL_HOLD`, a
FIFO the failure path waits on until the sync is seen in `fuse_sync_fs` with the connection's
`waiting` ≥ 1): on kamrui with the old order the sync was still held 15 s after the release,
`waiting` 1, the daemon's main thread `deactivate_super ← cleanup_mnt ← task_work_run ←
exit_to_user_mode_loop`; the row broke it through fusectl and failed `HostSyncHeld`.

**Fix.** `failStop` (fusectl `abort` for our connection; without fusectl `MNT_FORCE` in the
detach, or the device's close before `fusermount3 -u -z`) runs at the head of every bring-up
errdefer that can wait, inside `unmountDetached`, and in `close` when a drain missed its deadline.
Our own end leaves the mount ours to unmount (the sideband no longer clears `mounted` for it).
`Session.mount` owns fusermount3's fd on every path; the CLI's second `fusermount3 -u` and second
`close` of the same fd are gone. **Green:** the sync returns 20 ms after the release, 3 of 3 on
kamrui; the daemon exits 1 naming the fail-stop, nothing mounted. Unchanged rows re-run on kamrui:
`transport.session` (every e2e, the KmbufRingEnomem refusal included), the LATE bring-up refusal,
the frozen daemon (§Z), both BUSY umount rows, the POSIX pass.

## 2. THE TWO-DAEMON MATRIX: `df` off by 1–2 blocks

**Cause** (no product defect: the counts settle). A mint the dd abandoned (a superseded upload)
that B's grant window could not take back is a terminal free shipped to A at the cadence. B's
`df` drops it at A's `freed` verdict (`freed_since_word`), A's when its bit clears. The row read
B's `df` right after `sync -d` (before the cadence flush: B +2) and compared A's with every granted
block (after the flush: A −1). **Fix:** the row awaits the shipper at rest (B: every shipped block
answered, `pending_frees` 0; A: `free_grace_offsets` 0), then judges B = the file and A = granted −
B's freed; the later free counts are deltas. Loops: kamrui 20/20 (0–4 abandoned mints per run).

## 3. meta.conveyor §L "the window-fill hold"

**Cause.** Under load the hold's measured gain fell under `GAIN_CLOSE` (237/1000, 0/1000): the gate
closed, the declined windows went unpaced and the row's per-round count (274 > 248) judged the
worth gate's schedule, not the hold. **Restated:** the gate is pinned open (`WindowHold.pin_gate_open`,
a test seam; the gate's learning stays the worth-gate tie rows'). Asserted under ANY schedule: the
pacing law — pad-majority windows ≤ ⌊elapsed / D⌋ + 1 + deadlines (serialized a round trip apart
except by a deadline) — and its byte reading, device ≤ that × one grain + 2·s·entries. At the paced
schedule (elapsed ≤ 248 round trips) the original two-per-round count and pad bound still hold.
An unpaced pass (the original red: 16 windows per round) breaks the law at once. Loops: macOS 12/12
+ 10/10 under 11 × `yes`; kamrui 20/20 + 10/10 under 12 × `yes`; the VM 20/20. The flakes row is removed.
