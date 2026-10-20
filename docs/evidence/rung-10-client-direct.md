# Rung 10 — the client bypass's DIRECT ARM: a trusted client reads the data device itself under a layout lease (2026-10-20)

Wave 2 of `rung-10-client-bypass-rows.md`, whose verdict was a structural cap: the ring's op is
client → service thread → the daemon's own `io_uring_enter` → the CQE → `futex_wake` → client, two
thread hops and an arena copy around the one syscall an in-kernel filesystem pays; 39–54 % of ext4
on 4k random I/O. The owner's bar — fastest or on par on every served row, in memory — is reachable
only by the client issuing the device I/O. This is the step beyond DAOS's libioil (whose engine does
the I/O on dedicated cores): charter §7 "The direct arm", decision 0003 §AX.

**The design.** `-o interception=direct` is the mount owner's opt-in. At HELLO the daemon grants
the arm iff the peer's uid/gid could open EVERY data device node for reading (`rendezvous.
deviceReadable`: root, or the read bit that is theirs; `SessionOk.features & FEATURE_DIRECT`; a
refusal is counted, said once, and leaves a ring session); the client opens the nodes itself
(`O_RDONLY | O_DIRECT` — the kernel's permission check is the final word) and maps the **lease-epoch
page**: a sealed memfd of one monotonic word per inode stripe (`data/lease_epoch.zig`, the shape
table's stripes and mix). Per bound fd the client asks a **layout lease** — `{ino, size, epoch,
block → (volume ordinal, device offset)}` of a resident striped passthrough file with no write-side
custody, the epoch read BEFORE the map (`moved` refuses if it changed after) — and a read of one
aligned, mapped, in-size block is ONE `pread` on the device from the caller's thread, then the
**recheck with the bytes in hand**: the stripe still reads the lease's epoch, else the lease is
dropped and asked again, past two attempts the ring serves. The daemon bumps the epoch (a) at every
conveyor member's terminal outcome for every inode whose record or xattrs it changed — after the RAM
apply, before the ack, every origin (the FUSE direct leg's "incarnation still ∧ the map still binds",
charter §6.3, made cheap enough for a client) — and (b) at every write's admission (RAM custody the
device does not hold yet). Holes, tenants, inline/staged files, transformed volumes, multi-block and
unaligned requests, and every write stay on the ring; a refused binding backs off 200 ms. A dead
daemon cannot move the epochs its leases rest on, so the heartbeat ladder runs on the direct path and
poisons the session before any read. **The trust granted is whole-device** — the trust a FUSE mount's
root already holds — and is for the owner's own HPC clients; the default `-o interception` grants
nothing of it; device fds are never passed over `SCM_RIGHTS` (the client's own `open` is the check).

**Red first** (`src/ipc`, `src/shim`, `src/sdk`): the device screen's rows (root always, the owner /
group / other bit that is theirs, never another's); the lease epoch table (a bump moves its stripe
alone; a shared page inherits the private counts; unshare folds back); the frames' round-trip,
refusal classes and fuzz rows (the census names `LeaseOk.decode` and the four fixed frames);
`-o interception=direct` / `=ring` refused; the e2e row: the grant (page sealed and mapped, devices
open), a direct read byte-exact with `ipc_ops_read` flat and `il_direct_ops` +1, block 1 under the
same lease and an EOF past it, **a truncate through the kernel between the lease and the read** —
the stale lease served nothing, the recheck refused, the lease re-asked, the read answered the new
EOF — a kernel write's admission invalidating the lease (re-asked, the new bytes read), an
inline-born file refused (`kind`) and ring-served with the backoff, **kill -9 of the daemon ⇒ the
next direct read falls through** (never a serve under a lease nobody can invalidate). The uid-refusal
arm is the pure rule's rows (an in-process e2e runs as root). Counts: units 547 (ipc/lease/shim/sdk/
mount_options/op_guard/conveyor), rails 29 with the census 62/62, `sdk-smoke` both links,
`shim-smoke` 40, smoke 565 incl. both interception e2e rows; `fmt` clean.

**Measured** (the same substrate, shapes and rules as wave 1 — psync, 20 s rows, Tctl ≤ 70 °C at
start; dist `629836e`; rows under `/build/perf/mx/ram-sqz-il-direct-rfinal{,2}`; every direct row
100 % engaged — `direct` ops ≡ fio ops, 32 leases granted, 0 rechecks refused, FUSE flat):

| row (psync; 32 jobs unless QD1) | ext4 | xfs | FUSE | bypass (ring) | **direct** | direct ÷ ext4 | Tctl |
|---|---|---|---|---|---|---|---|
| 4k randread QD1 (IOPS, p50 µs) | 268,178 (3.8) | 236,330 (4.0) | 115,878 (8.2) | 144,230 (7.0) | **257,903** (4.0) | 96 % | 55→74 |
| 4k randread ×32 | 3,053,210 (8.8) | 2,847,022 (9.4) | 1,188,485 (20.9) | 1,224,870 (23.2) | **3,079,987** (8.6) | 101 % | 50→81 |
| 64k randread ×32 (MiB/s) | 79,223 | 78,724 | 43,400 | 36,902 | **80,415** | 102 % | 55→85 |
| 1 MiB seq read ×32 | 98,338 | 97,153 | 88,571 | 88,406 † | **97,994** | 100 % | 69→81 |
| 4k randwrite QD1 | 217,747 (4.6) | 215,840 (4.0) | 84,502 (11.1) | 110,633 (8.2) | 111,548 (ring) | 51 % | 56→56 |
| 4k randwrite ×32 | 3,027,972 (9.0) | 2,791,974 (9.7) | 789,400 (33.0) | 1,168,926 (24.7) | 1,168,013 (ring) | 39 % | 56→72 |
| 64k randwrite ×32 | 76,207 | 76,203 | 30,076 | 31,201 | 33,011 (ring) | 43 % | 60→75 |
| 1 MiB seq write ×32 | 85,076 | 84,865 | 53,165 | 51,287 † | 55,634 † | 65 % | 60→76 |

† kernel-routed by the lane gate (the ring's copies); the direct arm is asked BEFORE the gate
(`629836e`), which is what moved the 1 MiB read from 88 to 98 GB/s. CPU per 4k read op at ×32:
ext4 7.8 µs, direct 7.9 µs (24.1 host cores ÷ 3.08 M; the daemon 0.04 cores — it does nothing per
op), the ring 18.4. The non-direct bypass re-measured at this build: 136 k / 1.26 M / 1.11 M on the
4k rows — wave 1's levers hold (control arm, no regression).

**Verdict.** On every row the direct arm serves — all four read shapes — SqueezeFS is **on par with
or ahead of ext4 and xfs** (96 % of ext4 at QD1, 100–102 % at ×32 and on bandwidth; above xfs on
every one), at ext4's own CPU per op. Writes are not direct in this wave and stand where wave 1 left
them (39–51 % of ext4 on 4k, 43 % at 64k; ahead of FUSE); the binder is unchanged: the ring hop. What
the write arm needs (decision 0003 §AX.5): the W1 patch of a resident block within size as a client
`pwrite` on the device, a WRITE lease under which the daemon's tiers (hot, hold, NVMe cache) are
purged and refuse admission for the inode so a kernel reader goes device-true (the per-inode switch
is the data plane's), the postlude — the parked stamp, the size claim, the KD-11 notify — as a
batched note the daemon applies within the flush interval (the law charter §L.6 already gives a
write's stamp), and the two kill rows (a client killed mid-write acked nothing; a daemon killed with
client writes in flight leaves the device holding only acked bytes and the client fails loud by the
heartbeat). Nothing in it changes the lease mechanism; everything in it touches the read path's tier
admission, which is why it is the next wave and not this one.

**Not verified; risks.** The uid-refusal arm end to end (needs a non-root client); a transformed
(compressed/encrypted) volume refuses `kind` by design but was not run; real NVMe (only brd through
nvme-loop here — the device's own 4 KiB sector alignment is the `GRAIN` the arm requires); files past
≈ 16 GiB lease their first 4093 blocks and the rest ride the ring; a mixed read/write workload on one
file re-asks its lease per write (the admission bump — owed: bump only when custody is taken); the
security posture is the owner's: a direct client reads any block of the volume, so `interception=direct`
belongs only on mounts whose device permissions already name the clients it trusts.
