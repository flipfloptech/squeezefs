# 2026-08-06 — The READ dest-window lease: the serve dest-copy kill (copy-elimination phase 1)

Branch `perf/read-dest-lease` (worktree off `integrate/zcrx-wave` tip
`68ecf561`, **unmerged — the orchestrator merges**). Charter (the read
CPU-wall ruling, `.benchmarks/2026-08-06-read-cpu-wall.md`): kill the
serve dest-copy — `read_copy_dest_bytes` = 1.00 CPU passes/byte, ≈ 1/2.7
of the whole read CPU budget on the walled 32-core field client at
22 GB/s. Kernel legality adjudicated FIRST against the patched
linux-6.19.14 (the sqz kernel: sha256-pinned tarball + the 27-patch
series from `docker/kernel-sqz/`, applied clean — the exact tree the
field runs). Cluster READ-ONLY; field rows via report.

Commits: red `554301fb` (contracts + inert scaffolding) · green
`436705b1` (the lease gate + lane yield + engagement counter) · hygiene
`<exec-bits>` (pre-existing base condition, see §6) · docs + this note.
Local artifacts `/tmp/sqz-ab-results/` (per-leg fio JSON, stats
before/after, mount logs, cpu samples, md5 ledgers).

## 1. The kernel adjudication (both kernel-half mechanisms are DEAD)

**(A) Registered-buffer substitution — NOT expressible.** The
COMMIT_AND_FETCH handler consumes ONLY `commit_id` and `qid` from the
commit SQE (`fs/fuse/dev_uring.c` `fuse_uring_commit_fetch`,
:1279–1360: `READ_ONCE(cmd_req->commit_id)` / `READ_ONCE(cmd_req->qid)`
— the uapi `struct fuse_uring_cmd_req` carries `flags`, `commit_id`,
`qid` and the REGISTER-only `init` union, no address/iovec/buf_index
consumed at commit). The payload source is fixed at REGISTER for the
ring's life: `fuse_uring_create_ring_ent` (:1443–1499) captures
`ent->payload = iov[1].iov_base` once (`fuse_uring_get_iovec_from_sqe`
is called from nowhere else), and the commit copy imports exactly that
VA — `setup_fuse_copy_state` (:782–812):
`import_ubuf(dir, ent->payload, ring->max_payload_sz, iter)`. On the
bufring arm `sqe->buf_index` is consumed only at REGISTER as
`ent->fixed_buf_id` (:1470 — the headers fixed-index/zc slot id), never
as per-commit payload addressing. And `struct fuse_uring_ent_in_out`
(uapi `fuse.h` :1269–1284) carries `payload_sz` ONLY — no offset field:
**the reply body must sit at the ent window base at commit time.**

**(B) kmbuf/bufring lease over tier pages — NOT expressible.**
`fuse_uring_buf_ring_setup` (:294–333) pins bgid 0 and REFUSES it
unless `io_uring_is_kmbuf_ring` — a user-provided pbuf ring (the only
ring shape whose entries carry daemon-chosen addresses) is structurally
rejected (`goto error` → EINVAL). Kernel-managed ring entries are
minted by the kernel over its own allocation — `io_uring/kbuf.c`
`io_setup_kmbuf_ring` (:915–954): `io_create_region_multi_buf` then
`buf->addr = region + i × buf_size` for every bid. The daemon's only
verb against the ring is recycling the kernel's own buffer
(`io_uring_kmbuf_recycle`, :110–152). No bufring entry can ever name a
tier page.

**Corollary (the lease inversion):** lending a tier/fill buffer to the
transport can only RELOCATE the serve copy — `apply_reply`'s body
`copy_from_slice` (`crates/fuse3/…/fuse_over_uring.rs` :4669–4681)
would pay the identical pass on the queue worker (a worse venue: it
serializes the queue). The only zero-pass serve this kernel admits is a
fill that LANDS at the ent window base — so phase 1 builds the
**inverse lease**: the request's dest window is leased to the FILL
(option C of the charter, scoped honestly), riding the §5.4 exclusivity
argument (the handler owns the ent window until its reply commits) and
the existing `DestDmaLease`/`RangedDest` transport machinery.

**(zc, for the record):** the true full kill — device DMA into the
kernel-registered request folios (`fuse_uring_set_up_zero_copy` →
`io_buffer_register_bvec` at `ent->fixed_buf_id`, `skip_folio_copy`
both directions — deletes the serve copy AND the K1 commit copy) —
remains the staged follow-on (design-zero-copy-write-path §5.4c;
CAP_SYS_ADMIN, sqz-kernel-only). This phase's gate/hint plumbing is the
scaffolding a zc serve integration will reuse.

## 2. The mechanism (green `436705b1`)

* **Admission**: the FUSE read handler mints `ReadClassHint::dest_lease`
  (kernel ent-payload dest, not an arena window,
  `SQUEEZEFS_READ_DEST_LEASE` on — default ON). The router's
  single-block arm composes it with the request geometry: 4 KiB-aligned
  window AND dest pointer, strictly sub-block (whole blocks keep the
  raw full-block dest leg), passthrough, not device-true, and **no fill
  in flight for the block** (joining the single-flight cohort beats a
  second device fetch).
* **The serve**: the existing binding-validated ranged primitive
  (`get_block_range_for_index`) with a lease-tagged `RangedDest` —
  device DMA to dest, incarnation snapshot + still-check + binding
  recheck verbatim, rebind loop + whole-block fallback unchanged (a
  refused/failed lease is never a lost or wrong read).
  `read_dest_lease_bytes` counts at the VALIDATED serve (subset of
  `read_dest_dma_bytes`; the 2026-08-02 closure law is untouched).
* **Lane yield**: dest-leaseable traffic stamps
  `StreamLanes::dest_lease_ms`; while fresh (the 2 s issue-owner
  staleness convention) BOTH speculative issue arms decline —
  `pipeline_touch` returns before its R2 top-up and `read_lane_top_up`
  declines at its head, which also kills the completion-driven refill
  chain (fill-clamp campaign) for leased streams. Consume/classifier
  bookkeeping stays; the stamp ages out when the pattern stops being
  leaseable.
* **Ghost-escalation exemption**: the hybrid second-touch dispatch is
  lease-exempt — the ghost keys on the BLOCK, so the second sub-block
  window of ONE sequential pass read as a "re-read" and self-escalated
  to a whole-block fill + admission (2× device bytes and the very serve
  copy the lease deletes; caught red-first by the per-window contract).
* **Scope (phase 1, honest)**: kernel transport only (il keeps
  direct-drive/E-IL2 — already copy-parity), single-block windows only
  (multi-block rides the assembly arms), no new memory (the fill lands
  in the transport's EXISTING registered payload arena — already the R5
  `transport_payload_buffers` component), no ABI/KD-7 surface.

## 3. Local venue (labeled)

32-core dev box, `tests/dev_substrate.sh` **tcp** substrate (nvmet-tcp
on localhost — the fabric-sensitive-row venue; service port slice
54100–54199): meta 4× nullb, data 4× 8 GiB zram namespaces. Cache-less
format, 4 MiB blocks, release build (default features), fill = fresh
8 × 1 GiB (`fresh_write_pass.job`, labeled). Instrument fio-3.42 via
`tests/fio/run_fio_row.sh` (`exa_read_bw.job`: libaio bs=1M qd8 nj8,
runtime 30 ramp 5, `--no-numa` single-node fanout). Cold = fresh mount
per leg. Daemon CPU = utime+stime delta over the row wall.

## 4. The A-B-B-A bracket (lease ON = A, `SQUEEZEFS_READ_DEST_LEASE=0` = B)

| leg | GB/s | daemon cores | dest GB | lease GB | dest_dma GB | fill GB | pf_issued | lane_fetches | CPU s/GB |
|---|---|---|---|---|---|---|---|---|---|
| A1 | **11.06** | 1.60 | 0.0 | 399.0 | 399.0 | 0.0 | 0 | 0 | 0.147 |
| B1 | 8.33 | 2.41 | 302.6 | 0.0 | 0.0 | 304.1 | 623 | 1742 | 0.291 |
| B2 | 7.29 | 2.49 | 267.8 | 0.0 | 0.0 | 269.3 | 558 | 2759 | 0.340 |
| A2 | **9.03** | 1.53 | 0.0 | 329.5 | 329.5 | 0.0 | 0 | 0 | 0.169 |

**Side medians 10.05 vs 7.81 GB/s = +28.6 %, order-independent; daemon
CPU/byte 0.158 vs 0.316 s/GB = −50 %.** Engagement EXACT on both A
legs: `read_copy_dest_bytes = 0`, `read_dest_lease_bytes ≡
read_dest_dma_bytes ≡ ranged_read_bytes` (399.0 / 329.5 GB — every
served byte), `read_fill_dma_bytes = 0`, `prefetch_issued =
read_lane_fetches = 0` (the yield), `read_dest_overruns = 0`,
`bounce = 0`, rebinds 0. B legs are the pre-campaign shape verbatim
(fills + NT-stored serve copies + pipeline engaged). `transport_dest_dma_leases`
counts one window lease per serve (380,513 on A1 ≡ ranged_reads).

* **Sustained rule**: 120 s flat row (lease ON, same shape) —
  **9.59 GB/s**, in the A-side band, no decay signal.
* **Correctness**: md5 of the full 8-file set identical between a
  lease-ON and a lease-OFF mount; every per-window content assertion in
  the suite is byte-exact vs the written pattern; the fork's 138-test
  suite and the whole read-path battery green (§6).
* Venue note: the box is a shared dev machine (leg-to-leg drift visible
  A1 11.06 → A2 9.03); the bracket is read as SIDE MEDIANS, and both
  orders agree on direction and CPU/byte.

## 5. Trades priced (honest)

* **Lease rows are device-true**: on fill-sharing shapes (2 readers/
  file; hold-window re-reads) device bytes rise toward amp 1.0 from the
  shared-fill 0.58–0.69 — the §9 read-copy-count re-open condition
  ("a fabric with free bandwidth AND a CPU wall") is exactly the field
  posture per the 2026-08-06 ruling. On the 16-stream 1-reader/file
  field shape amp is already ~1.0: no device-byte cost.
* `ranged_reads`/`ranged_read_bytes` now count lease serves (1:1 with
  user bytes) — the rand-4k amplification framing of those instruments
  is unchanged (ratio ≈ 1.0).
* Warm convergence for lease cohorts comes from tiers OTHER traffic
  populated (the escalation exemption); a beyond-budget cold re-read
  loop under the lever stays device-true. `SQUEEZEFS_READ_DEST_LEASE=0`
  is the escape and the A/B control.
* qd1-no-readahead streams lean on client/kernel-readahead concurrency
  once the lane yields; the local qd8 bracket and the field's
  readahead-pipelined rows carry their own depth. If a field shape
  regresses, the lever gates it off per-mount while phase 2 composes
  the lane with the lease.

## 6. Gates

Red-first (`554301fb` fails phase A: lease 0 vs 12,582,912 — and the
per-window contract caught the ghost-escalation false positive during
the green build). Suites green: `read_dest_lease_tests` ×10,
`read_copy_ledger`, `ranged_read`, `hybrid_io`, `read_lane`,
`read_dest_bound`, `nt_read_serve`, saturation/prefetch-pipeline/
prefetch-window/admission-governor/tier-admission/stream-transient/
refetch-churn/rebind-starvation/serve-phase/fingerprint/full-length/
hole-zeros/ipc-hold-probe, `env_knob_convention`; fuse3 fork suite
(138) green; both clippys (`--all-features` and shipped-config)
`-D warnings` clean; `cargo fmt --check` clean; full
`cargo test --all-features -- --test-threads=1` from zero. One
pre-existing base failure surfaced by the full gate and fixed as a
hygiene commit: `script_exec_bit_tests` (six tracked rig scripts'
exec bits never recorded in the index under `core.fileMode=false` —
present at base `68ecf561`, not this campaign's surface).

## 7. The field acceptance row (via report — cluster read-only)

On squeeze-test (sqz kernel, kmbuf negotiated, sysctl-1024 geometry),
the cold 128 GB 16-stream bs=1M row that adjudicated the CPU wall:

* `read_copy_dest_bytes → ~0` with `read_dest_lease_bytes` carrying the
  row (closure `dest + bounce + dest_dma ≡ user × ramp` unchanged;
  lease ⊆ dest_dma) — "dest_bytes → lease_bytes".
* Daemon cores/GB down ~1/2.7 class (local counted −50 % of daemon
  CPU/byte on the same shape; the field's serve pass is NT-stored, so
  expect the DRAM column to drop ≈ 2 B/B as well).
* Cold row 22 → toward 28+ GB/s if the row is CPU-elastic as the
  ruling's arithmetic says (idle 8 % → the freed ~1 pass/byte is direct
  capacity); `%sys` softirq share rises with device bytes on
  fill-sharing shapes only.
* Sanity beside it: `read_dest_overruns = 0`, `stale_binding_rebinds`
  quiet, `prefetch_issued`/`read_lane_fetches` ≈ 0 on the leased rows,
  amp column stated (2-readers/file fio shapes will show the amp trade;
  the dd 16-stream shape will not), md5/content spot-check per the §4
  discipline, and one `SQUEEZEFS_READ_DEST_LEASE=0` control leg
  A-B-B-A per the aging-store rule.

## 8. Residuals (ranked, for phase 2+)

1. **Warm/hold serves keep the 1.00 pass** (kernel-interface-bound —
   the adjudication's hard wall). The A1 warm-split decision rule
   stands: price any tier-buffer handoff from a counted warm row's
   `warm_serve` share; on THIS kernel it can only move the copy, so the
   real phase 2 is **zc serve integration** (READ_FIXED into the
   registered request folios — kills serve AND commit passes; the
   negotiation face already ships).
2. **il arena dests** (phase-1 exempt): aligned il cold reads already
   direct-drive; the unaligned-arena residual keeps E-IL2's single
   pass.
3. **Lane/lease composition** for shapes that need daemon-side depth
   AND the lease (qd1 O_DIRECT streams): a lease-aware ahead arm would
   need scatter fills (dest window + pooled complement) — priced only
   if a field shape demands it.

## FIELD ACCEPTANCE (2026-08-06, pair ccc91701 — the phase-1 verdict)

A-B-B-A on the cold 128 GB 16-stream row, order-independent:
LEASE 27.98 / 27.74 GB/s vs CONTROL 21.47 / 21.98 — **+28.3 %**, engagement
exact (dest 0.0, lease 1,958.7/1,946.1 GB ≡ the rows' served bytes),
daemon %CPU down ~90-130 points at +28 % more delivered bytes (CPU/byte
−28 %). The local +28.6 % transferred verbatim. Reads now 67 % of raw
(27.9/41.8); the phase-2 zcrx engagement campaign owns the remaining gap
to the 85 % bar (35.5).
