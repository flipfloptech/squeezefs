# Rung 10 — the sqz kernel series against mainline (2026-10-21)

The owner's direction: know which kernel patches the daemon REQUIRES and which are performance;
the performance patches that are ours get MAINLINED; find out how much of the zero-copy base is
already upstream. Venue: the workstation (`local`, `7.2.8-9.33-omarchy-sqz`); mainline cloned to
`/build/kernel/linux-mainline` (torvalds, `v7.3` = `c03c1a14507d`, 2026-10-18 — no 7.4-rc yet;
master is the 7.4 merge window at `v7.3-1703-g…`, fs/fuse untouched there but for the idmap
const churn), `fuse/for-next` (`5a9745611d62`, 2026-10-20) fetched beside it. Worktrees:
`linux-mainline-v7.3` (pristine), `linux-fuse-next`, `linux-mainline-sqz` (the rebase, branch
`sqz/mainline-v7.3`).

## 1. The zero-copy base (0001–0021) in mainline

**The finding.** Joanne Koong's v4 series the daemon speaks (io_uring kernel-managed buffer
rings + FUSE kmbuf bufring + zero-copy, Jan 2026) was NOT merged in that form. Its v2
(2026-04-02, "fuse: add io-uring buffer rings and zero-copy", 14 patches) dropped the io_uring
kmbuf infrastructure entirely — "move logic into fuse … cleaner io-uring code, more flexible
fuse user configurations (mlock limits), easier setup" — and 7.3 landed FUSE-owned **buffer
pools**: `FUSE_IO_URING_CMD_ADD_QUEUE` (= 3), `FUSE_IO_URING_CMD_ADD_BUFPOOL` (= 4), a USER
memory pool per queue (optionally an io_uring registered buffer the server names by
`sqe->buf_index` on every REGISTER/COMMIT), `FUSE_URING_ZERO_COPY` as an ADD_QUEUE flag, the
zc slot chosen per entry (`ent_zero_copy_buf_index`), `FUSE_URING_ENT_ZERO_COPY`,
`FUSE_HAS_IO_URING_BUFPOOL` (INIT bit 43), minor 7.46 — and zero-copy **per open file**:
`FOPEN_IO_URING_ZERO_COPY` (bit 8) on the OPEN reply, `args->zero_copy` read from
`ff->open_flags` in `fuse_read_args_fill` / the write path. Two consequences: (a) the daemon's
BufRing and ZeroCopy modes talk a UAPI with no upstream future (its opcode numbers 3 and 4 even
collide with ADD_QUEUE/ADD_BUFPOOL); (b) under our zero-message opens (OPEN → ENOSYS, charter
§4) the daemon never sends an OPEN reply, so mainline's zero-copy is unreachable until either
the daemon serves OPEN or upstream grows a connection-wide default (an INIT flag) — a small
upstream patch worth proposing with the daemon's port.

| 7.2 series | mainline | commit |
|---|---|---|
| 0001 kbuf: refactor pbuf register · 0002 rename unregister_pbuf_ring · 0003 kmbuf rings · 0004 mmap · 0005 kmbuf in selection · 0006 pinning · 0007 recycling · 0008 `io_uring_fixed_index_get/put` · 0009 `io_uring_is_kmbuf_ring` · 0010 export `io_ring_buffer_select` · 0011 buffer id · 0012 cmd selected buffer index | **not in** (dropped by the author in v2; none of the symbols exist, `io_ring_buffer_select` still static, `io_unregister_pbuf_ring` unrenamed) | — |
| 0013 fuse: buffer copying for kernel addresses | **not in** (`fuse_copy_state` has no `is_kaddr`; the pool is user memory) | — |
| 0014 fuse: kmbuf buffer ring | **superseded** — buffer pools, different UAPI | `b45aaabc628b`, `96caf2496e15` (7.3) |
| 0015 rsrc: rename `io_buffer_register_bvec` · 0016 split `io_buffer_register_request` · 0018 add `io_buffer_register_bvec()` | **in** (+ `95961b72c57b` exports `IO_IMU_DEST/SOURCE`) | `51e08eaf954d`, `fbc32d5f44d3`, `bd62a2cfff9f` (7.3) |
| 0017 rsrc: release callback optional | **not in, unneeded** (mainline's API requires a release; fuse passes one) | — |
| 0019 fuse: zero-copy over io-uring | **superseded** — ADD_QUEUE flag + per-entry slot + `FOPEN_IO_URING_ZERO_COPY` | `43f8343858eb` (7.3) |
| 0020 hold page refs for zc registrations (ours) | **in, equivalent** — `fuse_zero_copy_release` folio_get/put through the imu release | part of `43f8343858eb` |
| 0021 docs | **superseded** | `767094250c6c` (7.3) |
| the FUSE-over-io-uring base | in since **6.14** (`24fe962c86f5`, `c090c8abae4b`, `4a9bfb9b6850` …); the 7.2.4–7.2.6 stable fixes the series composed with (`6330b1f61ed1` decoupled ring creation, `fd10f40af314` stack-bounced headers, `1f59015e9581`, `4ef7c8cc9894`) are all in 7.3 | — |
| FUSE_SYNCFS for privileged servers (our 0027 = the backport) | **in `fuse/for-next`**, 7.4 material, not in 7.3 (bit 44, minor 7.47) | `989db66818a4` |
| FUSE_REQUEST_TIMEOUT | in since 6.15 (7.43) | — |

## 2. Required vs performance — what the daemon does without each patch

The daemon REQUIRES exactly one kernel feature: FUSE-over-io_uring (6.14+, `fuse.enable_uring`
— charter §1.2, "mount fails if it cannot arm"; `session` e2e "EchoFs mounts over the ring — INIT
negotiated FUSE_OVER_IO_URING …"). Everything in the series is probed and degrades to a named
posture (charter §4: "the probe is the only degrade point"). On a mainline 7.3 kernel the
ladder reads: kmbuf absent (register 38 is `IORING_REGISTER_LAST` → EINVAL; 37 is
`IORING_REGISTER_BPF_FILTER`, whose `cmd_type` reads our `buf_size` low half ≠ 1 → EINVAL), so
`auto` mounts **UserEnts**; the FEATURES probe (cmd 4 = ADD_BUFPOOL there) meets no queue 0 yet →
EINVAL → 1 ring per queue; the retention probe is never sent off a zc session; our 24-byte
`fuse_uring_cmd_req` puts `init_flags`/`queue_depth` inside mainline's `padding[6]` and leaves
its union (offset 24) zero, so a UserEnts REGISTER is well-formed.

| patch | required? | without it the daemon … | the test |
|---|---|---|---|
| 0001–0014, 0019–0021 kmbuf + zc base | **performance** (BufRing/ZeroCopy) | mounts UserEnts: an arena, every payload copied, no zc, no retention, no fused reads; byte-exact, the laws close | `session` e2e "the stock stratum (`SQUEEZEFS_TEST_KMBUF_ABSENT`) — auto mounts UserEnts …; an explicit bufring/zc refused loud naming kmbuf"; `probes` "the lattice table", "the kmbuf ladder" |
| 0022 seam check | kmbuf-only error path | n/a | — |
| 0023 FUSE_TIME_LIMITS | **optional** (correctness at the edge) | does not echo the range; `s_time_min/max` stay the VFS defaults, far dates stay incore while the daemon clamps durable state (fstests generic/634 diffs — the standing adjudication) | `codec` "INIT reply echoes the time range iff FUSE_TIME_LIMITS (bit 62) was offered"; `probes` "FUSE_TIME_LIMITS: bit 62 offered is the probe"; `session` e2e TIME_LIMITS skips naming the feature |
| 0024 zc retention | **performance** (ACK-early) | probe reads absent → no RETAIN commits, `transport_probe_retention` 0 | `probes` "the retention verdict table: ENOTCONN/ENOENT present; EINVAL/EOPNOTSUPP/… absent"; the retention e2e rows skip |
| 0025 nvme host-scoped subsystems | **product-specific** (multi-writer fencing) | reads the param absent: "stock kernel … identities merge under one multipath head unless nvme_core.multipath=N"; a merged head is REFUSED (`refuse_merged`), never served | `initiator` "host-scoped parameter and admin_timeout read on the venue; absent elsewhere", "the ladder verdict …"; `multipath` "the serving shape of a head" |
| 0026 per-queue bg accounting | **performance** (no uapi) | unchanged behaviour; `fch->bg_lock` on every commit | — (kernel-internal) |
| 0027 FUSE_SYNCFS | **optional** (durability of `sync -f`) | declines/never offered → `syncfs(2)`/`sync` never reach it; fsync/fdatasync/fsyncdir are the durability points; `kv_fs` logs "SYNCFS NOT negotiated" | `codec` "INIT reply asks for FUSE_SYNCFS … iff the kernel offered it … and only beside the request timeout"; `kv_fs` "FUSE_SYNCFS is a per-file fsync for the whole mount …" |
| 0028 abort ends unclaimed requests | **fix** (kernel) | the daemon is unaffected; a STOPPED daemon holds `sync` callers in D state past `request_timeout` (measured 45 s +) | — (kernel; the repro is in the commit message) |
| 0029 short bulk allocation | **fix** (kmbuf registration) | under page-cache pressure the kmbuf region ENOMEMs after 5 retries → the typed `KmbufRingEnomem` refusal, the mount fails (never a silent downgrade) | `session` e2e "the kmbuf ring answering ENOMEM after every retry … fails the mount with the TYPED KmbufRingEnomem refusal" |
| 0030 several rings per queue | **performance** (reads 1.05–1.70×) | FEATURES absent → `geometry.ringsPerQueue` derives 1; an explicit `SQUEEZEFS_FUSE_RINGS_PER_QUEUE=2` fails the mount loud | `probes` "the FEATURES verdict table"; `geometry` "rings per queue: 3 where the FEATURES probe reads present …, 1 … wherever it reads absent"; `session` e2e "rings per queue" skips naming 0030 |
| 0031 cheaper round trip · 0032 shmem NOWAIT · 0033 large-folio user pages | **performance** (kernel-internal, booted 9.33, unmeasured) | unchanged behaviour; 0032 absent = the memfd `READ_FIXED` punts to io-wq (2.6 cores at 2.1 M reads/s) | — |

## 3. The rebase on v7.3 (`/build/kernel/linux-mainline-sqz`, `scripts/kernel/patches-mainline/`)

Carrying the dropped kmbuf series onto mainline was tried (`git am -3` of 0001: `io_create_region`
now takes a `user_struct`, not the ctx; 0014/0019 would land on a `dev_uring.c` that already has
the pool/zc design in the same functions) and NOT done: it would maintain a competing UAPI
beside upstream's for the same feature — the divergence the owner wants gone, and the author's
own abandoned branch. The mainline series carries what has a future there, bisectable, each
commit `W=1` clean (`fs/fuse/`, `io_uring/`, `mm/shmem.o`, `drivers/nvme/host/`):

- **0032 → 0001** applied clean (io_uring 7.3 still gates buffered async reads on
  `FOP_BUFFER_RASYNC`/`FMODE_NOWAIT` exactly as 7.2 did; checked in `io_uring/rw.c`).
- **0033 → 0002** context only (`fuse_read_args_fill` now sets `args->zero_copy` above the
  release helper); mainline's `fuse_copy_folio` maps per page within a multi-page descriptor and
  `fuse_uring_set_up_zero_copy` emits one bvec per descriptor, so the coalesced descriptors are
  consumed as on 7.2. `fuse/for-next`'s "fuse: enable large folios" (`b46e09667411`) leaves the
  `fuse_get_user_pages` loop per-page — the patch stands there too.
- **0029 → 0003** re-targeted: the kmbuf function is gone; `io_region_allocate_pages()` has the
  identical compound-then-bulk-then-fail shape, so every kernel-allocated region gets the fix.
- **0028 → 0004** re-expressed: the per-queue ledger credit (`fuse_uring_bg_uncharge`, 0026)
  does not exist on mainline, `fuse_dev_end_requests` → `fuse_request_end` settles the
  connection ledger; the task work's no-request arm also recycles the entry's pool buffer
  (mainline entries hold one from dispatch).
- **0023 → 0005** two UAPI hunks re-placed (bits 43/44 now sit between REQUEST_TIMEOUT and ours);
  bit 62 and the `init_out` layout unchanged, so the daemon's codec is untouched.
- **0027 → 0006** replaced by the upstream commit, `cherry-pick -x 989db66818a4` (one comment-order
  conflict with 0005). Drop when the base is ≥ 7.4-rc1.
- **0025 → 0007** one hunk re-placed by hand (`__nvme_find_get_subsystem()` gained
  `__must_hold`); the rest context.
- **0026, 0030, 0031: not carried.** 0026 and 0031 are written against the kmbuf `dev_uring.c`
  (0031's per-entry scratch is the v4 zc carrier; its part 4 is upstream as `fc60668db855`);
  0030's problem does not exist on mainline — the pool is user memory each ring registers at
  the same index, so N rings per queue already work; its FEATURES opcode is ADD_BUFPOOL there.
  A small upstream hardening remains possible (refuse a COMMIT from a ring other than the
  entry's, `-EXDEV`). 0024 and 0022 ride the kmbuf code.

## 4. Posted where (`scripts/kernel/upstream/`, the owner posts)

Four standalone postings, `checkpatch.pl --strict` clean, `To:`/`Cc:` from `get_maintainer.pl`,
`base-commit: c03c1a14507d`: `fuse-uring-abort-unclaimed/` (fuse-devel: Miklos; Cc Bernd,
Joanne), `io_uring-memmap-short-bulk/` (io-uring: Jens; Cc Pavel), `mm-shmem-nowait-read/`
(linux-mm: Hugh, Baolin, Andrew; Cc Jens, io-uring), `fuse-large-folio-user-pages/`
(fuse-devel: Miklos; Cc Joanne, Bernd). The `Signed-off-by` is the repo's identity — a GitHub
noreply address; the README says to replace it before posting. Owed: 0023 as an RFC with a
maintainer-allocated bit; the 0026/0030/0031 ideas re-expressed on the mainline UAPI once the
daemon speaks it; a "zero-copy without FOPEN on no_open connections" proposal.

## 5. The mainline kernel for this box

`scripts/kernel/build-host-kernel --local --mainline`: linux 7.3 from kernel.org (Torvalds'
signature) + `patches-mainline` + the fragment, pkgbase `linux-mainline-sqz`, pkgrel `1.7`,
Omarchy's CURRENT `config.x86_64` (omarchy-pkgs `31c30c8`, 7.2.8-10) as the seed through
`olddefconfig`, none of Omarchy's 194 patches (7.2.x), its PKGBUILD's build/package functions
verbatim; the `--prepare-only` pass applied all 7 at `--fuzz=0` and passed the checklist
(`7.3.0-1.7-mainline-sqz`, 6315 modules; a first dry pass read `1.33` from the wrong patch dir
— fixed). The install writes `/etc/limine-entry-tool.d/sqz-mainline-kernel.conf` with
`BOOT_ORDER="linux-omarchy-sqz, linux-t2, linux-omarchy, linux-omarchy-*, linux-mainline-sqz, *,
…"` so the entry sits BELOW the default (which stays `7.2.8-9.33-omarchy-sqz`); nothing reboots.
**Built and installed 2026-10-21 13:12–13:33 EDT** under the venue's gate lock and every slot
(`/build/kernel/mainline/linux-mainline-sqz{,-headers}-7.3-1.7-x86_64.pkg.tar.zst`, 158 + 329 MiB):
`pacman -Q` lists `linux-mainline-sqz 7.3-1.7` beside `linux-omarchy-sqz 7.2.8-9.33`; the UKI
`/boot/EFI/Linux/omarchy_linux-mainline-sqz.efi` exists; `/boot/limine.conf` reads
`default_entry: 2` → `linux-omarchy-sqz` (7.2.8-9.33), then `linux-omarchy`, then
`linux-mainline-sqz` (7.3.0-1.7-mainline-sqz), then the snapshots; the running kernel's
`/usr/lib/modules/7.2.8-9.33-omarchy-sqz` was backed up and restored by the hooks; `modinfo -k`
on the new tree shows `fuse.enable_uring` and `nvme_core.fabrics_host_scoped_subsystems`,
`CONFIG_FUSE_IO_URING=y`, the nvmet/null_blk modules from the fragment.

**Not verified here** (a boot is the owner's): that the box boots `7.3.0-1.7-mainline-sqz`
(Omarchy's config on a mainline base has not been booted on this hardware); that the daemon
mounts UserEnts there with `syncfs negotiated`, `time_limits=true`, `rings per queue 1`, and the
smoke's stock-stratum rows pass; the behaviour of the four re-expressed patches at runtime
(compile-proven only); 0031–0033's measurements on 9.33 remain owed from the handoff.

**Risks.** The kernel series the daemon's fast modes need (kmbuf/zc) is now a permanent fork
of a design upstream rejected: every stable bump re-composes ~3,000 lines against a
`dev_uring.c` that keeps moving (7.3's pool code occupies the same functions). The way out is
the daemon speaking mainline's pool/zc UAPI (ADD_QUEUE, ADD_BUFPOOL as a registered buffer,
per-entry slots) plus one upstream change for zero-copy under `no_open` — a full-tier change
(the FUSE contract) with a measurement plan, not this pass. Until then 7.2.8 stays the venue
kernel and the mainline entry is for the stock stratum and the upstream patches.
