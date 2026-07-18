# NVMe-oF Dual-Stack A/B Rerun — Product Verbs on the Fidelity Rig (PR 7 / N7, gate G5)

Date: 2026-07-18 · Dev at `45a4eda` (docs branch `docs/nvmeof-program-close`; **zero runtime-code change** since PR 6) · Box: AMD RYZEN AI MAX+ PRO 395 (32 threads, capped 3.5 GHz, governor `performance`), 109 GiB RAM, kernel 7.1.3-2-cachyos.

**What this is**: the closing A/B rerun the design doc's PR 7 row orders (`docs/design-nvmeof-target-management.md` — gate **G5** named rows, plus the two user-resolved recorded-class additions: **2-/4-reactor spdk_tgt scaling** and **`bdev_uring` vs `bdev_aio`**). Unlike the 2026-07-17 scoping A/B (`.agents/spdk-scoping/scoping-report.md` §3, rig-plumbed), every target here is stood up **through the product's own verbs** on the fidelity substrate (`tests/nvmeof_target_substrate.sh`): `target setup` → `target start [--cores N]` → `share --target-stack {spdk,nvmet}` → `connect`. The harness supplied only backings, fio, and assertions.

## Provenance

| Item | Value |
|---|---|
| Product binary | `target/release/squeezefs`, md5 `fa91b46d27696290db9ed35a189f4897` (== dev `45a4eda`) |
| spdk_tgt (six-row + scaling arms) | sanctioned scoping build **v26.05** sha `d519b163cbc0e2f28c35d9bc86d610da368b032c`, via the `SQUEEZEFS_SPDK_TGT_BIN` relocation seam (loud unpinned warning printed — §6.5 rig posture) |
| spdk_tgt (uring row only) | **throwaway** rebuild of the same commit `d519b16` with `--with-uring` (liburing 2.15 headers), at `/var/tmp/spdk-uring-pr7` — built for this row, **removed after** (zero-residue rail); reported `"SPDK v26.05 git sha1 d519b16"` → no version drift, product `target start` accepted it under the loud-unpinned warning |
| Substrate | fidelity substrate, product-verb-driven; bench backings = three 4 GiB **zram** (`hot_add`, indexes 5–7, manifest-recorded), prefilled with incompressible fio data before sharing; NVMe/TCP **localhost** (127.0.0.1); nvmet port ids in the test slice (54000–54099); hugepages prior **0** → 1024 × 2 MiB → **restored 0** |
| Instrument | **fio-3.42, `ioengine=io_uring`, `direct=1`, `numjobs=1`, ramp 2 s, runtime 10 s, `norandommap`, `randrepeat=0`, railed `--cpus_allowed=0-7`, against the raw connected namespace — no squeezefs in the data path** (house rule: every measurement states its instrument) |
| Discipline | n=3 per row, medians [min–max]; runs serialized (sole box-owner); quiet gate per run: loadavg < 6.0 (up to 4 legitimate poller cores in the scaling rows) **and Tctl < 80 °C**; Tctl observed 56.2–75.2 °C across all runs (cap 3.5 GHz; the ≥ 86 °C sustained cool-down rule never triggered) |
| CPU accounting | whole-system busy CPU-s per ~13.4 s run window (`/proc/stat` delta, **corrected formula** — see anomaly 1) + spdk_tgt process CPU-s (`/proc/<pid>/stat` delta); ambient baseline (no poller, corrected formula): **5.5 CPU-s / window = 0.41 cores** |
| Isolation | during kernel-nvmet rows the spdk_tgt was **SIGSTOPped and its initiator associations disconnected** (method refinement over scoping, which only SIGSTOPped — see anomaly 3) |
| Cleanup | substrate teardown → **ZERO residue** (before/after snapshot diff empty, verified independently: `nr_hugepages` 0, only zram0 remains, no spdk_tgt, empty nvmet tree); throwaway uring prefix deleted |

## 1. The six G5 rows (1-reactor spdk_tgt vs kernel nvmet, product-shared)

Medians of 3 [min–max]; IOPS; p50/p99 µs; sys CPU = whole-system busy CPU-s per ~13.4 s window (ambient 5.5 included); spdk CPU = spdk_tgt process CPU-s.

| Row | Arm | IOPS | BW MB/s | p50 µs | p99 µs | sys CPU s | spdk_tgt CPU s |
|---|---|---:|---:|---:|---:|---:|---:|
| rand4k read QD32 | **spdk-tcp** | **235,030** [234,689–235,747] | 918 | 127 | 216 | 29.1 | 12.0 |
| | nvmet-tcp | 111,501 [111,312–111,722] | 435 | 288 | **387** | 18.0 | — |
| rand4k write QD32 | **spdk-tcp** | **45,487** [45,175–45,496] | 177 | 692 | **880** | 19.9 | 13.0 |
| | nvmet-tcp | 36,884 [36,795–36,984] | 144 | 872 | 1,597 | 18.0 | — |
| rand4k read QD1 | spdk-tcp | 56,289 [56,145–56,401] | 219 | 11 | 19 | 23.9 | 11.5 |
| | **nvmet-tcp** | **102,420** [102,281–102,450] | 400 | **2–3** | **11** | 17.3 | — |
| rand4k write QD1 | spdk-tcp | 27,167 [27,048–27,206] | 106 | 30 | 35 | 21.0 | 12.3 |
| | **nvmet-tcp** | **35,751** [35,722–35,849] | 139 | **23** | **29** | 17.6 | — |
| seq128k read QD8 | **spdk-tcp** | 19,158 [19,144–19,178] | **2,394** | 387 | 790 | 21.8 | 12.2 |
| | nvmet-tcp | 13,047 [12,987–13,215] | 1,631 | 569 | 1,171 | 18.4 | — |
| seq128k write QD8 | spdk-tcp | 1,594 [1,588–1,598] | 199 | 4,947 | **6,914** | 17.7 | 13.2 |
| | **nvmet-tcp** | **1,951** [1,915–1,961] | **244** | 4,751 | 8,847 | 27.6 | — |

### G5 verdicts (ordered rows: spdk ≥ nvmet required — all three PASS)

| G5 row | spdk | nvmet | Verdict |
|---|---|---|---|
| rand4k read QD32 | 235,030 IOPS | 111,501 | **PASS** (2.11×; ordering also holds on the slow association signature — 136 k ≥ 111.5 k, anomaly 2) |
| rand4k write QD32 | 45,487 IOPS | 36,884 | **PASS** (+23 %, p99 1.8× tighter — 880 vs 1,597 µs) |
| seq128k read QD8 | 2,394 MB/s | 1,631 | **PASS** (+47 %) |

### Recorded rows (unordered by G5 — recorded and attributed)

- **seq128k write QD8**: nvmet marginally ahead (244 vs 199 MB/s) and **both arms collapse** against the zram raw-write ceiling (≈ 1.5 GB/s class measured by the loop reference in scoping §3.2) — reproduces the scoping attribution verbatim: an arm-symmetric TCP-transport/backing interaction, **not** an SPDK-vs-nvmet differentiator. The §6.3 deployment-class table's "no stack preference" row stands.
- **rand4k read QD1**: nvmet wins the latency floor — p50 **2–3 µs vs 11 µs** (the kernel target completes inline in softirq; the single spdk reactor adds a hop at QD1). Accepted, documented in the deployment-class table.
- **rand4k write QD1**: nvmet wins — 35,751 vs 27,167 IOPS, p50 23 vs 30 µs. Accepted, same attribution.

### Per-core honesty (G5's framing clause — governs every SPDK perf claim)

- The 1-reactor spdk_tgt burned **0.86–0.99 of its core during load** (11.5–13.2 CPU-s / 13.4 s) and — dogfooded via the product's own `target status --json` — **busy-polls ~100 % of the core even idle**: process CPU occupancy measured **0.99 cores at zero load**, while the `reactors[].busy_pct` gauge correctly reads **0.0 idle / 90.6–90.9 under QD32 load** (it is a tick-based *useful-work* fraction from `framework_get_reactors`, not core occupancy — the pair answers "is the poller doing work" vs "is the core spent"; both are true signals, document both).
- **IOPS per net system core** (ambient 0.41 cores subtracted), rand4k read QD32: spdk **≈ 133 k/core** on the fresh-association signature (235,030 / 1.76 net cores) but **≈ 74 k/core** on the keep-alive-scarred signature (136,028 / 1.84 — anomaly 2); nvmet **≈ 120 k/core** (111,501 / 0.93). Kernel nvmet sits **inside the spdk bracket**: per-system-core efficiency remains **parity-class, not an SPDK win** — SPDK's absolute queued-row wins come from its dedicated poller core, exactly the scoping posture. The deployment-class table's converged-node guidance stands unchanged.
- Cross-session note: the scoping report's *absolute* sys-CPU columns (63–87 CPU-s) are not comparable to this note's (its formula included iowait — anomaly 1 — and its ambient differed); within-session comparisons in both documents stand.

## 2. Reactor scaling rows (2 and 4 reactors — recorded class, user decision Resolved Questions #2)

Product verbs end-to-end: `target stop --force` → `target start --cores N` (mask allocated from the highest online CPUs down) → initiator reconnect → rows. **Every row below is TCP-localhost-bound: a single fio stream over loopback saturates one connection/qpair long before reactor scaling can show — these rows bound the *shape* (what extra reactors cost on this rig), they are NOT a fleet scaling claim.** Multi-initiator/multi-connection scaling on real fabric remains unmeasured (future program, with the dynamic-scheduler work).

| Row | 1 reactor | 2 reactors | 4 reactors |
|---|---|---|---|
| rand4k read QD32 IOPS | **235,030** | 218,008 | 236,712 |
| rand4k write QD32 IOPS | **45,487** | 32,071 (0.71×) | 32,323 (0.71×) |
| seq128k read QD8 MB/s | **2,394** | 1,688 (0.71×) | 1,757 (0.73×) |
| spdk_tgt CPU s (r32 row) | 12.0 (0.90 core) | 25.0 (1.87) | 51.6 (3.85) |
| **IOPS per burned reactor-core** (r32) | **≈ 262 k** | ≈ 117 k | ≈ 61 k |
| reactor busy_pct under load (`target status`, dogfooded) | 90.9 | 77.6 / 24.0 | 64.7 / 16.9 / 16.4 / 10.1 |

Reading (localhost-bound, per-core honest): on this single-stream localhost shape **extra reactors buy nothing and cost everything** — reads stay flat (within the association-signature noise band), rand-write and seq-read *regress* ~29 % (cross-reactor qpair distribution adds inter-core hops for a single stream), and every added reactor burns a full core regardless of useful work (process accounting ≈ 0.94–0.96 core/reactor; the busy_pct spread shows one hot reactor + idle-polling siblings). Per-burned-core throughput falls 262 k → 117 k → 61 k IOPS/core. **v1's one-reactor default is the measured right default for this deployment shape**; do not raise `--cores` without a multi-connection workload and a measurement.

## 3. `bdev_uring` vs `bdev_aio` (recorded class, user decision Resolved Questions #6)

The pinned scoping build carries no uring bdev (`SPDK_CONFIGURE_ARGS` has no `--with-uring` — the PR 5 soft-RoCE leg's RESIDUAL line called this out), so the row took the sanctioned honest branch: a **one-off throwaway rebuild** of the same pinned commit with `--with-uring` (liburing 2.15), started via the product (`SQUEEZEFS_SPDK_TGT_BIN` seam, loud unpinned warning, version handshake green — same v26.05). **Both arms ran on that same binary, same 1-reactor config**: the aio arm is the product-shared `bdev_aio` bench namespace (`load_config`-restored); the uring arm is a **harness-level rpc.py-built** subsystem over an identically prefilled 4 GiB zram (zero product code touches bdev_uring), rpc.py-deleted before any product `save_config` could capture it.

| Row (QD32, 4 KiB, same binary/reactor/backing class) | bdev_aio | bdev_uring | Read |
|---|---|---|---|
| rand4k read IOPS | 136,028 [134,150–138,082] | 131,538 [131,409–131,915] | parity-class (−3 %; and the aio arm sat on the *slower* association signature — anomaly 2 — so this if anything flatters uring) |
| rand4k read p50 / p99 µs | 226 / 643 | 152 / **1,269** | uring: lower median, **2× worse tail** |
| rand4k write IOPS | **45,693** [45,540–45,875] | 23,524 [23,502–23,550] | **uring 0.51× — decisive aio win** |
| rand4k write p99 µs | **880** | 2,441 | 2.8× worse tail |

**Verdict: the data says do not switch.** `bdev_aio` stays the shipped backing (write throughput 1.94×, tails 2–2.8× tighter, read parity). The "switch only if the data says so" posture is now discharged *with* data; any future revisit is its own PR with its own rerun (and would start from a liburing/uring-bdev configuration investigation, not a flag flip).

## 4. Honest anomalies & count events (multi-run discipline)

1. **CPU-formula fix, count restarted from zero.** The seed harness (`.agents/spdk-scoping/bench.sh`) computes "busy" as `$2+$3+$4+$6+$7+$8` of `/proc/stat` — its comment says user+nice+sys+irq+softirq+steal, but **`$6` is iowait** (the correct set is `$2+$3+$4+$7+$8+$9`). On this desktop ~10 parked io_uring cqring waiters keep `nr_iowait`≈10 and PSI-io ≈ 80 % at true idle (state S, `in_iowait` — zero actual cycles), so the inherited formula counted **~8 phantom idle cores as busy** (ambient read 109.5 CPU-s/window; corrected: 5.5). First full six-row count on both arms was quarantined (`summary.csv.quarantined-iowait-formula`, kept as signature data) and **both arms re-ran from zero** with the corrected formula. fio-side numbers (IOPS/lat/BW) were never touched by the bug; the quarantined count agrees with the accepted one on every fio column. Scoping's sys-CPU columns carry the same formula — treat cross-session sys-CPU comparisons as invalid (its per-core *conclusion* is re-derived above from clean data and survives).
2. **QD32-read association-lifetime bimodality — the scoping contamination-note effect, reproduced and bracketed.** Fresh initiator associations measure **198–237 k IOPS** on spdk rand4k-read-QD32 (three independent sessions in this run: 198–205 k, 234–236 k, 236–238 k @ 4r); associations that have ridden target bounces/keep-alive losses measure **131–138 k** (the uring-session aio arm, whose controller had reconnected through two `target stop`/`start` cycles). The scoping report's accepted 126 k was the scarred signature; its quarantined pre-overlap readings (175–178 k) were the fresh one. **Every signature preserves the G5 ordering** (min spdk 131 k ≥ nvmet 111.5 k), so no verdict depends on which signature a run draws; the per-core bracket in §1 states both. Operators comparing raw QD32-read numbers across reconnect histories should expect this band.
3. **nvmet seq128k-read reads higher than scoping (1,631 vs 1,406 MB/s)** — attributed to a deliberate isolation refinement: this run **disconnects** the spdk-stack initiator associations before SIGSTOPping spdk_tgt for the kernel arms, so no keep-alive timeout/reconnect churn overlaps nvmet rows (scoping only SIGSTOPped, and its report records the associations "riding out several keep-alive losses" during exactly those windows). spdk seq-read moved +4 % (2,304 → 2,394) — within session variance.
4. **First 4-reactor busy_pct probe read 0.0** — the status sample raced fio's ramp window; re-sampled deeper into the load window (values in §2). Kept both samples in the session artifacts.
5. **`busy_pct` semantics pinned while dogfooding**: `reactors[].busy_pct` is the tick-based useful-work fraction (0.0 idle) — it is **not** core occupancy (0.99 cores at idle by process accounting). Both signals are correct; README's observability section now documents the pair.

## 5. Cleanup proof

Substrate teardown ended **ZERO residue** (its built-in before/after snapshot diff: identical; archived in the session artifacts). Independently verified after teardown: `nr_hugepages` = 0 (prior restored by the product's `--restore-prior` path), `/dev/zram0` (user swap) the only zram remaining, no `spdk_tgt` process, nvmet configfs subsystem tree empty, no listeners in 4400–4899. Throwaway `/var/tmp/spdk-uring-pr7` prefix deleted after its row (build-info archived); `/var/tmp/spdk-scoping` untouched (read-only per its convention). `/etc/nvme/hostnqn|hostid`, user mounts, `~/tmp/nvme`, and containers never touched.

---

*Gate G5 adjudication: the three ordered rows PASS on product-verb-shared namespaces; the full six-row table is recorded (no subsetting); the seq-write and QD1 rows are recorded with their standing attributions; the reactor-scaling and bdev_uring recorded rows carry their caveats (TCP-localhost-bound; switch-only-if-data-says-so → data says no). Per-core framing accompanies every SPDK claim above and in the shipped docs.*
