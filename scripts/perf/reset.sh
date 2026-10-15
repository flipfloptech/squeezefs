#!/usr/bin/env bash
# reset.sh <posture> [bin] — umount /mnt/sqz, reformat the raw targets and mount again.
#   posture: sym2   = 2 MDT + 2 OST symmetric (dev_mount's set)
#            single = 1 MDT (mdt0) --single-writer + 2 OST
#            sym1   = 1 MDT (mdt0) symmetric + 2 OST
#   SQZ_NOCACHE=1 formats without a cache path (every write past inline striped: the data
#   plane's drains reach the OSTs).
set -euo pipefail
P=$1; B=${2:-$(cd "$(dirname "$0")/../.." && pwd)/zig-out-dev/bin/squeezefs}
# The target layout: dev_mount.sh by default; perf_mount.sh with SQZ_TGT=/build/perf/targets SQZ_STATE=/build/perf.
T=${SQZ_TGT:-/build/dev/targets}; SET=${SQZ_STATE:-/build/dev/set}; MNT=/mnt/sqz; LOG=$SET/daemon.log
# The layout words are this script's, not the daemon's (its knob screen warns on every SQZ_*).
C=(--disk-cache-paths "$SET/cache"); [ "${SQZ_NOCACHE:-0}" = 1 ] && C=()
unset SQZ_TGT SQZ_STATE SQZ_NOCACHE
cd /
if mountpoint -q $MNT; then $B umount $MNT; fi
while pgrep -x squeezefs >/dev/null; do sleep 0.2; done
rm -rf $SET/cache; mkdir -p $SET/cache
case $P in
  sym2) META="sqmeta://$T/mdt0,$T/mdt1"; F=() ;;
  single) META="sqmeta://$T/mdt0"; F=(--single-writer) ;;
  sym1) META="sqmeta://$T/mdt0"; F=() ;;
esac
$B format --force ${F[@]+"${F[@]}"} ${C[@]+"${C[@]}"} "$META" "sqdata://$T/ost0,$T/ost1" > $SET/format-$P.log 2>&1 || { tail -5 $SET/format-$P.log; exit 1; }
: > $LOG
$B mount --daemon --supervise --log-file $LOG -o default_permissions,allow_other,fsname=squeezefs-dev "$META" $MNT
grep -E "buffer mode" $LOG | sed 's/.*drain group(s) (width 1), //'
