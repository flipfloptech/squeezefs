# Shared helpers for the bracket harness (sourced by bracket.sh and io500.sh).
# Needs: $BRK (brk.py), $SCRATCH, $BUDGET_MB, log().

# The block device name under a mounted path (`vdc` for /build on the venue).
backing_disk_of() {
  local src
  src=$(findmnt -no SOURCE --target "$1" 2>/dev/null | head -1)
  src=${src%%[*}
  local name
  name=$(basename "$src")
  # A partition → its disk (vdc1 → vdc, nvme0n1p1 → nvme0n1).
  if [ -e "/sys/class/block/$name/partition" ]; then
    name=$(basename "$(readlink -f "/sys/class/block/$name/..")")
  fi
  printf '%s' "$name"
}

# The diskstats name of a device node path (/dev/nvme1n1 → nvme1n1; a regular
# file → the disk of the filesystem holding it).
disk_name_of() {
  local p=$1
  if [ -b "$p" ]; then basename "$(readlink -f "$p")"; else backing_disk_of "$(dirname "$p")"; fi
}

LEG_DAEMON_PID=""
LEG_MNT=""

# Mount with the leg's binary as a daemon; wait for `ready:` in its log.
leg_mount() {   # bin meta_uri mnt log opts
  local bin=$1 uri=$2 mnt=$3 dlog=$4 opts=$5
  local argv=("$bin" mount --daemon --log-file "$dlog")
  [ -n "$opts" ] && argv+=(-o "$opts")
  argv+=("$uri" "$mnt")
  : > "$dlog"
  SQUEEZEFS_MEM_BUDGET_MB=$BUDGET_MB "${argv[@]}" > "$dlog.spawn" 2>&1 || { cat "$dlog.spawn" "$dlog" >&2; log "mount failed"; exit 1; }
  local i
  for i in $(seq 1 600); do
    grep -q 'ready: ' "$dlog" 2>/dev/null && break
    if grep -q 'error(' "$dlog" 2>/dev/null && ! grep -q 'ready: ' "$dlog"; then
      sleep 1
      grep -q 'ready: ' "$dlog" && break
      cat "$dlog" >&2; log "mount refused"; exit 1
    fi
    sleep 0.2
  done
  grep -q 'ready: ' "$dlog" || { cat "$dlog" >&2; log "mount never became ready"; exit 1; }
  LEG_MNT=$mnt
  # The daemon's pid from .stats (mount.pid); the only process we may ever kill.
  LEG_DAEMON_PID=$(python3 - "$mnt/.stats" <<'EOF'
import json,sys
print(json.load(open(sys.argv[1]))["mount"]["pid"])
EOF
)
  log "mounted $mnt (daemon pid $LEG_DAEMON_PID)"
}

daemon_alive() {
  [ -n "$LEG_DAEMON_PID" ] && [ -d "/proc/$LEG_DAEMON_PID" ] && [ "$(awk '{print $3}' "/proc/$LEG_DAEMON_PID/stat" 2>/dev/null)" != Z ]
}

leg_umount() {
  [ -n "$LEG_MNT" ] || return 0
  local mnt=$LEG_MNT pid=$LEG_DAEMON_PID
  LEG_MNT=""; LEG_DAEMON_PID=""
  if mountpoint -q "$mnt"; then
    if [ -n "$pid" ] && ! { [ -d "/proc/$pid" ] && [ "$(awk '{print $3}' "/proc/$pid/stat" 2>/dev/null)" != Z ]; }; then
      # A dead daemon leaves a FUSE mount that answers ENOTCONN: detach it.
      log "daemon $pid is dead under $mnt; lazy-detaching the stale mount"
      umount -l "$mnt" 2>/dev/null || true
    else
      "$BRK_BIN" umount "$mnt" > "$SCRATCH/umount.log" 2>&1 || { log "umount verb failed: $(tail -2 "$SCRATCH/umount.log")"; umount -l "$mnt" 2>/dev/null || true; }
    fi
  fi
  local i
  for i in $(seq 1 600); do
    [ -n "$pid" ] && [ -d "/proc/$pid" ] && [ "$(awk '{print $3}' "/proc/$pid/stat" 2>/dev/null)" != Z ] || return 0
    sleep 0.2
  done
  log "daemon $pid still alive 120 s after umount; SIGKILL (our own child)"
  kill -9 "$pid" 2>/dev/null || true
}

# `sync` the mount, then wait for the reclaim ledger to quiesce (the reclaimer
# is asynchronous; a snapshot mid-drain reads an open identity that closes a
# moment later) — bounded.
settle() {
  local mnt=$1 i prev="" cur
  sync -f "$mnt" 2>/dev/null || sync
  for i in $(seq 1 25); do
    cur=$(python3 - "$mnt/.stats" <<'EOF'
import json,sys
sys.path.insert(0, __import__("os").environ["BRK_HERE"])
import brk
t = brk.terms(json.load(open(sys.argv[1])))
print(t["terminal_frees"], t["block_free_reclaim_queued"], t["block_free_reclaim_elided"], t["meta_kv_journal_entries"])
EOF
)
    if [ "$cur" = "$prev" ]; then return 0; fi
    prev=$cur
    sleep 0.4
  done
}

# `.stats` + /proc/diskstats + the time, as a named pair of files.
snapshot() {   # mnt prefix
  cat "$1/.stats" > "$2.stats.json"
  cat /proc/diskstats > "$2.diskstats"
  date -u +%s%N > "$2.ns"
}

# Run the instrument under a wall bound of `2 × span + 120 s`. An instrument
# that outlives it is sitting on FUSE requests the daemon never answers (the
# daemon's watchdog logs them overdue; the harness saw three such wedges in
# the commit conveyor on its first day): capture `.stats`, the daemon's and
# the instrument's kernel stacks beside the leg, leave a `wedged` marker for
# the record, then kill the daemon (our own child — the instrument's calls
# return ENOTCONN and it exits). The leg is INVALID `daemon_wedged`.
bounded_run() {   # span-secs outprefix cmd…
  local secs=$1 out=$2; shift 2
  local bound=$(( secs * 2 + 120 )) waited=0 cpid
  "$@" &
  cpid=$!
  while kill -0 "$cpid" 2>/dev/null; do
    sleep 2
    waited=$((waited + 2))
    if [ "$waited" -ge "$bound" ]; then
      capture_wedge "$out" "$waited"
      log "WEDGE: the instrument outlived its ${bound}s bound; daemon $LEG_DAEMON_PID killed, evidence beside $out"
      kill -9 "$LEG_DAEMON_PID" 2>/dev/null || true
      sleep 2
      kill -TERM "$cpid" 2>/dev/null || true
      wait "$cpid" 2>/dev/null || true
      return 75
    fi
  done
  wait "$cpid"
}

capture_wedge() {   # outprefix waited-secs
  local out=$1 waited=$2 pid=$LEG_DAEMON_PID t p
  timeout 10 cat "$LEG_MNT/.stats" > "$out.wedge.stats.json" 2>/dev/null || true
  { for t in /proc/"$pid"/task/*; do echo "== $(cat "$t/comm" 2>/dev/null) $(basename "$t") $(cat "$t/wchan" 2>/dev/null)"; head -8 "$t/stack" 2>/dev/null; done; } > "$out.wedge.daemon-stacks.txt" 2>/dev/null || true
  { for p in $(pgrep -x fio; pgrep -x mdtest; pgrep -x tar; pgrep -f renames.py); do echo "== $(cat /proc/"$p"/comm 2>/dev/null) $p $(cat /proc/"$p"/wchan 2>/dev/null)"; head -6 /proc/"$p"/stack 2>/dev/null; done; } > "$out.wedge.instrument-stacks.txt" 2>/dev/null || true
  local overdue inflight queued parked
  overdue=$(python3 "$BRK" get "$out.wedge.stats.json" fuse_op_watchdog_overdue 2>/dev/null || echo "?")
  inflight=$(python3 "$BRK" get "$out.wedge.stats.json" fuse_ops_inflight 2>/dev/null || echo "?")
  queued=$(python3 "$BRK" get "$out.wedge.stats.json" meta_conveyor_queued 2>/dev/null || echo "?")
  parked=$(python3 "$BRK" get "$out.wedge.stats.json" meta_commit_parked 2>/dev/null || echo "?")
  printf 'instrument outlived its bound after %s s; fuse_ops_inflight %s, fuse_op_watchdog_overdue %s, meta_conveyor_queued %s, meta_commit_parked %s; daemon pid %s killed by the harness; evidence: %s.wedge.*\n' \
    "$waited" "$inflight" "$overdue" "$queued" "$parked" "$pid" "$(basename "$out")" > "$(dirname "$out")/wedged"
}

# Run a command with the leg's LD_PRELOAD environment (the shim path) when the
# row asked for it, else plainly.
with_preload() {
  if [ -n "${BRK_PRELOAD_ENV:-}" ]; then env $BRK_PRELOAD_ENV "$@"; else "$@"; fi
}

export BRK_HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
