# SPDK-Mandatory Program — Target-Side Scoping Report

Date: 2026-07-17 · Dev at `ad68be1` (clean) · Box: AMD RYZEN AI MAX+ PRO 395 (32 threads, capped 3.5 GHz, governor `performance`), 109 GiB RAM, kernel 7.1.3-2-cachyos, btrfs root.
Scope charter: SPDK becomes **mandatory for NVMe-oF target serving**; **client/initiator side is explicitly out of scope** (daemon stays kernel io_uring `NvmeBlockDev`, kernel initiator, PR ioctls on kernel block nodes). `tests/dev_substrate.sh` stays kernel nvmet-loop as the default dev substrate; this pass seeds a later target-fidelity test tier. This is a scoping report — **no product code changed, no design decisions locked.**

Artifacts: rig scripts live next to this file (`snapshot.sh`, `rig-up.sh`, `bench.sh`, `bench-all.sh`, `rerun-row.sh`, `pr-matrix.sh`, `guard-smoke.sh`, `teardown.sh`); raw run outputs under `/tmp/spdkscope/results/` (session-local; everything material is quoted verbatim below); SPDK build tree at `/var/tmp/spdk-scoping/` (local, never installed system-wide).

---

## 1. `src/nvmeof.rs` inventory (1,549 lines)

Header facts that frame everything below: the module opens with a **JuiceFS Apache-2.0 copyright header** (copy-paste provenance) and `#![allow(clippy::all)]` — it is exempted from the house `-D warnings` gate wholesale. All tests are **mock-mode only** (`SQUEEZEFS_MOCK_NVMEOF=1`, `tests/nvmeof_tests.rs` — every path short-circuits before touching a real kernel or a real SPDK socket). Git history: born in the early feature burst (`87e12a7`, `ec5b18f`…`15e5faf` for the SPDK verbs), then untouched except one real fix — `eafab6c` (btrfs NoCOW guard, the only battle-tested part).

### 1.1 Kernel-nvmet config path (`share_target` / `unshare_target` / `list_nvmeof`)

What it does today:
- **Duplicate guard** against a JSON share registry (see 1.4 — broken as root) with canonicalized backing paths.
- **Auto-creates a missing backing file as a 1 GiB sparse file** (silent side effect on typos).
- `ensure_nocow_backing()` — the good part: on btrfs it applies/verifies `FS_NOCOW_FL` on empty/hole-only files and **fails loud with remediation** on data-carrying CoW files (O_DIRECT silently degrades to buffered on CoW → the single-threaded target wedges in the dirty-page throttle; the fix commit documents a live fabric wedge). Has real unit tests (`nocow_tests`).
- `modprobe nvmet nvmet-tcp`, mounts configfs if absent.
- Regular files → **losetup** association (reuses existing or `losetup -f`).
- Configfs verbs: `subsystems/<nqn>` mkdir → `attr_allow_any_host=1` → `namespaces/1/device_path` + `enable=1`. Default NQN `nqn.2026-06.io.squeezefs:subsystem-<uuid>`.
- **Loop association is recorded by `fs::write(sub_dir/"associated_loop_device")` inside configfs** — configfs does not allow arbitrary file creation, so on a real kernel this write fails (silently, `let _ =`) and `unshare_target`'s loop detach never fires outside the mock. Mock-only behavior shipped as product code.
- Ports: scans `ports/1..n` for an ip:port match, else creates the **next free small integer id** (1, 2, 3…) — exactly the id namespace `tests/dev_substrate.sh` defends against with `SQZ_DEVSUB_PORT_ID=52026` ("ports carry no name"). `addr_trtype=tcp` hardcoded, ipv4 only. Symlinks the subsystem in.
- `unshare_target`: registry-based SPDK dispatch (broken registry ⇒ SPDK shares fall through to the configfs branch and fail NotFound), symlink removal across all ports, namespace disable + rmdir, loop detach (never fires, see above).
- `list_nvmeof`: walks configfs subsystems + `nvmf_get_subsystems` RPC + `/sys/class/nvme` for connected fabric disks.
- Error handling: `io::Result` plumbing, but a dozen `let _ =` swallows on the teardown/registry edges.

### 1.2 The `--spdk` path — how live is it?

Mechanism (a real JSON-RPC client, not vestigial *code* — but **unexercised**, vestigial *in practice*):
- `call_spdk_rpc()`: hand-rolled JSON-RPC 2.0 over `UnixStream` to `/var/tmp/spdk.sock` (`SQUEEZEFS_SPDK_SOCK` override). No timeout, no version negotiation, single-shot id=1.
- `share_target_spdk()`: duplicate guard (registry + live `bdev_get_bdevs` aio-filename scan) → NoCOW guard → `bdev_aio_create` (block_size 4096) → `nvmf_create_transport TCP` (errors ignored) → `nvmf_create_subsystem` (allow_any_host, serial `SQ<uuid>`) → `nvmf_subsystem_add_ns` (**no `ptpl_file`, no pinned ns UUID** — reservation persistence never configured; see §4) → `nvmf_subsystem_add_listener` per IP.
- `unshare_target_spdk()`: `nvmf_get_subsystems` → find ns bdev → `nvmf_delete_subsystem` → `bdev_aio_delete`.
- `spdk_install()`: **`git clone` of master to `/opt/spdk`** — no tag pin, no checksum, system-wide location; runs `pkgdep.sh` (mutates system packages), `pip3 install --break-system-packages`, bare `./configure`, `make -j$(nproc)`.
- `spdk_setup(mb)`: writes 2M-page count to sysfs (or `setup.sh config_huge`).
- `spdk_bind`/`spdk_unbind`: `setup.sh bind` / sysfs `driver/unbind` + `drivers_probe`.
- `spdk_start()`: spawns `/opt/spdk/build/bin/nvmf_tgt -i 0 -m 0x1` detached with stdout/stderr → null, `pgrep` dedupe, **no pidfile, no supervision, no config file, no restart story**.

Liveness evidence:
- **Zero non-mock tests.** `test_nvmeof_spdk_helpers_mock` asserts `spdk_install()` returns Ok in mock mode — it tests the mock switch, nothing else.
- **Zero call sites outside the CLI dispatch** (`main.rs` 2862–2956 is the only consumer).
- `extract_nvmeof_connection_details()` — `pub`, **zero callers anywhere** (dead code by house rules).
- `SqueezefsError::NvmeOfBackend` — **constructed nowhere** (dead variant; only defined + errno-mapped).
- This pass exercised the same RPC verb shapes against a real spdk_tgt v26.05 (`bdev_aio_create`, `nvmf_create_subsystem`, `add_ns`, `add_listener` all current), so the path would likely *function* today against a running target — but nothing in CI or the field has ever proven it, and it misses the PR-critical `ptpl_file`/`nsid`/`uuid` parameters.

### 1.3 CLI surface (`squeezefs storage nvmeof …`, clap `NvmeofActions`)

| Verb | Args | Notes |
|---|---|---|
| `share <backing_path>` | `--subnqn`, `--port` (4420), `--ip` (required, multi), `--spdk` | kernel configfs or SPDK RPC |
| `unshare <subnqn>` | `--spdk` | registry-dispatched (broken as root, 1.4) |
| `connect` | `--ip`, `--port`, `--subnqn` | **client side — stays** (nvme-cli, `/dev/nvme-fabrics` fallback) |
| `disconnect <subnqn>` | | client side — stays (sysfs `delete_controller`) |
| `list` | | targets (configfs + SPDK RPC) + connected fabric disks |
| `restore-shares` | | re-share from registry (non-functional as root, 1.4) |
| `spdk-install` | | clone+build **master** → `/opt/spdk` |
| `spdk-setup` | `--hugepages` (2GB) | hugepage reservation |
| `spdk-bind` / `spdk-unbind` | `--pci` | vfio/uio bind, return to kernel |
| `spdk-start` | | background `nvmf_tgt`, `-m 0x1`, RPC at `/var/tmp/spdk.sock` |

### 1.4 Registry bug (found during inventory — the liveness smoking gun)

`get_shares_config_path()` (nvmeof.rs:1019–1033): when running as root it does `fs::write(&path, "[]")` **on every resolution** as its writability probe — i.e. `load_shares()` truncates `/etc/squeezefs/nvmeof_shares.json` to `[]` *before reading it*. Consequences (all root paths): the registry duplicate-share guard never fires, `restore-shares` always restores nothing, and `unshare` of an SPDK share never detects `is_spdk` (falls into the configfs branch → NotFound). Only the `SQUEEZEFS_TEST_ENV` tmpfile path (what the mock tests use) behaves. Share persistence has therefore **never worked in production** — and nobody noticed, which is the strongest liveness signal in the module.

### 1.5 Adjacent couplings

- **`storage.rs` pool plumbing:** none. No `nvmeof::` references; LVM pool/volume management is independent. (`SqueezefsError::NvmeOfBackend` in `error.rs` is an orphan.)
- **Writer guard (M1) expectations:** documentary coupling only. `src/meta_backend/reservation.rs` cites nvmeof.rs twice: (a) the nvme-cli shell-out precedent legitimizing control-plane ioctls ("io_uring-first governs data paths, not mount-time admin plumbing"), (b) the `/etc/nvme/hostnqn`+`hostid` **host-identity convention** — `get_host_nqn()`/`get_host_id()` in nvmeof.rs create those files, and the guard's `host_identity()` reads the same files. That convention must survive any deletion.
- **README guard-class table (README:292)** already carries the row this pass answers: *"SPDK-served namespace (`storage nvmeof share --spdk`) — PR support exists in SPDK's nvmf target — probe decides…; validated in OQ 4's scope (SPDK differs from kernel-nvmet)"*. §4 below is the measured answer.
- **QUICKSTART §4** documents the whole verb surface incl. the SPDK lifecycle as the blessed fabric runbook. Claims to revisit in the design loop: `/opt/spdk` master-build install, 2GB/4GB hugepage guidance, unsupervised background `nvmf_tgt` start.
- **`tests/dev_substrate.sh`:** self-contained bash over configfs; **does not go through nvmeof.rs** (own nvmet/port/namespace plumbing, devsub-prefixed, port id 52026). Dev/test uses are not product uses.
- **`docs/design-metadata-throughput.md`** §89/§398 already records "SPDK-for-targets: separate program" and points at this scoping.

---

## 2. Delete-vs-keep recommendation — kernel-nvmet config path

**Recommendation: DELETE the kernel-nvmet *target-serving* path when SPDK target serving becomes mandatory** (`share_target`, the configfs branch of `unshare_target`/`list_nvmeof`, the losetup plumbing, and the shares registry in its current form). Rationale under the forward-only house lens:

1. **Dev/test uses are not product uses.** The only working kernel-nvmet consumer in this repo is `dev_substrate.sh`, which never touches nvmeof.rs and stays kernel nvmet-loop by decision. Deleting the product path does not touch the dev substrate.
2. **The path is demonstrably unexercised**: registry persistence broken-as-root since inception (1.4), loop-device bookkeeping physically impossible on real configfs (1.1), zero real-kernel tests, `clippy::all` allowed, dead pub fn + dead error variant. Keeping it means adopting and hardening code that has never had a production user.
3. **Refuse-loud beats silent fallback** (house norm): a `share` without SPDK present should say "kernel-nvmet target serving was removed; SPDK is the supported target — run `squeezefs storage nvmeof spdk-…`" rather than silently standing up a second, weaker fabric with different PR semantics (kernel nvmet: no PTPL — §4).
4. **Two target stacks = two PR behavior matrices** for the M1 guard to document and test. Mandatory-SPDK collapses the support matrix to one enforcement-grade story — and §4 shows SPDK is the *better* PR citizen (PTPL) while also being the *stricter* one (Register semantics), so pretending both stacks are interchangeable would be actively wrong.

Keep (explicitly):
- `connect`/`disconnect`/the initiator half of `list` — client side, in scope to stay (kernel initiator).
- `ensure_nocow_backing` — already guards the SPDK path; its tests are real.
- The `/etc/nvme/hostnqn|hostid` identity convention (guard dependency).
- The `spdk-*` lifecycle verbs as the seed of the managed-target story — rebuilt per §5 (version pinning, config persistence, supervision, ptpl/uuid-aware add_ns), not as-is.

Also delete regardless of the nvmet decision (house no-dead-code): `extract_nvmeof_connection_details`, `SqueezefsError::NvmeOfBackend`, the `#![allow(clippy::all)]` header (fix or rewrite what it hides), and the self-truncating registry (SPDK's native `save_config`/`load_config` — used successfully in §4 step 10 — makes a homegrown registry redundant).

**The decision itself belongs to the design loop.**

---

## 3. Target A/B on this box (the core measurement)

### 3.1 Provenance

- SPDK **v26.05** (tag), commit `d519b163cbc0e2f28c35d9bc86d610da368b032c`, built locally at `/var/tmp/spdk-scoping/spdk` with `./configure --disable-tests --disable-unit-tests --disable-examples`, gcc 16.1.1, `taskset -c 0-15 make -j16` (~35 s wall), **never installed system-wide**.
- Hugepages: prior value **0** (2M and 1G) → reserved **1024 × 2M = 2 GiB** (≤ 4 GiB cap) → **restored to 0** (§6).
- Rig (all objects manifest-recorded, `spdkscope` NQNs, port ids 52470/52471; zram0 = user swap, never touched): three 4 GiB **zram** devices (hot_add, indexes 1–3) prefilled with incompressible data; served as:
  - **spdk-tcp**: `spdk_tgt -m 0x1000000` (1 reactor, core 24) `-s 1024`, `bdev_aio` over the zram node, NVMe/TCP 127.0.0.1:4460.
  - **nvmet-tcp**: kernel nvmet subsystem, `device_path` = zram node, NVMe/TCP 127.0.0.1:4461.
  - **nvmet-loop**: same class, loop transport — informational reference row (today's dev-substrate class).
  - Same backing class + same kernel initiator (`nvme connect -t tcp` / `-t loop`, default queue counts) in all arms.
- fio 3.42, `ioengine=io_uring, direct=1, numjobs=1, ramp 2 s, runtime 10 s, norandommap, randrepeat=0`, railed `--cpus_allowed=0-7`; **n=3 per row, medians reported**; runs serialized; quiet-gated (loadavg < 3.0 net of the poller + Tctl < 80 °C; Tctl logged per run; one gate pause observed at 84.6 °C after a loop-write row — cooled to 50 °C before the next run; never ≥ 86 °C sustained). spdk_tgt was **SIGSTOPped during the kernel arms** so its busy-poller could not pollute them. Instrument statement (house rule): **fio-3.42/io_uring against the raw connected namespace — no squeezefs in the data path.**
- CPU accounting per run: whole-system busy CPU-seconds (`/proc/stat` delta over the ~13.4 s window incl. ramp) + spdk_tgt process CPU-seconds (`/proc/<pid>/stat` delta).
- **Contamination event (recorded per the multi-run discipline):** the first spdk-tcp arm invocation was cancelled by a 300 s harness timeout; its detached fio children overlapped the restarted run's first row. The overlapped rows (rand4k-read-qd32 at 30 k/33 k/91 k + three stray seq-write rows) were quarantined to `summary.csv.contaminated` and the row was **re-measured cleanly ×3 from zero** after verifying no stray load (`rerun-row.sh`). The aborted count had also produced three tight pre-overlap readings of the same row (175–178 k IOPS, p50 ≈ 155 µs); they are *declared signature data, not acceptance* — the accepted figures below are the clean count (126 k ×3, p50 236 µs). The gap is attributed to association state: between the counts the initiator's bench connection rode out several keep-alive losses (SIGSTOP windows), so the two counts saw different controller-association lifetimes. Both counts agree qualitatively (spdk-tcp ≥ nvmet-tcp on this row); the write rows measured identically in both counts (45.3–45.7 k).

### 3.2 Results (medians of 3; IOPS, p50/p99 µs; CPU = busy CPU-seconds per ~13.4 s run window)

| Row | Arm | IOPS | BW MB/s | p50 µs | p99 µs | sys CPU s | spdk_tgt CPU s |
|---|---|---:|---:|---:|---:|---:|---:|
| rand4k read QD32 | **spdk-tcp** | **126,409** | 493 | 236 | 872 | 80.4 | 11.0 |
| | nvmet-tcp | 112,568 | 439 | 284 | **387** | 63.4 | — |
| | nvmet-loop *(ref)* | 601,298 | 2,348 | 50 | 63 | 87.3 | — |
| rand4k write QD32 | **spdk-tcp** | **45,469** | 177 | 692 | **888** | 71.3 | 13.0 |
| | nvmet-tcp | 37,278 | 145 | 872 | 1,581 | 66.9 | — |
| | nvmet-loop *(ref)* | 435,908 | 1,702 | 71 | 113 | 223.1 | — |
| rand4k read QD1 | spdk-tcp | 57,263 | 223 | 11 | 19 | 71.3 | 11.5 |
| | **nvmet-tcp** | **101,797** | 397 | **5** | **12** | 67.5 | — |
| | nvmet-loop *(ref)* | 180,132 | 703 | 2 | 10 | 64.2 | — |
| rand4k write QD1 | spdk-tcp | 27,597 | 107 | 30 | 35 | 73.3 | 12.3 |
| | **nvmet-tcp** | **35,859** | 140 | **23** | **29** | 67.6 | — |
| | nvmet-loop *(ref)* | 41,970 | 163 | 21 | 28 | 64.7 | — |
| seq128k read QD8 | **spdk-tcp** | 18,434 | **2,304** | 403 | 823 | 73.9 | 12.3 |
| | nvmet-tcp | 11,253 | 1,406 | 667 | 1,417 | 66.7 | — |
| | nvmet-loop *(ref)* | 201,433 | 25,179 | 34 | 57 | 140.9 | — |
| seq128k write QD8 | spdk-tcp | 1,600 | 200 | 4,947 | **7,110** | 67.0 | 13.2 |
| | **nvmet-tcp** | **1,903** | **238** | 4,882 | 8,978 | 79.6 | — |
| | nvmet-loop *(ref)* | 11,978 | 1,497 | 610 | 1,011 | 148.6 | — |

Readings:
- **Queued rows (the daemon's shape — QD32, seq QD8 reads): SPDK wins** — +12 % rand-read QD32, +22 % rand-write QD32 (and p99 888 µs vs 1,581 µs — 1.8× tighter tail), +64 % seq-read throughput.
- **Latency floor (QD1): kernel nvmet-tcp wins** — 5 µs vs 11 µs p50 read, 23 µs vs 30 µs write. The kernel target completes inline in softirq context; the single spdk reactor adds a hop at these depths. QD1 is not the production shape, but it is a real row.
- **seq-write over TCP collapses on both arms** (200–238 MB/s vs the loop reference's 1.5 GB/s zram ceiling) — a transport/backing interaction, arm-symmetric, not an SPDK-vs-nvmet differentiator on this rig.
- **nvmet-loop reference** confirms the standing dev-substrate posture: the loop class is 4–30× faster than either TCP arm; it remains the right *speed* substrate for barrier-bound measurement, while the SPDK-TCP tier is about *target-behavior* fidelity, not speed.

### 3.3 Per-core honesty

- spdk_tgt ran **one reactor pinned to core 24**, burning **0.82–0.99 of that core during load** (11.0–13.2 CPU-s per 13.4 s window; write rows highest). SPDK's default reactor **busy-polls at ~100 % of its core even when idle** (by design; the rig SIGSTOPped it during the kernel arms rather than measuring its idle spin as noise). One dedicated core is the entry price per target node under the default scheduler.
- Whole-system busy CPU per row (fio + initiator + target, all arms same box): spdk-tcp rand4k-read-QD32 ≈ 6.0 cores vs nvmet-tcp ≈ 4.7 cores. Per-core-honest throughput: **~21.0 k IOPS/system-core (spdk) vs ~23.9 k (nvmet)** on that row — SPDK's absolute wins on queued rows come from its dedicated poller, not from lower total system cost; at localhost scale the two are within ~15 % on system efficiency while SPDK holds the tail-latency and throughput edge.
- Multi-core reactor scaling was **not** measured (single-core mask only); it is a design-loop question (§7 Q5).

---

## 4. PR / PTPL matrix (the hard gate question) + writer-guard smoke

Probed with kernel nvme-cli against the SPDK guard namespace exactly what `reservation.rs` does (`pr-matrix.sh`, full transcript `/tmp/spdkscope/results/pr-matrix.txt`). Attribution note baked into the rig: native NVMe multipath folds every association to a subsystem into one head node, so cross-host phases were **serialized** (one live association at a time) — the same subtlety any future automated PR suite must handle.

| Probe (what the M1 guard checks) | kernel nvmet (M1 record) | SPDK v26.05 (measured) |
|---|---|---|
| Identify NS `RESCAP` (byte 31) | ≠ 0 (PR supported), **PTPL bit 0** | **0xff** — all seven capability bits incl. **bit0 PTPL-capable** |
| Register (RREGA=0) + IEKEY + CPTPL=11b | accepted; IEKEY behaves replace-ish (M1: re-register idempotent) | **accepted** (rc=0); Report shows registrant, **`ptpls:1`** |
| Acquire Write-Exclusive (rtype=1) | works | **works** (`rtype:1`, `rcsts:1`) |
| Holder write | works | **works** |
| Non-holder write (2nd hostnqn/hostid) | fenced | **fenced** — `dd: error writing … : Invalid exchange` = **EBADE, the exact errno class `is_reservation_conflict()` pins** |
| Preempt (RACQA=1, victim key) | works (TTL-stale takeover path) | **works** — victim unregistered, new holder writes, `gen` bumps |
| Registration survives host disconnect | yes (M1: stale registration conflicts next register) | **yes** — reservation+registration intact with zero live associations |
| Reservation Report form | extended (EDS) required on fabrics | **same** — EDS report used throughout |
| **PTPL across target power cycle** | **no** (nvmet PTPL=0; guard heals via `writer_guard_pr_reacquires`) | **YES** — spdk_tgt SIGKILL → relaunch → `load_config`: holder key/rtype restored from the `ptpl_file` (`rtype:1`, same rkey, `ptpls:1`, `cntlid:65535` = no live association) |
| Release semantics | release + unregister leaves zero residue | **same** — post-release+unregister Report: `regctl:0` |

Two SPDK-specific behaviors the design loop must own:

1. **PTPL state binds to the namespace UUID.** Re-adding a namespace with the same `ptpl_file` but a re-created bdev (fresh auto UUID) is refused loud: `Existing bdev UUID is not same with configuration file / Subsystem restore reservation failed`. Fix: pin `-n nsid -u uuid` at `add_ns` (the rig does; `nvmeof.rs` today does not) and persist via `save_config`/`load_config` — which worked verbatim in step 10.
2. **SPDK is spec-strict on Register.** RREGA=0 (+IEKEY) from a host that already has a (different-key) registration returns **Reservation Conflict**, where kernel nvmet accepted the guard's IEKEY re-register. This is what broke the crash-remount (below).

### Writer-guard smoke (the "does M1 work unchanged on SPDK targets" answer)

Real volume on the SPDK guard namespaces (`guard-smoke.sh`, transcript `/tmp/spdkscope/results/guard-smoke.txt`): `squeezefs format sqmeta:///dev/nvme4n1 sqdata:///dev/nvme4n2` → mount `--daemon --allow-other` → IO → kill -9 → remount → clean unmount.

| Step | Result |
|---|---|
| format + mount on SPDK-served namespaces | **works** (v3 format, cache-less) |
| `.stats` guard fields (path `metrics.*`) | **`writer_guard_mode: ["flock+pr"]`** — enforcement grade; `writer_guard_fenced: [0]`, `writer_guard_pr_reacquires: [0]`, `meta_volume_atomicity: ["cow-checksummed"]`, `meta_volume_atomicity_physical: ["atomic4k"]` |
| basic IO + sync | works (8 MiB write/read-back) |
| kill -9 → `squeezefs clients` | correct classification: `writer … dead (reclaimable)`, client record still live |
| **kill -9 → remount** | **FAILS today**: `reservation register failed on the PR-capable namespace: Invalid exchange (os error 52)` — the dead incarnation's registration+WE reservation persist (same hostid), and SPDK's strict Register conflicts where nvmet replaced. Loud, fail-closed — but dead-pid reclaim does not complete. **This is the one guard change mandatory-SPDK requires.** **[FIXED 2026-07-17** — `fix/guard-pr-register-ladder` (`register_ladder` in `reservation.rs`): green ×10 restart-from-zero on this rig, plus PTPL survive-restart; evidence `.benchmarks/2026-07-17-guard-pr-register-ladder.md`. **Correction to the row above:** the fix session re-measured kernel nvmet (7.1.3, `resv_enable=1`) with the *different-key* re-register the guard actually issues after kill -9 — it conflicts there too (the M1 "IEKEY behaves replace-ish" note covered only same-key idempotency), so the P0 was both-stack, not SPDK-only, and the ladder + a semantics *probe* (not per-arm assumptions) now govern both.**]** |
| adapted flow, executed manually | Report → recognize **own hostid** + dead-pid claim → `resv-register RREGA=1 (unregister) crkey=<stale own key>` (drops the WE reservation with it) → remount: **succeeds**, fresh key registered + WE re-acquired, `flock+pr` again, data intact (md5 verified) |
| clean unmount | **zero PR residue** (`regctl:0` — release+unregister exactly as `reservation.rs::release()` intends) |
| `claim clear` while client registration live | correctly **refused** ("live client registrations") |
| `claim clear` after clean unmount | "no writer claim present — nothing to clear" |

**Verdict: the guard's probe, enforcement, fencing errno, preempt, release, and stats surface all work unchanged on SPDK targets — and gain PTPL (cross-host enforcement now survives target power cycles, upgrading the README guarantee-class row). One targeted change is required: the register ladder must handle SPDK-strict Register by detecting *our own* stale registration in the Report and unregistering it (or using RREGA=REPLACE) before registering the fresh key — same-host-only, never for foreign hostids (that stays the preempt/TTL path).**

---

## 5. Ops story notes

- **spdk_tgt lifecycle:** RPC-built state is volatile — a restart loses subsystems/bdevs unless re-applied. `save_config` → `load_config` round-trips everything including `ptpl_file` + pinned ns UUIDs (proven in §4 step 10) and obsoletes the homegrown share registry. Two viable owners: a **systemd unit** (`Restart=always`, `ExecStartPost=rpc.py load_config`) or a **squeezefs-supervised child** (the `mount --daemon --supervise` precedent). Scoping lean: systemd unit generated/managed by squeezefs verbs — target lifetime should not couple to any one daemon, and the box already runs user daemons that way; final call is the design loop's.
- **Crash blast radius observed:** killing spdk_tgt drops all its namespaces; kernel initiators enter reconnect storms (`Connect Invalid Data Parameter … Failed reconnect attempt 42/60`, 10 s cadence, ~10 min until `ctrl_loss_tmo` gives up) if a subsystem is not restored. Restore-under-the-same-NQN + `load_config` lets initiators reattach transparently. Initiator-side reconnect knobs stay kernel knobs (client side out of scope) but the target program must document them.
- **Hugepage sizing rule of thumb:** `-s 1024` (1 GiB DPDK mem) comfortably served 5 aio namespaces + TCP transport at the measured rates; reserving **1024 × 2M pages (2 GiB)** gave headroom with zero allocation failures. Scale with transport buffers (`in_capsule_data_size × queue_depth × connections`), not namespace count. Record-prior/restore-after must be part of any managed setup verb (the rig's pattern).
- **Poller core budget vs throughput:** one reactor core sustained 126 k rand-read / 45 k rand-write 4k IOPS and 2.3 GB/s seq-read over localhost TCP at 0.82–0.99 core burn. The same core busy-polls ~100 % idle (default static scheduler) — on storage nodes that is a permanently spent core; SPDK's dynamic scheduler/interrupt mode is the mitigation to evaluate before multi-tenant nodes.
- **Version pinning posture:** pin exact release tags (this pass: v26.05, sha `d519b16`). v26.05 already logs deprecations scheduled for v26.09 removal (`nvmf_namespace_hide_metadata`, sock-callback API) — API churn between releases is real; `spdk_install`'s unpinned master clone is a hazard. Options for the design loop: pinned-tag build script (this pass's `build.sh` shape) vs vendoring (the `third_party/fuse3` precedent) — SPDK's size (~464 MiB tree with submodules) argues for pinned-tag + checksum, not vendoring.
- **Failure modes observed:** (1) `add_ns` reusing a `ptpl_file` against a re-created bdev UUID refuses loud (§4 pt 1 — pin UUIDs); (2) initiator reconnect storms on target death (above); (3) **no spdk_tgt crashes, allocation failures, or OOM** across ~40 min of load, SIGSTOP cycles, and two SIGKILL power-cycles; teardown by PID was instant and clean; (4) fio requires page-aligned-by-default io_uring buffers — no instrument surprises on the raw-device path.
- **Build cost:** ~35 s on 16 railed cores for the target-only configure — cheap enough to build per-release in CI.

---

## 6. Cleanup proof (zero residue)

Before/after snapshots (`snapshot.sh`, `/tmp/spdkscope/snapshot-{before,after}.txt`):

| Dimension | Before (13:42) | Peak (rig) | After (14:39) |
|---|---|---|---|
| hugepages 2M `nr_hugepages` | **0** | 1024 (2 GiB) | **0** (restored) |
| zram | zram0 (user swap) only | + zram1–5 (spdkscope) | **zram0 only**, hot_removed 1–5 |
| nvmet configfs subsystems/ports | empty | 2 subsystems + ports 52470/52471 | **empty** |
| spdk processes | none | spdk_tgt (pids 149620→154929→155518) | **none** (killed by recorded PID) |
| nvme controllers | nvme0 (pcie, user disk) | + nvme1–5 (spdkscope tcp/loop) | **nvme0 only** (all NQNs disconnected) |
| modules loaded by rig | — | nvmet_tcp, nvme_tcp | **unloaded** (nvmet/nvme_loop were pre-loaded, untouched) |
| listening 4420–4699 | none | 4460/4461 | **none** |
| `/etc/nvme/hostnqn|hostid` | pre-existing (2026-06-27 mtimes) | read only | **untouched** (same mtimes) |
| user state (`/mnt/squeezefs`, `/mnt/juicefs`, `~/tmp/nvme`, nullb0, docker) | present | never touched | unchanged |

Intentionally retained: `/var/tmp/spdk-scoping/` (the sanctioned local build tree + `build-info.txt` provenance) and `/tmp/spdkscope/` (session artifacts on tmpfs — gone on reboot). Both outside kernel state.

---

## 7. Ranked open questions for the design loop

1. **P0 — guard register ladder vs SPDK-strict Register** (§4): adopt Report→verify-own-hostid→unregister-stale-own-key→register (proven manually), or an RREGA=REPLACE rung. Must stay same-host-only; foreign keys remain the preempt/TTL path. Tests-first against `FakeReservationClient` taught the strict behavior, then a root-tier acceptance on the rig. **[CLOSED 2026-07-17: landed as `fix/guard-pr-register-ladder` — the Report→unregister-own→register form, matching on the ASSOCIATION's Get-Features host identifier (the `/etc/nvme` files measurably diverge from the wire identity on this box); same-host-only law device-enforced (crkey) AND guard-enforced (fail-closed on any non-own shape). Kill-9 remount green ×10 on BOTH stacks (kernel nvmet measured spec-strict here too — see §4 correction); S1's §8 gate shape satisfied. Evidence `.benchmarks/2026-07-17-guard-pr-register-ladder.md`.]**
2. **spdk_tgt lifecycle owner + config-persistence law**: systemd unit vs squeezefs-supervised child; when is `save_config` written (after every mutating verb?); where does `tgt-config.json` + `ptpl_file`s live (they are now cluster-critical state — backup story).
3. **Namespace identity policy**: pin nsid+UUID at share time (PTPL binding, stable initiator device naming across target restarts); today's `share_target_spdk` pins neither.
4. **Initiator reconnect posture** (documented, not owned): `ctrl_loss_tmo`/`reconnect_delay` defaults vs the meta-backend barrier-failure escalation timing — does a 10-min reconnect window mask or delay `disabled_volumes` fail-stop?
5. **Poller budget & scaling**: cores-per-target-node policy, multi-core reactor scaling curve (unmeasured), dynamic scheduler/interrupt mode for idle nodes, hugepage sizing verb.
6. **Guarantee-class table upgrade**: with PTPL=1, PR enforcement survives target power cycles — README table row and `writer_guard_pr_reacquires` semantics (should stay ~0 on SPDK; growth = PTPL regression signal).
7. **Registry deletion + migration**: replace the (broken) JSON registry with `save_config`; refuse-loud message text for operators with existing kernel-nvmet shares.
8. **Fidelity test tier shape** (charter): generalize `rig-up.sh`/`teardown.sh` into `tests/spdk_target_substrate.sh` (devsub ownership conventions, zram-backed, spdk-tcp + kernel initiator); which suites run on it (guard root-suite + a curated fstests subset, nightly) while general suites stay substrate-agnostic.
9. **Bind/unbind scope**: is PCIe passthrough (`spdk-bind`, vfio) in the mandatory story at all, or is bdev_aio/uring over kernel block nodes the only supported backing for v1? (This pass used aio-over-zram only.)
10. **Version pinning mechanics**: pinned-tag build script + sha verification vs vendoring; deprecation-tracking cadence (v26.09 removals already announced).

## 8. Proposed program shape (sketch — PRs + gate candidates)

- **S1 `fix(meta)`: SPDK-strict PR register ladder** — tests-first (fake gains strict-Register mode; loom not needed — one-shot ioctls), then root acceptance: guard smoke (format→mount→kill -9→remount→clean unmount) green **×10 restart-from-zero** on the SPDK rig. *Gate: the §4 smoke table all-green, `patch` row "remount FAILS" flipped.* **[LANDED 2026-07-17 ahead of the program (product-correctness fix, independent of SPDK-mandatory): gate met ×10 on spdk AND nvmet arms + PTPL survive-restart productized in `guard-smoke.sh`; `.benchmarks/2026-07-17-guard-pr-register-ladder.md`.]**
- **S2 `feat(nvmeof)`: managed SPDK target MVP** — pinned-tag acquire/build verb (replaces master-clone `spdk_install`), spdk_tgt lifecycle (systemd unit or supervised child per Q2), `save_config` persistence, `share`/`unshare` rebuilt on RPC with pinned nsid/UUID + `ptpl_file`, kernel-nvmet serving path deleted with refuse-loud guidance, registry deleted, dead code removed, `#![allow(clippy::all)]` dropped. *Gate: share→connect→format→mount→IO→target-restart→IO-resumes acceptance; clippy clean.*
- **S3 `test`: target-fidelity tier** — `tests/spdk_target_substrate.sh` (devsub conventions), guard root-suite + curated quick fstests subset wired to it at nightly cadence; PR-matrix automated as a root test. *Gate: tier green in nightly; teardown-to-zero-residue proof automated.*
- **S4 `docs/perf`: ops hardening** — QUICKSTART §4 rewrite, README guarantee-table row upgrade (PTPL), hugepage/poller-budget guidance, A/B re-run vs real NVMe (not zram) recorded in `.benchmarks/`. *Gate: spdk-tcp ≥ nvmet-tcp on the QD32/seq rows on the reference substrate (already true on this rig), QD1 delta documented as accepted.*

---

*End of scoping report. Decision authority: the design loop; nothing here is locked.*
