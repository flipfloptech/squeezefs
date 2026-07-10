# fsx copy_file_range short-copy crawl — format-agnostic (v3 generic/616, v2 generic/112)

Captured live during the K7 fstests `-g auto` run on a v3 mount
(dev branch `perf/kv-v3-gates`, 2026-07-09 ~19:00, fsx PID 1071638).
**Update (same day, v2 comparison leg):** the identical signature
reproduced on a REAL v2 mount — see "v2 replication" below — so this is
**not a v3 delta**; it is a shared-code (dev) follow-up.

## Symptom
- `generic/616` (`ltp/fsx -S 0 -U -q -N 100000 -p 1000 -o 128000 -l 600000`)
  ran > 2 h wall with fsx in R-state but only ~13 min CPU and **zero
  progress markers** (`-p 1000` prints every 1000 ops; 616.full stayed at
  the command-echo line the whole time ⇒ < 1000 ops in 2 h).
- FUSE connection `waiting = 1` (serial in-flight request), daemon healthy,
  temps fine — not a hang, a crawl.

## Smoking gun (strace -c, 12 s attach)
```
% time     seconds  usecs/call     calls    errors syscall
100.00    1.482540           2    528808           copy_file_range
```
- **528,808 copy_file_range calls in 12 s, ~2 µs/call, 0 errors.**
- 2 µs/call is far below a FUSE round-trip ⇒ the kernel is satisfying most
  calls without a daemon trip (or erroring/short-returning immediately),
  and fsx's CFR loop ("repeat until requested length consumed") advances
  by a pathologically small amount per call — likely 0/1-byte or
  sub-block short copies, possibly a kernel-side generic fallback loop
  after the daemon reported EXDEV/EOPNOTSUPP/short length once per byte.

## v2 replication (comparison leg, same session — the delta answer)

`generic/112` (fsx `-A` AIO variant) on a **real v2 mount** of the same
binary wedged the same way: 28 min wall / ~10 % CPU / zero op progress,
and `strace -c` for 12 s attached to fsx PID 1641513 read:

```
% time     seconds  usecs/call     calls    errors syscall
100.00    1.397891           2    483676           copy_file_range
```

**483,676 copy_file_range calls in 12 s, ~2 µs/call, 0 errors** — the
identical crawl on a volume that never touches the KV backend. The
CFR short-copy bug therefore lives in shared code (the
`copy_file_range` handler / its interaction with the kernel CFR loop),
present on both formats of the same binary, i.e. **zero v2-vs-v3
delta** for the K7 gate. (On the v3 full run, generic/112 self-aborted
with an output mismatch rather than wedging — same recorded verdict,
different luck in the AIO op sequence.)

## Where to look (fix task)
- `src/fuse_client.rs` `copy_file_range` handler: K6b routed full-file
  clone via `DataRouter::clone_file`; check the PARTIAL-range path's
  return length (short-copy contract: returning < requested is
  legal but each call must make real progress; returning 0 forces the
  caller loop; verify we never return 0 for a nonzero request).
  Format-agnostic per the v2 replication — not the kv layout path.
- fsx flags that matter: `-U` (no mmap), 128 KB max op ⇒ ranges cross our
  4 MiB block boundaries rarely; suspicion is the sub-block/partial-block
  leg of the shared CFR handler.

## Action taken during the runs
- v3 leg: fsx (616) killed to unwedge the suite; 616 recorded as FAILED;
  suite continued.
- v2 leg: fsx (112) killed after capturing the strace evidence above;
  112 recorded as FAILED (it also failed on v3 — verdict symmetric);
  suite continued.
- Fix is a dedicated follow-up (tests-first: a CFR progress regression
  test + LTP-style repro) before K8/K9 ship. Out of K7 scope: the crawl
  pre-exists this branch's metadata changes and shows no format delta.
