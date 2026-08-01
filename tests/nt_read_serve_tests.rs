//! `SQUEEZEFS_NT_READ_SERVE` — the read-serve NT-store measurement lever
//! (read-copy-count campaign, 2026-08-02).
//!
//! Contracts (red-first):
//! - **Policy purity + default-OFF**: unlike the write-side DMA-destined
//!   sites (`SQUEEZEFS_NT_COPY`, default on), the read-serve destination
//!   is CPU-read next (kernel commit copy / shim `slab_read`), so the NT
//!   posture is opt-in per the counted-A/B rule — `read_serve_policy_from`
//!   is pure and defaults to disabled.
//! - **Byte exactness with guard bytes** at every alignment/length shape
//!   the serve sites produce (unaligned sources — pooled buffers sliced
//!   at request offsets; 4 KiB-aligned dests — registered payloads).
//! - **Engagement**: with the lever armed in this process
//!   (`SQUEEZEFS_NT_READ_SERVE=1`, floor lowered), a dest-arm serve copy
//!   runs the NT body and feeds `nt_read_serve_bytes` (asserted at the
//!   copy-helper level; the mount-level row engagement is the field
//!   instrument).
//!
//! One test fn: the policy OnceLock reads the env exactly once per
//! process, so the env must be set before ANY helper call.

use squeezefs::nt_copy;

fn pat(i: usize) -> u8 {
    ((i * 31 + 7) % 251) as u8
}

#[test]
fn nt_read_serve_policy_exactness_and_engagement() {
    // Arm the lever for this process BEFORE the first helper call (the
    // policy is cached in a OnceLock).
    std::env::set_var("SQUEEZEFS_NT_READ_SERVE", "1");
    std::env::set_var("SQUEEZEFS_NT_READ_SERVE_MIN", "64");

    // --- Policy purity (pure core, env-independent).
    let p = nt_copy::read_serve_policy_from(None, None);
    assert!(!p.enabled, "read-serve NT must default OFF (opt-in lever)");
    assert_eq!(p.min_bytes, nt_copy::DEFAULT_MIN_BYTES);
    assert!(nt_copy::read_serve_policy_from(Some("1"), None).enabled);
    assert!(nt_copy::read_serve_policy_from(Some("true"), None).enabled);
    assert!(!nt_copy::read_serve_policy_from(Some("0"), None).enabled);
    assert!(!nt_copy::read_serve_policy_from(Some(""), None).enabled);
    assert_eq!(
        nt_copy::read_serve_policy_from(Some("1"), Some("131072")).min_bytes,
        131072
    );
    assert_eq!(
        nt_copy::read_serve_policy_from(Some("1"), Some("junk")).min_bytes,
        nt_copy::DEFAULT_MIN_BYTES,
        "unparseable floor falls back, never panics"
    );

    // --- Byte exactness with guard bytes across the serve-site shapes:
    // unaligned sources (pooled fills sliced at request offsets) into a
    // 4 KiB-aligned dest, lengths straddling the floor and the 16/64 B
    // NT body boundaries.
    const GUARD: usize = 32;
    let layout = std::alloc::Layout::from_size_align(1 << 20, 4096).unwrap();
    // SAFETY: valid non-zero layout.
    let dest_base = unsafe { std::alloc::alloc_zeroed(layout) };
    assert!(!dest_base.is_null());

    for &len in &[1usize, 15, 16, 63, 64, 65, 4096, 65536, 393216] {
        for &src_skew in &[0usize, 1, 7, 13] {
            for &dst_skew in &[0usize, 1, 9] {
                let src: Vec<u8> = (0..len + src_skew).map(pat).collect();
                // Guarded dest region: [guard][payload][guard].
                let total = GUARD + dst_skew + len + GUARD;
                assert!(total <= layout.size());
                // SAFETY: within the allocation.
                unsafe { std::ptr::write_bytes(dest_base, 0xAB, total) };
                let dst = unsafe { dest_base.add(GUARD + dst_skew) };
                let engaged =
                    // SAFETY: src/dst valid for len, non-overlapping.
                    unsafe { nt_copy::read_serve_copy_raw(dst, src[src_skew..].as_ptr(), len) };
                if cfg!(target_arch = "x86_64") {
                    assert_eq!(
                        engaged,
                        len >= 64,
                        "NT body engagement follows the armed floor (len {len})"
                    );
                }
                // Payload exact.
                let out = unsafe { std::slice::from_raw_parts(dst, len) };
                assert!(
                    out.iter().enumerate().all(|(i, &b)| b == pat(i + src_skew)),
                    "byte exactness at len {len} src_skew {src_skew} dst_skew {dst_skew}"
                );
                // Guards untouched.
                let before = unsafe { std::slice::from_raw_parts(dest_base, GUARD + dst_skew) };
                let after = unsafe { std::slice::from_raw_parts(dst.add(len), GUARD) };
                assert!(
                    before.iter().all(|&b| b == 0xAB) && after.iter().all(|&b| b == 0xAB),
                    "guard bytes clobbered at len {len} src_skew {src_skew} dst_skew {dst_skew}"
                );
            }
        }
    }
    // SAFETY: allocated with this layout above.
    unsafe { std::alloc::dealloc(dest_base, layout) };
}
