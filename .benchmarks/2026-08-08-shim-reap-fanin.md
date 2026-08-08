# 2026-08-08 — SHIM IOPS round 2: the client observation/reap path under fan-in

| | |
|---|---|
| **Charter** | The shim-iops campaign's §5 "shaped follow-ons", promoted to top lever by the 2026-08-07 22:19 field row (squeeze-test, 32×32 rand-4k il, engagement exact): daemon `ipc_direct_phase_ns` clean (admit 4–8 µs, finish ~4 µs, inflight median ~110 µs / mean ~330 µs — the tail venue-owned) against fio clat median 529 µs / MEAN 1,554 µs / p99 20 ms — **~1.2 ms of MEAN residence and a ~5× tail multiplier between daemon completion and client observation**. |
| **Branch / SHAs** | `perf/shim-reap-fanin` off dev `d10e5a22` — ingress-stamp protocol `a9d635c4`, red `1c0a7cdf` / green `860b49f6`, loom-build fix `c5287a64`, doorbell batch protocol `1fa60146`, client batch park + ceremony economy `856bc57e`, this note + rig |
| **Venue (every local row)** | tcp devsub (`SQZ_DEVSUB_TRANSPORT=tcp`, nvmet-tcp 127.0.0.1 — 4× null_blk mds `/dev/nvme1-4n1` + 4× zram-8G oss `/dev/nvme5-8n1`), 32-CPU strixhalo (thermally capped — every counted row behind a `Tctl ≤ 70 °C` cooldown gate; a FOREIGN agent was cycling test daemons on the box, sampled per row into `leg*-foreign.log`). Mount: armed (FUSE_ZC default-on at both commits) + `--interception --allow-other` + `SQUEEZEFS_DIRECT_DEVICE_TRUE=1`. Instrument: fio 3.42 (dynamic) + `LD_PRELOAD=libsqueezefs_il.so`, `--rw=randread --bs=4k --direct=1 --randrepeat=0`, matched libaio, 32× 128 MiB prefilled fileset, 60 s time_based + 5 s ramp. Pair same-commit per leg (KD-7): binary-PAIR A-B-B-A (`C1 B1 B2 C2` — fresh format + fresh fileset + remount per leg, the aging rule). Rig: `.benchmarks/rigs/2026-08-08-reapfanin-rig.sh` + `-analyze.py` (engagement gates FATAL per row). |
| **Verdict** | All three levers landed. **Lever 3 rewrote the attribution**: the prior ledger's "client + ring ingress" subtraction term is, measured, ~ALL RING INGRESS on this venue (publish→dequeue mean ≈ 145 µs at 32×8 / ≈ 520 µs at 32×32) with client observation ≈ 0 — so the FIELD's ~1.2 ms gap must be split by `ipc_ingress_ns` before any client-reap claim. **Lever 1 wins where the observation term lives**: 32×8 clat p50 −16 % / IOPS +2 % (medians, both orders agree); 32×32 wash locally (ingress-dominated, as the ledger now proves). The **flat-event-park herd row** (the posture the 24→2 history retreated from) loses 3–10 % IOPS with +16–36 % p99/p99.9 at 0.58–0.80 wakes/op vs the threshold's 0.14–0.47 — the batch mark beats BOTH prior postures at fan-in, which is the protocol's whole claim, counted. Engagement exact on every row (`ops ≡ dd_serves`, ingress n ≡ ops, tripwires 0, fallbacks 0). Loom green (k=1 + new batch model); fence weakening re-verified FAILING; (b)/(c) weakening sensitivity found lost PRE-change and filed. Field spec below (the box was busy per the charter — user benchmarks there). |

---

## 1. The levers built

### 1.1 Lever 3 (built FIRST — the instrument): the slot ingress stamp

`IpcSlot`'s `_reserved` word became the **ingress stamp** (`stamp_ingress` /
`ingress_stamp`, IPC_ABI 4 → 5): the shim stamps every publish (all three
sites) with CLOCK_MONOTONIC low-32 ns (`mono_core.rs`, ONE `#[path]`-shared
clock for both ends), the daemon's drain reads it ONCE at dequeue and buckets
`now − stamp` through the `ingress_delta_ns` law (never-stamped / ≥ 1 s
wrap-ambiguous / hostile-future stamps DISCARDED — the stamp is
client-writable shm, display-only per §5.3.1) into the always-on
**`ipc_ingress_ns`** histogram. The ledger's former `fio clat − daemon total`
subtraction now splits into a MEASURED ingress term and the residual
OBSERVATION term (daemon completion → client harvest):

```
clat = slat + ingress(measured) + [dequeue→probe ≈ 0] + total(measured) + observation(residual)
```

Contracts: `ingress_stamp_roundtrip_and_delta_law` (squeezefs-ipc),
`ring_ops_record_ingress_residence` (red `1c0a7cdf` → green `860b49f6`).
Row-validity rule from here on: **an il row is INVALID unless the
`ipc_ingress_ns` count delta accounts for its ops** (this bracket gates it
FATAL at ≥ 0.99×; every leg passed exact — n ≡ `ipc_ops_read` delta).

### 1.2 Lever 1: the CqeDoorbell batch-wake threshold + the deep-regime batch park

`CqeDoorbell` grew a third word (`wake_at`, 8 → 16 B, same header line —
the PERF-18 co-location falsification stands): `park_begin_batch(k)`
registers a wake MARK at `snapshot + k`; the daemon's `complete()` elides
wakes toward parked reapers until the seq REACHES the earliest mark
(wrapping order). `park_begin` ≡ `park_begin_batch(1)` — the shipped
sparse-regime semantics are bit-identical. Strand-freedom: the futex
admission (`seq == snapshot`) certifies zero post-snapshot completions, so
`k ≤ own pending` keeps the k-th wake live; pre-registration completions are
found by the mandatory re-scan; pre-admission ones fail the admission.
Concurrent parkers compose by earliest mark (wrapping-min against the live
snapshot); the one benign first-parker-overwrite race is bounded by the
park's own timeout (documented at the store).

The client's deep arm (`parks > REAP_EVENT_PARK_MAX = 2`) **replaces the
blind `SQUEEZEFS_IL_REAP_QUANTUM_US` sleep** (quantum/2 mean observation lag
+ nanosleep timer slack + runqueue delay under fan-in — the field's client
segment) with the batch park: `k = sizing::reap_batch_wake_threshold(own
pending on the session)` = `clamp(pending/4, 2, pending)` (depth-derived,
liveness-capped — never a constant; no new knob), age-bounded at the SAME
quantum. **Worst case = the shipped sleep exactly; a completion burst cuts
the wait event-exact; the daemon pays ≤ 1 wake per k completions** — never
the flat park's wake-per-completion herd (the 24→2 history: 17.8 M wakes /
22.2 M ops, +43 µs daemon inflight at 32×8).

Loom: the existing k=1 model green (now exercising the wake_at path) + the
new **`ipc_cqe_batch_parked_reaper_never_stranded`** (two completer threads,
k = 2 = the pending population) green. Weakening verification (applied,
run, restored):

| weakening | k=1 model | batch model | note |
|---|---|---|---|
| (a) remove the daemon-side Dekker `fence(SeqCst)` in `complete()` | **FAILED** | **FAILED** | the fence is load-bearing, re-verified |
| (b) daemon `parked` load → `Relaxed` | ok | ok | **no longer discriminates — PRE-EXISTING**: re-run against the PRE-change core + the shipped k=1 model, also ok. With both sides carrying the SeqCst fence between their store and load, the fence pair subsumes the load's ordering; the 2026-07-28 record predates model reshaping. The SHIPPED code keeps SeqCst. |
| (c) `park_begin` snapshot-before-register | ok | ok | same pre-existing class as (b) — re-verified on the pre-change core: also ok. Kept register-first in shipped code (the documented protocol). |

(b)/(c) are recorded honestly rather than re-claimed: the models still hold
the strand-freedom INVARIANT on the shipped code; what they lost since
2026-07-28 is sensitivity to those two specific reorders. Filed as a
follow-on: tighten the cqe models (e.g. model the futex admission + sleep
as separate labeled steps) so all three weakenings discriminate again.

Also fixed while here: **the loom build was BROKEN at dev tip** (the
2026-08-07 wedge census gave `conveyor_core` a `super::META_CONVEYOR_QUEUED`
reference with no mirror in loom-models' include scope — `--cfg loom` did
not compile). `c5287a64` restores it; loom runs were impossible on dev for
one day.

### 1.3 Lever 2: reap-pass ceremony economy (the fan-in audit)

Audit finding: the harvest walk itself is convoy-free (poll-consume skips
slow completions — no head-of-line blocking; the 4-sweep spin is
sparse-arm-only), so **the blind sleep WAS the convoy** — during every
50 µs (+slack) window, all completions landing behind it waited for the
timer, and under fan-in the timer waited for the runqueue. What remained:
the loop allocated its park snapshot, token buffer, wait entries and parked
tokens **per empty pass** on the host app's malloc (empty passes = the
fan-in steady state). Now: pass-reused buffers + `pending_tokens_into`
(zero-alloc snapshot, parity-pinned). The deep arm carries no pre-park
spin by design (the park's admission + mandatory re-scan cover the
just-completing case; the spin's O(sweeps×qd) remote-line probes are CPU
theft at fan-in — the 2026-07-26 sizing lesson).

## 2. Local A-B-B-A bracket (C = candidate pair `856bc57e`, B = base pair `d10e5a22`)

(rows below — engagement EXACT on every leg: `ipc_ops_read` ≡ `dd_serves`,
fallbacks 0, tripwires 0, ingress n ≡ ops on C legs / structurally absent on
B legs; per-row foreign-activity sample logs kept)

### 2.1 Row table (60 s time_based + 5 s ramp each; buckets are ≤-bounds, µs)

| leg | qd | IOPS | clat p50 | p99 | p99.9 | inflight p50/p99 | ingress p50/p99 | wakes/op | thirds Δ% |
|---|---|---|---|---|---|---|---|---|---|
| C1 | 8 | 744,940 | 169.0 | 1,728 | 2,572 | 128/2000 | 64/2000 | 0.474 | +10.6 |
| B1 | 8 | 718,949 | 209.9 | 1,565 | 2,277 | 128/2000 | — | 0.000 | +6.0 |
| B2 | 8 | 702,416 | 214.0 | 1,581 | 2,310 | 128/2000 | — | 0.000 | −1.1 |
| C2 | 8 | 704,582 | 187.4 | 1,761 | 2,572 | 128/2000 | 64/2000 | 0.472 | −0.9 |
| C1 | 32 | 1,022,565 | 790.5 | 3,555 | 5,014 | 512/4000 | 512/4000 | 0.142 | −0.3 |
| B1 | 32 | 1,015,155 | 815.1 | 3,457 | 4,817 | 512/4000 | — | 0.000 | +3.7 |
| B2 | 32 | 985,215 | 839.7 | 3,555 | 4,948 | 512/4000 | — | 0.000 | +4.8 |
| C2 | 32 | 970,862 | 847.9 | 3,654 | 5,079 | 512/4000 | 512/4000 | 0.140 | +5.4 |

Lever row — the **flat event park** (`SQUEEZEFS_IL_REAP_PARK_MAX=4096` on
the C pair — the posture the 24→2 history retreated from, now comparable
on ONE binary):

| leg | qd | IOPS | clat p50 | p99 | p99.9 | wakes/op | inflight mean | thirds Δ% |
|---|---|---|---|---|---|---|---|---|
| C3 (flat) | 8 | 688,657 | 164.9 | 2,089 | 3,490 | 0.802 | ~226 µs | −5.5 |
| C3 (flat) | 32 | 916,670 | 864.3 | 4,227 | 6,259 | 0.580 | ~577 µs | **−18.0** |

### 2.2 Reading

* **32×8 (the field-ladder shape): the batch park deletes the observation
  quantum.** clat p50 C 169/187 vs B 210/214 — **−16 % at medians, both
  orders agree** (the deleted ~25–45 µs IS the quantum/2 + harvest the
  1×8 row priced at 4.6×); IOPS medians C 724.8 k vs B 710.7 k = **+2.0 %**
  (C > B in BOTH orders; below the 5 % bar as an IOPS claim — the
  latency-shape claim is the counted one). p99/p99.9 runs ~+10 % on C
  (1.73/2.57 ms vs 1.57/2.31) — the mark trades a thin tail band for the
  median (ops that just miss a wake batch ride to the next mark or the
  age bound); honest and bounded by the quantum.
* **32×32: wash** (C 996.7 k vs B 1,000.2 k medians; orders disagree —
  thermal drift across legs). Expected: §3 shows this shape's client-side
  term is ~all ingress, which no client park can touch.
* **The herd row is the design's counted proof**: flat event park at
  fan-in pays 0.58–0.80 wakes/op (vs the threshold's 0.14/0.47 ≈ 1/k for
  k = 8/2), inflates daemon inflight (+50–70 µs at qd32 — the
  wake-herd tax reproduced), loses 3–10 % IOPS, +16–36 % tails, and its
  qd32 row DECAYS −18 % across thirds. The batch threshold beats the flat
  park at fan-in AND the blind sleep at the median — neither prior
  posture wins any column.
* Lever-3 cost: B vs C at 32×32 wash (the stamp's clock read + the
  daemon's parked-path `wake_at` load + 0.14 wakes/op priced ≤ noise).
* Tail multiplier (client p99 ÷ daemon inflight p99, bucket-bounded):
  0.78–0.91 on EVERY leg both pairs — **the field's ×5 multiplier does
  not exist on this venue** (stated per the charter: the local box's low
  fabric RTT hides the mean win; the multiplier is the local instrument
  and it reads ≈ 1 here, so the field row is the adjudicator).

## 3. The term ledger — stamp-measured (bucket-midpoint means, µs)

| row | fio clat mean | slat | **ingress (measured)** | admit | inflight | finish | total | observation (residual) |
|---|---|---|---|---|---|---|---|---|
| C1 32×8 | 343 | 0.4 | **144** | 2.6 | 202 | 1.8 | 209 | ≈ 0 (−11) |
| C2 32×8 | 363 | 0.4 | **153** | 2.8 | 212 | 2.0 | 219 | ≈ 0 (−10) |
| B1 32×8 | 355 | 0.3 | — | 2.5 | 178 | 1.2 | 183 | 172 (= ingress+obs, unsplit) |
| C1 32×32 | 1,001 | 0.3 | **517** | 2.7 | 505 | 1.5 | 511 | ≈ 0 (−27) |
| C2 32×32 | 1,054 | 0.3 | **547** | 2.9 | 528 | 1.6 | 534 | ≈ 0 (−27) |
| B1 32×32 | 1,008 | 0.2 | — | 2.7 | 495 | 1.4 | 501 | 507 (unsplit) |

(Negative residuals = log-bucket midpoint overestimate; read as ≈ 0.)

**The attribution shift this buys:** the shim-iops ledger's ~160 µs
"client observation + ring ingress" subtraction at 32×8 is, measured,
**~145 µs of RING INGRESS + ≈ 0 observation** — ops sit in the submission
ring behind the svc-thread drain funnel (Little: 745 k × 144 µs ≈ 107 of
the 256 in-flight ops queued pre-dequeue; at 32×32, 1 M × 520 µs ≈ 520 of
1,024). The base rows' unsplit 172/507 µs terms decompose the same way.
So on ANY venue, the first read of a client-side gap is now
`ipc_ingress_ns`: if the field's ~1.2 ms mean gap is mostly ingress, the
next lever is the DRAIN FUNNEL (svc lane width / sweep economy / eager
flush at saturation), not the client reap — and the instrument that
decides landed with this campaign.

## 3.5 Verification

- Suites ×10 serial, final tree (`ipc_direct_drive_tests` 17 —
  incl. the new ingress contract, `ipc_op_economy_tests` 37,
  `ipc_host_tests` 24, `preload_session_tests` 3): green ×10.
  `env_knob_convention_tests` 21, `derivation_sweep_tests` 24,
  squeezefs-ipc crate (42 — incl. the 4 new doorbell/threshold pins +
  the delta-law pin), squeezefs-preload incl. `interposers` (the
  `pending_tokens_into` parity pin): green. Full
  `cargo test --all-features -- --test-threads=1`: green.
- Loom: `ipc_cqe_parked_reaper_never_stranded` +
  `ipc_cqe_batch_parked_reaper_never_stranded` green (the k=1 model now
  exercises the wake_at path); weakening (a) fence-removal FAILS both
  (load-bearing re-verified); (b)/(c) recorded as pre-existing
  sensitivity loss (§1.2). **The loom build itself was broken at dev
  tip** — fixed (`c5287a64`) before any model could run.
- Both-workspace clippy `-D warnings` (all-features AND shipped config,
  + preload-release profile, + fuse3 workspace) clean; `cargo fmt
  --check` clean both workspaces.
- Preload gate: leg 1 PASSED (Issue-4 guard, passthrough battery, aio
  lifecycle ×3 orderings, direct-link battery); leg 2 (sudo, mounted)
  PASSED (mount parity, engagement rows, establish-refused shapes,
  direct-drive kill-9 soak ×5 — zero residue).

## 4. Field spec (the box was busy per the charter — user benchmarks there)

Venue: squeeze-test, fresh reset (venue stationarity), pair from THIS branch
(KD-7 same-commit, md5 both ends). All rows matched libaio, `direct=1`,
engagement gates FATAL (now including `ipc_ingress_ns` n ≡ ops).

1. **A-B-B-A vs the shipped pair** (`d10e5a22` class): rand-4k 32×8 and
   32×32, 60 s sustained, medians of 3, thirds flat. Read per row:
   `ipc_ingress_ns` (the first MEASURED field ingress), `ipc_direct_phase_ns`,
   `ipc_cqe_wake_{writes,elided}` (expect wakes/op ≈ 1/k on C vs ≈ 0 on B —
   the daemon-side cost the observation win must price against at 80–85 %-CPU
   field reapers), fio clat mean + p99.
2. **Acceptance row**: 32×32 client MEAN converging toward daemon
   mean + fabric (the ~1.2 ms residual shrinking toward ingress+observation
   measured floor); tail multiplier (client p99 ÷ daemon inflight p99)
   toward ~1.
3. **The 1 M attempt** after the user's storage-node reset (the raw re-grade
   first — the venue-degradation rule).
4. Lever rows if the base A/B moves: `SQUEEZEFS_IL_REAP_QUANTUM_US` sweep
   {10, 50, 200} on the C pair (the age bound's residual share),
   `SQUEEZEFS_IL_REAP_PARK_MAX=4096` (flat-park comparison — the herd row).

**Projection (arithmetic-on-measured-constants, adjudicated by row 2):**
the field 32×32 row ran 659 k at clat MEAN 1,554 µs with daemon total
mean ≈ 342 µs — a ~1.2 ms unmeasured gap. If its ingress share matches
the local shape (~0.5 ms at this depth) the remaining ~0.7 ms is
observation (the blind quantum + scheduler slack at 32-job fan-in on a
saturated box — exactly what the batch park deletes), and the mean
converges toward ingress + total + fabric ≈ 0.9–1.1 ms ⇒ **32×32 ≈
0.95–1.1 M IOPS (the 1 M class, from 659 k)** with the tail multiplier
→ ~1 — bounded above by the venue's raw re-grade and by the daemon's
saturated inflight tail. If instead `ipc_ingress_ns` reads ~1 ms there,
the gap is the DRAIN FUNNEL's and the next campaign is daemon-side
(svc sweep economy at 80–85 %-CPU reapers — fusion's venue); either way
row 1's first stats read decides, which is what lever 3 was for.

## 5. Standing residuals (filed, not built)

- The cqe loom models' weakening sensitivity for (b)/(c) — §1.2.
- The svc park ceremony at `SPIN_US=0` (unchanged from the prior note).
- Multi-session-per-ctx batch parks use per-session k from per-session
  pending — a need-spread-across-sessions shape could oversleep to the age
  bound; bounded, and not the fio/field shape (one file → one session per
  ctx). Filed.

## 6. FIELD ADDENDUM (2026-08-08, squeeze-test, binary `82a9999f` — the merged branch; reconstructed from the field brief, exact histograms to be pasted verbatim below)

The §4 spec's row 1 ran post-storage-reset (32×32 rand-4k il, engagement
exact: **45.84 M ops**, ≈ **656 k IOPS** sustained). The first field read
of `ipc_ingress_ns` ADJUDICATED the §4 projection's either/or — **the gap
is the DRAIN FUNNEL's**:

| term | field value | share of clat |
|---|---|---|
| fio clat MEAN | 1,561 µs | — |
| **`ipc_ingress_ns` mean** | **~1,330 µs** | **~85 %** |
| device inflight mean (`ipc_direct_phase_ns`) | ~316 µs | ~20 % (overlaps nothing — sequenced after dequeue) |
| observation (residual) | ≈ 0 | the round-2 batch park holds in the field |

Ingress histogram shape (reconstructed): **71 % of ops in the 256–512 µs
bucket** (the funnel's steady-state queue), **10 % ≥ 2 ms**, including
**~810 k ops at 16–64 ms** — which accounts for the client p99 20 ms tail
**fully**. Little's law both ways: 656 k × 316 µs ≈ **210 concurrent
device ops** vs 656 k × 1,330 µs ≈ 870 — i.e. **~1,000 of the 1,024
in-flight ops pool PRE-DEQUEUE** while the device runs at a fifth of its
concurrency. Same-day raw re-grade: **2.486 M @ p99 4.5 ms** — the funnel
sits on ~2.5 M of device headroom.

Round-2 verdicts updated by this row: the batch park is **exonerated and
correct** (the field number was unmoved because the term was never
client-side at this shape — exactly what §3.5's attribution shift
predicted); the §4 projection's second arm fired verbatim ("if
`ipc_ingress_ns` reads ~1 ms there, the gap is the DRAIN FUNNEL's and the
next campaign is daemon-side"). Round 3 (`perf/shim-drain-funnel`) owns
the dequeue-side serialization.

(Orchestrator: paste the verbatim field histograms here.)
