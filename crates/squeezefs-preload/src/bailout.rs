//! Bind-time and per-op **bail-out classification** (§5.4.1).
//!
//! This is the client-side mirror of the §5.2 daemon fd screen — an
//! optimization that avoids doomed BIND round trips, **never the
//! security boundary** (the daemon re-screens every received fd; a
//! divergence here costs one refused BIND, not correctness).

/// Why a bind was not attempted / would be refused (client-side reason
/// buckets for the `preload_bind_refused{reason}` counters).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BindRefusal {
    /// fd status/type flags (O_PATH, O_APPEND, O_SYNC/O_DSYNC,
    /// O_TMPFILE-class incl. `st_nlink == 0`).
    Flags,
    /// `st_mode` is not a regular file.
    NotRegular,
}

/// Classify an fd from its `F_GETFL` flags + `fstat` results, mirroring
/// the daemon screen's order (§5.2 rules 1–5, minus st_dev — the caller
/// already matched the mount by st_dev to get here). `Ok((read_ok,
/// write_ok))` = bind-eligible with those per-direction rights.
pub fn classify_fd(
    flags: libc::c_int,
    st_mode: libc::mode_t,
    st_nlink: u64,
) -> Result<(bool, bool), BindRefusal> {
    // Rule 1 (load-bearing): O_PATH descriptions are obtainable with
    // search-only permission and their access-mode bits read O_RDONLY —
    // refuse before any mode interpretation.
    if flags & libc::O_PATH != 0 {
        return Err(BindRefusal::Flags);
    }
    // Rule 2: regular files only.
    if st_mode & libc::S_IFMT != libc::S_IFREG {
        return Err(BindRefusal::NotRegular);
    }
    // Rule 4 semantics screens: O_APPEND (atomic size authority),
    // O_SYNC/O_DSYNC (per-op durable barriers — the kernel path already
    // provides), the O_TMPFILE bits, and nlink == 0 (unnamed regular
    // file / open-then-unlinked — conservatively passthrough).
    if flags & libc::O_APPEND != 0
        || flags & libc::O_SYNC == libc::O_SYNC
        || flags & libc::O_DSYNC == libc::O_DSYNC
        || flags & libc::O_TMPFILE == libc::O_TMPFILE
        || st_nlink == 0
    {
        return Err(BindRefusal::Flags);
    }
    // Rule 5: rights strictly from the access mode, both directions.
    match flags & libc::O_ACCMODE {
        libc::O_RDONLY => Ok((true, false)),
        libc::O_WRONLY => Ok((false, true)),
        libc::O_RDWR => Ok((true, true)),
        _ => Err(BindRefusal::Flags),
    }
}

/// Per-op RWF screen for `preadv2`/`pwritev2` (§5.1): `true` = this op
/// must passthrough (binding stays — transient, like arena exhaustion).
///
/// Allow-list, not deny-list: **any flag we do not positively know the
/// ring can honor passes through** — silently dropping an unknown
/// future RWF flag would change semantics (§5.4.2
/// fallback-is-correctness).
pub fn rwf_passthrough(flags: libc::c_int) -> bool {
    // Served: RWF_HIPRI only — a priority hint, semantics-free.
    // RWF_SYNC/RWF_DSYNC demand a per-op durable barrier the v1 ring
    // does not provide (the same reason O_SYNC/O_DSYNC refuse at bind
    // time); RWF_APPEND is the atomic-size-authority refusal;
    // RWF_NOWAIT is known-but-unservable (§5.1). All of them — and any
    // unknown future flag — take the real call.
    const SERVED: libc::c_int = libc::RWF_HIPRI;
    flags & !SERVED != 0
}
