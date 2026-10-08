//! THE cluster wire (charter §5.5 "one wire family over the cluster
//! wire"; §9 thread-per-connection; §1.7 unknown schema refuses; §11 fuzz
//! every wire decoder; decision 0003 §E storage-trust HMAC now, mTLS at
//! rung 8; `docs/porting/sym_wire_membership_recovery.md` §1): every
//! symmetric-plane verb — manager, slot, token, cross-owner, clone,
//! allocation lease, death ledger, membership, record ship — rides ONE
//! frame family over TCP with `TCP_NODELAY`, authenticated by the set's
//! `job:enroll` secret, every word screened before any effect, every
//! refusal typed.
//!
//! Portable: `codec`, `frame`, `verbs`, `screen`, `auth`, `endpoint`,
//! `service`, `session` (the framer and the link law), `loopback` (N
//! in-process daemons without sockets), `stats`. Linux only: `listener`
//! (`sqz-wire-acc` + `sqz-wire-conn`), `dialer` (`sqz-wire-mux`), the TCP
//! e2e. The wire is deliberately NOT on the io_uring hot path (charter
//! §1.2's TCP/TLS exception): its threads are OS threads that block on
//! their own sockets and nothing else.

const builtin = @import("builtin");
const is_linux = builtin.os.tag == .linux;

pub const codec = @import("codec.zig");
pub const frame = @import("frame.zig");
pub const verbs = @import("verbs.zig");
pub const screen = @import("screen.zig");
pub const auth = @import("auth.zig");
pub const endpoint = @import("endpoint.zig");
pub const service = @import("service.zig");
pub const session = @import("session.zig");
pub const loopback = @import("loopback.zig");
pub const stats = @import("stats.zig");
pub const listener = if (is_linux) @import("listener.zig") else struct {};
pub const dialer = if (is_linux) @import("dialer.zig") else struct {};

pub const Verb = verbs.Verb;
pub const Status = frame.Status;
pub const RefusalClass = frame.RefusalClass;
pub const WireIdentity = verbs.WireIdentity;
pub const EndpointWire = endpoint.EndpointWire;
pub const Service = service.Service;
pub const LoopbackHub = loopback.LoopbackHub;

/// The `wire` family of the `.stats` document.
pub const writeStatsJson = stats.writeStatsJson;

test {
    _ = codec;
    _ = frame;
    _ = verbs;
    _ = screen;
    _ = auth;
    _ = endpoint;
    _ = service;
    _ = session;
    _ = loopback;
    _ = stats;
    if (is_linux) {
        _ = listener;
        _ = dialer;
        _ = @import("tcp_e2e_linux.zig");
    }
}
