# 2026-08-06 — zcrx engagement round 3: the pool term read out of the driver (striding-RQ standing demand), the whole-NIC release

Branch `perf/zcrx-pool-term` (worktree off `integrate/zcrx-wave` tip
`50045dda`, **unmerged — the orchestrator merges**). Charter: round 2's
discriminator fired outcome (2) exactly — A-sides `starved_ms = 9370`
(≡ 10 × `park_fail_bound`: EVERY failover window starved),
`structural = 5`, `bypasses` 6/0, waits 3200–3366, fill stuck at 0.6 GB,
A 25.6–26.4 vs B 27.9 (residual rent ~5–8 %), and the mlx5 bracket saw
per-queue `rx*_pp_recycle_cache_full` counters reset on zcrx REGISTER
(queue re-creation) with the aggregate growing 1.7 M. Adjudicate the
pool term AT THE DRIVER SOURCE; fix by derivation; adjudicate the
residual rent. Cluster READ-ONLY. Design: `docs/design-zcrx-read-lane.md`
Rev 4b.

Commits: red `9ceaf0b2` (contracts, compile-red at base) · green
`<GREEN_SHA>` (the derivation + the whole-NIC release + Rev 4b + this
note).

Source venue: the sqz **linux-6.19.14** tree (the exact field kernel —
extracted from the `squeezefs-kernel-sqz-work` build volume; the
27-patch series touches io_uring/FUSE only, mlx5 is pristine stable).

## 1. THE DRIVER ADJUDICATION (task 1 — citations)

**Q1: what RQ mode does a zcrx-provider-backed mlx5 queue run?**
STRIDING RQ (MPWQE) + SHAMPO HDS — it does NOT fall back to
legacy/cyclic:

* `mlx5_rq_shampo_alloc` is called from the
  `MLX5_WQ_TYPE_LINKED_LIST_STRIDING_RQ` arm of `mlx5e_alloc_rq`
  (en_main.c:961) and carries the unreadable-MP branch:
  `netif_rxq_has_unreadable_mp(rq->netdev, rq->ix)` (en_main.c:826)
  gives the queue a **separate header page pool** (normal kernel
  pages) while payload strides ride the main pool, which gets
  `PP_FLAG_ALLOW_UNREADABLE_NETMEM` when SHAMPO is on
  ("Shampo header data split allow for unreadable netmem",
  en_main.c:1005–1007) — i.e. the provider path is designed INTO the
  striding+SHAMPO mode.
* The zcrx queue restart reuses the channel params VERBATIM:
  `mlx5e_queue_mem_alloc` copies `chs->params` and calls
  `mlx5e_open_channel` (en_main.c:5561–5601) — REGISTER_ZCRX_IFQ
  changes no geometry. `netdev_queue_mgmt_ops` (en_main.c:5668) has
  members alloc/free/start/stop/get_dma_dev only — **no per-queue
  ring-size hook**, so `ethtool -G` remains device-global (charter
  option (b): NOT expressible on this kernel).

**Q2: what is the standing provider demand?** When fully posted the RQ
holds `pages_per_wqe × wq_size` provider pages (also the page_pool
sizing — `pool_size = rq->mpwqe.pages_per_wqe <<
mlx5e_mpwqe_get_log_rq_size(...)`, en_main.c:940–941; the wq loop posts
every WQE and each UMR maps `pages_per_wqe` pages for the queue's
lifetime). The algebra collapses to a userspace-probeable form:

```
log_rq_size       = log_rq_mtu_frames − log_pkts_per_wqe        (params.c:415)
log_pkts_per_wqe  = log_wqe_sz − order2(linear_stride_sz)       (params.c:292–301)
pages_per_wqe     = 2^(log_wqe_sz − PAGE_SHIFT)                 (params.c:128–149)
⇒ standing_bytes  = 2^(log_frames + log_stride) = FRAMES × STRIDE   (log_wqe_sz CANCELS)
```

* `ethtool -g` rx reports FRAMES: `param->rx_pending = 1 <<
  log_rq_mtu_frames` (en_ethtool.c:372) — the probe the lane already
  runs (`rx_ring_descriptors`).
* `linear_stride_sz = roundup_pow_of_two(MLX5_SKB_FRAG_SZ(headroom +
  hw_mtu))` (params.c:284 via :252–262): `hw_mtu = sw_mtu + hard_mtu`
  (en.h:75), `headroom = NET_IP_ALIGN + MLX5_RX_HEADROOM(=NET_SKB_PAD
  64)` (en.h:79, params.c:227–241), `+ SKB_DATA_ALIGN(skb_shared_info)`
  (320 on x86_64). Total overhead ≤ ~498 B.
* `MLX5_MPWRQ_MAX_LOG_WQE_SZ = 18` (params.c:14) — 64 pages/WQE — is
  the term that cancels; no driver-internal probe needed.

**Field rail arithmetic**: MTU 9000 ⇒ stride = roundup_pow2(~9420) =
**16384**; 8192 frames × 16 KiB = **128 MiB/queue** — vs the round-6
legacy model's 96 MiB (8192 × ⌈9000/4096⌉ × 4096). The 184 MiB round-1
area left the fills 184 − 128 = 56 MiB against a 64 MiB fully-admitted
window (+24 MiB expected slack): **permanently short by ≥ 32 MiB**,
which is exactly "every failover window starved" and the 0.6 GB
trickle. The pp_recycle_cache_full counter resets confirm the queue
re-creation on REGISTER (fresh page_pool per zcrx queue); the 1.7 M
aggregate growth is recycle-path churn on the starved pools, consistent.
The round-6 caveat "striding-RQ fleets over-provision, the safe
direction" is **falsified and retired** — striding demand is LARGER at
jumbo MTU.

## 2. THE FIX (task 2 — probeable, derived, no constants)

`area::ring_standing_bytes(rx_descs, mtu, chunk)` =
**`max(legacy, striding)`**:

* legacy (round 6, kept as the floor): `descs × ⌈mtu/chunk⌉ × chunk` —
  governs small-MTU rails (1500: 2 KiB strides < 1-chunk frames).
* striding (round 3): `frames × mpwqe_stride_bytes(mtu)` where
  `mpwqe_stride_bytes = roundup_pow2(mtu + 512)` — the kernel skb
  arithmetic above ceiled to 512 B on the line (over-estimating
  overhead only rounds UP at a pow2 boundary — the safe direction;
  boundary pins: 3584 → 4096 exact, 3585 → 8192).
* The RQ mode is not portably probeable (mlx5 priv-flags are
  driver-specific strings), so max() is the honest posture; unknown
  MTU keeps the round-6 one-chunk floor with the existing loud warn.

Field effect: area = 64 window + 24 slack + **128 ring** = 216 MiB/
queue (PMD-rounded), ~2.2 GiB across 10 sessions — R5-gauged,
non-sheddable, Red blocks arms (the affordability gate is R5, as
charter option (a) prescribes). Charter option (c) — the honest stop —
is NOT needed: the term is finite and probeable end-to-end.

## 3. THE RESIDUAL-RENT ADJUDICATION (task 3)

* **Did torn sessions restore their RSS mid-row? YES** — per-session
  teardown runs quiesce → `ArmedSteering::restore_now` → lease release
  (initiator.rs teardown; steering.rs restore) — the 5 structural
  teardowns returned their 5 queues to the RSS set within ~2 s of arm.
* **Who paid the residual 5–8 %?** The 5 SURVIVING sessions: their
  pools trickled just under the per-session economics boundary's
  evaluation points, so they held 5 of 32 queues (~16 % width) excluded
  at ~0 engagement for the remaining ~58 s — plus their slow-serve
  admission churn (waits 3200+).
* **Should majority teardown release the whole NIC? YES — landed**
  (`nic_census` + `sweep_nic_sessions`): the pool term is per-NIC
  physics (same driver, ring geometry, MTU for every session on the
  rail), so a structural vote reaching HALF the NIC's ever-armed
  sessions (the same ½ majority derivation as the per-session arm,
  never a fresh constant) sweeps the remaining live sessions from
  inside the voter's teardown (awaited; depth-one recursion by
  construction — a swept peer is never structural itself). Under this
  law the round-2 tape's 5th structural teardown releases all 10
  sessions' width — the residual rent collapses to the first ~2 s.

## 4. Red evidence + verification

Red `9ceaf0b2` compile-red at base (`mpwqe_stride_bytes`,
`ring_standing_bytes(area)`, `nic_census`, the two seams — absent).
Green: `zcrx_lane_tests` **67** (64 + striding-arithmetic + census-law +
whole-NIC-release contracts) — ×10 consecutive; in-module `--lib zcrx`
**38** (the stale round-6 96-MiB pin retired with the law's move to
area.rs — its replacement pins live in the integration suite);
`zcrx_steering_tests` 23; `derivation_sweep_tests` 20; read-path spot
battery green; both clippys `-D warnings` clean; fmt clean; markdown
links clean.

## 5. FIELD EXPECTATIONS

Same instrument (A-B-B-A, `tests/fio/zcrx_field_rows.sh`):

1. **Arm-time**: `zcrx_area_bytes` ≈ 10 × ~216 MiB (~2.2 GiB — R5
   headroom permitting; a Red refusal is loud and the row is INVALID
   for engagement purposes, not a regression).
2. **The pool verdict**: `starved_ms` ≈ 0 and `parks/failovers` ≈ 0 —
   the 128 MiB term covered, fills complete at wire speed. Then
   engagement follows round 1's admission arithmetic: `share ≥ 0.5`
   with `gather ≡ fill` exact, `bypasses ≈ 0`, waits bounded, and the
   composed row read against the 35.5 bar with CPU/byte down.
3. **If starvation persists anyway**: the standing term has ANOTHER
   component the source walk missed — but now the failure is cheap and
   named: the economics arm bounds each session to ~2 windows, the
   majority sweep releases the WHOLE NIC within ~2 s (A ≈ B for the
   rest of the row — the residual rent gone), and `starved_ms`/
   `structural`/`bypasses` say exactly that. The next unknown would be
   measured against a ~2 s tape, not a 60 s one.
4. rand-4k inert-par stays the guard; default-on unchanged (counted
   win only).
