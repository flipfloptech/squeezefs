#!/usr/bin/env bash
# table.sh <dir> <label> [phases]  — the dev-perf table's instruments against one directory.
# phases: comma list of mdtest,randread,randwrite,ior,oc,append (default the first four)
#   oc      the open/close loops (scripts/perf/ocloop.c): open/close and open + 4 KiB read + close,
#           1 and 4 threads, 5 s each, on one 4 KiB file
#   append  the open-append-close loop (500 × `echo <72 B> >> f`, rung-10-dev-perf.md §10.4)
# Every data phase prints the OSTs' device write bytes it caused (diskstats over $SQZ_TGT/ost*),
# counted once the daemon's write custody settled (owed and parked both 0, ≤ 10 s).
set -uo pipefail
D=$1; L=$2; P=${3:-mdtest,randread,randwrite,ior}
OUT=/build/perf/$L; mkdir -p "$OUT"
T=${SQZ_TGT:-/build/dev/targets}
OC=${SQZ_OCLOOP:-/build/perf/bin/ocloop}
stats() { [ -r /mnt/sqz/.stats ] && case "$D" in /mnt/sqz*) cat /mnt/sqz/.stats > "$OUT/$1.stats.json" ;; esac; true; }
has() { case ",$P," in *",$1,"*) return 0 ;; esac; return 1; }
ostw() { local s=0 t d; for t in "$T"/ost*; do d=$(basename "$(readlink -f "$t")"); s=$((s + $(awk '{print $7}' "/sys/class/block/$d/stat"))); done; echo $((s * 512)); }
settle() { local i=0; while [ $i -lt 100 ]; do grep -q '"fuse_owed_drain_inos":0,' /mnt/sqz/.stats && grep -q '"parked_buffers":0,' /mnt/sqz/.stats && return; sleep 0.1; i=$((i + 1)); done; }
devw() { settle; echo "$1 dev_write_bytes $(( $(ostw) - $2 )) user_bytes $3"; }
if has mdtest; then
  rm -rf "$D/md"; mkdir -p "$D/md"
  stats mdtest.before
  w0=$(ostw)
  mpirun -np 4 mdtest -n 2000 -d "$D/md" > "$OUT/mdtest.txt" 2>&1
  devw mdtest "$w0" 0
  stats mdtest.after
  grep -E '^ *(Directory|File|Tree) (creation|stat|read|removal|rename)' "$OUT/mdtest.txt" | awk '{printf "%s %s %s\n",$1,$2,$3}'
  rm -rf "$D/md"
fi
if has randread; then
  fio --name=lay --directory="$D" --filename=rr.dat --rw=write --bs=1m --size=256m --direct=1 --ioengine=io_uring --iodepth=8 > /dev/null 2>&1
  stats randread.before
  fio --name=r --directory="$D" --filename=rr.dat --rw=randread --bs=4k --size=256m --direct=1 --ioengine=io_uring --iodepth=32 --runtime=15 --time_based --output-format=json > "$OUT/randread.json" 2>&1
  stats randread.after
  python3 -c 'import json,sys; j=json.load(open(sys.argv[1])); r=j["jobs"][0]["read"]; print("randread iops %.0f lat_mean_us %.1f p99_us %.0f" % (r["iops"], r["lat_ns"]["mean"]/1e3, r["clat_ns"]["percentile"]["99.000000"]/1e3))' "$OUT/randread.json"
fi
if has randwrite; then
  stats randwrite.before
  w0=$(ostw)
  fio --name=w --directory="$D" --filename=rr.dat --rw=randwrite --bs=4k --size=256m --direct=1 --ioengine=io_uring --iodepth=32 --runtime=15 --time_based --output-format=json > "$OUT/randwrite.json" 2>&1
  devw randwrite "$w0" "$(python3 -c 'import json,sys; print(json.load(open(sys.argv[1]))["jobs"][0]["write"]["io_bytes"])' "$OUT/randwrite.json")"
  stats randwrite.after
  python3 -c 'import json,sys; j=json.load(open(sys.argv[1])); r=j["jobs"][0]["write"]; print("randwrite iops %.0f lat_mean_us %.1f p99_us %.0f" % (r["iops"], r["lat_ns"]["mean"]/1e3, r["clat_ns"]["percentile"]["99.000000"]/1e3))' "$OUT/randwrite.json"
fi
has randread || has randwrite && rm -f "$D/rr.dat"
if has ior; then
  stats ior.before
  w0=$(ostw)
  mpirun -np 4 ior -a POSIX -t 1m -b 64m -F -o "$D/ior.dat" > "$OUT/ior.txt" 2>&1
  devw ior "$w0" $((4 * 64 << 20))
  stats ior.after
  grep -E '^(write|read) ' "$OUT/ior.txt" | awk '{printf "ior %s %s MiB/s\n",$1,$2}'
  rm -f "$D"/ior.dat*
fi
if has oc; then
  [ -x "$OC" ] || zig cc -O2 -o "$OC" "$(cd "$(dirname "$0")" && pwd)/ocloop.c" -lpthread
  head -c 4096 /dev/urandom > "$D/oc.f"
  for m in oc ocr; do for t in 1 4; do "$OC" $m "$D/oc.f" $t 5; done; done
  rm -f "$D/oc.f"
fi
if has append; then
  stats append.before
  w0=$(ostw); t0=$(date +%s%N)
  i=0; while [ $i -lt 500 ]; do echo "line $(printf %04d $i) of the open-append-close loop: seventy-two bytes in all, newline" >> "$D/ap.f"; i=$((i + 1)); done
  t1=$(date +%s%N)
  devw append500 "$w0" "$(stat -c %s "$D/ap.f")"
  echo "append500 ms $(( (t1 - t0) / 1000000 ))"
  stats append.after
  rm -f "$D/ap.f"
fi
