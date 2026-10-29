# SqueezeFS quickstart

Everything below runs as root on Linux ≥ 7.3 with FUSE-over-io_uring buffer pools in the
kernel (`CONFIG_FUSE_IO_URING`). Build the binary once:

```sh
zig build -Dprofile=dist dist          # → zig-out/bin/squeezefs
zig-out/bin/squeezefs --version
```

The examples write `squeezefs`; put `zig-out/bin` on your `PATH` or spell the path out.

## 1. Try it in five minutes, on image files

Image files are fine for a first look. Any number you measure on them is the host filesystem's,
so use real devices (section 2) before you benchmark.

```sh
D=/var/tmp/sqz; mkdir -p $D/mnt
truncate -s 2G  $D/meta0 $D/meta1          # two metadata volumes
truncate -s 16G $D/data0 $D/data1          # two data volumes (sparse)

# Format the set: every metadata volume in ONE sqmeta:// URI, one sqdata:// per data volume.
squeezefs format sqmeta://$D/meta0,$D/meta1 sqdata://$D/data0 sqdata://$D/data1

# Mount it supervised, as a daemon, logging to a file.
squeezefs mount --daemon --log-file $D/daemon.log --supervise sqmeta://$D/meta0,$D/meta1 $D/mnt

# Use it like any filesystem.
mkdir -p $D/mnt/projects/alpha
cp /etc/hostname $D/mnt/projects/alpha/
dd if=/dev/urandom of=$D/mnt/projects/alpha/big bs=1M count=64

# Look at it.
squeezefs df $D/mnt                        # capacity per data volume, heap per metadata volume
squeezefs clients $D/mnt                   # who holds a writer registration on each member
squeezefs config show sqmeta://$D/meta0,$D/meta1

# Unmount and check.
squeezefs umount $D/mnt
squeezefs fsck --offline sqmeta://$D/meta0,$D/meta1      # "findings: 0 (clean)"
```

What the `format` line tells you: the format version, the member count, the node size, the journal
size (derived from the volume size), the block size (4 MiB) and `dir spread depth 3`.

`--supervise` matters. A supervised mount keeps a **write cache** in RAM (a kernel-held memory file
the supervisor hands to the next daemon after a crash) and a journal tail the supervisor replays, so
a `kill -9` of the daemon loses nothing that was acknowledged. A mount without a supervisor runs
without the write cache: small writes go straight to the data volumes.

## 2. On real devices

Metadata and data volumes are block devices: local NVMe namespaces, partitions, or NVMe-oF
namespaces. `squeezefs storage list` shows what the host sees; `squeezefs storage validate <path>`
says whether a path is usable. `format` refuses a device that carries another filesystem unless you
pass `--force`.

```sh
squeezefs format sqmeta:///dev/nvme0n1,/dev/nvme1n1 sqdata:///dev/nvme2n1 sqdata:///dev/nvme3n1
squeezefs mount --daemon --log-file /var/log/squeezefs.log --supervise \
    sqmeta:///dev/nvme0n1,/dev/nvme1n1 /mnt/sqz
```

Sizing: a metadata volume of a few GiB holds millions of files; its journal is derived from its size
(`clamp(volume/64, 8 MiB, 32 MiB)`). Data volumes can be any size and any count; capacity is added
later with `squeezefs volume add-data`. Two metadata volumes double the metadata rate for a tree that
spreads over them (section 4); one is plenty for a small set.

Format-time choices, all optional:

| Flag | Meaning |
|---|---|
| `--meta-node-kib 64\|128\|256\|512\|1024` | metadata tree node size (default derived) |
| `--block-size BYTES` | the data block, default 4 MiB |
| `--dir-spread-depth N` | how many directory levels round-robin over the metadata volumes, default 3 (section 4) |
| `--compress none\|lz4\|zstd` | transparent compression of data blocks |
| `--encrypt aes-256-gcm\|chacha20-poly1305 --encrypt-key PATH\|-` | encryption at rest; the key is a file of ≥ 32 bytes, mode 0600 |
| `--single-writer` | one writer at a time; skips the multi-writer plane |

## 3. Mount options and the two caches

```sh
squeezefs mount [-o opt,opt…] [--daemon --log-file PATH [--supervise]] [--encrypt-key PATH|-] sqmeta://… <mountpoint>
```

`-o` takes the usual words — `ro`, `noatime`, `nodiratime`, `relatime` — plus `interception`, which
arms the client bypass (section 6). Without `--daemon` the daemon runs in the foreground, which is
handy for a first look at its log.

The daemon keeps two caches, each sized as a percentage of system RAM and each defaulting to 25 %:

- `SQUEEZEFS_WRITE_CACHE_PCT` — the write cache (supervised mounts only). Small writes land here and
  are promoted to the data volumes at `fsync`, when the cache fills, or on the flush cadence.
- `SQUEEZEFS_READ_CACHE_PCT` — the read cache of device blocks, kept coherent by a per-file epoch.

Set either to `0` to turn it off. Both are environment variables of the `mount` command, like every
other `SQUEEZEFS_*` setting; a misspelt or out-of-range one refuses the mount by name.

## 4. Spreading a tree over several metadata volumes

A file always lives on the metadata volume of its directory. A directory at depth 1–3 (counting from
the filesystem root) is placed round-robin over the healthy metadata volumes; a deeper directory
follows its parent. So `/projects`, `/projects/alpha` and `/projects/alpha/src` spread, and everything
below `/projects/alpha/src` stays with it. With two metadata volumes a tree of per-user or per-job
directories uses both; a single process extracting an archive stays on one.

The depth is a property of the set, so every mount follows the same rule:

```sh
squeezefs format --dir-spread-depth 10 sqmeta://… sqdata://…          # at format
squeezefs config set-dir-spread-depth 10 sqmeta://…                    # later, with the set unmounted
squeezefs config show sqmeta://…                                       # "dir spread depth 10"
```

A change applies to directories created from the next mount on; existing directories keep their
homes. `0` keeps every directory below the root's children on its parent's volume. If you run Lustre,
the depth is `lfs setdirstripe -D -i -1 --max-inherit-rr=N`.

## 5. Several nodes on one set

Every node that can see the same devices can mount the same set, and every read-write mount is an
equal writer. Over a network the devices are NVMe-oF namespaces:

```sh
# On the host that owns the devices: a kernel NVMe-oF target, one share per device.
squeezefs nvmeof target setup
squeezefs nvmeof share --transport tcp --ip 10.0.0.1 --port 4420 /dev/nvme0n1
#   → "shared <subnqn>" and the exact `squeezefs nvmeof connect …` line for the clients
squeezefs nvmeof list

# On every client: connect, then mount the set by the device paths that appear.
squeezefs nvmeof connect --ip 10.0.0.1 --port 4420 <subnqn>
squeezefs storage list
squeezefs mount --daemon --log-file /var/log/squeezefs.log --supervise sqmeta://… /mnt/sqz
```

The second and later mounts find the set's manager through the set itself and join it; `--join
host:port` is only needed when a manager moved its listener before that record landed.
`squeezefs clients` lists every writer. Each host presents an NVMe host identity (`/etc/nvme/hostnqn`
and `hostid`, or `SQUEEZEFS_HOSTNQN`/`SQUEEZEFS_HOSTID`); the data volumes are fenced with NVMe
persistent reservations keyed on it, so a node that loses its lease loses its access to the devices
too. `squeezefs nvmeof resv-report <dev>` shows the reservations.

## 6. The bypass: skipping the kernel round trip

For applications on the same host as the daemon, the `LD_PRELOAD` interposer moves `read`, `write`
and friends over a shared-memory ring to the daemon; metadata operations keep going through the
kernel mount.

```sh
zig build shim                                  # → zig-out/lib/libsqueezefs_il.so
squeezefs mount -o interception --daemon --log-file … --supervise sqmeta://… /mnt/sqz
LD_PRELOAD=$PWD/zig-out/lib/libsqueezefs_il.so your-application /mnt/sqz/…
```

The shim finds the mount by itself and pairs only with a daemon of the same build. FIEMAP (`filefrag`,
`xfs_io fiemap`) is answered for preloaded programs. `-o
interception=direct` additionally lets a trusted client (one that may open the data devices) read
striped files from the devices directly. The C SDK (`zig build sdk`, `include/squeezefs_sdk.h`) offers
the same path to programs you can link.

## 7. Looking after a set

| | |
|---|---|
| `squeezefs status <mnt>` | the mount's state (on a multi-member set it reports one member) |
| `squeezefs df <mnt>` | capacity and allocation per volume |
| `squeezefs clients <mnt>` | writer registrations: live, stale, dead |
| `cat <mnt>/.stats` | every counter, gauge and histogram of the daemon, as JSON |
| `squeezefs fsck --online <mnt>` | a consistency check while mounted; `--offline` on the URIs when not; `--scrub` reads every block; `--repair --apply` fixes what it can |
| `squeezefs scrub …` | data checksums only |
| `squeezefs defrag --data\|--pack\|--meta <mnt>` | background compaction; `--report-only` first |
| `squeezefs job list <mnt>` | long-running jobs (rebalance, drains); `pause`, `resume`, `cancel`, `throttle` |
| `squeezefs volume add-data <mnt> <device>` | more capacity; `remove-data` drains a volume first |
| `squeezefs volume add-meta --take-slots K sqmeta://… <device>` | another metadata volume |
| `squeezefs umount [--lazy] <mnt>` | unmount; `--lazy` detaches a busy mount and lets the daemon finish |

The daemon's log (`--log-file`) is the first place to look when something refuses. Refusals name their
cause: an unknown format bit, a knob out of range, a device another filesystem owns, a kernel feature
the probe did not find.

## 8. Running the tests

```sh
zig build test                 # unit tests; the Linux-only ones need Linux
zig build gate                 # the per-commit gate: fmt, units, static rails, the smoke rows (root)
scripts/venue zig build gate   # the same, on the configured Linux venue
```

`scripts/dev/provision-local-venue` checks and (with `--apply`) installs what a Linux workstation
needs to run the gate; `AGENTS.md` describes the venues, the gate and how changes land.
