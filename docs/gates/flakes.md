# Known flaky gate rows — each has ONE owner

A row listed here still runs and reports in every gate; a failure of it does not block an
unrelated landing, and nobody but its owner investigates it. A row leaves this list when its
owner lands the fix and the gate train (`scripts/gate-train`) shows it green in three
consecutive runs. Add a row the first time a failure is attributed away from the change that
met it (with the repro rate and where it was seen).

| Row | Symptom | Rate / where seen | Owner | Class |
|---|---|---|---|---|
| smoke "THE SHARED HOT DIRECTORY ACROSS DAEMONS" | `expected 0, found 11` open intents after B's kill (`xv_cross_owner_intent_stuck`) | 1 of 4 venue gates on `352a16f` (`rung-10-gate-blockers.md` §3); the signature `rung-10-symmetric-defects.md` item 2 fixed once | agent `intents2` | product |
| smoke "CUSTODY NO CLOSE COVERS IS DRAINED ONCE IDLE" | `parked_buffers` 0 read in the same stats tail as the idle drain's count | 1 venue gate on `352a16f` | agent `lighttier` | test |
| unit `ipc.host.e2e_linux` "THE INTERCEPTION HOST" | a kernel `pread` of the no-wait ticket pages read 0 of 32,768 bytes (`e2e_linux.zig:533`) | gate of 2026-10-13 (`/build/tmp/umount-gate.log`); venue gate F1 on `2b2409c` | unassigned | unknown |
| smoke "THE `-o ro` READER OF A LIVE SET" | C's daemon not exiting 32 s after its umount signal | no-open gates 3 and 6, its cost runs 2–3 | unassigned | unknown |
| unit `handover_moves` "THE MOVE MATRIX" (offered leg) | B's unlink parked 218 s behind its own `acquire_slot`, then `HolderUnresolved` | one gate run; 0 of 13 alone (`rung-10-gate-blockers-2.md` §7) | unassigned | product |
| any mount | `KmbufRingEnomem` at mount: the kmbuf bulk allocation treats a short `alloc_pages_bulk_node` as ENOMEM instead of reclaiming (kernel patch 0003) | 1/60 rejoin runs; every mount while LTO links held 13 of 16 GB | orchestrator (kernel patch 0029) | product (kernel) |
