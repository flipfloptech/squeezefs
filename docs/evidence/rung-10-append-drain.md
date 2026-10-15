# Rung 10 — an extending append's drain, the transformed tail, ONE `.stats` document

Owed by `rung-10-dev-perf.md` §10.4. Decisions 0003 §AK.1–§AK.7; 0002 rows 534–536.

**Instrument.** Perf container `sqz-appamp` (1 MDT + 2 OST raw partitions, dist build, 4 MiB
blocks), formatted WITHOUT `--disk-cache-paths`. Shapes: `echo500` (500 × `echo <72 B> >> f`),
`fork2000` (one fd, 2,000 `/bin/echo`), `py{1k,4k}_{close,fsync}` (300 × open-append-close, ± an
fsync). Bytes per appended byte = OST diskstats ÷ file growth; both open postures.

**Cause.** A tail append is stream-adjacent, so it parks a DEFERRED full buffer; its drain read
the whole block (the complement) and uploaded the whole block to a fresh destination. A held-open
appender's first write parks a W2 page overlay and its second escalated it (a whole-block seed).

**Bound (§AK.1, §AK.3).** Over a sole-owner passthrough mapping a drain of `u` bytes writes its
touched pages in place: `pages(u, 1) × P` per drain, `≤ pages(e, a) × P / e` per appended byte;
edge seeds ≤ bytes written. Crash law = W1's: an edge page's untouched bytes are rewritten with
themselves — a kill at any grain leaves old bytes intact, each appended byte old or new. Per
block: one whole landing at its first drain (write-before-publish), so the harness verdict
`append_drain_bound`: `device ≤ (pages(bs, a) × P / bs + 1) × appended + files × P`.

**Red rows.** `write_admit.zig`: "the extending append's drain on a cache-less set" (65,536 B per
72 B drain), "the in-place drain's crash law" (a torn DMA at every grain), "a held-open appender's
ONE drain"; `write_e2e_linux.zig` (nvme_dev: ×57.7 per appended byte against ×58,254);
`transform.zig`, `read_path.zig` (a short image: EIO, then a panic), `accumulate.zig` (superseded
and trimmed-image ledger rows), the smoke row "THE TRANSFORMED APPEND"; `stats.zig`, `budget.zig`
and every smoke `.stats` read (strict parse).

**Passthrough, before → after (OST bytes, × per appended byte).**

| shape | control before | control after | no-open before | no-open after |
|---|---|---|---|---|
| echo500 (36 KB) | 1,740,644,352 (48,351×) | 1,851,392 (51.4×) | 4,202,496 (117×) | 40,960 (1.14×) |
| fork2000 (144 KB) | 4,198,400 (29×) | 147,456 (1.02×) | 12,591,104 (87×) | 151,552 (1.05×) |
| py1k_close | 1,170,219,008 (3,809×) | 1,216,512 (3.96×) | 4,202,496 (13.7×) | 311,296 (1.01×) |
| py1k_fsync | 1,237,327,872 (4,028×) | 1,216,512 (3.96×) | same | 1,216,512 (3.96×) |
| py4k_fsync | 1,249,910,784 (1,017×) | 1,228,800 (1.00×) | same | 1,228,800 (1.00×) |

Σ device-write arms ≡ OST diskstats to the byte on every row.

**Transformed (§AK.4–§AK.6).** Before: every append past 4 KiB answered EIO (the image seam read
the chunk-long window into a block-sized seed buffer). A transformed block cannot be patched in
place (a frame rewrite reuses its nonce), so its drain lands the image whole, now trimmed to the
live bytes: **per drain `≤ storedImageBound(r) = alignUp(table + r + ⌈r/F⌉τ, P)`**, per appended
byte `storedImageBound(r)/u` — the CoW price grows with the live extent `r ≤ B`. The untrimmed
image stored every zero frame raw on an encrypted set (≈ 4.2 MB per drain). Measured aes-256-gcm,
control arm: echo500 5.4 MB (151×), py1k_fsync 47.0 MB (153×, `Σ storedImageBound(r_i)` to 0.2 %),
py4k_fsync 186 MB (152×); lz4: echo500 1.2 MB (34×), py4k_fsync 1.9 MB (1.5×). Short images now
read zeros past their plaintext (a ranged read EIO'd; the fill's short decode panicked a serve,
`index 245760, len 65536`). `write_through_bytes`, `write_pipeline_superseded_bytes` (now an arm)
and `own_block_dma_bytes` count the stored bytes: Σ arms ≡ diskstats on all six aes rows.

**`.stats` (§AK.7).** Repeated keys: `/meta/node/meta_kv_appender_fence_breach` (a counter beside
the tripwire of the same site — the counter is gone) and `/mem_budget/mem_budget_components/
block_alloc_table` (one per data volume — one entry per name with `instances` and sums). Rails:
the render test parses strictly; every smoke `.stats` read parses strictly; `use_first` is gone.

**Found, not fixed.** `DEVICE_WRITE_REQS` double-counts DMAs that ride `put_obj` (wareq-sz only);
a short transformed image's whole-block fill still DMAs the chunk; a write past EOF inside a
promoted block read recycled bytes between the old end and the write (fixed below, §AL).

**Gates.** Full venue gate on the rebased tree: 49/49 steps, 2,917 / 2,997 (80 skipped); macOS
29/29, 2,266 / 2,295 (29 skipped).

## The extension law (decision 0003 §AL; 0002 row 538)

**Defect (data exposure).** A promotion lands a small file's image on its own block as one grain
(`ownBlockStored`); the rest of the block is whatever its previous owner wrote — a block-device
free elides its discard. A write past EOF then served that owner's bytes across `[EOF, write)`:
the W2 fold and the §AK drain land only their touched pages, the overlay seeds its gaps and the
CoW upload its complement from the old block. Not a read-path miss: the bytes were on the device
under a size that named them.

**Red rows.** `write_admit.zig` "a write past EOF never exposes the bytes a recycled block's
previous owner left": six arms (W2 fold in place, in-place drain, overlay landing, CoW of a shared
block, CoW on the transformed volume, a write into the NEXT block), live through the write side's
view and on the device — 0xAA at byte 4,096. `write_e2e_linux.zig` "a recycled block never leaks"
(`nvme_dev`): file X lands a block of 0xAA and is unlinked, small file Y's promotion re-mints that
exact block (`allocateSpecificBlock`), Y written past EOF read X's marker at byte 4,096.

**Fix.** `WritePath.zeroExtension`: a growth from `e` to `s` writes `[e, min(s, block end))` of a
mapped EOF block as zeros FIRST, through the ladder like any byte; the write then continues it,
stream-adjacent, in the same custody. Both growers call it (the striped write; setattr's grow,
before its custody flush so the zeros land before the size commits). A hole's EOF block and a
block-aligned EOF write nothing. Cost: zeros ≤ the growth inside the EOF block, landed with the
write — in place `pages(e, s − e) × P` (a 100 B → 24,577 B grow lands 7 pages, binding
unchanged); an adjacent append opens no gap and pays nothing. `write_gap_zero_bytes` joins the
admission ledger's closure. Truncate-up and truncate-up-then-write ride the same law; `fallocate`
and punch are not ops of this filesystem.

**The ordered law (§AL.5).** An extending write's size claim parks (fold-visible) and the times
drain journals `min(claim, custody floor)` behind one data barrier, so the durable size never
covers bytes still in RAM. Red: `kv_fs.zig` "THE ORDERED LAW" — on the pre-fix postlude a kill
before the drain read 20,010 B durable over 8,192 B of durable bytes (expected 8,192); the row
walks every kill point (before the drain, mid-drain, landed-undrained, drained: 8,192 → 8,192 →
8,292 → 8,292 → 20,010) and fsync (20,060 durable at return); the later write's base is the
earlier claim (8,292), never inside its range. Cost: the size lags acceptance ≤ `T_wb + τ`; one
data barrier per drain pass that journals a claim; one more meta barrier per fsync after an
extension.

**Cross-daemon size changes (§AL.6).** `two_holders.zig` "a shipped size change runs the
HOLDER's own size path": the holder applied the size alone; now it runs the filesystem's truncate
face or rejects.

**The interception-host row (gate F1 on the first cut).** Attributed to this wave, not the row's
flake (`flakes.md` names a different signature): `THE INTERCEPTION HOST` ×10 — `dev` 10/10 green;
the ordered law's first cut 7/7 red (`pread` of the ring-written block read 0 B: the plane's read
path clamped at the DURABLE size); folding the claim into `DataPlane.inodeSize` — 10/10 green.


## The ordered law on the write-back timer (2026-10-15)

**One crash law** (0003 §AJ.4 = §AL.5, charter §1.10): after a kill every fsynced byte and size is
present; the file ends at its last DURABLE size with only its own bytes below it; an unsynced
extension is whole or absent; never another file's bytes. The timer lands data, and a size follows
its data: the pass that landed a claimant's custody wakes the times drain once at its end
(`afterTimerPass`), so an unsynced extension is durable within `T_wb` + one drain. Under the close
posture the drained FLUSH/RELEASE journals the claim after its landing (`closeDrain`), and the
try-only FLUSH probe declines while a claim is parked.

**The handed-over red rows, attributed (rebased onto `dev` 703dd98, VM).** THE KILL MATRIX failed
on the in-flight file, not on the fsynced files: `c.size >= claimed` asserted the size `stat` saw
before the kill. That is the per-write size commit the ordered law removes (on `dev` the in-flight
file came back claiming 50 MiB over 48 MiB of bytes: 512 hole pages). Restated to the law: half the
kill point is fsynced (`conv=fsync`), the recovered size is ≥ that half, and every page below it
is the image's, with **no hole**. Measured: durable 25/50/75 MiB, 6,400/12,800/19,200 pages exact,
0 holes; acked files byte-exact. A NO-OPEN CLOSE / THE CONTROL ARM read `g-LOST`: the claim waited
for the next drain tick and the kill after the close came first. Both now await or assert
`meta_kv_times_echo_pending` 0.

**fsync, found by reading the floor path.** (1) The data leg flushed ONE census buffer (64) of
parked blocks: landed on its own on `dev` (`7ff9e42`/`e1c334f`, `accumulate.flushBornBy`). (2) The
drain clipped fsync's claim to the custody floor read AFTER the data leg, so a write that opened
custody under the claim (an overlay record, a rewrite epoch) left the fsynced extension undurable.
kv_fs "an fsynced extension is durable at its full size": 8,192 B durable after fsync of 8,292 B.
Fix: fsync and the close drain journal at least the claim found at entry (`landed`); a claim
re-parked below it after a size-setting commit is excluded.

**THE ORDERED LAW'S KILL ROW** (new; `SQUEEZEFS_TEST_OWN_BLOCK_POISON` writes a promotion's own
block whole with 0xAA before its image grain lands, so the bytes a recycled block's previous owner
leaves are planted on the file-backed rig). A 100 B file grows by one grain at 8,192 B, and the
daemon is killed at four points: right after the write, after the timer landed it, after
`conv=fsync`, and after an fsynced growth plus a truncate-up. Red on `dev` + the row: step write,
`y0` (12,288 B), byte 4,096 reads 0xAA. Green here: 100 / 12,288 / 12,288 / 20,480 B, every byte
the file's own.

**Cost.** Close posture: one data barrier per drained close of an extended file. Timer: no new
barrier (the woken drain is the one the tick would have run). fsync: unchanged.

**THE BLOCK-MAP TREE** (red in the branch's kamrui gate, `smoke.zig` "big claims ≥ the inline
cap" after the mid-crossing kill): the same per-write size commit the kill matrix assumed. Its
crash arm now runs `sync -d big` at the inline cap (1,364 MiB) before the crossing begins. Alone on
kamrui ×2: 1,364 MiB durable, 1,364 blocks exact, 0 holes.

**THE HANDOVER DRAINS PARKED STAMPS** (red in the branch's VM gate: times pending 70 at A, row
end). Its setup `sync -d d` (an fsync of the directory) journals only the claims whose bytes
landed. With the flush cadence held a minute away, the 72 files' claims stayed parked. The setup
is now `sync -f d` (a syncfs, which lands them): 2/2 green on the VM, pending 0.

**The writer-storm row** (owned by `storm` on `dev`). Alone on kamrui, same base (`e1c334f`), 5
runs each: `dev` failed 2 of 5, the branch 5 of 5, both with `waits + cold ≥ 1`. Instrumented
(admission bytes by arm): the 1,000 B tails never park on EITHER tree. The only parked buffers are
1 MiB heads spilled by `overlay_registry_full`, a must-stay-0 tripwire: accumulate bytes =
registry-full count × 1 MiB (`dev` 4 → 4.39 MB, 8 → 8.58 MB; branch 2 → 2.29 MB, 3 → 3.34 MB).
The row engages the gate only when that tripwire fires. The branch drops the per-write size
commit, so it overflows the registry less. This is the capacity defect `storm` is fixing, not a
change in where tails park.
