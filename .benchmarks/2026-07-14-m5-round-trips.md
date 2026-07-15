# PR M5 acceptance — round trips: clean-handle fast path, FLUSH elision, negative entries (G7)

| | |
|---|---|
| **Program** | metadata-throughput (`docs/design-metadata-throughput.md`), PR M5 — §5.1 D1.d + §5.2 D2.a–d + survey riders P1-B/P1-C (`~/tmp/refclients_20260714/survey.md`) |
| **Branch** | `perf/fuse-round-trips` (off dev `cb6ee9c`) |
| **Box / rails** | same 3.5 GHz-capped box as the baseline/M2/M6; daemons caged (`systemd-run --user --scope -p MemoryMax=8G -p MemorySwapMax=0`), binaries named `sqm5` / `sqm5dev` (kill-pattern immunity), kills by PID only; Tctl 44–66 °C throughout |
| **Substrate** | file-backed sandboxes on the btrfs home fs (A2-class; sanctioned for non-barrier work), default cadence |
| **Shape** | mdstorm 8 threads × 100 k (mkdir/create/stat/rename/unlink/mfcreate/mfunlink/rmdir), per-phase `.stats` snapshots; rig-ON (SQUEEZEFS_OP_PROFILE=1) op-mix pairs + rig-OFF timed pairs ×2/side; targeted 1-thread and negative-disabled probes |
| **CO-TENANT CAVEAT** | a root juicefs experiment (podman + 4 mounts at /mnt/juicefs) occupied the box the whole session (load 20–43); **every timed row carries the harness DIRTY flag** and is context-only. The gate rows are **per-op counter ratios — exact integer deltas over exact op counts, load-invariant** (the M6-sanctioned posture). Left untouched per rails; `/mnt/squeezefs` never approached. |

## Verdicts up front

1. **The FLUSH round trip is GONE, measured on the real kernel**: create-phase
   FLUSH 1.000/op (dev) → **0.000/op** (exactly ONE FLUSH per session — the
   ENOSYS latcher). But not via the design's FOPEN_NOFLUSH bet: the running
   7.1.3 kernel's own uapi header scopes that bit to "don't flush data cache
   on close (**unless FUSE_WRITEBACK_CACHE**)", and a live probe measured
   FLUSH still arriving 1.0/close with the bit advertised under this daemon's
   default writeback-cache mount. The honored elision switch under wb-cache
   is the standard FUSE optional-op protocol: **clean-handle FLUSH replies
   ENOSYS → the kernel latches `fc->no_flush`** and stops sending FLUSH
   connection-wide, while `fuse_flush` still runs `write_inode_now` +
   `fuse_sync_writes` + `filemap_check_errors` *before* the latch check
   (dirty pages still write back at close; close(2) still sees writeback
   errors; the kernel converts the ENOSYS itself). FOPEN_NOFLUSH stays
   advertised for `--no-writeback` mounts where the kernel honors it
   per-handle.
2. **G7 (fuse_ops/create 5.18 → ≤ 4.2): met on two of three shapes, missed
   on the one-dir 8-thread row for a measured kernel-side reason.**
   Many-dirs create (mfcreate) **5.011 → 3.994** ✅; solo (1-thread one-dir)
   **4.992 → 3.968** ✅ (stretch-adjacent); one-dir 8-thread **5.203 →
   4.992** ❌ (≤ 4.2 not met). The −1.0 elision landed exactly in every
   shape; the 8-thread residual is **parent-permission refetch
   amplification**: each create invalidates the parent's kernel attrs
   (`fuse_dir_changed`) and each create syscall makes ~two permission-bearing
   parent checks; with M5's tighter per-op pacing a concurrent create's
   invalidation now lands *between* a walker's two checks nearly every time,
   saturating parent GETATTRs at 2.0/op (dev's looser pipeline: 1.21/op).
   Attribution is direct: a debug-logged 8T run shows **9,950/10,000 extra
   GETATTRs target the parent ino**; a negative-caching-disabled control
   (`SQUEEZEFS_FUSE_NEGATIVE_TTL_MS=0`) reads the identical 4.992 (not
   D2.b's doing); the 1-thread row reads 1.001 getattr/op (no interleave, no
   amplification). Every one of those extra round trips is served at
   **~1.0 µs** from the D2.c-refreshed attr cache (pure transport cost);
   the one-dir create row still got *faster* end-to-end (context rows
   below). This is kernel traffic the daemon cannot suppress under
   `default_permissions`; recorded with numbers per the §5.8(c) escalation
   rule — the residue is transport-priced and lands in D3/M7 territory
   (fewer/cheaper syscalls per round trip), not handler territory.
3. **D2.c closed the trailing-GETATTR cost class**: unlink-phase GETATTR
   **1.693/op @ ~22.6 µs (dev; M2 measured 1.82 @ 45 µs) → 1.058/op @
   ~1.0 µs**, rename-phase 1.105 @ 22.6 µs → 1.042 @ 1.0 µs — count −0.6/op
   on unlink AND ~20× cheaper per hit (refresh-instead-of-invalidate:
   parent + child re-seeded from the RAM-authoritative backend after
   unlink/rename; values exact and monotone through the M6 fold).
   Unlink fuse_ops/op **5.683 → 5.069**.
4. **D2.b negative dentries work end-to-end on the real kernel**: 200
   repeated stats of one missing name reached the daemon as **5 LOOKUPs**
   (1 s TTL re-probes; pre-M5: 200) — a 40× repeated-miss reduction — and
   create-after-miss is immediately visible (no stale negative; the
   handler-level pin + fstests generic/001/013 cover the dcache
   conversion). Honest scope held: the create-storm row itself is unmoved
   by D2.b (unique names; the negative-disabled control measured it).
5. **D2.d probe (measurement only)**: the kernel advertises init bit 42,
   which is **FUSE_REQUEST_TIMEOUT** per its own build-tree header
   (7.43-era; `/usr/include/linux/fuse.h` lags at 7.45-without-bit-42 —
   the probe's "unknown bit" log caught the skew exactly as designed).
   **No atomic-open-class capability is advertised** at protocol 7.45;
   absence recorded, adoption remains a follow-up if a kernel ever offers
   it.
6. **G4/M6 surfaces intact**: journal entries/op create 1.0057 (both tips),
   rename 1.0025 (dev) vs 1.0083 (M5), unlink 1.0192 vs 1.0244 — the small
   M5-side excess is ambient heartbeat entries scaling with the longer
   rig-ON phase walls under co-tenant load (mkdir reads 1.0057 on both),
   not a new committer. `meta_updates` 2.0/op on rename/unlink unchanged
   (the SETATTR echo still arrives and is still absorbed).

## G7 op-mix table (rig-ON pairs, counter-exact; 100 k ops/phase, 8 threads)

| phase | tip | fuse_ops/op | lookup | create/unlink/rename | getattr (cost class) | flush | release | setattr | forget |
|---|---|---:|---:|---:|---:|---:|---:|---:|---:|
| create (one dir) | dev | **5.203** | 1.000 | 1.000 | 1.207 @ 1.0 µs | **1.000** | 1.000 | — | — |
| create (one dir) | M5 | **4.992** | 1.000 | 1.000 | 1.999 @ 1.0 µs | **0.000** | 1.000 | — | — |
| mfcreate (many dirs) | dev | **5.011** | 1.000 | 1.000 | 1.000 @ 1.0 µs | **1.000** | 1.000 | — | — |
| mfcreate (many dirs) | M5 | **3.994** | 1.001 | 1.000 | 1.000 @ 1.0 µs | **0.000** | 1.000 | — | — |
| unlink | dev | **5.683** | 1.000 | 1.000 | **1.693 @ 22.6 µs** | — | — | 1.000 | 0.999 |
| unlink | M5 | **5.069** | 1.000 | 1.000 | **1.058 @ 1.0 µs** | — | — | 1.000 | 0.996 |
| rename | dev | 5.088 | 1.975 | 1.000 | 1.105 @ 22.6 µs | — | — | 1.000 | — |
| rename | M5 | 5.011 | 1.971 | 1.000 | 1.042 @ 1.0 µs | — | — | 1.000 | — |
| mfunlink | dev | 4.858 | 1.000 | 1.000 | 1.000 @ 5.7 µs | — | — | 1.000 | 0.868 |
| mfunlink | M5 | 4.896 | 1.001 | 1.000 | 1.000 @ 1.0 µs | — | — | 1.000 | 0.888 |

Controls (30 k creates): 1-thread one-dir M5 **3.968** (getattr 1.001);
1-thread dev 4.992 (flush 1.000); 8-thread one-dir M5 with negative
caching disabled **4.992** (getattr 1.997 — the residual is not D2.b).
Getattr-target attribution (debug-logged 8T run): 9,950/10,000 to the
parent ino.

**G7 ledger vs the design's arithmetic**: 5.18 − 1.0 (D2.a) = 4.18 held
exactly where pacing is uncontended (mfcreate 3.99, solo 3.97 — the
mfcreate row also banks D2.c since its getattrs were already cache-warm).
The one-dir 8T row instead traded op mix: −1.0 FLUSH + **+0.79 parent
GETATTR @ ~1 µs** = 4.99. M6's +0.21 rename residual (parent-attr
revalidation) is the same phenomenon's first sighting; M5's tightening
made its create-phase form saturate. Net one-dir daemon time per create
still fell (timed context below).

## New-counter rows (M5 machinery, per op)

| phase | flush_fast/op | release_fast/op | negative/op | attr_refresh/op |
|---|---:|---:|---:|---:|
| create (M5) | 0.000 (kernel latched — 1 total) | **1.000** | 1.000 | 0 |
| mfcreate (M5) | 0.000 | **1.000** | 1.000 | 0 |
| rename (M5) | 0 | 0 | 1.000 | **1.000** |
| unlink (M5) | 0 | 0 | 0 | **2.000** |

(create-phase negative/op = 1.000 is the pre-create LOOKUP miss now
replied as a cacheable negative entry; the kernel's EXCL-create walk
revalidates it regardless — no row moved by it, as designed.)

## Timed rows — context ONLY (every row DIRTY: co-tenant load 20–43)

Paired same-window A/B, rig-OFF, ops/s; medians of 2 runs/side where the
window wasn't spike-hit:

| row | dev @ cb6ee9c | M5 tip | note |
|---|---:|---:|---|
| create one-dir | 6,392 (r2; r1 1,789 spike-hit) | **6,726 / 6,832** | +7 % class despite +0.79 op/create |
| mfcreate many-dirs | 26,776 / 26,461 | **29,701 / 28,370** | +9 % class |
| unlink one-dir | 5,367 / 5,195 | 5,406 / 5,541 | parity-to-plus |
| rename one-dir | 3,801 / 5,074 | 4,694 / 4,902 | noise-bounded parity |
| stat | 251,071 | 257,098 | parity |

Do not quote these as clean numbers — the load ramp owned the box (the
dev r1 create row at 1,789/s is the proof). The counter tables above are
the acceptance; the next quiet session (M7's slot is natural) should
collect clean before/after ops/s pairs. A box-quiet monitor ran through
this session and never fired.

## Red→green evidence (the TDD trail)

- RED `8fac8b4`: clean-handle fast-path tests fail (counters never move),
  negative-lookup + TTL-knob suite fails 7/9, supervisor sysfs plumbing
  fails 4/7 (state machine landed with its spec — pure logic).
- GREEN `ee9c7fc` (D1.d/D2.a/D2.b/D2.d/TTLs + vendored-fuse3 negative
  `ReplyEntry` with wire-encoding tests + `KernelInit` INIT surface),
  `6cc7cae` (supervisor + `--supervise` + README).
- RED `5469aa2` → GREEN `f2ce54a` (D2.c refresh-instead-of-invalidate;
  3 contract tests flip).
- Amendment `77826d1` (kernel-verified): FOPEN_NOFLUSH scoped by the
  kernel to non-writeback opens → clean-FLUSH ENOSYS latch added; the
  live probe (141 FLUSHes before, exactly 1 after) is the red→green.
- Full gate at every commit: clippy `-D warnings` clean, fmt clean,
  `cargo test --all-features -- --test-threads=1` green (81/81 binaries),
  `doc --no-deps` 0 warnings, bench smoke green.

## External verification

- **fstests (root, singles pinned by the design's M5 row)**:
  `generic/001`, `generic/013`, `generic/075` — **3/3 pass** at the branch
  tip (075 is the open/close-churn regression net for the FLUSH-latch
  change; 001/013 exercise create-after-miss over cached negatives).
- Survey features are correctness/ops surfaces, cargo-tier only per the
  mission: TTL knobs (env + `-o` options; non-default values test-pinned
  to every reply surface) and the `--supervise` external watchdog (state
  machine + sysfs abort plumbing test-pinned; the full wedge→abort→
  manual-restart loop against a live mount is a **manual-verify runbook
  item** — it requires root and a deliberately wedged daemon).

## Honest residuals

1. **G7's one-dir 8-thread row lands at 4.99, not ≤ 4.2** — kernel-side
   parent-permission refetch amplification (mechanism + attribution
   above; ~1 µs/hit daemon-side). Levers that could close it are outside
   M5's charter: dropping `default_permissions` (a mount security-posture
   change), kernel-side invalidation-mask changes, or D2.d atomic-open if
   a kernel ever offers it. mfcreate (3.99) and solo (3.97) meet the gate;
   the program's G2 target (one-dir throughput) is M7's ledger where the
   under-lock µs — not the ~µs-priced trailing ops — dominate.
2. **The no_flush latch is connection-wide**: after the first clean close,
   dirty handles also stop sending FLUSH. By design review: their FLUSH
   work was soft (fsync is the barrier), kernel dirty-page writeback at
   close is untouched (runs before the latch cut), and RELEASE's
   background flush covers the daemon side. generic/075 (fsx churn) is
   the regression net and passes.
3. **Timed rows are all DIRTY** (co-tenant). Counters carried the gate —
   the M6 precedent. Quiet-session throughput pairs deferred to M7's
   acceptance slot.
4. **M5 rig-ON unlink/rename walls ran slower than dev's** in this
   session's spike windows (rig instrumentation + load; rig-OFF pairs
   show parity-to-plus). Rig-ON rows are never quoted as timing.

## Artifacts

`~/tmp/m5_rt_20260714/`: `results.tsv` (all rows with load/tctl headers),
`stats/<tag>/` (per-phase pre/post `.stats` + daemon logs),
`fstests_g{001,013,075}.log`, harness (`lib.sh`, `run_m5_storm.sh`,
`analyze_m5.py`), `bin/sqm5{,dev}` + `devtree/` worktree (dev @ cb6ee9c).
Sandboxes unique per run and deleted at run end; daemons killed by PID
only; `/mnt/squeezefs` and `~/tmp/nvme/*` untouched.
