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
#   s8-serial-ab        (rung 9 — needs a fleet created with --multi-writer
#                       --cowriters=1; design row S8-a, spec §6.9 S8's gate
#                       VERBATIM: "serial tar -x A/B, published even if it
#                       regresses" — risk R1 accepted as ruling D10). The
#                       authority-LOCAL serial baseline vs the netns
#                       co-writer's SHIPPED stream at wire RTT ~0 / 250 µs
#                       / 1 ms (netem 0/125/500 µs per veth end); publishes
#                       the ops/s table + the meta_ship_phase_ns /
#                       meta_ship_owner_phase_ns attribution medians;
#                       row validity = the engagement law (client shipped
#                       == owner served on BOTH ledgers) + zero un-routed
#                       local commits. Instrument: SQZ_MWMATRIX_TAR_SRC=
#                       <dir> tars a real tree; default synthesizes the
#                       tar-x shape (labeled).
#   s8-crucible [--secs=S]  (rung 9; design row S8-b) the shipped-verb
#                       crucible over every co-writer member: sustained
#                       mdstorm-shaped mixed verbs (create/chmod/utimes/
#                       rename/unlink), INJECTED retries via server-side
#                       TCP session kills at the authority's custody port
#                       (a veth flap is INVISIBLE to TCP — retransmission
#                       absorbs it; measured), an authority kill-9 + remount
#                       mid-stream (the era split: stale_term_refusals on
#                       the NEW authority vs era_relearns on the clients),
#                       a co-writer kill-9 mid-stream followed by the
#                       online fsck/C8 oracle, then a post-events storm
#                       that must be error-free. Gates: owner_panics == 0,
#                       meta_ship_publish.refusals == 0,
#                       local_commit_refusals == 0 (the falsifier),
#                       fsck findings: 0. Default 1800 s
#                       (SQZ_MWMATRIX_S8B_SECS / --secs=S override —
#                       shorter windows are labeled in the row).
#   s10-delegation      (rung 12; design row §8.2 lever 1) LOOKUP-class
#                       delegations end-to-end on a --cowriters fleet:
#                       grant -> serve-local -> foreign-mutation -> recall
#                       -> CURRENT serve. Engagement = the wire-verb
#                       ledger (a delegated stat pass must NOT ship;
#                       dlm_delegation.* deltas account the serves);
#                       coherence = the authority's create in a delegated
#                       dir returns only after the holder ACKED the
#                       recall, and the holder sees the fresh name
#                       IMMEDIATELY (no staleness window). Includes the
#                       SQUEEZEFS_DELEGATION=0 A/B control mount (dark:
#                       zero hits, everything ships) and an authority
#                       kill-9 pass (holder re-admits by remount — the
#                       rung-10 documented posture — re-EARNS delegations
#                       under the successor era; fsck+C8 green). The
#                       surviving-process grace re-assert is pinned in
#                       cargo (tests/mw_delegation_tests.rs).
#   s10-intents         (rung 13; KD-MW-13, design §8.2 lever 1's UPDATE
#                       arm) EXCLUSIVE per-directory UPDATE grants +
#                       create-intent batches on a --cowriters fleet:
#                       earn -> local mints (zero wire) + deferred
#                       setattrs -> fsync(dir) flush (the contract point)
#                       -> foreign visibility; the OQ-2 live round (the
#                       owner's ls RECALLS the holder, which FLUSHES
#                       before the serve); the STORM row (the OQ-2 price,
#                       published — the reopening trigger's instrument);
#                       the SQUEEZEFS_UPDATE_INTENTS=0 A/B control; and
#                       the TWO-SIDED MW-8 kill-9 (pre-fsync = the
#                       acked-un-fsynced class, loss is a FIFO prefix;
#                       post-fsync = every name durable), fsck+C8 green
#                       after each kill.
#   s10-intents-tarx    (rung 13's measured row; the rung-9 s8-serial-ab
#                       instrument verbatim) tar -x on the netns co-writer
#                       at 250 µs RTT, UPDATE intents ON vs OFF, A-B-B-A;
#                       publishes entries/s + wire verbs/entry either way
#                       (row 14 owns the formal <=1.10x gate — this row is
#                       its input). Quiet-gated (cargo + loadavg).
#   s10-placement-tarx  (rung 14's FORMAL GATE row — design §8.3 / PR row
#                       14) tar -x, A-B-B-A of the co-writer at 250 µs RTT
#                       (placement+intents ON, the shipped defaults) vs
#                       the authority-LOCAL S0 baseline, SAME venue/
#                       binary/tarball. Publishes the gate table + verdict
#                       (<=1.10x of local) either way — a gate MISS prints
#                       the honest-statement pointer and exits 0 (the
#                       charter's explicit alternative); an INVALID row
#                       (engagement/oracle) exits nonzero. Also proves the
#                       migration policy's shipped-topology dark posture
#                       live (candidates == 0 on a one-authority fleet).
#                       Quiet-gated (cargo + loadavg).
#   cowriters-admission GATED (the 5b gate): the multi-identity legs rungs
#                       7-10 build on. Probes the recorded host-scoped
#                       verdict; on this kernel it SKIPs loud with the
#                       reason + remedy. `--require-host-scoped-subsys`
#                       turns the skip into a hard failure (automation on
#                       5b-kernel boxes). On a capable kernel the body
#                       still refuses: it lands with rungs 7-10.
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
#   s7-device-fence     (rung 8 — needs a fleet created with --multi-writer;
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
#   s7-kill-matrix [--rounds=N]  (rung 8 — needs --multi-writer; design
#                       row S7-b) kill -9 × N (default 10) of the ARMED
#                       writer at RANDOMIZED phases under sustained write
#                       load; each round: kill → dead-mount sweep →
#                       remount (the successor re-arms MW over the dead
#                       incarnation's STANDING WERO reservation — the
#                       device-observed takeover, fence_mode=1 asserted)
#                       → FULL online fsck with the C8 oracle
#                       (findings: 0; meta_kv_block_refs_drift == 0 — the
#                       --multi-writer format stamps bit 9, so the
#                       durable ledger runs for real) → tripwires flat
#                       (invariant_tripwires, data_dma_fence_refusals,
#                       R5 backstops all 0 on the successor). COUNTED-
#                       RESTART discipline: any failure aborts the count;
#                       the matrix restarts from zero on the fixed
#                       binary. Reader recovery + dirty-skip tripwire
#                       asserted at matrix end.
#   s9-fanout [--mb=M]  (rung 10 — needs --multi-writer --cowriters=K;
#                       design row S9-a) THE FAN-OUT ROW: K co-writers +
#                       the authority writing DATA concurrently — the
#                       first true multi-writer data rows. Two phases:
#                       fresh fan-out (per-member dd conv=fsync — the RW6
#                       durable discipline), then a concurrent REWRITE of
#                       the same files (the displaced-free + reuse face:
#                       co-writer frees SHIP and the lane harvest serves
#                       reuse). Emits per-member throughput + engagement
#                       columns (publish shipped/served, free ledger,
#                       write-through bytes) and the WRITE-AMPLIFICATION
#                       instrument per the tests/write_amp_rig.sh
#                       discipline: device bytes / user bytes on the DATA
#                       namespaces (/proc/diskstats deltas — meta rides
#                       its own namespaces, so the data delta is exact),
#                       wareq-sz vs the 4 MiB block size, and the
#                       block_free_* ledger. ATTRIBUTION LIMIT, stated:
#                       co-located members share one merged head per
#                       namespace, so device columns are per-NAMESPACE
#                       (fleet-aggregate), and per-member attribution is
#                       stats-side. Ends with the full online fsck + the
#                       C8 oracle. Per-member size auto-derives from the
#                       lane share (SQZ_MWMATRIX_S9A_MB / --mb=M caps it).
#   s9-failover         (rung 10; design row S9-b) authority kill -9
#                       MID-FAN-OUT: co-writers first write an
#                       fsync-acked corpus (sha256-recorded), then stream
#                       under load while the authority dies. The
#                       successor remounts (rung-8 takeover machinery);
#                       every co-writer must FENCE or relearn — zero
#                       silent old-era survivors (the E2 law); the
#                       fsync-acked corpus must verify byte-identical
#                       through the successor AND through every
#                       re-admitted co-writer (zero acked-data loss);
#                       fsck + C8 clean. Co-writer re-admission is BY
#                       REMOUNT — the rung-10 documented posture
#                       (docs/operations.md §Multi-writer co-writer
#                       mounts, "Failure and re-admission").
#   s9-colocated-fence [--victim=IDX]  (rung 10; design row S9-c) the
#                       CO-LOCATED fencing story: the victim co-writer
#                       shares the PR host identity with its authority,
#                       so THE DEVICE CANNOT REJECT its DMA (no
#                       reservation conflict exists for the holder's own
#                       key — classify_dma_outcome is structurally
#                       unreachable). SIGSTOP the victim past the
#                       authority's TTL (custody sweep + membership
#                       eviction + dead-epoch mint), SIGCONT: the victim
#                       must fail-stop CLIENT-SIDE (T_self self-fence /
#                       UnknownLease custody poison) with NO
#                       device-rejection line in its log — proving the
#                       epoch/poison gates alone carry the fencing story
#                       (cargo pin: tests/mw_colocated_fence_tests.rs).
#                       Blast radius = the victim; fsck + C8 clean;
#                       victim re-admits by remount.
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
#   s11-range [--mb=M]  (rung 15, KD-MW-7 — needs a fleet created with
#                       --multi-writer --cowriters=2) THE FIRST SUB-FILE
#                       MULTI-WRITER ROWS: the authority creates ONE
#                       striped file; TWO co-writers acquire DISJOINT
#                       byte-range custody of it over the S9 custody wire
#                       (the §9.2 required/desired admit — each holder's
#                       stream coalesces to ONE widened grant) and write
#                       their halves CONCURRENTLY under those grants
#                       (dd conv=fsync — durable rows). Engagement: each
#                       co-writer's dlm_custody_range_acquires delta >= 1
#                       (the ranged path ENGAGED, never a silent
#                       whole-file fallback), the authority's
#                       range_custody ledger accounts (grants >= 2,
#                       extensions reported) with cap_refusals delta == 0
#                       (the Issue-19 column: a within-budget legitimate
#                       shape refused = INVALID row) and publish ships
#                       accounted per co-writer. Verify: each half sha256
#                       through its writer's own mount, then the WHOLE
#                       file through a REMOUNTED authority (cold caches —
#                       the merged two-writer layout must compose
#                       byte-identically). Kill arm: kill -9 one holder
#                       MID-REWRITE — its ranges die with its era (the
#                       authority's custody sweep retires them:
#                       dlm_revokes_expired moves and range_custody_active
#                       converges to 0 once the survivor completes +
#                       releases), the SURVIVOR's stream completes green,
#                       the victim remounts. THE COMPOSITION GATE RUNS
#                       LAST AND IS STANDING-RED until rungs 16/17 land
#                       the concurrent same-ino publish composition (the
#                       G-RW2 pattern, live-leg form): cold-authority
#                       verify of the survivor's acked half + fsck
#                       findings 0 + C8 drift 0 — its red names the owed
#                       rungs, and the leg still tears down to zero
#                       residue (leg files removed, fleet healthy, every
#                       member re-admitted) before the verdict.
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
#   s11-mpiio [--procs=P]  (rung 18, §9.5 — needs a range-custody-ARMED
#                       fleet, --cowriters>=2; the design shape is 8)
#                       THE MPI-IO ACCEPTANCE ROW: `ior` (PINNED release
#                       4.0.0 + sha256, built on demand into
#                       target/mw-ior — OQ-4's adjudication), POSIX api,
#                       MPMD over the co-writer mounts (each app context
#                       names its own mount's path of ONE shared file —
#                       global ranks interleave 4 MiB-aligned segments
#                       block-cyclically), P procs per mount (default 4).
#                       A-B-B-A: shared, disjoint (-F file-per-proc,
#                       same fleet/geometry), disjoint, shared — each
#                       phase self-sized by a probe pass to a sustained
#                       >=60 s window of >=3 iterations (flatness gated:
#                       first-vs-last steady iteration within 30%); the
#                       shared file is capped at 10,240 MiB — the zram
#                       substrate budget, NOT a correctness boundary:
#                       the indirect domain composes (rung 20 residual
#                       #1's blob-aware owner-side merge). Runs over
#                       the mwfleet MEMBERS table, or over LIVE external
#                       mounts via SQZ_MWMATRIX_MOUNTS (the FIELD venue —
#                       see the EXTERNAL-MOUNTS MODE note below).
#                       GATE: BOTH brackets shared >= 0.8x disjoint.
#                       Engagement exact: every co-writer's ranged
#                       acquires engaged, authority grants account,
#                       cap_refusals == 0 (Issue-19), demotions == 0 and
#                       patch/overlay range-shared clauses == 0 (aligned
#                       rows share nothing), publish ships accounted.
#                       Correctness: ior read-back-exact (-r -R -C, the
#                       reorder-tasks cross-mount check under a fixed -G
#                       signature) + cold-authority fsck + C8 drift 0.
#   s11-blockcyclic     (rung 18, §9.5 — needs a range-custody-ARMED
#                       fleet) THE NON-COALESCIBLE LEGITIMATE SHAPE: one
#                       ior proc per co-writer mount, round-robin
#                       block-cyclic decomposition of one file (a
#                       holder's spans are NEVER adjacent by
#                       construction — nothing coalesces). GATES: grants
#                       ~= blocks-in-file with ZERO cap refusals below
#                       the R5 byte budget (the Issue-19 shape
#                       adjudicated live, not discovered), the live
#                       span table (range_custody_active,
#                       dlm_grant_table_bytes sampled DURING the write)
#                       accounts ~= spans x 48 B, and the aggregate
#                       stays within band (>= 0.8x) of a same-width
#                       disjoint (-F) control. fsck + C8 clean.
#   s11-tiny            (rung 18, §9.5 — needs --cowriters>=2) THE
#                       ADVERSARIAL TINY-RANGES BOUNDS ROW, live face:
#                       one co-writer floods byte-granular unaligned
#                       tiny writes across one file (required-only-class
#                       asks; desired block-aligns) while a SECOND
#                       co-writer's own-file fsync ops sample foreign-
#                       client latency. GATES: admit-time coalescing
#                       holds live spans <= the file's geometry cap
#                       (spans ~= O(file blocks)), dlm_grant_table_bytes
#                       bounded (<= spans x 48 B + wholes, << budget),
#                       zero cap refusals (a within-budget shape refused
#                       = the Issue-19 class), no wedge (every write
#                       completes), foreign-client latency during the
#                       storm within 5x its baseline median (reported
#                       exact). The AT-BUDGET refusal law itself is the
#                       standing in-process pin set (rung 15 pins a/b +
#                       the wire face — refusals name the arithmetic and
#                       converge by release); no live sub-budget shape
#                       can reach the derived byte budget honestly.
#   s11-killrange [--rounds=N]  (rung 18, §9.5 — needs a range-custody-
#                       ARMED fleet, --cowriters>=2) THE RANGE KILL
#                       MATRIX. Cell H x N (default 10): kill -9 a range
#                       HOLDER mid-write — the survivor's concurrent
#                       stream completes green, the authority sweeps the
#                       victim's era (dlm_revokes_expired moves,
#                       range_custody_active converges to 0),
#                       dlm_custody_grace_conflicts == 0, the victim
#                       re-admits by remount, fsck + C8 clean AFTER
#                       EVERY CELL. Cell A x 2: kill -9 the AUTHORITY
#                       mid-ASSEMBLY (two-holder sub-block extent churn
#                       live) — the authority remounts, co-writers
#                       re-admit, retained extents re-ship idempotently
#                       (MW-11's live face), the re-driven pass
#                       completes, bytes verify cold, fsck + C8 clean.
#
# Usage:  sudo tests/run_mw_matrix.sh <leg> [--require-host-scoped-subsys]
#         [--window=S] [--netem=MS] [--victim=IDX]   (the s6-* legs)
#         [--rounds=N]                               (s7-kill-matrix, s11-killrange)
#         [--procs=P]                                (s11-mpiio)
# Exit:   0 green (or a loud SKIP), nonzero on any INVALID row / violation.
#
# EXTERNAL-MOUNTS MODE (SQZ_MWMATRIX_MOUNTS — the FIELD venue; s11-mpiio
# ONLY, the smallest honest surface):
#   SQZ_MWMATRIX_MOUNTS=<authority-mnt>,<cw-mnt>,...  drives the named LIVE
#   mounts instead of the mwfleet MEMBERS table — e.g. the
#   tests/cluster_reset_v5_mw.sh fleet on a REAL nvme-tcp fabric. First
#   entry = the AUTHORITY mount, the rest = co-writer mounts (>= 2). The
#   fleet CONF/MEMBERS files are NEVER read: posture comes from each
#   mount's own .stats (`cat`, never cp), refused loud on a missing or
#   wrong-posture mount. Rows land under SQZ_MWMATRIX_ROWDIR (default:
#   <dirname of the authority mount>/mwmatrix-rows — the field's
#   /scratch/tmp convention). Netem arms do not exist on a real fabric (no
#   veth to shape; the s11-mpiio leg carries none anyway), and the fsck
#   oracle runs WARM on the live authority (an external fleet's remount
#   recipe belongs to its harness — the local fleet leg keeps its COLD
#   remount). Every other leg drives fleet-lifecycle verbs (kill/remount/
#   netem) an external fleet does not expose and REFUSES this mode.
#
# Requires: root, a live fleet (sudo tests/mw_fleet.sh create N=2), python3.

set -euo pipefail

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
S8B_SECS="${SQZ_MWMATRIX_S8B_SECS:-1800}"
S9A_MB_CAP="${SQZ_MWMATRIX_S9A_MB:-1024}"
S11_MB_CAP="${SQZ_MWMATRIX_S11_MB:-256}"
S10C_MB="${SQZ_MWMATRIX_S10C_MB:-3072}"
S10C_RUNS="${SQZ_MWMATRIX_S10C_RUNS:-3}"
for a in "$@"; do
    case "$a" in
    --require-host-scoped-subsys) REQUIRE_HS=1 ;;
    --window=*) S6_WINDOW_S="${a#--window=}" ;;
    --netem=*) S6_NETEM_MS="${a#--netem=}" ;;
    --victim=*) S6_VICTIM="${a#--victim=}" ;;
    --rounds=*) S7_ROUNDS="${a#--rounds=}" ;;
    --secs=*) S8B_SECS="${a#--secs=}" ;;
    --mb=*)
        S9A_MB_CAP="${a#--mb=}"
        S11_MB_CAP="${a#--mb=}"
        ;;
    --corpus-mb=*) S10C_MB="${a#--corpus-mb=}" ;;
    --runs=*) S10C_RUNS="${a#--runs=}" ;;
    --procs=*) S11_PROCS="${a#--procs=}" ;;
    *) die "unknown argument '$a'" ;;
    esac
done
S11_PROCS="${S11_PROCS:-4}"
[[ "$S11_PROCS" =~ ^[0-9]+$ ]] && [ "$S11_PROCS" -ge 1 ] && [ "$S11_PROCS" -le 16 ] ||
    die "--procs takes 1..16 (got '$S11_PROCS')"
[[ "$S9A_MB_CAP" =~ ^[0-9]+$ ]] && [ "$S9A_MB_CAP" -ge 64 ] || die "--mb takes MiB >= 64 (got '$S9A_MB_CAP')"
[[ "$S11_MB_CAP" =~ ^[0-9]+$ ]] && [ "$S11_MB_CAP" -ge 64 ] || die "--mb takes MiB >= 64 (got '$S11_MB_CAP')"
[[ "$S6_WINDOW_S" =~ ^[0-9]+$ ]] || die "--window takes seconds (got '$S6_WINDOW_S')"
[[ "$S6_NETEM_MS" =~ ^[0-9]+$ ]] || die "--netem takes ms (got '$S6_NETEM_MS')"
[[ "$S7_ROUNDS" =~ ^[0-9]+$ ]] && [ "$S7_ROUNDS" -ge 1 ] || die "--rounds takes a positive integer (got '$S7_ROUNDS')"
[[ "$S8B_SECS" =~ ^[0-9]+$ ]] && [ "$S8B_SECS" -ge 60 ] || die "--secs takes seconds >= 60 (got '$S8B_SECS')"
[[ "$S10C_MB" =~ ^[0-9]+$ ]] && [ "$S10C_MB" -ge 256 ] || die "--corpus-mb takes MiB >= 256 (got '$S10C_MB')"
[[ "$S10C_RUNS" =~ ^[0-9]+$ ]] && [ "$S10C_RUNS" -ge 1 ] || die "--runs takes a positive integer (got '$S10C_RUNS')"

ensure_root "$LEG" "$@"
# The admin-lane client half of the KD-7 dev override (the daemon half is
# mw_fleet.sh's mount env): dev-tree `-dirty` identities are degenerate,
# and the s7-kill-matrix's online-fsck oracle rides the admin lane.
export SQUEEZEFS_IPC_ALLOW_DEV=1
command -v python3 >/dev/null 2>&1 || die "python3 is required (the row emitter)"

# --- external-mounts mode (SQZ_MWMATRIX_MOUNTS — see the header note) --------
EXT_MODE=0
declare -A EXT_MNT=()
EXT_CW_IDXS=()
if [ -n "${SQZ_MWMATRIX_MOUNTS:-}" ]; then
    EXT_MODE=1
    [ "$LEG" = "s11-mpiio" ] ||
        die "SQZ_MWMATRIX_MOUNTS (external-mounts mode) supports ONLY the s11-mpiio leg — '$LEG' drives fleet-lifecycle verbs (kill/remount/netem) an external fleet does not expose"
    IFS=, read -r -a EXT_LIST <<<"$SQZ_MWMATRIX_MOUNTS"
    [ "${#EXT_LIST[@]}" -ge 3 ] ||
        die "SQZ_MWMATRIX_MOUNTS needs >= 3 comma-separated mounts (authority first, then >= 2 co-writers) — got ${#EXT_LIST[@]}: '$SQZ_MWMATRIX_MOUNTS'"
    EXT_MNT[0]="${EXT_LIST[0]}"
    for ((__i = 1; __i < ${#EXT_LIST[@]}; __i++)); do
        # Co-writers land on the COWRITER_BASE index slice (50..) so the
        # snapshots/rows read identically to the fleet leg's.
        EXT_MNT[$((49 + __i))]="${EXT_LIST[$__i]}"
        EXT_CW_IDXS+=("$((49 + __i))")
    done
    HOST_SCOPED=0 # irrelevant to this leg; kept for the shared plumbing
else
    [ -f "$CONF" ] || die "no live fleet at $STATE — run: sudo tests/mw_fleet.sh create N=2"
    # shellcheck disable=SC1090 # generated by mw_fleet.sh create
    . "$CONF"
    HOST_SCOPED="$(cat "$STATE/host_scoped" 2>/dev/null || echo 0)"
fi

mnt_of() {
    if [ "$EXT_MODE" = "1" ]; then
        echo "${EXT_MNT[$1]:-}"
    else
        awk -F'\t' -v i="$1" '$1==i {print $3}' "$MEMBERS"
    fi
}
role_of() {
    if [ "$EXT_MODE" = "1" ]; then
        if [ "$1" = "0" ]; then echo writer; else echo cowriter; fi
    else
        awk -F'\t' -v i="$1" '$1==i {print $2}' "$MEMBERS"
    fi
}
member_idxs() {
    if [ "$EXT_MODE" = "1" ]; then
        printf '%s\n' 0 "${EXT_CW_IDXS[@]}"
    else
        awk -F'\t' '{print $1}' "$MEMBERS" | sort -n
    fi
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
\$SQZ format --multi-writer "sqmeta:///dev/\$MH" "sqdata:///dev/\$DH" --force >/tmp/format.out 2>&1 || { cat /tmp/format.out; exit 1; }
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
require_mw() {
    require_membership
    [ "${MW:-0}" = "1" ] ||
        die "this leg needs a multi-writer-armed fleet — create it with: sudo tests/mw_fleet.sh create N=2 --multi-writer [--lease-ttl-ms=15000]"
    [ "$(stat_field 0 data_plane_fence_mode)" = "1" ] ||
        die "member 0 data_plane_fence_mode != 1 — the S7 WERO hold is not standing"
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

leg_s7_kill_matrix() {
    require_mw
    local rowdir w_mnt reader_idx
    rowdir="$STATE/rows/s7kill-$(date +%s)"
    mkdir -p "$rowdir"
    w_mnt="$(mnt_of 0)"
    reader_idx="$(member_idxs | awk '$1!=0' | head -1)"
    log "s7-kill-matrix: kill -9 x$S7_ROUNDS of the ARMED writer at randomized phases under sustained write load; per round: remount (WERO takeover over the dead incarnation's standing reservation) + FULL online fsck with the C8 oracle. COUNTED-RESTART discipline applies."

    local round phase_ms dd_pid t_kill t_up out findings drift fence_ref trip backstops fm
    printf '%-6s %-9s %-9s %-10s %-6s %-10s %-6s %s\n' ROUND PHASE_MS REMOUNT_S FSCK DRIFT FENCE_REF TRIP VERDICT | tee "$rowdir/matrix.tsv"
    for ((round = 1; round <= S7_ROUNDS; round++)); do
        # Sustained load, randomized kill phase (0.5 .. 8.5 s into it).
        rm -f "$w_mnt/s7kill.dat" 2>/dev/null || true
        (exec dd if=/dev/zero of="$w_mnt/s7kill.dat" bs=1M count=16384 conv=fsync status=none) &
        dd_pid=$!
        phase_ms=$((500 + RANDOM % 8000))
        sleep "$(python3 -c "print($phase_ms/1000)")"
        kill -0 "$dd_pid" 2>/dev/null ||
            die "round $round: write load died before the kill phase (${phase_ms}ms)"
        "$MWFLEET" kill 0 --sig 9
        t_kill="$(date +%s)"
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
        fm="$(stat_field 0 data_plane_fence_mode)"
        [ "$fm" = "1" ] || die "round $round: successor data_plane_fence_mode=$fm (want 1)"
        # The oracle: FULL online fsck (C1-C10, C8 ungated on this stamped
        # format — the durable ledger runs for real).
        out="$("$SQZ" fsck "$w_mnt" 2>&1)" ||
            die "round $round: online fsck FAILED or found:
$out"
        echo "$out" >"$rowdir/fsck-r$round.out"
        echo "$out" | grep -q "findings: 0" ||
            die "round $round: fsck findings != 0:
$out"
        findings=0
        drift="$(stat_field 0 meta_kv_block_refs_drift)"
        [ "$drift" = "0" ] || die "round $round: meta_kv_block_refs_drift=$drift (C8 oracle RED)"
        fence_ref="$(stat_field 0 data_dma_fence_refusals)"
        [ "$fence_ref" = "0" ] || die "round $round: successor data_dma_fence_refusals=$fence_ref (a fresh mount fenced itself)"
        # Finding-#4 tripwire: a K=0 fleet's authority is SOLO — any
        # engaged allocation partition here is a phantom lane (the
        # claim-identity mismatch class, or accreted phantom writers).
        local lanes
        lanes="$(stat_field 0 alloc_lane_writers)"
        [ "$lanes" = "0" ] || die "round $round: alloc_lane_writers=$lanes on a 0-co-writer fleet — a phantom allocation partition is engaged (rung-8 findings #3/#4 class)"
        trip="$(stat_field 0 invariant_tripwires)"
        [ "$trip" = "0" ] || die "round $round: invariant_tripwires=$trip on the successor"
        backstops="$(stat_field 0 mem_budget_hard_backstops)"
        [ "$backstops" = "0" ] || die "round $round: mem_budget_hard_backstops=$backstops (R5 column)"
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
    log "s7-kill-matrix GREEN: $S7_ROUNDS/$S7_ROUNDS rounds (table + fsck reports in $rowdir)"
}

# --- rung 9: the S8 rows ------------------------------------------------------

cowriter_idxs() {
    if [ "$EXT_MODE" = "1" ]; then
        printf '%s\n' "${EXT_CW_IDXS[@]}"
    else
        awk -F'\t' '$2=="cowriter" {print $1}' "$MEMBERS" | sort -n
    fi
}

require_cowriters() { # min
    require_mw
    local n
    n="$(cowriter_idxs | wc -l)"
    [ "$n" -ge "${1:-1}" ] ||
        die "this leg needs ${1:-1} co-writer member(s) (found $n) — create the fleet with: sudo tests/mw_fleet.sh create N=2 --multi-writer --cowriters=${1:-1}"
}

# Weighted-median bucket label of a phase histogram DELTA between two raw
# stats snapshots, plus its sample count: the attribution instrument the
# S8-a row publishes (LatencyHistogram is bucket-only by design).
phase_median() { # p0.json p1.json hist_key phase -> "<median-bucket> n=<count>"
    python3 - "$1" "$2" "$3" "$4" <<'PYEOF'
import json, sys
p0, p1, hist, phase = sys.argv[1:5]
def load(p):
    root = json.load(open(p))
    m = root.get("metrics", root)
    return m.get(hist, {}).get(phase, {})
a, b = load(p0), load(p1)
delta = [(k, (b.get(k, 0) or 0) - (a.get(k, 0) or 0)) for k in b]
total = sum(n for _, n in delta)
if total <= 0:
    print("- n=0")
    sys.exit(0)
# Bucket order = the label's numeric bound (parse "<=NNNus/ms/s").
def bound(lbl):
    s = lbl.lstrip("<=>")
    for suf, mul in (("us", 1), ("ms", 1000), ("s", 1000000)):
        if s.endswith(suf):
            return int(s[: -len(suf)]) * mul
    return 1 << 62
delta.sort(key=lambda kv: bound(kv[0]))
acc = 0
for lbl, n in delta:
    acc += n
    if acc * 2 >= total:
        print(f"{lbl} n={total}")
        break
PYEOF
}

# One serial tar -x venue: extract the leg's tarball onto `mnt` under a
# fresh dir, timed; snapshot writer+cowriter around it; emit the row line.
s8a_venue() { # rowdir label mnt cw_idx entries tarball
    local rowdir="$1" label="$2" mnt="$3" cw="$4" entries="$5" tarball="$6"
    local dest t0 t1 wall ops
    dest="$mnt/s8a-$label"
    mkdir -p "$dest"
    # In-flight settle, then CLIENT-first snapshot order (both ends): a
    # `.stats` read through the co-writer mount SHIPS its own kernel
    # lookup (S8 raw — every metadata verb pays the wire), so the client
    # snapshot's self-inflicted verb must land INSIDE the owner's served
    # window or exact shipped==served equality races the instrument.
    sleep 2
    [ -n "$cw" ] && snap "$cw" "${label}0" "$rowdir"
    snap 0 "${label}0" "$rowdir"
    t0="$(date +%s.%N)"
    tar -xf "$tarball" -C "$dest" ||
        die "s8a venue $label: tar -x FAILED on $mnt (a shipped verb errored — see the daemon logs)"
    t1="$(date +%s.%N)"
    sleep 2
    [ -n "$cw" ] && snap "$cw" "${label}1" "$rowdir"
    snap 0 "${label}1" "$rowdir"
    # Return the venue's blocks before the next one (untimed; a cache-less
    # fleet stores every beyond-inline file as a whole striped block, so a
    # 4-venue sweep would otherwise exhaust the lane share — on a
    # co-writer mount this also exercises the S9 shipped-free path).
    rm -rf "$dest"
    wall="$(python3 -c "print(f'{$t1-$t0:.2f}')")"
    ops="$(python3 -c "print(f'{$entries/($t1-$t0):.0f}')")"
    echo "$label $wall $ops"
}

s8a_delta() { # rowdir idx label key -> delta of a flattened stats key
    python3 - "$1" "$2" "$3" "$4" <<'PYEOF'
import json, sys
rowdir, idx, label, key = sys.argv[1:5]
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
va, vb = a.get(key, 0) or 0, b.get(key, 0) or 0
print(int(vb - va) if isinstance(vb, (int, float)) and isinstance(va, (int, float)) else 0)
PYEOF
}

leg_s8_serial_ab() {
    # Design row S8-a — spec §6.9 S8's gate, VERBATIM: "serial `tar -x`
    # A/B, published even if it regresses" (risk R1: at 50-150 µs RTT a
    # serial stream drops from 9,100/s to 6.7-20 k/s before owner
    # queueing; ruling D10 ACCEPTS it — S10's delegation is the recovery).
    # Venues: authority-LOCAL baseline, then the netns co-writer at wire
    # RTT ~0 / 250 µs / 1 ms (netem delay 0/125/500 µs per veth end). The
    # row's attribution is meta_ship_phase_ns (route/queue_wait/encode/
    # rtt/decode medians) + the owner-side execute median; its validity is
    # the engagement law (client shipped == owner served, BOTH ledgers).
    require_cowriters 1
    local rowdir cw w_mnt cw_mnt tarball src entries
    rowdir="$STATE/rows/s8a-$(date +%s)"
    mkdir -p "$rowdir"
    cw="$(cowriter_idxs | head -1)"
    w_mnt="$(mnt_of 0)"

    # The instrument: a real source tree when SQZ_MWMATRIX_TAR_SRC names
    # one (the linux-src venue), else a synthesized tar-x-shaped tree
    # (serial create-heavy: dirs + small inline-sized files) — the
    # fallback the rung-9 brief sanctions, labeled in the row.
    tarball="$STATE/s8a-src.tar"
    src="${SQZ_MWMATRIX_TAR_SRC:-}"
    if [ -n "$src" ]; then
        [ -d "$src" ] || die "SQZ_MWMATRIX_TAR_SRC='$src' is not a directory"
        tar -cf "$tarball" -C "$(dirname "$src")" "$(basename "$src")"
        log "s8a instrument: REAL tree $src"
    else
        local synth="$STATE/s8a-tree" d f
        rm -rf "$synth"
        for ((d = 0; d < 120; d++)); do
            mkdir -p "$synth/d$d"
            for ((f = 0; f < 24; f++)); do
                head -c $((128 + (d * 24 + f) % 1900)) /dev/zero >"$synth/d$d/f$f.c"
            done
        done
        tar -cf "$tarball" -C "$STATE" s8a-tree
        log "s8a instrument: SYNTHESIZED tar-x shape (120 dirs x 24 inline-sized files; set SQZ_MWMATRIX_TAR_SRC=<dir> for a real tree)"
    fi
    entries="$(tar -tf "$tarball" | wc -l)"
    log "s8a tarball: $entries entries"

    # The co-writer must sit in a netns so its SHIPPING wire is shapeable;
    # remount it there (the fleet mounts co-writers plain).
    "$MWFLEET" unmount "$cw"
    "$MWFLEET" mount "$cw" --netns
    cw_mnt="$(mnt_of "$cw")"

    local -a rows=()
    local out label netem
    # Venue 1: the authority-LOCAL serial baseline (the S0 reference).
    out="$(s8a_venue "$rowdir" local "$w_mnt" "" "$entries" "$tarball")"
    rows+=("$out -")
    # Venues 2-4: the SHIPPED stream across the RTT ladder.
    for netem in off 125us 500us; do
        case "$netem" in
        off) label="ship-rtt0" ;;
        125us) label="ship-rtt250us" ;;
        500us) label="ship-rtt1ms" ;;
        esac
        [ "$netem" = "off" ] || "$MWFLEET" netem "$cw" "$netem"
        out="$(s8a_venue "$rowdir" "$label" "$cw_mnt" "$cw" "$entries" "$tarball")"
        # Engagement (row validity): the client's shipped verbs == the
        # owner's served verbs, on BOTH ledgers (S8 trait + S9 publish).
        local ship_d served_d pub_ship_d pub_served_d batches_d bverbs_d refusals panics lcr
        ship_d="$(s8a_delta "$rowdir" "$cw" "$label" meta_ship.shipped_verbs)"
        served_d="$(s8a_delta "$rowdir" 0 "$label" meta_ship.served_verbs)"
        pub_ship_d="$(s8a_delta "$rowdir" "$cw" "$label" meta_ship_publish.shipped)"
        pub_served_d="$(s8a_delta "$rowdir" 0 "$label" meta_ship_publish.served)"
        batches_d="$(s8a_delta "$rowdir" "$cw" "$label" meta_ship.batches)"
        bverbs_d="$(s8a_delta "$rowdir" "$cw" "$label" meta_ship.batched_verbs)"
        [ "$ship_d" -gt 0 ] || die "s8a $label: shipped_verbs delta 0 — the row did not engage the S8 plane"
        # Engagement: shipped == served, within the instrument's OWN skew —
        # reading a co-writer's .stats ships the read's own kernel verbs
        # (S8 raw pays the wire for everything, the instrument included),
        # so the two windows can differ by the snapshot ceremony's reads.
        # A MATERIAL gap is the falsifier; ±4 is the ceremony's size.
        [ $((ship_d - served_d)) -le 4 ] && [ $((served_d - ship_d)) -le 4 ] ||
            die "s8a $label: ships that don't account — cw shipped=$ship_d vs owner served=$served_d"
        [ $((pub_ship_d - pub_served_d)) -le 4 ] && [ $((pub_served_d - pub_ship_d)) -le 4 ] ||
            die "s8a $label: publish ships that don't account — shipped=$pub_ship_d vs served=$pub_served_d"
        refusals="$(stat_field 0 meta_ship_publish.refusals)"
        [ "$refusals" = "0" ] || die "s8a $label: meta_ship_publish.refusals=$refusals (must stay 0)"
        panics="$(stat_field 0 meta_ship.owner_panics)"
        [ "$panics" = "0" ] || die "s8a $label: owner_panics=$panics"
        lcr="$(s8a_delta "$rowdir" "$cw" "$label" cowriter.local_commit_refusals)"
        [ "$lcr" = "0" ] || die "s8a $label: cowriter.local_commit_refusals moved by $lcr — an un-routed local commit (the S8-b falsifier)"
        rows+=("$out ship=$ship_d+pub=$pub_ship_d coalesce=$(python3 -c "print(f'{$bverbs_d/max(1,$batches_d):.2f}')")")
    done
    "$MWFLEET" netem "$cw" off || true

    # THE PUBLISHED TABLE (spec R1: even if it regresses — it will).
    echo ""
    echo "== S8-a: serial tar -x A/B (entries=$entries; venue=tcp devsub; instrument above) =="
    printf '%-14s %-8s %-8s %s\n' VENUE WALL_S OPS_S ENGAGEMENT
    local r
    for r in "${rows[@]}"; do
        # shellcheck disable=SC2086 # deliberate word split of the row line
        printf '%-14s %-8s %-8s %s\n' $r
    done | tee "$rowdir/s8a-table.txt"
    echo ""
    echo "== S8-a attribution (phase medians over the ship-rtt1ms venue deltas) =="
    local ph
    for ph in route queue_wait encode rtt decode; do
        printf '  %-11s %s\n' "$ph" "$(phase_median "$rowdir/m${cw}_pship-rtt1ms0.json" "$rowdir/m${cw}_pship-rtt1ms1.json" meta_ship_phase_ns "$ph")"
    done | tee "$rowdir/s8a-attribution.txt"
    for ph in admit dispatch execute reply_encode total; do
        printf '  owner/%-11s %s\n' "$ph" "$(phase_median "$rowdir/m0_pship-rtt1ms0.json" "$rowdir/m0_pship-rtt1ms1.json" meta_ship_owner_phase_ns "$ph")"
    done | tee -a "$rowdir/s8a-attribution.txt"
    log "s8-serial-ab PUBLISHED (rows + snapshots in $rowdir)"
}

s8b_storm() { # mnt tag secs faillog — the mdstorm-shaped mixed-verb loop
    local mnt="$1" tag="$2" secs="$3" faillog="$4" base i=0 t_end d f
    base="$mnt/s8b-$tag"
    mkdir -p "$base"
    t_end=$(($(date +%s) + secs))
    while [ "$(date +%s)" -lt "$t_end" ]; do
        d="$base/dir$((i % 16))"
        f="$d/f$i"
        {
            mkdir -p "$d" &&
                : >"$f" &&
                chmod 600 "$f" &&
                touch -d @1699999999 "$f" &&
                mv "$f" "$f.r" &&
                rm "$f.r"
        } 2>>"$faillog" || echo "op-cycle $i failed at $(date +%s.%N)" >>"$faillog"
        i=$((i + 1))
    done
    echo "$i" >"$faillog.cycles"
}

# stat_field on a FENCED co-writer mount (dead-until-remount) answers
# EINVAL/empty — the crucible reads through this defaulting form.
sfield0() { # idx key -> value or 0
    local v
    v="$(stat_field "$1" "$2" 2>/dev/null || true)"
    echo "${v:-0}"
}

leg_s8_crucible() {
    # Design row S8-b — the shipped-verb crucible: mdstorm-shaped mixed
    # verbs (create/chmod/utimes/rename/unlink — inline-sized, the
    # metadata plane's row) sustained across every co-writer, with the
    # dedup window exercised by INJECTED retries (partition flaps on a
    # netns co-writer), the era split proven across an authority restart
    # (stale_term_refusals on the NEW authority / era_relearns on the
    # clients — counted on opposite sides so one event can never
    # double-count), a co-writer kill-9 mid-stream, and the fsck/C8
    # oracle after the events. Gates: owner_panics == 0,
    # meta_ship_publish.refusals == 0, local_commit_refusals == 0 (the
    # falsifier: any un-routed local commit), engagement shipped==served.
    require_cowriters 1
    local secs="$S8B_SECS"
    local rowdir w_mnt cws idx
    rowdir="$STATE/rows/s8b-$(date +%s)"
    mkdir -p "$rowdir"
    w_mnt="$(mnt_of 0)"
    mapfile -t cws < <(cowriter_idxs)
    log "s8-crucible: ${#cws[@]} co-writer(s), ${secs}s total (phase A storm -> injected TCP-kill retries -> authority restart -> co-writer kill-9 -> fsck oracle -> phase C storm). Design asks K=5; this fleet runs K=${#cws[@]} (stated in the row)."

    command -v ss >/dev/null 2>&1 || die "s8-crucible needs iproute2 ss (the E1 TCP-kill injector)"
    local mw_port="${MW_ENDPOINT##*:}"
    [ -n "$mw_port" ] || die "no MW_ENDPOINT recorded"

    for idx in 0 "${cws[@]}"; do snap "$idx" 0 "$rowdir"; done

    # ---- Phase A: quiet sustained storm (must be error-free) ------------
    local phase_a=$((secs * 4 / 10)) phase_c=$((secs * 3 / 10))
    local -a pids=()
    for idx in "${cws[@]}"; do
        s8b_storm "$(mnt_of "$idx")" "a-m$idx" "$phase_a" "$rowdir/fail-a-m$idx" &
        pids+=("$!")
    done

    # ---- E1: injected retries (server-side TCP kills, mid-storm) --------
    # A 2 s veth partition is INVISIBLE to TCP (retransmission absorbs it
    # — measured: five flaps under storm, zero retries), so the injector
    # kills the ESTABLISHED wire sessions at the authority's port instead:
    # the client's next call fails instantly, the router reconnects once
    # and RESENDS THE SAME IDS (its documented law) — and a kill that
    # landed after execute-before-reply is answered from the dedup window.
    sleep $((phase_a / 3))
    local retries0 retries1 dedup0 dedup1
    rsum() {
        local acc=0 i
        for i in "${cws[@]}"; do
            acc=$((acc + $(sfield0 "$i" meta_ship.retries)))
        done
        echo "$acc"
    }
    retries0="$(rsum)"
    dedup0="$(stat_field 0 meta_ship.dedup_hits)"
    for _flap in 1 2 3 4 5; do
        ss -K state established "( sport = :$mw_port )" >/dev/null 2>&1 || true
        sleep 4
    done
    wait "${pids[@]}" || true
    retries1="$(rsum)"
    dedup1="$(stat_field 0 meta_ship.dedup_hits)"
    log "E1 injected retries: retries(sum) $retries0 -> $retries1, owner dedup_hits $dedup0 -> $dedup1"
    [ "$retries1" -gt "$retries0" ] ||
        die "E1: five wire-session kills under storm injected NO transport retries — the injector did not engage"
    [ "$dedup1" -gt "$dedup0" ] ||
        warn "E1: no dedup-window replay landed (kills never split execute from reply this run) — retries prove the resend law; dedup exactness stays pinned in meta_ship_tests"
    # Phase-A verdict: failures during the kill windows are recorded and
    # REPORTED (a kill can consume both of one exchange's attempts); the
    # hard error-free gate is phase C, after every event settles.
    for idx in "${cws[@]}"; do
        if [ -s "$rowdir/fail-a-m$idx" ]; then
            warn "phase A: co-writer m$idx storm recorded $(grep -c . "$rowdir/fail-a-m$idx") failure line(s) during injection (recorded, phase C is the hard gate)"
        fi
    done
    # The S7 COMPOSITION face (observed live): a kill storm that also
    # starves a co-writer's custody RENEWAL past its TTL makes the
    # authority sweep the lease, and the co-writer SELF-FENCES — poisoned,
    # dead-until-remount (the pull-based revocation law working, never a
    # bug). Record every fenced member and REMOUNT it (the rung-9
    # re-admission posture) before the next event.
    local fenced=0
    for idx in "${cws[@]}"; do
        if ! cat "$(mnt_of "$idx")/.stats" >/dev/null 2>&1 ||
            [ "$(sfield0 "$idx" mount_posture)" != "co-writer" ]; then
            fenced=$((fenced + 1))
            log "E1: co-writer m$idx SELF-FENCED under the injection (custody renewal starved past TTL — the S7 law engaging); remounting"
            "$MWFLEET" unmount "$idx" || true
            "$MWFLEET" mount "$idx" || die "E1: fenced co-writer m$idx could not re-admit"
        fi
    done
    echo "E1 retries=$((retries1 - retries0)) dedup_hits=$((dedup1 - dedup0)) self_fenced_remounted=$fenced" >>"$rowdir/events.txt"

    # ---- E2: authority restart mid-stream (the era split) ---------------
    local stale1 relearn0 relearn1 t_kill t_up
    relearn0=0
    for idx in "${cws[@]}"; do
        relearn0=$((relearn0 + $(sfield0 "$idx" meta_ship.era_relearns)))
    done
    local -a pids2=()
    for idx in "${cws[@]}"; do
        s8b_storm "$(mnt_of "$idx")" "e2-m$idx" 45 "$rowdir/fail-e2-m$idx" &
        pids2+=("$!")
    done
    sleep 5
    local w_pid
    w_pid="$(awk -F'\t' '$1==0 {print $7}' "$MEMBERS")"
    "$MWFLEET" kill 0 --sig 9
    t_kill="$(date +%s)"
    umount -l "$w_mnt" 2>/dev/null || true
    wait_for_unmounted "$w_mnt"
    # The daemon-lifetime flock dies WITH THE PROCESS, and a SIGKILL'd
    # daemon under 5 storming co-writers takes seconds to actually exit —
    # remounting before then meets a live holder (measured: 'another
    # squeezefs process holds the writer lock … age=6s').
    local tries
    for ((tries = 0; tries < 120; tries++)); do
        kill -0 "$w_pid" 2>/dev/null || break
        sleep 0.5
    done
    kill -0 "$w_pid" 2>/dev/null && die "E2: the killed authority (pid $w_pid) never exited"
    "$MWFLEET" mount 0 || die "E2: successor authority remount FAILED"
    t_up="$(date +%s)"
    wait "${pids2[@]}" || true
    # THE MEASURED COMPOSITION (this leg, live, three runs): the S7 custody
    # law WINS the race against the S8 era gate — within seconds of the
    # successor answering, every co-writer's custody renewal meets
    # UnknownLease and the mount SELF-FENCES (poisoned, dead-until-remount,
    # 'the client's is stricter'), usually before any old-era mutating
    # frame lands a STALE_TERM refusal. That is the STRICTER outcome (a
    # fail-stop instead of a relearn), so the live gate is: the successor's
    # era ADVANCED, and NO co-writer survives the restart SILENTLY — each
    # one either relearned the era (the S8 face) or self-fenced (the S7
    # face). The split's own exactness (stale counted on the ISSUING owner,
    # relearn on the CLIENT, refused-whole, exactly-once across the retry)
    # is deterministically pinned in-process: tests/meta_ship_tests.rs.
    local tries fenced_e2 silent
    stale1=0
    relearn1="$relearn0"
    for ((tries = 0; tries < 30; tries++)); do
        stale1="$(stat_field 0 meta_ship.stale_term_refusals)"
        relearn1=0
        for idx in "${cws[@]}"; do
            relearn1=$((relearn1 + $(sfield0 "$idx" meta_ship.era_relearns)))
        done
        fenced_e2=0
        for idx in "${cws[@]}"; do
            if ! cat "$(mnt_of "$idx")/.stats" >/dev/null 2>&1 ||
                [ "$(sfield0 "$idx" mount_posture)" != "co-writer" ]; then
                fenced_e2=$((fenced_e2 + 1))
            fi
        done
        # Settle when every co-writer has produced ONE of the two signals.
        [ $((fenced_e2)) -ge ${#cws[@]} ] && break
        [ "$relearn1" -gt "$relearn0" ] && [ "$fenced_e2" -eq 0 ] && break
        sleep 1
    done
    silent=0
    for idx in "${cws[@]}"; do
        if cat "$(mnt_of "$idx")/.stats" >/dev/null 2>&1 &&
            [ "$(sfield0 "$idx" mount_posture)" = "co-writer" ] &&
            [ "$(sfield0 "$idx" meta_ship.era_relearns)" = "0" ]; then
            silent=$((silent + 1))
            warn "E2: co-writer m$idx SURVIVED the restart with no era relearn and no fence — a silent old-era survivor"
        fi
    done
    log "E2 authority restart: remount $((t_up - t_kill))s; successor stale_term_refusals=$stale1; co-writer era_relearns $relearn0 -> $relearn1; self-fenced=$fenced_e2/${#cws[@]} (the S7 face — 'the client's is stricter')"
    echo "E2 stale_term_refusals=$stale1 era_relearns_delta=$((relearn1 - relearn0)) self_fenced=$fenced_e2 remount_s=$((t_up - t_kill))" >>"$rowdir/events.txt"
    [ "$silent" -eq 0 ] ||
        die "E2: $silent co-writer(s) kept operating on the dead authority's era with NO signal — the era fence has a hole"
    # Rung-9 posture: co-writer RE-ADMISSION after an authority failover is
    # by REMOUNT at this rung (S9-b's automatic re-admission row is rung
    # 10's); the successor's era must ADMIT them (its arm re-enrolled the
    # roster, a fresh era).
    for idx in "${cws[@]}"; do
        "$MWFLEET" unmount "$idx" || true
        "$MWFLEET" mount "$idx" ||
            die "E2: co-writer m$idx could not re-admit under the successor era"
    done
    log "E2: all ${#cws[@]} co-writer(s) re-admitted under the successor era (by remount — the rung-9 posture; automatic re-admission is S9-b/rung 10)"

    # ---- E3: co-writer kill -9 mid-stream + the oracle -------------------
    local victim="${cws[$((${#cws[@]} - 1))]}"
    s8b_storm "$(mnt_of "$victim")" "e3" 60 "$rowdir/fail-e3" &
    local storm3=$!
    sleep 3
    local v_pid
    v_pid="$(awk -F'\t' -v i="$victim" '$1==i {print $7}' "$MEMBERS")"
    "$MWFLEET" kill "$victim" --sig 9
    kill -9 "$storm3" 2>/dev/null || true
    wait "$storm3" 2>/dev/null || true
    umount -l "$(mnt_of "$victim")" 2>/dev/null || true
    for ((tries = 0; tries < 120; tries++)); do
        kill -0 "$v_pid" 2>/dev/null || break
        sleep 0.5
    done
    # The dedup window's exactly-once face: nothing half-applied survives.
    local out
    out="$("$SQZ" fsck "$w_mnt" 2>&1)" ||
        die "E3: online fsck FAILED after the co-writer kill-9:
$out"
    echo "$out" >"$rowdir/fsck-e3.out"
    echo "$out" | grep -q "findings: 0" || die "E3: fsck findings != 0 after a co-writer kill-9:
$out"
    local drift
    drift="$(stat_field 0 meta_kv_block_refs_drift)"
    [ "$drift" = "0" ] || die "E3: meta_kv_block_refs_drift=$drift (C8 oracle RED)"
    "$MWFLEET" mount "$victim" || die "E3: the killed co-writer could not RE-ADMIT (its slot id is mount-point-stable, so the roster still names it)"
    log "E3: co-writer m$victim kill-9 -> fsck findings:0, drift=0, re-admitted"

    # ---- Phase C: post-events storm (must be error-free everywhere) -----
    local -a pids3=()
    for idx in "${cws[@]}"; do
        s8b_storm "$(mnt_of "$idx")" "c-m$idx" "$phase_c" "$rowdir/fail-c-m$idx" &
        pids3+=("$!")
    done
    wait "${pids3[@]}" || true
    for idx in "${cws[@]}"; do
        [ -s "$rowdir/fail-c-m$idx" ] &&
            die "phase C: co-writer m$idx storm errored AFTER the events settled:
$(head -5 "$rowdir/fail-c-m$idx")"
    done

    for idx in 0 "${cws[@]}"; do snap "$idx" 1 "$rowdir"; done

    # ---- Verdicts ---------------------------------------------------------
    local panics refusals lcr trip
    panics="$(stat_field 0 meta_ship.owner_panics)"
    [ "$panics" = "0" ] || die "s8b: owner_panics=$panics (must stay 0)"
    refusals="$(stat_field 0 meta_ship_publish.refusals)"
    [ "$refusals" = "0" ] || die "s8b: meta_ship_publish.refusals=$refusals (must stay 0)"
    for idx in "${cws[@]}"; do
        lcr="$(stat_field "$idx" cowriter.local_commit_refusals)"
        [ "$lcr" = "0" ] || die "s8b: co-writer m$idx local_commit_refusals=$lcr — an un-routed local commit (THE falsifier)"
        trip="$(stat_field "$idx" invariant_tripwires)"
        [ "$trip" = "0" ] || die "s8b: co-writer m$idx invariant_tripwires=$trip"
    done
    trip="$(stat_field 0 invariant_tripwires)"
    [ "$trip" = "0" ] || die "s8b: authority invariant_tripwires=$trip"
    # Engagement across the whole crucible: since E2 restarted the
    # authority, engagement is asserted on the POST-E2 half (phase C):
    # served on the successor >= the sum of phase-C ships is not exactly
    # decomposable per-phase from totals, so the law is asserted live:
    local ship_now served_now pub_ship pub_served
    ship_now=0
    pub_ship=0
    for idx in "${cws[@]}"; do
        ship_now=$((ship_now + $(sfield0 "$idx" meta_ship.shipped_verbs)))
        pub_ship=$((pub_ship + $(sfield0 "$idx" meta_ship_publish.shipped)))
    done
    served_now="$(stat_field 0 meta_ship.served_verbs)"
    pub_served="$(stat_field 0 meta_ship_publish.served)"
    log "s8b ledgers at close: cw shipped(S8)=$ship_now vs successor served=$served_now (pre-E2 serves died with the old authority — recorded, not equated); publish shipped=$pub_ship vs served=$pub_served"
    log "s8-crucible GREEN (events + snapshots + fail logs in $rowdir)"
}

# --- rung 10: the S9 rows ------------------------------------------------------

# /proc/diskstats write columns for one device basename: "wios wsect".
disk_wcols() { # /dev/nvmeXnY
    awk -v d="$(basename "$1")" '$3==d {print $8, $10}' /proc/diskstats
}

# Snapshot the write columns of every DATA + META namespace.
disk_wsnap() { # rowdir phase
    local out="$1/disk_p$2.tsv" p
    : >"$out"
    local IFS=,
    for p in $FORMAT_DATA_PATHS; do
        echo "data $(basename "$p") $(disk_wcols "$p")" >>"$out"
    done
    for p in $FORMAT_META_PATHS; do
        echo "meta $(basename "$p") $(disk_wcols "$p")" >>"$out"
    done
}

# One member's timed dd, backgrounded; the wall + rc land in $out.
s9_dd() { # out file mb [extra dd conv flags appended to conv=fsync]
    local out="$1" f="$2" mb="$3" conv="${4:-fsync}"
    (
        local t0 t1 rc=0
        t0="$(date +%s.%N)"
        dd if=/dev/zero of="$f" bs=1M count="$mb" conv="$conv" status=none 2>"$out.err" || rc=$?
        t1="$(date +%s.%N)"
        echo "$rc $t0 $t1 $mb" >"$out"
    ) &
}

leg_s9_fanout() {
    require_cowriters 1
    local rowdir cws idx
    rowdir="$STATE/rows/s9a-$(date +%s)"
    mkdir -p "$rowdir"
    mapfile -t cws < <(cowriter_idxs)
    local k="${#cws[@]}"

    # Foreign-load honesty (the measured-row law): state the box.
    log "s9-fanout preconditions: loadavg=$(cut -d' ' -f1-3 /proc/loadavg), foreign cargo=$(pgrep -c cargo || true)"

    # Sizing from the LANE SHARE (never a free constant): total data
    # capacity / the engaged width, phase W at 40% of a lane, rewrite at
    # half of that — so the row never manufactures an ENOSPC and the
    # harvest stays the cargo suite's row, not this one's.
    local total_bytes=0 p w
    local IFS_SAVE="$IFS"
    IFS=,
    for p in $FORMAT_DATA_PATHS; do
        total_bytes=$((total_bytes + $(blockdev --getsize64 "$p")))
    done
    IFS="$IFS_SAVE"
    w="$(stat_field 0 alloc_lane_writers)"
    [ -n "$w" ] && [ "$w" -ge 2 ] ||
        die "s9-fanout: alloc_lane_writers=$w on the authority — a --cowriters fleet must run an engaged allocation partition"
    local lane_mb=$((total_bytes / w / 1024 / 1024))
    local mb=$((lane_mb * 40 / 100))
    [ "$mb" -le "$S9A_MB_CAP" ] || mb="$S9A_MB_CAP"
    [ "$mb" -ge 64 ] || die "s9-fanout: derived per-member size ${mb}MiB < 64MiB — the volumes are too small for a meaningful row (grow SQZ_MWFLEET_OSS_GB)"
    local rw_mb=$((mb / 2))
    log "s9-fanout: $((k + 1)) concurrent writers (authority + $k co-writers), W=$w, lane share ${lane_mb}MiB, phase-W ${mb}MiB/member + phase-R rewrite ${rw_mb}MiB/member (all conv=fsync — durable rows)"

    for idx in $(member_idxs); do snap "$idx" 0 "$rowdir"; done
    disk_wsnap "$rowdir" 0

    # ---- Phase W: the concurrent fresh fan-out ------------------------------
    local -a pids=()
    s9_dd "$rowdir/wall-w-m0" "$(mnt_of 0)/s9a-m0.dat" "$mb"
    pids+=("$!")
    for idx in "${cws[@]}"; do
        s9_dd "$rowdir/wall-w-m$idx" "$(mnt_of "$idx")/s9a-m$idx.dat" "$mb"
        pids+=("$!")
    done
    wait "${pids[@]}" || true
    for idx in 0 "${cws[@]}"; do
        read -r rc _ _ _ <"$rowdir/wall-w-m$idx"
        [ "$rc" = "0" ] || die "s9-fanout phase W: member m$idx's write FAILED (rc=$rc): $(head -2 "$rowdir/wall-w-m$idx.err")"
    done

    # ---- Phase R: the concurrent in-place REWRITE (displaced frees SHIP) ----
    pids=()
    s9_dd "$rowdir/wall-r-m0" "$(mnt_of 0)/s9a-m0.dat" "$rw_mb" fsync,notrunc
    pids+=("$!")
    for idx in "${cws[@]}"; do
        s9_dd "$rowdir/wall-r-m$idx" "$(mnt_of "$idx")/s9a-m$idx.dat" "$rw_mb" fsync,notrunc
        pids+=("$!")
    done
    wait "${pids[@]}" || true
    for idx in 0 "${cws[@]}"; do
        read -r rc _ _ _ <"$rowdir/wall-r-m$idx"
        [ "$rc" = "0" ] || die "s9-fanout phase R: member m$idx's rewrite FAILED (rc=$rc): $(head -2 "$rowdir/wall-r-m$idx.err")"
    done

    # In-flight settle (writeback + shipped frees + reclaim), then p1.
    sleep 3
    disk_wsnap "$rowdir" 1
    for idx in $(member_idxs); do snap "$idx" 1 "$rowdir"; done

    # ---- The row: engagement + amplification, gated -------------------------
    python3 - "$rowdir" "$mb" "$rw_mb" "$k" "${cws[@]}" <<'PYS9A' || die "s9-fanout: INVALID ROW"
import json, sys

rowdir, mb, rw_mb, k = sys.argv[1], int(sys.argv[2]), int(sys.argv[3]), int(sys.argv[4])
cws = sys.argv[5:]
BLOCK = 4 * 1024 * 1024

def flat(d, out=None, pfx=""):
    out = {} if out is None else out
    for kk, v in d.items():
        if isinstance(v, dict): flat(v, out, pfx + kk + ".")
        else: out[pfx + kk] = v
    return out

def load(i, ph):
    root = json.load(open(f"{rowdir}/m{i}_p{ph}.json"))
    return flat(root.get("metrics", root))

def wall(tag, i):
    rc, t0, t1, n = open(f"{rowdir}/wall-{tag}-m{i}").read().split()
    return float(t1) - float(t0), int(n)

bad = []
user_bytes = 0
print(f"== S9-a fan-out row: authority + {k} co-writer(s), phase-W {mb}MiB + phase-R {rw_mb}MiB per member, conv=fsync ==")
print(f"{'member':<8}{'role':<10}{'W_MBps':<9}{'R_MBps':<9}{'wt_MiB_d':<10}{'rw_MiB_d':<10}{'seeds_d':<8}{'pub_ship_d':<11}{'free_ship_d':<12}{'harvest_d':<10}{'lcr':<5}{'enospc':<7}")
members = [("0", "authority")] + [(i, "cowriter") for i in cws]
sum_pub_ship = 0
sum_free_ship = 0
for i, role in members:
    d0, d1 = load(i, 0), load(i, 1)
    dd = lambda kk: int(d1.get(kk, 0) or 0) - int(d0.get(kk, 0) or 0)
    ww, wn = wall("w", i)
    rw, rn = wall("r", i)
    user_bytes += (wn + rn) * 1024 * 1024
    wt_d = dd("write_through_bytes") // (1024 * 1024)
    rw_d = dd("rewrite_user_bytes") // (1024 * 1024)
    seeds = dd("overwrite_seed_materialized")
    pub_ship = dd("meta_ship_publish.shipped")
    free_ship = dd("meta_ship_publish.free_shipped_blocks")
    harv = dd("meta_ship_publish.harvest_shipped_blocks")
    lcr = int(d1.get("cowriter.local_commit_refusals", 0) or 0)
    enospc = dd("alloc_lane_enospc_refusals")
    print(f"m{i:<7}{role:<10}{wn/ww:<9.1f}{rn/rw:<9.1f}{wt_d:<10}{rw_d:<10}{seeds:<8}{pub_ship:<11}{free_ship:<12}{harv:<10}{lcr:<5}{enospc:<7}")
    # Engagement gates (charter: each co-writer's shipped publish/free
    # ledger deltas must account for its blocks). The written bytes ride
    # THREE vehicles on a buffered dd venue — complete-block write-through,
    # the rewrite vehicle (in-place overwrites of a mapped block), and
    # partial-coverage residue (seed-materialized overwrites, OOO-split
    # active blocks) — so the accounting gate is the SUM of the two byte
    # ledgers, and the seed count is a REPORTED column (the buffered
    # mid-block flush class; the rewrite program's own <=1.05 amp SLO
    # belongs to its direct sequential vehicle, not this venue).
    # Floor 50%: the residual rides the THIRD vehicle family (seeded
    # partial overwrites, OOO-split active-block flushes, extent riders)
    # whose byte ledgers are per-family; both counted runs measured
    # 58-66% on the two big vehicles, so 50% is a regression floor, not
    # an instrument-noise tripwire.
    total_mb = wn + rn
    if wt_d + rw_d < total_mb // 2:
        bad.append(f"m{i}: write_through {wt_d}MiB + rewrite {rw_d}MiB < 50% of the {total_mb}MiB written — the row's bytes are not accounted by the write vehicles")
    if role == "cowriter":
        rw_blocks = rn * 1024 * 1024 // BLOCK
        if pub_ship < 1:
            bad.append(f"m{i}: meta_ship_publish.shipped delta 0 — a co-writer's publishes must SHIP")
        if free_ship < rw_blocks // 2:
            bad.append(f"m{i}: free_shipped_blocks delta {free_ship} < half of the ~{rw_blocks} rewritten blocks — displaced frees are leaking")
        if lcr != 0:
            bad.append(f"m{i}: local_commit_refusals={lcr} (un-routed local commits — the S8-b falsifier)")
        sum_pub_ship += pub_ship
        sum_free_ship += free_ship
    if enospc != 0:
        bad.append(f"m{i}: alloc_lane_enospc_refusals moved ({enospc}) — a writer starved while the set has space")
    if dd("invariant_tripwires") != 0:
        bad.append(f"m{i}: invariant_tripwires moved")
    if dd("mem_budget_hard_backstops") != 0 or dd("parked_gate_timeouts") != 0:
        bad.append(f"m{i}: R5 columns moved")

# Aggregate engagement: co-writer ships account against the authority's
# serves (client-first snapshots; ±4/member instrument self-skew).
a0, a1 = load(0, 0), load(0, 1)
served_d = int(a1.get("meta_ship_publish.served", 0)) - int(a0.get("meta_ship_publish.served", 0))
free_served_d = int(a1.get("meta_ship_publish.free_served_blocks", 0)) - int(a0.get("meta_ship_publish.free_served_blocks", 0))
if served_d + 4 * (k + 1) < sum_pub_ship:
    bad.append(f"aggregate: authority served {served_d} publish verbs < co-writers' shipped {sum_pub_ship}")
if free_served_d + 4 * (k + 1) < sum_free_ship:
    bad.append(f"aggregate: authority free-served {free_served_d} < co-writers' free-shipped {sum_free_ship}")
for key, name in [("meta_ship_publish.refusals", "publish refusals"),
                  ("meta_ship_publish.owner_panics", "owner panics"),
                  ("alloc_lane_raise_refusals", "lane raise refusals"),
                  ("meta_ship_publish.harvest_refusals", "harvest refusals"),
                  ("block_free_reclaim_fence_halts", "reclaim fence halts")]:
    v = int(a1.get(key, 0) or 0)
    if v != 0:
        bad.append(f"authority: {name} = {v} (must stay 0)")

# ---- The write-amplification instrument (write_amp_rig discipline) -----
print("\n== device columns (per NAMESPACE — meta rides its own namespaces, so the data delta is exact;")
print("   co-located members share one merged head, so per-member device attribution is stats-side) ==")
p0 = {}
for line in open(f"{rowdir}/disk_p0.tsv"):
    cls, dev, wios, wsect = line.split()
    p0[dev] = (cls, int(wios), int(wsect))
data_wbytes = 0
meta_wbytes = 0
print(f"{'class':<6}{'dev':<12}{'wios_d':<9}{'MiB_d':<9}{'wareq_KiB':<10}")
for line in open(f"{rowdir}/disk_p1.tsv"):
    cls, dev, wios, wsect = line.split()
    dios = int(wios) - p0[dev][1]
    dbytes = (int(wsect) - p0[dev][2]) * 512
    if cls == "data": data_wbytes += dbytes
    else: meta_wbytes += dbytes
    wareq = dbytes / dios / 1024 if dios else 0.0
    print(f"{cls:<6}{dev:<12}{dios:<9}{dbytes/1048576:<9.1f}{wareq:<10.1f}")
amp = data_wbytes / user_bytes if user_bytes else 0.0
print(f"\nuser bytes {user_bytes/1048576:.0f}MiB, data-namespace device bytes {data_wbytes/1048576:.0f}MiB -> amp {amp:.3f}x (block size 4MiB; meta-namespace bytes {meta_wbytes/1048576:.0f}MiB, separate)")
for key in ["block_free_reclaim_queued", "block_free_reclaim_commands", "block_free_discards",
            "block_free_discard_bytes", "block_free_file_punches", "block_free_punch_bytes"]:
    print(f"  {key}_d = {int(a1.get(key, 0) or 0) - int(a0.get(key, 0) or 0)}")
# The amp SANITY band: on a buffered dd venue the honest ceiling includes
# the seed-materialized partial-overwrite class (a mid-block writeback
# flush materializes the 4 MiB seed before coverage completes — every
# seed is up to one extra block of device bytes), so the hard gate here is
# accounting sanity; the NUMBER is the row's published column and the
# evidence note carries its attribution. The rewrite program's <=1.05 SLO
# stays its own vehicle's gate.
if amp > 2.0:
    bad.append(f"amp {amp:.3f}x > 2.0 — beyond the seed-class ceiling on a sequential durable row")
if amp < 0.5:
    bad.append(f"amp {amp:.3f}x < 0.5 — the instrument is not accounting (wrong devices?)")

if bad:
    print("S9-a GATE FAILED:", file=sys.stderr)
    for b in bad:
        print(f"  {b}", file=sys.stderr)
    sys.exit(1)
print("\nS9-a GATE GREEN (engagement exact, amp columns present, tripwires flat)")
PYS9A

    # ---- The oracle ----------------------------------------------------------
    local out drift
    out="$("$SQZ" fsck "$(mnt_of 0)" 2>&1)" || die "s9-fanout: online fsck FAILED:
$out"
    echo "$out" >"$rowdir/fsck.out"
    echo "$out" | grep -q "findings: 0" || die "s9-fanout: fsck findings != 0:
$out"
    drift="$(stat_field 0 meta_kv_block_refs_drift)"
    [ "$drift" = "0" ] || die "s9-fanout: meta_kv_block_refs_drift=$drift (C8 oracle RED)"
    log "s9-fanout GREEN — fsck findings:0, drift=0 (row + snapshots in $rowdir). Evidence tier: measured-simulated (one box, co-located identities; the 15k extrapolation is the evidence note's arithmetic)"
}

leg_s9_failover() {
    require_cowriters 1
    local rowdir cws idx w_mnt
    rowdir="$STATE/rows/s9b-$(date +%s)"
    mkdir -p "$rowdir"
    mapfile -t cws < <(cowriter_idxs)
    w_mnt="$(mnt_of 0)"
    log "s9-failover: authority kill -9 mid-fan-out over ${#cws[@]} co-writer(s); gates = zero silent old-era survivors + zero acked-data loss + fsck/C8 clean; re-admission by remount (the rung-10 documented posture)"

    for idx in $(member_idxs); do snap "$idx" 0 "$rowdir"; done

    # ---- The ACKED corpus: per co-writer, fsync-durable, sha256-recorded ----
    local f n
    for idx in "${cws[@]}"; do
        mkdir -p "$(mnt_of "$idx")/s9b-m$idx"
        for n in 0 1 2 3 4 5 6 7; do
            f="$(mnt_of "$idx")/s9b-m$idx/acked-$n.dat"
            dd if=/dev/urandom of="$f" bs=1M count=4 conv=fsync status=none ||
                die "s9-failover: corpus write failed on m$idx"
        done
        (cd "$(mnt_of "$idx")/s9b-m$idx" && sha256sum acked-*.dat) >"$rowdir/sha-m$idx" ||
            die "s9-failover: corpus checksum failed on m$idx"
    done
    log "acked corpus written + fsynced (8 x 4MiB per co-writer, sha256 recorded)"

    # ---- The fan-out stream the kill lands in --------------------------------
    local -a pids=()
    for idx in "${cws[@]}"; do
        (exec dd if=/dev/zero of="$(mnt_of "$idx")/s9b-m$idx/stream.dat" bs=1M count=16384 conv=fsync status=none) 2>/dev/null &
        pids+=("$!")
    done
    sleep 5
    for idx in "${!pids[@]}"; do
        kill -0 "${pids[$idx]}" 2>/dev/null || die "s9-failover: a stream died before the kill"
    done

    # ---- Kill -9 the authority mid-stream ------------------------------------
    local w_pid t_kill t_up tries
    w_pid="$(awk -F'\t' '$1==0 {print $7}' "$MEMBERS")"
    "$MWFLEET" kill 0 --sig 9
    t_kill="$(date +%s)"
    umount -l "$w_mnt" 2>/dev/null || true
    wait_for_unmounted "$w_mnt"
    for ((tries = 0; tries < 120; tries++)); do
        kill -0 "$w_pid" 2>/dev/null || break
        sleep 0.5
    done
    kill -0 "$w_pid" 2>/dev/null && die "s9-failover: the killed authority (pid $w_pid) never exited"
    "$MWFLEET" mount 0 || die "s9-failover: successor authority remount FAILED"
    t_up="$(date +%s)"
    [ "$(stat_field 0 data_plane_fence_mode)" = "1" ] ||
        die "s9-failover: successor did not re-arm the WERO hold"
    log "successor authority up in $((t_up - t_kill))s (WERO re-armed)"

    # ---- The fence scan: zero silent old-era survivors (the E2 law) ---------
    local fenced=0 relearned silent=0
    for ((tries = 0; tries < 60; tries++)); do
        fenced=0
        for idx in "${cws[@]}"; do
            if ! cat "$(mnt_of "$idx")/.stats" >/dev/null 2>&1 ||
                [ "$(sfield0 "$idx" mount_posture)" != "co-writer" ]; then
                fenced=$((fenced + 1))
            fi
        done
        [ "$fenced" -ge "${#cws[@]}" ] && break
        sleep 1
    done
    for idx in "${cws[@]}"; do
        if cat "$(mnt_of "$idx")/.stats" >/dev/null 2>&1 &&
            [ "$(sfield0 "$idx" mount_posture)" = "co-writer" ]; then
            relearned="$(sfield0 "$idx" meta_ship.era_relearns)"
            if [ "$relearned" = "0" ]; then
                silent=$((silent + 1))
                warn "s9-failover: co-writer m$idx SURVIVED with no relearn and no fence — a silent old-era survivor"
            fi
        fi
    done
    for idx in "${!pids[@]}"; do
        kill -9 "${pids[$idx]}" 2>/dev/null || true
        wait "${pids[$idx]}" 2>/dev/null || true
    done
    [ "$silent" -eq 0 ] || die "s9-failover: $silent silent old-era survivor(s) — the era fence has a hole"
    log "fence scan: $fenced/${#cws[@]} co-writer(s) fenced (S7 wins the race; the rest relearned) — zero silent survivors"
    echo "remount_s=$((t_up - t_kill)) fenced=$fenced silent=$silent" >>"$rowdir/events.txt"

    # ---- Re-admission BY REMOUNT (the documented rung-10 posture) -----------
    for idx in "${cws[@]}"; do
        "$MWFLEET" unmount "$idx" || true
        "$MWFLEET" mount "$idx" ||
            die "s9-failover: co-writer m$idx could not re-admit under the successor era (its slot id is mount-point-stable — the roster still names it)"
    done
    log "all ${#cws[@]} co-writer(s) re-admitted under the successor era (by remount)"

    # ---- ZERO ACKED-DATA LOSS: the corpus verifies EVERYWHERE ----------------
    for idx in "${cws[@]}"; do
        (cd "$w_mnt/s9b-m$idx" && sha256sum -c --quiet "$rowdir/sha-m$idx") ||
            die "s9-failover: ACKED DATA LOSS — m$idx's fsynced corpus does not verify through the SUCCESSOR authority"
        (cd "$(mnt_of "$idx")/s9b-m$idx" && sha256sum -c --quiet "$rowdir/sha-m$idx") ||
            die "s9-failover: ACKED DATA LOSS — m$idx's fsynced corpus does not verify through the re-admitted co-writer"
    done
    log "acked corpus verified byte-identical through the successor AND every re-admitted co-writer (zero acked-data loss)"

    # ---- The oracle + tripwires ----------------------------------------------
    local out drift v
    out="$("$SQZ" fsck "$w_mnt" 2>&1)" || die "s9-failover: online fsck FAILED:
$out"
    echo "$out" >"$rowdir/fsck.out"
    echo "$out" | grep -q "findings: 0" || die "s9-failover: fsck findings != 0:
$out"
    drift="$(stat_field 0 meta_kv_block_refs_drift)"
    [ "$drift" = "0" ] || die "s9-failover: meta_kv_block_refs_drift=$drift (C8 oracle RED)"
    for v in meta_ship.owner_panics meta_ship_publish.refusals meta_ship_publish.owner_panics invariant_tripwires; do
        [ "$(stat_field 0 "$v")" = "0" ] || die "s9-failover: successor $v != 0"
    done
    for idx in "${cws[@]}"; do
        [ "$(stat_field "$idx" cowriter.local_commit_refusals)" = "0" ] ||
            die "s9-failover: re-admitted m$idx local_commit_refusals != 0"
    done
    for idx in $(member_idxs); do snap "$idx" 1 "$rowdir"; done
    log "s9-failover GREEN (events + snapshots + checksums in $rowdir). Automatic re-admission stays the DOCUMENTED deferral (ops.md 'Failure and re-admission')"
}

leg_s9_colocated_fence() {
    require_cowriters 1
    local rowdir cws victim w_mnt ttl renew_est
    rowdir="$STATE/rows/s9c-$(date +%s)"
    mkdir -p "$rowdir"
    mapfile -t cws < <(cowriter_idxs)
    victim="${S6_VICTIM:-${cws[0]}}"
    w_mnt="$(mnt_of 0)"
    ttl="$(stat_field 0 membership_lease_ttl_ms)"
    renew_est="$(owner_renew_est_s)"
    log "s9-colocated-fence: victim = co-writer m$victim (CO-LOCATED — shared PR host identity: the device CANNOT reject its DMA; the client-side epoch/poison gates must). T_owner=${ttl}ms"

    for idx in $(member_idxs); do snap "$idx" 0 "$rowdir"; done
    local evict0 exp0
    evict0="$(stat_field 0 membership_evictions)"
    exp0="$(stat_field 0 dlm_custody.dlm_revokes_expired)"

    # ---- Sustained write load on the victim, running when the freeze lands --
    (exec dd if=/dev/zero of="$(mnt_of "$victim")/s9c-load.dat" bs=1M count=16384 conv=fsync status=none) 2>/dev/null &
    local dd_pid=$!
    sleep 3
    kill -0 "$dd_pid" 2>/dev/null || die "s9-colocated-fence: victim load exited before the freeze"

    # ---- Freeze past the authority's TTL -------------------------------------
    "$MWFLEET" kill "$victim" --sig STOP
    log "victim m$victim SIGSTOPped (frozen daemon; its kernel keeps draining already-submitted DMA)"
    # The authority sweeps: custody lease expiry + membership eviction (the
    # dead-epoch mint) — both observable on the AUTHORITY's stats.
    local deadline=$((ttl / 1000 + 6 * renew_est + 90))
    wait_stat_ge 0 dlm_custody.dlm_revokes_expired $((exp0 + 1)) "$deadline" "authority custody sweep of the frozen victim" >/dev/null
    log "authority swept the victim's custody lease (dlm_revokes_expired moved)"
    wait_stat_ge 0 membership_evictions $((evict0 + 1)) "$deadline" "authority eviction of the frozen victim" >/dev/null
    log "authority evicted the frozen victim + minted its S7 dead epoch"

    # ---- Resume: the fencing story must be CLIENT-SIDE ------------------------
    "$MWFLEET" kill "$victim" --sig CONT
    log "victim m$victim resumed (SIGCONT) — its next renewal/deadline check must fail-stop it"
    # (wait_log_line is grep -F; this wants the ALTERNATION of the two
    # client-side fail-stop lines, so it polls -E inline.)
    local t0f
    t0f="$(date +%s)"
    while ! grep -Eq "SELF-FENCED|data-plane custody POISONED" "$STATE/m$victim.log"; do
        [ $(($(date +%s) - t0f)) -lt 120 ] ||
            die "victim client-side fail-stop: neither 'SELF-FENCED' nor 'data-plane custody POISONED' appeared in m$victim.log within 120s"
        sleep 1
    done
    kill -9 "$dd_pid" 2>/dev/null || true
    wait "$dd_pid" 2>/dev/null || true

    # THE CLASS ASSERTION: no device rejection existed or was needed. A
    # co-located identity can never meet a reservation conflict (its key IS
    # the holder's), so any such line would mean the leg's premise — and
    # the co-located adoption machinery — is broken.
    if grep -Eq "os error 52|Invalid exchange|reservation-conflict" "$STATE/m$victim.log"; then
        die "s9-colocated-fence: the victim's log carries a DEVICE-rejection line — a co-located identity met a reservation conflict, which the shared-key shape makes impossible (the WERO adoption machinery is broken)"
    fi
    log "victim fail-stopped CLIENT-SIDE with zero device-rejection lines (the co-located fencing story: epoch/poison gates only — cargo pin tests/mw_colocated_fence_tests.rs)"

    # Best-effort victim counter read (a fenced mount's .stats may be dead
    # — dead-until-remount; the split law is pinned in cargo either way).
    local vfence vepoch
    vfence="$(sfield0 "$victim" data_dma_fence_refusals)"
    vepoch="$(sfield0 "$victim" data_dma_epoch_refusals)"
    [ "$vepoch" -le "$vfence" ] ||
        die "s9-colocated-fence: victim epoch_refusals=$vepoch > fence_refusals=$vfence — the class split inverted"
    echo "victim fence_refusals=$vfence epoch_refusals=$vepoch (0/0 = stats died with the fence — the log lines above are the primary gate)" >>"$rowdir/events.txt"

    # ---- Blast radius + the oracle -------------------------------------------
    sleep 2
    for idx in $(member_idxs); do
        [ "$idx" = "$victim" ] && continue
        snap "$idx" 1 "$rowdir"
        python3 - "$rowdir" "$idx" <<'PYBLAST' || die "s9-colocated-fence: blast radius violated on m$idx"
import json, sys
rowdir, i = sys.argv[1], sys.argv[2]
def flat(d, out=None, pfx=""):
    out = {} if out is None else out
    for k, v in d.items():
        if isinstance(v, dict): flat(v, out, pfx + k + ".")
        else: out[pfx + k] = v
    return out
def load(ph):
    root = json.load(open(f"{rowdir}/m{i}_p{ph}.json"))
    return flat(root.get("metrics", root))
d0, d1 = load(0), load(1)
dd = lambda k: int(d1.get(k, 0) or 0) - int(d0.get(k, 0) or 0)
bad = []
if dd("data_dma_fence_refusals") != 0 or dd("data_dma_epoch_refusals") != 0:
    bad.append("fence/epoch refusals moved on a non-victim")
if dd("invariant_tripwires") != 0:
    bad.append("invariant_tripwires moved")
if bad:
    print("\n".join(f"m{i}: {b}" for b in bad), file=sys.stderr)
    sys.exit(1)
PYBLAST
    done
    local out drift
    out="$("$SQZ" fsck "$w_mnt" 2>&1)" || die "s9-colocated-fence: online fsck FAILED:
$out"
    echo "$out" >"$rowdir/fsck.out"
    echo "$out" | grep -q "findings: 0" || die "s9-colocated-fence: fsck findings != 0:
$out"
    drift="$(stat_field 0 meta_kv_block_refs_drift)"
    [ "$drift" = "0" ] || die "s9-colocated-fence: meta_kv_block_refs_drift=$drift (C8 oracle RED)"

    # ---- Victim re-admission (by remount — the documented posture) -----------
    "$MWFLEET" unmount "$victim" || true
    "$MWFLEET" mount "$victim" ||
        die "s9-colocated-fence: the fenced victim could not re-admit by remount"
    [ "$(stat_field "$victim" mount_posture)" = "co-writer" ] ||
        die "s9-colocated-fence: re-admitted victim posture != co-writer"
    log "s9-colocated-fence GREEN (victim fenced client-side, device silent, blast radius = victim, fsck clean, re-admitted; rows in $rowdir)"
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

leg_s11_range() {
    require_cowriters 2
    # Rung 15 ships DARK (SQUEEZEFS_RANGE_CUSTODY default-off; the
    # default-on revisit is rung 18's, behind this gate staying green):
    # this leg REQUIRES an explicitly armed fleet, and it is the
    # concurrent same-ino publish composition's acceptance surface. The
    # composition LANDED: rung 17 shipped the machinery (chain-onto-head,
    # batch-prior compaction, custody-scoped Puts) and the
    # zeros-interleave fix armed it in production (the missing
    # range-geometry install —
    # .benchmarks/2026-08-17-s11-zeros-interleave-fix.md), flipping this
    # gate GREEN ×3 from zero. Any red here is a REGRESSION now, never a
    # standing adjudication. Rung 16's live gate HERE is the
    # zero-misfire columns (prs_d/ors_d — the clause must stay silent on
    # block-aligned custody, §9.5's aligned-row law) + the ledger-export
    # check; the clause's FIRING venue is the demotion-barrier row
    # (the acquire algebra keeps every live grant edge block-aligned —
    # outward-rounded desired, aligned-wall clipping, required-conflict
    # serialization — so no product write can reach a range-shared block
    # on this leg's shape; the firing half is pinned in-process against
    # directly-minted sub-block grants and runs live in s11-subblock).
    [ "${RANGE_CUSTODY:-0}" = "1" ] ||
        die "s11-range needs a range-custody-ARMED fleet: sudo SQZ_MWFLEET_RANGE_CUSTODY=1 tests/mw_fleet.sh create N=1 --cowriters=2"
    local rowdir cws m1 m2 idx w_mnt
    rowdir="$STATE/rows/s11r-$(date +%s)"
    mkdir -p "$rowdir"
    mapfile -t cws < <(cowriter_idxs)
    m1="${cws[0]}"
    m2="${cws[1]}"
    w_mnt="$(mnt_of 0)"

    # Sizing from the LANE SHARE (never a free constant — the s9-fanout
    # law): each half at 20% of a lane, capped, floor 64 MiB.
    local total_bytes=0 p w
    local IFS_SAVE="$IFS"
    IFS=,
    for p in $FORMAT_DATA_PATHS; do
        total_bytes=$((total_bytes + $(blockdev --getsize64 "$p")))
    done
    IFS="$IFS_SAVE"
    w="$(stat_field 0 alloc_lane_writers)"
    [ -n "$w" ] && [ "$w" -ge 2 ] ||
        die "s11-range: alloc_lane_writers=$w — a --cowriters fleet must run an engaged allocation partition"
    local lane_mb=$((total_bytes / w / 1024 / 1024))
    local half_mb=$((lane_mb * 20 / 100))
    [ "$half_mb" -le "$S11_MB_CAP" ] || half_mb="$S11_MB_CAP"
    [ "$half_mb" -ge 64 ] || die "s11-range: derived half ${half_mb}MiB < 64MiB — grow SQZ_MWFLEET_OSS_GB"
    local half_bytes=$((half_mb * 1024 * 1024))
    log "s11-range: ONE $((2 * half_mb))MiB striped file, co-writer m$m1 takes [0,${half_mb}M), m$m2 takes [${half_mb}M,$((2 * half_mb))M) — disjoint range custody, concurrent durable writes (KD-MW-7, the first sub-file multi-writer rows)"

    # The authority creates the shared file (striped by size; the halves
    # are written by the CO-WRITERS only).
    truncate -s $((2 * half_bytes)) "$w_mnt/s11-range.dat" ||
        die "s11-range: authority could not create the shared file"

    # Deterministic sources (verify needs bytes, not /dev/urandom-in-place).
    head -c "$half_bytes" /dev/urandom >"$rowdir/src-m$m1"
    head -c "$half_bytes" /dev/urandom >"$rowdir/src-m$m2"

    for idx in $(member_idxs); do snap "$idx" 0 "$rowdir"; done

    # ---- Phase 1: the CONCURRENT disjoint-half writes ------------------------
    local -a pids=()
    (
        t0="$(date +%s.%N)"
        rc=0
        dd if="$rowdir/src-m$m1" of="$(mnt_of "$m1")/s11-range.dat" bs=4M \
            seek=0 conv=fsync,notrunc status=none 2>"$rowdir/w-m$m1.err" || rc=$?
        echo "$rc $t0 $(date +%s.%N)" >"$rowdir/w-m$m1"
    ) &
    pids+=("$!")
    (
        t0="$(date +%s.%N)"
        rc=0
        dd if="$rowdir/src-m$m2" of="$(mnt_of "$m2")/s11-range.dat" bs=4M \
            seek=$((half_mb / 4)) conv=fsync,notrunc status=none 2>"$rowdir/w-m$m2.err" || rc=$?
        echo "$rc $t0 $(date +%s.%N)" >"$rowdir/w-m$m2"
    ) &
    pids+=("$!")
    wait "${pids[@]}" || true
    local rc t0 t1
    for idx in "$m1" "$m2"; do
        read -r rc t0 t1 <"$rowdir/w-m$idx"
        [ "$rc" = "0" ] || die "s11-range: m$idx's half FAILED (rc=$rc): $(head -3 "$rowdir/w-m$idx.err")"
        log "m$idx wrote its ${half_mb}MiB half in $(python3 -c "print(f'{$t1-$t0:.1f}')")s (concurrent, conv=fsync)"
    done
    sleep 3 # publish/writeback settle
    for idx in $(member_idxs); do snap "$idx" 1 "$rowdir"; done

    # ---- The rung-15 CUSTODY row: engagement + the Issue-19 column, gated ----
    python3 - "$rowdir" "$m1" "$m2" <<'PYS11' || die "s11-range: INVALID ROW"
import json, sys
rowdir, m1, m2 = sys.argv[1], sys.argv[2], sys.argv[3]
def flat(d, out=None, pfx=""):
    out = {} if out is None else out
    for k, v in d.items():
        if isinstance(v, dict): flat(v, out, pfx + k + ".")
        else: out[pfx + k] = v
    return out
def load(i, ph):
    root = json.load(open(f"{rowdir}/m{i}_p{ph}.json"))
    return flat(root.get("metrics", root))
bad = []
print("== S11 rung-15 row: 2 co-writers, disjoint range custody of ONE file ==")
print(f"{'member':<8}{'rng_acq_d':<11}{'rng_ext_d':<11}{'pub_ship_d':<12}{'lcr':<5}"
      f"{'prs_d':<7}{'ors_d':<7}")
for i in (m1, m2):
    d0, d1 = load(i, 0), load(i, 1)
    dd = lambda k: int(d1.get(k, 0) or 0) - int(d0.get(k, 0) or 0)
    acq = dd("dlm_custody.dlm_custody_range_acquires")
    ext = dd("dlm_custody.dlm_custody_range_extensions")
    pub = dd("meta_ship_publish.shipped")
    lcr = int(d1.get("cowriter.local_commit_refusals", 0) or 0)
    # Rung 16 (KD-MW-12): the two fast-path range-clause ledgers. On a
    # BLOCK-ALIGNED ranged row nothing shares a block (§9.5's MPI-IO
    # law), so both must stay 0 — movement here is the finding-#2 class
    # (own covering custody refusing itself = the clause misfiring on
    # every own-covered write of the stream).
    prs = dd("patch_ineligible_range_shared")
    ors = dd("overlay_ineligible_range_shared")
    print(f"m{i:<7}{acq:<11}{ext:<11}{pub:<12}{lcr:<5}{prs:<7}{ors:<7}")
    if acq < 1:
        bad.append(f"m{i}: dlm_custody_range_acquires delta {acq} — the ranged write "
                   "path did NOT engage (silent whole-file fallback = the row is a lie)")
    if pub < 1:
        bad.append(f"m{i}: meta_ship_publish.shipped delta 0 — a co-writer's publishes must SHIP")
    if lcr != 0:
        bad.append(f"m{i}: local_commit_refusals={lcr}")
    if "overlay_ineligible_range_shared" not in d1:
        bad.append(f"m{i}: overlay_ineligible_range_shared missing from the stats "
                   "inode — the rung-16 B4 clause ledger is not exported")
    if prs != 0 or ors != 0:
        bad.append(f"m{i}: range-clause ledgers moved on a BLOCK-ALIGNED ranged row "
                   f"(patch_ineligible_range_shared d={prs}, "
                   f"overlay_ineligible_range_shared d={ors}) — own covering custody "
                   "refused itself (rung 15 finding #2's class) or a block is "
                   "unexpectedly shared")
    if dd("invariant_tripwires") != 0:
        bad.append(f"m{i}: invariant_tripwires moved")
    if dd("mem_budget_hard_backstops") != 0 or dd("parked_gate_timeouts") != 0:
        bad.append(f"m{i}: R5 columns moved")
a0, a1 = load("0", 0), load("0", 1)
ad = lambda k: int(a1.get(k, 0) or 0) - int(a0.get(k, 0) or 0)
grants = ad("range_custody.range_custody_grants")
exts = ad("range_custody.range_custody_extensions")
trims = ad("range_custody.range_custody_desired_trims")
caps = ad("range_custody.range_custody_cap_refusals")
confl = ad("range_custody.range_custody_conflicts")
tbl = int(a1.get("range_custody.dlm_grant_table_bytes", 0) or 0)
print(f"authority: grants_d={grants} extensions_d={exts} desired_trims_d={trims} "
      f"cap_refusals_d={caps} conflicts_d={confl} table_bytes={tbl}")
if grants < 2:
    bad.append(f"authority: range_custody_grants delta {grants} < 2 — the two holders' "
               "grants must be issued (and their streams coalesce into them)")
if caps != 0:
    bad.append(f"authority: range_custody_cap_refusals moved ({caps}) on a within-budget "
               "shape — the Issue-19 class: a constant refusing the workload S11 exists for")
if confl != 0:
    bad.append(f"authority: range_custody_conflicts moved ({confl}) — DISJOINT halves "
               "under the coalescing algebra must never contend")
for key, name in [("meta_ship.owner_panics", "owner panics"),
                  ("meta_ship_publish.owner_panics", "publish owner panics")]:
    v = int(a1.get(key, 0) or 0)
    if v != 0:
        bad.append(f"authority: {name} = {v} (must stay 0)")
if bad:
    print("S11 CUSTODY GATE FAILED:", file=sys.stderr)
    for b in bad:
        print(f"  {b}", file=sys.stderr)
    sys.exit(1)
print("S11 custody GATE GREEN (ranged engagement exact, Issue-19 column 0, zero conflicts, "
      "ships accounted, rung-16 range-clause ledgers exported + silent on aligned custody)")
PYS11

    # ---- Own-mount verification (each writer serves its own acked half) ------
    local sha_src sha_got
    for idx in "$m1" "$m2"; do
        local skip=0
        [ "$idx" = "$m2" ] && skip=$((half_mb / 4))
        sha_src="$(sha256sum "$rowdir/src-m$idx" | cut -d' ' -f1)"
        sha_got="$(dd if="$(mnt_of "$idx")/s11-range.dat" bs=4M skip="$skip" count=$((half_mb / 4)) status=none | sha256sum | cut -d' ' -f1)"
        [ "$sha_src" = "$sha_got" ] ||
            die "s11-range: m$idx's own half does not verify through its own mount ($sha_src != $sha_got)"
    done
    log "own-mount half verification green (both writers)"

    # ---- The kill arm: a holder's ranges die with its era --------------------
    local exp0 ttl deadline
    exp0="$(stat_field 0 dlm_custody.dlm_revokes_expired)"
    ttl="$(stat_field 0 membership_lease_ttl_ms)"
    deadline=$((ttl / 1000 + 90))
    # The victim rewrites its half in an endless loop (a 64MiB half
    # finishes in well under a second on this venue — a single pass would
    # complete before the kill lands); the survivor's single re-write is
    # the acked-durability side.
    (
        exec bash -c 'while :; do
            dd if=/dev/zero of="$1" bs=4M seek=0 count="$2" conv=fsync,notrunc status=none || exit
        done' _ "$(mnt_of "$m1")/s11-range.dat" "$((half_mb / 4))"
    ) 2>"$rowdir/k-m$m1.err" &
    local dd1=$!
    (
        rc=0
        dd if="$rowdir/src-m$m2" of="$(mnt_of "$m2")/s11-range.dat" bs=4M \
            seek=$((half_mb / 4)) conv=fsync,notrunc status=none 2>"$rowdir/k-m$m2.err" || rc=$?
        echo "$rc" >"$rowdir/k-m$m2"
    ) &
    local dd2=$!
    sleep 2
    kill -0 "$dd1" 2>/dev/null || die "s11-range: the victim's rewrite exited before the kill"
    "$MWFLEET" kill "$m1" --sig 9
    log "victim m$m1 killed -9 MID-REWRITE (its dd dies with the mount — expected; the survivor must not notice)"
    umount -l "$(mnt_of "$m1")" 2>/dev/null || true
    pkill -9 -P "$dd1" 2>/dev/null || true
    kill -9 "$dd1" 2>/dev/null || true
    wait "$dd1" 2>/dev/null || true
    # The survivor's stream completes green.
    wait "$dd2" || true
    read -r rc <"$rowdir/k-m$m2"
    [ "$rc" = "0" ] || die "s11-range: the SURVIVOR m$m2's rewrite FAILED (rc=$rc) after the peer's kill: $(head -3 "$rowdir/k-m$m2.err")"
    log "survivor m$m2's rewrite completed green through the peer's death"
    # The authority sweeps the dead holder: its lease expires and its
    # ranges die with its era.
    wait_stat_ge 0 dlm_custody.dlm_revokes_expired $((exp0 + 1)) "$deadline" \
        "authority custody sweep of the killed range holder" >/dev/null
    log "authority swept the victim's lease (dlm_revokes_expired moved) — its ranges died with its era"
    # Convergence: once the survivor's releases travel (renewal-cadence
    # drain), NO range custody remains live on the authority.
    local tries active
    for ((tries = 0; tries < 60; tries++)); do
        active="$(stat_field 0 range_custody.range_custody_active)"
        [ "$active" = "0" ] && break
        sleep 1
    done
    [ "$active" = "0" ] ||
        die "s11-range: range_custody_active=$active never converged to 0 — a dead/released holder's ranges are stranded in the table"
    log "range_custody_active converged to 0 (dead holder's ranges retired, survivor's released)"

    # ---- Victim re-admission by remount (grace-bounded retry) ---------------
    local admitted=0
    for ((tries = 0; tries < 15; tries++)); do
        if "$MWFLEET" mount "$m1" >/dev/null 2>&1; then
            admitted=1
            break
        fi
        sleep 10
    done
    [ "$admitted" = "1" ] || die "s11-range: victim m$m1 could not re-admit by remount within the grace ladder"
    log "victim m$m1 re-admitted by remount"

    # ==== THE COMPOSITION GATE (GREEN — flipped ×3 from zero,
    # ==== 2026-08-17) =========================================================
    # The merged two-writer layout read cold + the C8 oracle. Rung 17's
    # chain-onto-head machinery flipped the BYTE half (shipped layout
    # merges CHAIN ONTO THE DURABLE HEAD — owner-restamped claim +
    # owner-minted link version, the versioned DeltaUsed reply, publish
    # schema 6; the chain-cap full save stands down for chained targets;
    # batch-prior pass mates never compact over each other), and the
    # zeros-interleave fix flipped the C8 half: shipped full Puts are
    # CUSTODY-SCOPED **and the scoping is armed in production** (the
    # range-geometry source `arm_multi_writer` was missing — a range
    # holder's Put now applies scoped or REFUSES, never verbatim;
    # .benchmarks/2026-08-17-s11-zeros-interleave-fix.md). Any failure
    # below is a REGRESSION.
    local comp_bad=""
    "$MWFLEET" unmount 0 >/dev/null 2>&1 || true
    "$MWFLEET" mount 0 >/dev/null 2>&1 || die "s11-range: authority remount failed"
    sha_src="$(cat "$rowdir/src-m$m1" "$rowdir/src-m$m2" | sha256sum | cut -d' ' -f1)"
    # (phase-1 sources: the kill arm rewrote m1's half with zeros and
    # re-wrote m2's half from its source, so the composed expectation is
    # zeros||src-m2.)
    sha_src="$( (head -c "$half_bytes" /dev/zero; cat "$rowdir/src-m$m2") | sha256sum | cut -d' ' -f1)"
    sha_got="$(sha256sum "$w_mnt/s11-range.dat" 2>/dev/null | cut -d' ' -f1)"
    [ "$sha_src" = "$sha_got" ] ||
        comp_bad="cold-authority whole-file verify: $sha_src != ${sha_got:-<read failed>}"
    local out drift
    if out="$("$SQZ" fsck "$w_mnt" 2>&1)"; then
        echo "$out" >"$rowdir/fsck.out"
        echo "$out" | grep -q "findings: 0" || comp_bad="${comp_bad:+$comp_bad; }fsck findings != 0"
    else
        echo "$out" >"$rowdir/fsck.out"
        comp_bad="${comp_bad:+$comp_bad; }fsck FAILED"
    fi
    drift="$(stat_field 0 meta_kv_block_refs_drift)"
    [ "$drift" = "0" ] || comp_bad="${comp_bad:+$comp_bad; }meta_kv_block_refs_drift=$drift (C8)"

    # ---- Re-admit the co-writers the authority remount fenced ---------------
    # ALL of them, not just the leg's participants: an authority bounce
    # SELF-FENCES every co-writer's membership/custody lease (the S7
    # designed posture — "a fenced holder is dead until remount"), so a
    # wider fleet's bystanders wedge unless re-admitted here (found live
    # on the rung-18 8-co-writer fleet: m52..m57 fenced at leg end).
    for idx in $(cowriter_idxs); do
        "$MWFLEET" unmount "$idx" >/dev/null 2>&1 || true
        admitted=0
        for ((tries = 0; tries < 15; tries++)); do
            if "$MWFLEET" mount "$idx" >/dev/null 2>&1; then
                admitted=1
                break
            fi
            sleep 10
        done
        [ "$admitted" = "1" ] || die "s11-range: co-writer m$idx could not re-admit after the cold-verify remount"
    done

    # ---- Zero residue --------------------------------------------------------
    rm -f "$w_mnt/s11-range.dat" || die "s11-range: could not remove the leg's file"
    rm -f "$rowdir/src-m$m1" "$rowdir/src-m$m2"
    for idx in $(member_idxs); do
        cat "$(mnt_of "$idx")/.stats" >/dev/null 2>&1 ||
            die "s11-range: member m$idx is not healthy at leg end"
    done
    for idx in $(member_idxs); do snap "$idx" 2 "$rowdir"; done

    if [ -n "$comp_bad" ]; then
        die "s11-range COMPOSITION GATE FAILED — a REGRESSION (this gate flipped GREEN x3 from zero on 2026-08-17): $comp_bad.
The composition is LANDED machinery: chain-onto-head shipped merges + batch-prior compaction + custody-scoped shipped full Puts (rung 17, .benchmarks/2026-08-17-s11-authority-assembler.md) with the scoping ARMED in production by the range-geometry install and the scoped-or-refused Put law (.benchmarks/2026-08-17-s11-zeros-interleave-fix.md). A cold-verify mismatch, fsck finding, or C8 drift here reintroduces a fixed conviction — stop and read those two notes. Lineage: .benchmarks/2026-08-17-s11-range-wire.md + .benchmarks/2026-08-17-s11-b4-clause.md."
    fi
    log "s11-range GREEN — the first sub-file multi-writer rows, composition included (snapshots + fsck in $rowdir). Evidence tier: measured-simulated (one box, co-located members)"
}
# Rung 17 (KD-MW-8, §9.5's "sub-block exception row" — PRICED, never
# gating): TWO co-writers stream 4 KiB records into the two halves of ONE
# 4 MiB block. The second holder's first ask fires the DEMOTION BARRIER
# (grant withheld until the incumbent's renewal-carried notice is acked);
# the block then belongs to the AUTHORITY's assembler and BOTH holders'
# writes ship as WriteExtent records (the symmetric law). Price columns:
# extent ship rate, the authority daemon's merge CPU (utime+stime delta
# per served extent), retention residency (→ 0 at quiesce). The full
# 4K-ALTERNATING anti-shape (non-adjacent slots) meets the §9.2 geometry
# cap by design — the split-halves interleave is the coalescible face;
# the alternating face is rung 18's bounds row.
leg_s11_subblock() {
    require_cowriters 2
    [ "${RANGE_CUSTODY:-0}" = "1" ] ||
        die "s11-subblock needs a range-custody-ARMED fleet: sudo SQZ_MWFLEET_RANGE_CUSTODY=1 tests/mw_fleet.sh create N=1 --cowriters=2"
    local rowdir cws m1 m2 idx w_mnt
    rowdir="$STATE/rows/s11sb-$(date +%s)"
    mkdir -p "$rowdir"
    mapfile -t cws < <(cowriter_idxs)
    m1="${cws[0]}"
    m2="${cws[1]}"
    w_mnt="$(mnt_of 0)"
    local rec=4096 half=$((2 * 1024 * 1024))
    log "s11-subblock: ONE 8MiB striped file; m$m1 streams ${rec}B records over [0,2M), m$m2 over [2M,4M) — sub-block sharing of block 0, the demotion barrier's live venue"

    truncate -s $((8 * 1024 * 1024)) "$w_mnt/s11-subblock.dat" ||
        die "s11-subblock: authority could not create the shared file"
    local m0_pid
    m0_pid="$(pgrep -f "squeezefs.*mount.*$w_mnt" | head -1)"
    [ -n "$m0_pid" ] || die "s11-subblock: no authority daemon pid"
    cpu_of() { awk '{print $14 + $15}' "/proc/$1/stat"; }

    for idx in $(member_idxs); do snap "$idx" 0 "$rowdir"; done
    local cpu0 cpu1 t_m2
    cpu0="$(cpu_of "$m0_pid")"

    # The INCUMBENT's LIVE stream (the §9.3 shape verbatim: "holder A
    # direct-DMAing a block-aligned grant; B acquires an overlapping-block
    # range MID-STREAM"): m1 holds ONE OPEN FD and loops over its half —
    # its grant stays live across the whole row (a per-phase close would
    # RELEASE the grant and dissolve the sharing before it exists, which
    # is exactly what this leg's first cut proved). Final pass pattern 51.
    (
        python3 - "$(mnt_of "$m1")/s11-subblock.dat" 0 "$rec" "$half" >"$rowdir/a-passes" 2>"$rowdir/a.err"
        echo $? >"$rowdir/a-rc"
    ) <<'PYA' &
import os, sys, time
path, start, rec, half = sys.argv[1], int(sys.argv[2]), int(sys.argv[3]), int(sys.argv[4])
# The INCUMBENT stays BUFFERED (per-pass fsync): pre-demotion it holds
# WHOLE-BLOCK custody, and a co-writer's small overwrite rides
# CoW-rewrite (W1 is authority-only BY DECISION) — an O_SYNC record
# there is a 4 MiB CoW per 4 KiB write, which exhausts the allocation
# lane inside ONE pass (measured live: 512 records = the whole
# 512-block lane share). Post-demotion its fsync slices ship as
# max_write-coalesced chunked extents — the coalesced face of the
# price table; the per-record face is phase B's O_SYNC stream.
fd = os.open(path, os.O_WRONLY)
deadline = time.monotonic() + 9.0
n = 0
while True:
    n += 1
    last = time.monotonic() >= deadline
    pat = 51 if last else 16 + (n % 8)
    buf = bytes([pat]) * rec
    t0 = time.monotonic()
    off = start
    while off < start + half:
        os.pwrite(fd, buf, off)
        off += rec
    os.fsync(fd)
    print(f"pass {n} pat {pat} {time.monotonic() - t0:.3f}s", flush=True)
    if last:
        break
os.close(fd)
PYA
    local a_pid=$!
    sleep 1
    # B acquires MID-STREAM: the barrier parks its first record's grant
    # until A's renewal-carried notice is acked; every record then ships.
    t_m2="$(python3 - "$(mnt_of "$m2")/s11-subblock.dat" "$half" 34 "$rec" "$half" <<'PYB'
import os, sys, time
path, start, pat, rec, half = sys.argv[1], int(sys.argv[2]), int(sys.argv[3]), int(sys.argv[4]), int(sys.argv[5])
fd = os.open(path, os.O_WRONLY | os.O_SYNC)
buf = bytes([pat]) * rec
t0 = time.monotonic()
off = start
while off < start + half:
    os.pwrite(fd, buf, off)
    off += rec
os.fsync(fd)
print(f"{time.monotonic() - t0:.3f}")
os.close(fd)
PYB
)" || die "s11-subblock: m$m2's through-demotion stream failed"
    log "phase B (m$m2 through the demotion + extent ship): ${t_m2}s for $((half / rec)) records"
    wait "$a_pid" || true
    [ "$(cat "$rowdir/a-rc" 2>/dev/null)" = "0" ] ||
        die "s11-subblock: m$m1's live stream failed: $(head -3 "$rowdir/a.err" 2>/dev/null)"
    cpu1="$(cpu_of "$m0_pid")"
    log "phase A/C (m$m1's live stream, pre→post demotion): $(head -1 "$rowdir/a-passes") … $(tail -1 "$rowdir/a-passes")"
    for idx in $(member_idxs); do snap "$idx" 3 "$rowdir"; done

    # ---- Engagement + the demotion ledger, gated -----------------------------
    python3 - "$rowdir" "$m1" "$m2" $((half / rec)) <<'PYSB' || die "s11-subblock: INVALID ROW"
import json, sys
rowdir, m1, m2, recs = sys.argv[1], sys.argv[2], sys.argv[3], int(sys.argv[4])
def flat(d, out=None, pfx=""):
    out = {} if out is None else out
    for k, v in d.items():
        if isinstance(v, dict): flat(v, out, pfx + k + ".")
        else: out[pfx + k] = v
    return out
def load(i, ph):
    root = json.load(open(f"{rowdir}/m{i}_p{ph}.json"))
    return flat(root.get("metrics", root))
bad = []
d = lambda i, a, b, k: int(load(i, b).get(k, 0) or 0) - int(load(i, a).get(k, 0) or 0)
m2_ship = d(m2, 0, 3, "meta_ship_publish.extent_shipped")
m1_ship = d(m1, 0, 3, "meta_ship_publish.extent_shipped")
served = d("0", 0, 3, "meta_ship_publish.extent_served")
flushes = d("0", 0, 3, "meta_ship_publish.extent_flush_forces") \
    + d(m1, 0, 3, "meta_ship_publish.extent_flush_forces") \
    + d(m2, 0, 3, "meta_ship_publish.extent_flush_forces")
dem = d("0", 0, 3, "range_custody.range_custody_demotions")
acks = d("0", 0, 3, "range_custody.range_custody_demotion_acks")
fres = d("0", 0, 3, "range_custody.range_custody_demotion_fence_resolves")
fpub = d("0", 0, 3, "range_custody.range_custody_demotion_fenced_publishes")
prs = d("0", 0, 3, "patch_ineligible_range_shared")
ors = d("0", 0, 3, "overlay_ineligible_range_shared")
ret_end = int(load(m1, 3).get("meta_ship_publish.extent_retained_bytes", 0) or 0) \
    + int(load(m2, 3).get("meta_ship_publish.extent_retained_bytes", 0) or 0)
print("== S11 rung-17 sub-block row (PRICED, never gating) ==")
print(f"m{m2} extent_shipped d={m2_ship} (phase B)  m{m1} d={m1_ship} (phase C)  "
      f"authority served d={served}  flush_forces={flushes}")
print(f"demotions d={dem} acks d={acks} fence_resolves d={fres} fenced_publishes d={fpub}")
print(f"authority clause ledgers: patch_ineligible_range_shared d={prs} "
      f"overlay_ineligible_range_shared d={ors}")
print(f"retained_bytes at quiesce: {ret_end}")
if m2_ship < recs:
    bad.append(f"m{m2}: extent_shipped d={m2_ship} < {recs} — phase B's records did not ship "
               "(silent local landing = the row is a lie)")
if m1_ship < 2:
    bad.append(f"m{m1}: extent_shipped d={m1_ship} < 2 — the INCUMBENT's post-demotion "
               "writes must ship too (KD-MW-8's symmetric law; buffered slices ship "
               "as max_write-coalesced chunked extents, so the floor is chunks, "
               "not records)")
if served < m2_ship + m1_ship:
    bad.append(f"authority: extent_served d={served} < shipped {m2_ship + m1_ship} — "
               "shipped ≡ served is the engagement law")
if dem < 1: bad.append("no demotion fired — the barrier venue did not engage")
if dem != acks + fres:
    bad.append(f"demotion ledger does not close: {dem} != {acks} + {fres}")
if fres != 0:
    bad.append(f"fence_resolves={fres} on a healthy row (the clean path is acks)")
if fpub != 0:
    bad.append(f"range_custody_demotion_fenced_publishes={fpub} (must stay 0 on the clean path)")
if prs < 1 or ors < 1:
    bad.append(f"the rung-16 clause ledgers did not move on the authority's assembly "
               f"(prs={prs}, ors={ors}) — the demotion row is their live firing venue")
if ret_end != 0:
    bad.append(f"extent_retained_bytes={ret_end} after fsync — retention did not release "
               "(the covering-version law broke)")
if bad:
    print("S11 SUB-BLOCK GATE FAILED:", file=sys.stderr)
    for b in bad: print(f"  {b}", file=sys.stderr)
    sys.exit(1)
print("S11 sub-block engagement GREEN (both holders shipped, ledger closed, clauses fired, retention quiesced)")
PYSB

    # ---- The price table ------------------------------------------------------
    local cpu_d shipped_total
    cpu_d=$((cpu1 - cpu0))
    shipped_total="$(python3 - "$rowdir" "$m1" "$m2" <<'PYT'
import json, sys
rowdir, m1, m2 = sys.argv[1], sys.argv[2], sys.argv[3]
def flat(d, out=None, pfx=""):
    out = {} if out is None else out
    for k, v in d.items():
        if isinstance(v, dict): flat(v, out, pfx + k + ".")
        else: out[pfx + k] = v
    return out
def load(i, ph):
    root = json.load(open(f"{rowdir}/m{i}_p{ph}.json"))
    return flat(root.get("metrics", root))
t = 0
for i in (m1, m2):
    t += int(load(i, 3).get("meta_ship_publish.extent_shipped", 0) or 0) - \
         int(load(i, 0).get("meta_ship_publish.extent_shipped", 0) or 0)
print(t)
PYT
)"
    log "PRICE (measured-simulated, tcp devsub, ${rec}B records, $((half / rec)) records/pass):"
    log "  incumbent per-pass walls (pre → post demotion): see $rowdir/a-passes; B through-demotion: ${t_m2}s for $((half / rec)) records"
    log "  extents shipped (both holders): $shipped_total; authority daemon CPU over the row: $cpu_d ticks ($(python3 -c "print(f'{$cpu_d/100:.2f}')")s)"
    log "  authority CPU/extent: $(python3 -c "print(f'{$cpu_d * 10_000 / max(1, $shipped_total):.0f}')") µs (upper bound — serve+merge+fold+publish inclusive)"

    # ---- Cold verify + oracle -------------------------------------------------
    local comp_bad=""
    "$MWFLEET" unmount 0 >/dev/null 2>&1 || true
    "$MWFLEET" mount 0 >/dev/null 2>&1 || die "s11-subblock: authority remount failed"
    python3 - "$w_mnt/s11-subblock.dat" "$half" <<'PYV' || comp_bad="cold-authority byte verify failed"
import sys
path, half = sys.argv[1], int(sys.argv[2])
data = open(path, "rb").read()
assert len(data) == 8 * 1024 * 1024, f"size {len(data)}"
assert data[:half] == bytes([51]) * half, "m1's phase-C half diverged"
assert data[half:2*half] == bytes([34]) * half, "m2's half diverged"
assert data[2*half:] == bytes(len(data) - 2*half), "the untouched tail diverged"
PYV
    local out drift
    if out="$("$SQZ" fsck "$w_mnt" 2>&1)"; then
        echo "$out" >"$rowdir/fsck.out"
        echo "$out" | grep -q "findings: 0" || comp_bad="${comp_bad:+$comp_bad; }fsck findings != 0"
    else
        echo "$out" >"$rowdir/fsck.out"
        comp_bad="${comp_bad:+$comp_bad; }fsck FAILED"
    fi
    drift="$(stat_field 0 meta_kv_block_refs_drift)"
    [ "$drift" = "0" ] || comp_bad="${comp_bad:+$comp_bad; }meta_kv_block_refs_drift=$drift (C8)"
    # ALL co-writers, not just the participants (the authority bounce
    # fences every one — the s11-range re-admit note applies verbatim).
    for idx in $(cowriter_idxs); do
        "$MWFLEET" unmount "$idx" >/dev/null 2>&1 || true
        local admitted=0 tries
        for ((tries = 0; tries < 15; tries++)); do
            if "$MWFLEET" mount "$idx" >/dev/null 2>&1; then admitted=1; break; fi
            sleep 10
        done
        [ "$admitted" = "1" ] || die "s11-subblock: co-writer m$idx could not re-admit after the cold-verify remount"
    done
    rm -f "$w_mnt/s11-subblock.dat" || die "s11-subblock: could not remove the leg's file"
    for idx in $(member_idxs); do
        cat "$(mnt_of "$idx")/.stats" >/dev/null 2>&1 ||
            die "s11-subblock: member m$idx is not healthy at leg end"
    done
    [ -z "$comp_bad" ] || die "s11-subblock CORRECTNESS RED: $comp_bad
The rung-18 standing-red (the fourth dangling-take face) RETIRED at rung 19:
the authority's composed commits recompute their durable accounting from the
composition itself (.benchmarks/2026-08-18-s11-widthn-refs-fix.md — this leg
went GREEN x3 from zero on that fix). ANY drift here is a REGRESSION of the
width-N refs/lineage composition, not a known residual."
    log "s11-subblock GREEN — the sub-block exception row priced (snapshots + fsck in $rowdir). Evidence tier: measured-simulated (one box, co-located members)"
}

# =============================================================================
# Rung 18 (§9.5): the S11 closing rows — MPI-IO / block-cyclic / tiny-ranges
# bounds / range kill matrix. `ior` is the PINNED external instrument
# (OQ-4's orchestrator-adopted default: "ior, pinned release + checksum,
# scoreboard-style"); it builds on demand into target/mw-ior (a BUILD
# product, the target/mw-guest precedent — never fleet residue).
# =============================================================================

IOR_VERSION="4.0.0"
IOR_SHA256="510b7d4ad0f287375848121aa5a1f9842db077c1d81ad0dde738e96255298158"
IOR_URL="https://github.com/hpc/ior/releases/download/$IOR_VERSION/ior-$IOR_VERSION.tar.gz"
IOR_BIN="$REPO/target/mw-ior/ior-$IOR_VERSION/src/ior"

ensure_ior() {
    command -v mpirun >/dev/null 2>&1 ||
        die "the MPI-IO rows need an MPI launcher (openmpi/mpich mpirun) on PATH — install the distro package (versions are recorded in the row)"
    [ -x "$IOR_BIN" ] && return 0
    command -v mpicc >/dev/null 2>&1 ||
        die "building the pinned ior needs mpicc (openmpi/mpich devel) on PATH"
    command -v curl >/dev/null 2>&1 || die "building the pinned ior needs curl"
    local bdir="$REPO/target/mw-ior"
    mkdir -p "$bdir"
    local tarball="$bdir/ior-$IOR_VERSION.tar.gz"
    if [ ! -f "$tarball" ] || ! echo "$IOR_SHA256  $tarball" | sha256sum -c --quiet - 2>/dev/null; then
        log "fetching pinned ior $IOR_VERSION"
        curl -fsSL -o "$tarball" "$IOR_URL" || die "could not fetch $IOR_URL (the pin: $IOR_SHA256)"
    fi
    echo "$IOR_SHA256  $tarball" | sha256sum -c --quiet - ||
        die "ior tarball checksum MISMATCH (expected $IOR_SHA256) — refusing an unpinned instrument"
    (
        cd "$bdir" && tar xzf "$tarball" && cd "ior-$IOR_VERSION" &&
            # -std=gnu17: ior 4.0.0's option.c calls a ()-declared fn
            # pointer with an argument — an error under GCC>=15's C23
            # default, legal pre-C23 (recorded build nuance, not a patch).
            ./configure --without-hdf5 --without-ncmpi CFLAGS="-std=gnu17 -O2" >configure.log 2>&1 &&
            make -j"$(nproc)" >make.log 2>&1
    ) || die "pinned ior build failed — see $bdir/ior-$IOR_VERSION/{configure,make}.log"
    [ -x "$IOR_BIN" ] || die "ior build produced no binary at $IOR_BIN"
    log "pinned ior $IOR_VERSION ready ($IOR_BIN, sha256 $IOR_SHA256)"
}

# One MPMD ior invocation over the co-writer mounts: every app context
# carries IDENTICAL options apart from -o (its own mount's path of the
# SAME file — global ranks compose one shared-file layout; proven
# semantics: `tasks: K*P`, single-shared-file, one inode). stdout to $1.
run_ior() { # outfile procs_per_mount fname extra-ior-args...
    local outfile="$1" procs="$2" fname="$3"
    shift 3
    local args=() idx first=1
    for idx in $(cowriter_idxs); do
        [ "$first" = "1" ] || args+=(":")
        first=0
        args+=(-np "$procs" "$IOR_BIN" -a POSIX -o "$(mnt_of "$idx")/$fname" "$@")
    done
    # --bind-to none: 32 ranks + N daemons share the cores — MPI core
    # binding would pin ranks onto the daemons' lanes. Root launch is the
    # matrix's own posture (ensure_root), hence --allow-run-as-root.
    # --map-by :OVERSUBSCRIBE: PRRTE's default slot count is PHYSICAL
    # cores; the row's ranks are IO-blocked and legitimately exceed it.
    timeout 1200 mpirun --allow-run-as-root --bind-to none --map-by :OVERSUBSCRIBE "${args[@]}" >"$outfile" 2>&1 || {
        tail -5 "$outfile" >&2
        die "ior invocation failed (rc=$? — full output in $outfile)"
    }
}

# Per-iteration bandwidths (MiB/s) of one access class from an ior run:
# the short per-iteration Results rows (the ~26-field 'Summary of all
# tests' row is excluded by the field-count guard).
ior_iter_bws() { # outfile write|read
    awk -v cls="$2" '$1 == cls && NF <= 12 { print $2 }' "$1"
}

# External-mounts posture preflight (the header's EXTERNAL-MOUNTS MODE):
# every named mount must be LIVE and in the posture its position claims —
# the mw_fleet mount_member engagement gates, read-side form. Loud
# refusals, never a silent degradation.
ext_verify_mounts() {
    local mnt v idx
    for idx in $(member_idxs); do
        mnt="$(mnt_of "$idx")"
        if [ -z "$mnt" ] || ! mountpoint -q "$mnt"; then
            die "external mount m$idx ('$mnt') is not a live mountpoint"
        fi
        cat "$mnt/.stats" >/dev/null 2>&1 ||
            die "external mount m$idx ($mnt) has no readable .stats inode"
    done
    v="$(stat_field 0 mount_posture)"
    [ "$v" = "writer" ] ||
        die "external authority mount ($(mnt_of 0)): mount_posture='$v' (want writer) — the first SQZ_MWMATRIX_MOUNTS entry must be the multi-writer-armed authority"
    v="$(stat_field 0 data_plane_fence_mode)"
    [ "$v" = "1" ] ||
        die "external authority: data_plane_fence_mode='$v' (want 1) — the S7 WERO hold is not standing (SQUEEZEFS_MULTI_WRITER arm missing, or a non-PR substrate)"
    v="$(stat_field 0 membership_mode)"
    [ "$v" = "owner" ] ||
        die "external authority: membership_mode='$v' (want owner) — the S6 plane is not armed"
    for idx in "${EXT_CW_IDXS[@]}"; do
        v="$(stat_field "$idx" mount_posture)"
        [ "$v" = "co-writer" ] ||
            die "external mount m$idx ($(mnt_of "$idx")): mount_posture='$v' (want co-writer) — a silently-degraded mount would fake the row"
        v="$(stat_field "$idx" membership_mode)"
        [ "$v" = "member" ] ||
            die "external co-writer m$idx: membership_mode='$v' (want member) — the S6 join did not engage"
    done
    log "external mounts verified: authority=$(mnt_of 0) (posture=writer, fence_mode=1, membership=owner) + ${#EXT_CW_IDXS[@]} co-writers (posture=co-writer, membership=member)"
    # The range-custody ARM has no stats-probeable posture gauge (the
    # rung-15 grant-census residual): the post-probe ranged-acquire check
    # below is the pre-row conviction, and the leg's engagement gate stays
    # the row-end one.
}

leg_s11_mpiio() {
    if [ "$EXT_MODE" = "1" ]; then
        ext_verify_mounts
    else
        require_cowriters 2
        [ "${RANGE_CUSTODY:-0}" = "1" ] ||
            die "s11-mpiio needs a range-custody-ARMED fleet: sudo SQZ_MWFLEET_RANGE_CUSTODY=1 tests/mw_fleet.sh create N=1 --cowriters=8"
    fi
    ensure_ior
    local rowdir cws k ranks idx w_mnt
    if [ "$EXT_MODE" = "1" ]; then
        # Rows land beside the authority mount by default (the field's
        # /scratch/tmp convention), never under the fleet STATE dir.
        rowdir="${SQZ_MWMATRIX_ROWDIR:-$(dirname "$(mnt_of 0)")/mwmatrix-rows}/s11mpiio-$(date +%s)"
    else
        rowdir="$STATE/rows/s11mpiio-$(date +%s)"
    fi
    mkdir -p "$rowdir"
    mapfile -t cws < <(cowriter_idxs)
    k="${#cws[@]}"
    ranks=$((k * S11_PROCS))
    w_mnt="$(mnt_of 0)"
    local sig=$((RANDOM * 32768 + RANDOM + 1))
    log "s11-mpiio: $k co-writer mounts x $S11_PROCS procs = $ranks ranks, ONE shared file, 4 MiB-aligned block-cyclic segments (ior $IOR_VERSION POSIX MPMD, -G $sig); baseline = -F file-per-proc, SAME fleet/geometry; A-B-B-A"
    if [ "$EXT_MODE" = "1" ]; then
        log "instrument: ior $IOR_VERSION (sha256 $IOR_SHA256) + $(mpirun --version 2>&1 | head -1); substrate: EXTERNAL mounts ($SQZ_MWMATRIX_MOUNTS) — the row's substrate/tier is the HARNESS's to state (a real-fabric field row is a different evidence tier from the local fleet's measured-simulated rows; docs/rc-manifest.md)"
    else
        log "instrument: ior $IOR_VERSION (sha256 $IOR_SHA256) + $(mpirun --version 2>&1 | head -1); substrate: tcp devsub (nvmet-tcp localhost)"
    fi
    local provisional=""
    pgrep -x cargo >/dev/null 2>&1 && provisional="PROVISIONAL (foreign cargo work running)"
    [ -n "$provisional" ] && warn "quiet gate: $provisional — the table is labeled; the gate still enforces"

    # ---- probe: self-size the sustained window --------------------------------
    # External-mounts mode: capture each co-writer's ranged ledger BEFORE the
    # probe — the probe pass is the pre-row range-custody conviction (there is
    # no armed-posture gauge to read; a whole-file-lease fleet would otherwise
    # burn four phases before the engagement gate names it).
    local -A __pre_rng=()
    if [ "$EXT_MODE" = "1" ]; then
        local __a __e
        for idx in "${cws[@]}"; do
            __a="$(stat_field "$idx" dlm_custody.dlm_custody_range_acquires)"
            __e="$(stat_field "$idx" dlm_custody.dlm_custody_range_extensions)"
            __pre_rng[$idx]=$((${__a:-0} + ${__e:-0}))
        done
    fi
    local s_probe=8 probe_bytes
    probe_bytes=$((ranks * 4 * s_probe))
    truncate -s "$((probe_bytes * 1024 * 1024))" "$w_mnt/s11-mpiio.dat" ||
        die "s11-mpiio: authority could not create the shared file"
    run_ior "$rowdir/probe.out" "$S11_PROCS" "s11-mpiio.dat" \
        -b 4m -t 4m -s "$s_probe" -w -e -k -E -G "$sig" -i 1
    local bw_probe
    bw_probe="$(ior_iter_bws "$rowdir/probe.out" write | head -1)"
    [ -n "$bw_probe" ] || die "s11-mpiio: probe parsed no write bandwidth ($rowdir/probe.out)"
    if [ "$EXT_MODE" = "1" ]; then
        local __post
        for idx in "${cws[@]}"; do
            __a="$(stat_field "$idx" dlm_custody.dlm_custody_range_acquires)"
            __e="$(stat_field "$idx" dlm_custody.dlm_custody_range_extensions)"
            __post=$((${__a:-0} + ${__e:-0}))
            [ "$__post" -gt "${__pre_rng[$idx]}" ] ||
                die "external co-writer m$idx ($(mnt_of "$idx")): ZERO ranged acquires/extensions across the probe pass — SQUEEZEFS_RANGE_CUSTODY is not armed on this mount (cluster_reset_v5_mw.sh arms it on every co-writer; a whole-file-lease row would be a lie)"
        done
        log "range-custody conviction: every co-writer's ranged ledger moved across the probe pass"
    fi
    # Target ~22 s per iteration, >=3 steady iterations after the allocation
    # pass. The file cap is the 10 GiB zram-budget clamp, RESTORED: the
    # ~6 GiB inline-map boundary (past which the shared file's composed map
    # spills `indirect:`) is no longer a refusal wall — the blob-aware
    # owner-side merge (rung 20 residual #1) rehydrates the blob and
    # COMPOSES chained/scoped publishes onto the full map, so a fast fabric
    # (the 200GbE field probe) self-sizing into the indirect domain is now
    # a covered row, not surprise fsync EIO (the retired fail-safe:
    # .benchmarks/2026-08-18-s11-widthn-refs-fix.md fix 4).
    local inline_cap_mb=10240
    local s_row n_iter s_capped
    read -r s_row s_capped <<<"$(python3 -c "
bw=$bw_probe; r=$ranks
s=int(bw*22/(r*4))
cap=int($inline_cap_mb/(r*4))
print(max(8, min(s, cap)), 1 if s > cap else 0)")"
    local row_mb=$((ranks * 4 * s_row))
    # Iteration count: ~70 s of steady window. When the inline-map cap binds
    # (fast fabrics), per-iteration wall shrinks — allow up to 128 iterations
    # to keep the >=60 s window instead of silently shipping a short row.
    local iter_ceil=24
    [ "$s_capped" = "1" ] && iter_ceil=128
    n_iter="$(python3 -c "
bw=$bw_probe; r=$ranks; s=$s_row
wall=r*4*s/max(bw,1)
import math
print(max(4, min($iter_ceil, math.ceil(70/max(wall,0.1))+1)))")"
    if [ "$s_capped" = "1" ]; then
        log "self-sizer wanted $(python3 -c "print(int($bw_probe*22/($ranks*4))*$ranks*4)") MiB — CAPPED to ${row_mb} MiB (s=$s_row): the 10 GiB zram-budget clamp (the indirect domain COMPOSES now — the rung-20 blob-aware owner-side merge; this cap is substrate budget, not a correctness boundary). Iteration ceiling raised to $iter_ceil to preserve the sustained window."
        local est_window
        est_window="$(python3 -c "print(int($n_iter*$ranks*4*$s_row/max($bw_probe,1)))")"
        [ "$est_window" -ge 60 ] ||
            warn "sustained window ~${est_window}s < 60s at the probed rate — bounded by the 10 GiB zram-budget cap; the row is LABELED by its own iteration table, read it with that bound in mind"
    fi
    log "probe: $bw_probe MiB/s aggregate -> file ${row_mb} MiB (s=$s_row), $n_iter iterations/phase (sustained window sized >=60 s + >=3 steady iterations)"
    truncate -s "$((row_mb * 1024 * 1024))" "$w_mnt/s11-mpiio.dat"

    # ---- A-B-B-A ---------------------------------------------------------------
    local phase=0
    for idx in $(member_idxs); do snap "$idx" 0 "$rowdir"; done
    declare -A PHASE_BW PHASE_ITERS
    run_phase() { # label shared|fpp
        phase=$((phase + 1))
        local label="$1" mode="$2" extra=()
        [ "$mode" = "fpp" ] && extra+=(-F)
        local t0 t1
        t0="$(date +%s)"
        run_ior "$rowdir/$label.out" "$S11_PROCS" \
            "$([ "$mode" = "fpp" ] && echo "s11-mpiio-fpp.dat" || echo "s11-mpiio.dat")" \
            -b 4m -t 4m -s "$s_row" -w -e -k -E -G "$sig" -i "$n_iter" "${extra[@]}"
        t1="$(date +%s)"
        local bws
        bws="$(ior_iter_bws "$rowdir/$label.out" write | tr '\n' ' ')"
        PHASE_ITERS[$label]="$bws"
        # Steady mean = iterations 2..N (iteration 1 is the allocation/
        # first-touch regime, reported separately); flatness = first vs
        # last steady iteration within 30%.
        # shellcheck disable=SC2086 # $bws is a deliberate word-split list of per-iteration numbers
        PHASE_BW[$label]="$(python3 - "$label" $bws <<'PYF'
import sys
label = sys.argv[1]
bws = [float(x) for x in sys.argv[2:]]
if len(bws) < 4:
    print(f"phase {label}: only {len(bws)} iterations — the sustained window needs >= 4", file=sys.stderr)
    sys.exit(1)
steady = bws[1:]
first, last = steady[0], steady[-1]
if last < 0.70 * first:
    print(f"phase {label}: NOT SUSTAINED — steady iterations decay {first:.0f} -> {last:.0f} MiB/s (> 30%): a burst number that decays is a FAILED row", file=sys.stderr)
    sys.exit(1)
print(f"{sum(steady)/len(steady):.1f}")
PYF
)" || die "s11-mpiio: phase $label failed the sustained-window gate"
        log "phase $label ($mode): steady ${PHASE_BW[$label]} MiB/s over $((t1 - t0))s wall (iters: ${PHASE_ITERS[$label]})"
        for idx in $(member_idxs); do snap "$idx" "$phase" "$rowdir"; done
    }
    run_phase A1 shared
    run_phase B1 fpp
    run_phase B2 fpp
    run_phase A2 shared

    # ---- the ≥0.8× gate, BOTH brackets ----------------------------------------
    python3 - "${PHASE_BW[A1]}" "${PHASE_BW[B1]}" "${PHASE_BW[B2]}" "${PHASE_BW[A2]}" <<'PYG' || die "s11-mpiio GATE FAILED"
import sys
a1, b1, b2, a2 = (float(x) for x in sys.argv[1:5])
r1, r2 = a1 / b1, a2 / b2
print(f"A-B-B-A: shared {a1:.0f} / disjoint {b1:.0f} = {r1:.3f}; shared {a2:.0f} / disjoint {b2:.0f} = {r2:.3f}")
if r1 < 0.8 or r2 < 0.8:
    print(f"S11 MPI-IO GATE FAILED: shared/disjoint bracket below 0.8x (r1={r1:.3f}, r2={r2:.3f})", file=sys.stderr)
    sys.exit(1)
print(f"S11 MPI-IO GATE: shared >= 0.8x disjoint in BOTH brackets (min {min(r1, r2):.3f})")
PYG

    # ---- engagement, exact ------------------------------------------------------
    python3 - "$rowdir" "$k" "$phase" "${cws[@]}" <<'PYE' || die "s11-mpiio: INVALID ROW (engagement)"
import json, sys
rowdir, k, last = sys.argv[1], int(sys.argv[2]), int(sys.argv[3])
cws = sys.argv[4:]
def flat(d, out=None, pfx=""):
    out = {} if out is None else out
    for kk, v in d.items():
        if isinstance(v, dict): flat(v, out, pfx + kk + ".")
        else: out[pfx + kk] = v
    return out
def load(i, ph):
    root = json.load(open(f"{rowdir}/m{i}_p{ph}.json"))
    return flat(root.get("metrics", root))
d = lambda i, a, b, key: int(load(i, b).get(key, 0) or 0) - int(load(i, a).get(key, 0) or 0)
bad = []
for m in cws:
    ranged = d(m, 0, last, "dlm_custody.dlm_custody_range_acquires") \
        + d(m, 0, last, "dlm_custody.dlm_custody_range_extensions")
    shipped = d(m, 0, last, "meta_ship_publish.shipped")
    print(f"m{m}: ranged acquires+extensions d={ranged} publish shipped d={shipped}")
    if ranged < 1:
        bad.append(f"m{m}: the ranged path never engaged (a silent whole-file fallback = the row is a lie)")
    if shipped < 1:
        bad.append(f"m{m}: no publish shipped — the co-writer's layout publishes must travel")
grants = d("0", 0, last, "range_custody.range_custody_grants")
caps = d("0", 0, last, "range_custody.range_custody_cap_refusals")
dem = d("0", 0, last, "range_custody.range_custody_demotions")
prs = d("0", 0, last, "patch_ineligible_range_shared")
ors = d("0", 0, last, "overlay_ineligible_range_shared")
conflicts = d("0", 0, last, "range_custody.range_custody_conflicts")
trims = d("0", 0, last, "range_custody.range_custody_desired_trims")
print(f"authority: grants d={grants} cap_refusals d={caps} demotions d={dem} conflicts d={conflicts} desired_trims d={trims} prs d={prs} ors d={ors}")
if grants < k: bad.append(f"authority grants d={grants} < {k} mounts — stripes unaccounted")
if caps != 0: bad.append(f"range_custody_cap_refusals d={caps} on a within-budget shape — the Issue-19 class")
if dem != 0: bad.append(f"demotions d={dem} on a 4MiB-ALIGNED row — fabricated block sharing")
if prs != 0 or ors != 0: bad.append(f"range-shared clause ledgers moved (prs={prs}, ors={ors}) — nothing should share a block on aligned rows")
if bad:
    print("S11 MPI-IO ENGAGEMENT FAILED:", file=sys.stderr)
    for b in bad: print(f"  {b}", file=sys.stderr)
    sys.exit(1)
print("S11 MPI-IO engagement GREEN (ranged engaged on every mount, Issue-19 column 0, zero fabricated sharing)")
PYE

    # ---- correctness: cross-mount read-back exact + cold oracle ----------------
    run_ior "$rowdir/readcheck.out" "$S11_PROCS" "s11-mpiio.dat" \
        -b 4m -t 4m -s "$s_row" -r -R -C -k -E -G "$sig" -i 1
    grep -qiE "incorrect|error" "$rowdir/readcheck.out" &&
        die "s11-mpiio: read-back-exact FAILED (reorder-tasks cross-mount check): $(grep -icE 'incorrect' "$rowdir/readcheck.out") bad transfers — $rowdir/readcheck.out"
    log "read-back exact: $(ior_iter_bws "$rowdir/readcheck.out" read | head -1) MiB/s aggregate reorder-read, zero data-check errors"

    for idx in "${cws[@]}"; do
        rm -f "$(mnt_of "$idx")/s11-mpiio-fpp.dat".* 2>/dev/null || true
    done
    local comp_bad=""
    if [ "$EXT_MODE" = "1" ]; then
        # External mounts have no fleet-lifecycle owner here: the oracle runs
        # WARM on the LIVE authority (online fsck over the admin lane; same
        # findings/C8 teeth). The COLD-remount oracle belongs to the local
        # fleet leg — on the field the teardown/re-reset cycle is where cold
        # verification lives (stated on the row, never silently equated).
        log "external mounts: WARM-authority fsck oracle (no remount owner; the local fleet leg's oracle is cold-remounted)"
    else
        "$MWFLEET" unmount 0 >/dev/null 2>&1 || true
        "$MWFLEET" mount 0 >/dev/null 2>&1 || die "s11-mpiio: authority remount failed"
    fi
    local out drift
    if out="$("$SQZ" fsck "$w_mnt" 2>&1)"; then
        echo "$out" >"$rowdir/fsck.out"
        echo "$out" | grep -q "findings: 0" || comp_bad="fsck findings != 0"
    else
        echo "$out" >"$rowdir/fsck.out"
        comp_bad="fsck FAILED"
    fi
    drift="$(stat_field 0 meta_kv_block_refs_drift)"
    [ "$drift" = "0" ] || comp_bad="${comp_bad:+$comp_bad; }meta_kv_block_refs_drift=$drift (C8)"
    rm -f "$w_mnt/s11-mpiio.dat" || die "s11-mpiio: could not remove the leg's file"
    if [ "$EXT_MODE" != "1" ]; then
        for idx in "${cws[@]}"; do
            "$MWFLEET" unmount "$idx" >/dev/null 2>&1 || true
            local admitted=0 tries
            for ((tries = 0; tries < 15; tries++)); do
                if "$MWFLEET" mount "$idx" >/dev/null 2>&1; then admitted=1; break; fi
                sleep 10
            done
            [ "$admitted" = "1" ] || die "s11-mpiio: co-writer m$idx could not re-admit after the cold remount"
        done
    fi
    for idx in $(member_idxs); do
        cat "$(mnt_of "$idx")/.stats" >/dev/null 2>&1 ||
            die "s11-mpiio: member m$idx is not healthy at leg end"
    done
    [ -z "$comp_bad" ] || die "s11-mpiio CORRECTNESS RED: $comp_bad
The rung-18 standing-red (the width-N same-ino publish refs composition)
RETIRED at rung 19: the authority computes displaced/inserted INSIDE the
chained merge / scoped Put, node compaction preserves the version lineage a
live link claims, and the accounting owner is the GLOBAL ino
(.benchmarks/2026-08-18-s11-widthn-refs-fix.md). ANY C8 drift here is a
REGRESSION of that composition — INCLUDING the indirect domain: past the
inline cap (~6 GiB at 4 MiB blocks) the composed map spills indirect and
the blob-aware owner-side merge (rung 20 residual #1, landed) rehydrates
the blob and composes chained/scoped publishes onto the full map, so a
refusal or drift there is a regression too."
    if [ "$EXT_MODE" = "1" ]; then
        log "s11-mpiio GREEN${provisional:+ [$provisional]} — the MPI-IO acceptance row over EXTERNAL mounts (outputs + snapshots + fsck in $rowdir). Evidence tier: the HARNESS's venue states it (real-fabric field rows are a different tier from the local fleet's measured-simulated ones — docs/rc-manifest.md); oracle: warm-authority"
    else
        log "s11-mpiio GREEN${provisional:+ [$provisional]} — the MPI-IO acceptance row (outputs + snapshots + fsck in $rowdir). Evidence tier: measured-simulated (one box, co-located members)"
    fi
}

leg_s11_blockcyclic() {
    require_cowriters 2
    [ "${RANGE_CUSTODY:-0}" = "1" ] ||
        die "s11-blockcyclic needs a range-custody-ARMED fleet: sudo SQZ_MWFLEET_RANGE_CUSTODY=1 tests/mw_fleet.sh create N=1 --cowriters=8"
    ensure_ior
    local rowdir cws k idx w_mnt
    rowdir="$STATE/rows/s11bc-$(date +%s)"
    mkdir -p "$rowdir"
    mapfile -t cws < <(cowriter_idxs)
    k="${#cws[@]}"
    w_mnt="$(mnt_of 0)"
    local sig=$((RANDOM * 32768 + RANDOM + 2))
    # ~512 blocks: enough that the span table's O(blocks) population is a
    # real statement, small enough for a bounded row.
    local s_bc=$((512 / k)) blocks=$((512 / k * k)) file_mb=$((512 / k * k * 4))
    log "s11-blockcyclic: $k mounts x 1 proc, round-robin block-cyclic over ONE ${file_mb}MiB file ($blocks x 4MiB blocks; a holder's spans NEVER adjacent — nothing coalesces); control = same-width -F"
    truncate -s $((file_mb * 1024 * 1024)) "$w_mnt/s11-bc.dat" ||
        die "s11-blockcyclic: authority could not create the file"

    for idx in $(member_idxs); do snap "$idx" 0 "$rowdir"; done
    # The live span-table sampler: the population only exists while the
    # write phase runs (grants release at close), so the gauge is sampled
    # DURING, max-held.
    (
        max_active=0 max_bytes=0
        while [ ! -f "$rowdir/.sampler-stop" ]; do
            a="$(stat_field 0 range_custody.range_custody_active 2>/dev/null || echo 0)"
            b="$(stat_field 0 range_custody.dlm_grant_table_bytes 2>/dev/null || echo 0)"
            [ -n "$a" ] && [ "$a" -gt "$max_active" ] 2>/dev/null && max_active="$a"
            [ -n "$b" ] && [ "$b" -gt "$max_bytes" ] 2>/dev/null && max_bytes="$b"
            echo "$max_active $max_bytes" >"$rowdir/sampler.out"
            sleep 0.5
        done
    ) &
    local sampler_pid=$!
    run_ior "$rowdir/bc.out" 1 "s11-bc.dat" -b 4m -t 4m -s "$s_bc" -w -e -k -E -G "$sig" -i 1
    touch "$rowdir/.sampler-stop"
    wait "$sampler_pid" 2>/dev/null || true
    for idx in $(member_idxs); do snap "$idx" 1 "$rowdir"; done
    local bc_bw
    bc_bw="$(ior_iter_bws "$rowdir/bc.out" write | head -1)"

    # Same-width disjoint control (the band's denominator).
    run_ior "$rowdir/bc-ctl.out" 1 "s11-bc-fpp.dat" -b 4m -t 4m -s "$s_bc" -w -e -k -G "$sig" -i 1 -F
    local ctl_bw
    ctl_bw="$(ior_iter_bws "$rowdir/bc-ctl.out" write | head -1)"
    for idx in $(member_idxs); do snap "$idx" 2 "$rowdir"; done

    local max_active max_bytes
    read -r max_active max_bytes <"$rowdir/sampler.out"
    log "block-cyclic: $bc_bw MiB/s vs same-width disjoint control $ctl_bw MiB/s; live span table max: active=$max_active grant_table_bytes=$max_bytes (expected ~= $blocks spans x 48 B + wholes)"

    python3 - "$rowdir" "$blocks" "$k" "$max_active" "$max_bytes" "$bc_bw" "$ctl_bw" <<'PYBC' || die "s11-blockcyclic: GATE FAILED"
import json, sys
rowdir, blocks, k, max_active, max_bytes, bc_bw, ctl_bw = \
    sys.argv[1], int(sys.argv[2]), int(sys.argv[3]), int(sys.argv[4]), int(sys.argv[5]), float(sys.argv[6]), float(sys.argv[7])
def flat(d, out=None, pfx=""):
    out = {} if out is None else out
    for kk, v in d.items():
        if isinstance(v, dict): flat(v, out, pfx + kk + ".")
        else: out[pfx + kk] = v
    return out
def load(i, ph):
    root = json.load(open(f"{rowdir}/m{i}_p{ph}.json"))
    return flat(root.get("metrics", root))
d = lambda i, a, b, key: int(load(i, b).get(key, 0) or 0) - int(load(i, a).get(key, 0) or 0)
bad = []
grants = d("0", 0, 1, "range_custody.range_custody_grants")
caps = d("0", 0, 2, "range_custody.range_custody_cap_refusals")
dem = d("0", 0, 2, "range_custody.range_custody_demotions")
budget = int(load("0", 1).get("range_custody.dlm_grant_table_budget_bytes", 0) or 0)
print(f"grants d={grants} (blocks={blocks})  cap_refusals d={caps}  demotions d={dem}")
print(f"span table max: active={max_active} bytes={max_bytes} (budget {budget})")
if grants < blocks:
    bad.append(f"grants d={grants} < blocks {blocks} — the non-coalescible shape must mint ~one span per block")
if caps != 0:
    bad.append(f"range_custody_cap_refusals d={caps} on a within-budget block-cyclic shape — THE ISSUE-19 CLASS: a constant refusing the workload S11 exists for")
if dem != 0:
    bad.append(f"demotions d={dem} on an aligned block-cyclic row — fabricated sharing")
# The live-population PEAK is reported, never gated: grants release at
# file CLOSE (the per-open-episode law), and a fast write phase's peak
# sits between two sampler polls by construction — the ACCOUNTING gate
# is the grants delta above (every span was minted and paid the caps).
if budget and max_bytes > budget:
    bad.append(f"grant table bytes {max_bytes} exceeded the R5 share {budget}")
if bc_bw < 0.8 * ctl_bw:
    bad.append(f"block-cyclic aggregate {bc_bw:.0f} MiB/s below 0.8x its same-width disjoint control {ctl_bw:.0f}")
if bad:
    print("S11 BLOCK-CYCLIC GATE FAILED:", file=sys.stderr)
    for b in bad: print(f"  {b}", file=sys.stderr)
    sys.exit(1)
print(f"S11 block-cyclic GREEN: grants ~= blocks ({grants}/{blocks}), ZERO cap refusals, table bounded (sampled max {max_active} spans / {max_bytes} B <= {budget} B), band {bc_bw/ctl_bw:.3f}x")
PYBC

    for idx in "${cws[@]}"; do
        rm -f "$(mnt_of "$idx")/s11-bc-fpp.dat".* 2>/dev/null || true
    done
    local comp_bad="" out drift
    "$MWFLEET" unmount 0 >/dev/null 2>&1 || true
    "$MWFLEET" mount 0 >/dev/null 2>&1 || die "s11-blockcyclic: authority remount failed"
    if out="$("$SQZ" fsck "$w_mnt" 2>&1)"; then
        echo "$out" >"$rowdir/fsck.out"
        echo "$out" | grep -q "findings: 0" || comp_bad="fsck findings != 0"
    else
        echo "$out" >"$rowdir/fsck.out"
        comp_bad="fsck FAILED"
    fi
    drift="$(stat_field 0 meta_kv_block_refs_drift)"
    [ "$drift" = "0" ] || comp_bad="${comp_bad:+$comp_bad; }meta_kv_block_refs_drift=$drift (C8)"
    rm -f "$w_mnt/s11-bc.dat"
    for idx in "${cws[@]}"; do
        "$MWFLEET" unmount "$idx" >/dev/null 2>&1 || true
        local admitted=0 tries
        for ((tries = 0; tries < 15; tries++)); do
            if "$MWFLEET" mount "$idx" >/dev/null 2>&1; then admitted=1; break; fi
            sleep 10
        done
        [ "$admitted" = "1" ] || die "s11-blockcyclic: co-writer m$idx could not re-admit"
    done
    [ -z "$comp_bad" ] || die "s11-blockcyclic CORRECTNESS RED: $comp_bad
The rung-18 standing-red (the width-N same-ino publish refs composition)
RETIRED at rung 19: the authority computes displaced/inserted INSIDE the
chained merge / scoped Put, node compaction preserves the version lineage a
live link claims, and the accounting owner is the GLOBAL ino
(.benchmarks/2026-08-18-s11-widthn-refs-fix.md). ANY C8 drift here is a
REGRESSION of that composition — INCLUDING the indirect domain: past the
inline cap (~6 GiB at 4 MiB blocks) the composed map spills indirect and
the blob-aware owner-side merge (rung 20 residual #1, landed) rehydrates
the blob and composes chained/scoped publishes onto the full map, so a
refusal or drift there is a regression too."
    log "s11-blockcyclic GREEN — the Issue-19 shape adjudicated live (outputs in $rowdir). Evidence tier: measured-simulated (one box, co-located members)"
}

leg_s11_tiny() {
    require_cowriters 2
    [ "${RANGE_CUSTODY:-0}" = "1" ] ||
        die "s11-tiny needs a range-custody-ARMED fleet"
    local rowdir cws m1 m2 w_mnt idx
    rowdir="$STATE/rows/s11tiny-$(date +%s)"
    mkdir -p "$rowdir"
    mapfile -t cws < <(cowriter_idxs)
    m1="${cws[0]}"
    m2="${cws[1]}"
    w_mnt="$(mnt_of 0)"
    log "s11-tiny: co-writer m$m1 floods 4096 byte-granular unaligned tiny writes across ONE 64MiB file (16 blocks); co-writer m$m2's own-file fsync ops sample foreign-client latency before/during"
    truncate -s $((64 * 1024 * 1024)) "$w_mnt/s11-tiny.dat" ||
        die "s11-tiny: authority could not create the file"

    # Foreign-client latency BASELINE (m2, own file, 20 fsync'd 4KiB ops).
    lat_probe() { # idx outfile
        python3 - "$(mnt_of "$1")/s11-tiny-probe-$1.dat" >"$2" <<'PYL'
import os, sys, time
path = sys.argv[1]
fd = os.open(path, os.O_WRONLY | os.O_CREAT, 0o644)
buf = b"\x44" * 4096
for i in range(20):
    t0 = time.monotonic()
    os.pwrite(fd, buf, i * 4096)
    os.fsync(fd)
    print(f"{(time.monotonic() - t0) * 1000:.2f}")
os.close(fd)
PYL
    }
    lat_probe "$m2" "$rowdir/lat-before"

    for idx in 0 "$m1" "$m2"; do snap "$idx" 0 "$rowdir"; done
    # The storm + the DURING-probe, concurrent.
    (
        python3 - "$(mnt_of "$m1")/s11-tiny.dat" >"$rowdir/storm.out" 2>&1 <<'PYS'
import os, random, sys, time
path = sys.argv[1]
fd = os.open(path, os.O_WRONLY)
rng = random.Random(0x511)
buf = b"\x77" * 137
t0 = time.monotonic()
for _ in range(4096):
    off = rng.randrange(0, 64 * 1024 * 1024 - 137)
    os.pwrite(fd, buf, off)
os.fsync(fd)
os.close(fd)
print(f"storm complete: 4096 x 137B unaligned writes in {time.monotonic() - t0:.2f}s")
PYS
        echo $? >"$rowdir/storm-rc"
    ) &
    local storm_pid=$!
    # Sample the live span table while the storm runs.
    (
        max_active=0 max_bytes=0
        while kill -0 "$storm_pid" 2>/dev/null; do
            a="$(stat_field 0 range_custody.range_custody_active 2>/dev/null || echo 0)"
            b="$(stat_field 0 range_custody.dlm_grant_table_bytes 2>/dev/null || echo 0)"
            [ -n "$a" ] && [ "$a" -gt "$max_active" ] 2>/dev/null && max_active="$a"
            [ -n "$b" ] && [ "$b" -gt "$max_bytes" ] 2>/dev/null && max_bytes="$b"
            echo "$max_active $max_bytes" >"$rowdir/sampler.out"
            sleep 0.2
        done
    ) &
    local sampler_pid=$!
    lat_probe "$m2" "$rowdir/lat-during"
    wait "$storm_pid" 2>/dev/null || true
    wait "$sampler_pid" 2>/dev/null || true
    [ "$(cat "$rowdir/storm-rc" 2>/dev/null)" = "0" ] ||
        die "s11-tiny: the tiny-write storm FAILED (a wedge or refusal on a within-budget shape): $(tail -3 "$rowdir/storm.out")"
    log "$(head -1 "$rowdir/storm.out")"
    for idx in 0 "$m1" "$m2"; do snap "$idx" 1 "$rowdir"; done

    local max_active=0 max_bytes=0
    [ -f "$rowdir/sampler.out" ] && read -r max_active max_bytes <"$rowdir/sampler.out"
    python3 - "$rowdir" "$m1" "$max_active" "$max_bytes" <<'PYT' || die "s11-tiny: GATE FAILED"
import json, statistics, sys
rowdir, m1, max_active, max_bytes = sys.argv[1], sys.argv[2], int(sys.argv[3]), int(sys.argv[4])
def flat(d, out=None, pfx=""):
    out = {} if out is None else out
    for kk, v in d.items():
        if isinstance(v, dict): flat(v, out, pfx + kk + ".")
        else: out[pfx + kk] = v
    return out
def load(i, ph):
    root = json.load(open(f"{rowdir}/m{i}_p{ph}.json"))
    return flat(root.get("metrics", root))
d = lambda i, a, b, key: int(load(i, b).get(key, 0) or 0) - int(load(i, a).get(key, 0) or 0)
bad = []
caps = d("0", 0, 1, "range_custody.range_custody_cap_refusals")
grants = d("0", 0, 1, "range_custody.range_custody_grants")
ext = d("0", 0, 1, "range_custody.range_custody_extensions")
budget = int(load("0", 1).get("range_custody.dlm_grant_table_budget_bytes", 0) or 0)
before = [float(x) for x in open(f"{rowdir}/lat-before")]
during = [float(x) for x in open(f"{rowdir}/lat-during")]
mb, md = statistics.median(before), statistics.median(during)
# The file is 16 blocks; the geometry cap is max(16, blocks) = 16: the
# coalescing law must hold the live population AT OR UNDER it.
print(f"storm: grants d={grants} extensions d={ext} cap_refusals d={caps}")
print(f"live span table max: active={max_active} bytes={max_bytes} (budget {budget}; 16-block file => <= 16+1 spans expected)")
print(f"foreign-client latency median: before {mb:.2f} ms -> during {md:.2f} ms ({md/max(mb,0.01):.2f}x)")
if caps != 0:
    bad.append(f"cap_refusals d={caps}: a within-budget tiny-ranges shape refused — the Issue-19 class")
if max_active > 17:
    bad.append(f"live spans peaked at {max_active} on a 16-block file — admit-time coalescing is not holding O(file blocks)")
if budget and max_bytes > budget:
    bad.append(f"grant table bytes {max_bytes} exceeded the R5 share {budget}")
if md > 5 * max(mb, 0.01):
    bad.append(f"foreign-client latency collateral: {md:.2f} ms during vs {mb:.2f} ms before (> 5x)")
if bad:
    print("S11 TINY-RANGES GATE FAILED:", file=sys.stderr)
    for b in bad: print(f"  {b}", file=sys.stderr)
    sys.exit(1)
print("S11 tiny-ranges bounds GREEN: coalescing holds O(blocks), table bounded, zero refusals, no wedge, foreign latency within band")
PYT

    rm -f "$w_mnt/s11-tiny.dat" "$(mnt_of "$m2")/s11-tiny-probe-$m2.dat"
    local out drift comp_bad=""
    if out="$("$SQZ" fsck "$w_mnt" 2>&1)"; then
        echo "$out" | grep -q "findings: 0" || comp_bad="fsck findings != 0"
    else
        comp_bad="fsck FAILED"
    fi
    echo "$out" >"$rowdir/fsck.out"
    drift="$(stat_field 0 meta_kv_block_refs_drift)"
    [ "$drift" = "0" ] || comp_bad="${comp_bad:+$comp_bad; }drift=$drift"
    [ -z "$comp_bad" ] || die "s11-tiny CORRECTNESS RED: $comp_bad"
    log "s11-tiny GREEN — the adversarial bounds row, live face (at-budget refusal law = the standing rung-15 in-process pins). Row in $rowdir"
}

leg_s11_killrange() {
    require_cowriters 2
    [ "${RANGE_CUSTODY:-0}" = "1" ] ||
        die "s11-killrange needs a range-custody-ARMED fleet"
    local rounds="$S7_ROUNDS" rowdir cws m1 m2 w_mnt idx round
    rowdir="$STATE/rows/s11kr-$(date +%s)"
    mkdir -p "$rowdir"
    mapfile -t cws < <(cowriter_idxs)
    m1="${cws[0]}"
    m2="${cws[1]}"
    w_mnt="$(mnt_of 0)"
    log "s11-killrange: cell H x $rounds (kill -9 a range HOLDER mid-write) + cell A x 2 (kill -9 the AUTHORITY mid-assembly); fsck + C8 after EVERY cell"

    # ---- Cell H: holder kill -9 mid-write, x rounds ---------------------------
    local half_mb=32 half_bytes=$((32 * 1024 * 1024))
    for ((round = 1; round <= rounds; round++)); do
        local f="s11-kr-h$round.dat"
        truncate -s $((2 * half_bytes)) "$w_mnt/$f" || die "cell H round $round: create failed"
        snap 0 "h${round}a" "$rowdir"
        dd if=/dev/urandom of="$rowdir/src-h$round" bs=1M count="$half_mb" status=none
        # Victim m1 writes low half (kill mid-write); survivor m2 writes high.
        (
            while :; do
                dd if=/dev/zero of="$(mnt_of "$m1")/$f" bs=4M count=$((half_mb / 4)) \
                    conv=fsync,notrunc oflag=seek_bytes seek=0 2>/dev/null || exit 0
            done
        ) &
        local v_pid=$!
        (
            dd if="$rowdir/src-h$round" of="$(mnt_of "$m2")/$f" bs=4M \
                conv=fsync,notrunc oflag=seek_bytes seek="$half_bytes" 2>"$rowdir/k-m$m2-$round.err"
            echo $? >"$rowdir/k-rc-$round"
        ) &
        local s_pid=$!
        sleep 1
        local d_pid
        d_pid="$(pgrep -f "squeezefs.*mount.*$(mnt_of "$m1")" | head -1)"
        [ -n "$d_pid" ] || die "cell H round $round: no victim daemon pid"
        kill -9 "$d_pid"
        kill "$v_pid" 2>/dev/null || true
        wait "$v_pid" 2>/dev/null || true
        wait "$s_pid" 2>/dev/null || true
        [ "$(cat "$rowdir/k-rc-$round" 2>/dev/null)" = "0" ] ||
            die "cell H round $round: the SURVIVOR m$m2's stream FAILED after the peer's kill: $(head -3 "$rowdir/k-m$m2-$round.err")"
        # The era law: the authority sweeps the victim's lease; its ranges
        # die with the era; the table converges.
        local swept=0 tries active
        for ((tries = 0; tries < 40; tries++)); do
            active="$(stat_field 0 range_custody.range_custody_active)"
            [ "$active" = "0" ] && { swept=1; break; }
            sleep 2
        done
        [ "$swept" = "1" ] ||
            die "cell H round $round: range_custody_active=$active never converged to 0 — a dead holder's ranges are stranded"
        local gcon
        gcon="$(stat_field 0 dlm_custody.dlm_custody_grace_conflicts)"
        [ "$gcon" = "0" ] || die "cell H round $round: dlm_custody_grace_conflicts=$gcon (must stay 0)"
        # Victim re-admits (grace ladder).
        umount -l "$(mnt_of "$m1")" 2>/dev/null || true
        local admitted=0
        for ((tries = 0; tries < 15; tries++)); do
            if "$MWFLEET" mount "$m1" >/dev/null 2>&1; then admitted=1; break; fi
            sleep 10
        done
        [ "$admitted" = "1" ] || die "cell H round $round: victim m$m1 could not re-admit"
        # Survivor's half verifies; fsck + C8 clean AFTER EVERY CELL.
        local sha_src sha_got out drift
        sha_src="$(sha256sum "$rowdir/src-h$round" | cut -d' ' -f1)"
        sha_got="$(dd if="$(mnt_of "$m2")/$f" bs=4M skip=$((half_mb / 4)) count=$((half_mb / 4)) status=none | sha256sum | cut -d' ' -f1)"
        [ "$sha_src" = "$sha_got" ] || die "cell H round $round: the survivor's acked half diverged"
        out="$("$SQZ" fsck "$w_mnt" 2>&1)" || die "cell H round $round: fsck FAILED: $(echo "$out" | tail -3)"
        echo "$out" | grep -q "findings: 0" || die "cell H round $round: fsck findings != 0"
        drift="$(stat_field 0 meta_kv_block_refs_drift)"
        [ "$drift" = "0" ] || die "cell H round $round: C8 drift=$drift"
        rm -f "$w_mnt/$f" "$rowdir/src-h$round"
        log "cell H round $round/$rounds GREEN (victim swept, survivor exact, fsck+C8 clean)"
    done

    # ---- Cell A: authority kill -9 mid-assembly, x2 ---------------------------
    for round in 1 2; do
        local f="s11-kr-a$round.dat"
        truncate -s $((8 * 1024 * 1024)) "$w_mnt/$f" || die "cell A round $round: create failed"
        # Two-holder sub-block extent churn (the demotion + assembler live),
        # long-running: the kill lands MID-assembly.
        (
            python3 - "$(mnt_of "$m1")/$f" 0 >"$rowdir/a1-$round.out" 2>&1 <<'PYA'
import os, sys, time
path, start = sys.argv[1], int(sys.argv[2])
fd = os.open(path, os.O_WRONLY)
buf = b"\x61" * 4096
deadline = time.monotonic() + 30
while time.monotonic() < deadline:
    off = start
    while off < start + 2 * 1024 * 1024:
        os.pwrite(fd, buf, off)
        off += 4096
    try:
        os.fsync(fd)
    except OSError as e:
        print(f"fsync interrupted (expected across the authority kill): {e}", flush=True)
        time.sleep(1)
os.close(fd)
print("holder A stream done", flush=True)
PYA
        ) &
        local a_pid=$!
        (
            python3 - "$(mnt_of "$m2")/$f" $((2 * 1024 * 1024)) >"$rowdir/a2-$round.out" 2>&1 <<'PYB'
import os, sys, time
path, start = sys.argv[1], int(sys.argv[2])
fd = os.open(path, os.O_WRONLY)
buf = b"\x62" * 4096
deadline = time.monotonic() + 30
while time.monotonic() < deadline:
    off = start
    while off < start + 2 * 1024 * 1024:
        os.pwrite(fd, buf, off)
        off += 4096
    try:
        os.fsync(fd)
    except OSError as e:
        print(f"fsync interrupted (expected across the authority kill): {e}", flush=True)
        time.sleep(1)
os.close(fd)
print("holder B stream done", flush=True)
PYB
        ) &
        local b_pid=$!
        # Let the demotion + extent ship engage, then kill the ASSEMBLER.
        local engaged=0 tries served
        for ((tries = 0; tries < 20; tries++)); do
            served="$(stat_field 0 meta_ship_publish.extent_served 2>/dev/null || echo 0)"
            [ -n "$served" ] && [ "$served" -gt 0 ] 2>/dev/null && { engaged=1; break; }
            sleep 1
        done
        [ "$engaged" = "1" ] || warn "cell A round $round: extent ship not yet observed pre-kill (the kill still lands mid-custody)"
        local auth_pid
        auth_pid="$(pgrep -f "squeezefs.*mount.*$w_mnt" | head -1)"
        [ -n "$auth_pid" ] || die "cell A round $round: no authority daemon pid"
        kill -9 "$auth_pid"
        log "cell A round $round: authority killed -9 mid-assembly"
        wait "$a_pid" 2>/dev/null || true
        wait "$b_pid" 2>/dev/null || true
        # Recover the fleet: authority first, then the co-writers.
        umount -l "$w_mnt" 2>/dev/null || true
        local admitted=0
        for ((tries = 0; tries < 15; tries++)); do
            if "$MWFLEET" mount 0 >/dev/null 2>&1; then admitted=1; break; fi
            sleep 5
        done
        [ "$admitted" = "1" ] || die "cell A round $round: authority could not remount"
        for idx in $(cowriter_idxs); do
            "$MWFLEET" unmount "$idx" >/dev/null 2>&1 || true
            admitted=0
            for ((tries = 0; tries < 15; tries++)); do
                if "$MWFLEET" mount "$idx" >/dev/null 2>&1; then admitted=1; break; fi
                sleep 10
            done
            [ "$admitted" = "1" ] || die "cell A round $round: co-writer m$idx could not re-admit after the authority kill"
        done
        # Re-drive a short two-holder pass on the recovered fleet: the
        # plane must serve again (re-grant, re-demote, re-assemble).
        snap 0 "a${round}r" "$rowdir"
        python3 - "$(mnt_of "$m1")/$f" 0 <<'PYR' || die "cell A round $round: post-recovery holder-A pass failed"
import os, sys
path, start = sys.argv[1], int(sys.argv[2])
fd = os.open(path, os.O_WRONLY)
buf = b"\x63" * 4096
off = start
while off < start + 2 * 1024 * 1024:
    os.pwrite(fd, buf, off)
    off += 4096
os.fsync(fd)
os.close(fd)
PYR
        python3 - "$(mnt_of "$m2")/$f" $((2 * 1024 * 1024)) <<'PYR2' || die "cell A round $round: post-recovery holder-B pass failed"
import os, sys
path, start = sys.argv[1], int(sys.argv[2])
fd = os.open(path, os.O_WRONLY)
buf = b"\x64" * 4096
off = start
while off < start + 2 * 1024 * 1024:
    os.pwrite(fd, buf, off)
    off += 4096
os.fsync(fd)
os.close(fd)
PYR2
        # Cold byte verify of the ACKED final passes + the oracle.
        "$MWFLEET" unmount 0 >/dev/null 2>&1 || true
        "$MWFLEET" mount 0 >/dev/null 2>&1 || die "cell A round $round: cold remount failed"
        python3 - "$w_mnt/$f" <<'PYV' || die "cell A round $round: cold byte verify FAILED (acked post-recovery bytes lost)"
import sys
data = open(sys.argv[1], "rb").read()
half = 2 * 1024 * 1024
assert data[:half] == b"\x63" * half, "holder A's post-recovery acked half diverged"
assert data[half:2 * half] == b"\x64" * half, "holder B's post-recovery acked half diverged"
PYV
        # Cell A's venue IS the sub-block assembler plane, so its fsck
        # verdict inherits the ADJUDICATED standing-red shape EXACTLY
        # (the fstests expected-shape discipline): one C2+C8 pair on ONE
        # offset ('1 durable vs 0') = the fourth dangling-take face
        # (rung 19's, .benchmarks/2026-08-18-s11-mpiio-row.md) —
        # continue LOUD-labeled. ANY other finding dies.
        local out drift
        out="$("$SQZ" fsck "$w_mnt" 2>&1)" || {
            # The FACE, exactly: every finding is a C2 'leaked block' or a
            # C8 '1 durable vs 0' — in matched pairs (fsck is report-only,
            # so each round's pair PERSISTS and the count accumulates one
            # pair per demoted-churn round). Any other class/text dies.
            local c2 c8 other
            c2="$(echo "$out" | grep -c '\[C2\].*leaked block' || true)"
            c8="$(echo "$out" | grep -c '\[C8\].*1 durable record(s)' || true)"
            other="$(echo "$out" | grep -c '^\s*\[C' || true)"
            if [ "$c2" -ge 1 ] && [ "$c2" = "$c8" ] && [ "$((c2 + c8))" = "$other" ]; then
                warn "cell A round $round: the ADJUDICATED fourth-face shape ($c2 C2+C8 dangling-take pair(s), accumulated report-only across rounds) — standing-red, rung 19's; the custody/era laws' halves are green"
            else
                die "cell A round $round: fsck FAILED with a NON-adjudicated shape: $(echo "$out" | tail -4)"
            fi
        }
        drift="$(stat_field 0 meta_kv_block_refs_drift)"
        case "$drift" in 0 | 2 | 4 | 6) ;; *)
            die "cell A round $round: C8 drift=$drift (the adjudicated face reads 2 per accumulated pair; anything else is a regression)"
            ;;
        esac
        # The cold-verify bounce fenced every co-writer AGAIN (the S7
        # posture) — re-admit the fleet before the next cell.
        for idx in $(cowriter_idxs); do
            "$MWFLEET" unmount "$idx" >/dev/null 2>&1 || true
            admitted=0
            for ((tries = 0; tries < 15; tries++)); do
                if "$MWFLEET" mount "$idx" >/dev/null 2>&1; then admitted=1; break; fi
                sleep 10
            done
            [ "$admitted" = "1" ] || die "cell A round $round: co-writer m$idx could not re-admit after the cold verify"
        done
        rm -f "$w_mnt/$f"
        log "cell A round $round/2 GREEN (authority killed mid-assembly, fleet recovered, re-driven pass exact, fsck+C8 clean)"
    done

    for idx in $(member_idxs); do
        cat "$(mnt_of "$idx")/.stats" >/dev/null 2>&1 ||
            die "s11-killrange: member m$idx is not healthy at leg end"
    done
    log "s11-killrange GREEN — cell H x $rounds + cell A x 2, fsck+C8 clean after every cell. Evidence tier: measured-simulated (one box, co-located members)"
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

leg_s10_delegation() {
    # Rung 12 (design §8.2 lever 1 + PR row 12): LOOKUP-class delegations
    # end-to-end on the LIVE fleet — grant → serve-local → foreign
    # mutation → recall → CURRENT serve — plus the lever-off A/B control
    # and an authority kill-9 re-assertion pass. The engagement
    # instrument is the wire-verb ledger: delegated LOOKUPs must NOT
    # ship (that is the point), and `dlm_delegation.*` deltas must
    # account for the serves. The in-place grace re-assert (a holder
    # PROCESS surviving an authority restart) is pinned in cargo
    # (tests/mw_delegation_tests.rs::grace_reassertion_...); the live
    # fleet's co-writer posture is re-admission-BY-REMOUNT (the rung-10
    # documented posture), so the kill-9 pass here proves the remounted
    # holder RE-EARNS delegations and serves current, with the fsck/C8
    # oracle green after the kill.
    require_cowriters 1
    local rowdir cw w_mnt cw_mnt files n
    rowdir="$STATE/rows/s10d-$(date +%s)"
    mkdir -p "$rowdir"
    cw="$(cowriter_idxs | head -1)"
    w_mnt="$(mnt_of 0)"
    cw_mnt="$(mnt_of "$cw")"
    files=24

    dfield() { # idx key -> value-or-0 (the dlm_delegation family nests)
        local v
        v="$(stat_field "$1" "$2")"
        echo "${v:-0}"
    }
    drop_dentries() { # force the next stat/ls to the DAEMON (kernel
        # dentry+inode caches dropped; daemon-side delegations are
        # untouched — they are the thing under test)
        sync
        echo 2 >/proc/sys/vm/drop_caches
    }
    warm_delegations() { # earn grants: one shipped pass over the tree.
        # `ls -1 --color=never` + `stat`, never `ls -l`: the long form
        # issues getxattr/listxattr per entry (ACL/security probes), and
        # the XATTR delegation class is rows 13+'s — those verbs SHIP by
        # design in rung 12 and would pollute the LOOKUP-class ledger
        # (measured live: ls -l = exactly its 25 xattr calls shipped;
        # stat = 0).
        drop_dentries
        ls -1 --color=never "$cw_mnt/s10-deleg" >/dev/null || die "s10-delegation: warm ls failed"
        for ((n = 0; n < files; n++)); do
            stat "$cw_mnt/s10-deleg/f$n" >/dev/null || die "s10-delegation: warm stat failed"
        done
    }
    wait_delegated_serving() { # poll until a delegated serve engages
        # (the co-writer's reader view must catch up to the grant stamps
        # — bounded by reader_staleness_bound_ms — and the recall
        # channel's first round must complete)
        local tries h0 h1
        for ((tries = 0; tries < 60; tries++)); do
            warm_delegations
            h0="$(dfield "$cw" dlm_delegation.dlm_delegation_hits)"
            drop_dentries
            stat "$cw_mnt/s10-deleg/f0" >/dev/null 2>&1 || true
            h1="$(dfield "$cw" dlm_delegation.dlm_delegation_hits)"
            [ "$h1" -gt "$h0" ] && return 0
            sleep 1
        done
        die "s10-delegation: delegated serves never engaged within 60s (hits flat at $h1 — view catch-up or the recall channel is broken)"
    }

    # ---- Phase 0: the tree, built by the AUTHORITY ---------------------------
    mkdir -p "$w_mnt/s10-deleg" || die "s10-delegation: mkdir failed"
    for ((n = 0; n < files; n++)); do
        echo "payload-$n" >"$w_mnt/s10-deleg/f$n" || die "s10-delegation: seed write failed"
    done
    sync "$w_mnt/s10-deleg" 2>/dev/null || true
    log "tree built on the authority ($files files); waiting for the co-writer's delegated serves to engage"
    wait_delegated_serving

    # ---- Phase 1: ENGAGEMENT — delegated LOOKUPs do not ship -----------------
    for idx in $(member_idxs); do snap "$idx" 0 "$rowdir"; done
    local hits0 ship0 hits1 ship1 hits_d ship_d grants0
    hits0="$(dfield "$cw" dlm_delegation.dlm_delegation_hits)"
    ship0="$(dfield "$cw" meta_ship.shipped_verbs)"
    grants0="$(dfield 0 dlm_delegation.dlm_delegation_grants)"
    drop_dentries
    ls -1 --color=never "$cw_mnt/s10-deleg" >/dev/null || die "s10-delegation: measured ls failed"
    for ((n = 0; n < files; n++)); do
        stat "$cw_mnt/s10-deleg/f$n" >/dev/null || die "s10-delegation: measured stat failed"
    done
    hits1="$(dfield "$cw" dlm_delegation.dlm_delegation_hits)"
    ship1="$(dfield "$cw" meta_ship.shipped_verbs)"
    hits_d=$((hits1 - hits0))
    ship_d=$((ship1 - ship0))
    [ "$hits_d" -ge "$files" ] ||
        die "s10-delegation: only $hits_d delegated serves across a $files-file stat pass — the delegation is not serving"
    # The point of S10: the delegated pass ships (near) nothing. The
    # allowance is the instrument's own ceremony (reading a co-writer's
    # .stats ships its verbs — the s8-a ±4 law, two snapshots + probes).
    [ "$ship_d" -le 12 ] ||
        die "s10-delegation: the delegated stat pass SHIPPED $ship_d verbs (hits=$hits_d) — delegated LOOKUPs must not ship"
    log "engagement: $hits_d delegated serves, $ship_d shipped verbs (ceremony skew), owner grants so far: $grants0"

    # ---- Phase 2: THE COHERENCE LAW, live ------------------------------------
    # The authority's create in the delegated directory must RECALL the
    # holder before it applies — proven two ways: the recall ledger moves
    # (acked >= 1, timeouts 0), and the holder sees the fresh name
    # IMMEDIATELY after the create returns (no staleness window, no
    # sleep: the recall preceded the publish).
    local acked0 acked1 to0 to1
    warm_delegations
    wait_delegated_serving
    acked0="$(dfield 0 dlm_recall.dlm_revokes_acked)"
    to0="$(dfield 0 dlm_delegation.dlm_delegation_recall_timeouts)"
    touch "$w_mnt/s10-deleg/fresh-coherence" || die "s10-delegation: the gated create failed"
    acked1="$(dfield 0 dlm_recall.dlm_revokes_acked)"
    to1="$(dfield 0 dlm_delegation.dlm_delegation_recall_timeouts)"
    [ "$acked1" -gt "$acked0" ] ||
        die "s10-delegation: the conflicting create applied without an ACKED recall (acked $acked0 -> $acked1) — the coherence law did not engage"
    [ "$to1" = "$to0" ] ||
        die "s10-delegation: recall TIMED OUT under a live holder (timeouts $to0 -> $to1) — the channel is broken"
    drop_dentries
    stat "$cw_mnt/s10-deleg/fresh-coherence" >/dev/null ||
        die "s10-delegation: STALE SERVE — the holder cannot see a name whose create already returned (recall-before-publish broken)"
    [ "$(dfield "$cw" dlm_delegation.dlm_delegation_stale_serves)" = "0" ] ||
        die "s10-delegation: dlm_delegation_stale_serves moved on the holder (must stay 0)"
    log "coherence: recall acked before the publish; the fresh name visible on the holder IMMEDIATELY after create returned"

    # ---- Phase 3: the LEVER-OFF A/B control ----------------------------------
    "$MWFLEET" unmount "$cw" || die "s10-delegation: control unmount failed"
    SQUEEZEFS_DELEGATION=0 "$MWFLEET" mount "$cw" || die "s10-delegation: lever-off control mount failed"
    local c_hits0 c_hits1 c_ship0 c_ship1
    drop_dentries
    ls -1 --color=never "$cw_mnt/s10-deleg" >/dev/null || die "s10-delegation: control warm failed"
    c_hits0="$(dfield "$cw" dlm_delegation.dlm_delegation_hits)"
    c_ship0="$(dfield "$cw" meta_ship.shipped_verbs)"
    drop_dentries
    for ((n = 0; n < files; n++)); do
        stat "$cw_mnt/s10-deleg/f$n" >/dev/null || die "s10-delegation: control stat failed"
    done
    c_hits1="$(dfield "$cw" dlm_delegation.dlm_delegation_hits)"
    c_ship1="$(dfield "$cw" meta_ship.shipped_verbs)"
    [ "$c_hits1" = "$c_hits0" ] ||
        die "s10-delegation: the LEVER-OFF control served $((c_hits1 - c_hits0)) delegated hits — the A/B control is not dark"
    [ $((c_ship1 - c_ship0)) -ge "$files" ] ||
        die "s10-delegation: the lever-off control shipped only $((c_ship1 - c_ship0)) verbs across $files stats — the control did not engage the wire"
    log "lever-off control: 0 delegated serves, $((c_ship1 - c_ship0)) shipped verbs (the A/B holds); restoring the lever-on mount"
    "$MWFLEET" unmount "$cw" || die "s10-delegation: control restore unmount failed"
    "$MWFLEET" mount "$cw" || die "s10-delegation: lever-on restore mount failed"
    wait_delegated_serving

    # ---- Phase 4: authority kill-9 + the re-assertion pass -------------------
    # Live posture: the co-writer self-fences on the dead lease and
    # re-admits BY REMOUNT (rung-10's documented deferral); the remounted
    # holder must RE-EARN delegations against the successor era, serve
    # current, and the oracle must be green after the kill. (The
    # surviving-process grace re-assert is the cargo suite's — a live
    # co-writer does not survive its authority here.)
    local w_pid t_kill t_up tries
    w_pid="$(awk -F'\t' '$1==0 {print $7}' "$MEMBERS")"
    "$MWFLEET" kill 0 --sig 9
    t_kill="$(date +%s)"
    umount -l "$w_mnt" 2>/dev/null || true
    wait_for_unmounted "$w_mnt"
    for ((tries = 0; tries < 120; tries++)); do
        kill -0 "$w_pid" 2>/dev/null || break
        sleep 0.5
    done
    kill -0 "$w_pid" 2>/dev/null && die "s10-delegation: the killed authority (pid $w_pid) never exited"
    "$MWFLEET" mount 0 || die "s10-delegation: successor authority remount FAILED"
    t_up="$(date +%s)"
    log "successor authority up in $((t_up - t_kill))s; re-admitting the holder by remount (the documented posture)"
    "$MWFLEET" unmount "$cw" || true
    # A remounted co-writer is a FRESH membership acquire, which the
    # successor's grace window refuses by design (reclaim only — the
    # S6/S8 law this rung's cargo suite pins for the surviving-process
    # shape). Wait the window out, as an operator's retry would.
    local grace
    for ((tries = 0; tries < 90; tries++)); do
        grace="$(stat_field 0 membership_grace_remaining_ms)"
        [ -z "$grace" ] || [ "$grace" = "0" ] && break
        sleep 1
    done
    log "successor grace window closed (waited ${tries}s); re-admitting the holder"
    "$MWFLEET" mount "$cw" ||
        die "s10-delegation: co-writer could not re-admit under the successor era"
    wait_delegated_serving
    # One more live coherence round under the SUCCESSOR era.
    acked0="$(dfield 0 dlm_recall.dlm_revokes_acked)"
    touch "$w_mnt/s10-deleg/fresh-after-failover" || die "s10-delegation: post-failover create failed"
    acked1="$(dfield 0 dlm_recall.dlm_revokes_acked)"
    [ "$acked1" -gt "$acked0" ] ||
        die "s10-delegation: the successor's coherence law did not engage (acked flat)"
    drop_dentries
    stat "$cw_mnt/s10-deleg/fresh-after-failover" >/dev/null ||
        die "s10-delegation: STALE SERVE after failover — the re-earned delegation broke coherence"
    log "post-failover: delegations re-earned under the successor era, coherence law live"

    # ---- The oracle + tripwires ----------------------------------------------
    local out drift v
    out="$("$SQZ" fsck "$w_mnt" 2>&1)" || die "s10-delegation: online fsck FAILED:
$out"
    echo "$out" >"$rowdir/fsck.out"
    echo "$out" | grep -q "findings: 0" || die "s10-delegation: fsck findings != 0:
$out"
    drift="$(stat_field 0 meta_kv_block_refs_drift)"
    [ "$drift" = "0" ] || die "s10-delegation: meta_kv_block_refs_drift=$drift (C8 oracle RED)"
    for v in meta_ship.owner_panics meta_ship_publish.refusals invariant_tripwires \
        dlm_delegation.dlm_delegation_stale_serves dlm_delegation.dlm_delegation_recall_timeouts; do
        [ "$(dfield 0 "$v")" = "0" ] || die "s10-delegation: successor $v != 0"
    done
    [ "$(dfield "$cw" dlm_delegation.dlm_delegation_stale_serves)" = "0" ] ||
        die "s10-delegation: holder stale_serves != 0 (the must-stay-0 coherence tripwire)"
    [ "$(dfield "$cw" cowriter.local_commit_refusals)" = "0" ] ||
        die "s10-delegation: re-admitted holder local_commit_refusals != 0"
    for idx in $(member_idxs); do snap "$idx" 1 "$rowdir"; done
    log "s10-delegation GREEN (engagement $hits_d hits/$ship_d ships; coherence acked-before-publish; lever-off dark; kill-9 re-earn; fsck+C8 clean). Snapshots in $rowdir"
}

leg_s10_intents() {
    # Rung 13 (KD-MW-13; design §8.2 lever 1, the UPDATE arm): EXCLUSIVE
    # per-directory UPDATE grants + create-intent batches end-to-end on
    # the LIVE fleet — earn → local mints (zero wire) → fsync(dir) flush →
    # foreign visibility — plus the OQ-2 recall-forces-flush live round
    # (the owner's own ls forces the holder's flush), the lever-off A/B
    # control, the STORM row (the OQ-2 price — the reopening trigger's
    # live instrument, published never gated), and the TWO-SIDED MW-8
    # kill (pre-fsync: the acked-un-fsynced class, loss is a FIFO prefix,
    # never corruption; post-fsync: every name durable — fsync(dir) IS
    # the contract point), with the fsck/C8 oracle green after each kill.
    require_cowriters 1
    local rowdir cw w_mnt cw_mnt n
    rowdir="$STATE/rows/s10i-$(date +%s)"
    mkdir -p "$rowdir"
    cw="$(cowriter_idxs | head -1)"
    w_mnt="$(mnt_of 0)"
    cw_mnt="$(mnt_of "$cw")"

    ifield() { # idx key -> value-or-0 (the meta_ship_intent family nests)
        local v
        v="$(stat_field "$1" "$2")"
        echo "${v:-0}"
    }
    remount_cw() { # [env assignments...] — kill-free co-writer remount
        "$MWFLEET" unmount "$cw" || die "s10-intents: co-writer unmount failed"
        env "$@" "$MWFLEET" mount "$cw" || die "s10-intents: co-writer remount failed"
    }
    earn_grant() { # dir-path — one shipped create earns/refreshes the
        # grant; the client-face ABSORPTION gauge (authorities) is the
        # signal (the mkdir of the dir itself usually already earned it —
        # the created-dir preference — so the owner-face grant counter may
        # legitimately stay flat here).
        local d="$1" a0 m0 tries
        a0="$(ifield "$cw" meta_ship_intent.meta_ship_intent_authorities)"
        m0="$(ifield "$cw" meta_ship_intent.meta_ship_intent_mints)"
        touch "$d/earn-$RANDOM-$RANDOM" || die "s10-intents: the grant-earning create failed"
        for ((tries = 0; tries < 40; tries++)); do
            # Earned by THIS ship (gauge rose), or already held (the
            # create MINTED — e.g. the dir's own mkdir carried the grant).
            [ "$(ifield "$cw" meta_ship_intent.meta_ship_intent_authorities)" -gt "$a0" ] && return 0
            [ "$(ifield "$cw" meta_ship_intent.meta_ship_intent_mints)" -gt "$m0" ] && return 0
            sleep 0.25
        done
        die "s10-intents: no UPDATE authority absorbed on the co-writer (gauge flat at $a0)"
    }

    # ---- Phase 1: earn → mint (zero wire) → fsync(dir) → foreign visibility --
    mkdir -p "$w_mnt/s10i" || die "s10-intents: authority mkdir failed"
    local files=24 mints0 mints1 ship0 ship1 batches0 verbs0 defer0
    mkdir -p "$cw_mnt/s10i/d1" || die "s10-intents: co-writer mkdir failed"
    earn_grant "$cw_mnt/s10i/d1"
    mints0="$(ifield "$cw" meta_ship_intent.meta_ship_intent_mints)"
    ship0="$(ifield "$cw" meta_ship.shipped_verbs)"
    defer0="$(ifield "$cw" meta_ship_intent.meta_ship_intent_deferred_setattrs)"
    batches0="$(ifield "$cw" meta_ship_intent.meta_ship_intent_batches)"
    verbs0="$(ifield "$cw" meta_ship_intent.meta_ship_intent_verbs)"
    # The METADATA-plane instrument is create+utime (touch): the write
    # path's lease/publish ceremony is the DATA plane's (priced by the
    # tar-x row), and mixing it in here would gate rung 13 on rung-9
    # machinery it does not own. Measured live: touch = ZERO wire.
    for ((n = 0; n < files; n++)); do
        touch "$cw_mnt/s10i/d1/f$n" || die "s10-intents: minted create failed"
        touch -d @1700000000 "$cw_mnt/s10i/d1/f$n" || die "s10-intents: deferred utime failed"
    done
    mints1="$(ifield "$cw" meta_ship_intent.meta_ship_intent_mints)"
    ship1="$(ifield "$cw" meta_ship.shipped_verbs)"
    [ $((mints1 - mints0)) -ge $((files - 2)) ] ||
        die "s10-intents: only $((mints1 - mints0)) local mints across $files creates — the grant is not minting"
    # The zero-round-trip law: the metadata-plane mint span ships (near)
    # nothing — the ±12 allowance is the s8-a instrument-skew law's (the
    # .stats snapshot ceremony ships its own verbs).
    [ $((ship1 - ship0)) -le 12 ] ||
        die "s10-intents: the mint span SHIPPED $((ship1 - ship0)) metadata verbs — creates are not local"
    # One WRITTEN file composes the data plane (create mints, the write's
    # publish barriers on the pending ino — ungated here, priced by tarx).
    echo "payload" >"$cw_mnt/s10i/d1/withdata" || die "s10-intents: written mint failed"
    [ "$(ifield "$cw" meta_ship_intent.meta_ship_intent_deferred_setattrs)" -gt "$defer0" ] ||
        die "s10-intents: no setattr deferred into the batch (the tar utime shape is not engaging)"
    sync "$cw_mnt/s10i/d1" || die "s10-intents: fsync(dir) failed"
    # fsync(dir) GUARANTEES flushed-ness (the delayed release-kick may
    # already have shipped part of the span — that is the coalescer
    # working, not a miss): pending must be 0 and the span's intents must
    # all have travelled, in however many frames the window coalesced to.
    [ "$(ifield "$cw" meta_ship_intent.meta_ship_intent_pending)" = "0" ] ||
        die "s10-intents: pending != 0 after fsync(dir) — the contract point did not flush"
    local vd bd
    vd=$(($(ifield "$cw" meta_ship_intent.meta_ship_intent_verbs) - verbs0))
    bd=$(($(ifield "$cw" meta_ship_intent.meta_ship_intent_batches) - batches0))
    [ "$bd" -ge 1 ] || die "s10-intents: no flush frame shipped across the whole span"
    [ "$vd" -ge $((mints1 - mints0)) ] ||
        die "s10-intents: only $vd intents travelled for $((mints1 - mints0)) mints"
    # Foreign visibility after the contract point: the AUTHORITY sees
    # every name + the deferred times (owner-current serve; ls here also
    # exercises the OQ-2 gate against a now-empty queue).
    ls -1 --color=never "$cw_mnt/s10i/d1" >/dev/null || die "s10-intents: minter ls failed"
    [ "$(find "$cw_mnt/s10i/d1" -mindepth 1 -maxdepth 1 | wc -l)" = "$((files + 2))" ] ||
        die "s10-intents: the minter cannot list its own names"
    ls -1 --color=never "$w_mnt/s10i/d1" >/dev/null || die "s10-intents: authority ls failed"
    [ "$(find "$w_mnt/s10i/d1" -mindepth 1 -maxdepth 1 | wc -l)" = "$((files + 2))" ] ||
        die "s10-intents: post-fsync the authority does not see every flushed name"
    local mt
    mt="$(stat -c %Y "$w_mnt/s10i/d1/f0")"
    [ "$mt" = "1700000000" ] ||
        die "s10-intents: the deferred utime did not apply (owner mtime $mt != 1700000000)"
    log "phase 1: $((mints1 - mints0)) mints / $((ship1 - ship0)) shipped in the mint span; fsync flushed $vd intents in $bd frame(s) (coalesce $(python3 -c "print(f'{$vd/max(1,$bd):.1f}')")); foreign visibility + deferred times EXACT"

    # ---- Phase 2: OQ-2 recall-forces-flush, live ------------------------------
    # Phase 1's OWNER-side reads RECALLED d1's grant (the read gate — the
    # law working); re-earn before the minted shapes below.
    local rr0 ff0 sub mints2
    earn_grant "$cw_mnt/s10i/d1"
    sub="$cw_mnt/s10i/d1/sub"
    mkdir "$sub" || die "s10-intents: minted mkdir failed"
    echo x >"$sub/pending-child" || die "s10-intents: create into pending dir failed"
    rr0="$(ifield 0 meta_ship_intent.meta_ship_intent_read_recalls)"
    ff0="$(ifield "$cw" meta_ship_intent.meta_ship_intent_flush_forces)"
    mints2="$(ifield "$cw" meta_ship_intent.meta_ship_intent_mints)"
    echo y >"$cw_mnt/s10i/d1/unsynced" || die "s10-intents: pre-read mint failed"
    [ "$(ifield "$cw" meta_ship_intent.meta_ship_intent_mints)" -gt "$mints2" ] ||
        die "s10-intents: 'unsynced' did not MINT (the phase-2 shape needs a pending name)"
    # The foreign read: the OWNER's ls under the granted dir must recall
    # the holder (forcing its flush) and then serve the acked name — no
    # fsync ever ran for `unsynced`.
    ls -1 --color=never "$w_mnt/s10i/d1" >/dev/null || die "s10-intents: owner read failed"
    [ -e "$w_mnt/s10i/d1/unsynced" ] ||
        die "s10-intents: the owner's read missed an acked name (recall-forces-flush broken)"
    [ "$(ifield 0 meta_ship_intent.meta_ship_intent_read_recalls)" -gt "$rr0" ] ||
        die "s10-intents: the foreign read recalled nothing (read_recalls flat)"
    [ "$(ifield "$cw" meta_ship_intent.meta_ship_intent_flush_forces)" -gt "$ff0" ] ||
        die "s10-intents: the recall forced no flush (flush_forces flat)"
    [ "$(ifield "$cw" dlm_delegation.dlm_delegation_stale_serves)" = "0" ] ||
        die "s10-intents: stale_serves != 0"
    log "phase 2: OQ-2 live — the owner's read recalled, the flush applied, the acked name served"

    # ---- Phase 3: the STORM row (the OQ-2 price — published, not gated) ------
    local rounds=12 t0 t1 sr0 sf0 sd0 sto0
    mkdir -p "$cw_mnt/s10i/hot" || die "s10-intents: storm mkdir failed"
    earn_grant "$cw_mnt/s10i/hot"
    sr0="$(ifield 0 meta_ship_intent.meta_ship_intent_read_recalls)"
    sf0="$(ifield "$cw" meta_ship_intent.meta_ship_intent_flush_forces)"
    sd0="$(ifield 0 dlm_recall.dlm_thrash_demotions)"
    sto0="$(ifield 0 dlm_delegation.dlm_delegation_recall_timeouts)"
    t0="$(date +%s.%N)"
    for ((n = 0; n < rounds; n++)); do
        echo "$n" >"$cw_mnt/s10i/hot/w$n" || die "s10-intents: storm create failed"
        ls -1 --color=never "$w_mnt/s10i/hot" >/dev/null || die "s10-intents: storm read failed"
    done
    t1="$(date +%s.%N)"
    [ "$(ifield 0 dlm_delegation.dlm_delegation_recall_timeouts)" = "$sto0" ] ||
        die "s10-intents: storm recalls TIMED OUT under a live holder"
    {
        echo "== s10-intents STORM row (OQ-2's price — the reopening trigger's live instrument) =="
        echo "rounds=$rounds wall=$(python3 -c "print(f'{$t1-$t0:.2f}')")s per_round_ms=$(python3 -c "print(f'{($t1-$t0)*1000/$rounds:.1f}')")"
        echo "read_recalls_delta=$(($(ifield 0 meta_ship_intent.meta_ship_intent_read_recalls) - sr0))"
        echo "flush_forces_delta=$(($(ifield "$cw" meta_ship_intent.meta_ship_intent_flush_forces) - sf0))"
        echo "thrash_demotions_delta=$(($(ifield 0 dlm_recall.dlm_thrash_demotions) - sd0)) (the valve is the brake; demotion under a hot shared dir is DESIGNED)"
    } | tee "$rowdir/storm-row.txt"

    # ---- Phase 4: the lever-off A/B control -----------------------------------
    remount_cw SQUEEZEFS_UPDATE_INTENTS=0
    mkdir -p "$cw_mnt/s10i/off" || die "s10-intents: lever-off mkdir failed"
    local om0 og0 os0
    om0="$(ifield "$cw" meta_ship_intent.meta_ship_intent_mints)"
    og0="$(ifield "$cw" meta_ship_intent.meta_ship_intent_update_grants)"
    os0="$(ifield "$cw" meta_ship.shipped_verbs)"
    for ((n = 0; n < 8; n++)); do
        touch "$cw_mnt/s10i/off/f$n" || die "s10-intents: lever-off create failed"
    done
    [ "$(ifield "$cw" meta_ship_intent.meta_ship_intent_mints)" = "$om0" ] ||
        die "s10-intents: the LEVER-OFF control MINTED — the A/B is not dark"
    [ "$(ifield "$cw" meta_ship_intent.meta_ship_intent_update_grants)" = "$og0" ] ||
        die "s10-intents: the lever-off control was GRANTED — the A/B is not dark"
    [ $(($(ifield "$cw" meta_ship.shipped_verbs) - os0)) -ge 8 ] ||
        die "s10-intents: the lever-off control did not ship its creates"
    log "phase 4: lever-off control dark (0 mints/grants, creates shipped); restoring"
    remount_cw

    # ---- Phase 5: MW-8, BOTH SIDES of fsync(dir) ------------------------------
    # (a) PRE-fsync: the acked-un-fsynced class — the batch dies with the
    # client, loss is a FIFO PREFIX of the queue (release kicks flush in
    # order), and the store is CLEAN (fsck + C8 green). Loss here is the
    # DISCLOSED class, never an assertion of presence.
    local kdir="$cw_mnt/s10i/mw8" cw_pid tries present
    mkdir -p "$kdir" || die "s10-intents: mw8 mkdir failed"
    earn_grant "$kdir"
    for ((n = 0; n < 16; n++)); do
        echo "$n" >"$kdir/pre$n" || die "s10-intents: mw8 pre-fsync create failed"
    done
    cw_pid="$(awk -F'\t' -v i="$cw" '$1==i {print $7}' "$MEMBERS")"
    "$MWFLEET" kill "$cw" --sig 9
    umount -l "$cw_mnt" 2>/dev/null || true
    wait_for_unmounted "$cw_mnt"
    for ((tries = 0; tries < 120; tries++)); do
        kill -0 "$cw_pid" 2>/dev/null || break
        sleep 0.5
    done
    kill -0 "$cw_pid" 2>/dev/null && die "s10-intents: the killed co-writer (pid $cw_pid) never exited"
    # The applied set must be a FIFO prefix: if pre_i survived, every
    # pre_j (j<i) must have (batches apply in order; a hole = reordering).
    present=-1
    for ((n = 15; n >= 0; n--)); do
        if [ -e "$w_mnt/s10i/mw8/pre$n" ]; then
            present=$n
            break
        fi
    done
    for ((n = 0; n <= present; n++)); do
        [ -e "$w_mnt/s10i/mw8/pre$n" ] ||
            die "s10-intents: MW-8 pre-fsync loss is NOT a FIFO prefix (pre$n missing below pre$present)"
    done
    log "phase 5a: pre-fsync kill — $((present + 1))/16 applied (a FIFO prefix; the acked-un-fsynced class, disclosed)"
    local out drift
    out="$("$SQZ" fsck "$w_mnt" 2>&1)" || die "s10-intents: fsck after pre-fsync kill FAILED:
$out"
    echo "$out" >"$rowdir/fsck-mw8a.out"
    echo "$out" | grep -q "findings: 0" || die "s10-intents: fsck findings != 0 after pre-fsync kill:
$out"
    drift="$(stat_field 0 meta_kv_block_refs_drift)"
    [ "$drift" = "0" ] || die "s10-intents: drift=$drift after pre-fsync kill"

    # (b) POST-fsync: fsync(dir) IS the contract point — every name durable.
    for ((tries = 0; tries < 60; tries++)); do
        "$MWFLEET" mount "$cw" 2>/dev/null && break
        sleep 2
    done
    mountpoint -q "$cw_mnt" || die "s10-intents: co-writer re-admission failed after the kill"
    mkdir -p "$cw_mnt/s10i/mw8b" || die "s10-intents: mw8b mkdir failed"
    earn_grant "$cw_mnt/s10i/mw8b"
    for ((n = 0; n < 16; n++)); do
        echo "$n" >"$cw_mnt/s10i/mw8b/post$n" || die "s10-intents: mw8b create failed"
    done
    sync "$cw_mnt/s10i/mw8b" || die "s10-intents: mw8b fsync(dir) failed"
    cw_pid="$(awk -F'\t' -v i="$cw" '$1==i {print $7}' "$MEMBERS")"
    "$MWFLEET" kill "$cw" --sig 9
    umount -l "$cw_mnt" 2>/dev/null || true
    wait_for_unmounted "$cw_mnt"
    for ((tries = 0; tries < 120; tries++)); do
        kill -0 "$cw_pid" 2>/dev/null || break
        sleep 0.5
    done
    for ((n = 0; n < 16; n++)); do
        [ -e "$w_mnt/s10i/mw8b/post$n" ] ||
            die "s10-intents: post$n LOST after fsync(dir) returned — the contract point is broken (MW-8)"
    done
    log "phase 5b: post-fsync kill — 16/16 durable (fsync(dir) IS the contract point)"
    out="$("$SQZ" fsck "$w_mnt" 2>&1)" || die "s10-intents: fsck after post-fsync kill FAILED:
$out"
    echo "$out" >"$rowdir/fsck-mw8b.out"
    echo "$out" | grep -q "findings: 0" || die "s10-intents: fsck findings != 0 after post-fsync kill:
$out"
    drift="$(stat_field 0 meta_kv_block_refs_drift)"
    [ "$drift" = "0" ] || die "s10-intents: drift=$drift after post-fsync kill"
    for ((tries = 0; tries < 60; tries++)); do
        "$MWFLEET" mount "$cw" 2>/dev/null && break
        sleep 2
    done
    mountpoint -q "$cw_mnt" || die "s10-intents: final co-writer re-admission failed"

    # ---- The tripwires ---------------------------------------------------------
    local v
    for v in meta_ship.owner_panics meta_ship_publish.refusals invariant_tripwires \
        dlm_delegation.dlm_delegation_stale_serves; do
        [ "$(ifield 0 "$v")" = "0" ] || die "s10-intents: authority $v != 0"
    done
    [ "$(ifield "$cw" meta_ship_intent.meta_ship_intent_refusals)" = "0" ] ||
        die "s10-intents: meta_ship_intent_refusals != 0 on a healthy fleet (the must-stay-~0 gauge)"
    [ "$(ifield "$cw" cowriter.local_commit_refusals)" = "0" ] ||
        die "s10-intents: co-writer local_commit_refusals != 0"
    log "s10-intents GREEN (mint/flush/visibility; OQ-2 live; storm priced; lever-off dark; MW-8 both sides; fsck+C8 clean x2). Rows in $rowdir"
}

leg_s10_intents_tarx() {
    # Rung 13's MEASURED row (the rung-9 serial instrument, verbatim
    # venue): tar -x on the netns co-writer at wire RTT 250 µs, UPDATE
    # intents ON vs OFF, A-B-B-A (the store ages ~90 MB/arm). Publishes
    # entries/s + wire verbs/entry either way — row 14 owns the formal
    # ≤1.10x gate; this row is its input. Quiet-gate: refuse a loaded box.
    require_cowriters 1
    if pgrep -x cargo >/dev/null 2>&1; then
        die "s10-intents-tarx: a cargo build is running — the measured row needs a quiet box"
    fi
    local load
    load="$(awk '{print int($1)}' /proc/loadavg)"
    [ "$load" -le 4 ] || die "s10-intents-tarx: loadavg $load > 4 — the measured row needs a quiet box"
    local rowdir cw w_mnt cw_mnt tarball src entries
    rowdir="$STATE/rows/s10itarx-$(date +%s)"
    mkdir -p "$rowdir"
    cw="$(cowriter_idxs | head -1)"
    w_mnt="$(mnt_of 0)"

    tarball="$STATE/s10i-src.tar"
    src="${SQZ_MWMATRIX_TAR_SRC:-}"
    if [ -n "$src" ]; then
        [ -d "$src" ] || die "SQZ_MWMATRIX_TAR_SRC='$src' is not a directory"
        tar -cf "$tarball" -C "$(dirname "$src")" "$(basename "$src")"
        log "s10i-tarx instrument: REAL tree $src"
    else
        local synth="$STATE/s10i-tree" d f
        rm -rf "$synth"
        for ((d = 0; d < 120; d++)); do
            mkdir -p "$synth/d$d"
            for ((f = 0; f < 24; f++)); do
                head -c $((128 + (d * 24 + f) % 1900)) /dev/zero >"$synth/d$d/f$f.c"
            done
        done
        tar -cf "$tarball" -C "$STATE" s10i-tree
        log "s10i-tarx instrument: SYNTHESIZED tar-x shape (set SQZ_MWMATRIX_TAR_SRC=<dir> for a real tree)"
    fi
    entries="$(tar -tf "$tarball" | wc -l)"
    log "s10i-tarx tarball: $entries entries; venue = netns co-writer at netem 125us/end (250 µs RTT), A-B-B-A"

    tarx_arm() { # label intents(1|0) -> row line (also engagement-checked)
        local label="$1" lever="$2"
        "$MWFLEET" unmount "$cw" || die "s10i-tarx: unmount failed"
        SQUEEZEFS_UPDATE_INTENTS="$lever" "$MWFLEET" mount "$cw" --netns ||
            die "s10i-tarx: co-writer mount failed (lever=$lever)"
        "$MWFLEET" netem "$cw" 125us || die "s10i-tarx: netem failed"
        cw_mnt="$(mnt_of "$cw")"
        local out mints_d ship_d pub_d intents_d verbs_per
        out="$(s8a_venue "$rowdir" "$label" "$cw_mnt" "$cw" "$entries" "$tarball")"
        mints_d="$(s8a_delta "$rowdir" "$cw" "$label" meta_ship_intent.meta_ship_intent_mints)"
        ship_d="$(s8a_delta "$rowdir" "$cw" "$label" meta_ship.shipped_verbs)"
        pub_d="$(s8a_delta "$rowdir" "$cw" "$label" meta_ship_publish.shipped)"
        intents_d="$(s8a_delta "$rowdir" "$cw" "$label" meta_ship_intent.meta_ship_intent_verbs)"
        if [ "$lever" = "1" ]; then
            [ "$mints_d" -gt 0 ] || die "s10i-tarx $label: intents ON but 0 mints — the row did not engage"
        else
            [ "$mints_d" = "0" ] || die "s10i-tarx $label: intents OFF but $mints_d mints — the control is not dark"
        fi
        verbs_per="$(python3 -c "print(f'{($ship_d+$pub_d)/$entries:.2f}')")"
        echo "$out mints=$mints_d ship=$ship_d pub=$pub_d intent_verbs=$intents_d verbs/entry=$verbs_per"
    }

    local -a rows=()
    rows+=("$(tarx_arm int-on-1 1)")
    rows+=("$(tarx_arm int-off-1 0)")
    rows+=("$(tarx_arm int-off-2 0)")
    rows+=("$(tarx_arm int-on-2 1)")
    "$MWFLEET" netem "$cw" off || true

    echo ""
    echo "== S10 rung-13: serial tar -x, UPDATE intents ON vs OFF (entries=$entries; RTT 250us; A-B-B-A) =="
    printf '%-10s %-8s %-8s %s\n' ARM WALL_S OPS_S ENGAGEMENT
    local r
    for r in "${rows[@]}"; do
        # shellcheck disable=SC2086 # deliberate word split of the row line
        printf '%-10s %-8s %-8s %s\n' $r
    done | tee "$rowdir/s10i-tarx-table.txt"

    # The oracle after the measured sweep.
    local out drift
    out="$("$SQZ" fsck "$w_mnt" 2>&1)" || die "s10i-tarx: fsck FAILED:
$out"
    echo "$out" >"$rowdir/fsck.out"
    echo "$out" | grep -q "findings: 0" || die "s10i-tarx: fsck findings != 0:
$out"
    drift="$(stat_field 0 meta_kv_block_refs_drift)"
    [ "$drift" = "0" ] || die "s10i-tarx: drift=$drift"
    [ "$(stat_field 0 "meta_ship.owner_panics")" = "0" ] || die "s10i-tarx: owner_panics != 0"
    log "s10-intents-tarx PUBLISHED (table + snapshots in $rowdir)"
}

leg_s10_placement_tarx() {
    # Rung 14's FORMAL GATE (design §8.3, PR row 14): tar -x on the netns
    # co-writer at wire RTT 250 µs with placement + intents ON (the
    # shipped defaults) vs the authority-LOCAL S0 baseline — SAME venue,
    # binary, tarball, store; A-B-B-A (pl, local, local, pl) so store
    # aging cannot masquerade as a verdict. GATE: pl_median <= 1.10 x
    # local_median. A miss is a PUBLISHED honest outcome (exit 0 — the
    # charter's explicit alternative: operations.md + rc-manifest carry
    # the statement); an INVALID row (engagement, oracle) exits nonzero.
    require_cowriters 1
    if pgrep -x cargo >/dev/null 2>&1; then
        die "s10-placement-tarx: a cargo build is running — the measured row needs a quiet box"
    fi
    local load
    load="$(awk '{print int($1)}' /proc/loadavg)"
    [ "$load" -le 4 ] || die "s10-placement-tarx: loadavg $load > 4 — the measured row needs a quiet box"
    local src="${SQZ_MWMATRIX_TAR_SRC:-}"
    { [ -n "$src" ] && [ -d "$src" ]; } ||
        die "s10-placement-tarx: the FORMAL gate requires the REAL linux-src tree — set SQZ_MWMATRIX_TAR_SRC=<linux>/fs (a synthesized all-inline tree cannot exercise the oracles)"
    local rowdir cw w_mnt cw_mnt tarball entries
    rowdir="$STATE/rows/s10pl-$(date +%s)"
    mkdir -p "$rowdir"
    cw="$(cowriter_idxs | head -1)"
    w_mnt="$(mnt_of 0)"
    tarball="$STATE/s10pl-src.tar"
    tar -cf "$tarball" -C "$(dirname "$src")" "$(basename "$src")"
    entries="$(tar -tf "$tarball" | wc -l)"
    log "s10pl-tarx instrument: REAL tree $src ($entries entries); venue = netns co-writer at netem 125us/end (250us wire RTT) vs authority-local S0; A-B-B-A"

    # The co-writer at the gate's RTT, shipped-default levers (placement
    # AND intents ON — the row under test IS the default posture).
    "$MWFLEET" unmount "$cw" || die "s10pl: unmount failed"
    "$MWFLEET" mount "$cw" --netns || die "s10pl: netns co-writer mount failed"
    "$MWFLEET" netem "$cw" 125us || die "s10pl: netem failed"
    cw_mnt="$(mnt_of "$cw")"

    pl_arm() { # label -> row line (engagement-checked, both planes)
        local label="$1" out mints_d ship_d pub_d place_d verbs_per
        out="$(s8a_venue "$rowdir" "$label" "$cw_mnt" "$cw" "$entries" "$tarball")"
        mints_d="$(s8a_delta "$rowdir" "$cw" "$label" meta_ship_intent.meta_ship_intent_mints)"
        ship_d="$(s8a_delta "$rowdir" "$cw" "$label" meta_ship.shipped_verbs)"
        pub_d="$(s8a_delta "$rowdir" "$cw" "$label" meta_ship_publish.shipped)"
        place_d="$(s8a_delta "$rowdir" 0 "$label" meta_ship_placement.meta_ship_placement_client_slot_mints)"
        [ "$mints_d" -gt 0 ] || die "s10pl $label: 0 intent mints — the row did not engage the intent plane"
        [ "$place_d" -gt 0 ] || die "s10pl $label: 0 client-targeted mints on the owner — placement did not engage"
        verbs_per="$(python3 -c "print(f'{($ship_d+$pub_d)/$entries:.2f}')")"
        echo "$out mints=$mints_d ship=$ship_d pub=$pub_d place=$place_d verbs/entry=$verbs_per"
    }
    local_arm() { # label -> row line (the S0 shape)
        local label="$1" out
        out="$(s8a_venue "$rowdir" "$label" "$w_mnt" "" "$entries" "$tarball")"
        echo "$out local-S0"
    }

    local -a rows=()
    rows+=("$(pl_arm pl-on-1)")
    rows+=("$(local_arm local-1)")
    rows+=("$(local_arm local-2)")
    rows+=("$(pl_arm pl-on-2)")
    "$MWFLEET" netem "$cw" off || true

    # The shipped-topology dark posture, proven LIVE: on a one-authority
    # fleet no shipping client owns a metadata volume, so the policy's
    # migration half must never fire (the honest-statement's live face).
    local cand trig
    cand="$(stat_field 0 meta_ship_placement.meta_ship_placement_migration_candidates)"
    trig="$(stat_field 0 meta_ship_placement.meta_ship_placement_migrations_triggered)"
    [ "$cand" = "0" ] || die "s10pl: migration_candidates=$cand on a one-authority fleet (must be structurally 0)"
    [ "$trig" = "0" ] || die "s10pl: migrations_triggered=$trig on a one-authority fleet (must be structurally 0)"

    echo ""
    echo "== S10 rung-14 FORMAL GATE: tar -x, placement+intents ON @250us RTT vs authority-local S0 (entries=$entries; A-B-B-A) =="
    printf '%-10s %-8s %-8s %s\n' ARM WALL_S OPS_S ENGAGEMENT
    local r
    for r in "${rows[@]}"; do
        # shellcheck disable=SC2086 # deliberate word split of the row line
        printf '%-10s %-8s %-8s %s\n' $r
    done | tee "$rowdir/s10pl-table.txt"

    # The verdict (medians of two arms each; A-B-B-A agrees or the table
    # itself shows the ordering artifact).
    local pl1 pl2 lo1 lo2
    pl1="$(echo "${rows[0]}" | awk '{print $2}')"
    lo1="$(echo "${rows[1]}" | awk '{print $2}')"
    lo2="$(echo "${rows[2]}" | awk '{print $2}')"
    pl2="$(echo "${rows[3]}" | awk '{print $2}')"
    python3 - "$pl1" "$pl2" "$lo1" "$lo2" <<'PYGATE' | tee "$rowdir/s10pl-verdict.txt"
import sys
pl = (float(sys.argv[1]) + float(sys.argv[2])) / 2
lo = (float(sys.argv[3]) + float(sys.argv[4])) / 2
r = pl / lo
verdict = "GATE MET" if r <= 1.10 else "GATE NOT MET"
print(f"gate: co-writer {pl:.2f}s vs local {lo:.2f}s -> {r:.2f}x of S0 (gate <= 1.10x): {verdict}")
if verdict == "GATE NOT MET":
    print("the honest product statement governs (the charter's alternative): "
          "docs/operations.md #Metadata function shipping + docs/rc-manifest.md carry the measured number")
PYGATE

    # The oracle after the measured sweep.
    local out drift
    out="$("$SQZ" fsck "$w_mnt" 2>&1)" || die "s10pl: fsck FAILED:
$out"
    echo "$out" >"$rowdir/fsck.out"
    echo "$out" | grep -q "findings: 0" || die "s10pl: fsck findings != 0:
$out"
    drift="$(stat_field 0 meta_kv_block_refs_drift)"
    [ "$drift" = "0" ] || die "s10pl: drift=$drift"
    [ "$(stat_field 0 "meta_ship.owner_panics")" = "0" ] || die "s10pl: owner_panics != 0"
    log "s10-placement-tarx PUBLISHED (table + verdict + snapshots in $rowdir)"
}

leg_cowriters_admission() {
    if [ "$HOST_SCOPED" != "1" ]; then
        local reason="multi-identity (co-writer) legs need host-scoped fabric subsystems: this kernel merges controllers by subsysnqn ignoring hostnqn (nvme_core.multipath=Y), so co-located identities share one head — rung 5b (the sqz-kernel fix, validated in the rung-6b qemu guest) unlocks them. Stock-kernel workaround: nvme_core.multipath=N (boot parameter)"
        [ "$REQUIRE_HS" = "1" ] && die "--require-host-scoped-subsys: $reason"
        skip "$reason"
    fi
    die "host-scoped subsystems present, but the co-writer leg bodies land with rungs 7-10 (S6 arm onward) — this rung ships only the gate"
}

case "$LEG" in
smoke) leg_smoke ;;
multipath-negative) leg_multipath_negative ;;
s6-journal) leg_s6_journal ;;
s6-fence) leg_s6_fence ;;
s6-vm-fence) leg_s6_vm_fence ;;
s7-device-fence) leg_s7_device_fence ;;
s7-kill-matrix) leg_s7_kill_matrix ;;
s8-serial-ab) leg_s8_serial_ab ;;
s8-crucible) leg_s8_crucible ;;
s9-fanout) leg_s9_fanout ;;
s9-failover) leg_s9_failover ;;
s9-colocated-fence) leg_s9_colocated_fence ;;
s11-range) leg_s11_range ;;
s11-subblock) leg_s11_subblock ;;
s11-mpiio) leg_s11_mpiio ;;
s11-blockcyclic) leg_s11_blockcyclic ;;
s11-tiny) leg_s11_tiny ;;
s11-killrange) leg_s11_killrange ;;
s10c-fsck-scale) leg_s10c_fsck_scale ;;
s10c-kill-shard) leg_s10c_kill_shard ;;
s10-delegation) leg_s10_delegation ;;
s10-intents) leg_s10_intents ;;
s10-intents-tarx) leg_s10_intents_tarx ;;
s10-placement-tarx) leg_s10_placement_tarx ;;
cowriters-admission) leg_cowriters_admission ;;
vm-hostscope-validate) leg_vm_hostscope_validate ;;
vm-multi-identity) leg_vm_multi_identity ;;
*) die "unknown leg '$LEG' (smoke|multipath-negative|s6-journal|s6-fence|s6-vm-fence|s7-device-fence|s7-kill-matrix|s8-serial-ab|s8-crucible|s9-fanout|s9-failover|s9-colocated-fence|s11-range|s11-subblock|s11-mpiio|s11-blockcyclic|s11-tiny|s11-killrange|s10c-fsck-scale|s10c-kill-shard|s10-delegation|s10-intents|s10-intents-tarx|s10-placement-tarx|cowriters-admission|vm-hostscope-validate|vm-multi-identity)" ;;
esac
