# Rung 9 — release gates: the index and the rung's verdict

The consolidation of rung 9 (charter §13 step 9, §12). Six notes carry the detail; this note is the
index, the headline numbers, the defect count, and the owed list in one place. Decision 0003 §H–§M;
decision 0002 rows 442–482. `dev` moved from `ce0edf4` (rung 8) to the HEAD this note names in §6,
169 commits by twelve parallel agents in their own worktrees, each landing with `--ff-only`.

| Note | What it proves |
|---|---|
| `rung-09-external-suites.md` | pjdfstest / LTP / xfstests as hard gates with adjudicated ledgers; 27 product defects found by them and fixed red-first |
| `rung-09-gates.md` | the fuzz census rail + nightly campaign, the literal rail, the skip ledger + `-Drelease-gate`, the microbench baseline, the release act (dry run), five flaky-test hazards made deterministic |
| `rung-09-brackets.md` | the A-B-B-A bracket harness with device ÷ user ledgers; 17 findings on its first day; the ReleaseSafe scoping rows |
| `rung-09-memory-footprint.md` | the daemon's resident footprint: the Debug-binary finding, the idle-trim law, five pools gauged, LTP `syscalls` surviving |
| `rung-09-read-amplification.md` | the device-read ledger, per-shape bounds (0003 §K), the framed transform image |
| `rung-09-sqzhash.md` | sqzhash128 as the format's hash (format v5), pinned vectors, path equality, the bench |

## 1. The owner's rulings this rung

- **Performance first — amplification is a defect** (2026-10-11): a measured write or read
  amplification is fixed red-first with a derived bound and a `.stats` closure identity, never
  recorded as "known, not fixed". Decision 0003 §J (write), §K (read), §L (the journal's pad economy).
- **sqzhash128 replaces xxh3-64** everywhere (0003 §M): measured faster on our shapes (100 B record
  8.55 → 3.11 ns; 4 KiB page 3.67 µs → 29 ns on the venue), SMHasher3-clean including the low 64
  bits alone. Format v4 → v5.
- **Names by mechanism**: `patch_w1.zig` → `sole_owner_patch.zig` (`afbbb22`).

## 2. The external suites (hard gates)

| suite | result | ledger rows |
|---|---|---|
| pjdfstest (8,798 subtests) | 3 consecutive clean runs + the confirming run on the integrated binary: 0 failures | 0 |
| LTP `fs` | 67 pass / 0 fail, 3 consecutive runs | 4 (LTP total) |
| LTP `syscalls` | 1,236 pass; 93 failures triaged: 87 venue (ext4 control), 6 ours adjudicated; the daemon survives the run since the footprint work | — |
| xfstests `-g auto` | 213 distinct passes, 0 fsck findings on every scrub; every stall (074/464/551) and every tripwire fixed; the three-run discipline stays open on a quiet venue | 7 |

## 3. The defects (all red-first, all in the product)

- **Suites:** 27 (`rung-09-external-suites.md` §"Defects found by the suites"), among them a data
  corruption (two write-side authorities on one block, defect 17), three liveness stalls, a lost
  wake on a single-awaiter group join, the fsync drain cap, truncate/utimensat/ctime laws, the
  foreign-signature refusal of a bare `format`, and the per-write journal entry on size-neutral
  writes (defect 27).
- **Brackets:** F1 the commit-conveyor wedge (one fix, two repros, a model weakening), F2 EAGAIN to
  a blocking `write(2)`, F6 the tick-rescue verdict, F17 the grant ledger that never retired its own
  consumed grants (ENOSPC at 25 % fill), the staging ring's `StorageFull` answered to the
  application, the shim's libaio face, the IL arena's population floor.
- **Amplification:** the W2 fold 130× → 7×; the staged append (whole-image re-stage → in place);
  the journal's pad (0.71× → 0.48× meta ÷ user on `rand-4k-write`, then → 0.0008 with the parked
  stamps); random 1 MiB reads 4× → 1×; 512 B reads 21× → 8.2× (the LBA law); transformed volumes
  627× → 15.5× (the framed image) and their decode 782 → 19,599 IOPS (per-lane codecs).
- **Memory:** the gate binary had been a Debug build (`SafeAllocator` retention); the product is
  ReleaseSafe by default; VmRSS peak 5.80 → 1.74 GB under the suites; the staged cell's hasher
  state 4,736 → 1,056 B and gauged.
- **Test hazards:** five timing lotteries made deterministic (the dead-victim rendezvous, the
  supervise smoke, the journal-lane kick pin, the kill row's write-grain oracle, the hash bench and
  shim e2e waits).

## 4. Measured, with their rulers

Every number below is from the venue (`7.2.6-sqz_aarch64_apple-vm_9c_virtio-blk`, ReleaseSafe,
nvmet-loop unless stated) and every row is SCOPING: the charter's bars are the box's.

| row | at rung 8's close (Debug) | at this note (ReleaseSafe) |
|---|---|---|
| `rand-4k-read` IOPS | 25.8 k | 39.6–47.1 k, device ÷ user 1.000 |
| `rand-4k-write` IOPS | 3.0 k (INVALID) | 24.4–24.5 k, meta ÷ user 0.0008 |
| `seq-write-1m-direct` | INVALID (EAGAIN, wedges) | 1.2–1.7 GB/s, 0 EAGAIN, 0 wedges, device ÷ user 1.000 |
| `frag-512b-write` device ÷ user (data / meta) | 130× / 5.9× | 7.06× / 0.019× |
| `fsync-4k` mean | 1,343 µs | 252–277 µs |
| transformed `rand-4k-read` (zstd, text) | 782 IOPS at 447× | 19.6 k IOPS at ≤ 10× |
| daemon VmRSS peak under the suites | 5.80 GB | 1.74 GB |

## 5. Owed (consolidated; each note keeps its own list)

- The box run: 32 cores, five memory-backed nvme-tcp targets, the dist profile, 16 GiB+ datasets —
  every bar in charter §12 is measured there, never here.
- xfstests `-g auto` three consecutive clean runs and LTP `syscalls` ×3 on a quiet venue with the
  final binary (every stall and tripwire they found is fixed; the runs themselves are owed).
- The literal allowlist's ~609 OWED rows; ~~the `stats` "allocation law" under fabric controllers~~
  (resolved `8b0df62`, `rung-09-gates.md` §9); the nightly 60 s campaign; a quiet-venue `bench-compare`.
- The joiner-side grant ledger (a wire writer's consumed grants until the renewal declaration); the
  slot-lease handover draining parked stamps; ~~the node snapshot `RecordIndex.entries` arrays charged
  to `kv_node_cache`; the `Segment`'s fixed per-cell tables as a budget component~~ (resolved
  `352f7f2` and `1d91625`, `rung-09-memory-footprint.md` §6).
- A 512 B DMA grain on 512 B-LBA devices (the read-side 8× and the journal's pad floor both move
  with it); the transient save's fold into a full next epoch vs the 64-wide save buffers (latent).
- The harness's third lever (a `SQUEEZEFS_*` A/B knob on one binary).

## 6. The closing gate (`cba6cb6`, 2026-10-12, quiet venue, clean caches)

| venue | steps | tests | unit | rails | smoke | bench-smoke | shim-smoke | nvmet-smoke |
|---|---|---|---|---|---|---|---|---|
| Linux 7.2.6-sqz (aarch64 VM) | 46/46 | 2,610 pass / 45 skip | 2,397 / 43 skip | 16/16 + census | 64 / 2 skip | 41 | 40 | 51 |
| macOS (portable modules) | 28/28 | 2,083 pass / 25 skip | 1,959 / 25 skip | 16/16 | — | — | — | — |

Every skip is a ledgered class (`-Drelease-gate=true` refuses an unledgered one). The venue's `/build`
volume had taken EXT4 metadata damage in the host crash of 2026-10-11 (`fstrim` refused with "Structure
needs cleaning"); `e2fsck -f` reclaimed the orphaned bitmap ranges before this gate ran.
