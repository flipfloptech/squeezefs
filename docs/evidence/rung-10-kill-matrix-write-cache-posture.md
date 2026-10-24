# Rung 10 — THE KILL MATRIX's `staged_payload_lost_reads = 4`: the foreground posture, not a lost promotion

**The symptom.** Beside the full smoke (the UnknownLease and C3 agents' loads, trees before `2fbc3ec`)
THE KILL MATRIX failed its must-stay-0 gauge `staged_payload_lost_reads = 4`: four 1 MiB reads of
ino `0xF00_0000_0002` served zeros (`staged payload unreachable … serving 1048576 zero bytes`), the
storm census `8 of 8 files claiming 32 MiB: 7168 pages exact, 1024 holes`, 13 blocks. Reproduced on
`origin/dev` `7761bb0`: the row 4-wide beside the full smoke, **6 of 240 red**, every one the same
signature (a diag build logged each staged commit and promotion with its monotonic time).

**The mechanism (the trace).** The matrix mounts in the FOREGROUND (`rig.spawnMount`: no supervisor
holds the journal tail or the write cache — the mount says so at start, "a crash of this process
loses its unsynced residents"). The storm kill fires on the KERNEL's claimed sizes, which the FUSE
writeback cache runs far ahead of the daemon: at 100 MiB claimed the daemon has served only the
storm's first ~100 ms — each cp's 4 MiB staged image (the staged layout and its size, ONE
transaction), most of them promoted to their own block. In every red, the lost ino is the ONE cp
whose promotion never began: its 4 MiB staged commit acked and landed 4–23 ms before the kill (the
last-created file, ino 16 in five of six), no `promoteToStriped` for it. The device names a ring record
that died with the process; the file reads 4 MiB of zeros. No ring entry was released early — the
suspected promotion/release race is excluded (the lost ino's release never ran); the supervised
handover was never involved (the matrix has no supervisor). This is the charter's stated exposure
(§6.1 "a mount without a supervisor has a power cut's exposure for them"; §1 item 10, an acked
unsynced write whole or absent — here acked < 0.1 s before the kill, inside `T_wb` too), counted by
the very gauge the row asserted 0. The assertion was the file-backed ring's: before `5a0008b`
(2026-10-21) a foreground kill kept the ring's files and the custody; the memfd ring dies with a
foreground daemon. Unloaded, the kill lands after every promotion's window landed (gaps 3–40 ms), so
the row passed ~800 times in the logs and went red only under load.

**The law, judged per posture** (`killMatrixStorm`, `StormJudge`; test-only). The storm runs twice:
- *Foreground*: a file whose reads move `staged_payload_lost_reads` (read off `.stats` around each
  census) is a staged image the kill discarded and must be ABSENT WHOLE — at most one block, no page
  of it the image's; every other page exact or a hole.
- *Supervised* (`--daemon --supervise`, killed, the supervisor's replay awaited, fsck, remount): the
  ring is handed to the remount; no staged read may be lost (the counter 0, every resident exact).
A 3 MiB resident (closed, never synced: nothing promotes it) is written before each storm, so every
run meets both laws rather than only a late cp. 3 MiB, not 1: a stray SYNCFS (a host-wide `sync(2)`
reaches every FUSE mount — the load's rows issue them) promotes a ≤ 2 MiB image into a packed slot
whose open pack keeps its block until the dismount seal: with 1 MiB the df baseline failed 11/100
under load (whole rounds) and 8/8 under a targeted `sync -f` loop; 3 MiB takes its own block, 12/12.

**Red.** `origin/dev`'s row with the resident added and censused, unloaded: red 1/1,
`must-stay-0 gauge "staged_payload_lost_reads": = 1`, the resident 1 MiB, 0 pages exact.

**Numbers (after).** The row 4-wide beside 21 full smokes: **100/100**. Foreground: the resident
absent whole 100/100 (3 lost reads each); one run also had a cp absent whole (the original failure's
shape, now judged lawful: 7 lost reads). Supervised: the resident exact 100/100, lost 0, the remount
recovering 1 ring record (94) or 2 (6 — a late cp's image recovered and read exact: the strong law
in the original shape). Unit `staging` 69/70, `promote` 50/60, `plane` 178/182 (the rest skip);
rails 32/32; the write-cache rows (12 filters, 56 tests) green; `zig fmt --check` clean.

**Open (owner).** A foreground mount's staged size is durable before its bytes (§AL.5 holds for the
striped extension only): after its crash a staged file reads zeros under its size rather than
ending at its last durable length. Options: keep the stated posture (this note); or withhold a
staged size from the journal until promotion; or no write cache without a supervisor.
