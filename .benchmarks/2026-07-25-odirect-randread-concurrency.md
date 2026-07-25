# 2026-07-25 — O_DIRECT 4k randread concurrency: the per-read DashMap shard scan

Branch `perf/odirect-randread-concurrency` (off dev `b969efb`). Fix commits:
red `7d72d66` (tests/parked_overlay_gate_tests.rs), green `ed110e1`
(src/fuse_client.rs `park_overlay_entry` / O(1) gate).

## 1. The field report (2026-07-25, 6-node cluster)

Client: 32-CPU box, kernel-FUSE mount over NVMe-oF/TCP (zram-backed OSS,
~235 µs device RTT). Raw fio on the fabric namespace: **134k IOPS** 4k
randread @ QD32. SqueezeFS on the same namespaces: **45k IOPS** (elbencho
`-r --rand -t 16 --iodepth 16 -b 4k --direct`, 16 × 10 GB files). FULL
transport posture (queues=32, q_depth=32, max_background=256, payload
1 GiB). Signatures: more threads = SLOWER; iodepth changes = nothing;
`-o direct_device_true` remount = no change; user hypothesized per-inode
serialization (16 files × ~355 µs/op ≈ 45k).

## 2. Substrate (labeled; used for every row below)

Dev box: 23 CPUs (single socket), 109 GiB RAM, kernel 7.1.4-1-cachyos,
FUSE protocol 7.45. Custom latency substrate (the dev_substrate nvmet-loop
pattern with a timer-mode data device):

| Role | Backing | nvmet | Host device |
|---|---|---|---|
| data (oss) | configfs null_blk `sqzlat_oss0`: 36 GiB, `memory_backed=1`, **`completion_nsec=235000`, `irqmode=2` (timer)**, bs 4096, 8 submit queues, hw QD 128 | loop subsys `nqn.…:sqzlat-oss0`, port 52126, `resv_enable=1` | `/dev/nvme1n1` (`nvme connect -t loop -i 8`) |
| meta (mds) | configfs null_blk `sqzlat_mds0`: 3 GiB, memory-backed, 256 MiB write-back cache, completion 0 | loop subsys `nqn.…:sqzlat-mds0` | `/dev/nvme2n1` |

**Raw ceilings** (instrument: fio 3.42 on `/dev/nvme1n1`):

| Row | IOPS | Note |
|---|---|---|
| psync QD1 | 4,119 | clat avg 242 µs — the latency knob verified |
| libaio QD32 ×1 job | 109k | ≈ the field report's 134k raw shape |
| libaio QD16 ×16 jobs (256 in-flight) | **298k** | the offered-load ceiling for the elbencho rows |

Filesystem: `format sqmeta:///dev/nvme2n1 sqdata:///dev/nvme1n1` —
**cache-less** (no staging declared ⇒ all beyond-inline data striped),
4 MiB blocks. Mount: `--daemon --allow-other -o direct_device_true`
(device-true engagement verified per run: `ranged_reads` ==
`read_device_true_reads` == instrument op count; `read_odirect_tier_serves`
= 0; transport queues=32 depth=32). Dataset: 16 × 1.5 GiB files (24 GiB ≫
1 GiB mem cache), written with elbencho `-w -t 16 -b 4M --direct`.

Instruments: elbencho 3.1-10 (dynamic), fio 3.42. Box quiet for every
counted row (one polluted sweep — run concurrently with a background cargo
build — was discarded and re-run; noted in §6).

## 3. Reproduction + the discriminating experiment

elbencho `-r --rand -t 16 --iodepth 16 -b 4k --direct`, dev `b969efb`:

| Files | IOPS (old binary) |
|---|---|
| 1 | 176k |
| 4 | 148k |
| 16 | 129k (first-mount cold pass; 180–189k warmed, see §5) |

**No per-inode serialization**: 1 file ≥ 16 files at the same offered load,
on both binaries. The user's "k × file_count" arithmetic was coincidence.
What did reproduce is the **collapse against the raw ceiling** (≈184k vs
298k at 256 in-flight ⇒ effective device concurrency ~34 of 256 offered)
and the field signatures: **more client threads = slower** (t32 qd16 =
128k < t16 qd32 = 159k, same 512 in-flight), iodepth-indifference past the
collapse point, `direct_device_true`-indifference (the collapse is
orthogonal to tiering — engagement was already 100 % device-true).

## 4. Root cause (perf, code anchors)

perf (dwarf call graphs, 10 s under t16 qd16 load, dev binary):

```
45.45%  squeezefs::fuse_client::SqueezefsFilesystem::capture_parked_runs
11.87%  dashmap::lock::RawRwLock::lock_shared_slow
```

`capture_parked_runs` runs **twice per FUSE READ** (pre/post-runs of the
moving-custody read protocol, `src/fuse_client.rs` read handler; also ×2 in
`copy_file_range`) and gated on `active_block_buffers.is_empty()`.
`DashMap::is_empty()` (dashmap 5.5.3, `lib.rs:1110 _len`) **read-locks
every shard and sums lengths** — 128 shards at ≥32 CPUs. At saturation
that is ~50 M shard-rwlock acquisitions/second of pure cache-line
coherence traffic across all tokio workers (22 workers at ~65–75 % CPU
each ⇒ ~51 µs CPU/op), and the cost **grows with handler-thread count** —
the "more threads = slower" engine. `SQUEEZEFS_OP_PROFILE=1` phase
histograms agreed: `read/backend` p50 landed in the ≤1024 µs bucket
against a 235 µs device (queuing on CPU, not on the device — device
inflight sampled 32–203, bursty).

The map is **empty** on a pure-read workload — every read paid the full
scan to learn nothing.

## 5. The fix and the A/B

`parked_overlay_count`: a lock-free `AtomicUsize` — incremented **before**
a park publishes into `active_block_buffers` (all three park sites route
through the new `park_overlay_entry`, the mandated single insert path;
replacements re-balance), decremented **after** `retire_parked_overlay`
removes. `0` proves the map empty; a transient over-count only costs the
ordinary per-block probe. `capture_parked_runs` now gates on one Acquire
load. Contract pinned by `tests/parked_overlay_gate_tests.rs` (red at
`7d72d66`: park assertions fail with the counter unwired; green at
`ed110e1`).

Same mount session per side, same substrate, medians of 3 where shown
(instrument stated per row):

| Row (instrument) | dev `b969efb` | fixed `ed110e1` | Δ |
|---|---|---|---|
| raw ceiling, libaio 16×QD16 (fio) | 298k | 298k | — |
| t16 qd16, 16 files (elbencho) | **184k** (189/184/180) | **316k** (322/315/314) | **+71 %** |
| t16 qd16, 1 file (elbencho) | 173k | 273–286k | +64 % |
| t32 qd16, 16 files (elbencho) | 128k | 303k | +137 % — thread-degradation gone |
| t64 sync, 16 files (elbencho qd1) | 125k (125/125/124) | 180k (182/180/180) | +44 % |
| t16 sync, 16 files (elbencho qd1) | 28.9k | 36.4k | +26 % |
| psync ×16, 1 file (fio) | 28.3k | 35.9k | +27 % |
| libaio QD32, 1 file (fio) | 56.5k | 75–77k | +36 % |

Post-fix perf: `capture_parked_runs` and the dashmap lock are **gone from
the profile** (top symbol 3.65 % `fuse3::raw::session::spawn`); the
t16 qd16 row lands at/above the raw 16×QD16 ceiling shape (kernel splits +
readahead-free 4k FUSE reads pipeline slightly differently than fio's
libaio pattern, hence ≥). Engagement exact on every counted row
(`ranged_reads` == `read_device_true_reads`, tier serves 0). File-count
sweep post-fix: 1/4/16 files = 273k/313k/313k — flat, no per-file term.

## 6. Kernel-side adjudication (the user's per-inode hypothesis)

On kernel 7.1.4 / FUSE 7.45 with our reply flags (`FOPEN_NOFLUSH |
FOPEN_PARALLEL_DIRECT_WRITES`, no `FOPEN_DIRECT_IO` for regular files) —
clean box, fixed binary, ONE inode:

| Concurrency on one file (fio) | IOPS |
|---|---|
| psync ×1 | 3.3k (497 µs/op incl. FUSE round trip) |
| psync ×4 | 8.0k |
| psync ×16 | 36.2k |
| psync ×64 | 180k |
| libaio QD1→QD32 ×1 job | 3.2k → 77k |

Near-linear in sync threads AND in iodepth ⇒ **no kernel per-inode
serializer for O_DIRECT reads here**: the kernel takes `i_rwsem` shared
for direct-IO reads (exclusive is write-side — that's what
`FOPEN_PARALLEL_DIRECT_WRITES` relaxes; there is no read-side equivalent
to advertise because reads are already parallel), and `FUSE_ASYNC_DIO` is
negotiated (fuse3 echoes it; the iodepth scaling proves it live).
A first sweep of this table was discarded — it ran concurrently with a
background `cargo test` build and showed a fake serialization shape
(psync ×4 ≈ ×1); the counted table above is from the quiet box.
**Adjudication**: the field symptoms (thread-negative scaling,
iodepth-indifference, `direct_device_true`-indifference) are all
signatures of the daemon-side coherence collapse in §4, which scales
*worse* on wider/NUMA boxes (the user's 32-CPU client vs this 23-CPU
single socket). No kernel-interface mitigation is required on this
kernel. If the user's kernel still shows genuine per-file scaling after
this fix, re-run the §3 discriminator there and escalate with that
kernel's version; the LD_PRELOAD interception plane remains the
kernel-bypass lever of record.

## 7. Residuals (out of scope here, recorded)

- **QD1 round-trip economy**: 497 µs/op sync vs 242 µs device — ~200 µs
  of transport+daemon per-op latency dominates low-concurrency rows
  (t16 sync = 36k). That is round-trip/transport-economy territory
  (L3/D2-class), not a concurrency bug.
- `read_custody_fingerprint` runs twice per read (moka get + metadata
  clone, ~3–4 % of CPU post-fix) — next visible term if this path needs
  more.
- `.stats` reads via `cat` intermittently truncated ~31 bytes short of the
  full JSON while the same open read via python was complete — worth a
  look at the virtual-inode short-read path (observed twice during this
  campaign, not investigated).

## 8. Substrate teardown

`nvme disconnect -n nqn.…:sqzlat-{oss0,mds0}`, unlink port 52126 subsys
links, `rmdir` nvmet namespaces/subsystems/port, `echo 0 > power` +
`rmdir` the two `sqzlat_*` configfs null_blk items. (Left up while the
branch is under review — the devices are RAM-backed and reboot-ephemeral.)
