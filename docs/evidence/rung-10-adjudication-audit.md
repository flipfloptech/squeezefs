# Rung 10 — the adjudication audit (every standing exclusion against the 100 % standard)

**Ruling (owner, 2026-10-22):** "we shouldn't skip a test or adjudicate tests unless we are 100 %
certain it's not a bug/defect/unsupported." A harness limitation is ours to fix, never to
exclude (`rung-10-huge-volume.md`: generic/620's 17 TB hugedisk was "the venue's" until the
harness formatted the raw device).

**Venue:** the Omarchy workstation (`local`; not a container — root holds `=ep`, `/proc/sys`
writable, `ulimit -f unlimited`, loop and dm available), kernel `7.3.0-1.7-mainline-sqz`, head
`7e0792a`, 2026-10-22 18:30–19:45. Every row below was run by hand on this host with its
exclusion lifted; the LTP rows on a squeezefs mount AND on the venue's ext4 with kirk's exact
environment (`~/tmp/audit-ltp-byhand.sh`: cwd `testcases/bin`, `TMPDIR` on the filesystem under
test, `LTPROOT`, `LTP_TIMEOUT_MUL=4`, default signal dispositions).

**Verdicts:** KEEP (certain, with the citation that rules it) · STALE (passes here; deleted) ·
DEFECT (ours; mechanism; fixed or a fix item) · OWNER-CALL (a by-design claim with no ruling —
the owner decides, nothing invented here).

## 1. `docs/gates/fstests-exceptions.md` (10 rows)

| row | current claim | what ran / was read on this host | verdict |
|---|---|---|---|
| generic/003, 192 | `noatime` | Both failed in today's `-g auto` (18:31, head 7e0792a) exactly as ledgered: atime never moves on a READ. Kernel side verified in `fs/fuse/dir.c` `fuse_flush_times`: the kernel's time writeback to a FUSE daemon carries `FATTR_MTIME` (+ `FATTR_CTIME`), never atime; the daemon's read ladder writes no inode | **KEEP** — charter §11 names `noatime` one of the three adjudicated classes. Citation present and sufficient |
| generic/441, 484 | `by-design-fail-stop-metadata-eio` | dm-error under the set's ONE device → every metadata barrier fails → the volume latches FAILED; 441's second write on each fd answers EIO at `write(2)` (write-through), 484's second syncfs answers EIO | **KEEP** — charter §1.8 ("Fail-stop lattice: repeated journal-barrier failure or a device fence latches the volume FAILED (EIO until remount)"). The ledger rows cite §1.8 correctly |
| generic/798 | `by-design-write-through` | `cachestat(2)` sees `Dirty: 0`: the kernel holds no dirty page because the daemon does not advertise `FUSE_WRITEBACK_CACHE` (the `-o writeback` mount option exists and is off in the release posture; `session.zig:1318`); the test's own header rejects "any mount option that writes data back synchronously" | **KEEP (citation weak)** — the behaviour is certain and the test declares itself inapplicable, but no charter line says "the kernel's writeback cache is off by default" (decision 0003 Q9 notes the §4 INIT list is silent). Charter §6.1 / §1 item 10 make the daemon's RAM ring THE write cache, which implies it. The owner may add the sentence to §4 |
| generic/409, 410, 411, 589, 732 | `by-design-single-writer-guard` ("decision 0002 row 7-class") | A second `mount` of SCRATCH_DEV is refused `another writer holds the single-writer guard` (today's run: 409–411 `.out.bad` carry the refusal). **The cited ruling is wrong:** decision 0002 row 7 is about `std::sync::Mutex` in the reference's cache; no 0002 row rules the guard. Charter §5.7 does — and it says the refusal is the `--single-writer` CLASS's: "Under the symmetric default the D0 winner IS the manager and a later RW mount JOINS it". The suites format `--single-writer` (`common.sh` `SUITE_SINGLE_WRITER=1`, a harness choice from 2026-10-10, before the default flip) — so these five tests judge the NON-DEFAULT format, and under the shipped default a second mount of one device on one host is a capability we SHIP (`tarx.sh`'s A-B-B-A row is exactly that shape) | **OWNER-CALL** — (a) keep the suites on `--single-writer`: the five rows are KEEP, with the citation corrected to charter §5.7 (done in the ledger); or (b) run the release suites on the default symmetric format (`SUITE_SINGLE_WRITER=0`, now honoured by `mkfs.squeezefs` through the env file): the five rows then judge the join and leave the ledger (pass) or become defects. §2.4 below has the first measurement under (b) |

## 2. `scripts/suites/fstests-venue-exclude.txt` (16 rows, all container-era but one)

Run as `SQZ_VENUE_SLOT=1 SQZ_VENUE=local scripts/venue 'SUITE_RUNS=1 FSTESTS_TESTS="…" scripts/suites/fstests.sh'`
with the rows deleted from the list (the harness's "ledger rc 3 stale rows" ending is the
subset's artefact: the ledger rows did not run). Results in §2.1–2.3; the table is the verdict.

| row | current claim | this host, exclusion lifted | verdict |
|---|---|---|---|
| generic/394 | `ulimit -f` refused in the container | **pass** (2 s): `RLIMIT_FSIZE` set, the 1 GiB + 1 truncate answers "File size limit exceeded" | **STALE** — deleted |
| generic/564 | `ulimit -f` refused in the container | **pass** (2 s): every `copy_range` refusal class (EINVAL/EBADF/EISDIR/EOVERFLOW/EFBIG, the 8 EiB bounds) as golden | **STALE** — deleted |
| generic/597 | `/proc/sys` read-only (fs.protected_symlinks/hardlinks) | **pass** (1 s); the sysctls restored to 1/1 | **STALE** — deleted |
| generic/598 | `/proc/sys` read-only (fs.protected_regular/fifos) | **pass** (1 s); restored | **STALE** — deleted |
| generic/460 | `/proc/sys` read-only (vm.dirty_*ratio) | **pass** (2–3 s) — and it left the HOST at `dirty_ratio=100/100` with `dirty_bytes=0`: this workstation is tuned through `vm.dirty_bytes` (Omarchy: 256 MiB / 64 MiB), the ratios read 0, and xfstests' `restore_dirty_ratio` restores only a non-zero saved value. Harness fix: `fstests.sh` snapshots the eight `vm.dirty_*` / `fs.protected_*` sysctls before `check` and restores them after (bytes first where the host uses bytes); verified: "host sysctls a test left changed were restored" and the host back at 256 MiB / 64 MiB | **STALE** — deleted (the hazard fixed in the harness, §7) |
| generic/750 | `/proc/sys` read-only (vm.compact_memory) | the test **passes** (19 s) — the run is red on the daemon: TRIPWIRE `alloc_trim_window_parks` ×3, 235 × `syncfs: a leg failed (InputOutput) — EIO to the application`, `pack refill: vol … has no free block for the open pack` (fsstress, 4 procs × 25,000 ops, filling the 2 × 8 GiB scratch). The same tripwire fired in today's release `-g auto` run under generic/475 (set-scratch, 19:12). `enospc.zig:201` documents the site as "≈ 0 except exactly that regime" (a full store under rewrite holding its free list inside trim windows) yet it is a `Tripwire` — the fence-tripwire classification question of 2026-10-19 again | **STALE as an exclusion** — deleted; the tripwire + the syncfs EIOs are a **product item (F7)**, not the test's |
| generic/795 | `/proc/sys` read-only (vm.drop_caches) | **pass** (123 s) when run alone. A first run FAILED `_check_generic_filesystem: … inconsistent — data volume discovery: …-d0 carries a data superblock this binary refuses`: a SECOND harness instance (`.wt-small`, generic/563 at 19:28:41) had re-pointed `/sbin/{mount,mkfs,fsck}.fuse.squeezefs` and `/run/squeezefs-suites.env` to its tree mid-test, so the final fsck stood the OTHER tree's slice layout over this run's device. Not the product's — a harness defect (F6) | **STALE** — deleted |
| generic/426, 467, 477, 756, 777 | exportfs: `open_by_handle_at` EPERM in the container (CapEff lacks CAP_DAC_READ_SEARCH; "nothing to judge") | ALL FIVE **fail on the product**: `open_by_handle(…) returned 116 incorrectly on a linked file!` (ESTALE) after drop_caches / a cycle mount — 1,024 lines in 426, every file in 467/477/756, files AND the dir in 777 (connectable handles). The handle is minted (`name_to_handle_at` works: `fuse_encode_fh`), but the daemon does not advertise `FUSE_EXPORT_SUPPORT`, so once the inode left the icache the kernel's `fuse_get_dentry` (`fs/fuse/inode.c:1054`) answers ESTALE instead of asking us `LOOKUP(nodeid, ".")`. With the container's EPERM gone, the exclusions' reason was false and the rows hid a missing capability | **not the venue's — deleted**; **OWNER-CALL**: rule export handles unsupported (then five `by-design-no-export-handles` ledger rows citing the ruling) or implement **F3** (~1 day). Until one happens the release `-g auto` stops at generic/426 |
| generic/310 | `dmesg -c` refused (no CAP_SYSLOG) | **pass** (64 s) | **STALE** — deleted |
| generic/504 | `/proc/locks` shows no flock row (ext4 control agreed, in the container) | **pass** (2 s): the `flock -x` row appears for our inode | **STALE** — deleted |
| generic/519 | FIBMAP "requires root privileges" (no CAP_SYS_RAWIO) | **pass** (2 s) | **STALE** — deleted |
| generic/650 | CPU hotplug: offlined CPUs never return on this host (7.2.8), nvme fabrics EXDEV after | not re-run: a failed re-test needs a reboot, and the venue carried the release's `-g auto` and a sibling branch's runs all evening; the row's evidence is this host's | **KEEP for now** — reworded; the owner picks the moment to re-test on 7.3.0 (F5) |

Fsck after every subset run: 0 findings on both sets; no daemon alert but 750's.

### 2.4 The single-writer rows under the DEFAULT (symmetric) format — the first measurement

`SUITE_SINGLE_WRITER=0 SUITE_RUNS=1 FSTESTS_TESTS=generic/732 scripts/suites/fstests.sh` (19:39,
`mkfs.squeezefs` now honours the variable through the env file): the scratch set formatted in
the default symmetric posture; the test's second `_scratch_mount` **JOINED** ("JOINED as
appender 1 (term 1 …) at 192.168.86.52:38005: ring 524288 B in 1 s" — the guard refusal is
gone, as §5.7 says); `mkdir A B; touch A/f; mv A/f B/` through mount 1 and `cat B/f` through
mount 2 passed; then **`mv mount2/B/f mount2/A/f` answered EIO after 90 s**: the joiner's
daemon logged `move gate: rename met a slot move that did not resolve in 90000 ms (last answer
SlotMoved) — answering EIO (the fail-stop lattice's deadline)` with the FUSE op watchdog naming
`rename2 (ino 4) in flight for at least 74999 ms, parked past station [meta_backend_done]`.
Both sets fsck-clean; no tripwire. So under the SHIPPED default the five "by-design" rows
cover a **product defect (F8)**: a rename through a joined mount, into a directory the manager
holds, never resolves its slot move. The by-design rows stand only because the suites run the
non-default class.

## 3. LTP — `docs/gates/ltp-exceptions.md` (5 rows), `ltp-venue-skip.txt` (2 rows), the control

### 3.1 The ledger rows (by hand on squeezefs AND ext4, kirk's environment)

| row | current claim | squeezefs / ext4 on this host | requirement · what we do · ruling | verdict |
|---|---|---|---|---|
| syscalls/epoll_ctl06 | `by-design-fuse-poll` | `TFAIL: epoll_ctl() on file succeeded` (24/25 pass; the directory case passes) / 25 pass | Linux: `epoll_ctl(ADD)` → EPERM iff `!file_can_poll(file)` = `f_op->poll == NULL` (`fs/eventpoll.c:2651`, `include/linux/poll.h:73`). FUSE: `fuse_file_operations.poll = fuse_file_poll` on EVERY regular file (`fs/fuse/file.c:3092`); the daemon's ENOSYS to FUSE_POLL only latches `no_poll` → DEFAULT_POLLMASK. No INIT flag withdraws it | **KEEP** — the kernel module's shape, certain; outside the daemon. No charter line records it (§3 could) |
| syscalls/fallocate03 | `by-design-no-fallocate` ("outside the must-implement set, decision 0003") | all 8 cases `EOPNOTSUPP` / 8 pass | Linux `fallocate(2)`: mode 0 allocates + extends size, `KEEP_SIZE` allocates only. We: `filesystem.zig` default ENOSYS (kernel → EOPNOTSUPP); `kv_fs` implements no `fallocate`. Ruling: NONE — the charter is silent; decision 0003 has no fallocate row; `docs/porting/fuse_abi_session.md` §6 lists fallocate in "What the daemon MUST implement" and `fuse_handlers_lanes.md` has it as R4 (the reference served it). The citation was wrong | **OWNER-CALL / DEFECT (feature gap)** — fix item F1 |
| syscalls/mremap06 | `by-design-no-fallocate` | `TBROK: fallocate() failed` / 3 pass | the test's setup is `fallocate()` | as fallocate03 |
| syscalls/ioctl_loop05 | `by-design-ltp-pseudo-fs-policy` | `TBROK: …/test.img resides on an unsupported pseudo-file system` / 8 pass | LTP `tst_find_backing_dev` (`lib/tst_device.c:857`) needs the BLOCK DEVICE under TMPDIR's filesystem (`BLKSSZGET`); it refuses `major(st_dev) == 0` unless btrfs/overlay. Every FUSE superblock is anonymous (`fs/fuse/inode.c` `set_anon_super_fc` / `get_tree_nodev`), as tmpfs/NFS. No filesystem op is reached; loop over a squeezefs file itself works (every `.all_filesystems` test does it) | **KEEP** — the test's precondition, certain; reworded with the citations |
| syscalls/setxattr03 | `by-design-no-ioctl` | `TBROK: Set setxattr03immutable immutable failed: ENOTTY` / 2 pass | Linux: `chattr +i/+a` = `ioctl(FS_IOC_SETFLAGS)` → FUSE `fuse_fileattr_set` → FUSE_IOCTL to the daemon (`fs/fuse/ioctl.c:543`); FUSE never sets `S_IMMUTABLE`/`S_APPEND` on its inodes, so a daemon that supports the flags enforces them itself in every mutating op. We: ENOSYS → ENOTTY (legal for an unsupported ioctl; the capability is absent). Ruling: NONE — charter silent; porting spec: the reference served a GDS ioctl only | **OWNER-CALL / DEFECT (feature gap)** — fix item F2 |

### 3.2 `ltp-venue-skip.txt`

| row | current claim | this host | verdict |
|---|---|---|---|
| proc01 | `/proc/kmsg` EPERM inside the container (fails on ext4 too) | **TPASS** on the squeezefs mount: `readproc() completed successfully, total read: 697,665,972 bytes, 2167 objs` (root holds CAP_SYSLOG) | **STALE** — deleted (`5130797`) |
| setsockopt11 | a deliberate OOM; took the daemon down on the 16 GiB VM | Read, not run beside the release run: `tst_pollute_memory(0, …)` (`lib/tst_memutils.c:22`) mmaps and fills `min(MemFree, MemAvailable) − 128 MiB` — and with swap present (this host: 219 GiB) the margin is 0 — up to 64 times in its 300 s runtime; it touches no file of the mount | **KEEP** (certain: no filesystem op; a hazard to every co-tenant daemon — order-8 kmbuf pages, swap) — reworded for this host; run it alone (`LTP_PATTERN='^setsockopt11$'`) when the venue is quiet if the owner wants the row judged |

### 3.3 The "venue's" mechanism (`ltp.sh` `ltp_control_cmd`) — two defects, fixed (`5130797`)

Today's full LTP run on this host (17:00–17:53, head d848fb5) classified 15 failures "the
venue's" (fail on ext4 too). Reading each pair:

| rows | squeezefs vs ext4 failing assertions | cause | verdict |
|---|---|---|---|
| swapon01/02/03, swapoff01/02 | SAME (`swapon() on exfat failed: EINVAL`; tmpfs TCONF) | `.all_filesystems` tests format the loop device with every kernel fs; exfat has no swapfile support. Our mount only carries the loop's backing file and the mountpoints — and does so | the venue's (the kernel's exfat), verified identical |
| preadv203, preadv203_64 | SAME (`write(…) failed: ENOSPC` on the tmpfs iteration) | the tmpfs iteration, kernel side | the venue's, verified identical |
| clock_settime03, leapsec01 | SAME (`Test killed! (timeout?)`; `adjtimex status 8208 not set`) | the host's clock discipline / this kernel; no filesystem op | the venue's, verified identical |
| init_module01/02, finit_module01/02, delete_module01/03 (+ insmod/lsmod rows) | **DIFFERENT**: `ENOEXEC` on the mount vs `open(init_module.ko) ENOENT` on ext4 | (1) LTP's `configure` baked `LINUX_DIR=/lib/modules/7.2.8-7-omarchy-sqz` and the `.ko`s carried that vermagic under the running 7.3.0 — a stale harness install; (2) the control ran from its tmpdir, kirk runs from `testcases/bin`, so the control could not even find the module | **harness DEFECT, fixed**: `ltp_rebuild_modules` reconfigures and rebuilds + reinstalls the 11 module directories on a kernel change (5 s + 17 s); the control runs from kirk's cwd. By hand after the rebuild: all six pass on squeezefs and on ext4 |

The deeper hole: the control compared EXIT CODES, so a test failing on both sides for different
reasons was "the venue's" — exactly the kill11 mis-attribution class (`5e28397`). Now
`ltp_control_cmd` says `venue` only when the test's TFAIL/TBROK lines (colours stripped, paths
and numbers folded) equal the ext4 run's; a different failure is OURS (fail early). The fail-early
watcher reads the test's lines off kirk's debug log; the post-run judgement off the JSON report.
Verified on today's data: swapon01/clock_settime03 SAME, init_module01/finit_module02 DIFFERENT.
Remaining environment differences between kirk and the control: kirk exports
`LTP_COLORIZE_OUTPUT=1` (stripped by the comparison) and appends `testcases/bin` to PATH where
the control prepends it; both run without a controlling tty.

## 4. The `by-design` citations

| row | cited | actually rules it | status |
|---|---|---|---|
| fstests 441, 484 | charter §1.8 | §1.8 fail-stop lattice, EIO until remount | sound |
| fstests 798 | "write-through" | §6.1 / §1 item 10 (the RAM ring is the write cache); no explicit "no FUSE_WRITEBACK_CACHE" sentence | weak — owner may add it |
| fstests 409–411, 589, 732 | "charter §5.5; decision 0002 row 7-class" | §5.7 rules the `--single-writer` class's refusal; 0002 row 7 is unrelated. §5.7 also says the DEFAULT format joins | corrected to §5.7; the posture itself is the owner's call (§1) |
| LTP epoll_ctl06 | "the kernel module's shape" | kernel source (cited above); no charter line | sound in substance; owner may record it in §3 |
| LTP fallocate03, mremap06 | "decision 0003" | nothing; the porting spec says MUST implement | **no ruling** — OWNER-CALL |
| LTP ioctl_loop05 | "LTP's tst_device" | LTP source + FUSE's anonymous `s_dev` | sound |
| LTP setxattr03 | "rung-4 op outside the must-implement set" | nothing; the porting spec lists ioctl as must-implement (GDS only in the reference) | **no ruling** — OWNER-CALL |

## 5. `docs/gates/flakes.md` (4 rows) against the 2026-10-22 train (`.gate-train/*.log`, 61 local runs since 2026-10-19)

| row | ledgered symptom's last sighting | seen 2026-10-22? | verdict |
|---|---|---|---|
| smoke "THE `-o ro` READER OF A LIVE SET" | 2026-10-14 (kamrui, the log-line variant); 0 of 60 six-wide locally 2026-10-17 | the row went red twice on `5b4e210` (2026-10-21T22:10/22:18) — a DIFFERENT assertion (`membership_grace_dead_rows` under the holder census's cost), fixed by `cb76e65` + `6a1f2fd`; green 3/3 on `eaa208a` and 3/3 on `3492203`. The ledgered symptom: not in any local log | **retirement candidate** (manual commit; no fix landed for the ledgered symptom — the owner's convention asks for one) |
| smoke "THE TWO-DAEMON MATRIX …" wedge | 2026-10-16 (VM gates) | no VM in `GATE_TRAIN_VENUES` on Linux; never on kamrui or local | **not judgeable here** — a coverage gap (the aarch64 VM is not in the train); stays |
| smoke "A DEAD MANAGER HOLDING A STRIPED HOT DIRECTORY" | 2026-10-16 (kamrui) | **YES**: `local-6a1f2fd402dc-20261022T000244Z.log` — rows (b) and (c) passed, then the test HUNG in `storm_b.deinit` → `child.kill` → `wait4`: the storm child parked in a FUSE request on B's mount that SIGKILL did not end; the hang watch aborted B's connection (1 of 12 train gates today) | **DEFECT to chase** (product: a request parked on the joined daemon past the reclaim); the row annotated |
| unit `meta.sym.handover_moves` "THE MOVE MATRIX" | 2026-10-17 (1 of 9 local gates) | no `HolderUnresolved` in any of the 83 train logs | **retirement candidate** (as above: no fix landed) |

## 6. Fix items (mechanisms)

- **F1 `fallocate` (LTP fallocate03, mremap06; 31 xfstests `_notrun` on `falloc`).** `lifecycle.zig:722` already dispatches `fs.vtable.fallocate`; `kv_fs` adds `fallocate` under the inode's EXCLUSIVE guard (`op_guard.Table.acquire(.exclusive)`, as truncate): mode 0 / `KEEP_SIZE` reserve `[off, off+len)` as allocated-zero blocks in the layout (the coverage-aware read already zero-fills uncovered bytes; the reservation must be charged against the volume's free space so a later write cannot ENOSPC inside it) and, without `KEEP_SIZE`, raise the size under the size epoch (`2250ca9`'s law); `PUNCH_HOLE|KEEP_SIZE` frees the covered blocks and zeroes the coverage; `ZERO_RANGE`, `COLLAPSE_RANGE`, `INSERT_RANGE` answer EOPNOTSUPP (legal). Red tests: a unit row per mode over the loopback harness; the LTP pair re-run. ~1–2 days; a full-tier change (layout semantics).
- **F2 inode flags (LTP setxattr03).** A `flags` word is already in the inode record (charter §5.1 `{…, flags, …, flags2}`); serve `FS_IOC_GETFLAGS`/`SETFLAGS` (+ `FSGETXATTR`/`FSSETXATTR`) through `kv_fs.ioctl` for `IMMUTABLE`/`APPEND` (`CAP_LINUX_IMMUTABLE` checked by the kernel before the ioctl reaches us), and refuse with EPERM in every mutating handler of an immutable inode (write, setattr, setxattr/removexattr, unlink, rename, link, open for write/trunc) and in the non-append writes of an append-only one. ~1 day; red: a loopback row per op.
- **F3 export handles (xfstests 426, 467, 477, 756, 777) — if the owner rules them in scope.** Advertise `FUSE_EXPORT_SUPPORT` (`codec.InitWants.export_support`, already plumbed); serve `LOOKUP(nodeid, ".")` for ANY inode kind (today `kv_fs.lookup` serves "." through `readDir`, directories only) and `LOOKUP(dir, "..")` from a DURABLE parent (today `parentOf` is a RAM memo filled at lookup; after a cycle-mount it is empty → ESTALE). The kernel's `fuse_get_dentry` (`fs/fuse/inode.c:1054`) does the rest. ~1 day.
- **F4 the DEAD MANAGER row's second shape** (§5): the storm child's parked request on B after the reclaim — the next sighting carries B's threads (the `printWedge` capture) — a product item for the recovery owner.
- **F5 generic/650 (CPU hotplug)**: the row's evidence is from THIS host on 7.2.8 (CPUs that never came back; nvme fabrics EXDEV); not re-run on 7.3.0 today because a failed re-test needs a reboot and the venue carries the release runs — the owner picks the moment.
- **F6 two fstests harness instances on one host fight** (the invalid first generic/795 run; the sibling worktree's generic/563 run met my helpers in return). Shared state: `/sbin/{mount,mkfs,fsck}.fuse.squeezefs` + `/sbin/umount.fuse` (re-pointed by every `fstests_build`), `/run/squeezefs-suites.env` (rewritten by every `fstests_sets`: `SQZ_BIN`, `SUITE_SETS_DIR`, now `SUITE_SINGLE_WRITER`), and the slice names `sqz-set-{test,scratch}-*` (keyed by the backing image's directory NAME). Mechanism: `check` already runs with `PATH=$SETS/bin:$PATH` for `mount.typed`, so run-scoped `mount`/`umount`/`mkfs`/`fsck` wrappers in `$SETS/bin` dispatching `-t fuse.squeezefs` (and our mountpoints) to THIS tree's helpers, plus `SQZ_SUITES_ENV=$SETS/suites.env` exported to `check` (the helpers honour it), make a run self-contained without `/sbin`; the slice names need a per-run key (the `.wt-small` branch is hashing them). Until then: one fstests harness per host at a time, and `scripts/venue` could take the `.gate.lock`-style flock for it.
- **F7 `alloc_trim_window_parks`** (generic/750 here; generic/475 in today's release `-g auto`): `enospc.zig:201` fires it from `awaitTrimReturn` — "≈ 0 except exactly that regime" (a full store under rewrite with its free list inside trim claim windows), i.e. a counted outcome by its own doc, declared as a must-stay-0 `Tripwire` in `block_alloc.zig:456`. With it come `syncfs: a leg failed (InputOutput)` ×235 and `pack refill: … no free block for the open pack` on the 2 × 8 GiB scratch under fsstress: a full data store answers EIO to syncfs where ENOSPC is the honest word. Owner's call on the classification (as the fence tripwires, 2026-10-19) and a product item on the EIO.
- **F8 a joiner's rename never resolves its slot move** (generic/732 under the symmetric default, §2.4): `move_gate` retried `SlotMoved` for 90 s and answered EIO; the directory's slot moved between the manager and the joiner on the first rename and the joiner's second rename could not find its holder. The recovery/handover owner's item; the smoke's "TWO MOUNTS OF ONE SET ON ONE HOST" row is the rig to extend (a rename of a file the other member created, both directions). **Fixed `f1d0d21`** (`rung-10-joiner-rename-home.md`): no handover was involved — the joiner wrote the intent of a foreign-step-0 rename into slot 0, the manager's; generic/732 passes under `SUITE_SINGLE_WRITER=0`; 409–411 and 589 then fail only on the umount helper's lines at a whole-tree bind/move of the joined mount (F6's area).

## 7. Commits on `audit/adjudications-100pct`

- `5130797` fix(suites): LTP's ext4 control runs from kirk's cwd and says `venue` only on the SAME failing assertions; the module testcases are rebuilt when the kernel changes; proc01 leaves the venue skip list.
- `800ee5b` fix(suites): the container-era xfstests exclusions pruned (ten pass; the exportfs five fail on the product and leave the list unadjudicated); the harness restores the host sysctls a test leaves changed; `SUITE_SINGLE_WRITER` reaches `mkfs`.
- the docs commit carrying this note: the two ledgers reworded with this host's evidence and the corrected citations (OWNER-CALL rows say so), the flakes rows annotated with today's sightings.

**What the next release `-g auto` meets because of this audit:** generic/426 (then 467, 477, 756,
777) are unadjudicated until the owner rules export handles out of scope or F3 lands; the
LTP ledger's fallocate03 / mremap06 / setxattr03 rows still pass the gate mechanically but
say OWNER-CALL. Not settled here: the suites' format posture (§1, the five `mount` rows);
`alloc_trim_window_parks`' class (F7); generic/650's re-test (F5); the TWO-DAEMON wedge's venue.
