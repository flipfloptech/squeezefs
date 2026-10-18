# Handoff — 2026-10-18: one device under the cut, the size epoch, fabric multipath; the release sequence

**Checkpoint:** `dev` at the commit that adds this note (on `9a165eb`); everything pushed to `origin` (the hook).
Venue: this host (`SQZ_VENUE=local`, Omarchy, 32 threads, 109 GiB, `7.2.8-7-omarchy-sqz`); the
2026-10-17 note has the venue's shape. A full gate takes 4–7 minutes here; the train's three greens
on `539ae02` and on `c6e0f5b` are in `.gate-train/ledger-v2.tsv`.

## What landed since `539ae02`, in order

| commit | what |
|---|---|
| `455ed6c` | the staging residents census is sized and filled under ONE lock hold (generic/013: a SIGABRT under syncfs) |
| `34ca652` `c917653` | the staged arm's rebuilt image zeroes every byte its resident image did not cover (generic/075, 127 fsx on the one-device layout) |
| `482300c` | the times drain journals a size claim as it stands under its guards (the FUSE-reachable face of the generic/112 size race) |
| `1fbb62d` | **the overlay settle ladder polls the volume's FAILED latch** — a transient on a FAILED volume is the fence (generic/475 under dm-error on the one-device layout: the gap seed's EIO retried past attempt 1024, the umount waited for ever, a truncate's `drainForIno` wedged a setattr 200 s) |
| `c6e0f5b` | **one device under the cut**: each xfstests set is ONE loop image carved by dm-linear into `sqz-<set>-{meta,d0,d1,cache}` (`scripts/suites/setpaths.sh`); `umount.fuse` by the type prefix; the 71 interim exclusions are gone (29 venue-shaped rows remain) |
| `bba2f0c` | LTP keeps kirk's debug log on a run that did not pass (kill11's "broken (2m 0s)" left nothing to read) |
| `2250ca9` | **the per-inode SIZE EPOCH** (`src/data/size_epoch.zig`): size setters bump it under their exclusive guards; a write's postlude claims no size past a set that crossed its landing; a write-back unit formed before a truncate inserts nothing at or past the new EOF and raises no size (`eof_discarded`). Evidence `rung-10-size-epoch.md`; the flakes row for generic/112 is marked fixed (the train retires it) |
| `cf3a42d` | **a staged read whose identity moved between its layout resolve and its ring lookup re-resolves** instead of serving zeros — a promotion (here a host-wide `sync` → SYNCFS under the lever) commits the packed layout and releases the ring entry between the two (generic/127 `READ BAD DATA` at 0x27000, the daemon's `staged payload unreachable … serving 4096 zero bytes`). Seam `SQUEEZEFS_TEST_STAGED_READ_HOLD`, smoke row "THE STAGED READ'S IDENTITY WINDOW"; evidence `rung-10-staged-unreachable.md` |
| `9a165eb` | **a holder's re-point whose dial fails still drops every cached token** — `Divert.repoint` returned false on the resolver's null and left the dead manager's tokens served (THE KILL ROWS (b) red in gate 18: `dlm_token_holder_repoints` 0 with the successor's listener not yet dialable) |
| `8f6e479` `5d1ed68` `022b84c` | **fabric multipath**: `set_config` v2 — a data volume's endpoint is a LIST of paths of one subsystem, connected under the mount's one identity and merged by the kernel into one head accepted only when EXACTLY ours; the metadata head judged per path (rule 2); `SQUEEZEFS_FABRIC_IOPOLICY` written and read back (default `round-robin`, owner ruling); path-loss clocks `fast_io_fail_tmo 45 / ctrl_loss_tmo 90 / reconnect_delay 10` on every tcp connect; `nvmeof status` lists paths; fidelity leg `multipath-tcp` (two loopback listeners → one head, both paths carry sectors). Evidence `rung-10-fabric-multipath.md`, decision 0002 row 543 |

## Rulings (owner, 2026-10-18)

- **Class C suites run only in the release window.** Restated after a mid-development LTP run was
  started by mistake. The sequence: finish the feature work → ONE LTP run and ONE xfstests run,
  fixing what they find as we go → once each has a clean single run with nothing left to fix, a
  final full gate run with no fixes → the tag. Performance work comes before the formal release.
- **PR is not required for multi-writer**: at `RESCAP = 0` every layer degrades to detection grade
  (flock+claim, the frame screen, the full tail scan at recovery); PR is the device-enforced fence,
  which the product's `share` verbs enable (`resv_enable = 1`, target kernel ≥ 6.13).
- **Multipath covers the metadata volume too** (the operator connects its paths under one pair;
  the mount judges, polices and counts them). Follow-up: a `sqmeta://` fabric URI / mount option so
  the daemon owns those connects as well.
- **`SQUEEZEFS_FABRIC_IOPOLICY` default `round-robin`**: queue-depth left a lone stream 131072 / 0
  sectors on one path and skewed four writers 409600 / 114688; round-robin balances within 5 %.
- **Pending**: the flakey corpus's durability lever (`SQUEEZEFS_FSYNC_PROMOTE_STAGED`; the
  2026-10-17 note states the three options; the release-window xfstests run uses the lever ON until
  ruled); the fence tripwires' classification on a FAILED volume.

## Open defects and nits

- Side findings from the generic/127 work (`rung-10-staged-unreachable.md`), unfixed: `-o
  direct_device_true` reaches only the stats row (the read path's ruler is the env knob); the
  staging ring's placement treats only indexed/pending extents as live, so a retired cell a reader
  still leases could be overlaid and its reclaim punch the newcomer (not reached).
- LTP `kill11` "broken (2m 0s)" once (syscalls run 1): passes by hand on our mount and on ext4;
  nothing in the daemon log or the journal for its two minutes. Re-judge in the release-window run
  (the debug log is kept now).
- A FAILED volume's times-drain cadence warns every 50 ms; `squeezefs umount` returns before a
  FAILED daemon exits when its `.stats` is unreadable; the overlay's fence verdict fires
  `overlay_fence_drops` like the other fence arms (scoped by the harness on dm-error daemons).

## Owed items carried from the retired progress ledger

`docs/memory/` (the agent-memory snapshot that carried the notes to this workstation) is gone;
its progress ledger's actionable lists live here now. Everything else it held is in `git log`,
the evidence notes and the decision ledgers.

- **Deliberate limits still standing**: `submitReplyFdBody` falls back to a copy
  (`transport_fd_body_fallbacks`); `--daemon` requires `--log-file` (0002 row 93).
- **Owed from the rungs**: forest/slot-tree `compact_node_forced` and `reachable_node_addrs`
  (rung 2b); a lane ticket waited from a fiber on another lane still parks the raw futex,
  supervisor escalation tested only as the pure state machine, the empty-submit enter in
  `ring_park.park` (rung 3); NT stores and per-op NVMe phase stamps (rung 4); the KVDA page sink
  follow-ups and a parked joiner's `.stats` EAGAIN exemption (rung 6); key rotation as a
  defrag-class mover (rung 8); the smoke's cache-path data-plane stage fails when
  `SQUEEZEFS_TEST_DATA_DIR` points at `/build/ext` (a test-side timing assumption on a non-sparse
  staging-ring preallocation).
- **Owed from the syncfs work**: fsync flushes at most 64 parked blocks per inode; the `sync.mpmc`
  pool row assumes its race happened (fails under load); a failed nvmet share leaves its subsystem
  behind; `quarantineBlock`'s virgin arm vs a concurrent mint (judged benign).
- **Suites open items** (re-judge in the release window): generic/069 staged-append amplification
  (≈ 380 appends/s, correct but slow); staging ring full → ENOSPC under a 20-writer 4 MiB burst;
  generic/740 bare mkfs must refuse a foreign fs; generic/756/777 `open_by_handle` on an unlinked
  file returns success; daemon RSS 1.9 → 3.7 GB under the suites without return.
- **Post-release list**: the exec lane starves its own end-of-pass flush/reap while a fiber
  yield-loops (no product fiber does); peer unlink/rename ~10 s wait for the slot offer during
  handover; multi-worker per kernel queue; no-opendir; best-effort caches that still allocate under
  a spin lock (`kv_fs` parents, `manager` identities, `plane_arm` declared — their drops are
  counted; move the allocation out of the lock); the pjdfstest row's daemon refuses unmount while
  `prove` is in the mount and never retries (1 leak in ~230 VM gate runs).
- **Owner rulings pending from `rung-10-seqwrite.md`**: (a) fiber-aware device waits (revise
  decision 0001 item 2 so a device write parks only its fiber; ≈ +50 % on big writes); (b)
  zero-copy big writes from the slot (charter §6.2 item 1; with (a) ≈ 2×). Recommended both, (a)
  first — this is where the performance stretch starts.
- **Hardware-owed**: the box brackets on reference hardware, zcrx against a real HDS NIC, PR preempt
  on a real substrate — the 200 G venue below covers the first two.

## The performance venue (owner, 2026-10-18)

Five hosts with 2 × 200 GbE each (40 GB/s measured over the two links). Plan: 1 MDT host, 2 OSS
hosts, 2 client hosts (SqueezeFS has no target-side HA, and real multi-writer needs two client
hosts; the second "MDT host" becomes a client). Needs an ssh venue class per host in
`scripts/venue` (the kamrui pattern), the sqz kernel on the clients only, `nvmet` + `nvmet-tcp` on
the targets (6.13+ for PR). The charter's bandwidth bars are reachable there; the zcrx read lane
needs mlx5 with tcp-data-split; `transport=rdma` is a small addition if TCP's CPU cost bites.

## Resuming here

1. `scripts/venue status`; `nproc` must read 32.
2. `scripts/gate-train` on the current head until `streak 3/3`, then stop it (it retires the
   generic/112 flakes row).
3. Owner's pick for the performance stretch: the local levers (`rung-10-perf-baseline.md`:
   `layoutlookup`, `slabcopy`) or standing the fabric venue up first.
4. The release window, when called: `SQZ_VENUE_SLOT=1 scripts/venue 'SUITE_RUNS=1 scripts/suites/ltp.sh'`,
   then `SQZ_VENUE_SLOT=1 scripts/venue 'SQUEEZEFS_FSYNC_PROMOTE_STAGED=on SUITE_RUNS=1 scripts/suites/fstests.sh'`
   (one at a time; `scripts/venue zig build` first — the suites run the installed binary).
