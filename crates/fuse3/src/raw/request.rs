use crate::raw::abi::fuse_in_header;

/// Where a request's reply must go (FUSE-2 ⊕ PERF-16).
///
/// A FUSE-over-io_uring request is delivered on one ring slot and is
/// answered by a `COMMIT_AND_FETCH` against **that** slot. Carrying the
/// address in the request is what let the transport's sharded
/// `unique → (qid, ent_idx, commit_id)` map be **deleted** rather than
/// wrapped: the reply path addresses its slot directly (no mutex, no
/// hash, nothing to miss and nothing to collide in — spec FUSE-2 rows 4
/// and 10, PERF-16's three sharded-mutex acquisitions per request).
#[derive(Debug, Default, Copy, Clone, Eq, PartialEq, Hash, Ord, PartialOrd)]
pub enum ReplySlot {
    /// Classical `/dev/fuse` delivery — `FUSE_INIT`, the kernel-mandated
    /// post-arm sideband (FORGET/INTERRUPT/resends), switchover
    /// stragglers, and daemon-initiated notifications. The reply rides a
    /// device write; there is no ring ent to address.
    #[default]
    Classical,
    /// FUSE-over-io_uring delivery: the reply commits against this slot.
    Ring {
        qid: u16,
        ent_idx: u16,
        commit_id: u64,
    },
}

impl ReplySlot {
    /// True when the reply must ride `COMMIT_AND_FETCH` on a ring slot.
    #[inline]
    pub fn is_ring(&self) -> bool {
        matches!(self, ReplySlot::Ring { .. })
    }
}

#[derive(Debug, Default, Copy, Clone, Eq, PartialEq, Hash, Ord, PartialOrd)]
/// Request data
pub struct Request {
    /// the unique identifier of this request.
    pub unique: u64,
    /// the uid of this request.
    pub uid: u32,
    /// the gid of this request.
    pub gid: u32,
    /// the pid of this request.
    pub pid: u32,
    /// Where this request's reply must be committed (FUSE-2 ⊕ PERF-16).
    ///
    /// Set by the dispatch loop from the delivery that produced the
    /// request; [`ReplySlot::Classical`] for classical/synthetic
    /// requests. Carrying it here is what deleted the transport's
    /// `unique → slot` map.
    pub slot: ReplySlot,
}

impl From<&fuse_in_header> for Request {
    fn from(header: &fuse_in_header) -> Self {
        Self {
            unique: header.unique,
            uid: header.uid,
            gid: header.gid,
            pid: header.pid,
            slot: ReplySlot::Classical,
        }
    }
}
