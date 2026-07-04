Detailed Specification: Custom Shared-Block Metadata Backend for Rust FUSE FS
(“MetaLV” + DLM + io_uring Direct-Access Design)
This design completely removes the KV server, RESP protocol, network hop (even localhost), generic data structures, and parsing overhead. Every metadata operation becomes direct block I/O to a dedicated shared logical volume (/dev/mapper/metadata-lv), coordinated by the Linux kernel DLM (already used by your cluster for cLVM/nvme-oF locking). It scales exactly as your data path scales because it reuses the identical nvme-oF → LVM → shared-LUN model.
1. High-Level Architecture

Shared Storage Layer
One dedicated nvme-oF target exposed to all FUSE nodes → CLVM logical volume (metadata-lv, e.g. 64–512 GiB depending on inode count target). Formatted with a custom on-disk layout (no filesystem on it — raw block access with O_DIRECT).
Per-Node Components (Rust library meta-backend linked into your FUSE daemon)
In-memory cache (lock-protected slab + hash maps + LRU)
io_uring ring (dedicated or shared with your FUSE rings) for all reads/writes
DLM lock handles (fine-grained)
Tiny per-node journal buffer (flushed to MetaLV)

Coordination
Linux DLM (via libdlm + thin Rust wrapper) for inode locks, dentry locks, allocation locks.
Local spinlocks/mutexes only for cache; DLM for cross-node coherence.
Durability
Write-ahead journal (small circular log on MetaLV) + checkpointing. Matches or exceeds Garnet/Dragonfly persistence guarantees.
FUSE Integration
Replace your current Garnet client calls with meta_backend::InodeHandle / DentryHandle methods. All operations remain async and batchable.

2. On-Disk Format (Simple, Performant, Extensible)
Layout (all offsets 4 KiB aligned for direct I/O):

Superblock (sector 0): magic, version, inode count, free inode bitmap root, dentry B-tree root, journal start/size, checksums.
Inode Table (fixed-size slots, 256 bytes each): inode# → struct { mode, uid, gid, size, nlink, blocks[], atime/mtime/ctime, xattr_ptr, flags }. Use a sparse extent-like allocator or simple bitmap + slab for active inodes.
Dentry Index (B+tree or hash + chained buckets on disk): key = (parent_ino, hash(name)) → { child_ino, name_len, name (inline if short), type, next }. Supports fast lookup + readdir iteration.
Xattr & Symlink Blob Store (separate extent allocator or log-structured append area).
Journal (circular, 2–8 MiB): redo records + checksum + sequence. Two checkpoints for safety.
Allocator (bitmap + free-list B-tree for variable blocks).

Total overhead: ~1–2% of volume for metadata structures. Supports 100M+ inodes comfortably on a 128 GiB LV.
3. Rust Crates & Stack (2026-era, production-ready)

IO — tokio-uring or rio (high-level, nice API) + io-uring low-level fallback. Use registered buffers + fixed buffers for zero-copy.
Direct block access — O_DIRECT | O_RDWR on the raw LV device (opened once at startup).
DLM — Bind libdlm (or dlm from cluster stack) with bindgen + thin safe wrapper crate (or use the higher-level distributed-lock crate with a custom “block” backend if you want simplicity; kernel DLM preferred for perf).
Data structures — zerocopy, bytemuck, bitflags, slab, dashmap or parking_lot::RwLock for cache, redb or custom B-tree (or btree + persistence) for on-disk indices.
Serialization — zerocopy + manual structs (no serde for hot path).
Async runtime — tokio (or glommio for thread-per-core if you want ultimate perf) with tokio-uring integration.
Testing — tempfile + loop devices or qemu for multi-node simulation.

4. Concurrency & Locking Model (DLM-Centric)

Lock Granularity
Inode lock (DLM lock name = "I<ino>" or hashed range) — EX for write, SH for read.
Dentry lock (per parent + name hash) — for create/unlink/rename.
Allocator lock (global or striped) — short-lived.

Locking Flow (example for create):
DLM lock(parent_inode, EX)
DLM lock(new_inode, EX) + allocate
Write journal entry (atomic)
Update dentry + inode on disk (batched io_uring)
DLM unlock

Cache Coherency
Use DLM “lock value block” (LVB) for cache invalidation hints or simple lease model (hold SH lock = cache valid). On DLM lock grant, invalidate or refresh cache entry.
Local fast path — If you hold the DLM lock already (common in directory traversal), skip re-locking.

This is exactly how GFS2/OCFS2 work but simplified for metadata-only and tuned for your workload.
5. io_uring & Async Integration

One (or two) dedicated IoUring instance per FUSE worker thread or shared ring with linked SQEs for dependent ops (e.g., journal → data → checkpoint).
All reads/writes use IORING_OP_READ/WRITE with registered buffers + IOSQE_IO_LINK for atomicity where needed.
Batch metadata ops naturally (your FUSE already batches via io_uring — just feed multiple metadata requests into the same ring).
Direct I/O + huge pages + huge TLB for maximum throughput.

Expected: sub-50 µs p50 for hot cached lookups, <200 µs cold (with DLM + disk) on NVMe-oF.
6. Key Operations & FUSE API Mapping
Rustpub trait Metadata {
    async fn lookup(&self, parent: Ino, name: &str) -> Result<Inode>;
    async fn create(&self, parent: Ino, name: &str, mode: u32) -> Result<Inode>;
    async fn unlink(&self, parent: Ino, name: &str) -> Result<()>;
    async fn rename(&self, old_parent: Ino, old_name: &str, new_parent: Ino, new_name: &str) -> Result<()>; // fully atomic
    async fn readdir(&self, dir: Ino, offset: u64, max: usize) -> Result<Vec<DirEntry>>;
    // ... getattr, setattr, readlink, symlink, xattr get/set/list, etc.
}
All methods are async and return futures that drive the io_uring + DLM.
Rename uses a single journal record + two dentry updates under held locks → fully atomic across nodes.
7. Crash Recovery & Consistency

Every mutating op writes a redo journal record first (io_uring linked).
On mount: replay journal from last checkpoint, verify checksums.
Checkpoint every 5–30 s (background task) or on sync/fsync.
Guarantees: metadata is always consistent after crash (journal replay), no dangling references, atomic rename/link.

8. Scalability, HA & Operations

Scales with nvme-oF targets + LVM (add more targets, grow LV, more nodes — DLM handles it).
HA: same cluster stack you already use for data path. Node failure → other nodes continue (DLM releases locks).
Monitoring: expose Prometheus metrics (io_uring depth, DLM lock contention, cache hit rate, journal replay time).
Backup: snapshot the MetaLV or rsync the journal + data at checkpoint.
Sharding (future): hash inode ranges across multiple MetaLVs if you ever exceed single-LV limits.

9. Implementation Phases (Realistic Timeline)
Phase 0 (1–2 weeks) — Skeleton + superblock + inode table read/write + basic io_uring direct access + DLM binding proof-of-concept. Single-node, no journal.
Phase 1 (2–3 weeks) — Full CRUD for inodes + dentries, journal, crash recovery, basic FUSE integration (replace Garnet for lookup/create/unlink).
Phase 2 (2–3 weeks) — Rename, readdir, xattr, caching + DLM coherence, multi-node testing.
Phase 3 (1–2 weeks) — Polish, benchmarks, fsck tool, monitoring, production hardening.
Total: 6–10 weeks for a solid MVP that beats your current Garnet path, then ongoing optimization.
Recommended starting repo structure
meta-backend/ with storage.rs, dlm.rs, journal.rs, inode.rs, dentry.rs, bench/, fuzz/.
10. Expected Wins & Trade-offs
Wins vs current Garnet/Valkey/Dragonfly:

Zero protocol/network overhead → 2–5× lower p99 latency possible.
Perfect FS semantics (atomic rename, exact crash behavior).
Full control over caching, batching, layout.
Scales with your existing storage fabric.
Lower CPU/RAM (no Redis structures).

Trade-offs:

You own the metadata engine (but it’s simpler than a general KV).
DLM adds a small coordination cost (still far cheaper than KV round-trips).
Initial dev time (but you already have the team building a high-perf FUSE).

Performance Targets to Validate

Hot lookup: <20 µs
Cold create + fsync: <150 µs
1M inodes, 10k ops/sec mixed workload across 4 nodes: stable, low tail latency, <5% CPU overhead vs current KV.

This design is battle-tested in spirit by every clustered filesystem (GFS2/OCFS2) but stripped to metadata-only and written in modern Rust with io_uring from day one — exactly what your stack deserves.