# Rung 10 — a JoinAppender never adopts another incarnation's Live page (decision 0003 §DA) (2026-10-30)

**The claim (§CY.9 (a), read from the code).** A same-node successor joining before its predecessor's
eviction adopts the predecessor's still-Live page. `Manager.joinAppender` matched a Live page on
(node, mount slot); the reference matched the whole identity. The successor would then start at a
fresh head, and the predecessor's landed entries past its last checkpoint would never be replayed.

**As written: refuted.** The joined open's census read the appender directory off the device and
refused any Live page under its pair: `PredecessorLive`, the mount `Busy`, exit 1. The predecessor's
page is Live and barriered before its JoinAppender reply, so before it lands anything. Red
THE KILL ROWS (a) EARLY SUCCESSOR on `7ffdc39` (kill -9 B, remount at once, T_owner 10 s): "appender 1
(term 1) is LIVE on the directory under this mount point's identity", BUSY.

**Reachable all the same.** The census is check-then-act. The JoinAppender comes after the secret and
endpoint reads, `awaitManagerListener` (≤ 3 s) and §CY's Deferred loop (≤ T_park_max). Another
incarnation of the pair whose join lands inside that window goes unseen. That needs a second mount at
the same mount point whose open overlaps. The manager then answered `already` with that incarnation's
page. The adopter stands its region at the page's `head_hint` (its last cycle head), with no ring
replay and no slot entries, so its lane overwrites the landed suffix. Nothing recovers that suffix.
The pair is one membership member: the adopter's membership renews it. Once evicted, the recovery
finds the member live and retires the record, or, before the adopter's membership join, it recovers
the adopter's region under it (§CY). With the other incarnation alive, two processes write one ring.

**Reds** (`a1b4438`). Units: manager (`already`, appender 1, `head_hint` 1200); the real-backend dead
wire joiner, whose last 4 keys are in its ring past its last checkpoint (`already`, its appender 1,
`head_hint` 73296). Smoke THE KILL ROWS (a) OVERLAPPING SUCCESSOR: seam
`SQUEEZEFS_TEST_JOIN_APPENDER_HOLD` holds S, a second mount at B's mount point, between its census and
its JoinAppender. B mounts, writes `a-dir/own/f` (strict), kill -9 B, S released:
`lost: cat: a-dir/own/f: Input/output error`, and S's lookup stays parked for more than 40 s.

**Fix** (`7ffdc39`, `7546096`). The join replays only a Live page under the WHOLE identity. The writer
id is per process incarnation, so this is one incarnation's retried join. A Live page under the pair
with another writer id answers `Deferred`: nothing written, nothing minted, counted
`appender_join_live_incarnation_deferrals`. The joiner retries every 100 ms up to T_park_max
(T_OWNER_MS + T_owner). Once the eviction's recovery settles, §CY retires the record and the join
takes a fresh page. The joiner's page screen checks the whole identity too. The census refuses only a
predecessor proven ALIVE (`e39e692`): a process of this host whose pid and start ticks hash to the
page's writer id (`proc.liveWriterPid`) ⇒ `PredecessorLive`, Busy at once; otherwise the predecessor
crashed, and the remount waits for its recovery and joins instead of failing. The proof is re-run
every tenth deferral. The first cut (`7546096`) dropped the refusal whole, and a second mount at a
SERVED mount point then waited 90 s and failed (red `a2e014a`, the orchestrator's review).

**Numbers.** OVERLAPPING / EARLY SUCCESSOR: 140–144 deferrals, then a fresh page; B's file is whole;
`joined_rejoins` 0; A: recoveries 1, dead members acted 1. A second mount at a live joiner's mount
point: refused at once, 0 deferrals; one whose census preceded the served mount's join: refused at its
first deferral, exactly 1.

**Verified.** Decision 0003 §DA.7.
