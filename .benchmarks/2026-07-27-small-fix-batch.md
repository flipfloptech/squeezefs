# 2026-07-27 — Small-fix batch: two suite flakes root-caused + the TCP substrate standardized

Branch `fix/small-batch-flakes-substrate` off dev `e8bab83`. Three items,
sequential; every fix red-first. Box: 32-CPU / 109 GiB, kernel
7.1.4-1-cachyos. Left unmerged for review.

## 1. `mount_writer_guard_tests::test_torn_claim_entry_recovers_and_reclaims` (P3 §7 flake)

**Recorded shape** (P3 `.benchmarks/2026-07-27-write-side-economy.md` §7):
one full-suite failure — `Busy("… another squeezefs process holds the
writer lock …")` on the remount-after-torn-claim leg — with 5/5 isolated
and 3/3 whole-binary green on branch AND clean dev.

**Root cause (product, proven):** the torn open's claim COMMIT already
spawned the conveyor pass task. After the failure fan-out wakes the
committer, the pass loops and its next-iteration `weak.upgrade()` races
the failed `open`'s own `Arc` drop; when the pass wins AND its worker
thread loses its OS quantum inside the empty-drain tail (between the
upgrade and the `drop(be)` — a straight-line window with no awaits), it
pins the backend struct and its **Layer A writer flock** past `open`'s
error return. The torn claim entry never replays, so the volume carries
NO readable claim — the same-process teardown absorption
(`await_same_process_teardown_flock`) cannot attribute the holder and
refuses instantly. An instant remount then loses to our own dying
teardown. The window is an OS-preemption event, which is why it fired
~once per full-suite run (loaded box) and never isolated.

**Counted attribution runs (declared rate/signature gathering, dev-tip
binary):** bare ×100 under 64-hog CPU load: 0 failures; ×100 pinned to
one CPU against 3 hogs: 0 failures — consistent with a nanosecond-scale
preemption window, NOT reproducible by load alone. **Deschedule
injection** (working-tree probe: 200 ms sleep between the pass tail's
upgrade and drop): failure **1/1, exact recorded signature** — mechanism
proven, probe stripped.

**Red (4b1afe1):** new seam `TEST_CONVEYOR_HOLD_EMPTY_DRAIN_TAIL` parks
the pass's empty-drain tail WHILE holding its backend upgrade (the one
deliberate exception to the drop-before-park rule — it models the OS
quantum) + `TEST_CONVEYOR_EMPTY_TAIL_PARKED` barrier gauge; test
`test_failed_claim_commit_releases_flock_despite_pinned_pass_tail`
**RED 3/3 deterministic** with the recorded signature verbatim.

**Fix (5aedead):** `KvMetaBackend::open`'s gate-failure path takes the
guard fd and closes it **before** returning `Err` — the drop-order
release made synchronous. Safe: nothing of the failed mount writes after
the gate refusal (the batch failure's rollback + hole checkpoint ran
inside the pass before its fan-out woke the committer); no
checkpoint/times-drain task exists yet. The same-process absorption
posture (foreign holders never wait) is unchanged.

**Exit bar:** red test deterministic-green; whole writer-guard binary
**10/10 green** (the deterministic failing context — the seam pin — runs
inside every pass); original torn test **100/100 under 64-hog load** on
the fixed binary. Counts restarted post-fix per the multi-run discipline.

## 2. `crash_kill_tests::test_kill9_remount_soak_v3_batched` (P2 §10's 1/8 flake)

**Standalone reproduction (counted, dev-tip binary):** 20 runs × 10
rounds → **1/20 runs failed**, signature verbatim: `round 9: acked create
'l2-f18' lost after a mid-batch kill-9 … Dentry l2-f18 not found`.

**Root cause (test harness, NOT crash recovery):** `ledger_append` used
`writeln!`, which issues the payload and the `'\n'` as **two `write(2)`
calls**. The batched child runs 3 concurrent lanes appending through
separate `O_APPEND` fds, so lane payloads interleave before their
newlines. Post-mortem of leaked ledgers showed torn lines in **green**
rounds too (1–6 per ~700-line ledger):

```
start create l2-f0start create l0-f0start create l1-f0
ack unlink l2-f36 112ack destroy 111
```

A fused `start unlink <name>` / `ack unlink …` line fails
`parse_ledger`'s exact-token slice match, the supersession marker
silently vanishes, and the model demands a file the child had REALLY
unlinked → "acked create lost". Confirmations: every observed failing
name had `i ≡ 0 (mod 3)` (exactly the child's unlink cadence), and the
single-lane serial soak cannot interleave — matching the recorded
batched-only incidence. Wedge-level seriousness applied and discharged:
the product's D0 acked-durability contract was never violated; the
harness lied about what was acked-and-unsuperseded.

**Red (5052c7c):** `test_ledger_append_line_atomicity_under_concurrent_
lanes` — 3 threads × 1,000 records through `ledger_append` into one
ledger; every line must be exactly one well-formed record. **RED 3/3
deterministic** on the two-write `writeln!`.

**Fix (4665606):** one `write_all("{line}\n")` per record — a single
`write(2)` on an `O_APPEND` fd is atomic between appenders. Product code
untouched.

**Exit bar:** **20/20 from zero at the failing cadence** (default
10-round runs, counted post-fix) + atomicity test 5/5 + whole crash
binary green.

## 3. TCP-substrate standardization (harness + docs)

**Harness:** `tests/dev_substrate.sh` grew `SQZ_DEVSUB_TRANSPORT=tcp` —
same shape (memory-backed null_blk mds + zram oss), same
create/teardown/status/recreate verbs, exported via **nvmet-tcp on
127.0.0.1** instead of loop. Ownership is transport-scoped and disjoint
(NQNs `devsubtcp-*`, null_blk items `sqzdevsubtcp_*`, state dir
`/run/squeezefs-devsub-tcp`, configfs port id 52027), so loop + tcp
substrates coexist and neither mode's stale-state sweep can claim the
other's objects. TCP service-port slice **54100–54199** (default 54129)
— deliberately outside the NVMe-oF fidelity tier's 54000–54099.
Shellcheck-clean.

**Docs:** AGENTS.md → Benchmarks & Profiling now carries the
**two-substrate rule** (loop = controlled-latency A/B; tcp = MANDATORY
for fabric-sensitive rows: writes, bandwidth-bound shapes,
multi-connection — the loop rig hides bandwidth-economy and
network-stack effects, proven by the amplification campaign) and the
**write-amplification instrument** as a standing row requirement:
device bytes ÷ user bytes on the data namespace, `wareq-sz` vs block
size, and the `block_free_*` reclaim counters
(`.benchmarks/2026-07-27-shim-write-amplification.md`).

**End-to-end verification (this box):**

- `sudo SQZ_DEVSUB_TRANSPORT=tcp SQZ_DEVSUB_MDS_COUNT=1 SQZ_DEVSUB_MDS_GB=4
  SQZ_DEVSUB_OSS_COUNT=1 SQZ_DEVSUB_OSS_GB=32 tests/dev_substrate.sh create`
  → `/dev/nvme5n1` (mds, null_blk) + `/dev/nvme6n1` (oss, zram), both
  controllers `transport=tcp`, address `traddr=127.0.0.1,trsvcid=54129`.
  The sibling campaign's foreign `sqztcp-*` objects and the loop-rig
  `sqzlat-*` objects on the box were untouched throughout (ownership
  policy held).
- One write row + one read row through the amplification instrument
  (`tests/write_amp_rig.sh`, elbencho dynamic 3.0.25, 16×512 MiB
  `--direct`, 1 rep, same-commit daemon+shim pair rebuilt at the branch
  tip), **exit 0, engagement exact on every row**:
  - `seq_write_1m` — kernel: 362 MiB/s, **AMP=0.994**, wareq
    **3.98 MiB** (4 MiB blocks), `write_through_blocks`=2032; shim:
    364 MiB/s, **AMP=1.001**, wareq 3.98 MiB, **8192 ring
    `ipc_ops_write`** (charter rule 4), `block_free_discards`=2063 /
    `block_free_discard_bytes`≈8.6 GB live on the fresh-file delete leg
    (BLKDISCARD reclaim — the campaign's landed counters); shim-frag
    (`SQUEEZEFS_IL_MAX_RUN_SLOTS=1`): 344 MiB/s, **AMP=0.992**, 131072
    ring ops, `active_block_ooo_runs`=71881 with
    `write_through_blocks`=2032 — exactly one write-through per block
    under 64 KiB out-of-order arrival (the campaign's green posture,
    reproduced on the standardized substrate).
  - `seq_read_1m` (cold, per-rep remount) — kernel: 2863 MiB/s,
    **AMP=1.092**, rareq 4.00 MiB; shim: 3114 MiB/s, **AMP=1.016**,
    **131072 ring `ipc_ops_read`**.
  - An earlier attempt with the stale on-disk pair failed the rig's
    engagement check exactly as designed (commit-mismatched KD-7
    identity ⇒ no ring binds ⇒ INVALID rows, nonzero exit); the pair
    was rebuilt and the run repeated — counted per the multi-run
    discipline, the valid run is the evidence.
- `sudo SQZ_DEVSUB_TRANSPORT=tcp tests/dev_substrate.sh teardown` →
  **zero residue**: nvmet subsystems/ports, null_blk configfs item, zram
  device, and host controllers all gone; post-teardown snapshot of
  `/sys/kernel/config/nvmet/{subsystems,ports}`, `/sys/kernel/config/nullb`
  and `/sys/block/zram*` byte-identical to the pre-create snapshot.
- **Coexistence pass (final script):** loop + tcp substrates created
  side by side (loop → `/dev/nvme5n1`+`/dev/nvme6n1`, tcp →
  `/dev/nvme7n1`+`/dev/nvme8n1`), each mode's `status` sees only its
  own, both teardowns → snapshot again byte-identical to pre-create.
  The box's foreign rigs (`sqzlat-*` loop port 52126, the sibling's
  ad-hoc `sqztcp-*` port 54200) were untouched throughout.

**Found + fixed while verifying (pre-existing, proven on unmodified
dev):** loop-mode `create` failed on any box carrying a FOREIGN nvmet
loop port — a bare `nvme connect -t loop` binds the box's FIRST
registered loop port (here the fuse-per-op campaign's `sqzlat` port
52126), so devsub subsystems were rejected with "connect request for
invalid subsystem" (dmesg). Fix: devsub's loop port stamps a free-form
`addr_traddr` (`sqzdevsub`, set before any subsystem link — the attr is
write-locked after) and connects with `-a sqzdevsub`; a legacy
traddr-less devsub port is recreated when link-free and refused loud
otherwise.

## Gates (branch tip)

- `cargo clippy --all-targets --all-features -- -D warnings`: clean.
- `cargo fmt --check`: clean.
- `cargo test --all-features --no-fail-fast -- --test-threads=1`:
  from-zero run — **rc 0, 140 green result lines, 0 failures** (this
  batch's purpose: the suite reliably green; both fixed flakes' tests
  ran inside it).
- `cargo doc --no-deps`: 3 warnings + summary — byte-identical to the
  P2/P3 recorded pre-existing intra-doc-link set (`handoff_spawn` ×2 +
  `GhostTable`); zero delta.
- `cargo bench --benches -- --test`: green (bench smoke, rc 0).
- loom (`tests/run_loom.sh`): **44/44 green** (belt-and-suspenders — the
  `ConveyorCore` lead/unlead protocol is unchanged; the pass-task change
  is a test-only awaited park, same shape as the existing hold stages).
- Harness class: `shellcheck tests/dev_substrate.sh` clean; tcp-mode
  e2e above (create → format/mount → write+read amplification rows,
  engagement exact, rig rc 0 → teardown-to-zero-residue), loop-mode
  regression + coexistence pass.
- Docs class: markdown anchors verified (AGENTS.md internal reference
  *Benchmark substrates* ↔ Testing section pointer; the referenced
  benchmark notes exist).
