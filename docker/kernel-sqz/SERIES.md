# sqz kernel — series manifest (base, message-ids, conflict resolutions)

The sqz kernel = **linux-6.19.14** (kernel.org stable, sha256
`cde8bf6739be4a0777fedbbba5330b8188c55680c45a922a4dfa289cbec6f185`)
+ the 30 patches in `patches/` + the client base config + `config-fragment`,
built `LOCALVERSION=-sqz` → `uname -r` = `6.19.14-sqz`.

**v2 delta (2026-08-04 + 2026-08-09):** the 2026-08-04 scoping campaign
authored **0028** (was 0027: `FUSE_TIME_LIMITS`). The 2026-08-09
zc-write charter (`docs/design-zc-write-kernel-v2.md`) inserted
**0025** (abort-race folio refs) immediately after Koong 0024 and
appended **0029** (payload retention). Old 0025–0027 renumbered
0026–0028 (no hunk overlap; both tracks apply `--fuzz=0`). Selective
zc delivery (**0030**) was **not** built — the §4.3 gate row ran on the
booted 7.1.6-sqz-v2 kernel (2026-08-09): armed WINS the 4 KiB randread
A-B-B-A outright (+40.7 %/+27.8 %, both orders; register/unregister
~0.25 % of cycles) — **refusal FINAL on measurement** (evidence
`.benchmarks/2026-08-09-kernel-zc-write-v2.md` §0030).

## What was taken, exactly

**Joanne Koong, "[PATCH v4 00/25] fuse/io-uring: add kernel-managed
buffer rings and zero-copy"**, 2026-01-16, fetched via `b4 am` from the
lore thread (`t.mbox.gz` of the cover):

* Cover message-id: `20260116233044.1532965-1-joannelkoong@gmail.com`
  (patches N/25 are `20260116233044.1532965-{N+1}-joannelkoong@gmail.com`)
* Lore: <https://lore.kernel.org/all/20260116233044.1532965-1-joannelkoong@gmail.com/>
* Declared base (cover): commit `b71e635feefc` in the io-uring tree =
  mainline `b71e635feefc852405b14620a7fc58c4c80c0f73` (6.19-rc5 era).
* The series is **self-contained**: patches 01–12 are the kmbuf io_uring
  infrastructure, 13–19 the FUSE refactors + kmbuf ring, 20–23 the rsrc
  bvec-registration refactor, 24 the FUSE zero-copy, 25 docs.
* UAPI surface it adds: the `fuse_uring_cmd_req.init.flags` union
  (`FUSE_URING_BUF_RING` / `FUSE_URING_ZERO_COPY` + `queue_depth`;
  the series adds a "7.46" version-history comment but leaves
  `FUSE_KERNEL_MINOR_VERSION` at 45 — negotiation rides the init
  flags, not a minor bump),
  io_uring `IORING_REGISTER_KMBUF_RING=37` / `IORING_UNREGISTER_KMBUF_RING=38`,
  `IORING_OFF_KMBUF_RING`. It adds **no new Kconfig symbols** (rides
  `CONFIG_FUSE_IO_URING` + `CONFIG_IO_URING`).

## Why v4 and why base 6.19.14 (the "latest coherent revision" ruling)

The series lineage after v4 (surveyed on lore, 2026-08-01):

| Series | Latest | Cover message-id | FUSE consumer? |
|---|---|---|---|
| fuse/io-uring kmbuf + zero-copy | **v4, 25 patches, 2026-01-16** | `20260116233044.1532965-1-…` | **yes — the only revision carrying the FUSE-side patches** |
| io_uring: add kernel-managed buffer rings (split-out infra) | v3, 8 patches, 2026-03-06 | `20260306003224.3620942-1-…` | no (API evolved: `IOU_PBUF_RING_KERNEL_MANAGED` folded into pbuf reg; no reposted FUSE consumer) |
| io_uring: extend bvec registration (split-out infra) | v7, 4 patches, 2026-06-12 | `20260612184840.4058966-1-…` | no |

Taking the evolved split-out infra (kmbuf v3 / bvec v7) without a
reposted FUSE consumer would have required hand-porting FUSE core onto
a changed API — banned ("NEVER hand-hack FUSE core logic silently").
**v4 is the latest coherent revision**, so v4 it is, on the base family
it targets.

Base choice: the v4 series applies **cleanly (25/25, zero conflicts)**
on its declared base `b71e635feefc` (verified). The 7.1.x line was
tried first (client parity) and rejected honestly: 7.1.5 has rewritten
the exact files the series touches (`fs/fuse/dev.c` +257/-, `dev_uring.c`
+147/-, io_uring kbuf/rsrc/uapi heavily), and patch 1 already needed a
structural merge — continuing would have been silent FUSE-core hacking.
6.19.14 (the base's own stable line, EOL'd with .14) carries ≥6.17 mlx5
HDS (`ETHTOOL_RING_USE_TCP_DATA_SPLIT` in `en_ethtool.c`),
`mlx5e_queue_mgmt_ops`, and the full zcrx surface
(`IORING_REGISTER_ZCRX_IFQ`/`_CTRL`, `IORING_OP_RECV_ZC`) — everything
frontier cell 1 needs. The sqz kernel is a **frontier vehicle**, not the
client's daily driver; the ELRepo 7.1.2 kernel remains the permanent
grub default.

## Transplant onto 6.19.14: every conflict, honestly

`git cherry-pick` of the 25 patches onto v6.19.14 hit **3 conflict
sites** (6.19.y stable fixes landed after rc5) + **1 seam** found by
audit. Nothing in FUSE core conflicted — all 7 FUSE patches (13–19, 24)
applied clean.

1. **patch 01, `io_uring/kbuf.c` (`io_register_pbuf_ring`)** — stable
   made `io_buffer_add_list()` return int (xa_store failure) with a
   free-region+bl error path; the series refactors the function into
   helpers. Resolution: series structure kept, stable's return check +
   cleanup preserved in the refactored caller.
2. **patch 05, `io_uring/kbuf.c`** — (a) `io_should_commit()`: series
   adds the `bl` param + kernel-managed early-return; stable had
   switched the opcode test to `io_is_uring_cmd(req)`. Both kept.
   (b) `io_ring_buffer_select()`: stable added the
   `io_kbuf_commit()==false → REQ_F_BUF_MORE` incremental-consumption
   fix; series changes the `io_should_commit` call signature. Both kept.
3. **patch 20, `drivers/block/ublk_drv.c`** — stable moved the
   `io_buffer_unregister_bvec()` call earlier (before
   `ublk_need_complete_req()`); the rename patch edits the old location.
   Resolution: rename applied at the moved location, no re-insertion at
   the old one.
4. **patch 26 (docs; was 25)** — Koong "docs: fuse: add io-uring bufring
   and zero-copy documentation". Renumbered 0025 → 0026; no hunk overlap
   with new 0025 (docs-only vs `dev_uring.c`/`rsrc.c`/`cmd.h`).
5. **patch 27 (was 26; sqz-authored seam commit)** — the kmbuf register
   path (patch 03) inherits the pre-stable unchecked `io_buffer_add_list()`
   call. Extends the same stable error handling to
   `io_register_kmbuf_ring()` (free internal ring struct + buffers
   region + bl; bl not yet xarray-visible so no double-free). Commit
   message carries the full rationale. Renumbered 0026 → 0027 when
   0025 (abort-race) was inserted; no hunk overlap with 0025.
6. **patch 28 (was 27; sqz-authored, v2 — `FUSE_TIME_LIMITS`)** — no
   upstream original (the V2-CANDIDATES.md candidate-5 sketch, authored
   2026-08-04): `fuse_init_out` carves `time_min`/`time_max` i64s out
   of `unused[11]` (→ `unused[3]` placed FIRST so the i64s stay
   naturally aligned and the struct stays 64 bytes), init flag
   `FUSE_TIME_LIMITS (1ULL << 62)` (deliberately far above upstream's
   bit-42 watermark), kernel advertises it in `fuse_send_init`, and
   `process_init_reply` applies `sb->s_time_min/max` beside the
   existing `time_gran` block when the daemon echoes the flag with a
   nonzero `time_max`. Applies fuzz=0 on the fully-patched tree; zero
   overlap with the series' FUSE hunks (different functions). Design
   precedent: djwong's fuse-iomap `FUSE_IOMAP_CONFIG_TIME`.
   Renumbered 0027 → 0028 with the 0025 insert.
7. **patch 25 (NEW 2026-08-09; sqz-authored — abort-race folio refs)** —
   inserted IMMEDIATELY after Koong 0024. `io_buffer_register_bvec()`
   grows `release`/`priv` (patch 0022's optional-callback machinery;
   same shape as `io_buffer_register_request()`).
   `fuse_uring_set_up_zero_copy()` `folio_get()`s into a GFP_KERNEL_ACCOUNT
   carrier; the imu release callback puts them when the last rsrc node
   drops. Closes three windows: abort-without-unregister UAF, in-flight
   FIXED I/O across COMMIT, and the retention (0029) steady state.
   Teardown does **not** gain an unregister (no uring-cmd issue context).
   Files: `fs/fuse/dev_uring.c`, `io_uring/rsrc.c`,
   `include/linux/io_uring/cmd.h`. Zero overlap with 0026–0028.
8. **patch 29 (NEW 2026-08-09; sqz-authored — zc payload retention)** —
   Approach B ACK-early accelerator. uapi: `FUSE_IO_URING_CMD_RELEASE_PAYLOAD=3`,
   `FUSE_URING_PAYLOAD_RETENTION (1<<2)`, `FUSE_URING_COMMIT_RETAIN (1<<0)`;
   `fuse_uring_cmd_req` stays 24 bytes (union absorbs `commit.flags`;
   `BUILD_BUG_ON` in `dev_uring.c`). Arming requires ZERO_COPY.
   COMMIT+RETAIN parks the ent in `FRRS_RETAINED` (no unregister, no
   fetch, cmd pending). RELEASE: `-ENOENT` unknown / `-EBUSY` live
   un-retained / `0` then re-arm. Teardown drains retained (pr_warn on
   nonzero). Depends on 0025. FUSE values identical on both tracks
   (uapi collision audit 2026-08-09: opcode 3 and bits 2/0 free in
   6.19.14 and 7.1.6). 7.1 adaptations: `io_uring_sqe128_cmd`; cancel
   composes with 7.1's list_del+kfree AVAILABLE path. 6.19: `io_uring_sqe_cmd`;
   cancel moves RETAINED onto `ent_in_userspace` like AVAILABLE.

9. **patch 30 (NEW 2026-08-15; sqz-authored — nvme host-scoped fabric
   subsystems, opt-in)** — the multi-writer program's rung-5b fix
   (design note `docs/design-mw-multipath-kernel.md`; the rung-6 STOP
   finding, design-full-multi-writer §5.2): on `nvme_core.multipath=Y`
   kernels `__nvme_find_get_subsystem()` matches subsysnqn ALONE, so
   two co-located per-mount host identities merge under ONE multipath
   head whose round-robin voids per-mount PR fencing. New opt-in
   module param **`nvme_core.fabrics_host_scoped_subsystems`** (bool,
   default off, perm 0444 — boot-scoped so the subsystem match can
   never go asymmetric): fabric controllers group subsystems by
   `(subsysnqn, hostnqn)`; `struct nvme_subsystem` gains
   `host_scope[NVMF_NQN_SIZE]` (empty = unscoped — param off and PCIe
   are byte-identical to upstream); `nvme_global_check_duplicate_ids()`
   skips host-scoped SIBLINGS of one subsysnqn (they present the same
   target namespaces on purpose — without the skip the fabric arm
   refuses the second identity's namespace as a duplicate ID); new
   read-only `sqz_host_scope` subsystem sysfs attr (guest-validation /
   daemon observability). Connect-time dedup needs no change
   (`nvmf_ctlr_matches_baseopts` already compares the host pair);
   target-side PR state is per-namespace keyed by hostid, so host-side
   splitting cannot fork fencing truth. Files:
   `drivers/nvme/host/{core.c,nvme.h,sysfs.c}` — the series' first
   `drivers/nvme/` patch, zero hunk overlap with 0001–0029.
   **AUTHORING ORDER REVERSED (user ruling 2026-08-15)**: authored and
   compile-verified on the **7.1.6 track FIRST** (the locally-running
   kernel — `make LLVM=1 drivers/nvme/host/` with the running
   `7.1.6-1-cachyos-sqz` config on the fully-patched pristine tree,
   zero new warnings; full 30-patch series re-verified `patch -p1
   --fuzz=0` clean on a fresh extraction), then backported to 6.19.14.
   Backport adaptations are **offsets-only** (the touched regions are
   code-identical across the trees; 6.19's extra
   `subsys->awupf = …` line and `kzalloc` vs `kzalloc_obj` idiom sit
   outside every hunk — the adaptation table lives in the design
   note §4). BOOT-VERIFIED on this (6.19.14) track by rung 6b's qemu
   guest legs (2026-08-15 — `tests/run_mw_matrix.sh
   vm-hostscope-validate` both arms + `vm-multi-identity`; ledger in
   the design note §6); the 7.1 track stays compile-verified only (no
   host reboot on the critical path).
   Number-reuse note: "0030" was earlier planning shorthand for
   selective zc delivery, whose refusal is FINAL on measurement
   (2026-08-09) — that slot was never built, and this unrelated nvme
   patch takes the number.

`patches/` is the `git format-patch` export of the resolved transplant;
`build-kernel.sh` applies it with `patch -p1 --fuzz=0` (any regression
in the transplant fails loud at apply time).

## Config

Base: `config-base-7.1.2-1.el8.elrepo.x86_64` (the client's running
config, copied read-only 2026-08-01) → `make olddefconfig` on 6.19.14 →
`config-fragment` (each entry asserted in the final `.config`; the
assertion failing fails the build). `CONFIG_ULP_DDP` does not exist in
this tree (never merged upstream) — documented absence.

## Rebuild

```bash
docker/kernel-sqz/build.sh            # podman/docker, 16 cpus, nice
# artifacts: dist/kernel-sqz/*.rpm + config-6.19.14-sqz + SHA256SUMS
```
