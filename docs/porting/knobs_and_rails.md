# Porting spec — env knobs & static "rails" (squeezefs → Zig)

Scope: the env-knob registry + startup refusal gate, the shared value parser, the
source-scanning convention tests ("rails"), the per-thread sharded counters, the
must-stay-0 invariant tripwire, thread-name → CPU-class accounting, and the
derivation tie-test structure. Every claim cites `file:line` in
`/Users/joberdorf/Source/squeezefs`. Nothing here is advice; it is the contract.

---

## 1. The ONE parsing convention

Defined in `crates/squeezefs-ipc/src/env_knob_core.rs` (267 lines, pure: no logging,
no env reads, no syscalls — `env_knob_core.rs:28-32`). It is `#[path]`-included
verbatim into three other crates so they share ONE file, not a copy:
`src/lib.rs:31-32`, `crates/fuse3/src/lib.rs:111-112`,
`crates/squeezefs-preload/src/lib.rs:49-50`. The convention test pins the
bool-spelling arrays are identical across all four
(`tests/env_knob_convention_tests.rs:571-589`).

### 1.1 Absent

`present(raw: Option<&str>) -> Option<&str>` (`env_knob_core.rs:71-78`):
unset → None; otherwise `trim()`; empty after trim → None; else `Some(trimmed)`.
So `VAR=`, `VAR="   "`, `VAR="\t\n"` are all ABSENT, and `" 7 "` parses as `"7"`
(`env_knob_core.rs:178-184`). Every kind routes through `present` first.

### 1.2 Booleans

`BOOL_TRUE = ["1","true","yes","on"]`, `BOOL_FALSE = ["0","false","no","off"]`
(`env_knob_core.rs:81-83`). Match is `eq_ignore_ascii_case` (`:90-95`) — so
`YES`, `On`, `FALSE` all work. Anything else → `KnobError { want: "expected a
boolean — 1/true/yes/on or 0/false/no/off" }` (`:96-100`). `=0` DISABLES even a
default-ON knob (`:208-215`; the ENG-10 defect was 19 presence-based sites where
`FREE_FORENSICS=0` enabled the feature — module doc `:5-8`).

### 1.3 Integers

- `parse_int<T: FromStr>(key, raw)` (`:105-120`): absent → `Ok(None)`; `v.parse::<T>()`;
  failure → `want = "expected an integer ({type_name})"`. `"4 2"`, `"1e6"`, `"-1"` for
  u64 all refuse (`:228-236`).
- `parse_int_in<T>(key, raw, lo, hi)` (`:127-142`): inclusive range. Parse failure AND
  out-of-range use the SAME message: `"expected an integer in {lo}..={hi}"` (`:136`) —
  the operator sees what IS admissible, never the Rust type name. **Out-of-range
  REFUSES, never clamps** (`:122-126`; test `:239-246`: `64` against `1..=32` refuses,
  `0` refuses, boundaries `1` and `32` pass).
- The registry validates every Int as `i128` with the row's `lo..=hi`
  (`src/env_knobs.rs:508-510`).

### 1.4 Enums

`parse_enum(key, raw, allowed: &[&'static str])` (`:146-164`): case-insensitive match,
returns the CANONICAL spelling from `allowed` (so `SECOND-TOUCH` → `"second-touch"`,
`:249-259`). Miss → `want = "expected one of {allowed.join("|")}"`.

### 1.5 Strings

`parse_str(raw)` = `present(raw)` (`:169-171`). Presence is all this layer checks;
a nonexistent path is the consumer's better error (`Kind::Str` doc, `env_knobs.rs:60-62`).

### 1.6 Percentage knobs and `explicit > percentage > derived`

Percentages are registered as plain `Int` in `1..=100` (e.g.
`SQUEEZEFS_TRANSPORT_MEM_PCT`, `SQUEEZEFS_META_NODE_CACHE_PCT`, `SQUEEZEFS_IPC_MEM_PCT`,
`env_knobs.rs:154,269,280`), so the startup gate refuses `0`, `250`, `-3`, `12.5`.
The precedence is implemented per family in a pure `resolve_*` function that takes
the raw env strings and never refuses (the gate already did), e.g.
`src/mem_budget.rs:643-679` (`resolve_transport_buffer_cap(budget_bytes,
mem_max_mib: Option<&str>, mem_pct: Option<&str>)`):

1. if `mem_max_mib` present and parses as `u64` → return `mib.saturating_mul(1 MiB)`
   verbatim (explicit wins; `0` is a legal explicit value) (`:648-657`);
   junk → `log::warn!` and fall through.
2. else if `mem_pct` present and parses as finite `f64` > 0 → `> 100` is clamped
   to 100 WITH a warning; return `(budget as f64 * pct/100) as u64` (`:658-677`).
   Non-positive / NaN / junk → warn, fall through. (In-process this reader is more
   lenient than the gate — it accepts `12.5` — because the gate refuses before any
   of these run; the derivation sweep test exercises the lenient path directly,
   `tests/derivation_sweep_tests.rs:67-102`.)
3. else the derived default (`budget / 8`, `mem_budget.rs:626-628`).

The identical shape is `resolve_ipc_arena_cap` (`mem_budget.rs:718-751`) and
`resolve_node_cache_budget` (`src/meta_backend/kv/backend.rs`, tie test
`derivation_sweep_tests.rs:113-142`: absolute `512` beats pct `50`; pct `25` of
16 GiB = 4 GiB; garbage falls to the floor). The memory-budget root itself is
`flag → env → cgroup×0.8 → RAM×0.7` (`mem_budget.rs:953-969`).

### 1.7 Duration knobs

There is no duration type. Every timing knob is an `Int` in milliseconds or seconds,
named with a `_MS` / `_SECS` / `_US` suffix. Ceilings: `MS_MAX = 600_000` (10 min,
`env_knobs.rs:100`); `DAY_MS = 86_400_000` for "park the timer" idioms (`:126`);
seconds knobs cap at `86_400`. `"5s"` is a refusal (`tests/...:283`).

### 1.8 List knobs

Lists are `Kind::Str` (comma-separated); only presence is validated:
`SQUEEZEFS_TEST_POWER_CUT_DEVS`, `SQUEEZEFS_ZCRX_LANE_TARGET` ("6 comma-separated
fields; malformed = lane not armed, loudly"), `SQUEEZEFS_TEST_SYM_APPENDER_SLOTS`
(Harness, `<id>:<slot>[,<slot>…][;<id>:…]`).

### 1.9 The registry's in-process readers (`src/env_knobs.rs:600-678`)

These are what call sites use; they never panic, because the gate already refused:

| fn | absent | valid | malformed |
|---|---|---|---|
| `bool_knob(key, default) -> bool` (`:617-627`) | `default` | value | `log::error!("{e} — keeping the default ({default})")`, return `default` |
| `int_knob<T>(key, default) -> T` (`:632-645`) | `default` | value | error-log, `default` (note: uses `parse_int`, NOT range-checked here) |
| `opt_int_knob<T>(key) -> Option<T>` (`:650-661`) | `None` | `Some(v)` | error-log `"— ignoring the override"`, `None` |
| `enum_knob(key, allowed, default) -> &'static str` (`:668-678`) | `default` | canonical | error-log, `default` |

Tests: `tests/env_knob_convention_tests.rs:365-443`.

### 1.10 The startup gate: `refusal_report()` and what `main` does

`validate_vars(iter of (K,V)) -> Validation { errors: Vec<String>, unknown: Vec<String> }`
(`env_knobs.rs:539-563`): for each var, skip unless `is_ours(key)` =
`key.starts_with("SQUEEZEFS_") || key.starts_with("SQZ_")` (`:483-485`); registered →
`validate_one`; unregistered → push to `unknown`. Both vectors are `sort()`ed.
`validate_environment()` = `validate_vars(std::env::vars())` (`:566-568`).

`validate_one(knob, raw) -> Option<String>` (`:504-535`):
- Bool / Int / Enum → the core parser's `KnobError.to_string()`, which is
  `"{key}='{raw}' is invalid: {want}"` (`env_knob_core.rs:50-54`; raw is the TRIMMED
  value, quoted).
- Enum with a retired VALUE (matched `eq_ignore_ascii_case` on trimmed raw, checked
  BEFORE the allowed set): `"{key}='{raw}' was RETIRED (forward-only — never a silent
  alias): {why}"` (`:511-521`). The message must NOT contain "expected one of"
  (`tests/...:263-266`).
- `Str | BuildTime | Harness` → `None` (never invalid).
- `Retired { successor }` → only if `present(raw)` is Some:
  `"{key}='{val}' was RETIRED (forward-only — never a silent alias): use {successor}
  instead"` (`:527-533`). An EMPTY retired knob is not an error (`tests/...:230`).

`refusal_report() -> Option<String>` (`:577-599`):
1. For every `unknown` name, `eprintln!("Warning: {name} is not a SqueezeFS knob (typo?
   `src/env_knobs.rs` is the registry) — ignored")`. Announced, NEVER refused (mixed-
   version fleets and the shim's client-side knobs share the environment, `:19-24`).
2. If `errors` is empty → `None`.
3. Else build:
   ```
   refusing to start: invalid SqueezeFS environment knob(s)
     - <error 1>
     - <error 2>
     (a malformed knob is never silently defaulted — fix or unset it; empty means unset)
   ```
   (`:588-597`; every offender at once — test `:351-359` expects 3 errors for 3 bad knobs).

`main` (`src/main.rs:2784-2787`), as the FIRST thing after an optional profiler and
before argument parsing, volume open or mount:
```rust
if let Some(report) = squeezefs::env_knobs::refusal_report() {
    eprintln!("Error: {report}");
    std::process::exit(1);
}
```
So stderr carries `Error: refusing to start: ...`, exit code 1, and `squeezefs
--version` with `SQUEEZEFS_READ_LANE=yess` fails (`tests/...:614-630`), while
`SQUEEZEFS_NOT_A_KNOB=1` warns on stderr and still prints the version with exit 0
(`:641-648`).

### 1.11 The shim's documented asymmetry

The LD_PRELOAD shim (`crates/squeezefs-preload`) must never kill its host application
over an env typo, so it does NOT run a refusal gate: it calls the shared parsers
directly and on `Err` keeps its documented default while announcing
(`env_knob_core.rs:21-23`; `crates/squeezefs-preload/src/session.rs:329-341`:
`parse_bool(...).ok().flatten().unwrap_or(false)`, with a once-per-process
`libc::write(2, ...)` notice when engaged, `:342-352`). Other shim readers:
`interpose.rs:2773`, `lane_gate.rs:58,111`. Values MEAN the same thing on both sides
(same file); only loudness differs.

### 1.12 Knob-name hygiene laws (pinned by tests)

- **Census completeness**: every `"SQUEEZEFS_…"`/`"SQZ_…"` string literal in the
  scanned trees must be registered (§3).
- **Inverse census**: every non-Retired registered knob must appear as a literal
  somewhere in the tree (`tests/...:131-144`) — "no dead code".
- **Collision law**: among live knobs (not Retired/Harness/BuildTime), if `a.key` is a
  strict prefix of `b.key` then `a.kind` MUST be `Bool` (a family switch such as
  `SQUEEZEFS_NT_COPY` + `SQUEEZEFS_NT_COPY_MIN`) (`:152-178`). This is why inode
  reclaim was renamed `SQUEEZEFS_INODE_RECLAIM_*` and why
  `SQUEEZEFS_TEST_DROP_COMMIT_WAKE_TID` is singular (it would otherwise prefix
  `..._WAKES`).
- **Uniqueness**: each name registered exactly once (`:188-199`; also unit test
  `env_knobs.rs:684-691`).
- **Every row has a non-empty default string and a doc > 10 chars**
  (`env_knobs.rs:693-699`).

---

## 2. The registry schema and THE FULL TABLE

### 2.1 Schema (`src/env_knobs.rs:44-92`)

```rust
pub enum Kind {
    Bool,                                       // 1/true/yes/on | 0/false/no/off
    Int { lo: i128, hi: i128 },                 // inclusive; out-of-range refuses
    Enum { allowed: &'static [&'static str],
           retired: &'static [(&'static str, &'static str)] }, // (value, why)
    Str,                                        // presence only
    Retired { successor: &'static str },        // any present value refuses
    BuildTime,                                  // rustc env from build.rs; census only
    Harness,                                    // suites/rigs; known, unvalidated
}
pub struct Knob { key: &'static str, kind: Kind, default: &'static str, doc: &'static str }
pub static KNOBS: &[Knob];                      // 314 rows, grouped by subsystem
pub fn lookup(key) -> Option<&'static Knob>;    // linear scan (:477-479)
pub fn is_ours(key) -> bool;                    // SQUEEZEFS_ | SQZ_ prefix (:483-485)
```

`default` is documentation ("derived", "on", "4096", "-" for retired/harness), not a
parsed value — call sites carry their own typed default. Range constants used by the
table (`env_knobs.rs:96-126`): `MS_MAX=600_000`; `BYTES_MAX=1<<40`;
`DAY_MS=86_400_000`; `SYM_RING_KB_MIN = SYM_RING_FLOOR_BYTES/1024 = 512`
(`(256 KiB + 128 KiB).next_power_of_two()` = 512 KiB, `appender.rs:870-871`,
`journal.rs:163`); `SYM_RING_KB_MAX = JOURNAL_RING_MAX/1024 = 32768`
(`superblock.rs:644`); `SYM_GRANT_EXTENTS_{MIN,MAX} = 8, 65_536` (`appender.rs:1019-1023`);
`SYM_MINT_SLOTS_MAX = MINT_SPREAD = 64` (`slot_lease_core.rs:75`);
`SYM_AFFINITY_MAX_MB_MAX = BYTES_MAX>>20 = 1_048_576`; `INLINE_MAX_FLOOR=4096`,
`INLINE_MAX_CEILING = 65_536 − 4096 = 61_440` (`routing.rs:16,358-359,660`;
`node.rs:254`); `LBA_GRAIN=4096`, `CHUNK_SIZE=4 MiB` (`routing.rs:26`,
`block_allocator.rs:112`).

### 2.2 Class legend (derived from the row's kind/doc for this spec)

- **retired (refuse)** — `Kind::Retired`; any non-empty value refuses naming the successor. Port as a refusal row; do not implement behaviour.
- **test seam** — product code armed only by suites (`SQUEEZEFS_TEST_*`, or doc begins "Test seam"/"TESTING"). Must exist (the gate validates them) but default-inert.
- **harness** — `Kind::Harness`; read only by test binaries/scripts; registered so the census passes; never validated.
- **A/B lever** — a Bool (or measurement Int) whose `0`/explicit value is the same-binary control arm; the doc names the A/B control.
- **switch** — a Bool that is a real feature/posture toggle without an A/B framing.
- **derived override** — Int whose absence means "derive it" (default string contains "derived"); explicit wins verbatim.
- **tuning** — Int with a literal shipped default.
- **config** — `Kind::Str` (paths, URIs, bind addresses).
- **enum select** / **build-time** — as named.

Counts: Int 129 · Bool 85 · Harness 58 · Retired 18 · Str 14 · BuildTime 7 · Enum 3 = **314**.

### 2.3 THE TABLE (order = `KNOBS` order, `src/env_knobs.rs:132-474`)

Ranges are inclusive. `2^N` means `1 << N`. Retired rows show `→ successor`.
| # | name | kind | range / successor | default | class | purpose (first sentence of the registry doc) |
|---|------|------|-------------------|---------|-------|------------------------------------------------|
| 1 | `SQUEEZEFS_BUILD_COMMIT` | BuildTime | — | git | build-time | Build commit hash embedded by build.rs. |
| 2 | `SQUEEZEFS_BUILD_COMMIT_SHORT` | BuildTime | — | git | build-time | Short form of the build commit. |
| 3 | `SQUEEZEFS_BUILD_DIRTY` | BuildTime | — | git | build-time | 1 when the build tree carried uncommitted tracked changes. |
| 4 | `SQUEEZEFS_BUILD_TAG` | BuildTime | — | git | build-time | Exact stable-*/lts-* tag on the build commit, else empty. |
| 5 | `SQUEEZEFS_BUILD_PROFILE` | BuildTime | — | cargo | build-time | Cargo profile name the binary was built under (release = thin LTO; dist = fat LTO, tagged releases only). |
| 6 | `SQUEEZEFS_BUILD_TIMESTAMP` | BuildTime | — | git | build-time | UTC RFC3339 build timestamp (honors SOURCE_DATE_EPOCH). |
| 7 | `SQUEEZEFS_IL_BUILD_COMMIT` | BuildTime | — | git | build-time | The shim's build commit (KD-7 daemon/shim pairing). |
| 8 | `SQUEEZEFS_META_URI` | Str | — | none | config | Default sqmeta:// URI for the verbs that take one (clap env). |
| 9 | `SQUEEZEFS_HOSTNQN` | Str | — | none (/etc/nvme/hostnqn) | config | KD-MW-3 (design-full-multi-writer §5.2): per-mount NVMe-oF host NQN — every fabrics connection this daemon makes pres... |
| 10 | `SQUEEZEFS_HOSTID` | Str | — | none (/etc/nvme/hostid) | config | KD-MW-3: the paired per-mount NVMe-oF host id (see SQUEEZEFS_HOSTNQN — pair-or-neither). |
| 11 | `SQUEEZEFS_ENCRYPT_KEY_FILE` | Str | — | /etc/squeezefs/keys/<id>.key | config | Encryption key material path (docs/design-key-handling.md). |
| 12 | `SQUEEZEFS_DAEMON_PIPE` | Int | 0..=2147483647 | none | tuning | Internal: fd the forked daemon writes `ready` to. |
| 13 | `SQUEEZEFS_TIMEOUT` | Int | 1..=86400 | 30 | tuning | Metadata-operation timeout, seconds (memoized at launch). |
| 14 | `SQUEEZEFS_SUPERVISE_INTERVAL_SECS` | Int | 1..=86400 | 5 | tuning | `mount --supervise` probe interval, seconds. |
| 15 | `SQUEEZEFS_SUPERVISE_UNRESPONSIVE_SECS` | Int | 1..=86400 | 30 | tuning | `mount --supervise` unresponsiveness threshold before abort, seconds. |
| 16 | `SQUEEZEFS_MEM_BUDGET_MB` | Int | 1..=2^30 | derived | derived override | R5 memory budget, MiB (absolute > pct > cgroup/RAM derivation). |
| 17 | `SQUEEZEFS_FLEET_SHARE` | Int | 1..=4096 | 1 | tuning | KD-MW-14 (design-full-multi-writer §5.6): N co-located daemons divide the machine — ONE divisor applied at the ROOT I... |
| 18 | `SQUEEZEFS_META_NODE_CACHE_MB` | Int | 1..=2^30 | derived | derived override | Per-volume KV node-cache budget, MiB (absolute wins over _PCT). |
| 19 | `SQUEEZEFS_META_NODE_CACHE_PCT` | Int | 1..=100 | derived | derived override | Per-volume KV node-cache budget as a percentage of the R5 budget. |
| 20 | `SQUEEZEFS_META_CHECKPOINT_MAX_DIRTY_NODES` | Int | 1..=2^32 | derived | derived override | Dirty-node checkpoint cap = the mount-replay working-set bound. |
| 21 | `SQUEEZEFS_META_COMMIT_BATCH_TXS` | Int | 1..=2^20 | derived | derived override | M7 commit-conveyor batch cap, transactions. |
| 22 | `SQUEEZEFS_META_COMMIT_BATCH_BYTES` | Int | 1..=2^40 | derived | derived override | M7 commit-conveyor batch cap, bytes (clamped to the ring's admissible capacity). |
| 23 | `SQUEEZEFS_JOURNAL_LANE` | Bool | — | on | A/B lever | C-2 (e2e perf audit DLM board #3): each writable metadata volume gets its OWN journal lane — one OS thread (sqz-jrnl{... |
| 24 | `SQUEEZEFS_META_FLUSH_INTERVAL_MS` | Int | 0..=86400000 | 50 | tuning | Journal/checkpoint cadence, ms; 0 = strict per-commit. |
| 25 | `SQUEEZEFS_JOURNAL_FLUSH_INTERVAL_MS` | Int | 0..=86400000 | unset | tuning | Legacy alias for SQUEEZEFS_META_FLUSH_INTERVAL_MS (the new spelling wins). |
| 26 | `SQUEEZEFS_META_REVALIDATE_MS` | Int | 1..=86400000 | derived | derived override | Coherent-READER node-cache revalidation cadence, ms (spec §6.8 item 2). |
| 27 | `SQUEEZEFS_BLOCK_REFS_VERIFY` | Bool | — | off | switch | Run the durable-vs-derived block-reference oracle at MOUNT (spec §6.2 item 1). |
| 28 | `SQUEEZEFS_DEFAULT_BLOCK_SIZE` | Int | 4096..=2^40 | 4194304 | tuning | Default striped block size, bytes, when format did not record one. |
| 29 | `SQUEEZEFS_INLINE_MAX_BYTES` | Int | 4096..=61440 | 4096 | A/B lever | The inline-layout ceiling, bytes: a file up to it keeps its whole payload IN its layout record on the metadata volume... |
| 30 | `SQUEEZEFS_SMALL_FILE_PACKING` | Bool | — | on | A/B lever | Small-file PACKING (docs/design-small-file-packing.md, PR PK2): a staged-layout file whose stored image's slot fits S... |
| 31 | `SQUEEZEFS_PACK_MAX_SLOT_BYTES` | Int | 4096..=4194304 | derived CHUNK_SIZE/2 (2 MiB) | derived override | The packer's OWN-BLOCK threshold, bytes (design-small-file-packing §5.5, KD-5): a stored image whose LBA-rounded slot... |
| 32 | `SQUEEZEFS_PUBLISH_COALESCE_MAX` | Int | 0..=2^20 | 64 | A/B lever | Per-ino publish-coalescing window; 1 = the pre-campaign serialized posture (A/B lever). |
| 33 | `SQUEEZEFS_LAYOUT_DELTA_MAX_CHAIN` | Int | 0..=2^20 | 64 | A/B lever | Layout delta-record chain cap before a full re-base save; 0 = full saves only (A/B lever). |
| 34 | `SQUEEZEFS_PUBLISH_COMMIT_GROUP_MAX` | Int | 0..=2^20 | 0 (derives from META_COMMIT_BATCH_TXS) | A/B lever | Layout saves aggregated into one multi-ino KvTx per conveyor window; 1 = per-save commits (A/B lever). |
| 35 | `SQUEEZEFS_KVMAP` | Bool | — | on | A/B lever | PB-class block-map tree (design-kvmap-block-map-tree, PR 2): whether NEW beyond-inline crossings take the tree-7 kvma... |
| 36 | `SQUEEZEFS_KVMAP_OVERLAY` | Bool | — | on | switch | PB-class bounded maps (design-kvmap-block-map-tree §14, PR 6c-i): whether kvmap inos whose size-estimated map exceeds... |
| 37 | `SQUEEZEFS_MAP_MIGRATE_CHUNK` | Int | 64..=1024 | 512 | tuning | kvmap crossing-train ops per transaction (the finding-38 BLOCK_REF_TX_CHUNK law). |
| 38 | `SQUEEZEFS_PATCH_MAX_BYTES` | Int | 0..=2^40 | derived block_size/8 | derived override | W1 sole-owner in-place patch ceiling, bytes; 0 = the acceptance A/B lever. |
| 39 | `SQUEEZEFS_FOLD_MAX_EXTENTS` | Int | 0..=2^24 | 64 | tuning | W2 fold trigger: parked extents per block. |
| 40 | `SQUEEZEFS_FOLD_MAX_BYTES` | Int | 0..=2^40 | derived block_size/4 | derived override | W2 fold trigger: parked bytes per block. |
| 41 | `SQUEEZEFS_PARKED_BUFFERS` | Int | 0..=2^24 | derived | derived override | W2 parked-write budget in buffers' worth of bytes (× block size). |
| 42 | `SQUEEZEFS_PARKED_GATE_ASSIST_MS` | Int | 0..=600000 | 200 | tuning | R5-Red parked-gate self-flush assist window, ms. |
| 43 | `SQUEEZEFS_INPLACE_OVERWRITE` | Bool | — | off | switch | Opt a substrate into in-place eligible full-block overwrites (real-SSD DSM fleets; measured loss on zram-lz4). |
| 44 | `SQUEEZEFS_DEVICE_OVERLAY` | Bool | — | on | A/B lever | Approach B device-backed visible overlay (design-device-overlay, PR B2; one-path write store): eligible fresh/hole al... |
| 45 | `SQUEEZEFS_OVERLAY_OVERWRITE` | Bool | — | on | A/B lever | The overlay OVERWRITE arm (design-overlay-overwrite, PR B4): aligned single-block overwrites of mapped striped passth... |
| 46 | `SQUEEZEFS_OVERLAY_CLOSE_BARRIER` | Bool | — | off | switch | OQ-5 (design-overlay-overwrite): barrier the data devices (flush_data_devices) BEFORE the coverage-triggered close of... |
| 47 | `SQUEEZEFS_REWRITE_SHADOW` | Bool | — | on | A/B lever | Rewrite-program shadow swaps; 0 = the A/B control. |
| 48 | `SQUEEZEFS_REWRITE_SUPPLY_CLOSE` | Retired | → (nothing — a rewrite epoch closes on its routine KD-1.6/1... | - | retired (refuse) | RETIRED at the symmetric default flip (PR 14, forward-only): the supply-coupled rewrite-epoch close on a laned CO-WRI... |
| 49 | `SQUEEZEFS_WRITEBACK_QUEUE_CAP` | Int | 1..=2^24 | 4096 | tuning | Writeback flush-unit queue capacity. |
| 50 | `SQUEEZEFS_WRITE_PIPELINE_DEPTH_BLOCKS` | Int | 0..=2^20 | derived (BDP) | derived override | Write-pipeline depth target override, blocks — MEASUREMENT lever; the default is the runtime BDP derivation. |
| 51 | `SQUEEZEFS_RECLAIM_BATCH_BLOCKS` | Int | 1..=1024 | 64 | tuning | Background block-reclaim drain batch, blocks. |
| 52 | `SQUEEZEFS_RECLAIM_BATCH_MS` | Int | 0..=600000 | 2 | tuning | Background block-reclaim batch-accumulation window, ms. |
| 53 | `SQUEEZEFS_RECLAIM_QUEUE_MAX_BLOCKS` | Int | 1..=2^20 | derived (floor 4096) | derived override | Block-reclaim deferred-space cap, blocks (queued + in-flight); at-cap enqueues park (park-don't-spill). |
| 54 | `SQUEEZEFS_RECLAIM_LANES_PER_DEV` | Int | 1..=64 | 32 | tuning | Parallel reclaim drain lanes per device. |
| 55 | `SQUEEZEFS_ALLOC_LANE_RESERVE_BLOCKS` | Retired | → (nothing — allocation under the symmetric plane is a rang... | - | retired (refuse) | RETIRED at the symmetric default flip (PR 14, forward-only): the per-lane durable reservation grain of the S9 data-pl... |
| 56 | `SQUEEZEFS_RECLAIM_CAP_PARK_MS` | Int | 0..=60000 | derived (50..1000) | derived override | At-cap reclaim enqueue park bound, ms, before soft overflow — the safety bound only: the park ends on the drain's roo... |
| 57 | `SQUEEZEFS_DISCARD_ELISION` | Bool | — | on | A/B lever | Elide discards for blocks a rewrite is about to overwrite; 0 = the A/B control. |
| 58 | `SQUEEZEFS_INODE_RECLAIM_BATCH` | Int | 1..=1024 | 64 | tuning | Deferred inode-reclaim gather batch, inodes. |
| 59 | `SQUEEZEFS_INODE_RECLAIM_WINDOW_MS` | Int | 0..=1000 | 20 | tuning | Deferred inode-reclaim gather window, ms. |
| 60 | `SQUEEZEFS_INODE_RECLAIM_CONCURRENCY` | Int | 1..=2^16 | max(4, cpus) | tuning | Concurrent inode-reclaim workers. |
| 61 | `SQUEEZEFS_RECLAIM_BATCH` | Retired | → SQUEEZEFS_INODE_RECLAIM_BATCH | - | retired (refuse) | Retired (ENG-10): collided with the SQUEEZEFS_RECLAIM_BATCH_BLOCKS block-reclaim family. |
| 62 | `SQUEEZEFS_RECLAIM_BATCH_WINDOW_MS` | Retired | → SQUEEZEFS_INODE_RECLAIM_WINDOW_MS | - | retired (refuse) | Retired (ENG-10): inode reclaim, renamed out of the block-reclaim prefix. |
| 63 | `SQUEEZEFS_RECLAIM_CONCURRENCY` | Retired | → SQUEEZEFS_INODE_RECLAIM_CONCURRENCY | - | retired (refuse) | Retired (ENG-10): inode reclaim, renamed out of the block-reclaim prefix. |
| 64 | `SQUEEZEFS_READ_HOT_BLOCK_CACHE_MB` | Int | 0..=2^30 | derived | derived override | R4 hot-block RAM tier budget, MiB. |
| 65 | `SQUEEZEFS_READ_ADMISSION_FILL_PCT` | Int | 0..=100 | 5 | tuning | Scan-resistant admission governor's bounded-waste share, percent of device read bytes. |
| 66 | `SQUEEZEFS_READ_TIER_ADMISSION` | Enum | always,second-touch,never | second-touch | enum select | R1b tier-admission policy. |
| 67 | `SQUEEZEFS_READ_PREFETCH_WINDOW` | Int | 0..=2^20 | derived | derived override | R2 prefetch window override, blocks. |
| 68 | `SQUEEZEFS_READ_PREFETCH_SHARE_PCT` | Int | 1..=100 | 50 | tuning | R2 prefetch share of the read budget, percent. |
| 69 | `SQUEEZEFS_READ_RANGED_THRESHOLD` | Int | 0..=2^40 | 262144 | tuning | R3 ranged-read threshold, bytes. |
| 70 | `SQUEEZEFS_READ_DEST_LEASE` | Bool | — | on | A/B lever | READ dest-window lease (copy-elimination phase 1): cold aligned sub-block windows DMA device bytes straight into the ... |
| 71 | `SQUEEZEFS_READ_LANE` | Bool | — | on | A/B lever | R1b read-lane hold (the shipped default win); 0 = the A0 control. |
| 72 | `SQUEEZEFS_READ_LANE_DEPTH` | Int | 0..=2^16 | derived (engage-governor) | derived override | Read-ahead lane depth pin, blocks — unset = probe-governed (probe-adopt-retreat); 0 = ahead-issue off (the hold-only ... |
| 73 | `SQUEEZEFS_NVME_READ_LANES` | Int | 1..=1024 | derived (cpus / data devices) | derived override | Per-device READ submission fan-out lanes (read-queue-wall campaign); 1 = single-worker A/B posture. |
| 74 | `SQUEEZEFS_NVME_WRITE_LANES` | Int | 1..=1024 | derived (cpus / data devices) | derived override | Per-device DATA-WRITE submission fan-out lanes, block-offset affinity (write-lane-fanout campaign); 1 = single-worker... |
| 75 | `SQUEEZEFS_DIRECT_DEVICE_TRUE` | Bool | — | off | switch | Strict device-true O_DIRECT (the amplification-measurement escape); default serves O_DIRECT like buffered. |
| 76 | `SQUEEZEFS_WRITE_SHARED` | Bool | — | on | A/B lever | Shared-mode write admission (design-write-inode-convoy): fully-mapped within-EOF striped overwrites take the inode RE... |
| 77 | `SQUEEZEFS_WRITE_GUARD_NARROW` | Bool | — | on | A/B lever | W-2 write-stream-guard (2026-09-05): the Shared class widens to EVERY cache-resident striped write — the fresh/append... |
| 78 | `SQUEEZEFS_NT_COPY` | Bool | — | on | A/B lever | Non-temporal stores at the two DMA-destined copy sites; 0 = the A/B control. |
| 79 | `SQUEEZEFS_NT_COPY_MIN` | Int | 0..=2^40 | 262144 | tuning | NT-store engagement floor, bytes. |
| 80 | `SQUEEZEFS_NT_READ_SERVE` | Bool | — | on | A/B lever | Non-temporal stores on dest-arm read serves; 0 = the A/B control. |
| 81 | `SQUEEZEFS_NT_READ_SERVE_MIN` | Int | 0..=2^40 | 262144 | tuning | Read-serve NT-store floor, bytes (keeps rand-4k/warm-small serves cached). |
| 82 | `SQUEEZEFS_NUMA` | Bool | — | on | A/B lever | NUMA placement actions (structurally inert on single-node hosts); 0 = the A/B lever, instrument stays live. |
| 83 | `SQUEEZEFS_READ_MOSTLY_CACHE` | Bool | — | on | A/B lever | L3 coherence campaign (2026-08-08): read-mostly backing for the hot process-global caches (metadata/stream-lanes/attr... |
| 84 | `SQUEEZEFS_CACHE_TOUCH_SECS` | Int | 0..=86400 | derived (TTI/4) | derived override | L3 mech-2: the TTI-cache policy-touch sampling horizon, seconds — an entry read at least once per horizon keeps acces... |
| 85 | `SQUEEZEFS_FUSE_MAX_WRITE` | Int | 4096..=2^40 | derived | derived override | Negotiated FUSE max_write, bytes. |
| 86 | `SQUEEZEFS_FUSE_OVER_IO_URING_QUEUES` | Int | 1..=512 | kernel possible CPUs | tuning | FUSE-over-io_uring queue count (TESTING only — fewer than possible CPUs never becomes ready). |
| 87 | `SQUEEZEFS_FUSE_OVER_IO_URING_Q_DEPTH` | Int | 1..=32 | derived (desired 32) | derived override | Entries per FUSE-over-io_uring queue; explicit value wins verbatim. |
| 88 | `SQUEEZEFS_FUSE_IO_URING_ENTRIES` | Int | 64..=4096 | 1024 | tuning | SQ depth for the classical FUSE rings (INIT/notify). |
| 89 | `SQUEEZEFS_FUSE_IO_URING_SQPOLL_IDLE_MS` | Int | 0..=600000 | off | tuning | Enable SQPOLL on the transport rings with this idle timeout, ms (measured NOT recommended). |
| 90 | `SQUEEZEFS_FUSE_IO_URING_SQPOLL_CPU` | Int | 0..=4095 | unpinned | tuning | Pin the SQPOLL kernel thread to this CPU. |
| 91 | `SQUEEZEFS_FUSE_IO_URING_SPIN_US` | Int | 0..=100000 | off (0) | A/B lever | Spin-before-park CAP for the FUSE-over-io_uring queue workers, us (e2e perf audit R-4, reap-thread economy; .benchmar... |
| 92 | `SQUEEZEFS_FUSE_PIN_SCOPE` | Enum | node,core | node | enum select | Transport thread affinity scope; `core` is the pre-campaign hard-pin posture. |
| 93 | `SQUEEZEFS_FUSE_SAME_LANE_DISPATCH` | Bool | — | 1 | A/B lever | READ handler futures spawn_local on the dispatching TPC lane instead of round-robining to another lane (transport-ing... |
| 94 | `SQUEEZEFS_FUSE_READ_FAST_DISPATCH` | Bool | — | on | A/B lever | READ fast-dispatch from the reap thread (e2e perf audit R-2, read board #2): the FUSE-over-io_uring queue worker runs... |
| 95 | `SQUEEZEFS_FUSE_DRAIN_GROUP` | Int | 1..=512 | derived (node possible CPUs / 4, floor 1) | derived override | Queues per FUSE-over-io_uring drain context (ingress-queue-spread lever 2); explicit width wins verbatim, `1` = the p... |
| 96 | `SQUEEZEFS_FUSE_KMBUF` | Bool | — | on | A/B lever | kmbuf reply-buffer negotiation; 0 = the A/B control. |
| 97 | `SQUEEZEFS_FUSE_ZC` | Bool | — | on | A/B lever | FUSE_URING_ZERO_COPY serve integration (K1 kill; sqz kernel + CAP_SYS_ADMIN required — declines loud elsewhere, stock... |
| 98 | `SQUEEZEFS_FUSE_ZC_WRITE_FUSION` | Bool | — | on | A/B lever | Handler/worker FUSION for small armed FUSE WRITEs. |
| 99 | `SQUEEZEFS_FUSE_ZC_READ_FUSION` | Bool | — | off | A/B lever | Handler/worker FUSION for armed FUSE READs (e2e audit R-3, read board #3's zc-leg form): arm (b) of the composed READ... |
| 100 | `SQUEEZEFS_READ_ZC_SERVE` | Bool | — | on | A/B lever | The fd-SOURCE zc READ serve (e2e perf audit R-4, read board #4/#5, .benchmarks/2026-09-05-r4-read-zc-serve.md). |
| 101 | `SQUEEZEFS_FUSE_ZC_FUSION_MAX` | Int | 4096..=2^30 | derived (payload/8) | derived override | Fused-dispatch payload ceiling, bytes — bounds the handler work the queue worker's drain loop runs inline (the write-... |
| 102 | `SQUEEZEFS_FUSE_PLACED_MERGE` | Retired | → (deleted — FUSE placed-merge was falsified; IL placed_sev... | - | retired (refuse) | Retired (class-1 delete, one-path P1): FUSE placed-merge was falsified at ~25% cohort capture (.benchmarks/2026-08-09... |
| 103 | `SQUEEZEFS_FUSE_ZC_RETENTION` | Bool | — | on | A/B lever | zc payload-retention ARMING (kernel 0029, design-zc-write-kernel-v2 §6.1): where the RELEASE_PAYLOAD opcode probes Pr... |
| 104 | `SQUEEZEFS_ZC_ACK_EARLY` | Bool | — | on | A/B lever | ACK-early for device-overlay stores: eligible overlay stores reply while the device DMA runs. |
| 105 | `SQUEEZEFS_OVERLAY_DEPTH_GOVERNOR` | Bool | — | on | A/B lever | W-3 (e2e perf audit write board #3): device-overlay stores admit their in-flight segment bytes through the write-pipe... |
| 106 | `SQUEEZEFS_GAP_SEED_RANGED` | Bool | — | on | A/B lever | W-6 (e2e perf audit write board #10): the device overlay's settle sources a PARTIAL overwrite record's uncovered rang... |
| 107 | `SQUEEZEFS_FSYNC_TOUCHED_NAMESPACES` | Bool | — | on | A/B lever | W-5 (e2e perf audit write board #9 / §5.3 row 15, `.benchmarks/2026-09-08-w5-fsync-economy.md`): an fsync barriers on... |
| 108 | `SQUEEZEFS_FSYNC_PARALLEL_LEGS` | Bool | — | on | A/B lever | W-5 (e2e perf audit row 15): the fsync ladder's INDEPENDENT waits run concurrently and are all awaited — the staged-p... |
| 109 | `SQUEEZEFS_FSYNC_PROMOTE_STAGED` | Bool | — | off | A/B lever | The fsync(2) of a STAGED-layout file (inline ceiling < size <= block) also promotes it to the shared backend through ... |
| 110 | `SQUEEZEFS_ZC_ACK_EARLY_ODIRECT` | Bool | — | off | switch | Lets a HELD O_DIRECT/GUP overlay slot ACK early by snapshotting (extract) before the reply — bytes sampled at ACK, so... |
| 111 | `SQUEEZEFS_ZC_BRIDGE_TIMEOUT_MS` | Int | 100..=600000 | 30000 | tuning | zc bridge-op deadline, ms (the bounded-outcome law): past it the worker pushes AsyncCancel and the op's CQE resolves ... |
| 112 | `SQUEEZEFS_TEST_ZC_DROP_WRITE_CQES` | Int | 0..=1000000 | 0 | test seam | Test seam: the zc worker consumes-and-drops the first N WRITE-class bridge CQEs (pend + deadline stay live) — the det... |
| 113 | `SQUEEZEFS_TEST_DROP_COMMIT_WAKES` | Int | 0..=1000000 | 0 | test seam | Test seam: the reply path skips the first N eventfd wake writes after arming the coalescer — the commit message stays... |
| 114 | `SQUEEZEFS_TEST_ALLOC_ARM_HOLD_MS` | Int | 0..=600000 | 0 | test seam | Test seam: the symmetric allocation lease's arm parks this many ms between its hold and the direction-judged re-snaps... |
| 115 | `SQUEEZEFS_TEST_ALLOC_ARM_VERDICT_HOLD_MS` | Int | 0..=600000 | 0 | test seam | Test seam: the symmetric allocation lease's arm parks this many ms between its second snapshot and its verdict — the ... |
| 116 | `SQUEEZEFS_TEST_DROP_COMMIT_WAKE_TID` | Int | 1..=4294967295 | unset | test seam | Test seam, the scoped form of SQUEEZEFS_TEST_DROP_COMMIT_WAKES: its budget strikes only replies to requests the kerne... |
| 117 | `SQUEEZEFS_TEST_PACK_COMMIT_STALL_MS` | Int | 0..=600000 | 0 | test seam | Test seam: a PACKED staged promotion (design-small-file-packing §5.2) parks this many ms between its slot DMA and its... |
| 118 | `SQUEEZEFS_FUSE_NO_KILLPRIV` | Bool | — | off | test seam | TESTING escape: refuse FUSE_HANDLE_KILLPRIV_V2 and restore the kernel's per-write GETXATTR probe. |
| 119 | `SQUEEZEFS_FUSE_ATTR_TTL_MS` | Int | 0..=600000 | 1000 | tuning | Kernel attribute-cache TTL, ms (-o attr_timeout overrides). |
| 120 | `SQUEEZEFS_FUSE_ENTRY_TTL_MS` | Int | 0..=600000 | 1000 | tuning | Kernel dentry TTL, ms (-o entry_timeout overrides). |
| 121 | `SQUEEZEFS_FUSE_DIR_ENTRY_TTL_MS` | Int | 0..=600000 | 1000 | tuning | Kernel directory-entry TTL, ms (-o dir_entry_timeout overrides). |
| 122 | `SQUEEZEFS_FUSE_NEGATIVE_TTL_MS` | Int | 0..=600000 | 1000 | tuning | Kernel negative-entry TTL, ms (-o negative_timeout overrides). |
| 123 | `SQUEEZEFS_TRANSPORT_MEM_MAX` | Int | 1..=2^30 | derived | derived override | Transport payload-buffer cap, MiB (absolute > pct > derived). |
| 124 | `SQUEEZEFS_TRANSPORT_MEM_PCT` | Int | 1..=100 | derived | derived override | Transport payload-buffer cap as a percentage of the R5 budget. |
| 125 | `SQUEEZEFS_TRANSPORT_DEBUG` | Bool | — | off | switch | Verbose transport tracing (development). |
| 126 | `SQUEEZEFS_IPC` | Bool | — | off | switch | Env half of `-o interception` (arms the session host). |
| 127 | `SQUEEZEFS_IPC_ALLOW_DEV` | Bool | — | off | switch | Admit degenerate (`unknown`/`-dirty`) build identities past the KD-7 skew gate — DEV ONLY, announced loudly on both e... |
| 128 | `SQUEEZEFS_IPC_ARENA_MB` | Int | 1..=2^20 | derived | derived override | Per-session payload arena, MiB. |
| 129 | `SQUEEZEFS_IPC_ARENA_THP` | Bool | — | on | switch | PMD-align + MADV_COLLAPSE session arenas; 0 disables. |
| 130 | `SQUEEZEFS_IPC_IDLE_SECS` | Int | 0..=86400 | 300 | tuning | Idle-session reap window, seconds; 0 disables. |
| 131 | `SQUEEZEFS_IPC_INVAL_WINDOW_MS` | Int | 1..=600000 | 1000 | tuning | W1 invalidation coalescing window, ms. |
| 132 | `SQUEEZEFS_IPC_MAX_OP_BYTES` | Int | 1..=2^40 | layout default | tuning | Per-op payload ceiling, bytes. |
| 133 | `SQUEEZEFS_IPC_MEM_MAX` | Int | 1..=2^30 | derived | derived override | Session-arena admission cap, MiB (absolute > pct > derived). |
| 134 | `SQUEEZEFS_IPC_MEM_PCT` | Int | 1..=100 | derived | derived override | Session-arena admission cap as a percentage of the R5 budget. |
| 135 | `SQUEEZEFS_IPC_SERVICE_THREADS` | Int | 1..=2^12 | derived clamp(3*cpus/8,2,64) | derived override | Service-thread ceiling (threads spawn on session admission; the drain-lane pair's submitter half — same derivation as... |
| 136 | `SQUEEZEFS_IPC_DD_SHARDS` | Int | 1..=2^12 | derived clamp(3*cpus/8,2,64) | derived override | Direct-drive uring shards (one ring + pinned reaper per shard, one lane per service thread; override/measurement lever). |
| 137 | `SQUEEZEFS_IPC_DD_EAGER_FLUSH` | Int | 0..=2^12 | governed (CadenceCore) | A/B lever | Direct-drive issue cadence (fourth adjudication, write-wall Addendum 8): absent = the CadenceCore-GOVERNED threshold ... |
| 138 | `SQUEEZEFS_IPC_DD_INLINE_REAP` | Bool | — | on | A/B lever | Reaper/drain fusion: the owning service thread drains its lane's direct-drive CQ inline (zero syscall); 0 = the A/B l... |
| 139 | `SQUEEZEFS_IPC_DD_PACKED` | Bool | — | on | A/B lever | Direct-drive READ prelude, the PACKED-tenant arm (PK8, design-small-file-packing §5.10): a promoted small file's size... |
| 140 | `SQUEEZEFS_IPC_SOCKET_DIR` | Str | — | derived (XDG_RUNTIME_DIR / /run) | config | Rendezvous socket directory; `none` disables the filesystem-path socket. |
| 141 | `SQUEEZEFS_IPC_SPIN_US` | Int | 0..=100000 | 0 (absent; =1 on SQUEEZEFS_IPC_SPIN_ADAPTIVE routes absent to the governor) | tuning | Service-thread empty-pass spin window, us. |
| 142 | `SQUEEZEFS_IPC_SPIN_ADAPTIVE` | Bool | — | on | A/B lever | The spin governor arm — DEFAULT ON per the efficiency doctrine (user ruling 2026-08-14: same work for less effort is ... |
| 143 | `SQUEEZEFS_IL_SESSIONS` | Int | 1..=2^12 | derived clamp(cpus/4,2,16) | derived override | Per-mount fd-shard session count (override lever; the default ties to the daemon's ceiling). |
| 144 | `SQUEEZEFS_IL_SPINS` | Int | 0..=2^24 | adaptive | tuning | Fixed completion spin count (pins the adaptive spin for measurement). |
| 145 | `SQUEEZEFS_IL_MAX_RUN_SLOTS` | Int | 0..=2^16 | 0 (unbounded) | tuning | Cap on concurrently claimed slots per session. |
| 146 | `SQUEEZEFS_IL_OP_TIMEOUT_MS` | Int | 1..=600000 | bounded default | tuning | Per-op bounded wait against a stalled daemon, ms. |
| 147 | `SQUEEZEFS_IPC_DD_LANE_FLUSH` | Bool | — | on | A/B lever | Direct-drive flush scope: on = a service thread enters only its OWN lane's ring (the r3 drain-funnel fix — flush-all ... |
| 148 | `SQUEEZEFS_IPC_DD_COOP_TASKRUN` | Bool | — | on | A/B lever | Direct-drive ring COOP_TASKRUN (r3): on = completion task-work runs only at a ring entry by the submitting task — a C... |
| 149 | `SQUEEZEFS_IL_REAP_PARK_MAX` | Int | 0..=2^16 | 24 | tuning | libaio reap: queue depth at or below which the event-driven park is used (24 since the 2026-08-13 fleet-residue recou... |
| 150 | `SQUEEZEFS_IL_SPARSE_BATCH_MARKS` | Bool | — | on | A/B lever | Sparse-arm batch park marks (design-il-wake-economy OQ2): the event park's per-session mark derives from its pending ... |
| 151 | `SQUEEZEFS_IPC_CQE_WAKE_LATCH` | Bool | — | on | A/B lever | Completion-doorbell wake-collapse latch (design-il-wake-economy L1): on = a park era pays at most ONE cqe FUTEX_WAKE ... |
| 152 | `SQUEEZEFS_IL_REAP_QUANTUM_US` | Int | 1..=1000 | 50 | A/B lever | libaio reap: deep-regime quantum, µs — since reap-fanin 2026-08-08 the batch-threshold park's AGE BOUND (the k-th com... |
| 153 | `SQUEEZEFS_IL_READ_DEST` | Bool | — | on | A/B lever | Arena-destination read serves (E-IL2); 0 = the A/B lever. |
| 154 | `SQUEEZEFS_IL_DIRECT_WRITE` | Bool | — | on | A/B lever | IL direct-drive WRITE lane (design-il-direct-write §3): W1-patch-shaped ring writes DMA in place on the svc thread's ... |
| 155 | `SQUEEZEFS_IL_KERNEL_LANE_MIN` | Int | 0..=2^40 | derived (memBW probe x lane-RTT delta, clamp [slab, max_op]) | derived override | Hybrid lane gate (D14 corollary): ops STRICTLY larger than this ride the kernel FUSE lane, smaller ops the IPC ring; ... |
| 156 | `SQUEEZEFS_ZCRX_LANE` | Bool | — | off | switch | Arm the zcrx read lane. |
| 157 | `SQUEEZEFS_ZCRX_LANE_FORCE_COPY` | Bool | — | off | test seam | Test seam: allow the classic-recv backend to arm (copy parity). |
| 158 | `SQUEEZEFS_ZCRX_LANE_AREA_SIM` | Bool | — | off | test seam | Test seam: AREA backend over simulated NIC DMA. |
| 159 | `SQUEEZEFS_ZCRX_LANE_SIM_CHUNK` | Int | 1..=2^40 | derived | derived override | Simulated area chunk size, bytes (rounded up to a power of two). |
| 160 | `SQUEEZEFS_ZCRX_LANE_TARGET` | Str | — | none | config | Lane target tuple (6 comma-separated fields); malformed = lane not armed, loudly. |
| 161 | `SQUEEZEFS_OP_PROFILE` | Bool | — | off | switch | Per-op phase histograms + the under-i_rwsem estimator (zero cost off). |
| 162 | `SQUEEZEFS_OP_TRACE` | Bool | — | off | switch | e2e audit A2: arm the per-op TRACE RING at mount — one CLOCK_MONOTONIC stamp per phase boundary per sampled op (op id... |
| 163 | `SQUEEZEFS_FREE_FORENSICS` | Bool | — | off | switch | Capture a backtrace per block free to attribute double frees (expensive). |
| 164 | `SQUEEZEFS_STATS_KEY_CENSUS` | Bool | — | off | switch | VAL-7a: arm the `.stats` key census (live object keys + per-inode write custody — a debugging surface). |
| 165 | `SQZ_CLW_RTT_ENDPOINT` | Harness | — | loopback | harness | RTT instrument: dial a REAL coordinator instead of a local loopback listener (tests/cluster_wire_tests.rs `rtt_row`). |
| 166 | `SQZ_CLW_RTT_SECRET_HEX` | Harness | — | none | harness | RTT instrument: the target volume set's job:enroll secret, hex (required for a remote row). |
| 167 | `SQZ_CLW_RTT_SAMPLES` | Harness | — | 2000 | harness | RTT instrument: counted samples after the discarded warm-up. |
| 168 | `SQZ_CLW_RTT_PAYLOAD` | Harness | — | 0 | harness | RTT instrument: payload bytes per direction. |
| 169 | `SQZ_STRIPE_WAVE_HOLDERS` | Harness | — | 7 | harness | PR 7b creator-wave row (tests/sym_dir_stripe_tests.rs `a_creator_wave_spreads_over_the_stripe_holders`): declared str... |
| 170 | `SQZ_STRIPE_WAVE_CREATES` | Harness | — | 2000 | harness | PR 7b creator-wave row: creates into the striped directory (the 12,500-creator wave's local stand-in). |
| 171 | `SQZ_STRIPE_WAVE_NODE_KIB` | Harness | — | 64 | harness | PR 7b creator-wave row: the harness volume's node size in KiB (256 admits 31 declared holders per directory extent). |
| 172 | `SQZ_STRIPE_FLIP_LAW_SHAPE` | Harness | — | - | harness | PR 7b flip lock-law pin (tests/sym_dir_stripe_tests.rs `flip_lock_law_child_entry`): the child-process marker and its... |
| 173 | `SQZ_SYM_HANG_FACTOR` | Harness | — | 4 | harness | The sym-forest matrix runner's per-suite watchdog (tests/run_sym_forest_suites.sh; PR 10 review round 3, Issue 30): a... |
| 174 | `SQZ_SYM_HANG_FLOOR_S` | Harness | — | 600 | harness | The sym-forest matrix runner's per-suite watchdog floor in seconds (the smallest bound any suite gets, whatever its f... |
| 175 | `SQZ_STORM_THREADS` | Harness | — | 32 | harness | D-3 in-process metadata storm (tests/dlm_stripe_storm_tests.rs): concurrent committers — the in-flight population the... |
| 176 | `SQZ_STORM_PER_THREAD` | Harness | — | 512 release / 64 debug | harness | D-3 in-process metadata storm: names per committer per phase. |
| 177 | `SQUEEZEFS_CLUSTER_WIRE_SVC_THREADS` | Int | 1..=2^12 | derived clamp(cpus/8, 1, 8) | derived override | Owner-side RPC lanes (pinned service threads — §6.7's venue rule: never the conveyor's task). |
| 178 | `SQUEEZEFS_MULTI_WRITER` | Retired | → (nothing — every RW mount of a symmetric-forest set is a ... | - | retired (refuse) | RETIRED at the symmetric default flip (PR 14, forward-only): the S7/S9 multi-writer opt-in armed the device-enforced ... |
| 179 | `SQUEEZEFS_MW_ROLE` | Retired | → (nothing — there are no roles under the symmetric plane: ... | - | retired (refuse) | RETIRED at the symmetric default flip (PR 14, forward-only): the authority / co-writer / set-authority / partial-auth... |
| 180 | `SQUEEZEFS_MW_AUTHORITY` | Retired | → (nothing — there is no set authority to dial: a foreign o... | - | retired (refuse) | RETIRED at the symmetric default flip (PR 14, forward-only): the co-writer's and the S5/PR-5 reader's declaration of ... |
| 181 | `SQUEEZEFS_MW_MEMBERS` | Retired | → (nothing — no roster: a writer is enrolled by JOINING, it... | - | retired (refuse) | RETIRED at the symmetric default flip (PR 14, forward-only): the S9 authority's operator-declared co-writer roster. |
| 182 | `SQUEEZEFS_MW_BIND` | Str | — | auto | config | DLM S9: where this mount serves the write-custody and publish authority — `auto` (0.0.0.0:0, ruling D2's posture; the... |
| 183 | `SQUEEZEFS_META_SHIP_BATCH_MAX` | Int | 1..=4096 | derived clamp(cpus x 2, 64, 4096) | derived override | Verbs per shipped metadata frame — the S8 pipelining unit. |
| 184 | `SQUEEZEFS_META_SHIP_DEDUP_MAX` | Int | 1..=2^24 | derived max(batch_max x 128, 8192) | derived override | Owner-side idempotency window entries — how far back a client's retry may reach and still be answered from its origin... |
| 185 | `SQUEEZEFS_META_SHIP_INLINE_SERVE` | Bool | — | off | A/B lever | D-5 (e2e perf audit DLM board #7): the OWNER-side dispatch venue on both shipped planes (the S8 verb frame; the S9 pu... |
| 186 | `SQUEEZEFS_PUBLISH_CONVEYOR_GROUP` | Bool | — | on | A/B lever | DLM S9 publish plane, OWNER side (E2E perf audit D-1c — one conveyor group per shipped frame): a served frame's indep... |
| 187 | `SQUEEZEFS_PUBLISH_SHIP_DEPTH` | Int | 1..=64 | derived clamp(ceil(cpus/8), 2, 8) | derived override | DLM S9 publish plane (E2E perf audit D-1b): publish FRAMES one co-writer keeps in flight per authority — one authenti... |
| 188 | `SQUEEZEFS_PUBLISH_SHIP_MULTIPLEX` | Bool | — | off | switch | D-5 (e2e perf audit DLM #8's single-connection half): the co-writer's in-flight publish frames toward one authority r... |
| 189 | `SQUEEZEFS_DLM_STRIPES` | Int | 1..=2^24 | derived next_pow2(max(shipped, 16 × possible_cpus × q_depth)) | derived override | D-3 (e2e perf audit DLM board #4): the stripe population of every DLM-class lock table — the 4a DlmLockManager inode ... |
| 190 | `SQUEEZEFS_DLM_TOKEN_CACHE_MAX` | Int | 1..=2^24 | derived max(R5 budget/8192/32 B, 4096) | derived override | Client fencing-token cache entries (S8's resolution of the S4 fencing-read contract; spec §6.5 item 1 requires >= 99.... |
| 191 | `SQUEEZEFS_DLM_RECALL_BATCH_MAX` | Int | 1..=32768 | derived CONTROL frame cap/2/32 B = 16384 | derived override | S10 recall lane (design-full-multi-writer §8, spec R5/R6): recall entries per batched per-client frame — MEASUREMENT ... |
| 192 | `SQUEEZEFS_DLM_RECALL_DEADLINE_MS` | Int | 1..=600000 | derived 4x live p99 (rtt+owner), clamp [1 ms, lease TTL] | derived override | S10 recall lane: per-frame ack deadline, ms — MEASUREMENT lever; the default derives from the LIVE meta_ship_phase_ns... |
| 193 | `SQUEEZEFS_DLM_RECALL_COOLDOWN_MS` | Int | 1..=600000 | derived 8x the thrash window (= the deadline) | derived override | S10 thrash valve (spec R5): how long a demoted (owner-served) hot object refuses new delegation grants — MEASUREMENT ... |
| 194 | `SQUEEZEFS_DELEGATION` | Bool | — | on | A/B lever | DLM S10 (rung 12): LOOKUP-class subtree delegations — the A/B lever (design-full-multi-writer §11; the instrument sta... |
| 195 | `SQUEEZEFS_RANGE_CUSTODY` | Bool | — | off | switch | DLM S11 (rung 15, KD-MW-7): distributed byte-range custody on the S9 custody lease. |
| 196 | `SQUEEZEFS_UPDATE_INTENTS` | Bool | — | on | A/B lever | DLM S10 (rung 13, KD-MW-13): per-directory EXCLUSIVE UPDATE grants + asynchronous create-intent batches — the A/B lever. |
| 197 | `SQUEEZEFS_SLOT_PLACEMENT` | Bool | — | on | A/B lever | DLM S10 (rung 14, KD-MW-6): client-owned-slot placement — the A/B lever. |
| 198 | `SQUEEZEFS_FLEET_JOBS` | Bool | — | on | switch | KD-MW-16 (rung 10c, docs/design-mw-fleet-jobs.md): fleet-parallel maintenance participation. |
| 199 | `SQUEEZEFS_MEMBERSHIP_BIND` | Str | — | off | config | DLM S6: where this mount serves the membership plane — `off` (default), `auto` (0.0.0.0:0, ruling D2's posture), or a... |
| 200 | `SQUEEZEFS_MEMBERSHIP_LEASE_TTL_MS` | Int | 1000..=600000 | 45000 (= CLIENT_STALE_TTL_SECS) | tuning | DLM S6: the OWNER's lease TTL, ms. |
| 201 | `SQUEEZEFS_MEMBERSHIP_SKEW_MAX_MS` | Int | 1..=60000 | derived max(T_owner × 500 ppm, observed RTT) | derived override | DLM S6 (spec §6.7): clock-skew bound between owner and member, ms. |
| 202 | `SQUEEZEFS_MEMBERSHIP_PURGE_MS` | Int | 0..=600000 | derived max(2 × checkpoint cadence, observed RTT) | derived override | DLM S6 (spec §6.7's D_purge): how long a member may take to fail-stop — purge cached custody / stop in-flight DMA — o... |
| 203 | `SQUEEZEFS_MEMBERSHIP_GRACE_MS` | Int | 0..=600000 | derived (= the lease TTL) | derived override | DLM S6 (spec §6.7 Recovery): the successor owner's failover grace window, ms — reclaim admitted, conflicting fresh ac... |
| 204 | `SQUEEZEFS_MEMBERSHIP_RENEW_LANE` | Bool | — | on | A/B lever | The membership renewal's blocking VENUE (finding 15 phase B1, `.benchmarks/2026-09-07-membership-renewal-isolation.md... |
| 205 | `SQUEEZEFS_FREE_GRACE_MAX_MS` | Int | 1000..=600000 | derived (2 x one acknowledgement cycle) | derived override | Spec §6.8 item 3: how long a terminally-freed offset may wait for a reader's acknowledgement before that reader is FE... |
| 206 | `SQUEEZEFS_COWRITER_LANE_PLACEMENT` | Retired | → (nothing — placement weights are the device-fill `health_... | - | retired (refuse) | RETIRED at the symmetric default flip (PR 14, forward-only): lane-aware write placement + allocation failover on a la... |
| 207 | `SQUEEZEFS_ALLOC_LANE_HARVEST_AHEAD` | Retired | → (nothing — a writer's grant window tops up at 50 % consum... | - | retired (refuse) | RETIRED at the symmetric default flip (PR 14, forward-only): the ahead-of-stall lane refill's watermark tick. |
| 208 | `SQUEEZEFS_ALLOC_LANE_REFILL_HINT` | Retired | → (nothing — the holder's grant reply is the supply, PR 8 /... | - | retired (refuse) | RETIRED at the symmetric default flip (PR 14, forward-only): the co-writer lane refill's hint gate (`.benchmarks/2026... |
| 209 | `SQUEEZEFS_ALLOC_LANE_HARVEST_SINGLE_FLIGHT` | Retired | → (nothing — the grant window's top-up ask is single-flight... | - | retired (refuse) | RETIRED at the symmetric default flip (PR 14, forward-only): the co-writer lane harvest's single-flight lever (`.benc... |
| 210 | `SQUEEZEFS_FREE_GRACE_DEMAND` | Bool | — | on | A/B lever | The freed-offset grace loop's DEMAND arm (finding 15 part 2, docs/design-free-grace-sustain.md §5.2–§5.4 / PR 3): whe... |
| 211 | `SQUEEZEFS_FREE_GRACE_PASS_ELASTIC` | Bool | — | on | switch | L2b of the free-grace sustain campaign (docs/design-free-grace-sustain.md §5.2b — OQ 3, user decision 2026-08-25): a ... |
| 212 | `SQUEEZEFS_FREE_GRACE_ACK_PIPELINE` | Bool | — | on | A/B lever | The freed-offset grace loop's PIPELINED acknowledgement ladder (finding 15 part 2, docs/design-free-grace-sustain.md ... |
| 213 | `SQUEEZEFS_FREE_GRACE_ACK_RENEWAL` | Bool | — | on | A/B lever | The freed-offset grace loop's hold-time lever (b) (finding 15's remaining half, `.benchmarks/2026-09-06-free-grace-ho... |
| 214 | `SQUEEZEFS_FREE_GRACE_CAUGHT_UP_RELAX` | Bool | — | on | A/B lever | The freed-offset grace valve's CAUGHT-UP arm (finding 15 phase B1, `.benchmarks/2026-09-07-membership-renewal-isolati... |
| 215 | `SQUEEZEFS_FREE_GRACE_REFRESH_ON_ACK` | Bool | — | on | A/B lever | The freed-offset grace loop's hold-time lever (d) (finding 15's remaining half): the owner's renewal marks the reallo... |
| 216 | `SQUEEZEFS_FREE_GRACE_QUALIFY_CEILING` | Bool | — | on | A/B lever | The reader ack ladder's qualify window re-derived (ladder re-derivation item 1, user decision 2026-09-06, `.benchmark... |
| 217 | `SQUEEZEFS_FREE_GRACE_DRAIN_EPOCH_STAMP` | Bool | — | on | A/B lever | The reader ack ladder's drain window re-derived, part one (ladder re-derivation item 2, user decision 2026-09-06): th... |
| 218 | `SQUEEZEFS_FREE_GRACE_DRAIN_OBSERVED` | Bool | — | on | A/B lever | The reader ack ladder's drain window re-derived, part two (ladder re-derivation item 3, user decision 2026-09-06): th... |
| 219 | `SQUEEZEFS_FREE_GRACE_CHECKPOINT_COMPOSITE` | Bool | — | on | A/B lever | The freed-offset grace loop's writer→member CHECKPOINT COMPOSITE (finding 15 adjudication item 4, user decision 2026-... |
| 220 | `SQUEEZEFS_ALLOC_LANE_VOLUME_HINT` | Retired | → (nothing — a grant window is per data volume by construct... | - | retired (refuse) | RETIRED at the symmetric default flip (PR 14, forward-only): the co-writer lane refill's per-volume hint (`.benchmark... |
| 221 | `SQUEEZEFS_FREE_GRACE_LANE_PUSH` | Retired | → (nothing — under read tokens the recall is a free's quali... | - | retired (refuse) | RETIRED at the symmetric default flip (PR 14, forward-only): the freed-offset grace loop's lane-push lever — release ... |
| 222 | `SQUEEZEFS_FREE_GRACE_VALVE` | Bool | — | on | A/B lever | Spec §6.8 item 3's pressure-coupled release valve (rung-20 residual 6, docs/design-full-multi-writer.md): the graded ... |
| 223 | `SQUEEZEFS_FREE_GRACE_MAX_OFFSETS` | Int | 1..=2^26 | derived max(budget/1024/24 B, 131072) | derived override | Spec §6.8 item 3: per-volume cap on offsets held in the freed-offset grace period. |
| 224 | `SQUEEZEFS_FSCK_SETTLE_MS` | Int | 0..=600000 | 2000 | tuning | fsck suspect-settle window, ms (the zero-FP ladder). |
| 225 | `SQUEEZEFS_JOB_WIRE_BIND` | Str | — | off | config | Job-shard execution wire listen address (VAL-6: the posture switch). |
| 226 | `SQUEEZEFS_JOB_WIRE_CA_CERT` | Str | — | none | config | Job-wire CA certificate (PEM or DER). |
| 227 | `SQUEEZEFS_JOB_WIRE_CA_KEY` | Str | — | none | config | Job-wire CA private key (PEM or DER). |
| 228 | `SQUEEZEFS_JOB_WIRE_ENROLL_FRESHNESS_MS` | Int | 0..=600000 | VAL-6 default | tuning | Enrollment challenge freshness window, ms. |
| 229 | `SQUEEZEFS_JOB_WIRE_MAX_CONNS` | Int | 0..=2^20 | derived from cores | derived override | Concurrent job-wire connection cap. |
| 230 | `SQUEEZEFS_CLUSTER_WIRE_MAX_CONNS` | Int | 64..=2^16 | derived: raw CPUs × 16, ceilinged by RLIMIT_NOFILE / 8 | derived override | Symmetric PR 13c (F-B3): the cluster-wire LISTENER's concurrent-connection cap (the S8 owner listener and the members... |
| 231 | `SQUEEZEFS_JOB_WIRE_VERIFY_PERMILLE` | Int | 0..=1000 | 1000 plaintext / sampled TLS | tuning | Pre-publish verify-read sampling, per mille. |
| 232 | `SQUEEZEFS_NVMEOF_TARGET_STACK` | Enum | nvmet (retired value: spdk) | nvmet | enum select | Target stack selection (`--target-stack` flag > this env > nvmet — `nvmeof::resolve_stack`). |
| 233 | `SQUEEZEFS_NVMEOF_STATE_DIR` | Str | — | /var/lib/squeezefs/nvmeof | config | Durable target-ledger directory. |
| 234 | `SQUEEZEFS_NVMEOF_RUN_DIR` | Retired | → (deleted — SPDK was retired as an NVMe-oF target, R-SYM-8... | - | retired (refuse) | Retired with SPDK (R-SYM-8): the runtime directory held spdk_tgt's RPC socket and pidfile. |
| 235 | `SQUEEZEFS_NVMET_PORT_ID_BASE` | Int | 0..=4294967295 | derived | derived override | kernel-nvmet port-id allocation base. |
| 236 | `SQUEEZEFS_SPDK_TGT_BIN` | Retired | → (deleted — SPDK was retired as an NVMe-oF target, R-SYM-8... | - | retired (refuse) | Retired with SPDK (R-SYM-8): pointed the SPDK stack at an unpinned spdk_tgt build. |
| 237 | `SQUEEZEFS_URING_FS_WORKERS` | Int | 1..=64 | derived | derived override | `uring_fs` process-worker count for ad-hoc local file I/O. |
| 238 | `SQUEEZEFS_TEST_UPLOAD_STALL_MS` | Int | 0..=600000 | 0 | test seam | Test seam: stall each block upload, ms. |
| 239 | `SQUEEZEFS_TEST_WRITE_STALL_MS` | Int | 0..=600000 | 0 | test seam | Test seam: stall the write handler, ms (the transport-lease-overlong pin). |
| 240 | `SQUEEZEFS_TEST_RECLAIM_STALL_MS` | Int | 0..=600000 | 0 | test seam | Test seam: stall each reclaim batch, ms. |
| 241 | `SQUEEZEFS_TEST_THP_PREP_STALL_MS` | Int | 0..=600000 | 0 | test seam | Test seam: stall each deferred session-arena THP prep job, ms (the fleet-launch admission pin). |
| 242 | `SQUEEZEFS_TEST_INVAL_TAIL_STALL_MS` | Int | 0..=600000 | 0 | test seam | Test seam: stall the detached pipeline upload before its invalidation tail, ms (the 2026-08-04 tail-vs-writer race pin). |
| 243 | `SQUEEZEFS_TEST_CHECKOUT_STALL_MS` | Int | 0..=600000 | 0 | test seam | Test seam: stall the write handler post-checkout inside its held block-lock window, ms (pairs with the inval-tail sta... |
| 244 | `SQUEEZEFS_TEST_GROWTH_COMMIT_STALL_MS` | Int | 0..=600000 | 0 | test seam | Test seam: park the staged→striped growth transition of a write between its block DMA and its layout commit, ms — the... |
| 245 | `SQUEEZEFS_TEST_ROUTER_DISPATCH_STALL_MS` | Int | 0..=600000 | 0 | test seam | Test seam: park a router-route WRITE classified staged-within-block (inode guard dropped) between its classification ... |
| 246 | `SQUEEZEFS_TEST_ATTR_PUBLISH_STALL_MS` | Int | 0..=600000 | 0 | test seam | Test seam: stall the write handler between its data phase and its size/attr postlude publish, ms (the KD-5/KD-6 attr-... |
| 247 | `SQUEEZEFS_TEST_EXIT_HOLD_MS` | Int | 0..=600000 | 0 | test seam | Test seam: hold the `mount` process alive past its whole teardown, ms — a predecessor daemon whose EXIT outlives its ... |
| 248 | `SQUEEZEFS_TEST_DISMOUNT_HOLD_MS` | Int | 0..=600000 | 0 | test seam | Test seam: hold the dismount teardown open before its data-plane close (mount gone, staging root still this daemon's)... |
| 249 | `SQUEEZEFS_TEST_NVME_READ_TIMEOUT_MS` | Int | 1..=600000 | 30000 | test seam | Test seam: NVMe read timeout, ms. |
| 250 | `SQUEEZEFS_TEST_POWER_CUT_DEVS` | Str | — | none | test seam | Test seam: comma list of device paths armed for the power-cut simulator. |
| 251 | `SQUEEZEFS_TEST_OVERLAY_BYTES` | Bool | — | 0 | test seam | Test seam: force the overlay Bytes vehicle on even when SQUEEZEFS_DEVICE_OVERLAY is off (in-process suites). |
| 252 | `SQUEEZEFS_TEST_CORPSE_SWEEP_FAIL_DESTROY` | Bool | — | 0 | test seam | Test seam: the mount-time corpse sweep refuses every chunk's `destroy_inodes` commit AFTER the chunk's corpses ran `d... |
| 253 | `SQUEEZEFS_TEST_DESTROY_CHUNK_STOP_AFTER` | Int | 0..=4294967295 | 0 | test seam | Test seam: a single-ino CHUNKED destroy (one corpse whose record + xattrs + reference releases exceed the journal's w... |
| 254 | `SQUEEZEFS_TEST_FORMAT_FLAT` | Bool | — | 0 | test seam | Test seam (PR 14 — the inverted matrix): a DEFAULT-class format request builds the `--single-writer` (flat) volume in... |
| 255 | `SQUEEZEFS_TEST_STAMP_BLOCK_REFS` | Bool | — | 0 | test seam | Test seam: stamp incompat bit 9 (durable block refcounts) at format, so a suite can point the write-path fixtures at ... |
| 256 | `SQUEEZEFS_TEST_STAMP_WRITER_SCOPE` | Bool | — | 0 | test seam | Test seam: stamp incompat bit 10 (writer-scoped staging — §6.2 items 8/10) at format, so a suite can drive the scoped... |
| 257 | `SQUEEZEFS_TEST_META_BUFFERED` | Bool | — | 0 | test seam | Test seam (PR 13i F-C1, design-symmetric-metadata §5.12): keep every registered metadata device path on BUFFERED I/O ... |
| 258 | `SQUEEZEFS_SYM_RING_KB` | Int | 512..=32768 | derived | derived override | The per-appender journal ring size, KiB, on a symmetric-forest (bit 17) volume — the ring an appender ≥ 1 joins with ... |
| 259 | `SQUEEZEFS_SYM_GRANT_EXTENTS` | Int | 8..=65536 | derived | derived override | The extent GRANT the manager of a symmetric-forest (bit 17) volume answers an appender with, in 256 KiB-class heap ex... |
| 260 | `SQUEEZEFS_SYM_ALLOW_NON_PR` | Bool | — | 0 | switch | KD-SYM-13: LOUD opt-in to arm the symmetric plane — a declared appender partition, a join, WERO on the metadata names... |
| 261 | `SQUEEZEFS_META_PR_WERO` | Bool | — | 1 | switch | On a symmetric-forest (bit 17) volume whose symmetric plane is armed, the manager holds the metadata namespace under ... |
| 262 | `SQUEEZEFS_PR_REGISTRANT_CAP` | Int | 1..=65535 | unset (learned from the device; unbounded on nvmet) | tuning | A DECLARED NVMe Persistent Reservation registrant cap for a third-party PR array whose per-namespace registration tab... |
| 263 | `SQUEEZEFS_TEST_SYM_APPENDER_SLOTS` | Harness | — | - | harness | Test seam (PR 2): the DECLARED static appender partition of a forest volume — `<id>:<forest slot>[,<slot>…][;<id>:…]`... |
| 264 | `SQUEEZEFS_SYMMETRIC_META` | Bool | — | 1 | switch | The SYMMETRIC metadata plane — ON by default since the PR-14 flip (design-symmetric-metadata §7.2 / PR-plan row 14; K... |
| 265 | `SQUEEZEFS_SYM_MINT_SLOTS` | Int | 1..=64 | derived | derived override | The ROTOR: how many non-native slots an armed symmetric writer leases at join for the inodes whose parent it does not... |
| 266 | `SQUEEZEFS_SYM_AFFINITY_MAX_MB` | Int | 1..=1048576 | derived | derived override | An explicit STATIC affinity ceiling, MiB, for the bounded parent-slot mint affinity of an armed symmetric writer (des... |
| 267 | `SQUEEZEFS_SYM_T_IDLE_MS` | Int | 1000..=600000 | derived | derived override | The handover window `T_idle`, ms, of an armed symmetric writer (design-symmetric-metadata §5.1.4 — PR 4): the holder ... |
| 268 | `SQUEEZEFS_NODE_ID_FILE` | Str | — | none | config | Node-identity source file, ahead of /etc/machine-id, /var/lib/dbus/machine-id and /etc/squeezefs/node-id (writer-scop... |
| 269 | `SQUEEZEFS_SYM_DIR_STRIPES` | Int | 1..=64 | derived | derived override | The stripe count K a hot shared directory is split into at its FLIP under the armed symmetric plane (design-symmetric... |
| 270 | `SQUEEZEFS_TEST_REQUIRE_MOUNT` | Harness | — | - | harness | Turn mount-class skips into failures (TEST-2). |
| 271 | `SQUEEZEFS_TEST_REQUIRE_ALL` | Harness | — | - | harness | Promote every skip class to a failure. |
| 272 | `SQUEEZEFS_TEST_REQUIRE_HARDWARE` | Harness | — | - | harness | Promote hardware-class skips. |
| 273 | `SQUEEZEFS_TEST_REQUIRE_NON_ROOT` | Harness | — | - | harness | Promote non-root-class skips. |
| 274 | `SQUEEZEFS_TEST_REQUIRE_OPT_IN` | Harness | — | - | harness | Promote opt-in-class skips. |
| 275 | `SQUEEZEFS_TEST_REQUIRE_ROOT` | Harness | — | - | harness | Promote root-class skips. |
| 276 | `SQUEEZEFS_TEST_REQUIRE_SUDO` | Harness | — | - | harness | Promote sudo-class skips. |
| 277 | `SQUEEZEFS_TEST_REQUIRE_TOOLCHAIN` | Harness | — | - | harness | Promote toolchain-class skips. |
| 278 | `SQUEEZEFS_TEST_SKIP_LEDGER` | Harness | — | - | harness | Path the skip ledger is appended to. |
| 279 | `SQUEEZEFS_CRASH_CHILD_V3` | Harness | — | - | harness | KV crash-suite re-exec child marker. |
| 280 | `SQUEEZEFS_CRASH_CHILD_V3_BATCHED` | Harness | — | - | harness | KV crash-suite batched-child marker. |
| 281 | `SQUEEZEFS_CRASH_LEDGER` | Harness | — | - | harness | KV crash-suite ledger path. |
| 282 | `SQUEEZEFS_CRASH_ROUNDS` | Harness | — | - | harness | KV crash-suite round count. |
| 283 | `SQUEEZEFS_CRASH_VOL` | Harness | — | - | harness | KV crash-suite volume path. |
| 284 | `SQUEEZEFS_GUARD_CHILD` | Harness | — | - | harness | Writer-guard suite child marker. |
| 285 | `SQUEEZEFS_GUARD_HOLD_CHILD` | Harness | — | - | harness | Writer-guard hold-child marker. |
| 286 | `SQUEEZEFS_GUARD_OUT` | Harness | — | - | harness | Writer-guard suite output path. |
| 287 | `SQUEEZEFS_GUARD_READY` | Harness | — | - | harness | Writer-guard readiness handshake path. |
| 288 | `SQUEEZEFS_GUARD_VOL` | Harness | — | - | harness | Writer-guard suite volume path. |
| 289 | `SQUEEZEFS_WCE_CRASH_CHILD` | Harness | — | - | harness | Volatile-cache crash-suite child marker. |
| 290 | `SQUEEZEFS_WCE_CRASH_ROUNDS` | Harness | — | - | harness | Volatile-cache crash-suite round count. |
| 291 | `SQUEEZEFS_WCE_LEDGER` | Harness | — | - | harness | Volatile-cache crash-suite ledger path. |
| 292 | `SQUEEZEFS_D2_CRASH_CHILD` | Harness | — | - | harness | Two-stage-conveyor crash-suite child marker. |
| 293 | `SQUEEZEFS_D2_CRASH_ROUNDS` | Harness | — | - | harness | Two-stage-conveyor crash-suite round count. |
| 294 | `SQUEEZEFS_D2_LEDGER` | Harness | — | - | harness | Two-stage-conveyor crash-suite ledger path. |
| 295 | `SQUEEZEFS_D2_VOL` | Harness | — | - | harness | Two-stage-conveyor crash-suite volume path. |
| 296 | `SQUEEZEFS_WCE_VOL` | Harness | — | - | harness | Volatile-cache crash-suite volume path. |
| 297 | `SQUEEZEFS_M1_ROOT_DEV` | Harness | — | - | harness | Meta-slot migration suite device path. |
| 298 | `SQUEEZEFS_M1_ROOT_VICTIM_KEY` | Harness | — | - | harness | Meta-slot migration suite victim key. |
| 299 | `SQUEEZEFS_TEST_REQUIRE_CAPABILITY` | Harness | — | - | harness | Promote capability-class skips. |
| 300 | `SQUEEZEFS_TEST_REQUIRE_VENUE` | Harness | — | - | harness | Promote venue-class PARTIAL skips (a rate law left unjudged on a thermally throttling host) to failures — set on the ... |
| 301 | `SQUEEZEFS_RECLAIM_CRASH_CHILD` | Harness | — | - | harness | Block-reclaim crash-suite child marker. |
| 302 | `SQUEEZEFS_RECLAIM_CRASH_DEV` | Harness | — | - | harness | Block-reclaim crash-suite device path. |
| 303 | `SQUEEZEFS_RECLAIM_CRASH_LEDGER` | Harness | — | - | harness | Block-reclaim crash-suite ledger path. |
| 304 | `SQUEEZEFS_RECLAIM_CRASH_ROUNDS` | Harness | — | - | harness | Block-reclaim crash-suite round count. |
| 305 | `SQUEEZEFS_STAGED_CRASH_CHILD` | Harness | — | - | harness | Staged-payload crash-suite child marker. |
| 306 | `SQUEEZEFS_STAGED_CRASH_MANIFEST` | Harness | — | - | harness | Staged-payload crash-suite manifest path. |
| 307 | `SQUEEZEFS_STAGED_CRASH_META` | Harness | — | - | harness | Staged-payload crash-suite meta path. |
| 308 | `SQUEEZEFS_STAGED_CRASH_STAGING` | Harness | — | - | harness | Staged-payload crash-suite staging path. |
| 309 | `SQUEEZEFS_SCALE_DIR_ENTRIES` | Harness | — | - | harness | Directory-scale suite entry count. |
| 310 | `SQUEEZEFS_SKIP_LEDGER_CHILD` | Harness | — | - | harness | Skip-ledger suite child marker. |
| 311 | `SQZ_ALLOC_TRACE` | Harness | — | - | harness | ipc op-economy allocation-site profiler. |
| 312 | `SQZ_BSET_ALLOC_CHILD` | Harness | — | - | harness | Bset allocation-authority repro child marker. |
| 313 | `SQZ_FENCING_REMOUNT_CHILD_DIR` | Harness | — | - | harness | Fencing-remount suite child directory. |
| 314 | `SQZ_VAL3_HARDEN_CHILD` | Harness | — | - | harness | Key-handling hardening repro child marker. |

Rows whose default differs from the "natural" boolean spelling but mean the same:
`SQUEEZEFS_FUSE_SAME_LANE_DISPATCH` default `"1"`, `SQUEEZEFS_META_PR_WERO` `"1"`,
`SQUEEZEFS_SYMMETRIC_META` `"1"`, test-seam Bools `"0"` — the default string is
documentation; the reader's typed default is what matters.

Defaults the convention test pins verbatim (`tests/env_knob_convention_tests.rs:447-529`):
`META_FLUSH_INTERVAL_MS=50`, `READ_RANGED_THRESHOLD=262144`, `READ_PREFETCH_SHARE_PCT=50`,
`READ_ADMISSION_FILL_PCT=5`, `FOLD_MAX_EXTENTS=64`, `WRITEBACK_QUEUE_CAP=4096`,
`INODE_RECLAIM_BATCH=64`, `INODE_RECLAIM_WINDOW_MS=20`, `RECLAIM_BATCH_BLOCKS=64`,
`RECLAIM_CAP_PARK_MS="derived (50..1000)"`, `IPC_IDLE_SECS=300`,
`IPC_SPIN_US="0 (absent; =1 on SQUEEZEFS_IPC_SPIN_ADAPTIVE routes absent to the governor)"`,
`IL_REAP_PARK_MAX=24`, `FSCK_SETTLE_MS=2000`, `NT_COPY_MIN=262144`, `TIMEOUT=30`,
`FUSE_ATTR_TTL_MS=1000`, `SUPERVISE_INTERVAL_SECS=5`, `SUPERVISE_UNRESPONSIVE_SECS=30`,
`PARKED_GATE_ASSIST_MS=200`; Bool ON: `READ_LANE, NT_COPY, NT_READ_SERVE, NUMA,
IPC_ARENA_THP, IL_READ_DEST, REWRITE_SHADOW, DEVICE_OVERLAY, DISCARD_ELISION, FUSE_KMBUF,
FUSE_ZC`; Bool OFF: `FREE_FORENSICS, OP_PROFILE, INPLACE_OVERWRITE, DIRECT_DEVICE_TRUE,
IPC_ALLOW_DEV, FUSE_NO_KILLPRIV, ZCRX_LANE`. The exact `default` strings `"on"`/`"off"`
are asserted for these.

---

## 3. The convention ("rails") tests — scanning logic

### 3.1 Knob census (`tests/env_knob_convention_tests.rs`)

**Roots** (`:35-46`), relative to the crate manifest dir:
`src`, `crates/squeezefs-ipc/src`, `crates/squeezefs-preload/src`,
`crates/squeezefs-testkit/src`, `crates/fuse3/src`, `tests`, `benches`.
Recursive walk, `.rs` extension only (`:48-60`). Sanity: `files.len() > 100` (`:85-89`).

**Literal extraction** (`:78-107`) — NOT a regex over identifiers; it is a scan over
`"`-delimited spans:
```
for each index of '"' in the file text:
    lit = text between this '"' and the NEXT '"'
    accept if (lit starts_with "SQUEEZEFS_" or "SQZ_")
          and lit not in CENSUS_EXEMPT
          and every char of lit is [A-Z0-9_]
```
Equivalent regex on the accepted set: `^(SQUEEZEFS_|SQZ_)[A-Z0-9_]*$`. Because the scan
pairs every quote with the next quote, odd/even alternation means it also "sees" text
between string literals — harmless, since the all-uppercase filter rejects it.

**Exemptions** `CENSUS_EXEMPT` (`:66-75`): `SQUEEZEFS_`, `SQZ_` (the bare prefixes in
`is_ours`), and the deliberate fakes `SQUEEZEFS_TYPO_KNOB`, `SQUEEZEFS_NOT_A_KNOB`,
`SQUEEZEFS_X`, `SQUEEZEFS_N`, `SQUEEZEFS_D`, `SQUEEZEFS_E` (used by the parser's unit
tests and these tests).

**How it finds the registry**: it links the library and reads `squeezefs::env_knobs::KNOBS`
/ `lookup()` directly (`:27`, `:114-125`) — no file parsing of `env_knobs.rs`.

Assertions: every literal has `lookup() != None` (`:114-125`); every non-Retired
registered key is in the literal set (`:131-144`); collision law (`:152-178`);
uniqueness (`:188-199`); retired spellings refuse naming successor (`:203-232`); retired
enum values refuse with "RETIRED" and without "expected one of", both cases (`:242-274`);
malformed Bool/Int/Enum refuse quoting the value (`:280-296`); `Q_DEPTH=64` refuses
containing `1..=32`, `32` passes (`:301-312`); `""`, `"   "`, `"\t"` are absent for
Bool/Int/Enum/Str (`:316-332`); unknown announced not refused and `is_clean()==false`
(`:338-347`); all offenders together (`:351-359`); reader-level `=0` disables for a
default-OFF and default-ON knob with all 8 spellings, `bogus` keeps default (`:365-396`);
through the real binary via `CARGO_BIN_EXE_squeezefs --version` (`:595-670`).

### 3.2 No-tokio rail (`tests/no_tokio_convention_tests.rs`)

ROOTS = `src`, `crates/fuse3/src`, `crates/squeezefs-ipc/src` (`:15`). EXEMPT =
`crates/squeezefs-ipc/src/bin/ipc_hop_rig.rs` (`:18-23`). Sanity `files.len() > 100`.
Per file (`:60-76`): the "product half" is `text[..cut]` where `cut = min(find("#[cfg(test)]"),
find("#[cfg(all(test"))` (or whole file). For each line in that half: skip if
`trim_start()` starts with `//`; violation if the line contains the literal `tokio::`.
Report `"{rel}:{line_no}: {trimmed}"`. Second test (`:88-115`): in `Cargo.toml` and
`crates/fuse3/Cargo.toml`, everything before `[dev-dependencies` must not have a line
starting `tokio ` / `tokio=` / exactly `[dependencies.tokio]` / starting `tokio-rustls`
(comment lines starting `#` skipped); `crates/squeezefs-ipc/Cargo.toml` must contain
`optional = true` or no `tokio` at all.

### 3.3 sqz-sync rail (`tests/sqz_sync_convention_tests.rs`)

An explicit POPULATION list of 14 files (`:25-40`: `src/meta_backend/dlm.rs`,
`src/meta_backend/mod.rs`, `src/meta_backend/kv/{backend,node_cache,journal,checkpoint,
superblock,tree,block_refs}.rs`, `src/fuse_client.rs`, `src/routing.rs`,
`src/stripe_locks.rs`, `src/ipc_direct.rs`, `src/ipc_service.rs`). FORBIDDEN literals
= `tokio::sync::Mutex`, `tokio::sync::RwLock` (`:44`). Per line: skip if the line contains
the same-line marker `sqz-sync-exempt` (`:55-57`); else any FORBIDDEN substring is a
violation `"{rel}:{line}: {trimmed}"`. Second test: every POPULATION path `is_file()`.

### 3.4 Pattern to port

All three rails: (1) link against the library for registry facts, scan source text
for literals; (2) walk fixed root lists recursively for `.rs`; (3) `files.len() > 100`
sanity so a wrong root fails loud; (4) exemption lists are explicit constants, never
heuristics; (5) failure message lists every violation with `path:line`. The
derivation test at `tests/derivation_sweep_tests.rs:1639-1751` uses the same walk to
pin the exact SET of files containing `possible_cpus(` (expected:
`src/cpu.rs, src/fuse_client.rs, src/mem_budget.rs, src/stripe_locks.rs,
crates/fuse3/src/raw/connection/fuse_over_uring.rs`) and `raw_parallelism(`
(`src/cpu.rs, src/cluster_wire.rs`) over roots `src, crates/fuse3/src,
crates/squeezefs-ipc/src, crates/squeezefs-preload/src`.

---

## 4. Sharded counters and the invariant tripwire

Three sharded instrument families exist; all share one law: each thread owns a
stripe chosen once (round-robin), increments are `Relaxed` RMWs on a core-local
cache line, and a read FOLDS every stripe (addition commutes ⇒ exact for everything
that happened-before the read; a concurrent increment may or may not be included,
as with a single atomic).

### 4.1 `ShardedAtomic` (root crate, `src/fuse_client.rs:4894-4953`)

```rust
const SHARDED_ATOMIC_STRIPES: usize = 64;                 // :4897 (fixed, not derived)
#[repr(align(64))] pub struct Align64<T>(pub T);          // :4990-4992
pub struct ShardedAtomic { stripes: [Align64<AtomicU64>; 64] }   // 64 × 64 B = 4 KiB
fn sharded_stripe_index() -> usize {                      // :4930-4939
    thread_local! { static STRIPE: usize = { static NEXT: AtomicUsize;
        NEXT.fetch_add(1, Relaxed) % SHARDED_ATOMIC_STRIPES }; }
    STRIPE.with(|s| *s)
}
fetch_add(val, order) { stripes[sharded_stripe_index()].fetch_add(val, order) }  // :4942
load(order) -> u64   { stripes.iter().map(|s| s.load(order)).sum() }             // :4950
```
- Stripe selection: a process-global `AtomicUsize` counter, `fetch_add(1, Relaxed) %
  64`, evaluated ONCE per thread on first use and cached in TLS (`:4924-4929`). Never
  `sched_getcpu` per op; a migrating thread keeps its stripe. Above 64 live threads
  stripes are shared but still exact (`:4894-4897`).
- Caller passes the `Ordering`; all production sites use `Relaxed`.
- Replaced a `ProbabilisticAtomic` (per-thread 128-batches) whose invisible residue
  manufactured a 2× ghost in stats deltas (`:4899-4911`) — exactness is the requirement.
- The SAME `sharded_stripe_index()` serves `ShardedLatencyHistogram` (`:5167-5226`:
  `OnceLock<Vec<HistStripe>>`, `#[repr(align(256))] struct HistStripe(LatencyHistogram)`
  — `LatencyHistogram = { buckets: [AtomicU64; 26], count: AtomicU64, sum_ns: AtomicU64 }`
  = 224 B padded to 256 B; 64 stripes lazily allocated; `record` = 3 Relaxed RMWs;
  `count/sum_ns/buckets` fold) and `ShardedQueueDepthHistogram` (`:4958-4988`,
  `Box<[QueueDepthHistogram]>` of 64, 15 buckets each).
- Bucketing: `latency_bucket_index(micros)` = `0` if `micros <= 1` else
  `min((micros-1).ilog2()+1, 25)` (`crates/squeezefs-ipc/src/latency_core.rs:32-38`);
  26 labels `<=1us … <=16s, >16s` (`:22-26`). `record` divides ns by 1000 first
  (`fuse_client.rs:5078`).
- JSON shape for every histogram (`fuse_client.rs:5039-5069`):
  `{"buckets": {label: n, …26}, "count": c, "sum_ns": s, "mean_ns": s/c (0 if c==0)}`.

### 4.2 `ShardedCounter` (fuse3 fork, `crates/fuse3/src/raw/read_phase.rs:217-265`)

```rust
pub(crate) fn shard_count() -> usize {                    // :172-181, OnceLock
    available_parallelism().unwrap_or(1).next_power_of_two().clamp(1, 64)
}
pub(crate) fn shard_index() -> usize {                    // :196-209
    static NEXT: AtomicUsize;  thread_local! { static MINE: Cell<usize> = usize::MAX }
    if MINE == usize::MAX { MINE = NEXT.fetch_add(1, Relaxed) % shard_count() }  MINE
}
#[repr(align(64))] struct Line(AtomicU64);
pub(crate) struct ShardedCounter { shards: OnceLock<Vec<Line>> }   // const fn new()
add(n)  { shards()[shard_index()].0.fetch_add(n, Relaxed) }        // :252-256
load()  { shards().iter().map(|l| l.0.load(Relaxed)).sum() }       // :259-264
```
Differences from the root crate's: stripe count is DERIVED
(`available_parallelism → next_pow2 → clamp [1,64]`), shards are lazily allocated on
first `add`/`load` (so a `static X: ShardedCounter = ShardedCounter::new()` costs nothing
until used), the fuse3 fork has its own TLS index (separate from the root crate's). The
same `shard_count()`/`shard_index()` also index the transport phase tables
`shards[shard][op_class][phase]` (`:154-215`). Instances: `FAST_DISPATCH_{SERVES,DEMOTES}`
(`fast_dispatch.rs:147-148`), `STATS_{REQUESTS,REPLIES}`, `TRANSPORT_COMMIT_FLUSHES`,
`TRANSPORT_COMMITS_SUBMITTED` (`fuse_over_uring.rs:1789-1798`),
`SPIN_{ABSORBED,EXPIRED,NS,REFUSED_BUSY}` (`spin.rs:63-66`), `FUSED_{MIDPASS,PASSBOTTOM}_REAPS`
(`read_phase.rs:459-460`), two in `kmbuf.rs:842,1003`. Each is exposed by a plain
accessor (`pub fn fast_dispatch_serves() -> u64 { FAST_DISPATCH_SERVES.load() }`,
`fast_dispatch.rs:199-201`) that the daemon's stats builder places under a flat key
(`"transport_fast_dispatch_serves": fuse3::fast_dispatch_serves()`,
`src/fuse_client.rs:12020`; `"transport_spin_absorbed"`, `:12383`).

### 4.3 The invariant tripwire (`src/lib.rs:548-612`)

Purpose (RES-22): a concurrency-outcome assertion must be loud, counted, NEVER fatal —
`debug_assert!` in a handler task loses a FUSE reply (`:564-580`).

```rust
static INVARIANT_TRIPWIRE_SITES: Lazy<scc::HashMap<&'static str, AtomicU64>>;  // :553-555
pub fn invariant_tripwire_count(site: &str) -> u64;      // :558-562, read_sync, Relaxed, 0 if absent
pub fn note_invariant_tripwire(site: &'static str, detail: &str) {   // :584-612
    METRICS.invariant_tripwires.fetch_add(1, Relaxed);                 // the TOTAL (Align64<AtomicU64>, fuse_client.rs:6315)
    SITES.entry_sync(site).or_insert_with(|| AtomicU64::new(0)).get().fetch_add(1, Relaxed); // per-site label
    // rate limit: one log line per site per SECOND
    static LAST: Lazy<scc::HashMap<&'static str, AtomicU64>>;
    let now = coarse_realtime_ns() / 1_000_000_000;      // CLOCK_REALTIME_COARSE, lib.rs:538-546
    let quiet = LAST.read_sync(&site, |_, last| last.swap(now, Relaxed) == now)
                    .unwrap_or_else(|| { LAST.insert_sync(site, AtomicU64::new(now)); false });
    if !quiet { log::error!("INVARIANT TRIPWIRE '{site}': {detail} — a concurrency outcome the design says cannot happen just happened. Counted in invariant_tripwires; the daemon proceeds (a panic here would lose a FUSE reply, which is strictly worse than a wrong-but-served op)"); }
}
```
- The per-site label is the `&'static str` `site` argument (e.g.
  `"transport_lease_overlong"`, `"xv_local_step_unguarded"`,
  `"served_publish_over_dirty_entry"`, `"res22_unit"`); the count, not the first fire,
  is what contracts read — `entry_sync` avoids a racing-`insert` drop (`:548-552`).
- The total is a PLAIN `Align64<AtomicU64>` (not sharded — tripwires are rare by
  contract), declared `pub invariant_tripwires` in the `METRICS` struct
  (`fuse_client.rs:6309-6315`, "**0 on a healthy daemon**").
- Stats JSON: `"invariant_tripwires": METRICS.invariant_tripwires.load(Relaxed)`
  (`fuse_client.rs:11600`), flat key, total only. The per-site census is NOT in the
  JSON; it is an in-process API (`invariant_tripwire_count`) that tests use to allow-list
  ONE known routed site while every other stays must-stay-0 (e.g.
  `tests/sym_n_daemon_tests.rs:5032`). Neighbouring must-stay-0 keys with the same
  framing: `job_worker_panics`, `detached_task_panics`, `lock_ticked_reregisters`
  (`:11591-11606`).
- Contract test `tests/invariant_tripwire_tests.rs:41-53`: one call increments the
  total by exactly 1 and returns; `:56-73`: a converted site (`ActiveBlockBuf::
  zero_complete` on a deferred-seed buffer) reports and still establishes validity.
- The suites assert `invariant_tripwires == 0` after scenarios via `stat_u64(&mnt,
  "invariant_tripwires")` (e.g. `tests/corpse_sweep_tests.rs:577`).

---

## 5. Thread naming → CPU accounting class (`src/daemon_cpu.rs`)

### 5.1 Names

Every per-mount thread name goes through `squeezefs_ipc::comm_core::comm_name(base)`
(`crates/squeezefs-ipc/src/comm_core.rs:33-68`): if the process's `COMM_TAG`
(mount slot, set once by `writer_scope::set_mount_identity`) is 0, the base is used
verbatim; else the suffix `m{slot & 0xf:x}` (2 chars) is appended and the BASE is
truncated to `COMM_MAX − 2 = 13` so the whole name fits the kernel's 15-visible-char
comm budget (`TASK_COMM_LEN − NUL`). The suffix never truncates. The fuse3 fork has
its own `#[path]` copy of the static, seeded separately. Spawn sites:

| base pattern | where | class |
|---|---|---|
| `fuse3-tpc{i}`, `fuse3-tpc-fallback` | `crates/fuse3/src/raw/session.rs:6036,6088` | `fuse3-tpc` |
| `f3-ur{qid}` / `f3-ur{first}-{last}`, `f3-ur-watch` | `fuse_over_uring.rs:3162-3164,3236` | `fuse3-ur` |
| `sqz-ipc-svc{n}` | `src/ipc_host.rs:2851` | `sqz-ipc-svc` |
| `sqz-ipc-dd{idx}` | `src/ipc_direct.rs:998` | `sqz-ipc-dd` |
| `sqz-meta{i}` | `src/meta_exec.rs:58` | `sqz-meta` |
| `sqz-jrnl{idx}` | `src/meta_backend/kv/journal_lane.rs:141` | `sqz-jrnl` |
| `sqz-blk{seq}` | `crates/squeezefs-ipc/src/sqz_blocking.rs:164` | `sqz-blk` |
| `sqz-nvme{lane}` | `src/nvme_dev.rs:621` | `sqz-nvme` |
| `sqz-zcrx-q{rxq}`, `-admin`, `-wr{qid}`, `-rd{qid}` | `src/zcrx_lane/*.rs` | `sqz-zcrx` |
| `sqz-timer` | `crates/squeezefs-ipc/src/sqz_time.rs:104` | `sqz-timer` |
| `sqz-lease` (`meta_exec.rs:112`), `sqz-lease-io` (`membership_wire.rs:116`), `sqz-clw-accept/-conn/-mux` (`cluster_wire.rs:2236,2363,3623`), `sqz-fdwatch`, `sqz-signal`, main | — | `other` |

`tests/mount_comm_tests.rs:44-80` sweeps the worst-width bases (`sqz-ipc-svc63`,
`sqz-jrnl9999`, `fuse3-tpc511`, `f3-ur0-511`, …) through `comm_name_with(base,
0xffff_ffff)` and asserts `len <= 15` and `ends_with("mf")`.

### 5.2 Classification (`daemon_cpu.rs:28-94`)

```rust
pub const CLASSES: [ThreadClass; 10] = [ (prefix, class):
  ("fuse3-tpc","fuse3-tpc"), ("f3-ur","fuse3-ur"), ("sqz-ipc-svc","sqz-ipc-svc"),
  ("sqz-ipc-dd","sqz-ipc-dd"), ("sqz-meta","sqz-meta"), ("sqz-jrnl","sqz-jrnl"),
  ("sqz-blk","sqz-blk"), ("sqz-nvme","sqz-nvme"), ("sqz-zcrx","sqz-zcrx"), ("sqz-timer","sqz-timer") ];
pub const OTHER: &str = "other";
pub fn classify(comm) -> &'static str { first CLASSES row where comm.starts_with(prefix), else OTHER }
```
First match wins; order matters when one prefix could prefix another. The `m{x}`
suffix sits after the index so prefix matching tolerates it.

### 5.3 Sampling (`daemon_cpu.rs:123-220`) — at stats-read time ONLY

1. `process_cpu_ns()` = `getrusage(RUSAGE_SELF)` utime+stime in ns (`:124-133`).
2. `thread_cpu_ns(dir)` (`:137-154`): read `/proc/self/task/<tid>/schedstat`, take field
   1 (`sum_exec_runtime`, ns). Fallback without `CONFIG_SCHED_INFO`: read `.../stat`,
   find `rfind(')') + 2`, split whitespace, `utime = nth(11)`, `stime = next()` (1-based
   fields 14/15), `ns = (utime+stime) * 1e9 / sysconf(_SC_CLK_TCK)` (default 100 if ≤ 0).
3. `sample()` (`:158-220`) under a plain `Mutex<Option<Ledger>>` (a stats read is a FUSE
   op, so a mutex is in-policy): `Ledger { live: HashMap<tid, (class_idx, ns)>, retired:
   Vec<u64> of len CLASSES.len()+1 }`. For each `/proc/self/task/*`: parse tid, get ns,
   read `comm` (trim_end), `ci = class_index(classify(comm))` (OTHER → index 10). If the
   tid is live with the same class and `ns >= old` → update; if class changed or ns
   DROPPED (monotone runtime ⇒ tid reuse) → `retired[old_ci] += old_ns`, replace; new →
   insert. After the scan, every live tid not seen this pass is removed and its last
   ns added to `retired[ci]` — so a class NEVER falls (monotone gauge; the CPU between a
   thread's last sample and exit is the one lost term, `:7-11`). `sums = retired +
   Σ live`. Then `total_ns = process_cpu_ns()` is read AFTER the per-thread fold so
   `Σ by_class ≤ total_ns` holds (`:104-106`).
4. Output `CpuSample { total_ns, by_class: Vec<(&str,u64)> }` with every class present,
   `other` last; JSON `by_class_json()` = `{class: ns}` (`:223-230`), emitted as
   `"daemon_cpu_ns": total` and `"daemon_cpu_ns_by_class": {…}` (`fuse_client.rs:11966-11967`).

---

## 6. Derivation tie tests (`tests/derivation_sweep_tests.rs`, 4581 lines)

### 6.1 Structure

- Every sizing law has a PURE function (`resolve_*(env_str_opt, …roots)` or
  `derived_*(roots)`) separate from its OnceLock-cached production reader; the test
  calls the pure form (`:4-8`).
- Two canonical shapes (`:9-15, 25-31`): **field** = 251 GB RAM ⇒ `FIELD_BUDGET = 176 GiB`
  (70 %), 32 CPUs, 4 MiB blocks, 32 MiB rings; **floor** = 4 GB / 2 CPUs ⇒
  `FLOOR_BUDGET = 2 GiB + 820 MiB`. Each law is pinned on both, plus boundary points.
- Precedence everywhere: `absolute env > percentage env > derived`; explicit wins
  verbatim (including the "A0 lever" value that restores the pre-derivation constant);
  junk env warns and falls through to the derived value — never fails a mount
  (`:16-19`). Floors are the shipped value ("never-regress-below-shipped") or a
  physical minimum, never tuning.
- Fleet share (`SQUEEZEFS_FLEET_SHARE = N`): ONE divisor at the two ROOT inputs
  (`ram`/`cgroup` before ×0.7/×0.8; CPU root `ceil(raw/share)`, floor 1), so every
  downstream formula quarters at share=4 where above its floor; floors never divided;
  explicit tiers never divided; the FUSE queue COUNT (`possible_cpus`) is exempt
  (`:1350-1360, 1371-1439, 1446-1508, 1515-1561`).
- Tests also assert the registry row (`Kind::Int{lo,hi}`, default string) for the knob
  that overrides the law (e.g. `:405-412, 532-542, 600-613, 1756-1780`).

### 6.2 The first ~30 pinned derivations (formula per line; `FB` = `FIELD_BUDGET`, `cpus` = fleet-share-divided process parallelism unless stated)

1. `transport_buffer_cap(budget) = budget / 8` — no fixed ceiling (`:45-61`; FB → 22 GiB; 0 → 0).
2. `resolve_transport_buffer_cap(budget, MEM_MAX_mib, MEM_PCT)`: abs MiB verbatim (0 allowed) > pct in (0,100] (>100 clamps to 100) > budget/8 (`:68-102`).
3. `resolve_node_cache_budget(budget, MB, PCT) = max(budget/16, 512 MiB)` (`:114-142`; FB → 11 GiB; floor box → 512 MiB).
4. `dir_entry_capacity(total) = max(50_000, total / 200_000)` (`:154-170`).
5. `resolve_max_dirty_nodes(budget, node_size, env) = max(budget/32 / node_size, 4096)`; node_size 0 → 4096 (`:182-207`; FB@256 KiB → 22528).
6. `resolve_commit_batch_txs(env, cpus) = max(64, cpus × 2)`; env `0`/junk → derived (`:218-226`; 128 → 256).
7. `resolve_commit_batch_bytes(env, ring_bytes) = max(256 KiB, ring / 16)` (`:234-247`; 32 MiB → 2 MiB).
8. `derived_stripe_width(shipped, possible_cpus, q_depth) = next_pow2(max(shipped, 16 × cpus × depth))`; `STRIPE_LOAD_FACTOR_INV=16`, `DLM_STRIPES_SHIPPED=4096`, `LEASE_STRIPES_SHIPPED=1024` (`:265-343`; (4096,32,32) → 16384; (4096,192,32) → 131072).
9. `transport_inflight_ceiling() = possible_cpus × q_depth` (no factor, no rounding; depth = explicit `Q_DEPTH` clamped `1..=Q_DEPTH_DESIRED(32)`, else 32) (`:358-375`).
10. `resolve_dlm_stripes(explicit, shipped, cpus, depth)`: explicit rounds UP to pow2 (1000 → 1024; 1 allowed), else law 8 (`:381-436`).
11. `derived_patch_max_bytes(block) = block / 8` (`:447-455`; 4 MiB → 512 KiB).
12. `derived_fold_max_bytes(block) = block / 4` (`:461-464`).
13. `derived_inline_max_bytes(cap) = INLINE_MAX_FLOOR = 4096` at every node size; `inline_max_bytes_ceiling(cap) = xattr_value_cap(node) − 4096` where `xattr_value_cap = min(65536, node/4)` (256 KiB node → 60 KiB; 64 KiB node → 12 KiB) (`:483-542`).
14. `pack_max_slot_bytes() = CHUNK_SIZE / 2` (2 MiB); `pack_slot_len(n) = round_up(n, 4096)`; override verbatim (`:562-623`).
15. `pack_victim_max_live_bytes() == pack_max_slot_bytes()`; `is_pack_victim(live) ⇔ live ≤ threshold` (`:636-670`).
16. `resolve_parked_cap_buffers(env, budget, block) = max(256, budget/16 / block)`; block 0 defensive (`:688-708`; FB → 2816).
17. `ipc_session_population_target(cpus) = max(128, cpus × 8)` (`EXA_CANON_QD = 8`) (`:737-750`).
18. `resolve_ipc_arena_bytes(env_mib, cap, cpus) = max(64 MiB, align_down(cap / population_target(cpus), 4 MiB))`; explicit MiB verbatim (`:779-864`; 22 GiB/32 cpus → 88 MiB).
19. `Geometry::slot_slab() = min(arena/slots, max_op_bytes)`, floored to a 4 KiB multiple when ≥ 4 KiB (`:874-904`).
20. `resolve_worker_count(env, cpus) = clamp(cpus/4, 4, 64)`; explicit clamped to 64 (`:920-929`; 32 → 8).
21. `ipc_per_uid_session_cap(cap, arena) = clamp(cap / max(arena,1), 64, 4096)`; monotone non-increasing in arena (`:955-1033`).
22. `EXA_CANON_QD = 8` must equal `IODEPTH="N"` in `tests/fio/run_fio_row.sh` and the qd column of the 4 battery rows in `tests/fio/exa_client_perf.sh` (`:1053-1103`).
23. `METADATA_CACHE_TTI_SECS = 300`; `EPOCH_IDLE_HORIZON_MS = TTI × 1000 / 10 = 30_000` (`:1112-1123`).
24. `READ_SIZE_CLASS_BOUNDARY_BYTES = 256 KiB == READ_LANE_MIN_FILL_BYTES` (`:1132-1140`).
25. zcrx `lane_geometry(cpus) = (clamp(cpus/8, 1, 8), clamp(cpus × 2, 4, 64))` (`:1150-1165`; 32 → (4,64)).
26. `LANE_MAX_XFER_CAP_BYTES == fuse3 PAYLOAD_BASE == 1 MiB` (`:1174-1179`).
27. zcrx `lane_eligible_queues(channels, devices)` width = `clamp(devices, channels/4, channels/2)`, highest-indexed slice, 0 when channels < 4; `admission_permits(window, 4096) = window/4096`; `delivery_slack_bytes(window, Some(9000), 4096) = 24 MiB`, `None` → window (`:1193-1220`).
28. fuse3 `drain_group_width(node_possible_cpus) = max(1, cpus/4)`; groups never span a NUMA node; interleaved numbering handled by membership (`:1235-1301`).
29. `il_drain_lanes_default(cpus) = clamp(3 × cpus / 8, 2, 64)`; dominates `il_sessions_default(cpus) = clamp(cpus/4, 2, 16)` for all cpus in 1..=512 (`:1325-1348`).
30. `fleet_shared_root(v, share) = ceil(v / max(share,1))`; `resolve_budget_from_shared(flag, env, cgroup, ram, share) = resolve_budget_from(flag, env, cgroup.map(÷share), ram ÷ share)` (`:1372-1411`).
31. `effective_parallelism_from(raw, share) = max(1, ceil(max(raw,1) / max(share,1)))` (`:1416-1439`).
32. `fleet_share_floor_refusal(share, divided_budget, possible_cpus)`: `None` if share == 1 or `divided_budget ≥ possible_cpus × Q_DEPTH_FLOOR(4) × PAYLOAD_BASE(1 MiB)`; else a message naming `SQUEEZEFS_FLEET_SHARE=N`, both byte numbers, `Q_DEPTH_FLOOR`, and the CPU count (`:1594-1627`).

Later sections (same pattern) pin: `pool_cap_from(c) = max(8, c × 16)`;
`block_pool_capacity_from(c) = max(64, c × 16)`; `ranged_pool_capacity_from(c) = max(512,
c × 64)`; `lru::shard_count_from(c) = max(16, next_pow2(c))`; `scratch_pool_capacity_from(c)
= clamp(c/4, 4, 16)`; `cluster_wire::service_threads_from(env, c) = clamp(ceil(c/8), 1, 8)`
(explicit never exceeds c); `meta_ship batch_max_from(env, c) = clamp(c × 2, 64, 4096)`;
`publish_ship_depth_from(env, c) = clamp(ceil(c/8), 2, 8)` (explicit clamped `1..=64`);
`fabric_workers_default(c) = clamp(c/4, 2, 8)`; `format_pool_permits(c) = max(2, c × 2)`;
`meta_lanes_from(c) = 1 if c == 1 else 2` (`:1800-1979`);
`max_connections_from(raw_cpus, nofile) = clamp(raw × 16, 64, nofile / 8)` (`:2001-2076`);
W-4 reclaim: `room_latency_ms(batch, rate) = max(1, batch × 1000 / rate)` (rate 0 → 1000),
`derived_park_bound_ms(room) = clamp(4 × room, 50, 1000)`,
`derived_queue_cap_blocks(rate, room, budget) = clamp(rate × room / 1000, 4096,
clamp(budget/1024/entry_bytes, 4096, 2^20))` (`:2464-2628`).

---

## 7. Port checklist (what the Zig side must reproduce byte-for-byte)

1. `present/parse_bool/parse_int/parse_int_in/parse_enum/parse_str` with the exact
   `KnobError` Display `"{key}='{raw}' is invalid: {want}"` and the three `want` strings.
2. A static registry of the 314 rows above with `lookup`, `is_ours`, `validate_vars`
   (sorted `errors`/`unknown`), the Retired and retired-enum-value message formats, and
   `refusal_report` → `main` printing `Error: {report}` and exiting 1, after warning per
   unknown name with the exact `Warning: … — ignored` line.
3. `bool_knob/int_knob/opt_int_knob/enum_knob` in-process readers (log + default).
4. The rails: knob-literal census over the seven roots with `CENSUS_EXEMPT`; inverse
   census excluding Retired; Bool-only-prefix collision law; uniqueness; the gate
   through the binary.
5. `ShardedAtomic` (64 fixed stripes, TLS round-robin, Relaxed), fuse3-side
   `ShardedCounter` (`next_pow2(available_parallelism).clamp(1,64)` lazily allocated
   stripes), the 26-bucket histogram and its JSON shape.
6. `note_invariant_tripwire(site, detail)`: total + per-site count + 1-line/site/second
   rate-limited error log; `invariant_tripwires` flat stats key; `invariant_tripwire_count`.
7. `comm_name` (15-char budget, `m{nibble:x}` suffix, base truncation), the 10-row
   prefix→class table with first-match-wins, `/proc/self/task/*/{schedstat|stat,comm}`
   sampling with the retired-ledger monotone fold, `Σ by_class ≤ total_ns`.
8. Pure `resolve_*`/`derived_*` forms for every law in §6 so tie tests can pin both
   canonical shapes.
