use std::io;
use thiserror::Error;

#[derive(Debug, Error)]
#[error("Mock Metadata Error")]
pub struct MockRedisError;

/// The class of a symmetric-plane RETRYABLE refusal
/// ([`SqueezefsError::Retryable`]) — the typed word the cross-owner
/// classifiers `matches!` on (PR 13 review round 1, Issue 7).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RefusalClass {
    /// The commit door refused a slot ANOTHER appender leases
    /// (`KvError::SlotBusy { slot, holder, g }`): the slot moved between the
    /// plan and the apply — re-resolve and re-dispatch (defects 29/30/35).
    SlotMoved { slot: u32, holder: u32 },
    /// A served travelling guard refused a key whose slot the serving mount
    /// does not lease: the initiator's holder view is stale — re-resolve
    /// through tree 0 and ship again (defect 11).
    StaleHolderView,
    /// A record-level verb on an object whose slot appender `holder`
    /// leases, from a mount that knows no endpoint for it (PR 13b — the
    /// join ladder has not published one, or the holder is dead until
    /// PR 10's recovery re-leases its slots): the ship cannot travel yet —
    /// retry (`meta_backend::record_ship`).
    HolderUnreachable { holder: u32 },
    /// A token holder's membership screen refused this member (its lease
    /// is not live at the holder's owner — a manager failover's
    /// re-assertion window, PR 13b §4.4ag) and no grant was re-asserted
    /// within the park bound: retry (`token_plane::TokenReaderPlane::call`).
    MembershipPending,
}

impl RefusalClass {
    /// The wire word (`WireError::class`): 0 = none, then this table.
    pub fn to_wire(self) -> u8 {
        match self {
            RefusalClass::SlotMoved { .. } => 1,
            RefusalClass::StaleHolderView => 2,
            RefusalClass::HolderUnreachable { .. } => 3,
            RefusalClass::MembershipPending => 4,
        }
    }

    /// The inverse of [`Self::to_wire`] — total: an unknown word is `None`
    /// (an unclassed refusal, never a guessed class). The slot and holder do
    /// not travel (the client re-resolves through tree 0 either way).
    pub fn from_wire(word: u8) -> Option<Self> {
        match word {
            1 => Some(RefusalClass::SlotMoved { slot: 0, holder: 0 }),
            2 => Some(RefusalClass::StaleHolderView),
            3 => Some(RefusalClass::HolderUnreachable { holder: 0 }),
            4 => Some(RefusalClass::MembershipPending),
            _ => None,
        }
    }
}

/// Why a shipped publish did not land ([`SqueezefsError::PublishFailure`]).
/// Exactly one class is outcome-UNKNOWN; every other class means the
/// owner applied NOTHING (or said exactly what it did).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PublishFailureClass {
    /// The transport failed past the resend ladder against an owner that
    /// MAY have applied the frame — the true sent-then-lost ambiguity.
    TransportOutcomeUnknown,
    /// The owner refused the whole FRAME with this wire status (schema,
    /// malformed, the pack-group refusals): nothing was applied.
    FrameRefused(u16),
    /// The owner refused THIS call with this `PUBLISH_*` status before
    /// executing it: nothing was applied.
    CallRefused(u16),
    /// A protocol violation (an unencodable frame, an undecodable reply, a
    /// schema or outcome-count mismatch): unreachable on a KD-7 same-commit
    /// fleet, classed KNOWN by design-small-file-packing §5.3.
    Protocol,
}

#[derive(Error, Debug)]
pub enum SqueezefsError {
    #[error("I/O error: {0}")]
    Io(#[from] io::Error),

    #[error("Metadata Backend error: {0}")]
    Redis(#[from] MockRedisError),

    #[error("Lock acquisition failed: {reason}")]
    LockFailed { reason: String },

    #[error("Lock expired or invalid fencing token: {token} (expected >= {expected})")]
    FencingTokenExpired { token: u64, expected: u64 },

    /// A refusal whose POSIX errno is **EINVAL** — the generic "this
    /// operation is not valid here" class.
    ///
    /// POSIX-6: the message is prose for the operator, NEVER wire
    /// format. `to_errno` no longer reads it. A refusal that owes the
    /// caller a specific errno must say so structurally
    /// ([`SqueezefsError::Refused`] and its named constructors), because
    /// the moment an application depends on a substring rule the phrasing
    /// can never be changed again.
    #[error("Invalid operation: {0}")]
    InvalidOperation(String),

    /// A refusal that carries its POSIX errno **structurally** (POSIX-6).
    ///
    /// Mint it through the named constructors — [`Self::already_exists`],
    /// [`Self::no_space`], [`Self::too_many_links`], [`Self::too_large`],
    /// [`Self::busy`] — or [`Self::refused`] for a one-off errno. The
    /// message is free to be rephrased, translated, or enriched without
    /// changing a single caller's `errno`.
    #[error("{msg}")]
    Refused { errno: libc::c_int, msg: String },

    /// RES-6 (pre-RC engineering spec §7): this mount's D0 writer claim
    /// was fenced / fail-stopped, so the DATA plane is closed — a fenced
    /// zombie's DMA can land on offsets the successor writer has
    /// replayed and reallocated. Classified as a fence drop by
    /// [`crate::write_pipeline::pipeline_disposition`]: custody is
    /// discarded (the W5 law — publish nothing, free nothing, successor
    /// accounting owns it), never retried.
    #[error(
        "writer guard FENCED: this mount's D0 claim was lost or fail-stopped; the data \
         plane is closed permanently (remount required) — successor accounting owns \
         these offsets"
    )]
    WriterGuardFenced,

    #[error(
        "indirect block map: unsupported on-disk encoding ({detail}); \
         pre-beta or foreign blob — reformat required (no backwards compatibility)"
    )]
    IndirectMapFormat { detail: String },

    /// DLM S6: the membership lease this member presented is **not
    /// custody** (evicted, swept past the owner's TTL, or granted by a
    /// previous owner) — the wire's `RPC_MEMBERSHIP_UNKNOWN_LEASE` class,
    /// carried structurally because the member's renewal ladder keys on it:
    /// the correct response is self-fence (purge) then re-join fresh,
    /// never a retry (`membership::member_renewal_tick`).
    #[error("membership lease is not custody: {0}")]
    MembershipLeaseNotCustody(String),

    /// A shipped publish (DLM S9's layout-publish lane) did not land, with
    /// its outcome CLASS carried structurally (design-small-file-packing
    /// §5.3 — the lane's first typed failure class): a co-writer's pack
    /// release keys `Known`/`Unknown` on it, and the pack-group refusal
    /// latch keys on the wire STATUS — never on the message, which stays
    /// prose for logs.
    #[error("publish failed ({class:?}): {msg}")]
    PublishFailure {
        class: PublishFailureClass,
        msg: String,
    },

    /// **A RETRYABLE refusal of the symmetric plane, classed** (PR 13 review
    /// round 1, Issue 7): the decision "the slot moved — re-dispatch / leave
    /// the intent open" versus "a device error — fail-stop through the S3.5
    /// lattice" is made on [`RefusalClass`], never on the message text
    /// (`crossvol_tx::refusal_class`). Minted at the ONE `KvError::SlotBusy`
    /// conversion (`kv/mod.rs`) and at the served travelling-guard's
    /// stale-holder refusals; carried across the S8 wire as
    /// [`crate::meta_ship::WireError::class`] (a typed word, the
    /// `STATUS_DEFERRED` precedent) and rebuilt into this variant at the
    /// client. Presents `EAGAIN` (the class every caller retries).
    #[error("{msg}")]
    Retryable { class: RefusalClass, msg: String },

    #[error("GPU Direct Storage error: {0}")]
    GdsError(String),

    #[error("System RAM cache overflow")]
    CacheOverflow,

    #[error("Timeout error")]
    Timeout,
}

impl SqueezefsError {
    /// A refusal carrying `errno` verbatim (POSIX-6). Prefer the named
    /// constructors below; this exists for one-off errnos.
    pub fn refused(errno: libc::c_int, msg: impl Into<String>) -> Self {
        SqueezefsError::Refused {
            errno,
            msg: msg.into(),
        }
    }

    /// `EEXIST` — the name is taken (create/link/rename-NOREPLACE).
    pub fn already_exists(msg: impl Into<String>) -> Self {
        Self::refused(libc::EEXIST, msg)
    }

    /// `ENOSPC` — the store cannot make room for this operation.
    pub fn no_space(msg: impl Into<String>) -> Self {
        Self::refused(libc::ENOSPC, msg)
    }

    /// `EMLINK` — the link ceiling for this inode is reached.
    pub fn too_many_links(msg: impl Into<String>) -> Self {
        Self::refused(libc::EMLINK, msg)
    }

    /// `E2BIG` — a value exceeds the filesystem maximum (`setxattr(2)`'s
    /// documented errno for an oversized value; `EINVAL` is its *flags*
    /// error).
    pub fn too_large(msg: impl Into<String>) -> Self {
        Self::refused(libc::E2BIG, msg)
    }

    /// `EBUSY` — the resource is held by another owner (the D0
    /// single-writer mount guard's class).
    pub fn busy(msg: impl Into<String>) -> Self {
        Self::refused(libc::EBUSY, msg)
    }

    /// A classed retryable refusal (`EAGAIN`) — see [`Self::Retryable`].
    pub fn retryable(class: RefusalClass, msg: impl Into<String>) -> Self {
        SqueezefsError::Retryable {
            class,
            msg: msg.into(),
        }
    }

    /// The refusal's class, if it is a classed retryable one.
    pub fn refusal_class(&self) -> Option<RefusalClass> {
        match self {
            SqueezefsError::Retryable { class, .. } => Some(*class),
            _ => None,
        }
    }

    /// The POSIX errno this error presents to userspace.
    ///
    /// **POSIX-6 (one-way door, pinned by `tests/posix_errno_tests.rs`):**
    /// the mapping is *structural and total*. No branch reads an error
    /// message; every variant has an explicit ruling; the only
    /// catch-all is [`io::ErrorKind`]'s (the type is `#[non_exhaustive]`,
    /// so one is unavoidable) and it is `EIO` — the POSIX-safe answer
    /// for "the filesystem could not do it and cannot say why".
    pub fn to_errno(&self) -> libc::c_int {
        match self {
            // A real OS errno is never re-derived — it IS the answer.
            SqueezefsError::Io(e) => match e.raw_os_error() {
                Some(code) => code,
                None => Self::io_kind_errno(e.kind()),
            },
            SqueezefsError::Redis(_) => libc::ECOMM,
            // POSIX-5: `EAGAIN` is reserved for `O_NONBLOCK`, so a lost
            // lease wait must not wear it — `write(2)`/`ftruncate`/
            // `fsync`/`fallocate` retry with backoff (see
            // `fuse_client::acquire_write_lease`) and an escaped
            // `LockFailed` is the exhausted case: `EIO`.
            SqueezefsError::LockFailed { .. } => libc::EIO,
            SqueezefsError::FencingTokenExpired { .. } => libc::EIO,
            // The message is prose, not wire format (POSIX-6).
            SqueezefsError::InvalidOperation(_) => libc::EINVAL,
            SqueezefsError::Refused { errno, .. } => *errno,
            // The symmetric plane's classed retry: every caller retries
            // (the cross-owner arm re-dispatches, the roll-forward
            // cadence completes the intent, the application retries).
            SqueezefsError::Retryable { .. } => libc::EAGAIN,
            SqueezefsError::WriterGuardFenced => libc::EIO,
            SqueezefsError::IndirectMapFormat { .. } => libc::EIO,
            // A fail-stopped lease reaching a data path is the same class
            // as a fenced writer guard: the I/O must not proceed.
            SqueezefsError::MembershipLeaseNotCustody(_) => libc::EIO,
            // A publish the authority did not land is "the filesystem
            // could not do it" — the class is for the lane's own arms,
            // never a userspace contract (before it was typed this face
            // read EINVAL through `InvalidOperation`, a refusal shape no
            // application could act on).
            SqueezefsError::PublishFailure { .. } => libc::EIO,
            SqueezefsError::GdsError(_) => libc::EIO,
            SqueezefsError::CacheOverflow => libc::ENOMEM,
            SqueezefsError::Timeout => libc::ETIMEDOUT,
        }
    }

    /// The [`io::ErrorKind`] table for errors minted in-process (no
    /// `raw_os_error`). Every kind the tree constructs has a row; the
    /// rest fall to `EIO` deliberately.
    fn io_kind_errno(kind: io::ErrorKind) -> libc::c_int {
        match kind {
            io::ErrorKind::NotFound => libc::ENOENT,
            io::ErrorKind::PermissionDenied => libc::EACCES,
            io::ErrorKind::AlreadyExists => libc::EEXIST,
            io::ErrorKind::InvalidInput => libc::EINVAL,
            io::ErrorKind::WouldBlock => libc::EWOULDBLOCK,
            io::ErrorKind::TimedOut => libc::ETIMEDOUT,
            io::ErrorKind::Unsupported => libc::ENOTSUP,
            // POSIX-6 correction: the staging tier mints `StorageFull`
            // deliberately (`cache/nvme.rs`) and it used to reach
            // userspace as EIO — an out-of-space condition reported as a
            // device failure.
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
            // Deliberate `EIO`: corruption (`InvalidData`), short device
            // I/O (`UnexpectedEof`, `WriteZero`), transport loss
            // (`BrokenPipe`, `NotConnected`, the `Connection*`/`Addr*`
            // network family — a filesystem syscall must never surface a
            // socket errno), `Other`, and every kind a future std adds.
            _ => libc::EIO,
        }
    }
}

pub type Result<T> = std::result::Result<T, SqueezefsError>;

#[cfg(test)]
mod tests {
    use super::*;
    use std::io;

    #[test]
    fn test_to_errno_mapping() {
        let err_not_found =
            SqueezefsError::Io(io::Error::new(io::ErrorKind::NotFound, "not found"));
        assert_eq!(err_not_found.to_errno(), libc::ENOENT);

        let err_permission =
            SqueezefsError::Io(io::Error::new(io::ErrorKind::PermissionDenied, "denied"));
        assert_eq!(err_permission.to_errno(), libc::EACCES);

        let err_other = SqueezefsError::Io(io::Error::other("other"));
        assert_eq!(err_other.to_errno(), libc::EIO);
    }
}
