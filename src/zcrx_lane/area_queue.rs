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

/// Admission accounting grain — ONE definition ([`super::area`]).
pub(crate) use super::area::ADMISSION_UNIT;

// Admission permits: since the 2026-08-06 engagement campaign the
// admitted window is granted in FULL (`super::area::admission_permits`
// — the sizing-law home); the retired /2's delivery-slack budget lives
// in the AREA size (`super::area::delivery_slack_bytes`).
//
// Round 6 kernel adjudication (io_uring/zcrx.c @ v6.19.14) still
// stands underneath: the round-5 wire-grain `amp` model is DELETED —
// falsified by the field (MTU 9000 ⇒ amp 1, still instant exhaustion)
// and by the source (`io_zcrx_copy_chunk` packs fallback niovs FULLY
// via io_copy_page — no per-segment page burn). The real structural
// consumer is the NIC RX ring itself (see
// [`super::area::ring_standing_bytes`]).

// The NIC RX ring's standing provider-pool demand lives in
// `super::area::ring_standing_bytes` (round 3: it gained the
// striding-RQ/MPWQE model beside the round-6 legacy one — the sizing
// laws live in area.rs; the round-6 caveat "striding-RQ fleets
// over-provision" was FALSIFIED by the field: striding demand is
// LARGER at jumbo MTU — 128 MiB vs 96 MiB on the field rail).

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
    /// Round-8 no-harm escalation: the starvation proved STRUCTURAL
    /// ([`super::uring_zcrx::REFILL_STRUCTURAL_FAILOVERS`] consecutive
    /// failover windows) — the funnel tears the session down so the
    /// RSS width restores (prolonged-degraded ≡ poisoned in lifecycle
    /// terms).
    pub starved_structural: AtomicBool,
}

impl AreaShared {
    /// `admit_window_len` is the ADMITTED payload window — granted in
    /// full (2026-08-06 engagement law; the caller derives its venue's
    /// delivery slack into the AREA size, never into this semaphore).
    pub(crate) fn new(depth: u16, area: &ZcrxArea, admit_window_len: usize) -> Arc<AreaShared> {
        Arc::new(AreaShared {
            table: FillTable::new(),
            free_cids: Arc::new(std::sync::Mutex::new((0..depth).collect())),
            cid_gate: Arc::new(tokio::sync::Semaphore::new(depth as usize)),
            cid_capacity: depth as usize,
            admission: Arc::new(tokio::sync::Semaphore::new(super::area::admission_permits(
                admit_window_len,
                area.chunk_bytes(),
            ))),
            poisoned: AtomicBool::new(false),
            starved: AtomicBool::new(false),
            starved_structural: AtomicBool::new(false),
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
            log::error!("zcrx-lane: IO queue poisoned: {why}");
            mark_session_poisoned(session_poison, why);
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
    // Sim: no NIC ring exists and the delivery grain is the loopback
    // try_read (unprobed — no MTU to derive from), so HALF the area is
    // the admitted window and the other half stays the slack budget:
    // byte-identical to every Z2 contract's arithmetic (the real
    // backend's slack is derived into the AREA size instead —
    // `super::area::delivery_slack_bytes`).
    let shared = AreaShared::new(depth, &area, area.len() / 2);
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
    fn admission_grants_the_full_admitted_window() {
        // 2026-08-06 engagement law: the caller's admitted window grants
        // in FULL (the /2 retired — its slack budget lives in the AREA
        // size, `super::super::area::delivery_slack_bytes`).
        let window = 64 << 20;
        assert_eq!(
            super::super::area::admission_permits(window, 4096),
            window / ADMISSION_UNIT,
            "the WHOLE admitted window admits payload"
        );
        assert!(
            super::super::area::admission_permits(4096, 4096) >= 1,
            "floor one unit"
        );
    }
}
