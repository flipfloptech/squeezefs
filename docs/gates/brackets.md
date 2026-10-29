# Gate: the box brackets (charter §11 measurement law, §13 bars)

The acceptance procedure for the performance bars. It runs on the **quiet
reference box** only — a 32-core client, five memory-backed nvme-tcp targets,
the sqz kernel — with the `dist` profile binary. Dev-box rows (the macOS
`container` venue, a laptop, a VM) are SCOPING and never acceptance; cloud
runs need explicit per-run approval. Every number this gate accepts comes out
of `scripts/bracket/bracket.sh` unmodified: the harness IS the instrument of
record, and a hand-typed number is not a result.

## 1. The law a row obeys (what the harness enforces)

1. **Header**: instrument (name + version + the exact shape), substrate (the
   full chain to the medium), venue id (`<kernel>_<arch>_<cpu>_<cores>c_<disk>`
   + RAM + `fuse.enable_uring`), build profile (`squeezefs --version`: train,
   commit, profile; a `-dirty` commit is refused for acceptance), date.
2. **A-B-B-A on one aging store**: the set is formatted once; legs run
   A → B → B → A, each a fresh mount of the leg's binary over the same
   volumes and the same files; a warm-up span precedes every measured span and
   is excluded. The order cancels monotone drift (the aging store, thermal
   state, a filling disk) to first order; the spread within each pair bounds
   what drift remains.
3. **≥ 60 s sustained** per measured span for every headline row. The harness
   refuses a shorter span without `--scoping`.
4. **Write rows carry** `device bytes ÷ user bytes` (the daemon's device-write
   arms AND `/proc/diskstats` deltas on every backing namespace — data and
   metadata separately), `wareq-sz` vs the block size (diskstats
   `sectors ÷ writes`, and the daemon's own request counters beside it), the
   reclaim ledger `queued + elided ≡ terminal frees` (delta and absolute at
   quiesce), the admission ledger `Σ admit_write_* ≡ user bytes`.
5. **Read rows carry** the read copy ledger
   (`Σ {read_copy_*, read_dest_dma, read_zc_serve, transport_zc_serve, …} ≈ user bytes`).
6. **Every row carries** `invariant_tripwires` and every must-stay-0 (Δ 0), the
   tick rescues (transport park, journal lane, conveyor, reclaim, exec lanes,
   ticked re-registers), the memory budget level (Green for the whole leg), the
   solo mount's wire census (0), and the fsync phase exact sum when the family
   moved.
7. **Engagement**: the mechanism under test must account for the row's ops —
   kernel-FUSE rows by `Δtransport_ring_requests ≥ ops` (direct) / `> 0`
   (buffered; `fuse_ops` is only the handler-lane share: a zero-copy
   session's direct-leg reads are served at the delivery CQE and never reach a
   lane); shim rows by `Δ(ipc_ops_read + ipc_ops_write) + lane-gate routes ≥ ops`
   (an op above the hybrid lane gate routes to kernel FUSE by design and is
   judged on the kernel lane). The shim rows run fio's **libaio** engine — the
   box's instrument shape — through the shim's `io_queue_init` interposition
   (the first scoping day ran them psync because libaio's queue face bypassed
   the shim: rung-9 finding F3); the verdict's detail prints the daemon's
   admission refusals (a refused client process is kernel-served for its
   backoff window) and `ipc_client_aio_unknown_ctx_ops` (iocbs on a context
   the shim never registered) so a deficit names its cause. A row that fails
   is **INVALID** and stays in the record with its reason; it is never a number.
8. **Noise floor first**: before any A ≠ B bracket, run the row with
   A = B = the candidate; the spread over the four legs is the venue's noise
   band for that row and is recorded with the row. A difference inside the
   noise band is not a difference.
9. **The 3-run discipline** (charter §11 "multi-run"): acceptance of a bar
   needs **three consecutive VALID brackets** on the same binary, each meeting
   the bar on **both A legs** (A = the candidate, B = the previous accepted
   binary, or A = B when none exists). A failure or an INVALID row on any run
   aborts the count; the count restarts from zero on the fixed binary. Any
   change to the binary restarts the count.

## 2. The box

| item | requirement |
|---|---|
| client | 32 cores (the reference class), one NUMA domain or the recorded topology; Linux ≥ 7.3 with `fuse.enable_uring=Y` (the venue's `mainline-sqz` kernel, decision 0003 §CT), buffer pools and zero-copy armed (the daemon's INIT line logs `bufpool present, zero-copy true`) |
| targets | 5 memory-backed nvme-tcp targets (kernel nvmet; `nvmeof share --ip … --port …`, memory-backed namespaces), connected with `nvmeof connect`; 4 data + 1 meta, or the recorded split |
| quiet | nothing else runs: no gates, no other agents, no `pkill` sweeps; `loadavg` at the start ≤ 0.5 and recorded |
| build | `zig build -Dprofile=dist dist`; `squeezefs --version` says `dist`; the shim from the same build (`lib/libsqueezefs_il.so`) — the daemon refuses to pair across builds |
| instruments | fio ≥ 3.36; OpenMPI; IOR/mdtest at io500's pinned hash (`5528b3619236d475`); io500 `io500-sc26_v1` with its pfind — versions recorded in each record |
| scratch | enough for the dataset the shape needs (≥ 16 GiB per the bench's auto shape so no RAM tier holds it; the harness's default is 4 × 1 GiB files — pass `BRK_FILE_SIZE=4g`), the memory budget default (no `--budget-mb`) |

## 3. The exact commands

```sh
# once per box: the instruments
apt-get install -y fio openmpi-bin libopenmpi-dev electric-fence
git clone https://github.com/hpc/ior.git && (cd ior && git checkout 5528b3619236d475 && ./bootstrap && ./configure --prefix=/build/tools && make -j && make install)
git clone https://github.com/IO500/io500.git && (cd io500 && git checkout io500-sc26_v1 && ./prepare.sh)   # or link build/ior to the tree above
export BRK_TOOLS=/build/tools/bin IO500_SRC=/path/to/io500

# the candidate (dist) and the incumbent
A=/opt/squeezefs/<candidate>/bin/squeezefs; B=/opt/squeezefs/<incumbent>/bin/squeezefs
H=scripts/bracket/bracket.sh

# 0. the noise floor, per row (A = A)
$H --substrate nvmet-tcp  seq-read-1m-direct  $A $A
# … every row below once with $A $A

# 1. bandwidth (fabric-sensitive → nvmet-tcp), shim data path = the charter's bars
BRK_FILE_SIZE=4g $H --substrate nvmet-tcp shim-seq-read-1m   $A $B      # bar: read ≥ 43.9 GB/s
BRK_FILE_SIZE=4g $H --substrate nvmet-tcp shim-seq-write-1m  $A $B      # bar: write ≥ 36.4 GB/s
BRK_FILE_SIZE=4g $H --substrate nvmet-tcp seq-read-1m-direct  $A $B     # kernel FUSE, informational beside the shim row
BRK_FILE_SIZE=4g $H --substrate nvmet-tcp seq-write-1m-direct $A $B
BRK_FILE_SIZE=4g $H --substrate nvmet-tcp seq-read-1m-buffered $A $B    # hybrid I/O: the same mechanism under the page cache
BRK_FILE_SIZE=4g $H --substrate nvmet-tcp seq-write-1m-buffered $A $B

# 2. IOPS (latency-shaped → nvmet-loop; the fabric row on nvmet-tcp beside it)
$H --substrate nvmet-loop --numjobs 32 shim-rand-4k-read  $A $B          # bar: ≥ 942 k read IOPS
$H --substrate nvmet-loop --numjobs 32 shim-rand-4k-write $A $B          # bar: ≥ 727 k write IOPS
$H --substrate nvmet-loop --numjobs 32 rand-4k-read  $A $B               # bar: kernel-FUSE rand-4k ≥ 555 k IOPS on the sqz kernel
$H --substrate nvmet-loop --numjobs 32 rand-4k-read --mount-opts direct_device_true $A $B   # the ruler row (device-true)
$H --substrate nvmet-loop --numjobs 32 rand-4k-write $A $B
$H --substrate nvmet-tcp  --numjobs 32 shim-rand-4k-read $A $B           # the fabric-sensitive twin
$H --substrate nvmet-loop --numjobs 32 rand-4k-read-zstd $A $B           # the transformed twins (decision 0003 §K7/§K10):
$H --substrate nvmet-loop --numjobs 32 rand-4k-read-lz4  $A $B           #   device ÷ user ≤ 18.9 per leg, IOPS vs rand-4k-read
$H --substrate nvmet-loop --numjobs 32 rand-4k-read-aes  $A $B

# 3. metadata
$H --substrate nvmet-loop --numjobs 32 creates-mdtest $A $B              # bar: ≥ 32 k creates/s many-dirs
$H --substrate nvmet-loop rename-unlink $A $B                            # bar: ≤ 1.05 journal entries/op
$H --substrate nvmet-loop tar-x $A $B                                    # bar: 0 wire verbs/entry, wire census 0
$H --substrate nvmet-loop fsync-4k $A $B                                 # fsync phases, exact sum

# 4. the posture row: a solo symmetric mount vs --single-writer, same binary
$H --substrate nvmet-loop --numjobs 32 --lever posture creates-mdtest $A $A   # bar: A within the row's noise band of B, wire census 0

# 5. io500 (the distribution's config-minimal.ini, unmodified)
scripts/bracket/io500.sh --substrate nvmet-tcp --np 32 $A
```

Every command writes `bench/brackets/<venue-id>/<row>-<date>.{json,md,log}`
and the legs' daemon logs beside them. Commit the records with the evidence
note that cites them.

## 4. Pass criteria

A bar passes when, over three consecutive runs of its row (§1 item 9), every
leg is VALID and the A arm's mean meets the bar:

| bar (charter §13) | row | number | substrate |
|---|---|---|---|
| read 43.9 GB/s (shim) | `shim-seq-read-1m` | `headline.A.mean` MiB/s ≥ 41,864 | nvmet-tcp, 5 memory-backed targets |
| write 36.4 GB/s (shim) | `shim-seq-write-1m` | ≥ 34,714 MiB/s | nvmet-tcp |
| 942 k read IOPS (shim, 4 KiB) | `shim-rand-4k-read` | `headline.A.mean` ≥ 942,000 | nvmet-loop (the latency ruler) and nvmet-tcp |
| 727 k write IOPS (shim, 4 KiB) | `shim-rand-4k-write` | ≥ 727,000 | nvmet-loop and nvmet-tcp |
| kernel-FUSE rand-4k ≥ 555 k IOPS | `rand-4k-read` | ≥ 555,000 | nvmet-loop, sqz kernel, zero-copy session |
| creates ≥ 32 k/s many-dirs | `creates-mdtest` | `creates/s` ≥ 32,000 | nvmet-loop |
| rename/unlink ≤ 1.05 journal entries/op | `rename-unlink` | `journal entries/op` ≤ 1.05 on every leg | nvmet-loop |
| `tar -x` 0 wire verbs/entry in its own tree | `tar-x` | `wire verbs/entry` = 0 on every leg | nvmet-loop |
| solo mount within noise of single-writer, `dlm_rpcs == 0` | `creates-mdtest --lever posture` | `a_over_b` within `1 ± noise_band` of the row's A = A run; `wire_rpcs` Δ 0 on every leg (the port's wire census: `cluster_wire_frames_sent + joined_wire_verbs + xv_cross_owner_steps_shipped + slot_resolve_rpcs + xv_cross_owner_guard_rpcs + dlm_token_grants + slot_ships + free_shipped_blocks`) | nvmet-loop |

Plus, on every accepted row: `device ÷ user` (diskstats, data namespaces)
within 1.0–1.1 for a write row of whole-block shape and reported for every
other; `wareq-sz` ≥ the block size for ≥ 4 KiB-aligned rows (a smaller value
is a split the record must explain); `queued + elided ≡ terminal frees`;
`Σ admit_write_* ≡ user bytes`; tripwires Δ 0; tick rescues Δ 0 (a non-zero
rescue is a lost-wake finding, recorded, and the row is re-run after the
fix); mem budget Green.

### 4.1 The read-amplification rows (decision 0003 §K; `docs/evidence/rung-09-read-amplification.md`)

Every read row carries a DERIVED `device ÷ user` bound the harness asserts per
leg (`read_amplification_bound`; the ruler is the exclusive namespaces'
`/proc/diskstats` read sectors, which count every direct read issued to the
namespace even when its backing file sits in the VM's page cache, else the
daemon's device-read arms) and the device-read ledger's closure
(`device_read_ledger`: `Σ` the plane's purpose arms `≡ data_device_read_bytes`).
`f = SQUEEZEFS_READ_ADMISSION_FILL_PCT` (5 %) is the ghost escalations' share.

| shape | row | bound (`device ÷ user`) | WHY | substrate |
|---|---|---|---|---|
| random 4 KiB, aligned | `rand-4k-read`, `shim-rand-4k-read` | ≤ 1.05 | one 4 KiB window per miss × (1 + f) | nvmet-loop |
| random 4 KiB at a 512 B grain | `rand-4k-read-unaligned` | ≤ 2.1 | `max W(off, 4096) / 4096 = 2` × (1 + f) | nvmet-loop |
| random 512 B | `rand-512b-read` | ≤ 8.4 | the DMA grain is the namespace's 4 KiB LBA: `W/len = 8` × (1 + f) | nvmet-loop |
| random 256 KiB / 1 MiB | `rand-256k-read`, `rand-1m-read` | ≤ 1.05 | every strictly sub-block random read is ranged (`block − 4 KiB` ceiling) | nvmet-loop |
| sequential, full files | `seq-read-1m-direct`, `seq-read-1m-buffered`, `shim-seq-read-1m` | ≤ 1.05 | whole blocks once; prefetch is read, never dropped | nvmet-loop |
| sequential with an early stop (8 MiB of 1 GiB files) | `seq-read-early-stop` | ≤ 2.1 | ≤ one prefetch window of overshoot per stream | nvmet-loop |
| 4 KiB reads after 512 B random writes | `read-after-small-write` | ≤ 2.1 on the read arms | extent records compose over a RANGED base window; the fold's seed reads (`write_side_read_dma_bytes`) are the write-amplification ledger's and reported beside | nvmet-loop |
| random 4 KiB on `--compress` / `--encrypt` | `rand-4k-read-zstd`, `rand-4k-read-lz4`, `rand-4k-read-aes` (each names its own `--format-opts`) | ≤ 18.9 (incompressible frames) | the table grain + one 64 KiB frame window: `(4 + 68) / 4` × (1 + f); the headline IOPS is the transform's throughput (§K10: a codec per lane, `transform_codec_pool_waits` 0), judged against `rand-4k-read` on the same venue | nvmet-loop |
| `stat` storm / `find` over 100 k files | `stat-mdtest`, `find-readdir` | cold `node reads/record ≤ 3` (the tree height); warm ≈ 0 | one root→leaf walk per cold record; the node cache holds the working set | nvmet-loop |

## 5. What a failure means

- An INVALID row is a **defect report**, filed red-first against the ledger
  that did not close, with the record attached; the count restarts after the
  fix. It is never worked around by a weaker posture (charter §1.6, §1.8): no
  knob, no option, no smaller shape makes a number.
- A VALID row under the bar is a **performance defect**: the charter's
  mechanism holds its slot on measured evidence; the fix is a counted A/B
  (A = fix, B = incumbent) through this very harness.
- A noise band wider than the margin a bar is met by is **not a pass**: widen
  the span (`--duration`), quiet the box, or record the row as not yet
  judgeable.

## 6. What the harness does on its own (learned on the dev venue)

- It **copies** both binaries and their shims into its scratch before the
  first leg and records their sha256: a rebuild of the install prefix under a
  running row cannot change what is measured (it did, once).
- It **bounds** every instrument at `2 × span + 120 s`; an instrument that
  outlives the bound is sitting on FUSE requests the daemon never answered:
  the harness captures `.stats`, the daemon's and the instrument's kernel
  stacks beside the leg, kills the daemon (its own child), records the leg
  INVALID `daemon_wedged`, and remounts the same store for the next leg.
- A daemon that dies for any other reason mid-leg (a crash, an external
  kill) is recorded INVALID `daemon_alive`; an instrument that fails against
  a live mount (EAGAIN/EIO) is recorded INVALID `instrument_ok` with the
  daemon's `answered EAGAIN` count; a leg whose daemon ran Yellow/Red is
  INVALID `mem_budget_green` with the top R5 components in the detail.
- Teardown is by manifest plus a pid-tagged residue sweep (`nvmeof adopt` +
  `unshare` for anything the manifest missed); a stale FUSE mount under a dead
  daemon is lazy-detached.

## 7. Where the records go

`bench/brackets/<venue-id>/` in this repository; the evidence note of the
rung that produced them (`docs/evidence/rung-09-brackets.md` for the first
scoping rows; the box run gets its own note) cites each record by file name.
