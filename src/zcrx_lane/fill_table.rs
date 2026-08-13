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
use super::area_queue::AdmissionUnits;
use super::pdu;
use super::pdu_stream::ParseEvent;
use crate::error::{Result, SqueezefsError};
use squeezefs_ipc::sqz_channel::oneshot;
use std::collections::HashMap;
use std::sync::atomic::Ordering;
use std::sync::Mutex;

fn io_err(msg: String) -> SqueezefsError {
    SqueezefsError::Io(std::io::Error::other(msg))
}

/// One completed fill: the command's payload as refcounted area spans.
/// Dropping it releases every chunk ref (the refill discipline's last
/// edge) AND the op's admission permits — on the requester's normal exit
/// after the gather, or inside the dead completion channel when the
/// requester future was cancelled (MEM-3: admission accounting stays
/// exact under future-drop). [`ZcrxFill::gather_into`] is the ONE priced
/// completion pass.
pub struct ZcrxFill {
    segs: Vec<(u32, AreaSlice)>,
    len: usize,
    _admission: Option<AdmissionUnits>,
}

impl ZcrxFill {
    /// Bench/contract constructor (`benches/zcrx_bench.rs` — the sim
    /// venue's fused-vs-two-pass gather pair): a fill over caller-built
    /// spans, no admission custody.
    pub fn from_parts(segs: Vec<(u32, AreaSlice)>, len: usize) -> ZcrxFill {
        ZcrxFill {
            segs,
            len,
            _admission: None,
        }
    }

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
    /// # Safety
    ///
    /// `dest..dest+len` must be writable and exclusively the caller's;
    /// every span was validated `datao + len ≤ command len` at record time.
    pub unsafe fn gather_into(&self, dest: *mut u8) {
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
    /// Admission custody: moves into the completed [`ZcrxFill`]; released
    /// with the entry on error/poison paths (MEM-3 exact accounting).
    admission: Option<AdmissionUnits>,
    /// CID + depth-permit custody — returns when the entry is destroyed
    /// (completion, send-failure cancel, or poison drain), never on the
    /// requester's exits (the MEM-3 no-leak law; lock order: this table's
    /// `pending` → the slot's CID pool).
    _slot: super::initiator::CidSlot,
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
    /// completion (fill or op error). The entry takes custody of the
    /// op's CID slot and admission permits (see [`PendingFill`]).
    pub(crate) fn insert(
        &self,
        cid: u16,
        len: usize,
        slot: super::initiator::CidSlot,
        admission: Option<AdmissionUnits>,
    ) -> oneshot::Receiver<Result<ZcrxFill>> {
        debug_assert_eq!(slot.cid(), cid, "slot/cid custody mismatch");
        let (tx, rx) = oneshot::channel();
        let prev = self.pending.lock().expect("fill table lock").insert(
            cid,
            PendingFill {
                len,
                received: 0,
                segs: Vec::new(),
                tx: Some(tx),
                admission,
                _slot: slot,
            },
        );
        debug_assert!(prev.is_none(), "CID reuse while pending");
        rx
    }

    /// Cancel a registration (send-side failure before the wire — the
    /// entry drop returns CID/permit custody).
    pub(crate) fn cancel(&self, cid: u16) {
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
                    "lane read failed: {}",
                    pdu::describe_status(cqe.status)
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
                let _ = tx.send(Err(io_err(why.to_string())));
            }
        }
    }
}

/// Completion law (exact length — the nvme_dev exact-length contract):
/// short data with a success status is a framing violation, never
/// partial data (counted here, matching the Z1 classic path). Admission
/// custody rides the fill; the entry's CID slot returns when `p` drops —
/// after the send, so a fresh op can only reuse the CID once this entry
/// is gone (cancelled requesters included: the failed send drops the
/// fill, releasing spans + admission right here).
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
                _admission: p.admission.take(),
            })
        } else {
            Err(io_err(format!(
                "lane read completed with {} of {} bytes",
                p.received, p.len
            )))
        });
    }
}
