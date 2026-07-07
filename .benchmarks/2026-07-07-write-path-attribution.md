# Write-path performance: instrument audit + first attribution

Trigger: owner observed higher write speeds from elbencho than from
`squeezefs bench` and asked whether the benchmark, the filesystem, or both
are at fault. Answer: **both suspicions partially wrong — the built-in
bench is the FASTEST honest consumer measured; the filesystem write path
is genuinely ~4–5× off the substrate, and it is memcpy-bound.**

## Instrument audit (same unprivileged mount, btrfs NoCOW backing, quiet box)

| Consumer (10 threads × 1 GiB, 1 MiB writes, fsync at end) | Aggregate |
|---|---|
| `squeezefs bench --only large-seq-write` | **430–512 MiB/s** |
| python tight loop (std write) | 424 MiB/s |
| elbencho `-w -t 10 -s 1g -b 1m` (±`--sync` identical) | ~230 MiB/s |
| **raw substrate control** (dd 1 MiB, direct AND buffered+fsync, same dir) | **2.1–2.2 GB/s** |

- elbencho's per-line MiB/s columns are aggregate first/last-done values —
  earlier "elbencho is faster" readings compared its aggregate against our
  per-phase number on different substrates/params. On identical shapes it
  is the *slowest* of the three here.
- Bench-instrument nits found while auditing (worth fixing, not the
  bottleneck): the write loops use `tokio::fs` (a spawn_blocking hop per
  1 MiB chunk) and `sync_all` inside the timed window; progress-bar inc per
  chunk.

## Filesystem attribution (perf, daemon-side, 71843 samples during storm)

~45–50% of all daemon cycles are glibc memcpy (anonymous `0x1b1dxx`
cluster) spread across `tokio-rt-worker`s and the `fuse-over-uring`
transport thread, with `SqueezefsFilesystem::insert_active_block_buffer`
the top named symbol. At 430 MiB/s delivered vs 2.2 GB/s substrate, the
write path spends its budget copying each byte multiple times:

1. transport payload buffer → FUSE write handler bytes
2. handler → active-block buffer (`insert_active_block_buffer`)
3. active block → staging segment put (mmap)
4. staging → upload/`process_write` (crypto/compress path copies even in
   pass-through mode — to verify)
5. upload → uring write payload

## Next step (owner-approved direction: "really dig into write performance")

Copy-count audit of the write path with file:line citations (which of the
five copies exist, which are avoidable), then a zero-copy/bytes-slicing
redesign plan per AGENTS.md's zero-copy non-negotiable — candidates:
`bytes::Bytes` end-to-end from the transport payload, staging-first
placement without intermediate Vec, in-place crypto/compress. Perf gate:
large-seq write ≥ 3× current (≥ ~1.3 GB/s) on this substrate profile.
