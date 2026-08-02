# PERF-7 — shim bound-fd offset mirror: lseek pair deleted from every offsetful op

**Date:** 2026-08-02 · **Branch:** `perf/shim-lseek` (base dev @ `402ca77`) ·
**Spec:** `docs/pre-rc-engineering-spec.md` §9 PERF-7 · **Ruling:** execution-plan D7 Track 2
**Design amendment:** `docs/design-preload-interception.md` Rev 19 (§5.4.3, KD-6)

## What changed

Every offsetful shim op (`read`/`write`/`readv`/`writev` — the default for `cp`,
`dd`, `tar`) paid two `lseek64` syscalls (`interpose.rs` ring_offsetful/ring_iovec)
to keep kernel `f_pos` authoritative. The offset now mirrors in the shared
`BindingCell` (the object dup'd fds already share) under the normative authority
predicate (`crates/squeezefs-preload/src/fd_table.rs` module docs): armed at bind
with a pre-open fork-epoch snapshot, snapshot still current, never demoted. All
transitions out of mirror authority flush mirror → kernel `f_pos` first (atfork
prepare, `posix_spawn{,p}`, unbind-class demotes, offsetful fallthroughs,
`sendfile{,64}`/`copy_file_range`/`splice` NULL-offset shapes). `lseek`/`lseek64`
are interposed: SEEK_SET/CUR pure arithmetic on armed fds; SEEK_END/DATA/HOLE
kernel-routed (POSIX-8 i_size scope unchanged). Contracts:
`crates/squeezefs-preload/tests/offset_mirror_tests.rs` (red `90a4820` → green
`b2eb30b`).

## Microbench (the per-op term deleted)

`cargo bench -p squeezefs-preload --profile preload-release --bench
offset_mirror_bench` — dev box (24 CPU, quiet-untuned, relative truth only):

| row | median |
|---|---|
| `armed_op_cycle` (lookup+lock+armed+load+publish) | ~73 ns/op |
| `lseek_served_seek_cur` (interposed ftell idiom) | ~101 ns/op |
| `lseek_syscall_pair` (the replaced SEEK_CUR+SEEK_SET) | ~1.48 µs/op |

## Engagement proof (strace -c, the PERF-7 instrument)

**Instrument:** `strace -c -f dd bs=128k` (256 MiB rows) under
`LD_PRELOAD=libsqueezefs_il.so` on a `--interception` mount; **substrate:
file-backed /dev/shm volumes** (engagement + syscall census only — NOT a
bandwidth venue; per the two-substrate rule any throughput read here is scoping,
never acceptance). Same-commit KD-7 pairs per face: branch `fbe684d` vs base
`402ca77` (separate worktree builds). Engagement exact on every row:
`ipc_ops_read +2049 / ipc_ops_write +2048` per face (2048 × 128 KiB ops + probe).

| face | dd read row `lseek` calls | dd write row `lseek` calls |
|---|---|---|
| base `402ca77` | **4104** (2×2048 + dd's 8) | **4103** |
| branch (mirror) | **8** (dd's own startup seeks) | **8** |

Reproduced in BOTH A-B-B-A orders (deterministic census). The sudo preload gate
(`tests/run_preload_gate.sh`, both legs) passed on the branch, including the
2e SEEK_CUR pin (13 FUSE ops / 10k seeks — now served from the mirror), 2f
offsetful `cat` parity, dup transparency, fork/kill-9 soaks, fio/elbencho verify.

Timed dd rows on this venue are label-only (0.06–0.5 s wall, warm RAM tier,
first-face warm-up noise): the reversed-order bracket ran branch ≥ base (read
median 0.078 s vs 0.096 s; write ~par), the first-order bracket inverted —
ordering artifact, not signal. The +30–60 % PERF-7 expectation is a
fabric-latency-venue claim (each deleted pair ≈ 1.5 µs of per-op budget) and
must be adjudicated there (`SQZ_DEVSUB_TRANSPORT=tcp` rig) per the standing
substrate law; this note claims **engagement (lseek → 0) and the deleted per-op
term**, not a throughput multiple.

## Dup/fork sharing verdict (the honest subset)

- **dup (intra-process): exact.** dup'd fds share the `BindingCell` ⇒ one mirror,
  one offset lock — kernel shared-`f_pos` semantics, and strictly tighter than
  the old per-fd stripe (which split dup siblings across stripes).
- **fork: exact at the boundary via prepare-flush**; the parent's live bindings
  demote to the pre-PERF-7 kernel-resync discipline (epoch bump), the child is
  poisoned-passthrough on flushed offsets. Bind-vs-fork races are closed by the
  pre-open epoch snapshot (a raced bind never arms).
- **Out of model (documented, §5.4.3 Rev 19):** cross-process concurrent
  offsetful racing (already excluded pre-PERF-7), `SCM_RIGHTS` fds, raw-syscall/
  io_uring/glibc-internal f_pos consumers on an armed fd, fork-less `execve`
  with a mid-file inherited bound fd.
