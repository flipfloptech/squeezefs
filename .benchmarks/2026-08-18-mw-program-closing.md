# 2026-08-18 — The FULL MULTI-WRITER program: CLOSING RECORD (rungs 1–19)

**Program**: `docs/design-full-multi-writer.md` (consensus 2026-08-15, 4
review rounds, 28 issues, 0 open) — arm S6–S9, build S10–S11, single-node
client fleets. **Ran**: 2026-08-15 → 2026-08-18, all 19 rungs plus six fix
campaigns; the per-rung landed SHAs are annotated on the design doc's
PR-plan table, and this rung (19, `docs/mw-guarantees`) is the docs-class
close: the operations.md guarantee tables (§5.4 postures, byte-range
custody, S10 delegations, fleet-share sizing), the AGENTS.md + README
scale-claim correction (spec §6.12), the rc-manifest S6–S11 evidence-tier
rows, and the rung-20 residual board (design doc §Program closing).

**The one-line verdict**: SqueezeFS ships one metadata authority + N
coherent readers + N admitted co-writers, with byte-range custody of one
shared file built and proven behind a default-off lever; `format` is
multi-writer-capable by default; every distributed plane is dark until
opted in, and solo mounts are measured indistinguishable from the pre-program
tree. The proving venue was a single box, by charter, and the evidence tiers
below say exactly what that does and does not license.

**Venues** (the standing statements): the `tests/mw_fleet.sh` tcp devsub —
nvmet-tcp on 127.0.0.1 with `resv_enable=1` (a real kernel PR target), 2 mds
null_blk + 2 oss zram, N daemons with per-mount identity, netns/netem RTT
injection, plus a qemu/KVM guest member (6.19.14-sqz — an independent kernel
AND clock domain). Box: 32-core / 117 GiB, host kernel 7.1.6-sqz. Instrument:
`tests/run_mw_matrix.sh` — per-mount stats-inode deltas with mandatory
engagement + R5-pressure columns; a row whose ledgers do not close is
INVALID and exits nonzero.

---

## The verdict table (every gate, with its tier and its note)

| Gate | Verdict | Number | Tier | Evidence |
|---|---|---|---|---|
| **S4 re-gate** — stamped-solo vs unstamped-solo | **PASS** (what licensed the rung-10b default flip) | seq CPU/byte 36.9–37.4 j/GiB all four legs; mdstorm largest phase delta −3.3 % inside the venue's own 4.2 % spread; `dlm_rpcs = 0`, drift 0 everywhere; QUICK set 44 ran / 0 unexpected | measured-real | `.benchmarks/2026-08-15-mw-s4-regate.md`, `.benchmarks/2026-08-16-mw-s4-residuals.md` |
| **S6** — heartbeat off the journal | **GREEN** | N=32, 600 s: 1,868 renewals vs 185 owner-residue journal entries = **0.099 txs/beat**; registration commits 0; readers visible in the census | measured-real | `.benchmarks/2026-08-16-mw-s6-arm.md` |
| **S6** — the self-fence clock law | **GREEN** | `membership_self_fences` exactly 1 on the victim (SIGSTOP+netem AND the qemu hung-kernel pause), fence-then-rejoin never resume-as-live, `T_self 42955 < T_owner 45000` from the published gauges | measured-real | same |
| **S7** — device rejection (spec R2 verbatim) | **GREEN — the device rejects** | the resumed zombie's DMA refused in the reservation-conflict class (EBADE), fail-stop latch + custody poison, PR preempt observed on the target | measured-real | `.benchmarks/2026-08-16-mw-s7-arm.md` |
| **S7** — kill-9 ×10 + oracle | **GREEN 10/10 from zero** | `fsck_findings = 0` and `meta_kv_block_refs_drift = 0` after EVERY kill (the engaged C8 ledger) | measured-real | same |
| **S8** — the R1 honesty row ("published even if it regresses") | **PUBLISHED — it regresses, exactly as R1 priced** | serial `tar -x`: 0.60× / **0.14×** / 0.046× of authority-local at RTT ≈0 / 250 µs / 1 ms, 100 % rtt-attributed, coalesce 1.00 | measured-real | `.benchmarks/2026-08-16-mw-s8-arm.md` |
| **S8** — the K=5 crucible | **GREEN from zero** | ~5.3 M shipped mutations, `local_commit_refusals = 0` everywhere, dedup exactly-once under injected session kills, the era split with ZERO silent old-era survivors, fsck+C8 clean after a co-writer kill-9 | measured-real | same |
| **S9** — fan-out / failover / co-located fencing | **GREEN** (fencing gates + engagement; throughput columns provisional — foreign load) | engagement exact (ships ≡ serves, frees shipped, amp columns 1.34–1.41× attributed); failover: successor in 1 s, 2/2 fenced, **zero acked-data loss** (sha256 corpus byte-identical through successor AND re-admitted co-writers) | measured-simulated (correctness gates load-independent) | `.benchmarks/2026-08-16-mw-s9-arm.md` |
| **S9 finding #6** — the shipped-publish era gate + witness | **CLOSED** (publish schema 5) | s9-colocated-fence GREEN ×3 from zero (pre-fix: C1 divergent chain + 220 C8 findings, drift 48,620) | measured-simulated | `.benchmarks/2026-08-16-mw-publish-era-gate.md` |
| **Rung 10b** — the default-format flip | **LANDED** | bare `format` stamps the nine bits (`0xffd7` word incl. bits 7–15); `--single-writer` = the pre-flip class byte-identically; contradictory pair refuses | — (format act, gated on the rows above) | `.benchmarks/2026-08-16-mw-default-flip.md` |
| **Rung 10c** — fleet-parallel maintenance (KD-MW-16) | **GATE MET** | fsck wall-clock 1.00× / 1.92× / **2.50×** at N=1/2/4 (gate ≥ 2.4 = 0.6×-linear); census covered exactly once at every width; kill-9 mid-shard re-leases with zero double-coverage, zero PR preempts on read shards | measured-simulated | `.benchmarks/2026-08-17-mw-fleet-jobs.md` |
| **S10** — the tar-x gate (`≤ 1.10× of S0 at 250 µs`) | **NOT MET — the honest product statement governs** (the charter's own alternative) | **6.73×** (14.87 s vs 2.21 s authority-local, A-B-B-A < 1 % both orders, engagement exact); structurally unmeetable while ONE node holds every volume's D0 claim — no client-ownable slot exists. Spec R1's fallback, verbatim: *remote clients are throughput-oriented; latency-sensitive metadata work runs on the owner* | measured-real | `.benchmarks/2026-08-17-s10-slot-placement.md` (ladder inputs: `…-s10-delegation.md` — the read row 26 hits / 0 ships; `…-s10-update-intents.md` — +12 % e/s, −17 % wire verbs/entry, OQ-2 priced ~2 ms and not reopened) |
| **S11** — the MPI-IO acceptance row (spec's S11 gate) | **MET — the verdict is ISSUED** | A-B-B-A shared vs file-per-proc ≥ 0.8× in BOTH brackets: **1.411× / 2.273×** (32 ranks, one shared file, pinned ior 4.0.0); engagement exact, `cap_refusals` 0, demotions 0; read-back exact (zero data-check errors); cold fsck findings 0 / C8 drift 0 | measured-simulated | `.benchmarks/2026-08-18-s11-widthn-refs-fix.md` (the leg is rung 18's, unmodified: `.benchmarks/2026-08-18-s11-mpiio-row.md`) |
| **S11** — block-cyclic (Issue-19 live) / tiny-ranges / demotion / kill matrix | **GREEN** | grants ≈ blocks with ZERO cap refusals, 1.52–2.5× the disjoint control, fsck ×3 clean; 4,096 unaligned writes → 2 grants + 15 extensions, foreign latency 1.02×; `demotions ≡ acks`, sub-block price ≈ 3 ms authority CPU/extent (the D1 exception, priced); holder/authority kills oracle-green every round | measured-simulated | `.benchmarks/2026-08-18-s11-mpiio-row.md` |
| **Solo re-gate** (every rung) | **HELD** | `dlm_rpcs == 0`, every `dlm_custody`/`meta_ship` field 0/off by construction on unarmed mounts; one relaxed load per hook | measured-real | each rung's own gates section |

**The 15 k arithmetic, restated at close** (tier:
arithmetic-on-measured-constants — the formula published per
`docs/rc-manifest.md`): the S9-a row measured 3.6 publish verbs/MiB of
co-writer ingest and S8-a measured one authority serving 9,473 wire verbs/s
at the veth floor ⇒ **aggregate co-writer ingest through ONE authority
≈ 2.6 GiB/s, independent of writer count** (data bytes never funnel —
device-direct DMA). That bound is the wall the fleet-of-authorities recipe
(residual board item 3) exists to move; no 15 k row was measured and none is
claimed.

## The rungs, at a glance (SHAs on the design-doc table)

| Phase | Rungs | State |
|---|---|---|
| A — foundations | 1 (client identity), 2 (per-mount hostnqn + `fabric_endpoint:`), 3 (co-location audit), 3b (fleet share), 4 (loom cores), 5 (one-act stamping + S4 gate), 5b (sqz-kernel 0030 host-scoped fabric subsystems — the rung-6 STOP finding, adjudicated), 6/6b (fleet rig + VM leg) | all landed 2026-08-15/16 |
| B — arm & prove | 7 (S6), 8 (S7), 9 (S8), 10 (S9), 10b (default flip), 10c (fleet jobs) | all landed 2026-08-16/17; every row above |
| C — S10 | 11 (recall valve — the brake before the engine), 12 (LOOKUP delegations), 13 (UPDATE intents), 14 (slot placement + the gate adjudication) | landed 2026-08-17; gate = the honest statement |
| D — S11 | 15 (range wire, ships dark), 16 (B4 clause + fast-path tax), 17 (authority assembler), 18 (the §9.5 rows), 19 (this close, + the width-N fix campaign in its window) | landed 2026-08-17/18; verdict MET |

## The findings ledger (the crucible doctrine's own scoreboard)

Every count below is the note's own, red-first per the repro-port mandate;
the six fix campaigns are footnoted on the design doc. Sum: **≈ 60 product
findings** convicted by the program's own rows and rigs — none by quiet
testing.

| Note | Product findings |
|---|---|
| S4 residuals | 2 (staging-flock teardown race; scoreboard sudo-mount blindness) |
| S6 arm | 4 product + 1 rig (headline: a frozen member resumed as a live member — the S6-b′ falsifier was live; the fleet-share cgroup-arm divide) |
| S7 arm | 4 (headline: the device's reservation-conflict rejection was not classified as the fence it IS; phantom claim-set writers under crash loops) |
| S8 arm | 5 (headline: the first real co-writer mount DESTROYED its authority's live WERO hold through the merged multipath head) |
| S9 arm | 6 (headline: the custody renewal cadence doubled every cycle until the lease died; finding #6 → its own campaign) |
| Era-gate campaign | the #6 pair (no era gate on layout publishes; no idempotence witness) |
| Default flip | 3 audit catches + 1 pre-existing standing red recorded |
| Fleet jobs | 1 (partial-census persist) + the loom-core campaign's grant-vs-revoke orphan window (rung 4) |
| Delegation | 4 (headline: attr triples are not a sound dentry-set version — the crucible falsified the token within minutes) |
| Intents | 4 + FOUND the pre-existing tar+rm C8 leak |
| C8 campaign | 1 (reclaim vs the open rewrite epoch) |
| C10 campaign | 2 (the fleet detector mirage; the format-time root-nlink law) |
| Range wire | 3 (headline: the fd-less truncate lease strand starving every range acquire) |
| Assembler | 3 composition bugs + the characterized zeros residual |
| Zeros-interleave | 1 (the un-armed production geometry source) + the zeros-dependence FALSIFIED |
| MPI-IO rows | 9 (6 product + 3 dangling-take faces; headline: the §6.2-item-9 divergence refusal latched terminal in three coats) |
| Width-N campaign | 4 (the family: fork latch / swapped-pair refs / global-ino identity — its own first cut's regression, caught by its own instruments / indirect-head hole) |

## The residual board

Lives in ONE place — `docs/design-full-multi-writer.md` §Program closing,
"The rung-20 residual board (ordered)" — 13 items collected from every
note's own residual section. The top of the board: (1) the indirect-map
width-N composition (the ≥ ~6 GiB shared-file boundary refuses fail-safe —
fsync EIO, never corruption; the blob-aware owner-side merge is the work,
the ≥ 750 MiB/s-probe MPI-IO leg is the acceptance); (2) the
`SQUEEZEFS_RANGE_CUSTODY` default flip it gates; (3) per-volume claim
admission — the fleet-of-authorities recipe that unlocks the S10 gate and
moves the 2.6 GiB/s single-authority wall.

## This rung's own gate (docs-class)

Markdown link/anchor check (`tests/check_markdown_links.sh`) — PASS; no
`.rs`/`.toml`/test/script touched (the change-class law: no cargo gate).
Files: `docs/operations.md` (the §5.4 posture + pair tables, §Byte-range
custody, §Subtree delegations & UPDATE intents, §Fleet-share sizing,
capacity extensions, the pre-flip staleness retired), `AGENTS.md` +
`README.md` (§6.12 scale correction + the D9 stamping-phrasing sweep; the
bit-numbering sweep verified every bit number against
`src/meta_backend/kv/superblock.rs` — the specific bit-8/9 staleness the
charter cites was already corrected by the `8ac11062` pull-forward),
`docs/rc-manifest.md` (§2 tiers, §3 ledger post-flip, §3b1 S6–S11 rows),
`docs/design-full-multi-writer.md` (per-rung SHAs, fix-campaign footnotes,
§Program closing + the residual board), and this note.
