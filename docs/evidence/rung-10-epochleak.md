# Rung 10 — the write epoch's silent drops (epochleak)

Found by reading `write_epoch.zig` / `plane.zig` while chasing another leak; each is a space leak
(a block the allocator tracks and nothing references) or a silent drop of one. Charter §6.2
item 3: parked keys are freed after the save, a fence frees nothing — nothing else may drop one.

| # | site | reachable? | red (before) | fix (after) |
|---|------|------------|--------------|-------------|
| 1 | `Epochs.closeEpoch`, the merge's `error.NotFound` arm | **yes**: the arm exists because an epoch can outlive its file's generation (a truncate-to-zero re-class — `mapMerge` answers NotFound for a non-striped record — or a destroyed ino; a write landing after the truncate's `settleCustody` opens the epoch). A block rewritten twice in that epoch parks its first B as `.epoch` | write_epoch "a reclaimed ino frees EVERY key the epoch minted …": 1 of 2 Bs freed; the intermediate B stayed tracked, referenced by nothing, its guard dropped | the arm frees every `.epoch` parked key with the shadow's Bs (under their guards; `dropEpoch` drops the guards after); the map's A keys stay the destroy's |
| 2 | `Epochs.supersede`, orphans past `[8]Binding` | **not provably bounded**: the epoch parks one `.epoch` key per rewrite of a block, so a hot block of a not-yet-covered file can hold any number; the supersede runs on the upload's durable arm for a block the open epoch shadows (the ino read as not striped) | write_epoch "a supersede of a block rewritten more times in the epoch than its orphan batch holds …": 10 records of one block, the supersede returned 8 of the 10 Bs and dropped 2 (9 of 11 frees) | the `.epoch` keys past the batch stay PARKED (guards held) and the epoch's close frees them with the rest of its parked set (11 of 11; no allocation under the spin lock) |
| 3 | `plane.foldInserts` (and its twin, `kvmap.stageWindow`), displaced past `MergeOutcome.max_displaced` | **no**: a fold displaces at most one binding per distinct insert, and every `MergeRequest` carries ≤ `max_displaced` inserts (the ONE save's `[max_displaced]` buffer over its block-keyed shadow; every other publisher sends one) — now `MergeRequest.inserts`' documented contract | plane "foldInserts displaces at most one binding per insert …", kvmap "the window put displaces at most one binding per (unique) insert …": `max_displaced` inserts fill `MergeOutcome` exactly; one more dropped a binding without a word (no log) | the drop fires the must-stay-0 tripwires `layout_map_merge_displaced_overflow` / `kvmap_merge_displaced_overflow` (in both stats JSONs) |

**Verified (kamrui, x86_64).** The four new tests red on `e9cfd69` (`expected 2, found 1`;
`expected 11, found 9`; both overflow tests `expectAllFired` 0 ≠ 1), green on the fix; the
filters `write_epoch`, `kvmap`, `data.plane`, `accumulate`, `write_admit`, `device_overlay`,
`fsync`: 166 pass / 13 skip in 8 fresh runs (one earlier run, while a full gate held the box,
failed a test that its truncated output did not name; it did not recur in 7 runs). The smoke rows
"the data plane WITH / WITHOUT a cache path" (4 KiB overwrites, truncate, rm → df at the baseline
with the census) passed twice. Full gate on kamrui (`-Dfresh=true`, 893 s): `53/53 steps
succeeded; 2966/3049 tests passed (83 skipped)`.
