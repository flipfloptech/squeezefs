//! notify kernel.

use std::ffi::OsString;
use std::os::unix::ffi::OsStrExt;

use bincode::Options;
use bytes::{Buf, Bytes};
use futures_util::future::Either;

use crate::helper::get_bincode_config;
use crate::raw::abi::{
    fuse_notify_code, fuse_notify_delete_out, fuse_notify_inval_entry_out,
    fuse_notify_inval_inode_out, fuse_notify_poll_wakeup_out, fuse_notify_prune_out,
    fuse_notify_retrieve_out, fuse_notify_store_out, fuse_out_header, FUSE_NOTIFY_DELETE_OUT_SIZE,
    FUSE_NOTIFY_INVAL_ENTRY_OUT_SIZE, FUSE_NOTIFY_INVAL_INODE_OUT_SIZE,
    FUSE_NOTIFY_POLL_WAKEUP_OUT_SIZE, FUSE_NOTIFY_PRUNE_OUT_SIZE, FUSE_NOTIFY_RETRIEVE_OUT_SIZE,
    FUSE_NOTIFY_STORE_OUT_SIZE, FUSE_OUT_HEADER_SIZE,
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

/// Serialized `FUSE_NOTIFY_PRUNE` frame (uapi 7.45): the out header —
/// `len` covering the nodeid array too, since `fuse_dev_do_write` refuses
/// `oh.len != nbytes` — `fuse_notify_prune_out { count }`, then the
/// nodeids verbatim (`fuse_notify_prune` reads exactly `count × 8` bytes
/// past the struct). ONE encoder for both enqueue forms
/// ([`Notify::prune`] / [`Notify::prune_detached`]), the
/// [`inval_inode_frame`] no-drift law.
#[doc(hidden)]
pub fn prune_frame(inodes: &[u64]) -> Vec<u8> {
    let len = FUSE_OUT_HEADER_SIZE + FUSE_NOTIFY_PRUNE_OUT_SIZE + inodes.len() * 8;
    let out_header = fuse_out_header {
        len: len as u32,
        error: fuse_notify_code::FUSE_NOTIFY_PRUNE as i32,
        unique: 0,
    };
    let prune_out = fuse_notify_prune_out {
        count: inodes.len() as u32,
        _padding: 0,
        _spare: 0,
    };
    let mut data = Vec::with_capacity(len);
    get_bincode_config()
        .serialize_into(&mut data, &out_header)
        .expect("vec size is not enough");
    get_bincode_config()
        .serialize_into(&mut data, &prune_out)
        .expect("vec size is not enough");
    for ino in inodes {
        data.extend_from_slice(&ino.to_le_bytes());
    }
    data
}

/// Whether an out frame (its `fuse_out_header` first) is a daemon-initiated
/// NOTIFICATION rather than a request's reply: a notification carries
/// `unique == 0` and its `fuse_notify_code` in the header's `error` field
/// (positive — every request reply's `error` is `0` or `-errno`). The
/// reply task classifies the kernel's answer by it (PR 13h): a
/// notification the kernel answers `ENOENT` names an inode it does not
/// hold — an expected, counted outcome ([`crate::raw::read_phase::
/// notify_enoent`]), never the "interrupted request" WARN a request
/// reply's `ENOENT` is. A frame shorter than a header is no notification.
///
/// `pub` + `doc(hidden)` (the [`inval_inode_frame`] precedent): the fork's
/// contracts drive it with the shipping encoders' frames.
#[doc(hidden)]
pub fn frame_is_notify(frame: &[u8]) -> bool {
    if frame.len() < FUSE_OUT_HEADER_SIZE {
        return false;
    }
    let error = i32::from_le_bytes([frame[4], frame[5], frame[6], frame[7]]);
    let unique = u64::from_le_bytes([
        frame[8], frame[9], frame[10], frame[11], frame[12], frame[13], frame[14], frame[15],
    ]);
    unique == 0 && error > 0
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

            NotifyKind::Prune { inodes } => Either::Left(prune_frame(inodes)),

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

    /// Ask the kernel to prune the unreferenced dentry aliases of
    /// `inodes` (`FUSE_NOTIFY_PRUNE`, uapi 7.45 — a kernel below it
    /// answers the device write `EINVAL`, which the reply task logs;
    /// callers gate on the negotiated minor). An inode nobody holds open
    /// is evicted and re-instantiated from the daemon's attrs at its next
    /// lookup — how a FUSE_WRITEBACK_CACHE kernel, which owns a cached
    /// regular inode's size/mtime/ctime, adopts a peer mount's change.
    pub async fn prune(mut self, inodes: Vec<u64>) {
        let _ = self.notify(NotifyKind::Prune { inodes }).await;
    }

    /// Synchronous fire-and-forget form of [`Notify::prune`] — the
    /// [`Notify::invalid_inode_detached`] venue law (no task, no runtime;
    /// one `unbounded_send` of the ONE shared encoding).
    pub fn prune_detached(&self, inodes: &[u64]) {
        self.sender.send_detached(Either::Left(prune_frame(inodes)));
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

    /// prune the unreferenced dentry aliases of these inodes (uapi 7.45).
    Prune { inodes: Vec<u64> },

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

    /// `FUSE_NOTIFY_PRUNE` (uapi 7.45): ONE frame — the out header whose
    /// `len` covers the header, `fuse_notify_prune_out` AND the nodeid
    /// array (`fuse_dev_do_write` refuses `oh.len != nbytes`), code 9,
    /// `unique` 0, `count` = the nodeids, then the nodeids verbatim — the
    /// kernel's `fuse_notify_prune` reads exactly `count × 8` bytes past
    /// the struct. The detached and async forms enqueue the identical
    /// frame.
    #[test]
    fn prune_frame_is_one_header_struct_and_nodeid_array() {
        let frame = prune_frame(&[7, 0x1_0000_0000]);
        assert_eq!(
            frame.len(),
            FUSE_OUT_HEADER_SIZE + FUSE_NOTIFY_PRUNE_OUT_SIZE + 16,
            "header + prune_out + two nodeids"
        );
        // out header: len (u32), error (i32 = the notify code), unique (u64)
        assert_eq!(
            u32::from_le_bytes(frame[0..4].try_into().unwrap()) as usize,
            frame.len(),
            "oh.len covers the nodeid array"
        );
        assert_eq!(
            i32::from_le_bytes(frame[4..8].try_into().unwrap()),
            fuse_notify_code::FUSE_NOTIFY_PRUNE as i32
        );
        assert_eq!(u64::from_le_bytes(frame[8..16].try_into().unwrap()), 0);
        // fuse_notify_prune_out: count (u32), padding (u32), spare (u64)
        assert_eq!(u32::from_le_bytes(frame[16..20].try_into().unwrap()), 2);
        assert_eq!(&frame[20..32], &[0u8; 12]);
        assert_eq!(u64::from_le_bytes(frame[32..40].try_into().unwrap()), 7);
        assert_eq!(
            u64::from_le_bytes(frame[40..48].try_into().unwrap()),
            0x1_0000_0000
        );

        let (notify, mut rx) = notify_test_channel();
        notify.prune_detached(&[7, 0x1_0000_0000]);
        assert_eq!(rx.try_next_frame().expect("detached prune frame"), frame);

        let (notify2, mut rx2) = notify_test_channel();
        notify2
            .prune(vec![7, 0x1_0000_0000])
            .now_or_never()
            .expect("the async enqueue never suspends");
        assert_eq!(rx2.try_next_frame().expect("async prune frame"), frame);
    }

    /// **A notification's `ENOENT` is a counted outcome, never the
    /// "interrupted request" WARN** (symmetric PR 13h — the fourth box
    /// pass read 272,071 / 285,461 `may reply interrupted fuse request,
    /// ignore this error No such file or directory` lines per row set,
    /// ≈ 2 × the served-mutation hook's `FUSE_NOTIFY_INVAL_INODE` +
    /// `FUSE_NOTIFY_PRUNE`: the kernel answering "not cached" for an inode
    /// it had already forgotten, logged per call). The classifier is the
    /// out header: every notification carries `unique == 0` and its
    /// notify code in `error` (positive); a request's reply carries its
    /// `unique` and `0` or `-errno`. The reply task's verdict: a
    /// notification's `ENOENT` counts (`fuse3_notify_enoent`), its other
    /// errnos log and never end the task (a notification owes nothing);
    /// a request reply keeps the shipped law — `ENOENT` the interrupted
    /// WARN, anything else fatal. RED on the fork before PR 13h: every
    /// `ENOENT` read as an interrupted request.
    #[test]
    fn a_notifications_enoent_is_counted_never_the_interrupted_request_warn() {
        use crate::raw::session::{reply_write_verdict, ReplyWriteVerdict};
        use std::io::ErrorKind;
        // The classifier over the shipping encoders' frames.
        assert!(frame_is_notify(&inval_inode_frame(772, 0, 0)));
        assert!(frame_is_notify(&inval_inode_frame(9, -1, 0)));
        assert!(frame_is_notify(&prune_frame(&[772])));
        assert!(frame_is_notify(&prune_frame(&[])));
        // A request's reply: `unique` set, `error` 0 (success) or `-errno`.
        let mut reply_ok = vec![0u8; FUSE_OUT_HEADER_SIZE];
        reply_ok[0..4].copy_from_slice(&(FUSE_OUT_HEADER_SIZE as u32).to_le_bytes());
        reply_ok[8..16].copy_from_slice(&42u64.to_le_bytes());
        assert!(!frame_is_notify(&reply_ok));
        let mut reply_err = reply_ok.clone();
        reply_err[4..8].copy_from_slice(&(-libc::ENOENT).to_le_bytes());
        assert!(!frame_is_notify(&reply_err));
        // A notify code with a unique is no notification (the kernel's
        // request replies never carry a positive error); a short frame is
        // no notification.
        let mut odd = reply_ok.clone();
        odd[4..8].copy_from_slice(&(fuse_notify_code::FUSE_NOTIFY_PRUNE as i32).to_le_bytes());
        assert!(!frame_is_notify(&odd));
        assert!(!frame_is_notify(&reply_ok[..8]));
        // The reply task's verdict.
        assert_eq!(
            reply_write_verdict(true, ErrorKind::NotFound),
            ReplyWriteVerdict::NotifyEnoent,
            "a notification's ENOENT is COUNTED — the kernel does not hold the inode"
        );
        assert_eq!(
            reply_write_verdict(true, ErrorKind::InvalidInput),
            ReplyWriteVerdict::NotifyFailed,
            "a notification's other errno is logged and never ends the reply task"
        );
        assert_eq!(
            reply_write_verdict(false, ErrorKind::NotFound),
            ReplyWriteVerdict::InterruptedRequest,
            "a request reply's ENOENT stays the interrupted-request WARN"
        );
        assert_eq!(
            reply_write_verdict(false, ErrorKind::BrokenPipe),
            ReplyWriteVerdict::Fatal,
            "a request reply's other errno ends the reply task, as shipped"
        );
        // The counter moves once per counted outcome.
        let before = crate::raw::read_phase::notify_enoent();
        crate::raw::read_phase::note_notify_enoent();
        assert_eq!(crate::raw::read_phase::notify_enoent(), before + 1);
    }
}
