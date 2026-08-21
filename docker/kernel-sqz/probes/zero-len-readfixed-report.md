# Upstream bug report draft — zero-length `IORING_OP_READ_FIXED` NULL deref

**Status:** btrfs and f2fs legs VERIFIED locally 2026-08-21 with the
attached reproducer (the splat below is the real btrfs capture, not a
sample; f2fs's is in §Second confirmed filesystem). ext4/ext2/xfs/exfat
still to sweep before sending.
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

Captured on 7.1.8 (x86_64, Zen 5) running the attached reproducer
against btrfs on a 512 MiB loopback image:

```
BUG: kernel NULL pointer dereference, address: 0000000000000008
#PF: supervisor read access in kernel mode
#PF: error_code(0x0000) - not-present page
PGD 0 P4D 0
Oops: Oops: 0000 [#1] SMP NOPTI
CPU: 6 UID: 0 PID: 1502655 Comm: zero_len_readfi Tainted: G        W           7.1.8 #1 PREEMPT(full)
RIP: 0010:iov_iter_alignment_bvec+0xf/0x70
Code: ... 55 48 89 e5 48 8b 4f 10 8b 77 08 48 8b 57 18 <8b> 41 08 29 f0 48 39 c2 ...
RSP: 0018:ffffd179e244bae0 EFLAGS: 00010246
RAX: 0000000000000000 RBX: 0000000000000fff RCX: 0000000000000000
RDX: 0000000000000000 RSI: 0000000000000000 RDI: ffff8cf6d38aef18
CR2: 0000000000000008
Call Trace:
 <TASK>
 btrfs_direct_read+0x65/0x210 [btrfs]
 btrfs_file_read_iter+0x5a/0x90 [btrfs]
 __io_read+0x19f/0x600
 io_read_fixed+0x9e/0x150
 __io_issue_sqe+0x50/0x160
 io_issue_sqe+0x47/0x4d0
 io_submit_sqes+0x43e/0x990
 __se_sys_io_uring_enter+0x23e/0xa20
 do_syscall_64+0x13f/0xad0
 entry_SYSCALL_64_after_hwframe+0x76/0x7e
 </TASK>
---[ end trace 0000000000000000 ]---
```

Two registers corroborate the analysis below: `RAX = 0` is the NULL
`bvec` being dereferenced at `+8` (`bv_len`), and `RBX = 0xfff` is
btrfs's `blocksize_mask` (`sectorsize - 1`), i.e. we are inside
`check_direct_IO()`'s alignment test. `iov_iter_alignment()` itself is
inlined into `btrfs_direct_read()`, which is why it has no frame.

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

### Second confirmed filesystem: f2fs (a wider entry point)

Same kernel, same reproducer, `mkfs.f2fs` loopback image — and the fault
arrives through the *eligibility decision* rather than a DIO-internal
alignment check:

```
BUG: kernel NULL pointer dereference, address: 0000000000000008
Oops: Oops: 0000 [#2] SMP NOPTI
RIP: 0010:iov_iter_alignment_bvec+0xf/0x70
Call Trace:
 <TASK>
 f2fs_should_use_dio+0xcd/0x120 [f2fs]
 f2fs_file_read_iter+0x59/0x480 [f2fs]
 __io_read+0x19f/0x600
 io_read_fixed+0x9e/0x150
 io_submit_sqes+0x43e/0x990
 __se_sys_io_uring_enter+0x23e/0xa20
```

`f2fs_should_use_dio()` answers "should this read use direct I/O at
all?", so on f2fs the crash does not require the DIO path to be taken —
only for the question to be asked, which `f2fs_file_read_iter()` does
unconditionally for an `IOCB_DIRECT` read. Two filesystems, two
independent call sites, one shared root cause in `lib/iov_iter.c`.

### Reachability is per-filesystem

A filesystem is affected iff its `O_DIRECT` read path reaches
`iov_iter_alignment()` without an earlier zero-count early return.
Callers of `iov_iter_alignment()` in 7.1.8:

| Site | Note |
|---|---|
| `fs/btrfs/direct-io.c:837` (`check_direct_IO`) | **AFFECTED, reproduced** — `btrfs_file_read_iter()` dispatches to `btrfs_direct_read()` with no zero-count guard (splat above; probe exits 137/SIGKILL) |
| `fs/f2fs/file.c:4788` (`f2fs_should_use_dio`) | **AFFECTED, reproduced** — through a *wider* door than btrfs: this is the **DIO eligibility decision**, called unconditionally from `f2fs_file_read_iter()` (`:4904`) before any zero-count check, so the fault happens while merely deciding whether direct I/O applies. Its write-path call (`:5264`) is shielded by `generic_write_checks()` returning 0 first |
| `fs/ext4/file.c:66`, `:201` | guarded on read — `ext4_file_read_iter()` has `if (!iov_iter_count(to)) return 0;`. The **write** site (`:201`) is worth a second look for zero-length `WRITE_FIXED` |
| `fs/ext2/file.c:240`, `fs/exfat/file.c:696`, `fs/ntfs3/file.c:59` | untested by me — same shape, guards not audited |
| `fs/direct-io.c:1121` (legacy `do_blockdev_direct_IO`) | untested; any filesystem still on the legacy DIO path inherits it |
| `block/blk-map.c:511`, `block/bio-integrity.c:394` | untested; different entry (passthrough / integrity) |

XFS does not appear in that list (its iomap path checks alignment
differently), so it should be a clean negative control.

### Security impact

**Not a memory-corruption / RCE candidate.** The faulting access is a
*read* of a fixed offset (`NULL + 8`, `bv_len`), the value read only
feeds an alignment comparison, and nothing attacker-controlled is
written anywhere. Pointing the read at attacker data would require
mapping page 0, which `vm.mmap_min_addr` (65536 by default) forbids and
lowering it needs privilege; even then the iter still carries
`count == 0`, so the follow-on I/O transfers nothing.

**It is an unprivileged local denial of service**, in three escalating
tiers:

1. **Task death.** The oops kills the submitting task. No caps are
   needed to reach it — an ordinary user with io_uring, one registered
   buffer and an `O_DIRECT` file on an affected filesystem.
2. **A wedged ring, and leaked pinned memory.** `io_uring_enter()` holds
   `ctx->uring_lock` across `io_submit_sqes()`
   (`io_uring/io_uring.c:2650`), and a task killed by an oops releases no
   mutexes. Any other thread sharing that ring then blocks on it
   indefinitely, and the ring's teardown path — which also takes that
   mutex — cannot complete, so the registered buffer's **pinned** pages,
   the file reference and the mount reference are not reclaimed. Repeated
   triggering therefore leaks locked memory and can leave the filesystem
   unmountable, which is a resource-exhaustion vector rather than a
   one-shot crash. (Verified: the lock is held at the fault point.
   Inferred, not measured: the exact teardown blocking behavior.)
3. **Full system DoS where `kernel.panic_on_oops=1`** — a common
   hardened/production setting — turns an unprivileged local user's
   zero-length read into an immediate panic.

**Remote reach is indirect but real for storage services.** Any
network-facing daemon that turns client input into a zero-length direct
read inherits a remotely-triggerable version of tier 1/2. That is not
hypothetical for this class of software: the bug was found by a
distributed filesystem whose device layer legitimately asks for 0 bytes.

Exposure is reduced where io_uring is restricted (Docker's default
seccomp profile blocks it; `kernel.io_uring_disabled=2` disables it) and
is limited to filesystems whose direct-read path reaches
`iov_iter_alignment()` unguarded — btrfs and f2fs so far.

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
   sudo env "PATH=$PATH" \
     ./docker/kernel-sqz/probes/zero_len_readfixed_matrix.fish btrfs
   ```
   Expect: process killed, splat in `dmesg`, taint word non-zero.
3. **Sweep the others**, one per boot for clean attribution (`ext4`,
   `ext2`, `f2fs`, `exfat`, and `xfs` as the negative control). Record
   each result in the table above, replacing "untested by me".
4. **Capture each splat**: the harness saves the full block to
   `$STATE/splat-<fs>.txt`; `journalctl -k | sed -n '/BUG: kernel
   NULL/,/end trace/p'` reads it unprivileged too. Add `uname -a`, and
   if you want to be thorough, confirm your three files match vanilla:
   ```sh
   diff <vanilla>/lib/iov_iter.c $SP/lib/iov_iter.c   # $SP = linux-src-patched
   ```
5. **Reboot** before any performance work — the box stays tainted `G D`.

Plain-text email, no HTML, reproducer inline or attached; `git
send-email`-style plain body is what the list expects.
