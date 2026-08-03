//! VAL-7g / VAL-7h / VAL-7i: path-traversal, log-file and subprocess-PATH
//! hardening (pre-RC spec §3 item VAL-7).
//!
//! * **7g** — `validate_nqn_component` rejected `/` and whitespace but not
//!   `.` or `..`. Its consumer builds configfs object paths from the
//!   component and its `ENOTEMPTY` fallback is `remove_dir_all` on that
//!   path, so `..` was a recursive delete of a configfs PARENT (the
//!   subsystem/port tree) one bad argument away. Dot components and
//!   leading-dot components now refuse loud.
//! * **7h** — the `--log-file` target was created `0644` with no
//!   `O_NOFOLLOW`: a pre-planted symlink at the path made the daemon
//!   append its log (which carries device paths, staging dirs, key names)
//!   through to an attacker-chosen file, world-readable.
//! * **7i** — root subprocesses inherited `$PATH` verbatim. The pinned-
//!   commit + clean-worktree checks are already in the tree (FIND-N3-B);
//!   the remaining half is not resolving `gcc`/`nvme`/`modprobe` through a
//!   caller-controlled, possibly world-writable search path.

use std::os::unix::fs::PermissionsExt;

/// VAL-7g: every path-traversal spelling refuses; ordinary NQNs still
/// pass (dots INSIDE a component are the normal NQN grammar —
/// `nqn.2026-07.io.squeezefs:x` — and must keep working).
#[test]
fn nqn_components_refuse_path_traversal() {
    use squeezefs::nvmeof::validate_nqn_component;

    for good in [
        "nqn.2026-07.io.squeezefs:share-x",
        "nqn.2014-08.org.nvmexpress:uuid:0000",
        "subsys1",
        "a.b.c",
    ] {
        validate_nqn_component("subnqn", good)
            .unwrap_or_else(|e| panic!("valid NQN '{good}' must pass: {e}"));
    }

    for bad in [
        "", "..", ".", "../x", "x/..", "a/b", ".hidden", "..leading", "x y", "\tx",
    ] {
        assert!(
            validate_nqn_component("subnqn", bad).is_err(),
            "'{bad}' names a configfs object that gets remove_dir_all'd on \
             the ENOTEMPTY fallback — it must refuse (VAL-7g)"
        );
    }
}

/// VAL-7h: the log file is created `0600` and the open refuses to follow a
/// symlink. Both legs are observable without root.
#[test]
fn log_file_is_private_and_never_follows_a_symlink() {
    let dir = tempfile::tempdir().expect("tempdir");

    // Fresh target: 0600.
    let fresh = dir.path().join("daemon.log");
    let f = squeezefs::open_log_file(&fresh).expect("open a fresh log target");
    drop(f);
    let mode = std::fs::metadata(&fresh).unwrap().permissions().mode() & 0o7777;
    assert_eq!(
        mode, 0o600,
        "the daemon log carries device paths, staging dirs and key names — \
         it must not be world-readable (VAL-7h)"
    );

    // Symlink at the target: refused, and the victim is untouched.
    let victim = dir.path().join("victim");
    std::fs::write(&victim, b"original\n").unwrap();
    let link = dir.path().join("planted.log");
    std::os::unix::fs::symlink(&victim, &link).unwrap();
    let err = squeezefs::open_log_file(&link)
        .expect_err("a symlinked log target must refuse (O_NOFOLLOW)");
    assert_eq!(
        err.raw_os_error(),
        Some(libc::ELOOP),
        "the refusal must be the kernel's O_NOFOLLOW ELOOP, got {err}"
    );
    assert_eq!(
        std::fs::read(&victim).unwrap(),
        b"original\n",
        "the symlink victim must never be written through"
    );

    // Appending to an existing regular file still works (log rotation /
    // restart) and keeps its bytes.
    let mut existing = std::fs::File::create(dir.path().join("existing.log")).unwrap();
    use std::io::Write;
    existing.write_all(b"prior\n").unwrap();
    drop(existing);
    let f = squeezefs::open_log_file(&dir.path().join("existing.log"))
        .expect("append to an existing log");
    drop(f);
    assert_eq!(
        std::fs::read(dir.path().join("existing.log")).unwrap(),
        b"prior\n",
        "opening for append must never truncate an existing log"
    );
}

/// VAL-7i: the root-subprocess search path drops relative entries, empty
/// entries (which mean "cwd") and group/world-writable directories, and
/// falls back to the standard system path when nothing survives.
#[test]
fn root_subprocess_path_is_sanitized() {
    use squeezefs::sanitized_root_path;

    let dir = tempfile::tempdir().expect("tempdir");
    let safe = dir.path().join("safe");
    std::fs::create_dir(&safe).unwrap();
    std::fs::set_permissions(&safe, std::fs::Permissions::from_mode(0o755)).unwrap();
    let hostile = dir.path().join("hostile");
    std::fs::create_dir(&hostile).unwrap();
    std::fs::set_permissions(&hostile, std::fs::Permissions::from_mode(0o777)).unwrap();

    let input = format!(
        "{}:{}::relative/bin:{}",
        safe.display(),
        hostile.display(),
        safe.display()
    );
    let out = sanitized_root_path(&input);
    let parts: Vec<&str> = out.split(':').collect();
    assert!(
        parts.contains(&safe.to_str().unwrap()),
        "a safe absolute dir must survive: {out}"
    );
    assert!(
        !parts.contains(&hostile.to_str().unwrap()),
        "a world-writable dir must be dropped — it lets any local user \
         plant `gcc`/`nvme` for a root subprocess: {out}"
    );
    assert!(
        !parts.iter().any(|p| p.is_empty()),
        "an empty PATH entry means the CWD and must be dropped: {out}"
    );
    assert!(
        !parts.iter().any(|p| !p.starts_with('/')),
        "relative entries must be dropped: {out}"
    );

    // Nothing survivable ⇒ the standard system path, never an empty PATH
    // (an empty PATH silently resolves nothing and would break every verb).
    let out = sanitized_root_path(&format!(":{}:rel", hostile.display()));
    assert!(
        out.contains("/usr/bin") && out.starts_with('/'),
        "an unusable PATH must fall back to the system default: {out}"
    );
}
