# CPU census: an omitted tid is not a dead thread

2026-11-05. Independent `fix/stats-cpu-sample` investigation; product fix
`2e48c2b`, regression `d925b2c`, literal cleanup `54e3124`, red `dbbd153`. No integration.

## Cause and red

The frozen PR-2 run failed at `stats.zig:1500`, count 16. Eight render threads
can contribute at most eight render/JSON errors because those branches return;
therefore some CPU sums exceeded the published total. The log has no sample
values or tids: its exact historical interleaving cannot be recovered. The
isolated ESRCH diagnostic is not evidence that ESRCH caused the inequality.

The producer assumed a `/proc/self/task` walk was a complete census. Stock
7.3.1 `fs/proc/base.c:3834–3925` disproves that: `next_tid` ends at an exiting
task; if a buffered continuation tid disappears, `first_tid` resumes by list
position, which can skip still-live successors. `Ledger.finishPass` retired
unseen live rows; their next observation added their full runtime again.
`Renderer.sample` then published that invalid sample through the unchanged
seqlock. JSON ownership is not needed to produce the failure.

Three committed runtime reds passed 40/43 common/selected items, exit 1
(`/tmp/sqz-stats-cpu-red-commit.log`): real getdents continuation across 93 exits
retired live tail threads (35,135 ns); a synthetic omitted/resumed tid doubled
100 ns to 200 ns beside total 100; a live missing `comm` incorrectly succeeded.
The sampler file is byte-identical at `bb75ca8`, `91cd3d4`, and branch base
`2bbd842` (`/tmp/sqz-stats-cpu-base-equivalence.log`); no branch was rewritten.

## Fix and verification

Before retirement, stat only unseen task directories. Present tids keep their
last baseline; only ENOENT proves absence. Live read/proof errors and an
unavailable task root refuse publication, retaining the previous sample and
its age. No clock replacement, max/clamp, seqlock change, render syscall, or
product thread-lifetime change.

Loaded repetition 13 exposed the regression's join-as-disappearance assumption.
`clear_child_tid` wakes join
before `exit_notify`; `d925b2c` waits for actual task-directory absence at the
cursor seam, primes owned tids, and asserts owned tail membership. No sleep
or widened bound. The failed set remains `/tmp/sqz-stats-cpu-loaded.log`.

All checks used `scripts/venue`, local x86_64 Linux 7.3.1-1-omarchy-mainline,
Zig 0.17.0. Final stats/cpu/model selection: **67/67**, including the unchanged
exhaustive sampled-census model and its failing `no_recheck` weakening. Fresh
uncached repetitions after the regression correction: **20 × 67/67** without
synthetic load and **20 × 67/67** with 32 owned CPU load workers, exit 0
(`/tmp/sqz-stats-cpu-{idle,loaded}-final.log`, source `54e3124`). Those runs strictly parsed
12,800 stress documents and checked document/direct samples for closure and
monotone classes; allocation-law and sampler-only census checks passed too.
The non-test build passed 11/11; rails' later product compile passed too.
Static rails caught one stale literal row: `54e3124` documents Linux USER_HZ
and deletes it, adding no exemption. Retry: 31/31 static rails; shim census
3/3; fmt clean (`/tmp/sqz-stats-cpu-static-rails-final.log`).

Review/integration, other-architecture checks, and the orchestrator's gate are
owed. No exclusions, target rigs, kernel/system changes, benchmark, or full
gate. Owned load workers were trap-cleaned; regression threads joined. Policy:
0003 §CC.7; the named 0002 CPU-sample row needs numeric mapping at integration.
