# Known flaky gate rows — each has ONE owner

A row listed here still runs and reports in every gate; a failure of it does not block an
unrelated landing, and nobody but its owner investigates it. A row leaves this list when its
owner lands the fix and the gate train (`scripts/gate-train`) shows it green in three
consecutive runs. Add a row the first time a failure is attributed away from the change that
met it (with the repro rate and where it was seen).

| Row | Symptom | Rate / where seen | Owner | Class |
|---|---|---|---|---|
| smoke "THE SHARED HOT DIRECTORY ACROSS DAEMONS" | `expected 0, found 11` open intents after B's kill (`xv_cross_owner_intent_stuck`) | 1 of 4 venue gates on `352a16f` (`rung-10-gate-blockers.md` §3); the signature `rung-10-symmetric-defects.md` item 2 fixed once | agent `intents2` | product |
| unit `ipc.host.e2e_linux` "THE INTERCEPTION HOST" | a kernel `pread` of the no-wait ticket pages read 0 of 32,768 bytes (`e2e_linux.zig:533`) | gate of 2026-10-13 (`/build/tmp/umount-gate.log`); venue gate F1 on `2b2409c` | unassigned | unknown |
| smoke "THE `-o ro` READER OF A LIVE SET" | C's daemon not exiting 32 s after its umount signal; once C fail-stopped (ESTALE ×2) without the `ledger_advance` line in its log (`smoke.zig:4836`) | no-open gates 3 and 6, its cost runs 2–3; the log-line variant 1 of 3 kamrui gates (2026-10-14) | unassigned | unknown |
| unit `handover_moves` "THE MOVE MATRIX" (offered leg) | B's unlink parked 218 s behind its own `acquire_slot`, then `HolderUnresolved` | one gate run; 0 of 13 alone (`rung-10-gate-blockers-2.md` §7) | unassigned | product |
| any mount | `KmbufRingEnomem` at mount: the kmbuf bulk allocation treats a short `alloc_pages_bulk_node` as ENOMEM instead of reclaiming (kernel patch 0003) | 1/60 rejoin runs; every mount while LTO links held 13 of 16 GB | orchestrator (kernel patch 0029) | product (kernel) |
| `fsck --offline` after a kill -9 (smoke "UNDER THE WRITE-BACK TIMER fsync, fdatasync AND syncfs …") | the metadata open refuses `Corrupt`: "tree 0 … traversal retry budget exhausted descending to level 0 (routing loop — SMO protocol bug) restarts [root-seq] 256" | 1 of 11 runs of the row on `.wt-wbtimer`, beside two storm rows in sibling shards; 0 of 8 alone (`rung-10-writeback-timer.md`); 0 of 300 in a 3-wide loop. Root cause (a kill between a guest root's naming and its claim's bitmap page: the root recovers unallocated, the open's re-mint overwrites it) reproduced by the seam cut "kill matrix: mid-C4b" every run; fixed by the mount's claim adoption (`rung-10-routing-loop.md`) — leaves this list after three train greens | agent `routingloop` | product |
| any write storm at a small parked cap | `active_block_table_full` (must-stay-0) fires: W2 page-overlay entries hold table slots without pool cells, so a full checkout finds no entry below the pool cap | `SQUEEZEFS_PARKED_BUFFERS=8`, 16 `head -c` writers, under both write-back postures (`rung-10-writeback-timer.md`) | unassigned | product |
