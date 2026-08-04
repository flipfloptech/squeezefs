# 2026-08-04 — The `.stats` torn-JSON bug: three mechanisms, closed by per-lookup virtual-inode generations

**Branch:** `fix/stats-torn-json` (commits `fe35c9ee` red / `dd5e01f5` fix 1,
`79fa941d` fix 2, `3aad8f6b` red / `5578aff0` fix 3).
**Field face:** `cat /mnt/…/.stats | json-parse` failed 39/40 on a busy
default mount — the JSON tears mid-string at a stale byte bound — while
`dd`/plain-read loops of the same inode were clean, and write-through
(interception) mounts were clean after fixes 1–2.

## The three mechanisms

The virtual `.stats`/`.config` files are regenerating payloads on FIXED
inos (`STATS_INODE = 0xffff_ffff_ffff_fffd`, `CONFIG_INODE` beside it).
Three independent mechanisms each produced the same torn-prefix face:

1. **Cross-clone state split** (`dd5e01f5`). The live session serves each
   over-uring queue from its own `SqueezefsFilesystem` clone, and `Clone`
   split the snapshot machinery per clone: `open_virtual_files` deep-copied
   (an OPEN's pinned generation invisible to READs on other queues, which
   regenerated per call), `latest_stats_json`/`latest_config_json` were
   fresh ArcSwap cells while `latest_stats_size` WAS shared (GETATTR's size
   and READ's bytes from different generations — `cat` clamped at the stale
   size), and `next_virtual_fh` was a split counter (two queues could mint
   the SAME fh). Fix: one `Arc` instance per cell across clones (the
   `kernel_notify` one-cell law), pinned by
   `metrics_tests::stats_snapshot_protocol_holds_across_handler_clones`.

2. **Retained kernel pages** (`79fa941d`). The 40-cat churn probe proved
   the kernel serves these inos from retained page cache regardless of the
   FOPEN_DIRECT_IO open reply on the patched over-uring kernel — and when
   two generations have EQUAL size (steady-state counter churn),
   AUTO_INVAL_DATA sees no size change, keeps the previous generation's
   pages, and splices stale-prefix + fresh-tail mid-string. Fix (the
   fixed-ino era's belt): a synchronous `notify_inval_inode_sync(ino, 0, -1)`
   before the OPEN reply. On **write-through** mounts, fixes 1+2 measured
   **0/40 torn** (was 39/40).

3. **Writeback-cache size authority** (`5578aff0`, this session). On
   default mounts the kernel negotiates FUSE_WRITEBACK_CACHE, under which
   the kernel OWNS `i_size` for regular files and DISCARDS the size in
   every attr reply after inode instantiation. Measured live: daemon
   GETATTR replies sized 71352 → 71350 → 71349 while the kernel kept
   serving a frozen 71352 — `cat` (the splice path) clamps at the frozen
   size and tears mid-string; `stat -c %s` never moved. No attr-reply
   protocol (zero TTLs, published-size pins, page purges) can fix a FIXED
   ino under that ownership: the size the kernel trusts is the one it
   instantiated the inode with.

## The probe matrix (mechanism attribution, pre-fix-3)

| Consumer / posture | Result | Why |
|---|---|---|
| `cat` (splice path), default wb-cache mount | **39/40 torn** | kernel-owned frozen `i_size` clamps the copy bound |
| `dd` / plain-read loop, same mount | 0/40 torn | FOPEN_DIRECT_IO reads short-terminate honestly at the daemon's EOF |
| `cat`, writeback-cache OFF (write-through / interception mounts — the field cluster) | 0/40 torn | attr-reply size honored; fixes 1+2 suffice |

## Fix 3: per-lookup virtual-inode generations

Every `.stats`/`.config` LOOKUP generates its payload ONCE and mints a
FRESH kernel inode from the reserved range
`0xffff_ffff_0000_0000 ..= 0xffff_ffff_ffff_fff0` — disjoint from real
inos (v3 minting is monotonic-from-1, no reuse, cap ≥ 100 M ≪ 2^32) and
from the canonical virtual inos; the class rides bit 0 so it survives
registry eviction; the mint counter and the payload registry
(`virtual_gen_payloads`, `Arc<DashMap<gen_ino → immutable payload>>`) are
one cell across clones per the fix-1 law. The fresh kernel inode's
wb-cache size authority initializes from the LOOKUP entry's attr size and
**never needs to change** — that generation's payload is immutable — so
splice/cat is coherent under every kernel cache posture, and the fixed-ino
OPEN pin's last-open-wins residual is structurally gone (each lookup's
reader opens its OWN inode).

Lifecycle: zero entry TTL (every path walk revalidates → fresh mint);
FORGET/BATCH_FORGET retire the registry entry; retired generations answer
ESTALE on GETATTR/OPEN (the next walk mints fresh). Safety cap: 256 live
generations (~18 MiB at the field-measured ~72 KiB payload) against a
kernel that never FORGETs, evicting the oldest mint. The `79fa941d`
OPEN-time purge is removed (a fresh ino has no retained pages by
construction); `notify_inval_inode_sync` itself stays (the generic/451 DIO
sink). Legacy fixed-ino GETATTR/OPEN/READ arms keep the pin machinery for
old fds / handle reconnects, consolidated behind `virtual_ino_attr` — which
the `LOOKUP(nodeid, ".")` export-reconnect arm now also consults (a sweep
finding beyond the ~18 guard sites: it previously fed virtual inos into
`get_attr_internal`). The guard sweep routed every
`== STATS_INODE || == CONFIG_INODE` site through
`is_virtual_ino`/`virtual_gen_class` (open write-intent exemption, read,
write refusal, setattr — which previously screened only `CONFIG_INODE` —
lseek, flush, release, fsync, `queue_reclaim_inode`,
`reclaim_orphaned_batch`, forget/batch_forget). readdir/readdirplus are
untouched: the root virtuals are LOOKUP-ONLY by design (never listed), so
no readdirplus mint arm exists to need one.

## Acceptance (live, this session)

Rig: tcp dev substrate (`SQZ_DEVSUB_TRANSPORT=tcp`, nvmet-tcp localhost —
4 × nullb mds + 4 × zram oss), debug build, default mount (NO
`--interception`, wb-cache negotiated) at `/mnt/sqz_teartest`; churn =
`fio randwrite bs=4k size=64m numjobs=4 iodepth=8 libaio direct=1
time_based runtime=25` in a subdir (avg 23,021 write IOPS across the
probe window); probe = 40 × `cat .stats | python3 json.load` at 0.4 s
spacing. Instrument stated per the standing rule: `cat` (splice) + fio
libaio, tcp substrate.

| Row | Result |
|---|---|
| Pre-change (same shape, fixed inos, wb-cache) | 39/40 torn |
| **Per-lookup generations (this branch)** | **0/40 torn** |
| `stat -c %s` across the 40 iterations | **38 distinct sizes** (71331 → 71721, monotone churn growth — the frozen-71352 face is gone) |

Deterministic pins: `tests/metrics_tests.rs` suite green **×10**
(`stats_lookup_mints_fresh_generation_inos_wb_cache_face` — red at
`3aad8f6b` with both lookups answering the fixed ino — plus the extended
cross-clone pin and the two prior-era pins). Neighbor suites green:
`phantom_backend0_tests`, `forget_sweep_tests`, `val7_access_control_tests`,
`rand_write_rig_off_tests`, `dio_write_page_coherence_tests`. Both clippy
configs (`--all-features` and default, `-D warnings`) and `cargo fmt
--check` clean.

## Notes

- A generation ino means `st_ino` for `.stats`/`.config` changes per
  lookup. Deliberate: these are synthesized control files (the
  `.zfs`/`.lustre` hidden-file pattern, never listed by readdir), not
  hardlink-stable POSIX objects; the canonical inos remain reachable for
  old fds and handle reconnects.
- The registry cap is a safety rail with documented arithmetic (256 ×
  ~72 KiB ≈ 18 MiB), admissible under the derivation law as a
  misbehaving-kernel backstop, not a tuning knob.
