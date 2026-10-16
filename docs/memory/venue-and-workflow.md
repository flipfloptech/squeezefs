# Venue and workflow for squeezefs-zig
How to build, test, and work on squeezefs-zig: the macOS dev box, the Linux venues (vm container, kamrui), kernel build/swap, disk hygiene, gate locks and gate train, landing protocol, git publishing, subagent pattern, and owner preferences.

## Gate locks and timing
- Full gates are serialized via `flock /build/.gate.lock` in `scripts/venue` (40bf775); run them only via `scripts/venue`, never from wrapper scripts inside the container, never under `yes` load.
- Venue concurrency: a full gate takes `/build/.gate.lock` exclusive and all `/build/.build-slot.{0,1,2}`; slot commands take the gate lock shared before each slot try and poll all slots (d79db03). kamrui has 3 slots, VM 2.
- `scripts/venue-gate-holder` (6183f59) reads /proc/locks inside a venue to name the gate-lock holder (worktree, pid, elapsed, kernel build/install by name); `scripts/venue` waiting messages include it. A 'full gate holds the venue' wait usually means the train's legitimate run (~10 min on kamrui), not a stale lock. Agent worktrees see old messages until they rebase onto dev.
- A hung unit test in a kamrui full gate holds the gate lock and all three slots until the outer `timeout 3900` (65 min); apparent long 'build' times may be queueing behind it.
- Gates started before the lock existed don't hold it: to see what runs, `ps` grep `zig build`, `squeezefs-smoke`, `test_runner`, then `readlink /proc/<pid>/cwd` to map to a worktree.
- One full gate (`zig build --cache-dir /build/zig-cache-v2 gate`, 4 smoke shards) ≈ 6–7 min on VM, ~10 min on kamrui; an agent looks idle meanwhile. A flat tool-call count AFTER the gate ends is the real stuck sign.
- 2–3 concurrent full gates livelock the 8 vCPU/16 GiB VM. Recovery: SIGTERM the host `container-runtime-linux start … --uuid sqz-zig` process, then `container stop` + `container start sqz-zig`.

## Venues
- `vm` (default): macOS container sqz-zig, aarch64, Ubuntu 24.04, 7.2.6-sqz kernel, Zig 0.17.0. Owner cannot afford cloud compute; the Mac VM stays. Reserved for the gate train + short aarch64-only checks (opening it to agents deferred: OOM risk from concurrent builds).
- `kamrui` (`SQZ_VENUE=kamrui scripts/venue '<cmd>'`, calls ≤ ~15 min): x86_64 kamrui-dev (justin@192.168.86.250, passwordless ssh+sudo), AMD Ryzen 5 5500U 6C/12T 15 W laptop CPU, 61 GiB RAM, single WD Blue SN550 1 TB DRAM-less NVMe (nvme0n1, 512 B LBA), Ubuntu 26.04, kernel 7.2.6-sqz (modules nvmet, nvme_loop, nvmet_tcp, brd, null_blk). VG ubuntu-vg: permanent LVs sqzbuild 200G ext4 at /build and ubuntu-lv 100G; ~628 GB free for raw LVs. Zig 0.17.0 /opt/zig, fio 3.41, OpenMPI. kamrui is the throughput bottleneck (agents queue there).
- kamrui runs: rsync to same absolute path `/Users/joberdorf/Source/squeezefs-zig` then `ssh … sudo bash -lc` (sudo-rs rejects `-E`); `scripts/venue pull <path>` copies outputs back (Mac tree is source of truth). Cache dir /build/zig-cache-v2. `~/Source/squeezefs-zig` on kamrui is the owner's own clone — leave untouched.
- Remote process checks/kills over ssh ALWAYS use bracket patterns (`pgrep -f "[b]uild/kernel/build.sh"`, `pkill -f "[p]rove"`): otherwise the pattern matches the ssh command itself (false 'alive', or kills the remote shell → exit 255).

## Kernel build
- `scripts/kernel/build-host-kernel <host>` builds 7.2.6-sqz remotely via bindeb-pkg (host config + localmodconfig, fragment + TLS=y, dpkg); installs build deps incl. libdw-dev; feeds `make` via `< <(yes '')` (piping `yes ''` under pipefail SIGPIPE-killed the set -e script silently); ERR trap logs 'BUILD FAILED at line N'; `--status` tails /build/kernel/build.log; success marker BUILD-AND-INSTALL-OK. Default GRUB boots 7.2.6-sqz; stock 7.0.0 kernels stay as fallback. `systemd-networkd-wait-online` failing on kamrui is harmless.
- 2026-10-15: both venues run 7.2.6-sqz with 0001–0029. kamrui: `build-host-kernel kamrui-dev --no-install`, then a DETACHED script (`setsid nohup`) waits for gate lock + every slot, checks .deb sha256s, `dpkg -i`, `systemctl reboot` at once. `dracut[E]: … overlayfs` is harmless. VM: `build-venue-kernel /tmp/ksqz-<tag>` (SQZ_KERNEL_CPUS=3 ~7 min), boot-test `container run --rm -k <Image> …`, `container system kernel set --binary <Image> --arch arm64 --force`, recreate while the train is on kamrui. `container rm -f` stale exec record → `container stop` then `rm`. /build is a named volume.
- squeezefs-zig carries 0027+ in `scripts/kernel/patches-7.2/`; base 0001–0026 in ~/Source/squeezefs @44752ce5 `docker/kernel-sqz/patches-7.2/`.

## Gate train and landing
- `scripts/gate-train` gates dev head on `GATE_TRAIN_VENUES="vm kamrui"`, re-gates to 3 greens, retries red once; logs in `.gate-train/` (`ledger-v2.tsv`, per-run `<venue>-<sha>-<UTC>.log`). Agents do no post-landing gates. `-Dfresh=true` forces gated test steps (Zig caches by ZIR; a summary far below ~2,900 tests = cached run).
- To swap gate-train mid-wave: stage edit, kill monitor + `.wt-train` processes, install, commit, restart — strictly in sequence.
- Edit shared scripts atomically (temp file + `mv`); in-place rewrites break running bash invocations.
- Ubuntu 26.04 uutils: `sync FILE` = host-wide `sync()` (rigs use `sync -d`/`-f`); `dd` truncates after last write (use `conv=notrunc`). Pin `LC_ALL=C` in harness sorts.
- Lock waits are NOT covered by SQZ_VENUE_TIMEOUT. Diagnose a stuck agent with `sudo lsof /build/.gate.lock /build/.build-slot.*` (`rW` holder, `r` waiter) + `ps --ppid`.
- NEVER commit into an agent's worktree; agents run their worktree's own `scripts/venue`, so venue fixes reach them only after they rebase onto dev (or `git checkout dev -- scripts/venue`).
- Orphaned daemons: janitor monitor stops `squeezefs` daemons older than 3 h. Disk: `scripts/venue trim` (fstrim, `scripts/venue-prune` under locks, fstrim); auto-trim monitor when Mac < 40 GiB; janitor trims kamrui when /build > 60%. Restart monitors at their 10 h limit.
- `container exec` ignores local SIGTERM; kill inside the VM by pid.
- Owner rule (2026-10-15): external suites (pjdfstest, LTP, xfstests — class C) run ONLY right before a release tag, never as discovery/probe passes while fix agents work (competes for kamrui slots); the gate's 2-min pjdfstest `suites-smoke` stays.
- AGENTS.md Landing: light tier = red test + fix + targeted venue runs; full tier adds ONE venue gate. Red train on your commit: fix within an hour or revert. Flaky rows in docs/gates/flakes.md.

## Git publishing
- origin `git@github.com:flipfloptech/squeezefs-zig.git`. Push `dev` after every landing; agents never push own branches, never force-push. Local hooks push dev in background (`.git/sqz-push.log`).

## Subagents
- Small single-defect subagents, ~150 tool-call budget, fresh not resumed; ≤ 2 at once (org 30M tokens/min). Near context limit: wip commit + status file.
- Session-only restart cancels monitors/background commands/subagents; containers keep running.

## Dev box and vm details
- aarch64 macOS 11 cores/36 GiB; repo `/Users/joberdorf/Source/squeezefs-zig`, branch `dev`; AGENTS.md read first.
- `/build` on vm is `/dev/vdc` sparse ext4; repair after host crash: `umount /build/ext; umount /build; e2fsck -f -y /dev/vdc`.

## Owner preferences
- Present recommendations, not open choices. Performance first; amplification is a defect. Substrate gaps: fix the substrate, fail loud.