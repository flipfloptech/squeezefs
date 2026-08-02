//! The per-queue fill table: CID-keyed scatter accumulation + the
//! completion laws shared by every area backend (sim AND the real ring
//! driver — design §4.3/§4.4).
//!
//! Laws carried over verbatim from the Z1 reader (the contract suite pins
//! them on this engine too):
//! * a C2HData span for an unknown CID, or exceeding the command length,
//!   is a framing violation (`Err` — the driver counts + poisons);
//! * SUCCESS on a non-LAST C2HData is a framing violation;
//! * completion (SUCCESS elision or CapsuleResp status 0) must have
//!   landed EXACTLY the command's bytes — short data + success status is
//!   a framing violation (counted here, op fails; never partial data);
//! * a nonzero CapsuleResp status fails the OP, never the session.
//!
//! The table is a std `Mutex` (not tokio): its critical sections are
//! pointer pushes and map ops with no await inside, and the real ring
//! driver is a plain thread. Requesters await their oneshot outside it.

use super::area::AreaSlice;
use super::pdu;
use super::pdu_stream::ParseEvent;
use crate::error::{Result, SqueezefsError};
use std::collections::HashMap;
use std::sync::atomic::Ordering;
use std::sync::Mutex;
use tokio::sync::oneshot;

fn io_err(msg: String) -> SqueezefsError {
    SqueezefsError::Io(std::io::Error::other(msg))
}

/// One completed fill: the command's payload as refcounted area spans.
/// Dropping it releases every chunk ref (the refill discipline's last
/// edge); [`ZcrxFill::gather_into`] is the ONE priced completion pass.
pub struct ZcrxFill {
    segs: Vec<(u32, AreaSlice)>,
    len: usize,
}

impl ZcrxFill {
    pub fn len(&self) -> usize {
        self.len
    }
    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// Gather the scatter list into `dest` — the single CPU pass over
    /// payload bytes (priced in `zcrx_gather_bytes` by the caller; the Z3
    /// serve fusion folds it into `read_copy_dest_bytes`).
    ///
    /// SAFETY contract: `dest..dest+len` is writable and exclusively the
    /// caller's; every span was validated `datao + len ≤ command len` at
    /// record time.
    pub fn gather_into(&self, dest: *mut u8) {
        for (datao, s) in &self.segs {
            let src = s.as_slice();
            // SAFETY: span bounds validated at record time (on_c2h_span);
            // area chunks are pinned by the slices' grant refs.
            unsafe {
                std::ptr::copy_nonoverlapping(src.as_ptr(), dest.add(*datao as usize), src.len());
            }
        }
    }
}

struct PendingFill {
    len: usize,
    received: u64,
    segs: Vec<(u32, AreaSlice)>,
    tx: Option<oneshot::Sender<Result<ZcrxFill>>>,
}

/// CID → in-flight fill map (see module docs).
#[derive(Default)]
pub struct FillTable {
    pending: Mutex<HashMap<u16, PendingFill>>,
}

impl FillTable {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn is_empty(&self) -> bool {
        self.pending.lock().expect("fill table lock").is_empty()
    }

    /// Register an in-flight command; the receiver resolves at
    /// completion (fill or op error).
    pub fn insert(&self, cid: u16, len: usize) -> oneshot::Receiver<Result<ZcrxFill>> {
        let (tx, rx) = oneshot::channel();
        let prev = self.pending.lock().expect("fill table lock").insert(
            cid,
            PendingFill {
                len,
                received: 0,
                segs: Vec::new(),
                tx: Some(tx),
            },
        );
        debug_assert!(prev.is_none(), "CID reuse while pending");
        rx
    }

    /// Cancel a registration (send-side failure before the wire).
    pub fn cancel(&self, cid: u16) {
        self.pending.lock().expect("fill table lock").remove(&cid);
    }

    /// Apply one parser event. `Err` = framing violation (the driver
    /// counts the tripwire and poisons).
    pub fn apply(&self, ev: ParseEvent) -> std::result::Result<(), String> {
        match ev {
            ParseEvent::C2hSpan { cid, datao, slice } => self.on_c2h_span(cid, datao, slice),
            ParseEvent::C2hEnd { cid, last, success } => self.on_c2h_end(cid, last, success),
            ParseEvent::Cqe(cqe) => self.on_cqe(&cqe),
        }
    }

    fn on_c2h_span(
        &self,
        cid: u16,
        datao: u32,
        slice: AreaSlice,
    ) -> std::result::Result<(), String> {
        let mut map = self.pending.lock().expect("fill table lock");
        let Some(p) = map.get_mut(&cid) else {
            return Err(format!("C2HData for unknown CID {cid}"));
        };
        let end = datao as usize + slice.len();
        if end > p.len {
            return Err(format!(
                "C2HData span {}..{} exceeds command length {}",
                datao, end, p.len
            ));
        }
        p.received += slice.len() as u64;
        p.segs.push((datao, slice));
        Ok(())
    }

    fn on_c2h_end(&self, cid: u16, last: bool, success: bool) -> std::result::Result<(), String> {
        if !success {
            return Ok(());
        }
        if !last {
            return Err("SUCCESS on a non-LAST C2HData".into());
        }
        let mut map = self.pending.lock().expect("fill table lock");
        let Some(p) = map.remove(&cid) else {
            return Err(format!("SUCCESS C2HData for unknown CID {cid}"));
        };
        drop(map);
        complete(p);
        Ok(())
    }

    fn on_cqe(&self, cqe: &pdu::Cqe) -> std::result::Result<(), String> {
        let mut map = self.pending.lock().expect("fill table lock");
        let Some(mut p) = map.remove(&cqe.cid) else {
            return Err(format!("CapsuleResp for unknown CID {}", cqe.cid));
        };
        drop(map);
        if cqe.status != 0 {
            if let Some(tx) = p.tx.take() {
                let _ = tx.send(Err(io_err(format!(
                    "lane read failed: controller status {:#06x}",
                    cqe.status
                ))));
            }
            return Ok(());
        }
        complete(p);
        Ok(())
    }

    /// Fail every in-flight op (queue poison); dropping the pending segs
    /// releases their chunk refs (the drain's recycle edge).
    pub fn fail_all(&self, why: &str) {
        let mut map = self.pending.lock().expect("fill table lock");
        for (_, mut p) in map.drain() {
            if let Some(tx) = p.tx.take() {
                let _ = tx.send(Err(io_err(format!("lane queue poisoned: {why}"))));
            }
        }
    }
}

/// Completion law (exact length — the nvme_dev exact-length contract):
/// short data with a success status is a framing violation, never
/// partial data (counted here, matching the Z1 classic path).
fn complete(mut p: PendingFill) {
    let ok = p.received == p.len as u64;
    if !ok {
        crate::fuse_client::METRICS
            .zcrx_frame_violations
            .fetch_add(1, Ordering::Relaxed);
    }
    if let Some(tx) = p.tx.take() {
        let _ = tx.send(if ok {
            Ok(ZcrxFill {
                segs: std::mem::take(&mut p.segs),
                len: p.len,
            })
        } else {
            Err(io_err(format!(
                "lane read completed with {} of {} bytes",
                p.received, p.len
            )))
        });
    }
}
