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
use super::initiator::{mark_session_poisoned, WriterMsg};
use super::pdu_stream::{ParseEvent, StreamParser};
use crate::error::{Result, SqueezefsError};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

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

/// Owned, splittable admission-unit custody over a queue's admission
/// semaphore — the tokio `OwnedSemaphorePermit` shape the MEM-3
/// admission face rides (rip-tokio-total: `sqz_semaphore` has no
/// many-owned try-acquire or `split`, so custody is carried here —
/// the units return via `add_permits` exactly when the holder drops,
/// never early). The admission semaphore is TRY-only by design (round
/// 8: over-demand declines, never parks), so no waiter fairness is in
/// play.
pub(crate) struct AdmissionUnits {
    sem: Arc<squeezefs_ipc::sqz_semaphore::Semaphore>,
    units: usize,
}

impl AdmissionUnits {
    /// All-or-nothing take of `units` (never waits — the round-8
    /// decline law's primitive).
    pub(crate) fn try_take(
        sem: &Arc<squeezefs_ipc::sqz_semaphore::Semaphore>,
        units: u32,
    ) -> std::result::Result<AdmissionUnits, squeezefs_ipc::sqz_semaphore::TryAcquireError> {
        // `forget` transfers the borrowed permit's units into this
        // owned holder; Drop below is the one return path.
        sem.try_acquire_many(units)?.forget();
        Ok(AdmissionUnits {
            sem: Arc::clone(sem),
            units: units as usize,
        })
    }

    /// Carve `n` units off into a new holder (the per-segment split of
    /// a whole-read admission); `None` if fewer than `n` remain.
    pub(crate) fn split(&mut self, n: usize) -> Option<AdmissionUnits> {
        if n > self.units {
            return None;
        }
        self.units -= n;
        Some(AdmissionUnits {
            sem: Arc::clone(&self.sem),
            units: n,
        })
    }
}

impl Drop for AdmissionUnits {
    fn drop(&mut self) {
        if self.units > 0 {
            self.sem.add_permits(self.units);
        }
    }
}

/// Shared state of one area queue.
pub(crate) struct AreaShared {
    pub table: FillTable,
    /// Free CID pool (std mutex — push/pop only, no await inside; shared
    /// with [`super::initiator::CidSlot`] custody. Lock order where both
    /// are held: fill-table `pending` → `free_cids`).
    pub free_cids: Arc<std::sync::Mutex<Vec<u16>>>,
    pub cid_gate: Arc<squeezefs_ipc::sqz_semaphore::Semaphore>,
    /// CID namespace size (== queue depth) — the `cid_slots` diagnostic's
    /// denominator (the MEM-3 no-leak instrument).
    pub cid_capacity: usize,
    /// In-flight payload admission (see [`admission_permits`]); Arc'd so
    /// OWNED units ([`AdmissionUnits`]) can ride the pending fill → the
    /// completed [`super::fill_table::ZcrxFill`] (exact accounting under
    /// cancellation — the MEM-3 custody law's admission face).
    pub admission: Arc<squeezefs_ipc::sqz_semaphore::Semaphore>,
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
            cid_gate: Arc::new(squeezefs_ipc::sqz_semaphore::Semaphore::new(depth as usize)),
            cid_capacity: depth as usize,
            admission: Arc::new(squeezefs_ipc::sqz_semaphore::Semaphore::new(
                super::area::admission_permits(admit_window_len, area.chunk_bytes()),
            )),
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

/// How a queue's capsules reach its wire: the sim's writer OS thread or
/// the real ring driver's doorbell lane.
pub(crate) enum CommandSink {
    Chan(std::sync::mpsc::Sender<WriterMsg>),
    Ring(Arc<super::uring_zcrx::RingCmd>),
}

impl CommandSink {
    pub(crate) fn send(&self, capsule: Vec<u8>) -> std::result::Result<(), ()> {
        match self {
            CommandSink::Chan(tx) => tx.send(WriterMsg::Capsule(capsule)).map_err(|_| ()),
            CommandSink::Ring(cmds) => cmds.send(capsule),
        }
    }
}

pub(crate) struct AreaQueue {
    pub shared: Arc<AreaShared>,
    pub sink: CommandSink,
    /// Sim reader + writer OS threads — shutdown-and-join is the
    /// quiescence law (poison drain; timeout path): shutting `socket`
    /// down makes both blocking loops return, and the JOIN proves no
    /// lane context survives holding chunk refs. Empty on the ring
    /// backend.
    pub threads: std::sync::Mutex<Vec<std::thread::JoinHandle<()>>>,
    /// The sim backend's kept socket handle — the shutdown half of
    /// shutdown-and-join (None on the ring backend: its wire is owned
    /// by the driver thread and stopped via the doorbell close).
    pub socket: Option<std::net::TcpStream>,
    /// The real backend's driver thread (None on the sim backend).
    pub driver: std::sync::Mutex<Option<std::thread::JoinHandle<()>>>,
    pub area: Arc<ZcrxArea>,
}

impl AreaQueue {
    /// The signal half of the quiescence law: close the ring doorbell /
    /// send the writer Stop AND shut the socket down (belt and braces —
    /// a blocking read/write on a shutdown socket returns an error, so
    /// both sim loops exit). Never blocks.
    pub(crate) fn signal_stop(&self) {
        match &self.sink {
            CommandSink::Ring(cmds) => cmds.close(),
            CommandSink::Chan(tx) => {
                let _ = tx.send(WriterMsg::Stop);
            }
        }
        if let Some(s) = &self.socket {
            let _ = s.shutdown(std::net::Shutdown::Both);
        }
    }

    /// Take every joinable handle (sim threads + ring driver) — the
    /// join half of shutdown-and-join. Idempotent (second call yields
    /// nothing).
    pub(crate) fn take_handles(&self) -> Vec<std::thread::JoinHandle<()>> {
        let mut hs: Vec<std::thread::JoinHandle<()>> = self
            .threads
            .lock()
            .expect("queue threads lock")
            .drain(..)
            .collect();
        if let Some(h) = self.driver.lock().expect("driver handle lock").take() {
            hs.push(h);
        }
        hs
    }

    /// The drain half of the quiescence law: stop the wire (close the
    /// ring doorbell / Stop + socket shutdown for the sim threads), then
    /// JOIN every driver so no lane context survives holding chunk refs
    /// — the join is the PROOF of quiescence (stronger than the retired
    /// abort+join: the loops ran to completion on a dead socket).
    pub(crate) async fn drain(&self) {
        self.signal_stop();
        let handles = self.take_handles();
        if !handles.is_empty() {
            squeezefs_ipc::sqz_blocking::run_blocking(move || {
                for h in handles {
                    let _ = h.join();
                }
            })
            .await;
        }
    }
}

/// Spawn the SIM area queue over an established (post-Connect) stream.
pub(crate) fn spawn_area_queue(
    stream: std::net::TcpStream,
    qid: u16,
    depth: u16,
    area: Arc<ZcrxArea>,
    session_poison: Arc<AtomicBool>,
) -> Result<AreaQueue> {
    // Sim: no NIC ring exists and the delivery grain is the loopback
    // recv extent (unprobed — no MTU to derive from), so HALF the area
    // is the admitted window and the other half stays the slack budget:
    // byte-identical to every Z2 contract's arithmetic (the real
    // backend's slack is derived into the AREA size instead —
    // `super::area::delivery_slack_bytes`).
    let shared = AreaShared::new(depth, &area, area.len() / 2);
    let sock_err = |what: &str, e: std::io::Error| {
        SqueezefsError::Io(std::io::Error::other(format!("lane sim queue {what}: {e}")))
    };
    let writer_sock = stream.try_clone().map_err(|e| sock_err("clone", e))?;
    let mut reader_sock = stream.try_clone().map_err(|e| sock_err("clone", e))?;
    let (tx, rx) = std::sync::mpsc::channel();

    let s2 = Arc::clone(&shared);
    let p2 = Arc::clone(&session_poison);
    let writer = std::thread::Builder::new()
        .name(format!("sqz-zcrx-wr{qid}"))
        .spawn(move || {
            if let Err(why) = super::initiator::writer_loop(writer_sock, rx) {
                // drain=false: the reader/driver may still be applying events
                // — its own exit performs the drain (MEM-3 drain discipline).
                s2.poison(&why, &p2, false);
            }
        })
        .map_err(|e| sock_err("writer thread spawn", e))?;
    let s3 = Arc::clone(&shared);
    let a3 = Arc::clone(&area);
    let reader = std::thread::Builder::new()
        .name(format!("sqz-zcrx-rd{qid}"))
        .spawn(move || {
            if let Err(why) = sim_reader_loop(&mut reader_sock, &a3, &s3) {
                // drain=true: the driver is exiting — no further events.
                s3.poison(&why, &session_poison, true);
            }
        })
        .map_err(|e| sock_err("reader thread spawn", e))?;

    Ok(AreaQueue {
        shared,
        sink: CommandSink::Chan(tx),
        threads: std::sync::Mutex::new(vec![reader, writer]),
        socket: Some(stream),
        driver: std::sync::Mutex::new(None),
        area,
    })
}

/// The SIM receive-extent cap (test lever `SQUEEZEFS_ZCRX_LANE_SIM_CHUNK`
/// shrinks the chunk GEOMETRY at arm; this is just its readback for the
/// recv call).
fn recv_cap(area: &ZcrxArea) -> usize {
    area.chunk_bytes()
}

/// Park until `sock` is readable (POLLIN — an EOF/HUP also reads as
/// readable, so a socket shutdown always releases the parked reader).
/// The grant-after-readable ordering below depends on this: the sim
/// thread must never hold a granted chunk while parked on the wire.
fn wait_readable(sock: &std::net::TcpStream) -> std::result::Result<(), String> {
    use std::os::fd::AsRawFd;
    let mut pfd = libc::pollfd {
        fd: sock.as_raw_fd(),
        events: libc::POLLIN,
        revents: 0,
    };
    loop {
        // SAFETY: one valid pollfd, infinite timeout; EINTR retried.
        let rc = unsafe { libc::poll(&mut pfd, 1, -1) };
        if rc > 0 {
            return Ok(());
        }
        let err = std::io::Error::last_os_error();
        if rc < 0 && err.kind() != std::io::ErrorKind::Interrupted {
            return Err(format!("socket readiness: {err}"));
        }
    }
}

/// The SIM driver (a dedicated OS thread — `sqz-zcrx-rd{N}`): recv into
/// freshly-granted chunks (standing in for NIC DMA), push extents
/// through the parser, apply events to the fill table. Grants are taken
/// only once the socket is readable so an idle queue holds ZERO chunks
/// (the recycle-law diagnostics stay exact); a socket shutdown makes
/// both the poll and the read return, so the quiescence join is prompt.
fn sim_reader_loop(
    sock: &mut std::net::TcpStream,
    area: &Arc<ZcrxArea>,
    shared: &AreaShared,
) -> std::result::Result<(), String> {
    use std::io::Read;
    let metrics = &crate::fuse_client::METRICS;
    let mut parser = StreamParser::new();
    let mut events: Vec<ParseEvent> = Vec::new();
    loop {
        wait_readable(sock)?;
        let grant = area.grant_chunk_blocking();
        let ptr = grant.chunk_ptr();
        // SAFETY: a fresh grant is exclusively this driver's until a
        // slice of it is published; the extent stays within the chunk.
        let buf = unsafe { std::slice::from_raw_parts_mut(ptr, recv_cap(area)) };
        match sock.read(buf) {
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
