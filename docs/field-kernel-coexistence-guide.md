# Field kernel coexistence guide — MOFED + Lustre + the sqz kernel on Rocky 8.10

**Status: this is the BLOCKER for the field MPI-IO row**
(`docs/field-mpiio-runbook.md`). The field boxes are Rocky 8.10 and carry
a day job: Mellanox OFED (mlx5, 2×200GbE) and the Lustre client. The
multi-writer test stack wants newer-kernel features on specific roles.
This guide is the decision tree for making those coexist — including the
paths where the conflict never has to be solved at all. Facts below are
dated 2026-08-18; re-verify the two vendor matrices before committing to
Path B or C.

---

## 1. What actually needs which kernel (the requirements are SPLIT by role)

| Role | Hard requirement | Probe (truth over version strings) | Needs MOFED? | Needs Lustre? |
|---|---|---|---|---|
| **Storage nodes** (nvmet targets) | nvmet **Persistent Reservations** (mainline **v6.13+**, `drivers/nvme/target/pr.c`; some vendor trees backport) | the transient configfs probe in the runbook's Preflight 2 (`resv_enable` attribute exists) | Not for nvme-**tcp** serving (in-tree `mlx5_core` drives the NIC; no RDMA used) | Only if the node ALSO mounts/serves Lustre — check, don't assume |
| **Client** (aqs-37) | **FUSE-over-io_uring** (mainline v6.14+; the sqz kernels add the kmbuf extras the perf rows used) | `ls /sys/module/fuse/parameters/enable_uring` on the kernel it will boot for the test | Not for nvme-tcp or the test (no RDMA in this path) | Only for its day job — NOT for the test |

Two consequences worth internalizing before touching anything:

* **nvme-tcp does not use RDMA.** In-tree `mlx5_core`/`mlx5e` in any
  6.19/7.1-class kernel is *newer* than the driver in any shipping
  MOFED/DOCA and drives ConnectX NICs for plain TCP at full rate. MOFED
  is required only for the OFED/RDMA userspace, site tooling, and
  Lustre-over-IB (`ko2iblnd`). **If the test window can run without
  Lustre mounted, MOFED is not needed on the sqz kernel at all.**
* **Patch 0030 is irrelevant here** (user ruling 2026-08-18, recorded in
  `docs/design-mw-multipath-kernel.md`): co-located co-writers share the
  client's one fabric identity; real second clients bring their own.

## 2. The vendor matrices (why "just build the kmods" is not a plan)

* **MOFED → DOCA-OFED**: standalone MLNX_OFED ended Oct 2024 (LTS
  security fixes to ~Oct 2027); the current mechanism is DOCA-Host with
  **DKMS** as the rebuild path (`doca-extra` /
  `/opt/mellanox/doca/tools/doca-kernel-support` for packaging). The
  official Rocky **8.10 support row is the stock `4.18.0-553` kernel
  only**; NVIDIA's own docs say the custom-kernel tooling "does not
  support fully customized or unofficial kernels". A 6.19/7.1 sqz kernel
  is exactly that — DKMS may compile or may not, per rebase, forever.
* **Lustre client**: out-of-tree kmods, tested only against enterprise
  kernels. 2.16 tops out at 6.8-class (Ubuntu 24.04); **2.17
  (2025-12-29) tops out at the RHEL 10.1 6.12-class kernel**; mainline
  beyond ~6.12/6.14 needs master-branch patches (LU-18475 etc.) and real
  porting. **No released Lustre client has ever been tested on a
  6.19/7.1 kernel.** Building master against the sqz kernel is a porting
  project with a per-rebase tail, not a build step.

The honest reading: making MOFED + Lustre run *on* the sqz kernel
(Path C) is the worst of the four options. The tree below orders them.

## 3. The decision tree

### Path A — role isolation + reboot windows (RECOMMENDED; zero porting)

The conflict dissolves if the sqz kernel never has to host MOFED/Lustre:

1. **Storage nodes**: if a node's day job does not include Lustre/IB
   duty (probe: `lsmod | grep -E 'lustre|lnet|ko2iblnd|mlx5_ib'`,
   `systemctl list-units | grep -i lustre`, `mount -t lustre`), boot it
   on the **sqz kernel RPMs** (`docker/kernel-sqz/`, 6.19.x track —
   nvmet PR included) for the test epoch. Nothing else on a target node
   needs MOFED for nvme-tcp serving.
2. **Client**: boot the test window into the sqz kernel with **in-tree
   mlx5**, Lustre unmounted for the window; reboot back to the site
   kernel after. Sanity ladder on first boot: NIC link at 200Gb
   (`ethtool <if> | grep Speed`), a plain `iperf3` row between client
   and one node, then the runbook's Preflight 1/4.
3. If the client's **current** kernel already passes the
   `enable_uring` probe (it ran the 41.6 GiB/s-era rows on sqz-class
   kernels — check what it boots today), the client half of this path is
   already done.

Cost: reboot windows and a maintenance slot. Porting: none. This is the
only path with no per-rebase tail.

### Path B — one negotiated kernel for everything (the real coexistence fix)

If the boxes must run Lustre + MOFED + sqz **simultaneously** on one
kernel, pick the newest kernel all three tolerate and meet in the
middle: a **6.12-class EL10-alike** (DOCA supports RHEL 10.x 6.12;
Lustre 2.17 client is *tested* on RHEL 10.1's 6.12) with the sqz patch
set rebased onto it plus two feature backports:

| Backport | From | Size |
|---|---|---|
| nvmet PR (`target/pr.c`) | v6.13 | moderate, self-contained (nodes only — skip on the client kernel) |
| FUSE-over-io_uring (+ the sqz kmbuf extras) | v6.14 + sqz series | the heavy one (client only — skip on node kernels) |

Note the role split applies here too: the CLIENT kernel needs the fuse
backport but not nvmet PR; the NODES need PR but not fuse — two small
variant builds beat one maximal kernel. MOFED lands via DOCA DKMS
(6.12-class is inside its tested world), Lustre 2.17 client via DKMS or
source against the same tree. Cost: a kernel-team backport program and a
frozen kernel version; sqz features that later depend on >6.12
primitives would re-open the negotiation.

### Path C — DKMS-everything on the sqz 6.19/7.1 kernel (LAST resort)

DOCA-OFED DKMS + Lustre **master** source-built against the sqz kernel.
Both are explicitly outside their vendors' tested worlds; expect build
breaks at every sqz rebase and undebuggable vendor-side refusals. Only
justified if Paths A/B/D are all ruled out by site policy.

### Path D — a dedicated test client (sidestep on the client side)

Put the sqz client role on hardware (or a VM with NIC passthrough/SR-IOV)
that has no Lustre/MOFED duty. A VM is its own kernel and its own
natural fabric identity (the 0030 ruling's point), so nothing on the
host changes. Combine with Path A's node half. Cost: hardware/VM
plumbing; the 200GbE row then measures the passthrough path.

## 3b. Toolchain parity — required for EVERY path that builds modules (field-verified 2026-08-18)

The sqz kernels are built with **gcc-toolset-14** (the kernel's own
stamp: `gcc (GCC) 14.2.1 ... (Red Hat 14.2.1-11)`), and their kbuild
flags include GCC>=13.1 options (`-fmin-function-alignment=16`). Rocky
8.10's base compiler is GCC 8.5, so ANY out-of-tree build against
`/lib/modules/<sqz>/build` — DOCA-OFED DKMS, Lustre, anything — fails
with `unrecognized command line option '-fmin-function-alignment=16'`
until the matching toolset is on PATH (first field attempt hit exactly
this):

```
dnf install gcc-toolset-14
source /opt/rh/gcc-toolset-14/enable     # per-shell
# durable for dnf/cron-triggered DKMS rebuilds on this kernel:
echo 'source /opt/rh/gcc-toolset-14/enable' > /etc/profile.d/zz-gcc14-for-sqz-kernel.sh
```

rpm scriptlets inherit the invoking shell's environment, so `dnf
install/reinstall <mod>-dkms` from the enabled shell builds correctly;
the manual form is `dkms build <mod>/<ver> -k <sqz-kver> && dkms
install ...` from the same shell. The "compiler differs from the one
used to build the kernel" warning disappears once versions match.

Field triage notes from the first DOCA-OFED 3.4.0 attempt on
6.19.14-sqz (Rocky 8.10): install the CORE `mlnx-ofa_kernel-dkms`
FIRST and read its verdict before anything else — the profile's
`isert-dkms` aborts on `BUILD_DEPENDS: mlnx-ofa_kernel` when the core
is absent (a cascade, not a compile failure), and `xpmem`/`ucx-xpmem`/
`isert` are HPC-SHMEM/iSER extras needed by nothing in this program
(exclude them: `--exclude=xpmem\*,ucx-xpmem,isert-dkms`). The
`doca-ofed` meta-package reports "Complete!" even when every module
scriptlet failed — `dkms status` is the truth, never the rpm summary.
Secure Boot boxes additionally need the one-time
`mokutil --import /var/lib/dkms/mok.pub` + reboot enrollment or the
built modules will refuse to load.

## 3c. Lustre-build preflights (field-verified 2026-08-18)

* **`.config` in the devel tree**: DKMS/kbuild external builds need only
  `include/config/auto.conf` + `Module.symvers` (DOCA builds fine), but
  Lustre's configure demands the literal
  `/lib/modules/<sqz>/build/.config`, which the sqz kernel-devel tree
  does not ship — configure dies "Kernel config could not be found".
  Fix: `cp /boot/config-<sqz-kver> .../build/.config`, or
  `modprobe configs && zcat /proc/config.gz > .../build/.config`. No
  `make` needed after (the prepare artifacts already exist).
  *Residual for the kernel-sqz tree: the devel RPM should ship
  `.config` so this preflight dies.*
* **Configure line** (from the gcc-toolset-14 shell): `./configure
  --disable-server --with-linux=/lib/modules/<sqz-kver>/build` plus
  `--with-o2ib=/usr/src/ofa_kernel/default` ONLY on o2ib (IB/RoCE
  verbs) LNet sites — that path is the DOCA `mlnx-ofa_kernel` headers,
  the reason MOFED core must build first there. tcp-LNet sites omit it
  and MOFED leaves Lustre's build path entirely.
* **Version law**: master (or newest 2.17.x) only — the tested ceiling
  is 6.12-class kernels; 2.15/2.16 will not approach a 6.19 build.
  Compile-time kernel-API breaks past configure are the REAL Path-C
  verdict for the Lustre half; if they pile up, fall back to Path A/B
  rather than maintaining a per-rebase port.

## 4. The probe checklist that picks the path (run these first, ~10 min)

On each storage node:
```
lsmod | grep -E 'lustre|lnet|ko2iblnd|mlx5_ib' || echo "no lustre/IB duty"
mount -t lustre || true
# nvmet PR probe: runbook Preflight 2 (transient configfs resv_enable)
```
On the client:
```
uname -r; ls /sys/module/fuse/parameters/enable_uring && echo "fuse-over-uring PRESENT"
lsmod | grep -E 'lustre|lnet' || echo "no lustre mounted right now"
ofed_info -s 2>/dev/null || echo "no MOFED stack on this kernel"
ethtool <200g-if> | grep -E 'Speed|Link'
```
Decision rule: if the nodes show no Lustre/IB duty AND the client can
take reboot windows → **Path A, start today**. If simultaneous
coexistence is mandatory → **Path B**, and the deliverable to schedule is
the 6.12-class dual-variant backport build. C and D are fallbacks.

## 5. What happens after unblock

Nothing in the test procedure changes: `docs/field-mpiio-runbook.md`
Preflights 1–4, then `tests/cluster_reset_v5_mw.sh --dry-run` → the reset
→ the printed `SQZ_MWMATRIX_MOUNTS=… s11-mpiio` line. The row lands at
measured-real tier beside the local verdict
(`.benchmarks/2026-08-18-s11-mpiio-row.md`, 1.411×/2.273×).
