# Rung 10 — Linux ≥ 7.3 only: the 7.2 series and every transport rung but the buffer pool removed (2026-10-29; decision 0003 §CT)

The owner's ruling (2026-10-29): "since everything is tested on kernel 7.3 and we know a lot of good is in
7.3 vs 7.2, remove all backwards compatibility to 7.2 and the kernel patches etc that were there. We will
focus on being 7.3+ compatible only." Branch `chore/drop-7.2` off `85d6ddf`, `7.3.0-1.7-mainline-sqz`.

**The minimum kernel and its enforcement.** Linux ≥ 7.3 with FUSE-over-io_uring buffer pools (uapi 7.46,
`CONFIG_FUSE_IO_URING=y`, `fuse.enable_uring=Y`, which the daemon writes before `mount(2)`). The INIT
offer's `FUSE_HAS_IO_URING_BUFPOOL` (bit 43) is the one gate: absent ⇒ `Session.mount` fails with
`error.BufpoolAbsent` before any ring exists, and `squeezefs mount` exits 1 with "the FUSE session could
not arm at <mountpoint>: the kernel (<release>) offers no FUSE-over-io_uring buffer pools — its INIT
carried no FUSE_HAS_IO_URING_BUFPOOL; SqueezeFS needs Linux >= 7.3 with CONFIG_FUSE_IO_URING=y and
fuse.enable_uring=Y". The seam `SQUEEZEFS_TEST_FUSE_BUFPOOL_ABSENT` proves it on a 7.3 venue (the
session's "e2e: the buffer-pool gate" row, the smoke's late-refusal row). `scripts/dev/provision-local-venue`
checks `uname -r` ≥ 7.3 and `fuse.enable_uring`; no `sqz` suffix is required.

**Removed.**

| area | what | lines |
|---|---|---|
| kernel | `scripts/kernel/patches-7.2/` 0001–0033; `build-venue-kernel` (arm64 container); `config-fragment-container-arm64.delta`; `build-host-kernel`'s Debian 7.2.8 `.deb` and Omarchy-PKGBUILD + series tracks (the mainline track alone remains, first in Limine); the 7.2-only `config-fragment` options; the suites' kmbuf `ENOMEM` retry loops | −7,643 / +134 (series 7,276) |
| daemon | `transport/kmbuf.zig` (261); the lattice (`geometry.BufferMode`), `BufRing`/`ZeroCopy`/`UserEnts`, the kmbuf ladder and `FEATURES`/retention probes, payload retention (`Retained`, `replyRetain`, `RELEASE_PAYLOAD`), the readdir latch, `PayloadClass`, multi-queue drain groups, `uring128`'s buffer-ring registration, the overlay's ACK-early continuation | src −3,729 / +760 |
| knobs | `SQUEEZEFS_FUSE_BUFFER_MODE`, `_FUSE_DRAIN_GROUP`, `_ZC_ACK_EARLY`, `_ZC_ACK_EARLY_ODIRECT`, `_TEST_KMBUF_ABSENT`, `_TEST_MAINLINE_POOLS`, `_TEST_KMBUF_REGISTER_ENOMEM` (added: `_TEST_FUSE_BUFPOOL_ABSENT`) | 7 rows |
| gauges | 28 names: `transport_buffer_mode`, `transport_drain_group{s,_width}`, `transport_kmbuf_*` (5), `transport_probe_{bufpool,kmbuf,retention,rings_per_queue}`, the retention family (`transport_zc_retain*`, `_release*`, `transport_retained_*`, `transport_releases_requested`), `transport_zc_readdir_latch`, `overlay_ack_early_*` (4) | — |
| tests | 53 unit-test names gone or rewritten, 24 rewritten for the pool (net −29); cost rows of the gone tests dropped | — |

Per file (lines before → after): `session.zig` 5,312 → 4,668; `worker.zig` 4,414 → 3,917; `probes.zig`
671 → 184; `zc.zig` 609 → 437; `geometry.zig` 1,210 → 870; `abi.zig` 1,151 → 1,023; `uring128.zig`
1,479 → 1,401. `src/` 407,831 → 404,770.

**Kept, and why.** `scripts/kernel/patches-mainline/` (0001–0007), `scripts/kernel/upstream/`, the daemon's
`FUSE_TIME_LIMITS` (0005) and host-scoped NVMe-oF (0007) code: a pending owner question decides them.
Zero-copy on the pool (`CAP_SYS_ADMIN` ∧ a sparse table, `probes.zcVerdict`; copying queues otherwise) with
its held write, bounce and bridges; `SQUEEZEFS_FUSE_RINGS_PER_QUEUE` (3 derived, no patch needed on the
pool), `SQUEEZEFS_FUSE_ZC_OPEN`, `SQUEEZEFS_FUSE_READ_FUSION`, `SQUEEZEFS_TEST_FUSE_CAP_SYS_ADMIN_ABSENT`.
No piece of `UserEnts` had a 7.3-only reason to stay.

**Checks** (this host, every load inside `scripts/venue`):

| step | before (`85d6ddf`) | after |
|---|---|---|
| unit | 3,172 pass / 173 skip / 3,345 | 3,163 pass / 153 skip / 3,316 |
| smoke | 168 / 7 / 175 | 167 / 8 / 175 (the 2 new skips: `external_interference` — another agent's task stood stopped on the host, so the two `sync` rows declined; re-run alone, both pass: the frozen mount's `sync` returned in 15.3 s of its 45 s bound, the failed bring-up's in 20 ms) |
| shim-smoke · nvmet-smoke | 45/45 · 55/55 | 45/45 · 55/55 |
| `no_kernel_feature` skips | 21 (every one kmbuf) | 0 |
| fmt · rails | green | green (29/29) |
| xfstests generic/001 013 (`scripts/suites/fstests.sh`) | — | 2 ran, 0 failed, 0 alert lines, fsck 0 findings |

The zc rows that skipped without kmbuf now run on the pool: zc READ moved 1,048,576 B with zero daemon
copies, the fused poll took 16 fusions, the measured row read 1.00 enters/request copying vs 3.04 zero-copy,
the owed spin hit 58 times.

## The A-B-B-A (2026-10-29, quiet host on AC, the RAM-NVMe perf mount, 2 MDT + 2 OST, dist builds)

`dev` `6415dc1` against `35a0b82`. **fio matrix** (one `baseline.sh ram-sqz` psync session per leg, legs
dev / the fallocate branch / this / this / the fallocate branch / dev): every 4 KiB and 1 MiB row within
± 4.5 % of `dev` (4k randread QD1 +4.5 %, ×32 +0.2 %, ×32×4 −1.0 %; 4k randwrite +1.8 / −0.1 / −0.3 %; 1 MiB
seq read −0.8 / −1.5 %, write −1.9 / +1.2 %; 4 MiB write −3.7 %), 64 KiB +5.2 % read / +8.2 % write.
**mdtest** `-n 10000 -u -i 1` at -np 32 over three rounds (6 legs per arm), plain / `-w 4096 -e 4096`:
directory creation −0.1 / +0.3 %, removal +1.7 / −0.8 %, file creation +2.2 / +1.2 %, read +2.3 / +1.4 %,
removal −1.8 / −0.2 %; -np 64 (one round) within the legs' spread. fsck clean after every run. Landed
(gate 181 red on an unrelated timing row — DEFRAG --fold … `awaitJobMidRun` JobCompletedEarly, fixed on
its own branch — and gate 182 green on the identical tree). Sessions: `/build/perf/mx/results.tsv`
(`ram-sqz-rrc*`), `/build/perf/rc-md/`, `d7-md2/`, `d7-md3/`.
