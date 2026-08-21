# Upstream bug report draft — zero-length `IORING_OP_READ_FIXED` NULL deref

**Status:** draft, awaiting local verification on the reporter's box.
**Send to:** `io-uring@vger.kernel.org`
**Cc:** `linux-btrfs@vger.kernel.org`, `linux-fsdevel@vger.kernel.org`,
Jens Axboe `<axboe@kernel.dk>`
**Found by:** the SqueezeFS test suite (a zero-length durable clip
legitimately asks the block device for 0 bytes), 2026-08-21.

Before sending, run the verification checklist at the bottom and paste
your own `dmesg` excerpt + `uname -a` in place of the sample.

---

## Subject

`[BUG] io_uring: NULL pointer deref in iov_iter_alignment_bvec() from zero-length IORING_OP_READ_FIXED on an O_DIRECT file`

## Body

Hi,

A zero-length `IORING_OP_READ_FIXED` against a file opened `O_DIRECT`
NULL-derefs in `iov_iter_alignment_bvec()` on filesystems whose direct
read path checks iter alignment without first short-circuiting an empty
iter. Reproducer attached (liburing, ~120 lines).

Observed on 7.1.8 (x86_64). The three files involved are byte-identical
to the kernel.org 7.1.8 tarball on my box, so this is not a
distro/out-of-tree artifact; I diffed them explicitly because my kernel
also carries an out-of-tree series (which touches neither
`lib/iov_iter.c`, `io_uring/rw.c`, nor `fs/btrfs/*`).

### Splat

```
BUG: kernel NULL pointer dereference, address: 0000000000000008
#PF: supervisor read access in kernel mode
#PF: error_code(0x0000) - not-present page
RIP: 0010:iov_iter_alignment_bvec+0xf/0x70
Call Trace:
 <TASK>
 iov_iter_alignment
 btrfs_direct_read
 btrfs_file_read_iter
 __io_read
 io_read_fixed
 io_issue_sqe
 io_submit_sqes
 __do_sys_io_uring_enter
 do_syscall_64
```

### Mechanism

`io_import_fixed()` deliberately installs an EMPTY bvec iter — with a
**NULL** `bvec` pointer — for a zero-length import:

```c
/* io_uring/rsrc.c */
static int io_import_fixed(int ddir, struct iov_iter *iter,
                           struct io_mapped_ubuf *imu,
                           u64 buf_addr, size_t len)
{
        ...
        if (unlikely(!len)) {
                iov_iter_bvec(iter, ddir, NULL, 0, 0);
                return 0;
        }
```

`iov_iter_alignment()` guards the zero-length case on its **ubuf** arm
but not on its **bvec** arm:

```c
/* lib/iov_iter.c */
unsigned long iov_iter_alignment(const struct iov_iter *i)
{
        if (likely(iter_is_ubuf(i))) {
                size_t size = i->count;
                if (size)
                        return ((unsigned long)i->ubuf + i->iov_offset) | size;
                return 0;                      /* <-- guarded */
        }
        ...
        if (iov_iter_is_bvec(i))
                return iov_iter_alignment_bvec(i);   /* <-- not guarded */
```

and `iov_iter_alignment_bvec()` is a `do {} while`, so it dereferences
`bvec->bv_len` **before** testing `size`:

```c
static unsigned long iov_iter_alignment_bvec(const struct iov_iter *i)
{
        const struct bio_vec *bvec = i->bvec;      /* NULL */
        ...
        do {
                size_t len = bvec->bv_len - skip;  /* NULL + 8 */
                ...
        } while (size);
```

`bv_len` is at offset 8 in `struct bio_vec`, matching the faulting
address `0x8`.

Note `iov_iter_alignment_iovec()` has the same do/while shape, but the
iovec/kvec arms are not reachable with a NULL base from this path.

### Reachability is per-filesystem

A filesystem is affected iff its `O_DIRECT` read path reaches
`iov_iter_alignment()` without an earlier zero-count early return.
Callers of `iov_iter_alignment()` in 7.1.8:

| Site | Note |
|---|---|
| `fs/btrfs/direct-io.c:837` (`check_direct_IO`) | **affected** — `btrfs_file_read_iter()` dispatches to `btrfs_direct_read()` with no zero-count guard (verified: oops above) |
| `fs/ext4/file.c:66`, `:201` | guarded on read — `ext4_file_read_iter()` has `if (!iov_iter_count(to)) return 0;`. The **write** site (`:201`) is worth a second look for zero-length `WRITE_FIXED` |
| `fs/ext2/file.c:240`, `fs/exfat/file.c:696`, `fs/f2fs/file.c:4788`, `fs/ntfs3/file.c:59` | untested by me — same shape, guards not audited |
| `fs/direct-io.c:1121` (legacy `do_blockdev_direct_IO`) | untested; any filesystem still on the legacy DIO path inherits it |
| `block/blk-map.c:511`, `block/bio-integrity.c:394` | untested; different entry (passthrough / integrity) |

XFS does not appear in that list (its iomap path checks alignment
differently), so it should be a clean negative control.

### Blast radius note

The oops kills **only the submitting task**. In a threaded program (mine
is a storage daemon with per-device io_uring worker threads) the worker
dies mid-`io_uring_enter()` without unwinding: its `JoinHandle` never
completes, its channel is never dropped, and every caller parked on that
worker waits forever. From userspace it presents as a silent hang, not a
crash — which is what cost me the debugging time; the splat was only
visible in `dmesg`.

### Suggested fix

Either guard the bvec arm like the ubuf arm:

```diff
--- a/lib/iov_iter.c
+++ b/lib/iov_iter.c
@@ static unsigned long iov_iter_alignment_bvec(const struct iov_iter *i)
 {
 	const struct bio_vec *bvec = i->bvec;
 	unsigned res = 0;
 	size_t size = i->count;
 	unsigned skip = i->iov_offset;
 
+	if (!size)
+		return 0;
+
 	do {
```

…or, if an empty bvec iter is considered malformed rather than legal,
have `io_import_fixed()` install something walkable for the zero-length
case. The first seems more in keeping with the ubuf arm's existing
behavior, and fixes every caller at once (the legacy DIO path and the
block sites included).

### Reproducer

`zero_len_readfixed_oops.c` (attached). Build and run:

```sh
gcc -O2 -o zero_len_readfixed_oops zero_len_readfixed_oops.c -luring
./zero_len_readfixed_oops /mnt/<fs-under-test>/probe.bin
```

It runs two controls first — a zero-length **non-fixed** read (ubuf arm,
returns 0 cleanly) and a normal-length **fixed** read (proves the
registered path works) — then submits the zero-length fixed read. On an
affected pair the process dies by SIGKILL at that point and the splat
lands in `dmesg`; on an unaffected pair it prints `SURVIVED`.

Thanks,
<your name>

---

## Verification checklist (do this before sending)

1. **Build the probe as your user** (inside `nix-shell`/direnv — a compile
   under `sudo` loses `NIX_CFLAGS_COMPILE` and cannot find `liburing.h`):
   ```fish
   fish docker/kernel-sqz/probes/zero_len_readfixed_matrix.fish --build
   ```
2. **Reproduce on btrfs as root** (the run reuses the binary from step 1;
   `env "PATH=$PATH"` carries the shell's mkfs/mount tools through sudo):
   ```fish
   sudo env "PATH=$PATH" fish \
     docker/kernel-sqz/probes/zero_len_readfixed_matrix.fish btrfs
   ```
   Expect: process killed, splat in `dmesg`, taint word non-zero.
3. **Sweep the others**, one per boot for clean attribution (`ext4`,
   `ext2`, `f2fs`, `exfat`, and `xfs` as the negative control). Record
   each result in the table above, replacing "untested by me".
4. **Capture the real splat**: `dmesg | sed -n '/BUG: kernel NULL/,+25p'`
   and paste it over the sample; add `uname -a` and, if you want to be
   thorough, confirm your three files match vanilla:
   ```sh
   diff <vanilla>/lib/iov_iter.c $SP/lib/iov_iter.c   # $SP = linux-src-patched
   ```
5. **Reboot** before any performance work — the box stays tainted `G D`.

Plain-text email, no HTML, reproducer inline or attached; `git
send-email`-style plain body is what the list expects.
