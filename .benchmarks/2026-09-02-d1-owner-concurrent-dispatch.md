# 2026-09-02 — D-1: the S8 owner dispatches a frame's verbs concurrently (F-A)

**Branch** `perf/d1-owner-concurrent-dispatch` (worktree off dev
`98346dab` — A1 exact-sum histograms + A2 trace ring in). RED `cf97ed03`
→ fix `9a463db7`. Campaign: `docs/design-e2e-perf-audit.md` §3 board
**#1** / Appendix D **F-A** (the DLM ledger's first structural finding);
contracts `tests/meta_ship_owner_dispatch_tests.rs`.

**Time-boxed session** (host reboot at ~17:20 EDT): everything below
the "Measured" line is in-process; the fleet field row is **owed** with
its recipe in §Owed.

## The conviction (read-only, dev tip 98346dab)

`MetaShipService::run_batch` (`src/meta_ship/service.rs:673-690`):

```rust
for op in ops {
    out.push(self.run_op(client_epoch, &client_id, op).await);
}
```

Every mutating verb's `execute_inner` ends in `commit_tx`, which
enqueues on the volume's M7 conveyor and parks on a oneshot until the
pass task drains, writes ONE journal entry and barriers. The loop awaits
that before starting the next verb, so a frame of N verbs — which the
client side builds by coalescing N *independently in-flight* callers
(`router.rs` drain: "N verbs in flight to one owner cost one round
trip") — pays **N conveyor passes, serially**. The conveyor's whole
design (D5: union leaf locks, one write, one barrier per batch) was
structurally unreachable from the owner's serve path: the committers
never overlapped.

The ledger's arithmetic: 9,473 verbs/s per authority = 1 ÷ the owner's
serial per-verb latency (≈ 106 µs on the fleet). The in-process row
below measures that per-verb term at **127.7 µs** on a debug build over
a file-backed sandbox — same shape, same order of magnitude.

## The fix — dependency chains, concurrent across objects

`run_batch` partitions the frame with `dependency_chains(&ops)` — a
union-find over `MetaCall::named_inos()` overlap (rename names both
parents, link its inode + the new parent, so a two-object verb fuses
chains). One chain = the ops that transitively name a common inode,
executed **serially in submission order** inside its own
`SHIP_CLIENT`/`SHIP_REVOKES` task-local scopes (rung 12's mutation
gate keeps reading the mutator's identity; per-chain revoke scopes keep
one chain's surrenders out of a sibling's reply). Chains dispatch
concurrently on the sqz-meta pool (`spawn_meta_join` — the venue that
owns the backend's tasks, per the module's handoff law), and results
are re-slotted into op order. A single-chain frame takes the serial
form in-task (byte-identical to before).

What is preserved, and why it is preserved BY CONSTRUCTION rather than
by care:

* **In-batch causality** ("a create and a lookup of the same name in
  one frame see each other") is the same-parent case — one chain.
  Production frames coalesce independently in-flight callers, each
  awaiting its own reply, so no caller can name an inode a same-frame
  verb has not yet minted; the only cross-op dependency that can exist
  is through a NAMED inode, which is exactly the relation.
* **Children an unlink/rename DISCOVERS** under guards are not in the
  relation — the client cannot see them, so no same-frame op can depend
  on them, and two ops racing such a child through its own ino are the
  4a-guard race two local tasks already have.
* **Reply order and id correlation** — results are slotted by op index.
* **Dedup window** — per op, unchanged; a replayed frame's duplicates
  await the winner's `OnceCell` whichever chain the winner ran on.
* **One tx = one checksummed journal entry** — untouched; the row
  asserts entries per frame stays N. What collapses is the PASS count.
* **Era / not-owner / grace gates** — frame-level, before dispatch,
  unchanged.
* **Unwind law** — a chain that panics refuses the frame whole with
  `STATUS_PANIC` and `owner_panics += 1`, as the serial batch did.

Design note: `SERVE_INO_LOCKS` (publish.rs) is the S9 publish plane's
per-ino stripe and is NOT this path's serializer; the S8 batch verbs
serialize on the backend's own 4a guards, which the chain discipline
keeps in submission order per named inode.

## Measured (in-process two-node harness, debug build, file-backed KV sandbox, 32-CPU dev box)

Frame = 64 independent `Setattr` verbs on 64 distinct inos (the M7 /
S8 batch-cap floor — the widest frame every machine ships), one owner
+ one shipping client in one process over the real `cluster_wire`
(`127.0.0.1`, authenticated frames). Instruments: process-global
`META_CONVEYOR_LEADER_PASSES` and `META_KV_JOURNAL_ENTRIES` deltas
around the one frame; owner-side wall = the client's `ship_ops` await
(includes one wire RTT).

| Binary | passes / frame | journal entries / frame | wall / frame | per verb |
|---|---|---|---|---|
| dev tip `98346dab` (serial) | **64** | 64 | 8.18 ms | 127.7 µs |
| fix `9a463db7` (×5) | **3, 2, 3, 3, 3** | 64 | 3.86–5.11 ms | 60.3–79.9 µs |

Passes per frame **64 → 2–3** (the ideal is 1; the stragglers are the
pass task mid-drain when the first committers land — one pass per
"wave" of arrivals). Owner wall −45 to −53 % on a debug build whose
per-verb CPU (encode, guards, fold) is serialized by nothing but the
pool's width; the release-build and fleet numbers are owed.

**Contract pinned:** ≤ 4 passes per 64-verb frame; entries per frame ≡
N; every reply id-correlated in op order; a failing verb (ENOENT ino)
lands in ITS slot with every sibling applied; three same-ino setattrs
spread across a frame answer their own writes in order and the LAST is
durable, while a create → lookup → unlink → lookup chain on the parent
runs beside 31 concurrent siblings; a replayed 16-mutation frame is 16
dedup hits with identical outcomes and exactly-once durable effect.

Suites green on the fix (all `--test-threads=1`): the new suite (4),
`meta_ship_tests` (15), `dlm_cowriter_tests` (18),
`mw_cowriter_free_tests` (49), `mw_cowriter_lane_tests` (26),
`kvmap_mw_hazard_tests` (7), `kvmap_crossing_tests` (9),
`durable_block_refs_tests` (17), `mw_widthn_refs_tests` (15),
`mw_delegation_tests` (14), `mw_intent_batch_tests` (20). `cargo clippy
--lib --test meta_ship_owner_dispatch_tests -- -D warnings` clean,
`cargo fmt --check` clean. `task check` deferred (batched by the user).

## What this does and does not move

* **Moves:** the S8 metadata verb plane (`create`/`unlink`/`link`/
  `rename`/`setattr`/`setxattr`/`removexattr`/`destroy_inode` shipped
  by a co-writer or partial authority). A K-writer fan-out whose verbs
  coalesce into frames now costs ~1 pass per frame at the authority
  instead of N.
* **Does NOT move by itself:** the S9 **publish** plane
  (`PublishCall::{Set,Merge}LayoutAndSize` — the layout publishes the
  co-writer INGEST wall rides). A publish frame carries ONE call, and
  the `PublishClient` holds ONE mutex-serialized session per endpoint
  (`publish.rs` `lane()`): each co-writer's publishes are stop-and-wait
  depth 1 (board **DLM #8**), and concurrency at the authority comes
  only from K connections (thread-per-connection, F-B). F-A's mechanism
  is the right one for that plane too, but it needs the client side to
  BATCH publishes (a `PublishRequestFrame` with N calls, or N sessions)
  before the owner can co-queue them — that is the D-1 follow-on, not
  this landing. The audit's "≈ 2.6 GiB/s wall" attribution to F-A
  therefore stands for the verb plane's share of the authority's
  conveyor budget; the publish plane's own serial term is owed a row.

## Owed

1. **The fleet field row** (the campaign's verdict instrument): from
   this worktree, `sudo -n -E env "PATH=$PATH" SQZ_MWFLEET_OSS_GB=64
   tests/mw_fleet.sh create N=1 --multi-writer --cowriters=8`, then
   `tests/run_mw_matrix.sh s9-fanout` (the S9-a row: per-member ingest
   + `meta_ship_publish` engagement) and an S8-class verb storm
   (`s8a`-family rows — `meta_ship_owner_phase_ns` sum/count +
   `meta_txpass_phase_ns` + `meta_commit_group_size`), **A-B-B-A vs
   dev tip `98346dab`**, tcp devsub, one fleet at a time, teardown to
   zero residue. Not run: the box was serving this session's suite
   runs (foreign cargo load invalidates a row) and the reboot deadline
   forbade a fleet start after 17:05.
2. **Release-build in-process row** — the table above is `cargo test`
   debug; a `--release` rerun of `a_frame_of_independent_verbs_…`
   prices the per-verb CPU term honestly.
3. **Publish-plane batching** (the follow-on above): a multi-call
   `PublishRequestFrame` or per-co-writer session fan-out so layout
   publishes co-queue at the authority the way verbs now do; verdict =
   aggregate co-writer ingest GiB/s vs the S9-a wall.
4. **Chain-width instrument** — a `meta_ship_owner_chains` /
   `meta_ship_owner_chain_width` pair on the stats inode (chains per
   frame; ≈ frame width = fully independent, ≈ 1 = one hot object) so
   the field can see the lever engage without the test harness.
5. `task check` (the full gate) — deferred by instruction; only the
   suites named above ran.
