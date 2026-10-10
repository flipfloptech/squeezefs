//! The zcrx read lane (charter §6.3; `docs/porting/ops_zcrx.md`; decision
//! 0003 §G): a SECOND userspace NVMe/TCP host association per nvme-tcp data
//! device serving COLD read fills only — the kernel initiator keeps writes,
//! metadata, admin, discovery, multipath and recovery. One lane queue = one
//! io_uring (`DEFER_TASKRUN | SINGLE_ISSUER | CQE32`) = one zcrx ifq = one
//! NIC RX queue = one TCP connection; multishot `RECV_ZC` into a PMD-aligned
//! NUMA-bound area; a streaming PDU parser; ONE gather pass into the
//! caller's destination.
//!
//! Two strata, the probe is the degrade point: the PORTABLE cores (the
//! byte-exact io_uring surface, the PDU codecs and stream parser, the span
//! ledger with its exhaustive model, the sizing derivations, the fill table
//! and whole-read admission, the park governor, the probe ladder, the
//! steering state machine over a NIC face) run on every host; the
//! ASSOCIATION (Linux) connects for real against nvmet-tcp with plain
//! `RECV` into granted area chunks standing in for `RECV_ZC` — everything
//! above the opcode is the product code; the NIC leg (ifq registration,
//! RSS/ntuple steering, HDS, live `RECV_ZC`) is hardware-owed: on a host
//! without a steerable HDS-capable NIC the ladder refuses LOUDLY at its
//! gate, `zcrx_lane_absent_reason` names why, and the mount stays on the
//! kernel path byte-identical.

const std = @import("std");
const builtin = @import("builtin");

pub const stats = @import("zcrx/stats.zig");
pub const abi = @import("zcrx/abi.zig");
pub const pdu = @import("zcrx/pdu.zig");
pub const pdu_stream = @import("zcrx/pdu_stream.zig");
pub const span_ledger = @import("zcrx/span_ledger.zig");
pub const span_ledger_model = @import("zcrx/span_ledger_model.zig");
pub const area = @import("zcrx/area.zig");
pub const fill_table = @import("zcrx/fill_table.zig");
pub const governor = @import("zcrx/governor.zig");
pub const steering = @import("zcrx/steering.zig");
pub const nic_census = @import("zcrx/nic_census.zig");
pub const probe = @import("zcrx/probe.zig");
pub const ethtool = if (builtin.os.tag == .linux) @import("zcrx/ethtool.zig") else struct {};
pub const ethtool_nl = if (builtin.os.tag == .linux) @import("zcrx/ethtool_nl.zig") else struct {};
pub const funnel = @import("zcrx/funnel.zig");
pub const lane = if (builtin.os.tag == .linux) @import("zcrx/lane.zig") else struct {};
pub const association = if (builtin.os.tag == .linux) @import("zcrx/association.zig") else struct {};
pub const arm = if (builtin.os.tag == .linux) @import("zcrx/arm.zig") else struct {};

test {
    _ = stats;
    _ = abi;
    _ = pdu;
    _ = pdu_stream;
    _ = span_ledger;
    _ = span_ledger_model;
    _ = area;
    _ = fill_table;
    _ = governor;
    _ = steering;
    _ = nic_census;
    _ = probe;
    if (builtin.os.tag == .linux) _ = ethtool;
    if (builtin.os.tag == .linux) _ = ethtool_nl;
    _ = funnel;
    if (builtin.os.tag == .linux) _ = lane;
    if (builtin.os.tag == .linux) _ = association;
    if (builtin.os.tag == .linux) _ = arm;
}
