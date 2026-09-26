#!/usr/bin/env bash
# shellcheck disable=SC2153  # host-side vars interpolated into generated guest scripts (directives cannot reach heredoc bodies; see the vm-hostscope leg)
# tests/run_mw_matrix.sh — the fleet-row emitter (PR 6, rung 6)
# =============================================================================
#
# design-full-multi-writer §5.5: runs one named leg on the LIVE fleet built
# by tests/mw_fleet.sh and emits per-mount stats DELTAS with the mandatory
# columns — a row without its engagement (`dlm_*` / `meta_ship_*` /
# `membership_*`) deltas is INVALID, and every N > 1 row carries the
# R5-PRESSURE columns (`mem_budget_red_events` bounded, `hard_backstops ==
# 0`, `parked_gate_timeouts == 0`) so fleet oversubscription can never be a
# row's silent story. Snapshots are `cat $MNT/.stats` (never cp — the aging
# trap) and are preserved under the fleet state dir per row.
#
# Legs (this rung ships exactly these):
#   smoke               N=2 acceptance: writer (explicit identity, the
#                       proven rung-2 N=1 shape) does basic I/O; the S5
#                       read-only reader observes it coherently WITHIN THE
#                       PUBLISHED STALENESS BOUND (`reader_staleness_bound_ms`
#                       read from the reader's own stats — S5 visibility:
#                       readers do not appear in `squeezefs clients` until
#                       S6 arms, so posture/identity come from each mount's
#                       stats inode); per-mount deltas + R5 columns emitted;
#                       distinct client slots asserted.
#   multipath-negative  a THIRD explicit identity attempts to mount the
#                       shared volume set: on a stock multipath=Y kernel the
#                       only openable meta node is the (merged) subsystem
#                       head, served by the writer's identity — the rung-2
#                       rule-2 refusal MUST fire. Pinned LOOSELY (class +
#                       rule number, not byte-exact: rung 5b part 3 upgrades
#                       the message to name remedies). The leg does NOT add
#                       a second identity path to the live head (a live
#                       writer's I/O would round-robin onto an unregistered
#                       association and be PR-rejected) — the two-identity
#                       merge itself is probed once, pre-mount, by
#                       mw_fleet.sh create (the recorded host_scoped
#                       verdict, which this leg consults: on a 5b
#                       host-scoped kernel the merged-head shape does not
#                       exist and the leg SKIPs loud).
#   pv-volume-scaling [--ns=1,4,16,46] [--files=N] [--idle-secs=S]
#                       [--repeats=R] [--threads=T] [--budget=SIZE]
#                       [--node-cache-mb=MB] [--tag=NAME] [--single-writer]
#                       (design-per-volume-claim-admission PR 0 — THE
#                       VIABILITY GATE; risks R9/R15) THE VOLUME-SCALING
#                       SWEEP. UNPRIVILEGED and fleet-free: it drives its
#                       own fixture (tests/pv_volume_set.sh — ONE daemon
#                       over N metadata volumes, file-backed by default,
#                       device-backed via SQZ_PVSET_META_DEVS) and needs
#                       no root, so it dispatches ahead of the fleet
#                       preamble. Per N: (1) aggregate daemon RSS/anon at
#                       mount and after a fixed mdstorm workload, (2) the
#                       idle-window daemon CPU + meta_kv_checkpoints rate
#                       — the per-volume background-task cost, (3) R5
#                       level/yellow/red/hard_backstops + the
#                       kv_node_cache component's bytes AND shed count,
#                       (4) the meta_kv_journal_entries_per_volume
#                       balance (spec-R4's own row: max, mean, max/mean,
#                       volumes moved/carrying), (5) the R15 read-tier
#                       row — node-cache hit rate over a cold and a warm
#                       stat pass, re-runnable with --node-cache-mb set
#                       to a DIVIDED per-volume budget to price the
#                       set-aware derivation PR 9 would land. Row
#                       validity: the daemon must have opened exactly N
#                       volumes (the per-volume journal array's length),
#                       every phase engaged, dlm_rpcs == 0 (the solo
#                       re-gate), invariant_tripwires and
#                       meta_kv_revalidate_dirty_skips flat. No product
#                       change: the arithmetic columns
#                       (ncTarget/vol, aggXbudget) are
#                       arithmetic-on-measured-constants over the LIVE
#                       mem_budget_bytes. Quiet-gated (foreign cargo/
#                       rustc/fio, loadavg, the >= 80 °C thermal refusal).
#                       Evidence tier: the memory rows are
#                       substrate-independent measured-real; the CPU and
#                       journal rows are barrier-sensitive and owe a
#                       root + tests/dev_substrate.sh re-run.
#   vm-hostscope-validate  (rung 6b — needs a fleet created with --vm=V)
#                       BOOT-VALIDATES sqz kernel patch 0030 inside the
#                       qemu guest (design-mw-multipath-kernel §6):
#                       POSITIVE arm on fleet guest 0 (param=Y): the 5b
#                       probe's param face answers host-scoped=true
#                       in-guest; two identities' connects to ONE subnqn
#                       land in TWO subsystems (distinct sqz_host_scope,
#                       one openable head each, each dir's controller
#                       links carrying only its identity); a same-
#                       identity duplicate_connect still MERGES (same-
#                       identity multipath preserved); dmesg carries no
#                       "duplicate IDs" refusal. NEGATIVE arm on an
#                       ephemeral param-OFF guest (idx 90): the same two
#                       connects MERGE into one subsystem (both hostnqns
#                       under one dir, empty scope) and an explicit-
#                       identity mount over the merged head refuses with
#                       the upgraded rule-2 text naming the shape + both
#                       remedies. All against the RESERVED guest-leg
#                       namespace — no live host writer's subsystem is
#                       ever touched.
#   s6-journal [--window=S]  (rung 7 — needs a fleet created with
#                       --membership; design row S6-a) the S6 gate LIVE:
#                       a sustained quiet window over the whole fleet in
#                       which `membership_renewals` grows with the
#                       heartbeat while `meta_kv_journal_entries` does NOT
#                       grow proportionally (the pre-S6 plane paid ONE
#                       journal transaction PER BEAT — spec §6.5 item 3's
#                       455 beats/s serialization); registration commits
#                       stay flat, the census serves engage (`squeezefs
#                       clients` probes during the window), self-fences/
#                       evictions stay 0, and every row carries the §5.5
#                       R5-pressure columns. Default window 600 s
#                       (SQZ_MWMATRIX_S6_WINDOW_S / --window=S override —
#                       shorter windows are labeled in the row).
#   s6-fence [--netem=MS] [--victim=IDX]  (rung 7; design row S6-b) the
#                       self-fence clock law under duress: the victim
#                       reader is remounted into its own netns with netem
#                       delay (default 200 ms per veth end), SIGSTOPped
#                       past its T_self and past the owner's TTL; the
#                       owner must evict (+ mint the S7 dead epoch) while
#                       the victim is frozen, and the victim must
#                       SELF-FENCE + purge on resume — self_fences=1 on
#                       the victim, 0 elsewhere, grace refusals 0.
#   s6-vm-fence         (rung 7 — needs --vm=V + --membership; design row
#                       S6-b') the HUNG-KERNEL shape: guest 0 joins the
#                       host fleet as a READ-ONLY member over the fabric
#                       (in-guest operator connects reproduce the
#                       format-time instance numbering), `mw_fleet.sh
#                       pause` freezes the guest kernel past the owner's
#                       TTL (its monotonic domain cannot observe T_self —
#                       the shape kill-9 cannot produce), and on resume
#                       the guest must observe itself dead and self-fence
#                       (purge) BEFORE holding any fresh lease — never
#                       resume as a live member on its stale caches. The
#                       owner side must have evicted + minted the S7 dead
#                       epoch. Real halves: the frozen kernel and the
#                       host-vs-guest clock domains are REAL; large-skew
#                       injection stays on the membership_sim.rs seam.
#   s7-device-fence     (rung 8 — the device-enforced data plane every
#                       fleet arms since PR 14 (the join ladder's rung 4);
#                       design row S7-a, spec §6.9 S7 gate R2) the DEVICE-
#                       REJECTION row, SCOPED to the zombie-rejection +
#                       quarantine half (stated posture: in this rig the
#                       WRITER is the membership owner, the custody
#                       authority and the WERO holder — co-writer mounts
#                       and custody HANDOFF are rungs 9-10, so the frozen
#                       victim is the armed writer itself and the recovery
#                       actor is the rig driving the rung-2-proven product
#                       preempt primitive, the same act a successor's
#                       drain proof performs). Steps: sustained write load;
#                       SIGSTOP the armed writer past the membership TTL
#                       (its reader members observe the frozen owner and
#                       SELF-FENCE first — the S6 composition face); the
#                       recovery identity registers + PREEMPTS the
#                       zombie's WERO key on EVERY data namespace (PR
#                       preempt observed on target: report re-read); on
#                       SIGCONT the zombie's resumed DMA must be REJECTED
#                       BY THE DEVICE (reservation-conflict errno class),
#                       the zombie must FAIL-STOP its data plane
#                       (data_dma_fence_refusals moving; epoch_refusals ⊆
#                       fence_refusals — 0 here BY CONSTRUCTION: no
#                       custody moved inside the zombie's process), the
#                       resumed owner's TTL sweep must evict its dead
#                       members + mint their S7 dead epochs, and
#                       dlm_quarantined_offsets stays 0 with the reason
#                       stated (a READER's dead epoch names no offsets;
#                       the offset-holding cohorts are S9's custody
#                       grants and the job wire's destinations — their
#                       no-release-without-a-drain-proof law is pinned in
#                       cargo by tests/dlm_data_fence_tests.rs).
#   s7-kill-matrix [--rounds=N]  (rung 8; design row S7-b) kill -9 × N
#                       (default 10) of the ARMED
#                       writer at RANDOMIZED phases under sustained write
#                       load; each round: kill → dead-mount sweep →
#                       remount (the successor re-arms MW over the dead
#                       incarnation's STANDING WERO reservation — the
#                       device-observed takeover, fence_mode=1 asserted)
#                       → FULL online fsck with the C8 oracle
#                       (findings: 0; meta_kv_block_refs_drift == 0 — the
#                       default format stamps bit 9, so the durable
#                       ledger runs for real) → tripwires flat
#                       (invariant_tripwires, data_dma_fence_refusals,
#                       R5 backstops all 0 on the successor). COUNTED-
#                       RESTART discipline: any failure aborts the count;
#                       the matrix restarts from zero on the fixed
#                       binary. Reader recovery + dirty-skip tripwire
#                       asserted at matrix end.
#   sym-crash [--rounds=N]  (symmetric PR 10 — needs `mw_fleet.sh create
#                       --symmetric`; design §8 gates 3/4's LOCAL fleet
#                       leg) the s7-kill-matrix body on the SYMMETRIC
#                       fleet (bit 17 + SQUEEZEFS_SYMMETRIC_META=1 on the
#                       manager): kill -9 × N of the manager at randomized
#                       phases under sustained write load; per round the
#                       successor's own-residue recovery is asserted
#                       (appender_self_recoveries ≥ 1, live_pages_at_
#                       mount ≥ 1 — the dead incarnation's Live page),
#                       the symmetric tripwires flat (meta_kv_forest_key_
#                       violations, appender_fence_breach, manager_verb_
#                       refusals, meta_kv_replay_{key,lease,extent}_
#                       violations, fsck_slot_custody_conflicts,
#                       fsck_unrecovered_appenders, appender_park_expiries
#                       all 0), the manager lease `held`, and the FULL
#                       online fsck with the C8 oracle clean (C14/C15 ride
#                       it). On a `--writers=K` fleet (PR 12b) every
#                       JOINED WRITER must survive the manager's death and
#                       follow the SUCCESSOR (joined_wire_redials ≥ 1, its
#                       post-failover writes land and are read by the
#                       successor). The in-process matrix is
#                       tests/sym_crash_matrix_tests.rs.
#   sym-storm [--rounds=N]  (symmetric PR 12b — needs `mw_fleet.sh create
#                       --symmetric --writers=K` with K ≥ 2, a short
#                       --lease-ttl-ms) THE N-DAEMON ROW: the manager and
#                       every joined writer run the acked-writes oracle at
#                       once (per-file `dd conv=fsync`, a ledger of names
#                       whose fsync RETURNED); at a randomized phase ONE
#                       JOINER is killed -9; the manager's S6 eviction
#                       records its death past the lease TTL and the
#                       ledger poll RECOVERS its region (PR 10's driver on
#                       a real second daemon — appender_recoveries +1);
#                       every acked name of every writer is then present
#                       with content at the manager AND at a surviving
#                       joiner; the victim remounts into a FRESH region
#                       (§5.8.3, appender_self_recoveries 0) and reads
#                       every name; fsck clean; the symmetric must-stay-0
#                       set flat on every daemon. COUNTED-RESTART applies.
#                       PR 13: `--victims=K` kills K joiners AT ONCE (design
#                       §8 gate 4 (e) — one projection recovers K × volumes
#                       regions); `--cross-owner` runs a MOVER per joiner
#                       beside its oracle (its acked files renamed into a
#                       directory the manager holds — gate 4 (c): a victim
#                       dies mid-plan and the intent rolls FORWARD: every
#                       name at exactly one of source / destination, every
#                       returned mv at the destination). K = every joiner
#                       is admitted (gate 4 (e) proper — the manager reads
#                       the oracle). `--striped` flips every storm
#                       directory to SQZ_MWMATRIX_SYM_STRIPE_K (64) stripes
#                       before its creators start (gate 4 (d') — a holder
#                       dies with inserts in flight on its stripes; C17
#                       judged 0 after). On a `--token-readers` fleet the
#                       RECALLED-READER arm runs each round: the reader
#                       takes a token on a victim's acked object before
#                       the kill, the successor lessee (the manager)
#                       setattrs it after the recovery, the reader is
#                       recalled (`dlm_token_recalls_received`) and reads
#                       the new word exactly. The `sym-crash` leg (the
#                       manager's kill) on a ≥ 8-member fleet is gate 4
#                       (d''): every joiner parks at T_self and reclaims
#                       (`appender_park_expiries == 0`,
#                       `membership_self_fences == 0` per joiner), and the
#                       removed round directory is GONE through every
#                       mount ("deleted stays deleted").
#   sym-tarx            (symmetric PR 13 — design §8 gate 2; needs
#                       `mw_fleet.sh create --symmetric --writers>=1` and
#                       SQZ_MWMATRIX_TAR_SRC=<linux>/fs) `tar -x` on a JOINED
#                       WRITER re-mounted in a netns at netem 125 µs/end
#                       (250 µs wire RTT) into a directory IT created vs
#                       the manager-local S0 wall; A-B-B-A (sym, local,
#                       local, sym). GATE ≤ 1.10× S0. Engagement:
#                       wire_verbs_per_entry ≈ 0 (< 0.05), slot_handovers
#                       == 0, the must-stay-0 set flat.
#   sym-scale [--scale-ns=1,2,4,8] [--sym-files=N] [--sym-threads=T]
#             [--ingest-mb=M]  (PR 13 — gate 3; needs --writers >= max N − 1)
#                       aggregate create/s and ingest MiB/s with N RW
#                       mounts each writing its OWN directory (exactly N
#                       appenders live per row — the extra joiners are
#                       cleanly unmounted and rejoined between rows);
#                       GATE ≥ 0.7 × N × the N=1 rate; the manager's
#                       measured load and CPU reported per N. Engagement:
#                       appenders_known == N, slot_handovers == 0,
#                       slot_ships ≤ 1 per writer (its mkdir under /), dlm_rpcs == 0.
#   sym-shared-dir [--sym-files=N]  (PR 13 — gate 3b; needs --writers >= 2)
#                       N creators into ONE directory: the holder's flip
#                       to K stripes on the observed creator count
#                       (dir_stripe_flips == 1 on exactly one daemon),
#                       every post-flip foreign create a stripe ship
#                       (Σ dir_stripe_ships + own-stripe creates ≡ creates
#                       into the striped directory, read off the holders'
#                       served steps), slot_handovers == 0; then
#                       sym-shared-dir-ls: a COLD token reader's
#                       `readdir + stat` of the result = K stripe tokens +
#                       C inode tokens (+ the directory's own), 0 leaf
#                       reads (needs --token-readers).
#   sym-foreign-touch [--touch-rounds=R]  (PR 13 — gate 3c; --writers >= 2)
#                       each writer's job tree touched by the others: a
#                       LIVE holder's tree is shipped to, never moved; an
#                       IDLE tree touched with a dominating burst is
#                       HANDED OVER (slot_handovers ≥ 1, phase histogram
#                       reported); a PAUSED live job's tree stays under a
#                       single touch per round. Handovers/s and their
#                       cost against the node's own rate.
#   sym-readers         (PR 13 — gate 5; needs --symmetric --token-readers,
#                       N readers ≥ 1) a foreign create / rename / setattr
#                       is visible at a token reader's NEXT resolve — exact,
#                       never bounded (no sleep between the writer's ack
#                       and the reader's stat); the broadcast shape (every
#                       reader holds one file's token, the writer
#                       publishes) recalls one token per reader per
#                       publish — dlm_token_recall_fanout ≡ readers,
#                       dlm_token_recalls ≡ mutations × holders,
#                       dlm_token_recall_timeouts_live == 0; the recall-
#                       driven free-grace hold (free_grace_hold_ms ≈ one
#                       recall RTT, deferrals ≡ releases + offsets);
#                       reader_staleness_bound_ms == 0 on every reader.
#   sym-walls [--walls-files=F] [--walls-mb=M]  (PR 13 — gate 7, the
#                       RELOCATED WALLS; needs --symmetric --writers >= 1)
#                       Row (a) the FREE wall per allocation holder under
#                       w_rewrite at N: every joiner pre-writes F files of
#                       M MiB (whole 4 MiB striped blocks) in its own
#                       directory, then rewrites each in place at once
#                       (`dd conv=notrunc,fsync` — every rewritten block is a
#                       CoW displacement whose terminal free SHIPS to the
#                       data volume's allocation HOLDER, the manager).
#                       Engagement: Σ joiners' meta_ship_publish.free_
#                       shipped_blocks ≡ the displaced blocks (F × M/4 per
#                       joiner) ≡ the holder's free_served_blocks; the
#                       holder's free rate (blocks/s) and manager_service_
#                       ns / manager_verbs_per_s / manager_load_pct reported;
#                       the must-stay-0 set per row. Row (b) the JOIN STORM:
#                       every joiner leaves cleanly, then ALL remount at
#                       once — wall to the last joiner armed, the manager's
#                       manager_verbs delta and its service time over the
#                       storm, and each joiner's `mkdir /jobs/<j>` right
#                       after its join (the ship to /jobs's holder):
#                       xv_cross_owner_steps_served at the manager ≡ N.
#                       N = the fleet's writer count; N = 32 is the box's.
#   sym-foreign-file [--ff-files=F]  (PR 13b — the record-level metanode
#                       ship; needs --symmetric --writers >= 2) every
#                       writer chmod / touch / setfattr / APPEND (+ fsync)
#                       / truncate / unlink a COLLEAGUE's files (the next
#                       joiner's, the last joiner's the manager's) under
#                       the acked-writes + deleted-stays-deleted oracle,
#                       read back at the HOLDER and at a THIRD mount.
#                       Engagement: Σ record_ships ≡ Σ record_served across
#                       the daemons, record_refusals 0 everywhere, Σ
#                       foreign_publish_ships ≡ Σ foreign_publish_served, foreign_publish_refusals == 0,
#                       dlm_custody_via_slot_holder > 0 at every mutator;
#                       fsck + the must-stay-0 set after. LOCAL = "it
#                       works" (the box prices the row).
#   sym-reclaim-hint [--rh-files=F]  (PR 13h — the corpse-reclaimer law's
#                       PRODUCTION wiring; needs --symmetric --writers >= 2,
#                       the fleet created with SQUEEZEFS_SYM_AFFINITY_MAX_MB
#                       set so a joiner's file mints into its DIRECTORY's
#                       slot) three shapes: (A) the box's — a joiner reads
#                       the manager's tree as a token client, the manager
#                       `rm -rf`s it (recall → prune → FORGET → the hint to
#                       the manager); (B) the MOVED-slot corpse — a joiner's
#                       8 MiB file unlinked while OPEN, its directory's slot
#                       handed to a second joiner by dominance, the close's
#                       FORGET hinted to the NEW holder, which destroys it;
#                       (C) the UNLEASED corpse — the same, then the new
#                       holder LEAVES (the slot Unleased) before the close:
#                       hinted to the MANAGER. Laws: Σ hint_inos_shipped ≡
#                       Σ (served + forwarded + misrouted) across the
#                       daemons, a LIVE file's close shipping nothing
#                       (reclaim_hint_skipped_live), reclaim_hint_failures
#                       0, reclaim_destroy_refused_release_failed 0, no
#                       `corrupt KV encoding`
#                       / `destroy WITHHELD` line, the corpses' blocks
#                       released at the reclaimer (meta_kv_block_refs_
#                       released) and cleared at the allocation holder
#                       (data_alloc_bitmap_population), the post-leave +
#                       offline census 0 / 0. LOCAL = "it works".
#   vm-multi-identity   (rung 6b — needs --vm=V) the N>=2-identity mount
#                       shape LIVE inside guest 0 on the 0030 kernel:
#                       writer A formats --multi-writer over the reserved
#                       guest pair (records name $VM_GW — the guest-
#                       domain fabric address), mounts with explicit
#                       identity (daemon-owned data connects resolve A's
#                       OWN scoped head); writer-candidate B's explicit-
#                       identity mount gets ITS OWN scoped head, passes
#                       rule 2 (the 5b/6b stack unblocks the fabric
#                       layer) and refuses BEYOND identity at the D0
#                       single-writer guard (arming is rungs 7-10 —
#                       posture/admission stay gated). Asserts the two
#                       heads are distinct and B's refusal is NOT the
#                       rule-2 class.
#   s10c-fsck-scale [--corpus-mb=M] [--runs=R]  (rung 10c, KD-MW-16;
#                       design-mw-fleet-jobs §9 — needs a fleet created
#                       with --membership and >= 4 members) THE FLEET
#                       FSCK SCALING ROWS: one corpus written+fsynced on
#                       the writer, then `squeezefs fsck --scrub` timed
#                       at fleet widths N=1/2/4 (readers remounted per
#                       row; every member remounted per RUN so each run
#                       is cold — the R1b second-touch law keeps one
#                       scrub pass from warming the disk cache, and the
#                       remount clears the RAM ghosts). Row validity =
#                       the engagement ledger (writer deltas:
#                       job_fleet_shards_dispatched == completed == N-1,
#                       relocal 0; each reader: job_fleet_worker_shards
#                       == 1) + the exactly-once coverage law (the
#                       writer's fsck_inodes_scanned delta is IDENTICAL
#                       at every width — the coordinator publishes the
#                       whole fleet's share) + findings: 0 at every N.
#                       GATE: median wall-clock scales >= 0.6x-linear to
#                       N=4 (t1/t4 >= 2.4). Quiet-gated: foreign cargo
#                       work or high load labels the table PROVISIONAL
#                       (the gate still enforces). Evidence tier:
#                       measured-simulated (one box, co-located members
#                       sharing the device + CPUs — stated in the row).
#   s10c-kill-shard [--corpus-mb=M]  (rung 10c, KD-MW-16 — needs
#                       --membership and >= 3 members) kill -9 a member
#                       MID-SHARD: a no-kill baseline learns the fleet's
#                       exact census total, then a throttled fleet fsck
#                       runs while reader 1 dies holding its shard. The
#                       lease expires (job_remote_lease_expiries moves),
#                       the residue RE-LEASES (relocal or a re-dispatch),
#                       the pass completes findings: 0 with the census
#                       total IDENTICAL to the baseline (zero
#                       double-coverage — the fencing-checked proposal
#                       law), and read-shard expiry moves NEITHER
#                       job_remote_pr_preempts NOR the destination
#                       quarantine (the design-§5 split: a read worker
#                       DMAs nothing). The victim remounts at leg end
#                       (zero residue).
#
# Usage:  sudo tests/run_mw_matrix.sh <leg> [--require-host-scoped-subsys]
#         [--window=S] [--netem=MS] [--victim=IDX]   (the s6-* legs)
#         [--rounds=N]                               (s7-kill-matrix, sym-crash)
#         [--venue=laptop|box]                       (the sym-* legs; default laptop — the
#                                                     VENUE word: `appender_flush_ceiling_overruns`,
#                                                     the ruling's timing-shaped gauge, is REPORTED
#                                                     per round as venue-attributed on the laptop
#                                                     and stays must-stay-0 on the box; every other
#                                                     law is fatal on both)
#         bash tests/run_mw_matrix.sh pv-volume-scaling [--ns=…] [--files=N]
#              [--idle-secs=S] [--repeats=R] [--threads=T] [--budget=SIZE]
#              [--node-cache-mb=MB] [--tag=NAME]     (unprivileged, fleet-free)
# Exit:   0 green (or a loud SKIP), nonzero on any INVALID row / violation.
#
# Requires: root, a live fleet (sudo tests/mw_fleet.sh create N=2), python3.

set -euo pipefail
# A `set -e` exit is never SILENT (PR 12b round 3: a `sym-storm` round
# died between its fsck and its row with rc 1 and no line — the
# "deleted stays deleted" arm's bare `timeout … stat`, whose EXPECTED
# failure `set -e` took as the leg's). Every non-zero exit names its line
# and command; `-E` carries the trap into the legs' functions.
set -E
trap 'rc=$?; [ "$rc" = "0" ] || echo "[mwmatrix] ERROR: exit $rc at line $LINENO: $BASH_COMMAND" >&2' ERR

REPO="$(cd "$(dirname "$0")/.." && pwd)"
SQZ="${SQZ_BIN:-$REPO/target/release/squeezefs}"
[ -x "$SQZ" ] || SQZ="$REPO/target/debug/squeezefs"

STATE="${SQZ_MWFLEET_STATE_DIR:-/run/squeezefs-mwfleet}"
MEMBERS="$STATE/members.tsv"
CONF="$STATE/config.env"

log() { echo "[mwmatrix] $*"; }
warn() { echo "[mwmatrix] WARN: $*" >&2; }
die() {
    echo "[mwmatrix] ERROR: $*" >&2
    # A leg that dies mid-storm leaves its background writers (the acked
    # writers, the cross-owner movers, the storms) running against the
    # fleet — the next leg then measures a fleet under someone else's
    # load (PR 13's from-zero batch found the storm's movers alive an hour
    # after the leg's exit). Every child of this harness goes with it.
    pkill -P $$ 2>/dev/null || true
    exit 1
}
skip() {
    echo "[mwmatrix] SKIP: $*" >&2
    exit 0
}

ensure_root() {
    [ "$(id -u)" -eq 0 ] && return 0
    log "root required (fleet mounts, stats inodes) — re-executing via sudo"
    local knobs=()
    # SQZ_MWMATRIX_* rides too (rung 14): the tarx legs' TAR_SRC was
    # silently dropped by the re-exec, downgrading a real-tree run to the
    # synthesized fallback unless invoked as root directly.
    while IFS= read -r kv; do knobs+=("$kv"); done \
        < <(env | grep -E '^(SQZ_MWFLEET_|SQZ_MWMATRIX_|SQZ_BIN=)' || true)
    exec sudo env "${knobs[@]}" bash "$0" "$@"
}

LEG="${1:-}"
[ -n "$LEG" ] || {
    awk 'NR > 1 && /^#/ { sub(/^# ?/, ""); print; next } NR > 1 { exit }' "$0"
    exit 1
}
shift || true
REQUIRE_HS=0
S6_WINDOW_S="${SQZ_MWMATRIX_S6_WINDOW_S:-600}"
S6_NETEM_MS=200
S6_VICTIM=""
S7_ROUNDS=10
S10C_MB="${SQZ_MWMATRIX_S10C_MB:-3072}"
S10C_RUNS="${SQZ_MWMATRIX_S10C_RUNS:-3}"
PV_NS="${SQZ_MWMATRIX_PV_NS:-1,4,16,46}"
PV_FILES="${SQZ_MWMATRIX_PV_FILES:-20000}"
PV_IDLE_S="${SQZ_MWMATRIX_PV_IDLE_S:-30}"
PV_THREADS="${SQZ_MWMATRIX_PV_THREADS:-8}"
PV_REPEATS="${SQZ_MWMATRIX_PV_REPEATS:-3}"
PV_BUDGET="${SQZ_MWMATRIX_PV_BUDGET:-}"
PV_NODE_CACHE_MB="${SQZ_MWMATRIX_PV_NODE_CACHE_MB:-}"
# The read rows measure the DAEMON's metadata plane, so the kernel's
# per-class attr/entry caches are pinned OFF (instrument alignment: at the
# shipped 1 s TTLs a re-stat sweep never reaches the daemon at all).
PV_TTL_MS="${SQZ_MWMATRIX_PV_TTL_MS:-0}"
PV_TAG="${SQZ_MWMATRIX_PV_TAG:-derived}"
# Symmetric PR 13, gate 6 (format cost), INVERTED at the PR-14 flip: the
# default formats the plain default — the symmetric forest, every mount an
# armed writer — and the emitter adds the forest's per-volume columns (slot
# trees minted, slot-tree bytes p99/max vs A_max, ring bytes, checkpoints
# over the create phase); `--single-writer` formats the flat solo class
# (the A arm — what every pre-PR-13 row measured).
PV_SYMMETRIC="${SQZ_MWMATRIX_PV_SYMMETRIC:-1}"
# Paced-quiet thresholds (the thermally-capped-box law): rows resume
# below PV_RESUME_C and never wait longer than PV_WAIT_MAX_S.
PV_RESUME_C="${SQZ_MWMATRIX_PV_RESUME_C:-68}"
PV_WAIT_MAX_S="${SQZ_MWMATRIX_PV_WAIT_MAX_S:-1800}"
# Symmetric PR 13 (the acceptance legs, design §8 gates 2/3/3b/3c/5):
# files per writer for the create rows, threads per writer, the N ladder
# for sym-scale, the stripe-row creators, the shared-dir file count, the
# ingest MiB per writer, the foreign-touch cadence and the readers row's
# member count.
SYM_FILES="${SQZ_MWMATRIX_SYM_FILES:-40000}"
SYM_THREADS="${SQZ_MWMATRIX_SYM_THREADS:-4}"
SYM_SCALE_NS="${SQZ_MWMATRIX_SYM_SCALE_NS:-1,2,4,8}"
SYM_INGEST_MB="${SQZ_MWMATRIX_SYM_INGEST_MB:-1024}"
SYM_TOUCH_ROUNDS="${SQZ_MWMATRIX_SYM_TOUCH_ROUNDS:-3}"
# sym-walls (gate 7): the rewrite set per joiner — F files of M MiB (whole
# 4 MiB blocks), rewritten in place once.
SYM_WALLS_FILES="${SQZ_MWMATRIX_SYM_WALLS_FILES:-16}"
SYM_WALLS_MB="${SQZ_MWMATRIX_SYM_WALLS_MB:-64}"
# sym-foreign-file (PR 13b): files per writer a colleague mutates.
SYM_FF_FILES="${SQZ_MWMATRIX_SYM_FF_FILES:-64}"
# sym-reclaim-hint (PR 13h): the manager's tree a joiner instantiates, in files.
SYM_RH_FILES="${SQZ_MWMATRIX_SYM_RH_FILES:-128}"
SYM_VICTIMS=1
SYM_XO=0
SYM_STRIPED=0
# The VENUE word (PR 13 review round 1, Issue 2; the venue ruling
# `51bf21e1`): on the LAPTOP a timing-shaped must-stay-0 gauge —
# `appender_flush_ceiling_overruns`, the one the ruling names — is a
# venue reading the box bracket decides, so it is REPORTED per round as
# `venue-attributed` (ledger `$STATE/rows/venue-attributed.txt`) instead
# of failing the leg; on the BOX it stays must-stay-0. Every other law
# (acked loss, deleted-stays-deleted, fsck, refusals, violations,
# conflicts) is fatal on both venues.
SYM_VENUE="${SQZ_MWMATRIX_VENUE:-laptop}"
# `--striped`'s K: the design's operating point (`MINT_SPREAD` = 64 — a
# flip to fewer stripes than the fleet's creators is a different row).
SYM_STRIPE_K="${SQZ_MWMATRIX_SYM_STRIPE_K:-64}"
for a in "$@"; do
    case "$a" in
    --sym-files=*) SYM_FILES="${a#--sym-files=}" ;;
    --sym-threads=*) SYM_THREADS="${a#--sym-threads=}" ;;
    --scale-ns=*) SYM_SCALE_NS="${a#--scale-ns=}" ;;
    --ingest-mb=*) SYM_INGEST_MB="${a#--ingest-mb=}" ;;
    --touch-rounds=*) SYM_TOUCH_ROUNDS="${a#--touch-rounds=}" ;;
    --walls-files=*) SYM_WALLS_FILES="${a#--walls-files=}" ;;
    --walls-mb=*) SYM_WALLS_MB="${a#--walls-mb=}" ;;
    --ff-files=*) SYM_FF_FILES="${a#--ff-files=}" ;;
    --rh-files=*) SYM_RH_FILES="${a#--rh-files=}" ;;
    --victims=*) SYM_VICTIMS="${a#--victims=}" ;;
    --cross-owner) SYM_XO=1 ;;
    --striped) SYM_STRIPED=1 ;;
    --venue=*) SYM_VENUE="${a#--venue=}" ;;
    --require-host-scoped-subsys) REQUIRE_HS=1 ;;
    --window=*) S6_WINDOW_S="${a#--window=}" ;;
    --netem=*) S6_NETEM_MS="${a#--netem=}" ;;
    --victim=*) S6_VICTIM="${a#--victim=}" ;;
    --rounds=*) S7_ROUNDS="${a#--rounds=}" ;;
    --corpus-mb=*) S10C_MB="${a#--corpus-mb=}" ;;
    --runs=*) S10C_RUNS="${a#--runs=}" ;;
    --ns=*) PV_NS="${a#--ns=}" ;;
    --files=*) PV_FILES="${a#--files=}" ;;
    --idle-secs=*) PV_IDLE_S="${a#--idle-secs=}" ;;
    --threads=*) PV_THREADS="${a#--threads=}" ;;
    --repeats=*) PV_REPEATS="${a#--repeats=}" ;;
    --budget=*) PV_BUDGET="${a#--budget=}" ;;
    --node-cache-mb=*) PV_NODE_CACHE_MB="${a#--node-cache-mb=}" ;;
    --kernel-ttl-ms=*) PV_TTL_MS="${a#--kernel-ttl-ms=}" ;;
    --tag=*) PV_TAG="${a#--tag=}" ;;
    --symmetric) PV_SYMMETRIC=1 ;;
    --single-writer) PV_SYMMETRIC=0 ;;
    *) die "unknown argument '$a'" ;;
    esac
done
case "$SYM_VENUE" in laptop | box) ;; *) die "--venue takes laptop|box (got '$SYM_VENUE')" ;; esac
[[ "$S6_WINDOW_S" =~ ^[0-9]+$ ]] || die "--window takes seconds (got '$S6_WINDOW_S')"
[[ "$S6_NETEM_MS" =~ ^[0-9]+$ ]] || die "--netem takes ms (got '$S6_NETEM_MS')"
[[ "$S7_ROUNDS" =~ ^[0-9]+$ ]] && [ "$S7_ROUNDS" -ge 1 ] || die "--rounds takes a positive integer (got '$S7_ROUNDS')"
[[ "$S10C_MB" =~ ^[0-9]+$ ]] && [ "$S10C_MB" -ge 256 ] || die "--corpus-mb takes MiB >= 256 (got '$S10C_MB')"
[[ "$S10C_RUNS" =~ ^[0-9]+$ ]] && [ "$S10C_RUNS" -ge 1 ] || die "--runs takes a positive integer (got '$S10C_RUNS')"

# =============================================================================
# pv-volume-scaling — the PR 0 VIABILITY GATE leg
# (design-per-volume-claim-admission PR 0; risks R9 and R15)
#
# It sits AHEAD of the fleet preamble on purpose: this leg drives its own
# fixture (tests/pv_volume_set.sh — one daemon, N metadata volumes) and
# needs no fleet, no fabric and no root, so `ensure_root` and the live-
# fleet CONF requirement below must not apply to it. Everything it uses
# (log/die/skip, the arg parse) is already defined above.
#
# Rows, per N in --ns (default 1,4,16,46 — the PR 0 widths):
#   1  aggregate daemon RSS/anon at mount and after a fixed metadata
#      workload (plus smaps_rollup anon + AnonHugePages),
#   2  checkpoint CPU: idle-window daemon CPU (utime+stime deltas) and the
#      meta_kv_checkpoints rate — the per-volume background-task cost,
#   3  R5: mem_budget level / yellow / red / hard_backstops and the
#      kv_node_cache component (current bytes AND its shed count),
#   4  meta_kv_journal_entries_per_volume balance (spec-R4's own row):
#      max, mean, max/mean and the count of volumes that MOVED,
#   5  the R15 read-tier row: node-cache hit rate over a cold and a warm
#      stat pass — run again with --node-cache-mb=<divided> to price the
#      set-aware derivation PR 9 would land.
#
# Substrate: whatever the fixture is given (SQZ_PVSET_META_DEVS switches
# it to devices). Every row carries the label. The memory-plane rows
# (1/3/5) are substrate-independent by construction; the CPU and journal
# rows are barrier-sensitive and need the devsub re-run before they are
# acceptance evidence — the emitter prints that with the table.
# =============================================================================
pv_cpu_temp_mc() { # max Tctl/Tdie in millidegrees, 0 when no sensor
    local hw name t v max_mc=0
    for hw in /sys/class/hwmon/hwmon*; do
        [ -r "$hw/name" ] || continue
        name="$(cat "$hw/name")"
        case "$name" in
        zenpower | k10temp | coretemp)
            for t in "$hw"/temp[12]_input; do
                [ -r "$t" ] || continue
                v="$(cat "$t")"
                [ "$v" -gt "$max_mc" ] && max_mc="$v"
            done
            ;;
        esac
    done
    echo "$max_mc"
}

pv_foreign_work() { # comm-exact (pgrep -f false-positives on our own line)
    pgrep -x cargo >/dev/null || pgrep -x rustc >/dev/null ||
        pgrep -x fio >/dev/null || pgrep -x elbencho >/dev/null
}

# The run_bench_baseline.sh discipline, paced form: FOREIGN work refuses
# immediately (poll, never contend), while the box's own heat and the
# previous row's load decay are WAITED OUT — a thermally-capped box would
# otherwise abort a sweep half-way through and leave the widths measured
# under different conditions, which is worse than waiting.
pv_quiet_or_die() {
    pv_foreign_work &&
        die "foreign cargo/rustc/fio work running — refuse the measured row"
    local waited=0 mc load announced=0
    while :; do
        mc="$(pv_cpu_temp_mc)"
        load="$(cut -d' ' -f1 /proc/loadavg)"
        if { [ "$mc" = "0" ] || [ "$mc" -lt "$((PV_RESUME_C * 1000))" ]; } &&
            awk -v l="$load" 'BEGIN{exit !(l < 4.0)}'; then
            [ "$mc" = "0" ] && [ "$announced" = "0" ] &&
                warn "no CPU temperature sensor — thermal gate skipped"
            [ "$waited" -gt 0 ] &&
                log "quiet after ${waited}s (cpu $((mc / 1000)) °C, load1=$load)"
            return 0
        fi
        [ "$announced" = "0" ] &&
            log "pacing: cpu $((mc / 1000)) °C (resume < $PV_RESUME_C), load1=$load — waiting"
        announced=1
        sleep 15
        waited=$((waited + 15))
        [ "$waited" -ge "$PV_WAIT_MAX_S" ] &&
            die "box never went quiet in ${PV_WAIT_MAX_S}s (cpu $((mc / 1000)) °C, load1=$load)"
        pv_foreign_work &&
            die "foreign cargo/rustc/fio work started — refuse the measured row"
    done
}

pv_snap() { # pid mnt out-prefix
    local pid="$1" mnt="$2" out="$3"
    cat "$mnt/.stats" >"$out.json" || die "cannot read $mnt/.stats"
    {
        echo "ts_ns $(date +%s%N)"
        awk '{print "utime " $14; print "stime " $15}' "/proc/$pid/stat"
        sed -nE 's/^(VmRSS|VmHWM|Threads):[[:space:]]*([0-9]+).*/\1 \2/p' \
            "/proc/$pid/status"
        sed -nE 's/^(Rss|Pss|Anonymous|AnonHugePages):[[:space:]]*([0-9]+).*/smaps_\1 \2/p' \
            "/proc/$pid/smaps_rollup" 2>/dev/null || true
    } >"$out.proc"
}

pv_ops_s() { # <mdstorm output line> -> ops/s  ("create ops=N wall_s=W ops_s=X")
    awk '{for (i = 1; i <= NF; i++) if ($i ~ /^ops_s=/) { sub(/^ops_s=/, "", $i); print $i }}' <<<"$1"
}

# The phase plan (each snapshot is stats JSON + /proc CPU/RSS):
#   p0  mount + settle                     (fresh, clean daemon)
#   p1  after the CLEAN idle window        -> per-volume background tick cost
#   p2  after the create workload          -> journal balance, node-cache fill
#   p3  after the WARM stat pass           -> warm node-cache hit rate
#   p4  after the POST-WORK idle window    -> checkpoint drain cost + rate
#   p5  after a REMOUNT (timed, NEW pid)   -> mount/replay cost at width N
#   p6  after the COLD stat pass           -> cold node-cache hit rate
# The remount is what makes "cold" honest: counters and caches restart with
# the daemon, so p5->p6 is a clean-cache read pass (kernel TTLs are pinned
# to 0 by the fixture, so the stat sweep reaches the daemon at all).
pv_one_row() { # n rep rowdir storm fixture
    local n="$1" rep="$2" rowdir="$3" storm="$4" fixture="$5"
    local st="$PV_ROOT/n${n}r${rep}" pfx="$rowdir/n${n}r${rep}"
    local -a create_args=("$n" "--tag=pv-n$n-r$rep" "--kernel-ttl-ms=$PV_TTL_MS")
    [ -n "$PV_BUDGET" ] && create_args+=("--mem-budget=$PV_BUDGET")
    [ -n "$PV_NODE_CACHE_MB" ] && create_args+=("--node-cache-mb=$PV_NODE_CACHE_MB")

    SQZ_PVSET_STATE_DIR="$st" bash "$fixture" teardown >/dev/null 2>&1 || true
    local t0 t1 mount_s remount_s
    t0="$(date +%s.%N)"
    SQZ_PVSET_STATE_DIR="$st" SQZ_PVSET_SYMMETRIC="$PV_SYMMETRIC" bash "$fixture" create "${create_args[@]}" \
        >"$pfx.fixture.log" 2>&1 || {
        cat "$pfx.fixture.log" >&2
        die "fixture create N=$n failed"
    }
    t1="$(date +%s.%N)"
    mount_s="$(awk -v a="$t0" -v b="$t1" 'BEGIN{printf "%.3f", b - a}')"

    local mnt pid
    mnt="$(SQZ_PVSET_STATE_DIR="$st" bash "$fixture" mnt)"
    pid="$(SQZ_PVSET_STATE_DIR="$st" bash "$fixture" pid)"

    sleep 3 # settle: the 1 Hz R5 sampler + the first checkpoint ticks
    pv_snap "$pid" "$mnt" "$pfx.p0"
    sleep "$PV_IDLE_S" # the CLEAN idle window: background-task cost only
    pv_snap "$pid" "$mnt" "$pfx.p1"

    local work="$mnt/storm" line create_s stat_warm_s stat_cold_s
    mkdir -p "$work"
    line="$("$storm" "$work" "$PV_THREADS" "$PV_FILES" create)" ||
        die "mdstorm create failed at N=$n"
    create_s="$(pv_ops_s "$line")"
    pv_snap "$pid" "$mnt" "$pfx.p2"
    line="$("$storm" "$work" "$PV_THREADS" "$PV_FILES" stat)" ||
        die "mdstorm stat (warm) failed at N=$n"
    stat_warm_s="$(pv_ops_s "$line")"
    pv_snap "$pid" "$mnt" "$pfx.p3"
    sleep "$PV_IDLE_S" # the POST-WORK idle window: the checkpoint drain
    pv_snap "$pid" "$mnt" "$pfx.p4"

    t0="$(date +%s.%N)"
    SQZ_PVSET_STATE_DIR="$st" bash "$fixture" remount >>"$pfx.fixture.log" 2>&1 ||
        die "fixture remount N=$n failed"
    t1="$(date +%s.%N)"
    remount_s="$(awk -v a="$t0" -v b="$t1" 'BEGIN{printf "%.3f", b - a}')"
    local remount_open_s
    remount_open_s="$(SQZ_PVSET_STATE_DIR="$st" bash "$fixture" remount-times |
        sed -nE 's/^PVSET_LAST_MOUNT_S=(.*)$/\1/p')"
    pid="$(SQZ_PVSET_STATE_DIR="$st" bash "$fixture" pid)"
    pv_snap "$pid" "$mnt" "$pfx.p5"
    line="$("$storm" "$work" "$PV_THREADS" "$PV_FILES" stat)" ||
        die "mdstorm stat (cold) failed at N=$n"
    stat_cold_s="$(pv_ops_s "$line")"
    pv_snap "$pid" "$mnt" "$pfx.p6"

    {
        echo "n $n"
        echo "rep $rep"
        echo "files $PV_FILES"
        echo "threads $PV_THREADS"
        echo "idle_s $PV_IDLE_S"
        echo "mount_s $mount_s"
        echo "remount_s $remount_s"
        echo "remount_open_s ${remount_open_s:-0}"
        echo "create_ops_s $create_s"
        echo "stat_warm_ops_s $stat_warm_s"
        echo "stat_cold_ops_s $stat_cold_s"
        echo "substrate ${SQZ_PVSET_SUBSTRATE:-file}"
        echo "budget_label ${PV_BUDGET:-derived}"
        echo "node_cache_label ${PV_NODE_CACHE_MB:-derived}"
        echo "ttl_ms $PV_TTL_MS"
        echo "tag $PV_TAG"
        echo "layout $([ "$PV_SYMMETRIC" = "1" ] && echo symmetric || echo flat)"
    } >"$pfx.meta"

    SQZ_PVSET_STATE_DIR="$st" bash "$fixture" teardown >>"$pfx.fixture.log" 2>&1 ||
        die "fixture teardown N=$n failed"
}

pv_emit() { # rowdir
    python3 - "$1" <<'PYEOF'
import glob, json, os, re, statistics, sys

rowdir = sys.argv[1]

def load_stats(p):
    m = json.load(open(p))["metrics"]
    return m

def load_proc(p):
    out = {}
    for line in open(p):
        k, _, v = line.strip().partition(" ")
        try:
            out[k] = int(v)
        except ValueError:
            out[k] = v
    return out

def meta(p):
    out = {}
    for line in open(p):
        k, _, v = line.strip().partition(" ")
        out[k] = v
    return out

HZ = os.sysconf("SC_CLK_TCK")
MIB = 1024 * 1024

PHASES = 7

rows, violations = [], []
for mpath in sorted(glob.glob(os.path.join(rowdir, "*.meta"))):
    pfx = mpath[: -len(".meta")]
    md = meta(mpath)
    n = int(md["n"])
    files = int(md["files"])
    s = {ph: load_stats(f"{pfx}.p{ph}.json") for ph in range(PHASES)}
    pr = {ph: load_proc(f"{pfx}.p{ph}.proc") for ph in range(PHASES)}

    def comp(ph, name, field="current"):
        return s[ph]["mem_budget_components"].get(name, {}).get(field, 0)

    def cpu_s(a, b):
        return ((pr[b]["utime"] + pr[b]["stime"]) - (pr[a]["utime"] + pr[a]["stime"])) / HZ

    def wall_s(a, b):
        return (pr[b]["ts_ns"] - pr[a]["ts_ns"]) / 1e9

    def total(v):  # per-volume arrays sum; scalars pass through
        return sum(v) if isinstance(v, list) else v

    def as_list(v):  # per-volume arrays as is; a scalar as a one-element list
        return v if isinstance(v, list) else [v]

    def d(a, b, key):
        return total(s[b].get(key, 0)) - total(s[a].get(key, 0))

    idle_wall = wall_s(0, 1)
    idle_cpu = cpu_s(0, 1)
    work_idle_wall = wall_s(3, 4)
    work_idle_cpu = cpu_s(3, 4)
    jrnl0 = s[0]["meta_kv_journal_entries_per_volume"]
    jrnl1 = s[1]["meta_kv_journal_entries_per_volume"]
    jrnl2 = s[2]["meta_kv_journal_entries_per_volume"]
    if len(jrnl2) != n:
        violations.append(f"N={n} r{md['rep']}: the daemon opened {len(jrnl2)} volumes, not {n}")
    # Row 4 — spec-R4 balance over the CREATE phase (p1 -> p2).
    work_jrnl = [b - a for a, b in zip(jrnl1, jrnl2)]
    jmax, jsum = max(work_jrnl), sum(work_jrnl)
    jmean = jsum / len(work_jrnl)
    moved = sum(1 for v in work_jrnl if v > 0)
    # A volume "carries the stream" when it takes >= 10% of the mean-fair
    # share; the count is the honest balance statement, not a pass/fail.
    carrying = sum(1 for v in work_jrnl if v >= 0.1 * jmean)
    # Row 5 — node-cache hit rate over the WARM (p2->p3, same daemon) and
    # COLD (p5->p6, post-remount daemon) stat passes.
    def hitrate(a, b):
        h, m_ = d(a, b, "meta_kv_node_cache_hits"), d(a, b, "meta_kv_node_cache_misses")
        return (100.0 * h / (h + m_)) if (h + m_) else float("nan"), h + m_
    hr_warm, probes_warm = hitrate(2, 3)
    hr_cold, probes_cold = hitrate(5, 6)
    misses_cold = d(5, 6, "meta_kv_node_cache_misses")
    if probes_cold == 0 or probes_warm == 0:
        violations.append(f"N={n} r{md['rep']}: no node-cache probes in a stat pass (row 5 not engaged)")
    if d(1, 2, "meta_kv_journal_entries") <= 0:
        violations.append(f"N={n} r{md['rep']}: the create phase emitted no journal entries (row not engaged)")
    if s[6].get("dlm_rpcs", 0) != 0:
        violations.append(f"N={n} r{md['rep']}: dlm_rpcs != 0 (solo re-gate violated)")
    if d(0, 4, "invariant_tripwires") != 0 or s[6].get("invariant_tripwires", 0) != 0:
        violations.append(f"N={n} r{md['rep']}: invariant_tripwires moved")
    if s[4].get("meta_kv_revalidate_dirty_skips", 0) != 0:
        violations.append(f"N={n} r{md['rep']}: meta_kv_revalidate_dirty_skips != 0")
    if total(s[6].get("meta_kv_replay_dropped_torn", 0)) != 0:
        violations.append(f"N={n} r{md['rep']}: the remount dropped torn journal entries")

    budget = s[4]["mem_budget_bytes"]
    # The R9 arithmetic, on the constants this mount actually resolved:
    # per-volume target = max(budget/16, 512 MiB) unless the knob pins it
    # (backend::resolve_node_cache_budget). Labeled arithmetic-on-
    # measured-constants wherever it is published.
    knob = md["node_cache_label"]
    per_vol = int(knob) * MIB if knob.isdigit() else max(budget // 16, 512 * MIB)
    agg = per_vol * n

    rows.append({
        "n": n,
        "rep": int(md["rep"]),
        "files": files,
        "substrate": md["substrate"],
        "layout": md.get("layout", "flat"),
        "budget_lbl": md["budget_label"],
        "nc_lbl": knob,
        "mount_s": float(md["mount_s"]),
        "remount_s": float(md["remount_s"]),
        "remount_open_s": float(md["remount_open_s"]),
        "rss_mount_mb": pr[0]["VmRSS"] / 1024,
        "anon_mount_mb": pr[0].get("smaps_Anonymous", 0) / 1024,
        "rss_work_mb": pr[4]["VmRSS"] / 1024,
        "anon_work_mb": pr[4].get("smaps_Anonymous", 0) / 1024,
        "thr": pr[0]["Threads"],
        "idle_cpu_pct": 100.0 * idle_cpu / idle_wall,
        "widle_cpu_pct": 100.0 * work_idle_cpu / work_idle_wall,
        "idle_ckpt_s": d(0, 1, "meta_kv_checkpoints") / idle_wall,
        "widle_ckpt_s": d(3, 4, "meta_kv_checkpoints") / work_idle_wall,
        "idle_jrnl_s": (sum(jrnl1) - sum(jrnl0)) / idle_wall,
        "work_cpu_s": cpu_s(1, 2),
        "create_ops_s": float(md["create_ops_s"]),
        "stat_cold_ops_s": float(md["stat_cold_ops_s"]),
        "stat_warm_ops_s": float(md["stat_warm_ops_s"]),
        "nc_mb": comp(4, "kv_node_cache") / MIB,
        "nc_b_per_file": comp(4, "kv_node_cache") / files,
        "nc_evict": d(0, 4, "meta_kv_node_cache_evictions"),
        "nc_sheds": comp(4, "kv_node_cache", "sheds"),
        "cold_miss": misses_cold,
        "r5_lvl": s[4]["mem_budget_level"],
        "r5_yellow": d(0, 4, "mem_budget_yellow_events"),
        "r5_red": d(0, 4, "mem_budget_red_events"),
        "r5_backstop": d(0, 4, "mem_budget_hard_backstops"),
        "budget_gb": budget / (1 << 30),
        "pervol_target_mb": per_vol / MIB,
        "agg_target_gb": agg / (1 << 30),
        "agg_over_budget": agg / budget,
        "j_max": jmax,
        "j_mean": jmean,
        "j_maxmean": (jmax / jmean) if jmean else float("nan"),
        "j_moved": moved,
        "j_carrying": carrying,
        "hr_cold": hr_cold,
        "hr_warm": hr_warm,
        # Symmetric PR 13, gate 6 — the forest's format cost per volume
        # (every one 0 on a FLAT set): slot trees minted over the create
        # phase, the lease family's slot-tree bytes (p99 / max over the
        # set) against the affinity cap in force, the appender ring's
        # bytes per volume (the fixed ring on a solo set), and the
        # checkpoints the create phase cost (each writes every appender
        # page + the ledger).
        "f_slot_trees": d(0, 4, "meta_kv_forest_slot_trees_minted"),
        "f_tree_p99_kb": max(as_list(s[4].get("slot_tree_bytes_p99", 0)) or [0]) / 1024,
        "f_tree_max_kb": max(as_list(s[4].get("slot_tree_bytes_max", 0)) or [0]) / 1024,
        "f_a_max_kb": max(as_list(s[4].get("affinity_a_max_bytes", 0)) or [0]) / 1024,
        "f_ring_kb": total(s[4].get("appender_ring_bytes", 0)) / 1024,
        "f_ckpt_create": d(1, 2, "meta_kv_checkpoints"),
        "f_free_ext": total(s[4].get("meta_kv_free_extents", 0)),
    })

if not rows:
    print("no rows collected", file=sys.stderr)
    sys.exit(1)

MED = [
    ("mount_s", "mount_s", "{:.2f}"),
    ("remount_open_s", "reopen_s", "{:.2f}"),
    ("rss_mount_mb", "rssMnt_MB", "{:.0f}"),
    ("anon_mount_mb", "anonMnt_MB", "{:.0f}"),
    ("rss_work_mb", "rssWrk_MB", "{:.0f}"),
    ("anon_work_mb", "anonWrk_MB", "{:.0f}"),
    ("thr", "thr", "{:.0f}"),
    ("idle_cpu_pct", "idleCPU_%core", "{:.2f}"),
    ("widle_cpu_pct", "wIdleCPU_%core", "{:.2f}"),
    ("idle_ckpt_s", "ckptIdle/s", "{:.2f}"),
    ("widle_ckpt_s", "ckptWIdle/s", "{:.2f}"),
    ("idle_jrnl_s", "jrnlIdle/s", "{:.2f}"),
    ("work_cpu_s", "createCPU_s", "{:.2f}"),
    ("create_ops_s", "create_ops/s", "{:.0f}"),
    ("stat_warm_ops_s", "statW_ops/s", "{:.0f}"),
    ("stat_cold_ops_s", "statC_ops/s", "{:.0f}"),
    ("nc_mb", "nodeCache_MB", "{:.1f}"),
    ("nc_b_per_file", "nc_B/file", "{:.0f}"),
    ("nc_evict", "ncEvict", "{:.0f}"),
    ("nc_sheds", "ncSheds", "{:.0f}"),
    ("cold_miss", "coldMisses", "{:.0f}"),
    ("r5_lvl", "r5lvl", "{:.0f}"),
    ("r5_yellow", "r5yellow", "{:.0f}"),
    ("r5_red", "r5red", "{:.0f}"),
    ("r5_backstop", "r5backstop", "{:.0f}"),
    ("pervol_target_mb", "ncTarget/vol_MB", "{:.0f}"),
    ("agg_target_gb", "ncTargetAgg_GB", "{:.1f}"),
    ("agg_over_budget", "aggXbudget", "{:.2f}"),
    ("j_max", "jrnlMax", "{:.0f}"),
    ("j_mean", "jrnlMean", "{:.0f}"),
    ("j_maxmean", "jrnlMax/Mean", "{:.1f}"),
    ("j_moved", "volsMoved", "{:.0f}"),
    ("j_carrying", "volsCarrying", "{:.0f}"),
    ("hr_cold", "ncHitCold_%", "{:.2f}"),
    ("hr_warm", "ncHitWarm_%", "{:.2f}"),
    ("f_slot_trees", "symSlotTrees", "{:.0f}"),
    ("f_tree_p99_kb", "symTreeP99_KB", "{:.0f}"),
    ("f_tree_max_kb", "symTreeMax_KB", "{:.0f}"),
    ("f_a_max_kb", "symAmax_KB", "{:.0f}"),
    ("f_ring_kb", "symRing_KB", "{:.0f}"),
    ("f_ckpt_create", "symCkptCreate", "{:.0f}"),
    ("f_free_ext", "freeExtents", "{:.0f}"),
]

ns = sorted({r["n"] for r in rows})
reps = max(r["rep"] for r in rows)
head = rows[0]
print(f"== pv-volume-scaling (substrate={head['substrate']}, budget={head['budget_lbl']}, "
      f"node_cache_mb={head['nc_lbl']}, files={head['files']}, "
      f"repeats={reps}, layout={head['layout']}, medians) ==")
print(f"   R5 budget resolved: {head['budget_gb']:.1f} GiB")
cols = ["N"] + [c for _, c, _ in MED]
table = []
for n in ns:
    sel = [r for r in rows if r["n"] == n]
    row = [str(n)]
    for key, _, fmt in MED:
        vals = [r[key] for r in sel]
        row.append(fmt.format(statistics.median(vals)))
    table.append(row)
w = {i: max(len(cols[i]), max(len(r[i]) for r in table)) for i in range(len(cols))}
print("  ".join(cols[i].ljust(w[i]) for i in range(len(cols))))
for r in table:
    print("  ".join(r[i].ljust(w[i]) for i in range(len(cols))))

print()
print("TIERS: memory rows (RSS/anon, R5, node-cache) are substrate-independent")
print("       and measured-real on this venue; the CPU and journal-balance")
print("       rows are barrier-sensitive on a file substrate — re-run under")
print("       root on tests/dev_substrate.sh before citing them as acceptance.")
print("       ncTarget*/aggXbudget are arithmetic-on-measured-constants")
print("       (backend::resolve_node_cache_budget over the LIVE mem_budget_bytes).")

if violations:
    print("INVALID ROW(S):", file=sys.stderr)
    for v in violations:
        print(f"  {v}", file=sys.stderr)
    sys.exit(1)
print("rows VALID (set width asserted, phases engaged, solo re-gate held, tripwires flat)")
PYEOF
}

leg_pv_volume_scaling() {
    local fixture="$REPO/tests/pv_volume_set.sh"
    [ -f "$fixture" ] || die "missing $fixture (the N-volume fixture)"
    [ -x "$SQZ" ] || die "missing $SQZ (cargo build --release)"
    command -v python3 >/dev/null 2>&1 || die "python3 is required (the row emitter)"
    command -v cc >/dev/null 2>&1 || die "cc is required (tests/mdstorm.c — the workload)"
    [[ "$PV_FILES" =~ ^[0-9]+$ ]] && [ "$PV_FILES" -ge 100 ] ||
        die "--files takes an integer >= 100 (got '$PV_FILES')"
    [[ "$PV_IDLE_S" =~ ^[0-9]+$ ]] && [ "$PV_IDLE_S" -ge 5 ] ||
        die "--idle-secs takes seconds >= 5 (got '$PV_IDLE_S')"
    [[ "$PV_REPEATS" =~ ^[0-9]+$ ]] && [ "$PV_REPEATS" -ge 1 ] ||
        die "--repeats takes a positive integer (got '$PV_REPEATS')"
    [[ "$PV_THREADS" =~ ^[0-9]+$ ]] && [ "$PV_THREADS" -ge 1 ] ||
        die "--threads takes a positive integer (got '$PV_THREADS')"
    [[ "$PV_TTL_MS" =~ ^[0-9]+$ ]] ||
        die "--kernel-ttl-ms takes milliseconds (got '$PV_TTL_MS')"
    local -a ns=()
    IFS=, read -r -a ns <<<"$PV_NS"
    local n
    for n in "${ns[@]}"; do
        [[ "$n" =~ ^[0-9]+$ ]] && [ "$n" -ge 1 ] && [ "$n" -le 256 ] ||
            die "--ns takes comma-separated widths in 1..256 (got '$n')"
    done

    PV_ROOT="${SQZ_MWMATRIX_PV_ROOT:-${TMPDIR:-/tmp}/squeezefs-pvscale}"
    mkdir -p "$PV_ROOT"
    local rowdir
    rowdir="$PV_ROOT/rows-$PV_TAG-$(date +%s)"
    mkdir -p "$rowdir"
    local storm="$REPO/target/pv-mdstorm"
    cc -O2 -pthread -o "$storm" "$REPO/tests/mdstorm.c" ||
        die "cc tests/mdstorm.c failed"

    log "pv-volume-scaling: ns=$PV_NS files=$PV_FILES idle=${PV_IDLE_S}s repeats=$PV_REPEATS budget=${PV_BUDGET:-derived} node_cache_mb=${PV_NODE_CACHE_MB:-derived} kernel_ttl_ms=$PV_TTL_MS"
    local rep
    for rep in $(seq 1 "$PV_REPEATS"); do
        for n in "${ns[@]}"; do
            pv_quiet_or_die
            log "row: N=$n rep=$rep"
            pv_one_row "$n" "$rep" "$rowdir" "$storm" "$fixture"
        done
    done
    log "snapshots: $rowdir"
    pv_emit "$rowdir"
}

if [ "$LEG" = "pv-volume-scaling" ]; then
    leg_pv_volume_scaling
    exit 0
fi

ensure_root "$LEG" "$@"
# The admin-lane client half of the KD-7 dev override (the daemon half is
# mw_fleet.sh's mount env): dev-tree `-dirty` identities are degenerate,
# and the s7-kill-matrix's online-fsck oracle rides the admin lane.
export SQUEEZEFS_IPC_ALLOW_DEV=1
command -v python3 >/dev/null 2>&1 || die "python3 is required (the row emitter)"

[ -f "$CONF" ] || die "no live fleet at $STATE — run: sudo tests/mw_fleet.sh create N=2"
# shellcheck disable=SC1090 # generated by mw_fleet.sh create
. "$CONF"
HOST_SCOPED="$(cat "$STATE/host_scoped" 2>/dev/null || echo 0)"

mnt_of() {
    awk -F'\t' -v i="$1" '$1==i {print $3}' "$MEMBERS"
}
role_of() {
    awk -F'\t' -v i="$1" '$1==i {print $2}' "$MEMBERS"
}
member_idxs() {
    awk -F'\t' '{print $1}' "$MEMBERS" | sort -n
}

snap() { # idx phase rowdir  — cat, never cp (the aging trap)
    local mnt
    mnt="$(mnt_of "$1")"
    cat "$mnt/.stats" >"$3/m$1_p$2.json" ||
        die "cannot snapshot member $1's stats inode"
}

stat_field() { # idx json_key -> value (flattened key)
    local mnt
    mnt="$(mnt_of "$1")"
    cat "$mnt/.stats" | python3 -c '
import json, sys
def flat(d, out, pfx=""):
    for k, v in d.items():
        if isinstance(v, dict): flat(v, out, pfx + k + ".")
        else: out[pfx + k] = v
    return out
root = json.load(sys.stdin)
d = flat(root.get("metrics", root), {})  # stats nest under "metrics"
print(d.get(sys.argv[1], ""))' "$2"
}

# A per-volume gauge (a JSON array) folded: the SUM of its numeric
# elements (a scalar is its own sum). The symmetric families publish per
# volume (PR 2's `appender_*`, PR 4's `symmetric_meta`).
stat_sum() { # idx json_key -> sum
    local mnt
    mnt="$(mnt_of "$1")"
    cat "$mnt/.stats" | python3 -c '
import json, sys
def flat(d, out, pfx=""):
    for k, v in d.items():
        if isinstance(v, dict): flat(v, out, pfx + k + ".")
        else: out[pfx + k] = v
    return out
root = json.load(sys.stdin)
v = flat(root.get("metrics", root), {}).get(sys.argv[1], 0)
if isinstance(v, list):
    print(sum(x for x in v if isinstance(x, (int, float))))
elif isinstance(v, (int, float)):
    print(v)
else:
    print(0)' "$2"
}

# Every element of a per-volume gauge equals `want` (a scalar compared
# directly); prints 1/0.
stat_all_eq() { # idx json_key want -> 1|0
    local mnt
    mnt="$(mnt_of "$1")"
    cat "$mnt/.stats" | python3 -c '
import json, sys
def flat(d, out, pfx=""):
    for k, v in d.items():
        if isinstance(v, dict): flat(v, out, pfx + k + ".")
        else: out[pfx + k] = v
    return out
root = json.load(sys.stdin)
v = flat(root.get("metrics", root), {}).get(sys.argv[1], None)
want = sys.argv[2]
vals = v if isinstance(v, list) else [v]
print(1 if vals and all(str(x) == want for x in vals) else 0)' "$2" "$3"
}

# --- the row emitter ---------------------------------------------------------
# Per-mount deltas + the §5.5 mandatory columns; exits nonzero on any
# INVALID row (missing engagement, R5 tripwire movement, dlm_rpcs != 0).
emit_rows() { # rowdir leg idx...
    local rowdir="$1" leg="$2"
    shift 2
    python3 - "$rowdir" "$leg" "$@" <<'PYEOF'
import json, sys

rowdir, leg, idxs = sys.argv[1], sys.argv[2], sys.argv[3:]

def flat(d, out=None, pfx=""):
    out = {} if out is None else out
    for k, v in d.items():
        if isinstance(v, dict):
            flat(v, out, pfx + k + ".")
        else:
            out[pfx + k] = v
    return out

def num(v):
    return v if isinstance(v, (int, float)) else 0

# The §5.5 mandatory columns. Engagement: this rung's fleet runs every
# distributed plane dark by construction, so the dlm_*/meta_ship_*/
# membership_* columns are asserted-zero rows, not omitted rows.
DELTA_COLS = [
    ("meta_kv_journal_entries", "jrnl_d"),
    ("write_through_bytes", "wt_bytes_d"),
    ("meta_kv_revalidate_polls", "reval_polls_d"),
    ("meta_kv_revalidate_epochs", "reval_epochs_d"),
    ("meta_kv_revalidate_dirty_skips", "reval_dirty_d"),
    ("membership_renewals", "memb_renew_d"),
    ("membership_registration_commits", "memb_reg_d"),
    ("membership_self_fences", "memb_fence_d"),
    ("membership_evictions", "memb_evict_d"),
    ("membership_census_serves", "memb_census_d"),
    ("meta_ship.shipped_verbs", "ship_d"),
    ("mem_budget_red_events", "r5_red_d"),
    ("mem_budget_hard_backstops", "r5_backstop_d"),
    ("parked_gate_timeouts", "r5_gate_to_d"),
    ("invariant_tripwires", "tripwire_d"),
]
GAUGE_COLS = [
    ("mount_posture", "posture"),
    ("client_slot", "slot"),
    ("dlm_rpcs", "dlm_rpcs"),
    ("dlm_mode", "dlm_mode"),
    ("membership_mode", "memb_mode"),
    ("mem_budget_level", "r5_lvl"),
    ("reader_staleness_bound_ms", "stale_ms"),
]

def load(path):  # the stats JSON nests under a top-level "metrics" object
    root = json.load(open(path))
    return flat(root.get("metrics", root))

rows, violations, slots = [], [], {}
for i in idxs:
    p0 = load(f"{rowdir}/m{i}_p0.json")
    p1 = load(f"{rowdir}/m{i}_p1.json")
    row = {"m": i}
    for key, col in DELTA_COLS:
        row[col] = num(p1.get(key, 0)) - num(p0.get(key, 0))
    for key, col in GAUGE_COLS:
        row[col] = p1.get(key, "-")
    rows.append(row)
    slots[i] = row["slot"]

    # Row validity (§5.5): R5 tripwires + the dark-plane invariants.
    if row["r5_backstop_d"] != 0 or num(p1.get("mem_budget_hard_backstops", 0)) != 0:
        violations.append(f"m{i}: mem_budget_hard_backstops moved (R5 column)")
    if row["r5_gate_to_d"] != 0:
        violations.append(f"m{i}: parked_gate_timeouts moved (R5 column)")
    if num(p1.get("dlm_rpcs", 0)) != 0:
        violations.append(f"m{i}: dlm_rpcs != 0 (solo-invariant violated)")
    if row["tripwire_d"] != 0:
        violations.append(f"m{i}: invariant_tripwires moved")
    if num(p1.get("meta_kv_revalidate_dirty_skips", 0)) != 0:
        violations.append(
            f"m{i}: meta_kv_revalidate_dirty_skips != 0 — the FIXED rung-6 "
            "pinned-node finding regressed (must stay 0 on every posture; "
            "cargo pin readonly_mount_tests::"
            "reader_bootstrap_into_a_dirty_journal_tail_never_pins_nodes)"
        )
    posture = p1.get("mount_posture", "?")
    if posture == "writer" and row["jrnl_d"] <= 0:
        violations.append(f"m{i}: writer emitted no journal entries (row not engaged)")
    if posture == "reader" and row["reval_polls_d"] <= 0:
        violations.append(f"m{i}: reader revalidation never polled (row not engaged)")

if len(set(slots.values())) != len(slots):
    violations.append(f"client slots not distinct: {slots}")

cols = ["m"] + [c for _, c in GAUGE_COLS] + [c for _, c in DELTA_COLS]
widths = {c: max(len(c), max((len(str(r[c])) for r in rows), default=0)) for c in cols}
print(f"== {leg} rows (deltas p0->p1; engagement + R5-pressure columns) ==")
print("  ".join(c.ljust(widths[c]) for c in cols))
for r in rows:
    print("  ".join(str(r[c]).ljust(widths[c]) for c in cols))

if violations:
    print("INVALID ROW(S):", file=sys.stderr)
    for v in violations:
        print(f"  {v}", file=sys.stderr)
    sys.exit(1)
print("rows VALID (R5 tripwires flat, dark planes at 0, per-role engagement present)")
PYEOF
}

# --- legs --------------------------------------------------------------------
leg_smoke() {
    local rowdir
    rowdir="$STATE/rows/smoke-$(date +%s)"
    mkdir -p "$rowdir"
    local w_mnt r_idx r_mnt
    w_mnt="$(mnt_of 0)"
    [ -n "$w_mnt" ] || die "no writer member"
    [ "$(role_of 0)" = "writer" ] || die "member 0 is not the writer"
    r_idx="$(member_idxs | awk '$1!=0' | head -1)"
    [ -n "$r_idx" ] || die "smoke needs N>=2 (a reader member)"
    r_mnt="$(mnt_of "$r_idx")"

    # Identity/posture assertions — writer via `squeezefs clients` (the D0
    # claim + heartbeat record), reader via its OWN stats inode (S5: readers
    # are invisible to `clients` until S6 arms — stated in the header).
    local meta0 clients_out
    meta0="${FORMAT_META_PATHS%%,*}"
    : "$meta0" # clients probes the CURRENT writer-identity paths:
    clients_out="$("$SQZ" clients "sqmeta://$META_PATHS" 2>/dev/null)" ||
        die "squeezefs clients probe failed"
    echo "$clients_out" >"$rowdir/clients.out"
    echo "$clients_out" | grep -q "live" ||
        die "writer not visible live in squeezefs clients:
$clients_out"
    [ "$(stat_field 0 mount_posture)" = "writer" ] || die "member 0 posture != writer"
    [ "$(stat_field "$r_idx" mount_posture)" = "reader" ] ||
        die "member $r_idx posture != reader"
    [ "$(stat_field "$r_idx" read_only_mount)" = "True" ] ||
        die "member $r_idx is not a read-only mount"
    local w_slot r_slot
    w_slot="$(stat_field 0 client_slot)"
    r_slot="$(stat_field "$r_idx" client_slot)"
    [ -n "$w_slot" ] && [ -n "$r_slot" ] && [ "$w_slot" != "$r_slot" ] ||
        die "client slots not distinct (writer=$w_slot reader=$r_slot)"
    log "identities: writer slot=$w_slot (clients: live), reader slot=$r_slot (stats: posture=reader, ro=true)"

    local bound_ms
    bound_ms="$(stat_field "$r_idx" reader_staleness_bound_ms)"
    [[ "$bound_ms" =~ ^[0-9]+$ ]] && [ "$bound_ms" -gt 0 ] ||
        die "reader publishes no staleness bound (got '$bound_ms')"

    # p0 → writer I/O → reader coherence within the published bound → p1.
    local i
    for i in $(member_idxs); do snap "$i" 0 "$rowdir"; done

    mkdir -p "$w_mnt/mwsmoke"
    dd if=/dev/urandom of="$w_mnt/mwsmoke/coh.dat" bs=64K count=32 conv=fsync \
        status=none || die "writer I/O failed"
    local want_sum t0 now deadline got_sum="" observed_ms=-1
    want_sum="$(sha256sum "$w_mnt/mwsmoke/coh.dat" | awk '{print $1}')"
    t0="$(date +%s%3N)"
    # Deadline: the published bound + the reader's 1 s kernel attr/entry TTL
    # + grace. Exceeding it is a FAILED row, not a retry.
    deadline=$((bound_ms + 1000 + 5000))
    while :; do
        now="$(date +%s%3N)"
        if [ -f "$r_mnt/mwsmoke/coh.dat" ]; then
            got_sum="$(sha256sum "$r_mnt/mwsmoke/coh.dat" 2>/dev/null | awk '{print $1}')" || got_sum=""
            if [ "$got_sum" = "$want_sum" ]; then
                observed_ms=$((now - t0))
                break
            fi
        fi
        [ $((now - t0)) -lt "$deadline" ] ||
            die "reader did not observe the writer's data within ${deadline}ms (published staleness bound ${bound_ms}ms + TTL + grace) — coherence FAILED"
        sleep 0.2
    done
    log "coherence: reader observed the write in ${observed_ms}ms (published bound ${bound_ms}ms + 1000ms kernel TTL; sha256 match)"

    # Let the reader's revalidation cadence tick at least once more so the
    # engagement column is unambiguous, then p1.
    sleep 2
    for i in $(member_idxs); do snap "$i" 1 "$rowdir"; done
    # shellcheck disable=SC2046 # member_idxs is a controlled numeric list
    emit_rows "$rowdir" smoke $(member_idxs)
    log "smoke leg GREEN (rows + snapshots preserved in $rowdir)"
}

leg_multipath_negative() {
    if [ "$HOST_SCOPED" = "1" ]; then
        skip "this kernel scopes fabric subsystems by host identity (rung 5b present) — the merged-head refusal shape does not exist here; the 5b kernel's own validation legs live in rung 6b"
    fi
    local rowdir
    rowdir="$STATE/rows/mpneg-$(date +%s)"
    mkdir -p "$rowdir"
    local hostid hostnqn mnt out rc=0
    hostid="$(printf 'cafef1e7-%04d-4000-8000-%012d' 91 "$CREATE_PID")"
    hostnqn="nqn.2014-08.org.nvmexpress:uuid:$hostid"
    mnt="$(mktemp -d /tmp/sqz-mwneg-XXXXXX)"
    # The merged head: the meta paths are subsystem head nodes served by the
    # WRITER's identity (the create-time probe recorded that a second
    # identity MERGES rather than getting its own subsystem). A second
    # explicit identity's mount attempt must refuse with the rule-2 class.
    out="$(timeout 120 "$SQZ" mount "sqmeta://$META_PATHS" "$mnt" \
        -o "hostnqn=$hostnqn,hostid=$hostid" 2>&1)" || rc=$?
    echo "$out" >"$rowdir/refusal.out"
    if [ "$rc" -eq 0 ] || mountpoint -q "$mnt"; then
        "$SQZ" umount "$mnt" >/dev/null 2>&1 || umount -l "$mnt" 2>/dev/null || true
        rmdir "$mnt" 2>/dev/null || true
        die "a second explicit identity MOUNTED on the merged head — the rule-2 refusal did not fire"
    fi
    # Loose pin: the refusal CLASS + rule number (rung 5b part 3 upgrades
    # the message text to name the shape + remedies — do not pin bytes).
    if ! echo "$out" | grep -Eq 'mount refused \(rule 2'; then
        rmdir "$mnt" 2>/dev/null || true
        die "mount refused, but not with the rule-2 class:
$out"
    fi
    rmdir "$mnt" 2>/dev/null || true
    log "second explicit identity refused with the rule-2 class (refusal preserved in $rowdir/refusal.out)"
    log "multipath-negative leg GREEN"
}

# --- rung-6b guest legs --------------------------------------------------
MWFLEET="$REPO/tests/mw_fleet.sh"

require_vm_fleet() {
    [ "${VM_COUNT:-0}" -ge 1 ] ||
        die "this leg needs a fleet created with --vm=V (sudo tests/mw_fleet.sh create N=2 --vm=1) — the 0030 kernel boots only in the qemu guest"
    [ -n "${GUEST_META_NQN:-}" ] || die "fleet config carries no reserved guest-leg meta NQN"
}

guest_id() { printf 'cafef1e7-%04d-4000-8000-%012d' "$1" "$CREATE_PID"; }
guest_nqn() { echo "nqn.2014-08.org.nvmexpress:uuid:$(guest_id "$1")"; }

# The busybox-sh helper preamble every in-guest job shares: the scoped-
# subsystem walk (head + controller-link census) in shell.
guest_job_preamble() {
    cat <<PREAMBLE
set -e
export LD_LIBRARY_PATH=/share/lib
SQZ=/share/bin/squeezefs
GW='$VM_GW'
SVC='$TCP_SVC'
subsys_dirs_for_nqn() { # nqn -> subsystem dir paths
    for s in /sys/class/nvme-subsystem/nvme-subsys*; do
        [ -r "\$s/subsysnqn" ] || continue
        [ "\$(cat "\$s/subsysnqn")" = "\$1" ] && echo "\$s"
    done
}
head_of_dir() { # subsystem dir -> head name (strict nvme<X>n<Y>)
    for c in "\$1"/nvme*; do
        b=\$(basename "\$c")
        echo "\$b" | grep -qE '^nvme[0-9]+n[0-9]+\$' && { echo "\$b"; return 0; }
    done
    return 1
}
ctrl_links_of_dir() { # subsystem dir -> "ctrl:hostnqn" lines
    for c in "\$1"/nvme*; do
        b=\$(basename "\$c")
        echo "\$b" | grep -qE '^nvme[0-9]+\$' || continue
        echo "\$b:\$(cat "\$c/hostnqn" 2>/dev/null)"
    done
}
head_for_scope() { # nqn hostnqn-scope -> head name
    for s in \$(subsys_dirs_for_nqn "\$1"); do
        [ "\$(cat "\$s/sqz_host_scope" 2>/dev/null)" = "\$2" ] || continue
        head_of_dir "\$s" && return 0
    done
    return 1
}
disconnect_nqn() { # nqn — delete every controller serving it
    for c in /sys/class/nvme/nvme*; do
        [ "\$(cat "\$c/subsysnqn" 2>/dev/null)" = "\$1" ] || continue
        echo 1 >"\$c/delete_controller" 2>/dev/null || true
    done
    sleep 1
}
PREAMBLE
}

leg_vm_hostscope_validate() {
    require_vm_fleet
    local rowdir a_nqn a_id b_nqn b_id
    rowdir="$STATE/rows/vmhs-$(date +%s)"
    mkdir -p "$rowdir"
    a_nqn="$(guest_nqn 80)" a_id="$(guest_id 80)"
    b_nqn="$(guest_nqn 81)" b_id="$(guest_id 81)"

    # ---------------- POSITIVE arm: fleet guest 0 (param=Y) ----------------
    log "positive arm: 0030 grouping proof in fleet guest 0 (param=Y)"
    {
        guest_job_preamble
        cat <<POS
P=/sys/module/nvme_core/parameters/fabrics_host_scoped_subsystems
[ -r "\$P" ] || { echo "FAIL: 0030 module param file absent — not the patched kernel"; exit 1; }
echo "param fabrics_host_scoped_subsystems=\$(cat \$P)"
[ "\$(cat \$P)" = "Y" ] || { echo "FAIL: param not Y on the fleet guest"; exit 1; }
# The rung-6 5b probe's PARAM FACE (mw_fleet.sh probe_host_scoped arm a),
# verbatim glob — must answer host-scoped=true in-guest:
probe=0
for f in /sys/module/nvme_core/parameters/*host*scope* /sys/module/nvme_core/parameters/*scope*host*; do
    [ -r "\$f" ] || continue
    case "\$(cat "\$f")" in Y|y|1) probe=1 ;; esac
done
echo "5b-probe-param-face: host_scoped=\$probe"
[ "\$probe" = 1 ] || { echo "FAIL: the 5b probe would not unlock multi-identity legs here"; exit 1; }
NQN='$GUEST_META_NQN'
\$SQZ nvmeof connect --ip "\$GW" --port "\$SVC" --subnqn "\$NQN" --hostnqn '$a_nqn' --hostid '$a_id'
\$SQZ nvmeof connect --ip "\$GW" --port "\$SVC" --subnqn "\$NQN" --hostnqn '$b_nqn' --hostid '$b_id'
sleep 1
echo "=== scoped census (two identities, one subnqn) ==="
count=0
scopes=""
for s in \$(subsys_dirs_for_nqn "\$NQN"); do
    count=\$((count + 1))
    scope=\$(cat "\$s/sqz_host_scope" 2>/dev/null)
    head=\$(head_of_dir "\$s") || { echo "FAIL: subsystem \$s has no openable head"; exit 1; }
    [ -b "/dev/\$head" ] || { echo "FAIL: /dev/\$head is not a block device"; exit 1; }
    heads_n=0
    for c in "\$s"/nvme*; do b=\$(basename "\$c"); echo "\$b" | grep -qE '^nvme[0-9]+n[0-9]+\$' && heads_n=\$((heads_n + 1)); done
    [ "\$heads_n" = 1 ] || { echo "FAIL: subsystem \$s carries \$heads_n heads (want 1)"; exit 1; }
    links=\$(ctrl_links_of_dir "\$s")
    echo "SUBSYS \$(basename "\$s") scope=\$scope head=\$head ctrls: \$links"
    [ -n "\$links" ] || { echo "FAIL: subsystem \$s carries no controller links"; exit 1; }
    for l in \$links; do
        [ "\${l#*:}" = "\$scope" ] || { echo "FAIL: controller \$l under scope \$scope — the dir is NOT identity-dedicated"; exit 1; }
    done
    scopes="\$scopes \$scope"
done
echo "subsys_count=\$count scopes=\$scopes"
[ "\$count" = 2 ] || { echo "FAIL: want TWO host-scoped sibling subsystems, got \$count"; exit 1; }
echo "\$scopes" | grep -q '$a_nqn' || { echo "FAIL: identity A's scope missing"; exit 1; }
echo "\$scopes" | grep -q '$b_nqn' || { echo "FAIL: identity B's scope missing"; exit 1; }
if dmesg | grep -i "duplicate IDs"; then
    echo "FAIL: the kernel refused a scoped sibling's namespace as a duplicate ID (the 0030 dup-ID skip did not engage)"
    exit 1
fi
echo "=== same-identity multipath preservation (duplicate_connect) ==="
printf 'transport=tcp,traddr=%s,trsvcid=%s,nqn=%s,hostnqn=%s,hostid=%s,duplicate_connect' \
    "\$GW" "\$SVC" "\$NQN" '$a_nqn' '$a_id' >/dev/nvme-fabrics
sleep 1
count2=0
for s in \$(subsys_dirs_for_nqn "\$NQN"); do count2=\$((count2 + 1)); done
[ "\$count2" = 2 ] || { echo "FAIL: same-identity second path minted a THIRD subsystem (\$count2)"; exit 1; }
a_ctrls=0
for s in \$(subsys_dirs_for_nqn "\$NQN"); do
    [ "\$(cat "\$s/sqz_host_scope" 2>/dev/null)" = '$a_nqn' ] || continue
    for l in \$(ctrl_links_of_dir "\$s"); do a_ctrls=\$((a_ctrls + 1)); done
    heads_n=0
    for c in "\$s"/nvme*; do b=\$(basename "\$c"); echo "\$b" | grep -qE '^nvme[0-9]+n[0-9]+\$' && heads_n=\$((heads_n + 1)); done
    [ "\$heads_n" = 1 ] || { echo "FAIL: A's subsystem grew a second head"; exit 1; }
done
[ "\$a_ctrls" = 2 ] || { echo "FAIL: A's subsystem carries \$a_ctrls controller links (want 2 — N paths, one identity, ONE subsystem)"; exit 1; }
echo "same-identity multipath preserved: 2 paths, 1 subsystem, 1 head"
disconnect_nqn "\$NQN"
echo "POSITIVE ARM GREEN"
POS
    } >"$rowdir/pos-arm.sh"
    "$MWFLEET" vm-exec 0 "$rowdir/pos-arm.sh" 420 | tee "$rowdir/pos-arm.out" ||
        die "positive arm FAILED (output: $rowdir/pos-arm.out)"

    # ------------- NEGATIVE arm: ephemeral param-OFF guest (idx 90) --------
    log "negative arm: param-off merge control on ephemeral guest 90"
    "$MWFLEET" vm-boot 90 --no-hostscope
    {
        guest_job_preamble
        cat <<NEG
P=/sys/module/nvme_core/parameters/fabrics_host_scoped_subsystems
[ -r "\$P" ] || { echo "FAIL: param file absent — not the patched kernel"; exit 1; }
echo "param fabrics_host_scoped_subsystems=\$(cat \$P)"
[ "\$(cat \$P)" = "N" ] || { echo "FAIL: negative arm expects the param OFF"; exit 1; }
NQN='$GUEST_META_NQN'
\$SQZ nvmeof connect --ip "\$GW" --port "\$SVC" --subnqn "\$NQN" --hostnqn '$a_nqn' --hostid '$a_id'
\$SQZ nvmeof connect --ip "\$GW" --port "\$SVC" --subnqn "\$NQN" --hostnqn '$b_nqn' --hostid '$b_id'
sleep 1
echo "=== merged census (param off) ==="
count=0
merged_dir=""
for s in \$(subsys_dirs_for_nqn "\$NQN"); do
    count=\$((count + 1))
    merged_dir="\$s"
    echo "SUBSYS \$(basename "\$s") scope='\$(cat "\$s/sqz_host_scope" 2>/dev/null)' ctrls: \$(ctrl_links_of_dir "\$s")"
done
[ "\$count" = 1 ] || { echo "FAIL: param-off control expects ONE merged subsystem, got \$count"; exit 1; }
[ -z "\$(cat "\$merged_dir/sqz_host_scope" 2>/dev/null)" ] || { echo "FAIL: scope not empty with the param off"; exit 1; }
ctrl_links_of_dir "\$merged_dir" | grep -q '$a_nqn' || { echo "FAIL: A's controller missing from the merged dir"; exit 1; }
ctrl_links_of_dir "\$merged_dir" | grep -q '$b_nqn' || { echo "FAIL: B's controller missing from the merged dir"; exit 1; }
head=\$(head_of_dir "\$merged_dir") || { echo "FAIL: merged subsystem has no head"; exit 1; }
echo "merged shape reproduced: 1 subsystem, head \$head, 2 hostnqns"
echo "=== upgraded rule-2 refusal over the merged head ==="
# The mount reads the format config BEFORE the identity ladder — format
# the reserved pair first so the probe reaches the ladder (offline
# format over the merged head is fine; no identity in play).
# GUEST_DATA_NQN is a HOST-side variable, interpolated at guest-script
# generation time (the unescaped dollar in this expanding heredoc) —
# not a misspelling of GUEST_META_NQN (SC2153 disabled file-wide;
# directives cannot reach heredoc bodies).
DNQN='$GUEST_DATA_NQN'
\$SQZ nvmeof connect --ip "\$GW" --port "\$SVC" --subnqn "\$DNQN" --hostnqn '$a_nqn' --hostid '$a_id'
sleep 1
dhead=""
for s in \$(subsys_dirs_for_nqn "\$DNQN"); do dhead=\$(head_of_dir "\$s") && break; done
[ -n "\$dhead" ] || { echo "FAIL: no head for the reserved data NQN"; exit 1; }
\$SQZ format "sqmeta:///dev/\$head" "sqdata:///dev/\$dhead" --force >/tmp/format.out 2>&1 || { cat /tmp/format.out; exit 1; }
mkdir -p /mnt/neg
rc=0
timeout 90 \$SQZ mount "sqmeta:///dev/\$head" /mnt/neg -o 'hostnqn=$a_nqn,hostid=$a_id' >/tmp/refusal.out 2>&1 || rc=\$?
cat /tmp/refusal.out
grep -q " /mnt/neg " /proc/mounts && { echo "FAIL: mounted on the merged head"; exit 1; }
[ "\$rc" != 0 ] || { echo "FAIL: mount exited 0"; exit 1; }
grep -q 'MULTIPATH-MERGED' /tmp/refusal.out || { echo "FAIL: refusal does not name the shape"; exit 1; }
grep -q 'fabrics_host_scoped_subsystems=Y' /tmp/refusal.out || { echo "FAIL: refusal does not name the sqz-kernel remedy"; exit 1; }
grep -q 'multipath=N' /tmp/refusal.out || { echo "FAIL: refusal does not name the stock workaround"; exit 1; }
disconnect_nqn "\$NQN"
disconnect_nqn "\$DNQN"
echo "NEGATIVE ARM GREEN"
NEG
    } >"$rowdir/neg-arm.sh"
    local neg_rc=0
    "$MWFLEET" vm-exec 90 "$rowdir/neg-arm.sh" 420 | tee "$rowdir/neg-arm.out" || neg_rc=$?
    "$MWFLEET" vm-stop 90
    [ "$neg_rc" = 0 ] || die "negative arm FAILED (output: $rowdir/neg-arm.out)"
    log "vm-hostscope-validate GREEN (both arms; evidence in $rowdir)"
}

leg_vm_multi_identity() {
    require_vm_fleet
    local rowdir a_nqn a_id b_nqn b_id
    rowdir="$STATE/rows/vmmid-$(date +%s)"
    mkdir -p "$rowdir"
    a_nqn="$(guest_nqn 85)" a_id="$(guest_id 85)"
    b_nqn="$(guest_nqn 86)" b_id="$(guest_id 86)"

    # ---- job 1: writer A — the full explicit-identity mount, in-guest ----
    {
        guest_job_preamble
        cat <<JOBA
P=/sys/module/nvme_core/parameters/fabrics_host_scoped_subsystems
[ "\$(cat \$P 2>/dev/null)" = "Y" ] || { echo "FAIL: this leg needs the 0030 kernel armed"; exit 1; }
MNQN='$GUEST_META_NQN'
DNQN='$GUEST_DATA_NQN'
\$SQZ nvmeof connect --ip "\$GW" --port "\$SVC" --subnqn "\$MNQN" --hostnqn '$a_nqn' --hostid '$a_id'
\$SQZ nvmeof connect --ip "\$GW" --port "\$SVC" --subnqn "\$DNQN" --hostnqn '$a_nqn' --hostid '$a_id'
sleep 1
MH=\$(head_for_scope "\$MNQN" '$a_nqn') || { echo "FAIL: no scoped meta head for A"; exit 1; }
DH=\$(head_for_scope "\$DNQN" '$a_nqn') || { echo "FAIL: no scoped data head for A"; exit 1; }
echo "A heads: meta=/dev/\$MH data=/dev/\$DH"
\$SQZ format "sqmeta:///dev/\$MH" "sqdata:///dev/\$DH" --force >/tmp/format.out 2>&1 || { cat /tmp/format.out; exit 1; }
VOL=\$(\$SQZ volume list "sqmeta:///dev/\$MH" | awk -v b="/dev/\$DH" 'NR>1 && \$NF==b {print \$1}')
[ -n "\$VOL" ] || { echo "FAIL: no durable volume id for /dev/\$DH"; \$SQZ volume list "sqmeta:///dev/\$MH"; exit 1; }
# Records carry the GUEST-DOMAIN fabric address (\$GW — THE VM LEG note).
ep_ok=0
for t in 1 2 3 4 5; do
    if \$SQZ config set-fabric-endpoints "sqmeta:///dev/\$MH" "\$VOL=\$GW:\$SVC:\$DNQN" >/tmp/ep.out 2>&1; then ep_ok=1; break; fi
    grep -q "holds the writer lock" /tmp/ep.out || { cat /tmp/ep.out; exit 1; }
    sleep 2
done
[ "\$ep_ok" = 1 ] || { echo "FAIL: set-fabric-endpoints never cleared the post-format guard"; cat /tmp/ep.out; exit 1; }
# Un-pre-connect the DATA plane: the writer's daemon-owned connect is the point.
disconnect_nqn "\$DNQN"
mkdir -p /mnt/a
\$SQZ mount "sqmeta:///dev/\$MH" /mnt/a -o 'hostnqn=$a_nqn,hostid=$a_id' --daemon --log-file /tmp/a.log >/tmp/a.mount.out 2>&1 || { cat /tmp/a.mount.out; exit 1; }
i=0
while [ \$i -lt 240 ]; do grep -q " /mnt/a " /proc/mounts && break; i=\$((i + 1)); sleep 0.5; done
grep -q " /mnt/a " /proc/mounts || { echo "FAIL: writer A never mounted"; cat /tmp/a.log; exit 1; }
grep -q "daemon-owned controller resolved" /tmp/a.log || { echo "FAIL: no daemon-owned connect line (rung-2 engagement)"; exit 1; }
found=0
for c in /sys/class/nvme/nvme*; do
    [ "\$(cat "\$c/subsysnqn" 2>/dev/null)" = "\$DNQN" ] || continue
    [ "\$(cat "\$c/hostnqn" 2>/dev/null)" = '$a_nqn' ] && found=1
done
[ "\$found" = 1 ] || { echo "FAIL: no data controller under A's identity post-mount"; exit 1; }
echo mw-guest-proof >/mnt/a/proof.txt && sync
grep -q '"mount_posture": *"writer"' /mnt/a/.stats || { echo "FAIL: posture != writer"; exit 1; }
echo "A_HEAD=\$MH"
echo "WRITER A GREEN (mounted, daemon-owned data connect under A, posture=writer)"
JOBA
    } >"$rowdir/job-a.sh"
    "$MWFLEET" vm-exec 0 "$rowdir/job-a.sh" 600 | tee "$rowdir/job-a.out" ||
        die "writer-A job FAILED (output: $rowdir/job-a.out)"
    local a_head
    a_head="$(awk -F= '/^A_HEAD=/ {print $2}' "$rowdir/job-a.out" | tr -d '\r')"
    [ -n "$a_head" ] || die "writer-A job reported no head"

    # ---- job 2: writer-candidate B — past rule 2, refused beyond identity ----
    {
        guest_job_preamble
        cat <<JOBB
MNQN='$GUEST_META_NQN'
\$SQZ nvmeof connect --ip "\$GW" --port "\$SVC" --subnqn "\$MNQN" --hostnqn '$b_nqn' --hostid '$b_id'
sleep 1
BH=\$(head_for_scope "\$MNQN" '$b_nqn') || { echo "FAIL: no scoped meta head for B"; exit 1; }
echo "B head: /dev/\$BH (A's was /dev/$a_head)"
[ "\$BH" != '$a_head' ] || { echo "FAIL: B resolved A's head — identities merged"; exit 1; }
mkdir -p /mnt/b
rc=0
timeout 120 \$SQZ mount "sqmeta:///dev/\$BH" /mnt/b -o 'hostnqn=$b_nqn,hostid=$b_id' >/tmp/b.out 2>&1 || rc=\$?
echo "=== B refusal (rc=\$rc) ==="
cat /tmp/b.out
grep -q " /mnt/b " /proc/mounts && { echo "FAIL: writer-candidate B MOUNTED under a live writer"; exit 1; }
[ "\$rc" != 0 ] || { echo "FAIL: B's mount exited 0"; exit 1; }
# The rule-2 pin matches the REFUSAL class only (the KD-MW-3 engagement
# banner legitimately says "verified (rule 2)"): both the ladder's
# "mount refused (rule 2" and the daemon-connect resolution family
# ("produced no namespace under this mount's identity" / "the fabric is
# mis-sharing the subsystem") are fabric-layer blocks.
if grep -Eq 'mount refused \(rule 2|produced no namespace under this|mis-sharing the subsystem' /tmp/b.out; then
    echo "FAIL: B refused AT the fabric layer — scoped-sibling resolution regressed"
    exit 1
fi
grep -Eqi 'claimed by a live writer|holds the writer lock|single-writer' /tmp/b.out ||
    { echo "FAIL: B's refusal is not the D0 writer-guard class (beyond identity)"; exit 1; }
echo "WRITER-CANDIDATE B GREEN (own scoped head, PAST rule 2, refused at the single-writer guard)"
JOBB
    } >"$rowdir/job-b.sh"
    local b_rc=0
    "$MWFLEET" vm-exec 0 "$rowdir/job-b.sh" 600 | tee "$rowdir/job-b.out" || b_rc=$?

    # ---- job 3: cleanup (always) ----
    {
        guest_job_preamble
        cat <<JOBC
set +e
\$SQZ umount /mnt/a >/dev/null 2>&1 || umount -l /mnt/a 2>/dev/null
i=0
while [ \$i -lt 120 ]; do grep -q " /mnt/a " /proc/mounts || break; i=\$((i + 1)); sleep 0.5; done
disconnect_nqn '$GUEST_META_NQN'
disconnect_nqn '$GUEST_DATA_NQN'
echo "cleanup done"
exit 0
JOBC
    } >"$rowdir/job-cleanup.sh"
    "$MWFLEET" vm-exec 0 "$rowdir/job-cleanup.sh" 300 | tee "$rowdir/job-cleanup.out" ||
        warn "in-guest cleanup reported errors"
    [ "$b_rc" = 0 ] || die "writer-candidate-B job FAILED (output: $rowdir/job-b.out)"
    log "vm-multi-identity GREEN (evidence in $rowdir)"
}

# --- PR 13i — the TWO-HOST fixture (design-symmetric-metadata §5.12) --------
# `sym-two-host`: two KERNELS sharing one metadata LUN — the venue every
# single-box fleet (netns members included) structurally cannot be: one
# kernel is one page cache. Needs `create N=1 --symmetric --vm=1
# --vm-net=tap` (SQZ_MWGUEST_KERNEL_SRC=host boots this host's kernel in
# the guest). Two phases:
#
#   1. THE PIN (RED on a buffered-metadata binary, GREEN under O_DIRECT —
#      the F-C1 mechanism, product-verb-driven): the guest connects the
#      fleet's meta NQN under its OWN identity, a HOLDER process keeps the
#      namespace's block device OPEN (Linux drops a bdev's page cache at
#      its LAST close — `blkdev_put_whole` → `kill_bdev` — so two bare
#      listings would read the device twice; the holder is what a
#      long-lived daemon is), and lists the appender directory (`squeezefs
#      appenders --json` — its kernel caches the pages), the HOST manager
#      takes ≥ 2 checkpoint cycles (page 0's generation moves), the host
#      takes its own listing, the guest lists AGAIN. Two buffered faces
#      read RED here: the READER's stale cache (the guest's second read is
#      its first image — the cloud row's joiner reading its own page one
#      generation behind) and the WRITER's write-behind (the host's page
#      image dirty in its cache until the next cycle's barrier — a second
#      kernel reads the device one image behind); under O_DIRECT on both
#      sides the guest reads at least the host's word taken before it.
#   2. THE JOIN: the guest mounts as a JOINED writer over the wire (its
#      listener on the tap, the manager's on the tap's host address),
#      `mkdir` + creates + `rm -rf` land, the host reads every acked name
#      through the divert, the guest leaves clean, the post-leave census
#      (online fsck on the manager) is clean.
leg_sym_two_host() {
    require_symmetric
    require_vm_fleet
    [ "${VM_NET:-user}" = "tap" ] ||
        die "sym-two-host needs the TAP guest network (create … --vm=1 --vm-net=tap): the manager must dial the joiner's listener, which slirp cannot route"
    local rowdir g_nqn g_id creates="${SQZ_MWMATRIX_TWOHOST_FILES:-200}"
    rowdir="$STATE/rows/twohost-$(date +%s)"
    mkdir -p "$rowdir"
    g_nqn="$(guest_nqn 90)" g_id="$(guest_id 90)"
    local meta_nqn meta_path
    meta_nqn="$(echo "$META_NQNS" | awk '{print $1}')"
    meta_path="$(echo "$FORMAT_META_PATHS" | cut -d, -f1)"
    [ "$(echo "$META_NQNS" | wc -w)" = 1 ] ||
        die "sym-two-host is the one-metadata-volume shape (SQZ_MWFLEET_MDS_COUNT=1; got '$META_NQNS')"

    # ---- job 1: connect the fleet's meta NQN in-guest and list the directory ----
    {
        guest_job_preamble
        cat <<JOB1
NQN='$meta_nqn'
# Idempotent over a previous run's controller (EALREADY = already connected).
if ! \$SQZ nvmeof connect --ip "\$GW" --port "\$SVC" --subnqn "\$NQN" --hostnqn '$g_nqn' --hostid '$g_id' >/tmp/connect.out 2>&1; then
    grep -q "already in progress" /tmp/connect.out || { cat /tmp/connect.out; echo "FAIL: guest meta connect"; exit 1; }
fi
head=""
i=0
while [ \$i -lt 40 ]; do
    for s in \$(subsys_dirs_for_nqn "\$NQN"); do head=\$(head_of_dir "\$s") && break; done
    [ -n "\$head" ] && [ -b "/dev/\$head" ] && break
    i=\$((i + 1)); sleep 0.5
done
[ -n "\$head" ] && [ -b "/dev/\$head" ] || { echo "FAIL: \$NQN resolved no openable head in-guest"; exit 1; }
echo "GUEST_META_HEAD=\$head"
echo "guest logical block size: \$(cat /sys/block/\$head/queue/logical_block_size)"
# The bdev HOLDER: an fd on the namespace kept open across both listings
# (the pin's premise — the guest's page cache of the device persists the
# way a long-lived daemon's does; killed by job 2 after its listing).
( exec 3</dev/\$head; exec sleep 3600 ) >/dev/null 2>&1 &
echo \$! >/tmp/bdev-holder.pid
sleep 0.3
kill -0 "\$(cat /tmp/bdev-holder.pid)" 2>/dev/null || { echo "FAIL: the bdev holder did not start"; exit 1; }
echo "GUEST_BDEV_HOLDER=\$(cat /tmp/bdev-holder.pid)"
\$SQZ appenders "sqmeta:///dev/\$head" --json >/tmp/appenders-1.json 2>/tmp/appenders-1.err || { cat /tmp/appenders-1.err; echo "FAIL: appenders listing 1"; exit 1; }
cat /tmp/appenders-1.json
JOB1
    } >"$rowdir/job1.sh"
    "$MWFLEET" vm-exec 0 "$rowdir/job1.sh" 300 >"$rowdir/job1.out" 2>&1 ||
        die "guest job 1 (connect + list) FAILED: $(tail -5 "$rowdir/job1.out")"
    local g_head g_gen1
    g_head="$(awk -F= '/^GUEST_META_HEAD=/ {print $2}' "$rowdir/job1.out" | tr -d '\r')"
    [ -n "$g_head" ] || die "guest job 1 reported no head"
    g_gen1="$(python3 - "$rowdir/job1.out" <<'PY'
import json, sys
txt = open(sys.argv[1]).read()
# The JSON array starts at the first line that is exactly `[` (the job's
# log lines above it carry bracketed timestamps).
start = 0 if txt.startswith('[\n') else txt.index('\n[\n') + 1
rows = json.loads(txt[start:txt.rindex(']') + 1])
print(next(r["generation"] for r in rows if r.get("appender_id") == 0 and r.get("state") == "live"))
PY
)" || die "guest listing 1 carries no Live page 0"
    log "guest listed the directory (head /dev/$g_head): page 0 at generation $g_gen1"

    # ---- the host writes: ≥ 2 checkpoint cycles move page 0's generation ----
    local mnt0 ck0 ck1 i
    mnt0="$(mnt_of 0)"
    ck0="$(stat_sum 0 meta_kv_checkpoints)"
    mkdir -p "$mnt0/two-host-pin"
    for i in $(seq 1 64); do
        printf 'pin:%s\n' "$i" | dd of="$mnt0/two-host-pin/f$i" conv=fsync status=none
    done
    for i in $(seq 1 120); do
        ck1="$(stat_sum 0 meta_kv_checkpoints)"
        [ "$ck1" -ge "$((ck0 + 2))" ] && break
        sleep 0.5
    done
    [ "$ck1" -ge "$((ck0 + 2))" ] || die "the manager took no two checkpoint cycles in 60 s ($ck0 → $ck1)"
    # Quiesce the manager (no cycle for 3 s) so the two listings below
    # compare one durable image, never a race with a late cycle.
    local stable=0
    for i in $(seq 1 120); do
        sleep 1
        ck0="$(stat_sum 0 meta_kv_checkpoints)"
        if [ "$ck0" = "$ck1" ]; then stable=$((stable + 1)); else stable=0; ck1="$ck0"; fi
        [ "$stable" -ge 3 ] && break
    done
    [ "$stable" -ge 3 ] || die "the manager never quiesced (checkpoints kept moving for 120 s)"
    log "host wrote ≥ 2 checkpoint cycles and quiesced at $ck1 (guest's first read: page 0 at generation $g_gen1)"

    # ---- the host's OWN word for the durable image, taken BEFORE the
    # guest's second read (its cache is the writer's — coherent with its
    # writes by construction). The pin compares the guest's read against
    # the word the host had ALREADY written when the read began: a cycle
    # that lands between the two listings (the forest's trailing root
    # publications, the slot-lease cadence's page writes — none of them a
    # `meta_kv_checkpoints` step the quiesce loop above counts) moves the
    # host's page AFTER the guest read it, which is not incoherence. Under
    # buffered I/O the guest's second read stands at its first (below the
    # host's word); under O_DIRECT it is at least the host's word.
    local h_gen
    "$SQZ" appenders "sqmeta://$meta_path" --json >"$rowdir/host-appenders.json" 2>"$rowdir/host-appenders.err" ||
        die "host appenders listing failed: $(tail -3 "$rowdir/host-appenders.err")"
    h_gen="$(python3 -c '
import json, sys
rows = json.load(open(sys.argv[1]))
print(next(r["generation"] for r in rows if r.get("appender_id") == 0 and r.get("state") == "live"))' "$rowdir/host-appenders.json")" ||
        die "host listing carries no Live page 0"
    [ "$h_gen" -gt "$g_gen1" ] ||
        die "page 0's generation did not move on the host ($g_gen1 → $h_gen) — the pin's premise (a host write after the guest's read) is unmet"

    # ---- job 2: the guest lists AGAIN — the pin ----
    {
        guest_job_preamble
        cat <<JOB2
kill -0 "\$(cat /tmp/bdev-holder.pid)" 2>/dev/null || { echo "FAIL: the bdev holder died between the listings (the cache-persistence premise)"; exit 1; }
\$SQZ appenders "sqmeta:///dev/$g_head" --json >/tmp/appenders-2.json 2>/tmp/appenders-2.err || { cat /tmp/appenders-2.err; echo "FAIL: appenders listing 2"; exit 1; }
kill "\$(cat /tmp/bdev-holder.pid)" 2>/dev/null || true
cat /tmp/appenders-2.json
JOB2
    } >"$rowdir/job2.sh"
    "$MWFLEET" vm-exec 0 "$rowdir/job2.sh" 300 >"$rowdir/job2.out" 2>&1 ||
        die "guest job 2 (second listing) FAILED: $(tail -5 "$rowdir/job2.out")"
    local g_gen2
    g_gen2="$(python3 - "$rowdir/job2.out" <<'PY'
import json, sys
txt = open(sys.argv[1]).read()
# The JSON array starts at the first line that is exactly `[` (the job's
# log lines above it carry bracketed timestamps).
start = 0 if txt.startswith('[\n') else txt.index('\n[\n') + 1
rows = json.loads(txt[start:txt.rindex(']') + 1])
print(next(r["generation"] for r in rows if r.get("appender_id") == 0 and r.get("state") == "live"))
PY
)" || die "guest listing 2 carries no Live page 0"
    # The host's word AFTER the guest's read, for the evidence file (a
    # cycle between the two listings shows here as h_gen_after > h_gen).
    local h_gen_after
    "$SQZ" appenders "sqmeta://$meta_path" --json >"$rowdir/host-appenders-after.json" 2>/dev/null &&
        h_gen_after="$(python3 -c '
import json, sys
rows = json.load(open(sys.argv[1]))
print(next(r["generation"] for r in rows if r.get("appender_id") == 0 and r.get("state") == "live"))' "$rowdir/host-appenders-after.json" 2>/dev/null)" || h_gen_after="?"
    echo "guest_gen_before=$g_gen1 guest_gen_after=$g_gen2 host_gen_before_read=$h_gen host_gen_after_read=$h_gen_after" >"$rowdir/pin.txt"
    [ "$g_gen2" -ge "$h_gen" ] ||
        die "F-C1 PIN RED: the guest re-read page 0 at generation $g_gen2 while the host had written it to $h_gen BEFORE that read (the guest's first read left it at $g_gen1) — $([ "$g_gen2" = "$g_gen1" ] && echo "the READER's stale cache: the guest's kernel served its own first image of a block the manager rewrote" || echo "the WRITER's write-behind: the host's page image sat in its cache, barriered only by a later cycle, and the second kernel read the device one image behind"): shared-LUN metadata I/O is not coherent (design-symmetric-metadata §5.12; evidence $rowdir)"
    log "F-C1 PIN GREEN: the guest's second read ($g_gen2) is at least the host's word before it ($h_gen; the host's word after it: $h_gen_after)"

    # ---- job 3: the guest JOINS as a writer and creates ----
    # The guest mounts under ITS OWN per-mount identity (the pair job 1
    # connected the meta NQN with — KD-MW-3): the DATA volume's device
    # name is the HOST's (`/dev/nvmeXnY` from the format), so the guest's
    # daemon must connect it itself off the durable `fabric_endpoint:`
    # record (the tap's host address) and resolve the head under its own
    # controller — the rung-2 daemon-owned connect, exactly what a second
    # host does in the field.
    local mw_port="${MW_PORT:-45999}"
    {
        guest_job_preamble
        cat <<JOB3
mkdir -p /mnt/j /etc/squeezefs
# The FRESH-KERNEL premise (the fork fix this venue found): the guest has
# never mounted, so \`fuse.enable_uring\` reads its default N here. Since
# 7.2.4 the kernel creates a connection's FUSE ring at INIT-REPLY time iff
# the parameter is Y THEN, so a daemon that flips it after the reply (the
# pre-PR-13i fork) registers nothing — every REGISTER EINVAL, the mount
# refused — and only its SECOND mount succeeds. The join below is the
# guest's FIRST mount: it must arm FUSE-over-io_uring at the first attempt.
eu=\$(cat /sys/module/fuse/parameters/enable_uring 2>/dev/null || echo '?')
echo "GUEST_ENABLE_URING_BEFORE=\$eu"
env SQUEEZEFS_SYMMETRIC_META=1 SQUEEZEFS_MW_BIND='$VM_TAP_GUEST_IP:$mw_port' \
    \$SQZ mount "sqmeta:///dev/$g_head" /mnt/j -o 'hostnqn=$g_nqn,hostid=$g_id' --daemon --log-file /tmp/j.log >/tmp/j.mount.out 2>&1 || { cat /tmp/j.mount.out; cat /tmp/j.log 2>/dev/null | tail -30; echo "FAIL: guest joiner mount"; exit 1; }
i=0
while [ \$i -lt 240 ]; do grep -q " /mnt/j " /proc/mounts && break; i=\$((i + 1)); sleep 0.5; done
grep -q " /mnt/j " /proc/mounts || { echo "FAIL: joiner never mounted"; tail -30 /tmp/j.log; exit 1; }
grep -q "mounted as a JOINED symmetric appender" /tmp/j.log || { echo "FAIL: no joined-door line"; tail -30 /tmp/j.log; exit 1; }
grep -q "daemon-owned controller resolved" /tmp/j.log || { echo "FAIL: no daemon-owned data connect line (rung-2 engagement — the guest resolved the host's device name?)"; tail -30 /tmp/j.log; exit 1; }
grep -q "FUSE-over-io_uring transport armed" /tmp/j.log || { echo "FAIL: the transport never armed on the guest's FIRST mount (enable_uring was \$eu before it)"; grep -n "REGISTER\|rejected\|enable_uring" /tmp/j.log | tail -8; exit 1; }
posture=\$(grep -o '"mount_posture": *"[a-z-]*"' /mnt/j/.stats | head -1 | sed 's/.*"\([a-z-]*\)"\$/\1/')
[ "\$posture" = "writer" ] || { echo "FAIL: guest posture \$posture"; exit 1; }
jid=\$(grep -o '"joined_appender_id": *[0-9]*' /mnt/j/.stats | head -1 | grep -o '[0-9]*\$')
echo "GUEST_APPENDER_ID=\$jid"
mkdir /mnt/j/two-host-guest || { echo "FAIL: mkdir under the root (the cloud row's shape) errno=\$?"; tail -20 /tmp/j.log; exit 1; }
n=0
i=0
while [ \$i -lt $creates ]; do
    printf 'guest:%s\n' "\$i" | dd of="/mnt/j/two-host-guest/g\$i" conv=fsync status=none && n=\$((n + 1))
    i=\$((i + 1))
done
[ "\$n" = $creates ] || { echo "FAIL: \$n of $creates guest creates acked"; exit 1; }
echo "GUEST_CREATES=\$n"
grep -o '"joined_control_refusals": *[0-9]*' /mnt/j/.stats | head -1
grep -o '"invariant_tripwires": *[0-9]*' /mnt/j/.stats | head -1
echo "GUEST JOIN GREEN"
JOB3
    } >"$rowdir/job3.sh"
    "$MWFLEET" vm-exec 0 "$rowdir/job3.sh" 900 >"$rowdir/job3.out" 2>&1 ||
        die "guest job 3 (join + creates) FAILED: $(tail -15 "$rowdir/job3.out")"
    grep -q "GUEST JOIN GREEN" "$rowdir/job3.out" || die "guest job 3 did not report GREEN"
    # The host reads every acked name through the divert (the guest holds
    # the slot; nothing here is a device read of the guest's tree).
    local missing=0
    for i in $(seq 0 $((creates - 1))); do
        [ "$(cat "$mnt0/two-host-guest/g$i" 2>/dev/null)" = "guest:$i" ] || missing=$((missing + 1))
    done
    [ "$missing" = 0 ] || die "the host misses $missing of $creates guest-acked files (cross-kernel read through the divert)"
    log "the host reads all $creates guest-acked files"

    # ---- job 4: rm -rf from the guest, then the clean leave ----
    {
        guest_job_preamble
        cat <<JOB4
rm -rf /mnt/j/two-host-guest || { echo "FAIL: guest rm -rf"; exit 1; }
[ ! -e /mnt/j/two-host-guest ] || { echo "FAIL: the directory survived its rm -rf"; exit 1; }
\$SQZ umount /mnt/j >/tmp/j.umount.out 2>&1 || { cat /tmp/j.umount.out; umount -l /mnt/j 2>/dev/null; echo "FAIL: guest umount"; exit 1; }
i=0
while [ \$i -lt 120 ]; do grep -q " /mnt/j " /proc/mounts || break; i=\$((i + 1)); sleep 0.5; done
grep -q " /mnt/j " /proc/mounts && { echo "FAIL: /mnt/j still mounted"; exit 1; }
echo "GUEST LEAVE GREEN"
JOB4
    } >"$rowdir/job4.sh"
    "$MWFLEET" vm-exec 0 "$rowdir/job4.sh" 600 >"$rowdir/job4.out" 2>&1 ||
        die "guest job 4 (rm -rf + leave) FAILED: $(tail -10 "$rowdir/job4.out")"
    [ ! -e "$mnt0/two-host-guest" ] || die "the host still sees the directory the guest removed"
    # The post-leave census on the manager: the guest's region is Free on
    # the DEVICE (a host read of a page the guest wrote), fsck clean.
    "$SQZ" appenders "sqmeta://$meta_path" --json >"$rowdir/host-appenders-after.json" 2>"$rowdir/host-appenders-after.err" ||
        die "host appenders listing (after the leave) failed: $(tail -3 "$rowdir/host-appenders-after.err")"
    python3 - "$rowdir/host-appenders-after.json" <<'PY' || die "the guest's region is not Free at the host after its clean leave (a stale host read of the guest's page?)"
import json, sys
rows = json.load(open(sys.argv[1]))
live = [r for r in rows if r.get("state") == "live"]
assert len(live) == 1 and live[0]["appender_id"] == 0, f"live pages after the leave: {[(r['appender_id'], r['state']) for r in rows]}"
PY
    local frc=0
    timeout 900 "$SQZ" fsck "$mnt0" --json >"$rowdir/fsck.json" 2>"$rowdir/fsck.err" || frc=$?
    [ "$frc" != "124" ] || die "the post-leave online fsck HUNG past 900 s — $rowdir/fsck.err"
    [ "$frc" = "0" ] || die "post-leave fsck failed (rc=$frc): $(tail -5 "$rowdir/fsck.err")"
    python3 -c '
import json, sys
r = json.load(open(sys.argv[1]))
by_class = {}
for f in r["findings"]:
    by_class[f["class"]] = by_class.get(f["class"], 0) + 1
print(f"post-leave census: findings by class {by_class or {}}")
assert not r["findings"], f"fsck findings: {by_class}"' "$rowdir/fsck.json" || die "post-leave fsck reports findings (evidence $rowdir/fsck.json)"
    # Job 5: disconnect the guest's controllers — the meta one job 1 made
    # and the data one its daemon made (zero residue).
    local data_nqns_all
    data_nqns_all="$(echo "$DATA_NQNS" | tr ' ' '\n' | sed "s/^/disconnect_nqn '/; s/\$/'/")"
    {
        guest_job_preamble
        echo "disconnect_nqn '$meta_nqn'"
        echo "$data_nqns_all"
        echo "echo done"
    } >"$rowdir/job5.sh"
    "$MWFLEET" vm-exec 0 "$rowdir/job5.sh" 120 >"$rowdir/job5.out" 2>&1 || warn "guest disconnect reported errors"
    log "sym-two-host GREEN (pin + join + $creates creates + rm -rf + leave + census; evidence in $rowdir)"
}

# --- rung-7 S6 legs (design-full-multi-writer §7.2) ------------------------
require_membership() {
    [ -n "${MEMBERSHIP:-}" ] ||
        die "this leg needs a membership-armed fleet — create it with: sudo tests/mw_fleet.sh create N=<n> --membership"
    [ "$(stat_field 0 membership_mode)" = "owner" ] ||
        die "member 0 is not the membership OWNER (membership_mode != owner) — the arm did not engage"
}

# Poll one flattened stats field on member <idx> until it is >= <want>,
# within <deadline_s>. Echoes the final value; dies loud on timeout.
wait_stat_ge() { # idx key want deadline_s what
    local idx="$1" key="$2" want="$3" deadline="$4" what="$5" v t0 now
    t0="$(date +%s)"
    while :; do
        v="$(stat_field "$idx" "$key")"
        [[ "$v" =~ ^[0-9]+$ ]] && [ "$v" -ge "$want" ] && {
            echo "$v"
            return 0
        }
        now="$(date +%s)"
        [ $((now - t0)) -lt "$deadline" ] ||
            die "$what: m$idx $key=$v never reached $want within ${deadline}s"
        sleep 1
    done
}

# Poll one flattened stats field on member <idx> until it EQUALS <want>.
wait_stat_eq() { # idx key want deadline_s what
    local idx="$1" key="$2" want="$3" deadline="$4" what="$5" v t0 now
    t0="$(date +%s)"
    while :; do
        v="$(stat_field "$idx" "$key")"
        [ "$v" = "$want" ] && return 0
        now="$(date +%s)"
        [ $((now - t0)) -lt "$deadline" ] ||
            die "$what: m$idx $key=$v never reached $want within ${deadline}s"
        sleep 1
    done
}

# Wait for a literal line (grep -F) to appear in a log file.
wait_log_line() { # file pattern deadline_s what
    local file="$1" pat="$2" deadline="$3" what="$4" t0 now
    t0="$(date +%s)"
    while :; do
        grep -Fq "$pat" "$file" && return 0
        now="$(date +%s)"
        [ $((now - t0)) -lt "$deadline" ] ||
            die "$what: '$pat' never appeared in $file within ${deadline}s"
        sleep 1
    done
}

# The member uuid a mount's LATEST membership join minted (from its log).
member_uuid_of() { # idx
    grep -o "membership MEMBER armed: [a-z-]* '[0-9a-f-]*'" "$STATE/m${1}.log" |
        tail -1 | grep -o "'[0-9a-f-]*'" | tr -d "'"
}

# The owner's renewal cadence estimate, seconds: min(10, T_self/3) — the
# same derivation LeaseClocks ships, read back from the published gauges.
owner_renew_est_s() {
    local tself
    tself="$(stat_field 0 membership_self_deadline_ms)"
    python3 -c "print(max(1, min(10, int($tself) // 3000)))"
}

leg_s6_journal() {
    require_membership
    local rowdir n_members
    rowdir="$STATE/rows/s6journal-$(date +%s)"
    mkdir -p "$rowdir"
    n_members="$(member_idxs | wc -l)"
    [ "$n_members" -ge 2 ] || die "s6-journal needs N>=2"
    local i
    for i in $(member_idxs); do
        [ "$i" = "0" ] && continue
        [ "$(stat_field "$i" membership_mode)" = "member" ] ||
            die "reader $i is not a live membership member — the row would under-count beats"
    done
    local ttl tself renew_est
    ttl="$(stat_field 0 membership_lease_ttl_ms)"
    tself="$(stat_field 0 membership_self_deadline_ms)"
    renew_est="$(owner_renew_est_s)"
    log "s6-journal: N=$n_members (1 owner + $((n_members - 1)) members), window ${S6_WINDOW_S}s, owner clocks T_owner=${ttl}ms T_self=${tself}ms, renew cadence ~${renew_est}s"

    for i in $(member_idxs); do snap "$i" 0 "$rowdir"; done
    # The window is QUIET on purpose: it isolates the liveness plane's
    # journal cost (the S6 gate is about the BEAT plane, and a quiet
    # writer's journal delta is exactly the liveness + own-heartbeat
    # residue). Census probes ride the window — the read side S6 also
    # replaced (`squeezefs clients` = one record + a paged census RPC).
    local t0 now probes=0
    t0="$(date +%s)"
    while :; do
        now="$(date +%s)"
        [ $((now - t0)) -lt "$S6_WINDOW_S" ] || break
        sleep 30
        if "$SQZ" clients "sqmeta://$META_PATHS" >"$rowdir/clients.$probes.out" 2>&1; then
            probes=$((probes + 1))
        else
            die "squeezefs clients probe failed mid-window: $(tail -2 "$rowdir/clients.$probes.out")"
        fi
    done
    for i in $(member_idxs); do snap "$i" 1 "$rowdir"; done
    # Readers must be VISIBLE in the census (the S5 gap this plane closed).
    grep -q "member-reader" "$rowdir/clients.0.out" ||
        die "no member-reader row in squeezefs clients — readers stayed invisible:
$(cat "$rowdir/clients.0.out")"
    # shellcheck disable=SC2046 # member_idxs is a controlled numeric list
    emit_rows "$rowdir" s6-journal $(member_idxs)

    # The S6-a gates (design §7.2 row 1), over the owner's snapshots.
    python3 - "$rowdir" "$n_members" "$S6_WINDOW_S" "$renew_est" "$probes" <<'PYGATE'
import json, sys

rowdir, n, window, renew_est, probes = (
    sys.argv[1], int(sys.argv[2]), int(sys.argv[3]), int(sys.argv[4]), int(sys.argv[5]))

def flat(d, out=None, pfx=""):
    out = {} if out is None else out
    for k, v in d.items():
        if isinstance(v, dict):
            flat(v, out, pfx + k + ".")
        else:
            out[pfx + k] = v
    return out

def load(p):
    root = json.load(open(p))
    return flat(root.get("metrics", root))

o0, o1 = load(f"{rowdir}/m0_p0.json"), load(f"{rowdir}/m0_p1.json")
d = lambda k: int(o1.get(k, 0)) - int(o0.get(k, 0))

renewals = d("membership_renewals")
jrnl = d("meta_kv_journal_entries")
reg = d("membership_registration_commits")
census = d("membership_census_serves")
evict = d("membership_evictions")
fences = d("membership_self_fences")
refusals = d("membership_grace_refusals")

members = n - 1
expected_beats = members * window // max(renew_est, 1)
per_beat = jrnl / renewals if renewals else float("inf")
# The OWNER's own cadence residue is N-INDEPENDENT (its client:/
# writer_claim heartbeats, echo drains, checkpoints — measured ~3 tx per
# 10 s beat on this tree; the allowance carries 2x headroom). The S6
# regression the gate exists to catch is journal growth COUPLED to the
# member beats (the pre-S6 plane paid exactly 1.0 tx per beat), so the
# bound is: owner allowance + half a tx per beat — N-independent on a
# healthy plane, violated the moment beats start committing.
owner_allowance = (window // 10 + 1) * 6
bound = owner_allowance + renewals // 2

print(f"== s6-journal arithmetic (the spec §6.5 item-3 gate) ==")
print(f"  members (beating)          : {members}")
print(f"  window                     : {window}s, renew cadence ~{renew_est}s")
print(f"  membership_renewals delta  : {renewals} (expected ~{expected_beats})")
print(f"  meta_kv_journal_entries d  : {jrnl}  <- must stay ~= the OWNER's own N-independent residue")
print(f"  owner-cadence allowance    : {owner_allowance} (6 tx / 10 s writer cadence, 2x-headroom)")
print(f"  regression bound           : jrnl < allowance + renewals/2 = {bound}")
print(f"  journal txs PER BEAT       : {per_beat:.4f} (the pre-S6 plane paid 1.0 per beat)")
print(f"  pre-S6 equivalent cost     : ~{renewals} journal txs this window would have paid")
print(f"  registration_commits delta : {reg} (bounded by membership CHANGES; 0 here)")
print(f"  census_serves delta        : {census} over {probes} clients probes")
print(f"  self_fences/evictions/grace: {fences}/{evict}/{refusals}")

bad = []
if renewals < expected_beats // 2:
    bad.append(f"renewals {renewals} < half the expected {expected_beats} — the beat plane is not engaged")
if jrnl >= bound:
    bad.append(f"journal delta {jrnl} >= bound {bound} — journal growth is coupling to the heartbeat (the S6 regression)")
if members >= 8 and per_beat >= 0.25:
    bad.append(f"journal txs per beat {per_beat:.3f} >= 0.25 at N={members} beating members — beat-coupled growth (the sharp at-scale face; at small N the owner residue legitimately dominates this ratio)")
if reg != 0:
    bad.append(f"registration_commits moved ({reg}) with zero membership changes")
if census < probes:
    bad.append(f"census_serves {census} < {probes} probes — the census read side did not engage")
if fences != 0 or evict != 0 or refusals != 0:
    bad.append(f"self_fences={fences} evictions={evict} grace_refusals={refusals} on a healthy window (all must be 0)")
if bad:
    print("S6-a GATE FAILED:", file=sys.stderr)
    for b in bad:
        print(f"  {b}", file=sys.stderr)
    sys.exit(1)
print("S6-a GATE GREEN (heartbeat off the journal; census engaged; R5 columns in the row table above)")
PYGATE
    log "s6-journal leg GREEN (rows + snapshots in $rowdir)"
}

leg_s6_fence() {
    require_membership
    local rowdir victim
    rowdir="$STATE/rows/s6fence-$(date +%s)"
    mkdir -p "$rowdir"
    victim="${S6_VICTIM:-$(member_idxs | awk '$1!=0' | tail -1)}"
    [ -n "$victim" ] && [ "$victim" != "0" ] || die "s6-fence needs a reader victim (N>=2)"
    [ "$(role_of "$victim")" = "reader" ] || die "victim $victim is not a reader"

    local ttl tself renew_est
    ttl="$(stat_field 0 membership_lease_ttl_ms)"
    tself="$(stat_field 0 membership_self_deadline_ms)"
    renew_est="$(owner_renew_est_s)"
    [ "$tself" -lt "$ttl" ] ||
        die "clock law violated in the published gauges: T_self ($tself) must be strictly earlier than T_owner ($ttl)"
    log "s6-fence: victim m$victim, netem ${S6_NETEM_MS}ms/end, T_owner=${ttl}ms T_self=${tself}ms (member fences FIRST by construction)"

    # Remount the victim inside its own netns, with netem shaping its
    # membership wire (design row S6-b: 'netem +200 ms on one member's
    # veth, freeze via SIGSTOP past T_self').
    "$MWFLEET" unmount "$victim"
    "$MWFLEET" mount "$victim" "--netns=$S6_NETEM_MS"
    [ "$(stat_field "$victim" membership_mode)" = "member" ] ||
        die "victim did not re-join through the shaped netns wire"
    local vuuid
    vuuid="$(member_uuid_of "$victim")"
    [ -n "$vuuid" ] || die "cannot read the victim's member uuid from its log"
    log "victim m$victim re-joined through the netem-shaped netns wire as '$vuuid' (delayed renewals still inside the deadlines — the shaping is duress, not partition)"

    # Settle the census FIRST: the unmount->remount dance above leaves the
    # victim's PRIOR incarnations as stale census entries (a clean unmount
    # exits before its renewal loop's next wake can send the leave), and
    # their TTL sweeps would false-match any counter-based eviction wait —
    # so the eviction below is keyed on the victim's OWN member uuid, and
    # p0 is taken only once the census carries exactly the live members.
    local n_members evict_deadline
    n_members="$(member_idxs | wc -l)"
    evict_deadline=$(((ttl / 1000) + 3 * renew_est + 30))
    wait_stat_eq 0 membership_members "$((n_members - 1))" "$evict_deadline" "census settle (stale incarnations swept)"
    local i
    for i in $(member_idxs); do snap "$i" 0 "$rowdir"; done
    local fence0
    fence0="$(stat_field "$victim" membership_self_fences)"

    # Freeze the victim past T_self AND past the owner's TTL. SIGSTOP
    # leaves its monotonic clock RUNNING (unlike the VM pause), so on
    # resume the member observes T_self passed and fences by its OWN
    # clock — the row's 'member fences before the owner re-grants' half
    # is the arithmetic T_self < T_owner asserted above, enforced by the
    # owner acting only at ITS deadline.
    "$MWFLEET" kill "$victim" --sig STOP
    log "victim m$victim SIGSTOPped (frozen daemon, running clock)"
    # The owner must evict THIS incarnation (uuid-keyed — see above) and
    # the eviction line itself names the S6->S7 handoff: the dead lease
    # epoch and the do-not-reallocate quarantine.
    wait_log_line "$STATE/m0.log" "member '$vuuid' (reader) EVICTED" "$evict_deadline" "owner eviction of the frozen victim"
    grep -F "member '$vuuid' (reader) EVICTED" "$STATE/m0.log" | grep -q "quarantined" ||
        die "the victim's eviction line does not name the dead-epoch quarantine"
    grep -q "declared DEAD (membership: member '$vuuid'" "$STATE/m0.log" ||
        die "no dead-epoch mint for the victim's eviction — the S6->S7 handoff did not engage"
    log "owner evicted the frozen victim '$vuuid' + minted its S7 dead epoch (while the victim was still frozen)"

    "$MWFLEET" kill "$victim" --sig CONT
    log "victim m$victim resumed (SIGCONT) — it must now self-fence on its own clock"
    local fences
    fences="$(wait_stat_ge "$victim" membership_self_fences $((fence0 + 1)) $((3 * renew_est + 60)) "victim self-fence")"
    [ "$fences" = "$((fence0 + 1))" ] ||
        die "victim self-fenced $((fences - fence0)) times (want exactly 1)"
    grep -q "SELF-FENCED" "$STATE/m${victim}.log" ||
        die "victim log carries no SELF-FENCED line"
    grep -q "membership self-fence: dropped" "$STATE/m${victim}.log" ||
        die "victim log carries no purge line — the reader fail-stop did not drop its cached blocks"
    log "victim self-fenced + purged (membership_self_fences $fence0 -> $fences)"

    sleep 3 # let every reader's revalidation cadence tick before p1
    for i in $(member_idxs); do snap "$i" 1 "$rowdir"; done
    # Everyone else: ZERO fences, zero grace refusals (the blast radius is
    # exactly the deliberate victim).
    python3 - "$rowdir" "$victim" <<'PYGATE'
import json, sys

rowdir, victim = sys.argv[1], sys.argv[2]

def flat(d, out=None, pfx=""):
    out = {} if out is None else out
    for k, v in d.items():
        if isinstance(v, dict):
            flat(v, out, pfx + k + ".")
        else:
            out[pfx + k] = v
    return out

def load(p):
    root = json.load(open(p))
    return flat(root.get("metrics", root))

import glob, re
bad = []
for p1 in glob.glob(f"{rowdir}/m*_p1.json"):
    i = re.match(r".*/m(\d+)_p1", p1).group(1)
    d1, d0 = load(p1), load(f"{rowdir}/m{i}_p0.json")
    fences = int(d1.get("membership_self_fences", 0)) - int(d0.get("membership_self_fences", 0))
    refusals = int(d1.get("membership_grace_refusals", 0)) - int(d0.get("membership_grace_refusals", 0))
    if i == victim:
        if fences != 1:
            bad.append(f"m{i} (victim): self_fences delta {fences} != 1")
    elif fences != 0:
        bad.append(f"m{i}: self_fences delta {fences} != 0 — the blast radius leaked past the victim")
    if refusals != 0:
        bad.append(f"m{i}: grace_refusals moved ({refusals}) — no failover happened here")
if bad:
    print("S6-b GATE FAILED:", file=sys.stderr)
    for b in bad:
        print(f"  {b}", file=sys.stderr)
    sys.exit(1)
print("S6-b GATE GREEN (fence exactly on the victim; grace quiet)")
PYGATE
    # shellcheck disable=SC2046 # member_idxs is a controlled numeric list
    emit_rows "$rowdir" s6-fence $(member_idxs)

    # Restore the fleet: clear the shaping, remount the victim normally,
    # and require it to re-join live.
    "$MWFLEET" netem "$victim" off
    "$MWFLEET" unmount "$victim"
    "$MWFLEET" mount "$victim"
    [ "$(stat_field "$victim" membership_mode)" = "member" ] ||
        die "victim did not re-join after the restore remount"
    log "s6-fence leg GREEN (victim restored; rows + snapshots in $rowdir)"
}

leg_s6_vm_fence() {
    require_membership
    require_vm_fleet
    [ -n "${MEMBERSHIP_ENDPOINT:-}" ] || die "fleet config carries no MEMBERSHIP_ENDPOINT"
    case "$MEMBERSHIP_ENDPOINT" in
    127.0.0.1:* | 0.0.0.0:*)
        die "the owner advertises $MEMBERSHIP_ENDPOINT, which a guest cannot dial through slirp — this box has no routable primary interface; the S6-b' row needs one"
        ;;
    esac
    local rowdir g_nqn g_id
    rowdir="$STATE/rows/s6vmfence-$(date +%s)"
    mkdir -p "$rowdir"
    g_nqn="$(guest_nqn 70)" g_id="$(guest_id 70)"

    # The in-guest connect PLAN: the reader resolves its DATA volumes by
    # the FORMAT-TIME device paths (the identity-less-reader law), and the
    # host's format-era instance numbers (real local NVMe occupies the low
    # slots) cannot be reproduced by a fresh guest kernel's lowest-free
    # numbering — so the guest connects each fleet NQN, VERIFIES the
    # resolved head serves exactly that NQN (sysfs — the reader-safety
    # check's guest face), and then ALIASES the format-time name to it
    # (a devtmpfs symlink the daemon's open() follows; the verification
    # is what makes the alias safe, and an occupied name refuses loud).
    local plan meta_basenames
    plan="$(python3 - <<PYPLAN
import sys
meta_paths = "$FORMAT_META_PATHS".split(",")
data_paths = "$FORMAT_DATA_PATHS".split(",")
meta_nqns = "$META_NQNS".split()
data_nqns = "$DATA_NQNS".split()
pairs = list(zip(meta_paths, meta_nqns)) + list(zip(data_paths, data_nqns))
rows = []
for path, nqn in pairs:
    base = path.rsplit("/", 1)[-1]
    if not (base.startswith("nvme") and base.endswith("n1")):
        sys.exit(f"unexpected head name {base}")
    rows.append((int(base[4:-2]), base, nqn))
rows.sort()
for k, base, nqn in rows:
    print(f"{k} {base} {nqn}")
PYPLAN
)" || die "connect plan generation failed: $plan"
    echo "$plan" >"$rowdir/connect-plan"
    meta_basenames="$(python3 -c 'import sys
print(",".join("/dev/" + p.rsplit("/", 1)[-1] for p in sys.argv[1].split(",")))' "$FORMAT_META_PATHS")"
    log "s6-vm-fence: guest connect plan ($(echo "$plan" | wc -l) namespaces), meta URI in-guest: $meta_basenames"

    # ---- job A: join the fleet as a READ-ONLY member, in-guest ----
    {
        guest_job_preamble
        echo "PLAN='$plan'"
        cat <<JOBA
echo "\$PLAN" | while read -r k base nqn; do
    [ -n "\$nqn" ] || continue
    \$SQZ nvmeof connect --ip "\$GW" --port "\$SVC" --subnqn "\$nqn" --hostnqn '$g_nqn' --hostid '$g_id'
    head=""
    i=0
    while [ \$i -lt 40 ]; do
        for s in \$(subsys_dirs_for_nqn "\$nqn"); do head=\$(head_of_dir "\$s") && break; done
        [ -n "\$head" ] && [ -b "/dev/\$head" ] && break
        i=\$((i + 1)); sleep 0.5
    done
    [ -n "\$head" ] && [ -b "/dev/\$head" ] || { echo "FAIL: \$nqn resolved no openable head in-guest"; exit 1; }
    if [ "\$head" != "\$base" ]; then
        [ -e "/dev/\$base" ] && { echo "FAIL: format-time name /dev/\$base is already occupied in-guest — cannot alias safely"; exit 1; }
        ln -s "/dev/\$head" "/dev/\$base"
        echo "aliased /dev/\$base -> /dev/\$head (verified serving \$nqn)"
    fi
done || exit 1
echo "connect plan resolved (every format-time name verified against its NQN)"
mkdir -p /mnt/member
\$SQZ mount "sqmeta://$meta_basenames" /mnt/member --read-only --daemon --log-file /tmp/member.log >/tmp/member.mount.out 2>&1 || { cat /tmp/member.mount.out; cat /tmp/member.log 2>/dev/null; exit 1; }
i=0
while [ \$i -lt 240 ]; do grep -q " /mnt/member " /proc/mounts && break; i=\$((i + 1)); sleep 0.5; done
grep -q " /mnt/member " /proc/mounts || { echo "FAIL: member mount never appeared"; cat /tmp/member.log; exit 1; }
i=0
mode=""
while [ \$i -lt 60 ]; do
    mode=\$(grep -o '"membership_mode": *"[a-z]*"' /mnt/member/.stats | grep -o '"[a-z]*"\$' | sed 's/"//g')
    [ "\$mode" = "member" ] && break
    i=\$((i + 1)); sleep 0.5
done
[ "\$mode" = "member" ] || { echo "FAIL: guest membership_mode='\$mode' (want member) — cannot dial the owner at $MEMBERSHIP_ENDPOINT?"; grep -i membership /tmp/member.log; exit 1; }
fences=\$(grep -o '"membership_self_fences": *[0-9]*' /mnt/member/.stats | grep -o '[0-9]*\$')
epoch=\$(grep -o '"membership_epoch": *[0-9]*' /mnt/member/.stats | grep -o '[0-9]*\$')
guuid=\$(grep -o "membership MEMBER armed: reader '[0-9a-f-]*'" /tmp/member.log | tail -1 | grep -o "'[0-9a-f-]*'" | sed "s/'//g")
[ -n "\$guuid" ] || { echo "FAIL: cannot read the guest member uuid from its log"; exit 1; }
echo "GUEST_FENCES_BASE=\$fences"
echo "GUEST_EPOCH_BASE=\$epoch"
echo "GUEST_UUID=\$guuid"
echo "GUEST MEMBER GREEN (RO mount joined the host fleet's membership plane)"
JOBA
    } >"$rowdir/job-a.sh"
    # Settle the census FIRST (the S6-b lesson): stale prior incarnations
    # (a rig re-run's dead guest member, a remounted reader's old lease)
    # sweep on the owner's cadence and would false-match any COUNTER-based
    # eviction wait — so the census must read exactly the live host
    # members before the guest joins, and the eviction below is keyed on
    # the guest's OWN member uuid.
    local ttl renew_est n_host_members
    ttl="$(stat_field 0 membership_lease_ttl_ms)"
    renew_est="$(owner_renew_est_s)"
    n_host_members="$(($(member_idxs | wc -l) - 1))"
    wait_stat_eq 0 membership_members "$n_host_members" $(((ttl / 1000) + 3 * renew_est + 30)) "census settle (stale incarnations swept)"

    "$MWFLEET" vm-exec 0 "$rowdir/job-a.sh" 600 | tee "$rowdir/job-a.out" ||
        die "guest member join FAILED (output: $rowdir/job-a.out)"
    local g_fence0 g_epoch0 g_uuid
    g_fence0="$(awk -F= '/^GUEST_FENCES_BASE=/ {print $2}' "$rowdir/job-a.out" | tr -d '\r')"
    g_epoch0="$(awk -F= '/^GUEST_EPOCH_BASE=/ {print $2}' "$rowdir/job-a.out" | tr -d '\r')"
    g_uuid="$(awk -F= '/^GUEST_UUID=/ {print $2}' "$rowdir/job-a.out" | tr -d '\r')"
    [ -n "$g_fence0" ] && [ -n "$g_epoch0" ] && [ -n "$g_uuid" ] ||
        die "guest job reported no baselines"

    snap 0 0 "$rowdir"
    # The owner census must carry the guest (member-reader, mount point
    # /mnt/member) — the S5 gap closed cross-KERNEL for the first time.
    "$SQZ" clients "sqmeta://$META_PATHS" >"$rowdir/clients-joined.out" 2>&1 ||
        die "clients probe failed"
    grep -q "member-reader" "$rowdir/clients-joined.out" ||
        die "guest member not visible in squeezefs clients:
$(cat "$rowdir/clients-joined.out")"

    # ---- the hung kernel: qemu pause past the owner's TTL ----
    "$MWFLEET" pause 0
    log "guest 0 PAUSED (vcpus + guest clock frozen — the monotonic domain cannot observe T_self)"
    wait_log_line "$STATE/m0.log" "member '$g_uuid' (reader) EVICTED" $(((ttl / 1000) + 3 * renew_est + 60)) "owner eviction of the paused guest"
    grep -F "member '$g_uuid' (reader) EVICTED" "$STATE/m0.log" | grep -q "quarantined" ||
        die "the guest's eviction line does not name the dead-epoch quarantine"
    grep -q "declared DEAD (membership: member '$g_uuid'" "$STATE/m0.log" ||
        die "no dead-epoch mint for the guest's eviction — the S6->S7 handoff did not engage"
    log "owner evicted the paused guest '$g_uuid' + minted its S7 dead epoch (while the guest kernel was frozen)"

    "$MWFLEET" resume 0
    log "guest 0 resumed — it must observe itself dead and self-fence BEFORE holding any fresh lease"

    # ---- job B: the resume law, asserted in-guest ----
    {
        guest_job_preamble
        cat <<JOBB
i=0
fences=""
while [ \$i -lt 120 ]; do
    fences=\$(grep -o '"membership_self_fences": *[0-9]*' /mnt/member/.stats | grep -o '[0-9]*\$')
    [ -n "\$fences" ] && [ "\$fences" -gt "$g_fence0" ] && break
    i=\$((i + 1)); sleep 1
done
[ -n "\$fences" ] && [ "\$fences" -gt "$g_fence0" ] || { echo "FAIL: guest never self-fenced after resume (membership_self_fences=\$fences, base $g_fence0) — it resumed as a live member on its stale caches (the S6-b' falsifier)"; grep -i membership /tmp/member.log | tail -5; exit 1; }
[ "\$fences" = "$((g_fence0 + 1))" ] || { echo "FAIL: guest fenced \$fences times (want exactly $((g_fence0 + 1)))"; exit 1; }
grep -q "SELF-FENCED" /tmp/member.log || { echo "FAIL: no SELF-FENCED line in the guest daemon log"; exit 1; }
grep -q "membership self-fence: dropped" /tmp/member.log || { echo "FAIL: no purge line — the reader fail-stop did not drop its cached blocks"; exit 1; }
# The fixed ladder: fence FIRST, then a FRESH re-join (clean view, new
# epoch) — availability restored without ever serving the stale view.
i=0
mode=""
epoch=""
while [ \$i -lt 60 ]; do
    mode=\$(grep -o '"membership_mode": *"[a-z]*"' /mnt/member/.stats | grep -o '"[a-z]*"\$' | sed 's/"//g')
    epoch=\$(grep -o '"membership_epoch": *[0-9]*' /mnt/member/.stats | grep -o '[0-9]*\$')
    [ "\$mode" = "member" ] && [ -n "\$epoch" ] && [ "\$epoch" != "$g_epoch0" ] && break
    i=\$((i + 1)); sleep 1
done
[ "\$mode" = "member" ] || { echo "FAIL: guest did not re-join fresh after its fence (mode=\$mode)"; exit 1; }
[ "\$epoch" != "$g_epoch0" ] || { echo "FAIL: guest resurrected its dead epoch $g_epoch0"; exit 1; }
echo "GUEST_EPOCH_FRESH=\$epoch"
echo "GUEST RESUME LAW GREEN (fenced + purged FIRST, then re-joined fresh: epoch $g_epoch0 -> \$epoch)"
JOBB
    } >"$rowdir/job-b.sh"
    "$MWFLEET" vm-exec 0 "$rowdir/job-b.sh" 300 | tee "$rowdir/job-b.out" ||
        die "guest resume-law job FAILED (output: $rowdir/job-b.out)"

    snap 0 1 "$rowdir"
    local refusals0 refusals1
    local graceprobe='import json, sys
def flat(d, out=None, pfx=""):
    out = {} if out is None else out
    for k, v in d.items():
        if isinstance(v, dict): flat(v, out, pfx + k + ".")
        else: out[pfx + k] = v
    return out
root = json.load(open(sys.argv[1]))
print(flat(root.get("metrics", root)).get("membership_grace_refusals", 0))'
    refusals0="$(python3 -c "$graceprobe" "$rowdir/m0_p0.json")"
    refusals1="$(python3 -c "$graceprobe" "$rowdir/m0_p1.json")"
    [ "$refusals0" = "$refusals1" ] ||
        die "membership_grace_refusals moved ($refusals0 -> $refusals1) — no failover happened here"

    # ---- job C: cleanup (always) ----
    {
        guest_job_preamble
        cat <<JOBC
set +e
\$SQZ umount /mnt/member >/dev/null 2>&1 || umount -l /mnt/member 2>/dev/null
i=0
while [ \$i -lt 120 ]; do grep -q " /mnt/member " /proc/mounts || break; i=\$((i + 1)); sleep 0.5; done
echo "\$PLAN" >/dev/null 2>&1
for c in /sys/class/nvme/nvme*; do
    [ "\$(cat "\$c/hostnqn" 2>/dev/null)" = '$g_nqn' ] || continue
    echo 1 >"\$c/delete_controller" 2>/dev/null
done
sleep 1
echo "cleanup done"
exit 0
JOBC
    } >"$rowdir/job-cleanup.sh"
    "$MWFLEET" vm-exec 0 "$rowdir/job-cleanup.sh" 300 | tee "$rowdir/job-cleanup.out" ||
        warn "in-guest cleanup reported errors"
    log "s6-vm-fence leg GREEN (evidence in $rowdir)"
}

# --- rung-8 S7 legs (design-full-multi-writer §7.2 rows S7-a / S7-b) --------
# Every fleet's writer arms the device-enforced data plane since PR 14 (the
# join ladder's rung 4 — the retired `--multi-writer` flag named the same
# WERO hold as an opt-in); the leg still demands the hold be STANDING.
require_mw() {
    require_membership
    [ "$(stat_field 0 data_plane_fence_mode)" = "1" ] ||
        die "member 0 data_plane_fence_mode != 1 — the S7 WERO hold is not standing (a non-PR substrate? the fleet's writer log names the refusal)"
}

# Controller (nvmeX) serving <subsysnqn> under <hostnqn> — the rung-2
# ctrl-char-dev discipline (the head block node round-robins paths; the
# char device pins the association).
ctrl_for() { # subsysnqn hostnqn -> nvmeX
    local c
    for c in /sys/class/nvme/nvme*; do
        [ -d "$c" ] || continue
        [ "$(cat "$c/subsysnqn" 2>/dev/null)" = "$1" ] || continue
        [ "$(cat "$c/hostnqn" 2>/dev/null)" = "$2" ] || continue
        basename "$c"
        return 0
    done
    return 1
}

# Reservation report probe: prints "<regctl> <rtype> <rkey0>" (rkey0 = the
# first registrant's key, 0x-hex; '-' when none). nvme-cli json spellings
# vary across releases — parse defensively.
resv_probe() { # ctrl-char-dev nsid
    nvme resv-report "$1" -n "$2" -o json 2>/dev/null | python3 -c '
import json, sys
try:
    r = json.load(sys.stdin)
except Exception:
    print("- - -"); raise SystemExit
regctl = r.get("regctl", 0)
rtype = r.get("rtype", 0)
regs = r.get("regctlext") or r.get("regctl_ext") or r.get("regctls") or []
rkey = "-"
if regs:
    k = regs[0].get("rkey", 0)
    rkey = hex(k) if isinstance(k, int) else str(k)
print(regctl, rtype, rkey)'
}

# Wait until <ctrl> has scanned at least one namespace: reservation ioctls
# on a controller whose namespaces have not attached yet answer ENOTTY
# ('Inappropriate ioctl for device') — the run-3 rig lesson. Multipath
# kernels expose per-path namespaces as nvmeXcYnZ under the controller.
wait_ctrl_ns() { # nvmeX
    local t d b
    for t in $(seq 1 60); do
        : "$t"
        for d in "/sys/class/nvme/$1"/nvme*; do
            b="$(basename "$d")"
            if [[ "$b" =~ ^nvme[0-9]+(c[0-9]+)?n[0-9]+$ ]]; then
                return 0
            fi
        done
        sleep 0.25
    done
    die "controller $1 never scanned a namespace (reservation ioctls would answer ENOTTY)"
}

# Delete ONE controller by name (sysfs) — never `nvmeof disconnect <nqn>`,
# which would drop the WRITER's association on the same NQN too.
delete_ctrl() { # nvmeX
    echo 1 >"/sys/class/nvme/$1/delete_controller" 2>/dev/null || true
    local t
    for t in $(seq 1 40); do
        [ -d "/sys/class/nvme/$1" ] || return 0
        sleep 0.25
    done
    warn "controller $1 did not tear down within 10s"
}

# Read one flattened stats field out of a SNAPSHOT file (frozen daemons
# cannot serve their stats inode — snapshots are the frozen-window truth).
snap_field() { # snapfile key
    python3 -c '
import json, sys
def flat(d, out=None, pfx=""):
    out = {} if out is None else out
    for k, v in d.items():
        if isinstance(v, dict): flat(v, out, pfx + k + ".")
        else: out[pfx + k] = v
    return out
root = json.load(open(sys.argv[1]))
print(flat(root.get("metrics", root)).get(sys.argv[2], ""))' "$1" "$2"
}

# Best-effort operator restore when the s7-device-fence leg dies mid-row:
# a `die` inside the frozen window would otherwise strand the writer
# SIGSTOPped (the run-2/run-3 rig lesson — a frozen daemon hangs every
# later mountpoint/umount probe in D-state). Loud, never a verdict.
S7_RESTORE_WRITER_PID=""
S7_RESTORE_DD_PID=""
S7_RECOVERY_HELD=0
S7_RECOVERY_KEY=""
S7_RECOVERY_NQN=""
S7_RECOVERY_ID=""

# Release the recovery identity's WERO hold on every data namespace —
# shared by the leg's own restore section and the fail trap (a stranded
# recovery reservation makes every later MW mount refuse: 'a partial
# fence is not a fence' — the s7-b round-1 cascade).
s7_release_recovery_hold() {
    [ "$S7_RECOVERY_HELD" = "1" ] || return 0
    local nqn rctrl i
    local -a nqns=()
    read -r -a nqns <<<"$DATA_NQNS"
    for nqn in "${nqns[@]}"; do
        "$SQZ" nvmeof connect --ip "$TCP_ADDR" --port "$TCP_SVC" --subnqn "$nqn" \
            --hostnqn "$S7_RECOVERY_NQN" --hostid "$S7_RECOVERY_ID" >/dev/null 2>&1 || true
        rctrl=""
        for i in $(seq 1 40); do
            rctrl="$(ctrl_for "$nqn" "$S7_RECOVERY_NQN")" && break
            sleep 0.25
        done
        if [ -n "$rctrl" ]; then
            wait_ctrl_ns "$rctrl"
            nvme resv-release "/dev/$rctrl" -n 1 --crkey="$S7_RECOVERY_KEY" --rtype=3 >/dev/null 2>&1 || true
            nvme resv-register "/dev/$rctrl" -n 1 --crkey="$S7_RECOVERY_KEY" --rrega=1 >/dev/null 2>&1 || true
            delete_ctrl "$rctrl"
        fi
    done
    S7_RECOVERY_HELD=0
}

s7_restore_on_fail() {
    local rc=$?
    [ "$rc" -eq 0 ] && return 0
    warn "s7-device-fence leg exiting rc=$rc — best-effort restore (SIGCONT writer, kill load, release recovery hold)"
    [ -n "$S7_RESTORE_DD_PID" ] && kill -9 "$S7_RESTORE_DD_PID" 2>/dev/null
    [ -n "$S7_RESTORE_WRITER_PID" ] && kill -CONT "$S7_RESTORE_WRITER_PID" 2>/dev/null
    s7_release_recovery_hold || true
    return 0
}

leg_s7_device_fence() {
    require_mw
    trap s7_restore_on_fail EXIT
    S7_RESTORE_WRITER_PID="$(awk -F'\t' '$1==0 {print $7}' "$MEMBERS")"
    local rowdir victim_reader
    rowdir="$STATE/rows/s7fence-$(date +%s)"
    mkdir -p "$rowdir"
    victim_reader="$(member_idxs | awk '$1!=0' | head -1)"
    [ -n "$victim_reader" ] || die "s7-device-fence needs N>=2 (a reader member observes the frozen owner)"

    local w_mnt ttl renew_est
    w_mnt="$(mnt_of 0)"
    ttl="$(stat_field 0 membership_lease_ttl_ms)"
    renew_est="$(owner_renew_est_s)"
    log "s7-device-fence (SCOPED posture — see header): victim = the ARMED WRITER m0 (owner+authority+WERO holder); recovery actor = the rig via the rung-2 preempt primitive; T_owner=${ttl}ms"

    # --- device-truth p0: the zombie's registrant key per data namespace ----
    # Read through the WRITER's own daemon-owned controllers (reservation
    # REPORT is a read; the char dev pins the association).
    local nqn wctrl regctl rtype zkey="" probe
    local -a data_nqns=()
    read -r -a data_nqns <<<"$DATA_NQNS"
    [ "${#data_nqns[@]}" -ge 1 ] || die "config carries no DATA_NQNS"
    for nqn in "${data_nqns[@]}"; do
        wctrl="$(ctrl_for "$nqn" "$W_HOSTNQN")" ||
            die "no writer-identity controller for data NQN $nqn"
        probe="$(resv_probe "/dev/$wctrl" 1)"
        read -r regctl rtype zkey <<<"$probe"
        [ "$rtype" = "3" ] ||
            die "$nqn: standing reservation rtype=$rtype (want 3 = WERO) — the S7 hold is not what the arm claims"
        [ "$regctl" = "1" ] ||
            die "$nqn: regctl=$regctl (want exactly 1 = the writer) before the recovery actor registers"
        [ "$zkey" != "-" ] || die "$nqn: no registrant key readable"
        log "p0 device truth: $nqn rtype=3 regctl=1 zombie key=$zkey (via /dev/$wctrl)"
    done

    local i
    for i in $(member_idxs); do snap "$i" 0 "$rowdir"; done
    local rfence0
    rfence0="$(stat_field "$victim_reader" membership_self_fences)"

    # --- sustained write load, running when the freeze lands ----------------
    log "starting sustained write load on the writer mount"
    (exec dd if=/dev/zero of="$w_mnt/s7load.dat" bs=1M count=16384 conv=fsync status=none) &
    local dd_pid=$!
    S7_RESTORE_DD_PID="$dd_pid"
    sleep 3 # let the pipeline fill (in-flight DMA to resume later)
    kill -0 "$dd_pid" 2>/dev/null || die "write load exited before the freeze (too small for this box?)"

    # --- freeze the armed writer past the membership TTL --------------------
    "$MWFLEET" kill 0 --sig STOP
    log "armed writer m0 SIGSTOPped (frozen daemon; its kernel keeps draining already-submitted DMA)"
    # S6 composition face: the reader member observes the FROZEN owner and
    # self-fences by its own clock (T_self) before any re-grant could exist.
    local rfences
    rfences="$(wait_stat_ge "$victim_reader" membership_self_fences $((rfence0 + 1)) $((ttl / 1000 + 6 * renew_est + 60)) "reader self-fence against the frozen owner")"
    log "reader m$victim_reader self-fenced against the frozen owner (membership_self_fences $rfence0 -> $rfences) — the S6 owner-death law"
    # Let already-submitted kernel I/O drain to the single existing path
    # before a second path exists (merged-head kernels round-robin).
    sleep 3

    # --- the recovery actor: register + PREEMPT the zombie's key ------------
    # The rung-2-proven product takeover primitive, per data namespace: a
    # RECOVERY identity registers its own key and preempts the zombie's
    # (racqa=1) — the same act a successor's drain proof performs
    # (WeroHold::preempt). The reservation stays HELD by the recovery key:
    # releasing it would re-admit the zombie (WERO rejects only while a
    # reservation stands).
    local r_id r_nqn rkey=0x51e7a8 rctrl
    r_id="$(printf 'cafef1e7-%04d-4000-8000-%012d' 87 "$CREATE_PID")"
    r_nqn="nqn.2014-08.org.nvmexpress:uuid:$r_id"
    S7_RECOVERY_KEY="$rkey" S7_RECOVERY_NQN="$r_nqn" S7_RECOVERY_ID="$r_id" S7_RECOVERY_HELD=1
    local -a rctrls=()
    for nqn in "${data_nqns[@]}"; do
        "$SQZ" nvmeof connect --ip "$TCP_ADDR" --port "$TCP_SVC" --subnqn "$nqn" \
            --hostnqn "$r_nqn" --hostid "$r_id" >/dev/null 2>&1 ||
            die "recovery-identity connect failed for $nqn"
        rctrl=""
        for i in $(seq 1 40); do
            rctrl="$(ctrl_for "$nqn" "$r_nqn")" && break
            sleep 0.25
        done
        [ -n "$rctrl" ] || die "no recovery-identity controller for $nqn"
        rctrls+=("$rctrl")
        wait_ctrl_ns "$rctrl"
        nvme resv-register "/dev/$rctrl" -n 1 --nrkey="$rkey" --cptpl=0 >/dev/null ||
            die "recovery register failed on $nqn"
        nvme resv-acquire "/dev/$rctrl" -n 1 --crkey="$rkey" --prkey="$zkey" \
            --rtype=3 --racqa=1 >/dev/null ||
            die "recovery preempt of the zombie key $zkey failed on $nqn"
        probe="$(resv_probe "/dev/$rctrl" 1)"
        read -r regctl rtype _ <<<"$probe"
        [ "$regctl" = "1" ] && [ "$rtype" = "3" ] ||
            die "$nqn post-preempt report: regctl=$regctl rtype=$rtype (want 1/3) — the preempt did not land"
        log "PR preempt observed on target: $nqn zombie key $zkey removed, recovery key $rkey holds WERO (regctl=1)"
    done
    echo "${rctrls[*]}" >"$rowdir/recovery-ctrls"
    # Drop the recovery PATHS before the zombie resumes: on a merged-head
    # (multipath=Y) kernel the resumed zombie's I/O must ride ITS OWN
    # association only. The reservation is host-keyed device state and
    # stands after the disconnect.
    for rctrl in "${rctrls[@]}"; do delete_ctrl "$rctrl"; done
    log "recovery paths dropped (reservation stands, held by $rkey)"

    # --- resume: the zombie's DMA must be rejected BY THE DEVICE ------------
    "$MWFLEET" kill 0 --sig CONT
    log "zombie writer m0 resumed (SIGCONT) — its in-flight + new DMA now meets the device fence"
    # The reservation-conflict errno class (EBADE, 'Invalid exchange') on a
    # DATA volume, then the zombie's own fail-stop: the fence latch + the
    # custody poison + refusals counted (data_dma_fence_refusals).
    local t0 now deadline=120
    t0="$(date +%s)"
    while :; do
        grep -Eq "os error 52|Invalid exchange" "$STATE/m0.log" && break
        now="$(date +%s)"
        [ $((now - t0)) -lt "$deadline" ] ||
            die "no reservation-conflict errno class (EBADE/os error 52) in the zombie's log within ${deadline}s — the device did not reject the resumed DMA"
        sleep 1
    done
    log "device rejection observed: reservation-conflict errno class in the zombie's log"
    wait_log_line "$STATE/m0.log" "writer guard FENCED" "$deadline" "zombie data-plane fence latch"
    wait_log_line "$STATE/m0.log" "data-plane custody POISONED" "$deadline" "zombie custody poison"
    local refusals
    refusals="$(wait_stat_ge 0 data_dma_fence_refusals 1 "$deadline" "zombie data_dma_fence_refusals")"
    log "zombie FAIL-STOPPED: fence latched, custody poisoned, data_dma_fence_refusals=$refusals"
    kill -9 "$dd_pid" 2>/dev/null || true
    wait "$dd_pid" 2>/dev/null || true

    # --- the S6->S7 handoff on the resumed owner: evict + dead-epoch mint ---
    # RACE, stated honestly: the fenced reader's retry loop re-presents
    # FRESH the moment the owner answers (the rung-8 finding-#1 fix), and a
    # fresh JOIN replaces the dead lease in the owner's RAM table before
    # the TTL sweep can expire it — so the resumed owner mints a dead epoch
    # ONLY when its sweep wins that race. Both outcomes are correct
    # product behavior; the leg accepts EITHER the mint (sweep won) or the
    # reader's fenced-then-fresh re-join (the fix's path won — its own law
    # is pinned in cargo, and the S6→S7 mint composition is rung 7's
    # proven S6-b/S6-b' row). One of the two MUST hold, loudly.
    local mint_deadline mint_seen=0 t0m
    mint_deadline=$((6 * renew_est + 30))
    t0m="$(date +%s)"
    while :; do
        if grep -Fq "declared DEAD (membership: member" "$STATE/m0.log"; then
            mint_seen=1
            break
        fi
        [ $(($(date +%s) - t0m)) -lt "$mint_deadline" ] || break
        sleep 1
    done
    if [ "$mint_seen" = "1" ]; then
        log "resumed owner evicted its expired member(s) + minted the S7 dead epoch(s) (sweep won the race)"
    else
        grep -q "re-joined FRESH after its self-fence" "$STATE/m${victim_reader}.log" ||
            die "neither the owner's dead-epoch mint nor the reader's fenced-then-fresh re-join happened — the S6->S7 handoff is broken on BOTH arms"
        log "reader re-presented FRESH before the owner's sweep could expire its dead lease (the finding-#1 fix's path; the mint arm is rung 7's proven row)"
    fi

    sleep 2
    for i in $(member_idxs); do snap "$i" 1 "$rowdir"; done

    # --- the leg gate --------------------------------------------------------
    python3 - "$rowdir" "$victim_reader" "$mint_seen" <<'PYGATE'
import glob, json, re, sys

rowdir, victim_reader, mint_seen = sys.argv[1], sys.argv[2], sys.argv[3] == "1"

def flat(d, out=None, pfx=""):
    out = {} if out is None else out
    for k, v in d.items():
        if isinstance(v, dict):
            flat(v, out, pfx + k + ".")
        else:
            out[pfx + k] = v
    return out

def load(p):
    root = json.load(open(p))
    return flat(root.get("metrics", root))

bad = []
for p1 in sorted(glob.glob(f"{rowdir}/m*_p1.json")):
    i = re.match(r".*/m(\d+)_p1", p1).group(1)
    d1, d0 = load(p1), load(f"{rowdir}/m{i}_p0.json")
    dd = lambda k: int(d1.get(k, 0)) - int(d0.get(k, 0))
    fence = dd("data_dma_fence_refusals")
    epoch = dd("data_dma_epoch_refusals")
    quarantined = int(d1.get("dlm_quarantined_offsets", 0))
    releases = dd("dlm_quarantine_releases")
    trip = dd("invariant_tripwires")
    backstops = dd("mem_budget_hard_backstops")
    gate_to = dd("parked_gate_timeouts")
    evict = dd("membership_evictions")
    if i == "0":
        # The VICTIM: fenced, refusing, epoch class ⊆ fence class (0 here
        # BY CONSTRUCTION — no custody moved inside the zombie's process:
        # no term bump, no revoked grant; the fence is the POISON latch).
        if fence < 1:
            bad.append(f"m0 (victim): data_dma_fence_refusals delta {fence} < 1")
        if not (0 <= epoch <= fence):
            bad.append(f"m0 (victim): epoch_refusals {epoch} not within [0, fence {fence}]")
        if epoch != 0:
            bad.append(f"m0 (victim): epoch_refusals {epoch} != 0 — nothing advanced the zombie's custody generation in this scoped posture")
        if mint_seen and evict < 1:
            bad.append(f"m0: membership_evictions delta {evict} < 1 — a dead-epoch mint was observed without its eviction")
        if int(d1.get("write_pipeline_fence_drops", 0)) < 0:
            bad.append("m0: fence_drops went negative (counter corruption)")
    else:
        # The BLAST RADIUS: no fence movement anywhere but the victim.
        if fence != 0 or epoch != 0:
            bad.append(f"m{i}: data-plane fence/epoch refusals moved ({fence}/{epoch}) on a non-victim")
        if int(d1.get("data_plane_fence_mode", 0)) != 0:
            bad.append(f"m{i}: data_plane_fence_mode != 0 on a reader")
    # The QUARANTINE law (scoped): a READER's dead epoch names no offsets,
    # so the gauge stays 0 and NOTHING was released without a drain proof.
    # The offset-holding cohorts (S9 custody grants, job-wire destinations)
    # are rungs 9-10; their no-release-without-a-proof law is pinned in
    # cargo (tests/dlm_data_fence_tests.rs).
    if quarantined != 0:
        bad.append(f"m{i}: dlm_quarantined_offsets={quarantined} — nothing at this rung may hold offsets")
    if releases != 0:
        bad.append(f"m{i}: dlm_quarantine_releases moved ({releases}) with no drain proof issued")
    if trip != 0:
        bad.append(f"m{i}: invariant_tripwires moved ({trip})")
    if backstops != 0 or gate_to != 0:
        bad.append(f"m{i}: R5 columns moved (backstops={backstops}, gate_timeouts={gate_to})")

if bad:
    print("S7-a GATE FAILED:", file=sys.stderr)
    for b in bad:
        print(f"  {b}", file=sys.stderr)
    sys.exit(1)
print("S7-a GATE GREEN (device rejection + zombie fail-stop + preempt observed; quarantine law held; blast radius = the victim)")
PYGATE

    # --- restore the fleet ----------------------------------------------------
    # The zombie is DEAD BY DESIGN (a fenced holder is dead until remount):
    # kill it, clear the recovery hold (safe — the zombie is gone), remount
    # the writer fresh (a fresh WERO under a fresh key), and require the
    # reader to re-join the new owner.
    "$MWFLEET" kill 0 --sig 9 || true
    sleep 1
    s7_release_recovery_hold
    log "recovery reservation released + recovery identity unregistered (the zombie is dead; the successor takes its own hold)"
    umount -l "$w_mnt" 2>/dev/null || true
    wait_for_unmounted "$w_mnt"
    "$MWFLEET" mount 0
    [ "$(stat_field 0 data_plane_fence_mode)" = "1" ] ||
        die "restored writer did not re-arm the WERO hold"
    local ttl_s=$((ttl / 1000))
    wait_stat_eq "$victim_reader" membership_mode member $((ttl_s + 6 * renew_est + 90)) "reader re-join of the restored owner"
    trap - EXIT
    log "s7-device-fence leg GREEN (writer restored, reader re-joined; rows + device truth in $rowdir)"
}

wait_for_unmounted() { # mountpoint
    local t
    for t in $(seq 1 120); do
        : "$t"
        mountpoint -q "$1" || return 0
        sleep 0.5
    done
    die "$1 never unmounted"
}

# The symmetric fleet's precondition (PR 10): `mw_fleet.sh create
# --symmetric` recorded it, and the manager's stats say the plane is armed.
require_symmetric() {
    # Not `require_mw`: a symmetric fleet arms the S6 plane (the death
    # ledger's writer) and the D0 guard's WERO on a PR substrate without
    # the S9 multi-writer opt-in — `--symmetric` alone is the shape.
    require_membership
    [ "${SYMMETRIC:-0}" = "1" ] ||
        die "this leg needs a SYMMETRIC fleet — create it with: sudo tests/mw_fleet.sh create N=2 --symmetric [--lease-ttl-ms=15000]"
    [ "$(stat_all_eq 0 writer_guard_mode flock+pr)" = "1" ] ||
        die "member 0 writer_guard_mode != flock+pr on every volume — the metadata PR the death path's preempt fences is not held (a non-PR substrate?)"
    [ "$(stat_all_eq 0 symmetric_meta 1)" = "1" ] ||
        die "member 0 symmetric_meta != 1 on every volume — the symmetric plane is not armed on the manager (SQUEEZEFS_SYMMETRIC_META=1 on a bit-17 set)"
    [ "$(stat_all_eq 0 manager_lease held)" = "1" ] ||
        die "member 0 manager_lease != held on every volume — the D0 winner of a symmetric set IS its manager (KD-SYM-3)"
}

leg_sym_crash() {
    require_symmetric
    s7_kill_body sym
}

# The joined-writer member indices of a symmetric fleet (PR 12b —
# `mw_fleet.sh create --symmetric --writers=K`; empty on the one-appender
# shape).
joiner_idxs() {
    awk -F'\t' '$2=="joiner" {print $1}' "$MEMBERS" 2>/dev/null | sort -n
}

# One acked-writes oracle writer (the s7 kill body's, made a function so
# the N-daemon legs run one per writer mount): per-file `dd conv=fsync`
# into `dir`, every name whose fsync RETURNED appended to `ledger`; runs
# until killed. Content `<tag>:<i>`.
ack_writer() { # dir ledger tag
    local dir="$1" ledger="$2" tag="$3" i f
    mkdir -p "$dir"
    : >"$ledger"
    i=0
    while :; do
        f="$dir/f$(printf '%06d' "$i")"
        if printf '%s:%s\n' "$tag" "$i" | dd of="$f" conv=fsync status=none 2>/dev/null; then
            echo "$f" >>"$ledger"
        fi
        i=$((i + 1))
    done
}

# The oracle's verdict over one ledger, read through `read_mnt` (a path
# under `orig_mnt` is re-rooted): every acked name present with content.
# Prints the lost count; the misses go to `lostfile`.
ack_verify() { # ledger tag orig_mnt read_mnt lostfile
    local ledger="$1" tag="$2" orig="$3" read_mnt="$4" lostfile="$5" f g want got lost=0 kind
    while IFS= read -r f; do
        [ -n "$f" ] || continue
        g="$read_mnt${f#"$orig"}"
        # The stat's OWN error travels into the finding (round 4: a
        # "LOST (absent)" that was 36 EAGAIN refusals at one survivor read
        # as lost data until the daemon logs said otherwise) — an absent
        # name is ENOENT, a refused read names its errno.
        if ! kind="$(stat -c %F "$g" 2>&1 >/dev/null)"; then
            lost=$((lost + 1))
            echo "LOST (absent: ${kind##*: }): $g" >>"$lostfile"
            continue
        fi
        kind="$(stat -c %F "$g" 2>/dev/null)"
        if [ "$kind" != "regular file" ]; then
            lost=$((lost + 1))
            echo "LOST (not a regular file: $kind): $g" >>"$lostfile"
            continue
        fi
        want="$tag:$((10#${f##*/f}))"
        got="$(cat "$g" 2>/dev/null || true)"
        if [ "$got" != "$want" ]; then
            lost=$((lost + 1))
            echo "LOST (content '$got' != '$want'): $g" >>"$lostfile"
        fi
    done <"$ledger"
    echo "$lost"
}

# **sym-storm (PR 12b)**: every RW daemon of the symmetric fleet — the
# manager and K joined writers — runs the acked-writes oracle into its
# own directory at once; at a randomized phase ONE JOINER is killed -9;
# the manager's S6 eviction records its death after the lease TTL, the
# ledger poll RECOVERS its region (its ring replayed into the slot trees,
# its slots unleased — PR 10's driver on a REAL second daemon); every
# acked name of every writer is then present with content at the manager
# AND at a surviving joiner (the dead writer's through the recovery, the
# live writers' through their holders' tokens); the killed writer
# remounts at the same point and joins a FRESH region (§5.8.3 — a
# Recovered ring is never rejoined), reading every name too; online
# fsck clean; the symmetric must-stay-0 set flat on every daemon. Per
# round; COUNTED-RESTART discipline applies.
#
# WHAT THE GREEN FSCK ROW PROVES (PR 12b review round 2, Issue 27): the
# block plane (C2–C8, the C8 oracle and the bitmap oracle) and C1 over
# every tree but the LIVE lessees' — while joiners live their slot trees
# are projections at the censusing mount and are SCOPED OUT
# (`fsck_c1_projection_slots_scoped`), and the inode plane (C9/C10)
# records NO verdict over a volume with a slot leased to a live appender
# (every row's transcript reads "the inode plane covered 0 of 2 volumes —
# this pass is INCOMPLETE"). A clean row here is NOT a C9/C10 verdict;
# the inode plane's verdict on this fleet is the census at each lessee
# over its own slots (PR 13's shipped census), or a run with every joiner
# left.
leg_sym_storm() {
    require_symmetric
    local joiners victims survivors m_mnt rowdir round ttl_ms phase_ms nvol
    mapfile -t joiners < <(joiner_idxs)
    [ "${#joiners[@]}" -ge 2 ] ||
        die "sym-storm needs a symmetric fleet with ≥ 2 joined writers — create it with: sudo tests/mw_fleet.sh create N=2 --symmetric --writers=3 --lease-ttl-ms=15000"
    # K = every joiner is gate 4 (e) — "N appenders at once": the MANAGER
    # is then the surviving reader of the oracle (PR 13).
    [[ "$SYM_VICTIMS" =~ ^[1-9][0-9]*$ ]] && [ "$SYM_VICTIMS" -le "${#joiners[@]}" ] ||
        die "sym-storm --victims=$SYM_VICTIMS needs 1 ≤ K ≤ joiners (${#joiners[@]})"
    m_mnt="$(mnt_of 0)"
    rowdir="$STATE/rows/symstorm-$(date +%s)"
    mkdir -p "$rowdir"
    ttl_ms="$(stat_field 0 membership_lease_ttl_ms)"
    # The regions a dead joiner holds = one per metadata volume it leases
    # slots on (every joiner's rotor spans every volume of the set).
    nvol="$(echo "$META_PATHS" | tr ',' '\n' | grep -c .)"
    # The recalled-reader arm (gate 4's last clause, PR 13): a `-o ro`
    # TOKEN reader holds a token on a victim's object across the kill; the
    # successor lessee (the manager, after the recovery) mutates that
    # object and the reader is RECALLED before it can serve the stale
    # word — `dlm_token_recalls_received` moves and the next resolve is
    # exact. Only on a `--token-readers` fleet.
    local token_reader=""
    if [ "${TOKEN_READERS:-0}" = "1" ]; then
        token_reader="$(member_idxs | awk '$1!=0' | head -1)"
    fi
    log "sym-storm: ${#joiners[@]} joined writer(s) + the manager write under the acked-writes oracle$([ "${SYM_XO:-0}" = "1" ] && echo ' + a cross-owner MOVER per joiner (its acked files renamed into a directory the MANAGER holds — a victim dies mid-plan, the intent rolls forward)')$([ "${SYM_STRIPED:-0}" = "1" ] && echo " + every storm directory STRIPED (K=$SYM_STRIPE_K) before its creators start — a victim dies with inserts in flight on its stripes (gate 4 d', C17 judged after)")$([ -n "$token_reader" ] && echo " + the recalled-reader arm on m$token_reader"); $SYM_VICTIMS joiner(s) killed -9 AT ONCE per round, recovered by the manager's ledger (lease TTL ${ttl_ms} ms, $nvol region(s) each); x$S7_ROUNDS rounds"
    printf '%-6s %-9s %-14s %-10s %-10s %-8s %s\n' ROUND PHASE_MS VICTIMS RECOVER_S ACKED LOST VERDICT | tee "$rowdir/matrix.tsv"
    for ((round = 1; round <= S7_ROUNDS; round++)); do
        victims=()
        survivors=()
        local j k
        for ((k = 0; k < SYM_VICTIMS; k++)); do
            victims+=("${joiners[$(((round - 1 + k) % ${#joiners[@]}))]}")
        done
        for j in "${joiners[@]}"; do
            local is_victim=0
            for k in "${victims[@]}"; do [ "$j" = "$k" ] && is_victim=1; done
            [ "$is_victim" = "1" ] || survivors+=("$j")
        done
        # The writers: one oracle per RW mount (the manager's is m0's); with
        # --cross-owner a mover per joiner beside it.
        local pids=() idx dir ledger xo_dir
        xo_dir="$m_mnt/storm-xo-r$round"
        [ "${SYM_XO:-0}" = "1" ] && mkdir -p "$xo_dir"
        for idx in 0 "${joiners[@]}"; do
            dir="$(mnt_of "$idx")/storm-w$idx-r$round"
            ledger="$rowdir/acked-w$idx-r$round.ledger"
            if [ "${SYM_STRIPED:-0}" = "1" ]; then
                # Gate 4 (d'): the writer's directory is flipped to K
                # stripes by its HOLDER (the explicit flip is holder-local
                # until PR 12 — the creator mints the directory, so it is
                # the holder) before the creators start; the kill lands
                # with inserts and migrations in flight on the stripes.
                mkdir -p "$dir" || die "round $round: mkdir $dir"
                setfattr -n user.squeezefs.stripes -v "$SYM_STRIPE_K" "$dir" ||
                    die "round $round: the explicit stripe flip of $dir (K=$SYM_STRIPE_K) refused on m$idx"
                [ "$(getfattr -n user.squeezefs.stripes --only-values "$dir" 2>/dev/null)" = "$SYM_STRIPE_K" ] ||
                    die "round $round: $dir does not read back K=$SYM_STRIPE_K stripes"
            fi
            ack_writer "$dir" "$ledger" "w$idx:r$round" &
            pids+=($!)
            if [ "${SYM_XO:-0}" = "1" ] && [ "$idx" != "0" ]; then
                xo_mover "$dir" "$(mnt_of "$idx")/storm-xo-r$round" "w$idx" "$rowdir/moved-w$idx-r$round.ledger" &
                pids+=($!)
            fi
        done
        phase_ms=$((2000 + RANDOM % 6000))
        sleep "$(python3 -c "print($phase_ms/1000)")"
        # The recalled-reader arm's TOKEN: the reader resolves one acked
        # object of the first victim (a getattr = one inode token from the
        # victim's plane) before the kill.
        local rr_obj="" rr_recalls0=0 rr_grants0=0
        if [ -n "$token_reader" ]; then
            rr_obj="$(head -1 "$rowdir/acked-w${victims[0]}-r$round.ledger" 2>/dev/null || true)"
            rr_obj="${rr_obj#"$(mnt_of "${victims[0]}")"}"
            if [ -n "$rr_obj" ]; then
                rr_grants0="$(stat_sum "$token_reader" dlm_token_grants)"
                local rr_hits0
                rr_hits0="$(stat_sum "$token_reader" dlm_token_hits)"
                # Under `--cross-owner` the victim's MOVER renames every
                # acked file into the manager's directory concurrently:
                # the object is at its source or at its destination
                # (`ack_verify_xo`'s law) — the reader resolves whichever
                # holds it now (both are foreign tokens for the reader).
                local rr_dst=""
                [ "${SYM_XO:-0}" = "1" ] && rr_dst="/storm-xo-r$round/w${victims[0]}-$(basename "$rr_obj")"
                if ! timeout 60 stat "$(mnt_of "$token_reader")$rr_obj" >/dev/null 2>&1; then
                    if [ -n "$rr_dst" ] && timeout 60 stat "$(mnt_of "$token_reader")$rr_dst" >/dev/null 2>&1; then
                        rr_obj="$rr_dst"
                    else
                        die "round $round: the token reader m$token_reader could not resolve victim m${victims[0]}'s acked object $rr_obj${rr_dst:+ (nor its moved form $rr_dst)} before the kill"
                    fi
                fi
                # The arm's premise is that the reader HOLDS a token on the
                # object it resolved: a fetch (`dlm_token_grants`) or a serve
                # from a token it already held (`dlm_token_hits` — the
                # storm's earlier rounds left the reader holding `/` and
                # the round's directories) both satisfy it; only a resolve
                # that touched NO token is the arm's failure.
                if ! { [ "$(stat_sum "$token_reader" dlm_token_grants)" -gt "$rr_grants0" ] ||
                    [ "$(stat_sum "$token_reader" dlm_token_hits)" -gt "$rr_hits0" ]; } 2>/dev/null; then
                    die "round $round: the token reader's resolve of $rr_obj touched no token (dlm_token_grants flat at $rr_grants0, dlm_token_hits flat at $rr_hits0)"
                fi
                rr_recalls0="$(stat_sum "$token_reader" dlm_token_recalls_received)"
            fi
        fi
        # Every daemon's .stats MID-STORM, bounded (round 3: a round whose
        # first ops parked 17 s at every daemon acked 25 files and died at
        # its oracle with no gauge of the stall on record — the stall's
        # recall / guard / ship faces are read off these).
        for idx in 0 "${joiners[@]}"; do
            timeout 15 cat "$(mnt_of "$idx")/.stats" >"$rowdir/stats-m$idx-r$round-storm.json" 2>/dev/null || true
        done
        local recov0 t_kill t_rec
        recov0="$(stat_sum 0 appender_recoveries)"
        for k in "${victims[@]}"; do
            "$MWFLEET" kill "$k" --sig 9
        done
        t_kill="$(date +%s)"
        # Stop every oracle (the victims' died with their daemons; the
        # ledgers hold what was ACKED).
        for p in "${pids[@]}"; do
            kill -9 "$p" 2>/dev/null || true
            wait "$p" 2>/dev/null || true
        done
        for k in "${victims[@]}"; do
            umount -l "$(mnt_of "$k")" 2>/dev/null || true
            wait_for_unmounted "$(mnt_of "$k")"
        done
        # The recovery: the S6 eviction past the lease TTL, the record, the
        # poll's projection — EVERY region of EVERY victim (K × volumes),
        # bounded by TTL + the recovery bound + slack.
        local bound_ms deadline want
        bound_ms="$(stat_sum 0 appender_recovery_bound_ms)"
        deadline=$(((ttl_ms + bound_ms * SYM_VICTIMS) / 1000 + 90))
        want=$((recov0 + SYM_VICTIMS * nvol))
        local t0 now v
        t0="$(date +%s)"
        while :; do
            v="$(stat_sum 0 appender_recoveries)"
            [ "$v" -ge "$want" ] 2>/dev/null && break
            now="$(date +%s)"
            [ $((now - t0)) -lt "$deadline" ] ||
                die "round $round: the manager never recovered every region of joiner(s) ${victims[*]} (appender_recoveries $recov0 → $v, want $want within ${deadline}s; dead_members_recorded=$(stat_sum 0 dead_members_recorded) acted=$(stat_sum 0 dead_members_acted))"
            sleep 1
        done
        t_rec="$(date +%s)"
        log "round $round: ${#victims[@]} region set(s) RECOVERED by the manager $((t_rec - t_kill)) s after the kill (appender_recoveries $recov0 → $v; dead_members_acted=$(stat_sum 0 dead_members_acted))"
        # The intent register SETTLES before the oracle judges (PR 13
        # review round 2, Issue 25): PR 10's driver runs
        # `roll_forward_open_intents` AFTER the per-region recoveries that
        # move `appender_recoveries`, and PR 6's law completes a cross-owner
        # rename the kill caught mid-plan FORWARD — a name at NEITHER home
        # while its intent is still OPEN is the S3.5 lattice's designed
        # transient, never a loss. Bounded by the landing ceiling × a few +
        # the stuck grace (`CLIENT_STALE_TTL_SECS` = 45 s); past it the die
        # names `xv_cross_owner_intents_stuck`. The three gauges are
        # snapshotted per round beside the `.stats` files.
        local ceil_ms xv_open xv_stuck xv_rolled xv_deadline xv_t0
        ceil_ms="$(stat_sum 0 appender_flush_ceiling_ms)"
        ceil_ms=$((ceil_ms / (nvol > 0 ? nvol : 1)))
        [ "$ceil_ms" -gt 0 ] 2>/dev/null || ceil_ms=1100
        xv_deadline=$((ceil_ms * 8 / 1000 + 45 + 30))
        xv_t0="$(date +%s)"
        while :; do
            xv_open="$(stat_sum 0 xv_cross_owner_intents_open)"
            [ "$xv_open" = "0" ] && break
            now="$(date +%s)"
            [ $((now - xv_t0)) -lt "$xv_deadline" ] ||
                die "round $round: $xv_open cross-owner intent(s) still OPEN at the manager ${xv_deadline}s after the recovery (xv_cross_owner_intents_stuck=$(stat_sum 0 xv_cross_owner_intents_stuck), recovery_intents_rolled_forward=$(stat_sum 0 recovery_intents_rolled_forward)) — the roll-forward never settled"
            sleep 1
        done
        xv_stuck="$(stat_sum 0 xv_cross_owner_intents_stuck)"
        xv_rolled="$(stat_sum 0 recovery_intents_rolled_forward)"
        printf 'round=%s xv_cross_owner_intents_open=%s xv_cross_owner_intents_stuck=%s recovery_intents_rolled_forward=%s settle_s=%s\n' \
            "$round" "$xv_open" "$xv_stuck" "$xv_rolled" "$(($(date +%s) - xv_t0))" >>"$rowdir/intents-r$round.txt"
        log "round $round: intent register SETTLED (xv_cross_owner_intents_open $xv_open, stuck $xv_stuck, recovery_intents_rolled_forward $xv_rolled — $(($(date +%s) - xv_t0)) s after the recovery)"
        # The oracle at the MANAGER and at one survivor, over EVERY writer's
        # ledger (the victims' names through the recovery); with the mover
        # a name is at its source OR its cross-owner destination, never
        # both, never neither — and every RETURNED mv is at the destination.
        # A name at neither home whose `mv` did NOT return is judged
        # against the intent register (`ack_verify_xo`'s last argument):
        # open ⇒ "in flight", not LOSS (the register read 0 above, so here
        # every such name IS a loss — the two populations stay split in the
        # lost file's labels for the attribution recipe, §4.4af).
        local acked=0 lost=0 n l
        local -a oracle_readers=(0)
        [ "${#survivors[@]}" -gt 0 ] && oracle_readers+=("${survivors[0]}")
        for idx in 0 "${joiners[@]}"; do
            ledger="$rowdir/acked-w$idx-r$round.ledger"
            n="$(wc -l <"$ledger" | tr -d ' ')"
            acked=$((acked + n))
            for reader_idx in "${oracle_readers[@]}"; do
                if [ "${SYM_XO:-0}" = "1" ] && [ "$idx" != "0" ]; then
                    l="$(ack_verify_xo "$ledger" "w$idx:r$round" "$(mnt_of "$idx")" "$(mnt_of "$reader_idx")" "$rowdir/lost-r$round.txt" "/storm-xo-r$round" "w$idx" "$rowdir/moved-w$idx-r$round.ledger" "$xv_open")"
                else
                    l="$(ack_verify "$ledger" "w$idx:r$round" "$(mnt_of "$idx")" "$(mnt_of "$reader_idx")" "$rowdir/lost-r$round.txt")"
                fi
                lost=$((lost + l))
            done
        done
        # `${survivors[0]}` is UNBOUND under `set -u` when every joiner is a
        # victim (`--victims=K` = all): the die itself died on it and the
        # acked-writes red read as a harness syntax error (fix round 1,
        # batch 2 round 4).
        [ "$lost" = "0" ] ||
            die "round $round: ACKED-WRITES ORACLE RED — $lost miss(es) over $acked fsynced file(s) across the manager and ${survivors[0]:+joiner }${survivors[0]:-the manager} (see $rowdir/lost-r$round.txt)"
        log "round $round: acked-writes oracle GREEN ($acked fsynced file(s) from ${#joiners[@]} joiners + the manager, all present at the manager and at ${survivors[0]:+joiner }${survivors[0]:-the manager})"
        # The recalled-reader arm's VERDICT: the recovered slot's lessee
        # is the manager now; its first mutation of the object the reader
        # holds a token on RECALLS the reader (the conveyor pass's hook,
        # PR 5) before the commit, and the reader's next resolve is the
        # new word — never the cached one. A setattr keeps the name (the
        # ledgers below still verify it).
        if [ -n "$rr_obj" ]; then
            # The mover may have re-homed it since the reader's pre-kill
            # resolve (the xo law: at its source or at its destination).
            if [ ! -e "$m_mnt$rr_obj" ] && [ -n "${rr_dst:-}" ] && [ -e "$m_mnt$rr_dst" ]; then
                rr_obj="$rr_dst"
            fi
            # The reader HOLDS the object's token at the mutation: its
            # pre-kill token died with the victim's plane (fail-closed,
            # dropped), and whether the oracle's read above re-fetched
            # one — or the R5 cache evicted it since — is the reader's
            # business, not the arm's premise. One resolve here (a hit or
            # a fresh grant from the recovered slot's lessee), THEN the
            # recall count the mutation must move.
            timeout 60 stat "$(mnt_of "$token_reader")$rr_obj" >/dev/null 2>&1 ||
                die "round $round: the token reader m$token_reader could not resolve $rr_obj at the successor before the mutation"
            rr_recalls0="$(stat_sum "$token_reader" dlm_token_recalls_received)"
            chmod 0600 "$m_mnt$rr_obj" ||
                die "round $round: the manager could not setattr the recovered object $rr_obj"
            local rr_mode rr_recalls
            rr_mode="$(timeout 60 stat -c %a "$(mnt_of "$token_reader")$rr_obj" 2>/dev/null || echo TIMEOUT)"
            [ "$rr_mode" = "600" ] ||
                die "round $round: the token reader m$token_reader reads mode $rr_mode for $rr_obj after the successor's setattr (want 600 — a stale token served)"
            rr_recalls="$(stat_sum "$token_reader" dlm_token_recalls_received)"
            [ "$rr_recalls" -gt "$rr_recalls0" ] 2>/dev/null ||
                die "round $round: the token reader's dlm_token_recalls_received is flat at $rr_recalls0 across the successor's mutation of $rr_obj (the recovered slot's lessee recalled nobody)"
            log "round $round: recalled-reader arm GREEN (reader m$token_reader recalled $((rr_recalls - rr_recalls0))×, reads the successor's setattr exactly)"
        fi
        # The victims remount: a FRESH region each (a Recovered ring is
        # never rejoined), every name readable there too.
        for k in "${victims[@]}"; do
            "$MWFLEET" mount "$k" ||
                die "round $round: joiner $k's remount FAILED"
            v="$(stat_sum "$k" appender_self_recoveries)"
            [ "$v" = "0" ] ||
                die "round $round: the remounted joiner $k recovered its own residue ($v) — a Recovered ring was rejoined (§5.8.3)"
        done
        lost=0
        for idx in 0 "${joiners[@]}"; do
            ledger="$rowdir/acked-w$idx-r$round.ledger"
            if [ "${SYM_XO:-0}" = "1" ] && [ "$idx" != "0" ]; then
                l="$(ack_verify_xo "$ledger" "w$idx:r$round" "$(mnt_of "$idx")" "$(mnt_of "${victims[0]}")" "$rowdir/lost-r$round.txt" "/storm-xo-r$round" "w$idx" "$rowdir/moved-w$idx-r$round.ledger" "$(stat_sum 0 xv_cross_owner_intents_open)")"
            else
                l="$(ack_verify "$ledger" "w$idx:r$round" "$(mnt_of "$idx")" "$(mnt_of "${victims[0]}")" "$rowdir/lost-r$round.txt")"
            fi
            lost=$((lost + l))
        done
        [ "$lost" = "0" ] || die "round $round: the remounted joiner ${victims[0]} misses $lost acked name(s)"
        # Every daemon's .stats BEFORE the fsck verdict (review round 1,
        # Issue 9: round 3's attribution had to be reconstructed from INFO
        # lines because the verdict died first).
        for idx in 0 "${joiners[@]}"; do
            cat "$(mnt_of "$idx")/.stats" >"$rowdir/stats-m$idx-r$round.json" 2>/dev/null || true
        done
        # fsck at the manager — BOUNDED (Issue 13: a wedged fleet worker
        # hung the coordinator's fsck for 31 min; the census's own progress
        # deadline terminates the job now, and the harness bounds the verb
        # the way the matrix's watchdog bounds a suite); its output is kept
        # whatever the verdict. The manager's inode-plane census takes NO
        # verdict over a volume with a slot leased to a LIVE joiner (Issue
        # 1 — a projection's staleness it cannot bound; counted on
        # `fsck_inode_plane_foreign_dentry_scoped`), so the verdict here is
        # honest with joiners live: the block classes judged, the dentry
        # classes deferred to a census with no foreign lessee.
        # The verb's status is read WITHOUT `set -e` exiting the leg on a
        # failing substitution (round 4: a red fsck died here before its
        # transcript was kept — the finding read only in the daemon log).
        local out rc=0
        out="$(timeout 900 "$SQZ" fsck "$m_mnt" 2>&1)" || rc=$?
        echo "$out" >"$rowdir/fsck-r$round.out"
        [ "$rc" != "124" ] || die "round $round: online fsck HUNG past 900 s (the fleet census never terminated) — transcript $rowdir/fsck-r$round.out"
        [ "$rc" = "0" ] || die "round $round: online fsck FAILED or found:
$out"
        echo "$out" | grep -q "findings: 0" || die "round $round: fsck findings != 0:
$out"
        sym_storm_daemon_asserts "$round" 0
        for j in "${joiners[@]}"; do
            sym_storm_daemon_asserts "$round" "$j"
        done
        # PR 13 (gate 4's per-kill set): the C8 / bitmap oracles and the
        # replay's torn count on every daemon; `dead_members_acted ≡
        # recorded × regions` on the manager (the closure).
        for idx in 0 "${joiners[@]}"; do
            [ "$(stat_sum "$idx" meta_kv_block_refs_drift)" = "0" ] || die "round $round: meta_kv_block_refs_drift != 0 on m$idx"
            [ "$(stat_sum "$idx" meta_kv_replay_dropped_torn)" = "0" ] || die "round $round: meta_kv_replay_dropped_torn != 0 on m$idx"
        done
        # Gate 4 (d'): the stripe census (C17, report-only) is clean after
        # a holder died with inserts in flight on its stripes — read off
        # the manager's fsck, which walks the striped population.
        if [ "${SYM_STRIPED:-0}" = "1" ]; then
            local c17
            c17="$(stat_sum 0 fsck_stripe_findings)"
            [ "$c17" = "0" ] || die "round $round: fsck_stripe_findings=$c17 after the striped storm (C17 must stay 0)"
        fi
        local recorded acted
        recorded="$(stat_sum 0 dead_members_recorded)"
        acted="$(stat_sum 0 dead_members_acted)"
        [ "$acted" = "$((recorded * nvol))" ] ||
            warn "round $round: dead_members_acted=$acted vs recorded × regions = $((recorded * nvol)) (a record whose regions are still being released, or a rejoin's retirement — read beside the recovery log)"
        for idx in 0 "${joiners[@]}"; do
            rm -rf "$(mnt_of "$idx")/storm-w$idx-r$round" 2>/dev/null || true
        done
        rm -rf "$xo_dir" 2>/dev/null || true
        # "Deleted stays deleted" (Issue 1's oracle half): every round
        # directory just removed must be GONE through every mount — the
        # manager, the survivors and the remounted victims — never a name
        # a stale projection still serves.
        # Every stat is BOUNDED: a lookup that parks (the round-2 run: a
        # live joiner's lookup of a removed directory whose child's holder
        # had died and rejoined parked 24 min at station [entry]) is a red
        # with its daemon named, never a leg that hangs.
        local reader_idx stale=0 verdict
        for idx in 0 "${joiners[@]}"; do
            for reader_idx in 0 "${joiners[@]}"; do
                # The EXPECTED verdict is a failing `stat` (ENOENT): the
                # classifier reads its status inside its own condition — a
                # bare `timeout … stat` under `set -e` exited the leg
                # silently right here (the round-3 re-runs' `rc 1` with no
                # line after the fsck). ONE classifier for every arm
                # (`sym_stat_deleted`): only ENOENT is "deleted" — an EIO /
                # EAGAIN here is a daemon that cannot answer (the round-1
                # run: a joiner whose volumes FAIL-STOPPED read every
                # removed name as "gone" and the leg counted it green —
                # defect 29).
                verdict="$(sym_stat_deleted "$(mnt_of "$reader_idx")/storm-w$idx-r$round" 60)"
                case "$verdict" in
                hung) die "round $round: stat of removed storm-w$idx-r$round through m$reader_idx HUNG past 60 s — a parked lookup (m$reader_idx's log: fuse_op_watchdog_overdue)" ;;
                resurrected)
                    stale=$((stale + 1))
                    echo "STALE: m$reader_idx still resolves storm-w$idx-r$round" >>"$rowdir/stale-r$round.txt"
                    ;;
                deleted) ;;
                error:*) die "round $round: stat of removed storm-w$idx-r$round through m$reader_idx failed with something other than ENOENT: ${verdict#error:}" ;;
                esac
            done
        done
        [ "$stale" = "0" ] || die "round $round: $stale removed round directory(ies) still resolve through a mount (see $rowdir/stale-r$round.txt)"
        printf '%-6s %-9s %-14s %-10s %-10s %-8s %s\n' "$round" "$phase_ms" "m${victims[*]}" "$((t_rec - t_kill))" "$acked" 0 GREEN | tee -a "$rowdir/matrix.tsv"
    done
    log "sym-storm GREEN: $S7_ROUNDS/$S7_ROUNDS rounds (table + fsck reports in $rowdir)"
}

# The cross-owner MOVER (PR 13 — gate 4 (c), "a node mid-cross-owner-
# rename"): renames the acked-writes oracle's files out of `src` (the
# joiner's own slot tree) into `dst` — a directory the MANAGER holds, so
# every `rename` is PR 6's cross-owner intent (the dentry insert shipped to
# the holder, the removal local) — one at a time until killed; every mv
# that RETURNED is appended to `ledger` as its destination name.
xo_mover() { # src dst prefix ledger
    local src="$1" dst="$2" prefix="$3" ledger="$4" f
    : >"$ledger"
    while :; do
        for f in "$src"/f*; do
            [ -e "$f" ] || continue
            if mv "$f" "$dst/$prefix-$(basename "$f")" 2>/dev/null; then
                echo "$dst/$prefix-$(basename "$f")" >>"$ledger"
            fi
        done
        sleep 0.05
    done
}

# `ack_verify` under the mover: an acked name is at its SOURCE or at its
# cross-owner DESTINATION (`<dst_rel>/<prefix>-<name>`) — exactly one of the
# two, with its content — and every mv the mover's ledger says RETURNED is
# at the destination (a plan the kill caught rolls FORWARD, never back).
# A name at NEITHER home is split into the two populations the attribution
# recipe judges apart (PR 13 review round 2, Issue 25; record §4.4af): its
# `mv` RETURNED (its destination in `moved_ledger` — an acked rename PR 6's
# law says was durable at its holder before the ack: LOST whatever the
# timing) or did NOT return (the kill caught the rename mid-plan: LOST once
# the intent register reads 0 — the caller settles on it first — and
# "IN-FLIGHT", NOT a loss, while `intents_open` > 0, the S3.5 lattice's
# designed transient the roll-forward completes).
ack_verify_xo() { # ledger tag orig_mnt read_mnt lostfile dst_rel prefix moved_ledger [intents_open]
    local ledger="$1" tag="$2" orig="$3" read_mnt="$4" lostfile="$5" dst_rel="$6" prefix="$7" moved="$8" intents_open="${9:-0}"
    local f g h want got lost=0 at_src at_dst returned why_g why_h
    while IFS= read -r f; do
        [ -n "$f" ] || continue
        g="$read_mnt${f#"$orig"}"
        h="$read_mnt$dst_rel/$prefix-$(basename "$f")"
        at_src=0
        at_dst=0
        [ -f "$g" ] && at_src=1
        [ -f "$h" ] && at_dst=1
        if [ $((at_src + at_dst)) != 1 ]; then
            # The mover's ledger holds DESTINATION paths (`xo_mover`); the
            # acked ledger holds the source — match on the fixed-width name.
            returned=0
            grep -qE -- "/${prefix}-$(basename "$f")\$" "$moved" 2>/dev/null && returned=1
            if [ "$at_src" = "0" ] && [ "$at_dst" = "0" ] && [ "$returned" = "0" ] && [ "${intents_open:-0}" != "0" ]; then
                echo "IN-FLIGHT (xo: src=0 dst=0, mv NOT returned, xv_cross_owner_intents_open=$intents_open — the roll-forward's window, not a loss): $g | $h" >>"$lostfile"
                continue
            fi
            # The ERRNO behind each absence (PR 13b, §4.4af's attribution
            # recipe): a name whose `stat` fails EIO is a READ the daemon
            # could not serve (a stale projection, a dead holder), not a
            # record that is gone — the two are different defects.
            why_g="$(stat -c %F "$g" 2>&1 >/dev/null | sed 's/^stat: //')"
            why_h="$(stat -c %F "$h" 2>&1 >/dev/null | sed 's/^stat: //')"
            lost=$((lost + 1))
            if [ "$returned" = "1" ]; then
                echo "LOST (xo RETURNED mv — acked rename, P0: src=$at_src dst=$at_dst; src stat: ${why_g:-ok}; dst stat: ${why_h:-ok}): $g | $h" >>"$lostfile"
            else
                echo "LOST (xo UNRETURNED mv, xv_cross_owner_intents_open=$intents_open: src=$at_src dst=$at_dst; src stat: ${why_g:-ok}; dst stat: ${why_h:-ok}): $g | $h" >>"$lostfile"
            fi
            continue
        fi
        want="$tag:$((10#${f##*/f}))"
        [ "$at_src" = "1" ] && got="$(cat "$g" 2>/dev/null || true)" || got="$(cat "$h" 2>/dev/null || true)"
        if [ "$got" != "$want" ]; then
            lost=$((lost + 1))
            echo "LOST (xo content '$got' != '$want'): $g | $h" >>"$lostfile"
        fi
    done <"$ledger"
    while IFS= read -r f; do
        [ -n "$f" ] || continue
        h="$read_mnt$dst_rel/$(basename "$f")"
        if [ ! -f "$h" ]; then
            lost=$((lost + 1))
            why_h="$(stat -c %F "$h" 2>&1 >/dev/null | sed 's/^stat: //')"
            echo "LOST (xo: a RETURNED mv is not at its destination; dst stat: ${why_h:-ok}): $h" >>"$lostfile"
        fi
    done <"$moved"
    echo "$lost"
}

# The symmetric rows' LAWS — the venue word, the must-stay-0 set, the
# snapshot readers, the deleted-stays-deleted classifier and the gate
# 2 / 3 / 3b verdicts — live in tests/sym_rows_lib.sh, sourced here AND by
# the multi-node cloud driver (tests/cloud_sym_rows.sh, PR 15): ONE law per
# row on every venue. The lib needs die/log/warn (above), SYM_VENUE and
# the venue-attributed ledger's path.
# shellcheck disable=SC2034  # read by the lib's sym_zero_venue_note
SYM_VENUE_LEDGER="$STATE/rows/venue-attributed.txt"
# shellcheck disable=SC2034  # the lib's stderr prefix — this harness's tag
SYM_LOG_TAG="[mwmatrix]"
# shellcheck source=tests/sym_rows_lib.sh
. "$REPO/tests/sym_rows_lib.sh"

# The symmetric must-stay-0 set on one daemon of the N-daemon fleet (the
# manager's `sym_crash_round_asserts` set plus the Joined family's).
sym_storm_daemon_asserts() { # round idx
    local round="$1" idx="$2" k v
    cat "$(mnt_of "$idx")/.stats" >"$STATE/rows/stats-m$idx-r$round.json" 2>/dev/null || true
    for k in meta_kv_forest_key_violations appender_fence_breach foreign_frame_overwrite_detected \
        manager_verb_refusals meta_kv_replay_key_violations meta_kv_replay_lease_violations \
        meta_kv_replay_extent_violations fsck_slot_custody_conflicts \
        appender_park_expiries meta_kv_leaf_lease_refusals dlm_token_recall_timeouts_live \
        appender_flush_ceiling_overruns dead_member_write_deferrals data_alloc_bitmap_drift \
        joined_control_refusals xv_cross_owner_intents_stuck invariant_tripwires; do
        v="$(stat_sum "$idx" "$k")"
        sym_zero_judge "round $round" "$idx" "$k" "$v"
    done
    [ "$(stat_all_eq "$idx" symmetric_meta 1)" = "1" ] ||
        die "round $round: symmetric_meta != 1 on every volume of m$idx"
}

# ===========================================================================
# Symmetric PR 13 — the ACCEPTANCE legs (design-symmetric-metadata §8 gates
# 2 / 3 / 3b / 3c / 5). Every leg prints its engagement law's gauges beside
# its number and exits nonzero when a law is violated: a row without its
# engagement is INVALID, never a number. Dev-box walls are SCOPING; the
# counted brackets run on squeeze-test (the venue law, AGENTS.md).
# ===========================================================================

# The mdstorm driver (tests/mdstorm.c — T threads, one phase), built once
# per leg into the fleet's state dir. The create rows drive it with T
# threads per writer so the DAEMON, not a single-threaded client, is the
# bottleneck the N-scaling law reads.
SYM_STORM=""
sym_build_storm() {
    SYM_STORM="$STATE/mdstorm"
    cc -O2 -pthread -o "$SYM_STORM" "$REPO/tests/mdstorm.c" || die "cc tests/mdstorm.c failed"
}

# (`sym_delta` — the Σ-folded per-volume snapshot delta — is the lib's;
# `s8a_delta` reads scalars only.)

# (`SYM_ZERO_KEYS` — the must-stay-0 set, `dlm_rpcs` asserted ABSOLUTE by
# the rows — is the lib's.)

# The must-stay-0 set on one daemon as a REPORT: prints every violated
# gauge as `key=value` (nothing on a clean daemon). The rows that publish a
# table read it so a violated law lands IN the row's VERDICT column with
# its number beside the row's rates, instead of a die before the row
# prints (sym-scale N = 8's first read of `appender_flush_ceiling_overruns`
# had no row to stand beside).
sym_zero_violations() { # idx
    local idx="$1" k v
    for k in $SYM_ZERO_KEYS; do
        v="$(stat_sum "$idx" "$k")"
        [ "$v" = "0" ] && continue
        if sym_zero_venue_attributed "$k"; then
            sym_zero_venue_note report "$idx" "$k" "$v"
            continue
        fi
        printf '%s=%s ' "$k" "$v"
    done
}

# (`sym_zero_violations_delta` — the set judged as a per-row DELTA — is
# the lib's.)

sym_zero_set() { # label idx
    local label="$1" idx="$2" k v
    for k in $SYM_ZERO_KEYS; do
        v="$(stat_sum "$idx" "$k")"
        sym_zero_judge "$label" "$idx" "$k" "$v"
    done
    # A READER arms no slot leases (`symmetric_meta` is the writer's
    # posture word); its posture word is `reader_staleness_bound_ms == 0`.
    if [ "$(role_of "$idx")" = "reader" ]; then
        [ "$(stat_field "$idx" reader_staleness_bound_ms)" = "0" ] ||
            die "$label: reader_staleness_bound_ms != 0 on reader m$idx (R-SYM-4)"
    else
        [ "$(stat_all_eq "$idx" symmetric_meta 1)" = "1" ] ||
            die "$label: symmetric_meta != 1 on every volume of m$idx"
    fi
}

# The mean of one exact-sum phase histogram family's `total` (a per-volume
# array of `{phase: {mean_ns, …}}`), in µs — the number a row prints beside
# the whole table it keeps in its snapshot.
sym_phase_mean_us() { # idx key
    stat_field "$1" "$2" | python3 -c '
import ast, sys
v = ast.literal_eval(sys.stdin.read().strip() or "None")
vols = v if isinstance(v, list) else [v]
tot = [x["total"]["mean_ns"] for x in vols if isinstance(x, dict) and "total" in x and x["total"].get("count", 0)]
print(f"{max(tot)/1000:.1f}" if tot else "0")'
}

# One writer's CPU ticks (utime + stime, /proc/<pid>/stat) — the manager's
# CPU share row.
sym_cpu_ticks() { # idx
    local pid
    pid="$(awk -F'\t' -v i="$1" '$1==i {print $7}' "$MEMBERS" 2>/dev/null)"
    [ -n "$pid" ] && [ -r "/proc/$pid/stat" ] || {
        echo 0
        return
    }
    awk '{print $14 + $15}' "/proc/$pid/stat"
}

# The AGENTS write-amplification instrument on the sym legs' WRITE rows
# (the box re-run's §3.9.4.5 owed item): the DATA namespaces' /proc/diskstats
# read + write columns snapshotted per phase, and the row's device bytes ÷
# user bytes and `wareq-sz` printed beside the daemons' ledgers. The fleet's
# data namespaces are the devsub's nvmet-tcp host-side devices (the same
# face gate 1's fio rows read on the fabric).
sym_disk_snap() { # rowdir phase
    local out="$1/disk_p$2.tsv" p
    : >"$out"
    local IFS=,
    for p in $FORMAT_DATA_PATHS; do
        awk -v d="$(basename "$p")" '$3==d {print "data", d, $4, $6, $8, $10}' /proc/diskstats >>"$out"
    done
}
# Prints `dev_w/user=<x> wareq_kib=<k> dev_r/user=<y> dev_wbytes=<b>` from
# the phase's two snapshots against `user_bytes` (the row's submitted user
# bytes); a snapshot with no data device line prints `dev_w/user=n/a`.
sym_disk_amp() { # rowdir phase user_bytes
    python3 - "$1/disk_p${2}0.tsv" "$1/disk_p${2}1.tsv" "$3" <<'PYEOF'
import sys
def load(p):
    d = {}
    for line in open(p):
        f = line.split()
        if len(f) == 6 and f[0] == "data":
            d[f[1]] = [int(x) for x in f[2:]]
    return d
a, b = load(sys.argv[1]), load(sys.argv[2])
user = float(sys.argv[3])
rios = wios = rsect = wsect = 0
for dev, y in b.items():
    x = a.get(dev, [0, 0, 0, 0])
    rios += y[0] - x[0]; rsect += y[1] - x[1]; wios += y[2] - x[2]; wsect += y[3] - x[3]
if not b:
    print("dev_w/user=n/a wareq_kib=n/a dev_r/user=n/a dev_wbytes=0")
else:
    wb, rb = wsect * 512, rsect * 512
    print(f"dev_w/user={wb/max(1.0,user):.3f} wareq_kib={(wb/1024/wios if wios else 0):.0f} "
          f"dev_r/user={rb/max(1.0,user):.3f} dev_wbytes={wb}")
PYEOF
}

# F-B1's faces per writer, read off the row's END snapshot (PR 13e, the
# cadence derivation — `meta_kv_checkpoint_{term,trigger}_ms` beside the
# audit's gauges): `overruns` Σ over volumes (must stay 0 — the row's law
# reads it as a delta; this is the absolute), the anticipated term (max
# over volumes) and the trigger in force (min), the excused Σ.
sym_fb1_faces() { # rowdir label idx...
    local rowdir="$1" label="$2"
    shift 2
    local idx
    for idx in "$@"; do
        python3 - "$rowdir/m${idx}_p${label}1.json" "$idx" <<'PYEOF'
import json, sys
def flat(d, out, pfx=""):
    for k, v in d.items():
        if isinstance(v, dict): flat(v, out, pfx + k + ".")
        else: out[pfx + k] = v
    return out
root = json.load(open(sys.argv[1]))
d = flat(root.get("metrics", root), {})
def arr(k):
    v = d.get(k, 0)
    return [x for x in v if isinstance(x, (int, float))] if isinstance(v, list) else [v] if isinstance(v, (int, float)) else [0]
ov, term, trig, exc = arr("appender_flush_ceiling_overruns"), arr("meta_kv_checkpoint_term_ms"), arr("meta_kv_checkpoint_trigger_ms"), arr("appender_flush_ceiling_excused_ns")
# PR 13g's second term beside the horizon term: the LIVE projection off the
# pending work, the decision's raw lateness, the measured units per class.
proj, late = arr("meta_kv_checkpoint_projected_ms"), arr("meta_kv_checkpoint_late_max_ms")
unode, uimg = arr("meta_kv_checkpoint_node_unit_ns"), arr("meta_kv_checkpoint_image_unit_ns")
print(f"   F-B1 m{sys.argv[2]}: flush_ceiling_overruns={sum(ov)} checkpoint_term_ms={term} checkpoint_trigger_ms={trig} excused_ns={sum(exc)} ceiling_ms={d.get('appender_flush_ceiling_ms')}"
      f" projected_ms={proj} late_max_ms={late} node_unit_us={[round(x/1000, 1) for x in unode]} image_unit_us={[round(x/1000, 1) for x in uimg]}")
PYEOF
    done
}

# F-R5's faces per writer (PR 13g — the joiner's extent supply): the ring
# above the 512 KiB floor (`appender_ring_bytes`, its grows / segments, the
# declines), the grant as a POOL (`extent_grant_{claimed,returned,unclaimed}`
# absolute — a joiner's `granted` is not a published face, so the closure
# is read SET-WIDE against the manager's `extent_grant_extents`), and the
# storm's wire economy as DELTAS over the phase `p<label>0 → p<label>1`:
# returned vs the compactions (claim-and-retire churn reads returned ≈
# compactions), the wire grants (≈ the derived asks, not ≈ 100 per storm),
# the reactive asks, the pressure cycles vs the checkpoints; the new
# must-stay-0 / hygiene gauges beside them. The manager's line carries its
# verb / grant / return deltas and `manager_service_ns.execute` per volume.
sym_fr5_faces() { # rowdir label idx...
    local rowdir="$1" label="$2"
    shift 2
    local idx
    for idx in "$@"; do
        python3 - "$rowdir" "$idx" "$label" <<'PYEOF'
import json, sys
rowdir, idx, label = sys.argv[1:4]
def flat(d, out=None, pfx=""):
    out = {} if out is None else out
    for k, v in d.items():
        if isinstance(v, dict): flat(v, out, pfx + k + ".")
        else: out[pfx + k] = v
    return out
def load(ph):
    root = json.load(open(f"{rowdir}/m{idx}_p{label}{ph}.json"))
    return flat(root.get("metrics", root))
a, b = load(0), load(1)
def arr(d, k):
    v = d.get(k, 0)
    if isinstance(v, list): return [x for x in v if isinstance(x, (int, float))]
    return [v] if isinstance(v, (int, float)) else [0]
def delta(k): return [y - x for x, y in zip(arr(a, k), arr(b, k))] if len(arr(a, k)) == len(arr(b, k)) else [sum(arr(b, k)) - sum(arr(a, k))]
def dsum(k): return sum(delta(k))
def s(k): return arr(b, k)
ring = s("appender_ring_bytes")
line = (f"   F-R5 m{idx}: ring_kib={[x // 1024 for x in ring]} ring_grows={s('appender_ring_grows')} ring_segments={s('appender_ring_segments')}"
        f" grow_declined={s('joined_ring_grow_declined')} short_declines={s('appender_grow_ring_short_declines')}"
        f" pool[claimed,returned,unclaimed]={s('extent_grant_claimed')},{s('extent_grant_returned')},{s('extent_grant_unclaimed')}"
        f" Δreturned={delta('extent_grant_returned')} Δcompactions={dsum('meta_kv_node_compactions')} Δnode_images={dsum('meta_kv_node_images')}"
        f" Δwire_grants={delta('joined_wire_extent_grants')} Δreactive={delta('joined_wire_reactive_grants')} Δwire_returns={delta('joined_wire_extent_returns')} Δwire_ring_grows={delta('joined_wire_ring_grows')}"
        f" Δpressure_cycles={delta('appender_pressure_cycles')} Δcheckpoints={dsum('meta_kv_checkpoints')}"
        f" pool_restored={s('appender_pool_restored_extents')} pending_segs_returned={s('appender_pending_segments_returned')} join_residue={s('appender_join_residue_returned')}"
        f" stale_page_words={s('appender_stale_page_words_dropped')} return_run_cap_refusals={s('extent_return_run_cap_refusals')} joined_control_refusals={s('joined_control_refusals')}"
        # The hygiene faces the fourth box pass read moving OUTSIDE the
        # must-stay-0 set (absolute at the snapshot): a reclaim whose
        # destroy was WITHHELD on a failed pricing read, a frame the §5.8.2
        # screen ended a log at, a projection root refreshed on a
        # recycled extent (defect 18 / 34's class).
        f" hygiene[reclaim_destroy_refused={s('reclaim_destroy_refused_release_failed')} foreign_frames_screened={s('foreign_frames_screened')}"
        f" projection_root_refreshes={s('meta_kv_projection_root_refreshes')} xv_intents_open={s('xv_cross_owner_intents_open')}]")
# The manager's terms (the joiner's wire asks land here): the verb wall.
# `manager_service_ns` is a per-volume list of dicts, kept whole by `flat`.
def svc(d):
    v = d.get("manager_service_ns")
    if isinstance(v, list): return [x.get("execute", 0) if isinstance(x, dict) else 0 for x in v]
    return []
ex_a, ex_b = svc(a), svc(b)
dsvc = [round((y - x) / 1e9, 3) for x, y in zip(ex_a, ex_b)] if ex_a and len(ex_a) == len(ex_b) else []
if idx == "0":
    line += (f" | manager: Δmanager_verbs={delta('manager_verbs')} Δextent_grants={delta('extent_grants')} Δextent_grant_extents={delta('extent_grant_extents')}"
             f" Δextent_returns={delta('extent_returns')} Δservice_execute_s={dsvc} Δdirectory_reads={dsum('appender_directory_reads')}"
             f" appenders_known={s('appenders_known')} ring_budget_remaining_kib={[x // 1024 for x in s('appender_ring_budget_remaining_bytes')]}")
print(line)
PYEOF
    done
}

# The create row's per-writer storm timeline off the stamps the spawn
# loop wrote (`launch-n<N>.tsv` + each `create-n<N>-w<idx>.txt`'s
# `launch_ts= end_ts=` line): `m<idx>[+launch, +end, wall]` in seconds
# relative to the row's clock start, each setup mkdir's wall, and the
# Σ of the per-writer storm rates — an UPPER bound on the storms' own
# concurrency (exact when every storm ran the whole row beside the
# others; a staggered storm ran part of its wall with fewer competitors),
# beside the table's law, which reads the row's wall from its clock.
sym_scale_launch_offsets() { # rowdir n t0 rate1 idx...
    local rowdir="$1" n="$2" t0="$3" rate1="$4"
    shift 4
    python3 - "$rowdir" "$n" "$t0" "$rate1" "$@" <<'PYEOF'
import re, sys
rowdir, n, t0, rate1 = sys.argv[1], sys.argv[2], float(sys.argv[3]), float(sys.argv[4])
out, rates, mk = [], [], []
for line in open(f"{rowdir}/launch-n{n}.tsv"):
    f = line.split()
    if len(f) == 4 and f[0] == "mkdir":
        mk.append(f"m{f[1]} {float(f[3]) - float(f[2]):.3f}s")
for idx in sys.argv[5:]:
    txt = open(f"{rowdir}/create-n{n}-w{idx}.txt").read()
    m = re.search(r"launch_ts=([\d.]+) end_ts=([\d.]+)", txt)
    w = re.search(r"wall_s=([\d.]+)", txt)
    o = re.search(r"ops=(\d+)", txt)
    if w and o and float(w.group(1)) > 0:
        rates.append(int(o.group(1)) / float(w.group(1)))
    if not m:
        out.append(f"m{idx}[?]")
        continue
    out.append(f"m{idx}[+{float(m.group(1))-t0:.2f}, +{float(m.group(2))-t0:.2f}, {w.group(1) if w else '?'}]")
bound = sum(rates)
print(" ".join(out) + f"; setup mkdir walls: {', '.join(mk) or 'n/a'}; Σ per-writer rates {bound:.0f} creates/s = {bound / max(rate1, 1):.2f}× the N=1 rate (an upper bound)")
PYEOF
}

# The quiet-box gate the measured rows share (the s10pl leg's).
# SQZ_MWMATRIX_ALLOW_BUSY=1 turns the refusal into a loud WARN for a
# mechanism-only SCOPING run (the row's numbers are then labelled busy and
# are never acceptance evidence — the venue law).
sym_quiet_or_die() { # label
    local busy=""
    if pgrep -x cargo >/dev/null 2>&1 || pgrep -x rustc >/dev/null 2>&1; then
        busy="a cargo/rustc build is running"
    fi
    local load
    load="$(awk '{print int($1)}' /proc/loadavg)"
    [ "$load" -le 8 ] || busy="${busy:+$busy; }loadavg $load > 8"
    [ -n "$busy" ] || return 0
    if [ "${SQZ_MWMATRIX_ALLOW_BUSY:-0}" = "1" ]; then
        warn "$1: $busy — SQZ_MWMATRIX_ALLOW_BUSY=1: the row runs as MECHANISM SCOPING ONLY (its walls are not evidence)"
        SYM_BUSY_ROW=" [BUSY-SCOPING]"
        return 0
    fi
    die "$1: $busy — a measured row needs a quiet box (SQZ_MWMATRIX_ALLOW_BUSY=1 for a mechanism-only scoping run)"
}
SYM_BUSY_ROW=""

# The per-daemon fsck oracle + C8 + the must-stay-0 set after a measured
# sweep (BOUNDED, its transcript kept whatever the verdict).
sym_oracle() { # label rowdir
    local label="$1" rowdir="$2" out rc=0 idx
    out="$(timeout 900 "$SQZ" fsck "$(mnt_of 0)" 2>&1)" || rc=$?
    echo "$out" >"$rowdir/fsck-$label.out"
    [ "$rc" != "124" ] || die "$label: online fsck HUNG past 900 s — transcript $rowdir/fsck-$label.out"
    [ "$rc" = "0" ] || die "$label: online fsck FAILED or found:
$out"
    echo "$out" | grep -q "findings: 0" || die "$label: fsck findings != 0:
$out"
    for idx in 0 $(joiner_idxs); do
        [ "$(stat_sum "$idx" meta_kv_block_refs_drift)" = "0" ] ||
            die "$label: meta_kv_block_refs_drift != 0 on m$idx (C8 oracle RED)"
        sym_zero_set "$label" "$idx"
    done
    log "$label: oracle clean (fsck findings 0, C8 drift 0, the must-stay-0 set flat on every writer)"
}

# Exactly the joiners `want...` mounted (gate 3's "exactly N appenders
# live" law): every other joiner LEAVES cleanly (its page Free), a wanted
# one not up JOINS; then the manager's directory must count N Live pages.
sym_ensure_joiners() { # n want_idx...
    local n="$1" j want
    shift
    for j in $(joiner_idxs); do
        want=0
        for w in "$@"; do [ "$w" = "$j" ] && want=1; done
        if [ "$want" = "1" ]; then
            mountpoint -q "$(mnt_of "$j")" || "$MWFLEET" mount "$j" || die "joiner $j (re)mount failed"
        else
            if mountpoint -q "$(mnt_of "$j")"; then
                "$MWFLEET" unmount "$j" || die "joiner $j unmount failed"
                wait_for_unmounted "$(mnt_of "$j")"
            fi
        fi
    done
    local t
    for t in $(seq 1 60); do
        : "$t"
        [ "$(stat_all_eq 0 appenders_known "$n")" = "1" ] && return 0
        sleep 1
    done
    die "the manager's appender directory never read $n Live page(s) (appenders_known=$(stat_field 0 appenders_known)) — a joiner's leave or join did not land"
}

# A named-prefix creator (mdstorm's names collide across writers into ONE
# directory — the shared-dir rows need `<prefix>-<i>`): `count` files
# under `dir`, the count that landed on stdout.
sym_prefixed_create() { # dir prefix count
    python3 - "$1" "$2" "$3" <<'PYEOF'
import os, sys
d, pfx, n = sys.argv[1], sys.argv[2], int(sys.argv[3])
ok = 0
for i in range(n):
    try:
        fd = os.open(f"{d}/{pfx}-{i:07d}", os.O_CREAT | os.O_WRONLY | os.O_EXCL, 0o644)
        os.close(fd)
        ok += 1
    except OSError as e:
        sys.stderr.write(f"create {pfx}-{i}: {e}\n")
        break
print(ok)
PYEOF
}

# ACKED WRITES PRESENT (the lib's laws, PR 15 review round 1, Issue 11):
# a tree the writer acked ≡ the tree ANOTHER mount reads (entries + bytes);
# an fsynced ingest file read back whole through another mount, all zero.
sym_tree_census() { # path -> "entries bytes"
    python3 -c "$SYM_TREE_CENSUS_PY" "$1"
}
sym_acked_tree_check() { # label writer_mnt via_idx rel
    local label="$1" w_mnt="$2" via="$3" rel="$4" we wb ge gb
    read -r we wb <<<"$(sym_tree_census "$w_mnt$rel")"
    read -r ge gb <<<"$(sym_tree_census "$(mnt_of "$via")$rel")"
    sym_law_acked_tree "$label" "$we" "$ge" "$wb" "$gb" "m$via"
    log "$label: acked writes present — $we entries / $wb bytes read back identical through m$via"
}
# The s8a_venue hook for the sym-tarx leg: the extracting mount's tree read
# back through the OTHER side (the manager for the joiner's arm, the first
# joiner for the manager's).
sym_tarx_census_hook() { # label mnt rel
    local label="$1" mnt="$2" rel="$3" via
    if [ "$mnt" = "$(mnt_of 0)" ]; then via="$SYM_TARX_JW"; else via=0; fi
    sym_acked_tree_check "sym-tarx $label" "$mnt" "$via" "$rel"
}
SYM_TARX_JW=""

# --- gate 2: sym-tarx ------------------------------------------------------
leg_sym_tarx() {
    require_symmetric
    local joiners
    mapfile -t joiners < <(joiner_idxs)
    [ "${#joiners[@]}" -ge 1 ] ||
        die "sym-tarx needs a symmetric fleet with ≥ 1 joined writer — create it with: sudo tests/mw_fleet.sh create N=2 --symmetric --writers=1"
    sym_quiet_or_die sym-tarx
    local src="${SQZ_MWMATRIX_TAR_SRC:-}"
    { [ -n "$src" ] && [ -d "$src" ]; } ||
        die "sym-tarx: the gate row requires the REAL linux-src tree — set SQZ_MWMATRIX_TAR_SRC=<linux>/fs (design §5.10: the linux fs/ corpus, 2,384 entries)"
    local rowdir jw w_mnt jw_mnt tarball entries
    rowdir="$STATE/rows/symtarx-$(date +%s)"
    mkdir -p "$rowdir"
    jw="${joiners[0]}"
    w_mnt="$(mnt_of 0)"
    tarball="$STATE/symtarx-src.tar"
    tar -cf "$tarball" -C "$(dirname "$src")" "$(basename "$src")"
    entries="$(tar -tf "$tarball" | wc -l)"
    log "sym-tarx instrument: REAL tree $src ($entries entries); venue = joined writer m$jw in a netns at netem 125us/end (250 µs wire RTT) extracting into a directory IT created, vs the manager-local S0; A-B-B-A"
    # The joiner re-mounts inside its netns: a clean leave (its slots
    # released, its page Free) and a fresh join over the shaped wire — the
    # join ladder itself at 250 µs RTT is part of what the row prices.
    "$MWFLEET" unmount "$jw" || die "sym-tarx: joiner unmount failed"
    wait_for_unmounted "$(mnt_of "$jw")"
    "$MWFLEET" mount "$jw" --netns || die "sym-tarx: netns joiner mount failed"
    "$MWFLEET" netem "$jw" 125us || die "sym-tarx: netem failed"
    jw_mnt="$(mnt_of "$jw")"
    SYM_TARX_JW="$jw"
    S8A_VENUE_CENSUS_HOOK=sym_tarx_census_hook

    sym_arm() { # label -> row line
        local label="$1" out wire xv ship pub verbs_per h_j h_m rpcs
        out="$(s8a_venue "$rowdir" "$label" "$jw_mnt" "$jw" "$entries" "$tarball")"
        wire="$(sym_delta "$rowdir" "$jw" "$label" joined_wire_verbs)"
        xv="$(sym_delta "$rowdir" "$jw" "$label" xv_cross_owner_steps_shipped)"
        ship="$(sym_delta "$rowdir" "$jw" "$label" meta_ship.shipped_verbs)"
        pub="$(sym_delta "$rowdir" "$jw" "$label" meta_ship_publish.shipped)"
        h_j="$(sym_delta "$rowdir" "$jw" "$label" slot_handovers)"
        h_m="$(sym_delta "$rowdir" 0 "$label" slot_handovers)"
        rpcs="$(stat_field "$jw" dlm_rpcs)"
        # THE ENGAGEMENT LAW (§8 gate 2) — the lib's, one definition for
        # every venue: verbs/entry < 0.05, no handover, dlm_rpcs 0.
        verbs_per="$(sym_law_gate2_engagement "$label" "$entries" "$wire" "$xv" "$ship" "$pub" "$h_j" "$h_m" "$rpcs")"
        echo "$out wire=$wire xv=$xv ship=$ship pub=$pub verbs/entry=$verbs_per handovers=0"
    }
    local_arm() { # label -> row line (the S0 shape)
        local label="$1" out
        out="$(s8a_venue "$rowdir" "$label" "$w_mnt" "" "$entries" "$tarball")"
        echo "$out local-S0"
    }
    local -a rows=()
    rows+=("$(sym_arm sym-1)")
    rows+=("$(local_arm local-1)")
    rows+=("$(local_arm local-2)")
    rows+=("$(sym_arm sym-2)")
    S8A_VENUE_CENSUS_HOOK=""
    "$MWFLEET" netem "$jw" off || true

    echo ""
    echo "== PR 13 gate 2: tar -x on a JOINED WRITER @250us RTT vs manager-local S0 (entries=$entries; A-B-B-A; tier: $(hostname) $(uname -r) — dev-box rows are SCOPING) =="
    printf '%-10s %-8s %-8s %s\n' ARM WALL_S OPS_S ENGAGEMENT
    local r
    for r in "${rows[@]}"; do
        # shellcheck disable=SC2086 # deliberate word split of the row line
        printf '%-10s %-8s %-8s %s\n' $r
    done | tee "$rowdir/symtarx-table.txt"
    local s1 s2 l1 l2
    s1="$(echo "${rows[0]}" | awk '{print $2}')"
    l1="$(echo "${rows[1]}" | awk '{print $2}')"
    l2="$(echo "${rows[2]}" | awk '{print $2}')"
    s2="$(echo "${rows[3]}" | awk '{print $2}')"
    sym_law_gate2_verdict "$s1" "$s2" "$l1" "$l2" | tee "$rowdir/symtarx-verdict.txt"
    sym_oracle sym-tarx "$rowdir"
    log "sym-tarx PUBLISHED (table + verdict + snapshots in $rowdir)"
}

# --- gate 3: sym-scale -----------------------------------------------------
leg_sym_scale() {
    require_symmetric
    sym_build_storm
    local joiners ns maxn
    mapfile -t joiners < <(joiner_idxs)
    IFS=',' read -r -a ns <<<"$SYM_SCALE_NS"
    maxn=0
    for n in "${ns[@]}"; do [ "$n" -gt "$maxn" ] && maxn="$n"; done
    [ "${#joiners[@]}" -ge $((maxn - 1)) ] ||
        die "sym-scale N=$maxn needs $((maxn - 1)) joined writers (found ${#joiners[@]}) — create the fleet with: sudo tests/mw_fleet.sh create N=2 --symmetric --writers=$((maxn - 1))"
    sym_quiet_or_die sym-scale
    local rowdir
    rowdir="$STATE/rows/symscale-$(date +%s)"
    mkdir -p "$rowdir"
    log "sym-scale: N ∈ {${ns[*]}} RW mounts each creating $SYM_FILES files ($SYM_THREADS threads) in its OWN directory, then ingesting $SYM_INGEST_MB MiB (4 MiB blocks, conv=fsync); exactly N appenders live per row"
    # C/CPU-S (PR 13c, gate 3's venue term): the create phase's creates per
    # DAEMON-CPU-second, Σ over the row's writers — the co-located venue's
    # reading of the ≥ 0.7 × N law (design §8 gate 3: on one box the
    # writers share the cores, so the wall-clock multiple is bounded by the
    # box, not the mechanism). MGR_CPU is the manager's process CPU over
    # the WHOLE row (create + ingest) — the first build divided the
    # create + ingest CPU by the INGEST wall alone (1,322 % on the box was
    # 9.8 CPU-s ÷ 0.74 s).
    log "sym-scale: the row's clock is the N STORMS' concurrent window (t0 after the row's setup mkdirs, the start snapshots and the manager's CPU baseline at t0; the setup's walls — the N mkdirs under /, the root's flip, a fresh joiner's first create into a striped root — are stamped and printed beside the multiple, never inside it)"
    sym_gate3_header | tee "$rowdir/symscale-table.tsv"
    local n rate1="" ingest1="" verdict_all=MET zero_miss_all=""
    # A per-run tag on every directory: a died run's residue never
    # collides with the next run's creates ("File exists").
    local SYM_RUN
    SYM_RUN="$(date +%s)"
    for n in "${ns[@]}"; do
        local -a writers=(0)
        local i
        for ((i = 0; i < n - 1; i++)); do writers+=("${joiners[$i]}"); done
        sym_ensure_joiners "$n" "${writers[@]:1}"
        sleep 2
        local idx
        local cpu0 t0 t1 t_row0
        # THE ROW'S CLOCK (design §8 gate 3: "aggregate create/s … scale
        # with N" — measured over the N STORMS' concurrent window). The
        # row's SETUP — N `mkdir`s under the shared root (the 3b shape: the
        # root flips at the fifth creator, and on the box each later FRESH
        # joiner's `mkdir -p` into the striped root took ≈ 3.0 s — a 9.1 s
        # skew inside an 18 s wall on the third pass's clock) — runs BEFORE
        # t0, before `cpu0` and before the row's start snapshots, so
        # neither the wall multiple nor `MGR_CPU` / `C/CPU-S` carry it (a
        # job launching N fresh writers into one root pays it once; the
        # finding states it). Every setup mkdir's wall, every storm's
        # LAUNCH and END are stamped, so what remains inside the clock
        # (the spawn loop's own skew) is MEASURED.
        local -a pids=()
        local t_last launch_tsv t_setup0 t_setup1 t_create0
        launch_tsv="$rowdir/launch-n$n.tsv"
        : >"$launch_tsv"
        t_setup0="$(date +%s.%N)"
        for idx in "${writers[@]}"; do
            local t_mk0
            t_mk0="$(date +%s.%N)"
            mkdir -p "$(mnt_of "$idx")/scale-$SYM_RUN-n$n-w$idx"
            printf 'mkdir\t%s\t%s\t%s\n' "$idx" "$t_mk0" "$(date +%s.%N)" >>"$launch_tsv"
        done
        t_setup1="$(date +%s.%N)"
        # The start snapshots and the manager's CPU baseline AT the clock's
        # start (review round 1, Issue 4: sampled before the setup they
        # folded its CPU — the flip, the fresh joiners' waits — into
        # `MGR_CPU` and `C/CPU-S` over the storms' wall alone).
        for idx in "${writers[@]}"; do snap "$idx" "n${n}0" "$rowdir"; done
        cpu0="$(sym_cpu_ticks 0)"
        t0="$(date +%s.%N)"
        t_row0="$t0"
        t_create0="$t0"
        for idx in "${writers[@]}"; do
            t_last="$(date +%s.%N)"
            printf 'launch\t%s\t%s\n' "$idx" "$t_last" >>"$launch_tsv"
            (
                "$SYM_STORM" "$(mnt_of "$idx")/scale-$SYM_RUN-n$n-w$idx" "$SYM_THREADS" "$SYM_FILES" create \
                    >"$rowdir/create-n$n-w$idx.txt" 2>&1
                src=$?
                echo "launch_ts=$t_last end_ts=$(date +%s.%N)" >>"$rowdir/create-n$n-w$idx.txt"
                exit "$src"
            ) &
            pids+=($!)
        done
        local p rc=0
        for p in "${pids[@]}"; do wait "$p" || rc=1; done
        t1="$(date +%s.%N)"
        [ "$rc" = "0" ] || die "sym-scale N=$n: a create storm FAILED (see $rowdir/create-n$n-w*.txt)"
        local create_rate launch_skew setup_wall
        create_rate="$(python3 -c "print(f'{$n*$SYM_FILES/($t1-$t0):.0f}')")"
        launch_skew="$(python3 -c "print(f'{$t_last-$t0:.3f}')")"
        setup_wall="$(python3 -c "print(f'{$t_setup1-$t_setup0:.3f}')")"
        # The create phase's own daemon-CPU face (the snapshot between the
        # two phases — the ingest's CPU never pollutes it).
        for idx in "${writers[@]}"; do snap "$idx" "n${n}c" "$rowdir"; done
        local create_cpu_ns=0 v_cpu creates_per_cpu_s
        for idx in "${writers[@]}"; do
            v_cpu="$(python3 -c "
import json
a=json.load(open('$rowdir/m${idx}_pn${n}0.json'))['metrics']['daemon_cpu_ns']
b=json.load(open('$rowdir/m${idx}_pn${n}c.json'))['metrics']['daemon_cpu_ns']
print(int(b)-int(a))" 2>/dev/null || echo 0)"
            create_cpu_ns=$((create_cpu_ns + v_cpu))
        done
        creates_per_cpu_s="$(python3 -c "print(f'{$n*$SYM_FILES*1e9/max(1,$create_cpu_ns):.0f}')")"
        # The ingest row: 4 MiB blocks, conv=fsync, one file per writer.
        # The data namespaces' /proc/diskstats bracket it (the write row's
        # amplification columns — the AGENTS instrument).
        sym_disk_snap "$rowdir" "in${n}0"
        pids=()
        t0="$(date +%s.%N)"
        for idx in "${writers[@]}"; do
            dd if=/dev/zero of="$(mnt_of "$idx")/scale-$SYM_RUN-n$n-w$idx/ingest.bin" bs=4M \
                count=$((SYM_INGEST_MB / 4)) conv=fsync status=none 2>"$rowdir/ingest-n$n-w$idx.err" &
            pids+=($!)
        done
        for p in "${pids[@]}"; do wait "$p" || rc=1; done
        t1="$(date +%s.%N)"
        sym_disk_snap "$rowdir" "in${n}1"
        [ "$rc" = "0" ] || die "sym-scale N=$n: an ingest dd FAILED (see $rowdir/ingest-n$n-w*.err)"
        local ingest_rate cpu1 mgr_cpu
        ingest_rate="$(python3 -c "print(f'{$n*$SYM_INGEST_MB/($t1-$t0):.0f}')")"
        cpu1="$(sym_cpu_ticks 0)"
        sleep 2
        for idx in "${writers[@]}"; do snap "$idx" "n${n}1" "$rowdir"; done
        # THE ENGAGEMENT LAW (§8 gate 3): every mount in its own slot
        # trees — no handover, no ship, no lock RPC; the manager's load
        # is what its verbs cost, reported per N.
        local handovers=0 ships=0 rpcs=0 v zero_miss=""
        for idx in "${writers[@]}"; do
            v="$(sym_delta "$rowdir" "$idx" "n$n" slot_handovers)"
            handovers=$((handovers + v))
            v="$(sym_delta "$rowdir" "$idx" "n$n" slot_ships)"
            ships=$((ships + v))
            v="$(stat_field "$idx" dlm_rpcs)"
            rpcs=$((rpcs + v))
            v="$(sym_zero_violations_delta "$rowdir" "$idx" "n$n")"
            [ -z "$v" ] || zero_miss="$zero_miss m$idx:{$v}"
        done
        # THE ENGAGEMENT LAW (§8 gate 3) — the lib's: handovers 0, ships
        # ≤ 1 per writer (its directory's mkdir under /), Σ dlm_rpcs 0.
        sym_law_gate3_engagement "$n" "$handovers" "$ships" "$rpcs"
        local mgr_load
        mgr_load="$(stat_field 0 manager_load_pct | tr -d '[] ' | cut -d, -f1)"
        mgr_cpu="$(python3 -c "
import os
hz = os.sysconf('SC_CLK_TCK')
print(f'{100*($cpu1-$cpu0)/hz/max(1e-9, $t1-$t_row0):.0f}')")"
        [ -n "$rate1" ] || rate1="$create_rate"
        [ -n "$ingest1" ] || ingest1="$ingest_rate"
        local cr ir verdict
        cr="$(python3 -c "print(f'{$create_rate/$rate1:.2f}')")"
        ir="$(python3 -c "print(f'{$ingest_rate/$ingest1:.2f}')")"
        verdict="$(sym_law_gate3_row "$n" "$create_rate" "$rate1" "$ingest_rate" "$ingest1")"
        # A violated must-stay-0 law is the row's verdict too — printed
        # with its number, and the leg exits nonzero after the table.
        if [ -n "$zero_miss" ]; then
            verdict="MISS(must-stay-0:$zero_miss)"
            zero_miss_all="$zero_miss_all N=$n:$zero_miss"
        fi
        [ "$verdict" = "MET" ] || verdict_all=MISS
        sym_gate3_row_line "$n" "$create_rate" "$cr" "$creates_per_cpu_s" "$ingest_rate" "$ir" "$mgr_load" "$mgr_cpu" "$handovers" "$ships" "$rpcs" "$verdict" | tee -a "$rowdir/symscale-table.tsv"
        # The write row's amplification columns (the ingest's user bytes =
        # N × SYM_INGEST_MB) + F-B1's per-writer faces on the row's end
        # snapshot — beside the table, never in its verdict.
        {
            echo "   N=$n create setup (the N mkdirs under /, before the clock) ${setup_wall}s; launch skew (first→last storm launch, inside the clock) ${launch_skew}s; per writer [launch+s, end+s, storm wall_s] + the Σ-of-per-writer-rates bound: $(sym_scale_launch_offsets "$rowdir" "$n" "$t_create0" "$rate1" "${writers[@]}")"
            echo "   N=$n ingest amplification (/proc/diskstats, data namespaces; user $((n * SYM_INGEST_MB)) MiB): $(sym_disk_amp "$rowdir" "in$n" $((n * SYM_INGEST_MB * 1024 * 1024)))"
            sym_fb1_faces "$rowdir" "n$n" "${writers[@]}"
            # F-R5 over the whole row (pn<N>0 → pn<N>1) and over the CREATE
            # phase alone (pn<N>0 → pn<N>c — the storm's wire economy).
            sym_fr5_faces "$rowdir" "n$n" "${writers[@]}"
            for idx in "${writers[@]}"; do cp "$rowdir/m${idx}_pn${n}c.json" "$rowdir/m${idx}_pc${n}1.json"; cp "$rowdir/m${idx}_pn${n}0.json" "$rowdir/m${idx}_pc${n}0.json"; done
            echo "   (the create phase alone:)"
            sym_fr5_faces "$rowdir" "c$n" "${writers[@]}"
        } | tee -a "$rowdir/symscale-faces.txt"
        # ACKED WRITES PRESENT: every writer's tree + its fsynced ingest file
        # read back through ANOTHER writer of the row (N ≥ 2) or the token
        # reader; at N = 1 with no reader the manager's own view stands.
        for idx in "${writers[@]}"; do
            local via="" j zb zok
            for j in "${writers[@]}"; do [ "$j" != "$idx" ] && { via="$j"; break; }; done
            [ -n "$via" ] || via="$(awk -F'\t' '$2=="reader" {print $1}' "$MEMBERS" 2>/dev/null | sort -n | head -1)"
            [ -n "$via" ] || { log "sym-scale N=$n m$idx: no other mount to read the acked writes back through (N = 1, no reader)"; continue; }
            sym_acked_tree_check "sym-scale N=$n m$idx" "$(mnt_of "$idx")" "$via" "/scale-$SYM_RUN-n$n-w$idx"
            read -r zb zok <<<"$(python3 -c "$SYM_ZERO_FILE_PY" "$(mnt_of "$via")/scale-$SYM_RUN-n$n-w$idx/ingest.bin")"
            sym_law_acked_ingest "sym-scale N=$n m$idx" "$((SYM_INGEST_MB * 1024 * 1024))" "$zb" "$zok" "m$via"
        done
        for idx in "${writers[@]}"; do
            # A sample of the names about to be removed — the LAST ones the
            # storm created (the "deleted stays deleted" arm below judges
            # them after every writer's CLEAN LEAVE: the in-process pin
            # found a joiner's final tombstones lost across its leave).
            # `ls -U` = readdir order — the order `rm -rf` unlinks in, so
            # the tail IS the storm's last unlinks (the class the pin found).
            ls -U "$(mnt_of "$idx")/scale-$SYM_RUN-n$n-w$idx" 2>/dev/null | tail -200 |
                sed "s|^|/scale-$SYM_RUN-n$n-w$idx/|" >>"$rowdir/removed-sample.txt" || true
            rm -rf "$(mnt_of "$idx")/scale-$SYM_RUN-n$n-w$idx" 2>/dev/null || true
        done
    done
    # The last row's removals are outside every per-row snapshot pair: one
    # more snapshot of every writer after them (`pend0` = the last row's
    # end, `pend1` = now) so the leg's faces cover the between-rows window
    # the fourth box pass found the joiners' spurious reclaims in.
    local maxn_idx
    for maxn_idx in 0 "${joiners[@]:0:$((maxn - 1))}"; do
        cp "$rowdir/m${maxn_idx}_pn${maxn}1.json" "$rowdir/m${maxn_idx}_pend0.json" 2>/dev/null || true
        snap "$maxn_idx" "end1" "$rowdir"
    done
    {
        echo "   after the last row's removals (pn${maxn}1 → pend1):"
        sym_fb1_faces "$rowdir" "end" 0 "${joiners[@]:0:$((maxn - 1))}"
        sym_fr5_faces "$rowdir" "end" 0 "${joiners[@]:0:$((maxn - 1))}"
    } | tee -a "$rowdir/symscale-faces.txt"
    sym_law_gate3_verdict_line "$verdict_all" | tee "$rowdir/symscale-verdict.txt"
    [ -z "$zero_miss_all" ] || die "sym-scale: a must-stay-0 gauge moved:$zero_miss_all (rows above; the leg is RED)"
    # DELETED STAYS DELETED across every joiner's CLEAN LEAVE (PR 13): every
    # joiner unmounts (the product umount = the leave's flush-then-transfer
    # of every slot), then the removed sample is judged through the
    # MANAGER — a name that resolves is a tombstone the leave lost; then
    # through a REMOUNTED joiner (a fresh open of the durable state).
    if [ -s "$rowdir/removed-sample.txt" ]; then
        local j resurrected=0 total
        for j in "${joiners[@]}"; do
            "$MWFLEET" unmount "$j" || die "sym-scale: joiner m$j's clean unmount failed"
        done
        total="$(wc -l <"$rowdir/removed-sample.txt" | tr -d ' ')"
        # ONE classifier for every arm (`sym_stat_deleted`, Issue 8): a
        # manager whose volume FAIL-STOPPED after the leaves answers EIO
        # for every removed name — that is "cannot answer", never
        # "deleted"; only ENOENT counts, anything else dies loud.
        local verdict
        while IFS= read -r rel; do
            [ -n "$rel" ] || continue
            verdict="$(sym_stat_deleted "$(mnt_of 0)$rel" 30)"
            case "$verdict" in
            deleted) ;;
            resurrected)
                resurrected=$((resurrected + 1))
                echo "RESURRECTED at the manager: $rel" >>"$rowdir/resurrected.txt"
                ;;
            hung) die "sym-scale: stat of removed $rel through the manager HUNG past 30 s (a parked lookup)" ;;
            error:*) die "sym-scale: stat of removed $rel through the manager failed with something other than ENOENT: ${verdict#error:}" ;;
            esac
        done <"$rowdir/removed-sample.txt"
        echo "deleted-stays-deleted (manager, after every joiner's clean leave): $resurrected of $total sampled removed names resolve" | tee -a "$rowdir/symscale-verdict.txt"
        "$MWFLEET" mount "${joiners[0]}" || die "sym-scale: joiner m${joiners[0]}'s remount failed"
        local resurrected_j=0
        while IFS= read -r rel; do
            [ -n "$rel" ] || continue
            verdict="$(sym_stat_deleted "$(mnt_of "${joiners[0]}")$rel" 30)"
            case "$verdict" in
            deleted) ;;
            resurrected)
                resurrected_j=$((resurrected_j + 1))
                echo "RESURRECTED at remounted joiner m${joiners[0]}: $rel" >>"$rowdir/resurrected.txt"
                ;;
            hung) die "sym-scale: stat of removed $rel through remounted joiner m${joiners[0]} HUNG past 30 s (a parked lookup)" ;;
            error:*) die "sym-scale: stat of removed $rel through remounted joiner m${joiners[0]} failed with something other than ENOENT: ${verdict#error:}" ;;
            esac
        done <"$rowdir/removed-sample.txt"
        echo "deleted-stays-deleted (remounted joiner m${joiners[0]}): $resurrected_j of $total" | tee -a "$rowdir/symscale-verdict.txt"
        [ "$resurrected" = "0" ] && [ "$resurrected_j" = "0" ] ||
            die "sym-scale: DELETED DID NOT STAY DELETED — $resurrected (manager) / $resurrected_j (remounted joiner) of $total sampled removed names resolve after the joiners' clean leaves (see $rowdir/resurrected.txt)"
    fi
    # Every joiner back up for the legs that follow.
    sym_ensure_joiners $((${#joiners[@]} + 1)) "${joiners[@]}"
    sym_oracle sym-scale "$rowdir"
    # The post-leave census (PR 13e's arm, the foreign-touch leg's): the
    # online fsck above scopes every LIVE lessee's slots out of the inode
    # plane, so a joiner's own trees are never judged while it runs; after
    # every joiner leaves and then the manager, the offline probe judges
    # the set whole (nothing exempted as current-era, findings 0).
    sym_post_leave_census sym-scale "$rowdir" "$SYM_RUN" 0 "${joiners[@]}"
    log "sym-scale PUBLISHED (table + verdict + snapshots in $rowdir)"
}

# --- gate 3b: sym-shared-dir (+ the -ls leg) --------------------------------
leg_sym_shared_dir() {
    require_symmetric
    local joiners
    mapfile -t joiners < <(joiner_idxs)
    [ "${#joiners[@]}" -ge 2 ] ||
        die "sym-shared-dir needs ≥ 2 joined writers (the flip triggers on foreign creates from MORE THAN ONE creator) — create the fleet with: sudo tests/mw_fleet.sh create N=2 --symmetric --writers=3 --token-readers"
    sym_quiet_or_die sym-shared-dir
    local rowdir holder h_mnt shared per_writer idx
    rowdir="$STATE/rows/symshared-$(date +%s)"
    mkdir -p "$rowdir"
    # The HOLDER is the joiner that creates the directory (its inode mints
    # in that joiner's rotor: `/`'s children are slot 0's dentries, the
    # child a rotor mint — §5.1.2); every other writer's create into it
    # is a foreign create the holder serves.
    holder="${joiners[0]}"
    h_mnt="$(mnt_of "$holder")"
    shared="$h_mnt/shared-$(date +%s)"
    mkdir "$shared" || die "sym-shared-dir: the holder's mkdir failed"
    local -a writers=(0 "${joiners[@]}")
    per_writer=$((SYM_FILES / ${#writers[@]}))
    local k_stripes
    k_stripes="$(stat_first_sym 0 slot_rotor)"
    log "sym-shared-dir: ${#writers[@]} creators × $per_writer files into ONE directory held by m$holder; the flip to K stripes (K derives from MINT_SPREAD = $k_stripes) on the holder's observed creator count"
    for idx in "${writers[@]}"; do snap "$idx" "sd0" "$rowdir"; done
    local flips0=0 v
    for idx in "${writers[@]}"; do
        v="$(stat_sum "$idx" dir_stripe_flips)"
        flips0=$((flips0 + v))
    done
    local -a pids=()
    local t0 t1
    t0="$(date +%s.%N)"
    for idx in "${writers[@]}"; do
        sym_prefixed_create "$(mnt_of "$idx")${shared#"$h_mnt"}" "w$idx" "$per_writer" \
            >"$rowdir/shared-w$idx.count" 2>"$rowdir/shared-w$idx.err" &
        pids+=($!)
    done
    local p rc=0
    for p in "${pids[@]}"; do wait "$p" || rc=1; done
    t1="$(date +%s.%N)"
    [ "$rc" = "0" ] || die "sym-shared-dir: a creator FAILED (see $rowdir/shared-w*.err)"
    sleep 3
    for idx in "${writers[@]}"; do snap "$idx" "sd1" "$rowdir"; done
    local created=0 c
    for idx in "${writers[@]}"; do
        c="$(cat "$rowdir/shared-w$idx.count")"
        [ "$c" = "$per_writer" ] || die "sym-shared-dir: m$idx created $c of $per_writer (see $rowdir/shared-w$idx.err)"
        created=$((created + c))
    done
    local listed
    listed="$(ls -f "$shared" | grep -c '^w')"
    [ "$listed" = "$created" ] ||
        die "sym-shared-dir: the directory lists $listed names but $created creates were acked (the striped readdir merge or a lost dentry)"
    # ACKED WRITES PRESENT: the holder's census of the directory ≡ the
    # manager's (a creator reading a foreign holder's directory).
    sym_acked_tree_check sym-shared-dir "$h_mnt" 0 "${shared#"$h_mnt"}"
    # THE ENGAGEMENT LAW (§8 gate 3b): exactly ONE flip, at the holder;
    # the directory striped; every foreign create either a served
    # cross-owner step (pre-flip) or a stripe ship (post-flip) — the
    # shipped steps of the creators ≡ the served steps at the holders
    # (the closure), stripe ships > 0, no handover anywhere.
    local flips=0 flip_at="" striped shipped=0 served=0 stripe_ships=0 handovers=0
    for idx in "${writers[@]}"; do
        v="$(sym_delta "$rowdir" "$idx" sd dir_stripe_flips)"
        [ "$v" = "0" ] || flip_at="${flip_at}m$idx($v) "
        flips=$((flips + v))
        v="$(sym_delta "$rowdir" "$idx" sd xv_cross_owner_steps_shipped)"
        shipped=$((shipped + v))
        v="$(sym_delta "$rowdir" "$idx" sd xv_cross_owner_steps_served)"
        served=$((served + v))
        v="$(sym_delta "$rowdir" "$idx" sd dir_stripe_ships)"
        stripe_ships=$((stripe_ships + v))
        v="$(sym_delta "$rowdir" "$idx" sd slot_handovers)"
        handovers=$((handovers + v))
        sym_zero_set sym-shared-dir "$idx"
    done
    striped="$(stat_sum "$holder" dir_striped_dirs)"
    # K_D (the shared directory's stripes) and K_root (the mount ROOT's —
    # 0 while `/` is unstriped; a fleet whose joiners' `mkdir /…` striped
    # `/` reads K there) through the lib's ONE die-loud reader.
    local xattr_k k_root
    xattr_k="$(sym_stripe_k "$shared")"
    k_root="$(sym_stripe_k "$(mnt_of 0)")"
    echo "== PR 13 gate 3b: ${#writers[@]} creators × $per_writer into ONE directory (holder m$holder): wall $(python3 -c "print(f'{$t1-$t0:.2f}')") s, $(python3 -c "print(f'{$created/($t1-$t0):.0f}')") creates/s aggregate ==" | tee "$rowdir/symshared-table.txt"
    echo "   flips=$flips at [$flip_at] striped_dirs(holder)=$striped K_D=$xattr_k K_root=$k_root xv_shipped=$shipped xv_served=$served dir_stripe_ships=$stripe_ships handovers=$handovers" | tee -a "$rowdir/symshared-table.txt"
    # THE ENGAGEMENT LAW (§8 gate 3b) — the lib's: one flip at the holder,
    # striped, stripe ships > 0, shipped ≡ served, handovers 0.
    sym_law_gate3b_engagement "$holder" "$flips" "$flip_at" "$striped" "$stripe_ships" "$shipped" "$served" "$handovers"
    log "sym-shared-dir: flip at the holder, $stripe_ships stripe ships, closure shipped ≡ served ($shipped), 0 handovers"

    # --- sym-shared-dir-ls: a COLD token reader's `readdir + stat` ----------
    local reader
    reader="$(awk -F'\t' '$2=="reader" {print $1}' "$MEMBERS" 2>/dev/null | sort -n | head -1)"
    if [ "${TOKEN_READERS:-0}" != "1" ] || [ -z "$reader" ]; then
        warn "sym-shared-dir-ls SKIPPED: needs a --token-readers fleet with ≥ 1 reader (the cold readdir + stat row is K stripe tokens + C inode tokens, 0 leaf reads)"
    else
        local r_mnt r_dir
        r_mnt="$(mnt_of "$reader")"
        r_dir="$r_mnt${shared#"$h_mnt"}"
        # COLD: the reader's caches hold nothing of this directory — it was
        # created after the reader mounted and never resolved there. The
        # kernel's own dcache is dropped for good measure (every TTL is 0
        # under tokens anyway).
        sync
        echo 3 >/proc/sys/vm/drop_caches 2>/dev/null || true
        snap "$reader" "ls0" "$rowdir"
        t0="$(date +%s.%N)"
        local statted
        statted="$(ls -l "$r_dir" | grep -c '^-')"
        t1="$(date +%s.%N)"
        snap "$reader" "ls1" "$rowdir"
        [ "$statted" = "$created" ] || die "sym-shared-dir-ls: the reader statted $statted of $created children"
        local grants merges misses hits
        grants="$(sym_delta "$rowdir" "$reader" ls dlm_token_grants)"
        merges="$(sym_delta "$rowdir" "$reader" ls dir_stripe_readdir_merges)"
        misses="$(sym_delta "$rowdir" "$reader" ls meta_kv_node_cache_misses)"
        hits="$(sym_delta "$rowdir" "$reader" ls dlm_token_hits)"
        echo "== sym-shared-dir-ls: cold readdir + stat of $created children over K_D=$xattr_k stripes (root K_root=$k_root) on token reader m$reader: $(python3 -c "print(f'{$t1-$t0:.2f}')") s; dlm_token_grants=$grants (law: K_D + K_root + C = $((xattr_k + k_root + created)), + the directory itself, its parent and the reader's root) readdir_merges=$merges node_cache_misses=$misses token_hits=$hits ==" | tee -a "$rowdir/symshared-table.txt"
        # THE ENGAGEMENT LAW (design §8 row 3b, PR 13d's adjudication):
        # dlm_token_grants ∈ K_D + K_root + C + [0, 4] — one token per
        # stripe of D, one per child, one records-only grant per stripe of
        # the mount ROOT (a token reader's first `stat /` folds the root's
        # stripes once per token lifetime; 0 on an unstriped root), plus
        # the constant (D's own token — its attrs and its dentry page are
        # separate grants when `stat D` precedes the listing — its
        # parent's, the reader's root: the fresh-fleet runs read + 3
        # exactly, 20,067 for K_D = 64, K_root = 0, C = 20,000). "0 leaf
        # reads" is judged NET of the S5 control plane (the poll's dropped
        # images per epoch step + one tree-0 read per stripe slot) — the
        # lib's law, one definition for every venue.
        local dropped epochs
        dropped="$(sym_delta "$rowdir" "$reader" ls meta_kv_revalidate_nodes_dropped)"
        epochs="$(sym_delta "$rowdir" "$reader" ls meta_kv_revalidate_epochs)"
        sym_law_gate3b_ls "$grants" "$xattr_k" "$k_root" "$created" "$misses" "$dropped" "$epochs" "$merges"
        sym_zero_set sym-shared-dir-ls "$reader"
        log "sym-shared-dir-ls: $grants tokens for K_D=$xattr_k + K_root=$k_root + C=$created, 0 data-leaf reads for the listing ($misses misses = the poll's $dropped dropped images over $epochs epoch steps + ≤ K tree-0 lessee reads)"
    fi
    rm -rf "$shared" 2>/dev/null || true
    sym_oracle sym-shared-dir "$rowdir"
    log "sym-shared-dir PUBLISHED (table + snapshots in $rowdir)"
}

# The first element of a per-volume gauge (`stat_first` is mw_fleet.sh's).
stat_first_sym() { # idx key
    stat_field "$1" "$2" | tr -d '[] ' | cut -d, -f1
}

# --- gate 3c: sym-foreign-touch ---------------------------------------------
leg_sym_foreign_touch() {
    require_symmetric
    sym_build_storm
    local joiners
    mapfile -t joiners < <(joiner_idxs)
    [ "${#joiners[@]}" -ge 2 ] ||
        die "sym-foreign-touch needs ≥ 2 joined writers — create the fleet with: sudo tests/mw_fleet.sh create N=2 --symmetric --writers=3"
    sym_quiet_or_die sym-foreign-touch
    local rowdir a b c beat_ms n_floor
    rowdir="$STATE/rows/symtouch-$(date +%s)"
    mkdir -p "$rowdir"
    a="${joiners[0]}"
    b="${joiners[1]}"
    c=0
    beat_ms="$(stat_field 0 membership_renew_cadence_ms)"
    [ -n "$beat_ms" ] && [ "$beat_ms" != "0" ] || beat_ms=10000
    n_floor="$(stat_first_sym "$a" slot_offer_n_floor)"
    [ -n "$n_floor" ] && [ "$n_floor" -ge 2 ] || n_floor=2
    local burst=$((n_floor * 4))
    [ "$burst" -ge 64 ] || burst=64
    log "sym-foreign-touch: writers m$a (holder A), m$b (requester), m$c (the manager, holder C); beat ${beat_ms} ms, N_floor(A)=$n_floor, burst=$burst; $SYM_TOUCH_ROUNDS round(s) per phase"
    # Phase 1 — every writer's OWN job tree, its own rate.
    local idx own_rate_a
    for idx in "$a" "$b" "$c"; do
        mkdir -p "$(mnt_of "$idx")/job-w$idx"
        "$SYM_STORM" "$(mnt_of "$idx")/job-w$idx" "$SYM_THREADS" $((SYM_FILES / 4)) create >"$rowdir/own-w$idx.txt" 2>&1 ||
            die "sym-foreign-touch: m$idx's own job tree failed"
    done
    own_rate_a="$(awk '{for(i=1;i<=NF;i++) if($i ~ /^ops_s=/) {sub("ops_s=","",$i); print $i}}' "$rowdir/own-w$a.txt")"
    local a_tree_on_b
    a_tree_on_b="$(mnt_of "$b")/job-w$a"
    # Every touch's per-create status is kept (F-R4's verdict, PR 13e): a
    # create that met an errno is one line `create <name>: <error>` on
    # stderr AND in $rowdir/touch-errors.txt — the leg judges the file at
    # its end (the box's one `ENOENT` mid-handover was on the leg's stderr
    # alone and read as a finding, not a verdict). The ledger is written
    # SYNCHRONOUSLY (the creator's stderr captured, then appended) so the
    # end-of-leg read never races a writer — a `>(tee …)` substitution
    # nothing waited on made the verdict timing-dependent (review round 1,
    # Issue 11).
    : >"$rowdir/touch-errors.txt"
    sym_touch_create() { # dir prefix count
        local err rc=0
        err="$(sym_prefixed_create "$1" "$2" "$3" 2>&1 >/dev/null)" || rc=$?
        [ -z "$err" ] || printf '%s\n' "$err" | tee -a "$rowdir/touch-errors.txt" >&2
        return "$rc"
    }

    # Phase 2 — LIVE holder: A keeps creating in its tree while B touches
    # it with bursts — ships, NEVER a handover.
    for idx in "$a" "$b" "$c"; do snap "$idx" live0 "$rowdir"; done
    # The live directory exists BEFORE the storm is launched into it: the
    # first run of this leg launched the storm first, its first mkdir
    # failed ENOENT and died, and the "live" holder was IDLE for the whole
    # phase — the dominance law then moved the slot to B correctly and the
    # leg read it as "a live holder recalled by a touch" (PR 13, harness).
    # The holder stays LIVE for the whole phase: one storm of SYM_FILES
    # finishes in seconds at the holder's own rate, so the storm runs in
    # rounds (a fresh subdirectory each) until the touches are over.
    local live_root
    live_root="$(mnt_of "$a")/job-w$a/live"
    mkdir -p "$live_root" || die "sym-foreign-touch: the live directory's mkdir failed"
    (
        i=0
        while :; do
            i=$((i + 1))
            mkdir -p "$live_root/r$i" || exit 1
            "$SYM_STORM" "$live_root/r$i" "$SYM_THREADS" "$SYM_FILES" mkdir >>"$rowdir/live-a.txt" 2>&1 || exit 1
        done
    ) &
    local live_pid=$!
    local r
    for ((r = 1; r <= SYM_TOUCH_ROUNDS; r++)); do
        sym_touch_create "$a_tree_on_b" "touch-live-r$r" "$burst" || die "sym-foreign-touch: a live touch failed"
        sleep "$(python3 -c "print($beat_ms/1000)")"
    done
    # The LIVE verdict is vacuous unless the holder's storm was ALIVE
    # through every touch: a storm that died is an idle holder.
    kill -0 "$live_pid" 2>/dev/null ||
        die "sym-foreign-touch LIVE: the holder's storm died before the touches ended (see $rowdir/live-a.txt) — the phase measured an IDLE holder"
    # The loop AND its running storm (the subshell's child).
    pkill -P "$live_pid" 2>/dev/null || true
    kill "$live_pid" 2>/dev/null || true
    wait "$live_pid" 2>/dev/null || true
    sleep 2
    for idx in "$a" "$b" "$c"; do snap "$idx" live1 "$rowdir"; done
    local live_handovers=0 live_ships v
    for idx in "$a" "$b" "$c"; do
        v="$(sym_delta "$rowdir" "$idx" live slot_handovers)"
        live_handovers=$((live_handovers + v))
    done
    live_ships="$(sym_delta "$rowdir" "$b" live xv_cross_owner_steps_shipped)"
    [ "$live_handovers" = "0" ] || die "sym-foreign-touch LIVE: slot_handovers=$live_handovers — a live holder was recalled by a touch"
    [ "$live_ships" -ge $((burst * SYM_TOUCH_ROUNDS)) ] || die "sym-foreign-touch LIVE: only $live_ships shipped steps for $((burst * SYM_TOUCH_ROUNDS)) foreign creates"
    log "sym-foreign-touch LIVE: $live_ships ships, 0 handovers (a live holder is never recalled by a touch)"

    # Phase 3 — IDLE holder: A stopped; B's dominating bursts over the
    # T_idle window earn the OFFER and the handover (holder-decided, ONE
    # requester, `ops_q ≥ 2 × ops_h ∧ ops_q ≥ N_floor`); the carriage
    # rides the renewal beat, so the handover lands within a few beats.
    for idx in "$a" "$b" "$c"; do snap "$idx" idle0 "$rowdir"; done
    local t_idle0 handed=0 rounds_used=0 t_hand
    t_idle0="$(date +%s.%N)"
    for ((r = 1; r <= SYM_TOUCH_ROUNDS * 4; r++)); do
        sym_touch_create "$a_tree_on_b" "touch-idle-r$r" "$burst" || die "sym-foreign-touch: an idle touch failed"
        rounds_used="$r"
        sleep "$(python3 -c "print($beat_ms/1000)")"
        handed=0
        for idx in "$a" "$b" "$c"; do
            snap "$idx" idle1 "$rowdir"
            v="$(sym_delta "$rowdir" "$idx" idle slot_handovers)"
            handed=$((handed + v))
        done
        [ "$handed" -ge 1 ] && break
    done
    t_hand="$(date +%s.%N)"
    local idle_ships offers
    idle_ships="$(sym_delta "$rowdir" "$b" idle xv_cross_owner_steps_shipped)"
    offers=0
    for idx in "$a" "$b" "$c"; do
        v="$(sym_delta "$rowdir" "$idx" idle slot_offers)"
        offers=$((offers + v))
    done
    local phase_a
    phase_a="$(stat_field "$a" slot_handover_phase_ns)"
    echo "== PR 13 gate 3c: foreign-touch on m$a's IDLE tree by m$b — $rounds_used burst(s) of $burst over $(python3 -c "print(f'{$t_hand-$t_idle0:.1f}')") s: handovers=$handed offers=$offers ships=$idle_ships; the departing holder's slot_handover_phase_ns=$phase_a; own create rate(A)=$own_rate_a/s ==" | tee "$rowdir/symtouch-table.txt"
    [ "$handed" -ge 1 ] || die "sym-foreign-touch IDLE: no handover after $rounds_used dominating bursts of $burst (offers=$offers) — the idle arm never fired"
    log "sym-foreign-touch IDLE: handed over after $rounds_used burst(s) ($(python3 -c "print(f'{$handed/max(1e-9,$t_hand-$t_idle0):.3f}')") handovers/s)"
    # The handover's accounting lands on THREE daemons at three instants
    # (the departing holder's release, the manager's served `ReleaseSlot`
    # spending the recall, the requester's accept) and the loop above
    # breaks at the FIRST — let the rest land before the PAUSED phase
    # snapshots, or a count that belongs to this handover reads as the
    # paused phase's (the from-zero batch read PAUSED handovers=1 off the
    # manager's late count).
    local settle_prev settle_now settle_t0
    settle_prev=-1
    settle_t0="$(date +%s)"
    while :; do
        settle_now=0
        for idx in "$a" "$b" "$c"; do
            v="$(stat_sum "$idx" slot_handovers)"
            settle_now=$((settle_now + ${v:-0}))
        done
        [ "$settle_now" = "$settle_prev" ] && break
        settle_prev="$settle_now"
        [ $(($(date +%s) - settle_t0)) -lt 30 ] || break
        sleep 2
    done

    # Phase 4 — a PAUSED live job: C's creator SIGSTOPped mid-tree, B a
    # SINGLE touch per beat — the tree STAYS (design §5.1.4: a live job on
    # a pause keeps its tree because `ops_h(T_idle)` — its own burst — dwarfs
    # `2 × ops_q`). The premise is that the pause is SHORTER than `T_idle`
    # (= the membership lease TTL, 15 s on this fleet): past it the job is
    # IDLE by the design's own definition and the idle arm decides on
    # `N_floor` alone — a derived ratio (`ewma_handover / ewma_ship`) that
    # reads 2 when a served ship costs as much as a handover on a hot box.
    # Attempt 12 from zero ran the three touches at the 10 s membership
    # beat (31 s > 15 s), and the tree moved at the 21 s touch by the rule
    # — a harness premise, not a product term. The touches are paced so
    # the whole phase sits inside the holder's window with margin — and
    # the window the holder COUNTS over is two half-`T_idle` buckets on
    # an absolute clock (`HolderOps::total` = the current + the previous
    # bucket; `DominanceWindow::epoch`), so what it guarantees is
    # `T_idle / 2`, not `T_idle`: attempt 13's 3 s beats fit 15 s and
    # still moved the tree once the storm's bucket aged out under the
    # third touch. The phase fits the HALF window.
    local t_idle_ms paused_beat_ms
    t_idle_ms="$(stat_field 0 membership_lease_ttl_ms)"
    [ -n "$t_idle_ms" ] && [ "$t_idle_ms" -gt 0 ] 2>/dev/null || t_idle_ms=45000
    paused_beat_ms=$(((t_idle_ms / 2 - 2500) / (SYM_TOUCH_ROUNDS + 1)))
    [ "$paused_beat_ms" -lt "$beat_ms" ] || paused_beat_ms="$beat_ms"
    [ "$paused_beat_ms" -ge 1000 ] || die "sym-foreign-touch PAUSED: T_idle ${t_idle_ms} ms leaves no room for $SYM_TOUCH_ROUNDS touches (beat would be ${paused_beat_ms} ms) — fewer --touch-rounds or a longer --lease-ttl-ms"
    log "sym-foreign-touch PAUSED: T_idle=${t_idle_ms} ms, $SYM_TOUCH_ROUNDS touches at ${paused_beat_ms} ms (the phase inside the holder's window)"
    for idx in "$a" "$b" "$c"; do snap "$idx" paused0 "$rowdir"; done
    # The storm's root is created FIRST (mdstorm's `mkdir` phase never
    # creates its own root — the LIVE phase's law at `live_root`): from PR
    # 13's `8b7cc418` to the box re-run this phase launched the storm
    # into a directory that did not exist, its first `mkdir` failed
    # ENOENT, every worker stopped, and the "paused live job" was no job
    # at all — the touched slot read IDLE (`slot_offers_idle` +1 in every
    # position) and the phase's outcome was the IDLE arm's, never the
    # paused-job law's. Backgrounded and waited with `|| true`, nothing
    # noticed (the box re-run's review, Issue 1). The phase now judges its
    # job the way the LIVE phase judges its storm: alive and STOPPED at
    # the pause, COMPLETED after the resume, the holder's journal moved by
    # the storm's entries — a storm that died is an idle holder, and the
    # phase dies loud.
    local paused_root paused_pid paused_state
    paused_root="$(mnt_of "$c")/job-w$c/paused"
    mkdir -p "$paused_root" || die "sym-foreign-touch PAUSED: the paused job's directory mkdir failed"
    "$SYM_STORM" "$paused_root" "$SYM_THREADS" "$SYM_FILES" mkdir >"$rowdir/paused-c.txt" 2>&1 &
    paused_pid=$!
    sleep 1
    kill -STOP "$paused_pid" 2>/dev/null || die "sym-foreign-touch PAUSED: the paused job died before the pause (see $rowdir/paused-c.txt)"
    # A job that failed at its first syscall exits before the STOP lands
    # and reads as a zombie (`kill -0` still succeeds); the process STATE
    # is the witness — `T` (stopped) is a live job holding its work.
    for t in $(seq 1 20); do
        : "$t"
        paused_state="$(awk '/^State:/ {print $2}' "/proc/$paused_pid/status" 2>/dev/null)"
        [ "$paused_state" = "T" ] && break
        sleep 0.1
    done
    [ "$paused_state" = "T" ] ||
        die "sym-foreign-touch PAUSED: the paused job is not a STOPPED live process (state '${paused_state:-gone}'; see $rowdir/paused-c.txt) — the phase would measure an IDLE holder"
    grep -q "failed" "$rowdir/paused-c.txt" 2>/dev/null &&
        die "sym-foreign-touch PAUSED: the paused job FAILED before the pause: $(head -1 "$rowdir/paused-c.txt")"
    local c_tree_on_b
    c_tree_on_b="$(mnt_of "$b")/job-w$c"
    for ((r = 1; r <= SYM_TOUCH_ROUNDS; r++)); do
        sym_touch_create "$c_tree_on_b" "touch-paused-r$r" 1 || die "sym-foreign-touch: a paused-job touch failed"
        sleep "$(python3 -c "print($paused_beat_ms/1000)")"
    done
    # Resume and let the job COMPLETE its phase (SYM_FILES mkdirs — seconds
    # at the holder's own rate): its row line is the proof it was a live
    # job through the pause, and the holder's journal must carry its
    # entries (one commit per mkdir, the D4 economy).
    kill -CONT "$paused_pid" 2>/dev/null || die "sym-foreign-touch PAUSED: the paused job vanished before the resume (see $rowdir/paused-c.txt)"
    wait "$paused_pid" || die "sym-foreign-touch PAUSED: the paused job FAILED after the resume (see $rowdir/paused-c.txt)"
    grep -q "^mkdir ops=$SYM_FILES " "$rowdir/paused-c.txt" ||
        die "sym-foreign-touch PAUSED: the paused job's row is missing — it did not complete its $SYM_FILES mkdirs (see $rowdir/paused-c.txt)"
    sleep 2
    for idx in "$a" "$b" "$c"; do snap "$idx" paused1 "$rowdir"; done
    local paused_journal
    paused_journal="$(sym_delta "$rowdir" "$c" paused meta_kv_journal_entries)"
    [ "$paused_journal" -ge "$SYM_FILES" ] ||
        die "sym-foreign-touch PAUSED: the holder's journal moved by $paused_journal entries over the phase, fewer than the job's $SYM_FILES mkdirs — the job's work did not land at the holder"
    log "sym-foreign-touch PAUSED: the paused job was a live STOPPED process through the touches and completed after the resume ($(tr '\n' ' ' <"$rowdir/paused-c.txt"); the holder's journal +$paused_journal entries)"
    local paused_handovers=0
    for idx in "$a" "$b" "$c"; do
        v="$(sym_delta "$rowdir" "$idx" paused slot_handovers)"
        paused_handovers=$((paused_handovers + v))
    done
    # The paused-job law (design §5.1.4 as built at PR 13c, F-B2): a
    # dentry-bearing commit under `job-wC/paused/dN` credits `paused`'s,
    # `job-wC`'s and `/`'s slots (`install_liveness_ancestors`), so the
    # touched `job-wC` slot is LIVE at the holder for the phase's whole
    # window whatever rotor the job's children mint into — no IDLE offer,
    # no DOMINATED offer, no handover. All three gauges are READ on the
    # holder and every one must be 0; a handover, an idle offer or a
    # dominated offer against a live holder names which law broke.
    local paused_idle paused_dominated
    paused_idle="$(sym_delta "$rowdir" "$c" paused slot_offers_idle)"
    paused_dominated="$(sym_delta "$rowdir" "$c" paused slot_offers_dominated)"
    [ "$paused_handovers" = "0" ] || die "sym-foreign-touch PAUSED: slot_handovers=$paused_handovers (holder slot_offers_idle=+$paused_idle slot_offers_dominated=+$paused_dominated) — a live holder's slot moved under single touches"
    [ "$paused_idle" = "0" ] || die "sym-foreign-touch PAUSED: slot_offers_idle=+$paused_idle at the holder — a live (paused) job's slot read IDLE; the subtree liveness credit failed"
    [ "$paused_dominated" = "0" ] || die "sym-foreign-touch PAUSED: slot_offers_dominated=+$paused_dominated at the holder — a live holder's slot was DOMINATED by a single touch per beat"
    log "sym-foreign-touch PAUSED: handovers=0, holder slot_offers_idle=+0, slot_offers_dominated=+0 (a paused live job's slot is never offered or moved by a touch)"
    echo "   PAUSED: $SYM_TOUCH_ROUNDS single touches over $SYM_TOUCH_ROUNDS beats: handovers=$paused_handovers holder idle offers=+$paused_idle dominated offers=+$paused_dominated — a paused live job's slot is never offered or moved" | tee -a "$rowdir/symtouch-table.txt"
    sym_fb1_faces "$rowdir" paused "$a" "$b" "$c" | tee -a "$rowdir/symtouch-table.txt"
    for idx in "$a" "$b" "$c"; do
        sym_zero_set sym-foreign-touch "$idx"
    done
    # F-R4's verdict (PR 13e): every touch of every phase landed — a
    # `sym_prefixed_create` that met an errno printed `create <name>: …`
    # on stderr and stopped its burst; the box read ONE `ENOENT` on the
    # IDLE burst that moved the slot to the toucher (record §3.9.4.3). The
    # LIVE / IDLE / PAUSED counts above are the ships' side; this is the
    # application's.
    local touch_errors
    touch_errors="$(grep -c "^create touch-" "$rowdir/touch-errors.txt" 2>/dev/null || true)"
    [ "${touch_errors:-0}" = "0" ] ||
        die "sym-foreign-touch: $touch_errors touch create(s) answered an errno to the application (F-R4's class) — $rowdir/touch-errors.txt"
    log "sym-foreign-touch: every touch create of every phase landed (0 errnos to the application — F-R4)"
    # The end-of-leg `rm -rf` is F-R3's shape (PR 13e; record §3.9.4.3):
    # every writer removes ITS job tree through ITS mount, and every child
    # another appender minted into it (the touches) is a cross-owner
    # unlink whose count step must read the child's record AT THE HOLDER —
    # the box read this daemon's PROJECTION, found no record, dropped the
    # count step and shipped the name's removal alone: 430 of 512 children
    # orphaned per tree, each logged "no inode record — removing the
    # dangling name". The census arm judges it at BOTH ends.
    local rm_t0
    rm_t0="$(date +%s.%N)"
    for idx in "$a" "$b" "$c"; do
        rm -rf "$(mnt_of "$idx")/job-w$idx" ||
            die "sym-foreign-touch: rm -rf job-w$idx through m$idx failed (F-R3's shape — a name a cross-owner unlink could not remove)"
    done
    sym_oracle sym-foreign-touch "$rowdir"
    sym_post_leave_census sym-foreign-touch "$rowdir" "$rm_t0" "$a" "$b" "$c"
    log "sym-foreign-touch PUBLISHED (table + snapshots in $rowdir)"
}

# F-R3's fleet proof (PR 13e): (1) the writers' logs carry ZERO "no inode
# record" lines from the leg's removals — the cross-owner plan builders'
# dangling-name arm, a must-stay-0 on an armed mount
# (`xv_cross_owner_dangling_names`); (2) after EVERY joiner LEAVES (its
# slots released to the manager — Unleased, the manager's to judge), the
# manager's online fsck covers the inode plane WHOLE and reads C9 = C10 = 0:
# the orphan an F-R3 unlink leaves lives in the CREATOR's slot tree — a
# projection at the censusing mount while the creator lives, scoped out of
# every live-fleet row (`fsck_inode_plane_foreign_dentry_scoped`); (3) after
# EVERY member leaves, the OFFLINE probe judges the set with nothing in
# flight and must exempt NOTHING as current-era beside findings 0 — the
# clause with teeth for the joiner-minted class (review round 1, Issue 2:
# the era floor never named a joined appender's slots until it consulted
# tree 0). The fleet is mounted back afterwards (the leg leaves it as it
# found it).
sym_post_leave_census() { # label rowdir since_epoch idx...
    local label="$1" rowdir="$2" since="$3"
    shift 3
    local idx n_dangling=0 lines
    for idx in "$@"; do
        lines="$(grep -c "no inode record" "$STATE/m$idx.log" 2>/dev/null || true)"
        lines="${lines:-0}"
        [ "$lines" = "0" ] || {
            grep "no inode record" "$STATE/m$idx.log" >"$rowdir/dangling-m$idx.txt"
            n_dangling=$((n_dangling + lines))
        }
        [ "$(stat_sum "$idx" xv_cross_owner_dangling_names)" = "0" ] ||
            die "$label: xv_cross_owner_dangling_names != 0 on m$idx — a cross-owner unlink dropped its count step (F-R3)"
        [ "$(stat_sum "$idx" xv_cross_owner_witness_refusals)" = "0" ] ||
            warn "$label: xv_cross_owner_witness_refusals=$(stat_sum "$idx" xv_cross_owner_witness_refusals) on m$idx (the belt fired — the retry landed the read at the holder)"
    done
    [ "$n_dangling" = "0" ] ||
        die "$label: $n_dangling 'no inode record' line(s) in the writers' logs (F-R3's orphan class — see $rowdir/dangling-m*.txt)"
    log "$label: zero 'no inode record' lines across the writers' logs (since $since)"
    # Every joiner LEAVES; the manager's census then judges every slot.
    local joiners j
    mapfile -t joiners < <(joiner_idxs)
    for j in "${joiners[@]}"; do
        if mountpoint -q "$(mnt_of "$j")"; then
            "$MWFLEET" unmount "$j" || die "$label: joiner $j's leave failed"
            wait_for_unmounted "$(mnt_of "$j")"
        fi
    done
    local t
    for t in $(seq 1 60); do
        : "$t"
        [ "$(stat_all_eq 0 appenders_known 1)" = "1" ] && break
        sleep 1
    done
    [ "$(stat_all_eq 0 appenders_known 1)" = "1" ] ||
        die "$label: the manager's appender directory still counts $(stat_field 0 appenders_known) Live page(s) after every joiner left"
    [ "$(stat_all_eq 0 slot_lease_conflicts 0)" = "1" ] || die "$label: slot_lease_conflicts != 0 at the manager after the leaves"
    # The manager's per-volume `symmetric_meta` array is the set's width —
    # the census must cover EVERY volume (a scoped pass is no verdict).
    local volumes
    volumes="$(stat_field 0 symmetric_meta | python3 -c 'import ast,sys; v=ast.literal_eval(sys.stdin.read().strip() or "1"); print(len(v) if isinstance(v, list) else 1)')"
    local out rc=0
    out="$(timeout 900 "$SQZ" fsck "$(mnt_of 0)" --json 2>"$rowdir/fsck-$label-post-leave.err")" || rc=$?
    echo "$out" >"$rowdir/fsck-$label-post-leave.json"
    [ "$rc" != "124" ] || die "$label: the post-leave online fsck HUNG past 900 s — $rowdir/fsck-$label-post-leave.err"
    [ "$rc" = "0" ] || die "$label: the post-leave online fsck FAILED (rc=$rc) — $rowdir/fsck-$label-post-leave.err"
    python3 - "$rowdir/fsck-$label-post-leave.json" "$label" "$volumes" <<'PYEOF' || die "$label: the post-leave inode-plane census is RED (F-R3) — $rowdir/fsck-$label-post-leave.json"
import json, sys
path, label, volumes = sys.argv[1], sys.argv[2], int(sys.argv[3])
r = json.load(open(path))
c = r["counters"]
covered = c["inode_plane_volumes_covered"]
by_class = {}
for f in r["findings"]:
    by_class[f["class"]] = by_class.get(f["class"], 0) + 1
c9 = by_class.get("C9", 0)
c10 = by_class.get("C10", 0)
print(f"{label} post-leave census: inode plane covered {covered} of {volumes} volume(s), findings by class {by_class or '{}'}")
ok = covered == volumes and c9 == 0 and c10 == 0 and len(r["findings"]) == 0
if covered != volumes:
    print(f"{label}: the inode plane covered {covered} of {volumes} volumes after every joiner left — a scoped pass records no C9/C10 verdict", file=sys.stderr)
if c9 or c10:
    for f in r["findings"]:
        if f["class"] in ("C9", "C10"):
            print(f"  [{f['class']}] {f['object']} — {f['evidence']}", file=sys.stderr)
sys.exit(0 if ok else 1)
PYEOF
    log "$label: post-leave census clean — the inode plane judged WHOLE at the manager, C9 = C10 = 0 (every joiner left; the F-R3 orphan class would read C9 here)"
    # The OFFLINE census (review round 1, Issue 2): every member leaves —
    # readers, then the manager — and the offline probe judges the set with
    # NOTHING in flight, so an inode it exempts as current-era is an inode
    # it did not judge: `current_era_exempted` must read 0 beside findings 0
    # (before PR 13e's era floor consulted tree 0, every JOINER-minted ino
    # read exempt at every censusing mount — the online census above and
    # this probe alike — and the fleet arm's C9 clause was vacuous for the
    # box's 430; the `no inode record` grep and the dangling-name gauge
    # were its teeth).
    local i
    for i in $(member_idxs); do
        [ "$i" = "0" ] && continue
        if mountpoint -q "$(mnt_of "$i")"; then
            "$MWFLEET" unmount "$i" || die "$label: member $i's unmount before the offline census failed"
            wait_for_unmounted "$(mnt_of "$i")"
        fi
    done
    "$MWFLEET" unmount 0 || die "$label: the manager's unmount before the offline census failed"
    wait_for_unmounted "$(mnt_of 0)"
    rc=0
    out="$(timeout 900 "$SQZ" fsck "sqmeta://$META_PATHS" --offline --json 2>"$rowdir/fsck-$label-offline.err")" || rc=$?
    echo "$out" >"$rowdir/fsck-$label-offline.json"
    [ "$rc" != "124" ] || die "$label: the OFFLINE fsck HUNG past 900 s — $rowdir/fsck-$label-offline.err"
    [ "$rc" = "0" ] || die "$label: the OFFLINE fsck FAILED (rc=$rc) — $rowdir/fsck-$label-offline.err"
    python3 - "$rowdir/fsck-$label-offline.json" "$label" "$volumes" <<'PYEOF' || die "$label: the OFFLINE census is RED (F-R3 / Issue 2) — $rowdir/fsck-$label-offline.json"
import json, sys
path, label, volumes = sys.argv[1], sys.argv[2], int(sys.argv[3])
r = json.load(open(path))
c = r["counters"]
covered = c["inode_plane_volumes_covered"]
exempt = c["current_era_exempted"]
by_class = {}
for f in r["findings"]:
    by_class[f["class"]] = by_class.get(f["class"], 0) + 1
print(f"{label} offline census: inode plane covered {covered} of {volumes} volume(s), current_era_exempted {exempt}, findings by class {by_class or '{}'}")
ok = covered == volumes and exempt == 0 and len(r["findings"]) == 0
if exempt:
    print(f"{label}: the offline probe exempted {exempt} inode(s) as current-era — a probe has nothing in flight, so these are inodes the census did NOT judge (a joined appender's mints before the era floor consulted tree 0)", file=sys.stderr)
for f in r["findings"]:
    print(f"  [{f['class']}] {f['object']} — {f['evidence']}", file=sys.stderr)
sys.exit(0 if ok else 1)
PYEOF
    log "$label: OFFLINE census clean — every writer left, the probe judged every inode (current_era_exempted 0), C9 = C10 = 0"
    "$MWFLEET" mount 0 || die "$label: the manager's re-mount failed"
    # The remounted manager is a SUCCESSOR S6 owner inside its failover
    # grace window (T_owner, reclaim only — spec §6.7): a remounted reader
    # or joiner is a FRESH membership acquire, which the window refuses by
    # design, and a reader takes the refusal as final (the shipped S5
    # posture: it stays invisible to `squeezefs clients`), so the fleet's
    # `membership_mode=member` gate would read `off`. Wait the window out,
    # as an operator's retry would (the s10-delegation leg's posture); the
    # first run of this arm found the ordering (PR 13e review round 2).
    local grace
    for ((t = 0; t < 90; t++)); do
        # An empty read is "not yet" (the stats inode not served yet), never
        # "closed" (PR 13e review round 3, Issue 14).
        grace="$(stat_field 0 membership_grace_remaining_ms)"
        [ -n "$grace" ] && [ "$grace" = "0" ] && break
        sleep 1
    done
    [ "$t" -lt 90 ] ||
        die "$label: the remounted manager's grace window did not close within 90 s (membership_grace_remaining_ms=${grace:-unread})"
    log "$label: the remounted manager's grace window closed (waited ${t}s); re-admitting the members"
    for i in $(member_idxs); do
        [ "$i" = "0" ] && continue
        case " ${joiners[*]} " in *" $i "*) continue ;; esac
        "$MWFLEET" mount "$i" || die "$label: member $i's re-mount failed"
    done
    for j in "${joiners[@]}"; do
        "$MWFLEET" mount "$j" || die "$label: joiner $j's re-mount failed"
    done
    for t in $(seq 1 60); do
        : "$t"
        [ "$(stat_all_eq 0 appenders_known $((${#joiners[@]} + 1)))" = "1" ] && break
        sleep 1
    done
    [ "$(stat_all_eq 0 appenders_known $((${#joiners[@]} + 1)))" = "1" ] ||
        die "$label: the joiners' re-join never landed (appenders_known=$(stat_field 0 appenders_known))"
}

# --- PR 13b: sym-foreign-file ------------------------------------------------
# The record-level metanode ship on a real fleet (design §5.10's "write to a
# FOREIGN-owned file" row; PR 13 defect 32, the flip's first blocker): every
# writer mutates a COLLEAGUE's files — the next joiner's, the last joiner's
# the manager's — through the ordinary syscalls (chmod / touch / setfattr /
# an APPEND + fsync / a truncate / an unlink), each verb SHIPPED to the
# file's slot holder and applied there; the result is read back at the
# HOLDER (the authority for the record) and at a THIRD mount (the ledger
# every reader resolves); the deleted files stay deleted at every mount.
# The engagement laws close the ledger at the two ends of every ship.
# LOCAL = "it works" (the venue ruling): no rate this leg prints is a
# verdict.
leg_sym_foreign_file() {
    require_symmetric
    local joiners
    mapfile -t joiners < <(joiner_idxs)
    [ "${#joiners[@]}" -ge 2 ] ||
        die "sym-foreign-file needs ≥ 2 joined writers — create the fleet with: sudo tests/mw_fleet.sh create N=2 --symmetric --writers=2"
    [[ "$SYM_FF_FILES" =~ ^[1-9][0-9]*$ ]] || die "--ff-files takes a positive integer (got '$SYM_FF_FILES')"
    local rowdir writers idx
    rowdir="$STATE/rows/symff-$(date +%s)"
    mkdir -p "$rowdir"
    writers=(0 "${joiners[@]}")
    log "sym-foreign-file: ${#writers[@]} writers (the manager + ${#joiners[@]} joiners), $SYM_FF_FILES files each, every writer mutating its right neighbour's files through the record-level ship"
    for idx in "${writers[@]}"; do snap "$idx" ff0 "$rowdir"; done

    # Phase 1 — every writer's OWN files, fsync-acked (the acked-writes
    # oracle's ledger: name + content).
    local i f mnt
    for idx in "${writers[@]}"; do
        mnt="$(mnt_of "$idx")"
        mkdir -p "$mnt/ff-w$idx" || die "sym-foreign-file: mkdir ff-w$idx on m$idx failed"
        : >"$rowdir/acked-w$idx.ledger"
        for ((i = 0; i < SYM_FF_FILES; i++)); do
            f="$mnt/ff-w$idx/f$(printf '%04d' "$i")"
            printf 'w%s:%04d' "$idx" "$i" | dd of="$f" conv=fsync status=none 2>>"$rowdir/dd-w$idx.err" ||
                die "sym-foreign-file: m$idx's own write of f$i failed (see $rowdir/dd-w$idx.err)"
            echo "f$(printf '%04d' "$i")" >>"$rowdir/acked-w$idx.ledger"
        done
    done
    sleep 1

    # Phase 2 — the FOREIGN mutations: writer q mutates holder h's files
    # (h = q's right neighbour in the writer ring) through q's mount. Every
    # verb must succeed at the syscall: the interim posture answered
    # ENOENT / EOPNOTSUPP / a refused publish here (record §4.4z).
    local n q h qmnt hdir rc err ops_q
    n=${#writers[@]}
    : >"$rowdir/mutations.tsv"
    for ((k = 0; k < n; k++)); do
        q="${writers[$k]}"
        h="${writers[$(((k + 1) % n))]}"
        qmnt="$(mnt_of "$q")"
        hdir="$qmnt/ff-w$h"
        ops_q=0
        for ((i = 0; i < SYM_FF_FILES; i++)); do
            f="$hdir/f$(printf '%04d' "$i")"
            # The DATA face first (an append / a truncate moves mtime), the
            # record verbs after it — the touched mtime is the final word.
            case $((i % 4)) in
            0 | 1)
                # An APPEND + fsync (the DATA face: the publish ships to
                # the holder under q's custody lease there).
                err="$(python3 - "$f" "$q" 2>&1 <<'PYEOF'
import os, sys
p, q = sys.argv[1], sys.argv[2]
fd = os.open(p, os.O_WRONLY | os.O_APPEND)
os.write(fd, f"|by-w{q}".encode())
os.fsync(fd)
os.close(fd)
PYEOF
)" || die "sym-foreign-file: m$q append+fsync into m$h's f$i: $err"
                ;;
            2)
                err="$(truncate -s 4 "$f" 2>&1)" || die "sym-foreign-file: m$q truncate of m$h's f$i: $err"
                ;;
            3)
                err="$(rm "$f" 2>&1)" || die "sym-foreign-file: m$q unlink of m$h's f$i: $err"
                continue
                ;;
            esac
            err="$(chmod 640 "$f" 2>&1)" || die "sym-foreign-file: m$q chmod of m$h's f$i: $err"
            err="$(touch -d '@1700000000' "$f" 2>&1)" || die "sym-foreign-file: m$q touch of m$h's f$i: $err"
            err="$(setfattr -n user.ff -v "by-w$q" "$f" 2>&1)" || die "sym-foreign-file: m$q setfattr on m$h's f$i: $err"
            ops_q=$((ops_q + 3))
        done
        printf 'q=%s h=%s files=%s record_verbs=%s\n' "$q" "$h" "$SYM_FF_FILES" "$ops_q" >>"$rowdir/mutations.tsv"
        log "sym-foreign-file: m$q mutated m$h's $SYM_FF_FILES files ($ops_q record verbs, $((SYM_FF_FILES / 2)) appends, $((SYM_FF_FILES / 4)) truncates, $((SYM_FF_FILES / 4)) unlinks)"
    done
    # Let the appends' writeback publishes and the holders' checkpoints
    # land before the read-back (the fsync acked each; the close-time
    # writeback of the page cache is the kernel's).
    sleep 2

    # Phase 3 — the read-back at the HOLDER and at a THIRD mount: mode,
    # mtime, xattr, content, size; the unlinked files ENOENT everywhere
    # (deleted stays deleted — `sym_stat_deleted`, only ENOENT is deleted).
    local third tmnt hmnt want got lost=0 verdict
    : >"$rowdir/lost.txt"
    for ((k = 0; k < n; k++)); do
        q="${writers[$k]}"
        h="${writers[$(((k + 1) % n))]}"
        third="${writers[$(((k + 2) % n))]}"
        hmnt="$(mnt_of "$h")"
        tmnt="$(mnt_of "$third")"
        for ((i = 0; i < SYM_FF_FILES; i++)); do
            f="f$(printf '%04d' "$i")"
            case $((i % 4)) in
            0 | 1) want="$(printf 'w%s:%04d|by-w%s' "$h" "$i" "$q")" ;;
            2) want="$(printf 'w%s:%04d' "$h" "$i" | head -c 4)" ;;
            3) want="" ;;
            esac
            for m in "$hmnt" "$tmnt"; do
                if [ $((i % 4)) = 3 ]; then
                    verdict="$(sym_stat_deleted "$m/ff-w$h/$f" 20)"
                    [ "$verdict" = "deleted" ] || { lost=$((lost + 1)); echo "NOT DELETED at $m ($verdict): ff-w$h/$f (unlinked by m$q)" >>"$rowdir/lost.txt"; }
                    continue
                fi
                got="$(cat "$m/ff-w$h/$f" 2>&1)" || { lost=$((lost + 1)); echo "UNREADABLE at $m: ff-w$h/$f: $got" >>"$rowdir/lost.txt"; continue; }
                [ "$got" = "$want" ] || { lost=$((lost + 1)); echo "CONTENT at $m: ff-w$h/$f '$got' != '$want' (mutated by m$q)" >>"$rowdir/lost.txt"; }
                [ "$(stat -c %a "$m/ff-w$h/$f" 2>&1)" = "640" ] || { lost=$((lost + 1)); echo "MODE at $m: ff-w$h/$f $(stat -c %a "$m/ff-w$h/$f" 2>&1) != 640 (chmod by m$q)" >>"$rowdir/lost.txt"; }
                [ "$(stat -c %Y "$m/ff-w$h/$f" 2>&1)" = "1700000000" ] || { lost=$((lost + 1)); echo "MTIME at $m: ff-w$h/$f $(stat -c %Y "$m/ff-w$h/$f" 2>&1) != 1700000000 (touch by m$q)" >>"$rowdir/lost.txt"; }
                [ "$(getfattr --absolute-names -n user.ff --only-values "$m/ff-w$h/$f" 2>&1)" = "by-w$q" ] || { lost=$((lost + 1)); echo "XATTR at $m: ff-w$h/$f user.ff='$(getfattr --absolute-names -n user.ff --only-values "$m/ff-w$h/$f" 2>&1)' != by-w$q" >>"$rowdir/lost.txt"; }
            done
        done
    done
    [ "$lost" = "0" ] || die "sym-foreign-file: $lost read-back violation(s) — see $rowdir/lost.txt:
$(head -20 "$rowdir/lost.txt")"
    log "sym-foreign-file: every shipped mutation reads back exact at the holder and at a third mount; every unlinked file ENOENT at both"

    # Phase 4 — the engagement laws (the ledger closes at the two ends).
    for idx in "${writers[@]}"; do snap "$idx" ff1 "$rowdir"; done
    local ships=0 served=0 refusals=0 unreachable=0 pships=0 pserved=0 prefused=0 v custody
    for idx in "${writers[@]}"; do
        v="$(sym_delta "$rowdir" "$idx" ff meta_ship.record_ships)"; ships=$((ships + v))
        [ "$v" -ge $((SYM_FF_FILES / 4 * 3 * 3)) ] || die "sym-foreign-file: m$idx record_ships +$v for $((SYM_FF_FILES / 4 * 3 * 3)) foreign record verbs (chmod + touch + setfattr per surviving file — three of every four)"
        v="$(sym_delta "$rowdir" "$idx" ff meta_ship.record_served)"; served=$((served + v))
        v="$(sym_delta "$rowdir" "$idx" ff meta_ship.record_refusals)"; refusals=$((refusals + v))
        v="$(sym_delta "$rowdir" "$idx" ff meta_ship.record_unreachable)"; unreachable=$((unreachable + v))
        v="$(sym_delta "$rowdir" "$idx" ff meta_ship.foreign_publish_ships)"; pships=$((pships + v))
        [ "$v" -ge 1 ] || die "sym-foreign-file: m$idx foreign_publish_ships +$v — its appends into a colleague's files published through no slot holder"
        v="$(sym_delta "$rowdir" "$idx" ff meta_ship.foreign_publish_served)"; pserved=$((pserved + v))
        v="$(sym_delta "$rowdir" "$idx" ff meta_ship.foreign_publish_refusals)"; prefused=$((prefused + v))
        custody="$(sym_delta "$rowdir" "$idx" ff dlm_custody.dlm_custody_via_slot_holder)"
        [ "$custody" -ge 1 ] || die "sym-foreign-file: m$idx dlm_custody_via_slot_holder +$custody — its write custody of a colleague's file came from no slot holder"
    done
    [ "$refusals" = "0" ] || die "sym-foreign-file: record_refusals=$refusals across the fleet (must stay 0)"
    [ "$unreachable" = "0" ] || die "sym-foreign-file: record_unreachable=$unreachable across the fleet — a holder had no endpoint bound"
    [ "$ships" = "$served" ] || die "sym-foreign-file: Σ record_ships $ships != Σ record_served $served (a shipped verb landed nowhere, or a served one was nobody's)"
    # Review round 1, Issue 4: `foreign_publish_ships` counts the publishes
    # that LANDED (at the terminal reply, once per logical publish), so it
    # closes against served EXACTLY; a terminal failure is its own gauge.
    [ "$pships" = "$pserved" ] || die "sym-foreign-file: Σ foreign_publish_ships $pships != Σ foreign_publish_served $pserved"
    [ "$prefused" = "0" ] || die "sym-foreign-file: foreign_publish_refusals=$prefused across the fleet — a shipped publish failed terminally at its holder"
    echo "== PR 13b sym-foreign-file: ${#writers[@]} writers × $SYM_FF_FILES files: record_ships=$ships ≡ record_served=$served, record_refusals=0, foreign_publish_ships=$pships ≡ served=$pserved, foreign_publish_refusals=0$SYM_BUSY_ROW ==" | tee "$rowdir/symff-table.txt"
    for idx in "${writers[@]}"; do
        rm -rf "$(mnt_of "$idx")/ff-w$idx" 2>/dev/null || true
    done
    sym_oracle sym-foreign-file "$rowdir"
    log "sym-foreign-file PUBLISHED (table + snapshots in $rowdir)"
}

# --- PR 13h: sym-reclaim-hint ------------------------------------------------
# The corpse-reclaimer law on a real fleet (design §5.1.3; record §4.4ap —
# review round 2, Issue 9): a FORGET whose ino lives in a slot this daemon
# does not reclaim TRAVELS to the slot's reclaimer as a reclaim hint over the
# production wire (rung 7's step shipper, the S8 listener, `main.rs`'s
# sink). Three shapes, each on the production paths alone — no seam, no
# in-process venue: (A) the box's LEASED-foreign shape (a token client's
# forgets of the holder's corpses), (B) the MOVED-slot corpse (class ii),
# (C) the UNLEASED corpse (class i — the manager sweeps unleased slots at
# mount only and its kernel never held the ino, so without the hint it
# leaked until a remount). The fleet is created with
# `SQUEEZEFS_SYM_AFFINITY_MAX_MB` set (64 MiB — the registered static
# affinity ceiling): a fresh joiner's derived `A_max` is `max(used/64,
# node_size)` = one extent, and a one-extent directory tree sits exactly AT
# it, so its files mint into the ROTOR and no storm into the directory
# would move their slot; the leg asserts the premise off the published
# `affinity_mints` — per create, because a directory's children mint
# round-robin over the set's metadata VOLUMES and the affinity holds only
# on the parent's (`sym_rh_make_corpse`). LOCAL = "it works" (the venue
# ruling): no rate here is a verdict.
sym_rh_logs_clean() { # label idx line0
    local n
    n="$(tail -n "+$(( $3 + 1 ))" "$STATE/m$2.log" 2>/dev/null | grep -c "corrupt KV encoding\|destroy WITHHELD" || true)"
    [ "${n:-0}" = "0" ] ||
        die "$1: $n 'corrupt KV encoding' / 'destroy WITHHELD' line(s) in m$2's log since the leg began (F-R6's shape) — $STATE/m$2.log"
}
sym_rh_hold_open() { # path -> pid of a process holding it open
    # The holder's stdio is detached: a `$(…)` capture of this function
    # would otherwise wait on the pipe the background process holds.
    python3 - "$1" >/dev/null 2>&1 <<'PYEOF' &
import os, signal, sys
fd = os.open(sys.argv[1], os.O_RDONLY)
signal.pause()
PYEOF
    echo $!
}
sym_rh_handover() { # label holder_idx requester_idx dir_at_requester rowdir phase burst beat_ms
    local label="$1" h="$2" q="$3" dir="$4" rowdir="$5" ph="$6" burst="$7" beat_ms="$8"
    local r v handed=0 idx
    for idx in "$h" "$q" 0; do snap "$idx" "${ph}0" "$rowdir"; done
    for ((r = 1; r <= 12; r++)); do
        sym_prefixed_create "$dir" "touch-$ph-r$r" "$burst" >/dev/null ||
            die "$label: the requester's touch round $r into $dir failed"
        sleep "$(python3 -c "print($beat_ms/1000)")"
        handed=0
        for idx in "$h" "$q" 0; do
            snap "$idx" "${ph}1" "$rowdir"
            v="$(sym_delta "$rowdir" "$idx" "$ph" slot_handovers)"
            handed=$((handed + v))
        done
        [ "$handed" -ge 1 ] && break
    done
    [ "$handed" -ge 1 ] ||
        die "$label: the directory's slot never moved from m$h to m$q in 12 dominating rounds (slot_handovers 0 — the handover premise)"
    # The holder RELEASED (the manager counts the wire `ReleaseSlot` that
    # spent its recall as the handover); the requester TAKES the slot at
    # its next op into the directory — the offer stands 10 s while the
    # holder's recall rides its 10 s renewal beat, so the offer routinely
    # lapses before the release lands (`slot_offers_expired`) and a LIVE
    # requester's next ship first-touches the unleased slot at the door.
    # Keep the requester live until it holds the slot (its wire acquire).
    local took=0 r2
    for ((r2 = 0; r2 <= 6; r2++)); do
        snap "$q" "${ph}1" "$rowdir"
        took="$(sym_delta "$rowdir" "$q" "$ph" joined_wire_acquires)"
        [ "$took" -ge 1 ] && break
        sym_prefixed_create "$dir" "touch-$ph-take$r2" 8 >/dev/null ||
            die "$label: the requester's first-touch round $r2 into $dir failed"
        sleep 2
    done
    [ "$took" -ge 1 ] ||
        die "$label: the requester m$q never took the released slot (joined_wire_acquires unmoved after the holder's release) — the leg's premise, the door's first touch"
    log "$label: the directory's slot moved m$h → m$q after $r round(s) of $burst creates (slot_handovers +$handed; the requester's wire acquire +$took)"
}
# The corpse: a file whose record lives in its DIRECTORY's slot — the
# parent-slot affinity mint (`affinity_mints` +1 at the creator). On a set
# of V metadata volumes a directory's children mint round-robin over the
# volumes (`pick_mint_volume`) and the affinity applies only when the child
# lands on the parent's volume, so the create is tried up to 2 × V times
# (empty — the mint decides at the create; the 8 MiB is written into the
# one that took the affinity, the others removed). Echoes the path.
sym_rh_make_corpse() { # label creator_idx dir
    local label="$1" idx="$2" dir="$3" volumes k aff0 f
    volumes="$(stat_field "$idx" symmetric_meta | python3 -c 'import ast,sys; v=ast.literal_eval(sys.stdin.read().strip() or "1"); print(len(v) if isinstance(v, list) else 1)')"
    for ((k = 0; k < 2 * volumes; k++)); do
        f="$dir/corpse-$k"
        aff0="$(stat_sum "$idx" affinity_mints)"
        : >"$f" || die "$label: the corpse's create failed ($f)"
        if [ "$(( $(stat_sum "$idx" affinity_mints) - aff0 ))" -ge 1 ]; then
            dd if=/dev/urandom of="$f" bs=1M count=8 conv=fsync status=none || die "$label: the corpse's write failed ($f)"
            echo "$f"
            return 0
        fi
        rm -f "$f"
    done
    die "$label: no create into $dir took the parent-slot affinity in $((2 * volumes)) tries — create the fleet with SQUEEZEFS_SYM_AFFINITY_MAX_MB=64 (m$idx a_max $(stat_first_sym "$idx" affinity_a_max_bytes) B)"
}
sym_rh_wait() { # label deadline_s cmd... (a shell condition string)
    local label="$1" deadline="$2" cond="$3" t
    for ((t = 0; t < deadline; t++)); do
        eval "$cond" && return 0
        sleep 1
    done
    die "$label: not within ${deadline}s — condition: $cond"
}
leg_sym_reclaim_hint() {
    require_symmetric
    local joiners
    mapfile -t joiners < <(joiner_idxs)
    [ "${#joiners[@]}" -ge 2 ] ||
        die "sym-reclaim-hint needs ≥ 2 joined writers — create the fleet with: sudo SQUEEZEFS_SYM_AFFINITY_MAX_MB=64 tests/mw_fleet.sh create N=2 --symmetric --writers=3"
    [[ "$SYM_RH_FILES" =~ ^[1-9][0-9]*$ ]] || die "--rh-files takes a positive integer (got '$SYM_RH_FILES')"
    sym_quiet_or_die sym-reclaim-hint
    local rowdir a b idx readers=() writers
    rowdir="$STATE/rows/symrh-$(date +%s)"
    mkdir -p "$rowdir"
    a="${joiners[0]}"
    b="${joiners[1]}"
    writers=(0 "${joiners[@]}")
    for idx in $(member_idxs); do [ "$(role_of "$idx")" = "reader" ] && readers+=("$idx"); done
    local beat_ms n_floor burst
    beat_ms="$(stat_field 0 membership_renew_cadence_ms)"
    [ -n "$beat_ms" ] && [ "$beat_ms" != "0" ] || beat_ms=10000
    n_floor="$(stat_first_sym "$a" slot_offer_n_floor)"
    [ -n "$n_floor" ] && [ "$n_floor" -ge 2 ] || n_floor=2
    burst=$((n_floor * 4))
    [ "$burst" -ge 64 ] || burst=64
    local block_size blocks
    block_size="$(python3 -c 'import json,sys; print(json.load(open(sys.argv[1]))["block_size"])' "$(mnt_of 0)/.config")"
    [ -n "$block_size" ] && [ "$block_size" -gt 0 ] || die "sym-reclaim-hint: the manager's .config names no block_size"
    blocks=$(( (8 * 1024 * 1024 + block_size - 1) / block_size ))
    log "sym-reclaim-hint: manager m0, forgetter m$a, requester/new holder m$b, readers (${readers[*]:-none}); beat ${beat_ms} ms, burst $burst, block $block_size B (an 8 MiB corpse = $blocks block(s)); $SYM_RH_FILES files in the box shape"
    # The log baselines and the closure's start snapshot.
    declare -A log0
    for idx in $(member_idxs); do log0[$idx]="$(wc -l <"$STATE/m$idx.log" 2>/dev/null || echo 0)"; done
    for idx in "${writers[@]}" "${readers[@]}"; do snap "$idx" s0 "$rowdir"; done
    local mmnt amnt bmnt
    mmnt="$(mnt_of 0)"; amnt="$(mnt_of "$a")"; bmnt="$(mnt_of "$b")"

    # ---- (A) the box's shape: a token client's forgets of the holder's corpses.
    mkdir -p "$mmnt/rh-mgr" || die "sym-reclaim-hint A: mkdir rh-mgr at the manager failed"
    local i
    for ((i = 0; i < SYM_RH_FILES; i++)); do
        printf 'rh:%05d' "$i" | dd of="$mmnt/rh-mgr/f$(printf '%05d' "$i")" conv=fsync status=none ||
            die "sym-reclaim-hint A: the manager's write of f$i failed"
    done
    # The joiner (and every reader) instantiates every inode as a token
    # client: lookup + getattr + read through its own mount.
    find "$amnt/rh-mgr" -type f -exec cat {} + >/dev/null || die "sym-reclaim-hint A: m$a's read of the manager's tree failed"
    for idx in "${readers[@]}"; do
        find "$(mnt_of "$idx")/rh-mgr" -type f -exec cat {} + >/dev/null || die "sym-reclaim-hint A: reader m$idx's read of the manager's tree failed"
    done
    # The READ phase's closes (review round 3, Issue 17): every `cat`'s
    # last close runs the RELEASE handler's unlink-while-open probe, and
    # the joiner's STANDING TOKEN on each file reads `nlink 1` — no corpse,
    # no hint, no resolve at the manager.
    sleep 1
    for idx in "${writers[@]}" "${readers[@]}"; do snap "$idx" a0 "$rowdir"; done
    local live_skipped live_inos live_resolves
    live_skipped="$(python3 - "$rowdir" "$a" <<'PYEOF'
import json, sys
r, i = sys.argv[1], sys.argv[2]
def m(ph):
    d = json.load(open(f"{r}/m{i}_p{ph}.json")); return d.get("metrics", d)
print(int(m("a0").get("reclaim_hint_skipped_live", 0)) - int(m("s0").get("reclaim_hint_skipped_live", 0)))
PYEOF
)"
    live_inos="$(python3 - "$rowdir" "$a" <<'PYEOF'
import json, sys
r, i = sys.argv[1], sys.argv[2]
def m(ph):
    d = json.load(open(f"{r}/m{i}_p{ph}.json")); return d.get("metrics", d)
print(int(m("a0").get("reclaim_hint_inos_shipped", 0)) - int(m("s0").get("reclaim_hint_inos_shipped", 0)))
PYEOF
)"
    live_resolves="$(python3 - "$rowdir" 0 <<'PYEOF'
import json, sys
r, i = sys.argv[1], sys.argv[2]
def m(ph):
    d = json.load(open(f"{r}/m{i}_p{ph}.json")); v = d.get("metrics", d).get("slot_resolve_rpcs", 0)
    return sum(x for x in v if isinstance(x, (int, float))) if isinstance(v, list) else int(v or 0)
print(m("a0") - m("s0"))
PYEOF
)"
    [ "$live_skipped" -ge "$SYM_RH_FILES" ] ||
        die "sym-reclaim-hint A: m$a reclaim_hint_skipped_live +$live_skipped for $SYM_RH_FILES live-file closes — the standing-token arm did not read the files live"
    [ "$live_inos" = "0" ] ||
        die "sym-reclaim-hint A: m$a shipped $live_inos hinted ino(s) for the closes of LIVE files (a standing token reading nlink ≥ 1 is no corpse)"
    log "sym-reclaim-hint A (the read phase): $SYM_RH_FILES live-file closes at m$a → reclaim_hint_skipped_live +$live_skipped, hinted inos +$live_inos, the manager's slot_resolve_rpcs +$live_resolves"
    rm -rf "$mmnt/rh-mgr" || die "sym-reclaim-hint A: the manager's rm -rf failed"
    # Every unreferenced inode the recall's prune left behind is evicted:
    # the kernel FORGETs at every mount that held one.
    sync; echo 2 >/proc/sys/vm/drop_caches 2>/dev/null || true
    sleep 3
    for idx in "${writers[@]}" "${readers[@]}"; do snap "$idx" a1 "$rowdir"; done
    local v a_foreign a_shipped a_inos a_fail a_served
    a_foreign="$(sym_delta "$rowdir" "$a" a reclaim_foreign_slot_forgets)"
    a_shipped="$(sym_delta "$rowdir" "$a" a reclaim_hints_shipped)"
    a_inos="$(sym_delta "$rowdir" "$a" a reclaim_hint_inos_shipped)"
    a_fail="$(sym_delta "$rowdir" "$a" a reclaim_hint_failures)"
    a_served="$(sym_delta "$rowdir" 0 a reclaim_hints_served)"
    [ "$a_foreign" -ge 1 ] || die "sym-reclaim-hint A: m$a reclaim_foreign_slot_forgets +$a_foreign — the joiner's kernel forgot none of the manager's $SYM_RH_FILES corpses (no recall → prune → FORGET reached its reclaim)"
    [ "$a_shipped" -ge 1 ] && [ "$a_inos" -ge "$a_foreign" ] || die "sym-reclaim-hint A: m$a shipped $a_shipped hint(s) / $a_inos ino(s) for $a_foreign foreign forgets"
    [ "$a_fail" = "0" ] || die "sym-reclaim-hint A: m$a reclaim_hint_failures +$a_fail"
    [ "$a_served" -ge 1 ] || die "sym-reclaim-hint A: the manager served $a_served hinted ino(s) — the joiner's hints reached no sink at the manager"
    for idx in "${writers[@]}" "${readers[@]}"; do
        v="$(sym_delta "$rowdir" "$idx" a reclaim_destroy_refused_release_failed)"
        [ "$v" = "0" ] || die "sym-reclaim-hint A: m$idx withheld $v destroy(ies) (reclaim_destroy_refused_release_failed)"
    done
    for idx in "${readers[@]}"; do
        v="$(sym_delta "$rowdir" "$idx" a reclaim_reader_forgets)"
        [ "$v" -ge 1 ] || warn "sym-reclaim-hint A: reader m$idx reclaim_reader_forgets +$v (its kernel kept the inodes past the settle — no verdict)"
        [ "$(sym_delta "$rowdir" "$idx" a reclaim_hints_shipped)" = "0" ] || die "sym-reclaim-hint A: reader m$idx shipped a hint — a reader hints nobody"
    done
    log "sym-reclaim-hint A: m$a forgot $a_foreign of the manager's corpses as a token client → $a_shipped hint(s) / $a_inos ino(s) → the manager served $a_served; nothing withheld anywhere"

    # ---- (B) the MOVED-slot corpse (class ii).
    local pop_b0 pop_b1 hold_pid refs_b0 free_b0 corpse
    pop_b0="$(stat_sum 0 data_alloc_bitmap_population)"
    mkdir -p "$amnt/rh-ja" || die "sym-reclaim-hint B: mkdir rh-ja at m$a failed"
    corpse="$(sym_rh_make_corpse sym-reclaim-hint-B "$a" "$amnt/rh-ja")"
    hold_pid="$(sym_rh_hold_open "$corpse")"
    sleep 0.5
    kill -0 "$hold_pid" 2>/dev/null || die "sym-reclaim-hint B: the holder process died"
    rm "$corpse" || die "sym-reclaim-hint B: the unlink failed"
    sleep 1
    pop_b1="$(stat_sum 0 data_alloc_bitmap_population)"
    sym_rh_handover sym-reclaim-hint-B "$a" "$b" "$bmnt/rh-ja" "$rowdir" hb "$burst" "$beat_ms"
    for idx in "${writers[@]}"; do snap "$idx" b0 "$rowdir"; done
    refs_b0="$(stat_sum "$b" meta_kv_block_refs_released)"
    free_b0="$(stat_sum 0 meta_ship_publish.free_served_blocks)"
    # The close: m$a's kernel FORGETs the corpse — its slot is m$b's now.
    kill "$hold_pid" 2>/dev/null || true
    wait "$hold_pid" 2>/dev/null || true
    sym_rh_wait sym-reclaim-hint-B 90 "[ \$(( \$(stat_sum $b meta_kv_block_refs_released) - $refs_b0 )) -ge $blocks ]"
    sym_rh_wait sym-reclaim-hint-B 90 "[ \$(( \$(stat_sum 0 meta_ship_publish.free_served_blocks) - $free_b0 )) -ge $blocks ]"
    sym_rh_wait sym-reclaim-hint-B 90 "[ \$(stat_sum 0 data_alloc_bitmap_population) -le $(( pop_b1 - blocks )) ]"
    for idx in "${writers[@]}"; do snap "$idx" b1 "$rowdir"; done
    local b_foreign b_unleased b_shipped b_inos b_served
    b_foreign="$(sym_delta "$rowdir" "$a" b reclaim_foreign_slot_forgets)"
    b_unleased="$(sym_delta "$rowdir" "$a" b reclaim_unleased_slot_forgets)"
    b_shipped="$(sym_delta "$rowdir" "$a" b reclaim_hints_shipped)"
    b_inos="$(sym_delta "$rowdir" "$a" b reclaim_hint_inos_shipped)"
    b_served="$(sym_delta "$rowdir" "$b" b reclaim_hints_served)"
    [ "$b_foreign" -ge 1 ] || die "sym-reclaim-hint B: m$a counted the corpse's forget foreign $b_foreign time(s)"
    [ "$b_shipped" -ge 1 ] && [ "$b_inos" -ge 1 ] || die "sym-reclaim-hint B: m$a shipped $b_shipped hint(s) / $b_inos ino(s)"
    [ "$(sym_delta "$rowdir" "$a" b reclaim_hint_failures)" = "0" ] || die "sym-reclaim-hint B: m$a reclaim_hint_failures moved"
    [ "$b_served" -ge 1 ] || die "sym-reclaim-hint B: the new holder m$b served $b_served hinted ino(s)"
    for idx in "${writers[@]}"; do
        v="$(sym_delta "$rowdir" "$idx" b reclaim_destroy_refused_release_failed)"
        [ "$v" = "0" ] || die "sym-reclaim-hint B: m$idx withheld $v destroy(ies)"
    done
    log "sym-reclaim-hint B (the MOVED-slot corpse): m$a's close → FORGET counted foreign ($b_foreign; unleased $b_unleased) → $b_shipped hint / $b_inos ino → m$b served $b_served and destroyed it: m$b meta_kv_block_refs_released +$(( $(stat_sum "$b" meta_kv_block_refs_released) - refs_b0 )), the manager free_served_blocks +$(( $(stat_sum 0 meta_ship_publish.free_served_blocks) - free_b0 )), data_alloc_bitmap_population $pop_b1 → $(stat_sum 0 data_alloc_bitmap_population) (≥ $blocks block(s) cleared; before the corpse's create $pop_b0 — a joiner's grant window is SET whole at its carve, so the population moves by the window, never by the file)"

    # ---- (C) the UNLEASED corpse (class i).
    local pop_c1 hold2_pid refs_c0 pop_c0 corpse2
    pop_c0="$(stat_sum 0 data_alloc_bitmap_population)"
    mkdir -p "$amnt/rh-jb" || die "sym-reclaim-hint C: mkdir rh-jb at m$a failed"
    corpse2="$(sym_rh_make_corpse sym-reclaim-hint-C "$a" "$amnt/rh-jb")"
    hold2_pid="$(sym_rh_hold_open "$corpse2")"
    sleep 0.5
    kill -0 "$hold2_pid" 2>/dev/null || die "sym-reclaim-hint C: the holder process died"
    rm "$corpse2" || die "sym-reclaim-hint C: the unlink failed"
    sleep 1
    pop_c1="$(stat_sum 0 data_alloc_bitmap_population)"
    sym_rh_handover sym-reclaim-hint-C "$a" "$b" "$bmnt/rh-jb" "$rowdir" hc "$burst" "$beat_ms"
    # The new holder LEAVES cleanly: its slots go Unleased at tree 0 — the
    # corpse now sits in a tree nobody leases, at the manager. Its process
    # counters restart at the remount, so its served / forwarded /
    # misrouted words up to the leave are carried into the closure by hand.
    local b_pre_served b_pre_fwd b_pre_mis
    b_pre_served="$(stat_sum "$b" reclaim_hints_served)"
    b_pre_fwd="$(stat_sum "$b" reclaim_hints_forwarded)"
    b_pre_mis="$(stat_sum "$b" reclaim_hints_misrouted)"
    "$MWFLEET" unmount "$b" || die "sym-reclaim-hint C: m$b's leave failed"
    wait_for_unmounted "$bmnt"
    sym_rh_wait sym-reclaim-hint-C 60 "[ \"\$(stat_all_eq 0 appenders_known ${#joiners[@]})\" = 1 ]"
    for idx in 0 "$a"; do snap "$idx" c0 "$rowdir"; done
    refs_c0="$(stat_sum 0 meta_kv_block_refs_released)"
    kill "$hold2_pid" 2>/dev/null || true
    wait "$hold2_pid" 2>/dev/null || true
    sym_rh_wait sym-reclaim-hint-C 90 "[ \$(( \$(stat_sum 0 meta_kv_block_refs_released) - $refs_c0 )) -ge $blocks ]"
    sym_rh_wait sym-reclaim-hint-C 90 "[ \$(stat_sum 0 data_alloc_bitmap_population) -le $(( pop_c1 - blocks )) ]"
    for idx in 0 "$a"; do snap "$idx" c1 "$rowdir"; done
    local c_unleased c_foreign c_shipped c_inos c_served
    c_unleased="$(sym_delta "$rowdir" "$a" c reclaim_unleased_slot_forgets)"
    c_foreign="$(sym_delta "$rowdir" "$a" c reclaim_foreign_slot_forgets)"
    c_shipped="$(sym_delta "$rowdir" "$a" c reclaim_hints_shipped)"
    c_inos="$(sym_delta "$rowdir" "$a" c reclaim_hint_inos_shipped)"
    c_served="$(sym_delta "$rowdir" 0 c reclaim_hints_served)"
    [ "$c_unleased" -ge 1 ] || die "sym-reclaim-hint C: m$a reclaim_unleased_slot_forgets +$c_unleased (foreign +$c_foreign) — the corpse's slot was not read UNLEASED at the forget"
    [ "$c_shipped" -ge 1 ] && [ "$c_inos" -ge 1 ] || die "sym-reclaim-hint C: m$a shipped $c_shipped hint(s) / $c_inos ino(s)"
    [ "$(sym_delta "$rowdir" "$a" c reclaim_hint_failures)" = "0" ] || die "sym-reclaim-hint C: m$a reclaim_hint_failures moved"
    [ "$c_served" -ge 1 ] || die "sym-reclaim-hint C: the manager served $c_served hinted ino(s)"
    for idx in 0 "$a"; do
        v="$(sym_delta "$rowdir" "$idx" c reclaim_destroy_refused_release_failed)"
        [ "$v" = "0" ] || die "sym-reclaim-hint C: m$idx withheld $v destroy(ies)"
    done
    log "sym-reclaim-hint C (the UNLEASED corpse): m$b left, m$a's close → FORGET counted unleased ($c_unleased) → $c_shipped hint / $c_inos ino → the manager served $c_served and destroyed it: meta_kv_block_refs_released +$(( $(stat_sum 0 meta_kv_block_refs_released) - refs_c0 )), data_alloc_bitmap_population $pop_c1 → $(stat_sum 0 data_alloc_bitmap_population) (≥ $blocks cleared; before the create $pop_c0)"
    "$MWFLEET" mount "$b" || die "sym-reclaim-hint C: m$b's re-mount failed"
    sym_rh_wait sym-reclaim-hint-C 60 "[ \"\$(stat_all_eq 0 appenders_known $(( ${#joiners[@]} + 1 )))\" = 1 ]"

    # ---- The closure at rest, the logs, the oracle, the census.
    rm -rf "$mmnt/rh-ja" "$mmnt/rh-jb" 2>/dev/null || true
    sleep 3
    local shipped_sum served_sum fwd_sum mis_sum fail_sum
    sym_rh_closure() {
        shipped_sum=0; served_sum=0; fwd_sum=0; mis_sum=0; fail_sum=0
        local i
        for i in "${writers[@]}"; do
            snap "$i" s1 "$rowdir"
            shipped_sum=$(( shipped_sum + $(sym_delta "$rowdir" "$i" s reclaim_hint_inos_shipped) ))
            served_sum=$(( served_sum + $(sym_delta "$rowdir" "$i" s reclaim_hints_served) ))
            fwd_sum=$(( fwd_sum + $(sym_delta "$rowdir" "$i" s reclaim_hints_forwarded) ))
            mis_sum=$(( mis_sum + $(sym_delta "$rowdir" "$i" s reclaim_hints_misrouted) ))
            fail_sum=$(( fail_sum + $(sym_delta "$rowdir" "$i" s reclaim_hint_failures) ))
        done
        # m$b's pre-leave words (its s1 snapshot is the REMOUNTED process's).
        served_sum=$(( served_sum + b_pre_served ))
        fwd_sum=$(( fwd_sum + b_pre_fwd ))
        mis_sum=$(( mis_sum + b_pre_mis ))
        [ "$shipped_sum" = "$(( served_sum + fwd_sum + mis_sum ))" ]
    }
    local t
    for ((t = 0; t < 30; t++)); do sym_rh_closure && break; sleep 1; done
    sym_rh_closure || die "sym-reclaim-hint: the family does not close at rest — Σ hint_inos_shipped $shipped_sum ≠ Σ served $served_sum + forwarded $fwd_sum + misrouted $mis_sum (failures $fail_sum)"
    [ "$fail_sum" = "0" ] || die "sym-reclaim-hint: Σ reclaim_hint_failures = $fail_sum across the writers"
    for idx in $(member_idxs); do sym_rh_logs_clean sym-reclaim-hint "$idx" "${log0[$idx]}"; done
    local refused_sum=0
    for idx in "${writers[@]}" "${readers[@]}"; do
        v="$(sym_delta "$rowdir" "$idx" s reclaim_destroy_refused_release_failed 2>/dev/null || echo 0)"
        refused_sum=$(( refused_sum + ${v:-0} ))
    done
    echo "== PR 13h sym-reclaim-hint: Σ reclaim_hint_inos_shipped=$shipped_sum ≡ Σ served=$served_sum + forwarded=$fwd_sum + misrouted=$mis_sum, reclaim_hint_failures=$fail_sum, reclaim_destroy_refused_release_failed=$refused_sum, 0 'corrupt KV encoding' / 'destroy WITHHELD' lines; A-read: $SYM_RH_FILES live closes → skipped_live +$live_skipped, hinted +$live_inos, manager slot_resolve_rpcs +$live_resolves; A: m$a foreign +$a_foreign → manager served +$a_served; B (moved slot): foreign +$b_foreign → m$b served +$b_served, refs released ≥ $blocks, bitmap cleared ≥ $blocks; C (unleased): unleased +$c_unleased → manager served +$c_served, refs released ≥ $blocks, bitmap cleared ≥ $blocks$SYM_BUSY_ROW ==" | tee "$rowdir/symrh-table.txt"
    sym_oracle sym-reclaim-hint "$rowdir"
    sym_post_leave_census sym-reclaim-hint "$rowdir" "$(date +%s)" "${writers[@]}"
    log "sym-reclaim-hint PUBLISHED (table + snapshots in $rowdir)"
}

# --- gate 5: sym-readers ----------------------------------------------------
leg_sym_readers() {
    require_symmetric
    [ "${TOKEN_READERS:-0}" = "1" ] ||
        die "sym-readers needs a --token-readers fleet (the readers are read-token clients, PR 5's §5.7.2) — create it with: sudo tests/mw_fleet.sh create N=32 --symmetric --writers=1 --token-readers"
    local readers writer
    mapfile -t readers < <(awk -F'\t' '$2=="reader" {print $1}' "$MEMBERS" 2>/dev/null | sort -n)
    [ "${#readers[@]}" -ge 1 ] || die "sym-readers needs ≥ 1 reader"
    writer="$(joiner_idxs | head -1)"
    [ -n "$writer" ] || writer=0
    local rowdir w_mnt d rel idx
    rowdir="$STATE/rows/symreaders-$(date +%s)"
    mkdir -p "$rowdir"
    w_mnt="$(mnt_of "$writer")"
    d="$w_mnt/readers-$(date +%s)"
    rel="${d#"$w_mnt"}"
    mkdir "$d" || die "sym-readers: the writer's mkdir failed"
    log "sym-readers: writer m$writer, ${#readers[@]} token reader(s); exactness at the NEXT resolve (no sleep), the broadcast recall shape, the recall-driven free-grace hold"
    for idx in "${readers[@]}" "$writer"; do
        [ "$(stat_field "$idx" reader_staleness_bound_ms)" = "0" ] ||
            die "sym-readers: reader_staleness_bound_ms != 0 on m$idx (R-SYM-4: 0 under tokens)"
        snap "$idx" ex0 "$rowdir"
    done
    # --- exactness: create / rename / setattr, each visible at EVERY
    #     reader's next resolve — the assertion runs the instant the
    #     writer's syscall returned.
    local misses=0 r_mnt got
    : >"$d/f"
    for idx in "${readers[@]}"; do
        r_mnt="$(mnt_of "$idx")"
        stat "$r_mnt$rel/f" >/dev/null 2>&1 || { misses=$((misses + 1)); echo "MISS create: m$idx did not see $rel/f" >>"$rowdir/misses.txt"; }
    done
    mv "$d/f" "$d/g"
    for idx in "${readers[@]}"; do
        r_mnt="$(mnt_of "$idx")"
        stat "$r_mnt$rel/g" >/dev/null 2>&1 || { misses=$((misses + 1)); echo "MISS rename(new): m$idx did not see $rel/g" >>"$rowdir/misses.txt"; }
        if stat "$r_mnt$rel/f" >/dev/null 2>&1; then misses=$((misses + 1)); echo "MISS rename(old): m$idx still sees $rel/f" >>"$rowdir/misses.txt"; fi
    done
    chmod 600 "$d/g"
    for idx in "${readers[@]}"; do
        r_mnt="$(mnt_of "$idx")"
        got="$(stat -c %a "$r_mnt$rel/g" 2>/dev/null || echo none)"
        [ "$got" = "600" ] || { misses=$((misses + 1)); echo "MISS setattr: m$idx read mode $got" >>"$rowdir/misses.txt"; }
    done
    [ "$misses" = "0" ] || die "sym-readers EXACTNESS RED: $misses miss(es) — see $rowdir/misses.txt"
    log "sym-readers: create / rename / setattr exact at every reader's next resolve (0 misses over ${#readers[@]} reader(s))"

    # --- the broadcast shape: every reader holds ONE file's token, the
    #     writer publishes K times; each publish recalls one token per
    #     reader, and every reader re-resolves the new size exactly.
    local k=5 i size
    : >"$d/bcast"
    for idx in "${readers[@]}"; do stat "$(mnt_of "$idx")$rel/bcast" >/dev/null; done
    for idx in "${readers[@]}" "$writer"; do snap "$idx" bc0 "$rowdir"; done
    for ((i = 1; i <= k; i++)); do
        dd if=/dev/zero of="$d/bcast" bs=4096 count="$i" conv=fsync status=none
        for idx in "${readers[@]}"; do
            size="$(stat -c %s "$(mnt_of "$idx")$rel/bcast" 2>/dev/null || echo -1)"
            [ "$size" = "$((i * 4096))" ] || { misses=$((misses + 1)); echo "MISS bcast $i: m$idx read size $size" >>"$rowdir/misses.txt"; }
        done
    done
    sleep 1
    for idx in "${readers[@]}" "$writer"; do snap "$idx" bc1 "$rowdir"; done
    [ "$misses" = "0" ] || die "sym-readers BROADCAST RED: $misses miss(es) — see $rowdir/misses.txt"
    local recalls acks fanout_p99 timeouts recv=0 racks=0
    recalls="$(sym_delta "$rowdir" "$writer" bc dlm_token_recalls)"
    acks="$(sym_delta "$rowdir" "$writer" bc dlm_token_recall_acks)"
    timeouts="$(stat_sum "$writer" dlm_token_recall_timeouts_live)"
    fanout_p99="$(stat_field "$writer" dlm_token_recall_fanout_p99 | tr -d '[] ' | tr ',' ' ' | awk '{m=0; for(i=1;i<=NF;i++) if($i+0>m) m=$i+0; print m}')"
    for idx in "${readers[@]}"; do
        v="$(sym_delta "$rowdir" "$idx" bc dlm_token_recalls_received)"
        recv=$((recv + v))
        v="$(sym_delta "$rowdir" "$idx" bc dlm_token_recalls_acked)"
        racks=$((racks + v))
    done
    local rtt_us
    rtt_us="$(sym_phase_mean_us "$writer" dlm_token_recall_rtt_ns)"
    echo "== PR 13 gate 5: broadcast shape — 1 writer × ${#readers[@]} reader(s) of one file, $k publishes: dlm_token_recalls=$recalls (law: mutations × holders = $((k * ${#readers[@]}))) acks=$acks readers_received=$recv readers_acked=$racks fanout_p99=$fanout_p99 (≡ the readers' bucket edge) timeouts_live=$timeouts recall_rtt_mean=${rtt_us}us ==" | tee "$rowdir/symreaders-table.txt"
    [ "$recv" = "$recalls" ] && [ "$racks" = "$recalls" ] ||
        die "sym-readers: the readers received $recv / acked $racks recalls against the holder's $recalls (the reader face must fold every per-holder plane)"
    # THE ENGAGEMENT LAW: recalls ≡ mutations × holders (every reader
    # re-held the token before the next publish — the stat above), the
    # fan-out's p99 ≡ the reader count, no live timeout.
    [ "$recalls" = "$((k * ${#readers[@]}))" ] ||
        die "sym-readers: dlm_token_recalls=$recalls ≠ mutations × holders = $((k * ${#readers[@]}))"
    [ "$recalls" = "$acks" ] || die "sym-readers: recalls $recalls ≠ acks $acks (closure: recalls ≡ acks + expired_with_lease; nothing expired here)"
    # The fan-out p99 is a LOG-BUCKET histogram's upper bound
    # (`QueueDepthHistogram::percentile`: 0, 1, 2, <=4, <=8, … — the
    # reader count's bucket edge), so 31 readers read 32 (PR 13c: the
    # first 32-member fleet to FORM — F-B3's cap had refused every earlier
    # one — read `p99=32 ≠ readers 31` on a law only a power-of-two reader
    # count could meet). The exact per-batch count is `dlm_token_recalls`
    # ÷ batches, asserted above as mutations × holders.
    local fanout_edge
    fanout_edge="$(python3 -c "
r=${#readers[@]}
print(r if r <= 2 else 1 << ((r - 1).bit_length()))")"
    [ "$fanout_p99" = "$fanout_edge" ] ||
        die "sym-readers: dlm_token_recall_fanout_p99=$fanout_p99 ≠ the bucket edge $fanout_edge of ${#readers[@]} readers"
    [ "$timeouts" = "0" ] || die "sym-readers: dlm_token_recall_timeouts_live=$timeouts"

    # --- the recall-driven free-grace hold: a striped file's block
    #     displaced under a live reader's token — the freeing publish's
    #     recall IS the qualification (free_grace_recall_gated_frees), the
    #     hold ≈ one recall RTT, closure deferrals ≡ releases + offsets.
    for idx in "${readers[@]}" "$writer" 0; do snap "$idx" fg0 "$rowdir"; done
    dd if=/dev/urandom of="$d/grace" bs=4M count=2 conv=fsync status=none
    for idx in "${readers[@]}"; do dd if="$(mnt_of "$idx")$rel/grace" of=/dev/null bs=4M count=1 status=none 2>/dev/null || true; done
    for ((i = 1; i <= 4; i++)); do
        dd if=/dev/urandom of="$d/grace" bs=4M count=1 conv=notrunc,fsync status=none
        for idx in "${readers[@]}"; do dd if="$(mnt_of "$idx")$rel/grace" of=/dev/null bs=4M count=1 status=none 2>/dev/null || true; done
    done
    sleep 3
    for idx in "${readers[@]}" "$writer" 0; do snap "$idx" fg1 "$rowdir"; done
    local holder_of_frees gated hold deferrals releases offsets
    # The terminal free lands on the data volume's ALLOCATION HOLDER (the
    # manager on this fleet); its ledger is the closure's.
    holder_of_frees=0
    gated="$(sym_delta "$rowdir" "$holder_of_frees" fg free_grace_recall_gated_frees)"
    hold="$(stat_field "$holder_of_frees" free_grace_hold_ms)"
    deferrals="$(stat_field "$holder_of_frees" free_grace_deferrals)"
    releases="$(stat_field "$holder_of_frees" free_grace_releases)"
    offsets="$(stat_field "$holder_of_frees" free_grace_offsets)"
    echo "   free-grace (recall-driven): recall_gated_frees=$gated (frees published DIRECTLY — the freeing publish's recall IS the qualification; the hold under tokens is the recall RTT ${rtt_us}us, vs the S5 composite's 2,724 ms) free_grace_hold_ms(ring)=$hold deferrals=$deferrals releases=$releases offsets=$offsets (closure deferrals ≡ releases + offsets) at the allocation holder m$holder_of_frees ==" | tee -a "$rowdir/symreaders-table.txt"
    [ "$deferrals" = "$((releases + offsets))" ] ||
        die "sym-readers: free_grace_deferrals=$deferrals ≠ releases + offsets = $((releases + offsets))"
    [ "$gated" -ge 1 ] ||
        die "sym-readers: free_grace_recall_gated_frees=$gated — no displaced block was qualified by its recall (the ring's timeout path served every free)"
    for idx in "${readers[@]}" "$writer"; do sym_zero_set sym-readers "$idx"; done
    rm -rf "$d" 2>/dev/null || true
    sym_oracle sym-readers "$rowdir"
    log "sym-readers PUBLISHED (table + snapshots in $rowdir)"
}

leg_s7_kill_matrix() {
    require_mw
    s7_kill_body
}

# The kill matrix's body: `sym` = the symmetric fleet's leg (PR 10) — the
# same rounds with the successor's own-residue recovery and the symmetric
# tripwires asserted per round.
s7_kill_body() { # [sym]
    local sym="${1:-}" leg="s7-kill-matrix" tag="s7kill"
    [ "$sym" = "sym" ] && leg="sym-crash" && tag="symcrash"
    local rowdir w_mnt reader_idx
    rowdir="$STATE/rows/$tag-$(date +%s)"
    mkdir -p "$rowdir"
    w_mnt="$(mnt_of 0)"
    reader_idx="$(member_idxs | awk '$1!=0' | head -1)"
    log "$leg: kill -9 x$S7_ROUNDS of the ARMED writer at randomized phases under sustained write load; per round: remount (WERO takeover over the dead incarnation's standing reservation) + FULL online fsck with the C8 oracle. COUNTED-RESTART discipline applies."

    local round phase_ms dd_pid t_kill t_up out findings drift fence_ref trip backstops fm
    printf '%-6s %-9s %-9s %-10s %-6s %-10s %-6s %s\n' ROUND PHASE_MS REMOUNT_S FSCK DRIFT FENCE_REF TRIP VERDICT | tee "$rowdir/matrix.tsv"
    for ((round = 1; round <= S7_ROUNDS; round++)); do
        # Sustained load, randomized kill phase (0.5 .. 8.5 s into it).
        rm -f "$w_mnt/s7kill.dat" 2>/dev/null || true
        # A load that OUTLASTS the longest kill phase whatever the box's
        # bandwidth: 4 GiB passes rewritten in place until the kill (one
        # 16 GiB pass finished inside an 8.2 s phase at 2 GB/s on the
        # zram devsub and read as "write load died" — PR 13's batch).
        (
            while :; do
                dd if=/dev/zero of="$w_mnt/s7kill.dat" bs=1M count=4096 conv=fsync,notrunc status=none || exit 1
            done
        ) &
        dd_pid=$!
        # The ACKED-WRITES ORACLE (PR 10, review round 1, Issue 14): a
        # ledger of names the client created and FSYNCED before the kill —
        # every one MUST resolve on the successor with its content intact.
        # Runs beside the dd load until the kill so the acked set straddles
        # the kill phase. "Acked" = a per-file `fsync(2)` RETURNED (`dd
        # conv=fsync` — the daemon's fsync ladder ran for THAT file: the
        # journal barrier included), never `sync -f`: that is `syncfs(2)`,
        # which the FUSE fork does not serve (no `FUSE_SYNCFS`), so the
        # kernel pushes writeback and returns success with no daemon
        # barrier behind it — a word for "create acked + writeback pushed",
        # exact for a process kill and an overclaim for power loss (review
        # round 2, Issue 27).
        local ack_dir ack_ledger ack_pid
        ack_dir="$w_mnt/acked-r$round"
        ack_ledger="$rowdir/acked-r$round.ledger"
        mkdir -p "$ack_dir"
        : >"$ack_ledger"
        (
            i=0
            while :; do
                f="$ack_dir/f$(printf '%06d' "$i")"
                if printf 'r%s:%s\n' "$round" "$i" |
                    dd of="$f" conv=fsync status=none 2>/dev/null; then
                    echo "$f" >>"$ack_ledger"
                fi
                i=$((i + 1))
            done
        ) &
        ack_pid=$!
        phase_ms=$((500 + RANDOM % 8000))
        sleep "$(python3 -c "print($phase_ms/1000)")"
        kill -0 "$dd_pid" 2>/dev/null ||
            die "round $round: write load died before the kill phase (${phase_ms}ms)"
        "$MWFLEET" kill 0 --sig 9
        kill -9 "$ack_pid" 2>/dev/null || true
        wait "$ack_pid" 2>/dev/null || true
        t_kill="$(date +%s)"
        pkill -9 -P "$dd_pid" 2>/dev/null || true
        kill -9 "$dd_pid" 2>/dev/null || true
        wait "$dd_pid" 2>/dev/null || true
        # Sweep the dead FUSE mount, then the successor takes the D0 ladder
        # AND the WERO takeover (same host identity: the register ladder
        # replaces the dead incarnation's registration; the acquire lands
        # on the standing rtype-3 reservation).
        umount -l "$w_mnt" 2>/dev/null || true
        wait_for_unmounted "$w_mnt"
        "$MWFLEET" mount 0 ||
            die "round $round: successor remount FAILED (the WERO takeover or the D0 ladder refused)"
        t_up="$(date +%s)"
        # The oracle's verdict: every acked name resolves with its content.
        # A name whose fsync returned is in the ledger; a name the kill
        # caught mid-fsync is not (its presence either way is legal).
        local acked lost
        acked="$(wc -l <"$ack_ledger" | tr -d ' ')"
        lost=0
        while IFS= read -r f; do
            [ -n "$f" ] || continue
            if [ ! -f "$f" ]; then
                lost=$((lost + 1))
                echo "LOST (absent): $f" >>"$rowdir/acked-r$round.lost"
                continue
            fi
            local want got
            want="r$round:$((10#${f##*/f}))"
            got="$(cat "$f" 2>/dev/null || true)"
            if [ "$got" != "$want" ]; then
                lost=$((lost + 1))
                echo "LOST (content '$got' != '$want'): $f" >>"$rowdir/acked-r$round.lost"
            fi
        done <"$ack_ledger"
        [ "$lost" = "0" ] ||
            die "round $round: ACKED-WRITES ORACLE RED — $lost of $acked fsynced file(s) lost across the kill (see $rowdir/acked-r$round.lost)"
        log "round $round: acked-writes oracle GREEN ($acked fsynced file(s) all present with content)"
        rm -rf "$ack_dir" 2>/dev/null || true
        if [ "$sym" = "sym" ]; then
            # "Deleted stays deleted" (PR 13 — the oracle's other half, the
            # 12b C10 finding's class): the round's directory the successor
            # just removed is GONE through every joined writer and the
            # reader — never a name a stale projection or token still
            # serves. Bounded stats; a parked lookup is a red, not a hang.
            # ONE classifier for every arm (`sym_stat_deleted`, Issue 8): a
            # joiner or the reader answering EIO for the removed round
            # directory is a daemon that cannot answer, never "gone".
            local sidx verdict stale=0
            for sidx in $(joiner_idxs) $reader_idx; do
                verdict="$(sym_stat_deleted "$(mnt_of "$sidx")/acked-r$round" 60)"
                case "$verdict" in
                deleted) ;;
                hung) die "round $round: stat of the removed acked-r$round through m$sidx HUNG past 60 s (a parked lookup)" ;;
                resurrected)
                    stale=$((stale + 1))
                    echo "STALE: m$sidx still resolves acked-r$round" >>"$rowdir/stale-r$round.txt"
                    ;;
                error:*) die "round $round: stat of the removed acked-r$round through m$sidx failed with something other than ENOENT: ${verdict#error:}" ;;
                esac
            done
            [ "$stale" = "0" ] || die "round $round: the removed acked-r$round still resolves through $stale mount(s) (see $rowdir/stale-r$round.txt)"
        fi
        if [ "$sym" = "sym" ]; then
            # The symmetric fleet's device fence is the D0 guard's PR on the
            # METADATA namespaces (`flock+pr` — the death path's preempt
            # target); `data_plane_fence_mode` there is the job wire's
            # WERO, re-acquired over the dead incarnation's registration on
            # its own retry cadence (~40 s measured) and not this plane's
            # guarantee, so the sym leg does not gate on it.
            [ "$(stat_all_eq 0 writer_guard_mode flock+pr)" = "1" ] ||
                die "round $round: successor writer_guard_mode != flock+pr on every volume (the metadata PR the preempt fences is not held)"
            fm="$(stat_field 0 data_plane_fence_mode)"
        else
            fm="$(stat_field 0 data_plane_fence_mode)"
            [ "$fm" = "1" ] || die "round $round: successor data_plane_fence_mode=$fm (want 1)"
        fi
        # PR 12b review round 2, Issue 26: the `-o ro` READER follows the
        # failover WITHOUT fencing (its prior lease re-asserts until the
        # successor's grace deadline — the re-assertion half survives the
        # writers' early close), and its fleet WORKER re-enrolls at the
        # successor's coordinator so the round's fsck runs a MEMBER-SIDE
        # census shard (Issue 24's law on the real fleet).
        [ "$sym" = "sym" ] && [ -n "$reader_idx" ] && sym_crash_reader_follows "$round" "$reader_idx"
        # Every daemon's .stats BEFORE the fsck verdict (review round 1,
        # Issue 9), then the oracle: FULL online fsck (C1-C10, C8 ungated on
        # this stamped format — the durable ledger runs for real), BOUNDED
        # (Issue 13) with its transcript kept whatever the verdict.
        local sidx
        for sidx in 0 $(joiner_idxs) $reader_idx; do
            cat "$(mnt_of "$sidx")/.stats" >"$rowdir/stats-m$sidx-r$round.json" 2>/dev/null || true
        done
        local frc=0
        out="$(timeout 900 "$SQZ" fsck "$w_mnt" 2>&1)" || frc=$?
        echo "$out" >"$rowdir/fsck-r$round.out"
        [ "$frc" != "124" ] || die "round $round: online fsck HUNG past 900 s (the fleet census never terminated) — transcript $rowdir/fsck-r$round.out"
        [ "$frc" = "0" ] ||
            die "round $round: online fsck FAILED or found:
$out"
        echo "$out" | grep -q "findings: 0" ||
            die "round $round: fsck findings != 0:
$out"
        findings=0
        drift="$(stat_field 0 meta_kv_block_refs_drift)"
        [ "$drift" = "0" ] || die "round $round: meta_kv_block_refs_drift=$drift (C8 oracle RED)"
        fence_ref="$(stat_field 0 data_dma_fence_refusals)"
        [ "$fence_ref" = "0" ] || die "round $round: successor data_dma_fence_refusals=$fence_ref (a fresh mount fenced itself)"
        trip="$(stat_field 0 invariant_tripwires)"
        [ "$trip" = "0" ] || die "round $round: invariant_tripwires=$trip on the successor"
        backstops="$(stat_field 0 mem_budget_hard_backstops)"
        [ "$backstops" = "0" ] || die "round $round: mem_budget_hard_backstops=$backstops (R5 column)"
        [ "$sym" = "sym" ] && sym_crash_round_asserts "$round" "$rowdir"
        [ "$sym" = "sym" ] && [ -n "$reader_idx" ] && sym_crash_reader_shard_asserts "$round" "$reader_idx"
        printf '%-6s %-9s %-9s %-10s %-6s %-10s %-6s %s\n' "$round" "$phase_ms" "$((t_up - t_kill))" "findings:$findings" "$drift" "$fence_ref" "$trip" GREEN | tee -a "$rowdir/matrix.tsv"
    done

    # Matrix-end fleet health: the reader must have re-joined the LAST
    # successor and its rung-6 tripwire must be flat.
    if [ -n "$reader_idx" ]; then
        local ttl ttl_s renew_est
        ttl="$(stat_field 0 membership_lease_ttl_ms)"
        ttl_s=$((ttl / 1000))
        renew_est="$(owner_renew_est_s)"
        wait_stat_eq "$reader_idx" membership_mode member $((ttl_s + 6 * renew_est + 90)) "reader re-join after the matrix"
        [ "$(stat_field "$reader_idx" meta_kv_revalidate_dirty_skips)" = "0" ] ||
            die "reader dirty_skips != 0 after the matrix (the rung-6 finding-#1 tripwire)"
        [ "$(stat_field "$reader_idx" invariant_tripwires)" = "0" ] ||
            die "reader invariant_tripwires != 0 after the matrix"
        log "reader m$reader_idx healthy after the matrix (member, dirty_skips=0, tripwires=0)"
    fi
    log "$leg GREEN: $S7_ROUNDS/$S7_ROUNDS rounds (table + fsck reports in $rowdir)"
}

# PR 10's per-round assertions on the symmetric successor: the dead
# incarnation's Live page was OWN RESIDUE (the D0 flock is the proof), the
# manager role passed with it, and every symmetric must-stay-0 gauge is 0.
sym_crash_round_asserts() { # round rowdir
    local round="$1" rowdir="$2" v k
    cat "$(mnt_of 0)/.stats" >"$rowdir/stats-r$round.json" 2>/dev/null || true
    v="$(stat_sum 0 appender_self_recoveries)"
    [ "$v" -ge 1 ] 2>/dev/null ||
        die "round $round: appender_self_recoveries=$v — the successor did not recover its dead incarnation's page as own residue"
    v="$(stat_sum 0 appender_live_pages_at_mount)"
    [ "$v" -ge 1 ] 2>/dev/null ||
        die "round $round: appender_live_pages_at_mount=$v — the kill left no Live page (the leave ran?)"
    [ "$(stat_all_eq 0 manager_lease held)" = "1" ] ||
        die "round $round: manager_lease != held on every volume of the successor"
    [ "$(stat_all_eq 0 symmetric_meta 1)" = "1" ] ||
        die "round $round: symmetric_meta != 1 on every volume of the successor"
    for k in meta_kv_forest_key_violations appender_fence_breach foreign_frame_overwrite_detected \
        manager_verb_refusals meta_kv_replay_key_violations meta_kv_replay_lease_violations \
        meta_kv_replay_extent_violations fsck_slot_custody_conflicts fsck_unrecovered_appenders \
        appender_park_expiries meta_kv_leaf_lease_refusals dlm_token_recall_timeouts_live \
        appender_flush_ceiling_overruns dead_member_write_deferrals data_alloc_bitmap_drift; do
        v="$(stat_sum 0 "$k")"
        sym_zero_judge "round $round (successor)" 0 "$k" "$v"
    done
    # The ledger's terms: nothing foreign died (the joiners, if any, are
    # alive), so the driver recovered nothing and acted on nothing.
    v="$(stat_sum 0 appender_recoveries)"
    [ "$v" = "0" ] || die "round $round: appender_recoveries=$v (a foreign recovery ran on a fleet whose every other appender is alive?)"
    log "round $round: symmetric successor OK (self_recoveries=$(stat_sum 0 appender_self_recoveries), manager held, tripwires 0)"
    # PR 12b: every JOINED WRITER survived the manager's death — its next
    # wire act re-dials the SUCCESSOR's listener (published at its rung 7)
    # and lands: a create in a fresh directory (a first-touch acquire over
    # the re-dialed wire), `joined_wire_redials` ≥ 1, its must-stay-0 set
    # flat, and the successor reads the name it made.
    local j jm f ttl_ms parked t0
    ttl_ms="$(stat_field 0 membership_lease_ttl_ms)"
    for j in $(joiner_idxs); do
        jm="$(mnt_of "$j")"
        # /proc/mounts, never `mountpoint -q`: a joiner PARKED at T_self
        # (PR 8's law while the successor's grace window is not yet
        # reached) answers its root stat EAGAIN and is very much mounted.
        grep -q " $jm " /proc/mounts || die "round $round: joined writer m$j is no longer mounted after the manager's death"
        # The park releases when the reclaim lands at the successor (its
        # grace window admits it) — within a renewal beat of the
        # successor's arm; bounded by the lease TTL + the failover bound.
        t0="$(date +%s)"
        while :; do
            parked="$(stat_sum "$j" appender_parked)"
            [ "${parked:-0}" = "0" ] && break
            [ $(($(date +%s) - t0)) -lt $((ttl_ms / 1000 + 60)) ] ||
                die "round $round: joined writer m$j is still PARKED $(($(date +%s) - t0)) s after the successor armed (appender_parked=$parked; its reclaim never landed — membership_reclaim_refusals=$(stat_sum "$j" membership_reclaim_refusals))"
            sleep 1
        done
        # Gate 4 (d'') on a home shard of ≥ 8 members: the joiner PARKED at
        # T_self and RECLAIMED in the successor's grace — never expired
        # into the poison, never had a region recovered for it (the
        # manager's `appender_recoveries == 0` above is that half).
        v="$(stat_sum "$j" appender_park_expiries)"
        [ "$v" = "0" ] || die "round $round: joined writer m$j appender_park_expiries=$v (the park expired into the poison instead of reclaiming)"
        [ "$(stat_sum "$j" membership_self_fences)" = "0" ] ||
            die "round $round: joined writer m$j membership_self_fences != 0 across the manager's death"
        f="$jm/after-failover-r$round-m$j"
        local verbs0
        verbs0="$(stat_sum "$j" joined_wire_verbs)"
        mkdir -p "$f" || die "round $round: joined writer m$j could not create after the manager failover"
        echo "r$round" >"$f/mark" || die "round $round: joined writer m$j could not write after the manager failover"
        # The re-dial is judged on the joiner's FIRST manager verb since
        # the failover — and a `mkdir` under `/` with a file in it needs
        # none (the dentry ships as a cross-owner step on the S8 lane, the
        # mints land in slots the joiner already leases: PR 14's fleet on
        # the flip binary read `manager_verbs 0` at the successor with the
        # mkdir landed). So the law is conditional on a verb: a joiner that
        # asked the successor anything since the kill (`joined_wire_verbs`
        # advanced — the extent refill, a first-touch acquire, a slot
        # release) reached it through a re-dialed wire (`joined_wire_
        # redials ≥ 1`), with no wire failure; one that asked nothing has
        # nothing to re-dial and says so. (A create BURST forces the verb
        # — 512 inline creates spread over the rotor eat the `8 + M` grant
        # — but its leaf compactions MOVE the joiner's slot roots, and the
        # successor's block-plane census reads those slots through its
        # PROJECTIONS, stale at the grant-time root: 10 false C2 "leaked"
        # findings on live 5 MiB files — the acceptance record's §4.4ba,
        # PR 14b's; the burst stays out of this leg until the census is
        # scoped.)
        v="$(stat_sum "$j" joined_wire_verbs)"
        if [ "${v:-0}" -gt "${verbs0:-0}" ] 2>/dev/null; then
            v="$(stat_sum "$j" joined_wire_redials)"
            [ "$v" -ge 1 ] 2>/dev/null ||
                die "round $round: joined writer m$j joined_wire_redials=$v — its wire never re-dialed the successor (joined_wire_verbs $verbs0 → $(stat_sum "$j" joined_wire_verbs))"
            log "round $round: joined writer m$j's first manager verb since the kill landed through a re-dialed wire (joined_wire_redials=$v)"
        else
            log "round $round: joined writer m$j asked the successor nothing since the kill (joined_wire_verbs $verbs0) — nothing to re-dial; the data plane below proves the venue"
        fi
        [ "$(stat_sum "$j" joined_wire_failures)" = "0" ] ||
            die "round $round: joined writer m$j joined_wire_failures=$(stat_sum "$j" joined_wire_failures) across the manager's death"
        [ "$(cat "$(mnt_of 0)/after-failover-r$round-m$j/mark" 2>/dev/null)" = "r$round" ] ||
            die "round $round: the successor does not read joined writer m$j's post-failover name"
        # PR 12b review round 1, Issue 2 — the DATA plane follows too: a
        # STRIPED write (≥ 1 block, never the inline `mark`) needs a block
        # GRANT from the allocation holder — the SUCCESSOR now — and its
        # displaced free must reach the successor's ladder. Before the
        # fix the joiner's grant sink and free target kept the dead
        # manager's endpoint for the mount's life (the 7-byte inline mark
        # hid it). `dd conv=fsync`: the acked-writes oracle's own shape.
        local grants0 striped0 shipped0 served0 sum topups0 topups remaining
        grants0="$(stat_sum 0 block_grants)"
        topups0="$(stat_sum "$j" block_grant_topups)"
        remaining="$(stat_sum "$j" block_grant_window_remaining)"
        striped0="$(stat_sum "$j" layout_striped_writes)"
        shipped0="$(stat_sum "$j" meta_ship_publish.free_shipped_blocks)"
        served0="$(stat_sum 0 meta_ship_publish.free_served_blocks)"
        dd if=/dev/urandom of="$f/striped.bin" bs=1M count=5 conv=fsync status=none ||
            die "round $round: joined writer m$j could not write a STRIPED file after the manager failover (its block grants never followed the successor?)"
        sum="$(sha256sum <"$f/striped.bin" | cut -d' ' -f1)"
        # A whole-file rewrite displaces every block: the frees SHIP to
        # the holder at the fsync (KD-1.6: fsync closes the rewrite epoch).
        dd if=/dev/urandom of="$f/striped.bin" bs=1M count=5 conv=fsync,notrunc status=none ||
            die "round $round: joined writer m$j could not rewrite after the manager failover"
        v="$(stat_sum "$j" layout_striped_writes)"
        [ "$v" -gt "$striped0" ] 2>/dev/null ||
            die "round $round: joined writer m$j layout_striped_writes=$v (was $striped0) — the post-failover write never went striped"
        # The grant half of "the data plane follows": a write the joiner's
        # WINDOW still covers asks nothing — the grant the DEAD manager
        # carved stays the joiner's (the successor's re-hold leaves a live
        # page's remainder alone, `data_alloc_bitmap_leaks_deferred`), so
        # the successor's `block_grants` moves only when the joiner asked
        # a top-up. The arm is judged on the joiner's ASK count (round 3:
        # round 2's window of 60+ blocks covered the 2-block write and the
        # successor's flat `block_grants` was read as a lost grant); the
        # free half below is asked of every round regardless.
        topups="$(stat_sum "$j" block_grant_topups)"
        v="$(stat_sum 0 block_grants)"
        if [ "${topups:-0}" -gt "${topups0:-0}" ] 2>/dev/null; then
            [ "$v" -gt "$grants0" ] 2>/dev/null ||
                die "round $round: joined writer m$j asked $((topups - topups0)) block grant(s) after the failover and the successor's block_grants=$v (was $grants0) — the ask never landed at the SUCCESSOR"
            log "round $round: joined writer m$j's post-failover block grant landed at the successor (block_grants $grants0 → $v)"
        else
            log "round $round: joined writer m$j's window ($remaining blocks, granted before the failover) covered its post-failover write — no ask; the free half proves the venue"
        fi
        [ "$(sha256sum <"$(mnt_of 0)/after-failover-r$round-m$j/striped.bin" | cut -d' ' -f1)" != "$sum" ] ||
            die "round $round: the successor reads joined writer m$j's PRE-rewrite bytes (the rewrite was not published?)"
        [ "$(stat -c %s "$(mnt_of 0)/after-failover-r$round-m$j/striped.bin")" = "$((5 * 1024 * 1024))" ] ||
            die "round $round: the successor reads joined writer m$j's striped file at the wrong size"
        t0="$(date +%s)"
        while :; do
            v="$(stat_sum "$j" meta_ship_publish.free_shipped_blocks)"
            [ "${v:-0}" -gt "${shipped0:-0}" ] 2>/dev/null && break
            [ $(($(date +%s) - t0)) -lt 60 ] ||
                die "round $round: joined writer m$j free_shipped_blocks=$v (was $shipped0) 60 s after its rewrite — the displaced free never shipped to the successor"
            sleep 1
        done
        t0="$(date +%s)"
        while :; do
            v="$(stat_sum 0 meta_ship_publish.free_served_blocks)"
            [ "${v:-0}" -gt "${served0:-0}" ] 2>/dev/null && break
            [ $(($(date +%s) - t0)) -lt 60 ] ||
                die "round $round: the successor's free_served_blocks=$v (was $served0) — joined writer m$j's displaced free never reached the SUCCESSOR's ladder"
            sleep 1
        done
        v="$(stat_sum "$j" meta_ship_publish.free_ship_failures)"
        [ "${v:-0}" = "0" ] || die "round $round: joined writer m$j free_ship_failures=$v"
        sym_storm_daemon_asserts "$round" "$j"
        log "round $round: joined writer m$j followed the failover (redials=$(stat_sum "$j" joined_wire_redials), a striped write from its window or a grant at the successor, its free served there)"
    done
    # PR 12b: the successor's §6.8 item-3 bound advances with JOINED
    # writers as members — every member acknowledges the label its grant
    # carried (a joiner at once: its bindings are token-governed; the S5
    # reader through its ladder). A member that never acks holds the min
    # at 0 and the manager's every deferred free in the grace ring until
    # ENOSPC (round 5 of the first run: `free_grace_releases 0`,
    # `data_alloc_bitmap_population` at the volume). Bounded by two beats.
    if [ -n "$(joiner_idxs)" ]; then
        local beat_ms
        beat_ms="$(stat_field "$(joiner_idxs | head -1)" membership_renew_cadence_ms)"
        [ -n "$beat_ms" ] && [ "$beat_ms" != "0" ] || beat_ms=10000
        t0="$(date +%s)"
        while :; do
            v="$(stat_sum 0 membership_min_acked_free_epoch)"
            [ "${v:-0}" -gt 0 ] 2>/dev/null && break
            [ $(($(date +%s) - t0)) -lt $((3 * beat_ms / 1000 + 15)) ] ||
                die "round $round: the successor's membership_min_acked_free_epoch is still 0 $(($(date +%s) - t0)) s after its arm — a member never acknowledged a freed-offset label (ack lag: $(stat_field 0 free_grace_member_ack_lag_ms))"
            sleep 1
        done
        log "round $round: the freed-offset epoch fan-in advances with $(joiner_idxs | wc -l) joined writers as members (min_acked_free_epoch=$v)"
    fi
    # PR 12b review round 2, Issue 25 — the successor's re-hold DEFERRED the
    # dead incarnation's bitmap leaks while peer pages were Live; every
    # live joiner DECLARES its block-grant windows on its renewal, the
    # successor ADOPTS the declared ranges into its ledger and RELEASES
    # the rest once every live peer has declared (the ledger poll's
    # cadence). Closure `deferred ≡ released + adopted + pending`, and
    # pending → 0 within a renewal beat + the lease TTL + one poll — the
    # bound on a fleet whose peers all live. Round 4's acceptance tape read
    # 53 → 340 SET-and-unreferenced blocks per volume across two failovers.
    local deferred released adopted pending beat_ms2
    beat_ms2="$(stat_field 0 membership_renew_cadence_ms)"
    if [ -n "$(joiner_idxs)" ]; then
        beat_ms2="$(stat_field "$(joiner_idxs | head -1)" membership_renew_cadence_ms)"
    fi
    [ -n "$beat_ms2" ] && [ "$beat_ms2" != "0" ] || beat_ms2=10000
    t0="$(date +%s)"
    while :; do
        pending="$(stat_field 0 data_alloc_bitmap_leaks_pending)"
        [ "${pending:-0}" = "0" ] && break
        [ $(($(date +%s) - t0)) -lt $((beat_ms2 / 1000 + ttl_ms / 1000 + 15)) ] ||
            die "round $round: data_alloc_bitmap_leaks_pending=$pending on the successor $(($(date +%s) - t0)) s after its arm — the deferred leak release never converged (deferred=$(stat_field 0 data_alloc_bitmap_leaks_deferred) released=$(stat_field 0 data_alloc_bitmap_leaks_released) adopted=$(stat_field 0 data_alloc_bitmap_leaks_adopted); a live peer never declared its windows?)"
        sleep 1
    done
    deferred="$(stat_field 0 data_alloc_bitmap_leaks_deferred)"
    released="$(stat_field 0 data_alloc_bitmap_leaks_released)"
    adopted="$(stat_field 0 data_alloc_bitmap_leaks_adopted)"
    [ "$((released + adopted))" = "$deferred" ] ||
        die "round $round: deferred-leak closure broken on the successor: deferred=$deferred != released=$released + adopted=$adopted (pending 0)"
    log "round $round: the dead incarnation's deferred bitmap leaks converged on the successor (deferred=$deferred = released $released + adopted $adopted, pending 0 after $(($(date +%s) - t0)) s)"
}

# PR 12b review round 2, Issue 26 — the READER across a manager failover,
# asserted BEFORE the round's fsck: (1) it re-joins the successor as a
# `member` WITHOUT a self-fence — a `-o ro` reader is a RAM-only member in
# no claim set, so the successor's window never awaits it; its prior
# lease re-asserts until the window's DEADLINE (the re-assertion half
# survives the writers' early close), and `membership_self_fences` is
# must-stay-0 on every member kind; (2) its fleet WORKER (KD-MW-16 — the
# only member-side census venue on this fleet: joiners arm no worker)
# re-enrolls at the SUCCESSOR's coordinator, so the fsck that follows
# dispatches a member-side shard. The worker discovers the coordinator
# off the heartbeat-fresh `client:` records, and the dead incarnations'
# stay fresh for CLIENT_STALE_TTL (45 s) — the bound below is that TTL
# plus the worker's retry grain and the wire's dial deadline.
SYMC_SHARDS0=0
SYMC_RSHARDS0=0
SYMC_RSCOPED0=0
sym_crash_reader_follows() { # round reader_idx
    local round="$1" r="$2" ttl ttl_s renew_est v t0
    ttl="$(stat_field 0 membership_lease_ttl_ms)"
    ttl_s=$((ttl / 1000))
    renew_est="$(owner_renew_est_s)"
    # The successor's roster listing a READER is the witness that the
    # reader's re-assertion LANDED (a member-side word would read `member`
    # through the whole re-assert loop); the fence counter is cumulative,
    # so a purge-then-fresh-join before the listing is caught too.
    wait_stat_ge 0 membership_readers 1 $((ttl_s + 6 * renew_est + 90)) "round $round: the reader's re-assertion at the successor" >/dev/null
    wait_stat_eq "$r" membership_mode member 30 "round $round: reader membership_mode"
    v="$(stat_field "$r" membership_self_fences)"
    [ "$v" = "0" ] ||
        die "round $round: the reader m$r SELF-FENCED across the manager failover (membership_self_fences=$v) — its prior lease was refused at the successor (the re-assertion half closed with the writers' early close?)"
    [ "$(stat_field "$r" membership_self_fenced)" = "False" ] || # python's rendering of the JSON bool
        die "round $round: reader m$r membership_self_fenced=$(stat_field "$r" membership_self_fenced)"
    [ "$(stat_field "$r" invariant_tripwires)" = "0" ] ||
        die "round $round: reader m$r invariant_tripwires != 0 after the failover"
    log "round $round: reader m$r followed the failover as a member without fencing (self_fences=0, reclaim_refusals=$(stat_field "$r" membership_reclaim_refusals))"
    # The member-side census venue: the reader's worker enrolled at the
    # successor. Snapshot the shard ledgers the post-fsck assert reads.
    t0="$(date +%s)"
    while :; do
        v="$(stat_field 0 job_remote_workers)"
        [ "${v:-0}" -ge 1 ] 2>/dev/null && break
        [ $(($(date +%s) - t0)) -lt $((45 + 10 + 10 + 15)) ] ||
            die "round $round: no fleet worker enrolled at the successor's coordinator $(($(date +%s) - t0)) s after its arm (job_remote_workers=$v) — the reader's worker never re-discovered the coordinator (its log: 'fleet worker: enrollment at')"
        sleep 1
    done
    log "round $round: a member worker is enrolled at the successor (job_remote_workers=$v, waited $(($(date +%s) - t0)) s) — the fsck dispatches a member-side shard"
    SYMC_SHARDS0="$(stat_field 0 job_fleet_shards_completed)"
    SYMC_RSHARDS0="$(stat_field "$r" job_fleet_worker_shards)"
    SYMC_RSCOPED0="$(stat_field "$r" fsck_c1_projection_slots_scoped)"
}

# After the round's fsck: a MEMBER-SIDE shard ran on the reader and walked
# only what it may judge — a reader leases NO slot, so every slot tree is
# a projection to it and `fsck_c1_projection_slots_scoped` grows by the
# forest's slot-tree count (Issue 24: coverage incomplete, never a
# finding; the fsck's `findings: 0` above is the coordinator's admitted
# total, this member's included) — and the reader reads a joined writer's
# POST-failover name through its per-holder token plane.
sym_crash_reader_shard_asserts() { # round reader_idx
    local round="$1" r="$2" v j t0
    v="$(stat_field 0 job_fleet_shards_completed)"
    [ "${v:-0}" -gt "${SYMC_SHARDS0:-0}" ] 2>/dev/null ||
        die "round $round: job_fleet_shards_completed=$v (was $SYMC_SHARDS0) — the fsck completed no member-side shard although a worker was enrolled"
    v="$(stat_field "$r" job_fleet_worker_shards)"
    [ "${v:-0}" -gt "${SYMC_RSHARDS0:-0}" ] 2>/dev/null ||
        die "round $round: reader m$r job_fleet_worker_shards=$v (was $SYMC_RSHARDS0) — its worker served no census shard"
    v="$(stat_field "$r" fsck_c1_projection_slots_scoped)"
    [ "${v:-0}" -gt "${SYMC_RSCOPED0:-0}" ] 2>/dev/null ||
        die "round $round: reader m$r fsck_c1_projection_slots_scoped=$v (was $SYMC_RSCOPED0) — its census shard walked the projected slot trees as its own (Issue 24's class)"
    log "round $round: member-side census shard on reader m$r (worker shards $SYMC_RSHARDS0 → $v; C1 scoped $SYMC_RSCOPED0 → $(stat_field "$r" fsck_c1_projection_slots_scoped) projected trees, findings admitted 0)"
    for j in $(joiner_idxs); do
        t0="$(date +%s)"
        while :; do
            [ "$(cat "$(mnt_of "$r")/after-failover-r$round-m$j/mark" 2>/dev/null)" = "r$round" ] && break
            [ $(($(date +%s) - t0)) -lt 60 ] ||
                die "round $round: the reader m$r does not read joined writer m$j's post-failover name 60 s after it landed"
            sleep 1
        done
    done
    log "round $round: reader m$r reads every joined writer's post-failover name"
}

# --- gate 7: sym-walls (the relocated walls) ----------------------------------
leg_sym_walls() {
    require_symmetric
    local joiners n
    mapfile -t joiners < <(joiner_idxs)
    n="${#joiners[@]}"
    [ "$n" -ge 1 ] ||
        die "sym-walls needs ≥ 1 joined writer — create the fleet with: sudo tests/mw_fleet.sh create N=2 --symmetric --writers=7"
    [[ "$SYM_WALLS_FILES" =~ ^[0-9]+$ ]] && [ "$SYM_WALLS_FILES" -ge 1 ] ||
        die "--walls-files takes a positive integer (got '$SYM_WALLS_FILES')"
    [[ "$SYM_WALLS_MB" =~ ^[0-9]+$ ]] && [ "$SYM_WALLS_MB" -ge 4 ] && [ $((SYM_WALLS_MB % 4)) = 0 ] ||
        die "--walls-mb takes a multiple of 4 MiB ≥ 4 (got '$SYM_WALLS_MB')"
    sym_quiet_or_die sym-walls
    sym_ensure_joiners $((n + 1)) "${joiners[@]}"
    local rowdir
    rowdir="$STATE/rows/symwalls-$(date +%s)"
    mkdir -p "$rowdir"
    local run blocks_per_file blocks_per_joiner
    run="$(date +%s)"
    blocks_per_file=$((SYM_WALLS_MB / 4))
    blocks_per_joiner=$((SYM_WALLS_FILES * blocks_per_file))
    log "sym-walls (gate 7): $n joined writer(s), each $SYM_WALLS_FILES × $SYM_WALLS_MB MiB pre-written then REWRITTEN in place at once — $blocks_per_joiner displaced blocks per joiner ship to the allocation holder (the manager)"

    # --- Row (a): the pre-write (untimed; the set that is rewritten). ---
    local j f pids=() p rc=0
    for j in "${joiners[@]}"; do
        mkdir -p "$(mnt_of "$j")/walls-$run-w$j"
        for f in $(seq 1 "$SYM_WALLS_FILES"); do
            dd if=/dev/zero of="$(mnt_of "$j")/walls-$run-w$j/f$f" bs=4M count="$blocks_per_file" \
                conv=fsync status=none 2>>"$rowdir/prewrite-w$j.err" &
            pids+=($!)
        done
    done
    for p in "${pids[@]}"; do wait "$p" || rc=1; done
    [ "$rc" = "0" ] || die "sym-walls: a pre-write FAILED (see $rowdir/prewrite-w*.err)"
    sleep 3 # the publishes and the holder's ledger settle before the snapshot
    local idx
    for idx in 0 "${joiners[@]}"; do snap "$idx" "wa0" "$rowdir"; done
    local cpu0 t0 t1
    cpu0="$(sym_cpu_ticks 0)"
    # The REWRITE: every joiner overwrites every file in place, all at once.
    # The data namespaces' /proc/diskstats bracket it (row (a)'s
    # amplification columns — the box re-run stated them owed).
    sym_disk_snap "$rowdir" "wa0"
    pids=()
    t0="$(date +%s.%N)"
    for j in "${joiners[@]}"; do
        for f in $(seq 1 "$SYM_WALLS_FILES"); do
            dd if=/dev/urandom of="$(mnt_of "$j")/walls-$run-w$j/f$f" bs=4M count="$blocks_per_file" \
                conv=notrunc,fsync status=none 2>>"$rowdir/rewrite-w$j.err" &
            pids+=($!)
        done
    done
    for p in "${pids[@]}"; do wait "$p" || rc=1; done
    t1="$(date +%s.%N)"
    sym_disk_snap "$rowdir" "wa1"
    [ "$rc" = "0" ] || die "sym-walls: a rewrite FAILED (see $rowdir/rewrite-w*.err)"
    local cpu1 wall
    cpu1="$(sym_cpu_ticks 0)"
    wall="$(python3 -c "print(f'{$t1-$t0:.2f}')")"
    # The shipped frees land after the rewrites' publishes (the joiner's
    # free path is asynchronous to the write's ack): wait for the holder's
    # served count to reach the displaced population, bounded.
    local displaced served shipped t2
    displaced=$((n * blocks_per_joiner))
    t2="$(date +%s)"
    while :; do
        served="$(stat_field 0 meta_ship_publish.free_served_blocks)"
        [ "${served:-0}" -ge $(( $(sym_snapshot_value "$rowdir" 0 wa0 meta_ship_publish.free_served_blocks) + displaced )) ] 2>/dev/null && break
        [ $(($(date +%s) - t2)) -lt 120 ] || break
        sleep 1
    done
    sleep 2
    for idx in 0 "${joiners[@]}"; do snap "$idx" "wa1" "$rowdir"; done
    shipped=0
    local v zero_miss=""
    for j in "${joiners[@]}"; do
        v="$(sym_delta "$rowdir" "$j" wa meta_ship_publish.free_shipped_blocks)"
        shipped=$((shipped + v))
        v="$(sym_zero_violations_delta "$rowdir" "$j" wa)"
        [ -z "$v" ] || zero_miss="$zero_miss m$j:{$v}"
    done
    served="$(sym_delta "$rowdir" 0 wa meta_ship_publish.free_served_blocks)"
    v="$(sym_zero_violations_delta "$rowdir" 0 wa)"
    [ -z "$v" ] || zero_miss="$zero_miss m0:{$v}"
    local free_rate mgr_cpu mgr_load verbs verbs_per_s svc_total svc_exec
    free_rate="$(python3 -c "print(f'{$served/($t1-$t0):.0f}')")"
    mgr_cpu="$(python3 -c "
import os
hz = os.sysconf('SC_CLK_TCK')
print(f'{100*($cpu1-$cpu0)/hz/max(1e-9, $t1-$t0):.0f}')")"
    mgr_load="$(stat_field 0 manager_load_pct | tr -d '[] ' | cut -d, -f1)"
    verbs="$(sym_delta "$rowdir" 0 wa manager_verbs)"
    verbs_per_s="$(stat_field 0 manager_verbs_per_s | tr -d '[] ' | cut -d, -f1)"
    svc_total="$(sym_delta_arr_field "$rowdir" 0 wa manager_service_ns total)"
    svc_exec="$(sym_delta_arr_field "$rowdir" 0 wa manager_service_ns execute)"
    local verdict_a=MET minted failures
    minted="$(sym_delta "$rowdir" 0 wa block_grant_blocks)"
    failures=0
    for j in "${joiners[@]}"; do
        v="$(sym_delta "$rowdir" "$j" wa meta_ship_publish.free_ship_failures)"
        failures=$((failures + v))
    done
    # THE ENGAGEMENT LAW (§8 gate 7): every free the joiners SHIPPED was
    # SERVED at the holder (exact closure), every displaced OLD block is
    # among them (`served ≥ displaced` — the rewrite's ACK-early overlay
    # mints intermediate images per kernel-split segment and frees them
    # as it settles, so the free population is displaced + superseded;
    # `minted` beside it: a rewrite leaves the live set unchanged, so
    # minted ≈ served at quiesce), and none FAILED (a failed ship is a
    # durably-free offset unreturned until the next derivation).
    [ "$shipped" = "$served" ] || verdict_a="MISS(shipped=$shipped≠served=$served)"
    [ "$served" -ge "$displaced" ] || verdict_a="$verdict_a MISS(served=$served<displaced=$displaced)"
    [ "$failures" = "0" ] || verdict_a="$verdict_a MISS(free_ship_failures=+$failures)"
    [ -z "$zero_miss" ] || verdict_a="$verdict_a MISS(must-stay-0:$zero_miss)"
    {
        echo "== sym-walls row (a): the relocated FREE wall (w_rewrite, N=$n joiners × $SYM_WALLS_FILES × $SYM_WALLS_MB MiB)$SYM_BUSY_ROW =="
        printf '%-6s %-10s %-8s %-8s %-8s %-11s %-10s %-8s %-10s %-13s %-12s %s\n' N DISPLACED MINTED SHIPPED SERVED FREE_BLK_S REWRITE_S MGR_CPU MGR_VERBS SVC_TOTAL_NS SVC_EXEC_NS VERDICT
        printf '%-6s %-10s %-8s %-8s %-8s %-11s %-10s %-8s %-10s %-13s %-12s %s\n' "$n" "$displaced" "$minted" "$shipped" "$served" "$free_rate" "$wall" "${mgr_cpu}%" "$verbs" "$svc_total" "$svc_exec" "$verdict_a"
        echo "manager_load_pct=$mgr_load manager_verbs_per_s=$verbs_per_s free_ship_failures=+$failures free_refused_blocks=$(stat_sum 0 meta_ship_publish.free_refused_blocks)"
        # The AGENTS amplification columns: the rewrite's user bytes (N ×
        # files × MiB) against the data namespaces' device writes, beside
        # the daemons' own ledger (Σ rewrite_device_write_bytes ÷ Σ
        # rewrite_user_bytes — what the joiners SUBMITTED).
        local ledger_dev=0 ledger_user=0
        for j in "${joiners[@]}"; do
            v="$(sym_delta "$rowdir" "$j" wa rewrite_device_write_bytes)"
            ledger_dev=$((ledger_dev + v))
            v="$(sym_delta "$rowdir" "$j" wa rewrite_user_bytes)"
            ledger_user=$((ledger_user + v))
        done
        echo "amplification (/proc/diskstats, data namespaces; user $((displaced * 4)) MiB): $(sym_disk_amp "$rowdir" wa $((displaced * 4 * 1024 * 1024))) | ledger rewrite_device_write_bytes/rewrite_user_bytes=$(python3 -c "print(f'{$ledger_dev/max(1,$ledger_user):.3f}')") ($ledger_dev / $ledger_user)"
        sym_fb1_faces "$rowdir" wa 0 "${joiners[@]}"
    } | tee "$rowdir/symwalls-a.txt"
    for j in "${joiners[@]}"; do rm -rf "$(mnt_of "$j")/walls-$run-w$j" 2>/dev/null || true; done

    # --- Row (b): the JOIN STORM — every joiner leaves, all rejoin at once. ---
    sleep 2
    for j in "${joiners[@]}"; do
        "$MWFLEET" unmount "$j" || die "sym-walls: joiner m$j's clean unmount failed"
        wait_for_unmounted "$(mnt_of "$j")"
    done
    local t
    for t in $(seq 1 60); do
        : "$t"
        [ "$(stat_all_eq 0 appenders_known 1)" = "1" ] && break
        sleep 1
    done
    mkdir -p "$(mnt_of 0)/jobs"
    snap 0 "wb0" "$rowdir"
    cpu0="$(sym_cpu_ticks 0)"
    pids=()
    t0="$(date +%s.%N)"
    for j in "${joiners[@]}"; do
        "$MWFLEET" mount "$j" >"$rowdir/join-w$j.log" 2>&1 &
        pids+=($!)
    done
    rc=0
    for p in "${pids[@]}"; do wait "$p" || rc=1; done
    [ "$rc" = "0" ] || die "sym-walls: a joiner's remount in the storm FAILED (see $rowdir/join-w*.log)"
    for t in $(seq 1 120); do
        : "$t"
        [ "$(stat_all_eq 0 appenders_known $((n + 1)))" = "1" ] && break
        sleep 0.5
    done
    t1="$(date +%s.%N)"
    [ "$(stat_all_eq 0 appenders_known $((n + 1)))" = "1" ] ||
        die "sym-walls: the manager never read $((n + 1)) Live pages after the join storm (appenders_known=$(stat_field 0 appenders_known))"
    local join_wall
    join_wall="$(python3 -c "print(f'{$t1-$t0:.2f}')")"
    # Each joiner's `mkdir /jobs/<j>` right after its join — the ship to
    # /jobs's HOLDER, §5.10's one root-level ship per mount. The holder is
    # the manager until `/jobs` STRIPES: N creators into one directory is
    # the 3b shape, and at N = 31 the holder's flip trigger fires DURING
    # the storm (the box re-run read `dir_stripe_flips` +1 with 28 of 31
    # steps served at the manager — the other three landed at the stripe
    # holders, which are joiners). So the law is FLEET-WIDE: every mkdir
    # landed (asserted below), the steps the joiners SHIPPED were SERVED
    # somewhere (Σ served over every member ≡ Σ shipped), and the ones
    # that shipped nowhere were own-stripe local lands (≤ n, reported).
    local steps0
    steps0="$(stat_sum 0 xv_cross_owner_steps_served)"
    for j in "${joiners[@]}"; do snap "$j" "wb0" "$rowdir"; done
    for j in "${joiners[@]}"; do
        mkdir "$(mnt_of "$j")/jobs/w$j-$run" || die "sym-walls: joiner m$j's mkdir /jobs/… failed after the storm"
    done
    sleep 2
    cpu1="$(sym_cpu_ticks 0)"
    snap 0 "wb1" "$rowdir"
    for j in "${joiners[@]}"; do snap "$j" "wb1" "$rowdir"; done
    local steps verbs_b svc_b_total shipped_b=0 served_fleet flips_b
    steps=$(( $(stat_sum 0 xv_cross_owner_steps_served) - steps0 ))
    served_fleet="$steps"
    for j in "${joiners[@]}"; do
        v="$(sym_delta "$rowdir" "$j" wb xv_cross_owner_steps_shipped)"
        shipped_b=$((shipped_b + v))
        v="$(sym_delta "$rowdir" "$j" wb xv_cross_owner_steps_served)"
        served_fleet=$((served_fleet + v))
    done
    flips_b="$(sym_delta "$rowdir" 0 wb dir_stripe_flips)"
    verbs_b="$(sym_delta "$rowdir" 0 wb manager_verbs)"
    svc_b_total="$(sym_delta_arr_field "$rowdir" 0 wb manager_service_ns total)"
    mgr_cpu="$(python3 -c "
import os
hz = os.sysconf('SC_CLK_TCK')
print(f'{100*($cpu1-$cpu0)/hz/max(1e-9, $t1-$t0):.0f}')")"
    local verdict_b=MET
    [ "$served_fleet" = "$shipped_b" ] || verdict_b="MISS(shipped=$shipped_b≠served_fleet=$served_fleet)"
    [ "$shipped_b" -le "$n" ] || verdict_b="$verdict_b MISS(shipped=$shipped_b>$n)"
    v="$(sym_zero_violations_delta "$rowdir" 0 wb)"
    [ -z "$v" ] || verdict_b="$verdict_b MISS(must-stay-0:m0:{$v})"
    {
        echo "== sym-walls row (b): the JOIN STORM (N=$n joiners leave, then all rejoin at once)$SYM_BUSY_ROW =="
        printf '%-6s %-12s %-10s %-12s %-9s %-14s %-12s %-12s %s\n' N JOIN_WALL_S MGR_VERBS SVC_TOTAL_NS MGR_CPU JOBS_SHIPPED JOBS_AT_MGR JOBS_LOCAL VERDICT
        printf '%-6s %-12s %-10s %-12s %-9s %-14s %-12s %-12s %s\n' "$n" "$join_wall" "$verbs_b" "$svc_b_total" "${mgr_cpu}%" "$shipped_b" "$steps" "$((n - shipped_b))" "$verdict_b"
        echo "manager_failover_bound_ms=$(stat_field 0 manager_failover_bound_ms | tr -d '[] ' | cut -d, -f1) appenders_known=$(stat_field 0 appenders_known | tr -d '[] ' | cut -d, -f1) served_fleet=$served_fleet dir_stripe_flips_during_storm=$flips_b (a flip of /jobs re-homes the later mkdirs' ships to the stripe holders)"
        sym_fb1_faces "$rowdir" wb 0 "${joiners[@]}"
    } | tee "$rowdir/symwalls-b.txt"
    sym_oracle sym-walls "$rowdir"
    [ "$verdict_a" = "MET" ] && [ "$verdict_b" = "MET" ] ||
        die "sym-walls: gate 7 rows RED — (a) $verdict_a; (b) $verdict_b (tables in $rowdir)"
    log "sym-walls PUBLISHED (rows (a) + (b) + snapshots in $rowdir)"
}

# The delta of one FIELD summed over a per-volume ARRAY OF OBJECTS
# (`manager_service_ns` = `[{admit, execute, reply, total}, …]`) between
# a row's two snapshots — `sym_delta`'s flattening reaches no list of
# objects.
sym_delta_arr_field() { # rowdir idx label key field
    python3 - "$1" "$2" "$3" "$4" "$5" <<'PYEOF'
import json, sys
rowdir, idx, label, key, field = sys.argv[1:6]
def load(ph):
    root = json.load(open(f"{rowdir}/m{idx}_p{label}{ph}.json"))
    m = root.get("metrics", root)
    v = m.get(key, [])
    if isinstance(v, dict):
        v = [v]
    return sum(int(o.get(field, 0) or 0) for o in v if isinstance(o, dict))
print(load(1) - load(0))
PYEOF
}

# One flattened key's value out of a saved snapshot (the row's own p0,
# for a bounded wait against the delta the row expects).
sym_snapshot_value() { # rowdir idx label key
    python3 - "$1" "$2" "$3" "$4" <<'PYEOF'
import json, sys
rowdir, idx, label, key = sys.argv[1:5]
def flat(d, out=None, pfx=""):
    out = {} if out is None else out
    for k, v in d.items():
        if isinstance(v, dict): flat(v, out, pfx + k + ".")
        else: out[pfx + k] = v
    return out
def fold(v):
    if isinstance(v, list):
        return sum(x for x in v if isinstance(x, (int, float)))
    return v if isinstance(v, (int, float)) else 0
root = json.load(open(f"{rowdir}/m{idx}_p{label}.json"))
print(int(fold(flat(root.get("metrics", root)).get(key, 0))))
PYEOF
}

# --- KD-MW-16 (rung 10c) — fleet-parallel maintenance ------------------------

# One corpus on the writer: n files x file_mb MiB of urandom, fsync'd,
# then a 2 s settle past the writer's <=1 s checkpoint ceiling so a
# freshly-mounted reader's coherent view carries it.
s10c_write_corpus() { # writer_mnt total_mb file_mb
    local mnt="$1" total_mb="$2" file_mb="$3" n i
    n=$((total_mb / file_mb))
    [ "$n" -ge 1 ] || die "s10c: corpus too small ($total_mb MiB / $file_mb MiB files)"
    rm -rf "$mnt/fleetscale" 2>/dev/null || true
    mkdir -p "$mnt/fleetscale" || die "s10c: cannot mkdir the corpus dir"
    log "s10c: writing the corpus — $n x ${file_mb} MiB urandom files (conv=fsync)"
    for ((i = 0; i < n; i++)); do
        dd if=/dev/urandom of="$mnt/fleetscale/f$i.bin" bs=1M count="$file_mb" \
            conv=fsync status=none || die "s10c: corpus write f$i failed"
    done
    sync
    sleep 2
}

# Remount the coordinator + exactly readers 1..(want-1); wait for the
# coordinator to see (want-1) enrolled workers. Fresh daemons per call:
# every run is COLD (remounts clear the RAM tiers and the R1b ghosts;
# one scrub touch per block never passes second-touch admission).
s10c_set_width() { # want
    local want="$1" i deadline w
    for i in $(member_idxs); do
        [ "$i" = "0" ] && continue
        "$MWFLEET" unmount "$i" >/dev/null 2>&1 || true
    done
    "$MWFLEET" unmount 0 >/dev/null 2>&1 || true
    "$MWFLEET" mount 0 >/dev/null || die "s10c: writer remount failed"
    for ((i = 1; i < want; i++)); do
        "$MWFLEET" mount "$i" >/dev/null || die "s10c: reader $i remount failed"
    done
    deadline=$((SECONDS + 90))
    while :; do
        w="$(stat_field 0 job_remote_workers)"
        [ "${w:-0}" = "$((want - 1))" ] && break
        [ "$SECONDS" -lt "$deadline" ] ||
            die "s10c: only ${w:-0}/$((want - 1)) fleet workers enrolled within 90 s (is the fleet created with --membership?)"
        sleep 1
    done
    for ((i = 1; i < want; i++)); do
        [ "$(stat_field "$i" membership_mode)" = "member" ] ||
            die "s10c: reader $i is not a membership MEMBER — fleet workers arrive by membership (KD-MW-16)"
    done
}

# One timed fleet fsck at the CURRENT width; validates the engagement +
# coverage row and appends "width run t_ms inodes scrub_bytes" to
# $rowdir/rows.tsv. Prints nothing on stdout (logs ride stderr).
s10c_timed_fsck() { # width run rowdir extra_fsck_args...
    local width="$1" run="$2" rowdir="$3"
    shift 3
    local i t0 t1 out rc idxs=(0)
    for ((i = 1; i < width; i++)); do idxs+=("$i"); done
    for i in "${idxs[@]}"; do snap "$i" "0w${width}r${run}" "$rowdir"; done
    t0=$(date +%s%N)
    set +e
    out="$("$SQZ" fsck "$(mnt_of 0)" --scrub "$@" 2>&1)"
    rc=$?
    set -e
    t1=$(date +%s%N)
    echo "$out" >"$rowdir/fsck-w${width}r${run}.log"
    [ "$rc" = "0" ] || die "s10c: fsck (N=$width run $run) FAILED (rc=$rc): $(head -3 "$rowdir/fsck-w${width}r${run}.log")"
    echo "$out" | grep -q "findings: 0" || die "s10c: findings != 0 at N=$width run $run:
$out"
    for i in "${idxs[@]}"; do snap "$i" "1w${width}r${run}" "$rowdir"; done
    python3 - "$rowdir" "$width" "$run" "$(((t1 - t0) / 1000000))" 1>&2 <<'PYS10C' || die "s10c: INVALID ROW (N=$width run $run)"
import json, sys

rowdir, width, run, t_ms = sys.argv[1], int(sys.argv[2]), sys.argv[3], int(sys.argv[4])

def flat(d, out=None, pfx=""):
    out = {} if out is None else out
    for k, v in d.items():
        if isinstance(v, dict):
            flat(v, out, pfx + k + ".")
        else:
            out[pfx + k] = v
    return out

def load(i, ph):
    return flat(json.load(open(f"{rowdir}/m{i}_p{ph}w{width}r{run}.json")).get("metrics", {}))

bad = []
w0, w1 = load(0, 0), load(0, 1)
d = lambda k: w1.get(k, 0) - w0.get(k, 0)
if d("job_fleet_shards_dispatched") != width - 1:
    bad.append(f"writer dispatched {d('job_fleet_shards_dispatched')} != {width-1}")
if d("job_fleet_shards_completed") != width - 1:
    bad.append(f"writer completed {d('job_fleet_shards_completed')} != {width-1} (the engagement ledger must close)")
if d("job_fleet_shards_relocal") != 0:
    bad.append(f"relocal {d('job_fleet_shards_relocal')} != 0 (a lost shard invalidates a SCALING row)")
if d("fsck_findings") != 0:
    bad.append(f"fsck_findings moved by {d('fsck_findings')}")
inodes = d("fsck_inodes_scanned")
scrub = d("scrub_bytes_scanned")
if inodes <= 0:
    bad.append("writer fsck_inodes_scanned did not move (coverage not accounted)")
for i in range(1, width):
    r0, r1 = load(i, 0), load(i, 1)
    got = r1.get("job_fleet_worker_shards", 0) - r0.get("job_fleet_worker_shards", 0)
    if got != 1:
        bad.append(f"reader {i} executed {got} fleet shards != 1")
if bad:
    for b in bad:
        print(f"  INVALID: {b}", file=sys.stderr)
    sys.exit(1)
with open(f"{rowdir}/rows.tsv", "a") as f:
    f.write(f"{width}\t{run}\t{t_ms}\t{inodes}\t{scrub}\n")
print(f"  N={width} run {run}: {t_ms} ms, inodes {inodes}, scrub_bytes {scrub} (engagement exact)")
PYS10C
}

leg_s10c_fsck_scale() {
    local rowdir members
    rowdir="$STATE/rows/s10c-fsck-scale-$(date +%s)"
    mkdir -p "$rowdir"
    members="$(member_idxs | wc -l)"
    [ "$members" -ge 4 ] || die "s10c-fsck-scale needs >= 4 members (create N=4 --membership); have $members"
    local provisional=""
    if pgrep -x cargo >/dev/null 2>&1; then
        provisional="PROVISIONAL (foreign cargo work running)"
    else
        local load ncpu
        load="$(cut -d' ' -f1 /proc/loadavg)"
        ncpu="$(nproc)"
        awk -v l="$load" -v n="$ncpu" 'BEGIN { exit !(l > n / 2) }' &&
            provisional="PROVISIONAL (loadavg $load on $ncpu cpus)"
    fi
    [ -n "$provisional" ] && warn "quiet gate: $provisional — the table is labeled; the gate still enforces"

    # Corpus once, at N=1 width (writer only, no workers to race).
    s10c_set_width 1
    s10c_write_corpus "$(mnt_of 0)" "$S10C_MB" 32

    local width run
    for width in 1 2 4; do
        log "s10c-fsck-scale: width N=$width — $S10C_RUNS run(s), every member remounted per run (cold rows)"
        for ((run = 1; run <= S10C_RUNS; run++)); do
            s10c_set_width "$width"
            # --throttle 10: the KD-3 duty cycle is the STRETCH
            # instrument — it applies PER MEMBER (each shard runs at the
            # same duty), so linearity is preserved and the >=0.6x gate
            # is a RATIO; unthrottled, this box's zram scrubs the whole
            # corpus in ~1-2 s, inside the CLI's 500 ms poll quantum.
            s10c_timed_fsck "$width" "$run" "$rowdir" --throttle 10
        done
    done

    python3 - "$rowdir" "${provisional:-quiet}" <<'PYS10S' || die "s10c-fsck-scale: GATE FAILED"
import statistics, sys

rowdir, quiet = sys.argv[1], sys.argv[2]
rows = {}
inodes = set()
for line in open(f"{rowdir}/rows.tsv"):
    w, run, t, ino, scrub = line.split()
    rows.setdefault(int(w), []).append(int(t))
    inodes.add(int(ino))

print("== s10c-fsck-scale — fleet fsck wall-clock (ms; median of runs) ==")
print(f"   quiet gate: {quiet}")
print("   evidence tier: measured-simulated (one box; co-located members share the device + CPUs)")
med = {}
for w in sorted(rows):
    med[w] = statistics.median(rows[w])
    print(f"   N={w}: runs {rows[w]} -> median {med[w]} ms")
if len(inodes) != 1:
    print(f"COVERAGE VIOLATION: fsck_inodes_scanned differed across rows: {sorted(inodes)}", file=sys.stderr)
    sys.exit(1)
print(f"   coverage: fsck_inodes_scanned identical at every width ({inodes.pop()}) — exactly-once holds")
speedup = med[1] / med[4] if med.get(4) else 0.0
floor = 0.6 * 4
print(f"   N=4 speedup: {speedup:.2f}x (gate: >= {floor:.1f}x-of-4 = {floor / 4:.0%}-linear, i.e. t1/t4 >= {floor:.1f})")
if speedup < floor:
    print(f"GATE FAILED: t1/t4 = {speedup:.2f} < {floor:.1f} (>=0.6x-linear at N=4)", file=sys.stderr)
    sys.exit(1)
print("GATE MET: fleet fsck scales >= 0.6x-linear to N=4")
PYS10S
    log "s10c-fsck-scale GREEN (rows + snapshots + fsck logs in $rowdir)"
}

leg_s10c_kill_shard() {
    local rowdir members
    rowdir="$STATE/rows/s10c-kill-shard-$(date +%s)"
    mkdir -p "$rowdir"
    members="$(member_idxs | wc -l)"
    [ "$members" -ge 3 ] || die "s10c-kill-shard needs >= 3 members (create N=3 --membership); have $members"

    s10c_set_width 1
    s10c_write_corpus "$(mnt_of 0)" "$S10C_MB" 32

    # Baseline (no kill), throttled — learns the fleet's exact census
    # total AND the shard runtime the kill window rides.
    s10c_set_width 3
    log "s10c-kill-shard: baseline fleet fsck (throttle 5 — the KD-3 stretch the kill window rides)"
    s10c_timed_fsck 3 "base" "$rowdir" --throttle 5
    local t0_inodes
    t0_inodes="$(awk -F'\t' '$2=="base" {print $4}' "$rowdir/rows.tsv")"
    log "s10c-kill-shard: baseline census total = $t0_inodes inodes"

    # The kill run: fresh width, fsck in the background, kill -9 reader 1
    # the moment both worker shards are IN FLIGHT (dispatched == 2,
    # completed == 0).
    s10c_set_width 3
    local i
    for i in 0 1 2; do snap "$i" "0kill" "$rowdir"; done
    local d0 c0 e0 p0 q0 r0
    d0="$(stat_field 0 job_fleet_shards_dispatched)"
    c0="$(stat_field 0 job_fleet_shards_completed)"
    e0="$(stat_field 0 job_remote_lease_expiries)"
    p0="$(stat_field 0 job_remote_pr_preempts)"
    q0="$(stat_field 0 job_remote_quarantined_destinations)"
    r0="$(stat_field 0 job_fleet_shards_relocal)"
    local wmnt
    wmnt="$(mnt_of 0)"
    "$SQZ" fsck "$wmnt" --scrub --throttle 5 >"$rowdir/fsck-kill.log" 2>&1 &
    local fsck_pid=$!
    local deadline=$((SECONDS + 120)) dd cc
    while :; do
        dd="$(stat_field 0 job_fleet_shards_dispatched)"
        cc="$(stat_field 0 job_fleet_shards_completed)"
        if [ "$((dd - d0))" -ge 2 ]; then
            [ "$((cc - c0))" -eq 0 ] || die "s10c-kill-shard: kill window missed (a shard already completed) — grow --corpus-mb"
            break
        fi
        kill -0 "$fsck_pid" 2>/dev/null || die "s10c-kill-shard: fsck exited before shards dispatched: $(head -3 "$rowdir/fsck-kill.log")"
        [ "$SECONDS" -lt "$deadline" ] || die "s10c-kill-shard: shards never dispatched within 120 s"
        sleep 0.25
    done
    log "s10c-kill-shard: both worker shards in flight — kill -9 reader 1 MID-SHARD"
    "$MWFLEET" kill 1 >/dev/null || die "s10c-kill-shard: kill verb failed"
    local rc=0
    wait "$fsck_pid" || rc=$?
    [ "$rc" = "0" ] || die "s10c-kill-shard: fsck FAILED after the kill (rc=$rc): $(tail -5 "$rowdir/fsck-kill.log")"
    grep -q "findings: 0" "$rowdir/fsck-kill.log" || die "s10c-kill-shard: findings != 0 after the kill:
$(tail -10 "$rowdir/fsck-kill.log")"
    snap 0 "1kill" "$rowdir"
    snap 2 "1kill" "$rowdir"

    local d1 c1 e1 p1 q1 r1 inodes
    d1="$(stat_field 0 job_fleet_shards_dispatched)"
    c1="$(stat_field 0 job_fleet_shards_completed)"
    e1="$(stat_field 0 job_remote_lease_expiries)"
    p1="$(stat_field 0 job_remote_pr_preempts)"
    q1="$(stat_field 0 job_remote_quarantined_destinations)"
    r1="$(stat_field 0 job_fleet_shards_relocal)"
    inodes="$(python3 - "$rowdir" <<'PYK'
import json, sys
def flat(d, out=None, pfx=""):
    out = {} if out is None else out
    for k, v in d.items():
        if isinstance(v, dict): flat(v, out, pfx + k + ".")
        else: out[pfx + k] = v
    return out
rowdir = sys.argv[1]
p0 = flat(json.load(open(f"{rowdir}/m0_p0kill.json")).get("metrics", {}))
p1 = flat(json.load(open(f"{rowdir}/m0_p1kill.json")).get("metrics", {}))
print(p1.get("fsck_inodes_scanned", 0) - p0.get("fsck_inodes_scanned", 0))
PYK
)"
    [ "$((e1 - e0))" -ge 1 ] || die "s10c-kill-shard: the victim's lease never expired (lease_expiries delta $((e1 - e0)))"
    if [ "$((r1 - r0))" -lt 1 ] && [ "$((d1 - d0))" -lt 3 ]; then
        die "s10c-kill-shard: the lost residue never re-leased (relocal delta $((r1 - r0)), dispatched delta $((d1 - d0)))"
    fi
    [ "$((p1 - p0))" = "0" ] || die "s10c-kill-shard: a READ-shard expiry PR-preempted a host (delta $((p1 - p0))) — the design-§5 split is broken"
    [ "$((q1 - q0))" = "0" ] || die "s10c-kill-shard: a READ-shard expiry quarantined destinations (delta $((q1 - q0))) — read shards have none"
    [ "$inodes" = "$t0_inodes" ] || die "s10c-kill-shard: census total $inodes != baseline $t0_inodes — the re-leased residue double- or under-counted"
    log "s10c-kill-shard: lease expired ($((e1 - e0))), residue re-leased (relocal $((r1 - r0)), redispatch $((d1 - d0 - 2)), completed $((c1 - c0))), census exact ($inodes), preempts/quarantine 0"

    # Zero residue: the victim remounts and is a member again. kill -9
    # leaves a stale FUSE endpoint (ENOTCONN) — reap it first.
    fusermount3 -u "$(mnt_of 1)" 2>/dev/null || umount -l "$(mnt_of 1)" 2>/dev/null || true
    "$MWFLEET" mount 1 >/dev/null || die "s10c-kill-shard: victim remount failed"
    [ "$(stat_field 1 mount_posture)" = "reader" ] || die "s10c-kill-shard: remounted victim posture != reader"
    log "s10c-kill-shard GREEN (events + snapshots in $rowdir; victim remounted)"
}

case "$LEG" in
sym-*)
    # The venue word governs the sym legs' must-stay-0 judgement (Issue 2).
    log "venue: $SYM_VENUE — appender_flush_ceiling_overruns is $([ "$SYM_VENUE" = laptop ] && echo 'REPORTED as venue-attributed (the box bracket decides, 51bf21e1)' || echo 'must-stay-0')"
    ;;
esac
case "$LEG" in
smoke) leg_smoke ;;
multipath-negative) leg_multipath_negative ;;
s6-journal) leg_s6_journal ;;
s6-fence) leg_s6_fence ;;
s6-vm-fence) leg_s6_vm_fence ;;
s7-device-fence) leg_s7_device_fence ;;
s7-kill-matrix) leg_s7_kill_matrix ;;
sym-crash) leg_sym_crash ;;
sym-storm) leg_sym_storm ;;
sym-tarx) leg_sym_tarx ;;
sym-scale) leg_sym_scale ;;
sym-shared-dir) leg_sym_shared_dir ;;
sym-foreign-touch) leg_sym_foreign_touch ;;
sym-foreign-file) leg_sym_foreign_file ;;
sym-reclaim-hint) leg_sym_reclaim_hint ;;
sym-readers) leg_sym_readers ;;
sym-walls) leg_sym_walls ;;
s10c-fsck-scale) leg_s10c_fsck_scale ;;
s10c-kill-shard) leg_s10c_kill_shard ;;
vm-hostscope-validate) leg_vm_hostscope_validate ;;
vm-multi-identity) leg_vm_multi_identity ;;
sym-two-host) leg_sym_two_host ;;
*) die "unknown leg '$LEG' (pv-volume-scaling|smoke|multipath-negative|s6-journal|s6-fence|s6-vm-fence|s7-device-fence|s7-kill-matrix|sym-crash|sym-storm|sym-tarx|sym-scale|sym-shared-dir|sym-foreign-touch|sym-foreign-file|sym-reclaim-hint|sym-readers|sym-walls|s10c-fsck-scale|s10c-kill-shard|vm-hostscope-validate|vm-multi-identity|sym-two-host)" ;;
esac
