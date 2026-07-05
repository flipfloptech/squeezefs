# PERFORMANCE_OPTIMIZATIONS.md — Deep-Dive Code Audit

**Date:** 2026-07-04 · **Branch:** `dev` (`da9ba0b`)
**Scope:** performance bottlenecks, lock contention, resource exhaustion, and extreme-concurrency issues across the full tree: `src/meta_backend/`, `src/fuse_client.rs`, `src/routing.rs`, `src/cache/`, `src/tiering/`, `src/nvme_dev.rs`, `src/uring_fs.rs`, `src/block_allocator.rs`, `src/dlm.rs`, `src/jobs.rs`, `src/main.rs`, and `third_party/fuse3/`.

**Severity legend**

| Severity | Meaning |
|---|---|
| **P0** | Deadlock, data loss/corruption, memory-unsafety, or hard availability failure under load. Fix before any load/scale testing. |
| **P1** | Major throughput/latency bottleneck, unbounded resource growth, or scale-breaking design. |
| **P2** | Measurable papercut, hygiene issue, or latent hazard. |

Line numbers reference the tree as of `da9ba0b`.

---

## Executive summary

The FUSE-over-io_uring transport and data-path *design* are strong (atomic metrics, zero-copy read replies, bounded staging merge queue, single-flight block fetch, bg admission control). But the system currently cannot survive its own target load for four structural reasons:

1. **The metadata backend is a synchronous, globally-serialized bottleneck.** Every FUSE op funnels into `MetaLvStorage`: blocking `pread`/`pwrite` + full `fsync` per sector write, under **two stacked per-volume global mutexes**, executed directly on tokio worker threads. On top of that, dentry lookup and inode allocation are full-table linear scans (16,384 preads per negative lookup; 20,000 per create near capacity). This violates the repo's own io_uring non-negotiable and dominates every latency number the FS can produce.
2. **Guaranteed deadlock at write saturation.** The staged-write path holds a `BLOCK_FLUSH_LOCKS` guard, and when the writeback queue fills it synchronously re-enters the same lock (and inverts the documented lock order). It fires exactly when the system is busiest.
3. **On-disk layout overlaps corrupt data at moderate scale.** The inode table collides with the dentry table at 1 MiB (inode #4080+), and the journal region overlaps the xattr region at 16 MiB. Separately, the JSON-in-a-1KiB-xattr layout record overflows for inline files > ~250 B and striped files > ~30–60 blocks.
4. **Several "durable" mechanisms are illusions.** The journal, staging recovery, and defrag are no-op stubs; the block allocator has no persistence (remount reallocates block 0 over live data); O_DIRECT silently falls back to buffered I/O with zero fsync; the staging mmap path never msyncs; the DLM is an in-process mock — two nodes sharing a MetaLV have **zero** mutual exclusion.

Findings: **13 P0, 27 P1, 30+ P2**, detailed below with a prioritized remediation plan at the end.

---

## 1. Metadata backend (`src/meta_backend/`, `src/dlm.rs`)

Every FUSE op executes this code (`RoutedMetaBackend` wired at `src/main.rs:1981`, called from all `fuse_client.rs` handlers).

### 1.1 Blocking I/O on the async runtime

- **[P0] Entire metadata store is synchronous `pread`/`pwrite` on tokio workers — no io_uring, no `spawn_blocking`.**
  `storage.rs:4` (`FileExt`), `storage.rs:113-115`, `storage.rs:128-129`. All `Metadata` trait methods (`mod.rs:128-467, 551-1160`) are `async` but do blocking syscalls inline. With N runtime workers, N concurrent metadata ops stall the entire runtime — including the FUSE-over-uring queue servicing. Violates the "always io_uring" non-negotiable.
  *Fix:* port `MetaLvStorage` to `crate::uring_fs`-style async uring I/O (proper async `read_at`/`write_at`/`fdatasync`); `spawn_blocking` as an interim stopgap only.

- **[P0] Full-device `sync_all()` (fsync) on every single sector write.**
  `storage.rs:129` (and `:100` for superblock). One `create()` = ≥3 fsyncs (child inode `mod.rs:178`, dentry `mod.rs:181`, parent times `mod.rs:191`), each blocking a tokio worker **while holding the global op lock and file mutex**.
  *Fix:* batch durability at transaction boundaries behind a real journal; use `fdatasync`/O_DIRECT via uring.

- **[P1] `get_allocated_inode_count` = up to ~20,000 blocking preads, per volume, per directory create.**
  `mod.rs:83-91` → `get_volume_health` (`mod.rs:507`) → called for **every volume** in `RoutedMetaBackend::create` (`mod.rs:588`). 4 volumes ⇒ ~80,000 serialized syscalls per `mkdir`.
  *Fix:* in-memory atomic allocated-count per volume, updated on alloc/free.

### 1.2 Lock contention

- **[P0] Two stacked per-volume global mutexes serialize *all* metadata ops, held across blocking I/O and fsync.**
  `storage.rs:33` (`Arc<Mutex<File>>`) + `storage.rs:35` (`op_lock: ReentrantMutex<()>`). `op_lock` covers inode table + dentry table + xattrs + allocator (`inode.rs:57,98`, `dentry.rs:52,114,132`, `xattr.rs:84,146,230,280`); `create()` holds it across the full 20k-slot inode scan (`mod.rs:161-179`). The file mutex is redundant — `pread`/`pwrite` are positioned and thread-safe — and the fsync at `storage.rs:129` runs **inside** it, blocking all readers for the flush duration. Both are non-async parking_lot locks blocked on from async context.
  *Fix:* delete the file mutex; replace `op_lock` with per-region/per-sector locking; move fsync outside locks.

- **[P1] DLM lock map leaks entries forever.** `meta_backend/dlm.rs:7,18-23` — `DashMap<String, Arc<RwLock<()>>>` grows one entry per unique `"I{ino}"` / `"D{parent}:{name}"` ever touched; never removed. Unbounded memory + growing shard contention on a long-lived mount.
  *Fix:* remove on guard drop when `strong_count == 1`, or a fixed striped-lock array keyed by hash.

- **[P2] Two `String` allocations + a stored never-read key clone per lock acquisition** (`mod.rs:130,149-153,366`; `dlm.rs:30,40`). Key on `(kind, ino, hash)` tuples instead.
- **[P2] `ReentrantMutex` is per-*thread*, not per-task** (`storage.rs:35`) — any future `.await` under `lock_op()` is a latent deadlock/false-reentrancy hazard. Guard with a comment/assertion.

### 1.3 Algorithmic issues

- **[P0] `find_dentry` is a full linear scan: 16,384 preads / 64 MiB of I/O per negative lookup.**
  `dentry.rs:90-104`. No hashing despite the `next_ptr` hash-chain field (`dentry.rs:17`, written 0, never followed). Each slot read re-reads its full 4 KiB sector (8 slots/sector ⇒ 8× re-read), under both global locks. `insert_dentry`/`remove_dentry`/`list_dentries` (`dentry.rs:107-159`) scan identically. `lookup()` is the highest-frequency FUSE op in existence.
  *Fix:* implement the hash-bucket + `next_ptr` chain the format already reserves; scan a sector once for its 8 slots; add an in-memory dentry cache.

- **[P0] `MAX_DENTRY_SLOTS = 16384` caps total dentries per *volume*.** `dentry.rs:8`, error at `:125-127`. The whole volume tops out at 16,384 files/dirs, and each failing create pays a full-table scan first.

- **[P0] Inode allocation is an O(n) pread scan under the global lock; the free-inode bitmap doesn't exist.**
  `mod.rs:163-170, 630-636` — `for i in 2..20000 { read_inode(...) }`. The superblock advertises `inode_count: 1_000_000` and a `free_inode_bitmap_root` (`mod.rs:110-111`), but no bitmap code exists; true capacity is the hardcoded 20,000.
  *Fix:* implement the bitmap the superblock already reserves.

- **[P1] `readdir` ignores `offset`/`max`** (`mod.rs:351,1010`, params named `_offset`/`_max`) and rebuilds the full listing per FUSE READDIR chunk ⇒ O(table²) per directory stream.

- **[P1] Xattr limits: 3 entries × 64 B key × 1 KiB value per inode** (`xattr.rs:110-113,203-207`) — with `"layout"` consuming one slot, users get two. The `[[u8;32];32]` chunked layout forces flatten/copy allocations per get (`xattr.rs:66-81`).

- **[P2] Per-op copies:** whole-sector reads to extract 256/512-B slots (`inode.rs:62-63`, `dentry.rs:56-57`); `get_name()` allocates a `String` per slot compared — 16,384 allocations per `find_dentry` (`dentry.rs:43-47,99`). Compare name bytes without allocating.

### 1.4 Concurrency correctness (perf-adjacent)

- **[P0] On-disk overlap: inode table collides with dentry table.**
  `INODE_TABLE_START = 4096` + slot 256 B (`inode.rs:5-6`) means inode #4080 lands exactly at `DENTRY_TABLE_START = 1 MiB` (`dentry.rs:5`). Allocator scans up to 20,000 ⇒ beyond ~4,078 inodes, inode writes overwrite dentry slots and vice versa. **Metadata corruption at moderate scale.**

- **[P0] Journal region overlaps xattr region at 16 MiB.**
  Journal at 16 MiB + 4 MiB (`mod.rs:75,113-114`) vs `XATTR_BLOCK_START = 16 MiB` + `ino×4096` (`xattr.rs:5,46-48`) — xattr blocks for inos 0–1023 live inside the journal. Latent only because the journal is a no-op.

- **[P1] The journal is a complete no-op** (`journal.rs:15-24`): multi-sector ops (create = 3 writes; rename = remove+insert, `mod.rs:989-1000`) are not atomic — a crash mid-rename loses the dentry entirely. Meanwhile the per-write `sync_all` pays full durability cost for zero atomicity.

- **[P1] Sector RMW is only safe within one process.** 16 inodes / 8 dentries share each 4 KiB sector; `write_inode_raw`/`write_dentry_raw` do read-patch-write guarded only by the in-process `op_lock` (`inode.rs:80-94`, `dentry.rs:68-81`). The DLM is an in-process DashMap. **Two nodes mounting the same MetaLV over NVMe-oF have zero mutual exclusion** — concurrent sector RMW silently loses updates. A real distributed lease/fencing layer is required before any shared-LV deployment.

- **[P1] `rename` locks the two parent inodes in argument order, not global order** (`mod.rs:317-331, 879-909`) — concurrent `rename(A→B)` / `rename(B→A)` is a textbook ABBA deadlock on tokio RwLocks with no timeout. (`unlink`/`link` are consistent.) *Fix:* sort lock keys before acquiring.

- **[P1] Superblock writes bypass `op_lock` and compute no checksum** (`storage.rs:93-103`; `checksum: 0` at `mod.rs:115`).

### 1.5 RoutedMetaBackend

- **[P1] Directory-create health scoring is catastrophically expensive** — see §1.1 (`mod.rs:582-606`): all volumes × up-to-20k preads per mkdir. *Fix:* cached atomic counters.
- **[P2] `route_ino` redirection loop has no cycle detection** (`mod.rs:530-533`) — a redirection cycle (0→1,1→0) infinite-loops every op while holding a DashMap ref. Bound to `volumes.len()`.
- **[P2] `ino % num_volumes` sharding** (`mod.rs:520-548`) renumbers every inode if volume count changes — blocks online expansion; document.
- **[P2] Cross-volume `lookup` holds the parent dentry guard across the child-volume `getattr`** (`mod.rs:553-569`) — doubles DLM traffic per lookup.

### 1.6 Mock DLM (`src/dlm.rs`)

- **[P1] `LOCK_MAP`/`FENCING_MAP` are process-global `Mutex<HashMap>`s** (`dlm.rs:8-9`) touched on every write op (steady state: one global mutex hit per write via `LockLease::is_held`, `dlm.rs:182`; callers `fuse_client.rs:956,3262-3315`, `routing.rs:2272-2276`). Single cache-line ping-pong point across all cores.
- **[P1] Contended acquisition is sleep-polling: 50 ms × 3, then hard `LockFailed`** (`dlm.rs:153-159`). No waiter queue or fairness; violates the repo's "no sleep for synchronization" rule. *Fix:* `Notify`/watch per key.
- **[P1] TTL is ignored (`_ttl`, `dlm.rs:115`)** — a leaked lease (task aborted before Drop) blocks its key forever; all leases share one `client_id` (`dlm.rs:49`) so release-by-string-compare can free the wrong lease.
- **[P2] `FENCING_MAP` grows unbounded** — one entry per path ever locked, kept for process lifetime (`dlm.rs:139-143`).

---

## 2. FUSE hot path (`src/fuse_client.rs`, `third_party/fuse3/`)

### 2.1 Deadlocks and lock contention

- **[P0] Self-deadlock + lock-order inversion when the writeback queue fills.**
  `write_file_staged` holds a `BLOCK_FLUSH_LOCKS` guard (level 3, `fuse_client.rs:1123-1126`) and calls `enqueue_writeback` (`:1260`). On a full queue, `enqueue_writeback` synchronously calls `flush_due_active_blocks_for_inode` (`:1342-1356`) → `flush_single_active_block` (`:4706`), which (a) takes the per-inode lock (level 1) at `:4728` while level 3 is held — the documented "must not" inversion — and (b) re-acquires the **same** `BLOCK_FLUSH_LOCKS` tokio `Mutex` for the same `(ino, block)` at `:4731-4733` ⇒ **guaranteed async self-deadlock, triggered exactly at peak write load**. On the `EntireOp` path (`:2302-2311`) the caller also still holds the inode *write* guard, so the `read().await` deadlocks too (tokio RwLock is not reentrant).
  *Fix:* drop the block guard before enqueueing, or plumb a `locked: bool` through the synchronous-fallback path.

- **[P1] `active_inode_locks` is a fixed 4096-stripe array — collisions serialize unrelated inodes.** `fuse_client.rs:50-106,472,563`. Memory is bounded (good), but hot colliding files exclusive-block each other. `StripeLocks::remove` is a no-op (`:103-105`), making the release-time "clean up inode lock" logic (`:3547-3550`) dead code that clones an Arc per release.

- **[P1] Every write holds the exclusive inode guard across metadata backend RTTs.** `fuse_client.rs:2189-2260`: lease acquire → `fetch_metadata` (`:2201-2205`) → (growth) `save_metadata_to_backend` (`:2254-2257`) — all against the blocking, globally-mutexed meta backend (§1). "MetaPrepOnly" only shortens the data portion.

- **[P1] Reads hold the striped inode read guard across the whole backend read** (`:2108-2109, 2148-2152`, up to the 30 s timeout) — blocking writers on that stripe.

- **[P1] uring pool `pending` map is one global `std::sync::Mutex<HashMap>` hit 2–3× per request.** `fuse_over_uring.rs:174, 516-519, 547-550, 945-948` — shared by all queue workers, all reply paths, and the zero-copy `get_payload_buffer` call (`fuse_client.rs:2140-2145`). *Fix:* shard by qid — `(qid, ent_idx)` is already qid-local.

- **[P1] The per-CPU kernel queue fan-out is re-serialized through one global `Mutex<VecDeque>` + `Condvar`.** `fuse_over_uring.rs:117-161` — all N uring queue workers push and all session workers pop through a single mutex. *Fix:* per-qid inbound queues with matching consumers, or a lock-free MPMC.

- **[P2] `active_posix_locks` iterates the entire map (all inodes) per getlk/setlk/release** (`fuse_client.rs:3701-3732, 3772-3823, 3534-3539`). Index by inode.
- **[P2] `copy_file_range` orders local locks numerically but DLM leases lexicographically** (`:3241-3253` vs `:3280-3284`; `"inode_9" > "inode_10"`) — cross-op ABBA risk. Use numeric order everywhere.
- **[P2] `lease_locks` stripe mutex held across DLM retry loops (up to 5 s × 5)** (`:938-961`) — blocks lease refresh for colliding inodes.

### 2.2 Transport overhead (fuse3 vendored)

- **[P0] One `spawn_blocking` per FUSE request to pop the inbound queue.** `third_party/fuse3/src/raw/connection/tokio.rs:414-440` — every `read_fuse_request` dispatches a fresh blocking-pool task that condvar-waits (200 ms loop). At high IOPS this saturates the blocking pool and adds two context switches per request.
  *Fix:* dedicated consumer thread per session worker, or bridge the queue to an async mpsc pushed from the uring worker.

- **[P1] Every inbound request is copied twice before the FS sees it.** `fuse_over_uring.rs:894-899` (header+payload `to_vec`) then `tokio.rs:456-497` (copy into session bufs). For a 1 MiB write that's 2 MiB of avoidable memcpy. Entries stay USERSPACE until COMMIT — hand the session a view/lease of the ring entry.

- **[P1] Per-mount pinned buffer memory = possible_CPUs × depth × ≥1 MiB, and docs disagree with code.** `fuse_over_uring.rs:246-284, 679-699`: `nqueues` defaults to all possible CPUs (clamped 512, not the documented `min(nproc,8)`/max 32), `payload_sz ≥ 1 MiB` ⇒ 512 MiB pinned on a 128-CPU box, plus 128 SQE128 rings + 128 threads. Fix docs; consider a small pool of full-payload entries.

- **[P1] Hidden thread-pool multiplication.** Per mount: possible-CPU uring workers (`fuse_over_uring.rs:337-350`) + (cores−1) tokio workers (`fuse_client.rs:4110-4129`) + (cores−1) TPC current-thread runtimes with unbounded channels (`session.rs:4522-4589`) + a per-request-churned blocking pool (max 8192, §5). Every request crosses ≥4 threads (uring worker → blocking popper → TPC session worker → handler task → reply task → uring worker), each hop a channel+wake. **This is the dominant structural latency cost of the hot path.** *Fix:* qid-affine dispatch — handle the request on the thread that read it.

- **[P1] Unbounded reply channels carry full read payloads.** `session.rs:33,307,553` (`futures mpsc::unbounded`) — a burst of large reads queues unbounded bytes. Bound to `nqueues × depth`.

- **[P2] eventfd write syscall per reply + submit syscall per commit** (`fuse_over_uring.rs:538-540, 746-758, 925-934`) — batch: push all pending SQEs then submit once; wake only on empty→non-empty.
- **[P2] SQ-full on commit push propagates as an error that kills the queue → pool shutdown → mount death** (`fuse_over_uring.rs:1078-1080` via `:749-757`). On full: `submit()` and retry.
- **[P2] Per-reply header `Vec` allocations** (`tokio.rs:583`, `CommitMsg.header`).
- **[P2] `Bytes::from_static` over reused ring/mmap buffers is unsound aliasing.** `routing.rs:1837,1882, 1965-1967, 2031-2036, 2237-2243`; `tokio.rs:572`. The fabricated `'static` slice flows through the unbounded reply channel and could be held past COMMIT, after which the kernel reuses the buffer ⇒ torn data. Needs a lifetime-carrying guard (like the existing `backing` mechanism), not `from_static`.

### 2.3 Task spawning & background work

- **[P1] Fire-and-forget spawn per request, no `JoinSet`, no admission control** (`session.rs:1440,1512,2367,2452` → `TPC_SCHEDULER` `:4592-4602`); panics silently dropped (`:4563-4565`). Effective in-flight bound is `nqueues × depth` (512 on a big box), each holding buffers/locks.
- **[P1] Writeback worker spawns the task *before* acquiring the upload semaphore** (`fuse_client.rs:4532-4553`, permit at `:4546`) — up to `WRITEBACK_QUEUE_CAP = 4096` parked tasks. Acquire `acquire_owned` before spawning. Retry backoff sleeps **while holding an upload permit** (`:4617-4619`) — a retry storm parks upload slots doing nothing.
- **[P2] Sleep-poll loops on the op path:** `ensure_delegation_held` 50 ms × 40 (`:977-1013`); blocking `setlk` 100 ms × 20 (`:3799-3841`). Use `Notify` per inode.
- **[P2] `unlink`/`rename` spawn untracked reclaim tasks cloning the whole FS handle set** (`:2709-2713, 2775-2779`) — an unlink storm = unbounded background backend traffic.

### 2.4 Copies on the read/write path

- **[P1] Write path: 4–5 full-payload copies before crypto.** kernel→ring (DMA) → `payload.to_vec()` (`fuse_over_uring.rs:898`) → session `data_buf` (`tokio.rs:466-475`) → `data.to_vec()` (`session.rs:2447`) → `Bytes::copy_from_slice`/staged block copy (`fuse_client.rs:2235,2272` / `:1232-1236`) → `process_write` output (`:4744`). Eliminate the transport copies (§2.2) and pass `Bytes` end-to-end.
- **[Good] Read path zero-copy works when it hits:** ring-entry payload pointer (`fuse_client.rs:2140-2145`) → `read_file_range_zero_copy` decrypts directly into it (`routing.rs:1833-1890`); pointer-match skips the copy (`fuse_over_uring.rs:1030-1033`, `tokio.rs:569-573`). But the pointer fetch takes two global mutexes (§2.1), and a non-match costs an extra payload copy + header `to_vec` (`tokio.rs:574-583`).
- **[P2] `.stats`/`.config` reads clone the full cached `Vec<u8>` per 4 KiB read call** (`fuse_client.rs:2065-2105`). Store `Bytes`.
- **[P2] Staged cold-block RMW does avoidable `to_vec()`s on LRU hits** (`:1170-1218`).

### 2.5 Caching & metadata RTT storms

- **[P1] Attr cache effective TTL is 1 s while moka TTL is 300 s.** `fuse_client.rs:543-546,1568-1571,2924-2926,3133-3136` — every consumer re-checks `cached_at < 1s`, so entries are dead weight for 299 s of their lifetime, and every inode costs a backend getattr RTT per second per node. At 15k nodes: a metadata stampede into the globally-locked backend. *Fix:* one TTL; lease-based invalidation instead of time.
- **[P1] `readdirplus` does N+1 sequential getattrs — then re-fetches each entry again.** `:2922-2939, 3144-3176`: batch-fetch loop followed by per-entry `get_attr_internal`. `ls -l` on 100k entries = 100k+ serialized blocking backend reads inside one FUSE op. Batch + `buffer_unordered` + reuse the fetched attrs.
- **[P1] `release` is heavyweight even for read-only closes.** `:3493-3560`: acquires a **write lease** (with fencing token) for never-written files, scans **every staged key in the process** (`:1281` → `list_keys()`), iterates all POSIX locks, then a backend getattr for reclaim. open/close storms (`find`, `grep`) multiply this. *Fix:* dirty-inode tracking to skip lease+flush when clean; per-inode staging index (see §3.3).
- **[P2] Directory cache: one Arc slab of up to 100k entries, rescanned per readdir chunk (O(N²) per stream), 300 s staleness, local-only invalidation** (`:2815-2828, 2909-2962, 536-540, 2704, 2768-2769`).
- **[P2] Negative lookups never cached** (`:1749-1816`; `negative_timeout` stripped at `:4159-4161`) — miss-storms hammer the linear-scan dentry table (§1.3).

### 2.6 Metrics & misc

- **[Good]** Op counters are cache-line-aligned relaxed atomics (`fuse_client.rs:206-221,353-384`).
- **[P1] `.stats` generation enumerates every cache key in the process** (`:792-891`; `lru.rs:117-123`; `cache/nvme.rs:841-853`) — hundreds of thousands of `String` allocs per stat of `.stats`, TTL 0 so the kernel re-asks constantly. A 1 Hz monitoring poll = periodic latency spikes. Report counts/bytes; rate-limit regeneration.
- **[P1] Concurrent block flushes lose block-map updates.** `flush_single_active_block` does fetch-meta → insert one entry → save under only the per-block lock (`:4760-4772`); `buffer_unordered(8)` (`:4697`) + writeback worker run these concurrently for one inode ⇒ last save wins, **dropped block-map entry = data loss**. Needs a per-inode meta-commit lock or CAS/merge.
- **[P2] `ProbabilisticAtomic` TLS is latched to the first instance and loses up to 127 counts/thread** (`:157-199`) — safe only while exactly one instance exists.
- **[P2] Blocking calls on runtime workers:** `stdin().read_line` in the shutdown handler (`:4424`); std-mutex `writeback_rx.lock()` in async (`:617-619,1674`); `std::env::var("SQUEEZEFS_TIMEOUT")` **on every op** (`:23-27` — cache it in a `Lazy`); sync config-file I/O per `.config` lookup (`:658-700`).
- **[P2] `open_inodes` DashMap never removes zero-count entries** (`:601-607`) — one permanent entry per inode ever opened. `remove_if(count == 0)`.
- **[P2] `open`/`release` counting has TOCTOU races with `unlink`'s `is_open` check** (`:596-615, 3552-3557, 2708`).
- **[P3] Per-op `format!("inode_{ino}")` allocations** (`lib.rs:162-164`; raw duplicates at `fuse_client.rs:1084,3227-3228,3902,4554`).

---

## 3. Data path (`src/routing.rs`, `src/crypto_compress.rs`, `src/cache/`, `src/tiering/`)

### 3.1 Layout metadata & write-path correctness

- **[P0] Layout JSON overflows the 1,024-byte xattr limit — writes fail after data is written.**
  `routing.rs:544-569` serializes `LayoutMetadata` (incl. `data_key: Option<Vec<u8>>` inline payload and the full `block_map`) as JSON into `setxattr(ino,"layout",…)`, hard-capped at 1,024 B (`xattr.rs:140-144`). serde_json renders `Vec<u8>` as a decimal array (~3.7×): **inline files > ~250 B fail**; striped block maps overflow at ~30–60 blocks (~120–240 MiB). This is a data-path availability bug *today*.
  *Fix:* binary layout record (zerocopy/bincode) with an overflow/indirect block; add tests writing a 4 KiB inline file and a 1 GiB striped file.

- **[P0] `write_striped`: no per-block locking, fencing token unused, double-free + lost-update under concurrency.**
  `routing.rs:1272-1509` — reads `block_map` (`:1296`), allocates new blocks (`:1420-1432`), frees old keys (`:1475-1478`), commits merged map (`:1481-1486`); takes no `BLOCK_FLUSH_LOCKS`; `_fencing_token` ignored (`:1278`). Two concurrent writers: both free the same old block (allocator corruption / live-block reuse), last meta save wins ⇒ the other writer's blocks leak and its data is silently lost.
  *Fix:* per-block `BLOCK_FLUSH_LOCKS` + fencing-token validation before the meta commit.

- **[P0] Staging merge worker clobbers layout without fencing and packs with the wrong crypto state.**
  `cache/nvme.rs:699-722`: `flush_batch` re-reads the layout, blindly sets `block_map[0]` and `file_id = None` — no fencing check, no coordination with concurrent writes/promotions ⇒ resurrects stale data over striped maps. And `cache/nvme.rs:624-628,677` constructs `CryptoCompressState::new("none","none",None)` for packing, while readers decode with the **global** crypto state (`routing.rs:1003,1554,1669`) — with lz4/zstd/AEAD enabled, merged blocks read back as garbage.
  *Fix:* pass the router's crypto state into the merge worker; compare fencing token + `file_id` before committing; cancel superseded pending merges (`routing.rs:1094-1116`).

- **[P0→P1] Inline/staged writes are whole-file RMW — O(n²) write amplification.** `routing.rs:984-1064,1088-1147`: each write reads the entire existing payload (up to 4 MiB copy, `cache/nvme.rs:487-508`), patches, and re-stages the whole file under a fresh UUID. A 4 KiB-at-a-time sequential writer to a 4 MiB file copies ~2 GiB. *Fix:* share the block-granular delta path `write_file_staged` already implements.

- **[P1] Layout promotion materializes the whole file in RAM under the inode write guard** (`routing.rs:1023-1059,1148-1177`, `EntireOp` scope per `fuse_client.rs:134-141`) — full re-upload with concurrent readers stalled; exactly the P1-8 pattern AGENTS.md forbids for the striped path.

- **[P1] `save_metadata_to_backend` = 2 backend RTTs + full JSON + two full `block_map` clones per data write** (`routing.rs:554-575` + callers `:1043,1482`), each taking the exclusive backend `I{ino}` lock. Dirty-delta the map; combine setxattr+setattr; keep `Arc<HashMap>` in `CachedMetadata`.

- **[P1] Backend health checks do blocking `std::fs` syscalls per stripe-block allocation** (`routing.rs:118-227`, called per block at `:1211,1420-1421`). The 5 s health worker (`:344-431`) already exists — cache size/health in atomics and read those.

- **[P1] `read_file` double-caches whole files alongside their blocks** (`routing.rs:1563-1617`): assembles all blocks into one `Vec` (copy #1) then `Bytes::from(data.clone())` (copy #2) into `read_lru` keyed by path, while the same plaintext blocks sit in `read_lru` keyed by block. Halves effective cache capacity; large-file allocation spikes.

- **[P2]** 1 s metadata TTL races cross-node layout changes (`:765-768`); stringly-typed block keys parsed/formatted per block (`:248-259,1231,1423` — use a `(backend, offset)` struct); single-flight waiters hard-fail on primary fetch error instead of retrying (`:703-760`).

### 3.2 Crypto/compression

- **[P1] zstd/lz4 + AES-GCM run inline on tokio workers — never `spawn_blocking`.** `crypto_compress.rs:154-192,350-384`; hot call sites `routing.rs:680,1003,1235,1430`, `fuse_client.rs:1186-1194`. A 4 MiB zstd block ≈ tens of ms of CPU; with up to 64 concurrent block tasks, all runtime workers pin in compression and the uring commit loops starve. *Fix:* route ≥64 KiB payloads through `spawn_blocking`/a compute pool; keep the passthrough fast path inline.
- **[P2] Two avoidable full-payload memcpys per encrypt, one per decrypt** (`crypto_compress.rs:265-281,339-347`) — reserve header space up front.
- **[Good]** RSA session-key wrap happens once at init; unwraps are moka-cached; `get_crypto()` is an uncontended `OnceCell` load (`routing.rs:646-656`).

### 3.3 Caches & staging

- **[P1] mmap staging `put`: sync page faults + up-to-4 MiB memcpy inside a shard write lock, on tokio workers.** `tiering/nvme.rs:173-237`; async callers `cache/nvme.rs:380-384,459` (only *some* call sites use `spawn_blocking`, e.g. `fuse_client.rs:1244`). Major faults + dirty-throttling stalls block the whole shard *and* the runtime worker. Consistent `spawn_blocking` (or a uring-registered writer) for all mmap puts.
- **[P1] `'static`-transmuted mmap read guards pin shard `RwLock`s across FUSE reply lifetimes.** `tiering/nvme.rs:432-482` (`get_static`), stashed as reply backing in `routing.rs:1869-1902,1952-1985,2018`. One slow reply convoys every writer (then every reader) of that shard. Replace with epoch/refcount pins.
- **[P1] Full staging-key scans per fsync/flush/unlink.** `fuse_client.rs:1281-1289`, `routing.rs:2348-2358` → `list_keys()` clones **every** staged key under every shard lock (`tiering/nvme.rs:628-639`). O(total staged) per fsync ⇒ quadratic fsync storms. Maintain a `DashMap<ino, SmallVec<block>>` index.
- **[P2] Clock eviction sweep (up to `max(queue*2,512)` steps) runs inline in one unlucky `put` on the read path** (`tiering/memory.rs:67-107`). Bound per-put work; defer to a sweeper.
- **[P2] Dehydration spawns one `spawn_blocking` per >64 KiB evicted block** (`cache/mod.rs:104-125`; drops unmetered on channel overflow, `lru.rs:60,96`) — eviction storms saturate the shared blocking pool. Batch on one worker.
- **[P2] BufferPool: recycled buffers keep enlarged capacity; pool pre-allocates `cores×16 × 4 MiB` (4 GiB on 64 cores) regardless of config** (`cache/pool.rs:20-26,61-75`). `shrink_to(buf_size)` on return; size from block_size/budget.
- **[P2] Shard-count floors silently override operator cache limits** (`lru.rs:49-53`, `cache/nvme.rs:198-201,230-233`): 128-core node ⇒ ≥512 MiB per LRU regardless of request. Reduce shards instead.
- **[P2] Copying `read_staged`/`get_cached_read_block` (4 MiB per hit) dominate paths where zero-copy variants exist** (`cache/nvme.rs:487-508,798-802`; callers `routing.rs:994,1162,1545`).

### 3.4 Durability gaps (perf-relevant: they make current numbers unrealistically good)

- **[P1] No msync/fdatasync anywhere on the staging path** (`tiering/nvme.rs` — zero flush calls): staged writes are acked to the OS after an mmap memcpy; power loss loses acked data.
- **[P1] `NvmeBlockDev` O_DIRECT failure silently falls back to buffered with zero fsync** (`nvme_dev.rs:183-199`): the "block I/O durable before meta type flip" P0 invariant is void on such devices, and page-cache double-buffering competes with the RAM LRU. Fail loud per policy.

### 3.5 DHT / cluster tier (`tiering/dht.rs`, `p2p.rs`, `bg_admit.rs`)

- **[P1] Peer-controlled allocations: frame len (u32 ≤ 4 GiB) and `value_len` drive `vec![0u8; len]` / `set_len`-before-fill** (`dht.rs:330-334,357-375`) × 10,000 allowed bidi streams per connection (`:153`) — remote memory-exhaustion vector (and technically UB on partial-read error paths). Cap at block_size + slack.
- **[P1] Connection pool: no single-flight dial, no eviction.** `dht.rs:404,474-496` — N concurrent requests to a cold peer open N QUIC connections (N−1 orphaned for 30 s); one pooled connection per peer ever contacted, never reaped ⇒ 15k live QUIC conns at target scale, behind one global `Mutex<HashMap>`.
- **[P1] Inbound streams spawn unbounded untracked tasks** (`dht.rs:566-597`; `eprintln!` on runtime paths). Route through bounded admission.
- **[P2] `providers` map grows per key hash forever** (`dht.rs:397,615,690-692`) — needs TTL/refresh.
- **[P2] `bg_admit` is well designed** (drop-not-queue, bounded sems, `bg_admit.rs:20-26,63-65`), but sequential `acquire_owned` loops while holding earlier permits let one large ranged read capture all `STRIPED_IO_SEM` permits and head-of-line block everyone (`routing.rs:1716-1724,2149-2157`); sem capacity freezes at first touch while `set_striped_block_concurrency` changes only the write-side number (`bg_admit.rs:63-65,98-103`).

---

## 4. Block I/O engines (`src/nvme_dev.rs`, `src/uring_fs.rs`, `src/block_allocator.rs`)

- **[P0] Block allocator has no persistence — remount overwrites live data.**
  `block_allocator.rs:18-27,35-46` starts every mount with `highest_block = 0`; the only rebuild mechanism (`allocate_specific_block`, `:99`) has **zero callers**; `_client`/`_volume_id` are unused. First allocation after remount returns offset 0 and the write path (`routing.rs:1123,1222,1422`) clobbers existing blocks. *Fix:* rebuild from `block_map` metadata or persist a bitmap in MetaLV.

- **[P0] Cancellation-unsafe zero-copy: the uring worker keeps raw pointers into caller memory after the future is dropped.**
  `nvme_dev.rs:508-531` (`WriteData::Aligned { ptr: SendConstPtr(...) }`), `:664-669` (`dest_addr` reads). If the awaiting future is cancelled (FUSE op timeout, `select!`, shutdown), the kernel still DMAs from/to freed memory ⇒ use-after-free. *Fix:* requests must own their buffers (pool guard / owned `Bytes`), or registered buffers with completion-owned guards.

- **[P0] `read_block` can overflow the 4 MiB pool buffer and silently truncates short reads.**
  `nvme_dev.rs:670-672,396-400` + `pool.rs:179-185`: caller-supplied `size` unchecked against `buf_size` (configurable block sizes > 4 MiB ⇒ kernel writes past the allocation = heap corruption); any non-negative CQE `res` returns `bytes.slice(0..size)` — short reads hand back stale tail bytes as valid data. Bound the size; loop on short reads.

- **[P1] One uring worker thread per device; submissions gate on `submit_and_wait(1)`.** `nvme_dev.rs:142-146,204,375-378`: requests arriving during the wait sit in the channel until some CQE lands; single submitter core caps device throughput. No SQPOLL, no registered buffers (fixed file only, `:216`). Multi-issue eventfd loop or N rings per device.
- **[P1] Backlog > SQ depth fails requests with "Submission queue full" instead of backpressure** (`nvme_dev.rs:334-363`, channel 4096 vs SQ 1024) — spurious I/O errors to FUSE clients under load; and via `error.rs:25-26,62` queue-full maps to **EINVAL**, which applications treat as a bug, not "retry". Submit-and-retry on push failure; add a `Busy`/EAGAIN variant.
- **[P1] `uring_fs`: process-global, strictly serial (depth-in-flight = 1), with an fd cache that never evicts.** `uring_fs.rs:73,170-322`: one static worker, one blocking `submit_and_wait(1)` per op, inline blocking `open`/`create_dir_all`/`metadata`; `open_cache` (`:170,226,285-295`) leaks one fd per unique path until `EMFILE` poisons the whole process. Pipeline a slot table like `nvme_dev`; LRU-cap the fd cache.
- **[P2] Every unaligned write = oneshot alloc + full memcpy + tail memset** (`nvme_dev.rs:514,540-556,571-579`) — make pool-aligned buffers the norm end-to-end once the P0 ownership fix lands.
- **[P2] O_DIRECT alignment unvalidated on reads** (`nvme_dev.rs:654-693`; e.g. `jobs.rs:119` passes raw lengths) — EINVAL only at completion, masked by the buffered fallback.
- **[P2] `free_block` treats unknown offsets as freeable** (`block_allocator.rs:48-71`) — double-free/double-allocation corruption after remount (compounds the P0); `increment_refcount` yields 2 for missing entries (`:29-33`); two mutex hops + `HashSet::iter().next()` scan per alloc (`:35-46`).

---

## 5. Runtime, background workers, shutdown (`src/main.rs`, `src/jobs.rs`, misc)

- **[P1] Mount runtime: `max_blocking_threads(8192)` + core-pinning race.** `main.rs:1138-1152`: staging puts `spawn_blocking` on the write hot path (`fuse_client.rs:1049,1244,1408`) can burst thousands of OS threads; `on_thread_start` pins by a shared counter and **fires for blocking threads too** — early blocking threads steal `core_ids` slots, leaving real workers unpinned. Distinguish worker threads; cap the pool at 2–4× cores.
- **[P1] `fuse_client::init_runtime` claims core pinning but does none** (`fuse_client.rs:4108-4129` — `on_thread_start` only logs) and duplicates the real builder in `main.rs`. `tests/affinity_tests.rs` may be asserting a lie. Implement or delete (dead code).
- **[P1] `jobs.rs` worker: global mutex + 100 ms idle poll + O(jobs) scan + `Vec::remove(0)` + no shutdown** (`jobs.rs:81-171`); `submit_and_wait_for_job` busy-polls at 50 ms (`:65-77`); paused jobs leak their pending tasks (`:156-158`). Replace with mpsc + `Notify`, `VecDeque`, `CancellationToken`.
- **[P1] `defrag` and `recover_staging` are silent no-op stubs** (`defrag.rs:70-77`, `recovery.rs:3-12`) — `squeezefs defrag` reports success doing nothing; crash recovery of staged writes recovers nothing (orphaned staging data). Implement or fail loud.
- **[P2] Shutdown gaps:** no drain of the writeback queue before unmount (`fuse_client.rs:4460-4469` — only RAM→staging is flushed at `:4394`); in-flight writeback tasks abandoned at runtime drop; `heartbeat_handle` is a placeholder `tokio::spawn(async {})` (`:4271`); signal loop re-registers handlers every iteration and races `ctrl_c()` vs `sigint.recv()` (`:4294-4356`).
- **[P2] Umount drain loop re-scans and JSON-parses every staged key every 500 ms** (`main.rs:2710-2803`); `/proc/<pid>` polls at 100 ms (`:2907-2912,2972-2977`); fixed post-kill sleeps (`:2871,2943,2987`). Cold path, but O(files) per tick hurts with large staging sets.
- **[P2] Leaked forever-tasks with no shutdown story:** health-check worker (`routing.rs:350-353`), merge worker (`cache/nvme.rs:566`), dehydration (`cache/mod.rs:107`), prefetcher thread + private runtime (`routing.rs:2436-2479` — a dedicated OS thread + tokio runtime to run one channel recv), `P2pServer::run` 1-hour sleep loop (`p2p.rs:88-92`), `MockMessageStream` parked on `sleep(999999s)` (`dlm.rs:268`).

---

## 6. Sleep-based polling inventory

23 `sleep` call sites; the ones that matter under load:

| Location | Interval | Path | Verdict |
|---|---|---|---|
| `dlm.rs:159` | 50 ms × 3 | lock contention (op path) | **replace with Notify** |
| `fuse_client.rs:1012` | 50 ms × 40 | delegation wait (op path) | **replace with Notify** |
| `fuse_client.rs:3840` | 100 ms × 20 | blocking setlk (op path) | **replace with Notify** |
| `cache/nvme.rs:348-360` | spin up to 2 s | merge-queue capacity (write path) | latency cliff — use awaitable capacity |
| `tokio.rs:414-429` | 200 ms | idle inbound pop (per worker) | fold into per-qid async channel fix |
| `jobs.rs:65-77,167` | 50/100 ms | job completion/idle | event-driven queue |
| `fuse_over_uring.rs:364-388,619-647` | 1 ms / 250 ms | startup / connection watch | acceptable |
| `main.rs` umount/daemon polls | 100–500 ms | cold path | acceptable; noted above |

---

## 7. Unbounded-growth inventory

| Structure | Location | Growth driver | Sev |
|---|---|---|---|
| Meta DLM lock map | `meta_backend/dlm.rs:7` | +1 per unique ino/dentry key, never removed | P1 |
| `uring_fs` fd cache | `uring_fs.rs:170,226` | +1 fd per unique path → EMFILE | P1 |
| DHT connection pool | `dht.rs:404` | +1 QUIC conn per peer, never reaped | P1 |
| DHT inbound tasks | `dht.rs:590-595` | spawn per stream, 10k streams/conn | P1 |
| Reply/TPC channels | `session.rs:307,553,4542` | unbounded, carry full payloads | P1 |
| `open_inodes` | `fuse_client.rs:601-607` | +1 per inode ever opened (never removed at 0) | P2 |
| `FENCING_MAP` | `dlm.rs:139-143` | +1 per path ever locked | P2 |
| DHT `providers` | `dht.rs:397,615` | +1 per key hash, no TTL | P2 |
| `active_leases` | `fuse_client.rs:960` | +1 per written ino | P2 |
| Writeback spawn burst | `fuse_client.rs:4532-4553` | up to 4096 parked tasks | P2 |
| Pinned uring payload buffers | `fuse_over_uring.rs:679-699` | CPUs × depth × ≥1 MiB (bounded but huge) | P1 |
| Buffer pools | `pool.rs:69-75` | cores×16 × 4 MiB at first touch; fat-buffer recycling | P2 |

---

## 8. Prioritized remediation plan

### Wave 1 — correctness blockers (fix before any load testing)
1. **Writeback queue-full self-deadlock + lock inversion** — `fuse_client.rs:1342-1356` vs guards at `:1125/:2191/:4728-4733`.
2. **On-disk overlaps** — inode/dentry at 1 MiB; journal/xattr at 16 MiB (`inode.rs:5-6`, `dentry.rs:5`, `mod.rs:75`, `xattr.rs:5`). Requires an on-disk format rev.
3. **Layout record: replace 1 KiB JSON xattr with binary + overflow blocks** (`routing.rs:544-569`, `xattr.rs:140-144`).
4. **Block allocator persistence** (`block_allocator.rs`) + `free_block` unknown-offset handling.
5. **`write_striped` locking + fencing; merge-worker fencing + crypto state** (`routing.rs:1272-1509`, `cache/nvme.rs:624-628,699-722`); **concurrent-flush block-map merge** (`fuse_client.rs:4760-4772`).
6. **`nvme_dev` cancellation-safe buffer ownership + read size bound + short-read handling** (`nvme_dev.rs:396-400,508-531,664-693`).
7. **`rename` ABBA lock ordering** (`meta_backend/mod.rs:317-331`).

### Wave 2 — the metadata backend rewrite (dominant performance win)
8. Async uring I/O for `MetaLvStorage`; delete the file mutex; per-region locks; fsync out of locks (`storage.rs`).
9. Dentry hash chains + sector-batched scans + in-memory dentry cache; free-inode bitmap; real readdir paging (`dentry.rs`, `mod.rs`).
10. Real journal (atomic multi-sector ops) so per-write `sync_all` can become batched `fdatasync`.
11. Cached per-volume health counters (`mod.rs:582-606`); DLM map eviction.

### Wave 3 — transport & hot-path structure
12. Kill `spawn_blocking`-per-request; per-qid inbound queues; shard the `pending` map; qid-affine dispatch to collapse the 4-thread-hop pipeline (`tokio.rs:414`, `fuse_over_uring.rs:117,174`, `session.rs:4522-4602`).
13. Eliminate the two transport payload copies; pass `Bytes` end-to-end; bound reply channels; fix `Bytes::from_static` aliasing with owned guards.
14. `spawn_blocking` for ≥64 KiB compress/encrypt; mmap puts off the runtime; per-inode staging index to kill `list_keys()` scans.
15. Attr-cache TTL unification + readdirplus batching + lightweight read-only `release`.

### Wave 4 — hardening for 15k-node scale
16. DHT: dial single-flight, pool eviction, frame-size caps, bounded stream admission.
17. `nvme_dev` multi-issue worker + backpressure-not-EINVAL; `uring_fs` pipelining + fd-cache cap.
18. Runtime: blocking-pool cap, correct core pinning, writeback drain on shutdown, event-driven jobs worker, implement-or-fail-loud `defrag`/`recover_staging`.
19. Replace sleep-poll loops with `Notify`/watch (§6); real distributed DLM before any shared-MetaLV multi-node deployment.

### What's already good (keep)
- Cache-line-aligned relaxed-atomic metrics; `bg_admit` drop-not-queue admission; bounded staging merge queue with rollback; single-flight block reads with guard cleanup; moka TTL caches; zero-copy read-reply design (once the pointer-fetch locks and aliasing guard are fixed); RSA-once session-key handling.

---

*Generated from a four-track deep audit (metadata backend, FUSE hot path, data path/caches, block engines/infra). Line references are indicative anchors into `da9ba0b`; verify before patching.*
