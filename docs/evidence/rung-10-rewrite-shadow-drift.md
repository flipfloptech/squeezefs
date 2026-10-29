# Rung 10 — the perf matrix's drift: a writer read the block's binding before the epoch probe (2026-10-29; decision 0003 §CN)

**The symptom.** The perf matrix on `9811f84` fired `rewrite_shadow_displaced_drift` once in each psync
session (`ram-sqz` 12:05:10, `ram-sqz-il` 12:20:46). Every row's `.stats` holds the count 1 from its first
snapshot, and the line precedes the lay-down's fio JSON by 56 ms: the LAY-DOWN fired it — fio
`fallocate(fd, 0, 0, 384 MiB)` per file (every block reserved), then 32 files × 1 MiB O_DIRECT QD16, 1.26 s.
The displaced key was WRITTEN (`unwritten false`). Both run records' offline fsck: clean.

**The reproduction.** That lay-down on sparse images (2 × 24 GiB OST on the venue's zram, `--supervise`), fio
crc32c `verify_only` after every pass, a temporary event ring dumped at the tripwire. Dev `0778bb4` fired at
pass 20: the tripwire AND `verify: bad magic header 0` at mx.17 block 83, 1 MiB — acked bytes read back as zeros.

**The cause** (the ring, block 83). fio's fallocate reserved R; a Hole overlay record (dest D1) took the first
MiB; the write-back timer's drain settled it partial (gap seeded zeros) and fed the epoch (R parked, D1
shadowed); the save landed {83 → D1} and left sight; 1.1 ms later the next piece installed a record with
`lookup` = D1 — as a HOLE. The arm had read `overwrite = map.lookup(83) != null` BEFORE `claim`'s epoch screen:
the read saw R (a hole to `lookup`), the save landed and left sight, the screen saw no epoch. The Hole record
seeded its uncovered first MiB with zeros, its feed parked nothing, the next save displaced D1 unparked (the
tripwire) and freed it. A save runs under no block guard, so a map read before the epoch probe can predate a
save landing in between. Three more readers had that order (§CN.2): `installRecord`'s overwrite capture,
`pickDest`'s take-over (a reservation the save freed, minted by another owner since, is SOLE to
`beginPatchSoleOwner`: the landing writes over that owner's block), and the upload's record base (the freed
reservation parked and freed again).

**Red** (`a45d052`: two write_admit rows on the RamMap's new after-read seam, which lands the save right after a
writer's binding read). The overlay's row: the first half reads zeros, the tripwire "0 parked key(s)". The
upload's row: the landing takes over the freed reservation another owner minted, its `0xCC` overwritten.

**The fix** (`95aa8fa`). Probe first (`write_epoch.Base`): a probe that sees no epoch makes the map read after it
exact while the block guard holds — no epoch starts shadowing a block without that guard, and a save names B
before it leaves sight. `claim` classifies after its screen (`StoreRequest.overwrite` removed), `installRecord`
re-checks before it captures, `pickDest` probes before it reads the reservation, a record's base is
`Epochs.baseFor`, and a record whose probed epoch has left sight answers `base_moved`
(`rewrite_shadow_base_moved`): nothing recorded, the base read again. `write_epoch_model.ReaderOrderModel`
holds; `map_before_probe` (dev's order) and `no_moved_check` fail.

**Checks.** The two rows and write_epoch's `base_moved` row green; `data.write_` (epoch, model, admit, side view,
e2e), `data.accumulate`, `data.device_overlay`, `data.fsync`: 196 green; fmt, rails green. The loop, with verified
4k and 64k psync randwrite rows after every pass: dev fired at pass 20 alone and at pass 2 beside an fstests run,
each with a verify failure (1 MiB, 3 MiB of zeros) and the same event sequence; the fix, 60 passes (30 alone, 30
beside fstests): 0 tripwires, 0 `base_moved`, every byte verified, fsck clean. Smoke: the data plane with and
without the write cache, the overwrite and storm stages LOOPED ×24, the streaming overwrite, the promotion kill,
the kill matrix, the ordered law's kill row, fallocate's modes — pass. xfstests generic/001 013 074 075 112 127
263 — pass, 0 alert lines, fsck clean (an earlier pass lost 127 and 263 to an outside SIGKILL of the TEST daemon:
wait status 0x9, no panic, no OOM; rerun clean).

**Seen, not fixed** (§CN.6). fsck cannot see this loss (the drift path frees the displaced key). A Hole record
over a reservation mints a fresh dest, so a fallocate'd lay-down transiently holds twice its size (on 2 × 14 GiB
OSTs, right after unlinking the previous pass, the set latched exhausted and the lay-down crawled — not
investigated). `perf_mount.sh`'s teardown `cp`s the virtual `.stats`: the run records lack `stats-at-exit.json`.
