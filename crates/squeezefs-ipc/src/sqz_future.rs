//! Tiny first-party future combinators (the rip-tokio-total program) —
//! the `tokio::select!` replacement for the shapes the daemon uses.
//! Biased by argument order (poll `a` first, then `b`) — determinism
//! over tokio's random fairness; every converted site chose its order
//! deliberately.

use std::future::Future;
use std::pin::Pin;
use std::task::{Context, Poll};

pub enum Either<A, B> {
    Left(A),
    Right(B),
}

/// Race two futures, biased left. Loser is dropped (cancel-safety is
/// the callee futures' contract, same as `tokio::select!`).
pub fn race2<FA, FB>(a: FA, b: FB) -> Race2<FA, FB>
where
    FA: Future,
    FB: Future,
{
    Race2 {
        a: Box::pin(a),
        b: Box::pin(b),
    }
}

pub struct Race2<FA: Future, FB: Future> {
    a: Pin<Box<FA>>,
    b: Pin<Box<FB>>,
}

impl<FA: Future, FB: Future> Future for Race2<FA, FB> {
    type Output = Either<FA::Output, FB::Output>;
    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        if let Poll::Ready(v) = self.a.as_mut().poll(cx) {
            return Poll::Ready(Either::Left(v));
        }
        if let Poll::Ready(v) = self.b.as_mut().poll(cx) {
            return Poll::Ready(Either::Right(v));
        }
        Poll::Pending
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn race_is_left_biased_and_loser_cancels() {
        let out = crate::sqz_blocking::block_on(race2(
            crate::sqz_time::sleep(Duration::from_millis(10)),
            crate::sqz_time::sleep(Duration::from_secs(3600)),
        ));
        assert!(matches!(out, Either::Left(())), "short sleep wins");
        // The hour-long loser was dropped (tombstoned) — the timer
        // thread stays healthy for the next sleep.
        let t0 = std::time::Instant::now();
        crate::sqz_blocking::block_on(crate::sqz_time::sleep(Duration::from_millis(20)));
        assert!(t0.elapsed() < Duration::from_secs(5));
    }
}
