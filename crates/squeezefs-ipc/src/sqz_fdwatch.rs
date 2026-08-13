//! First-party fd-readability watcher (the rip-tokio-total program,
//! 2026-08-13) — replaces the last tokio REACTOR dependence in the
//! product: the classical FUSE rings' `AsyncFd<eventfd>` completion
//! waits (`fuse3` `connection/tokio.rs`). One `sqz-fdwatch` OS thread
//! parks in `epoll_wait` and fires registered wakers; the awaiting
//! future's poll delivery rides whatever executor polls it.
//!
//! **This file is `#[path]`-shared** (the `wake_core`/`thp.rs`
//! precedent): it is NOT declared in `squeezefs-ipc`'s lib.rs (libc is
//! optional there) — including crates (`fuse3`, the root crate) own the
//! `libc` dependency and a sibling `crate::sqz_channel` module (for the
//! ticked backstop).
//!
//! Contract (deliberately narrower than tokio's `AsyncFd` — KISS):
//! * [`readable`] registers `fd` EPOLLIN **oneshot** for THIS await and
//!   deregisters on completion or drop — level-triggered semantics per
//!   await, so "drain then re-await" call sites port 1:1 and a spurious
//!   wake is absorbed by the caller's drain loop.
//! * One concurrent awaiter per fd (the ring-servicing-loop shape).
//!   A second registration of a live fd replaces the first loudly (the
//!   first waiter completes with `Replaced` — a bug surfacing, never a
//!   silent starve).
//! * The park is tick-backstopped ([`crate::sqz_channel::ticked`]): a
//!   lost wake costs one TICK, never a wedge.

use std::collections::HashMap;
use std::future::Future;
use std::os::fd::RawFd;
use std::pin::Pin;
use std::sync::{Mutex, OnceLock};
use std::task::{Context, Poll, Waker};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FdEvent {
    /// EPOLLIN (or HUP/ERR — the caller's next read sees the error).
    Ready,
    /// A newer awaiter replaced this registration (one-awaiter contract
    /// violation surfacing loudly).
    Replaced,
}

enum SlotState {
    Waiting(Option<Waker>),
    Fired(FdEvent),
}

/// One registration; `gen` disambiguates a replaced awaiter's slot from
/// its replacement's (a replaced future's drop must never deregister
/// the live successor).
struct Slot {
    gen: u64,
    state: SlotState,
}

struct Watch {
    epfd: RawFd,
    slots: Mutex<HashMap<RawFd, Slot>>,
    next_gen: std::sync::atomic::AtomicU64,
}

fn watch() -> &'static Watch {
    static W: OnceLock<&'static Watch> = OnceLock::new();
    W.get_or_init(|| {
        // SAFETY: plain epoll fd creation; CLOEXEC so children never
        // inherit the reactor fd.
        let epfd = unsafe { libc::epoll_create1(libc::EPOLL_CLOEXEC) };
        assert!(epfd >= 0, "epoll_create1 failed: {}", std::io::Error::last_os_error());
        let w: &'static Watch = Box::leak(Box::new(Watch {
            epfd,
            slots: Mutex::new(HashMap::new()),
            next_gen: std::sync::atomic::AtomicU64::new(1),
        }));
        std::thread::Builder::new()
            .name("sqz-fdwatch".to_string())
            .spawn(move || service_loop(w))
            .expect("sqz-fdwatch thread spawns");
        w
    })
}

fn service_loop(w: &'static Watch) {
    let mut events = [libc::epoll_event { events: 0, u64: 0 }; 16];
    loop {
        // SAFETY: valid epfd + a stack event buffer; -1 = park until an
        // event (registrations use EPOLL_CTL so no wakeup fd is needed).
        let n = unsafe {
            libc::epoll_wait(w.epfd, events.as_mut_ptr(), events.len() as i32, -1)
        };
        if n < 0 {
            let err = std::io::Error::last_os_error();
            if err.kind() == std::io::ErrorKind::Interrupted {
                continue;
            }
            // A broken reactor must be LOUD, never a silent spin.
            panic!("sqz-fdwatch epoll_wait failed: {err}");
        }
        let mut wakers: Vec<Waker> = Vec::new();
        {
            let mut slots = w.slots.lock().unwrap_or_else(|e| e.into_inner());
            for ev in events.iter().take(n as usize) {
                let fd = ev.u64 as RawFd;
                if let Some(slot) = slots.get_mut(&fd) {
                    let prev =
                        std::mem::replace(&mut slot.state, SlotState::Fired(FdEvent::Ready));
                    if let SlotState::Waiting(Some(wk)) = prev {
                        wakers.push(wk);
                    }
                }
            }
        }
        for wk in wakers {
            wk.wake();
        }
    }
}

/// Await EPOLLIN readability of `fd` (oneshot, level-triggered per
/// await; see the module contract). Tick-backstopped.
pub async fn readable(fd: RawFd) -> std::io::Result<FdEvent> {
    let fut = ReadableFut { fd, reg_gen: None };
    crate::sqz_channel::ticked(fut).await
}

struct ReadableFut {
    fd: RawFd,
    /// Our live registration's generation (None = not registered).
    reg_gen: Option<u64>,
}

impl Unpin for ReadableFut {}

impl Future for ReadableFut {
    type Output = std::io::Result<FdEvent>;
    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let w = watch();
        let mut slots = w.slots.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(my_gen) = self.reg_gen {
            match slots.get_mut(&self.fd) {
                Some(slot) if slot.gen == my_gen => match &mut slot.state {
                    SlotState::Fired(ev) => {
                        let ev = *ev;
                        slots.remove(&self.fd);
                        drop(slots);
                        self.reg_gen = None;
                        // Oneshot auto-disarmed in the kernel; drop the
                        // epoll registration too.
                        epoll_del(w.epfd, self.fd);
                        return Poll::Ready(Ok(ev));
                    }
                    SlotState::Waiting(wk) => {
                        *wk = Some(cx.waker().clone());
                        return Poll::Pending;
                    }
                },
                // Slot gone or owned by a successor: we were replaced
                // (contract violation surfacing) — finish loudly.
                _ => {
                    self.reg_gen = None;
                    return Poll::Ready(Ok(FdEvent::Replaced));
                }
            }
        }
        // Fresh registration (EPOLLONESHOT: exactly one firing per await).
        let gen = w
            .next_gen
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let prev = slots.insert(
            self.fd,
            Slot {
                gen,
                state: SlotState::Waiting(Some(cx.waker().clone())),
            },
        );
        let replaced_waker = match prev {
            Some(Slot {
                state: SlotState::Waiting(Some(wk)),
                ..
            }) => Some(wk),
            _ => None,
        };
        let mut ev = libc::epoll_event {
            events: (libc::EPOLLIN | libc::EPOLLONESHOT) as u32,
            u64: self.fd as u64,
        };
        // SAFETY: fd owned by the caller for the await's duration (the
        // future's drop deregisters before the caller can close it in
        // the ring teardown order).
        let rc = unsafe { libc::epoll_ctl(w.epfd, libc::EPOLL_CTL_MOD, self.fd, &mut ev) };
        let rc = if rc < 0
            && std::io::Error::last_os_error().raw_os_error() == Some(libc::ENOENT)
        {
            unsafe { libc::epoll_ctl(w.epfd, libc::EPOLL_CTL_ADD, self.fd, &mut ev) }
        } else {
            rc
        };
        if rc < 0 {
            let err = std::io::Error::last_os_error();
            slots.remove(&self.fd);
            return Poll::Ready(Err(err));
        }
        drop(slots);
        if let Some(wk) = replaced_waker {
            wk.wake(); // the replaced awaiter re-polls and sees Replaced
        }
        self.reg_gen = Some(gen);
        Poll::Pending
    }
}

impl Drop for ReadableFut {
    fn drop(&mut self) {
        if let Some(my_gen) = self.reg_gen {
            let w = watch();
            let mut slots = w.slots.lock().unwrap_or_else(|e| e.into_inner());
            // Only OUR generation: a replaced future must never
            // deregister its live successor.
            if slots.get(&self.fd).map(|s| s.gen) == Some(my_gen) {
                slots.remove(&self.fd);
                drop(slots);
                epoll_del(w.epfd, self.fd);
            }
        }
    }
}

fn epoll_del(epfd: RawFd, fd: RawFd) {
    // SAFETY: removing a registration; ENOENT (already gone) is fine.
    unsafe {
        libc::epoll_ctl(epfd, libc::EPOLL_CTL_DEL, fd, std::ptr::null_mut());
    }
}
