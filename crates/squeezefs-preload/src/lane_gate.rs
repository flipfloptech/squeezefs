//! The **hybrid lane gate** (ruling D14's fleet-posture corollary,
//! formalized inside the shim, 2026-08-07): with kernel-lane reads AND
//! writes at zc, the interception shim's charter narrows toward the
//! IOPS/rand-small lane — so LARGE ops route to the kernel FUSE lane
//! (the `RingServe::Real` fallthrough the availability valve already
//! proves out: poison law, binding lifetime, errno semantics all
//! preserved) and SMALL ops ride the IPC ring.
//!
//! This module owns the gate's pure pieces:
//!
//! * [`kernel_lane_route`] — the ONE routing predicate (strictly greater
//!   than the threshold routes kernel; `0` = gate off, the A/B lever);
//! * [`resolve_kernel_lane_min`] — `SQUEEZEFS_IL_KERNEL_LANE_MIN` under
//!   the ONE env convention (explicit wins verbatim; malformed announces
//!   and keeps the derived default — the shim's documented ENG-10
//!   asymmetry);
//! * [`probe_mem_bw_bytes_per_sec`] — the startup memcpy micro-probe
//!   (the NT-copy-floor class of machinery: a RUNTIME-derived input,
//!   never a CPU-ID table — portable-by-default), memoized per process
//!   by [`probed_mem_bw`];
//! * [`kernel_lane_min_for_geometry`] — the composed per-session
//!   threshold: probe → the shared derivation
//!   [`squeezefs_ipc::sizing::kernel_lane_min_default`] (memBW × the
//!   measured lane-RTT delta ÷ 2 copies, 4 KiB grain, clamped to the
//!   physical `[slab, max_op_bytes]` rails) → env override.
//!
//! Placement (the design's §1): POSITIONAL forms (`pread`/`pwrite`/
//! `preadv`/`pwritev`/libaio iocbs) gate PER OP — no `f_pos` state, so
//! routing is free. OFFSETFUL forms (`read`/`write`/`readv`/`writev`)
//! gate through the fd table's **sticky per-description latch**
//! (`fd_table.rs`): every ring→Real transition flushes and permanently
//! demotes the PERF-7 offset mirror, so per-op flapping would price the
//! fd back at the two-syscall resync discipline forever — the latch
//! makes the transition happen exactly once, after which the real call
//! consumes kernel `f_pos` at native cost (zero shim syscalls).
//! O_DIRECT is deliberately NOT consulted in v1 (the strongest
//! kernel-lane candidate is direct+large, but the size gate alone
//! already routes it; the binding's classify-time flags are the hook if
//! a counted row ever prices the refinement in).

use std::sync::OnceLock;

/// The ONE routing predicate: `true` = this op rides the kernel FUSE
/// lane (the real call). Strictly-greater: the threshold is the LARGEST
/// op the ring keeps, so the derived floor (= the slot slab) leaves the
/// single-flight serial path — the IOPS lane's own regime — untouched.
/// `threshold == 0` disables the gate entirely (the A/B lever).
#[inline]
pub fn kernel_lane_route(len: u64, threshold_bytes: u64) -> bool {
    threshold_bytes != 0 && len > threshold_bytes
}

/// `SQUEEZEFS_IL_KERNEL_LANE_MIN` under the ONE convention: absent ⇒
/// `derived`; explicit ⇒ VERBATIM (0 = gate off; sub-slab values are an
/// override lever's prerogative — no clamp); malformed ⇒ `derived` (the
/// caller announces — the shim never kills its host app over a typo).
pub fn resolve_kernel_lane_min(env: Option<&str>, derived: u64) -> u64 {
    match crate::env_knob_core::parse_int::<u64>("SQUEEZEFS_IL_KERNEL_LANE_MIN", env) {
        Ok(Some(v)) => v,
        Ok(None) | Err(_) => derived,
    }
}

/// One memcpy micro-probe pass set: best-of-3 copies of an 4 MiB buffer
/// (larger than L2 on every target part, so the pass prices the
/// cache-miss-bearing copy the ring actually pays on large payloads;
/// best-of because interference only ever SLOWS a pass). Costs ~1 ms
/// once per process. Returns 0 on degenerate timing (caller's derivation
/// then lands on the slab floor — conservative).
pub fn probe_mem_bw_bytes_per_sec() -> u64 {
    const PROBE_BYTES: usize = 4 << 20;
    const PASSES: u32 = 3;
    let src = vec![0xA5u8; PROBE_BYTES];
    let mut dst = vec![0u8; PROBE_BYTES];
    // Warm pass (page-faults both buffers in) — untimed.
    dst.copy_from_slice(&src);
    let mut best: u64 = 0;
    for _ in 0..PASSES {
        let t0 = std::time::Instant::now();
        dst.copy_from_slice(&src);
        std::hint::black_box(&mut dst);
        let dt = t0.elapsed().as_nanos();
        if dt > 0 {
            let bw = (PROBE_BYTES as u128)
                .saturating_mul(1_000_000_000)
                .checked_div(dt)
                .unwrap_or(0) as u64;
            best = best.max(bw);
        }
    }
    best
}

/// Process-memoized probe (run lazily at the first session establish —
/// never in a constructor; the interposer guard is held there, so the
/// probe's own allocation/timing never re-enters interception).
pub fn probed_mem_bw() -> u64 {
    static BW: OnceLock<u64> = OnceLock::new();
    *BW.get_or_init(probe_mem_bw_bytes_per_sec)
}

/// The composed per-session threshold: probed memBW through the shared
/// derivation, railed by THIS session's geometry, then the env override.
/// A malformed override is announced ONCE per process (stderr, the
/// interposer environment's loudness) and the derived default stands.
pub fn kernel_lane_min_for_geometry(slab_bytes: u64, max_op_bytes: u64) -> u64 {
    let derived =
        squeezefs_ipc::sizing::kernel_lane_min_default(probed_mem_bw(), slab_bytes, max_op_bytes);
    let raw = std::env::var("SQUEEZEFS_IL_KERNEL_LANE_MIN").ok();
    if let Err(e) =
        crate::env_knob_core::parse_int::<u64>("SQUEEZEFS_IL_KERNEL_LANE_MIN", raw.as_deref())
    {
        static ANNOUNCED: std::sync::Once = std::sync::Once::new();
        ANNOUNCED.call_once(|| {
            let msg = format!("squeezefs-il: {e} — keeping the derived default ({derived})\n");
            // SAFETY: plain write(2) to stderr; best-effort, establish
            // context only (never a signal handler).
            unsafe {
                libc::write(2, msg.as_ptr() as *const libc::c_void, msg.len());
            }
        });
    }
    resolve_kernel_lane_min(raw.as_deref(), derived)
}
