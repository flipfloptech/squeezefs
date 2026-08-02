# 2026-08-04 — fuse3 transport geometry + zc adoption: the REGISTER-bound law, 4 MiB max_write, and the kmbuf bufring arm

Branch `perf/fuse3-zc-adoption` (off dev tip `6d87b11`, **unmerged — the
orchestrator merges**). Charter: (1) fix the geometry REGISTER-refusal
bug unconditionally, red-first; (2) negotiate max_write up to block
size, sysctl-gated; (3) adopt the carried series' kmbuf bufring/zc ABI
behind a runtime capability probe. Tree-verified foundations:
`docker/kernel-sqz/V2-CANDIDATES.md` (candidates 1–2) +
`.benchmarks/2026-08-04-sqz-kernel-v2-scoping.md`; counted terms:
`.benchmarks/2026-08-02-interface-frontier.md` §3 Row A (FUSE commit
machinery ≈ 32.3 % of ALL client cycles = memcpy 16.9 % + FR_LOCKED
locking 12.8 % + fill/GUP/unpin 2.6 %). Design amendment:
`docs/design-zero-copy-write-path.md` **§5.4c** (the normative geometry
law + degradation table + the kmbuf composition argument); operator
posture: `docs/operations.md` §"FUSE max_write geometry".

## 1. The geometry law (item 1 — the unconditional bug fix)

**The bug** (found by the v2 scoping in passing, fixed here red-first):
`TransportGeometry::plan` hardcoded a 256-page payload floor while the
INIT reply advertised `max_pages = u16::MAX`. The kernel computes
`fc->max_pages = min(fs.fuse.max_pages_limit, advertised)` and refuses
REGISTER ents smaller than `ring->max_payload_sz = max(8192,
max_write, fc->max_pages × page)` — so ANY box with the sysctl raised
past 256 refused every REGISTER and **failed the mount** (over-uring is
mandatory).

**Proof pair (dev box, 7.1.5-cachyos — the sysctl exists, default 256):**

- **A-side (pre-fix dev tip `6d87b11`, built from a clean worktree):**
  `sysctl fs.fuse.max_pages_limit=1024` → format → mount →
  `FUSE-over-io_uring worker failed during setup: qid=1: Invalid
  argument (os error 22)` → **mount FAILS**, kernel log
  `fuse: Invalid req payload len 1048576` (the exact predicted
  signature, one line per refused REGISTER).
- **B-side (this branch):** same sysctl, same shape →
  `test_raised_sysctl_mounts_with_4mib_ents` (privileged, run under
  sudo) — mount SUCCEEDS with `transport_max_write=4194304`,
  `transport_max_pages=1024`, arena = queues × depth × 4 MiB; an 8 MiB
  write rides 2–4 payload leases (the 1 MiB shape pays 8); round-trip
  byte-exact; sysctl restored by the test's drop guard.

**The law** (fuse3 `TransportGeometry::plan`, unit-pinned against a
verbatim fs/fuse/dev_uring.c mirror — `kernel_ring_max_payload_sz` in
the fork suite): negotiated `max_write = clamp(desire, max(page,4096),
sysctl × page)`; advertised `max_pages = ceil(max_write/page)` — the
INIT reply describes the plan exactly, so the kernel bound EQUALS the
registered ents by construction and the failure class is
unrepresentable. `page` from sysconf (portable-by-default), sysctl
fallback 256 where the proc file is absent (= those kernels'
compiled-in constant). The blanket `DEFAULT_MAX_PAGES = u16::MAX`
advertisement is deleted. Today-shape pin: sysctl 256 + 1 MiB desire ⇒
byte-identical geometry (`test_plan_default_sysctl_shape_is_byte_identical`).

## 2. 4 MiB max_write (item 2) + the degradation table

Daemon INIT desire = `max(block_size, 1 MiB)` ("up to block size" opens
the ceiling, never lowers the floor); `SQUEEZEFS_FUSE_MAX_WRITE`
overrides the desire verbatim (the field bracket's A/B lever, still
sysctl-gated). **sqz-host posture: `fs.fuse.max_pages_limit=1024`**
(documented in operations.md; fleet kernels at the 256 default stay at
1 MiB gracefully — and now they *mount* either way). Expected win
(V2-CANDIDATES candidate 1): 4→1 WRITE ops per 4 MiB block on the seq
path — one payload lease, one merge; per-op fixed costs (dispatch, lock
discipline, commit batching, wake economy) quarter. The per-page
FR_LOCKED term is per PAGE and does not shrink with request size —
that's §3's job.

**The variable-ent budget ladder** (the L1 depth-degradation policy
re-derived; R5 cap `min(mem_budget/8, 2 GiB)` unchanged; depth leg
first, payload leg only past the floor-4 violation, base = the 1 MiB
pre-campaign ent, never below):

| shape (32 queues, cap 2 GiB unless noted) | max_write | max_pages | depth | arena |
|---|---|---|---|---|
| sysctl 256 (default / absent), desire 4 MiB | 1 MiB | 256 | 32 | 1 GiB *(today, byte-identical)* |
| **sysctl 1024, desire 4 MiB (field shape)** | **4 MiB** | **1024** | **16** | **2 GiB** |
| sysctl 1024, cap 819 MiB | 4 MiB | 1024 | 6 | 768 MiB |
| sysctl 1024, cap 256 MiB (payload leg engages) | 2 MiB | 512 | 4 | 256 MiB |
| sysctl 1024, cap ≤ 128 MiB (base pin) | 1 MiB | 256 | 4 | 128 MiB *(pre-L1 posture)* |
| sysctl 64 (lowered) | 256 KiB | 64 | 32 | ≤ cap |

Whether depth 16 × 4 MiB beats depth 32 × 1 MiB on the write-wall/EXA
rows is **the field A/B** (BDP says yes for bandwidth shapes; the
probe-up governor rows judge) — `SQUEEZEFS_FUSE_MAX_WRITE=1048576` is
the lever. Gauges: `transport_max_write` / `transport_max_pages`
(stats inode — the engagement instrument for the field row).

## 3. kmbuf bufring / zc adoption (item 3 — the -sqz vehicle)

**Module boundary:** `crates/fuse3/src/raw/connection/kmbuf.rs` — every
constant, layout, probe, and per-queue resource of the carried v4
series' ABI in ONE severable module (upstream dropped the kmbuf infra
from for-7.1 at the author's request 2026-03-30; the eventual upstream
FUSE-zc will be a DIFFERENT ABI — this is a knowing throwaway kept
re-portable).

**Capability lattice** (every cell contract-pinned):

| surface probe (`IORING_REGISTER_KMBUF_RING`, the kmbuf_smoke.c shape) | lever | resolved mode |
|---|---|---|
| Present (sqz kernel) | default | **BufRing** — kmbuf arm |
| Present | `SQUEEZEFS_FUSE_KMBUF=0` | UserEnts (A/B lever, logged) |
| Absent (`EINVAL` — stock kernels, incl. this dev box) | any | UserEnts — **today's path byte-identical** |
| ambiguous errno | any | UserEnts, loud warn (never arm on ambiguity) |
| post-probe registration refusal | — | **mount FAILS loud** (no silent downgrade; the lever is the escape) |
| `SQUEEZEFS_FUSE_ZC=1` | — | recognized, **loudly declined** (face present, serve integration staged) |

**What the bufring arm covers — BOTH directions** of the lock/GUP term:
`FUSE_URING_BUF_RING` REGISTERs (no iovecs, `init.flags`,
`sqe->buf_index = ent_idx`; wire encoding pinned byte-for-byte), fixed
headers buffer (index 0, 288 B/ent stride), kernel-managed payload
bufring (bgid 0, `buf_size = payload_sz`, pow2 entries ≥ depth), region
mmap, and the delivery-side **attachment law** (flagged CQE re-points,
unflagged keeps — the kernel's reuse case; stale attachments provably
unreachable by body writes; violations = loud shutdown). Kernel-side,
`cs->is_kaddr` short-circuits `fuse_copy_fill` for request payloads AND
reply bodies — the 12.8 % + 2.6 % term dies; one memcpy per folio (K1)
remains.

**How kmbuf joins the §5.4 lease machinery** (the deepest edit, argued
in §5.4c): `PayloadArena::from_kmbuf` gives the leases/wake/coalescer a
bid-indexed view over the mmap'd kernel region (mapping owned by
`KmbufQueue`, held alive by the arena Arc for lease lifetimes). The
severance law composes UNCHANGED: kernel recycles only at fetch ← fetch
only on our COMMIT_AND_FETCH ← the §5.4 gate already defers that until
the last lease drops. Deferred re-arm ≡ deferred recycle. Reply bodies
and `get_payload_buffer` in-place serves target the ATTACHED buffer —
exactly the kernel's commit-copy source.

**What is staged (honest scope):** the `FUSE_URING_ZERO_COPY` **serve
integration**. The negotiation face ships (flags composition,
`init.queue_depth`, the sparse-table shape with headers at index
`depth`), and the scoping proved the kernel covers zc in both
directions (`can_zero_copy_req` = `in_pages || out_pages`) — but with
zc negotiated the kernel *skips the folio copy entirely*, so the daemon
must serve READs via `READ_FIXED` into (and consume WRITE payloads via
`WRITE_FIXED`/fixed-buffer DMA from) the request folios registered at
`ent->fixed_buf_id`. That is a cross-crate program through the
read-serve and write-through paths, executable ONLY on the sqz kernel —
the staged follow-on PR, guarded by the `fuse3_zc_replies` gauge that
ships now (0 by construction until it lands; the read-inplace
silent-disengagement lesson).

**Instruments added:** `fuse3_kmbuf_negotiated` (0/1, set truthfully
only after the all-queues-REGISTERed barrier — the field arm proof),
`fuse3_zc_replies`, and **`commit_flush`** — the 5th phase of BOTH
`read/write_transport_phase_ns` families: the COMMIT-carrying
ring-flush syscall duration (the venue of the kernel's commit-side copy
machinery). Per-FLUSH sampled — mid-pass flushes always (wait-free
`submit()`), loop-bottom flushes only when entered with the CQ already
non-empty (saturated passes) — never a second syscall, never park-time
pollution; attribution per op class via the delivered opcode. The kmbuf
A/B methodology: same workload, `SQUEEZEFS_FUSE_KMBUF` on/off,
`commit_flush` median shift = the killed commit-side lock/GUP term;
`fio clat − transport_total` shift = the delivery-side residue.

## 4. Gates (dev box, thermal law observed: taskset 8-15, jobs 8, nice 10)

- **fuse3 fork root:** `cargo test --all-features` **60 passed / 0
  failed** (13 new: 6 geometry-law + 7 kmbuf/wire contracts);
  `cargo clippy --all-targets --all-features -- -D warnings` clean;
  `cargo fmt --check` clean.
- **Root:** full gate green — clippy `-D warnings` clean, fmt clean,
  `cargo test --all-features -- --test-threads=1` complete-suite pass,
  `cargo doc --no-deps` clean, bench smoke green (see the closing gate
  run recorded in the branch tip commit).
- **Red-first record:** geometry contracts committed RED
  (5 failed / 28 prior green) against a stubbed pre-fix planner, then
  the fix; root gauge contracts committed RED (missing
  `transport_max_write`) then the negotiation. Loom: not required — no
  lock-free core changed (the §5.4 lease_core/wake_core protocols are
  untouched; kmbuf composes AROUND them, argued in §5.4c, and the
  attachment table is a single-writer-per-ent atomic level).
- **Local live rows (sysctl present on this kernel):** the privileged
  raised-sysctl mount repro (sudo) green — B-side of the proof pair in
  §1; kmbuf probe Absent here, so the UserEnts branch of every
  capability contract is the live-exercised one.

## 5. FIELD-OWED (transport change class — the statfs lesson, stated loudly)

No loaded soak ran on this dev box (transport changes are field-owed by
standing rule). **The reformat window's manifest grows these rows:**

1. **kmbuf arm proof on the -sqz kernel:** mount the staged pair on
   `6.19.14-sqz` → expect `buffers=kmbuf-bufring` in the REGISTER log
   line and `fuse3_kmbuf_negotiated=1`; `SQUEEZEFS_FUSE_KMBUF=0`
   control mount reads 0. Then the standing correctness quick-pass
   (SQUEEZEFS_FSTESTS_QUICK) on the armed mount — the bufring arm's
   first real-kernel exercise.
2. **A-B-B-A zc-on/off** (i.e. kmbuf on/off — `SQUEEZEFS_FUSE_KMBUF`)
   at qd8 1M seq + rand-4k, medians of 3, both orders (the store ages):
   throughput + the `commit_flush` phase-delta table (before/after —
   the killed 12.8 %+2.6 % term made visible) + `fio clat −
   transport_total` residue shift.
3. **The 4 MiB-write row:** `sysctl fs.fuse.max_pages_limit=1024` +
   default mount (desire = block size) vs `SQUEEZEFS_FUSE_MAX_WRITE=
   1048576` control — write_matrix + the standing amplification columns
   (device bytes ÷ user bytes, `wareq-sz`, `block_free_*`), plus
   `transport_payload_leases`/ops engagement (4→1 per block) and the
   depth-16×4 MiB vs depth-32×1 MiB probe-up-governor adjudication.
4. **≥ 600 s sustained soak** on the winning configuration (flat across
   the window per the 2026-07-29 sustained-state rule), tripwires zero
   (`transport_lease_overlong`, `transport_parked_commits` bounded,
   `fuse3_zc_replies` still 0, `writer_guard_fenced` 0).
5. generic/634-adjacent: none of this touches the timestamp surface; no
   new adjudications expected.

## 6. Staging & hygiene

Rocky8 container pair (KD-7 same-commit daemon+shim) staged at
`/scratch/tmp/{squeezefs,libsqueezefs_il.so}.fzc` + `fzc.sha` (sha256
verified byte-exact after transfer; journaled in
`/scratch/tmp/agent_runs.log` at drop time). **File drop only — NO
mounts, no deploys** (epoch lock). The staged `--version` line + the
sha file on-box name the exact commit. Dev-box hygiene: builds under
taskset 8-15 / 8 jobs / nice 10; Tctl never above ~65 °C observed;
pre-fix proof worktree removed; sysctl restored to 256 after every
privileged row.
