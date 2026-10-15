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
| unit `meta.conveyor` "§L: the window-fill hold" | `windows ≤ 5·per_committer/2 + 8` exceeded: 274 windows > 248 (pad 68.2 %, inside its bound) | 2 of 3 kamrui gates (x86_64, 12 threads, 6 unit + 6 smoke shards); alone on kamrui 199 windows, green | unassigned | unknown (schedule under load) |
| unit `handover_moves` "THE MOVE MATRIX" (offered leg) | B's unlink parked 218 s behind its own `acquire_slot`, then `HolderUnresolved` | one gate run; 0 of 13 alone (`rung-10-gate-blockers-2.md` §7) | unassigned | product |
| any mount | `KmbufRingEnomem` at mount: the kmbuf bulk allocation treats a short `alloc_pages_bulk_node` as ENOMEM instead of reclaiming (kernel patch 0003) | 1/60 rejoin runs; every mount while LTO links held 13 of 16 GB | orchestrator (kernel patch 0029) | product (kernel) |
