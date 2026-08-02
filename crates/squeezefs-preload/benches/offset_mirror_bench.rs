//! PERF-7 microbench: the bound-fd **offset-mirror bookkeeping** vs the
//! **`lseek` syscall pair** it replaces on every offsetful shim op
//! (`docs/pre-rc-engineering-spec.md` §9 PERF-7 — `interpose.rs`
//! `ring_offsetful`/`ring_iovec` paid `SEEK_CUR` + `SEEK_SET` per
//! `read()`/`write()`; expected +30–60 % on read()-based drivers).
//!
//! FIELD-derived input shape: the offsetful drivers named by PERF-7
//! (`cp`, `dd`, `tar`) issue sequential same-fd `read()`/`write()`
//! streams — one binding, one description, monotone offset advance.
//! The advance width is irrelevant to the BOOKKEEPING cost (the mirror
//! stores a u64 either way); 128 KiB (dd's common `bs`) is used so the
//! offsets look like the field's. Both sides run syscall-pure loop
//! bodies (no clock reads inside the measured body — criterion times
//! whole batches).
//!
//! Groups:
//! - `armed_op_cycle`: lookup → lock → armed → load → publish — the
//!   full per-op mirror path an offsetful serve pays post-PERF-7.
//! - `lseek_served_seek_cur`: the interposed `lseek(fd, 0, SEEK_CUR)`
//!   mirror arm (lookup → lock → armed → arithmetic → store) — the
//!   ftell idiom that now costs zero syscalls.
//! - `lseek_syscall_pair`: raw `SYS_lseek(SEEK_CUR)` + `SYS_lseek(
//!   SEEK_SET)` against a real tmpfs/disk fd — the replaced cost
//!   (the design's "~100 ns syscalls" claim, re-priced on this box).

use criterion::{criterion_group, criterion_main, Criterion};
use squeezefs_il::fd_table::{mirror_seek, Binding, FdTable};
use std::hint::black_box;

const ADVANCE: u64 = 128 * 1024; // dd bs=128k field shape

fn armed_table() -> FdTable {
    let t = FdTable::new();
    let ep = t.fork_epoch();
    t.bind_with_mirror(
        3,
        Binding {
            binding_id: 1,
            ino: 101,
            read_ok: true,
            write_ok: true,
            session: 0,
        },
        0,
        ep,
    );
    t
}

fn bench_offset_paths(c: &mut Criterion) {
    let mut g = c.benchmark_group("preload_offset_mirror");

    // The replacement: full offsetful-op mirror bookkeeping.
    let t = armed_table();
    g.bench_function("armed_op_cycle", |b| {
        b.iter(|| {
            let (bind, m) = t.lookup_with_mirror(black_box(3)).expect("bound");
            let _l = m.lock_offsets().expect("lock");
            let armed = m.armed();
            let off = m.load();
            let served = m.publish(off.wrapping_add(ADVANCE));
            black_box((bind.binding_id, armed, served))
        })
    });

    // The interposed lseek(fd, 0, SEEK_CUR) serve (ftell idiom).
    let t2 = armed_table();
    g.bench_function("lseek_served_seek_cur", |b| {
        b.iter(|| {
            let (_bind, m) = t2.lookup_with_mirror(black_box(3)).expect("bound");
            let _l = m.lock_offsets().expect("lock");
            if !m.armed() {
                unreachable!("bench fd stays armed");
            }
            let new = mirror_seek(m.load(), 0, libc::SEEK_CUR).expect("arith");
            m.store(new);
            black_box(new)
        })
    });

    // The replaced cost: the SEEK_CUR + SEEK_SET pair on a real fd.
    let path = std::env::temp_dir().join(format!("sqz_perf7_bench_{}", std::process::id()));
    std::fs::write(&path, vec![0u8; 4096]).expect("fixture");
    let cpath = std::ffi::CString::new(path.as_os_str().as_encoded_bytes()).expect("cstring");
    // SAFETY: plain open(2) of the fixture.
    let fd = unsafe { libc::open(cpath.as_ptr(), libc::O_RDONLY) };
    assert!(fd >= 0, "fixture open");
    g.bench_function("lseek_syscall_pair", |b| {
        b.iter(|| {
            // SAFETY: raw lseek syscalls on our own fd — exactly the
            // pair ring_offsetful paid per op pre-PERF-7.
            unsafe {
                let cur = libc::syscall(libc::SYS_lseek, fd, 0i64, libc::SEEK_CUR);
                let set = libc::syscall(libc::SYS_lseek, fd, black_box(0i64), libc::SEEK_SET);
                black_box((cur, set))
            }
        })
    });
    g.finish();
    // SAFETY: closing our fixture fd.
    unsafe { libc::close(fd) };
    let _ = std::fs::remove_file(&path);
}

criterion_group!(benches, bench_offset_paths);
criterion_main!(benches);
