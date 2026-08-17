# 2026-08-17 — Block-map string-encoding bracket: sizing the packed-binary displacement question

Branch `perf/block-map-encoding-bracket` (off dev `ea0853a8`, **unmerged —
sizing campaign, never merges itself**). Charter: the user-raised question
(2026-08-17) — block mappings are STRINGS in the metadata plane
(`CachedMetadata.block_map: Arc<HashMap<u32, String>>`; `persist_block_key`
emits bare decimal `"offset"` for the default backend — the overwhelming
population — plus `"be://offset"`, decorated `"bk:off:len"`, and the bit-13
`"offset@base36"` forms; `DataRouter::parse_block_mapping`,
`src/routing.rs:5442`, parses them at use sites). Is the string round-trip a
real cost, and would a packed binary form (`u64 offset + form discriminant +
u32 off/len + incarnation`) pay for an on-disk format change? Per the
2026-08-01 ruling, displacement requires **counted measurement, never
suspicion** — this note is the counted answer. **No production code
changed**: the packed prototype lives in bench code only
(`benches/write_path_bench.rs` — the `block_map_encoding` group + packed
rows extending `write_layout_publish`).

## 1. Instrument, substrate, load (the honesty block)

* **Instrument**: Criterion microbenches (`cargo bench --bench
  write_path_bench -- block_map_encoding` / `-- write_layout_publish`),
  default 100-sample runs, medians reported. Pure CPU microbenches on an
  aging-free venue — the A-B-B-A rule does not apply (no shared store ages
  across runs); the repeat discipline is Criterion's own sampling plus a
  full second pass (§2 col 2), per the campaign spec.
* **The string parse under test is a bench-local VERBATIM mirror** of the
  private `DataRouter::parse_block_mapping` (src/routing.rs:5442) over the
  same pub `BackendRouter` machinery — pinned in-file, because this
  campaign's law is zero production changes. The composed rows put it where
  it runs: u32-keyed map get + parse (the string-hash-vs-u64 term is NOT in
  scope — the map key is already u32 on both sides).
* **Substrate**: not applicable (no device I/O in any row; the
  `BackendRouter` fixture's backing file is never touched by the measured
  code). Box: 32-CPU zenpower box, rows pinned `taskset -c 8-15`, `nice -n
  10`, per `tests/run_bench_baseline.sh`'s pattern; Tctl 60–63 °C
  throughout (< 80 °C thermal law).
* **Load / provisional labeling — BOTH PASSES ARE PROVISIONAL**: the box
  was shared throughout (rung 17's qemu fleet leg + intermittent foreign
  rustc builds). The bounded wait-for-quiet loop (comm-exact `pgrep -x
  cargo/rustc/squeezefs` + load1 < 2.0 + Tctl < 80 °C, 60 s polls, ~62 min
  total budget) NEVER fired — load1 bottomed at ~2.5 with the qemu leg
  resident — so per the campaign spec these rows publish PROVISIONAL.
  Mitigations, stated: rows pinned to cores 8–15 at nice 10; comm-exact
  foreign-work checks were clear at pass 2 start (the qemu/VM load is not
  cargo-class churn); pass 1 (load1 4.6→3.8) and pass 2 (load1 2.5→4.6)
  agree within ~1–4.5 % on EVERY row, and the decision margins below are
  10×–40× and 2× wire bytes — no plausible quiet-box correction moves the
  verdict.
* **Weighting law** (why bare decimal is THE row): `persist_block_key`
  emits the bare decimal offset for every default-slot block;
  the write-commit-economy streaming rows published 2,048/2,048 whole-block
  striped entries (`.benchmarks/2026-07-30-write-commit-economy.md` §5.1).
  Decorated 3-part is the promoted-staged/spill/clip minority
  (`patch_ineligible_decorated`); `be://offset` is multi-volume sets;
  `@base36` is incompat bit 13, which NOTHING stamps (ruling D9) — priced
  as the upgrade's cost, weight zero in the mix row.

## 2. Parse + warm-lookup rows (`block_map_encoding`)

| Row | Pass 1 (provisional) | Pass 2 (provisional) | Per-element |
|---|---|---|---|
| parse_string_bare_decimal_4m (`"4194304"`) | 52.10 ns | 51.49 ns | — |
| parse_string_bare_decimal_11digit | 54.59 ns | 54.69 ns | — |
| parse_string_named_backend (`vol-…://off`) | 62.58 ns | 61.49 ns | — |
| parse_string_decorated_3part (`off:0:len`) | 116.59 ns | 116.69 ns | — |
| parse_string_stamped_base36 (`off@…`, bit 13 — unstamped fleet) | 96.24 ns | 99.16 ns | — |
| **packed_decode_28b** (six fixed-offset LE reads) | **2.50 ns** | **2.50 ns** | — |
| parse_string_field_mix_1024 (1,008 bare + 8 named + 8 decorated) | 56.27 µs | 56.78 µs | 54.9 ns/mapping |
| packed_decode_mix_1024 | 331.5 ns | 331.0 ns | 0.32 ns/record |
| warm_lookup_string_get_parse_1024map (`Arc<HashMap<u32,String>>` get + parse) | 68.09 ns | 65.29 ns | — |
| **warm_lookup_packed_get_1024map** (`HashMap<u32,PackedMapping>` get) | **10.64 ns** | **10.54 ns** | — |

**The parse term, named: ~52–55 ns/op on the dominant form (~57 ns/op
composed at the warm-lookup shape); a packed decode would take it to
~2.5–10.6 ns.** The whole displaceable term is therefore **≈ 45–57 ns per
mapping resolution**. The sub-ns mix-row packed figure additionally shows
the fixed-width record is prefetch/ILP-friendly in a linear walk — real,
but it only amplifies a term already priced in nanoseconds.

## 3. Encode rows + wire bytes (`write_layout_publish`, extended)

| Row | Pass 1 (provisional) | Pass 2 (provisional) | Per-entry |
|---|---|---|---|
| full_save_encode_1024_blocks (string, bench `sqz:vol-…` keys) | 30.52 µs | 31.35 µs | 29.8 ns |
| delta_encode_64_entries (string, bench keys) | 228.2 ns | 231.5 ns | 3.6 ns |
| delta_encode_64_entries_field_bare_keys (string, field bare-decimal keys) | 232.5 ns | 221.7 ns | 3.6 ns |
| delta_decode_64_entries (string) | 3.92 µs | 3.79 µs | 61.3 ns |
| delta_apply_64_on_1024_base (string fold) | 119.1 µs | 118.7 µs | — |
| packed_full_save_encode_1024 | 7.04 µs | 7.04 µs | 6.9 ns |
| packed_delta_encode_64_entries | 482.7 ns | 483.6 ns | 7.5 ns |
| packed_delta_decode_64_entries | 27.6 ns | 27.7 ns | 0.43 ns |

(The packed delta ENCODE reads slower than the string's 228 ns because the
prototype is a naive 7-`extend_from_slice`-per-entry loop while
`LayoutDelta::encode` reserves and bulk-writes; a production packed encode
would be a memcpy-class floor below both. Either way both are sub-µs per
64-entry publish window — encode ns/op cannot justify anything.)

**Wire bytes (deterministic — the `[bmap bytes]` print):**

| Form | Delta-64 marginal B/entry | Full-save-1024 total |
|---|---|---|
| String, bench `sqz:vol-…` keys (~30-char historical shape) | 44 | 51,279 B |
| **String, field bare-decimal keys (the dominant population)** | **16** (= 4 block + 2 len + 10-digit key) | ≈ 23 KiB (arithmetic: 1,024 × (4 block + 8 bincode len + 10–11-digit key) + header; the measured 51,279 B row carries the 38-char bench keys at 50 B/entry, same formula) |
| **Packed prototype (28-B record + 4-B block)** | **32 (fixed)** | 32,768 B |

**The packed form is 2× LARGER on the wire than the field's dominant
string entry.** The string form's decimal offset is a compact varint in
disguise (≤ 13 B for any real device offset, 7–11 B typical); the packed
record pays fixed 28 B to carry an incarnation (0 on every fleet volume —
D9 stamps nothing), a rel_off/len (whole-block on the dominant population)
and a backend slot (default on the dominant population). A *minimal*
packed record (u64 offset + 1 form byte = 13 B/entry vs the string's 16)
would save ~3 B/entry ≈ 19 % of entry bytes — and entry bytes are only a
minority of the per-block journal cost (sustained rewrite J-bytes/block =
188–193 B, `.benchmarks/2026-07-30-write-commit-economy.md` §5.1, so
~3 B/entry ≈ **1.6 % of journal bytes/block**).

## 4. The end-to-end proportion (published decompositions; no live mount this window)

No live mount was read: the box had no mounted squeezefs and rung 17's
fleet legs own the substrate this window — creating a devsub beside them
risks collision, and the published decompositions suffice (as the campaign
spec allows). Anchors:

* **Read serve** (`.benchmarks/2026-08-01-serve-decomposition.md` §3.1,
  qd8 cold-read phase table, fio clat 10.50 ms/op): `key_resolve` sits in
  the "rest ≤ 0.01 ms" tail — i.e. **≤ 10 µs/op measured in the field,
  ≤ 0.1 % of the op**. The microbench says the parse itself is ~55 ns; even
  the full measured `key_resolve` phase (which contains more than the
  parse) is noise against block_fetch 4.81 ms, queue_wait 1.55 ms,
  dispatch_lag 1.70 ms. Deleting the string parse ENTIRELY (the packed
  ceiling) buys ≤ 0.001 % of a cold read op and ≤ ~0.06 % of even a fully
  warm ~100 µs-class serve.
* **Publish** (`.benchmarks/2026-08-01-rewrite-publish-drain.md` §3,
  saturated EXA rewrite, publish 4.796 ms/block): the WHOLE
  `apply/save_encode` phase is 0.026 ms = **0.54 % of the publish**, and
  the string-encode share of that phase is the 3.6 ns/entry measured here
  (≈ 0.23 µs per 64-entry window) — the phase is dominated by map
  apply/clone work, not string rendering. The wall is `meta_commit`
  2.227 ms = conveyor queueing at ρ ≈ 0.92, which no encoding change
  touches. Low-load publish (0.287 ms/block) moves the encode share to
  ~0.1 %-class — still noise.
* **Journal bytes**: the delta already collapsed the byte term 7.6–21×
  (write-commit-economy); §3's table shows packed would *grow* the
  dominant entry 2× or save ≤ 1.6 % of J-bytes/block in its minimal form.
* **RAM face** (arithmetic, stated not measured): `HashMap<u32, String>`
  pays a 24-B `String` header + a 7–13-B heap allocation per entry vs 28 B
  inline for packed — roughly parity in bytes, minus one indirection.
  The warm-lookup row already prices that indirection: 57 ns/lookup, a
  term the serve path pays once per block resolution against a µs-to-ms
  op.

## 5. VERDICT (recommendation with numbers — the ruling's terms)

**The string term is ~45–57 ns/op at the warm-lookup shape (parse
52–117 ns across forms) and 3.6 ns/entry at the publish-encode shape =
≤ 0.1 % of the measured read op (key_resolve ≤ 0.01 ms of 10.50 ms) and
≤ 0.54 % of the measured publish (the whole save_encode phase, of which
the string render is a sliver) — displacement is NOT justified.** On the
wire the packed form as specified is a 2× per-entry REGRESSION against
the dominant bare-decimal string (32 B vs 16 B), and its minimal form
saves ≤ 1.6 % of journal bytes/block. A v3 format change (layout
encoding + `LayoutDelta` wire + indirect-map blob + `kv_record`/
`layout_wire` fuzz targets + a migration/incompat-bit posture + every
`block_map` consumer across routing/fsck/movers/defrag) would be priced
against a term that is nanoseconds against milliseconds and bytes
against hundreds of bytes.

What WOULD reopen the bracket (stated so the next reader has a
falsification): (a) a field decomposition showing `key_resolve` or
`save_encode` promoted to a ≥ 5 %-of-op term (e.g. a future
sub-block-mapping design multiplying resolutions per op by 100×), or
(b) bit-13 incarnation stamping engaging fleet-wide — the stamped string
form grows to ~25 B/entry and 96 ns/parse, at which point the packed
28 B/2.5 ns record is at byte parity with a real parse win and the
bracket should be re-run at the then-current field shapes.

## 6. Gates (all on the branch tip)

* `cargo clippy --all-targets --all-features -- -D warnings` **PASS**;
  `cargo clippy --all-targets -- -D warnings` (shipped config, ENG-8)
  **PASS**.
* `cargo fmt --check` **PASS**.
* Bench smoke `cargo bench --benches -- --test` **PASS** (exit 0, all
  root bench targets — the gate's smoke line; fuse3's own suite untouched
  by this branch).
* Markdown link/anchor check (`tests/check_markdown_links.sh`) **PASS**.
* No test files moved; no production `.rs` touched (bench-only + this
  note). Full `task check` DEFERRED by the campaign spec (the
  orchestrator's merge gate owns it).
