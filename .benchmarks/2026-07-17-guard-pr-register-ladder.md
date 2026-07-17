# Guard PR Register Ladder — kill-9 Remount Recovery on Spec-Strict Targets

Date: 2026-07-17 · Branch `fix/guard-pr-register-ladder` off dev `fd3eed8` · Box: AMD RYZEN AI MAX+ PRO 395 (32 threads), kernel 7.1.3-2-cachyos · Rig: the committed `.agents/spdk-scoping/` scripts (SPDK v26.05 `d519b16` local build at `/var/tmp/spdk-scoping`, zram-backed NVMe/TCP; **new** kernel-nvmet guard arm added to `rig-up.sh` — 2 `resv_enable=1` namespaces, same shape as the SPDK guard subsystem).

Closes the SPDK scoping pass's **P0** (`.agents/spdk-scoping/scoping-report.md` §4 / §7 Q1 / §8 S1): the single-writer guard's PR layer assumed lenient Register semantics — after kill -9, the dead incarnation's stale registration (same host identity, old key) persists, and a **spec-strict target refuses the fresh Register with Reservation Conflict**, bricking remount (fail-closed; no data risk; a remount blocker).

**Instrument statement:** `guard-smoke.sh` (real `squeezefs format/mount --daemon/claim clear` CLI + nvme-cli probes) against rig-served namespaces; unit tier is `tests/mount_writer_guard_tests.rs` against the in-memory `FakeReservationClient`.

---

## 1. RED (measured before the fix)

### 1.1 Unit (RED commit `fa0a6fd`, naive ladder = today's plain register)

```
failures:
    test_kill9_remount_recovers_on_spec_strict_target
    test_register_ladder_own_stale_with_foreign_holder_leaves_holder_alone
    test_register_ladder_recovers_own_stale_registration_after_kill9
test result: FAILED. 35 passed; 3 failed
```

Mount-level failure text (identical error class to the live repro):

```
remount after kill -9 must succeed on a spec-strict target (the SPDK P0 finding: …):
Busy("/tmp/.tmpdHbHoC: reservation register failed on the PR-capable namespace:
Invalid exchange (os error 52) (single-writer guard)")
```

### 1.2 Live, dev binary `fd3eed8` (md5 `924162c9…`), SPDK arm (`results/guard-smoke-spdk-RED.txt`)

```
$ …squeezefs --log-file … mount sqmeta:///dev/nvme4n1 /tmp/spdkscope/mnt --daemon --allow-other
Failed to start squeezefs daemon:
Error: Invalid operation: kv metadata: /dev/nvme4n1: reservation register failed on the
PR-capable namespace: Invalid exchange (os error 52) (single-writer guard)
[guard-spdk] FAIL: cycle 1: REMOUNT did not appear after kill -9
```

Post-mortem namespace state — the dead incarnation's residue, exactly the scoping session's shape:
`{"regctl":1,"rtype":1,"ptpls":1,"regctlext":[{"rkey":0x4d4d…,"hostid":"20ba8dd9d1d24001b7825c1db3a414d3","rcsts":1}]}` — and the log shows the dead-pid claim reclaim SUCCEEDED first (`reclaiming writer_claim from dead same-host holder … ESRCH`); only the PR rung blocked.

### 1.3 Live, dev binary, **kernel-nvmet arm** — a MATERIAL CORRECTION (`results/guard-smoke-nvmet-RED.txt`)

```
Error: Invalid operation: kv metadata: /dev/nvme5n1: reservation register failed on the
PR-capable namespace: Invalid exchange (os error 52) (single-writer guard)
[guard-nvmet] FAIL: cycle 1: REMOUNT did not appear after kill -9
```

**Kernel nvmet on 7.1.3 (`nvmet` pr.c, `resv_enable=1`) is ALSO spec-strict** on a different-key re-register. The M1-era "nvmet: IEKEY behaves replace-ish / re-register idempotent" note only ever covered **same-key** idempotency (the M1 root session never re-registered a *different* key from the same host). The P0's blast radius was therefore **both stacks**, not SPDK-only: kill-9 → remount was broken on every PR-enforced (`flock+pr`) fabric volume on this kernel. The ladder fixes both; the smoke script now **probes** each target's Register semantics (`nvme resv-register` different-key shape) instead of assuming by arm — both arms measured `strict`.

---

## 2. The ladder (fix commit `cfe0ff1`)

`register_ladder(client, key)` — `src/meta_backend/reservation.rs` (~line 170), driven by the mount gate (`src/meta_backend/kv/backend.rs` `writer_guard_gate`, ~line 1944) and the heartbeat PTPL-lapse re-register (~line 2210), both via one `rsv_call` closure:

1. **Plain `register(key)`** — success = `RegisterOutcome::Registered`, today's fast path byte-identical (a genuinely lenient target never leaves it).
2. On the **reservation-conflict class only**: read the association's **wire host identifier** (new trait verb `wire_host_id()` — Get Features FID 0x81, EXHID=1 16 B, EXHID=0 8 B fallback; all-zero ⇒ unknown). *Deliberately not* `/etc/nvme/hostid`: the scoping session measured the association identity (`20ba8dd9…`) diverging from the config file (`4056db03…`) — only the device's answer is authoritative for matching. Unreadable/empty ⇒ **fail closed** with the original conflict.
3. **Reservation Report** → registrants whose `host_id` == ours AND `rkey != key` = **our own stale registration(s)** (the kill-9'd incarnation). None ⇒ **fail closed** with the original conflict + loud diagnostics (foreign registrations are *not ours to remove* — arbitration stays the acquire-conflict / claim / preempt path, not widened).
4. **`unregister(stale)`** (new trait verb, RREGA=1 `crkey`=stale — the device itself validates crkey against *this host's* registration, so foreign removal is impossible at the device level too). Unregistering the stale holder key **drops the WE reservation with it**.
5. **`register(key)`** fresh ⇒ `RecoveredOwnStale { unregistered }`, logged loud with the recovered keys; acquire proceeds as before.

Supporting surface: `ReservationReport` now carries per-registrant host identities (`ReservationRegistrant { rkey, host_id, holds_reservation }`; EDS hostid[16] @16 / short-form u64 @8 per the M1 byte-wise layout); `release()` rides `unregister()`; the fake models per-host registrations with `RegisterSemantics::{SpecStrict (default), LenientReplace}` + an `unregister_count()` law hook.

**Laws preserved (each pinned by a test):** foreign registrations never touched (fail-closed + device crkey check); empty own-identity never matches; non-conflict register errors propagate verbatim; lenient targets take the fast path with 0 unregisters; fencing/EBADE classification, claim TTL law, `claim clear` semantics, double-mount refusal all unchanged.

---

## 3. GREEN — both-stack live matrix (fix binary md5 `0bdf8d25…`)

Counts restarted from zero after every harness fix (multi-run discipline): the first SPDK GREEN run's step-6 "failure" was a **harness bug** (asserting `claim clear` succeeds while 5 crash-orphaned `client:*` records were still TTL-fresh — the refusal is the designed staleness law); the harness now asserts the refusal *and* the post-TTL no-op, and the count below is the clean rerun.

| Leg | SPDK v26.05 TCP (strict, PTPL) | kernel nvmet TCP (strict, no PTPL) |
|---|---|---|
| Register-semantics probe (nvme-cli, guard shape) | **strict** | **strict** (M1-era "lenient" corrected) |
| format + mount → `writer_guard_mode` | `flock+pr` ✅ | `flock+pr` ✅ |
| double-mount refusal while serving | refused naming the guard ✅ | refused naming the guard ✅ |
| `claim clear` while live-mounted | refused ✅ | refused ✅ |
| **kill -9 → remount ×10 (restart-from-zero count)** | **10/10 recovered**, `flock+pr`, data md5 intact, ladder log hits = 10 ✅ | **10/10 recovered**, `flock+pr`, data md5 intact, ladder log hits = 10 ✅ |
| PTPL: `save_config` → SIGKILL `spdk_tgt` → relaunch → `load_config` while holder ALIVE | holder keeps serving: `writer_guard_fenced=0`, `pr_reacquires=0`, data intact ✅; then kill-9 → remount recovers against the ptpl-restored state ✅ | n/a (`ptpls:0`; PTPL-lapse heals via the heartbeat re-check — M1 machinery, unit-pinned) |
| `claim clear` under TTL-fresh crash orphans / after TTL | refused (staleness law) / no-op ✅ | refused / no-op ✅ |
| clean unmount PR residue | `regctl=0` ✅ | `regctl=0` ✅ |
| verdict (`guard-smoke.sh` exit) | **0 failures** | **0 failures** |

Ladder log shape (daemon log, per remount):

```
WARN squeezefs::meta_backend::kv::backend] meta volume /dev/nvme4n1: reservation register
conflicted with our own stale registration(s) [ 0x5b35357a9d8107cd, ] — a crashed
incarnation's residue on a spec-strict target (SPDK-class Register semantics); unregistered
them and registered fresh (single-writer guard register ladder)
```

Transcripts: `.agents/spdk-scoping/results/guard-smoke-{spdk,nvmet}-{RED,GREEN}.txt` + `…-daemon.log`.

## 4. Gate

Per-commit cargo gate on the fix tip: `clippy --all-targets --all-features -D warnings` clean · `fmt --check` clean · `cargo test --all-features -- --test-threads=1` **98 suites, 0 failures** (mount_writer_guard 38/38 — the 3 RED flipped; mount_registration, format_guard, crash_kill green) · `cargo doc --no-deps` 0 warnings · `cargo bench --benches -- --test` clean. No lock-free core touched — no loom delta (ladder is one-shot ioctls under the existing `spawn_blocking` shape).

## 5. Cleanup proof

Rig teardown to **zero residue** (`snapshot-{before,after}-ladder.txt`, `/tmp/spdkscope/`): hugepages-2M restored to prior **0**; zram1–7 hot_removed (zram0 user swap untouched); all 5 spdkscope fabric controllers disconnected (only user `nvme0` remains); nvmet configfs subsystems/ports removed; `nvmet_tcp`/`nvme_tcp` modules unloaded; no 4460/4461 listeners; spdk_tgt killed by recorded pid. `/var/tmp/spdk-scoping/` build tree retained (sanctioned). RED build worktree `/tmp/sqz-red` removed.

## 6. Docs deltas

- Scoping report §4 smoke table + §7 Q1: P0 row annotated **FIXED** (this note; nvmet-also-strict correction recorded).
- README guarantee table: SPDK-served row upgraded from "probe decides … validated in OQ 4's scope" to measured enforcement-grade + ladder + PTPL; enforcement row gains the crash-remount ladder clause.
- `rig-up.sh` (nvmet guard arm) + `guard-smoke.sh` (productized both-stack assertions, semantics probe, kill-9 loops, PTPL leg) committed for reuse as the S3 fidelity-tier seed.
