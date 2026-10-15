# Rung 10 — the first x86_64 venue: kamrui brought up green (2026-10-14)

`SQZ_VENUE=kamrui` (aafa1c5) runs the gate over ssh on `kamrui-dev`: Ryzen 5 5500U (12 threads),
61 GiB, local NVMe (512 B LBA), Ubuntu 26.04, the STOCK kernel 7.0.0-38-generic (none of the sqz
patches: no kmbuf rings, no zc retention, no FUSE_HAS_SYNCFS, no FUSE_TIME_LIMITS). Until today
nothing had run on x86_64. The first gate (`/tmp/kamrui-gate1.log`): 34/53 steps, 21 tests failed,
104 skipped, 530 s.

**What changed.** `scripts/venue up` on an ssh venue now provisions the box
(`scripts/dev/provision-ssh-venue`, idempotent): `nvmet nvmet-tcp nvme-loop nvme-tcp nvme-fabrics
tls` loaded and persisted in `/etc/modules-load.d/sqz.conf`, IOR/mdtest at io500's hash in
`/opt/sqz-tools`, the root-MPI env in `/etc/profile.d`. OpenSSL is 3.5.5. Each failure was
attributed before it was touched (table); the stock-kernel rows now probe and skip, ledgered as
`no_kernel_feature` with the release in the detail; the rest were fixed red-first, the gate's own
red row standing as the repro.

**The rule for kernel-feature skips.** A row runs the transport's own probe: the kmbuf ladder for
BufRing, zero-copy and retention (both ride kmbuf), `RELEASE_PAYLOAD` for retention, the INIT offer
for TIME_LIMITS. Mixed rows judge their UserEnts half on every kernel and skip only the kmbuf half.
The every-verdict lattice row is gated on kmbuf alone: on a kernel that has it, every other verdict
must still read present, so a regressed probe on the venue kernel fails that row instead of
quietly skipping the rows behind it. Nothing a row proves on the VM changed. With kamrui on the sqz
kernel no venue lacks kmbuf, so the stock stratum is judged everywhere through a seam
(`SQUEEZEFS_TEST_KMBUF_ABSENT`: the ladder reads absent — `auto` mounts UserEnts, the write-window
ledger term counts, explicit `bufring`/`zc` are refused loud).

**Two findings beyond the failures.** (1) Zig's x86_64 Debug default is the self-hosted backend,
not LLVM: the venues compile the test binaries with different backends. Its one measured cliff (a
`@Vector` compare-and-reduce lowered lane by lane, 3.3 ms per MiB) is fixed; the product binary is
ReleaseSafe (LLVM) everywhere. (2) The data-plane admission ledger had never been closed on a non-zc
session: the VM always mounts ZeroCopy, so the `.stats` closure's `admission_ledger_closed` was false
on every stock-kernel mount.

| failure (first gate) | class | cause | resolution |
|---|---|---|---|
| `sdk-c-smoke`, `sdk-c-smoke-shared` compile | 26.04 userland | clang's driver probes /usr/lib/gcc; gcc-16's libgcc dir beside gcc-15's libstdc++ ⇒ `-Wgcc-install-dir-libstdcxx` under `-Werror` | `--gcc-toolchain=include` (zig supplies libc; no host GCC is an input); `-Werror` kept |
| `cli.fusermount` handshake | 26.04 userland | AppArmor's `fusermount3` profile admits only $HOME, /mnt, /media, /tmp, /run/user mountpoints | the row mounts under /tmp; HelperFailed's log names the confinement |
| `data.staging` ring under a tmp dir | venue (tree on ext4) | first run of the hole assertion: the first session's write faults read a 128 KiB readahead window in (30 of 64 grains resident before the scan) | the row drops the file's pages and faults MADV_RANDOM, so mincore counts the scan alone |
| `exec.lane` pluggable park (6.4 s) | x86_64 | the Debug canary scan, 3.3 ms per fiber recycle under the self-hosted backend | word-wise scan (0.25 ms/MiB x86_64, 0.2 ms/MiB aarch64); row judged by 0 tick rescues: 587 ms |
| 11 `fuse.transport.session` e2e rows | stock kernel | kmbuf / zc / retention / TIME_LIMITS absent | ledgered `no_kernel_feature` skips by the transport's probes (above) |
| smoke data plane WITH a cache path | stock kernel | the zc legs (held-slot store, direct leg, fd-source serve) cannot arm on UserEnts | the row judges the stratum the kmbuf ladder reports: legs ON with kmbuf, `off` and zero engagement without |
| smoke data plane WITHOUT a cache path; block-map tree | stock kernel ⇒ **product defect** | the admission ledger summed only zc WRITE terms | `transport_write_window_bytes` (UserEnts/BufRing payload), in the stats closure and the smoke ledger |
| `ops.nvmet.initiator` host-scoped parameter | stock kernel ⇒ **product defect** | `readHostScoped` returned `absent` before publishing: the gauge read `off` | published on every path |
| `wire.tls_e2e_linux` ×2 vs `s_server` | 26.04 userland | OpenSSL 3.5 prints `CN=node-b` / `ed25519` (3.0: `CN = node-b` / `Ed25519`) | the oracle accepts both spellings |
| 13 `nvmet-smoke` rows + `ops.zcrx.association` vs real nvmet-tcp (ran once the modules loaded) | stock kernel config ⇒ **product defect** | `CONFIG_NVME_MULTIPATH=y`, `nvme_core.multipath=Y`: the controller holds only the hidden path node; the namespace is the subsystem's head (spec §3.4 step (b), never ported) — every connect waited 60 s and failed `NamespaceTimeout`, leaving its controller reconnecting | `controllerNamespace` takes the head when every serving controller is ours (the merged shape still never handed out); a failed connect deletes its controller |
| `ipc.host.e2e_linux` interception host | x86_64 speed (timing dependency) | the read-backs classify a stream that vetoes ranged windows file-wide for `STALE_NS` (2 s); on kamrui the cold read came inside it (4 MiB whole-block fill, served hot at the "second touch") | the row waits out the staleness before the cold ladder |
| nvmet rows skipped ("target not ready") | venue setup | nvmet/nvme-loop/-tcp not loaded on a stock kernel | provisioned and persisted |
| wedge census `fuse_ops_inflight=1` (a passing row's alert) | not kamrui-specific | daemon A's `lookup` of the shared hot directory (ino 3, unique 9640) parked at `meta_backend_start` after B's kill until A recovered B's region (10.6 s); resolved before its 10 s report | attributed, not changed: a lookup that keys into a dead holder's region waits for the manager's recovery (8–13 s on the VM too), past the watchdog's 5 s; did not recur in an isolated run |
| `mtls matrix` mount "device I/O failed" lines | not a failure | daemons C/D's expected refusals (the row passes) | none |

**Numbers.** <filled after the green gate>
