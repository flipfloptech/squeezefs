# The edge-aware leaf split: measured, not landed (2026-10-27)

**What was tried.** Lever 3 of the post-v0.1.0 metadata work: a leaf whose overflowing fold is an insertion
run (every new key above — or below — every logged key) splits at its edge past ¾C instead of at the byte
midpoint, so append-ordered trees (ascending inos per stream, ascending dentry runs) keep full leaves; a random
batch keeps the K2 midpoint rule. Built red-first on the local branch `perf/edge-split` (`266f016` red,
`37bd762` the rule, off `dev` `672b84a`; not pushed) with a property test (two streams + a random band +
deletes, journaled, SMOs interleaved, crash replay), `insertEdge`/`planCut` ties, and the counters
`meta_kv_node_edge_splits` / `MaintenanceOutcome.edge_splits`. Unit, rails, fmt, 14 smoke rows and
xfstests generic/001 013 426 604 were green.

**The counters said near neutral.** mdtest -u on the 2 MDT + 2 OST rig: creates −13…−23 % compactions and
−4…−11 % rewrite bytes per op, standing leaves −2…−7 %; removes +11…+13 % compactions and +5…+16 % rewrite
bytes (a ¾C leaf has ¼C of log room for tombstone frames). The premise was weaker than assumed: `dev`'s
leaves were already ≈ 82 % full (238 MB of records in 1,125 leaves) — the maintenance queue runs full under
mdtest (38–52 k refusals per phase), so freezes come late with large deltas and C + δ cut at the midpoint
lands at 62–75 %, not 50 %. The half-full shape exists only for small deltas.

**The A-B-B-A (quiet host on AC, RAM-NVMe perf mount, dist builds, `mpirun mdtest -n 10000 -u -i 1`, two
rounds = four legs per arm, fsck clean in all 16 runs; k ops/s, mean of the legs):**

| | -np 32 dev | -np 32 split | Δ | -np 64 dev | -np 64 split | Δ |
|---|---|---|---|---|---|---|
| file creation | 182 | 178 | −2.7 % (one 155 k outlier leg; ≈ 0 without it) | 186 | 186 | 0.0 % |
| file removal | 231 | 228 | −1.4 % | 234 | 232 | −0.9 % |
| directory creation | 199 | 189 | −4.8 % | 185 | 196 | +6.1 % |
| directory removal | 209 | 194 | −7.2 % (every split leg ≤ 204 k, dev 199–220 k) | 209 | 202 | −3.4 % |

**Verdict.** Creates flat, removes pay for the fuller leaves; one row up, three down. Not landed (the rule:
land only what helps without regressing another row). Sessions: `/build/perf/es-abba/`, `/build/perf/es2-abba/`.

**What it pointed at instead.** The maintenance queue running full is the shape behind both the late freezes
and the removes' compaction cost, and it is pre-existing on `dev`: it is the next thing to look at for
sustained create/remove rates. Also seen: mdtest's whole subtree lands on volume 0 of a 2-MDT set (§AA.1
subtree affinity), so the rig is effectively one MDT.
