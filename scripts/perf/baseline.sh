#!/usr/bin/env bash
# baseline.sh <session> [rep] — one session of the perf baseline (docs/evidence/rung-10-perf-baseline.md):
# matrix.sh against one target, labelled <session>-r<rep>. Run each session holding the venue alone:
#   SQZ_VENUE=kamrui scripts/venue 'flock -o /build/.gate.lock flock -o /build/.build-slot.0 \
#     flock -o /build/.build-slot.1 flock -o /build/.build-slot.2 bash scripts/perf/baseline.sh ram-sqz 1'
# Sessions:
#   lvs-up | lvs-down   the scratch LVs in $VG (perf-ref 16G, perf-mdt{0,1} 4G, perf-ost{0,1} 32G,
#                       perf-slc 64G); lvs-down removes every perf-* LV and nothing else
#   slc                 the drive's SLC-cache knee: 56 GiB of 1 MiB sequential writes on perf-slc,
#                       MiB/s per 5 s
#   ssd-raw ssd-ext4 ssd-sqz   the scratch LVs (blkdiscard first)
#   ram-raw ram-ext4 ram-sqz   ramnvme.sh's namespaces (up and down inside the session)
# Extra matrix.sh knobs pass through the environment (MX_ROWS, MX_PROFILE, …).
set -euo pipefail
HERE=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)
REPO=$(cd "$HERE/../.." && pwd)
S=$1 R=${2:-1} L=$1-r${2:-1}
VG=${PERF_VG:-ubuntu-vg}
LVS="perf-ref:16G perf-mdt0:4G perf-mdt1:4G perf-ost0:32G perf-ost1:32G perf-slc:64G"
M=/mnt/sqz-mx
die() { echo "baseline: $*" >&2; exit 1; }
busy() {   # cores busy over 3 s (/proc/stat)
  local a b; a=($(head -1 /proc/stat)); sleep 3; b=($(head -1 /proc/stat))
  awk -v hz="$(getconf CLK_TCK)" 'BEGIN { split(ARGV[1], a); split(ARGV[2], b); t = 0; for (i = 2; i <= 9; i++) t += b[i] - a[i];
    printf "%.2f", (t - (b[5] - a[5]) - (b[6] - a[6])) / hz / 3; ARGC = 1 }' "${a[*]}" "${b[*]}"
}
quiet() {   # the venue is ours (every lock held); wait ≤ 60 s for what was running to drain
  local c i
  for i in $(seq 1 20); do c=$(busy); awk -v c="$c" 'BEGIN { exit !(c < 0.5) }' && break; done
  echo "baseline: $L start $(date -u +%FT%TZ) busy $c cores (3 s) load $(cut -d' ' -f1-3 /proc/loadavg)"
}
cleanup() {
  mountpoint -q "$M" && umount "$M"
  case "$S" in ram-*) "$HERE/ramnvme.sh" down ;; esac
}
trap cleanup EXIT
ext4_on() {   # $1 device: mkfs, mount at $M, run the matrix, unmount
  mkfs.ext4 -q -F "$1"; mkdir -p "$M"; mount "$1" "$M"
  "$HERE/matrix.sh" "$M" "$L"
  umount "$M"; wipefs -q -a "$1"
}
ram() { "$HERE/ramnvme.sh" up >/dev/null; }
ramdev() { sed -n "s/^$1=//p" /run/sqz-ramnvme/devs; }

case "$S" in
  lvs-up)
    for e in $LVS; do lvcreate -q -y -n "${e%%:*}" -L "${e#*:}" "$VG" >/dev/null; done
    lvs "$VG" ;;
  lvs-down)
    for e in $LVS; do [ -e "/dev/$VG/${e%%:*}" ] && lvremove -q -y "$VG/${e%%:*}"; done
    lvs "$VG" ;;
  slc)
    quiet; blkdiscard "/dev/$VG/perf-slc"; mkdir -p /build/perf
    fio --name=slc --filename="/dev/$VG/perf-slc" --rw=write --bs=1m --iodepth=16 --direct=1 --ioengine=io_uring \
      --size=56g --write_bw_log=/build/perf/slc --log_avg_msec=5000 --output-format=terse >/dev/null
    awk -F, '{printf "%ds %.0f MiB/s\n", $1/1000, $2/1024}' /build/perf/slc_bw.1.log
    blkdiscard "/dev/$VG/perf-slc" ;;
  ssd-raw)
    quiet; blkdiscard "/dev/$VG/perf-ref"; "$HERE/matrix.sh" "/dev/$VG/perf-ref" "$L" ;;
  ssd-ext4)
    quiet; blkdiscard "/dev/$VG/perf-ref"; ext4_on "/dev/$VG/perf-ref" ;;
  ssd-sqz)
    quiet
    for d in mdt0 mdt1 ost0 ost1; do blkdiscard "/dev/$VG/perf-$d"; done
    "$REPO/scripts/perf_mount.sh" --fresh --mdt "/dev/$VG/perf-mdt0" --mdt "/dev/$VG/perf-mdt1" \
      --ost "/dev/$VG/perf-ost0" --ost "/dev/$VG/perf-ost1" -- "$HERE/matrix.sh /mnt/sqz-perf $L" ;;
  ram-raw)
    quiet; ram; "$HERE/matrix.sh" "$(ramdev ost0)" "$L" ;;
  ram-ext4)
    quiet; ram; ext4_on "$(ramdev ost0)" ;;
  ram-sqz)
    quiet; ram; "$REPO/scripts/perf_mount.sh" --ram --fresh -- "$HERE/matrix.sh /mnt/sqz-perf $L" ;;
  *) die "unknown session '$S' (see the header)" ;;
esac
