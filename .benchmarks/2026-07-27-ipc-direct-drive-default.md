# 2026-07-27 — DIALED P1.5: direct-drive for governor-denied misses on DEFAULT mounts

Branch `perf/ipc-direct-drive-default` (off dev `1482131`, unmerged
pending review). Commits: red `6d05527` (the policy contract — the
default-mount governed-miss ladder, clamped-denial accounting, the
buffered and stream-classified controls, the non-reserving governor
peek), green `ecb994a` (the wiring). Contract suite:
`tests/ipc_direct_drive_tests.rs` §7–8 (4 fixture tests + 1 pure-API
pin added to the P1 suite; 3 red at `6d05527`).

**The finding (2026-07-26, 6-node field cluster):** on the
default-posture mount the shim served **0 ops via direct-drive**
(`ipc_async_handoffs` == all misses) — the P1 prelude was ddt-only by
policy. Post-governor, a **governor-denied miss on a default mount is
semantically identical to a device-true serve**: ranged device read, no
tier publish, nothing to invalidate. Those ops — the majority on
working sets ≫ budget — now direct-drive. Field target shape: 4k
O_DIRECT randread, 32 GiB set vs ~8 GiB budget, ~197–214k shim vs
269–276k kernel on that box.

## 1. Design (the decisions the charter asked to be reported)

- **The prelude runs the admission decision synchronously** on the IPC
  service thread, in dispatch order, every step latch-free single-word
  atomics / moka probes (verified: no lock crosses the prelude — the
  only mutex in the flow remains the engine's SQ mutex at submit, the
  shipped P1 design):
  1. **Tier probe first, unchanged**: the §5.5.1 sync fast path
     (staging ring → R4 hot tier → NVMe read cache) still serves
     O_DIRECT tier hits from tier — the 2026-07-15 hybrid-serve
     directive stays law. Only the **Miss** demotion enters the new
     path, and only for **O_DIRECT bindings** (buffered ring reads are
     byte-for-byte unchanged; pinned).
  2. **Dispatch parity**: `second-touch` admission mode only (the
     default; `always`/`never` diagnostic modes keep verbatim handler
     semantics) + the §5.6 `ranged_eligible` rule (threshold +
     stream-classification veto) — a read the handler would not range
     never direct-drives (whole-block + pipeline dispatch stays the
     handler's).
  3. **The P1 shape/custody prelude verbatim**
     (`ipc_direct_read_probe`): overlay screens, 795 custody snapshot,
     fallback-is-correctness for anything ambiguous. All P1 CQE
     machinery (795 revalidation, arena-DMA/64 KiB bounce legs,
     mapping-Arc teardown pins) applies unchanged.
  4. **The admission decision**: `DataRouter::
     ranged_escalation_candidate` — the handler dispatch's
     ghost/Red/cooldown prefix factored into ONE definition used by
     both sites — **records the ghost touch either way** (the charter's
     care: if direct-drive skipped the bookkeeping, hot subsets would
     never earn admission and the skew regime would regress; the ladder
     test's escalation phase is the weakening proof). A candidate then
     takes the governor's **non-reserving peek**.
- **`AdmissionGovernor::escalation_would_admit` (the peek)**: clamp
  verdict (ONE `clamp_engaged` definition shared with
  `allow_escalation`) + a plain token-availability load — **no
  reservation CAS**. DENIED ⇒ direct-drive, with the denial accounted
  by the peek and **no cooldown recorded** (the governor's design — hot
  keys retry and win the trickle). GRANT-shaped ⇒ the op rides the
  **unchanged handler path**, where `allow_escalation` remains the ONLY
  token-reservation site: the herd-safety argument is preserved — peeks
  can over-ADMIT into the handler near a token boundary (bounded per
  epoch; the losing racers degrade to handler-side ranged window
  reads), but can never over-SPEND the grant. Pinned pure-API: 64 peeks
  consume none of an 80-block grant; the authoritative reservation then
  finds all 80.
- **Ledger**: policy routings (mode / ranged-ineligibility / grants)
  ride the new **`ipc_direct_ineligible_policy`** row — the
  deliberate-routing sibling of the P1 refusal classes, NOT prelude rot
  (documented in design-read-path §Observability with its coherence
  tripwires). The only new counter; everything else is the existing
  `ipc_direct_drive_*` family.
- **Posture parity in the engine**: `read_device_true_reads` is now
  gated on the ddt escape (the handler counts it only under
  `device_true`; a default-mount direct-drive must not inflate the
  amplification-methodology ruler). All other governed accounting
  (`ranged_reads`, `ranged_read_bytes`, `read_odirect_requests`,
  governor `note_foreground`, `get_obj`) is posture-identical.
- **Racy-tolerance, documented and bounded**: a prelude-recorded ghost
  touch whose op then falls back to the handler (engine SQ-full /
  backend refusal / post-CQE 795 failure) is re-recorded by the
  handler's candidacy check — at worst one EXTRA touch = one earlier
  escalation, governor-bounded (the GhostTable's own tolerance class);
  fallbacks measured 0 on every counted row.
- **Miss-demotion counter semantics kept**: a direct-driven op is not a
  demotion (`ipc_fast_path_miss_demotions` keeps meaning "went to the
  async handoff", the ddt branch's own posture).
- **Loom**: not run — no new lock-free protocol and no ordering change
  (the peek is single-word Relaxed in the governor's existing
  racy-tolerant class; the candidate factoring is behavior-identical;
  the engine's mutex/CQE machinery untouched).

## 2. Substrate (labeled; the P1 rig, still up, verified knob-by-knob)

32-CPU box, 109 GiB RAM, kernel 7.1.4-1-cachyos. configfs null_blk
`sqzlat_oss0` (36 GiB memory-backed, `completion_nsec=235000`,
`irqmode=2`, bs 4096, 8 squeues, hw QD 128) → nvmet-loop →
`/dev/nvme1n1` (data); `sqzlat_mds0` (3 GiB) → `/dev/nvme2n1` (meta).
**Raw ceilings re-measured this session** (fio 3.42 on /dev/nvme1n1,
20 s): libaio 16×QD16 **473k** (clat 522 µs), libaio 32×QD32 **463k**
(clat 2175 µs) — ~1.5–3 % below the P1 session's 480.5k/477.3k, the
session-drift band that also brackets the ddt rows below.

Filesystem: fresh cache-less format (`sqmeta:///dev/nvme2n1
sqdata:///dev/nvme1n1`), 4 MiB blocks; mounts `--daemon --allow-other
--interception --mem-cache-size 1GB` (± `-o direct_device_true`),
`SQUEEZEFS_IPC_SERVICE_THREADS=8`; 1 GiB mem cache ⇒ **hot tier budget
128 MiB (32 blocks)**. Datasets: big 16 × 1.5 GiB (24 GiB ≫ budget,
shared by both sides), mid 16 × 8 MiB (≈ budget), small 16 × 4 MiB
(≪ budget), warm-row 4 × 200 MiB. Instruments: **elbencho 3.1-10
(dynamic)** threaded rows, **fio 3.42** 16-process libaio fleet + the
zipf:1.1 skew row. Baseline side = dev `1482131` daemon+shim pair,
ship side = `ecb994a` pair (KD-7 verified: each shim embeds exactly its
own commit). Per-row counters from zero (fresh snapshot brackets);
sides in one session window; box quiet (full cargo suite + gates
completed BEFORE the counted rows).

**A-B-A drift bracket (multi-run discipline):** the ship side ran
second; a third pass re-ran the BASE binary after ship (`base2` for
ddt+dflt, `base3` for fit/zipf-kernel) to separate binary effects from
session drift. Rows below cite base / ship / base-bracket.

## 3. A/B (medians of 3; bracket = base binary re-run after ship)

### Default posture (the headline — all il rows engagement-exact)

| Row | base `1482131` | **ship `ecb994a`** | base2 bracket | verdict |
|---|---|---|---|---|
| **il t16 qd16, s4** | 328.7k | **399.8k** | 335.4k | **+21.6 % vs base, +19.2 % vs bracket; 0.889× the same-session ddt-il row, 0.85× raw ceiling** |
| **il t32 qd32, s4** | 314.5k | **409.4k** | 325.2k | **+30.2 % / +25.9 %; 0.841× ddt-il t32, 0.88× raw 32×QD32** |
| **16-proc fio libaio qd16** | 303.1k (clat 842 µs) | **409.4k (clat 624 µs)** | 321.3k (795 µs) | **+35.1 % / +27.4 %** |
| il t1 qd1 (10 s) | 3,440 (291 µs/op) | **4,256 (235 µs/op)** | 3,367 | **+23.7 %** (below the 242 µs raw RTT because ~8 % of f0's blocks are tier-resident under the trickle — the hybrid dividend, stated) |
| il sync t32 qd1 | 99.5k | **121.7k** | 99.8k | **+22.3 %** (sync-lane denied misses direct-drive too) |
| kernel t16 qd16 (context) | 309.6k | 327.1k | 326.5k | untouched path; ship ≡ same-window bracket |
| kernel t32 qd32 (context) | 299.1k | 325.0k | 330.0k | untouched; drift-banded |

**Bar 1 adjudicated (state the achieved fraction):** default-mount shim
cold rows reached **0.84–0.89× of the same-session ddt shim rows**
(399.8k/449.7k at t16qd16, 409.4k/486.6k at t32qd32) — from **0.69–0.62×
at baseline** (328.7k/473.4k, 314.5k/511.1k) and from the field's
0-engagement posture. The residual fraction is CPU on the saturated
5-thread engine shape (§5): the default posture pays the per-op sync
tier probe + admission decision the ddt posture skips by design, and
O_DIRECT tier hits deliberately keep serving from tier. Default-mount
shim now beats the default kernel path 1.22–1.26× (field shape: shim
was 0.73–0.77× kernel).

### ddt posture + P1 rows (unregressed)

| Row | base | ship | base2 bracket | verdict |
|---|---|---|---|---|
| ddt-il t16 qd16 | 473.4k | 449.7k | 445.1k | ship ≥ bracket (+1.0 %) — session drift, not the binary |
| ddt-il t32 qd32 | 511.1k | 486.6k | 486.1k | ship ≡ bracket; still ≥ the same-session raw 32×QD32 (463k) |
| ddt-kernel t16 qd16 | 346.3k | 338.9k | 326.3k | untouched path, drift-banded |
| warm fast path (4×200 MiB, sync t8) | 19.0k | **34.4k** | — | fast-path serve MIX identical (32.6k vs 32.1k serves — the health bar); the row total improved +81 % because this row's 800 MiB set ≫ 128 MiB budget makes its miss slice churn-class, and churn misses now direct-drive |

ddt engagement stayed byte-exact 100 % direct-drive
(`ipc_ops_read Δ == ranged_reads Δ == read_device_true_reads Δ ==
serves Δ == submits Δ = 6,291,456`, handoffs 0) — posture unchanged.

### Governor regimes on default mounts (the campaign's exact rows)

| Row | base | ship | bracket (base3) | verdict |
|---|---|---|---|---|
| fit-mid kernel (128 MiB ≈ budget) | 575.1k | 532.7k | 538.6k | −1.1 % vs bracket — unregressed (kernel path untouched; the −7 % vs first-pass base is the session drift the bracket isolates) |
| fit-small kernel (64 MiB ≪ budget) | 601.9k | 574.6k | 557.5k | ship +3.1 % vs bracket |
| fit-mid il | 1,045.1k | 1,029.4k | — | −1.5 %, inside this row's rep spread (763k–1,147k both sides); 84 % fast-path serves, escalations still fire through the ring (50/row), **zero denials** |
| fit-small il | 1,314.9k | 1,302.1k | — | −1.0 %; 1,047,912 of 1,048,576 ops = fast-path RAM serves; zero denials — fitting sets never clamp, pre-governor policy byte-identical |
| zipf 1.1 / 24 GiB, kernel (fio) | 341.0k | 335.3k | 340.1k | −1.4 % vs bracket, in-band |
| **zipf 1.1 / 24 GiB, il (fio)** | 351.2k | **403.1k** | — | **+14.8 %** — the skew regime IMPROVES through the shim: 155.5k tier serves/row (the hot subset lives in RAM) + **617 escalations/row earned through ring-recorded ghost touches** (bar: hot sets still converge) + 9.90M denied tail ops direct-driving |

## 4. Engagement & coherence verdicts (ship, per-row deltas from zero)

- **Engagement exact on every default il row**: `ipc_ops_read Δ ==
  ipc_direct_drive_serves Δ + ipc_fast_path_serves Δ +
  ipc_async_handoffs Δ` exactly (t16qd16 r2: 6,291,456 == 6,257,893 +
  33,036 + 527), with `ipc_async_handoffs Δ == ipc_direct_ineligible_
  policy Δ (+ inel_meta where present)` — every handoff is an accounted
  grant/policy routing, zero silent slices.
- **Governor coherence (the churn-row tripwire)**: denials ≈
  direct-drive serves — 5,336,940 / 6,257,893 = **0.85** on the t16
  churn row; the remainder is ghost first-touches (2¹⁶-slot collisions
  + epoch rolls over the 6,144-block population) and cooldown-window
  refusals, neither of which is a governor denial by design.
- **Admission spend bounded**: 287 escalations/row × 4 MiB = 1.12 GiB
  admission fetches vs 25.6 GiB foreground = **4.4 % ≈ the 5 % knob**;
  `read_admission_wasted_bytes` 1.07 GiB (the churn steady state — the
  waste IS the clamp's evidence).
- **Amplification**: ranged window bytes 25.63 GiB for 25.77 GiB of
  user 4k reads = 0.995× (tier hits pay no device bytes); + the 4.4 %
  admission trickle ⇒ total device-byte ratio ≈ 1.04×. Device-true
  ledger clean: `read_device_true_reads Δ = 0` on every default row.
- **Tripwires (both sides, post-rowset)**: `write_path_seed_read_bytes`
  0, `patch_edge_rmw_reads` 0, `ipc_descriptor_rejects` 0,
  `ipc_sessions_poisoned` 0, `fsck_findings` 0,
  `ipc_direct_drive_fallbacks_post` 0, `ipc_direct_drive_bounces` 0,
  `stale_binding_rebinds` 0.
- **Umount promptness**: 0.003 s (base) / 0.004 s (ship) immediately
  after the default direct-drive rows; daemon exit within the poll
  window every time.

## 5. What the CPU does now (recorded, live ship t32qd32 default row)

At 408k IOPS: the 4 owning service threads ~89–91 % CPU each (drain +
sync tier probe + admission decision + prelude + SQE publish), the
reaper ~94 % (enter + revalidate + complete), the other 4 service
threads ~7 % — the same 5-thread ≈ 4.5-core shape that served 525k on
the P1 ddt row now serves 408k on the default posture, i.e. the
0.84–0.89× fraction is the priced cost of the hybrid posture's extra
per-op work (tier probes + ghost/governor decision) on saturated
threads. The reaper remains the next binder (P1 residual, unchanged).

## 6. Falsification duties (all on the final pair)

- **Cargo contract suite** (`tests/ipc_direct_drive_tests.rs` §7–8, red
  at `6d05527`): the default-mount ladder — first touch direct-drives
  with `read_device_true_reads Δ = 0`; the SAME key's second touch is
  grant-shaped, rides the handler, and **escalates** (proving the
  direct-drive prelude recorded the ghost touch — a bookkeeping-skipping
  implementation fails this phase); the admitted block then serves on
  the sync fast path (tier-hit law). Clamped denials: engaged clamp +
  empty grant ⇒ each ghost-hit miss direct-drives with exactly one
  governor denial and NO cooldown (a cooldown-recording implementation
  fails the repeat-denial assertion). Buffered bindings never
  direct-drive; stream-classified files route to the handler via the
  policy ledger; the governor peek is non-reserving (64 peeks leave an
  80-block grant intact for the authoritative site). All 11 suite tests
  green on the final pair; P1's ddt/bounce/795/teardown tests unchanged
  and green.
- **Preload gate** `sudo tests/run_preload_gate.sh`: both legs PASSED
  end-to-end on the final pair — incl. leg 2l (direct-drive kill-9 soak
  ×5, **engaged +15,360 serves**, zero session/arena residue), the
  kill-9 write soak, fork-kill-parent, netns, libaio lifecycle
  orderings, engagement rows.
- **Mid-drive write races**: not re-run — the P1 fio randrw crc32c
  verify covered the 795 protocol under racing writers and NONE of that
  machinery changed; the default path adds only pre-submit policy steps
  (the write-race window and its revalidation are identical code).
  `write_visibility`, `write_through_coverage`, `extent_*`, `hybrid_io`,
  `preload_*`, `ipc_host`, `read_admission_governor`,
  `read_tier_refetch_churn`, `hot_block_tier` suites all green.

## 7. Gates

Full cargo gate on `ecb994a`: `cargo clippy --all-targets
--all-features -- -D warnings` clean; `cargo fmt --check` clean;
`cargo test --all-features -- --test-threads=1` — **complete from-zero
pass, exit 0, zero failures** (the kv `pending_free_at_cap` flake the
P1 note recorded did not fire this run); `cargo doc --no-deps` — 3
warnings, **byte-identical set reproduced on clean dev `1482131`**
(pre-existing `handoff_spawn`/`GhostTable` private-link warnings, not
this branch's); bench smoke `cargo bench --benches -- --test` 22/22.
Loom: not required (no ordering change — §1). KD-7: both A/B sides
measured as same-commit daemon+shim pairs; no wire/ABI change.

## 8. Residuals (recorded, not chased)

- **The 0.84–0.89×-of-ddt fraction** is service-thread CPU: the default
  prelude re-derives `file_path`/key strings the sync probe already
  built (≈ 5 small allocs/op) and takes a second `metadata_cache` get.
  A probe-state-reuse pass (thread the sync-probe's metadata handle +
  path into the prelude) is the named follow-on if the fleet wants the
  last ~10 %; the reaper (~94 %) becomes the binder shortly after
  (per-service-thread rings / second reaper stay the pre-agreed shapes,
  P1 §8).
- **`always`/`never` admission modes** keep the handler path for every
  governed miss by policy (`ipc_direct_ineligible_policy`) — diagnostic
  escapes, deliberately not direct-driven.
- **Ghost slot-collision first-touches** (~15 % of churn-row misses)
  direct-drive without a denial — harmless (same device-true serve),
  but they keep "denials ≈ serves" a ≈, not an ==; the §5.3 OQ-2 2-way
  tag upgrade would tighten it.
- **Buffered cold misses** stay on the handler (kernel page cache +
  admission machinery own them) — out of scope by directive.
- The base-vs-bracket session drift (~2–6 % declining over the ~40 min
  window, visible identically on untouched kernel rows) is a rig
  characteristic (timer-based null_blk on a desktop box); every verdict
  above is drawn against the same-window bracket where it matters.

## 9. Substrate teardown

As the P1 note §9: disconnect the two nvmet-loop subsystems, unlink the
port, rmdir nvmet objects, power-off + rmdir the `sqzlat_*` configfs
null_blk items. Left up while the branch is under review (RAM-backed,
reboot-ephemeral).
