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
//!
//! Why steering is arm-fatal (design §5): a mis-steered flow degrades
//! zcrx to the kernel's copy fallback SILENTLY — banned by construction,
//! so any steering refusal refuses the whole arm loud.

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
    /// ntuple feature state (`ethtool -k … ntuple`): the flow-steering
    /// capability gate — a NIC with the feature off has no rule table to
    /// steer into (field row 3, 2026-08-04: the arm read a 0-slot table
    /// and refused per-session with no remedy named).
    fn ntuple_enabled(&mut self) -> Result<bool, String>;
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
pub fn lane_queue_picks(channels: u32, want: u16) -> Vec<u32> {
    let cap = channels / 4;
    let take = (want as u32).min(cap);
    (channels - take..channels).collect()
}

/// Pre-arm steering-capacity gate (field row 3, 2026-08-04): probe the
/// ntuple FEATURE state and the rule-slot capacity BEFORE any flow rule
/// (and, in the arm ladder, before any bring-up work). A zero-capacity
/// NIC refuses with the exact operator remedy named on the line; the
/// probe is READ-ONLY — the lane never flips NIC features itself.
/// `Ok` carries the reserved loc range `[lo, hi)` (≥ 1 slot).
pub fn steering_capacity_gate(nic: &mut dyn NicControl) -> Result<(u32, u32), String> {
    // RED PHASE: contract pinned by tests/zcrx_lane_tests.rs.
    let _ = nic;
    Ok((0, STEERING_RESERVED_SLOTS))
}

/// Loud-ONCE-per-NIC refusal throttle for the arm ladder (ten fabric
/// devices ride one NIC — the capacity refusal + remedy must print once,
/// not 10×; per-DEVICE refusal caching stays the caller's OnceCell).
/// Returns `true` exactly once per interface name per process.
pub fn note_arm_refusal_once(ifname: &str) -> bool {
    // RED PHASE: contract pinned by tests/zcrx_lane_tests.rs.
    let _ = ifname;
    true
}

/// The reserved ntuple loc range `[lo, hi)`: the top
/// [`STEERING_RESERVED_SLOTS`] of the rule table (clamped for tiny
/// tables — at least one slot as long as the table is non-empty).
pub fn reserved_loc_range(table_size: u32) -> (u32, u32) {
    let width = STEERING_RESERVED_SLOTS.min(table_size.max(1)).max(1);
    (table_size.saturating_sub(width), table_size)
}

/// Prior RSS table with the lane queues excluded: non-lane entries are
/// preserved verbatim; entries that pointed at a lane queue are remapped
/// round-robin over the remaining queues.
pub fn restricted_rss(prior: &[u32], lane_queues: &[u32], channels: u32) -> Vec<u32> {
    let keep: Vec<u32> = (0..channels).filter(|q| !lane_queues.contains(q)).collect();
    if keep.is_empty() {
        return prior.to_vec();
    }
    let mut rr = 0usize;
    prior
        .iter()
        .map(|&e| {
            if lane_queues.contains(&e) {
                let v = keep[rr % keep.len()];
                rr += 1;
                v
            } else {
                e
            }
        })
        .collect()
}

/// Recorded prior NIC state + applied steps — the restore/rollback ledger.
#[derive(Debug)]
pub struct SteeringGuard {
    prior_rss: Option<Vec<u32>>,
    rule_locs: Vec<u32>,
    restored: bool,
}

impl SteeringGuard {
    /// Restore the exact recorded prior state (disarm / unmount / poison
    /// / arm rollback). Idempotent; collects every failure loud (a
    /// half-restored NIC must still attempt the remaining steps).
    pub fn restore(&mut self, nic: &mut dyn NicControl) -> Result<(), String> {
        if self.restored {
            return Ok(());
        }
        self.restored = true;
        let mut errs = Vec::new();
        for loc in self.rule_locs.drain(..) {
            if let Err(e) = nic.delete_ntuple(loc) {
                errs.push(format!("delete rule @{loc}: {e}"));
            }
        }
        if let Some(prior) = self.prior_rss.take() {
            if let Err(e) = nic.set_rxfh_indir(&prior) {
                errs.push(format!("restore RSS: {e}"));
            }
        }
        if errs.is_empty() {
            Ok(())
        } else {
            Err(format!(
                "zcrx steering restore incomplete on {}: {}",
                nic.ifname(),
                errs.join("; ")
            ))
        }
    }

    /// Whether restore already ran (Drop-audit hook).
    pub fn restored(&self) -> bool {
        self.restored
    }
}

impl Drop for SteeringGuard {
    fn drop(&mut self) {
        if !self.restored && (self.prior_rss.is_some() || !self.rule_locs.is_empty()) {
            // A dropped-unrestored guard means NIC state leaked past the
            // session — loud; the reserved-loc reap makes the residue
            // recoverable at the next arm (crash-residue law).
            log::error!(
                "zcrx-lane: steering guard dropped without restore — NIC state \
                 leaked (next arm reaps reserved-range rules; RSS needs operator \
                 attention or a re-arm)"
            );
        }
    }
}

/// The arm state machine (design §5): HDS gate → channels → reap stale
/// reserved-range rules → record + restrict RSS → install per-flow rules.
/// Any refusal rolls back every applied step and returns the failing step
/// loud; success returns the restore ledger.
pub fn arm_steering(nic: &mut dyn NicControl, flows: &[FlowRule]) -> Result<SteeringGuard, String> {
    if flows.is_empty() {
        return Err("zcrx steering: no lane flows to steer (refusing a silent no-op arm)".into());
    }
    // Probes first — every gate refuses BEFORE any mutation.
    let hds = nic
        .tcp_data_split_on()
        .map_err(|e| format!("HDS probe on {}: {e}", nic.ifname()))?;
    if !hds {
        return Err(format!(
            "NIC {} has tcp-data-split OFF — zcrx requires HDS; arm refused \
             (NIC untouched)",
            nic.ifname()
        ));
    }
    let channels = nic
        .combined_channels()
        .map_err(|e| format!("channel probe on {}: {e}", nic.ifname()))?;
    let table_size = nic
        .ntuple_table_size()
        .map_err(|e| format!("ntuple table probe on {}: {e}", nic.ifname()))?;
    let (lo, hi) = reserved_loc_range(table_size);
    if flows.len() as u32 > hi - lo {
        return Err(format!(
            "zcrx steering: {} flows exceed the {} reserved rule slots on {}",
            flows.len(),
            hi - lo,
            nic.ifname()
        ));
    }
    let lane_queues: Vec<u32> = {
        let mut qs: Vec<u32> = flows.iter().map(|f| f.queue).collect();
        qs.sort_unstable();
        qs.dedup();
        qs
    };

    // Crash-residue reap (design §5): stale rules in the reserved range
    // are ALWAYS lane-owned (dead 4-tuples — inert but must not leak);
    // rules outside the range are operator-owned and never touched.
    let existing = nic
        .ntuple_locs()
        .map_err(|e| format!("rule enumeration on {}: {e}", nic.ifname()))?;
    for loc in existing.iter().filter(|l| (lo..hi).contains(l)) {
        nic.delete_ntuple(*loc)
            .map_err(|e| format!("stale lane rule reap @{loc} on {}: {e}", nic.ifname()))?;
    }

    let mut guard = SteeringGuard {
        prior_rss: None,
        rule_locs: Vec::new(),
        restored: false,
    };

    // Record → restrict RSS.
    let prior = match nic.rxfh_indir() {
        Ok(p) => p,
        Err(e) => return Err(format!("RSS read on {}: {e}", nic.ifname())),
    };
    let restricted = restricted_rss(&prior, &lane_queues, channels);
    if let Err(e) = nic.set_rxfh_indir(&restricted) {
        return Err(format!("RSS restrict on {}: {e}", nic.ifname()));
    }
    guard.prior_rss = Some(prior);

    // Install per-flow rules in the reserved range; any refusal rolls
    // back EVERYTHING applied so far (rules then RSS).
    for (i, flow) in flows.iter().enumerate() {
        let loc = lo + i as u32;
        if let Err(e) = nic.insert_ntuple(loc, flow) {
            let step = format!("ntuple insert @{loc} on {}: {e}", nic.ifname());
            if let Err(rb) = guard.restore(nic) {
                return Err(format!("{step}; ROLLBACK ALSO FAILED: {rb}"));
            }
            return Err(format!("{step} (rolled back — NIC untouched)"));
        }
        guard.rule_locs.push(loc);
    }
    Ok(guard)
}
