use std::ffi::OsString;
#[cfg(target_os = "linux")]
use std::os::unix::io::RawFd;

#[cfg(target_os = "freebsd")]
use nix::mount::Nmount;
#[cfg(target_os = "linux")]
use nix::unistd;

/// mount options.
#[derive(Debug, Clone, Default, Eq, PartialEq)]
pub struct MountOptions {
    // Options implemented within fuse3
    pub(crate) nonempty: bool,

    // mount options
    pub(crate) allow_other: bool,
    pub(crate) allow_root: bool,
    pub(crate) custom_options: Option<OsString>,
    #[cfg(target_os = "linux")]
    pub(crate) dirsync: bool,
    pub(crate) default_permissions: bool,
    pub(crate) fs_name: Option<String>,
    pub(crate) subtype: Option<String>,
    pub(crate) gid: Option<u32>,
    #[cfg(target_os = "freebsd")]
    pub(crate) intr: bool,
    #[cfg(target_os = "linux")]
    pub(crate) nodiratime: bool,
    pub(crate) noatime: bool,
    #[cfg(target_os = "linux")]
    pub(crate) nodev: bool,
    pub(crate) noexec: bool,
    pub(crate) nosuid: bool,
    pub(crate) read_only: bool,
    #[cfg(target_os = "freebsd")]
    pub(crate) suiddir: bool,
    pub(crate) sync: bool,
    pub(crate) uid: Option<u32>,

    // Optional FUSE features
    pub(crate) dont_mask: bool,
    pub(crate) no_open_support: bool,
    pub(crate) no_open_dir_support: bool,
    pub(crate) handle_killpriv: bool,
    pub(crate) handle_killpriv_v2: bool,
    pub(crate) write_back: bool,
    pub(crate) force_readdir_plus: bool,

    // Transport concurrency (L1 IOPS-parity program). These are
    // daemon-level values consumed by the INIT reply / the
    // FUSE-over-io_uring geometry resolver — never passed to mount(2).
    /// Override the INIT-reply `max_background` (0/None = policy default:
    /// clamp(queues × depth, 64, 256)).
    pub(crate) max_background: Option<u16>,
    /// Override the INIT-reply `congestion_threshold` (0/None = ¾ of
    /// `max_background`).
    pub(crate) congestion_threshold: Option<u16>,
    /// Cap on total FUSE-over-io_uring payload-arena bytes
    /// (queues × depth × payload_sz). The embedder derives this from its
    /// memory budget (SqueezeFS: min(mem_budget / 8, 2 GiB)); unset falls
    /// back to min(RAM / 8, 2 GiB).
    pub(crate) transport_buffer_cap_bytes: Option<u64>,

    // Other FUSE mount options
    // default 40000
    #[cfg(target_os = "linux")]
    pub(crate) rootmode: Option<u32>,
}

impl MountOptions {
    /// set fuse filesystem mount `user_id`, default is current uid.
    pub fn uid(&mut self, uid: u32) -> &mut Self {
        self.uid.replace(uid);

        self
    }

    /// set fuse filesystem mount `group_id`, default is current gid.
    pub fn gid(&mut self, gid: u32) -> &mut Self {
        self.gid.replace(gid);

        self
    }

    /// set fuse filesystem name — the mount's SOURCE column (`fsname=`),
    /// default is **fuse**. Independent of [`Self::subtype`]: xfstests'
    /// mount helper, for one, passes the device path here.
    pub fn fs_name(&mut self, name: impl Into<String>) -> &mut Self {
        self.fs_name.replace(name.into());

        self
    }

    /// set the fuse filesystem SUBTYPE (`subtype=`): the mount reports as
    /// type `fuse.<subtype>` in `/proc/mounts`, `mount` and `df -T` instead
    /// of a bare `fuse`. Independent of [`Self::fs_name`] (the source
    /// column) — the two are separate FUSE options. Unset = bare `fuse`.
    pub fn subtype(&mut self, subtype: impl Into<String>) -> &mut Self {
        self.subtype.replace(subtype.into());

        self
    }

    /// set fuse filesystem `rootmode`, default is 40000.
    #[cfg(target_os = "linux")]
    pub fn rootmode(&mut self, rootmode: u32) -> &mut Self {
        self.rootmode.replace(rootmode);

        self
    }

    /// set fuse filesystem `allow_root` mount option, default is disable.
    pub fn allow_root(&mut self, allow_root: bool) -> &mut Self {
        self.allow_root = allow_root;

        self
    }

    /// set fuse filesystem `allow_other` mount option, default is disable.
    pub fn allow_other(&mut self, allow_other: bool) -> &mut Self {
        self.allow_other = allow_other;

        self
    }

    /// set fuse filesystem `ro` mount option, default is disable.
    pub fn read_only(&mut self, read_only: bool) -> &mut Self {
        self.read_only = read_only;

        self
    }

    /// allow fuse filesystem mount on a non-empty directory, default is not allowed.
    pub fn nonempty(&mut self, nonempty: bool) -> &mut Self {
        self.nonempty = nonempty;

        self
    }

    /// set fuse filesystem `default_permissions` mount option, default is disable.
    ///
    /// When `default_permissions` is set, the [`raw::access`] and [`path::access`] is useless.
    ///
    /// [`raw::access`]: crate::raw::Filesystem::access
    /// [`path::access`]: crate::path::PathFilesystem::access
    pub fn default_permissions(&mut self, default_permissions: bool) -> &mut Self {
        self.default_permissions = default_permissions;

        self
    }

    /// don't apply umask to file mode on create operations, default is disable.
    pub fn dont_mask(&mut self, dont_mask: bool) -> &mut Self {
        self.dont_mask = dont_mask;

        self
    }

    /// mount with `MS_NOSUID` (suid/sgid bits ignored), default is disable.
    /// Root mounts apply it as a mount(2) flag; unprivileged mounts pass
    /// `nosuid` to fusermount.
    pub fn nosuid(&mut self, nosuid: bool) -> &mut Self {
        self.nosuid = nosuid;

        self
    }

    /// mount with `MS_NODEV` (device nodes not interpreted), default is disable.
    pub fn nodev(&mut self, nodev: bool) -> &mut Self {
        self.nodev = nodev;

        self
    }

    /// mount with `MS_NOEXEC` (execution refused), default is disable.
    pub fn noexec(&mut self, noexec: bool) -> &mut Self {
        self.noexec = noexec;

        self
    }

    /// make kernel support zero-message opens, default is disable
    pub fn no_open_support(&mut self, no_open_support: bool) -> &mut Self {
        self.no_open_support = no_open_support;

        self
    }

    /// make kernel support zero-message opendir, default is disable
    pub fn no_open_dir_support(&mut self, no_open_dir_support: bool) -> &mut Self {
        self.no_open_dir_support = no_open_dir_support;

        self
    }

    /// fs handle killing `suid`/`sgid`/`cap` on `write`/`chown`/`trunc`, default is disable.
    pub fn handle_killpriv(&mut self, handle_killpriv: bool) -> &mut Self {
        self.handle_killpriv = handle_killpriv;

        self
    }

    /// fs handles killing `suid`/`sgid`/`cap` on `write`/`chown`/`trunc`
    /// under the **v2** contract (`FUSE_HANDLE_KILLPRIV_V2`, Linux ≥ 5.11):
    /// the kernel stops its per-write(2) `GETXATTR("security.capability")`
    /// killpriv probe and instead flags requests
    /// (`FUSE_WRITE_KILL_SUIDGID` / `FUSE_OPEN_KILL_SUIDGID` /
    /// `FATTR_KILL_SUIDGID`) whose handler must clear S_ISUID always,
    /// S_ISGID only when the file is group-executable (sgid without
    /// group-exec is the mandatory-locking marker and must survive), and
    /// drop the `security.capability` xattr. Default is disable — only
    /// enable when the filesystem implements that clearing law.
    pub fn handle_killpriv_v2(&mut self, handle_killpriv_v2: bool) -> &mut Self {
        self.handle_killpriv_v2 = handle_killpriv_v2;

        self
    }

    /// try to set the `FUSE_WRITEBACK_CACHE` enable write back cache for buffered writes, default
    /// is disable.
    ///
    /// # Notes:
    ///
    /// if enable this feature, when write flags has `FUSE_WRITE_CACHE`, file handle is guessed.
    pub fn write_back(&mut self, write_back: bool) -> &mut Self {
        self.write_back = write_back;

        self
    }

    /// force filesystem use readdirplus only, when kernel use readdir will return `ENOSYS`,
    /// default is disable.
    ///
    /// # Notes:
    /// this may don't work with some old Linux Kernel.
    pub fn force_readdir_plus(&mut self, force_readdir_plus: bool) -> &mut Self {
        self.force_readdir_plus = force_readdir_plus;

        self
    }

    /// set custom options for fuse filesystem, the custom options will be used in mount
    pub fn custom_options(&mut self, custom_options: impl Into<OsString>) -> &mut Self {
        self.custom_options = Some(custom_options.into());

        self
    }

    /// Override the INIT-reply `max_background` (kernel cap on queued
    /// background requests — async DIO, readahead, writeback). Default is
    /// the L1 policy: clamp(over-uring queues × depth, 64, 256). Values
    /// of 0 are ignored (the kernel treats 0 as "keep default").
    pub fn max_background(&mut self, max_background: u16) -> &mut Self {
        if max_background > 0 {
            self.max_background = Some(max_background);
        }

        self
    }

    /// Override the INIT-reply `congestion_threshold`. Default is ¾ of
    /// `max_background` (the kernel's own ratio). Values of 0 are ignored.
    pub fn congestion_threshold(&mut self, congestion_threshold: u16) -> &mut Self {
        if congestion_threshold > 0 {
            self.congestion_threshold = Some(congestion_threshold);
        }

        self
    }

    /// Cap the total FUSE-over-io_uring payload-arena bytes; the per-queue
    /// ring depth degrades from its desired 32 toward the floor of 4 to
    /// fit (see `TransportGeometry` in the fuse-over-uring module).
    pub fn transport_buffer_cap_bytes(&mut self, cap: u64) -> &mut Self {
        self.transport_buffer_cap_bytes = Some(cap);

        self
    }

    #[cfg(target_os = "freebsd")]
    pub(crate) fn build(&self) -> Nmount {
        let mut nmount = Nmount::new();
        nmount
            .str_opt(c"fstype", c"fusefs")
            .str_opt(c"from", c"/dev/fuse");
        if self.allow_other {
            nmount.null_opt(c"allow_other");
        }
        if self.allow_root {
            nmount.null_opt(c"allow_root");
        }
        if self.default_permissions {
            nmount.null_opt(c"default_permissions");
        }
        if let Some(subtype) = &self.subtype {
            nmount.str_opt_owned(c"subtype=", subtype.as_str());
        }
        if self.intr {
            nmount.null_opt(c"intr");
        }
        if let Some(custom_options) = self.custom_options.as_ref() {
            nmount.null_opt_owned(custom_options.as_os_str());
        }
        // TODO: additional options: push_symlinks_in, max_read=, timeout=
        nmount
    }

    #[cfg(target_os = "linux")]
    pub(crate) fn build(&self, fd: RawFd) -> OsString {
        let mut opts = vec![
            format!("fd={fd}"),
            format!(
                "user_id={}",
                self.uid.unwrap_or_else(|| unistd::getuid().as_raw())
            ),
            format!(
                "group_id={}",
                self.gid.unwrap_or_else(|| unistd::getgid().as_raw())
            ),
            format!("rootmode={}", self.rootmode.unwrap_or(40000)),
        ];

        // `subtype=` is what makes the kernel report the mount as
        // `fuse.<subtype>` instead of a bare `fuse` (the FreeBSD builder's
        // nmount form of the same option). Never derived from `fs_name`:
        // that is the SOURCE column, and callers put device paths there.
        if let Some(subtype) = &self.subtype {
            opts.push(format!("subtype={subtype}"));
        }

        if self.allow_root {
            opts.push("allow_root".to_string());
        }

        if self.allow_other {
            opts.push("allow_other".to_string());
        }

        if self.default_permissions {
            opts.push("default_permissions".to_string());
        }

        let mut options = OsString::from(opts.join(","));

        if let Some(custom_options) = &self.custom_options {
            options.push(",");
            options.push(custom_options);
        }

        options
    }

    #[cfg(all(target_os = "linux", feature = "unprivileged"))]
    pub(crate) fn build_with_unprivileged(&self) -> OsString {
        let mut opts = vec![
            format!(
                "user_id={}",
                self.uid.unwrap_or_else(|| unistd::getuid().as_raw())
            ),
            format!(
                "group_id={}",
                self.gid.unwrap_or_else(|| unistd::getgid().as_raw())
            ),
            format!("rootmode={}", self.rootmode.unwrap_or(40000)),
            format!(
                "fsname={}",
                self.fs_name.as_ref().unwrap_or(&"fuse".to_string())
            ),
        ];

        // fusermount3 turns `subtype=<subtype>` into the `fuse.<subtype>`
        // type (and drops it from the options it passes the kernel).
        if let Some(subtype) = &self.subtype {
            opts.push(format!("subtype={subtype}"));
        }

        if self.allow_root {
            opts.push("allow_root".to_string());
        }

        if self.allow_other {
            opts.push("allow_other".to_string());
        }

        if self.read_only {
            opts.push("ro".to_string());
        }

        if self.default_permissions {
            opts.push("default_permissions".to_string());
        }

        if self.nosuid {
            opts.push("nosuid".to_string());
        }

        if self.nodev {
            opts.push("nodev".to_string());
        }

        if self.noexec {
            opts.push("noexec".to_string());
        }

        let mut options = OsString::from(opts.join(","));

        if let Some(custom_options) = &self.custom_options {
            if let Some(custom_str) = custom_options.to_str() {
                let filtered: Vec<&str> = custom_str
                    .split(',')
                    .filter(|opt| {
                        let name = opt.split('=').next().unwrap_or("").trim();
                        !matches!(
                            name,
                            "max_read"
                                | "max_write"
                                | "max_pages"
                                | "max_readahead"
                                | "max_background"
                                | "congestion_threshold"
                                | "async_read"
                        )
                    })
                    .collect();
                if !filtered.is_empty() {
                    options.push(",");
                    options.push(filtered.join(","));
                }
            } else {
                options.push(",");
                options.push(custom_options);
            }
        }

        options
    }

    #[cfg(target_os = "freebsd")]
    pub(crate) fn flags(&self) -> nix::mount::MntFlags {
        use nix::mount::MntFlags;

        let mut flags = MntFlags::empty();
        if self.noatime {
            flags.insert(MntFlags::MNT_NOATIME);
        }
        if self.noexec {
            flags.insert(MntFlags::MNT_NOEXEC);
        }
        if self.nosuid {
            flags.insert(MntFlags::MNT_NOSUID);
        }
        if self.read_only {
            flags.insert(MntFlags::MNT_RDONLY);
        }
        if self.suiddir {
            flags.insert(MntFlags::MNT_SUIDDIR);
        }
        if self.sync {
            flags.insert(MntFlags::MNT_SYNCHRONOUS);
        }
        flags
    }

    #[cfg(target_os = "linux")]
    pub(crate) fn flags(&self) -> nix::mount::MsFlags {
        use nix::mount::MsFlags;

        let mut flags = MsFlags::empty();
        if self.dirsync {
            flags.insert(MsFlags::MS_DIRSYNC);
        }
        if self.noatime {
            flags.insert(MsFlags::MS_NOATIME);
        }
        if self.nodev {
            flags.insert(MsFlags::MS_NODEV);
        }
        if self.nodiratime {
            flags.insert(MsFlags::MS_NODIRATIME);
        }
        if self.noexec {
            flags.insert(MsFlags::MS_NOEXEC);
        }
        if self.nosuid {
            flags.insert(MsFlags::MS_NOSUID);
        }
        if self.read_only {
            flags.insert(MsFlags::MS_RDONLY);
        }
        if self.sync {
            flags.insert(MsFlags::MS_SYNCHRONOUS);
        }
        flags
    }
}

#[cfg(all(test, target_os = "linux"))]
mod tests {
    use super::*;

    fn opts_of(s: &OsString) -> Vec<String> {
        s.to_string_lossy().split(',').map(str::to_string).collect()
    }

    /// The kernel names a FUSE mount `fuse` unless the mount options carry
    /// `subtype=<name>`, in which case `/proc/mounts`, `mount` and `df -T`
    /// show `fuse.<name>`. `subtype` and `fsname` are INDEPENDENT FUSE
    /// options — the type suffix vs the source column — and the 1.2.1 gate
    /// proved why they must stay so: xfstests' mount helper passes
    /// `-o fsname=<device>`, and deriving the subtype from the name produced
    /// the type `fuse./dev/shm/squeezefs_fstests_test_meta` (xfstests then
    /// refused: "mounted but not a type fuse filesystem").
    #[test]
    fn root_mount_options_carry_the_subtype_independent_of_fsname() {
        let mut mo = MountOptions::default();
        mo.subtype("squeezefs");
        mo.fs_name("/dev/shm/squeezefs_fstests_test_meta");
        let opts = opts_of(&mo.build(7));
        assert!(
            opts.iter().any(|o| o == "subtype=squeezefs"),
            "root mount(2) options must carry subtype=<subtype>: {opts:?}"
        );
        assert!(
            !opts.iter().any(|o| o.starts_with("subtype=/")),
            "the fsname must never leak into the subtype: {opts:?}"
        );
        assert!(opts.iter().any(|o| o == "fd=7"));
    }

    #[cfg(feature = "unprivileged")]
    #[test]
    fn fusermount_options_carry_the_subtype_and_the_fsname_separately() {
        let mut mo = MountOptions::default();
        mo.subtype("squeezefs");
        mo.fs_name("/dev/shm/squeezefs_fstests_test_meta");
        let opts = opts_of(&mo.build_with_unprivileged());
        assert!(
            opts.iter().any(|o| o == "subtype=squeezefs"),
            "fusermount3 options must carry subtype=<subtype>: {opts:?}"
        );
        assert!(
            opts.iter()
                .any(|o| o == "fsname=/dev/shm/squeezefs_fstests_test_meta"),
            "the source column is the fsname verbatim: {opts:?}"
        );
    }

    /// A subtype with no fsname: the source column keeps the historical
    /// `fuse` on the fusermount3 path; the type is `fuse.<subtype>`.
    #[cfg(feature = "unprivileged")]
    #[test]
    fn subtype_without_fsname_keeps_the_default_source() {
        let mut mo = MountOptions::default();
        mo.subtype("squeezefs");
        let opts = opts_of(&mo.build_with_unprivileged());
        assert!(opts.iter().any(|o| o == "subtype=squeezefs"), "{opts:?}");
        assert!(opts.iter().any(|o| o == "fsname=fuse"), "{opts:?}");
    }

    /// No subtype set stays the historical shape: type `fuse`, even when a
    /// fsname is given (the two options are independent).
    #[test]
    fn no_subtype_emits_no_subtype_even_with_a_fsname() {
        let mut mo = MountOptions::default();
        mo.fs_name("/dev/shm/some_meta");
        let opts = opts_of(&mo.build(7));
        assert!(!opts.iter().any(|o| o.starts_with("subtype=")), "{opts:?}");
    }
}
