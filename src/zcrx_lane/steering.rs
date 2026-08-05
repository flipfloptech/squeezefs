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

/// `RX_CLS_LOC_ANY` (<linux/ethtool.h>): the special rule location that
/// asks the DRIVER to pick a slot — the arm strategy for drivers whose
/// advertised table size is a lie (the 2026-08 field finding: mlx5
/// advertises 0 via `ETHTOOL_GRXCLSRLCNT` with ntuple ON, yet accepts
/// inserts — auto-loc returned rule ID 1023). The kernel writes the
/// assigned location back into `fs.location`.
pub const RX_CLS_LOC_ANY: u32 = 0xffff_ffff;

/// `RX_CLS_LOC_SPECIAL` (<linux/ethtool.h>): the flag bit a driver sets
/// in `GRXCLSRLCNT.data` to advertise special-location support, and the
/// bit that must be MASKED out of that word when reading it as a table
/// size (ethtool rxclass.c parity).
pub const RX_CLS_LOC_SPECIAL: u32 = 0x8000_0000;

/// What the capacity gate learned about a NIC's flow-rule slots — the
/// per-driver-class loc strategy (2026-08 field finding 1).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RuleSlots {
    /// The driver advertises a rule table: the reserved-range law
    /// applies (explicit locs in `[lo, hi)`, range-identified
    /// crash-residue reap).
    Reserved { lo: u32, hi: u32 },
    /// The driver advertises a 0-size table with ntuple ON (the mlx5
    /// class — the advertisement is a lie; inserts work empirically):
    /// arm via kernel-assigned locs (`RX_CLS_LOC_ANY`), tear down by
    /// the RETURNED rule IDs, and skip the range reap (no range
    /// identity exists — residue from a crashed daemon on this class
    /// is inert dead-4-tuple rules, operator-reapable via `ethtool
    /// -U <if> delete`). Capacity is the KERNEL's verdict at insert.
    KernelAssigned,
}

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

/// Free rule slots on `ifname` given the gate's verdict — the arm
/// ladder's queue-want clamp (flows ≡ queues 1:1). `None` = no static
/// bound exists (the kernel-assigned class: capacity is the kernel's
/// verdict at insert, so the clamp must NOT zero the queue want).
pub fn free_reserved_slots(ifname: &str, slots: &RuleSlots) -> Option<u32> {
    match slots {
        RuleSlots::Reserved { lo, hi } => {
            let total = hi.saturating_sub(*lo);
            let live = live_arms()
                .get(ifname)
                .map(|l| l.locs.len() as u32)
                .unwrap_or(0);
            Some(total.saturating_sub(live))
        }
        RuleSlots::KernelAssigned => None,
    }
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
    /// ntuple rule table size (loc namespace bound) as advertised by
    /// `ETHTOOL_GRXCLSRLCNT`'s data word (the `RX_CLS_LOC_SPECIAL` flag
    /// bit masked out).
    fn ntuple_table_size(&mut self) -> Result<u32, String>;
    /// Driver support for SPECIAL rule locations (`RX_CLS_LOC_ANY` …):
    /// `ETHTOOL_GRXCLSRLCNT`'s data flag `RX_CLS_LOC_SPECIAL` — the
    /// ethtool-parity probe (rxclass.c `rxclass_rule_ins`). mlx5 does
    /// NOT set it and refuses `@ANY` inserts with ENOSPC (field
    /// finding A).
    fn special_loc_supported(&mut self) -> Result<bool, String>;
    /// The rule-table size as reported by `ETHTOOL_GRXCLSRLALL`'s data
    /// word — the ethtool-parity size source when `GRXCLSRLCNT`
    /// advertises 0 (mlx5 reports MAX_NUM_OF_ETHTOOL_RULES = 1024 here;
    /// ethtool's rxclass_find_empty_slot derives its top-down scan from
    /// exactly this field).
    fn ntuple_table_size_hint(&mut self) -> Result<u32, String>;
    /// Locations of ALL installed ntuple rules.
    fn ntuple_locs(&mut self) -> Result<Vec<u32>, String>;
    /// Install a TCP 4-tuple rule at `loc` — or at a DRIVER-assigned
    /// location when `loc == RX_CLS_LOC_ANY` (the mlx5-class arm).
    /// Returns the EFFECTIVE location (the kernel writes it back), which
    /// the caller must record for teardown.
    fn insert_ntuple(&mut self, loc: u32, rule: &FlowRule) -> Result<u32, String>;
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

/// Pre-arm steering-capacity gate (field row 3, 2026-08-04 + the 2026-08
/// field finding 1): probe the ntuple FEATURE state and the rule-slot
/// capacity BEFORE any flow rule (and, in the arm ladder, before any
/// bring-up work). Ntuple OFF refuses with the exact operator remedy
/// named on the line; a 0-size ADVERTISEMENT with the feature ON is the
/// mlx5-class driver lie (empirical: inserts succeed — auto-loc returned
/// rule ID 1023 into a "0-sized" table) and arms via
/// [`RuleSlots::KernelAssigned`] — the kernel's insert verdict rules,
/// never the advertisement. The probe is READ-ONLY — the lane never
/// flips NIC features itself.
pub fn steering_capacity_gate(nic: &mut dyn NicControl) -> Result<RuleSlots, String> {
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
        // The mlx5 class: feature ON, advertisement 0. Empirically the
        // insert path works (field-verified: explicit `loc 8` insert OK,
        // auto-loc returned rule ID 1023 into the "0-sized" table), so
        // refusing here was the 2026-08 field bug — the KERNEL's insert
        // verdict rules; a real EOPNOTSUPP/ENOSPC-class refusal surfaces
        // loud at `arm_flow_rules` and unwinds the arm.
        if nic_note_once("mlx5-lie", &ifname) {
            log::info!(
                "zcrx steering: {ifname} advertises a 0-slot ntuple rule table with \
                 the feature ON (the mlx5-class advertisement lie) — arming via \
                 driver-assigned rule locations; the kernel's insert verdict rules"
            );
        }
        return Ok(RuleSlots::KernelAssigned);
    }
    Ok(RuleSlots::Reserved { lo, hi })
}

/// Phase A of the split arm (field finding 3): all probes (HDS,
/// channels, the capacity gate) + RSS exclusion of the LEASED queues —
/// run BEFORE any ifq registration, because a queue with a bound zcrx
/// ifq produces unreadable (net_iov) skbs and every host flow
/// RSS-hashed onto it gets `recv = EFAULT` (the field's `ICResp read:
/// Bad address` face). Returns the restore guard (no rules installed
/// yet — those are phase B's, post-connect).
pub fn arm_rss_exclusion(
    nic: &mut dyn NicControl,
    lane_queues: &[u32],
) -> Result<SteeringGuard, String> {
    if lane_queues.is_empty() {
        return Err(
            "zcrx steering: no lane queues to exclude (refusing a silent no-op arm)".into(),
        );
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
    let slots = steering_capacity_gate(nic)?;
    let mut lanes = lane_queues.to_vec();
    lanes.sort_unstable();
    lanes.dedup();

    // The mutation phase runs under the live-arm registry lock:
    // concurrent arms on one NIC serialize, and live-session accounting
    // can never skew mid-arm.
    let mut reg = live_arms();
    let ifname = nic.ifname().to_string();

    // Reserved class: room for one rule per queue must exist UP FRONT
    // (flows ≡ queues 1:1 — phase B would otherwise fail after the ifqs
    // are bound). The kernel-assigned class has no static bound: the
    // kernel's insert verdict rules at phase B.
    if let RuleSlots::Reserved { lo, hi } = slots {
        let live = reg.get(&ifname).map(|l| l.locs.len() as u32).unwrap_or(0);
        let free = (hi - lo).saturating_sub(live);
        if lanes.len() as u32 > free {
            return Err(format!(
                "zcrx steering: {} lane queues exceed the {} free of {} reserved \
                 rule slots on {} ({} held by live lane sessions)",
                lanes.len(),
                free,
                hi - lo,
                nic.ifname(),
                live
            ));
        }
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
            u.extend_from_slice(&lanes);
            (l.pristine_rss.clone(), u)
        }
        None => (current.clone(), lanes.clone()),
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
    entry.queues.extend_from_slice(&lanes);
    Ok(SteeringGuard {
        ifname,
        channels,
        lane_queues: lanes,
        rule_locs: Vec::new(),
        slots,
        rss_touched: true,
        registered: true,
        restored: false,
    })
}

/// Phase B of the split arm: install the lane flow rules (post-connect —
/// the 4-tuples need the ephemeral ports) into the guard phase A
/// returned. Reserved class: crash-residue reap (skipping live locs) +
/// explicit locs at the first free reserved slots. Kernel-assigned
/// class: `RX_CLS_LOC_ANY` inserts, the RETURNED ids recorded for
/// teardown, NO reap (no range identity exists). A refusal rolls back
/// ONLY this call's inserts and leaves the guard armed — at that point
/// the caller's ifqs may still be bound, and re-including the queues in
/// RSS is exactly the EFAULT window; the caller tears the ifqs down and
/// THEN restores.
pub fn arm_flow_rules(
    nic: &mut dyn NicControl,
    guard: &mut SteeringGuard,
    flows: &[FlowRule],
) -> Result<(), String> {
    if flows.is_empty() {
        return Err("zcrx steering: no lane flows to steer (refusing a silent no-op arm)".into());
    }
    if guard.restored {
        return Err(format!(
            "zcrx steering: flow rules requested on a restored guard for {}",
            guard.ifname
        ));
    }
    let mut reg = live_arms();

    // The per-flow REQUEST locs (explicit reserved slots, or the
    // driver-assigned sentinel for the mlx5 class).
    let requests: Vec<u32> = match guard.slots {
        RuleSlots::Reserved { lo, hi } => {
            let live_locs: BTreeSet<u32> = reg
                .get(&guard.ifname)
                .map(|l| l.locs.clone())
                .unwrap_or_default();
            let free = (hi - lo).saturating_sub(live_locs.len() as u32);
            if flows.len() as u32 > free {
                return Err(format!(
                    "zcrx steering: {} flows exceed the {} free of {} reserved rule \
                     slots on {} ({} held by live lane sessions)",
                    flows.len(),
                    free,
                    hi - lo,
                    nic.ifname(),
                    live_locs.len()
                ));
            }
            // Crash-residue reap (design §5): stale rules in the reserved
            // range are lane-owned (dead 4-tuples — inert but must not
            // leak) UNLESS a live session holds the loc; rules outside
            // the range are operator-owned and never touched.
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
            let mut cursor = lo;
            let mut picks = Vec::with_capacity(flows.len());
            for _ in flows {
                let loc = (cursor..hi)
                    .find(|l| !live_locs.contains(l))
                    .ok_or_else(|| {
                        format!(
                            "no free reserved loc on {} (slot accounting bug)",
                            guard.ifname
                        )
                    })?;
                cursor = loc + 1;
                picks.push(loc);
            }
            picks
        }
        RuleSlots::KernelAssigned => Vec::new(), // per-insert ladder below
    };

    // Rollback of THIS call's inserts only (guard stays armed — the
    // finding-3 unwind law: RSS re-inclusion waits for ifq teardown).
    fn roll_back(nic: &mut dyn NicControl, installed: &mut Vec<u32>, step: String) -> String {
        let mut errs = Vec::new();
        for loc in installed.drain(..) {
            if let Err(de) = nic.delete_ntuple(loc) {
                errs.push(format!("rollback delete @{loc}: {de}"));
            }
        }
        if errs.is_empty() {
            format!(
                "{step} (this arm's flow rules rolled back — RSS exclusion held \
                 until the caller's ifq teardown)"
            )
        } else {
            format!("{step}; ROLLBACK ALSO FAILED: {}", errs.join("; "))
        }
    }

    let mut installed: Vec<u32> = Vec::new();
    match guard.slots {
        RuleSlots::Reserved { .. } => {
            for (flow, req) in flows.iter().zip(&requests) {
                match nic.insert_ntuple(*req, flow) {
                    Ok(effective) => installed.push(effective),
                    Err(e) => {
                        let step = format!("ntuple insert @{req} on {}: {e}", nic.ifname());
                        return Err(roll_back(nic, &mut installed, step));
                    }
                }
            }
        }
        RuleSlots::KernelAssigned => {
            // The ethtool-parity insert ladder (field finding A —
            // rxclass.c `rxclass_rule_ins`): (1) probe the
            // RX_CLS_LOC_SPECIAL support flag; (2) supported ⇒ try
            // `@ANY` (the kernel assigns and echoes the id); (3)
            // unsupported OR refused anyway (mlx5 bounces special
            // values as ENOSPC) ⇒ SELF-SELECT the highest free explicit
            // loc below the GRXCLSRLALL-reported table size — the size
            // source ethtool scans from (mlx5 reports 1024 there while
            // GRXCLSRLCNT advertises 0; that is why the ethtool binary
            // lands at 'ID 1023' against this exact driver).
            let use_any = nic
                .special_loc_supported()
                .map_err(|e| format!("special-loc probe on {}: {e}", nic.ifname()))?;
            let mut taken: BTreeSet<u32> = nic
                .ntuple_locs()
                .map_err(|e| format!("rule enumeration on {}: {e}", nic.ifname()))?
                .into_iter()
                .collect();
            if let Some(l) = reg.get(&guard.ifname) {
                taken.extend(l.locs.iter().copied());
            }
            let mut size_hint: Option<u32> = None;
            for flow in flows {
                let mut effective: Option<u32> = None;
                let mut any_refusal: Option<String> = None;
                if use_any {
                    match nic.insert_ntuple(RX_CLS_LOC_ANY, flow) {
                        Ok(e) => effective = Some(e),
                        Err(e) => any_refusal = Some(e), // fall through
                    }
                }
                let effective = match effective {
                    Some(e) => e,
                    None => {
                        let size = match size_hint {
                            Some(sz) => sz,
                            None => {
                                let sz = nic.ntuple_table_size_hint().map_err(|e| {
                                    format!("rule-table size probe on {}: {e}", nic.ifname())
                                })?;
                                size_hint = Some(sz);
                                sz
                            }
                        };
                        if size == 0 {
                            let step = format!(
                                "no rule-table size reported by {} (GRXCLSRLALL data 0) — \
                                 cannot self-select an explicit loc{}",
                                nic.ifname(),
                                any_refusal
                                    .map(|e| format!("; @ANY also refused: {e}"))
                                    .unwrap_or_default()
                            );
                            return Err(roll_back(nic, &mut installed, step));
                        }
                        // Top-down first-free scan — ethtool
                        // rxclass_find_empty_slot parity.
                        let Some(loc) = (0..size).rev().find(|l| !taken.contains(l)) else {
                            let step = format!("all {size} rule slots taken on {}", nic.ifname());
                            return Err(roll_back(nic, &mut installed, step));
                        };
                        match nic.insert_ntuple(loc, flow) {
                            Ok(e) => e,
                            Err(e) => {
                                let step = format!(
                                    "ntuple insert @{loc} on {}: {e}{}",
                                    nic.ifname(),
                                    any_refusal
                                        .map(|a| format!(" (@ANY refused first: {a})"))
                                        .unwrap_or_default()
                                );
                                return Err(roll_back(nic, &mut installed, step));
                            }
                        }
                    }
                };
                taken.insert(effective);
                installed.push(effective);
            }
        }
    }
    for effective in installed {
        guard.rule_locs.push(effective);
        if let Some(l) = reg.get_mut(&guard.ifname) {
            l.locs.insert(effective);
        }
    }
    Ok(())
}

/// One latch per (tag, NIC): every per-NIC notice — refusal warns, the
/// mlx5-lie INFO line (field round 4: it printed ~40× in 30 s), future
/// classes — prints once per process. Keyed by BOTH so the classes can
/// never consume each other's latch. Returns `true` exactly once per
/// (tag, ifname).
pub fn nic_note_once(tag: &str, ifname: &str) -> bool {
    static NOTED: LazyLock<Mutex<std::collections::HashSet<String>>> =
        LazyLock::new(|| Mutex::new(std::collections::HashSet::new()));
    NOTED
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .insert(format!("{tag}:{ifname}"))
}

/// Loud-ONCE-per-NIC refusal throttle for the arm ladder (ten fabric
/// devices ride one NIC — the capacity refusal + remedy must print once,
/// not 10×; per-DEVICE refusal caching stays the caller's OnceCell).
/// Returns `true` exactly once per interface name per process.
pub fn note_arm_refusal_once(ifname: &str) -> bool {
    nic_note_once("arm-refusal", ifname)
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
    /// The gate's slot-class verdict (phase B keys its loc strategy —
    /// explicit reserved slots vs driver-assigned — on it).
    slots: RuleSlots,
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

/// NIC-sharing-aware per-session queue want (field finding C): the
/// eligible pool is a shared resource across every fabric device whose
/// target routes through this NIC, and cold sequential fills spread
/// across ALL namespaces — breadth beats depth.
/// `clamp(eligible / devices, 1, geometry_want)` — the §8-eligible pool
/// divided across the fabric devices routing through the NIC (field
/// shape: `clamp(8/10, 1, 4) = 1` → 8 of 10 devices get a lane instead
/// of 2×4). Floor 1 is the physical minimum (a lane with zero queues
/// cannot exist — the ARBITER, not this derivation, refuses when the
/// pool is truly exhausted); the ceiling is the derived geometry want
/// (never grant more than the session would drive).
pub fn fair_queue_want(eligible: u32, devices: usize, geometry_want: u16) -> u16 {
    let devices = devices.max(1) as u32;
    let share = (eligible / devices).max(1);
    share.min(u32::from(geometry_want.max(1))) as u16
}

/// An armed NIC + its restore guard as ONE owner (field finding B): the
/// arm future is CANCELLABLE (the OnceCell init runs inside a read op's
/// task and dies with it), so the hold must converge the NIC by itself
/// on ANY drop path. Normal paths call [`Self::restore_now`] explicitly
/// AFTER the ifq teardown (the finding-3 ordering law); Drop is the
/// last-resort convergence.
pub struct ArmedSteering<N: NicControl> {
    nic: N,
    guard: SteeringGuard,
}

impl<N: NicControl> ArmedSteering<N> {
    /// Phase A under single ownership: the guard is born INSIDE the
    /// hold — no drop window in which a bare guard can leak.
    pub fn arm(mut nic: N, lane_queues: &[u32]) -> Result<Self, String> {
        let guard = arm_rss_exclusion(&mut nic, lane_queues)?;
        Ok(ArmedSteering { nic, guard })
    }

    /// Phase B against the held NIC (see [`arm_flow_rules`]).
    pub fn flow_rules(&mut self, flows: &[FlowRule]) -> Result<(), String> {
        arm_flow_rules(&mut self.nic, &mut self.guard, flows)
    }

    /// Explicit ordered restore (idempotent; errors logged loud).
    pub fn restore_now(&mut self) {
        if let Err(e) = self.guard.restore(&mut self.nic) {
            log::error!("zcrx-lane: {e}");
        }
    }
}

impl<N: NicControl> Drop for ArmedSteering<N> {
    fn drop(&mut self) {
        // FINDING B (2026-08 field): the arm future is cancellable, and
        // a dropped-unrestored hold left the live NIC at 75 % RSS width
        // for a whole run — worse, the next arm recorded the crippled
        // table as its pristine. The ORDERED paths (join the ifqs, THEN
        // restore) call `restore_now` explicitly first, making this a
        // no-op; any other drop path converges the NIC right here
        // (idempotent; sync ioctls). The transient net_iov window this
        // accepts on ABNORMAL paths is bounded by the dying ring's fd
        // close — permanent RSS damage is the field-proven worse
        // outcome. This is what makes the SteeringGuard drop tripwire
        // structurally unreachable in product code: every product guard
        // is born inside an ArmedSteering (`arm`), and the hold restores
        // on every exit.
        self.restore_now();
    }
}

/// The ONE-SHOT arm: the composition of the split halves (phase A RSS
/// exclusion + phase B flow rules) with TOTAL rollback on a phase-B
/// refusal — the contract surface the steering suite pins the composed
/// laws against, and the arm for callers whose flows are already
/// connected. The PRODUCT bring-up uses the split halves directly
/// (`arm_rss_exclusion` BEFORE ifq registration, `arm_flow_rules` after
/// connect — the field-finding-3 EFAULT ordering law), with the ifq
/// teardown between a failure and the guard restore.
pub fn arm_steering(nic: &mut dyn NicControl, flows: &[FlowRule]) -> Result<SteeringGuard, String> {
    if flows.is_empty() {
        return Err("zcrx steering: no lane flows to steer (refusing a silent no-op arm)".into());
    }
    let lane_queues: Vec<u32> = flows.iter().map(|f| f.queue).collect();
    let mut guard = arm_rss_exclusion(nic, &lane_queues)?;
    if let Err(e) = arm_flow_rules(nic, &mut guard, flows) {
        if let Err(rb) = guard.restore(nic) {
            return Err(format!("{e}; ROLLBACK ALSO FAILED: {rb}"));
        }
        return Err(format!("{e} (rolled back — NIC untouched)"));
    }
    Ok(guard)
}
