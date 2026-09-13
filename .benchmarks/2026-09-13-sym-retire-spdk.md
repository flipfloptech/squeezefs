# 2026-09-13 — SPDK retired as an NVMe-oF target: nvmet is THE target (PR 16, R-SYM-8)

**Program:** `docs/design-symmetric-metadata.md` (Rev 7) — PR 16 of 16, the one PR
independent of the symmetric plane (it lands before PR 13). **Ruling:** owner
R-SYM-8 (2026-09-12, §1.1; §5.8.1; KD-SYM-23; §9's rejected fencing-group
shapes). **Branch:** `feat/retire-spdk-target` from `dev` @ `89f34ed7`.
**Class:** forward-only retirement — a code-and-harness change with no
measured row of its own; the row it owes is named in §6.

## 1. Why

SPDK's nvmf target hardcodes `SPDK_NVMF_MAX_NUM_REGISTRANTS = 16`
(`include/spdk/nvmf.h`). At the symmetric program's operating point every
appender registers a PR key on every metadata namespace it appends to
(12,500 registrants per namespace), so that constant was the ONLY hard
registrant ceiling anything SqueezeFS shipped — the wall Rev 4–6's fencing
groups, host-id rotation, device-handle swap and re-key park arm existed to
fit the fleet under. The kernel `nvmet` target keeps registrants in an
unbounded list. The owner dropped SPDK as a supported target; with it the
cap and the fencing-group problem leave the product (the group machinery is
a documented follow-on for vendor-capped third-party arrays, §9; the
registrant-cap probe + loud refusal of KD-SYM-18 stay for PR 3).

This is a registrant-ceiling ruling, not a re-measurement. The 2026-07
dual-stack A/B (`2026-07-17-spdk-target-scoping.md`,
`2026-07-18-nvmeof-dual-stack-ab.md`) stands as history: SPDK's queued-I/O
wins (a dedicated busy-polling core as the entry price) against nvmet's QD1
latency and zero-install posture. Nothing in it is contradicted; the
deployment-class table that chose between them has one column now.

## 2. What was deleted (no-dead-code law)

| Path | Lines | What it was |
|---|---:|---|
| `src/nvmeof/spdk/lifecycle.rs` | 1,830 | pinned v26.05 tag+sha install, toolchain probe, hugepage preflight, pidfile start/stop/status, reactor core-mask math, `save_config`/`load_config` composition, the SPDK systemd unit |
| `src/nvmeof/spdk/mod.rs` | 1,198 | `SpdkStack` (share/unshare/restore/live walk over JSON-RPC), `SpdkPaths`, the `ptpl_file` pinning |
| `src/nvmeof/spdk/rpc.rs` | 377 | the JSON-RPC unix-socket client (timeouts, typed errors, version handshake + drift) |
| `src/nvmeof/spdk/hugepages.rs` | 301 | 2 MiB hugepage reservation with recorded prior / `--restore-prior` |
| `tests/nvmeof_spdk_stack_tests.rs` | 1,712 | the SPDK stack suite (in-process fake `spdk_tgt` on a real `UnixListener`) |
| `tests/nvmeof_rpc_tests.rs` | 651 | the RPC client suite (framing, timeouts, drift matrix) |
| **Total deleted outright** | **6,069** | |

Deleted inside surviving files: the cross-stack live-state duplicate guard
(`other_stack_live_state`, `cross_stack_duplicate_guard`, `manual_steps_for`
— there is no other stack to walk; the ledger half of the guard is unchanged
and is exactly what holds an SPDK record's backing), `AdoptProbe` and the
`adopt_ambiguous` class (one stack cannot be ambiguous), the SPDK arm of
`build_adopt_record` (the state-dir `ptpl` probe — `Ledger::state_dir()`
went with it), `TargetStartOptions`, `refuse_spdk_only_flags`,
`refuse_drift_flag_on_nvmet`, `stack_for`, `LiveShare.bdev_name`,
`ShareRequest.nsid`, `ShareOptions.accept_version_drift`, and the CLI flags
`--accept-version-drift` (share/unshare/restore/target start), `unshare
--force`, `target setup --hugemem-mb/--restore-prior`, `target start
--core-mask/--cores/--dpdk-mem-mb`, `target stop --force`, `target
systemd-unit --core-mask/--cores/--dpdk-mem-mb`. Harness: the stalling
JSON-RPC crash-window proxy, the SPDK round-trip / crash-window / adopt-A2
/ G2 / guard / A/B-arm legs, the substrate's spdk_tgt resolution, install,
hugepage record/restore and pidfile handling, `guard_smoke.sh --ptpl`.
Test allowlist hygiene: `DPDK`, `RPC`, `SIGKILL`, `SIGTERM` left the
help-hygiene caps allowlist (no help page uses them now); the derivation
sweep's `available_parallelism` reader census dropped
`src/nvmeof/spdk/lifecycle.rs`.

`src/nvmeof/mod.rs` went 1,855 → 1,363 lines; `tests/run_nvmeof_fidelity.sh`
1,917 → 1,175; `tests/nvmeof_target_substrate.sh` 621 → 498;
`tests/guard_smoke.sh` 373 → 311. Net over `src/` + `tests/`: **+1,642 /
−10,346 lines** (`git diff --stat dev`). No Cargo dependency was SPDK-only
(`serde_json` is used throughout); no `docker/` SPDK build existed to remove
(the design row's `docker/` item was already vacuous — the pin lived in
`lifecycle.rs`).

## 3. The refusal surface (forward-only; every one pinned in `tests/nvmeof_retire_spdk_tests.rs`, 18 tests)

| Surface | Behaviour |
|---|---|
| `"spdk".parse::<StackKind>()` | `Err` naming the retirement + nvmet + the re-share sequence. `StackKind::Spdk` survives ONLY as this parse-and-refuse arm and as the ledger's serde decoder — no execution path constructs a stack for it (`nvmet_stack()` is the one constructor). |
| `--target-stack spdk` (share / restore / adopt / target setup / start / stop / status / systemd-unit) | `resolve_stack` refuses at the grammar rung, BEFORE root, with `spdk_retired_refusal(...)`: names R-SYM-8, nvmet, `SPDK_RESHARE_SEQUENCE`. The clap variant is declared hidden (`#[value(hide = true)]`, the `--meta-slots` precedent) so the refusal is ours, not clap's `invalid value`. |
| `SQUEEZEFS_NVMEOF_TARGET_STACK=spdk` | The registry knob is `Kind::Enum(&["nvmet"])`, default `nvmet`: the ENG-10 startup gate refuses naming the knob, the value and `nvmet`; the in-process `resolve_stack` parse gives the full retirement text. |
| `SQUEEZEFS_SPDK_TGT_BIN`, `SQUEEZEFS_NVMEOF_RUN_DIR` | `Kind::Retired { successor: "(deleted — SPDK was retired …)" }` — refuse at startup naming the retirement and nvmet; listed in the operations.md law-6 retiree table (docs-parity pinned). The generic retired-knob message lost its "(ENG-10 knob-namespace collision)" parenthetical, which was already false for `SQUEEZEFS_FUSE_PLACED_MERGE`. |
| `nvmeof target install [--version] [--with-pkgdep]` | Retired verb under the `removed_verb()` convention: hidden from help, its flags kept declared (hidden) so `install --version v26.05 --with-pkgdep` still reaches OUR refusal ("was removed … Superseded by `target setup` / `target start`"), before root. |
| the deleted SPDK-only flags | die on clap's `unexpected argument` — loud, never a silent accept (pinned for all eleven spellings). |
| `nvmeof target stop` | refuses on the ONE target ("not a process" — tear shares down with `unshare`); `--target-stack spdk` there refuses with the retirement first. |
| an SPDK share in the ledger | `classification_of` → `SPDK_RETIRED_CLASSIFICATION` (never `managed`, never a restore candidate) — `nvmeof list` shows it with the sequence; `partition_restorable` routes it to `RestoreOutcome::Skipped(<sequence>)` and only nvmet records replay; `unshare` → `retire_spdk_share` = `mark_removing` → `delete` on the ledger ONLY (law-6 order kept; nothing SPDK is driven; the note names the manual `rpc.py` teardown); `adopt_candidate` on its NQN → `adopt_already_ledgered` naming the sequence; `begin_share` of the same backing on nvmet → `AlreadyExists` naming `unshare <spdk-nqn>` until the record is gone (the ledger's duplicate-backing guard IS the sequence's step-1 enforcement). `adopt` can only ever mint `StackKind::Nvmet` candidates. |

**The re-share sequence** (`nvmeof::SPDK_RESHARE_SEQUENCE`, spelled once
via a `macro_rules!` so the `list` classification `&'static str` composes
it too): (1) `sudo squeezefs nvmeof unshare <subnqn>` — ledger record
removed, backing released; (2) if an `spdk_tgt` still serves the old
subsystem, tear it down yourself (`rpc.py nvmf_delete_subsystem`,
`rpc.py bdev_aio_delete`) — SqueezeFS speaks no SPDK RPC anymore; (3)
`sudo squeezefs nvmeof share <backing> --ip <ip> --target-stack nvmet`
(nvmet is the default; re-use `--ns-uuid` to keep the namespace identity).

Interpretation note: the design row says "`nvmeof status` lists the ledger's
SPDK shares". There is no `nvmeof status` verb; the ledger listing verb is
`nvmeof list`, which is where the retired classification lands (JSON
`classification` field included). `nvmeof target status` reports the kernel
target's health, not ledger records — unchanged.

## 4. What stayed, deliberately

- **The register ladder and its spec-strict contracts** (`src/meta_backend/reservation.rs`, `tests/mount_writer_guard_tests.rs`): measured on SPDK v26.05 AND on current kernel nvmet (2026-07-17). They are the ladder's contracts, not SPDK's — a third-party spec-strict array behaves the same way. Their historical citations of SPDK are left verbatim ("do not rewrite the history").
- **The ledger schema v1** (`ShareRecord.{nsid,bdev_name,ptpl_file}`): `deny_unknown_fields` means removing the fields would make a ledger holding one SPDK record unreadable — the opposite of "listed, never re-presented". The ledger tests' full-optional SPDK record round-trip and the proptest over both `StackKind`s stay as the decodability pin.
- `HARNESS_NQN_MARKERS` keeps `"spdkscope"`: a pre-retirement scoping rig may have left objects; adopt must still refuse them.
- `--nsid` on `share`: hidden, kept declared so a value ≠ 1 refuses loud (the nvmet index is structurally 1) instead of dying on clap.
- `nvmeof connect/disconnect`, `nvmet.rs` (only `render_nvmet_unit` moved in from the deleted `lifecycle.rs`), every nvmet share verb, `docs/design-nvmeof-target-management.md` (a status banner; the body is history).

## 5. What the fidelity tier now covers (nvmet alone)

`tests/run_nvmeof_fidelity.sh quick`: substrate up (target setup/start + two
guard shares) → nvmet round-trip (file + block backings, deterministic slice
port id, `resv_enable=1`, `device_uuid == ns_uuid`, 64 MiB O_DIRECT
md5 both ways, list reconciliation, unshare to zero residue) → guard
kill-9 ×1 (`guard_smoke.sh --stack nvmet`) → teardown-to-zero-residue.
`full` adds: the loud-fail matrix (now carrying the retirement refusals:
`--target-stack spdk`, `SQUEEZEFS_NVMEOF_TARGET_STACK=spdk`, the retired
`SQUEEZEFS_SPDK_TGT_BIN`, `target install`, the four deleted `target start`
flags, `unshare --force`; the idempotent second `target start` — the kernel
target is not a process, so "already running" is not a state — and the
`target stop` refusal; `target status` shape), crash-window states
(EADDRNOTAVAIL-refused mid-verb mutations → pending finalized / pending
GC'd), adopt legs A1/A1b/A2 (pre-rebuild-style small-int port id,
harness-owned refusal, adopt-after-ledger-loss), the PR matrix on nvmet
(RESCAP Write-Exclusive bit, baseline `regctl=0`, register/acquire, holder
write, registration persisting across the initiator's disconnect, cross-host
fence, preempt, takeover write, drain to `regctl=0` — the generic checks the
SPDK arm used to carry, folded in; no PTPL claim), G2 (configfs wipe →
`restore` → same identity → the connected initiator reattaches), soft-RoCE
plumbing, the A/B smoke row, guard kill-9 ×10, zero-residue teardown.

Gone with SPDK: the PTPL power-cycle legs (there is no PTPL on nvmet by
design — `writer_guard_pr_reacquires` growth across a target power cycle is
the expected posture there, the heartbeat re-check law covers it), the
stalling-proxy kill-mid-verb crash windows (the nvmet crash-window leg's
kernel-refused mutations produce the same two law-6 states for real), the
dead-RPC / version-drift / broken-override refusals (no RPC, no version, no
override).

## 6. Owed

- **The root-run of the fidelity tier on nvmet alone** — `sudo tests/run_nvmeof_fidelity.sh quick` (and `full` for the nightly row) on a box with root + the zram/nvmet-tcp substrate. This session had neither; the three scripts are `bash -n` clean and the exec-bit test is green, but the design row's closing evidence ("the fidelity tier's quick + full run green on nvmet alone") is the orchestrator's to record here, with the nvmet-only durations replacing the dual-stack 2m20s / 5m46s in the AGENTS.md tier table.
- The in-process TOCTOU-drift end-to-end test for `adopt_over` was dropped with its only seam (the fake `spdk_tgt`'s `flip_uuid_after_gets` hook). The drift detector itself stays pinned by `test_adopt_verify_unchanged_detects_drift`; the abort-and-GC branch of `adopt_over` is a five-line path with no in-process seam on configfs. If one is wanted, a `NvmetStack` injection seam that re-reads a mutable snapshot would be the shape — not built here (scope).

## 7. Gate state at the commit boundary

`cargo fmt --check` clean; `cargo clippy --all-targets --all-features -- -D warnings`
and `cargo clippy --all-targets -- -D warnings` clean (no new `#[allow]`);
`RUSTDOCFLAGS="-D warnings" cargo doc --no-deps` clean. Suites run green:
`nvmeof_retire_spdk_tests` (18), `nvmeof_grammar_tests` (17),
`nvmeof_adopt_tests` (13), `nvmeof_target_lifecycle_tests` (2),
`nvmeof_nvmet_stack_tests` (17), `nvmeof_ledger_tests` (19),
`nvmeof_port_alloc_tests`, `nvmeof_initiator_tests`,
`nvmeof_fabric_stats_tests`, `env_knob_convention_tests`,
`cli_help_hygiene_tests`, `derivation_sweep_tests`, `cli_version_tests`,
`script_exec_bit_tests`, `docs_parity_tests`, and
`mount_writer_guard_tests` (43, serially — in parallel that suite's
process-global fault-injection seams cross-talk, unrelated to this PR; the
gate runs `--test-threads=1`). `task check` is the orchestrator's.
