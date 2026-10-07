# Rung 5 evidence — the `.stats` inode, the ONE JSON aggregator, the rung-5 CLI verbs, `bench`

Spec: `docs/porting/stats_cli.md`; charter §8, §10, §11, §1.6. Decision 0002 rows 197–207.
Venue: `sqz-zig` (7.2.6-sqz, Zig 0.17.0, aarch64); dev box macOS for the portable suite.

## What landed

- **`src/stats/stats.zig`** — ONE comptime family table (`build, mount, stats_key_census,
  transport, data, meta, fuse, exec, sync, io, mem_budget, ops, op_trace, cpu, stats, daemon`)
  streamed through one `Io.Writer`; `Renderer` renders into ONE allocation sized from the last
  render (+12.5 % + a page; `stats_build_regrows` counts misses). The writers are GENERATED:
  `counters.writeNamespaceJson` / `writeFieldsJson` walk a module's `pub var` instruments (or a
  `stats: Stats` instance's fields) by type — a counter declared is a counter exported; a tripwire
  is keyed by its SITE. 20 namespaces that had counters and no writer (`meta/*` ×18 incl. the
  checkpointer's per-instance gauges, `fuse/handler/*` ×5, `sync/*` ×2, `exec/placement`,
  `fuse/codec`, `data/transform`, `data/write_seams`, the guard) now export through it.
  `daemon` carries `invariant_tripwires` + `invariant_tripwire_sites{site: n}` +
  `tripwire_sites_overflow` and the closure identities as computed booleans.
- **The inverse rail** (`tests/rails.zig`): a tokenizer scan for `: counters.<Type>`
  declarations outside test blocks; every declaring module must be a `stats.sources` row or a
  `stats.covered` row (whose `by` is a source); every path must exist. 44 sources, 39 covered.
- **`src/fuse/handler/virtual_inodes.zig`** + `kv_fs.zig`: `.stats`/`.config`/`.trace` as
  per-lookup GENERATION inos (range `2⁶⁴−2³² … fff0`, class in the low 2 bits, FIFO ring of 256,
  kernel + ring + fh references, ESTALE when retired), lookup-only (never in readdir; the two
  reserved cookies resume as the end), mode 0400 + mount uid/gid, `blocks = ceil(size/512)`,
  both TTLs 0, `FOPEN_DIRECT_IO`, reads slice the pinned immutable bytes. `.config` =
  build + mount + `set_config.Config.writeJson` + URIs; `.trace` drains the new sampled ring
  (`op_trace.Pool`, `SQUEEZEFS_OP_TRACE`, derived geometry). The xattr screen is an ALLOWLIST
  (`fuse_reserved_xattr_refusals`); `statfs.ffree` reports the LIVE population (`live_inodes`).
- **CLI**: typed `Verb` for `status TARGET`, `clients`, `df`, `clone`, `tune`, `config
  show|set-cache-paths`, `staging list|promote`, `claim clear`, `bench`; ONE positional
  `sqmeta://…|<mountpoint>` resolved through `.config` (`cli/target.zig`); the offline opener
  (`cli/offline.zig`: writer door → plane → fs → body → dismount pass); `clients`/`claim clear`
  wired to the guard agent's `gate.clients`/`gate.claimClear`; `json_scan.zig` is the one key reader.
- **`bench`** (`cli/bench.zig`): the suite, auto shape, full-coverage permutation, xorshift fill,
  per-worker `LatencyHistogram` (quantiles = bucket upper bounds), rows with instrument /
  substrate / venue / profile, `device bytes ÷ user bytes` (ppm), `wareq-sz`, the reclaim and
  admission ledgers, the must-stay-0 census, `scoping-only (<60 s)`, `--abba` (O_DIRECT vs
  buffered, A-B-B-A). `zig build bench-smoke` (in the gate) formats, mounts, runs `bench --smoke
  --json` (2 × 1 × 4 MiB, 6 rows, every row at full coverage, tripwires 0, dataset deleted,
  `.stats` 0400/uid), unmounts.

## Measured on the venue (`bench --smoke`, file-backed set with a cache path, Debug build)

```
instrument=squeezefs bench 0.1.0 32c1f481d365-dirty profile=gate  warning: profile gate is not a dist measurement
substrate=file venue=7.2.6-sqz sqz-zig   posture: hybrid (default)
| write 1048576 seq |  81 MiB/s |   81 IOPS | 8/8 | p50 32768 µs … | scoping-only (<60 s)
|   device bytes ÷ user bytes 0.000 (staged set: the bytes sit in the ring until promotion); admission 8388608 B
| read 1048576 seq  | 1762 MiB/s | 1762 IOPS | 8/8 |
| read 4096 rand    |  10 MiB/s | 2793 IOPS | 8/8 |
| write 4096 rand   |          |   67 IOPS | 8/8 |
```
`.stats` payload: 62,182 B (one render = one allocation after warm-up; pinned by a counting
allocator). SIGUSR1 → 16 family lines through the same writers (`stats daemon:` last).
Offline `clone` of an 8 MiB striped file: 2 blocks shared, 0 bytes copied; `status` refs census
4 references over 2 blocks (refcount 2); the clone read back byte-equal after a remount.

## Gates

- macOS `zig build gate`: fmt + 1229 unit tests (15 Linux skips) + rails (knob census, inverse
  knob census, stats inverse census) green; smoke/bench-smoke skip loudly.
- venue `scripts/venue zig build gate`: green (exit 0); no mounts or daemons left.

## Deviations stated

- The ~20 lacking namespaces are exported by the GENERATED writer from the registry, not by a
  per-module `writeStatsJson` (the spec's Q1/Q4 design; it also kept the edit footprint off the
  files two concurrent agents were editing).
- The key census (`SQUEEZEFS_STATS_KEY_CENSUS`) is a typed seam (`stats.KeyCensus`): the keys
  render present-and-empty until the data plane binds its iterators (none exist yet).
- `staging drain` is not a verb: the dismount pass IS promote + seal + drain (`staging promote`
  runs it offline; `fsync`/`umount` on a live mount) — stated on the verb's module doc.
- `df`'s `inodes.used` offline is the minted watermark (the live population needs the mount's
  destroy tally; `statfs` reports it live).
