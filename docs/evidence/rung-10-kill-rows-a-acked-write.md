# Rung 10 — THE KILL ROWS (a): an acked file empty after `kill -9 B` (§1, decision 0003 §CX); the successor's `Corrupt` unmount, a live region recovered under it (§2, §CY) (2026-10-30)

## 1. The acked-but-empty file

**Symptom.** 3 of 29 runs on `fix/orphan-supervisor` (load ≈ 100), 0 of 35 on dev: a file tar was acked on read back
EMPTY on A. Each was the FRONTIER, the last entry tar was acked on: its archive position equals 1000 − tar's unacked
count (#202/798, #211/789, #212/788).

**The ack is real.** tar's stderr is the ledger (no `Cannot open/write` ⇒ open and `write(2)` returned). With no
writeback cache and no passthrough, a returned `write(2)` means B's daemon answered `FUSE_WRITE`.

**Cause.** B is a foreground joiner. No supervisor holds its journal tail, so nothing replays it at a kill. Under the
coalesced cadence B answers its kernel's commits at the apply (§BB.1), and each lands when the lane issues the window.
tar's files are B-local commits: only `tree` is cross-owner, and every `t*` mints in `tree`'s slot. Under CPU load the
lane lags its answers, so a kill inside the lag loses the acked windows not yet issued. Charter §5.2 allows this
("a mount with no supervisor … the power-cut window applies"), and so does B's log ("a crash of this process has the
flush-window exposure"). §BB.7 re-judged the kill rows for the new ack point and missed this one (written 2026-10-09).

**Reproduction on dev `90094f4`** (instrumented copies of the row: device image copied at the kill, artefacts kept):
idle 30/30 and loaded 34/34 (tar under strace), loaded 38/39 (the one failure is §2). With B's `sqz-jrnl*` threads
reniced to 19 under `nproc` busy loops plus the smoke's 16 shards: coalesced **2/25 fail**, strict 0/25. In both
failures `t188` is the frontier. tar printed `Cannot utime: Software caused connection abort` and `Cannot change
ownership/mode: ENOTCONN` for it (its open and write returned). Its body occurs **0 times** in the device image at the
kill and at the verdict, while **199/199** whole acked bodies are there. One run lost the create too (`ACKED, absent`).
B's mid-tar `.stats`: `acks_at_apply` 524 / `acks_at_landing` 2, cross-owner intents 1. A: recoveries 1, rollbacks 0.
**The acked bytes never reached the device**, so this is not a recovery defect.

**Red** (`a5f77a5`): seam `SQUEEZEFS_TEST_LANDING_HOLD_AT_WRITE=N`. From the mount's N-th FUSE write on, the lane
issues nothing. Row "THE KILL ROWS (a) HELD WRITE": B writes t0–t2, creates t3 and syncs the filesystem, and its 4th write is held;
`kill -9 B`. On dev, B answers at the apply and the row reports `t3: body '' (ACKED by tar)` (4/4 at load ≈ 100).
**Fix** (`0ba6dda`): B runs the strict cadence in both rows (`kill_row_b_env`), the mid-tar row's strict-twin posture.
Every ack follows its landing, and a recovery that dropped a landed entry stays visible. Rejected: a prefix oracle
under coalesced (it would pass such a recovery); a supervised B (its death replay would race A's recovery, ordered by
time only).

## 2. The successor's `Corrupt` unmount (decision 0003 §CY)

**Symptom.** B's same-node successor sometimes ended `kv checkpoint tick failed … Corrupt` → `UNCLEAN unmount`, exit 1
(dev 1/39; B strict 2/14). **Trace** (diagnostic build, not committed; 5 captures): during the successor's leave, a
release publishes a slot root, and the manager rejects it (`tree 0 says unleased appender 0 g 1`, while the successor
holds the slot leased at g 1) → `Corrupt`. A's first tick after the successor's join: `1 death record(s) standing,
1 acted, … 1 retired` — **A recovered the live successor's region**.

**Cause.** The dead member's record is keyed by (node, mount slot). It still stood when the successor's JoinAppender
put its page Live. The successor's membership join comes only at the wire plane's start, so a poll in between found
the fresh Live page, saw no census member, and recovered it. The reference retires the joining pair's record at the
JoinAppender service edge (`retire_joiners_death_record`, whose comment names this failure); the porting spec and
`retireForJoin`'s doc state the same law. The Zig manager never called it.

**Reds.** Smoke "THE KILL ROWS (a) SUCCESSOR'S JOIN" (seam `SQUEEZEFS_TEST_MEMBERSHIP_JOIN_HOLD` holds the successor's
membership join across two ledger polls): `appender_recoveries` 2 on `0ba6dda`. The first fix retired the record
unconditionally at the join. Two `volume_bind` tests (real backend, dead wire joiner) showed what that breaks: joined
before any poll acted, the predecessor's page stays Live forever (`expected .recovered, found .live`, an orphan);
joined mid-recovery (`SQUEEZEFS_TEST_RECOVERY_HOLD`), the recovery writes `recovered:` under no record.

**Fix** (`df3ec9c`, `d0ab4a0`). `Ledger.retireJoiner` runs before `Manager.joinAppender`. It retires only once the
predecessor's recovery is settled on every volume: the hook (`Driver.recoverySettled`) finds no page, or a Recovered
page with `recovered:` written; a volume with no hook needs its row. Otherwise it answers `Deferred`, writing nothing,
and the joiner retries up to `T_park_max`.

**Verified.** Decision 0003 §CX.7 and §CY.8.
