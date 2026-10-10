# PORTING SPEC — `ops_nvmet_keys` (kernel `nvmet` target management, the fabrics initiator, the REAL PR legs, the fidelity tier, `storage`, key handling — rung 8)

Charter: §8 "NVMe-oF targets" (charter.md L257: nvmet via configfs is THE target; the
write-ahead-intent ledger `pending/active/removing`, atomic renamed writes, forward-only,
reconcile against live state, never trust configfs bookkeeping; one namespace per subsystem;
`resv_enable=1` before enable; `device_uuid` recorded once; port ids from a reserved range by
hash + probe, ownership = ledger membership; `restore` idempotent; `adopt`; the register ladder;
"nvmet has no PTPL — the heartbeat re-check heals"; the fidelity tier with zero mocks), the CLI
row (L250: `nvmeof {share,unshare,list,restore,adopt,target *,connect,disconnect}`, `storage`),
§3 (L68: `nvme_core.fabrics_host_scoped_subsystems`), §5.5 "Fencing" (L176), §5.7 B1 (L193),
§6.5 (L238: the key wrap v2, the resolution order, the key-file laws), §11 (L285: the nvmeof
fidelity tier is a release gate). Rung 8 of §13 (L312).

Reference files (all under `/Users/joberdorf/Source/squeezefs`; `D:` =
`docs/design-nvmeof-target-management.md` (684 lines, rev 5 + the R-SYM-8 banner), `DK:` =
`docs/design-key-handling.md` (146), `DMP:` = `docs/design-mw-multipath-kernel.md` (383),
`N:` = `src/nvmeof/nvmet.rs` (1,219), `L:` = `src/nvmeof/ledger.rs` (488), `S:` =
`src/nvmeof/stack.rs` (359), `M:` = `src/nvmeof/mod.rs` (1,465), `I:` =
`src/nvmeof/initiator.rs` (1,044), `F:` = `src/nvmeof/fabric.rs` (458), `NC:` =
`src/nvmeof/nocow.rs` (226), `R:` = `src/meta_backend/reservation.rs` (1,576), `K:` =
`src/keyfile.rs` (548), `MAIN:` = `src/main.rs`, `ST:` = `src/storage.rs` (832), `LIB:` =
`src/lib.rs`, `EK:` = `src/env_knobs.rs`, `FID:` = `tests/run_nvmeof_fidelity.sh` (2,018),
`SUB:` = `tests/nvmeof_target_substrate.sh` (498), `GS:` = `tests/guard_smoke.sh` (322),
`TN:` = `tests/nvmeof_nvmet_stack_tests.rs`, `TL:` = `tests/nvmeof_ledger_tests.rs`, `TP:` =
`tests/nvmeof_port_alloc_tests.rs`, `TA:` = `tests/nvmeof_adopt_tests.rs`, `TI:` =
`tests/nvmeof_initiator_tests.rs`, `TF:` = `tests/nvmeof_fabric_stats_tests.rs`, `TG:` =
`tests/nvmeof_grammar_tests.rs`, `TK:` = `tests/encrypt_key_handling_tests.rs`, `TW:` =
`tests/wero_rtype_tests.rs`, `TMW:` = `tests/mount_writer_guard_tests.rs`).

Port files that already exist (read-only here): `src/meta/guard/pr.zig` (the PR client over
`nvme_cmd.Namespace`: register L180-190, ladder L202-260, `wireHostId` L300-315, two-step
report L320-353, `IdentityStamp` L483-494), `src/meta/guard/pr_linux.zig` (sysfs `nsid` →
`/dev/ngXnY` through `/sys/class/nvme-generic`, `IORING_OP_URING_CMD` on a `Ring128`, L96-189;
its real-device test L222-250 SKIPS "rung 8 brings nvmet-loop"), `src/meta/guard/nvme_cmd.zig`
(the byte-exact codec: opcodes L105-110, FID 0x81 L120, rtypes L127-128, RREGA/RACQA/RRELA
L131-147, report codec L280-360), `src/meta/guard/fake_nvme.zig` (`RegisterSemantics
{spec_strict, lenient_replace}` L20), `src/meta/guard/gate.zig` (the heartbeat PR re-check
L502-532: `.reacquired` counts `pr_reacquires`), `src/meta/guard/proc.zig` (`EtcNvme.read`,
read-only, L158-183), `src/data/transform.zig` (wrap v2 L703-767; `KeySource`, `KeyFileError`,
`readKeyFile`/`readKeyStdin`/`resolveKeyMaterial`/`resolveVolumeKey`/`hardenProcess`
L774-958), `src/data/data_superblock.zig` (`device_uuid` at offset 56, L66-86),
`src/data/set_config.zig` (the device list with `device_uuid`, L94/149), `src/cli/mount.zig`
(the key resolution with `flag_path = null`, L655-669), `src/cli/format.zig` (the format-time
key read, L191-201), `src/cli/daemon.zig` (the double fork + ready pipe, L152-244),
`src/core/knobs.zig` (`SQUEEZEFS_ENCRYPT_KEY_FILE` L225, `SQUEEZEFS_HOSTNQN/HOSTID` L266-267).

## 0. Venue facts this spec is written against (probed 2026-10-09, `scripts/venue`)

Kernel `7.2.6-sqz`; `CONFIG_NVME_TARGET=y`, `CONFIG_NVME_TARGET_LOOP=y`,
`CONFIG_NVME_TARGET_TCP=y`, `CONFIG_NVME_TCP=y`, `CONFIG_CONFIGFS_FS=y`, `CONFIG_BLK_DEV_LOOP=y`;
**`CONFIG_NVME_MULTIPATH is not set`** (per-controller namespace nodes `nvme<X>n<Y>`; no head
nodes, no `nvme_core.multipath` parameter); `/sys/class/{nvme,nvme-fabrics,nvme-generic,
nvme-subsystem}` present (the `nvme-subsystem` class exists without multipath — it just carries
no namespace children); `/sys/kernel/config` exists and is EMPTY (configfs not mounted);
`/sys/module/nvme_core/parameters/fabrics_host_scoped_subsystems` reads `N` (the sqz patch is
present, off by default — DMP:88-91); `/sys/module/{nvme_core,nvme_tcp,nvmet_tcp}` exist and
**`/sys/module/nvmet` does NOT** (a built-in module with no parameters gets no `/sys/module`
dir — so the reference's `modules_present = /sys/module/nvmet exists` (N:1211) reads FALSE on a
kernel where nvmet is built in; §1.6 below judges readiness by the configfs `nvmet` subsystem
dir instead); the container runs as uid 0; `losetup` is present; **no `nvme`, `modprobe`,
`ip`, `jq`** (every shell-out of the reference — N:330 `modprobe`, N:344 `mount`, N:611-620
`losetup`, I:880-891 nvme-cli — is unavailable; the port drives configfs and `/dev/nvme-fabrics`
directly, which charter §1.2/§12 wants anyway).

Kernel facts the port relies on (stated with their source so a later kernel can be re-checked):
`drivers/nvme/host/fabrics.c` `nvmf_dev_write` creates the controller from the option string
and **`nvmf_dev_read` on the same fd returns `instance=%d,cntlid=%d\n`** — the one exact way
to learn WHICH controller a connect created; `nvmf_create_ctrl` `request_module("nvme-%s")`
autoloads a host transport. `drivers/nvme/target/core.c` `nvmet_ns_enable` tries the bdev
backend and on `-ENOTBLK` falls to the FILE backend (`io-cmd-file.c`, `O_DIRECT` unless
`buffered_io=1`) — a regular file is served WITHOUT a loop device; `nvmet_enable_port` runs
`request_module("nvmet-transport-%d")` when a port's trtype has no transport, and a port is
enabled by its FIRST subsystem link (`nvmet_port_subsys_allow_link`), after which every
`addr_*` store refuses (`-EACCES`). `drivers/nvme/target/configfs.c`: `device_path`,
`device_uuid`, `device_nguid` stores refuse `-EBUSY` while `enable=1` and `resv_enable` refuses
`-EINVAL` (`nvmet_ns_resv_enable_store`; measured 2026-10-09 — the port's `AttrLocked` folds
both, decision 0002 row 391);
`nvmet_ports_make` parses the port name with `kstrtou16` — **port ids are u16**;
`attr_allow_any_host` is 0 on a fresh subsystem. `drivers/nvme/target/pr.c` (≥ 6.13):
reservations live in nvmet, independent of the backing (file-backed namespaces enforce PR
too); RESCAP advertises every rtype and NO PTPL bit; a same-host different-key REGISTER
conflicts (the "spec-strict" measurement, R:1095-1098, GS:198-207); UNREGISTER is validated
against the issuing host's own registration (`crkey`); Get Features FID 0x81 requires
`EXHID=1` (16 B) else Invalid Field (R:1058-1062). `drivers/nvme/target/loop.c` registers the
`loop` transport on BOTH sides: a port with `addr_trtype=loop` needs no address; the host
connect `transport=loop,nqn=…[,traddr=…]` picks the first loop port when `traddr` is absent
(`nvme_loop_find_port`). Deleting a subsystem under a LIVE tcp association makes the host
controller reconnect (`reconnect_delay` 10 s, `ctrl_loss_tmo` 600 s — D:510 R3); under a LIVE
loop association the host controller is DELETED (loop has no reconnect).

## Rung map

| Mechanism | Rung | Where below |
|---|---|---|
| configfs recipe, the share ledger, port-id allocator, `share/unshare/list/restore/adopt/target *` | **8** | §1, §2 |
| The initiator: `/dev/nvme-fabrics` connect + read-back, identity, discovery, disconnect, `fabric_*` | **8** | §3 |
| The REAL PR legs on `/dev/ng*` against nvmet-loop and nvmet-tcp (the deferred row of `guard_claims.md` §5.3) | **8** | §4 |
| The fidelity tier (`zig build fidelity`, root, Linux) — a release gate (charter §11) | **8**, run per PR touching `src/ops/nvmeof/*`, `src/meta/guard/*`; nightly full | §5 |
| `storage` verb | **8** (reshaped, §6) | §6 |
| Key handling: `mount --encrypt-key`, the pre-fork stdin read, the key store verbs | **8** | §7 |
| `fabric_endpoint:` records + daemon-owned data-plane connects under an explicit identity | **8** (the record), the ladder's merged-shape arm stays fixture-tested | §3.7 |
| Key ROTATION (a re-wrap mover) | not in the reference; **owed later** (§7.4) | §7.4 |

---

## 1. The configfs recipe (byte-exact)

### 1.1 The tree the product writes (`N:62` `NVMET_CONFIGFS_ROOT = /sys/kernel/config/nvmet`)

```
/sys/kernel/config/nvmet/
├── hosts/<hostnqn>/                       # one dir per allow-listed host (N:666-667)
├── subsystems/<subnqn>/                   # mkdir (N:660); the NQN is a configfs path component
│   ├── attr_allow_any_host                # "1" | "0"  (N:661-664)
│   ├── allowed_hosts/<hostnqn> -> ../../../hosts/<hostnqn>   # symlink iff allow-listed (N:672)
│   └── namespaces/1/                      # the index is STRUCTURALLY 1 (N:683; D:359)
│       ├── device_path                    # the backing (N:685)
│       ├── device_uuid                    # the recorded ns_uuid, 36-char text (N:687)
│       ├── resv_enable                    # "1" BEFORE enable (N:688-689)
│       └── enable                         # "1" LAST (N:698)
└── ports/<id>/                            # id from the reserved range (N:599)
    ├── addr_trtype                        # "tcp" | "loop"   (N:600 pins tcp; §1.4 adds loop)
    ├── addr_adrfam                        # "ipv4" | "ipv6"  (N:601; `adrfam_of` N:96-105)
    ├── addr_traddr                        # the listener IP  (N:602)
    ├── addr_trsvcid                       # the TCP port as text (N:603)
    └── subsystems/<subnqn> -> ../../../subsystems/<subnqn>   # the link that ENABLES the port (N:716)
```

Other kernel attributes exist (`attr_serial`, `attr_model`, `attr_version`, `attr_cntlid_min/
max`, `attr_qid_max`, `attr_pi_enable`; namespace `device_nguid`, `ana_grpid`, `buffered_io`,
`revalidate_size`; port `addr_treq`, `addr_tsas`, `param_inline_data_size`,
`param_max_queue_size`, `ana_groups/`, `referrals/`) — the product writes NONE of them and
reads only the ones named here (walk: N:386-391 ports, N:539-542 namespace). Law 5 (D:295):
"nothing is ever written into configfs except real kernel attributes" — the pre-rebuild
`associated_loop_device` fake file (D:60) is the defect this fixes.

### 1.2 Create order — `apply_share` (N:628-724; the restore replay body too)

1. **Precondition**: the pending ledger record exists (§2.3) — `begin_share` BEFORE the first
   mutation (N:1111-1113). Every write below is checked (`write_attr` N:161-172,
   `create_dir_checked` N:174-181); a failure part-way returns with the intent record claiming
   whatever exists (N:1124-1133).
2. `mkdir subsystems/<subnqn>` (N:660). `attr_allow_any_host ← "1"` when no allow-list, else
   `"0"` + per host: `mkdir hosts/<host>`, `symlink(hosts/<host>, subsystems/<subnqn>/
   allowed_hosts/<host>)` (N:661-680; `allowed_hosts/` is a default group — `create_dir_all`
   on it is a no-op on a real kernel). WHY first: `attr_allow_any_host` is 0 on a fresh
   subsystem and the first initiator connect must find the final posture.
3. `mkdir subsystems/<subnqn>/namespaces/1` (N:684).
4. `device_path ← <backing>` (N:685) — a block node or a REGULAR FILE (§0: nvmet's file backend;
   the reference's `losetup` arm N:640-657 is quirk Q3 below).
5. `device_uuid ← <ns_uuid>` (N:687) — the recorded identity, re-presented verbatim by restore
   (D:359, D:362: a *changed* identity across a restore is exactly what the initiator's namespace
   revalidation trips on; duplicate/absent NGUID breaks connects on RAM-backed devices).
6. `resv_enable ← "1"` (N:688-689) iff the attribute exists; absent ⇒ the share proceeds with
   a LOUD note "serves WITHOUT NVMe Persistent Reservations; the single-writer mount guard
   lands detection-grade" (N:690-697). Steps 5–6 MUST precede 7: the kernel refuses them with
   `-EBUSY` once enabled (§0).
7. `enable ← "1"` (N:698).
8. Per listener: the port object (§1.4) then `symlink(subsystems/<subnqn>, ports/<id>/
   subsystems/<subnqn>)` (N:711-722) — the link is what ENABLES the port (§0), so an
   unbindable address (`EADDRNOTAVAIL`) fails HERE, after the namespace is live (the fidelity
   crash-window leg rides exactly that: FID:463-483).
9. `finalize_share` (`pending → active`, N:1117-1121).

### 1.3 Disable / remove order — `teardown` (N:732-855; `unshare` and the `removing`/GC arms)

1. **Precondition**: `mark_removing` BEFORE the first teardown write (N:1137-1142).
2. Every `ports/*/subsystems/<subnqn>` symlink unlinked (N:740-753) — the reference walks ALL
   ports for N1-era records whose ids were untracked; the port walks only the RECORDED ids
   (every port record carries them from birth) plus, for an ADOPTED record, the ids captured
   at adoption.
3. `namespaces/1/enable ← "0"` when the attr exists, then `rmdir namespaces/1` (N:756-762).
4. `allowed_hosts/*` links unlinked (N:765-779).
5. `rmdir subsystems/<subnqn>` (N:782-790; a vanished subsystem is a verified no-op with a
   note). On a real kernel `rmdir` succeeds once child OBJECTS are gone; the reference's
   `remove_dir_all` fallback (N:190-212) exists for its injected-root unit tier — the port's
   fake configfs is an in-process tree, so the fallback does not exist (quirk Q9).
6. Recorded port ids, `rmdir ports/<id>` iff `ports/<id>/subsystems/` is EMPTY (N:794-815) —
   last-out removal; a port carrying another share's link survives.
7. `hosts/<host>` removed iff no OTHER ledger record names the host (N:821-841); an `EBUSY`
   (a foreign reference) is logged, never fatal.
8. `delete` the ledger record (N:1143-1144).

### 1.4 Ports: tcp vs loop; the reserved id range; hash + probe

**tcp port** (N:599-603): `mkdir ports/<id>`; `addr_trtype ← "tcp"`, `addr_adrfam ← "ipv4"|
"ipv6"` (from `IpAddr` parse, N:96-105 — malformed refuses before root), `addr_traddr ← <ip>`,
`addr_trsvcid ← <port decimal>`. Reuse law: an existing `ports/<id>` is OURS-AND-MATCHING iff
`(trtype, traddr, trsvcid, adrfam)` match exactly (N:107-112) AND every link under it names a
LEDGERED nqn (N:138-143, N:577-590: "ownership = ledger membership"); otherwise FOREIGN — skip,
never touch. The scan→create race closes at `ensure_port` with a loud refusal naming the attrs
(N:591-597).

**loop port** (the port's addition; the reference pins `addr_trtype=tcp` — D:667 residual 1):
`mkdir ports/<id>`; `addr_trtype ← "loop"`; no address attributes (`nvmet_loop_add_port` ignores
them; the kernel's `addr_adrfam` default `loop` suffices). ONE loop port per host serves every
loop share (links are per subsystem); its probe key is the constant `"loop"`. The initiator
side is `transport=loop,nqn=<subnqn>` (§3.1). `share --transport loop` needs no `--ip`;
`--transport tcp` (default) requires ≥ 1 `--ip`.

**The reserved range** (N:52-58; D:358): `[base, base + 999]`, `base` default **53000**
(`SQUEEZEFS_NVMET_PORT_ID_BASE`), disjoint from the dev-substrate's 52026 and the scoping rig's
52470/52471 (D:358); the fidelity tier relocates to **54000–54099** (SUB:85, SUB:33-37). WHY a
range at all: "ports carry no name" (D:358) — identity rides the id. Width 1000 and hash slots
900: "the hash spreads over 900 slots; the last 100 are pure probe headroom" (D:358, N:57-59).
First candidate `base + (hash("tcp:{ip}:{port}") % 900)` (N:127-128), then a LINEAR PROBE
wrapping through the FULL `[base, base+999]` (N:133-146); every id foreign/incompatible ⇒ refuse
loud naming the knob (N:148-153). The reference hashes with FNV-1a 64 (N:67-74, frozen vectors
TP:49-50: `fnv1a_64("tcp:10.0.0.1:4420") = 0x8a9079e409b21076`, `"tcp:127.0.0.1:4420" =
0xef9af88e36f06311`; first candidates `10.0.0.1:4420 → base+382`, `127.0.0.1:4420 → base+773`,
`127.0.0.1:4421 → base+862`, `10.10.10.50:4420 → base+804`, TP:53-58). The port hashes with
**xxh3-64** (`core/checksum.zig` — one hash in the tree, quirk Q5) over the key
`"{trtype}:{ip}:{port}"` (`"loop"` for loop) and freezes ITS OWN vectors in a tie test; the
reference's ids are not carried (decision 0002 §2: no compatibility obligation). Port ids are
**u16** (§0 `kstrtou16`): `base ∈ [1, 65535 − 999]`, the knob's range (quirk Q6: the
reference admits `0..u32::MAX`, EK:391).

Share-time allocation is a PURE function over a configfs SNAPSHOT (`allocate_port_id`
N:118-154 over `PortSnapshotEntry` N:78-86 + an `is_ours` predicate): the ids are allocated
BEFORE the pending record is written so the intent claims them (N:1070-1092; N:1071-1073, D:358 "the
teardown law needs all of them, even across a crash window"); one id per `(ip, port)` listener
(D:358 "one configfs port object per listener"); a multi-listener share reuses one id for two
links only when the listener is the SAME `(ip, port)` (the fidelity round-trip: two subsystems,
one port 54065, two links — FID:245-249).

### 1.5 `allow_any_host` vs host entries

`--allow-host <nqn>` repeatable (M:316-318); empty = allow-any, "the trusted-fabric posture,
stated loudly in docs" (D:497). Each host NQN is validated as a configfs PATH COMPONENT
(`validate_nqn_component` M:333-353: non-empty, no `/`, no whitespace, no leading `.` — VAL-7g:
`..` once addressed the configfs PARENT and the removal fallback was a recursive delete). The
allow-list is RECORDED so `restore` re-presents it: "a restored share must never silently widen
to allow-any" (D:287, S:133-139). The live walk captures `allowed_hosts/` links so an adopted
share's restore never widens either (N:528-537). NQN grammar law for the port: ≤ 223 bytes
(`NVMF_NQN_SIZE`), the component rules above, dots inside legal (M:331-332).

### 1.6 Preflight — `target setup` / the `Share|Restore` rungs (N:324-359, M:680-701)

Reference ladder: (1) `modprobe nvmet nvmet-tcp` (N:329-339) (2) `mount -t configfs none
/sys/kernel/config` against the REAL root only (N:342-346) (3) re-probe `subsystems/` is a dir,
else refuse naming `mount | grep configfs` (N:347-358). **Port** (no `modprobe`, no `mount(8)`):
(1) `statx("/sys/kernel/config/nvmet/subsystems")` present ⇒ ready; (2) else `mount("none",
"/sys/kernel/config", "configfs", 0, null)` — `EBUSY` = already mounted, fine; (3) re-probe; the
`nvmet` dir appears iff nvmet is built in or loaded (the port never loads modules: a missing
tcp/loop transport surfaces at the port link as `ENOENT`/`EINVAL` after the kernel's own
`request_module` and the refusal names `CONFIG_NVME_TARGET_TCP`/`_LOOP`). `target status`
(N:1181-1219) reports `configfs_mounted`, subsystem/namespace/port counts and
`resv_enabled_namespaces`; the port replaces `modules_present` (`/sys/module/nvmet`, false on a
built-in — §0) with `nvmet_present = configfs nvmet dir exists`. `preflight(op)` (N:1053-1061):
`Share|Restore` need the tree; `Unshare|List` tolerate an absent tree (a vanished object is a
verified no-op). `target stop` refuses — "the kernel nvmet target is not a process" (M:705-713);
`target install` is a retired SPDK verb in the reference (M:668-677) and simply does not exist
in the port (quirk Q1). `target systemd-unit` emits the oneshot unit to stdout, values baked,
never installs (N:241-259: `Type=oneshot`, `RemainAfterExit=yes`, `ExecStart=<exe> nvmeof
restore`, `Wants=`+`After=network-online.target`).

### 1.7 The live walk (`walk_live`, N:492-554 — the ONE probe `list`, the duplicate guard,
`restore` and `adopt` all read)

Per `subsystems/<nqn>`: `nsids` = every numeric child of `namespaces/` sorted (N:517-526; the
shape law is exactly `[1]`), `allow_hosts` = the `allowed_hosts/` link names sorted (N:528-537),
from `namespaces/1`: `device_path`, `device_uuid` (empty ⇒ null), `enabled = enable == "1"`
(N:539-542); `backing_canonical = resolve_backing_canonical(device_path)` (N:217-234:
canonicalize, then a loop node through `/sys/block/loopN/loop/backing_file` — the port keeps the
loop arm for ADOPTING a foreign loop-served share, never for its own); listeners from the port
snapshot's links (N:498-508: `(traddr, trsvcid, id)` per link). "Materialized" = `device_path
non-empty ∨ enabled` (N:440, N:910, M:989): an empty mkdir'd shell (a crash between mkdir and the
attribute writes) serves nothing and is absorbed by the share's own idempotent mkdir rather than
refused (N:433-437).

**The live-state duplicate guard** (`duplicate_guard` N:438-488, D:259 "ledger + live
state … the refusal message IS the runbook"): BEFORE any side effect, over every materialized
live share — (a) the requested NQN already live ⇒ refuse naming its classification
(managed/pending/removing/foreign, `classify_holder` N:410-416) and the three exits
(`unshare` / the manual configfs steps `manual_removal_steps` N:420-428 / `adopt`); (b) the
same backing (`backing_canonical` equal, or `device_path` equal to the request's path or
canonical) ⇒ refuse "the same backing must never be double-served" with `unshare` for a managed
holder, removal-first or `adopt` for a foreign one (N:459-485). The ledger half of the guard
re-runs atomically under the ledger lock at `begin_share` (L:218-250).

---

## 2. The share ledger

### 2.1 Location and the six laws (D:250-297; L:1-39)

State dir `/var/lib/squeezefs/nvmeof/` (`DEFAULT_STATE_DIR` L:61), relocatable by
`SQUEEZEFS_NVMEOF_STATE_DIR` (L:65) — a relocation seam in the §6.8 sense (D:375: "env
overrides that relocate real behavior are sanctioned; env vars that fork behavior are
banned"). Files: `shares.json` (L:54), `shares.json.lock` (L:56), `shares.json.tmp` (L:59).
The retired SPDK subtree (`spdk/`, M:446) and the old-registry rename (`/etc/squeezefs/
nvmeof_shares.json` → `.retired-by-rebuild`, L:410-441, M:212-217) are reference history the
port does not carry (quirk Q1).

Laws, each pinned by a test (D:289-297):
1. **Load never writes** (L:120-138): no lock file, no dir creation, no tmp cleanup; succeeds
   on a read-only filesystem; a leftover `.tmp` is logged and ignored. Named for the truncation
   bug (D:58: the pre-rebuild registry `fs::write(&path, "[]")` on every resolution).
2. **Writes are atomic and serialized** (L:149-198): `flock(LOCK_EX)` on the lock file
   (EINTR retried), read → mutate → serialize → write `.tmp` → `fsync` → `rename` → `fsync` the
   parent dir.
3. **Forward-only** (L:361-402): `format > 1` ⇒ "created by a newer squeezefs; upgrade" — the
   version gate runs on a LOOSE parse so a future format's unknown fields produce the upgrade
   message, not a field error; `format < 1` never existed; unknown fields within v1 refuse
   (`deny_unknown_fields`, S:113); absent optional fields are legal.
4. **Reconciliation, never blind trust**: every verb reads live state first (§2.5).
5. **Bookkeeping lives here, never in configfs** (D:295): port ids (and the reference's
   `loop_device`) are ledger fields.
6. **Write-ahead intent** (D:296): `share` appends `pending` BEFORE the first stack mutation
   and flips `active` after the last; `unshare` flips `removing` BEFORE the first teardown
   write and deletes after; "the product can never strand its own share as foreign".

### 2.2 The record — reference schema v1 (D:263-287, S:114-142) → the port's v4 `extern struct`

Reference JSON record: `subnqn`, `stack` (`spdk|nvmet`), `state` (`pending|active|removing`),
`backing_path`, `backing_canonical`, `nsid?` (SPDK-only, null on nvmet — D:287), `ns_uuid?`
(the both-stack identity; null only on N1-era records), `listeners[]` (`ip`, `port`,
`nvmet_port_id?`), `bdev_name?`/`ptpl_file?` (SPDK), `loop_device?` (nvmet), `created_utc`
(RFC 3339, L:444-472), `allow_hosts[]` (absent-not-`[]` on allow-any, S:133-139),
`adopted_from? {utc, class: pre-rebuild|foreign|ledger-loss}` (S:85-110). Value laws
(`validate` S:148-181): non-empty `subnqn`/`backing_path`/`backing_canonical`/`created_utc`,
`listeners ≥ 1` with non-empty `ip`, no empty host NQN.

**Port v4** — `src/ops/nvmeof/ledger.zig` (ONE file `/var/lib/squeezefs/nvmeof/shares.sqzl`,
binary, canonical, total; the `set_config` precedent, decision 0002 row 121 — no JSON, no
`stack`, no SPDK fields, no `loop_device`, no `nsid`):

```
LedgerHeadWire (64 B, LE, extern struct):
  magic[8] = "SQZNVMET" | version u16 = 1 | flags u16 = 0 (checked) | record_count u32
  | body_len u64 | generation u64 (+1 per write; `nvmeof list` prints it)
  | written_s i64 (realtimeCoarseS) | reserved[16] = 0 (checked) | xxh3 u64 over head[0..56] ‖ body
ShareRecordWire (48 B fixed head + length-framed tail):
  rec_len u32 (= head + tail exactly) | state u8 {1 pending, 2 active, 3 removing}
  | trtype u8 {1 tcp, 2 loop} | flags u16 {bit0 ADOPTED, bit1 ALLOW_ANY_HOST; others refuse}
  | created_s i64 | adopted_s i64 (0 unless ADOPTED) | adopt_class u8 {0 none, 1 ledger_loss, 2 foreign}
  | n_listeners u8 (≥ 1) | n_hosts u8 (0 iff ALLOW_ANY_HOST) | reserved[5] = 0 | ns_uuid [16]u8
  tail: subnqn (u16 len ‖ bytes, 1..=223) | backing_path (u16 ‖ bytes) | backing_canonical (u16 ‖ bytes)
      | n_listeners × ListenerWire (24 B: adrfam u8 {1 v4, 2 v6, 0 loop} | reserved u8 | trsvcid u16
        | port_id u16 | reserved u16 | traddr [16]u8 (v4 in the first 4)) | n_hosts × (u16 len ‖ bytes)
```
Decode: every count checked against the bytes present BEFORE any allocation ("a lying length is
never an allocation authority", charter §11); `rec_len` must equal the consumed length; the
head's xxh3 covers the whole body; `version > 1` refuses "newer binary"; the per-record value
laws of S:148-181 apply at decode and at `begin_share`. The CLI renders JSON on `list --json`
(the ledger is not a wire). The `AdoptClass.pre_rebuild` arm (M:65-67, M:808-816) is the
reference's own history — not carried.

### 2.3 The intent lattice (`ShareState` S:40-54; transitions L:200-310)

| From | Verb | To | Law |
|---|---|---|---|
| ∅ | `begin_share(rec)` (L:205-253) | `pending` | refuses a duplicate `subnqn` OR a duplicate `backing_canonical` (any state) naming the holder — the ledger half of the guard, under the lock |
| `pending` | `finalize_share` (L:258-263) | `active` | only from `pending`; anything else is "illegal from state … (law 6)" (L:287-296) |
| any | `mark_removing` (L:268-270) | `removing` | idempotent on `removing` (a resumed teardown); NotFound if absent |
| any | `delete` (L:301-310) | ∅ | only after teardown completed |
| any | `update_record` (L:331-350) | same | bookkeeping refresh under law 2; the key (`subnqn`) may not change — the port needs it only for an adopted record's listener capture |

Forward-only across the lattice: `active → pending` and `removing → active` never happen
(`transition` L:272-298 pins the legal map). The `set_loop_device` mutation (L:315-324) does not
exist in the port.

### 2.4 Atomic renamed writes — the port's procedure (law 2, through the io_uring file layer)

`open(dir)`; `flock(LOCK_EX)` on `shares.sqzl.lock` (a dedicated fd, EINTR retried — L:166-175);
load (law 1's reader); mutate in RAM; encode into a page-aligned buffer; `openat(dir, "shares.
sqzl.tmp", O_WRONLY|O_CREAT|O_TRUNC|O_CLOEXEC, 0600)`; write; `fdatasync`; `renameat` over
`shares.sqzl`; `fsync(dir fd)`; close the lock. The reference uses `std::fs` "sanctioned here per
the reservation.rs precedent" (L:23-25); the port goes through `io/uring_fs.zig` like every
other ad-hoc file I/O (charter §1.2 "an io_uring file layer") — control plane, so an `io:
std.Io` parameter, never a hot path. The dir is created `0700` on the first mutation only
(L:150-158); loads never create it.

### 2.5 Reconcile against live state at every verb (law 4; "never trust configfs bookkeeping")

| Verb | Reads live | Decides |
|---|---|---|
| `share` (N:1063-1135, M:360-439) | the walk + the port snapshot | the live duplicate guard (§1.7); port ids over the snapshot; then `begin_share` → `apply_share` → `finalize_share` |
| `unshare` (M:514-540, N:1137-1146) | nothing before `mark_removing`; teardown tolerates vanished objects | the record is the AUTHORITY for what to remove (recorded ids, hosts); an NQN absent from the ledger refuses — "SqueezeFS never tears down target objects it did not record" — printing the manual configfs steps (M:529-539) |
| `list` (M:1292-1465) | the walk | per record: `live = own stack serves the NQN`; classification (§2.8); live ∖ ledger = `foreign_live` (displayed, never touched); plus the connected-fabric-disks section (§3.4) |
| `restore` (M:631-656, N:1152-1179, N:893-1045) | the walk per record | §2.6 |
| `adopt` (M:1175-1241) | the walk TWICE (classify, then TOCTOU re-verify) | §2.7 |
| `target status` (N:1181-1219) | counts over the tree | report only |

### 2.6 `restore` — idempotent, per-record outcomes (`RestoreOutcome` S:274-297)

| Record state | Live | Outcome (N:893-1045) |
|---|---|---|
| `removing` | any | resume the teardown, delete the record → `TeardownResumed` (N:895-903) |
| `pending` | materialized AND `live_matches_record` (N:861-890: the backing serves AND `device_uuid` equals the recorded `ns_uuid` case-insensitively; a live namespace WITHOUT a uuid while the record carries one is a mismatch) | `finalize_share` → `FinalizedPending` (N:910-921); the never-bound second listener of a crash-window record is NOT re-applied (FID:466-470) — its port shell is removed by the recorded-id teardown law at unshare |
| `pending` | materialized but MISMATCHED | `Failed("… DO NOT match … refusing to finalize or clobber; resolve manually")` (N:922-929) |
| `pending` | absent, or only an unmaterialized shell | teardown (tolerant), delete → `GarbageCollectedPending`, LOUD ("the interrupted share never completed and never returned success", N:931-951) |
| `active` | live AND matching | `VerifiedNoop` (N:960-980) |
| `active` | live but mismatched | `Failed("exists live but MISMATCHES its record … never clobbered")` (N:981-986) |
| `active` | gone | re-establish: `apply_share` with the RECORDED `ns_uuid` and RECORDED port ids → `Restored` (N:989-1042). The N1-era arms (mint a uuid once N:991-1005; allocate ids for null ones N:1008-1037) do not exist in the port — every record carries both from birth |

Per-record errors are COLLECTED into a report, never short-circuited (N:1168-1177; D:189-190);
the verdict (M:597-623) fails the verb when any record failed, so the systemd oneshot goes red
"never green over an unserved share". The G2 leg (FID:850-919): configfs wiped under a LIVE tcp
connection → `list` shows `down` → `restore` prints `<nqn>: restored` → `device_uuid` identical
→ the connected initiator reattaches with data intact (polled ≤ 120 s, FID:895-904) → a second
`restore` prints `already live — verified no-op`.

### 2.7 `adopt <subnqn>` — live, unledgered → managed, mutating NO target state (D:430-440)

Flow (`adopt_over` M:1175-1241): `live_shares()` → `adopt_candidate` (M:903-1090) → `begin_share
(pending with ADOPTED provenance)` → `live_shares()` AGAIN → `adopt_verify_unchanged`
(M:1097-1164: device path, canonical, `ns_uuid` (case-insensitive), `nsids == [1]`, the
listener set incl. ids, the allow-hosts set) → on drift or a failed re-probe: `delete` the
pending record and refuse "TOCTOU drift … no target state was touched" (M:1196-1216) →
`finalize_share`. Refusal classes (each a grep-able `[token]`, M:819-822):

| Token | When (M:903-1083) |
|---|---|
| `adopt_not_live` | the NQN is not in the walk (a ledgered-but-down share is `restore` territory) |
| `adopt_already_ledgered` | the NQN OR its canonical backing is already recorded, ANY state (`pending`/`removing` → `restore`; `active` → `unshare`/`restore`) |
| `adopt_backing_duplicated` | another MATERIALIZED live subsystem serves the same canonical backing or device path — "absorbing one of two same-backing servers would bless the double-serve" |
| `adopt_harness_owned` | the NQN carries a harness marker (`HARNESS_NQN_MARKERS = [":devsub-", ":fideli-", "spdkscope"]`, M:795) or rides a harness port id (`[52026, 52470, 52471]`, M:800) — "harness objects belong to their harness's teardown" |
| `adopt_shape_unsupported` | no materialized namespace; `nsids ≠ [1]`; no listener (rev 5 amendment D:663: "a subsystem with no fabric presence has nothing an initiator can reach and nothing the schema (`listeners ≥ 1`) can represent") — remediation removal-first + re-share |

The candidate (`build_adopt_record` M:827-893): a loop-served `device_path` becomes
`backing = backing_file`, the reference recorded the node as `loop_device`; the port records the
node AS the backing (it owns no loop association to detach) and notes it. A null live
`device_uuid` is recorded null with a loud note that restart-identity stability requires a
re-share (M:848-855). OUT-OF-RANGE port ids (a hand-built share's small-int id) are recorded
as-is with a note — "the reserved range governs only allocation; the link-free teardown law
covers removal" (M:856-871; FID:596-680 leg A1 proves unshare removes port `1..9`). The port's
classes are `ledger_loss` (the product's own prefix on an unledgered object) and `foreign`
(M:808-816 minus `pre-rebuild`).

### 2.8 `list` classification (`classification_of` M:1272-1286)

`pending` → "interrupted share; `nvmeof restore` finalizes or garbage-collects it";
`removing` → "interrupted unshare; `nvmeof restore` resumes the teardown"; `active ∧ live` →
`managed`; `active ∧ ¬live` → "down — restore candidate". JSON (M:1313-1368): `shares[]` with
`classification` + `live`, `foreign_live[]` (`subnqn, device_path, backing_canonical, ns_uuid,
enabled, listeners[]`), `connected_fabric_disks[]` (`device?, subnqn, address`).

---

## 3. The initiator (`src/ops/nvmeof/initiator.zig`, Linux-only)

### 3.1 The connect: the string written to `/dev/nvme-fabrics`, and the read-back

Reference (`fabrics_connect_string` I:846-868; the "no-nvme-cli fallback" I:892-911 — the ONLY
path the port has, §0): 
```
transport=tcp,traddr=<ip>,trsvcid=<port>,nqn=<subnqn>,hostnqn=<hostnqn>,hostid=<hostid>
  [,host_traddr=<src ip>][,host_iface=<ifname>][,nr_io_queues=<n>]
```
The port adds the loop shape `transport=loop,nqn=<subnqn>,hostnqn=…,hostid=…` (§0;
`traddr` optional). Procedure: `open("/dev/nvme-fabrics", O_RDWR|O_CLOEXEC)` (absent ⇒ refuse
naming `CONFIG_NVME_FABRICS`/`CONFIG_NVME_TCP`, I:894-901); `write(fd, string)` — a short
write never happens (the kernel consumes the whole buffer); errno table: `EINVAL` malformed
option / unknown transport after autoload, `EADDRNOTAVAIL`/`ECONNREFUSED`/`EHOSTUNREACH`
network, `EALREADY` an identical `(transport, traddr, trsvcid, nqn, hostnqn)` controller already
exists (`nvmf_ip_options_match`), `ENOENT`/`EIO` the target refused the subsystem/host
(allow-list), `EBUSY`; then **`read(fd)` → `instance=<N>,cntlid=<M>\n`** (§0) — the controller
is `/sys/class/nvme/nvme<N>`. The reference discards this answer and walks sysfs by `subsysnqn`
(I:934-940, a 100 ms sleep-poll for 2 s) — quirk Q7; the port uses the instance.
`--nr-io-queues 0` refuses BEFORE any side effect (I:924-930). `--host-traddr`/`--host-iface`
pass through only when supplied (I:782-799: an all-absent set is byte-identical to the base
string). `connect` requires root (I:876).

### 3.2 Identity: pair-or-neither; `/etc/nvme/` read-only; the derived default

`SQUEEZEFS_HOSTNQN` / `SQUEEZEFS_HOSTID` (I:46-48; knobs.zig L266-267) and the mount options
`-o hostnqn=` / `-o hostid=` (option > env per field, I:124-141; a PRESENT key with an EMPTY
value is malformed, I:97-112). **Pair-or-neither** (I:142-157): exactly one configured REFUSES
("a hostnqn presented under the host's default hostid is how registrants alias on the device").
Landed in the port as `pr.resolveExplicitIdentity` (pr.zig L470-477) and `HostIdentityRefusal`
(L446-457). `connect --hostnqn/--hostid` are `requires`-paired at the CLI (MAIN:1133-1140).

Defaults: the reference READS `/etc/nvme/hostid` and, when absent, MINTS a v4 uuid and WRITES
`/etc/nvme/hostid` (I:67-69) and `/etc/nvme/hostnqn = nqn.2014-08.org.nvmexpress:uuid:<hostid>`
(I:87-89) — decision 0002 row 187 / guard Q7: "a mount silently mutating host configuration";
the port's `proc.EtcNvme.read` is read-only (proc.zig L158-183) and "the fabrics initiator
(rung 8) decides minting". **Rung 8 decides: never write.** Resolution for a connect:
(1) the explicit pair; (2) `/etc/nvme/hostnqn` + `/etc/nvme/hostid` BOTH present (one of the two
present ⇒ refuse: the pair law applies to the files too — libnvme would mint the missing half
and alias); (3) the DERIVED pair: `hostid = uuid(sha256("squeezefs:nvme-hostid:" ‖
/etc/machine-id)[0..16])` with the RFC 4122 version/variant bits set, `hostnqn =
"nqn.2014-08.org.nvmexpress:uuid:" ‖ hostid` (libnvme's own `nvmf_hostnqn_generate` falls to
the machine id when DMI/`/etc/nvme` are absent — the same family, deterministic per host, never
written anywhere); `/etc/machine-id` absent ⇒ refuse naming the three sources. WHY not the
kernel default: `nvmf_default_host` is a RANDOM uuid per module load, so an un-named connect
registers under a different host identity every boot and the guard's identity-stability stamp
(`IdentityStamp`, pr.zig L483-494) would read a change at every reboot's first beat. The source
is published (`fabric_host_identity_source = explicit | etc_nvme | machine_id`) and logged at
the connect.

### 3.3 `nvme_core.fabrics_host_scoped_subsystems` — what the daemon checks and announces

The sqz kernel's opt-in, boot-scoped module parameter groups fabric controllers by `(subsysnqn,
hostnqn)` (charter §3 L68; DMP:86-91) so two per-mount identities on one host do not merge under
one multipath head "whose round-robin voids per-mount device fencing" (I:414-420). The
reference never reads the parameter; it detects the MERGED SHAPE from sysfs (`multipath_merged_
shape_at` I:450-491: a subsystem dir carrying the namespace whose SERVING controllers carry > 1
distinct `hostnqn`) and names the parameter in the refusal (I:702-712, both remedies: the sqz
kernel with `=Y`, or a stock kernel with `nvme_core.multipath=N`). Serving membership is
SUBSYSTEM-DIR-SCOPED (`subsystem_serving_controller_dirs` I:384-412: the `nvme<N>` entries the
kernel's `sysfs_create_link` put in the dir; a link-less dir falls back to global `subsysnqn`
matching) — load-bearing on the scoped kernel where SIBLING subsystems share one `subsysnqn` by
design (DMP:302-307). The kernel also exposes `sqz_host_scope` per subsystem dir (empty when
unscoped; DMP:155-163).

Port: read `/sys/module/nvme_core/parameters/fabrics_host_scoped_subsystems` ONCE at mount
(`Y`/`N`/absent = stock) and publish `fabric_host_scoped = {0, 1, 2(absent)}` on `.stats` and
`status`; port the merged-shape detector and the ladder VERBATIM (pure over injected sysfs
roots — TI:108-187 fixtures); announce at the first fabric connect: "host-scoped fabric
subsystems: on|off|stock kernel (co-located per-mount identities on this host will
merge/won't)". On the venue (no `CONFIG_NVME_MULTIPATH`) the merged shape cannot arise — two
identities on one subsystem yield two controllers with their own `nvme<N>n1` nodes in ONE
`nvme-subsys<K>` dir (the kernel warns "Found shared namespace … multipathing not supported"
and proceeds); the refusal arm stays fixture-tested, the two-identity FENCE is proven live (§4.8).

### 3.4 Discovery after connect: `/dev/nvme<X>n<Y>` + the `ng` char node; by `device_uuid`

Reference order (`find_device_for_nqn_at` I:232-251): the SUBSYSTEM class first
(`/sys/class/nvme-subsystem/*/subsysnqn == nqn` → its first strict-shape `nvme<X>n<Y>` child —
the multipath HEAD whose instance is the subsystem's), then the controller class; the shape
rule is STRICT `^nvme\d+n\d+$` (`split_nvme_namespace` F:157-163) so hidden path nodes
`nvme<X>c<C>n<Y>` and partitions never match (the 2026-07-25 `/dev/nvme0c0n1` bug, I:197-203);
"unresolvable is an honest `None` — device names are never fabricated by string concatenation"
(I:227-230). Under an explicit identity: `find_device_for_nqn_under_identity_at` (I:522-568) —
the per-controller node of a controller carrying OUR `hostnqn`+`hostid` attrs
(`controller_identity` I:277-282), else a head whose EVERY serving controller is ours (a mixed
or identity-less serving set is never handed out).

Port procedure (no `subsysnqn` guessing): from the connect's `instance=N`: (a) per-controller
node: the first `nvme<N>n<Y>` child of `/sys/class/nvme/nvme<N>` (the venue's shape);
(b) multipath kernels: the `nvme-subsys<K>` dir that lists `nvme<N>` → its `nvme<K>n<Y>` head
(offered only when the serving set is dedicated to our identity — the reference law). Then
VERIFY the namespace by identity, not by name: `/sys/class/block/<node>/uuid` (the Identify
Namespace Descriptor UUID the target stamped from `device_uuid`) must equal the ledger /
`fabric_endpoint` record's `ns_uuid` when one is known (a renumbered node binding the wrong
disk is the class decision 0002 row 125 forbids); `/sys/class/block/<node>/nsid` is the nsid;
the char twin is `/dev/ng<X>n<Y>` verified through `/sys/class/nvme-generic/ng<X>n<Y>/dev`
(pr_linux.zig L124-146 — already landed). Readiness: the namespaces materialize asynchronously
(the kernel's scan after `nvme_start_ctrl`); the port waits on a `NETLINK_KOBJECT_UEVENT`
socket opened BEFORE the write for `ACTION=add SUBSYSTEM=block DEVNAME=nvme<N>n*` (through the
`io` poll — no sleep-poll; the reference sleeps 100 ms × 20, I:934-940, and `wait_device_for_
nqn_under_identity` sleeps 100 ms up to 5 s, I:592-607, MAIN:5848-5853) with the deadline =
`/sys/module/nvme_core/parameters/admin_timeout` seconds (60 default — a scan is a sequence of
admin commands each bounded by it; the WHY on the line). Output of `connect`: the block node,
the char node, nsid, uuid, instance, cntlid.

### 3.5 Disconnect (`disconnect_controllers_at` I:950-966)

Write `"1"` to `/sys/class/nvme/nvme<N>/delete_controller` for EVERY controller whose
`subsysnqn` matches (multipath kernels attach several path controllers — "all of them are the
connection"); none ⇒ refuse "No connected controller found for NQN" (I:979-983). The port adds
`--identity-only` (delete only the controllers carrying OUR pair — a co-located mount's
controller is never deleted by another mount's disconnect) and makes it the daemon's own
teardown rule for daemon-owned connects (§3.7).

### 3.6 The `fabric_*` family (F:1-14; D:424)

Sampled from `/sys/class/nvme` at the stats cadence (10 s): `fabric_controllers` (gauge:
controllers with `transport != pcie`, F:224-229), `fabric_ctrl_not_live` (gauge: `state !=
"live"`; a missing state file reads `unknown` = not live, F:86-90), `fabric_ctrl_reconnects` —
"a SAMPLED-TRANSITION counter, not a kernel counter … counts observed not-live→live transitions
per identity and UNDERCOUNTS flaps faster than the cadence … do NOT try to fix it against a
kernel counter that does not exist" (F:294-303). Identity = `(transport, subsysnqn,
stable_endpoint(address))` where `stable_endpoint` keeps only `traddr=`/`trsvcid=` (F:61-76) —
FIND-N6-A (D:679): nvme-tcp appends `src_addr=` only while queue 0 is live, so the raw
`address` string differs between `connecting` and `live` and keying on it minted a fresh
identity at every reattach. Sampler state law (F:279-341): identities that vanish while
not-live are retained (a give-up + renumbered reappearance as live counts once); identities
that vanish while live are dropped; live-wins on duplicates. `squeezefs status` gets a
per-volume `Fabric` section (F:410-458: counted over the controllers backing THIS volume's
devices, `backs_device` F:127-149 matching plain child / multipath path node / controller
prefix; `fabric_ctrl_reconnects` is 0 by construction in a one-shot CLI sample). The port's
sampler runs on the `sqz-timer` cadence off the stats inode, `counters.Gauge` ×2 +
`ShardedCounter`.

### 3.7 `fabric_endpoint:` records and daemon-owned data-plane connects (KD-MW-15)

`squeezefs config set-fabric-endpoints <sqmeta-uri> <vol_id>=<traddr>:<trsvcid>:<subnqn>…`
writes ONE record per DATA volume (LIB:1078-1098: name `fabric_endpoint:{vol_tag:016x}`, on
ino 1 beside `writer_claim`, screened from FUSE by the xattr allowlist); the record is `version
u8 | (len u16 LE ‖ bytes) × {traddr, trsvcid, subnqn} | xxh3-64` (LIB:1114-1130), decode
total — torn/truncated/future-version refuse "never a guessed coordinate" (LIB:1133-1187).
`get-fabric-endpoints` lists them (MAIN:7244). The port carries the record in `set_config`'s
control record instead (one versioned, checksummed home — decision 0002 row 121): per data
volume an optional `FabricEndpointWire {trtype u8, adrfam u8, trsvcid u16, traddr [16]u8,
subnqn (u16 ‖ bytes)}` plus the volume's `device_uuid` it already carries.

The §5.2 refusal ladder (`fabric_ladder_verdict` I:681-771, pure over `VolumeShape` I:629-647):
with NO explicit identity everything accepts unchanged; with one: a DATA volume with a record →
`DaemonConnect`; the merged shape → refuse naming the shape + both remedies; any actual
controller identity ≠ the pair → refuse (rule 2: "an explicit per-mount identity is never
inert"); a DATA volume with NO record → refuse UNIFORMLY naming `set-fabric-endpoints` ("a
pre-connected controller is refused even when its identity matches" — one rule keeps the
ladder decidable); META volumes are the bootstrap exemption (operator connects, rule-2
verified; no fabric identity ⇒ `AcceptUnverified`, loud). The daemon's connect
(MAIN:5778-5860): reuse a controller already riding OUR pair, else §3.1 from the record and
resolve UNDER OUR controller (§3.4); the port stores `(instance, device_uuid)` per volume for
its own `disconnect --identity-only` at clean unmount.

---

## 4. The REAL PR legs (what rung 5 built on the fake; what rung 8 proves on nvmet)

### 4.1 What exists and what is deferred (`guard_claims.md` §5.3; pr_linux.zig L13-14)

Landed: the whole client over `nvme_cmd.Namespace` (pr.zig), the uring_cmd device
(pr_linux.zig), the fake with both register semantics, the ladder contracts, the heartbeat
re-check, the fence latch (`journal.FailureLatch.classifyFence`, decision 0002 row 183). The
real-device test SKIPS where no `/dev/ng*` exists (pr_linux.zig L230-233). Deferred to here:
"the real RESCAP byte; the `nsid` + `ngXnY` resolution; EDS = 1 vs the short form (SC 0x18);
IEKEY/CPTPL acceptance; the lenient vs SpecStrict register; the real `EBADE` on a preempted
association's O_DIRECT write/`fdatasync`; `preempt_and_abort` as plain preempt; a target
restart as the PTPL lapse; the shared-identity gauge with two associations".

### 4.2 The command set (R:823-828, R:927-1022; the port's `nvme_cmd.zig` L105-227)

| Verb | Opcode | CDW10 | CDW11 | Payload (LE) | Queue |
|---|---|---|---|---|---|
| Identify Namespace | `0x06` admin | CNS 0 | — | 4096 B out; RESCAP = byte 31 (R:1027-1038) | admin |
| Get Features Host Identifier | `0x0a` admin | FID `0x81` (SEL 0) | `EXHID` bit 0 = 1 ⇒ 16 B, 0 ⇒ 8 B (R:1056-1088) | out | admin |
| Register / Unregister | `0x0d` | `RREGA` bits 2:0 (0 register, 1 unregister), `IEKEY` bit 3, `CPTPL` bits 31:30 (R:1091-1118) | — | `[crkey u64 ‖ nrkey u64]` 16 B (R:927-940) | io |
| Report | `0x0e` | `NUMD` = `len/4 − 1` (R:945-953) | `EDS` bit 0 | out, two steps (§4.3) | io |
| Acquire / Preempt / Preempt-and-abort | `0x11` | `RACQA` bits 2:0 (0/1/2) ‖ `rtype << 8` (R:987-1000) | — | `[crkey ‖ prkey]` 16 B | io |
| Release | `0x15` | `RRELA` 0 ‖ `rtype << 8` (R:1004-1014) | — | `[crkey]` 8 B | io |

rtypes: WE = 1, WERO = **3** (R:832-846 — shipped as 2 = Exclusive Access until `fix/wero-
rtype`: a REGISTERED second host was refused READS on nvmet 2026-08-15; TW:116-187 pins the
enum). Status: Reservation Conflict `0x83` (R:846) ⇔ the ONE typed `error.ReservationConflict`
(`errors.zig`, `-EBADE` ↔ `BLK_STS_RESV_CONFLICT`, R:455-469). Transport: the 72 B
`nvme_uring_cmd` in the SQE128 command area, `NVME_URING_CMD_IO` on the `/dev/ng*` fd and
`NVME_URING_CMD_ADMIN` on the CONTROLLER node `/dev/nvme<N>` (`nvme_ns_uring_cmd` answers
`-ENOTTY` to an admin op on the namespace node — measured by the fidelity tier, decision 0002 row
392; `pr_linux.Device` holds both fds) — the reference's `ioctl(NVME_IOCTL_{IO,ADMIN}_CMD)` on
the BLOCK fd (R:816-820, 898-923) is decision 0002 row 182.

### 4.3 `REGCTL` sizing — the two-step report (R:131-211, R:960-982; pr.zig L320-353)

Header 64 B extended (EDS = 1, 128-bit host ids — every fabrics association) / 24 B short
(PCIe 64-bit ids); stride = header; `len = hdr + hdr × REGCTL` (R:148-161). Read the header
alone → `REGCTL` at bytes 5..7 → read `len` → if the second image's REGCTL grew, re-read ONCE
at the new size (R:968-982). Decode: `gen u32 ‖ rtype u8 ‖ regctl u16 ‖ … ‖ ptpls u8 @9`; each
structure `cntlid u16 ‖ rcsts u8 @2`; extended `rkey @8 ‖ hostid[16] @16`, short `hostid u64 @8
‖ rkey @16` (R:165-170, R:182-204). WHY: "the shipped read filled a fixed 4 KiB buffer and its
parse loop broke at the 63rd extended registrant ((4096 − 64)/64), so the 64th co-writer read
as unregistered" (R:136-139); the fidelity leg puts ≥ 64 real registrants on one namespace and
asserts `report_bytes == 64 + 64 × N` (FID:1216-1220). EDS first; "Host Identifier
Inconsistent Format, SC 0x18" on the short form against nvmet (R:1152-1154) ⇒ the short form is
the fallback for PCIe only (pr.zig L320-326). Gauges `pr_registrants_per_namespace`,
`pr_report_bytes` (R:213-247; per-client fields in the port, decision 0002 row 191).

### 4.4 FID 0x81 — the wire host identity (R:1056-1089; pr.zig L300-315)

16 B with `EXHID = 1` first ("kernel nvmet rejects EXHID = 0 with Invalid Field"), 8 B on a
device status (PCIe); all-zero ⇒ UNKNOWN ⇒ the ladder fails CLOSED. "Deliberately NO /etc/nvme
fallback: the 2026-07-17 scoping session measured the association identity diverging from the
config files, and matching a value the device did not attribute to this association could cross
the foreign-registration line" (R:1065-1069; D:370 "the files are never used to match
registrants").

### 4.5 The ladder — "unregister only OWN stale keys" (R:582-706; pr.zig L202-260)

1. Plain `register(key)`: `RREGA 0 | IEKEY | CPTPL` where CPTPL is requested iff RESCAP bit 0
   (PTPL-capable; decision 0002 row 190 — the reference retried three encodings on ANY
   non-conflict error, R:1099-1111); an Invalid Field (SC 0x02) retries the plain shape ONCE.
   Success = the fast path; a success at or past a LEARNED cap unlearns it (R:602-610).
2. A non-conflict DEVICE status in the command-/vendor-specific class with `REGCTL > 0`,
   repeated at the same count, LEARNS the registrant cap (R:613-636; never from an errno,
   never from a transient status R:339-365) — "unbounded on nvmet by construction" (EK:420).
3. On Reservation Conflict ONLY: `wire_host_id()` (empty/unreadable ⇒ fail closed with the
   ORIGINAL conflict, R:645-661) → `report()` (failed ⇒ fail closed, R:662-671) → own-stale =
   registrants with `host_id == ours ∧ rkey ≠ key` (R:672-678); none ⇒ "a foreign registration
   is not ours to remove; failing closed (arbitration stays the acquire-conflict / claim /
   preempt path)" (R:679-686) → `unregister` each (device-validated `crkey`, R:688-696) →
   `register(key)` fresh → `RecoveredOwnStale{unregistered}` (R:698-706).

WHY the ladder fires on nvmet for real: the port's `writer_id` is per INCARNATION (decision
0002 row 185: `xxh3(boot_id ‖ hostname ‖ mount_slot ‖ pid ‖ start_ticks)`, the PR key
`xxh3(writer_id ‖ NUL ‖ boot_id)`), so every `kill -9` → remount registers a DIFFERENT key under
the SAME host identity, and nvmet's `pr.c` conflicts (§0) — the own-stale arm is the kill-9
recovery path on THE target (GS:278-283 asserts "register ladder fired on the strict target").

### 4.6 The PTPL gap and the heartbeat heal (`guard_claims.md` §1.5; gate.zig L502-532)

nvmet advertises no PTPL (§0; D:84 "PTPL=0 — reservations do not survive target power cycles
there"). Every 10 s beat, BEFORE the claim refresh: `report()`; holder == our key ⇒ fine;
**holder == none ⇒ the PTPL lapse** (the target lost its reservation table — a configfs wipe +
`restore`, a reboot of the target host) ⇒ `registerLadder` + `acquire`, success counts
`writer_guard_pr_reacquires` and logs "reservation holdership had lapsed (PTPL-less target
power cycle?) — re-acquired … audit the fabric"; failure ⇒ fail-stop; holder == foreign ⇒
fail-stop "usurped" (`writer_guard_fenced`). "`pr_reacquires` growth is EXPECTED across nvmet
target power cycles" (D:369; charter §8) — a counter, not a tripwire. Stated residual: ≤ 10 s
after a target power cycle in which a second mount could acquire before the incumbent's
re-check (D:369 "the ≤ 10 s PTPL-lapse residual bound").

### 4.7 The fence (D:371; `guard_claims.md` §1.6)

`EBADE` "measured identical on both stacks" is the reservation-conflict errno a fenced holder's
write/`fdatasync` returns; the port's writes are O_DIRECT so the conflict arrives on the ENTRY
WRITE CQE as well as the barrier: `journal.FailureLatch.classifyFence` at the lane's write
verdict, the lane's barrier and the device loader's funnel latch FAILED at once
(`writer_guard_fenced` + `volume_fenced`; decision 0002 row 183). The data plane's `FenceSignal`
stops data DMA too.

### 4.8 What the venue can prove (loop + tcp on ONE host) — the legs §5 runs

| Leg | Substrate | Asserts |
|---|---|---|
| RESCAP | loop + tcp | `probeRescap() != 0`, PTPL bit 0 (nvmet), WE bit set (FID:772-777) |
| EDS first | loop + tcp | the extended report decodes; the short form answers SC 0x18 (R:1152) — pinned as the fallback's trigger |
| Register fast path + idempotent same key | both | `registered`; a second same-key register succeeds |
| Spec-strict probe (GS:198-207) | both | a different-key register from the same association CONFLICTS (0x83); the ladder recovers own-stale; `pr_own_stale_recoveries = 1` |
| Two identities, WE fence | both (two connects with distinct `hostnqn/hostid` → `nvme<A>n1`, `nvme<B>n1`) | A registers + acquires WE; a 4 KiB O_DIRECT write through `/dev/nvme<B>n1` fails `error.ReservationConflict` (`-EBADE`); B registers, `preempt(A)`, writes land; A's next write fails (FID:795-826 with nvme-cli; the port through its own `pr.Client` + `uring_fs`) |
| WERO rtype 3 | both | B registered, A holds WERO: B's writes land, an UNREGISTERED C's fail; the report echoes `rtype == 3` (TW:371) |
| Release + unregister = zero residue | both | `regctl == 0` after `release()` (pr.zig L289-296) |
| `preempt_and_abort` | both | executes as a plain preempt on nvmet (R:544-545) — the victim's registration is gone, the hold stands |
| Registration persists across the holder's DISCONNECT | tcp | disconnect A, reconnect under A's identity, `regctl == 1` (FID:801-810) — nvmet keys registrations by hostid, not by controller |
| PTPL lapse | tcp (loop deletes the host controller) | configfs wipe + `restore` under a live mount → the heartbeat re-acquires, `writer_guard_pr_reacquires += 1`, `writer_guard_fenced == 0` |
| ≥ 64 registrants | loop (`nr_io_queues=1`, 64–128 connects) | the product's two-step read reports N, `pr_report_bytes == 64 + 64 × N`, cap = 0 (FID:1130-1257) |
| Shared identity | both | two mounts under ONE pair: `pr_registrant_shared == 1` (pr.zig `identityShared` L364-371) |
| Kill -9 remount | tcp + loop | GS:251-291 through the port's daemon |

Not provable here: a PTPL-capable target (no SPDK; the fake's RESCAP bit 0 stands in), a PCIe
64-bit host id (the short form stays fake-pinned), a multipath head.

### 4.9 The `nvmeof resv-report <dev> [--json]` verb (MAIN:1158-1167, 3984-4050)

Opens the namespace the way the guard does (the port: `pr_linux.resolve` → `/dev/ng*`), reads
the two-step report, prints `regctl`, `report_bytes`, `rtype`, `holder_key`, `wero`,
`registrant_cap`, `registrants[] {rkey, host_id hex, holds_reservation}`; a non-namespace path
refuses "a file-backed or loop volume has no Persistent Reservations to report". The port adds
`--pr-matrix` (root, DESTRUCTIVE, refuses a namespace with `regctl > 0` unless `--force`): the
§4.8 single-host matrix as a verb so the fidelity tier needs no nvme-cli.

---

## 5. THE FIDELITY TIER (`zig build fidelity` — Linux, root; charter §11 release gate)

### 5.1 Principles (D:375-382; SUB:52-66)

Zero mocks for target paths (D:375): every leg drives the PRODUCT's verbs against the real kernel; unit
tiers use injection seams (an explicit configfs root; a relocated state dir; an injected
`/dev/nvme-fabrics` path), never env behavior forks. Ownership: every object carries the
harness marker (`nqn.2026-10.io.squeezefs:fideli-…`, refused by `adopt` — §2.7) or is recorded
by exact id in a manifest; teardown removes ONLY manifest/ledger entries; foreign configfs
trees, loop devices, user mounts are NEVER touched; modules are never unloaded. Port-id slice
`54000–54099` (`SQUEEZEFS_NVMET_PORT_ID_BASE=54000`, listener ports chosen so the first candidate
lands inside it — SUB:33-37; the port recomputes with xxh3). Zero residue: a configfs + ledger
+ `/sys/class/nvme` snapshot before and after; a non-empty diff fails the run (SUB:21-25).
The multi-run discipline: a fix restarts the kill-9 count from zero (GS:26-27).

### 5.2 The substrate on the venue (no nvme-cli / jq / modprobe / ip / zram)

Backings: sparse files under `/build/fideli/` (ext4 — AGENTS.md "real block semantics, sparse
files"; `share --create-size` creates them, M:271-279) served by nvmet's FILE backend (§0) —
no `losetup`, no zram (`/sys/class/zram-control` is absent; `SUB` `mkzram` is replaced by
`--create-size`). State under `/build/fideli/state` (`SQUEEZEFS_NVMEOF_STATE_DIR`). Transports:
`loop` (latency-shaped rows) and `tcp` on `127.0.0.1` (anything fabric-sensitive — charter §11
"nvmet-loop for latency-shaped rows, nvmet-tcp for anything fabric-sensitive"). Identities:
the derived machine-id pair (§3.2) for the first mount; explicit `SQUEEZEFS_HOSTNQN/HOSTID`
pairs for the second association. Every assertion that the reference made with `nvme`/`jq`
is made through the product (`nvmeof list --json` read by `core/json_scan.zig`, `nvmeof
resv-report --json`, `.stats`).

### 5.3 The legs

**quick** (per PR touching `src/ops/nvmeof/*`, `src/meta/guard/*`): `substrate-up` →
`roundtrip-loop` → `roundtrip-tcp` → `guard-loop ×1` → `substrate-down` (zero residue).
**full** (nightly / release) adds `loudfail`, `crash-window`, `adopt`, `pr-matrix` (both
transports), `two-identity-fence`, `g2-restore-tcp`, `pr-registrants`, `guard-tcp ×10`,
`guard-loop ×10`, `sigstop-preempt`, `storage-list`.

| Leg | Steps through product verbs | Asserts (file:line of the reference leg) |
|---|---|---|
| `roundtrip-{loop,tcp}` (G1) | `share <file> --create-size 1G [--transport loop \| --ip 127.0.0.1 --port P]` → `list` → `connect` → 64 MiB O_DIRECT md5 round trip through `uring_fs` → `disconnect` → `unshare` | subsystem dirs live; `resv_enable == 1`; `device_uuid == ledger ns_uuid`; `list`: managed+live; after unshare: no subsystem, the port removed (link-free last-out), ledger clean (FID:208-323) |
| `fidelity-mount` (THE row charter §8 names) | `share` meta (1 GiB) + data (4 GiB) files → `connect` both → `format sqmeta:///dev/nvme<A>n1 sqdata:///dev/nvme<B>n1 --single-writer` → `mount --daemon --log-file` → `.stats writer_guard_mode == "flock+pr"`, `pr_rescap != 0`, `pr_rtype == 1`, holder == our key via `resv-report` → write 8 MiB, `sync` → second mount under a SECOND identity pair (its own `connect`) refused naming the guard → `kill -9` → remount recovers (`pr_own_stale_recoveries == 1`, md5 intact) → `umount` → `regctl == 0` → `disconnect` → `unshare` | GS:216-319; "the metadata/data volume IS the connected namespace" — the daemon's `pr_linux.resolve` finds `/dev/ng<A>n1` through sysfs (pr_linux.zig L149-189) |
| `sigstop-preempt` | mount A; `SIGSTOP` the daemon; wait past `T_owner` (45 s); mount B under the second identity (the recovery ladder preempts A's key); `SIGCONT` A; A's next barrier | A: `writer_guard_fenced == 1`, `error.ReservationConflict` at a write/barrier CQE (D:393-394 "device rejection, not a latch"); B's writes land throughout |
| `g2-restore-tcp` (G2) | share + connect + mount; wipe the subsystem by hand (marker-attested); `list` → down; `restore` → `restored`; `device_uuid` identical; the controller reconnects; the daemon's heartbeat re-acquires | `writer_guard_pr_reacquires == 1`, `writer_guard_fenced == 0`, md5 intact; second `restore` → verified no-op (FID:850-919) |
| `crash-window` (law 6) | W-N1: `share --ip 127.0.0.1,192.0.2.55` (TEST-NET-1, unassigned ⇒ `EADDRNOTAVAIL` at the second port link) → record `pending`, partial objects live → a second share of the same backing refused MID-WINDOW naming the pending holder → `restore` → `pending intent finalized` → `unshare` removes BOTH recorded port ids. W-N2: `share --ip 192.0.2.66` → `pending` → manual wipe → `restore` → `garbage-collected`, the never-linked port shell swept | FID:463-573 |
| `adopt` | A1: hand-build a subsystem with a small-int port id → `connect` → IO → `list` shows foreign → configfs snapshot → `adopt` → snapshot IDENTICAL (shape+content+links, mtimes excluded — FID:578-590) → record carries the live uuid, the out-of-range id, `ADOPTED` → IO md5 unchanged → re-adopt refuses `[adopt_already_ledgered]` → `unshare` removes the small-int port. A2: product `share` → delete the ledger file → `list` foreign → `adopt` (`ledger_loss`) → `restore` no-op → `unshare`. A3: `adopt` of a `:fideli-` NQN refuses `[adopt_harness_owned]` | FID:592-680; TA:721-947 |
| `loudfail` (G3) | missing backing without `--create-size`; unledgered `unshare`; a `--allow-host ../x`; a malformed `--ip`; `--nr-io-queues 0`; `target stop`; `--transport loop --ip x` | each refusal text names its remediation (FID:328-461; TG:309-553) |
| `pr-matrix-{loop,tcp}` | `resv-report --pr-matrix` on a fresh share | §4.8 rows 1–8 |
| `two-identity-fence` | §4.8 row 5 through two product connects | the device refuses the non-holder (`-EBADE`) |
| `pr-registrants` | 64–128 `connect --hostnqn/--hostid --nr-io-queues 1` on loop; each registers its own key through `/dev/ng<N>n1`; `resv-report` | `regctl == N`, `report_bytes == 64 + 64 × N`, cap 0; drain to 0 (FID:1130-1257) |
| `guard-{loop,tcp} ×N` | the kill-9 matrix of `fidelity-mount` repeated N times | GS:251-291 |
| `storage-list` | `storage list` while the shares are connected | the inventory names each connected namespace, its transport, nsid, uuid, and the squeezefs superblock class (§6) |
| `substrate-down` | teardown by manifest; snapshot diff | empty diff (SUB:21-25) |

### 5.4 Gate rows

`zig build gate` keeps the portable unit tier (the pure allocator, the ledger codec, the walk
over an in-process fake configfs tree, the connect-string codec, the sysfs resolvers over
fixture trees, the ladder on the fake) plus `tests/rails.zig` (every knob a row; no
`std.process.Child`/`execve` in `src/ops/nvmeof/*` — the no-shell-out rail). `scripts/venue
zig build fidelity -Dtier=quick|full` runs §5.3 as root in the container; per PR for the named
paths, nightly `full`; the release gate requires `full` green (charter §11 L285). Every leg
writes a transcript under the state dir; the summary table names pass/fail/duration per leg
(FID:132-146).

---

## 6. The `storage` verb

Reference (`MAIN:841-999`, `ST:1-832`): an LVM wrapper — `storage pool {create,add,remove,
delete,list}` / `storage volume {create,extend,delete,list}` shelling to `pvcreate -y -ff`
(ST:21 — force re-init of whatever disk is named), `vgcreate`/`vgextend`, `lvcreate -y -i N -I
512K -n -L` (ST:86-106, default stripe 512K), `lvextend`, `lvremove`, `vgremove`, `vgs/pvs/lvs`
parsing, interactive confirmations (`confirm_action` ST:178), a loop-device detach inside pool
delete (ST:365), and `pv_is_squeezefs_backed` (ST:516-532) classifying PVs by the OLD NQN
prefix `nqn.2026-06.io.squeezefs:` (stale since N2's `nqn.2026-07…`, M:62). The one pure part:
`validate_backing_device` (ST:438-502): a missing path, a zero-size file, a zero-capacity block
device (`/sys/class/block/<n>/size == 0`, "an unattached loop device"), a CHAR device ("pass the
namespace block node /dev/nvme0n1, not the controller /dev/nvme0"), anything else refused by
class — used by `format`/`mount` (MAIN:4215, 5933).

Port: the LVM verbs are NOT carried (quirk Q10 — the charter's data plane stripes 4 MiB blocks
across whole namespaces itself (§6.1) and the capacity verbs are `volume {add-data, …}` (§8
L252); an LVM layer under a PR-fenced namespace would also hide the `ng` char node and the
reservation from the daemon). `storage` becomes the host inventory and classifier:
`storage list [--json]` — every NVMe controller (`/sys/class/nvme/*`: transport, state,
subsysnqn, address, hostnqn/hostid), every namespace node (block + `ng` twin, nsid, uuid, size,
RESCAP via Identify where root), and per node the squeezefs CLASS read from sector 0
(`SQZMETLV` metadata member → set uuid/member; `DataSuperblockWire` → `vol_tag`/`set_uuid`;
blank; foreign) — the `pv_is_squeezefs_backed` question answered by the superblock, never by an
NQN prefix; `storage validate <path>` — the `validate_backing_device` ladder plus the superblock
class and, for an NVMe namespace, the `ng` twin and RESCAP (the `format`/`mount` preflight
calls the same function).

---

## 7. Key handling beyond what exists

### 7.1 What is landed (transform.zig; `data_transform_config.md`)

The wrap `hkdf-sha256/aead-kw/v2` (DK:30-40: `PRK = Extract(salt 32 B, material ≥ 32 B)`, `KEK
= Expand(PRK, "squeezefs:key-wrap:v2:kek", 32)`, `key_id = Expand(PRK, "…:key-id", 8)`, blob
`"SK" ‖ 0x02 ‖ nonce(12) ‖ seal(KEK, nonce, aad = key_id, data_key)` = 63 B) — transform.zig
L703-767; the key-file laws on the FD (`O_RDONLY|O_NOFOLLOW|O_CLOEXEC`, statx: regular, `mode &
0o077 == 0`, owner ∈ {euid, 0} for every euid (decision 0002 row 120 vs K:380-388), size ≤ 64
KiB, trimmed ≥ 32 B — L839-875, K:334-403); stdin `-` bounded at 64 KiB + 1 (L878-895,
K:315-328); the resolution order flag → `SQUEEZEFS_ENCRYPT_KEY_FILE` → `/etc/squeezefs/keys/
<key_id>.key` (L908-922, K:436-480, DK:93-98) with the derived-id verification "a wrong key file
is a loud refusal at mount instead of an AEAD failure on every read" (L933-937, K:470-479);
`hardenProcess` = `prctl(PR_SET_DUMPABLE, 0)` + `RLIMIT_CORE = 0` on every key-touching call,
logged once (L948-958, K:270-297); `format --encrypt … --encrypt-key PATH|-` (format.zig
L191-201; args.zig L258-259 the two pairing refusals, MAIN:4139-4170); `mount` resolves with
`flag_path = null` (mount.zig L658: "`mount` grows `--encrypt-key` with the data plane").
`set_config` carries `{key_scheme u8 = 2, kdf_salt [32]u8, key_id [8]u8}` (decision 0002 row
123); the salt is `Io.randomSecure` and a failure REFUSES the format (row 119 vs K:209-222's
`fastrand` + clock fallback).

### 7.2 What rung 8 adds

1. **`mount --encrypt-key <path|->`** (MAIN:154-157, 641): the flag path threads into
   `resolveVolumeKey`'s `flag_path`; `--encrypt-key` on a plaintext set REFUSES ("nothing would
   be encrypted — re-run without the flag", K:533-540, mirroring format's pairing law).
2. **The pre-fork read** (MAIN:2871-2890, VAL-3): with `--daemon`, the material is read in the
   INVOKING process BEFORE `daemon.detach`'s first fork (daemon.zig L176-181), so `-` (stdin)
   works for daemonized mounts and a bad key file fails on the caller's console, not over the
   ready pipe; the material crosses the forks IN MEMORY (never argv, never the pipe); the parent
   `secureZero`s its copy immediately after the fork returns (the reference clears it after
   closing the pipe's write end, MAIN:2931-2933 "the child owns the key material now; the
   waiting parent must not keep a copy alive"); the intermediate child (C1, daemon.zig L216-221)
   zeroizes too. `hardenProcess` runs in the parent before the read and again in C2 (the
   reference re-issues it on every call "a latch would silently skip re-hardening a forked
   child whose flag was cleared", K:266-269).
3. **The key store** `/etc/squeezefs/keys/` (K:34): the reference only PRINTS the install hint
   at format ("place the same key file at /etc/squeezefs/keys/<id>.key", MAIN:4152-4161) and
   never manages the dir. Port verbs: `key install <sqmeta-uri> <path|->` — reads the material
   through the ONE reader, derives against the set's salt, REFUSES on a key-id mismatch, then
   writes `/etc/squeezefs/keys/<key_id>.key` atomically (`.tmp` 0600 root-owned → `fdatasync` →
   `rename`; the dir created 0700; an existing file with identical bytes is a no-op, a different
   one refuses without `--force`); `key show <sqmeta-uri>` — prints scheme, key id, salt hex,
   and WHICH source would resolve now (never material, never the KEK; `derive` runs and the
   result is zeroized); `key check <sqmeta-uri> [--encrypt-key …]` — the mount's resolution
   without a mount (exit 0 iff the id matches). `config show` already prints `encryption` and
   `key_id` (config_verb.zig L43). `tune` has no key surface (tune.zig is the sysctl/sysfs
   parameter verb).
4. **Zeroize audit points** (DK:111-112 "everything that holds material is `Zeroizing`"): the
   64 KiB material buffer (`defer secureZero`, L929-930 ✓); the `DerivedKey` after the data plane
   copied the KEK (mount.zig L666-668 "the caller zeroizes this copy right after" ✓); the session
   data key and the pre-wrapped blob in the crypto state at `deinit` (transform.zig L970-983 —
   audit the teardown path); the pre-fork stash in BOTH parents (new); the format path's
   `material_buf` (format.zig L191 — audit); nothing ever logs material (`log` lines carry the
   key ID and the source path only ✓); `/proc/self/cmdline` carries a PATH (✓ by construction);
   `madvise(MADV_DONTDUMP)` on the material page and `mlock` of the material buffer + the
   `DerivedKey` + the session key (≤ 80 KiB, inside the default `RLIMIT_MEMLOCK`) — the port
   addition with its WHY on the line: swap can spill what `PR_SET_DUMPABLE` protects from a core.
   `MADV_DONTFORK` is NOT applied: the daemon fork must inherit the material (item 2).

### 7.3 Compatibility posture (DK:114-131)

The reference refuses a pre-KW-1 (RSA-wrapped) encrypted volume with the copy-off/reformat
remedy (K:487-522) and keeps a `RedactedSecret` legacy field so the refusal can name a
cleartext-key config (K:64-84, DK:88-91). The port has NO legacy arm (decision 0002 row 129;
format v4 carries `key_scheme u8 = 2` and any other value refuses "newer binary / unknown
scheme" — the same text K:437-443 uses for a foreign scheme string).

### 7.4 Rotation — none in the reference; owed later

`grep rotate` finds only the job-fabric `rotate-enroll` secret (config_ops.rs:611) — the DATA
key is per mount SESSION (DK:43-45 "a random 32-byte data key per mount session, its wrap blob
carried in every block header"), so rotating the OPERATOR key means re-wrapping the blob in
EVERY block header: a defrag-class mover (charter §8 "ONE mover primitive") that rewrites each
block's 96 B `TransformFrameWire` header under a second `key_id`, with `set_config` carrying a
`key_id_prev` for the two-key window and the resolver accepting either id until the sweep
completes. The design's only rotation-adjacent note is the additive `0x03` public-wrap posture
(DK:56-60). Not rung 8; recorded so nobody invents a cheaper rotation that leaves old headers
readable under the retired key.

---

## 8. Knobs, gauges, constants

### 8.1 Knobs (registry rows; class per `knobs.Class`)

| Knob | Kind / range / default | Class | WHY / source |
|---|---|---|---|
| `SQUEEZEFS_NVMEOF_STATE_DIR` | str, default `/var/lib/squeezefs/nvmeof` | `config` | the ledger's home; a RELOCATION seam the fidelity tier points under its own root (L:61-65; D:375) |
| `SQUEEZEFS_NVMET_PORT_ID_BASE` | int `1..=64536`, default 53000 | `derived_override` | the reserved range base (N:52-55); u16 ids (§0) so `base + 999 ≤ 65535`; the fidelity tier sets 54000 |
| `SQUEEZEFS_HOSTNQN` / `SQUEEZEFS_HOSTID` | str, pair-or-neither | `config` | landed (knobs.zig L266-267; EK:142-143) |
| `SQUEEZEFS_PR_REGISTRANT_CAP` | int `1..=65535`, unset = learn (nvmet: unbounded) | `derived_override` | a DECLARED cap for a vendor array (R:249-253, EK:420); `pr.CapLearner.effective(declared)` already takes it (pr.zig L92-94) — the row is owed since rung 6 |
| `SQUEEZEFS_ENCRYPT_KEY_FILE` | str | `config` | landed (knobs.zig L225) |
| `SQUEEZEFS_TEST_FIDELITY_ROOT` | str | `test_seam` | the fidelity harness's state root (`/build/fideli` default); never read by product code — the harness only |
| NOT carried: `SQUEEZEFS_NVMEOF_TARGET_STACK` (EK:388), `SQUEEZEFS_NVMEOF_RUN_DIR` (EK:390), `SQUEEZEFS_SPDK_TGT_BIN` (EK:392) | — | — | quirk Q1: one stack exists; a knob whose only live arm refuses a retired value is dead code (charter §1.9) |

### 8.2 Gauges / counters (always exported, zero when inert — decision 0002 row 200)

| Name | Kind | Source | Must-stay-0? |
|---|---|---|---|
| `fabric_controllers`, `fabric_ctrl_not_live` | gauge | F:263-273 | no |
| `fabric_ctrl_reconnects` | counter (sampled transitions; undercounts — F:294-303) | F:304-341 | no |
| `fabric_host_scoped` | gauge `{0, 1, 2 absent}` | §3.3 | no |
| `fabric_host_identity_source` | gauge `{0 explicit, 1 etc_nvme, 2 machine_id}` | §3.2 | no |
| `fabric_connects`, `fabric_connect_failures`, `fabric_disconnects` | counters | §3.1/§3.5 | no |
| `writer_guard_pr_reacquires` | counter | gate.zig L502-503 | no — expected across nvmet restarts (charter §8) |
| `writer_guard_fenced`, `volume_fenced` | counter / latch | decision 0002 row 183 | no (a fence is a real event) |
| `pr_registrants_per_namespace`, `pr_report_bytes`, `pr_registrant_cap`, `pr_registrant_cap_refusals`, `pr_own_stale_recoveries`, `pr_ptpl_requested`, `pr_rescap`, `pr_rtype` | per-client fields | pr.zig L373-375 | `pr_registrant_cap_refusals` **yes on nvmet** ("0 by construction", EK:420) |
| `pr_registrant_shared` | gauge | pr.zig L364-371 | no (a guarantee-class row) |
| `nvmeof_ledger_generation` | gauge (CLI) | §2.2 | no |
| `nvmeof_configfs_write_failures` | tripwire (per site label: attr path) | §1.2 | **yes** in the fidelity tier |

### 8.3 Constants with their WHY

| Constant | Value | WHY |
|---|---|---|
| `NVMET_CONFIGFS_ROOT` | `/sys/kernel/config/nvmet` | the kernel's mount point (N:62) |
| `PORT_ID_RANGE` / `PORT_HASH_SLOTS` | 1000 / 900 | "the hash spreads over 900 slots; the last 100 are pure probe headroom" (N:57-59) |
| `PORT_ID_BASE_DEFAULT` | 53000 | disjoint from the dev-substrate 52026 and the rig 52470/52471 (D:358) |
| fidelity slice | 54000–54099 | disjoint from the product range (SUB:33-37) |
| `NVMF_NQN_SIZE` | 223 | the NVMe spec's NQN maximum; the kernel's field is 256 with the NUL |
| `OWNERSHIP_NQN_PREFIX` | `nqn.2026-10.io.squeezefs:share-` | default share NQNs; CLASSIFICATION only — "ownership is ledger membership, never a prefix" (M:57-62) |
| `HARNESS_NQN_MARKERS` | `[":devsub-", ":fideli-"]` | `adopt` refuses the test fabric by name (M:789-795) |
| default fabric port | 4420 | the IANA NVMe/TCP port (MAIN:1027) |
| namespace index | 1 | one namespace per subsystem; the G2 same-nsid clause holds structurally (D:359) |
| connect settle deadline | `nvme_core.admin_timeout` (60 s default), read from sysfs | a namespace scan is a sequence of admin commands each bounded by it (§3.4) |
| fabric reconnect facts (documented, not owned) | `reconnect_delay` 10 s, `ctrl_loss_tmo` 600 s | D:510 R3 "measured: 10 s cadence, ~10 min to give-up" |
| `T_owner` | 45 s | `lease_clock.T_OWNER_MS` (decision 0002 row 188) — the SIGSTOP leg waits past it |
| key constants | salt 32, KEK 32, key id 8, blob 63, material ≥ 32, file ≤ 64 KiB | K:39-56; DK:36-40 |

---

## 9. Tests (name — one sentence; `root`/`nvmet` where needed)

**Portable (gate):**
- `ledger: encode∘decode = id for every record shape; decoder total under std.testing.fuzz` — the v4 codec (TL:291-417 field-presence rules).
- `ledger: load never writes (a read-only dir survives 1,000 loads byte-identical)` — TL:63.
- `ledger: a leftover .tmp is ignored and reported; the durable file is authoritative` — TL:130.
- `ledger: version 2 refuses "newer binary"; an unknown flag bit refuses` — TL:188, TL:231.
- `ledger: the intent lattice — begin(pending) → finalize(active); finalize from active/removing refuses; mark_removing idempotent; delete after teardown` — TL:419.
- `ledger: begin_share refuses a duplicate subnqn and a duplicate backing_canonical naming the holder` — L:218-250.
- `ledger: two mutators under flock serialize; the generation increments once per write` — TL:554.
- `port alloc: frozen xxh3 first-candidate vectors; the first 900 slots only; wrap through the full range; ownership reuse vs foreign skip; exhaustion names the knob; a relocated base` — TP:47-211.
- `walk + share over the fake configfs tree: the exact attribute sequence and values (allow_any_host, device_path, device_uuid, resv_enable BEFORE enable, port attrs, links)` — TN:96-216, TN:218.
- `share: allow-host wiring (hosts/, allowed_hosts links, attr 0); a '..' host refuses` — TN:246; M:333-353.
- `share: a foreign live duplicate backing refuses with the runbook; a ledgered duplicate names unshare; a live NQN squat refuses` — TN:287-394.
- `share: a foreign port squat is skipped; a shared port is reused and removed last-out` — TN:395-478.
- `unshare: zero residue + ledger delete; vanished objects are a verified no-op` — TN:480-553.
- `restore: every intent state reconciles (removing resumed; pending finalized/GC'd; active no-op/re-established); a mismatched identity conflicts and never clobbers` — TN:555-752.
- `target status counts; loop and tcp port recipes` — TN:754.
- `adopt: the class heuristic; every refusal token; the candidate captures identity/listeners/out-of-range ids; TOCTOU drift aborts and GCs the pending record; a re-probe error propagates; the adopted record round-trips` — TA:247-983.
- `initiator: the connect string base shape; each option singly and combined; the loop shape; --nr-io-queues 0 refuses before anything` — TI:378-569.
- `initiator: the instance read-back parser ("instance=7,cntlid=3")`.
- `initiator: identity resolution — explicit pair; one env half refuses; one /etc/nvme file refuses; the machine-id derivation is deterministic and RFC 4122-shaped`.
- `discovery over fixture trees: non-multipath plain node; multipath head never a path node; mismatched instance follows the subsystem; multi-subsystem picks the matching NQN; honest null; missing roots` — TI:69-260.
- `discovery: controller_identities_for_device; the merged shape; find under identity skips a foreign scoped sibling` — TI:108; `mw_fabric_identity_tests`.
- `ladder verdict: every (plane, record, actual, merged) cell` — I:681-771.
- `disconnect writes delete_controller on every matching controller; --identity-only only on ours` — TI:315.
- `fabric sampler: skips pcie; unknown state is not-live; missing sysfs = zero sample; reconnect = observed not-live→live; renumbering tracked; src_addr churn ignored; same NQN on two addresses distinct` — TF:52-434.
- `status Fabric section: absent for non-fabric; family fields + rows; multipath path nodes and partitions match` — TF:446-515.
- `key: --encrypt-key on a plaintext set refuses; the stdin path through the pre-fork reader; key install refuses an id mismatch and writes 0600 atomically; key show prints no material` — TK:499-576 shape.
- `rails: no std.process.Child in src/ops/nvmeof/*; every knob a row`.

**Linux, root, nvmet (the fidelity tier, §5.3):** each leg above is a named test under `zig
build fidelity`; the unit binary's `pr_linux` real-device test (pr_linux.zig L222) stops
skipping once `substrate-up` ran (it finds `/dev/ng*`).

---

## 10. QUIRKS per decision 0002 (add the rows before diverging)

| # | Reference quirk | Why poor | Better design |
|---|---|---|---|
| Q1 | A `StackKind` enum, `--target-stack`, `SQUEEZEFS_NVMEOF_TARGET_STACK`, `target install`, `SPDK_RESHARE_SEQUENCE`, retired-SPDK ledger arms and residue notes — ≈ 400 lines whose only live behavior is to refuse the word `spdk` (M:62-180, M:446-496, M:571-623, S:14-16; `nvmeof_retire_spdk_tests.rs`) | A retired stack as a grammar; a `stack` field in a one-stack ledger; dead code under charter §1.9 — and the port never had SPDK | One stack, no selector, no `stack` field; the ONLY mention of SPDK is the charter's reason for nvmet (its 16-registrant cap) |
| Q2 | The ledger as tolerant-but-strict `serde_json` with `deny_unknown_fields`, `created_utc` as RFC 3339 text, `ns_uuid` as 36-char text, `Option` fields for another stack's data (L:72-77, S:114-142) | A serialization library as an on-disk format (0002 row 2); text for a 16-byte identity; nullable fields that are never null in the one stack | `LedgerHeadWire` + `ShareRecordWire` (§2.2): canonical, total, checksummed, every count checked before allocation; `list --json` renders |
| Q3 | File backings attached through `losetup -f`/`-j`/`-d` shell-outs and a `loop_device` ledger field; the share prints "loop devices expose no PR ⇒ detection-grade" (N:610-622, N:640-657, N:843-854; D:360) | nvmet serves a regular file directly (`io-cmd-file.c`) and its reservations live in nvmet — the loop device added a shell-out, a kernel object to leak, a ledger field, and a FALSE guarantee note (PR works on a file-backed nvmet namespace exactly as on a block one); `losetup -j` output parsing | `device_path ← <file>`; no loop, no field, no note; the NoCOW guard stays (nvmet's file backend opens `O_DIRECT`: the btrfs CoW wedge NC:34-50 applies verbatim) |
| Q4 | `modprobe nvmet nvmet-tcp`, `mount -t configfs`, `modprobe nvme-tcp` and nvme-cli as shell-outs (N:329-346, I:877-891); `modules_present = /sys/module/nvmet exists` (N:1211) | The venue has none of the binaries; `/sys/module/nvmet` is absent for a built-in; a `Command::new("nvme").arg("--version")` probe per connect | `mount(2)` for configfs; readiness = the configfs `nvmet` dir; transports autoload through the kernel's `request_module`; the connect writes `/dev/nvme-fabrics` always (§1.6, §3.1) |
| Q5 | FNV-1a 64 for the port-id first candidate (N:67-74) beside xxh3 everywhere else | Two hash functions in one tree for one probe start; the frozen vectors exist only to keep the ids stable, and the ids live in the ledger anyway | xxh3-64 (`core/checksum.zig`) with the port's own frozen vectors |
| Q6 | `SQUEEZEFS_NVMET_PORT_ID_BASE` admits `0..u32::MAX` (EK:391; N:284-291 parses `u32`) while the kernel names ports with `kstrtou16` | A base above 64536 creates no port and the refusal comes from `mkdir EINVAL`, far from its cause | `1..=64536`, u16 ids throughout (§1.4) |
| Q7 | `connect` discards the fabrics device's `instance=N,cntlid=M` answer and finds "the" device by a global `subsysnqn` walk with a 100 ms sleep-poll for 2 s (I:934-940), then a second identity-scoped poll of 5 s (I:592-607, MAIN:5848) because the global walk "is satisfied by a scoped sibling's head instantly" | Sleep as synchronization; a second association on the same NQN (another mount, the fence leg) can be returned as ours | Read `instance=N` back on the same fd; resolve UNDER `/sys/class/nvme/nvme<N>`; verify the namespace by `uuid`; wait for the uevent with the `admin_timeout` deadline (§3.4) |
| Q8 | `get_host_nqn`/`get_host_id` WRITE `/etc/nvme/{hostnqn,hostid}` on first read (I:67-69, I:87-89); 0002 row 187 deferred the decision | A mount silently mutating host configuration | Never write; explicit pair → `/etc/nvme` BOTH files → a deterministic machine-id derivation, the source published (§3.2) |
| Q9 | `remove_configfs_object` falls back to `remove_dir_all` on `ENOTEMPTY` "for the injected-root unit tier, where attrs are real files" (N:184-212), guarded by the VAL-7g component screen because `..` once recursed into the configfs PARENT (M:320-330) | A recursive delete in product code that exists only for a test double; a path screen standing between a test fallback and live kernel state | The fake configfs is an in-process tree (no real files), `rmdir` is the only removal, the NQN screen stays as grammar |
| Q10 | `storage pool/volume` as an LVM wrapper (`pvcreate -y -ff`, interactive confirms, `losetup -d` inside `pool delete`, PVs classified by the stale `nqn.2026-06` prefix — ST:19-38, 178, 365, 516-532) | `-ff` re-initializes whatever disk is named; an LVM layer hides the namespace (no `ng` twin, no PR) from the daemon; a prefix as a classifier; binaries the product does not own | `storage list` / `storage validate` over sysfs + the superblocks (§6); capacity verbs are `volume {add-data, …}` |
| Q11 | `fabric_endpoint:` as a bespoke length-framed record on ino 1 (LIB:1114-1187) beside `writer_claim`/`job:` | A second home for a set-level fact the `set_config` record already owns per volume (0002 rows 121, 125) | A `FabricEndpointWire` per data volume inside `set_config` (§3.7) |
| Q12 | `report_gauges` as a `Mutex<BTreeMap<PathBuf, …>>`, the cap learner as process statics (R:217-268) — already row 191; `nvme_status_error` carried in `io::Error::other` and downcast (R:276-308) | Path-keyed globals; a typed status smuggled through a string error | Per-client fields; `pr.Error` + `last_status` (landed) |
| Q13 | `created_utc` from `SystemTime::now()` through a hand-written civil-from-days (L:444-472) | Civil-date arithmetic in a ledger codec | `created_s: i64` = `clock.realtimeCoarseS()`; the CLI formats |
| Q14 | The fidelity tier asserts through `nvme`, `jq`, `losetup`, `ip`, `uuidgen`, `sensors` (FID:72, FID:772-834, FID:1181) | The product's own faces exist for every assertion (`resv-report`, `list --json`, `.stats`); the venue lacks the tools | §5.2: every assertion through product verbs read by `core/json_scan.zig`; `--pr-matrix` replaces the nvme-cli PR legs |
| Q15 | `--ns-uuid` accepted as a flag only to parse-and-refuse `--nsid ≠ 1` beside it (M:229-250; MAIN:1034-1039 `hide = true`) | A hidden flag kept so a refusal can be "ours" | No `--nsid` (the index is a structural constant); `--ns-uuid` stays (restore identity seeding) |

Not quirks (keep as designed): the write-ahead intent lattice and record-before-mutate on both
verbs; load-never-writes; atomic rename + dir fsync; forward-only with the version gate ahead
of field errors; one namespace per subsystem; `resv_enable` and `device_uuid` before `enable`;
ownership = ledger membership (never a prefix); the never-touch-foreign law and last-out port
removal; the live-state duplicate guard whose refusal is the runbook; `adopt` writing only the
ledger with a TOCTOU re-verify and the harness-owned refusal; the register ladder's fail-closed
arms and "unregister only OWN stale keys"; FID 0x81 as the ONLY registrant match authority;
the two-step REGCTL read; the heartbeat re-check as the PTPL heal; `EBADE` as the fence; the
pair-or-neither identity law; the strict `^nvme\d+n\d+$` shape rule and "never fabricate a
device name"; the sampled-transition reconnect counter with its stated undercount; the
dir-scoped serving-controller membership; the key wrap, the key-file laws on the FD, the
resolution order, the id verification, and `PR_SET_DUMPABLE`/`RLIMIT_CORE` before any material
is read.
