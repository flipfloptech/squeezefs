# Kernel zc-write v2 — evidence note (2026-08-09)

Charter: `docs/design-zc-write-kernel-v2.md`. Branch `feat/kernel-zc-write-v2`.
Never pushed. Userspace halves (fuse3 / daemon) **not** touched.

## Series layout after this work

Both tracks (`docker/kernel-sqz/patches/` = 6.19.14 field, `patches-7.1/` =
7.1.6 local) are 29 patches:

| # | What |
|---|---|
| 0001–0024 | unchanged (kmbuf, FUSE refactors, bvec rsrc, Koong zc) |
| **0025 NEW** | abort-race: imu-held folio refs on zc registrations |
| 0026 | was 0025 (docs) |
| 0027 | was 0026 (sqz kbuf seam) |
| 0028 | was 0027 (`FUSE_TIME_LIMITS`) |
| **0029 NEW** | zc payload retention (COMMIT_RETAIN + RELEASE_PAYLOAD) |
| 0030 | **not built** — see §4.3 gate below |

Renumber verified: both series apply `patch -p1 --fuzz=0` on fresh
pristine tarball extracts (29/29). `build-kernel.sh` will fail loud on
any fuzz.

## Compile proofs

**7.1.6 dir-build** (`/tmp/sqz-kpatch-test/linux-7.1.6`, running
CachyOS config via `zcat /proc/config.gz` + `make olddefconfig`,
`make io_uring/ fs/fuse/`):

| Stage | exit | new warnings vs baseline |
|---|---|---|
| baseline (27-patch v1) | 0 | 0 |
| +0025 | 0 | 0 |
| +0029 | 0 | 0 |

Logs: `/tmp/sqz-kpatch-test/logs/{baseline,0025,0029}-7.1.log`.

**6.19.14 field track:** apply-clean (`--fuzz=0` 29/29 on fresh
`linux-6.19.14`). Full container RPM build (`docker/kernel-sqz/build.sh`)
is the acceptance compile — **user checkpoint**, not run here.

## UAPI collision audit (both trees, post-0029)

FUSE (`include/uapi/linux/fuse.h`) — **identical on both tracks**:

```
FUSE_IO_URING_CMD_RELEASE_PAYLOAD = 3     /* next after COMMIT_AND_FETCH=2 */
FUSE_URING_PAYLOAD_RETENTION      (1 << 2) /* init.flags; 0 and 1 taken */
FUSE_URING_COMMIT_RETAIN          (1 << 0) /* commit.flags; was unused */
fuse_uring_cmd_req size           24 B     /* BUILD_BUG_ON in dev_uring.c */
```

No `FUSE_URING_ZC_SELECTIVE` (bit 3 reserved for a future 0030).
io_uring register opcodes unchanged (6.19 KMBUF=37; 7.1 KMBUF=38 after
BPF_FILTER=37). Nothing io_uring-uapi-visible was added.

## §4.3 selective-zc (0030) — not built

The charter forbids authoring 0030 unless per-op arithmetic from a
counted 4 KiB fio row (armed vs disarmed, perf-annotate the two
`io_ring_submit_lock` sites in `io_buffer_register_bvec` /
`io_buffer_unregister`) prices the crossover **below** the smallest
payload armed mounts actually deliver (daemon `zc::hold_candidate()`
already class-gates writes at payload/2).

That row needs the v2 kernel **booted** (or at minimum a zc-armed mount
on the current 7.1.6-sqz v1 kernel plus a disarmed A/B). Booting is the
user checkpoint. **Filing the refusal-until-measured is the successful
outcome of this charter's 0030 slot** — the patch file does not exist.
If the boot-test 4 KiB row later prices 0030 in, it takes the next
number (still 0030 if nothing else lands).

## Probe extension

`docker/kernel-sqz/probes/kmbuf_smoke.c --fuse-rungs`:

1. **Negotiation** — FUSE_INIT on `/dev/fuse` then SQE128
   `FUSE_IO_URING_CMD_RELEASE_PAYLOAD` with `commit_id=~0`.
   `-ENOENT` = armed present; `-ENOTCONN` = opcode present, not armed;
   `-EINVAL`/`-EOPNOTSUPP` = pre-0029. On this box without a fuse mount,
   INIT returns EPERM and the rung **SKIP**s (printed). Boot-test plan
   re-runs it on the v2 kernel with a real mount.
2. **Retention round-trip** — SKIP until armed zc+retention queue +
   paged WRITE (COMMIT+RETAIN, `READ_FIXED` still samples pages, second
   RELEASE `-ENOENT`, RELEASE of a live commit `-EBUSY`).
3. **Abort-race** — SKIP until KASAN dir-build: arm zc, park
   `READ_FIXED`, SIGKILL daemon. Unfixed 0024 must splat; 0025 must not.

gcc `-Wall -Wextra -Werror` clean. Existing kmbuf ladder unchanged
(this box: 38/39 7.1-sqz PRESENT).

## Boot-test plan (deliver, do not execute)

**Order:** local 7.1.6-sqz-v2 first (CachyOS kernel manager +
`~/sqz-kmbuf-zc-7.1.6-v2.patch`), then field 6.19.14-sqz RPM via
`docker/kernel-sqz/build.sh` + one-shot grub. ELRepo 7.1.2 (field) /
previous CachyOS entry (local) stay the grub **default** until smoke
passes — SERIES.md one-shot discipline.

**Local (this box, `7.1.6-1-cachyos` manager):**

1. Concat already at `~/sqz-kmbuf-zc-7.1.6-v2.patch` (`cat
   docker/kernel-sqz/patches-7.1/00*.patch`). Stack after CachyOS
   patches; any FAILED hunk is a CachyOS-base drift — report the `.rej`,
   do not fuzz.
2. Boot v2 one-shot. `uname -r` must carry the sqz tag.
3. `probes/kmbuf_smoke` (kmbuf ladder 38/39 PRESENT) then
   `--fuse-rungs` against a live mount.
4. Mount squeezefs with over-uring + zc. Pin `fuse3_kmbuf_negotiated=1`
   / `fuse3_zc_negotiated` on the stats inode.
5. One armed fio row (existing zc write/read shape) — no retention yet
   (daemon half is Approach A's; ACK-after-CQE bit-identical).
6. **0030 gate row:** 4 KiB randread, zc armed vs `SQUEEZEFS` zc
   disarmed (or a kmbuf-only queue), `perf annotate` the two
   `io_ring_submit_lock` sites. File the crossover in this note's
   follow-up; build 0030 only if it clears the delivered-shape floor.
7. Retention RT + abort-race rungs once a throwaway userspace
   (or the daemon PR) can COMMIT+RETAIN / park READ_FIXED.
8. Rollback: power-cycle (one-shot grub reverts) or
   `grub2-reboot '<previous>'`.

**Field (`squeeze-test`, 6.19.14-sqz RPM):** same smoke after the
container build lands RPMs under `dist/kernel-sqz/`. One-shot
`grub2-reboot` the `6.19.14-sqz` BLS entry; 7.1.2-elrepo stays default.

## Manager concat

```
cat docker/kernel-sqz/patches-7.1/00*.patch > ~/sqz-kmbuf-zc-7.1.6-v2.patch
```

v1 concat `~/sqz-kmbuf-zc-7.1.6.patch` left in place.

## 0030 refusal arithmetic (placeholder)

Not measured this session. The slot stays empty on purpose.
