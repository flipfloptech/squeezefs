# 2026-08-04 — sqz kernel v2 patch-set scoping: survey, verification, ranked manifest

Branch `feat/sqz-kernel-v2-scoping` (off dev tip `abc983f`). Charter:
SURVEY + VERIFY + ASSEMBLE only — **no kernel rebuild, no cluster work, no
dev-box load** (the box is Z2's this window). Deliverable of record:
**`docker/kernel-sqz/V2-CANDIDATES.md`** (per-candidate verdicts, lore
refs, apply-cleanliness, counted-term linkage, the ranked manifest). This
note records HOW each verdict was obtained and WHY the ranking landed
where it did.

## 1. Method (what "verified" means in the manifest)

* **Present-in-base verdicts were read from the real tree**: the
  `squeezefs-kernel-sqz-work` podman volume still holds the exact
  patched `linux-6.19.14` the v1 build compiled
  (`…/volumes/squeezefs-kernel-sqz-work/_data/linux-6.19.14`); verified
  it carries the series (kmbuf symbols in `io_uring/kbuf.c`,
  `FUSE_URING_BUF_RING`/`FUSE_URING_ZERO_COPY` at uapi:1301-02) before
  citing line numbers from it. Zero unpack/compile cost, zero CPU beyond
  grep/sed — the read-only-source rule was never even close to strained.
* **Lore access**: the HTML+search endpoints are bot-walled (Anubis /
  nginx 403), but the thread-mbox endpoints b4 uses work — `b4 mbox`
  fetched the kmbuf/zc v4 thread (58 msgs), the compounds v6 thread
  (31 msgs), the reduced-nr-queues v3 thread (26 msgs), and `b4 am`
  produced applyable mboxes for compounds v7 (4 patches) and
  reduced-nr-queues v4 (8 patches). Where lore search was needed, mirror
  archives (lkml.iu.edu, spinics, lore.gnuweeb.org, patchew) + web search
  filled in, and every message-id cited in the manifest was either
  b4-resolved or taken from a fetched cover's own changelog links.
* **Apply-cleanliness is measured, not guessed**: `git apply --check` +
  `patch -p1 --dry-run --fuzz=3 -f` against the patched tree.
  Compounds v7: **0 failed hunks** (offsets throughout, exactly two
  fuzz-2 hunks in `fs/fuse/dev_uring.c` — kmbuf-reworked territory).
  Reduced-nr-queues v4: **21 failed hunks** even at fuzz 3 — structural,
  not carriable without FUSE-core merging.

## 2. The load-bearing source findings (each changed a candidate's shape)

1. **`FUSE_MAX_MAX_PAGES` no longer exists.** The 4 MiB question
   dissolved into a sysctl: `fs.fuse.max_pages_limit` (default 256, hard
   cap 65535 = the `fuse_init_out.max_pages` u16), and fuse-uring's
   `ring->max_payload_sz` FOLLOWS `fc->max_pages`/`max_write`
   (`dev_uring.c:267-268`) rather than bounding them. Candidate 1 is
   **sysctl + daemon-only**.
2. **The fork has a latent REGISTER-refusal bug** the survey found in
   passing: `TransportGeometry::plan` hardcodes
   `KERNEL_MAX_PAGES_LIMIT = 256` (`fuse_over_uring.rs:997`) while the
   INIT reply advertises `max_pages = u16::MAX` — on any kernel whose
   sysctl is raised past 256, `ring->max_payload_sz` exceeds our 1 MiB
   ents and every REGISTER refuses (`dev_uring.c:1481`) ⇒ mount fails.
   Must be fixed regardless of the 4 MiB decision.
3. **The zc series already deletes the FR_LOCKED term** — candidate 2
   required no authoring. `cs->is_kaddr` short-circuits
   `fuse_copy_fill` (`dev.c:879`), so the bufring path pays zero
   per-page lock round-trips and zero GUP; and `can_zero_copy_req`
   covers `in_pages || out_pages` (`dev_uring.c:92`) — **the zc arm
   kills K1 in BOTH directions**, including FUSE_WRITE payloads (the
   mission brief's "writes still pay it" assumption is disproven by the
   tree: write folios register as an ITER_SOURCE fixed buffer the daemon
   can WRITE_FIXED straight to NVMe).
4. **Sideband still classical even in our tree**
   (`fuse_io_uring_ops.send_forget/send_interrupt → fuse_dev_queue_*`,
   `dev_uring.c:1789-96`) — candidate 3's kernel gap is real, but the
   only in-flight patch (Li Wang v3, FORGET-only, 2026-04-23) got a
   maintainer preference AGAINST the whole idea (Joanne, 2026-04-24) and
   cannot delete our sideband session anyway (INTERRUPT/resend/notify
   remain classical).
5. **`FUSE_NOTIFY_PRUNE` is in base** (7.45) — an unplanned sweep find:
   batched dentry-prune invalidation is available to the daemon today,
   no kernel change.

## 3. The upstream-trajectory facts that shaped the ranking

* **kmbuf infra was applied to axboe for-7.1 and DROPPED at the author's
  request (2026-03-30)** — kernel-managed buffer rings will NOT land as
  generic io_uring infrastructure; the future FUSE-zc will be
  fuse-internal and ABI-different. Consequences: (a) the v1 transplant
  remains the only working form of this ABI anywhere — carry it
  unchanged; (b) the daemon's bufring/zc arm must sit behind a runtime
  capability probe and is a knowing throwaway against the eventual
  upstream shape; (c) SERIES.md's "latest coherent revision" ruling got
  STRONGER since v1, not weaker.
* **The bvec split-out (v7) merged** (axboe for-next, 2026-06-12) — the
  re-port target for patch 24's consumer is now stable; nothing to do in
  v2.
* **Compounds (v7, 2026-06-04) is the only live atomic-open lineage**,
  design still moving (STATX-vs-GETATTR), no maintainer ack — hence
  CARRY-OPTIONAL rather than mandatory: it applies at fuzz today, but
  its uapi will churn and its win needs daemon fusion work that
  shouldn't be built twice.
* **fuse-iomap advertises s_time_min/max via FUSE_IOMAP_CONFIG** — the
  upstream precedent that makes our ~20-line `FUSE_TIME_LIMITS` INIT
  patch (candidate 5, sqz patch 0027) both safe to author and plausible
  to submit upstream later.

## 4. The ranking rationale (why this order)

The 32.3 %-of-client-cycles commit-machinery term
(`.benchmarks/2026-08-02-interface-frontier.md` §3 Row A: memcpy 16.9 % +
FR_LOCKED 12.8 % + pin 2.6 %) is the only counted term any kernel-side
candidate touches — and the survey's central result is that **the v1
kernel already ships everything needed to delete it**; what's missing is
daemon adoption (fork bufring/zc arm). So the manifest inverts the
campaign's implicit premise: v2's kernel delta is ONE authored ~20-line
patch (0027, the generic/634 fix), and the throughput program's next
moves are daemon campaigns against the kernel we already booted:

1. **0027 / FUSE_TIME_LIMITS** — v2's only kernel change. Tiny, guarded,
   converts a standing release-gate adjudication into a pass on sqz
   hosts, and is upstream-submission material.
2. **4 MiB max_write** — sysctl + daemon knobs (max_write 1 MiB → 4 MiB,
   planner derives payload_sz from the effective sysctl, R5
   `transport_payload_buffers` 4× under the existing L1 depth
   degradation — 32q × 4 MiB ⇒ depth 16 at the 2 GiB cap). Wins are
   request-count-proportional (one WRITE per 4 MiB block, one lease, no
   kernel splits) and fully A/B-able; the FR_LOCKED term is per-page and
   does NOT shrink with request size — that's item 3's job.
3. **bufring → zc adoption in the fork** — the 32.3 % kill. Bufring
   alone deletes the 12.8 + 2.6 lock/pin term (one memcpy remains); the
   zc arm deletes the 16.9 % memcpy in both directions (upstream prior
   +20–25 % randread @ 1M; our Row-C il measurement — commit machinery
   0 % — is the live proof of what deleting this term is worth:
   +6.8 GB/s on the same shape).
4. **Compounds v7** — optional carry for the D2.d atomic-open reserve
   (the ≤ 3.5 fuse_ops/create stretch); metadata-plane, not the counted
   data-plane term; gated on upstream design settling.

SKIPs: FORGET-over-ring (maintainer-blocked, FORGET-only, can't delete
the sideband session, conflicts with our transplanted uapi);
reduced-nr-queues v4 (21 failed hunks — structural merge in the exact
files our series rewrote; wins are footprint/NUMA-placement we already
own daemon-side; first v3 candidate once merged); FUSEX / large-folio
enablement / ulp_ddp (watch-only — no mergeable artifact or no counted
term).

## 5. Rebuild timing (recommendation to the orchestrator)

Build v2 RPMs whenever a free containerized window exists (recipe delta =
patch 0027 + SERIES.md/V2 note), but **boot with the reformat window**:
that window already owes the client a disruptive session (dd6c7ea
hold-probe bracket, zcrx-lane field acceptance), the one-shot grub
discipline serializes kernel swaps, and v1↔v2 are ABI-identical except
0027 — no daemon campaign is blocked waiting for the boot. Suggested
window sequence: boot v2 → capability matrix (expect only the
FUSE_TIME_LIMITS delta) → reformat-window items → generic/634 single-test
run on the sqz kernel to bank the PASS.

## 6. Campaign hygiene

Charter compliance: no rebuild, no boots, no dev-box load (source reading
= grep/sed over the retained volume; the only network was lore/b4 +
mirrors); no cluster contact. Artifacts: fetched thread mboxes + b4 am
outputs under `/tmp` (ephemeral by design); the two committed documents
are the deliverables. The `SQUEEZEFS_FSTESTS_QUICK`/release-gate posture
is untouched — candidate 5 explicitly keeps the generic/634 pinned shape
for fleet kernels and adds a per-kernel expected-PASS only when the sqz
kernel is the boot target.
