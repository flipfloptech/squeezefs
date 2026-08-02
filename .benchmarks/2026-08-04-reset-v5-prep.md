# 2026-08-04 — Reformat-window PREP (reset-v5): script, v2 kernel, runbook

**Campaign class: PREP ONLY.** Branch `feat/reset-v5-prep` (off dev tip
`d806e5c`, rebased onto `68e8474` when the rewrite-P0 merge landed
mid-campaign — the anticipated P0). The window itself is a separate
execution campaign (`docs/reset-v5-window-plan.md` is its runbook). Cluster
touched **READ-ONLY + file staging + journal lines only**: no resets, no
mounts, no NIC changes, no installs.

**The epoch decision consolidated here (USER, 2026-08-02):** reset-v5
formats the cluster CONVERGED — 1 meta + 2 data namespaces per node × 5
nodes (aqr37/38/39 at .191/.192/.193, aqs38 at .195, oss2 at .196) = 5 meta
volumes + 10 data namespaces. Dynamic routing (bit 6) makes the width
knob-free; the current store is epoch-locked pre-bit-6 and the window
reformats it.

## 1. Deliverable 1 — `tests/cluster_reset_v4.sh` (the converged reset)

Authored from the PATCHED v3 fetched read-only from
`squeeze-test:/scratch/tmp/cluster_reset_v3.sh` (the reset-v4 epoch's field
script; the repo copy `tests/cluster_reset.sh` was STALE against that
lineage — pre-oss2 node map, kind-split roles, `--subnqn`-flag disconnect).
What makes v4 (commit `1893a64`):

* **Converged node map**: every node symmetric (1 × m0 mds-class
  memory-backed null_blk + `OSS_NAMESPACES=2` × d0/d1 per `OSS_BACKING`,
  nullblk default 48 GiB) — NQNs `nqn.2026-07.io.squeezefs:<node>-{m0,d0,d1}`,
  15 subsystems × 2 fabric paths = 30 connects.
* **Enumeration-based teardown** (the oss2 lesson, journaled 2026-08-01 —
  the pre-reset journal was rotated by the re-provision, the lesson lives
  here and in the script comments): per node, sweep EVERY nvmet port whose
  `addr_traddr` matches the node's fabric IPs (unlink subsystem links →
  `rmdir` the port FIRST — releases the listeners), then every subsystem
  under our NQN prefix (disable + rmdir namespaces → unlink allowed_hosts →
  rmdir subsystem), with a loud residue check. Never expectation-named
  teardown.
* `nvmeof disconnect` takes the SUBNQN **positionally** (the v3 patch).
* Format: 5-meta URI (all five m0 in node order) + 10-data URI (node-major),
  **NO `--meta-slots`** (hard error now; width DERIVED). Mount:
  `--interception --allow-other --log-file /scratch/tmp/logs/sqz.log` (the
  2026-08-02 convention — /tmp banned); verify echoes `build_commit` +
  `meta_routing_width` off the stats inode.
* Loud venue-epoch banner (reset-v5, converged, node map) before the YES
  gate.

Validation: `bash -n` + `shellcheck` clean. **Staged** (file drop, NOT
executed) to `squeeze-test:/scratch/tmp/cluster_reset_v4.sh`, sha256
`656dcaf617bd77d72ede18dc8698f134822440ca5bfb8e8d30cf163bc84d3055`,
byte-verified after transfer, journal line 2026-08-02T11:27:48Z.

## 2. Deliverable 2 — the v2 sqz kernel

### 2.1 Patch 0027 (commit `136ffba`)

`docker/kernel-sqz/patches/0027-sqz-FUSE_TIME_LIMITS-INIT-advertisement-of-inode-tim.patch`
— V2-CANDIDATES.md rank 1, the ENTIRE v2 kernel delta (~20 lines):

* uapi: `fuse_init_out` carves `time_min`/`time_max` i64s from
  `unused[11]` → `unused[3]` placed FIRST so the pair stays naturally
  aligned and the struct stays the uapi 64 bytes (layout arithmetic
  verified: offsets 48/56, total 64); init capability
  `FUSE_TIME_LIMITS (1ULL << 62)` — far above upstream's bit-42 watermark.
* `fs/fuse/inode.c`: advertised in `fuse_send_init`; consumed in
  `process_init_reply` beside the `time_gran` block, guarded on the echoed
  flag + nonzero `time_max` → `sb->s_time_min/max`, so VFS
  `timestamp_truncate()` clamps incore exactly where the daemon clamps
  durable state. Converts fstests **generic/634** to expected-PASS on
  sqz-kernel hosts ONLY (the fleet-kernel adjudication stays pinned).
* Apply-cleanliness verified the recipe's own way: containerized dry-run —
  fresh tarball extraction + all **27 patches `patch -p1 --fuzz=0` clean**.
* Recipe docs: SERIES.md gains the 0027 authorship entry; README records
  the v2 delta AND the **strata ruling (USER 2026-08-02): the kmod-sqzfuse
  stratum is KILLED — two strata only, stock-graceful → full `-sqz`
  kernel** (matches dev's `e9ae373` V2-CANDIDATES.md record, landed
  independently mid-campaign).

### 2.2 The v2 RPM build (the container build IS the gate)

`docker/kernel-sqz/build.sh` run under the dev-box slot/thermal law: the
launch WAITED for the box to drain cargo work (the concurrent agents' slots
respected — the poll gated both runs), recipe's own `--cpus 16` + `nice`,
zenpower watchdog armed (≥90 °C ⇒ `podman pause`, ≤75 °C resume).

**Run 1** passed every gate step (27/27 patches `--fuzz=0`, ENABLE CHECKLIST
32/32, `-sqz` tag, three RPMs written by rpmbuild) and then died in the
recipe's artifact copy — a latent harness bug: under `set -euo pipefail`,
`found=$(find A MISSING_B | wc -l)` exits on find's nonzero status before
the no-RPMs guard runs (the `/root/rpmbuild` probe path does not exist in
the container). Fixed in commit `c961cb2` (absent-probe-tolerant finds; the
guard stays load-bearing). Per the counted-run discipline the fixed recipe
re-ran **FROM ZERO as the acceptance pass**.

**Run 2 (acceptance): PASS from zero, rc=0** — 27/27 patches fuzz=0 (0027
named in-log), checklist 32/32, `sqz kernel build complete: 6.19.14-sqz`.
The thermal watchdog FIRED during this run (foreign agents' load pushed
Tdie to 90 °C+; pause at 11:57:59Z, resumed after cool-down) — the
pause/resume law worked; the wrapper's paused-container listing needed
`podman ps -a` (operational note, wrapper-local).

Artifacts (see `SHA256SUMS` staged alongside):

* `kernel-6.19.14_sqz-1.x86_64.rpm` — sha256
  `a7b18627a78726d2580566b4f3e326626170d5b5021c4af82289093a1fc361b7`
  — **CAUTION: same FILENAME as the v1 RPM** (`binrpm-pkg`'s release
  counter resets on fresh extraction); identity is the sha + the `v2/`
  staging directory. Journaled loudly.
* `kernel-devel-…` `2cde632e…`, `kernel-headers-…` `f6c4ee8a…`,
  `config-6.19.14-sqz`, `SHA256SUMS`.

**Staged** (file drop + journal 2026-08-02T13:46:17Z, byte-verified far
side via `sha256sum -c`; NO install, NO boot) to
`squeeze-test:/scratch/tmp/kernel-sqz/v2/`. Boot rides the window's Phase 1
(one-shot grub discipline; ELRepo 7.1.2 stays permanent default).

### 2.3 Daemon-side bit-62 verdict: NOT previously covered → landed here

Checked the fzc lineage (dev tip includes `190f88c` kmbuf arm + the
geometry law): **no `FUSE_TIME_LIMITS`/time_min/time_max anywhere** in
`crates/fuse3` or `src/` — the fzc branch did NOT cover it. Landed
red-first on this branch:

* `2aec355` **test(fuse3)** — the negotiation contract (4 tests: fold
  placement bit 62 ↔ flags2 bit 30; offered ⇒
  `Some((-9_223_372_036, 9_223_372_036))` — the whole-second interior of
  the i64-ns storage word, floor conservative by one second so a kernel
  clamp to `(sec, 0)` always round-trips exactly; not-offered ⇒ `None`,
  reply BIT-IDENTICAL; wire ABI 64 bytes with the i64s at offsets 48/56).
  RED verified against the skeleton (`negotiate_time_limits` → `None`).
* `9a5a5e4` **feat(fuse3)** — `negotiate_time_limits()` + the
  `handle_init` arm: echo the flag in flags2 + populate the pair; stock
  kernels unaffected (gated on the offered bit; kernel additionally gates
  on nonzero `time_max`).

## 3. Deliverable 3 — the runbook

`docs/reset-v5-window-plan.md` (commit `836e252`): nine ordered phases
consolidating every owed row from the five evidence notes
(post-reset-baseline recipe; il-hold-probe §6; zcrx-z2 §5; fuse3-zc §5;
rewrite-program-p0 §5 — which landed mid-prep exactly as anticipated) plus
the v2-kernel obligations (boot + capability matrix + generic/634
single-test). Carries: the kernel/pair matrix (ONE window pair = dev tip at
window start, KD-7 build step stated — levers not binary swaps; ONE v2
boot, early), standing settle/labeling laws, journal discipline, the
per-phase abort ladder (v2 boot failure degrades to 7.1.2 with Phases 6/8
deferred; baseline sickness stops the perf program; the stop-the-line
tripwire set), per-phase wall-clock (**≈ 13 h total**), and the owed-row
checklist.

## 4. Gates (this branch)

| Gate | Verdict |
|---|---|
| v4 script: `bash -n` + `shellcheck` | PASS (clean) |
| kernel recipe: container build (27 patches fuzz=0, checklist 32/32, RPMs produced) | **PASS from zero (run 2, rc=0)** — run 1 = harness bug in artifact copy, fixed `c961cb2`, count restarted |
| fuse3 suite (`crates/fuse3` `cargo test --all-features`) | PASS — 64/64 + 4 new contracts |
| fuse3 clippy `-D warnings` + fmt | PASS |
| root clippy `-D warnings` + fmt (both-roots law) | **PENDING — NOT RUN** (the prep session ended before the root-side gate; owed before this branch merges to dev) |
| targeted root suites (`attr_refresh`, `transport_geometry`, `killpriv_v2`) | **PENDING — NOT RUN** (same close-out gap — the fuse3 change is INIT-reply-additive and stock-posture bit-identical by the pinned contract, but the root-side proof is owed, never assumed) |
| markdown link check (docs-class files) | PASS |

**Close-out note (resumed session, 2026-08-04):** the authoring session died
before the two root-side gate rows ran; they are recorded PENDING above, not
assumed. What the resume re-verified (derivable without execution): `bash -n`
clean on both touched scripts (`tests/cluster_reset_v4.sh`,
`docker/kernel-sqz/build-kernel.sh`); shellcheck 0.11.0 clean on
`cluster_reset_v4.sh` (the `build-kernel.sh` SC1090/SC2086 findings on the
toolset `source` line pre-exist at the dev base `68e8474` and are untouched
by this branch's `c961cb2` delta); and every file/commit this note cites
present in-tree at the stated SHAs (patch 0027, SERIES/README/V2-CANDIDATES,
`tests/cluster_reset_v4.sh`, `docs/reset-v5-window-plan.md`, all six source
evidence notes). Cluster-side records (the v3 fetch, the two staging drops +
sha256s, the journal lines, both container-build runs incl. the thermal
pause) are the executing session's record and were NOT re-executed — nothing
here re-touched the cluster.

## 5. Cluster hygiene ledger

READ-ONLY + file staging honored: fetched `cluster_reset_v3.sh` (read),
staged `cluster_reset_v4.sh` + `kernel-sqz/v2/` (file drops, sha-verified),
two journal lines appended. No resets, no reformats, no mounts, no NIC
changes, no installs, no raw-device writes, no storage-node changes.

## 6. What remains before the window can run

(Consolidated from `docs/reset-v5-window-plan.md` — the runbook governs;
nothing below has run.)

1. **Branch close-out (repo-side, owed pre-merge):** the two PENDING §4
   gates — root clippy `-D warnings` + fmt (both-roots law) and the
   targeted root suites (`attr_refresh`, `transport_geometry`,
   `killpriv_v2`) — then the normal ff-only merge of
   `feat/reset-v5-prep` to dev. The runbook's window pair = **the dev tip
   at window start**, so the window cannot see this prep's daemon-side
   bit-62 arm until the merge lands.
2. **Window Phase 0 (build-at-window-start, KD-7):** `task build:rocky8`
   on the then-current dev tip, in-container asserts, stage + sha-verify
   the pair to `squeeze-test:/scratch/tmp/`, journal. Deliberately NOT
   done at prep (the tip is still moving).
3. **USER approval in the session charter** for the destructive Phase 2
   (the YES gate is in-script, but the charter approval is the runbook's
   go/no-go item), plus the box-state preflight (grub one-shot state,
   root-fs headroom, staged-artifact sha re-verify, re-read of the five
   source notes at their dev-tip state per the authority law).
4. **The window itself** (≈ 13 h, phases 0–9): v2 one-shot boot +
   capability matrix → `cluster_reset_v4.sh` (the destructive act) → the
   canonical baseline table → the owed-row programs (rewrite-P0 §5,
   il hold-probe §6, fuse3-zc §5, zcrx Z2 §5) → generic/634 on live
   bit 62 → close-out + the post-reset-v5 baseline note. All execution
   evidence belongs to that campaign's note, never this one.
