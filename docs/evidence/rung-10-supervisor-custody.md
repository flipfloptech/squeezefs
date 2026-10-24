# Rung 10 — the supervisor's custody: a failed replay's tail is held; no write cache without a supervisor

**What changed (decision 0003 §BQ, the owner's rulings 4 and 6 of 2026-10-24).**
- *Ruling 4.* When the metadata device refuses the supervisor's replay of a journal tail, the
  tail is HELD, never discarded. Before the writer guards drop, the supervisor copies the tail's
  written slots into a sparse file of the same image at `/run/squeezefs/journal-tails/<volume uuid>.jtail`
  (RAM: it outlives the supervisor, SIGKILL included, and dies with the host). It retries every tick
  under a NON-BLOCKING writer lock, reading the device's superblock for the identity first, and
  removes the copy once the replay lands and is barriered. The volume's next writer
  (`KvBackend.open`, under its lock, after the superblock and the ledger, before the ring read)
  replays what is left, and refuses to open while the device refuses. Tail image v2 names the
  volume, the set and the ring kind, and gives every slot its reservation position. A foreign copy
  is refused. A copy whose windows are all behind the ledger's checkpoint tail is stale: it is
  removed unwritten, logged as an error and counted (`held_tail_stale`). A joined ring is not held
  (owed). If the copy cannot be written, the memfd and the guards stay with the supervisor.
- *Ruling 6 (c).* A mount without a supervisor runs no write cache. It says so once, sets the
  `write_cache_off_unsupervised` gauge to 1, and writes through the cache-less path.
  `SQUEEZEFS_WRITE_CACHE_PCT` > 0 named on such a mount is refused before the fork (exit 2). A
  cache-less mount whose member claim a supervisor still holds is refused (`StagingHeldForSuccessor`).
- *The latch agent's finding.* `squeezefs umount`'s process census took a custody-holding
  supervisor (childless once its daemon was reaped) for the daemon, and its TERM ended the hold.
  The supervisor now names itself `sqz-supervise`, and the census drops it: the verb answers
  `not mounted` and signals nobody. Custody leaves a supervisor only to a successor mount, or by
  the operator's `kill <pid>`, which the hold lines name.
- *Rows and rigs.* The kill matrix's foreground arm now judges the cache-less law: no staged read
  lost, no hole under a durable size. Its supervised arm remounts supervised. Every smoke row
  written for the write cache mounts `--daemon --supervise` (`Rig.spawnSupervised`, `mountAsSet`,
  `Peer.spawnAsSet`, and THE INTERCEPTION HOST's first row). The bracket probes now mount with
  `--supervise`. `perf_mount.sh`, `perf/reset.sh`, `dev_mount.sh` and the suites already did, so
  measured sessions keep the write cache.

**Red (origin/dev a0664fa).**
- The held-tail row (dm-error under the landing hold, kill -9): `replay FAILED (DeviceWriteFailed) — acked metadata of the flush window may be lost`, and the acked creates were `lost` after the device recovered.
- The unsupervised row:
  - `data_staging_on 1`.
  - After kill -9, 0 of 8 closed 3 MiB files were whole, each with 768 zero pages under its durable size.
  - Both unsupervised mounts with `SQUEEZEFS_WRITE_CACHE_PCT=25` came up.
- The custody row: the second umount exited 0, the supervisor was gone, the next mount recovered 0 ring records, 0 of 4 staged files were exact, and `staged_payload_lost_reads` was 4.

**Numbers (the branch).**

| run | result |
|---|---|
| held-tail row, both arms | the supervisor's retry: `replayed on retry`, then present; the successor: the copy held after the supervisor's SIGKILL, then `held_tail_replays` 1, present, the copy gone; fsck clean |
| unsupervised row | off, said once; 8 of 8 whole after kill -9; both knob mounts refused (exit 2); supervised with the knob: the cache on |
| custody row | the second umount: `not mounted`; the supervisor still holding; the next mount recovered 4 records; 4 of 4 exact; lost 0 |
| the three rows, two loops side by side under the venue's load (other agents' builds, suites and smokes) | 20/20 each (10/10 + 10/10) |
| full smoke tier (rebased on afd1649) | 138/142 pass, 4 skip (kmbuf absent, the loop seam unset, the shim's two other-step rows), 0 fail |
| unit filters `journal_tail` / `supervisor` / `staging` / `umount` | 47/47; 58 pass 11 skip; 70 pass 2 skip; 58 pass 44 skip |
| `zig build rails`, `zig fmt --check` | 18/18 steps, 32/32 tests; clean |
| xfstests `generic/753 475 388` | SUBSET |

**Seen, not fixed.**
- (a) Cache-less coherence. A joined peer reads the holder's durable size while the holder's
  parked write custody is un-landed: the O_TRUNC rows on foreground mounts read 0 for A's fresh 3 MB
  file on B. A foreign read recalls staged residents but not parked blocks. This was latent under
  `SQUEEZEFS_WRITE_CACHE_PCT=0` and is now the posture of every unsupervised multi-mount set.
- (b) `format` leaves a stale copy of the volume it overwrites in `/run` until reboot.
- (c) THE WRITE-BACK TIMER row failed in two full smoke runs with `fuse_syncfs 1`. A host-wide
  `sync(2)` from another runner on the venue landed its custody; the row passes alone.
