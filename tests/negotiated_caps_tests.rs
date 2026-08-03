//! FUSE-4b / 4c / 4d — every negotiated capability must match implemented
//! semantics (pre-rc-engineering-spec §4 FUSE-4).
//!
//! **4b — `FUSE_EXPORT_SUPPORT`.** The kernel encodes `(nodeid,
//! generation)` into every NFS file handle and compares the generation a
//! LOOKUP returns against the handle's (`fuse_get_dentry`: `generation !=
//! inode->i_generation ⇒ ESTALE`). Every reply hardcoded `1`, so a handle
//! minted before a `format` resolved happily against the same ino in the
//! NEW filesystem — a DIFFERENT FILE — where the protocol has a dedicated
//! error for exactly this. The volume-set generation identity (the v3
//! superblock uuids, joined in volume order) is already the filesystem's
//! generation identity; this is it, folded to the `u32` the kernel stores.
//!
//! **4c — `FUSE_CACHE_SYMLINKS`.** The kernel caches a symlink's target
//! page indefinitely and the daemon has no invalidation path for it. The
//! flag is nonetheless honest, and this suite pins the two facts that make
//! it so: a symlink target is written EXACTLY ONCE (at `symlink()`, in the
//! create transaction — POSIX-3) and can never be rewritten afterwards
//! (POSIX offers no retarget call, and the VAL-2 allowlist refuses
//! `system.symlink` through `setxattr`/`removexattr`), while v3 allocates
//! inodes monotonically and never reuses them, so a cached page can never
//! be re-pointed at another file's target either. The grep guard is the
//! enforcement: a NEW writer of `system.symlink` fails this test, because
//! such a writer would need the invalidation path this flag currently does
//! not require (the R-6 unified-purge grep-guard precedent).
//!
//! **4d — `max_readahead`.** Echoed verbatim and never consulted. Pinned
//! as DELIBERATE independence with the number published for operators:
//! `max_readahead` bounds the KERNEL's per-file readahead requests to the
//! daemon, while R2's prefetch window is a device-side pipeline depth
//! derived from measured bandwidth × latency and clamped by the R5 budget.
//! Coupling them would make a kernel-side page-cache limit clamp a
//! device-side pipeline (or the reverse) — different resources, different
//! units.

use squeezefs::fuse_client::{derive_entry_generation, entry_generation, set_entry_generation};

#[test]
fn the_entry_generation_is_derived_from_the_filesystem_identity() {
    // Distinct filesystems (a reformat mints a fresh superblock uuid) must
    // get distinct generations — that is the whole ESTALE mechanism.
    let a = derive_entry_generation("v3:00112233445566778899aabbccddeeff");
    let b = derive_entry_generation("v3:ffeeddccbbaa99887766554433221100");
    assert_ne!(
        a, b,
        "FUSE-4b: two formats must not share a generation, or a pre-reformat \
         NFS handle resolves to a different file instead of ESTALE"
    );
    // Deterministic across calls (the same mount must answer every LOOKUP
    // with the same generation).
    assert_eq!(a, derive_entry_generation("v3:00112233445566778899aabbccddeeff"));
    // The kernel stores `i_generation` as a u32 and SKIPS the comparison
    // when the handle's generation is 0 (`handle->generation && ...`).
    for s in [
        "",
        "v3:0",
        "v3:00112233445566778899aabbccddeeff|v3:ffeeddccbbaa99887766554433221100",
    ] {
        let g = derive_entry_generation(s);
        assert!(g > 0, "generation 0 disables the kernel's own check");
        assert!(
            g <= u64::from(u32::MAX),
            "generation must survive the kernel's u32 i_generation ({g})"
        );
    }
}

#[test]
fn the_published_generation_is_what_replies_carry() {
    set_entry_generation("v3:aabbccddeeff00112233445566778899");
    let g = entry_generation();
    assert_eq!(
        g,
        derive_entry_generation("v3:aabbccddeeff00112233445566778899")
    );
    assert_ne!(
        g, 1,
        "FUSE-4b: a mount with a real identity must not still answer 1 — \
         that value is the pre-fix hardcode and the in-RAM default"
    );
}

/// FUSE-4c: the grep guard. `system.symlink` may be WRITTEN in exactly one
/// place (the `symlink()` create transaction). A second writer would make
/// the kernel's cached symlink page staleable, which is precisely what
/// `FUSE_CACHE_SYMLINKS` promises cannot happen.
#[test]
fn nothing_but_symlink_creation_writes_a_symlink_target() {
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
    let mut writers = Vec::new();
    for entry in walk_rs(&root.join("src")) {
        let text = std::fs::read_to_string(&entry).expect("read source");
        for (i, line) in text.lines().enumerate() {
            let l = line.trim();
            if l.starts_with("//") || l.starts_with("///") {
                continue;
            }
            if !l.contains("\"system.symlink\"") {
                continue;
            }
            // Reads are unrestricted; only mutations matter.
            if l.contains("setxattr") || l.contains("removexattr") {
                writers.push(format!("{}:{}: {l}", entry.display(), i + 1));
            }
        }
    }
    assert_eq!(
        writers.len(),
        1,
        "FUSE-4c: exactly ONE writer of `system.symlink` may exist (the \
         symlink() create transaction). A new mutation path needs a kernel \
         symlink-cache invalidation before `FUSE_CACHE_SYMLINKS` can stay \
         advertised. Found:\n{}",
        writers.join("\n")
    );
    assert!(
        writers[0].contains("fuse_client.rs"),
        "the one writer must be the symlink handler, found {}",
        writers[0]
    );
}

fn walk_rs(dir: &std::path::Path) -> Vec<std::path::PathBuf> {
    let mut out = Vec::new();
    let Ok(rd) = std::fs::read_dir(dir) else {
        return out;
    };
    for e in rd.flatten() {
        let p = e.path();
        if p.is_dir() {
            out.extend(walk_rs(&p));
        } else if p.extension().is_some_and(|x| x == "rs") {
            out.push(p);
        }
    }
    out
}

/// FUSE-4d: the mount string must not carry `max_readahead` as a dead
/// letter. The kernel's readahead limit arrives in `fuse_init_in` and is
/// echoed from there; a token in the mount options is filtered out before
/// the kernel ever sees it, so shipping one only suggests a coupling that
/// does not exist.
#[test]
fn the_default_mount_options_do_not_carry_a_dead_readahead_token() {
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
    let text = std::fs::read_to_string(root.join("src/fuse_client.rs")).expect("read");
    let offenders: Vec<&str> = text
        .lines()
        .filter(|l| l.contains("max_readahead=") && !l.trim_start().starts_with("//"))
        .collect();
    assert!(
        offenders.is_empty(),
        "FUSE-4d: `max_readahead` is negotiated in the INIT reply (echoed \
         from `fuse_init_in`), and fuse3's own option filter strips this \
         token before the mount syscall — a dead letter that implies a \
         coupling with the R2 prefetch window that deliberately does not \
         exist:\n{}",
        offenders.join("\n")
    );
}
