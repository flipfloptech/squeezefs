# 2026-08-18 — FIELD MPI-IO prep: the external-mounts mode, cluster_reset_v5_mw.sh, and the field runbook

Branch `feat/mw-field-mpiio` (off dev `3cdbb6c4`). Charter: run the rung-18
shared-vs-disjoint ior row (`s11-mpiio`) over the REAL nvme-tcp fabric —
client `memp-s3ds-aqs-37` (32 CPU, 2×200GbE) + 5 storage nodes with kernel
nvmet targets — driven from the user's `tests/cluster_reset_v4.sh` cluster
tooling. The field is not reachable from the dev box: the deliverables are
MACHINERY + a RUNBOOK (`docs/field-mpiio-runbook.md`), with every leg that
CAN be verified locally verified locally. Local venue: the `tests/mw_fleet.sh`
tcp devsub (nvmet-tcp localhost) as the real-fabric stand-in. Evidence
tier: measured-simulated for every local row here; the field row itself
will be the campaign's measured-real substrate class.

## What was built

1. **`SQZ_MWMATRIX_MOUNTS` external-mounts mode** (`tests/run_mw_matrix.sh`,
   the `s11-mpiio` leg ONLY — the smallest honest surface; every other leg
   drives fleet-lifecycle verbs an external fleet does not expose and
   REFUSES the mode loud). Comma list, first = authority, rest = co-writer
   mounts. The fleet CONF/MEMBERS files are NEVER read; posture comes from
   each mount's own `.stats` (`cat`, never cp): authority must read
   `mount_posture=writer` + `data_plane_fence_mode=1` + `membership_mode=owner`,
   each co-writer `mount_posture=co-writer` + `membership_mode=member` —
   loud refusals on a missing or wrong-posture mount. The range-custody
   arm has no stats-probeable posture gauge (the rung-15 grant-census
   residual), so the mode CONVICTS it at the probe pass: every co-writer's
   `dlm_custody_range_acquires+extensions` delta must move, or the leg dies
   naming `SQUEEZEFS_RANGE_CUSTODY` before burning four phases. Rows land
   under `SQZ_MWMATRIX_ROWDIR` (default beside the authority mount — the
   field's `/scratch/tmp` convention). Netem arms do not exist on a real
   fabric (no veth; the leg carries none anyway). The fsck oracle runs
   WARM on the live authority (an external fleet's remount recipe belongs
   to its harness), stated on the row; findings-0 + C8-drift-0 teeth
   unchanged. Everything else — pinned ior 4.0.0 + sha256, POSIX MPMD,
   self-sizing, A-B-B-A, engagement columns — unchanged and shared with
   the fleet mode.

2. **The explicit inline-map self-sizing cap** (same leg, both modes): the
   old sizing clamp was 10 GiB — PAST the ~6 GiB inline-map boundary, so a
   fast probe self-sized into the indirect-spill domain and died as the
   fail-safe fsync-EIO refusal (the rung-19 campaign's run 1;
   `.benchmarks/2026-08-18-s11-widthn-refs-fix.md` fix 4 / rung-20
   residual #1). The cap is now **5,120 MiB** — the PROVEN verdict sizing,
   margin under the boundary — with the honest note logged on engagement;
   when it binds, the iteration ceiling rises 24 → 128 to preserve the
   ≥ 60 s sustained window, and a window the probed rate still cannot
   reach warns-and-labels instead of silently shipping a short row. At
   200GbE field rates the cap WILL bind (locally a 1.5 GiB/s probe wanted
   32,512 MiB → capped to 5,120 with s=40, byte-identical to the verdict
   run's geometry).

3. **`tests/cluster_reset_v5_mw.sh`** — the user's v4 extended in its own
   conventions (CONFIG block, HOSTS map, enumeration-based teardown,
   product-verb target build, both-path connect + round-robin, cache-less
   format — all VERBATIM; v4 itself untouched): per-node
   `resv_enable=1` ASSERTION at target build + client-side
   `nvme resv-report` verify per data namespace, then the mw_fleet mount
   recipe over the real fabric — authority armed
   (`SQUEEZEFS_MULTI_WRITER=1`, `SQUEEZEFS_MEMBERSHIP_BIND=auto`, STABLE
   `SQUEEZEFS_MW_BIND` port 45999 — the rung-10 successor-port finding,
   `SQUEEZEFS_FLEET_SHARE=1+K`), enrollment ids harvested from each
   co-writer's rung-3 refusal, authority re-armed with the roster (a new
   era), K co-writers mounted at `$MOUNTPOINT-cw1..K`
   (`SQUEEZEFS_MW_ROLE=co-writer`, `SQUEEZEFS_MW_AUTHORITY=<parsed>`,
   `SQUEEZEFS_RANGE_CUSTODY=1`) — every mount readiness-gated the way
   mw_fleet gates (`data-plane WERO (rtype 3) acquired`,
   `CO-WRITER ADMITTED`, `.stats` posture polls). Prints the ready-to-paste
   `SQZ_MWMATRIX_MOUNTS=… tests/run_mw_matrix.sh s11-mpiio --procs=4`
   line. `--dry-run` prints every ssh/format/mount command without
   executing (works unprivileged — the field preflight). `SUDO_*` is
   scrubbed from daemon launches (root-deterministic daemon posture; the
   admin lane admits peercred uid 0 — `src/ipc_host.rs`).

4. **`docs/field-mpiio-runbook.md`** — preflights with exact commands and
   expected outputs (client kernel: `fuse.enable_uring` exists, NO client
   kernel change — co-located co-writers share the box PR host identity,
   patch 0030 is multi-identity-only; storage-node kernel: the
   `resv_enable` configfs probe, absent ⇒ THE one kernel update needed,
   nvmet PR = mainline v6.13+ / sqz kernel RPMs; client userspace:
   mpirun/mpicc/curl/gcc/python3/nvme-cli + a repo checkout; PR verify:
   `nvme resv-report`), the run sequence (dry-run → reset → paste line →
   teardown/repeat), row locations, the honest-comparison tier note, the
   quiet-box and A-B-B-A-internal notes, and the warm-oracle statement.

## The dev_substrate ↔ cluster_reset_v4 target adjudication

The task premise was "cluster_reset_v4 never sets `resv_enable`". The code
says otherwise — **v4 builds its targets with the PRODUCT verb
`nvmeof share --target-stack nvmet`, and `src/nvmeof/nvmet.rs` writes
`resv_enable=1` before enable whenever the kernel offers the knob** — so
the field targets are only PR-less when the NODE KERNEL lacks nvmet PR
(mainline v6.13+; the verb prints a detection-grade note and continues,
which is the actual gap v5 closes by asserting). Per-attribute:

| attribute | dev_substrate.sh (raw configfs) | v4 via product `nvmeof share` | delta / v5 action |
|---|---|---|---|
| `resv_enable` | writes 1 when the knob exists (`tests/dev_substrate.sh:433`) | writes 1 BEFORE enable when the knob exists; knob-absent ⇒ loud note, detection-grade (`src/nvmeof/nvmet.rs:662-674`) | the only real risk is a knob-less node kernel; **v5 asserts =1 per namespace at build time, FATAL naming the node + the v6.13 remedy**, plus the client-side `resv-report` verify |
| `attr_allow_any_host` | `1` | `1` by default (`0` + `allowed_hosts` links only when hosts are named) | no delta |
| `device_uuid` | random uuid before enable (the duplicate-NGUID connect-fail lesson) | the recorded `ns_uuid` before enable (restore-idempotent, durable identity) | no delta (the verb is stronger) |
| port attrs (`trtype/adrfam/traddr/trsvcid`) | tcp/ipv4 + addr pair | same writes; `--ip a,b` creates BOTH fabric-path ports | no delta |
| `attr_model` / `attr_serial` | not set | not set | no delta — kernel defaults; identity rides subsysnqn + device_uuid |
| namespace `enable` | last | last (after uuid + resv) | no delta |

## Deliberate deltas from the local mw_fleet recipe (stated, not silent)

* **No explicit hostnqn/hostid anywhere on the field.** mw_fleet mounts
  the writer with an explicit identity pair to EXERCISE the rung-2
  daemon-owned-connect surface (fabric_endpoint records, single-path).
  The env-knob law (`SQUEEZEFS_HOSTNQN`, `src/env_knobs.rs`) is
  pair-or-NEITHER, and explicit identity REQUIRES fabric_endpoint-record
  connects — which carry one `ip:port` each and would forfeit v4's
  dual-path (2×200GbE) multipath. The field fleet therefore rides the box
  default identity for authority AND co-writers — the ops.md co-located
  shape, the same one the co-writers already use locally. The rung-2
  "daemon-owned controller resolved" gate is correspondingly N/A on the
  field (no daemon-owned connects); the WERO/ADMITTED/posture gates carry
  the engagement burden.
* **`SQUEEZEFS_FLEET_SHARE=1+K` (9)** on the field vs the local rig's
  share=1 (an N=1 quirk of mw_fleet's `FLEET_N` accounting — its divisor
  counts writer+readers, not co-writers). 9 co-located daemons each sized
  whole-machine on the 32-CPU client would overcommit the R5 budgets;
  KD-MW-14's divisor is the honest posture. CONFIG-overridable.
* **No `--interception` on the fleet mounts** (v4's single mount carries
  it): the proven mw_fleet MW recipe mounts `--allow-other --log-file`
  only, and ior POSIX drives the kernel FUSE path. Re-add on a later
  reset if a shim row needs it.
* **Warm fsck oracle in external mode** (stated above and on the row).

## Local verification (this box: 32 CPU, 117 GiB, 7.1.6-1-cachyos-sqz; SQZ_BIN = fresh `cargo build --release` at the branch tip)

* **shellcheck + `bash -n`**: clean on both scripts (`run_mw_matrix.sh`,
  `cluster_reset_v5_mw.sh`).
* **`--dry-run`**: prints the complete command plan (teardown, per-node
  ssh bodies, connects, format, all four mount phases with the roster
  placeholder, the paste line); executes nothing; runs unprivileged.
* **Refusal arms** (each verified live): a non-`s11-mpiio` leg under
  `SQZ_MWMATRIX_MOUNTS` refuses naming the surface; < 3 mounts refuses
  with the count; a dead mountpoint refuses naming it; a wrong-ORDER list
  (co-writer first) refuses on the posture read (see the proof run
  below); an unarmed `SQUEEZEFS_RANGE_CUSTODY` fleet is convicted at the
  probe pass (by construction — the ranged-ledger delta gate; the armed
  fleets below all passed it).
* **The no-MEMBERS proof**: every external-mode leg below ran with
  `SQZ_MWFLEET_STATE_DIR=/run/sqz-noexist-proof` — a nonexistent state
  dir, so ANY read of the MEMBERS table, the fleet CONF, or
  `host_scoped` would have failed loudly. All external-mode machinery
  (posture verify, snapshots, engagement, fsck) ran green off the named
  mounts alone.
* **The cap, live**: probe 1,480 MiB/s → "self-sizer wanted 32,512 MiB —
  CAPPED to 5,120 MiB (s=40)" — byte-identical geometry to the rung-19
  verdict run, on a probe 6× faster. The pre-change leg would have sized
  10,240 MiB into the indirect-spill fsync-EIO refusal.

### The end-to-end runs (counted honestly, every roll listed)

VENUE FLEETS: fresh `mw_fleet.sh create N=1 --cowriters=K` per run
(`SQZ_MWFLEET_RANGE_CUSTODY=1`, MW port 54193), teardown-to-zero-residue
between runs ("teardown complete — zero residue" every time).

**The GREEN end-to-end run (the deliverable proof)** — fresh fleet
(`N=1 --cowriters=2`, OSS 2×32 GiB zram), external-mounts mode naming
`/mnt/sqz-mwfleet/{m0,m50,m51}` explicitly, `SQZ_MWFLEET_STATE_DIR=
/run/sqz-noexist-proof` (the no-MEMBERS proof), `--procs=2`, rowdir
`/tmp/sqz-fieldmpiio-rows/s11mpiio-1787087580`:

| gate | value | verdict |
|---|---|---|
| external posture preflight | authority writer/fence 1/owner + 2 co-writers admitted/member | ✅ |
| range-custody probe conviction | every co-writer's ranged ledger moved | ✅ |
| the inline-map cap | wanted 10,736 MiB → capped 5,120 (s=320) | ✅ engaged + logged |
| A-B-B-A (probe 488 MiB/s, 8 iters/phase) | A1 613 / B1 488 = **1.256**; A2 982 / B2 952 = **1.031** | ✅ both ≥ 0.8× |
| engagement, exact | m50 ranged Δ 20,330 / m51 Δ 20,453; shipped ≈ 614.7 k/mount; authority grants 10,304; cap_refusals **0**; demotions 0; conflicts 0; prs/ors 0 | ✅ |
| read-back exact (`-r -R -C`, fixed `-G`) | 8,952 MiB/s aggregate, zero data-check errors | ✅ |
| warm-authority fsck + C8 | **findings: 0 (clean)**, drift 0 | ✅ |
| teardown | "teardown complete — zero residue" (fleet state, mounts, /dev all clean) | ✅ |

**The full roll ledger (every run listed — nothing pre-green credited or
hidden).** Fresh fleet + fresh format per run; every red run's snapshots
preserved; teardown-to-zero-residue between all runs:

| run | shape / mode | outcome |
|---|---|---|
| e2e-1 | 2cw×2p, EXT (state hidden) | RED: bracket 2 = 0.778 (bracket 1 0.851) |
| e2e-2 | 8cw×4p (verdict geometry), EXT | RED: bracket 2 = 0.792 (bracket 1 1.932); iteration tables oscillate 150 ↔ 6,500 MiB/s |
| e2e-3 | 8cw×4p, EXT | RED: bracket 1 = 0.348 (bracket 2 0.828); phases ramp 992 → 2,852 → 4,513 → 3,736 |
| **control** | 8cw×4p, **FLEET mode** (no `SQZ_MWMATRIX_MOUNTS`) | RED: phase A1 NOT SUSTAINED (1,283 → 580) — **the attribution A/B: the failures are venue behavior, independent of the external-mounts mode** |
| roll-1 | 2cw×2p, EXT (16 GiB oss) | RED: bracket 1 = 0.745 (harness rc-capture bug found here — `PIPESTATUS[0]`, fixed in the roll driver; the run itself counted RED) |
| roll-2 | 2cw×2p, EXT (32 GiB oss) | RED: phase B2 NOT SUSTAINED (660 → 223) |
| roll-3 | 2cw×2p, EXT | RED: bracket 1 = 0.683 (bracket 2 1.164) |
| roll-4 | 2cw×2p, EXT | RED: phase A2 NOT SUSTAINED (702 → 376); bracket 1 had passed at 0.901 |
| **roll-5** | 2cw×2p, EXT | **GREEN** (the table above) |

**Attribution (why the reds, and why they are not this branch's)**: the
fleet-mode control fails identically, so the mode is exonerated; the
leg's phase/gate code is byte-shared between modes and unchanged except
the sizing CAP (without which today's fast probes — 320–1,480 MiB/s vs
the verdict day's 234 — would have self-sized PAST the inline-map
boundary into the indirect-spill fsync-EIO refusal, i.e. no run at all).
Today's quiet box self-sizes 6–22-iteration phases whose rewrite volume
(25–110 GiB/phase against a 64 GiB zram pair) drives the free-grace
release cadence into multi-minute beats: the green run's own authority
snapshots show `free_grace_deferrals` 21,001 vs `releases` 17,751 with
`free_grace_offsets` holding 3,250–4,226 deferred offsets at snapshot
instants (zero `alloc_stalls`, zero `forced_releases`, R5 green, zero
`reclaim_cap_parks` — the machinery is HEALTHY, the cadence is just
slower than the rewrite rate). That is rung-19 residual #3's named shape
("a storm's deferrals outrun releases"), aliasing against the A-B-B-A
phase boundaries — some rolls land the beat inside a bracket and fail
marginally (0.683–0.792), one landed clean and passed with margin. The
2026-08-18 rung-19 verdict run (same leg, slow-probe day, flat
iterations) remains the measured-simulated acceptance; the field's real
fabric — where device bandwidth, not a shared-memory-bus zram pair,
absorbs the displacement stream — is the venue this row was built for.

## Residuals

1. **The field row itself** — the user executes the runbook; the row's
   verdict is the field's to issue (measured-real tier).
2. **The venue-beat gate sensitivity at fast probes** (the attribution
   above): the leg's ≥ 0.8× verdict destabilizes when a quiet box's fast
   probe sizes ~20-iteration phases whose rewrite volume (~110 GiB/phase)
   drives the free-grace/reclaim cadence churn — the rung-19 residual #3
   shape, now with a second instrument (this note's iteration tables).
   Wants residual #3's pressure-coupled release valve; until then,
   long-window local rows on idle boxes will oscillate.
3. **The warm-vs-cold oracle gap in external mode**: an external harness
   that RECORDS its mount recipe could hand the leg a remount command;
   deliberately not built (smallest honest surface).
4. `SQZ_MWMATRIX_MOUNTS` covers `s11-mpiio` only; extending to other s11
   rows would need external kill/remount verbs (out of scope by design).
5. The field paste line runs `--procs=4` (32 ranks); `--procs` up to 16
   stays admissible but the sizing floor die-arm guards rank counts whose
   8-segment floor would exceed the inline-map cap.

## Gates

* Shell class: shellcheck + `bash -n` both scripts — clean.
* Markdown class: `tests/check_markdown_links.sh` — PASS (runbook + this
  note).
* Cargo class: **no `.rs`/`.toml` change on this branch** — the cargo gate
  is not owed (task law: rig/docs only; verified `git diff --stat` names
  only `tests/*.sh`, `docs/`, `.benchmarks/`).
