# docker/kernel-sqz — the sqz custom kernel recipe

USER-AUTHORIZED program (2026-08-02): containerized custom kernel build
carrying the `sqz` tag, unlocking the two interface-frontier mechanisms
the 2026-08-02 census (`.benchmarks/2026-08-02-interface-frontier.md`)
found closed on the field kernel:

1. **io_uring zcrx + mlx5 tcp-data-split (HDS)** — in-tree since 6.17
   for mlx5/CX-7 (no MOFED).
2. **The FUSE zero-copy reply ABI** — Joanne Koong's in-flight lore
   series (kmbuf + FUSE-zc), not in any released kernel. Exact
   message-ids, versions, base commit, and every conflict resolution:
   **`SERIES.md`** (the manifest — read it first).

Result: **`6.19.14-sqz`** EL8 kernel RPMs (Rocky 8.10 installable),
built in a pinned Rocky 8 + gcc-toolset-14 + pahole v1.30 container.

## Contents

| Path | What |
|---|---|
| `Dockerfile` | pinned EL8 build image (gcc-toolset-14, from-source pahole v1.30 for BTF) |
| `build.sh` | host wrapper: image build + capped (`--cpus 16`, `nice`) kernel build; artifacts → `dist/kernel-sqz/` |
| `build-kernel.sh` | in-container: sha256-pinned tarball → 26 patches → config assembly → checklist assertion (fail loud) → `make binrpm-pkg` |
| `SERIES.md` | the series manifest: message-ids, base ruling, conflict resolutions |
| `patches/` | `git format-patch` export of the resolved transplant (25 series patches + 1 sqz seam commit) |
| `config-base-7.1.2-1.el8.elrepo.x86_64` | the field client's running config (the base; copied read-only 2026-08-01) |
| `config-fragment` | the ENABLE CHECKLIST — every entry asserted in the final `.config` |
| `probes/` | capability probes (see below) |

## Probes (`probes/`)

All gcc-8.5-clean, no libnl/liburing — they compile on the field box.

* `hds_query.c` — ethtool-netlink `RINGS_GET`/`RINGS_SET` for
  `tcp-data-split` + `hds-thresh` (the box's pre-6.15 ethtool-era
  probe; GET is read-only, `set on|off` is the Phase-0 arm).
* `zcrx_smoke.c` — `IORING_REGISTER_ZCRX_IFQ` smoke: `surface` mode is
  read-only-safe (valid args, if_idx=0 → ENODEV/EPERM ⇒ opcode
  present); `bind <ifidx> <rxq>` does a real queue bind (restarts the
  rx queue — flagged, not default).
* `kmbuf_smoke.c` — `IORING_REGISTER_KMBUF_RING` (=37) with valid args:
  success ⇒ the FUSE-zc io_uring surface is present (the sqz kernel);
  EINVAL ⇒ absent (stock kernels).
* `capability_matrix.sh` — the before/after matrix runner (kernel id,
  HDS attr, HW-GRO, zcrx surface, kmbuf surface, fuse_uring kallsyms,
  storage modules).

## Build

```bash
docker/kernel-sqz/build.sh            # podman or docker
# → dist/kernel-sqz/kernel-*.rpm + config-6.19.14-sqz + SHA256SUMS
```

## Install on the field client (safe-boot discipline)

The ELRepo 7.1.2 kernel stays the **permanent grub default**; the sqz
kernel boots as a **one-shot** so a failed boot self-reverts on power
cycle (the operator power-cycles; agents cannot):

```bash
rpm -ivh --oldpackage kernel-6.19.14_sqz-1.x86_64.rpm   # installs modules + BLS entry
grub2-reboot '<the 6.19.14-sqz entry>'                   # ONE-SHOT; default untouched
reboot
# after boot: uname -r == 6.19.14-sqz; run probes/capability_matrix.sh
```
