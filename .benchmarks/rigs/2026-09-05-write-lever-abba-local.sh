#!/usr/bin/env bash
# Local write-row lever A-B-B-A on the tcp dev substrate: ONE binary, ONE
# knob toggled per leg (A = the lever's default, B = its pre-campaign value),
# four legs A B B A, fresh format per leg. Each leg runs the w_fresh row
# (fio libaio direct=1, JOBS x qd QD, BS seq writes minting SIZE per job)
# and then the w_rewrite row (the identical job over the files it just
# wrote — the CoW displacement + deferred-reclaim regime), and snapshots
# `.stats` before/after each row so the lever's own gauges are read beside
# GiB/s and clat p99.9. Instrument stated per row (the standing lesson).
#
#   sudo KNOB=SQUEEZEFS_WRITE_GUARD_NARROW A_VAL=1 B_VAL=0 \
#        OUT=target/w2-abba bash .benchmarks/rigs/2026-09-05-write-lever-abba-local.sh
#   sudo KNOBS_B="SQUEEZEFS_RECLAIM_QUEUE_MAX_BLOCKS=4096 SQUEEZEFS_RECLAIM_CAP_PARK_MS=1000" \
#        OUT=target/w4-abba bash .benchmarks/rigs/2026-09-05-write-lever-abba-local.sh
#
# KNOB/A_VAL/B_VAL toggles one knob; KNOBS_B is the alternative form for
# levers whose B leg pins several knobs (A leg = all unset = derived).
set -eu
SQZ="${SQZ:-$PWD/target/release/squeezefs}"
OUT="${OUT:-$PWD/target/write-lever-abba}"
META="${META:-sqmeta:///dev/nvme1n1,/dev/nvme2n1,/dev/nvme3n1,/dev/nvme4n1}"
DATA="${DATA:-sqdata:///dev/nvme5n1,/dev/nvme6n1,/dev/nvme7n1,/dev/nvme8n1}"
MNT="${MNT:-/mnt/sqz-lever}"
JOBS="${JOBS:-16}"; QD="${QD:-16}"; BS="${BS:-1M}"; SIZE="${SIZE:-1G}"
RUNTIME="${RUNTIME:-30}"
KNOB="${KNOB:-}"; A_VAL="${A_VAL:-}"; B_VAL="${B_VAL:-}"; KNOBS_B="${KNOBS_B:-}"
mkdir -p "$OUT" "$MNT"
[ "$(id -u)" = 0 ] || { echo "run as root" >&2; exit 2; }
FIO="${FIO:-$(command -v fio || true)}"; [ -x "$FIO" ] || { echo "fio missing (set FIO=/path/to/fio)" >&2; exit 2; }
if ps -eo args | grep -q "[s]queezefs mount $META"; then echo "a daemon is up on $META — refusing" >&2; exit 2; fi
log() { echo "[$(date +%T)] $*" | tee -a "$OUT/driver.log"; }

leg_env() {  # prints the env assignments for leg $1 (A|B)
  if [ -n "$KNOB" ]; then
    case $1 in A) echo "$KNOB=$A_VAL";; B) echo "$KNOB=$B_VAL";; esac
  else
    case $1 in A) echo "";; B) echo "$KNOBS_B";; esac
  fi
}
fio_job() {  # $1 = row name
  cat <<EOF
[global]
ioengine=libaio
direct=1
bs=$BS
rw=write
iodepth=$QD
numjobs=$JOBS
size=$SIZE
directory=$MNT/rows
filename_format=f.\$jobnum
group_reporting=1
time_based=0
[$1]
EOF
}
run_row() {  # $1 = leg tag, $2 = row name
  local tag=$1 row=$2
  cp "$MNT/.stats" "$OUT/$tag.$row.pre.json"
  fio_job "$row" > "$OUT/$tag.$row.fio"
  "$FIO" --output-format=json --output="$OUT/$tag.$row.json" "$OUT/$tag.$row.fio" >/dev/null
  cp "$MNT/.stats" "$OUT/$tag.$row.post.json"
  python3 - "$OUT/$tag.$row.json" <<'PY'
import json,sys
d=json.load(open(sys.argv[1])); j=d["jobs"][0]["write"]
bw=j["bw_bytes"]/2**30; p=j["clat_ns"]["percentile"]
print(f"  {sys.argv[1].split('/')[-1]}: {bw:.2f} GiB/s  clat mean {j['clat_ns']['mean']/1e6:.2f} ms  p99 {p.get('99.000000',0)/1e6:.1f} ms  p99.9 {p.get('99.900000',0)/1e6:.1f} ms")
PY
}
for leg in A1 B1 B2 A2; do
  cls=${leg:0:1}
  envs=$(leg_env "$cls")
  log "== leg $leg [${envs:-derived/default}] load=$(cut -d' ' -f1-3 /proc/loadavg)"
  "$SQZ" format "$META" "$DATA" --force >"$OUT/$leg.format.log" 2>&1
  # shellcheck disable=SC2086
  env $envs "$SQZ" mount "$META" "$MNT" --daemon --allow-other --log-file "$OUT/$leg.daemon.log" >"$OUT/$leg.mount.log" 2>&1
  for _ in $(seq 1 100); do [ -r "$MNT/.stats" ] && break; sleep 0.2; done
  mkdir -p "$MNT/rows"
  run_row "$leg" w_fresh   | tee -a "$OUT/driver.log"
  run_row "$leg" w_rewrite | tee -a "$OUT/driver.log"
  "$SQZ" umount "$MNT" >"$OUT/$leg.umount.log" 2>&1 || umount -l "$MNT"
  for _ in $(seq 1 100); do mountpoint -q "$MNT" || break; sleep 0.2; done
done
log "== done; rows in $OUT"
