# Rung 8 wave A3 — NVMe-oF target management over kernel nvmet, the initiator, the real PR legs, the fidelity tier, `storage`, the key store

Spec: `docs/porting/ops_nvmet_keys.md` (§0 venue facts, §1 configfs recipe, §2 ledger, §3
initiator, §4 the real PR legs, §5 the fidelity tier, §6 `storage`, §7 keys, §8–§10). Charter §8
"NVMe-oF targets", §3 (host-scoped fabric subsystems), §5.5 "Fencing", §5.7 B1, §6.5, §11 (the
fidelity tier is a release gate). Decision 0003 §G; decision 0002 rows 380–394. Modules:
`src/ops/nvmet.zig` + `src/ops/nvmet/{stats, configfs, ledger, initiator, verbs, storage, pr_legs,
run, fidelity}.zig`, `src/ops/keys.zig`; the admin-queue split in `src/meta/guard/pr_linux.zig`;
the two-phase gate in `src/meta/guard/gate.zig` + `src/meta/backend.zig`; `FabricEndpoint` in
`src/data/set_config.zig`; the `nvmet` stats family; 4 knob rows (`SQUEEZEFS_NVMEOF_STATE_DIR`,
`SQUEEZEFS_NVMET_PORT_ID_BASE`, `SQUEEZEFS_PR_REGISTRANT_CAP`, `SQUEEZEFS_TEST_FIDELITY_ROOT`); 4
errno rows (`EADDRNOTAVAIL`, `ECONNREFUSED`, `EHOSTUNREACH`, `EALREADY`); the `nvmet-smoke` gate
step.

## What landed

- **configfs driven directly** (`configfs.zig`): `preflight` mounts configfs with `mount(2)` when
  absent (EBUSY = mounted) and judges readiness by the `nvmet` dir (not `/sys/module/nvmet`, absent
  for a built-in target); the recipe in the measured order — `attr_allow_any_host` → `namespaces/1`
  → `device_path` (a regular file: nvmet's file backend, no loop device) → `device_uuid` →
  `resv_enable=1` → `enable=1` → the port object → the `ports/<id>/subsystems/<nqn>` link that
  enables the port; teardown in the mirror order with last-out port removal and host dirs removed
  only when no other record names them; `AttrLocked` for a post-enable identity write (the kernel
  answers EBUSY for `device_path`/`device_uuid` and EINVAL for `resv_enable`, row 391); a refused
  port link is loud, never a tripwire (an operator's address); `nvmet_configfs_write_failures` on
  every attribute the recipe expected to succeed. Port ids: `base + xxh3(key) % 900` + a linear
  probe through `[base, base+999]`, u16 (`base ≤ 64536`), ownership = ledger membership through
  the `Ownership` face; the live walk (nsids, `allowed_hosts`, `device_path`/`device_uuid`/`enable`,
  listeners off the port snapshot); `target status`. The `FakeFs` is an in-process tree with the
  kernel's semantics (default groups, attribute locks, the first-link port enable, EBUSY on a
  linked target) so the EXACT op sequence is pinned by a string compare.
- **The share ledger** (`ledger.zig`): `LedgerHeadWire` 64 B / `ShareRecordWire` 48 B + framed
  tail / `ListenerWire` 24 B, xxh3 over `head[0..56] ‖ body`; decode total under fuzz, every
  count checked against the bytes present before any allocation; `version > 1` refuses "newer
  binary" before any field check; the value laws at decode and at `beginShare`; the lattice
  `beginShare` (duplicate subnqn OR canonical backing refused, any state) → `finalizeShare` (from
  pending only) → `markRemoving` (idempotent) → `delete`; `Store.load` never writes (a leftover
  `.tmp` logged and ignored); `Store.mutate` under `flock(LOCK_EX)` → tmp (0600) → the io_uring
  file layer's write → fdatasync → rename → fsync(dir), the pool ONE worker (the fd-cache
  reasoning on the module doc). Home `/var/lib/squeezefs/nvmeof/` (host-scoped: `restore` runs at
  boot before any set is mountable).
- **The verbs** (`verbs.zig`, `run.zig`): `share` (grammar → backing → the live duplicate guard
  whose refusal IS the runbook → port ids over the snapshot → the pending record → the recipe →
  active), `unshare` (the record is the authority; an unledgered NQN refuses naming `adopt` and
  the manual configfs steps), `list` (+`--json`: shares with classification + live, `foreign_live`,
  `connected_fabric_disks`), `restore` (per-record outcomes collected, the verb red over any
  failure), `adopt` (the live walk → the candidate → the five refusal tokens → the pending record →
  the walk AGAIN → TOCTOU drift aborts and GCs), `target {status, setup, systemd-unit, stop}`.
- **The initiator** (`initiator.zig`): the exact option string (tcp + loop shapes), the write to
  `/dev/nvme-fabrics` with its errno table through the ONE table, `instance=N,cntlid=M` read back on
  the same fd, the uevent-driven readiness wait (a `NETLINK_KOBJECT_UEVENT` socket opened BEFORE the
  write; every event re-checks sysfs; deadline = `nvme_core.admin_timeout`), the namespace VERIFIED
  by `uuid` under `/sys/class/nvme/nvme<N>` (+ the `ng` twin through `nvme-generic`), identity
  pair-or-neither → `/etc/nvme` both-or-neither → the machine-id derivation (never written),
  `fabrics_host_scoped_subsystems` read and announced, disconnect by `delete_controller` (+
  `--identity-only`), the `fabric_*` sampler (sampled transitions, undercounting by design), the
  §5.2 ladder verdict.
- **The real PR legs** (`pr_legs.zig`; `pr_linux.zig` routes admin passthru to the controller
  node — row 392): `nvmeof resv-report <dev> [--json] [--pr-matrix --peer <dev> [--force]]`.
- **`storage list|validate`** (`storage.zig`): the inventory + the sector-0 class, never LVM.
- **Keys** (`keys.zig`): `SecretBuf` (mlock + `MADV_DONTDUMP`, zeroized on deinit), `mount
  --encrypt-key <path|->` with the PRE-FORK stash (the invoking process reads; C1 scrubs after C2
  exists; the parent's `defer` zeroizes after the fork returns; the daemon derives against the
  set's salt and zeroizes the stash), `--encrypt-key` on a plaintext set refused, `key install`
  (id-verified, 0600 tmp → rename, identical = no-op, different needs `--force`), `key show` (no
  material, the resolving source), `key check`.
- **`set_config.FabricEndpoint`** (the PROPOSED §3.7 record, row 389) + `config
  set-fabric-endpoints` / `get-fabric-endpoints`.

## The fidelity tier (`zig build nvmet-smoke`; the venue, root; 7.2.6-sqz, nvmet built in)

Every leg through the binary (`./squeezefs` in the cwd), zero mocks, the harness marker
`:fideli-`, the port-id range 54000–54999, the ledger under `/build/fideli-<pid>/state`, a
zero-residue census per leg (no subsystem, port link, record or controller of the leg's NQNs
survives). The venue container has an EMPTY `/etc/machine-id`, so the first identity is an
explicit pair (the derived default refuses there, by design).

| Leg | loop | tcp (127.0.0.1) | What it asserted |
|---|---|---|---|
| `roundtrip` (G1) | PASS | PASS | share (`resv_enable == 1`, the ledger's `ns_uuid == device_uuid`, `list` managed+live) → connect (the namespace's sysfs `uuid` == the record) → 64 MiB O_DIRECT round trip through `uring_fs` on `/dev/nvmeXn1` → `resv-report` (RESCAP ≠ 0, regctl 0) → disconnect → unshare → zero residue, the port removed last-out |
| `pr-matrix` | PASS (9/9) | PASS (9/9) | see the PR table below; tcp adds: a registration survives the holder's DISCONNECT (reconnect under the same identity: `regctl 1`, the key present) |
| `fidelity-mount` (THE row) | PASS | PASS (`guard-tcp`) | meta + data shares → connect both → `format --single-writer sqmeta:///dev/nvmeAn1 sqdata:///dev/nvmeBn1` → mount: `writer_guard_mode flock+pr`, `pr_rescap ≠ 0`, `pr_rtype 1`, `pr_registrants_per_namespace 1`, `resv-report` holder == the mount's `pr_key`; 8 MiB written + synced; a second mount under a SECOND identity (its own connect) exits 1 naming the single-writer guard; `kill -9` → remount: `pr_own_stale_recoveries 1`, `writer_guard_fenced 0`, the payload's cksum intact; `umount` → `regctl 0`, `rtype 0` |
| `sigstop-preempt` | PASS | — | mount A; SIGSTOP; > T_owner (45 s); mount B under identity B: `writer_guard_preempts 1`, B reads A's file and writes; SIGCONT A: A's next beat reads the usurpation — `writer_guard_fenced 1`, the latch verdict `fenced` (FAILED by the fence class), A's writes answer EIO; B keeps writing. Found and fixed row 393 (the ring-recovery cover ran before the arbitration: B's first bset append answered `-EBADE`) |
| `g2-restore-tcp` | — | PASS | the subsystem wiped by hand under a live mount → `list` down → `restore` restored with the recorded uuid → the controller reconnects → `writer_guard_pr_reacquires 1`, `writer_guard_fenced 0`, payload intact → a second `restore` is a verified no-op |
| `crash-window` (law 6) | — | PASS (127.0.0.1 + TEST-NET-1) | W-N1: the 192.0.2.55 link fails `EADDRNOTAVAIL` after the namespace is live; the PENDING record claims both ids; a second share of the backing refuses naming the pending holder; `restore` finalizes; `unshare` removes both ports. W-N2: pending → wiped → `restore` garbage-collects and sweeps the port shell |
| `adopt` | A1/A2/A3 | — | a hand-built foreign share (port id 7) connected, listed foreign, adopted with configfs UNCHANGED (walk before == after), the record carries the live uuid + the out-of-range id, re-adopt `[adopt_already_ledgered]`, unshare removes port 7; the ledger file deleted → a product-prefixed share adopts as `ledger_loss` → `restore` no-op; a `:fideli-` NQN `[adopt_harness_owned]` |
| `pr-registrants` | 64 identities | — | 64 loop connects (`--nr-io-queues 1`), each registering through its `/dev/ng`: `regctl 64`, `report_bytes 4160 = 64 + 64 × 64` (past the 4 KiB a fixed read truncates at 63), cap 0; drained to 0 |
| `loudfail` + `storage-list` | — | — | an unledgered unshare, `--nr-io-queues 0` (no controller created), `target stop`, a missing backing, a `../x` host — each names its remediation; `storage list --json` names the connected namespace with class `meta`; `storage validate` prints the class, the `ng` twin and RESCAP; `target status` counts |

### The PR legs proven on the kernel target (`resv-report --pr-matrix`, identical on loop and tcp)

| Leg | Result |
|---|---|
| RESCAP | `0xfe`: WE, WERO advertised; PTPL NOT (nvmet) |
| EDS first | the 64 B extended report decodes; the short form answers SC `0x018` Host Identifier Inconsistent Format — pinned as the fallback's trigger |
| register fast path + idempotent same key | regctl 1 |
| spec-strict ladder | a2's plain register under a1's host CONFLICTS (0x83); the ladder unregisters exactly a1 (own stale) and leaves b1 (foreign); `pr_own_stale_recoveries 1` |
| two-identity WE fence | B's 4 KiB O_DIRECT write through `/dev/nvmeBn1` answers `-EBADE` (unregistered AND registered under A's WE); B preempts A; B's writes land; A's next write `-EBADE` |
| WERO rtype 3 (`fencing.acquireWero` + `joinAsRegistrant` on two real associations) | rtype 3; the registrant joined (regctl 2, cap 0) and WROTE; unregistered writes refused before the join and after leaving |
| preempt-and-abort | a plain preempt on nvmet: the victim's registration gone, the hold stands (rtype 1, regctl 1) |
| release + unregister | regctl 0, rtype 0 |
| shared identity | another rkey under OUR wire host id reads shared on A's association, not on B's |

## Found and fixed on the real target

1. **Admin passthru on the namespace node is `-ENOTTY`** (row 392): Identify and FID 0x81 go to
   `/dev/nvme<N>`; `pr_linux.Device` carries both fds.
2. **The ring-recovery cover before the arbitration** (row 393): `Guard.prepare` / `Guard.commit`.
3. **`resv_enable` refuses EINVAL, not EBUSY, once enabled** (row 391).

## Gates (this commit, rebased on `a7d2943`)

- Venue (7.2.6-sqz, root, quiet venue): `zig build gate` 38/38 steps, 2338/2377 tests passed
  (39 skipped) — unit 2143 pass / 37 skip, smoke 59 pass / 2 skip, bench-smoke 39, shim-smoke 39,
  shim-census 1, rails 7, **nvmet-smoke 50 pass**; zero nvmet residue after (no subsystem, port or
  controller). The fidelity tier alone: `All 47 tests passed` in ~3.5 min.
- macOS (portable modules): `zig build gate` 22/22 steps, 1862/1881 tests passed (19 skipped);
  the nvmet modules compile only on Linux (`src/squeezefs.zig`), `keys` and the pure codecs run
  on both hosts.
- The `cli.smoke` rows flake (`appender clear`, `dd: Software caused connection abort`) ONLY while
  a second gate runs on the same venue (`pkill -9 -x squeezefs` is global); the same rows pass
  alone and in this quiet run.

## Owed

- The daemon-owned data-plane connect under an explicit identity (the ladder's `daemon_connect`
  arm is decided and tested pure; the connect itself and `disconnect --identity-only` at clean
  unmount are not wired into `mount`).
- Key rotation (§7.4): a defrag-class mover; never a cheaper rotation.
