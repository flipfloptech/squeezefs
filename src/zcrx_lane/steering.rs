//! NIC flow-steering for the zcrx read lane (design §5 "Queue isolation"):
//! the record → apply → verify → restore state machine over the
//! [`NicControl`] seam, plus the reserved-loc crash-residue law.
//!
//! Laws (pinned by `tests/zcrx_steering_tests.rs` against a mock NIC; the
//! live `EthtoolNic` evidence is the reformat-window bracket's):
//! * arm applies exactly {RSS restriction excluding the lane queues, one
//!   ntuple rule per lane flow in the RESERVED loc range} — nothing else;
//! * a refusal at ANY step rolls back every applied step (the NIC is
//!   byte-identical after a failed arm) and the error names the step;
//! * disarm restores the exact recorded prior state;
//! * stale reserved-range rules from a crashed daemon are reaped at arm;
//!   rules outside the range are NEVER touched (operator-owned).

use std::net::SocketAddr;

/// Reserved ntuple location slots (the TOP of the driver's rule table) —
/// the crash-residue law's identity: a rule in this range is ALWAYS
/// lane-owned, so a stale one is reapable and a foreign one is impossible.
pub const STEERING_RESERVED_SLOTS: u32 = 64;

/// One lane connection's 4-tuple → queue steering rule.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FlowRule {
    /// Lane connection source (daemon side, post-connect — the ephemeral
    /// port is why rules install after connect).
    pub src: SocketAddr,
    /// Target side (traddr:trsvcid).
    pub dst: SocketAddr,
    /// NIC RX queue this flow steers to (a lane ZC queue).
    pub queue: u32,
}

/// The NIC control seam: `EthtoolNic` (ioctl + ethtool-genetlink) in the
/// field; a mock in the contract suite. Every method is fallible — the
/// state machine turns any refusal into rollback + loud arm failure.
pub trait NicControl {
    fn ifname(&self) -> &str;
    /// Combined channel (queue) count (`ethtool -l` equivalent).
    fn combined_channels(&mut self) -> Result<u32, String>;
    /// Header/data split state (`ethtool -g … tcp-data-split`) — the zcrx
    /// HDS precondition, probed via the RINGS_GET netlink ATTRIBUTE (the
    /// corrected probe: compare the attr, never a rendered string).
    fn tcp_data_split_on(&mut self) -> Result<bool, String>;
    /// RSS indirection table (`ethtool -x` equivalent).
    fn rxfh_indir(&mut self) -> Result<Vec<u32>, String>;
    /// Replace the RSS indirection table (`ethtool -X weight …`).
    fn set_rxfh_indir(&mut self, indir: &[u32]) -> Result<(), String>;
    /// ntuple rule table size (loc namespace bound).
    fn ntuple_table_size(&mut self) -> Result<u32, String>;
    /// Locations of ALL installed ntuple rules.
    fn ntuple_locs(&mut self) -> Result<Vec<u32>, String>;
    /// Install a TCP 4-tuple rule at an explicit location.
    fn insert_ntuple(&mut self, loc: u32, rule: &FlowRule) -> Result<(), String>;
    fn delete_ntuple(&mut self, loc: u32) -> Result<(), String>;
}

/// Lane ZC queue picks: the HIGHEST-indexed `want` queues, bounded so the
/// RSS set keeps ≥ ¾ of the NIC (design §8). Empty = NIC too narrow (the
/// caller refuses the arm loud).
pub fn lane_queue_picks(_channels: u32, _want: u16) -> Vec<u32> {
    Vec::new() // Z2 phase A stub — contracts red
}

/// The reserved ntuple loc range `[lo, hi)`: the top
/// [`STEERING_RESERVED_SLOTS`] of the rule table (clamped for tiny tables).
pub fn reserved_loc_range(table_size: u32) -> (u32, u32) {
    (0, table_size) // Z2 phase A stub — contracts red
}

/// Prior RSS table with the lane queues excluded: non-lane entries are
/// preserved verbatim; entries that pointed at a lane queue are remapped
/// round-robin over the remaining queues.
pub fn restricted_rss(prior: &[u32], _lane_queues: &[u32], _channels: u32) -> Vec<u32> {
    prior.to_vec() // Z2 phase A stub — contracts red
}

/// Recorded prior NIC state + applied steps — the restore/rollback ledger.
#[derive(Debug)]
pub struct SteeringGuard {
    prior_rss: Option<Vec<u32>>,
    rule_locs: Vec<u32>,
}

impl SteeringGuard {
    /// Restore the exact recorded prior state (disarm / unmount / poison).
    pub fn restore(&mut self, _nic: &mut dyn NicControl) -> Result<(), String> {
        let _ = (&self.prior_rss, &self.rule_locs);
        Err("zcrx steering not implemented (PR Z2 phase A)".into())
    }
}

/// The arm state machine (design §5): HDS gate → channels → reap stale
/// reserved-range rules → record + restrict RSS → install per-flow rules.
/// Any refusal rolls back every applied step and returns the failing step
/// loud; success returns the restore ledger.
pub fn arm_steering(
    _nic: &mut dyn NicControl,
    _flows: &[FlowRule],
) -> Result<SteeringGuard, String> {
    Err("zcrx steering not implemented (PR Z2 phase A)".into())
}
