# 2026-08-06 — FUSE_URING_ZERO_COPY serve integration: the K1 kill (read lever #1)

Branch `perf/fuse-zc-serve` (worktree off wave tip `50ad803d`,
**unmerged — the orchestrator merges**). Charter (the read CPU-wall
program): delete K1 — the kernel's `fuse_copy_*` folio copy from the
ent payload into the app's pages at COMMIT — measured at **≈ 32 % of
ALL client cycles** on the cold EXA read row (memcpy 16.9 % +
FR_LOCKED 12.8 % + GUP 2.6 %; `.benchmarks/2026-08-02-interface-frontier.md`).
Ruling **D13** (rc-manifest §3f) sanctions the custom-kernel
requirement; the field host runs the sqz kernel (6.19.14-sqz, patches
0019–0026 = the Koong v4 kmbuf+zc series) booted.

Commits: fork `628c4402` (zc arm + sparse-slot bridges) · root
`24b23ac5` (direct serve leg + ledger + contracts) · `7eccd53a`
(READDIR mirror pinned to the deployed kernel's measurement) · docs +
this note.

**Verdict up front: the bar is CLEARED.** A-B-B-A on the field host:
**39.84 / 39.92 GB/s armed** vs 27.95 / 27.33 GB/s control (60 s
sustained, engagement exact, zero fallbacks, all correctness gates
green) — **95.5 % of the 41.8 GB/s raw ceiling**, daemon CPU/GB
**−80 %**, box busy −14 pts at 1.43× the bytes. §5 has the table. The
rig's FATAL smoke also flushed out a PRE-EXISTING (base `50ad803d`,
zc-independent) fsync-during-writeback data-loss bug — §5b, P0
hand-off.

## 1. The kernel contract (verified line-by-line against patch 0024)

* Arm: REGISTER `init.flags = FUSE_URING_BUF_RING | FUSE_URING_ZERO_COPY`
  + nonzero `init.queue_depth`; requires the kmbuf ring and
  `CAP_SYS_ADMIN`; per-queue, all REGISTERs must agree.
* Table shape: sparse fixed-buffer entries `0..depth` (the kernel
  installs the CLIENT's request pages per request via
  `io_buffer_register_bvec`, ddir `ITER_DEST` for reads /
  `ITER_SOURCE` for writes, at `ent->fixed_buf_id` = the REGISTER
  SQE's `buf_index`), headers at index `depth`
  (`zc_headers_index`).
* Slot addressing: bvec-registered buffers carry `imu->ubuf = 0`
  (patch 0021 `io_kernel_buffer_init`), so RW-op `sqe.addr` is the
  byte OFFSET into the slot and `len ≤ registered total` (the ublk
  convention).
* **THE central finding — no per-request opt-out**:
  `can_zero_copy_req = queue->use_zero_copy && (in_pages || out_pages)`
  and COMMIT sets `skip_folio_copy` from it. On an armed queue EVERY
  paged request (READ, READDIR[PLUS], READLINK out-paged; WRITE
  in-paged) skips the folio copy — the briefing's "ineligible shapes
  fall back per-request to the kmbuf path unchanged" is NOT
  expressible for paged ops (`fuse_uring_req_has_copyable_payload`
  routes only NON-page args via kmbuf). Every paged byte must move
  through the RING against the slot, so the integration ships a
  **bounce vehicle** for every ineligible shape (§2).
* Reply length: rides `ent_in_out.payload_sz` at COMMIT
  (`fuse_copy_out_args(cs, args, payload_sz)`; the skipped folio arg's
  size derives from it), so a prefilled commit is header + length.
* Unregister at `fuse_uring_req_end` (our COMMIT, or abort). Noted
  hazard (upstream series semantics, not ours to fix in v1): the bvec
  copy holds no page references, so an ABORT racing an in-flight
  slot RW is a kernel-side use-after-free window by design of the
  carried series.

## 2. The design (what shipped)

### fuse3 (fork) — `TransportBufferMode::ZeroCopy`

Arm = `SQUEEZEFS_FUSE_ZC=1` + kmbuf surface Present + kmbuf lever on +
euid 0 (the CAP_SYS_ADMIN proxy, checked at resolution so the refusal
is loud there, not a per-queue REGISTER error). Every decline is a
single loud line and the session continues on the
bufring/user-ent path with `fuse3_zc_replies = 0` — **stock kernels
never attempt zc** (surface Absent ⇒ decline; verified on the dev box,
§4). Singleton drain groups (the kmbuf law). Per queue:

* kmbuf resources as before PLUS the sparse table shape (depth zeroed
  iovecs + headers at index `depth`, one `IORING_REGISTER_BUFFERS`
  call — sparse entries are stock 5.19+, pinned by
  `test_zc_setup_table_shape` on BOTH kernel classes);
* a **memfd bounce arena** (`ZcBounce`, depth × payload_sz, mapped
  MAP_SHARED): the same bytes reachable by VA (CPU serves, §5.4
  leases, dest windows — `PayloadArena::from_zc` wraps it so every
  existing protocol runs verbatim) and by FD (the ring bridges);
* three bridge ops on the queue ring (`RingOp::Fetch`, resolved
  against the per-ent `ZcPend`). The out-paged mirror is
  **{READ, READLINK}** — READDIR[PLUS] was bridged in the first cut
  and the field probe measured every readdir reply kmbuf-attached and
  kernel-copied on 6.19.14-sqz (one clean fallback per `ls`, output
  correct — the safety net's designed outcome), so `7eccd53a` pinned
  them back onto the kmbuf path (empiricism over source-reading; a
  kernel that DOES zc readdir hits the loud no-attachment EIO guard):
  - `READ_FIXED(device fd → slot)` — the direct leg
    (handler-initiated via `zc_device_fetch`, oneshot back to the
    handler which validates and replies `prefilled`);
  - `READ_FIXED(memfd → slot)` — the bounce bridge for every
    body-carrying out-paged reply (worker stages the body into the
    bounce — ptr-equality elides the copy for dest-armed serves —
    parks the commit, commits on the bridge CQE);
  - `WRITE_FIXED(slot → memfd)` — WRITE extraction at delivery; the
    inbound dispatch defers to its CQE and the §5.4 lease then rides
    the bounce mapping (parity: the kernel shmem copy replaces the
    delivery-time folio copy).
* fallback ladder (the opcode-mirror safety net): a failed bounce
  bridge falls back to the kmbuf attachment when one exists
  (an op we bridged that the kernel actually served copyable), else a
  header-only EIO — LOUD + `fuse3_zc_fallbacks`; a payload-announcing
  delivery with no attachment and no extraction arm (FUSE_IOCTL-class)
  delivers empty + `fuse3_zc_slot_payload_skips`. Teardown: parked
  bridges synthesize header-only EIO (never a body commit the kernel
  would skip), handler fetches unblock by sender drop, deferred WRITE
  deliveries ride the row-8 owing() synthesis.

### The seam adjudication

Option (a) from the charter — **routing hands the transport a
descriptor and the queue worker submits/reaps on ITS ring** — because
fixed-buffer slots are per-ring resources and the COMMIT must be
ordered after the fill on the same ring anyway. The handler AWAITS the
fetch (oneshot), keeping the dest-lease validation ladder intact
(resolve → DMA → incarnation-still + binding recheck → reply);
alternatives rejected: (b) exposing a submission handle to routing
couples the NVMe worker's backpressure/fence machinery to a foreign
ring it cannot pace; cloned buffer tables (`IORING_REGISTER_CLONE_BUFFERS`)
are snapshot-semantic — the kernel installs bvecs per-request into THE
queue ring's table, so a clone never sees them. The device fd is a
dedicated lazy `O_DIRECT` read-only fd per volume
(`NvmeBlockDev::zc_read_fd`) — sharing a worker fd would couple
lifetimes, not queues.

### Root — the direct leg (routing) + the prefilled reply

* `ZcReadServe` — the handler-minted fetch handle (kernel ring slots
  only, never il arena overrides); `ReplyData::zc_prefilled` — the
  session's prefilled COMMIT (header + length, no body move).
* Eligibility (the router's cold single-block arm, strictly after the
  overlay/hot/tier probes): 4 KiB-aligned start AND length,
  `start + len ≤ block_size` (**whole-block INCLUDED** — the
  max_write=4M cohort hazard dest-lease left behind is covered),
  passthrough only, never device-true, no in-flight fill for the block
  (the dest-lease single-flight admission law), undecorated block key
  (decorated `bk:off:len` partial mappings decline at parse — the same
  contract the raw dest leg trusts).
* Fill discipline mirrors the raw dest leg verbatim: incarnation
  snapshot → fetch → incarnation-still + binding recheck; ANY failure
  or movement falls through to the ordinary ladder, whose reply
  bridges through the bounce and OVERWRITES the pages.
* The handler composes with overlay-never-invisible: parked runs
  decline the leg per-iteration; a zc-served (empty-body) read
  re-validates runs + the custody fingerprint and re-reads WITHOUT the
  leg on any movement (an empty body is never a servable last
  compose). zc-eligible traffic stands the speculative fill machinery
  down via the same lane stamp as dest-lease.
* Single-flight/cohort choice (v1, stated per charter): an in-flight
  fill DECLINES the leg (join the cohort — strictly cheaper than a
  second device fetch); ledger-visible admission is never taken for
  zc-served bytes (the yield keeps armed rows cold, which is also what
  makes engagement exact).

### Instruments

`fuse3_zc_negotiated` (arm proof) · `fuse3_zc_replies` (paged replies
that rode the slot) · `fuse3_zc_fallbacks` / `fuse3_zc_slot_payload_skips`
(mirror tripwires, ≈ 0 / 0) · `read_zc_serve_bytes` (the direct-leg
ledger — a NEW closure term beside `read_copy_dest_bytes` /
`read_dest_dma_bytes`: zc bytes never touch a daemon-visible
destination, so armed-row closure reads
`zc_serve + dest + bounce + dest_dma ≈ served`). The A-B-B-A rig's
engagement gate is FATAL on: armed legs `read_zc_serve_bytes ≥ 95 %`
of the fio measurement window + `fuse3_zc_replies > 0` + fallbacks/
skips flat; control legs zc ledger identically 0; every leg passes a
correctness smoke (write→O_DIRECT-readback md5 + readdir + readlink —
the three bridge classes live) before its row.

## 3. Verification (D12 posture — targeted, both workspaces)

* fork: 143 green (`cd crates/fuse3 && cargo test`), including the new
  zc REGISTER wire shape (`init.flags|queue_depth|buf_index`), the
  sparse-table registration (capability-lattice test runs the REAL
  registration on both kernel classes), `ZcBounce` dual-face memory,
  the paged-opcode mirror pin, mode/gauge composition; clippy
  `-D warnings` clean; fmt clean.
* root: `fuse_zc_serve_tests` (injected fetch primitive — engagement +
  whole-block + ground-truth device bytes, warm-venue preservation,
  unaligned decline, broken-leg fallthrough) + targeted neighbors all
  green single-threaded: `read_dest_lease_tests`,
  `read_copy_ledger_tests`, `read_lane_tests`, `ranged_read_tests`,
  `read_dest_bound_tests`, `read_fingerprint_tests`,
  `read_full_length_tests`, `read_serve_phase_tests`,
  `read_saturation_tests`, `rebind_starvation_tests`,
  `transport_lease_overlong_tests`, `multi_queue_tests`,
  `skip_ledger_tests`, `env_knob_convention_tests`; clippy
  `--all-features` AND shipped-config `-D warnings` clean; fmt clean.

## 4. Local venue (dev box, 7.1.6-1-cachyos — the stock-kernel posture)

* Raw uapi probe: `io_uring_register(opcode 37)` answers **EINVAL** —
  the kmbuf/zc surface is Absent on this kernel (the carried series
  was dropped upstream at the author's request; nothing conflicts with
  opcode 37 here).
* Consequence, pinned by the capability-lattice tests: with
  `SQUEEZEFS_FUSE_ZC=1` the resolution declines LOUD and the session
  runs today's userspace-ent path byte-identically —
  `fuse3_zc_negotiated = 0`, `fuse3_zc_replies = 0`, never a mount
  failure. (The kmbuf-Present-but-zc-refused REGISTER shape — no such
  kernel exists in the fleet: kmbuf Present ⇒ the sqz series ⇒ zc —
  fails the session naming the `SQUEEZEFS_FUSE_ZC=0` lever; recorded
  here as the one deliberate non-degrade.)

## 5. Field acceptance (squeeze-test: EL8, 6.19.14-sqz, 32 CPUs, 2×200GbE, 10 nvme-tcp namespaces)

Headline cold row: fio libaio direct=1 bs=1M numjobs=16 iodepth=8
nrfiles=8 size=8g time_based 60 s ramp 10 s over `exa_perf`
(16×8×1 GiB prefilled). A-B-B-A legs, each on a FRESH mount
(`SQUEEZEFS_FUSE_ZC=1|0|0|1`), FATAL engagement + correctness gates
per leg (§2 Instruments). Baseline for comparison: dest-lease-ON
control ≈ 27.9–28.1 GB/s; raw ceiling 41.8 GB/s; bar 35.5 GB/s (85 %).

Binary `7eccd53a` (rocky8 pair, md5-verified both ends). Every leg:
fresh mount → arm-proof gate → correctness smoke (O_DIRECT+fsync
write → O_DIRECT AND buffered readback md5 + readdir + readlink — the
zc WRITE-extraction, direct-read, bounce and kmbuf classes all live)
→ 60 s + 10 s-ramp fio row → FATAL engagement verdict. All four legs
PASSED every gate. Artifacts: `squeeze-test:/scratch/tmp/zc-abba-final2/`
(per-leg fio JSON, stats before/after, /proc/stat + daemon-CPU
snapshots, mount logs).

| leg | zc | GB/s (60 s sustained) | box busy % | daemon CPU s | daemon ms/GB | zc GB | dest-lease GB | dest-copy GB | zc replies | fallbacks |
|----:|---:|----:|----:|----:|----:|----:|----:|----:|----:|----:|
| 1 (A) | 1 | **39.841** | 65.0 | 234.3 | **84.0** | 2790.7 | 0.1 | 0.0 | 2,661,427 | 0 |
| 2 (B) | 0 | 27.951 | 80.3 | 847.5 | 433.1 | 0.0 | 1958.3 | 0.0 | 0 | 0 |
| 3 (B) | 0 | 27.327 | 79.5 | 831.6 | 434.7 | 0.0 | 1914.6 | 0.0 | 0 | 0 |
| 4 (A) | 1 | **39.919** | 66.6 | 249.1 | **89.1** | 2792.1 | 0.1 | 0.0 | 2,662,769 | 0 |

* **Both brackets** (A→B and B→A) show the same delta — order-
  independent: **+42.6 % / +46.1 % GB/s** (39.84/39.92 vs 27.95/27.33).
* **Engagement EXACT**: armed legs' `read_zc_serve_bytes` (2.79 TB) =
  the row's full ramp-inclusive volume (fio measured-window io_bytes
  2.39 TB; ratio 1.167 ≈ 70 s/60 s); `fuse3_zc_replies` ≈ 2.66 M =
  the row's READ count; fallbacks + slot-skips **0**; control legs' zc
  ledger identically 0 with dest-lease carrying the row (1.9 TB) as
  before — the baseline mechanism intact.
* **Pass DELETION, not just GB/s**: daemon CPU/GB fell **433 → 84–89
  ms/GB (−80 %)** and whole-box busy fell 80 → 65–67 % while moving
  1.43× the bytes — the claim is CPU-work removal and the CPU columns
  prove it (the kernel-side K1/GUP/lock share rides the box column).
* Latency face: clat mean 3.35 ms vs 4.8 ms; p99 11.8 ms vs 17.5 ms.
* Sustained: 60 s time_based rows, per-leg `bw_min/bw_max` window
  34.5→43.4 GiB/s (armed) with no decay trend; a 15 s early probe on
  the pre-fix binary read the same (38.8 GiB/s) — burst ≈ sustained.
* **Verdict vs the bar**: 39.84/39.92 GB/s ≥ **35.5 GB/s bar (85 % of
  the 41.8 GB/s raw ceiling)** — CLEARED at **95.5 % of raw**. K1 is
  dead on the headline row.

## 5b. PRE-EXISTING data-loss bug found by the rig's FATAL smoke (NOT this branch's — P0 hand-off)

The rig's first armed leg FAILED its write→readback md5 smoke. Root
cause hunt (field, counted):

* Shape: `cp <32 MiB file> mnt/f && sync mnt/f` (buffered write +
  `fsync(2)` on the file) then read back ⇒ the file's **LAST 4 MiB
  block reads all-zeros**, PERSISTENTLY (O_DIRECT re-read, buffered
  re-read, both corrupt — the stored state is zeros). Always block 7
  of the 8-block file; `overwrite_seed_materialized` +1 per trial
  (the partially-covered-block seed path engaging on a block that ends
  fully covered — the smoking gun for an fsync-vs-writeback coverage
  race).
* Rates (10 trials each, same host/volume): zc=1 armed **6/10**
  corrupt · zc=0 control same binary **7/10** · **BASE `50ad803d`
  (the wave tip, pre-branch) 5/10** — the bug PREDATES this branch
  and is zc-independent.
* NOT corrupting (5/5 clean each): `dd oflag=direct conv=fsync`
  (O_DIRECT writes + fsync), and `cp` + **global** `sync` (syncfs).
  The trigger is specifically fsync-on-file racing in-flight kernel
  writeback of that file's tail block.
* Tripwires all silent during corruption: `write_path_seed_read_bytes 0`,
  `writeback_superseded_noops 0`, `invariant_tripwires 0`,
  `writeback_errors_latched 0` — nothing upstream noticed the loss.
* Hand-off: needs its own red-first campaign (repro-port: live-mount
  shape — kernel writeback + FUSE_FSYNC interleave; the
  `overwrite_seed_materialized` engagement per corrupt trial is the
  entry point). The rig's smoke now uses the O_DIRECT+fsync shape so
  THIS campaign's acceptance measures its own machinery; the finding
  is loud here, not dodged.

## 6. Honest notes / follow-ups

* **Write-path tax on armed mounts (priced, not hidden)**: every FUSE
  WRITE pays one extra ring round trip (extraction) with copy-count
  parity (kernel shmem copy replaces the delivery folio copy). The
  headline row is read-only; write rows on armed mounts need their own
  bracket before default-ON.
* **Warm out-paged replies** bridge at copy-count parity + one ring
  round trip; warm-serve zc (tier-buffer → slot without the bounce
  hop) is the separately-priced program (A1 decision rule).
* `ZcReadServe` minting allocates per READ on armed mounts (Box +
  Arc clones) — negligible at 1 MiB shapes; pool it before arming
  IOPS-class rows by default.
* The NUMA K1-crossing instrument does not count zc WRITE extractions
  or bounce bridges (kernel-side moves) — armed-mount locality rows
  read lower than the truth; instrument follow-up.
* Default stays **OFF**; flipping default-ON (on kmbuf-Present
  kernels) is a follow-up commit after field acceptance — the
  dest-lease precedent.
