# PR M1 acceptance — bounded root nvmet session (single-writer guard, D0/B1)

| | |
|---|---|
| **Date** | 2026-07-14 |
| **Scope** | `docs/design-metadata-throughput.md` §5.0 B1 pt 5 + PR M1 Verify row; settles **OQ 4a** (kernel-nvmet PR support) |
| **Box** | kernel `7.1.3-2-cachyos`, nvme-cli 2.16; nvmet-loop target over a memory-backed null_blk (the baseline recipe, M1-unique names: subsystem `sq-m1-guard`, port 19, nullb `m1_guard_back`) |
| **Artifacts** | `~/tmp/m1_guard_root/` — `setup_m1_nvmet.sh`, `run_m1_session.sh`, `teardown_m1_nvmet.sh`, `setup.log`, `root_harness_run{1..3}.log`, `session_run{1..3}.log`, `teardown.log` |
| **Rails** | TESTING PAUSE honored (no fstests/LTP); sudo state created by the session torn down and verified (`modules gone / no m1 nullb node / nvmet clean`); no pre-existing device or mount touched |

## OQ 4a verdict: kernel nvmet-loop **supports** Persistent Reservations on this line

- The nvmet namespace configfs attribute `resv_enable` **exists** on
  `7.1.3-2-cachyos` and was set before namespace enable (it is refused on an
  enabled namespace).
- Identify Namespace over our passthru client: **`RESCAP = 0xfe`** — Write
  Exclusive (and everything above it) supported, **bit 0 (PTPL) = 0**: no
  persist-through-power-loss. The §5.0 B1 pt 6 heartbeat-cadence Report
  re-check (`writer_guard_pr_reacquires`) is therefore *load-bearing* on this
  target class, not belt-and-braces.
- The guarantee-class table's "loop-backed nvmet namespace" row lands
  **enforcement-grade** here (the file-backed `losetup` share shape remains
  detection-grade as documented — loop *block devices* expose no PR; this
  session's namespace is nvmet-loop *transport* over null_blk).

## What was validated (real device state, not the fake)

| Leg | Mechanism | Result |
|---|---|---|
| RESCAP probe | our `NvmeReservationClient::rescap()` (Identify Namespace byte 31) | `0xfe` ✅ |
| Register + Acquire-WE + Report + Release | our passthru client end-to-end (`root_session_real_nvme_reservations`, run3) | holder key observed via our Report decode; release clears ✅ |
| Device fence, non-registrant | host B (2nd loop association, distinct hostnqn/hostid) passthru WRITE while A holds WE | rejected, NVMe status `0x6083` (Reservation Conflict + DNR) ✅ |
| Acquire arbitration | host B plain acquire while A holds | **Reservation Conflict** (`0x4083`) — exactly the §5.0 conflict branch ✅ |
| Preempt (stale-holder takeover) | host B `racqa=1, prkey=KEY_A` | success; report shows holder = B's key, A's key unregistered ✅ |
| Fence on the preempted holder | host A passthru WRITE post-preempt | rejected `0x6083` ✅ |
| **Fence at the BARRIER (Issue 14)** | buffered `pwrite` (succeeds into page cache) + `fdatasync` on the fenced path | `pwrite` OK, **`fdatasync` fails** — the fence surfaces at the barrier, never the buffered write ✅ |

## Measured finding: the barrier errno on the buffered path is `EIO`, not `EBADE`

Through the **buffered-writeback** path (page-cache write + `fdatasync` on the
namespace head node), the kernel normalized the reservation conflict to plain
**`EIO` (5)** by the time `fdatasync` reported (`mapping_set_error` collapses
address-space writeback errors); direct/passthru submissions DO surface the
conflict class (`0x83` status / `EBADE`). Consequence for the guard, already
designed for ("M1 pins the exact mapping, **falling back to generic escalation
on plain EIO**", §5.0 B1 pt 3):

- `EBADE`-class barrier errors latch `failed` **immediately**
  (`writer_guard_fenced`) — covers O_DIRECT/uring-cmd-shaped barrier paths and
  targets that surface the class.
- Plain-`EIO` barrier errors take the **consecutive rung**
  (`JOURNAL_FAILURE_LATCH = 3`, success-reset, dedicated counter): a fenced
  holder on this kernel's buffered path fail-stops within **3 barriers ≈ 3
  flush cadences (~150 ms at the 50 ms default)** instead of one. Both rungs
  are pinned by `tests/mount_writer_guard_tests.rs` (fence-injection at the
  barrier layer for the strict `commit_tx` path AND the checkpoint tick path;
  consecutive-generic escalation with success-reset).

The in-tree fence-injection tests carry the `EBADE` enforcement assertion (as
the design's worst-case plan anticipated), and this session carries the
real-target evidence that the *device* fences both the write path and the
barrier path.

## Protocol/topology notes recorded for the runbook

- **Native NVMe multipath folds same-box "hosts" into paths**: two loop
  associations with distinct host identities produce one head node
  (`/dev/nvme1n1`) with per-association hidden paths (`nvme1cXn1`). The
  session drives multi-host legs through the per-association controller char
  devices (`/dev/nvme1`, `/dev/nvme2`) — production cross-host mounts are
  unaffected (each host owns its association); the §5.0 B1 pt 6 multipath
  note stands.
- **Reservation Report needs EDS on fabrics**: 128-bit host identifiers make
  the short report form fail with `Host Identifier Inconsistent Format`
  (`0x18`); the client requests the extended structure first (64 B header +
  64 B entries — verified byte-wise) and falls back to the short form for
  PCIe-class controllers.
- **NVMe Release does not unregister**: a clean unmount now releases *and*
  unregisters its key, so no residue accumulates on the namespace across
  mounts (a stale registration otherwise survives and later reports as a
  ghost registrant).
- nvmet quirks inherited from the baseline recipe held: `-i 4` connect
  (offline-CPU hctx EXDEV), uuid/nguid stamping on null_blk backings,
  configfs symlink `unlink` (not `rm`) on teardown.
