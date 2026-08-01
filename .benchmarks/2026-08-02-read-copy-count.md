# 2026-08-02 — Read copy count: the closed READ copy ledger, two eliminations, and the NT read-serve falsification-of-the-falsification

Branch `perf/read-copy-count` (off dev tip `14d866e`, **unmerged — the
orchestrator merges**). Charter: build the READ copy ledger the write
path already has (`.benchmarks/2026-07-31-near-zero-copy.md`), then kill
what is eliminable. Inputs: the serve-latency decomposition
(`.benchmarks/2026-08-01-serve-decomposition.md`), the transport-ingress
verdict that the EXA read row is CPU-copy-governed
(`.benchmarks/2026-08-01-transport-ingress.md` §6 — the FIFO ceiling
probe converted nothing), and the near-zero-copy census's read table
(§1.2 — code-derived, never instrumented; its "eliminations available:
none" verdict is re-adjudicated here with counters).

Commits: red `df4d6b9` (ledger + E-IL1 + E-IL2 + NT-lever contracts) ·
green `53951bd` (the ledger wiring + both eliminations + the lever) ·
docs/closing (this note; SHAs recorded at merge). Field window
2026-08-01T12:03Z–…, journaled SESSION START/END + every row in
`/scratch/tmp/agent_runs.log`; artifacts `/scratch/tmp/rcc/rows/`.

## 1. The instrument (shipped, always-on)

Every daemon CPU pass over read payload bytes is attributed at its site
to exactly one stats-inode counter — closed accounting, not sampling:

| Counter | Site class |
|---|---|
| `read_copy_dest_bytes` | serve copies INTO the zero-copy final destination (registered uring ent payload / il arena dest): the hot/hold/tier/cold slice-out arms, ranged bounce-into-dest, multi-block dest assembly — **the ONE lawful serve copy** |
| `read_copy_bounce_bytes` | serve copies into intermediate heap memory (None-dest pooled-repr slices, tier mmap-guard copy-outs, multi-block pooled assembly) |
| `read_dest_dma_bytes` | device DMA landing DIRECTLY in the final destination (raw full-block dest leg, ranged zero-copy leg, ipc direct-drive aligned leg) — the zero-daemon-copy gauge |
| `read_fill_dma_bytes` | device DMA into pooled fill intermediates — the nvme-tcp RX-copy pricing denominator |
| `ipc_arena_copy_bytes` | the il boundary copy (`ArenaWindow::{write,write_at}`) |
| `ipc_read_dest_serves` | E-IL2 engagement: il cold reads served IN PLACE into the arena window |
| `nt_read_serve_bytes` | NT-store engagement at dest-arm serve copies (`SQUEEZEFS_NT_READ_SERVE`) |

Closure law (pinned by `tests/read_copy_ledger_tests.rs`, verified
exactly on every field row): kernel-path user bytes ≈ `dest + bounce +
dest_dma` (÷ the ramp factor); il user bytes ≈ `dest + arena +
dest_dma` (`ipc_bytes_out` ties it). The census column rides perf
uncore (`uncore_imc/cas_count_{read,write}`, whole-socket, full row
span) — **true DRAM bytes per payload byte**, available on the field
client's SPR sockets (the dev-rig LLC-miss proxy is retired for this
venue).

## 2. The ledger (field-measured, kernel + il, EXA cold shape)

**Venue (labeled once):** client squeeze-test (32 CPU / 2 NUMA /
dual-200GbE), substrate reset-v3 — nullblk 4-wide over **nvme-tcp**
(8 × 48 GiB data ns `nvme{4,6,8,10,12,14,16,18}n1`), meta
`nvme{0,2}n1`, cache-less format, 4 MiB blocks. Instrument fio-3.36 via
`tests/fio/run_fio_row.sh` (+ the campaign's census wrapper
`rcc_row.sh`: uncore DRAM sampler + mpstat + ledger extraction); rows
60 s + 10 s ramp; cold = fresh remount; settle discipline between
deploys; fill = fresh 16 × 8 GiB written this session (31.74 GB/s pass,
empty store). il rows: `SQUEEZEFS_IPC_MEM_MAX=8192` (the verified venue
posture), engagement-verified ≥ 0.90. Pairs (rocky8 container builds,
KD-7): **T** = `.rpd` (`e2efe52` — binary-identical to dev tip
`14d866e`, which is docs-only on top) · **C** = `.rcc` (`53951bd`).
Ramp caveat: counter totals are ramp-inclusive (≈ 1.16–1.19× the 60 s
fio window) — closure ratios state it.

### 2.1 Kernel FUSE path, cold 1 MiB libaio qd8 nj32 (26.0 GB/s, read_amp 0.69)

Per USER byte (ledger row rd-kern-C1; closure exact — `dest` ≡
user×ramp, `bounce` = 0, `fill_dma` ≡ device bytes):

| # | Move | Who | per user byte | Disposition |
|---|---|---|---|---|
| R-RX | nvme-tcp RX: skb → pooled fill buffer | kernel softirq | **0.69 CPU passes** (= read_amp; `read_fill_dma_bytes`/user = 0.691) | **interface-class** (no zero-copy TCP RX for nvme-tcp on this lineage) — counted, STOPPED on |
| R-F | fill DMA destination = the pooled 4 MiB `ALIGNED_BUF_POOL` buffer | — | 0 | already right: no intermediate bounce; `Bytes` refcount shares it to hold/hot/waiters |
| R-DEC | decode | — | 0 | passthrough volume (stated) |
| R-S | slice-out: block buffer (pool/hot/hold) → registered uring ent payload | handler lane | **1.00 CPU passes** (`read_copy_dest_bytes`/user = 1.00) | **load-bearing** (shared fills must land in private memory; the kernel consumes from the ent) — **cost cut by the NT lever, §4** |
| R-C | ent payload → app pages (fuse_uring commit) | kernel | **1.00 CPU passes** | **interface-class** (the K1 twin) — counted, STOPPED on |
| — | tier publish (ghost-admitted only) | blocking pool | ≈ 0 on this shape (`read_tier_admissions` trace-level) | governed (R1b) |

**Total: 2.69 CPU passes/user byte + 0.69 NIC DMA.** Measured DRAM
traffic: **11.8 B/B raw span (~10.1 ramp-adjusted)** — consistent with
~3 copies × ~3 fabric-B each + DMA + metadata machinery. The
before-number for this campaign's charter (~3 memory bytes/byte) was a
per-copy convention; the honest end-to-end DRAM column is ~10.

### 2.2 il shim path, cold 1 MiB psync nj32 — BEFORE vs AFTER

Pre-campaign (code-derived, T binary): cold il serve =
RX + slice-to-fresh-`Bytes` (**1 MiB heap alloc + full copy per op**,
`routing.rs` None-dest arm) + `payload.write` arena copy + client
`slab_read` = **RX + 3 CPU passes + 1 alloc/op**.

Shipped (C): **E-IL1** makes the None-dest slice a refcount share
(alloc + pass deleted); **E-IL2** hands the handler the op's validated
arena window as its serve dest (task-local, the registered-payload
twin; `SQUEEZEFS_IL_READ_DEST=0` restores the old posture), so the cold
serve lands IN PLACE: **RX + 1 daemon pass (fill → arena) + 1 client
pass (`slab_read`)** — copy-count parity with the kernel path.
Engagement on every C il row, exact: `ipc_read_dest_serves` ≡
`ipc_async_handoffs` (569,695 ≡ 569,695 on C1), `bounce` ≡ 0, closure
`dest + arena` ≡ `ipc_bytes_out` to the byte (597.39 + 1,789.92 =
2,387.3 GB ≡ 2,387.3 GB). Warm il serves (75 % of row ops: sync
fast-path hot serves) stay `write_at` serve-into-arena as before,
counted in `ipc_arena_copy_bytes`.

Exposure argument (the §5.2 boundary, stated): every byte a dest-armed
serve writes into the client-visible window is a binding-validated
serve of a file the session presented a kernel-granted fd for — the
same bytes the committed completion exposes; mid-serve partial
visibility (and 795-retry overwrites) are the client's own
concurrent-buffer POSIX hazard, exactly `ArenaWindow::write`'s standing
contract. DMA legs gained explicit dest-pointer 4 KiB gates (arena
windows are not alignment-guaranteed; unaligned dests ride the memcpy
arms).

## 3. Counted brackets (A-B-B-A, cold remount per leg, fixed read-only fill — no store aging on read rows)

**il cold EXA (psync 1M nj32; engagement 1.157–1.158 every leg):**

| leg (order) | GB/s | clat | read_amp | DRAM B/B (raw span) |
|---|---|---|---|---|
| T1 | 32.57 | 1.029 | 0.582 | 7.26 |
| C1 | 34.37 | 0.975 | 0.582 | 6.59 |
| C2 | 34.64 | 0.968 | 0.581 | 6.58 |
| C3 | 34.41 | 0.974 | 0.582 | 6.60 |
| T2 | 32.41 | 1.034 | 0.582 | 7.28 |

**Side medians 34.41 vs 32.49 GB/s = +5.9 %, DRAM/payload −9.3 %
(6.59 vs 7.27), read_amp identical** — the eliminated alloc+copy pass
converts on the copy-governed row, order-independent.

**kernel cold EXA (libaio 1M qd8 nj32) — no-regression side of the same
bracket:** T 26.21/25.72 vs C 26.06/26.00/26.03 (+C4 late control §4)
— **par** (the ledger adds are measurement-invisible), amp 0.68–0.70
both sides, DRAM/B 11.8 both sides.

## 4. The NT read-serve lever — the R2 disposition falsified by count

The near-zero-copy census (§1.2/§6) declared read serves "keep cached —
NT would trade an LLC hit for a consumer DRAM miss" **architecturally,
without a bracket**. Per the 2026-08-01 fastest-wins ruling, the lever
got its counted A/B (single binary, C; floor 256 KiB; small serves
structurally exempt):

| row (order) | GB/s | clat | DRAM B/B | engagement |
|---|---|---|---|---|
| C1/C2/C3 controls (lever off) | 26.06 / 26.00 / 26.03 | 10.27–10.29 | 11.76–11.82 | `nt_read_serve_bytes` 0 |
| NT1 (`SQUEEZEFS_NT_READ_SERVE=1`) | **28.95** | 9.248 | 10.11 | ≡ `read_copy_dest_bytes` exact (2,020.58 GB) |
| NT2 | **28.75** | 9.313 | 10.23 | exact |
| C4 (late control, after NT) | (§ field table) | | | 0 |

**+11 % on the EXA cold kernel row, DRAM/payload −14 %** — at
25+ GB/s the in-flight payload set (~256 × 1 MiB × pipeline) outruns
the LLC, so the consumer (the kernel's ent→user commit copy) pays DRAM
either way and the deleted destination RFO is pure win. The mechanism
the R2 disposition feared is real but priced: it applies to
LLC-resident shapes, which the 256 KiB floor exempts structurally
(rand-4k, warm fit-small never engage).

Default adjudication: (§ closing — pending the il-NT row and warm/rand
no-regression rows below).

## 5. No-regression table

(§ field rows being collected — qd32 T-vs-C on this fill, rand-4k cold
C-vs-T, warm fit-small, write EXA fresh/rewrite spots, il write parity,
tripwires.)

## 6. Rider — the write-side field audit (report-only)

(§ pending: DRAM B/B on the EXA write row, kernel + il, vs the 2-copy
ledger prediction with NT engaged.)

## 7. Soak

(§ pending: ≥ 600 s read-heavy + metadata storm, wedge indicators.)

## 8. What remains, honestly (the lawful floor)

On this venue the cold kernel read's copy chain after this campaign is:
**RX copy (interface) + ONE lawful serve copy (NT-cheapened) + kernel
commit copy (interface)** — the daemon-side count is 1 and cannot go
below 1 while shared fills, RAM tiers, and kernel-consumed replies
exist (scatter-DMA into cohort dests was examined and rejected: it
destroys the tier/hold landing that keeps read_amp < 1, i.e. it trades
a copy for device bytes and the amp gate). The il daemon-side count is
likewise 1. Everything else measurable in the DRAM column is
kernel-interface work (nvme-tcp RX, fuse_uring commit) or the client's
own POSIX buffer copy — the honest ceiling for FS-over-fabric reads
here.
