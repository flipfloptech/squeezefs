#!/usr/bin/env bash
# capability_matrix.sh — the sqz kernel program's before/after matrix.
# Read-only except the two explicitly-flagged probe arms. Run as root.
#
#   capability_matrix.sh <ifname> [--bind-rxq N]
#
# Rows: kernel identity, mlx5 tcp-data-split attr (hds_query GET),
# HW-GRO state, zcrx opcode surface, kmbuf/FUSE-zc surface, FUSE uapi
# level (kallsyms + module param census), fuse.enable_uring. The
# optional --bind-rxq arm runs the real ZCRX bind (queue restart!).
set -u

here=$(cd "$(dirname "$0")" && pwd)
ifname=${1:?usage: capability_matrix.sh <ifname> [--bind-rxq N]}
bind_rxq=""
[ "${2:-}" = "--bind-rxq" ] && bind_rxq=${3:?}

cc=${CC:-gcc}
for t in hds_query zcrx_smoke kmbuf_smoke; do
	[ -x "$here/$t" ] || $cc -O2 -o "$here/$t" "$here/$t.c" || exit 1
done

echo "=== sqz capability matrix @ $(date -u +%FT%TZ) ==="
echo "kernel: $(uname -r)"
echo "-- mlx5 / HDS --"
ethtool -k "$ifname" 2>/dev/null | grep -E "rx-gro-hw|large-receive" || true
"$here/hds_query" "$ifname" || true
echo "-- zcrx --"
"$here/zcrx_smoke" surface || true
if [ -n "$bind_rxq" ]; then
	ifidx=$(cat "/sys/class/net/$ifname/ifindex")
	echo "(bind arm: ifidx=$ifidx rxq=$bind_rxq — restarts the queue)"
	"$here/zcrx_smoke" bind "$ifidx" "$bind_rxq" || true
fi
echo "-- FUSE zc / kmbuf --"
"$here/kmbuf_smoke" || true
grep -c "io_register_kmbuf_ring\|io_uring_is_kmbuf_ring" /proc/kallsyms \
	2>/dev/null | sed 's/^/kmbuf kallsyms: /'
modprobe fuse 2>/dev/null || true
grep -c "fuse_uring" /proc/kallsyms 2>/dev/null | sed 's/^/fuse_uring kallsyms: /'
for p in enable_uring; do
	v=$(cat "/sys/module/fuse/parameters/$p" 2>/dev/null || echo "n/a")
	echo "fuse.$p: $v"
done
echo "-- storage side --"
for m in nvme_tcp nvmet nvmet_tcp null_blk zram brd; do
	modinfo -F name "$m" >/dev/null 2>&1 && echo "module present: $m" \
		|| echo "module MISSING: $m"
done
echo "=== end matrix ==="
