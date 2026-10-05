# 0002 — No compatibility obligation with the Rust reference; quirks are not requirements

Status: accepted 2026-10-05 (owner's ruling: "let's remember not to bring poor design decisions
over into the new code base / filesystem; the old application doesn't have to be able to mount the
new version").

## Decision

1. **The charter is the specification; the reference is evidence.** We port the charter's goals,
   mechanisms, invariants, laws and measured derivations. Where the reference's code embodies a
   choice the charter does not require and a better design exists, we take the better design and
   record it here.
2. **No on-disk or wire compatibility with the reference.** The Rust binary never has to mount a
   Zig-formatted volume, and vice versa. Byte-identity with the reference's images (achieved for
   `format` in rung 2a) was a correctness oracle of convenience, never a requirement. The format is
   still forward-only among OUR releases (charter §1.7): once a Zig release ships, later Zig
   binaries refuse older incompat bits loud — compatibility obligations begin at our first release,
   not before.
3. **The reference binary stays an oracle where it is cheap.** While our format still matches,
   `/build/ref/squeezefs fsck --offline` remains a free second opinion. The moment a decision here
   changes the format, the cross-checks against the reference that touch that unit are retired and
   OUR fsck (rung 8) becomes the oracle; the Zig-side invariants (replay ≡ RAM, SMO completeness,
   the kill matrix) already stand on their own.
4. **Every departure is recorded below with its reason**, so a later wave never "re-imports" a
   quirk by reading the reference.

## Ledger of reference quirks NOT carried (and what we do instead)

| # | Reference quirk | Why it is poor | What we do |
|---|---|---|---|
| 1 | Journal admission as a byte-count word CAS'd up and decremented on transfer | ABA-blind: over-commits under a 3-party race (rung 2a, model 6, 450 states) | Monotonic totals (`admitted_total` as the CAS word); landed |
| 2 | Layout-delta base (`0x4C31`) encoded with Rust `bincode` (trailing bytes allowed, any map order) | A serialization-library artifact as an on-disk format; not canonical, not total in the charter's sense | Rung 4 defines the layout record as an `extern struct` with a canonical total codec; the bincode decoder in `record.zig` is retired then |
| 3 | Two delta classes (Δinode vs layout delta) with class-specific fold arms | Two code paths for one algebra | One `Delta(field_mask, fields)` kind over a field table generated per record type (rung 4) |
| 4 | Inode value: varint version tag + `rdev` stored in the old `flags2` slot (57 B) | Legacy slot reuse that the charter's "≈ 96 B packed inode" never asked for | A fixed-width `extern struct` inode value with explicit fields incl. `rdev` and `flags2`, size-asserted (rung 4, with the layout record) |
| 5 | DUR-8c `next_ino` recovery raises the LOCAL watermark with a dentry's GLOBAL child ino | Mixes keyspaces; inflates the watermark (`next_ino 1245187` after 20 locals) | Fold per keyspace: route the child ino through `inos.routeIno` and raise only its own slot's watermark (fix in `inos.zig` with a red test) |
| 6 | The journal lane and the conveyor pass on different threads (cross-thread wake hop measured 59–62 % of the write round trip) — the charter already corrects this | — | Both stages on the ONE lane thread once the executor lands (rung 3) |
| 7 | `std::sync::Mutex`-class locks inside hot-ish paths of the reference's cache (eviction ring, retired set) | The charter bans blocking locks on the data hot path | Miss-path only in `node_cache.zig` (documented); hot reads are latch-free through `ArcSwap` |
| 8 | fsck refuses a volume with no `format_config` xattr ("is this volume formatted?") | A CLI bootstrap requirement smuggled into the format | Our fsck judges the superblock, not a config blob |
| 9 | Panic-guarded tasks (`catch_unwind`) as the error boundary | Hides programming errors behind a counter | Zig has no unwinding: recoverable outcomes are error unions + tripwires; a true bug aborts loud |

When a wave meets a new quirk it adds a row here before diverging.
