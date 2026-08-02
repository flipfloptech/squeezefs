# sqz kernel v2 — candidate manifest (scoping campaign 2026-08-04)

Survey + verify + assemble only — **no rebuild happened in this campaign**
(evidence note: `.benchmarks/2026-08-04-sqz-kernel-v2-scoping.md`). Every
"present-in-base" verdict below was read from the ACTUAL tree the v1 recipe
builds — the fully-patched `linux-6.19.14` (+ the 26-patch series) retained in
the `squeezefs-kernel-sqz-work` volume from the v1 build (byte-identical to
what `build-kernel.sh` produces; verified: kmbuf symbols in `io_uring/kbuf.c`,
`FUSE_URING_BUF_RING`/`FUSE_URING_ZERO_COPY` in the uapi). Lore refs were
fetched via `b4` (thread mbox endpoints work; HTML search is bot-walled) and
mirror archives; apply-cleanliness was measured by real `git apply --check` /
`patch --dry-run` against the patched tree.

Counted-term anchors (`.benchmarks/2026-08-02-interface-frontier.md` §3 Row A,
field 7.1.2 kernel, kern EXA read 1M qd8):

* FUSE ent→user commit machinery ≈ **32.3 %** of ALL client cycles =
  memcpy under `fuse_uring_copy_from_ring` **16.9 %** + `_raw_spin_lock`
  under `fuse_copy_fill` (per-page `FR_LOCKED` round-trips, ≈ 7 M/s)
  **12.8 %** + fill/GUP/unpin ≈ 2.6 %.
* nvme-tcp RX copy class ≈ 16–26 % of FS-row cycles (75–80 % of the raw
  ceiling) — owned by the zcrx lane program (PR Z1), not this kernel.

---

## Candidate 1 — max_pages / 4 MiB writes

**Present-in-base verdict: SYSCTL-ONLY. No kernel patch needed — on either
kernel.**

What the tree actually has (all cites = the patched 6.19.14):

* `fs/fuse/sysctl.c` — **`fs.fuse.max_pages_limit`** sysctl, default
  `fuse_max_pages_limit = 256` (`fs/fuse/inode.c:42`), writable range
  1..65535. **There is no `FUSE_MAX_MAX_PAGES` constant anymore** — the
  hard cap is `sysctl_fuse_max_pages_limit = 65535` (`sysctl.c:14`,
  "Bound by fuse_init_out max_pages, which is a u16"). 65535 pages
  = 256 MiB is the protocol ceiling; 4 MiB = 1024 pages is nowhere near it.
* INIT processing (`inode.c:1392`):
  `fc->max_pages = min(fc->max_pages_limit, max(arg->max_pages, 1))`.
  Our fuse3 fork already advertises `max_pages = u16::MAX`
  (`crates/fuse3/src/raw/abi.rs:41` `DEFAULT_MAX_PAGES`), so the effective
  value **is the sysctl**, verbatim.
* `fc->max_write` (`inode.c:1470-71`): daemon-supplied, floor 4096, **no
  upper clamp in the kernel**. Data-op size is bounded by
  `min(max_write, max_pages × PAGE_SIZE)` at the request-build sites.
* fuse-uring bounds it from BELOW, not above (`dev_uring.c:267-268`,
  `fuse_uring_create`):
  `ring->max_payload_sz = max(FUSE_MIN_READ_BUFFER, fc->max_write,
  fc->max_pages * PAGE_SIZE)`, and REGISTER **refuses** ents whose payload
  iovec is smaller (`dev_uring.c:1481` "Invalid req payload len"). The ring
  is created at first REGISTER, after INIT, so it sees the final negotiated
  values. The zc series' kmbuf/bufring geometry does NOT bound it lower:
  bufring buffer sizes are daemon-chosen at `IORING_REGISTER_KMBUF_RING`
  time, and `setup_fuse_copy_state` takes whatever length the selected
  buffer carries (`ent->payload_kvec.iov_len`).

**The exact lever chain to 4 MiB max_write:**

1. `sysctl fs.fuse.max_pages_limit=1024` (both kernels — the sysctl exists
   in 6.19.14; same-lineage 7.1.x carries it too, verify once with
   `sysctl fs.fuse.max_pages_limit` on the field box).
2. Daemon replies `max_write = 4 MiB` (today hardcoded 1 MiB at
   `src/fuse_client.rs:11285`, plus the mount-option string
   `max_write=1048576,max_pages=256` at `:15051`).
3. Daemon registers ≥ 4 MiB payload ents.

**Daemon-side charter (the follow-on campaign, prose):**

* **A latent REGISTER-refusal bug to fix first**: the fork's geometry
  planner hardcodes the kernel default —
  `fuse_over_uring.rs:997` `payload_sz = max(max_write, 8192, 256 × 4096)`
  with `const KERNEL_MAX_PAGES_LIMIT: usize = 256`. Because we advertise
  `max_pages = 65535`, on ANY box where an operator raises
  `fs.fuse.max_pages_limit` past 256 the kernel's `ring->max_payload_sz`
  becomes `sysctl × 4 KiB` > our 1 MiB ents and **every REGISTER fails →
  mount fails** (over-uring is mandatory). The planner must derive
  `payload_sz` from the EFFECTIVE `fc->max_pages` =
  `min(read /proc/sys/fs/fuse/max_pages_limit, advertised max_pages)`,
  falling back to 256 when the proc file is absent. This fix is needed
  regardless of whether we ever turn the 4 MiB knob.
* **R5 `transport_payload_buffers`**: arena = queues × depth × payload_sz.
  Field shape 32 queues × depth 32 × 4 MiB = 4 GiB → the existing L1
  degradation against the cap `min(mem_budget/8, 2 GiB)` lands depth 16
  (2 GiB). That is the designed behavior; the campaign must A/B whether
  depth 16 × 4 MiB beats depth 32 × 1 MiB on the write-wall and EXA rows
  (BDP says yes for bandwidth shapes; the probe-up governor rows judge).
* **INIT negotiation**: nothing to change — `DEFAULT_MAX_PAGES = u16::MAX`
  already lets the sysctl govern. Update the stale comment at
  `fuse_over_uring.rs:973` ("kernel clamps max_pages to
  fuse_max_pages_limit = 256" — true only at default).
* **Write path**: a 4 MiB `max_write` makes a whole 4 MiB block arrive as
  ONE `FUSE_WRITE` (today: 4 kernel-split, possibly out-of-order 1 MiB
  segments — the RW3b coverage-union machinery stays, it just engages
  less), one payload lease, one merge — and on the zc arm (candidate 2)
  zero merge. Per-op fixed costs (dispatch, handler-lane spawn, commit
  batching, wake economy) quarter on large-sequential rows.
* **What it does NOT buy**: the per-page `FR_LOCKED`/GUP term is per PAGE,
  not per request — 4 MiB requests do not shrink it per byte (candidate 2
  owns that term). fio/elbencho rows at 1 MiB block size are unchanged by
  construction.

**Win pricing**: request-count-proportional costs on ≥ 4 MiB-sequential
shapes (EXA ingest/read, block write-through). Bounded by the non-copy
share of the commit machinery ≈ a few % of client cycles + daemon per-op
costs; the real payoff is compositional (one lease per block; placed-sever
adoption trivially whole-block). **Risk: low** (sysctl + daemon knobs, all
A/B-able; no kernel delta).

**Recommendation: DAEMON CAMPAIGN + sysctl. Include in v2 rebuild: nothing
(no kernel change). Fix the planner bug in the fork now.**

---

## Candidate 2 — FR_LOCKED / per-page request-lock batching

**Present-in-base verdict: the zc series ALREADY deletes the term — for
daemons that adopt the bufring. No patch to author.**

Read from the patched tree:

* The classical userspace-ent path (what our fork uses TODAY —
  REGISTER with header+payload iovecs): `fuse_uring_copy_{to,from}_ring` →
  `setup_fuse_copy_state` → `import_ubuf` over `ent->payload` →
  `fuse_copy_args` loops `fuse_copy_fill` per 4 KiB: `unlock_request` →
  `iov_iter_get_pages2(…, 1 page)` → `lock_request`
  (`dev.c:874-932`) — two `req->waitq.lock` spin-lock round-trips + one
  GUP **per page**. That is the counted 12.8 % + 2.6 %. The memcpy itself
  (`fuse_copy_do`, `dev.c:936`) is the 16.9 %.
* With **`FUSE_URING_BUF_RING`** (kmbuf, series patch 19): the payload is a
  kernel address — `cs->is_kaddr = true`, `cs->len` spans the WHOLE buffer
  (`dev_uring.c:801-803`), `fuse_copy_fill` early-returns
  (`dev.c:879-880 "if (cs->is_kaddr) return 0"`), and `fuse_copy_folio`
  skips the fill branch entirely (`dev.c:1141 "!cs->len && !cs->is_kaddr"`).
  **Zero `FR_LOCKED` round-trips, zero GUP, zero kmap of the ent side** —
  one `memcpy` per folio remains (the K1 byte move, both directions).
  `lock_request`'s no-page-fault rationale is structurally void on kaddr.
* With **`FUSE_URING_ZERO_COPY`** (patch 24, `CAP_SYS_ADMIN`): zc-eligible
  requests (`in_pages || out_pages` — i.e. WRITE payloads AND READ replies,
  `can_zero_copy_req`, `dev_uring.c:92`) register the request folios as an
  io_uring **fixed buffer** in the daemon's ring
  (`fuse_uring_set_up_zero_copy` → `io_buffer_register_bvec`,
  `ent->fixed_buf_id`), set `cs->skip_folio_copy`, and `fuse_copy_args`
  returns before the folio copy (`dev.c:1233-1235`). **The K1 memcpy dies
  in BOTH directions** — the mission brief's "writes/K1 still pay it" is
  WRONG for the zc arm: a FUSE_WRITE's folios arrive as an ITER_SOURCE
  fixed buffer the daemon can `READ_FIXED` into its arena (1 copy, K1
  gone) or `WRITE_FIXED` straight to NVMe (0 copies) for complete-block
  write-through.
* **Large-folio FUSE does NOT coarsen the classical term**: the 6.16-era
  large-folio handling is in-base (`fuse_copy_folio` takes `folio_size`,
  writeback tmp-folio removed — no `tmp_folio` in `file.c`) but enablement
  is upstream-future (blocked on dirty tracking/iomap per Joanne's series
  notes), and even enabled, `fuse_copy_fill` still GUPs the *userspace ent*
  one page at a time — coarsening only helps the kaddr path, which already
  skips everything.

**What remains patchable**: a batch-lock/pin-once patch for the classical
userspace-ent path is writable (~50-line shape: take `FR_LOCKED` once per
commit + `iov_iter_get_pages2` the whole span, or pin `ent->payload` at
REGISTER like fixed buffers) — but it duplicates what patch 19 already
ships in-tree, would never be accepted upstream alongside it, and dies the
day we adopt the bufring. **Author nothing.**

**The v2 move is DAEMON ADOPTION, not a kernel patch**: teach the fork's
over-uring arm to (a) register the kmbuf ring + headers fixed buffer and
REGISTER with `init.flags = FUSE_URING_BUF_RING` (deletes 12.8 % + 2.6 %
lock/pin, keeps 1 memcpy), then (b) the `FUSE_URING_ZERO_COPY` arm
(+ `init.queue_depth`) with `READ_FIXED`/`WRITE_FIXED` serves (deletes the
16.9 % too; upstream prior +20–25 % randread @ 1M is the planning number).
Both are negotiated per-queue at REGISTER; the payload-lease law (§5.4)
maps onto buffer-recycle (deferred `COMMIT_AND_FETCH` re-arm ≈ deferred
recycle — same severance boundary).

**Counted-term linkage**: the whole 32.3 % commit-machinery term. **Risk:
medium** — the series is unmerged and its infra was REJECTED upstream in
its v4 form (see sweep: kmbuf dropped from for-7.1, "keep it fuse-internal"
is the stated future direction), so the daemon arm must live behind a
capability probe (`IORING_REGISTER_KMBUF_RING` success = sqz kernel) and
expect a full rebase when the FUSE consumer is reposted.

**Recommendation: KEEP the v1 series as-is in v2 (it already contains the
fix); charter the fuse3 bufring/zc adoption as the companion daemon
campaign. No kernel authoring.**

---

## Candidate 3 — Sideband traffic over the ring (FORGET/INTERRUPT/resend)

**Present-in-base verdict: NOT in base, including with our series.** The
uring fiq ops still route sideband classically
(`dev_uring.c:1789-1796`: `fuse_io_uring_ops = { .send_forget =
fuse_dev_queue_forget, .send_interrupt = fuse_dev_queue_interrupt, … }`);
`fuse_resend` + `FUSE_NOTIFY_RESEND` ride `dev.c` (`:2006`, `:2131`).

**Lore status**: exactly one in-flight patch, FORGET-only —
* Li Wang, **"[PATCH v3] fuse: optional FORGET delivery over io_uring"**,
  2026-04-23, opt-in via `FUSE_IO_URING_REGISTER_FORGET_COMMIT` on
  REGISTER, bumps `FUSE_KERNEL_MINOR_VERSION` to 46 (mirrors:
  lkml.iu.edu/hypermail/linux/kernel/2604.2/10614.html; exact
  message-id not recoverable from the mirrors — lore search is
  bot-walled; libfuse PR #1487, closed pending kernel acceptance).
* Maintainer posture is the verdict: Joanne Koong (2026-04-24,
  lkml.iu.edu/2604.3/01512.html) — "my preference would be to keep
  forget/interrupts on the legacy /dev/fuse path even when io-uring is
  enabled" (batching/fairness lost on-ring, per-request state for one-way
  notifications, ring-capacity contention). Bernd Schubert
  (2604.3/02838.html): "not a high priority", notes the real missing
  feature is *multiple request sizes on one queue*. No v4, not queued.

**Apply-cleanliness vs our base**: poor by construction — written against
Apr-2026 for-next *without* the kmbuf series; it touches
`fuse_uring_cmd_req.flags` semantics and `dev_uring.c` queueing that our
patches 13–19/24 rewrote, and its minor-version bump (46) collides with
the series' 7.46 reservation. Carrying = structural merge in FUSE core
(the banned class) for a maintainer-skeptical patch.

**Win pricing against our shapes**: near-zero. FORGET volume is
metadata-churn traffic already amortized by `FUSE_BATCH_FORGET` on the
classical path, and our sideband session is an io_uring `Readv` servicer
that idles between storms. Moving FORGET alone **cannot delete the
classical sideband session** — INTERRUPT, `fuse_resend` stragglers, and
the `fiq->ops` switchover window stay kernel-classical, and the W1
`notify_inval_inode` ride-along (daemon→kernel notify writes) uses the
same fd regardless. Authoring the full sideband-over-ring ourselves
(forget+interrupt+resend + a notify-over-ring UAPI) is a multi-week
FUSE-core UAPI program against an explicitly hostile maintainer current —
not priceable as a perf win on any counted term.

**Recommendation: SKIP for v2. Watch the thread; revisit only if a
maintainer-endorsed variant that moves ALL sideband classes lands.**

---

## Candidate 4 — Atomic open / lookup+open fusion

**Present-in-base verdict: only the ancient `fuse_atomic_open` create-path
shape; no lookup+open fusion, no compound machinery.**

**Lore status (the live lineage is COMPOUNDS, not the 2022/2023 atomic-open
series):**

* Historic: Dharmendra Singh "FUSE: Implement atomic lookup + open/create"
  (v5, 2022-05-17, `20220517100744.26849-1-dharamhans87@gmail.com`) and
  Bernd Schubert "fuse: full atomic open and atomic-open-revalidate" (v10,
  2023-10-23) — both dead; superseded by compounds per the maintainers.
* **Horst Birthelmer (DDN), "fuse: compound commands"** — the vehicle:
  * v6, 3 patches, 2026-02-26, cover
    **`20260226-fuse-compounds-upstream-v6-0-8585c5fcd2fc@ddn.com`**
    (fetched via b4, 31-message thread read): `FUSE_COMPOUND` container +
    auto-sequentialization fallback when the server lacks compound
    support + open+getattr as the first user. Miklos engaged
    substantively (wants list-of-`fuse_args` plumbing in dev.c,
    per-subrequest flags `FUSE_SUB_IS_ENTRY`/`FUSE_SUB_DEP_ENTRY` — i.e.
    lookup+open IS the design target); Joanne reviewing; Bernd flagged
    open+getattr must stay server-opt-in.
  * **v7, 4 patches, 2026-06-04, cover
    `20260604-fuse-compounds-upstream-v7-0-27331d085c2a@ddn.com`** —
    latest; adds a dentry-revalidate compound (LOOKUP+GETATTR),
    per-subrequest headers/flags. Still unmerged, no maintainer
    sign-off, live STATX-vs-GETATTR design debate (Amir, Bernd), no v8.
    Author claims 15–20 % metadata gains on a distributed-lock backend
    (our exact class).
* **Measured apply-cleanliness vs our patched 6.19.14** (real dry-run):
  `git apply --check` fails (offsets), but `patch -p1 --fuzz=3` lands
  **all 4 patches with ZERO failed hunks** — offsets everywhere plus
  exactly two fuzz-2 hunks in `fs/fuse/dev_uring.c` (they fall in
  kmbuf-reworked territory; manual review of those two sites is the whole
  transplant cost). New file `fs/fuse/compound.c` + `dir.c`/`file.c`/
  `fuse_i.h`/uapi hunks are effectively clean.

**Counted-term linkage**: the D2/D3 round-trip economy
(`docs/design-metadata-throughput.md`): fuse_ops/create 5.18 → ~4.0 landed
by D2.a; **D2.d "atomic-open probe" is the standing recorded reserve** —
the only path to the ≤ 3.5-ops stretch, worth ~1 full under-i_rwsem round
trip on the single-dir serial chain (G2's floor arithmetic: ~8–12 µs of
the ≤ 55 µs under-lock budget). Compound LOOKUP+GETATTR revalidate also
composes with our per-class kernel TTLs on storm shapes.

**Risk class: medium-high** — uapi WILL churn before merge (per-sub
STATX shape is likely in v8), so anything the daemon builds against v7's
uapi is throwaway-by-design; the win only materializes with fork-side
compound handling + SqueezeFS handler fusion (a real daemon workstream).

**Recommendation: CARRY-OPTIONAL (rank below the mandatory set). Include
in v2 only if the rebuild happens after a v8/maintainer-ack; otherwise
defer to v3 — the field win today is metadata-plane, not the counted
32 % data-plane term.**

---

## Candidate 5 — s_time_min/max INIT advertisement

**Present-in-base verdict: does not exist, as predicted.** The only
timestamp field FUSE_INIT carries is `time_gran` (`fuse_init_out`,
uapi:925; consumed at `inode.c:1381-82`); `sb->s_time_min/max` stay at the
VFS defaults (±TIME64), so the kernel keeps huge dates incore while the
daemon persists the clamp — exactly the generic/634 adjudicated diff
(`tests/run_fstests.sh:615-625`, "incore clamping needs sb->s_time_max,
which the FUSE protocol cannot advertise — kernel-interface-only").

**Upstream precedent found (design authority, not a carry):** Darrick J.
Wong's **fuse-iomap** program advertises `s_time_min`/`s_time_max` (+
`s_maxbytes` etc.) via a `FUSE_IOMAP_CONFIG` handshake
(`fuse_iomap_config_out`, flag `FUSE_IOMAP_CONFIG_TIME`;
lists.openwall.net/linux-ext4/2026/02/23/86 and the `fuse-iomap*` branches
on djwong's k.org tree) — proof the concept is upstream-palatable, but the
series is enormous and iomap-coupled; carrying it for two i64s is absurd.

**The sqz patch sketch (~20 lines kernel + ~10 daemon):**

* uapi: burn 16 of `fuse_init_out.unused[11]`'s 22 bytes →
  `int64_t time_min; int64_t time_max;` (+ shrink `unused` to `[3]`);
  new init flag **in flags2 high space, `FUSE_TIME_LIMITS (1ULL << 62)`**
  — deliberately far above upstream's watermark (bit 42 is the last
  allocated in-tree) so an upstream collision is a recompile, not an ABI
  trap; the sqz kernel is a private vehicle, and the daemon gates on the
  echoed flag so stock kernels are unaffected.
* `process_init_reply`: `if (flags & FUSE_TIME_LIMITS && arg->time_max)
  { sb->s_time_min = arg->time_min; sb->s_time_max = arg->time_max; }`
  next to the existing `time_gran` block (`inode.c:1381`). VFS
  `timestamp_truncate()` then clamps incore setattr/utimes exactly where
  the daemon clamps durable state (±9223372036 s, the i64-nanoseconds
  range pinned in `tests/attr_refresh_tests.rs::
  out_of_range_timestamps_saturate_*`).
* Daemon: set the flag + the two i64s in the fork's `fuse_init_out`
  (`session.rs:1251-1265`), gated on the kernel echoing the capability.

**Win**: converts the generic/634 release-gate adjudication into an
expected-PASS **on sqz-kernel hosts only** — the adjudication must stay
pinned for the fleet kernel (the runner's expected-shape logic keys per
kernel). Zero data-plane effect. **Risk: very low** (one guarded branch at
INIT; feature-absent ⇒ bit-identical behavior). This is also the natural
first upstream-submission candidate (tiny, self-contained, precedented by
fuse-iomap's config).

**Recommendation: AUTHOR (sqz patch 0027). Include in v2.**

---

## Candidate 6 — Sweep (fuse + io_uring since ~6.19, counted-term-relevant)

Findings, each with verdict:

1. **kmbuf infra REJECTED upstream (v2-manifest-critical fact).** The
   split-out "io_uring: add kernel-managed buffer rings" v3
   (`20260306003224.3620942-1-joannelkoong@gmail.com`) was applied to
   axboe for-7.1 (~2026-03-20) and **dropped at Joanne's own request
   2026-03-30** (`CAJnrk1ZMmRpLc3uPMuD2jcb8J14gZryb7jno5vN4cNE4OXEBXw@
   mail.gmail.com` on lore.gnuweeb.org/io-uring): kernel-managed buffers
   don't integrate with generic io_uring paths; the future direction is
   **fuse-internal buffer management** (Pavel's suggestion). Consequence:
   the v4-coherent transplant we carry is the ONLY working form of this
   ABI anywhere, and the eventual upstream FUSE-zc will be a DIFFERENT
   ABI — plan the fork's bufring/zc arm behind a capability probe and
   expect a full re-port, not a rebase. **Carry v1 series unchanged.**
2. **bvec-registration split-out MERGED** ("extend bvec registration" v7,
   `20260612184840.4058966-1-…`, applied to axboe for-next 2026-06-12,
   commits `24963b8a6b09`, `8219b09eb4c2`, `0ebfe2954f40`,
   `e2a9be1b1774`). Patches 20–23 of our series are the same
   infrastructure in older form — nothing to do for v2 (6.19.14 will
   never receive the 7.2 backport), but the eventual re-port target for
   patch 24's `io_buffer_register_bvec` consumer is now stable upstream.
   **No action.**
3. **Bernd Schubert, "fuse: {io-uring} Allow to reduce the number of
   queues and request distribution" v4, 8 patches, 2026-04-13**, cover
   **`20260413-reduced-nr-ring-queues_3-v4-0-982b6414b723@bsbernd.com`**
   (v3 thread `20251013-reduced-nr-ring-queues_3-v3-0-6d87c8aa31ae@
   ddn.com` fetched + read): registered-queue bitmaps, cpu→queue mapping,
   NUMA-affine selection, `FUSE_URING_REDUCED_Q` init flag; no
   client/server protocol change; unmerged, under active review, no v5
   yet. **Measured apply-cleanliness vs our tree: FAILS — 21 failed hunks
   even at fuzz 3** (it renames `ring->nr_queues`→`max_nr_queues` and
   reworks queue selection across exactly the `dev_uring.c` our patches
   13–19/24 rewrote). Carrying = structural FUSE-core merging. Win for
   us: memory economy (fewer queues × payload arena — compounding with
   candidate 1's 4× payload growth: 4 GiB → e.g. 1 GiB at 8 queues) and
   kernel-side NUMA routing that duplicates what our NUMA campaign
   already does daemon-side; it deletes NO counted term (`is_ring_ready`
   still requires every *registered* queue concept — it lifts the
   all-possible-CPUs REGISTER obligation, a startup/footprint win).
   **SKIP for v2; first candidate for v3 once it merges (it will rebase
   the transplant for us by forcing a re-port anyway).**
4. **FUSEX** — Miklos' experimental parallel FUSE implementation
   (fs/fuse/fusex.c on the `fusex` branch of mszeredi/fuse.git, ~2026-04-29,
   preceded by the transport/filesystem layer-separation series
   `20260416091658.462783-1-mszeredi@redhat.com`): `FUSE_LOOKUPX`,
   `FUSE_MKOBJX`, `FUSE_SETSTATX`, compound-requests-as-goal, local-only,
   io-uring-only transport interest (Horst). Not mergeable, not carriable
   — but it is WHY 7.1.5's fuse tree diverged so hard (SERIES.md's base
   ruling) and it signals the 2027 protocol direction (fewer round trips
   as first-class ops — our D2 economy thesis validated upstream).
   **Watch only.**
5. **`FUSE_NOTIFY_PRUNE` (7.45) is in our base** (`uapi:687`,
   `dev.c:2070`) — batched dentry-prune notify, **daemon-only
   opportunity, no kernel change**: the reclaim/invalidations the L4 W1
   arm and the lifecycle movers issue one-at-a-time today could batch.
   Not a counted term; file under daemon backlog.
6. **nvme-tcp side: nothing new to carry.** ulp_ddp/DDP receive offload
   remains unmerged (v30 2025-07-15 was the last posting; Jan-2026
   blktests thread proposes deleting its test for lack of mainline
   progress) — the RX-copy term stays owned by our zcrx userspace lane
   (open on BOTH kernels since the ethtool-7.1 correction + PR Z1);
   nothing in 6.19→7.2 io_uring/nvme-tcp touches our counted terms
   beyond what zcrx already exploits.
7. **Large-folio FUSE**: handling merged pre-base (6.16 era; verified
   in-tree — folio-aware `fuse_copy_folio`, tmp-folio-free writeback),
   **enablement not merged anywhere** (blocked on granular dirty tracking
   / iomap per the series notes). No lever for us; candidate 2's kaddr
   path makes it irrelevant to the lock term. **No action.**

---

## The ranked v2 manifest

What the v2 rebuild should contain, in order; base stays **linux-6.19.14 +
the existing 26-patch series** (nothing in the sweep dislodges the "latest
coherent revision" ruling — the upstream form of this ABI got LESS
mergeable since v1, not more):

| Rank | Item | Kernel delta | Class |
|---|---|---|---|
| 1 | **Candidate 5: `FUSE_TIME_LIMITS` INIT advertisement** — sqz patch **0027** (~20 lines: uapi + `process_init_reply`) + daemon INIT arm | AUTHOR (new patch) | very-low risk; converts generic/634 to PASS on sqz hosts |
| 2 | **Candidate 1: 4 MiB max_write** — `fs.fuse.max_pages_limit=1024` | **NONE (sysctl-only)** + daemon campaign (planner-bug fix is unconditional) | low risk, A/B-able end to end |
| 3 | **Candidate 2: bufring/zc daemon adoption** — the 32.3 %-term kill | **NONE (already in the carried series)** — fuse3 fork campaign behind a kmbuf capability probe | medium (unmerged ABI, private to sqz kernel; expect re-port when upstream reposts) |
| 4 | **Candidate 4: compounds v7** (`20260604-…-v7-0-27331d085c2a@ddn.com`) | CARRY-OPTIONAL (applies at fuzz, 2 reviewed hunks in dev_uring.c) | medium-high (uapi will churn; daemon work required before any win) — take only if the rebuild slips past an upstream v8 |
| — | Candidate 3 (FORGET-over-ring), reduced-nr-queues v4, FUSEX, large-folio enablement, ulp_ddp | SKIP | maintainer-blocked / conflicts-structural / no counted term |

**Net kernel delta for v2 = one ~20-line authored patch (0027).** Every
counted-term win in this manifest is unlocked by daemon work against
surfaces the v1 kernel already ships — which is the strongest possible
argument that v2 is CHEAP and the daemon campaigns should not wait for it.

**Rebuild timing vs the reformat window**: build the v2 RPMs whenever
convenient (containerized, dev-box manners), but **boot it WITH the
reformat window**, not before — the window already owes the box a
boot-disruptive session (the dd6c7ea il hold-probe bracket + the zcrx-lane
field acceptance are both queued on it), the one-shot grub discipline
serializes kernel swaps anyway, and nothing in v2 blocks the daemon-side
campaigns (bufring adoption can only be *field-verified* on a kmbuf
kernel, but it develops against the v1 kernel the box is already
running — v1 and v2 are ABI-identical for everything except patch 0027).
