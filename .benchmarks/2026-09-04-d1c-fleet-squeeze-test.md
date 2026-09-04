# D-1c same-binary lever A-B-B-A — fleet brackets on `squeeze-test`

Three 4-leg brackets (lever 0,1,1,0) of
`.benchmarks/rigs/2026-09-04-d1c-fleet-lever-abba.sh` on the sqz-kernel box
`squeeze-test` (hostname `memp-s3ds-aqs-37`), 2026-09-04 19:31–19:43 UTC.
Artifacts: `/tmp/release-1.2/d1c-box/` — per-bracket rig log
(`d1c-abba-<shape>.log`), `<shape>/analysis.txt`, `<shape>/box.log`,
`<shape>/L{0a,1a,1b,0b}.table.txt`, the driver log, the build log, the
aborted first attempt (`*.attempt1-zstd.*`, see §Environment), and
`d1c-rigs-shipped.sha256`.

## Headline

All **12 legs VALID** (ledger closure served ≈ shipped — exact, +0, on
every leg; refusals = owner_panics = 0; every stream rc 0), every leg torn
down to **zero residue**, live mount untouched.

| bracket | shape | ingest GiB/s (L0a / L1a / L1b / L0b) | passes per served frame (off → on) | engagement (`frame_groups` ÷ served frames) | group size (on) | calls/frame |
|---|---|---|---|---|---|---|
| **a. streaming** | 8 cw × 24 streams × 128 MiB | 8.56 / 8.39 / 8.71 / 8.63 — **par** | 0.303, 0.326 → 0.302, 0.299 (−4 %) | 3.3 % / 3.1 % | 1.99 / 1.79 | 1.67–1.71 |
| **b. full-save** | 8 cw × 24 streams × 64 files × 2 MiB | 0.66 / 0.70 / 0.70 / 0.66 — **+6 % on, both orders** | 0.303, 0.334 → 0.289, 0.304 (−7 %) | 4.9 % / 4.7 % | 2.29 / 2.28 | 1.42–1.45 |
| **c. full-save, `SHIP_DEPTH=1`** | as b + `SQUEEZEFS_PUBLISH_SHIP_DEPTH=1` | 0.70 / 0.71 / 0.70 / 0.70 — **par** | 0.456, 0.428 → 0.381, 0.385 (−13 %) | 6.9 % / 7.2 % | 2.93 / 2.94 | 1.67–1.72 |

Read: the lever engages on **3–7 % of served frames** on this box (the
frame-fill term — calls/frame 1.4–1.7 means most frames carry a single
`SetLayoutAndSize`, and a one-call frame has nothing to group), so the
rung's `passes/frame → 1.0` cannot be read here: the two-stage conveyor
already co-queues single calls from MANY frames, so the venue number is
0.30–0.46 passes per served frame with the lever off and the lever removes
only the passes its 3–7 % of multi-call frames would have split into
(−4 / −7 / −13 %, consistent in both orders of every bracket). Where it does
engage, group size is 1.8–2.9 txs per group. Ingest is par on the streaming
row and par-or-up on the full-save rows (+6 % A-B-B-A-consistent on b, par
on c); authority CPU and journal entries per publish are flat within noise.
Same qualitative verdict as the laptop rows (engagement bounded by
calls/frame 1.5–2.4 → 3–10 % of frames there).

## Identity

| Item | Value |
|---|---|
| Branch / commit | `perf/d1c-conveyor-group-per-frame` tip **`be98a5db5506c218c2fdd8d46ab345a04b4492e7`** — `docs(audit): D-1c evidence note, fleet lever rig, ladder row + operator surface`; `88e84d9a` (dev) verified as an ancestor (rebased) |
| Source transport | `/tmp/release-1.2/sqz-d1c.bundle` → scp → `git fetch <bundle> perf/d1c-conveyor-group-per-frame:d1c && git checkout --detach d1c` in `/scratch/tmp/sqz-agent/src-1.2`; tree clean at build time |
| Binary (both legs of every bracket) | `squeezefs 1.2.0 (be98a5db5506 / be98a5db5506c218c2fdd8d46ab345a04b4492e7) built 2026-09-04T19:26:20Z profile release` — `cargo build --release`, default features, 2 m 04 s, 0 warnings; copied to `/scratch/tmp/sqz-agent/squeezefs.d1c` (=`BIN`) |
| Toolchain | rustc 1.98.1 / cargo 1.98.1 (channel `stable`, scratch-homed rustup from the earlier gate work) |
| Box / kernel | Rocky 8.10, **6.19.14-sqz**, 32 CPUs, 251 GiB RAM, `nvme_core.multipath=Y`, `fabrics_host_scoped_subsystems` param present; 5b probe recorded **MERGED** (co-located co-writers share the PR host identity — the harness's stated honest-residual shape) |
| Substrate | tcp devsub instance `mwfleet` (isolated: `nvmet-tcp` on `127.0.0.1:54143`, nvmet port id 52055, NQN prefix `nqn.2026-07.io.squeezefs:devsubtcpmwfleet-`), 2 mds (memory-backed `null_blk`) + 2 oss (`zram`, **64 GiB virtual each, `lzo-rle`** — see §Environment), created and torn down per leg by `tests/mw_fleet.sh` |
| Fleet | `create N=1 --multi-writer --cowriters=8` → 1 authority (m0) + 8 co-writers (m50–m57), 17 fleet mounts total per the ledger; `SQZ_MWFLEET_MNT_ROOT=/scratch/tmp/sqz-agent/mwfleet-mnt` (root fs is 100 % full) |
| Rig scripts | The bundle's `be98a5db` has **no `FILES` knob**; the brackets b/c need it, so the two rig scripts were copied from the laptop's **uncommitted working tree** after the build (so the binary's git stamp stays clean): `2026-09-02-d1b-fleet-row.sh` sha256 `9b191e95…320ebb`, `2026-09-04-d1c-fleet-lever-abba.sh` sha256 `2db809e2…935c42` (`d1c-box/d1c-rigs-shipped.sha256`). Analyzer `2026-09-04-d1c-fleet-analyze.py` unchanged from the commit; compiles on the box's Python 3.6.8. The box checkout therefore shows exactly those two files as ` M`. |
| Loadavg at bracket starts | a: 0.48 1.62 0.88 (idle) · b: 21.87 8.62 3.48 (a's tail) · c: 52.18 44.90 21.30 (b's tail — the 1-min figure is the fleet's own 8×24 dd streams + 17 daemons, not foreign load; the box had no other work) |
| Bracket wall | a 132 s · b 297 s · c 292 s |

## Bracket a — streaming (COWRITERS=8 STREAMS=24 MB=128, defaults)

Rig log `d1c-box/d1c-abba-streaming.log`; per-leg loadavg at leg start: L0a 0.48 · L1a 0.43 · L1b 17.39 · L0b 9.86.

Row lines (verbatim, `<leg>/table.txt`):

```
== L0a
aggregate co-writer ingest 8.56 GiB/s over 2.8 s (24576 MiB)
client: shipped 43777, frames 26220 -> frames/publish 0.599, calls/frame 1.67, depth waits 5880, session dials 24
owner : served 43777, served_frames 26220, served_chains 31509 (chains/frame 1.20), conveyor passes 7938 -> passes/publish 0.181, journal entries 9846
row VALID (closure within instrument skew, tripwires flat)
== L1a
aggregate co-writer ingest 8.39 GiB/s over 2.9 s (24576 MiB)
client: shipped 43709, frames 25537 -> frames/publish 0.584, calls/frame 1.71, depth waits 6153, session dials 24
owner : served 43709, served_frames 25537, served_chains 30856 (chains/frame 1.21), conveyor passes 7720 -> passes/publish 0.177, journal entries 9730
row VALID (closure within instrument skew, tripwires flat)
== L1b
aggregate co-writer ingest 8.71 GiB/s over 2.8 s (24576 MiB)
client: shipped 42934, frames 25569 -> frames/publish 0.596, calls/frame 1.68, depth waits 5977, session dials 24
owner : served 42934, served_frames 25569, served_chains 30456 (chains/frame 1.19), conveyor passes 7654 -> passes/publish 0.178, journal entries 9367
row VALID (closure within instrument skew, tripwires flat)
== L0b
aggregate co-writer ingest 8.63 GiB/s over 2.8 s (24576 MiB)
client: shipped 43480, frames 25464 -> frames/publish 0.586, calls/frame 1.71, depth waits 5930, session dials 24
owner : served 43480, served_frames 25464, served_chains 30816 (chains/frame 1.21), conveyor passes 8312 -> passes/publish 0.191, journal entries 9891
row VALID (closure within instrument skew, tripwires flat)
```

`analysis.txt` (verbatim):

```
== L0a: wall 2.8 s, authority CPU 2.46 s (sqz-meta 0.92, sqz-jrnl 0.50, other incl. RPC lanes 1.01)
  RUNG: served frames 26220 (1.67 calls/frame, 1.20 chains/frame), conveyor passes 7938 -> PASSES PER SERVED FRAME 0.303 (passes/publish 0.181)
  groups: 0 commits carrying 0 txs (group size 0.00), frame_groups 0 of 26220 served frames (lever OFF / not engaged)
  conveyor: rho(apply) 0.215, size-1 batches 92 % of 7938, tx_queue_wait 185 us (n=9795), pass_total 76 us, window_total 299 us
  authority: served publishes 43777, S8 verbs 2697 -> 16577 verbs/s; journal entries 9846 (0.225/publish)
  co-writers: publish_phase_ns.total mean 17.7 ms, meta_ship rtt mean 1.32 ms, shipped 43777 (closure vs served: +0)
== L1a: wall 2.9 s, authority CPU 2.46 s (sqz-meta 0.94, sqz-jrnl 0.50, other incl. RPC lanes 1.00)
  RUNG: served frames 25537 (1.71 calls/frame, 1.21 chains/frame), conveyor passes 7720 -> PASSES PER SERVED FRAME 0.302 (passes/publish 0.177)
  groups: 834 commits carrying 1662 txs (group size 1.99), frame_groups 834 of 25537 served frames (ENGAGED)
  conveyor: rho(apply) 0.182, size-1 batches 90 % of 7720, tx_queue_wait 152 us (n=9676), pass_total 67 us, window_total 344 us
  authority: served publishes 43709, S8 verbs 2674 -> 16218 verbs/s; journal entries 9730 (0.223/publish)
  co-writers: publish_phase_ns.total mean 23.1 ms, meta_ship rtt mean 1.51 ms, shipped 43709 (closure vs served: +0)
== L1b: wall 2.8 s, authority CPU 2.35 s (sqz-meta 0.89, sqz-jrnl 0.48, other incl. RPC lanes 0.95)
  RUNG: served frames 25569 (1.68 calls/frame, 1.19 chains/frame), conveyor passes 7654 -> PASSES PER SERVED FRAME 0.299 (passes/publish 0.178)
  groups: 782 commits carrying 1401 txs (group size 1.79), frame_groups 781 of 25569 served frames (ENGAGED)
  conveyor: rho(apply) 0.210, size-1 batches 91 % of 7654, tx_queue_wait 146 us (n=9314), pass_total 76 us, window_total 249 us
  authority: served publishes 42934, S8 verbs 2584 -> 16513 verbs/s; journal entries 9367 (0.218/publish)
  co-writers: publish_phase_ns.total mean 26.9 ms, meta_ship rtt mean 1.53 ms, shipped 42934 (closure vs served: +0)
== L0b: wall 2.8 s, authority CPU 2.46 s (sqz-meta 0.93, sqz-jrnl 0.50, other incl. RPC lanes 0.99)
  RUNG: served frames 25464 (1.71 calls/frame, 1.21 chains/frame), conveyor passes 8312 -> PASSES PER SERVED FRAME 0.326 (passes/publish 0.191)
  groups: 0 commits carrying 0 txs (group size 0.00), frame_groups 0 of 25464 served frames (lever OFF / not engaged)
  conveyor: rho(apply) 0.205, size-1 batches 92 % of 8312, tx_queue_wait 122 us (n=9835), pass_total 69 us, window_total 257 us
  authority: served publishes 43480, S8 verbs 2923 -> 16685 verbs/s; journal entries 9891 (0.227/publish)
  co-writers: publish_phase_ns.total mean 26.2 ms, meta_ship rtt mean 1.43 ms, shipped 43480 (closure vs served: +0)
```

### Verdict columns — bracket a

| column | L0a (off) | L1a (on) | L1b (on) | L0b (off) | read |
|---|---|---|---|---|---|
| passes per served frame | 0.303 | **0.302** | **0.299** | 0.326 | on ≈ off (−4 % vs the off mean 0.315); the venue ratio is already ≪ 1 |
| group size / `frame_groups` | 0 / 0 | 1.99 / 834 of 25537 (3.3 %) | 1.79 / 781 of 25569 (3.1 %) | 0 / 0 | engaged on the on legs only; bounded by calls/frame |
| `pass_total` mean / ρ(apply) | 76 µs / 0.215 | 67 µs / 0.182 | 76 µs / 0.210 | 69 µs / 0.205 | par; ρ 0.18–0.22 (the authority is far from the D-1b note's 0.97 on this 32-core box) |
| aggregate ingest GiB/s | 8.56 | 8.39 | 8.71 | 8.63 | **par** (on mean 8.55 vs off 8.60, −0.6 %) |
| verbs/s (authority) | 16 577 | 16 218 | 16 513 | 16 685 | par (−1.6 %) |
| authority CPU s (meta / jrnl / other) | 2.46 (0.92/0.50/1.01) | 2.46 (0.94/0.50/1.00) | 2.35 (0.89/0.48/0.95) | 2.46 (0.93/0.50/0.99) | par |
| journal entries / publish | 0.225 | 0.223 | 0.218 | 0.227 | unchanged |
| calls/frame (frame-fill) | 1.67 | 1.71 | 1.68 | 1.71 | the term that bounds engagement |
| validity | VALID, +0 | VALID, +0 | VALID, +0 | VALID, +0 | |

Note the co-writer publish latency mean rose on the on legs (17.7 → 23.1 / 26.9 ms) but L0b also reads 26.2 ms, so it is leg order / warm-up, not the lever.

## Bracket b — full-save (MB=2 FILES=64, STREAMS=24, COWRITERS=8)

Rig log `d1c-box/d1c-abba-fullsave.log`; row rig line: `8 co-writers x 24 streams x 64 files x 2 MiB (dd bs=1M conv=fsync, /dev/zero)` (the `table.txt` header prints the per-stream TOTAL, 128 MiB = 64 × 2). Per-leg loadavg at leg start: L0a 21.87 · L1a 51.47 · L1b 59.04 · L0b 52.45.

Row lines (verbatim):

```
== L0a
aggregate co-writer ingest 0.66 GiB/s over 36.5 s (24576 MiB)
client: shipped 182222, frames 127935 -> frames/publish 0.702, calls/frame 1.42, depth waits 13629, session dials 24
owner : served 182221, served_frames 127934, served_chains 153597 (chains/frame 1.20), conveyor passes 38715 -> passes/publish 0.212, journal entries 62104
row VALID (closure within instrument skew, tripwires flat)
== L1a
aggregate co-writer ingest 0.70 GiB/s over 34.4 s (24576 MiB)
client: shipped 172593, frames 121018 -> frames/publish 0.701, calls/frame 1.43, depth waits 14512, session dials 24
owner : served 172593, served_frames 121018, served_chains 147365 (chains/frame 1.22), conveyor passes 34923 -> passes/publish 0.202, journal entries 61014
row VALID (closure within instrument skew, tripwires flat)
== L1b
aggregate co-writer ingest 0.70 GiB/s over 34.4 s (24576 MiB)
client: shipped 174013, frames 122399 -> frames/publish 0.703, calls/frame 1.42, depth waits 14094, session dials 24
owner : served 174013, served_frames 122399, served_chains 147845 (chains/frame 1.21), conveyor passes 37241 -> passes/publish 0.214, journal entries 62745
row VALID (closure within instrument skew, tripwires flat)
== L0b
aggregate co-writer ingest 0.66 GiB/s over 36.4 s (24576 MiB)
client: shipped 172442, frames 118848 -> frames/publish 0.689, calls/frame 1.45, depth waits 13660, session dials 24
owner : served 172442, served_frames 118848, served_chains 145235 (chains/frame 1.22), conveyor passes 39706 -> passes/publish 0.230, journal entries 61784
row VALID (closure within instrument skew, tripwires flat)
```

`analysis.txt` (verbatim — the analyzer emitted no `co-writers:` line on the full-save legs):

```
== L0a: wall 36.5 s, authority CPU 13.09 s (sqz-meta 5.79, sqz-jrnl 2.06, other incl. RPC lanes 4.53)
  RUNG: served frames 127934 (1.42 calls/frame, 1.20 chains/frame), conveyor passes 38715 -> PASSES PER SERVED FRAME 0.303 (passes/publish 0.212)
  groups: 0 commits carrying 0 txs (group size 0.00), frame_groups 0 of 127934 served frames (lever OFF / not engaged)
  conveyor: rho(apply) 0.104, size-1 batches 75 % of 38715, tx_queue_wait 151 us (n=61858), pass_total 98 us, window_total 279 us
  authority: served publishes 182221, S8 verbs 64069 -> 6750 verbs/s; journal entries 62104 (0.341/publish)
== L1a: wall 34.4 s, authority CPU 13.08 s (sqz-meta 5.83, sqz-jrnl 2.07, other incl. RPC lanes 4.45)
  RUNG: served frames 121018 (1.43 calls/frame, 1.22 chains/frame), conveyor passes 34923 -> PASSES PER SERVED FRAME 0.289 (passes/publish 0.202)
  groups: 5983 commits carrying 13685 txs (group size 2.29), frame_groups 5937 of 121018 served frames (ENGAGED)
  conveyor: rho(apply) 0.104, size-1 batches 71 % of 34923, tx_queue_wait 168 us (n=60747), pass_total 103 us, window_total 375 us
  authority: served publishes 172593, S8 verbs 64072 -> 6879 verbs/s; journal entries 61014 (0.354/publish)
== L1b: wall 34.4 s, authority CPU 12.86 s (sqz-meta 5.60, sqz-jrnl 2.09, other incl. RPC lanes 4.45)
  RUNG: served frames 122399 (1.42 calls/frame, 1.21 chains/frame), conveyor passes 37241 -> PASSES PER SERVED FRAME 0.304 (passes/publish 0.214)
  groups: 5861 commits carrying 13345 txs (group size 2.28), frame_groups 5809 of 122399 served frames (ENGAGED)
  conveyor: rho(apply) 0.114, size-1 batches 73 % of 37241, tx_queue_wait 155 us (n=62475), pass_total 106 us, window_total 311 us
  authority: served publishes 174013, S8 verbs 63870 -> 6906 verbs/s; journal entries 62745 (0.361/publish)
== L0b: wall 36.4 s, authority CPU 12.71 s (sqz-meta 5.54, sqz-jrnl 2.06, other incl. RPC lanes 4.36)
  RUNG: served frames 118848 (1.45 calls/frame, 1.22 chains/frame), conveyor passes 39706 -> PASSES PER SERVED FRAME 0.334 (passes/publish 0.230)
  groups: 0 commits carrying 0 txs (group size 0.00), frame_groups 0 of 118848 served frames (lever OFF / not engaged)
  conveyor: rho(apply) 0.103, size-1 batches 76 % of 39706, tx_queue_wait 131 us (n=61513), pass_total 94 us, window_total 305 us
  authority: served publishes 172442, S8 verbs 64024 -> 6492 verbs/s; journal entries 61784 (0.358/publish)
```

### Verdict columns — bracket b

| column | L0a (off) | L1a (on) | L1b (on) | L0b (off) | read |
|---|---|---|---|---|---|
| passes per served frame | 0.303 | **0.289** | **0.304** | 0.334 | on mean 0.297 vs off 0.319: **−7 %** |
| group size / `frame_groups` | 0 / 0 | 2.29 / 5937 of 121018 (4.9 %) | 2.28 / 5809 of 122399 (4.7 %) | 0 / 0 | engaged; 13.7 k / 13.3 k txs grouped |
| `pass_total` mean / ρ(apply) | 98 µs / 0.104 | 103 µs / 0.104 | 106 µs / 0.114 | 94 µs / 0.103 | par (fuller passes cost a few µs more each; ρ flat at ~0.10) |
| aggregate ingest GiB/s | 0.66 (36.5 s) | **0.70** (34.4 s) | **0.70** (34.4 s) | 0.66 (36.4 s) | **+6 % on, in BOTH orders** (wall −2.0 s / −2.1 s) |
| verbs/s (authority) | 6 750 | 6 879 | 6 906 | 6 492 | on +4 % vs off mean |
| authority CPU s (meta / jrnl / other) | 13.09 (5.79/2.06/4.53) | 13.08 (5.83/2.07/4.45) | 12.86 (5.60/2.09/4.45) | 12.71 (5.54/2.06/4.36) | par (±1.5 %) |
| journal entries / publish | 0.341 | 0.354 | 0.361 | 0.358 | unchanged within the leg-to-leg publish-count spread |
| calls/frame (frame-fill) | 1.42 | 1.43 | 1.42 | 1.45 | lower than streaming — smaller files, more single-call frames |
| validity | VALID, served 182221 vs shipped 182222 (+1, instrument skew) | VALID, +0 | VALID, +0 | VALID, +0 | |

## Bracket c — full-save at ship depth 1 (MB=2 FILES=64 + `SQUEEZEFS_PUBLISH_SHIP_DEPTH=1`)

Rig log `d1c-box/d1c-abba-fullsave-depth1.log`. Per-leg loadavg at leg start: L0a 52.18 · L1a 52.24 · L1b 56.89 · L0b 60.20. (`session dials 0` on every leg is the depth-1 control's shape — one stop-and-wait session per authority.)

Row lines (verbatim):

```
== L0a
aggregate co-writer ingest 0.70 GiB/s over 34.4 s (24576 MiB)
client: shipped 156783, frames 91756 -> frames/publish 0.585, calls/frame 1.71, depth waits 32067, session dials 0
owner : served 156783, served_frames 91756, served_chains 136841 (chains/frame 1.49), conveyor passes 41859 -> passes/publish 0.267, journal entries 64071
row VALID (closure within instrument skew, tripwires flat)
== L1a
aggregate co-writer ingest 0.71 GiB/s over 33.9 s (24576 MiB)
client: shipped 170979, frames 102476 -> frames/publish 0.599, calls/frame 1.67, depth waits 33617, session dials 0
owner : served 170979, served_frames 102476, served_chains 148339 (chains/frame 1.45), conveyor passes 39033 -> passes/publish 0.228, journal entries 64966
row VALID (closure within instrument skew, tripwires flat)
== L1b
aggregate co-writer ingest 0.70 GiB/s over 34.5 s (24576 MiB)
client: shipped 167842, frames 100119 -> frames/publish 0.597, calls/frame 1.68, depth waits 33408, session dials 0
owner : served 167842, served_frames 100119, served_chains 145810 (chains/frame 1.46), conveyor passes 38581 -> passes/publish 0.230, journal entries 64546
row VALID (closure within instrument skew, tripwires flat)
== L0b
aggregate co-writer ingest 0.70 GiB/s over 34.5 s (24576 MiB)
client: shipped 165966, frames 96650 -> frames/publish 0.582, calls/frame 1.72, depth waits 33939, session dials 0
owner : served 165966, served_frames 96650, served_chains 142251 (chains/frame 1.47), conveyor passes 41413 -> passes/publish 0.250, journal entries 62767
row VALID (closure within instrument skew, tripwires flat)
```

`analysis.txt` (verbatim):

```
== L0a: wall 34.4 s, authority CPU 11.48 s (sqz-meta 5.20, sqz-jrnl 1.88, other incl. RPC lanes 3.74)
  RUNG: served frames 91756 (1.71 calls/frame, 1.49 chains/frame), conveyor passes 41859 -> PASSES PER SERVED FRAME 0.456 (passes/publish 0.267)
  groups: 0 commits carrying 0 txs (group size 0.00), frame_groups 0 of 91756 served frames (lever OFF / not engaged)
  conveyor: rho(apply) 0.100, size-1 batches 76 % of 41859, tx_queue_wait 109 us (n=63804), pass_total 82 us, window_total 224 us
  authority: served publishes 156783, S8 verbs 63898 -> 6415 verbs/s; journal entries 64071 (0.409/publish)
== L1a: wall 33.9 s, authority CPU 12.96 s (sqz-meta 6.26, sqz-jrnl 1.97, other incl. RPC lanes 3.99)
  RUNG: served frames 102476 (1.67 calls/frame, 1.45 chains/frame), conveyor passes 39033 -> PASSES PER SERVED FRAME 0.381 (passes/publish 0.228)
  groups: 7274 commits carrying 21326 txs (group size 2.93), frame_groups 7080 of 102476 served frames (ENGAGED)
  conveyor: rho(apply) 0.095, size-1 batches 75 % of 39033, tx_queue_wait 120 us (n=64699), pass_total 83 us, window_total 208 us
  authority: served publishes 170979, S8 verbs 64604 -> 6945 verbs/s; journal entries 64966 (0.380/publish)
== L1b: wall 34.5 s, authority CPU 12.05 s (sqz-meta 5.59, sqz-jrnl 1.94, other incl. RPC lanes 3.83)
  RUNG: served frames 100119 (1.68 calls/frame, 1.46 chains/frame), conveyor passes 38581 -> PASSES PER SERVED FRAME 0.385 (passes/publish 0.230)
  groups: 7254 commits carrying 21318 txs (group size 2.94), frame_groups 7160 of 100119 served frames (ENGAGED)
  conveyor: rho(apply) 0.098, size-1 batches 74 % of 38581, tx_queue_wait 117 us (n=64279), pass_total 87 us, window_total 204 us
  authority: served publishes 167842, S8 verbs 63887 -> 6719 verbs/s; journal entries 64546 (0.385/publish)
== L0b: wall 34.5 s, authority CPU 11.79 s (sqz-meta 5.41, sqz-jrnl 1.87, other incl. RPC lanes 3.88)
  RUNG: served frames 96650 (1.72 calls/frame, 1.47 chains/frame), conveyor passes 41413 -> PASSES PER SERVED FRAME 0.428 (passes/publish 0.250)
  groups: 0 commits carrying 0 txs (group size 0.00), frame_groups 0 of 96650 served frames (lever OFF / not engaged)
  conveyor: rho(apply) 0.100, size-1 batches 77 % of 41413, tx_queue_wait 111 us (n=62489), pass_total 83 us, window_total 215 us
  authority: served publishes 165966, S8 verbs 63808 -> 6669 verbs/s; journal entries 62767 (0.378/publish)
```

### Verdict columns — bracket c

| column | L0a (off) | L1a (on) | L1b (on) | L0b (off) | read |
|---|---|---|---|---|---|
| passes per served frame | 0.456 | **0.381** | **0.385** | 0.428 | on mean 0.383 vs off 0.442: **−13 %** (the largest of the three — depth 1 makes frames fuller: 1.7 calls, 1.45–1.49 chains) |
| group size / `frame_groups` | 0 / 0 | 2.93 / 7080 of 102476 (6.9 %) | 2.94 / 7160 of 100119 (7.2 %) | 0 / 0 | engaged; 21.3 k txs grouped per on leg |
| `pass_total` mean / ρ(apply) | 82 µs / 0.100 | 83 µs / 0.095 | 87 µs / 0.098 | 83 µs / 0.100 | par |
| aggregate ingest GiB/s | 0.70 (34.4 s) | 0.71 (33.9 s) | 0.70 (34.5 s) | 0.70 (34.5 s) | **par** |
| verbs/s (authority) | 6 415 | 6 945 | 6 719 | 6 669 | on +4 % vs off mean (the on legs also SHIPPED more publishes — 171 k / 168 k vs 157 k / 166 k — the Lever-B coalescing count moves with timing) |
| authority CPU s (meta / jrnl / other) | 11.48 (5.20/1.88/3.74) | 12.96 (6.26/1.97/3.99) | 12.05 (5.59/1.94/3.83) | 11.79 (5.41/1.87/3.88) | on +7 % mean (L1a +13 %, L1b +2 %) — tracks the higher publish count on those legs (CPU per served publish: 73 / 76 / 72 / 71 µs — par) |
| journal entries / publish | 0.409 | 0.380 | 0.385 | 0.378 | unchanged within the publish-count spread (absolute entries 64.1 k / 65.0 k / 64.5 k / 62.8 k) |
| calls/frame (frame-fill) | 1.71 | 1.67 | 1.68 | 1.72 | |
| validity | VALID, +0 | VALID, +0 | VALID, +0 | VALID, +0 | |

## Cross-bracket reading

* **The rung's number (`passes/frame → 1.0`) is not observable on this venue** for the same reason the streaming laptop rows gave 3–10 %: with a derived ship depth the client's frames carry 1.4–1.7 calls on average, i.e. most frames are single-call and un-groupable, and the two-stage conveyor already co-queues single calls from many concurrent frames (off-lever passes per served frame 0.30–0.46, well under 1). The lever's effect is confined to the 3–7 % of frames that are multi-call, where it groups 1.8–2.9 txs each — and the pass count drops by exactly that share (−4 % streaming, −7 % full-save, −13 % full-save at depth 1, each consistent in both A-B-B-A orders).
* **No row lost**: ingest par on a and c, +6 % on b in both orders; verbs/s par-or-up on every bracket; authority CPU par (per served publish); ρ(apply) 0.10–0.22 on this 32-core box — the authority is not the wall here, so the ingest headline is not expected to move much.
* **Journal entries per publish unchanged** on every bracket (one tx = one entry; the per-publish ratio moves only with the leg's Lever-B coalescing count).
* The engagement instrument behaves as designed: `frame_groups` = 0 on every off leg, > 0 with `group size` ≈ the multi-call frames' calls on every on leg.

## Environment / deviations (all substrate, none product)

1. **First launch aborted at fleet create** (`d1c-box/d1c-driver.attempt1-zstd.log`, `d1c-abba-streaming.attempt1-zstd.log`): `[devsub] ERROR: zram algorithm 'zstd' unavailable (have: [lzo-rle] lzo )` — this sqz kernel's zram was built with only the LZO backends (no `ZRAM_BACKEND_ZSTD`; the `zstd.ko` crypto module exists but 6.12+ zram uses Kconfig backends). The partial devsub (2 mds subsystems, port 52055, 2 zram, 2 nullb items, state dir) was torn down with the harness's own scoped `tests/mw_fleet.sh teardown` → `teardown complete — zero residue`. Re-launched with **`SQZ_DEVSUB_OSS_ALGO=lzo-rle`**; the source is `/dev/zero` so the compressor is not the row's term (label: oss zram lzo-rle, not the rig's default zstd).
2. `SQZ_MWFLEET_MNT_ROOT=/scratch/tmp/sqz-agent/mwfleet-mnt` — the harness's default `/mnt/sqz-mwfleet` sits on the box's **100 %-full root fs** (pre-existing). `/run` (fleet + devsub state, member logs) is tmpfs. The harness scrubs `SQZ_*` from daemon invocations, so the daemons saw only `SQUEEZEFS_PUBLISH_CONVEYOR_GROUP` (and `SQUEEZEFS_PUBLISH_SHIP_DEPTH=1` in bracket c) as intended.
3. `SQZ_MWFLEET_OSS_GB=64` (the rig's default, set explicitly), 2 oss → 128 GiB virtual zram on a 251 GiB box; zeros compress to nothing so RAM use stayed low.
4. Rig scripts with the `FILES` knob came from the laptop's uncommitted working tree (§Identity) — the bundle's committed rigs cannot express brackets b/c.
5. Kernel modules: `null_blk`, `zram`, `nvmet`, `nvmet-tcp`, `nvme-tcp`, `nvme-fabrics` all present as `.ko` for 6.19.14-sqz (`configfs` builtin); `null_blk`/`zram` were loaded on demand by the devsub and remain loaded (refcount 0).

## Live-mount safety and residue (verified)

* The rig/harness scoping was READ before running: `teardown_fleet` kills only ledgered member pids, mounts under `$MNT_ROOT`, daemons whose cmdline matches `squeezefs.*mount.*$MNT_ROOT/`, controllers whose `subsysnqn` starts with the instance NQN prefix, and the instance devsub (`sweep_prefixed_orphans` is prefix-scoped). No `pkill squeezefs`, nothing touches `/dev/nvme*n1`. The live fabric's targets are remote hosts; the box had **0 nvmet subsystems / 0 ports before** and has **0 / 0 after** (`/sys/kernel/config/nvmet/{subsystems,ports,hosts}` all empty).
* Live mount `/scratch/tmp/test`: daemon **849991 continuous** (3 h 51 m before → 4 h 05 m after), fusectl `waiting=0`, `.stats` served, `/dev/nvme*n1` count 15 unchanged; `pgrep -a squeezefs` lists only 849991.
* No `/run/squeezefs-*` dirs, no mounts under the fleet root (the dir itself was removed by the harness), 0 instance controllers; `zram0` (disksize 0) is the module's auto-created default device, not an instance object; `null_blk` configfs holds only `features`. The `/var/lib/squeezefs/nvmeof/shares.json` ledger is still `{"format":1,"shares":[]}`.
* Kept under `/scratch/tmp/sqz-agent/` (now 12 GB; `/scratch` at 12 %): `src-1.2` at `be98a5db` (+ release target), `squeezefs.d1c`, `d1c-abba-{streaming,fullsave,fullsave-depth1}/` (23 MB each: per-leg stats snapshots, tables, create/row/teardown logs), the three bracket logs, `run-d1c.sh`, `d1c-driver*.log`, the attempt-1 dirs, `d1c-build.log`. Nothing outside `/scratch/tmp/sqz-agent/` was removed; nothing installed via dnf. Laptop repo not modified (reads + `scp` from the working tree only).
