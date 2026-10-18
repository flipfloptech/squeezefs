# rung 10 — fabric multipath: several paths per volume, one head, both links carrying bytes

**What changed.** A data volume's `fabric_endpoint` record was ONE `trtype:traddr:trsvcid:subnqn`
(`set_config` version 1); the daemon made one association per volume. Now the record is a LIST
of paths of one subsystem (`FabricEndpoint = {subnqn, paths[1..=16]}`; `set_config` version 2:
`FabricEndpointWire` head + `FabricPathWire` × n + the NQN; a version 1 image decodes as a list
of one and re-encodes byte-exactly as version 2 — the fixture test). The CLI takes
`vol-<tag>=tcp:A:4420:nqn,tcp:B:4420:nqn` (one subnqn on every path, else refused naming both;
a path twice refused). At mount the daemon connects EVERY path under its one identity (a
controller already riding that identity on the path's address is reused), the kernel's native
multipath merges them into one head, and the head is handed to discovery only when its serving
set is EXACTLY ours (`multipath.judgeServing`, pure; fixture-tested). A path that fails beside
a serving sibling is a loud warning + `fabric_paths_degraded`; zero paths up refuses as before.
The kernel's `iopolicy` default `numa` sends every I/O of a NUMA node down one path, so the
mount writes `SQUEEZEFS_FABRIC_IOPOLICY` (default `round-robin` — the ruling below) to every
owned subsystem and reads it back (`fabric_iopolicy_sets` / `fabric_iopolicy_failures`, never
a refusal); `fabric_paths_live` sums the live serving controllers. `nvmeof status
[--json]` lists every fabric subsystem's head, policy and paths. Decision 0002 row 543.

**The metadata volume (owner extension).** Its node is the mount's argument, so the operator
connects its paths (`squeezefs nvmeof connect` twice under one pair). `mount_fabric.Shape.of`
now resolves the node to its head's subsystem dir and judges rule 2 PER PATH: every serving
controller carries the configured pair; another identity among them is the merged shape
(`refuse_merged`); the head's policy is set and read back, its paths count in
`fabric_paths_live`, `.stats` lists them (`fabric_meta_bound`). Before this, `Shape.of` read
the controller `nvme<N>` for a head `/dev/nvme<N>n<Y>` — on a multipath kernel the head's N
is the SUBSYSTEM instance, so it read a coincidental controller (right in every earlier leg
by numbering luck; the new leg's data head `nvme3n1` sits under `nvme-subsys3` with
controllers `nvme3`/`nvme4` while `nvme1`/`nvme2` serve the metadata head).

**Path-loss clocks.** Every daemon connect and every `nvmeof connect` now carries
`fast_io_fail_tmo` = the claim's staleness TTL (45 s), `ctrl_loss_tmo` = 2 × TTL (90 s),
`reconnect_delay` = the beat (10 s) — `multipath.deriveConnectTimeouts`, tie-tested; three
`SQUEEZEFS_FABRIC_*_S` rows override a field. A single path's loss fails over at once (the
path's requests are cancelled and re-queued on the head); with EVERY path down the head holds
I/O until fail-fast expires and then fails it — the journal/data fail-stop — instead of the
kernel's 600 s. Operators connecting metadata paths with nvme-cli pass the same:
`--fast-io-fail-tmo 45 --ctrl-loss-tmo 90 --reconnect-delay 10`.

**The red leg** (`nvmet fidelity: multipath-tcp`, 7.2.8-7-omarchy-sqz, `nvme_core.multipath=Y`,
RAM-backed images under `/build/tmp`, both shares with listeners on 127.0.0.1 and 127.0.0.2).
Before the change the mount made one association and `.stats` had no path rows. Now: metadata
head connected by hand on both paths (`fabric_meta_bound` lists both live, policy read back);
data record = both live paths + a dead 127.0.0.3 → `fabric_daemon_connects 2`,
`fabric_paths_degraded 1`, `fabric_paths_live 4`; ONE head (`nvme-subsys3`, `nvme3n1`; both
hidden path nodes `nvme3c3n1`, `nvme3c4n1`); under the default both paths carry the parallel
write; kill -9 → remount under an explicit `queue-depth` `fabric_daemon_reuses 2`,
`fabric_daemon_connects 0`, both subsystems read the knob back; umount releases both
(`2 controller(s) … released`); a mount under the default again reads `round-robin` back and
both paths carry the parallel cold read; the metadata paths stand until the operator's one
`--identity-only` disconnect releases both; zero residue.

**Per-path sectors** (`/sys/class/nvme/nvme<N>/nvme<K>c<N>n1/stat`, three runs agree within
5 %):

| policy | workload | path A | path B |
|---|---|---|---|
| round-robin (default) | 4 × 64 MiB files written in parallel + `sync -f` | 254160 | 270128 |
| round-robin (default) | the same files read cold by 4 parallel readers | 262144 | 262144 |
| round-robin | the same files read cold by one serial reader | 265552 | 266776 |
| queue-depth | the same files read cold by one serial reader | 487920 | 44408 |
| queue-depth | the same files read cold by 4 parallel readers | 296960 | 227328 |
| queue-depth | 4 × 64 MiB written in parallel (by hand, `exp4.sh`) | 409600 | 114688 |
| queue-depth | ONE serial `dd` writer, 64 MiB O_DIRECT on the raw head | 131072 | 0 |

Round-robin alternates per bio, so every stream — serial or parallel — splits within 5 %.
Queue-depth picks the path with the fewest requests in flight and ties go to the first path,
so a lone serial stream stays 100 % on one link, a serial reader 92/8, four parallel writers 78/22 (the
first leg draft asserted both paths under queue-depth after a serial-ish write-back and read
524288/0 once). The leg asserts BOTH paths under the default for the write and the 4-reader
phases, and measures queue-depth under the explicit knob (asserting only that the reads
reached the device, ≥ 524288 sectors).

**Ruling (owner, 2026-10-18).** The `SQUEEZEFS_FABRIC_IOPOLICY` default is `round-robin`: the
fabric this is for has two 200 GbE links and the goal is aggregate bandwidth; queue-depth
leaves a lone sequential stream entirely on one link (131072 / 0) and skews four parallel
writers 78/22, while round-robin balances every workload in the table within 5 %. `numa` (the
kernel's default) pins a single-socket client to one link. `queue-depth` stays selectable for
a fabric whose paths differ in latency; the knob flips it per mount.

**Follow-up (not in this change).** A `sqmeta://` fabric URI form (or a mount option) carrying
the metadata volume's paths on the command line, so the daemon owns the metadata connects
under the mount's identity as well — the same fan-out code, no record needed.
