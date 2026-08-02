# sqz kernel — series manifest (base, message-ids, conflict resolutions)

The sqz kernel = **linux-6.19.14** (kernel.org stable, sha256
`cde8bf6739be4a0777fedbbba5330b8188c55680c45a922a4dfa289cbec6f185`)
+ the 26 patches in `patches/` + the client base config + `config-fragment`,
built `LOCALVERSION=-sqz` → `uname -r` = `6.19.14-sqz`.

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
4. **patch 26 (sqz-authored seam commit)** — the kmbuf register path
   (patch 03) inherits the pre-stable unchecked `io_buffer_add_list()`
   call. Extends the same stable error handling to
   `io_register_kmbuf_ring()` (free internal ring struct + buffers
   region + bl; bl not yet xarray-visible so no double-free). Commit
   message carries the full rationale.

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
