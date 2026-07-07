//! Backend health probe hysteresis.
//!
//! A probe shares the device's I/O lanes with real traffic, so a slow or
//! starved probe is **not** evidence the device is dead — the dismount-storm
//! incident proved a saturated queue can flip a healthy backend offline and
//! turn live I/O into `AddrNotAvailable` failures. The state machine
//! therefore distinguishes three observations:
//!
//! - `Ok`      — probe completed: heal to healthy immediately, reset streak.
//! - `Failed`  — probe returned a hard error (I/O error, missing device):
//!   counts toward the consecutive-failure streak; only
//!   [`HealthState::FAILURE_THRESHOLD`] *consecutive* hard failures mark the
//!   backend unhealthy.
//! - `Inconclusive` — probe timed out: the device is busy, not proven dead.
//!   Preserves the streak (neither counts nor resets) and never flips state.

/// Per-backend probe hysteresis state.
#[derive(Debug, Default)]
pub struct HealthState {
    consecutive_failures: u32,
    unhealthy: bool,
}

/// One probe outcome.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Probe {
    Ok,
    Failed,
    /// Timed out — device busy is not device dead.
    Inconclusive,
}

/// State transition produced by [`HealthState::observe`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Transition {
    /// No state change.
    None,
    /// Healthy → unhealthy (streak reached threshold).
    WentUnhealthy,
    /// Unhealthy → healthy (successful probe).
    Recovered,
}

impl HealthState {
    /// Consecutive hard failures required to mark a backend unhealthy.
    pub const FAILURE_THRESHOLD: u32 = 3;

    pub fn is_unhealthy(&self) -> bool {
        self.unhealthy
    }

    /// Fold one probe outcome into the state; returns the transition.
    pub fn observe(&mut self, probe: Probe) -> Transition {
        match probe {
            Probe::Ok => {
                self.consecutive_failures = 0;
                if self.unhealthy {
                    self.unhealthy = false;
                    Transition::Recovered
                } else {
                    Transition::None
                }
            }
            Probe::Failed => {
                self.consecutive_failures = self.consecutive_failures.saturating_add(1);
                if !self.unhealthy && self.consecutive_failures >= Self::FAILURE_THRESHOLD {
                    self.unhealthy = true;
                    Transition::WentUnhealthy
                } else {
                    Transition::None
                }
            }
            Probe::Inconclusive => Transition::None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_single_failure_does_not_flip() {
        let mut h = HealthState::default();
        assert_eq!(h.observe(Probe::Failed), Transition::None);
        assert_eq!(h.observe(Probe::Failed), Transition::None);
        assert!(!h.is_unhealthy(), "below threshold must stay healthy");
    }

    #[test]
    fn test_threshold_consecutive_failures_flip() {
        let mut h = HealthState::default();
        h.observe(Probe::Failed);
        h.observe(Probe::Failed);
        assert_eq!(h.observe(Probe::Failed), Transition::WentUnhealthy);
        assert!(h.is_unhealthy());
        // Further failures do not re-announce.
        assert_eq!(h.observe(Probe::Failed), Transition::None);
    }

    #[test]
    fn test_success_resets_streak() {
        let mut h = HealthState::default();
        h.observe(Probe::Failed);
        h.observe(Probe::Failed);
        assert_eq!(h.observe(Probe::Ok), Transition::None);
        // Streak restarted: two more failures are still below threshold.
        h.observe(Probe::Failed);
        assert_eq!(h.observe(Probe::Failed), Transition::None);
        assert!(!h.is_unhealthy());
    }

    #[test]
    fn test_inconclusive_preserves_streak_and_state() {
        let mut h = HealthState::default();
        h.observe(Probe::Failed);
        h.observe(Probe::Failed);
        // Busy device mid-streak: neither counts nor resets.
        assert_eq!(h.observe(Probe::Inconclusive), Transition::None);
        assert!(!h.is_unhealthy());
        assert_eq!(
            h.observe(Probe::Failed),
            Transition::WentUnhealthy,
            "third hard failure after an inconclusive probe completes the streak"
        );
        // Inconclusive while unhealthy stays unhealthy (no phantom recovery).
        assert_eq!(h.observe(Probe::Inconclusive), Transition::None);
        assert!(h.is_unhealthy());
    }

    #[test]
    fn test_recovery_is_immediate_on_success() {
        let mut h = HealthState::default();
        for _ in 0..HealthState::FAILURE_THRESHOLD {
            h.observe(Probe::Failed);
        }
        assert!(h.is_unhealthy());
        assert_eq!(h.observe(Probe::Ok), Transition::Recovered);
        assert!(!h.is_unhealthy());
    }
}
