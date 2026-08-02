# 2026-08-03 — The sqz custom kernel: build, Phase-0 HDS verdict, safe boot, capability matrix

Branch `feat/sqz-kernel` (off dev tip `f6532fd`). USER-AUTHORIZED
(2026-08-02, verbatim): "custom build the kernel via podman/docker
containers and make sure everything we need enabled is enabled. make
sure to also add a custom kernel build tag identifier 'sqz'". Charter:
unlock the two interface-frontier cells the 2026-08-02 census closed
(`.benchmarks/2026-08-02-interface-frontier.md` §2): (1) io_uring zcrx
+ mlx5 tcp-data-split, (2) the FUSE zero-copy reply ABI (in-flight
lore series, in no released kernel).

## 1. What was built (Phase 1 — dev box, containerized)

**`6.19.14-sqz`** x86_64 kernel RPMs, EL8-installable, from the
committed recipe **`docker/kernel-sqz/`** (Dockerfile + build scripts +
patches + config + probes — reproducibility is the deliverable;
`SERIES.md` there is the full manifest).

* **Base:** kernel.org stable **linux-6.19.14** (sha256
  `cde8bf67…c6f185`, pinned in `build-kernel.sh`).
* **Series:** Joanne Koong, *"[PATCH v4 00/25] fuse/io-uring: add
  kernel-managed buffer rings and zero-copy"*, 2026-01-16, fetched via
  `b4 am` from lore; cover message-id
  **`20260116233044.1532965-1-joannelkoong@gmail.com`** (self-contained:
  kmbuf infra 01–12, FUSE refactors + kmbuf ring 13–19, rsrc bvec
  refactor 20–23, FUSE zero-copy 24, docs 25). Declared base
  `b71e635feefc` resolved to mainline
  `b71e635feefc852405b14620a7fc58c4c80c0f73` (6.19-rc5 era) — the
  series applies there **25/25 clean** (verified).
* **The base ruling** (mission rule: "the base the SERIES applies to
  most cleanly"): 7.1.x was tried first for client parity and rejected
  honestly — 7.1.5 rewrote exactly the files the series rewires
  (`fs/fuse/dev.c` ±257, `dev_uring.c` ±147, io_uring
  kbuf/rsrc/uapi heavily); patch 1 already required structural
  FUSE/io_uring merges, i.e. silent core hacking. The series lineage
  after v4 splits the infra out (kmbuf v3 2026-03-06
  `20260306003224.3620942-1-…`, "extend bvec registration" v7
  2026-06-12 `20260612184840.4058966-1-…`) but **no FUSE consumer was
  ever reposted against the evolved API** — v4 is the latest coherent
  revision. 6.19.14 (the base's own stable line) carries the complete
  ≥6.17 mlx5 HDS + zcrx surface (`ETHTOOL_RING_USE_TCP_DATA_SPLIT` in
  `en_ethtool.c:2738`, `mlx5e_queue_mgmt_ops`,
  `IORING_REGISTER_ZCRX_IFQ`), so nothing is lost for cell 1. The sqz
  kernel is the frontier **vehicle**; ELRepo 7.1.2 stays the permanent
  default.
* **Transplant onto 6.19.14:** 3 textual conflict sites (6.19.y stable
  fixes vs the rc5 base) + 1 audited seam, all documented in
  `docker/kernel-sqz/SERIES.md` and carried as patch 0026 (the
  `io_register_kmbuf_ring` `io_buffer_add_list` return-check seam).
  **Zero FUSE-core conflicts** — all 7 FUSE patches applied clean.
* **Toolchain (pinned in the Dockerfile):** Rocky 8 container,
  gcc-toolset-14 (GCC 14.2.1), pahole v1.30 (from source — EL8 dwarves
  1.22 too old for BTF), rpm 4.14.3; `make -j16 INSTALL_MOD_STRIP=1
  binrpm-pkg`; podman `--cpus=16` + `nice` (dev-box manners; one build
  at a time).
* **Config:** client running config (`/boot/config-7.1.2-1.el8.elrepo.x86_64`,
  copied read-only) → `olddefconfig` → `config-fragment`. The ENABLE
  CHECKLIST is asserted in the final `.config` by the build (fail
  loud): **32/32 verified** —

| Checklist item | Final .config |
|---|---|
| `CONFIG_LOCALVERSION="-sqz"` (+`LOCALVERSION_AUTO=n`) | ✅ (uname = `6.19.14-sqz`) |
| `CONFIG_IO_URING=y` / `CONFIG_IO_URING_ZCRX=y` | ✅ (`def_bool y` in 6.19, asserted) |
| `CONFIG_FUSE_FS=m` / `CONFIG_FUSE_IO_URING=y` | ✅ (series adds NO new Kconfig symbols; surface = FUSE 7.46 uapi + `IORING_REGISTER_KMBUF_RING`) |
| `CONFIG_UDMABUF=y` (census: absent on ELRepo) | ✅ — devmem-TCP cell's dma-buf source closed |
| `CONFIG_NET_DEVMEM=y` / `CONFIG_DMA_SHARED_BUFFER=y` | ✅ |
| mlx5 full (`MLX5_CORE=m`, `CORE_EN=y`, `EN_ARFS=y`, `EN_RXNFC=y`, `CORE_EN_DCB=y`, `CLS_ACT=y`, `SW_STEERING=y`, `PAGE_POOL=y`) | ✅ |
| nvme-tcp + nvmet(+tcp,+loop) + null_blk + zram + brd (all =m) | ✅ |
| `CONFIG_ULP_DDP` | **does not exist in the tree** (never merged upstream) — documented absence, nothing to enable |
| BTF (`DEBUG_INFO_BTF[_MODULES]=y`) + `MODULE_SIG=y` parity | ✅ |

* Loaded-module coverage: client `lsmod` snapshot (90 modules) taken
  before the build; the config is the client's own config
  `olddefconfig`-forwarded, so coverage is inherited rather than
  reconstructed. (Verified at boot — §4.)

## 2. Phase 0 — the zcrx config-block hunt: RESOLVED, read-only, on the CURRENT kernel

The census suspect list ("HW-GRO off gating the attr, ELRepo config
omissions, or probe error") is adjudicated:

* **Code-level fact** (from the 7.1.5 tree, same 7.1.x lineage as the
  client's 7.1.2): mlx5 registers
  `supported_ring_params = ETHTOOL_RING_USE_TCP_DATA_SPLIT` — the HDS
  control surface EXISTS in the ELRepo build. The GET-side attr is
  emitted only when `dev->cfg->hds_config != 0` (unknown), and the
  mlx5 SET-side refuses ENABLED while `NETIF_F_GRO_HW` is off
  (`en_ethtool.c`: "TCP-data-split is not supported when GRO HW is
  disabled"). So the census's "ATTR NOT REPORTED" was the **unset
  hds_config state**, with HW-GRO-off (census posture) blocking any
  enable — *not* a missing driver surface and not an ELRepo config
  omission.
* **Field verdict (2026-08-01, read-only probes only — no NIC state
  was changed by this campaign):** since the census, the box's ethtool
  was upgraded to 6.15 and **HW-GRO is now ON on both fabric ports**;
  both the rebuilt genetlink probe (`hds_query`, recreated in
  `docker/kernel-sqz/probes/` — the original was lost in a
  re-provision) and native `ethtool -g` report **`tcp-data-split: on
  (enabled)` on ens1f0np0 AND ens2f0np0** on the running
  `7.1.2-1.el8.elrepo` kernel. `zcrx_smoke surface` on 7.1.2: ENODEV
  at netdev lookup ⇒ zcrx opcode+path present (as censused).
* **Consequence:** the zcrx lane is **unblocked on 7.1.2 without the
  new kernel** — frontier rank 2's driver-block is LIFTED (the §2.1
  probe gate condition "a future build reports this attr" is met by
  the live config state). The sqz kernel remains the FUSE-zc vehicle.
* One probe fix landed during verification: the recreated hds_query's
  family-resolve left a stale netlink ACK queued (GET then parsed the
  ACK, printing "ACK" instead of attrs). Fixed (no `NLM_F_ACK` on
  resolve/GET), verified against ethtool 6.15 output on both ports.
* `kmbuf_smoke` on 7.1.2: `REGISTER_KMBUF_RING` EINVAL ⇒ FUSE-zc
  surface ABSENT on the stock kernel (the before-cell of the matrix).

## 3. Boot discipline (Phase 2 plan of record)

SecureBoot disabled (verified), EFI + BLS entries, `GRUB_DEFAULT=saved`
with `saved_entry=…7.1.2-1.el8.elrepo` — the sqz entry boots via
**`grub2-reboot` one-shot**; a failed boot self-reverts on power cycle
(the USER must power-cycle if the box wedges — agents cannot).
Pre-existing landmine found and journaled: the client grubenv carried a
**stale `next_entry=1`** (a leftover one-shot that would have booted a
non-ELRepo entry at the next power cycle); the Phase-2 sequence
overwrites and then clears it. `/boot` 172M free (vmlinuz+map+initramfs
≈ 75M — fits); root fs 1.8G free (stripped module set fits; RPMs staged
under `/scratch/tmp/kernel-sqz/`). nvme-tcp topology snapshot (24 live
controllers, 12 subsystems × 2 paths) captured before any mutation;
reconnect is the product verb (`squeezefs nvmeof connect`, per
`cluster_reset_v3.sh` step 3).

## 4. Phase 2 — boot + capability matrix (gate observed, then executed 2026-08-02 03:46–03:57 Z)

**Gate:** the il hold-probe campaign journaled SESSION END 02:13:13 Z
(blocker-measured, venue untouched — the dd6c7ea pair refuses the
pre-bit-6 store loud; its field bracket is owed at the next reformat
window) and the sibling zcrx-lane session ENDed 02:54:04 Z (verdict GO:
zcrx live on 7.1.2, −65 % CPU at 49.5 GB/s line rate — independent
corroboration of §2). No open session remained; Phase 2 proceeded.

**Sequence (all journaled live):** RPM staged + sha-verified →
standing mount cleanly unmounted → `rpm -ivh` (modules 724 M,
initramfs 61 M — `kernel-install` **flipped the grub default to sqz;
caught and restored** to 7.1.2 before reboot, then `grub2-reboot` set
the one-shot in the right order: set-default clears next_entry, so
one-shot LAST) → reboot → **up in ~80 s on `6.19.14-sqz`**, one-shot
consumed (`next_entry=` empty), permanent default still 7.1.2, no
error-class dmesg beyond cosmetic (SELinux runtime-disable notice,
i801 SMBus busy).

**The capability matrix (retained on-box:
`/scratch/tmp/kernel-sqz/matrix-{before-7.1.2,after-6.19.14-sqz}.txt`):**

| Cell | 7.1.2-1.el8.elrepo (before) | 6.19.14-sqz (after) |
|---|---|---|
| mlx5 `tcp-data-split` attr | **on (enabled)** — reported once ethtool 6.15 + HW-GRO on (§2) | **on (enabled)** after `rx-gro-hw on` + RINGS_SET on BOTH ports (fresh-boot defaults off/unset; done in the pre-reconnect zero-traffic window) |
| zcrx opcode surface | present (ENODEV at netdev walk) | present |
| **zcrx real queue bind** | not attempted (live fabric traffic) | **`REGISTER_ZCRX_IFQ` SUCCEEDED** — ens1f0np0 ifidx 6 rxq 31, rq_entries 64, registered + unbound clean. **Cell OPEN end-to-end.** |
| kmbuf / FUSE-zc io_uring surface | ABSENT (`REGISTER_KMBUF_RING` EINVAL) | **PRESENT — register SUCCEEDS**; kallsyms carry `io_register_kmbuf_ring` (+4 kmbuf syms) and the series' FUSE side (`fuse_uring_select_buffer` etc.; fuse_uring syms 50 → 68) |
| FUSE zc-reply negotiation face | — | `fuse_uring_cmd_req.init.flags` (`FUSE_URING_BUF_RING` / `FUSE_URING_ZERO_COPY`) + `queue_depth`; note: v4 leaves `FUSE_KERNEL_MINOR_VERSION` at **45** (the 7.46 entry exists only in the version-history comment) — negotiation rides the init flags, not a minor bump |
| storage modules (nvme_tcp/nvmet/nvmet_tcp/null_blk/zram/brd) | present | present |

**Daemon proof (bit-identical standing pair `109d7bc`, nothing new
deployed):** fabric reconnected via the product verb (12 subsystems ×
2 paths, **24/24 live**, iopolicy round-robin, device numbering
identical to the pre-reboot snapshot) → mount armed first try —
`FUSE-over-io_uring registered: queues=32 depth=32 payload_sz=1048576`,
"session path armed (ready=true)", "transport armed for this session",
classical sideband servicer armed; `enable_uring` auto-set Y. Stats
inode: `transport_queues 32 / q_depth 32 / payload 1 GiB /
max_background 256`; **tripwires all zero**
(`ipc_sessions_poisoned`, `ipc_descriptor_rejects`, `fsck_findings`,
`writer_guard_fenced`, `write_pipeline_fence_drops`);
`fuse_killpriv_negotiated=1`. Smoke rows (SMOKE label — the new 5-wide
epoch, not comparable to reset-v4): kernel-path fio libaio 1M qd8 nj4
read **18.6 GB/s, err=0** (30 s); il lane `LD_PRELOAD` dd 512×1 MiB
O_DIRECT **6.3 GB/s** with `ipc_ops_read` delta **exactly 512** —
engagement exact. Store READ-ONLY all session (zero data-plane writes).

## 5. Client state

Box **left running `6.19.14-sqz`** with the standing pair mounted and
armed (any power cycle reverts to the ELRepo 7.1.2 default;
`grub2-reboot '…6.19.14-sqz'` re-enters the sqz kernel). NIC posture on
-sqz: rx-gro-hw + tcp-data-split ON both ports (fresh-boot defaults of
THIS kernel are off — an ELRepo boot re-applies its own defaults, so
nothing to restore there). Artifacts retained:
`/scratch/tmp/kernel-sqz/` (RPM + SHA256SUMS + config + before/after
matrices + arm-proof mount log + probe sources & binaries) on the
client; `dist/kernel-sqz/` (kernel/devel/headers RPMs + config +
SHA256SUMS) on the dev box; recipe committed under `docker/kernel-sqz/`.
Journal: SESSION START/END + boot/toggle/reboot lines in
`/scratch/tmp/agent_runs.log`. Riders flagged for the operator: client
root fs at 94 % after the 724 M module install; a stale grubenv
`next_entry=1` predating this campaign was consumed by the sequence
(saved_entry was and is the ELRepo kernel).

## 6. What flipped OPEN, and the recommended follow-on order

1. **zcrx initiator-lane prototype first.** The RX-copy cell is open on
   BOTH kernels (7.1.2 needed only the HW-GRO+HDS toggles — §2), the
   zcrx-lane sibling campaign already counted the win shape on 7.1.2
   (−65 % CPU at 49.5 GB/s; raw ceiling is 75–80 % RX-copy per the
   frontier census), and the real ifq bind is proven on -sqz. This lane
   does not depend on the sqz kernel, so it can ship against the fleet
   kernel — highest value, lowest coupling.
2. **fuse3 zc-reply adoption second** (needs the sqz kernel): the
   surface is live (kmbuf ring registers; init flags present) but the
   series is unmerged and its infra is still iterating upstream (kmbuf
   v3, bvec v7) — prototype the fork's `FUSE_URING_BUF_RING`/zc arm
   behind a knob on -sqz, and expect a rebase when the FUSE consumer is
   reposted upstream. The upstream benchmark prior (+20–25 % randread
   at 1 M) is the planning number; the K1 commit-copy term it deletes
   is ≈ 32 % of kern-row client cycles (census §3 Row A).
3. `CONFIG_UDMABUF=y` also flipped the devmem-TCP precondition cell on
   -sqz (census: absent) — no consumer yet; noted for the day a
   host-RAM dma-buf placement experiment is chartered.
