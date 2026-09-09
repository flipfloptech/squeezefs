# 2026-09-09 — pricing "fsync promotes a staged-layout file" (step 3 of the dismount-residue program)

**Verdict: unconditional promote-on-fsync (option A) is REJECTED, and not on
latency alone — the promotion PRIMITIVE takes one whole 4 MiB striped block
per file, so fsync'ing small files fills the set 64× faster than the data.
Option B (multi-client-only) inherits the same primitive and the same
space law. The lever `SQUEEZEFS_FSYNC_PROMOTE_STAGED` stays OFF; the design
question moves from "when to promote" to "what a small file promotes INTO".**

## 1. Venue

| | |
|---|---|
| box | squeeze-test, kernel `6.19.14-sqz`, the 5-node nvme-tcp set (5 meta + 10 data namespaces, **480 GiB data capacity**, 12,288 × 4 MiB blocks per data volume) |
| format | WITH a staging dir — `/dev/shm/sqz-staging` (RAM-backed: the box's local disk is SATA, slower than the fabric; the staging term must not be the row), `--disk-cache-size 64GB` (the rows stay under the 75 % high-water arm, so pressure promotion cannot confound the lever) |
| binary | ONE binary, `77d726f1` rocky8 `release` build (the lever commit); arms A = `SQUEEZEFS_FSYNC_PROMOTE_STAGED=0` (shipped default), B = `1` |
| order | **A B B A**, 15:57–16:13Z, fresh reset + format per arm, `write_BW` 40 s prep |
| rows | fsync-storm (24 × 256 KiB writes, fsync per write, one 256 MiB file per job), **smallf-fsync** (24 jobs × 64 KiB files, `fsync_on_close`, ≈ 500 k fsyncs per 30 s row), wdur-kern (`write_BW` + `--end_fsync=1`), rr4k-kern |
| rig / reducer | `.benchmarks/rigs/2026-09-09-fsync-promote-abba.sh` (the meta URI comes from each reset's printed mount line; namespace paths are checked to be block devices; the mountpoint is cleared — three faults the first launches found) → `2026-09-08-campaign-rows-reduce.py` (fsync-promotion columns added) |

## 2. The rows (medians of 2, B/A)

| row | what the lever touched | fsyncs/s or IOPS | fsync total µs | `staged_promote` µs | p99.9 | daemon µs/op | promotions / failures |
|---|---|---|---|---|---|---|---|
| **smallf-fsync** (THE pricing row) | every fsync'd 64 KiB file | **−16.7 %** (6,184 → 5,151 files/s) | 338 → **425 (+26 %)** | 0 → **128** | **+122 %** (389 → 864 µs) | +12 % | **140,183 promoted / 132,439 FAILED `StorageFull`** |
| fsync-storm | almost nothing — each job rewrites ONE 256 MiB file (striped, not staged); 384 promotions of the first 256 KiB image | +0.9 % | 6,761 → 6,699 | 43 | −3.7 % | −0.6 % | 384 / 0 |
| wdur-kern (striped stream, lever-neutral) | nothing directly — but it ran on the set the small-file row had FILLED | **−29.9 %** (33.8 → 23.7 GB/s) | | | +481 % | +60 % | `transport_lease_overlong` 1,038 on B2 (write handlers > 1 s under backpressure — the full-set shape) |
| rr4k-kern | nothing | +0.1 % | | | −7 % | +0.3 % | |

Every B-arm number on the small-file row is a MIX of two costs that the
row cannot separate: the promotion itself (one `allocate_placed_block` +
one 4 MiB `write_block` + one meta commit per fsync — the 128 µs
`staged_promote` mean is that, amortised with the ENOSPC refusals that
followed) and the ENOSPC refusals once the set was full (the pre-lever
contract held: the file stays durable in local staging, the fsync
succeeds, `fsync_promote_failures` counts it, one WARN per failure —
132 k WARN lines in 30 s, which is its own problem).

## 3. The space law is the finding

`promote_staged_file` (`src/routing.rs:13414`) allocates **one placed
striped block per file** (`allocate_placed_block`, a size-carrying
`bk:0:packed_len` mapping). For a file that will keep growing that is the
right promotion; for the small-file population it is the 4 MiB block
economy applied to 64 KiB of data — **64× space amplification**:

* 140,183 promotions × 4 MiB = **548 GiB** against a 480 GiB set — the set
  filled ≈ 8 s into the first B row (all 10 data volumes report
  `12288 of 12288 blocks allocated`); the ≈ 30 GB of user data those files
  carry would have fit 16× over;
* the same law governs **step (2)'s dismount promotion** (landed
  `5e47c1c2`): the fstests TEST device's 2,193 resident staged files become
  ≈ 8.6 GiB of blocks at every clean unmount for ≈ 100–140 MiB of data.
  Correct semantics (durable + visible everywhere) at a real, permanent
  space cost until the files are deleted or rewritten — and an unmount on a
  nearly full set fails its promotions (counted, entries stay resident,
  never lost) rather than helping.

The staged layout exists precisely because small files do not fit the
block economy; promoting them one-per-block does not change that, it
moves the cost from "invisible elsewhere" to "64× the space".

## 4. What this decides

* **Option A** (promote on every fsync): rejected — +26 % per small-file
  fsync and p99.9 +122 % BEFORE the space law, and the space law makes it
  inadmissible on any set with a small-file population.
* **Option B** (promote on fsync only for multi-client sets): the same
  primitive, the same space law, the same rejection — the WHEN was never
  the problem.
* **Option C** (bounded staleness by a periodic sweep): same primitive
  again.
* **The lever** stays registered and OFF as the measurement lever it is.

The question that remains is a **design item for the board**: what does a
small staged-layout file promote INTO so that every client can read it at
a space cost proportional to its size? Candidates the evidence points at:
(i) a **small-file packing layout** — many staged files share one striped
block via size-carrying sub-block mappings (the `bk:0:packed_len` form
already carries a length; the allocator and the C8 refcount ledger would
need shared-block ownership for it); (ii) **cross-client read-through** —
a reader of a staged-layout file fetches the bytes from the owning host
over the cluster wire (the S8 function-shipping plane already carries
verbs to the owner; a `ReadStaged(file_id, range)` is one more), leaving
the bytes local until pressure promotes them; (iii) raising the inline
threshold for fsync'd small files (meta-resident, no block) — bounded by
the KV value cap. Each wants its own counted rows; none is a lever flip.

## 5. Step (2) in this light — the owner's call

The dismount promotion is landed and gated (`9ac570ab`), not released
(1.2.3-bound). It keeps its correctness argument (a clean unmount is a
durability boundary for every other custody class, and "Dismount clean"
now means other clients read the files) but at the space cost in §3.
Options: keep it as is (visibility over space; the failure arm is safe);
make it size-aware (promote only files above a derived fraction of the
block, e.g. those the packing/inline candidates would not serve — a
derived threshold, not a constant); or hold it behind the same
measurement posture as the fsync lever until the small-file primitive
exists. The note leaves this to the owner with the numbers above.
