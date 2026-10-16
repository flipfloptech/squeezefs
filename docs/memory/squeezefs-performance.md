# SqueezeFS-Zig performance measurement and work
Benchmarking on kamrui, known baselines, measurement noise, perf plan, and open perf/correctness findings from perf mounts.

## Direction
- Owner (2026-10-16): work on read/write throughput and IOPS before running the full gates; class C suites wait until perf work is done.
- Plan: `perfbase` agent builds `scripts/perf/ramnvme.sh` (brd/null_blk RAM disks via nvmet loop) and `scripts/perf/matrix.sh` (fio matrix), measures SSD and RAM baselines on kamrui (raw LV / ext4 / SqueezeFS), profiles, writes `docs/evidence/rung-10-perf-baseline.md`; then one lever per agent, each A/B'd against that table.

## Known numbers
- No raw-device fio baseline on kamrui yet. SqueezeFS 4k randwrite QD16×4 ≈ 57k IOPS (SN550 spec ~405k); QD1 ≈ 8k; 4 MiB writes 620–820 MiB/s (possibly SLC-cache bound); one drain worker per kernel queue ~95% busy.
- VM container vs ext4 (`rung-10-dev-perf.md`): randread 52%, randwrite 42%, creates 21%, stat 4%, open/close ~1%. Causes: ~8 µs FUSE transport/request, one drain worker per kernel queue, 1 s entry TTL extra LOOKUPs, dir i_rwsem serializing creates.

## Measurement noise
- On kamrui 4 MiB random-write fio (rw-4m) is device-bound at ~200 IOPS (~800 MB/s) and scatters (B/A 0.46–3.06): noise for A/B. Use 4k and 64k randwrite for stable A/B.
- epochperf A/B of the epochcap fix (r1): 4k randwrite B/A 1.00–1.01, p99 equal or better; 64k B/A 1.02–1.07 — no regression; r2 repeat requested before light-tier landing.

## Findings from perf mounts
- Writer claim of members ≥1 was an xattr at local ino 1 → routed to slot 0 (member 0 only) → heartbeats and unmount delete failed SlotBusy. Fix (0003 §AS): writer claim is a tree-0 control record (`control_records.writerClaimKey`), no door token, never migrated. Evidence `docs/evidence/rung-10-umountclaim.md`; 4/4 repro → 22/22 clean. Migration bulk copy also unretires extents it writes.
- 2-MDT perf mounts logged `routing_split_refused` (~67×, meta_set.zig ~487) and 'times drain cadence: drain failed (InputOutput)' (~1100×, cli/mount.zig ~1344): drainPendingTimes batched refinements across two members in one transaction; release blocker (fixed by timesdrain, see progress ledger).

## Known test issues
- pjdfstest suite can leak a mount (~1 in 230 runs, unfixed): if prove/sh hold cwd at SIGTERM, the daemon refuses unmount and never retries; a second SIGTERM after holders exit unmounts cleanly.
- exec.lane test "a dead ring keeps a READ the kernel took…" (ringdeadread c5f956a/63b8296) intermittently hangs at ~199% CPU under kamrui gate load (bb94bf1 gate hung 63 min); `deadreadhang` agent judges product vs test bug; `hangwatch` adds a per-test watchdog in `tests/test_runner.zig` printing `HUNG TEST:` lines.