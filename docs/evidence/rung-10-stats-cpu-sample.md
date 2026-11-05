# CPU census: absence, incarnation, and source baselines

2026-11-05. Independent `fix/stats-cpu-sample`; omission red/fix
`dbbd153`/`2e48c2b`; B1 red/fix `e7bc686`/`0901406`. No integration.

## Causes and reds

The frozen PR-2 count 16 proves at least eight CPU-sum violations: eight render
threads can contribute at most eight terminal render/JSON errors. Its exact
tids/interleaving are unavailable; ESRCH alone is not causal evidence.

A proc walk is not a census snapshot: an exiting continuation can skip live
tails (`fs/proc/base.c:3834–3925`, stock 7.3.1). Retiring omitted live rows billed
their full runtime again on reappearance. Original reds: **40/43 passed, three
failures**, including real live-tail retirement (35,135 ns), synthetic
100 → 200 ns beside total 100, and a missing live comm incorrectly succeeding
(`/tmp/sqz-stats-cpu-red-commit.log`). The cursor fixture now waits for proc
absence, not merely pthread join (`d925b2c`); its earlier loaded failure remains.

Adjacent B1: rename/runtime-source decrease is not reuse. Committed reds:
**40/44 passed, four failures**
(`/tmp/sqz-stats-cpu-b1-red.log`): journal charged 1,500 instead of delta 500;
12,345,678 → stat 10,000,000 billed 22,345,678; higher-runtime reuse lost the old
incarnation; replacement between runtime/comm reads blended tasks. Totals
match each synthetic lifetime; start-time ticks deliberately coincide.

## Fix and proof

`0901406` holds one task-directory FD per known live PID object. Relative
runtime/comm reads cannot follow a reused numeric tid. An old anchor losing
its fixed stat entry while the current candidate has it proves replacement;
class, runtime, inode numbers and tick timestamps never identify reuse.
Kernel proof: `proc_pid_make_inode` holds a PID reference and
`proc_pident_lookup` resolves that object's task; `free_pid` removes numeric
IDs independently (`/tmp/sqz-stats-cpu-b1-pid-anchor-proof.log`). Anchors do not
hold tasks alive: the cursor test exits threads while they remain.

Each incarnation keeps a known CPU baseline. Renames preserve the old class's
prefix and charge only new deltas. Changed/coarse sources retain the high-water
baseline; repeated precise-source regression refuses the sample. Valid stat
fallback is permitted when schedstat is unavailable/malformed; otherwise live
read/proof errors and root failure preserve the prior publication and its age.
Omitted live baselines remain; proven absent tasks retire once. No class/total
clamp, clock replacement, render walk, or publication synchronization change.

## Verification and limits

All Linux checks: `scripts/venue`, local x86_64 Linux 7.3.1-1-omarchy-mainline,
Zig 0.17.0. Final selection **72/72**; fresh **20 × 72/72** without synthetic
load and **20 × 72/72** with 32 owned CPU workers, exit 0
(`/tmp/sqz-stats-cpu-b1-{idle,loaded}.log`). The runs strictly parsed 12,800
stress documents, checked coherent sums/monotone classes, and retained the
allocation law. Descriptor tests prove closure on OOM, read refusal, retirement
and teardown. Non-test sampler/deinit instantiation compiles; static rails
**31/31**, fmt clean (`/tmp/sqz-stats-cpu-b1-static.log`). The unchanged exhaustive
publication model holds; its `no_recheck` weakening fails.

Correctness/simplicity re-review, integration, other architecture, and the
orchestrator gate remain owed. No full gate, benchmark, system change or target
rig. Load workers were trap-cleaned; anchors/threads are owned and closed/joined.
FD exhaustion refuses sampling rather than inventing death. Policy: 0003
§CC.7–8 and decision 0002 row 570. Interim `/tmp` transcript names above identify
verification artifacts; the committed tests, source reasoning and counts retain the evidence.
