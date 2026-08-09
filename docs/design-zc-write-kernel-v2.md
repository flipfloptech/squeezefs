# Kernel zc-write v2 — implementation specification for the sqz kernel series

**Status:** SPEC — ready for an independent implementer. No code from this
document exists yet; the series in both tracks ends at patch 0027.
**Authority:** ruling D13 (2026-08-06) — custom-kernel work is fully
sanctioned ("anything is OK in the realm of a custom KERNEL or custom
KERNEL requirements for it to work"); the sqz kernel series is a
first-class product surface. Portability law governs CPU/topology only,
never kernel version.
**Companion documents:** `docker/kernel-sqz/SERIES.md` (series manifest,
transplant record), `docker/kernel-sqz/V2-CANDIDATES.md` (the 2026-08-04
scoping campaign this note extends), `docs/design-device-overlay.md`
(Approach B — §8 carries the retention accelerator's charter and its
future read-synchronization law, quoted verbatim in §3.6 below),
`docs/rc-manifest.md` §3f (the write-bandwidth adjudication that ordered
this work).
**Evidence note (create with the work):**
`.benchmarks/2026-08-09-kernel-zc-write-v2.md`.

---

## 0. What this document is

The write-bandwidth program (rc-manifest §3f) runs three tracks in
parallel: Approach A (fd-backed placed-merge assembly, daemon-side,
running), Approach B (device-backed visible overlay, design Rev 2,
implementation-ready), and **this kernel track**. The kernel track's
deliverables, in priority order:

| # | Deliverable | Class | Section |
|---|---|---|---|
| 1 | **Abort-race soundness fix** — page references for zero-copy registrations | MANDATORY, blocks #2 | §2 |
| 2 | **Payload retention** (`0029`-class) — COMMIT keep-payload + explicit release | BUILD — Approach B's ACK-early accelerator | §3 |
| 3 | **Selective zc delivery** — REGISTER-time eligibility mask | CONDITIONAL — build only if the pricing arithmetic clears (§4.3) | §4 |
| 4 | **Fixed-copy opcode** | PARKED — design appendix only, do not build | Appendix A |

Scope boundary (hard): **kernel patches only.** The userspace halves
(fuse3 fork negotiation, daemon retention leases, probe-ladder rungs)
are SPECIFIED here (§6) but NOT built by this track — `crates/fuse3/`
and the daemon are owned by the running Approach A campaign until it
merges. The implementer's terminal deliverables are: the patches in both
series directories, compile proofs on both trees, the regenerated
manager-ready concatenation, updated `SERIES.md`, and a boot-test plan.
**A kernel that needs booting is a user checkpoint — deliver the plan
and stop there** (the field host and the local CachyOS box are both
user-managed boot surfaces).

## 1. The two tracks and the series layout

Two parallel patch series exist and **every patch in this charter lands
in both**:

| Track | Directory | Base | kmbuf opcodes | Runs on |
|---|---|---|---|---|
| 6.19 (field) | `docker/kernel-sqz/patches/` | linux-6.19.14 | REGISTER=37 / UNREGISTER=38 | `squeeze-test` fleet, `6.19.14-sqz` |
| 7.1 (dev) | `docker/kernel-sqz/patches-7.1/` | linux-7.1.6 | REGISTER=38 / UNREGISTER=39 (upstream 7.1 took 37 for `IORING_REGISTER_BPF_FILTER`) | local dev box, `7.1.6-1-cachyos-sqz` |

Both series currently end at `0027`. Target layout after this charter
(numbers are the `git format-patch` export order; regenerate, do not
hand-edit filenames):

```
0001–0024   unchanged (kmbuf infra, FUSE refactors, bvec rsrc, zero-copy)
0025        NEW  fuse/io_uring: hold page references for zero-copy
            registrations (§2 — inserted IMMEDIATELY after 0024: it
            corrects 0024/0023's lifetime contract before anything
            builds on it)
0026        was 0025 (docs)
0027        was 0026 (sqz kbuf error-path seam)
0028        was 0027 (sqz FUSE_TIME_LIMITS)
0029        NEW  sqz: fuse-uring zc payload retention (§3)
0030        NEW  sqz: fuse-uring selective zc delivery (§4 — ONLY if
            the §4.3 gate clears; otherwise this slot does not exist)
```

Renumbering discipline: 0025–0027 (old) are docs/kbuf/inode.c patches
with **no hunk overlap** against the new 0025 (which touches
`fs/fuse/dev_uring.c`, `io_uring/rsrc.c`, `include/linux/io_uring/cmd.h`)
— verify with `patch --dry-run --fuzz=0` after reordering; any fuzz is a
red flag, stop and rebase properly. `build-kernel.sh` applies the series
with `--fuzz=0` and will fail loud on a bad renumber. Record the
renumber and both new patches in `SERIES.md` (it is the manifest of
record — same register as the existing patch-26/27 entries: what, why,
conflict sites, message-id lineage where applicable).

Prior-references note: earlier planning shorthand called selective
delivery "0028". This document supersedes that numbering — selective is
**0030** (conditional); retention keeps its long-standing **0029** name.

## 2. Patch 0025 — abort-race soundness fix (MANDATORY)

### 2.1 The defect, precisely

`fuse_uring_set_up_zero_copy()` (patch 0024, `fs/fuse/dev_uring.c`)
builds a `bio_vec` array from the request's folios and registers it as a
fixed buffer in the daemon's io_uring:

```c
for (i = 0; i < ap->num_folios; i++) {
        total_bytes += ap->descs[i].length;
        bvs[i].bv_page = folio_page(ap->folios[i], 0);
        bvs[i].bv_offset = ap->descs[i].offset;
        bvs[i].bv_len = ap->descs[i].length;
}
err = io_buffer_register_bvec(ent->cmd, bvs, ap->num_folios,
                              total_bytes, ddir, ent->fixed_buf_id,
                              issue_flags);
kfree(bvs);
```

`io_buffer_register_bvec()` (patch 0023, `io_uring/rsrc.c`) copies the
bvec array into an `io_mapped_ubuf` via
`io_kernel_buffer_init(ctx, nr_bvecs, total_bytes, dir, NULL, NULL,
index)` — **NULL release callback, NULL priv, and no page references
taken anywhere**. The registered table holds bare `struct page`
pointers whose lifetime is owned entirely by the FUSE request's folios.

Three windows make that unsound:

1. **Connection abort / daemon termination.**
   `fuse_uring_entry_teardown()` → `fuse_uring_stop_fuse_req_end()` →
   `fuse_request_end()` ends a zero-copied request **without calling
   `io_buffer_unregister()`** (the teardown path has no `issue_flags`
   uring-cmd context, and `ent->zero_copied` is never consulted there).
   The folios are released to their owners (application GUP pages or
   page cache); the daemon's ring — which may outlive the FUSE
   connection, since teardown is triggered by abort as well as by ring
   death — still holds a fixed-buffer table entry pointing at freed
   pages. Any in-flight or subsequent `READ_FIXED`/`WRITE_FIXED`
   against that slot is a use-after-free DMA.
2. **In-flight I/O across COMMIT.** Even on the normal path,
   `fuse_uring_req_end()` unregisters and then ends the request — but
   `io_buffer_unregister()` only removes the table slot; an in-flight
   fixed-buffer op that already resolved its `io_rsrc_node` keeps the
   `io_mapped_ubuf` alive and continues DMA against pages the request
   just released. Today this is "safe by protocol discipline" (a
   correct daemon COMMITs only after its own I/O completed) — which is
   not a soundness argument, merely an accident of the current daemon.
3. **Retention (§3) is impossible without the fix.** Keeping the slot
   registered past `fuse_request_end()` is the entire point of 0029;
   with bare pointers, retention converts window 2 from a race into the
   steady state.

### 2.2 The fix

Key the page lifetime to the `io_mapped_ubuf` itself, using the release
mechanism patch 0022 ("Allow buffer release callback to be optional")
already provides:

1. **io_uring half** (`io_uring/rsrc.c`, `include/linux/io_uring/cmd.h`):
   extend `io_buffer_register_bvec()` with `void (*release)(void *)` and
   `void *priv` parameters, passed through to `io_kernel_buffer_init()`
   (which already accepts and stores them — patch 0022's machinery; the
   ublk-facing `io_buffer_register_request()` uses exactly this shape).
   The release callback fires when the imu's final reference drops —
   i.e. after the table slot is gone AND every in-flight op's rsrc node
   has been put. Update the one existing caller (fuse) and the
   `-EOPNOTSUPP` stub signature.
2. **fuse half** (`fs/fuse/dev_uring.c`): in
   `fuse_uring_set_up_zero_copy()`, take a reference per folio
   (`folio_get()` — the request's folios are guaranteed live at this
   point, so bare gets are safe) **before** registration, and pass a
   release callback that puts them. The callback needs the folio set
   after the request may be gone, so allocate a small carrier
   (`{ nr; struct folio *folios[]; }`) — the carrier is the `priv`, the
   callback puts each folio and frees the carrier. On
   `io_buffer_register_bvec()` failure, put the references and free the
   carrier on the error path (the callback will never fire).
3. **Teardown audit** (the window-1 closure): with references held by
   the imu, the missed-unregister on the teardown path no longer causes
   use-after-free — the pages live until the daemon's ring drops the
   slot (unregister, ring exit, or ctx free). That converts window 1
   from memory corruption into a bounded page-pin leak on abort, which
   the ring's own death then releases. This is the designed end state:
   **page lifetime follows the io_uring registration, request lifetime
   follows FUSE, and neither trusts the other's ordering.** Do NOT
   attempt to add an `io_buffer_unregister()` call to the teardown path
   itself — it requires a uring-cmd issue context teardown does not
   have; the reference fix is the whole fix.

`ent->zero_copied` handling in `fuse_uring_req_end()` is unchanged
(unregister at COMMIT stays — 0029 modifies that, not this patch).

### 2.3 Constraints and verification

- **GFP discipline:** carrier allocation is `GFP_KERNEL_ACCOUNT`
  (matches the existing `bvs` kcalloc site).
- **No behavior change for non-zc queues** — the entire patch is inside
  `can_zero_copy_req()`-gated code plus the rsrc signature change.
- **Compile proof, both trees** (§5). Zero new warnings.
- **Runtime proof (7.1.6 local, user-booted checkpoint):** extend
  `docker/kernel-sqz/probes/kmbuf_smoke.c` with an abort-race rung —
  arm a zc queue, deliver a paged request, kill the mount daemon
  (SIGKILL) while a `READ_FIXED` against the slot is parked, assert no
  oops/KASAN splat (the probe README documents running it under a
  KASAN-enabled dir-build where available). This is the red-first
  repro: on the unfixed kernel under KASAN it must splat; on the fixed
  kernel it must not.
- **Commit message** carries the three-window analysis from §2.1
  verbatim-in-substance, and names patch 0022 as the mechanism donor.
  Attribution: sqz-authored fix to the Koong series (same register as
  patch 0026's seam-commit rationale).

## 3. Patch 0029 — payload retention (ACK-early accelerator)

### 3.1 Charter

Today, `fuse_uring_req_end()` unregisters the zc pages at COMMIT —
ACK-after-DMA: the daemon must finish every device operation that reads
the payload before it commits the FUSE reply. Retention inverts that
for writes: **the daemon may COMMIT (ACK the write to the application)
while the slot's pages remain registered**, and DMA from the retained
sparse-slot bvecs later, releasing the slot explicitly when done.

This is Approach B's accelerator (design-device-overlay §8): it moves
ACK latency off the device round-trip, it does not move bytes. It is
explicitly **not a prerequisite** for any B rung — every B rung is
correct and measurable with ACK-after-CQE. Stock kernels keep
ACK-after-CQE forever (negotiation, §3.5).

Dependency: **0029 requires 0025.** After `fuse_request_end()` the
folios' owners drop their references; only the imu-held references from
0025 make a retained slot's pages live. The commit message states this
dependency explicitly.

### 3.2 uapi surface

All additions to `include/uapi/linux/fuse.h`; run the §5.2 collision
audit on BOTH trees before freezing values.

```c
/* enum fuse_uring_cmd — next free value after COMMIT_AND_FETCH = 2 */
FUSE_IO_URING_CMD_RELEASE_PAYLOAD = 3,

/* fuse_uring_cmd_req init flags (bits 0/1 taken by BUF_RING/ZERO_COPY) */
#define FUSE_URING_PAYLOAD_RETENTION   (1 << 2)

/* new member of the fuse_uring_cmd_req union, selected by the
 * COMMIT_AND_FETCH opcode (init selects the init member): */
struct {
        uint16_t flags;
} commit;

/* commit flags */
#define FUSE_URING_COMMIT_RETAIN       (1 << 0)
```

Layout law: `struct fuse_uring_cmd_req` must not grow past its current
size (it lives in the 80-byte SQE128 command area; the union absorbs
the new member — adjust `padding[]` accordingly and add a
`static_assert`/BUILD_BUG_ON on the struct size in `dev_uring.c`).
Update the uapi version-history comment in the same register as the
series' existing "7.46" entry.

### 3.3 Semantics

**Arming.** A queue accepts `FUSE_URING_PAYLOAD_RETENTION` in
`init.flags` only when `FUSE_URING_ZERO_COPY` is also set (retention of
a copied payload is meaningless); refuse `-EINVAL` otherwise, through
the same error arm as the existing zc validation
(`fuse_uring_buf_ring_setup()`). Store `queue->use_retention : 1`.
The re-REGISTER consistency check (`queue->use_bufring != use_bufring`
etc.) gains the retention bit.

**COMMIT with RETAIN.** On `FUSE_IO_URING_CMD_COMMIT_AND_FETCH` whose
`commit.flags` carries `FUSE_URING_COMMIT_RETAIN`:

- Valid only when the queue is retention-armed AND the committing ent
  has `ent->zero_copied` AND the registration direction was
  `ITER_SOURCE` (a write payload — record the direction on the ent at
  registration time; a `bool zc_dir_source : 1` beside `zero_copied`).
  Any violation fails the commit with `-EINVAL` (loud, per the series'
  existing commit-path error discipline). Read-direction retention is
  refused by design: a retained READ dest has no consumer once the
  reply is committed.
- The request ends normally (`fuse_request_end()` — the application's
  write(2) returns) but `io_buffer_unregister()` is **skipped** and the
  ent transitions to a new state `FRRS_RETAINED` instead of fetching
  the next request. The COMMIT_AND_FETCH cmd stays pending (no CQE) —
  **the ent's re-arm is deferred exactly like a §5.4 payload lease**
  (design-device-overlay §8 uses these words; this is their kernel
  face).
- Pin accounting is therefore **structural, not counted**: one retained
  slot = one ent out of the fetch pool. Retained slots per queue are
  bounded by `zero_copy_depth` by construction, and retained bytes by
  `depth × ring->max_payload_sz`, with no new accounting variable to
  get wrong. A daemon that retains everything simply starves its own
  queue — its problem, correctly priced.

**RELEASE.** `FUSE_IO_URING_CMD_RELEASE_PAYLOAD` is a new uring_cmd on
the same queue fd, addressed by `commit_id` (the same lookup COMMIT
uses):

- Ent found in `FRRS_RETAINED`: `io_buffer_unregister()` the slot
  (dropping the table entry; 0025's release callback frees the pages
  once in-flight I/O drains), transition the ent back into the fetch
  path — the parked COMMIT_AND_FETCH cmd proceeds to
  `fuse_uring_get_next_fuse_req()` exactly as an un-retained commit
  would have. The RELEASE's own cmd completes immediately with 0.
- `commit_id` unknown: complete `-ENOENT`.
- Ent found but not `FRRS_RETAINED`: complete `-EBUSY`.
- Double release is therefore structurally refused (`-ENOENT` — the
  first release moved the ent out of `FRRS_RETAINED` and recycled the
  commit_id space); the distinct errno per cause is deliberate — the
  daemon's tripwire counters key on them, and `-EINVAL` is reserved for
  "opcode unknown" so the negotiation probe (§3.5) stays unambiguous.

**Teardown/abort drain.** `fuse_uring_teardown_entries()` gains a pass
over `FRRS_RETAINED` ents (a `queue->ent_retained` list beside
`ent_in_userspace` / `ent_avail_queue`): teardown treats them like
userspace-held ents whose request already ended — no
`fuse_uring_stop_fuse_req_end()` (there is no req), the parked cmd is
completed `-ECONNABORTED` through the existing IO_URING_F_CANCEL-safe
path, and the pages survive until the ring itself releases them (0025's
law). A retained slot can never outlive the daemon's ring, and never
strands a FUSE request.

**Lost-release policy.** The kernel does NOT carry a timer. A leaked
retention is bounded (≤ depth × payload per queue), visible (the ent
pool shrinks — the daemon's own fetch starvation is the symptom), and
reclaimed at teardown. The watchdog is the DAEMON's (§6.3 — the
`transport_lease_overlong` precedent: loud-never-fatal). The kernel
half adds one ratelimited `pr_warn` when teardown drains a nonzero
retained population — the "daemon lost releases" forensic line.

### 3.4 The stability law (record in the patch, own by the daemon)

After COMMIT-with-RETAIN, write(2) has returned: the application
legally owns its buffer again and may modify it, while the retained DMA
has not yet sampled the pages. The bytes that land are sampled at DMA
time within the retention window — **unstable-write semantics** (the
NFS UNSTABLE/COMMIT class). Kernel-side this is a documentation duty
only; the enforcement lives in the daemon program:

- **Page-cache writeback folios**: sound by composition — a post-ACK
  modification is a redirty, a redirty is a new WRITE covering the
  page, and the overlay's newest-wins generation law
  (design-device-overlay §4/law 6) supersedes the retained store's
  range. fsync ownership: design-device-overlay §6.2 step 2's COMPLETE
  arm must await retained stores the kernel believes are already done —
  that restatement is chartered to the accelerator's daemon PR, not to
  this kernel patch.
- **O_DIRECT / GUP user pages**: NOT sound in general — a buffer reuse
  without a covering re-write persists scribbled bytes. The daemon may
  only ACK-early such writes under an explicit opt-in posture
  documented as unstable-write semantics. The kernel patch neither
  knows nor cares which class a request is; it states the law in its
  commit message and `Documentation/filesystems/fuse-io-uring.rst`
  (patch 0026's file) so the daemon PR has a contract to cite.

The read-side half of the law is already recorded — quote it verbatim
in the patch's documentation hunk (design-device-overlay §8):

> *A read intersecting an ACKed-but-incomplete store must wait for that
> store or serve from its retained source; it may never treat the range
> as an uncovered old-map gap.*

### 3.5 Negotiation (no init-flag echo exists — probe by opcode)

`fuse_uring_register()` ignores unknown init-flag bits, so a daemon
cannot detect retention support by setting bit 2 (an old kernel accepts
and silently ignores it — the worst failure class). The negotiation is
therefore an **opcode probe**, extending the existing kmbuf probe
ladder (`crates/fuse3/src/raw/connection/kmbuf.rs` +
`docker/kernel-sqz/probes/kmbuf_smoke.c`): after arm, issue
`FUSE_IO_URING_CMD_RELEASE_PAYLOAD` with an impossible `commit_id` on
qid 0 —

- `-ENOENT` → opcode exists → retention supported (the errno split in
  §3.3 exists precisely for this).
- `-EINVAL`/`-EOPNOTSUPP` → opcode unknown → stock/old-sqz kernel →
  the daemon never sets bit 2 and keeps ACK-after-CQE.

The probe rung is a userspace deliverable (§6.1) — this section is its
kernel-side contract; keep the errnos stable.

### 3.6 What 0029 does NOT do

- No read-direction retention (refused, §3.3).
- No ACK-early decision-making — the kernel provides the mechanism;
  whether any given write ACKs early is entirely the daemon's charter
  (design-device-overlay §8's accelerator PR).
- No change to non-zc queues, kmbuf-only queues, or the classical
  sideband. A retention-armed queue with zero RETAIN commits behaves
  bit-identically to a zc queue (pin this in the smoke probe).

## 4. Patch 0030 — selective zc delivery (CONDITIONAL)

### 4.1 The problem it would solve

`can_zero_copy_req()` is `armed ∧ (in_pages || out_pages)` — on an
armed queue, EVERY paged request rides zc: a 4 KiB read pays
`kcalloc` + `io_buffer_register_bvec` + table churn + `io_buffer_unregister`
(two `io_ring_submit_lock` round-trips) to avoid a 4 KiB memcpy, and
forces the daemon to serve via ring RW ops on the slot instead of its
warm-tier memcpy fast paths. Upstream's own numbers (patch 0024's
commit message) are 1 MiB-shape wins; nothing established the small-op
crossover.

### 4.2 The shape (if built)

REGISTER-time, per-queue — never per-request daemon round-trips:

- init flag `FUSE_URING_ZC_SELECTIVE (1 << 3)` + two init fields
  (inside the existing union/padding budget, same size law as §3.2):
  `uint16_t zc_min_kb` (payload floor, KiB; 0 = no floor) and
  `uint8_t zc_dir_mask` (bit 0 = writes/`ITER_SOURCE`, bit 1 =
  reads/`ITER_DEST`).
- `can_zero_copy_req()` gains the mask/floor test (request payload size
  is available at that point via the last in/out arg — the same
  arithmetic `fuse_uring_args_to_ring()` uses for `payload_sz`).
- A request failing the mask delivers exactly as on a zc-disarmed
  queue (kmbuf copy path) — patch 0024's
  `fuse_uring_req_has_copyable_payload()` split already handles mixed
  populations per-request, which is what makes this patch small.

### 4.3 The build gate (do not skip)

Build 0030 **only if** per-op arithmetic from the vehicle ledgers
prices it in: take the measured small-op zc cost (register+unregister
cycles from a counted bracket on the 7.1.6 box — perf-annotate the two
`io_ring_submit_lock` sites under a 4 KiB fio row, armed vs disarmed)
against the memcpy it displaces. If the crossover lands below the
smallest payload our armed mounts actually deliver (the daemon already
class-gates writes via `zc::hold_candidate()` — size < payload/2,
`crates/fuse3/src/raw/connection/zc.rs:330`), **file the refusal
arithmetic in the evidence note instead of building the patch** — that
is a successful outcome of this charter, not a failure. The refusal
must state the measured crossover and the delivered-shape floor it
cleared.

## 5. Cross-cutting duties

### 5.1 Compile venues and proofs (every patch, both tracks)

- **7.1 track:** dir-build in `/tmp/sqz-kpatch-test/linux-7.1.6`
  (present; refetch:
  `curl -sfLO https://cdn.kernel.org/pub/linux/kernel/v7.x/linux-7.1.6.tar.xz && tar xf`)
  against the running config (`zcat /proc/config.gz > .config`,
  `make olddefconfig`), then `make io_uring/ fs/fuse/` — exit 0,
  **zero new warnings** (diff the warning set against a pre-patch
  build, not against silence).
- **6.19 track:** the containerized recipe —
  `docker/kernel-sqz/build-kernel.sh` applies the series `--fuzz=0`
  (any renumber mistake fails at apply) and builds the full RPM set via
  `docker/kernel-sqz/build.sh`. A full container build is the
  acceptance compile for the field track.
- Regenerate the manager-ready concatenation for the local box:
  `~/sqz-kmbuf-zc-7.1.6-v2.patch` (the CachyOS kernel-manager consumes
  a single concatenated patch — same recipe that produced
  `~/sqz-kmbuf-zc-7.1.6.patch`).

### 5.2 uapi collision audit (the 37-vs-BPF precedent)

Before freezing ANY new constant, diff the relevant uapi headers of
BOTH base trees (`include/uapi/linux/fuse.h`, and
`include/uapi/linux/io_uring.h` if anything io_uring-visible is added):
verify `FUSE_IO_URING_CMD_RELEASE_PAYLOAD = 3`, init bits 2/3, and the
commit-flags bit are free in both 6.19.14 and 7.1.6, and record the
audit (grep output, both trees) in the evidence note. The precedent:
kmbuf REGISTER=37 collided with upstream 7.1's
`IORING_REGISTER_BPF_FILTER` and forced the per-track 38/39 renumber —
constants may legitimately DIFFER per track if a collision exists;
the probe ladder, not the constant, is the source of truth for
userspace (`kmbuf.rs` `KMBUF_OPCODES_SQZ_619`/`_SQZ_71` is the
pattern). FUSE-side values are expected to be identical on both tracks
(the fuse uapi diverges far less than io_uring's) — verify, don't
assume.

### 5.3 Probe extension

Extend `docker/kernel-sqz/probes/kmbuf_smoke.c` with:

1. the §2.3 abort-race rung (red on unfixed+KASAN, green on fixed);
2. a retention round-trip rung: arm with bit 2, deliver one paged
   write, COMMIT+RETAIN, verify the source pages stay readable through
   a `READ_FIXED` on the slot, RELEASE, verify second RELEASE returns
   `-ENOENT` and a RELEASE on a live (un-retained) commit returns
   `-EBUSY`;
3. the §3.5 negotiation rung (impossible-commit_id RELEASE errno
   split), asserted on both a retention kernel (`-ENOENT`) and, where
   available, a pre-0029 kernel (`-EINVAL`).

### 5.4 Boot-test plan (deliver, do not execute)

The final deliverable is a written plan in the evidence note: which box
boots which artifact first (local 7.1.6-sqz-v2 via the kernel manager
before any field RPM), the smoke sequence (probe ladder → mount →
`fuse3_zc_negotiated` pin → one armed fio row), and the rollback line
(the ELRepo/previous kernel stays the grub default until the smoke
passes — the SERIES.md "one-shot grub discipline"). **Booting is the
user's checkpoint.**

### 5.5 Workflow

Branch `feat/kernel-zc-write-v2` off dev. Never push to origin. The
change class is "code/harness touched" for the repo gate ONLY if any
in-repo script/probe changes (`kmbuf_smoke.c`, `SERIES.md` are in-repo:
docs+probe C file — no cargo surface; the full cargo gate is NOT
triggered by kernel patch files, which the workspace does not compile).
Do not touch `crates/fuse3/` or `src/` (Approach A owns them). Commit
per patch (0025, 0029, 0030-or-refusal), each with its compile proof
noted; SERIES.md update rides the last commit.

## 6. Userspace halves — SPEC ONLY (owned by follow-on daemon PRs)

Recorded so the kernel contract has a named consumer; none of this is
the kernel implementer's to build.

1. **Probe rung** (`kmbuf.rs`): a `retention: bool` capability on the
   probe result, discovered per §3.5, surfaced in the mount log beside
   the kmbuf verdict line.
2. **fuse3 retention lease**: COMMIT+RETAIN on eligible armed writes;
   the retained slot becomes a transport-lease-class object (the §5.4
   severance boundary maps onto RELEASE exactly as the design-device-
   overlay §8 accelerator entry states); ent-pool sizing accounts for
   the structural retention bound (§3.3).
3. **Daemon watchdog**: retained-slot age tripwire, loud-never-fatal
   (`transport_lease_overlong` precedent), stats-inode counter family
   for retain/release/refused, and the fsync COMPLETE-arm restatement
   chartered in design-device-overlay §8.
4. **Stability-law enforcement** (§3.4): ACK-early only for
   writeback-class folios by default; O_DIRECT ACK-early behind an
   explicit unstable-write opt-in posture.

## Appendix A — the parked fixed-copy opcode (design-only)

A kernel opcode copying fixed→fixed buffers (sparse zc slot →
registered daemon arena) would let Approach A's assembly skip its
`WRITE_FIXED`-to-memfd hop. It is PARKED by the write-bandwidth
adjudication: it is Approach A's ceiling with more maintenance surface,
and the un-park condition is precise — **A reports engagement-exact but
unconverted, naming the memfd arena hop as the limiting term**. If
un-parked, the shape is a small `IORING_OP_` or uring-cmd taking
(src_index, src_off, dst_index, dst_off, len) resolving both through
`io_uring_fixed_index_get()`-class lookups with a kernel-side copy loop
— no DMA, no page ownership transfer, so 0025's reference law covers
the source side unchanged. Do not build ahead of the un-park signal.
