# Saturation-suite cage OOM (bench finding #2) — verification: NOT CURED

Date: 2026-07-12 · Binary: `fix/real-statfs` tip (dev `2027f75` + honest
statfs, includes follow-up C pooled seeds `e4a8434` and the KV-cache
authority registration `c281089`) · Box: AMD RYZEN AI MAX+ PRO 395
(32 hw threads, 94 GiB RAM, CPU capped 3.5 GHz)

## Protocol

Same shape as the original 2/4-OOM session (2026-07-12, bench
auto-saturation acceptance): fresh file-backed sandbox (1 G meta,
144 G data, staging declared), user mount under
`systemd-run --user --scope -p MemoryMax=8G -p MemorySwapMax=0` with
`--mem-budget 5G`, then 4 consecutive **bare** `squeezefs bench <mnt>`
saturation suites against the one daemon (auto shape: threads=16,
1 file/thread, 2 g/file ⇒ 32 g total; run 2 re-derived 1792 m/file ⇒
28 g under the now-honest statfs cap while run 1's reclaim was still in
flight). Daemon RSS sampled from `memory.current` (0.2 s / 0.5 s),
`.stats` `mem_budget_*` correlated, kernel journal for OOM events.

## Verdict: **finding #2 still reproduces — 1 OOM kill in 3 completed-run
attempts (run 2 of 4; runs 3–4 unreachable, mount dead)** + 1 clean
enrichment run. Follow-up C (pooled staged-RMW seeds) did NOT close this
class; do not re-run this protocol expecting green until the PR 7
authority chain lands a fix.

| Run | Result | Elapsed | Peak RSS (cage 8192 MiB) | Notes |
|-----|--------|---------|--------------------------|-------|
| 1 | clean, exit 0 | 185 s | **8192 MiB (at ceiling)** | full sane table (write 1118 MiB/s, read 355 MiB/s, rand-4k 39.5 k IOPS @ 14 % cov) |
| 2 | **OOM-killed** | 144 s (died in/after seq-read, before the table) | 8192 MiB | kernel: `Memory cgroup out of memory: Killed process (squeezefs.bin) total-vm:37.7 GB, anon-rss:8323796kB` — **7.94 GiB ANONYMOUS heap**, not reclaimable page cache (file-rss 13 MB) |
| 3 | not reachable | — | — | mount dead (transport gone after run 2's kill) |
| 4 | not reachable | — | — | — |
| B (enrichment, fresh mount) | clean, exit 0 | 151 s | 8192 MiB (at ceiling ≥ 100 s) | correlated `.stats` timeline below |

## Fingerprint (run B, 0.5 s sampling, 10 s bucket maxima)

```
t+000s rss=6280MiB lvl=0 red=0 gauges= 523MiB   (write seq 1m)
t+020s rss=7526MiB lvl=2 red=1 gauges=3883MiB   (write→read transition)
t+030s rss=8191MiB lvl=2 red=1 gauges=5104MiB   (read seq 1m, 16 direct streams)
t+090s rss=8192MiB lvl=2 red=1 gauges=5424MiB
t+110s rss=8191MiB lvl=2 red=1 gauges=6832MiB   (rand 4k passes)
t+120s rss=8191MiB lvl=2 red=1 gauges=7144MiB
t+150s rss=5037MiB lvl=2 red=1 gauges=4006MiB   (stat/del, drain)
```

- The authority **does** see the pressure: `mem_budget_level=2` (Red),
  `mem_budget_red_events=1`, but `mem_budget_gauge_sum_bytes` climbs to
  **7.1 GiB against the 5 GiB pinned budget** while RSS sits pinned at
  the 8 GiB cage for 100+ s. Shedding fires yet does not converge below
  budget under 16 concurrent O_DIRECT 1 m/4 k streams + durable writes.
- Whether the kernel OOM-killer fires inside that ceiling-riding window
  is a race — hence 2/4 in the original session, 1-in-3 here, and clean
  runs either side. The kill is **anon heap** (7.94 GiB anon-rss), so
  this is registered-consumer/allocator memory, not cgroup-counted page
  cache of the file-backed volume.
- Died-at pass: run 2's kill landed ≈ 144 s in, i.e. late seq-read /
  entering the rand passes — the same region where run B's gauge sum
  peaks (7.1 GiB).

## Disposition

Per the task rails this note records the fingerprint and STOPS: the
convergence gap (gauge sum 2.1 GiB over budget at Red, anon heap at the
cage) belongs to the joint-memory-authority chain (R5/PR 7 owners), not
to the bench or the statfs fix. Raw samples preserved at
`~/tmp/sqfs_f2_rss_runs12.txt` / `~/tmp/sqfs_f2_rss_runB.txt` on the
capture box.
