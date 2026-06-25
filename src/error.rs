use std::io;
use thiserror::Error;

#[derive(Error, Debug)]
pub enum SqueezefsError {
    #[error("I/O error: {0}")]
    Io(#[from] io::Error),

    #[error("Redis / Garnet error: {0}")]
    Redis(#[from] redis::RedisError),

    #[error("S3 error: {0}")]
    S3(String),

    #[error("Lock acquisition failed: {reason}")]
    LockFailed { reason: String },

    #[error("Lock expired or invalid fencing token: {token} (expected >= {expected})")]
    FencingTokenExpired { token: u64, expected: u64 },

    #[error("Invalid operation: {0}")]
    InvalidOperation(String),

    #[error("GPU Direct Storage error: {0}")]
    GdsError(String),

    #[error("System RAM cache overflow")]
    CacheOverflow,

    #[error("Timeout error")]
    Timeout,
}

impl SqueezefsError {
    pub fn to_errno(&self) -> libc::c_int {
        match self {
            SqueezefsError::Io(e) => {
                if let Some(code) = e.raw_os_error() {
                    code
                } else {
                    match e.kind() {
                        io::ErrorKind::NotFound => libc::ENOENT,
                        io::ErrorKind::PermissionDenied => libc::EACCES,
                        io::ErrorKind::AlreadyExists => libc::EEXIST,
                        io::ErrorKind::InvalidInput => libc::EINVAL,
                        io::ErrorKind::WouldBlock => libc::EWOULDBLOCK,
                        io::ErrorKind::TimedOut => libc::ETIMEDOUT,
                        io::ErrorKind::Unsupported => libc::ENOTSUP,
                        _ => libc::EIO,
                    }
                }
            }
            SqueezefsError::Redis(_) => libc::ECOMM,
            SqueezefsError::S3(_) => libc::EIO,
            SqueezefsError::LockFailed { .. } => libc::EAGAIN,
            SqueezefsError::FencingTokenExpired { .. } => libc::EACCES,
            SqueezefsError::InvalidOperation(_) => libc::EINVAL,
            SqueezefsError::GdsError(_) => libc::EIO,
            SqueezefsError::CacheOverflow => libc::ENOMEM,
            SqueezefsError::Timeout => libc::ETIMEDOUT,
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
