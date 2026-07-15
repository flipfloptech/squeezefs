# IOPS-parity investigation, Phase 1 — reproduction + decomposition (2026-07-15)

**Charter**: metadata-throughput closing report residual row 5 ("the iops-parity
investigation"), user mandate: *JuiceFS ~500k IOPS vs SqueezeFS ~70k* on
`elbencho -r --rand -t 16 -b 4k --iodepth 16 --direct <16 files>` (16 threads ×
iodepth 16 = 256 in-flight 4 KiB random reads, O_DIRECT). Phase 1 is
**measurement-only** (env-knob experiments allowed, zero product-code change):
reproduce both classes, decompose where JuiceFS's number comes from and where
SqueezeFS's ceiling binds, deliver a ranked lever board.

## Provenance

| | |
|---|---|
| Tree / binary | dev @ `ae92c47` (full metadata program in), `cargo build --release` md5 `a6ba63d7a6f79656fed407ad4864c61d` |
| Box | AMD RYZEN AI MAX+ PRO 395, 32 possible / 25 online CPUs, capped 3.5 GHz (performance governor), kernel `7.1.3-2-cachyos`, 109 GiB RAM, PC SN8000S 2TB NVMe, /home = btrfs |
| Rails | `taskset -c 0-15` timed rows; daemons `systemd-run --user --scope -p MemoryMax=8G` (SqueezeFS; JuiceFS 24G — tmpfs cache pages charge the memcg, deviation noted); kills by PID; `/mnt/squeezefs` + `~/tmp/nvme/*.nvme` untouched; Tctl 42–66 °C all session (≤ 88 rail never approached) |
| Sandbox | `~/tmp/iops_parity_3456308/` (volumes, results, logs, harness — preserved). SqueezeFS: house shape, 1 GiB `sqmeta.img` + 4×8 GiB `sqdata*.img` on /home, staging declared at format, mount `--read-mem-cache-size 1G --write-mem-cache-size 1G`. JuiceFS `1.5.0-dev+2026-07-14.292f44cd` (the survey clone's lineage): `sqlite3://` meta + `file://` object store on /home + `--cache-dir` on tmpfs `--cache-size 20G` (the user's own mounts use a tmpfs cache dir `/tmp/juicefs`; their real stack is redis-container meta + rustfs-container S3 — my `file://` objstore *skips the HTTP hop*, i.e. flatters JuiceFS) |
| Dataset | 16 × 1 GiB files, `elbencho -w -t 16 -s 1g -b 1m --direct` — the user's own shell history shows `-s 1G` for these files. Timed rows add `--timelimit` (15–30 s) to the user's line; elbencho steady-state IOPS ("LAST DONE") is the row value |
| Box-state honesty | the user's **own live run** (their elbencho against their real `/mnt/squeezefs`, 4 meta vols on `~/tmp/nvme`) was executing at session start and was sampled **passively** (proc-only); it ended before my timed rows. A pytest co-tenant (~1.5 cores) overlapped a few rows — flagged DIRTY in `results/*.env`; all headline rows re-ran quiet |
| User's live scenario (passive, DIRTY) | their daemon served **~39,900 × 4 KiB device reads/s at 1.16 cores** (`/proc/<pid>/io` + diskstats over 20 s) — the "~70k class" is real and **device-true** on their substrate; no `SQUEEZEFS_*` env knobs in their daemon (stock transport) |

## The decomposition matrix

Every cell: user's exact elbencho line (±timelimit) unless noted. "device r/s" =
nvme0n1 reads/s during the row (diskstats delta); SqueezeFS daemon
`/proc/<pid>/io` read_bytes confirms 1.00× amplification (device-true) on every
O_DIRECT row.

### JuiceFS

| Row | IOPS | device r/s | What served it (counter evidence) |
|---|---:|---:|---|
| **Warm** (user's scenario: whole 16 GiB in tmpfs disk-cache) | **269,443–272,554** | **0–76** | `juicefs_blockcache_hits` +4.26 M ≈ ops — **100 % local-cache serve, zero object-store GETs**; daemon **15.7 cores** (58 µs CPU/op) |
| Warm, bare (no taskset — user's exact shape) | 259,294 | 12 | same |
| Cold (cache wiped + `drop_caches`; 30 s blended) | 198,883 | 14,015 (803 MiB/s) | **self-warming**: 19.2 GB of 4 MiB-block GETs in the first seconds re-fill the cache; 96 % of the row's ops were already blockcache hits |
| `--cache-size 0`, kernel page cache available | 222,258 | 32,728 (864 MiB/s) | the `file://` objstore itself rides the **kernel page cache** — "cache disabled" isn't |
| **`--cache-size 0` + 2 G cage (forced device-serve)** | **53,587** | 56,425 (**2,441 MiB/s = 11.7× amplification**) | object GET 12.1 GB per 6.1 GiB user reads — 4 KiB random on 4 MiB objects is the pathological shape for their chunk model |
| 10 G disk cache < 16 G dataset (first-gen config) | 54,202–56,706 | 52,755–56,516 (**≈4 GB/s**) | miss-heavy: every miss fetches whole blocks; cache churns |
| Sparse files (hole-serve hypothesis for the user's now-empty volume) | 110,534 | ~0 | metadata-only zero-fill, CPU-bound at 16.1 cores |

### SqueezeFS (all rows device-true 1.00× unless "hot-tier"/"buffered")

| Row | IOPS | device r/s | daemon cores | Notes |
|---|---:|---:|---:|---|
| **Stock** (QD=4, kernel `max_background`=12) | **34,489–58,194** over 7 quiet rows, median **44,148** | = IOPS (1.00×) | 1.1–1.9 | the user's ~70k class (their faster substrate); sampled device inflight **1–8**, fuse `waiting` pinned at **256** |
| `Q_DEPTH=16` | 86,074–97,698 | 1.00× | 2.5–2.9 | +2.2× |
| `max_background=256` alone (QD=4, via fusectl) | 30,338 | 1.00× | 0.9 | **no effect alone** |
| **`Q_DEPTH=16` + `max_background=256`** | **247,438–274,065** (×3) | 1.00× | 6.3–6.5 | the interaction is the lever |
| **`Q_DEPTH=32` + `max_background=256`** | **316,124** | 1.00× (1,286 MiB/s) | 7.3 | **7.2× stock**, RSS 790 MiB (payload buffers), fits the 8G cage |
| `Q_DEPTH=32` + `max_background=64` | 146,212 | 1.00× | 4.2 | mb scales the row once QD is open |
| Buffered (no `--direct`), page-cache-warm | 78,540 | 42,331 | 1.8 | regular opens carry no `FOPEN_KEEP_CACHE` → kernel drops file pages on each open; page cache only partially serves |
| **Hot-tier warm O_DIRECT** (768 MiB set inside the 1 G RAM tier), QD32+mb256 | **416,436–492,391** | **0** | 7.0 | **the 500k class on SqueezeFS** — 14 µs CPU/op, zero device work (O_DIRECT serves from resident tier; only *publish* is skipped by PR4) |
| Hot-tier warm O_DIRECT, stock knobs | 326,470 | 0 | 5.1 | R-10 lineage control (294k @ 8t in the closing report) reproduced at user shape |

### Raw substrate control (same 16×1 GiB shape, btrfs /home, no FUSE)

| Row | IOPS | Note |
|---|---:|---|
| t16 × iodepth16 (256 in-flight) | **608,418–727,612** | the substrate ceiling; ~420 µs/op at full queue |
| t16 × iodepth1 (16 sync) | 38,400 | this box's 4k-random latency is high at low concurrency — concurrency IS the game on this substrate |

## Attribution — where SqueezeFS's stock ceiling binds

**The binding constraint is (a) the in-flight cap — delivered transport
concurrency — not per-op cost.** Evidence:

1. **Little's law**: stock 44k × ~420 µs substrate latency ⇒ effective
   concurrency ≈ **18** of the 256 offered (sampled: device inflight 1–8,
   `/sys/fs/fuse/connections/<m>/waiting` = 256 — the queue sits **in the
   kernel**, upstream of the daemon). Opened up (QD32+mb256): ≈ 133. Raw: 256.
2. **Two kernel-side gates, multiplicative, both must open**:
   `SQUEEZEFS_FUSE_OVER_IO_URING_Q_DEPTH` (default 4/CPU-queue; code clamps
   1..32; queues are pinned at kernel possible CPUs = 32 here — note AGENTS.md's
   "min(nproc,8)" line is stale vs `fuse_over_uring.rs:617`) and the FUSE
   connection's `max_background` (INIT-negotiated **12**, vendored-fuse3
   `abi.rs:39` `DEFAULT_MAX_BACKGROUND`, `congestion_threshold` 9 — gates async
   DIO). QD16 alone: 2.2×. mb256 alone: 1.0×. Together: **6–7×**.
3. **Not daemon CPU** (1.1 cores stock, 25 available), **not the device** (raw
   608k same shape/box), **not handler wall** (`SQUEEZEFS_OP_PROFILE=1`: READ
   p50 ≤ 128 µs, p90 ≤ 1 ms, handler_to_backend/backend_to_reply p50 ≤ 1 µs
   — the op wall is the device fetch itself), **not read-path per-op cost**
   (hot-tier rows: 326k stock knobs / 492k opened — per-op resolution+transport
   supports the 500k class today when serves are RAM-resident).
4. Residual per-op transport fat exists but is secondary: strace over the stock
   row ≈ 8.3 syscalls/op (1.28 `io_uring_enter`, 1.67 eventfd `write`, 1.0
   `epoll_wait`, 1.0 `read`, plus a `pipe2+splice+vmsplice+2×close+fcntl` block
   on ~⅓ of replies and `statx` 0.34/op) — L3 fodder, consistent with the
   M4→M10 "transport residual" hand-off.
5. Session-noise honesty: two mid-ladder rows collapsed to 18–20k with device
   inflight ~1 (persisting across remounts, all QDs), then recovered — the
   rand4k-per-op report's documented ±2× drive-state noise class on this
   substrate. The knob attributions above rest on within-mount A/Bs and the
   repeated quiet rows, not on any single row.

## JuiceFS's 500k, decomposed

- **Warm (their design point): 100 % local-cache serve.** `--direct` on
  JuiceFS bypasses the **kernel page cache for the FUSE file only**; their RAM
  buffer + disk cache (tmpfs, in the user's config!) still serve. Counter
  proof: blockcache_hits ≈ ops, object GETs 0, device 0. The 500k-class number
  is a **RAM round trip**, and costs them 15.7 cores (58 µs CPU/op vs
  SqueezeFS's 14 µs on its RAM row).
- **Forced to the device** (cache 0 + tight cage): **53.6k at 11.7× read
  amplification** — *below* SqueezeFS stock, *5.9× below* SqueezeFS opened-up,
  on identical hardware. Their 4 MiB-object chunk model is hostile to true
  4 KiB random device reads; ours is purpose-built for it (R3 ranged reads,
  0.98–1.00× amplification).
- The user's live JuiceFS volume is now empty (objstore 48 KiB, redis dbsize
  11): their exact 500k row isn't re-runnable; its class is reproduced here at
  260–273k on the 3.5 GHz-capped box (their measurement predates the cap-state
  I inherited; same serve mechanism regardless — device ≈ 0 is the signature).

## Semantics verdict (the apples-to-apples question)

**500k-vs-70k is cache-serve vs device-serve, not a filesystem-efficiency
gap.** SqueezeFS's O_DIRECT is deliberately device-true (read-path PR4
no-publish: bytes at 1.00× from the device, every time); JuiceFS's `--direct`
is kernel-page-cache-bypass with their own caches fully live. Like-for-like on
this box:

| Comparison | JuiceFS | SqueezeFS |
|---|---:|---:|
| True device-serve, user's line | 53.6k (11.7× amp) | 44k stock → **316k** opened-up (1.00× amp) |
| RAM-resident serve, user's line | 260–273k @ 15.7 cores | **326k stock → 416–492k** opened-up @ ~7 cores |

SqueezeFS wins both honest cells once transport concurrency is opened; the
remaining gap to the raw 608k ceiling is per-op transport cost (L3/L4).

## Lever board (ranked)

| # | Lever | Measured / expected win | Effort | Semantics / risk |
|---|---|---|---|---|
| **L1** | **Transport in-flight defaults**: raise over-uring `Q_DEPTH` default 4 → 16–32 for mounts that will see iodepth workloads (env knob exists today), and raise the INIT-reply `max_background`/`congestion_threshold` defaults (vendored fuse3 `DEFAULT_MAX_BACKGROUND=12` → 256-class, or a mount knob; runtime-writable via fusectl as proven here) | **MEASURED: 44k → 316k (7.2×)** on the user's exact line, device-true, ×3 repeats 247–316k; hot-tier ceiling 326k → 492k | **S** (knob + one INIT constant + a default policy; no format/on-disk change) | None to O_DIRECT semantics (still device-true). Memory: payload buffers = queues × depth × ~1 MiB ⇒ QD32 = 1 GiB registered (RSS 790 MiB observed, inside the 8G cage) — needs a sizing policy (e.g. scale depth with `read-mem-cache-size` or a mount flag), and the M3 batch histograms re-checked at depth. `Q_DEPTH>4` is already supported/clamped (1..32); QUEUES must stay = possible CPUs (kernel readiness, `fuse_over_uring.rs:602-627`) |
| **L2** | **O_DIRECT tier-serve parity option** (serve O_DIRECT reads from resident RAM/NVMe tiers *and admit their fills*, JuiceFS-style) — **a user decision, presented not decided**: it trades the read-path program's deliberate device-true O_DIRECT contract (PR4) for cache-serve numbers | Hot-tier rows bound it: **416–492k** for tier-resident data (500k class); with a 16 G dataset it needs `--read-mem-cache-size` ≥ dataset or the NVMe read tier | **M** (mount-scoped opt-in flag + admission-policy change + test sweep; the serve path already exists — O_DIRECT serves resident blocks today, only *admission* skips) | **Semantics change**: O_DIRECT stops being a device-truth/verification tool on that mount; fstests DIO families + `--write-verification` interplay must be re-baselined. Cross-mount coherence unaffected (single-writer guard) |
| **L3** | **Per-op transport cost** (post-M3 residual): the reply-path `pipe2+splice+vmsplice+close×2+fcntl` block (~⅓ of replies — splice fallback taken with `backing` payloads), eventfd write 1.67/op, `pop_timeout` timer park (M4→M10 hand-off), statx 0.34/op | +10–20 % at the opened-up ceiling (316k → ~350–380k est.); bigger share as L1 lands | **M** (vendored fuse3 + session glue; profiling-first per house rules) | None; pure economy. Do after L1 — it's invisible while concurrency-starved |
| **L4** | **LD_PRELOAD interception** (survey P3-A / DAOS libioil shape): ioctl handshake hands {volume identity, fencing token, lease view, block map} to an in-process io_uring reader | Escapes FUSE entirely: raw-control class (**608–727k** here) is the bound; the only lever that closes the last 2× | **L** (new client library + in-process lease/fencing + fallback ladder; prototype read-only first) | Big: single-writer/fencing invariants across two processes; per-fd fallback ladder exactly as DAOS `int_posix.c:99` |
| **L5a** | `FOPEN_KEEP_CACHE` for read handles (data surfaced: buffered re-opens drop the page cache → 78.5k instead of RAM-class) | buffered/repeat-read workloads only; JuiceFS sets it via open-cache | S–M | staleness window vs attr TTLs; irrelevant to `--direct` |
| **L5b** | Docs: AGENTS.md over-uring queue-default line ("min(nproc,8)") is stale vs code (possible CPUs, clamp 512); record the fusectl runtime `max_background` trick in QUICKSTART | hygiene | S | — |

**Recommendation**: land **L1** (defaults + sizing policy + a
`--iodepth-workload`-class mount posture or auto-scale) — it is a 7× measured
win on the user's exact workload with no semantics change; then put **L2** in
front of the user as an explicit semantics decision (their 500k target is
reachable today at 416–492k on tier-resident data, and L1+L2 together match or
beat JuiceFS's warm number at ~½ the CPU); schedule **L3** behind L1 in the
existing transport-residual charter; keep **L4** as the strategic program with
a read-only prototype gate.

## Artifacts

`~/tmp/iops_parity_3456308/` — `row.sh`/`remount_sq.sh`/`ladder.sh`/`sampler.sh`
(harness), `results/*.{elbencho,stats.*,io.*,disk.*,env,inflight}` (every row),
`logs/` (daemon logs incl. the `FUSE-over-io_uring registered: queues=32
depth={4,8,16,32}` arm lines per config). JuiceFS instance artifacts (meta db,
objstore, tmpfs cache) removed at session end; SqueezeFS sandbox volumes
retained with the results for takeover-recovery.
