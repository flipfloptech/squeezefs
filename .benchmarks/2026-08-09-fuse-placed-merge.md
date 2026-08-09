# Approach A — the FUSE placed-merge assembly (bounded falsification, 2026-08-09)

Branch `perf/fuse-placed-merge` off dev `71f2e967` (the write-bandwidth
program adjudication — rc-manifest §3f is this campaign's law). Charter:
port the IPC placed-sever design to FUSE delivery so the armed 1 MiB
streaming path stops paying an extraction destination (slot → memfd
bounce) before its real accumulation destination (NT merge →
`ActiveBlockBuf`); WRITE_FIXED cannot copy fixed→fixed, so the assembly
is a **memfd whose mmap view is adopted as the ActiveBlockBuf backing**.
Bounded falsification: exact engagement without throughput conversion is
a REPORTED falsification that redirects to Approach B.

## 1. Step 0 — the mandatory red gate (term confirmed on `71f2e967`)

Venue: tcp devsub (nvmet-tcp on lo, 4× nullb meta + 4× 8 GiB zram data),
armed default mount, fio 3.42 libaio direct=1 `bs=1M iodepth=8 numjobs=8
nrfiles=4 size=512m runtime=45 ramp=0` (one accounting window — device
deltas and fio io_bytes must share it). Rig:
`.benchmarks/rigs/2026-08-09-placed-merge-step0.sh` (all stats reads are
python over the `.stats` JSON — the colon-space law; artifacts
`/run/sqz-placed/step0-red2`, persisted copy in the artifacts dir).

| gauge | value | verdict |
|---|---|---|
| row | io 60.18 GB, **1.332 GB/s** | the red baseline |
| `fuse3_zc_write_extract_bytes` | 60,181,970,944 = **100.0 %** of user bytes | ✓ every streaming byte pays the bounce destination |
| `fuse3_zc_write_direct_bytes` | 0 = **0.0 %** | ✓ the direct vehicle never engages on streaming |
| `nt_copy_bytes` | 60,148,416,512 = **99.9 %** | ✓ every byte pays the second (NT merge) copy |
| placed severs / adoptions / elides | 0 / 0 / 0 | kernel lane has no placed machinery (the term this campaign builds) |
| `write_pipeline_phase_ns.dma` mean | 115.7 ms (n=14.8k) | saturation queue residence (baseline attribution) |
| `write_pipeline_phase_ns.total` mean | 295.0 ms | 〃 |
| `write_transport_phase_ns.transport_total` mean | 43.9 ms (queue_wait 5.5 µs) | 〃 |
| amplification / wareq-sz | **1.044** / 4,089 KiB | ✓ ≤ 1.05, no request-size collapse |
| `data_write_lanes` | 8 | — |
| `data_write_lane_submits` | 4 devices × 8 lanes, **all 8 moving per device, max share 13–14 %** | ✓ **SPREAD — the campaign-stop precondition passes** |

## 2. The design — cohort-capture mechanics

DESIGN

## 3. Red contracts

RED

## 4. Brackets

BRACKETS

## 5. Verdict

VERDICT

## 6. Gates

GATES
