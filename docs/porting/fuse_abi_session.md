# PORTING SPEC — FUSE kernel ABI + the session layer (INIT, geometry, sideband, codec, `Filesystem`, FORGET, notify, supervisor, watchdog, TTLs)

Reference tree: `/Users/joberdorf/Source/squeezefs` (read-only). Primary files: `crates/fuse3/src/raw/abi.rs`
(1218), `crates/fuse3/src/raw/session.rs` (7530), `raw/request.rs` (66), `raw/reply.rs` (579),
`raw/filesystem.rs` (642), `mount_options.rs` (570), `helper.rs` (62), `lib.rs` (423), `errno.rs` (93),
`notify.rs` (649), `raw/op_trace.rs` (631), `raw/read_phase.rs` (907),
`raw/connection/fuse_over_uring.rs` (11171 — ONLY the ABI constants, `TransportGeometry`, the delivery
codec, `apply_reply`/`fail_ent` and the gauge statics are in scope here; the worker loop, slot machine,
drain groups, leases and kmbuf/zc are the ring-transport spec), `raw/connection/uring_conn.rs` (2278 —
INIT read + the uring→classical body reconstruction), `raw/connection/kmbuf.rs` (2238 — uring init
flags + the opcode-ladder probes), daemon side `src/fuse_client.rs` (34397), `src/supervisor.rs` (577),
`src/mem_budget.rs`, `src/env_knobs.rs`, `src/main.rs`. Every `file:line` is into that tree; bare
`session.rs:N` means `crates/fuse3/src/raw/session.rs`.

Charter anchors: §3 (platform/probes), §4 (bring-up, geometry, sideband, notify, teardown, supervisor,
watchdog, TTL mount options), §12 (extern structs, comptime size asserts, one errno table).
Decision 0002: the reference is evidence; every QUIRK below gets a ledger row before we diverge.

---

## 0. Scope in one paragraph

The reference's `fuse3` fork owns `/dev/fuse` from `mount(2)` to the last FORGET: it reads ONE classical
`FUSE_INIT` via io_uring `Readv` (uring_conn.rs:1318), negotiates the reply flag by flag (session.rs:6451–
6624), resolves the ring geometry BEFORE serializing `fuse_init_out` (session.rs:1857–1879), writes
`fuse.enable_uring=Y` (fuse_over_uring.rs:2953–2976), writes the INIT reply classically (session.rs:1960),
REGISTERs every queue (session.rs:1977), then turns the primary session into a permanent classical
"sideband" reader (session.rs:960–991) while N per-queue `Session` clones drain the ring. Every request
parses as `fuse_in_header ‖ op_in ‖ body` through a bincode LE fixint codec (helper.rs:56–61); every reply
is `fuse_out_header ‖ out-struct [‖ body]`; errors are a 16-byte header with `error = -errno`
(session.rs:5746–5758). The `Filesystem` trait (filesystem.rs) is the daemon's surface — every op defaults
to `ENOSYS` except `forget`/`batch_forget` (no-op), `fsync`/`releasedir` (`Ok`). FORGET/BATCH_FORGET
return per-inode `nlookup` references that the daemon decrements, evicting at zero and tripping
`open_count_stranded` if open handles remain (fuse_client.rs:10025–10053). Notifications are out-frames
with `unique = 0` and the notify code in `error` (notify.rs:86–109). An external supervisor stats
`.stats` every 5 s and writes `/sys/fs/fuse/connections/<minor>/abort` after 30 s (supervisor.rs);
an in-daemon watchdog LOGS overdue ops every 5 s and never cancels (fuse_client.rs:1988–2047).

---

## 1. The kernel ABI, byte-exact

Wire endianness: **native = little-endian on every target** (the codec is `bincode … with_little_endian()
.with_fixint_encoding()`, helper.rs:56–61; `fuse_out_header` fields are hand-written `to_le_bytes` at
session.rs:1513–1514). Zig: `extern struct` with LE fields, `comptime` size asserts on every row below,
canonical total decode = "exactly N bytes" (no `allow_trailing_bytes`, which the reference enables).

### 1.1 Protocol version
- `FUSE_KERNEL_VERSION = 7` (abi.rs:43). `FUSE_KERNEL_MINOR_VERSION = 36` **exactly** (abi.rs:45–53): the
  extended-init revision; the 31→36 audit found no `fc->minor`-gated kernel behaviour in (23, 36]; raising
  it needs a fresh audit of every `fc->minor` gate (pinned, session.rs:7276–7280).
- `FUSE_MIN_READ_BUFFER_SIZE = 8192` (abi.rs:41; kernel `FUSE_MIN_READ_BUFFER`).
- `DEFAULT_TIME_GRAN = 1` (abi.rs:55), `DEFAULT_MAP_ALIGNMENT = 0` (abi.rs:64 — correct iff
  `FUSE_MAP_ALIGNMENT` is never advertised, which it is not).

### 1.2 Headers and attribute structs
| struct | size | fields (type@offset) | cite |
|---|---|---|---|
| `fuse_in_header` | 40 | len u32@0, opcode u32@4, unique u64@8, nodeid u64@16, uid u32@24, gid u32@28, pid u32@32, total_extlen u16@36 + padding u16@38 (ref names the tail `_padding u32`; uapi ≥ 7.40 splits it — 0 unless `FUSE_SECURITY_CTX`/`CREATE_SUPP_GROUP` negotiated, which they are not) | abi.rs:1018–1029 |
| `fuse_out_header` | 16 | len u32@0, error i32@4, unique u64@8 | abi.rs:1033–1039 |
| `fuse_attr` | 88 | ino@0 size@8 blocks@16 atime@24 mtime@32 ctime@40 (u64); atimensec@48 mtimensec@52 ctimensec@56 mode@60 nlink@64 uid@68 gid@72 rdev@76 blksize@80 (u32); flags u32@84 (ref `_padding`; uapi `FUSE_ATTR_SUBMOUNT`/`DAX` — always 0) | abi.rs:272–298 |
| `fuse_kstatfs` | 80 | blocks@0 bfree@8 bavail@16 files@24 ffree@32 (u64); bsize@40 namelen@44 frsize@48 padding@52 (u32); spare[6] u32@56 | abi.rs:300–321 |
| `fuse_entry_out` | 128 | nodeid u64@0, generation u64@8, entry_valid u64@16, attr_valid u64@24, entry_valid_nsec u32@32, attr_valid_nsec u32@36, attr@40 | abi.rs:543–553 |
| `fuse_attr_out` | 104 | attr_valid u64@0, attr_valid_nsec u32@8, dummy u32@12, attr@16 | abi.rs:592–599 |
| `fuse_statfs_out` | 80 | st: fuse_kstatfs | abi.rs:789–793 |
| `fuse_file_lock` | 24 | start u64@0, end u64@8, type u32@16, pid u32@20 | abi.rs:322–330 |

The attr seconds fields are `u64` on the wire but the kernel casts them to `time_t` (i64): `attr.atime.sec
as u64` (reply.rs:60–66). `fuse_attr.blksize` is the daemon's `FileAttr.blksize`.

### 1.3 INIT
| struct | size | fields | cite |
|---|---|---|---|
| `fuse_init_in` | 16 classical / 64 extended (≥ 7.36) | major u32@0, minor u32@4, max_readahead u32@8, flags u32@12; **flags2 u32@16** valid iff `flags & FUSE_INIT_EXT`; unused[11] u32@20 | abi.rs:866–873; read of flags2 session.rs:1780–1784 (`data.len() >= 20`) |
| `fuse_init_out` | **64** | major u32@0, minor u32@4, max_readahead u32@8, flags u32@12, max_background u16@16, congestion_threshold u16@18, max_write u32@20, time_gran u32@24, max_pages u16@28, map_alignment u16@30, flags2 u32@32, max_stack_depth u32@36, request_timeout u16@40, unused[3] u16@42, **time_min i64@48, time_max i64@56** (sqz carve-out of upstream `unused[11]`; natural alignment kept) | abi.rs:877–904; pinned 64 B + offsets 48/56 session.rs:7130–7164 |

Init flags (classical word, bits 0..31) — abi.rs:92–189: ASYNC_READ 1<<0, POSIX_LOCKS 1<<1 (cfg(test):
never echoed), FILE_OPS 1<<2, ATOMIC_O_TRUNC 1<<3, EXPORT_SUPPORT 1<<4, BIG_WRITES 1<<5, DONT_MASK 1<<6,
[SPLICE_WRITE/MOVE/READ 1<<7/8/9 deleted — never advertised, abi.rs:118–120], FLOCK_LOCKS 1<<10,
HAS_IOCTL_DIR 1<<11, AUTO_INVAL_DATA 1<<12, DO_READDIRPLUS 1<<13, READDIRPLUS_AUTO 1<<14, ASYNC_DIO 1<<15,
WRITEBACK_CACHE 1<<16, NO_OPEN_SUPPORT 1<<17, PARALLEL_DIROPS 1<<18, HANDLE_KILLPRIV 1<<19, POSIX_ACL
1<<20 (cfg(test): never echoed), ABORT_ERROR 1<<21, MAX_PAGES 1<<22, CACHE_SYMLINKS 1<<23,
NO_OPENDIR_SUPPORT 1<<24, EXPLICIT_INVAL_DATA 1<<25, MAP_ALIGNMENT 1<<26, **HANDLE_KILLPRIV_V2 1<<28**
(Linux ≥ 5.11, abi.rs:176–183), **INIT_EXT 1<<30** (fuse ≥ 7.36, abi.rs:185–189).
Folded 64-bit word = `flags | flags2 << 32` (`KernelInit::new`, session.rs:345–351):
**FUSE_OVER_IO_URING = 1<<41 = flags2 bit 9** (`FUSE_OVER_IO_URING_FLAGS2 = 1<<9`, fuse_over_uring.rs:109;
Linux ≥ 6.14), **FUSE_TIME_LIMITS = 1<<62 = flags2 bit 30** (sqz patch 0027; abi.rs:191–201; pinned
session.rs:7071–7084).

### 1.4 Per-op request structs (`op_in`)
| struct | size | fields | cite |
|---|---|---|---|
| `fuse_forget_in` | 8 | nlookup u64 | abi.rs:555–559 |
| `fuse_forget_one` | 16 | nodeid u64@0, nlookup u64@8 | abi.rs:563–571 |
| `fuse_batch_forget_in` | 8 | count u32@0, dummy u32@4; followed by `count × fuse_forget_one` | abi.rs:575–580 |
| `fuse_getattr_in` | 16 | getattr_flags u32@0 (`FUSE_GETATTR_FH = 1`), dummy u32@4, fh u64@8 | abi.rs:582–588; flag abi.rs:224 |
| `fuse_mknod_in` | 16 | mode@0 rdev@4 umask@8 padding@12 (u32); name follows | abi.rs:613–620 |
| `fuse_mkdir_in` | 8 | mode u32@0, umask u32@4; name follows | abi.rs:624–629 |
| `fuse_rename_in` | 8 | newdir u64; `oldname\0newname\0` follow | abi.rs:633–637 |
| `fuse_rename2_in` | 16 | newdir u64@0, flags u32@8, padding@12; names follow | abi.rs:641–647 |
| `fuse_link_in` | 8 | oldnodeid u64; newname follows | abi.rs:660–664 |
| `fuse_setattr_in` | 88 | valid u32@0 padding@4 fh u64@8 size u64@16 lock_owner u64@24 atime u64@32 mtime u64@40 ctime u64@48 atimensec@56 mtimensec@60 ctimensec@64 mode@68 unused4@72 uid@76 gid@80 unused5@84 (u32) | abi.rs:666–699; 88 pinned lib.rs:376–392 |
| `fuse_open_in` | 8 | flags u32@0, **open_flags u32@4** (uapi ≥ 7.33; carries `FUSE_OPEN_KILL_SUIDGID`) | abi.rs:701–708; pinned abi.rs:1184–1205 |
| `fuse_create_in` | 16 | flags@0 mode@4 umask@8 open_flags@12 (ref `_padding`; uapi ≥ 7.33 names it `open_flags`) ; name follows | abi.rs:712–719 |
| `fuse_release_in` | 24 | fh u64@0, flags u32@8, release_flags u32@12, lock_owner u64@16 | abi.rs:731–738 |
| `fuse_flush_in` | 24 | fh u64@0, unused u32@8, padding@12, lock_owner u64@16 | abi.rs:740–747 |
| `fuse_read_in` | 40 | fh@0 offset@8 (u64), size u32@16, read_flags u32@20, lock_owner u64@24, **flags u32@32** (open flags, O_DIRECT class), padding@36 | abi.rs:749–762 |
| `fuse_write_in` | 40 | fh@0 offset@8, size u32@16, write_flags u32@20, lock_owner u64@24, flags u32@32, padding@36; payload follows | abi.rs:766–776 |
| `fuse_fsync_in` | 16 | fh u64@0, fsync_flags u32@8 (bit 0 = datasync, session.rs:3249), padding@12 | abi.rs:795–801 |
| `fuse_setxattr_in` | 8 | size u32@0, flags u32@4 (no `FUSE_SETXATTR_EXT`: the 16-byte form is never negotiated); `name\0value` follow | abi.rs:805–814 |
| `fuse_getxattr_in` | 8 | size u32@0, padding@4; name follows (GETXATTR) / nothing (LISTXATTR) | abi.rs:818–827 |
| `fuse_lk_in` | 48 | fh u64@0, owner u64@8, lk(24)@16, lk_flags u32@40 (`FUSE_LK_FLOCK = 1`), padding@44 — `file-lock` feature only | abi.rs:838–847 |
| `fuse_access_in` | 8 | mask u32@0, padding@4 | abi.rs:859–864 |
| `fuse_interrupt_in` | 8 | unique u64 (the interrupted request) | abi.rs:931–935 |
| `fuse_bmap_in` | 16 | block u64@0, blocksize u32@8, padding@12 | abi.rs:937–943 |
| `fuse_ioctl_in` | 32 | fh u64@0, flags u32@8, cmd u32@12, arg u64@16, in_size u32@24, out_size u32@28 | abi.rs:953–962 |
| `fuse_poll_in` | 24 | fh u64@0, kh u64@8, flags u32@16 (`FUSE_POLL_SCHEDULE_NOTIFY = 1`), events u32@20 | abi.rs:980–987 |
| `fuse_fallocate_in` | 32 | fh@0 offset@8 length@16 (u64), mode u32@24, padding@28 | abi.rs:1006–1014 |
| `fuse_notify_retrieve_in` | 40 | dummy1 u64@0, offset u64@8, size u32@16, dummy2@20, dummy3 u64@24, dummy4 u64@32 ("matches the size of fuse_write_in") | abi.rs:1130–1140 |
| `fuse_lseek_in` | 24 | fh u64@0, offset u64@8, whence u32@16, padding@20 | abi.rs:1142–1149 |
| `fuse_copy_file_range_in` | 56 | fh_in@0 off_in@8 nodeid_out@16 fh_out@24 off_out@32 len@40 flags@48 (u64) | abi.rs:1159–1169 |

### 1.5 Per-op reply structs (`out`)
| struct | size | fields | cite |
|---|---|---|---|
| `fuse_open_out` | 16 | fh u64@0, open_flags u32@8, backing_id i32@12 (ref `_padding`; uapi ≥ 7.40 PASSTHROUGH — 0) | abi.rs:723–729 |
| `fuse_write_out` | 8 | size u32@0, padding@4 | abi.rs:780–785 |
| `fuse_getxattr_out` | 8 | size u32@0, padding@4 | abi.rs:831–836 |
| `fuse_lk_out` | 24 | lk | abi.rs:853–857 |
| `fuse_bmap_out` | 8 | block u64 | abi.rs:947–951 |
| `fuse_ioctl_out` | 16 | result i32@0, flags u32@4, in_iovs u32@8, out_iovs u32@12 | abi.rs:971–978 |
| `fuse_ioctl_iovec` | 16 | base u64, len u64 | abi.rs:964–969 |
| `fuse_poll_out` | 8 | revents u32@0, padding@4 | abi.rs:991–996 |
| `fuse_lseek_out` | 8 | offset u64 | abi.rs:1153–1157 |
| `fuse_dirent` | 24 + name, padded to 8 | ino u64@0, off u64@8, namelen u32@16, type u32@20 (`mode >> 12`), name[namelen]@24, zero pad to `(24+namelen+7)&~7` | abi.rs:1043–1051; pad law helper.rs:50–55; type session.rs:3834 |
| `fuse_direntplus` | 152 + name, padded | entry_out(128)@0, dirent(24)@128, name, pad | abi.rs:1055–1060; session.rs:4842–4868 |

### 1.6 Notify out structs (frame = `fuse_out_header{len, error = code, unique = 0}` ‖ struct ‖ tail)
| code | value | struct | size | fields | cite |
|---|---|---|---|---|---|
| `FUSE_POLL` | 1 | `fuse_notify_poll_wakeup_out` | 8 | kh u64 | abi.rs:490, 1000–1004 |
| `FUSE_NOTIFY_INVAL_INODE` | 2 | `fuse_notify_inval_inode_out` | 24 | ino u64@0, off i64@8, len i64@16 (`off < 0` = attrs only; `(0,0)` = all pages to EOF, fuse_client.rs:31375–31379) | abi.rs:493, 1064–1070 |
| `FUSE_NOTIFY_INVAL_ENTRY` | 3 | `fuse_notify_inval_entry_out` ‖ name | 16 + namelen | parent u64@0, namelen u32@8, flags u32@12 (ref `_padding`; uapi ≥ 7.38 `FUSE_EXPIRE_ONLY = 1`; 0 = full invalidate) | abi.rs:496, 1074–1080 |
| `FUSE_NOTIFY_STORE` | 4 | `fuse_notify_store_out` ‖ data | 24 + size | nodeid u64@0, offset u64@8, size u32@16, padding@20 | abi.rs:499, 1107–1114 |
| `FUSE_NOTIFY_RETRIEVE` | 5 | `fuse_notify_retrieve_out` | 32 | notify_unique u64@0, nodeid u64@8, offset u64@16, size u32@24, padding@28 | abi.rs:502, 1118–1126 |
| `FUSE_NOTIFY_DELETE` | 6 | `fuse_notify_delete_out` ‖ name | 24 + namelen | parent u64@0, child u64@8, namelen u32@16, padding@20 | abi.rs:505, 1084–1091 |
| `FUSE_NOTIFY_PRUNE` | 9 | `fuse_notify_prune_out` ‖ nodeids | 16 + 8·count | count u32@0, padding u32@4, spare u64@8, then `count × u64` (uapi 7.45; `oh.len` MUST cover the array — `fuse_dev_do_write` refuses `oh.len != nbytes`) | abi.rs:507–514, 1095–1103; frame notify.rs:54–85, pinned notify.rs:507–544 |

Codes 7 (`FUSE_NOTIFY_RESEND`) and 8 (`FUSE_NOTIFY_INC_EPOCH`) are not in the reference's enum (abi.rs:488–515);
an unknown code is `InvalidNotifyCodeError` (abi.rs:517–538). Notify frames are never read back by the daemon;
the enum exists for `frame_is_notify` (`unique == 0 && error > 0`, notify.rs:100–109).

### 1.7 Opcodes (abi.rs:338–398; `TryFrom<u32>` abi.rs:406–472)
LOOKUP 1, FORGET 2 (no reply), GETATTR 3, SETATTR 4, READLINK 5, SYMLINK 6, MKNOD 8, MKDIR 9, UNLINK 10,
RMDIR 11, RENAME 12, LINK 13, OPEN 14, READ 15, WRITE 16, STATFS 17, RELEASE 18, FSYNC 20, SETXATTR 21,
GETXATTR 22, LISTXATTR 23, REMOVEXATTR 24, FLUSH 25, INIT 26, OPENDIR 27, READDIR 28, RELEASEDIR 29,
FSYNCDIR 30, GETLK 31 / SETLK 32 / SETLKW 33 (`file-lock` feature; never reached — POSIX_LOCKS unadvertised),
ACCESS 34, CREATE 35, INTERRUPT 36 (no reply), BMAP 37, DESTROY 38, IOCTL 39, POLL 40, NOTIFY_REPLY 41
(no reply), BATCH_FORGET 42 (no reply), FALLOCATE 43, READDIRPLUS 44, RENAME2 45, LSEEK 46,
COPY_FILE_RANGE 47. Not known to the reference (unknown ⇒ header-only `-ENOSYS`, session.rs:1424–1432):
SETUPMAPPING 48, REMOVEMAPPING 49, SYNCFS 50, TMPFILE 51, STATX 52. Zig: know the names to ≥ 7.45 for the
log line; still answer `-ENOSYS` (the kernel latches `fc->no_<op>` on ENOSYS for every optional op).

### 1.8 Request/reply flag bits (abi.rs:66–270)
- `fuse_setattr_in.valid`: MODE 1<<0, UID 1<<1, GID 1<<2, SIZE 1<<3, ATIME 1<<4, MTIME 1<<5, FH 1<<6,
  ATIME_NOW 1<<7, MTIME_NOW 1<<8, LOCKOWNER 1<<9, CTIME 1<<10, **KILL_SUIDGID 1<<11** (abi.rs:66–81).
- Release: `FUSE_RELEASE_FLUSH 1<<0`, `FUSE_RELEASE_FLOCK_UNLOCK 1<<1` (abi.rs:219–221).
- Write: `FUSE_WRITE_CACHE 1<<0`, `FUSE_WRITE_LOCKOWNER 1<<1`, **`FUSE_WRITE_KILL_SUIDGID 1<<2`** (abi.rs:230–238).
- Open-in: **`FUSE_OPEN_KILL_SUIDGID 1<<0`** (abi.rs:241–245). Read: `FUSE_READ_LOCKOWNER 1<<1` (abi.rs:248).
- Open-out (`FOPEN_*`, daemon side fuse_client.rs:8764–8846): `FOPEN_DIRECT_IO 1<<0` (virtual
  `.stats/.config` inodes only, fuse_client.rs:27541–27544), `FOPEN_KEEP_CACHE 1<<1` (:8830),
  `FOPEN_NOFLUSH 1<<5` (fuse ≥ 7.35 / Linux ≥ 5.16; honoured only WITHOUT writeback cache — under
  writeback the FLUSH elision is the clean-handle ENOSYS latch, :8770–8798), `FOPEN_PARALLEL_DIRECT_WRITES
  1<<6` (fuse ≥ 7.38 / Linux ≥ 6.2, :8764–8768). Regular open/create replies = `KEEP_CACHE | NOFLUSH |
  PARALLEL_DIRECT_WRITES` (`regular_open_reply_flags`, :8844–8846; pinned :33603–33693).
- Ioctl: COMPAT 1<<0, UNRESTRICTED 1<<1, RETRY 1<<2, 32BIT 1<<3, DIR 1<<4, `FUSE_IOCTL_MAX_IOV = 256`
  (abi.rs:250–266). Poll: `FUSE_POLL_SCHEDULE_NOTIFY 1<<0` (abi.rs:270).
- Killpriv bits pinned together: abi.rs:1211–1217.

### 1.9 FUSE-over-io_uring ABI (fuse_over_uring.rs:108–169; kmbuf.rs:170–205)
| item | value / layout | cite |
|---|---|---|
| `FUSE_URING_IN_OUT_HEADER_SZ` / `FUSE_URING_OP_IN_OUT_SZ` | 128 / 128 | fuse_over_uring.rs:111–112 |
| `fuse_uring_ent_in_out` | 32: flags u64@0, commit_id u64@8, payload_sz u32@16, padding u32@20, reserved u64@24 | :126–133 |
| `fuse_uring_req_header` | **288**: in_out[128]@0 (holds `fuse_in_header` on delivery, `fuse_out_header` on commit), op_in[128]@128 (`in_args[0]` / out-struct), ring_ent_in_out@256 | :135–141; kmbuf.rs:1196–1198 |
| `fuse_uring_cmd_req` | 24: flags u64@0, commit_id u64@8, qid u16@16, **init.flags u16@18** (REGISTER) ≡ **commit.flags u16@18** (COMMIT_AND_FETCH), init.queue_depth u16@20 (zc only), padding[2]@22 — sqz series repurposes 4 of upstream's 6 padding bytes ("uapi 7.46", minor stays 45) | :153–169 |
| SQE128 placement | cmd area (80 B) at SQE offset 48; `buf_index` u16 at SQE offset 40 (kmbuf fixed-buffer id); user-ent REGISTER: `addr` u64@16 = iovec ptr, `len` u32@24 = 2 (header iov + payload iov) | :8586–8641; pinned :9523–9525 |
| `FUSE_IO_URING_CMD_REGISTER` | 1 | :114 |
| `FUSE_IO_URING_CMD_COMMIT_AND_FETCH` | 2 | :115 |
| `FUSE_IO_URING_CMD_RELEASE_PAYLOAD` | 3 (sqz patch 0029, "uapi 7.47"; doubles as the retention PROBE opcode) | :116–119 |
| `FUSE_URING_BUF_RING` | init.flags 1<<0 (kmbuf; series patch 19) | kmbuf.rs:174–176 |
| `FUSE_URING_ZERO_COPY` | init.flags 1<<1 (CAP_SYS_ADMIN; requires BUF_RING) | kmbuf.rs:177–179 |
| `FUSE_URING_PAYLOAD_RETENTION` | init.flags 1<<2 (valid ONLY with ZERO_COPY — kernel EINVAL otherwise) | kmbuf.rs:180–187 |
| `FUSE_URING_COMMIT_RETAIN` | commit.flags 1<<0 | kmbuf.rs:188–194 |
| `FUSE_URING_RINGBUF_GROUP` | bgid 0 | kmbuf.rs:196 |
| `FUSE_URING_FIXED_HEADERS_OFFSET` | 0 (zc: headers at index `queue_depth`, `zc_headers_index`) | kmbuf.rs:200, 247–249 |
| `IORING_REGISTER_KMBUF_RING`/`UNREGISTER` | **per kernel track**: 37/38 (6.19-sqz), 38/39 (7.1-sqz — upstream 7.1 took 37 for `BPF_FILTER`); resolved by the probe ladder, never by version | kmbuf.rs:126–167 |
| `IORING_OFF_KMBUF_RING` / `IORING_OFF_KMBUF_SHIFT` | 0x8800_0000 / 16 (mmap offset = base ‖ bgid << 16) | kmbuf.rs:170–172 |
| `io_uring_buf_reg` (series) | 40: `union{ring_addr u64; buf_size u32}`@0, ring_entries u32@8, bgid u16@12, flags u16@14, resv[3] u64@16 | kmbuf.rs:207–227 |
| `IORING_CQE_F_BUFFER` / `IORING_CQE_BUFFER_SHIFT` | 1 / 16 | kmbuf.rs:202–205 |
| Retention probe verdict | RELEASE_PAYLOAD with impossible commit_id: `-ENOTCONN` (pre-arm) or `-ENOENT` (post-arm) ⇒ Present; `-EINVAL`/`-EOPNOTSUPP`/anything else ⇒ Absent (fail-safe) | kmbuf.rs:281–318 |

---

## 2. The INIT handshake — ordered procedure (session.rs:1179–1997)

0. **Mount** (`Session::mount`, session.rs:817–868): refuse a non-empty mountpoint unless `nonempty`
   (:740–758, `ErrorKind::AlreadyExists`); open `/dev/fuse` (`FuseConnection::new`); `mount(2)` with
   source = `fs_name` or `"fuse"`, type `"fuse"`, `MsFlags` from §10, data = §10 option string (:828–847).
   Unprivileged variant spawns `fusermount3` (:777–813). The mount task runs on an OS thread named
   `fuse3-mount` (:451–466; charter name law applies — row 20/21).
1. **Read INIT classically** (`init_filesystem`, :1179–1251): buffers `header[40]` + `data[8192]`
   (:1184–1185); the read is an io_uring `Readv` on `/dev/fuse` (uring_conn.rs:1318–1326). `Destroy` ⇒
   `UnexpectedEof` (:1191–1195). Unknown opcode ⇒ reply `-ENOSYS` AND fail the session (:1210–1221).
   Opcode ≠ INIT ⇒ fail (:1225–1229). Body bounds via `validated_body(len, filled, 0, cap)` — a lying
   INIT header refuses the SESSION (:1235–1246).
2. **Decode `fuse_init_in`** as the 16-byte prefix (:1744); a decode failure replies a 16-byte header with
   `error = EINVAL` and fails (:1751–1769). `flags2 = data[16..20]` iff `flags & FUSE_INIT_EXT` and
   `data.len() ≥ 20`, else 0 (:1780–1784). Publish `KernelInit{major, minor, flags|flags2<<32}` into a
   process-global `OnceLock` BEFORE `fs.init` (:1785–1790; `kernel_init_info()`, :360–362).
3. **flags2**: ALWAYS `FUSE_OVER_IO_URING_FLAGS2` (1<<9) — "No opt-out" (:1792–1797). Add
   `(FUSE_TIME_LIMITS >> 32)` = 1<<30 iff the kernel's folded word has bit 62 (`negotiate_time_limits`,
   :6380–6386); then `(time_min, time_max) = (−9_223_372_036, +9_223_372_036)` s, else `(0, 0)`
   (:1810–1818; constants :6372–6374 — the whole-second interior of the i64-ns word, floor conservative
   by one second so a kernel clamp to `(sec, 0)` round-trips exactly).
4. **Reply minor + INIT_EXT** (`negotiate_init_ext`, :6412–6426): `reply_minor = min(kernel_minor, 36)`;
   `FUSE_INIT_EXT` rides the reply iff kernel offered it ∧ `flags2 ≠ 0` ∧ `reply_minor ≥ 36`. Without it
   the kernel DISCARDS `flags2` (bit 41 then only via the module-param side door; bit 62 has none)
   (:6388–6411; pinned :7260–7373).
5. **Classical reply flags** (`negotiate_reply_flags`, :6451–6624) — echo iff offered: ASYNC_READ,
   FILE_OPS, ATOMIC_O_TRUNC, EXPORT_SUPPORT (honest only with a REAL generation — the daemon derives it
   from the volume-set identity, :6483–6495), BIG_WRITES, HAS_IOCTL_DIR, AUTO_INVAL_DATA, ASYNC_DIO,
   PARALLEL_DIROPS, MAX_PAGES, CACHE_SYMLINKS (a symlink target is written exactly once, :6600–6616).
   Option-gated: DONT_MASK (`dont_mask`), WRITEBACK_CACHE (`write_back`), NO_OPEN_SUPPORT,
   NO_OPENDIR_SUPPORT, HANDLE_KILLPRIV (v1, `handle_killpriv`), **HANDLE_KILLPRIV_V2**
   (`handle_killpriv_v2`, :6575–6584). READDIRPLUS: `DO_READDIRPLUS` iff offered OR `force_readdir_plus`;
   `READDIRPLUS_AUTO` iff offered AND NOT forced (:6533–6543). **Never**: POSIX_LOCKS (kernel-local
   `posix_lock_file`, :6461–6469), FLOCK_LOCKS (:6515–6519), SPLICE_* (:6509–6513), POSIX_ACL (every create
   would probe default ACLs — blanket EOPNOTSUPP regression, :6586–6592); also not echoed: ABORT_ERROR,
   EXPLICIT_INVAL_DATA, MAP_ALIGNMENT, bits ≥ 31. `reply_flags |= init_ext`; publish into
   `NEGOTIATED_REPLY_FLAGS` BEFORE `fs.init` (:1825–1829; `negotiated_reply_flags()`, :376–378).
6. **`fs.init(request)`** → `ReplyInit{max_write: NonZeroU32}` = the DESIRE (:1832–1856); an `Err` replies a
   header-only `-errno` and fails the mount (:1833–1853). Daemon desire = `SQUEEZEFS_FUSE_MAX_WRITE` verbatim
   else `max(block_size, 1 MiB)`, clamped `[4096, u32::MAX]` (fuse_client.rs:27033–27056).
7. **Resolve transport geometry BEFORE serializing** (:1857–1886): `TransportGeometry::resolve(desire,
   transport_buffer_cap_bytes, max_background_override, congestion_threshold_override)` → the reply's
   `max_background`, `congestion_threshold`, `max_write`, `max_pages` (§3).
8. **Serialize** `fuse_init_out` (:1895–1914): `major 7, minor reply_minor, max_readahead = kernel's
   verbatim` (`negotiate_max_readahead`, :6441–6444 — never clamped, never consulted; published as
   `transport_max_readahead`), `flags, max_background, congestion_threshold, max_write (negotiated),
   time_gran 1, max_pages (negotiated), map_alignment 0, flags2, max_stack_depth 0, request_timeout,
   unused [0;3], time_min, time_max`; out header `len = 16 + 64`. `request_timeout` (Zig, decision
   0003 §Z; the reference sends 0) = `T_owner + T_park_max` in whole seconds (135 s) with
   FUSE_REQUEST_TIMEOUT iff offered; FUSE_HAS_SYNCFS rides only beside it.
9. **`fuse.enable_uring=Y`** (:1933–1956 → fuse_over_uring.rs:2953–2976): read
   `/sys/module/fuse/parameters/enable_uring`; if not `y|1|yes|true|on`, write `Y`, re-read; warn on
   failure. WHY the order: since 7.2.4 (stable 303b6eeedf29) the kernel creates the connection's ring at
   INIT-REPLY time iff the parameter reads Y THEN; a later REGISTER on a ring-less connection is `-EINVAL`
   (pinned as a static source-order rail, :6955–6972). Control-path `write(2)` to sysfs — a sanctioned
   exception to §1.2 (one-shot, not an I/O path).
10. **Write the INIT reply classically** (:1958–1968) — the kernel refuses REGISTER until `fch->initialized`
    (`-EAGAIN`).
11. **REGISTER every queue** (`enable_fuse_over_uring(geom)`, :1976–1983) → `pool.mark_ready()` → stderr +
    info "FUSE-over-io_uring transport armed". Classical traffic that arrived in the INIT→REGISTER window
    is serviced by the sideband (§4).
12. **`SQUEEZEFS_DAEMON_PIPE`**: if set to an fd, write `"ready\n"` (:1985–1990) — the parent's readiness
    signal (main.rs forks; env_knobs.rs:145).
13. Return the NEGOTIATED `max_write` — the dispatch buffers size to what the kernel was told, not the desire
    (:1993–1996); `buffer_size = max(max_write + 40, 8192)` (:1375).
14. **Post-INIT in `inner_mount`** (:953–1071): per queue qid in `0..nqueues` clone the `Session` with its
    own reply channel + reply task + dispatch loop (`inner_mount_worker`, :912–951); mark the primary
    connection `set_classical_sideband()` (:987); register the fused-write dispatcher, the zc hold gate and
    the READ fast-dispatcher (:992–1069 — ring-transport spec).
15. **Daemon `init` hook logs** the kernel protocol, `FOPEN_NOFLUSH` honour (`minor ≥ 35`), any init bit
    above 41 as "UNKNOWN capability", and sets `fuse_killpriv_negotiated` from the published reply flags
    (fuse_client.rs:26499–26571).

A **duplicate INIT** on the dispatch loop is warned and re-handled (:1491–1496). **DESTROY**: run
`fs.destroy`, reply a 16-byte success header INLINE through the connection (not the reply task — shutdown
clears the pending map), then `teardown_over_uring()` and exit the loop (:1498–1529). The read path's
disconnect set — `ENODEV | ECONNABORTED | ENOTCONN | EPIPE | EBADF | ESHUTDOWN` or `NotConnected`/`BrokenPipe`
— ends the session via `fs.destroy(Request::default())` (:1276–1298, :1388–1392).

---

## 3. Transport geometry (fuse_over_uring.rs:2590–2950) + the published gauges

Inputs (`TransportGeometry::resolve`, :2722–2760): `kernel_nqueues = sysconf(_SC_NPROCESSORS_CONF)`
(fallback `available_parallelism`, then 4; :2675–2684 — "what fuse_uring_create() sizes queues by;
qid == kernel cpu id is what NUMA placement relies on"); env `SQUEEZEFS_FUSE_OVER_IO_URING_QUEUES`,
`SQUEEZEFS_FUSE_OVER_IO_URING_Q_DEPTH`; `max_pages_limit()` = `/proc/sys/fs/fuse/max_pages_limit` parsed,
fallback **256**, clamped `[1, 65535]` (:2650–2656 — "the compiled-in default kernels without the sysctl
still carry"); `page_size()` = `sysconf(_SC_PAGE_SIZE)` fallback 4096 (:2661–2668); `buffer_cap_bytes` =
the embedder's cap, else `default_buffer_cap()` = physical RAM / 8, 0 if RAM unknown (:2634–2642).

**The pure plan** (`TransportGeometry::plan`, :2809–2894) — Zig: one pure fn over `derive.Facts` + budget,
with a tie test restating each line:
```
page            = max(page_sz, 512)
limit           = clamp(max_pages_limit, 1, 65535)
pages_for(mw)   = clamp(ceil(mw / page), 1, limit)
payload_for(mw) = max(8192, mw, pages_for(mw) × page)          -- the kernel's ring->max_payload_sz mirror
max_write_target= clamp(desired, max(page, 4096), limit × page)
nqueues         = clamp(env_queues ?? kernel_nqueues, 1, 512)
depth_at(mw)    = clamp(cap / (nqueues × payload_for(mw)), 4, 32)   -- cap = buffer_cap_bytes
if env_depth:   (max_write, depth) = (max_write_target, clamp(env_depth, 1, 32))   -- bypasses BOTH legs
else:
  depth = depth_at(target); floor_arena = nqueues × 4 × payload_for(target); base = min(target, 1 MiB)
  if depth > 4 or floor_arena ≤ cap or target ≤ base: (target, depth)
  else:  fit = cap / (nqueues × 4); mw = clamp((fit / page) × page, base, target); (mw, depth_at(mw))
max_pages       = pages_for(max_write)                           -- describes max_write EXACTLY
payload_sz      = payload_for(max_write)
max_background  = override (> 0) ?? clamp(nqueues × depth, 64, u16::MAX)
congestion      = override (> 0) ?? max_background / 4 × 3
total_payload   = nqueues × depth × payload_sz
```
Constants with WHY: `Q_DEPTH_DESIRED = 32` (:2606 — measured-best: QD32 + mb256 = 316 k IOPS on the
decomposition box; depth 16 alone +2.2×, mb 256 alone +0, both 44 k → 316 k, :2590–2600); `Q_DEPTH_FLOOR = 4`
(:2610 — the pre-L1 shipped posture; at floor the arena is exactly yesterday's `queues × 4 × payload_sz`);
`PAYLOAD_BASE = 1 MiB` (:2617 — the pre-4 MiB-campaign shipped ent = 256 × 4 KiB); `MAX_BACKGROUND_FLOOR = 64`
(:2622 — the historical dead-letter `max_background=64` option's intent); NO fixed buffer ceiling (:2624–2633
— the former 2 GiB cap degraded depth below 32 on > 64-CPU big-RAM boxes for no physical reason; the
structural bound is `nqueues × 32 × payload_sz`); `max_background` ceiling = `u16::MAX` because the wire
field is u16 — "a protocol bound, not a policy constant" (:2795–2805; the fixed 256 was retired 2026-08-04).
Kernel mirror being matched (test :9570–9595): `fc->max_pages = min(limit, max(arg->max_pages, 1))`,
`fc->max_write = max(4096, arg->max_write)`, `ring->max_payload_sz = max(8192, max_write, max_pages ×
PAGE)`, REGISTER refuses `iov[1].iov_len < ring->max_payload_sz` — so `payload_sz == kernel bound` makes
acceptance structural (the pre-fix planner pinned 256 pages while advertising `max_pages = u16::MAX`:
every REGISTER refused on a raised-sysctl box, mount failed, :2766–2774). `nqueues < kernel_nqueues`
warns "will never become ready" (:2752–2758): `is_ring_ready()` needs ≥ 1 ent on EVERY queue.

Tie rows (fuse_over_uring.rs:9534–9779, `page = 4096`): `(32q, 1 MiB, limit 256, cap 2 GiB)` → depth 32,
payload 1 MiB, max_pages 256, mb 1024, ct 768, arena 1 GiB (:9639–9648); `limit ∈ {512,1024,4096,65535}`
with 1 MiB desire → payload 1 MiB, max_pages 256 (:9614–9631); `(4 MiB, limit 1024, 2 GiB)` → mw 4 MiB,
pages 1024, depth 16, mb 512, arena 2 GiB (:9656–9671); `(4 MiB, limit 256)` → mw 1 MiB (:9675–9680);
`(1 MiB, limit 64)` → mw 256 KiB, pages 64 (:9684–9688); ladder: `(4 MiB, 1024, 819 MiB)` → mw 4 MiB,
depth 6; `(4 MiB, 1024, 256 MiB)` → depth 4, mw 2 MiB, pages 512; `(…, 64 MiB)` and `(…, cap 0)` → depth 4,
mw 1 MiB (:9697–9724); `(8q, 4096, 256, 32 MiB)` → mw 4096, pages 1, payload 8192, depth 32 (:9731–9742);
env depth 8 with cap 64 MiB → depth 8, mw 4 MiB (:9748–9763).

Daemon-side cap (`resolve_transport_buffer_cap`, mem_budget.rs:643–679): `SQUEEZEFS_TRANSPORT_MEM_MAX`
(MiB, absolute, 0 legal) > `SQUEEZEFS_TRANSPORT_MEM_PCT` (percent of the resolved budget) > `budget / 8`
(`transport_buffer_cap`, :626–628). Ties: `8 GiB → 1 GiB`, `(8 GiB, "2048", "50") → 2 GiB`,
`FIELD_BUDGET → 22 GiB` (derivation_sweep_tests.rs:45–101). QUIRK: the pct arm parses `f64`, accepts
`12.5`, clamps `> 100` with a warning — decision 0002 row 14 already retires this parser; the Zig cap
takes the integer the ONE parser admitted and refuses `> 100`.

**Published gauges** (statics :2907–2917, readers :2922–2950; `.stats` keys fuse_client.rs:12337–12343):
`transport_queues`, `transport_q_depth`, `transport_payload_buffer_bytes` (= q × d × payload),
`transport_max_background`, `transport_max_write`, `transport_max_pages`, `transport_max_readahead`
(`note_negotiated_max_readahead`, :2948–2950; echoed-verbatim law pinned session.rs:7514–7529). All are
LEVELS (zero until a session arms), stored once by `try_start`. Zig: `counters.Gauge` each.

---

## 4. The sideband session (what it services forever; coexistence with the ring)

- Kernel facts the design rests on (fuse_over_uring.rs:7–21; uring_conn.rs:722–734): with the ring armed
  the kernel STILL queues FORGET/BATCH_FORGET (`fuse_io_uring_ops.send_forget`) and INTERRUPT
  (`.send_interrupt`) on the classical `fiq->pending`; `fuse_resend` splices resends there; regular
  requests can land classically in the unlocked `WRITE_ONCE(fiq->ops, …)` switchover window (dev_uring.c,
  v6.14–v7.1). Without a permanent reader they strand: `fusectl waiting ≥ 1`, `syncfs` blocks, `umount`
  EBUSY (the storm-teardown wedge, 2026-07-08 root cause).
- Mechanism: after arm the PRIMARY `Session` keeps its dispatch loop on the primary connection with
  `classical_sideband = true` (uring_conn.rs:731–734); its `read_vectored` then never drains the uring
  inbound queues (:808–814) and falls through to the classical `Readv` on `/dev/fuse` (:976+). Every reply
  from a sideband-delivered request is `ReplySlot::Classical` → classical vectored write (request.rs:14–20;
  session.rs:1123–1131). Counter `transport_classical_sideband` (`note_classical_sideband`,
  fuse_over_uring.rs:2539–2547) counts sideband deliveries; `> 0` after a storm is asserted by
  `test_classical_sideband_serviced_and_unmount_clean` (multi_queue_tests.rs:765–774).
- The sideband is the ONE serialized reader; its handler futures are spawned onto lanes, so the reader is
  bounded by "one request in flight per `Readv`" and never by a channel — RES-18 explains why it must stay
  unbounded: a bounded channel would delay INTERRUPT deliveries, turning memory pressure into a liveness
  failure (session.rs:5892–5907).
- Notify writes ride the same classical connection (§8). Teardown: the primary's read returns the disconnect
  set → `fs.destroy` (once — `dismount_once`, fuse_client.rs:27065–27068) → loop exit.
- Classical rings: SQ depth `SQUEEZEFS_FUSE_IO_URING_ENTRIES` default 1024 clamp `[64, 4096]`
  (uring_conn.rs:28–37); optional SQPOLL via `SQUEEZEFS_FUSE_IO_URING_SQPOLL_IDLE_MS` (+ `_CPU` pin)
  (:40–63 — charter: "a knob, measured not recommended").

---

## 5. The codec

### 5.1 Request parsing (classical and uring)
- Classical read: `header[40]` + `data[max(max_write + 40, 8192)]` via `Readv`; `n < 40` ⇒ error (dropped,
  loop continues, session.rs:1316–1333); `fuse_in_header` decoded from `header` (:1335–1350);
  `filled = n − 40` (:1360).
- Uring delivery (fuse_over_uring.rs:7156–7237 → uring_conn.rs:843–972): the worker reads `opcode =
  in_out[4..8]`, `payload_sz = ring_ent_in_out.payload_sz`, `commit_id`, builds `header_and_op = in_out[..40]
  ‖ op_in[..128]` (:7198–7200), `capped_sz = min(payload_sz, ent.payload_len)` (:7201–7205); FUSE_WRITE
  payload = zero-copy LEASE over the registered buffer, every other opcode copies (:7216–7237). The
  connection then RECONSTRUCTS a classical contiguous body: `body_need = in_header.len − 40`; WRITE copies
  only `fuse_write_in` (40 B) from `op_in` and hands the payload `Bytes` separately (:923–931); otherwise
  `from_op = min(body_need − payload.len(), op_in.len())`, `from_payload = …` copied in order (:933–947);
  **if `filled == 0`, a heuristic fallback copies payload first then a NUL-terminated prefix of `op_in`**
  (:948–968). QUIRK Q4 (§15).
- Settled detail (dev_uring.c `fuse_uring_args_to_ring` + `fuse_set_zero_arg0`, confirmed against the
  reference's "LOOKUP often has name only in payload" comment, uring_conn.rs:949): `op_in` receives
  `in_args[0]` ONLY; for the opcodes whose arg0 is the kernel's zero-size placeholder — LOOKUP, SYMLINK,
  UNLINK, RMDIR, REMOVEXATTR, NOTIFY_REPLY (`fuse_retrieve`) — `op_in` is EMPTY (stale bytes from earlier
  deliveries) and every argument, including NOTIFY_REPLY's `fuse_notify_retrieve_in`, leads the payload.
  Classically a zero-size arg contributes nothing, so the logical body `in_args[0] ‖ in_args[1..]` is
  identical on both venues: the port's per-opcode table carries `in_args[0].size` (0 for those six) and ONE
  parser serves both (`codec.Delivery`, decision 0002 row 54).
- **Body bounds** (`validated_body`, session.rs:5846–5863; pinned :6861–6940): `declared = len − 40`
  (underflow ⇒ `ShortHeader`); `available = filled + payload_len`; `declared > available` ⇒ `Overdeclared`;
  else `Valid(min(declared, filled, buf_len))`. Refusal ⇒ reply `-EINVAL`, never present the slice
  (:1474–1488). For a D14 held-slot WRITE the placeholder payload is empty and the held length is taken
  from the connection (:1454–1473). INIT uses `payload_len = 0` and refuses the session (:1235–1246).
- Op decode: `bincode` on the body prefix; decode failure ⇒ `-EINVAL` (every handler, e.g. :2868–2878);
  names via `get_first_null_position` (helper.rs:21–23) — missing NUL ⇒ `-EINVAL` (:2007–2016).
  `BATCH_FORGET`: body after the 8-byte header, `while data.len() ≥ 16` decode `fuse_forget_one`,
  `forgets.len() != count` ⇒ ignore (:4636–4672; `batch_forget_body`, :5793–5795). `NOTIFY_REPLY`: body
  after the 40-byte `fuse_notify_retrieve_in` must hold `size` bytes (`notify_retrieve_body`, :5784–5789).
- `Request{unique, uid, gid, pid, slot}` from the header (request.rs:39–66); `slot` = the delivery's
  `ReplySlot` (`Classical` | `Ring{qid, ent_idx, commit_id}`) — the reply address travels WITH the request;
  no `unique → slot` map exists (request.rs:3–11).

### 5.2 Reply serialization (per op)
Header-only success/error: `fuse_out_header{len 16, error = -errno | 0, unique}` (`reply_error_in_place`,
session.rs:5746–5758; `Errno → c_int` negates, errno.rs:12–16). Body replies: `len = 16 + sizeof(out)
[+ body.len()]`. Per op: LOOKUP/SYMLINK/MKNOD/MKDIR/LINK → `fuse_entry_out` (128); GETATTR/SETATTR →
`fuse_attr_out` (104; GETATTR echoes `dummy = getattr_in.dummy`, :2165); READLINK → raw bytes; OPEN/OPENDIR
→ `fuse_open_out` (16); READ → raw bytes truncated to `read_in.size` (:5391–5393); WRITE/COPY_FILE_RANGE →
`fuse_write_out` (8); STATFS → `fuse_statfs_out` (80); RELEASE/FSYNC/SETXATTR/REMOVEXATTR/FLUSH/
RELEASEDIR/FSYNCDIR/ACCESS/UNLINK/RMDIR/RENAME/RENAME2/FALLOCATE/SETLK → header only; GETXATTR/LISTXATTR →
`ReplyXAttr::Size(n)` = `fuse_getxattr_out{size}` with `error 0` (a SUCCESS reply — a positive errno here
is malformed and makes `getxattr(2)` fail EINVAL; ERANGE is the FILESYSTEM's `Err(ERANGE)`, :3432–3460) |
`Data(bytes)` = header + raw bytes (:3462–3478); READDIR → packed `fuse_dirent` records, each
`24 + namelen` padded to 8, stop when `entry_data.len() + dir_entry_size > read_in.size` (:3801–3845);
READDIRPLUS → packed `fuse_direntplus` (:4820–4870); CREATE → `fuse_entry_out ‖ fuse_open_out`
(:4251–4272); BMAP → `fuse_bmap_out`; IOCTL → `fuse_ioctl_out` [‖ data]; POLL → `fuse_poll_out`; LSEEK →
`fuse_lseek_out`; GETLK → `fuse_lk_out`.
- `FuseData = Either<Vec<u8>, (Vec<u8>, Bytes, Option<Arc<dyn Any>>)>` — header, optional body, optional
  keep-alive backing (raw/mod.rs:66–72).
- **Oversize rule** (`apply_reply`, fuse_over_uring.rs:8400–8416): `want = header.len() − 16 + body.len()
  > ent.payload_len` ⇒ header-only `-EIO`, `payload_sz = 0`, `transport_replies_oversize += 1`, error log —
  never a truncated body under a header claiming the full length (pinned :8742–8760). Zig: compare against
  `payload_sz` before serialization and fail the op through the one errno table.
- Header-only error reply law: `fail_ent` (fuse_over_uring.rs:8299–8341) = THE one non-reply exit: header
  `error_out_header(unique, errno)`, no body, legal even while a payload lease lives (:8316–8319); a
  commit that cannot be pushed is `transport_requests_abandoned` (:8328–8338). The session-side twin:
  `ReplyTx` owes exactly one reply from construction; `Drop` while owing synthesizes `-EIO` + logs
  (session.rs:256–294); a dead reply task falls back to `commit_reply_direct` (:224–253).
- Reply write verdict on the classical device (`reply_write_verdict`, :116–123): notify + ENOENT ⇒ count
  `fuse3_notify_enoent`; notify + other ⇒ count `fuse3_notify_failed`, WARN once per errno class (8-slot
  announced set, read_phase.rs:386–409); request + ENOENT ⇒ WARN "interrupted"; request + other ⇒ fatal
  (ends the reply task).

### 5.3 Dirent `type` and padding
`type = mode_from_kind_and_perm(kind, 0) >> 12` (session.rs:3834); `get_padding_size(n) = ((n + 7) & ~7) − n`
(helper.rs:50–55). `FileType` ↔ `S_IF*` (lib.rs:181–217).

---

## 6. The `Filesystem` interface (filesystem.rs) — every op, reply shape, default

`Result<T> = Result<T, Errno>` (lib.rs:177); `Errno(c_int)`, `From<IoError>` maps a raw os error else
`EIO` (errno.rs:24–33). Request = `Request{unique, uid, gid, pid, slot}`. Required (no default): `init →
ReplyInit{max_write}` (filesystem.rs:21), `destroy` (:99), `getlk`/`setlk` (file-lock feature only,
:417–453). Everything else has a default:

| op | signature (after `req`) | reply | default | notes |
|---|---|---|---|---|
| lookup | parent, name | `ReplyEntry{ttl, attr, generation}` | ENOSYS | negative entry = `ReplyEntry::negative(ttl)` → nodeid 0 + entry_valid (reply.rs:99–144; pinned :554–565) |
| forget | inode, nlookup | — | no-op | no wire reply ever (session.rs:2086, 2100–2103) |
| getattr | inode, fh: Option, flags | `ReplyAttr{ttl, attr}` | ENOSYS | `fh` present iff `FUSE_GETATTR_FH` (session.rs:2139–2143) |
| setattr | inode, fh: Option, `SetAttr` | `ReplyAttr` | ENOSYS | `SetAttr` from `valid` bits, `*_NOW` resolved on `CLOCK_REALTIME_COARSE` (lib.rs:262–277, 279–329); `kill_suidgid` from bit 11 |
| readlink | inode | `ReplyData` | ENOSYS | |
| symlink | parent, name, link | `ReplyEntry` | ENOSYS | |
| mknod | parent, name, mode, rdev | `ReplyEntry` | ENOSYS | umask NOT passed (abi `_umask`) |
| mkdir | parent, name, mode, umask | `ReplyEntry` | ENOSYS | |
| unlink / rmdir | parent, name | `()` | ENOSYS | |
| rename | parent, name, new_parent, new_name | `()` | ENOSYS | |
| rename2 | … + flags | `()` | ENOSYS | |
| link | inode, new_parent, new_name | `ReplyEntry` | ENOSYS | |
| open | inode, flags, open_flags | `ReplyOpen{fh, flags}` | ENOSYS | `open_flags` carries `FUSE_OPEN_KILL_SUIDGID` (:224–242) |
| read | inode, fh, offset, size, flags | `ReplyData{data, backing, zc_prefilled, zc_fd_body}` | ENOSYS | `flags` = file open flags (:249–250); session truncates to `size` |
| write | inode, fh, offset, data: Bytes, write_flags, flags | `ReplyWrite{written}` | ENOSYS | `data` is the payload LEASE on the ring |
| statfs | inode | `ReplyStatFs{blocks,bfree,bavail,files,ffree,bsize,namelen,frsize}` | ENOSYS | |
| release | inode, fh, flags, lock_owner, flush: bool | `()` | ENOSYS | `flush = release_flags & FUSE_RELEASE_FLUSH` (session.rs:3180) |
| fsync | inode, fh, datasync | `()` | **Ok(())** | |
| setxattr | inode, name, value, flags, position | `()` | ENOSYS | `position` is macOS-only (0 on Linux) |
| getxattr | inode, name, size | `ReplyXAttr::{Size(u32), Data(Bytes)}` | ENOSYS | `size == 0` ⇒ answer `Size`; too small ⇒ `Err(ERANGE)` (:329–331) |
| listxattr | inode, size | `ReplyXAttr` | ENOSYS | same law |
| removexattr | inode, name | `()` | ENOSYS | |
| flush | inode, fh, lock_owner | `()` | ENOSYS | the kernel latches `fc->no_flush` on ENOSYS — the writeback-cache FLUSH elision (fuse_client.rs:8777–8780) |
| opendir | inode, flags | `ReplyOpen` | ENOSYS | |
| readdir | parent, fh, offset: i64 | `ReplyDirectory{entries: Stream<DirectoryEntry{inode, kind, name, offset}>}` | ENOSYS | `force_readdir_plus` ⇒ session answers ENOSYS before the hook (session.rs:3758–3762) |
| releasedir | inode, fh, flags | `()` | **Ok(())** | |
| fsyncdir | inode, fh, datasync | `()` | ENOSYS | |
| access | inode, mask | `()` | ENOSYS | never called under `default_permissions` |
| create | parent, name, mode, flags | `ReplyCreated{ttl, attr, generation, fh, flags}` | ENOSYS | umask/open_flags NOT passed (abi `_umask`, `_padding`) |
| interrupt | unique | `()` | ENOSYS | result only logged; no wire reply (session.rs:4276–4319) |
| bmap | inode, blocksize, idx | `ReplyBmap{block}` | ENOSYS | |
| ioctl | inode, fh, flags, cmd, arg, in_size, out_size | `ReplyIoctl{result, flags, in_iovs, out_iovs}` | ENOSYS | |
| poll | inode, fh, kh: Option, flags, events, notify: &Notify | `ReplyPoll{revents}` | ENOSYS | |
| notify_reply | inode, offset, data | `()` | ENOSYS | answer to our NOTIFY_RETRIEVE; error arm still sends a header (no-reply handle, :4559, 4607) |
| batch_forget | `&[(inode, nlookup)]` | — | no-op | per-entry nlookup (FUSE-3k, :550–558) |
| fallocate | inode, fh, offset, length, mode | `()` | ENOSYS | PORTED 2026-10-23 (charter §6.6, decision 0003 §BH): the reference reserved nothing (0002 row 554); ours reserves UNWRITTEN blocks, punches, zeroes, shifts at the block grain. The kernel's FUSE client forwards only KEEP_SIZE/PUNCH_HOLE/ZERO_RANGE (`fs/fuse/file.c`) |
| readdirplus | parent, fh, offset: u64, lock_owner | `ReplyDirectoryPlus{entries: Stream<DirectoryEntryPlus{inode, generation, kind, name, offset, attr, entry_ttl, attr_ttl}>}` | ENOSYS | |
| lseek | inode, fh, offset, whence | `ReplyLSeek{offset}` | ENOSYS | |
| copy_file_range | inode, fh_in, off_in, inode_out, fh_out, off_out, length, flags | `ReplyCopyFileRange{copied}` → `fuse_write_out` | ENOSYS | `copied as u32` (reply.rs:534–541) |
| zc_write_hold_eligible | ino, offset, len, odirect | bool | **false** | SYNC, lock-free, on the worker thread (:23–55) |
| read_fast_probe | ino, fh, offset, size, flags, dest: Option<(addr, cap)> | `FastReadProbe::{Served(Bytes), ServedFd{body, fd, off}, Demote}` | **Demote** | SYNC try-only (:57–93; reply.rs:246–272) |

What the daemon MUST implement (vs the ENOSYS default): init, destroy, lookup, forget, batch_forget,
getattr, setattr, readlink, symlink, mknod, mkdir, unlink, rmdir, rename(2), link, open, read, write,
statfs, release, fsync, {set,get,list,remove}xattr, flush (or rely on ENOSYS latch), opendir, readdir,
readdirplus, releasedir, fsyncdir, create, fallocate, lseek, copy_file_range, ioctl (the daemon serves
no data ioctls — `FUSE_IOCTL` payloads on a zc queue are delivered empty + counted,
fuse_over_uring.rs:7165–7182). Optional/never: access (default_permissions), bmap, poll, getlk/setlk
(kernel-local), notify_reply (no NOTIFY_RETRIEVE is ever sent), interrupt.

---

## 7. FORGET / BATCH_FORGET, nlookup, `open_count_stranded`

- Session: FORGET and BATCH_FORGET take NO reply (`no_reply` handles, session.rs:154–167); over-uring the
  worker auto-COMMITs them with an empty reply BEFORE the session consumes the payload (fuse_over_uring.rs:
  7206–7212). INTERRUPT and NOTIFY_REPLY are no-reply too (FUSE-3i, session.rs:4276–4288).
- Daemon bookkeeping (`OpenEntry{count, dirty, lookups}`, fuse_client.rs:8738–8752): `note_lookup(ino)`
  adds ONE reference per entry reply sent — LOOKUP, CREATE, MKNOD, MKDIR, SYMLINK, LINK and each READDIRPLUS
  entry; root (`ino ≤ 1`) is never counted (:9992–10006). `return_lookups(ino, nlookup)` (:10025–10053):
  `n = max(nlookup, 1)` ("the kernel never sends 0; treat as one so a malformed forget can never make an
  inode unevictable"); `lookups −= n` saturating; evict iff `lookups == 0` (an inode the daemon never counted
  evicts on its first FORGET — the pre-3k fallback, cache retention only). **POSIX-14**: reaching zero while
  `count > 0` = a lost RELEASE (the kernel cannot evict an inode a `struct file` holds) ⇒
  `open_count_stranded += 1` + ERROR, and the count is NOT zeroed (an unlinked-open file's data must never
  be destroyed on accounting we know is wrong; the orphan belongs to a dismount sweep) (:10013–10024,
  10035–10041). The entry is removed only when `lookups == 0 && count == 0` (:10043–10051).
- `forget` (:30322–30348): virtual per-lookup-generation inos retire their payload and return;
  otherwise `return_lookups` false ⇒ return; true ⇒ `attr_cache.invalidate`, `forget_side_maps`,
  `queue_reclaim_inode` (nlink==0-while-open reclaim). `batch_forget` (:30357–30383) = exactly N forgets
  (the drop_caches / memory-pressure path; fuse3's default no-op leaked every batch-evicted orphan for the
  mount's life). `fuse_force_forget(1)` returns ONE reference while the kernel keeps the rest (:30335–30339).
- Gauge: `open_count_stranded` (must stay 0; `.stats` fuse_client.rs:11411; `METRICS` :5683). Tests
  §14 (lookup_refcount_tests.rs).

---

## 8. Notify writes (notify.rs)

- `Notify` wraps a no-reply `ReplyTx` cloned from the session's reply channel (session.rs:695–697) — a
  notification always has `ReplySlot::Classical` and rides the classical device write even after arm
  (notify.rs:113–118). Two forms per kind: async `.await` (never actually suspends — one `unbounded_send`,
  pinned notify.rs:483–497) and `*_detached(&self)` synchronous fire-and-forget from ANY thread, no runtime
  (:175–195, :300–319, :331–336). A dead reply task DROPS the frame (fire-and-forget contract).
- Encoders (ONE per kind, shared by both forms — the "no-drift law"): `inval_inode_frame(ino, off, len)`
  (:33–52), `prune_frame(&[ino])` (:62–85) — the only two the daemon ever sends (fuse_client.rs:31368–31386,
  :8820–8829). The others exist but are UNUSED and malformed (Q13): INVAL_ENTRY / DELETE / STORE set
  `oh.len = 16 + sizeof(out)` EXCLUDING the trailing name/data (notify.rs:160–163, 192–196, 225–229) while
  `fuse_dev_do_write` refuses `oh.len != nbytes`, and INVAL_ENTRY/DELETE omit the trailing NUL the kernel
  requires (`size == sizeof(outarg) + namelen + 1`); RETRIEVE and POLL wakeup are well-formed (:128–148,
  :251–281).
- Error semantics (session.rs:1132–1170, read_phase.rs:357–421): ENOENT = the kernel does not hold the inode
  — EXPECTED (272 k / 275 k WARN lines per box pass were 46 % / 21 % of `served_mutation_{invals,prunes}`),
  counted `fuse3_notify_enoent`, never logged; any other errno (ENODEV after a connection abort, EBADF,
  EINVAL from a pre-7.45 kernel on PRUNE) counted `fuse3_notify_failed`, WARN once per errno class (8
  slots), never ends the reply task — "the notify arm takes no part in the session's teardown". ENOTCONN
  arrives only on the ring path and is a disconnect for REQUESTS (session.rs:1280–1292), not a notify
  verdict.
- Daemon use: the served-mutation hook sends `INVAL_INODE(ino, −1, 0)` for attr verbs / `(0, 0)` for data,
  then `PRUNE([ino])` iff `kernel_prune_capable()` = writeback negotiated ∧ kernel minor ≥ 45 (announced
  once when not, fuse_client.rs:31368–31419). PRUNE is the ONE way a `FUSE_WRITEBACK_CACHE` kernel adopts
  a peer mount's size/mtime/ctime (abi.rs:507–514). W1 ring-write invalidation uses
  `invalid_inode_detached`; generic/451 DIO coherence uses a synchronous ranged INVAL_INODE awaited before
  the WRITE reply (fuse_client.rs:8820–8829).
- The port's issue law (decision 0003 §CK): io_uring writes `/dev/fuse` INLINE on the submitter (the device
  polls EPOLLOUT; fuse ignores IOCB_NOWAIT), so a frame the kernel may sleep on — INVAL_ENTRY (the parent's
  `i_rwsem`), a page-invalidating INVAL_INODE (a folio a WRITE or READ holds locked), PRUNE — goes
  `IOSQE_ASYNC` (`SbCell.sleeps`); attrs-only INVAL_INODE, INC_EPOCH and replies stay inline. Otherwise the
  sideband sleeps behind a request whose lane awaits it (the shim's fsstress wedge).

---

## 9. Supervisor + watchdog — exact behaviour and constants

### 9.1 External mount supervisor (`squeezefs mount --daemon --supervise`; supervisor.rs)
- Runs in the PARENT process after the child's `ready` pipe message (main.rs:3048–3099), std-only, no
  runtime. `SupervisorPolicy{probe_interval 5 s, unresponsive_after 30 s, write_abort true}` (supervisor.rs:
  31–49; env `SQUEEZEFS_SUPERVISE_INTERVAL_SECS`/`_UNRESPONSIVE_SECS`, > 0 else default, main.rs:3075–3092).
- Connection id resolved at START (`stat(mountpoint).st_dev` minor via glibc `minor()` — a wedged mount
  wedges stat too) (:124–139, :250–264). Loop: sleep `probe_interval`; `kill(pid, 0)` fails ⇒ exit
  (:268–274); `probe_stats_inode(mnt, timeout = probe_interval)` = `metadata(mnt/.stats)` on a detached
  thread (`sqfs-supervise-probe`) with `recv_timeout`; a timed-out thread is leaked by design (:200–217).
- Pure state machine `SupervisorState::observe` (:69–122): success resets `last_ok`, re-arms, returns
  `Recovered` iff probes had been failing; failure inside the window ⇒ `None` (loop logs "grace window");
  failure with `now − last_ok ≥ 30 s` ⇒ `Escalate{unresponsive_for}` exactly once until a success re-arms.
- Escalate (:293–325): print `dump_daemon_state(pid)` = `/proc/<pid>/status` + per-task `comm`, `wchan`,
  `stack` (root-only, skipped silently) (:165–198); read `/sys/fs/fuse/connections/<id>/waiting` (:147–150);
  iff `write_abort && waiting > 0` write `"1"` to `…/abort` (:158–160) — destructive; the mount is dead
  (ECONNABORTED to blocked callers); prints the manual recovery (kill BY PID, `squeezefs umount`, remount).
  **The reference kills nothing** (:9–15: "Killing/restarting the daemon stays a manual runbook step").
  Charter §4 requires `SIGABRT → SIGKILL` before the abort write — the charter wins (§15 Q10).
- Tests: `tests/supervisor_tests.rs` (§14).

### 9.2 In-daemon deadline watchdog (D1.b; fuse_client.rs:440–496, 1962–2047, 2138–2244)
- Per-op `timeout()` wrappers are RETIRED: a dropped handler future mid-`commit_tx` leaks ring budget /
  stalls `completed_upto` forever — the 30 s per-op timeout WAS a volume-wedge vector (:450–466). Ops no
  longer synthesize ETIMEDOUT; the watchdog LOGS and never cancels (:467–473). The await-disposition audit
  table (:479–495) is the exception list: barrier waits bounded INSIDE the coalescer, ring-admission parks
  escalate to the journal failure latch, everything else watchdog-only.
- Registry: a lock-free slab of 64-byte-aligned `OpSlot{state, kind, ino, start_ns, station}` (:1447–1458)
  claimed per in-flight op; geometry `shards = max(possible_cpus, ceil(256 / 64))`, `slots/shard = 2 ×
  Q_DEPTH_DESIRED = 64` (`op_registry_derived_geometry`, :1418–1422) — ≥ 2× the delivered ring capacity,
  never below the shipped 256 (:1395, :1397–1417; measured 6.65 % of fused-worker cycles on the old 256-slot
  single-cursor slab at 525 k IOPS).
- Tick: `OP_WATCHDOG_TICK = 5 s` (:2140); the tick refreshes `COARSE_NOW_NS` first (ages accurate to ± one
  tick, :1667–1672), then `op_watchdog_tick(threshold)` (:1988–2047): for every claimed slot with
  `age ≥ threshold`: if the appender PARK gate is live and the op's last station precedes PUBLISH and
  `age ≤ parked_for + threshold` ⇒ WARN "PARKED", not counted; else ERROR "FUSE op watchdog: {op} (ino) in
  flight for {age} ms (> {threshold}) parked past station [{station}] — op is NOT cancelled" and
  `fuse_op_watchdog_overdue += 1` (one observation per scan per stuck op). After any overdue op: the lock
  census, the write-phase census and `wedge_census_line()` (atomics only — readable when `.stats` itself is
  wedged; :2142–2205).
- Threshold = `SQUEEZEFS_TIMEOUT` seconds, default 30, memoized (:197–211). Decision 0002 row 17 retired the
  `tuning` class; the Zig watchdog threshold is a documented constant (propose 30 s with the WHY: "the
  historical per-op timeout, now a log threshold") or an A/B lever row.
- Spawned once per process from FUSE `init` on the `sqz-meta` venue (:2215–2244). Transport-side twin:
  `SLOT_WATCHDOG_INTERVAL = 5 s`, `SLOT_OVERDUE_NS = 5 s` → `transport_slots_overdue` (fuse_over_uring.rs:
  335–341) — "deliberately generous: name a WEDGE, not second-guess a slow handler".
- Tests: `fuse_watchdog_teardown_tests.rs` (§14).

---

## 10. Mount options and the TTL classes

### 10.1 `MountOptions` (mount_options.rs:12–69) → kernel strings
- Root path `build(fd)` (:310–352): `fd=N,user_id=U,group_id=G,rootmode=40000[,subtype=S][,allow_root]
  [,allow_other][,default_permissions][,<custom>]`; `uid/gid` default to the process's. `flags()` (:466–495):
  `MS_DIRSYNC|NOATIME|NODEV|NODIRATIME|NOEXEC|NOSUID|RDONLY|SYNCHRONOUS` from the booleans. `fs_name`
  (source column) and `subtype` (type `fuse.<subtype>`) are INDEPENDENT (:86–103; pinned :515–569 —
  xfstests passes the device path as `fsname`).
- fusermount3 path `build_with_unprivileged()` (:355–437): `user_id,group_id,rootmode,fsname=<fs_name|fuse>
  [,subtype][,allow_root][,allow_other][,ro][,default_permissions][,nosuid][,nodev][,noexec][,custom
  minus max_read|max_write|max_pages|max_readahead|max_background|congestion_threshold|async_read]`.
- Daemon-level (never passed to the kernel): `max_background`, `congestion_threshold` (> 0 only; INIT-reply
  overrides, :250–270), `transport_buffer_cap_bytes` (:272–279), `write_back`, `handle_killpriv[_v2]`,
  `dont_mask`, `no_open_support`, `no_open_dir_support`, `force_readdir_plus`, `nonempty`.
- The daemon's posture (`start_mount`, fuse_client.rs:31484–31619): `subtype("squeezefs")`,
  `fs_name("squeezefs")` (or `-o fsname=`), uid/gid iff root, `allow_other`, `write_back(posture.write_back
  && !read_only)`, `read_only`, **`default_permissions(true)`**, `handle_killpriv_v2(!SQUEEZEFS_FUSE_NO_KILLPRIV)`,
  `nosuid/nodev/noexec` from `-o`, `-o max_background=`/`congestion_threshold=` (u16 > 0 else warn),
  `transport_buffer_cap_bytes(resolve_transport_buffer_cap(…))`. `parse_custom_options` strips
  `entry_timeout|attr_timeout|negative_timeout|dir_entry_timeout|direct_device_true|stripe_dirs|
  interception|writeback|writeback_cache` from the kernel string (:30904–30938).
- Unmount (`inner_unmount`, session.rs:490–594): notify destroy; await the mount task; unprivileged:
  `fusermount3 -u` up to 10× at 100 ms, then `-u -z` (lazy); root: `umount(2)` up to 10× then
  `umount2(MNT_DETACH)` — a bystander's transient fd keeps the non-lazy form EBUSY. **Port (0002 row
  499, 0003 §S): the lazy detach is NOT the default.** The orderly stop runs the holder census
  (`cli/holders.zig`, by mount id) BEFORE the drains and REFUSES a held mount (`error.Busy`, the
  holders named, `transport_umount_busy_refusals`); `squeezefs umount --lazy` (SIGUSR2) detaches a
  held mount and the daemon serves it until the kernel ends the connection; the supervisor probes a
  detached or unmounting daemon by its heartbeat page, not by a stat of a path that is gone.

### 10.2 Kernel TTL classes (`KernelCacheTtls{attr, entry, dir_entry, negative}`, fuse_client.rs:338–437)
- Defaults **1 s each** (write mount, :345–353). Read-only mounts derive all four from the reader's
  staleness bound = poll interval + the writer's ≤ 1 s checkpoint ceiling (`read_only_defaults`, :356–382).
- Precedence: derived default → `SQUEEZEFS_FUSE_{ATTR,ENTRY,DIR_ENTRY,NEGATIVE}_TTL_MS` (env_knobs.rs:264–267,
  `int(0, MS_MAX)`, default "1000") → `-o attr_timeout=|entry_timeout=|dir_entry_timeout=|negative_timeout=`
  (libfuse-style float seconds; unparseable/negative/non-finite silently ignored, :415–437 — Q11).
- Use: `entry_ttl_for(kind)` = `dir_entry` for directories (a dir dentry's invalidation covers a subtree),
  `entry` otherwise (:25105–25111); LOOKUP reply `ttl = entry_ttl_for(kind)` (:27205–27209) — which feeds
  BOTH `entry_valid` and `attr_valid` (reply.rs:146–160; Q1); GETATTR/SETATTR `ttl = attr` (:28635);
  READDIRPLUS entries `(entry_ttl_for(kind), attr)` (:29230); negative LOOKUP = `ReplyEntry::negative(negative)`
  iff ENOENT ∧ `negative ≠ 0` (:27221–27231; counter `fuse_lookup_negative_replies`); `negative_timeout=0`
  restores the bare ENOENT.
- Tests: lookup_negative_tests.rs (§14).

---

## 11. Knobs read (name / kind / range / default / purpose / class)

| knob | kind, range, default | purpose | class | reads |
|---|---|---|---|---|
| `SQUEEZEFS_FUSE_MAX_WRITE` | int [4096, BYTES_MAX], derived (= max(block, 1 MiB)) | INIT max_write DESIRE, verbatim, still sysctl-gated | derived_override | fuse_client.rs:27047–27052 |
| `SQUEEZEFS_FUSE_OVER_IO_URING_QUEUES` | int [1, 512], kernel possible CPUs | TESTING only — fewer than possible CPUs never becomes ready | test seam (charter) | fuse_over_uring.rs:2735–2737 |
| `SQUEEZEFS_FUSE_OVER_IO_URING_Q_DEPTH` | int [1, 32], derived (desired 32) | explicit depth wins verbatim, bypasses the budget ladder | derived_override | :2738–2740 |
| `SQUEEZEFS_FUSE_IO_URING_ENTRIES` | int [64, 4096], 1024 | SQ depth of the classical (INIT/sideband/notify) rings | derived_override | uring_conn.rs:31–37 |
| `SQUEEZEFS_FUSE_IO_URING_SQPOLL_IDLE_MS` / `_CPU` | int [0, MS_MAX] off / int [0, 4095] unpinned | SQPOLL the rings (measured NOT recommended) | ab_lever | uring_conn.rs:41–55 |
| `SQUEEZEFS_TRANSPORT_MEM_MAX` / `_PCT` | int [1, 2^30] MiB derived / int [1, 100] derived | payload-arena cap: absolute > pct > budget/8 | derived_override | mem_budget.rs:643–679 |
| `SQUEEZEFS_TRANSPORT_DEBUG` | bool off | per-request transport tracing to stderr | ab_lever/test | fuse_over_uring.rs:41–42 |
| `SQUEEZEFS_FUSE_NO_KILLPRIV` | bool off | TESTING escape: refuse HANDLE_KILLPRIV_V2 | test seam | fuse_client.rs:31520 |
| `SQUEEZEFS_FUSE_{ATTR,ENTRY,DIR_ENTRY,NEGATIVE}_TTL_MS` | int [0, MS_MAX], 1000 | kernel TTL classes | derived_override | fuse_client.rs:398–401 |
| `SQUEEZEFS_TIMEOUT` | int [1, 86400] s, 30 | watchdog overdue threshold (was the per-op timeout) | retired class (0002 row 17) → constant | fuse_client.rs:197–211 |
| `SQUEEZEFS_SUPERVISE_INTERVAL_SECS` / `_UNRESPONSIVE_SECS` | int [1, 86400], 5 / 30 | supervisor cadence / escalation | ab_lever or constants | main.rs:3082–3092 |
| `SQUEEZEFS_DAEMON_PIPE` | int fd, none | parent↔child readiness pipe ("ready\n") | config (internal) | session.rs:1985–1990 |
| `SQUEEZEFS_OP_PROFILE` | bool off | per-op phase histograms + i_rwsem estimator | ab_lever | fuse_client.rs:500–524 |
| `SQUEEZEFS_OP_TRACE` | bool off | arm the per-op trace ring at mount | ab_lever | env_knobs.rs:311 |
| `SQUEEZEFS_STATS_KEY_CENSUS` | bool off | opt-in `.stats` key census | switch | env_knobs.rs:313 |
| `SQUEEZEFS_FUSE_PIN_SCOPE` | enum node\|core, node | transport thread affinity scope | enum_select | env_knobs.rs:237 |
| `SQUEEZEFS_FUSE_SAME_LANE_DISPATCH` | bool 1 | READ handler on the dispatching lane | ab_lever | session.rs:6239 |
| `SQUEEZEFS_FUSE_READ_FAST_DISPATCH`, `_DRAIN_GROUP`, `_KMBUF`, `_ZC`, `_ZC_WRITE_FUSION`, `_ZC_READ_FUSION`, `_ZC_FUSION_MAX`, `_ZC_RETENTION`, `_IO_URING_SPIN_US` | see env_knobs.rs:236–248 | ring-transport levers (ring spec) | ab_lever | — |
| `-o max_background=`, `-o congestion_threshold=` | u16 > 0 | INIT-reply overrides (the A0 lever restoring 256) | ab_lever | fuse_client.rs:31583–31598 |

---

## 12. Gauges / counters / histograms (name — meaning — flags)

Transport geometry levels (§3): `transport_{queues,q_depth,payload_buffer_bytes,max_background,max_write,
max_pages,max_readahead}`.
Reply integrity (fuse_over_uring.rs:2170–2182, 2342–2374): `transport_requests_failed_synthetic` (honest
count of synthesized errors), **`transport_requests_abandoned` must-stay-0** (a delivered request left its
slot with no commit), `transport_replies_refused_stale` (double/late/mis-addressed, never an overwrite),
`transport_replies_dropped_no_slot`, `transport_ents_retired` (after `REGISTER_RETRY_MAX = 8` failures;
backoff 1 ms doubling to 128 ms, :444–450), `transport_slots_overdue` (≥ 5 s owing), **`transport_replies_
oversize` must-stay-0**. CQ (:2270–2295): **`transport_cq_overflows` must-stay-0**, `transport_cq_nodrop`
(IORING_FEAT_NODROP probe). Park (:2183–2217): `transport_park_backstop_ticks` (100 ms tick; growth under
traffic is the backstop working), `transport_park_tick_{commit,cqe}_rescues` ≈ 0 (lost-wake tripwires).
Leases (:2562–2587): `transport_payload_leases` (adoption), `transport_parked_commits`,
`transport_unparked_commits` (closure `parked ≡ unparked` at quiesce), `transport_leases_outstanding` (→ 0
at quiesce), `transport_lease_max_age_ms`, `transport_lease_overlong` (≥ 1 s, loud never fatal),
`transport_dest_dma_leases`. Wake (:2163–2168): `transport_wake_writes`, `transport_wakes_elided`
(writes/(writes+elided) ≈ 1 under saturation = regression). Sideband: `transport_classical_sideband` (> 0
after a storm). Dispatch partition: `transport_fast_dispatch_{serves,demotes}` + `fuse3_zc_read_fusions`
≡ READs. Session: `fuse3_read_inplace_replies` / `fuse3_write_inplace_replies` (engagement ≈ every
READ/WRITE on an armed session — the READ arm sat disengaged for weeks because nothing measured it,
read_phase.rs:318–355), `fuse3_notify_enoent` (expected), `fuse3_notify_failed` (ENODEV after abort),
`fuse3_tpc_lane_redispatches` (≈ 0: a lane thread died, session.rs:6203–6208).
Daemon: `fuse_killpriv_negotiated` (0/1), `fuse_killpriv_clears`, `fuse_lookup_negative_replies`,
`fuse_op_watchdog_overdue` (observations), **`open_count_stranded` must-stay-0**.
Histograms (read_phase.rs; sharded per thread, exact `count`/`sum_ns`, shared `latency_core` buckets,
`shard_count = clamp(next_pow2(available_parallelism), 1, 64)` :172–181): `read_transport_phase_ns` /
`write_transport_phase_ns` phases `queue_wait, dispatch_lag, reply_commit, transport_total, commit_flush`
(:42–73; error replies record nothing); `fuse3_fused_timeline_ns` = `wake_to_poll, bridge_rtt` +
`fuse3_fused_{midpass,passbottom}_reaps` (:424–497); `transport_reap_gap_ns` = `blind, blind_cqe, park`
(`ReapCadence`, :499–637); `zc_bridge_phase_ns` = `msg_hop, sq_wait, device_cq, wake_hop, total`
(containment `total ≈ Σ`, :639–712). Per-op trace stages (op_trace_core.rs:62–82 transport part):
`transport_recv 1, dispatch 2, handler_entry 3, handler_return 4, reply_commit 5, fast_dispatch 6`; device
`dev_submit 32, dev_complete 33`; bridge `bridge_sent 39, bridge_taken 40` (the op id is the FUSE `unique`;
0 is never an id; a full ring DROPS and counts `op_trace_dropped`, op_trace.rs:14–42).

---

## 13. Constants with WHY (not already placed above)

- `FUSE_KERNEL_MINOR_VERSION 36`, `TIME_LIMITS_{MIN,MAX}_SEC ±9_223_372_036` (§1.1, §2.3).
- `FUSE_MIN_READ_BUFFER 8192`; dispatch buffer `max(max_write + 40, 8192)` (session.rs:1375).
- INIT classical buffer 8192 (:1185) — the kernel's minimum read.
- `NOTIFY_FAILED_CLASSES 8` (read_phase.rs:386 — a notification's errnos are a handful).
- `SLOT_WATCHDOG_INTERVAL 5 s`, `SLOT_OVERDUE_NS 5 s`, `PARK_BACKSTOP_TICK 100 ms`, `RESCUE_LOG_INTERVAL
  60 s` (fuse_over_uring.rs:335–347); `REGISTER_RETRY_MAX 8`, backoff 1 → 128 ms (:444–450).
- `OP_WATCHDOG_TICK 5 s`, `OP_REGISTRY_SHIPPED_SLOTS 256`, slots/shard `2 × 32` (§9.2).
- Supervisor 5 s / 30 s (JuiceFS precedent, supervisor.rs:34–37); unmount retry 10 × 100 ms (session.rs:521–523).
- `rootmode 40000` default (mount_options.rs:321); `fsname` default `"fuse"`.
- KILLPRIV_V2 law (`kill_suidgid_mode`, fuse_client.rs:30889–30902): `S_ISUID` always cleared; `S_ISGID`
  cleared only if `S_IXGRP` (sgid-without-group-exec is the mandatory-locking marker — preserved);
  `security.capability` removed if present (:15443–15450); folded into ONE commit on SETATTR (:28510–28590);
  `FUSE_OPEN_KILL_SUIDGID` on O_TRUNC opens (:27561–27573); `FUSE_WRITE_KILL_SUIDGID` on writes (:27795);
  the per-ino `killpriv_clean` latch makes the common case a contains-check (insert-before-read /
  commit-before-remove race law, :15388–15400). Measured motive: without V2 the kernel sends
  `GETXATTR("security.capability")` once per `write(2)` — HALF of every write-syscall-bound stream's requests
  (abi.rs:176–183; session.rs:7202–7212).
- Negative entry = `nodeid 0 + entry_valid` (`fuse_lookup_name`: "Zero nodeid is same as -ENOENT, but with
  valid timeout", reply.rs:99–110).

---

## 14. Tests (name — one sentence — venue)

In-crate (no mount): `init_negotiation_tests` (session.rs:6941–7373): `the_kernel_uring_enable_precedes_the_
init_reply` (source-order rail), `init_reply_never_advertises_{splice,flock_locks,posix_locks,posix_acl}`,
`time_limits_capability_is_folded_bit_62`, `time_limits_populate_when_offered`, `time_limits_absent_without_
offer`, `init_out_abi_64_bytes_time_limits_at_48_56`, `init_reply_echoes_implemented_caps`, `init_reply_
advertises_handle_killpriv_v2_when_offered_and_enabled` (v1 never rides along), `init_reply_never_advertises_
handle_killpriv_v2_when_option_off`, `init_ext_and_minor_36_ride_every_reply_with_nonzero_flags2`,
`init_ext_never_invented_without_kernel_offer`, `init_ext_absent_when_reply_flags2_zero`, `reply_minor_never_
exceeds_kernel_offer_min_semantics` (whole shape space). `kernel_init_folds_flags2_into_high_bits` (:7384).
`the_kernels_readahead_limit_is_echoed_verbatim_and_published` (:7514). `reply_guard_tests` (:6631–6793):
dropping an owing handle synthesizes EIO, a replied handle synthesizes nothing, `reply_error_in_place` is the
one reply, a no-reply handle stays silent, a dead reply task is reported not ignored. `dispatch_bounds_tests`
(:6796–6858): short NOTIFY_REPLY / BATCH_FORGET bodies rejected not split. `delivery_bounds_tests`
(:6861–6940): short header never underflows, overdeclared body refused not clamped over stale bytes, the
WRITE lease body rides the payload, buffer capacity is the last bound. `tpc_dispatch_tests::dead_lane_
redispatches_loudly_healthy_lanes_never` (:7432). abi.rs: `fuse_open_in_carries_open_flags_at_offset_4`,
`killpriv_v2_uapi_bits_are_pinned` (:1184–1217). lib.rs: `setattr_in_fattr_kill_suidgid_surfaces_on_set_attr`
(:400). reply.rs: `negative_entry_encodes_nodeid_zero_with_entry_ttl`, `positive_entry_keeps_nodeid`
(:554–579). notify.rs: `invalid_inode_detached_enqueues_synchronously_without_a_runtime`, `detached_and_
async_forms_enqueue_identical_frames`, `prune_frame_is_one_header_struct_and_nodeid_array`, `a_notifications_
enoent_is_counted_never_the_interrupted_request_warn`, `a_notifications_other_errno_is_counted_per_frame_and_
announced_once_per_class` (:461–648). mount_options.rs (:515–569): subtype independent of fsname (root +
fusermount), subtype without fsname keeps `fuse`, no subtype emits none. read_phase.rs (:714–906):
`families_are_phase_exact_and_independent`, `shard_fold_is_exact_to_the_ns`, `reap_cadence_records_blind_
park_and_per_cqe_weight_exactly`, `reap_gap_snapshot_names_the_three_phases`, `zc_bridge_family_is_phase_
exact_and_folds_across_threads`. Geometry (fuse_over_uring.rs:9614–9779): `test_plan_raised_sysctl_register_
bound_holds`, `test_plan_default_sysctl_shape_is_byte_identical`, `test_plan_4mib_max_write_sysctl_gated`,
`test_plan_budget_ladder_degrades_payload_before_floor_violation`, `test_plan_small_max_write_stops_inflating_
to_256_pages`, `test_plan_env_depth_bypasses_payload_ladder`, `test_plan_default_ample_budget_is_measured_
class`; `an_oversize_reply_becomes_eio_not_a_truncated_body` (:8746), `fail_ent_synthesizes_for_an_owing_slot`
/ `_parked_slot` (:9016, 9042).

Root-suite, in-process filesystem (no mount): `killpriv_v2_tests.rs` — `flagged_write_clears_suid`,
`flagged_write_preserves_sgid_without_group_exec`, `flagged_write_clears_sgid_with_group_exec`, `flagged_write_
drops_security_capability_xattr`, `unflagged_write_clears_nothing`, `otrunc_open_kill_flag_clears_and_absence_
preserves`, `setattr_kill_suidgid_clears_folded_into_one_commit`, `no_priv_bits_flagged_write_is_zero_metadata_
traffic`, `clean_latch_rearms_on_mode_and_caps_mutation`. `lookup_refcount_tests.rs` — `entry_replies_count_
and_a_partial_forget_does_not_evict`, `batch_forget_carries_per_entry_nlookup`, `an_uncounted_inode_still_
evicts_on_its_first_forget`, `a_lost_release_leaves_a_stranded_open_count_that_is_detected`. `forget_sweep_
tests.rs` — `forget_sweeps_dir_gen_and_killpriv_clean`, `batch_forget_sweeps_exactly_like_n_forgets`.
`lookup_negative_tests.rs` — `lookup_miss_replies_cacheable_negative_entry`, `create_after_negative_lookup_sees_
the_file`, `unlink_then_lookup_is_negative_again`, `negative_ttl_zero_disables_negative_replies`, `non_enoent_
lookup_errors_stay_errors`, `ttl_knobs_reach_reply_surfaces`, `ttl_mount_option_parsing`, `ttl_env_knobs_seed_
construction`, `ttl_options_stay_out_of_kernel_mount_string`. `supervisor_tests.rs` — `state_machine_escalates_
once_after_threshold`, `state_machine_recovery_rearms`, `abort_write_plumbing` (tempdir sysfs), `waiting_count_
parsing`, `connection_id_is_dev_minor`, `probe_stats_inode_smoke`, `dump_daemon_state_smoke`. `fuse_watchdog_
teardown_tests.rs` — `watchdog_tick_reports_overdue_with_op_detail_and_clears_on_exit`, `slow_commit_completes_
and_watchdog_logs_instead_of_timeout_drop`, `admission_park_past_threshold_escalates_to_disabled_volume`,
`cfr_registers_with_watchdog_while_guard_blocked`, `fallocate_registers_with_watchdog_while_guard_blocked`,
plus bounded-barrier and readdir-coherence rows. `attr_only_inval_tests.rs` — `fallocate_setid_strip_pushes_
attrs_only_inval`, `killpriv_clear_pushes_attrs_only_inval`, `unmoved_mode_pushes_no_invalidation`.
`served_mutation_kernel_tests.rs` — data mutation invalidates whole inode then prunes; attrs mutation invalidates
attrs-only then prunes; without prune capability only the invalidation travels; the hook delivers from a plain
OS thread; the hook skips pre-mount fires.

Need a REAL mount (root/`fuse.enable_uring`, skip-ledgered via `mount_supported`): `transport_geometry_tests.rs`
— `test_default_mount_gauges_negotiated_write_geometry`, `test_env_max_write_override_wins_verbatim`, `test_small_
block_size_keeps_the_1mib_floor`, `test_raised_sysctl_mounts_with_4mib_ents` (the mirror `expected_max_write =
clamp(desire, max(page, 4096), limit × page)`, :51–55). `multi_queue_tests.rs` — `test_single_queue_qdepth4_small_
file_storm_no_starvation` (parked ≡ unparked, leases → 0, max age ≪ 1 s, clean unmount under FORGET storms),
`test_classical_sideband_serviced_and_unmount_clean`. `transport_lease_overlong_tests.rs` — `overlong_write_lease_
is_a_tripwire_not_a_lost_reply` (stalled mount, counter moves, reply still lands).

---

## 15. Reference QUIRKS (decision 0002 candidates) with the proposed better design

| # | Quirk (cite) | Why it is poor | Proposed Zig design |
|---|---|---|---|
| Q1 | `ReplyEntry{ttl}` feeds BOTH `entry_valid` and `attr_valid` (reply.rs:146–160), so a LOOKUP reply's attr TTL is the ENTRY class (dir_entry/entry), not `attr` (fuse_client.rs:27205–27209); `DirectoryEntryPlus` already carries both (reply.rs:487–505) | Four TTL classes exist but LOOKUP/CREATE can only express one | `EntryReply{entry_ttl, attr_ttl, attr, generation}`; ONE `fuse_entry_out` encoder shared by LOOKUP, CREATE, READDIRPLUS, negative entries |
| Q2 | `ReplyCreated → fuse_entry_out` writes `subsec_micros()` into the `*_valid_nsec` fields (reply.rs:433–434) while every other encoder uses `subsec_nanos()` | A unit bug on the wire (1.5 s → nsec 500_000 = 0.5 ms) hidden by having two encoders | Q1's single encoder; a tie test that CREATE and LOOKUP encode the same TTL byte-identically |
| Q3 | `bincode` + serde as the ABI codec with `allow_trailing_bytes` (helper.rs:56–61); `fuse_init_in` decoded as a 16-byte prefix + a manual `data[16..20]` read (session.rs:1744, 1780–1784); module-level `#[allow(dead_code)]` for the whole ABI (abi.rs:20–27) | A serialization-library artifact standing in for the wire (0002 row 2's class); not total; dead-code suppression | `extern struct` per uapi struct, LE fields, `comptime` size asserts (§1 sizes); decode = exact-length slice cast; `fuse_init_in` is one 64-byte struct whose `flags2` is read iff `INIT_EXT` and `len ≥ 20`; unused ABI rows are allowed to exist ONLY in the codec table that the static rail reads |
| Q4 | Uring deliveries are RE-SERIALIZED into a classical contiguous body (`op_in ‖ payload` by `in_header.len`) with a `filled == 0` heuristic ("payload first, then a NUL-terminated op_in prefix", uring_conn.rs:933–968) | A guess about where the kernel put `in_args[0]`; a second copy of every name/xattr body; a heuristic on a hot path | Parse per opcode from the `(in_header, op_in[128], payload)` triple: `in_args[0]` is in `op_in`, the rest in `payload` (dev_uring.c); the op table says for each opcode which arg is where; no reconstruction, no copy except what the handler keeps |
| Q5 | One `Session` CLONE per queue, each with its own unbounded reply channel + reply task + dispatch loop (session.rs:969–986, 912–951); replies hop channel → reply task → connection unless the "in-place" arm fires | N dispatch loops + N reply tasks for one transport; the in-place arm had to be bolted on and gauged to stay wired (read_phase.rs:326–333) | Charter §4: the worker dispatches at the delivery CQE straight to the lane homed on the queue's CPU; every reply goes through `submit_reply*` on the slot; ONE sideband reader; no reply task |
| Q6 | INIT decode failure replies `fuse_out_header{error: libc::EINVAL}` — POSITIVE (session.rs:1751–1755); every other site negates | The kernel refuses `oh.error > 0` (`fuse_dev_do_write`), so the "reply" is itself rejected | ONE `errorHeader(unique, errno)` that always negates through `core/errors.zig`; no hand-built headers |
| Q7 | INTERRUPT is never answered (FUSE-3i, session.rs:4276–4288); the hook result is only logged | NOT a divergence — recorded so nobody "fixes" it: `fuse_dev_do_write` latches `fc->no_interrupt = 1` on a header-only `-ENOSYS` whose `unique` carries `FUSE_INT_REQ_BIT`, BUT only after `request_find` locates the interrupted request in the WRITER's `fpq`; a ring-delivered request lives in the ring queue's own `fpq`, so a classical reply answers `-ENOENT` and the latch is unreachable on an armed session — exactly the reference's observation. Interrupt traffic therefore persists for the mount's life | Keep no-reply; since per-op cancellation is banned (charter §4) the hook is a counter (`fuse_interrupts`) only; re-evaluate iff a kernel routes INTERRUPT over the ring (`fuse_io_uring_ops.send_interrupt`) |
| Q8 | `KERNEL_INIT` / `NEGOTIATED_REPLY_FLAGS` are process-global `OnceLock`s, "first session wins" (session.rs:354–378) | Hidden global state the filesystem reads back through free functions | The negotiated `Init` record is owned by the session and PASSED to `Filesystem.init(io, req, kernel_init)`; the daemon's gauges read the session's record |
| Q9 | `ReplyTx` Drop-guard synthesizing `-EIO` as the exactly-one-reply law (session.rs:256–294) + `catch_unwind` lanes | Zig has no unwinding or destructors on task death (0002 row 9) | The slot state machine owns the obligation: a handler returns `Error!Reply`; the dispatcher ALWAYS commits (reply or `fail_ent` with the table's errno); a true bug aborts loud |
| Q10 | Supervisor dumps state and writes `abort` but **kills nothing** (supervisor.rs:9–15, 293–325) | The charter §4 ladder is stat → dump → `SIGABRT` → `SIGKILL` → abort iff `waiting > 0`; a wedged daemon left alive keeps the fd open and the mount un-remountable | Implement the charter's rungs with kill-BY-PID (the pid is the fork's own); keep the pure state machine + tempdir-rooted sysfs helpers |
| Q11 | `KernelCacheTtls::with_mount_options` parses `f64` seconds and SILENTLY ignores junk/negative values (fuse_client.rs:415–437); `-o max_background=` junk only warns (:31583–31598) | A second lenient parser (0002 row 14); silent ignore violates §1.6 "refuse, never clamp/ignore" | `-o *_timeout=<int seconds>` and `-o max_background=<u16>` go through the ONE parser; a fraction or junk refuses the mount naming the option |
| Q12 | `SQUEEZEFS_TIMEOUT` as the watchdog threshold (a `tuning` row, fuse_client.rs:197–211) | Already retired as a class (0002 row 17) | `watchdog.OVERDUE_NS = 30 s` as a documented constant ("the historical per-op timeout, now a log threshold; 6 ticks of the 5 s cadence") or an explicit A/B row whose doc names the arm |
| Q13 | INVAL_ENTRY / DELETE / STORE frames set `oh.len` to header + struct only, EXCLUDING the name/data body (notify.rs:160–163, 192–196, 225–229), and INVAL_ENTRY/DELETE omit the trailing NUL ("TODO should I add null", :182, 215); `fuse_notify_inval_entry_out`'s last word is named `_padding` (abi.rs:1076–1080). Never exercised: the daemon sends only INVAL_INODE and PRUNE | `fuse_dev_do_write` refuses `oh.len != nbytes`, and `fuse_notify_inval_entry` requires `size == sizeof(outarg) + namelen + 1` — every such frame is EINVAL on the wire; uapi ≥ 7.38 names the word `flags` (`FUSE_EXPIRE_ONLY`) | ONE frame builder `notifyFrame(code, out_struct, tail)` whose `len` is computed from the bytes it emits; names carry the NUL; the field is `flags`; a tie test per code against the uapi length law (the PRUNE test at notify.rs:507–532 is the template) |
| Q14 | `fuse_in_header` tail `_padding u32` (abi.rs:1027), `fuse_open_out._padding` (:728), `fuse_create_in._padding` (:718), `fuse_attr._padding` (:297) | uapi names them `total_extlen/padding`, `backing_id`, `open_flags`, `flags`; a renamed field is a silently-ignored capability | Name every field by its uapi name; assert the unnegotiated ones are 0 on decode (`total_extlen == 0` — no extensions negotiated) |
| Q15 | Stale docs: `max_background` "clamp(queues × depth, 64, 256)" (mount_options.rs:52–55, 250–253; fuse_client.rs:31581–31582) vs the shipped `u16::MAX` ceiling (fuse_over_uring.rs:2874–2879) | Two statements of one derivation | The derivation lives in ONE pure fn with its tie test; docs cite it |
| Q16 | Transport thread/task names `fuse3-mount`, `fuse3-unmount`, `sqfs-supervise-probe` (session.rs:420, 458; supervisor.rs:209) | 0002 rows 20–21 | `sqz-fuse-mount`, `sqz-fuse-umount`, `sqz-supervise` through `thread_name.nameCurrentThread` |
| Q17 | `transport_*` statics are process-global plain `AtomicU64`s RMW'd per op from every worker (fuse_over_uring.rs:2160–2182) with a later `ShardedCounter` retrofit for some (read_phase.rs:217–265) | Shared-line RMW on hot paths (charter §10) | `counters.ShardedCounter` for every per-op count from day one; `Gauge` for levels; `Tripwire` for the must-stay-0 set |
| Q18 | `handle_getxattr` sizes its `Size` reply buffer with `FUSE_STATFS_OUT_SIZE` (session.rs:3450) | Copy-paste constant (harmless, 80 ≥ 8) | One `replyWithOut(T)` that sizes from `@sizeOf(T)` |
| Q19 | Opcode enum stops at 47 and omits NOTIFY codes 7/8 (abi.rs:338–398, 488–515) | A 7.45 kernel's `FUSE_SYNCFS`/`STATX`/`TMPFILE` log as "unknown opcode N" | Know names through uapi 7.45; the reply (`-ENOSYS`) is unchanged |

---

## 16. What the Zig rung-3 modules must contain (the port checklist this spec drives)

- `fuse/abi.zig`: every struct of §1 as `extern struct` with `comptime` size asserts; opcode/notify/flag
  enums with uapi names and the version each was introduced; the uring structs of §1.9.
- `fuse/init.zig`: the §2 procedure as a pure `negotiate(kernel_init_in, mount_opts, geometry) → fuse_init_out`
  plus the ordered side effects (enable_uring → classical write → REGISTER); tie tests restating §2.4–2.5.
- `fuse/geometry.zig`: §3's `plan` over `derive.Facts` + budget; every tie row of §3 as a test.
- `fuse/codec.zig`: §5 — per-opcode `(op_in, payload)` placement table (Q4), `validatedBody`, reply
  encoders per op, the oversize rule, `errorHeader`; fuzz "never panic on arbitrary bytes".
- `fuse/filesystem.zig`: §6 as a comptime-checked vtable/interface with the ENOSYS/Ok defaults.
- `fuse/sideband.zig`: §4 — the permanent classical reader (Readv ring, `SQUEEZEFS_FUSE_IO_URING_ENTRIES`),
  notify writes (§8 encoders with Q13 fixed), verdict table for write errors.
- `fuse/forget.zig` (daemon side): §7 `OpenEntry{count, lookups}`, `noteLookup`, `returnLookups`,
  `open_count_stranded` tripwire.
- `cli/supervisor.zig`: §9.1 with the charter's SIGABRT→SIGKILL rungs (Q10); `fuse/watchdog.zig`: §9.2
  with a sharded slot registry sized `max(cpus, 4) × 64`.
- `fuse/mount_options.zig`: §10 strings + the TTL classes through the ONE parser (Q11).
