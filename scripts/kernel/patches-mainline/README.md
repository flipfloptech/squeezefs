# scripts/kernel/patches-mainline — the sqz series on mainline

The sqz kernel series re-expressed on mainline **linux v7.3** (`c03c1a14507d`, 2026-10-18), the
input of `scripts/kernel/build-host-kernel --local --mainline` (pkgbase `linux-mainline-sqz`,
a second boot entry below the sqz Omarchy kernel). `docs/evidence/rung-10-kernel-mainline.md`
is the reasoning; `scripts/kernel/upstream/` holds the posting-ready form of the first four.

What is here and what is not (the 7.2 series is `patches-7.2/`, 0001–0033):

| mainline | 7.2 series | status |
|---|---|---|
| 0001 `mm/shmem: honour IOCB_NOWAIT …` | 0032 | unchanged; posted to linux-mm |
| 0002 `fuse: coalesce the contiguous pages of a folio …` | 0033 | context only; posted to fuse-devel |
| 0003 `io_uring/memmap: finish a short bulk allocation …` | 0029 | re-targeted at `io_region_allocate_pages()` (mainline has no kmbuf rings); posted to io-uring |
| 0004 `fuse: io-uring: end requests a stopped server never claimed on abort` | 0028 | re-expressed (no per-queue ledger on mainline); posted to fuse-devel |
| 0005 `sqz: fuse: FUSE_TIME_LIMITS …` | 0023 | sqz-private flag (bit 62) kept; the daemon speaks it |
| 0006 `fuse: allow FUSE_SYNCFS for privileged userspace servers` | 0027 | the UPSTREAM commit `989db66818a4` from `fuse/for-next` (7.4 material), cherry-picked `-x`; drop it when the base is ≥ 7.4-rc1 |
| 0007 `sqz: nvme: host-scoped fabric subsystems (opt-in)` | 0025 | product-specific; one hunk re-placed (`__must_hold`) |
| — | 0001–0014, 0019–0022, 0024 | the kmbuf + zero-copy base and what rides it: NOT carried — upstream replaced it with FUSE buffer pools (`FUSE_IO_URING_CMD_ADD_BUFPOOL`, 7.3), a different UAPI; the daemon runs **UserEnts** on this kernel |
| — | 0015–0018 | in 7.3 (`51e08eaf954d`, `fbc32d5f44d3`, `bd62a2cfff9f`; 0017 unneeded) |
| — | 0026, 0030, 0031 | FUSE-uring performance patches written against the kmbuf code: wait for the daemon's port to the mainline UAPI (0031's attr_version part is upstream: `fc60668db855`) |

Regenerate after a change on the branch: `git -C /build/kernel/linux-mainline-sqz format-patch
--no-signature -o scripts/kernel/patches-mainline v7.3..sqz/mainline-v7.3` (then this README's
table). Every patch applies at `--fuzz=0` and compiles `W=1` clean at its own commit
(`fs/fuse/`, `io_uring/`, `mm/shmem.o`, `drivers/nvme/host/`).
