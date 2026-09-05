#!/usr/bin/env bash
# Local read-row lever A-B-B-A on the tcp dev substrate (root; zc arms only
# on an sqz-series kernel — the row is INVALID unless `fuse3_zc_negotiated`
# is 1 on every leg). ONE binary, ONE knob toggled per leg, four legs A B B A.
# The file set is minted ONCE (fresh format, JOBS x SIZE, 1 MiB seq writes);
# every leg then remounts COLD and runs:
#   r_cold — fio 1 MiB seq reads, 2 readers per file (the EXA follower
#            shape: `numjobs=2*JOBS`, readers j and j+JOBS share file j), qd QD;
#   r_warm — the same readers again on the now-resident set (the warm arms:
#            hot tier / read-lane hold / read cache).
# `.stats` before/after each row so the lever's gauges are read beside
# GiB/s, clat and daemon CPU (`daemon_cpu_ns_by_class` delta).
#
#   sudo FIO=... KNOB=SQUEEZEFS_READ_ZC_SERVE A_VAL=1 B_VAL=0 \
#        OUT=target/r4-abba bash .benchmarks/rigs/2026-09-05-read-lever-abba-local.sh
set -euo pipefail
SQZ="${SQZ:-$PWD/target/release/squeezefs}"
OUT="${OUT:-$PWD/target/read-lever-abba}"
META="${META:-sqmeta:///dev/nvme1n1,/dev/nvme2n1,/dev/nvme3n1,/dev/nvme4n1}"
DATA="${DATA:-sqdata:///dev/nvme5n1,/dev/nvme6n1,/dev/nvme7n1,/dev/nvme8n1}"
MNT="${MNT:-/mnt/sqz-lever}"
JOBS="${JOBS:-8}"; QD="${QD:-8}"; BS="${BS:-1M}"; SIZE="${SIZE:-1G}"
# OFFSET (bytes, default 0): a non-block-aligned start (e.g. 4096) makes
# every read span two blocks, so it takes the pooled-fill slice-out arm
# cold and the tier arms warm — the population a tier-buffer lever acts on;
# aligned direct=1 reads on an armed session ride the direct zc leg and
# never touch a tier. RUNTIME (s, default 0 = one pass): time_based loop
# for a sustained row.
OFFSET="${OFFSET:-0}"; RUNTIME="${RUNTIME:-0}"
KNOB="${KNOB:?KNOB=<name>}"; A_VAL="${A_VAL:?}"; B_VAL="${B_VAL:?}"
FIO="${FIO:-$(command -v fio || true)}"; [ -x "$FIO" ] || { echo "fio missing (set FIO=/path/to/fio)" >&2; exit 2; }
mkdir -p "$OUT" "$MNT"
[ "$(id -u)" = 0 ] || { echo "run as root" >&2; exit 2; }
if ps -eo args | grep -q "[s]queezefs mount $META"; then echo "a daemon is up on $META — refusing" >&2; exit 2; fi
log() { echo "[$(date +%T)] $*" | tee -a "$OUT/driver.log"; }

mount_leg() {  # $1 = env assignment, $2 = tag
  # shellcheck disable=SC2086
  env $1 "$SQZ" mount "$META" "$MNT" --daemon --allow-other --log-file "$OUT/$2.daemon.log" >"$OUT/$2.mount.log" 2>&1
  for _ in $(seq 1 100); do [ -r "$MNT/.stats" ] && break; sleep 0.2; done
}
umount_leg() {
  "$SQZ" umount "$MNT" >"$OUT/$1.umount.log" 2>&1 || umount -l "$MNT"
  for _ in $(seq 1 100); do mountpoint -q "$MNT" || break; sleep 0.2; done
}
write_job() {  # $1 = job file path; 2*JOBS readers, readers j and j+JOBS share file j
  python3 - "$1" "$JOBS" "$QD" "$BS" "$SIZE" "$MNT/rows" "$OFFSET" "$RUNTIME" <<'PY'
import sys
p, jobs, qd, bs, size, d, off, rt = sys.argv[1], int(sys.argv[2]), sys.argv[3], sys.argv[4], sys.argv[5], sys.argv[6], int(sys.argv[7]), int(sys.argv[8])
out = f"[global]\nioengine=libaio\ndirect=1\nbs={bs}\niodepth={qd}\nrw=read\nsize={size}\ndirectory={d}\ngroup_reporting=1\n"
if off:
    out += f"offset={off}\n"
if rt:
    out += f"time_based=1\nruntime={rt}\n"
for j in range(jobs * 2):
    out += f"[r{j}]\nfilename=f.{j % jobs}\n"
open(p, "w").write(out)
PY
}
run_row() {  # $1 = leg tag, $2 = row name
  local tag=$1 row=$2
  cat "$MNT/.stats" > "$OUT/$tag.$row.pre.json"
  write_job "$OUT/$tag.$row.fio"
  "$FIO" --output-format=json --output="$OUT/$tag.$row.json" "$OUT/$tag.$row.fio" >/dev/null
  cat "$MNT/.stats" > "$OUT/$tag.$row.post.json"
  python3 - "$OUT/$tag.$row.json" <<'PY'
import json,sys
d=json.load(open(sys.argv[1])); j=d["jobs"][0]["read"]
bw=j["bw_bytes"]/2**30; p=j["clat_ns"]["percentile"]
print(f"  {sys.argv[1].split('/')[-1]}: {bw:.2f} GiB/s  clat mean {j['clat_ns']['mean']/1e6:.2f} ms  p99 {p.get('99.000000',0)/1e6:.1f} ms")
PY
}

log "== mint: fresh format + $JOBS x $SIZE"
"$SQZ" format "$META" "$DATA" --force >"$OUT/mint.format.log" 2>&1
mount_leg "" mint
mkdir -p "$MNT/rows"
for ((j = 0; j < JOBS; j++)); do dd if=/dev/urandom of="$MNT/rows/f.$j" bs=1M count=$(( ${SIZE%G} * 1024 )) status=none; done
sync; umount_leg mint

for leg in A1 B1 B2 A2; do
  cls=${leg:0:1}; val=$A_VAL; [ "$cls" = B ] && val=$B_VAL
  log "== leg $leg [$KNOB=$val] load=$(cut -d' ' -f1-3 /proc/loadavg)"
  mount_leg "$KNOB=$val" "$leg"
  zc=$(python3 -c "import json;d=json.load(open('$MNT/.stats'));print(d.get('metrics',{}).get('fuse3_zc_negotiated', d.get('fuse3_zc_negotiated')))")
  log "   fuse3_zc_negotiated=$zc"
  run_row "$leg" r_cold | tee -a "$OUT/driver.log"
  run_row "$leg" r_warm | tee -a "$OUT/driver.log"
  umount_leg "$leg"
done
log "== done; rows in $OUT"
