# 2026-09-09 — inline-ceiling sweep (LOCAL SCOPING): the raise is priced out as a default

**Verdict: the inline ceiling stays at the shipped 4 KiB (one page) by
default. Raising it moves the payload onto the metadata plane at ≈ 2× the
payload in bytes (journal entry + CoW node append) and costs small-file
throughput in proportion — −34 % at 16 KiB, −68 % at 32 KiB — and at
32 KiB the churn exhausted a 1 GiB metadata volume in 20 s and
FAIL-STOPPED it. The size-dispatching promotion and the override stay as
operator/measurement levers; the 4 KiB–4 MiB population's answer is
PACKING (data-plane bytes, metadata-plane mapping), the next campaign.**

Scoping venue per the venue rule (acceptance rows run on squeeze-test) —
the numbers pick the direction, not the release default's exact value.

## 1. Venue

| | |
|---|---|
| box | the dev laptop (32 CPUs, `7.2.3-cachyos-lto`), release build of `aec1241f` (phase A) |
| substrate | tcp devsub: 4 × 1 GiB null_blk metadata volumes + 4 zram data namespaces over nvmet-tcp on localhost; format WITH a RAM-backed staging dir (`/dev/shm`, `--disk-cache-size 16GB`) |
| grid | ceiling T ∈ {4096 (shipped), 16384, 32768, 61440 (the format bound)} × file size S ∈ {4, 16, 32, 60 KiB}; per cell an **fsync row** (24 jobs, one S-sized file per op, `fsync_on_close`, 20 s) and a **create row** (same, no fsync); fresh format + mount per leg |
| rig | `.benchmarks/rigs/2026-09-09-inline-raise-sweep-local.sh` → `2026-09-09-inline-raise-sweep-reduce.py` |
| completed | 11 of 16 legs — the T = 32768 / S = 32 KiB **create** row fail-stopped metadata volume 1 (§3), which ended the sweep; T = 61440 was never reached and is moot |

## 2. The rows (fsync per file; the create rows agree)

| ceiling T | size S | posture | files/s | fsync µs | journal B/file | node-append B/file | daemon µs/file |
|---|---|---|---|---|---|---|---|
| 4096 | 4 KiB | inline (shipped) | 16,725 | 322 | 4,681 | 6,392 | 817 |
| 4096 | 16 KiB | **staged (shipped)** | 16,255 | 278 | 308 | 309 | 883 |
| 16384 | 16 KiB | **inline** | 10,676 (**−34 %**) | 546 (**2.0×**) | 18,264 (**59×**) | 15,537 (**50×**) | 981 |
| 4096 | 32 KiB | staged (shipped) | 15,160 | 278 | 311 | 309 | 943 |
| 32768 | 32 KiB | **inline** | 4,838 (**−68 %**) | 917 (**3.3×**) | 36,475 (**117×**) | 29,653 (**96×**) | 1,787 |
| 4096 | 60 KiB | staged (shipped) | 14,682 | 280 | 312 | 299 | 969 |

Create rows: 16 KiB inline 14,474 vs staged 25,586 files/s (**−44 %**);
32 KiB inline 11,056 vs 26,346 (**−58 %**) before the fail-stop. The
4 KiB cells are lever-neutral (always inline) and agree within noise across
ceilings — the raise costs nothing where it changes nothing.

The mechanism: an inline file's payload IS its layout record, so every
write of it rides the metadata plane twice — once as the checksummed
journal entry, once as the CoW node append — and both scale with the
payload (16 KiB file → 18 KB journal + 15.5 KB node log per file). The
staged path's metadata cost is a fixed ≈ 0.3 KB per file regardless of
size; the payload rides one local NVMe put. That is exactly why the inline
layout was one page: at 4 KiB the record's payload costs about what a
block mapping would; above it the metadata plane becomes the data plane
for small files, and it is not built to be one.

## 3. The fail-stop (a finding beside the sweep — P1)

At T = 32768, 20 s into the 32 KiB create row, metadata volume
`/dev/nvme2n1` (1 GiB) reported `checkpoint: metadata heap exhausted
(free=0, reserve=80) … compaction deferred` on twelve nodes in one tick,
then `pending-free retirements wedged (61 parked) and 8 consecutive
barriered checkpoint cycles neither released one nor advanced the ledger
tail … volume marked FAILED` (the §4.7 wedged-tail bound), and every
subsequent op on that volume answered `Metadata volume 1 is disabled`
(EIO to fio; `writeback error latched` on two inodes). The heap was
genuinely full — 11k × 32 KiB inline files at ≈ 2× payload on a 1 GiB
volume — but the outcome a full metadata volume should produce is
**ENOSPC on the metadata plane**, not a fail-stop with EIO: the CoW
compaction that frees space needs free space, the pending-free protocol
parks behind it, and the wedged-tail detector reads the standstill as
corruption. Pre-existing (any workload that fills a metadata volume hits
it); the raise made it reachable in 20 s. Board item: "metadata volume
full → fail-stop instead of ENOSPC" — a robustness campaign with a
red-first repro (fill a small meta volume; assert ENOSPC, not EIO, and no
`FAILED`).

## 4. What this decides

* **Default ceiling = 4 KiB** (the `INLINE_MAX_FLOOR`, one page — now the
  DERIVED default, not the fallback): phase A's derivation returned the
  format bound (60 KiB) as the default on the reasoning that "anything
  smaller is the trade the sweep prices" — the sweep priced it: −34 %/−68 %
  throughput and 50–117× metadata bytes per file. The format bound stays
  what it is — the MAXIMUM the override may name.
* **`SQUEEZEFS_INLINE_MAX_BYTES` stays** (4096..=format bound, refusal
  outside) as the operator's lever for a set with generously sized
  metadata volumes and a hard small-file-visibility requirement, and as
  the measurement lever.
* **The size-dispatching promotion stays**: with the default ceiling it
  dispatches nothing inline (every staged file is by definition above the
  ceiling), so the dismount pass's 64× space law (`.benchmarks/2026-09-09-fsync-promote-staged-ab.md`
  §3) is NOT fixed by this campaign — it is fixed by packing.
* **Next: the packing campaign** — many small files' payloads share one
  striped block on the DATA plane via size-carrying sub-block mappings;
  the metadata plane carries only the mapping (the staged path's ≈ 0.3 KB
  economy), the C8 ledger already counts one record per reference, and the
  partial-free / compaction story rides the defrag D1 axis. Design first.
* The READ fast-probe inline arm phase A added is a correct fix regardless
  (every inline read used to demote off the reap thread) and stays.
