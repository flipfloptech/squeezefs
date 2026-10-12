# Rung 10 — operations wiring: the daemon-owned fabric connect, `defrag --rekey`, the joiner's online volume (2026-10-12)

Three operations the charter names and the rung-8 notes owed (`rung-08-operations.md` §owed,
`rung-08a3-nvmet-fidelity.md` §Owed), each landed red-first in its own commit on `ops-wiring`
(worktree `.wt-ops`), rebased onto `dev` under the other agents' landings (`69bf78a` → `2e37fea`),
both gates green at the landing. Decision rows: 0002 rows 486–488; 0003 §O.1–§O.3.

| Item | Commit | Red first | Venue verification |
|---|---|---|---|
| 1. the daemon-owned nvmet connect at `mount`, `disconnect --identity-only` at the clean `umount` | `feat(nvmet): the daemon-owned data-plane connect…` | `nvmet fidelity: daemon-connect-loop` / `daemon-connect-tcp` (`src/ops/nvmet/fidelity.zig`) | §1 — `/sys/class/nvme` before == after on both transports |
| 2. `defrag --rekey` (key rotation as a defrag-class mover) | `feat(defrag): \`defrag --rekey\`…` | `rekey.zig`: the RAM sweep, the kill matrix at every `KillPoint`, the confirming census; `admin_frames` round trip + fuzz; `args` shape laws; `smoke: THE REKEY ROW` | §2 — the kill table; the live row over `lz4 + aes-256-gcm` |
| 3. a joiner learns an online `volume add-data` / `remove-data` | `feat(sym): a joiner learns an online…` | `shard.zig` (the grant carries the generation); `verbs.zig` (`resolve_set_config` + its fuzz); `smoke: THE ONLINE VOLUME ROW ACROSS DAEMONS` | §3 — B learns at its renewal, no remount |

## 1. The daemon-owned data-plane connect (charter §8; 0002 row 486; 0003 §O.1)

**Design.** A data volume whose set-config record carries a `fabric_endpoint` is the daemon's connect
(`src/ops/nvmet/mount_fabric.zig`, bound by `mount.loadSetConfig` BEFORE discovery): the plan resolves the
mount's host identity — the explicit `SQUEEZEFS_HOSTNQN/HOSTID` pair → `/etc/nvme/hostnqn` + `hostid` →
the machine-id derivation SALTED BY THE MOUNTPOINT (`resolveMountIdentityWith`:
`sha256("squeezefs:nvme-hostid:" ‖ machine_id ‖ NUL ‖ mountpoint)`), stable per mount slot (the
single-writer guard's identity stamp never reads a change under a reservation) and distinct across
co-located mounts (ownership = identity holds exactly) — then reuses a live controller of the subsystem
already riding that pair (a kill -9's leftover, logged loud) or connects and awaits the namespace, and
the record's uri becomes the live node for `align_probe` and the data superblock's identity law. Every
teardown (a FAILED volume's exit 1 included) runs `disconnect --identity-only` over the bound subsystems:
exactly the controllers under the mount's pair, each awaited gone; a foreign identity's association is
never touched. The ladder's other arms (META, a recordless data volume's `refuse_no_record`, the merged
shape) stay the reference's under the explicit pair alone. Counters `fabric_daemon_connects / reuses /
connect_failures / disconnects`, `fabric_ladder_refusals` under the nvmet family
(`counters.writeNamespaceEntries` lets the namespace and the plan share one object).

**Venue (nvmet-loop and nvmet-tcp targets, `zig build nvmet-smoke -Dnvmet-filter='nvmet fidelity:
daemon-connect'`, 53/53 of the tier green; the same legs in the gate).** The operator connects the META
volume only; the data volume has NO pre-connected namespace:

```
daemon-connect (loop): /sys/class/nvme before the mount:
nvme0:nqn.2026-10.io.squeezefs:fideli-2449228-dcmeta:nqn.2014-08.org.nvmexpress:uuid:a0a0…
during (the daemon's own association beside the operator's meta connect):
nvme0:nqn.2026-10.io.squeezefs:fideli-2449228-dcmeta:nqn.2014-08.org.nvmexpress:uuid:a0a0…
nvme1:nqn.2026-10.io.squeezefs:fideli-2449228-dcdata:nqn.2014-08.org.nvmexpress:uuid:a0a0…
after the umount:
nvme0:nqn.2026-10.io.squeezefs:fideli-2449228-dcmeta:nqn.2014-08.org.nvmexpress:uuid:a0a0…
fabric_daemon_connects 1 fabric_daemon_reuses 0 fabric_ladder_refusals 0
```

The tcp leg reads the same (`nvme1` the daemon's during the mount, gone after). The leg also pins:
without the record the ladder refuses naming `config set-fabric-endpoints`; a namespace connected under
ANOTHER identity survives the daemon's release.

## 2. `defrag --rekey` — key rotation as a defrag-class mover (charter §6.5; `ops_nvmet_keys.md` §7.4; 0002 row 487; 0003 §O.2)

**Design.** Never a cheaper rotation: the CLI derives the new key in the invoking process and installs
its key-dir file FIRST (`keys.linux_verbs.installMaterial`; an identical file is fine, a different one
under the same id refuses), then the admin lane's `admin_rekey` (0x0705, 48 B: the public `{salt,
key_id, throttle}` — no material crosses the lane) opens the window: `set_config.next_key`
(`{kdf_salt, key_id}` behind `FLAG_REKEY_WINDOW`, ONE tx, the generation bumped) lands durably BEFORE the
plane goes two-key (`transform.CryptoState` with an immutable `Wrap` carrying a `prev` arm: new writes
wrap under the next KEK, unwraps fall to the previous and count `crypto_key_unwraps_previous`; the
codec + scratch barriers prove no writer holds the retired wrap). The sweep is a `.rekey` job on the
fabric (the duty-cycle throttle, adopted on a remount): `RekeyBody` plans from the census and THE ONE
MOVER's new `rewrite` hook decodes every tenant frame of a block (`Windows.of(task)` from the refs) and
re-encodes it under the new wrap in RAM before the hash — the fresh block lands, the verify-read
compares, the map publishes, the old block enters pending-free, so the kill matrix at every
`mover.KillPoint` is the rotation's by construction. A pass with nothing rewritten quiesces the write
side (open packs sealed `.rekey`, in-flight tenants 0), classifies every ALLOCATED block the census did
not reach, and defers the close while any reads `previous` (`rekey_confirm_deferrals`); the close flips
`{salt, key_id}` in the tx that clears the window (+ fsync), then the plane retires the old wrap behind
the codec barrier — the old key is forgotten only after the LAST block's publish is durable. A mount
inside the window resolves the next key from the key dir and refuses by name without it. Gauges
`rekey_window_open`, `rekey_remaining_blocks`; counters `rekey_blocks_rewritten / bytes_rewritten /
blocks_skipped_current / passes / confirm_deferrals / windows_closed`; tripwires
`rekey_tenant_len_conflict`, `rekey_image_len_drift`.

**The kill-at-every-step table** (`rekey.zig` "the kill matrix": a RAM plane of 6 blocks under key A
and 8 bindings — one block clone-shared by three files, one block already under B as a write inside the
window; the mover killed at each `KillPoint` on the first task, the "remount" re-seeds the allocator
from the bindings, every binding read back byte-exact and classified, the resumed sweep converges):

| kill point | after the kill: bindings under B / under A | allocated blocks | the resume rewrote | window |
|---|---|---|---|---|
| `after_seal` | 1 / 7 | 6 | 5 | closed once |
| `after_pin` | 1 / 7 | 6 | 5 | closed once |
| `after_copy` | 1 / 7 | 6 | 5 | closed once |
| `after_mint` | 1 / 7 | 6 (the unpublished mint reclaimed) | 5 | closed once |
| `after_write` | 1 / 7 | 6 | 5 | closed once |
| `after_raise` | 1 / 7 | 6 | 5 | closed once |
| `mid_publish` | 3 / 5 | 7 (two of three refs moved; the source holds its last) | 4 | closed once |
| `after_publish` | 2 / 6 | 6 | 4 | closed once |
| `after_unpin` | 2 / 6 | 6 | 4 | closed once |

Every binding readable under exactly one of the two keys at every row; `skipped-current` 0 on every
resume (a crash never leaves a block the resume cannot place); the refcount reconciliation
(`checkReadable`: allocated ⇔ referenced, population = refcount) holds at every row.

**The live row** (`smoke: THE REKEY ROW`, 1 MiB blocks, `lz4 + aes-256-gcm`, a cache path; 40 packed
64 KiB files, three fsync'd 20 MiB incompressible striped files, an 8 MiB compressible one — the shape of
`scripts/bracket/rows/rand-4k-read-aes`):

```
smoke[rekey]: A → B: 72 block(s) rewritten (75792384 B), 2 pass(es), 0 confirming deferral(s);
  windows closed 1, window open 0, previous-key unwraps 0; tripwires len_drift 0 tenant_len_conflict 0
smoke[rekey]: B → C killed -9 mid-sweep → remount adopted it: 67 block(s) rewritten after the remount,
  windows closed 1, window open 0, previous-key unwraps 1 (the remount's unwrap cache is cold)
smoke[kill]: fsck --offline --scrub after the rekey row: 47 inode(s), 46 dentry ref(s), 76 block(s)
  checked, 76 refcount(s) checked → findings: 0 (clean)
```

A write during the A → B sweep lands under B and is skipped as current; every file byte-exact after
each rotation; a mount under A refuses by id, under B serves; the durable record (`config show` after
the clean umount, which reads the newest ledger record) names B then C with no next key;
`packed_mapping_refusals` 0 and `invariant_tripwires` 0 at the end. `previous-key unwraps 0` on the
A → B line is the blob-keyed unwrap cache the pre-rotation shas filled; after the kill the remount's
cache is cold and every block the sweep decoded under B shares ONE session wrap blob — one unwrap.

**A pre-existing defect the row found.** The byte-exact check of the 40 packed files against their source
read `packed-bad=40` before any rotation: `read_path.readMappingWindow` ran the window law under
`block_size` (a transformed tenant's block offset is a multiple of `chunk = block + 4 KiB`, so every
tenant past block 0 refused `MappingRefused`, EIO, and tripped `packed_mapping_refusals` — 80 fires for
40 files × two 32 KiB reads) and never decoded the frame. The earlier rows compared shas of the SAME read
path on both sides and passed with every tenant EIO'd (`sha256sum` skipped them identically). Fixed:
the volume's stride, a transformed tenant read as ONE frame decoded whole on the lane's codec, the
plaintext slice served; the unit test plants a framed tenant at block 3 (3 × chunk is not a
`block_size` multiple) on the transformed fixture.

## 3. A joiner learns an online `volume add-data` / `remove-data` (charter §8; 0002 row 488; 0003 §O.3)

**Design.** A joined daemon has no foreign read of tree 0, so the manager PUSHES and the joiner PULLS:
the membership grant's head carries the owner's `set_config` generation (`GrantHead.config_generation`,
read off the new lifecycle gauge `volume_config_generation` that the mount's read and every lifecycle
commit — `PlaneWorld.commit`, the rekey close — set; 8 B per renewal, the slot-offer carriage's push
mechanism), and a member whose word is behind pulls volume 0's record with the new manager verb
`ResolveSetConfig` (0x05B5; the bytes ≤ `SET_CONFIG_MAX = geometry.recordValueCap(1 MiB node)` in the
reply scratch; the record's own total codec decodes them; the reply codec has its round-trip fuzz) on
its beat thread and applies the diff: a volume it lacks is attached (`attachVolume`, router row
disabled), armed as the writer plane's next row (`WriterPlane.armVolume`; the plane is pre-sized at
`router.MAX_VOLUMES` like the holder's, so a `Vol` never moves under the allocator's seams) and set to
the record's state; a volume it has takes the record's state through the router's lattice, and
`draining` / `retired` ALSO return the joiner's unconsumed window to the holder (`returnWindowOf`: a run
left SET at the holder would hold the drain's "used 0" forever). The word moves to the record's
generation only when every volume applied; a partial apply retries at the next renewal
(`volume_config_pulls`, `volume_online_attached`, `volume_online_flips`, `volume_config_pull_failures`).

**The two-daemon row** (`smoke: THE ONLINE VOLUME ROW ACROSS DAEMONS`; `T_owner` 3 s ⇒ a 265 ms renew
cadence; `SQUEEZEFS_FSYNC_PROMOTE_STAGED=1` so each file places through the router at its fsync):

```
smoke[online-volume]: B learnt generation 2 at its renewal; d2 armed online as ordinal 2;
  8 file(s) on B put 2 block(s) on d2 (A's holder population 65); every sha from A equal
smoke[online-volume]: remove-data on A → B flipped d2 draining at generation 3 (its window returned;
  24 more files placed nothing there) and retired at 4; B's files read equal on both
```

B's log: `set config: vol … (…/d2.img) joined this mount ONLINE as ordinal 2 (active) at generation 2 —
the manager's `volume add-data` learnt at the renewal`; `data volume vol-…: active → draining`;
`draining → retired`. B's `.stats` at the bump: `data_volumes 3`, `block_grant_writer_volumes 3`,
`volume_online_attached 1`, `volume_config_pulls 1`, `volume_config_pull_failures 0`; A's holder row on d2
shows `writers 2`. Closure (`expectMustStayZero`) on both; clean umounts. Run 4× on the venue green.

**Owed (stated, not hidden).** (a) The mover's census under the symmetric plane: the manager's drain
plans from its OWN slot trees (`forEachLayout` ranges `TREE_XATTRS` of this mount's view), so blocks a
JOINER's files reference on a draining volume are never planned and the drain re-plans forever
("census empty but the victim still holds N"). The row removes B's files on d2 before `remove-data`
(the frees ship to A at B's cadence) and the retire follows; the cross-owner census (the holders' maps
through tokens, or a joiner-side mover under the manager's plan) is the next lifecycle item. (b) A
volume added online with a `fabric_endpoint` record is not connected by a joiner online (logged loud,
its word stays behind): the daemon-owned connect is the mount's act under the mount's identity and its
plan's lifetime is the mount's.

## Gates at the landing (the three commits rebased onto `dev` `2e37fea`)

- macOS `zig build gate`: 28/28 steps, 2104/2130 tests (26 skipped; the Linux-only modules skip by
  class) — unit 1980 pass / 26 skip, rails 16/16; 7m12.
- venue `zig build gate --summary all` (Ubuntu 24.04, kernel 7.2.6-sqz, 9 vCPU): 46/46 steps,
  2645/2698 tests (53 skipped) — unit 2424 pass / 51 skip, rails 16/16, shim-census 1/1, smoke 70 pass /
  2 skip (the REKEY row and the ONLINE VOLUME row among them), bench-smoke 41/41, shim-smoke 40/40,
  nvmet-smoke 53/53 (the two `daemon-connect` legs among them); 13m09.
