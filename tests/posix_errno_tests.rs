//! Pre-RC POSIX-semantics contracts, spec §5 **POSIX-6** (structured
//! errno) and its **POSIX-5** rider (`EAGAIN` is reserved for
//! `O_NONBLOCK`).
//!
//! POSIX-6 is a **one-way door**: until this landed, `SqueezefsError::
//! to_errno()` derived the errno from **substring matches on English
//! error text** (`error.rs:63-72` — `"exists"` ⇒ `EEXIST`, `"table
//! full"`/`"No space"` ⇒ `ENOSPC`, `"Too many links"` ⇒ `EMLINK`, else
//! `EINVAL`), which made every `InvalidOperation` message load-bearing
//! wire format. An out-of-space refusal phrased any other way — the KV
//! allocator's own `"no space: … (ENOSPC)"`, lower-case `n` — returned
//! `EINVAL` to `write(2)`. Once applications depend on the substring
//! behavior it can never be changed, so the mapping becomes
//! **structural** here and is pinned mapping-by-mapping below.
//!
//! The law this file pins:
//!
//! 1. **Text is never wire format.** A message's words cannot change an
//!    errno. Every `InvalidOperation` is `EINVAL`, whatever it says.
//! 2. **Refusals carry their errno structurally** — `SqueezefsError::
//!    Refused { errno, .. }`, minted through named constructors.
//! 3. **The mapping is total**: every variant has a pinned errno (the
//!    exhaustive match in `pinned_errno` fails to COMPILE if a variant
//!    is added without a ruling), and every `io::ErrorKind` the tree
//!    constructs has an explicit row — no accidental `EIO`.
//! 4. **Producer-side**: the real backend paths that used to spell
//!    `EEXIST`/`EMLINK` in English still deliver those errnos.

use squeezefs::error::SqueezefsError;
use squeezefs::meta_backend::dlm::DlmGuard;
use squeezefs::meta_backend::kv::builder::{format_v3, FormatV3Options, ROOT_INO};
use squeezefs::meta_backend::kv::KvError;
use squeezefs::meta_backend::{open_volume_for_mount, Metadata};
use std::io;
use tempfile::NamedTempFile;

// ---------------------------------------------------------------------------
// 1. Totality — every variant has a ruling, every used ErrorKind a row.
// ---------------------------------------------------------------------------

/// The pinned table, written as an **exhaustive** match: adding a
/// `SqueezefsError` variant without ruling on its errno is a compile
/// error, which is what makes the POSIX-6 mapping total rather than
/// "whatever falls through".
fn pinned_errno(e: &SqueezefsError) -> libc::c_int {
    match e {
        // Faithful to the OS error when there is one, else the kind table.
        SqueezefsError::Io(io) => io
            .raw_os_error()
            .unwrap_or_else(|| io_kind_errno(io.kind())),
        SqueezefsError::Redis(_) => libc::ECOMM,
        // POSIX-5: a lost lease wait is NOT `EAGAIN` (reserved for
        // `O_NONBLOCK`); the retry ladder exhausts into `EIO`.
        SqueezefsError::LockFailed { .. } => libc::EIO,
        SqueezefsError::FencingTokenExpired { .. } => libc::EIO,
        SqueezefsError::InvalidOperation(_) => libc::EINVAL,
        SqueezefsError::Refused { errno, .. } => *errno,
        // RES-6: the D0 writer claim was fenced, so the data plane is
        // closed for this mount's lifetime. EIO is the honest report —
        // the operation cannot be retried into success here, and a
        // successor mount owns the accounting.
        SqueezefsError::WriterGuardFenced => libc::EIO,
        SqueezefsError::IndirectMapFormat { .. } => libc::EIO,
        // DLM S6: a fail-stopped lease reaching a data path is the same
        // class as a fenced writer guard — the I/O must not proceed.
        SqueezefsError::MembershipLeaseNotCustody(_) => libc::EIO,
        // PK4: a shipped publish the authority did not land — the class
        // is the lane's, the errno is the honest "could not do it".
        SqueezefsError::PublishFailure { .. } => libc::EIO,
        // PR 13 (review round 1, Issue 7): the symmetric plane's CLASSED
        // retryable refusal — every caller retries, the application too.
        SqueezefsError::Retryable { .. } => libc::EAGAIN,
        // PR 13 §4.4ai: the interim foreign-slot refusal is `EREMOTE` —
        // `EOPNOTSUPP` is the class coreutils' chmod/chown swallow (rc 0).
        SqueezefsError::ForeignSlotFileMutation { .. } => libc::EREMOTE,
        SqueezefsError::GdsError(_) => libc::EIO,
        SqueezefsError::CacheOverflow => libc::ENOMEM,
        SqueezefsError::Timeout => libc::ETIMEDOUT,
    }
}

/// Every `io::ErrorKind` the tree constructs, plus the POSIX-meaningful
/// neighbours, with the errno each must yield. `ErrorKind` is
/// `#[non_exhaustive]`, so the catch-all `EIO` stays — this table is what
/// keeps it from swallowing a kind that has a real errno.
fn io_kind_errno(kind: io::ErrorKind) -> libc::c_int {
    match kind {
        io::ErrorKind::NotFound => libc::ENOENT,
        io::ErrorKind::PermissionDenied => libc::EACCES,
        io::ErrorKind::AlreadyExists => libc::EEXIST,
        io::ErrorKind::InvalidInput => libc::EINVAL,
        io::ErrorKind::WouldBlock => libc::EWOULDBLOCK,
        io::ErrorKind::TimedOut => libc::ETIMEDOUT,
        io::ErrorKind::Unsupported => libc::ENOTSUP,
        io::ErrorKind::StorageFull => libc::ENOSPC,
        io::ErrorKind::OutOfMemory => libc::ENOMEM,
        io::ErrorKind::QuotaExceeded => libc::EDQUOT,
        io::ErrorKind::FileTooLarge => libc::EFBIG,
        io::ErrorKind::IsADirectory => libc::EISDIR,
        io::ErrorKind::NotADirectory => libc::ENOTDIR,
        io::ErrorKind::DirectoryNotEmpty => libc::ENOTEMPTY,
        io::ErrorKind::ReadOnlyFilesystem => libc::EROFS,
        io::ErrorKind::CrossesDevices => libc::EXDEV,
        io::ErrorKind::TooManyLinks => libc::EMLINK,
        io::ErrorKind::ResourceBusy => libc::EBUSY,
        io::ErrorKind::Interrupted => libc::EINTR,
        // Corruption / short device I/O / transport loss: a filesystem
        // syscall says EIO, never a socket errno.
        _ => libc::EIO,
    }
}

/// One instance of every variant, so the table above is exercised and
/// not merely written down.
fn one_of_each() -> Vec<SqueezefsError> {
    vec![
        SqueezefsError::Io(io::Error::new(io::ErrorKind::NotFound, "x")),
        SqueezefsError::Io(io::Error::from_raw_os_error(libc::EPERM)),
        SqueezefsError::Redis(squeezefs::error::MockRedisError),
        SqueezefsError::LockFailed {
            reason: "held".into(),
        },
        SqueezefsError::FencingTokenExpired {
            token: 1,
            expected: 2,
        },
        SqueezefsError::InvalidOperation("anything at all".into()),
        SqueezefsError::already_exists("File already exists"),
        SqueezefsError::no_space("insufficient capacity"),
        SqueezefsError::too_many_links("Too many links"),
        SqueezefsError::IndirectMapFormat {
            detail: "foreign".into(),
        },
        SqueezefsError::GdsError("gds".into()),
        SqueezefsError::CacheOverflow,
        SqueezefsError::Timeout,
        SqueezefsError::PublishFailure {
            class: squeezefs::error::PublishFailureClass::TransportOutcomeUnknown,
            msg: "lost".into(),
        },
        SqueezefsError::PublishFailure {
            class: squeezefs::error::PublishFailureClass::FrameRefused(0x57),
            msg: "refused".into(),
        },
        SqueezefsError::retryable(
            squeezefs::error::RefusalClass::SlotMoved { slot: 4, holder: 7 },
            "slot moved",
        ),
        SqueezefsError::retryable(
            squeezefs::error::RefusalClass::StaleHolderView,
            "stale holder view",
        ),
        SqueezefsError::foreign_slot_file_mutation("setattr of a foreign-slot file (PR 13b)"),
    ]
}

/// **PR 13 review round 1, Issue 7 — a symmetric-plane retryable refusal is
/// classified by its TYPED class, never by its prose.** The cross-owner
/// ladder decides "re-dispatch the step" versus "fail-stop the volume" on
/// `SqueezefsError::refusal_class()`; before this the two classifiers were
/// `to_string().contains("is leased by appender")` /
/// `contains("holder view is stale")` — a message-text rename, a
/// localized log line, or an unrelated refusal that happened to carry the
/// words (the striping flip's "directory … is leased by appender …" is
/// one) would have moved a live op onto the S3.5 lattice or off it. Pinned
/// here: (1) the ONE `KvError::SlotBusy` conversion mints the class; (2)
/// the class survives ANY message text, including one that names nothing;
/// (3) the OLD text without the class classifies as NOTHING; (4) the wire
/// word (`WireError::class`) carries the class across a ship and rebuilds
/// it, and an unknown word rebuilds an unclassed refusal with its errno.
#[test]
fn a_retryable_refusal_is_classified_by_its_typed_class_never_its_text() {
    use squeezefs::error::RefusalClass;
    use squeezefs::meta_ship::WireError;
    // (1) the conversion site.
    let e: SqueezefsError = KvError::SlotBusy {
        slot: 10,
        holder: 7,
        g: 3,
    }
    .into();
    assert!(
        matches!(
            e.refusal_class(),
            Some(RefusalClass::SlotMoved {
                slot: 10,
                holder: 7
            })
        ),
        "the SlotBusy conversion mints SlotMoved: {e:?}"
    );
    assert_eq!(e.to_errno(), libc::EAGAIN);
    // (2) the class rides ANY text.
    for text in [
        "",
        "renamed entirely",
        "slot 10 is leased by appender 7 (g 3)",
    ] {
        let e = SqueezefsError::retryable(RefusalClass::SlotMoved { slot: 1, holder: 2 }, text);
        assert!(matches!(
            e.refusal_class(),
            Some(RefusalClass::SlotMoved { .. })
        ));
        let e = SqueezefsError::retryable(RefusalClass::StaleHolderView, text);
        assert!(matches!(
            e.refusal_class(),
            Some(RefusalClass::StaleHolderView)
        ));
        assert_eq!(e.to_errno(), libc::EAGAIN);
    }
    // (3) the old prose alone classifies as nothing.
    for old in [
        "forest slot 10 is leased by appender 7 (g 3) — ships to its holder",
        "cross-owner guards … the initiator's holder view is stale",
    ] {
        let e = SqueezefsError::refused(libc::EAGAIN, old);
        assert_eq!(e.refusal_class(), None, "prose is never wire format: {old}");
        assert_eq!(
            SqueezefsError::InvalidOperation(old.into()).refusal_class(),
            None
        );
    }
    // (4) the wire word.
    for class in [
        RefusalClass::SlotMoved { slot: 5, holder: 6 },
        RefusalClass::StaleHolderView,
    ] {
        let w = WireError::from_error(&SqueezefsError::retryable(class, "any words"));
        assert_eq!(w.class, class.to_wire());
        assert_ne!(w.class, 0);
        let back = w.into_error();
        assert_eq!(
            back.refusal_class().map(RefusalClass::to_wire),
            Some(class.to_wire()),
            "the class rebuilds at the initiator from the word, not the text"
        );
        assert_eq!(back.to_errno(), libc::EAGAIN);
    }
    let plain = WireError::from_error(&SqueezefsError::refused(libc::ENOENT, "gone"));
    assert_eq!(plain.class, 0);
    assert_eq!(plain.clone().into_error().refusal_class(), None);
    assert_eq!(plain.into_error().to_errno(), libc::ENOENT);
    let unknown = WireError {
        errno: libc::EAGAIN,
        msg: "future class".into(),
        class: 0xEE,
    };
    assert_eq!(RefusalClass::from_wire(0xEE), None);
    let back = unknown.into_error();
    assert_eq!(
        back.refusal_class(),
        None,
        "an unknown word is never a guessed class"
    );
    assert_eq!(back.to_errno(), libc::EAGAIN);
}

#[test]
fn every_variant_maps_to_its_pinned_errno() {
    for e in one_of_each() {
        assert_eq!(
            e.to_errno(),
            pinned_errno(&e),
            "POSIX-6: {e:?} must map to its pinned errno"
        );
    }
}

#[test]
fn every_constructed_io_kind_has_an_explicit_row() {
    // Exactly the kinds this tree constructs (`ErrorKind::` census) plus
    // the POSIX-meaningful neighbours the table rules on.
    const KINDS: &[io::ErrorKind] = &[
        io::ErrorKind::NotFound,
        io::ErrorKind::PermissionDenied,
        io::ErrorKind::AlreadyExists,
        io::ErrorKind::InvalidInput,
        io::ErrorKind::InvalidData,
        io::ErrorKind::WouldBlock,
        io::ErrorKind::TimedOut,
        io::ErrorKind::Unsupported,
        io::ErrorKind::StorageFull,
        io::ErrorKind::OutOfMemory,
        io::ErrorKind::UnexpectedEof,
        io::ErrorKind::WriteZero,
        io::ErrorKind::Interrupted,
        io::ErrorKind::BrokenPipe,
        io::ErrorKind::NotConnected,
        io::ErrorKind::ConnectionRefused,
        io::ErrorKind::AddrNotAvailable,
        io::ErrorKind::DirectoryNotEmpty,
        io::ErrorKind::Other,
    ];
    for &k in KINDS {
        let e = SqueezefsError::Io(io::Error::new(k, "probe"));
        assert_eq!(
            e.to_errno(),
            io_kind_errno(k),
            "POSIX-6: io::ErrorKind::{k:?} must map explicitly"
        );
    }
}

#[test]
fn raw_os_error_still_passes_through_verbatim() {
    for code in [libc::ENOENT, libc::EEXIST, libc::ENOTEMPTY, libc::EPERM] {
        let e = SqueezefsError::Io(io::Error::from_raw_os_error(code));
        assert_eq!(e.to_errno(), code, "an OS errno is never re-derived");
    }
}

// ---------------------------------------------------------------------------
// 2. Corrections — the mappings that were provably wrong.
// ---------------------------------------------------------------------------

/// `io::ErrorKind::StorageFull` is constructed deliberately by the
/// staging tier (`cache/nvme.rs` ×4) and travelled to userspace as
/// **EIO** — the `_ => EIO` fallthrough — so an out-of-space staging
/// write looked like a device failure. It is ENOSPC.
#[test]
fn storage_full_is_enospc_not_eio() {
    let e = SqueezefsError::Io(io::Error::new(io::ErrorKind::StorageFull, "staging full"));
    assert_eq!(e.to_errno(), libc::ENOSPC);
}

/// POSIX-5: `LockFailed` mapped straight to `EAGAIN`, which POSIX
/// reserves for `O_NONBLOCK` — `write(2)`/`ftruncate`/`fsync`/
/// `fallocate` must never return it. The handlers retry with backoff;
/// an escaped `LockFailed` is `EIO`.
#[test]
fn lock_failed_is_eio_not_eagain() {
    let e = SqueezefsError::LockFailed {
        reason: "lock I42 still held after 5s wait budget".into(),
    };
    assert_eq!(e.to_errno(), libc::EIO);
    assert_ne!(e.to_errno(), libc::EAGAIN);
}

// ---------------------------------------------------------------------------
// 3. Text is not wire format.
// ---------------------------------------------------------------------------

/// The headline POSIX-6 law: the exact phrases that USED to steer the
/// errno steer nothing now. (Producers that need those errnos say so
/// structurally — the next section.)
#[test]
fn invalid_operation_text_never_steers_the_errno() {
    for msg in [
        "File already exists",
        "Already exists",
        "the volume exists",
        "Inode table full",
        "No space left on device (os error 28)",
        "Too many links",
        "insufficient capacity",
        "",
    ] {
        let e = SqueezefsError::InvalidOperation(msg.to_string());
        assert_eq!(
            e.to_errno(),
            libc::EINVAL,
            "POSIX-6: message text ({msg:?}) must not be load-bearing"
        );
    }
}

/// …and the structural refusals keep their errno even when the message
/// says nothing recognizable (rephrase-proof, translation-proof).
#[test]
fn structured_refusals_carry_errno_regardless_of_text() {
    assert_eq!(
        SqueezefsError::already_exists("a name is taken").to_errno(),
        libc::EEXIST
    );
    assert_eq!(
        SqueezefsError::no_space("insufficient capacity").to_errno(),
        libc::ENOSPC
    );
    assert_eq!(
        SqueezefsError::too_many_links("link ceiling reached").to_errno(),
        libc::EMLINK
    );
    assert_eq!(
        SqueezefsError::refused(libc::EROFS, "mounted read-only").to_errno(),
        libc::EROFS
    );
    // Display stays human — the message is still the operator's, it is
    // just no longer the errno's.
    assert!(SqueezefsError::already_exists("File already exists")
        .to_string()
        .contains("already exists"));
}

// ---------------------------------------------------------------------------
// 4. The KV refusal surface — typed at the source, not spelled in prose.
// ---------------------------------------------------------------------------

/// The spec's own example, exactly: the KV allocator's ENOSPC refusal
/// reads `"no space: … (ENOSPC)"` — lower-case `n`, so the old
/// `contains("No space")` rule MISSED it and `write(2)` got `EINVAL` on
/// a full metadata volume.
#[test]
fn kv_no_space_is_enospc() {
    let e: SqueezefsError = KvError::NoSpace {
        free: 3,
        reserve: 8,
    }
    .into();
    assert_eq!(e.to_errno(), libc::ENOSPC);
}

/// `setxattr(2)` says E2BIG for a value over the filesystem maximum
/// (EINVAL is its *flags* error). The cap only fires below
/// `XATTR_SIZE_MAX` (small-node volumes) — the kernel screens the rest.
#[test]
fn kv_value_too_large_is_e2big() {
    let e: SqueezefsError = KvError::ValueTooLarge {
        len: 70_000,
        cap: 65_536,
    }
    .into();
    assert_eq!(e.to_errno(), libc::E2BIG);
}

/// The D0 single-writer refusal is EBUSY, not "invalid argument".
#[test]
fn kv_busy_is_ebusy() {
    let e: SqueezefsError = KvError::Busy("another writer holds the volume".into()).into();
    assert_eq!(e.to_errno(), libc::EBUSY);
}

/// Device I/O still passes through untouched, and the remaining typed
/// refusals stay `EINVAL` — the pre-POSIX-6 behavior, preserved.
#[test]
fn kv_io_passes_through_and_the_rest_stay_einval() {
    let io_err: SqueezefsError =
        KvError::Io(SqueezefsError::Io(io::Error::from_raw_os_error(libc::EIO))).into();
    assert_eq!(io_err.to_errno(), libc::EIO);

    for e in [
        KvError::NodeFull {
            needed: 1,
            available: 0,
        },
        KvError::EntryTooLarge {
            len: 1 << 20,
            cap: 1 << 17,
        },
        KvError::PendingFreeFull { pending: 9 },
        KvError::JournalReserveExhausted { needed: 4096 },
    ] {
        let mapped: SqueezefsError = e.into();
        assert_eq!(mapped.to_errno(), libc::EINVAL);
    }
}

// ---------------------------------------------------------------------------
// 5. Producer side — the real backend still delivers EEXIST / EMLINK.
// ---------------------------------------------------------------------------

const VOL_LEN: u64 = 96 * 1024 * 1024;

async fn mutable_volume() -> (
    std::sync::Arc<squeezefs::meta_backend::kv::backend::KvMetaBackend>,
    NamedTempFile,
) {
    let file = NamedTempFile::new().expect("temp volume");
    file.as_file().set_len(VOL_LEN).unwrap();
    format_v3(
        file.path(),
        VOL_LEN,
        &FormatV3Options {
            node_size: 65536,
            journal_len_override: Some(8 * 1024 * 1024),
            force: false,
            full_wipe: false,
            format_config_xattr: None,
        },
    )
    .await
    .unwrap();
    let backend = open_volume_for_mount(file.path().to_str().unwrap())
        .await
        .expect("open_volume_for_mount");
    (backend, file)
}

/// `create` over an existing name: EEXIST — from the type, not from the
/// words "File already exists".
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn duplicate_create_reports_eexist_structurally() {
    let (b, _f) = mutable_volume().await;
    b.create(ROOT_INO, "dup.txt", libc::S_IFREG | 0o644, 0, 0)
        .await
        .expect("first create");
    let err = b
        .create(ROOT_INO, "dup.txt", libc::S_IFREG | 0o644, 0, 0)
        .await
        .expect_err("second create must refuse");
    assert_eq!(err.to_errno(), libc::EEXIST, "create over a name: {err}");
    assert!(
        matches!(err, SqueezefsError::Refused { .. }),
        "the refusal must be structural, not prose: {err:?}"
    );
}

/// The routed link path at the nlink ceiling: EMLINK — likewise
/// structural. (`routed_nlink_adjust` carries the same ceiling as
/// `routed_link_local`, and seeds it in one call instead of 65 000
/// real links.)
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn link_at_the_nlink_ceiling_reports_emlink_structurally() {
    let (b, _f) = mutable_volume().await;
    let f = b
        .create(ROOT_INO, "many.txt", libc::S_IFREG | 0o644, 0, 0)
        .await
        .expect("create");
    let guards: std::sync::Arc<[DlmGuard]> = std::sync::Arc::from(Vec::new());
    b.routed_nlink_adjust(f.ino, 64_999, false, guards.clone())
        .await
        .expect("seed nlink to the ceiling");
    let err = b
        .routed_nlink_adjust(f.ino, 1, false, guards)
        .await
        .expect_err("link past the ceiling must refuse");
    assert_eq!(err.to_errno(), libc::EMLINK, "link at the ceiling: {err}");
    assert!(
        matches!(err, SqueezefsError::Refused { .. }),
        "the refusal must be structural, not prose: {err:?}"
    );
}
