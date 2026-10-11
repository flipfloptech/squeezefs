# Rung 9 — read amplification: the ledger, the bounds, the A/B, the format call

The owner's ruling (2026-10-11): performance first — amplification is a product defect. This note
is the READ side: every device byte a read costs is in one ledger arm, every read shape has a
DERIVED bound (decision 0003 §K), every shape was measured against it on the venue, and every
shape over its bound was fixed red-first and re-measured as a counted A/B through the bracket
harness. The write side is `.wt-amp`'s (decision 0003 §J).

Venue: `7.2.6-sqz_aarch64_apple-vm_9c_virtio-blk` (9 vCPU, 16 GiB, three agents sharing the
cgroup; load 8–15 during every row below). Substrate `nvmet-loop` unless stated (3 kernel nvmet
namespaces, `logical_block_size 4096`, backed by sparse files on `/build`); `file` for the
cross-check. Harness: `scripts/bracket/bracket.sh`, 4 legs A→B→B→A × 60 s + 10 s warm-up
excluded, numjobs 4, iodepth 32, datasets 4 × 1 GiB (≫ the hot tier's 64–384 MiB), one set per
row aged across its legs. A = `564631b` (the ledger alone, no behaviour change over `dev`),
B = this series' head. Both ReleaseSafe (§6).

## 1. The ledger (`564631b`)

Every READ-side DMA byte lands in exactly one PURPOSE arm at the plane's one read choke point
(`plane.deviceRead(vol, off, out, purpose)`), and the arms close against the device total:

```
data_device_read_bytes ≡ read_dma_ranged_bytes            the ranged 4 KiB-outward window
                       + read_dma_block_demand_bytes      a demand whole-block fill (incl. ghost escalations)
                       + read_dma_block_prefetch_bytes    the stream pipeline's speculative fills
                       + read_dma_block_lane_bytes        the ahead lane's fills (FillClass.lane)
                       + read_dma_image_bytes             transformed images: the table grain + frame windows, whole images
                       + read_dma_window_bytes            mapping windows (packed tenants)
                       + write_side_read_dma_bytes        the write side's seed / overlay base reads behind a READ or a fold
                       + compose_read_dma_bytes           the W2 extent composition's ranged base window
                       + pack_read_dma_bytes              the pack re-stage source
```

Outside the plane, beside it: `transport_zc_direct_leg_dma_bytes` (the kernel session's direct
device leg into the registered reply window), `zcrx_fill_bytes` (the zcrx lane), and
`fsck_scrub_read_bytes` (recovery, excluded from every bound). The WHERE partition
`read_fill_dma_bytes + read_dest_dma_bytes` (pooled vs registered destination) sums to the same
plane total. The user side: `read_user_bytes` (FUSE READ bytes answered) and `ipc_bytes_out` for
the shim; `READ_COPY_ARMS` is the exact user-byte partition (`read_copy_dest_bytes +
read_dest_dma_bytes + read_zc_serve_bytes + transport_zc_direct_leg_serve_bytes` — the first cut
double-counted the dest copy's subsets and read 2.19× on the first tier-serving row). The
metadata side: `meta_kv_node_read_bytes ÷ (lookups + getattrs + fuse_readdir_entries)`.

`.stats` asserts the closure (`device_read_ledger_closed`), `brk.py` carries the lists
(`DEVICE_READ_ARMS`, `DEVICE_READ_PLANE_ARMS`, `DEVICE_READ_BESIDE`, `META_READ`) and the
verdicts `device_read_ledger` + `read_amplification_bound` on every leg, `bench.zig` pins the
same keys and the rails ("bracket ledger pin", "stats name census") pin both.

**The diskstats ruler.** `rung-09-brackets.md` F10/F12 had said reads on this venue never reach
a device. They do: the nvmet namespace's `/proc/diskstats` read sectors count every I/O issued to
the namespace even when its backing file sits in the VM's page cache — verified with one
`dd if=/dev/nvme0n1 iflag=direct bs=4k count=1024` (4,096 KiB of read sectors, exactly). Every
row below reports `device ÷ user` from the daemon's arms AND from diskstats; they agree to four
digits on every leg (the ledger is complete).

## 2. The bounds (decision 0003 §K; each a tie test and a per-leg harness verdict)

`g` = the DMA grain = the namespace's 4 KiB LBA (a 512 B direct read is EINVAL there);
`W(off, len) = ceil_g(off + len) − floor_g(off)`; `f = SQUEEZEFS_READ_ADMISSION_FILL_PCT` = 5 %.

| shape (row) | bound | the derivation |
|---|---|---|
| random sub-block, passthrough, not stream-classified, every tier missed | `W/len × (1 + f)` | one window per miss is the floor; the ghost escalation's waste is governed to `f` of the foreground device bytes |
| `rand-4k-read`, `shim-rand-4k-read` | 1.05 | aligned: `W = len` |
| `rand-4k-read-unaligned` (512 B grain) | 2.1 | `max W(off, 4096) = 8192` |
| `rand-512b-read` | 8.4 | `W = 4096`; **8× stands on a 4 KiB-LBA device** (§K1: every reply path is page-grained; a 512 B-LBA device's measured grain is the upgrade) |
| `rand-256k-read`, `rand-1m-read` | 1.05 | the ranged ceiling is DERIVED, `block − g`: a random reader has proven no contiguity; a whole-block fetch for a 1 MiB random read was 4× for nothing |
| `seq-read-1m-*`, `shim-seq-read-1m` | 1.05 | a classified stream fetches every block once |
| `seq-read-early-stop` (first 8 MiB of many files) | 2.1 | ≤ one AIMD start window (2 blocks = 8 MiB) of overshoot per stream |
| `read-after-small-write` | 2.1 on the read arms | extent records compose over a RANGED base window; the fold's seed reads are the write ledger's, reported beside |
| transformed `--compress` / `--encrypt`, random 4 KiB | 18 | the table grain + one 64 KiB frame window: `(4 + 68) / 4` on an incompressible frame (§4) |
| `stat-mdtest`, `find-readdir` | cold ≤ 3 node reads/record, warm ≈ 0 | one root→leaf walk per cold record (tree height 3); the node cache holds the working set |

## 3. Measured: A (ledger only) → B (the fixes), through the harness

Ratios are `device ÷ user`; "A" rows from the ledger build before any fix (scoping legs of
2026-10-11 15:37–16:04 and the counted A legs), "B" from the A/B legs of 18:16 onward. Every leg's
stats and diskstats ratios agree; every plane ledger closed.

| shape | bound | A | B | verdict |
|---|---|---|---|---|
| `rand-4k-read` | 1.05 | 1.000 | 1.000 (direct leg) | in bound before and after |
| `rand-4k-read-unaligned` | 2.1 | 1.699 (scoping) · **2.986 / 2.811** (nvmet-loop) · **2.958 / 2.678** (file) | **1.970 / 2.001** (nvmet-loop) · **2.005 / 1.991** (file) | fixed (the governor, §5.2) |
| `rand-512b-read` | 8.4 | **20.9** (scoping) · **17.27 / 16.90** (nvmet-loop) · **17.00 / 16.96** (file) | **8.236 / 8.328** (nvmet-loop) · **8.328 / 8.379** (file) | fixed (§5.2); 8× is the LBA law |
| `rand-256k-read` | 1.05 | 1.000 | 1.000 | in bound (the direct leg at 256 KiB) |
| `rand-1m-read` | 1.05 | 1.000 | 1.000 | in bound (the direct leg); the derived ceiling covers the pooled path |
| `seq-read-1m-direct` | 1.05 | 1.000 | 1.000 | in bound |
| `seq-read-early-stop` | 2.1 | 1.000 | 1.000 | in bound: the pipeline stops with the reader |
| `read-after-small-write` | 2.1 | **4.431** (scoping) · **2.644 / 3.189** (nvmet-loop) · **2.649 / 3.639** (file) | **2.009 / 1.993** (nvmet-loop) · **1.987 / 1.994** (file) | fixed (§5.2: the demand arm was the governor's escalations, 4.3–8.6 GB per A leg; the composition itself is ranged — one 4 KiB `compose_read` window per overlaid read) |
| `shim-rand-4k-read` | 1.05 | 1.000 | 1.000 | in bound |
| `rand-4k-read --compress zstd` | 18 | **446.8** | §4 | fixed by the framed image |
| `rand-4k-read --encrypt aes-256-gcm` | 18 | **446.5** | §4 | fixed by the framed image |
| `rand-4k-read --compress lz4` | 18 | **446.7** | §4 | fixed by the framed image |
| `stat-mdtest` (100 k files) | cold ≤ 3 / warm 0 | cold 0.00026 reads/record (26 reads, 68 B/record); warm 0 | — | in bound |
| `find-readdir` (100 k files) | cold ≤ 3 / warm 0 | 0 node reads / 103 k records (same-mount cold walk) | — | in bound; see §3.2 |

### 3.1 The counted A/B legs (`bench/brackets/<venue>/<row>-2026-10-11T18*.{json,md}`)

Six rows (three shapes × `nvmet-loop`, `file`), A→B→B→A, 60 s legs; the per-leg numbers are in
the table above. Every A leg over its bound is tagged INVALID by the harness's new
`read_amplification_bound` verdict (the defect, now a verdict); every B leg is in bound. Two B
legs (leg 2 of `rand-512b-read` on nvmet-loop, leg 3 of `read-after-small-write` on file) are
tagged INVALID on `mem_budget_green` alone — level 2 at the after-snapshot with < 400 MiB of
components under a 6 GiB budget: the budget reads the shared cgroup, where two other agents'
daemons and suites ran at load 12–15 throughout; their ratios (8.236, 1.994) match their twin
legs. On `file` the exclusive-namespace ruler is unavailable (the backing files share `/dev/vdc`
with everything else), so the verdict uses the arms and diskstats is reported beside (it reads
0.1–1.1 higher: the other agents' traffic). IOPS A/B: 0.94–1.02 (B never slower; the ranged path
is cheaper than the escalations it replaces).

### 3.2 Metadata

`stat-mdtest` (mdtest `-T` over 100,242 files): the first cold pass (the creating mount's
warm-up) cost 26 node reads / 6.8 MB for 100,242 records = **0.00026 node reads per record,
68 B/record**: the tree's 100 k inodes live in ~1,660 leaves, each loaded once. Warm: 0 node
reads, 200,471 cache hits for 100,264 records. The REMOUNT-cold pass (leg 2 of the 2-leg scoping
run, a fresh mount after a clean unmount): **2 node reads / 512 KiB for 100,204 records**
(5.2 B/record). `find-readdir` (103,003 dirents + 2,003 readdirs + ~800 lookups per walk):
0 node reads on both the creating mount's walk (91.6 k entries/s) and the remount-cold walk
(200 k entries/s, 112,030 node-cache hits, 0 misses). The mount's ledger scan loads the 66 trees
(≈ 7 MB, far under the node-cache budget) before `ready`, so a remount is warm for a tree this
size; the per-record bound (≤ 3) is exercised only by a tree larger than the node cache, which
this venue's rows do not build. Both rows' `META_READ` ledger (`node_read_bytes ÷ records`) is
in place for that.

## 4. Transformed volumes: the format call

Before: one AEAD/compression frame per block, so a 4 KiB read decoded the whole 4 MiB image —
`A = chunk/len` = 1025× in principle, **446× measured** (zstd 446.8, AES-GCM 446.5, lz4 446.7 on a
1 GiB set of which the 384 MiB hot tier held a third). The honest bound of the one-frame format IS
1025×; no cache mitigates a random reader over a set larger than RAM.

The fix is a format change (we are pre-release on format v4 with no compatibility obligation,
decision 0002): **the framed image** (transform version 2, decision 0003 §K7, porting spec
`data_transform_config.md` §1.4):

- `header (96 B) ‖ frame table (16 B head: xxh3 over the entries, count, grain; 16 B entries: off,
  body_len, plain_len, flags) ‖ frames ‖ zero pad to 4 KiB`; frame `F = 64 KiB` of plaintext
  (lz4's match window — no ratio lost; zstd −3 loses a few percent; AES-GCM pays 28 B per frame),
  each compressed with its own raw escape and sealed under its own nonce (base counter + index)
  with AAD = header ‖ entry ‖ index.
- **The table is compact, never a padded page**: frame 0 begins at `tableLen(count)` (128 B for one
  frame, 1,136 B for 64). A 4 KiB header page per image — the first cut — would have doubled every
  small packed tenant's slot (`pack_slot_len = ceil_4k(image)`: a 1 B file 4 KiB → 8 KiB) and the
  DMA that reads it. The headroom stays one grain (`chunk = block + 4 KiB`).
- The sub-block read (`read_path.zig` arm 8f): DMA the first grain (the table always fits it), learn
  the span's window, DMA the window (reusing the grain in hand when frame 0 is inside it), decode
  those frames alone; never published; ghost-escalated like a ranged read.
- The data superblock's `features_incompat` bit 0 (`feature_framed_images`) names the format;
  `format` stamps it on transformed volumes; an older train refuses the bit loud at mount; this
  train refuses a transformed volume without it (reformat). fsck's C-pass verifies every frame
  through the same decoder (table xxh3, body coverage, every tag/decode). New decoders carry
  `test "fuzz: …"` rows (`tableFuzz`, the span fuzz) per the fuzz census rail.

Bound after: `(g + ceil_g(F + tag)) / len × (1 + f)` = **≤ 18×** at 4 KiB on an incompressible frame,
`(4 + ceil_4k(F/ρ)) / 4` at ratio ρ (9× at ρ = 2); ≤ 1.02× at 1 MiB. Measured after: §4.1.

### 4.1 Measured before/after (`scripts/bracket/xform-probe.sh`: 4 × 256 MiB, fio randread 4 KiB
O_DIRECT, 4 jobs × QD16, 20 s; A = the one-frame format, B = the framed image)

| volume | source | A `device ÷ user` (IOPS) | B `device ÷ user` (IOPS) | bound |
|---|---|---|---|---|
| `--compress zstd` | incompressible (urandom: every frame raw) | **627.1** (2,623) | **15.58** (13,363) | 18 |
| `--encrypt aes-256-gcm` | incompressible | **627.9** (836) | **15.53** (12,462) | 18 |
| `--compress lz4` | incompressible | **627.4** (3,134) | **15.44** (15,415) | 18 |
| `--compress zstd` | compressible text (ρ ≈ 3.5 at 64 KiB) | never published (§4.2: 335 `ProofMissing`, reads from custody) | **10.15** (849; ranged arm alone 5.75, the rest the tier fill's escalations in a 20 s probe) | `(4 + ceil_4k(64/ρ)) / 4` ≈ 6–7 on the ranged arm |
| `--compress lz4` | compressible text (ρ ≈ 2.2 at 64 KiB) | never published (§4.2) | **10.43** (7,795; ranged arm alone 9.62) | ≈ 10 on the ranged arm |

The A ratio is the whole image per 4 KiB read (627 = `chunk / len` × the share the hot tier did
not hold); B is the table grain + one 64 KiB frame window rounded outward (`(4 + 68) / 4 = 18` at
the bound; 15.5 measured because a 4 KiB read inside frame 0 reuses the grain in hand and a
window's rounding is one grain, not two, when the frame starts aligned). Every B arm is
`read_dma_image_bytes` alone; the ledger closed on every run; 256 ghost escalations per run (the
second touches, one whole image each) are inside the 5 %. On the compressible runs the
escalations are the 64–96-slot tier FILLING (73 and 123 whole images in 20 s with no evictions
yet, so no evidence for the clamp): the ranged arm alone is at the ρ law. The zstd sub-block
path's IOPS (849) is the plane's ONE transform context: `Decode.decode_range` takes `x.lock`, so
every frame decode is serialized (lz4 at 7.8 k IOPS shows the same lock at a faster codec); a
per-lane decode context is the throughput follow-up (§7) — the amplification is in bound.

### 4.2 The write-side defect the compressible run exposed (fixed, `807d6e2`)

With a compressible source, BOTH arms logged `the block-map publish of 1 binding(s) failed
(ProofMissing) — the upload keeps its custody and retries` on every block (768 per run) and the
reads then came from the write side's custody at RAM speed (280 k IOPS, 0 device bytes).
`DmaProof.covers` demanded that a whole-block mapping's proof span the whole slot
(`slotLen` = the chunk), but `DeviceSeam.write` mints the proof at the image's grain-padded length
— so on a compressed volume **every block that actually compressed never published** (the
"zstd write wedge" of the first scoping pass; incompressible images are exactly one chunk and
passed). Red: `layout` "publishLayout …" (a 1 MiB proof from the slot's start covers a whole-block
mapping; a proof from mid-slot, or shorter than a grain, does not; a tenant's proof still covers
its slot). Fix: a whole-block DMA is the complete payload or image by construction (an image is
self-delimiting; a passthrough payload is bounded by the layout's size), so its proof covers the
mapping from the slot's start at any grain-padded length; the own-block mint's proof names what
it landed instead of being inflated to the block. The compressible A/B is re-measured under the
fix in §4.1's last row.

## 5. The fixes, red-first

### 5.1 The ranged ceiling is derived (`9afb295`)

Root cause: `SQUEEZEFS_READ_RANGED_THRESHOLD = 262144` — the reference's read-LRU population
bound borrowed as a DMA policy — sent every random read above 256 KiB to a whole-block fetch (4×
at 1 MiB on the pooled path). Red: `read_path` "the ranged ceiling is derived" tie test
(`derivedRangedThreshold(block) = block − g`; `rangedEligible` at 1 MiB and at `block − 4 KiB`,
refused at `block` and under a fresh stream). Fix: the knob's default is `derived`
(`resolveRangedThreshold`), 262144 stays the A/B lever, 0 the kill switch.

### 5.2 The admission governor (`9afb295`, `8ed8824`)

Root cause (A: 17–21× on `rand-512b-read`, 2.8–3.0× on `rand-4k-read-unaligned`): the clamp read
the PREVIOUS epoch alone, so wherever the trickle bought fewer than two blocks per epoch the
throttled epoch's single eviction failed the floor and the next epoch re-admitted everything the
cooldown allowed — a tier's worth every two epochs; and a released clamp opened in one step. Red:
`admission` "governor: waste → clamp → denials …" (hand-advanced epochs; the ladder 5 → 10 → 20 →
40 → 80 → 100; a wasteful epoch resets) and `read_path` "the bounded-waste law under a random
sub-block storm …" (a 16-block set over a 4-block tier). Fix: the verdict is re-decided at the
roll from the finishing epoch's own eviction evidence (no evictions ⇒ it stands), releases by the
additive-increase ladder, pays back in the device bytes a hit avoided; gauge
`read_admission_governor_share_pct`.

Second root cause, found on the ReleaseSafe A/B (B 9.94× whole-run on the 60 s
`scripts/bracket/read-probe.sh` storm): read at
the roll alone, a storm's ONSET escalated at device bandwidth for a whole 2 s epoch before the
first verdict — 1.37 GB of whole blocks, 30 % of the foreground. Red: the same two tests flipped
to the stronger law (engaged inside the epoch after the 6th escalation of pass 2; the 7th and 8th
denied). Fix (`8ed8824`): `Governor.engage` from `onEviction` the moment the running epoch's
victims clear the floor ∧ ratio; tokens stay 0 until the roll mints them; the release stays the
ladder at the roll (anti-flap preserved). After: 512 MiB per 60 s storm = the 96-slot tier fill +
the 5 % trickle; 8.50× whole-run with the onset, 8.24–8.33× in the harness window.

### 5.3 The ledger's own defects (`564631b`, `9afb295`)

`read_lane_fetch_bytes` was declared and printed but never incremented (the ahead lane's fills
are their own `FillClass.lane` now); `READ_COPY_ARMS` summed subsets (2.19× on the first
tier-serving row); `fuse_readdir_entries` is the dirent denominator; the transport's direct leg
counts its DMA apart from its serves.

## 6. The measurement finding that reordered the bisect: a Debug daemon

The first A/B (Debug daemons: this branch predates `dev`'s ReleaseSafe-exe `build.zig`) showed
"B" 2–4× slower than "A" with 150 MB–1.4 GB of anonymous RSS growth per 30 s storm, deterministic
across back-to-back runs, independent of the config values (DBG-printed identical) and present or
absent across binaries that differed by one `orelse`. gdb (`thread apply all bt`) on the slow
daemon's lanes: `heap.SafeAllocator.captureStackTrace → debug.captureCurrentStackTrace →
Elf.unwindFrame → Io.RwLock.lockShared → futex` — `std.process.Init.gpa` is `SafeAllocator` in
Debug AND ReleaseSafe (`start.zig`), but only Debug captures a 7-frame DWARF stack trace on every
alloc/free under `SelfInfo`'s one RwLock and `overwriteFreed`s with `0xAA` (the growth: buckets
pinned by surviving small allocations). The trigger is code-layout-dependent. Rebuilt ReleaseSafe
(after `git rebase dev`): A′ 19,003 IOPS, B′ 19,054 IOPS, both +5 MB RSS, both exactly 8.000× with
admission off. Every number in §3 is from ReleaseSafe daemons unless marked.

## 7. What stands, with its reason

- **8× at 512 B** stands on a 4 KiB-LBA device (§K1): the DMA grain is the device's; a 512 B grain
  for a 512 B-LBA device is the measured-evidence upgrade (OQ #1 of the reference design).
- **`read-after-small-write`'s background fold reads** (`write_side_read_dma_bytes`, 2.6 % of the
  user bytes on the probe) are the W2 fold's — the write-amplification agent's arm (defect 24).
- **The metadata bound is not exercised by a 100 k-file tree**: the mount pre-loads it (§3.2); the
  ledger and the rows stand for a tree larger than the node cache.
- **The transformed sub-block decode is serialized** under the plane's one transform context
  (`x.lock` in `Decode.decode_range`): 849 IOPS for zstd, 7.8 k for lz4 — in bound on
  amplification, a throughput item for the owner (a per-lane decode context).
- **The compressed-volume publish wedge** (`ProofMissing` on every block that compressed) was the
  write side's and is fixed here (§4.2) because it stood between the format change and its
  measurement; the W2 fold and the staged append stay `.wt-amp`'s.

## 8. Gates and suites

- macOS `zig build gate` on the rebased series: 28/28 steps, unit 1,926 pass / 25 skip, rails 16/16,
  `zig fmt --check` clean.
- Venue `zig build gate` (ReleaseSafe exe, 7.2.6-sqz): unit 2,361 pass / 41 skip, rails 16/16, smoke
  61 pass / 2 skip (the interception host's step 3 restated to the derived ceiling: the unaligned
  512 KiB read's second touch escalates, then the aligned read serves warm), bench-smoke 40,
  shim-smoke 40. One run saw `shim.e2e` "fio's shape" collect 3 of 4 stub-host completions inside
  its 5 s deadline under the gate's full parallel load; it passes 7/7 in isolation on this tree
  and on `dev`, and the re-run gate on the rebased series was clean: 46/46 steps, 2,554 pass /
  43 skip (unit 2,362, smoke 61, bench-smoke 40, shim-smoke 40).
- `scripts/suites/fstests.sh` generic/013, 075, 112, 127, 263 on A and on B: all five pass, fsck
  `--offline --scrub` 0 findings on both sets. Both arms log the same single alert class,
  `write_shared_class_violation` (16 on A, 19 on B: the shared-mode write classifier against
  striped files extended by fsstress — the write side's, pre-existing on `dev`); the suite's
  "stale ledger rows" are the adjudicated failures a 5-test subset never reaches.
- Every read-path stale-serve law stands: the binding-validated serves (every arm re-checks the
  binding and the fill incarnation after its bytes are in hand, arm 8f included), the one purge
  helper over all tiers, the coverage-aware zero fill; `fsx`/`fsstress` rounds above are green.

## 9. The commits (on `dev`, in order)

`907c3de` the ledger · `f05a7a5` the governor's ladder, the derived ceiling, the rows, §K ·
`8ba05c3` the framed image · `e0a21f2` the governor's mid-epoch engage · `2560995` the compact
table, the one-grain headroom, the feature bit · `569f1dd` §K7/§K3, porting §1.4 · `62ed47c`
`BACKOFF_SHIFT_CAP`, the sequential rows' bounds, brackets.md §4.1, 0002 rows 473–476 · `2a655d4`
the whole-block proof law · `92feb14` this note · `d00f8d8` `read-probe.sh` · `f9239a6` the
records · `3fe8f98` the interception host smoke · the landing commit (this section, the gate
totals).
