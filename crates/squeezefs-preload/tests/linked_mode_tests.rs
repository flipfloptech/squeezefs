//! Tier-1 direct-link (`-lsqueezefs_il`) support — load-mode detection
//! contracts (SDK campaign 2026-08-04, `docs/design-sdk.md` Deliverable 1).
//!
//! The shim is deliberately ctor-free ("no ctor ordering games; first
//! call initializes" — interpose.rs), so loading it as a `DT_NEEDED`
//! dependency instead of an `LD_PRELOAD` entry changes nothing about
//! init order. What linked mode DOES need is an honest diagnostic: the
//! bootstrap path prints one line when the shim is active without
//! LD_PRELOAD naming it (direct link, dlopen, /etc/ld.so.preload), so
//! operators and the preload-gate battery can tell the two apart.
//!
//! These tests pin the PURE classification function the announce line
//! rides. The line itself (exactly-once, names DT_NEEDED) is pinned by
//! `tests/run_preload_gate.sh` leg 2b-linked against a real mount.

use squeezefs_il::session::{load_mode_from, LoadMode};

/// No LD_PRELOAD at all: we can only have arrived via the link map
/// (DT_NEEDED direct link, dlopen, or /etc/ld.so.preload).
#[test]
fn absent_env_is_linked() {
    assert_eq!(load_mode_from(None), LoadMode::Linked);
}

/// Empty/whitespace LD_PRELOAD carries no entries — still linked.
#[test]
fn empty_env_is_linked() {
    assert_eq!(load_mode_from(Some("")), LoadMode::Linked);
    assert_eq!(load_mode_from(Some("   ")), LoadMode::Linked);
}

/// The canonical preload spelling: the bare soname.
#[test]
fn bare_soname_is_preload() {
    assert_eq!(
        load_mode_from(Some("libsqueezefs_il.so")),
        LoadMode::Preload
    );
}

/// Absolute-path preload entries match on their basename.
#[test]
fn absolute_path_is_preload() {
    assert_eq!(
        load_mode_from(Some("/usr/lib64/libsqueezefs_il.so")),
        LoadMode::Preload
    );
}

/// glibc splits LD_PRELOAD on colons AND spaces; the shim may be any
/// entry, not the first.
#[test]
fn colon_and_space_separated_lists_match() {
    assert_eq!(
        load_mode_from(Some("libjemalloc.so.2:/opt/sqz/libsqueezefs_il.so")),
        LoadMode::Preload
    );
    assert_eq!(
        load_mode_from(Some("libjemalloc.so.2 libsqueezefs_il.so")),
        LoadMode::Preload
    );
}

/// A foreign preload list without the shim: the shim itself was linked.
#[test]
fn foreign_preloads_only_is_linked() {
    assert_eq!(load_mode_from(Some("libjemalloc.so.2")), LoadMode::Linked);
    assert_eq!(
        load_mode_from(Some("/usr/lib/libasan.so.8:libtsan.so.2")),
        LoadMode::Linked
    );
}

/// Prefix discipline: only a basename that IS this shim counts.
/// `libsqueezefs.so` (a different library) must not match; a directory
/// component named like the shim must not match (basename check).
#[test]
fn near_miss_names_are_linked() {
    assert_eq!(load_mode_from(Some("libsqueezefs.so")), LoadMode::Linked);
    assert_eq!(
        load_mode_from(Some("/opt/libsqueezefs_il/otherlib.so")),
        LoadMode::Linked
    );
}

/// Versioned/renamed copies of the shim still count (packagers may
/// ship `libsqueezefs_il.so.1.1.0` with a symlink chain).
#[test]
fn versioned_soname_is_preload() {
    assert_eq!(
        load_mode_from(Some("/usr/lib64/libsqueezefs_il.so.1.1.0")),
        LoadMode::Preload
    );
}
