# shellcheck shell=bash
#
# cloud_bench_node_scripts.sh — the NODE-side scripts tests/cloud_bench_cluster.sh
# feeds to `remote` whose LOGIC a --dry-run transcript cannot exercise (a dry
# run PRINTS a heredoc), kept here so tests/cloud_bench_cluster_units.sh runs
# them against fake system tools (a fake `systemctl` / `fuser` / `remote` /
# `systemd-machine-id-setup`). Sourced by the rig and by the unit file —
# ONE definition, so the transcript and the pin can never diverge.
#
# The sourcing harness must define: die, warn, remote (<ip> [VAR=val …], the
# script on stdin), node_pub (<name> -> ip), DRY_RUN (true | false).

# --- deploy: Ubuntu's unattended apt off for the session ----------------------
# Bounds for the wait on a LIVE apt/dpkg transaction (env words the rig passes
# to the node script; the unit file shortens them).
# 10 min: generous against one security-upgrade transaction on a fresh cloud
# image and small against the max-spend guard's hours; past it the node is not
# in a state a row may run on and the deploy dies loud naming it — dpkg is
# NEVER killed (a mid-dpkg kill leaves a half-configured node).
APT_UPGRADE_WAIT_MAX_S="${APT_UPGRADE_WAIT_MAX_S:-600}"
APT_UPGRADE_POLL_S="${APT_UPGRADE_POLL_S:-5}"

# Env: NODE, APT_UPGRADE_WAIT_MAX_S, APT_UPGRADE_POLL_S.
#
# The law: a LIVE apt/dpkg transaction is detected by its LOCKS and by the
# oneshot units that run apt (`apt-daily.service` / `apt-daily-upgrade
# .service` read `activating` while `apt.systemd.daily` runs — `is-active`
# would never see them) — NEVER by `unattended-upgrades.service`'s state,
# which is `active` on every booted Ubuntu node (the long-lived
# `unattended-upgrade-shutdown --wait-for-signal` waiter; review round 1,
# Issue 1: the first build's `is-active` on it was always true and stalled
# every deploy for the whole bound). The drain is `systemctl stop
# unattended-upgrades.service` — its stop handler waits for a running
# `unattended-upgrade` child under the unit's own stop timeout — run FIRST;
# `apt-daily*.service` is never `stop`ped (a `stop` SIGTERMs the unit's
# cgroup: apt.systemd.daily → unattended-upgrade → dpkg); a lock held past
# the bound dies loud naming the node. On a quiet node the whole script is
# a handful of systemctl calls: seconds.
NODE_APT_HYGIENE_SCRIPT="$(cat <<'EOS'
set -euo pipefail
export DEBIAN_FRONTEND=noninteractive
# cloud-init's own user-data apt (the package install) finishes first — the
# same bounded wait the package verify takes; absent on a non-cloud host.
command -v cloud-init >/dev/null 2>&1 && { cloud-init status --wait --long >/dev/null 2>&1 || true; }
apt_busy() { # 0 while an apt/dpkg transaction may be live
  # the LOCKS: dpkg's frontend + backend, apt's lists + archives
  if command -v fuser >/dev/null 2>&1; then
    fuser -s /var/lib/dpkg/lock-frontend /var/lib/dpkg/lock /var/lib/apt/lists/lock /var/cache/apt/archives/lock 2>/dev/null && return 0
  elif command -v lslocks >/dev/null 2>&1; then
    lslocks -n -o PATH 2>/dev/null | grep -qE '^/var/lib/dpkg/lock|^/var/lib/apt/lists/lock|^/var/cache/apt/archives/lock' && return 0
  fi
  # the oneshot units that run apt: in flight they read `activating`
  local u s
  for u in apt-daily.service apt-daily-upgrade.service; do
    s="$(systemctl show -p ActiveState --value "$u" 2>/dev/null || true)"
    case "$s" in activating|active|reloading|deactivating) return 0 ;; esac
  done
  # the upgrader process itself (comm is truncated to 15 chars)
  pgrep -x unattended-upgr >/dev/null 2>&1 && return 0
  return 1
}
# 1. the graceful drain: waits for a running unattended-upgrade to finish
systemctl stop unattended-upgrades.service 2>/dev/null || true
# 2. nothing re-fires for the session: the timers off, the upgrader masked
systemctl stop apt-daily.timer apt-daily-upgrade.timer 2>/dev/null || true
systemctl disable apt-daily.timer apt-daily-upgrade.timer 2>/dev/null || true
systemctl mask apt-daily.timer apt-daily-upgrade.timer unattended-upgrades.service 2>/dev/null || true
# 3. wait ONLY while a transaction is live (apt-daily.service's update half,
#    a straggler) — bounded, then die loud; dpkg is never killed
deadline=$((SECONDS + APT_UPGRADE_WAIT_MAX_S))
said=0
while apt_busy; do
  if [ "$SECONDS" -ge "$deadline" ]; then
    echo "FATAL[$NODE]: an apt/dpkg transaction is still live after ${APT_UPGRADE_WAIT_MAX_S}s — NOT killed (a mid-dpkg kill leaves a half-configured node); let it finish, then re-run deploy" >&2
    exit 1
  fi
  [ "$said" = 1 ] || echo "an apt/dpkg transaction is live on $NODE — waiting for it (bounded ${APT_UPGRADE_WAIT_MAX_S}s; never killed)"
  said=1
  sleep "$APT_UPGRADE_POLL_S"
done
echo "apt hygiene on $NODE: unattended-upgrades drained+masked, apt-daily*.timer stopped+disabled+masked, no apt/dpkg transaction live"
EOS
)"

# --- assemble-sym: /etc/machine-id per client node --------------------------
# Env: NODE, REGEN (0 | 1); MID_ETC / MID_DBUS override the two paths (the unit
# file's fake root — the defaults are the real files).
NODE_MACHINE_ID_SCRIPT="$(cat <<'EOS'
set -euo pipefail
ETC="${MID_ETC:-/etc/machine-id}"
DBUS="${MID_DBUS:-/var/lib/dbus/machine-id}"
if [ "$REGEN" = 1 ]; then
  : >"$ETC"
  systemd-machine-id-setup
  if [ -f "$DBUS" ] && [ ! -L "$DBUS" ]; then
    cp "$ETC" "$DBUS"
  fi
fi
[ -s "$ETC" ] || { echo "FATAL[$NODE]: $ETC is missing or empty" >&2; exit 1; }
echo "MACHINE_ID $(tr -d '[:space:]' <"$ETC")"
EOS
)"

# sym_assert_machine_ids <client node name…> — every client node's
# /etc/machine-id read and asserted DISTINCT across the fleet; a duplicate (a
# baked AMI's clone) is regenerated ONCE on the later node, re-read and
# re-asserted; a duplicate STILL standing dies loud. The daemon's node token
# — half of the KD-MW-2 `(node_token, mount_slot)` identity — is derived from
# the file (src/writer_scope.rs), so two nodes with one id alias as one
# writer. Runs after the prologue (no daemon is up) and before any
# identity-bearing step. The dry run sends REGEN=0 per node and fabricates
# distinct ids (the transcript shows the step; the regeneration arm is the
# unit file's).
sym_assert_machine_ids() {
  local -A seen_mid=()
  local c ip mid out try
  for c in "$@"; do
    ip="$(node_pub "$c")"
    mid=""
    if $DRY_RUN; then
      remote "$ip" NODE="$c" REGEN=0 <<<"$NODE_MACHINE_ID_SCRIPT"
      mid="dryrun-machine-id-$c"
    else
      for try in 1 2; do
        out="$(remote "$ip" NODE="$c" REGEN="$([ "$try" = 2 ] && echo 1 || echo 0)" <<<"$NODE_MACHINE_ID_SCRIPT")"
        mid="$(awk '/^MACHINE_ID /{print $2}' <<<"$out")"
        [[ "$mid" =~ ^[0-9a-f]{32}$ ]] || die "$c: could not read a well-formed /etc/machine-id (got '${mid:-nothing}')"
        if [ -n "${seen_mid[$mid]:-}" ]; then
          [ "$try" = 1 ] || die "$c: /etc/machine-id $mid duplicates ${seen_mid[$mid]}'s — STILL after regeneration (the daemon's node token would alias; every joiner would carry the manager's identity)"
          warn "$c: /etc/machine-id $mid duplicates ${seen_mid[$mid]}'s (a baked AMI's clone); regenerating with systemd-machine-id-setup"
          continue
        fi
        break
      done
    fi
    seen_mid[$mid]="$c"
    echo "  $c: machine-id $mid"
  done
}
