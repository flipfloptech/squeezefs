//! First-party signal delivery (the rip-tokio-total program) — replaces
//! `tokio::signal` in the mount foreground loop. Classic self-pipe: the
//! `sigaction` handler writes ONE byte (async-signal-safe) into a
//! non-blocking pipe; the `sqz-signal` watcher thread reads and forwards
//! onto an sqz channel the async shutdown loop recvs from.
//!
//! Process-global and armed ONCE (the mount foreground is the only
//! consumer); re-arming returns the same receiver slot refusal loudly.

use std::sync::atomic::{AtomicI32, Ordering};
use std::sync::Mutex;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Sig {
    /// SIGINT (Ctrl+C).
    Int,
    /// SIGTERM.
    Term,
}

static PIPE_WR: AtomicI32 = AtomicI32::new(-1);

/// SAFETY contract: async-signal-safe — one `write(2)` on a pre-opened
/// non-blocking pipe fd, nothing else.
extern "C" fn on_signal(signo: libc::c_int) {
    let fd = PIPE_WR.load(Ordering::Relaxed);
    if fd >= 0 {
        let byte: u8 = if signo == libc::SIGTERM { b'T' } else { b'I' };
        // SAFETY: fd is a live pipe write end; a full pipe drops the
        // byte (EAGAIN), which is fine — signal delivery coalesces.
        unsafe {
            let _ = libc::write(fd, &byte as *const u8 as *const libc::c_void, 1);
        }
    }
}

/// Arm SIGINT+SIGTERM delivery; returns the receiver the shutdown loop
/// recvs from. One arm per process — a second call refuses loudly.
pub fn arm() -> std::io::Result<squeezefs_ipc::sqz_channel::mpsc::Receiver<Sig>> {
    static ARMED: Mutex<bool> = Mutex::new(false);
    {
        let mut armed = ARMED.lock().unwrap_or_else(|e| e.into_inner());
        if *armed {
            return Err(std::io::Error::other("signal delivery already armed"));
        }
        *armed = true;
    }

    let mut fds = [0i32; 2];
    // SAFETY: plain pipe2; CLOEXEC so root subprocesses never inherit,
    // NONBLOCK so the handler can never block.
    let rc = unsafe { libc::pipe2(fds.as_mut_ptr(), libc::O_CLOEXEC | libc::O_NONBLOCK) };
    if rc != 0 {
        return Err(std::io::Error::last_os_error());
    }
    let (rd, wr) = (fds[0], fds[1]);
    PIPE_WR.store(wr, Ordering::Release);

    // SAFETY: standard sigaction installation; the handler is
    // async-signal-safe by the contract above. SA_RESTART keeps
    // unrelated syscalls unperturbed.
    unsafe {
        let mut sa: libc::sigaction = std::mem::zeroed();
        sa.sa_sigaction = on_signal as usize;
        sa.sa_flags = libc::SA_RESTART;
        libc::sigemptyset(&mut sa.sa_mask);
        for signo in [libc::SIGINT, libc::SIGTERM] {
            if libc::sigaction(signo, &sa, std::ptr::null_mut()) != 0 {
                return Err(std::io::Error::last_os_error());
            }
        }
    }

    let (tx, rx) = squeezefs_ipc::sqz_channel::mpsc::channel::<Sig>(16);
    std::thread::Builder::new()
        .name("sqz-signal".to_string())
        .spawn(move || {
            let mut buf = [0u8; 16];
            loop {
                // Blocking-read emulation on the nonblocking read end:
                // poll(2) parks the thread until a byte arrives.
                let mut pfd = libc::pollfd {
                    fd: rd,
                    events: libc::POLLIN,
                    revents: 0,
                };
                // SAFETY: one pollfd on a live pipe read end.
                let n = unsafe { libc::poll(&mut pfd, 1, -1) };
                if n < 0 {
                    let err = std::io::Error::last_os_error();
                    if err.kind() == std::io::ErrorKind::Interrupted {
                        continue;
                    }
                    log::error!("sqz-signal poll failed: {err}");
                    return;
                }
                // SAFETY: reading into a stack buffer from the pipe.
                let n = unsafe { libc::read(rd, buf.as_mut_ptr() as *mut libc::c_void, buf.len()) };
                if n <= 0 {
                    continue;
                }
                for &b in &buf[..n as usize] {
                    let sig = if b == b'T' { Sig::Term } else { Sig::Int };
                    // A dropped receiver = the foreground loop exited;
                    // the default disposition takes over at process end.
                    if tx.try_send(sig).is_err() {
                        return;
                    }
                }
            }
        })
        .map_err(std::io::Error::other)?;

    Ok(rx)
}
