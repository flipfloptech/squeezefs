# MW multipath kernel fix — host-scoped fabric subsystems (rung 5b)

**Status:** IMPLEMENTED with this document — patch `0030` in both sqz
series tracks (`docker/kernel-sqz/patches-7.1/` authored FIRST per the
2026-08-15 authoring-order ruling, then backported to
`docker/kernel-sqz/patches/`). Compile-verified on both tracks; **NOT
boot-verified** — boot/behavior validation is rung 6b's qemu-guest leg
(no host reboot on the critical path).
**Authority:** ruling D13 (custom-kernel work is a first-class product
surface) + the rung-6 STOP adjudication 2026-08-15 (user: "lets go with
the sqz-kernel fix for sure") + the authoring-order ruling 2026-08-15
(verbatim: "should be built around the latest 7.1.x kernel code we run
locally and then back ported to the 6.19.14 kernel").
**Companion documents:** `docs/design-full-multi-writer.md` §5.2 + PR
rung 5b (the finding and the charter), `docker/kernel-sqz/SERIES.md`
(series manifest — the 0030 entries), `docs/design-zc-write-kernel-v2.md`
(the campaign-shape precedent this note follows).
**Code studied (not paraphrased from memory):** linux-7.1.6
`drivers/nvme/host/{core.c,nvme.h,fabrics.h,fabrics.c,sysfs.c,multipath.c}`
and `drivers/nvme/target/{nvmet.h,pr.c}` from the kernel.org tarball the
7.1 track builds from; linux-6.19.14 from the series work volume.

---

## 0. The defect, precisely (the rung-6 STOP finding)

§5.2's device-enforced co-location tier assumes each per-mount identity
resolves its OWN `/dev` node. On `nvme_core.multipath=Y` kernels (the
upstream **default**, `CONFIG_NVME_MULTIPATH`), that assumption is
false, and the code says exactly why:

`nvme_init_subsystem()` (7.1.6 `core.c:3238`) groups every new
controller into a `struct nvme_subsystem` found by
`__nvme_find_get_subsystem(subsys->subnqn)` (`core.c:3164`), whose
match is **one strcmp on `subsys->subnqn`**:

```c
list_for_each_entry(subsys, &nvme_subsystems, entry) {
        if (strcmp(subsys->subnqn, subsysnqn))
                continue;
        if (!kref_get_unless_zero(&subsys->ref))
                continue;
        return subsys;
}
```

The controller's **hostnqn** (`ctrl->opts->host->nqn`) never enters the
match. Two co-located SqueezeFS mounts connecting to one target
subsystem under two per-mount identities (§5.2 rules 1–2) therefore
land in ONE host-side subsystem; on a multipath kernel each namespace
gets ONE ns_head gendisk (`nvme_mpath_alloc_disk`, `multipath.c:758`:
`"nvme%dn%d", ctrl->subsys->instance, head->instance`) whose per-path
`nvme<X>c<C>n<Y>` nodes are hidden, and the one openable head
round-robins I/O across BOTH identities' paths (the subsystem
iopolicy). Per-mount PR fencing is void: a WERO preempt of registrant A
does not stop the head from routing the "fenced" mount's DMA down
registrant B's path — worse, either mount's I/O may ride either
identity at any instant. Rung 2's refusal ladder already detects and
refuses this shape (a head served by a foreign identity can never
verify as dedicated), which is correct — but a refusal is not a
capability. The product answer is the kernel fix below.

## 1. The fix — `(subsysnqn, hostnqn)` subsystem grouping, opt-in

**One sentence:** when the new opt-in module parameter
**`nvme_core.fabrics_host_scoped_subsystems`** is set, fabric
controllers group into host-side subsystems by the PAIR
`(subsysnqn, hostnqn)` instead of `subsysnqn` alone, so co-located
identities get separate subsystems → separate ns_heads → separate
`/dev/nvmeXnY` nodes, while same-identity multipath (N paths, one
hostnqn) keeps merging exactly as upstream.

### 1.1 Mechanism (patch 0030, both tracks)

Four touch points, all in `drivers/nvme/host/`:

1. **`nvme.h`** — `struct nvme_subsystem` gains
   `char host_scope[NVMF_NQN_SIZE];` immediately after `subnqn`. Empty
   = unscoped (upstream behavior; every subsystem when the param is
   off, and every PCIe subsystem always).
2. **`core.c`** — the param (bool, **perm 0444**: boot/modprobe-time
   only — a runtime flip would make the subsystem match asymmetric
   between subsystems created before and after it, so it is
   deliberately not writable) + one helper:

   ```c
   static const char *nvme_ctrl_host_scope(struct nvme_ctrl *ctrl)
   {
           if (!fabrics_host_scoped_subsystems)
                   return "";
           if (!(ctrl->ops->flags & NVME_F_FABRICS))
                   return "";
           if (!ctrl->opts || !ctrl->opts->host)
                   return "";
           return ctrl->opts->host->nqn;
   }
   ```

   `nvme_init_subsystem()` stamps the candidate's scope
   (`strscpy` right after `nvme_init_subnqn()`), and
   `__nvme_find_get_subsystem()` gains the scope argument and a second
   strcmp. Param off ⇒ every scope is `""` ⇒ the extra strcmp compares
   two empty strings ⇒ **byte-identical grouping to upstream**.
3. **`core.c` `nvme_global_check_duplicate_ids()`** — the load-bearing
   second half. Splitting one fabric subsystem into host-scoped
   siblings puts the SAME target namespace (same nguid/uuid/eui64)
   under TWO host-side subsystems, and the global duplicate-ID sanity
   check (`core.c:3980`) would then hit its fabric arm in
   `nvme_init_ns_head()` — `"ignoring nsid %d because of duplicate
   IDs"` — and the second identity's namespace would never appear.
   Identical IDs across host-scoped siblings of one subsysnqn are the
   EXPECTED shape (they ARE the same target namespace), not a
   collision, so the walk skips exactly that shape:

   ```c
   if ((this->host_scope[0] || s->host_scope[0]) &&
       !strcmp(s->subnqn, this->subnqn))
           continue;
   ```

   The `host_scope[0]` guard keeps the stock walk literally unchanged
   when the param is off (all scopes empty — including the
   discovery-subsystem case, where same-subnqn subsystems exist even
   upstream because `__nvme_find_get_subsystem` fails discovery
   matches). Two same-subnqn subsystems where at least one is scoped
   can only exist BECAUSE of scoping (an unscoped and a scoped
   subsystem with one subnqn cannot coexist with the param constant
   for the module lifetime, but the disjunction is kept so the skip is
   correct even for that theoretically-unreachable shape). The
   **within-subsystem** duplicate check
   (`nvme_subsys_check_duplicate_ids`) is untouched.
4. **`sysfs.c`** — a read-only subsystem attribute **`sqz_host_scope`**
   (empty line when unscoped). This is the rung-6b observability
   handle (`cat /sys/class/nvme-subsystem/nvme-subsysN/sqz_host_scope`)
   and the forward-looking daemon key: the daemon can distinguish
   scoped siblings without it (the subsystem dir's controller LINKS
   are the membership — `nvme_init_subsystem()`'s
   `sysfs_create_link`), but an explicit attribute makes the guest
   validation script and any future resolution arm one `read_to_string`
   instead of a link walk. The `sqz_` prefix marks it non-upstream.

### 1.2 What deliberately does NOT change

- **Same-identity multipath**: N controllers, one `(subsysnqn,
  hostnqn)` pair → one scoped subsystem → one head, ANA + iopolicy
  untouched. The fix splits identities, never paths.
- **PCIe controllers**: scope is always `""` (no `opts`); grouping
  unchanged even with the param on.
- **Discovery controllers**: `__nvme_find_get_subsystem` already fails
  every discovery match (each discovery controller gets a unique
  subsystem); the scope strcmp is a no-op on an already-failed match.
- **`nvme_validate_cntlid`**: runs per (now scoped) subsystem. Within
  a scope it is upstream-verbatim; across scopes two controllers can
  now carry equal cntlids without tripping the duplicate-cntlid
  refusal — which is correct, because the target hands each connect
  its own cntlid and the two scopes are deliberately independent
  host-side views of one target.
- **Head/disk naming**: each scoped subsystem takes its own instance
  (`dev_set_name(&subsys->dev, "nvme-subsys%d", ctrl->instance)`), so
  the second identity's head is a distinct `nvme<X>n<Y>` — exactly the
  per-identity `/dev` node §5.2 assumes.

### 1.3 Connect-time semantics (duplicate-controller detection)

Verified in 7.1.6 `fabrics.h:181`: `nvmf_ctlr_matches_baseopts()`
**already** compares `opts->host->nqn` and `uuid_equal(&opts->host->id,
…)` alongside `subsysnqn`, and `nvmf_ip_options_match()`
(`fabrics.c:1219`, used by tcp/rdma `create_ctrl`) builds on it. So
duplicate-connect detection is ALREADY host-scoped upstream: two
connects differing only in hostnqn create two controllers today (that
is exactly how the merged-head shape arises). **The patch needs no
connect-path change** — under the scoped key, a re-connect with the
same `(subsysnqn, hostnqn, addr)` still dedups into the existing
controller (`-EALREADY` absent `duplicate_connect`), and a connect with
a new hostnqn creates a new controller that now lands in its own
subsystem instead of the shared one. No new connect option is added;
the opt-in is the module parameter alone (weighed in §2, alternative D).

### 1.4 The PR-state argument (why host-side splitting cannot fork fencing)

Reservation state lives per-**namespace-object at the TARGET**, keyed
by registrant **hostid**, and the host-side subsystem topology is
invisible to it. Verified in 7.1.6 target code: `struct nvmet_pr`
(enable, generation, holder, `registrant_list`) is embedded in `struct
nvmet_ns` (`drivers/nvme/target/nvmet.h:76/:129`), and every lookup is
`nvmet_pr_find_registrant(pr, &ctrl->hostid)` (`pr.c:28`) — the
target-side controller's hostid, carried at connect. Two host-side
scoped subsystems over one target namespace therefore read and mutate
ONE `nvmet_pr` instance; a WERO preempt of identity A's registrant is
enforced against every path A holds, regardless of which host-side
subsystem the path's controller sits in. The SPDK target is the same
shape by the fidelity tier's standing evidence (PR/PTPL matrix in
`tests/run_nvmeof_fidelity.sh` runs per-namespace against both stacks).
Host-side grouping is a pure VIEW decision; fencing truth stays at the
device. (This is also why the fix is sufficient: with separate heads,
each mount's DMA rides only controllers registered under its own
identity, so device-enforced rejection regains its meaning.)

### 1.5 Consequences accepted and recorded

- **`/dev/disk/by-id` aliasing**: scoped siblings expose the same
  nguid/uuid through two heads, so udev's by-id links for the two
  heads collide (last-processed wins). Accepted: the sqz fleet resolves
  devices by the sysfs controller/subsystem walk under identity
  (`src/nvmeof/initiator.rs`), never by-id; the param is opt-in and
  documented for SqueezeFS fleet/guest configs, not general-purpose
  hosts. Recorded in the patch body.
- **The global duplicate-ID skip narrows a sanity check**: between
  same-subnqn scoped siblings only. Cross-subsystem collisions between
  UNRELATED subsysnqns keep the full upstream check.
- **hostid aliasing is out of scope**: grouping keys on hostnqn per the
  rung charter. A same-hostnqn/different-hostid pair is registrant
  aliasing the daemon already refuses at identity resolution
  (pair-or-neither, KD-MW-3) — the kernel does not defend against it.

## 2. Alternatives weighed (all rejected)

| # | Alternative | Verdict |
|---|---|---|
| A | **Unconditional fabrics host-scoping** (no param — always group by the pair) | REJECTED. It silently changes stock multipath semantics for every fabric user of the sqz kernel: an operator who deliberately connects one subsystem under two hostnqns for path diversity (legal, if unusual) would lose head merging with no way back. The sqz kernel is also the daily-driver CachyOS/EL8 kernel (D13), not a single-purpose appliance image — opt-in keeps the stock contract by default and makes the fleet posture an explicit boot-line fact the rig/guest config owns. Cost of the param: one strcmp of two empty strings on the (cold) subsystem-create path — nothing. |
| B | **Per-path block nodes** (un-hide `nvme<X>c<C>n<Y>` gendisks, mount opens its own path node) | REJECTED. Upstream hides path nodes on purpose (`multipath.c` makes them `GENHD_FL_HIDDEN`); exposing them re-opens the pre-multipath dual-node confusion for EVERY nvme user (udev storms, by-id collisions on ALL paths, fsck/mount tools seeing N duplicates), needs the daemon to grow a path-node resolution arm that upstream actively refuses to stabilize, and still leaves the HEAD live as a foot-gun (I/O through the head keeps round-robining across identities — the void-fencing shape survives, one open(2) away). The fix must remove the merged head, not add escape hatches around it. |
| C | **Char-dev passthrough** (`/dev/ng*` generic nodes / uring passthrough, bypassing the block head) | REJECTED. `/dev/ng<X>n<Y>` per-controller char nodes do exist per-path, but the entire SqueezeFS block plane (`NvmeBlockDev` io_uring block workers, discard/write-zeroes ladder, `zcrx_lane` probe, devsub substrates) speaks block-layer semantics; moving the data plane to NVMe passthrough is a product-wide rewrite with its own reservation/rq accounting, not a multipath fix — displacement of that scale demands counted A/B evidence, not a topology workaround (2026-08-01 ruling). |
| D | **Connect option instead of / beside the module param** (`nvme connect ... host_scoped=1`) | REJECTED for v1. A per-connect knob makes the grouping key AMBIENT STATE OF THE CONNECT MIX: a scoped connect and an unscoped connect to one subsystem would have to answer "does the unscoped one match the scoped subsystem?" — order-dependent grouping, exactly the asymmetry class the 0444 param perm exists to forbid. One boot-scoped switch keeps every subsystem on one law for the module lifetime. (A fabrics option can be layered later if a mixed-policy host ever materializes; nothing in the on-disk/uapi surface constrains it.) |
| E | **`nvme_core.multipath=N` as the product answer** (per-controller namespace nodes, no heads) | REJECTED as product posture, KEPT as the documented stock-kernel workaround. It fixes the merge but globally: it disables native multipath for EVERY nvme device on the host (including real multi-path fabrics the fleet may legitimately run), and it is exactly the stock-kernel remedy the upgraded rule-2 refusal names (`fix/mw-multipath-refusal`). The sqz param keeps same-identity multipath alive. |

## 3. Compatibility posture

- **Default OFF in the patch** — a `7.1.6-sqz`/`6.19.14-sqz` kernel
  boots byte-identical to its unpatched self on every nvme surface
  (the param default is `false`; the added strcmp compares empty
  strings; the dup-ID skip guard is `host_scope[0]` = false; the new
  sysfs attr reads empty).
- **Enabled via cmdline in the sqz guest/fleet config**:
  `nvme_core.fabrics_host_scoped_subsystems=Y` goes on the rung-6b
  qemu guest kernel cmdline and, later, the mw fleet rig's guest
  config — never on general-purpose hosts by default.
- **No uapi, no Kconfig, no on-disk surface**: the patch adds a module
  param + one sqz-prefixed sysfs attr. Nothing for the probe ladder;
  stock kernels are detected by the DAEMON's rule-2 sysfs walk (the
  merged shape is visible in `/sys/class/nvme*`), not by a kernel
  feature probe — which is why the refusal upgrade
  (`fix/mw-multipath-refusal`, §5) ships independent of the kernel.
- **Portability law (D13)**: kernel-version-dependent by sanction;
  degrades LOUD on stock kernels via the upgraded refusal naming both
  remedies.

## 4. The 6.19.14 backport — every adaptation

The 7.1 patch was authored first (authoring-order ruling) and
backported. Touched-region differences found by side-by-side read:

| Site | 7.1.6 | 6.19.14 | Adaptation |
|---|---|---|---|
| `nvme_init_subsystem` alloc | `kzalloc_obj(*subsys)` | `kzalloc(sizeof(*subsys), GFP_KERNEL)` | context-only (not in a hunk) |
| `nvme_init_subsystem` id copy tail | ends at `subsys->cmic = id->cmic;` | carries an extra `subsys->awupf = le16_to_cpu(id->awupf);` | the `strscpy` hunk anchors on `nvme_init_subnqn(...)` + the serial/model memcpys (identical on both trees), so the hunk is line-offset-only |
| `struct nvme_subsystem` | no `awupf` | `u16 awupf; /* 0's based value. */` after `vendor_id` | `host_scope` inserts after `subnqn` (identical context both trees) — offset-only |
| helpers between find and validate | 7.1 adds `nvme_admin_ctrl`/`nvme_is_io_ctrl` | absent | context outside hunks |
| `__nvme_find_get_subsystem` / `nvme_global_check_duplicate_ids` / `sysfs.c` attrs | — | identical code | hunks apply with offsets only |

Both patches carry the base-track note in the commit body (the
SERIES.md 0025–0029 precedent: every adapted patch states its
adaptations; here the adaptation set is offsets-only, stated as such).

## 5. The daemon half shipped with this rung (`fix/mw-multipath-refusal`)

Stock `multipath=Y` kernels keep rung 2's refusal, upgraded to NAME the
shape and both remedies. `src/nvmeof/initiator.rs` gains the
multipath-merged detection (`multipath_merged_shape_at`, riding the
same `controller_identities_for_device_at` sysfs injection seam):

- **The shape**: the device is a subsystem HEAD (it lives under a
  `/sys/class/nvme-subsystem/<subsys>/` dir) whose SERVING controllers
  carry **> 1 distinct hostnqn**.
- **Serving-controller membership is subsystem-dir-scoped**: when the
  subsystem dir carries controller entries (`nvme<N>` — the kernel's
  `sysfs_create_link` membership), exactly those controllers are the
  serving set; only a link-less dir falls back to global
  subsysnqn-attr matching. This is what keeps the detector HONEST on
  the sqz kernel: two host-scoped sibling subsystems share a subsysnqn
  by design, and an attr-matched walk would read every dedicated
  scoped head as "merged".
- **The refusal** (rule 2's arm in `fabric_ladder_verdict`, fired ahead
  of the generic foreign-identity text when the merged shape is
  present) names the head, the subsysnqn, the distinct hostnqns, and
  both remedies verbatim: the sqz kernel's
  `nvme_core.fabrics_host_scoped_subsystems=Y` and stock
  `nvme_core.multipath=N`.
- Without explicit identity nothing changes (the ladder's accept-all
  law); the guarantee-row gauges keep reading actual identities.

Deliberately NOT in this rung: teaching
`find_device_for_nqn_under_identity` to resolve scoped-sibling heads on
an sqz kernel (its `foreign_serves` computation is subsysnqn-attr-
global and will read a scoped sibling as foreign). That resolution arm
lands with rung 6b's guest validation, where it can be proven against
the real scoped sysfs shape instead of a hand-built fixture.

## 6. Validation plan (rung 6b — qemu guest, no host reboot)

1. Guest kernel = the sqz build (either track) with
   `nvme_core.fabrics_host_scoped_subsystems=Y` on the cmdline; host
   runs the tcp devsub target (port slice 54100–54199).
2. **Grouping proof**: two `nvme connect` invocations to ONE subnqn
   with two hostnqn/hostid pairs → assert TWO
   `/sys/class/nvme-subsystem/` entries, each `sqz_host_scope` reading
   its pair's hostnqn, each with its own `nvme<X>n<Y>` head; repeat a
   same-identity second connect → still one subsystem (multipath
   preserved, two `nvme<X>c<C>n<Y>` paths under one head).
3. **Param-off control** (same guest, `=N` or absent): the two-identity
   connect merges into one subsystem — and the upgraded refusal names
   the shape + both remedies when a mount walks it.
4. **Fencing proof**: registrant preempt of identity A (the
   `guard_smoke.sh` verbs) while B's head keeps serving; A's next
   journal barrier fail-stops — the §5.2 device-enforced row, now real
   on one box with multipath=Y.
5. **N=1 regression row**: single explicit-identity mount on the scoped
   kernel and on stock — end-to-end unaffected (the rung charter's
   closing requirement).
6. Duplicate-ID skip proof: `dmesg` carries NO `"ignoring nsid …
   duplicate IDs"` for the scoped siblings; both heads carry the same
   nguid by design.

**This rung delivers compile evidence only** (both tracks, §SERIES.md
entries); a kernel that needs booting is a user checkpoint — rung 6b
owns the boots.
