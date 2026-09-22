#!/usr/bin/env bash
# The symmetric-metadata program's PR-1 SOLO RE-GATE (design gate 1,
# docs/design-symmetric-metadata.md §8) on squeeze-test — an A-B-B-A whose
# arms are BINARIES over one boot, the same fabric set, the same fio jobs
# (the 2026-09-08 campaign-rows rig's shape):
#   A = dev tip BEFORE PR 1 (3228fcb8), B = PR 1 landed (a9827378),
#   S = B with the forest STAMPED at format (SQUEEZEFS_TEST_STAMP_SYMMETRIC=1
#       — bit 17; SCOPING evidence only, the default format is flat at PR 1).
# Per arm, in order: cluster reset (fresh backings + format) → the mdstorm
# leg (tests/run_mdstorm.sh, ITS OWN file-backed /dev/shm substrate, no
# fabric daemon up — so its quiet gate sees an idle box) → the TIMED first
# mount → w_fresh (write_BW on the never-written file set) → the campaign
# prep (write_BW 40 s, the rand rows' precondition) → kern rand-4k read →
# kern rand-4k write → a TIMED clean unmount + remount of the populated set.
# Every fio row snapshots thermal / .stats / /proc/stat and prints the gate
# reads beside it: dlm_rpcs (MUST be 0 — the rig fails loud otherwise),
# Δinvariant_tripwires, Δfsck_findings, the meta_kv journal / checkpoint /
# node-append deltas, and the PR-1 forest gauges (0 on every flat mount).
#
# RT: the measured fio window per row (default 60 s + the job files' 10 s
# ramp). Until 2026-09-22 the CLI `--runtime=$RT` was OVERRIDDEN by the job
# files' own `runtime=30` (a job-section value wins over a CLI option given
# after the job file), so the 2026-09-13 (PR 1) and 2026-09-22 (PR 13b)
# brackets measured 30 s windows — comparable to each other and to the
# campaign rows; `row()` now writes a per-row job copy carrying RT and
# snapshots the data namespaces' /proc/diskstats beside the .stats pair
# (acceptance record §3.9.1b). Rows run with this revision are NOT
# window-comparable to those two brackets unless RT=30.
#
#   sudo env BIN_A=/scratch/tmp/sym-pr1/squeezefs-A \
#            BIN_B=/scratch/tmp/sym-pr1/squeezefs-B \
#            [SEQ="A B B A"] [RT=60] [ROWS="mdstorm mount wfresh-kern rr4k-kern rw4k-kern remount"] \
#        bash 2026-09-13-sym-pr1-solo-regate.sh
#   sudo env BIN_B=... SEQ="S" OUT=<same dir> bash 2026-09-13-sym-pr1-solo-regate.sh   # the stamped leg
#
# Artifacts: $OUT/<arm><pos>-<row>.{stats0,stats1,procstat0,procstat1,
# diskstats0,diskstats1,job,thermal0,thermal1,fio.json,fio.txt,dmesg,_bw.*.log}, $OUT/<arm><pos>.mount.*
# / .remount.* / .umount.time (the timed mount legs + their daemon logs +
# the .stats census read at first mount), $OUT/<arm><pos>-mdstorm.{txt,row,
# pre.json,post.json}, $OUT/reset-<arm><pos>.log, $OUT/format-<arm><pos>.log
# (stamped legs). Reduce with 2026-09-13-sym-pr1-solo-regate-reduce.py <OUT>.
set -u
TS=$(date -u +%Y%m%d-%H%M%S)
OUT=${OUT:-/scratch/tmp/sym-pr1/rows-$TS}
MNT=/scratch/tmp/test
JOBS=/scratch/tmp/fio_jobs
RESET=${RESET:-/scratch/tmp/cluster_reset_v4.sh}
MDSTORM=${MDSTORM:-/scratch/tmp/rigs/mdstorm/tests/run_mdstorm.sh}
MDSTORM_DIR=${MDSTORM_DIR:-/dev/shm/sqz_mdstorm}
SEQ=${SEQ:-"A B B A"}
RT=${RT:-60}
ROWS=${ROWS:-"mdstorm mount wfresh-kern rr4k-kern rw4k-kern remount"}
BIN_S=${BIN_S:-${BIN_B:-}}
mkdir -p "$OUT"
echo "== sym-pr1 solo re-gate $TS host $(hostname) kernel $(uname -r) seq [$SEQ] rows [$ROWS] RT=$RT out $OUT"
for a in $SEQ; do
  v="BIN_$a"; [ -x "${!v:-}" ] || { echo "missing $v"; exit 2; }
  echo "   $a: $("${!v}" --version 2>/dev/null | head -1) sha256 $(sha256sum "${!v}" | cut -c1-16)"
done
echo "   fio: $(fio --version) reset: $RESET mdstorm: $MDSTORM ($MDSTORM_DIR)"
[ -x "$RESET" ] || { echo "missing reset script $RESET"; exit 2; }
[ -x /scratch/tmp/squeezefs ] || { echo "the reset expects a daemon at /scratch/tmp/squeezefs (cp squeezefs-B there first)"; exit 2; }

thermal() { for h in /sys/class/hwmon/hwmon*/temp*_input; do [ -r "$h" ] && echo "hwmon $(basename "$(dirname "$h")")/$(basename "$h")=$(cat "$h")"; done > "$1" 2>/dev/null; }
now() { date +%s.%N; }
dsec() { awk -v a="$1" -v b="$2" 'BEGIN{printf "%.3f", b-a}'; }
arm_env() {  # the stamped arm's format-time seam, NEVER leaked to A/B (a non-SQZ name: the knob gate announces unknown SQZ_* names)
  if [ "$1" = S ]; then echo "SQUEEZEFS_TEST_STAMP_SYMMETRIC=1"; else echo "RIG_ARM=$1"; fi
}

# The gate reads beside every row: dlm_rpcs (absolute — must be 0),
# tripwire / fsck deltas, the meta_kv economy deltas, the forest gauges.
gate_reads() {  # $1 = stats0 $2 = stats1 $3 = label
  python3 - "$1" "$2" "$3" <<'PY'
import json, sys
a = json.load(open(sys.argv[1]))["metrics"]; b = json.load(open(sys.argv[2]))["metrics"]
def d(k):
    x, y = a.get(k), b.get(k)
    return (y - x) if isinstance(x, (int, float)) and isinstance(y, (int, float)) else None
forest = {k: b.get(k) for k in sorted(b) if k.startswith("meta_kv_forest_")}
trip = {k: d(k) for k in ("invariant_tripwires", "fuse_op_watchdog_overdue", "transport_cq_overflows",
                          "transport_lease_overlong", "write_pipeline_fence_drops", "data_dma_fence_refusals",
                          "writeback_errors_latched", "detached_task_panics", "job_worker_panics")}
print("   gate[%s]: dlm_mode=%s dlm_rpcs=%s mount_posture=%s Δfsck_findings=%s Δtripwires=%s" % (
    sys.argv[3], b.get("dlm_mode"), b.get("dlm_rpcs"), b.get("mount_posture"), d("fsck_findings"),
    {k: v for k, v in trip.items() if v}))
print("   meta_kv[%s]: Δjournal_entries=%s Δjournal_bytes=%s Δcheckpoints=%s Δnode_appends=%s Δnode_append_bytes=%s Δblock_refs_drift=%s forest=%s" % (
    sys.argv[3], d("meta_kv_journal_entries"), d("meta_kv_journal_bytes"), d("meta_kv_checkpoints"),
    d("meta_kv_node_appends"), d("meta_kv_node_append_bytes"), d("meta_kv_block_refs_drift"), forest))
bad = []
if b.get("dlm_rpcs") not in (0, None): bad.append("dlm_rpcs=%s" % b.get("dlm_rpcs"))
if forest.get("meta_kv_forest_key_violations"): bad.append("forest_key_violations=%s" % forest["meta_kv_forest_key_violations"])
if bad:
    print("   GATE FAIL[%s]: %s" % (sys.argv[3], ", ".join(bad)))
    sys.exit(3)
PY
}

row() {  # $1 = tag, $2 = jobfile, $3.. = extra fio args
  local tag="$1" jobfile="$2"; shift 2
  # RT is honoured through a per-row COPY of the job file: fio lets a
  # job-section `runtime=` override a CLI `--runtime` given after the job
  # file, so the box's standing files (`runtime=30`) made every fio row of
  # the 2026-09-13 and 2026-09-22 brackets a 30 s window whatever RT said
  # (acceptance record §3.9.1b). The copy carries the RT this run names.
  sed -E "s/^runtime=.*/runtime=$RT/" "$jobfile" > "$OUT/$tag.job"
  grep -q "^runtime=$RT\$" "$OUT/$tag.job" || echo "runtime=$RT" >> "$OUT/$tag.job"
  echo "-- row $tag: $(basename "$jobfile") (runtime=$RT via $tag.job) $* loadavg=$(cut -d' ' -f1-3 /proc/loadavg) $(date -u +%FT%TZ)"
  thermal "$OUT/$tag.thermal0"
  sync; sleep 1
  cat "$MNT/.stats" > "$OUT/$tag.stats0"
  head -1 /proc/stat > "$OUT/$tag.procstat0"
  # The data namespaces' /proc/diskstats lines — the AGENTS write-
  # amplification instrument's device-byte face (sectors written ÷ user
  # bytes); the daemon's own ledger rides the .stats pair beside it.
  grep -E " nvme[0-9]+n[0-9]+ " /proc/diskstats > "$OUT/$tag.diskstats0"
  fio "$OUT/$tag.job" "$@" --output-format=json --output="$OUT/$tag.fio.json" \
    --write_bw_log="$OUT/$tag" --log_avg_msec=1000 > "$OUT/$tag.fio.txt" 2>&1 \
    || { echo "fio failed: $(tail -3 "$OUT/$tag.fio.txt")"; return 1; }
  cat "$MNT/.stats" > "$OUT/$tag.stats1"
  head -1 /proc/stat > "$OUT/$tag.procstat1"
  grep -E " nvme[0-9]+n[0-9]+ " /proc/diskstats > "$OUT/$tag.diskstats1"
  thermal "$OUT/$tag.thermal1"
  python3 - "$OUT/$tag.fio.json" "$OUT/$tag.procstat0" "$OUT/$tag.procstat1" <<'PY'
import json, sys
jobs = json.load(open(sys.argv[1]))["jobs"]
side = "read" if sum(j["read"]["total_ios"] for j in jobs) else "write"
iops = sum(j[side]["iops"] for j in jobs); bw = sum(j[side]["bw_bytes"] for j in jobs) / 2**20
ios = sum(j[side]["total_ios"] for j in jobs); byt = sum(j[side]["io_bytes"] for j in jobs) / 2**30
a = [int(x) for x in open(sys.argv[2]).read().split()[1:]]; b = [int(x) for x in open(sys.argv[3]).read().split()[1:]]
d = [y - x for x, y in zip(a, b)]; tot = sum(d); idle = d[3] + d[4]
print("   fio: %s iops %.0f bw %.0f MiB/s ios %d bytes %.1f GiB | box cpu busy %.1f%% (user %.1f sys %.1f irq %.1f softirq %.1f iowait %.1f)" % (
    side, iops, bw, ios, byt, 100 * (tot - idle) / tot, 100 * d[0] / tot, 100 * d[2] / tot, 100 * d[5] / tot, 100 * d[6] / tot, 100 * d[4] / tot))
PY
  echo "   thermal: $(grep -h hwmon "$OUT/$tag.thermal0" | sort -t= -k2 -n | tail -1) -> $(grep -h hwmon "$OUT/$tag.thermal1" | sort -t= -k2 -n | tail -1)"
  gate_reads "$OUT/$tag.stats0" "$OUT/$tag.stats1" "$tag" || GATE_FAILED=1
  dmesg -T 2>/dev/null | grep -iE "fuse|WARN|lockdep|BUG|nvme.*(error|reset|timeout)" | tail -5 > "$OUT/$tag.dmesg" || true
  [ -s "$OUT/$tag.dmesg" ] && { echo "   dmesg tail:"; sed 's/^/     /' "$OUT/$tag.dmesg"; }
}

# The arm binaries are named squeezefs-A/B, so the daemon's comm is NOT
# `squeezefs` — match the mount argv (the reset script's own pattern).
DAEMON_PAT='squeezefs[-A-Za-z]* moun[t] sqmeta://'
FABRIC_PAT=' moun[t] sqmeta:///dev/nvme'
daemon_pids() { pgrep -f "$DAEMON_PAT"; }
fabric_daemon_up() { pgrep -f "$FABRIC_PAT" >/dev/null; }
kill_daemon() {
  daemon_pids | xargs -r kill 2>/dev/null; sleep 2; daemon_pids | xargs -r kill -9 2>/dev/null
  umount -l "$MNT" 2>/dev/null; sleep 1
}

reset_arm() {  # $1 = arm letter, $2 = tag  → sets META, DATA
  kill_daemon
  echo YES | "$RESET" > "$OUT/reset-$2.log" 2>&1 || { echo "RESET FAILED ($2) — tail:"; tail -20 "$OUT/reset-$2.log"; exit 1; }
  # The URIs from THIS reset's printed mapping lines — namespace numbering
  # moves across resets (the 2026-09-09 n1→n2 lesson); order = the reset's
  # format order (meta in HOSTS order, then data node-major).
  META="sqmeta://$(grep -oE ':[a-z0-9]+-m0 -> /dev/nvme[0-9]+n[0-9]+' "$OUT/reset-$2.log" | sed 's/.*-> //' | paste -sd,)"
  DATA="sqdata://$(grep -oE ':[a-z0-9]+-d[0-9]+ -> /dev/nvme[0-9]+n[0-9]+' "$OUT/reset-$2.log" | sed 's/.*-> //' | paste -sd,)"
  local echoed; echoed="$(grep -oE 'sqmeta://[^ ]+' "$OUT/reset-$2.log" | tail -n 1)"
  [ "$META" = "$echoed" ] || { echo "META URI mismatch: parsed $META vs echoed $echoed ($2)"; exit 1; }
  for d in $(echo "$META,$DATA" | sed 's#sq[a-z]*://##g; s#,# #g'); do [ -b "$d" ] || { echo "device $d is not a block device ($2)"; exit 1; }; done
  echo "   reset ok: meta $META data $DATA"
  kill_daemon; rm -rf "$MNT"/client_validation 2>/dev/null
  [ -z "$(ls -A "$MNT" 2>/dev/null)" ] || { echo "MOUNTPOINT NOT EMPTY ($2)"; exit 1; }
  if [ "$1" = S ]; then
    # The stamped SCOPING leg: the reset's format verbatim (cache-less,
    # default bits) re-run --force under the format-time seam — the reset
    # runs its format through SQZ where the env cannot be scoped to that
    # one command, so the format is re-issued here with the same args.
    local v="BIN_$1"
    env SQUEEZEFS_TEST_STAMP_SYMMETRIC=1 "${!v}" format --force "$META" "$DATA" > "$OUT/format-$2.log" 2>&1 \
      || { echo "STAMPED FORMAT FAILED ($2):"; tail -5 "$OUT/format-$2.log"; exit 1; }
  fi
  # Sector-0 features_incompat (u64 LE at offset 16) per meta volume — bit 17
  # is the forest; must read 1 on S and 0 on A/B.
  for d in $(echo "$META" | sed 's#sqmeta://##; s#,# #g'); do
    dd if="$d" bs=4096 count=1 iflag=direct 2>/dev/null | python3 -c 'import sys,struct; b=sys.stdin.buffer.read(); v=struct.unpack_from("<Q",b,16)[0]; print("   %s features_incompat=%#x bit17=%d" % (sys.argv[1], v, (v>>17)&1))' "$d"
  done | tee "$OUT/features-$2.txt"
  local want=0; [ "$1" = S ] && want=1
  grep -q "bit17=$((1-want))" "$OUT/features-$2.txt" && { echo "STAMP MISMATCH ($2): expected bit17=$want on every meta volume"; exit 1; }
}

mount_timed() {  # $1 = arm letter, $2 = tag, $3 = label (mount|remount)
  local v="BIN_$1"; local bin="${!v}" tag="$2" label="$3" t0 t1 t2 rc armed
  [ -n "${META:-}" ] || { echo "no META URI (run with a reset, or pass META=sqmeta://... with NORESET=1)"; exit 1; }
  t0=$(now)
  env "$(arm_env "$1")" "$bin" mount "$META" "$MNT" --daemon --interception --allow-other \
    --log-file "$OUT/$tag.$label.daemon.log" > "$OUT/$tag.$label.out" 2>&1; rc=$?
  t1=$(now)
  local i; for i in $(seq 1 1200); do cat "$MNT/.stats" > "$OUT/$tag.$label.stats" 2>/dev/null && break; sleep 0.05; done
  t2=$(now)
  armed="$(grep -m1 -oE 'FUSE-over-io_uring session path armed[^]]*' "$OUT/$tag.$label.daemon.log" | head -1)"
  # The armed print carries no timestamp; the INFO line that precedes it
  # (the REGISTER summary) does — second precision (env_logger default).
  local armed_ts; armed_ts="$(grep -m1 'INFO.*FUSE-over-io_uring registered' "$OUT/$tag.$label.daemon.log" | grep -oE '^\[[0-9T:Z.-]+' | tr -d '[')"
  echo "MOUNT $tag $label rc=$rc mount_s=$(dsec "$t0" "$t1") stats_s=$(dsec "$t0" "$t2") t0=$t0 armed_log_ts=${armed_ts:-none} | $(tail -1 "$OUT/$tag.$label.out")" | tee "$OUT/$tag.$label.time"
  [ "$rc" = 0 ] || { echo "MOUNT FAILED ($tag $label):"; tail -20 "$OUT/$tag.$label.daemon.log"; exit 1; }
  [ -s "$OUT/$tag.$label.stats" ] || { echo "no .stats after mount ($tag $label)"; exit 1; }
  echo "   $armed"
  grep -m1 -E 'transport (enabled|geometry)|FUSE-over-io_uring registered' "$OUT/$tag.$label.daemon.log" | sed 's/^/   /' || true
}

umount_timed() {  # $1 = tag
  local t0 t1 tag="$1"
  t0=$(now)
  fusermount3 -u "$MNT" 2>"$OUT/$tag.umount.err" || umount "$MNT" 2>>"$OUT/$tag.umount.err"
  local i; for i in $(seq 1 1200); do fabric_daemon_up || break; sleep 0.05; done
  t1=$(now)
  echo "UMOUNT $tag umount_s=$(dsec "$t0" "$t1") daemon_left=$(daemon_pids | wc -l) $(cat "$OUT/$tag.umount.err" 2>/dev/null | tr '\n' ' ')" | tee "$OUT/$tag.umount.time"
  fabric_daemon_up && { echo "daemon did not exit on clean unmount ($tag) — killing"; kill_daemon; }
}

mdstorm_leg() {  # $1 = arm letter, $2 = tag — the packaged instrument, its own /dev/shm file-backed substrate
  local v="BIN_$1"; local bin="${!v}" tag="$2"
  [ -x "$MDSTORM" ] || { echo "missing mdstorm rig $MDSTORM"; return 1; }
  echo "-- mdstorm $tag: $bin loadavg=$(cut -d' ' -f1-3 /proc/loadavg) $(date -u +%FT%TZ) (substrate: file-backed under $MDSTORM_DIR — barrier-bound, relative rows)"
  rm -rf "$MDSTORM_DIR"
  thermal "$OUT/$tag-mdstorm.thermal0"
  local t0 t1; t0=$(now)
  env "$(arm_env "$1")" SQZ_BIN="$bin" SQZ_MDSTORM_DIR="$MDSTORM_DIR" bash "$MDSTORM" leg --tag="$tag" > "$OUT/$tag-mdstorm.txt" 2>&1 \
    || { echo "mdstorm leg FAILED ($tag):"; tail -10 "$OUT/$tag-mdstorm.txt"; return 1; }
  t1=$(now)
  thermal "$OUT/$tag-mdstorm.thermal1"
  cp "$MDSTORM_DIR/row_$tag.txt" "$OUT/$tag-mdstorm.row" 2>/dev/null
  cp "$MDSTORM_DIR/stats_${tag}_pre.json" "$OUT/$tag-mdstorm.pre.json" 2>/dev/null
  cp "$MDSTORM_DIR/stats_${tag}_post.json" "$OUT/$tag-mdstorm.post.json" 2>/dev/null
  cp "$MDSTORM_DIR/mount.log" "$OUT/$tag-mdstorm.daemon.log" 2>/dev/null
  echo "   leg wall $(dsec "$t0" "$t1") s: $(grep -E 'leg .*\(' "$OUT/$tag-mdstorm.txt" | tail -1)"
  sed 's/^/   /' "$OUT/$tag-mdstorm.row"
  echo "   thermal: $(grep -h hwmon "$OUT/$tag-mdstorm.thermal0" | sort -t= -k2 -n | tail -1) -> $(grep -h hwmon "$OUT/$tag-mdstorm.thermal1" | sort -t= -k2 -n | tail -1)"
  [ -s "$OUT/$tag-mdstorm.post.json" ] && { gate_reads "$OUT/$tag-mdstorm.pre.json" "$OUT/$tag-mdstorm.post.json" "$tag-mdstorm" || GATE_FAILED=1; }
  rm -rf "$MDSTORM_DIR"
}

GATE_FAILED=0
i=0
for arm in $SEQ; do
  i=$((i+1)); tag="${arm}${i}"
  echo "##### arm $arm (position $i, tag $tag) $(date -u +%FT%TZ)"
  # NORESET=1 (or a rows list without a fabric row) skips the cluster reset —
  # the mdstorm leg is the only row that needs no fabric set.
  if [ -n "${NORESET:-}" ] || [ "$ROWS" = mdstorm ]; then echo "   (no reset: rows [$ROWS])"; else reset_arm "$arm" "$tag"; fi
  for r in $ROWS; do
    case "$r" in
      mdstorm)    mdstorm_leg "$arm" "$tag" ;;
      mount)      mount_timed "$arm" "$tag" mount ;;
      wfresh-kern) fabric_daemon_up || mount_timed "$arm" "$tag" mount
                  mkdir -p "$MNT/client_validation"
                  # w_fresh: write_BW on the NEVER-written file set (no prep before it)
                  row "${tag}-wfresh-kern" "$JOBS/write_BW.job" --runtime="$RT" ;;
      rr4k-kern|rw4k-kern)
                  fabric_daemon_up || mount_timed "$arm" "$tag" mount
                  mkdir -p "$MNT/client_validation"
                  if [ -z "${PREPPED:-}" ]; then
                    # the campaign rig's prep: the rand rows' data set (write_BW's 24 × 8 GiB files)
                    echo "-- prep $tag: write_BW 40 s $(date -u +%FT%TZ)"
                    fio "$JOBS/write_BW.job" --runtime=40 --ramp_time=0 --output-format=json --output="$OUT/prep-$tag.fio.json" > /dev/null 2>&1
                    PREPPED=1
                  fi
                  case "$r" in
                    rr4k-kern) row "${tag}-rr4k-kern" "$JOBS/randread_iops.job"  --runtime="$RT" ;;
                    rw4k-kern) row "${tag}-rw4k-kern" "$JOBS/randwrite_iops.job" --runtime="$RT" ;;
                  esac ;;
      remount)    fabric_daemon_up || mount_timed "$arm" "$tag" mount
                  umount_timed "$tag"
                  mount_timed "$arm" "$tag" remount
                  # a fresh process: absolute reads only (deltas against itself are 0)
                  gate_reads "$OUT/$tag.remount.stats" "$OUT/$tag.remount.stats" "$tag-remount" || GATE_FAILED=1 ;;
      *) echo "unknown row $r"; exit 2 ;;
    esac
  done
  unset PREPPED
  kill_daemon
done
rm -rf "$MDSTORM_DIR"
echo "== done $(date -u +%FT%TZ) $OUT gate_failed=$GATE_FAILED"
grep -h "^MOUNT\|^UMOUNT" "$OUT"/*.time 2>/dev/null
[ "$GATE_FAILED" = 0 ] || { echo "GATE FAILED — dlm_rpcs moved or a forest key violation was counted (see gate[...] lines)"; exit 3; }
