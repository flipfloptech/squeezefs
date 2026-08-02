//! The streaming NVMe/TCP PDU parser over a CHUNK sequence (design §4.3):
//! zcrx delivers raw TCP payload in area chunks, so PDU headers arrive
//! interleaved with data and possibly split across chunk seams. This
//! push-based state machine copies ONLY header bytes (≤ 128 B per PDU,
//! priced in `zcrx_hdr_copy_bytes` — the honestly-priced edge copy, small
//! *because it excludes payload*), skips PDO padding by arithmetic, and
//! emits C2HData payload as REF sub-slices of the pushed chunk (each
//! holding a grant ref — never copied at parse time).
//!
//! Sync + push-based on purpose: the same machine runs on the async sim
//! reader task (contract venue) and the real backend's ring driver thread
//! (field), and unit-level tests can split the stream at every byte.
//! Every geometry refusal is a FRAMING violation the caller must poison
//! on (the counter is the caller's — this module only reports).

use super::area::AreaSlice;
use super::pdu;

/// One parsed occurrence the driver applies to the fill table.
pub enum ParseEvent {
    /// A C2HData payload fragment (a ref into the area, never a copy).
    C2hSpan {
        cid: u16,
        datao: u32,
        slice: AreaSlice,
    },
    /// A C2HData PDU fully consumed (flags per the header).
    C2hEnd { cid: u16, last: bool, success: bool },
    /// A CapsuleResp completion.
    Cqe(pdu::Cqe),
}

enum State {
    /// Accumulating CH (+PSH) bytes into scratch.
    Header,
    /// Skipping PDO padding (no copy, no emit).
    Pad {
        left: usize,
        cid: u16,
        datao: u32,
        datal: u32,
        last: bool,
        success: bool,
    },
    /// Mapping payload bytes to ref spans.
    Payload {
        left: usize,
        cid: u16,
        next_datao: u32,
        last: bool,
        success: bool,
    },
}

/// Push-based PDU stream parser (see module docs).
pub struct StreamParser {
    scratch: [u8; 128],
    have: usize,
    need: usize,
    state: State,
}

impl Default for StreamParser {
    fn default() -> Self {
        Self::new()
    }
}

impl StreamParser {
    pub fn new() -> Self {
        StreamParser {
            scratch: [0u8; 128],
            have: 0,
            need: 8,
            state: State::Header,
        }
    }

    /// Feed one received chunk extent; emits events in stream order.
    /// `Err` = framing violation (the caller counts the tripwire and
    /// poisons the session).
    pub fn push(&mut self, chunk: &AreaSlice, out: &mut Vec<ParseEvent>) -> Result<(), String> {
        let bytes = chunk.as_slice();
        let mut pos = 0usize;
        while pos < bytes.len() {
            match &mut self.state {
                State::Header => {
                    let want = self.need - self.have;
                    let take = want.min(bytes.len() - pos);
                    self.scratch[self.have..self.have + take]
                        .copy_from_slice(&bytes[pos..pos + take]);
                    crate::fuse_client::METRICS
                        .zcrx_hdr_copy_bytes
                        .fetch_add(take as u64, std::sync::atomic::Ordering::Relaxed);
                    self.have += take;
                    pos += take;
                    if self.have < self.need {
                        continue;
                    }
                    if self.have == 8 {
                        // CH complete: extend to the full header.
                        let hlen = self.scratch[2] as usize;
                        if !(8..=128).contains(&hlen) {
                            return Err(format!("CH hlen {hlen} out of range"));
                        }
                        if hlen > 8 {
                            self.need = hlen;
                            continue;
                        }
                    }
                    self.dispatch_header(out)?;
                }
                State::Pad {
                    left,
                    cid,
                    datao,
                    datal,
                    last,
                    success,
                } => {
                    let take = (*left).min(bytes.len() - pos);
                    *left -= take;
                    pos += take;
                    if *left == 0 {
                        let (cid, datao, datal, last, success) =
                            (*cid, *datao, *datal, *last, *success);
                        self.enter_payload(cid, datao, datal, last, success, out);
                    }
                }
                State::Payload {
                    left,
                    cid,
                    next_datao,
                    last,
                    success,
                } => {
                    let take = (*left).min(bytes.len() - pos);
                    out.push(ParseEvent::C2hSpan {
                        cid: *cid,
                        datao: *next_datao,
                        slice: chunk.sub(pos, take),
                    });
                    *next_datao += take as u32;
                    *left -= take;
                    pos += take;
                    if *left == 0 {
                        out.push(ParseEvent::C2hEnd {
                            cid: *cid,
                            last: *last,
                            success: *success,
                        });
                        self.reset_header();
                    }
                }
            }
        }
        Ok(())
    }

    fn reset_header(&mut self) {
        self.have = 0;
        self.need = 8;
        self.state = State::Header;
    }

    fn enter_payload(
        &mut self,
        cid: u16,
        datao: u32,
        datal: u32,
        last: bool,
        success: bool,
        out: &mut Vec<ParseEvent>,
    ) {
        if datal == 0 {
            out.push(ParseEvent::C2hEnd { cid, last, success });
            self.reset_header();
        } else {
            self.state = State::Payload {
                left: datal as usize,
                cid,
                next_datao: datao,
                last,
                success,
            };
        }
    }

    /// Full header accumulated: classify and transition.
    fn dispatch_header(&mut self, out: &mut Vec<ParseEvent>) -> Result<(), String> {
        let hlen = self.have;
        let ch = pdu::parse_common(&self.scratch[..8]).map_err(|e| e.to_string())?;
        match ch.pdu_type {
            pdu::PDU_C2H_DATA => {
                let c2h =
                    pdu::parse_c2h_data(ch, &self.scratch[..hlen]).map_err(|e| e.to_string())?;
                let pad = ch.pdo as usize - hlen;
                if pad > 0 {
                    self.state = State::Pad {
                        left: pad,
                        cid: c2h.cccid,
                        datao: c2h.datao,
                        datal: c2h.datal,
                        last: c2h.last,
                        success: c2h.success,
                    };
                } else {
                    self.enter_payload(c2h.cccid, c2h.datao, c2h.datal, c2h.last, c2h.success, out);
                }
                Ok(())
            }
            pdu::PDU_CAPSULE_RESP => {
                // The 16-byte CQE IS the PSH (hlen 24 = 8 CH + 16 CQE) —
                // consuming past it desynchronizes the stream (the Z1
                // contract-suite catch, re-pinned here).
                if hlen < 24 {
                    return Err(format!("CapsuleResp hlen {hlen} < 24"));
                }
                let cqe = pdu::parse_cqe(&self.scratch[8..24]).map_err(|e| e.to_string())?;
                out.push(ParseEvent::Cqe(cqe));
                self.reset_header();
                Ok(())
            }
            other => Err(format!("unexpected PDU type {other:#x} on IO queue")),
        }
    }
}
