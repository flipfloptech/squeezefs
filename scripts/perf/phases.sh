#!/usr/bin/env bash
# phases.sh <dir> <label> — mdtest's file phases one at a time (create, stat, read, remove), each under the
# FUSE opcode trace when <dir> is the SqueezeFS mount: requests per user op and their kernel round trips.
set -uo pipefail
D=$1; L=$2; H=$(cd "$(dirname "$0")" && pwd)
rm -rf "$D/mdp"; mkdir -p "$D/mdp"
for ph in C T E r; do
  case $ph in C) n=create ;; T) n=stat ;; E) n=read ;; r) n=remove ;; esac
  echo "== $n"
  case "$D" in
    /mnt/sqz*) "$H/opcount.sh" "$L-$n" mpirun -np 4 mdtest -n 2000 -F -$ph -d "$D/mdp" > /tmp/phase-$n.txt 2>&1; tail -6 /tmp/phase-$n.txt | grep -E "FUSE_" ;;
    *) mpirun -np 4 mdtest -n 2000 -F -$ph -d "$D/mdp" > /tmp/phase-$n.txt 2>&1 ;;
  esac
  grep -E "^ *File (creation|stat|read|removal)" /tmp/phase-$n.txt | awk '{print "rate", $1, $2, $3}'
done
rm -rf "$D/mdp"
