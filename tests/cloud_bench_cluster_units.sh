#!/usr/bin/env bash
#
# cloud_bench_cluster_units.sh — the shell pin for the cloud rig's NODE-side
# scripts (tests/cloud_bench_node_scripts.sh): the logic a --dry-run
# transcript cannot exercise, run here against FAKE system tools on PATH
# (`systemctl`, `fuser`, `pgrep`, `sleep`, `systemd-machine-id-setup`) and a
# fake `remote`. No cargo, no root, no network, no aws; seconds.
#
#   bash tests/cloud_bench_cluster_units.sh          # every case; exit 0 = green
#
# Cases (each red-first against the shape it names — the 2026-09-24 cloud
# row's review, round 1, Issues 1 / 2 / 7):
#   apt-1  a QUIET node whose `unattended-upgrades.service` is `active` (the
#          `--wait-for-signal` waiter — ALWAYS active on Ubuntu) must not be
#          waited on: zero polls, and the drain is `systemctl stop
#          unattended-upgrades.service` FIRST (its stop handler waits for a
#          running child), then the timers off + masked.
#   apt-2  the LOCK arm alone: the dpkg lock held for three polls (every unit
#          inactive, no upgrader process) IS waited for — exactly until the
#          lock frees — and `apt-daily-upgrade.service` is NEVER `stop`ped
#          (a `stop` SIGTERMs the unit's cgroup: apt.systemd.daily →
#          unattended-upgrade → dpkg). Runs under a SHORT bound so a broken
#          lock arm fails in seconds, never a real 600 s wait.
#   apt-3  a lock held past the bound dies LOUD, nonzero, naming the node —
#          never a `stop`/`kill` of the transaction.
#   apt-4  the UNIT-STATE arm alone: `apt-daily-upgrade.service` reads
#          `activating` for three probes with the lock never held and no
#          upgrader process — exactly three polls, no `stop`.
#   apt-5  the PROCESS arm alone: the upgrader `unattended-upgrade` is in the
#          process table for three probes, no lock, every unit inactive —
#          exactly three polls.
# Mutation law (state it in the commit that touches `apt_busy`): deleting the
# lock arm → apt-2 + apt-3 RED; the unit-state arm → apt-4 RED; the process
# arm → apt-5 RED. `CLOUD_NODE_SCRIPTS_LIB=<path>` points the pin at a
# mutant copy of the lib.
#   mid-1  REGEN=1 with a REGULAR-FILE dbus id carrying the clone yields a NEW
#          id (systemd-machine-id-setup seeds from that file first, so it must
#          go BEFORE the setup) and the dbus file is recreated with the new id.
#   mid-2  REGEN=1 with the Debian SYMLINK dbus id yields a new id and keeps
#          the symlink.
#   mid-3  `sym_assert_machine_ids`: a try-1 duplicate is regenerated (REGEN=1
#          on the LATER node) and a distinct try-2 id passes.
#   mid-4  `sym_assert_machine_ids`: a duplicate STILL standing on try 2 dies.
set -euo pipefail

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
WORK="$(mktemp -d /tmp/sqz-cloud-units.XXXXXX)"
trap 'rm -rf "$WORK"' EXIT
FAKES="$WORK/bin"
mkdir -p "$FAKES"

# The harness words the lib expects.
die() { echo "FATAL: $*" >&2; exit 99; }
warn() { echo "WARN: $*" >&2; }
node_pub() { echo "ip-$1"; }
DRY_RUN=false
# shellcheck source=tests/cloud_bench_node_scripts.sh
. "${CLOUD_NODE_SCRIPTS_LIB:-$HERE/cloud_bench_node_scripts.sh}"

PASS=0
FAIL=0
ok() { PASS=$((PASS + 1)); echo "  ok   $*"; }
bad() { FAIL=$((FAIL + 1)); echo "  FAIL $*"; }
assert_eq() { # <label> <want> <got>
  if [ "$2" = "$3" ]; then ok "$1 = $2"; else bad "$1: want '$2' got '$3'"; fi
}
assert_grep() { # <label> <pattern> <file>
  if grep -qE -- "$2" "$3"; then ok "$1"; else bad "$1 (no match for '$2' in $(basename "$3"))"; fi
}
assert_no_grep() { # <label> <pattern> <file>
  if grep -qE -- "$2" "$3"; then bad "$1 (found '$2' in $(basename "$3"))"; else ok "$1"; fi
}

# --- the fake system tools ---------------------------------------------------
# Every fake logs its argv to $FAKE_LOG (one line per call) and answers off a
# per-case state dir ($FAKE_STATE): `units` (unit -> ActiveState, one per
# line), `lock_held_polls` (how many more `fuser` probes read the lock as
# HELD; decremented per probe), `activating_polls` (how many more `show`
# queries of a unit tabled `activating` read it so — INDEPENDENT of the lock,
# so the unit-state arm is pinned on its own), `procs` (the process table —
# lines `<pid> <comm> <cmdline>` — and `upgrader_polls`, how many more
# `pgrep` calls still see the `unattended-upgrade` upgrader in it).
cat >"$FAKES/systemctl" <<'EOF'
#!/usr/bin/env bash
echo "systemctl $*" >>"$FAKE_LOG"
state_of() { awk -v u="$1" '$1 == u {print $2}' "$FAKE_STATE/units"; }
case "$1" in
  show)
    # `show -p ActiveState --value <unit>`: the real word for one unit. A
    # unit tabled `activating` is the LIVE upgrade — it reads `activating`
    # for `activating_polls` more queries (its own countdown, never the
    # lock's) and `inactive` after, as the real oneshot does when its run
    # ends.
    u="${@: -1}"; s="$(state_of "$u")"
    if [ "$s" = activating ]; then
      n="$(cat "$FAKE_STATE/activating_polls" 2>/dev/null || echo 0)"
      if [ "$n" -gt 0 ]; then echo $((n - 1)) >"$FAKE_STATE/activating_polls"; else s=inactive; fi
    fi
    echo "${s:-inactive}"; exit 0 ;;
  is-active)
    # the real systemctl: 0 if AT LEAST ONE named unit is active/reloading
    shift; [ "${1:-}" = "--quiet" ] && shift
    for u in "$@"; do
      case "$(state_of "$u")" in active|reloading) exit 0 ;; esac
    done
    exit 3 ;;
  stop|start|disable|enable|mask|unmask) exit 0 ;;
  *) exit 0 ;;
esac
EOF
cat >"$FAKES/fuser" <<'EOF'
#!/usr/bin/env bash
echo "fuser $*" >>"$FAKE_LOG"
n="$(cat "$FAKE_STATE/lock_held_polls" 2>/dev/null || echo 0)"
if [ "$n" -gt 0 ]; then echo $((n - 1)) >"$FAKE_STATE/lock_held_polls"; exit 0; fi
exit 1
EOF
# pgrep over the fake process table: `-x <comm>` matches the 15-char comm
# exactly, `-f <regex>` the full command line (grep -E). The upgrader's line
# is present while `upgrader_polls` > 0 (decremented per pgrep call); the
# waiter's line, when tabled, is always present (it never exits on its own).
cat >"$FAKES/pgrep" <<'EOF'
#!/usr/bin/env bash
echo "pgrep $*" >>"$FAKE_LOG"
mode="$1"; pat="$2"
n="$(cat "$FAKE_STATE/upgrader_polls" 2>/dev/null || echo 0)"
table="$(cat "$FAKE_STATE/procs" 2>/dev/null || true)"
if [ "$n" -gt 0 ]; then
  echo $((n - 1)) >"$FAKE_STATE/upgrader_polls"
  table="$table
4242 unattended-upgr /usr/bin/python3 /usr/bin/unattended-upgrade"
fi
[ -n "$table" ] || exit 1
case "$mode" in
  -x) awk -v c="$pat" '$2 == c {found = 1} END {exit found ? 0 : 1}' <<<"$table" ;;
  -f) cut -d' ' -f3- <<<"$table" | grep -qE -- "$pat" ;;
  *) exit 1 ;;
esac
EOF
cat >"$FAKES/sleep" <<'EOF'
#!/usr/bin/env bash
echo "sleep $*" >>"$FAKE_LOG"
exit 0
EOF
# systemd-machine-id-setup as documented: a REGULAR-FILE dbus id is the first
# source (a symlink is skipped); otherwise a fresh id (here: random).
cat >"$FAKES/systemd-machine-id-setup" <<'EOF'
#!/usr/bin/env bash
echo "systemd-machine-id-setup $*" >>"$FAKE_LOG"
if [ -f "$MID_DBUS" ] && [ ! -L "$MID_DBUS" ] && [ -s "$MID_DBUS" ]; then
  tr -d '[:space:]' <"$MID_DBUS" >"$MID_ETC"; echo >>"$MID_ETC"
else
  head -c 16 /dev/urandom | od -An -tx1 | tr -d ' \n' >"$MID_ETC"; echo >>"$MID_ETC"
fi
EOF
chmod +x "$FAKES"/*

new_case() { # <name> — a fresh state dir + log; prints the case dir
  local d="$WORK/$1"
  mkdir -p "$d"
  : >"$d/log"
  : >"$d/units"
  : >"$d/procs"
  echo 0 >"$d/lock_held_polls"
  echo 0 >"$d/activating_polls"
  echo 0 >"$d/upgrader_polls"
  echo "$d"
}
# Every apt case runs under a SHORT bound (10 s; the fakes' polls are
# instant, so a healthy arm finishes in ms) — a regressed arm fails FAST
# instead of spinning the production 600 s bound against the real clock.
APT_PIN_WAIT_MAX_S=10
run_apt() { # <casedir> [WAIT_MAX] [POLL] — the node script under the fakes
  local d="$1" wait_max="${2:-$APT_PIN_WAIT_MAX_S}" poll="${3:-5}"
  PATH="$FAKES:$PATH" FAKE_LOG="$d/log" FAKE_STATE="$d" \
    NODE=unit-node APT_UPGRADE_WAIT_MAX_S="$wait_max" APT_UPGRADE_POLL_S="$poll" \
    bash -c "$NODE_APT_HYGIENE_SCRIPT" >"$d/out" 2>"$d/err"
}

echo "== apt-1: the always-active waiter on a quiet node is not waited on; the drain comes first"
d="$(new_case apt1)"
printf '%s\n' "unattended-upgrades.service active" "apt-daily-upgrade.service inactive" "apt-daily.service inactive" >"$d/units"
rc=0; run_apt "$d" || rc=$?
assert_eq "apt-1 exit" 0 "$rc"
assert_eq "apt-1 polls (sleep calls) on a quiet node" 0 "$(grep -c '^sleep ' "$d/log" || true)"
assert_no_grep "apt-1 never stops apt-daily-upgrade.service" '^systemctl stop .*apt-daily-upgrade\.service' "$d/log"
first_stop="$(grep -m1 '^systemctl stop' "$d/log" || true)"
assert_eq "apt-1 the graceful drain is the FIRST stop" "systemctl stop unattended-upgrades.service" "$first_stop"
assert_grep "apt-1 timers stopped" '^systemctl stop apt-daily\.timer apt-daily-upgrade\.timer' "$d/log"
assert_grep "apt-1 timers disabled" '^systemctl disable apt-daily\.timer apt-daily-upgrade\.timer' "$d/log"
assert_grep "apt-1 the upgrader masked" '^systemctl mask .*unattended-upgrades\.service' "$d/log"

echo "== apt-2: the LOCK arm alone — the dpkg lock held for 3 polls (every unit inactive, no upgrader) is waited for exactly, and never stopped"
d="$(new_case apt2)"
printf '%s\n' "unattended-upgrades.service active" "apt-daily-upgrade.service inactive" "apt-daily.service inactive" >"$d/units"
echo 3 >"$d/lock_held_polls"
rc=0; run_apt "$d" || rc=$?
assert_eq "apt-2 exit" 0 "$rc"
assert_eq "apt-2 polls = the 3 held probes" 3 "$(grep -c '^sleep ' "$d/log" || true)"
assert_eq "apt-2 the lock was probed (fuser) on every pass" 4 "$(grep -c '^fuser ' "$d/log" || true)"
assert_no_grep "apt-2 never stops apt-daily-upgrade.service while/after the lock" '^systemctl stop .*apt-daily-upgrade\.service' "$d/log"
assert_grep "apt-2 says it is waiting" 'waiting' "$d/out"

echo "== apt-3: a lock held past the bound dies loud, nonzero, without killing the transaction"
d="$(new_case apt3)"
printf '%s\n' "unattended-upgrades.service active" "apt-daily-upgrade.service activating" >"$d/units"
echo 1000000 >"$d/lock_held_polls"
rc=0; run_apt "$d" 0 0 || rc=$?
if [ "$rc" -ne 0 ]; then ok "apt-3 exit nonzero ($rc)"; else bad "apt-3 exited 0 with the lock still held"; fi
assert_grep "apt-3 names the node in its FATAL" 'FATAL\[unit-node\]' "$d/err"
assert_no_grep "apt-3 never stops apt-daily-upgrade.service" '^systemctl stop .*apt-daily-upgrade\.service' "$d/log"
assert_no_grep "apt-3 never kills" '^(systemctl kill|kill|pkill) ' "$d/log"

echo "== apt-4: the UNIT-STATE arm alone — apt-daily-upgrade.service reads activating for 3 probes, the lock never held, no upgrader process"
d="$(new_case apt4)"
printf '%s\n' "unattended-upgrades.service active" "apt-daily-upgrade.service activating" "apt-daily.service inactive" >"$d/units"
echo 3 >"$d/activating_polls"
rc=0; run_apt "$d" || rc=$?
assert_eq "apt-4 exit" 0 "$rc"
assert_eq "apt-4 polls = the 3 activating probes" 3 "$(grep -c '^sleep ' "$d/log" || true)"
assert_grep "apt-4 the unit state was consulted" '^systemctl show -p ActiveState --value apt-daily-upgrade\.service' "$d/log"
assert_no_grep "apt-4 never stops apt-daily-upgrade.service" '^systemctl stop .*apt-daily-upgrade\.service' "$d/log"

echo "== apt-5: the PROCESS arm alone — the upgrader unattended-upgrade is in the process table for 3 probes, no lock, every unit inactive"
d="$(new_case apt5)"
printf '%s\n' "unattended-upgrades.service active" "apt-daily-upgrade.service inactive" "apt-daily.service inactive" >"$d/units"
echo 3 >"$d/upgrader_polls"
rc=0; run_apt "$d" || rc=$?
assert_eq "apt-5 exit" 0 "$rc"
assert_eq "apt-5 polls = the 3 probes that saw the upgrader" 3 "$(grep -c '^sleep ' "$d/log" || true)"
assert_grep "apt-5 the process table was consulted" '^pgrep ' "$d/log"
assert_no_grep "apt-5 never stops apt-daily-upgrade.service" '^systemctl stop .*apt-daily-upgrade\.service' "$d/log"

# --- the machine-id node script --------------------------------------------------
run_mid() { # <casedir> <REGEN> — the node script under the fakes with MID_* in the case dir
  local d="$1" regen="$2"
  PATH="$FAKES:$PATH" FAKE_LOG="$d/log" FAKE_STATE="$d" \
    MID_ETC="$d/etc-machine-id" MID_DBUS="$d/dbus/machine-id" NODE=unit-node REGEN="$regen" \
    bash -c "$NODE_MACHINE_ID_SCRIPT" >"$d/out" 2>"$d/err"
}
CLONE="0123456789abcdef0123456789abcdef"

echo "== mid-1: REGEN=1 with a REGULAR-FILE dbus id carrying the clone yields a NEW id and recreates the dbus copy"
d="$(new_case mid1)"; mkdir -p "$d/dbus"
echo "$CLONE" >"$d/etc-machine-id"; echo "$CLONE" >"$d/dbus/machine-id"
rc=0; run_mid "$d" 1 || rc=$?
assert_eq "mid-1 exit" 0 "$rc"
got="$(awk '/^MACHINE_ID /{print $2}' "$d/out")"
if [[ "$got" =~ ^[0-9a-f]{32}$ ]] && [ "$got" != "$CLONE" ]; then ok "mid-1 a new well-formed id ($got)"; else bad "mid-1 id after regeneration is '$got' (the clone was $CLONE)"; fi
if [ -f "$d/dbus/machine-id" ] && [ ! -L "$d/dbus/machine-id" ]; then ok "mid-1 the dbus copy is a regular file again"; else bad "mid-1 the dbus copy was not recreated as a regular file"; fi
assert_eq "mid-1 the dbus copy carries the NEW id" "$got" "$(tr -d '[:space:]' <"$d/dbus/machine-id" 2>/dev/null || true)"

echo "== mid-2: REGEN=1 with the Debian SYMLINK dbus id yields a new id and keeps the symlink"
d="$(new_case mid2)"; mkdir -p "$d/dbus"
echo "$CLONE" >"$d/etc-machine-id"; ln -s "$d/etc-machine-id" "$d/dbus/machine-id"
rc=0; run_mid "$d" 1 || rc=$?
assert_eq "mid-2 exit" 0 "$rc"
got="$(awk '/^MACHINE_ID /{print $2}' "$d/out")"
if [[ "$got" =~ ^[0-9a-f]{32}$ ]] && [ "$got" != "$CLONE" ]; then ok "mid-2 a new well-formed id"; else bad "mid-2 id after regeneration is '$got'"; fi
if [ -L "$d/dbus/machine-id" ]; then ok "mid-2 the symlink stays"; else bad "mid-2 the symlink was replaced"; fi

echo "== mid-0: REGEN=0 reads the id verbatim and touches nothing"
d="$(new_case mid0)"; mkdir -p "$d/dbus"
echo "$CLONE" >"$d/etc-machine-id"
rc=0; run_mid "$d" 0 || rc=$?
assert_eq "mid-0 exit" 0 "$rc"
assert_eq "mid-0 id verbatim" "$CLONE" "$(awk '/^MACHINE_ID /{print $2}' "$d/out")"
assert_eq "mid-0 no setup call" 0 "$(grep -c 'systemd-machine-id-setup' "$d/log" || true)"

# --- sym_assert_machine_ids under a fake `remote` ---------------------------------
# The fake answers canned ids per (node, REGEN) from $REMOTE_TABLE — the
# lines `node REGEN id` — and logs each call.
remote() { # <ip> [VAR=val …]  (the script on stdin is consumed and ignored)
  local ip="$1"; shift
  local node="" regen="0" kv
  for kv in "$@"; do
    case "$kv" in NODE=*) node="${kv#NODE=}" ;; REGEN=*) regen="${kv#REGEN=}" ;; esac
  done
  cat >/dev/null
  echo "remote $ip NODE=$node REGEN=$regen" >>"$REMOTE_LOG"
  echo "MACHINE_ID $(awk -v n="$node" -v r="$regen" '$1 == n && $2 == r {print $3}' "$REMOTE_TABLE")"
}
A="aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"; B="bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb"

echo "== mid-3: a try-1 duplicate is regenerated on the LATER node and a distinct try-2 id passes"
d="$(new_case mid3)"
REMOTE_LOG="$d/remote.log"; REMOTE_TABLE="$d/table"; : >"$REMOTE_LOG"
printf '%s\n' "client0 0 $A" "client1 0 $A" "client1 1 $B" >"$REMOTE_TABLE"
rc=0; (sym_assert_machine_ids client0 client1) >"$d/out" 2>"$d/err" || rc=$?
assert_eq "mid-3 exit" 0 "$rc"
assert_grep "mid-3 client0 read" 'client0: machine-id a{32}' "$d/out"
assert_grep "mid-3 the duplicate warned" 'WARN: client1: .*duplicates client0' "$d/err"
assert_grep "mid-3 REGEN=1 sent to the LATER node only" '^remote ip-client1 NODE=client1 REGEN=1$' "$d/remote.log"
assert_no_grep "mid-3 the first node never regenerates" '^remote ip-client0 NODE=client0 REGEN=1$' "$d/remote.log"
assert_grep "mid-3 client1's new id accepted" 'client1: machine-id b{32}' "$d/out"

echo "== mid-4: a duplicate STILL standing on try 2 dies"
d="$(new_case mid4)"
REMOTE_LOG="$d/remote.log"; REMOTE_TABLE="$d/table"; : >"$REMOTE_LOG"
printf '%s\n' "client0 0 $A" "client1 0 $A" "client1 1 $A" >"$REMOTE_TABLE"
rc=0; (sym_assert_machine_ids client0 client1) >"$d/out" 2>"$d/err" || rc=$?
assert_eq "mid-4 exit = die's 99" 99 "$rc"
assert_grep "mid-4 the FATAL names the still-standing duplicate" 'FATAL: client1: .*STILL after regeneration' "$d/err"

echo
echo "cloud_bench_cluster_units: PASS=$PASS FAIL=$FAIL"
[ "$FAIL" -eq 0 ]
