#![allow(clippy::style, clippy::complexity, clippy::pedantic)]
// The stats-inode `serde_json::json!` literal exceeds the default macro
// recursion limit (128) — compile-time only, no runtime effect.
#![recursion_limit = "2048"]
pub mod mem_budget;
pub mod nt_copy;
// Session-arena THP helper — canonical file in the squeezefs-ipc tree,
// `#[path]`-included here and by `squeezefs-preload` (the `wake_core`
// production-sharing precedent: the ipc LIBRARY stays dependency-free,
// while both consumers already link libc). Two type identities exist by
// construction; instances never cross the crate boundary.
// N-topology-general NUMA nearest-resource map — canonical file in the
// squeezefs-ipc tree, `#[path]`-included here and by the fuse3 fork
// (the `thp.rs` production-sharing precedent). `src/numa.rs` is the
// METRICS-wired policy/instrument layer over it.
#[path = "../crates/squeezefs-ipc/src/numa_core.rs"]
pub mod numa_core;
// µs-bucket latency-histogram core — canonical file in the squeezefs-ipc
// tree, `#[path]`-included here (LatencyHistogram delegates to it) and by
// the fuse3 fork (read_transport_phase_ns), so root- and transport-side
// phase histograms bucket identically by construction.
#[path = "../crates/squeezefs-ipc/src/latency_core.rs"]
pub mod latency_core;
// The ONE env-knob parsing convention (ENG-10) — canonical file in the
// squeezefs-ipc tree, `#[path]`-included here, by the fuse3 fork and by
// the preload shim (the `numa_core`/`thp` production-sharing precedent, and
// required because the shared `numa_core` itself parses a knob: one
// `crate::env_knob_core` path must resolve in every including crate).
// Pure functions, no state — the duplicate type identities are inert.
// `src/env_knobs.rs` is the registry + startup refusal gate over it.
#[path = "../crates/squeezefs-ipc/src/env_knob_core.rs"]
pub mod env_knob_core;
pub mod nvme_dev;
#[path = "../crates/squeezefs-ipc/src/thp.rs"]
pub mod thp;
pub mod tiering;

pub mod numa;

pub mod assembly_tasks;
pub mod bench;
pub mod bg_admit;
pub mod cache;
// DLM stage S3 (pre-rc spec §6.7 *Transport*, §6.9 S3): the ONE cluster
// transport — binary framing, zero-config storage-trust mutual authn with a
// per-frame session MAC, DISC-1 peer auto-discovery, and the pinned
// owner-side RPC venue. `job_wire` rides it; S4's lock verbs plug into it.
pub mod cluster_wire;
pub mod config_ops;
pub(crate) mod cow_core;
pub mod cpu;
pub mod crypto_compress;
// DLM stage S7 (pre-RC spec §6.9 / §6.7 / RES-6): the data plane's
// custody-epoch fence — ONE authorization point for every DMA submission —
// the dead-epoch allocation quarantine, and the shared WERO hold on data
// namespaces. (Module docs live in the file: an outer doc comment here
// would merge into the crate root's link scope and break its intra-doc
// links.)
pub mod data_custody;
pub mod defrag;
/// RES-8 (pre-RC spec §7): unwind containment + counting for detached
/// (fire-and-forget) data-path tasks.
pub mod detached;
/// TEST-1 (pre-RC spec §11): the data-device power-cut harness — the
/// `uring_fs` volatile-cache simulator's coverage extended to the
/// `NvmeBlockDev` worker. Test-only; inert until armed.
pub mod dev_power_cut;
pub mod dlm;
// DLM stage S4 (pre-rc spec §6.7 decisions 2/3, §6.9 S4): the slot-homed lock
// authority — lock homing over the durable meta slot map plus the lock-free
// ownership query, in solo mode (this node owns every slot, `dlm_rpcs == 0` by
// construction). `dlm::DlmClient` resolves to its manager.
pub mod dlm_slot;
// ENG-10: the env-knob registry + the startup refusal gate. The parsing
// convention itself lives in the `#[path]`-shared `env_knob_core`. A plain
// comment, not a doc comment: an outer doc here is concatenated ahead of the
// module's own `//!` docs and resolves their intra-doc links in the CRATE
// scope, which silently breaks them.
pub mod env_knobs;
pub mod error;
pub mod fsck;
pub mod fuse_client;
pub(crate) mod gauge_core;
pub mod health;
pub(crate) mod incarnation_core;
pub(crate) mod ipc_direct;
pub mod ipc_host;
pub mod ipc_service;
pub mod job_wire;
/// Encryption key input, derivation, and mount-time resolution
/// (VAL-3 + KW-1 — `docs/design-key-handling.md`).
pub mod keyfile;
/// Per-writer lane cursors — the shared core of pre-RC engineering spec
/// §6.2 items 5 (ino lanes) and 6 (block-key incarnation stamps);
/// `docs/design-mw-cursors-and-incarnation.md`.
pub mod lane_core;
pub mod layout_wire;
pub mod meta_backend;
// DLM stage S8 (pre-rc spec §6.7 decision 1, §6.9 S8): metadata function
// shipping — the ownership plane, the verb vocabulary on the S3 cluster
// wire, the pipelined client router, the owner-side service and the client
// token cache. Solo mounts are unarmed: one relaxed load, then today's
// path. A plain comment, not a doc comment (the `env_knobs` note above).
pub mod meta_ship;
pub mod nvmeof;
pub(crate) mod patch_clone_core;
pub(crate) mod placed_core;
pub(crate) mod placed_sever;
pub mod read_lane;
pub(crate) mod refcount_core;
/// DLM stage S5 — read-only coherent mounts (reader revalidation cadence,
/// purge-on-revalidation, the node-cache revalidation seam).
pub mod ro_coherence;
pub mod routing;

#[macro_export]
macro_rules! coz_progress {
    ($name:expr) => {
        #[cfg(all(feature = "coz-on", not(test)))]
        coz::progress!($name);
    };
    () => {
        #[cfg(all(feature = "coz-on", not(test)))]
        coz::progress!();
    };
}
pub mod block_allocator;
pub mod block_reclaim;
pub mod jobs;
pub mod storage;
pub mod stripe_locks;
pub mod supervisor;
pub mod uring_fs;
pub mod version;
/// DUR-2: the data-volume volatile-write-cache probe
/// (`data_volume_write_cache`).
pub mod write_cache;
pub mod write_pipeline;
pub(crate) mod write_pipeline_core;
/// Spec §6.2 items 8/10: the node-scoped writer identity that labels
/// staging keys and staging-generation stamps (incompat bit 10 — built,
/// never stamped, ruling D9).
pub mod writer_scope;
pub mod zcrx_lane;

use parking_lot::RwLock;

pub static FS_PREFIX: RwLock<&'static str> = RwLock::new("squeezefs");

pub static WRITE_VERIFICATION: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);

/// VAL-7h (pre-RC spec §3): open the `--log-file` target for append —
/// mode `0600`, `O_NOFOLLOW`, never truncating.
///
/// The pre-fix opens (three of them: the parent preflight, the post-fork
/// stdio redirect, and the `env_logger` pipe) were plain
/// `create(true).append(true)` — mode `0644` with symlink following. The
/// daemon log carries backing-device paths, staging directories, object
/// key names and refusal detail, so a world-readable log is a disclosure
/// channel; and a symlink planted at a predictable log path (e.g. under a
/// shared `/var/log` or `/tmp`) made a root daemon append through it to
/// any file on the box. `O_NOFOLLOW` refuses that with `ELOOP` — loudly,
/// which ENG-3's "the daemon must be audible" law then turns into a failed
/// mount rather than a silent redirect.
///
/// Existing regular files keep their bytes (append) and their mode is left
/// alone — an operator who deliberately widened a log target is not
/// second-guessed; only files this daemon CREATES are 0600.
pub fn open_log_file(path: &std::path::Path) -> std::io::Result<std::fs::File> {
    use std::os::unix::fs::OpenOptionsExt;
    std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(path)
}

/// The standard system search path — the fallback when nothing in the
/// inherited `PATH` survives [`sanitized_root_path`].
const DEFAULT_ROOT_PATH: &str = "/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin";

/// VAL-7h/VAL-7i (pre-RC spec §3): filter an inherited `PATH` down to
/// entries safe to resolve **root** subprocesses through.
///
/// SqueezeFS runs ~63 argv-only root subprocesses (`nvme`, `modprobe`,
/// `gcc`, `git`, `fusermount3`, …). Argv-only is the correct discipline
/// (no shell), but the *resolution* still walked whatever `PATH` the
/// caller supplied: a relative entry, an empty entry (which means the
/// CWD), or a group/world-writable directory ahead of `/usr/bin` lets any
/// local user choose the binary a root subprocess executes.
///
/// Dropped: empty entries, relative entries, non-directories, and
/// directories writable by group or other. Kept in the caller's order
/// (operators legitimately front-load `/usr/local/bin` for SPDK tooling).
/// An input with no survivors yields [`DEFAULT_ROOT_PATH`] rather than an
/// empty `PATH` — an empty `PATH` resolves nothing and would break every
/// verb with a confusing error instead of a hardened one.
pub fn sanitized_root_path(input: &str) -> String {
    use std::os::unix::fs::MetadataExt;
    let mut kept: Vec<&str> = Vec::new();
    for entry in input.split(':') {
        if entry.is_empty() || !entry.starts_with('/') {
            continue;
        }
        let Ok(md) = std::fs::metadata(entry) else {
            continue;
        };
        if !md.is_dir() || md.mode() & 0o022 != 0 {
            continue;
        }
        if !kept.contains(&entry) {
            kept.push(entry);
        }
    }
    if kept.is_empty() {
        return DEFAULT_ROOT_PATH.to_string();
    }
    kept.join(":")
}

/// Apply [`sanitized_root_path`] to this process's `PATH` when running
/// with euid 0 — one call at startup covers every root subprocess site.
/// A no-op for unprivileged runs (there the caller's `PATH` is the
/// caller's own risk, and rewriting it would break user tooling).
pub fn harden_root_subprocess_path() {
    // SAFETY: geteuid is trivially safe.
    if unsafe { libc::geteuid() } != 0 {
        return;
    }
    let before = std::env::var("PATH").unwrap_or_default();
    let after = sanitized_root_path(&before);
    if after != before {
        log::info!(
            "root subprocess PATH sanitized (VAL-7i): dropped relative/empty/\
             group-or-world-writable entries; using '{after}'"
        );
        std::env::set_var("PATH", &after);
    }
}

/// When write verification is enabled, check every N-th write (P2-9).
/// `1` = verify every write (historical default).
static WRITE_VERIFICATION_SAMPLE_N: std::sync::atomic::AtomicU64 =
    std::sync::atomic::AtomicU64::new(1);

static WRITE_VERIFICATION_COUNTER: std::sync::atomic::AtomicU64 =
    std::sync::atomic::AtomicU64::new(0);

pub fn write_verification_enabled() -> bool {
    WRITE_VERIFICATION.load(std::sync::atomic::Ordering::Relaxed)
}

pub fn set_write_verification(enabled: bool) {
    WRITE_VERIFICATION.store(enabled, std::sync::atomic::Ordering::Relaxed);
}

/// How often enabled write-verification issues a read-after-write.
/// `every_n == 1` verifies all writes; larger N samples roughly 1/N of writes.
pub fn set_write_verification_sample_rate(every_n: u64) {
    WRITE_VERIFICATION_SAMPLE_N.store(every_n.max(1), std::sync::atomic::Ordering::Relaxed);
}

pub fn write_verification_sample_rate() -> u64 {
    WRITE_VERIFICATION_SAMPLE_N
        .load(std::sync::atomic::Ordering::Relaxed)
        .max(1)
}

/// Whether this write should run read-after-write verification (enabled + sample).
#[inline]
pub fn write_verification_should_check() -> bool {
    if !write_verification_enabled() {
        return false;
    }
    let n = write_verification_sample_rate();
    if n <= 1 {
        return true;
    }
    WRITE_VERIFICATION_COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed) % n == 0
}

/// The inode-timestamp clock: `CLOCK_REALTIME_COARSE`, i64 ns carried in
/// the u64 storage word (pre-epoch representable — fstests generic/258).
///
/// **Why coarse (fstests generic/423):** under the default writeback
/// cache the kernel authors regular-file cmtime LOCALLY
/// (`fuse_update_ctime` → `inode_set_ctime_current`) from the coarse
/// clock, which lags fine `CLOCK_REALTIME` by up to one tick (measured
/// 1.85 ms at HZ=1000). Daemon stamps taken from the fine clock ran
/// AHEAD of every kernel stamp in the same tick, so a daemon-stamped
/// inode (423's socket) created moments before an `ln` carried a ctime
/// LATER than the kernel's link stamp — a cross-inode ctime inversion.
/// Every daemon-authored inode timestamp must ride the kernel's clock
/// domain: a stamp taken here can never exceed a kernel stamp authored
/// later (same-tick stamps compare EQUAL, the kernel's own semantics).
/// Pinned by `tests/attr_refresh_tests.rs::
/// daemon_inode_stamps_never_lead_the_kernel_coarse_clock`.
pub fn coarse_realtime_ns() -> u64 {
    let mut t = libc::timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    // SAFETY: clock_gettime with a valid clock id and a valid out
    // pointer; CLOCK_REALTIME_COARSE cannot fail on Linux.
    unsafe { libc::clock_gettime(libc::CLOCK_REALTIME_COARSE, &mut t) };
    t.tv_sec.wrapping_mul(1_000_000_000).wrapping_add(t.tv_nsec) as u64
}

/// RES-22 (pre-RC engineering spec §7): report a runtime
/// **concurrency-outcome** invariant violation — loud, counted, never
/// fatal.
///
/// `debug_assert!` is right for pure arithmetic (bounds, LBA alignment,
/// range ordering): the predicate is a property of the caller's
/// arguments and a violation is a coding error. It is wrong for a
/// predicate whose truth depends on a concurrent schedule — those hold
/// in every schedule the author imagined and fail in the one production
/// finds, and in a debug build the failure is a PANIC inside a handler
/// task. That is the class that already produced a lost-reply stall
/// here: the §5.4 transport-lease watchdog's `debug_assert` panicked
/// write-handler tasks whose invocations legitimately exceeded 1 s under
/// writeback backpressure — losing the FUSE reply (fsync in D-state
/// forever, umount joins) — and the fix was to make it the
/// loud-never-fatal `transport_lease_overlong` tripwire.
///
/// Counted in `invariant_tripwires` (stats inode; **0 on a healthy
/// daemon**). The log line is rate-limited to one per site per second so
/// a violation that fires per-op cannot itself become the outage.
pub fn note_invariant_tripwire(site: &'static str, detail: &str) {
    crate::fuse_client::METRICS
        .invariant_tripwires
        .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    static LAST: once_cell::sync::Lazy<scc::HashMap<&'static str, std::sync::atomic::AtomicU64>> =
        once_cell::sync::Lazy::new(scc::HashMap::new);
    let now = coarse_realtime_ns() / 1_000_000_000;
    let quiet = LAST
        .read_sync(&site, |_, last| {
            last.swap(now, std::sync::atomic::Ordering::Relaxed) == now
        })
        .unwrap_or_else(|| {
            let _ = LAST.insert_sync(site, std::sync::atomic::AtomicU64::new(now));
            false
        });
    if !quiet {
        log::error!(
            "INVARIANT TRIPWIRE '{site}': {detail} — a concurrency outcome the \
             design says cannot happen just happened. Counted in \
             invariant_tripwires; the daemon proceeds (a panic here would lose \
             a FUSE reply, which is strictly worse than a wrong-but-served op)"
        );
    }
}

pub fn fs_prefix() -> &'static str {
    *FS_PREFIX.read()
}

pub fn set_fs_prefix(prefix: &str) {
    if !prefix.is_empty() {
        let leaked = Box::leak(prefix.to_string().into_boxed_str());
        *FS_PREFIX.write() = leaked;
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct FsKey(pub compact_str::CompactString);

impl std::ops::Deref for FsKey {
    type Target = str;
    fn deref(&self) -> &Self::Target {
        self.0.as_str()
    }
}

impl AsRef<str> for FsKey {
    fn as_ref(&self) -> &str {
        self.0.as_str()
    }
}

impl AsRef<[u8]> for FsKey {
    fn as_ref(&self) -> &[u8] {
        self.0.as_bytes()
    }
}

impl std::fmt::Display for FsKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

/// Destination for sync-serve payload bytes (the 2026-07-28 op-economy
/// campaign): the §5.5.1 tier serve legs write straight into the ring
/// op's arena window instead of bouncing through an intermediate
/// heap-allocated buffer (one alloc + one memcpy per warm op, deleted).
/// Implementors tolerate concurrent client writes to the destination
/// (the ipc arena is client-writable shared memory — §5.3.1).
pub trait PayloadSink {
    /// Copy `bytes` to `off` within the sink (clamped to the sink's
    /// bounds by the implementor).
    fn write_at(&self, off: usize, bytes: &[u8]);
    /// Zero `len` bytes at `off` (hole/short-tail composition).
    fn zero_at(&self, off: usize, len: usize);
}

pub fn build_fs_key(suffix: &str) -> FsKey {
    let prefix = fs_prefix();
    let mut s = compact_str::CompactString::with_capacity(prefix.len() + 1 + suffix.len());
    s.push_str(prefix);
    s.push(':');
    s.push_str(suffix);
    FsKey(s)
}

#[macro_export]
macro_rules! fs_key {
    ($suffix:expr) => {
        $crate::build_fs_key(&$suffix)
    };
}

/// Garnet/Redis key construction for the hot path (P2-1 / P2-2).
///
/// # Namespace convention
///
/// - **Volume keys** (format, free_blocks, used_bytes, attr, dir, job sets, …):
///   always under [`fs_prefix`] via [`fs_key!`] / [`build_fs_key`] / [`keys::attr`].
/// - **Layout keys** (per-file metadata, inline payload, block maps, staging maps,
///   active-block buffers): **unprefixed** historical names
///   (`metadata:…`, `inline_data:…`, `block_map:…`, `mapping:…`, `active_block:…`).
///   Do **not** put these under `FS_PREFIX` without an on-disk format migration —
///   existing volumes already store the unprefixed forms.
///
/// Prefer these helpers over ad-hoc `format!(…)` so key shape stays consistent and
/// we avoid redundant intermediate `String`s on the FUSE write/read path.
///
/// # Writer scope (spec §6.2 item 8)
///
/// The staging families (`active_block:`, `active_block_ext:`, `mapping:`)
/// name node-PRIVATE payloads, so on a writer-scoped volume set (incompat
/// bit 10 — never stamped today, ruling D9) they carry a trailing
/// `:w_{16 hex}` scope component. It is appended by these helpers only:
/// an ad-hoc `format!` of a full key would mint an UNSCOPED key that
/// probes the wrong record. Scan PREFIXES stay unscoped by design (a
/// foreign record must be visible to be classified) — see
/// [`crate::writer_scope`].
pub mod keys {
    use super::FsKey;
    use compact_str::CompactString;
    use std::fmt::Write;

    /// Append this process's writer-scope component (spec §6.2 item 8) —
    /// a no-op, byte-for-byte, on every un-stamped volume set (ruling D9:
    /// nothing stamps incompat bit 10 today, so this is the shipped path).
    ///
    /// The scope goes LAST, after the identity components, which is what
    /// keeps every historical scan prefix (`active_block:inode_{ino}:`,
    /// `active_block_ext:{file_path}:`) matching its own records — the
    /// same reservation the durable-block-refcount key layout made for a
    /// writer id after `block_index`
    /// (docs/design-durable-block-refcounts.md §3.2). Recovery classifies
    /// with [`crate::writer_scope::classify_key`]; the visibility is
    /// deliberate — a foreign record must be SEEN to be classified.
    #[inline]
    fn push_writer_scope(s: &mut CompactString) {
        if let Some(suffix) = crate::writer_scope::scoped_key_suffix() {
            s.push_str(suffix.as_str());
        }
    }

    /// Fixed-capacity **stack** key: zero-heap formatting for the sync
    /// serve prelude (the 2026-07-28 op-economy campaign — every heap key
    /// on the warm §5.5.1 fast path was a convicted allocation site).
    /// Capacity covers every `active_block[_ext]:inode_{u64}:block_{u64}`
    /// form with headroom; [`StackKey::format`] returns `None` on
    /// overflow so pathological inputs fall back to the heap helpers
    /// instead of truncating (a truncated key would serve wrong data).
    pub struct StackKey {
        buf: [u8; 192],
        len: usize,
    }

    impl StackKey {
        /// Format `args` into a stack key; `None` = capacity overflow
        /// (caller falls back to the heap form — never truncates).
        #[inline]
        pub fn format(args: std::fmt::Arguments<'_>) -> Option<Self> {
            let mut k = StackKey {
                buf: [0; 192],
                len: 0,
            };
            struct W<'a>(&'a mut StackKey);
            impl Write for W<'_> {
                fn write_str(&mut self, s: &str) -> std::fmt::Result {
                    let end = self.0.len.checked_add(s.len()).ok_or(std::fmt::Error)?;
                    if end > self.0.buf.len() {
                        return Err(std::fmt::Error);
                    }
                    self.0.buf[self.0.len..end].copy_from_slice(s.as_bytes());
                    self.0.len = end;
                    Ok(())
                }
            }
            let mut w = W(&mut k);
            w.write_fmt(args).ok()?;
            Some(k)
        }

        /// [`Self::format`] plus this process's writer-scope component
        /// (§6.2 item 8) — the scoped mint for the zero-heap staging-key
        /// paths. `None` on capacity overflow, INCLUDING the overflow the
        /// suffix itself would cause: a stack key that cannot hold its
        /// scope must fall back to the heap helper, never emit an
        /// unscoped key that a peer could collide with.
        #[inline]
        pub fn format_scoped(args: std::fmt::Arguments<'_>) -> Option<Self> {
            let mut k = Self::format(args)?;
            if let Some(suffix) = crate::writer_scope::scoped_key_suffix() {
                let end = k.len.checked_add(suffix.len())?;
                if end > k.buf.len() {
                    return None;
                }
                k.buf[k.len..end].copy_from_slice(suffix.as_bytes());
                k.len = end;
            }
            Some(k)
        }

        #[inline]
        pub fn as_str(&self) -> &str {
            // SAFETY-free: only whole `&str`s were copied in, at valid
            // boundaries (write_str appends complete UTF-8 slices).
            std::str::from_utf8(&self.buf[..self.len]).expect("StackKey holds concatenated strs")
        }
    }

    impl std::ops::Deref for StackKey {
        type Target = str;
        fn deref(&self) -> &str {
            self.as_str()
        }
    }

    /// Zero-heap `inode_{ino}` (fits always: 6 + ≤ 20 chars).
    #[inline]
    pub fn inode_path_stack(ino: u64) -> StackKey {
        StackKey::format(format_args!("inode_{ino}")).expect("inode path fits StackKey capacity")
    }

    /// Zero-heap `active_block:inode_{ino}:block_{block}` plus the
    /// writer scope when engaged (fits always: 66 + 19 ≤ 192).
    #[inline]
    pub fn active_block_stack(ino: u64, block: u64) -> StackKey {
        StackKey::format_scoped(format_args!("active_block:inode_{ino}:block_{block}"))
            .expect("active_block key fits StackKey capacity")
    }

    /// Zero-heap scoped `active_block:{file_path}:block_{block}` — the
    /// path-form twin of [`active_block_stack`]. `None` on capacity
    /// overflow (pathological path length): the caller falls back to
    /// [`active_block_for_path`], which is scoped too.
    #[inline]
    pub fn active_block_path_stack(file_path: &str, block: u32) -> Option<StackKey> {
        StackKey::format_scoped(format_args!("active_block:{file_path}:block_{block}"))
    }

    /// Zero-heap scoped `active_block_ext:{file_path}:block_{block}` (the
    /// W2 existence-probe key; see [`active_block_path_stack`]).
    #[inline]
    pub fn active_block_ext_path_stack(file_path: &str, block: u32) -> Option<StackKey> {
        StackKey::format_scoped(format_args!("active_block_ext:{file_path}:block_{block}"))
    }

    /// Logical file path used as the in-process cache / layout identity: `inode_{ino}`.
    ///
    /// Returns [`String`] so it plugs into existing `String`-keyed caches (moka/scc)
    /// without extra conversion noise at call sites.
    #[inline]
    pub fn inode_path(ino: u64) -> String {
        format!("inode_{ino}")
    }

    /// Layout meta hash key: `metadata:inode_{ino}`.
    #[inline]
    pub fn metadata_for_inode(ino: u64) -> FsKey {
        let mut s = CompactString::with_capacity(20);
        let _ = write!(s, "metadata:inode_{ino}");
        FsKey(s)
    }

    /// Layout meta hash key for a path that is already `inode_N` (or similar):
    /// `metadata:{file_path}`.
    #[inline]
    pub fn metadata_for_path(file_path: &str) -> FsKey {
        let mut s = CompactString::with_capacity(10 + file_path.len());
        s.push_str("metadata:");
        s.push_str(file_path);
        FsKey(s)
    }

    /// Inline payload key: `inline_data:{file_path}`.
    #[inline]
    pub fn inline_data(file_path: &str) -> FsKey {
        let mut s = CompactString::with_capacity(12 + file_path.len());
        s.push_str("inline_data:");
        s.push_str(file_path);
        FsKey(s)
    }

    /// Block-map hash key: `block_map:{block_map_id}`.
    #[inline]
    pub fn block_map(block_map_id: &str) -> FsKey {
        let mut s = CompactString::with_capacity(10 + block_map_id.len());
        s.push_str("block_map:");
        s.push_str(block_map_id);
        FsKey(s)
    }

    /// Staged-file mapping hash key: `mapping:{file_id}` (+ the writer
    /// scope when engaged — §6.2 item 8).
    #[inline]
    pub fn mapping(file_id: &str) -> FsKey {
        let mut s = CompactString::with_capacity(8 + file_id.len());
        s.push_str("mapping:");
        s.push_str(file_id);
        push_writer_scope(&mut s);
        FsKey(s)
    }

    /// Active-block buffer / staging key: `active_block:inode_{ino}:block_{block}`
    /// (+ the writer scope when engaged — §6.2 item 8).
    #[inline]
    pub fn active_block(ino: u64, block: u64) -> FsKey {
        let mut s = CompactString::with_capacity(40);
        let _ = write!(s, "active_block:inode_{ino}:block_{block}");
        push_writer_scope(&mut s);
        FsKey(s)
    }

    /// Active-block key when `file_path` is already `inode_N`:
    /// `active_block:{file_path}:block_{block}` (+ the writer scope).
    #[inline]
    pub fn active_block_for_path(file_path: &str, block: u32) -> FsKey {
        let mut s = CompactString::with_capacity(24 + file_path.len());
        let _ = write!(s, "active_block:{file_path}:block_{block}");
        push_writer_scope(&mut s);
        FsKey(s)
    }

    /// Scan prefix for an inode's active blocks: `active_block:inode_{ino}:`.
    ///
    /// Deliberately **unscoped**: the writer scope is a trailing key
    /// component, so this prefix still matches every writer's records for
    /// the ino — which is what lets recovery SEE a foreign record in order
    /// to classify it ([`crate::writer_scope::classify_key`]). Sites that
    /// mean "mine only" filter with
    /// [`crate::writer_scope::key_is_mine`].
    #[inline]
    pub fn active_block_ino_prefix(ino: u64) -> CompactString {
        let mut s = CompactString::with_capacity(28);
        let _ = write!(s, "active_block:inode_{ino}:");
        s
    }

    /// Scan prefix when `file_path` is already `inode_N`:
    /// `active_block:{file_path}:` (unscoped — see
    /// [`active_block_ino_prefix`]).
    #[inline]
    pub fn active_block_path_prefix(file_path: &str) -> CompactString {
        let mut s = CompactString::with_capacity(14 + file_path.len());
        s.push_str("active_block:");
        s.push_str(file_path);
        s.push(':');
        s
    }

    /// W2 staged extent-record key (design-random-small-writes §5.2):
    /// `active_block_ext:inode_{ino}:block_{block}` — the 4 KiB-class spill
    /// form of a parked [`crate::cache::active_block::ActiveBlockBuf`]
    /// extent overlay (and the staged-layout rider's sub-image record,
    /// keyed at block 0). Deliberately NOT a prefix of `active_block:` so
    /// existing key-family scans never misparse it; classification points
    /// use [`crate::cache::nvme::key_is_block_family`].
    #[inline]
    pub fn active_block_ext(ino: u64, block: u64) -> FsKey {
        let mut s = CompactString::with_capacity(44);
        let _ = write!(s, "active_block_ext:inode_{ino}:block_{block}");
        push_writer_scope(&mut s);
        FsKey(s)
    }

    /// Extent-record key when `file_path` is already `inode_N`:
    /// `active_block_ext:{file_path}:block_{block}` (+ the writer scope).
    #[inline]
    pub fn active_block_ext_for_path(file_path: &str, block: u32) -> FsKey {
        let mut s = CompactString::with_capacity(28 + file_path.len());
        let _ = write!(s, "active_block_ext:{file_path}:block_{block}");
        push_writer_scope(&mut s);
        FsKey(s)
    }

    /// Scan prefix for an inode's staged extent records:
    /// `active_block_ext:inode_{ino}:` (unscoped — see
    /// [`active_block_ino_prefix`]).
    #[inline]
    pub fn active_block_ext_ino_prefix(ino: u64) -> CompactString {
        let mut s = CompactString::with_capacity(32);
        let _ = write!(s, "active_block_ext:inode_{ino}:");
        s
    }

    /// POSIX attr hash under the volume prefix: `{fs_prefix}:attr:{ino}`.
    #[inline]
    pub fn attr(ino: u64) -> FsKey {
        let prefix = super::fs_prefix();
        let mut s = CompactString::with_capacity(prefix.len() + 20);
        let _ = write!(s, "{prefix}:attr:{ino}");
        FsKey(s)
    }

    /// Directory listing hash under the volume prefix: `{fs_prefix}:dir:{ino}`.
    #[inline]
    pub fn dir(ino: u64) -> FsKey {
        let prefix = super::fs_prefix();
        let mut s = CompactString::with_capacity(prefix.len() + 20);
        let _ = write!(s, "{prefix}:dir:{ino}");
        FsKey(s)
    }
}

/// Durable data-volume state values (design-volume-lifecycle §5.3/§5.4).
/// `active`/`disabled` since VL3; the drain machinery (PR VL4) adds the
/// §5.4 state machine: `Active → Draining → Retired` (with
/// `Draining → Active` on `volume undrain`). `Retired` is terminal and
/// the record is kept FOREVER — the id is never reused (KD-5).
pub const VOL_STATE_ACTIVE: &str = "active";
pub const VOL_STATE_DISABLED: &str = "disabled";
/// §5.4: excluded from write placement and new allocations, but SERVES
/// READS and refcount ops normally until retired.
pub const VOL_STATE_DRAINING: &str = "draining";
/// §5.4 terminal state: evacuation census reached 0; the runtime backend
/// is deregistered and the record's path cleared, id retained forever.
pub const VOL_STATE_RETIRED: &str = "retired";

/// One member of the durable data-volume set (KD-5,
/// design-volume-lifecycle §5.3): a never-reused volume id, its current
/// backing device path, and its lifecycle state. Legacy sets (pre-VL3
/// `data_lv`-only configs) synthesize records whose ids are EXACTLY the
/// device-path basenames — the grandfathering that keeps every existing
/// `name://offset` block key resolving to the same backend.
#[derive(serde::Serialize, serde::Deserialize, Clone, Debug, PartialEq, Eq)]
pub struct DataVolumeRecord {
    /// `vol-{16 hex}` (random, allocated once, never reused) for volumes
    /// added by `squeezefs volume add-data`; the historical basename for
    /// grandfathered legacy members.
    pub id: String,
    /// Current backing device path (host-resolvable).
    pub backing_dev: String,
    /// [`VOL_STATE_ACTIVE`] / [`VOL_STATE_DISABLED`] (VL4 adds
    /// draining/retired).
    pub state: String,
    /// Unix seconds when the record was created (`0` for synthesized
    /// legacy records).
    pub added_ts: u64,
}

/// Mint a fresh durable volume id: `vol-{16 hex}`, random, never reused
/// (KD-5 — retired ids are recorded forever so a stale key can never
/// resolve to the wrong device).
pub fn new_data_volume_id() -> String {
    format!("vol-{:016x}", fastrand::u64(..))
}

/// One member of the durable META-volume set (PR VL5a,
/// design-volume-lifecycle §5.5.1, KD-7): the `DataVolumeRecord`-style
/// identity meta volumes get on `--meta-slots` formats. This config
/// record is the HUMAN-READABLE MIRROR — the per-volume root-ledger
/// membership stamp ([`meta_backend::kv::checkpoint::MembershipStamp`])
/// is authoritative (§5.5.1a: the stamp solves the slot-0 bootstrap
/// chicken-and-egg the config, an xattr on ino 1, cannot).
#[derive(serde::Serialize, serde::Deserialize, Clone, Debug, PartialEq, Eq)]
pub struct MetaVolumeRecord {
    /// `vol-{16 hex}` (random, allocated once, never reused — KD-5).
    pub id: String,
    /// Backing device path at format time (host-resolvable).
    pub backing_dev: String,
    /// Canonical position in the set order (assigned at format = format
    /// order; the `volume_set_generation` ordering key, §5.5.1a).
    pub member_position: u16,
    /// Unix seconds when the record was created.
    pub added_ts: u64,
}

/// The grandfathered id of a legacy (`data_lv`) member: the device-path
/// basename, byte-identical to what mount has always registered.
pub fn legacy_volume_id(path: &str) -> String {
    std::path::Path::new(path)
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or(path)
        .to_string()
}

#[derive(serde::Serialize, serde::Deserialize, Clone, Debug)]
pub struct FormatConfig {
    pub name: String,
    pub block_size: u64,
    pub capacity: u64,
    pub inodes: u64,
    pub compression: String,
    pub encrypt_algo: String,
    /// LEGACY, READ-ONLY (VAL-3): pre-KW-1 binaries stored the RSA private
    /// key PEM **here**, in cleartext, on the volume it encrypts. It still
    /// deserializes so [`crate::keyfile::legacy_encrypted_volume_refusal`]
    /// can name such a volume precisely; `skip_serializing` means no
    /// current binary can ever write key material back (a config rewritten
    /// by `config set-cache-paths` drops the field). Redacting `Debug` +
    /// zeroize-on-drop live on [`keyfile::RedactedSecret`].
    #[serde(default, skip_serializing)]
    pub encrypt_key: Option<keyfile::RedactedSecret>,
    /// The whole persisted key surface on a KW-1 volume: a KDF salt and a
    /// key id, neither of them secret. The key itself resolves at mount
    /// from the flag, `SQUEEZEFS_ENCRYPT_KEY_FILE`, or
    /// `/etc/squeezefs/keys/<key_id>.key`
    /// (`docs/design-key-handling.md` §4).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub encrypt_key_ref: Option<keyfile::EncryptKeyRef>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub mem_cache_size: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub disk_cache_size: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub disk_cache_paths: Option<Vec<std::path::PathBuf>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub data_lv: Option<Vec<String>>,
    /// The durable data-volume set (design-volume-lifecycle §5.3, KD-5).
    /// `None` = legacy set: records are synthesized from `data_lv`
    /// basenames ([`FormatConfig::resolved_data_volumes`]) and the config
    /// stays byte-identical until the first lifecycle verb materializes
    /// them (which also stamps the `KV_VOLUME_LIFECYCLE` incompat bit —
    /// bit-before-durable-record ordering, §7).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub data_volumes: Option<Vec<DataVolumeRecord>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub read_cache_size: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub write_cache_size: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub read_mem_cache_size: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub write_mem_cache_size: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub dismount_wait: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub upload_delay: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub fuse_io_uring_sqpoll_idle_ms: Option<u32>,
    /// The DERIVED routing width W recorded at format
    /// (design-dynamic-meta-routing §5.1 — never a knob). MIRROR ONLY —
    /// mounts read W from the §5.5.1a membership stamps.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub meta_routing_width: Option<u32>,
    /// Per member (canonical order): the hosted-slot stride runs
    /// `(start, stride, count)` — O(runs), never O(W), so the mirror
    /// stays xattr-sized at the derived width (the retired per-slot
    /// `meta_slot_map` mirror was O(W)). MIRROR ONLY — the stamps'
    /// `slots_hosted` sets are authoritative.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub meta_slot_runs: Option<Vec<Vec<(u16, u16, u32)>>>,
    /// PR VL5a: the durable meta-volume identity records
    /// ([`MetaVolumeRecord`]), in `member_position` order. MIRROR ONLY.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub meta_volumes: Option<Vec<MetaVolumeRecord>>,
}

impl FormatConfig {
    /// The durable data-volume set this config names, in volume order:
    /// `data_volumes` verbatim when present, else records synthesized
    /// from the legacy `data_lv` paths with basename ids (grandfathering,
    /// KD-5 — mount must register backends under EXACTLY the same names
    /// as before VL3).
    pub fn resolved_data_volumes(&self) -> Vec<DataVolumeRecord> {
        if let Some(ref recs) = self.data_volumes {
            return recs.clone();
        }
        self.data_lv
            .as_deref()
            .unwrap_or_default()
            .iter()
            .map(|path| DataVolumeRecord {
                id: legacy_volume_id(path),
                backing_dev: path.clone(),
                state: VOL_STATE_ACTIVE.to_string(),
                added_ts: 0,
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_fs_prefix_default() {
        let prefix = fs_prefix();
        assert!(!prefix.is_empty());
    }

    #[test]
    fn test_write_verification_sample_gate() {
        set_write_verification(false);
        set_write_verification_sample_rate(1);
        assert!(!write_verification_should_check());

        set_write_verification(true);
        set_write_verification_sample_rate(1);
        assert!(write_verification_should_check());
        assert!(write_verification_should_check());

        set_write_verification_sample_rate(5);
        let mut hits = 0usize;
        for _ in 0..50 {
            if write_verification_should_check() {
                hits += 1;
            }
        }
        // Roughly 1/5 of 50 = 10; allow wide band for counter phase.
        assert!(
            (5..=20).contains(&hits),
            "expected ~10 sample hits in 50, got {hits}"
        );

        // Restore defaults so other tests are not affected.
        set_write_verification(false);
        set_write_verification_sample_rate(1);
    }

    #[test]
    fn test_layout_key_shapes_match_historical_format() {
        assert_eq!(keys::inode_path(42), "inode_42");
        assert_eq!(&*keys::metadata_for_inode(42), "metadata:inode_42");
        assert_eq!(&*keys::metadata_for_path("inode_7"), "metadata:inode_7");
        assert_eq!(
            &*keys::metadata_for_path(&keys::inode_path(7)),
            &*keys::metadata_for_inode(7)
        );
        assert_eq!(&*keys::inline_data("inode_1"), "inline_data:inode_1");
        assert_eq!(&*keys::block_map("abc"), "block_map:abc");
        assert_eq!(&*keys::mapping("fid"), "mapping:fid");
        assert_eq!(&*keys::active_block(9, 3), "active_block:inode_9:block_3");
        assert_eq!(
            &*keys::active_block_for_path("inode_9", 3),
            &*keys::active_block(9, 3)
        );
        assert_eq!(
            keys::active_block_ino_prefix(9).as_str(),
            "active_block:inode_9:"
        );
        assert_eq!(
            keys::active_block_path_prefix("inode_9").as_str(),
            "active_block:inode_9:"
        );
    }

    #[test]
    fn test_volume_attr_dir_use_fs_prefix() {
        let prefix = fs_prefix();
        assert_eq!(&*keys::attr(1), format!("{prefix}:attr:1"));
        assert_eq!(&*keys::dir(1), format!("{prefix}:dir:1"));
        // Same shape as fs_key! for attr suffix
        assert_eq!(&*keys::attr(1), &*build_fs_key("attr:1"));
    }
}
