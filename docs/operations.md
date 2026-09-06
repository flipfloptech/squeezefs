# SqueezeFS Operations Reference

This is the operator reference for SqueezeFS: the durability contract and its guarantee classes, the breaking-changes catalog, the configuration-knob reference beyond `--help`, the observability surfaces, NVMe-oF target operations, and the measured performance record. The hands-on walkthrough (sandbox → bare metal → fabric) is [QUICKSTART.md](../QUICKSTART.md); the project overview is [README.md](../README.md); normative designs live in `docs/design-*.md` and measurement records in `.benchmarks/` (each note states its box, substrate, and method).

## Contents

- [Versioning & releases](#versioning--releases)
  - [Verifying a build (`task check`)](#verifying-a-build-task-check)
- [Durability & crash contract](#durability--crash-contract)
  - [Metadata Durability (crash contract)](#metadata-durability-crash-contract)
  - [Single-writer mount guard (guarantee classes)](#single-writer-mount-guard-guarantee-classes)
  - [Write custody & visibility by mount posture (§5.4)](#write-custody--visibility-by-mount-posture-the-54-guarantee-tables)
  - [The DATA plane (DLM S7)](#the-data-plane-dlm-stage-s7)
  - [Multi-writer data plane (DLM S9)](#multi-writer-data-plane-dlm-stage-s9)
  - [Multi-writer capacity planning — the allocation partition](#multi-writer-capacity-planning--the-data-plane-allocation-partition)
  - [Multi-writer co-writer mounts (DLM S9)](#multi-writer-co-writer-mounts-dlm-stage-s9)
  - [Byte-range custody (DLM S11)](#byte-range-custody-dlm-stage-s11)
  - [Read-only coherent mounts (`-o ro`)](#read-only-coherent-mounts--o-ro--one-writer-plus-n-readers)
  - [Format v3 (CoW KV metadata)](#format-v3-cow-kv-metadata)
  - [Read-only coherent mounts — the stated consistency model](#read-only-coherent-mounts--the-stated-consistency-model-metadata)
  - [Cross-volume namespace operations](#cross-volume-namespace-operations-multi-volume-metadata-sets)
- [POSIX semantics — declared deviations](#posix-semantics--declared-deviations)
  - [`fallocate(mode = 0)` does not reserve space](#fallocatemode--0--posix_fallocate-does-not-reserve-space-posix-12)
  - [`noatime` is the only atime policy](#noatime-is-the-only-atime-policy--the-tool-classes-that-notice-posix-17)
  - [Rename-overwrite leaves a crash-window orphan](#rename-overwrite-leaves-a-crash-window-orphan-posix-15)
  - [Writable shared `mmap` and interception](#writable-shared-mmap-and-interception-posix-7)
- [Breaking changes & migration notes](#breaking-changes--migration-notes)
- [Removed verbs & flags](#removed-verbs--flags)
- [Configuration reference](#configuration-reference)
  - [Environment knobs — the parsing convention](#environment-knobs--the-parsing-convention)
  - [Environment knobs — the complete registry](#environment-knobs--the-complete-registry)
  - [SqueezeFS URI scheme](#squeezefs-uri-scheme)
  - [Format (`squeezefs format`)](#format-squeezefs-format)
  - [Mount (`squeezefs mount`)](#mount-squeezefs-mount)
  - [LD_PRELOAD interception (`-o interception`)](#ld_preload-interception--o-interception--security-posture--unsupported-mixes)
  - [Multi-user mounts — the single-tenant resource posture](#multi-user-mounts--the-single-tenant-resource-posture)
  - [Fleet-share sizing (co-located daemon fleets)](#fleet-share-sizing-co-located-daemon-fleets)
  - [Cache/staging paths (`squeezefs config`)](#cachestaging-paths-squeezefs-config)
  - [Transparent compression & encryption](#transparent-compression--encryption)
  - [Read-path tuning](#read-path-tuning-mount-env-design-docsdesign-read-pathmd)
  - [Hybrid I/O for O_DIRECT reads and the device-true escape](#hybrid-io-for-o_direct-reads-default-and-the-device-true-escape)
  - [Random-small-write path](#random-small-write-path-sole-owner-patch--extent-overlay-design-docsdesign-random-small-writesmd)
  - [Large files — the block-map tree](#large-files--the-block-map-tree)
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
  - [Cluster wire (the one cluster transport)](#cluster-wire-the-one-cluster-transport)
  - [Metadata function shipping (DLM S8)](#metadata-function-shipping-dlm-s8)
  - [Per-volume metadata owners (`volume set-owners`)](#per-volume-metadata-owners-squeezefs-volume-set-owners)
  - [Subtree delegations & UPDATE intents (DLM S10)](#subtree-delegations--update-intents-dlm-stage-s10)
  - [Membership plane — lease-based liveness (DLM S6)](#membership-plane--lease-based-liveness-dlm-s6)
  - [Freed-offset grace period (spec §6.8 item 3)](#freed-offset-grace-period-spec-68-item-3)
- [Observability](#observability)
  - [The `.stats` families — complete reference](#the-stats-families--complete-reference)
  - [`df` / statfs semantics](#df--statfs-semantics)
  - [Fabric observability](#fabric-observability)
- [NVMe-oF operations](#nvme-of-operations)
- [Performance records](#performance-records)
  - [Field records (squeeze-test)](#field-records-squeeze-test)
  - [Reference-box records (2026-07 programs)](#reference-box-records-2026-07-programs)
  - [The multi-reference scoreboard (release gate)](#the-multi-reference-scoreboard-release-gate)
  - [Built-in benchmark (`squeezefs bench`)](#built-in-benchmark-squeezefs-bench)

---

## Versioning & releases

**A SqueezeFS build carries two identities, both surfaced** (user directive 2026-07-24, superseding the commit-only 2026-07-18 policy):

1. **The release-train version** — `Cargo.toml`'s package `version` (currently the **1.2 train**), bumped **as a release act** (never by CI, never per commit). The first-party crates (`crates/fuse3` — the fully-diverged fork, `crates/squeezefs-ipc`, `crates/squeezefs-preload`) track the same train. What each train changed for operators is [`RELEASE_NOTES.md`](../RELEASE_NOTES.md) (1.2.0: highlights, the upgrade story from 1.1, fixes, new surface, the release gate).
2. **The git commit the build was produced from** — the fine-grained identity. Periodic releases remain **annotated git tags on specific commits** — `stable-YYYY.MM[.N]` and `lts-YYYY.MM` — created manually as a release act. The tag names the release; the commit pins the exact build. Tags are the **only** release names.

**Verify what a node is running** (the two surfaces carry the same build-time capture):

```bash
squeezefs --version        # or -V
# untagged build:  squeezefs 1.2.1 (f63455bcb824 / f63455bcb8249b064531d000624c40825a6e763e) built 2026-07-18T13:45:25Z profile release
# release build:   squeezefs 1.2.1 (f63455bcb824 / f63455bcb8249b064531d000624c40825a6e763e, tag stable-2026.09.1) built 2026-07-18T13:45:25Z profile dist
grep -E '"build_(commit|tag|profile)"' <mountpoint>/.stats   # the fleet mixed-version detector
```

**The two-profile LTO law (2026-09-02).** `cargo build --release` (and every `task build*`, the gate, every field A/B leg) builds the **`release`** profile — thin LTO, parallel codegen: the fast dev/field build. A **tagged release** ships from the **`dist`** profile (`task dist:<distro>` / `cargo build --profile dist`, shim `preload-dist`) — fat LTO plus a single codegen unit, the slow maximal-inlining build that only a `stable-*`/`lts-*` tag should pay for. The version line's trailing `profile <name>` and `.stats` `build_profile` say which one a binary is, so a measurement row can never silently compare a `release` leg against a `dist` leg (both legs of any A/B must name the same profile; the scoreboard and release-gate rows run `dist`).

The `.stats` inode exports `build_commit` (the full hash), `build_tag` (always present; empty string when the commit is not a release) and `build_profile` on every mounted daemon — sweep it across the fleet to find mixed-version nodes. A `-dirty` suffix on the hash means the binary was built from a tree with uncommitted **tracked** changes (untracked scratch does not count; the flag is captured when the build script runs) — a dirty rebuild of a tagged commit deliberately does not masquerade as the release.

Mechanics and edges:

- The commit identity is captured at compile time by `build.rs` (`git rev-parse HEAD`, `--short=12`, `git status --porcelain --untracked-files=no`, `git describe --tags --exact-match`) and embedded via `SQUEEZEFS_BUILD_*` rustc envs; the train version is `CARGO_PKG_VERSION`; `src/version.rs` is the single formatting source of truth for `--version`, the mount/format summary, the mount-ready log lines, `.config`'s `client_version`, and `.stats`.
- **Bumping the train is a release act**: edit the package `version` in the root `Cargo.toml` and the first-party crate manifests (`crates/fuse3`, `crates/squeezefs-ipc`, `crates/squeezefs-preload`) together — they carry the same train. (The pre-2026-07-24 "`0.1.0` cargo-internal placeholder, never bump" posture is retired.) The `.stats` `build_commit`/`build_tag` fields stay raw commit/tag — fleet tooling never parses the version line.
- **Tarball / no-git builds**: packagers set `SQUEEZEFS_BUILD_COMMIT` (and optionally `SQUEEZEFS_BUILD_TAG`) in the build environment to stamp the identity; without git *and* without the envs, the build still succeeds and embeds `unknown`. `SOURCE_DATE_EPOCH` is honored for a reproducible build timestamp.

### Verifying a build (`task check`)

`task check` (`Taskfile.yml`) is the authoritative verification gate — the one CI runs (`.github/workflows/gate.yml`) and the one every code-class commit must pass before merge. It runs, in order:

| Leg | Command | What it proves |
|---|---|---|
| clippy, all features | `cargo clippy --all-targets --all-features -- -D warnings` | lint-clean incl. the `gds`/`dhat-on`/`coz-on` cfgs; unused code is a failure (deleted, never allowed) |
| clippy, shipped config | `cargo clippy --all-targets -- -D warnings` | the default-feature configuration the release binary is built from |
| fmt | `cargo fmt --check` | formatting |
| tests | `cargo test --all-features -- --test-threads=1` | the whole suite, single-threaded (mount-class suites self-skip without `/dev/fuse` — see the require-mount leg) |
| rustdoc | `RUSTDOCFLAGS="-D warnings" cargo doc --no-deps` | rustdoc lints (broken intra-doc links) are warnings by default; the flag makes the line a gate |
| bench smoke | `cargo bench --benches -- --test` | one iteration of every Criterion bench, no measurement — a panicking bench fails the gate |
| `task check:fuse3` | the same clippy/fmt/test/doc/bench legs inside `crates/fuse3` | the first-party FUSE fork is its own excluded workspace; nothing above compiles its tests |
| `task check:loom` | `cargo build --release --tests` + `cargo fmt --check` under `RUSTFLAGS="--cfg loom -D warnings"` in `loom-models/` | the lock-free protocol models still compile against the cores they include (the model RUN is `tests/run_loom.sh`, nightly / on lock-free-core changes) |
| `task check:docs` | `tests/check_markdown_links.sh` | relative links and in-document anchors across every tracked markdown file — the whole gate for a docs-only commit |
| `task audit` | `cargo audit --deny unsound --deny yanked` over both lockfiles (root + `crates/fuse3`) | supply chain; vulnerabilities, unsound and yanked crates fail |

`--all-features` compiles `dhat-on`, which replaces the jemalloc allocator: correct for a correctness gate, never valid for a measurement. Measurement builds are `cargo build --release` (default features), and tagged releases ship from the `dist` profile via `task dist:<distro>`. The release-only legs beyond `task check` — the external POSIX suites (pjdfstest, LTP, fstests), `task check:require-mount`, the zc-capability gate, fuzzing, the bench baseline compare and the scoreboard — are tiered in [AGENTS.md](../AGENTS.md) §Testing.

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
- `SQUEEZEFS_INODE_RECLAIM_BATCH`: inode-reclaim group-commit batch size (default `64`, clamp 1–1024), with `SQUEEZEFS_INODE_RECLAIM_WINDOW_MS` (default `20`) and `SQUEEZEFS_INODE_RECLAIM_CONCURRENCY` (default `max(4, cpus)`). Renamed out of the unrelated **block**-reclaim prefix (ENG-10 — `SQUEEZEFS_RECLAIM_BATCH` was a strict prefix of `SQUEEZEFS_RECLAIM_BATCH_BLOCKS`); the three old spellings refuse the process loudly, naming these successors.
- `SQUEEZEFS_META_COMMIT_BATCH_TXS` / `SQUEEZEFS_META_COMMIT_BATCH_BYTES`: per-volume commit-conveyor batch caps (defaults derived since 2026-08-04: `max(64, cpus × 2)` transactions / `max(256 KiB, ring/16)` — floors are the shipped M7 posture; explicit env wins verbatim and bytes are clamped to the journal ring's admissible capacity). Group commit batches admission, locking, the journal write, and the barrier across concurrent transactions — **never the atomicity unit**: one transaction stays one checksummed journal entry (design `docs/design-metadata-throughput.md` §5.5). Watch `meta_commit_group_size` on the `.stats` inode; a strict-mode median ≈ 1 under concurrent writers means batching regressed.
- **The conveyor runs in two stages on a per-volume journal lane** (2026-09-03): the apply stage takes the batch's leaf locks, applies in RAM, drops the locks and *submits* the journal write; a durability lane then awaits the writes in order and takes each group through one barrier and the acknowledgements — so the device's completion latency never sits inside the serialized apply stage. Both stages run on one OS thread per writable metadata volume (`sqz-jrnl{N}`) that owns that volume's journal io_uring and parks in it. `SQUEEZEFS_JOURNAL_LANE=0` is the A/B control (both stages on the shared metadata pool). Engagement: `meta_conveyor_windows_inflight` (1 = serialized, ≥ 2 = overlapped), `meta_conveyor_durability_passes`, `journal_ring_lane_writes` ≈ conveyor passes (`journal_ring_pool_writes` carries the rest), `journal_lanes_{spawned,ringless}`; the atomicity unit is unchanged (one tx = one checksummed journal entry), acks still arrive in journal order and only after the barrier.
- `SQUEEZEFS_OP_PROFILE=1`: per-op FUSE phase histograms (`fuse_op_phase_ns`, `fuse_create_under_lock_ns`) on the `.stats` inode — diagnostics for metadata-latency attribution. Off by default; zero per-op cost when off.
- `SQUEEZEFS_OP_TRACE=1`: arm the **per-op trace ring** at mount (e2e audit A2, `docs/design-e2e-perf-audit.md` §1): one CLOCK_MONOTONIC stamp per phase boundary per sampled op, joined on ONE timeline — the op id is the FUSE request `unique` (the kernel's `fuse:fuse_request_send/end` tracepoints carry it) or the il slot ticket. Geometry and the sampling divisor are derived (rings from the thread population, the pool from the R5 budget as the `op_trace_rings` component, the divisor so one drain interval of the machine's op ceiling fits). `cat <mnt>/.trace` **drains** the rings into `{armed, divisor, dropped, samples_total, clock, stages, samples: [[op_id, stage, ns], …]}` (owner-only `0400`, like `.stats`); `tests/op_trace_stitch.py` stitches a dump against the row's `.stats` histograms and the kernel tracepoints. `.stats` carries `op_trace_{armed,samples,dropped,divisor}`; the `op-trace on|off|status` admin verb toggles the ring live. Off by default; one relaxed load per hook when off (`benches/high_concurrency_bench.rs` `op_trace_hook`: 0.3–0.6 ns disarmed, ~5 ns armed).

### Single-writer mount guard (guarantee classes)

The v3 metadata engine is single-writer by construction, and the mount enforces it: every **write** mount claims each metadata volume with (a) a dedicated daemon-lifetime `flock` (same-host exclusivity; the kernel releases it instantly on process death), (b) an **NVMe Persistent Reservation** (Write Exclusive) where the namespace advertises reservation support — cross-host *enforcement*: the device itself rejects a fenced or stale holder's writes — and (c) a `writer_claim` heartbeat record (identity + detection on every substrate). A second concurrent **write** mount is **refused loudly, naming the holder**. There is **no bypass flag**; read-only probes (`status`, format preflight) are never blocked, and a **read-only mount** (`-o ro`) is admitted alongside the writer without taking or granting any exclusion — see [Read-only coherent mounts](#read-only-coherent-mounts--o-ro--one-writer-plus-n-readers). Design: `docs/design-metadata-throughput.md` §5.0. What the guard guarantees depends on the substrate:

| Substrate | Guarantee |
|---|---|
| Same host, any volume | **Refusal-grade** (flock on a dedicated fd; kernel-enforced; instant crash reclaim; SIGSTOP-safe) |
| NVMe / NVMe-oF namespace with `RESCAP` PR support | **Enforcement-grade** (Write-Exclusive reservation: the device rejects a fenced/stale holder's writes; acquire arbitrates simultaneous mounts; automatic TTL-stale preemption is safe). **Fencing detection latency ≤ one flush cadence + one barrier** (50 ms default; immediate in strict/fsync — Issue 14); PTPL-lapse residual ≤ 10 s (heartbeat report re-check, §5.0 B1 pt 6). Crash (kill -9) remount recovery is portable across Register semantics: on **spec-strict** targets (SPDK v26.05 and current kernel nvmet, both measured 2026-07-17) the guard's **register ladder** proves the conflicting registration is its own dead incarnation's — via the association's device-reported host identifier — and unregisters exactly that key before re-registering; foreign registrations are never touched (preempt/TTL/`claim clear` territory) |
| — SPDK-served namespace (`nvmeof share` — the **default stack**; lifecycle + sharing fully live as of milestone N4) | **Enforcement-grade, measured** (2026-07-17 rig: mount `flock+pr`, fencing EBADE class, preempt, ladder crash-remount ×10; re-proven 2026-07-18 through the product's own verbs — N4 gate) — and reservations **persist through target restarts** (PTPL; a live holder rides out `spdk_tgt` kill + `target start`/`load_config` with `writer_guard_fenced=0` and `writer_guard_pr_reacquires=0`). Every product SPDK share pins `nsid` + ns UUID + `ptpl_file` (`<state>/spdk/ptpl/<uuid>.json`), and `restore` re-presents the recorded identity — PTPL state re-binds across re-creates by construction |
| — loop-device-backed nvmet namespace (the repo's own file-backed share path, `losetup` wrap) | loop devices expose no PR ⇒ lands in the **"block without PR"** row below — named explicitly because the repo's own tooling creates this shape |
| Block volume **without** PR support | **Detection-grade**: mounts separated by > ~1 heartbeat are refused; near-simultaneous mounts can both arm; a paused holder cannot detect usurpation — therefore automatic cross-host takeover is disabled (operator-attested `claim clear` only) |
| File-backed volume shared cross-host (NFS et al.), or containers with private `/dev` nodes | **Unsupported for concurrent-mount protection** — single-host operation of such volumes remains fully guarded by flock (former) / PR-if-available (latter) |
| **Co-writer mount** (`SQUEEZEFS_MULTI_WRITER=1` + `SQUEEZEFS_MW_ROLE=co-writer`, admitted by the five-rung ladder) | **A second write-capable mount, admitted — and the first row in this table that is** (DLM S9; guarantee class `co-writer`). It is NOT a second *appender*: it takes no `flock`, writes no `writer_claim`, registers no key on the metadata namespaces and spawns no checkpoint task, and its metadata write gate refuses every LOCAL commit — every metadata mutation is **shipped** to the authority, whose ladder above runs unchanged. Its DATA writes are its own, admitted only under a custody lease that authority granted. **What ENFORCES it:** on a PR substrate the authority's rtype-1 Write Exclusive on the metadata namespaces means a co-writer *on another host* is device-blocked from writing metadata at all, and the rtype-3 WERO hold on the data namespaces means a preempted co-writer's DMA is rejected by the namespace; which bytes each co-writer may write is the authority's custody arbitration. **What only DETECTS:** the durable claim-set enrollment and the membership census (they answer *who is attached* and mint the dead epoch a failure is quarantined under — they stop nothing by themselves), and — the honest residual — a co-writer sharing a HOST with its authority is inside the same PR host identity, so nothing device-side distinguishes them: on that shape the metadata read-only half is enforced by this mount's own code, not by the device. **What an operator must have configured:** every rung of the ladder, listed in [Multi-writer co-writer mounts](#multi-writer-co-writer-mounts-dlm-stage-s9). Volumes formatted since the rung-10b Phase-B flip pass rung 2 by default (the format stamps the nine capability bits); pre-flip sets and `--single-writer` formats upgrade offline with `squeezefs volume enable-multi-writer` |
| **Set-authority mount** (`SQUEEZEFS_MULTI_WRITER=1` + `SQUEEZEFS_MW_ROLE=set-authority`, on a set with per-volume owners assigned — see [Per-volume metadata owners](#per-volume-metadata-owners-squeezefs-volume-set-owners)) | **The classic writer row, on a SUBSET of the set.** On the volumes it owns — which include the one hosting the filesystem root — every guarantee above is unchanged, byte for byte: `flock`, `writer_claim`, the reservation where the namespace supports one, the fresh-foreign refusal, the recovery ladder. On the volumes a **peer** owns it takes none of the three (guarantee class `peer-owned` in `writer_guard_mode`) and every metadata mutation there is shipped to that peer, whose own row in this table is the guarantee. **What makes it safe:** the volume it does not claim is one the durable assignment says it must not append to, and a mount whose assignment and live evidence disagree is refused at open, not reconciled |
| **Partial-authority mount** (`SQUEEZEFS_MULTI_WRITER=1` + `SQUEEZEFS_MW_ROLE=partial-authority`) — **accepted 2026-08-24 on a 2-owner fleet** (`.benchmarks/2026-08-22-pv-claim-admission.md`: serial `tar -x` on the partial authority's own subtree at 250 µs RTT = **0.98× of authority-local** — gate ≤ 1.10× MET, replacing the co-writer row's 6.73×; the rewrite funnel 0.92× of the set authority, the honest cost of shipping every terminal free; fsck + the C8 oracle clean after every leg; tier measured-real, one box) | **The same row for a node that does not own the root's volume.** Identical per-volume guarantees — full writer on its own volumes, `peer-owned` on the rest — with the set-singular planes belonging to the set authority instead: allocation-lane assignment, the custody endpoint, the freed-offset grace ring, maintenance coordination. **What it costs beyond the set authority's row:** the sole-owner in-place small-overwrite path is not available to it (a lifetime ownership retire is durable ownership state), so isolated small overwrites take the copy-on-write path; and terminal frees are shipped to the set authority rather than performed locally. **Ownership does not fail over** — if this node dies, the volumes it owns have no appender, so its subtree stops (every verb about it refuses at the ship site, loudly and counted) while the rest of the set keeps serving; the repair is an offline re-assignment |
| **Read-only mount** (`-o ro` / `--read-only`, any substrate) | **Not a writer, and not an obstacle to one** — guarantee class `reader`. A read-only mount takes NO `flock`, writes NO `writer_claim` and registers NO PR key, so (a) it is admitted while a writer holds the volume — including a *fresh foreign* claim, which refuses a write mount — (b) it never refuses a write mount, in either mount order, and (c) it changes nothing about the rows above: a second WRITER is still refused by exactly the same ladder. It mutates no plane (metadata, block allocation, frees, device reclaim, in-place patch/overwrite are all refused) and the kernel mounts it `MS_RDONLY`. Its *consistency* guarantee — which is a separate question from exclusion — is in [Read-only coherent mounts](#read-only-coherent-mounts--o-ro--one-writer-plus-n-readers) |

**What DLM S9 changed, and what it did not.** Every row above except the
co-writer one is **byte-for-byte what it was**: the `flock`, the claim
classification (including the fresh-foreign refusal), the PR arbitration and
the `claim clear` attestation all behave identically, and a plain `mount` of a
claimed volume set is refused exactly as before — including when the volumes
carry every capability bit and this node is enrolled. The co-writer row is
reached through a **different entry point**, not through the gate: a caller
needs an admission decision (five rungs, all of them) to open a volume that
way, and the only thing that produces one is the ladder. So the sentence
"a second write mount of one metadata volume is refused" is still true in the
sense the guard means it — a second *appender* to a volume's journal ring,
extent bitmap and root ledger is impossible — and is now false in the looser
sense of "a second mount that can write user data", which is exactly the
capability S9 exists to add. The custody half of it is
[Multi-writer data plane](#multi-writer-data-plane-dlm-stage-s9); the mount
half is [Multi-writer co-writer mounts](#multi-writer-co-writer-mounts-dlm-stage-s9).

#### Write custody & visibility by mount posture (the §5.4 guarantee tables)

The one table that answers, per mount posture: *who may write what, what a
mount sees, and what fences it*. Every row cites the gauges that prove the
posture live and the measurement record that adjudicated it
(design `docs/design-full-multi-writer.md` §5.4; program closing record
`.benchmarks/2026-08-18-mw-program-closing.md`).

| Posture (how you get it) | Write custody | What this mount SEES | What fences it | Evidence |
|---|---|---|---|---|
| **Solo writer, single-writer format** (`format --single-writer`, or any pre-flip volume; a plain write mount) | Everything, locally — the D0 guard's classic row. Full POSIX, whole-tx atomic metadata | Its own state, immediately (it is the only writer) | Its own D0 latch: flock loss is impossible while alive; PR fencing on `RESCAP` substrates; `writer_guard_fenced` | The shipped baseline; guarantee-class table above |
| **Solo writer, multi-writer-capable format** (bare `format` — the DEFAULT class since the rung-10b flip stamps the nine capability bits) | Identical to the row above **by measurement, not just by construction**: `dlm_rpcs == 0`, no partition installs (`alloc_lane_writers == 0`), the bit-9 durable block-ref ledger runs live with `meta_kv_block_refs_drift == 0` | Same | Same | S4 re-gate A-B-B-A within noise (seq/rand: `.benchmarks/2026-08-15-mw-s4-regate.md`; mdstorm + scoreboard smoke + external QUICK set: `.benchmarks/2026-08-16-mw-s4-residuals.md`) |
| **Reader** (`-o ro`) | None — every write plane refuses at three gates | The writer's most recent **checkpoint** it has polled: bounded monotone staleness, published as `reader_staleness_bound_ms` (2 s shipped defaults). DATA half *eliminated* with the membership plane armed (freed-offset grace), *bounded* with it off — the default | Nothing fences a reader's writes (none exist); a lapsed member self-fences its **caches** at `T_self` and re-joins fresh | [Read-only coherent mounts](#read-only-coherent-mounts--o-ro--one-writer-plus-n-readers); S6 arm rows `.benchmarks/2026-08-16-mw-s6-arm.md` |
| **Co-writer, whole-file custody** (the five-rung admission ladder — see [Multi-writer co-writer mounts](#multi-writer-co-writer-mounts-dlm-stage-s9)) | DATA bytes of files/offsets it holds a **custody lease** on, DMA'd directly under its custody epoch; fresh blocks from its granted allocation lane. Metadata: none locally — every mutation SHIPS to the authority (`local_commit_refusals` must stay 0) | A reader's checkpoint view **plus** its own mutations current (they execute on the authority — read-your-own-writes over the wire); under a LOOKUP delegation, coherence-promised local serves (see [Subtree delegations](#subtree-delegations--update-intents-dlm-stage-s10)) | Three composed layers, strictly ordered: its own `T_self` self-fence (fires before the authority may re-grant), the authority's revocation/era gate (every mutating publish carries the lease epoch — a swept era's publishes refuse with nothing applied, `meta_ship_publish.stale_refusals`), and the DEVICE (WERO rejection of a preempted registrant's DMA — proven live, EBADE class). A fenced co-writer is dead until remount | S8 arm `.benchmarks/2026-08-16-mw-s8-arm.md` (5.3 M shipped mutations, zero un-routed commits); S9 arm `.benchmarks/2026-08-16-mw-s9-arm.md` (fan-out / failover with zero acked-data loss); era gate `.benchmarks/2026-08-16-mw-publish-era-gate.md`; device rejection `.benchmarks/2026-08-16-mw-s7-arm.md` |
| **Ranged co-writer** (`SQUEEZEFS_RANGE_CUSTODY=1` on an armed co-writer — **default OFF**; see [Byte-range custody](#byte-range-custody-dlm-stage-s11)) | Byte ranges of SHARED files under EX range grants (sub-grants of the custody lease): block-aligned spans DMA directly; a sub-block-shared block demotes to **authority-assembled** — both holders ship extents, the authority merges and publishes, single publisher per block at every instant | As the co-writer row, plus its own retained extents overlay its reads (read-your-own-writes for un-published sub-block ships) | The co-writer row's three layers, plus the demotion barrier (`demotions ≡ acks + fence_resolves`) and the bit-15 layout-version gate as the crash backstop | S11 rows on pinned ior 4.0.0: `.benchmarks/2026-08-18-s11-mpiio-row.md` + `.benchmarks/2026-08-18-s11-widthn-refs-fix.md` (MPI-IO verdict **MET**: A-B-B-A shared/file-per-proc ≥ 0.8× both brackets — 1.411× / 2.273×; read-back exact; fsck + C8 clean) |

**Fencing class per PAIR of mounts** (the co-location split — design §5.4's
second table). What enforces exclusion between two write-capable mounts
depends on whether the DEVICE can tell them apart:

| Pair of mounts | Write-exclusion / fencing class |
|---|---|
| Distinct hosts, PR substrate | **Device-enforced** — WERO rejects a fenced host's DMA; the preempt of its registrant key is the drain proof. Proven live: `.benchmarks/2026-08-16-mw-s7-arm.md` (S7-a: reservation-conflict rejection + fail-stop on a real nvmet-tcp kernel target) |
| Co-located, distinct hostnqn (`SQUEEZEFS_HOSTNQN`/`SQUEEZEFS_HOSTID` per mount, pair-or-neither) | **Device-enforced** — the same row as distinct hosts. Requires the sqz kernel's host-scoped fabric subsystems (patch 0030, `docs/design-mw-multipath-kernel.md`): stock `nvme_core.multipath=Y` kernels merge two identities' paths under ONE head, which voids per-mount fencing — the mount **refuses** that shape loudly, naming the sqz kernel and the `multipath=N` boot-param workaround as the remedies |
| Co-located, shared hostnqn (the default when identity is not split) | The device sees ONE registrant: fencing between the two mounts is **process-local** (flock + custody epochs + the `authorize_dma` poison latch). This is the fleet rig's own honest-residual shape, and the client-side story is proven and pinned (`tests/mw_colocated_fence_tests.rs`; S9-c in `.benchmarks/2026-08-16-mw-s9-arm.md`) |
| Any pair, non-PR substrate | Multi-writer **refuses to arm** (unchanged S9 law — a co-writer the device cannot reject is a co-writer nothing can fence) |

#### The DATA plane (DLM stage S7)

The rows above are the **metadata** plane: the guard claims meta volumes, and its reservations cover meta namespaces only. The data plane is fenced separately, and what it *enforces* versus what it only *detects* also depends on the substrate. Design: `docs/pre-rc-engineering-spec.md` §6.7/§7 RES-6; contracts: `tests/dlm_data_fence_tests.rs`.

| Data-plane posture | Substrate | Enforced | Detected |
|---|---|---|---|
| **Single-writer** (the default; no `SQUEEZEFS_MULTI_WRITER`) | any | **DMA submission** — every data-plane write passes one authorization point that refuses when this mount's D0 guard is fenced *or* when the submission's **custody epoch** (the durable writer term) is no longer current, so an upload admitted before a custody change can never land after it. Refusals are `EIO` and counted (`data_dma_fence_refusals`, class split `data_dma_epoch_refusals`). Device reclaims (discard/punch) cease permanently on the same latch (`block_free_reclaim_fence_halts`) | A **remote** host's writes to the same data namespace. Nothing device-side stops them; the D0 guard is what keeps a second write mount from existing, and its own guarantee class (the table above) is therefore the real bound |
| **Multi-writer opt-in** (`SQUEEZEFS_MULTI_WRITER=1`) — PR-capable data namespaces, a format carrying **all six** capability bits, and an armed membership plane | NVMe/NVMe-oF with `RESCAP` support | Everything above **plus the device**: a **WERO (Write Exclusive – Registrants Only, rtype 3)** reservation is held on every data namespace for the mount lifetime, so an unregistered — i.e. fenced/preempted — host's writes are rejected **by the namespace**, while every legitimate registrant (the coordinator, enrolled remote job workers, co-writers) keeps writing. A dead epoch's PR preempt is the **drain proof** that releases its quarantined blocks. Gauge: `data_plane_fence_mode` = 1. Since **DLM S9** this row also carries remote write custody — see [Multi-writer data plane](#multi-writer-data-plane-dlm-stage-s9) | — |
| **Multi-writer opt-in** on anything else | loop devices (incl. `tests/dev_substrate.sh`'s default), file-backed volumes, any `RESCAP=0` namespace, a format missing one of the six bits, a membership plane that is off, or `SQUEEZEFS_MW_BIND=off` | **The mount is REFUSED, loudly, naming the namespace, the missing bit (and its offline stamping path), or the absent plane.** Multi-writer over a substrate that can only detect a rogue writer is not a supported configuration (spec §6.7 "On external consensus"). Volumes formatted since the rung-10b Phase-B flip carry the capability bits by default; pre-flip and `--single-writer` volumes land here until `squeezefs volume enable-multi-writer` upgrades them offline — and ANY volume on a non-PR substrate (loop devices, plain files, `RESCAP=0`) stays here regardless of its bits | — |

**Dead-epoch blocks are quarantined, not reused.** When a custody epoch dies (a remote worker's lease TTL fires, a client is proven dead), its blocks enter a **do-not-reallocate quarantine**: they are taken out of the free list, a terminal free of one *defers* its free-list publish, and only a **drain proof** releases them (the landed WERO preempt on a PR substrate; recovery's proof of death otherwise). Live gauge `dlm_quarantined_offsets`, flow `dlm_quarantine_releases`. Operational consequence, by design: **quarantined space is unavailable until the proof arrives** — a store with no free space outside the quarantine refuses `ENOSPC` rather than handing a possibly-live zombie's offset to a new owner. On a detection-grade substrate the cohort waits for the next mount's recovery walk (the job wire's documented `deferred-reclaim` class), so `dlm_quarantined_offsets` staying high there is expected, not a leak.

**Recovery runbook**, in order of automation — the refusal message always names the holder (`{id, pid, boot, age}`) and the exact remedy:

1. **Same-host crash**: nothing to do — the flock died with the process, and a dead-pid-proven claim (same boot, `kill(pid,0)` = ESRCH) is reclaimed automatically and instantly.
2. **PR-capable volumes**: a TTL-stale holder (> 45 s without heartbeat) is **preempted automatically** at the device; a fresh holder refuses loudly.
3. **Non-PR volumes after a cross-host crash**: automatic takeover is deliberately disabled (a paused holder cannot detect usurpation). Verify the named holder is truly dead, then clear the stale claim by operator attestation:

   ```bash
   squeezefs claim clear sqmeta://<meta_dev>
   ```

   The verb probe-mounts read-only, re-verifies staleness (refusing a fresh claim), and removes the record — the same live-check style as the format preflight.

**Fabric host identity (normative).** `/etc/nvme/hostnqn` and `/etc/nvme/hostid` are the **connect-time identity inputs**: created-if-missing by the `nvmeof connect` path, passed to nvme-cli and the `/dev/nvme-fabrics` fallback string, and read by the guard's `host_identity()`. They are **never the match authority**: when the register ladder must decide whether an existing PR registration is its own dead incarnation's, it matches on **`wire_host_id()`** — the host identifier the device itself reports for the live association (Get-Features FID 0x81) — because the wire identity was **measured diverging from the `/etc/nvme` files** on a real box (S1 session, 2026-07-17). Practical consequences: editing `/etc/nvme/hostid` changes what future connects present, not what the guard matches against; and only same-host stale keys (proven via the wire identity) are ever unregistered — foreign registrations always stay preempt/TTL/`claim clear` territory.

Live signals on the `.stats` inode: `writer_guard_mode` per volume (`flock+pr` = enforcement-grade | `flock+claim` = detection-grade | `flock` = a WRITE mount degraded read-only by unknown-ro feature bits | `reader` = an `-o ro` mount, which holds no lock and claims nothing | `co-writer` = a DLM S9 co-writer, which also holds no lock and claims nothing but DOES write data under a granted custody lease — see [Multi-writer co-writer mounts](#multi-writer-co-writer-mounts-dlm-stage-s9)) — alert on fleet drift, and note that the mount-wide `mount_posture` field is the one word that classifies the daemon itself; `writer_guard_fenced` (a fenced/usurped holder fail-stopped — working as designed, always investigate); `writer_guard_pr_reacquires` (the target dropped reservations, e.g. a PTPL-less power cycle — audit the fabric). Data-plane twins (DLM S7): `data_plane_fence_mode` (1 = device-enforced WERO held on every data namespace | 0 = detection grade), `data_dma_fence_refusals` and its `data_dma_epoch_refusals` split (**both must stay 0** — growth means a fenced or custody-stale writer tried to submit and was stopped; read beside `writer_guard_fenced`), and `dlm_quarantined_offsets` / `dlm_quarantine_releases` (dead-epoch blocks awaiting, and released by, a drain proof).

#### Multi-writer data plane (DLM stage S9)

**Status: armed and proven.** The plane is opt-in per mount
(`SQUEEZEFS_MULTI_WRITER=1`) and was proven at crucible discipline on the
single-node fleet rig — real daemons, real nvmet-tcp kernel target with PR
(`resv_enable=1`), kill-9 / freeze / failover matrices with the fsck + C8
oracle green after every kill (`.benchmarks/2026-08-16-mw-s9-arm.md`,
`.benchmarks/2026-08-16-mw-publish-era-gate.md`; evidence tier
measured-simulated — one box, co-located members; the S7 device-rejection
half is measured-real on the same target,
`.benchmarks/2026-08-16-mw-s7-arm.md`). Since the rung-10b Phase-B flip the
default `format` stamps the capability bits, so a fresh volume set passes the
format rung out of the box; a field deployment additionally needs PR-capable
data namespaces (rung 5 below — the arm refuses anything less). Design:
`docs/pre-rc-engineering-spec.md` §6.9 S9 / §6.7 +
`docs/design-full-multi-writer.md`; contracts
`tests/dlm_multi_writer_tests.rs`.

What it is: `SQUEEZEFS_MULTI_WRITER=1` arms three planes together —
metadata ownership (S8), the data-plane custody fence and its WERO hold (S7),
and **remote write custody**. A co-writer asks this mount's authority for
custody of a file (or one byte range), receives a **fencing token and a
custody epoch**, and then writes the bytes **itself, straight to the shared
namespace**. Only custody travels the wire; data never does.

| Question | Answer |
|---|---|
| What is granted | A lease the AUTHORITY holds on the co-writer's behalf — whole-file, or one `[start,end)` span. Two co-writers may hold **disjoint ranges of one file** at the same time; an overlapping exclusive span is **refused**, named, inside the caller's own wait budget |
| What authorizes a co-writer's DMA | Its **custody epoch** = `(durable writer term << 40) | custody generation`. Losing a grant **advances the generation**, so every submission authorized under the old one is refused at the single authorization point (`data_dma_epoch_refusals`) while the mount stays alive |
| How a revocation reaches the co-writer | **At its next renewal — this plane has no push backchannel.** The window is bounded twice: by the co-writer's own `T_self` (strictly earlier than the authority's TTL, at which point it fail-stops its own data custody), and by the **device**, which rejects a preempted host's writes. A revoked co-writer's *belief* can outlive the revoke by up to one renewal cadence; its *writes* cannot |
| What happens to a dead co-writer's blocks | The offsets it **declared in-flight on its last renewal** enter the S7 do-not-reallocate quarantine under one dead epoch. Release requires a **drain proof**: the landed WERO preempt of its registrant key, or an attested proof of death. A co-writer that published no registrant key can only be released by attestation — its space stays honestly unavailable (`dlm_quarantined_offsets`) |
| What the authority does on failover | A successor **bumps the durable term before arming** (an equal era is refused), opens a **grace window** that admits reclaim and refuses conflicting fresh acquires (`dlm_custody_grace_conflicts` must stay 0 on a healthy failover), and every pre-failover token is stale by construction |

**The format must say it can take a second writer.** The arm requires **six**
`features_incompat` bits, each one a §6.2 single-writer assumption whose
absence makes a second writer *unsound* — 7 (durable writer term), 9 (durable
block refcounts), 10 (writer-scoped staging), 11 (multi-writer data), 13
(`offset ‖ incarnation` block keys), 14 (the claim-set record). Bits 8 and 12
are deliberately **not** required: they express two appenders on ONE volume,
and ownership granularity is the volume. **S9 introduces no new bit.**

**The two blockers that once stood between this and a live cluster are both
closed** (kept here because older notes cite them):

1. ~~Nothing stamps the capability bits (ruling D9).~~ **Closed by the
   rung-10b Phase-B flip** (user ruling 2026-08-15;
   `.benchmarks/2026-08-16-mw-default-flip.md`): the default `format` stamps
   all nine multi-writer bits — `--single-writer` is the explicit opt-out —
   and pre-flip sets upgrade offline with
   `squeezefs volume enable-multi-writer` (one act, ordered, idempotent,
   crash-resumable). The flip gated on the stamped-solo S4 re-gate: solo
   mounts of a stamped format are within noise on every row class with
   `dlm_rpcs == 0`.
2. ~~The D0 guard still refuses a second write mount on every substrate.~~
   **Closed**: the co-writer posture and its admission gate exist —
   [Multi-writer co-writer mounts](#multi-writer-co-writer-mounts-dlm-stage-s9).
   The D0 gate itself was not weakened to do it (a co-writer enters through a
   second door that requires a five-rung admission decision).

Operator surface: `SQUEEZEFS_MULTI_WRITER=1` (arm; refuses loudly, naming the
missing piece), `SQUEEZEFS_MW_BIND` (`auto` — the default — / `addr:port` /
`off`, where `off` refuses rather than arming an inert mount). Live signals on
`.stats`: `dlm_custody.mode` (`off` | `authority` | `co-writer` | `both`),
`dlm_custody_held`, `dlm_custody_grants` / `_renewals` / `_releases` /
`_conflicts`, `dlm_revokes_issued` / `_expired`, `dlm_custody_unknown_leases`
(the pull-based revocation channel firing), `dlm_custody_self_fences`,
`dlm_custody_quarantined_offsets` / `_drain_proofs`,
`dlm_custody_generation` / `dlm_custody_epoch_advances`,
`dlm_custody_phase_ns` (rtt / arbitrate / adopt / renew), and
`meta_ship_publish.{shipped,local,served,refusals,owner_panics}` — where
**`refusals` and `owner_panics` must stay 0** on an armed mount (a refusal
means the ownership plane was armed without its publish half, which is
refused rather than executed locally). Every one of these is 0 with
`mode = off`, which is every mount that ships.

#### Multi-writer capacity planning — the data-plane allocation partition

**Status: the mechanism ships and is INERT on every mount today (a single
writer installs no partition at all). It engages only on a multi-writer mount
of a volume set carrying incompat bit 11 — the default format class since the
rung-10b Phase-B flip (`--single-writer` and pre-flip sets omit it).**
Design: `docs/design-mw-data-alloc-partition.md`; contracts
`tests/mw_data_alloc_lane_tests.rs`.

What it is: with `W` writers, a data volume's block index space is partitioned
by residue class — writer `w` allocates **only** blocks `w`, `w + W`,
`w + 2W`, … Two writers therefore never hand the same device offset to two
owners, with no arbitration and no message between them. **Frees are not
partitioned**: any writer frees any block (the owning lane is arithmetic, so a
free needs no lookup), and the block returns to the free supply of the lane
that owns it.

**The number to plan with.** A writer's own share of a device is exactly

```text
lane share  = ⌈(capacity_blocks − w) / W⌉        (shares differ by ≤ 1 block)
granularity cost = at most (W − 1) blocks of usable capacity, set-wide
reachability bound = capacity_blocks − Σ(owned lane shares)
                   ≤ capacity_blocks × (W − 1) / W  +  (W − 1)
```

**The width is rounded up to a power of two** (1, 2, 4, 8, 16 — what the
append-partition descriptor every partitioned structure runs on admits, with 16
writers the ceiling). Three writers therefore run at `W = 4`: plan on the
ROUNDED width, because the unassigned lane is unreachable in exactly the way a
live peer's is, and it comes back in full when the next writer is enrolled in a
new authority era.

Read both rows:

* **if every writer stays inside its share, partitioning costs `W − 1`
  blocks** — with the shipped 4 MiB block and 4 writers, 12 MiB per volume.
  That is the whole price of the partition;
* **a writer that needs more than `capacity/W` will be refused while the set
  still has space.** That is the reachability bound, it is published live as
  **`alloc_lane_stranded_bytes`** on `.stats`, and it is why capacity planning
  for a multi-writer set is per-writer (`capacity/W` each), not set-wide.
* **with the freed-offset grace period armed, a writer's share must also
  hold its churn in flight:** every block a rewrite displaces sits in the
  grace ring for the loop's hold time before it is reallocatable, so the
  lane law is

  ```text
  lane share needed = churn_rate × hold_time + live_share   must be  ≤  cap/W
  ```

  where `hold_time` is the writer's `free_grace_hold_ms` (≈ 8 s on the
  shipped cadences — six seconds of it the readers' derived qualify and
  drain windows) and `churn_rate` the co-writer's own displacement rate. The
  co-writer publishes the law live: **`alloc_lane_share_needed_blocks`**
  (`ceil(claim rate × refill horizon) + live`) and
  **`alloc_lane_headroom_pct`** (`(share − needed) ÷ share`, saturating at
  0) — a lane exhausts exactly when the headroom reaches 0, and
  `alloc_lane_enospc_refusals` follows. Worked: 4 GiB lanes (1,024 blocks),
  1.25 GiB live, 200 MiB/s of displacement at a 9 s hold ⇒ 450 + 320 = 770
  blocks, 25 % headroom — the 2026-09-06 s11 row's edge. Size the lane, cut
  the churn, or shorten the hold (`.benchmarks/2026-09-06-free-grace-hold-time.md`).

**The ENOSPC rule, in order.** An allocation tries (1) this lane's free list,
(2) this lane's virgin share, (3) a lane **adopted** because its holder was
proven dead, and then (4) refuses `StorageFull` **loudly, naming how many free
blocks belong to lanes it cannot reach**, and counting
`alloc_lane_enospc_refusals` (a **must-stay-0** tripwire). A live peer's lane
is never stolen: doing so would need arbitration this layer deliberately does
not perform, and a stolen offset could collide with that peer's own reuse.

**Adoption is the answer to a dead writer's space**, and its witness is the
same drain proof S7's quarantine demands — a landed WERO preempt of the dead
holder's registrant key on a PR substrate, an attested proof of death
otherwise. Adoption needs no durable record: the reservation watermark is
keyed on the **lane**, so allocating in an adopted lane raises that lane's own
watermark and any future holder of it recovers above us. `alloc_lanes_owned`
gauges what this mount may mint in; `alloc_lane_adoptions` counts the acts.

**What it costs at run time.** One durable metadata commit per
`SQUEEZEFS_ALLOC_LANE_RESERVE_BLOCKS` **fresh** blocks per lane — the
watermark that lets a successor resume above every offset its predecessor
*could* have minted, including an unpublished in-flight tail. **Reuse pays
nothing** (a freed offset is already dominated by the recovered floor), so a
rewrite-heavy workload adds zero commits. The default derives from the write
pipeline's cold window; `1` is an A/B control (a commit per block), never an
operational setting.

**Fragmentation note.** Contiguity-aware allocation (the VL4 evacuation mover,
VL7's D1/D2 axes, the W1 in-place patch) keeps working **within** a lane, one
stride coarser: a run of blocks a writer owns is `W`-strided rather than dense.
`frag_d1_contiguity` therefore reads lower on a partitioned volume by
construction — compare it against other partitioned mounts, not against a
single-writer baseline.

Three more capacity rows to plan with (verified against the arm campaign,
`.benchmarks/2026-08-16-mw-s9-arm.md` / `.benchmarks/2026-08-18-s11-mpiio-row.md`):

* **The width is fixed for the authority's era.** Enrolling another writer
  means re-arming the authority (a new era) — plan the roster before the
  arm, because a mid-era join is refused at the custody grant, never
  squeezed into a live partition.
* **Sustained rewrite reaches steady state through the lane free HARVEST**
  (see the co-writer section): a co-writer's displaced frees return to its
  own reachable supply, so rewrite workloads do not consume the virgin
  share monotonically. `alloc_lane_harvested_blocks` beside
  `alloc_lane_enospc_refusals == 0` is the healthy shape.
* **A reader-armed fleet under sustained rewrite churn couples capacity to
  reader acknowledgement**: every displaced free also waits out the
  freed-offset grace ring, and a storm whose deferrals outrun releases
  climbs `free_grace_offsets` toward lane-share ENOSPC (a measured shape —
  the program's residual board carries the pressure-coupled release valve).
  Watch `free_grace_alloc_stalls` on rewrite-heavy reader-armed fleets.
#### Multi-writer co-writer mounts (DLM stage S9)

**Status: the posture and its admission gate ship. Volumes formatted since
the rung-10b Phase-B flip pass rung 2 by default (the format stamps the nine
capability bits); pre-flip sets and `--single-writer` formats need the
offline `squeezefs volume enable-multi-writer` upgrade first.** Design:
`docs/pre-rc-engineering-spec.md` §6.2 item 7 (consumer half) / §6.9 S9;
contracts `tests/dlm_cowriter_tests.rs`.

A **co-writer** is a second write-capable mount of one volume set that holds no
metadata authority:

| Plane | A co-writer's authority |
|---|---|
| metadata | **none locally.** Every mutation is SHIPPED to the authority (the D0 claim holder). A local commit is refused, naming the shipped path (`cowriter.local_commit_refusals`) |
| data (DMA) | **yes, under a granted custody lease** — the bytes go straight to the shared namespace, authorized locally under the custody epoch. Only custody travels |
| fresh block **allocation** | **yes, from the lane its authority granted** — a residue class no peer mints in, whose durable reservation the authority commits ahead of every hand-out. See *What a co-writer can and cannot do* below |
| ownership accounting (terminal free / specific claim / W1 incarnation retire / device reclaim) | **none.** Which device offsets are OWNED is durable metadata on volumes it cannot commit to, so these are refused (`cowriter.accounting_refusals`) |

**The admission ladder, in evaluation order.** Every refusal names its rung,
what is missing and the remedy; nothing that mutates state (rung 5's device
registration) runs before every declarative rung has passed, and a later
refusal undoes it.

| Rung | Requirement | Why it is a rung and not a nicety |
|---|---|---|
| **1** | `SQUEEZEFS_MULTI_WRITER=1`, `SQUEEZEFS_MW_ROLE=co-writer`, `SQUEEZEFS_MW_AUTHORITY=addr:port`, and **not** `-o ro` | the posture is **declared, never inferred**. A plain `mount` of a claimed set still refuses `FreshForeign`, so no operator can acquire a second write-capable mount by accident |
| **2** | every metadata volume of the set carries all six capability bits — **14 (`KV_CLAIM_SET`) included** — and answers a **durable** `claim_set` record | a half-engaged set is not a claim set. Un-engaged, membership *is* the singular `writer_claim` read through a projection: a record that expresses exclusion and cannot represent a second member |
| **3** | that durable set names **this node** as a `writer` member | admission is by durable **enrollment**, written by the authority — never by a claim the joining node makes about itself |
| **4** | the membership plane is armed and this mount holds a **live** lease from the authority, whose era is not older than the set's `writer_claim` | a co-writer with no live authority has no custody source and, worse, **no evictor**: S6's eviction is what mints the dead epoch its in-flight offsets are quarantined under |
| **5** | the data namespaces hold a standing **WERO (rtype 3)** reservation and this node's key is among the device's **registrants** | §6.7's "refused on non-PR" governs the *admission* decision too. A co-writer the device cannot reject is a co-writer nothing can fence, and its death could never produce a drain proof — the proof *is* the preempt of this registrant key |

**Who writes the enrollment record (and why a co-writer cannot).** The
`claim_set` record is a metadata commit on ino 1, and a co-writer holds no
metadata authority — so it cannot enroll itself, and rung 3 is not a formality
it can satisfy by asserting. Enrollment is an act of the **authority** over its
operator-declared roster: `SQUEEZEFS_MW_MEMBERS=node_…,node_…` on the authority
mount, committed once per membership change at its multi-writer arm. That forces
the enrollment identity to be a **node** identity rather than a per-mount uuid,
because the entry must exist *before* the mount that would mint a uuid: it is
the writer-scope node token (`/etc/machine-id` and its ladder — the same
identity that scopes this node's staged payloads), rendered `node_{16 hex}`.

**Bringing one up.**

1. On the AUTHORITY: a normal write mount with `SQUEEZEFS_MULTI_WRITER=1`,
   `SQUEEZEFS_MW_BIND=<addr:port>` (or `auto`), `SQUEEZEFS_MEMBERSHIP_BIND`
   armed, and `SQUEEZEFS_JOB_WIRE_BIND` armed (it writes `job:enroll`, the
   cluster's root of trust). It refuses loudly if the substrate, the format or
   the plane is not ready.
2. On the CO-WRITER: mount with `SQUEEZEFS_MULTI_WRITER=1`,
   `SQUEEZEFS_MW_ROLE=co-writer`, `SQUEEZEFS_MW_AUTHORITY=<the authority's
   MW_BIND endpoint>`. It will be **refused at rung 3**, and the refusal prints
   this node's `node_{16 hex}` id.
3. Back on the AUTHORITY: add that id to `SQUEEZEFS_MW_MEMBERS` and re-arm, so
   the entry is committed durably. `squeezefs clients` and the census then show
   the node once it joins.
4. Remount the co-writer. On success its log says `CO-WRITER ADMITTED`, naming
   the authority claim, the era, the membership owner, the custody endpoint and
   the device registrant key; `.stats` reads `mount_posture: "co-writer"` and
   each volume's `writer_guard_mode` reads `co-writer`.

**What a co-writer can and cannot do (allocation).** Fresh **allocation
works**, from a lane the authority grants — the design record is
[`docs/design-mw-data-alloc-partition.md`](design-mw-data-alloc-partition.md)
§9a. What an operator needs to know:

* **the lane comes from the authority, on the custody lease.** There is no knob
  and there never will be one: two co-writers choosing their own lanes is the
  collision the partition exists to prevent. `alloc_lane_id` / `alloc_lane_writers`
  on `.stats` are what each mount was granted;
* **the width is the writer count rounded up to a power of two**, and it is
  fixed for the authority's era. Plan capacity on the rounded width — with an
  authority plus two co-writers, `W = 4` and a quarter of each data volume is
  unreachable until a fourth writer is enrolled (published live as
  `alloc_lane_stranded_bytes`);
* **enrolling a co-writer after the authority armed does not widen a live
  partition.** That node is refused at the custody join, naming the remedy: add
  it to `SQUEEZEFS_MW_MEMBERS` and **re-arm the authority** (a new era). A live
  writer's residue class cannot be redefined under offsets it has already
  minted;
* **every reservation a co-writer needs is committed by the authority.** The
  `alloc_lane:` record is a metadata commit and a co-writer holds none, so the
  raise ships and the authority writes it, validating that the lane is the one
  it assigned and that the frontier only rises. `alloc_lane_shipped_reservations`
  is that path's engagement gauge; `alloc_lane_raise_refusals` **must stay 0**
  (a refused raise means a write stalled loudly rather than using an offset no
  durable record covers);
* **a co-writer REWRITES, and its displaced frees SHIP.** A rewrite's new
  block comes from its lane; the layout publish (carrying the durable
  `TREE_BLOCK_REFS` delete for the displaced block) ships as before; and the
  displaced block's terminal free now travels as its own publish verb
  (`free_blocks`) that the **authority executes end to end** — RAM release,
  read-tier purge, reclaim queue, `finish_free` with the freed-offset grace
  period and the dead-epoch quarantine composing inside — exactly as if it
  had freed locally. The freed offset re-enters the free supply of whichever
  lane the arithmetic (`b % W`) names — and since rung 10 that supply is
  actually **reachable** by its lane's holder: at lane exhaustion a co-writer
  ships a lane free **harvest** (`harvest_lane_free`), and the authority hands
  back free-listed offsets of that lane (removed from its own list —
  exactly-once; recorded against the lease epoch so an epoch that dies with
  the reference not yet durable quarantines them until a drain proof, the
  same law fresh mints get from the durable frontier). Sustained rewrite
  therefore reaches steady state instead of leaking toward ENOSPC — read
  `harvest_shipped_blocks`/`harvest_served_blocks` on the publish ledger and
  `alloc_lane_harvested_blocks` beside `alloc_lane_enospc_refusals` (which
  must stay 0). Retries
  are safe (`(lease_epoch, request_id)` is answered exactly-once from the
  authority's dedup window), a fenced mount's in-flight frees are refused by
  era, and a free that never lands is the **leak-safe** direction: the block
  is already durably unreferenced, so the authority's next derivation (mount
  recovery / fsck C6) returns it — counted loud on
  `meta_ship_publish.free_ship_failures`, which should read ≈ 0;
* **the W1 in-place patch stays the authority's — by decision, not by gap.**
  The patch retires a block's *lifetime* (durable ownership state, spec §6.2
  item 6), and its clone/patch fence is a two-word process-local protocol no
  wire can compose; shipping the retire would also put a control-class round
  trip inside the one path whose entire win is "one DMA, zero metadata". A
  co-writer's small overwrite therefore rides **CoW-rewrite + shipped free**,
  and the local W1 arm keeps refusing (counted in
  `cowriter.accounting_refusals`). What that counter still covers, complete:
  the specific block claim (`allocate_specific_block` — lane-blind by
  design), the W1 incarnation retire, the ownership recovery walk, direct
  device reclaim, and any allocator-level free reached without the router
  (no product surface does). **Steady growth on a rewriting co-writer is
  therefore a bug**, not the honest gap it used to be. Reads and metadata are
  unchanged: served coherently, shipped respectively.

**A co-writer takes no lock, and denies the authority nothing.** It runs the S5
reader's *released* `flock(LOCK_SH)` probe — classification for the mount log
only — and retains nothing, so both mount orders work and **two co-writer mounts
on one host are legitimate** (their mutual exclusion is the authority's custody,
not a local lock). It is also invisible to the recovery ladder as an owner:
`squeezefs clients`' live/stale/dead classification, the dead-pid proof, the PR
preempt arbitration and `squeezefs claim clear` all read evidence a co-writer
never writes (no `writer_claim`, no retained flock, no metadata-namespace
registrant). What it *does* leave — a data-namespace registrant key and a
claim-set member entry — is deliberate: those are what make it fenceable and
visible.

**Coherence.** A co-writer's metadata view is a **reader's** view: the state of
the most recent checkpoint it has polled, with the same derived cadence and the
same `reader_staleness_bound_ms` (§6.8 item 2's revalidation and item 5's purge
are both armed for it). Its own mutations are never stale, because they execute
on the authority. Everything the reader section says about DATA staleness
applies verbatim — bounded with the membership plane off, eliminated with it
armed (§6.8 item 3, the
[freed-offset grace period](#freed-offset-grace-period-spec-68-item-3)).

**Failure and re-admission (the rung-10 posture, stated honestly).** A
co-writer that loses its custody — its renewal meets `UnknownLease` after a
revocation or an authority failover, or its own `T_self` deadline fires first
— **self-fences: it poisons process data custody and is dead until remount.**
Re-admission after an authority failover is **by remount, deliberately**: the
successor's era re-enrolls the roster, and a fresh mount re-runs the five-rung
ladder and joins under a fresh lease epoch. *Automatic in-place re-admission
was considered at rung 10 and deferred*, because it would require a production
path that clears the sticky custody poison — and "a fenced holder is dead
until remount" is a load-bearing safety law, not an implementation accident:
the poison is what guarantees that **no DMA authorized in the dead era can
ever land**, every gate in the tree (`authorize_dma`, the reclaim queue's
fence halt, the write pipeline's fence drop) stands on its one-way latch, and
a fenced mount also holds fenced-era state a resume would have to prove
coherent (staged custody, layout caches, local locks minted under the dead
era). Un-poisoning is therefore a designed transition of its own — it needs
the S6/S7/S9 planes' adjudication, not a rung's convenience patch. Until it
lands: run co-writers under `mount --daemon --supervise` or an external
supervisor and treat a `membership_self_fences`/`dlm_custody_self_fences`
increment as "remount this mount"; the remount is cheap (the slot id is
mount-point-stable, so the roster still names it) and the fleet rig's
crucibles exercise exactly this path.

Since the finding-#6 fix (`docs/design-mw-layout-versions.md` §6a) the
publish plane itself is a fence channel: **every mutating shipped publish
verb carries the mount's lease epoch, and a swept era's publishes REFUSE on
the authority with nothing applied** (`meta_ship_publish.stale_refusals`).
A refusal for the mount's *current* epoch composes the same self-fence as a
failed renewal — the zombie learns it is dead at that round trip. Its
**acked-un-fsynced writes are the POSIX crash class**: with the poison latch
set, constant-writeback units resolve as verified fencing-stale no-ops
(`writeback_fence_noops`) instead of retrying forever; the staged bytes stay
on disk for the remount's staging recovery ("stale fencing tokens discard
staged work" — the remount contract), and `fsync` on the fenced mount keeps
failing loud, so no application is ever told discarded data was durable.

**Live signals on `.stats`:** `mount_posture` (`writer` | `reader` |
`co-writer`) and the `cowriter` object — `mw_role`, `admissions`,
`admission_refusals`, `accounting_refusals`, `local_commit_refusals`,
`custody_endpoint` — read `accounting_refusals` beside the `alloc_lane_*`
family: since the allocation-lane grant AND the shipped free path it counts
only the arms that stay local by decision (the specific claim, the W1
retire, the recovery walk, direct reclaim), so on a rewriting co-writer it
should be FLAT. **`local_commit_refusals` should stay 0** on a healthy
co-writer: nonzero means a daemon surface still commits metadata directly
instead of shipping it (S8's "the daemon is not switched onto the router" gap
meeting a real workload). The free path's own rows ride the publish ledger
(`meta_ship_publish.*`): `free_shipped_blocks` is the rewrite-engagement
instrument (its delta must account for a rewrite workload's displaced
blocks), `free_served_blocks` the authority's executed half,
`free_refused_blocks` the authority's `Refused` verdicts (a genuine double
release — the block was already free, graced, quarantined, mid-reclaim or
names a dead lifetime; `served + refused + non-terminal ≡ the peers'
shipped`, and the authority's log names the class per refusal; a STREAM of
them beside a healthy rewrite is a duplicate free ISSUER on a peer, never a
leak — the 2026-09-06 s11 row's 3,449 were one own-mint map blob reclaimed
≈ 7× per save, `.benchmarks/2026-09-06-cowriter-free-refcount-leak.md`),
`free_recomputed_blocks` the displaced blocks the authority freed itself on
recomputed publishes (since that note the caller frame's RAM-only lifetimes
included — a binding minted, displaced and never persisted in between,
which the head→composed diff cannot name), `free_replays` the exactly-once
witness engaging (a lost-reply retry landing here is the mechanism working),
`free_stale_refusals` the era fence firing around a revocation, and
`free_ship_failures` ≈ 0 (each is a leak-safe unreturned-until-recovery
offset). The co-writer's own blob hygiene reads on
`publish_blob_orphan_reclaims` (own-mint map blobs whose lineage the
mount closed) beside `publish_blob_orphan_reclaim_dedups` (re-arms of an
already-closed lineage the router DECLINED — a stale clone of a reclaimed
entry re-inserted and discarded again; every one of these used to ship a
free the authority refused), and its never-published mints on
`cowriter.unpublished_recycles` (a live laned co-writer's superseded overlay
destination / failed-publish upload returned to its OWN lane free list — no
ledger anywhere ever named it) beside `cowriter.unpublished_abandons`, which
now grows only around custody loss or on an offset outside this mount's
lanes. Read them beside the custody ledger (`dlm_custody.*`), the rest of
the publish ledger (whose `refusals` must stay 0) and
`data_plane_fence_mode` = 1.

**What an end-to-end two-host write needs, plainly.** The machinery is
complete — lane-granted allocation, custody-authorized DMA, shipped
publishes (era-gated and witnessed), and the displaced block freed through
the authority's full ladder with the offset reusable by its lane's owner
(ruling D8's mixed rewrite workload is expressible) — and it was **proven
end to end on the single-node fleet rig** (N real daemons, real nvmet-tcp
kernel target with PR, plus a qemu/KVM guest member as an independent
kernel and clock domain — `.benchmarks/2026-08-16-mw-s{6,7,8,9}-arm.md`).
What a FIELD deployment needs:

1. **a multi-writer-capable format** — the default class since the rung-10b
   flip; pre-flip and `--single-writer` sets upgrade offline with
   `squeezefs volume enable-multi-writer`;
2. **real PR namespaces.** Rung 5 demands a standing WERO (rtype 3)
   reservation whose registrants include this node, on namespaces that
   actually advertise `RESCAP` — loop devices and plain files do not.
   Without the device half a co-writer can be detected but not rejected,
   and its death can never produce the drain proof that releases its
   quarantined offsets. (nvmet-tcp with `resv_enable=1` qualifies — it is
   exactly what the proving fleet runs.)

What a second *physical* machine adds beyond the proven single-node fleet is
real NIC/fabric congestion physics (a perf concern, not a correctness one —
design §12), and the honest residuals that remain are: the same-host shared
PR-identity shape in the guarantee-class table (split it with per-mount
`SQUEEZEFS_HOSTNQN`/`SQUEEZEFS_HOSTID` on an sqz kernel — patch 0030,
`docs/design-mw-multipath-kernel.md`), and the allocation residuals in
[`docs/design-mw-data-alloc-partition.md`](design-mw-data-alloc-partition.md)
§9a.6 (lane-scoped lifetime stamps, and a co-writer declaring its in-flight
destinations so S7's quarantine covers them).

#### Byte-range custody (DLM stage S11)

**Status: built, proven on the §9.5 acceptance rows, and shipped
`SQUEEZEFS_RANGE_CUSTODY=off` — arming it is an explicit per-mount act.**
This is the MPI-IO shape: two or more co-writers hold **disjoint byte ranges
of ONE file** and write them concurrently. Design:
`docs/design-full-multi-writer.md` §9; contracts
`tests/dlm_range_custody_tests.rs` + `tests/mw_authority_assembler_tests.rs`;
loom `range_custody_core` (4 models, weakening-verified); evidence:
`.benchmarks/2026-08-17-s11-range-wire.md` (the wire),
`.benchmarks/2026-08-17-s11-b4-clause.md` (the composed fast-path refusals),
`.benchmarks/2026-08-17-s11-authority-assembler.md` +
`.benchmarks/2026-08-17-s11-zeros-interleave-fix.md` (the assembler),
`.benchmarks/2026-08-18-s11-mpiio-row.md` +
`.benchmarks/2026-08-18-s11-widthn-refs-fix.md` (the acceptance rows).

**Arming it.** On an armed multi-writer fleet (authority + admitted
co-writers), set `SQUEEZEFS_RANGE_CUSTODY=1` on the co-writer mounts. Set
while the multi-writer plane is NOT armed it is **announced-inert** at mount
(a startup notice; every `range_custody_*` gauge stays structurally 0) —
never a refusal. The fleet rig arms it with `SQZ_MWFLEET_RANGE_CUSTODY=1`.

**What is granted.** **EX ranges only** in v1 (whole-file EX stays the
default; CR/CW modes are built but not issuable — KD-MW-9). A range grant is
a **sub-grant of the S9 custody lease**, arbitrated by the file's authority,
cached client-side, revalidated by the range vector every renewal reply
carries. Ranges share the FILE's fencing generator — no new token algebra —
and a range writer fences on its **own** lease token.

- **Required vs desired**: the acquire carries `required` (the write's exact
  span — **never silently trimmed**; a refusal is loud or the grant covers
  it) and `desired` (block-aligned outward, stretched only along a
  sequentially-advancing write frontier or an abutting own span — the v2
  stretch, so strided/block-cyclic decompositions never fabricate cross-mount
  conflicts). Adjacent same-holder grants **coalesce at admit** (4,096
  byte-granular asks converge to ~2 live spans, measured).
- **Budgets and refusals — no free constants**: the per-file span cap is the
  file's own geometry (`max(16, ceil(size / block_size))`); the real ceiling
  is the R5 byte budget `dlm_grant_table_bytes` (derived `R5/256`, floor
  16 MiB — sized so even a 1 TiB block-cyclic decomposition fits). At budget
  the acquire **refuses loud naming the arithmetic**, counted
  `range_custody_cap_refusals` — which must stay **0** on any within-budget
  shape (the block-cyclic acceptance row pinned it: grants ≈ blocks-in-file,
  zero refusals).
- **The whole-file fast path is structurally untouched**: a file with zero
  live range grants probes an empty table O(1); on a solo mount the plane is
  unreachable (no verb issues ranges without the arm). The single-writer
  41.6 GiB/s row is guarded by the fast-path tax row on every S11 rung.

**Block-aligned ranges DMA directly; sub-block sharing demotes to the
authority.** The block is the unit of CoW, refcount and overlay, so when two
live grants share ONE block the block becomes **authority-assembled**: both
holders ship their writes to that block as extents
(`meta_ship_publish.extent_*` ledger — `shipped ≡ served` is the engagement
law), the authority merges via the extent overlay and publishes once, single
publisher per block at every instant. The transition is barriered **at grant
issuance**: the overlapping grant is not issued until the incumbent
acknowledged the demotion (the notice rides the incumbent's next
custody-channel reply — acquire, release, or renewal, so a churn-shaped
incumbent hears within one interaction; the renewal is the worst-case
carrier for a quiet one) or its lease expired on the authority's clock —
the closed ledger is
`range_custody_demotions ≡ demotion_acks + demotion_fence_resolves`
(healthy fleet: `fence_resolves == 0`), with
`range_custody_demotion_fenced_publishes` the ≈ 0 crash-window tripwire.

**Demotion is reserved for TRUE sharing — a stretch-tail overlap SHRINKS
instead (§9.3a, 2026-08-19).** A grant remembers the union of the REQUIRED
spans it was asked for; a peer's required landing only in the
desired-minted stretch beyond that union marks the grant *shrink-pending*
(`range_custody_tail_shrinks`) — the incumbent's next custody-channel
reply carries the notice (acquire/release/renewal — widened from
renewal-only by finding 16 half (a), because block-cyclic grants churn
faster than a renewal cadence and 40 of 51 notices were dying with their
grant), its client stops serving the tail, answers its written high-water,
and the tail is released: the asker gets exclusive custody with **no
demotion and no shared clauses**. Only an incumbent that truly wrote into
the contested tail escalates to the demotion barrier
(`range_custody_shrink_demotions`, ≈ 0 on disjoint workloads). The closed
ledger is `range_custody_tail_shrinks ≡ tail_shrink_acks +
tail_shrink_fence_resolves` (healthy fleet: `fence_resolves == 0`), and the
client learns a per-file stretch ceiling from each shrink notice AND from
the acquire reply's own desired trim (finding 16 half (b) — the trim
teacher, so the lesson lands even when no shrink round ever runs)
(`range_custody_stretch_ceiling_clamps`) so a steady block-cyclic
interleave pays at most one shrink round per custody episode.
A shipper **retains** each extent until the covering layout version is
visible (release is pull-only: ack-carried version, renewal observation,
the synchronous `FlushExtents` an `fsync` forces, or the at-budget W2
spill), so an authority death between ack and publish loses nothing acked —
**a sub-block writer's `fsync` chains through the authority's publish
barrier**. Priced as the D1 exception path: ≈ 3 ms authority CPU per extent
upper bound vs the aligned rows' direct DMA
(`.benchmarks/2026-08-18-s11-mpiio-row.md` §4). The W1 in-place patch and
the B4 device overlay both refuse a range-shared span
(`patch_ineligible_range_shared` / `overlay_ineligible_range_shared` — one
custody core, two ledger buckets; both 0 on aligned rows).

**The measured verdicts** (pinned ior 4.0.0, 8 mounts × 4 procs = 32 ranks,
one shared file, tcp devsub; evidence tier measured-simulated):

| Row | Verdict |
|---|---|
| MPI-IO acceptance (shared vs file-per-proc, A-B-B-A) | **MET** — ≥ 0.8× in both brackets (1.411× / 2.273×); engagement exact, zero cap refusals/conflicts/demotions on the aligned decomposition; read-back exact; cold fsck findings 0, C8 drift 0 |
| Block-cyclic (the never-coalescing legitimate shape) | grants ≈ blocks, **zero** cap refusals, table bounded (peak ~1.9 KB vs a 345 MB budget), 1.52–2.5× its disjoint control; fsck/C8 green ×3 from zero |
| Adversarial tiny-ranges (bounds falsifier) | 4,096 byte-granular unaligned writes in 0.78 s → 2 grants + 15 extensions; foreign-client latency 1.02× |
| Demotion barrier + sub-block price | `demotions 1 ≡ acks 1`, fenced publishes 0, retention → 0 at quiesce; fsck/C8 green ×3 from zero |
| Range kill matrix | kill -9 holders and the authority mid-assembly: quarantine covers, peers unaffected, retained extents re-ship idempotently, `dlm_custody_grace_conflicts == 0`, oracle green every round |

**Why the default stays OFF, and the one boundary to know.** The composed
map of a shared file past the inline cap (**≈ 6 GiB at the shipped 4 MiB
block**) spills to an `indirect:` head, and concurrent chained publishes
onto one **refuse fail-safe** — the writer's `fsync` fails loud (EIO,
retried-class), the volume is never poisoned, fsck stays clean — until the
blob-aware owner-side merge lands (the program's named rung-20 residual,
`.benchmarks/2026-08-18-s11-widthn-refs-fix.md`). The default flip is gated
on that composition plus ×3 from-zero re-runs of the row gates on the
flipped default; until then, arm it explicitly on fleets whose shared files
fit the inline-map domain.

**Live signals** (`.stats`): the `range_custody` object —
`grants` / `extensions` / `covered_serves` / `releases` / `active` /
`conflicts` / `waits` / `desired_trims` / `cap_refusals` (must stay 0
within budget), the demotion family
`demotions` / `demotion_acks` / `demotion_fence_resolves` /
`demotion_wait_ns` / `demotion_fenced_publishes` (≈ 0), the §9.3a
tail-shrink family `tail_shrinks` / `tail_shrink_acks` /
`tail_shrink_fence_resolves` (`shrinks ≡ acks + fence_resolves`) /
`shrink_demotions` (≈ 0 on disjoint workloads) /
`stretch_ceiling_clamps` (client side), and
`dlm_grant_table_bytes` (authority side, an R5 component that refuses
admission rather than shedding custody); client-side
`dlm_custody_range_{acquires,extensions}` and
`dlm_token_cache_range_spans` counted into `dlm_token_cache_bytes`; the
extent plane on `meta_ship_publish.extent_{shipped,served,replays,
stale_refusals,retained_bytes,flush_forces,spills}` (`retained_bytes` → 0
at quiesce); and the two fast-path refusal ledgers
`patch_ineligible_range_shared` / `overlay_ineligible_range_shared`.

### Read-only coherent mounts (`-o ro`) — one writer plus N readers

**Status: the mount mode and its metadata coherence both ship. The DATA-block half now has two postures, and which one you get depends on one knob — read the guarantee below before you rely on it: with the membership plane armed the stale-serve window is *eliminated*; with it off (the default) it is *bounded*, exactly as before.**

A read-only mount is a *reader*: `squeezefs mount --read-only …` or `-o ro`. It takes no write lease, writes no `writer_claim`, registers no NVMe reservation and spawns no checkpoint task, so **one writer plus N readers of the same volume set is a supported shape** — and, symmetrically, a reader is never an obstacle to the writer (see the `reader` row in the guarantee-class table above). Design: `docs/pre-rc-engineering-spec.md` §6.8, DLM stage S5.

```bash
# on the writer host (unchanged)
squeezefs mount sqmeta://<meta_dev> /mnt/sqz --data-lv <data_dev>

# on any number of reader hosts sharing the same namespaces
squeezefs mount --read-only sqmeta://<meta_dev> /mnt/sqz-ro --data-lv <data_dev>
#   equivalently:  -o ro
#   `-o ro` together with an explicit `-o rw` is refused loudly
```

**What a reader refuses.** Every plane, at three independent gates: the kernel (the filesystem is mounted `MS_RDONLY`, so the VFS returns `EROFS` before the daemon is involved), the FUSE handler surface (`EROFS` — this is the gate that also covers `LD_PRELOAD`/SDK ring writes, which never traverse the VFS), and the data plane (block allocation, terminal frees, device reclaim/discard, the W1 in-place patch and the in-place-overwrite lever all refuse). The writeback cache is off, and the writer-side background engines (writeback flusher, extent-record recovery, fold worker, reclaim pool, job fabric, job wire, defrag gauges, block-ownership recovery) are not armed.

**Consistency guarantee — stated, not implied.**

| Plane | What a reader sees |
|---|---|
| **Data blocks** | Consistent as of a revalidation epoch. Every epoch that observes the writer's roots advance drops the reader's whole block-key census (all five block-key stores + the read-lane hold). What that alone gives you is a stale serve **bounded by one revalidation interval**; the **freed-offset grace period** (spec §6.8 item 3) closes the window outright, and it is armed with the membership plane. **Plane armed** (`SQUEEZEFS_MEMBERSHIP_BIND` set on the writer, and this reader joined — check `free_grace_mode == "armed"` on the writer's `.stats` and `membership_mode == "member"` on the reader's): a block the writer frees is **not reallocatable until every live reader has acknowledged passing it**, so no interval exists in which a reused offset can serve another file's bytes; the residual risk is a reader that stops acknowledging, which is **fenced** (evicted) rather than waited on and counted in `free_grace_forced_releases` / `free_grace_laggard_fences`. **Plane off (the default)**: the pre-item-3 statement stands verbatim — within one revalidation interval a block the writer freed and reallocated to a different file can serve the other file's bytes, loudly on a transformed (compressed/encrypted) volume because the AEAD tag fails, **silently on a passthrough volume, which is the default**. See [Freed-offset grace period](#freed-offset-grace-period-spec-68-item-3) |
| **Metadata** | The state of **the most recent checkpoint the reader has polled** — bounded staleness, not a frozen snapshot. Each poll reads the volume's newest A/B root-ledger record; a newer record is adopted, the cached nodes the new roots do not cover are dropped, and the reader serves the new epoch. The bound is `reader_staleness_bound_ms` on the `.stats` inode (the poll interval plus the writer's ≤ 1 s checkpoint ceiling — 2 s with the shipped defaults), and it is machine-readable precisely so this paragraph cannot drift from the number in force. Epochs are **monotone** (a reader never moves backwards). Two things are deliberately *not* promised: a reader observes only what the writer has **checkpointed**, so committed-but-not-yet-checkpointed transactions are invisible (that is what makes the model cheap — no journal replay on the read side); and a single multi-key operation is not atomic across a poll, so a `readdir` spanning one may mix two adjacent checkpoints (read `meta_kv_revalidate_epochs` before and after, seqlock-style, if you need one epoch for a whole operation) |
| **Membership** | A reader does **not** appear in `squeezefs clients` — the `client:` registration is a metadata write, and a reader performs none. Reader visibility is the S6 membership plane's job |

**Revalidation cadence and TTLs are derived, not configured.** The reader polls each metadata volume's A/B root ledger (one ledger read per volume per pass) on the derived cadence `max(SQUEEZEFS_META_FLUSH_INTERVAL_MS, 1 s)` — strict mode (`0`) reading as the checkpoint task's own tick — because polling faster than the writer's checkpoint guarantee cannot reduce staleness (records do not exist to be found) and pays a node drop pass for it. Live as `reader_revalidate_interval_ms`; the staleness bound it implies is `reader_staleness_bound_ms` = interval + the ≤ 1 s ceiling.

That **bound** — not the raw cadence — is the DEFAULT for every kernel cache TTL (`attr`/`entry`/`dir_entry`/`negative`) and for the daemon dentry, attr and parent-memo caches, whose shipped 300 s horizon is cut to it (never lengthened past 300 s): a cache may not hold an entry longer than the interval over which the reader can prove freshness, and holding it for exactly that long is free. Explicit `SQUEEZEFS_FUSE_*_TTL_MS` and `-o *_timeout=` values still win verbatim — the standard precedence — which also means **an explicit value lengthens the staleness window by exactly the amount it exceeds the bound**.

**Live signals** (`.stats`). Posture and the two derived numbers: `read_only_mount`, `reader_revalidate_interval_ms`, `reader_staleness_bound_ms`, plus `writer_guard_mode == "reader"` per volume. Activity is the `meta_kv_revalidate_*` family — all of it 0 for the life of a write mount, so nonzero values ARE the statement "this mount is a coherent reader":

| Signal | Healthy reading |
|---|---|
| `meta_kv_revalidate_polls` | grows once per volume per interval — flat means the cadence task is not running |
| `meta_kv_revalidate_epochs` | grows while the writer checkpoints, flat while it is idle (an inert poll is free by design) |
| `meta_kv_revalidate_nodes_dropped` | **the inverted tripwire:** epochs advancing while this stays flat means the drop pass is finding nothing to drop, i.e. the reader is not actually re-reading the writer's tree. An armed reader that observes an advance must drop |
| `meta_kv_revalidate_keys_purged` | same shape for the DATA half: epochs advancing on a mount that has cached blocks, with this flat, means the R-6 purge is not reaching the tiers — a coherence promise silently not kept |
| `meta_kv_revalidate_stale_serves` | a node was served from an older epoch than the one now in force — expected in small numbers around a step (a traversal that began before it), sustained growth means something holds a long-lived node reference |
| `meta_kv_revalidate_dirty_skips`, `meta_kv_node_partition_refusals` | **must stay 0** on every posture: the first means revalidation was armed on a mount that writes, the second that an appender reached for a node population it does not own |

**What a reader does not weaken.** Nothing about the single-writer guard: the writer's `flock(LOCK_EX)` acquisition, its fresh-foreign-claim refusal, its PR arbitration and its recovery runbook are byte-identical with readers attached, and a second writer is refused exactly as before. The reader's own `flock(LOCK_SH)` is a *released probe* used only to log whether a local exclusive holder exists — a retained shared lock would conflict with the writer's exclusive one and let a reader deny a legitimate write mount, which is the opposite of the intent.

### Format v3 (CoW KV metadata)

Metadata volumes format as **v3**: a copy-on-write, typed key/value btree (bcachefs-style 256 KiB CoW nodes + a logical reservation journal + background checkpoints). Full design: `docs/design-cow-kv-metadata.md`; measured gates: `.benchmarks/2026-07-09-kv-v3-gates.md`.

Capacity/scale: ≥ 100 M inodes per volume, 1 M+ entries per directory, unlimited xattrs (values up to `min(64 KiB, node_size/4)`), and O(active-set) mount time (a 100 M-inode volume cold-mounts in ~22 ms on the reference box).

**v3 tuning knobs** (format-time and mount-env):

- `--meta-node-kib <64|128|256|512|1024>` (format): btree node size, default `256`. Below 256 the per-volume record-value cap becomes `node_size/4` and a warning prints, spilling large xattrs / layout maps to the indirect mechanism sooner — leave at 256 unless cold-read latency on tiny-record workloads dominates.
- `--meta-journal-mb <MiB>` (format): journal ring size; default `clamp(volume/64, 8 MiB, 32 MiB)`.
- `SQUEEZEFS_META_NODE_CACHE_MB` (mount env): RAM budget for the demand-paged node cache, absolute MiB — explicit wins verbatim (the `512` spelling restores the pre-sweep flat default). `SQUEEZEFS_META_NODE_CACHE_PCT` is the percentage spelling (percent of the resolved memory budget, clamp (0,100]). Default with neither set = `max(budget/16, 512 MiB)` per volume (derivation sweep 2026-08-04 — the budget itself is machine-derived, so the fraction is scale-free; 512 MiB is the shipped never-regress floor). Precedence absolute > percentage > derived.
- `SQUEEZEFS_META_CHECKPOINT_MAX_DIRTY_NODES` (mount env): dirty-node checkpoint cap; bounds the mount-replay working set. Explicit wins verbatim; default derived = `max(4096, budget/32 ÷ node_size)` (derivation sweep 2026-08-04 — 4096 is the shipped never-regress floor).
- `SQUEEZEFS_META_FLUSH_INTERVAL_MS` (mount env): the journal/checkpoint cadence — `0` = strict per-commit durability.

> **Legacy format v2**: support was removed entirely (always forward — no backwards compatibility). A v2 superblock refuses to mount with a precise "no longer supported; reformat required" error; `squeezefs format --force` reformats such a volume to v3 (destroying the old contents). The offline `squeezefs migrate` v2→v3 converter was deleted along with v2 support.

### Read-only coherent mounts — the stated consistency model (metadata)

**Not a shipped mount mode yet.** The shipped product is one mount per volume set (the guard above). This section states the consistency model the *machinery* now provides, because the model is the deliverable an operator has to be able to read before the mode ships: the metadata revalidation path (pre-RC engineering spec §6.8 item 2) is built, the mount option that arms it (`-o ro`) is not. A mount either declares itself a coherent reader — after which it **may not write**, enforced, not documented — or is a normal write mount, for which every mechanism below is inert.

**What a coherent reader sees: the state of the most recent checkpoint it has polled.**

| Property | Statement |
|---|---|
| **Bounded staleness** | A reader re-reads the writer's A/B root ledger (one 128 KiB read) every `SQUEEZEFS_META_REVALIDATE_MS`. A write mount publishes a ledger record at the end of every checkpoint cycle that had work — at most 1 s apart under load (the §4.6 pt 2 ceiling), and **never** while idle. Worst case a record lands just after a poll and is seen at the next one, so the bound is **poll interval + 1 s** (≈ 2 s with defaults). |
| **Monotone** | Epochs only advance. A reader never moves backwards and never loses a record it has already served. |
| **Per-operation atomicity** | Every node a reader resolves belongs to exactly one checkpoint epoch, and an operation that *starts* after a poll sees only that epoch's nodes. A single multi-key operation that spans a poll (a long `readdir`) may mix two adjacent checkpoints; a caller that needs one epoch across several steps reads the live epoch before and after (seqlock-style) and retries on a change. |
| **Not durability, not linearizability** | A reader observes only what the writer has **checkpointed**. Committed-but-not-yet-checkpointed transactions (up to the flush cadence) are invisible by design — that is what makes the model cheap: no journal replay on the read side. |
| **Metadata only** | Metadata coherence does **not** by itself make data-block bindings coherent: an epoch step fires the unified block-key purge, which bounds the window to one interval. Closing it is the freed-offset grace period (spec §6.8 item 3), which **is built** and arms with the membership plane — see the Data-blocks row of [Read-only coherent mounts](#read-only-coherent-mounts--o-ro--one-writer-plus-n-readers) for both postures. With the plane **off** (the default) the older statement still holds: a reader of a volume whose writer frees and reallocates blocks can serve another file's bytes for a block whose binding it cached — loudly on a transformed (AEAD) volume, silently on a passthrough one. |
| **Cost** | A poll that finds nothing new costs one ledger read plus ~6 ns of bookkeeping. A poll that finds a newer record drops the reader's cached nodes and re-demand-pages the working set: ≈ 575 ns per cached node (≈ 1.2 ms for a 2,048-node working set), i.e. ≈ 0.1 % of one core at the 1 s cadence. Measured: `.benchmarks/2026-08-05-mw-node-cache-coherence.md`. |

`SQUEEZEFS_META_REVALIDATE_MS` (mount env, reader only): poll cadence in ms. Explicit wins verbatim. Default **derived** = `max(flush cadence, 1000)` — polling faster than the writer mints records cannot reduce staleness and pays a drop pass for nothing; a larger value trades staleness (bound above) for fewer drop passes and a warmer cache.

Live signals on the `.stats` inode — all eight are **0 for the whole life of a write mount**, so a nonzero value is itself the statement "this mount is a coherent reader":

- `meta_kv_revalidate_polls` / `_epochs` — polls performed vs polls that found a newer record. `polls` growing with `epochs` flat is the designed idle-writer steady state.
- `meta_kv_revalidate_nodes_dropped` — the reader's reload bill; `nodes_dropped / epochs` is the live working-set size. Approaching the whole cache every poll means the cadence is finer than this workload wants.
- `meta_kv_revalidate_stale_serves` — hit-path rejections of a node stamped in a superseded epoch (normal under a busy writer).
- `meta_kv_revalidate_dirty_skips` — **must stay 0**: a drop pass met un-durable RAM records, i.e. revalidation was armed on a mount that writes.
- `meta_kv_revalidate_keys_purged` — block keys the epoch step purged. 0 while `epochs` grows means the data-plane trigger is not wired (honest and visible, not a silent hole).
- `meta_kv_reader_load_retries` — extent reads a reader re-tried because they raced the writer's in-flight append (bounded per load; normal on a hot volume).
- `meta_kv_node_partition_refusals` — **must stay 0**: an appender reached for a node population it does not own (a non-authority structural mutation, an armed reader's write attempt, or an append whose destination page already held a peer's frame). Every one of these is silent divergence prevented.

### Cross-volume namespace operations (multi-volume metadata sets)

On a metadata set with **more than one volume**, `link`, `unlink`/`rmdir`
and a `rename` across parents can touch two (or more) volumes: the child's
link count lives on the volume its inode routes to, the directory entry on
its parent's. "One transaction = one checksummed journal entry" is a
**per-volume** guarantee, so these ops are a small **distributed
transaction** (DLM stage S3.5; design `docs/design-cow-kv-metadata.md`
§4.10a). Single-volume sets never enter this path and behave exactly as
before.

What an operator needs to know:

- **A crash mid-operation is completed at the next mount, not left behind.**
  The first commit carries a durable *intent record*; the next write mount
  rolls the transaction forward before it serves anything, so a `stat`
  after a crash shows the state before the operation or the state after it —
  never a half-state. Before S3.5 the half-states were permanent: a link
  count of 1 with no directory entry (invisible AND unreclaimable — space
  that `df` never gets back), or a directory's link count drifting so
  `rmdir` either succeeded with children present or refused forever.
- **The mount says so, loud.** Recovery logs
  `cross-volume transaction recovery: N open intent(s) found at mount` and
  one line per completed transaction. `N` is normally 0.
- **A mount refuses rather than serve a half-applied transaction it cannot
  read**: an intent whose record does not decode (real corruption — a torn
  one cannot exist) fails the open naming the transaction id and pointing at
  `squeezefs fsck`. An intent written by a NEWER binary refuses the same
  way rather than guessing its plan.
- **Cost**: one extra small journal entry, and up to two coalesced device
  flushes, per *cross-volume* op. On a strict-durability mount
  (`SQUEEZEFS_META_FLUSH_INTERVAL_MS=0`) the flushes are already paid per
  commit and are skipped. There is **no knob**: the ordering is what makes
  the guarantee true.
- **A device error part-way through** returns the error to the application
  AND fail-stops the volumes the transaction touched (mutations refuse
  until remount), because the alternative is letting later operations move
  objects the interrupted transaction still has to finish. The next mount
  completes it.
- **No format change**: no new incompat bit, no reformat. An older binary
  reads such a volume byte-identically — it simply would not run the
  recovery.

Live signals on the `.stats` inode:

- `crossvol_tx_started` / `crossvol_tx_completed` — equal in steady state.
- `crossvol_tx_recovered` — transactions a mount rolled forward. Nonzero
  means a crash interrupted one; it is the machinery working.
- `crossvol_tx_steps_applied` / `crossvol_tx_steps_already_applied` — the
  recovery ledger; "already applied" is what makes a re-run safe.
- `crossvol_tx_steps_foreign` — **must stay 0**: recovery met an object that
  moved under an interrupted transaction and skipped it rather than
  overwriting it (loudly logged; run `squeezefs fsck`).
- `crossvol_tx_midplan_escalations` — **must stay 0**: the fail-stop above
  fired.

## POSIX semantics — declared deviations

Everything here is behavior an application can observe and a conformance
suite can measure. It is DECLARED, not accidental: each entry names what
POSIX/Linux would do, what SqueezeFS does, and what it costs you. (The
pre-RC audit's POSIX board — `docs/pre-rc-engineering-spec.md` §5 — is
the source; items not listed here were fixed rather than declared.)

### `fallocate(mode = 0)` / `posix_fallocate` does not reserve space (POSIX-12)

A successful `posix_fallocate()` promises that the subsequent writes into
that range will not fail with `ENOSPC`. SqueezeFS **extends the file's
size and reserves nothing**: the striped backend is thin — blocks are
allocated at write time — so a later write into the "preallocated" range
can still return `ENOSPC` if the volume filled in between. Databases and
media writers that preallocate for this guarantee (PostgreSQL WAL,
SQLite, `ffmpeg`) get the size, not the guarantee.

Rationale: honoring it means a real reservation ledger — allocator space
held against an inode, surviving crashes and unlink, and subtracted from
`statfs` free — which is a feature, not a flag. Until it exists the
honest posture is a declared deviation rather than a silent one. (This
is a different statement from the thin-provisioning reporting
adjudication of fstests generic/213: that one is about what `df`
reports, this one is about what `fallocate` promises.)

Operationally: size your volumes with headroom, and treat `ENOSPC` on a
write into a preallocated range as expected on a full filesystem rather
than as a bug.

### `noatime` is the only atime policy — the tool classes that notice (POSIX-17)

SqueezeFS never updates access times (mounting `relatime`/`strictatime`
does not change that; the fstests adjudications for generic/003 and
generic/192 record it as by-design). `st_atime` therefore tracks the
inode's other timestamps rather than reads, and every consumer of
"when was this last READ" silently no-ops. The classes that matter in
practice:

- **Maildir new-mail detection** — MUAs and `biff`-class notifiers
  compare `atime` against `mtime` on `new/` to decide "unread mail
  arrived"; with a frozen atime the heuristic mis-fires (typically
  reporting new mail forever, or never).
- **`tmpwatch --atime` / `tmpreaper` / systemd-tmpfiles age policies** —
  cleanup keyed on access age deletes files that ARE being read. Key
  those policies on mtime/ctime (`tmpwatch --mtime`) on SqueezeFS
  filesystems.
- **`updatedb` / locate freshness heuristics** — index-staleness
  decisions that consult atime lose their signal (correctness is
  unaffected; refresh cadence is not).
- **HSM / tiering agents** (external ones — SqueezeFS's own tiering uses
  its internal counters, not atime) — "demote what has not been read in
  N days" demotes hot data. Point such agents at the `.stats` read
  counters or run them off application-level telemetry.
- **`find -atime` / `-anewer`, du-style reporting on access age** —
  return whatever the other timestamps imply, not read history.

### Rename-overwrite leaves a crash-window orphan (POSIX-15)

`rename()` over an existing file unlinks the destination inode, but its
teardown is deferred to the kernel's FORGET (a still-open replaced file
must survive to its last close, exactly like `unlink`). A daemon exit
INSIDE that window — between the rename commit and the FORGET —
leaves the replaced inode's record and its blocks allocated with no name
pointing at them, and **there is no mount-time reconciliation sweep**.
`fsck` class [C9](#c9--unreferenced-inodes-also-the-cleanup-path-for-pre-s35-damage)
now walks the inode tree for unreferenced records — but it claims only
the `nlink >= 1` shape, and a rename-overwrite orphan is `nlink == 0`
(the destination was unlinked), which C9 deliberately does not claim
because that shape is indistinguishable from a legitimately
unlinked-but-open file without the live open-handle registries. Class
[C10](#c10--inode-plane-reference-consistency-which-direction-means-stop-and-read)
claims the other half of `nlink == 0` — the half **with** a name still
pointing at it, which no legitimate state produces — and stops at the same
line, for the same reason. So this one stays uncovered: the space is not
lost data — it is leaked capacity, bounded by how many rename-overwrites
were in flight at the crash.

### Writable shared `mmap` and interception (POSIX-7)

Within one process the shim handles it: any `MAP_SHARED` mapping unbinds
every in-process binding on that inode AND poisons it, so the ring is
never re-armed underneath the mapping (a later `open()` of the same file
stays kernel-served for the process's lifetime). **Across processes it
is unsupported and undetectable** — see the interception section's
"Unsupported mixes": a peer's mapping is invisible to this process's
shim, and the kernel never tells the daemon about mappings. Run
mmap-writer workloads without the shim.

## Breaking changes & migration notes

SqueezeFS moves **always forward** — no backwards compatibility. Refusals are loud, name their cause, and state the remedy. Current refusal classes an operator can hit:

> **⚠️ Legacy metadata format v2 — removed.** A v2 superblock refuses to mount with *"no longer supported; reformat required"*. Reformat to v3 with `squeezefs format --force` (destroys the old contents). The offline `squeezefs migrate` v2→v3 converter was deleted along with v2 support.

> **⚠️ Pre-watermark v3 volumes — refused (REFORMAT REQUIRED).** v3 volumes formatted before the node-seq mint watermark (the Finding-A KV-corruption fix era) fail the superblock feature gate: *"pre-watermark v3 volume: formatted before the node-seq mint watermark (Finding A) and no longer supported; reformat required"*. Volumes carrying **unknown** incompat bits (formatted by a newer binary) also refuse, naming the bits — upgrade squeezefs instead.

> **⚠️ Pre-fix compressed/encrypted volumes — refused (REFORMAT REQUIRED).** Volumes formatted with `--compression`/`--encrypt-algo` before the FIND-RW4-A incompressible-block fix cannot hold worst-case stored images; mounts refuse with *"compressed/encrypted volume geometry cannot hold incompressible blocks (FIND-RW4-A) … refusing to mount"* (full-size incompressible blocks on such volumes were never readable — the refusal names the fix). Reformat with a current binary: `format` now reserves per-chunk headroom on transformed volumes (clamping the block size loudly when needed), and compression became **best-effort per block** — incompressible blocks are stored raw (`compress_stored_raw` counts them in `.stats`).

> **⚠️ Pre-KW-1 encrypted volumes — refused (REFORMAT REQUIRED).** Volumes formatted with `--encrypt-algo` before the key-handling fix wrapped their data keys with RSA-OAEP, whose implementation (`rsa 0.9.x`, RUSTSEC-2023-0071 "Marvin", no fixed release) has been removed from squeezefs. Such a volume (no key reference in its format config) refuses to mount, naming the remedy: copy the data off with a binary at or before commit `7d1ec2e1`, `squeezefs format --force --encrypt-algo aes256gcm --encrypt-key <path>`, copy back. Note the defect this replaces — `--encrypt-key` was documented as a path but consumed as PEM *content*, so volumes formatted the documented way never accepted a single write and carry no data; the usage that did work stored the key **in cleartext on the volume it encrypted**, so treat any such key as compromised. Unencrypted volumes are unaffected.

> **⚠️ Staging directories are generation-bound.** Staging/cache dirs are stamped with the filesystem generation (the v3 superblock uuid set). A mount that finds staged content from a **dead generation** (e.g. after a reformat over live staging dirs) wipes it with one loud `STAGING GENERATION MISMATCH` line and counts `staging_generation_discards` in `.stats` — staged writes stamped by the old generation are gone **by design** (reformat discards data).

> **⚠️ Cache/staging paths are format-declared.** `mount --disk-cache-paths` is refused loudly (never silently ignored). Change paths with the admin op `squeezefs config set-cache-paths <sqmeta-uri> <paths...>` (guarded like `format`: refused while any client has the volume mounted; the new dirs are wiped so the next mount stamps a fresh staging generation). Read them back with `config get-cache-paths`. A filesystem formatted without `--disk-cache-paths` is **permanently cache-less**.

> **⚠️ Multi-writer-capable formats and older binaries.** Since 2026-08-16 a fresh `format` stamps the nine multi-writer capability bits (7–15) by default; a binary from before that refuses such a set loudly, naming the unknown bits. `format --single-writer` produces the older unstamped class for exactly that case (a recovery scratch volume an old binary must read), and `squeezefs volume enable-multi-writer` upgrades an unstamped set offline. **A volume that has ever carried a file into the block-map tree** ([Large files](#large-files--the-block-map-tree)) additionally carries incompat bit 16 (`KV_BLOCK_MAP_TREE`), stamped at the first crossing and never on untouched volumes — binaries older than 2026-09-01 refuse it the same way. Forward-only in every case: upgrade the binary, never downgrade the volume.

> **⚠️ Frozen-width metadata routing — refused (REFORMAT REQUIRED).** Metadata routing widths are derived since 2026-08-02 (`KV_DYNAMIC_ROUTING`, incompat bit 6, presence-required): a set formatted under the earlier frozen `--meta-slots` scheme refuses to mount, naming the reformat. The flag itself is a hard error (see below).

## Removed verbs & flags

Kept here so stale scripts fail comprehensibly:

> **Removed flags/verbs**: `--strict-meta-atomicity` (only ever gated v2 volumes; deleted with them), `squeezefs migrate` (deleted with v2), `format --meta-slots` (a hard error since 2026-08-02 naming its successors `volume add-meta --take-slots` / `migrate-meta-slot` — routing widths are derived, never chosen), `format --multi-writer` (accepted and announced-inert: multi-writer-capable is the default; `--single-writer` is the opt-out), `mount --local-ips` (the socket-level multi-rail bonding was removed in the 2026-07-04 connection simplification — fabric multipath is the kernel NVMe initiator's domain), `mount --disk-cache-paths` (see [Breaking changes](#breaking-changes--migration-notes)), the `config data-volume add/remove/migrate` / `config metadata-volume add/remove/migrate` / `config … fsck` fake admin verbs (deleted 2026-07-19 in the volume-lifecycle program's honesty cleanup — they reported success without doing the work; their real successors are `squeezefs volume …` and `squeezefs fsck`, below). `squeezefs defrag` was removed 2026-07-17 for the same honesty reason and **returned as a real implementation in the 2026-07 volume-lifecycle program** — see [Volume lifecycle & online maintenance](#volume-lifecycle--online-maintenance).
>
> **NVMe-oF verb migration (2026-07-17, target-management program PR 2/N2** — `docs/design-nvmeof-target-management.md` §API): the whole `squeezefs storage nvmeof <verb>` surface **moved to the top-level `squeezefs nvmeof <verb>`**, and within it: `share --spdk`/`unshare --spdk` → `--target-stack {spdk|nvmet}` (default spdk; `unshare` now resolves the stack from the share ledger, never a flag); `restore-shares` → `restore` (and it works — the old registry truncated itself to `[]` on every root invocation, so share persistence had **never** worked; a pre-existing `/etc/squeezefs/nvmeof_shares.json` is retired to `.retired-by-rebuild` on the first mutating verb, and pre-rebuild live shares surface in `list` as foreign/unmanaged — as of milestone **N4b** the managed exit is **`squeezefs nvmeof adopt <subnqn>`**, which absorbs the live share into the ledger with `adopted_from: pre-rebuild` provenance and zero serving interruption; manual removal-first + re-share remains the documented fallback for shapes adopt refuses); `spdk-install`/`spdk-setup`/`spdk-start` → `nvmeof target install/setup/start` (live as of milestone N3, joined by the new `target stop`/`status`/`systemd-unit`; SPDK *sharing* went live with milestone **N4** — the default stack shares for real, and the interim loud-fail message is gone); `spdk-bind`/`spdk-unbind` **deleted** (PCIe vfio passthrough backing is a future program — v1 serves kernel block nodes and files, `bdev_aio` on the SPDK stack); share's silent 1 GiB sparse auto-create on a missing path **deleted** (refuse loud; `--create-size <sz>` is the explicit opt-in).

## Configuration reference

### Environment knobs — the parsing convention

Every `SQUEEZEFS_*` environment knob obeys **one** convention (ENG-10). The
authoritative list — name, accepted values, admissible range, default, and one
line of purpose for each — is the registry in **`src/env_knobs.rs`**, which a
gate test ties to the code: a knob that exists in the source but not in the
registry fails the build, so a new knob cannot ship undocumented. The
subsystem sections below cover the knobs an operator normally touches; the
registry covers all of them, including the test seams.

The laws:

1. **Unset, empty, or whitespace-only means absent.** `SQUEEZEFS_X=` is "not
   set", never "set to garbage".
2. **Explicit values win verbatim**, then percentage forms, then the derived
   default (`SQUEEZEFS_*_MAX` > `SQUEEZEFS_*_PCT` > derivation).
3. **Booleans have one spelling set**: `1`/`true`/`yes`/`on` enable,
   `0`/`false`/`no`/`off` disable, case-insensitive. This applies to the knobs
   whose default is ON as well — `SQUEEZEFS_NUMA=off` and `SQUEEZEFS_NUMA=0`
   are the same thing. (Before ENG-10, 19 flags were *presence*-based:
   `SQUEEZEFS_FREE_FORENSICS=0` **enabled** forensics.)
4. **A malformed, out-of-range, or retired knob refuses the process at
   startup**, naming every offender at once, before any volume is opened or
   anything is mounted:

   ```console
   $ SQUEEZEFS_READ_LANE=yess SQUEEZEFS_IPC_IDLE_SECS=abc squeezefs mount …
   Error: refusing to start: invalid SqueezeFS environment knob(s)
     - SQUEEZEFS_IPC_IDLE_SECS='abc' is invalid: expected an integer in 0..=86400
     - SQUEEZEFS_READ_LANE='yess' is invalid: expected a boolean — 1/true/yes/on or 0/false/no/off
     (a malformed knob is never silently defaulted — fix or unset it; empty means unset)
   ```

   Out of range is a refusal, not a clamp: a knob set past its admissible
   range is a mistake worth naming, and silently clamping it is how "I set it
   and nothing happened" happens.
5. **An unrecognized `SQUEEZEFS_*` / `SQZ_*` name is announced, not refused** —
   the typo detector for knob NAMES (`Warning: SQUEEZEFS_RECLAIM_BACH is not a
   SqueezeFS knob …`). It cannot refuse: a mixed-version fleet legitimately
   carries the next release's knobs, and the interception shim's client-side
   knobs live in the same environment as the daemon's.
6. **Retired spellings refuse loudly, naming the successor.** Current retirees
   (the ENG-10 namespace-collision rename — the inode-reclaim family shared a
   prefix with the unrelated *block*-reclaim family):

   | Retired | Use instead |
   |---|---|
   | `SQUEEZEFS_RECLAIM_BATCH` | `SQUEEZEFS_INODE_RECLAIM_BATCH` |
   | `SQUEEZEFS_RECLAIM_BATCH_WINDOW_MS` | `SQUEEZEFS_INODE_RECLAIM_WINDOW_MS` |
   | `SQUEEZEFS_RECLAIM_CONCURRENCY` | `SQUEEZEFS_INODE_RECLAIM_CONCURRENCY` |
   | `SQUEEZEFS_FUSE_PLACED_MERGE` | *(deleted — the FUSE placed-merge lever was falsified and removed; the interception shim's placed-sever path is a different mechanism and has no knob)* |

   (`SQUEEZEFS_RECLAIM_BATCH_BLOCKS`, `..._BATCH_MS`, `..._QUEUE_MAX_BLOCKS`,
   `..._LANES_PER_DEV` and `..._CAP_PARK_MS` are unchanged — they are the
   *block*-reclaim family and always were.)

**The client shim is the one deliberate asymmetry**: `libsqueezefs_il.so`
never kills its host application over an environment typo. It announces the
bad value on stderr and keeps the documented default. Everything else about
the value law is identical, because both sides parse through the same shared
file (`crates/squeezefs-ipc/src/env_knob_core.rs`).

Knobs that are **measurement levers, not operational settings** say so in the
registry (`SQUEEZEFS_WRITE_PIPELINE_DEPTH_BLOCKS`, `SQUEEZEFS_PATCH_MAX_BYTES=0`,
`SQUEEZEFS_PUBLISH_COALESCE_MAX=1`, `SQUEEZEFS_NUMA=0`, `SQUEEZEFS_READ_LANE=0`,
`SQUEEZEFS_NT_COPY=0`, `SQUEEZEFS_OVERLAY_DEPTH_GOVERNOR=0`, …). They exist so an A/B can be counted; a fleet running
one of them is running an experiment.

### Environment knobs — the complete registry

Every knob the tree reads, grouped as the registry groups them (`src/env_knobs.rs` is the source; this table is its operator rendering and a gate test keeps the two in step — a knob that exists in the source but not in the registry fails the build). **Accepts** is the admissible form: `bool` = `1/true/yes/on` vs `0/false/no/off`; `int lo..hi` = an inclusive range, out of range refuses; enum words are exact. **Default** is what an absent knob means (`derived` = a sizing function of the memory budget, core count, block/ring geometry or a live measurement — see the subsystem section). Knobs whose purpose says *A/B lever* or *control* exist so a measurement can be counted; a fleet running one is running an experiment. Knobs marked *test seam* are product code armed only by the suites; never set them in production.

**Build identity (embedded at compile time by build.rs — not read from the process environment)**

| Knob | Accepts | Default | Purpose |
|---|---|---|---|
| `SQUEEZEFS_BUILD_COMMIT` | build-time | `git` | Build commit hash embedded by build.rs. |
| `SQUEEZEFS_BUILD_COMMIT_SHORT` | build-time | `git` | Short form of the build commit. |
| `SQUEEZEFS_BUILD_DIRTY` | build-time | `git` | 1 when the build tree carried uncommitted tracked changes. |
| `SQUEEZEFS_BUILD_TAG` | build-time | `git` | Exact stable-*/lts-* tag on the build commit, else empty. |
| `SQUEEZEFS_BUILD_PROFILE` | build-time | `cargo` | Cargo profile name the binary was built under (release = thin LTO; dist = fat LTO, tagged releases only). |
| `SQUEEZEFS_BUILD_TIMESTAMP` | build-time | `git` | UTC RFC3339 build timestamp (honors SOURCE_DATE_EPOCH). |
| `SQUEEZEFS_IL_BUILD_COMMIT` | build-time | `git` | The shim's build commit (KD-7 daemon/shim pairing). |

**CLI / process**

| Knob | Accepts | Default | Purpose |
|---|---|---|---|
| `SQUEEZEFS_META_URI` | string | `none` | Default `sqmeta://` URI for the verbs that take one. |
| `SQUEEZEFS_HOSTNQN` | string | `none (/etc/nvme/hostnqn)` | Per-mount NVMe-oF host NQN presented on every fabrics connection this daemon makes (the mount becomes its own reservation registrant). Pair-or-neither with `SQUEEZEFS_HOSTID`; requires daemon-owned data-plane connects from durable `fabric_endpoint:` records (`config set-fabric-endpoints`). `-o hostnqn=` wins. |
| `SQUEEZEFS_HOSTID` | string | `none (/etc/nvme/hostid)` | The paired per-mount NVMe-oF host id (see `SQUEEZEFS_HOSTNQN` — pair-or-neither). `-o hostid=` wins. |
| `SQUEEZEFS_ENCRYPT_KEY_FILE` | string | `/etc/squeezefs/keys/<id>.key` | Encryption key material path (see [Transparent compression & encryption](#transparent-compression--encryption)). |
| `SQUEEZEFS_DAEMON_PIPE` | int 0..2^31−1 | `none` | Internal: the fd the forked daemon writes `ready` to. Set by the parent process, never by hand. |
| `SQUEEZEFS_TIMEOUT` | int 1..86400 | `30` | Metadata-operation timeout, seconds (read once at launch). |
| `SQUEEZEFS_SUPERVISE_INTERVAL_SECS` | int 1..86400 | `5` | `mount --supervise` probe interval, seconds. |
| `SQUEEZEFS_SUPERVISE_UNRESPONSIVE_SECS` | int 1..86400 | `30` | `mount --supervise` unresponsiveness threshold before abort, seconds. |

**Memory budget (R5)**

| Knob | Accepts | Default | Purpose |
|---|---|---|---|
| `SQUEEZEFS_MEM_BUDGET_MB` | int 1..2^30 | `derived` | R5 memory budget, MiB (absolute > pct > cgroup/RAM derivation). |

**Fleet share (co-located daemon fleets)**

| Knob | Accepts | Default | Purpose |
|---|---|---|---|
| `SQUEEZEFS_FLEET_SHARE` | int 1..4096 | `1` | N co-located daemons divide the machine: one divisor applied at the root of the derived-sizing tree (memory budget and CPU count become `ceil(system/share)`). Derived tier only — explicit knobs still win, floors are never divided, the FUSE queue count is exempt. Never auto-detected; `1` = whole-machine posture. See [Fleet-share sizing](#fleet-share-sizing-co-located-daemon-fleets). |

**Metadata (the v3 CoW KV store)**

| Knob | Accepts | Default | Purpose |
|---|---|---|---|
| `SQUEEZEFS_META_NODE_CACHE_MB` | int 1..2^30 | `derived` | Per-volume KV node-cache budget, MiB (absolute wins over _PCT). |
| `SQUEEZEFS_META_NODE_CACHE_PCT` | int 1..100 | `derived` | Per-volume KV node-cache budget as a percentage of the R5 budget. |
| `SQUEEZEFS_META_CHECKPOINT_MAX_DIRTY_NODES` | int 1..2^32 | `derived` | Dirty-node checkpoint cap = the mount-replay working-set bound (derived `max(4096, budget/32 ÷ node_size)`). |
| `SQUEEZEFS_META_COMMIT_BATCH_TXS` | int 1..2^20 | `derived` | Commit-conveyor batch cap, transactions (derived `max(64, cpus × 2)`). |
| `SQUEEZEFS_META_COMMIT_BATCH_BYTES` | int 1..1 TiB | `derived` | Commit-conveyor batch cap, bytes (derived `max(256 KiB, ring/16)`, clamped to the ring's admissible capacity). |
| `SQUEEZEFS_JOURNAL_LANE` | bool | `on` | Each writable metadata volume gets its own journal lane: one OS thread (`sqz-jrnl{N}`) that runs the commit conveyor's apply pass and durability lane and owns the volume's journal io_uring. `0` = both stages on the shared metadata pool (A/B control). Engagement `journal_ring_lane_writes` ≈ conveyor passes. |
| `SQUEEZEFS_META_FLUSH_INTERVAL_MS` | int 0..86400000 | `50` | Journal/checkpoint cadence, ms; `0` = strict per-commit durability. (The day-long ceiling exists for the suites that park the timer.) |
| `SQUEEZEFS_JOURNAL_FLUSH_INTERVAL_MS` | int 0..86400000 | `unset` | Legacy alias for SQUEEZEFS_META_FLUSH_INTERVAL_MS (the new spelling wins). |
| `SQUEEZEFS_META_REVALIDATE_MS` | int 1..86400000 | `derived` | Coherent-reader node-cache revalidation cadence, ms. Derived default = max(flush cadence, the 1 s checkpoint ceiling); trades staleness (interval + 1 s) against reload cost. Inert on write mounts. |
| `SQUEEZEFS_BLOCK_REFS_VERIFY` | bool | `off` | Run the durable-vs-derived block-reference oracle at mount (pays the inode-tree walk the durable records exist to avoid; fsck runs the same comparison unconditionally as class C8). |

**Layout / publish economy (incl. the block-map tree)**

| Knob | Accepts | Default | Purpose |
|---|---|---|---|
| `SQUEEZEFS_DEFAULT_BLOCK_SIZE` | int 4096..1 TiB | `4194304` | Default striped block size, bytes, when format did not record one. |
| `SQUEEZEFS_PUBLISH_COALESCE_MAX` | int 0..2^20 | `64` | Per-inode publish-coalescing window; `1` = the serialized per-block posture (A/B lever). |
| `SQUEEZEFS_LAYOUT_DELTA_MAX_CHAIN` | int 0..2^20 | `64` | Layout delta-record chain cap before a full re-base save; `0` = full saves only (A/B lever). |
| `SQUEEZEFS_PUBLISH_COMMIT_GROUP_MAX` | int 0..2^20 | `0 (derives from META_COMMIT_BATCH_TXS)` | Layout saves aggregated into one multi-inode transaction per conveyor window; `1` = per-save commits (A/B lever). |
| `SQUEEZEFS_KVMAP` | bool | `on` | Whether new beyond-inline block-map crossings take the block-map tree ([Large files](#large-files--the-block-map-tree)). `0` = new crossings take the legacy indirect-blob arm (A/B lever); never disables resolution of an existing tree head. |
| `SQUEEZEFS_KVMAP_OVERLAY` | bool | `on` | Whether block-map-tree inos whose estimated map exceeds the derived RAM budget share take the partial store (dirty overlay + bounded warm windows + tree read-through). `0` = whole map in RAM at any size (acceptance control, never an operational escape). |
| `SQUEEZEFS_MAP_MIGRATE_CHUNK` | int 64..1024 | `512` | Block-map-tree crossing ops per transaction. The registry cap keeps a mis-set value under the whole-journal-entry admission bound. |

**Write path**

| Knob | Accepts | Default | Purpose |
|---|---|---|---|
| `SQUEEZEFS_PATCH_MAX_BYTES` | int 0..1 TiB | `derived block_size/8` | Sole-owner in-place patch ceiling, bytes (also the device overlay's minimum segment length); `0` = the acceptance A/B lever. |
| `SQUEEZEFS_FOLD_MAX_EXTENTS` | int 0..2^24 | `64` | Extent-overlay fold trigger: parked extents per block. |
| `SQUEEZEFS_FOLD_MAX_BYTES` | int 0..1 TiB | `derived block_size/4` | Extent-overlay fold trigger: parked bytes per block. |
| `SQUEEZEFS_PARKED_BUFFERS` | int 0..2^24 | `derived` | Parked-write budget in buffers' worth of block size (derived `max(256, budget/16 ÷ block_size)`). |
| `SQUEEZEFS_PARKED_GATE_ASSIST_MS` | int 0..600000 | `200` | Memory-budget-Red parked-gate self-flush assist window, ms. |
| `SQUEEZEFS_INPLACE_OVERWRITE` | bool | `off` | Opt a substrate into in-place eligible full-block overwrites (real-SSD DSM fleets; measured loss on zram-lz4). |
| `SQUEEZEFS_DEVICE_OVERLAY` | bool | `on` | The device-backed visible write overlay: eligible fresh/hole aligned segments store straight to an unpublished destination, ACK early, publish at coverage completion/fsync. Default on — the large-write A-leg. `0` = the accumulation control. Engagement `overlay_store_bytes` / `overlay_ack_early_bytes`. |
| `SQUEEZEFS_OVERLAY_OVERWRITE` | bool | `on` | The overlay's overwrite arm: aligned single-block overwrites of mapped striped passthrough blocks ride the zero-copy slot→device overlay with a fresh CoW destination (never in place). Default on (field-adjudicated on the CPU-bound fabric venue at half the daemon CPU; device-bound local substrates measured the `0` control ahead at low queue depth). `0` = the fresh-only gate. Engagement `overlay_overwrite_installs` / `overlay_overwrite_bytes`. |
| `SQUEEZEFS_OVERLAY_CLOSE_BARRIER` | bool | `off` | Barrier the data devices before the coverage-triggered close of an overlay-fed rewrite epoch (upgrades those non-fsync saves out of the write-back-cache volatility class). Default off — the identical window ships in the rewrite program's own non-fsync closes; fsync-class boundaries are always fully ordered regardless. |
| `SQUEEZEFS_REWRITE_SHADOW` | bool | `on` | Rewrite-program shadow swaps; 0 = the A/B control. |
| `SQUEEZEFS_WRITEBACK_QUEUE_CAP` | int 1..2^24 | `4096` | Writeback flush-unit queue capacity. |
| `SQUEEZEFS_WRITE_PIPELINE_DEPTH_BLOCKS` | int 0..2^20 | `derived (BDP)` | Write-pipeline depth target override, blocks — measurement lever; the default is the runtime bandwidth-delay derivation. |

**Block reclaim (device deallocation), inode reclaim, and the allocation-lane reservation**

| Knob | Accepts | Default | Purpose |
|---|---|---|---|
| `SQUEEZEFS_RECLAIM_BATCH_BLOCKS` | int 1..1024 | `64` | Background block-reclaim drain batch, blocks. |
| `SQUEEZEFS_RECLAIM_BATCH_MS` | int 0..600000 | `2` | Background block-reclaim batch-accumulation window, ms. |
| `SQUEEZEFS_RECLAIM_QUEUE_MAX_BLOCKS` | int 1..2^20 | `derived (floor 4096)` | Block-reclaim deferred-space cap, blocks (queued + in-flight); at-cap enqueues park (park-don't-spill). W-4 derivation (`.benchmarks/2026-09-05-w4-reclaim-derivation.md`): `clamp(measured displacement rate × the drain's room latency, 4096 = the shipped floor, budget/1024 ÷ per-entry RAM ≤ 2^20)`. Explicit value wins verbatim (`4096` = the A/B lever). Live value: `block_free_reclaim_queue_cap`. |
| `SQUEEZEFS_RECLAIM_LANES_PER_DEV` | int 1..64 | `32` | Parallel reclaim drain lanes per device. |
| `SQUEEZEFS_ALLOC_LANE_RESERVE_BLOCKS` | int 0..2^32 | `derived` | Fresh blocks one durable allocation-lane reservation covers on a partitioned (multi-writer) data plane; derived from the write pipeline's cold window. `1` = a commit per fresh block (pathological A/B control). Inert on every unpartitioned mount. |
| `SQUEEZEFS_RECLAIM_CAP_PARK_MS` | int 0..60000 | `derived (50..1000)` | At-cap reclaim enqueue park bound, ms, before soft overflow — the SAFETY bound only: since W-4 the park ends on the drain's room-made edge (a reclaimed range's `finish_free`). Derivation: `clamp(4 × batch_blocks ÷ measured drain rate, 50 ms manners tick, 1000 ms)` — the shipped 1,000 ms (which the 2026-09-01 rewrite rows read straight off the p99.9) is now the CEILING, never exceeded. Explicit value wins verbatim (`1000` = the A/B lever, `0` = never park). Live value: `block_free_reclaim_park_bound_ms`. |
| `SQUEEZEFS_DISCARD_ELISION` | bool | `on` | Elide discards for blocks a rewrite is about to overwrite; 0 = the A/B control. |
| `SQUEEZEFS_INODE_RECLAIM_BATCH` | int 1..1024 | `64` | Deferred inode-reclaim gather batch, inodes. |
| `SQUEEZEFS_INODE_RECLAIM_WINDOW_MS` | int 0..1000 | `20` | Deferred inode-reclaim gather window, ms. |
| `SQUEEZEFS_INODE_RECLAIM_CONCURRENCY` | int 1..2^16 | `max(4, cpus)` | Concurrent inode-reclaim workers. |
| `SQUEEZEFS_RECLAIM_BATCH` | retired | `-` | Retired: use `SQUEEZEFS_INODE_RECLAIM_BATCH` (the old name collided with the block-reclaim family). |
| `SQUEEZEFS_RECLAIM_BATCH_WINDOW_MS` | retired | `-` | Retired: use `SQUEEZEFS_INODE_RECLAIM_WINDOW_MS`. |
| `SQUEEZEFS_RECLAIM_CONCURRENCY` | retired | `-` | Retired: use `SQUEEZEFS_INODE_RECLAIM_CONCURRENCY`. |

**Read path**

| Knob | Accepts | Default | Purpose |
|---|---|---|---|
| `SQUEEZEFS_READ_HOT_BLOCK_CACHE_MB` | int 0..2^30 | `derived` | Hot-block RAM read tier budget, MiB (`0` disables the tier). |
| `SQUEEZEFS_READ_ADMISSION_FILL_PCT` | int 0..100 | `5` | Scan-resistant admission governor's bounded-waste share, percent of device read bytes. |
| `SQUEEZEFS_READ_TIER_ADMISSION` | always / second-touch / never | `second-touch` | NVMe read-tier admission policy for large fills (`second-touch` default). |
| `SQUEEZEFS_READ_PREFETCH_WINDOW` | int 0..2^20 | `derived` | Per-stream prefetch window cap, blocks (`0` disables). |
| `SQUEEZEFS_READ_PREFETCH_SHARE_PCT` | int 1..100 | `50` | The prefetch pipeline's share of the hot-tier budget, percent. |
| `SQUEEZEFS_READ_RANGED_THRESHOLD` | int 0..1 TiB | `262144` | Reads at or under this size fetch only their aligned device window instead of the whole block (passthrough volumes); `0` disables. |
| `SQUEEZEFS_READ_DEST_LEASE` | bool | `on` | Cold aligned sub-block reads DMA device bytes straight into the reply's registered destination (the serve copy deleted). `0` = the pre-lease fill+copy shape (A/B control). Engagement `read_dest_lease_bytes`. |
| `SQUEEZEFS_READ_LANE` | bool | `on` | The read-lane hold (a completed-fill store the demand cohort serves from); `0` = the control. |
| `SQUEEZEFS_READ_LANE_DEPTH` | int 0..2^16 | `derived (engage-governor)` | Read-ahead lane depth pin, blocks — unset = probe-governed; `0` = ahead-issue off (hold-only control). |
| `SQUEEZEFS_NVME_READ_LANES` | int 1..1024 | `derived (cpus / data devices)` | Per-device READ submission fan-out lanes; `1` = single worker (A/B posture). |
| `SQUEEZEFS_NVME_WRITE_LANES` | int 1..1024 | `derived (cpus / data devices)` | Per-device data-WRITE submission fan-out lanes with block-offset affinity; `1` = single worker (A/B posture). |
| `SQUEEZEFS_DIRECT_DEVICE_TRUE` | bool | `off` | Strict device-true O_DIRECT reads (measurement escape); the default serves O_DIRECT from the read tiers like buffered I/O. |

**Copy economy**

| Knob | Accepts | Default | Purpose |
|---|---|---|---|
| `SQUEEZEFS_WRITE_SHARED` | bool | `on` | Shared-mode write admission: fully-mapped within-EOF striped overwrites take the inode READ guard (classify → acquire → revalidate → at most one upgrade). Default on (+7–8 % on both lanes at ~525 k IOPS); `0` = the A/B lever. |
| `SQUEEZEFS_WRITE_GUARD_NARROW` | bool | `on` | W-2 write-stream-guard (2026-09-05): the Shared class widens to EVERY cache-resident striped write — the fresh/append (extending) stream and hole-fills included — so a qd-N stream's meta-prep takes the inode READ guard instead of handing one exclusive guard down an N-deep wake chain (its data path already ran under block locks only). `0` = the pre-campaign class (mapped within-EOF overwrites only) — the same-binary A/B lever, never an operational escape. Inert under `SQUEEZEFS_WRITE_SHARED=0`. Engagement: `write_lock_scope_shared` counts the stream's writes; `write_lock_hold_shared` is their hold. |
| `SQUEEZEFS_NT_COPY` | bool | `on` | Non-temporal stores at the two DMA-destined copy sites; 0 = the A/B control. |
| `SQUEEZEFS_NT_COPY_MIN` | int 0..1 TiB | `262144` | NT-store engagement floor, bytes. |
| `SQUEEZEFS_NT_READ_SERVE` | bool | `on` | Non-temporal stores on dest-arm read serves; 0 = the A/B control. |
| `SQUEEZEFS_NT_READ_SERVE_MIN` | int 0..1 TiB | `262144` | Read-serve NT-store floor, bytes (keeps rand-4k/warm-small serves cached). |
| `SQUEEZEFS_NUMA` | bool | `on` | NUMA placement actions (structurally inert on single-node hosts); 0 = the A/B lever, instrument stays live. |
| `SQUEEZEFS_READ_MOSTLY_CACHE` | bool | `on` | Read-mostly backing for the hot process-global caches (metadata, stream lanes, attrs): reads are pure loads, eviction rides a policy shell. `0` = the classic value caches (A/B control). |
| `SQUEEZEFS_CACHE_TOUCH_SECS` | int 0..86400 | `derived (TTI/4)` | Policy-touch sampling horizon for the TTI caches, seconds — an entry read at least once per horizon keeps access-refreshed residency. `0` = touch the policy on every read (isolation lever). |

**Transport (fuse3 fork)**

| Knob | Accepts | Default | Purpose |
|---|---|---|---|
| `SQUEEZEFS_FUSE_MAX_WRITE` | int 4096..1 TiB | `derived` | The daemon's desired FUSE `max_write`, bytes (still gated by the kernel's `fs.fuse.max_pages_limit`); A/B lever. |
| `SQUEEZEFS_FUSE_OVER_IO_URING_QUEUES` | int 1..512 | `kernel possible CPUs` | FUSE-over-io_uring queue count (TESTING only — fewer than possible CPUs never becomes ready). |
| `SQUEEZEFS_FUSE_OVER_IO_URING_Q_DEPTH` | int 1..32 | `derived (desired 32)` | Entries per FUSE-over-io_uring queue (default 32 degraded to the payload-buffer cap, floor 4); explicit wins verbatim. |
| `SQUEEZEFS_FUSE_IO_URING_ENTRIES` | int 64..4096 | `1024` | SQ depth for the classical FUSE rings (INIT/notify). |
| `SQUEEZEFS_FUSE_IO_URING_SQPOLL_IDLE_MS` | int 0..600000 | `off` | Enable SQPOLL on the transport rings with this idle timeout, ms (measured NOT recommended). |
| `SQUEEZEFS_FUSE_IO_URING_SQPOLL_CPU` | int 0..4095 | `unpinned` | Pin the SQPOLL kernel thread to this CPU. |
| `SQUEEZEFS_FUSE_IO_URING_SPIN_US` | int 0..100000 | `off (0)` | Spin-before-park cap (µs) for the FUSE-over-io_uring queue workers; the window adapts to each worker's own park gap, engages only with ops in flight and is refused past the box's busy knee. Default off: at the field it deleted 69 % of the workers' parks and moved no latency term at +5.5 % CPU/op. Any nonzero value engages it (A/B lever). Ledger `transport_spin_{absorbed,expired,ns,refused_busy,window_us}`. |
| `SQUEEZEFS_FUSE_PIN_SCOPE` | node / core | `node` | Transport thread affinity scope; `core` is the pre-campaign hard-pin posture. |
| `SQUEEZEFS_FUSE_SAME_LANE_DISPATCH` | bool | `1` | READ handler futures run on the dispatching lane instead of round-robining to another lane; `0` restores the rotation (control). |
| `SQUEEZEFS_FUSE_READ_FAST_DISPATCH` | bool | `on` | READ fast-dispatch from the reap thread: the queue worker runs the filesystem's sync try-only warm probe at the delivery CQE — a warm READ is served inline, a miss mints the handler straight onto a `fuse3-tpc` lane. `0` = every READ rides the inbound queue (A/B control). Engagement `transport_fast_dispatch_{serves,demotes}`. |
| `SQUEEZEFS_FUSE_DRAIN_GROUP` | int 1..512 | `derived (node possible CPUs / 4, floor 1)` | Queues per FUSE-over-io_uring drain context; explicit width wins, `1` = one worker per queue (control). |
| `SQUEEZEFS_FUSE_KMBUF` | bool | `on` | kmbuf reply-buffer negotiation; 0 = the A/B control. |
| `SQUEEZEFS_FUSE_ZC` | bool | `on` | FUSE zero-copy serve integration (sqz kernel + CAP_SYS_ADMIN required; stock kernels decline loudly and run the buffer-ring path byte-identically). Default on: reads +40–75 %. `0` = the escape / A/B lever. See [FUSE zero-copy serve integration](#fuse-zero-copy-serve-integration-sqz-kernel-opt-in). |
| `SQUEEZEFS_FUSE_ZC_WRITE_FUSION` | bool | `on` | Handler/worker fusion for small zero-copy-armed FUSE WRITEs (the hold gates on the in-place-patch eligibility probe; ineligible shapes extract at delivery). Default on. `0` = the A/B control. Engagement `fuse3_zc_write_fusions`/`_bytes`; `fuse3_zc_write_lazy_extractions` ≈ 0 is the gate's staleness law. |
| `SQUEEZEFS_FUSE_ZC_READ_FUSION` | bool | `off` | Handler/worker fusion for zero-copy-armed FUSE READs the inline probe demoted (arm (b) of the READ dispatch law). Default off on measurement: composed behind the fast dispatch it read −6.5 % IOPS vs fast dispatch alone. `1` = fuse (the tail-shaped A/B posture). Engagement `fuse3_zc_read_fusions`. |
| `SQUEEZEFS_READ_ZC_SERVE` | bool | `on` | The fd-SOURCE zc READ serve (R-4, read board #4/#5): builds the whole-block READ fill pool as ONE memfd slab so hot-tier / read-lane-hold / cold-fill serves on a zero-copy-armed session are bridged into the request's pages STRAIGHT from the tier buffer (`READ_FIXED(pool fd → slot)`) — the daemon's tier → bounce copy deleted (2.0 → 1.0 passes/byte warm, 2.69 → 1.69 cold-slice). No kernel change; the write path's pool is untouched. Default off until the field A-B-B-A adjudicates. Engagement `read_zc_pool_serve_bytes` (+ `_warm_bytes`), `fuse3_zc_fd_body_replies`; `fuse3_zc_fd_body_fallbacks` must stay ≈ 0. |
| `SQUEEZEFS_FUSE_ZC_FUSION_MAX` | int 4096..2^30 | `derived (payload/8)` | Fused-dispatch payload ceiling, bytes — bounds the handler work the queue worker runs inline (never payload-scale copies). Default derived payload/8 (128 KiB at the shipped 1 MiB geometry). |
| `SQUEEZEFS_FUSE_PLACED_MERGE` | retired | `-` | Retired: the FUSE placed-merge lever was falsified and deleted (the shim's placed-sever path is a different mechanism). |
| `SQUEEZEFS_FUSE_ZC_RETENTION` | bool | `on` | Arm zero-copy payload retention where the kernel offers the RELEASE_PAYLOAD opcode and the session runs zc. Default on (arming alone is bit-identical; stock kernels decline informationally). `0` = the A/B escape. Proof `fuse3_zc_retention_negotiated`. |
| `SQUEEZEFS_ZC_ACK_EARLY` | bool | `on` | ACK-early for device-overlay stores: eligible stores reply while the DMA runs (page-cache writes retain the zc slot; O_DIRECT extracts at delivery). Coverage, publication, fsync and read-wait stay CQE-anchored. `0` = ACK-after-CQE (A/B). Engagement `overlay_ack_early_stores`/`_bytes`. |
| `SQUEEZEFS_OVERLAY_DEPTH_GOVERNOR` | bool | `on` | Device-overlay stores admit their in-flight bytes through the write-pipeline depth governor (one gauge, one target, one backpressure gate for both write vehicles). ACK-early unchanged — only admission waits. Default on; `0` = the open-loop overlay store (A/B control). Engagement `overlay_governed_stores`. |
| `SQUEEZEFS_ZC_ACK_EARLY_ODIRECT` | bool | `off` | Let a held O_DIRECT overlay slot ACK early by snapshotting before the reply. Production O_DIRECT overlay does not hold (it extracts at delivery), so this is a labeled test/measurement posture, default off. |
| `SQUEEZEFS_ZC_BRIDGE_TIMEOUT_MS` | int 100..600000 | `30000` | zc bridge-op deadline, ms: past it the worker cancels the op and its completion resolves through the loud fallback ladders (`fuse3_zc_bridge_cancels`, must stay 0). |
| `SQUEEZEFS_TEST_ZC_DROP_WRITE_CQES` | int 0..1000000 | `0` | Test seam: the zc worker consumes-and-drops the first N WRITE-class bridge CQEs (pend + deadline stay live) — the deterministic lost-CQE interleave of the zcws-9 W4 wedge. Never set in production. |
| `SQUEEZEFS_FUSE_NO_KILLPRIV` | bool | `off` | TESTING escape: refuse FUSE_HANDLE_KILLPRIV_V2 and restore the kernel's per-write GETXATTR probe. |
| `SQUEEZEFS_FUSE_ATTR_TTL_MS` | int 0..600000 | `1000` | Kernel attribute-cache TTL, ms (-o attr_timeout overrides). |
| `SQUEEZEFS_FUSE_ENTRY_TTL_MS` | int 0..600000 | `1000` | Kernel dentry TTL, ms (-o entry_timeout overrides). |
| `SQUEEZEFS_FUSE_DIR_ENTRY_TTL_MS` | int 0..600000 | `1000` | Kernel directory-entry TTL, ms (-o dir_entry_timeout overrides). |
| `SQUEEZEFS_FUSE_NEGATIVE_TTL_MS` | int 0..600000 | `1000` | Kernel negative-entry TTL, ms (-o negative_timeout overrides). |
| `SQUEEZEFS_TRANSPORT_MEM_MAX` | int 1..2^30 | `derived` | Transport payload-buffer cap, MiB (absolute > pct > derived). |
| `SQUEEZEFS_TRANSPORT_MEM_PCT` | int 1..100 | `derived` | Transport payload-buffer cap as a percentage of the R5 budget. |
| `SQUEEZEFS_TRANSPORT_DEBUG` | bool | `off` | Verbose transport tracing (development). |

**LD_PRELOAD interception — daemon side**

| Knob | Accepts | Default | Purpose |
|---|---|---|---|
| `SQUEEZEFS_IPC` | bool | `off` | Env half of `-o interception` (arms the session host). |
| `SQUEEZEFS_IPC_ALLOW_DEV` | bool | `off` | Admit degenerate (`unknown` / `-dirty`) build identities past the daemon/shim same-build gate — dev boxes only, announced loudly on both ends and counted in `ipc_binds_dev_override`. |
| `SQUEEZEFS_IPC_ARENA_MB` | int 1..2^20 | `derived` | Per-session payload arena, MiB. |
| `SQUEEZEFS_IPC_ARENA_THP` | bool | `on` | PMD-align + MADV_COLLAPSE session arenas; 0 disables. |
| `SQUEEZEFS_IPC_IDLE_SECS` | int 0..86400 | `300` | Idle-session reap window, seconds; 0 disables. |
| `SQUEEZEFS_IPC_INVAL_WINDOW_MS` | int 1..600000 | `1000` | W1 invalidation coalescing window, ms. |
| `SQUEEZEFS_IPC_MAX_OP_BYTES` | int 1..1 TiB | `layout default` | Per-op payload ceiling, bytes. |
| `SQUEEZEFS_IPC_MEM_MAX` | int 1..2^30 | `derived` | Session-arena admission cap, MiB (absolute > pct > derived). |
| `SQUEEZEFS_IPC_MEM_PCT` | int 1..100 | `derived` | Session-arena admission cap as a percentage of the R5 budget. |
| `SQUEEZEFS_IPC_SERVICE_THREADS` | int 1..4096 | `derived clamp(3*cpus/8,2,64)` | Interception service-thread ceiling (threads spawn on session admission; same derivation as `SQUEEZEFS_IPC_DD_SHARDS`). |
| `SQUEEZEFS_IPC_DD_SHARDS` | int 1..4096 | `derived clamp(3*cpus/8,2,64)` | Interception direct-drive io_uring shards (one ring + pinned reaper per shard; override/measurement lever). |
| `SQUEEZEFS_IPC_DD_EAGER_FLUSH` | int 0..4096 | `governed (CadenceCore)` | Direct-drive issue cadence: absent = the governed threshold (probed downward, adopted only on live delivery response); explicit K > 0 = enter after K unflushed SQEs (measurement lever); `0` = sweep-only with the governor ignored (A/B control). Gauges `ipc_dd_cadence_{k,probe_ups,probe_backoffs}`. |
| `SQUEEZEFS_IPC_DD_INLINE_REAP` | bool | `on` | Reaper/drain fusion: the owning service thread drains its lane's direct-drive completion queue inline (zero syscall); `0` = reaper-only completion (A/B lever). Auto-disarmed on kernels without `IORING_ENTER_EXT_ARG`. |
| `SQUEEZEFS_IPC_SOCKET_DIR` | string | `derived (XDG_RUNTIME_DIR / /run)` | Rendezvous socket directory; `none` disables the filesystem-path socket. |
| `SQUEEZEFS_IPC_SPIN_US` | int 0..100000 | `0 (absent; =1 on SQUEEZEFS_IPC_SPIN_ADAPTIVE routes absent to the governor)` | Service-thread empty-pass spin window, µs. Explicit wins verbatim (incl. 0); absent with `SQUEEZEFS_IPC_SPIN_ADAPTIVE=1` routes to the spin governor. |
| `SQUEEZEFS_IPC_SPIN_ADAPTIVE` | bool | `on` | The service-thread spin governor (default on): a measured 100 µs window engaged only on multi-session lanes in the park-churn regime under a derived busy ceiling. `0` = the pre-governor posture (A/B control). Instruments `ipc_spin_{window_us,absorbed_parks,disengaged_busy}`. |

**LD_PRELOAD interception — client (shim) side**

| Knob | Accepts | Default | Purpose |
|---|---|---|---|
| `SQUEEZEFS_IL_SESSIONS` | int 1..4096 | `derived clamp(cpus/4,2,16)` | Client: fd-sharded sessions per mount (override lever; the default ties to the daemon's ceiling). |
| `SQUEEZEFS_IL_SPINS` | int 0..2^24 | `adaptive` | Client: fixed completion spin count (pins the adaptive spin for measurement). |
| `SQUEEZEFS_IL_MAX_RUN_SLOTS` | int 0..2^16 | `0 (unbounded)` | Client: cap on concurrently claimed ring slots per session (`0` = unbounded). |
| `SQUEEZEFS_IL_OP_TIMEOUT_MS` | int 1..600000 | `bounded default` | Client per-op bounded wait against a stalled daemon, ms; a timeout poisons the session → passthrough. |
| `SQUEEZEFS_IPC_DD_LANE_FLUSH` | bool | `on` | Direct-drive flush scope: on = a service thread enters only its own lane's ring; `0` = the flush-all sweep that serialized every service thread on every shard's kernel lock (A/B lever). |
| `SQUEEZEFS_IPC_DD_COOP_TASKRUN` | bool | `on` | Direct-drive rings built with `COOP_TASKRUN` (completion task-work runs at ring entry by the submitting task); `0` = signal-delivered task-work (A/B lever for completion-latency attribution). |
| `SQUEEZEFS_IL_REAP_PARK_MAX` | int 0..2^16 | `24` | Client libaio reap: queue depth at or below which the event-driven park is used (24 since the 2026-08-13 recount). |
| `SQUEEZEFS_IL_SPARSE_BATCH_MARKS` | bool | `on` | Client reap: the event park's per-session wake mark derives from its pending population (age-bounded at the reap quantum). Default on (higher IOPS on saturated-reaper fleets; a −3 % local unsaturated cost is recorded); `0` = the A/B control for latency-sensitive mounts. |
| `SQUEEZEFS_IPC_CQE_WAKE_LATCH` | bool | `on` | Completion-doorbell wake-collapse latch: a park era pays at most one completion `FUTEX_WAKE` (`ipc_cqe_wake_collapsed` counts the rest); `0` = wake per mark-passed completion (A/B control). |
| `SQUEEZEFS_IL_REAP_QUANTUM_US` | int 1..1000 | `50` | Client libaio reap: the deep-regime park's age bound, µs (the k-th completion cuts it short); measurement lever. |
| `SQUEEZEFS_IL_READ_DEST` | bool | `on` | Client arena-destination read serves; `0` = the A/B lever. |
| `SQUEEZEFS_IL_DIRECT_WRITE` | bool | `on` | The client direct-drive WRITE lane: patch-shaped ring writes DMA in place on the service thread's direct-drive lane (no sever bounce, no handler handoff); `0` = every ring write rides the sever→handoff path (control). |
| `SQUEEZEFS_IL_KERNEL_LANE_MIN` | int 0..1 TiB | `derived (memBW probe x lane-RTT delta, clamp [slab, max_op])` | Hybrid lane gate: ops strictly larger than this ride the kernel FUSE lane, smaller ops the IPC ring; derived from a memory-bandwidth probe × the lane RTT delta. `0` = gate off (all eligible ops ring — A/B lever). |

**zcrx read lane**

| Knob | Accepts | Default | Purpose |
|---|---|---|---|
| `SQUEEZEFS_ZCRX_LANE` | bool | `off` | Arm the zcrx read lane. |
| `SQUEEZEFS_ZCRX_LANE_FORCE_COPY` | bool | `off` | Test seam: allow the classic-recv backend to arm (copy parity). |
| `SQUEEZEFS_ZCRX_LANE_AREA_SIM` | bool | `off` | Test seam: AREA backend over simulated NIC DMA. |
| `SQUEEZEFS_ZCRX_LANE_SIM_CHUNK` | int 1..1 TiB | `derived` | Simulated area chunk size, bytes (rounded up to a power of two). |
| `SQUEEZEFS_ZCRX_LANE_TARGET` | string | `none` | zcrx lane target tuple (6 comma-separated fields); malformed = lane not armed, loudly. |

**Diagnostics / forensics**

| Knob | Accepts | Default | Purpose |
|---|---|---|---|
| `SQUEEZEFS_OP_PROFILE` | bool | `off` | Per-op phase histograms + the under-i_rwsem estimator (zero cost off). |
| `SQUEEZEFS_OP_TRACE` | bool | `off` | Arm the per-op trace ring at mount (one clock stamp per phase boundary per sampled op; `cat <mnt>/.trace` drains it; the `op-trace on\\|off\\|status` admin verb toggles it live). Geometry and sampling divisor derived. Off = one relaxed load per hook. See [Metadata Durability](#metadata-durability-crash-contract). |
| `SQUEEZEFS_FREE_FORENSICS` | bool | `off` | Capture a backtrace per block free to attribute double frees (expensive). |
| `SQUEEZEFS_STATS_KEY_CENSUS` | bool | `off` | Arm the `.stats` key census (live object keys + per-inode write custody — a debugging surface); the `*_count` gauges always export. |

**Cluster wire and the multi-writer arm**

| Knob | Accepts | Default | Purpose |
|---|---|---|---|
| `SQUEEZEFS_CLUSTER_WIRE_SVC_THREADS` | int 1..4096 | `derived clamp(cpus/8, 1, 8)` | Owner-side cluster-wire RPC lanes (pinned service threads, never the conveyor's task); absolute override, clamped to the core count. |
| `SQUEEZEFS_MULTI_WRITER` | bool | `off` | Arm the multi-writer planes (device-enforced WERO hold on every data namespace, remote write custody, metadata ownership, the publish path on the wire). Refuses the mount loudly on a non-PR substrate, a format missing any capability bit, an off membership plane or `SQUEEZEFS_MW_BIND=off`. Off = the shipped single-writer posture. See [Multi-writer data plane](#multi-writer-data-plane-dlm-stage-s9). |
| `SQUEEZEFS_MW_ROLE` | authority / co-writer / set-authority / partial-authority | `authority` | Which multi-writer posture this mount declares: `authority` (default — holds the claim on every volume), `co-writer` (metadata read-only locally, data under a custody lease; refused unless all five admission rungs hold), `set-authority` / `partial-authority` (per-volume owners assigned offline by `volume set-owners`). Read only when `SQUEEZEFS_MULTI_WRITER` is on. See [Multi-writer co-writer mounts](#multi-writer-co-writer-mounts-dlm-stage-s9) and [Per-volume metadata owners](#per-volume-metadata-owners-squeezefs-volume-set-owners). |
| `SQUEEZEFS_MW_AUTHORITY` | string | `none` | `addr:port` of the set authority (that node's `SQUEEZEFS_MW_BIND` endpoint), dialled for write custody, the lane grant and the publish path. Required for `co-writer` and `partial-authority`; not read for `set-authority`. |
| `SQUEEZEFS_MW_MEMBERS` | string | `none` | The authority's operator-declared co-writer roster: a comma list of client ids (`node_{16 hex}.m{8 hex}`, printed by a refused co-writer's own mount log; the bare node form names every mount slot of that node). Not read on the set-authority / partial-authority path (`volume set-owners` writes the enrollment). |
| `SQUEEZEFS_MW_BIND` | string | `auto` | Where this mount serves the write-custody and publish authority: `auto` (0.0.0.0:0, the default), an explicit `addr:port`, or `off` (refuses a multi-writer arm rather than arming an inert one). Read only when `SQUEEZEFS_MULTI_WRITER` is on. |

**Metadata function shipping**

| Knob | Accepts | Default | Purpose |
|---|---|---|---|
| `SQUEEZEFS_META_SHIP_BATCH_MAX` | int 1..4096 | `derived clamp(cpus x 2, 64, 4096)` | Verbs per shipped metadata frame (the pipelining unit); derived like the commit-conveyor batch cap, bounded by the cluster wire's control-class cap. |
| `SQUEEZEFS_META_SHIP_DEDUP_MAX` | int 1..2^24 | `derived max(batch_max x 128, 8192)` | Owner-side idempotency window entries — how far back a client's retry may reach and still be answered from its original outcome. |
| `SQUEEZEFS_PUBLISH_SHIP_DEPTH` | int 1..64 | `derived clamp(ceil(cpus/8), 2, 8)` | Publish frames one co-writer keeps in flight per authority (one authenticated session each). `1` = stop-and-wait (A/B control, never an operational escape). |
| `SQUEEZEFS_PUBLISH_CONVEYOR_GROUP` | bool | `on` | Owner side: a served frame's independent layout publishes commit as ONE conveyor group per dispatch round (one apply pass by construction). `0` = the per-call path (A/B control, never an operational escape). |
| `SQUEEZEFS_DLM_TOKEN_CACHE_MAX` | int 1..2^24 | `derived max(R5 budget/8192/32 B, 4096)` | Client fencing-token cache entries; every eviction costs one loud miss and one shipped refresh, never a wrong answer. |
| `SQUEEZEFS_DLM_STRIPES` | int 1..2^24 | `derived next_pow2(max(shipped, 16 × possible_cpus × q_depth))` | D-3: the stripe population of every DLM-class lock table — the 4a `DlmLockManager` inode + dentry classes (shipped floor 4096), the lease waiter / grant-floor pair and the S9 owner serve stripe (floor 1024). Derived from the FUSE-over-io_uring transport's deliverable concurrency (one queue per kernel possible CPU × `SQUEEZEFS_FUSE_OVER_IO_URING_Q_DEPTH`, default 32) at a 1/16 load factor, so a false-sharing wait — a full commit on the 4a tables (the D5 hold) — hits at most one acquire in sixteen at full ring occupancy; 16,384 on a 32-CPU box. Explicit wins verbatim (a non-power-of-two rounds up). MEASUREMENT lever: `4096` = the shipped-4a control, `1` = the everything-serializes crucible. Live: `<table>_stripes`, `<table>_stripe_collisions` / `<table>_key_waits`. |

**Delegations, recall lane, byte-range custody, slot placement**

| Knob | Accepts | Default | Purpose |
|---|---|---|---|
| `SQUEEZEFS_DLM_RECALL_BATCH_MAX` | int 1..32768 | `derived CONTROL frame cap/2/32 B = 16384` | Recall entries per batched per-client frame (measurement lever); the default derives from the cluster wire's control-class cap so one frame carries a client's whole token set. |
| `SQUEEZEFS_DLM_RECALL_DEADLINE_MS` | int 1..600000 | `derived 4x live p99 (rtt+owner), clamp [1 ms, lease TTL]` | Per-frame recall ack deadline, ms (measurement lever); derived from the live shipping RTT + owner p99, floored at 1 ms and ceilinged at the membership lease TTL. A timed-out recall's grant is dead, loudly. |
| `SQUEEZEFS_DLM_RECALL_COOLDOWN_MS` | int 1..600000 | `derived 8x the thrash window (= the deadline)` | How long a demoted (owner-served) hot object refuses new delegation grants (measurement lever); the thrash valve itself has no knob. |
| `SQUEEZEFS_DELEGATION` | bool | `on` | LOOKUP-class subtree delegations on an armed multi-writer fleet (grants piggyback on shipped replies; recall-before-conflicting-publish). Read only when the ownership plane is armed; `=1` unarmed is announced-inert, `=0` armed is the A/B control. |
| `SQUEEZEFS_RANGE_CUSTODY` | bool | `off` | Byte-range write custody on the multi-writer custody lease (the shared-file / MPI-IO shape). Default off (ships dark; the composition gate is green). Read only when the ownership plane is armed. See [Byte-range custody](#byte-range-custody-dlm-stage-s11). |
| `SQUEEZEFS_UPDATE_INTENTS` | bool | `on` | Per-directory exclusive UPDATE grants + asynchronous create-intent batches on an armed fleet (a co-writer mints children of a granted directory locally). Rides the delegation plane; `=0` armed is the tar-x row's control. |
| `SQUEEZEFS_SLOT_PLACEMENT` | bool | `on` | Client-owned-slot placement on an armed fleet: each shipping client's fresh inos land in a dedicated migratable slot; the migration half is dark on one-authority fleets and disarmed on multi-owner planes. `=0` armed is the A/B control. |

**Fleet jobs and the membership plane**

| Knob | Accepts | Default | Purpose |
|---|---|---|---|
| `SQUEEZEFS_FLEET_JOBS` | bool | `on` | Fleet-parallel maintenance: a reader/co-writer that joined the membership plane spawns the fleet job worker and serves read shards (fsck census/scrub) to the coordinator; on the coordinator, lets fsck fan out across enrolled workers. `0` disarms both halves on the mount that sets it. |
| `SQUEEZEFS_MEMBERSHIP_BIND` | string | `off` | Where this mount serves the membership plane: `off` (default), `auto` (0.0.0.0:0) or an explicit `addr:port`. Armed, a write mount becomes the lease authority and a read-only mount joins as a member. See [Membership plane](#membership-plane--lease-based-liveness-dlm-s6). |
| `SQUEEZEFS_MEMBERSHIP_LEASE_TTL_MS` | int 1000..600000 | `45000 (= CLIENT_STALE_TTL_SECS)` | The owner's membership lease TTL, ms; defaults to the one staleness law's 45 s so `live`/`stale` mean the same on the plane and in the claim records. The member's own deadline is always stricter. |
| `SQUEEZEFS_MEMBERSHIP_SKEW_MAX_MS` | int 1..60000 | `derived max(T_owner × 500 ppm, observed RTT)` | Clock-skew bound between owner and member, ms; derived from the physical clock-rate bound (500 ppm) and the measured renewal RTT. A value that collapses the member's deadline refuses the plane rather than clamping. |
| `SQUEEZEFS_MEMBERSHIP_PURGE_MS` | int 0..600000 | `derived max(2 × checkpoint cadence, observed RTT)` | How long a member may take to fail-stop (purge cached custody, stop in-flight DMA) once it decides to; derived from two revalidation cadences floored at one RTT. A safety input, not a tuning knob. |
| `SQUEEZEFS_MEMBERSHIP_GRACE_MS` | int 0..600000 | `derived (= the lease TTL)` | The successor owner's failover grace window, ms (reclaim admitted, conflicting fresh acquires refused). Default = the lease TTL; `0` opens no window. |

**Freed-offset grace period and lane refill**

| Knob | Accepts | Default | Purpose |
|---|---|---|---|
| `SQUEEZEFS_FREE_GRACE_MAX_MS` | int 1000..600000 | `derived (2 x one acknowledgement cycle)` | How long a terminally-freed offset may wait for a reader's acknowledgement before that reader is fenced and the offset released; derived = 2 × one acknowledgement cycle. A value below one cycle refuses. Inert unless a reader plane is armed. See [Freed-offset grace period](#freed-offset-grace-period-spec-68-item-3). |
| `SQUEEZEFS_ALLOC_LANE_HARVEST_AHEAD` | bool | `on` | A laned co-writer's background lane refill: harvest the lane's freed supply from the authority ahead of a stall when the allocator is owed blocks and lane-reachable stock sits below the derived watermark. `0` = ENOSPC-triggered harvests only. Engagement `alloc_lane_ahead_harvests`. |
| `SQUEEZEFS_FREE_GRACE_DEMAND` | bool | `on` | The freed-offset grace loop's demand arm: when recycle coupling is observed, a TTL'd demand mark asks the members holding the free list to renew at the floor cadence, widens the valve's bound refresh, and re-bases the runway on the lane-reachable supply. `0` = the pre-campaign valve (A/B control). |
| `SQUEEZEFS_FREE_GRACE_PASS_ELASTIC` | bool | `on` | A member adopting a prodded renewal cadence also tightens its reader revalidation pass cadence (more passes, never shorter qualification windows — the published staleness bound never moves). `0` = the routine pass cadence always. |
| `SQUEEZEFS_FREE_GRACE_ACK_PIPELINE` | bool | `on` | The reader's pipelined acknowledgement ladder: a bounded derived-depth FIFO of label candidates so acks advance one label per pass. `0` = the single-candidate ladder (A/B control). Reader-side; inert without a member session. |
| `SQUEEZEFS_FREE_GRACE_ACK_RENEWAL` | bool | `on` | Hold-time lever (b): a reader's promoted acknowledgement wakes its lease-renewal loop and renews as carriage (the ack travels now; the beat and the learned label stay the routine renewal's). `0` = the ack rides the next routine beat. Engagement `free_grace_ack_renewals`. |
| `SQUEEZEFS_FREE_GRACE_REFRESH_ON_ACK` | bool | `on` | Hold-time lever (d): a binding member's advancing acknowledgement marks the reallocation bound dirty and the next harvest recomputes it (rate-limited to the refresh floor ÷ members, never faster than 2× the measured scan). `0` = the floor-cadence / sweep refresh. Engagement `free_grace_bound_refreshes_on_ack`. |
| `SQUEEZEFS_FREE_GRACE_VALVE` | bool | `on` | The pressure-coupled release valve: as the grace ring's runway shortens the writer prods waiting members onto a shorter renewal cadence and slides the fence deadline toward the pressure bound (never below one acknowledgement cycle). `0` = routine bound + allocation-cliff bound only (A/B control). Engagement `free_grace_prods` / `free_grace_bound_tightenings`. |
| `SQUEEZEFS_FREE_GRACE_MAX_OFFSETS` | int 1..2^26 | `derived max(budget/1024/24 B, 131072)` | Per-volume cap on offsets held in the freed-offset grace period; derived from the memory budget with a field floor (≈ 120 k blocks — one acknowledgement cycle of saturated ingest). At cap the ring forces progress through the same fence act the deadline uses. |

**fsck / jobs**

| Knob | Accepts | Default | Purpose |
|---|---|---|---|
| `SQUEEZEFS_FSCK_SETTLE_MS` | int 0..600000 | `2000` | fsck suspect-settle window, ms (the zero-FP ladder). |
| `SQUEEZEFS_JOB_WIRE_BIND` | string | `off` | Job-shard execution wire listen address (VAL-6: the posture switch). |
| `SQUEEZEFS_JOB_WIRE_CA_CERT` | string | `none` | Job-wire CA certificate (PEM or DER). |
| `SQUEEZEFS_JOB_WIRE_CA_KEY` | string | `none` | Job-wire CA private key (PEM or DER). |
| `SQUEEZEFS_JOB_WIRE_ENROLL_FRESHNESS_MS` | int 0..600000 | `VAL-6 default` | Job-wire enrollment challenge freshness window, ms (0 is refused by the loader — it would expire every challenge). |
| `SQUEEZEFS_JOB_WIRE_MAX_CONNS` | int 0..2^20 | `derived from cores` | Concurrent job-wire connection cap (0 is refused by the loader — it would refuse every worker). |
| `SQUEEZEFS_JOB_WIRE_VERIFY_PERMILLE` | int 0..1000 | `1000 plaintext / sampled TLS` | Pre-publish verify-read sampling, per mille. |

**NVMe-oF target management**

| Knob | Accepts | Default | Purpose |
|---|---|---|---|
| `SQUEEZEFS_NVMEOF_TARGET_STACK` | spdk / nvmet | `probed` | Target stack selection; a malformed value never falls back to a default. |
| `SQUEEZEFS_NVMEOF_STATE_DIR` | string | `/var/lib/squeezefs/nvmeof` | Durable target-ledger directory. |
| `SQUEEZEFS_NVMEOF_RUN_DIR` | string | `/run/squeezefs/nvmeof` | Runtime directory for target sockets/pids. |
| `SQUEEZEFS_NVMET_PORT_ID_BASE` | int 0..2^32−1 | `derived` | kernel-nvmet port-id allocation base. |
| `SQUEEZEFS_SPDK_TGT_BIN` | string | `pinned install` | Override the spdk_tgt binary path. |

**Local file I/O**

| Knob | Accepts | Default | Purpose |
|---|---|---|---|
| `SQUEEZEFS_URING_FS_WORKERS` | int 1..64 | `derived` | `uring_fs` process-worker count for ad-hoc local file I/O. |

**Test seams (product code, armed only by suites and rigs — never in production)**

| Knob | Accepts | Default | Purpose |
|---|---|---|---|
| `SQUEEZEFS_TEST_UPLOAD_STALL_MS` | int 0..600000 | `0` | Test seam: stall each block upload, ms. |
| `SQUEEZEFS_TEST_WRITE_STALL_MS` | int 0..600000 | `0` | Test seam: stall the write handler, ms (the transport-lease-overlong pin). |
| `SQUEEZEFS_TEST_RECLAIM_STALL_MS` | int 0..600000 | `0` | Test seam: stall each reclaim batch, ms. |
| `SQUEEZEFS_TEST_THP_PREP_STALL_MS` | int 0..600000 | `0` | Test seam: stall each deferred session-arena THP prep job, ms (the fleet-launch admission pin). |
| `SQUEEZEFS_TEST_INVAL_TAIL_STALL_MS` | int 0..600000 | `0` | Test seam: stall the detached pipeline upload before its invalidation tail, ms (the 2026-08-04 tail-vs-writer race pin). |
| `SQUEEZEFS_TEST_CHECKOUT_STALL_MS` | int 0..600000 | `0` | Test seam: stall the write handler post-checkout inside its held block-lock window, ms (pairs with the inval-tail stall). |
| `SQUEEZEFS_TEST_ATTR_PUBLISH_STALL_MS` | int 0..600000 | `0` | Test seam: stall the write handler between its data phase and its size/attr postlude publish, ms (the KD-5/KD-6 attr-merge race pin — design-write-inode-convoy). |
| `SQUEEZEFS_TEST_NVME_READ_TIMEOUT_MS` | int 1..600000 | `30000` | Test seam: NVMe read timeout, ms. |
| `SQUEEZEFS_TEST_POWER_CUT_DEVS` | string | `none` | Test seam: comma list of device paths armed for the power-cut simulator. |
| `SQUEEZEFS_TEST_OVERLAY_BYTES` | bool | `0` | Test seam: force the overlay `Bytes` vehicle on even when `SQUEEZEFS_DEVICE_OVERLAY` is off (in-process suites). |
| `SQUEEZEFS_TEST_STAMP_BLOCK_REFS` | bool | `0` | Test seam: stamp incompat bit 9 (durable block refcounts) at format on a `--single-writer` base so the ledger can be graded in isolation. Never set in production. |
| `SQUEEZEFS_TEST_STAMP_WRITER_SCOPE` | bool | `0` | Test seam: stamp incompat bit 10 (writer-scoped staging) at format on a `--single-writer` base. Never set in production. |
| `SQUEEZEFS_NODE_ID_FILE` | string | `none` | Node-identity source file, ahead of `/etc/machine-id`, `/var/lib/dbus/machine-id` and `/etc/squeezefs/node-id` (writer-scoped staging); the bytes must be host- and reboot-stable. |

**Harness-only variables (suites, rigs, re-exec children — known to the registry, deliberately unvalidated)**


`SQZ_CLW_RTT_ENDPOINT` (RTT instrument: dial a REAL coordinator instead of a local loopback listener (tests/cluster_wire_tests.rs `rtt_row`)); `SQZ_CLW_RTT_SECRET_HEX` (RTT instrument: the target volume set's job:enroll secret, hex (required for a remote row)); `SQZ_CLW_RTT_SAMPLES` (RTT instrument: counted samples after the discarded warm-up); `SQZ_CLW_RTT_PAYLOAD` (RTT instrument: payload bytes per direction); `SQUEEZEFS_TEST_REQUIRE_MOUNT` (Turn mount-class skips into failures (TEST-2)); `SQUEEZEFS_TEST_REQUIRE_ALL` (Promote every skip class to a failure); `SQUEEZEFS_TEST_REQUIRE_HARDWARE` (Promote hardware-class skips); `SQUEEZEFS_TEST_REQUIRE_NON_ROOT` (Promote non-root-class skips); `SQUEEZEFS_TEST_REQUIRE_OPT_IN` (Promote opt-in-class skips); `SQUEEZEFS_TEST_REQUIRE_ROOT` (Promote root-class skips); `SQUEEZEFS_TEST_REQUIRE_SUDO` (Promote sudo-class skips); `SQUEEZEFS_TEST_REQUIRE_TOOLCHAIN` (Promote toolchain-class skips); `SQUEEZEFS_TEST_SKIP_LEDGER` (Path the skip ledger is appended to); `SQUEEZEFS_CRASH_CHILD_V3` (KV crash-suite re-exec child marker); `SQUEEZEFS_CRASH_CHILD_V3_BATCHED` (KV crash-suite batched-child marker); `SQUEEZEFS_CRASH_LEDGER` (KV crash-suite ledger path); `SQUEEZEFS_CRASH_ROUNDS` (KV crash-suite round count); `SQUEEZEFS_CRASH_VOL` (KV crash-suite volume path); `SQUEEZEFS_GUARD_CHILD` (Writer-guard suite child marker); `SQUEEZEFS_GUARD_HOLD_CHILD` (Writer-guard hold-child marker); `SQUEEZEFS_GUARD_OUT` (Writer-guard suite output path); `SQUEEZEFS_GUARD_READY` (Writer-guard readiness handshake path); `SQUEEZEFS_GUARD_VOL` (Writer-guard suite volume path); `SQUEEZEFS_WCE_CRASH_CHILD` (Volatile-cache crash-suite child marker); `SQUEEZEFS_WCE_CRASH_ROUNDS` (Volatile-cache crash-suite round count); `SQUEEZEFS_WCE_LEDGER` (Volatile-cache crash-suite ledger path); `SQUEEZEFS_D2_CRASH_CHILD` (Two-stage-conveyor crash-suite child marker); `SQUEEZEFS_D2_CRASH_ROUNDS` (Two-stage-conveyor crash-suite round count); `SQUEEZEFS_D2_LEDGER` (Two-stage-conveyor crash-suite ledger path); `SQUEEZEFS_D2_VOL` (Two-stage-conveyor crash-suite volume path); `SQUEEZEFS_WCE_VOL` (Volatile-cache crash-suite volume path); `SQUEEZEFS_M1_ROOT_DEV` (Meta-slot migration suite device path); `SQUEEZEFS_M1_ROOT_VICTIM_KEY` (Meta-slot migration suite victim key); `SQUEEZEFS_TEST_REQUIRE_CAPABILITY` (Promote capability-class skips); `SQUEEZEFS_RECLAIM_CRASH_CHILD` (Block-reclaim crash-suite child marker); `SQUEEZEFS_RECLAIM_CRASH_DEV` (Block-reclaim crash-suite device path); `SQUEEZEFS_RECLAIM_CRASH_LEDGER` (Block-reclaim crash-suite ledger path); `SQUEEZEFS_RECLAIM_CRASH_ROUNDS` (Block-reclaim crash-suite round count); `SQUEEZEFS_STAGED_CRASH_CHILD` (Staged-payload crash-suite child marker); `SQUEEZEFS_STAGED_CRASH_MANIFEST` (Staged-payload crash-suite manifest path); `SQUEEZEFS_STAGED_CRASH_META` (Staged-payload crash-suite meta path); `SQUEEZEFS_STAGED_CRASH_STAGING` (Staged-payload crash-suite staging path); `SQUEEZEFS_SCALE_DIR_ENTRIES` (Directory-scale suite entry count); `SQUEEZEFS_SKIP_LEDGER_CHILD` (Skip-ledger suite child marker); `SQZ_ALLOC_TRACE` (ipc op-economy allocation-site profiler); `SQZ_BSET_ALLOC_CHILD` (Bset allocation-authority repro child marker); `SQZ_FENCING_REMOUNT_CHILD_DIR` (Fencing-remount suite child directory); `SQZ_VAL3_HARDEN_CHILD` (Key-handling hardening repro child marker).

### SqueezeFS URI scheme

To centralize block storage connectivity, SqueezeFS utilizes two connection URIs:

* **Metadata Volumes**: `sqmeta://<path_to_block_device_or_file>` (e.g. `sqmeta://dev/xai-meta/mds01`).
* **Data Volumes**: `sqdata://<path_to_block_device_or_file>` (e.g. `sqdata://dev/xai-data/oss01`).

### Format (`squeezefs format`)

Initialize physical block maps and metadata. New metadata volumes are formatted as **v3** (CoW KV metadata — see [Format v3](#format-v3-cow-kv-metadata) and [Metadata Durability](#metadata-durability-crash-contract)). Executes concurrently across all target devices.

Since the multi-writer program's **Phase-B default flip** (rung 10b, user ruling 2026-08-15), a fresh format is **multi-writer-capable by default**: the nine multi-writer incompat bits (7–15) stamp on every metadata volume in one planned superblock write (KD-MW-1 — one act, never piecemeal). A stamped volume **mounts solo verbatim** (the stamped-solo S4 gate's posture — measured performance-invisible), and every repair/fsck verb runs under the D0-guarded open regardless of stamps; the one real difference is the **compatibility boundary**: pre-multi-writer binaries refuse a stamped set loudly.

```bash
squeezefs format sqmeta://<meta_dev> [sqmeta://...] sqdata://<data_dev> [sqdata://...] [options]
```

*Options:*
- `--block-size <bytes>`: Block size in bytes (e.g. `4M`, `1M`, default: `4M`). On compressed/encrypted volumes the effective block size is clamped so a worst-case (incompressible) stored image plus headroom fits its allocator chunk — the clamp prints loudly.
- `--capacity <bytes>`: Formatted capacity (default: the summed physical size of the data volumes). May be **lower** than physical (useful for testing); values above physical are refused — thin-provision underneath via LVM/fabric instead.
- `--inodes <count>`: Hard quota limit for number of inodes (default: `1000000`).
- `--disk-cache-paths <paths>`: Comma-separated paths to NVMe cache staging directories. **Declared here, at format** — recorded in the format config as the single source of truth. Omit it and the filesystem is **permanently cache-less**: mounts run with RAM tiers + direct block I/O only (no NVMe staging/read-cache tier). Change later with `squeezefs config set-cache-paths`.
- `--compression <lz4|zstd|none>` / `--encrypt-algo <aes256gcm|chacha20|none>` / `--encrypt-key <path>` (`-` = stdin): transparent per-volume compression / client-side encryption. `--encrypt-key` names a key **file** — never the key itself, which would land on `/proc/<pid>/cmdline` (see [Transparent compression & encryption](#transparent-compression--encryption)).
- `--mem-cache-size` / `--disk-cache-size` / `--{read,write}-cache-size` / `--{read,write}-mem-cache-size`: cache budget defaults recorded in the format config (overridable per mount).
- `--single-writer`: Format the **unstamped (pre-flip) class** — none of the nine multi-writer bits. For recovery scratch volumes and anything a **pre-multi-writer binary** must be able to read; fixing a filesystem never *requires* it (see above). Upgrade later with `squeezefs volume enable-multi-writer`. Conflicts with `--multi-writer` (two contradictory class declarations refuse loudly).
- `--multi-writer`: Accepted, **announced-inert** — multi-writer-capable is the default since the Phase-B flip; the flag survives as the forward spelling from the dark-opt-in era and has no effect.
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
- `--log-file <path>`: Path to write daemon logs to when running in background. Created **mode 0600 with `O_NOFOLLOW`** (VAL-7h): the log carries backing-device paths, staging directories and object key names, and a symlink planted at the target would have had a root daemon appending through it — a symlinked target now refuses loudly (`ELOOP`) and, per ENG-3, fails the mount rather than silently redirecting. Appends never truncate, and an existing file's mode is left as the operator set it.
- `--admin-uid <uid>` (= `-o admin_uid=<uid>`): the uid admitted on the ADMIN control lane, which serves the mutating maintenance verbs (`squeezefs job …`, `volume …`, health overrides). The lane admits uid 0 plus exactly this uid. Default: the invoking owner — which under `sudo` derives from `SUDO_UID`, i.e. caller-controlled environment, which is why the explicit surface exists (VAL-7c). An unparseable value keeps the default and warns; it never widens to root. The lane also runs the data plane's full screening ladder — ABI + `build_commit` equality, then bootstrap-nonce freshness, then `SO_PEERCRED` — so a CLI from a different build refuses instead of driving mutating verbs against durable state it may encode differently.
- `--mem-budget <size>`: the daemon's joint memory budget (shed-don't-OOM authority) — see [Hybrid I/O](#hybrid-io-for-o_direct-reads-default-and-the-device-true-escape).
- `--mem-cache-size <size>`: System RAM cache size (e.g. `16GB` or `20%`); the other cache-size family flags override the format-config defaults the same way.
- `--uid <uid>` / `--gid <gid>`: presented owner of files in the mount (presentation-only; staging I/O runs as the mounting user).
- `-o <opts>`: FUSE options, including the per-class kernel TTLs (`attr_timeout`, `entry_timeout`, `dir_entry_timeout`, `negative_timeout`), `max_background` / `congestion_threshold` INIT overrides, and `direct_device_true` — each documented in its section below.
- `--no-writeback`: disable the FUSE writeback cache (enabled by default).
- `--read-only` (= `-o ro`): mount as a coherent **reader** (DLM S5) — no write lease, no `writer_claim`, no NVMe reservation, every plane refused, kernel `MS_RDONLY`, writeback cache off, and kernel/dentry TTLs derived from the writer's checkpoint cadence. Combining it with an explicit `-o rw` refuses loudly. Read the exact consistency guarantee first: [Read-only coherent mounts](#read-only-coherent-mounts--o-ro--one-writer-plus-n-readers).
- `--interception` (= `-o interception` = `SQUEEZEFS_IPC=1`): arm the L4 LD_PRELOAD interception session host for this mount — see [LD_PRELOAD interception](#ld_preload-interception--o-interception--security-posture--unsupported-mixes).
- `--write-verification` (+ `--write-verification-sample <N>`): opt-in read-after-write checksum verification.
- `--dismount-wait <secs>` / `--upload-delay <dur>`: staging drain window on dismount / background upload cadence.

### LD_PRELOAD interception (`-o interception`) — security posture & unsupported mixes

*Hands-on benchmarking walkthrough (build → mount → run → verify engagement): `QUICKSTART.md` → "Benchmarking the LD_PRELOAD Interception Path (Manual)". Measured reference numbers: `.benchmarks/2026-07-19-l4-interception-closing.md`.*

Opt-in at **both** ends (`docs/design-preload-interception.md`, v1 posture): the mount arms the session host (`--interception` / `-o interception` / `SQUEEZEFS_IPC=1`), and each app opts in with `LD_PRELOAD=libsqueezefs_il.so` (built with `cargo build -p squeezefs-preload --profile preload-release --features interposers` — the ONLY sanctioned build; a plain `--release` build refuses at compile time because the root profile's `panic="abort"` would turn shim panics into host-app aborts). Everything the shim cannot serve identically falls through to the real fd via kernel FUSE — **correctness never depends on interception**. Data ops on bound fds ride a shared-memory ring; warm reads serve synchronously from the daemon tiers (measured ~2× kernel-FUSE warm rand-4k on the dev substrate); everything else rides an async handoff into the exact FUSE handler bodies. The per-PR gate is `tests/run_preload_gate.sh` (leg 1 unprivileged; leg 2 root: parity + engagement + dup/close_range/lseek rows + kill-9 and fork-kill-parent soaks).

**KD-11 — interception forces kernel write-through.** `-o interception` flips the mount to `write_back = false` (the same knob `--no-writeback` drives): the default-on kernel writeback cache acks buffered writes before the daemon sees them, and a ring read (direct-to-daemon by construction) would miss them. Combining `-o interception` with an explicit `writeback`/`writeback_cache` request is a contradiction and **refuses the mount loudly**. Cost: buffered kernel-path small writes on interception mounts lose the kernel's dirty-page batching (interception mounts exist to take the ring path for exactly those writes).

**Direct link (`-lsqueezefs_il`) — the supported linked mode (SDK Tier 1, `docs/design-sdk.md` §4).** Applications may link the shim as an ordinary shared library instead of preloading it — same interposers, same fallback ladder, same engagement discipline; the shim is ctor-free (lazy first-call init), so load order needs no ceremony. What linked mode buys: it survives everything that strips `LD_PRELOAD` — **setuid/AT_SECURE binaries** (the dynamic linker ignores untrusted preloads there), `sudo`'s and systemd's environment scrubbing, and container images that never propagate env vars — making interception a property of the *binary*, not of how it was launched. Rules:
- **Link order requirement:** the shim must precede `libc` in the dependency list (automatic — `-lc` is appended last) and should come **first** in the link line so no other library can shadow the interposed symbols. Because modern toolchains default to `--as-needed`, wrap the reference: `cc app.c -Wl,--no-as-needed -lsqueezefs_il -Wl,--as-needed …` — otherwise an app that happens to reference no interposed symbol at static-link time silently drops the DT_NEEDED entry (no interception, no error). The cdylib carries `SONAME libsqueezefs_il.so` (so by-path links record a clean name; provide `-L`/rpath or `LD_LIBRARY_PATH` at run time) and `-z nodelete` (`pthread_atfork` handlers can never be unregistered — a loaded shim is permanent by design; `dlclose` is a structural no-op).
- **Failure-mode difference, stated:** a missing DT_NEEDED library is **fatal at exec** ("error while loading shared libraries"), whereas a missing `LD_PRELOAD` entry is only a loader warning. A linked binary must ship with its `.so`.
- **Same KD-7 rules, verbatim:** linked mode changes delivery, not the version lock — the shim still binds only against the identical `build_commit` + `IPC_ABI` (refusal ⇒ passthrough with the reason line), so a linked app must refresh its `libsqueezefs_il.so` alongside daemon upgrades (the `dist/<target>` same-commit pairing-folder discipline). App-cadence delivery with a compat window is the SDK program's charter (`docs/design-sdk.md` SKD-4), not the shim's.
- **Detection line:** on first contact with an interception-armed mount, a shim that was *not* named in `LD_PRELOAD` prints once per process: `squeezefs-il: active via direct link (DT_NEEDED), not LD_PRELOAD — same KD-7 build pairing applies` (also covers `dlopen` and `/etc/ld.so.preload` loads; a setuid binary whose ignored environment still names the shim misreports as preloaded — the line is diagnostic, never a correctness input). The gate battery pins linked-mode parity, ctor-context I/O, scope occupancy, and this line (`tests/run_preload_gate.sh` legs 1e / 2b-linked).

**Security posture (the §5.2 daemon fd screen is the boundary):**
- The bind credential is a **real open fd** passed over an abstract AF_UNIX socket (`SCM_RIGHTS`). The daemon re-derives everything from the received fd itself: `O_PATH` descriptions are refused outright (obtainable with search-only permission — accepting one would grant reads without read permission), non-regular files refuse, `st_dev` must match the mount, and per-op rights come strictly from the description's access mode **in both directions** (an `O_WRONLY` binding cannot ring-read; an `O_RDONLY` binding cannot ring-write — both surface as `EBADF`). `O_APPEND`, `O_SYNC`/`O_DSYNC`, and `O_TMPFILE`-class (unnamed regular file, `st_nlink == 0` — which also conservatively refuses open-then-unlinked fds; passthrough serves them) refuse at bind.
- **Version lock (forward-only):** sessions bind only between identical builds (`build_commit` equality + `IPC_ABI`). Degenerate identities — `unknown` (no-git tarball) or `-dirty` — refuse on *either* side; `SQUEEZEFS_IPC_ALLOW_DEV=1` is the dev-box override, **counted** in `.stats` `ipc_binds_dev_override` (nonzero outside dev boxes is a fleet-hygiene alarm).
- **Multi-user (`--allow-other`) posture:** any uid that can open files on the mount can establish sessions. Sessions are per-process, arenas are private mappings (no cross-process payload visibility), per-uid session caps and the R5 `ipc_session_arenas` budget component bound resource use (shed = refuse-new-sessions, never tearing live ones). The trust model is exactly POSIX-fd trust plus resource caps. `SO_PEERCRED` labels accounting and is defense-in-depth, not the authorizer.
- **Observability:** `.stats` carries the refusal ledger (`ipc_bind_refused_{version,nonce,flags,mode,budget,peercred}`), lifecycle gauges (`ipc_sessions_{active,total}`, `ipc_arena_bytes`, `ipc_binds`, `ipc_admission_refusals`) and two **must-stay-0 tripwires**: `ipc_descriptor_rejects` and `ipc_sessions_poisoned` — nonzero means a client bug or an attack (one loud log line per event).

**Unsupported mixes (documented contract, not detected):**
- **Concurrent cross-process `MAP_SHARED` mmap-writers + ring writers on the same file** — page-granularity writeback can clobber ring-written bytes (lost updates). Same-process mmap is fully handled (POSIX-7): the shim unbinds *all* in-process bindings on the mapped inode AND **poisons** the inode, so a later `open()` of the same file — or an `mmap()` that preceded the first open — can never re-arm the ring underneath a live mapping; that fd stays kernel-served for the process's lifetime (the poison set is fail-safe: if it ever fills, the whole mount degrades to kernel-served rather than forgetting an entry). Cross-process remains declared unsupported — a peer's mapping is invisible to this shim and the kernel never tells the daemon about mappings; run such workloads without the shim.
- **Cross-process buffered/mmap readers** can observe a bounded staleness window on ring-written data (same class as attr-TTL staleness); the daemon's `notify_inval_inode` handoff bounds it — fired on bind and rate-limited per `(ino, window)` on ring writes (`SQUEEZEFS_IPC_INVAL_WINDOW_MS`, default 1000; `.stats` `ipc_inval_{notifies,suppressed}`), delivered over the classical sideband even on armed over-uring sessions. **Size coherence is exempt from that window (POSIX-8):** a ring write that GROWS the file always fires an attrs-only invalidation (`.stats` `ipc_inval_attrs_only`) — no page-cache work — so `lseek(SEEK_END)` and `stat` never read a stale `i_size` and an append can never land at a stale offset; the last unbind of an inode fires one whole-inode shootdown so nothing the window suppressed outlives the bindings.
- **Mixed-ABI fd lifecycles**: apps that close *and* recreate fds exclusively through raw `syscall(2)`/io_uring (invisible to the shim) and then issue libc data calls on the reused number are unsupported under the shim (`SQUEEZEFS_IL_PARANOID_FSTAT=1` is the triage knob).
- **Containers with their own network namespace** *(solved in v1.1 — OQ-6)*: the abstract-socket rendezvous is per-netns, so pre-v1.1 such apps silently stayed on kernel FUSE. Since v1.1 the daemon **also binds a filesystem-path ctl socket** and advertises it in the bootstrap blob; the shim's connect ladder tries abstract first (same-netns fast path), then the path. **Operator contract for container fleets:** bind-mount the socket runtime dir into the container alongside the filesystem — default `/run/squeezefs` (root mounts) or `$XDG_RUNTIME_DIR/squeezefs`, else `/tmp/squeezefs-il-<uid>` (user mounts); override with `SQUEEZEFS_IPC_SOCKET_DIR=<dir>` (`none` disables, restoring the v1 zero-residue posture). The socket file is mode 0666 **because connecting is not a credential** — `SO_PEERCRED` + the daemon fd screen remain the security boundary, identical over both rendezvous. Note the user-mount default under `$XDG_RUNTIME_DIR` is a 0700 dir: other uids cannot reach it (user mounts serve same-uid apps; point `SQUEEZEFS_IPC_SOCKET_DIR` at a shared dir if you need more). A failed path bind degrades loudly to abstract-only and never fails the mount; the file is unlinked at daemon shutdown (zero residue restored), and a same-name stale file from a crash is replaced at the next spawn (names embed pid+random, so a collision is always our own residue).

*Env knobs:* `SQUEEZEFS_IPC=1` (arm), `SQUEEZEFS_IPC_ARENA_MB` (per-session payload arena, explicit MiB wins verbatim; default derived = `max(64 MiB, DMA-aligned admission-cap ÷ max(128, cpus × 8))` — the population-derived fraction, 2026-08-04: the divisor is the session population the cap is sized to hold (`cpus × 8` = the matched-inflight client-fleet slope; the retired fixed /128 admitted only half of a 256-process matched-inflight fleet on a 32-CPU/176 GiB-budget client — `ipc_admission_refusals`, engagement-INVALID rows), 64 MiB is the shipped floor and boxes with `cpus × 8 < 128` keep the old cap/128 arithmetic exactly), `SQUEEZEFS_IPC_MAX_OP_BYTES` (default 1 MiB), `SQUEEZEFS_IPC_MEM_PCT` (session-shm admission cap as a percent of the resolved memory budget, clamp (0,100] — the preferred spelling), `SQUEEZEFS_IPC_MEM_MAX` (MiB; absolute session-shm admission cap, explicit-wins-verbatim — compat spelling), default with neither set = 12.5 % of the budget (`budget/8`, no fixed ceiling; precedence absolute > percentage > derived), `SQUEEZEFS_IPC_ALLOW_DEV=1` (counted dev-build skew override), `SQUEEZEFS_IPC_SERVICE_THREADS` (service-thread ceiling; default = the drain-LANE derivation `clamp(3×cpus/8, 2, 64)` — the counted 2026-08-06 field width sweep, `.benchmarks/2026-08-06-dd-width-slope.md`: one lane = svc submitter + direct-drive reaper = 2 OS threads, sized so the lane-pair population takes ¾ of the core budget; it dominates the shim's `clamp(cpus/4, 2, 16)` session default at every machine size, so every default session still owns a drain thread and spawn-on-bind never parks a spare — threads spawn on session admission), `SQUEEZEFS_IPC_DD_SHARDS` (direct-drive uring shards — one ring + pinned reaper per shard, one lane per service thread; default = the SAME drain-lane derivation — the pair moves together: a shard set wider than the ceiling is production-dark — override/measurement lever clamp 1..=64), `SQUEEZEFS_IPC_DD_INLINE_REAP` (reaper/drain fusion, default ON — the owning service thread drains its lane's direct-drive CQ inline, zero syscall; `0` = the A/B lever, and kernels without `IORING_ENTER_EXT_ARG` auto-disarm it loudly; engagement gauge `.stats` `ipc_direct_inline_reaps`; shim-iops campaign 2026-08-07), `SQUEEZEFS_IPC_DD_EAGER_FLUSH` (mid-sweep direct-drive enter once a lane's unflushed SQE count reaches K; default 0 = end-of-sweep only, the shipped M3 submit-batch posture — a counted measurement lever, `.benchmarks/2026-08-07-shim-iops.md` §2.1), `SQUEEZEFS_IPC_DD_LANE_FLUSH` (default ON — a service thread's sweep-end flush enters only its OWN direct-drive lane's ring; the r3 drain-funnel fix, `.benchmarks/2026-08-08-shim-drain-funnel-r3.md`: the flush-ALL sweep serialized every svc thread on every shard's kernel uring_lock — 31 % of svc cycles spinning, the field's ring-ingress pool; `0` = the pre-r3 flush-all A/B lever; dd rings also build with `IORING_SETUP_COOP_TASKRUN` where the kernel offers it, degrading loudly pre-5.19), `SQUEEZEFS_IPC_IDLE_SECS` (idle-session reap, default 300, 0 = off; `.stats` `ipc_sessions_reaped`), `SQUEEZEFS_IPC_INVAL_WINDOW_MS` (W1 invalidation rate window, default 1000), `SQUEEZEFS_IPC_SOCKET_DIR` (path-socket runtime dir, `none` = abstract-only — see the container-netns entry above). Client side: `SQUEEZEFS_IL_OP_TIMEOUT_MS` (per-op ring deadline; timeout poisons the session → passthrough), `SQUEEZEFS_IL_REAP_QUANTUM_US` (the libaio reap's deep-regime quantum, default the shipped 50 µs, clamp 1..=1000 — since the reap-fanin campaign, 2026-08-08, it is the batch-threshold doorbell park's AGE BOUND rather than a blind sleep: the k-th completion cuts the wait short (k = `clamp(pending/4, 2, pending)` derived per session, never a knob) and the daemon pays ≤ 1 wake per k completions; worst case identical to the retired sleep — `.benchmarks/2026-08-08-shim-reap-fanin.md`). The daemon-side dequeue exports **`ipc_ingress_ns`** (always-on histogram): the MEASURED client-publish→daemon-dequeue ring-ingress residence via the v5 slot ingress stamp — an il row is INVALID unless its count delta accounts for the row's ring ops.

*Data-plane observability:* `ipc_ops_{read,write}` / `ipc_bytes_{in,out}` are the **engagement instrument** — an interception benchmark row is only valid if their deltas account for the row's ops (silent passthrough measuring kernel FUSE is the failure mode the check exists for); `ipc_fast_path_serves` vs `ipc_async_handoffs` + the `ipc_fast_path_{lock,miss}_demotions` split are the fast-path health signal.

### Multi-user mounts — the single-tenant resource posture

**Stated posture (VAL-7d, pre-RC engineering spec §3): one SqueezeFS mount is a SINGLE resource tenant. There is no per-uid quota, no per-uid rate limit, and no per-uid accounting anywhere in the FUSE data path — by design, not by omission.** An operator sharing one mount between untrusted workloads must not expect resource isolation from SqueezeFS; use one mount per tenant, or a cgroup/container boundary around each workload, and give each its own volume set.

What this means concretely:

- **Every R5 memory-budget component is process-global.** The gauges (`mem_budget_components{…}`) aggregate the whole daemon: staged payloads, parked extents, transport payload arenas, IPC session arenas and severed buffers, write-pipeline in-flight bytes, job copy buffers, read-lane holds. One workload that drives the budget to **Red** therefore pauses maintenance jobs, tier publishes and dehydration *for every user of that mount*. That is the designed Red semantics (`docs/design-read-path.md` §5.7 — converge by completion, never OOM); it is not partitioned by uid, and the pause is fleet-visible in `mem_budget_{level,red_events}`, `job_paused_mem_pressure` and `read_tier_publishes_paused`.
- **Nothing rate-limits a uid's op stream.** The bounds that exist are *global* and structural — the transport's `queues × q_depth` in-flight ceiling, the write-pipeline BDP admission target (`write_pipeline_admission_waits` is the honest-backpressure gauge), the bounded writeback queue, the reclaim queue — so a heavy workload's backpressure is felt by everyone on the mount. Fairness between uids is the kernel's I/O scheduler and cgroup v2's job, not the daemon's.
- **What IS per-uid.** Exactly two things, both on the interception control plane, both admission caps rather than accounting: `per_uid_session_cap` (concurrent IPC sessions per uid — derived, `clamp(session-shm admission cap ÷ per-session arena, 64, 4096)`, i.e. the session population the budget can actually hold; `mem_budget::ipc_per_uid_session_cap`, no knob of its own — the budget knobs are the levers) and the `ipc_session_arenas` R5 component's shed = refuse-new-sessions behavior. Neither bounds the FUSE path.
- **Access control is separate and IS enforced.** `-o default_permissions` puts POSIX mode/ACL checks in the kernel, POSIX advisory locks are kernel-arbitrated per mount (see the DLM section in [AGENTS.md](../AGENTS.md)), the reserved-xattr screen keeps daemon records invisible through FUSE (`fuse_reserved_xattr_refusals`), the `.stats`/`.config` inodes are `0400` owned by the mount uid with their key census opt-in (`SQUEEZEFS_STATS_KEY_CENSUS=1`), staging/read-cache trees are `0700`/`0600`, and the ADMIN lane admits only uid 0 or `-o admin_uid=N`. **Confidentiality and integrity are multi-user-safe; resource consumption is not partitioned.**
- **Why this is the honest answer for the design target.** The 15,000+-node target (ruling D1) scales by NODES: each node runs its own daemon over its own mount, so cluster multi-tenancy is a *scheduler* property (one mount per training job / per container), and per-uid accounting inside a single daemon would buy nothing for it while adding a per-op charge to the hot path. If a future deployment genuinely needs several untrusted uids sharing one daemon's budget, that is a new program — per-uid R5 sub-budgets plus a per-uid admission gate — and it must arrive with its own measured cost, not as a claim made here.

### Fleet-share sizing (co-located daemon fleets)

**Every resource derivation in the tree reads the WHOLE machine** — the R5
memory budget, transport payload arenas, drain-lane ceilings, conveyor
batches, blocking pools. That is correct at one daemon per host and wrong by
N× the moment N daemons share a machine (same-machine multi-mount clients
are a product surface — each mount is its own client identity,
`docs/design-full-multi-writer.md` §5). The sizing law (KD-MW-14):

| Knob | Default | Purpose |
|---|---|---|
| `SQUEEZEFS_FLEET_SHARE` | `1` (= today's whole-machine posture) | Integer ≥ 1: a divisor applied **once, at the root inputs of the derivation tree** — the effective memory budget and effective CPU count become `ceil(system / share)`, and every downstream derived value scales through the existing formulas untouched. Set it to the number of co-located daemons, per daemon (the fleet rig exports it automatically; it is never auto-detected — daemons discovering each other to divide a machine would be a coordination plane where a knob suffices) |

The rules an operator must know:

- **It modifies the derived tier only.** Explicit absolute knobs still win
  verbatim; percentage knobs apply to the shared budget
  (`absolute > percentage > derived`, unchanged).
- **Floors are never divided.** Physical minima and never-regress-below-
  shipped floors hold per daemon regardless of the share; a share that
  cannot satisfy its floors **refuses loudly at startup, naming the
  arithmetic** — never a silent clamp below a floor.
- **Kernel-mandated geometry is exempt** (and pinned by a tie test): the
  FUSE-over-io_uring queue COUNT is one queue per kernel possible CPU per
  mount — a session with fewer never becomes ready — so N daemons always
  hold N × possible-CPUs queues. The *memory* behind those queues DOES
  scale: per-queue depth degrades to fit the divided payload-buffer cap.
- **The cgroup-unreclaimable pressure arm divides through the same root**:
  in a shared cage the arm reads the CAGE's residue, so without the divisor
  all N quiet daemons read each other's anon as their own pressure — the
  proven failure was all 32 daemons in Red with tier publishes paused
  fleet-wide (`.benchmarks/2026-08-16-mw-s6-arm.md`, finding #4). Per-process
  RSS stays undivided, so each daemon's own balloon is still policed at
  full strength.

Proven at N=32 on one 32-CPU box (the S6-a row: R5-pressure columns flat
fleet-wide — `mem_budget_red_events` 0, `hard_backstops` 0,
`parked_gate_timeouts` 0 on all 32 daemons). Tie tests:
`tests/derivation_sweep_tests.rs` (share=4 quarters every divisible derived
cap; the exemption list is pinned to exactly the kernel-mandated set).

### Cache/staging paths (`squeezefs config`)

Changing cache/staging directories is an admin op, guarded like `format` (refused while any client has the volume mounted); it rewrites the format config and wipes the new directories so the next mount stamps a fresh staging generation.

```bash
squeezefs config set-cache-paths sqmeta://<meta_dev> <path> [<path>...]
squeezefs config get-cache-paths sqmeta://<meta_dev>
```

### Transparent compression & encryption

Optional per-volume transforms declared at format: `--compression lz4|zstd` and `--encrypt-algo aes256gcm|chacha20`, applied across all three write layouts. Compression is **best-effort per block**: an incompressible block is stored raw (frame-flagged, counted as `compress_stored_raw` in `.stats`) instead of expanding — and transformed volumes reserve per-chunk headroom at format so worst-case images always fit (see [Breaking changes](#breaking-changes--migration-notes)).

**The encryption key never touches the volume it protects, and never rides `argv`** (design: `docs/design-key-handling.md`). Generate one, then keep it somewhere the daemon can read and nobody else can:

```bash
head -c 32 /dev/urandom | base64 > /etc/squeezefs/keys/volume.key
chmod 600 /etc/squeezefs/keys/volume.key

squeezefs format sqmeta://<meta> sqdata://<data> \
  --encrypt-algo aes256gcm --encrypt-key /etc/squeezefs/keys/volume.key
```

- The file must hold **at least 32 bytes of key material** (not a passphrase — the derivation has no work factor), be a regular file, mode `0600`, owned by the invoking user. It is opened `O_NOFOLLOW` and checked on the fd; anything else refuses loudly. `--encrypt-key -` reads the material from **stdin** instead.
- `format` prints a **key id** and persists only that plus a derivation salt in the volume's format config. Losing the key file loses the data: nothing on the volume can reconstruct it.
- **Mounting** resolves the key, in order, from `mount --encrypt-key <path>`, `SQUEEZEFS_ENCRYPT_KEY_FILE=<path>`, then `/etc/squeezefs/keys/<key_id>.key` — so a key file parked at the default path needs no flag at all. A missing key refuses the mount naming all three sources; a *wrong* key refuses naming the expected and observed key ids (never a mount that fails every read).
- Data keys are wrapped with AES-256-GCM/ChaCha20-Poly1305 under an HKDF-SHA-256-derived key-encryption key (the volume's own record cipher). A process holding key material is set undumpable with core dumps disabled.


### Read-path tuning (mount env; design `docs/design-read-path.md`)

Defaults are the measured sweet spot — override only with a live-counter reason (the `.stats` inode exposes every family):

- `SQUEEZEFS_READ_TIER_ADMISSION` (`second-touch` default | `always` | `never`): NVMe read-tier admission for >256 KiB fills. `second-touch` kills the streaming publish tax (a cold 16 GiB pass writes ~0 instead of ~16.9 GiB to the tier) while re-read heat still converges to the tier; `always` restores unconditional first-touch publishes (A/B escape hatch).
- `SQUEEZEFS_READ_HOT_BLOCK_CACHE_MB`: RAM hot-block tier budget for >256 KiB blocks (default derived from the read-mem cache; `0` disables the tier and admission auto-degrades to `always`).
- `SQUEEZEFS_READ_PREFETCH_WINDOW` (default `16`, `0` disables): per-stream prefetch pipeline depth cap in blocks. The window is adaptive (2→cap, AIMD) and contention-scaled; the cap is a ceiling, not a target.
- `SQUEEZEFS_READ_PREFETCH_SHARE_PCT` (default `50`): the prefetch pipeline's share of the hot-tier budget in the contention-scaling formula — lower it if concurrent stream count routinely exceeds hot-tier capacity.
- `SQUEEZEFS_READ_RANGED_THRESHOLD` (default `262144`, `0` disables): reads at or under this size on passthrough (uncompressed/unencrypted) volumes fetch only their 4 KiB-aligned device window instead of the whole block — the rand-4k amplification kill (≈1000× → ~1.0×). Compressed/encrypted volumes always fetch whole blocks (decode requirement).
- `SQUEEZEFS_READ_DEST_LEASE` (default on, `0` = the A/B control): the READ **dest-window lease** (copy-elimination phase 1, 2026-08-06). Cold, 4 KiB-aligned, sub-block kernel reads on passthrough volumes DMA device bytes **straight into the reply's registered payload window** — the per-byte serve copy (1.00 CPU passes/byte, ~1/2.7 of the read CPU budget on a CPU-walled client) is deleted, and the speculative fill machinery stands down for such traffic (a pooled ahead fill for a window the demand read DMAs itself is a double-fetch). Trade stated honestly: lease-served bytes are device-true — re-read-heavy cold loops pay device fetches instead of tier warmth (fitting warm sets still serve from tiers other traffic populated). `0` restores the pre-campaign fill+serve-copy shape byte-identically. Engagement: `read_dest_lease_bytes` in `.stats` (subset of `read_dest_dma_bytes`).

### Hybrid I/O for O_DIRECT reads (default) and the device-true escape

**Hybrid I/O (default, user directive 2026-07-15):** O_DIRECT reads get the best of both worlds — they keep bypassing the *kernel page cache* (the kernel's side of O_DIRECT, unchanged) while serving from and admitting into *SqueezeFS's own read tiers* exactly like buffered reads. Tier hits serve from RAM (binding-validated — the ~536–558 k IOPS class on tier-resident data, `.benchmarks/2026-07-15-hybrid-io.md` + the RW5 close §3b); misses use **evidence-based admission** — first touch of a block reads the device (device-true, nothing admitted: streaming/scan pollution protection), a **second touch within the ghost window** admits the block (one whole-block fetch → RAM hot tier + NVMe read tier), so re-read-heavy O_DIRECT workloads (rand-4k databases, repeated scans) converge to RAM speed after one warm-up pass. Admission pauses under memory-budget Red. Watch `read_odirect_tier_serves` / `ranged_read_ghost_escalations` in `.stats`.

- **`-o direct_device_true`** (mount option) / **`SQUEEZEFS_DIRECT_DEVICE_TRUE=1`** (daemon env): the **measurement/diagnostic escape** — O_DIRECT reads become strictly device-true (no tier serve, no admission, no ghost recording, no prefetch classification; every O_DIRECT read is a validated device read of exactly its aligned window). This is the posture for device-path benchmarking and the `.benchmarks` amplification methodology (`squeezefs bench --direct` prints which posture the mount carries by sniffing `.stats`). Buffered traffic on the same mount keeps full hybrid behavior. Mode visible as `"direct_device_true"` in `.stats`; adoption counted by `read_device_true_reads`.
- `--mem-budget <size>` (mount flag) / `SQUEEZEFS_MEM_BUDGET_MB`: the daemon's joint memory budget. Unset, the budget follows cgroup v2 `memory.max` × 0.8 (re-read every second — a runtime-lowered cage tightens the budget live), else 70 % of RAM. Under pressure the daemon sheds (early flushes, cache clamps, prefetch pause) instead of OOMing; watch `mem_budget_level`/`mem_budget_red_events` in `.stats`.
- `SQUEEZEFS_TRANSPORT_MEM_MAX` (MiB, absolute) / `SQUEEZEFS_TRANSPORT_MEM_PCT` (percent of the budget): the FUSE-over-io_uring payload-arena cap. Default with neither set = budget/8, **no fixed ceiling** (derivation sweep 2026-08-04 — the retired 2 GiB ceiling degraded ring depth on > 64-CPU big-RAM boxes; `SQUEEZEFS_TRANSPORT_MEM_MAX=2048` restores it verbatim as the A0 lever). Precedence absolute > percentage > derived.
- `SQUEEZEFS_PARKED_BUFFERS` (count, absolute): the parked-write buffer budget (buffers' worth of block size). Explicit wins verbatim; default derived = `max(256, budget/16 ÷ block_size)` (derivation sweep 2026-08-04 — 256 is the shipped never-regress floor; the R5 authority gauges and sheds these bytes, and Red halves the effective cap).
- `SQUEEZEFS_URING_FS_WORKERS` (count, clamp 1..=64): the io_uring file-worker pool for ad-hoc local file I/O. Default derived = `clamp(cpus/4, 4, 64)` of process parallelism (derivation sweep 2026-08-04 — the retired `clamp(nproc, 4, 8)` pinned every ≥ 8-CPU box at 8 with no basis).

### Random-small-write path (sole-owner patch + extent overlay; design `docs/design-random-small-writes.md`)

Small random overwrites of striped files no longer pay a whole-block read-modify-write. Two levers, both default-on (program Implemented 2026-07; closing evidence `.benchmarks/2026-07-17-rand-write-program-closing.md`):

- **Sole-owner extent patch (W1)**: an isolated, LBA-aligned, non-extending small write to an exclusively-owned, passthrough, whole-block-mapped striped block becomes **one in-place sub-block DMA** — zero reads, zero metadata commits, zero staging (354–397 → 61–67 k IOPS on the 4 KiB random-write shape; device amplification ~1× writes). Sequential streams are predicate-excluded (adjacency guard) and keep the whole-block write-through economy.
- **Extent overlay + batched fold (W2)**: patch-ineligible shapes (compressed/encrypted volumes, refcount-shared blocks post-clone, holes, unaligned) park 4 KiB-class extents instead of 4 MiB buffers, spill as checksummed staging *extent records* (never a seed read at spill), and fold into blocks lazily — compressed-volume rand-write amplification ~2,500× → 15–26×.
- **Torn-extent durability note (the v1 aligned-only contract)**: a patch rewrites **only device sectors wholly inside the application's own write range** — bytes the application never wrote are never rewritten, so a crash can never perturb foreign data. The residual exposure is a per-sector old/new mix *strictly inside an un-fsynced in-flight write* (POSIX-legal; fsync acks only after DMA completion — the write ACK on this shape is *stronger* than before, since data reaches the device before ACK instead of a parked buffer).
- **The device overlay's length floor (finding 47, 2026-09-02)**: the same cap is the device overlay's (`docs/design-overlay-overwrite.md` §5.1) minimum segment length — a sub-cap aligned write is this program's whatever its block's state (hole, clone-shared, decorated, co-writer-refused, stream-adjacent), never an overlay store that would mint a whole CoW dest per touched block and read + re-seed the whole old image at settle. `overlay_ineligible_sub_cap` counts the floor engaging (≈ per sub-cap aligned write on overlay-armed mounts by design); `overlay_gap_seed_old_bytes` growing on a rand-4k row while it stays flat is predicate rot.
- Knobs (acceptance/diagnostic, not operational escape hatches): `SQUEEZEFS_PATCH_MAX_BYTES` (default derived = block_size/8 — 512 KiB on the shipped 4 MiB block; explicit wins verbatim, `0` disables the patch path — A/B lever — and, by the same act, empties the overlay's sub-cap class so every aligned length is overlay-eligible), `SQUEEZEFS_FOLD_MAX_EXTENTS` / `SQUEEZEFS_FOLD_MAX_BYTES` (fold triggers, default 64 / block_size/4 = 1 MiB on the 4 MiB block).
- Watch in `.stats`: `patch_writes` ≈ ops on the patch shape (`patch_ineligible_*` growing there = predicate rot), `patch_edge_rmw_reads` **must stay 0**, `fold_fill` median ≥ 16, `extent_records_{recovered,torn_discarded,future_refused}` on recovery.

### Large files — the block-map tree

A striped file's block map lives inline in its metadata record until the encoded map outgrows the record-value cap — **roughly 6–8 GiB of file at the default 4 MiB block size** (~60 KiB of map). Since 2026-09-01 (`docs/design-kvmap-block-map-tree.md`) a file that crosses that point moves its map into a dedicated metadata tree, **one record per block (with run records for contiguous stretches)**, riding the same journal, checkpoint and node cache as every other metadata record. Nothing is configured: the crossing happens on the publish that outgrows the inline cap, once per file, and announces itself in the mount log (`kvmap crossing: ino N entered the block-map tree …` — `grep kvmap <log>`). Below the crossing the on-disk bytes are pinned identical to before.

- **Ceiling.** Block indices are 32-bit, so a file may hold at most `2³² − 1` blocks: **`(2³² − 1) × block_size` = 16 PiB − one block at 4 MiB**, `4 PiB` at 1 MiB. The practical bound is metadata capacity — on the order of 8 GiB of map records plus 13 GiB of block-reference records per PiB of file at 4 MiB blocks, and run records collapse the map side by the run length for sequential data. (The previous scheme, a single indirect blob, capped a file at ≈ `block_size² / 30` ≈ 545 GiB at 4 MiB; a volume still carrying such a legacy blob converts it on the file's first publish under a current binary.)
- **RAM.** A tree-mapped file's map is read on demand through the node cache, not rehydrated whole. Files whose *write* map would exceed the derived share (`kvmap_write_map_budget_bytes` = memory budget ÷ 16) take the **partial store** — a dirty overlay plus bounded warm windows with tree read-through — gauged as `kvmap_partial_inos`; `kvmap_write_map_bytes` is the live write-map footprint, `block_map_window_{bytes,hits,fills,evictions}` the read-window tier (an R5 component with floor 0).
- **Truncate and unlink of a tree-mapped file are O(1) at the call**: the size (or the unlink) commits immediately together with a durable per-file sweep cursor, and the record deletes, block-reference releases and device reclaims run as a **background job** in chunks — resumable from the cursor across crashes and remounts, regenerated at mount for every cursor found. `squeezefs job list <mountpoint>` shows them as `kvmap_sweep` jobs; the ledger is `map_sweep_{jobs,chunks,records,resumed}`. Reads clamp to the new size, so shadowed records are unreadable the moment the truncate returns.
- **Crossing ledger.** `map_migrate_{inos,records,resumed}` count files that crossed, records written and crash-resumed crossings; `publish_map_record_bytes` is the per-publish journal cost on tree-mapped files, and `publish_indirect_blob_bytes` / `layout_indirect_map_reads` must trend to 0 on a converted volume. The per-volume tree counters are `meta_kv_block_map_{puts,deletes,run_puts,lookup_exact,lookup_range,lookup_floor,range_records}`.
- **fsck class C11 — map-plane consistency (report-only).** Orphan map records (a crossing that crashed mid-train, a dead inode), heads with no records, and run-vs-point coverage anomalies are detected and reported, never auto-repaired: `fsck_map_orphan_records` **must stay 0**, alongside `fsck_map_empty_heads` and `fsck_map_run_foreign_shadows`; `fsck_crossing_exempted` counts files whose crossing was in flight during the pass (an incomplete pass records no verdict). A crashed crossing's residue is invisible to reads and is deleted by the next crossing's sweep prologue.
- **Format face.** The first crossing on a volume stamps incompat bit 16 (`KV_BLOCK_MAP_TREE`) — never on untouched volumes; binaries older than 2026-09-01 refuse such a volume loudly ([Breaking changes](#breaking-changes--migration-notes)).
- **Knobs** (measurement levers, not operational settings): `SQUEEZEFS_KVMAP` (default on; `0` = new crossings take the legacy blob arm — never disables resolution of an existing tree head), `SQUEEZEFS_KVMAP_OVERLAY` (default on; `0` = whole map in RAM at any size), `SQUEEZEFS_MAP_MIGRATE_CHUNK` (crossing ops per transaction, default 512, range 64–1024).

### FUSE transport in-flight concurrency (defaults are the L1 policy; knobs are overrides)

Random-4k iodepth workloads are gated by two multiplicative kernel-side limits: the FUSE-over-io_uring per-queue ring depth and the INIT-negotiated `max_background`. Opening both measured **44k → 316k IOPS (7.2×, device-true)** on `elbencho --rand -t 16 -b 4k --iodepth 16 --direct` (`.benchmarks/2026-07-15-iops-parity-decomposition.md`); since L1 that class is the **default** — no knobs required:

- **Per-queue depth** (`SQUEEZEFS_FUSE_OVER_IO_URING_Q_DEPTH`, clamp 1..32): default is **32 degraded to the payload-buffer cap** — `mem-budget/8`, no fixed ceiling since the 2026-08-04 derivation sweep (`SQUEEZEFS_TRANSPORT_MEM_MAX` MiB absolute / `SQUEEZEFS_TRANSPORT_MEM_PCT` override it; the pinned-arena bound is the geometry's structural demand cap queues × 32 × payload) — with floor 4 (the pre-L1 posture — small-RAM boxes keep yesterday's footprint). An explicit value wins verbatim over the cap. Payload arenas cost `queues × depth × ~1 MiB` of registered anon memory — gauged as `transport_payload_buffer_bytes` in `.stats` and attributed to the memory budget as the `transport_payload_buffers` component.
- **Queues** (`SQUEEZEFS_FUSE_OVER_IO_URING_QUEUES`, testing only): pinned to kernel **possible CPUs** — registering fewer never becomes ready (kernel readiness requirement).
- **`-o max_background=N` / `-o congestion_threshold=N`**: INIT-reply overrides; defaults `clamp(queues × depth, 64, u16::MAX)` (the delivered ring capacity — the fixed 256 ceiling was retired by the 2026-08-04 derivation sweep; `-o max_background=256` is the A0 lever restoring the old posture) and ¾ of it. Also runtime-writable per live connection via fusectl: `echo 256 | sudo tee /sys/fs/fuse/connections/<minor>/max_background`.
- **FIND-L1-A (≥ 13-writer O_DIRECT convoy): FIXED 2026-07-17.** The convoy was a write-path completion-trigger defect (one write's end as a proxy for block completeness — kernel-split out-of-order WRITE segments misfired it), not a transport trade; the coverage-union trigger cured it (`t16` default/mb12 = 1.006–1.026, t16 ≥ 1.10× t8, both cells *rose*). No `max_background` throttle is needed or recommended anymore. Forensics + fix: `.benchmarks/2026-07-17-rw3-find-l1a-forensics.md`, `.benchmarks/2026-07-17-rw3b-write-through-coverage-fix.md`.

### FUSE max_write geometry (block-size writes; sysctl-gated)

The INIT `max_write`/`max_pages` pair is **negotiated per mount** (2026-08-04 geometry campaign; `.benchmarks/2026-08-04-fuse3-zc-adoption.md`): the daemon desires the **volume block size** (floored at the historical 1 MiB), and the fuse3 transport gates that desire by the kernel's `fs.fuse.max_pages_limit` sysctl and the payload budget, then advertises `max_pages = ceil(max_write/page)` — exactly what the plan stands on, so the kernel's fuse-over-uring REGISTER bound (`ring->max_payload_sz`) always equals the registered ents. Negotiated values are gauged in `.stats` as `transport_max_write` / `transport_max_pages`.

- **sqz-host posture: `fs.fuse.max_pages_limit=1024`** (`sysctl -w fs.fuse.max_pages_limit=1024`, persist in `/etc/sysctl.d/90-squeezefs.conf`). With the default 4 MiB block size a whole block then arrives as **one** FUSE_WRITE / one payload lease / one merge instead of 4 kernel-split 1 MiB segments — per-request fixed costs (dispatch, lock discipline, commit batching, wake economy) quarter on ≥ block-size sequential shapes. Reads scale the same way (readahead gathers to `max_pages`).
- **Fleet kernels without the sysctl (or at the 256 default) stay at their advertised cap gracefully**: negotiation lands on 1 MiB — byte-identical to the pre-campaign shape, and the mount always succeeds (pre-fix, a raised sysctl made every REGISTER refuse and the mount **fail**; the geometry law makes that structurally impossible).
- **Payload-arena cost scales with the negotiated ent size**: `queues × depth × max_write` registered anon bytes under the `mem-budget/8` cap (no fixed ceiling since the 2026-08-04 derivation sweep; `SQUEEZEFS_TRANSPORT_MEM_MAX`/`_PCT` override) — the depth leg degrades 32→4 first, and only past that does `max_write` itself degrade back toward the 1 MiB base (never below it). Field shape 32 queues × 4 MiB ⇒ depth 16 at a 2 GiB cap (e.g. `SQUEEZEFS_TRANSPORT_MEM_MAX=2048` or a 16 GiB budget).
- `SQUEEZEFS_FUSE_MAX_WRITE=<bytes>`: overrides the daemon's desire verbatim (still sysctl-gated) — the A/B lever for depth-16×4 MiB vs depth-32×1 MiB brackets; not an operational escape.

### FUSE zero-copy serve integration (sqz kernel; opt-in)

On the sqz kernel series (`docker/kernel-sqz/` — the kmbuf bufring + `FUSE_URING_ZERO_COPY` transplant), the FUSE-over-io_uring transport can register each request's pages in a per-queue sparse buffer table and serve eligible traffic **without daemon copies**: cold aligned reads DMA device→caller-pages (`READ_FIXED`), eligible WRITE payloads DMA caller-pages→device (`WRITE_FIXED` — the D14 write-side leg, W1 patch class), everything else bridges through a per-queue memfd bounce. Design `crates/fuse3/src/raw/connection/zc.rs`; adjudications `.benchmarks/2026-08-06-fuse-zc-write-side.md` + `.benchmarks/2026-08-07-zc-bridge-cqe-wedge.md`.

- `SQUEEZEFS_FUSE_ZC` (**default on** — ruling D16, 2026-08-07, superseding the earlier 0.97× all-write-rows flip rule; rc-manifest §3f): arms where the surface admits it — requires the kmbuf surface, the kmbuf lever, and CAP_SYS_ADMIN (the kernel's own zc REGISTER gate); every decline is loud, and a kernel that refuses the zc REGISTER (any stock kernel) degrades to the bufring path byte-identically. Armed wins: reads +40–75 % (device DMA → app pages, the K1 kill), durable/seq writes ≥ par. **Known caveat**: UN-shimmed kernel-lane rand-4k writes paid ~20 % (the extraction round trip). **Handler/worker fusion LANDED 2026-08-07** (`.benchmarks/2026-08-07-zc-write-fusion.md` — small armed WRITEs run their handler on the queue worker's own fused lane, zero cross-thread wakes per op; local tcp-devsub A-B-B-A: fusion +12 %/+22 % on the rand rows at −27..29 % daemon CPU/byte, armed-vs-unarmed recovered to 0.973×/1.000× at median): the caveat SUNSET awaits the field bracket (rc-manifest §3f rule: armed rand-4k ≥ 0.97× unarmed on squeeze-test — the sunset row spec is in the fusion note §5). Until then small-op-heavy workloads may still prefer the interception shim (whose ring lane never paid it). `SQUEEZEFS_FUSE_ZC=0` is the escape / A-B lever. Engagement: `fuse3_zc_negotiated` (0/1), `fuse3_zc_replies`, the write-vehicle pair `fuse3_zc_write_directs`/`fuse3_zc_write_extractions` (+ byte faces), the fusion trio `fuse3_zc_write_fusions`/`_bytes`/`_demotions`, `read_zc_serve_bytes`.
- `SQUEEZEFS_FUSE_ZC_WRITE_FUSION` (**default on** — RESOLVED 2026-08-08, fused-lane-predicate fix: the field falsification (armed rand-4k 0.45× at ~300 µs fabric RTT) was the shape-only hold PREDICATE, not the lane — W1-ineligible writes were held/fused and then paid hold + fused poll + LATE extraction serialized at fabric RTT (the both-vehicles signature `fuse3_zc_write_fusions` ≈ ops ∧ `fuse3_zc_write_extractions` ≈ ops). The hold now gates on the filesystem's W1-eligibility probe (`Filesystem::zc_write_hold_eligible` — the `try_sole_owner_patch` ladder's lock-free read-only mirror) and ineligible shapes extract AT DELIVERY on the classic dispatch; acceptance A-B-B-A on the fabric-emulated venue (fused ≥ fusion-off on the field shape and the eligible shape; armed ≥ 0.97× unarmed on both) and un-emulated (the W1-shape +11.6 % win, both bracket orders) — `.benchmarks/2026-08-08-fused-lane-predicate.md`): handler/worker fusion for small armed WRITEs — W1-ELIGIBLE hold-candidate deliveries at or under the fusion ceiling run their handler future on the queue worker's own fused lane, deleting the bridge round trip's cross-thread wakes. `0` = the A/B control. Engagement: `fuse3_zc_write_fusions`/`_bytes`; `fuse3_zc_write_fusion_demotions` ≈ 0 steady-state; **`fuse3_zc_write_lazy_extractions` ≈ 0 is the hold gate's staleness law** (growth = the delivery probe drifting from the W1 ladder).
- `SQUEEZEFS_FUSE_ZC_READ_FUSION` (**default off** — e2e perf audit R-3, read board #3's zc-leg form; `.benchmarks/2026-09-03-r3-fill-issue-economy.md` §9: the composition verdict): handler/worker fusion for armed FUSE READs at or under the fusion ceiling — the READ handler runs on the reaping queue worker's fused lane, so the zc direct leg's fetch message, its CQE resume and its prefilled COMMIT are same-thread channel ops instead of three cross-thread wakes (the 4k-random attribution measured them at 35–47 µs each around a 40 µs DMA: `zc_bridge_phase_ns` `msg_hop` / `device_cq` − device / `wake_hop`). Composed with R-2's READ fast-dispatch as ONE law at the delivery CQE: (a) a warm READ is served inline by the reap thread's try-only probe; (b) else a zc-eligible READ at or under the ceiling is fused onto the worker's lane; (c) else the minted handler is handed to the `fuse3-tpc` lane homed on the queue's CPU — `transport_fast_dispatch_serves + fuse3_zc_read_fusions + transport_fast_dispatch_demotes ≡ READs`, every READ exactly one arm. Reads above the ceiling (`SQUEEZEFS_FUSE_ZC_FUSION_MAX`, the same bound the write lane uses — a serve the probe declined may still copy the request's size on the worker) take arm (c). Standing alone against the pre-R-2 dispatch it won (field kern rand-4k 24×8 +15.5 %, local +17 %, `msg_hop` 32 → 0.6 µs, `transport_wake_writes` 1.57 → 0.62/op); COMPOSED behind R-2's fast dispatch it measured −6.5 % IOPS vs R-2 alone (481 k vs 514 k; p50 +70 µs, p99 −66 %) — the two levers converge on the reap thread's per-op serialization (the fused pass's run-queue waits replace the lane hop and the bridge's two worker hops at a net loss), so the default is OFF and a demoted READ ships on arm (c). `1` = fuse (the tail-shaped A/B posture). Engagement: `fuse3_zc_read_fusions` ≈ every cold kernel READ on an armed session (the 1 MiB stream rows read 0 by the ceiling); `fuse3_zc_read_fusion_demotions` ≈ 0 steady-state.
- `SQUEEZEFS_READ_ZC_SERVE` (**default on** since the 2026-09-05 field bracket (daemon CPU/GiB −24 % cold / −25 % warm on unaligned reads, A-B-B-A; `0` = the A/B control) — e2e perf audit R-4, read board #4/#5; `.benchmarks/2026-09-05-r4-read-zc-serve.md`): the fd-SOURCE zc READ serve. On a zero-copy-armed session every out-paged reply body reaches the request's pages by `READ_FIXED(fd → slot)` — an fd-SOURCED primitive (the device fd on the direct leg, the per-ent bounce memfd otherwise), and no io_uring op copies an anonymous VA into a fixed buffer — so a warm (hot / read-lane hold) or cold-slice serve paid the daemon's tier → bounce copy AND the kernel's bounce → folios pass (2.0 passes/byte warm, 2.69 cold whole-block). `1` builds the whole-block READ fill pool as ONE `MAP_SHARED` memfd slab (the `ZcBounce` two-way-reachability law applied to the fill pool): fills, hot-tier entries and read-lane holds become fd-addressable, the router's hot/hold/fill-slice arms and the R-2 fast probe hand the transport the tier buffer's own `(fd, offset)` instead of copying, and the queue worker bridges THAT into the folios — 1.0 warm, 1.69 cold-slice, no kernel change (the bridge op is the one the bounce already rides), the write path's pool untouched (a separate pool — the A/B moves the READ serve only). Non-pool sources (NVMe read-cache mmap, transform decode output, over-capacity fresh backings) keep the copy path, counted. Default off until the field `r_cold`/`r_warm` A-B-B-A adjudicates. Engagement: `read_zc_pool_serve_bytes` (a closure term beside `read_copy_dest_bytes`/`read_copy_bounce_bytes`/`read_dest_dma_bytes`/`read_zc_serve_bytes`) + `read_zc_pool_serve_warm_bytes` (the hot/hold subset), `fuse3_zc_fd_body_replies` (the transport vehicle count); `fuse3_zc_fd_body_fallbacks` (an fd-source bridge that failed and re-staged through the bounce — the reply is never lost) must stay ≈ 0.
- `SQUEEZEFS_FUSE_ZC_FUSION_MAX` (default derived payload/8 — 128 KiB at the shipped 1 MiB transport geometry; range 4096–1073741824): the fused-dispatch payload ceiling — bounds the handler work the queue worker's drain loop runs inline (never move payload-scale memcpys onto the worker). The default brackets the measured hop-vs-inline-copy crossover; explicit values win verbatim.
- `SQUEEZEFS_FUSE_KMBUF` (default on; `0` = the A/B control): the kernel-managed reply-buffer negotiation zc rides on (zc is bufring-plus).
- `SQUEEZEFS_FUSE_IO_URING_SPIN_US` (default **off** = `0`; range 0–100000; e2e perf audit R-4, `.benchmarks/2026-09-03-r4-reap-thread-economy.md`): the spin-before-park CAP, µs, for the FUSE-over-io_uring queue workers (`f3-ur*`). With a nonzero cap, where a worker would block in `submit_and_wait` it first spins a bounded window watching `IORING_SQ_TASKRUN` (the ring's "deferred task work landed" flag — rings are built with `IORING_SETUP_TASKRUN_FLAG`), the CQ tail, the wake coalescer's armed flag and the shutdown word, so a device CQE or a lane message landing inside the window is reaped without a park and the scheduler's wake latency. The window is ADAPTIVE: 2 × the worker's own park-gap EWMA (a reaction clock — every event it waits for echoes something it just did) capped by this value and the 200 µs rail, engaged only while that worker's queues hold ops in flight (a delivered-not-committed slot or a bridge pend — an idle queue never spins, so an idle mount burns nothing), and refused past the box's queueing knee (whole-box busy > 80 %, the Erlang-C wait onset for c ≥ 16 servers; `/proc/stat` sampled once per 100 ms across the pool). **Measured NOT fat at the field and shipped off**: on the kern rand-4k 24×8 row (cap 200, the rail) the governor absorbed 0.50 parks/op — 69 % of the worker's parks, 0.76 → 0.24/op — and `zc_bridge_phase_ns` `msg_hop` / `device_cq` / `wake_hop` did not move by a µs (27.1 → 27.6 / 97.5 → 96.2 / 36.6 → 38.8), at +5.5 % CPU/op (+10 % on the workers) and a −2.7 % / +0.6 % IOPS bracket with p99.9 +5–21 %: the parks the window covered were the ≤ 16 µs class the kernel wait already returns from without a sleep, and the long class that carries the wake latency sits past any window a 75–80 %-busy box can afford (the ipc theft verdict). Any nonzero value engages the governor with that cap (the registered A/B lever). Engagement (stats inode): `transport_spin_absorbed` (parks the spin deleted — an event caught inside the window), `transport_spin_expired` (windows that ran out and parked), `transport_spin_ns` (the CPU paid — the cost column of any row crediting the lever), `transport_spin_refused_busy` (regime hits the box gauge refused), `transport_spin_window_us` (the last derived window). Closure: `absorbed + expired` ≡ the spins run.
- `SQUEEZEFS_ZC_BRIDGE_TIMEOUT_MS` (default 30000, range 100–600000): the **bounded-outcome law** (zc-bridge-cqe-wedge campaign, 2026-08-07) — every zc bridge op (device fetch/store, bounce bridge, WRITE extraction) in flight past this deadline gets one `AsyncCancel`, and the cancel's own CQE classifies the recovery: found+canceled resolves through the loud fallback ladders; `-ENOENT` with a live pend is a PROVEN lost ring completion and synthesizes its resolution (EIO/fallback — never a silent hang, never an acked byte lost); still-running re-arms the deadline loudly. Tripwires (**must stay 0**): `fuse3_zc_bridge_cancels` (the deadline fired) and `fuse3_zc_bridge_lost` (a completion was lost — kernel-side evidence; report with the mount log). The zcws-9 W4 field wedge (140 requests, 28/32 rings, 96 min, connection-abort recovery) is the class this bounds.

### FUSE io_uring SQPOLL (mount env; measured — leave unset)

- `SQUEEZEFS_FUSE_IO_URING_SQPOLL_IDLE_MS` (default unset = **off**): opts every FUSE-side io_uring into kernel submission-queue polling with the given idle timeout — the classical `/dev/fuse` INIT/notify/sideband rings (one poller each) **and** the FUSE-over-io_uring queue rings, which share **one** poller for all queues (qid 0 creates it, the rest attach via `IORING_SETUP_ATTACH_WQ`; a kernel that declines SQPOLL degrades loudly to plain rings, never failing the mount).
- `SQUEEZEFS_FUSE_IO_URING_SQPOLL_CPU`: pin that one queue-ring poller (`IORING_SETUP_SQ_AFF`; the leader's pin governs the shared group — per-queue pins do not exist by design).
- **Measured posture (2026-07-15, `.benchmarks/2026-07-15-m10-sqpoll.md`, resolves design OQ 3): not recommended — including for dedicated metadata-heavy nodes.** On the post-M3 transport (one `io_uring_enter` already carries commit+wait), SQPOLL-on measured **+25 % enters/create** (wake-cycle fragmentation; 9.10 → 11.40), **one full core burned by the poller under storm** (idle mounts burn 0.0 % — the idle timeout parks it), and **flat-to-worse paired mdstorm rows** (−1.5 % create … −12.9 % many-dirs unlink) at byte-identical op shape. Consider only on boxes with uncontended spare cores, and only if a live profile of *your* workload (strace `io_uring_enter` counts + `iou-sqp` thread CPU, the M10 method) proves it out.

### Kernel cache TTLs (mount options / env; per-class)

Four kernel-cache TTL classes, each defaulting to the historical 1 s (the DAOS per-class split: directory dentries invalidate whole subtrees, so they get their own knob). Mount options are libfuse-style float seconds (`-o attr_timeout=2.5`) and win over the env knobs (milliseconds); both are per-mount. Longer TTLs widen the staleness window a single mount can observe of its own metadata — safe under the single-writer mount guard; revisit before any multi-writer future. On a **read-only mount** the four defaults are not 1 s: they derive from the writer's checkpoint cadence, because that is the interval a reader can prove freshness over ([Read-only coherent mounts](#read-only-coherent-mounts--o-ro--one-writer-plus-n-readers)). Explicit values still win verbatim there, which means setting one lengthens a reader's staleness window by exactly that much.

- `-o attr_timeout=<s>` / `SQUEEZEFS_FUSE_ATTR_TTL_MS`: GETATTR/SETATTR reply TTL + the daemon attr-cache freshness window.
- `-o entry_timeout=<s>` / `SQUEEZEFS_FUSE_ENTRY_TTL_MS`: dentry TTL for non-directory lookup/create results.
- `-o dir_entry_timeout=<s>` / `SQUEEZEFS_FUSE_DIR_ENTRY_TTL_MS`: dentry TTL for directory results.
- `-o negative_timeout=<s>` / `SQUEEZEFS_FUSE_NEGATIVE_TTL_MS`: TTL for cacheable negative lookup replies (kernel-side negative dentries — repeated misses of the same name stop paying a round trip). `0` disables negative caching (misses reply bare ENOENT).

### External mount supervisor (`mount --daemon --supervise`)

With `--supervise` the `mount --daemon` parent stays alive as an external watchdog (JuiceFS-supervisor precedent): it probes `<mountpoint>/.stats` every 5 s (`SQUEEZEFS_SUPERVISE_INTERVAL_SECS`) and, after 30 s of sustained unresponsiveness (`SQUEEZEFS_SUPERVISE_UNRESPONSIVE_SECS`), logs loudly, dumps the daemon's `/proc` state (per-task wchan + kernel stacks when root), and — when kernel callers are blocked (`waiting > 0`) and the supervisor runs as root — writes `/sys/fs/fuse/connections/<id>/abort` to release them with `ECONNABORTED`. The abort kills the mount by design; the escalation message prints the daemon PID and the exact manual recovery commands (kill-by-PID → `squeezefs umount` → remount). This complements the in-daemon op watchdog, which can log a wedge but cannot clear one.

### Host auto-tuning (`squeezefs tune`)

Built-in host auto-tuning (`squeezefs tune`, requires root) optimizes virtual memory dirty page ratios (40/10), network socket buffer maxima (64 MiB), and live FUSE connection limits (`max_background`/`congestion_threshold` raise-only to the 256/192 measured class — never lowering a ring-capacity-negotiated mount, `read_ahead_kb` to 0). See [Kernel Tuning](../QUICKSTART.md#6-kernel-tuning-for-bare-metal-auto-tune).

```bash
squeezefs tune
```

### Other verbs

* **Unmount** — safely unmounts SqueezeFS by waiting for staging caches to flush before tearing down FUSE:
  ```bash
  squeezefs umount <mountpoint> [--force]
  ```
* **Instant metadata clone (CoW)** — OFFLINE and D0-guarded (like every offline mutating verb: refused while the set is mounted). The `sqmeta://` URI is **required** — both paths are resolved through the metadata set, and the single-writer guard is taken over it for the clone's duration. Metadata-only: the clone's map names the source's blocks and their refcounts rise; not one data byte is copied. A **staged** source is refused loudly (its acked payload lives in the mount's isolated staging, which the offline coordinator never opens) — clone those through a live mount (`cp --reflink=always`, which rides `copy_file_range`):
  ```bash
  squeezefs clone -g sqmeta://<meta_dev>[,<meta_dev>…] <src> <dest>
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
squeezefs volume enable-multi-writer <sqmeta-uri>       # OFFLINE: stamp a pre-flip / --single-writer set multi-writer-capable
squeezefs volume set-owners <sqmeta-uri> <vol-id>=<member-id>[+<successor>][:/subtree/root] ...
                                                        # OFFLINE, whole fleet unmounted: assign per-volume metadata owners
squeezefs volume get-owners <sqmeta-uri|mountpoint>     # owners beside live claims (the drift view)
squeezefs volume locate <sqmeta-uri|mountpoint> <path>  # which volume hosts this path's inode, and who owns it
```

- Metadata routing granularity: **format anywhere, grow forever, no knobs** (dynamic meta routing, 2026-08-02). Every format freezes the DERIVED virtual width (65536 slots — never chosen) and spreads minting across 64 slots per metadata volume, so any volume's existing metadata is divisible into ≥ 64 movable slices from birth: a single-metadata-volume filesystem grows to two (or two hundred) by `volume add-meta --take-slots …` / `migrate-meta-slot` with no format-time planning. The retired `format --meta-slots` flag is a hard error naming these verbs; volumes formatted under the old frozen-width scheme refuse loud (reformat required — forward-only).
- Membership changes are crash-safe: interrupted `add-meta`/`remove-meta` **re-run with the same arguments and converge**; `repair-set` reconciles the stamps when a crash left them mid-flip. Old binaries refuse lifecycle-marked sets loudly (forward-only).
- Set changes drain local staging first, then rebind the staging generation — durable staged payloads survive the membership change.
- `set-owners` / `get-owners` / `locate` are the per-volume ownership surface — several nodes as metadata authorities for one set, one appender per volume. The assignment is offline, whole-set, bracketed and idempotent, and it changes the namespace's semantics across owners: read [Per-volume metadata owners](#per-volume-metadata-owners-squeezefs-volume-set-owners) before running it. A set with no owners assigned is unaffected in every respect.
- `enable-multi-writer` stamps the nine multi-writer incompat bits on every volume of the set in one crash-resumable invocation (dependency order, bit 11 terminal, bracketed by a durable intent marker — a writable mount refuses while the upgrade is incomplete; re-run the verb to resume). Fresh formats carry the bits by default since the Phase-B flip, so this verb exists for pre-flip sets and `--single-writer` formats. Forward-only: no downgrade verb; pre-multi-writer binaries refuse the upgraded set. `add-meta` keeps the set uniform in both directions: a fresh member joining a stamped set formats stamped, one joining an unstamped set formats unstamped, and a foreign bit-11 volume refuses to join a non-upgraded set.

### fsck / scrub

```bash
squeezefs fsck <target> [--json] [--throttle N]        # detect: 11 classes, verified findings only, exit != 0 on findings
squeezefs fsck <target> --scrub                        # add the C7 data scrub (AEAD/frame/readability per stored form)
squeezefs fsck <sqmeta-uri> --shards k/N ...           # offline zero-coordination sharding; union with `fsck merge-reports`
squeezefs fsck <target> --repair                       # plan per-class repairs (DRY RUN)
squeezefs fsck <target> --repair --apply               # execute: quarantine-first, idempotent, verified findings only
```

Online fsck runs against the live daemon with **zero false positives by design** (every suspect is verified before it is reported — concurrent writes, drains, and parked work are exempted through the live registries, never guessed at). Repair is dry-run by default, quarantines before every discard (per-run quarantine dir + JSON manifest), and is honest where no redundancy exists: torn nodes and scrub-failed blocks are quarantined and reported, never fabricated. On plain (uncompressed, unencrypted) data the scrub can only verify readability — the report says so (`scrub_readability_only`).

The classes: C1–C7 (the volume-lifecycle seven: block/refcount/allocator/layout consistency plus the C7 data scrub), C8 (durable-vs-derived block-reference drift, report-only), C9 (unreferenced inodes) and C10 (inode-plane reference consistency) below, and **C11 — map-plane consistency for tree-mapped large files (report-only)**, described under [Large files](#large-files--the-block-map-tree).

#### C9 — unreferenced inodes (also the cleanup path for pre-S3.5 damage)

**Detects** an inode record that exists in the metadata tree and that
**no directory entry names**. Nothing else finds one: the kernel never
learned the inode exists, so it never sends a FORGET and the ordinary
orphan reclaim (which only admits already-unlinked inodes) will never see
it; the block classes ask "is this BLOCK referenced by nobody", and an
unreferenced inode's layout still names its blocks, so they stay
correctly silent. Three shapes reach it:

- a cross-volume `create` interrupted between its two commits — one
  inode, no name, no data (a bounded metadata leak for an operation the
  caller was never told succeeded);
- **damage that predates the cross-volume transaction machinery** (DLM
  S3.5): a filesystem that ran the older code and crashed during a
  cross-volume `link`/`unlink` carries the same shape **plus every block
  that inode owned**, invisible and unreclaimable. S3.5 prevents new
  occurrences and does nothing about existing ones — **this class is the
  only way to learn whether a volume carries such damage, and its repair
  is the only cleanup path.** Run it once after upgrading a filesystem
  that has been through a crash;
- a dead writer's in-flight create, or a recovered cross-volume plan
  whose inode landed while its directory-entry volume was fenced.

**Repairs** by destroying the inode and reclaiming its blocks: the inode
record and **every** one of its xattr records (including `layout`, the
only map to its blocks) are copied into the per-run quarantine with the
block keys enumerated in the manifest; the blocks are then freed through
the ordinary terminal-free path (durable references released, discards
queued on the background reclaimer, tiers purged); finally the record and
its xattrs are destroyed in one transaction. Block **contents** are not
copied — an inode's data is unbounded, so the manifest states what was
reclaimed rather than pretending to keep it. Dry run (`--repair` without
`--apply`) reports the inode, its size and its block count first, which
is the moment to decide. An interrupted repair converges: the next run
re-detects the same inode and both remaining steps are idempotent.

Two properties worth knowing:

- **Residue created by the CURRENT mount is reported by the NEXT one.**
  A live `create` legitimately holds an inode record before its directory
  entry, so the only false-positive-free candidate filter is "minted
  under an earlier writer term" — which is exactly what crash residue is.
  This costs no coverage (the damage is by definition from a prior mount)
  and it is why `fsck_findings` stays 0 on a busy healthy filesystem.
- **The `nlink == 0` unreferenced shape is deliberately NOT claimed.**
  That is the POSIX unlinked-but-open state (and the rename-overwrite
  crash orphan of
  [POSIX-15](#rename-overwrite-leaves-a-crash-window-orphan-posix-15)):
  telling it apart from a corpse nobody will ever
  FORGET needs the live open-handle registries, so claiming it could
  destroy an open file's data. It remains an accounting leak, not lost
  data.

Gauges: `fsck_dentry_refs_indexed` (the single dentry pass that answers
"which inodes are named" — C9 never runs the per-inode reverse scan),
`fsck_current_era_exempted` (unnamed inodes this mount minted, i.e. the
in-flight creates the writer-term filter protected), `fsck_repair_classC9`.

#### C10 — inode-plane reference consistency (which direction means stop-and-read)

C9 asks whether an inode is named at all. C10 asks whether its **link
count matches the names**, and whether every name **resolves**. Both
questions come from the same single dentry pass, and the two directions are
not equally serious:

| Finding | What it means | Urgency |
|---|---|---|
| `nlink` **exceeds** the names | The inode and every block it owns can never be reclaimed. Leaked capacity that reads as healthy. | Repair at convenience. |
| `nlink` is **below** the names | The count no longer covers a live path. Once ordinary `rm`s drive it to 0, **the filesystem is entitled to destroy an inode a path still resolves.** | **Stop and read.** |
| `nlink 0` with a live name | The same thing, already at 0. Nothing legitimate has this shape. | **Stop and read.** |
| A **dangling** name | The name resolves to nothing: `ls` shows it, every `stat` of it fails. | **Stop and read** (then remove it). |

The first row is a leak; the other three are the loss direction — an
`rm` of an unrelated name can turn one of them into missing data, so
repair those before resuming write traffic on the affected paths. The
shapes come from the same place C9's do: a cross-volume `link` or `unlink`
that committed one of its two steps and not the other (pre-S3.5 damage, or
a plan whose participant volume was fenced). The `nlink`-below and dangling
shapes were **removable but undetectable** before this class existed.

**Repairs**, per direction, and deliberately asymmetric:

- **Raising** a count to the names that exist is safe (an over-count only
  delays reclaim) and is what both `nlink`-below and `nlink 0` get. Names
  are never dropped to match a low count — that would be the data loss the
  finding warns about.
- **Lowering** a count is the one C10 action that could make a named inode
  reclaimable if the count of names were wrong, so it runs only when a
  third independent dentry pass agrees with the two the detection used, the
  record has not changed, and no cross-volume plan is open.
- A **dangling name can only be removed** (there is nothing to re-point it
  at). The dentry record's key AND value are quarantined first, so the name
  can be reconstructed exactly.
- **Refusals are loud and expected**: a directory (its `nlink` counts `.`
  and every child's `..`, which are synthesized — this class does not
  compute that number, so it reports and never guesses), a name whose child
  inode came back, a count that healed since the scan, a direction that
  reversed, and any inode an open cross-volume plan names.

Three properties worth knowing:

- **Directories are not counted.** A directory's `nlink` is
  `2 + subdirectories`; comparing it to "names in the metadata tree" would
  fire on every directory in the filesystem. A directory that is genuinely
  named twice is therefore also not claimed — stated rather than hidden.
- **`nlink == 0` WITHOUT a name is still not claimed** — that is POSIX
  unlinked-but-open and the rename-overwrite orphan of
  [POSIX-15](#rename-overwrite-leaves-a-crash-window-orphan-posix-15), the
  same line [C9](#c9--unreferenced-inodes-also-the-cleanup-path-for-pre-s35-damage)
  draws. `nlink == 0` **with** a name is unambiguous, which is why C10
  claims that half.
- **A continuously rewritten file may be reported by the NEXT run.** The
  false-positive guard is that the inode's record must not change while the
  verifying dentry pass runs (every operation that moves a name count also
  mutates that inode's record), so a file being written throughout the scan
  keeps clearing. Like C9's writer-era filter, that costs coverage, never
  safety.

Gauges: `fsck_nlink_mismatch_high` (the leak direction),
`fsck_nlink_mismatch_low` / `fsck_nlink_zero_named` /
`fsck_dangling_dentries` (**the loss direction — nonzero here is the
stop-and-read signal**), `fsck_nlink_names_counted` (the multi-named
population the counting rides — 0 on a tree with no hardlinks),
`fsck_nlink_transient_cleared` (suspects the guards cleared: concurrent
link/unlink/rename traffic and open cross-volume plans — expected to grow
on a busy mount, and its growth is what shows the guard is live),
`fsck_repair_classC10`.

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

### Cluster wire (the one cluster transport)

Cluster-internal RPC — the job-shard execution wire today, and the DLM's
cross-mount traffic as those stages land — rides a single framed transport
(`src/cluster_wire.rs`). It is deliberately **not** io_uring: it is a TLS/TCP
network path, the documented exception in the io_uring policy. Every frame is
length-prefixed, size-classed, and authenticated per frame
(`HMAC-SHA256(session_key, direction ‖ sequence ‖ length ‖ body)`), so tamper,
reorder, replay, and reflection are refused at the frame boundary rather than
by the handler; enrollment proves storage membership against the volume set's
`job:enroll` secret before a session key exists.

Owner-side requests execute on **pinned service lanes** (`sqz-cluster-svc{n}`),
never on the metadata commit conveyor's task — a remote peer must not be able
to occupy the local commit path. One knob:

| Knob | Default | Purpose |
|---|---|---|
| `SQUEEZEFS_CLUSTER_WIRE_SVC_THREADS` | derived `clamp(cpus/8, 1, 8)` | Owner-side RPC lane count. Absolute override (measurement lever); still clamped to the core count, so it cannot oversubscribe the box. |
| `SQUEEZEFS_MULTI_WRITER` | off | **DLM S7**: demand a device-enforced multi-writer data plane — a WERO (rtype 3) reservation on every data namespace, held for the mount lifetime. **Refuses the mount loudly** on a substrate without NVMe reservation support (every loop device, including `tests/dev_substrate.sh`'s default) and on a format without incompat bit 11 — the default format stamps it since the rung-10b Phase-B flip; `--single-writer` formats and pre-flip sets refuse until `squeezefs volume enable-multi-writer`. Off = the shipped single-writer posture (see [the data-plane guarantee rows](#the-data-plane-dlm-stage-s7)). |

What decides cluster performance is the **fabric**, not this wire: on loopback
at qd1 the authenticated round trip measures single-digit microseconds, while
the same exchange across a real network costs tens to hundreds — and the
metadata-op cost of a synchronous cross-mount hop scales with that number, not
with framing. Plan cluster topology around round-trip latency first.

### Metadata function shipping (DLM S8)

**Status: the mechanism ships, and since DLM S9 it has an arm —
`SQUEEZEFS_MULTI_WRITER=1` (see
[Multi-writer data plane](#multi-writer-data-plane-dlm-stage-s9)), which arms
ownership, the data-plane fence and remote write custody together or refuses
naming the missing piece. On a field volume that arm still refuses (nothing
stamps the capability bits — ruling D9), so there is no operator action here
yet.** This section documents the `.stats` fields, the knobs and the refusal
texts where an operator meets them.

What it is: when a metadata volume is owned by **another node**, a metadata
operation on that volume's inodes **travels to the owner and executes there**
(`src/meta_ship/`, spec §6.7 decision 1) instead of a lock travelling to the
caller. The KV engine is RAM-authoritative and single-writer by construction,
so there is nothing for a remote node to serialize into.

* **Granularity is the volume.** A metadata volume has exactly one owner, and
  the owner is the node holding its D0 `writer_claim` — the record that already
  names its holder and its durable era. Nothing new is written on disk for
  ownership, and **no incompat bit is involved.**
* **Locally-owned volumes are untouched.** A mount that owns every volume of
  its set (every mount that ships today) pays one relaxed load per operation
  and then takes exactly today's path: no session, no frame, no round trip.
  `meta_ship.shipped_verbs == 0` is its signature and must stay 0.
* **Cross-owner `rename`/`link` refuse loudly with `EXDEV`**, naming the
  machinery they need (the S3.5 cross-volume transaction: intent record +
  compensation + crash recovery). A cross-*volume* operation whose volumes
  share ONE owner is not affected — it is today's path, executed on the owner.
* **Serial workloads are the known cost** (`tar -x`, `make`, `rsync`): a serial
  stream pays one fabric round trip per operation, which spec §6.10 R1 prices
  at 9,100/s → 6.7–20 k/s at 50–150 µs RTT. That regression is accepted
  (ruling D10) and is **published, measured** (real linux-src `fs/` tar -x,
  netem 250 µs wire RTT, A-B-B-A): S8 raw runs **0.14× of authority-local**;
  with the S10 recovery levers ON — subtree delegations
  (`SQUEEZEFS_DELEGATION`) + UPDATE intents (`SQUEEZEFS_UPDATE_INTENTS`) +
  client-owned-slot placement (`SQUEEZEFS_SLOT_PLACEMENT`), all default-on —
  it recovers to **0.149× (6.73× the local wall; 160–162 entries/s vs
  1,057–1,105 local)** with the create/utime plane fully local
  (`.benchmarks/2026-08-17-s10-slot-placement.md`). The spec-§6.9 S10 gate
  ("tar -x back to ≤ 1.10× of S0") is **NOT met on the shipped topology, and
  cannot be**: exactly one node holds every metadata volume's D0 claim, a
  co-writer is metadata-read-only, so no client-owned slot exists to place
  its work into and the residual ~11.7 shipped verbs/entry are per-entry
  reads plus per-file custody/publish ceremony. Spec R1's own fallback is
  therefore the product statement: **remote clients are throughput-oriented;
  latency-sensitive serial metadata work runs on the owner.** Concurrent
  streams amortize: verbs to one owner coalesce into one frame, and
  `meta_ship.batched_verbs / meta_ship.batches` is the live coalesce factor.
* **Client-owned-slot placement** (DLM S10 rung 14, `SQUEEZEFS_SLOT_PLACEMENT`,
  default on, read only when the ownership plane is armed): an armed authority
  mints each shipping client's fresh inos into a slot **dedicated to that
  client** — outside the volume's mint rotor, stable per client — so a
  client's minted population is migratable as ONE unit through the existing
  online `migrate-meta-slot` engine. The auto-policy migrates a SUSTAINED
  client's hot slots toward a metadata volume that client **owns**, after
  which its verbs on those inos run locally (it IS the S8 authority for
  them). On every fleet the product can mount today no shipping client owns
  a volume, so the migration half is **structurally dark**
  (`meta_ship_placement_migration_candidates` stays 0 — proven live by the
  gate row) while the mint-targeting half engages
  (`meta_ship_placement_client_slot_mints`). The policy is valve-bounded (the
  rung-11 arithmetic, no knob): alternating clients can never ping-pong a
  shared directory's slot — it demotes to stay-put for the derived cooldown
  (`meta_ship_placement_thrash_demotions`).
* **Failover** (when an owner dies and a successor takes its volumes): the
  successor bumps its durable era before arming, which makes every request and
  every token from the old era stale by construction, then opens a **grace
  window** that admits only *reclaim* requests and refuses fresh mutations by
  name. Reads keep serving throughout, and a client relearns the new era from
  any reply.

| Knob | Default | Purpose |
|---|---|---|
| `SQUEEZEFS_META_SHIP_BATCH_MAX` | derived `clamp(cpus × 2, 64, 4096)` | Verbs per shipped frame — the pipelining unit. Derived like the M7 commit-conveyor batch cap, because a frame's ops become that many transactions on the owner's conveyor. |
| `SQUEEZEFS_META_SHIP_DEDUP_MAX` | derived `max(batch_max × 128, 8192)` | Owner-side idempotency window entries: how far back a client's retry may reach and still be answered from its ORIGINAL outcome instead of re-applying. |
| `SQUEEZEFS_PUBLISH_SHIP_DEPTH` | derived `clamp(ceil(cpus / 8), 2, 8)` | S9 publish plane (D-1b): publish FRAMES a co-writer keeps in flight per authority — one authenticated session each. A frame carries every publish queued when a slot frees (the S8 lane shape: no timer, no added delay on a serial stream) and the owner runs its independent inos concurrently, so they co-queue into one conveyor pass. `1` = the stop-and-wait A/B control (frames still batch; nothing pipelines). Live: `meta_ship_publish.ship_frames` / `ship_framed_calls` (calls ÷ frames = the coalesce factor), `ship_depth_waits`, `ship_session_dials`, `served_frames` / `served_frame_calls` / `served_chains`. |
| `SQUEEZEFS_PUBLISH_CONVEYOR_GROUP` | `on` | S9 publish plane, OWNER side (D-1c — one conveyor group per shipped frame): the authority prepares a served frame's independent `SetLayoutAndSize` calls concurrently and commits them as ONE conveyor group per dispatch round — every staged transaction enqueued under one queue lock, so a frame is one apply pass by construction (before it, the co-queue was arrival spread ÷ pass latency: 3–14 passes per 24 publishes). One tx = one checksummed journal entry is unchanged. `0` = the D-1b per-chain dispatch (the same-binary A/B control). Live: `meta_conveyor_group_commits` / `meta_conveyor_group_txs` (txs ÷ commits = the group size), `meta_ship_publish.frame_groups` (served frames that committed a group) — all 0 under `0` and on a solo mount. |
| `SQUEEZEFS_DLM_TOKEN_CACHE_MAX` | derived `max(R5 budget/8192/32 B, 4096)` | Client fencing-token cache entries. Every eviction costs one loud miss line and one shipped `getattr` refresh — never a wrong answer. |

**Live signals** (`.stats`, under `meta_ship`): `armed` (false on every shipped
mount), `local_verbs` vs `shipped_verbs` (the routing ledger), `served_verbs`
(what this node executed for peers — it must equal what its clients shipped),
`batches` / `batched_verbs` (the coalesce factor), `dedup_hits` (replays
answered from the window), `retries`, `stale_term_refusals` (refusals this node
issued as an owner) vs `era_relearns` (times it learned a successor's era as a
client), `cross_owner_refusals`, `not_owner_refusals` (a client's ownership map
is stale), `dlm_grace_reclaims` / `dlm_grace_conflicts`, `mint_redirects`, and
the `dlm_token_cache_*` family — of which **`dlm_token_cache_misses` and
`owner_panics` must stay 0**. The offline assignment verb's own ledger lives
in the same object and is **0 on every mount by construction** (a daemon never
assigns): `owner_assignments` (volume ownership records written),
`owner_assign_refusals`, `subtree_roots_minted`. `meta_ship_phase_ns` and
`meta_ship_owner_phase_ns` decompose the added latency (route / queue wait /
encode / RTT / decode, and admit / dispatch / execute / reply encode) so a
regression can be attributed to a term instead of to "the network".

### Per-volume metadata owners (`squeezefs volume set-owners`)

**Status: the verb ships and both mount postures arm; nothing happens until an
operator runs it.** A set with no owner records behaves exactly as it always
has: one node holds every volume's claim, `meta_ship.armed` is false, and every
record on disk is byte-identical to a set formatted before this feature
existed. Running the verb is an explicit, offline, fleet-wide act. On an
assigned set the node that owns the slot-0 volume mounts as `set-authority` and
every other owner as `partial-authority` — the latter arms the client halves
toward the set authority (its custody lease, the allocation lane that lease
carries, the shipped publish path) composed with an owner half serving only the
volumes it appends to. An assignment is durable, verifiable (`get-owners`,
`locate`) and reversible (`--clear`). Design:
`docs/design-per-volume-claim-admission.md`; contracts
`tests/pv_owner_verb_tests.rs`, `tests/pv_partial_arm_tests.rs`. **Acceptance
is not in yet**: the fleet rows (the `tar -x` gate, the cross-owner refusal
table, the rewrite funnel) are the acceptance rung's, so treat a multi-owner
fleet as unproven at scale until that evidence note lands.

What it is: each metadata volume of a set gets a **durable owner** — the node
that appends to it. One appender per volume never changes; what changes is that
the appender may be a *different* node per volume, so several nodes are
metadata authorities for one volume set instead of one node being the authority
for all of them. Ownership is recorded in each volume's own claim-set record
and is read back at mount as *assignment conjoined with the live claim*: a
volume whose assigned owner is not claiming makes every mount of the set refuse
rather than serve a set with a hole.

```bash
# 1. every node unmounted; the set must already be multi-writer-capable
squeezefs volume enable-multi-writer sqmeta://<dev0>,<dev1>     # pre-flip sets only

# 2. see the plan and the cross-owner name count before anything is written
squeezefs volume set-owners sqmeta://<dev0>,<dev1> --dry-run \
    vol-0a1b2c3d4e5f6071=node_00000000deadbeef.m00000001:/projects/a \
    vol-1122334455667788=node_00000000feedface.m00000001:/projects/b

# 3. apply, acknowledging the counted population
squeezefs volume set-owners sqmeta://<dev0>,<dev1> \
    vol-0a1b2c3d4e5f6071=node_00000000deadbeef.m00000001:/projects/a \
    vol-1122334455667788=node_00000000feedface.m00000001:/projects/b \
    --accept-cross-owner-names 2

# 4. verify placement, then mount each node
squeezefs volume get-owners sqmeta://<dev0>,<dev1>
squeezefs volume locate     sqmeta://<dev0>,<dev1> /projects/b
```

* **It is offline and it is one invocation.** The verb takes the exclusive
  writer guard on **every** volume of the set for the length of the run, so it
  is momentarily the only authority in the fleet and writes every record
  itself. A volume held by a live node fails the run, naming that volume and
  its holder. Every volume of the set must be named in one invocation: a
  partial map is refused, because a volume nobody is assigned to belongs to
  everyone and to nobody and would refuse every subsequent mount.
* **It is bracketed, and a mount refuses inside the bracket.** The first act
  writes an ownership-assignment intent record; the last act deletes it. A
  write mount refuses while it exists, naming the re-run — so a run killed
  half-way never leaves a set an operator can mount in a half-assigned state.
  Re-run the verb with the same arguments to finish it (idempotent), or with
  `--clear` to unassign the set.
* **The subtree root is the part that makes it worth doing.** `:<path>` mints
  that directory with its inode on the volume being assigned, and everything
  created under it inherits that owner. Without one, a node owns a volume but
  no work: every new inode descends from the filesystem root, which belongs to
  the slot-0 volume's owner, so that node keeps sending all of its metadata
  operations to the same place as before. The verb admits an assignment with no
  subtree root and warns; `squeezefs volume locate` is how the placement is
  confirmed.
* **The slot-0 volume's owner is the set authority.** It assigns allocation
  lanes, serves the custody endpoint, owns the only freed-offset grace ring,
  coordinates maintenance jobs, and homes the filesystem root. The verb prints
  which node that is; mount it with `SQUEEZEFS_MW_ROLE=set-authority` and every
  other owner with `SQUEEZEFS_MW_ROLE=partial-authority`. **Mount order is not
  optional** — see the bring-up sequence below.
* **`--clear` is the rollback.** Offline, whole-set, and it restores the
  unassigned record byte-for-byte (the roster enrollment the assignment wrote
  is separate durable state and stays). The set then mounts as a single
  authority exactly as before. Subtree roots are ordinary directories and are
  left in place — deleting them is the operator's act.

#### Bringing a multi-owner fleet up (the order, and why it is the order)

Every node of the fleet exports the SAME multi-writer opt-in and the SAME set
authority endpoint; only the role differs. The sequence is forced by the
admission ladder, not by preference.

```bash
# --- on EVERY node, identically -------------------------------------------
export SQUEEZEFS_MULTI_WRITER=1
export SQUEEZEFS_MW_AUTHORITY=<set-authority-host>:7100   # its SQUEEZEFS_MW_BIND
export SQUEEZEFS_MW_BIND=<this-host>:7100                 # a KNOWN port, not `auto`
export SQUEEZEFS_MEMBERSHIP_BIND=<set-authority-host>:7200  # the owner's bind value
export SQUEEZEFS_JOB_WIRE_BIND=<this-host>:0              # writes job:enroll, the trust root

# --- 1. the SET AUTHORITY first (the owner of the slot-0 volume) ----------
SQUEEZEFS_MW_ROLE=set-authority squeezefs mount sqmeta://<dev0>,<dev1> /mnt/sqz --daemon

# --- 2. then each PARTIAL AUTHORITY ---------------------------------------
SQUEEZEFS_MW_ROLE=partial-authority squeezefs mount sqmeta://<dev0>,<dev1> /mnt/sqz --daemon
```

1. **The set authority must be up first.** A partial authority's admission
   rung 4 demands a LIVE membership lease, and the set authority is the
   membership owner (D20) — so a partial authority cannot even open the set
   before it. There is no ordering among the partial authorities themselves.
2. **Bind a known port on every node** (`SQUEEZEFS_MW_BIND=<host>:<port>`, not
   `auto`). Each owner publishes its endpoint in the claim-set record of the
   volumes it owns at arm, and its peers resolve it from there; an ephemeral
   port moves on every restart, so peers would keep dialling a dead address
   until their own next remount.
3. **The set authority learns its peers a moment after they arrive.** It
   mounts before any peer exists (see 1), so at that instant NOTHING claims
   the peer-owned volumes: it admits them **degraded** — it never appends to
   them and never takes their claims — and its entries for them carry no
   endpoint until each peer publishes one. It fills them in on its membership
   renewal cadence; until then a verb about that peer's volume refuses loudly
   at the ship site rather than guessing. Expect a short window of such
   refusals during bring-up, and none afterwards. The degraded state is
   readable: `peer_volume_unclaimed_admits` and the gauge
   `meta_ship.volumes_peer_unclaimed` — both **mount-time** readings, so the
   set authority reports one per peer for the life of that mount (it derived
   its map before any peer existed) and a peer mounting later reports only the
   owners still missing. Neither is a liveness monitor: `squeezefs volume
   get-owners` answers who is appending NOW, and it is what to read when a
   ship-site refusal outlasts the bring-up window.
4. **Verify before you use it.** On every node: `mount_posture` reads
   `set-authority` or `partial-authority`, `meta_ship.armed` is `true`,
   `meta_ship.not_owner_refusals` and `meta_ship.owner_panics` are 0, and
   `alloc_lane_writers` is the same width everywhere (rounded up to a power of
   two over the fleet — see the capacity table). On a partial authority
   `alloc_lane_shipped_reservations` grows and `alloc_lane_raise_refusals`
   stays 0.
5. **Take the fleet down in reverse**: partial authorities first, the set
   authority last. A partial authority whose set authority has gone loses its
   custody lease and self-fences at its own deadline; the volumes it owned then
   have no appender, so its subtree stops while the rest of the set keeps
   serving ([ownership does not fail over](#ownership-does-not-fail-over)).

#### The namespace becomes owner-partitioned — read this before assigning

This is a product statement, not a footnote. **At fleet scale SqueezeFS
presents an owner-partitioned namespace.** The operator divides the tree into
one subtree per authority. **Inside** a subtree everything is ordinary POSIX.
**Across** subtrees:

| Operation across two owners' subtrees | Result |
|---|---|
| `rename` / `mv` | `EXDEV`. `mv` works — coreutils falls back to copy + unlink, and the unlink half is inside one owner — but it copies the bytes |
| `link` (hard link) | `EXDEV`, with no fallback. An application that hard-links across the partition must be placed inside one subtree |
| `unlink` / `rmdir` of a name whose inode lives under another owner | `EXDEV`. This is why the verb counts such names and makes you acknowledge them |
| Everything else (`open`, `read`, `write`, `stat`, `readdir`, `chmod`, …) | Unaffected — any node reads and writes any file |
| `rmdir` of a subtree root itself | `EXDEV`. Removing a root is a teardown act: `volume set-owners --clear` first |

Lustre DNE remote directories and CephFS subtree pinning expose the same shape,
so this is a normal topology rather than a novel restriction — the difference
is that SqueezeFS refuses the cross-boundary operation loudly instead of
performing an expensive distributed transaction, and **publishes the refusal
rate**: `meta_ship.cross_owner_refusals` is a RATE, per verb, not a tripwire.
Watch it per workload; a rate above the band your placement predicts means work
is landing in the wrong subtree, not that something broke. A workload that
cannot be divided into weakly-interacting subtrees does not benefit from
per-volume owners and should run as a single authority.

**The existing-tree case, stated plainly.** On a set with a pre-existing tree,
inodes are spread across volumes by allocation history rather than by subtree,
so most parent/child pairs already live on different volumes and become
cross-owner names the moment an assignment is made. The verb counts them in one
pass and refuses unless `--accept-cross-owner-names <N>` matches the count
exactly. Acknowledging a large population is almost never right: it pays the
whole refusal cost and buys nothing, because the existing inodes do not follow
the new subtree boundaries. The supported shape is a fresh or newly-organised
set whose per-owner subtree roots the verb mints (cost: exactly one cross-owner
name per root — the root's own name).

#### Ownership does not fail over

**If a node that owns volumes dies, those volumes have no appender: its
subtree stops, and the rest of the set keeps serving.** Every verb about a
volume whose owner is absent refuses loudly at the ship site; every other node
still mounts, admitting that volume **degraded** — no node ever takes a claim
it was not assigned. The failure is immediate and visible
(`peer_volume_unclaimed_admits` and the `meta_ship.volumes_peer_unclaimed`
gauge at each mount, plus `squeezefs volume get-owners`, which prints the
drift in words), and the repair is an **offline verb requiring every node
unmounted** — a fleet-wide maintenance window. Compared with a single
authority, where the next mount simply reclaims a dead holder's claim, this is
an availability regression of the same order as the throughput gain: failure
probability grows with the number of owners while recovery goes from "restart
it" to "schedule an outage".

What still **refuses** a mount is a peer-owned volume whose `writer_claim`
cannot be reconciled with the assignment — a claim no holder attestation
names, or one naming a node the record does not entitle. That is not an absent
owner; it is something appending to a volume the set cannot account for, and
it counts on `peer_volume_unclaimed_refusals`. Verify the holder is gone and
run `squeezefs claim clear <sqmeta-uri>`, or re-assign offline.

The bounded opt-in is a **declared successor**: `vol-…=<owner>+<successor>`
records an ordered adoption candidate. A successor may take the volume only
when the ordinary writer-guard ladder would grant it the claim anyway (a
proven-dead holder, or a device-fenced preempt on a reservation-capable
substrate) — never from a live holder. It is empty by default because a
declared successor is a durable statement an operator must mean.

Two more consequences of several owners, both operational:

* **The whole-set offline fsck needs every node unmounted.** Online fsck under
  several owners covers each volume from its own owner and declines the
  destructive repairs (report-only); the full-teeth pass is the offline one,
  and that is a fleet outage. Plan it like a fsck window on any shared
  filesystem.
* **Maintenance has exactly one coordinator: the set authority.** `fsck`,
  `defrag` and `job submit` on any other node refuse, naming it. Participation
  as a detection shard is not affected — that is the coordinator asking.

#### Fleet width and stranded capacity

Every enrolled member gets a data-plane allocation lane, and the lane count is
rounded up to a power of two. Capacity in the unused lanes is honestly
unavailable, so **size fleets at powers of two**:

| Owners | Lanes | Stranded capacity |
|---|---|---|
| 2 | 2 | 0 % |
| 3 | 4 | 25 % |
| 4 | 4 | 0 % |
| 5 | 8 | 37.5 % |
| 8 | 8 | 0 % |
| 9 | 16 | 43.75 % |
| 16 | 16 | 0 % |

**Sixteen members is a hard bound** — the journal admits 16 appenders — and the
verb refuses a larger assignment, naming the count. Live gauges:
`alloc_lane_writers`, `alloc_lane_stranded_bytes`.

#### `squeezefs claim clear` under several owners

`claim clear` iterates **every** volume of the URI it is given, which was the
right behaviour when one node claimed all of them. On an assigned set it is
almost never what you want: clearing the whole set removes healthy owners'
claims along with the dead one's. Pass only the volume you mean —
`squeezefs claim clear sqmeta://<the one device>` — after confirming with
`squeezefs volume get-owners` which node is actually gone. The verb still
refuses a fresh claim, on every volume, exactly as before.

#### Compatibility — the one hazard, and it is an operator hazard

Per-volume ownership takes **no incompat bit** (it is gated on the capability
bit an upgraded set already carries), which means an **older SqueezeFS binary
does not refuse an assigned set**. It reads the ownership fields as unknown
keys and ignores them — and the sharp edge: the next membership change it
writes **drops them**, silently unassigning the volumes. It also does not know
the assignment-intent record, so it will happily mount a set a `set-owners` run
was killed in the middle of.

There is no format-level protection against this, by design (the program takes
no bit). The protection is operator discipline:

* every node that touches an assigned set runs a binary from the same release
  as the one that assigned it — verify with `squeezefs --version` on each node
  before the first multi-owner mount;
* if an older binary has mounted the set, re-run
  `squeezefs volume get-owners`: unassigned volumes on a set you assigned mean
  exactly this happened, and the repair is to re-run `volume set-owners`
  offline with the same arguments;
* `squeezefs volume set-owners --clear` before deliberately going back to an
  older release.

**The same-release sibling hazard: a mount that declares no role.** With the
whole fleet DOWN, a plain `squeezefs mount` of an assigned set is admitted —
correctly, since no peer holds a claim and the D0 guard has nothing to refuse
— and it behaves as the single authority it always was. What it does *not* do
is honour the owner partition: the owned-candidate mint filter engages only
when the ownership plane is armed, so files created under **another owner's**
subtree root land on whatever volume the ordinary rotor picks, minting fresh
cross-owner names that then return `EXDEV` on unlink in place. In a live fleet
this cannot happen (the owners hold their claims and an undeclared mount meets
the unchanged `FreshForeign` refusal). The discipline is the same as above:
on an assigned set, mount with a role — `set-authority` or
`partial-authority` — or `--clear` first.

### Subtree delegations & UPDATE intents (DLM stage S10)

**Status: built, proven live on the fleet rig, and default-ON — but read
only when the multi-writer ownership plane is armed, so every shipped mount
pays one relaxed load and nothing else.** These are the S8 serial-latency
recovery levers (design `docs/design-full-multi-writer.md` §8; evidence
`.benchmarks/2026-08-17-s10-{recall-valve,delegation,update-intents,slot-placement}.md`).
The honest bottom line lives in the section above: even with all three
levers ON, serial `tar -x` on a co-writer at 250 µs RTT runs **6.73× the
authority-local wall** — remote clients are throughput-oriented;
latency-sensitive serial metadata work runs on the owner.

**LOOKUP delegations** (`SQUEEZEFS_DELEGATION`, default on when armed; `=0`
is the A/B control; set-but-unarmed is announced-inert). A delegation is a
RAM capability token granted **piggybacked on a metadata reply the client
was already receiving** (acquisition never costs its own round trip; a
lookup earns the parent and the resolved child). What it buys: the holder
serves lookups / getattrs / readdirs — including **authoritative negative
answers** — from its local reader view, with zero wire traffic (the live
leg: 26 delegated serves, 0 shipped verbs across a 24-file stat pass; the
lever-off control shipped 50).

- **The coherence law**: the owner **recalls before any conflicting
  mutation publishes** — the mutation gate completes recall + ack before
  the transaction enqueues — and a holder drains in-flight serves before
  acknowledging a recall. `dlm_delegation_stale_serves` is the must-stay-0
  tripwire. Measured live: the authority's `touch` in a delegated directory
  returned only after the holder's ack, and the holder saw the fresh name
  immediately — no staleness window.
- **The staleness bound IS the recall**: under a live delegation the kernel
  attr TTL stretches to the channel-validity horizon, because the
  delegation is the coherence promise (the recall is what bounds
  staleness, not a timer). Grant currency is the volume's **journal commit
  watermark** — never an attr triple, which aliases under serial-create
  rates (the rung-12 live finding).
- **The recall valve** (no knob — structural): recall fan-out is
  rate-limited and batched with **derived** caps and deadlines
  (`dlm_recall_batch_max`, `dlm_recall_deadline_ms`,
  `dlm_recall_rate_cap_per_s` — all published on `.stats` so this page
  cannot drift), and an object cycling grant→recall ≥ 3 times inside the
  window **demotes to owner-served** for a derived cooldown
  (`dlm_thrash_demotions`) — a hot shared directory can never storm the
  wire. Overdue recalls are loud (`dlm_delegation_recall_timeouts`),
  fence the holder on this plane, and evict it from an armed membership
  plane.

**UPDATE intents** (`SQUEEZEFS_UPDATE_INTENTS`, default on when armed and
under delegation; `=0` is the A/B control). Per-directory **EXCLUSIVE**
UPDATE authority, earned on a shipped create's reply: the holder then
creates children **locally** — ino pre-supplied from an owner cursor
reservation, `O_EXCL` decidable locally against the grant-carried name
census — and ships ordered intent batches asynchronously (live coalesce ≈
25 intents/frame; deferred `utimes` ride the same lane, which is the tar
shape). What an operator must know:

- **`fsync(dir)` is the contract point**: an unshipped batch dies with the
  client (the acked-un-fsynced crash class, disclosed — MW-8), and a
  deferred apply-refusal (ENOSPC/quota at the owner) surfaces at
  `fsync(dir)`/close per the POSIX-16 errseq precedent, destroying the
  local mint and counting `meta_ship_intent_refusals` (must stay ≈ 0).
- **Foreign readers force the flush** (OQ-2): a foreign lookup/readdir
  under a delegated directory recalls the grant, which flushes the batch
  BEFORE the foreign serve — coherence over latency, priced at ~2 ms per
  foreign-read round at RTT ≈ 0. The published visibility bound is
  `meta_ship_intent_visibility_bound_ms`.
- Measured: intents take the 250 µs tar-x row from 144–145 to 161–162
  entries/s (−17 % wire verbs/entry) with the create/utime plane fully
  local — real, and deliberately NOT the ≤ 1.10×-of-local recovery, which
  needs per-volume claim admission — **built and accepted 2026-08-24**:
  [Per-volume metadata owners](#per-volume-metadata-owners-squeezefs-volume-set-owners)
  (the `tar -x` gate is MET at 0.98× of local on a 2-owner fleet,
  `.benchmarks/2026-08-22-pv-claim-admission.md`).

**Client-owned-slot placement** (`SQUEEZEFS_SLOT_PLACEMENT`, default on
when armed) is documented in the S8 section above: mint targeting engages
today (each shipping client's fresh inos land in a dedicated, migratable
slot); the migration half is structurally dark on a one-authority fleet
(no shipping client owns a volume) and **disarmed by design while a
multi-owner plane is armed** — every migration the policy could select
there is cross-owner and refused, so `migrations_triggered` /
`migrations_failed` stay 0 by construction while `migration_candidates`
counts the demand signal.

**Failover**: delegations and intents are RAM — a holder re-asserts in the
successor's grace window (NFSv4 pattern), un-reasserted grants are gone,
and a dead holder's batch follows MW-8. All state dies with a fenced
incarnation; a remounted successor of the same mount point earns fresh
grants (the incarnation fence clears on its first frame).

**Live signals** (`.stats`): the `dlm_delegation` family
(`grants` / `hits` / `recalls` / `reasserts` / `entries` / `bytes` —
bytes ride R5 as a sheddable component; tripwires
`stale_serves` must-stay-0, `recall_timeouts` loud), the `dlm_recall`
object (issued/acked/timed_out/frames/coalesced/rate_deferred +
`dlm_thrash_demotions` / `repromotions` + the published derivations),
`dlm_revoke_phase_ns` / `dlm_delegation_recall_phase_ns`, and the
`meta_ship_intent` object (`batches` / `verbs` — the coalesce factor —
`flush_forces`, `refusals` ≈ 0, `mints`, `local_negatives`,
`deferred_setattrs`, owner-face `applied` / `replays` /
`stale_refusals` / `read_recalls`, and the published
`meta_ship_intent_visibility_bound_ms`).

### Membership plane — lease-based liveness (DLM S6)

**What it replaces.** Every mounted client used to prove it was alive by
rewriting a `client:{uuid}` xattr on the root inode every 10 s — a full
metadata transaction, on the single metadata volume ino 1 routes to. That
plane serializes about **455 beats/s** at the measured saturated commit
wait, against the **1,500/s** a 15,000-client fleet needs; past saturation
records age out of the 45 s TTL and *live* mounts start reading as *stale*.
The read side was worse: listing clients meant one directory-style xattr
listing plus one attribute read **per client**, under a shared lock on that
same inode — paid by `squeezefs clients`, by `squeezefs status`, and by the
`format` preflight.

**What it is now.** Liveness is a **lease renewed over the cluster wire**,
held in the owner's RAM. The durable footprint is one `membership_owner`
record per volume, written when the owner arms and removed when it disarms —
never per beat. A heartbeat therefore costs **zero metadata transactions**,
and enumerating members costs one attribute read plus a paged census RPC
whatever the member count.

**Arming it (off by default).**

| Knob | Default | Purpose |
|---|---|---|
| `SQUEEZEFS_MEMBERSHIP_BIND` | `off` | Where a **write** mount serves the plane: `off`, `auto` (`0.0.0.0:0`, the discovery posture), or an explicit `addr:port`. A malformed value refuses the mount rather than serving somewhere you did not ask for. |
| `SQUEEZEFS_MEMBERSHIP_LEASE_TTL_MS` | `45000` | The **owner's** lease TTL. Defaults to the same 45 s the `client:`/`writer_claim` records use, so `live`/`stale` means one thing everywhere. |
| `SQUEEZEFS_MEMBERSHIP_SKEW_MAX_MS` | derived `max(TTL × 500 ppm, observed RTT)` | Clock-skew bound between owner and member. The default comes from a physical bound — two independent oscillators drift by at most ~500 ppm, i.e. 22.5 ms over a 45 s lease — not from a preference. |
| `SQUEEZEFS_MEMBERSHIP_PURGE_MS` | derived `max(2 × checkpoint cadence, observed RTT)` | How long a member may take to *stop* once it decides to (drop cached blocks, halt in-flight DMA). It is subtracted from the member's own deadline, so an honest value is a safety input. |
| `SQUEEZEFS_MEMBERSHIP_GRACE_MS` | derived (= the lease TTL) | A successor owner's failover grace window: reclaim admitted, conflicting fresh acquisitions refused, closing early once every prior member has re-asserted. |

A **read-only** mount needs no knob: it joins whenever it finds a fresh
`membership_owner` record, which is the first time a reader becomes visible
in `squeezefs clients` at all (a reader performs no metadata write, by
contract, so it has no record to enumerate). Arming requires the volume
set's `job:enroll` secret to exist — possession of volume access *is*
cluster membership — so enable the cluster listener
(`SQUEEZEFS_JOB_WIRE_BIND`) on the same mount.

**Two clocks, and the member's is stricter.** The owner expires a lease at
`T_owner`; the member fences its own objects at
`T_self = T_owner − 2·skew_max − D_purge`, measured from the instant it
*sent* the renewal, so the round trip counts against the member too. A
member that cannot renew in time therefore stops — a writer halts its DMA,
a reader drops every cached block — **before** the owner can hand those
objects to anyone else. A false-positive eviction costs availability, never
divergence. A configuration where that inequality collapses (skew plus purge
budget ≥ the TTL) **refuses to arm** and names the knobs; it is never
silently clamped.

**Owner failure.** Lease state is RAM-only by design and is rebuilt by
**re-assertion**: the successor bumps its durable writer era before arming
(an equal era is refused), then opens the grace window above, admitting
members that present the lease they already held and refusing conflicting
new acquisitions. Without that window a failover turns into a cluster-wide
forced-flush storm at the worst possible moment.

**Membership records (`claim_set`).** `writer_claim` expresses *exclusion* —
one holder. A volume carrying the claim-set capability additionally records
the **set of writer members**, each with its identity, endpoint and NVMe
registrant key, in one `claim_set` record rewritten only when membership
changes. Volumes formatted since the rung-10b flip carry the capability (bit
14) by default; a volume without it — pre-flip or `--single-writer` — reads
the singleton *projection* of `writer_claim` instead: no new record, no
changed claim bytes, and single-writer behaviour byte-for-byte as before.
Either way, an UN-ENGAGED set (no multi-writer arm) never writes a
`claim_set` key.
The registrant keys are the device-side face of set membership — several
registrants under **one** shared reservation, never a second reservation.

**Guarantee rows this does NOT change.** Write exclusion is still the
single-writer mount guard's (see
[Single-writer mount guard](#single-writer-mount-guard-guarantee-classes)):
the `flock`, the Write-Exclusive reservation and the `writer_claim`
heartbeat are untouched, and the membership plane grants no write custody
whatsoever. It answers *who is here*, not *who may write*. With the plane
off — the default — nothing about liveness, staleness or the guard changes.

### Freed-offset grace period (spec §6.8 item 3)

**What it fixes.** A reader resolves a file's block to a device offset and
caches the bytes under that offset. Block keys *are* bare device offsets, so
once the writer frees an offset and hands it to a different file, a reader
can serve the wrong file's bytes — loudly on a transformed volume (the AEAD
tag fails), **silently on a passthrough volume**. Dropping the reader's
whole block-key census at every revalidation epoch bounds that window to one
interval; it cannot close it, because nothing stops the writer from reusing
an offset *inside* the interval.

**What it does.** With the membership plane armed, a terminally-freed offset
is held out of the free list until **every live registered reader has
acknowledged passing it**. The acknowledgement rides the lease renewal a
reader already sends, so it costs no metadata write and no extra round trip,
and it means *"I have finished using anything freed at or before this"* —
emitted only after a revalidation pass that actually ran the block-key purge,
and only after the drain window in which pre-purge serves finish and the
reader's own layout/attr caches expire.

**A laggard is fenced, not waited on.** A reader that keeps renewing but
stops acknowledging would otherwise turn into the writer's ENOSPC, so past
the grace bound the writer names it, **evicts it from the plane** and
proceeds. Eviction costs that reader availability (it must re-join, and it
self-fences its own caches on its stricter deadline); it never costs anyone
correctness.

**The pressure-coupled release valve.** The rule above says what happens
when the supply runs out; the valve is what stops it running out. A rewrite
storm displaces blocks far faster than a reader answers on its routine
10 s beat, and the field showed the consequence: `free_grace_offsets`
climbing monotonically until the lane's share ENOSPCs (0 → 825 across one
8-rank row, never draining). Capacity was never the constraint, so the
writer instead makes the readers **answer sooner**, on a graded ladder:

1. **Prod.** The ring's own two end labels give the storm's measured
   deferral rate; against the smaller of the ring's headroom and the
   volume's free blocks that is a **runway** in milliseconds. When the
   runway is shorter than one acknowledgement cycle, the members the
   writer is waiting on are granted a **shorter renewal cadence** — the
   plane's own cycle inverted against the runway, floored at the shortest
   interval a reader's answer can actually change in (its revalidation
   cadence) — and their answers already in hand are read at once instead
   of at the owner's next sweep. Costs nobody any coherence.
2. **Tighten.** The fence deadline slides from the routine bound toward
   the pressure bound as the runway shortens — linearly, so there is no
   threshold to oscillate across. The pressure bound is the **floor**, and
   it is at minimum one honest acknowledgement cycle, so this rung can
   never fence a reader that is answering as designed.
3. **Force.** Unchanged, and still the last rung: a release without an
   acknowledgement, always together with that member's eviction.

Rungs 1 and 2 exist to make rung 3 and `free_grace_alloc_stalls`
unreachable on a healthy fleet. The prod rides the renewal a reader is
already making (on the isolated `sqz-lease` lane), so it needs no extra
round trip and is deliverable under exactly the storm that provokes it —
but its first delivery still waits out the member's CURRENT beat, so a
store whose whole runway is shorter than one routine cadence will still
reach ENOSPC. Size the free supply for at least one acknowledgement cycle
of displacement.

| Knob | Default | Purpose |
|---|---|---|
| `SQUEEZEFS_FREE_GRACE_MAX_MS` | derived (2 × one acknowledgement cycle ≈ 76 s with the shipped clocks) | How long a freed offset waits on a reader before that reader is fenced. One *cycle* is `3 × renewal interval + 3 × reader staleness bound + skew_max + D_purge` — every term a published number. A value **below one cycle refuses** rather than fencing readers that are answering as designed. |
| `SQUEEZEFS_FREE_GRACE_MAX_OFFSETS` | derived `max(budget/1024/24 B, 131072)` | Per-volume cap on held offsets. The floor is field-derived: 12.7 GB/s of saturated ingest over one cycle displaces ≈ 120 k 4 MiB blocks. At the cap the writer forces progress through the same fence act — never by quietly releasing something unacknowledged. |
| `SQUEEZEFS_FREE_GRACE_VALVE` | `on` | The pressure ladder above. `0` disarms rungs 1 and 2 (the A/B control): readers keep their routine cadence under write pressure and the fence deadline never tightens, which is the pre-valve shape whose measured signature is the monotone climb. Rung 3 and the ENOSPC ruling are unaffected either way. |
| `SQUEEZEFS_FREE_GRACE_ACK_RENEWAL` | `on` | Hold-time lever (b), reader-side (`.benchmarks/2026-09-06-free-grace-hold-time.md`): a promoted acknowledgement WAKES the lease-renewal loop and renews as *carriage* — the ack travels now instead of at the next beat (the lease renews, a prod is honoured, the beat and the learned label stay the routine renewal's). `0` = the ack rides the next routine beat verbatim. |
| `SQUEEZEFS_FREE_GRACE_REFRESH_ON_ACK` | `on` | Hold-time lever (d), writer-side: a renewal from a member whose recorded acknowledgement sat at or below the published bound marks the bound dirty, and the next harvest recomputes the minimum at once (rate-limited to the acknowledgement-refresh floor ÷ members, never faster than twice the recompute's own measured cost) instead of at the next floor beat or sweep. `0` = the floor-cadence / sweep refresh verbatim. |

**Space pressure: ENOSPC, not corruption.** If the free list is entirely in
grace, allocation refuses `ENOSPC` — promptly, loudly, counted in
`free_grace_alloc_stalls`. Reallocating an offset a reader may still resolve
would serve another file's bytes, silently, on a passthrough volume; a
bounded availability loss is the lesser failure, and this wait always ends
by itself because allocation evaluates a **shorter** pressure deadline (one
cycle, floored) and fences past it. `df` counts held offsets as **used**,
which is honest: they are genuinely unavailable until acknowledged.

**The wait is BOUNDED in wall time and TERMINAL past its wall** (the
2026-09-05 s11 fleet wedge, `.benchmarks/2026-09-06-cowriter-enospc-wedge.md`):
a write-path allocation parks on grace-held space for at most twice the
routine fence bound (`free_grace_fence_bound_base_ms`), floored at one
second — a duration, never the reallocation label — and on a co-writer only
while the authority's last harvest reply reported a held ring
(`free_grace_bound_age_ms` nonzero). Past the wall the `ENOSPC` stands for
that write: the block stripe is released and the write replies. Before the
fix a co-writer's exhausted-lane write parked forever under its block
stripe (the field's 30-minute watchdog storm with `.stats` in D-state).

**Live signals** (`.stats`, writer side). `free_grace_mode` is `off` (no
plane — the shipped default), `idle` (armed, no members) or `armed`:

| Signal | Healthy reading |
|---|---|
| `free_grace_deferrals` / `free_grace_releases` / `free_grace_offsets` | the ledger: `deferrals = releases + offsets`. `offsets` should oscillate with churn and fall back toward 0, not climb monotonically |
| `free_grace_bytes` | device bytes held — the space the readers currently owe you back |
| `free_grace_forced_releases` | **the tripwire:** offsets released *without* an acknowledgement, i.e. past the bound or at the ring cap. 0 on a healthy fleet; nonzero means at least one reader was fenced, and only that reader's coherence was ever at stake |
| `free_grace_laggard_fences` | readers evicted for not acknowledging. Investigate alongside `membership_renewals` — a reader renewing but not acknowledging is a revalidation problem, not a network one |
| `free_grace_alloc_stalls` | allocations that refused ENOSPC with offsets held. Expected only on a genuinely full store; sustained growth means the readers are too slow for the write rate (raise capacity, or shorten the cycle with `SQUEEZEFS_MEMBERSHIP_LEASE_TTL_MS` / `SQUEEZEFS_META_REVALIDATE_MS`) |
| `write_enospc_refusals` | WRITE replies refused `ENOSPC` — the synchronous face of exhaustion (a promotion or sparse-stripe mint the write itself owns; custody the never-lossy ladder ACKed reports at `fsync`, not here). Growth is honest pressure. Its complement is the tripwire: writes that neither reply nor count while `alloc_lane_enospc_refusals` climbs (`fuse_op_watchdog_overdue` growing on `write`) is the pre-fix wedge shape |
| `free_grace_bound` | the label the writer may reallocate up to; `free_grace_fence_bound_ms` / `free_grace_pressure_bound_ms` / `free_grace_ring_cap` publish the derived numbers in force so this page cannot drift from them. **`free_grace_fence_bound_ms` is the deadline IN FORCE** — it moves as rung 2 tightens it — and `free_grace_fence_bound_base_ms` is the un-tightened derivation beside it |
| `free_grace_pressure_pct` | the graded pressure reading: `0` = the supply outlives the routine bound at the measured deferral rate (quiet), `100` = it is already gone. It is a RATE-derived forecast, not an occupancy: a nearly-empty ring under a violent storm reads high, which is the point |
| `free_grace_prods` (rung 1) | grants that carried a shortened renewal cadence to a member the writer was waiting on. `0` on a quiet writer; growth under a storm is the ladder working, and `free_grace_prod_renew_ms` is the cadence currently being handed out (`0` = none in force). **Growth with `free_grace_bound` flat is the stop-and-read signal**: the ask is being delivered and not answered, so expect the fence next — check that reader's `meta_kv_revalidate_epochs` and its `free_grace_reader_acks` |
| `free_grace_prod_decays` | expiry windows in which a live ask RELAXED one doubling step toward routine instead of lapsing (finding 18, `.benchmarks/2026-08-25-s11-freeloop-stall.md`: under a storm the runway reading sawtooths, and a pre-fix lapse handed the very next renewal beat the ROUTINE cadence — one routine grant is one routine-beat acknowledgement hole, and `free_grace_bound_age_ms` inherits the widest member's hole). Growth under a bursty storm is the bleed working; `0` with `free_grace_prods` churning just means the readings never lapsed. The ladder retires at routine, and a DRAINED ring retires at once with no decay step |
| `free_grace_bound_tightenings` (rung 2) | harvests that evaluated a deadline below the routine bound. Its ratio against `free_grace_forced_releases` is the whole point of the valve: tightenings are supposed to be many and forced releases none. Both `0` while `free_grace_offsets` climbs means the valve is disarmed (`SQUEEZEFS_FREE_GRACE_VALVE=0`) |
| `free_grace_demand_pct` | **the coupling face**, beside the scarcity face `free_grace_pressure_pct` (the sustain campaign, `docs/design-free-grace-sustain.md`): a sustained rewrite whose throughput is paced by the loop reads ≈ 100 HERE while scarcity reads 0 — the measured s11 failure shape. Read `free_grace_demand_waits` (the standing site-0 detector's count) beside it; `0` with the loop churning = `SQUEEZEFS_FREE_GRACE_DEMAND=0` |
| `free_grace_demand_prods` | rung a′'s share of `free_grace_prods` (⊆): members asked to renew at the FLOOR cadence because recycle coupling — not space scarcity — is live. The fence deadline never tightens off this arm: asked sooner, never fenced sooner |
| `free_grace_bound_refreshes` | bound recomputes run on the demand/prod harvest path (T7 collapsed to the floor beat). Law: ≤ elapsed ÷ the floor — a breach is a bug |
| `free_grace_bound_age_ms` / `free_grace_residence_ms` | the loop-latency instruments (owner-clock `now − BOUND` while holding; the per-release residence histogram). Post-campaign target on a coupled storm: bound age ≤ 12 s sustained |
| `free_grace_hold_phase_ns` | **the hold DECOMPOSED** (the hold-time campaign, `.benchmarks/2026-09-06-free-grace-hold-time.md`): per released offset, `defer_checkpointed` (the first KV checkpoint completed after the free — the instant a reader's poll could adopt a root carrying the dereference), `checkpointed_min_acked` (until the first bound publish that covered the label — every member acknowledged past it) and `min_acked_released` (the harvest); `total` is the residence and the three are exact-sum with it over the placed population (`free_grace_hold_unplaced` counts the rest). On the s11 shape the middle stage carries ≈ 7 s — the readers' qualify (`staleness + skew`) and drain (`staleness + D_purge`) windows plus their pass/beat quantization — and the other two are sub-second; a large FIRST stage means the writer is not checkpointing (check `meta_kv_checkpoints`), a large LAST stage means nothing is allocating or freeing (the harvest runs per free and per allocation) |
| `free_grace_hold_ms` | the live hold — the EWMA of the residence at release — the number the capacity law multiplies your churn by (below) |
| `free_grace_member_ack_lag_ms` (`max` / `mean` / `min` / `members`) | owner-clock `now − acked` per live member (`now − joined` for one that has acknowledged nothing). The bound is the MIN over members, so `max` is the member holding the free list; `max − min` well above one beat names a laggard. With `SQUEEZEFS_STATS_KEY_CENSUS=1` the per-member census `free_grace_member_ack_lag_census` names it |
| `free_grace_checkpoint_marks` / `free_grace_checkpoint_cycle_ms` | the checkpoint marks the first stage is read against (≈ `meta_kv_checkpoints` on an armed writer) and the measured checkpoint cycle cost |
| `free_grace_bound_refreshes_on_ack` / `free_grace_bound_scan_ms` | lever (d)'s engagement (⊆ `free_grace_bound_refreshes`; growth under a storm is the arm working, 0 = `SQUEEZEFS_FREE_GRACE_REFRESH_ON_ACK=0`) and the measured cost of one O(members) minimum, the floor its rate limit rides |
| `free_grace_ack_renewals` (reader-side) | lever (b)'s engagement: promotions that carried themselves home at once (≈ `free_grace_reader_acks` under a storm; 0 = `SQUEEZEFS_FREE_GRACE_ACK_RENEWAL=0`) |
| `free_grace_pass_prods` / `free_grace_pass_interval_ms` (reader-side) | L2b's elastic revalidation passes: a prodded member also tightens its pass cadence toward the 1 s checkpoint ceiling (more qualify/promote passes — the qualification windows and the PUBLISHED staleness bound never move). Structurally 0 where the routine interval already sits at the floor; `SQUEEZEFS_FREE_GRACE_PASS_ELASTIC=0` restores the routine cadence verbatim |
| `free_grace_ack_pipeline_depth` / `free_grace_acked_lag_ms` (reader-side) | the pipelined acknowledgement ladder (L1): candidates in flight (≤ the derived cap; ≤ 1 under `SQUEEZEFS_FREE_GRACE_ACK_PIPELINE=0`) and the reader's own promote lag |
| `free_grace_reader_acks` (reader side — a reader's `.stats` reads `free_grace_mode: "reader"`, beside `free_grace_{learned,acked,reader_pending}_label`) | acknowledgements this reader has emitted. **Flat while the writer churns is the failure to look for** — it means this reader is holding the writer's free list. `learned` moving with `acked` flat says the revalidation pass is not advancing (check `meta_kv_revalidate_epochs`); a standing `reader_pending_label` says an acknowledgement is waiting out its drain window, which is normal |

## Observability

A mounted filesystem exposes live daemon metrics as JSON on the virtual **`.stats`** inode at the mount root (`cat <mountpoint>/.stats`) — the preferred live regression signal (layout mix, cache/tier counters, `meta_kv_*`, `writer_guard_*`, transport geometry, patch/fold ledgers, memory-budget level).

**`.stats` / `.config` access (VAL-7a).** Both virtual inodes are **mode `0400` owned by the mount uid** (with `-o default_permissions` always set, the kernel enforces that): their payload is a map of the daemon's private state — every backing-device path, every staging directory, the read-cache census and per-inode write custody — so on an `--allow-other` mount they must not be readable by co-tenants. Read them as the mount owner or as root.

**Lock-manager fields (`dlm_*`).** `dlm_mode` is the lock authority's mode: **`solo`** means this daemon is the lock master for every metadata slot, which is the only mode that ships — so `dlm_rpcs` (lock operations needing a remote slot owner) is **0 by construction and must stay 0**. `dlm_rpcs` keeps that meaning exactly: it counts **lock** round trips, never metadata ones — the metadata face is `meta_ship.dlm_rpcs_meta` (see [Metadata function shipping](#metadata-function-shipping-dlm-s8)). Nonzero on a single-node mount is a bug, never load: the lock refused rather than granting custody its owner never issued, and the daemon logged one loud line per event naming the object and its home slot. `dlm_term` is the durable writer era every fencing token this mount mints carries (the process-wide maximum; the per-volume face is `writer_guard_term`) — it must be strictly greater than any predecessor's on the same volume set, and `0` means the volumes predate incompat bit 7. Cross-**mount** write exclusion is not this subsystem's job in `solo` mode — it is the single-writer mount guard's (see [Single-writer mount guard](#single-writer-mount-guard-guarantee-classes)). Since **DLM S9** `dlm_rpcs` counts a *travelling* acquire wherever a custody client is armed, and the remote-custody ledger lives in the `dlm_custody` object beside it (`mode`, `dlm_custody_held`, the grant/renew/revoke counters, `dlm_custody_generation`) — all `0`/`off` on every mount that ships; the field guide is [Multi-writer data plane](#multi-writer-data-plane-dlm-stage-s9).

**Allocation-partition fields (`alloc_lane_*`, DLM S9).** `alloc_lane_writers` is the number of lanes a data volume's block space is partitioned into — **`0` means unpartitioned**, which is every mount that ships, and then every other field in the family is `0` by construction (a single writer installs no partition at all). On a partitioned mount: `alloc_lane_id` is this mount's own lane, `alloc_lanes_owned` is what it may mint in (its own plus every lane adopted after its holder was proven dead — `alloc_lane_adoptions` counts those acts), `alloc_lane_reservations` is the durable watermark commits (one per grain of **fresh** blocks per lane; reuse pays none, so raises ÷ fresh blocks is the live amortization factor and growth proportional to allocations means the grain collapsed), `alloc_lane_shipped_reservations` is the CO-WRITER engagement gauge (a co-writer holds no metadata authority, so every one of its raises travels to the authority and is committed there — 0 with a nonzero `alloc_lane_id` means the mount is committing its own frontier, which only an authority may do), `alloc_lane_raise_refusals` **must stay 0** (a raise refused for naming a lane the authority did not assign, a width that is not the era's, or a lease that is not custody — and any refusal means an offset was NOT handed out, so the write stalled loudly instead of using an uncovered one), `alloc_lane_stranded_bytes` is the **published capacity bound** — bytes belonging to lanes this mount cannot reach, including the unassigned lanes the power-of-two rounding leaves — and `alloc_lane_enospc_refusals` **must stay 0**: it counts allocations refused because this lane was exhausted while the set still had free space. The planning rules are in [Multi-writer capacity planning](#multi-writer-capacity-planning--the-data-plane-allocation-partition).

**Membership fields (`membership_*`, DLM S6).** `membership_mode` is `off` (no plane armed — the default), `owner` (this mount is the lease authority) or `member`. On an owner, `membership_members` / `membership_readers` / `membership_writers` are the live census, `membership_lease_ttl_ms` and `membership_self_deadline_ms` are the two clocks as armed, `membership_grace_remaining_ms` is a failover window in progress, and `membership_min_acked_free_epoch` is the freed-offset epoch every live member has acknowledged passing. The counters: `membership_renewals` is the heartbeat itself — it is the counter that used to be one journal transaction per client per 10 s, so it grows while `meta_kv_journal_entries` does not, which is the whole point; `membership_registration_commits` is bounded by mounts and membership changes, so growth proportional to renewals is a regression, not load; `membership_self_fences` **should stay 0** — nonzero means members are fencing their own objects because renewals are not completing (availability lost, divergence prevented); `membership_renew_refusals`, `membership_evictions`, `membership_grace_refusals`, `membership_grace_reclaims` and `membership_census_serves` are the refusal, revoke, failover and read-side ledgers. On a member, `membership_renew_sched_lag_ms` is a **max-gauge**: the worst observed intended-wake → actual-run scheduling lag of the renewal tick — growth toward `T_self` is lease-venue starvation surfacing in stats *before* it becomes a self-fence (the renewal cadences run on the dedicated `sqz-lease` thread precisely so meta-plane congestion cannot cause it; finding 2, 2026-08-20).

The **key census** fields (`read_lru_keys`, `write_lru_keys`, `nvme_staged_write_file_ids`, `nvme_read_cache_block_keys`, `active_writes`) are **opt-in**: set `SQUEEZEFS_STATS_KEY_CENSUS=1` on the daemon to populate them (read live, no remount needed — the flag also reports itself as `stats_key_census`). The census-free count gauges always export and are what tooling should key on: `read_lru_key_count`, `write_lru_key_count`, `nvme_staged_write_file_count`, `nvme_read_cache_block_count`, `active_write_block_count` (`squeezefs umount`'s unflushed-staged-write check reads the last two).

* **Show filesystem status:**
  ```bash
  squeezefs status [sqmeta://<meta_dev> | <mountpoint>]   # config + volume summary (JSON)
  ```
  The report's `"Clients"` array carries the volume's real mount registrations (same records and classification as `squeezefs clients` below). When the backing device is fabric-attached, the report also carries a per-volume `"Fabric"` section (see [Fabric observability](#fabric-observability)).

* **List client mount registrations:**
  Serves the `client:{id}` heartbeat records and the single-writer `writer_claim` recorded on the volume set's root inos — the same records the format preflight and the mount guard consume, under the same staleness law. Read-only probe: works beside a live mount and never perturbs it. States: `live` (fresh heartbeat), `stale` (heartbeat older than the 45 s TTL — crashed or partitioned holder), `dead` (writer claim whose same-host pid is provably gone — reclaimable immediately, no TTL wait).

  When a mount serves the [membership plane](#membership-plane--lease-based-liveness-dlm-s6), the report additionally carries its **live members** — kinds `member-writer` and `member-reader` — read from the owner's RAM census (one attribute read plus a paged RPC, independent of member count) rather than from per-client records. Read-only **coherent readers appear only this way**: a reader writes nothing, anywhere, so it has no record to list. Their `live`/`stale` classification is the same 45 s law as the records', measured against the lease instead of a heartbeat timestamp.
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

### The `.stats` families — complete reference

Every key the daemon exports, grouped by family. Shapes: a bare name is a monotone **counter** unless marked *gauge* (a live level) or *word* (a string/enum); `*_bytes` are bytes; families written `prefix_{a,b,c}` expand to one key per brace member. **Tripwires** are keys that must stay 0 on a healthy mount — nonzero is the stop-and-read signal, never load. **Engagement gauges** are the keys a benchmark row must account for before its number is valid (a row whose engagement delta does not match its op count measured something else). Percentages, ratios and "≈" laws are the reading, not a promise.

**Latency histograms — the one shape.** Every `*_phase_ns` / `*_gap_ns` / `*_timeline_ns` family (and `fold_fill`, `lease_lock_wait`, `block_lock_wait`, `write_lock_wait*`, `dlm_acquire_time`, `striped_block_concurrency`, `meta_reclaim_batch_size`, `ipc_ingress_ns`, `ipc_session_admission_ns`, `transport_commit_batch`, `fuse_write_inflight`) is an object per phase of the form `{ "buckets": { "<label>": n, … }, "count": N, "sum_ns": S, "mean_ns": S/N }` — 26 log-spaced buckets for the distribution plus **exact** count and sum, so `sum_ns ÷ count` is the true mean (not a bucket-midpoint estimate) and a phase table's members can be checked for containment to the nanosecond (`total ≡ Σ phases` wherever the family says *exact-sum*). Root-daemon and transport-side histograms share one bucketing core, so the two tables compose end to end (`fio clat − read_transport_phase_ns.total` is the kernel-side residue by subtraction). Histograms are always-on unless the family says it rides `SQUEEZEFS_OP_PROFILE`.

#### Identity, posture, build

| Keys | Meaning |
|---|---|
| `build_commit`, `build_tag`, `build_profile` (words) | The fleet mixed-version detector: full git commit (`-dirty` when built from a modified tree), the exact release tag (empty when untagged), the cargo profile (`release` / `dist`). See [Versioning & releases](#versioning--releases) |
| `mount_posture` (word) | `writer` / `reader` / `co-writer` — which of the three mount shapes this daemon is |
| `read_only_mount`, `reader_revalidate_interval_ms`, `reader_staleness_bound_ms`, `reader_staleness_bound_owners` | The read-only mount's posture and its two DERIVED numbers (interval + the writer's ≤ 1 s checkpoint ceiling — 0 on write mounts); `_owners` is the same bound per owner on a multi-owner set |
| `direct_device_true`, `write_shared_enabled`, `read_mostly_cache`, `read_lane_armed`, `zcrx_lane_armed`, `stats_key_census` | Posture words/flags echoing the mount's levers: strict device-true O_DIRECT, shared-mode write admission, read-mostly cache backing, the read-lane hold armed, the zcrx lane armed, the key census armed |
| `meta_format_version` (word, per volume), `meta_volume_atomicity` (`cow-checksummed`), `meta_volume_atomicity_physical` (`atomic4k` / `likely` / `unknown` / `file-backed`), `data_volume_write_cache` (`<dev>=<class>` per data device) | The durability faces: the metadata format (always `3`), the contract class vs the informational hardware probe, and the DATA plane's write-cache class (`write-back` = acknowledged writes stay volatile until a barrier) |
| `meta_routing_width`, `meta_slot_mint_spread`, `meta_slot_stamp_{runs,cursors}_max` | Dynamic metadata routing: the derived width (65536), the mint spread (64 slots per volume), the stamp encoding's high-water marks |
| `volume_states`, `volumes_peer_owned`, `placement`, `placement_table_refreshes`, `backend_placement_picks`, `backend_fill_{ratio,spread}` | Per-volume lifecycle state, volumes a peer owns (multi-owner sets), the balance-aware write-placement table and its refresh/pick ledger |
| `client_slot` | This mount's slot in the node's client identity (`node_{…}.m{slot}`) |
| `metrics` | The container object most counters below live in (`.stats` has a small top level: identity, caches, the `metrics` object, and the per-volume arrays) |

#### Tripwires (must stay 0) — the daemon-wide set

`invariant_tripwires` (a runtime concurrency-outcome violation reported instead of a panic), `detached_task_panics` (a detached data-path task unwound — its work is lost), `job_worker_panics`, `lane_exec_task_panics` (a handler completed by panic), `lane_exec_tick_rescues` (a lane notify that never delivered), `lock_ticked_reregisters` (a lost lock wake absorbed by the 2 s tick), `open_count_stranded` (a final forget with open handles counted — a lost RELEASE), `dir_nlink_underflows` (a suppressed parent-nlink decrement), `data_dma_fence_refusals` / `data_dma_epoch_refusals` (DMA refused because this mount is fenced / custody moved), `write_pipeline_fence_drops`, `rewrite_shadow_fence_drops`, `overlay_fence_drops`, `block_free_reclaim_fence_halts` (the write-side, rewrite-side, overlay-side and reclaim-side faces of a fenced writer), `block_double_frees` (an offset released twice), `block_untracked_free_refusals` / `block_live_free_refusals` (a free refused because the offset is untracked / still justified by the durable ledger — leak-safe containment of a double-free lineage), `block_key_incarnation_{refusals,exhausted,unknown}` (a block named by a dead incarnation), `transport_cq_overflows` (dropped ring completions; `transport_cq_nodrop` is the capability probe), `transport_requests_abandoned`, `read_dest_overruns`, `ipc_direct_reap_stalls`, `ipc_descriptor_rejects`, `ipc_sessions_poisoned`, `fuse3_zc_bridge_cancels` / `fuse3_zc_bridge_lost` / `fuse3_zc_bridge_orphans` (a zc bridge op hit its deadline / a ring completion was provably lost / a pend lost its deadline coverage), `fuse3_zc_release_failures` / `fuse3_zc_retain_refused`, `fuse3_zc_slot_payload_skips`, `zcrx_lane_poisoned`, `meta_kv_block_refs_drift` (fsck C8), `fsck_findings` (on a healthy volume), `fsck_map_orphan_records`, `staging_foreign_scope_{records,refusals}` (another node's staging root), `alloc_lane_{raise_refusals,enospc_refusals}`, `dlm_token_cache_misses`, `dlm_custody_grace_conflicts`, `dlm_grace_conflicts`, `membership_self_fences`, `free_grace_{forced_releases,laggard_fences}`, `meta_ship_publish.{refusals,owner_panics}`, `writer_guard_fenced` (this mount was fenced — read beside every fence-drop family above). `fuse_op_watchdog_overdue` and `transport_lease_overlong` are **loud-never-fatal**: growth is a long-running op or a write handler past the 1 s lease watchdog, attributed in the log.

#### Metadata store (`meta_kv_*` and the conveyor)

| Keys | Meaning |
|---|---|
| `meta_kv_node_cache_{hits,misses,evictions}`, `meta_kv_node_appends`, `meta_kv_node_{append,rewrite}_bytes`, `meta_kv_node_{compactions,splits}`, `meta_kv_node_freeze_shadow_dropped`, `meta_kv_node_dropped_tail_bsets` | The CoW node layer: cache behaviour, appends and rewrites, structural modifications, overlay records shadowed at freeze, torn tails dropped at load |
| `meta_kv_journal_{bytes,entries,full_stalls}`, `meta_kv_journal_{entries,bytes}_per_volume` (arrays) | Journal traffic (one checksummed entry per transaction) and the per-volume balance instrument — a healthy multi-volume set moves every volume under load; `full_stalls` = admission parked on a full ring |
| `meta_kv_checkpoints`, `meta_kv_commit_smo_retries`, `meta_kv_dentry_collision_overflows`, `meta_kv_delta_orphans`, `meta_kv_replay_{entries,dropped_torn,ms}` | Checkpoint cadence, lock-then-revalidate retries against structural modifications, dentry hash overflows, orphaned delta records, and the mount-time replay (torn entries detected-and-dropped, never applied) |
| `meta_kv_block_refs_{staged,released,recovered,drift,unresolved}` | The durable block-reference ledger (one record per reference); `drift` is fsck C8's live must-stay-0 |
| `meta_kv_free_extents`, `meta_kv_pending_free`, `meta_kv_pending_free_{parked,released,overflow}` | The A/B extent allocator: free extents, frees parked on durable-tail coverage and released by it (`parked − released` tracks the gauge), and at-cap overflow (≈ 0 steady state) |
| `meta_kv_block_map_{puts,deletes,run_puts,lookup_exact,lookup_range,lookup_floor,range_records}` (per volume) | The block-map tree's record traffic and lookup arms ([Large files](#large-files--the-block-map-tree)) |
| `meta_kv_fold_{head_serves,memo_hits,memo_misses,memo_bytes}` | Record folds served from overlay heads / snapshot memos (memo bytes are charged to the node-cache budget) |
| `meta_kv_times_echo_{absorbed,pending,drained,drain_commits}` | The kernel's SETATTR ctime/mtime echo absorbed instead of committed, and its drain |
| `meta_kv_revalidate_{polls,epochs,nodes_dropped,stale_serves,dirty_skips,keys_purged}`, `meta_kv_reader_load_retries`, `meta_kv_node_partition_refusals` | The coherent-reader revalidation family — all 0 for the life of a write mount; `dirty_skips` and `partition_refusals` are tripwires on every posture, and `epochs` advancing while `nodes_dropped` / `keys_purged` stay flat is a coherence promise not kept |
| `meta_commit_group_{size,bytes}` (histograms), `meta_conveyor_{leader_passes,pass_panics}`, `meta_conveyor_windows_inflight[_hwm]` (gauge), `meta_conveyor_durability_passes`, `meta_conveyor_group_{commits,txs}` | The commit conveyor: batch shape, apply passes, windows overlapped between the apply and durability stages (1 = serialized, ≥ 2 = overlapped), durability-lane passes, group commits (staged tx sets enqueued under one queue lock — txs ÷ commits = the group size; 0 on a solo mount) |
| `journal_ring_{lane,pool}_writes`, `journal_lanes_{spawned,ringless}` | Journal writes issued on the volume's own journal lane vs the shared pool (lane ≈ passes when `SQUEEZEFS_JOURNAL_LANE` is on), lanes spawned / lanes that could not build a ring |
| `meta_txpass_phase_ns` (exact-sum: `tx_queue_wait`, `pass_{admission,leaf_locks,journal_write,total}`, `window_lane_wait`, `window_total`), `uring_fs_write_phase_ns` (`queue_hop`, `device`, `wake_hop`, `total`) | The conveyor pass interior (`pass_total` = the apply stage's service time; ρ = Σ ÷ wall reads the serialized server) and the journal write's round trip by hop |
| `lock_phase_ns` (exact-sum; incl. `leaf_lock_hold` — the "never across device I/O" tripwire-by-magnitude — and `dlm_guard_wait`, the CONTENDED 4a acquire's parked span: one sample per contended acquire, so its count ≡ Σ the two 4a census classes below), `meta_op_phase_ns` | Lock-hold and metadata-operation phase histograms |
| `{dlm_inode,dlm_dentry,serve_ino,inode_meta,block_flush,lease_waiter}_stripes`, `…_stripe_collisions`, `…_key_waits`, `lease_waiters_parked` | D-3 stripe-collision census, per striped lock table: the width in force, contended acquires against a DIFFERENT key on the stripe (false sharing — the width's cost; on the two 4a classes each one is a full commit waited for nothing), and against the SAME key (true contention — the workload's own serialization, e.g. every same-dir rename on `I{parent}`). `lease_waiter` collisions are spurious wakes (the custody table is per key), never waits. `lease_waiters_parked` = lease asks parked right now |
| `meta_updates`, `meta_sync_requests`, `meta_device_syncs`, `meta_flush_deferred` | Metadata commits, barrier requests vs barriers actually issued (coalescing), commits deferred to the flush cadence |
| `meta_reclaim_batch_size` (histogram), `meta_reclaim_gather_{fill,window_closes,cap_closes,channel_closes}`, `meta_reclaim_inflight_waits` | Deferred inode reclaim: batch shape, why gathers closed (window / cap / channel), and parks on an ino a concurrent batch already claimed (never double-driven, never returned early) |
| `meta_parent_scans`, `readdir_parent_memo_hits` | Reverse-dentry scans (legitimate only on the `open_by_handle_at` reconnect path — growing per `readdir` is the regression) vs `..` served from the parent memo |
| `meta_slot_{migrations,records_copied,delta_keys,delta_overflows,gate_parked_commits,cutover_ms_max}` | Online metadata-slot migration: slots moved, records copied, the conveyor delta tee and its overflow fallback, commits parked on a per-slot cutover gate, worst cutover |
| `crossvol_tx_{started,completed,recovered}`, `crossvol_tx_midplan_escalations`, `crossvol_tx_steps_already_applied`, `xv_cross_owner_intents` | Cross-volume namespace transactions (`link`/`rename`/`unlink` spanning metadata volumes) and their crash recovery; cross-owner intents on multi-owner sets |
| `dlm_mode` (word), `dlm_rpcs`, `dlm_term`, `dlm_acquire_time` (histogram), `active_leases_count` (gauge), `lease_acquire_{ok,fail}`, `lease_lock_wait` (histogram), `lease_retry_{waits,exhaustions}` | The lock authority (`solo` on every shipped mount; `dlm_rpcs` must stay 0 there), the durable writer era, per-inode lease acquisition and the retry ladder (waits that later won vs the ladder giving up) |

#### Writer guard, data plane, multi-writer, membership, grace

| Keys | Meaning |
|---|---|
| `writer_guard_{mode,fenced,pr_reacquires,term}` | The single-writer mount guard: guarantee class in force (`flock+pr`, `flock+claim`, `co-writer`, `peer-owned`, …), fenced (tripwire), reservation re-acquires (a PTPL regression signal on SPDK targets), the durable writer term |
| `pr_registrant_shared`, `data_plane_fence_mode`, `data_plane_wero_reacquires`, `dlm_quarantined_offsets` (gauge), `dlm_quarantine_releases` | Data-plane custody: a shared PR host identity (co-located mounts), whether a WERO hold is device-enforced on the data namespaces, its re-acquires (growth = the target dropped reservations), the dead-epoch do-not-reallocate quarantine and its drain-proof releases |
| `dlm_custody` (object: `mode`, `dlm_custody_held`, `dlm_custody_{grants,renewals,releases,conflicts}`, `dlm_revokes_{issued,expired}`, `dlm_custody_unknown_leases`, `dlm_custody_self_fences`, `dlm_custody_{quarantined_offsets,drain_proofs}`, `dlm_custody_grace_conflicts`, `dlm_custody_grant_census` (opt-in)), `dlm_custody_generation`, `dlm_custody_epoch_advances`, `dlm_custody_phase_ns` (`rtt`, `arbitrate`, `adopt`, `renew`) | Remote write custody on an armed multi-writer plane — all `0`/`off` on a single-writer mount by construction ([Multi-writer data plane](#multi-writer-data-plane-dlm-stage-s9)) |
| `dlm_delegation` (object), `dlm_recall` (object), `dlm_delegation_recall_phase_ns`, `dlm_revoke_phase_ns` | Subtree delegations, the recall lane and their phase tables ([Subtree delegations](#subtree-delegations--update-intents-dlm-stage-s10)) |
| `range_custody` (object) | Byte-range custody on an armed plane ([Byte-range custody](#byte-range-custody-dlm-stage-s11)) |
| `meta_ship` (object), `meta_ship_publish` (object incl. `ship_frames`, `ship_framed_calls`, `ship_depth_waits`, `ship_session_dials`, `served_{frames,frame_calls,chains}`, `free_{shipped_blocks,served_blocks,replays,stale_refusals,ship_failures}`, `map_{shipped,served,refused}`), `meta_ship_intent` (object), `meta_ship_placement` (object), `meta_ship_phase_ns` (`route`, `queue_wait`, `encode`, `rtt`, `decode`), `meta_ship_owner_phase_ns` (`admit`, `dispatch`, `execute`, `reply_encode`, `total`) | Metadata function shipping and its publish/intent/placement planes ([Metadata function shipping](#metadata-function-shipping-dlm-s8)) |
| `cowriter` (object: `mw_role`, `custody_endpoint`, `admissions`, `admission_refusals`, `accounting_refusals`, `local_commit_refusals`), `peer_volume_local_commit_refusals`, `peer_volume_unclaimed_{admits,refusals}` | The co-writer posture and the per-volume-owner gate ([Multi-writer co-writer mounts](#multi-writer-co-writer-mounts-dlm-stage-s9), [Per-volume metadata owners](#per-volume-metadata-owners-squeezefs-volume-set-owners)) |
| `alloc_fresh_mints`, `alloc_from_freelist`, `alloc_lane_writers` (0 = unpartitioned), `alloc_lane_id`, `alloc_lanes_owned`, `alloc_lane_{reservations,shipped_reservations,adoptions,raise_refusals,enospc_refusals,stranded_bytes}`, `alloc_lane_{harvests,harvested_blocks,ahead_harvests,owed_blocks,reachable_blocks,harvest_watermark,harvest_horizon_ms,horizon_hints}`, `alloc_lane_{share_needed_blocks,headroom_pct}` (the capacity law, live), `alloc_lane_stale_records_{pruned,folded}` | Block allocation and the multi-writer allocation partition ([Multi-writer capacity planning](#multi-writer-capacity-planning--the-data-plane-allocation-partition)); the harvest family is the laned co-writer's refill from the authority; stale-record hygiene runs once per affected set |
| `membership_mode` (word), `membership_{members,readers,writers}`, `membership_{lease_ttl,self_deadline,grace_remaining}_ms`, `membership_min_acked_free_epoch`, `membership_{joins,renewals,renew_refusals,registration_commits,self_fences,evictions,grace_refusals,grace_reclaims,census_serves}`, `membership_renew_sched_lag_ms` (max-gauge) | The membership plane ([Membership plane](#membership-plane--lease-based-liveness-dlm-s6)); `renewals` grows while `meta_kv_journal_entries` does not — that is the point |
| `free_grace_mode` (word), `free_grace_{deferrals,releases,offsets,bytes}`, `free_grace_bound`, `free_grace_{fence_bound_ms,fence_bound_base_ms,pressure_bound_ms,ring_cap}`, `free_grace_reader_acks`, `free_grace_{forced_releases,laggard_fences,alloc_stalls}`, `free_grace_pressure_pct`, `free_grace_{prods,prod_renew_ms,bound_tightenings}`, `free_grace_demand_{pct,prods}`, `free_grace_bound_refreshes`, `free_grace_pass_{prods,interval_ms}`, `free_grace_ack_pipeline_depth`, `free_grace_acked_lag_ms`, `free_grace_hold_phase_ns`, `free_grace_hold_{unplaced,ms}`, `free_grace_checkpoint_{marks,cycle_ms}`, `free_grace_member_ack_lag_ms` (+ the gated `_census`), `free_grace_bound_refreshes_on_ack`, `free_grace_bound_scan_ms`, `free_grace_ack_renewals` | The freed-offset grace period and its pressure valve ([Freed-offset grace period](#freed-offset-grace-period-spec-68-item-3)); closure law `deferrals ≡ releases + offsets` |

#### Layouts, publish, block-map tree

| Keys | Meaning |
|---|---|
| `layout_{inline,staged,striped}_writes` | Writes by layout class (tiny inline / locally staged / striped blocks) |
| `layout_publish_{batches,batched_blocks}` | Per-inode publish coalescing: blocks ÷ batches is the live coalesce factor (≈ 1 on a streaming shape = the conveyor regressed to per-block publishes) |
| `layout_{delta,full}_commits`, `layout_delta_{bytes,folds}`, `publish_full_save_{indirect,chain_cap,other}` | Layout delta records vs full re-base saves, and why a full save happened |
| `publish_phase_ns` (exact-sum: `queue_wait`, `lock_wait`, `base_fetch`, `apply`, `save_encode`, `blob_write`, `meta_commit`, `commit_{guard,inode_read,slot_probe,tx_wait}`, `total`) | The publish path's residence decomposition |
| `publish_base_{dirty_serves,ram_serves,fetches}` | Where the read-modify-write base came from (closes exactly against the pass count; steady rewrite runs `ram_serves ≈ passes`) |
| `publish_commit_group{s,_saves}` | Delta-class layout saves aggregated into one multi-inode transaction per conveyor window (saves ÷ groups = the live aggregation factor) |
| `publish_indirect_blob_bytes`, `layout_indirect_map_read{s,_bytes}`, `publish_blob_{composes,orphan_reclaims,foreign_free_skips}`, `publish_compose_spills`, `publish_stale_binding_drops` | The legacy indirect-blob map path (trends to 0 on tree-mapped volumes), its compose/orphan hygiene, and a served publish dropping entries stamped with a dead block incarnation (the shield working under stale-cache churn) |
| `publish_map_record_bytes`, `map_migrate_{inos,records,resumed}`, `map_sweep_{jobs,chunks,records,resumed}`, `kvmap_write_map_bytes` (gauge), `kvmap_write_map_budget_bytes`, `kvmap_partial_inos` (gauge), `kvmap_{overlay,window}_saves`, `block_map_window_{bytes,hits,fills,evictions}` | The block-map tree ([Large files](#large-files--the-block-map-tree)) |
| `unpublished_offsets_recovered` | Pre-allocated-but-unpublished destinations recovered at mount |

#### Write path

| Keys | Meaning |
|---|---|
| `put_obj`, `get_obj`, `del_obj`, `fuse_ops` | Raw op counts (`get_obj` counts ranged reads by design) |
| `write_through_{blocks,bytes,fallbacks}`, `write_through_inplace_{overwrites,rewrites}` | Complete-block write-through past staging; never-lossy staging fallbacks; in-place full-block overwrites (`SQUEEZEFS_INPLACE_OVERWRITE`) and the brim in-place rewrite taken only at genuine space pressure |
| `write_path_seed_read_bytes`, `flush_seed_read_bytes`, `overwrite_seed_{materialized,skipped,deferred}` | Seed reads on the write path (**`write_path_seed_read_bytes` must stay 0** — a fully-covered buffer never pays a seed) and the overwrite-seed decision ledger |
| `active_block_ooo_runs`, `active_block_cow_copies`, `active_block_memset_elided_bytes`, `write_block_revisits`, `striped_block_concurrency` (histogram) | Kernel-split out-of-order WRITE segments (normal), CoW copies of a shared active buffer, zero-fills elided, block revisits at checkout, concurrent writers per striped block |
| `patch_{writes,write_bytes,dma_errors}`, `patch_edge_rmw_reads` (0 by definition), `patch_ineligible_{unmapped,decorated,unaligned,overlay,device_overlay,shared,range_shared,transform,adjacent,oversize,posture}` | The sole-owner in-place patch and its decision ledger — growth of an `ineligible_*` arm on a shape that should patch is predicate rot ([Random-small-write path](#random-small-write-path-sole-owner-patch--extent-overlay-design-docsdesign-random-small-writesmd)) |
| `parked_extent_bytes`, `parked_full_buffer_bytes` (gauges), `extent_{parks,escalations,implicit_escalations,spills,spill_bytes,record_absorbs}`, `fold_{passes,seed_reads,extents_folded}`, `fold_fill` (histogram, median ≥ 16), `staged_rider_{extent_writes,folds}`, `extent_records_{recovered,stale_discarded,torn_discarded,future_refused,foreign_scope}` | The extent overlay, batched fold and staged extent records (recovery classes kept distinct; `foreign_scope` = another writer's records skipped by the mount sweep) |
| `overlay_{stores,store_bytes,short_stores,store_fallbacks,store_submits_qids}`, `overlay_ack_early_{stores,bytes,retries,lost}`, `overlay_governed_stores`, `overlay_{installs,overwrite_installs,overwrite_bytes}`, `overlay_{publishes,published_bytes,unpublished_at_fsync}`, `overlay_{epoch_feeds,feed_fallbacks,supersessions,superseded_by_merge,claim_conflicts,window_escalations,enospc_declines,mover_skips,teardown_waits}`, `overlay_ineligible_{sub_cap,shadow_bound,range_shared}`, `overlay_gap_{seeds,seed_bytes,seed_old_bytes}`, `overlay_read_{serves,serve_bytes,drains,gap_serves,gap_bytes}`, `overlay_dma_{passthrough,pool_copy}_bytes`, `overlay_inflight_bytes` (gauge), `overlay_open` (gauge), `overlay_fence_drops` (tripwire) | The device-backed write overlay: stores and their ACK-early half, admission through the depth governor, the overwrite arm, publication, supersession, the length floor (`ineligible_sub_cap` ≈ per sub-cap aligned write on an armed mount), old-image seeding at settle (`gap_seed_old_bytes` growing on a rand-4k row is predicate rot), reads served from unpublished overlay bytes, and the DMA vehicle split |
| `write_pipeline_{inflight_blocks,inflight_bytes,depth_target,depth_target_base,admission_waits,admission_tick_wakes,depth_probe_ups,depth_probe_backoffs,supersessions,superseded_bytes,fence_drops}`, `write_pipeline_phase_ns` (exact-sum: `admit_wait`, `detach_lag`, `lock_wait`, `crypto`, `allocate`, `dma`, `publish`, `displaced_free`, `inval_tail`, `total`) | The write-pipeline depth governor (the runtime bandwidth-delay target and its probe-up multiplier; `admission_waits` is the honest-backpressure gauge) and the ten-phase residence histogram |
| `rewrite_{blocks,user_bytes,device_write_bytes}`, `rewrite_shadow_{swaps,bytes,parked_bytes,fallbacks,superseded,fence_drops,open_epochs}` | The rewrite program: `device_write_bytes ÷ user_bytes` is the vehicle-blind amplification (gate ≤ 1.05); shadow swaps with `open_epochs` → 0 at quiesce |
| `write_lock_wait`, `write_lock_wait_{exclusive,shared}` (histograms), `write_lock_scope_{entire,metaprep,shared}`, `write_lock_scope_shared_upgrades`, `write_lock_candidate_{entire,metaprep,shared}` | Inode write-guard acquisition: waits by mode, the scope each write took vs the scope it was classified for, and shared-to-exclusive upgrades |
| `write_lock_hold_{shared,metaprep,entire}` (histograms, exact `sum_ns`/`count`) | Inode write-guard HOLD per final scope — acquisition → drop (W-2 write-stream-guard, 2026-09-05). Closure: Σ counts ≡ Σ `write_lock_scope_*`. `shared`/`metaprep` are meta-prep only (µs — the P1-8 tripwire: a device-length value there means the guard is held across a DMA); `entire` legitimately spans the inline/staged commit and layout promotions |
| `block_lock_wait` (histogram), `block_lock_wait_by_site` (object) | Per-block flush-lock waits, total and attributed by call site |
| `nt_copy_bytes`, `numa_{local,remote}_bytes`, `numa_nodes`, `aligned_pool_{hits,misses}` | Non-temporal-store engagement at the DMA-destined copy sites; the NUMA-crossing estimate for daemon copy passes (`numa_nodes` = 1 ⇒ structurally inert); aligned-buffer pool recycling (misses ÷ total ≈ 0 is healthy) |
| `writeback_queue_depth` (gauge), `writeback_enqueued_{drain,flush,teardown,wt_fallback}`, `writeback_{superseded_noops,stale_token_retries,fence_noops,orphan_discards,retry_exhaustions}`, `writeback_errors_{latched,reported}` | The never-lossy writeback queue: admissions by driver, supersession-aware outcomes (fencing-stale ⇒ no-op, reclaimed-ino ⇒ orphan discard, transient ⇒ retry), and close-time error reporting (`latched − reported` = failures still waiting for an fsync to consume them) |
| `durable_upload_bytes_{writeback,self_flush,escalation}` | Durable block uploads by driver: the per-block flush unit, the memory-budget-Red self-flush, staging-refusal escalations |
| `staging_put_bytes_{drain,flush,teardown,wt_fallback}`, `staging_drain_barriers`, `spill_{seed_reads,seed_read_bytes,staging_puts,staging_put_bytes,pass_cacheless_skips}`, `restage_churn_{removes,bytes}`, `staging_sibling_{probes,hops_elided}`, `staged_rmw_pooled_seeds`, `staged_spill_escalations`, `staged_identity_retries`, `staged_payload_lost_reads`, `staged_truncate_{inplace_shrinks,durable_clips}` | Local NVMe staging: puts by driver, inline victim spills (the foreground writer paying a victim's seed), same-key re-stage churn, the staged-sibling probe (latch-free; hops elided when the occupancy index says absent), never-lossy StorageFull escalations, and truncate-shrink correctness arms. **`staged_payload_lost_reads` nonzero after a crash remount = acked-unfsynced staged data was lost and served as zeros (degraded, never errored)** |
| `staging_generation_discards`, `staging_writer_scope` (word), `staging_scope_upgrades`, `staging_foreign_scope_{records,refusals,discards}` | Staging roots bound to the filesystem generation and the node's writer scope: dead-generation wipes, the scope word, one-time scope upgrades, and the foreign-scope tripwires |
| `nvme_staging_{current,max}_bytes`, `nvme_staged_write_file_count`, `nvme_staged_write_file_ids` (census), `nvme_unaligned_write_fallbacks`, `active_write_block_count`, `active_writes` (census) | Staging tier occupancy, unflushed staged writes (`squeezefs umount`'s check), unaligned staging fallbacks, live active-write blocks |
| `bg_admit_capacity`, `bg_admit_available_permits` (gauges), `bg_spawn_{admitted,rejected}` | Best-effort background-task admission |
| `block_free_{discards,discard_bytes,file_punches,punch_bytes,trim_discards,trim_bytes,reclaim_skipped}`, `block_free_reclaim_{queued,queue_bytes,batches,commands,sync_drains,supply_drains,cap_parks,cap_overflow,elided,fence_halts,drain_rate,arrival_rate,queue_cap,park_bound_ms,park_tick_wakes}`, `block_free_elided_debt_bytes`, `block_free_debt_{drain_passes,pressure_drains}` | Device deallocation: discards/punches issued (blocks ÷ `commands` = the coalesce factor), the background reclaim queue (`sync_drains` must stay 0 except under real space pressure; `supply_drains` = fill-coupled pacing engaging at high fill; `cap_parks` = at-cap enqueues parked, `cap_overflow` ≈ 0 while the drain keeps pace — the norm when displacement outruns the target's deallocate service, each costing the write path one `park_bound_ms`; W-4: `drain_rate`/`arrival_rate` blocks/s are the derivation's measured inputs, `queue_cap`/`park_bound_ms` its live outputs, `park_tick_wakes` ≈ 0 = parks resume on room edges, not the tick), the discard-elision debt ledger (`queued + elided ≡ terminal frees`) and its drainer's pacing (bounded growth at idle) |
| `data_device_sync_requests`, `data_device_syncs` | Data-device barrier requests vs barriers issued (the difference is coalesced work; a ratio collapsing to 1.0 under concurrent fsync means coalescing stopped) |
| `data_read_lanes`, `data_write_lanes`, `data_write_lane_submits` | Per-device read/write submission lanes armed at mount and the write lanes' per-lane engagement |
| `dev_{enters,fills,submit_enters,wake_batches,wake_writes,wakes_elided}` | The NvmeBlockDev lane worker: `io_uring_enter` calls vs fills (enters ÷ fills ≪ 1 under concurrency), request-arrival wakes coalesced |
| `compress_stored_raw` | Incompressible blocks stored raw (best-effort compression) |
| `evacuate_{blocks_moved,bytes_moved,shared_blocks_moved,inflight_bytes,stale_token_noops,deferred_staged_blocks,replans,needed_bytes,avail_bytes,transient_bytes}`, `volume_preflight_refusals` | Volume drain/evacuation (a drain row is invalid unless `evacuate_bytes_moved` accounts for the victim's used bytes) and capacity-preflight refusals |

#### Read path

| Keys | Meaning |
|---|---|
| `cache_hits`, `cache_misses`, `cache_hit_ratio`, `cache_capacities` (object), `internal_caches` (object), `metadata_cache_size` | The legacy aggregate cache counters and capacity/occupancy objects |
| `read_mostly_policy_touches`, `read_mostly_dirty_pins` | The read-mostly cache backing: sampled policy touches (rate ≈ hot inodes ÷ horizon; growth proportional to op rate means sampling regressed to per-read) and evictions declined because the entry holds dirty layout state (≈ 0 steady state; growth = the persist cadence is not outrunning the idle horizon) |
| `read_lru_{current,max}_bytes`, `read_lru_key_count`, `read_lru_keys` (census), `write_lru_{current,max}_bytes`, `write_lru_key_count`, `write_lru_keys` (census) | The RAM read/write LRU tiers (counts always export; the key census is opt-in) |
| `hot_block_{hits,misses,evictions,probation_drops,dehydrate_skips,current_bytes,max_bytes}` | The RAM hot-block tier for large blocks |
| `nvme_read_cache_{current,max}_bytes`, `nvme_read_cache_block_count`, `nvme_read_cache_block_keys` (census) | The NVMe read-cache tier |
| `read_tier_admissions`, `read_tier_admission_ghost_hits`, `read_tier_admission_mode` (word), `read_fill_publishes_skipped`, `read_tier_publishes_paused` | Second-touch tier admission (skipped ≈ streamed cold blocks) and publishes paused under memory-budget Red |
| `read_admission_{evicted_unhit,wasted_bytes,governor_denials,governor_clamped,stream_transients}` | The scan-resistant admission governor (`governor_denials` ≈ 0 on fitting working sets; `wasted_bytes` ÷ device read bytes ≤ ~5 %) |
| `prefetch_{issued,completed,wasted,inflight_bytes,window_hwm,foreground_waits,evicted_unconsumed,active_streams}`, `read_streams_classified` | The sequential prefetch pipeline (`evicted_unconsumed` is the refetch-spiral detector) and stream classification |
| `ranged_{reads,read_bytes,read_unaligned_bounces,read_rebinds,read_ghost_escalations}` | Sub-block ranged reads on passthrough volumes (`ranged_read_bytes` vs user bytes is the rand-4k amplification bound) |
| `read_lane_{fetches,fetch_bytes,holds,serves,serve_bytes,hold_retired,hold_evicted_unconsumed,hold_ahead_evictions,wasted,covered_skips,depth_target,depth_probe_ups,depth_probe_backoffs,hold_bytes,inflight_bytes}` | The read lane: the hold (a completed-fill store the demand cohort serves from; `hold_evicted_unconsumed` is its spiral detector) and the probe-governed ahead lane |
| `read_odirect_{requests,tier_serves,ghost_admits}`, `read_device_true_reads` | Hybrid O_DIRECT: requests, tier serves and second-touch admits; device-true reads under the escape |
| `read_copy_{dest,bounce,warm_serve}_bytes`, `read_{dest,fill}_dma_bytes`, `read_dest_lease_bytes`, `read_zc_serve_bytes`, `nt_read_serve_bytes`, `ipc_arena_copy_bytes` | The READ copy ledger — every daemon CPU pass over read payload bytes by site (closure: `dest + bounce + dest_dma ≈ kernel user bytes`; `dest + arena + dest_dma ≈ il bytes out`; `bounce` ≈ 0 on cold il rows); the dest-window lease and zc serves are the copy-free subsets |
| `read_serve_phase_ns` (exact-sum: `prelude`, `meta_resolve`, `key_resolve`, `classify_probe`, `sf_wait`, `block_fetch`, `binding_check`, `slice_out`, `post_validate`, `total`), `read_fill_phase_ns` (`dev_queue`, `dev_service`, `fetch_dma`, `decode`, `admission`, `deposit`, `fill_total`), `read_transport_phase_ns` / `write_transport_phase_ns` (`queue_wait`, `dispatch_lag`, `reply_commit`, `commit_flush`, `transport_total`), `zc_bridge_phase_ns` (exact-sum: `msg_hop`, `sq_wait`, `device_cq`, `wake_hop`, `total`) | The read residence tables — per serve, per fill, per transport hop, and the zc direct leg's four hops around one DMA |
| `singleflight_waiter_result_serves`, `stale_binding_{rebinds,escalations}`, `read_settle_stale_head_refetches`, `seed_settle_escalations` | Cohort serves; the rebind ladder (rebinds are latency, escalations are exhaustions that took the serialized settle arm instead of EIO); cached-head refetches under served-publish churn; the same ladder on stripe-holding seed fetches |
| `lseek_holes_reported` | Sparse-file export engagement (0 across a `cp --sparse` run means no tool learned about a hole) |

#### Memory budget (R5)

| Keys | Meaning |
|---|---|
| `mem_budget_{bytes,pressure_bytes,gauge_sum_bytes,unreclaimable_bytes}`, `mem_budget_level` (word), `mem_budget_components` (object — each component's `current`, `floor`, `weight`, `sheds`) | The daemon's joint memory budget, what is charged against it, and its level (Green / Yellow / Red — Red sheds and pauses, never OOMs) |
| `mem_budget_{yellow_events,red_events,hard_backstops,backstop_active,tier_publish_paused,dehydrate_paused,floors_clamped}`, `parked_gate_{waits,self_flushes,timeouts}` | Pressure transitions and their consequences; `hard_backstops` / `parked_gate_timeouts` growing on a quiet workload is a convergence regression |

#### FUSE transport (kernel lane)

| Keys | Meaning |
|---|---|
| `fuse_over_uring_{sessions_active,registers,requests,replies,cqe_errors}`, `transport_classical_sideband` | FUSE-over-io_uring session state and traffic; requests the kernel routes over the classical sideband (FORGET/INTERRUPT/resends) |
| `transport_{queues,q_depth,payload_buffer_bytes,max_background,max_write,max_pages,max_readahead}`, `transport_drain_groups`, `transport_drain_group_width` | Negotiated transport geometry (queues = possible CPUs; depth degraded to the payload cap; INIT limits; `max_readahead` echoed verbatim) and the drain-context grouping |
| `transport_payload_leases`, `transport_leases_outstanding` (gauge), `transport_lease_max_age_ms`, `transport_lease_overlong`, `transport_ents_retired`, `transport_slots_overdue`, `transport_parked_commits`, `transport_unparked_commits`, `transport_park_backstop_ticks` | The zero-copy WRITE payload lease and its watchdog; the park ledger (`parked − unparked` at quiesce is the wedge count — closure is the invariant) |
| `transport_commit_batch` (histogram), `transport_commit_batch_{commits,flushes}`, `transport_wake_writes`, `transport_wakes_elided`, `transport_reap_gap_ns` (`blind`, `blind_cqe`, `park`) | COMMIT_AND_FETCH batching per drain, queue-eventfd wake coalescing (`writes ÷ (writes + elided)` ≈ 1 under saturation means the coalescer stopped eliding), and the worker's park-gap instrument |
| `transport_fast_dispatch_{serves,demotes}`, `fuse3_read_inplace_replies`, `fuse3_write_inplace_replies`, `fuse3_tpc_lane_redispatches` | The READ dispatch law (`fast_dispatch_serves + fuse3_zc_read_fusions + fast_dispatch_demotes ≡ READs`), in-place reply arms, and dead-lane re-dispatches (0 on a healthy daemon) |
| `transport_spin_{absorbed,expired,ns,refused_busy,window_us}` | The queue workers' spin-before-park governor (`absorbed + expired ≡` spins; all 0 on the default) |
| `transport_replies_{dropped_no_slot,oversize,refused_stale}`, `transport_requests_failed_synthetic`, `transport_dest_dma_leases` | Reply-integrity outcomes (a reply that could not be delivered is failed synthetically, never silently dropped) and read-destination owner-token claims (≈ one per dest-bearing device-read on an armed session) |
| `transport_timer_{arms,tombstones_skipped,heap_entries,live_sleeps}`, `timer_{arms,tombstones_skipped,heap_entries,live_sleeps}` | The two timer registries (transport-side and daemon-side): arms, cancelled arms popped later, heap occupancy, live sleeps |
| `fuse3_kmbuf_negotiated`, `fuse3_zc_negotiated`, `fuse3_zc_retention_negotiated` (0/1), `fuse3_zc_replies`, `fuse3_zc_fallbacks`, `fuse3_zc_write_{directs,direct_bytes,extractions,extract_bytes,lazy_extractions}`, `fuse3_zc_write_{fusions,fusion_bytes,fusion_demotions}`, `fuse3_zc_read_{fusions,fusion_demotions}`, `fuse3_zc_{retain_commits,retained_outstanding,releases,release_failures,retain_refused,slot_payload_skips}`, `fuse3_zc_bridge_{cancels,lost,orphans}`, `fuse3_fused_{midpass,passbottom}_reaps`, `fuse3_fused_timeline_ns` | The zero-copy serve integration on the sqz kernel ([FUSE zero-copy serve integration](#fuse-zero-copy-serve-integration-sqz-kernel-opt-in)): negotiation, reply/write vehicles, handler/worker fusion, payload retention, bridge deadlines, and the fused lane's mid-pass vs pass-bottom bridge resolutions |
| `fuse3_numa_{local,remote}_bytes` | The NUMA-crossing estimate for transport passes (payload delivery, reply serves) |
| `fuse_write_inflight` (histogram), `fuse_write_phase_ns`, `fuse_op_phase_ns`, `fuse_create_under_lock_ns` | Per-op write concurrency and phase histograms (`fuse_op_phase_ns` / `fuse_create_under_lock_ns` ride `SQUEEZEFS_OP_PROFILE=1`) |
| `fuse_{flush,release}_clean_fastpath`, `fuse_lookup_negative_replies`, `fuse_attr_cache_refreshes`, `fuse_killpriv_negotiated` (0/1), `fuse_killpriv_clears`, `fuse_reserved_xattr_refusals`, `fuse_dio_write_invals`, `fuse_dio_inval_detached` | Round-trip economy (clean-handle FLUSH/RELEASE, negative dentries, refresh-instead-of-invalidate), the killpriv-v2 clearing law, the reserved-xattr tamper tripwire, and DIO-write page-coherence invalidations (`detached` ≈ 0; growth marks ring-capacity pressure) |
| `op_trace_{armed,samples,dropped,divisor}` | The per-op trace ring's state (`SQUEEZEFS_OP_TRACE`; `cat <mnt>/.trace` drains it) |
| `daemon_cpu_ns`, `daemon_cpu_ns_by_class` (object: `fuse3-tpc`, `fuse3-ur`, `sqz-ipc-svc`, `sqz-ipc-dd`, `sqz-meta`, `sqz-jrnl`, `sqz-blk`, `sqz-nvme`, `sqz-zcrx`, `sqz-timer`, `other`) | Daemon CPU by thread class — the CPU/op denominator every campaign row cites |
| `uring_queue_full` | io_uring submission-queue-full events |

#### LD_PRELOAD interception (`ipc_*`, `il_*`, `placed_*`)

| Keys | Meaning |
|---|---|
| `ipc_sessions_{active,total,poisoned,reaped}`, `ipc_session_owners`, `ipc_binds`, `ipc_binds_dev_override`, `ipc_bind_refused_{version,nonce,flags,mode,budget,peercred,disabled}`, `ipc_admission_refusals`, `ipc_descriptor_rejects`, `ipc_session_admission_ns` (histogram) | Session lifecycle and the refusal ledger (`disabled` = a mount without `--interception` refusing data sessions — the expected default posture); `dev_override` nonzero outside dev boxes is a fleet-hygiene alarm |
| `ipc_arena_bytes`, `ipc_arena_prep_{queued,done,skipped_dead,skipped_pressure}` | Session payload arenas and the deferred huge-page prep worker (skipped when the session died first / under memory-budget Red) |
| `ipc_ops_{read,write}`, `ipc_bytes_{in,out}` | **The interception engagement instrument** — an il row is invalid unless these deltas account for its ops |
| `ipc_fast_path_serves`, `ipc_async_handoffs`, `ipc_fast_path_{lock,miss}_demotions`, `ipc_hold_probe_{serves,misses}`, `ipc_read_dest_serves` | The sync warm fast path vs handoffs into the FUSE handler bodies, why serves demoted, the read-lane hold probe on the service thread, arena-destination read serves |
| `ipc_direct_drive_{submits,serves,bounces,fallbacks_post}`, `ipc_direct_ineligible_{backend,layout,meta,overlay,policy,shape}`, `ipc_direct_shards`, `ipc_direct_inline_reaps`, `ipc_direct_reap_stalls`, `ipc_direct_phase_ns` (`admit`, `inflight`, `finish`, `total`, `sq_wait`, `device_cq`), `ipc_dd_cadence_{k,probe_ups,probe_backoffs}` | The direct-drive READ lane (device reads issued from the interception host's own rings): decision ledger, shards, inline reaps, the per-op phase table (`fio clat − total` = client + ingress), and the governed issue cadence |
| `ipc_dd_write_{serves,bytes,block_parks,park_redrives,times_dispatches,fence_refusals}`, `ipc_dd_write_ineligible_{shape,align,backend,block_lock,fence_backoff,killpriv,lease,range,overlay}` | The direct-drive WRITE lane (patch-shaped ring writes DMA in place) and its per-cause decision ledger |
| `ipc_ingress_ns` (histogram) | Client-publish → daemon-dequeue ring-ingress residence (the measured "client + ingress" term; count delta must account for the row's ring ops) |
| `ipc_drain_{pass_ns,flush_ns}` (histograms), `ipc_drain_empty_passes`, `ipc_service_threads` (gauge), `ipc_service_parks`, `ipc_spin_{window_us,absorbed_parks,disengaged_busy}` | Service-thread drain passes (ops ÷ count = live ops per pass), empty passes, spawned threads, parks, and the spin governor |
| `ipc_cqe_wake_{writes,elided,collapsed}`, `ipc_cqe_pass_wake_flushes`, `il_park_eras`, `il_slot_reroutes`, `il_submit_harvested` | The completion doorbell: wakes paid only toward parked reapers (`writes ÷ (writes + elided + collapsed)` is the wake economy), pass-scoped flushes, client park eras (the wakes-per-era denominator), slot reroutes, harvested submits |
| `ipc_severed_pool_{hits,misses,bytes}`, `ipc_placed_severs`, `ipc_placed_sever_fallbacks`, `placed_{adoptions,adoption_refusals,merge_elides,assembly_bytes}` | Ring-write buffer economy: the severed-buffer pool and the one-copy placed-sever path (whole-block ring chunks sever straight into the assembly the first merge adopts) |
| `ipc_inval_{notifies,suppressed,attrs_only}` | Kernel page-cache invalidations after ring writes (rate-limited per window; size growth always fires an attrs-only notify) |
| `ipc_lane_gate_{threshold_bytes,kernel_routes,kernel_bytes}` | The hybrid lane gate: ops above the derived threshold routed to the kernel lane |

#### Jobs, fsck, scrub, defrag

| Keys | Meaning |
|---|---|
| `job_{submitted,completed,cancelled,failed,tasks_done,checkpoint_writes,copy_buffer_bytes,paused_mem_pressure,paused_capacity,serialized_waits,mover_custody_defers}` | The maintenance job fabric ([Jobs & distributed execution](#jobs--distributed-execution)); `paused_*` are the loud pauses an operator resumes |
| `job_remote_{workers,enrollments,enroll_refused,shards,submissions,refused_stale,lease_expiries,reassignments,bytes_moved,verify_read_bytes,quarantined_destinations,pr_preempts,fence_mode}` | Remote job-shard execution over the wire (`shards` is the remote-engagement instrument; `enroll_refused` / `refused_stale` are security/fencing tripwires) |
| `job_fleet_{shards_dispatched,shards_completed,shards_relocal,worker_shards,worker_red_aborts}` | Fleet-parallel maintenance on an armed membership plane |
| `fsck_{inodes_scanned,nodes_walked,blocks_checked,refcounts_checked,suspects,suspects_cleared,findings,scan_secs}`, `fsck_{epoch,inflight,mover_ledger,current_era,crossing,foreign_lane}_exempted`, `fsck_inode_plane_{volumes_covered,proposals_admitted,proposals_stripped,foreign_scoped,cross_owner_declined}` | fsck detection and its zero-false-positive shields (each `*_exempted` is a shield engaging: allocation epoch, in-flight registry, mover ledger, the writer era's ino floor, an in-flight block-map crossing, a foreign allocation lane) |
| `fsck_dentry_refs_indexed`, `fsck_nlink_mismatch_{high,low}`, `fsck_nlink_zero_named`, `fsck_dangling_dentries`, `fsck_nlink_names_counted`, `fsck_nlink_transient_cleared` | Classes C9/C10 — `mismatch_low` / `zero_named` / `dangling_dentries` are the LOSS direction ([fsck / scrub](#fsck--scrub)) |
| `fsck_map_{orphan_records,empty_heads,run_foreign_shadows}` | Class C11 ([Large files](#large-files--the-block-map-tree)) |
| `fsck_repairs_{planned,applied,refused}`, `fsck_repair_refused_multi_owner`, `fsck_quarantined_{records,blocks,bytes}`, `fsck_repair_classC{1..10}` | Repair planning and execution (dry-run by default, quarantine-first; C8 has no repair arm and C11 is report-only, so neither has a repair counter) |
| `scrub_{blocks_scanned,bytes_scanned,aead_verified,frame_verified,readability_only,failures}` | The C7 data scrub by stored form |
| `defrag_{blocks_moved,bytes_moved,folds_kicked,meta_compactions_kicked,passes}`, `frag_d1_{contiguity,reclaimable_tail}`, `frag_d2_locality`, `frag_d3_pressure_bytes`, `frag_d4_dead_bset_ratio` | The four-axis defragmenter and its gauges |

#### Fabric, zcrx lane

| Keys | Meaning |
|---|---|
| `fabric_controllers`, `fabric_ctrl_not_live`, `fabric_ctrl_reconnects` | NVMe-oF initiator state sampled from sysfs ([Fabric observability](#fabric-observability)) |
| `zcrx_lane_armed`, `zcrx_{fills,fill_bytes,gather_bytes,dest_gather_bytes,hdr_copy_bytes,fill_fallbacks}`, `zcrx_area_bytes`, `zcrx_area_admission_waits`, `zcrx_{recv_parks,recv_failovers,conn_errors,degraded_bypasses,structural_teardowns,starved_ms}`, `zcrx_parks_{cq_full,pool_dry,rq_empty}`, `zcrx_frame_violations`, `zcrx_lane_poisoned` | The opt-in zero-copy-receive read lane (`SQUEEZEFS_ZCRX_LANE`; engagement law `gather ≡ fill`; `lane_poisoned` is the tripwire — `docs/design-zcrx-read-lane.md`) |

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

> **fio engine policy (user ruling 2026-08-07, `.benchmarks/2026-08-07-fio-engine-policy.md`):** every throughput/IOPS row runs `ioengine=libaio --direct=1` with a stated iodepth on BOTH lanes (kernel and il — the il lane rides the v1.1 aio interposers), every A/B comparison uses the SAME engine both sides, and psync survives only as explicitly-labeled §5.5.1 sync-lane coverage rows (never a headline, never cross-lane compared); `io_uring` is a labeled kernel-lane-only extra.

Every number traces to a committed `.benchmarks/` note (box/substrate/method inside each). Two venues carry the record: the **field** (squeeze-test — the fabric the product is built for) and the **reference dev box** (the 2026-07 programs' controlled-latency substrate). The README's four hero numbers are the first field row below.

### Field records (squeeze-test)

**Venue:** 5 storage nodes over nvme-tcp, memory-backed (nullblk) NVMe targets; client 32-core Xeon 6426Y, 251 GB RAM, 2×200 GbE; cacheless mount with `--interception`. Two lanes per row: **kern** (kernel FUSE-over-io_uring) and **il** (`LD_PRELOAD=libsqueezefs_il.so`, same-commit pairing). Tier: measured-real. 4 KiB rows are 24 jobs × iodepth 8, closed-loop (≈ 190 ops in flight), so every il-vs-kern delta is a latency delta at equal depth.

| Row | Result | Binary / instrument | Record |
|---|---|---|---|
| **Hero row — user-run EXA client-validation script v1.2.1, il mode, 40 s rows** | **Read BW 43.9 GB/s · Write BW 36.4 GB/s · Read IOPS 942 k · Write IOPS 727 k** | `c985fa8c`; the user's instrument, not the rig's | [`.benchmarks/2026-09-02-e2e-audit-baseline.md`](../.benchmarks/2026-09-02-e2e-audit-baseline.md) (addendum) |
| Sequential read, 1 MiB, 24 × qd16 (`r_cold`) | il **40.35 GiB/s** (97 % of the 41.6 GiB/s wire payload) / kern 36.88 GiB/s (89 %) | `aecf1561`, fio libaio 30 s + 10 s ramp | same note |
| Sequential rewrite of tree-mapped files (`w_rewrite`) | kern **32.26 GiB/s** / il 30.48 GiB/s; `w_durable` (`end_fsync`) 31.0 GiB/s both lanes | `aecf1561` | same note |
| Random 4 KiB read, 24 × qd8 (`rr_4k`), kern | **434–441 k IOPS** (clat ≈ 0.43 ms) before the read-path work → **512 k sustained** (+14.8 %, A-B-B-A) after READ fast-dispatch; the composed default binary 507–510 k | `aecf1561` → R-2 branch | [`2026-09-03-4k-random-attribution.md`](../.benchmarks/2026-09-03-4k-random-attribution.md), [`2026-09-03-r2-read-fast-dispatch.md`](../.benchmarks/2026-09-03-r2-read-fast-dispatch.md), [`2026-09-03-r3-fill-issue-economy.md`](../.benchmarks/2026-09-03-r3-fill-issue-economy.md) |
| Random 4 KiB read, 24 × qd8 (`rr_4k`), il | **858–874 k IOPS** (clat ≈ 0.22 ms) | `aecf1561` | same notes |
| Random 4 KiB write, 24 × qd8 (`rw_4k`) | kern 476 k / il 704 k IOPS (the in-place patch path) | `aecf1561` | baseline note |
| Raw fabric controls (fio libaio direct on the ten data namespaces, FS idle) | qd1 4 KiB RTT **24.8 µs** mean; 4 KiB randread ceiling **1.90 M** (24×8, the FS job's shape) – **2.03 M** (32×8); 1 MiB seq read **38.59 GiB/s** (24×16) | same client, same day | baseline addendum + the 4k attribution note |

The campaign notes behind the read-side movement — `2026-09-02-r1-device-read-executor.md`, `2026-09-03-r2-read-fast-dispatch.md`, `2026-09-03-r3-fill-issue-economy.md`, `2026-09-03-r4-reap-thread-economy.md` — and the write/metadata side — `2026-09-02-w3-overlay-depth-governor.md`, `2026-09-02-f46-kvmap-stream-publish.md`, `2026-09-02-d1b-publish-plane-batching.md`, `2026-09-03-d2-two-stage-conveyor.md`, `2026-09-03-c2-uring-fs-completion-hop.md` — each state their bracket order, engagement counters and sustained window. The 30 s baseline rows are scoping rows, not sustained claims; the 512 k row is a sustained (≥ 60 s, first-third vs last-third flat) row; the hero row is the user's own 40 s instrument, recorded as such.

### Reference-box records (2026-07 programs)

Headline classes on the reference dev box (zram/nullblk virtual NVMe substrate):

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
