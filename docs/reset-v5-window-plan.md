# The reset-v5 reformat window — execution plan (prep 2026-08-04)

**Status: RUNBOOK — prep artifact.** Authored by the reformat-window PREP
campaign (`.benchmarks/2026-08-04-reset-v5-prep.md`); the window itself is a
separate execution campaign. Everything here is ordered, gated, and owed —
nothing in this document has run yet.

**The epoch decision (USER, 2026-08-02):** reset-v5 formats the cluster
**CONVERGED** — 1 meta + 2 data namespaces per node × 5 nodes
(memp-s3ds-aqr-37/38/39 at .191/.192/.193, memp-s3ds-aqs-38 at .195, oss2 at
10.181.177.196) = **5 meta volumes + 10 data namespaces**, every node
symmetric. Dynamic meta routing (`KV_DYNAMIC_ROUTING` bit 6,
[design-dynamic-meta-routing](design-dynamic-meta-routing.md)) makes the
width knob-free — **no `--meta-slots` anywhere** (hard error). The current
store is epoch-locked pre-bit-6 (every post-`8eb3d73` binary refuses it loud
pre-write); the window's reformat is what unblocks every staged post-bit-6
pair.

**The five source notes this plan consolidates** (authority law: if a source
note is amended between prep and window, **the note governs** — re-read all
five at window start):

1. `.benchmarks/2026-08-02-post-reset-baseline.md` — the canonical-baseline
   recipe (§1 venue labeling, §2 the row table, §5 bracketing rules).
2. `.benchmarks/2026-08-03-il-hold-probe.md` §6 — the epoch-blocked il
   hold-probe field acceptance.
3. `.benchmarks/2026-08-04-zcrx-z2.md` §5 — the Z3 reformat-window zcrx-lane
   acceptance script.
4. `.benchmarks/2026-08-04-fuse3-zc-adoption.md` §5 — the kmbuf/4 MiB-write
   field-owed rows.
5. `.benchmarks/2026-08-04-rewrite-program-p0.md` §5 — the rewrite-program
   field-window row manifest.

Plus the v2 kernel obligations from this prep:
`.benchmarks/2026-08-04-sqz-kernel-v2-scoping.md` (patch 0027) — v2 boot +
capability matrix + the generic/634 single-test.

---

## 0. The window pair (KD-7 — build at window start, NOT before)

The window pair = **the dev tip at window start** (the tip is still moving;
do not pre-build). At window start, on the dev box:

```bash
git checkout dev && git pull
task build:rocky8          # dist/rocky8/{squeezefs, libsqueezefs_il.so} — the pairing unit
```

Assert in-container checks pass (`--version` never `unknown`, glibc ≤ 2.28),
record `--version` (train + commit) and both sha256s, stage to
`squeeze-test:/scratch/tmp/{squeezefs,libsqueezefs_il.so}.resetv5` +
`resetv5.sha`, verify byte-exact after transfer, journal the drop. Deploy to
the standard names only inside Phase 2 (retain the outgoing pair as
`.prev`). The dev tip at prep close contains ALL five campaigns' machinery
(hold-probe `dd6c7ea`, zcrx Z2, fuse3-zc/kmbuf `190f88c`, rewrite-P0
`1b197d7`, dynamic routing `f6532fd`) plus this prep's `FUSE_TIME_LIMITS`
daemon arm — so **one pair serves every phase; A/B legs ride levers, not
binary swaps**. The retained per-campaign pairs
(`/scratch/tmp/*.{holdprobe,zcrxz2,fzc}`) are attribution fallbacks only
(divergence-hunting), never row vehicles.

Retained control binaries: `f6532fd` (bit-6-capable pre-hold-probe control —
the il hold-probe note's chosen control; build+stage it ONLY if a
binary-attribution bracket becomes necessary).

## 1. Kernel matrix (which kernel each phase runs on)

| Kernel | Identity | Role in the window |
|---|---|---|
| `7.1.2-1.el8.elrepo` | permanent grub default | the fleet kernel; the fallback if the v2 boot fails; the kernel the standing generic/634 adjudication stays pinned FOR |
| `6.19.14-sqz` (v1) | currently running (one-shot from 2026-08-02) | superseded by v2 this window — do not run rows on it |
| `6.19.14-sqz` **v2** | staged at `/scratch/tmp/kernel-sqz/v2/` (27 patches; 0027 = `FUSE_TIME_LIMITS`) | **the window kernel** — every phase below runs on it unless the abort ladder says otherwise |

**One boot, early** (the one-shot grub discipline serializes kernel swaps;
the v2-scoping ruling: boot v2 WITH the window): Phase 1 installs and boots
v2 as a **one-shot** (`grub2-reboot`; ELRepo 7.1.2 stays the permanent
default — a wedged box self-reverts on power cycle, and ONLY THE USER can
power-cycle). All subsequent phases run on v2. v1→v2 is ABI-identical except
patch 0027, so every v1-proven surface (kmbuf probe, zcrx bind, arm-proof)
transfers.

**Kernel-posture note for rows:** on `-sqz` kernels the window pair
auto-arms **kmbuf** (capability-probed; `fuse3_kmbuf_negotiated=1`) and
recognizes **bit 62** (time limits). The canonical baseline (Phase 3) runs
the DEFAULT posture — defaults are the product; the kmbuf delta is measured
explicitly in Phase 6's A-B-B-A, whose off-leg doubles as the
transport-parity row for any cross-epoch reasoning against the reset-v4
table (measured on 7.1.2, no kmbuf).

## 2. Standing laws (apply to every phase)

* **Labeling:** every row states instrument + shape + substrate + epoch
  (`reset-v5`) + kernel (`6.19.14-sqz2`) + pair commit + lever posture +
  fill/order. A row missing its amplification columns (write rows), ledger
  closure (read rows), or engagement deltas (il rows, ≥ 0.90) is INVALID.
* **Settle hygiene between rows/deploys:** reclaim queue 0 AND
  `meta_kv_pending_free` 0 AND mem level 0 AND `block_free_elided_debt_bytes`
  drained-or-stable AND `rewrite_shadow_open_epochs` 0, ×3 at 1 Hz (the
  baseline recipe + the rewrite-P0 rig additions).
* **A-B-B-A both orders** on anything sharing an aging store; **sustained
  ≥ 60 s flat** (first-third vs last-third) for every headline claim;
  medians of 3 unless the phase states otherwise.
* **Journal discipline:** SESSION START/END + per-row START/DONE lines in
  `/scratch/tmp/agent_runs.log`; every deploy sha-verified and journaled;
  every NIC/sysctl change recorded with its restore; mounts log to
  `/scratch/tmp/logs/sqz.log` (the /tmp ban).
* **Stop-the-line tripwires (any phase):** `writer_guard_fenced`,
  `write_pipeline_fence_drops`, `rewrite_shadow_fence_drops`,
  `block_free_reclaim_fence_halts`, `ipc_sessions_poisoned`,
  `ipc_descriptor_rejects`, `fsck_findings` — any nonzero stops the window
  for diagnosis before more rows run.

## 3. Phase table

| # | Phase | Kernel | Pair / lever posture | Est. wall-clock |
|---|---|---|---|---|
| 0 | Preflight + window-pair build/stage | any | — | 0h45 |
| 1 | v2 kernel install + one-shot boot + capability matrix | 7.1.2 → **sqz2** | no mount during swap | 0h30 |
| 2 | Cluster reset (`cluster_reset_v4.sh`) + mount + arm proof | sqz2 | window pair, defaults | 0h30 |
| 3 | Post-reset-v5 canonical baseline table | sqz2 | window pair, defaults (+A0 legs) | 3h30 |
| 4 | Rewrite-P0 §5 rows | sqz2 | defaults vs `SQUEEZEFS_REWRITE_SHADOW=0 SQUEEZEFS_DISCARD_ELISION=0` | 2h00 |
| 5 | il hold-probe §6 acceptance | sqz2 | defaults vs `SQUEEZEFS_READ_LANE=0` (A0) | 1h00 |
| 6 | fuse3-zc §5 rows (kmbuf + 4 MiB write) | sqz2 | defaults vs `SQUEEZEFS_FUSE_KMBUF=0`; sysctl `fs.fuse.max_pages_limit=1024` vs `SQUEEZEFS_FUSE_MAX_WRITE=1048576` | 2h30 |
| 7 | zcrx Z2 §5 (Z3 lane acceptance) | sqz2 | `SQUEEZEFS_ZCRX_LANE=1` vs `=0` | 1h30 |
| 8 | generic/634 single-test (bit 62 live) | sqz2 | window pair, defaults | 0h15 |
| 9 | Close-out: journal, artifacts, epoch note, restore | sqz2 | — | 0h30 |

**Estimated total: ≈ 13 h** (one long working day; the soaks in 3/4/6/7 are
the bulk — phases 4–8 are order-flexible after 3, so a split across two
sessions cuts at the Phase-4 boundary with the store quiesced).

---

## Phase 0 — Preflight

1. Build + stage the window pair (§0). Verify staged artifacts present and
   sha-clean: `/scratch/tmp/cluster_reset_v4.sh`,
   `/scratch/tmp/kernel-sqz/v2/` (RPM + SHA256SUMS + config),
   the retained campaign pairs, probes (`/scratch/tmp/kernel-sqz/probes/`).
2. Journal SESSION START naming this plan; capture pre-state (`uname -r`,
   grub default + one-shot state, mount state, `nvme list-subsys`, NIC
   feature state on both ports, root-fs headroom — it was 94 % on
   2026-08-02 with the v1 RPM installed; **remove the v1 kernel RPM after
   the v2 boot proves out, not before**).
3. Re-read the five source notes at their dev-tip state (§ authority law).

**Go/no-go:** all staged artifacts sha-verified; box reachable; USER
approval for the destructive Phase 2 confirmed in the session charter.

## Phase 1 — v2 kernel boot + capability matrix

The v1 boot record (`.benchmarks/2026-08-03-sqz-kernel.md` §3/§4) is the
procedure of record; deltas only:

1. Clean-unmount the standing mount; `rpm -ivh --oldpackage` the **v2** RPM
   from `/scratch/tmp/kernel-sqz/v2/` (sha-verify first). Verify the
   permanent grub default is STILL 7.1.2 (kernel-install flips
   `saved_entry` — restore it as the v1 session did), set `grub2-reboot`
   one-shot to the sqz2 entry, journal the reboot line with the wedge
   warning (USER power-cycles; agents cannot).
2. After boot: `uname -r` = `6.19.14-sqz` (v2 RPM identity via
   `rpm -q kernel --last` + the staged sha), run
   `probes/capability_matrix.sh` — expect the v1 matrix (kmbuf PRESENT,
   zcrx surface PRESENT, fuse_uring kallsyms) **unchanged**; re-apply the
   NIC toggles (`rx-gro-hw on`, `tcp-data-split on`, both ports — fresh-boot
   defaults are off) in the zero-traffic window.
3. Fabric reconnect via the product verb; expect the pre-reset topology
   (24/24 at reset-v4 shape) live; iopolicy round-robin.

**Abort ladder:** boot wedge ⇒ USER power-cycle (self-reverts to 7.1.2).
On 7.1.2 the window CONTINUES with **Phase 6 (kmbuf rows) and Phase 8
(generic/634) DEFERRED** and every row re-labeled `7.1.2`; zcrx (Phase 7)
still runs (HDS + zcrx verified available on 7.1.2 with ethtool 6.15 +
hw-gro). A v2 boot that comes up but fails its capability matrix is treated
the same as a boot wedge (do not run rows on a half-proven kernel).

## Phase 2 — the reset (the window's destructive act)

1. Deploy the window pair to the standard names on client AND all five
   storage nodes (`REMOTE_SQZ` path law), sha-verified, journaled.
2. Run `sudo /scratch/tmp/cluster_reset_v4.sh` (staged copy of
   `tests/cluster_reset_v4.sh`): venue banner → YES gate → client
   disconnect (loud) → per-node **enumeration teardown** (ports by
   `addr_traddr` first, then prefixed subsystems — the oss2 lesson) →
   1 m0 + 2 d0/d1 per node → 30 connects → format (5-meta + 10-data URIs,
   NO `--meta-slots`) → mount (`--interception --allow-other
   --log-file /scratch/tmp/logs/sqz.log`).
3. Arm proof on the fresh mount: "FUSE-over-io_uring ready" + queues/depth
   gauges, `fuse3_kmbuf_negotiated=1` (sqz2 default posture),
   `meta_routing_width` = 65536 (derived), `meta_slot_mint_spread` present,
   tripwire set all-zero, `.stats` `build_commit` = the window pair.
4. Journal the epoch line: **reset-v5 executed** (script version, node map,
   namespace count, format flags).

**Abort ladder:** an unreachable node BLOCKS the window (the converged
format needs all five). A teardown residue warning is a fix-in-window item —
the sweep is idempotent; re-run after manual inspection. A format/mount
refusal on the fresh store is a P0 product bug: capture loud, stop.

## Phase 3 — the post-reset-v5 canonical baseline

Re-establish the canonical table per the reset-v4 recipe
(post-reset-baseline §1/§2 — same instrument `tests/fio/run_fio_row.sh` +
census wrapper, same shapes, same reps), labeled
`reset-v5 / 6.19.14-sqz2 / <pair>`:

1. Fill: `fresh_write_pass` nj32 → 16 × 8 GiB (record the fill rate — the
   epoch's first-bytes figure; reset-v4 recorded 30.99 GB/s).
2. Rows (each with its validity columns): read kern qd8 cold (+ NT-A0 leg —
   re-establish the NT attribution on this epoch, baseline rule 3), read
   kern qd32 cold, read il qd8 cold (engagement), write kern fresh/rewrite
   A-B-B-A (now ALSO carrying the Phase-4 `rewrite_amp` columns — the two
   phases share legs where shapes coincide), write il, rand-4k read ×3,
   rand-4k write ×2, the 600 s mixed soak with the ×21 wedge-indicator
   sampling.
3. Flat-120s legs on the headline rows (sustained-state law).

**Go/no-go:** the soak's indicator set all-zero and flat legs decay-free are
the gate for Phases 4–8. A sick epoch (tripwires, decay, engagement
failures) stops the window's perf program — diagnosis owns the box.
Absolute deltas vs the reset-v4 table are EXPECTED (new kernel, kmbuf-armed
transport, 5-meta plane) and are labeled cross-kernel/cross-epoch — the
reset-v5 table brackets only against itself (baseline rule 1).

## Phase 4 — rewrite-P0 §5 (the charter's field acceptance)

Per `.benchmarks/2026-08-04-rewrite-program-p0.md` §5, on the baseline's
fileset:

1. Fresh-vs-rewrite A-B-B-A with the `rewrite_amp` columns: gates **±5 %**
   device-byte rate, `rewrite_amp ≤ 1.05`, **zero mid-row discards**
   (`d_ops == 0`; `block_free_reclaim_elided` ≈ displaced;
   `block_free_trim_*` quiet until idle). Lever legs:
   `SQUEEZEFS_REWRITE_SHADOW=0 SQUEEZEFS_DISCARD_ELISION=0` = pre-campaign
   posture, same binary.
2. Loop-rewrite latest-wins row (hot set, deep qd, time-based):
   `write_pipeline_supersessions` engagement + device/unique + coalesce
   columns; non-overlapping face measured-and-reported (gate arms with
   Idea 8).
3. Zero-mid-row-discards assert on a ≥ 60 s sustained row (flatness law).
4. The 600 s loaded soak (rewrite-publish-drain §7 recipe: fio write +
   8-worker mdstorm + syncfs@10 s) — wedge set all-zero and
   `rewrite_shadow_open_epochs` → 0 at quiesce.

**Abort:** a gate miss here defers THIS phase's verdict (file the counted
rows; the program adjudicates) — it does not block Phases 5–8 unless a
stop-the-line tripwire fired.

## Phase 5 — il hold-probe §6 acceptance

Per `.benchmarks/2026-08-03-il-hold-probe.md` §6 item 1, now that the
post-bit-6 epoch exists:

1. Chartered A-B-B-A: il randread vs kernel randread on the SAME state +
   the A0 leg (`SQUEEZEFS_READ_LANE=0` — hold-probe serves 0 by
   construction) + soak. Acceptance: **il ≥ kernel randread at matched
   state**, engagement exact (`ipc_hold_probe_serves` accounting the row's
   hold serves), kernel rows untouched, warm/seq no-regression.
2. Binary-attribution fallback ONLY if the lever legs diverge
   inexplicably: `f6532fd` control pair vs the window pair (both bit-6;
   the pre-bit-6 `109d7bc` pair CANNOT mount this store — stated in the
   note).

## Phase 6 — fuse3-zc §5 (kmbuf + the 4 MiB-write row)

Per `.benchmarks/2026-08-04-fuse3-zc-adoption.md` §5:

1. kmbuf arm proof: default mount → `buffers=kmbuf-bufring` REGISTER log +
   `fuse3_kmbuf_negotiated=1`; `SQUEEZEFS_FUSE_KMBUF=0` control reads 0.
   Then **SQUEEZEFS_FSTESTS_QUICK on the armed mount** (the bufring arm's
   first real-kernel correctness pass). Any failure: repro-port mandate,
   and the kmbuf rows stop until adjudicated.
2. A-B-B-A kmbuf on/off at qd8 1M seq + rand-4k (medians of 3, both
   orders): throughput + the `commit_flush` phase-delta table + the
   `fio clat − transport_total` residue shift.
3. The 4 MiB-write row: `sysctl fs.fuse.max_pages_limit=1024` + default
   mount (desire = block size) vs `SQUEEZEFS_FUSE_MAX_WRITE=1048576`
   control — `tests/write_matrix.sh` + standing amplification columns +
   lease/ops engagement (4→1 per block) + the depth-16×4 MiB vs
   depth-32×1 MiB probe-up-governor adjudication. Record and RESTORE the
   sysctl.
4. ≥ 600 s sustained soak on the winning configuration; tripwires zero
   (`transport_lease_overlong`, `transport_parked_commits` bounded,
   `fuse3_zc_replies` still 0, `writer_guard_fenced` 0).

**Abort:** deferred wholesale if Phase 1 fell back to 7.1.2 (kmbuf absent
⇒ structurally inert; running "rows" there measures nothing).

## Phase 7 — zcrx Z2 §5 (the Z3 lane acceptance)

Per `.benchmarks/2026-08-04-zcrx-z2.md` §5, verbatim (the note is the
script): arm proof under `SQUEEZEFS_ZCRX_LANE=1` (NIC before/after
snapshots; unmount must restore byte-identically), engagement closure
byte-exact (`zcrx_fill_bytes` ≡ cold-fill bytes ≡ `zcrx_gather_bytes`,
violations/poisoned 0, per-queue `rx*_bytes` ≡ lane bytes), A-B-B-A vs
`SQUEEZEFS_ZCRX_LANE=0` with RX CPU + DRAM B/B columns (parity-class
throughput is the Z2 bar — do NOT adjudicate the lane on Z2 CPU; −65 % CPU
is Z3's bar), ≥ 30 min loaded soak (tripwires 0, engagement still exact,
`zcrx_area_bytes` → 0 at unmount), and the failure-lattice spot-check (kill
one lane TCP connection: ONE `zcrx_lane_poisoned` transition, zero failed
reads, clean unmount + NIC restore).

**Abort:** a failed NIC-restore assert is stop-the-line for the phase (the
steering law is a correctness contract); the lane is opt-in, so deferring
the phase leaves the epoch clean.

## Phase 8 — generic/634 on the live bit-62 pair (v2 kernel)

1. Confirm negotiation: mount log carries the `FUSE_TIME_LIMITS` advert
   line (kernel offered bit 62 — v2 only).
2. `sudo tests/run_fstests.sh generic/634` against the mount → **expected
   PASS on sqz2** (the kernel now clamps incore via `sb->s_time_min/max` =
   ±9,223,372,036 s exactly where the daemon clamps durably).
3. Record LOUDLY: the standing generic/634 adjudication **stays pinned for
   the fleet kernel** (the runner's expected-shape logic keys per kernel);
   this row upgrades sqz-kernel hosts only.

## Phase 9 — close-out

Journal SESSION END (rows run, gates met/deferred, artifacts); write the
**post-reset-v5 baseline evidence note** (the reset-v4 note's successor —
the canonical table every subsequent campaign brackets against); retain
per-row artifacts under `/scratch/tmp/` (never /tmp, never /root); restore
every recorded NIC/sysctl/env change not part of the standing posture;
remove the v1 kernel RPM once v2 is proven (root-fs headroom); leave the
standing pair mounted + armed and the box on the sqz2 one-shot (permanent
default 7.1.2 — a power cycle reverts, by design).

---

## Owed-row checklist (the window is done when every box is ticked or
adjudicated-deferred)

- [ ] v2 kernel booted one-shot; capability matrix recorded (Phase 1)
- [ ] reset-v5 converged format executed + journaled (Phase 2)
- [ ] canonical baseline table + NT-A0 legs + 600 s soak (Phase 3)
- [ ] rewrite-P0: fresh-vs-rewrite A-B-B-A gates (±5 %, amp ≤ 1.05, d_ops 0) (Phase 4)
- [ ] rewrite-P0: loop-rewrite latest-wins + sustained zero-discard row + soak (Phase 4)
- [ ] il hold-probe: A-B-B-A + A0 + soak, il ≥ kernel randread, engagement exact (Phase 5)
- [ ] kmbuf arm proof + QUICK fstests on armed mount (Phase 6)
- [ ] kmbuf A-B-B-A (commit_flush delta) + 4 MiB-write row + depth adjudication + 600 s soak (Phase 6)
- [ ] zcrx Z2: arm/engagement/A-B-B-A/30-min soak/failure-lattice/NIC restore (Phase 7)
- [ ] generic/634 PASS on sqz2; fleet adjudication re-affirmed pinned (Phase 8)
- [ ] post-reset-v5 baseline note written; epoch journal closed (Phase 9)
