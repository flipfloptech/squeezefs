# SqueezeFS Operations Reference

This is the operator reference for SqueezeFS: the durability contract and its guarantee classes, the breaking-changes catalog, the configuration-knob reference beyond `--help`, the observability surfaces, NVMe-oF target operations, and the measured performance record. The hands-on walkthrough (sandbox → bare metal → fabric) is [QUICKSTART.md](../QUICKSTART.md); the project overview is [README.md](../README.md); normative designs live in `docs/design-*.md` and measurement records in `.benchmarks/` (each note states its box, substrate, and method).

## Contents

- [Versioning & releases](#versioning--releases)
- [Durability & crash contract](#durability--crash-contract)
  - [Metadata Durability (crash contract)](#metadata-durability-crash-contract)
  - [Single-writer mount guard (guarantee classes)](#single-writer-mount-guard-guarantee-classes)
  - [Format v3 (CoW KV metadata)](#format-v3-cow-kv-metadata)
- [Breaking changes & migration notes](#breaking-changes--migration-notes)
- [Removed verbs & flags](#removed-verbs--flags)
- [Configuration reference](#configuration-reference)
  - [SqueezeFS URI scheme](#squeezefs-uri-scheme)
  - [Format (`squeezefs format`)](#format-squeezefs-format)
  - [Mount (`squeezefs mount`)](#mount-squeezefs-mount)
  - [LD_PRELOAD interception (`-o interception`)](#ld_preload-interception--o-interception--security-posture--unsupported-mixes)
  - [Cache/staging paths (`squeezefs config`)](#cachestaging-paths-squeezefs-config)
  - [Transparent compression & encryption](#transparent-compression--encryption)
  - [Read-path tuning](#read-path-tuning-mount-env-design-docsdesign-read-pathmd)
  - [Hybrid I/O for O_DIRECT reads and the device-true escape](#hybrid-io-for-o_direct-reads-default-and-the-device-true-escape)
  - [Random-small-write path](#random-small-write-path-sole-owner-patch--extent-overlay-design-docsdesign-random-small-writesmd)
  - [FUSE transport in-flight concurrency](#fuse-transport-in-flight-concurrency-defaults-are-the-l1-policy-knobs-are-overrides)
  - [FUSE io_uring SQPOLL](#fuse-io_uring-sqpoll-mount-env-measured--leave-unset)
  - [Kernel cache TTLs](#kernel-cache-ttls-mount-options--env-per-class)
  - [External mount supervisor](#external-mount-supervisor-mount---daemon---supervise)
  - [Host auto-tuning (`squeezefs tune`)](#host-auto-tuning-squeezefs-tune)
  - [Other verbs](#other-verbs)
- [Volume lifecycle & online maintenance](#volume-lifecycle--online-maintenance)
  - [Data volumes](#data-volumes)
  - [Metadata volumes](#metadata-volumes)
  - [fsck / scrub](#fsck--scrub)
  - [Defragmentation](#defragmentation)
  - [Jobs & distributed execution](#jobs--distributed-execution)
- [Observability](#observability)
  - [`df` / statfs semantics](#df--statfs-semantics)
  - [Fabric observability](#fabric-observability)
- [NVMe-oF operations](#nvme-of-operations)
- [Performance records](#performance-records)
  - [The multi-reference scoreboard (release gate)](#the-multi-reference-scoreboard-release-gate)
  - [Built-in benchmark (`squeezefs bench`)](#built-in-benchmark-squeezefs-bench)

---

## Versioning & releases

**A SqueezeFS build carries two identities, both surfaced** (user directive 2026-07-24, superseding the commit-only 2026-07-18 policy):

1. **The release-train version** — `Cargo.toml`'s package `version` (currently the **1.1 train**), bumped **as a release act** (never by CI, never per commit). The first-party crates (`crates/fuse3` — the fully-diverged fork, `crates/squeezefs-ipc`, `crates/squeezefs-preload`) track the same train.
2. **The git commit the build was produced from** — the fine-grained identity. Periodic releases remain **annotated git tags on specific commits** — `stable-YYYY.MM[.N]` and `lts-YYYY.MM` — created manually as a release act. The tag names the release; the commit pins the exact build. Tags are the **only** release names.

**Verify what a node is running** (the two surfaces carry the same build-time capture):

```bash
squeezefs --version        # or -V
# untagged build:  squeezefs 1.1.0 (f63455bcb824 / f63455bcb8249b064531d000624c40825a6e763e) built 2026-07-18T13:45:25Z
# release build:   squeezefs 1.1.0 (f63455bcb824 / f63455bcb8249b064531d000624c40825a6e763e, tag stable-2026.07) built 2026-07-18T13:45:25Z
grep -E '"build_(commit|tag)"' <mountpoint>/.stats   # the fleet mixed-version detector
```

The `.stats` inode exports `build_commit` (the full hash) and `build_tag` (always present; empty string when the commit is not a release) on every mounted daemon — sweep it across the fleet to find mixed-version nodes. A `-dirty` suffix on the hash means the binary was built from a tree with uncommitted **tracked** changes (untracked scratch does not count; the flag is captured when the build script runs) — a dirty rebuild of a tagged commit deliberately does not masquerade as the release.

Mechanics and edges:

- The commit identity is captured at compile time by `build.rs` (`git rev-parse HEAD`, `--short=12`, `git status --porcelain --untracked-files=no`, `git describe --tags --exact-match`) and embedded via `SQUEEZEFS_BUILD_*` rustc envs; the train version is `CARGO_PKG_VERSION`; `src/version.rs` is the single formatting source of truth for `--version`, the mount/format summary, the mount-ready log lines, `.config`'s `client_version`, and `.stats`.
- **Bumping the train is a release act**: edit the package `version` in the root `Cargo.toml` and the first-party crate manifests (`crates/fuse3`, `crates/squeezefs-ipc`, `crates/squeezefs-preload`) together — they carry the same train. (The pre-2026-07-24 "`0.1.0` cargo-internal placeholder, never bump" posture is retired.) The `.stats` `build_commit`/`build_tag` fields stay raw commit/tag — fleet tooling never parses the version line.
- **Tarball / no-git builds**: packagers set `SQUEEZEFS_BUILD_COMMIT` (and optionally `SQUEEZEFS_BUILD_TAG`) in the build environment to stamp the identity; without git *and* without the envs, the build still succeeds and embeds `unknown`. `SOURCE_DATE_EPOCH` is honored for a reproducible build timestamp.

---

## Durability & crash contract

### Metadata Durability (crash contract)

SqueezeFS metadata is **format v3** (CoW KV) — the only supported metadata format (v2 support was removed; v2 volumes refuse to mount with "no longer supported; reformat required"). Its crash contract holds **by construction** (design: `docs/design-cow-kv-metadata.md`; the historical D0/D1/D2 ladder it strictly strengthens is `docs/design-wal-crash-consistency.md` §3):

- **Every on-disk unit is checksummed** — superblock, journal pages and entries, btree nodes, bsets, the allocator bitmap, and root-ledger slots.
- **Torn writes are detected and ignored, never applied.** A torn journal entry, node append, or ledger slot fails its checksum and the last consistent state serves (the old copy-on-write node / the predecessor ledger record). Nothing overwrites live data in place.
- **Whole-transaction atomicity**: one transaction = one checksummed journal entry, replayed all-or-nothing at mount. A transaction is never visible half-applied.
- **No hardware-atomicity dependency**: a file-backed volume gets the same integrity guarantee as an atomic-4KiB device. The sector-atomicity probe still runs, purely informationally, and reports as `meta_volume_atomicity_physical` on the `.stats` inode (`atomic4k` / `likely` / `unknown` / `file-backed`); the contract field `meta_volume_atomicity` reads `cow-checksummed`. (The old `--strict-meta-atomicity` mount gate only ever gated v2 volumes and was deleted with them.)

**Acked durability** (`fsync`/`fsyncdir` returning success) is carried solely by post-apply coalesced `fdatasync` barriers — exactly one physical barrier per fsync.

- `SQUEEZEFS_META_FLUSH_INTERVAL_MS`: deferred metadata durability window in ms (default `50`); `0` = strict sync-on-commit — every metadata commit returns only after a post-apply device barrier. Legacy alias `SQUEEZEFS_JOURNAL_FLUSH_INTERVAL_MS` is honored; the new name wins if both are set.
- `SQUEEZEFS_RECLAIM_BATCH`: inode-reclaim group-commit batch size (default `64`, clamp 1–1024).
- `SQUEEZEFS_META_COMMIT_BATCH_TXS` / `SQUEEZEFS_META_COMMIT_BATCH_BYTES`: per-volume commit-conveyor batch caps (defaults `64` transactions / `256 KiB`; bytes are clamped to the journal ring's admissible capacity). Group commit batches admission, locking, the journal write, and the barrier across concurrent transactions — **never the atomicity unit**: one transaction stays one checksummed journal entry (design `docs/design-metadata-throughput.md` §5.5). Watch `meta_commit_group_size` on the `.stats` inode; a strict-mode median ≈ 1 under concurrent writers means batching regressed.
- `SQUEEZEFS_OP_PROFILE=1`: per-op FUSE phase histograms (`fuse_op_phase_ns`, `fuse_create_under_lock_ns`) on the `.stats` inode — diagnostics for metadata-latency attribution. Off by default; zero per-op cost when off.

### Single-writer mount guard (guarantee classes)

The v3 metadata engine is single-writer by construction, and the mount enforces it: every **write** mount claims each metadata volume with (a) a dedicated daemon-lifetime `flock` (same-host exclusivity; the kernel releases it instantly on process death), (b) an **NVMe Persistent Reservation** (Write Exclusive) where the namespace advertises reservation support — cross-host *enforcement*: the device itself rejects a fenced or stale holder's writes — and (c) a `writer_claim` heartbeat record (identity + detection on every substrate). A second concurrent mount is **refused loudly, naming the holder**. There is **no bypass flag**; read-only probes (`status`, format preflight) are never blocked. Design: `docs/design-metadata-throughput.md` §5.0. What the guard guarantees depends on the substrate:

| Substrate | Guarantee |
|---|---|
| Same host, any volume | **Refusal-grade** (flock on a dedicated fd; kernel-enforced; instant crash reclaim; SIGSTOP-safe) |
| NVMe / NVMe-oF namespace with `RESCAP` PR support | **Enforcement-grade** (Write-Exclusive reservation: the device rejects a fenced/stale holder's writes; acquire arbitrates simultaneous mounts; automatic TTL-stale preemption is safe). **Fencing detection latency ≤ one flush cadence + one barrier** (50 ms default; immediate in strict/fsync — Issue 14); PTPL-lapse residual ≤ 10 s (heartbeat report re-check, §5.0 B1 pt 6). Crash (kill -9) remount recovery is portable across Register semantics: on **spec-strict** targets (SPDK v26.05 and current kernel nvmet, both measured 2026-07-17) the guard's **register ladder** proves the conflicting registration is its own dead incarnation's — via the association's device-reported host identifier — and unregisters exactly that key before re-registering; foreign registrations are never touched (preempt/TTL/`claim clear` territory) |
| — SPDK-served namespace (`nvmeof share` — the **default stack**; lifecycle + sharing fully live as of milestone N4) | **Enforcement-grade, measured** (2026-07-17 rig: mount `flock+pr`, fencing EBADE class, preempt, ladder crash-remount ×10; re-proven 2026-07-18 through the product's own verbs — N4 gate) — and reservations **persist through target restarts** (PTPL; a live holder rides out `spdk_tgt` kill + `target start`/`load_config` with `writer_guard_fenced=0` and `writer_guard_pr_reacquires=0`). Every product SPDK share pins `nsid` + ns UUID + `ptpl_file` (`<state>/spdk/ptpl/<uuid>.json`), and `restore` re-presents the recorded identity — PTPL state re-binds across re-creates by construction |
| — loop-device-backed nvmet namespace (the repo's own file-backed share path, `losetup` wrap) | loop devices expose no PR ⇒ lands in the **"block without PR"** row below — named explicitly because the repo's own tooling creates this shape |
| Block volume **without** PR support | **Detection-grade**: mounts separated by > ~1 heartbeat are refused; near-simultaneous mounts can both arm; a paused holder cannot detect usurpation — therefore automatic cross-host takeover is disabled (operator-attested `claim clear` only) |
| File-backed volume shared cross-host (NFS et al.), or containers with private `/dev` nodes | **Unsupported for concurrent-mount protection** — single-host operation of such volumes remains fully guarded by flock (former) / PR-if-available (latter) |

**Recovery runbook**, in order of automation — the refusal message always names the holder (`{id, pid, boot, age}`) and the exact remedy:

1. **Same-host crash**: nothing to do — the flock died with the process, and a dead-pid-proven claim (same boot, `kill(pid,0)` = ESRCH) is reclaimed automatically and instantly.
2. **PR-capable volumes**: a TTL-stale holder (> 45 s without heartbeat) is **preempted automatically** at the device; a fresh holder refuses loudly.
3. **Non-PR volumes after a cross-host crash**: automatic takeover is deliberately disabled (a paused holder cannot detect usurpation). Verify the named holder is truly dead, then clear the stale claim by operator attestation:

   ```bash
   squeezefs claim clear sqmeta://<meta_dev>
   ```

   The verb probe-mounts read-only, re-verifies staleness (refusing a fresh claim), and removes the record — the same live-check style as the format preflight.

**Fabric host identity (normative).** `/etc/nvme/hostnqn` and `/etc/nvme/hostid` are the **connect-time identity inputs**: created-if-missing by the `nvmeof connect` path, passed to nvme-cli and the `/dev/nvme-fabrics` fallback string, and read by the guard's `host_identity()`. They are **never the match authority**: when the register ladder must decide whether an existing PR registration is its own dead incarnation's, it matches on **`wire_host_id()`** — the host identifier the device itself reports for the live association (Get-Features FID 0x81) — because the wire identity was **measured diverging from the `/etc/nvme` files** on a real box (S1 session, 2026-07-17). Practical consequences: editing `/etc/nvme/hostid` changes what future connects present, not what the guard matches against; and only same-host stale keys (proven via the wire identity) are ever unregistered — foreign registrations always stay preempt/TTL/`claim clear` territory.

Live signals on the `.stats` inode: `writer_guard_mode` per volume (`flock+pr` = enforcement-grade | `flock+claim` = detection-grade | `flock` = read-only mount) — alert on fleet drift; `writer_guard_fenced` (a fenced/usurped holder fail-stopped — working as designed, always investigate); `writer_guard_pr_reacquires` (the target dropped reservations, e.g. a PTPL-less power cycle — audit the fabric).

### Format v3 (CoW KV metadata)

Metadata volumes format as **v3**: a copy-on-write, typed key/value btree (bcachefs-style 256 KiB CoW nodes + a logical reservation journal + background checkpoints). Full design: `docs/design-cow-kv-metadata.md`; measured gates: `.benchmarks/2026-07-09-kv-v3-gates.md`.

Capacity/scale: ≥ 100 M inodes per volume, 1 M+ entries per directory, unlimited xattrs (values up to `min(64 KiB, node_size/4)`), and O(active-set) mount time (a 100 M-inode volume cold-mounts in ~22 ms on the reference box).

**v3 tuning knobs** (format-time and mount-env):

- `--meta-node-kib <64|128|256|512|1024>` (format): btree node size, default `256`. Below 256 the per-volume record-value cap becomes `node_size/4` and a warning prints, spilling large xattrs / layout maps to the indirect mechanism sooner — leave at 256 unless cold-read latency on tiny-record workloads dominates.
- `--meta-journal-mb <MiB>` (format): journal ring size; default `clamp(volume/64, 8 MiB, 32 MiB)`.
- `SQUEEZEFS_META_NODE_CACHE_MB` (mount env): RAM budget for the demand-paged node cache (default `512`).
- `SQUEEZEFS_META_CHECKPOINT_MAX_DIRTY_NODES` (mount env): dirty-node checkpoint cap; bounds the mount-replay working set (default `4096`).
- `SQUEEZEFS_META_FLUSH_INTERVAL_MS` (mount env): the journal/checkpoint cadence — `0` = strict per-commit durability.

> **Legacy format v2**: support was removed entirely (always forward — no backwards compatibility). A v2 superblock refuses to mount with a precise "no longer supported; reformat required" error; `squeezefs format --force` reformats such a volume to v3 (destroying the old contents). The offline `squeezefs migrate` v2→v3 converter was deleted along with v2 support.

## Breaking changes & migration notes

SqueezeFS moves **always forward** — no backwards compatibility. Refusals are loud, name their cause, and state the remedy. Current refusal classes an operator can hit:

> **⚠️ Legacy metadata format v2 — removed.** A v2 superblock refuses to mount with *"no longer supported; reformat required"*. Reformat to v3 with `squeezefs format --force` (destroys the old contents). The offline `squeezefs migrate` v2→v3 converter was deleted along with v2 support.

> **⚠️ Pre-watermark v3 volumes — refused (REFORMAT REQUIRED).** v3 volumes formatted before the node-seq mint watermark (the Finding-A KV-corruption fix era) fail the superblock feature gate: *"pre-watermark v3 volume: formatted before the node-seq mint watermark (Finding A) and no longer supported; reformat required"*. Volumes carrying **unknown** incompat bits (formatted by a newer binary) also refuse, naming the bits — upgrade squeezefs instead.

> **⚠️ Pre-fix compressed/encrypted volumes — refused (REFORMAT REQUIRED).** Volumes formatted with `--compression`/`--encrypt-algo` before the FIND-RW4-A incompressible-block fix cannot hold worst-case stored images; mounts refuse with *"compressed/encrypted volume geometry cannot hold incompressible blocks (FIND-RW4-A) … refusing to mount"* (full-size incompressible blocks on such volumes were never readable — the refusal names the fix). Reformat with a current binary: `format` now reserves per-chunk headroom on transformed volumes (clamping the block size loudly when needed), and compression became **best-effort per block** — incompressible blocks are stored raw (`compress_stored_raw` counts them in `.stats`).

> **⚠️ Staging directories are generation-bound.** Staging/cache dirs are stamped with the filesystem generation (the v3 superblock uuid set). A mount that finds staged content from a **dead generation** (e.g. after a reformat over live staging dirs) wipes it with one loud `STAGING GENERATION MISMATCH` line and counts `staging_generation_discards` in `.stats` — staged writes stamped by the old generation are gone **by design** (reformat discards data).

> **⚠️ Cache/staging paths are format-declared.** `mount --disk-cache-paths` is refused loudly (never silently ignored). Change paths with the admin op `squeezefs config set-cache-paths <sqmeta-uri> <paths...>` (guarded like `format`: refused while any client has the volume mounted; the new dirs are wiped so the next mount stamps a fresh staging generation). Read them back with `config get-cache-paths`. A filesystem formatted without `--disk-cache-paths` is **permanently cache-less**.

## Removed verbs & flags

Kept here so stale scripts fail comprehensibly:

> **Removed flags/verbs**: `--strict-meta-atomicity` (only ever gated v2 volumes; deleted with them), `squeezefs migrate` (deleted with v2), `mount --local-ips` (the socket-level multi-rail bonding was removed in the 2026-07-04 connection simplification — fabric multipath is the kernel NVMe initiator's domain), `mount --disk-cache-paths` (see [Breaking changes](#breaking-changes--migration-notes)), the `config data-volume add/remove/migrate` / `config metadata-volume add/remove/migrate` / `config … fsck` fake admin verbs (deleted 2026-07-19 in the volume-lifecycle program's honesty cleanup — they reported success without doing the work; their real successors are `squeezefs volume …` and `squeezefs fsck`, below). `squeezefs defrag` was removed 2026-07-17 for the same honesty reason and **returned as a real implementation in the 2026-07 volume-lifecycle program** — see [Volume lifecycle & online maintenance](#volume-lifecycle--online-maintenance).
>
> **NVMe-oF verb migration (2026-07-17, target-management program PR 2/N2** — `docs/design-nvmeof-target-management.md` §API): the whole `squeezefs storage nvmeof <verb>` surface **moved to the top-level `squeezefs nvmeof <verb>`**, and within it: `share --spdk`/`unshare --spdk` → `--target-stack {spdk|nvmet}` (default spdk; `unshare` now resolves the stack from the share ledger, never a flag); `restore-shares` → `restore` (and it works — the old registry truncated itself to `[]` on every root invocation, so share persistence had **never** worked; a pre-existing `/etc/squeezefs/nvmeof_shares.json` is retired to `.retired-by-rebuild` on the first mutating verb, and pre-rebuild live shares surface in `list` as foreign/unmanaged — as of milestone **N4b** the managed exit is **`squeezefs nvmeof adopt <subnqn>`**, which absorbs the live share into the ledger with `adopted_from: pre-rebuild` provenance and zero serving interruption; manual removal-first + re-share remains the documented fallback for shapes adopt refuses); `spdk-install`/`spdk-setup`/`spdk-start` → `nvmeof target install/setup/start` (live as of milestone N3, joined by the new `target stop`/`status`/`systemd-unit`; SPDK *sharing* went live with milestone **N4** — the default stack shares for real, and the interim loud-fail message is gone); `spdk-bind`/`spdk-unbind` **deleted** (PCIe vfio passthrough backing is a future program — v1 serves kernel block nodes and files, `bdev_aio` on the SPDK stack); share's silent 1 GiB sparse auto-create on a missing path **deleted** (refuse loud; `--create-size <sz>` is the explicit opt-in).

## Configuration reference

### SqueezeFS URI scheme

To centralize block storage connectivity, SqueezeFS utilizes two connection URIs:

* **Metadata Volumes**: `sqmeta://<path_to_block_device_or_file>` (e.g. `sqmeta://dev/xai-meta/mds01`).
* **Data Volumes**: `sqdata://<path_to_block_device_or_file>` (e.g. `sqdata://dev/xai-data/oss01`).

### Format (`squeezefs format`)

Initialize physical block maps and metadata. New metadata volumes are formatted as **v3** (CoW KV metadata — see [Format v3](#format-v3-cow-kv-metadata) and [Metadata Durability](#metadata-durability-crash-contract)). Executes concurrently across all target devices.

```bash
squeezefs format sqmeta://<meta_dev> [sqmeta://...] sqdata://<data_dev> [sqdata://...] [options]
```

*Options:*
- `--block-size <bytes>`: Block size in bytes (e.g. `4M`, `1M`, default: `4M`). On compressed/encrypted volumes the effective block size is clamped so a worst-case (incompressible) stored image plus headroom fits its allocator chunk — the clamp prints loudly.
- `--capacity <bytes>`: Formatted capacity (default: the summed physical size of the data volumes). May be **lower** than physical (useful for testing); values above physical are refused — thin-provision underneath via LVM/fabric instead.
- `--inodes <count>`: Hard quota limit for number of inodes (default: `1000000`).
- `--disk-cache-paths <paths>`: Comma-separated paths to NVMe cache staging directories. **Declared here, at format** — recorded in the format config as the single source of truth. Omit it and the filesystem is **permanently cache-less**: mounts run with RAM tiers + direct block I/O only (no NVMe staging/read-cache tier). Change later with `squeezefs config set-cache-paths`.
- `--compression <lz4|zstd|none>` / `--encrypt-algo <aes256gcm-rsa|chacha20-rsa|none>` / `--encrypt-key <pem>`: transparent per-volume compression / client-side encryption (see [Transparent compression & encryption](#transparent-compression--encryption)).
- `--mem-cache-size` / `--disk-cache-size` / `--{read,write}-cache-size` / `--{read,write}-mem-cache-size`: cache budget defaults recorded in the format config (overridable per mount).
- `-f, --force`: Force formatting even if a squeezefs volume is already detected (this is also the reformat path for refused legacy volumes — destroys old contents).
- `--full`: Performs full block-aligned zero-wiping of the backing device capacity with a progress bar (default is quick-format).
- `--meta-node-kib <64|128|256|512|1024>`: v3 metadata btree node size in KiB (default `256`). Below `256` prints a warning — the per-volume record-value cap drops to `node_size/4`, so large xattrs / layout maps spill to the indirect mechanism sooner.
- `--meta-journal-mb <MiB>`: v3 metadata journal ring size, overriding the default `clamp(volume/64, 8 MiB, 32 MiB)`.

### Mount (`squeezefs mount`)

```bash
squeezefs mount sqmeta://<meta_dev> [sqmeta://...] <mountpoint> [options]
```

Cache/staging paths come from the format config; passing `--disk-cache-paths` at mount is a loud error (use `squeezefs config set-cache-paths` to change them).

*Options (operator-relevant subset; `squeezefs mount --help` is authoritative):*
- `--daemon`: Run FUSE daemon in the background (changes its working directory to `/` to avoid locking paths).
- `--supervise` (requires `--daemon`): keep the parent alive as an external mount watchdog — see [External mount supervisor](#external-mount-supervisor-mount---daemon---supervise).
- `--allow-other` (alias `--allow-others`): Allow other users/root to access the mount (required for `sudo umount`).
- `--log-file <path>`: Path to write daemon logs to when running in background.
- `--mem-budget <size>`: the daemon's joint memory budget (shed-don't-OOM authority) — see [Hybrid I/O](#hybrid-io-for-o_direct-reads-default-and-the-device-true-escape).
- `--mem-cache-size <size>`: System RAM cache size (e.g. `16GB` or `20%`); the other cache-size family flags override the format-config defaults the same way.
- `--uid <uid>` / `--gid <gid>`: presented owner of files in the mount (presentation-only; staging I/O runs as the mounting user).
- `-o <opts>`: FUSE options, including the per-class kernel TTLs (`attr_timeout`, `entry_timeout`, `dir_entry_timeout`, `negative_timeout`), `max_background` / `congestion_threshold` INIT overrides, and `direct_device_true` — each documented in its section below.
- `--no-writeback`: disable the FUSE writeback cache (enabled by default).
- `--interception` (= `-o interception` = `SQUEEZEFS_IPC=1`): arm the L4 LD_PRELOAD interception session host for this mount — see [LD_PRELOAD interception](#ld_preload-interception--o-interception--security-posture--unsupported-mixes).
- `--write-verification` (+ `--write-verification-sample <N>`): opt-in read-after-write checksum verification.
- `--dismount-wait <secs>` / `--upload-delay <dur>`: staging drain window on dismount / background upload cadence.

### LD_PRELOAD interception (`-o interception`) — security posture & unsupported mixes

*Hands-on benchmarking walkthrough (build → mount → run → verify engagement): `QUICKSTART.md` → "Benchmarking the LD_PRELOAD Interception Path (Manual)". Measured reference numbers: `.benchmarks/2026-07-19-l4-interception-closing.md`.*

Opt-in at **both** ends (`docs/design-preload-interception.md`, v1 posture): the mount arms the session host (`--interception` / `-o interception` / `SQUEEZEFS_IPC=1`), and each app opts in with `LD_PRELOAD=libsqueezefs_il.so` (built with `cargo build -p squeezefs-preload --profile preload-release --features interposers` — the ONLY sanctioned build; a plain `--release` build refuses at compile time because the root profile's `panic="abort"` would turn shim panics into host-app aborts). Everything the shim cannot serve identically falls through to the real fd via kernel FUSE — **correctness never depends on interception**. Data ops on bound fds ride a shared-memory ring; warm reads serve synchronously from the daemon tiers (measured ~2× kernel-FUSE warm rand-4k on the dev substrate); everything else rides an async handoff into the exact FUSE handler bodies. The per-PR gate is `tests/run_preload_gate.sh` (leg 1 unprivileged; leg 2 root: parity + engagement + dup/close_range/lseek rows + kill-9 and fork-kill-parent soaks).

**KD-11 — interception forces kernel write-through.** `-o interception` flips the mount to `write_back = false` (the same knob `--no-writeback` drives): the default-on kernel writeback cache acks buffered writes before the daemon sees them, and a ring read (direct-to-daemon by construction) would miss them. Combining `-o interception` with an explicit `writeback`/`writeback_cache` request is a contradiction and **refuses the mount loudly**. Cost: buffered kernel-path small writes on interception mounts lose the kernel's dirty-page batching (interception mounts exist to take the ring path for exactly those writes).

**Security posture (the §5.2 daemon fd screen is the boundary):**
- The bind credential is a **real open fd** passed over an abstract AF_UNIX socket (`SCM_RIGHTS`). The daemon re-derives everything from the received fd itself: `O_PATH` descriptions are refused outright (obtainable with search-only permission — accepting one would grant reads without read permission), non-regular files refuse, `st_dev` must match the mount, and per-op rights come strictly from the description's access mode **in both directions** (an `O_WRONLY` binding cannot ring-read; an `O_RDONLY` binding cannot ring-write — both surface as `EBADF`). `O_APPEND`, `O_SYNC`/`O_DSYNC`, and `O_TMPFILE`-class (unnamed regular file, `st_nlink == 0` — which also conservatively refuses open-then-unlinked fds; passthrough serves them) refuse at bind.
- **Version lock (forward-only):** sessions bind only between identical builds (`build_commit` equality + `IPC_ABI`). Degenerate identities — `unknown` (no-git tarball) or `-dirty` — refuse on *either* side; `SQUEEZEFS_IPC_ALLOW_DEV=1` is the dev-box override, **counted** in `.stats` `ipc_binds_dev_override` (nonzero outside dev boxes is a fleet-hygiene alarm).
- **Multi-user (`--allow-other`) posture:** any uid that can open files on the mount can establish sessions. Sessions are per-process, arenas are private mappings (no cross-process payload visibility), per-uid session caps and the R5 `ipc_session_arenas` budget component bound resource use (shed = refuse-new-sessions, never tearing live ones). The trust model is exactly POSIX-fd trust plus resource caps. `SO_PEERCRED` labels accounting and is defense-in-depth, not the authorizer.
- **Observability:** `.stats` carries the refusal ledger (`ipc_bind_refused_{version,nonce,flags,mode,budget,peercred}`), lifecycle gauges (`ipc_sessions_{active,total}`, `ipc_arena_bytes`, `ipc_binds`, `ipc_admission_refusals`) and two **must-stay-0 tripwires**: `ipc_descriptor_rejects` and `ipc_sessions_poisoned` — nonzero means a client bug or an attack (one loud log line per event).

**Unsupported mixes (documented contract, not detected):**
- **Concurrent cross-process `MAP_SHARED` mmap-writers + ring writers on the same file** — page-granularity writeback can clobber ring-written bytes (lost updates). Same-process mmap is handled: the shim unbinds *all* in-process bindings on the mapped inode. Cross-process is declared unsupported; run such workloads without the shim.
- **Cross-process buffered/mmap readers** can observe a bounded staleness window on ring-written data (same class as attr-TTL staleness); the daemon's `notify_inval_inode` handoff bounds it — fired on bind and rate-limited per `(ino, window)` on ring writes (`SQUEEZEFS_IPC_INVAL_WINDOW_MS`, default 1000; `.stats` `ipc_inval_{notifies,suppressed}`), delivered over the classical sideband even on armed over-uring sessions.
- **Mixed-ABI fd lifecycles**: apps that close *and* recreate fds exclusively through raw `syscall(2)`/io_uring (invisible to the shim) and then issue libc data calls on the reused number are unsupported under the shim (`SQUEEZEFS_IL_PARANOID_FSTAT=1` is the triage knob).
- **Containers with their own network namespace** *(solved in v1.1 — OQ-6)*: the abstract-socket rendezvous is per-netns, so pre-v1.1 such apps silently stayed on kernel FUSE. Since v1.1 the daemon **also binds a filesystem-path ctl socket** and advertises it in the bootstrap blob; the shim's connect ladder tries abstract first (same-netns fast path), then the path. **Operator contract for container fleets:** bind-mount the socket runtime dir into the container alongside the filesystem — default `/run/squeezefs` (root mounts) or `$XDG_RUNTIME_DIR/squeezefs`, else `/tmp/squeezefs-il-<uid>` (user mounts); override with `SQUEEZEFS_IPC_SOCKET_DIR=<dir>` (`none` disables, restoring the v1 zero-residue posture). The socket file is mode 0666 **because connecting is not a credential** — `SO_PEERCRED` + the daemon fd screen remain the security boundary, identical over both rendezvous. Note the user-mount default under `$XDG_RUNTIME_DIR` is a 0700 dir: other uids cannot reach it (user mounts serve same-uid apps; point `SQUEEZEFS_IPC_SOCKET_DIR` at a shared dir if you need more). A failed path bind degrades loudly to abstract-only and never fails the mount; the file is unlinked at daemon shutdown (zero residue restored), and a same-name stale file from a crash is replaced at the next spawn (names embed pid+random, so a collision is always our own residue).

*Env knobs:* `SQUEEZEFS_IPC=1` (arm), `SQUEEZEFS_IPC_ARENA_MB` (per-session payload arena, default 64), `SQUEEZEFS_IPC_MAX_OP_BYTES` (default 1 MiB), `SQUEEZEFS_IPC_MEM_PCT` (session-shm admission cap as a percent of the resolved memory budget, clamp (0,100] — the preferred spelling), `SQUEEZEFS_IPC_MEM_MAX` (MiB; absolute session-shm admission cap, explicit-wins-verbatim — compat spelling), default with neither set = 12.5 % of the budget (`budget/8`, no fixed ceiling; precedence absolute > percentage > derived), `SQUEEZEFS_IPC_ALLOW_DEV=1` (counted dev-build skew override), `SQUEEZEFS_IPC_SERVICE_THREADS` (pinned service threads, default 2), `SQUEEZEFS_IPC_IDLE_SECS` (idle-session reap, default 300, 0 = off; `.stats` `ipc_sessions_reaped`), `SQUEEZEFS_IPC_INVAL_WINDOW_MS` (W1 invalidation rate window, default 1000), `SQUEEZEFS_IPC_SOCKET_DIR` (path-socket runtime dir, `none` = abstract-only — see the container-netns entry above). Client side: `SQUEEZEFS_IL_OP_TIMEOUT_MS` (per-op ring deadline; timeout poisons the session → passthrough).

*Data-plane observability:* `ipc_ops_{read,write}` / `ipc_bytes_{in,out}` are the **engagement instrument** — an interception benchmark row is only valid if their deltas account for the row's ops (silent passthrough measuring kernel FUSE is the failure mode the check exists for); `ipc_fast_path_serves` vs `ipc_async_handoffs` + the `ipc_fast_path_{lock,miss}_demotions` split are the fast-path health signal.

### Cache/staging paths (`squeezefs config`)

Changing cache/staging directories is an admin op, guarded like `format` (refused while any client has the volume mounted); it rewrites the format config and wipes the new directories so the next mount stamps a fresh staging generation.

```bash
squeezefs config set-cache-paths sqmeta://<meta_dev> <path> [<path>...]
squeezefs config get-cache-paths sqmeta://<meta_dev>
```

### Transparent compression & encryption

Optional per-volume transforms declared at format: `--compression lz4|zstd` and `--encrypt-algo aes256gcm-rsa|chacha20-rsa` (RSA-wrapped symmetric keys via `--encrypt-key`), applied across all three write layouts. Compression is **best-effort per block**: an incompressible block is stored raw (frame-flagged, counted as `compress_stored_raw` in `.stats`) instead of expanding — and transformed volumes reserve per-chunk headroom at format so worst-case images always fit (see [Breaking changes](#breaking-changes--migration-notes)).

### Read-path tuning (mount env; design `docs/design-read-path.md`)

Defaults are the measured sweet spot — override only with a live-counter reason (the `.stats` inode exposes every family):

- `SQUEEZEFS_READ_TIER_ADMISSION` (`second-touch` default | `always` | `never`): NVMe read-tier admission for >256 KiB fills. `second-touch` kills the streaming publish tax (a cold 16 GiB pass writes ~0 instead of ~16.9 GiB to the tier) while re-read heat still converges to the tier; `always` restores unconditional first-touch publishes (A/B escape hatch).
- `SQUEEZEFS_READ_HOT_BLOCK_CACHE_MB`: RAM hot-block tier budget for >256 KiB blocks (default derived from the read-mem cache; `0` disables the tier and admission auto-degrades to `always`).
- `SQUEEZEFS_READ_PREFETCH_WINDOW` (default `16`, `0` disables): per-stream prefetch pipeline depth cap in blocks. The window is adaptive (2→cap, AIMD) and contention-scaled; the cap is a ceiling, not a target.
- `SQUEEZEFS_READ_PREFETCH_SHARE_PCT` (default `50`): the prefetch pipeline's share of the hot-tier budget in the contention-scaling formula — lower it if concurrent stream count routinely exceeds hot-tier capacity.
- `SQUEEZEFS_READ_RANGED_THRESHOLD` (default `262144`, `0` disables): reads at or under this size on passthrough (uncompressed/unencrypted) volumes fetch only their 4 KiB-aligned device window instead of the whole block — the rand-4k amplification kill (≈1000× → ~1.0×). Compressed/encrypted volumes always fetch whole blocks (decode requirement).

### Hybrid I/O for O_DIRECT reads (default) and the device-true escape

**Hybrid I/O (default, user directive 2026-07-15):** O_DIRECT reads get the best of both worlds — they keep bypassing the *kernel page cache* (the kernel's side of O_DIRECT, unchanged) while serving from and admitting into *SqueezeFS's own read tiers* exactly like buffered reads. Tier hits serve from RAM (binding-validated — the ~536–558 k IOPS class on tier-resident data, `.benchmarks/2026-07-15-hybrid-io.md` + the RW5 close §3b); misses use **evidence-based admission** — first touch of a block reads the device (device-true, nothing admitted: streaming/scan pollution protection), a **second touch within the ghost window** admits the block (one whole-block fetch → RAM hot tier + NVMe read tier), so re-read-heavy O_DIRECT workloads (rand-4k databases, repeated scans) converge to RAM speed after one warm-up pass. Admission pauses under memory-budget Red. Watch `read_odirect_tier_serves` / `ranged_read_ghost_escalations` in `.stats`.

- **`-o direct_device_true`** (mount option) / **`SQUEEZEFS_DIRECT_DEVICE_TRUE=1`** (daemon env): the **measurement/diagnostic escape** — O_DIRECT reads become strictly device-true (no tier serve, no admission, no ghost recording, no prefetch classification; every O_DIRECT read is a validated device read of exactly its aligned window). This is the posture for device-path benchmarking and the `.benchmarks` amplification methodology (`squeezefs bench --direct` prints which posture the mount carries by sniffing `.stats`). Buffered traffic on the same mount keeps full hybrid behavior. Mode visible as `"direct_device_true"` in `.stats`; adoption counted by `read_device_true_reads`.
- `--mem-budget <size>` (mount flag) / `SQUEEZEFS_MEM_BUDGET_MB`: the daemon's joint memory budget. Unset, the budget follows cgroup v2 `memory.max` × 0.8 (re-read every second — a runtime-lowered cage tightens the budget live), else 70 % of RAM. Under pressure the daemon sheds (early flushes, cache clamps, prefetch pause) instead of OOMing; watch `mem_budget_level`/`mem_budget_red_events` in `.stats`.

### Random-small-write path (sole-owner patch + extent overlay; design `docs/design-random-small-writes.md`)

Small random overwrites of striped files no longer pay a whole-block read-modify-write. Two levers, both default-on (program Implemented 2026-07; closing evidence `.benchmarks/2026-07-17-rand-write-program-closing.md`):

- **Sole-owner extent patch (W1)**: an isolated, LBA-aligned, non-extending small write to an exclusively-owned, passthrough, whole-block-mapped striped block becomes **one in-place sub-block DMA** — zero reads, zero metadata commits, zero staging (354–397 → 61–67 k IOPS on the 4 KiB random-write shape; device amplification ~1× writes). Sequential streams are predicate-excluded (adjacency guard) and keep the whole-block write-through economy.
- **Extent overlay + batched fold (W2)**: patch-ineligible shapes (compressed/encrypted volumes, refcount-shared blocks post-clone, holes, unaligned) park 4 KiB-class extents instead of 4 MiB buffers, spill as checksummed staging *extent records* (never a seed read at spill), and fold into blocks lazily — compressed-volume rand-write amplification ~2,500× → 15–26×.
- **Torn-extent durability note (the v1 aligned-only contract)**: a patch rewrites **only device sectors wholly inside the application's own write range** — bytes the application never wrote are never rewritten, so a crash can never perturb foreign data. The residual exposure is a per-sector old/new mix *strictly inside an un-fsynced in-flight write* (POSIX-legal; fsync acks only after DMA completion — the write ACK on this shape is *stronger* than before, since data reaches the device before ACK instead of a parked buffer).
- Knobs (acceptance/diagnostic, not operational escape hatches): `SQUEEZEFS_PATCH_MAX_BYTES` (default 512 KiB; `0` disables the patch path — A/B lever), `SQUEEZEFS_FOLD_MAX_EXTENTS` / `SQUEEZEFS_FOLD_MAX_BYTES` (fold triggers, default 64 / 1 MiB).
- Watch in `.stats`: `patch_writes` ≈ ops on the patch shape (`patch_ineligible_*` growing there = predicate rot), `patch_edge_rmw_reads` **must stay 0**, `fold_fill` median ≥ 16, `extent_records_{recovered,torn_discarded,future_refused}` on recovery.

### FUSE transport in-flight concurrency (defaults are the L1 policy; knobs are overrides)

Random-4k iodepth workloads are gated by two multiplicative kernel-side limits: the FUSE-over-io_uring per-queue ring depth and the INIT-negotiated `max_background`. Opening both measured **44k → 316k IOPS (7.2×, device-true)** on `elbencho --rand -t 16 -b 4k --iodepth 16 --direct` (`.benchmarks/2026-07-15-iops-parity-decomposition.md`); since L1 that class is the **default** — no knobs required:

- **Per-queue depth** (`SQUEEZEFS_FUSE_OVER_IO_URING_Q_DEPTH`, clamp 1..32): default is **32 degraded to the payload-buffer cap** `min(mem-budget/8, 2 GiB)` with floor 4 (the pre-L1 posture — small-RAM boxes keep yesterday's footprint). An explicit value wins verbatim over the cap. Payload arenas cost `queues × depth × ~1 MiB` of registered anon memory — gauged as `transport_payload_buffer_bytes` in `.stats` and attributed to the memory budget as the `transport_payload_buffers` component.
- **Queues** (`SQUEEZEFS_FUSE_OVER_IO_URING_QUEUES`, testing only): pinned to kernel **possible CPUs** — registering fewer never becomes ready (kernel readiness requirement).
- **`-o max_background=N` / `-o congestion_threshold=N`**: INIT-reply overrides; defaults `clamp(queues × depth, 64, 256)` and ¾ of it. Also runtime-writable per live connection via fusectl: `echo 256 | sudo tee /sys/fs/fuse/connections/<minor>/max_background`.
- **FIND-L1-A (≥ 13-writer O_DIRECT convoy): FIXED 2026-07-17.** The convoy was a write-path completion-trigger defect (one write's end as a proxy for block completeness — kernel-split out-of-order WRITE segments misfired it), not a transport trade; the coverage-union trigger cured it (`t16` default/mb12 = 1.006–1.026, t16 ≥ 1.10× t8, both cells *rose*). No `max_background` throttle is needed or recommended anymore. Forensics + fix: `.benchmarks/2026-07-17-rw3-find-l1a-forensics.md`, `.benchmarks/2026-07-17-rw3b-write-through-coverage-fix.md`.

### FUSE io_uring SQPOLL (mount env; measured — leave unset)

- `SQUEEZEFS_FUSE_IO_URING_SQPOLL_IDLE_MS` (default unset = **off**): opts every FUSE-side io_uring into kernel submission-queue polling with the given idle timeout — the classical `/dev/fuse` INIT/notify/sideband rings (one poller each) **and** the FUSE-over-io_uring queue rings, which share **one** poller for all queues (qid 0 creates it, the rest attach via `IORING_SETUP_ATTACH_WQ`; a kernel that declines SQPOLL degrades loudly to plain rings, never failing the mount).
- `SQUEEZEFS_FUSE_IO_URING_SQPOLL_CPU`: pin that one queue-ring poller (`IORING_SETUP_SQ_AFF`; the leader's pin governs the shared group — per-queue pins do not exist by design).
- **Measured posture (2026-07-15, `.benchmarks/2026-07-15-m10-sqpoll.md`, resolves design OQ 3): not recommended — including for dedicated metadata-heavy nodes.** On the post-M3 transport (one `io_uring_enter` already carries commit+wait), SQPOLL-on measured **+25 % enters/create** (wake-cycle fragmentation; 9.10 → 11.40), **one full core burned by the poller under storm** (idle mounts burn 0.0 % — the idle timeout parks it), and **flat-to-worse paired mdstorm rows** (−1.5 % create … −12.9 % many-dirs unlink) at byte-identical op shape. Consider only on boxes with uncontended spare cores, and only if a live profile of *your* workload (strace `io_uring_enter` counts + `iou-sqp` thread CPU, the M10 method) proves it out.

### Kernel cache TTLs (mount options / env; per-class)

Four kernel-cache TTL classes, each defaulting to the historical 1 s (the DAOS per-class split: directory dentries invalidate whole subtrees, so they get their own knob). Mount options are libfuse-style float seconds (`-o attr_timeout=2.5`) and win over the env knobs (milliseconds); both are per-mount. Longer TTLs widen the staleness window a single mount can observe of its own metadata — safe under the single-writer mount guard; revisit before any multi-writer future.

- `-o attr_timeout=<s>` / `SQUEEZEFS_FUSE_ATTR_TTL_MS`: GETATTR/SETATTR reply TTL + the daemon attr-cache freshness window.
- `-o entry_timeout=<s>` / `SQUEEZEFS_FUSE_ENTRY_TTL_MS`: dentry TTL for non-directory lookup/create results.
- `-o dir_entry_timeout=<s>` / `SQUEEZEFS_FUSE_DIR_ENTRY_TTL_MS`: dentry TTL for directory results.
- `-o negative_timeout=<s>` / `SQUEEZEFS_FUSE_NEGATIVE_TTL_MS`: TTL for cacheable negative lookup replies (kernel-side negative dentries — repeated misses of the same name stop paying a round trip). `0` disables negative caching (misses reply bare ENOENT).

### External mount supervisor (`mount --daemon --supervise`)

With `--supervise` the `mount --daemon` parent stays alive as an external watchdog (JuiceFS-supervisor precedent): it probes `<mountpoint>/.stats` every 5 s (`SQUEEZEFS_SUPERVISE_INTERVAL_SECS`) and, after 30 s of sustained unresponsiveness (`SQUEEZEFS_SUPERVISE_UNRESPONSIVE_SECS`), logs loudly, dumps the daemon's `/proc` state (per-task wchan + kernel stacks when root), and — when kernel callers are blocked (`waiting > 0`) and the supervisor runs as root — writes `/sys/fs/fuse/connections/<id>/abort` to release them with `ECONNABORTED`. The abort kills the mount by design; the escalation message prints the daemon PID and the exact manual recovery commands (kill-by-PID → `squeezefs umount` → remount). This complements the in-daemon op watchdog, which can log a wedge but cannot clear one.

### Host auto-tuning (`squeezefs tune`)

Built-in host auto-tuning (`squeezefs tune`, requires root) optimizes virtual memory dirty page ratios (40/10), network socket buffer maxima (64 MiB), and live FUSE connection limits (`max_background`/`congestion_threshold` to the 256/192 policy ceiling, `read_ahead_kb` to 0). See [Kernel Tuning](../QUICKSTART.md#6-kernel-tuning-for-bare-metal-auto-tune).

```bash
squeezefs tune
```

### Other verbs

* **Unmount** — safely unmounts SqueezeFS by waiting for staging caches to flush before tearing down FUSE:
  ```bash
  squeezefs umount <mountpoint> [--force]
  ```
* **Instant metadata clone (CoW)**:
  ```bash
  squeezefs clone <src> <dest>
  ```
* **Storage pools & volumes (LVM)**:
  ```bash
  squeezefs storage pool create <name> <disks...>     # + add/remove/delete/list
  squeezefs storage volume create <pool> <name> --size <sz>   # + extend/delete/list
  ```

## Volume lifecycle & online maintenance

The 2026-07 volume-lifecycle program (`docs/design-volume-lifecycle.md`; closing record `.benchmarks/2026-07-22-volume-lifecycle-closing.md`) shipped dynamic volume membership, online fsck with repair, and a real defragmenter. Everything long-running executes as a **maintenance job**: durable (survives crashes and remounts; resumes by re-run or `job resume`), pausable, duty-cycle throttled (`--throttle 1–100`, retunable live), visible to `squeezefs job list` both on the live mount and offline from the volume records.

Every verb takes a **TARGET** that is either a live mountpoint (online — the operation runs on the mounted daemon through its admin lane) or a `sqmeta://` URI (offline — a short-lived coordinator takes the same exclusive writer guard as `format`; refused while anyone has the volume mounted).

### Data volumes

```bash
squeezefs volume list <target>                     # durable ids, states, honest capacity math
squeezefs volume add-data <target> <device>        # online add; auto-schedules a rebalance pass (--no-rebalance opts out)
squeezefs volume remove-data <target> <volume-id>  # preflight -> drain (CoW evacuation) -> retire
squeezefs volume undrain <target> <volume-id>      # cancel an in-flight drain; volume returns to active
```

- **Preflight is honest**: a remove that cannot fit the victim's census on the survivors is refused **with the numbers printed** (needed / available / in-flight transient / headroom). A running drain re-verifies at every checkpoint and self-pauses (`paused-capacity`) instead of running the survivors out of space.
- A **draining** volume is excluded from new placement but **serves reads to completion**. Shared clone blocks move exactly once. Retired volume ids are permanent — they never come back.
- Fill balance is also maintained continuously in the write path (balance-aware placement: emptier healthy volumes attract proportionally more new blocks; health always outranks balance). `defrag --rebalance` runs the bulk pass on demand.

### Metadata volumes

```bash
squeezefs volume add-meta <sqmeta-uri> <new-device> --take-slots <n|list>  # OFFLINE: unmount first
squeezefs volume remove-meta <sqmeta-uri> <victim-device>                  # OFFLINE: unmount first
squeezefs volume migrate-meta-slot <mountpoint> <slot> <volume-index>      # ONLINE background job
squeezefs volume repair-set <sqmeta-uri>                # reconcile membership stamps after a crashed change
```

- Metadata routing granularity: **format anywhere, grow forever, no knobs** (dynamic meta routing, 2026-08-02). Every format freezes the DERIVED virtual width (65536 slots — never chosen) and spreads minting across 64 slots per metadata volume, so any volume's existing metadata is divisible into ≥ 64 movable slices from birth: a single-metadata-volume filesystem grows to two (or two hundred) by `volume add-meta --take-slots …` / `migrate-meta-slot` with no format-time planning. The retired `format --meta-slots` flag is a hard error naming these verbs; volumes formatted under the old frozen-width scheme refuse loud (reformat required — forward-only).
- Membership changes are crash-safe: interrupted `add-meta`/`remove-meta` **re-run with the same arguments and converge**; `repair-set` reconciles the stamps when a crash left them mid-flip. Old binaries refuse lifecycle-marked sets loudly (forward-only).
- Set changes drain local staging first, then rebind the staging generation — durable staged payloads survive the membership change.

### fsck / scrub

```bash
squeezefs fsck <target> [--json] [--throttle N]        # detect: 7 classes, verified findings only, exit != 0 on findings
squeezefs fsck <target> --scrub                        # add the C7 data scrub (AEAD/frame/readability per stored form)
squeezefs fsck <sqmeta-uri> --shards k/N ...           # offline zero-coordination sharding; union with `fsck merge-reports`
squeezefs fsck <target> --repair                       # plan per-class repairs (DRY RUN)
squeezefs fsck <target> --repair --apply               # execute: quarantine-first, idempotent, verified findings only
```

Online fsck runs against the live daemon with **zero false positives by design** (every suspect is verified before it is reported — concurrent writes, drains, and parked work are exempted through the live registries, never guessed at). Repair is dry-run by default, quarantines before every discard (per-run quarantine dir + JSON manifest), and is honest where no redundancy exists: torn nodes and scrub-failed blocks are quarantined and reported, never fabricated. On plain (uncompressed, unencrypted) data the scrub can only verify readability — the report says so (`scrub_readability_only`).

### Defragmentation

```bash
squeezefs defrag <target> --report-only        # measure the four axes; move nothing
squeezefs defrag <target> --data [--volume id] # D1/D2: free-space contiguity + file locality movers
squeezefs defrag <target> --meta               # D4: compact metadata btree nodes
squeezefs defrag <target> --fold               # D3: kick parked/spilled extents through the fold (live mounts only)
squeezefs defrag <target> --rebalance          # bulk placement rebalance across data volumes
```

Fragmentation is four measured axes with live gauges on `.stats` (`frag_d1_contiguity`, `frag_d1_reclaimable_tail`, `frag_d2_locality`, `frag_d3_pressure_bytes`, `frag_d4_dead_bset_ratio`). Movers are safe under concurrent load and honor the same job throttle.

### Jobs & distributed execution

```bash
squeezefs job list <target>          # live or offline probe of the durable job records
squeezefs job status|pause|resume|cancel|throttle <mountpoint> <job-id> [pct]
squeezefs job worker <sqmeta-uri>    # enroll this client as a remote data-plane worker
```

Any client with storage access can enroll as a **remote worker** (`job worker`): it proves storage membership (an enrollment secret readable only with metadata-volume access), leases shards from the coordinator, and executes device-bound work (evacuation copies, scrub reads) under the coordinator's throttle. Safety model: remote workers write **only to coordinator-pre-allocated, unpublished destinations**; publication happens coordinator-side after verification, behind per-shard fencing; an expired lease's destinations are quarantined and never reused. Guarantee ladder for a paused/partitioned ("zombie") worker, strongest first:

| Substrate | Fence for an expired worker | Residual window |
|---|---|---|
| Data namespaces with NVMe PR support | the coordinator holds a Write Exclusive – Registrants Only reservation while remote workers are enrolled and **preempts the expired host's registration** — the device itself rejects that host's resumed DMA (`job_remote_fence_mode = "pr"`) | none — quarantine reclaim is unconditionally safe |
| No PR support | per-batch lease re-validation (a woken zombie aborts before its next batch) + destination quarantine (`job_remote_fence_mode = "deferred-reclaim"`) | a zombie pausing across job end into post-reclaim reallocation — documented residual; keep jobs short on such substrates |

Side effect to know about: while a WERO reservation stands (remote workers enrolled on a PR-capable data namespace), **hosts that are not registered participants are write-blocked by the device** until the job ends. On plaintext (non-TLS) wires the coordinator verify-reads 100 % of remote-written bytes before publishing (≈ 2× read cost on remote-moved data); TLS deployments sample instead.

## Observability

A mounted filesystem exposes live daemon metrics as JSON on the virtual **`.stats`** inode at the mount root (`cat <mountpoint>/.stats`) — the preferred live regression signal (layout mix, cache/tier counters, `meta_kv_*`, `writer_guard_*`, transport geometry, patch/fold ledgers, memory-budget level).

* **Show filesystem status:**
  ```bash
  squeezefs status [sqmeta://<meta_dev> | <mountpoint>]   # config + volume summary (JSON)
  ```
  The report's `"Clients"` array carries the volume's real mount registrations (same records and classification as `squeezefs clients` below). When the backing device is fabric-attached, the report also carries a per-volume `"Fabric"` section (see [Fabric observability](#fabric-observability)).

* **List client mount registrations:**
  Serves the `client:{id}` heartbeat records and the single-writer `writer_claim` recorded on the volume set's root inos — the same records the format preflight and the mount guard consume, under the same staleness law. Read-only probe: works beside a live mount and never perturbs it. States: `live` (fresh heartbeat), `stale` (heartbeat older than the 45 s TTL — crashed or partitioned holder), `dead` (writer claim whose same-host pid is provably gone — reclaimable immediately, no TTL wait).
  ```bash
  squeezefs clients sqmeta://<meta_dev> [--json]
  ```
  ```text
  KIND    ID                                     PID      STATE  AGE   VOLUME
  client  0d3179c8-6a02-4f45-9c11-0c8ad6a0a1b2   731022   live   4s    /dev/xai-meta/mds01
  writer  9c41c2e6-6a4e-4bfb-b41c-2fb1b1f2b7aa   731022   live   4s    /dev/xai-meta/mds01
  2 registration(s): 2 live, 0 stale, 0 dead (reclaimable).
  ```

* **Show space/inode usage (offline/URI query):**
  Answers from the same authoritative sources as the mounted daemon's statfs — formatted capacity/quotas from the format config, allocator-tracked striped-block usage (rebuilt by the same live-inode-tree walk a mount runs), and the v3 monotonic inode watermark — via read-only probes: **no live mount required**, and beside one it reports the durable point-in-time state. Aggregate plus per-volume rows (data volumes: size/allocated; meta volumes: KV heap size/free, next-ino).
  ```bash
  squeezefs df -g sqmeta://<meta_dev> [--json]
  ```
  ```text
  SqueezeFS 'squeezefs' — offline query over 1 meta / 1 data volume(s), durable state
  Data:   capacity 8.00 GiB   used 64.00 MiB (0.8%)   free 7.94 GiB
  Inodes: quota 1000000   used 2   free 999998
  ```
  A **mounted** filesystem also answers plain `df -h <mountpoint>` from the OS (see [`df` / statfs semantics](#df--statfs-semantics)).

### `df` / statfs semantics

A mounted SqueezeFS reports honest, cheap numbers to `statfs(2)` (`df`): **total** is the formatted capacity — the summed data-backend size, or the lower explicit `--capacity` quota chosen at format (the effective limit you experience); **used/free** track the bytes currently allocated on the striped block backends, maintained by the block allocators at alloc/free time (no metadata transactions or device I/O on the statfs path). Tiny inline payloads live in the metadata volume and staged-but-unpromoted small writes in the local NVMe staging dirs, so those transient bytes appear in `df` as their blocks promote via writeback rather than instantaneously; deletes return space after background reclaim completes. Inode columns (`df -i`) report the format inode quota against the v3 monotonic, no-reuse inode watermark — `IFree` is remaining create headroom, and deleting files does not raise it.

The **`squeezefs df`** verb answers the same accounting **offline** — read-only probes over the volume set, no mount required (`squeezefs df -g sqmeta://<meta_dev> [--json]`; see above). It reports the durable point-in-time state: beside a live mount, bytes still in flight through staging/journal deferral appear once durable.

### Fabric observability

Target side — **`nvmeof target status [--json]`** is the health verb (never refuses, even on version drift — it exists to diagnose exactly the drifted target): pinned tag/commit vs the live target's reported version (`rpc.drift`), RPC liveness + latency, run mode (pidfile/systemd) + uptime, hugepage gauges (`free_2m`/`total_2m`/DPDK reservation), subsystem/namespace/listener counts, `ptpl_files: {present, missing}` (**`missing > 0` is the PTPL-regression pre-alarm** — a reservation that will not survive the next target restart), and the ledger reconciliation counts (`managed/down/pending/removing/foreign_live`). Its `reactors[].busy_pct` is the tick-based **useful-work fraction** from `framework_get_reactors` (two samples 500 ms apart): it reads ~90 under saturating QD32 load and **0.0 idle — while the poller still burns ~0.99 of its core** (by-design busy-poll; process CPU accounting is the occupancy signal). Both numbers are true; alert on *work* with `busy_pct`, budget *cores* with occupancy. Initiator side — mounts whose meta/data devices are fabric-attached expose the **`fabric_*`** family on the `.stats` inode, sampled from sysfs at the stats cadence: `fabric_controllers`, `fabric_ctrl_not_live` (controllers in `connecting`/`resetting` — the reconnect-storm detector; measured storms run at 10 s cadence for ~10 min), and `fabric_ctrl_reconnects` — a **sampled-transition counter**, not a kernel counter: sysfs exposes only instantaneous controller state, so this counts observed `live → connecting/resetting` transitions at the sampling cadence and **undercounts flaps faster than it** (fine for a storm detector; do not "fix" it against a nonexistent kernel counter). Controller identity is renumbering-stable (transport + subsysnqn + target endpoint — `nvme3` can die and reattach as `nvme7` and still counts as a reconnect, not a new controller). `squeezefs status <sqmeta-uri>` reports the same data as a per-volume `"Fabric"` section when the backing device is fabric-attached. Guard signals stay `writer_guard_{mode,fenced,pr_reacquires}` with the per-stack reading: **growth of `pr_reacquires` on an SPDK-served volume is a PTPL regression signal** (alert), while across kernel-nvmet target power cycles it is *expected* (nvmet has no PTPL; the heartbeat re-check law covers it).

## NVMe-oF operations

The top-level **`squeezefs nvmeof`** verb (dual-stack) shares and dismantles NVMe-oF target subsystems, manages the target runtime, and connects clients (runbooks: [QUICKSTART §4](../QUICKSTART.md#4-nvme-of-fabric-setup-remote-block-storage)). Target-stack selection is explicit — `--target-stack {spdk|nvmet}` (or `SQUEEZEFS_NVMEOF_TARGET_STACK`), default **spdk** — and failure is loud: there is **no silent cross-stack fallback**. Both stacks are fully managed (milestone N4): the **SPDK path** (default) rides the pinned v26.05 lifecycle verbs (commit-sha-verified build, hugepage setup with recorded prior, pidfile start/stop, status, systemd-unit emission) and `save_config`/`load_config`-backed sharing with pinned `nsid` + ns UUID + `ptpl_file`; the **kernel-nvmet path** is the rebuilt configfs plumbing (reserved port-id range 53000–53999 with ownership checks, `resv_enable`+`device_uuid` stamped before enable). Both ride the write-ahead intent ledger, a working `restore`, and a **cross-stack live-state duplicate-backing guard** (the same backing must never be double-served — refusals name the live holder and the exact removal steps).

**Choosing a stack — per deployment class (measured).** The default is spdk everywhere (one default, no auto-switching); this table is what makes an explicit `--target-stack nvmet` an informed choice. Basis: the 2026-07-17 scoping A/B (`.benchmarks/2026-07-17-spdk-target-scoping.md` §3) re-proven 2026-07-18 through the product's own verbs on the fidelity rig (`.benchmarks/2026-07-18-nvmeof-dual-stack-ab.md`) — zram-backed NVMe/TCP localhost, fio io_uring, per-core accounting stated per row:

| Deployment class | Guidance | Measured basis |
|---|---|---|
| Dedicated storage/target node; queued I/O (the product's data-path shape: QD32 4 KiB blocks, QD8 sequential streams) | **spdk (default)** | rand-read QD32 2.11× (235 k vs 111.5 k IOPS), rand-write QD32 +23 % with a 1.8× tighter write p99 (880 vs 1,597 µs), seq-read +47 % (2,394 vs 1,631 MB/s); PTPL reservation persistence; **the dedicated poller core is the entry price** and is available on this class |
| Converged node (target + daemon + apps), core-constrained | consider `--target-stack nvmet` | SPDK's default reactor **busy-polls ~100 % of one core even idle** (0.99 cores measured at zero load); per-system-core efficiency is **parity-class, not an SPDK win** (rerun bracket: spdk 74–133 k IOPS/net-core across association signatures vs nvmet ≈ 120 k on rand-read QD32) — SPDK's absolute wins come from the dedicated poller, not lower total system cost |
| QD1-latency-dominated consumers | consider `--target-stack nvmet` | the kernel target completes inline in softirq: p50 2–3 µs vs 11 µs read, 23 µs vs 30 µs write at QD1 (TCP-localhost, single reactor) |
| Sequential-write-heavy service | **no stack preference from the evidence** | seq128k write QD8 collapsed on **both** TCP arms (spdk 199 vs nvmet 244 MB/s, nvmet marginally ahead, against the zram backing's ~1.5 GB/s raw ceiling) — an arm-symmetric transport/backing interaction, not a stack differentiator; choose on the other rows |
| Locked-down hosts (no hugepages, no out-of-distro binaries) | `--target-stack nvmet` | zero extra install; PR-capable (`resv_enable=1`) but no PTPL — the guard's heartbeat law covers target power cycles there |

Reactor scaling: leave `target start` at its **one-reactor default**. The rerun's 2-/4-reactor rows (TCP-localhost-bound — they bound the shape, not a fleet claim) measured **no gain on reads, ~29 % regressions on rand-write/seq-read, and a full burned core per added reactor** (262 k → 117 k → 61 k IOPS per reactor-core); raise `--cores` only with a multi-connection workload and your own measurement. The backing stays `bdev_aio`: the named `bdev_uring` comparison row measured uring writes at 0.51× with 2–2.8× worse tails on the same binary/backing (switch-only-if-data-says-so — the data says no).

```bash
squeezefs nvmeof share <backing> --ip <ip>[,...] [--port 4420] [--subnqn <nqn>] \
    [--target-stack spdk|nvmet] [--nsid <n>] [--ns-uuid <uuid>] \
    [--create-size <sz>] [--allow-host <hostnqn>]... [--accept-version-drift]
squeezefs nvmeof unshare <subnqn> [--force]   # stack resolved from the share ledger;
                                              # --force overrides the live-consumer refusal
squeezefs nvmeof list [--json]             # managed/down/pending/removing/foreign, both stacks
squeezefs nvmeof restore [--target-stack spdk|nvmet]   # replay ledger; reconcile intents
squeezefs nvmeof adopt <subnqn> [--target-stack spdk|nvmet]  # absorb a live foreign share
                                           # into the ledger — target state never touched
squeezefs nvmeof target install [--with-pkgdep]     # pinned SPDK v26.05 build, sha-verified
squeezefs nvmeof target setup [--hugemem-mb 2048] [--restore-prior]
squeezefs nvmeof target start [--core-mask 0x..|--cores N] [--dpdk-mem-mb 1024]
squeezefs nvmeof target stop [--force]     # save_config -> TERM -> grace -> KILL
squeezefs nvmeof target status [--json]    # RPC liveness, version+drift, reactors, ledger
squeezefs nvmeof target systemd-unit       # emitted to stdout, never installed
squeezefs nvmeof connect --ip <ip> --subnqn <nqn> [--port <port>]
squeezefs nvmeof disconnect <nqn>
```

Flag semantics: `--ns-uuid` seeds the recorded namespace identity on **both** stacks (generated once when absent; re-presented by `restore` so initiators reattach); `--nsid` is **SPDK-only** — the kernel-nvmet namespace index is structurally fixed at 1, so `--nsid` ≠ 1 with nvmet refuses loud. Missing backing paths refuse loud (`--create-size` is the explicit sparse-create opt-in; NoCOW-guarded on btrfs). `--allow-host` (repeatable) restricts a share to named host NQNs; the default is allow-any — the trusted-fabric posture. Every SPDK share pins `nsid` (default 1) + ns UUID + a `ptpl_file` under the state dir, so NVMe reservations persist through target restarts and `restore`/`load_config` re-present the same identity. `unshare` refuses while the subsystem has live initiator connections (`--force` overrides — unmount → `disconnect` → unshare is the sequence). Target-verb semantics: `target install` **never mutates system packages without `--with-pkgdep`** (a missing toolchain refuses loud with the package list); `target setup` records the prior `nr_hugepages` and `--restore-prior` restores it; a target whose version drifts from the v26.05 pin refuses mutating verbs (`share`/`unshare`/`restore`/`target start`) without `--accept-version-drift` (`target status` always reports drift, `target stop` warns and proceeds); dev/rig boxes may point `SQUEEZEFS_SPDK_TGT_BIN` at an existing build (loud, unpinned).

**`adopt` semantics** (design §6.10): an **explicit operator action** — never automatic, never another verb's fallback — that absorbs a live **foreign** (unledgered) share into management by **writing only the share ledger; the live target object is untouched**, so data keeps serving with zero interruption (the pre-rebuild-share and ledger-loss recoveries both ride it). The stack is auto-detected from where the subsystem lives; `--target-stack` only disambiguates an NQN live on **both** stacks (adopt otherwise fails closed naming both holders). The live object's identity is read as-is — backing, listeners (out-of-range nvmet port ids recorded verbatim; teardown removes them only when link-free), namespace UUID, allow-host list — with **loud nulls** where the object exposes nothing (a missing identity/`ptpl_file` records null with a note that a re-share upgrades it; a surviving state-dir ptpl file is re-bound). Adopt refuses loud on six named classes: `adopt_not_live`, `adopt_ambiguous`, `adopt_already_ledgered` (NQN **or** backing, any intent state — `restore`/`unshare` territory), `adopt_backing_duplicated` (the duplicate-backing guard applies verbatim), `adopt_harness_owned` (test-fabric NQNs/ports are never absorbed), `adopt_shape_unsupported` (multi-namespace / non-`bdev_aio` / listener-less shapes — removal-first + re-share is the remediation). Absorption rides the write-ahead intent protocol with `adopted_from` provenance (surfaced by `list`), re-verifies live state just before finalizing (drift aborts loud and leaves nothing behind), and on the SPDK stack ends with `save_config` so `tgt-config.json` describes what the target now serves under management. After adoption the share is fully managed — `list`/`restore`/`unshare` treat it like any other.

Target-side health and initiator-side fabric signals (`target status`, `fabric_*`, per-stack guard readings): see [Fabric observability](#fabric-observability).

## Performance records

Every number traces to a committed `.benchmarks/` note (box/substrate/method inside each). Headline classes on the reference box:

| Axis | Measured class | Evidence |
|---|---|---|
| rand-4k O_DIRECT read IOPS, **default mount, device-true** | **300–320 k** (zero knobs; 11.4× the pre-L1 stock posture) | `.benchmarks/2026-07-15-iops-parity-decomposition.md` (44 k → 316 k), `.benchmarks/2026-07-15-l1-transport-concurrency.md` |
| rand-4k O_DIRECT read IOPS, tier-resident (hybrid warm) | **~536–558 k** steady-state, zero device traffic | `.benchmarks/2026-07-15-hybrid-io.md`, reconfirmed `.benchmarks/2026-07-17-rand-write-program-closing.md` §3b |
| rand-4k write IOPS (sole-owner patch shape) | **59–67 k** (was 354–397 pre-program; device cost 4 KiB-class/op vs ~12 MiB/op) | `.benchmarks/2026-07-17-rand-write-program-closing.md` |
| Large sequential write | ~1.8 GB/s zero-copy write-through (≥ 3.5× pre-program); **4.4–4.6 GiB/s device-true** during scoreboard seq rows | `.benchmarks/2026-07-08-zero-copy-write-path-closing.md`, `2026-07-17-rand-write-program-closing.md` §2 |
| Metadata: create / entries-per-op | one-dir creates +44–62 % (110 µs/op serial wall), many-dirs 32.7 k/s; rename/unlink ≈ **1.0 journal entries/op** | `.benchmarks/2026-07-15-metadata-throughput-closing.md` |
| Mount time at scale | 100 M-inode volume cold-mounts in ~22 ms | `.benchmarks/2026-07-09-kv-v3-gates.md` |
| Crash contract soaks | kill-9 acked-loss **0** across 10/10 SMO soak rounds + 100/100 journal kill soaks; torn-write drops 0 | `.benchmarks/2026-07-17-rand-write-program-closing.md` §7, `2026-07-15-metadata-throughput-closing.md` G6 |

The rest of `.benchmarks/` is the per-program measurement lineage — baselines, attribution rigs, fix verifications, and closing adjudications; each program's closing record is indexed from its design doc (`docs/design-*.md`).

### The multi-reference scoreboard (release gate)

`tests/run_scoreboard.sh` is the standing **multi-reference scoreboard** — the proof surface for the top-3-fastest-FUSE-filesystems directive (PERFORMANCE IS PRIMARY, 2026-07-18). It measures SqueezeFS against the reference fast-FUSE field on one substrate with matched budgets and identical elbencho drivers: **JuiceFS** (meta engine + `file://` objstore), **SeaweedFS native** (`weed server` master+volume+filer, `weed mount`), and **geesefs** + **mountpoint-s3** over one shared local **RustFS** S3 store (object store held constant so those two rows differ only by client) — all pinned releases with recorded checksums, fetched into a non-repo tools dir. The grid is unchanged from the vs-JuiceFS lineage it absorbed: **three regimes** — R1 as-deployed (all cache layers live, dataset 2–4× cache), R2 device-true (sqz `-o direct_device_true` verified by counters; refs by cache-off knobs + tight cages on the page-cache-serving process, verified by device-byte evidence per row), R3 cold-cache (full drops + client cache wipes, first pass) — × the 6-shape workload grid (seq write/read 1 MiB, rand read/write 4k at `t16 iodepth16 --direct`, stat storm, del storm).

**RW6 durability-leveled timing:** write-family rows carry two modes. *Relaxed* is each system's native ACK semantics (published, labeled, never gating). *Durable* — which **governs the write-family verdicts** — is fsync-inclusive at matched depth: the harness times fdatasync on every dataset file through the mount, then syncfs on the mount, then syncfs on the substrate (every system's backing store flushed to the device before the clock stops). elbencho's `--sync` cannot deliver this (verified from v3.1-9 source: it is a separate post-phase syncfs step, excluded from the WRITE row, and several FUSE clients no-op `FUSE_SYNCFS`), so the pass is the harness's own, identical for every system. **RW6-del** applies the same pattern to the delete family: del rows carry relaxed (labeled) + *durable* (governs) modes, where durable adds a timed pass of tree-dir + mount-root fsync (the client flush lever — geesefs documents dir-fsync flushes all pending changes, deletes included, and its syncfs is not FUSE-wired) then mount + substrate syncfs (store settle). The historical `R1/R3.seq_write_1m` allowlist (JuiceFS's page-cache-ACK artifact) is retired, and the inaugural run's three adjudicated entries (`R1/R2.del_storm.gee`, `R2.seq_write_1m.jfs`) were deleted the same day their follow-ups landed (RW6-del + the `direct_device_true` write-path-inertness pin — see the baseline report's 2026-07-18 addendum) — **the standing allowlist is ∅ (bare run)**.

**Capability matrix:** where a reference does not support a workload *by design*, the cell reads **N/S (not supported by design)** — neutral, never "0 IOPS", never a LOSS, excluded from rank denominators. The matrix is declarative in the harness (one-line reason per cell) and verified empirically at mount time (the refusal errno is recorded in `capabilities.tsv`; a probe that succeeds un-declares the cell loudly). Current matrix: `mps3.rand_write_4k` (sequential-upload semantics; EBADF verified).

```bash
tests/run_scoreboard.sh                      # full scoreboard (~2–4 h, quiet-gated)
tests/run_scoreboard.sh teardown             # kill owned daemons + wipe stores
SQUEEZEFS_SB_SMOKE=1 tests/run_scoreboard.sh # ~10 min micro-grid plumbing proof (per-commit tier)
```

**The gate:** the run emits the primary kernel-FUSE table (per-reference W/L/TIE at ±5%, SqueezeFS rank per row, per-row-family **top-3 adjudication**) plus a labeled relaxed-write table, machine TSV, and per-row raw evidence (elbencho output + CSV, `.stats`/metrics counter snapshots, diskstats deltas, durability-pass splits, honesty lines), and **exits nonzero on any unattributed LOSS or any INVALID SqueezeFS cell** in the primary table. `SQUEEZEFS_SB_ALLOW_LOSS="R1.foo,R1.foo.jfs,..."` names attributed losses (row-wide or per-reference); a reference whose own setup fails 3× becomes an n/a-with-reason column (named residual), never a run abort — the SqueezeFS side always gates hard. Legacy `SQUEEZEFS_VS_*` env spellings are honored. Cadence: **per-release** and after any perf-relevant landing.

**Current standing (inaugural multi-reference run, 2026-07-18): see `.benchmarks/2026-07-18-multi-reference-scoreboard.md`** — provenance, the full grid, per-family top-3 adjudication, and the honest-anomalies ledger. JuiceFS-only lineage: 13 W / 3 TIE closing standing `.benchmarks/2026-07-17-rand-write-program-closing.md` §2, inaugural baseline `.benchmarks/2026-07-15-vs-juicefs-scoreboard.md` (both measured with the retired `tests/run_vs_juicefs.sh` protocol this harness absorbed).

### Built-in benchmark (`squeezefs bench`)

A **bare invocation is the full saturation suite**: over one auto-sized dataset it runs write seq `1m` `--direct` → read seq `1m` `--direct` → read rand `4k` `--direct` (30 s box) → write rand `4k` `--direct` (30 s box) → stat → del (timed; leaves the mount clean), and prints one table with a row per pass (THROUGHPUT / IOPS / coverage / latency min/avg/p99/max) plus the daemon's `.stats` metrics delta.

```bash
squeezefs bench /mnt/squeezefs
```

Auto-sizing (the default for `-t`/`-n`/`-s` everywhere; explicit flags always override): threads = `min(CPUs, 16)`, 1 file per thread, total = `max(16 GiB, 2 GiB × threads)` capped at 25% of the mountpoint's free space (loud error if even 4 GiB does not fit), per-file rounded down to 1 MiB. The computed shape — with `(auto)`/`(explicit)` provenance per value — is printed loudly in the header of **every** run.

*Explicit phases* run over the same **persistent, reusable dataset** at `<mountpoint>/squeezefs-bench/t{tid}/f{fid}.bin` and inherit the identical auto defaults, so single-phase numbers are directly comparable to the matching suite pass:

```bash
# reproduce the suite's rand-4k read pass against an existing dataset:
squeezefs bench /mnt/squeezefs -r --rand -b 4k --direct
# write then read 1 GiB/file across 4 threads at 1 MiB ops:
squeezefs bench /mnt/squeezefs -t 4 -w -r -s 1g -b 1m
# re-read the SAME dataset later at a different I/O size (no rewrite):
squeezefs bench /mnt/squeezefs -t 4 -r -s 1g -b 128k
```

*Phases* (any combination, always executed in this fixed order; none given ⇒ the full suite):
- `-w, --write`: create/overwrite the dataset, timed (includes create/open; each file is fsync'd before the clock stops — durable write numbers).
- `-r, --read`: read it back, timed (reuses the dataset from an earlier `-w`; loud shape-mismatch error otherwise — never silently creates files).
- `--stat`: stat every file, timed.
- `--del`: delete the dataset, timed (doubles as cleanup).

*Shape* (applies to all phases; `-t`/`-n`/`-s` auto-size when omitted):
- `-t, --threads <N>`: workers (default: auto = `min(CPUs, 16)`).
- `-n, --files <N>`: files per thread (default: auto = 1).
- `-s, --size <SZ>`: file size, human units `4k`/`128k`/`4m`/`10g` or plain bytes (default: auto-sized from free space, see above).
- `-b, --block <SZ>`: I/O size per operation, same units (default: `1m`; explicit phase runs only — the suite fixes `1m` seq / `4k` rand).
- `--rand`: random offsets (shuffled full-coverage block list — every block exactly once; explicit phase runs only).
- `--direct`: O_DIRECT (`-b` must be a multiple of 4096 and `-s` a multiple of `-b`); the suite's I/O passes are always O_DIRECT.
- `--time <SECS>`: wall-clock box for rand read/write passes (default: 30 for `--rand`, unlimited for sequential; `0` forces full coverage). Partial coverage is honest — reported over actual elapsed/bytes and stated in the row.
- `-i, --iterations <N>`: repeat the selected pass set (default: 1).

Write phases fill blocks with a deterministic non-zero pattern seeded per `(thread, file, block)`, so transparent compression cannot fake throughput numbers.
