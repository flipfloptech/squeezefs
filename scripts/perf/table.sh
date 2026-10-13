#!/usr/bin/env bash
# table.sh <dir> <label> [phases]  — the dev-perf table's instruments against one directory.
# phases: comma list of mdtest,randread,randwrite,ior (default all)
set -uo pipefail
D=$1; L=$2; P=${3:-mdtest,randread,randwrite,ior}
OUT=/build/perf/$L; mkdir -p "$OUT"
stats() { [ -r /mnt/sqz/.stats ] && case "$D" in /mnt/sqz*) cat /mnt/sqz/.stats > "$OUT/$1.stats.json" ;; esac; true; }
has() { case ",$P," in *",$1,"*) return 0 ;; esac; return 1; }
if has mdtest; then
  rm -rf "$D/md"; mkdir -p "$D/md"
  stats mdtest.before
  mpirun -np 4 mdtest -n 2000 -d "$D/md" > "$OUT/mdtest.txt" 2>&1
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
  fio --name=w --directory="$D" --filename=rr.dat --rw=randwrite --bs=4k --size=256m --direct=1 --ioengine=io_uring --iodepth=32 --runtime=15 --time_based --output-format=json > "$OUT/randwrite.json" 2>&1
  stats randwrite.after
  python3 -c 'import json,sys; j=json.load(open(sys.argv[1])); r=j["jobs"][0]["write"]; print("randwrite iops %.0f lat_mean_us %.1f p99_us %.0f" % (r["iops"], r["lat_ns"]["mean"]/1e3, r["clat_ns"]["percentile"]["99.000000"]/1e3))' "$OUT/randwrite.json"
fi
has randread || has randwrite && rm -f "$D/rr.dat"
if has ior; then
  stats ior.before
  mpirun -np 4 ior -a POSIX -t 1m -b 64m -F -o "$D/ior.dat" > "$OUT/ior.txt" 2>&1
  stats ior.after
  grep -E '^(write|read) ' "$OUT/ior.txt" | awk '{printf "ior %s %s MiB/s\n",$1,$2}'
  rm -f "$D"/ior.dat*
fi
