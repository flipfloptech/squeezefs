pub mod tiering;
#![allow(
    clippy::type_complexity,
    clippy::too_many_arguments,
    clippy::redundant_closure
)]

pub mod backend;
pub mod cache;
pub mod config_ops;
pub mod crypto_compress;
pub mod dlm;
pub mod error;
pub mod fuse_client;
pub mod p2p;
pub mod recovery;
pub mod routing;
pub mod nvmeof;

#[macro_export]
macro_rules! coz_progress {
    ($name:expr) => {
        #[cfg(feature = "coz-on")]
        coz::progress!($name);
    };
    () => {
        #[cfg(feature = "coz-on")]
        coz::progress!();
    };
}
