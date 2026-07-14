# Root-daemon bench EIO — residue-history hypothesis EXCLUDED (2026-07-13)

Branch `fix/root-daemon-bench-eio` off dev @ `c0dab9c` (takeover session; the
prior agent's lab at `~/tmp/rootd_eio` was inherited intact). Rails: taskset
0-15 + JOBS=12, 3.5 GHz cap untouched, Tctl 57-75 °C, aging daemons caged 8G
(`systemd-run --scope -p MemoryMax=8G -p MemorySwapMax=0`), the faithful
user-recipe daemon deliberately uncaged, unique paths `/tmp/rootd_mnt_*`,
`/mnt/squeezefs` and the live re-gate harness untouched. **Every run below
executed while the full fstests `-g auto` re-gate was live on the same box**
— the user's load condition, reproduced by construction.

## Verdict in one line

**The residue-history recipe does NOT reproduce the user's bench EIO — 3/3
PASS with proven residue present — and the code audit shows why it cannot:
v3 format burial is classification-blind (the "virgin path skips burial"
hypothesis is structurally false), now pinned by a mechanistic test.**

## The hypothesis (charter rank 1)

The user's meta volumes carried days of prior-format residue, and their
workaround for the pre-watermark format refusal was `dd if=/dev/zero bs=1M
count=1` over each **meta** volume head only. `format --force` then classified
the volumes **Blank** and took the *virgin* path. Hypothesis: the virgin path
skips the reformat burial machinery (53f8347/6350d91 quick-reformat burial,
8b9cdc1 watermark), admitting surviving journal-ring payloads, node frames,
or allocator residue beyond the 1 MiB horizon.

## Burial-delta audit: recognized-reformat vs virgin (charter job 3)

Read: `src/meta_backend/kv/builder.rs` (`format_preflight`, `format_v3`,
`ImageBuilder::build`, `node_seq_base`), `src/meta_backend/kv/superblock.rs`
(`classify_sector0`, `SuperblockV3::plan`), `src/main.rs` format arm
(~1731-2030).

**There is no delta. Classification decides only the *gate*; every wipe/bury
mechanism runs unconditionally downstream of it:**

| Mechanism | Where | Classification-dependent? |
|---|---|---|
| Preflight gate (refuse / allow / live-client probe) | `format_preflight` | **Yes — gate only, zero side effects** (Blank ⇒ allowed without `--force`; recognized ⇒ `--force`; live clients refuse always) |
| Staging/cache dir wipe | `main.rs` format arm | No — wiped whenever `--disk-cache-paths` is configured |
| Fixed-region zeroing `[0, heap.start)` — superblock + ledger + **whole journal ring** + bitmap | `ImageBuilder::build` (§9 quick-format hygiene) | **No — unconditional**, and the range comes from the *freshly planned* geometry (`SuperblockV3::plan`), never from the old superblock. For the user's 10 GiB meta volumes this zeroes ~33.7 MiB — a strict superset of their 1 MiB dd |
| Heap node-frame burial | `node_seq_base(uuid)` (6350d91) + persisted mint watermark (8b9cdc1) | No — a **fresh random uuid is minted per format invocation** (`BuilderConfig::new`), so dead frames fail the §4.5 `node_seq_at_write == node_seq` admission regardless of what the old superblock said (or whether one existed) |
| Fresh ino space / allocator | bootstrap `LedgerRecord { seq: 1, next_ino, alloc_bitmap_generation: 1, node_seq_watermark }` | No — built from the in-memory description only |
| Data-volume head wipe (quick: `min(capacity, 32 MiB)`) | `main.rs` format arm | No — data volumes are never classified |

The user's actual dd was therefore *redundant with what format was about to
zero anyway*; the only thing it changed was the **gate** (Blank ⇒ no refusal).
The pre-watermark refusal they were working around was the dev-tip behavior
where `format_preflight` propagated the classify error hard —
`fix/format-force-unsupported` (pending, 3f51a7a) re-routes exactly that
class to the plain `--force` gate, downstream of which the same unconditional
burial runs.

**Pending-branch verification (the user's future reformat path):** built
`3f51a7a` in the lab worktree and ran a throwaway mechanistic test
(`force_gate_over_refused_superblock_buries_all_residue_classes`): aged
volume (160 files + 8 KiB xattrs ⇒ ring residue past 1 MiB) → forge
pre-watermark superblock → `format` without force refuses pointing at
`--force` → with force reformats → **whole ring zero, heap frames survive
but no ghost served, fresh uuid + watermark bit, first create mints ino 2**.
PASS. The dd-zeroed-head pin (below) was also cherry-picked onto 3f51a7a and
passed — both ladders bury identically.

## Empirical: residue-history recipe 3/3 PASS (charter job 1)

`~/tmp/rootd_eio/residue.sh` per iteration, current binary (dev @ c0dab9c)
throughout:

1. **Aging, two generations**: fresh 4×10G meta + 4×50G data file volumes →
   `format --force` → caged root-daemon mount (`--allow-others --uid/gid
   1000`) → **full bench saturation suite** (24g, O_DIRECT) → 3000-file
   metadata churn (16k writes + fsync, xattrs, renames, unlinks) → clean
   umount → **second `format --force` (recognized-superblock path)** → mount
   → 800-file churn → clean umount.
2. **The workaround, faithfully**: `dd if=/dev/zero bs=1M count=1
   conv=notrunc` over each **meta** volume only (data untouched) →
   `format --force` (volumes classify Blank ⇒ virgin path) → **uncaged
   root-daemon mount** (exact user recipe incl. `--log-file`) → residue
   probes → **full bench saturation suite**.

Residue presence at the dd (proving the recipe is non-vacuous — nonzero
16-byte od lines in `[1 MiB, 35 MiB)`, i.e. surviving ring/bitmap/heap bytes
per meta volume): it1 110k-127k, it2 122k-127k, it3 114k-129k lines.

| It | bench rc | daemon alive | daemon ERRORs | ghost entries | replay_entries (Σ 4 vols) | verdict |
|----|----------|--------------|---------------|----------------|---------------------------|---------|
| 1 | 0 | yes | 0 | 0 | 0 | PASS |
| 2 | 0 | yes | 0 | 0 | 0 | PASS |
| 3 | 0 | yes | 0 | 0 | 0 | PASS |

Post-bench counters (it1 representative): `meta_kv_node_dropped_tail_bsets
0`, `meta_kv_replay_dropped_torn [0,0,0,0]`, `writeback_retry_exhaustions 0`,
`uring_queue_full 0`, `staging_generation_discards 0`, `stale_binding_rebinds
0` — no latent admission or writeback signature either.

## The pin (regression armor)

`144d829` — `tests/kv_backend_tests.rs::v3_dd_zeroed_head_virgin_format_buries_all_residue_classes`
(GREEN on dev at commit time — exclusion evidence, armor thereafter):
two aged generations → dd 1 MiB head zero → **classifies Blank** (the
workaround premise) → virgin format **without** `--force` succeeds → ring
bytes proven present past the dd pre-format and **all-zero post-format**;
heap frames proven to **survive** (burial-by-admission, not erasure) while
ghost dentries/xattrs are refused across trees; first create mints **ino 2**;
fresh generation uuid; identical knobs re-plan identical geometry.

## What this leaves for the user's failure (charter job 2)

Excluded with evidence, all under live `-g auto` load (their box condition):

- pristine exact recipe (prior agent artifact `bench_run1.out` + this
  session's 3× re-run — see acceptance table),
- stale-staging variant (prior agent, `stale/bench.log`),
- **residue-history variant (this note, 3/3)**,
- first-suite-after-format cold path (every gen3 bench above IS the first
  suite after its format),
- root-daemon-as-variable (prior agent's caged/uncaged matrix + all runs
  here run the daemon as root).

Their 18:21-18:24 EDT rand-write EIO (elbencho fine — it never fsyncs)
therefore needs **their fingerprint**: the next failing run carries
`--log-file` per the standing arrangement; the 18:27:33 daemon disappearance
stays UNCONFIRMED as a crash (no umount in the sudo audit; cosmic-files was
active — a GUI/udisks unmount is plausible). Candidate fingerprint surfaces
when it arrives: per-volume `meta_kv_*` families, `writeback_retry_
exhaustions`, ranged-read/EIO lines in the daemon log, dmesg block-layer
errors on their NVMe (my lab is file-backed; a device-level EIO under
saturation would look exactly like their report and never reproduce on
files).

## Acceptance

| Gate | Result |
|---|---|
| Residue-history recipe (aged 2 gens + dd-head-zero + virgin `format --force` + uncaged root-daemon + full bench), under live `-g auto` | **3/3 PASS** (rc=0, daemon alive, 0 ERRORs, 0 ghosts, 0 replayed entries; residue proven present pre-format) |
| Pristine exact user recipe (same binary), under live `-g auto` | **3/3 PASS** (rc=0, daemon alive, 0 ERRORs) |
| Unit pin on dev (`144d829`) | GREEN (and green cherry-picked onto 3f51a7a) |
| Pending-branch force-gate burial (throwaway vs 3f51a7a) | PASS |
| cargo clippy/fmt/test(all-features, single-thread)/doc/bench-smoke | ALL GREEN on the branch tip |
| fstests QUICK (curated set, 19 cases) | 16 pass; generic/074 flake solo-green on retry; **generic/003 + generic/213 fail identically on dev-tip code in the owner's `-g auto` inventory** (pre-existing, owned by that effort — not this branch: zero src delta) |
| LTP syscalls | PASS (0 TFAIL / 0 TBROK) |
| elbencho mount bench | PASS (write 3.7 GiB/s, read 28-31 GiB/s on the quick config) |

Housekeeping: the prior agent's untracked red-WIP `tests/
mount_owner_override_tests.rs` (the sudo-format root-ownership papercut —
separate defect, fix never started) was moved out of the test tree so gates
run clean; the papercut remains open and documented in that agent's session
notes and this handover.
