//! notify kernel.

use std::ffi::OsString;
use std::os::unix::ffi::OsStrExt;

use bincode::Options;
use bytes::{Buf, Bytes};
use futures_util::future::Either;

use crate::helper::get_bincode_config;
use crate::raw::abi::{
    fuse_notify_code, fuse_notify_delete_out, fuse_notify_inval_entry_out,
    fuse_notify_inval_inode_out, fuse_notify_poll_wakeup_out, fuse_notify_retrieve_out,
    fuse_notify_store_out, fuse_out_header, FUSE_NOTIFY_DELETE_OUT_SIZE,
    FUSE_NOTIFY_INVAL_ENTRY_OUT_SIZE, FUSE_NOTIFY_INVAL_INODE_OUT_SIZE,
    FUSE_NOTIFY_POLL_WAKEUP_OUT_SIZE, FUSE_NOTIFY_RETRIEVE_OUT_SIZE, FUSE_NOTIFY_STORE_OUT_SIZE,
    FUSE_OUT_HEADER_SIZE,
};
use crate::raw::session::ReplyTx;

/// Serialized `FUSE_NOTIFY_INVAL_INODE` frame — the exact bytes
/// [`Notify::invalid_inode`] enqueues, factored out so the synchronous
/// device-write path ([`crate::raw::connection::FuseConnection::
/// notify_inval_inode_sync`], generic/451) shares ONE encoding with the
/// async enqueue path and the two can never drift.
///
/// `pub` + `doc(hidden)` (the bench-seam precedent —
/// [`crate::get_bincode_config`]): the daemon's inval venue pins
/// (`tests/ipc_inval_venue_tests.rs` in the root crate) assert the
/// PRODUCTION hook delivers exactly this frame, so the expected bytes
/// must come from the shipping encoder, not a lookalike.
#[doc(hidden)]
pub fn inval_inode_frame(inode: u64, offset: i64, len: i64) -> Vec<u8> {
    let out_header = fuse_out_header {
        len: (FUSE_OUT_HEADER_SIZE + FUSE_NOTIFY_INVAL_INODE_OUT_SIZE) as u32,
        error: fuse_notify_code::FUSE_NOTIFY_INVAL_INODE as i32,
        unique: 0,
    };
    let invalid_inode_out = fuse_notify_inval_inode_out {
        ino: inode,
        off: offset,
        len,
    };
    let mut data = Vec::with_capacity(FUSE_OUT_HEADER_SIZE + FUSE_NOTIFY_INVAL_INODE_OUT_SIZE);
    get_bincode_config()
        .serialize_into(&mut data, &out_header)
        .expect("vec size is not enough");
    get_bincode_config()
        .serialize_into(&mut data, &invalid_inode_out)
        .expect("vec size is not enough");
    data
}

#[derive(Debug, Clone)]
/// notify kernel there are something need to handle.
pub struct Notify {
    /// Daemon-initiated notifications carry no request and owe no reply
    /// — they ride the classical device write (FUSE-2's `ReplySlot`
    /// routing sends them there by construction).
    sender: ReplyTx,
}

impl Notify {
    pub(crate) fn new(sender: ReplyTx) -> Self {
        Self { sender }
    }

    /// notify kernel there are something need to handle. If notify failed, the `kind` will be
    /// return in `Err`.
    async fn notify(&mut self, kind: NotifyKind) -> Result<(), NotifyKind> {
        let data = match &kind {
            NotifyKind::Wakeup { kh } => {
                let out_header = fuse_out_header {
                    len: (FUSE_OUT_HEADER_SIZE + FUSE_NOTIFY_POLL_WAKEUP_OUT_SIZE) as u32,
                    error: fuse_notify_code::FUSE_POLL as i32,
                    unique: 0,
                };

                let wakeup_out = fuse_notify_poll_wakeup_out { kh: *kh };

                let mut data =
                    Vec::with_capacity(FUSE_OUT_HEADER_SIZE + FUSE_NOTIFY_POLL_WAKEUP_OUT_SIZE);

                get_bincode_config()
                    .serialize_into(&mut data, &out_header)
                    .expect("vec size is not enough");
                get_bincode_config()
                    .serialize_into(&mut data, &wakeup_out)
                    .expect("vec size is not enough");

                Either::Left(data)
            }

            NotifyKind::InvalidInode { inode, offset, len } => {
                // ONE encoding, shared with the synchronous device-write
                // path (generic/451) — see `inval_inode_frame`.
                Either::Left(inval_inode_frame(*inode, *offset, *len))
            }

            NotifyKind::InvalidEntry { parent, name } => {
                let out_header = fuse_out_header {
                    len: (FUSE_OUT_HEADER_SIZE + FUSE_NOTIFY_INVAL_ENTRY_OUT_SIZE) as u32,
                    error: fuse_notify_code::FUSE_NOTIFY_INVAL_ENTRY as i32,
                    unique: 0,
                };

                let invalid_entry_out = fuse_notify_inval_entry_out {
                    parent: *parent,
                    namelen: name.len() as _,
                    _padding: 0,
                };

                let mut data =
                    Vec::with_capacity(FUSE_OUT_HEADER_SIZE + FUSE_NOTIFY_INVAL_ENTRY_OUT_SIZE);

                get_bincode_config()
                    .serialize_into(&mut data, &out_header)
                    .expect("vec size is not enough");
                get_bincode_config()
                    .serialize_into(&mut data, &invalid_entry_out)
                    .expect("vec size is not enough");

                // TODO should I add null at the end?

                Either::Right((data, Bytes::copy_from_slice(name.as_bytes()), None))
            }

            NotifyKind::Delete {
                parent,
                child,
                name,
            } => {
                let out_header = fuse_out_header {
                    len: (FUSE_OUT_HEADER_SIZE + FUSE_NOTIFY_DELETE_OUT_SIZE) as u32,
                    error: fuse_notify_code::FUSE_NOTIFY_DELETE as i32,
                    unique: 0,
                };

                let delete_out = fuse_notify_delete_out {
                    parent: *parent,
                    child: *child,
                    namelen: name.len() as _,
                    _padding: 0,
                };

                let mut data =
                    Vec::with_capacity(FUSE_OUT_HEADER_SIZE + FUSE_NOTIFY_DELETE_OUT_SIZE);

                get_bincode_config()
                    .serialize_into(&mut data, &out_header)
                    .expect("vec size is not enough");
                get_bincode_config()
                    .serialize_into(&mut data, &delete_out)
                    .expect("vec size is not enough");

                // TODO should I add null at the end?

                Either::Right((data, Bytes::copy_from_slice(name.as_bytes()), None))
            }

            NotifyKind::Store {
                inode,
                offset,
                data,
            } => {
                let out_header = fuse_out_header {
                    len: (FUSE_OUT_HEADER_SIZE + FUSE_NOTIFY_STORE_OUT_SIZE) as u32,
                    error: fuse_notify_code::FUSE_NOTIFY_STORE as i32,
                    unique: 0,
                };

                let store_out = fuse_notify_store_out {
                    nodeid: *inode,
                    offset: *offset,
                    size: data.len() as _,
                    _padding: 0,
                };

                let mut data_buf =
                    Vec::with_capacity(FUSE_OUT_HEADER_SIZE + FUSE_NOTIFY_STORE_OUT_SIZE);

                get_bincode_config()
                    .serialize_into(&mut data_buf, &out_header)
                    .expect("vec size is not enough");
                get_bincode_config()
                    .serialize_into(&mut data_buf, &store_out)
                    .expect("vec size is not enough");

                Either::Right((data_buf, data.clone(), None))
            }

            NotifyKind::Retrieve {
                notify_unique,
                inode,
                offset,
                size,
            } => {
                let out_header = fuse_out_header {
                    len: (FUSE_OUT_HEADER_SIZE + FUSE_NOTIFY_RETRIEVE_OUT_SIZE) as u32,
                    error: fuse_notify_code::FUSE_NOTIFY_RETRIEVE as i32,
                    unique: 0,
                };

                let retrieve_out = fuse_notify_retrieve_out {
                    notify_unique: *notify_unique,
                    nodeid: *inode,
                    offset: *offset,
                    size: *size,
                    _padding: 0,
                };

                let mut data =
                    Vec::with_capacity(FUSE_OUT_HEADER_SIZE + FUSE_NOTIFY_RETRIEVE_OUT_SIZE);

                get_bincode_config()
                    .serialize_into(&mut data, &out_header)
                    .expect("vec size is not enough");
                get_bincode_config()
                    .serialize_into(&mut data, &retrieve_out)
                    .expect("vec size is not enough");

                Either::Left(data)
            }
        };

        self.sender.send(data).await.or(Err(kind))
    }

    /// try to notify kernel the IO is ready, kernel can wakeup the waiting program.
    pub async fn wakeup(mut self, kh: u64) {
        let _ = self.notify(NotifyKind::Wakeup { kh }).await;
    }

    /// try to notify the cache invalidation about an inode.
    pub async fn invalid_inode(mut self, inode: u64, offset: i64, len: i64) {
        let _ = self
            .notify(NotifyKind::InvalidInode { inode, offset, len })
            .await;
    }

    /// Synchronous fire-and-forget `FUSE_NOTIFY_INVAL_INODE` enqueue —
    /// the W1 interception invalidator's venue-free form (2026-08-05, the
    /// il write residual): [`Notify::invalid_inode`]'s `.await` chain
    /// never suspends (`ReplyTx::send` is one `unbounded_send`), so the
    /// per-fire task the daemon used to spawn onto its multi-thread
    /// runtime's **global inject queue** purely to enter async context
    /// was pure venue tax — the same term the 2026-07-26 handoff-economy
    /// fix banned for ring handoffs. This form runs the same ONE shared
    /// encoding ([`inval_inode_frame`] — the generic/451 no-drift law)
    /// into the same reply channel from the caller's context: any thread,
    /// no runtime required, no clone (`&self`). Delivery stays
    /// fire-and-forget by contract (§5.6.2 W1's bounded-staleness
    /// adjudication lives at the daemon hook); a dead reply task
    /// (teardown) drops the frame.
    ///
    pub fn invalid_inode_detached(&self, inode: u64, offset: i64, len: i64) {
        self.sender
            .send_detached(Either::Left(inval_inode_frame(inode, offset, len)));
    }

    /// try to notify the invalidation about a directory entry.
    pub async fn invalid_entry(mut self, parent: u64, name: OsString) {
        let _ = self.notify(NotifyKind::InvalidEntry { parent, name }).await;
    }

    /// try to notify a directory entry has been deleted.
    pub async fn delete(mut self, parent: u64, child: u64, name: OsString) {
        let _ = self
            .notify(NotifyKind::Delete {
                parent,
                child,
                name,
            })
            .await;
    }

    /// try to push the data in an inode for updating the kernel cache.
    pub async fn store(mut self, inode: u64, offset: u64, mut data: impl Buf) {
        let _ = self
            .notify(NotifyKind::Store {
                inode,
                offset,
                data: data.copy_to_bytes(data.remaining()),
            })
            .await;
    }

    /// try to retrieve data in an inode from the kernel cache.
    pub async fn retrieve(mut self, notify_unique: u64, inode: u64, offset: u64, size: u32) {
        let _ = self
            .notify(NotifyKind::Retrieve {
                notify_unique,
                inode,
                offset,
                size,
            })
            .await;
    }
}

#[derive(Debug)]
/// the kind of notify.
enum NotifyKind {
    /// notify the IO is ready.
    Wakeup { kh: u64 },

    // TODO need check is right or not
    /// notify the cache invalidation about an inode.
    InvalidInode { inode: u64, offset: i64, len: i64 },

    /// notify the invalidation about a directory entry.
    InvalidEntry { parent: u64, name: OsString },

    /// notify a directory entry has been deleted.
    Delete {
        parent: u64,
        child: u64,
        name: OsString,
    },

    /// push the data in an inode for updating the kernel cache.
    Store {
        inode: u64,
        offset: u64,
        data: Bytes,
    },

    /// retrieve data in an inode from the kernel cache.
    Retrieve {
        notify_unique: u64,
        inode: u64,
        offset: u64,
        size: u32,
    },
}

/// `pub` + `doc(hidden)` test seam (the bench-seam precedent —
/// [`crate::get_bincode_config`]): a [`Notify`] over an inspectable
/// reply channel, for the daemon's inval **venue pins**
/// (`tests/ipc_inval_venue_tests.rs` in the root crate). The pins'
/// whole subject is *synchronous, runtime-free delivery*, which only a
/// receiver polled without any executor can witness — a real session's
/// reply task cannot.
#[doc(hidden)]
pub fn notify_test_channel() -> (Notify, NotifyTestRx) {
    let (tx, rx) = futures_channel::mpsc::unbounded();
    (Notify::new(ReplyTx::no_reply(tx)), NotifyTestRx { rx })
}

/// The receiving half of [`notify_test_channel`]: synchronous,
/// executor-free frame pops.
#[doc(hidden)]
pub struct NotifyTestRx {
    rx: futures_channel::mpsc::UnboundedReceiver<crate::raw::FuseReply>,
}

impl NotifyTestRx {
    /// Pop the next enqueued notification frame's header bytes.
    /// `None` = nothing is enqueued **right now** — i.e. for the venue
    /// pins, delivery was not synchronous.
    pub fn try_next_frame(&mut self) -> Option<Vec<u8>> {
        match self.rx.try_recv() {
            Ok(reply) => Some(match reply.data {
                Either::Left(header) => header,
                Either::Right((header, _body, _backing)) => header,
            }),
            Err(_) => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures_util::FutureExt;

    /// The venue-free form's whole contract: called from a plain OS
    /// thread with NO runtime anywhere, the frame is enqueued by the
    /// time the call returns, byte-identical to the ONE shared encoder
    /// (generic/451's no-drift law).
    #[test]
    fn invalid_inode_detached_enqueues_synchronously_without_a_runtime() {
        let (notify, mut rx) = notify_test_channel();
        std::thread::spawn(move || {
            notify.invalid_inode_detached(9, -1, 0);
        })
        .join()
        .expect("no runtime is required on the firing thread");
        assert_eq!(
            rx.try_next_frame()
                .expect("the frame is enqueued when the call returns"),
            inval_inode_frame(9, -1, 0),
        );
    }

    /// The premise the detached form rests on, pinned: the ASYNC form's
    /// `.await` chain never suspends (`ReplyTx::send` is one
    /// `unbounded_send`), so one poll with a noop waker completes it —
    /// and both forms enqueue the identical frame. If someone ever makes
    /// the async enqueue genuinely suspend (a bounded channel, an
    /// intermediate hop), `now_or_never` fails and the detached form
    /// must be re-adjudicated rather than silently diverging.
    #[test]
    fn detached_and_async_forms_enqueue_identical_frames() {
        let (notify, mut rx) = notify_test_channel();
        notify.invalid_inode_detached(11, 0, -1);
        let detached = rx.try_next_frame().expect("detached frame");

        let (notify2, mut rx2) = notify_test_channel();
        notify2
            .invalid_inode(11, 0, -1)
            .now_or_never()
            .expect("the async enqueue must complete in one poll — it never suspends");
        let asynchronous = rx2.try_next_frame().expect("async frame");

        assert_eq!(detached, asynchronous, "ONE shared encoding, two forms");
    }
}
