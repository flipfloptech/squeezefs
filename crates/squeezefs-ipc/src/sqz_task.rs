//! First-party task-local storage (the rip-tokio-total program) — the
//! `tokio::task_local!` shape the daemon uses (the il read-dest probe,
//! the authority-free-scope venue marker).
//!
//! Mechanism (executor-agnostic, tokio's own): [`LocalKey::scope`]
//! wraps a future; every poll PUSHES the value onto a thread-local
//! stack and POPS it on poll exit (panic-safe via a drop guard), so
//! the binding follows the task across await points, lanes and
//! threads — whoever polls the scope future sees the binding, nested
//! scopes shadow, and nothing leaks past a poll boundary.

use std::cell::RefCell;
use std::future::Future;
use std::pin::Pin;
use std::task::{Context, Poll};

/// Declare first-party task-locals (the `tokio::task_local!` shape).
#[macro_export]
macro_rules! sqz_task_local {
    ($(#[$attr:meta])* $vis:vis static $name:ident: $ty:ty;) => {
        $(#[$attr])*
        $vis static $name: $crate::sqz_task::LocalKey<$ty> = {
            ::std::thread_local! {
                static __SQZ_TL_SLOT: ::std::cell::RefCell<::std::vec::Vec<$ty>> =
                    const { ::std::cell::RefCell::new(::std::vec::Vec::new()) };
            }
            $crate::sqz_task::LocalKey {
                inner: &__SQZ_TL_SLOT,
            }
        };
    };
}

/// The declared key (see [`sqz_task_local!`]).
pub struct LocalKey<T: 'static> {
    #[doc(hidden)]
    pub inner: &'static std::thread::LocalKey<RefCell<Vec<T>>>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct NotInScope;

impl std::fmt::Display for NotInScope {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "task-local not in scope")
    }
}
impl std::error::Error for NotInScope {}

impl<T: 'static> LocalKey<T> {
    /// Bind `value` for the duration of `fut`: every poll of `fut` (and
    /// everything it awaits) observes it via [`Self::with`] /
    /// [`Self::try_with`].
    pub fn scope<F: Future>(&'static self, value: T, fut: F) -> Scoped<T, F> {
        Scoped {
            key: self,
            value: Some(value),
            fut: Box::pin(fut),
        }
    }

    /// Read the binding; panics when unbound (tokio parity).
    pub fn with<R>(&'static self, f: impl FnOnce(&T) -> R) -> R {
        self.try_with(f)
            .expect("sqz task-local accessed outside its scope")
    }

    /// Read the binding when bound.
    pub fn try_with<R>(&'static self, f: impl FnOnce(&T) -> R) -> Result<R, NotInScope> {
        self.inner.with(|slot| {
            let stack = slot.borrow();
            match stack.last() {
                Some(v) => Ok(f(v)),
                None => Err(NotInScope),
            }
        })
    }
}

// SAFETY-free Unpin: `value` is moved by value in/out of the Option
// (never observed through Pin), and the inner future is heap-pinned.
impl<T: 'static, F: Future> Unpin for Scoped<T, F> {}

pub struct Scoped<T: 'static, F: Future> {
    key: &'static LocalKey<T>,
    /// The binding between polls (moved into the thread-local stack for
    /// each poll; `None` after completion).
    value: Option<T>,
    fut: Pin<Box<F>>,
}

impl<T: 'static, F: Future> Future for Scoped<T, F> {
    type Output = F::Output;
    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        // Scoped is Unpin (value is plain data, fut is Pin<Box>).
        let this: &mut Scoped<T, F> = &mut self;
        let value = this.value.take().expect("Scoped polled after completion");
        // Panic-safe push/pop: the guard pops on EVERY exit path (Ready,
        // Pending, unwind) and hands the value back through the slot.
        struct PopGuard<'a, T: 'static> {
            key: &'static LocalKey<T>,
            back: &'a mut Option<T>,
        }
        impl<T: 'static> Drop for PopGuard<'_, T> {
            fn drop(&mut self) {
                let v = self.key.inner.with(|slot| {
                    slot.borrow_mut()
                        .pop()
                        .expect("sqz task-local stack underflow")
                });
                *self.back = Some(v);
            }
        }
        this.key.inner.with(|slot| slot.borrow_mut().push(value));
        let out = {
            let _guard = PopGuard {
                key: this.key,
                back: &mut this.value,
            };
            this.fut.as_mut().poll(cx)
        };
        if out.is_ready() {
            // Completed: the binding dies with the scope.
            this.value = None;
        }
        out
    }
}

#[cfg(test)]
mod tests {
    crate::sqz_task_local! {
        static PROBE: (u64, usize);
    }

    #[test]
    fn scope_binds_across_awaits_and_unbinds_outside() {
        assert!(PROBE.try_with(|_| ()).is_err(), "unbound before scope");
        let got = crate::sqz_blocking::block_on(PROBE.scope((7, 9), async {
            let a = PROBE.with(|v| v.0);
            crate::sqz_time::sleep(std::time::Duration::from_millis(10)).await;
            // Still bound after the await (possibly a different poll).
            let b = PROBE.with(|v| v.1);
            (a, b)
        }));
        assert_eq!(got, (7, 9));
        assert!(PROBE.try_with(|_| ()).is_err(), "unbound after scope");
    }

    #[test]
    fn nested_scopes_shadow() {
        let got = crate::sqz_blocking::block_on(PROBE.scope((1, 1), async {
            let outer = PROBE.with(|v| v.0);
            let inner = PROBE.scope((2, 2), async { PROBE.with(|v| v.0) }).await;
            let outer_again = PROBE.with(|v| v.0);
            (outer, inner, outer_again)
        }));
        assert_eq!(got, (1, 2, 1));
    }
}
