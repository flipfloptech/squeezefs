//! **The** environment-gate + skip-ledger helper for SqueezeFS test
//! binaries (`docs/pre-rc-engineering-spec.md` §11 **TEST-2**).
//!
//! ## The problem this closes
//!
//! Sixteen test files self-skipped on a missing `/dev/fuse`,
//! `fuse.enable_uring`, `fusermount3`, block device, O_DIRECT-capable
//! scratch fs, sudo, or git — each with its own private copy of the
//! ladder and its own `eprintln!("[SKIP] …")`. Two consequences, both
//! fatal to a release gate:
//!
//! 1. `cargo test --all-features` reports **all green** with the
//!    product's core mechanism (the FUSE-over-io_uring transport)
//!    entirely unexecuted — twelve of those files are the whole live-mount
//!    surface.
//! 2. libtest captures `eprintln!`, so without `--nocapture` the skip
//!    notices are **invisible**. A green run and a green-but-skipped run
//!    are indistinguishable from the outside.
//!
//! ## The contract
//!
//! Every environment-conditional skip in the tree routes through this
//! crate. That buys three things a per-file helper cannot:
//!
//! * **`SQUEEZEFS_TEST_REQUIRE_MOUNT=1`** turns every *mount-class* skip
//!   into a hard test **failure** (the release-gate posture —
//!   `tests/run_require_mount_gate.sh`). `SQUEEZEFS_TEST_REQUIRE_ALL=1`
//!   does the same for every class.
//! * **A machine-readable skip ledger**: one JSON object per line, both
//!   on an **uncaptured** stderr handle (`##SQUEEZEFS-SKIP## {…}` —
//!   written through [`std::io::stderr`], which libtest's
//!   `set_output_capture` does **not** intercept, unlike `eprintln!`) and,
//!   when `SQUEEZEFS_TEST_SKIP_LEDGER` names a path, appended there for
//!   the harness to diff.
//! * **One place a 15th file must go through.** A new environment gate
//!   calls [`declare`] (or a named gate below); there is no supported way
//!   to skip silently.
//!
//! ## Why a crate and not `tests/common/mod.rs`
//!
//! Each test binary uses a different subset of the gates. A private
//! module compiled into sixteen binaries would need a blanket
//! `#[allow(dead_code)]` for the unused ones — which AGENTS.md forbids.
//! A library crate's `pub` surface is exempt by construction.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{SystemTime, UNIX_EPOCH};

/// Why a test declined to run. The class is what a require-mode keys on
/// and what the ledger groups by, so it is a closed set — a new kind of
/// environment dependency adds a variant here, in the open.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SkipClass {
    /// A live FUSE mount is unavailable (`/dev/fuse`,
    /// `fuse.enable_uring`, `fusermount3`, fusectl). **The class the
    /// release gate refuses to tolerate** — it covers the product's core
    /// mechanism.
    Mount,
    /// The test needs to run as root and does not.
    Root,
    /// The test needs a *non-root* identity (permission-simulation
    /// contracts) and is running as root.
    NonRoot,
    /// Passwordless `sudo` is unavailable.
    Sudo,
    /// Absent hardware (NVMe namespace, controller char device, a
    /// zero-capacity block device …).
    Hardware,
    /// A host tool is missing (`git`, an external suite binary …).
    Toolchain,
    /// The scratch filesystem lacks a capability (O_DIRECT) or the host
    /// lacks capacity (free space, CPU count).
    Capability,
    /// Opt-in scenario: the test only runs when an env var selects it.
    OptIn,
}

impl SkipClass {
    /// The stable ledger token. Never rename one of these without
    /// updating the harness scripts that grep for them.
    pub fn as_str(self) -> &'static str {
        match self {
            SkipClass::Mount => "mount",
            SkipClass::Root => "root",
            SkipClass::NonRoot => "non-root",
            SkipClass::Sudo => "sudo",
            SkipClass::Hardware => "hardware",
            SkipClass::Toolchain => "toolchain",
            SkipClass::Capability => "capability",
            SkipClass::OptIn => "opt-in",
        }
    }

    /// The env var that promotes this class from skip to failure.
    /// `SQUEEZEFS_TEST_REQUIRE_ALL` promotes every class.
    pub fn require_var(self) -> &'static str {
        match self {
            SkipClass::Mount => "SQUEEZEFS_TEST_REQUIRE_MOUNT",
            SkipClass::Root => "SQUEEZEFS_TEST_REQUIRE_ROOT",
            SkipClass::NonRoot => "SQUEEZEFS_TEST_REQUIRE_NON_ROOT",
            SkipClass::Sudo => "SQUEEZEFS_TEST_REQUIRE_SUDO",
            SkipClass::Hardware => "SQUEEZEFS_TEST_REQUIRE_HARDWARE",
            SkipClass::Toolchain => "SQUEEZEFS_TEST_REQUIRE_TOOLCHAIN",
            SkipClass::Capability => "SQUEEZEFS_TEST_REQUIRE_CAPABILITY",
            SkipClass::OptIn => "SQUEEZEFS_TEST_REQUIRE_OPT_IN",
        }
    }
}

/// Where a skip was declared. Built by [`site!`] at the call site so the
/// ledger names the test, not this crate.
#[derive(Debug, Clone, Copy)]
pub struct Site {
    /// The test binary (`CARGO_CRATE_NAME` at the call site).
    pub bin: &'static str,
    /// Source file, repo-relative.
    pub file: &'static str,
    /// Source line of the gate call.
    pub line: u32,
    /// The enclosing function's path, via the `type_name` trick.
    pub test: &'static str,
}

/// Capture the call site. Use at the *gate call*, never inside a helper —
/// the point is to name the test that skipped.
#[macro_export]
macro_rules! site {
    () => {{
        fn __sqz_site_marker() {}
        fn __sqz_type_name_of<T>(_: T) -> &'static str {
            ::std::any::type_name::<T>()
        }
        // `…::__sqz_site_marker` → strip the marker to get the enclosing
        // function path (the standard function_name! trick).
        let raw = __sqz_type_name_of(__sqz_site_marker);
        let test = raw.strip_suffix("::__sqz_site_marker").unwrap_or(raw);
        $crate::Site {
            bin: ::std::env!("CARGO_CRATE_NAME"),
            file: ::std::file!(),
            line: ::std::line!(),
            test,
        }
    }};
}

/// Declare a one-off environment skip and **return from the test**:
/// `skip!(Hardware, "no NVMe namespace on this machine")`.
///
/// This is the replacement for the retired `eprintln!("[SKIP] …"); return;`
/// idiom. It ledgers the record (so a harness can diff it) and honours the
/// `SQUEEZEFS_TEST_REQUIRE_*` promotion, which a bare `eprintln!` cannot.
#[macro_export]
macro_rules! skip {
    ($class:ident, $($arg:tt)+) => {{
        let _ = $crate::declare(
            $crate::site!(),
            $crate::SkipClass::$class,
            &::std::format!($($arg)+),
        );
        return;
    }};
}

fn env_on(var: &str) -> bool {
    matches!(
        std::env::var(var)
            .unwrap_or_default()
            .trim()
            .to_ascii_lowercase()
            .as_str(),
        "1" | "y" | "yes" | "true" | "on"
    )
}

/// Is this class currently required (skip ⇒ failure)?
pub fn required(class: SkipClass) -> bool {
    env_on("SQUEEZEFS_TEST_REQUIRE_ALL") || env_on(class.require_var())
}

fn json_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 8);
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out
}

/// Emit one ledger record.
///
/// Two channels, on purpose:
/// * [`std::io::stderr`] — a direct handle write, which libtest's
///   `set_output_capture` does **not** intercept (only the `eprintln!`
///   family consults the capture TLS). This is the fix for "skip notices
///   are swallowed without `--nocapture`".
/// * the file named by `SQUEEZEFS_TEST_SKIP_LEDGER`, appended with
///   `O_APPEND` so parallel test binaries interleave whole lines.
fn emit(site: &Site, class: SkipClass, reason: &str, is_required: bool) {
    let ts_ms = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0);
    let line = format!(
        r#"{{"ts_ms":{ts_ms},"class":"{class}","bin":"{bin}","test":"{test}","file":"{file}","line":{line},"reason":"{reason}","required":{req}}}"#,
        class = class.as_str(),
        bin = json_escape(site.bin),
        test = json_escape(site.test),
        file = json_escape(site.file),
        line = site.line,
        reason = json_escape(reason),
        req = is_required,
    );
    // Uncaptured — visible without --nocapture.
    let mut err = std::io::stderr();
    let _ = writeln!(err, "##SQUEEZEFS-SKIP## {line}");
    let _ = err.flush();
    if let Ok(path) = std::env::var("SQUEEZEFS_TEST_SKIP_LEDGER") {
        if !path.is_empty() {
            if let Ok(mut f) = std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(&path)
            {
                let _ = writeln!(f, "{line}");
            }
        }
    }
}

/// Declare a skip: ledger it, then either return `false` (caller returns
/// from the test) or **panic** when the class is required.
///
/// This is the single sanctioned way to decline to run. Every named gate
/// below funnels here, and a new environment dependency calls it
/// directly rather than inventing a private `eprintln!` + `return`.
#[must_use = "a declared skip must actually skip — return from the test"]
pub fn declare(site: Site, class: SkipClass, reason: &str) -> bool {
    let req = required(class);
    emit(&site, class, reason, req);
    if req {
        panic!(
            "[REQUIRED-{}] {} ({}:{}) declined to run: {reason} — \
             {} is set, so this environment gate is a FAILURE, not a skip",
            class.as_str().to_ascii_uppercase(),
            site.test,
            site.file,
            site.line,
            if env_on("SQUEEZEFS_TEST_REQUIRE_ALL") {
                "SQUEEZEFS_TEST_REQUIRE_ALL"
            } else {
                class.require_var()
            },
        );
    }
    false
}

// ---------------------------------------------------------------------------
// Named gates
// ---------------------------------------------------------------------------

/// `fusermount3` on `PATH` or in the usual sbin/bin locations.
pub fn fusermount3_path() -> Option<PathBuf> {
    if let Ok(paths) = std::env::var("PATH") {
        for dir in std::env::split_paths(&paths) {
            let p = dir.join("fusermount3");
            if p.is_file() {
                return Some(p);
            }
        }
    }
    for p in [
        "/usr/bin/fusermount3",
        "/bin/fusermount3",
        "/usr/local/bin/fusermount3",
        "/usr/sbin/fusermount3",
    ] {
        let p = Path::new(p);
        if p.is_file() {
            return Some(p.to_path_buf());
        }
    }
    None
}

/// Is the kernel's `fuse.enable_uring` parameter on?
pub fn fuse_uring_enabled() -> Result<(), String> {
    match std::fs::read_to_string("/sys/module/fuse/parameters/enable_uring") {
        Ok(v)
            if matches!(
                v.trim().to_ascii_lowercase().as_str(),
                "y" | "1" | "yes" | "true" | "on"
            ) =>
        {
            Ok(())
        }
        other => Err(format!("kernel fuse.enable_uring not enabled ({other:?})")),
    }
}

/// The live-mount ladder: `/dev/fuse` → `fuse.enable_uring` →
/// `fusermount3`. `false` ⇒ the caller returns; under
/// `SQUEEZEFS_TEST_REQUIRE_MOUNT=1` this panics instead.
///
/// The FUSE request hot path is FUSE-over-io_uring **only** (AGENTS.md
/// non-negotiable), so a mount without `enable_uring` is not a degraded
/// mount — it is no mount at all, and the suites that need one must say
/// so rather than pass vacuously.
pub fn mount_supported(site: Site) -> bool {
    if !Path::new("/dev/fuse").exists() {
        return declare(site, SkipClass::Mount, "/dev/fuse not present");
    }
    if let Err(why) = fuse_uring_enabled() {
        return declare(site, SkipClass::Mount, &why);
    }
    if fusermount3_path().is_none() {
        return declare(site, SkipClass::Mount, "fusermount3 not available");
    }
    true
}

/// [`mount_supported`] plus a mounted fusectl at
/// `/sys/fs/fuse/connections` (the abort/queue-depth surface).
pub fn mount_supported_with_fusectl(site: Site) -> bool {
    if !mount_supported(site) {
        return false;
    }
    if !Path::new("/sys/fs/fuse/connections").is_dir() {
        return declare(
            site,
            SkipClass::Mount,
            "fusectl not mounted at /sys/fs/fuse/connections",
        );
    }
    true
}

/// Running as root?
pub fn is_root() -> bool {
    // SAFETY: `geteuid` is a pure query with no preconditions.
    unsafe { libc::geteuid() == 0 }
}

/// The test needs a non-root invoker (permission-simulation contracts).
pub fn non_root(site: Site, what: &str) -> bool {
    if is_root() {
        return declare(
            site,
            SkipClass::NonRoot,
            &format!("running as root — {what} needs a non-root test identity"),
        );
    }
    true
}

/// Passwordless `sudo` (non-root invoker assumed — check [`non_root`]
/// first when the contract needs both).
pub fn passwordless_sudo(site: Site) -> bool {
    let ok = Command::new("sudo")
        .args(["-n", "true"])
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false);
    if !ok {
        return declare(site, SkipClass::Sudo, "passwordless sudo unavailable");
    }
    true
}

/// Does `dir` support `O_DIRECT`? Probes with a real
/// `open(O_DIRECT|O_CREAT)` on a temp file, which is the only honest test
/// (tmpfs/overlayfs refuse at open time).
pub fn o_direct_supported(dir: &Path) -> bool {
    let probe = dir.join(format!(".sqz_odirect_probe_{}", std::process::id()));
    let c = match std::ffi::CString::new(probe.as_os_str().as_encoded_bytes()) {
        Ok(c) => c,
        Err(_) => return false,
    };
    // SAFETY: `c` is a valid NUL-terminated path for the duration of the
    // call; the mode is only consulted for O_CREAT.
    let fd = unsafe {
        libc::open(
            c.as_ptr(),
            libc::O_RDWR | libc::O_CREAT | libc::O_DIRECT | libc::O_CLOEXEC,
            0o600,
        )
    };
    if fd >= 0 {
        // SAFETY: `fd` is an fd this call just opened and still owns.
        unsafe { libc::close(fd) };
        let _ = std::fs::remove_file(&probe);
        true
    } else {
        let _ = std::fs::remove_file(&probe);
        false
    }
}

/// O_DIRECT gate on a scratch directory.
pub fn o_direct(site: Site, dir: &Path) -> bool {
    if !o_direct_supported(dir) {
        return declare(
            site,
            SkipClass::Capability,
            "scratch filesystem does not support O_DIRECT",
        );
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn class_tokens_are_stable_and_unique() {
        // The harness scripts grep these; a rename is a breaking change.
        let all = [
            SkipClass::Mount,
            SkipClass::Root,
            SkipClass::NonRoot,
            SkipClass::Sudo,
            SkipClass::Hardware,
            SkipClass::Toolchain,
            SkipClass::Capability,
            SkipClass::OptIn,
        ];
        let mut seen: Vec<&str> = all.iter().map(|c| c.as_str()).collect();
        seen.sort_unstable();
        let n = seen.len();
        seen.dedup();
        assert_eq!(n, seen.len(), "class tokens must be unique");
        assert_eq!(SkipClass::Mount.as_str(), "mount");
        assert_eq!(
            SkipClass::Mount.require_var(),
            "SQUEEZEFS_TEST_REQUIRE_MOUNT"
        );
    }

    #[test]
    fn json_escape_survives_quotes_and_control_bytes() {
        assert_eq!(json_escape("a\"b\\c"), "a\\\"b\\\\c");
        assert_eq!(json_escape("l1\nl2"), "l1\\nl2");
        assert_eq!(json_escape("\u{1}"), "\\u0001");
    }

    #[test]
    fn site_macro_names_the_enclosing_test() {
        let s = site!();
        assert!(
            s.test.ends_with("site_macro_names_the_enclosing_test"),
            "site! must name the enclosing fn, got {}",
            s.test
        );
        assert_eq!(s.bin, "squeezefs_testkit");
        assert!(s.file.ends_with("lib.rs"));
        assert!(s.line > 0);
    }
}
