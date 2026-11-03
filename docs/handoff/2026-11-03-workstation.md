# Handoff — 2026-11-03

`dev` = `origin/dev` = `a98d0b7`. The venue is quiet: no daemon, no mount, no venue slot held. Every landing below
had its own green full gate on the local venue (the gate train was not run). Decision ledger 0003 runs through §DF.

## The venue changed (owner rulings 2026-11-02)

- **No kernel patches of ours.** The sqz series is retired (§DB): `scripts/kernel/patches-mainline/` and `upstream/`
  are deleted; `FUSE_TIME_LIMITS` is gone from the daemon. The product floor is stock kernel.org ≥ 7.3.
- **Two boot entries** (§DD), both kernel.org 7.3.1 built by `scripts/kernel/build-host-kernel`:
  `linux-omarchy-mainline` (default: Omarchy's 82 applying patches pinned at omarchy-pkgs 99aa5aa + BORE's own 7.3
  patch + 8 ported AMD/VESA display fixes) and `linux-mainline-sqz` (pure stock). Neither touches FUSE, io_uring or
  NVMe; Omarchy's four FUSE patches are already upstream in 7.3.1.
- **`nvme_core.multipath=N`** on the venue (`/etc/limine-entry-tool.d/sqz-venue.conf`, checked by provisioning) so the
  two-identity PR rows run on one host; the multipath-tcp row skips by name.
- **7.3 sends no FUSE_SYNCFS** (upstream from 7.4): each mount probes it and says so (`.stats fuse_syncfs_reaches`);
  `sync`/`syncfs` make no staged byte durable on 7.3 — fsync does. Rows that need it skip by name.
- The thermal governor now sources its config before building its ladder (76/82/88/94 °C).

## What landed since the 10-30 note (red first; evidence in `docs/evidence/`)

- **§DB, §DD** the stock and Omarchy-flavoured kernels; **§DE** the nvmet head verdict under multipath=N (stock 7.3.1
  links every identity's controller into one subsystem dir) and rows that land data by `sync -d <file>`.
- **§DC** a deleted file's parked bytes and write-cache resident are discarded at the delete (the final FORGET at
  nlink 0), never uploaded after it; §CW's ownership exception deleted.
- **§CO.34–36** the no-open close row holds the times-drain tick (`SQUEEZEFS_TEST_TIMES_DRAIN_HOLD`).
- **§DF one NVMe host identity per host** (owner option C): one key per host and set, co-located mounts share their
  associations, the stale-sibling rule (prove dead or SIGKILL any other incarnation of the set on the host before
  a (re-)registration), the manager's hold WERO (device-enforced single manager), a writer registers before its first
  write after reading the claim, a hold taken by preempt is kept at unmount, the journal lane classifies a fence
  before the conveyor's rollback (DF.16). Six rounds of independent safety review (findings in
  `~/tmp/host-identity-review*.md`) and two exhaustive models (`src/meta/guard/host_fence_model.zig`: Election
  219,189 states, Member 7,216; every known weakening fails).

## Open — owner decisions (recommendations first)

- **Member-death fencing on nvmet 7.3** is detection-only with possible loss of metadata frames: nvmet's holder
  preempt ignores PRKEY (a spec deviation; draft upstream report `~/tmp/nvmet-pr-preempt-bug.md`, unsent).
  Recommended: a write-free fencing registrant per manager host (a non-holder's preempt takes nvmet's spec path).
- **Data namespaces have no PR registrants** (owed since rung 6): a frozen-then-resumed member can overwrite
  reallocated extents silently. Recommended next: register member hosts on data namespaces and fence them there.
- **Cross-member unlinked-but-open files** read zeros once the deleting member's kernel forgets the inode
  (pre-existing at the destroy, now at the delete). Recommended: such reads answer ESTALE.
- Owner checks: s2idle resume on 7.3.1, the display fixes on an external HDMI 2.1 VRR / OLED panel.

## Open defects / costs (no owner yet)

- Each NVMe writer open replays the ring privately three times (§DF.13(l): ~100 MB on the default 32 MiB ring,
  805 MB on 256 MiB) — a lighter claim probe.
- §DF.13 accepted windows: a PTPL-less target restart; the register→re-judge instant; the probe→hold instant (m);
  siblings in another mount namespace or with another hostname are invisible to the rule; requeued writes after
  the drain; containerised daemons refuse PR-capable mounts.
- With `SQUEEZEFS_META_FLUSH_INTERVAL_MS=0` the no-open close row fails at `drains >= 2` (§CO.36; also on dev before).
- The handoff-10-30 items still open: the other ino-keyed maps with removals, reclaim-tick starvation in removal
  storms, transport_lease_overlong at load ~50, §CI.8.

## Resume

1. `uname -r` = 7.3.1-1-omarchy-mainline, `cat /sys/module/nvme_core/parameters/multipath` = N, `scripts/venue status`,
   `scripts/dev/provision-local-venue`; `mount | grep squeezefs` empty.
2. Pick from the owner decisions above (they gate the next fencing work) or the open defects; one full gate per landing.
3. Benchmarks: AC power, a quiet host, `~/tmp/abba-*.sh` under the gate lock and every slot.
