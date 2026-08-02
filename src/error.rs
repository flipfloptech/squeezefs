use std::io;
use thiserror::Error;

#[derive(Debug, Error)]
#[error("Mock Metadata Error")]
pub struct MockRedisError;

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
            SqueezefsError::WriterGuardFenced => libc::EIO,
            SqueezefsError::IndirectMapFormat { .. } => libc::EIO,
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
