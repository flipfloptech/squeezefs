#!/usr/bin/env bash
# Fabric-RTT emulation for the tcp devsub (r4 dd-lane-depth campaign —
# `.benchmarks/2026-08-08-dd-lane-depth-r4.md` Task 2): port-SCOPED netem
# on lo, delaying ONLY the devsub's NVMe/TCP service port (default 54129)
# in BOTH directions — `delay $HALF_US` each way ⇒ ~2×HALF_US of added
# RTT, defaulting to ≈300 µs (the squeeze-test fabric class). Scoped on
# purpose: a bare `netem root` on lo would tax every localhost user on a
# shared box (foreign agents, resolvers) and pollute the rows.
#
# Live nvme-tcp connections SURVIVE the qdisc swap (verified: qdisc
# replacement only re-queues packets; TCP absorbs the transient — apply
# is still done between legs, never mid-row). `status` prints the tree;
# `off` restores the default noqueue root. Verification handles:
#   * scope probe: ICMP/other-port latency unchanged (no filter match);
#   * effect probe: raw QD1 4k randread clat on an oss namespace jumps
#     by ≈ the emulated RTT (the rig's red-baseline leg prints it).
set -u
DEV="${DEV:-lo}"
PORT="${PORT:-54129}"
HALF_US="${HALF_US:-150}"
LIMIT="${LIMIT:-100000}"   # netem queue cap in PACKETS ≫ BDP at 1M+ IOPS

case "${1:-}" in
  on)
    tc qdisc replace dev "$DEV" root handle 1: prio bands 4 \
      priomap 1 2 2 2 1 2 0 0 1 1 1 1 1 1 1 1
    tc qdisc replace dev "$DEV" parent 1:4 handle 40: netem \
      delay "${HALF_US}us" limit "$LIMIT"
    tc filter add dev "$DEV" protocol ip parent 1:0 prio 1 u32 \
      match ip dport "$PORT" 0xffff flowid 1:4
    tc filter add dev "$DEV" protocol ip parent 1:0 prio 1 u32 \
      match ip sport "$PORT" 0xffff flowid 1:4
    echo "fabric-emu ON: ${DEV} port ${PORT} delay ${HALF_US}us/way (~$((2 * HALF_US))us RTT)"
    ;;
  off)
    tc qdisc del dev "$DEV" root 2>/dev/null || true
    echo "fabric-emu OFF: ${DEV} restored to default root"
    ;;
  status)
    tc -s qdisc show dev "$DEV"
    tc filter show dev "$DEV" parent 1:0 2>/dev/null | head -10
    ;;
  *)
    echo "usage: $0 on|off|status  [DEV=lo PORT=54129 HALF_US=150 LIMIT=100000]" >&2
    exit 2
    ;;
esac
