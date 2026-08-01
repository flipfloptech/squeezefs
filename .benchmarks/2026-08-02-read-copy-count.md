# 2026-08-02 — Read copy count: the closed READ copy ledger, three eliminations, and the NT read-serve re-adjudication

Branch `perf/read-copy-count` (off dev tip `14d866e`, **unmerged — the
orchestrator merges**). Charter: build the READ copy ledger the write
path already has (`.benchmarks/2026-07-31-near-zero-copy.md`), then kill
what is eliminable. Inputs: the serve-latency decomposition
(`.benchmarks/2026-08-01-serve-decomposition.md`), the transport-ingress
verdict that the EXA read row is CPU-copy-governed
(`.benchmarks/2026-08-01-transport-ingress.md` §6 — suppressing ingress
waits converted nothing), and the near-zero-copy census's read table
(§1.2 — code-derived, never instrumented; its dispositions are
re-adjudicated here with counters and brackets).

Commits: red `df4d6b9` (ledger + E-IL1 + E-IL2 + NT-lever contracts) ·
green `53951bd` (the ledger wiring + both il eliminations + the lever) ·
`0db3f6a` (NT default ON for ring-ent dests + the arena-dest exemption —
both driven by the field brackets below) · docs (this note). Field
window 2026-08-01T12:03Z–15:00Z, journaled SESSION START/END + every row
in `/scratch/tmp/agent_runs.log`; artifacts `/scratch/tmp/rcc/rows/`
(per-row fio json, stats before/after/delta, uncore perf CSV, mpstat,
census.json).

## 1. The instrument (shipped, always-on)

Every daemon CPU pass over read payload bytes is attributed at its site
to exactly one stats-inode counter — closed accounting, not sampling:

| Counter | Site class |
|---|---|
| `read_copy_dest_bytes` | serve copies INTO the zero-copy final destination (registered uring ent payload / il arena dest): hot/hold/tier/cold slice-out arms, ranged bounce-into-dest, multi-block dest assembly — **the ONE lawful serve copy** |
| `read_copy_bounce_bytes` | serve copies into intermediate heap memory (None-dest pooled-repr slices, tier mmap-guard copy-outs, multi-block pooled assembly) |
| `read_dest_dma_bytes` | device DMA landing DIRECTLY in the final destination (raw full-block dest leg, ranged zero-copy leg, ipc direct-drive aligned leg) — the zero-daemon-copy gauge |
| `read_fill_dma_bytes` | device DMA into pooled fill intermediates — the nvme-tcp RX-copy pricing denominator |
| `ipc_arena_copy_bytes` | the il boundary copy (`ArenaWindow::{write,write_at}`) |
| `ipc_read_dest_serves` | E-IL2 engagement: il cold reads served IN PLACE into the arena window |
| `nt_read_serve_bytes` | NT-store engagement at ring-ent dest serve copies (`SQUEEZEFS_NT_READ_SERVE`) |

Closure law (pinned by `tests/read_copy_ledger_tests.rs`, verified
EXACT on every field row): kernel-path `dest + bounce + dest_dma ≡ user
bytes × ramp factor`; il `dest + arena + dest_dma ≡ ipc_bytes_out` (to
the byte: 597.39 + 1,789.92 = 2,387.3 GB ≡ `ipc_bytes_out` on rd-il-C1).
The census column rides perf uncore
(`uncore_imc/cas_count_{read,write}`, whole-socket, full row span) —
**true DRAM bytes per payload byte** on the field client's SPR sockets
(supersedes the dev-rig LLC-miss proxy for this venue). Counter totals
are ramp-inclusive (≈ 1.16–1.19× the 60 s fio window — the standing
caveat); DRAM column raw-span unless marked adjusted.

## 2. Venue (labeled once)

Client squeeze-test (32 CPU / 2× Xeon Gold 6426Y / 2 NUMA /
dual-200GbE), substrate reset-v3 — nullblk 4-wide over **nvme-tcp**
(8 × 48 GiB data ns `nvme{4,6,8,10,12,14,16,18}n1`), meta `nvme{0,2}n1`,
cache-less format, 4 MiB blocks. Instrument fio-3.36 via
`tests/fio/run_fio_row.sh` + the campaign census wrapper (`rcc_row.sh`);
rows 60 s + 10 s ramp (sustained row 120 s); cold = fresh remount;
settle discipline between deploys; fill = fresh 16 × 8 GiB written this
session onto an EMPTIED store (31.74 GB/s fill pass; every read row runs
32 jobs = 2 sequential readers/file — stated, it shapes amp vs the
prior 32-file sessions). il rows: `SQUEEZEFS_IPC_MEM_MAX=8192` (the
verified venue posture), engagement ≥ 0.90 verified per row. Pairs
(rocky8 container builds, KD-7 verified): **T** = `.rpd` (`e2efe52` —
binary-identical to dev tip `14d866e`, docs-only delta) · **C** =
`.rcc` (`53951bd`) · **C′** = `.rcc2` (`0db3f6a`, the final branch
binary). One incident, journaled + contained: an ad-hoc profiling fio
laid out 15 stray files into the fixed set (killed; strays deleted —
layout writes touch new names only; its captures discarded; the store's
thin-state aging from it is visible as a −3…−8 % venue drift on later
absolute numbers, which is why every late verdict is a
matched-venue-state sandwich).

## 3. The ledger (field-measured, closed)

### 3.1 Kernel FUSE path, cold 1 MiB libaio qd8 nj32 (26.0 GB/s, read_amp 0.69)

| # | Move | Who | per user byte (measured) | Disposition |
|---|---|---|---|---|
| R-RX | nvme-tcp RX: skb → pooled fill buffer | kernel softirq | **0.69 passes** (`read_fill_dma_bytes`/user = 0.691 ≡ read_amp) | **interface-class** — counted, STOPPED on |
| R-F | fill DMA destination = pooled 4 MiB `ALIGNED_BUF_POOL` buffer | — | 0 | already right (no intermediate bounce; `Bytes` refcount shares to hold/hot/waiters) |
| R-DEC | decode | — | 0 | passthrough volume (stated) |
| R-S | slice-out: block buffer → registered uring ent payload | handler lane | **1.00 passes** (`read_copy_dest_bytes`/user = 1.000; `bounce` = 0, `dest_dma` = 0) | **load-bearing** (shared fills must land in private memory; the kernel consumes from the ent) — **cost cut: NT stores, §5** |
| R-C | ent payload → app pages (fuse_uring commit) | kernel | 1.00 passes | **interface-class** (the K1 twin) — counted, STOPPED on |
| — | tier publish (ghost-admitted) | blocking pool | ≈ 0 on this shape | governed (R1b) |

**Total: 2.69 CPU passes/user byte + NIC DMA.** Measured DRAM: **11.8
B/B raw span (~10.1 ramp-adjusted)** pre-NT — consistent with ~3
copies × ~3 fabric-B + DMA + machinery. (The charter's "~3 memory
bytes/byte" was the per-copy convention; the honest end-to-end DRAM
column is ~10–12.) rand-4k corroboration: the ranged path serves with
**zero daemon copies** (`dest_dma` ≡ device bytes, r4k rows) — its DRAM
28 B/B is per-op machinery over 4 KiB payloads, not copies.

### 3.2 il shim path, cold 1 MiB psync nj32 — BEFORE vs AFTER

Pre-campaign (T): cold il serve = RX + slice-to-fresh-`Bytes` (**1
MiB-class heap alloc + full copy per op** — the None-dest
`copy_from_slice` bounce) + `payload.write` arena copy + client
`slab_read` = **RX + 3 CPU passes + 1 alloc/op**.

Shipped: **E-IL1** (zero-copy `Bytes` slice of the pooled fill — alloc
+ pass deleted) + **E-IL2** (the handler serves INTO the op's validated
arena window: task-local dest override, the registered-payload twin;
`SQUEEZEFS_IL_READ_DEST=0` restores the old posture) ⇒ cold serve =
**RX + 1 daemon pass (fill → arena) + 1 client pass** — copy-count
parity with the kernel path. Engagement exact on every C il row:
`ipc_read_dest_serves ≡ ipc_async_handoffs` (569,695 ≡ 569,695),
`bounce ≡ 0`, closure to the byte. Warm il serves (≈ 75 % of row ops —
sync fast-path hot serves) keep the §5.5.1 `write_at` serve-into-arena,
counted in `ipc_arena_copy_bytes`. Exposure argument (the §5.2
boundary): dest-armed serves write only binding-validated bytes of a
file the session holds a kernel-granted fd for — the same bytes the
committed completion exposes; mid-serve partial visibility is the
client's own concurrent-buffer POSIX hazard (`ArenaWindow::write`'s
standing contract). DMA legs gained explicit dest-pointer 4 KiB gates
(arena windows are not alignment-guaranteed).

## 4. Counted brackets — the eliminations convert

**il cold EXA A-B-B-A (psync 1M nj32, cold remount per leg, fixed
read-only fill; engagement 1.157–1.158 every leg):**

| leg (order) | GB/s | clat | read_amp | DRAM B/B |
|---|---|---|---|---|
| T1 | 32.57 | 1.029 | 0.582 | 7.26 |
| C1 | 34.37 | 0.975 | 0.582 | 6.59 |
| C2 | 34.64 | 0.968 | 0.581 | 6.58 |
| C3 | 34.41 | 0.974 | 0.582 | 6.60 |
| T2 | 32.41 | 1.034 | 0.582 | 7.28 |

**Side medians 34.41 vs 32.49 GB/s = +5.9 %, DRAM/payload −9.3 %,
read_amp identical** — the deleted alloc+copy pass converts on the
copy-governed row, order-independent. Sustained rule: 120 s flat row
**33.96 GB/s** (per-10 s device bytes 167.8–171.9 GB/10 s, no decay
trend), engagement 1.079 valid.

**kernel cold EXA (same bracket, libaio qd8):** T 26.21/25.72 vs C
26.06/26.00/26.03 — **par** (the ledger's relaxed adds are
measurement-invisible; E-IL1/E-IL2 don't touch this path).

## 5. The NT read-serve re-adjudication (the fastest-wins mechanism, both directions)

The near-zero-copy census (§1.2/§6) declared read serves "keep cached —
NT trades an LLC hit for a consumer DRAM miss" architecturally, without
a bracket. The lever got its counted A/Bs, and the answer is
**dest-class-dependent**:

**Ring-ent dests (kernel transport) — NT WINS.** Fresh venue: controls
26.06/26.00/26.03/25.76(late) vs NT1 **28.95** / NT2 **28.75** (+11 %,
clat 10.28 → 9.25 ms, DRAM/payload 11.8 → 10.1). Reproduced
A-B-B-A on the final binary at the aged venue: default(NT)
**26.36/26.70** vs `=0` **24.07/24.48** (**+9.3 %**, DRAM 12.1 → 10.6),
order-independent. qd32: 21.74 (no NT) → **23.92** (NT default, amp
1.071 → 0.978 — same-venue single reps). Engagement exact everywhere
(`nt_read_serve_bytes ≡ read_copy_dest_bytes`). Mechanism: at 25+ GB/s
the in-flight payload set outruns the LLC, the kernel's commit copy
pays DRAM either way, and the deleted destination RFO is pure win.

**Arena dests (il E-IL2 serves) — NT LOSES.** rd-il under the lever:
33.44 vs 34.4-class controls (**−2.8 %**) — the client's `slab_read`
consumes those lines within ~one op, exactly the census's feared
mechanism, real at THIS consumer distance.

**Shipped posture (`0db3f6a`):** `SQUEEZEFS_NT_READ_SERVE` **default
ON**, applied ONLY to ring-ent dests — arena dests are **structurally
exempt** (`ReadClassHint::dest_arena` → `routing::serve_copy_to_dest`
cached; pinned by the ledger suite's il test: nt gauge 0 on arena
serves even under default-ON), floor 256 KiB keeps rand-4k/warm-small
serves cached, `=0` is the A/B escape. The write-side `nt_copy`
policy/floor is untouched.

## 6. No-regression table (final binary C′ = `0db3f6a` unless noted)

| row | T (dev tip) | branch | Δ | verdict |
|---|---|---|---|---|
| EXA read kern 1M qd8 cold (GB/s) | 26.21/25.72 | C 26.0-26.1 (NT off) / **C′ 26.3-26.7 (NT default)** | par → **+2…+4 %** | ≥ par; DRAM/B 11.8 → 10.6 |
| EXA read il 1M psync cold (GB/s) | 32.49 med | **C 34.41 med**; C′ 32.50 vs old-C-same-venue 33.21 (aged-venue pair, −2 % in-spread) | **+5.9 %** (A-B-B-A) | WIN |
| read qd32 1M cold (GB/s) | 22.03 | C 21.74 / **C′ 23.92** | −1.3 % / **+8.6 %** | ≥ par (NT default); amp 1.043 → 0.978 |
| read_amp qd8 / qd32 | 0.693 / 1.043 | 0.68–0.72 / 0.978 | — | no amplification regression (2-readers/file fill shape stated) |
| rand-4k cold beyond-budget (IOPS) | 271.5k → 236.7k across session (13 % same-binary swing tracking venue aging) | final matched sandwiches: T 238.4k med vs C′ 231.9k | **−2.8 % bounded** | par-within-spread; interiors PAR (serve total 0.371 vs 0.366 ms, all terms uniformly spread — no attributable site; perf captures retained `/tmp/perf_{C,T}.data`); **flagged to the orchestrator** |
| warm fit-small 4k (IOPS) | 331,183 | 333,453 (C) | +0.7 % | par (sub-floor: NT never engages) |
| write EXA fresh / rewrite (GB/s) | 32.58/32.62 anchors (rpd) | 32.28 / 32.35 (C) | −0.9 % | par (write path untouched by this campaign) |
| tripwires (`bounce` on il rows, `ipc_descriptor_rejects`, `ipc_sessions_poisoned`, `write_path_seed_read_bytes`, `patch_edge_rmw_reads`, `fsck_findings`, `write_pipeline_fence_drops`, `block_double_frees`) | — | 0 across every row + both soaks | — | clean |

## 7. Rider — the write-side field audit (report-only)

Same census instrument on the EXA write rows (C binary; NT write levers
at their shipped defaults, `nt_copy_bytes` engaged):

| row | GB/s | write_amp | DRAM B/B raw (ramp-adj) |
|---|---|---|---|
| wrk fresh (libaio 1M qd8 nj32, fresh dir) | 32.28 | 1.124 | 8.42 (~7.2) |
| wrk rewrite (same dir) | 32.35 | 1.166 | 8.75 (~7.5) |
| wri il (psync 1M nj32, engagement 1.173) | 25.69 | 1.170 | 7.22 (~6.2) |

**Verdict: the field MATCHES the 2-copy ledger.** Kernel path ≈ 7.2:
K1 cached copy (~3) + M1 NT merge (~2) + TX splice DMA × amp (~1.1) +
meta/journal machinery ≈ 6–7 predicted. il ≈ 6.2: S1 cached app→arena
(~3) + S2 NT sever (~2) + DMA (~1.2). Nothing pays beyond the ledger.
(The wri row is a psync-instrument row on a rewritten dir — labeled;
not a matched-instrument parity comparison. The write path is
byte-identical to dev tip in this branch.)

## 8. Loaded soaks (both PASS)

Two 600 s soaks — kernel read plane (1M qd8 nj16 time_based on the
fixed set) + the 8-worker metadata storm (create/write-4k/stat/rename/
unlink + dir churn) + syncfs every 10 s; wedge indicators sampled every
30 s ×21:

* C (`53951bd`): **25.02 GB/s** for 600 s, ledger closure 1.017 over
  the whole window, 8/8 storm workers alive, every indicator 0.
* **C′ (`0db3f6a`, the final binary, NT default engaged): 28.66 GB/s
  for 600 s (+14.5 % over the pre-NT soak under the identical storm),**
  DRAM/payload 8.43, closure 1.016, 8/8 workers alive, every indicator
  0 across all 21 samples, no non-kernel D-states after quiesce, clean
  umount + standing-pair restore.

## 9. What was eliminated vs priced vs declared (the campaign summary)

| Item | Verdict |
|---|---|
| il cold slice bounce (1 MiB alloc + copy/op) | **ELIMINATED** (E-IL1 — refcount slice) |
| il `payload.write` arena copy on cold serves | **ELIMINATED** (E-IL2 — serve-into-arena-dest; engagement gauge `ipc_read_dest_serves`) |
| kernel slice-out (block buffer → ring ent) | load-bearing (1 lawful copy) — **COST CUT** −1 DRAM B/B class via NT default (counted +9…11 %) |
| multi-block pooled `final_buf` full-length re-copy | **ELIMINATED** (`PooledBuf::into_bytes` zero-copy view) — off the EXA shapes, unmeasured, structural |
| nvme-tcp RX copy (0.58–0.72 passes/user byte at these amps) | **interface-class** — priced, stopped on |
| fuse_uring ent→user commit copy (1.0) | **interface-class** (K1's read twin) — priced, stopped on |
| client `slab_read` arena→app copy (il) | structural POSIX (the app hands us ITS buffer) — priced |
| fill DMA landing | already right (pooled, refcount-shared) |
| scatter-DMA into cohort dests (kill the serve copy entirely) | REJECTED by analysis: destroys the tier/hold landing that keeps read_amp < 1 — trades a CPU pass for device bytes and the amp gate; re-open only if a fabric with free bandwidth AND a CPU wall wants it |

## 10. Honest ceiling + residuals (ranked)

1. **Reads are now at their lawful daemon copy floor: ONE CPU pass per
   served byte on both transports.** Everything else in the DRAM column
   is kernel-interface work (RX + commit ≈ 1.7 passes/user byte at amp
   0.7) or client-side POSIX. Further conversion on the EXA read row
   is interface-class (kernel FUSE zc-receive / registered-buffer
   replies; devmem-TCP-class RX) — charter-revision territory, not
   daemon work.
2. **The DRAM census beyond copies** (~10.6 B/B kern vs ~4.4 predicted
   from copies alone): per-op machinery (meta probes, counters, page
   tables, prefetch/lane bookkeeping) at 25k × 1 MiB ops/s. A
   dhat/alloc-trace pass on the EXA shape would name the next
   sub-terms; expected win class ≤ 10 %.
3. **rand-4k −2.8 % bounded residual** on the final sandwiches (no
   attributable site; interiors par; the row's same-binary session
   swing is ±13 %). Recommend: one clean-venue ×10 matrix before any
   attribution work; perf captures retained.
4. **il warm serves** (75 % of the loop row) still pay `write_at`
   cached — correct per the arena-consumer-distance law this campaign
   measured; NT there is a counted negative.

## 11. Client state

Standing mount (`b4edafc` pair) restored + verified armed at SESSION
END; campaign pairs retained at `/scratch/tmp/{squeezefs,libsqueezefs_il.so}.rcc`
(`53951bd`) and `.rcc2` (`0db3f6a` — the branch binary); helper scripts
+ per-row artifacts under `/scratch/tmp/rcc/`; `rcc/` (16×8g) + `warm/`
+ `wrc/` filesets left on the store (normal bench artifacts); store
settled. No resets, no reformats, no raw-device writes, no storage-node
changes. Incidents journaled: (1) the stray-layout profiling fio
(§2 — contained, artifacts discarded); (2) session-long venue drift on
absolute numbers after the write-heavy middle third (every verdict
above is a matched-state bracket).
