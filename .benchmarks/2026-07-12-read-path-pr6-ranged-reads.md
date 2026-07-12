# PR 6 — R3 sub-block ranged reads: the rand-4k amplification kill (2026-07-12)

**Branch:** `feat/read-path-pr6` (base dev @ `8b1bd1a` = PR 5 tip). Design:
`docs/design-read-path.md` §5.6 + the PR 6 plan entry. Lineage: PR 1 baseline
(`…-read-path-baseline.md` — row 3 = 326–331 IOPS, ≈730× read amplification, raw-substrate
control 363–815 k IOPS), PR 4 (row 3 = 325), PR 5 (row 3 = 369; same substrate/protocol,
same harness, 8 GiB cage, 3.5 GHz cap, Tctl ≤ 64 °C, quiet-gate). Raw artifacts:
`~/tmp/sqperf/results/p6a_*`, `p6b_*`.

## THE RAND-4K VERDICT (row 3, 8 threads × 4 KiB × iodepth 16 × 30 s, cold 16 GiB set)

| Measure | PR 1 baseline | PR 6 (`p6a_row3`) | Gate | Verdict |
|---|---|---|---|---|
| IOPS (last-done) | 302–331 | **37,066** | ≥ 30× baseline (≥ ~9,180) | **121× — PASS** ✓ |
| Device-read bytes / user bytes | ≈ 730× | 4,277 MiB / 4,344 MiB user = **0.98×** | ≤ 2× | **PASS** ✓ |
| Device writes during the read | 27.7 GiB (tier publish per fetch) | **0** | — | tier traffic eliminated ✓ |
| vs same-session raw-substrate control | 0.04–0.09 % | 37,066 / **408,041** = **9.1 %** | ≥ 50 % target (R-10) | **shortfall — recorded, judged at PR 8** |
| vs FUSE-round-trip fallback control (R-10) | — | 37,066 / **302,758** = 12.2 % | pre-agreed context row | recorded ✓ |

Counters (`p6a_row3` Δ): `ranged_reads` +1,094,809 ≈ `get_obj` +1,094,833 (every op a
4 KiB-window device read — `get_obj` counts ranged ops by design, §5.6);
`ranged_read_bytes` +4.48 GB ÷ (1.09 M × 4,096 user bytes) = **1.0002× window
amplification** (aligned elbencho shape: zero bounces); `hot_block_hits` +17,398 (block
re-touches served from the hot puts that whole-block fetches leave — ranged fills
themselves never publish); prefetch quiet.

**Controls, both recorded (R-10 framing):** the same-session raw-substrate control
(elbencho directly on files on the same /home NVMe: seq-write 5,198 MiB/s, rand-4k
**408,041 IOPS**) and the pre-agreed FUSE-round-trip-bound fallback — warm hot-tier
rand-4k through the mount (128 MiB set, zero device work: `hot_block_hits` +754 k,
device reads 44 MiB): **302,758 IOPS**. The residual row-3 gap (37 k vs 303 k transport
ceiling) is per-op resolution cost past the transport (metadata + block-map + binding
recheck per ranged serve — two map resolutions per op), not amplification (1.0×) and not
the transport itself; this is exactly the residual the design "explicitly does not own"
(§5.6 expected-effect note), left to PR 8's cumulative judgment.

**Kill-switch A/B (same binary, fresh format, `SQUEEZEFS_READ_RANGED_THRESHOLD=0`,
`p6b_row3`):** 493 IOPS, device reads **44.5 GiB** + writes **27.9 GiB** for ~58 MiB of
user reads (the whole-block shape, ≈770× amplification + the publish carousel) vs
ranged-on 37,066 IOPS / 4.28 GiB / 0. One flag, 75× IOPS and ~1000× less device traffic —
attribution airtight.

## Row 2 unchanged (streams don't range)

`p6a_row2`: **6,644 MiB/s** vs same-session row 1 3,858 (**1.72× inverted** — PR 5:
6,598/3,867 = 1.71×); device reads 16,352 MiB (1.00×), writes 0. The classifier keeps
streams on the whole-block + pipeline path; `ranged_reads` stays 0 on the row-2 shape.

## Rows 1/4/5

| Row | Lineage band | PR 6 | Verdict |
|---|---|---|---|
| 1 fresh create | 3,532–4,282 | 3,858 / 3,772 | flat ✓ |
| 4 overwrite | 649–869 | 654 | in-band ✓ |
| 5 rand-4k write | 63–152 (state-noisy) | 145–152 | in-band ✓ (daemon alive; the row-5 cage class remains PR 7's scenario) |

## Ciphertext-hole closure (§5.6 sibling-leg hygiene) + the framing bug it uncovered

- **Sibling gate landed:** the raw full-block dest leg now refuses transform configs
  (`is_passthrough()` gate; transform configs fall through to the validated whole-block
  loop, which decodes). Pinned: `raw_dest_leg_refuses_transform_configs` — a small-block
  (64 KiB) lz4 volume with a payload dest serves **decoded plaintext**, never
  lz4-frame/ciphertext bytes.
- **Pre-existing decode bug found by that pin, fixed forward (`9ff64c2`):**
  non-passthrough `process_write` images were not self-delimiting, but block reads return
  the full `block_size` window — so **cold DEVICE reads of every compressed/encrypted
  striped block failed loud** (`lz4_flex` rejects trailing bytes with `OffsetZero`; AEAD
  opens `data[header..]`, so padding fails the tag check). Reproduced through the full FS
  **on the parent commit** and standalone against `lz4_flex` (frame + zero padding ⇒
  `OffsetZero`; exact frame ⇒ OK). Cache/staging-served reads masked it, which is why
  write-path suites stayed green. Fix: every non-passthrough image is framed
  `[u32 LE image_len][image]` (one const, three touch points); passthrough stays
  byte-identical (R3 depends on it). Forward-only: unframed legacy blobs refuse loud —
  their cold reads never worked, so there is nothing behavioral to preserve. Pinned in
  `tests/crypto_block_framing_tests.rs` (lz4 / zstd / lz4+aes256gcm full-FS cold
  round-trips on cache-less AND staged-growth topologies; padded-window decode with
  non-zero padding; loud refusal for unframed/truncated; passthrough identity).

## Mechanism (delivered per §5.6)

`get_block_range_for_index` + `RangedDest` (zero-copy leg: 4 KiB-aligned request DMA'd
straight into the registered payload arena; bounce leg: ≤ request+8 KiB window into the
pooled aligned buffer, padding never served) · `BackendRouter::read_block_range`
(existing offset-read worker, conservative 4096 LBA per approved OQ #1, debug-asserted) ·
binding-validated fill discipline verbatim (incarnation before/after + current-map
recheck after bytes-in-hand, MAX_REBINDS + whole-block fallback, `Ok(None)` = hole) ·
never published, never single-flighted, ghost heat record-only (convergence via the next
whole-block fetch — consulting it for ranged dispatch would violate the pinned
N-disjoint-reads contract) · dispatch in the single-block arm AND the multi-block
per-block task, strictly after overlay/hot/tier probes · `SQUEEZEFS_READ_RANGED_THRESHOLD`
(default 262144, 0 = kill switch; small-block volumes excluded — their whole-block fetch
is already request-sized) · stats: `ranged_reads`, `ranged_read_bytes`,
`ranged_read_unaligned_bounces`, `ranged_read_rebinds`.

**Suite pins (the churn/WINDOW=0 precedent):** `read_tier_admission_tests`,
`hot_block_tier_tests`, `read_prefetch_pipeline_tests`, `read_tier_refetch_churn_tests`
fixture-pin `RANGED_THRESHOLD=0` — they pin the WHOLE-BLOCK machinery whose sub-block
reads now legitimately range (§5.6's granularity policy names this split); the ranged
path's own contracts live in `tests/ranged_read_tests.rs`.

## Gates

- **Cargo gate (tip):** clippy `--all-targets --all-features -D warnings` clean; fmt
  clean; `cargo test --all-features -- --test-threads=1` full-project green; doc 0
  warnings; bench smoke Success. (Loom: no new atomic protocols — ranged state is
  per-call; counters are single-word Relaxed.)
- **fstests QUICK (`MEMMAX=8G`):** 19 ran — failures {**generic/003, generic/213**} = the
  documented platform expected-fail set, plus **generic/618 in-suite** = the accumulated-
  state cage cascade (PR 4/5 lineage): **618 standalone on this tip passes 3/3** (A/B
  attributed, same disposition as the PR 5 gate).
- **074/075/091/616/617 explicit re-run:** all five **passed** inside the QUICK run
  (074/075/091/616 in the main pass; 617 likewise; 618 standalone 3/3).
- **LTP syscalls (`MEMMAX=8G`):** **PASS 174 / FAIL 0 / BROKEN 0 / SKIP 9** ✓.
- Churn `get_obj` assertions byte-identical (fixture-pinned); pipeline + admission + hot
  suites green 3–5×.
