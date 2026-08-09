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

## Local boot-test results (2026-08-09, v2 kernel BOOTED — usermode smoke)

Executed per the plan above on the local box, steps 2–6. Binary
`625c50d5` (`cargo build --release`, default features — this branch
touches only kernel patches/probes/docs, so the daemon is functionally
dev tip `c417ec27`). Venue: **tcp devsub** (nvmet-tcp on lo, 4× nullb
meta `nvme1-4n1` + 4× 8 GiB zram data `nvme5-8n1`), cache-less format,
armed default mounts. Instrument: fio 3.42 libaio `direct=1`;
substrate+instrument stated per the standing rule.

### Kernel identity (step 2)

* `uname -r` = `7.1.6-1-cachyos-sqz`, build stamp 2026-08-09 11:46 UTC
  (minutes after the v2 concat regen).
* **The honest discriminator — /proc/kallsyms carries both v2 symbols:**
  `fuse_zc_pages_release` (0025's imu release callback) and
  `fuse_uring_release_payload` (0029's RELEASE handler). A v1 build has
  neither. This is the v2 kernel.
* `kmbuf_smoke` ladder: **38/39 PRESENT (7.1-sqz track)**, unchanged.
  `--fuse-rungs` negotiation SKIPs unprivileged as documented (the
  probe's INIT rung needs to be its own toy daemon; step-7 deliverable).

### Negotiation + arm (steps 3–4)

Mount log: `FUSE-over-io_uring registered: queues=32 depth=32
payload_sz=1048576 … buffers=kmbuf-bufring+zero-copy kmbuf_ops=38/39
(7.1-sqz)`. Stats inode pins: **`fuse3_kmbuf_negotiated=1`,
`fuse3_zc_negotiated=1`**, `transport_queues=32`, `transport_q_depth=32`,
`transport_max_write=1048576`, `fuse_killpriv_negotiated=1`.
`SQUEEZEFS_FUSE_ZC=0` remount negotiates `buffers=kmbuf-bufring`,
`fuse3_zc_negotiated=0` — the A/B lever works on the v2 kernel.

### Armed write row (step 5) — the §3.6 bit-identical law holds

Placed-merge step0 shape verbatim (1 MiB seq write, 8 jobs × qd8 ×
nrfiles=4 × 512 MiB, 45 s), same venue/binary-class as the v1-kernel
red gate two days prior:

| | v1 kernel (step0, 2026-08-09) | **v2 kernel (this row)** |
|---|---|---|
| GB/s | 1.332 | **1.354** (+1.7 % — single runs, within venue noise: PAR) |
| `fuse3_zc_write_extract_bytes` / user | 100.0 % | **100.0 %** (61.17 GB, 58,336 extractions) |
| `fuse3_zc_write_direct_bytes` | 0 | 0 |
| `nt_copy_bytes` / user | 99.9 % | 99.9 % |
| amp / device bytes | 1.044 | **1.044** (63.87 GB) |

Tripwires all clean: `fuse3_zc_fallbacks=0`, `zc_slot_payload_skips=0`,
`invariant_tripwires=0`, `detached_task_panics=0`,
`data_dma_fence_refusals=0`, `write_pipeline_fence_drops=0`. Park
ledger CLOSED at quiesce (`transport_payload_leases=58,336` all
released, `transport_leases_outstanding=0`, parked/unparked 0/0).
`transport_lease_overlong=112` (max age 2.2 s) — the documented
loud-never-fatal saturation tripwire, consistent with the row's 700 ms
p99. **dmesg silent across the row** — 0025's per-folio get/put ran
under all 58 k zc writes with zero kernel complaints.

Correctness gate (bracket-rig shape): O_DIRECT+fsync 32 MiB
write→readback md5 both postures + the P0 `cp && sync` shape ×3 both
postures — **7/7 clean**.

### Abort-path live exercise (unplanned, evidentiary)

Every `squeezefs umount` this session took the SIGTERM-timeout →
direct-unmount → **kernel connection abort** path on a zc-armed mount —
exactly 0025's window 1 (teardown ends zc requests, no unregister).
Three such aborts, **dmesg clean after each** (no UAF splat, no
warnings). This is live-fire evidence, not the red-first proof — the
KASAN abort-race rung (unfixed-must-splat) stays deferred per the plan.
The SIGTERM-timeout itself reproduces on BOTH zc and non-zc mounts, so
it is a daemon/venue shutdown shape, not v2-kernel-attributable —
flagged for a separate look.

## 0030 refusal arithmetic — MEASURED, refusal FINAL

The §4.3 gate row: 4 KiB randread over a 4 GiB striped fileset (8 jobs
× qd8, 30 s measured after a 15 s warm pass), armed vs
`SQUEEZEFS_FUSE_ZC=0`, fresh mount per leg, **A-B-B-A** (aging store —
standing rule):

| leg | IOPS | clat p50 | p99 | zc engagement |
|---|---|---|---|---|
| armed (A1) | **411,210** | 87 µs | 946 µs | `fuse3_zc_replies` Δ=12.34 M ≡ inplace replies |
| disarmed (B1) | 292,243 | 108 µs | 1,253 µs | 0 |
| disarmed (B2) | 267,237 | 112 µs | 1,417 µs | 0 |
| armed (A2) | **341,418** | 101 µs | 1,139 µs | engaged |

Armed wins BOTH brackets (+40.7 % / +27.8 %); worst armed (341 k) beats
best disarmed (292 k). Per-site cost, flat system-wide perf on the live
armed row (kptr_restrict relaxed for the record, restored after):
`io_buffer_register_bvec` 0.02 %, `io_buffer_unregister` 0.02 %,
`io_kernel_buffer_init` 0.02 %, `mutex_lock`/`unlock` 0.03/0.02 %,
`fuse_zc_pages_release` 0.14 % (the 0025 reference tax made visible —
the largest new v2 symbol, priced and cheap), `io_import_fixed` 0.09 %.

**Verdict: there is no crossover to clear.** At the smallest payload
this transport delivers, zc-armed is FASTER than the kmbuf copy path
on this venue — the register/unregister machinery costs ~0.25 % of
system cycles while zc deletes the copy AND rides the in-place reply
arm (`fuse3_read_inplace_replies` ≡ `fuse3_zc_replies` on the armed
leg). A per-queue size floor (`zc_min_kb`) has nothing to elide.
**0030 is refused on measurement — the patch stays unbuilt.** Un-park
condition: a venue where small-op zc measurably loses (e.g. a
warm-tier-serve-dominated shape where the daemon's memcpy fast path
beats slot RW) — re-run this bracket there before reopening the slot.

### Retention round-trip — LIVE PASS ×5 (2026-08-09, second session)

The probe's toy-daemon extension landed (`kmbuf_smoke.c --fuse-rungs`
is now a real root-only FUSE daemon: private mount, classical INIT
echoing `FUSE_OVER_IO_URING`, one SQE128 io_uring per possible-CPU
queue — fuse pins bgid 0 of the ring each REGISTER rides and a second
pin refuses `-EALREADY`, so the shared-ring shortcut is structurally
illegal — sparse zc slot + headers fixed buffer at index 1 + kmbuf
ring per queue, REGISTER `BUF_RING|ZERO_COPY|PAYLOAD_RETENTION`
`queue_depth=1`, and a CPU-0-pinned child driving `FOPEN_DIRECT_IO`
writes). Green **×5 consecutive** on the booted `7.1.6-1-cachyos-sqz`
v2 kernel, dmesg oops-clean:

| rung | verdict |
|---|---|
| negotiation, pre-arm | `RELEASE` ⇒ **`-ENOTCONN`** (opcode present, no ring — the §3.5 middle arm, now measured) |
| negotiation, armed | impossible `commit_id` ⇒ **`-ENOENT`** |
| zc delivery | paged 8 KiB WRITE arrives on the sparse slot; `WRITE_FIXED` sample byte-exact |
| imu direction law | `READ_FIXED` against the `ITER_SOURCE` slot ⇒ `-EFAULT` |
| **ACK-early** | COMMIT+RETAIN parks the ent (no CQE) and `write(2)` returns — witnessed by pipe ordering |
| **post-ACK page liveness** | retained slot re-sampled byte-exact AFTER `fuse_request_end` — **0025's imu-held folio refs doing exactly their job** |
| RELEASE ladder | `0` → double ⇒ `-ENOENT` → live commit ⇒ `-EBUSY` |
| teardown drain | WRITE #2 deliberately leaked retained into connection death: no oops, request never strands, and the §3.3 `pr_warn` forensic line ("teardown with retained zc payloads") observed on every run |

Two latent probe bugs died in the rewrite: the SQE128 command area was
written at offset 64 (the kernel reads `sqe->cmd` at **48**; the zeroed
misread happened to also answer ENOENT, masking it), and the INIT
"handshake" wrote a request instead of reading the kernel's. The
`--fuse-rungs` SKIP paths (unprivileged, missing kernel) and the
ladder/`--signatures` modes are regression-checked unchanged.

**Still deferred:** the KASAN abort-race red-first rung (needs a KASAN
dir-build of unfixed-0024 vs 0025 — the toy daemon can now drive it:
arm zc, park a fixed-buffer op, SIGKILL) and the fuse3/daemon retention
lease (design §6 — Approach B's PR, not this track's).

### Field build (step for `squeeze-test`)

`docker/kernel-sqz/build.sh` completed 2026-08-09: series applied
`--fuzz=0` 29/29 in-container, RPMs at `dist/kernel-sqz/`
(`kernel-6.19.14_sqz-1` `19b08f15…`, `-devel` `12cf4b3e…`, `-headers`
`be506647…`, SHA256SUMS). Deploy + one-shot grub boot on `squeeze-test`
is the user checkpoint (7.1.2-elrepo stays default until the smoke
passes), then the same usermode smoke: probe ladder 37/38 → armed mount
negotiation pins → the step0 write row vs the 2026-08-06 6.19-v1
baselines.

**Field boot CONFIRMED (2026-08-10, user-run on `memp-s3ds-aqs-37`):**
the RPM booted and the probe passed end-to-end — kmbuf ladder
**CONFIRMED on the 37/38 rung (6.19-sqz track)** (the per-track opcode
split working as designed: the local 7.1 box resolves 38/39, the field
6.19 box 37/38, same binary), `--fuse-rungs` with `enable_uring=Y`:
negotiation pre-arm `-ENOTCONN` / armed `-ENOENT` (0029 kernel), 32
queues armed zc+retention, **retention-rt PASS** (zc delivery, dir law,
ACK-early, post-ACK page liveness, RELEASE ladder 0/ENOENT/EBUSY), and
**teardown-drain PASS** with the pr_warn forensic line observed. Both
tracks of the v2 series are now live-verified. Next: the field write
rows (the 2026-08-06 zc-write-bracket shapes) on the v2 kernel vs the
v1 baselines.
