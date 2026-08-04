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
//!   rules outside the range are NEVER touched (operator-owned);
//! * LIVE sessions coexist (the rxq arbiter made a second session per
//!   NIC reachable): reap and loc allocation exclude live sessions'
//!   locs, RSS writes exclude the UNION of live lane queues, and
//!   restores converge to the first armer's pristine table in any order.
//!
//! Why steering is arm-fatal (design §5): a mis-steered flow degrades
//! zcrx to the kernel's copy fallback SILENTLY — banned by construction,
//! so any steering refusal refuses the whole arm loud.

use std::collections::{BTreeSet, HashMap};
use std::net::SocketAddr;
use std::sync::{LazyLock, Mutex, MutexGuard};

/// Reserved ntuple location slots (the TOP of the driver's rule table) —
/// the crash-residue law's identity: a rule in this range is ALWAYS
/// lane-owned, so a stale one is reapable and a foreign one is impossible.
pub const STEERING_RESERVED_SLOTS: u32 = 64;

/// Per-NIC live-arm state — what makes a SECOND lane session on one NIC
/// safe now that the rxq arbiter (`rxq_alloc`) hands out distinct queues
/// (the pre-arbiter reap law assumed one session per NIC and would have
/// deleted a live peer's rules — the design-§5 banned silent-degrade):
/// * `locs`: reserved-range rule locations held by LIVE sessions — the
///   crash-residue reap and fresh-loc allocation exclude them (a
///   reserved-range rule is residue ONLY if no live session owns it);
/// * `queues`: live sessions' lane queues (with multiplicity) — every
///   RSS write excludes the UNION, and a restore re-excludes the
///   surviving peers' queues;
/// * `pristine_rss`: the table the FIRST armer read — what every restore
///   derives from, so restores converge to it in ANY order.
struct NicLive {
    pristine_rss: Vec<u32>,
    locs: BTreeSet<u32>,
    queues: Vec<u32>,
    sessions: u32,
}

/// Process-wide live-arm registry keyed by interface name (unique per
/// host). ONE coarse lock over the rare control-plane arms/restores;
/// each mutation phase (reap → RSS → rules) runs entirely under it, so
/// two devices arming one NIC concurrently serialize.
static LIVE_ARMS: LazyLock<Mutex<HashMap<String, NicLive>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

fn live_arms() -> MutexGuard<'static, HashMap<String, NicLive>> {
    // A panicked holder leaves plain collections in a valid state —
    // recover the inner map rather than poisoning every future arm.
    LIVE_ARMS.lock().unwrap_or_else(|e| e.into_inner())
}

/// Free reserved rule slots on `ifname` given the gate's reserved range
/// (total minus live sessions' holdings) — the arm ladder's queue-want
/// clamp (flows ≡ queues 1:1).
pub fn free_reserved_slots(ifname: &str, reserved: (u32, u32)) -> u32 {
    let total = reserved.1.saturating_sub(reserved.0);
    let live = live_arms()
        .get(ifname)
        .map(|l| l.locs.len() as u32)
        .unwrap_or(0);
    total.saturating_sub(live)
}

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

/// The lane-eligible RX-queue pool for a NIC: the HIGHEST-indexed
/// `channels / 4` queues — ONE definition of the §8 ceiling (RSS keeps
/// ≥ ¾ of the NIC's queues; the divisor is design §8's stated bound,
/// not tuning). The rxq arbiter (`rxq_alloc`) arbitrates session grants
/// WITHIN this range; empty = NIC too narrow to dedicate ZC queues.
pub fn lane_eligible_queues(channels: u32) -> std::ops::Range<u32> {
    channels - channels / 4..channels
}

/// Lane ZC queue picks: the HIGHEST-indexed `want` queues of the
/// eligible pool (design §8) — the pure, UNCONTENDED single-session
/// derivation (the arbiter's preferred grant). Empty = NIC too narrow
/// (the caller refuses the arm loud).
pub fn lane_queue_picks(channels: u32, want: u16) -> Vec<u32> {
    let pool = lane_eligible_queues(channels);
    let take = u32::from(want).min(pool.end - pool.start);
    (pool.end - take..pool.end).collect()
}

/// Pre-arm steering-capacity gate (field row 3, 2026-08-04: the arm read
/// `1 flows exceed the 0 reserved rule slots` AFTER connecting — ntuple
/// was off via `ethtool -K`, so the driver advertised a 0-slot rule
/// table and the refusal surfaced per-device with no remedy named):
/// probe the ntuple FEATURE state and the rule-slot capacity BEFORE any
/// flow rule (and, in the arm ladder, before any bring-up work). A
/// zero-capacity NIC refuses with the exact operator remedy named on
/// the line; the probe is READ-ONLY — the lane never flips NIC features
/// itself. `Ok` carries the reserved loc range `[lo, hi)` (≥ 1 slot).
pub fn steering_capacity_gate(nic: &mut dyn NicControl) -> Result<(u32, u32), String> {
    let ifname = nic.ifname().to_string();
    let on = nic
        .ntuple_enabled()
        .map_err(|e| format!("ntuple feature probe on {ifname}: {e}"))?;
    if !on {
        return Err(format!(
            "zcrx steering: ntuple flow steering is disabled on {ifname} — the lane \
             cannot steer its flows to dedicated ZC queues; remedy: `ethtool -K \
             {ifname} ntuple on`, then remount (the lane never flips NIC features \
             itself — NIC left untouched, kernel path serves)"
        ));
    }
    let table = nic
        .ntuple_table_size()
        .map_err(|e| format!("ntuple table probe on {ifname}: {e}"))?;
    let (lo, hi) = reserved_loc_range(table);
    if hi - lo == 0 {
        return Err(format!(
            "zcrx steering: {ifname} advertises a {table}-slot ntuple rule table — \
             0 reserved lane rule slots; remedy: `ethtool -K {ifname} ntuple on` \
             (drivers size the rule table when the feature arms), then remount; if \
             ntuple is already on, this driver/firmware reserves no rule slots and \
             the NIC cannot host the lane (kernel path serves)"
        ));
    }
    Ok((lo, hi))
}

/// Loud-ONCE-per-NIC refusal throttle for the arm ladder (ten fabric
/// devices ride one NIC — the capacity refusal + remedy must print once,
/// not 10×; per-DEVICE refusal caching stays the caller's OnceCell).
/// Returns `true` exactly once per interface name per process.
pub fn note_arm_refusal_once(ifname: &str) -> bool {
    static WARNED: std::sync::LazyLock<std::sync::Mutex<std::collections::HashSet<String>>> =
        std::sync::LazyLock::new(|| std::sync::Mutex::new(std::collections::HashSet::new()));
    WARNED
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .insert(ifname.to_string())
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

/// Recorded arm state + applied steps — the restore/rollback ledger.
#[derive(Debug)]
pub struct SteeringGuard {
    ifname: String,
    channels: u32,
    lane_queues: Vec<u32>,
    rule_locs: Vec<u32>,
    /// RSS was written by this arm; restore recomputes the table from
    /// the registry's pristine record + the surviving peers' queues.
    rss_touched: bool,
    /// Session registered in the live-arm registry (dereg on restore or
    /// Drop, so residue becomes reapable and peers stop excluding us).
    registered: bool,
    restored: bool,
}

impl SteeringGuard {
    /// Restore the recorded prior state (disarm / unmount / poison / arm
    /// rollback). Idempotent; collects every failure loud (a
    /// half-restored NIC must still attempt the remaining steps). With
    /// live peer sessions on the NIC, the RSS write keeps THEIR queues
    /// excluded; the LAST restore returns the pristine table — so
    /// restores converge in ANY order.
    pub fn restore(&mut self, nic: &mut dyn NicControl) -> Result<(), String> {
        let mut reg = live_arms();
        self.restore_locked(nic, &mut reg)
    }

    fn restore_locked(
        &mut self,
        nic: &mut dyn NicControl,
        reg: &mut HashMap<String, NicLive>,
    ) -> Result<(), String> {
        if self.restored {
            return Ok(());
        }
        self.restored = true;
        let mut errs = Vec::new();
        for loc in self.rule_locs.drain(..) {
            if let Err(e) = nic.delete_ntuple(loc) {
                errs.push(format!("delete rule @{loc}: {e}"));
            }
            if let Some(l) = reg.get_mut(&self.ifname) {
                l.locs.remove(&loc);
            }
        }
        let mut pristine = Vec::new();
        let mut peers = Vec::new();
        if self.registered {
            self.registered = false;
            if let Some(l) = reg.get_mut(&self.ifname) {
                l.sessions = l.sessions.saturating_sub(1);
                for q in &self.lane_queues {
                    if let Some(pos) = l.queues.iter().position(|x| x == q) {
                        l.queues.swap_remove(pos);
                    }
                }
                pristine = l.pristine_rss.clone();
                peers = l.queues.clone();
                if l.sessions == 0 {
                    reg.remove(&self.ifname);
                }
            }
        }
        if self.rss_touched {
            self.rss_touched = false;
            if pristine.is_empty() {
                errs.push(format!(
                    "restore RSS on {}: no pristine table recorded",
                    self.ifname
                ));
            } else {
                let table = if peers.is_empty() {
                    pristine
                } else {
                    restricted_rss(&pristine, &peers, self.channels)
                };
                if let Err(e) = nic.set_rxfh_indir(&table) {
                    errs.push(format!("restore RSS: {e}"));
                }
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
        if self.restored {
            return;
        }
        if self.registered {
            // Deregister so (a) the residue rules become REAPABLE by the
            // next arm (crash-residue law) and (b) live peers' restores
            // stop excluding our queues from the tables they write.
            let mut reg = live_arms();
            if let Some(l) = reg.get_mut(&self.ifname) {
                for loc in &self.rule_locs {
                    l.locs.remove(loc);
                }
                for q in &self.lane_queues {
                    if let Some(pos) = l.queues.iter().position(|x| x == q) {
                        l.queues.swap_remove(pos);
                    }
                }
                l.sessions = l.sessions.saturating_sub(1);
                if l.sessions == 0 {
                    reg.remove(&self.ifname);
                }
            }
        }
        if self.rss_touched || !self.rule_locs.is_empty() {
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
    // ntuple feature + rule-slot capacity BEFORE any flow rule (field
    // row 3 — the gate names the operator remedy; still probe-only).
    let (lo, hi) = steering_capacity_gate(nic)?;
    let lane_queues: Vec<u32> = {
        let mut qs: Vec<u32> = flows.iter().map(|f| f.queue).collect();
        qs.sort_unstable();
        qs.dedup();
        qs
    };

    // The whole mutation phase runs under the live-arm registry lock:
    // concurrent arms on one NIC serialize, and live-session accounting
    // can never skew mid-arm.
    let mut reg = live_arms();
    let ifname = nic.ifname().to_string();
    let live_locs: BTreeSet<u32> = reg.get(&ifname).map(|l| l.locs.clone()).unwrap_or_default();
    let free_slots = (hi - lo).saturating_sub(live_locs.len() as u32);
    if flows.len() as u32 > free_slots {
        return Err(format!(
            "zcrx steering: {} flows exceed the {} free of {} reserved rule slots \
             on {} ({} held by live lane sessions)",
            flows.len(),
            free_slots,
            hi - lo,
            nic.ifname(),
            live_locs.len()
        ));
    }

    // Crash-residue reap (design §5): stale rules in the reserved range
    // are lane-owned (dead 4-tuples — inert but must not leak) UNLESS a
    // live session holds the loc (the multi-session law: a reserved-range
    // rule is residue only if nobody live owns it); rules outside the
    // range are operator-owned and never touched.
    let existing = nic
        .ntuple_locs()
        .map_err(|e| format!("rule enumeration on {}: {e}", nic.ifname()))?;
    for loc in existing
        .iter()
        .filter(|l| (lo..hi).contains(l) && !live_locs.contains(l))
    {
        nic.delete_ntuple(*loc)
            .map_err(|e| format!("stale lane rule reap @{loc} on {}: {e}", nic.ifname()))?;
    }

    // Record → restrict RSS: pristine is the FIRST armer's read; every
    // arm writes pristine restricted to the UNION of live lane queues
    // (peers' + this arm's).
    let current = match nic.rxfh_indir() {
        Ok(p) => p,
        Err(e) => return Err(format!("RSS read on {}: {e}", nic.ifname())),
    };
    let (pristine, union) = match reg.get(&ifname) {
        Some(l) => {
            let mut u = l.queues.clone();
            u.extend_from_slice(&lane_queues);
            (l.pristine_rss.clone(), u)
        }
        None => (current.clone(), lane_queues.clone()),
    };
    let restricted = restricted_rss(&pristine, &union, channels);
    if let Err(e) = nic.set_rxfh_indir(&restricted) {
        return Err(format!("RSS restrict on {}: {e}", nic.ifname()));
    }
    let entry = reg.entry(ifname.clone()).or_insert_with(|| NicLive {
        pristine_rss: current,
        locs: BTreeSet::new(),
        queues: Vec::new(),
        sessions: 0,
    });
    entry.sessions += 1;
    entry.queues.extend_from_slice(&lane_queues);
    let mut guard = SteeringGuard {
        ifname: ifname.clone(),
        channels,
        lane_queues,
        rule_locs: Vec::new(),
        rss_touched: true,
        registered: true,
        restored: false,
    };

    // Install per-flow rules at the first FREE reserved locs (skipping
    // live peers'); any refusal rolls back EVERYTHING this arm applied
    // (rules, registration, RSS — peers stay excluded).
    let mut cursor = lo;
    for flow in flows {
        let loc = match (cursor..hi).find(|l| !live_locs.contains(l)) {
            Some(l) => l,
            None => {
                // Structurally unreachable (free_slots admitted the
                // flows) — refuse loud rather than trust the accounting.
                let step = format!("no free reserved loc on {} (slot accounting bug)", ifname);
                if let Err(rb) = guard.restore_locked(nic, &mut reg) {
                    return Err(format!("{step}; ROLLBACK ALSO FAILED: {rb}"));
                }
                return Err(format!("{step} (rolled back — NIC untouched)"));
            }
        };
        cursor = loc + 1;
        if let Err(e) = nic.insert_ntuple(loc, flow) {
            let step = format!("ntuple insert @{loc} on {}: {e}", nic.ifname());
            if let Err(rb) = guard.restore_locked(nic, &mut reg) {
                return Err(format!("{step}; ROLLBACK ALSO FAILED: {rb}"));
            }
            return Err(format!("{step} (rolled back — NIC untouched)"));
        }
        guard.rule_locs.push(loc);
        if let Some(l) = reg.get_mut(&ifname) {
            l.locs.insert(loc);
        }
    }
    Ok(guard)
}
