# SqueezeFS 1.3.0 (unreleased)

_Release train 1.3 — not yet tagged. Everything below is on `dev`._

## 1.3.0 — what changed

**SPDK retired as an NVMe-oF target (forward-only).** Owner ruling
R-SYM-8 (2026-09-12, [docs/design-symmetric-metadata.md](docs/design-symmetric-metadata.md)
§5.8.1, KD-SYM-23; record
[.benchmarks/2026-09-13-sym-retire-spdk.md](.benchmarks/2026-09-13-sym-retire-spdk.md)).
The kernel `nvmet` target (the sqz-kernel target) is **THE** NVMe-oF target.
SPDK's compiled-in `SPDK_NVMF_MAX_NUM_REGISTRANTS = 16` was the only hard
registrant ceiling anything SqueezeFS shipped — the wall the symmetric
metadata program's fencing-group machinery existed to fit 12,500 hosts
under — and nvmet's registrant list is unbounded. The retirement is a
registrant-ceiling ruling, not a re-measurement; the 2026-07 dual-stack A/B
stands as history.

- **Deleted:** `src/nvmeof/spdk/` (the JSON-RPC client, the pinned v26.05
  build/install, hugepage management, the pidfile start/stop/status
  lifecycle, `save_config`/`load_config` persistence, `ptpl_file` pinning),
  the cross-stack live-state duplicate guard, `adopt_ambiguous`, and every
  SPDK-only flag (`--accept-version-drift`, `unshare --force`,
  `--hugemem-mb`, `--restore-prior`, `--core-mask`, `--cores`,
  `--dpdk-mem-mb` — a deleted flag dies on clap, never a silent accept).
- **Refuses loud, naming nvmet and the re-share sequence:**
  `--target-stack spdk`, `SQUEEZEFS_NVMEOF_TARGET_STACK=spdk` (the knob's
  only admissible value is now `nvmet`, its default; `spdk` is a RETIRED
  value the startup gate refuses with the same text), `nvmeof target
  install` (SPDK-only by definition — a retired verb naming `target setup`
  / `target start`), and the retired knobs `SQUEEZEFS_SPDK_TGT_BIN` and
  `SQUEEZEFS_NVMEOF_RUN_DIR`. Nothing falls back silently.
- **An SPDK share still in your share ledger is never re-presented.** The
  ledger schema is unchanged and the record stays decodable: `nvmeof list`
  shows it as `spdk — RETIRED target stack` with the re-share sequence,
  `nvmeof restore` reports it `skipped` **and exits nonzero until the record
  is removed** (`target start` and the nvmet oneshot unit stay red), `nvmeof
  unshare` removes ONLY its ledger record, and the duplicate-backing guard
  holds its backing until that record is gone. **The operator's re-share sequence:**
  1. `sudo squeezefs nvmeof unshare <subnqn>` — the SPDK ledger record is
     removed (no `spdk_tgt` is driven); the backing is released.
  2. If an `spdk_tgt` still serves the old subsystem, tear it down
     yourself (`rpc.py nvmf_delete_subsystem <subnqn>`,
     `rpc.py bdev_aio_delete <bdev>`) — SqueezeFS no longer speaks SPDK
     RPC.
  3. `sudo squeezefs nvmeof share <backing> --ip <ip> [--ns-uuid <uuid>]`
     — nvmet is the default; re-use the old `--ns-uuid` for initiators
     that must reattach under the same namespace identity.

  And the three things a pre-retirement SPDK host has that the sequence
  does not touch (`docs/operations.md` §NVMe-oF operations has the exact
  commands): the **old SPDK systemd unit** — its `ExecStartPost … --target-stack
  spdk` now refuses, so systemd stops `spdk_tgt` at the next boot and the SPDK
  shares go dark loudly; `systemctl disable --now` it and remove the file
  (**`nvmeof restore` / `target start` / the nvmet oneshot unit exit nonzero
  while any SPDK record remains in the ledger** — red until step 1 has run
  for every SPDK share); the **hugepage reservation** the retired
  `target setup --hugemem-mb` made — `--restore-prior` is gone, restore
  `nr_hugepages` by hand from `<state>/spdk/hugepages-prior`; and the
  **state-dir residue** under `<state>/spdk/` (`unshare` and `list` print its
  path) — `rm -r` it once every SPDK record is gone. A retired knob
  (`SQUEEZEFS_SPDK_TGT_BIN`, `SQUEEZEFS_NVMEOF_RUN_DIR`) still exported in a
  unit's `Environment=` or a shell profile refuses **every** `squeezefs`
  verb at startup, `mount` included — unset it.
- **Fidelity tier:** `tests/run_nvmeof_fidelity.sh`,
  `tests/nvmeof_target_substrate.sh` and `tests/guard_smoke.sh` run on
  nvmet alone (the SPDK legs and the PTPL power-cycle leg are gone; the
  loud-fail matrix grew the retirement refusals; reservation persistence
  across a TARGET restart stays nvmet's G2 leg — nvmet has no PTPL by
  design, so `writer_guard_pr_reacquires` growth across a target power
  cycle is expected there). Contracts: `tests/nvmeof_retire_spdk_tests.rs`.
- **Unchanged:** the writer guard's register ladder and its spec-strict
  Register contracts (measured on kernel nvmet — and on SPDK v26.05 before
  its retirement), the `nvmeof connect`/`disconnect` initiator half, the
  ledger's §6.4 laws, and every nvmet share verb.

**S9 multi-writer authority: a clean unmount now releases its data-namespace
WERO registration before exit.** (Symmetric PR 12, found by the fidelity
tier's new `sym-join-ladder` leg on the real nvmet target; record
[.benchmarks/2026-09-16-sym-pr12-mount-posture.md](.benchmarks/2026-09-16-sym-pr12-mount-posture.md).)
Every clean unmount of a `SQUEEZEFS_MULTI_WRITER=1` authority left its OWN
registrant key on every data namespace: the custody sweep kept a reference
to the WERO hold alive across its 10 s cadence, and the release ioctl runs
in the hold's drop — which died with the process. `nvme resv-report` listed
the departed writer's key until the next mount's register ladder recovered
it as own-stale; **no fence was lost** (the hold's reservation went with the
process's association; the residue was the symptom), but zero residue after
a clean leave is the fidelity tier's law and the shipped binary broke it.
The sweep now observes the hold through a weak reference and the disarm's
own release is the last one. Contracts: `dlm_multi_writer_tests`
(`a_shipped_authoritys_clean_leave_releases_its_data_namespace_wero_before_returning`,
the flat layout) and `sym_mount_posture_tests` (the armed layout).

**The job wire's enrollment secret (`job:enroll`) is minted ONCE per set and
reused by every later coordinator.** (Symmetric PR 12b, found by the
`sym-crash` fleet leg — a manager failover with joined writers mounted;
record
[.benchmarks/2026-09-17-sym-pr12b-n-daemon.md](.benchmarks/2026-09-17-sym-pr12b-n-daemon.md).)
Every coordinator start used to write a FRESH 32-byte secret over the set's
`job:enroll` record — an accidental rotation on every restart that nothing
documented and nothing relied on for security: under ruling D2 (possession
of volume access IS membership) a member that can read the record is
already inside the trust boundary, so a permanent per-set secret adds no
reach. It DID break every member that outlives a coordinator's incarnation
(a joined writer, an S9 co-writer, an S6 member reader): its sessions to the
successor failed `mac invalid` for the member's life, and on the fleet the
joiners parked at `T_self` against a successor whose grace window would have
admitted them. The record is now read first: absent ⇒ minted; present and
well-formed ⇒ reused verbatim; present but corrupt (another length,
undecodable) ⇒ the coordinator REFUSES loud naming the record; an I/O error
⇒ propagated, never re-minted. **Operators who rotated the secret by
restarting the coordinator** now rotate it by REMOVING the record on a
quiesced set (every member re-enrolls at its next arm). Contract:
`job_wire_tests::a_coordinators_restart_keeps_the_sets_enroll_secret`
(layout-blind).

**The S9 co-writer's custody renewal retries a FAILED renewal at the cadence
law, never a 25 ms storm.** (Symmetric PR 12b review round 3, Issue 22 —
found by the `sym-crash` fleet leg at every manager failover; an UNARMED
S9 surface the shipped co-writer shares; record
[.benchmarks/2026-09-17-sym-pr12b-n-daemon.md](.benchmarks/2026-09-17-sym-pr12b-n-daemon.md).)
The renewal loop (`spawn_custody_renewal`) slept a fixed 25 ms after a
failed renewal, so a dead authority was dialed ≈ 80 times a second (two
dials per attempt — the verb's one reconnect-and-resend) with two warnings
per attempt for the whole `T_self` window. The retry now waits
`clamp(remaining-to-T_self / 3, 100 ms, one renewal cadence)` — three
retries always fit before the deadline, none comes faster than the floor —
and the §6.7 fence is byte-identical: `T_self` from the LAST successful
renewal, never from a failed dial (the pin measured 66 → 14 dials over the
window, the fence unmoved). Under the ARMED symmetric plane the per-holder
custody client also re-resolves its holder off durable state at every paced
retry (tree 0's lessee → its published listener) and fences at once when
the holder MOVED — the owner's word that the lease died with its listener
(`dlm_custody_holder_moves`; 0 on every unarmed mount, where no resolver is
installed). New gauge `dlm_custody_renew_retries`. Contract:
`membership_liveness_tests::a_custody_renewal_at_a_dead_authority_paces_its_dials_and_fences_at_t_self`
(the flat, shipped shape).

**A membership member that the successor's durable roster cannot name — an
S5 `-o ro` reader — re-asserts its lease at a successor until the grace
DEADLINE instead of self-fencing.** (Symmetric PR 12b review round 2,
Issue 26 — the `sym-crash` fleet's reader fenced at every manager
failover; an UNARMED S6 surface every layout reaches under
`SQUEEZEFS_MEMBERSHIP_BIND`.) The failover grace window closed BOTH of its
halves the moment the last claim-set writer had re-asserted, so a RAM-only
member (a reader is in no claim set, so it is never among the members the
window awaits) polling the rendezvous one beat later met "no grace window
is open": `UnknownLease`, a purge of every cached block, a fresh join —
and `membership_self_fences` moving on a healthy failover. The window is
now two halves on one deadline: the fresh-refusal half protects the
durable roster and still closes early (`membership_grace_remaining_ms`
reads it, unchanged); the re-assertion half admits a member presenting a
prior lease epoch until the deadline (the lease TTL, ≥ every member's
`T_self` — the only bound a RAM-only member's own clock can meet; the trust
model is unchanged, since a prior epoch is presented only by a member that
renewed within its own `T_self`). A successor whose predecessor left no
writer in the claim set opens the same deadline-bounded re-assertion with
nothing refused. **`membership_self_fences` is flat across a manager
failover on every member kind**, readers included. Contract:
`dlm_membership_tests::a_ram_only_members_reclaim_is_admitted_until_the_deadline_after_the_writers_closed_the_window`
(layout-blind; the `sym-crash` leg asserts the reader per round).

**A reader's freed-offset acknowledgement ladder restarts per owner era.**
(Symmetric PR 12b round 5 — found by the `sym-crash` leg's new reader
assertions; a pre-existing §6.8 item-3 defect on every layout under
`SQUEEZEFS_MEMBERSHIP_BIND`, hidden until now by the reader's self-fence at
each failover happening to land after the successor's clock had caught up.)
A freed-offset label is the OWNER's own monotonic instant, so a
successor's label space restarts near 0 — but the reader's ack ladder kept
a process-global monotone memo of the highest label it acknowledged under
the predecessor and adopted no label below it. Under a successor the reader
therefore acknowledged NOTHING until the successor's clock had run past the
predecessor's uptime: the successor's `membership_min_acked_free_epoch` sat
at 0, every deferred free stayed in the grace ring, and the pressure valve
refused `StorageFull` on a healthy fleet ("readers have not acknowledged
past label 44556" one failover later). A grant from a NEW owner term now
resets the ladder (`membership_min_acked_free_epoch` advances under a
successor within one qualification cycle); a same-term reclaim keeps it —
one label space. Contract:
`reader_free_grace_tests::a_new_owner_terms_labels_are_acknowledged_from_scratch`
(layout-blind).

**A checkpoint flush that appended a PARKED frozen delta took the dirty
floor of the records applied since (every layout).** (Symmetric PR 13,
defect 6, `e5a9210b` — found at N = 8 through a joined appender's
routinely refused SMO extents; reachable on a flat volume whenever an SMO
fails mid-way.) An SMO that froze a node for its fold (`freeze_for_smo`)
and FAILED before its swap — `ENOSPC` or a journal-reserve refusal on a
flat volume, a refused extent grant on a joined appender — left its frozen
delta parked; the next checkpoint flush step got that delta back, cleared
the node's WHOLE dirty floor, and appended the parked delta alone, so every
record committed into the open overlay meanwhile stayed RAM-only:
invisible to every later flush and to the clean unmount's coverage test,
and gone with the process (the storm read 21–280 of 192,000 acked unlinks
resolving again after a clean leave, always the last ones). 1.3.0 restores
the floor of the records still in the open delta in the same lock window
(`NodeDirty::overlay_floor`; `meta_kv_flush_floor_kept` counts the
engagement — 0 on a mount whose SMOs never fail mid-way). Pinned red-first
on the flat harness by
`kv_freeze_wedge_tests::a_flush_of_a_parked_frozen_delta_keeps_the_floor_of_the_records_applied_since`
(the suite runs on both layouts in the sym matrix) and by the eight-writer
storm pin. Record: `.benchmarks/2026-09-19-sym-acceptance.md` §4.3.

**A mixed sync/async `scc` bucket acquisition in the KV node loader could
wedge a mount's two metadata lanes for ever.** (Symmetric PR 12b review
round 1, Issue 13 — pre-existing on every layout, made ordinary by the
N-daemon posture's fsck C1 population; the fleet's `-o ro` reader hung
`squeezefs fsck` for 31 minutes and its own `umount` with it.) The
single-flight table was taken async by the loader and sync by the loader
guard's drop; `saa` hands a released bucket to a queued async waiter that
resumes only when polled, and both lanes that could poll it were parked in
a sync wait on that bucket. Every node-cache table is now taken in ONE
style (sync — no bucket is ever held across an await); the static rail
`kv_loader_lock_style_tests` keeps it so, and the fleet fsck's collect
loop is bounded by the shard lease (a wedged worker's shard is retired and
re-run locally) so `squeezefs fsck` always returns.

---

# SqueezeFS 1.2.4

_Release date: 2026-09-11 (tag `stable-2026.09.4`)_

1.2.4 is a point release on the 1.2 train: the metadata tree can now
**shrink**, a **full metadata volume answers ENOSPC** instead of failing, and
three defects the 1.2.3 binary ships are fixed — a large-file delete that
leaked its whole block ledger, a stale read through the interposer's
direct path that could return another file's bytes, and the fail-stop
itself. Nothing on disk changes; the cluster wire is unchanged from 1.2.3
(publish schema 17, cluster wire 4).

## 1.2.4 — what changed

**Upgrading from 1.2.3.** No on-disk format change and no wire change. The
same-commit fleet rule (KD-7) still governs a volume set's daemon, shim,
authority, co-writers and readers. A 1.2.3 volume mounts as-is; the first
1.2.4 mount of a full or delete-heavy volume may run the new merge sweep
(see below) and return extents over its first checkpoint cycles.

**The metadata tree shrinks — leaf merge (the headline).** Design
[docs/design-cow-kv-metadata.md §4.6a](docs/design-cow-kv-metadata.md);
record [.benchmarks/2026-09-11-kv-leaf-merge.md](.benchmarks/2026-09-11-kv-leaf-merge.md).
Since format v3 landed, deletes returned record space inside a leaf but
never the leaf's 256 KiB extent — a metadata volume filled once could never
give its extents back, however much was deleted. Now two adjacent underfull
leaves merge into one successor (the split's own ¾-fill target read
backwards, so a split is never undone by the next merge), interior nodes
merge the same way, and a root with one child collapses — the tree's height
decreases. Each merge is a structural operation under the existing §4.6
protocol (successor written and barriered before any lock; interior-pointer
and free records reserved inside the lock window; the freed extents return
through the pending-free protocol one or two checkpoint cycles later), so
no new record kind and no incompat bit.

- **What it does on a full volume:** fill a small volume to ENOSPC, delete
  90 % of its files spread across leaves, and creates resume for **938 of
  940** (1.2.3: 4) — 313 merges, free extents 11 → 325. Convergence is
  proven, not asserted: a height-3 tree with underfull leaves on both sides
  of every parent boundary reaches its fixed point in 4 passes against a
  derived bound of 7, with 0 stranded pairs.
- **Three triggers, all derived:** the flush pass checks an underfull leaf's
  sibling; a full heap runs a bounded recovery sweep (level by level, then
  the collapse chain); `squeezefs defrag --meta` runs the same walk under
  the job throttle. The sweep is bounded to one checkpoint tick per cycle
  and resumes from a cursor, so it never stalls the checkpoint cadence;
  measured at 2.2 µs per leaf on 4 KiB-value leaves and 50–58 µs on dense
  inode/dentry leaves (a 2,048-node cache = 2–3 cycles per lap).
- **Gauges:** `meta_kv_node_merges`, `meta_kv_interior_merges`,
  `meta_kv_root_collapses`, `meta_kv_merge_candidates` (exact),
  `meta_kv_merge_sweeps`/`_laps`, `meta_kv_merge_sweep_ns`,
  `frag_d4_mergeable_leaves`, `defrag_meta_merges`. No knob.

**A full metadata volume answers ENOSPC, never a fail-stop.** 1.2.3 (and
every 1.2 before it) marked a metadata volume FAILED when its heap filled —
EIO on every operation, `writeback error latched`, recovery = remount.
Root cause: the reserve meant for the checkpoint's compaction had no
user-side claimant, so acked user growth spent it through the checkpoint's
own node splits, and the wedged-tail audit read a full heap as a wedge.
Now the commit pass promises each leaf's eventual structural extents against
the claimable budget before anything is reserved; a commit that cannot be
promised refuses **`ENOSPC`** alone; reads, deletes, overwrites and every
in-place commit keep landing; and the wedged-tail bound distinguishes a
*space standstill* (counted, clears when budget returns — with leaf merge,
when deletes return extents) from a genuine wedge (FAILED). Gauges
`meta_kv_heap_full` (0/1), `meta_kv_enospc_refusals`, `meta_kv_heap_full_cycles`,
`meta_kv_heap_promised`. Record
[.benchmarks/2026-09-11-post-123-board-items-2-4.md](.benchmarks/2026-09-11-post-123-board-items-2-4.md).

**Fixes to paths 1.2.3 ships.**

- **Every unlink of a file with ≳ 3,000 block references (≈ 11.6 GiB at 4 MiB
  blocks) orphaned its whole block ledger** (`d7573e5b`): the release of the
  references was a standalone commit that was never chunked to the journal's
  entry cap, so it failed past ~3,000 records, the destroy then succeeded, and
  the references outlived their owner — the blocks could never free (a
  permanent leak fsck C8 reported forever). The release now rides the
  destroy's own transaction (one journal entry per reclaimed inode instead of
  N + 1), a failed release destroys nothing, and a single inode whose destroy
  exceeds the cap is destroyed across entries with its layout and record
  last.
- **The interposer's direct-drive read could return another file's bytes**
  (`445525e1`): the whole-block direct path had no dead-lifetime screen, so a
  stale block key (its offset freed and reissued while the layout cache still
  named it) passed the completion revalidation and the read returned the
  reissued offset's contents. Rare — the handler path's rebind counters
  (`stale_binding_rebinds`) are its rate — but a correctness class. The
  screen now covers both direct-drive arms; a stale key falls back to the
  handler, which answers the handler's own law.
- **The interposer reads packed small files on its direct path** (PK8,
  `445525e1`): 1.2.3 sent every packed tenant to the handler; now a
  passthrough tenant plans one ranged read on the mapping's own backend,
  bounded to the tenant (never a neighbour's bytes). `SQUEEZEFS_IPC_DD_PACKED`
  defaults on (`0` = the 1.2.3 handler fallback); 99.9 % of a 20,000-file
  shim randread went direct locally at par IOPS.

**Harness.** The external suites' source trees (xfstests, LTP, pjdfstest)
live in a durable cache instead of `/tmp`, with a marker-based liveness check
and the autotools resolved from the store on nix hosts — the 1.2.3 chain's
LTP leg had stalled on a checkout hollowed by the `/tmp` age cleaner.

**Known limitations — what this release does not claim.**

- **Scale.** Unchanged: metadata authority is per volume — one owner mount
  commits each metadata volume's changes (one for the whole set by default,
  up to 16 with the offline `volume set-owners` split); co-writer mounts
  write data directly and ship each metadata change to the owner; read-only
  mounts see a bounded delay. Evidence tiers in
  [docs/rc-manifest.md](docs/rc-manifest.md#2-guarantee-table-by-evidence-tier-ruling-d1).
- **Leaf merge never crosses a parent boundary directly**; underfull leaves
  under different parents become siblings through interior merges. The one
  shape that cannot: two adjacent interiors whose separator folds together
  exceed a node's fill target — bounded to at most one stranded leaf pair per
  such boundary (`1/fanout` of the population), cleared as soon as either
  parent shrinks.
- **A full metadata volume with nothing mergeable stays full** — the data is
  simply there; grow the set (`volume add-meta`).
- **Compaction and `defrag --meta` are operator-driven**; the heap-full sweep
  is the only automatic merge trigger beyond the flush pass.
- **The 1.2.3 limitations** on the co-writer venue's capacity and the
  partial-block fsync escalation stand.

**Verification.** Every leg ran on the tested tree `fe4c9390` — `task check`
(380 suites, 4,961 tests), fstests `-g auto` (787 ran, 783 clean, 4
expected-shape, 0 unexpected; `generic/650` excluded on the gate laptop),
pjdfstests (8,798), LTP (1,884 pass, 0 fail — unattended through the durable
suite tree), require-mount, zc-capability (187 tests, empty skip ledger), and
the fuzz campaign (12 targets, 399 M execs, 0 crashes). The chain ran once;
its zc leg was resumed after a venue condition (the gate disk at 98 % — one
bench pin needs 68 GiB free; build directories cleared), the product under
test unchanged. Record:
[.benchmarks/2026-09-11-1.2.4-release-gate.md](.benchmarks/2026-09-11-1.2.4-release-gate.md).

The rest of this document is the 1.2.3, 1.2.2, 1.2.1 and 1.2.0 record,
which 1.2.4 inherits.

---

# SqueezeFS 1.2.3

_Release date: 2026-09-11 (tag `stable-2026.09.3`)_

1.2.3 is a point release on the 1.2 train with one headline and three
data-loss fixes. The headline is **small-file packing**: files between one
page and half a block no longer cost a whole 4 MiB block each when they
leave the local staging ring — they share one block, so a 20,000-file tree
that used to occupy 80 GiB on the shared store occupies 592 MiB, and every
client reads those files byte-exact through the ordinary striped path. The
three fixes are to paths that shipped in 1.2.2 and could lose acked bytes:
a promoted small file whose block a remount could hand to another file, a
kernel-split write segment discarded by an unguarded read-modify-write, and
a mount-time sweep that freed live blocks. Nothing on disk changes; the
cluster wire does (publish schema 17, cluster wire 4).

## 1.2.3 — what changed

**Upgrading from 1.2.2.** No on-disk format change — the superblock and its
incompat bits are untouched; packed files ride the size-carrying `bk:off:len`
mapping every 1.2 binary already decodes, so a volume packed by 1.2.3 reads
byte-exact under 1.2.2 (and under `SQUEEZEFS_SMALL_FILE_PACKING=0`). The
**cluster wire changed**: the publish plane's schema went 16 → 17 (the
`pack_group` frame flag and its two owner refusals) and `CLUSTER_WIRE_SCHEMA`
3 → 4 (the membership grant advertises `pack_group_available`); the shim's
`IPC_ABI` stays 6. The same-commit fleet rule (KD-7) governs: **the daemon,
the shim, and every authority, co-writer and reader of one volume set
upgrade together.** A plain single-writer mount arms none of these planes.
The wire change is why this release's gate includes the fuzz campaign.

**Small-file packing (the headline).** Design
[docs/design-small-file-packing.md](docs/design-small-file-packing.md)
(five review rounds); acceptance
[.benchmarks/2026-09-10-packing-rows-squeeze-test.md](.benchmarks/2026-09-10-packing-rows-squeeze-test.md).
A file between the inline ceiling (4 KiB) and 4 MiB lives in the local
staging ring until pressure, a clean unmount, or (opt-in) `fsync` promotes it
to the shared store. 1.2.2 promoted each such file into its own whole 4 MiB
block — 64× the data for a 64 KiB file, and a 480 GiB set filled by 140,000
small files in eight seconds. 1.2.3 promotes them into the volume's shared
**open pack block**: one LBA-aligned slot per file, one DMA, one layout
commit that publishes the `bk:off:len` mapping and the file's durable block
reference in the same transaction. N tenants of a block are N references
in the durable ledger; the block frees only when the last one goes — the
arithmetic 1.2 already had, so **no new incompat bit**.

- **On the acceptance box (A-B-B-A, same binary, lever as the arm):** 96,000
  × 16 KiB create + fsync-on-close with promotion at fsync — files/s +0.5 %,
  fsync −3.7 %, daemon CPU/file −2.2 %, device/user bytes 1.00× on both arms,
  and **375 blocks instead of 96,000**; 20,000 small files promoted at unmount
  — **148 blocks instead of 20,000** at the same 2 s unmount wall, all 20,000
  byte-exact from a second mount point; oracle drift 0, fsck 0 findings,
  tripwires 0 on every position. fstests' standing regression set passes
  under both postures.
- **Existing volumes recover their space with one command.** A volume the
  1.2.2 dismount pass filled at one block per file is the compaction mover's
  first customer: `squeezefs defrag --pack` re-packs the live tenants
  (`--report-only` measures first — `frag_d1_pack_*`, `pack_reclaimable_bytes`)
  — **20,000 → 147 blocks in 5.5 s** on the box, every file intact. Deletes
  leave half-empty packs; the same mover compacts them (15 → 5 locally).
- **Every tenant operation is defined and pinned:** passthrough truncate-shrink
  is a pure mapping re-description (zero device writes); a transformed shrink
  lands a new tenant; a clone of a promoted file shares the window; overwrite
  goes back through the staging ring; `move_one` defers an open pack and seals
  it when a drain targets its volume; co-writers pack per (owner, home meta
  volume) as one atomically-enqueued conveyor group, falling back to
  one-block-per-file where the authority cannot serve the group frame; fsck
  gained class **C12** (tenant-range consistency, report-only) and the census
  a pack-open ledger.
- **`SQUEEZEFS_SMALL_FILE_PACKING`** defaults **on**; `0` is the A/B control
  and the rollback — the one-block-per-file arm, byte-identical to 1.2.2.
  `SQUEEZEFS_PACK_MAX_SLOT_BYTES` (derived: half the block) is the one law
  with two faces: a stored image above it takes its own block, and a pack
  whose live bytes fall at or below it is a compaction victim. Operator
  rows: [docs/operations.md → Environment knobs](docs/operations.md#environment-knobs--the-complete-registry).
- **The interposer is unaffected by construction:** the shim carries no
  layout logic, and the direct-drive read prelude hands decorated mappings to
  the handler path (the branch clones always took). A cold shim read of a
  packed small file therefore skips the direct-drive fast path — the same
  class 1.2.2's promoted files had.

**Data-loss fixes on the 1.2.2 path.** Each landed red-first with its own
contract; each is independent of packing (packing exposed two of them by
making one shared block live at every mount).

- **A promoted small file's block was not in the durable ledger** —
  FIND-PK-2 (`bfcf1e57`): the staged-file promotion published its mapping
  without its block reference (the 2026-08-02 ledger wiring named the site
  and never wired it), so on a volume with any striped file the next mount
  recovered every promoted block **free** and the next striped write
  overwrote promoted files (200/200 drift, 8 of 200 files clobbered by four
  writes in the repro). Since 1.2.2's dismount pass, that fired at every clean
  unmount of a mount holding staged files. The reference now rides the
  promotion's own commit. `.benchmarks/2026-09-09-promotion-durable-ref-hole.md`
- **A kernel-split write segment was discarded by an unguarded
  read-modify-write** (`d914b673`): a buffered file just over one block is
  written back as five concurrent 1 MiB WRITEs; when one segment's
  classification went stale across a sibling's staged→striped promotion, the
  router's `write_striped` — the one striped publisher that ran without the
  block guard — seeded from the device, never composed the open device-overlay
  record another segment had left, and published a merge that made the
  one-authority screen supersede that record's acked bytes: **1 MiB of zeros
  after a successful fsync**, ≈ 1 % of such files. `write_striped` is deleted;
  every caller re-dispatches through the single guarded striped path.
  `.benchmarks/2026-09-10-overlay-vs-growth-merge-data-loss.md`
- **The mount-time corpse sweep freed live blocks** (`86eff517`): the sweep
  released each never-forgotten unlinked inode's references and blocks, then
  destroyed all their records in ONE transaction; past the journal's entry cap
  that destroy failed, the records survived with their references gone, and
  the next mount's sweep decremented whichever **live** file now held each
  stale offset — at zero the live block was punched under its layout (fstests
  `generic/749` read a packed file as zeros; the 1.2.2 code has the same
  double release wherever a re-minted block sat at a stale corpse's offset).
  A release now decrements RAM only for a durable record it **witnessed**
  existing, a failed release frees nothing, and the sweep's destroys are
  chunked to the cap with release → free → destroy per chunk (68,099 corpses
  drained in ~1 s on the failing volume). `.benchmarks/2026-09-11-corpse-sweep-double-release.md`
- **Two smaller ledger holes beside packing:** a failed promotion save
  re-noted its own reference (a durable record to an abandoned block —
  `retract_block_ref_ops`, FIND-PK-4), and the staged read-modify-write and
  inline write arms released a superseded copy's RAM reference without its
  durable `−ref` (FIND-PK-5). Also fixed: promoted files placed on a
  non-default data volume read as **zeros from every other client** (the
  read arm used the default device — FIND-PK-0; one routed funnel now serves
  every decorated mapping), and `defrag --report-only` on a large legacy
  volume answered "reply too large" (FIND-PK-6; the report is bounded on the
  admin lane like fsck's).

**Dismount behaviour.** A clean unmount now promotes every resident
staged-layout file to the shared store (`dismount_promoted_*`), so files a
client wrote but never fsynced become visible to other clients at unmount
instead of living only in that client's staging ring; the "unflushed staged
files" NOTE 1.2.2 recorded is gone. `SQUEEZEFS_FSYNC_PROMOTE_STAGED` (default
off) promotes at `fsync` instead — priced at −3.7 % per fsync under packing;
an operator with a hard "fsync'd means visible everywhere" requirement turns
it on. The inline ceiling is a derived one page (`SQUEEZEFS_INLINE_MAX_BYTES`
raises it up to the format bound; the sweep that priced a raise out is
`.benchmarks/2026-09-09-inline-raise-sweep-local.md`).

**Known limitations — what this release does not claim.**

- **Scale.** Unchanged from 1.2.2: one write mount per volume set, any
  number of read-only and opt-in co-writer mounts; every scale claim carries
  its evidence tier in [docs/rc-manifest.md](docs/rc-manifest.md#2-guarantee-table-by-evidence-tier-ruling-d1).
- **A full metadata volume fail-stops** (`Metadata volume N is disabled`,
  EIO) instead of answering ENOSPC — seen when the inline-raise sweep
  exhausted a 1 GiB meta volume. Pre-existing, named, not fixed.
- **Compaction is operator-driven.** `defrag --pack` runs on the job fabric
  when invoked (throttled, pause/resume/cancel); no automatic trigger ships.
- **A cold shim read of a packed small file** takes the handler path, not the
  direct-drive fast path (a performance class, not a correctness one).
- **The 1.2.2 limitations** on the co-writer venue's capacity, partial-block
  fsync escalation, and the unnamed lost-wake loser stand.

**Verification.** Every leg of the release gate ran on the tested tree
`86eff517` — `task check` (377 suites, 4,926 tests), fstests `-g auto` (787
ran, 783 clean, 4 expected-shape, 0 unexpected; `generic/650` excluded on
the gate laptop as in 1.2.2), pjdfstests (8,798), LTP (174 syscall tests
pass, 0 fail), require-mount, the zc-capability leg on the sqz kernel (187
tests, empty skip ledger), **and the fuzz campaign** (12 targets, 401 M
execs, 0 crashes). The chain ran three times: the first stopped in `task
check` on a test whose premise was the pre-packing default, the second in
fstests on `generic/749` — the corpse-sweep fix above — and the third is
the release (its LTP leg was resumed once after the LTP source tree aged out
of `/tmp`; the product under test was unchanged). Record:
[.benchmarks/2026-09-11-1.2.3-release-gate.md](.benchmarks/2026-09-11-1.2.3-release-gate.md).

The rest of this document is the 1.2.2, 1.2.1 and 1.2.0 record, which 1.2.3
inherits.

---

# SqueezeFS 1.2.2

_Release date: 2026-09-08 (tag `stable-2026.09.2`)_

1.2.2 is a point release on the 1.2 train whose headline is the multi-writer
data path: the co-writer bugs the single-node proving fleet surfaced — a
write-path hang, a supply leak, a live-mount data loss, a read storm on the
authority — are fixed, each pinned by a test that reproduces it, and the S11
shared-file gate that exposed them passes its full matrix on the acceptance
box. Eight performance campaigns land — five with field rows on the
acceptance box — and one kernel-side patch joins the sqz series. Nothing on
disk changes; the cluster wire does.

## 1.2.2 — what changed

**Upgrading from 1.2.1.** No on-disk format change — the superblock and its
incompat bits are untouched; every volume mounts byte-identically under
either binary. The **cluster wire changed**: the publish plane's schema went
13 → 16 (the reply names the offsets the authority freed; every reply frame
carries the authority's lane-free notices) and `CLUSTER_WIRE_SCHEMA` 1 → 3
(the membership grant carries the checkpoint ceiling in force and a
per-volume lane-supply hint); the shim's `IPC_ABI` stays 6. So the
same-commit fleet rule (KD-7) governs: **the daemon, the shim, and every
authority, co-writer and reader of one volume set upgrade together** — a
peer on another commit is refused loud at its handshake or first frame. A
plain single-writer mount arms none of these planes and is unaffected. The
wire change is also why this release's gate includes the fuzz campaign.

**Multi-writer data path — correctness (the headline).** Every fix came out
of `tests/run_mw_matrix.sh s11-mpiio` on the 1-authority + 8-co-writer
range-custody fleet and landed red-first with its own contract suite.

- **Co-writers hung forever on a full lane** (`09bdfb06`): the "bounded"
  allocation park read a reallocation *label* as a duration — `u64::MAX` ms
  on every co-writer; five of eight sat with 100 writes in flight for 30 min.
  The park is now twice the routine fence bound (floor 1 s) and past it the
  write fails ENOSPC (`write_enospc_refusals`) instead of hanging. Fleet:
  wedge gone (`ac717c7f`). `.benchmarks/2026-09-06-cowriter-enospc-wedge.md`
- **The co-writer free path leaked supply and stormed the authority**
  (`e8d9e675`): a block lifetime under a recomputed publish was freed by
  nobody; a map blob's lineage re-armed a free at every discard. Fleet:
  refused frees 3,449 → 148 (`380ea732`). `.benchmarks/2026-09-06-cowriter-free-refcount-leak.md`
- **Acked bytes discarded on a live mount — data loss** (`b6c5e8d5`): a
  rewrite epoch's close read its own process-local lease rotation as the
  genuine cross-mount fence, **discarded acked, un-fsynced bytes while the
  mount was alive**, and `fsync` returned EIO. It now re-presents the current
  generation; the discard arm fires only on the genuine fence class. Fleet:
  fenced closes 3 → 0, no fsync failures (`8326bb81`).
  `.benchmarks/2026-09-06-cowriter-free-residual-lineage.md`
- **The authority could not read a recycled co-writer block** — finding 51
  (`10388c45`, `23d243a4`): its own `begin_free` had retired the block's
  incarnation word and the served publish adopting the offset never
  re-published it — 326 tripwires, 101 fsync EIOs in one 150 s row. The
  served publish is now the authority's DMA witness (`served_binding_witnesses`)
  and supersedes the open overlay record it displaces.
  `.benchmarks/2026-09-07-read-settle-lost-serialized-authority.md`
- **The `CLAIM ANOMALY` lineage** (`d90fcaab`, `265d69df` — publish schemas
  15/16): a co-writer's tracking of a block the authority had freed outlived
  the free. The served reply now names what the authority freed, and every
  reply frame carries the frees the authority's own publishes did on the
  client's blocks; `block_claim_anomalies` reads 0 on every phase.
  `.benchmarks/2026-09-07-cowriter-claim-anomaly-lineage.md`,
  `.benchmarks/2026-09-07-cowriter-claim-anomaly-population.md`
- **Three smaller ones.** A full store refused ENOSPC while its free list
  sat inside a discard-elision trim window — pending supply, not fullness
  (`1c895aef`, `1304a3bc`; `.benchmarks/2026-09-07-overlay-enospc-convergence-flake.md`).
  The shim's completion doorbell could lose a wake to an already-reaped
  completion — a p99 tail, never a hang; loom-pinned now (`6cc0454d`;
  `.benchmarks/2026-09-06-cqe-doorbell-lost-wake.md`). The S11 notice poll's
  park ask equalled the wire's reply timeout — a zero-margin race on a real
  fabric; now half the bound (`07b51047`; `.benchmarks/2026-09-08-assembler-contracts-notice-poll.md`).
- **Finding 15 — the co-writer supply loop, from "never sustains" to the
  gate met.** A co-writer's freed blocks return only through the authority's
  grace ring; that loop was slow in five places, each fixed and fleet-rowed:
  the reader-ack ladder re-derived (bound age 9,382 → 1,836 ms, ack lag
  6.9 → 0.7 s; `ee49cf04`), the release following the ack (2,500 → 15 ms;
  `4fdfbdcb`), **lane-aware placement + allocation failover** (`7d8b9ed2` —
  load-bearing: the gate fails without it), refills gated on the authority's
  advertised supply (`d0421372`, `b119ef78`), one harvest RPC in flight per
  volume instead of 124,240 in 9.5 min (`6e866be6`), rewrite epochs closed
  on the lane-supply signal (`37861895`), the renewal — the ack's carrier —
  on its own thread (`7d5c1958`, `61e45918`); each commit names its
  evidence note under `.benchmarks/`. **On squeeze-test** the S11 gate
  passes in both B positions of an A-B-B-A (3,193 MiB/s over 72 s, 3,417
  over 102 s; the pre-2026-09-07 tip decays 2,087 → 1,062 —
  `.benchmarks/2026-09-07-f15-day2-squeeze-test-abba.md`) and the **full
  four-phase `s11-mpiio` matrix passes in both control positions** (A1 3,497
  / B1 2,324 / B2 2,301 / A2 2,842 MiB/s steady; tripwires, stale refusals,
  forced releases 0; fsck clean) — the first full passes recorded,
  `.benchmarks/2026-09-07-f15-b1-squeeze-test-seq2.md`.

**Performance — campaigns with field rows.** Same-binary A-B-B-A on
`squeeze-test` (32-core Xeon, 5-node nvme-tcp fabric), medians of both
orders, exact means from `.stats` deltas; the first three share the record
`.benchmarks/2026-09-08-campaign-rows-squeeze-test.md`.

- **R-5 read-handler economy** (`28308f5c`, `eb2c6c67`): the kernel READ
  handler went 8 → 0 allocations per warm op, 11 → 1 on the cold zero-copy
  leg. `rr4k-kern`: **+6.8 % IOPS, −9.6 % daemon CPU/op**.
  `.benchmarks/2026-09-08-r5-read-handler-economy.md`
- **W-6 write-handler economy** (`1eeacaac`, `ea9b26b5`, `40f99e6d`): the
  in-place patch write went 14.6 → 3.06 allocations per op, its per-op
  counters became core-local, a partial overwrite's settle reads only the
  uncovered span. `rw4k-kern`: **+13.2 % IOPS, p50 −12.9 %, −12.2 % daemon
  CPU/op**. `.benchmarks/2026-09-08-w6-write-handler-economy.md`
- **W-5 fsync economy** (`8becf5bc`): an fsync barriers only the data
  namespaces the file touched and runs its independent legs joined;
  `fsync_phase_ns` names where the time goes. Fsync storm: **+23.8 %
  fsyncs/s, p50 −22.9 %**, barrier requests 10 → 1 per fsync; streaming
  `w_durable` par. `.benchmarks/2026-09-08-w5-fsync-economy.md`
- **D-5 owner dispatch** (`80bcd162`, `af6f49cb`, `0687dc91`): the served
  dispatch is split into hops (`meta_ship_owner_dispatch_ns`), the accept
  thread waits on its socket (a dial 100 ms → 0.2 ms), both sockets run
  `TCP_NODELAY`. The two venue levers **ship OFF on the fleet row** (inline
  serve: co-writer publish latency +15–49 %, ingest −1.5…−7 %; one
  multiplexed session ≈ 1.5× slower than the pool).
  `.benchmarks/2026-09-08-d5-fleet-squeeze-test.md`,
  `.benchmarks/2026-09-08-d5-owner-hop-and-depth.md`
- **Three derivation-class landings** (dev-box rows before the venue rule;
  throughput par on each): **D-3** (`d2cf0211`) — the DLM lock tables' width
  derives from the transport's concurrency (16,384 on a 32-CPU box, not a
  free 4,096), with a false-sharing census; mdstorm collisions −76 %, guard
  wait −58 %/op. **W-2** (`838c288a`) — a fresh/append stream's meta-prep
  runs under the inode's read guard; 16,384 exclusive waits per leg → 241.
  **W-4** (`6a5a5053`) — the reclaim queue cap and park bound derive from
  measured rates (4,096 / 1,000 ms become floor and ceiling).
  `.benchmarks/2026-09-05-d3-dlm-stripe-derivation.md`,
  `.benchmarks/2026-09-05-w2-write-stream-guard.md`,
  `.benchmarks/2026-09-05-w4-reclaim-derivation.md`
- **R-4 read zero-copy serve** (`2f0fc361`, `4c626fa9`): on a zc-armed
  session warm and cold-slice READ serves hand the transport the tier
  buffer's own fd instead of a bounce copy — daemon CPU per GiB −24 % cold /
  −25 % warm; default ON. `.benchmarks/2026-09-05-r4-read-zc-serve.md`
- **Kernel: per-queue FUSE background accounting** (`843048fd`, `241cb239`)
  — **a kernel-side change, not a daemon one**: sqz series patch `0031`
  (6.19.14 field track and 7.1; `0026` on 7.2) under `docker/kernel-sqz/`,
  design `docs/design-kernel-bg-per-queue.md`. The connection's `bg_lock`,
  taken twice per ring completion, becomes a per-queue budget: queue-worker
  µs/op −18 %, kern rand-4k **+9 % IOPS** (510 k → 556 k sustained). An
  unpatched kernel is correct and merely pays the lock.
  `.benchmarks/2026-09-06-kernel-bg-per-queue-ab.md`

**New operator surface.** Defaults are right; each lever exists so an A/B
can be counted. Two existing knobs changed default — `SQUEEZEFS_RECLAIM_QUEUE_MAX_BLOCKS`
/ `SQUEEZEFS_RECLAIM_CAP_PARK_MS` are now derived (floor 4096 / 50–1000 ms);
an explicit value still wins. Complete registry: [docs/operations.md → Environment knobs](docs/operations.md#environment-knobs--the-complete-registry).

| Knob | Default | Purpose |
|---|---|---|
| `SQUEEZEFS_READ_ZC_SERVE`, `SQUEEZEFS_GAP_SEED_RANGED`, `SQUEEZEFS_FSYNC_TOUCHED_NAMESPACES`, `SQUEEZEFS_FSYNC_PARALLEL_LEGS`, `SQUEEZEFS_WRITE_GUARD_NARROW` | on | The R-4 / W-6 / W-5 / W-2 A/B controls: `0` = that campaign's prior shape. |
| `SQUEEZEFS_DLM_STRIPES` | derived | Explicit stripe count for the DLM-class lock tables; `4096` = the 1.2.1 width. |
| `SQUEEZEFS_META_SHIP_INLINE_SERVE`, `SQUEEZEFS_PUBLISH_SHIP_MULTIPLEX` | off | D-5 venue levers; a fleet running either is running an experiment. |
| `SQUEEZEFS_COWRITER_LANE_PLACEMENT` | on | Lane-aware placement on a co-writer; `0` fails the S11 gate — a control, not a setting. |
| `SQUEEZEFS_ALLOC_LANE_{REFILL_HINT,HARVEST_SINGLE_FLIGHT,VOLUME_HINT}`, `SQUEEZEFS_REWRITE_SUPPLY_CLOSE`, `SQUEEZEFS_MEMBERSHIP_RENEW_LANE` | on | The co-writer supply loop's levers: refill hint gate, one harvest RPC in flight, per-volume hint, supply-coupled epoch close, the renewal on its own `sqz-lease-io` thread. |
| `SQUEEZEFS_FREE_GRACE_{ACK_RENEWAL,REFRESH_ON_ACK,QUALIFY_CEILING,DRAIN_EPOCH_STAMP,DRAIN_OBSERVED,CHECKPOINT_COMPOSITE,LANE_PUSH,CAUGHT_UP_RELAX}` | on | The freed-offset grace loop's hold-time levers; each `0` restores that one retired term. |

New `.stats` families (semantics in [docs/operations.md](docs/operations.md)): `fsync_phase_ns` + `fsync_{calls,noop_clean,data_namespaces_touched,data_namespaces_flushed,write_through_skips,parallel_joins}`;
`meta_ship_owner_dispatch_ns`; `membership_renew_phase_ns` / `membership_renew_serve_ns`; `free_grace_hold_phase_ns`, `free_grace_hold_ms`, `free_grace_member_ack_lag_ms`;
the lock census `<table>_stripe_collisions` / `<table>_key_waits` + `lock_phase_ns.dlm_guard_wait`; `block_free_reclaim_{drain_rate,arrival_rate,queue_cap,park_bound_ms}`;
and the fix tripwires `write_enospc_refusals`, `alloc_trim_window_parks`, `served_binding_witnesses`, `rewrite_shadow_close_retries`, `block_claim_anomalies`, `overlay_superseded_by_served_publish`, `dlm_custody_notice_poll_failures`.

**FUSE transport — the queue worker's park is bounded while a reply is owed**
(`8efc7e1b` → `1834bed4`, found by this release's own gate). Inside fstests
`generic/795` (drop_caches × fsstress × rm/cp/cmp on a fresh mount) two
delivered LOOKUPs were never answered for 23 minutes, every daemon thread
idle, the mount cleared only by the harness aborting the connection; the
runner scored the test clean. The class is the one the 2026-08-07 zc-bridge
campaign closed for zero-copy pends — a worker asleep in cq-wait that lost
exactly one wake — but an ordinary in-flight request had no bound. Now the
park is EXT_ARG-bounded (100 ms) while the worker's drain group owes any
reply; a tick that finds work a wake should have delivered is counted
(`transport_park_tick_{commit,cqe}_rescues`, ≈ 0 on a healthy mount —
nonzero **is** the lost-wake tripwire) and the first rescue, like the 5 s
overdue-slot line, logs the attribution snapshot (coalescer state,
`eventfd-count`, the ring's queue heads and pending poll list). Seam
`SQUEEZEFS_TEST_DROP_COMMIT_WAKES`; suite `tests/commit_wake_loss_tests.rs`
(red on the unfixed tree: the stat strands; green: it lands in one tick).
The fstests runner now fails a test whose mount logged a request unreplied
≥ 60 s. `.benchmarks/2026-09-08-generic-795-lookup-wedge.md`

**Known limitations — what this release does not claim.**

- **Scale.** What ships is one write mount per volume set, any number of
  read-only mounts and opt-in co-writer mounts; the S11 rows are nine
  co-located mounts on one box. Every scale claim carries its evidence tier in
  [docs/rc-manifest.md](docs/rc-manifest.md#2-guarantee-table-by-evidence-tier-ruling-d1); 15 k nodes has never been measured.
- **The s11 venue's remaining ENOSPC refusals are capacity, not code**: with
  the previous phase's file kept, each lane holds 640 live blocks of a
  1,024-block share against a ≈ 3.4 s recycle transit (≈ 10 % under) —
  `.benchmarks/2026-09-07-cowriter-fpp-supply-residue.md` §8.5,
  [docs/operations.md → Multi-writer capacity planning](docs/operations.md#multi-writer-capacity-planning--the-data-plane-allocation-partition).
- **fsync of a partial active block escalates the whole block**: every
  256 KiB write + fsync reads and uploads 4 MiB (18.4× the user bytes, 80 %
  of the 6.9 ms fsync). Pre-existing, named by W-5's instrument, not fixed —
  write board #11 in [docs/design-e2e-perf-audit.md](docs/design-e2e-perf-audit.md).
- **The lost wake's loser is not named.** The `generic/795` wedge is bounded
  (a lost wake now costs ≤ 100 ms and is counted), but whether the kernel's
  task-work wake or the daemon's coalescer lost it is unattributed — 48
  fresh-mount storms with the live capture armed did not recur. The rescue
  snapshot names it at the next exposure.
- **Dismounts reporting unflushed staged files**: 47 of one fstests pass's
  407 dismounts reported 1–2, four reported 200+. Orphans of deleted files
  or acked bytes lost at umount is unanswered; the runner records it as a
  NOTE, never a verdict.

**Verification.** Every leg of the release gate ran from zero on the tested
tree in one unattended chain — `task check`, fstests `-g auto` (with
`generic/650` excluded on the gate laptop: its CPU-hotplug storm hangs that
box's firmware — a platform hazard, stated in the record), pjdfstests, LTP,
require-mount, the zc-capability leg on the sqz kernel — **plus the fuzz
campaign over the twelve `fuzz/fuzz_targets/` decoders**, because the
publish and cluster wire schemas changed (`publish_wire` and
`cluster_wire_frame` are two of the twelve). The gate ran twice: the first
candidate's fstests leg found the `generic/795` wedge above and the tag was
held for the fix; the second candidate is the release. Record:
[.benchmarks/2026-09-08-1.2.2-release-gate.md](.benchmarks/2026-09-08-1.2.2-release-gate.md).

The rest of this document is the 1.2.1 and 1.2.0 record, which 1.2.2
inherits.

---

# SqueezeFS 1.2.1

_Release date: 2026-09-05 (tag `stable-2026.09.1`)_

1.2.1 is a point release on the 1.2 train: two shutdown fixes, a mount that
identifies itself, a corrected build-identity stamp on release artifacts,
and one publish-plane performance rung — all measured, none changing the
on-disk or wire formats. It upgrades from 1.2.0 in place (same superblock
bits, same volumes, same clients) and from 1.1 exactly as 1.2.0 does (see
§Upgrading from 1.1 below).

## 1.2.1 — what changed

- **The daemon unmounts itself on SIGTERM.** Since the multi-queue transport,
  `SIGTERM` (and `squeezefs umount`) reached "Dismount clean" and then the
  daemon sat, still mounted, until something external destroyed the FUSE
  connection — the `umount` verb's kernel-abort fallback masked it after a
  5 s wait every time. The destroy notification now reaches every session
  (one per queue plus the primary); SIGTERM → exit measures 0.17 s, and
  `squeezefs umount` completes in 130–230 ms instead of 5 s + abort. An
  exiting daemon also finishes with a lazy detach when a bystander (a
  desktop volume monitor, for example) holds a transient fd on the fresh
  mount — the mount leaves the namespace immediately.
- **`squeezefs umount` judges by the mount, not `/proc/<pid>`.** A daemon
  that is another process's child is a zombie until reaped, and `/proc/<pid>`
  still exists for a zombie; the verb used to read that as "still alive",
  wait out two 5 s windows and then fail the direct unmount of a mount that
  was already gone.
- **The mount identifies itself.** `mount`, `df -T` and `/proc/mounts` show
  `squeezefs on … type fuse.squeezefs` (FUSE's `subtype`), not a bare
  `fuse`; `-o fsname=<name>` still overrides the name. xfstests users:
  the runner normalizes `fuse.squeezefs` → `fuse` for the harness's exact
  type check, as it does for `fuse.glusterfs`.
- **Release artifacts name their profile correctly.** The 1.2.0 `dist`
  binaries reported `profile release`: `build.rs` parsed the profile name
  from `OUT_DIR` by the first `build` path component, which under the
  container's `/build/target/…` was the target directory. Fixed with a
  shared derivation anchored on the trailing `build/<pkg>-<hash>/out`, pinned
  on both venues' layouts; `docker/check-artifacts.sh` now refuses a binary
  whose version line does not end in the profile it was built with.
- **Publish plane: one conveyor group per shipped frame (D-1c).** On the
  metadata authority, a shipped frame's layout publishes are staged
  concurrently and enqueued onto the commit conveyor under one queue lock,
  so a frame costs one apply pass by construction instead of a
  timing-dependent 3–14. Adjudicated on two venues (six same-binary
  A-B-B-A brackets): +6 % aggregate co-writer ingest on the many-small-files
  row on the field box, par elsewhere, no loss; journal entries per publish
  unchanged. Lever `SQUEEZEFS_PUBLISH_CONVEYOR_GROUP` (default on; `0` is the
  A/B control). New gauges `meta_conveyor_group_{commits,txs}`,
  `meta_ship_publish.frame_groups`. Record:
  `.benchmarks/2026-09-04-d1c-conveyor-group-per-frame.md`.
- **Test-harness fixes** that the 1.2.0 gate surfaced: the D-1b publish
  batching contracts pin the co-queue law under a held pass (a venue ratio
  was being asserted); the executor park test awaits its third hook call;
  the live NVMe-reservation leg drives its I/O as `nvme io-passthru` so it
  runs on nvme-cli 1.x (EL8).

**Verification.** Every leg of the release gate ran from zero on one
commit (`ae4f27e8`) in one unattended chain — `task check` (4,626 cargo
tests), fstests `-g auto` (198 ran, only the four documented by-design
shapes), pjdfstests (8,798), LTP (1,884 / 0 / 0), require-mount — and the
zc-capability leg passed on two sqz-series kernels (149/149, skip ledger
empty, both). No fuzz re-run: nothing on disk or on the wire changed.
Record: [.benchmarks/2026-09-05-1.2.1-release-gate.md](.benchmarks/2026-09-05-1.2.1-release-gate.md).

The rest of this document is the 1.2.0 record, which 1.2.1 inherits.

---

# SqueezeFS 1.2.0

_Release date: 2026-09-04 (tag `stable-2026.09`)_

1.2.0 is the release train after 1.1. It removes the file-size ceiling, closes a set of data-integrity bugs found in the field, and lands the first campaigns of an end-to-end performance program together with the instruments that program runs on. Existing volumes mount unchanged; read [Upgrading from 1.1](#upgrading-from-11) before rolling it out to a mixed fleet.

## Highlights

- **Files of any size.** A file's block map now lives in a dedicated metadata tree once it outgrows its inline record — roughly 8 GiB of file at the default 4 MiB block size. Nothing is configured: the crossing happens once per file, automatically. The former ceiling of about 545 GiB per file is gone; the format's limit is now `(2³² − 1)` blocks per file — 16 PiB less one block at 4 MiB blocks. Truncating or deleting a very large file returns immediately and reclaims its records in the background.
- **Faster and safer: a set of data-integrity fixes.** A freshly written file could lose the last 3 MiB of its first block on a clean unmount of a cache-less mount; writing a file past ~8 GiB could corrupt the volume's metadata and wedge it permanently; co-writer fleets leaked freed blocks until the data volume filled; and on a cache-less mount, a heavy rewrite followed by an unmount could lose a block that was still being flushed. All four are fixed, each pinned by a test that reproduces it — see [Fixes](#fixes).
- **An end-to-end performance program, with instruments first.** Every latency histogram on `.stats` now carries an exact count, sum and mean; a per-operation trace (`.trace`) joins one request's phases on one timeline; daemon CPU is attributed by thread class. The first campaigns on those instruments: about +15 % on 4 KiB random-read IOPS through the kernel path on the reference fabric; and, for multi-writer ingest, the metadata commit pipeline no longer holds its serialized stage across the device write, while co-writer publishes travel batched instead of one round trip each.
- **Build and release changes.** `cargo build --release` is now a thin-LTO build (fast to rebuild — the dev, field-A/B and gate profile); tagged releases ship from the new `dist` profile (fat LTO, one codegen unit) via `task dist:<distro>`. Every binary names its profile: `squeezefs --version` ends in `profile <name>`, and `.stats` exports `build_profile`.
- **Documentation refresh.** The [README](README.md) is a plain overview with the four measured hero numbers; [QUICKSTART.md](QUICKSTART.md) is a hands-on walkthrough; [docs/operations.md](docs/operations.md) now lists every environment knob (with default and purpose) and every `.stats` key, and carries the large-file section, the field performance records and the build-verification gate.

## Upgrading from 1.1

**Volumes.** A volume formatted or written by 1.1 mounts unchanged under 1.2 — the on-disk format is the same, and a volume that never carries a very large file stays byte-identical whichever binary mounts it. Three forward-only boundaries to know about:

- **The block-map tree is stamped on first use.** The first time a 1.2 mount publishes a file past the inline map cap (about 6–8 GiB at 4 MiB blocks), it stamps superblock incompat bit 16 on that volume and announces it in the mount log (`grep kvmap <log>`). **Once a 1.2 mount has written a file past ~8 GiB to a volume, that volume can no longer be mounted by 1.1** — a 1.1 binary refuses it loudly, naming the unknown bit. There is no downgrade path other than reformatting; upgrade the binary, never downgrade the volume. Volumes that never cross mount on either train.
- **Fresh formats are multi-writer-capable by default since 2026-08-16.** A volume set formatted by 1.2 carries the multi-writer capability bits, which 1.1 binaries built before that date refuse. `format --single-writer` produces the older class for exactly that case (a scratch volume an old binary must read); `squeezefs volume enable-multi-writer` upgrades an older set offline.
- **One reformat class.** A volume set formatted by a 1.1 binary from before 2026-08-01 uses the frozen metadata-routing width that release retired; 1.2 refuses it with *reformat required*. Sets formatted on or after that date are unaffected.

**Binaries.** Deploy the daemon and the interception shim (`libsqueezefs_il.so`) from the same build folder — they refuse to pair across builds, so a 1.2 daemon will not accept a 1.1 shim or the reverse. `squeezefs --version` now prints a trailing `profile <name>` (`release` for a plain build, `dist` for a tagged release); tooling that parses the version line should key on `.stats` `build_commit` / `build_tag` / `build_profile` instead, which are unchanged.

**Retired flags, verbs and knob spellings since 1.1 began (2026-07-24).** Each refuses or announces loudly, naming its successor; the complete catalog (including retirements that predate 1.1) is [docs/operations.md → Removed verbs & flags](docs/operations.md#removed-verbs--flags).

| Retired | Since | What happens | Use instead |
|---|---|---|---|
| `format --meta-slots N` | 2026-08-01 | hard error | nothing — metadata routing widths are derived; grow a set with `squeezefs volume add-meta --take-slots …` (offline) or `squeezefs volume migrate-meta-slot` (online) |
| `format --multi-writer` | 2026-08-16 | accepted, announced as having no effect (it is the default) | `format --single-writer` is the explicit opt-out |
| `SQUEEZEFS_RECLAIM_BATCH` | 2026-08-02 | refused at startup | `SQUEEZEFS_INODE_RECLAIM_BATCH` |
| `SQUEEZEFS_RECLAIM_BATCH_WINDOW_MS` | 2026-08-02 | refused at startup | `SQUEEZEFS_INODE_RECLAIM_WINDOW_MS` |
| `SQUEEZEFS_RECLAIM_CONCURRENCY` | 2026-08-02 | refused at startup | `SQUEEZEFS_INODE_RECLAIM_CONCURRENCY` |
| `SQUEEZEFS_FUSE_PLACED_MERGE` | 2026-08-09 | refused at startup | nothing — the mechanism was measured and removed |

Also since 2026-08-02, **every environment knob obeys one value convention**: `0`/`false`/`no`/`off` disables and `1`/`true`/`yes`/`on` enables (case-insensitive) — including knobs whose default is on. Before that, 19 knobs were presence-based, so `SQUEEZEFS_FREE_FORENSICS=0` used to *enable* forensics. A malformed or out-of-range value now refuses the process at startup, naming every offender, instead of being silently clamped or defaulted; an unrecognized `SQUEEZEFS_*` name is announced as a probable typo. Details: [docs/operations.md → Environment knobs — the parsing convention](docs/operations.md#environment-knobs--the-parsing-convention).

**New environment knobs an operator might care about.** Defaults are right; the levers exist so an A/B can be counted, and a fleet running one of them is running an experiment.

| Knob | Default | Purpose |
|---|---|---|
| `SQUEEZEFS_OP_TRACE` | off | Arm the per-operation trace ring at mount; `cat <mnt>/.trace` drains it. Off costs one relaxed load per hook. |
| `SQUEEZEFS_KVMAP` | on | Measurement lever: `0` makes *new* large-file crossings keep the legacy single-blob map. It never disables reading a file that already lives in the tree. |
| `SQUEEZEFS_KVMAP_OVERLAY` | on | Measurement lever: `0` holds a very large file's whole write map in RAM regardless of size instead of the bounded partial store. |
| `SQUEEZEFS_MAP_MIGRATE_CHUNK` | 512 (64–1024) | Map records written per transaction while a file crosses into the tree. |
| `SQUEEZEFS_JOURNAL_LANE` | on | Measurement lever: `0` = the metadata journal's durability stage back on the shared pool instead of one lane per writable volume. |
| `SQUEEZEFS_FUSE_READ_FAST_DISPATCH` | on | Measurement lever: `0` = the pre-1.2 read dispatch path. |
| `SQUEEZEFS_OVERLAY_DEPTH_GOVERNOR` | on | Measurement lever: `0` = large-write overlay stores issue open-loop, outside the write-pipeline depth governor. |
| `SQUEEZEFS_FUSE_ZC_READ_FUSION` | off | Measurement lever, ships off on measurement (it loses to the default when composed with fast dispatch). |
| `SQUEEZEFS_FUSE_IO_URING_SPIN_US` | 0 (off) | Measurement lever, ships off on measurement: a spin-before-park cap for the FUSE queue workers. |

The complete registry — every knob, accepted values, range, default, purpose — is [docs/operations.md → Environment knobs — the complete registry](docs/operations.md#environment-knobs--the-complete-registry).

## Fixes

Every fix landed with a test that reproduces the bug (red before, green after). Commits are on `dev`; the internal record names the finding the engineering notes use.

| What you would have seen | Fixed in | Internal record |
|---|---|---|
| **Cache-less mount, heavy rewrites, then `umount`**: `Dismount durable upload failed for ino … journal entry length … exceeds the 131072-byte whole-entry cap` in the log, and the block named there was lost. A long rewrite storm had accumulated more block-reference bookkeeping than one metadata transaction may hold; every publish of it refused, and the unmount drain dropped the block. Oversize loads are now committed in chunks. | `b0ea5f04` | finding 38 — [`.benchmarks/2026-09-01-field-corruption-train.md`](.benchmarks/2026-09-01-field-corruption-train.md) |
| **Writes collapsed ~8× under memory pressure**, with multi-second stalls: the write pipeline's memory-pressure response clamped to a fixed floor instead of the measured drain rate. It now sheds only headroom and keeps the measured completion rate. | `9936c625` | finding 39 — same note |
| **A nearly full data volume (~97 %) made every write pay a 12–22 ms synchronous reclaim** on the fabric: background block reclaim deferred to foreground traffic even when the queued reclaim debt was most of the remaining free space. Reclaim now runs ahead of allocation when the free supply is thin, and once engaged it runs the queue to empty rather than stopping part-way (the follow-up). | `ac0c68bc`, `c985fa8c` | finding 40 — same note; the follow-up commit |
| **Writing a file past ~8 GiB could corrupt the volume's metadata and wedge it permanently**: `divergent layout-delta chain … folds onto 0x0` in the log, then `fsync` failures and every write stalling forever. A metadata node split could separate two records that must stay together; the stranded record became unroutable and poisoned every later fold. Splits now respect record groups, and a node write that would strand a record refuses loudly instead. Verified twice from zero on the 5-node fabric with the exact triggering workload. | `08202c32` | finding 41 — same note |
| **Co-writer fleets leaked freed blocks until the data volume filled** (`authority refused N shipped frees` in the authority's log, co-writers hitting ENOSPC with space that should have been free — about 26 GiB leaked in two minutes on the test fleet). When the authority recomputed a co-writer's publish, the displaced blocks were freed by nobody. The authority now frees them itself after commit, across all four code paths that could recompute. | `6101b09b`, `19e9fb76`, `82de27bf`, `418f06e8` | findings 36 / 36b — same note |
| *(Development builds only.)* The block-map tree never engaged on a real mount: its enabling gate depended on a stamp only the tree itself could write. Large files now self-arm the tree at the first crossing. | `fc0549aa` | finding 43 — [`docs/design-kvmap-block-map-tree.md`](docs/design-kvmap-block-map-tree.md) |
| Zeroed data after a rewrite-and-remount smoke test — traced to an unclean kill in that store's history (acknowledged, un-`fsync`ed writes carry no crash guarantee); not a defect on the shipped path. Content-verification tests for the rewrite/remount venue were added so the class cannot hide. | `7c5aad36` | finding 44 — same document |
| Nothing in the log or `.stats` said whether a large file had entered the block-map tree, so a correct run looked like the feared regression. Each crossing now logs one line (`kvmap crossing: ino N …`). | `a7684785` | finding 45 — same document |
| **Sequential writes fell ~2,000× after a file crossed ~8 GiB** (28.7 GB/s → 0.36 GiB/s, about 1 s per 1 MiB write) while rewrites of the same files ran at full speed: every steady-state publish re-read and re-diffed the file's whole map. A publish now touches only the blocks it publishes. | `87755e2e` | finding 46 — [`.benchmarks/2026-09-02-f46-kvmap-stream-publish.md`](.benchmarks/2026-09-02-f46-kvmap-stream-publish.md) |
| **Small writes could cost a whole block each**: a 4 KiB write into a hole or a clone-shared block paid a 4 MiB read plus a 4 MiB write at settle, and small sequential O_DIRECT segments were stored one device write per segment instead of accumulating into one block write. Writes at or under the in-place patch ceiling (512 KiB at 4 MiB blocks) no longer take the large-write path. | `3a2404e0` | finding 47 — [`.benchmarks/2026-09-02-f47-overlay-length-floor.md`](.benchmarks/2026-09-02-f47-overlay-length-floor.md) |
| **A freshly written file could lose the last 3 MiB of its first block across a clean unmount** on a cache-less mount — reading it back before the unmount showed the right bytes; after remount the bytes past the first 1 MiB read as zeros (23 of 24 files on the field run). Closing the file never settled the block's pending overlay record, and unmount never closed the rewrite state a later read had opened. Close and unmount are now durability boundaries for both. | `3688d1fb` | finding 48 — [`.benchmarks/2026-09-02-f48-warm-cold-overlay-gap.md`](.benchmarks/2026-09-02-f48-warm-cold-overlay-gap.md) |
| **A sustained create/unlink storm in one very large directory could fail-stop the volume**: `commit aborted while parked for ring space` after the journal ring filled, with the checkpoint stuck. The checkpoint's own maintenance drain starved its cadence, and one fold inside it ran for tens of seconds. The drain is now bounded per cadence period and the fold no longer rescans its input for every record. | `febd0d87` | finding 49 — [`.benchmarks/2026-09-03-c2-uring-fs-completion-hop.md`](.benchmarks/2026-09-03-c2-uring-fs-completion-hop.md) (the owed-items board) |
| **`df -i` could briefly report deleted inodes as still in use** after a delete: an explicit inode reclaim could return while a concurrent background reclaim still owned those inodes. It now waits for that owner's outcome. | `3975ac62` | finding 50 — same note |

## Performance

Measured on a 5-node NVMe-oF/TCP fabric (memory-backed targets) with a 32-core client, in interception mode:

| Read bandwidth | Write bandwidth | Read IOPS (4 KiB) | Write IOPS (4 KiB) |
|:---:|:---:|:---:|:---:|
| **43.9 GB/s** | **36.4 GB/s** | **942 k** | **727 k** |

Every measurement behind these numbers — venue, instrument, substrate, and the campaign notes that moved them — is recorded in [docs/operations.md → Performance records](docs/operations.md#performance-records).

## New operator surface

- **Exact latency histograms.** Every latency family on `.stats` exports `buckets` plus an exact `count`, `sum_ns` and `mean_ns` (means used to be estimated from bucket midpoints). Per-row means are now `Δsum_ns / Δcount` between two snapshots; bucket labels are unchanged, so existing tooling keeps working.
- **A per-operation timeline.** Mount with `SQUEEZEFS_OP_TRACE=1` and `cat <mnt>/.trace` drains one clock stamp per phase boundary per sampled operation, keyed by the FUSE request id (which the kernel's FUSE tracepoints also carry) or the interception ticket; `tests/op_trace_stitch.py` stitches a dump against the `.stats` histograms. Owner-only, like `.stats`.
- **Daemon CPU by thread class.** `daemon_cpu_ns_by_class` on `.stats` splits daemon CPU across the FUSE queue workers and handler lanes, the interception service threads, the metadata and journal lanes, the block and NVMe workers, and the timer thread — the denominator every CPU-per-operation figure now cites.
- **fsck class C11 — large-file map consistency (report-only).** `squeezefs fsck` checks the block-map tree for orphan map records, heads with no records, and run-versus-point coverage anomalies. It reports and never auto-repairs; `fsck_map_orphan_records` must stay 0.
- **Background sweeps in `squeezefs job`.** Truncating or deleting a tree-mapped large file returns at once and leaves a resumable background job that reclaims the records and blocks in chunks; `squeezefs job list <mountpoint>` shows them as `kvmap_sweep` jobs, and they regenerate at mount after a crash.
- **`task dist:<distro>`** (`rocky8`, `rocky9`, `ubuntu2404`, `ubuntu2604`, or `dist:all`) builds the fat-LTO release binaries for a tagged release into `dist/<distro>-dist/`; `task build:<distro>` stays the fast thin-LTO build.

## Known limitations

- **Very large write-active files and RAM.** A file whose write map would exceed its derived share of the memory budget takes a bounded partial store (a dirty overlay plus warm windows with tree read-through), so a single petabyte-class file being written does not hold its whole map in RAM. The cost of that store is bounded but its economy is not finished: per-write probes on maps with many scattered blocks, the whole map still travelling with each co-writer publish, and run re-coalescing happening only on truncate/fsync-class saves are the open items, listed at the end of [docs/design-kvmap-block-map-tree.md](docs/design-kvmap-block-map-tree.md).
- **Scale claims and their evidence.** What ships is one write mount per volume set, any number of read-only mounts, and opt-in co-writer mounts. The very-large-fleet design target has been proven on a single-node fleet of many co-located mounts; every scale claim carries its evidence tier in [docs/rc-manifest.md](docs/rc-manifest.md#2-guarantee-table-by-evidence-tier-ruling-d1), and no number in these notes supports a wider claim.
- **The read path's remaining headroom is in the kernel.** On 4 KiB random reads through the kernel path, the FUSE-over-io_uring queue worker now spends most of its per-operation time inside the kernel's commit path — including contention on the FUSE connection's per-connection and per-queue locks — not in the daemon. Moving that is a kernel-side change, not a daemon one.

## Verification

The tag ships only when every line below is ticked. Every leg ran from zero on one commit in one unattended chain — twice, on `ac3fb7eb` and again on `3a8fafc1` (the tag's code) after a build-identity fix; the record is [.benchmarks/2026-09-04-1.2.0-release-gate.md](.benchmarks/2026-09-04-1.2.0-release-gate.md).

- [x] `task check` — clippy (all features), clippy (shipped features), fmt, `cargo test --all-features -- --test-threads=1` (4,598 tests), rustdoc with `-D warnings`, Criterion bench smoke, the `crates/fuse3` fork's own suite, the loom-model build, the fuzz workspace type-check, the markdown link/anchor check, and `cargo audit` over both lockfiles ([the gate's legs](docs/operations.md#verifying-a-build-task-check)) — green on `ac3fb7eb` and `3a8fafc1` (4,598 / 4,601 tests), [.benchmarks/2026-09-04-1.2.0-release-gate.md](.benchmarks/2026-09-04-1.2.0-release-gate.md).
- [x] pjdfstests (`sudo tests/run_pjdfstests.sh`) — 238 files, 8,798 tests, all successful; [.benchmarks/2026-09-04-1.2.0-release-gate.md](.benchmarks/2026-09-04-1.2.0-release-gate.md)
- [x] LTP filesystem syscalls (`sudo tests/run_ltp_syscalls.sh`) — 1,884 passed, 0 failed, 0 broken, 44 kernel-feature skips; [.benchmarks/2026-09-04-1.2.0-release-gate.md](.benchmarks/2026-09-04-1.2.0-release-gate.md)
- [x] fstests `-g auto`, one complete pass from zero (`sudo tests/run_fstests.sh`) — 198 ran, 0 unexpected failures (the four by-design adjudications matched their pinned shapes; the expected-PASS sentinels 074/464 passed); [.benchmarks/2026-09-04-1.2.0-release-gate.md](.benchmarks/2026-09-04-1.2.0-release-gate.md)
- [x] The fuzz campaign over the twelve `fuzz/fuzz_targets/` decoders (on-disk metadata incl. the block-map tree, the cluster and publish wires, the interception shared memory) — 383.6 M executions, 0 crashes, on the same product code; [.benchmarks/2026-09-03-release-1.2-fuzz.md](.benchmarks/2026-09-03-release-1.2-fuzz.md)
