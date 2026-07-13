# Beta release gate — full nightly/release-tier sweep (2026-07-13)

**RELEASE VERDICT: NOT-READY** — one NEW, unattributed on-disk metadata-integrity class
(finding A below: persistent checksum-passing-but-undecodable KV records on the fstests
scratch volume, born on a live mount with no crash, no kill, no external device writer in
the window) must be root-caused or attributed (software vs this box's RAM/CPU history)
before beta. Every other failure in all four suites is attributed to a documented platform
class, a documented pre-existing class, or an external harness exposure — and the punch-list
suites themselves moved forward (LTP is fully green incl. the historical writev03 class;
QUICK-set members 069/074/075/091/112/127/616/617 all pass inside the full run).

## Provenance

| | |
|---|---|
| Tree | `dev @ af10e9a` (verified tip, clean tree), gate run as TEST-EXECUTION AND DISPOSITIONING — nothing fixed |
| Binary | `cargo build --release` under `taskset -c 0-15`, `CARGO_BUILD_JOBS=12` (2m47s); md5 `5b123ed91bcbe4845d09f0b4066e5a9d` |
| Box | AMD RYZEN AI MAX+ PRO 395, 32 hw threads, 109 GiB RAM, kernel `7.1.3-2-cachyos`, CPU capped 3.5 GHz (untouched) |
| Rails | `taskset -c 0-15` everywhere; `SQUEEZEFS_FSTESTS_MEMMAX=8G` on fstests; LTP + bench daemons caged `systemd-run … MemoryMax=8G MemorySwapMax=0`; Tctl watchdog armed (≥90 °C SIGSTOP / <80 °C resume) — **never fired** (run band 44–73 °C); `/mnt/squeezefs` untouched (stale attachment, no daemon; harness paths disjoint) |
| Box-state honesty | a parallel worker session (squeezefs-mveio checkout) was active on the box throughout: its own sandbox benches/QUICK/LTP rolls overlapped parts of the fstests run and forced timed rows into explicitly-verified quiet windows (annotated per row). Its volumes/paths never intersected this gate's. |
| Artifacts | `~/tmp/relgate_20260713/` — full harness logs, results dirs (both runs + A/B), both daemon logs, kernel-journal window, preserved meta-volume images, bench `.stats` |
| A/B reference | `origin/dev@93313c5`-era = its rebased equivalent `4e4800e` ("074 moves to deterministic PASS"), built in a worktree (md5 `43d206c3dbb44fffc46862f72ce01bca`), same harness, fresh volumes |

Historical baselines used for dispositioning: `tests/run_fstests.sh` QUICK provenance table
(2026-07-11, post-074/075/091/112/127/616/617/618 fixes), `.benchmarks/2026-07-09-kv-v3-gates.md`
(K7 full-run 85/784 + suite rows), `.benchmarks/2026-07-12-read-path-closing.md` (KV-flood
cage-kill class + elbencho-protocol rows), `.benchmarks/2026-07-13-mem-authority-convergence.md`
(saturation-suite acceptance bands).

## 1. Full fstests `-g auto` — 140 failed of 784 (2h20m wall)

Run 11:05–13:26 EDT, MEMMAX=8G, over-uring armed on both harness daemons (queues=32 depth=4,
log-verified). Wall clock is shorter than K7's 4h47m because the post-12:59 scratch cascade
(below) fast-fails ~90 tests. Raw counts: 140 fail / 340 notrun / 784 ran.

### Incident timeline (anchors every attribution)

| t (EDT) | event | evidence |
|---|---|---|
| 11:40:54 | **Finding A onset**: scratch daemon logs `corrupt KV encoding: dentry value: 9 trailing byte(s)` + `interior value must be 16 bytes, got 75` on a fresh, clean mount of the aged scratch volume; recurs on the same records for every scratch session until 12:59 (150 + 242 occurrences). TEST volume: **zero** decode errors all run. | scratch daemon log lines 7101+; journal shows the window's tests = generic/207–221 (aio-dio races, ENOSPC-mmap 211, unwritten-extent fallocate 213/214, DIO CoW 217–220); **no** daemon kill, **no** scope overlap (single scratch scope 11:30:29→11:57:18), **no** raw-device writer ran before onset (515/250/252/399/570 all later and/or notrun-bail) |
| 12:46:05 | scratch-scope cgroup OOM kill during generic/476 (fsstress): `anon-rss:244744kB` only — the 8G rail charges the /dev/shm tmpfs volume pages (MemorySwapMax=0 makes them unreclaimable) to the daemon scope; a fill-heavy soak busts the cage with a ~240 MB daemon | journal 12:46:05; 476.dmesg (`iou-wrk` invoked oom-killer, CONSTRAINT_MEMCG) |
| 12:58:58 | **generic/515 pwrites `0x58` over `SCRATCH_DEV[0..300 MiB]` raw, then bails notrun** (`_scratch_mkfs_sized` unsupported on FUSE). xfstests never re-mkfs a FUSE scratch, so the superblock stays destroyed: every later scratch mount fails `invalid superblock magic [88×8]` (1,060 log hits). Backing-file mtime 12:59 matches. | test source line 24; preserved image is X from byte 0 through 300 MiB, zeros beyond |
| 13:04:28 | TEST-scope cgroup OOM kill: `anon-rss:8213508kB` — **the documented KV-core allocation-flood fingerprint** (read-path closing report: anon ≈8.33 GiB, late soak block). Harness remounted TEST cleanly by 13:07 (generic/616/617 PASS at 13:07:21/13:07:57); no test failure is uniquely attributable to it (all mountfails in the window are scratch-side) | journal 13:04:28 (`squeezefs-fstests-squeezefs_test` scope); check.log |

### Failure inventory — every one of the 140, attributed

**(a) Documented / platform classes — 44 tests, each diff-verified:**

| Class | Tests | Signature (verified in .out.bad) |
|---|---|---|
| atime/ctime semantics under FUSE attr caching (QUICK-documented expected-fail) | **003** (the documented 10-ERROR-line diff, unchanged), 192, 309 | atime not updated / ctime jitter |
| thin provisioning (QUICK-documented expected-fail; honest-statfs shape re-pinned 2026-07-12) | **213** | exactly the one missing `fallocate: No space left on device` line |
| xattr surface (64 KiB value cap / list order / SGID) | 020, 377 | ATTRSIZE probe / ordering |
| nlink & unlink-while-open mapping | 035 | `nlink is 1, should be 0` |
| virtual control inodes in golden listings | 062 | `.config`/`.stats` extra lines |
| RENAME_WHITEOUT unsupported | 078 | whiteout legs missing |
| ACL ordering/masking family (K7-documented) | 099, 319, 444, 697 | ACL order / default-ACL / setgid-create |
| permission enforcement + fsgqa environment | 128, 688 | `su: cannot change directory` + perms |
| POSIX/OFD lock semantics family (K7-documented) | 131, 478, 504 | lock place/info mismatches |
| timestamp-wrap (negative epoch) | 258 | `Timestamp wrapped: 18131146533` |
| error-code golden mapping (EEXIST/ROFS) | 294, 306 | mknod/touch golden lines |
| idmapped-mount / user-ns family | 317, 318, 683, 684, 685 | `Invalid argument` on idmapped ops / mode-666 rows |
| SGID propagation | 375 | `-rwxrwsrwx` vs `-rwxr-sr-x` |
| getdents large-buf | 401 | `getdents: Invalid argument` |
| FICLONE/reflink surface (probe passes, ioctl ENOTSUP) | 415, 447, 513, 514 | `XFS_IOC_CLONE_RANGE: Operation not supported` |
| open_by_handle ESTALE (no exportfs-stable handles) | 426, 467, 477, 756, 777 | `returned 116 incorrectly on a linked file` |
| scratch drain > 60 s mount-helper timeout (K7 generic/105 class) | 452 | `previous daemon … still running after 60s` |
| 8G-rail × tmpfs charging artifact (environment, see 12:46 anchor) | 476 | memcg OOM with 244 MB anon; fsstress state confusion after |

**(b) Pre-existing, A/B-verified on the `93313c5`-era binary (fresh volumes, same harness,
identical signatures hunk-for-hunk) — 6 tests:**

| Test | Signature | Tip standalone | 4e4800e standalone |
|---|---|---|---|
| 209 | aio-dio invalidation race: `reader found old byte` | FAIL | FAIL |
| 451 | mixed buffered/DIO: `get stale data from buffer read` (0x55) | FAIL | FAIL |
| 533 | setfattr/attr golden mismatch | FAIL (`No such attribute`) | FAIL (`No such file or directory` — same test, same leg, errno variant) |
| 647 | `mmap-rw-fault: pread (D_DIRECT) from hole is broken` | FAIL | FAIL |
| 729 | same, -2 variant | FAIL | FAIL |
| 249 | splice/sendfile copy differ (TEST-side) | **PASS standalone** (order-dependent in-run flake; not reproducible on either binary) | PASS |
| (roll-1 window sweep also re-failed 208/210 — same aio-dio invalidation family as 209, flaky by run) | | | |

None of these six is in the QUICK must-pass table or any closing-report gate; the aio-dio
invalidation family (208/209/210/451) and the DIO-hole-pread family (647/729) are named
follow-up candidates for the punch list, **not** regressions of this tip.

**(c) Finding-A faces — 5 tests:** 340, 344, 345, 346, 354 — all `testfile: File exists`
(scratch cleanup `rm -rf` cannot unlink dentries whose records fail decode). These are the
test-visible face of the 11:40:54 corruption, not independent bugs.

**(d) generic/515 X-poison cascade (external harness exposure) — 84 tests:** every scratch
mount from 12:59 to run end reads an X superblock and fails; `_require_scratch` tests fail
as mountfail/`Format and mount` (verified: each checked `.mountfail` names `scratch_meta`):
517 518 519 523 524 525 529 530 536 537 540 541 542 543 544 546 551 554 562 565 567 569 585
588 590 599 604 605 606 608 610 611 614 615 **618** 622 628 630 631 634 635 636 641 642 643
646 648 651 652 653 654 655 657 658 659 660 661 662 663 664 665 666 667 668 669 670 671 672
673 674 675 696 700 702 705 707 732 733 737 748 749 750 751 754 758 761 766 774 775 778.
generic/754's K7-era "browser core" false-positive class is superseded here — its failure
today is a plain scratch mountfail. **generic/618's failure is this cascade** (scratch
mountfail 13:08:07), *not* the documented tier-tail OOM class — 616/617 passed on the
recovered TEST mount one minute earlier.

### QUICK-provenance cross-check inside the full run (regression gate)

PASS: 001 008 013 **069 074 075 091 112 127** 263 285 469 **616 617** · NOTRUN: 009 316
(fiemap canaries intact) · FAIL: 003 213 (documented platform, diffs unchanged) · 618 =
515-cascade mountfail (above). **Zero ZEROS-signature, zero stale-fill-signature, zero
074-family recurrences anywhere in 784 tests.** No STOP-AND-REPORT regression class fired.

### Finding A — NEW / unattributed (the release blocker)

Scratch-volume-only persistent KV record corruption on a live mount: records that **pass
the xxh3 bset checksum** decode as malformed (`dentry value: 9 trailing byte(s) after the
encoding`; `interior value must be 16 bytes, got 75` — a 75-byte value in an interior
position is dentry-record-shaped, i.e. content in the wrong tree position, not bit rot).
Born 11:40:54 under the aio-dio-race / ENOSPC-mmap / unwritten-extent test window
(generic/207–221) on a volume ~35 min and ~350 mount-cycles old; survived hundreds of
remounts (v3 correctly **detects and refuses loudly** every time — no silent garbage was
ever served, ops on the damaged keys error; the rest of the volume kept working); erased at
12:59 by the generic/515 device poison (so the on-disk evidence is the 1,452 daemon-log
error lines + results artifacts; the preserved image is post-poison). Exclusions proven:
no daemon kill / no SIGABRT / no scope overlap in the window; no external raw-device writer
before 12:58:58; TEST volume clean through identical workload classes for 4 h; not
reproduced by a fresh-volume 14-test window sweep (roll 1: failures {208,209,210,213} only,
**zero decode errors**), nor by any QUICK-era artifact on this box; no prior note documents
the signature. Because the bytes passed the checksum, the malformation predates writeback
(writer-side encode/merge defect on an aged tree, or an in-RAM node-image corruption escape
— this box's hard-crash/offline-CPU history and a `CPU8 failed to report alive state`
kernel line during the run keep a hardware contribution on the table at n=1). Violates the
v3 integrity contract (`docs/design-cow-kv-metadata.md`, AGENTS "Metadata format: v3")
regardless of which. **Dispositioned NEW/unattributed; DO NOT FIX in this gate; artifacts
preserved; must be root-caused (or pinned to hardware) before beta.**

## 2. Full LTP — PASS (matches tip expectation exactly)

`tests/run_ltp_syscalls.sh` (stock list) over a root mount caged 8G, over-uring armed:
**174 PASS / 0 FAIL / 0 BROKEN / 9 SKIP** (skips: chown0{1..5}_16, open14, openat03,
readdir21, statx07 — the standing TCONF set). The K7-era `writev03` BROKEN (punch-no-zero
class) stays fixed (`26b31b6` lineage). Run protocol note: executed with
`USE_EXISTING_MOUNT` onto an identically-shaped caged mount so the stock script's
`killall -9 squeezefs` could not kill the parallel worker session's daemons (rails:
no identity overrides, foreign session protected).

## 3. elbencho harness (stock `tests/run_elbencho_mount.sh`) — parity-with-annotation

Stock script (fresh /dev/shm 128M meta + 2G data, root daemon, `elbencho -w -r -t 4 -s 1G
-b 4M`), 3 valid rounds in a verified-quiet window (40–45 s cool-downs, Tctl 56–57 °C),
last-done column, medians; a 4th round was discarded mid-series when a foreign rustc storm
(99% CPU) landed mid-read (its read collapsed to 2,728 — recorded, excluded, re-rolled):

| MiB/s | r1 / r2 / r4 → median | K7 v3 lineage (median of 3) | Δ |
|---|---|---|---|
| WRITE | 2879 / 2879 / 3197 → **2879** | 3585 (3575/3585/3689) | **−19.7 %** |
| READ | 25500 / 26967 / 24052 → **25500** | 27263 (23622/27263/27406) | **−6.5 %** (inside the lineage's own 23.6–27.4 k spread) |

Attribution of the write delta: not a like-for-like pair — different session/era, desktop
ambient + resident foreign daemons, and the write path now carries the write-through +
mem-authority machinery accepted in `.benchmarks/2026-07-13-mem-authority-convergence.md`
(same-cage bare-suite write band 839–1,615 MiB/s spans ±46 % run-to-run on this box). The
K7 gate's own tolerance statement was ±10 % on a paired same-session A/B; this row is a
lineage context row, not a paired gate. READ parity holds. Dispositioned: **in-family,
noted for the next paired-A/B session; no data-path change is implicated by any
correctness suite.** (The read-path closing rows — 6.5 GB/s cold-seq, 59.5 k rand-4k IOPS,
0.98× amplification — are a different protocol/substrate and were not re-measured here;
their gates were closed on `3c89cd7` and nothing in this sweep contradicts them.)

## 4. Bare `squeezefs bench` saturation suite (operator smoke) — PASS

Committed protocol (fresh file-backed sandbox on /home NVMe: 1G meta + 144G data, staging
declared; **user** mount caged `MemoryMax=8G MemorySwapMax=0` with `--mem-budget 5G`;
`taskset -c 0-15`; quiet window): bare `squeezefs bench <mnt>` auto shape = 12 threads ×
2 g (24 g total), suite order write-seq→read-seq→rand-read→rand-write→stat→del.

| Row | This gate | Lineage (fingerprint clean / A2 fixed-tip band) | Verdict |
|---|---|---|---|
| write seq 1m | **1,410 MiB/s** | 1,118 / 839–1,615 | in-band ✓ |
| read seq 1m | **4,164 MiB/s** | 355 / 775–1,016 | **4–5× above band** (read-path program dividend on a 24 g set) ✓ |
| read rand 4k | **63,773 IOPS** (30 % cov) | 39.5 k / 37.6–42.6 k | above band ✓ |
| write rand 4k | **155 IOPS** | (PR 7 band 86–152; A2 101–138) | at/above band ✓ |
| stat / del | 77.2 k / 18.5 k ops/s | — | recorded |
| liveness | **rc=0, daemon ALIVE**, peak cage 8,078 MiB (touched, survived), red_events=1 (entered *and exited* — level 0 at settle), parked_gate waits 2,105 / self-flushes 428 / **timeouts 0**, hard_backstops=1 (fired once mid-saturation — the documented §5.7 last line; not a quiet-workload signal), tier publishes paused counted, `overwrite_seed_{deferred 4,363 / materialized 4,362 / skipped 1}` | pre-fix protocol OOM'd 1-in-2/3 | **survives** ✓ |

## 5. Cargo gate on the tip (once, for the record)

| Leg | Result |
|---|---|
| `cargo clippy --all-targets --all-features -- -D warnings` | clean |
| `cargo fmt --check` | clean |
| `cargo test --all-features -- --test-threads=1` | **690 passed / 0 failed** (73 test bins, one roll, no restarts) |
| `cargo doc --no-deps` | 0 warnings |
| `cargo bench --benches -- --test` | green — 117 ok (smoke) |

## Verdict, restated

**NOT-READY.** Sole blocker: **Finding A** (unattributed live-volume KV record corruption,
§1). Severity moderators, stated honestly: n=1 volume in one 2h20m adversarial suite; the
damage was *detected loudly and contained* by the v3 checksum/decode discipline (no silent
data served, no crash, volume otherwise operational); not reproducible on fresh volumes;
hardware contribution not excluded on this box. Everything else in the sweep is
READY-WITH-DOCUMENTED-CLASSES material: the platform classes are stable and diff-pinned,
the KV-flood cage-kill class and the drain-timeout class remain the two documented open
operational follow-ups, the aio-dio-invalidation and DIO-hole-pread families are
pre-existing punch-list items (A/B-proven not of this tip), LTP is fully green, and the
memory-authority + read-path programs hold their acceptance bands under this gate's loads.
Re-gate condition: root-cause (or hardware-pin) Finding A + one clean full `-g auto`
scratch-integrity pass (recommend: harness guard that re-formats SCRATCH_DEV when a test
notruns after raw device writes, so generic/515-class poison stops costing ~90 tests of
inventory per full run).
