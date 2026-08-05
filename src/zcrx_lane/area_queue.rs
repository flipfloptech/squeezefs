//! The area-backend IO queue (design §5): one queue = one TCP connection
//! = one receive area, a driver feeding chunk extents through the
//! streaming parser into the fill table, and admission-bounded in-flight
//! fills (area exhaustion backpressures at COMMAND admission — never a
//! mid-stream stall).
//!
//! Two drivers share every law here: the SIM driver below (socket recv
//! standing in for NIC DMA — the contract venue) and the real
//! `uring_zcrx` ring thread (field). Both push [`AreaSlice`] extents into
//! the same [`StreamParser`] → [`FillTable`] engine and poison through
//! the same lattice.

use super::area::{AreaSlice, ZcrxArea};
use super::fill_table::FillTable;
use super::initiator::mark_session_poisoned;
use super::pdu_stream::{ParseEvent, StreamParser};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use tokio::net::tcp::OwnedReadHalf;
use tokio::net::TcpStream;
use tokio::sync::mpsc;

/// Admission accounting grain (bytes per semaphore permit).
pub(crate) const ADMISSION_UNIT: usize = 4096;

/// Admission permits over the FILL WINDOW (the slice of the area the
/// provider pool can actually hand to fills — the area minus the NIC
/// ring's standing demand): HALF for admitted payload, half for
/// delivery slack (short-recv fragmentation, headers riding payload
/// chunks) — derived from the geometry, floor one chunk.
///
/// Round 6 kernel adjudication (io_uring/zcrx.c @ v6.19.14): the
/// round-5 wire-grain `amp` model is DELETED — falsified by the field
/// (MTU 9000 ⇒ amp 1, still instant exhaustion) and by the source
/// (`io_zcrx_copy_chunk` packs fallback niovs FULLY via io_copy_page —
/// no per-segment page burn). The real structural consumer is the NIC
/// RX ring itself (see [`ring_standing_bytes`]).
pub(crate) fn admission_permits(fill_window_len: usize, chunk: usize) -> usize {
    ((fill_window_len / 2).max(chunk) / ADMISSION_UNIT).max(1)
}

/// The NIC RX descriptor ring's STANDING demand on the provider pool
/// (round 6): when the queue restarts onto the zcrx memory provider,
/// the driver fills its RX ring FROM THE POOL — `rx_descs` descriptors
/// × ⌈mtu / chunk⌉ chunk-sized buffers each (worst-case legacy-RQ
/// geometry; striding-RQ fleets over-provision, the safe direction) —
/// and holds that many buffers for the queue's lifetime. The area must
/// cover this ON TOP of the fill window, or the pool is dry before the
/// first packet (the field shape: 8192 descs × ⌈9000/4096⌉=3 pages =
/// 96 MiB standing vs a 64 MiB round-5 area — instant ENOMEM, and
/// failover releases could never satisfy it). Unknown MTU degrades to
/// one chunk per descriptor (the floor the ring cannot be below).
pub(crate) fn ring_standing_bytes(rx_descs: u32, mtu: Option<u32>, chunk: usize) -> u64 {
    // RED PHASE skeleton: derivation pinned by the tests below.
    let _ = (rx_descs, mtu, chunk);
    0
}

pub(crate) fn admission_units(len: usize) -> u32 {
    len.div_ceil(ADMISSION_UNIT) as u32
}

/// Shared state of one area queue.
pub(crate) struct AreaShared {
    pub table: FillTable,
    /// Free CID pool (std mutex — push/pop only, no await inside; shared
    /// with [`super::initiator::CidSlot`] custody. Lock order where both
    /// are held: fill-table `pending` → `free_cids`).
    pub free_cids: Arc<std::sync::Mutex<Vec<u16>>>,
    pub cid_gate: Arc<tokio::sync::Semaphore>,
    /// CID namespace size (== queue depth) — the `cid_slots` diagnostic's
    /// denominator (the MEM-3 no-leak instrument).
    pub cid_capacity: usize,
    /// In-flight payload admission (see [`admission_permits`]); Arc'd so
    /// OWNED permits can ride the pending fill → the completed
    /// [`super::fill_table::ZcrxFill`] (exact accounting under
    /// cancellation — the MEM-3 custody law's admission face).
    pub admission: Arc<tokio::sync::Semaphore>,
    pub poisoned: AtomicBool,
    /// Round-5 blast-radius latch: a refill-starvation failover fired
    /// and the queue is recovering — NEW reads bypass the lane (kernel
    /// path serves) until payload flows or the episode drains.
    pub starved: AtomicBool,
}

impl AreaShared {
    pub(crate) fn new(depth: u16, area: &ZcrxArea, fill_window_len: usize) -> Arc<AreaShared> {
        Arc::new(AreaShared {
            table: FillTable::new(),
            free_cids: Arc::new(std::sync::Mutex::new((0..depth).collect())),
            cid_gate: Arc::new(tokio::sync::Semaphore::new(depth as usize)),
            cid_capacity: depth as usize,
            admission: Arc::new(tokio::sync::Semaphore::new(admission_permits(
                fill_window_len,
                area.chunk_bytes(),
            ))),
            poisoned: AtomicBool::new(false),
            starved: AtomicBool::new(false),
        })
    }

    /// Queue-level poison: propagate to the session lattice (counted
    /// once) and — when `drain` is true — fail every waiter (their segs
    /// drop → chunks recycle; entry drops return CID/permit custody).
    /// `drain` MUST be true only when the queue's driver provably pushes
    /// no further events (it is exiting, or it was joined) — the MEM-3
    /// drain discipline mirrored from the classic lane.
    pub(crate) fn poison(&self, why: &str, session_poison: &AtomicBool, drain: bool) {
        if !self.poisoned.swap(true, Ordering::SeqCst) {
            log::error!("zcrx-lane: IO queue poisoned: {why} — lane disarms, kernel path serves");
            mark_session_poisoned(session_poison);
        }
        if drain {
            self.table.fail_all(&format!("lane queue poisoned: {why}"));
        }
    }
}

/// How a queue's capsules reach its wire: the sim's tokio writer task or
/// the real ring driver's doorbell lane.
pub(crate) enum CommandSink {
    Chan(mpsc::UnboundedSender<Vec<u8>>),
    Ring(Arc<super::uring_zcrx::RingCmd>),
}

impl CommandSink {
    pub(crate) fn send(&self, capsule: Vec<u8>) -> Result<(), ()> {
        match self {
            CommandSink::Chan(tx) => tx.send(capsule).map_err(|_| ()),
            CommandSink::Ring(cmds) => cmds.send(capsule),
        }
    }
}

pub(crate) struct AreaQueue {
    pub shared: Arc<AreaShared>,
    pub sink: CommandSink,
    /// Sim reader + writer tasks — abort-and-join is the quiescence law
    /// (poison drain; timeout path). Empty on the ring backend.
    pub tasks: tokio::sync::Mutex<Vec<tokio::task::JoinHandle<()>>>,
    /// The real backend's driver thread (None on the sim backend).
    pub driver: std::sync::Mutex<Option<std::thread::JoinHandle<()>>>,
    pub area: Arc<ZcrxArea>,
}

impl AreaQueue {
    /// The drain half of the quiescence law: stop the wire (close the
    /// ring doorbell / abort the sim tasks), then JOIN every driver so
    /// no lane context survives holding chunk refs.
    pub(crate) async fn drain(&self) {
        if let CommandSink::Ring(cmds) = &self.sink {
            cmds.close();
        }
        let mut ts = self.tasks.lock().await;
        for t in ts.iter() {
            t.abort();
        }
        for t in ts.drain(..) {
            let _ = t.await;
        }
        drop(ts);
        let handle = self.driver.lock().expect("driver handle lock").take();
        if let Some(h) = handle {
            let _ = tokio::task::spawn_blocking(move || h.join()).await;
        }
    }
}

/// Spawn the SIM area queue over an established (post-Connect) stream.
pub(crate) fn spawn_area_queue(
    stream: TcpStream,
    depth: u16,
    area: Arc<ZcrxArea>,
    session_poison: Arc<AtomicBool>,
) -> AreaQueue {
    // Sim: no NIC ring exists — the whole area IS the fill window.
    let shared = AreaShared::new(depth, &area, area.len());
    let (read_half, write_half) = stream.into_split();
    let (tx, rx) = mpsc::unbounded_channel();

    let s2 = Arc::clone(&shared);
    let p2 = Arc::clone(&session_poison);
    let writer = tokio::spawn(async move {
        if let Err(why) = super::initiator::writer_loop(write_half, rx).await {
            // drain=false: the reader/driver may still be applying events
            // — its own exit performs the drain (MEM-3 drain discipline).
            s2.poison(&why, &p2, false);
        }
    });
    let s3 = Arc::clone(&shared);
    let a3 = Arc::clone(&area);
    let reader = tokio::spawn(async move {
        if let Err(why) = sim_reader_loop(read_half, &a3, &s3).await {
            // drain=true: the driver is exiting — no further events.
            s3.poison(&why, &session_poison, true);
        }
    });

    AreaQueue {
        shared,
        sink: CommandSink::Chan(tx),
        tasks: tokio::sync::Mutex::new(vec![reader, writer]),
        driver: std::sync::Mutex::new(None),
        area,
    }
}

/// The SIM receive-extent cap (test lever `SQUEEZEFS_ZCRX_LANE_SIM_CHUNK`
/// shrinks the chunk GEOMETRY at arm; this is just its readback for the
/// recv call).
fn recv_cap(area: &ZcrxArea) -> usize {
    area.chunk_bytes()
}

/// The SIM driver: recv into freshly-granted chunks (standing in for NIC
/// DMA), push extents through the parser, apply events to the fill
/// table. Grants are taken only once the socket is readable so an idle
/// queue holds ZERO chunks (the recycle-law diagnostics stay exact).
async fn sim_reader_loop(
    read_half: OwnedReadHalf,
    area: &Arc<ZcrxArea>,
    shared: &AreaShared,
) -> Result<(), String> {
    let metrics = &crate::fuse_client::METRICS;
    let mut parser = StreamParser::new();
    let mut events: Vec<ParseEvent> = Vec::new();
    loop {
        read_half
            .readable()
            .await
            .map_err(|e| format!("socket readiness: {e}"))?;
        let grant = area.grant_chunk().await;
        let ptr = grant.chunk_ptr();
        // SAFETY: a fresh grant is exclusively this driver's until a
        // slice of it is published; the extent stays within the chunk.
        let buf = unsafe { std::slice::from_raw_parts_mut(ptr, recv_cap(area)) };
        match read_half.try_read(buf) {
            Ok(0) => {
                // EOF with nothing pending = orderly teardown.
                if shared.table.is_empty() {
                    return Ok(());
                }
                return Err("connection closed mid-operation".into());
            }
            Ok(n) => {
                // SAFETY (MEM-4): `ptr` is the base of the chunk `grant`
                // holds (so the region cannot recycle and the area stays
                // mapped) and `n` bytes of it were just filled by the
                // read — `n <= chunk_bytes` because `buf` is that chunk.
                let slice = unsafe { AreaSlice::new(grant, ptr as *const u8, n) };
                events.clear();
                if let Err(why) = parser.push(&slice, &mut events) {
                    metrics
                        .zcrx_frame_violations
                        .fetch_add(1, Ordering::Relaxed);
                    return Err(why);
                }
                drop(slice); // the recv pass ref; payload spans hold theirs
                for ev in events.drain(..) {
                    if let Err(why) = shared.table.apply(ev) {
                        metrics
                            .zcrx_frame_violations
                            .fetch_add(1, Ordering::Relaxed);
                        return Err(why);
                    }
                }
            }
            Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                // Spurious readiness: return the untouched chunk.
                drop(grant);
                continue;
            }
            Err(e) => return Err(format!("socket read: {e}")),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn admission_derives_from_the_fill_window() {
        let window = 64 << 20;
        assert_eq!(
            admission_permits(window, 4096),
            (window / 2) / ADMISSION_UNIT,
            "half the FILL WINDOW (never the whole area) admits payload"
        );
        assert!(admission_permits(4096, 4096) >= 1, "floor one unit");
    }

    #[test]
    fn ring_standing_bytes_covers_the_field_geometry() {
        // Round 6 kernel adjudication: the RX ring holds pool buffers
        // for the queue's LIFETIME (io_uring/zcrx.c: the provider pool
        // is the ONLY buffer source once the queue restarts onto it).
        // Field rail: 8192 descriptors, MTU 9000, 4 KiB chunks ⇒
        // 8192 × 3 × 4096 = 96 MiB standing — the instant-exhaustion
        // arithmetic against the 64 MiB round-5 area, and why failover
        // releases could never refill the pool.
        assert_eq!(
            ring_standing_bytes(8192, Some(9000), 4096),
            8192 * 3 * 4096,
            "the field rail's standing demand"
        );
        assert_eq!(
            ring_standing_bytes(8192, Some(1500), 4096),
            8192 * 4096,
            "1500-MTU: one chunk per descriptor"
        );
        assert_eq!(
            ring_standing_bytes(1024, None, 4096),
            1024 * 4096,
            "unknown MTU degrades to one chunk per descriptor (the floor)"
        );
        assert_eq!(ring_standing_bytes(0, Some(9000), 4096), 0, "no probe => 0");
    }
}
