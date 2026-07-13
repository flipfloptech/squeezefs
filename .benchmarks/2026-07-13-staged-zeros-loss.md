# Staged zeros-LOSS family — four legs root-caused and fixed; residual escalated

Date: 2026-07-13 · Branch `fix/staged-zeros-loss` off dev @ `2027f75` · Rails: 16-core cap
(`taskset -c 0-15`, `CARGO_BUILD_JOBS=12`), CPU capped 3.5 GHz, 8G cages, quiet-gate timed
runs. Sandbox `~/tmp/szl` + `/tmp/szl_mnt`; all failure artifacts preserved under
`~/tmp/szl/fail{1..7}` (fsx recops/model/live images, lifecycle tapes, allocator tapes).

## Protocol

Aged repro (the C-era protocol): fsstress aging (`-n 30000 -p 8`, rename/creat/unlink/
setxattr/mkdir/rmdir) then fsx storms (`-S 0 -U -N 10M -o 128000 -l 600000`, 120 s
rounds, `--record-ops`) against a caged daemon. Base e1601bc and dev 2027f75 both fail
3/3 rounds with `READ BAD DATA … GOOD→0x0000`.

Forensics per failure: full-op `--record-ops` + model diff → **last-writer bracketing**
(attribute every lost zero-run to the op that owned those bytes, isolating the loss
event's op window); an env-gated **staged-lifecycle tape** (RMW seed state / stage-commit /
promote-commit / promote-remove / truncate / spill, since stripped) and an **allocator
lifecycle tape** (alloc/publish/begin_free/finish_free) with an offline invariant checker.

## Cleave verdict (the transient-vs-durable split)

The family is BOTH, by leg:
- **Transient serve**: fail1 — fsx aborts on zeros, yet warm re-read, `drop_caches`
  re-read, and cold-remount re-read are all byte-equal to the model (a stale-size clamp
  served zeros; the next write self-healed the entry).
- **Durable codification**: fail2+ — 43–257 KB of scattered all-zero runs survive
  `drop_caches` + cold remount (zeros entered the staged image itself, or a live device
  block was punched under its owner).

## The four legs — root cause, evidence, fix (each red-first)

| # | Root cause (file:line at fix time) | Evidence | Fix / commit |
|---|---|---|---|
| 1 | `fetch_metadata`'s 1 s TTL refill replaced a `layout_dirty` hot entry with the stale backend snapshot — or the default inline `size=0` when the backend never saw the deferred staged layout (aged merge queue saturated). Reads clamped to the stale size (zeros / 0-byte reads); the clobber dropped `layout_dirty`, so fsync persisted nothing (EAGAIN hard-failures ≈160/run). `src/routing.rs:2797` | Deterministic red: 600 KB staged file + >1 s idle → read returns **0 of 614400 bytes**; fsync EAGAIN. | Dirty entry = local authority; never re-validated from the backend; `metadata_cache` residency flipped to `time_to_idle`. `c322fce` (tests `f69fcf1`) |
| 2 | **Stage-generation ABA**: `remove_staged_if_generation` compared per-ledger-entry counters that RESTART when removal deletes the entry and a re-stage re-creates it. A queued/slow promotion that read the old incarnation passed its commit check against the new incarnation's recycled gen, published the stale image, and removed the ring's only copy of newer acked bytes. `src/cache/nvme.rs:753,881` | **On tape**: two promote/remove cycles for one file_id both at `gen=1`, both `removed=Ok(true)`; 29,539 zero bytes last-writer-bracketed to that window. Red unit test: recycled gen observed (`1 == 1`). | Process-global monotonic generation source (`next_stage_generation`); gen 0 reserved for mount-recovered entries. `48d4245` (tests `df2092a`) |
| 3 | `extend_file_size`'s inline/staged arm saved a whole-meta snapshot with **no lock**: racing the promotion commit (publish `block_map[0]` → release ring entry), the stale save (map=None) erased the mapping from cache+backend — payload's sole copy stranded. Reads wedged (`… kept moving after 65 re-resolves` EIO) or served the D0 zeros-degrade for LIVE data (`staged_payload_lost_reads=6` on tape); the next RMW codified zeros. `src/fuse_client.rs:2140` | Directed hammer (write→extend→verify under promotion churn): acked bytes read `0x00` within seconds, **3/3 red**; wedge + lost_reads on the aged tape. | `DataRouter::grow_layout_size`: freshest-entry, size-only grow under `INODE_META_LOCKS` (the promote-commit discipline). `d2d2612` (tests `cfef60f`) |
| 4 | **Stale-snapshot double-free**: `release_superseded_staged` freed the op-entry pre-commit `meta.block_map` alongside the fresh under-lock map. A mapping displaced in between (promotion re-publish / spill / truncate clip) was already freed by the displacing commit — the second `free_block` **punches the device extent**, zeroing a recycled offset under its NEW owner (scattered durable zero runs on unrelated files; `lost_reads=0`). | **On tape**: one device offset cycling as `block_map[0]` across consecutive promotion commits and into the truncate clip's own allocation. Post-fix allocator tape: **0 lifecycle violations** in a full failing round (pre-fix pattern gone). | `release_superseded_staged` takes ONE map — the last published layout captured under `INODE_META_LOCKS`; all four call sites updated; doc contract pins the rule. `4724126` |

## Honest residual — escalated, not shipped-around

After all four fixes the aged fsx still reds 3/3 (shape mutated again: zeros whose
last-writers now sit ≤ 2 ops before the failing read; latest window implicates the
`copy_file_range`/`mapwrite`+`zero_range` interleave; the allocator tape is clean, the
promotion/truncate tape shows only healthy interleaves in the final window). Per the
stop-rule (3+ distinct root-cause attempts without family convergence → escalate with
evidence): the four fixed legs are each independently proven (red-first tests + tape),
and the residual is a **fifth, distinct** mechanism needing its own instrumented
iteration — likely tape coverage of `write_file_staged`/CFR-source reads next.
Artifacts: `~/tmp/szl/fail7/` (recops, model, live image, full tape). The residual
remains OUT of the QUICK tier (fresh mounts; QUICK ×3 = {003,213} only on this tip).

## Acceptance on this branch

| Gate | Result |
|---|---|
| New red suites | refill (4) + ABA (2) + pressure/hammers (5) green; 8×/6× repeat-runs green serial |
| Full serial cargo gate | **666 passed / 0 failed** (`--all-features --test-threads=1`) |
| QUICK ×3 | **{003,213} only, 3/3** — trio 616/075/091 + 074 green all rounds |
| kill9 deep churn | `SQUEEZEFS_CRASH_ROUNDS=60` green |
| Unmount-kill soak | 30 cycles: 0 coredumps / SIGABRT / panics |
| LTP | **174 PASS / 0 FAIL / 0 BROKEN / 9 SKIPPED** |
| Perf rows 1–3 | order-controlled paired A/B, same sandbox, quiet: tip **1086 W / 4746 R MiB/s / 153.5k rand-4k IOPS** vs base **1111 / 5174 / 146.2k** — flat within run noise |
| clippy `-D warnings` / fmt / doc | clean |

Aged-protocol delta on this tip: rounds now run 700–1800+ ops before the residual fires
(pre-branch: op 64–235), `staged_payload_lost_reads` 6→0, EIO wedge gone, allocator
invariants clean — four fewer ways to lose acked bytes.
