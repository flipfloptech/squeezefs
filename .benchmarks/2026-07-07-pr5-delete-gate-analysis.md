# PR 5 Metadata-Delete gate: measurement & attribution

Gate as written (design §4.5 / Rollout §3): mount-bench **Metadata Delete ≥ 2×**
the committed baseline (1516.73 → ≥ 3033 ops/s). Observed after PR 4 + PR 5:
**2357–2828 ops/s** across nine controlled runs (+55–86%, not 2×).

## Where the ~400 µs/op actually goes

The bench's Delete row is a **single-op latency** measurement: all threads
unlink from one directory, serialized by the kernel on the parent's
`i_rwsem`. Controlled experiments (unprivileged mounts, 4 MiB settled files,
serialized `rm`, this machine):

| Experiment | µs/op | Meaning |
|---|---|---|
| unlink, fds held open (no kernel eviction, no FORGET) | **142** | transport + meta floor |
| rmdir twin (no page cache attached) | 131 | confirms the floor |
| unlink, kernel eviction inline | 354–415 | the Delete row |
| unlink with ALL daemon reclaim deferred past the storm (window=1s) | 383 | **daemon consumer exonerated** |
| reclaim knob sweeps (`SQUEEZEFS_RECLAIM_{BATCH,CONCURRENCY,BATCH_WINDOW_MS}`, over-uring `Q_DEPTH` 4/16/64) | 355–413 | all flat |
| in-process handler (`fuse_client::unlink` + reclaim chase, no kernel) | 53 | daemon-side total |

Attribution: ~210–270 µs of every unlink is **kernel-side inline eviction**
(`iput_final` → truncate of 4 MiB page cache per file + FORGET dispatch)
inside the unlink syscall — dominated by the *data* size of the bench's
"small" files (4 MiB), not by metadata work. The design's §4.5 premise that
the per-delete data-path share is cheap relative to the two metadata
transactions is **disproven by measurement**: the metadata share is
~50 µs of ~400 µs.

## What the meta-side (this design's scope) delivered

- `create_unlink_file` micro: 207.35 → ~102–112 µs (**−52%**, criterion vs
  `pre_wal_removal`); `set_get_xattr` −42%; `lookup` unchanged.
- One transaction per ≤64-corpse reclaim batch (test-pinned via
  `meta_commit_sectors`), xattr kill folded into the same commit (was a
  per-corpse `removexattr` transaction), FORGET-storm gather window.
- End-to-end Delete row: 1516.73 → ~2400–2800 (**+55–86%**), Rmdir +45%,
  Mkdir +41%.

## Bugs found & fixed during the investigation

1. Reclaim's per-corpse `removexattr("layout")` transaction — doubled
   meta-commit traffic under delete storms; a reused ino could resurrect
   the corpse's layout xattr if reclaim lost the race. Folded into the
   batched destroy transaction.
2. `batch_forget` was fuse3's default NO-OP — batch-evicted orphans
   (memory pressure, `drop_caches`, pre-umount sweeps) leaked inode slots
   until the next mount's reconciliation. Implemented as N FORGETs.
3. (Filed) `.stats` virtual inode serves a stale size attr — reads clamp
   to the previous generation's length, truncating the JSON.

## Disposition

The residual gap to 2× lives in kernel page-cache eviction semantics on the
FUSE unlink path — a data-path/transport concern outside this design's
stated scope (its Non-Goals exclude data-path work). Closing it would mean
investigating deferred-eviction strategies (e.g. releasing pages before
reply, invalidation batching), which warrants its own investigation rather
than a bolt-on here.

---

## Addendum: eviction-residual attribution (quiet-machine A/B, load < 5)

Scratch TTL knob + `--no-writeback` A/B on otherwise-identical unprivileged
mounts (120 × 4 MiB settled files, serialized unlink), with per-op FUSE
round-trip counting via debug-log deltas:

| Case | µs/op | FUSE round-trips per unlink |
|---|---|---|
| control (1 s reply TTLs, writeback on) | 346 | LOOKUP + GETATTR + UNLINK |
| reply TTLs 30 s | 315 | GETATTR + UNLINK |
| TTLs 30 s + `--no-writeback` | 259 | GETATTR + UNLINK |
| fd held open (eviction deferred) | 142 | UNLINK |

Decomposition of the ~205 µs above the transport floor:
- **~30 µs — entry-TTL revalidation LOOKUP**: settled files outlive the 1 s
  entry TTL, so the kernel re-LOOKUPs before UNLINK. (Also explains the
  earlier "settled slower than dirty" anomaly: freshly-written files still
  hold a valid dentry.) Raising reply TTLs is a coherency trade on a
  distributed filesystem — not taken as a default; candidate operator knob.
- **~30 µs — parent GETATTR**: kernel invalidates the parent's attrs after
  every unlink (`AUTO_INVAL_DATA` posture). Inherent.
- **~55 µs — writeback-cache eviction tax** (346→259 delta net of TTL):
  writeback mode makes `iput_final` walk the writeback machinery per inode.
  Toggling writeback off is not a fix (kept deliberately for the write
  path); documented as a delete-heavy-workload operator note.
- **~120 µs — inline `truncate_inode_pages` of 4 MiB page cache** in the
  unlink syscall. The daemon cannot pre-drop those pages: the
  FUSE-over-io_uring transport currently REJECTS outbound notify
  (`fuse_notify_inval_*` unsupported in the vendored fuse3 over-uring write
  path, `third_party/fuse3/src/raw/connection/tokio.rs:552-560`), so
  notify-based pre-eviction (or kernel ≥ 6.16 `FUSE_NOTIFY_INC_EPOCH`-style
  approaches) requires a transport feature first.

**Follow-up filed**: "notifications over FUSE-over-io_uring" transport
feature (enables pre-eviction invalidation, plus the long-standing inval
use-cases the tokio path already supports) — its own design; the remaining
per-round-trip tax (eventfd wake + re-arm submit per reply,
`fuse_over_uring.rs:510-540`, `:961-967`) is a second, independent
transport optimization candidate.

## Addendum 2: substrate invariance (operator cross-check)

Same machine, same day, full `squeezefs bench` on two substrates:

| Row | NVMe-oF (SPDK loopback) | Direct file-backed | Δ |
|---|---|---|---|
| Metadata Stat | 42,513 | 87,325 | +105% |
| Metadata Mkdir | 7,717 | 16,764 | +117% |
| Metadata Rmdir | 21,549 | 30,321 | +41% |
| **Metadata Delete** | **2,776** | **2,815** | **+1.4%** |

Removing the fabric RTT roughly doubles every metadata row EXCEPT Delete,
which is flat across substrates — independent confirmation that the Delete
row is bounded by kernel-side inline page-cache eviction (per the
attribution above), not by the metadata commit path or device latency.
