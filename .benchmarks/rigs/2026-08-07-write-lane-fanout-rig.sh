#!/usr/bin/env bash
# Write-lane fan-out local D15 rig (2026-08-07, perf/write-lane-fanout).
# Venue: the LOCAL tcp devsub (nvmet-tcp on 127.0.0.1 — the two-substrate
# rule's fabric-sensitive venue; meta /dev/nvme{1..4}n1, data
# /dev/nvme{5..8}n1, zram-backed). Shared venue rules: own mountpoint
# (/mnt/sqz-wlanes), fresh format per leg, format WAITS OUT foreign
# writer-guard holds (never tears a foreign mount down).
#
# Legs run A-B-B-A (A = derived write lanes, B = SQUEEZEFS_NVME_WRITE_LANES=1
# — the pre-change posture). On this zram venue the DEVICE may cap before
# the per-connection wall shows, so a flat GB/s delta is EXPECTED (the read
# fan-out was also PAR locally); the acceptance instruments are
#   (a) correctness — md5 + the P0 shape (cp && sync f) x3 clean per leg,
#   (b) engagement — data_write_lanes + per-lane data_write_lane_submits
#       deltas: A legs must move >1 lane per data device, B legs exactly
#       lane 0.
# The raw discriminator (1 vs 4 submitters/dev at the same in-flight,
# writes) runs LAST — it is destructive to the namespaces (the venue
# convention is fresh-format-per-use anyway) and gated on nobody holding
# them.
set -u

REPO_DIR="$(cd "$(dirname "$0")/../.." && pwd)"
BIN="${SQUEEZEFS_BIN:-$REPO_DIR/target/release/squeezefs}"
MNT="${MNT:-/mnt/sqz-wlanes}"
META="sqmeta:///dev/nvme1n1,/dev/nvme2n1,/dev/nvme3n1,/dev/nvme4n1"
DATA="sqdata:///dev/nvme5n1,/dev/nvme6n1,/dev/nvme7n1,/dev/nvme8n1"
DATA_DEVS=(/dev/nvme5n1 /dev/nvme6n1 /dev/nvme7n1 /dev/nvme8n1)
OUT="${1:-/tmp/wlanes-$(date +%Y%m%d-%H%M%S)}"
LEGS=(A B B A)
FIO_JOBS="${FIO_JOBS:-8}"
FIO_SIZE="${FIO_SIZE:-2g}"

mkdir -p "$OUT"
fatal() { echo "FATAL: $*" >&2; exit 1; }

jget() { python3 -c "import json,sys;d=json.load(open(sys.argv[1]));d=d.get('metrics',d);print(d.get(sys.argv[2],0))" "$1" "$2"; }

do_umount() {
  if mountpoint -q "$MNT"; then
    "$BIN" umount "$MNT" >/dev/null 2>&1 || sudo umount "$MNT" || true
  fi
  for _ in $(seq 1 120); do
    mountpoint -q "$MNT" || break
    sleep 1
  done
  mountpoint -q "$MNT" && fatal "own mountpoint still mounted"
  # Wait for OUR daemon (the one whose cmdline names $MNT) to exit.
  for _ in $(seq 1 120); do
    pgrep -f "squeezefs mount .* $MNT" >/dev/null 2>&1 || break
    sleep 1
  done
  return 0
}

format_fs() { # waits out foreign writer-guard holds (shared venue law)
  local tries=0
  while true; do
    tries=$((tries+1))
    if sudo "$BIN" format "$META" "$DATA" --force >"$OUT/format-$1.log" 2>&1; then
      sudo udevadm settle 2>/dev/null || true
      return 0
    fi
    if [ $tries -ge 360 ]; then
      fatal "format did not acquire the volume set after $tries tries (foreign hold?) — see $OUT/format-$1.log"
    fi
    sleep 10
  done
}

do_mount() { # $1 = leg tag, $2 = A|B
  local env=()
  [ "$2" = B ] && env=(SQUEEZEFS_NVME_WRITE_LANES=1)
  sudo mkdir -p "$MNT"
  sudo env "${env[@]}" "$BIN" mount "$META" "$MNT" --daemon --allow-other \
    --log-file "$OUT/mount-$1.log" >/dev/null 2>&1
  local ok=0
  for _ in $(seq 1 60); do
    if mountpoint -q "$MNT" && sudo cat "$MNT/.stats" >/dev/null 2>&1; then ok=1; break; fi
    sleep 1
  done
  [ "$ok" = 1 ] || fatal "mount gate failed (leg $1)"
  sudo cat "$MNT/.stats" > "$OUT/leg$1-arm.json"
  local lanes
  lanes=$(jget "$OUT/leg$1-arm.json" data_write_lanes)
  if [ "$2" = B ]; then
    [ "$lanes" = 1 ] || fatal "leg $1: B leg data_write_lanes=$lanes != 1"
  else
    [ "$lanes" -gt 1 ] || fatal "leg $1: A leg data_write_lanes=$lanes not >1 — fan-out did not arm"
  fi
  echo "leg $1: mounted, data_write_lanes=$lanes"
}

p0_smoke() { # $1 = leg tag — md5 + the P0 shape (cp && sync f) x3
  local d="$MNT/wlanes_smoke_$1" a b
  sudo mkdir -p "$d"
  dd if=/dev/urandom of=/tmp/wlanes_smoke.src bs=1M count=256 status=none
  a=$(md5sum < /tmp/wlanes_smoke.src | cut -d' ' -f1)
  for i in 1 2 3; do
    sudo cp /tmp/wlanes_smoke.src "$d/f$i" && sudo sync "$d/f$i" || fatal "leg $1: cp && sync f$i failed"
    b=$(sudo md5sum "$d/f$i" | cut -d' ' -f1)
    [ "$a" = "$b" ] || fatal "leg $1: md5 mismatch on f$i ($a vs $b) — CORRUPTION"
  done
  sudo rm -rf "$d"; rm -f /tmp/wlanes_smoke.src
  echo "leg $1: P0 smoke OK (cp && sync x3, md5 clean)"
}

run_row() { # $1 = leg tag — seq-write 1M row (instrument: fio libaio direct)
  sudo mkdir -p "$MNT/wlanes_fio_$1"
  sudo fio --name=wl --directory="$MNT/wlanes_fio_$1" --ioengine=libaio \
    --direct=1 --rw=write --bs=1M --iodepth=8 --numjobs="$FIO_JOBS" \
    --size="$FIO_SIZE" --group_reporting --fallocate=none --end_fsync=1 \
    --output-format=json --output="$OUT/leg$1-fio.json" >/dev/null 2>&1 \
    || fatal "leg $1: fio row failed"
}

engagement() { # $1 = leg tag, $2 = A|B
  python3 - "$OUT" "$1" "$2" <<'EOF'
import json, sys
out, leg, mode = sys.argv[1], sys.argv[2], sys.argv[3]
def load(p):
    d = json.load(open(p)); return d.get("metrics", d)
b, a = load(f"{out}/leg{leg}-before.json"), load(f"{out}/leg{leg}-after.json")
f = json.load(open(f"{out}/leg{leg}-fio.json"))
bw = sum(j["write"]["bw_bytes"] for j in f["jobs"]) / 1e9
io = sum(j["write"]["io_bytes"] for j in f["jobs"]) / 1e9
def lanes(snap):
    return {e.split("=")[0]: [int(x) for x in e.split("=")[1].split(",")]
            for e in snap.get("data_write_lane_submits", [])}
lb, la = lanes(b), lanes(a)
moved_multi = 0
bad = []
for dev, after in la.items():
    before = lb.get(dev, [0]*len(after))
    before += [0] * (len(after) - len(before))
    deltas = [x - y for x, y in zip(after, before)]
    moved = sum(1 for d in deltas if d > 0)
    if sum(deltas) > 0:
        if moved > 1: moved_multi += 1
        if mode == "B" and any(d > 0 for d in deltas[1:]):
            bad.append(f"{dev}: B leg moved lanes beyond 0: {deltas}")
    print(f"  {dev}: lane submit deltas {deltas}")
print(f"leg {leg} ({mode}): fio wrote {io:.1f} GB at {bw:.3f} GB/s; "
      f"devices with >1 submitting lane: {moved_multi}")
if mode == "A" and moved_multi == 0:
    sys.exit("ENGAGEMENT FAIL: A leg moved no device across >1 lane")
if bad:
    sys.exit("ENGAGEMENT FAIL: " + "; ".join(bad))
EOF
  [ $? -eq 0 ] || fatal "leg $1: engagement verdict failed"
}

# ---------------- legs ----------------
command -v fio >/dev/null || fatal "fio not installed"
[ -x "$BIN" ] || fatal "binary missing: $BIN"
leg=0
for mode in "${LEGS[@]}"; do
  leg=$((leg+1)); tag="$leg$mode"
  echo "=== leg $tag ==="
  do_umount
  format_fs "$tag"
  do_mount "$tag" "$mode"
  p0_smoke "$tag"
  sudo cat "$MNT/.stats" > "$OUT/leg$tag-before.json"
  run_row "$tag"
  sudo cat "$MNT/.stats" > "$OUT/leg$tag-after.json"
  engagement "$tag" "$mode"
  do_umount
done

# ---------------- raw discriminator (destructive; gated) ----------------
for d in "${DATA_DEVS[@]}"; do
  sudo fuser "$d" >/dev/null 2>&1 && fatal "raw leg refused: $d is held"
done
echo "=== raw discriminator (1 vs 4 submitters/dev, same in-flight) ==="
raw_row() { # $1 = tag, $2 = numjobs-per-dev, $3 = iodepth
  local args=()
  local i=0
  for d in "${DATA_DEVS[@]}"; do
    args+=(--name="w$i" --filename="$d" --numjobs="$2" --iodepth="$3")
    i=$((i+1))
  done
  sudo fio --ioengine=libaio --direct=1 --rw=write --bs=4M --size=6g \
    --time_based --runtime=30 --ramp_time=5 --group_reporting \
    --output-format=json --output="$OUT/raw-$1.json" "${args[@]}" >/dev/null 2>&1
  python3 -c "
import json
f = json.load(open('$OUT/raw-$1.json'))
bw = sum(j['write']['bw_bytes'] for j in f['jobs']) / 1e9
print(f'raw $1: {bw:.2f} GB/s')"
}
raw_row 1sub-qd16 1 16
raw_row 4sub-qd4 4 4
echo "rig complete — artifacts in $OUT"
