# The tarx row's wall bimodality: the host's clock, not the daemon (2026-10-22)

**The finding.** On `dev` `4a7401e` (binary `a08fce3`, gate profile), the release row
`scripts/suites/tarx.sh` phase 1 read, per extraction of the 2,452-entry corpus (mean of 3):
run 1 `a1 1.94 b1 1.89 b2 1.80 a2 1.78`, run 2 `2.22 1.82 1.93 2.23`, run 3 `1.66 2.06 2.00
0.97` (MISS 1.54×), then under the `performance` governor `1.43 0.79 1.27 0.87`. One
single-threaded `tar -x` on the same daemon swung 0.8 → 2.2 s between arms minutes apart, on
BOTH mounts.

**The cause: the laptop was on battery** (upower's charge history: discharging 12:29–13:05,
99 → 75 %; `pending-charge` 13:05:25). Every slow/fast pair is a uniform shift of every
latency the daemon measures — no tail, no stall, no shed:

| run 3 arm (wall/extraction) | write op mean | setattr | create | exec poll | io_uring `wake_hop` | device write | daemon CPU/arm |
|---|---|---|---|---|---|---|---|
| a1 1.66 s | 59.1 µs | 29.4 µs | 43.3 µs | 9.0 µs | 12.8 µs | 68.6 µs | 5.86 s |
| b1 2.06 s | 70.8 | 38.3 | 51.9 | 11.4 | 12.7 | 52.9 | 6.74 s |
| b2 2.00 s | 67.2 | 39.7 | 49.8 | 11.2 | 12.2 | 65.2 | 7.32 s |
| a2 0.97 s | 39.5 | 20.2 | 28.4 | 6.3 | 4.4 | 43.1 | 3.84 s |

The write op's mode moved from `≤32 µs` (19,012 of 31,998 in a2) to `≤64 µs` (17,948 in a1);
setattr's from `≤16 µs` (11,098) to `≤32 µs` (16,914). The pure-CPU poll loop, the kernel's
io_uring completion hop and the daemon's CPU per op all moved by the same ×1.5–2.5: the cores
ran slower. What did NOT move, arm to arm: the memory budget (level Green throughout,
`mem_budget_red/yellow_events` 0, no component shed; the write-cache bound 9.8 GB on A /
7.4 GB on B, never near its 64 MiB floor; `write_cache.current` 0 → 190 MB inside every arm and
back to 0 after the verify reads — the 6,189 evictions per arm are the other member's read-back
promotions, identical in every arm), `journal_full_stalls` (0–3 per arm), the checkpoint count
(33–48), the ring growths (b1 only, 3–5, in fast and slow runs alike). The 4.7 GB
"unreclaimable" IS the desktop's: the daemon runs in the owner's terminal's app scope
(`app-Hyprland-gtk-launch-….scope`, anon 4.9 GB) and the budget's cgroup arm counts it — 12 %
of the 41 GB budget, Green; harmless today, named below.

**On mains, measured with the probe** (`~/tmp/tarx-probe.sh`: per extraction the wall, `perf
stat` of tar, the daemon's CPU and hot threads, the idle-state census; the LTP fs run beside):
0.65–0.9 s per extraction; tar at 3.2–4.0 GHz; `/dev/cpu_dma_latency`=10 µs (C2/C3 off) moved
`wake_hop` 4 → 2 µs and the wall not at all; the governor `powersave`+`balance_power` cost the
FIRST extraction after the switch 1.10–1.14 s (the ramp) and nothing after (0.82–0.94 vs
0.79–0.97 under `performance`). What DID move the mains wall: this box's `sqz-thermal-governor`
(`scaling_max_freq` 5.19 → 4.0 GHz at Tctl 84 °C, back ~30 s later) flipped inside nearly every
40 s phase (a1 0.684 uncapped vs a2 0.903 capped: 1.32×), and one b2 read 1.35 s against b1
0.84 with the cap constant (a co-tenant's burst sharing the SoC's power budget: nothing in
sysfs).

**The fix is to the ROW's measurement (`scripts/suites/tarx.sh`, `scripts/suites/common.sh`);
the law is unchanged (B's mean ≤ 1.10 × S0):**
- the row REFUSES to run on battery (`suite_host_posture`: `mains=off` → exit 2 before a set
  is formatted);
- the harness PINS the clock for the run: `cpufreq/boost` 0 (ceiling = nominal 3.0 GHz; the
  guard's cap writes above it clamp to it; the SoC stays in its sustained budget), restored
  after — boost back, `scaling_max_freq` to the guard's current cap (`/run/sqz-thermal-governor/
  cap_khz`) when there is one, else to the value found at pin time (`TARX_PIN_CLOCK=0` leaves
  the clock; a host without `cpufreq/boost` is said and left alone);
- the host posture (`mains gov epp boost max_khz`) is recorded at every arm boundary
  (rows.tsv's `host_posture`) and must not change across the phase, and each mount's two arms
  must agree within 1.20× (A's drift over a phase on mains measured 1.11–1.18×, B's
  1.02–1.13×); a phase that breaks either is **INVALID** — logged with the arms and postures
  (`attempts-run<n>.txt`), never a pass — and re-run on a fresh set up to `TARX_ABBA_ATTEMPTS`=3
  times; every attempt invalid fails the run (`abba/invalid`). Exercised with the bound forced
  to 1.00: INVALID → re-run → `abba/invalid`, exit 1.

**The verdict** (`SUITE_RUNS=3`, mains, boost pinned, LTP's fs run beside; results
`/build/tmp/suites-tarx-20261022-132136-3321202`): run 1 `a1 1.116 b1 1.022 b2 1.088 a2 1.320`
→ 0.866× MET; run 2 `1.043 0.944 0.918 1.180` → 0.838× MET; run 3 `1.043 0.955 0.975 1.092` →
0.904× MET; every attempt valid at the first try, phase 2 8 × 2,452 entries in 2.22–2.30 s,
every tree byte-exact, fsck clean, tripwires 0: **3 consecutive clean runs**.

**Seen, not fixed.** (1) a2 is always slower than a1 (1.05–1.18× at 3 GHz, 1.11–1.15× at 5):
the set's kv root grows 3 levels during b2/a2 (`meta_kv_root_grows`), `meta_kv_commit_smo_retries`
0–3 → 18–25, the journal thread's CPU +30–70 %, lookups 10 → 20–27 µs — the metadata path's
cost per op grows with the set (12 trees, 29 k entries); the A-spread bound is 1.20 against a
systematic 1.18. (2) The local venue's daemons inherit the owner's desktop scope: the budget's
unreclaimable arm would read Red at 33 GB of desktop memory. (3) The row's `tar` chain burns
~1.3 cores in the daemon per single-threaded extractor (530 µs CPU per entry) — the clock
sensitivity is 1:1. (4) The daemon log's one alert per manager (`job fabric: no data namespaces
armed for WERO`, file-backed sets) predates this change.
