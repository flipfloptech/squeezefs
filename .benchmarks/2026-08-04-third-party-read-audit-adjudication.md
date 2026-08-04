# Third-party read-path audit — adjudication (2026-08-04)

An external auditor delivered seven read-path optimization proposals. House
law: every claim is adjudicated against code and COUNTED evidence before it
earns a roadmap slot. Verdicts below cite the measuring instrument for each.

| Item | Verdict | Ground truth |
|---|---|---|
| **A2 — ZCRX initiator lane default-on** | **ACCEPT (top priority)** | The auditor's 0.69 CPU-passes/byte softirq figure matches the standing RX-copy ledger (nvme-tcp RX pays one irreducible kernel copy per read byte; TX already zero-copy — the read-vs-write bandwidth asymmetry's named cause). The lane is BUILT behind the D5 gate chain (`docs/design-zcrx-read-lane.md`); promotion was always gated on field validation, and squeeze-test (6.19-sqz, 2×200G) is the venue. Scheduled as the next perf campaign after the 2026-08-04 integration deploy. |
| **A1 — zero-copy FUSE reply handoff** | **ADJUDICATE, then build the residual** | Partially stale: cold ranged fills ALREADY dest-DMA on kernel rows — the 15:10 cluster battery's randread-kernel shows `read_dest_dma_bytes` +38.7 GB ≈ ranged bytes (zero-copy cold serves live; `fuse3_zc_replies`/kmbuf exist). The remaining 1.00-pass population is WARM tier serves (read_bw-kernel: `read_copy_dest_bytes` 1,218 GB, NT-stored). The buildable item is a tier-buffer lease into the ring commit for warm serves; its win must be re-priced from the cold/warm ledger split, not the auditor's +15–25 % blanket. |
| **A3 + B2 — 512 B LBA geometry (ranged rounding + dest-DMA screen)** | **ACCEPT, bounded, merged** | Two 4096 constants exist (`ipc_direct::LBA`; R3's ranged rounding). Geometry-derived alignment (BLKSSZGET / sysfs at `NvmeBlockDev` init) is exactly the derivation law. BUT `read_copy_bounce_bytes` measures ≈ 0 on every current evidence row (4 KiB-aligned shapes) — the win exists only for sub-4K/unaligned workloads we do not yet benchmark. Gated on building an unaligned-shape row first: no row, no win claim. |
| **B1 — lock-free layout descriptor cache** | **DECLINE (mispriced ~100×)** | The serve prelude is already allocation-free (op-economy campaign: `SQZ_ALLOC_TRACE` allocs/op = 0 pin; PERF-12 borrows bindings from the live map). Measured phase tables (2026-08-04, cluster + local tcp substrate): `key_resolve` 0.8 µs + `meta_resolve` 0.8 µs + `classify_probe` 1.9 µs ≈ 3.5 µs of a 363 µs op (~1 %). The actual rand-4K bottleneck was transport queueing (~700 µs pre-handler), attacked by the same-lane dispatch lever (+22–25 % counted A-B-B-A, `.benchmarks/2026-08-04-transport-same-lane-dispatch.md`). A +20–35 % claim from a ~1 % term is arithmetic-refuted. |
| **B3 — shim reads daemon hot tier via shared memory** | **DECLINE (security boundary)** | The hot tier holds ANY file's blocks; a read-only client mapping bypasses the §5.2 daemon fd screen — a client process could read blocks of files it cannot open (the class that made `.stats` owner-only, VAL-7a). Client-side coherence (eviction/incarnation) is also unsolved without the seqlock ceremony crossing the trust boundary. The landed economics already serve warm hits in one ring round trip (§5.5.1 fast path + hold probe + completion doorbell). A capability-filtered per-session window would be a different, large design — not this item. |
| **C1 — dynamic SQPOLL + "scale depth up to 32"** | **DECLINE (contradicted by measurement)** | SQPOLL on the queue rings is measured NOT recommended (M10, in-tree). Depth already DEFAULTS to 32 (L1 policy; budget-degraded, floor 4) — there is no headroom named by "up to 32". NUMA-aware transport placement landed 2026-07-31 (affinity campaign; queue workers node-scoped). |

## Sequence adopted

1. (in flight) 2026-08-04 integration: known-bugs sweep + transport lever 1 →
   gate → deploy → battery.
2. **ZCRX D5 field validation on squeeze-test** (A2) — the read-bandwidth
   ceiling item.
3. A1 cold/warm copy-ledger adjudication row; build the warm-serve handoff if
   the residual prices in.
4. A3+B2 unaligned-shape benchmark row, then the geometry-derived LBA item.

Declines are recorded here for the auditor's next pass; re-open any of them
with a counted row that contradicts the citations above.
