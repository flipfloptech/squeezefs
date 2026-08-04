//! NVMe/TCP PDU codec for the zcrx read lane (`docs/design-zcrx-read-lane.md`
//! §4). Read-only initiator surface **by construction**: the encoder set is
//! exactly {ICReq, Connect, Property Get/Set, Read} — there is no
//! H2CData/write/reservation builder, so the lane cannot touch the D0/fencing
//! surface (§3).
//!
//! Layouts follow the NVMe/TCP transport spec 1.0 wire format (verified
//! against the Linux host/target implementations); all multi-byte fields are
//! little-endian.

use thiserror::Error;

/// PDU types (CH byte 0).
pub const PDU_ICREQ: u8 = 0x00;
pub const PDU_ICRESP: u8 = 0x01;
pub const PDU_H2C_TERM: u8 = 0x02;
pub const PDU_C2H_TERM: u8 = 0x03;
pub const PDU_CAPSULE_CMD: u8 = 0x04;
pub const PDU_CAPSULE_RESP: u8 = 0x05;
pub const PDU_C2H_DATA: u8 = 0x07;

/// C2HData flags (CH byte 1).
pub const C2H_FLAG_LAST: u8 = 1 << 2;
pub const C2H_FLAG_SUCCESS: u8 = 1 << 3;

/// Fabrics command type values (SQE byte 4 when opcode = 0x7F).
pub const FCTYPE_PROPERTY_SET: u8 = 0x00;
pub const FCTYPE_CONNECT: u8 = 0x01;
pub const FCTYPE_PROPERTY_GET: u8 = 0x04;

/// NVMe opcodes used by the lane.
pub const OPC_FABRICS: u8 = 0x7F;
pub const OPC_READ: u8 = 0x02;

/// Controller property offsets.
pub const PROP_CAP: u32 = 0x00;
pub const PROP_CC: u32 = 0x14;
pub const PROP_CSTS: u32 = 0x1C;

/// CC value the lane sets: EN=1, CSS=NVM, IOSQES=6 (64 B), IOCQES=4 (16 B).
pub const CC_ENABLE_NVM: u64 = 1 | (6 << 16) | (4 << 20);

/// Framing violations — every arm poisons the session loud
/// (`zcrx_frame_violations` tripwire, design §7).
#[derive(Debug, Error, PartialEq, Eq)]
pub enum FrameError {
    #[error("short PDU: need {need} bytes, have {have}")]
    Short { need: usize, have: usize },
    #[error("unexpected PDU type {got:#x} (expected {want:#x})")]
    UnexpectedType { got: u8, want: u8 },
    #[error("PDU geometry violation: {0}")]
    Geometry(String),
    #[error("IC negotiation refused: {0}")]
    Negotiation(String),
}

/// The 8-byte common header on every PDU.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CommonHdr {
    pub pdu_type: u8,
    pub flags: u8,
    pub hlen: u8,
    pub pdo: u8,
    pub plen: u32,
}

pub fn parse_common(b: &[u8]) -> Result<CommonHdr, FrameError> {
    if b.len() < 8 {
        return Err(FrameError::Short {
            need: 8,
            have: b.len(),
        });
    }
    Ok(CommonHdr {
        pdu_type: b[0],
        flags: b[1],
        hlen: b[2],
        pdo: b[3],
        plen: u32::from_le_bytes([b[4], b[5], b[6], b[7]]),
    })
}

fn put_common(buf: &mut [u8], pdu_type: u8, flags: u8, hlen: u8, pdo: u8, plen: u32) {
    buf[0] = pdu_type;
    buf[1] = flags;
    buf[2] = hlen;
    buf[3] = pdo;
    buf[4..8].copy_from_slice(&plen.to_le_bytes());
}

/// ICReq: PFV 0, HPDA 0, digests OFF, MAXR2T 0 (no writes ⇒ no R2T).
pub fn encode_icreq() -> [u8; 128] {
    let mut b = [0u8; 128];
    put_common(&mut b, PDU_ICREQ, 0, 128, 0, 128);
    // pfv (u16 le) = 0, hpda = 0, digest = 0, maxr2t (u32 le) = 0 — all zero.
    b
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct IcResp {
    pub pfv: u16,
    pub cpda: u8,
    pub digest: u8,
    pub maxh2cdata: u32,
}

/// Parse ICResp and enforce the lane's negotiation law: PFV 0, digests OFF
/// (a controller demanding digests refuses the arm loud — standing perf-fleet
/// posture, design §4.2).
pub fn parse_icresp(pdu: &[u8]) -> Result<IcResp, FrameError> {
    if pdu.len() < 128 {
        return Err(FrameError::Short {
            need: 128,
            have: pdu.len(),
        });
    }
    let ch = parse_common(pdu)?;
    if ch.pdu_type != PDU_ICRESP {
        return Err(FrameError::UnexpectedType {
            got: ch.pdu_type,
            want: PDU_ICRESP,
        });
    }
    let resp = IcResp {
        pfv: u16::from_le_bytes([pdu[8], pdu[9]]),
        cpda: pdu[10],
        digest: pdu[11],
        maxh2cdata: u32::from_le_bytes([pdu[12], pdu[13], pdu[14], pdu[15]]),
    };
    if resp.pfv != 0 {
        return Err(FrameError::Negotiation(format!(
            "controller PFV {} (lane speaks PFV 0 only)",
            resp.pfv
        )));
    }
    if resp.digest != 0 {
        return Err(FrameError::Negotiation(format!(
            "controller demands digests {:#04x} — lane negotiates digests off \
             (never silently accept a per-byte CRC pass)",
            resp.digest
        )));
    }
    Ok(resp)
}

/// SQE scaffold: opcode, PSDT=SGL (flags 0x40), CID.
fn sqe_base(opcode: u8, cid: u16) -> [u8; 64] {
    let mut sqe = [0u8; 64];
    sqe[0] = opcode;
    sqe[1] = 0x40; // PSDT: SGL used for data transfer
    sqe[2..4].copy_from_slice(&cid.to_le_bytes());
    sqe
}

/// In-capsule SGL Data Block descriptor (offset form) at SQE bytes 24..40.
fn sgl_incapsule(sqe: &mut [u8; 64], len: u32) {
    // address = 0 (offset from start of in-capsule data), length, type 0x01
    // (Data Block, Sub Type Offset).
    sqe[32..36].copy_from_slice(&len.to_le_bytes());
    sqe[39] = 0x01;
}

/// Transport SGL Data Block descriptor (type 5, sub type 0xA) — data rides
/// C2HData PDUs.
fn sgl_transport(sqe: &mut [u8; 64], len: u32) {
    sqe[32..36].copy_from_slice(&len.to_le_bytes());
    sqe[39] = 0x5A;
}

/// Connect data blob geometry: EXACTLY 1024 B in-capsule —
/// `sizeof(struct nvmf_connect_data)` (hostid 16 + cntlid 2 + resv 238 +
/// subsysnqn 256 + hostnqn 256 + resv 256). The length is load-bearing on
/// the wire: nvmet validates the Connect transfer length against it
/// (`nvmet_check_transfer_len`) and refuses anything else with Data SGL
/// Length Invalid | DNR = 0x400f — the 2026-08-04 field arm-refusal, which
/// shipped a 4096 B blob here.
pub const CONNECT_DATA_LEN: usize = 1024;

/// Fabrics Connect capsule: CH(72) + SQE + 1024 B connect data.
/// `cntlid` is 0xFFFF for the admin-queue connect (controller assigns) and
/// the assigned id for IO-queue connects. `sqsize0` is 0-based.
#[allow(clippy::too_many_arguments)]
pub fn encode_connect_capsule(
    qid: u16,
    sqsize0: u16,
    kato_ms: u32,
    cid: u16,
    hostid: &[u8; 16],
    cntlid: u16,
    subnqn: &str,
    hostnqn: &str,
) -> Vec<u8> {
    let plen = 8 + 64 + CONNECT_DATA_LEN;
    let mut pdu = vec![0u8; plen];
    put_common(&mut pdu, PDU_CAPSULE_CMD, 0, 72, 72, plen as u32);

    let mut sqe = sqe_base(OPC_FABRICS, cid);
    sqe[4] = FCTYPE_CONNECT;
    sgl_incapsule(&mut sqe, CONNECT_DATA_LEN as u32);
    // recfmt @40 = 0, qid @42, sqsize @44 (0-based), cattr @46 = 0, kato @48.
    sqe[42..44].copy_from_slice(&qid.to_le_bytes());
    sqe[44..46].copy_from_slice(&sqsize0.to_le_bytes());
    sqe[48..52].copy_from_slice(&kato_ms.to_le_bytes());
    pdu[8..72].copy_from_slice(&sqe);

    let data = &mut pdu[72..];
    data[0..16].copy_from_slice(hostid);
    data[16..18].copy_from_slice(&cntlid.to_le_bytes());
    let nqn = subnqn.as_bytes();
    debug_assert!(nqn.len() < 256, "subnqn must fit 256 B NUL-padded");
    data[256..256 + nqn.len()].copy_from_slice(nqn);
    let hn = hostnqn.as_bytes();
    debug_assert!(hn.len() < 256, "hostnqn must fit 256 B NUL-padded");
    data[512..512 + hn.len()].copy_from_slice(hn);
    pdu
}

fn encode_property_capsule(
    fctype: u8,
    cid: u16,
    attrib8: bool,
    offset: u32,
    value: u64,
) -> Vec<u8> {
    let mut pdu = vec![0u8; 72];
    put_common(&mut pdu, PDU_CAPSULE_CMD, 0, 72, 0, 72);
    let mut sqe = sqe_base(OPC_FABRICS, cid);
    sqe[4] = fctype;
    sqe[40] = u8::from(attrib8);
    sqe[44..48].copy_from_slice(&offset.to_le_bytes());
    if fctype == FCTYPE_PROPERTY_SET {
        sqe[48..56].copy_from_slice(&value.to_le_bytes());
    }
    pdu[8..72].copy_from_slice(&sqe);
    pdu
}

pub fn encode_property_set(cid: u16, offset: u32, value: u64, attrib8: bool) -> Vec<u8> {
    encode_property_capsule(FCTYPE_PROPERTY_SET, cid, attrib8, offset, value)
}

pub fn encode_property_get(cid: u16, offset: u32, attrib8: bool) -> Vec<u8> {
    encode_property_capsule(FCTYPE_PROPERTY_GET, cid, attrib8, offset, 0)
}

/// NVM Read capsule (opcode 0x02, Transport SGL): `nlb` is the 1-based block
/// count; `xfer_len` the byte transfer length the SGL advertises.
pub fn encode_read_capsule(cid: u16, nsid: u32, slba: u64, nlb: u32, xfer_len: u32) -> Vec<u8> {
    debug_assert!(nlb >= 1 && nlb <= 0x1_0000, "nlb out of CDW12 range");
    let mut pdu = vec![0u8; 72];
    put_common(&mut pdu, PDU_CAPSULE_CMD, 0, 72, 0, 72);
    let mut sqe = sqe_base(OPC_READ, cid);
    sqe[4..8].copy_from_slice(&nsid.to_le_bytes());
    sgl_transport(&mut sqe, xfer_len);
    sqe[40..48].copy_from_slice(&slba.to_le_bytes());
    sqe[48..50].copy_from_slice(&(((nlb - 1) & 0xFFFF) as u16).to_le_bytes());
    pdu[8..72].copy_from_slice(&sqe);
    pdu
}

/// Render a CQE Status Field ([`Cqe::status`] — post-phase-strip: DNR bit
/// 14, MORE bit 13, SCT bits 10:8, SC bits 7:0) with its SCT/SC
/// decomposition, the known name where this initiator's surface can meet
/// the code, and the DNR/MORE bits. A raw hex cost a field session
/// (2026-08-04: "controller status 0x400f" named nothing — it is nvmet's
/// `nvmet_check_transfer_len` refusal, Data SGL Length Invalid + DNR).
pub fn describe_status(sf: u16) -> String {
    let sc = (sf & 0xFF) as u8;
    let sct = ((sf >> 8) & 0x7) as u8;
    let sct_name = match sct {
        0 => "generic",
        1 => "command-specific/fabrics",
        2 => "media/data-integrity",
        3 => "path-related",
        7 => "vendor-specific",
        _ => "reserved",
    };
    let mut out = format!("controller status {sf:#06x} (SCT={sct:#x} {sct_name}, SC={sc:#04x}");
    if let Some(name) = status_name(sct, sc) {
        out.push(' ');
        out.push_str(name);
    }
    if sf & (1 << 13) != 0 {
        out.push_str(", MORE");
    }
    if sf & (1 << 14) != 0 {
        out.push_str(", DNR");
    }
    out.push(')');
    out
}

/// Names for the status codes the lane's read-only fabrics surface can
/// actually meet (the connect/property ladder + NVM Read) — honest `None`
/// for the rest, never an invented name.
fn status_name(sct: u8, sc: u8) -> Option<&'static str> {
    match (sct, sc) {
        (0, 0x00) => Some("Success"),
        (0, 0x01) => Some("Invalid Command Opcode"),
        (0, 0x02) => Some("Invalid Field in Command"),
        (0, 0x03) => Some("Command ID Conflict"),
        (0, 0x04) => Some("Data Transfer Error"),
        (0, 0x06) => Some("Internal Error"),
        (0, 0x0B) => Some("Invalid Namespace or Format"),
        (0, 0x0C) => Some("Command Sequence Error"),
        (0, 0x0D) => Some("Invalid SGL Segment Descriptor"),
        (0, 0x0E) => Some("Invalid Number of SGL Descriptors"),
        (0, 0x0F) => Some("Data SGL Length Invalid"),
        (0, 0x10) => Some("Metadata SGL Length Invalid"),
        (0, 0x11) => Some("SGL Descriptor Type Invalid"),
        (0, 0x80) => Some("LBA Out of Range"),
        (0, 0x81) => Some("Capacity Exceeded"),
        (0, 0x82) => Some("Namespace Not Ready"),
        (1, 0x80) => Some("Connect Incompatible Format"),
        (1, 0x81) => Some("Connect Controller Busy"),
        (1, 0x82) => Some("Connect Invalid Parameters"),
        (1, 0x83) => Some("Connect Restart Discovery"),
        (1, 0x84) => Some("Connect Invalid Host"),
        (2, 0x81) => Some("Unrecovered Read Error"),
        _ => None,
    }
}

/// Completion queue entry (16 B inside a CapsuleResp).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Cqe {
    pub dw0: u32,
    pub dw1: u32,
    pub sqhd: u16,
    pub cid: u16,
    /// Status Field (bits 15:1 of the raw status word — the phase bit is
    /// transport-reserved on NVMe-oF and stripped here).
    pub status: u16,
}

/// Parse the 16-byte CQE that follows a CapsuleResp CH.
pub fn parse_cqe(b: &[u8]) -> Result<Cqe, FrameError> {
    if b.len() < 16 {
        return Err(FrameError::Short {
            need: 16,
            have: b.len(),
        });
    }
    Ok(Cqe {
        dw0: u32::from_le_bytes([b[0], b[1], b[2], b[3]]),
        dw1: u32::from_le_bytes([b[4], b[5], b[6], b[7]]),
        sqhd: u16::from_le_bytes([b[8], b[9]]),
        cid: u16::from_le_bytes([b[12], b[13]]),
        status: u16::from_le_bytes([b[14], b[15]]) >> 1,
    })
}

/// C2HData per-PDU header (24 B) — payload location comes from the CH's PDO.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct C2hData {
    pub cccid: u16,
    pub datao: u32,
    pub datal: u32,
    pub last: bool,
    pub success: bool,
}

/// Parse the C2HData PSH given its already-parsed CH. `hdr` must hold the
/// full `ch.hlen` header bytes. Geometry law: `plen == pdo + datal` (digests
/// off, so the payload runs exactly to the PDU end) — anything else is a
/// framing violation that poisons the session.
pub fn parse_c2h_data(ch: CommonHdr, hdr: &[u8]) -> Result<C2hData, FrameError> {
    if ch.pdu_type != PDU_C2H_DATA {
        return Err(FrameError::UnexpectedType {
            got: ch.pdu_type,
            want: PDU_C2H_DATA,
        });
    }
    if hdr.len() < 24 || ch.hlen < 24 {
        return Err(FrameError::Short {
            need: 24,
            have: hdr.len().min(ch.hlen as usize),
        });
    }
    let datao = u32::from_le_bytes([hdr[12], hdr[13], hdr[14], hdr[15]]);
    let datal = u32::from_le_bytes([hdr[16], hdr[17], hdr[18], hdr[19]]);
    if (ch.pdo as u32) < ch.hlen as u32 || ch.plen != ch.pdo as u32 + datal {
        return Err(FrameError::Geometry(format!(
            "C2HData plen {} != pdo {} + datal {} (hlen {})",
            ch.plen, ch.pdo, datal, ch.hlen
        )));
    }
    Ok(C2hData {
        cccid: u16::from_le_bytes([hdr[8], hdr[9]]),
        datao,
        datal,
        last: ch.flags & C2H_FLAG_LAST != 0,
        success: ch.flags & C2H_FLAG_SUCCESS != 0,
    })
}
