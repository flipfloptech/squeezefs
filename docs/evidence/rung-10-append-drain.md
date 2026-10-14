# Rung 10 — an extending append's drain, the transformed tail, ONE `.stats` document

Owed by `rung-10-dev-perf.md` §10.4. Decisions 0003 §AK.1–§AK.7; 0002 rows 534–536.

**Instrument.** Perf container `sqz-appamp` (1 MDT + 2 OST raw partitions, dist build, 4 MiB
blocks), formatted WITHOUT `--disk-cache-paths`. Shapes: `echo500` (500 × `echo <72 B> >> f`),
`fork2000` (one fd, 2,000 `/bin/echo`), `py{1k,4k}_{close,fsync}` (300 × open-append-close, ± an
fsync). Bytes per appended byte = OST diskstats ÷ file growth; both open postures.

**Cause.** A tail append is stream-adjacent, so it parks a DEFERRED full buffer; its drain read
the whole block (the complement) and uploaded the whole block to a fresh destination. A held-open
appender's first write parks a W2 page overlay and its second escalated it (a whole-block seed).

**Bound (§AK.1, §AK.3).** Over a sole-owner passthrough mapping a drain of `u` bytes writes its
touched pages in place: `pages(u, 1) × P` per drain, `≤ pages(e, a) × P / e` per appended byte;
edge seeds ≤ bytes written. Crash law = W1's: an edge page's untouched bytes are rewritten with
themselves — a kill at any grain leaves old bytes intact, each appended byte old or new. Per
block: one whole landing at its first drain (write-before-publish), so the harness verdict
`append_drain_bound`: `device ≤ (pages(bs, a) × P / bs + 1) × appended + files × P`.

**Red rows.** `write_admit.zig`: "the extending append's drain on a cache-less set" (65,536 B per
72 B drain), "the in-place drain's crash law" (a torn DMA at every grain), "a held-open appender's
ONE drain"; `write_e2e_linux.zig` (nvme_dev: ×57.7 per appended byte against ×58,254);
`transform.zig`, `read_path.zig` (a short image: EIO, then a panic), `accumulate.zig` (superseded
and trimmed-image ledger rows), the smoke row "THE TRANSFORMED APPEND"; `stats.zig`, `budget.zig`
and every smoke `.stats` read (strict parse).

**Passthrough, before → after (OST bytes, × per appended byte).**

| shape | control before | control after | no-open before | no-open after |
|---|---|---|---|---|
| echo500 (36 KB) | 1,740,644,352 (48,351×) | 1,851,392 (51.4×) | 4,202,496 (117×) | 40,960 (1.14×) |
| fork2000 (144 KB) | 4,198,400 (29×) | 147,456 (1.02×) | 12,591,104 (87×) | 151,552 (1.05×) |
| py1k_close | 1,170,219,008 (3,809×) | 1,216,512 (3.96×) | 4,202,496 (13.7×) | 311,296 (1.01×) |
| py1k_fsync | 1,237,327,872 (4,028×) | 1,216,512 (3.96×) | same | 1,216,512 (3.96×) |
| py4k_fsync | 1,249,910,784 (1,017×) | 1,228,800 (1.00×) | same | 1,228,800 (1.00×) |

Σ device-write arms ≡ OST diskstats to the byte on every row.

**Transformed (§AK.4–§AK.6).** Before: every append past 4 KiB answered EIO (the image seam read
the chunk-long window into a block-sized seed buffer). A transformed block cannot be patched in
place (a frame rewrite reuses its nonce), so its drain lands the image whole, now trimmed to the
live bytes: **per drain `≤ storedImageBound(r) = alignUp(table + r + ⌈r/F⌉τ, P)`**, per appended
byte `storedImageBound(r)/u` — the CoW price grows with the live extent `r ≤ B`. The untrimmed
image stored every zero frame raw on an encrypted set (≈ 4.2 MB per drain). Measured aes-256-gcm,
control arm: echo500 5.4 MB (151×), py1k_fsync 47.0 MB (153×, `Σ storedImageBound(r_i)` to 0.2 %),
py4k_fsync 186 MB (152×); lz4: echo500 1.2 MB (34×), py4k_fsync 1.9 MB (1.5×). Short images now
read zeros past their plaintext (a ranged read EIO'd; the fill's short decode panicked a serve,
`index 245760, len 65536`). `write_through_bytes`, `write_pipeline_superseded_bytes` (now an arm)
and `own_block_dma_bytes` count the stored bytes: Σ arms ≡ diskstats on all six aes rows.

**`.stats` (§AK.7).** Repeated keys: `/meta/node/meta_kv_appender_fence_breach` (a counter beside
the tripwire of the same site — the counter is gone) and `/mem_budget/mem_budget_components/
block_alloc_table` (one per data volume — one entry per name with `instances` and sums). Rails:
the render test parses strictly; every smoke `.stats` read parses strictly; `use_first` is gone.

**Found, not fixed.** `DEVICE_WRITE_REQS` double-counts DMAs that ride `put_obj` (wareq-sz only);
a short transformed image's whole-block fill still DMAs the chunk; a write past EOF inside a
promoted block reads recycled bytes between the old end and the write (unchanged).

**Gates.** Full venue gate on the rebased tree: 49/49 steps, 2,917 / 2,997 (80 skipped); macOS
29/29, 2,266 / 2,295 (29 skipped).
