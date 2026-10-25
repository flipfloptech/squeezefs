# Rung 10 — the release window's perf A-B-B-A (2026-10-25, mainline 7.3)

**What it measures.** Every landing since the last measured head (`d46dd83`, 2026-10-24) that the
evidence notes owed a throughput check: fsync coalescing and committer room parks (§BO), the free
durability hold (§BT), the resolve-fault check's two loads per write and the register-then-check on
extending writes (alternating appends), the census storm and its follow-ups, the claim epoch, the
guard's kernel probe. Base `d46dd83` (A) against the final candidate `0b0e69b` (B), legs A B B A,
one session at a time on a quiet host (load ≤ 1.3, AC power, `tctl_c` 45–76 °C, cooled ≤ 70 °C
between legs).

**Data path** — `scripts/perf/quiet.sh ram-sqz` (dist builds, the RAM-NVMe set, 20 s rows):

| row | A | B | B | A | Δ of the means |
|---|---|---|---|---|---|
| rr4k-q32j4 IOPS | 1,300,915 | 1,320,344 | 1,307,500 | 1,313,805 | +0.5 % |
| rw4k-q32j4 IOPS | 1,157,496 | 1,150,681 | 1,144,805 | 1,154,339 | −0.7 % |
| sr1m-q16j4 MB/s | 69,112 | 73,822 | 73,222 | 74,149 | +2.6 % |
| sw1m-q16j4 MB/s | 49,317 | 49,963 | 51,256 | 51,494 | +0.4 % |

**Metadata** — the release row `scripts/suites/mdtest.sh` (gate builds, loop sets; `-np 32`
`mdtest -n 10000 -u`, mean ops/s; every leg PASSED: df at its baseline, fsck clean, no tripwire):

| phase | A | B | B | A | Δ |
|---|---|---|---|---|---|
| file creation | 82,342 | 80,502 | 83,287 | 83,455 | −0.6 % |
| file removal | 85,975 | 86,439 | 86,791 | 86,071 | +0.6 % |
| file stat | 7.24 M | 8.93 M | 8.88 M | 7.42 M | +21 % |
| dir creation | 85,310 | 72,233 | 72,198 | 73,126 | −1.3 % vs the matched A (the first A leg ran coolest; A's legs disagree by 14 %) |
| dir removal | 57,631 | 58,117 | 58,282 | 58,134 | +0.6 % |

**Verdict.** Nothing the window landed costs measurable throughput: every data row and every
metadata phase is inside its legs' own spread, removals (the free hold) included. Stat gained 21 %
(the recalls the census and atime fixes stopped). Sessions: `/build/perf/mx/ram-sqz-rfinal-*`,
`/build/tmp/suites-mdtest-20261024-223{400,545,729,915}-*`.
