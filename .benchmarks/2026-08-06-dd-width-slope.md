# 2026-08-06 — Direct-drive width re-grade: the drain-LANE pair moves to the counted 3×cpus/8 slope

**Branch:** `perf/dd-width-slope` (worktree off `integrate/zcrx-wave`
`e4c7d798`). **User ruling (verbatim):** "these need to be derived
values not fixed." **Lineage:** the 530k-ceiling note's filed next lever
(`.benchmarks/2026-08-05-il-530k-ceiling.md` §FIELD ADJUDICATION — "a
width sweep … prices whether the DIRECT-DRIVE shard width (not the
svc-thread ceiling globally) deserves its own class-measured slope").

## The counted field sweep (the measurement rows)

**Venue:** squeeze-test (32 CPUs / 2 nodes, real nvme-tcp fabric), the
balance-fix pair live (`owners == shards == W` mid-row on every width).
**Instrument:** `tests/fio/width_sweep_and_read_decomp.sh` (commit
`e4c7d798`) — il fio libaio rand-4k direct 32 procs × qd32, 3×30 s rows
per width, **W8 brackets at BOTH ends**, remount per width setting
`SQUEEZEFS_IPC_SERVICE_THREADS=$W SQUEEZEFS_IPC_DD_SHARDS=$W`
(the pair — see §Structure below for why it cannot be split), per-width
owners/shards/svc gauges + pidstat on row 2, engagement exact.

| width (lever) | IOPS (3 rows) | clat | note |
|---|---|---|---|
| W8 (derived default, cpus/4) — start bracket | 629–636k | — | |
| **W12** | **695–700k** | **1461–1472 µs** | **+10.4 % vs W8 — best** |
| W16 | 675–690k | — | +7 % |
| W24 | 616–626k | — | **−2 % — regression** |
| W8 — end bracket | 622–633k | — | no drift: the brackets close |

A genuine INTERIOR optimum at 12 on the 32-CPU box — bracketed on both
sides, with the venue's stability pinned by the closing W8 bracket.

## Structure (why the re-graded object is the LANE PAIR, and why 3×cpus/8)

1. **Candidate (c) — "give the dd shard width its own slope, keep the
   svc ceiling at cpus/4" — is structurally a NO-OP, not merely
   unmeasured.** Governed submits ride `lane = owner_idx % width`
   (`set_service_lane` in `IpcHost::service_loop`), and owner indices
   are bounded by the service-thread ceiling: a shard set wider than the
   ceiling is production-DARK (only the foreign-thread test fallback
   ever reaches those rings). Conversely a ceiling wider than the shard
   set shares reapers across lanes. The sweep set both knobs together
   AND the machinery requires both to move — the honest object of the
   re-grade is the **drain-lane width**, one number for both halves.
2. **Candidate (a) bare (`cpus × 3/8` as a fit) is numerology; the
   budget form (b) is the same number WITH the story.** One lane = TWO
   OS threads (the `sqz-ipc-svcN` submitter + the `sqz-ipc-ddN` reaper —
   the 530k note measured the pair saturating at ~93 % / ~82–85 % CPU).
   In lane-thread/core terms the sweep's grid is `2W/cpus ∈ {0.5, 0.75,
   1.0, 1.5}`: the optimum sits at **2W = ¾ × cpus** (24 daemon drain
   threads on 32 cores, the complement left to the co-located 32-process
   client fleet), W16 = 1.0× is already past the peak (+7 % < +10.4 %),
   and the ONE sampled point past 1.0× — W24 = 1.5× oversubscribed — is
   the ONE regression. The budget story's failure mode is verified
   inside the same sweep. `W = 3×cpus/8` is `2W = ¾·cpus` solved for W,
   and equals 1.5× the ingest-measured cpus/4 slope.
3. **The ingest surfaces are SUBSUMED by dominance, never retuned on a
   rand-4k sweep.** The shim's per-mount session default — the surface
   the 2026-07-28 +44 % ingest experiment actually measured — keeps
   `il_sessions_default` = `clamp(cpus/4, 2, 16)` verbatim (its
   `squeezefs-preload` tie test is untouched). The daemon ceiling's law
   against it is now DOMINANCE: `⌊3c/8⌋ ≥ ⌊c/4⌋` pointwise with equal
   floors, so every default session still owns its own drain thread at
   every machine size, and spawn-on-bind keeps idle spares
   unrepresentable — the DEFAULTS-MISMATCH topology cannot recur in
   either direction. (`docs/rc-manifest.md` perf item 2 had already
   filed the svc ceiling as "the next bottleneck-by-constant" for
   256-process fleets — a second workload class pointing the same way.)
4. **The fuse3 drain-group width keeps its own cpus/4 slope** — its
   counted bracket is its own venue (kernel-lane transport, 2026-08-05
   ingress-queue-spread); this re-grade is the ipc drain-lane class
   only. Same for `uring_fs::resolve_worker_count`.

## The landing

- **`squeezefs_ipc::sizing::il_drain_lanes_default(cpus)` =
  `clamp(3×cpus/8, 2, 64)`** (saturating) — the ONE drain-lane
  derivation; `service_thread_ceiling_from` and `dd_shards_from` both
  default to it (lane-pair equality is a red test). Canonical shapes:
  32 ⇒ 12, 96 ⇒ 36, 64 ⇒ 24, 4 ⇒ 2.
- **Floor 2** = the pre-L4-8 single-consumer plateau; never-regress
  holds POINTWISE (`⌊3c/8⌋ ≥ ⌊c/4⌋` — no box shape derives below the
  previously shipped width). **Rail 64** = the explicit-lever clamp
  parity (`SQUEEZEFS_IPC_{SERVICE_THREADS,DD_SHARDS}` admit 1..=64 and a
  default must be expressible as an explicit setting; engages only at
  cpus ≥ 174 — harmless where absent).
- **`service_thread_count` sizes from `crate::cpu::process_parallelism()`**
  (was `available_parallelism()`): the Hang-1 process-mask law, and the
  runtime half of the lane-pair equality — `dd_shards()` already used
  the process mask, so the two halves now read the SAME cpu count on a
  pinned-first-toucher schedule too.
- Env levers unchanged (explicit wins verbatim, clamp 1..=64,
  unparseable ⇒ derived). No new knobs; the two registry entries'
  default strings updated.

## Honest scope (what one box shape cannot say)

The sweep identifies the FRACTION at one cpu count (32) — among forms
through 12-at-32, `3×cpus/8` is chosen for the lane-pair budget story
and its same-sweep-verified failure mode, not because one box can
distinguish a slope from an affine form. **Filed confirmation rows**
(the slope's falsifiers, to run before any further re-grade):

1. a second box shape's width bracket (a different cpu count — the
   96-CPU class row would separate `3c/8` from any affine fit);
2. the write-side fleet check — the svc ceiling governs ring writes
   too: re-run `tests/fio/fleet_parity_row.sh` (the bs=1M ×256 psync
   rows) on the new default and confirm the write residual (il/kernel
   0.83) does not worsen (rc-manifest item 2 predicts it helps or is
   neutral: more drain width was that item's named direction);
3. the ingest regression guard — the 2026-07-28 large-sequential shape
   (single-process, 8 sessions on 32 CPUs) is structurally unchanged
   (8 sessions ⇒ 8 spawned threads under either ceiling), so any moved
   number there indicts the balance pick, not this slope.

## Red evidence

`fb5eda8a` — the contracts red (E0432: no `il_drain_lanes_default`):
the dd tie test re-graded to the lane derivation + canonical shapes
(32 ⇒ 12, 96 ⇒ 36, 4 ⇒ floor 2), the ingest tie test DELIBERATELY
rewritten to lane-pair equality + dominance (the subsumption law — the
shim-side pin untouched), the derivation-sweep pin (floors/rails +
pointwise dominance 1..=512). Green at the fix commit.

## Suites (targeted, D12 posture; all on the final SHA)

- `ipc_direct_drive_tests`, `ipc_admission_balance_tests`,
  `ingest_economy_tests` (the re-graded ties green; async suites ×10),
  `derivation_sweep_tests` (22), `env_knob_convention_tests` (21),
  `ipc_host_tests`, `numa_affinity_tests`, `squeezefs-ipc` crate (37,
  incl. the new sizing pins + pointwise dominance 1..=4096),
  `squeezefs-preload` crate (the UNTOUCHED shim-side session pins).
- clippy `--all-targets --all-features` AND `--all-targets` (shipped):
  clean `-D warnings`; `cargo fmt --check` clean; fuse3 fmt/clippy clean
  (comment-only edit there).
