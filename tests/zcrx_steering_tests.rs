//! zcrx lane NIC steering contracts (docs/design-zcrx-read-lane.md §5, PR Z2):
//! the record → apply → verify → restore state machine over the `NicControl`
//! seam. The REAL `EthtoolNic` (ioctl + ethtool-genetlink) executes only on a
//! zcrx-capable NIC — these contracts pin the state machine's laws against a
//! mock NIC so every arm/rollback/restore/reap path is exercised on every
//! commit; the live-NIC evidence is the reformat-window bracket's.
//!
//! Laws pinned here (design §5 "Queue isolation"):
//! 1. Arm applies exactly {RSS restriction excluding the lane queues, one
//!    ntuple rule per lane flow in the RESERVED loc range} — nothing else.
//! 2. Disarm restores the EXACT prior NIC state (RSS table byte-identical,
//!    reserved-range rules gone, foreign rules untouched).
//! 3. A refusal at ANY step leaves the NIC byte-identical to its prior state
//!    (rollback exactness) and the error names the failing step.
//! 4. Crash residue: stale rules in the reserved loc range are reaped at arm;
//!    rules OUTSIDE the range are never touched.
//! 5. HDS (tcp-data-split) off ⇒ arm refuses loud with ZERO mutations.

use squeezefs::zcrx_lane::steering::{
    arm_steering, lane_queue_picks, reserved_loc_range, restricted_rss, FlowRule, NicControl,
    STEERING_RESERVED_SLOTS,
};
use std::collections::BTreeMap;
use std::net::SocketAddr;

// ------------------------------------------------------------------ mock NIC

/// Which mock op refuses (failure-injection lattice).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum FailAt {
    None,
    Channels,
    Hds,
    RssGet,
    RssSet,
    TableSize,
    Locs,
    Insert(u32), // fail the n-th insert (0-based)
    Delete,
}

struct MockNic {
    channels: u32,
    hds_on: bool,
    rss: Vec<u32>,
    table_size: u32,
    rules: BTreeMap<u32, FlowRule>,
    fail_at: FailAt,
    inserts_seen: u32,
    ops: Vec<String>,
}

impl MockNic {
    fn new(channels: u32, rss_len: usize) -> Self {
        let rss: Vec<u32> = (0..rss_len).map(|i| (i as u32) % channels).collect();
        MockNic {
            channels,
            hds_on: true,
            rss,
            table_size: 1024,
            rules: BTreeMap::new(),
            fail_at: FailAt::None,
            inserts_seen: 0,
            ops: Vec::new(),
        }
    }

    /// Full externally-visible NIC state (the exactness instrument).
    fn snapshot(&self) -> (Vec<u32>, BTreeMap<u32, FlowRule>) {
        (self.rss.clone(), self.rules.clone())
    }
}

impl NicControl for MockNic {
    fn ifname(&self) -> &str {
        "mock0"
    }
    fn combined_channels(&mut self) -> Result<u32, String> {
        self.ops.push("channels".into());
        if self.fail_at == FailAt::Channels {
            return Err("mock: channels refused".into());
        }
        Ok(self.channels)
    }
    fn tcp_data_split_on(&mut self) -> Result<bool, String> {
        self.ops.push("hds".into());
        if self.fail_at == FailAt::Hds {
            return Err("mock: hds probe refused".into());
        }
        Ok(self.hds_on)
    }
    fn rxfh_indir(&mut self) -> Result<Vec<u32>, String> {
        self.ops.push("rss_get".into());
        if self.fail_at == FailAt::RssGet {
            return Err("mock: rss get refused".into());
        }
        Ok(self.rss.clone())
    }
    fn set_rxfh_indir(&mut self, indir: &[u32]) -> Result<(), String> {
        self.ops.push("rss_set".into());
        if self.fail_at == FailAt::RssSet {
            return Err("mock: rss set refused".into());
        }
        self.rss = indir.to_vec();
        Ok(())
    }
    fn ntuple_table_size(&mut self) -> Result<u32, String> {
        self.ops.push("table_size".into());
        if self.fail_at == FailAt::TableSize {
            return Err("mock: table size refused".into());
        }
        Ok(self.table_size)
    }
    fn ntuple_locs(&mut self) -> Result<Vec<u32>, String> {
        self.ops.push("locs".into());
        if self.fail_at == FailAt::Locs {
            return Err("mock: locs refused".into());
        }
        Ok(self.rules.keys().copied().collect())
    }
    fn insert_ntuple(&mut self, loc: u32, rule: &FlowRule) -> Result<(), String> {
        self.ops.push(format!("insert@{loc}"));
        let n = self.inserts_seen;
        self.inserts_seen += 1;
        if self.fail_at == FailAt::Insert(n) {
            return Err(format!("mock: insert {n} refused"));
        }
        assert!(
            !self.rules.contains_key(&loc),
            "steering must never overwrite an occupied loc"
        );
        self.rules.insert(loc, rule.clone());
        Ok(())
    }
    fn delete_ntuple(&mut self, loc: u32) -> Result<(), String> {
        self.ops.push(format!("delete@{loc}"));
        if self.fail_at == FailAt::Delete {
            return Err("mock: delete refused".into());
        }
        assert!(
            self.rules.remove(&loc).is_some(),
            "steering must only delete rules it knows exist"
        );
        Ok(())
    }
}

fn flow(sport: u16, queue: u32) -> FlowRule {
    FlowRule {
        src: SocketAddr::from(([10, 0, 0, 2], sport)),
        dst: SocketAddr::from(([10, 0, 0, 1], 4420)),
        queue,
    }
}

// ------------------------------------------------------- derivation-law tests

#[test]
fn test_lane_queue_picks_are_highest_indexed_and_bounded() {
    // Design §8: lane queues ≤ nic_queues / 4 so RSS keeps ≥ ¾ of the NIC.
    let picks = lane_queue_picks(32, 4);
    assert_eq!(picks, vec![28, 29, 30, 31], "highest-indexed queues");
    let picks = lane_queue_picks(32, 100);
    assert_eq!(picks.len(), 8, "bounded to channels/4");
    assert_eq!(*picks.last().unwrap(), 31);
    // Too-narrow NIC: no picks (the caller refuses the arm loud).
    assert!(lane_queue_picks(3, 1).is_empty(), "channels/4 < 1 ⇒ none");
    assert_eq!(lane_queue_picks(4, 2), vec![3], "exactly one at 4 channels");
}

#[test]
fn test_reserved_loc_range_is_top_slots() {
    let (lo, hi) = reserved_loc_range(1024);
    assert_eq!(hi, 1024);
    assert_eq!(lo, 1024 - STEERING_RESERVED_SLOTS);
    // Tiny tables still yield a non-empty, in-bounds range.
    let (lo, hi) = reserved_loc_range(16);
    assert!(lo < hi && hi <= 16);
}

#[test]
fn test_restricted_rss_excludes_lane_queues_and_preserves_rest() {
    let prior: Vec<u32> = (0..128).map(|i| i % 8).collect();
    let out = restricted_rss(&prior, &[6, 7], 8);
    assert_eq!(out.len(), prior.len(), "table length preserved");
    for (i, v) in out.iter().enumerate() {
        assert!(*v < 8, "entries stay in channel range");
        assert!(![6u32, 7].contains(v), "lane queues excluded @ {i}");
        if ![6u32, 7].contains(&prior[i]) {
            assert_eq!(*v, prior[i], "non-lane entries preserved @ {i}");
        }
    }
}

// --------------------------------------------------------- state-machine laws

#[test]
fn test_arm_applies_rss_restriction_and_reserved_rules() {
    let mut nic = MockNic::new(16, 128);
    let flows = vec![flow(50001, 14), flow(50002, 15)];
    let guard = arm_steering(&mut nic, &flows).expect("arm");

    // Law 1: RSS excludes the lane queues, rules sit in the reserved range.
    for v in &nic.rss {
        assert!(![14u32, 15].contains(v), "RSS must exclude lane queues");
    }
    assert_eq!(nic.rules.len(), 2, "one rule per lane flow");
    let (lo, hi) = reserved_loc_range(nic.table_size);
    for (loc, rule) in &nic.rules {
        assert!((lo..hi).contains(loc), "rule loc {loc} in reserved range");
        assert!([14u32, 15].contains(&rule.queue));
    }

    // Law 2: restore is byte-exact.
    let mut nic2 = MockNic::new(16, 128);
    let prior = nic2.snapshot();
    let flows2 = vec![flow(50001, 14), flow(50002, 15)];
    let mut g2 = arm_steering(&mut nic2, &flows2).expect("arm 2");
    assert_ne!(nic2.snapshot().0, prior.0, "arm visibly restricted RSS");
    g2.restore(&mut nic2).expect("restore");
    assert_eq!(
        nic2.snapshot(),
        prior,
        "disarm restores the exact prior state"
    );
    drop(guard);
}

#[test]
fn test_arm_failure_at_every_step_leaves_nic_untouched() {
    let flows = vec![flow(50001, 14), flow(50002, 15)];
    for fail in [
        FailAt::Channels,
        FailAt::Hds,
        FailAt::RssGet,
        FailAt::RssSet,
        FailAt::TableSize,
        FailAt::Locs,
        FailAt::Insert(0),
        FailAt::Insert(1),
    ] {
        let mut nic = MockNic::new(16, 128);
        // Pre-existing foreign rule OUTSIDE the reserved range must survive
        // every failure path.
        nic.rules.insert(3, flow(9999, 1));
        nic.fail_at = fail;
        let prior = nic.snapshot();
        let err = arm_steering(&mut nic, &flows)
            .err()
            .unwrap_or_else(|| panic!("arm must refuse under {fail:?}"));
        assert!(!err.is_empty());
        assert_eq!(
            nic.snapshot(),
            prior,
            "NIC must be byte-identical after refusal at {fail:?}"
        );
    }
}

#[test]
fn test_crash_residue_reap_deletes_only_reserved_range_rules() {
    let mut nic = MockNic::new(16, 128);
    let (lo, _hi) = reserved_loc_range(nic.table_size);
    // A stale lane rule from a crashed daemon (reserved range) and a foreign
    // rule (operator-owned, outside the range).
    nic.rules.insert(lo + 1, flow(40001, 15));
    nic.rules.insert(7, flow(40002, 2));

    let flows = vec![flow(50001, 14)];
    let mut guard = arm_steering(&mut nic, &flows).expect("arm");
    assert!(
        !nic.rules.contains_key(&(lo + 1)),
        "stale reserved-range rule must be reaped at arm"
    );
    assert!(
        nic.rules.contains_key(&7),
        "foreign rule outside the range must never be touched"
    );
    guard.restore(&mut nic).expect("restore");
    assert!(nic.rules.contains_key(&7), "foreign rule survives restore");
    assert_eq!(nic.rules.len(), 1, "only the foreign rule remains");
}

#[test]
fn test_hds_off_refuses_with_zero_mutations() {
    let mut nic = MockNic::new(16, 128);
    nic.hds_on = false;
    let prior = nic.snapshot();
    let err = arm_steering(&mut nic, &[flow(50001, 15)]).expect_err("HDS off must refuse the arm");
    assert!(
        err.contains("data-split") || err.contains("data split") || err.contains("HDS"),
        "refusal must name the law: {err}"
    );
    assert_eq!(nic.snapshot(), prior, "zero mutations on HDS refusal");
    assert!(
        !nic.ops
            .iter()
            .any(|o| o.starts_with("rss_set") || o.starts_with("insert")),
        "no mutating op may run before the HDS gate: {:?}",
        nic.ops
    );
}

#[test]
fn test_arm_refuses_empty_flows_and_full_reserved_range() {
    let mut nic = MockNic::new(16, 128);
    assert!(
        arm_steering(&mut nic, &[]).is_err(),
        "no flows ⇒ nothing to steer ⇒ refuse (never a silent no-op arm)"
    );

    // More flows than reserved slots must refuse, untouched.
    let mut nic = MockNic::new(16, 128);
    let many: Vec<FlowRule> = (0..STEERING_RESERVED_SLOTS + 1)
        .map(|i| flow(50000 + i as u16, 15))
        .collect();
    let prior = nic.snapshot();
    assert!(arm_steering(&mut nic, &many).is_err());
    assert_eq!(nic.snapshot(), prior);
}
