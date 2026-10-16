# rung-10-localvenue — the local Linux venue

**What changed.** `scripts/venue` gets a third venue, `local` (`SQZ_VENUE=local`; the default
when the controller is Linux, while macOS keeps `vm`). It runs commands on this host as
`sudo bash -lc "<env>; cd $REPO && <cmd>"`, with no sync and no VM, and keeps the same gate
lock, build slots, `SQZ_VENUE_TIMEOUT` and messages. It has one build slot per 4 threads and per
8 GiB (kamrui: 3, the same as its ssh mode; `SQZ_VENUE_SLOTS` overrides). Without a terminal it
uses `sudo -n`, so an agent never waits on a password prompt. The verbs: `status` adds the
nvmet/nvme_loop/brd/null_blk state (loaded, available, or built-in through `/proc/config.gz`)
and the build root's free space; `up` runs the new `scripts/dev/provision-local-venue`; `trim`
skips fstrim on a root without discard; `pull` does nothing.

The venue root is now `SQZ_BUILD_ROOT` (default `/build`). scripts/venue exports it to every
command along with `ZIG_GLOBAL_CACHE_DIR`. venue-gate-holder, venue-prune (which resolves the
root, so its live-mount check matches `/proc/mounts` under a symlinked root), gate-train
(`--cache-dir "$SQZ_BUILD_ROOT"/zig-cache-v2`, expanded on the venue), provision-ssh-venue,
build-host-kernel, perf_mount.sh (native dist cache) and the in-venue helpers under
`scripts/{suites,bracket,perf}` and `release.sh` all read it. Two kinds of `/build` stay
literal. The first is a container's own volume mount (`dev_mount.sh`, perf_mount's `sqz-bench`).
The second is the product's test paths (`/build/tmp`, knob defaults), which are product code.
For those, the provisioning check requires `/build` to resolve to the root.

gate-train defaults to `local` on Linux and to `vm kamrui` on the Mac.
`build-host-kernel --local [--no-install | --status]` builds on this host with the same patch
set, fragment checklist and localmodconfig procedure. It checks for the reference recipe before
it starts and prints the reboot it needs; it never reboots. `provision-local-venue` is check-only
and idempotent. It prints each fact as ok or MISSING, with the fix. `--apply` installs the
packages and runs provision-ssh-venue (modules, nvme_core parameter, IOR/mdtest, directories).
It never installs a kernel, reboots, or edits a filesystem, LV or sudoers file.

**Why.** Development is moving to a Linux workstation where the agent runs directly. On a
Linux controller, the venue must be the host itself.

**Evidence (kamrui, local mode, from its mirror of this branch).**
- `scripts/venue status` (SQZ_VENUE unset, and `=local`): `venue: local (build slots: 3)`,
  7.2.6-sqz, zig 0.17.0, `fuse.enable_uring=Y`, `nvmet=loaded nvme_loop=loaded brd=available
  null_blk=available`, `/build` 81 GiB free. `pull`: "the local venue shares the tree".
- `provision-local-venue`: every fact ok except one MISSING. kamrui has no Rust reference
  checkout, so `build-host-kernel --local` refuses up front with the clone line; `--status`
  tails the existing build log.
- Slot command (`SQZ_VENUE_SLOT=1`): queued behind seqwrite's perf A/B and the train's gate,
  naming each holder. It then ran as uid 0 holding `/build/.build-slot.0`, with
  `SQZ_BUILD_ROOT=/build` exported.
- `zig build test -Dtest-filter=knobs -Dfresh=true`: exit 0, `7/7 steps succeeded; 55/55
  tests passed`.
- `scripts/venue trim`: six idle caches removed (the zig cache was over its 30 GB cap),
  `/build: 96.6 GiB trimmed`, 166 GiB free afterwards; 26 s.
- Full gate (`SQZ_VENUE=local scripts/venue 'timeout 3900 zig build --cache-dir
  "$SQZ_BUILD_ROOT"/zig-cache-v2 -Dfresh=true gate --summary all'`): exit 0 in 1207 s
  (a cold cache after the trim). `Build Summary: 55/55 steps succeeded; 2999/3082 tests passed
  (83 skipped)`, which matches the train's kamrui ssh-venue gate of the same head line for line.

**The Mac venues still work.** `scripts/venue status` reports `venue: vm (build slots: 2)` with
`nvmet=built-in nvme_loop=built-in` and `/build` 492 G free. `SQZ_VENUE=kamrui scripts/venue
status` reports `venue: kamrui (build slots: 3)`.

**Limits.** One venue covers one architecture: an x86_64 workstation needs the Mac's vm or an
arm64 box over ssh for aarch64. Every controller of one build root must derive the same slot
count.
