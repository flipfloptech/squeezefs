# 2026-07-28 — FUSE_HANDLE_KILLPRIV_V2: the per-write killpriv GETXATTR probe deleted

| | |
|---|---|
| **Finding** | `.benchmarks/2026-07-27-oq1-overwrite-op-economy.md` §4/§5: every `write(2)` on every mount paid one `GETXATTR("security.capability")` FUSE round trip (kernel `file_remove_privs` killpriv probe — FUSE cannot set `SB_NOSEC`, and neither `FUSE_HANDLE_KILLPRIV` nor `FUSE_HANDLE_KILLPRIV_V2` was negotiated). 3,072 of the 3 GiB O_DIRECT row's 6,249 requests — **half the stream** — answered ENODATA every time. |
| **Fix** | Negotiate **FUSE_HANDLE_KILLPRIV_V2** (uapi init bit 28, Linux ≥ 5.11): the kernel stops probing/clearing and instead flags the daemon (`FUSE_WRITE_KILL_SUIDGID` / `FUSE_OPEN_KILL_SUIDGID` / `FATTR_KILL_SUIDGID`), which now owns the clearing law. A **security-semantics transfer**, every edge red-first. |
| **Measured** | Same rig/row as OQ-1: kernel total **6,274 → 3,223** (−48.6 %); GETXATTR **3,072 → 32** (−99 %; residual = 2/file, per-file not per-write — see §6). 4 KiB O_DIRECT write A-B-B-A: **t16 +29 %** (93.2k → 120.0k IOPS mid-bracket means), **t1 +55 %** (14.9k → 23.1k). Large-block (1 MiB t16): ~noise, as forecast (probe rode parallel lanes). il/shim lane: ~0 (never paid the probe), engagement exact. |
| **Semantics** | Flagged ⇒ clear S_ISUID **always**; clear S_ISGID **only if group-executable** (sgid-without-group-exec = mandatory-locking marker, PRESERVED — the classic trap); drop `security.capability`. Unflagged (root / CAP_FSETID — the kernel's call) ⇒ clear NOTHING. No-priv-bits common case = one lock-free latch contains-check, **zero metadata traffic** (journal-delta-equality pinned). |
| **Substrate / instrument** | nvmet-**tcp** devsub (`SQZ_DEVSUB_TRANSPORT=tcp tests/dev_substrate.sh`), 4 meta + 4 data namespaces, cache-less format; elbencho 3.1-10 (dynamic — loads the shim); kernel 7.1.4-1-cachyos `fuse_request_send` hist (`hist:key=connection,opcode`, exact, kernel-side) + `.stats` gauges; bpftrace `fuse_getxattr` name capture. Box shared-quiet; 4 KiB rows are single runs bracketed A-B-B-A (both orders), not medians — the bracket separation (≥ 25 % on t16, ≥ 50 % on t1, zero overlap) is the evidence. |
| **Branch** | `perf/fuse-killpriv-v2` off dev `7396ffd`. Commits: fork RED `9b8acb2`, fork negotiation `d2fbc29`, fork lint-drift `2956b5c`, daemon RED `f5cb3ea`, daemon law `2308002`, il RED `0678274`, il parity `c73f9de`, docs `7b09163`, this note. |

## 1. Kernel contract (citations)

* `include/uapi/linux/fuse.h`: `FUSE_HANDLE_KILLPRIV_V2 (1 << 28)` — *"fs kills suid/sgid/cap on write/chown/trunc. Upon write/truncate suid/sgid is only killed if caller does not have CAP_FSETID. Additionally upon write/truncate sgid is killed only if file has group execute permission. (Same as Linux VFS behavior)."* Request bits: `FUSE_WRITE_KILL_SUIDGID (1 << 2)` in `fuse_write_in.write_flags`; `FUSE_OPEN_KILL_SUIDGID (1 << 0)` in `fuse_open_in.open_flags` (the uapi ≥ 7.33 second word — the fork previously read it as `_unused`); `FATTR_KILL_SUIDGID (1 << 11)` in `fuse_setattr_in.valid`.
* `fs/fuse/file.c`: the probe deletion — `fuse_cache_write_iter` runs `file_remove_privs()` **only** `if (!fc->handle_killpriv_v2)`; the direct-IO path sets `FUSE_WRITE_KILL_SUIDGID` on each WRITE when `fc->handle_killpriv_v2 && !capable(CAP_FSETID)`. `fuse_send_open`/`fuse_create_open` set `FUSE_OPEN_KILL_SUIDGID` on O_TRUNC opens under the same capability check.
* `fs/fuse/dir.c` `fuse_do_setattr`: `FATTR_KILL_SUIDGID` on size-changing setattr (`!capable(CAP_FSETID)`) and unconditionally on non-directory chown.
* **V1 vs V2**: `FUSE_HANDLE_KILLPRIV` (bit 19) is the coarser *unconditional*-kill contract (no CAP_FSETID gate, no group-exec sgid preservation). V2 adopted, V1 never echoed — pinned in the fork's `init_negotiation_tests` (the POSIX_LOCKS/FLOCK_LOCKS never-advertise-what-you-don't-implement pattern).
* Negotiation is offer-gated: kernels that don't offer bit 28 keep the classical probe — behavior unchanged, the correct degraded posture (`fuse_killpriv_negotiated = 0`).

## 2. Mechanism (what landed)

* **fuse3 fork** (`crates/fuse3`): uapi bits in `abi.rs`; `fuse_open_in.open_flags` decoded (layout pinned at offset 4); `MountOptions::handle_killpriv_v2` gates the echo; `negotiated_reply_flags()` publishes the INIT reply word (KERNEL_INIT sibling — `kernel_init_info()` is the kernel's *offer*, not what we accepted); `Filesystem::open` gains `open_flags`; `SetAttr` gains `kill_suidgid`.
* **Daemon** (`src/fuse_client.rs`): `kill_suidgid_mode()` is the ONE law (suid always; sgid iff S_IXGRP; caps drop). WRITE: `FUSE_WRITE_KILL_SUIDGID → apply_killpriv(ino)` before data lands (VFS privs-before-write order). OPEN: `FUSE_OPEN_KILL_SUIDGID` folds into the existing ATOMIC_O_TRUNC → setattr(size=0) route. SETATTR: `kill_suidgid` folds the mode clear into the op's **one existing commit** (D4 economy — journal-delta equality pinned) + probe-then-drop for the caps xattr.
* **Economy**: `killpriv_clean` latch (lock-free scc set) — a flagged write on a known-clean ino is a contains-check, zero metadata traffic (pinned by journal-entry-delta equality flagged-vs-unflagged). Slow path ≤ once per ino per mount + once per re-arming mutation; a performed clear is one ordinary commit (per-transition, never per-write). Race law: `apply_killpriv` **inserts-then-reads**; mutators (setattr-with-mode, setxattr of `security.capability`) **commit-then-remove** — every interleaving converges, a stale "clean" cannot survive a mutation.
* **Mount**: `handle_killpriv_v2` always armed (the handlers exist, so the capability is honest). `SQUEEZEFS_FUSE_NO_KILLPRIV=1` = the **testing-only** A/B escape (restores the kernel probe posture); never operational advice.
* **CREATE**: the kill bit in `fuse_create_in.open_flags` is deliberately not plumbed — a CREATE names a file that did not resolve at lookup (nothing to kill on a caller-owned fresh mode); existing-file `O_CREAT|O_TRUNC` rides FUSE_OPEN, which is plumbed.

## 3. Opcode tables (kernel-side hist, exact; 3 GiB row: elbencho 16t × 192 MiB × 1 MiB O_DIRECT)

Same shape as the OQ-1 §2 rows (`fw` fresh files, `ow` overwrite), one binary (`7b09163`-class), one mount each posture:

| opcode | fw OFF (`SQUEEZEFS_FUSE_NO_KILLPRIV=1`) | ow OFF | fw ON (negotiated) | ow ON |
|---|---|---|---|---|
| WRITE | 3072 | 3072 | 3072 | 3072 |
| **GETXATTR** | **3072** | **3072** | **32** | **42** |
| GETATTR | 44 | 27 | 33 | 28 |
| LOOKUP | 17 | 18 | 17 | 18 |
| OPEN / CREATE | 1 / 16 | 17 / 0 | 1 / 16 | 17 / 0 |
| RELEASE / FLUSH | 17 / 17 | 17 / 17 | 17 / 17 | 17 / 17 |
| SETATTR | 16 | 16 | 16 | 16 |
| READ (stats snaps) | 2 | 2 | 2 | 2 |
| **kernel total** | **6274** | **6258** | **3223** | **3229** |

* **−48.6 % of all FUSE requests** on the write-syscall-bound stream; GETXATTR per write(2) **1 → ~0.01**.
* `fuse_killpriv_negotiated` read 1/0 per posture on every mount (the engagement gauge); `fuse_killpriv_clears` stayed 0 (no priv'd files in the stream — correct).

## 4. fstests killpriv coverage (root, mounted FS — `sudo tests/run_fstests.sh <singles>`)

Both postures, same binary, /dev/shm devices per the runner:

| test | what | negotiated (default) | `SQUEEZEFS_FUSE_NO_KILLPRIV=1` |
|---|---|---|---|
| generic/193 | setattr permission/killpriv checks (fsgqa) | **pass** | **pass** |
| generic/355 | suid/sgid clear on write + O_TRUNC | **pass** | **pass** |
| generic/683 | setgid strip on write | **pass** | **pass** |
| generic/684 | setgid strip on truncate | **pass** | **pass** |
| generic/685 | setgid strip on fallocate | **pass** | **pass** |
| generic/688 | setgid strip (fzero leg of the series) | **pass** | **pass** |
| generic/673 / 674 / 675 | notrun — reflink/dedupe not supported on FUSE | n/a | — |
| generic/686 / 687 | notrun — finsert/fcollapse not supported | n/a | — |
| generic/689 | notrun — idmapped mounts not supported by fuse | n/a | — |
| generic/689-class chown | covered by 193's chown legs | pass | pass |

Zero failures ⇒ zero repro-ports owed. (The 686/687/673-675/689 notruns are capability-class, recorded; they notrun identically on dev tip.)

## 5. Throughput face — 4 KiB O_DIRECT write A-B-B-A (request-bound forecast row)

Fresh dirs per row (no store aging *within* a row; bracket alternates postures across remounts — both orders present). elbencho `-w -b 4k --direct`, single runs:

| bracket position | posture | t16×48 MiB IOPS | t1×64 MiB IOPS |
|---|---|---|---|
| B1 | OFF | 93,861 | 15,209 |
| A1 | ON | 122,708 | 22,999 |
| A2 | ON | 117,300 | 23,159 |
| B2 | OFF | 92,603 | 14,622 |

* **t16: +28.7 %** (ON mean 120.0k vs OFF mean 93.2k); **t1 (latency-bound): +54.7 %** (23.1k vs 14.9k) — the removed round trip is worth most where lanes can't hide it, exactly the honest forecast ("up to ~2× on request-bound rows" — this rig's fabric RTT puts it at 1.29×/1.55×).
* **1 MiB t16 rows** (§3 runs): 1,335–1,466 MiB/s OFF vs 1,317–1,492 ON — **noise**, as stated up front (bandwidth-bound; the probe rode 16 parallel lanes).
* il/shim lane, same A-B-B-A shape (`-o interception`, dynamic elbencho + `LD_PRELOAD=libsqueezefs_il.so`, engagement exact — `ipc_ops_write` delta = 196,608 = the row's op count on every run): A1 128,184 / B1 122,979 / B2 125,615 / A2 131,131 — **~0 within noise**, expected: ring writes never paid the GETXATTR, and the env lever doesn't touch the ring lane. (A KD-7 lesson re-learned en route: a `-dirty` shim against a clean daemon refuses version-equality — dist-folder pairing exists for a reason.)

## 6. Residual GETXATTR under the negotiated posture

bpftrace on `fuse_getxattr` during an ON-posture O_DIRECT stream: `@[security.capability] = 1 per file` (4 for a 4-file run; the §3 rows' 32/42 ≈ 2/file incl. truncate legs) — a **per-file** probe from paths outside the per-write killpriv gate (open/truncate-adjacent), not per-write. 3,072 → 32 stands; the residue is bounded by file count, not write count. Not worth chasing.

## 7. The il-parity decision (documented per campaign charter)

Intercepted `write(2)` bypasses the kernel VFS, so `file_remove_privs` **never ran** on the ring path — a **pre-existing** gap, independent of this negotiation. Decision: **parity implemented** (`c73f9de`), not documented-away:

* The session peer's class is computed once at HELLO from the SO_PEERCRED-verified identity: uid 0 exempt, else `/proc/<pid>/status` `CapEff` bit 4 (CAP_FSETID) decides; unreadable /proc and malformed words classify **kill** (the conservative direction — clearing where the kernel might not is safe; preserving where it would clear is the hole).
* `BindingRights::kill_priv` carries it per BIND; `serve_write` translates it to `FUSE_WRITE_KILL_SUIDGID` on the **same** daemon write handler — one law, one latch, one economy. Root il fleets (the common case) never even consult the latch.
* **Documented approximation / open question (OQ-1 of this note)**: the kernel samples the writing task's capability per syscall; the ring samples the session peer's at establishment. A client changing CAP_FSETID mid-session keeps its HELLO-time class until it reconnects. Err direction is toward clearing. Revisit only if a real fleet runs non-root CAP_FSETID-toggling writers over il.

## 8. Stats family

* `fuse_killpriv_negotiated` (0/1 per mount) — the engagement gauge: 1 ⇔ the INIT reply advertised the capability (kernel offered + mount armed). 0 on pre-5.11 kernels / `SQUEEZEFS_FUSE_NO_KILLPRIV=1`.
* `fuse_killpriv_clears` — clears performed (a suid/sgid mode commit; a caps drop) across flagged WRITE / O_TRUNC OPEN / SETATTR and il-parity ring writes. Steady growth on workloads that never touch priv'd files = known-clean-latch regression.

## 9. Gates

* Root workspace full gate from zero at the final code state: clippy `-D warnings` clean, fmt clean, `cargo test --all-features -- --test-threads=1` **1,512 passed / 0 failed**, `cargo doc --no-deps` builds (3 intra-doc warnings **pre-existing at dev tip**, verified in a `7396ffd` worktree), bench smoke green.
* fuse3 fork standalone suite: 41 passed / 0 failed; clippy `-D warnings` clean **after** clearing the fork's pre-existing rust-1.96 toolchain-drift debt (`2956b5c` — 18 errors at dev tip: io_other_error ×10, cast/conversion ×4, redundant_guard, type_complexity, trait-companion too_many_arguments); fmt clean.
* New cargo pins: `tests/killpriv_v2_tests.rs` (9 — the clearing law incl. the sgid-marker trap, both passthrough faces, the D4 fold equality, the zero-meta fast path, latch re-arm), fork `init_negotiation_tests` + abi/SetAttr pins (5), il parity (4: parser, class, binding pin, end-to-end production-sink law).

## 10. Repro

```bash
SQZ_DEVSUB_TRANSPORT=tcp sudo tests/dev_substrate.sh create
sudo squeezefs format "sqmeta:///dev/nvme{1..4}n1" "sqdata:///dev/nvme{5..8}n1" --force
sudo squeezefs mount "sqmeta://..." /mnt/k --daemon --allow-others --mem-cache-size 1GB
grep fuse_killpriv_negotiated /mnt/k/.stats            # 1 = engaged
echo 'hist:key=connection,opcode' > /sys/kernel/tracing/events/fuse/fuse_request_send/trigger
# per row: snapshot hist; elbencho -w -t 16 -s 192m -b 1m --direct /mnt/k/d/f{1..16}; diff.
# OFF posture: remount with SQUEEZEFS_FUSE_NO_KILLPRIV=1 (testing-only).
# fstests singles: sudo tests/run_fstests.sh generic/193 generic/355 generic/683 generic/684 generic/685 generic/688
```
