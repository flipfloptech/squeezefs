//! Two-process IPC-hop rig — the G-L4-1 go/no-go spike
//! (design-preload-interception §5.8 / PR L4-2).
//!
//! PR L4-2 red phase: stub. The full rig (daemon-role + client-role over a
//! real sealed-memfd session, echo / serve-shaped / tokio-handoff legs)
//! lands in the green commit; this stub exists so the smoke suite compiles
//! and fails red.

fn main() {
    eprintln!("ipc_hop_rig: not implemented (PR L4-2 red phase)");
    std::process::exit(2);
}
