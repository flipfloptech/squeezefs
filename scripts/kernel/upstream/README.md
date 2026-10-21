# scripts/kernel/upstream — the sqz patches in upstream form

Posting-ready patches, one directory per posting, produced on 2026-10-21 from the mainline
rebase in `docs/evidence/rung-10-kernel-mainline.md`. The owner posts them (`git send-email
<dir>/*.patch` — the `To:`/`Cc:` headers are in the files, from `scripts/get_maintainer.pl`
on the mainline tree; each file ends in a `base-commit:` trailer).

**Base**: mainline `v7.3` (`c03c1a14507d`, released 2026-10-18); the rebased tree is
`/build/kernel/linux-mainline-sqz` (branch `sqz/mainline-v7.3` of the clone
`/build/kernel/linux-mainline`), the same commits as `scripts/kernel/patches-mainline/`.
Every patch: `scripts/checkpatch.pl --strict` clean, compiled with `W=1` (`fs/fuse/`,
`io_uring/`, `mm/shmem.o`) at its own commit and at the series' head, no warnings. The fuse
patches also apply to `fuse/for-next` (`5a9745611d62`, 2026-10-20) — check before posting:
`git -C /build/kernel/linux-mainline fetch fuse for-next`.

**The identity**: the author/`Signed-off-by` is the repo's git identity (`Justin Oberdorf
<109311040+flipfloptech@users.noreply.github.com>`, the dot-less form of `git log` on
`dev`). It is a real name but a GitHub noreply address: replace it with a reachable address
before posting (`sed -i 's/109311040+flipfloptech@users.noreply.github.com/<addr>/'`), since
maintainers reply to the From/SoB address and some lists bounce noreply domains.

| directory | subject | list / To | what it is in the series |
|---|---|---|---|
| `fuse-uring-abort-unclaimed/` | `fuse: io-uring: end requests a stopped server never claimed on abort` | fuse-devel; Miklos Szeredi; Cc Bernd Schubert, Joanne Koong | 0028 re-expressed on mainline (mainline has no per-queue background ledger, so the abort only moves the request; the task work also recycles the entry's pool buffer). A fix: a stopped server leaves callers in D state past the request timeout. |
| `io_uring-memmap-short-bulk/` | `io_uring/memmap: finish a short bulk allocation with reclaiming pages` | io-uring; Jens Axboe; Cc Pavel Begunkov | 0029 re-targeted at `io_region_allocate_pages()` — mainline has no kmbuf rings, but every kernel-allocated region (`IOU_PBUF_RING_MMAP` buffer rings, the SQ/CQ regions) takes the same bulk-then-fail path. A fix. |
| `mm-shmem-nowait-read/` | `mm/shmem: honour IOCB_NOWAIT in shmem_file_read_iter() and set FMODE_NOWAIT` | linux-mm; Hugh Dickins, Baolin Wang, Andrew Morton; Cc Jens Axboe, io-uring | 0032 unchanged (applied clean). Performance: an io_uring read of a resident memfd page completes inline instead of an io-wq punt (2.6 cores of io-wq at 2.1 M reads/s). |
| `fuse-large-folio-user-pages/` | `fuse: coalesce the contiguous pages of a folio in fuse_get_user_pages()` | fuse-devel; Miklos Szeredi; Cc Joanne Koong, Bernd Schubert | 0033 unchanged but for context. Performance: per-folio instead of per-page pin release / dirtying / zero-copy bvec / bio segment for THP-backed direct-I/O buffers (52 % of the ring thread's cost per 1 MiB read in the profile). Note `fuse/for-next` carries Joanne's "fuse: enable large folios" — mention it in the posting; the loop it touches is unchanged there. |

Each is a standalone posting (one patch), so no cover letter: the problem, the measurement and
the change are in the commit message. None depends on the zero-copy base (the FUSE/io_uring
kmbuf series the daemon speaks): that base is NOT upstream and will not be — see the evidence
note — so nothing here can lean on it.

**Not produced, judged** (`docs/evidence/rung-10-kernel-mainline.md` has the reasoning):
0023 (`FUSE_TIME_LIMITS`) is upstreamable as an RFC once the INIT flag is allocated by the
fuse maintainer (the sqz form sits at bit 62 on purpose); 0024 (zc retention), 0026 (per-queue
background accounting), 0030 (several rings per queue) and 0031 (cheaper round trip) wait for
the daemon's port to mainline's buffer-pool/zero-copy UAPI or a re-expression on it (0031's
part 4, the attr_version bump, is already in `fuse/for-next` as `fc60668db855`); 0025 (nvme
host-scoped fabric subsystems) is product-specific and stays ours; 0027 is upstream
(`989db66818a4`, `fuse/for-next`, 7.4 material).
