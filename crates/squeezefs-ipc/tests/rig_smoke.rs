//! Correctness smoke for the two-process IPC-hop rig (PR L4-2): the rig is
//! measurement infrastructure, so its transport correctness is pinned by a
//! real two-process run — a REAL sealed-memfd session over an abstract
//! AF_UNIX socket with SCM_RIGHTS fd-passing, tiny op counts, full payload
//! verification on. Only rig plumbing is exercised here; numbers are the
//! evidence note's job (`tests/run_ipc_hop_bench.sh`).
#![cfg(feature = "rig")]

use std::process::Command;

fn run_rig(args: &[&str]) -> (bool, String) {
    let out = Command::new(env!("CARGO_BIN_EXE_ipc_hop_rig"))
        .args(args)
        .output()
        .expect("rig binary must spawn");
    let stdout = String::from_utf8_lossy(&out.stdout).into_owned();
    let stderr = String::from_utf8_lossy(&out.stderr).into_owned();
    (out.status.success(), format!("{stdout}\n{stderr}"))
}

fn assert_leg_summary(text: &str, leg: &str) {
    let summary = text
        .lines()
        .find(|l| l.starts_with("SUMMARY "))
        .unwrap_or_else(|| panic!("{leg}: no SUMMARY line in output:\n{text}"));
    for key in [
        "leg=",
        "ops=",
        "ops_per_sec=",
        "rtt_p50_ns=",
        "rtt_p99_ns=",
        "client_syscalls_per_op=",
        "daemon_syscalls_per_op=",
        "verify_failures=0",
    ] {
        assert!(
            summary.contains(key),
            "{leg}: SUMMARY missing `{key}`: {summary}"
        );
    }
}

/// Echo leg, verified: every op's 4 KiB payload round-trips through the
/// arena intact (both directions), two real processes, zero failures.
#[test]
fn rig_echo_two_process_verified() {
    let (ok, text) = run_rig(&[
        "--leg",
        "echo",
        "--threads",
        "2",
        "--ops",
        "2000",
        "--warmup",
        "100",
        "--verify",
    ]);
    assert!(ok, "echo leg must exit 0:\n{text}");
    assert_leg_summary(&text, "echo");
}

/// Serve-shaped leg (binding-validation stand-in + scc probe + 2×4 KiB
/// memcpy + stats), verified.
#[test]
fn rig_serve_shaped_two_process_verified() {
    let (ok, text) = run_rig(&[
        "--leg",
        "serve",
        "--threads",
        "2",
        "--service-threads",
        "2",
        "--ops",
        "2000",
        "--warmup",
        "100",
        "--verify",
    ]);
    assert!(ok, "serve leg must exit 0:\n{text}");
    assert_leg_summary(&text, "serve");
}

/// Tokio-handoff leg (service thread → embedded-runtime task → completion
/// post), verified.
#[test]
fn rig_handoff_two_process_verified() {
    let (ok, text) = run_rig(&[
        "--leg",
        "handoff",
        "--threads",
        "2",
        "--ops",
        "2000",
        "--warmup",
        "100",
        "--verify",
    ]);
    assert!(ok, "handoff leg must exit 0:\n{text}");
    assert_leg_summary(&text, "handoff");
}

/// A client process that dies mid-run must not wedge the daemon role: the
/// orchestrator detects the child's failure and exits nonzero itself
/// (bounded, no hang) — the rig-scale stand-in for §5.7 socket-EOF
/// lifecycle.
#[test]
fn rig_client_kill_exits_nonzero_bounded() {
    let (ok, text) = run_rig(&[
        "--leg",
        "echo",
        "--threads",
        "1",
        "--ops",
        "100000000", // absurd op count; the kill knob ends it early
        "--kill-client-after-ms",
        "50",
    ]);
    assert!(
        !ok,
        "a killed client must surface as a nonzero orchestrator exit:\n{text}"
    );
}
