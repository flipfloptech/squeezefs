//! zcrx read lane contracts (docs/design-zcrx-read-lane.md, PR Z1):
//! PDU codec laws, the mini-initiator association + read machinery against
//! an in-process mock NVMe/TCP target, capability-probe refusals, the
//! disarmed-default byte-identical law, and the funnel wire-in engagement
//! gauges. The gate runs `--test-threads=1`, so env mutation per test is
//! safe; every test restores the env it touches.

use squeezefs::zcrx_lane::{area, initiator::LaneTarget, pdu, probe, LaneBackend, LaneSession};
use squeezefs_testkit::skip;
use std::sync::atomic::Ordering;
use std::sync::Arc;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

// ---------------------------------------------------------------- mock target

#[derive(Clone)]
struct MockCfg {
    /// ICResp digest byte (nonzero must refuse the arm loud).
    icresp_digest: u8,
    /// Max bytes per C2HData PDU (forces multi-PDU reassembly).
    c2h_span: usize,
    /// Complete reads with the SUCCESS flag on the last C2HData (no
    /// CapsuleResp) instead of an explicit response capsule.
    success_elision: bool,
    /// Complete reads with this nonzero status (error injection).
    fail_status: Option<u16>,
    /// Emit a C2HData whose datao exceeds the command length (framing
    /// violation injection).
    corrupt_datao: bool,
    /// PDO padding to insert between C2HData header and payload.
    c2h_pdo_pad: usize,
    /// Drop the connection mid-C2HData payload (mid-flight death
    /// injection — the PR Z2 poison-lattice venue).
    die_mid_c2h: bool,
    /// Answer the Nth read capsule of a connection (and only it) with
    /// an ERROR status (0 = off): the round-7 closure venue — SOME
    /// segments of a multi-segment read complete, one op-fails, and the
    /// whole read tears WITHOUT a connection death (an abrupt-death
    /// venue RSTs the socket with unread capsules queued and races away
    /// the completed segment's delivery — nondeterministic by design of
    /// TCP, so the torn-read class is pinned death-free).
    fail_on_read_n: usize,
    /// Gate NVM Read service: each read acquires ONE permit before any
    /// C2HData/response is emitted (the MEM-3 cancellation venue — the
    /// test cancels the requester while the target withholds the
    /// response, then `add_permits` releases the completions).
    read_gate: Option<Arc<tokio::sync::Semaphore>>,
    /// Read capsules received (counted at arrival, BEFORE the gate) —
    /// the deterministic "capsule is on the wire" edge the cancellation
    /// tests key their aborts on.
    reads_seen: Option<Arc<std::sync::atomic::AtomicUsize>>,
}

impl Default for MockCfg {
    fn default() -> Self {
        Self {
            icresp_digest: 0,
            c2h_span: 8192,
            success_elision: false,
            fail_status: None,
            corrupt_datao: false,
            c2h_pdo_pad: 0,
            die_mid_c2h: false,
            fail_on_read_n: 0,
            read_gate: None,
            reads_seen: None,
        }
    }
}

struct MockTarget {
    port: u16,
    device: Arc<Vec<u8>>,
    _accept: tokio::task::JoinHandle<()>,
}

const MOCK_SUBNQN: &str = "nqn.2026-08.io.squeezefs:zcrx-lane-mock";
const MOCK_CNTLID: u16 = 0x1234;
const MOCK_LBA_SHIFT: u32 = 9;

async fn read_exact_or_eof(s: &mut TcpStream, buf: &mut [u8]) -> Option<()> {
    match s.read_exact(buf).await {
        Ok(_) => Some(()),
        Err(_) => None,
    }
}

fn le16(b: &[u8]) -> u16 {
    u16::from_le_bytes([b[0], b[1]])
}
fn le32(b: &[u8]) -> u32 {
    u32::from_le_bytes([b[0], b[1], b[2], b[3]])
}
fn le64(b: &[u8]) -> u64 {
    u64::from_le_bytes([b[0], b[1], b[2], b[3], b[4], b[5], b[6], b[7]])
}

fn capsule_resp(cid: u16, dw0: u32, status_field: u16) -> Vec<u8> {
    let mut pdu = vec![0u8; 24];
    pdu[0] = 0x05; // CapsuleResp
    pdu[2] = 24;
    pdu[4..8].copy_from_slice(&24u32.to_le_bytes());
    pdu[8..12].copy_from_slice(&dw0.to_le_bytes());
    pdu[20..22].copy_from_slice(&cid.to_le_bytes());
    pdu[22..24].copy_from_slice(&(status_field << 1).to_le_bytes());
    pdu
}

async fn mock_conn(mut s: TcpStream, device: Arc<Vec<u8>>, cfg: MockCfg) -> Option<()> {
    // ICReq / ICResp
    let mut icreq = [0u8; 128];
    read_exact_or_eof(&mut s, &mut icreq).await?;
    assert_eq!(icreq[0], 0x00, "first PDU must be ICReq");
    assert_eq!(icreq[11], 0, "lane must negotiate digests off");
    let mut icresp = [0u8; 128];
    icresp[0] = 0x01;
    icresp[2] = 128;
    icresp[4..8].copy_from_slice(&128u32.to_le_bytes());
    icresp[11] = cfg.icresp_digest;
    icresp[12..16].copy_from_slice(&(128 * 1024u32).to_le_bytes());
    s.write_all(&icresp).await.ok()?;

    let mut cc_enabled = false;
    let mut conn_reads: usize = 0;
    loop {
        let mut ch = [0u8; 8];
        read_exact_or_eof(&mut s, &mut ch).await?;
        assert_eq!(ch[0], 0x04, "host must only send CapsuleCmd after IC");
        let plen = le32(&ch[4..8]) as usize;
        let mut rest = vec![0u8; plen - 8];
        read_exact_or_eof(&mut s, &mut rest).await?;
        let sqe = &rest[..64];
        let opcode = sqe[0];
        let cid = le16(&sqe[2..4]);
        match opcode {
            0x7F => {
                let fctype = sqe[4];
                match fctype {
                    0x01 => {
                        // Connect: validate in-capsule data geometry with real
                        // nvmet's strictness (fabrics-cmd.c): the Connect data
                        // is EXACTLY `sizeof(struct nvmf_connect_data)` = 1024
                        // bytes, and `nvmet_check_transfer_len` refuses any
                        // other SGL/transfer length with Data SGL Length
                        // Invalid | DNR = 0x400f — the 2026-08-04 field
                        // refusal this mock used to be too permissive to
                        // catch (it accepted the retired 4096-byte blob).
                        // The refusal is a CQE, never a mock panic: it is the
                        // live-target behavior under test.
                        assert_eq!(
                            sqe[39], 0x01,
                            "connect data must ride the in-capsule offset SGL"
                        );
                        let sgl_len = le32(&sqe[32..36]) as usize;
                        let icd = &rest[64..];
                        assert_eq!(
                            icd.len(),
                            sgl_len,
                            "in-capsule bytes must match the SGL length"
                        );
                        if sgl_len != 1024 {
                            s.write_all(&capsule_resp(cid, 0, 0x400F)).await.ok()?;
                            continue;
                        }
                        let data = icd;
                        let subnqn = std::str::from_utf8(&data[256..512])
                            .unwrap()
                            .trim_end_matches('\0');
                        assert_eq!(subnqn, MOCK_SUBNQN, "connect subnqn mismatch");
                        let hostnqn = std::str::from_utf8(&data[512..768])
                            .unwrap()
                            .trim_end_matches('\0');
                        assert!(!hostnqn.is_empty(), "hostnqn must be present");
                        let qid = le16(&sqe[42..44]);
                        if qid != 0 {
                            let cntlid = le16(&data[16..18]);
                            assert_eq!(cntlid, MOCK_CNTLID, "IO connect must carry cntlid");
                        }
                        let kato = le32(&sqe[48..52]);
                        assert_eq!(kato, 0, "lane v1 connects with KATO 0");
                        s.write_all(&capsule_resp(cid, MOCK_CNTLID as u32, 0))
                            .await
                            .ok()?;
                    }
                    0x00 => {
                        // Property Set (CC).
                        let off = le32(&sqe[44..48]);
                        let val = le64(&sqe[48..56]);
                        if off == 0x14 && val & 1 == 1 {
                            cc_enabled = true;
                        }
                        s.write_all(&capsule_resp(cid, 0, 0)).await.ok()?;
                    }
                    0x04 => {
                        // Property Get: CAP or CSTS.
                        let off = le32(&sqe[44..48]);
                        let dw0 = match off {
                            0x00 => 127u32, // CAP low: MQES = 127
                            0x1C => u32::from(cc_enabled),
                            other => panic!("mock: unexpected property get {other:#x}"),
                        };
                        s.write_all(&capsule_resp(cid, dw0, 0)).await.ok()?;
                    }
                    other => panic!("mock: unexpected fctype {other:#x}"),
                }
            }
            0x02 => {
                // NVM Read.
                conn_reads += 1;
                if let Some(seen) = &cfg.reads_seen {
                    seen.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                }
                if let Some(gate) = &cfg.read_gate {
                    gate.acquire().await.ok()?.forget();
                }
                let nsid = le32(&sqe[4..8]);
                assert_eq!(nsid, 1, "lane must address the discovered nsid");
                assert_eq!(sqe[39], 0x5A, "read must use the Transport SGL descriptor");
                let xfer = le32(&sqe[32..36]) as usize;
                let slba = le64(&sqe[40..48]);
                let nlb0 = le16(&sqe[48..50]) as usize;
                assert_eq!(
                    (nlb0 + 1) << MOCK_LBA_SHIFT,
                    xfer,
                    "SGL length must equal the LBA span"
                );
                if let Some(status) = cfg.fail_status {
                    s.write_all(&capsule_resp(cid, 0, status)).await.ok()?;
                    continue;
                }
                if cfg.fail_on_read_n > 0 && conn_reads == cfg.fail_on_read_n {
                    // Round-7 closure venue: exactly this capsule op-fails
                    // (LBA out of range class); the stream stays ordered
                    // and alive — siblings before AND after it complete.
                    s.write_all(&capsule_resp(cid, 0, 0x0080)).await.ok()?;
                    continue;
                }
                let start = (slba as usize) << MOCK_LBA_SHIFT;
                let payload = &device[start..start + xfer];
                if cfg.die_mid_c2h {
                    // Half a C2HData header + payload, then vanish: the
                    // mid-flight NIC/target death the poison lattice must
                    // drain from.
                    let span = cfg.c2h_span.min(xfer);
                    let mut hdr = vec![0u8; 24];
                    hdr[0] = 0x07;
                    hdr[2] = 24;
                    hdr[3] = 24;
                    hdr[4..8].copy_from_slice(&((24 + span) as u32).to_le_bytes());
                    hdr[8..10].copy_from_slice(&cid.to_le_bytes());
                    hdr[16..20].copy_from_slice(&(span as u32).to_le_bytes());
                    s.write_all(&hdr).await.ok()?;
                    s.write_all(&payload[..span / 2]).await.ok()?;
                    s.flush().await.ok()?;
                    return None; // drops the connection
                }
                let mut off = 0usize;
                while off < xfer {
                    let span = cfg.c2h_span.min(xfer - off);
                    let last = off + span == xfer;
                    let datao = if cfg.corrupt_datao {
                        (xfer + 4096) as u32
                    } else {
                        off as u32
                    };
                    let pdo = 24 + cfg.c2h_pdo_pad;
                    let mut hdr = vec![0u8; pdo];
                    hdr[0] = 0x07;
                    hdr[1] = if last { 0x04 } else { 0 }
                        | if last && cfg.success_elision { 0x08 } else { 0 };
                    hdr[2] = 24;
                    hdr[3] = pdo as u8;
                    hdr[4..8].copy_from_slice(&((pdo + span) as u32).to_le_bytes());
                    hdr[8..10].copy_from_slice(&cid.to_le_bytes());
                    hdr[12..16].copy_from_slice(&datao.to_le_bytes());
                    hdr[16..20].copy_from_slice(&(span as u32).to_le_bytes());
                    s.write_all(&hdr).await.ok()?;
                    s.write_all(&payload[off..off + span]).await.ok()?;
                    off += span;
                }
                if !cfg.success_elision {
                    s.write_all(&capsule_resp(cid, 0, 0)).await.ok()?;
                }
            }
            other => panic!("mock: unexpected opcode {other:#x}"),
        }
    }
}

impl MockTarget {
    async fn start(cfg: MockCfg, device_len: usize) -> Self {
        let device: Arc<Vec<u8>> =
            Arc::new((0..device_len).map(|i| (i / 512 + i % 251) as u8).collect());
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let dev = Arc::clone(&device);
        let accept = tokio::spawn(async move {
            loop {
                let Ok((s, _)) = listener.accept().await else {
                    break;
                };
                let dev = Arc::clone(&dev);
                let cfg = cfg.clone();
                tokio::spawn(async move {
                    let _ = mock_conn(s, dev, cfg).await;
                });
            }
        });
        MockTarget {
            port,
            device,
            _accept: accept,
        }
    }

    fn target(&self, io_queues: u16, queue_depth: u16) -> LaneTarget {
        LaneTarget {
            traddr: "127.0.0.1".into(),
            trsvcid: self.port.to_string(),
            subnqn: MOCK_SUBNQN.into(),
            nsid: 1,
            lba_shift: MOCK_LBA_SHIFT,
            max_xfer_bytes: 256 * 1024,
            io_queues,
            queue_depth,
        }
    }
}

fn zcrx_metric(name: &str) -> u64 {
    let m = &squeezefs::fuse_client::METRICS;
    match name {
        "armed" => m.zcrx_lane_armed.load(Ordering::Relaxed),
        "fills" => m.zcrx_fills.load(Ordering::Relaxed),
        "fill_bytes" => m.zcrx_fill_bytes.load(Ordering::Relaxed),
        "fallbacks" => m.zcrx_fill_fallbacks.load(Ordering::Relaxed),
        "frame_violations" => m.zcrx_frame_violations.load(Ordering::Relaxed),
        "conn_errors" => m.zcrx_conn_errors.load(Ordering::Relaxed),
        "hdr_copy_bytes" => m.zcrx_hdr_copy_bytes.load(Ordering::Relaxed),
        "area_bytes" => m.zcrx_area_bytes.load(Ordering::Relaxed),
        "gather_bytes" => m.zcrx_gather_bytes.load(Ordering::Relaxed),
        "admission_waits" => m.zcrx_area_admission_waits.load(Ordering::Relaxed),
        "poisoned" => m.zcrx_lane_poisoned.load(Ordering::Relaxed),
        "dest_gather_bytes" => m.zcrx_dest_gather_bytes.load(Ordering::Relaxed),
        _ => panic!("unknown metric {name}"),
    }
}

/// Env guard: sets the lane env for one test and restores on drop (the
/// suite runs `--test-threads=1`, so per-test mutation is safe; the guard
/// makes restoration panic-proof).
struct LaneEnv;

impl LaneEnv {
    fn area_sim(mock: &MockTarget, io_queues: u16, depth: u16) -> LaneEnv {
        std::env::set_var("SQUEEZEFS_ZCRX_LANE", "1");
        std::env::set_var("SQUEEZEFS_ZCRX_LANE_AREA_SIM", "1");
        std::env::set_var(
            "SQUEEZEFS_ZCRX_LANE_TARGET",
            format!(
                "127.0.0.1,{},{},1,{},262144,{},{}",
                mock.port, MOCK_SUBNQN, MOCK_LBA_SHIFT, io_queues, depth
            ),
        );
        LaneEnv
    }
}

impl Drop for LaneEnv {
    fn drop(&mut self) {
        std::env::remove_var("SQUEEZEFS_ZCRX_LANE");
        std::env::remove_var("SQUEEZEFS_ZCRX_LANE_AREA_SIM");
        std::env::remove_var("SQUEEZEFS_ZCRX_LANE_TARGET");
        std::env::remove_var("SQUEEZEFS_ZCRX_LANE_SIM_CHUNK");
    }
}

// ------------------------------------------------------------------ codec laws

#[test]
fn test_codec_common_header_roundtrip() {
    let icreq = pdu::encode_icreq();
    let ch = pdu::parse_common(&icreq).expect("icreq CH parses");
    assert_eq!(ch.pdu_type, pdu::PDU_ICREQ);
    assert_eq!(ch.hlen, 128);
    assert_eq!(ch.plen, 128);
    // digests off, pfv 0, hpda 0 on the wire
    assert_eq!(&icreq[8..16], &[0u8; 8]);
}

#[test]
fn test_codec_connect_capsule_layout() {
    let hostid = [7u8; 16];
    let c = pdu::encode_connect_capsule(3, 63, 0, 9, &hostid, 0xBEEF, "nqn.sub", "nqn.host");
    // The Connect data blob is EXACTLY 1024 bytes — the NVMe-oF
    // `struct nvmf_connect_data` (hostid 16 + cntlid 2 + resv 238 +
    // subsysnqn 256 + hostnqn 256 + resv 256). Real nvmet enforces it
    // (`nvmet_check_transfer_len`) and refuses anything else with
    // 0x400f (Data SGL Length Invalid, DNR) — the 2026-08-04 field
    // arm-refusal: the encoder shipped a 4096-byte blob.
    assert_eq!(
        pdu::CONNECT_DATA_LEN,
        1024,
        "sizeof(struct nvmf_connect_data)"
    );
    assert_eq!(c.len(), 72 + 1024);
    assert_eq!(c[0], pdu::PDU_CAPSULE_CMD);
    assert_eq!(c[3], 72, "pdo must point at the in-capsule data");
    assert_eq!(u32::from_le_bytes([c[4], c[5], c[6], c[7]]), 72 + 1024);
    let sqe = &c[8..72];
    assert_eq!(sqe[0], pdu::OPC_FABRICS);
    assert_eq!(sqe[4], pdu::FCTYPE_CONNECT);
    assert_eq!(le16(&sqe[2..4]), 9, "cid");
    assert_eq!(le16(&sqe[42..44]), 3, "qid");
    assert_eq!(le16(&sqe[44..46]), 63, "sqsize (0-based)");
    assert_eq!(le32(&sqe[48..52]), 0, "kato");
    assert_eq!(sqe[39], 0x01, "in-capsule SGL descriptor type");
    assert_eq!(
        le32(&sqe[32..36]),
        1024,
        "SGL length = the connect data size"
    );
    let data = &c[72..];
    assert_eq!(&data[0..16], &hostid);
    assert_eq!(le16(&data[16..18]), 0xBEEF, "cntlid rides the connect data");
    assert_eq!(&data[256..263], b"nqn.sub");
    assert_eq!(&data[512..520], b"nqn.host");
}

#[test]
fn test_codec_status_decode_names_sct_sc_dnr() {
    // The field session cost of a raw hex: "controller status 0x400f"
    // named nothing. The decoder must render SCT/SC/DNR with the known
    // names on the codes this initiator can actually meet.
    let s = pdu::describe_status(0x400F);
    assert!(s.contains("0x400f"), "raw hex stays greppable: {s}");
    assert!(s.contains("Data SGL Length Invalid"), "{s}");
    assert!(s.contains("SCT=0x0"), "{s}");
    assert!(s.contains("SC=0x0f"), "{s}");
    assert!(s.contains("DNR"), "{s}");

    // The fabrics Connect refusal class (SCT=1, SC=0x82, DNR).
    let s = pdu::describe_status(0x4182);
    assert!(s.contains("Connect Invalid Parameters"), "{s}");
    assert!(s.contains("SCT=0x1"), "{s}");
    assert!(s.contains("SC=0x82"), "{s}");
    assert!(s.contains("DNR"), "{s}");

    // Unknown codes stay honest (no invented name), still decoded.
    let s = pdu::describe_status(0x0177);
    assert!(s.contains("SCT=0x1"), "{s}");
    assert!(s.contains("SC=0x77"), "{s}");
    assert!(!s.contains("DNR"), "no DNR bit set: {s}");
}

#[test]
fn test_codec_read_capsule_layout() {
    let c = pdu::encode_read_capsule(0x42, 5, 0x1_0000_0001, 2048, 2048 * 512);
    assert_eq!(c.len(), 72);
    let sqe = &c[8..72];
    assert_eq!(sqe[0], pdu::OPC_READ);
    assert_eq!(le32(&sqe[4..8]), 5, "nsid");
    assert_eq!(le64(&sqe[40..48]), 0x1_0000_0001, "slba spans cdw10/11");
    assert_eq!(le16(&sqe[48..50]), 2047, "nlb is 0-based");
    assert_eq!(sqe[39], 0x5A, "transport SGL descriptor");
    assert_eq!(le32(&sqe[32..36]), 2048 * 512, "SGL length");
}

#[test]
fn test_codec_icresp_negotiation_laws() {
    let mut icresp = [0u8; 128];
    icresp[0] = pdu::PDU_ICRESP;
    icresp[2] = 128;
    icresp[4..8].copy_from_slice(&128u32.to_le_bytes());
    let ok = pdu::parse_icresp(&icresp).expect("clean icresp accepted");
    assert_eq!(ok.digest, 0);

    let mut digests = icresp;
    digests[11] = 0x3;
    let err = pdu::parse_icresp(&digests).expect_err("digests must refuse");
    assert!(matches!(err, pdu::FrameError::Negotiation(_)), "{err}");

    let mut pfv = icresp;
    pfv[8] = 1;
    assert!(pdu::parse_icresp(&pfv).is_err(), "nonzero PFV must refuse");

    let mut wrong = icresp;
    wrong[0] = pdu::PDU_CAPSULE_RESP;
    assert!(pdu::parse_icresp(&wrong).is_err(), "wrong type must refuse");
}

#[test]
fn test_codec_c2h_geometry_law() {
    let mut hdr = [0u8; 24];
    hdr[0] = pdu::PDU_C2H_DATA;
    hdr[1] = pdu::C2H_FLAG_LAST | pdu::C2H_FLAG_SUCCESS;
    hdr[2] = 24;
    hdr[3] = 24;
    hdr[4..8].copy_from_slice(&(24u32 + 100).to_le_bytes());
    hdr[8..10].copy_from_slice(&7u16.to_le_bytes());
    hdr[12..16].copy_from_slice(&512u32.to_le_bytes());
    hdr[16..20].copy_from_slice(&100u32.to_le_bytes());
    let ch = pdu::parse_common(&hdr).unwrap();
    let c2h = pdu::parse_c2h_data(ch, &hdr).expect("clean c2h parses");
    assert_eq!(
        (c2h.cccid, c2h.datao, c2h.datal, c2h.last, c2h.success),
        (7, 512, 100, true, true)
    );

    // plen != pdo + datal is a framing violation.
    let mut bad = hdr;
    bad[4..8].copy_from_slice(&(24u32 + 99).to_le_bytes());
    let ch = pdu::parse_common(&bad).unwrap();
    assert!(matches!(
        pdu::parse_c2h_data(ch, &bad),
        Err(pdu::FrameError::Geometry(_))
    ));
}

#[test]
fn test_codec_cqe_status_strips_phase_bit() {
    let mut b = [0u8; 16];
    b[12..14].copy_from_slice(&0xABCDu16.to_le_bytes());
    b[14..16].copy_from_slice(&((0x0002u16 << 1) | 1).to_le_bytes());
    let cqe = pdu::parse_cqe(&b).unwrap();
    assert_eq!(cqe.cid, 0xABCD);
    assert_eq!(cqe.status, 0x0002, "phase bit must not leak into status");
}

// ------------------------------------------------------------- probe contracts

#[test]
fn test_probe_sysfs_discovery_tcp_and_refusals() {
    let root = tempfile::tempdir().unwrap();
    let ctrl = root.path().join("class/nvme/nvme4");
    let blk = root.path().join("block/nvme4n1/queue");
    std::fs::create_dir_all(&ctrl).unwrap();
    std::fs::create_dir_all(&blk).unwrap();
    std::fs::write(ctrl.join("transport"), "tcp\n").unwrap();
    std::fs::write(ctrl.join("address"), "traddr=10.181.177.191,trsvcid=4420\n").unwrap();
    std::fs::write(ctrl.join("subsysnqn"), "nqn.test:sub\n").unwrap();
    std::fs::write(root.path().join("block/nvme4n1/nsid"), "1\n").unwrap();
    std::fs::write(blk.join("logical_block_size"), "512\n").unwrap();
    std::fs::write(blk.join("max_hw_sectors_kb"), "4096\n").unwrap();

    let t = probe::nvme_tcp_target_for_with_root("/dev/nvme4n1", root.path())
        .expect("tcp device resolves");
    assert_eq!(t.traddr, "10.181.177.191");
    assert_eq!(t.trsvcid, "4420");
    assert_eq!(t.subnqn, "nqn.test:sub");
    assert_eq!(t.nsid, 1);
    assert_eq!(t.lba_shift, 9);
    // 4 MiB device limit > the lane's own 1 MiB per-command cap ⇒ capped
    // (the sentinel-fix law; bounded-below-cap devices keep theirs — see
    // test_probe_unlimited_mdts_sentinel_arms_with_saturated_xfer_cap).
    assert_eq!(t.max_xfer_bytes, probe::LANE_MAX_XFER_CAP_BYTES);
    assert!(t.io_queues >= 1 && t.queue_depth >= 4, "derived geometry");

    // Non-tcp transport is ineligible.
    std::fs::write(ctrl.join("transport"), "pcie\n").unwrap();
    assert!(
        probe::nvme_tcp_target_for_with_root("/dev/nvme4n1", root.path()).is_none(),
        "pcie transport must be ineligible"
    );
    std::fs::write(ctrl.join("transport"), "tcp\n").unwrap();

    // Paths that are not plain nvme<C>n<N> are ineligible.
    for p in ["/dev/sda", "/dev/nvme4", "/dev/nvme4n1p2", "/dev/nvme4c4n1"] {
        assert!(
            probe::nvme_tcp_target_for_with_root(p, root.path()).is_none(),
            "{p} must be ineligible"
        );
    }
}

/// The 2026-08-04 squeeze-test field refusal: fabrics controllers with
/// UNLIMITED MDTS advertise `max_hw_sectors_kb = 2147483644` (the
/// i32::MAX-class sentinel — measured verbatim on every nvme-tcp data
/// controller of the field fleet). The probe computed `max_kb * 1024`
/// in u32, `checked_mul` overflowed, and the `?` silently refused —
/// `zcrx_lane_armed` stayed 0 on every REAL fabrics box while the
/// bounded dev-substrate backings (null_blk/zram) never produced the
/// sentinel. The transfer cap must SATURATE to the lane's own
/// per-command bound instead: a huge device limit means the LANE's
/// window is the binding constraint, never a refusal.
#[test]
fn test_probe_unlimited_mdts_sentinel_arms_with_saturated_xfer_cap() {
    let root = tempfile::tempdir().unwrap();
    let ctrl = root.path().join("class/nvme/nvme26");
    let blk = root.path().join("block/nvme26n1/queue");
    std::fs::create_dir_all(&ctrl).unwrap();
    std::fs::create_dir_all(&blk).unwrap();
    std::fs::write(ctrl.join("transport"), "tcp\n").unwrap();
    std::fs::write(
        ctrl.join("address"),
        "traddr=10.181.177.196,trsvcid=4420,src_addr=10.181.177.194\n",
    )
    .unwrap();
    std::fs::write(
        ctrl.join("subsysnqn"),
        "nqn.2026-07.io.squeezefs:aqs39-d0\n",
    )
    .unwrap();
    std::fs::write(root.path().join("block/nvme26n1/nsid"), "1\n").unwrap();
    std::fs::write(blk.join("logical_block_size"), "4096\n").unwrap();
    // The field sentinel, byte-for-byte.
    std::fs::write(blk.join("max_hw_sectors_kb"), "2147483644\n").unwrap();

    let t = probe::nvme_tcp_target_for_with_root("/dev/nvme26n1", root.path())
        .expect("an unlimited-MDTS fabrics controller must arm, not refuse");
    assert_eq!(
        t.max_xfer_bytes,
        probe::LANE_MAX_XFER_CAP_BYTES,
        "the sentinel saturates to the lane's own per-command cap"
    );
    assert_eq!(t.lba_shift, 12);

    // A bounded device below the cap keeps its own limit verbatim.
    std::fs::write(blk.join("max_hw_sectors_kb"), "128\n").unwrap();
    let t = probe::nvme_tcp_target_for_with_root("/dev/nvme26n1", root.path())
        .expect("bounded device resolves");
    assert_eq!(t.max_xfer_bytes, 128 * 1024, "bounded limit kept verbatim");
}

// -------------------------------------------------- association + read laws

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn test_association_and_multi_pdu_read() {
    let mock = MockTarget::start(MockCfg::default(), 1 << 20).await;
    let sess = LaneSession::connect(mock.target(2, 8)).await.expect("arm");
    assert!(!sess.poisoned());

    // 512 KiB read spans multiple sub-commands? (max_xfer 256 KiB ⇒ 2) and
    // each sub-command reassembles from 8 KiB C2HData PDUs.
    let mut buf = vec![0u8; 512 * 1024];
    sess.read_into_slice(4096, &mut buf).await.expect("read");
    assert_eq!(
        &buf[..],
        &mock.device[4096..4096 + 512 * 1024],
        "reassembled bytes must match the namespace"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn test_success_flag_elision_completes() {
    let cfg = MockCfg {
        success_elision: true,
        ..Default::default()
    };
    let mock = MockTarget::start(cfg, 1 << 20).await;
    let sess = LaneSession::connect(mock.target(1, 4)).await.expect("arm");
    let mut buf = vec![0u8; 64 * 1024];
    sess.read_into_slice(0, &mut buf).await.expect("read");
    assert_eq!(&buf[..], &mock.device[..64 * 1024]);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn test_c2h_pdo_padding_is_skipped_and_priced() {
    let cfg = MockCfg {
        c2h_pdo_pad: 8,
        ..Default::default()
    };
    let mock = MockTarget::start(cfg, 1 << 20).await;
    let before = zcrx_metric("hdr_copy_bytes");
    let sess = LaneSession::connect(mock.target(1, 4)).await.expect("arm");
    let mut buf = vec![0u8; 64 * 1024];
    sess.read_into_slice(0, &mut buf).await.expect("read");
    assert_eq!(&buf[..], &mock.device[..64 * 1024]);
    assert!(
        zcrx_metric("hdr_copy_bytes") > before,
        "the header edge copy must be priced"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn test_digest_demand_refuses_arm_loud() {
    let cfg = MockCfg {
        icresp_digest: 0x3,
        ..Default::default()
    };
    let mock = MockTarget::start(cfg, 4096).await;
    let err = LaneSession::connect(mock.target(1, 4))
        .await
        .expect_err("digest demand must refuse the arm");
    assert!(
        err.to_string().contains("digest"),
        "refusal must name the law: {err}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn test_error_status_fails_op_not_session() {
    let cfg = MockCfg {
        fail_status: Some(0x0281), // e.g. LBA out of range class
        ..Default::default()
    };
    let mock = MockTarget::start(cfg, 1 << 20).await;
    let sess = LaneSession::connect(mock.target(1, 4)).await.expect("arm");
    let mut buf = vec![0u8; 4096];
    let err = sess
        .read_into_slice(0, &mut buf)
        .await
        .expect_err("error status must surface");
    assert!(err.to_string().contains("status"), "{err}");
    assert!(
        !sess.poisoned(),
        "a per-op controller error is not a session poison"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn test_frame_violation_poisons_session_and_counts() {
    let cfg = MockCfg {
        corrupt_datao: true,
        ..Default::default()
    };
    let mock = MockTarget::start(cfg, 1 << 20).await;
    let before = zcrx_metric("frame_violations");
    let sess = LaneSession::connect(mock.target(1, 4)).await.expect("arm");
    let mut buf = vec![0u8; 8192];
    let err = sess
        .read_into_slice(0, &mut buf)
        .await
        .expect_err("frame violation must fail the op");
    assert!(!err.to_string().is_empty());
    // Poison propagates (reader task observed the violation).
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        while !sess.poisoned() {
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("session must poison after a framing violation");
    assert!(
        zcrx_metric("frame_violations") > before,
        "the tripwire must count"
    );
    // Subsequent ops refuse fast.
    assert!(sess.read_into_slice(0, &mut buf).await.is_err());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn test_concurrent_reads_complete_by_cid() {
    let mock = MockTarget::start(MockCfg::default(), 4 << 20).await;
    let sess = LaneSession::connect(mock.target(2, 8)).await.expect("arm");
    let mut handles = Vec::new();
    for i in 0..16u64 {
        let sess = Arc::clone(&sess);
        let dev = Arc::clone(&mock.device);
        handles.push(tokio::spawn(async move {
            let off = i * 128 * 1024;
            let mut buf = vec![0u8; 128 * 1024];
            sess.read_into_slice(off, &mut buf).await.expect("read");
            assert_eq!(
                &buf[..],
                &dev[off as usize..off as usize + 128 * 1024],
                "cid-matched completion must land the right region"
            );
        }));
    }
    for h in handles {
        h.await.expect("no leaked/panicked read tasks");
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn test_unaligned_range_is_ineligible() {
    let mock = MockTarget::start(MockCfg::default(), 1 << 20).await;
    let sess = LaneSession::connect(mock.target(1, 4)).await.expect("arm");
    assert!(!sess.range_eligible(1, 512), "unaligned offset");
    assert!(!sess.range_eligible(512, 100), "unaligned length");
    assert!(!sess.range_eligible(0, 0), "empty range");
    assert!(sess.range_eligible(512, 512));
    let mut buf = vec![0u8; 100];
    assert!(
        sess.read_into_slice(512, &mut buf).await.is_err(),
        "ineligible ranges must refuse (the funnel routes them to the kernel path)"
    );
}

// ------------------------------------------------------- funnel wire-in laws

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn test_disarmed_default_is_byte_identical_and_gauge_silent() {
    std::env::remove_var("SQUEEZEFS_ZCRX_LANE");
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("blockfile");
    let mut content = vec![0u8; 256 * 1024];
    for (i, b) in content.iter_mut().enumerate() {
        *b = (i % 253) as u8;
    }
    std::fs::write(&path, &content).unwrap();

    let before_fills = zcrx_metric("fills");
    let before_armed = zcrx_metric("armed");
    let dev = squeezefs::nvme_dev::NvmeBlockDev::new(path.to_str().unwrap());
    let got = dev.read_block(4096, 65536).await.expect("kernel-path read");
    assert_eq!(&got[..], &content[4096..4096 + 65536]);
    assert_eq!(
        zcrx_metric("fills"),
        before_fills,
        "disarmed mount must never touch the lane"
    );
    assert_eq!(zcrx_metric("armed"), before_armed);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn test_funnel_engagement_via_target_override() {
    // Arm the lane at a mock target through the funnel: the device node is a
    // plain file (kernel path would serve file bytes); the lane serves MOCK
    // namespace bytes — differing content proves engagement structurally,
    // and the gauges must account for it.
    let mock = MockTarget::start(MockCfg::default(), 4 << 20).await;
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("blockfile");
    std::fs::write(&path, vec![0xEEu8; 4 << 20]).unwrap();

    std::env::set_var("SQUEEZEFS_ZCRX_LANE", "1");
    std::env::set_var("SQUEEZEFS_ZCRX_LANE_FORCE_COPY", "1");
    std::env::set_var(
        "SQUEEZEFS_ZCRX_LANE_TARGET",
        format!(
            "127.0.0.1,{},{},1,{},262144,2,8",
            mock.port, MOCK_SUBNQN, MOCK_LBA_SHIFT
        ),
    );

    let before_fills = zcrx_metric("fills");
    let before_bytes = zcrx_metric("fill_bytes");
    let dev = squeezefs::nvme_dev::NvmeBlockDev::new(path.to_str().unwrap());
    let got = dev.read_block(8192, 128 * 1024).await.expect("lane read");

    std::env::remove_var("SQUEEZEFS_ZCRX_LANE");
    std::env::remove_var("SQUEEZEFS_ZCRX_LANE_FORCE_COPY");
    std::env::remove_var("SQUEEZEFS_ZCRX_LANE_TARGET");

    assert_eq!(
        &got[..],
        &mock.device[8192..8192 + 128 * 1024],
        "the lane must have served the read (namespace bytes, not file bytes)"
    );
    assert_eq!(zcrx_metric("fills"), before_fills + 1, "fills gauge");
    assert_eq!(
        zcrx_metric("fill_bytes"),
        before_bytes + 128 * 1024,
        "fill-bytes engagement gauge"
    );
    assert_eq!(zcrx_metric("armed"), 1, "armed gauge");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn test_funnel_gauge_closure_byte_exact() {
    // Closure law (design §9): over any run, `zcrx_fill_bytes` must account
    // byte-exactly for every lane-served fill, and `zcrx_fills` for every
    // funnel read — the engagement instrument the field bracket keys on.
    let mock = MockTarget::start(MockCfg::default(), 8 << 20).await;
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("blockfile");
    std::fs::write(&path, vec![0u8; 8 << 20]).unwrap();

    std::env::set_var("SQUEEZEFS_ZCRX_LANE", "1");
    std::env::set_var("SQUEEZEFS_ZCRX_LANE_FORCE_COPY", "1");
    std::env::set_var(
        "SQUEEZEFS_ZCRX_LANE_TARGET",
        format!(
            "127.0.0.1,{},{},1,{},262144,2,8",
            mock.port, MOCK_SUBNQN, MOCK_LBA_SHIFT
        ),
    );

    let before_fills = zcrx_metric("fills");
    let before_bytes = zcrx_metric("fill_bytes");
    let before_fallbacks = zcrx_metric("fallbacks");
    let dev = squeezefs::nvme_dev::NvmeBlockDev::new(path.to_str().unwrap());
    // Varied sizes incl. > max_xfer (sub-command split must not double-count).
    let sizes = [4096usize, 512 * 1024, 1 << 20, 65536];
    let mut expect = 0u64;
    for (i, sz) in sizes.iter().enumerate() {
        let off = (i as u64) * (2 << 20);
        let got = dev.read_block(off, *sz).await.expect("lane read");
        assert_eq!(
            &got[..],
            &mock.device[off as usize..off as usize + sz],
            "content law"
        );
        expect += *sz as u64;
    }

    std::env::remove_var("SQUEEZEFS_ZCRX_LANE");
    std::env::remove_var("SQUEEZEFS_ZCRX_LANE_FORCE_COPY");
    std::env::remove_var("SQUEEZEFS_ZCRX_LANE_TARGET");

    assert_eq!(
        zcrx_metric("fill_bytes") - before_bytes,
        expect,
        "fill_bytes must close byte-exact against served fills"
    );
    assert_eq!(
        zcrx_metric("fills") - before_fills,
        sizes.len() as u64,
        "one fill per funnel read regardless of sub-command split"
    );
    assert_eq!(
        zcrx_metric("fallbacks"),
        before_fallbacks,
        "a clean run pays zero fallbacks"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn test_funnel_lane_error_falls_back_to_kernel_path() {
    // Per-op fallback law (design §6): a lane error surfaces NOWHERE — the
    // kernel path serves the read (idempotent), and the retry is counted in
    // `zcrx_fill_fallbacks`.
    let cfg = MockCfg {
        fail_status: Some(0x0281),
        ..Default::default()
    };
    let mock = MockTarget::start(cfg, 1 << 20).await;
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("blockfile");
    let content: Vec<u8> = (0..1 << 20).map(|i| (i % 241) as u8).collect();
    std::fs::write(&path, &content).unwrap();

    std::env::set_var("SQUEEZEFS_ZCRX_LANE", "1");
    std::env::set_var("SQUEEZEFS_ZCRX_LANE_FORCE_COPY", "1");
    std::env::set_var(
        "SQUEEZEFS_ZCRX_LANE_TARGET",
        format!(
            "127.0.0.1,{},{},1,{},262144,1,4",
            mock.port, MOCK_SUBNQN, MOCK_LBA_SHIFT
        ),
    );

    let before_fallbacks = zcrx_metric("fallbacks");
    let before_fills = zcrx_metric("fills");
    let dev = squeezefs::nvme_dev::NvmeBlockDev::new(path.to_str().unwrap());
    let got = dev
        .read_block(4096, 65536)
        .await
        .expect("the op must succeed via the kernel path");

    std::env::remove_var("SQUEEZEFS_ZCRX_LANE");
    std::env::remove_var("SQUEEZEFS_ZCRX_LANE_FORCE_COPY");
    std::env::remove_var("SQUEEZEFS_ZCRX_LANE_TARGET");

    assert_eq!(
        &got[..],
        &content[4096..4096 + 65536],
        "kernel path must serve the FILE bytes (fallback engaged)"
    );
    assert!(
        zcrx_metric("fallbacks") > before_fallbacks,
        "the fallback must be counted"
    );
    assert_eq!(
        zcrx_metric("fills"),
        before_fills,
        "a failed lane op is not a fill"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn test_funnel_arm_refusal_without_force_copy_stays_kernel_path() {
    // SQUEEZEFS_ZCRX_LANE=1 alone (no FORCE_COPY seam): PR Z1 must refuse to
    // arm (zcrx backend not shipped) and serve the kernel path byte-identical.
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("blockfile");
    let content = vec![0xABu8; 1 << 20];
    std::fs::write(&path, &content).unwrap();

    std::env::set_var("SQUEEZEFS_ZCRX_LANE", "1");
    std::env::remove_var("SQUEEZEFS_ZCRX_LANE_FORCE_COPY");
    std::env::remove_var("SQUEEZEFS_ZCRX_LANE_TARGET");

    let before_fills = zcrx_metric("fills");
    let dev = squeezefs::nvme_dev::NvmeBlockDev::new(path.to_str().unwrap());
    let got = dev.read_block(0, 65536).await.expect("kernel-path read");

    std::env::remove_var("SQUEEZEFS_ZCRX_LANE");

    assert_eq!(&got[..], &content[..65536]);
    assert_eq!(
        zcrx_metric("fills"),
        before_fills,
        "refused arm must not serve through the lane"
    );
}

// ================================================================ PR Z2 laws
// The zcrx recv backend behind the funnel (design §5/§7/§8): area + refill
// discipline, the gather law, the poison lattice, R5 area budgeting. The
// AREA-SIM seam runs the REAL chunk-parser / ledger / gather machinery with
// socket recv standing in for NIC DMA — the io_uring surface itself is the
// field window's (no zcrx-capable NIC exists locally, stated in the Z2
// evidence note).

#[test]
fn test_z2_area_sizing_derivation_laws() {
    // Design §8: per-queue area = depth × max_xfer rounded up to PMD,
    // floor one PMD — DERIVED, no fixed constants.
    let pmd = area::PMD_BYTES;
    assert_eq!(area::area_bytes_per_queue(8, 262_144), 2 * 1024 * 1024);
    assert_eq!(
        area::area_bytes_per_queue(4, 4_096),
        pmd,
        "tiny windows floor at one PMD"
    );
    assert_eq!(
        area::area_bytes_per_queue(64, 1 << 20),
        64 * 1024 * 1024,
        "already PMD-aligned windows pass through"
    );
    assert_eq!(
        area::area_bytes_per_queue(9, 262_144),
        4 * 1024 * 1024,
        "non-multiple rounds UP to the next PMD"
    );
    let mut last = 0;
    for depth in [4u16, 8, 16, 32, 64] {
        let a = area::area_bytes_per_queue(depth, 262_144);
        assert!(a >= last, "monotonic in depth");
        assert_eq!(a % pmd, 0, "always PMD-granular");
        last = a;
    }

    // Refill ring: 1:1 with area chunks, next pow2 (§8).
    assert_eq!(area::rq_entries_for(512), 512);
    assert_eq!(area::rq_entries_for(513), 1024);
    for chunks in [512u64, 640, 1024, 4096] {
        let e = area::rq_entries_for(chunks);
        assert!(e.is_power_of_two(), "kernel requires pow2");
        assert!(e as u64 >= chunks, "never fewer entries than chunks");
    }
}

#[test]
fn test_z2_arm_admission_red_blocks_new_arms() {
    use squeezefs::mem_budget::Level;
    assert!(squeezefs::zcrx_lane::arm_admission(Level::Green));
    assert!(squeezefs::zcrx_lane::arm_admission(Level::Yellow));
    assert!(
        !squeezefs::zcrx_lane::arm_admission(Level::Red),
        "R5 Red must block NEW lane arms (design §7)"
    );
}

#[test]
fn test_z2_r5_component_registration_idempotent_and_gauged() {
    area::register_r5_component();
    area::register_r5_component(); // idempotent — never a duplicate entry
    let comps = squeezefs::mem_budget::MEM_BUDGET.stats_components();
    let mine: Vec<_> = comps.iter().filter(|c| c.0 == "zcrx_area").collect();
    assert_eq!(mine.len(), 1, "exactly one zcrx_area component: {comps:?}");
    let (_, current, _floor, _weight, sheds) = *mine[0];
    assert_eq!(
        current,
        zcrx_metric("area_bytes"),
        "the component's gauge IS the zcrx_area_bytes metric"
    );
    assert_eq!(sheds, 0, "non-sheddable: the shed hook must be a no-op");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn test_z2_area_sim_multi_pdu_content_and_full_recycle() {
    // The gather law's contract venue: multi-PDU + PDO pad + sub-command
    // split, headers split across TINY chunks (256 B — far below any PDU),
    // content byte-exact, and EVERY chunk recycled once the ops complete
    // (refill discipline: chunk return on last ref drop).
    std::env::set_var("SQUEEZEFS_ZCRX_LANE_SIM_CHUNK", "256");
    let cfg = MockCfg {
        c2h_span: 8192,
        c2h_pdo_pad: 8,
        ..Default::default()
    };
    let mock = MockTarget::start(cfg, 2 << 20).await;
    let before_hdr = zcrx_metric("hdr_copy_bytes");
    let before_gather = zcrx_metric("gather_bytes");
    let sess = LaneSession::connect_with(mock.target(2, 8), LaneBackend::AreaSim)
        .await
        .expect("area-sim arm");
    let (_, total) = sess.area_chunks();
    assert!(total > 0, "area backend must own chunks");

    let mut buf = vec![0u8; 512 * 1024];
    sess.read_into_slice(4096, &mut buf).await.expect("read");
    assert_eq!(
        &buf[..],
        &mock.device[4096..4096 + 512 * 1024],
        "reassembled bytes must match the namespace across chunk seams"
    );
    assert!(
        zcrx_metric("hdr_copy_bytes") > before_hdr,
        "header edge copy must be priced"
    );
    assert_eq!(
        zcrx_metric("gather_bytes") - before_gather,
        512 * 1024,
        "the ONE completion gather pass is priced byte-exactly"
    );

    // Refill discipline: with ops complete and fills dropped, every chunk
    // is back in the free set (the sim reader grants only when readable).
    sess.quiesce().await;
    let (free, total) = sess.area_chunks();
    assert_eq!(free, total, "all chunks recycled after ops complete");
    std::env::remove_var("SQUEEZEFS_ZCRX_LANE_SIM_CHUNK");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn test_z2_area_sim_funnel_engagement_and_gather_closure() {
    // Funnel-level engagement (the field bracket's instrument): AREA_SIM
    // env arms through `arm_for_device`; lane serves MOCK namespace bytes
    // (≠ file bytes — structural engagement); gauges close byte-exact.
    let mock = MockTarget::start(MockCfg::default(), 8 << 20).await;
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("blockfile");
    std::fs::write(&path, vec![0xEEu8; 8 << 20]).unwrap();

    let env = LaneEnv::area_sim(&mock, 2, 8);
    let before_fills = zcrx_metric("fills");
    let before_bytes = zcrx_metric("fill_bytes");
    let before_gather = zcrx_metric("gather_bytes");
    let before_fallbacks = zcrx_metric("fallbacks");
    let dev = squeezefs::nvme_dev::NvmeBlockDev::new(path.to_str().unwrap());
    let sizes = [4096usize, 512 * 1024, 1 << 20, 65536];
    let mut expect = 0u64;
    for (i, sz) in sizes.iter().enumerate() {
        let off = (i as u64) * (2 << 20);
        let got = dev.read_block(off, *sz).await.expect("lane read");
        assert_eq!(
            &got[..],
            &mock.device[off as usize..off as usize + sz],
            "the lane must have served the read (namespace bytes, not file bytes)"
        );
        expect += *sz as u64;
    }
    assert!(
        zcrx_metric("area_bytes") > 0,
        "armed area must be gauge-visible (the R5 component's source)"
    );
    drop(env);

    assert_eq!(
        zcrx_metric("fill_bytes") - before_bytes,
        expect,
        "fill_bytes closes byte-exact against served fills"
    );
    assert_eq!(
        zcrx_metric("gather_bytes") - before_gather,
        expect,
        "Z2 gather closure: every fill byte pays exactly one gather pass"
    );
    assert_eq!(
        zcrx_metric("fills") - before_fills,
        sizes.len() as u64,
        "one fill per funnel read regardless of sub-command split"
    );
    assert_eq!(zcrx_metric("fallbacks"), before_fallbacks, "clean run");
    assert_eq!(zcrx_metric("armed"), 1, "armed gauge");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn test_z2_area_exhaustion_backpressures_admission_never_deadlocks() {
    // Design §4.3/§5 as amended by ROUND 8: area exhaustion bounds
    // in-flight fills at COMMAND admission — and over-demand DECLINES
    // to the kernel path immediately (the round-5 park-until-permits
    // law this test used to pin was the field's starvation churn: 648
    // waits / 206 fills feeding 41 × 937 ms episodes of held reads).
    // The invariants: every read either completes byte-correct or
    // declines loud-and-fast; at least the window's worth completes;
    // declines are visible in the gauge; nothing deadlocks; the
    // quiesced area fully recycles.
    let mock = MockTarget::start(MockCfg::default(), 8 << 20).await;
    let sess = LaneSession::connect_with(mock.target(1, 16), LaneBackend::AreaSim)
        .await
        .expect("area-sim arm");
    let before_declines = zcrx_metric("admission_waits");
    let mut handles = Vec::new();
    for i in 0..16u64 {
        let sess = Arc::clone(&sess);
        let dev = Arc::clone(&mock.device);
        handles.push(tokio::spawn(async move {
            let off = i * 512 * 1024;
            let mut buf = vec![0u8; 512 * 1024];
            match sess.read_into_slice(off, &mut buf).await {
                Ok(()) => {
                    assert_eq!(
                        &buf[..],
                        &dev[off as usize..off as usize + 512 * 1024],
                        "an ADMITTED read must complete byte-correct"
                    );
                    true
                }
                Err(e) => {
                    assert!(
                        format!("{e}").contains("declined"),
                        "an unadmitted read must DECLINE (never park, never \
                         a foreign error): {e}"
                    );
                    false
                }
            }
        }));
    }
    let mut completed = 0usize;
    for h in handles {
        if h.await
            .expect("no wedged/panicked read under area pressure")
        {
            completed += 1;
        }
    }
    // Floor derivation (whole-read ATOMIC admission, finding I): area =
    // depth 16 × 256 KiB max_xfer = 4 MiB (PMD-exact); the sim fill
    // window is the whole area, so `admission_permits` = 4 MiB / 2 =
    // 2 MiB = 512 × 4 KiB units. One 512 KiB read = two 256 KiB
    // segments = 128 units, taken in ONE try-acquire (single queue).
    // Every holder therefore holds exactly 128 units, so a decline
    // (available < 128 ⇒ held > 384) can only be witnessed while
    // 512 / 128 = FOUR whole reads hold the window simultaneously — and
    // every admitted read completes byte-correct on this healthy mock.
    // Either nothing declines (all 16 complete) or ≥ 4 complete: the
    // floor is the geometry's arithmetic, deterministic, never a tuned
    // constant. (Per-SEGMENT admission broke exactly this: racing reads
    // held one segment while the sibling declined, and the observed
    // floor collapsed to 2–3 of 16.)
    assert!(
        completed >= 4,
        "at least the admission window's worth of reads completes \
         (window 512 units / 128 units per read = 4; got {completed})"
    );
    if completed < 16 {
        assert!(
            zcrx_metric("admission_waits") > before_declines,
            "declines are visible in the gauge"
        );
    }
    sess.quiesce().await;
    let (free, total) = sess.area_chunks();
    assert_eq!(free, total, "quiesced area fully recycled");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn test_z2_mid_flight_death_poisons_drains_and_falls_back() {
    // Poison lattice (design §7 + task law): mid-flight connection death →
    // session poisons LOUD, in-flight drains, the op retries on the kernel
    // path (file bytes served), `zcrx_lane_poisoned` counts, armed drops,
    // and subsequent reads never touch the lane again this mount.
    let cfg = MockCfg {
        die_mid_c2h: true,
        ..Default::default()
    };
    let mock = MockTarget::start(cfg, 1 << 20).await;
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("blockfile");
    let content: Vec<u8> = (0..1 << 20).map(|i| (i % 239) as u8).collect();
    std::fs::write(&path, &content).unwrap();

    let env = LaneEnv::area_sim(&mock, 1, 4);
    let before_poisoned = zcrx_metric("poisoned");
    let before_fallbacks = zcrx_metric("fallbacks");
    let before_fills = zcrx_metric("fills");
    let dev = squeezefs::nvme_dev::NvmeBlockDev::new(path.to_str().unwrap());
    let got = dev
        .read_block(4096, 65536)
        .await
        .expect("the op must succeed via the kernel path");
    assert_eq!(
        &got[..],
        &content[4096..4096 + 65536],
        "kernel path must serve the FILE bytes after the lane died"
    );
    assert_eq!(
        zcrx_metric("poisoned") - before_poisoned,
        1,
        "exactly one poison transition per session death"
    );
    assert!(
        zcrx_metric("fallbacks") > before_fallbacks,
        "the fallback retry is counted"
    );
    assert_eq!(zcrx_metric("armed"), 0, "poison disarms the armed gauge");

    // Second read: lane is dead for the mount lifetime — kernel path only,
    // no new lane activity, no second poison.
    let got2 = dev.read_block(0, 4096).await.expect("kernel path");
    assert_eq!(&got2[..], &content[..4096]);
    assert_eq!(zcrx_metric("fills"), before_fills, "no lane fills ever");
    assert_eq!(
        zcrx_metric("poisoned") - before_poisoned,
        1,
        "poison transition counted exactly once"
    );
    drop(env);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn test_z2_frame_violation_in_area_mode_poisons_and_recycles() {
    // The Z1 framing law re-pinned on the AREA path: corrupt datao →
    // violation tripwire + session poison + poisoned counter; quiescence
    // releases every chunk ref (no zombie holds into recycled memory).
    let cfg = MockCfg {
        corrupt_datao: true,
        ..Default::default()
    };
    let mock = MockTarget::start(cfg, 1 << 20).await;
    let before_viol = zcrx_metric("frame_violations");
    let before_poisoned = zcrx_metric("poisoned");
    let sess = LaneSession::connect_with(mock.target(1, 4), LaneBackend::AreaSim)
        .await
        .expect("area-sim arm");
    let mut buf = vec![0u8; 8192];
    let err = sess
        .read_into_slice(0, &mut buf)
        .await
        .expect_err("frame violation must fail the op");
    assert!(!err.to_string().is_empty());
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        while !sess.poisoned() {
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("session must poison after a framing violation");
    assert!(zcrx_metric("frame_violations") > before_viol, "tripwire");
    assert!(zcrx_metric("poisoned") > before_poisoned, "poison counted");

    sess.quiesce().await;
    let (free, total) = sess.area_chunks();
    assert_eq!(
        free, total,
        "poison quiescence must release every chunk ref"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn test_z2_classic_backend_leaves_area_gauges_silent() {
    // Backend distinction law: the classic (Z1 contract) backend owns no
    // area — its rows must never move the Z2 area/gather gauges.
    let mock = MockTarget::start(MockCfg::default(), 1 << 20).await;
    let before_gather = zcrx_metric("gather_bytes");
    let before_area = zcrx_metric("area_bytes");
    let before_waits = zcrx_metric("admission_waits");
    let sess = LaneSession::connect(mock.target(1, 4)).await.expect("arm");
    let mut buf = vec![0u8; 64 * 1024];
    sess.read_into_slice(0, &mut buf).await.expect("read");
    assert_eq!(&buf[..], &mock.device[..64 * 1024]);
    assert_eq!((sess.area_chunks()), (0, 0), "classic backend has no area");
    assert_eq!(zcrx_metric("gather_bytes"), before_gather);
    assert_eq!(zcrx_metric("area_bytes"), before_area);
    assert_eq!(zcrx_metric("admission_waits"), before_waits);
}

// ================================================================ MEM-3 laws
// Cancellation safety (pre-rc spec §2 MEM-3 — the D5 gate chain's first
// link): dropping a requester future mid-op must (a) never let the
// destination allocation recycle while a lane context can still write it,
// and (b) never leak the op's CID — after `queue_depth` cancellations the
// pool would be empty and the lane silently degraded for the mount
// lifetime. Cancellation is NOT a poison event: `zcrx_lane_poisoned` is a
// must-stay-0 tripwire and routine future-drops must keep it honest.

/// Poll `cond` until true or `secs` elapse (suite idiom — the existing
/// poison-propagation tests poll the same way).
async fn poll_true(secs: u64, mut cond: impl FnMut() -> bool) -> bool {
    tokio::time::timeout(std::time::Duration::from_secs(secs), async {
        while !cond() {
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    })
    .await
    .is_ok()
}

/// Keep-alive canary: flips `flag` when the last `Bytes` clone drops —
/// the observable for "the lane held the destination allocation".
struct DropFlagOwner {
    buf: Vec<u8>,
    flag: Arc<std::sync::atomic::AtomicBool>,
}

impl AsRef<[u8]> for DropFlagOwner {
    fn as_ref(&self) -> &[u8] {
        &self.buf
    }
}

impl Drop for DropFlagOwner {
    fn drop(&mut self) {
        self.flag.store(true, std::sync::atomic::Ordering::SeqCst);
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn test_mem3_classic_cancellation_returns_cids_and_lane_survives() {
    let gate = Arc::new(tokio::sync::Semaphore::new(0));
    let seen = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let cfg = MockCfg {
        read_gate: Some(Arc::clone(&gate)),
        reads_seen: Some(Arc::clone(&seen)),
        ..Default::default()
    };
    let mock = MockTarget::start(cfg, 1 << 20).await;
    let before_poisoned = zcrx_metric("poisoned");
    // One cancellation per queue (the mock services one connection
    // sequentially, so a gated read parks that connection's parse loop —
    // each cancel needs its own connection to observe the capsule edge).
    let sess = LaneSession::connect(mock.target(4, 2)).await.expect("arm");
    assert_eq!(sess.cid_slots(), (8, 8), "full pool at arm");

    // Cancel one in-flight op per queue: each read's capsule reaches the
    // target (withheld response), then the requester future is dropped.
    // Destination buffers are TEST-OWNED and outlive the whole test so
    // the un-fixed lane cannot scribble on freed memory while proving
    // the CID leak.
    let len = 32 * 1024usize;
    let mut bufs: Vec<Vec<u8>> = (0..4).map(|_| vec![0u8; len]).collect();
    for (i, buf) in bufs.iter_mut().enumerate() {
        let sess2 = Arc::clone(&sess);
        let ptr = buf.as_mut_ptr() as usize;
        let h = tokio::spawn(async move {
            let _ = sess2.read_into_ptr(0, ptr as *mut u8, len).await;
        });
        assert!(
            poll_true(5, || seen.load(std::sync::atomic::Ordering::SeqCst) > i).await,
            "read capsule {i} must reach the target before the cancel"
        );
        h.abort();
        let _ = h.await;
    }

    // Release the withheld completions: once the driver finishes the
    // abandoned ops every CID must return to the pool — a leak here is
    // the mount-lifetime lane degradation MEM-3 names.
    gate.add_permits(64);
    assert!(
        poll_true(5, || sess.cid_slots() == (8, 8)).await,
        "cancelled ops must return their CIDs once the driver completes \
         them (got {:?} of (8, 8))",
        sess.cid_slots()
    );

    // The lane survives at full depth, content-correct, zero poison.
    let mut buf = vec![0u8; 64 * 1024];
    sess.read_into_slice(4096, &mut buf)
        .await
        .expect("post-cancellation read must succeed (no CID/permit desync)");
    assert_eq!(&buf[..], &mock.device[4096..4096 + 64 * 1024]);
    assert_eq!(
        zcrx_metric("poisoned"),
        before_poisoned,
        "cancellation is not a poison event (the must-stay-0 tripwire \
         stays honest under D5 default-on)"
    );
    drop(bufs);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn test_mem3_classic_cancellation_holds_destination_keepalive() {
    let gate = Arc::new(tokio::sync::Semaphore::new(0));
    let seen = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let cfg = MockCfg {
        read_gate: Some(Arc::clone(&gate)),
        reads_seen: Some(Arc::clone(&seen)),
        ..Default::default()
    };
    let mock = MockTarget::start(cfg, 1 << 20).await;
    let before_poisoned = zcrx_metric("poisoned");
    let sess = LaneSession::connect(mock.target(1, 4)).await.expect("arm");

    let flag = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let len = 64 * 1024usize;
    let buf = vec![0u8; len];
    let ptr = buf.as_ptr() as usize; // Vec buffer address is stable across the move below
    let keep = bytes::Bytes::from_owner(DropFlagOwner {
        buf,
        flag: Arc::clone(&flag),
    });

    let sess2 = Arc::clone(&sess);
    let h = tokio::spawn(async move {
        let _ = sess2.read_into_pooled(0, ptr as *mut u8, len, keep).await;
    });
    assert!(
        poll_true(5, || seen.load(std::sync::atomic::Ordering::SeqCst) > 0).await,
        "read capsule must reach the target before the cancel"
    );
    h.abort();
    let _ = h.await;

    // The SendMutPtr custody law: with the requester future dropped and
    // the completion still withheld, the reader task can still write the
    // destination — the lane MUST be holding the keep-alive.
    assert!(
        !flag.load(std::sync::atomic::Ordering::SeqCst),
        "cancellation must not release the destination allocation while \
         a lane context can still write it (MEM-3 recycled-buffer write)"
    );

    // Completion releases custody: the abandoned entry drops, the buffer
    // frees (no leak either), the CID returns, nothing poisoned.
    gate.add_permits(64);
    assert!(
        poll_true(5, || flag.load(std::sync::atomic::Ordering::SeqCst)).await,
        "the abandoned entry must release the keep-alive at completion"
    );
    assert!(
        poll_true(5, || sess.cid_slots() == (4, 4)).await,
        "abandoned op's CID must return (got {:?})",
        sess.cid_slots()
    );
    assert_eq!(zcrx_metric("poisoned"), before_poisoned);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn test_mem3_area_cancellation_returns_cids_chunks_and_stays_clean() {
    // The area lane's face of MEM-3: the driver never writes the
    // destination (the requester gathers), but a dropped requester must
    // still return its CID at driver completion, release every chunk
    // ref (the fill drops in the dead completion channel), and leave
    // admission accounting exact — all with zero poison.
    let gate = Arc::new(tokio::sync::Semaphore::new(0));
    let seen = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let cfg = MockCfg {
        read_gate: Some(Arc::clone(&gate)),
        reads_seen: Some(Arc::clone(&seen)),
        ..Default::default()
    };
    let mock = MockTarget::start(cfg, 2 << 20).await;
    let before_poisoned = zcrx_metric("poisoned");
    // One cancellation per queue (one connection each — see the classic
    // test's venue note).
    let sess = LaneSession::connect_with(mock.target(2, 2), LaneBackend::AreaSim)
        .await
        .expect("area-sim arm");
    assert_eq!(sess.cid_slots(), (4, 4));

    let len = 64 * 1024usize;
    let mut bufs: Vec<Vec<u8>> = (0..2).map(|_| vec![0u8; len]).collect();
    for (i, buf) in bufs.iter_mut().enumerate() {
        let sess2 = Arc::clone(&sess);
        let ptr = buf.as_mut_ptr() as usize;
        let h = tokio::spawn(async move {
            let _ = sess2.read_into_ptr(0, ptr as *mut u8, len).await;
        });
        assert!(
            poll_true(5, || seen.load(std::sync::atomic::Ordering::SeqCst) > i).await,
            "read capsule {i} must reach the target before the cancel"
        );
        h.abort();
        let _ = h.await;
    }

    gate.add_permits(64);
    assert!(
        poll_true(5, || sess.cid_slots() == (4, 4)).await,
        "cancelled area ops must return their CIDs at driver completion \
         (got {:?} of (4, 4))",
        sess.cid_slots()
    );
    assert!(
        poll_true(5, || {
            let (free, total) = sess.area_chunks();
            free == total
        })
        .await,
        "abandoned fills must release every chunk ref (got {:?})",
        sess.area_chunks()
    );

    // Full-depth follow-up read serves content-correct (admission and
    // gate accounting are exact — no permit desync).
    let mut buf = vec![0u8; 128 * 1024];
    sess.read_into_slice(8192, &mut buf)
        .await
        .expect("post-cancellation area read");
    assert_eq!(&buf[..], &mock.device[8192..8192 + 128 * 1024]);
    assert_eq!(
        zcrx_metric("poisoned"),
        before_poisoned,
        "area cancellation is not a poison event"
    );
    drop(bufs);
}

// ================================================================== Z3 laws
// Gather fusion (design §4.4/§10 PR Z3 — pre-rc spec §9 PERF-1): when the
// funnel read carries a registered destination (`dest_addr` — the routing
// raw full-block leg and the R3 ranged zero-copy leg), the lane's ONE
// completion gather lands DIRECTLY in that destination. The Z2
// intermediate (gather → pooled bounce → serve copy) is deleted on this
// shape — the pass the Phase-1 bracket priced at −65–68 % RX CPU. Every
// Z2 law is preserved: gather ≡ fill byte-exact, poison lattice,
// `zcrx_lane_poisoned` must-stay-0, R5 Red arm gate, `SQUEEZEFS_ZCRX_LANE=0`
// kill switch, kernel path byte-identical when unarmed.

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn test_z3_dest_fused_serve_engages_and_closes() {
    // AREA_SIM lane armed through the funnel; a dest-carrying read must be
    // lane-served (mock namespace bytes, not file bytes) with the gather
    // fused into the destination: `zcrx_dest_gather_bytes` accounts the
    // row byte-exactly and gather ≡ fill still closes across sub-command
    // splits (512 KiB = 2 × 256 KiB max_xfer segments into ONE dest).
    let mock = MockTarget::start(MockCfg::default(), 8 << 20).await;
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("blockfile");
    std::fs::write(&path, vec![0xEEu8; 8 << 20]).unwrap();

    let env = LaneEnv::area_sim(&mock, 2, 8);
    let before_fills = zcrx_metric("fills");
    let before_bytes = zcrx_metric("fill_bytes");
    let before_gather = zcrx_metric("gather_bytes");
    let before_dest_gather = zcrx_metric("dest_gather_bytes");
    let before_fallbacks = zcrx_metric("fallbacks");

    let dev = squeezefs::nvme_dev::NvmeBlockDev::new(path.to_str().unwrap());
    // Registered-destination stand-in: a pooled 4 KiB-aligned buffer held
    // alive by the test across the op (the read_block_with_dest contract).
    let (dest_ptr, dest_bytes) = squeezefs::cache::pool::ALIGNED_BUF_POOL.alloc();
    let size = 512 * 1024usize;
    let got = dev
        .read_block_with_dest(8192, size, Some(dest_ptr as u64))
        .await
        .expect("dest read");
    drop(env);

    assert_eq!(
        &got[..size],
        &mock.device[8192..8192 + size],
        "the lane must serve the dest read (namespace bytes, not file \
         bytes) — the fused arm engages on dest_addr shapes"
    );
    // SAFETY: test-owned pooled buffer, op complete.
    let landed = unsafe { std::slice::from_raw_parts(dest_ptr, size) };
    assert_eq!(
        landed,
        &mock.device[8192..8192 + size],
        "the gather must land IN the destination (no intermediate)"
    );
    assert_eq!(zcrx_metric("fills") - before_fills, 1, "one fill per read");
    assert_eq!(
        zcrx_metric("fill_bytes") - before_bytes,
        size as u64,
        "fill-provenance engagement"
    );
    assert_eq!(
        zcrx_metric("gather_bytes") - before_gather,
        size as u64,
        "gather ≡ fill closure preserved under fusion"
    );
    assert_eq!(
        zcrx_metric("dest_gather_bytes") - before_dest_gather,
        size as u64,
        "the fused-serve gauge must account the dest row byte-exactly \
         (Z2 kept this 0 — the intermediate-copy shape)"
    );
    assert_eq!(zcrx_metric("fallbacks"), before_fallbacks, "clean run");
    drop(dest_bytes);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn test_z3_pooled_reads_stay_two_pass_and_dest_gauge_silent() {
    // Backend-shape distinction: pooled (dest-less) funnel reads keep the
    // Z2 shape — `zcrx_gather_bytes` moves, `zcrx_dest_gather_bytes` must
    // NOT (those bytes still pay the serve pass upstream; the fused gauge
    // must never lie about them).
    let mock = MockTarget::start(MockCfg::default(), 4 << 20).await;
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("blockfile");
    std::fs::write(&path, vec![0xEEu8; 4 << 20]).unwrap();

    let env = LaneEnv::area_sim(&mock, 1, 8);
    let before_gather = zcrx_metric("gather_bytes");
    let before_dest_gather = zcrx_metric("dest_gather_bytes");
    let dev = squeezefs::nvme_dev::NvmeBlockDev::new(path.to_str().unwrap());
    let got = dev.read_block(4096, 128 * 1024).await.expect("lane read");
    drop(env);

    assert_eq!(&got[..], &mock.device[4096..4096 + 128 * 1024]);
    assert_eq!(
        zcrx_metric("gather_bytes") - before_gather,
        128 * 1024,
        "pooled reads still pay their ONE gather"
    );
    assert_eq!(
        zcrx_metric("dest_gather_bytes"),
        before_dest_gather,
        "dest-fused gauge stays silent on pooled reads"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn test_z3_dest_reads_never_ride_the_classic_backend() {
    // The classic (FORCE_COPY contract) backend's reader task writes the
    // destination from a foreign task — for a registered dest that is the
    // MEM-1 hazard class, so dest fusion is area-backend-only BY LAW:
    // classic sessions must leave dest reads on the kernel path (file
    // bytes), with no fill, no fallback (ineligibility is not an error).
    let mock = MockTarget::start(MockCfg::default(), 1 << 20).await;
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("blockfile");
    let content: Vec<u8> = (0..1 << 20).map(|i| (i % 249) as u8).collect();
    std::fs::write(&path, &content).unwrap();

    std::env::set_var("SQUEEZEFS_ZCRX_LANE", "1");
    std::env::set_var("SQUEEZEFS_ZCRX_LANE_FORCE_COPY", "1");
    std::env::set_var(
        "SQUEEZEFS_ZCRX_LANE_TARGET",
        format!(
            "127.0.0.1,{},{},1,{},262144,1,4",
            mock.port, MOCK_SUBNQN, MOCK_LBA_SHIFT
        ),
    );

    let before_fills = zcrx_metric("fills");
    let before_fallbacks = zcrx_metric("fallbacks");
    let before_dest_gather = zcrx_metric("dest_gather_bytes");
    let dev = squeezefs::nvme_dev::NvmeBlockDev::new(path.to_str().unwrap());
    let (dest_ptr, dest_bytes) = squeezefs::cache::pool::ALIGNED_BUF_POOL.alloc();
    let size = 64 * 1024usize;
    let got = dev
        .read_block_with_dest(4096, size, Some(dest_ptr as u64))
        .await
        .expect("kernel-path dest read");

    std::env::remove_var("SQUEEZEFS_ZCRX_LANE");
    std::env::remove_var("SQUEEZEFS_ZCRX_LANE_FORCE_COPY");
    std::env::remove_var("SQUEEZEFS_ZCRX_LANE_TARGET");

    assert_eq!(
        &got[..size],
        &content[4096..4096 + size],
        "classic-backend dest reads must stay on the kernel path (FILE bytes)"
    );
    assert_eq!(zcrx_metric("fills"), before_fills, "no lane fill");
    assert_eq!(
        zcrx_metric("fallbacks"),
        before_fallbacks,
        "ineligibility is not a fallback (the ≈0 gauge stays honest)"
    );
    assert_eq!(zcrx_metric("dest_gather_bytes"), before_dest_gather);
    drop(dest_bytes);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn test_z3_dest_lane_error_falls_back_to_kernel_path() {
    // Per-op fallback law on the fused arm (design §6): a lane error on a
    // dest read surfaces NOWHERE — the kernel path serves the read into
    // the SAME destination (idempotent), counted in zcrx_fill_fallbacks.
    let cfg = MockCfg {
        fail_status: Some(0x0281),
        ..Default::default()
    };
    let mock = MockTarget::start(cfg, 1 << 20).await;
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("blockfile");
    let content: Vec<u8> = (0..1 << 20).map(|i| (i % 241) as u8).collect();
    std::fs::write(&path, &content).unwrap();

    let env = LaneEnv::area_sim(&mock, 1, 4);
    let before_fallbacks = zcrx_metric("fallbacks");
    let before_fills = zcrx_metric("fills");
    let before_dest_gather = zcrx_metric("dest_gather_bytes");
    let dev = squeezefs::nvme_dev::NvmeBlockDev::new(path.to_str().unwrap());
    let (dest_ptr, dest_bytes) = squeezefs::cache::pool::ALIGNED_BUF_POOL.alloc();
    let size = 64 * 1024usize;
    let got = dev
        .read_block_with_dest(4096, size, Some(dest_ptr as u64))
        .await
        .expect("the op must succeed via the kernel path");
    drop(env);

    assert_eq!(
        &got[..size],
        &content[4096..4096 + size],
        "kernel path must serve the FILE bytes into the dest after the \
         lane error"
    );
    assert!(
        zcrx_metric("fallbacks") > before_fallbacks,
        "the dest-arm fallback must be counted"
    );
    assert_eq!(
        zcrx_metric("fills"),
        before_fills,
        "a failed op is not a fill"
    );
    assert_eq!(zcrx_metric("dest_gather_bytes"), before_dest_gather);
    drop(dest_bytes);
}

// ------------------------------------------------ mock/nvmet strictness parity

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_mock_refuses_oversize_connect_data_like_nvmet() {
    // The gap that let the 2026-08-04 field failure ship: the mock accepted
    // ANY connect data length, while real nvmet checks the Connect transfer
    // length against `sizeof(struct nvmf_connect_data)` = 1024 and refuses
    // everything else with Data SGL Length Invalid | DNR = 0x400f
    // (`nvmet_check_transfer_len`, fabrics-cmd.c). The tightened mock must
    // refuse the RETIRED 4096-byte wire form with the field's exact status —
    // a mock that stays permissive re-opens the gap.
    let mock = MockTarget::start(MockCfg::default(), 4096).await;
    let mut s = TcpStream::connect(("127.0.0.1", mock.port)).await.unwrap();
    s.write_all(&pdu::encode_icreq()).await.unwrap();
    let mut icresp = [0u8; 128];
    s.read_exact(&mut icresp).await.unwrap();
    pdu::parse_icresp(&icresp).expect("mock ICResp");

    // The retired wire form: stretch the connect data blob to 4096 bytes
    // and re-stamp plen + the in-capsule SGL length (byte-identical to what
    // the pre-fix encoder shipped).
    let mut old =
        pdu::encode_connect_capsule(0, 31, 0, 7, &[9u8; 16], 0xFFFF, MOCK_SUBNQN, "nqn.host");
    old.resize(72 + 4096, 0);
    old[4..8].copy_from_slice(&((72 + 4096) as u32).to_le_bytes());
    old[8 + 32..8 + 36].copy_from_slice(&4096u32.to_le_bytes());
    s.write_all(&old).await.unwrap();

    let mut resp = [0u8; 24];
    s.read_exact(&mut resp).await.unwrap();
    let ch = pdu::parse_common(&resp[..8]).unwrap();
    assert_eq!(ch.pdu_type, pdu::PDU_CAPSULE_RESP);
    let cqe = pdu::parse_cqe(&resp[8..24]).unwrap();
    assert_eq!(cqe.cid, 7);
    assert_eq!(
        cqe.status,
        0x400F,
        "the mock must refuse the oversize connect data the way real nvmet \
         does (Data SGL Length Invalid | DNR), got {}",
        pdu::describe_status(cqe.status)
    );
}

// ----------------------------------------------------- live nvmet-tcp venue

/// Resolve a live NVMe/TCP lane target: the documented dev seam
/// (`SQUEEZEFS_ZCRX_LANE_TARGET`) when set, else the kernel initiator's own
/// sysfs attachment to a devsub-tcp namespace (`tests/dev_substrate.sh`
/// under `SQZ_DEVSUB_TRANSPORT=tcp` — subsystem NQNs
/// `nqn.2026-07.io.squeezefs:devsubtcp-*` on 127.0.0.1). Returns the target
/// plus the kernel block device path when sysfs named one (the byte-parity
/// witness for root runs).
fn live_devsub_target() -> Option<(LaneTarget, Option<String>)> {
    if let Some(t) = squeezefs::zcrx_lane::lane_target_override() {
        return Some((t, None));
    }
    // Walk /sys/block for plain nvme<C>n<N> names (the multipath c-path
    // `nvme<C>c<X>n<N>` children under the controller dir never match —
    // the probe itself is /sys/block + /sys/class/nvme based).
    let mut namespaces: Vec<String> = std::fs::read_dir("/sys/block")
        .ok()?
        .flatten()
        .filter_map(|e| e.file_name().into_string().ok())
        .filter(|n| {
            n.strip_prefix("nvme").is_some_and(|r| {
                r.split_once('n').is_some_and(|(c, ns)| {
                    !c.is_empty()
                        && !ns.is_empty()
                        && c.bytes().all(|b| b.is_ascii_digit())
                        && ns.bytes().all(|b| b.is_ascii_digit())
                })
            })
        })
        .collect();
    namespaces.sort();
    for ns in namespaces {
        let Some((ctrl_num, _)) = ns.strip_prefix("nvme").and_then(|r| r.split_once('n')) else {
            continue;
        };
        let dir = std::path::Path::new("/sys/class/nvme").join(format!("nvme{ctrl_num}"));
        let read = |n: &str| {
            std::fs::read_to_string(dir.join(n))
                .ok()
                .map(|s| s.trim().to_string())
        };
        if read("transport").as_deref() != Some("tcp") {
            continue;
        }
        if !read("subsysnqn")
            .is_some_and(|nqn| nqn.starts_with("nqn.2026-07.io.squeezefs:devsubtcp"))
        {
            continue;
        }
        let dev = format!("/dev/{ns}");
        if let Some(t) = probe::nvme_tcp_target_for(&dev) {
            return Some((t, Some(dev)));
        }
    }
    None
}

/// The standing LOCAL venue for the 2026-08-04 field refusal ("admin Connect
/// failed: controller status 0x400f" on every armable data device): REAL
/// nvmet-tcp — not the mock — must accept the lane's association, and one
/// read must round-trip. The red form of this test reproduced the field
/// failure byte-exactly: the encoder shipped a 4096-byte Connect data blob
/// where the NVMe-oF Connect data is EXACTLY 1024 bytes
/// (`struct nvmf_connect_data`), which nvmet refuses with Data SGL Length
/// Invalid | DNR (`nvmet_check_transfer_len`) while the then-permissive
/// mock accepted it.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn test_live_nvmet_tcp_association_and_read_roundtrip() {
    let Some((mut target, dev_path)) = live_devsub_target() else {
        skip!(
            Hardware,
            "no live devsub-tcp nvmet target (sudo SQZ_DEVSUB_TRANSPORT=tcp \
             tests/dev_substrate.sh create) and no SQUEEZEFS_ZCRX_LANE_TARGET \
             override"
        );
    };
    // Test-scoped geometry: bounded bring-up on the shared substrate.
    target.io_queues = 2;
    target.queue_depth = 8;
    let sess = LaneSession::connect(target.clone())
        .await
        .unwrap_or_else(|e| {
            panic!(
                "REAL nvmet refused the lane association against {}:{} ({}): {e} \
                 — the 2026-08-04 field failure class (wherever this diverges \
                 from the mock suite, the mock is too permissive)",
                target.traddr, target.trsvcid, target.subnqn
            )
        });

    let len = 2usize << target.lba_shift;
    let mut first = vec![0u8; len];
    sess.read_into_slice(0, &mut first)
        .await
        .expect("lane read against real nvmet");
    let mut second = vec![0u8; len];
    sess.read_into_slice(0, &mut second)
        .await
        .expect("second lane read against real nvmet");
    assert_eq!(first, second, "lane reads of one LBA range must be stable");

    // Byte-parity witness when the kernel initiator's device node is
    // readable (root runs): the lane must serve the SAME bytes the kernel
    // path serves.
    if let Some(dev) = dev_path {
        if let Ok(mut f) = std::fs::File::open(&dev) {
            use std::io::Read;
            let mut kernel_view = vec![0u8; len];
            f.read_exact(&mut kernel_view)
                .expect("kernel-initiator read of the same range");
            assert_eq!(
                first, kernel_view,
                "lane bytes must equal the kernel initiator's view of {dev}"
            );
        }
    }

    assert!(!sess.poisoned(), "association must stay healthy");
    let (free, total) = sess.cid_slots();
    assert_eq!(free, total, "CID custody must close at quiescence");
    sess.quiesce().await;
}

// -------------------------------------------- rxq arbiter + capacity gate
//
// Field row 3 (D12 session, 2026-08-04) pinned red-first:
//   (1) `REGISTER_ZCRX_IFQ (if_idx=6, rxq=31): File exists` — every lane
//       session derived the SAME fixed rxq for a NIC; sessions must get
//       DISTINCT queues from a process-wide per-NIC arbiter.
//   (2) `zcrx steering: 1 flows exceed the 0 reserved rule slots` — the
//       ntuple/capacity probe must run BEFORE bring-up, refuse loud ONCE
//       per NIC, and name the exact operator remedy.

use squeezefs::zcrx_lane::initiator::ZcrxPlan;
use squeezefs::zcrx_lane::rxq_alloc;
use squeezefs::zcrx_lane::steering::{self, NicControl};

#[test]
fn test_rxq_arbiter_two_sessions_on_one_nic_get_distinct_queues() {
    // (1) Two sessions on one NIC (ifindex-keyed) must never share an rxq.
    let ifx = 0xACE0;
    let a = rxq_alloc::acquire(ifx, "itest-nic-a", 32, 4).expect("first lease");
    assert_eq!(
        a.queues(),
        &[28, 29, 30, 31],
        "first session keeps parity with the §8 highest-indexed picks"
    );
    let b = rxq_alloc::acquire(ifx, "itest-nic-a", 32, 4).expect("second lease");
    for q in b.queues() {
        assert!(
            !a.queues().contains(q),
            "distinct rxqs required (REGISTER_ZCRX_IFQ EEXIST class): {:?} vs {:?}",
            b.queues(),
            a.queues()
        );
    }
    for q in a.queues().iter().chain(b.queues()) {
        assert!((24..32).contains(q), "grants stay in the derived pool");
    }
}

#[test]
fn test_rxq_arbiter_exhaustion_refuses_with_derived_numbers() {
    // (2 of the arbiter's laws) Exhaustion refusal must name the NIC, the
    // probed queue count, the DERIVED pool, and the demand — never a bare
    // errno-class line.
    let ifx = 0xACE1;
    let _hold = rxq_alloc::acquire(ifx, "itest-nic-b", 8, 2).expect("pool-filling lease");
    let err = rxq_alloc::acquire(ifx, "itest-nic-b", 8, 1).expect_err("exhausted pool");
    for needle in [
        "itest-nic-b",
        "8 RX queues",
        "2 lane-eligible",
        "demand for 1",
    ] {
        assert!(err.contains(needle), "refusal must carry {needle:?}: {err}");
    }
}

#[test]
fn test_rxq_arbiter_release_then_reacquire_reuses_index() {
    let ifx = 0xACE2;
    let a = rxq_alloc::acquire(ifx, "itest-nic-c", 8, 2).expect("lease");
    assert_eq!(a.queues(), &[6, 7]);
    drop(a);
    let b = rxq_alloc::acquire(ifx, "itest-nic-c", 8, 2).expect("reacquire");
    assert_eq!(b.queues(), &[6, 7], "freed indices must be reused");
}

#[test]
fn test_rxq_arbiter_range_derives_from_nic_queue_count() {
    // The derivation law ("never just 8"): the usable rxq range is a
    // FUNCTION of the probed queue count — channels/4 top slice (§8).
    for channels in [8u32, 12, 32, 64] {
        let ifx = 0xACE8 + channels;
        let lease = rxq_alloc::acquire(ifx, "itest-nic-d", channels, u16::MAX)
            .unwrap_or_else(|e| panic!("{channels}-queue NIC must grant: {e}"));
        assert_eq!(lease.queues().len() as u32, channels / 4);
        for q in lease.queues() {
            assert!(*q >= channels - channels / 4 && *q < channels);
        }
    }
}

/// A minimal read-only NicControl for the capacity gate: every MUTATING
/// verb refuses — pinning that the gate never touches NIC state.
struct CapMock {
    name: &'static str,
    ntuple_on: bool,
    table: u32,
}

impl NicControl for CapMock {
    fn ifname(&self) -> &str {
        self.name
    }
    fn combined_channels(&mut self) -> Result<u32, String> {
        Ok(32)
    }
    fn tcp_data_split_on(&mut self) -> Result<bool, String> {
        Ok(true)
    }
    fn ntuple_enabled(&mut self) -> Result<bool, String> {
        Ok(self.ntuple_on)
    }
    fn rxfh_indir(&mut self) -> Result<Vec<u32>, String> {
        Ok((0..64).collect())
    }
    fn set_rxfh_indir(&mut self, _: &[u32]) -> Result<(), String> {
        panic!("capacity gate must never mutate the NIC (RSS write)");
    }
    fn ntuple_table_size(&mut self) -> Result<u32, String> {
        Ok(self.table)
    }
    fn special_loc_supported(&mut self) -> Result<bool, String> {
        Ok(false)
    }
    fn ntuple_table_size_hint(&mut self) -> Result<u32, String> {
        Ok(self.table)
    }
    fn ntuple_locs(&mut self) -> Result<Vec<u32>, String> {
        Ok(Vec::new())
    }
    fn insert_ntuple(&mut self, _: u32, _: &steering::FlowRule) -> Result<u32, String> {
        panic!("capacity gate must never mutate the NIC (rule insert)");
    }
    fn delete_ntuple(&mut self, _: u32) -> Result<(), String> {
        panic!("capacity gate must never mutate the NIC (rule delete)");
    }
}

#[test]
fn test_steering_capacity_gate_ntuple_off_refuses_zero_advertised_arms() {
    // ntuple OFF keeps the remedy-naming refusal (unchanged law).
    let mut off = CapMock {
        name: "itest-cap-off",
        ntuple_on: false,
        table: 1024,
    };
    let err = steering::steering_capacity_gate(&mut off).expect_err("ntuple off must refuse");
    assert!(
        err.contains("ethtool -K itest-cap-off ntuple on"),
        "refusal must name the exact remedy: {err}"
    );

    // FINDING 1 (2026-08 field): a 0-size ADVERTISEMENT with ntuple ON
    // is the mlx5-class driver lie (inserts empirically succeed — the
    // remedy was applied and the lane still refused on every device).
    // The gate must NOT refuse: the kernel's insert verdict rules.
    let mut zero = CapMock {
        name: "itest-cap-zero",
        ntuple_on: true,
        table: 0,
    };
    assert_eq!(
        steering::steering_capacity_gate(&mut zero)
            .expect("0-advertised with ntuple ON must arm (kernel-assigned class)"),
        steering::RuleSlots::KernelAssigned,
        "the mlx5 class arms via kernel-assigned locs"
    );

    // Advertised tables keep the reserved-range law.
    let mut ok = CapMock {
        name: "itest-cap-ok",
        ntuple_on: true,
        table: 1024,
    };
    let (lo, hi) = steering::reserved_loc_range(1024);
    assert_eq!(
        steering::steering_capacity_gate(&mut ok).expect("healthy gate"),
        steering::RuleSlots::Reserved { lo, hi }
    );

    // The kernel-assigned class must never zero the queue want (there is
    // no static slot bound — capacity is the kernel's insert verdict).
    assert_eq!(
        steering::free_reserved_slots("itest-cap-zero", &steering::RuleSlots::KernelAssigned),
        None,
        "no static clamp exists on the kernel-assigned class"
    );

    // Loud-once-per-NIC: first refusal reports, repeats are throttled;
    // a DIFFERENT NIC reports again.
    assert!(steering::note_arm_refusal_once("itest-once-a"));
    assert!(
        !steering::note_arm_refusal_once("itest-once-a"),
        "second refusal on one NIC must be throttled (10 devices ride one NIC)"
    );
    assert!(steering::note_arm_refusal_once("itest-once-b"));
}

#[test]
fn test_rxq_arbiter_serial_rearm_rotates_pool_before_reuse() {
    // FINDING 2's engine: kernel ifq teardown is ASYNC (ring-fd close
    // defers the unregister), so a freed rxq re-granted instantly
    // re-registers into EEXIST — the deployed field log shows rxq=31
    // re-derived for every device's arm. Serial re-arms must walk the
    // WHOLE free pool before reusing a freed index.
    let ifx = 0xACF1;
    let mut seen: Vec<u32> = Vec::new();
    for i in 0..8 {
        let l = rxq_alloc::acquire(ifx, "itest-nic-rot", 32, 1).expect("grant");
        let q = l.queues()[0];
        assert!(
            !seen.contains(&q),
            "arm {i}: freed queue {q} reused while fresh queues remained \
             (the EEXIST-recycle class): {seen:?}"
        );
        seen.push(q);
    }
    // History exhausted: the 9th arm reuses the LEAST-recently-freed.
    let l = rxq_alloc::acquire(ifx, "itest-nic-rot", 32, 1).expect("grant");
    assert_eq!(
        l.queues()[0],
        seen[0],
        "reuse order is least-recently-freed first"
    );
}

#[test]
fn test_registration_rxqs_come_from_the_arbiter_grant_disjoint_across_sessions() {
    // FINDING 2's plumbing pin: the qid→rxq mapping the ring driver
    // registers with (`ZcrxPlan::rxq_for_qid`) is fed EXCLUSIVELY by the
    // arbiter's granted list — two sessions' plans on one NIC can never
    // present the same rxq at REGISTER_ZCRX_IFQ time.
    let ifx = 0xACF2;
    let a = rxq_alloc::acquire(ifx, "itest-nic-w", 32, 2).expect("lease A");
    let b = rxq_alloc::acquire(ifx, "itest-nic-w", 32, 2).expect("lease B");
    let plan = |lease: std::sync::Arc<rxq_alloc::RxqLease>| ZcrxPlan {
        ifname: "itest-nic-w".into(),
        ifindex: ifx,
        numa_node: None,
        rx_queues: lease.queues().to_vec(),
        rxq_lease: Some(lease),
        ring_fill_bytes: 0,
    };
    let pa = plan(std::sync::Arc::new(a));
    let pb = plan(std::sync::Arc::new(b));
    for qa in 1..=2u16 {
        let ra = pa.rxq_for_qid(qa).expect("A maps every qid");
        for qb in 1..=2u16 {
            let rb = pb.rxq_for_qid(qb).expect("B maps every qid");
            assert_ne!(
                ra, rb,
                "sessions A qid{qa} and B qid{qb} would collide at registration"
            );
        }
    }
    assert_eq!(pa.rxq_for_qid(3), None, "beyond the grant maps to None");
}

// ------------------------------------------- round-3: fair grant spread (C)

#[test]
fn test_fair_queue_want_derives_from_nic_sharing() {
    // FINDING C: two want-4 sessions consumed all 8 eligible queues and
    // 8 of 10 devices got nothing — for the cold seq shape fills spread
    // across ALL namespaces, so breadth beats depth. The per-session
    // want derives: clamp(eligible / devices_via_nic, 1, geometry want).
    use squeezefs::zcrx_lane::steering::fair_queue_want;
    assert_eq!(
        fair_queue_want(8, 10, 4),
        1,
        "the field shape: 8 eligible / 10 devices → every device gets breadth"
    );
    assert_eq!(fair_queue_want(8, 2, 4), 4, "2 devices: full derived want");
    assert_eq!(fair_queue_want(8, 4, 4), 2);
    assert_eq!(fair_queue_want(8, 3, 4), 2, "integer share rounds down");
    assert_eq!(fair_queue_want(8, 1, 4), 4, "sole device keeps its want");
    assert_eq!(fair_queue_want(2, 100, 4), 1, "floor 1 = physical minimum");
    assert_eq!(fair_queue_want(64, 2, 8), 8, "geometry want stays the cap");
}

#[test]
fn test_tcp_devices_via_nic_counts_namespaces_behind_the_route() {
    // FINDING C's input: devices-behind-this-NIC comes from the mount's
    // OWN sysfs + route probe — never a constant. Fixture: two tcp
    // controllers route via ens1 (3 namespaces total), one via ens2,
    // one fc controller (ignored).
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    let mk = |ctrl: &str, transport: &str, addr: &str, namespaces: &[&str]| {
        let c = root.join("class/nvme").join(ctrl);
        std::fs::create_dir_all(&c).unwrap();
        std::fs::write(c.join("transport"), format!("{transport}\n")).unwrap();
        std::fs::write(c.join("address"), format!("{addr}\n")).unwrap();
        for ns in namespaces {
            std::fs::create_dir_all(c.join(ns)).unwrap();
        }
    };
    mk(
        "nvme0",
        "tcp",
        "traddr=10.0.0.1,trsvcid=4420",
        &["nvme0n1", "nvme0n2"],
    );
    mk("nvme1", "tcp", "traddr=10.0.0.2,trsvcid=4420", &["nvme1n1"]);
    mk("nvme2", "tcp", "traddr=10.0.1.1,trsvcid=4420", &["nvme2n1"]);
    mk("nvme3", "fc", "traddr=nn-0x10,trsvcid=none", &["nvme3n1"]);
    let resolve = |ip: &std::net::IpAddr| -> Option<String> {
        match ip.to_string().as_str() {
            "10.0.0.1" | "10.0.0.2" => Some("ens1".into()),
            "10.0.1.1" => Some("ens2".into()),
            _ => None,
        }
    };
    assert_eq!(
        probe::tcp_devices_via_nic_with(root, "ens1", &resolve),
        3,
        "namespaces behind ens1"
    );
    assert_eq!(probe::tcp_devices_via_nic_with(root, "ens2", &resolve), 1);
    assert_eq!(
        probe::tcp_devices_via_nic_with(root, "ens9", &resolve),
        0,
        "a NIC no target routes through"
    );
}

// ------------------------------------------ round-4 field findings (red)

#[test]
fn test_census_counts_native_multipath_c_path_namespaces() {
    // FINDING D root cause: on a CONFIG_NVME_MULTIPATH fleet (modern
    // default) a controller's namespace children are nvme<C>c<P>n<N>
    // ("c-paths"), not nvme<C>n<N> — the round-3 census counted 0, so
    // devices degraded to 1 and fair_queue_want returned the FULL
    // geometry want (4): exactly the field's 2×4-queues shape. Both
    // shapes must count.
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    let mk = |ctrl: &str, addr: &str, namespaces: &[&str]| {
        let c = root.join("class/nvme").join(ctrl);
        std::fs::create_dir_all(&c).unwrap();
        std::fs::write(c.join("transport"), "tcp\n").unwrap();
        std::fs::write(c.join("address"), format!("{addr}\n")).unwrap();
        for ns in namespaces {
            std::fs::create_dir_all(c.join(ns)).unwrap();
        }
    };
    // The field fleet's shape: one controller per device, native
    // multipath ⇒ c-path children.
    for i in 0..10u32 {
        let ctrl = format!("nvme{}", 10 + 2 * i);
        let ns = format!("nvme{}c{}n1", 10 + 2 * i, 10 + 2 * i);
        mk(&ctrl, "traddr=10.181.177.193,trsvcid=4420", &[ns.as_str()]);
    }
    // A non-multipath controller (both shapes coexist across fleets).
    mk(
        "nvme50",
        "traddr=10.181.177.193,trsvcid=4420",
        &["nvme50n1", "nvme50n2"],
    );
    let resolve = |ip: &std::net::IpAddr| -> Option<String> {
        (ip.to_string() == "10.181.177.193").then(|| "ens1f0np0".to_string())
    };
    assert_eq!(
        probe::tcp_devices_via_nic_with(root, "ens1f0np0", &resolve),
        12,
        "10 c-path namespaces + 2 plain namespaces"
    );
}

#[test]
fn test_ten_devices_one_nic_first_session_wants_one_queue() {
    // FINDING D end-to-end (the composition the arm ladder runs): census
    // → fair_queue_want → arbiter. Ten multipath devices behind one
    // 32-queue NIC ⇒ the FIRST session registers 1 queue, not 4.
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    for i in 0..10u32 {
        let ctrl = format!("nvme{i}");
        let c = root.join("class/nvme").join(&ctrl);
        std::fs::create_dir_all(c.join(format!("nvme{i}c{i}n1"))).unwrap();
        std::fs::write(c.join("transport"), "tcp\n").unwrap();
        std::fs::write(c.join("address"), "traddr=10.0.0.1,trsvcid=4420\n").unwrap();
    }
    let resolve =
        |ip: &std::net::IpAddr| (ip.to_string() == "10.0.0.1").then(|| "mock-e2e".to_string());
    let devices = probe::tcp_devices_via_nic_with(root, "mock-e2e", &resolve);
    assert_eq!(devices, 10, "the census sees all ten devices");
    let eligible = 32 / 4; // lane_eligible_queues(32) width — the §8 pool
    let want = squeezefs::zcrx_lane::steering::fair_queue_want(eligible, devices, 4);
    assert_eq!(want, 1, "clamp(8/10, 1, 4) = 1");
    let lease = rxq_alloc::acquire(0xDD01, "mock-e2e", 32, want).expect("lease");
    assert_eq!(
        lease.queues().len(),
        1,
        "the first session registers ONE queue — 8 of 10 devices get a lane"
    );
}

#[test]
fn test_nic_note_once_latches_per_tag_and_nic() {
    // The mlx5-lie INFO line printed ~40× in 30 s (round 4): every
    // per-NIC notice rides ONE latch family, keyed (tag, ifname) so
    // classes never consume each other's latch.
    use squeezefs::zcrx_lane::steering::nic_note_once;
    assert!(nic_note_once("mlx5-lie", "note-nic-a"));
    assert!(
        !nic_note_once("mlx5-lie", "note-nic-a"),
        "second notice on one NIC is throttled"
    );
    assert!(
        nic_note_once("arm-refusal", "note-nic-a"),
        "a DIFFERENT tag on the same NIC still prints"
    );
    assert!(nic_note_once("mlx5-lie", "note-nic-b"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn test_f_shutdown_teardown_quiesces_and_releases_all_sessions() {
    // FINDING F: sessions live in per-device OnceCell statics — statics
    // never drop, so on the field the daemon exited with 16 rules + RSS
    // crippled. The shutdown hook must tear down every live session
    // (stop → join → restore → release), and the registry must SEE them.
    let before = squeezefs::zcrx_lane::live_lane_sessions();
    let mock = MockTarget::start(MockCfg::default(), 4 << 20).await;
    let s1 = LaneSession::connect_with(mock.target(1, 4), LaneBackend::AreaSim)
        .await
        .expect("session 1");
    let s2 = LaneSession::connect_with(mock.target(1, 4), LaneBackend::AreaSim)
        .await
        .expect("session 2");
    assert_eq!(
        squeezefs::zcrx_lane::live_lane_sessions(),
        before + 2,
        "every armed session registers as live"
    );
    squeezefs::zcrx_lane::teardown_all_lanes().await;
    assert!(s1.torn_down() && s2.torn_down(), "both sessions torn down");
    assert_eq!(
        squeezefs::zcrx_lane::live_lane_sessions(),
        before,
        "shutdown leaves no live lane session"
    );
    let (free1, total1) = s1.cid_slots();
    assert_eq!(free1, total1, "session 1 quiesced (no leaked CID)");
    let (free2, total2) = s2.cid_slots();
    assert_eq!(free2, total2, "session 2 quiesced (no leaked CID)");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn test_f_poisoned_session_tears_down_promptly() {
    // FINDING F's poison half: a poisoned session held its ifqs, rules,
    // RSS exclusion AND arbiter lease for the rest of the field row (75 %
    // RSS width). The funnel must trigger the ordered teardown on the
    // first read that observes the poison.
    let cfg = MockCfg {
        die_mid_c2h: true,
        ..Default::default()
    };
    let mock = MockTarget::start(cfg, 1 << 20).await;
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("blockfile");
    let content: Vec<u8> = (0..1 << 20).map(|i| (i % 241) as u8).collect();
    std::fs::write(&path, &content).unwrap();

    let before = squeezefs::zcrx_lane::live_lane_sessions();
    let _env = LaneEnv::area_sim(&mock, 1, 4);
    let dev = squeezefs::nvme_dev::NvmeBlockDev::new(path.to_str().unwrap());
    // First read: arms, dies mid-C2H, poisons, falls back to the kernel
    // path (registered live at arm).
    let got = dev.read_block(0, 65536).await.expect("kernel-path serve");
    assert_eq!(&got[..], &content[..65536]);
    assert_eq!(
        squeezefs::zcrx_lane::live_lane_sessions(),
        before + 1,
        "the armed (now poisoned) session is registered live"
    );
    // Second read observes the poison — the funnel must fire teardown.
    let _ = dev
        .read_block(65536, 4096)
        .await
        .expect("kernel-path serve");
    // Bounded settle for the spawned teardown (it joins driver tasks).
    let mut torn = false;
    for _ in 0..100 {
        if squeezefs::zcrx_lane::live_lane_sessions() == before {
            torn = true;
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    assert!(
        torn,
        "a poisoned session must release its NIC state promptly (live: {} vs before {})",
        squeezefs::zcrx_lane::live_lane_sessions(),
        before
    );
}

// --------------------------------------------- round 6: gauge honesty (red)

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn test_teardown_is_not_poison() {
    // Round 6 field: zcrx_lane_poisoned = 8 with ZERO poison log lines —
    // the shutdown teardown quiesced the queues, the TARGET then closed
    // the admin connection, and the admin watchdog marked that ORDERLY
    // close as a session poison. The design doc says poisoned is a
    // must-stay-0 tripwire: it must count REAL transport poison only,
    // so teardown retires the admin watchdog BEFORE the association
    // unwinds.
    // Determinism hardening (stop-ship flake, 2026-08): the single-shot
    // form lost a schedule race ~1 in 5 — abort() only REQUESTS
    // cancellation, and teardown() returned with no happens-before edge
    // to the watchdog's completion, so is_finished() raced the runtime.
    // 50 cycles at the measured 15 % loss rate ⇒ P(spurious green)
    // ≈ 0.85^50 ≈ 3e-4: deterministically red against the racy
    // implementation, and the standing regression pin for the fixed
    // one (teardown OWNS the retirement — take, abort, AWAIT).
    let mock = MockTarget::start(MockCfg::default(), 1 << 20).await;
    let before = zcrx_metric("poisoned");
    for cycle in 0..50 {
        let sess = LaneSession::connect_with(mock.target(1, 4), LaneBackend::AreaSim)
            .await
            .expect("arm");
        sess.teardown().await;
        assert!(
            sess.admin_watchdog_finished(),
            "cycle {cycle}: teardown must retire the admin watchdog — \
             DETERMINISTICALLY, before it returns (a post-teardown \
             target close must be unreadable as poison)"
        );
        assert!(sess.torn_down(), "cycle {cycle}: teardown latched");
        assert_eq!(
            zcrx_metric("poisoned"),
            before,
            "cycle {cycle}: an orderly teardown is NOT a poison transition"
        );
    }
}

// ------------------------------------ round 7: gather/fill closure (red)

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn test_gather_fill_closure_holds_across_mid_read_poison() {
    // Round 7 field: the FIRST healthy row broke the gather ≡ fill law
    // (+6.3 MB) — gather counted PER SEGMENT inside read_segment_area,
    // fill counts once per WHOLE funnel read; a poison mid-multi-segment
    // read left the completed segments' gathers counted with no fill.
    // The law decision: closure holds UNCONDITIONALLY — gather accounting
    // moves to the whole-read success boundary (physically identical on
    // success: the per-segment passes sum to the read), so torn reads
    // contribute to NEITHER counter and the engagement instrument the
    // row verdict keys on stays trustworthy on poisoned rows too.
    let cfg = MockCfg {
        fail_on_read_n: 6, // read 1 = capsules 1..4 (healthy); read 2's 2nd segment op-fails
        ..Default::default()
    };
    let mock = MockTarget::start(cfg, 4 << 20).await;
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("blockfile");
    let content: Vec<u8> = (0..4 << 20).map(|i| (i % 251) as u8).collect();
    std::fs::write(&path, &content).unwrap();

    let _env = LaneEnv::area_sim(&mock, 1, 8);
    let gather0 = zcrx_metric("gather_bytes");
    let fill0 = zcrx_metric("fill_bytes");
    let dev = squeezefs::nvme_dev::NvmeBlockDev::new(path.to_str().unwrap());

    // Read 1: 1 MiB = 4 × 256 KiB segments, all served — a healthy fill
    // (MOCK namespace bytes ≠ file bytes: the structural-engagement
    // check the funnel suite uses).
    let got = dev.read_block(0, 1 << 20).await.expect("healthy lane read");
    assert_eq!(&got[..], &mock.device[..1 << 20], "read 1 lane-served");
    // Read 2: capsules 5/7/8 serve their segments fully, capsule 6
    // op-fails — the WHOLE read tears (the field's poisoned-mid-fill
    // class, pinned death-free for determinism) and fails over to the
    // kernel path, which serves FILE bytes. Under the per-segment
    // accounting this leaves 3 × 256 KiB of gathered-but-never-filled
    // bytes — the field's +6.3 MB shape in miniature.
    let got2 = dev
        .read_block(1 << 20, 1 << 20)
        .await
        .expect("kernel-path serve after the torn read");
    assert_eq!(
        &got2[..],
        &content[1 << 20..2 << 20],
        "read 2 kernel-path-served after the torn read"
    );

    let gather_delta = zcrx_metric("gather_bytes") - gather0;
    let fill_delta = zcrx_metric("fill_bytes") - fill0;
    assert_eq!(
        gather_delta, fill_delta,
        "gather ≡ fill must hold ACROSS a mid-read poison — a torn \
         read's completed segments contribute to neither counter"
    );
    assert_eq!(fill_delta, 1 << 20, "exactly the one healthy read counted");
}

// ------------------------------------------- round 8: no-harm laws (red)

#[test]
fn test_poison_gauge_and_log_are_structurally_tied() {
    // FINDING H (round 8): poisoned=3 in the field with ZERO canonical
    // poison log lines — the third silent-poison burn. The structural
    // tie (the skip-ledger source-scan precedent): the gauge increments
    // ONLY through mark_session_poisoned, which REQUIRES a reason and
    // ALWAYS logs it; and no site may pre-store a queue's poisoned flag
    // outside the poison funnels (the 30 s-timeout arms' pre-store
    // suppressed the queue-level log — a silent-poison vector).
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
    let initiator =
        std::fs::read_to_string(root.join("src/zcrx_lane/initiator.rs")).expect("read initiator");
    let sig_at = initiator
        .find("fn mark_session_poisoned(")
        .expect("the one poison funnel exists");
    let sig = &initiator[sig_at..sig_at + 600];
    assert!(
        sig.contains("why: &str"),
        "mark_session_poisoned must REQUIRE a reason (the funnel law)"
    );
    assert!(
        sig.contains("log::error!"),
        "the poison funnel must LOG the reason on the winning transition"
    );
    // The gauge moves only inside the funnel (count the CODE shape —
    // the field-access increment — so doc prose cannot false-positive).
    assert_eq!(
        initiator.matches("zcrx_lane_poisoned.fetch_add").count(),
        1,
        "zcrx_lane_poisoned increments at exactly ONE site (the funnel)"
    );
    // No log-suppressing pre-store: `poisoned.store(true, …)` outside the
    // funnels makes the poison's first-swap log unreachable.
    let pre_stores = initiator.matches("poisoned.store(true").count();
    assert_eq!(
        pre_stores, 0,
        "no site may pre-store a poisoned flag outside the poison funnels \
         (found {pre_stores} — the silent-poison vector)"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn test_admission_overflow_declines_fast_never_parks() {
    // FINDING I (round 8): 648 admission waits vs 206 fills (>3/fill) —
    // parked over-admission fed the starvation churn (41 episodes ×
    // 937 ms of held reads). The law: a lane that can serve N concurrent
    // fills admits N and DECLINES the rest to the kernel path
    // immediately (declines are free) — never parks them.
    let gate = Arc::new(tokio::sync::Semaphore::new(0));
    let cfg = MockCfg {
        read_gate: Some(Arc::clone(&gate)),
        ..Default::default()
    };
    let mock = MockTarget::start(cfg, 4 << 20).await;
    // depth 8 × 256 KiB max_xfer ⇒ 2 MiB (PMD-rounded) sim area; the sim
    // fill window is the whole area ⇒ 1 MiB admitted = FOUR 256 KiB
    // segments in flight; the fifth must decline.
    let sess = LaneSession::connect_with(mock.target(1, 8), LaneBackend::AreaSim)
        .await
        .expect("arm");
    let mut held = Vec::new();
    for i in 0..4u64 {
        let sess = Arc::clone(&sess);
        held.push(tokio::spawn(async move {
            let mut buf = vec![0u8; 256 * 1024];
            sess.read_into_slice(i * 256 * 1024, &mut buf).await
        }));
    }
    // Deterministic edge: the fifth read may only be issued once the
    // four spawned reads provably HOLD the whole admission window —
    // observed on the lane's own diagnostic (the mock's gate parks its
    // serve loop, so wire-side counting cannot see reads 2..4).
    // Bounded settle, no sleep-sync semantics.
    for _ in 0..500 {
        if sess.admission_units_available() == 0 {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    assert_eq!(
        sess.admission_units_available(),
        0,
        "the four admitted reads must hold the whole window before the probe"
    );
    // The four admitted reads are parked at the MOCK's gate (in flight,
    // permits held). The fifth must return WouldBlock promptly — 2 s is
    // the generous ceiling that distinguishes an immediate decline from
    // the old park-until-permits behavior.
    let mut buf5 = vec![0u8; 256 * 1024];
    let fifth = tokio::time::timeout(
        std::time::Duration::from_secs(2),
        sess.read_into_slice(1 << 20, &mut buf5),
    )
    .await;
    match fifth {
        Ok(Err(e)) => {
            let msg = format!("{e}");
            assert!(
                msg.contains("declined"),
                "the overflow read must DECLINE (kernel path serves): {msg}"
            );
        }
        Ok(Ok(())) => panic!("the fifth read cannot complete while the gate holds all permits"),
        Err(_) => panic!(
            "the overflow read PARKED on admission (the 648-waits/206-fills \
             starvation churn) — it must decline immediately"
        ),
    }
    // Release the target; the admitted four complete normally.
    gate.add_permits(64);
    for h in held {
        h.await.expect("join").expect("admitted read completes");
    }
    sess.teardown().await;
}

// ---------------------------------------- engagement geometry (2026-08-06)
//
// Phase 2 of the read copy-elimination program (the zcrx ENGAGEMENT
// campaign). The round-8 verdict (`.benchmarks/2026-08-05-zcrx-z3-field-
// rows.md`) measured 0.05 % engagement at the flat-/4-pool +
// half-window geometry: 2 of 10 devices held NO lane, and the halved
// admission window (32 MiB/queue) declined most of the ~51 MiB/device
// the cold row offers — the copy-elimination thesis could not pay the
// RSS rent. The read CPU-wall ruling (2026-08-06) inverted the
// economics: reads are whole-box CPU-bound, so every RX byte moved to
// zero-copy is direct capacity. These contracts pin the re-derived
// geometry — every input probed or censused, no constants:
//
//   pool width  = clamp(devices_via_nic, channels/4, channels/2)
//   admission   = the FULL fill window (depth × max_xfer) — the /2
//                 retired; its implicit slack budget moves to the AREA
//                 size, derived from MTU/chunk burst occupancy
//   area        = fill_window + delivery_slack + ring_standing (rounds
//                 6–7 arithmetic unchanged underneath)

#[test]
fn test_engagement_pool_scales_with_the_device_census() {
    use squeezefs::zcrx_lane::steering::lane_eligible_queues;
    // The field shape: 32 RX queues, 10 fabric devices behind the rail.
    // The flat /4 pool (8) left 2 of 10 devices with NO lane — 20 % of
    // row bytes structurally kernel-path forever. The census widens the
    // pool to one queue per device.
    assert_eq!(
        lane_eligible_queues(32, 10),
        22..32,
        "field shape: every device can hold a lane"
    );
    // Floor: the standing §8 ¼ posture — a small census never NARROWS
    // the pool below channels/4 (today's exact behavior).
    assert_eq!(lane_eligible_queues(32, 2), 24..32);
    assert_eq!(
        lane_eligible_queues(32, 5),
        24..32,
        "the two-rail split (5 devices/rail) stays at the floor"
    );
    // Ceiling: the RSS set never falls below HALF the NIC — the kernel
    // path (writes, metadata, admin, declined reads) keeps ≥ channels/2
    // of the queue width no matter how many devices share the rail.
    assert_eq!(lane_eligible_queues(32, 100), 16..32);
    // A failed census (0) degrades to the floor — the sole-device
    // posture, byte-identical to the pre-campaign pool.
    assert_eq!(lane_eligible_queues(32, 0), 24..32);
    // The too-narrow refusal is unchanged: channels/4 == 0 ⇒ empty pool
    // (the arm refuses loud; a 2-queue NIC never dedicates ZC queues).
    let pool = lane_eligible_queues(3, 10);
    assert_eq!(
        pool.end - pool.start,
        0,
        "sub-4-queue NICs never dedicate ZC queues (unchanged)"
    );
}

#[test]
fn test_engagement_all_field_devices_lease_a_queue() {
    // Round 8: fair_queue_want(8, 10, 4) = 1 but the arbiter pool held
    // only 8 — the 9th and 10th sessions were REFUSED and their devices
    // stayed kernel-path for the mount lifetime. With the census-driven
    // pool all ten sessions hold a distinct queue.
    let devices = 10usize;
    let eligible = {
        let p = squeezefs::zcrx_lane::steering::lane_eligible_queues(32, devices);
        p.end - p.start
    };
    assert_eq!(eligible, 10, "the pool covers the census");
    let want = squeezefs::zcrx_lane::steering::fair_queue_want(eligible, devices, 4);
    assert_eq!(want, 1, "clamp(10/10, 1, 4) = 1 — breadth beats depth");
    let mut leases = Vec::new();
    let mut seen = std::collections::BTreeSet::new();
    for i in 0..devices {
        let l = rxq_alloc::acquire(0xE2E2, "engage-e2e", 32, devices, want)
            .unwrap_or_else(|e| panic!("device {i} must lease a queue: {e}"));
        assert_eq!(l.queues().len(), 1);
        for q in l.queues() {
            assert!((22..32).contains(q), "grants stay inside the pool");
            assert!(seen.insert(*q), "distinct queues across sessions");
        }
        leases.push(l);
    }
    drop(leases);
}

#[test]
fn test_engagement_admission_is_the_full_window_with_derived_slack() {
    use squeezefs::zcrx_lane::area;
    // The field geometry: depth 64 (32-CPU client), max_xfer 1 MiB
    // (LANE_MAX_XFER_CAP_BYTES), 4 KiB chunks, MTU 9000.
    let window = area::area_bytes_per_queue(64, 1 << 20);
    assert_eq!(window, 64 << 20);
    // Admission covers the FULL window (the /2 retired): the window and
    // the CID namespace are now the SAME arithmetic (64 CIDs × 1 MiB
    // max_xfer ≡ 64 MiB admitted payload).
    assert_eq!(
        area::admission_permits(window as usize, 4096),
        (64 << 20) / 4096,
        "the whole fill window admits payload"
    );
    // The slack the /2 implicitly budgeted is now DERIVED: an MTU-9000
    // payload burst lands in ⌈9000/4096⌉ = 3 page-grain niovs (12288 B
    // holding ~9000) — occupancy ~73 %, so the fills' chunk budget needs
    // window × 3288/9000 extra bytes, PMD-rounded.
    let slack = area::delivery_slack_bytes(window, Some(9000), 4096);
    assert_eq!(slack, 24 << 20, "64 MiB × 3288/9000 → 24 MiB PMD-rounded");
    // A chunk-exact MTU wastes nothing.
    assert_eq!(area::delivery_slack_bytes(window, Some(8192), 4096), 0);
    // 1500-MTU: every ≤ 1500-B payload burns a whole 4 KiB chunk —
    // the honest (large) slack a small-MTU rail pays.
    assert_eq!(
        area::delivery_slack_bytes(window, Some(1500), 4096),
        112 << 20,
        "64 MiB × 2596/1500 → 112 MiB PMD-rounded"
    );
    // Unknown MTU degrades to occupancy ½ — the retired half-window
    // posture, now explicit in the AREA instead of implicit in the
    // admission semaphore.
    assert_eq!(area::delivery_slack_bytes(window, None, 4096), window);
}

#[test]
fn test_engagement_field_shape_admits_the_offered_row() {
    use squeezefs::zcrx_lane::{area, probe};
    // The round-8 verdict's arithmetic, inverted: the field row offers
    // ~128 concurrent 4 MiB fills across 10 devices ≈ 51.2 MiB in
    // flight per device. The per-queue admitted window must cover it —
    // whole-read atomic admission then ADMITS the offered row instead
    // of declining most of it (the 0.05 %-engagement term).
    let (_queues, depth) = probe::lane_geometry(32); // the 32-CPU field client
    let window = area::area_bytes_per_queue(depth, probe::LANE_MAX_XFER_CAP_BYTES);
    let offered_per_device = 128u64 * (4 << 20) / 10;
    assert!(
        window >= offered_per_device,
        "the admitted window ({window}) must cover the offered per-device \
         in-flight ({offered_per_device})"
    );
    let permits = area::admission_permits(window as usize, 4096);
    let whole_read_units = (4usize << 20).div_ceil(4096);
    assert!(
        permits / whole_read_units >= 12,
        "≥ 12 concurrent whole 4 MiB reads admit per queue (halved window \
         admitted 8 vs ~12.8 offered; got {})",
        permits / whole_read_units
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn test_compose_dest_lease_shape_rides_the_fused_lane() {
    // The dest-lease compose adjudication (campaign item 4): a leased
    // window arrives at the funnel as a dest-carrying, 4 KiB-aligned,
    // SUB-BLOCK ranged read — exactly the Z3 fused-gather shape. The
    // lane serves it with ONE requester-side gather into the reply
    // window and ZERO kernel RX passes; the area is never
    // reply-reachable (NIC fills land in arrival order — no DMA can aim
    // a specific read's C2HData at a specific ent window), so the single
    // fused gather IS the compose's floor on this kernel. The lease's
    // ledger half (`read_dest_lease_bytes` ⊆ `read_dest_dma_bytes`) is
    // counted at the routing serve regardless of vehicle — closure
    // unchanged (design §4.4).
    let mock = MockTarget::start(MockCfg::default(), 8 << 20).await;
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("blockfile");
    std::fs::write(&path, vec![0xEEu8; 8 << 20]).unwrap();

    let env = LaneEnv::area_sim(&mock, 2, 8);
    let before_fills = zcrx_metric("fills");
    let before_bytes = zcrx_metric("fill_bytes");
    let before_gather = zcrx_metric("gather_bytes");
    let before_dest_gather = zcrx_metric("dest_gather_bytes");
    let before_fallbacks = zcrx_metric("fallbacks");

    let dev = squeezefs::nvme_dev::NvmeBlockDev::new(path.to_str().unwrap());
    let (dest_ptr, dest_bytes) = squeezefs::cache::pool::ALIGNED_BUF_POOL.alloc();
    // The lease shape: 4 KiB-aligned NON-block offset, odd 4 KiB-multiple
    // sub-block length (63 × 4 KiB — strictly inside one 4 MiB block,
    // not a max_xfer multiple).
    let (off, size) = (12288u64, 63 * 4096usize);
    let got = dev
        .read_block_with_dest(off, size, Some(dest_ptr as u64))
        .await
        .expect("lease-shaped dest read");
    drop(env);

    assert_eq!(
        &got[..size],
        &mock.device[off as usize..off as usize + size],
        "the lane must serve the lease-shaped window (namespace bytes)"
    );
    // SAFETY: test-owned pooled buffer, op complete.
    let landed = unsafe { std::slice::from_raw_parts(dest_ptr, size) };
    assert_eq!(
        landed,
        &mock.device[off as usize..off as usize + size],
        "the gather lands IN the leased destination (no intermediate)"
    );
    assert_eq!(zcrx_metric("fills") - before_fills, 1);
    assert_eq!(zcrx_metric("fill_bytes") - before_bytes, size as u64);
    assert_eq!(
        zcrx_metric("gather_bytes") - before_gather,
        size as u64,
        "gather ≡ fill closes on the lease shape"
    );
    assert_eq!(
        zcrx_metric("dest_gather_bytes") - before_dest_gather,
        size as u64,
        "the fused-serve gauge accounts the leased window byte-exactly"
    );
    assert_eq!(zcrx_metric("fallbacks"), before_fallbacks, "clean run");
    drop(dest_bytes);
}
