#!/usr/bin/env bash
# tests/perf_remote.sh — remote perf capture → local analysis round trip.
#
# WHY: on the field client, `perf record` works but analysis appears to do
# nothing: the TUI dies over non-tty ssh, and dwarf unwinding at report time
# crawls on a saturated box. Capture remotely, analyze locally. Two artifacts
# make the capture portable:
#   * `perf archive` — bundles every DSO the capture touched, indexed by
#     build-id (includes the running squeezefs binary, whose release profile
#     keeps debug symbols), so the local perf resolves userspace symbols with
#     zero path assumptions;
#   * a /proc/kallsyms snapshot — the field kernel (ELRepo kernel-ml) ships no
#     debuginfo package, so kernel symbols ride the snapshot via --kallsyms.
# Round trip validated 2026-08-01: perf 7.1.5 on both ends, identical
# symbolization local vs on-box.
#
# usage:
#   tests/perf_remote.sh capture <host> <name> [perf-record-args...]
#       default record args: -a -g --call-graph dwarf -F 199 -- sleep 30
#   tests/perf_remote.sh fetch <host> <name> [dest-dir]
#       dest default: ./perf_captures/<name>; unpacks DSOs into ~/.debug and
#       prints the local report command.
#
# example (profile the daemon for 30 s during a fio row, then analyze here):
#   tests/perf_remote.sh capture squeeze-test rccread -p "$(ssh squeeze-test pidof squeezefs)" \
#       -g --call-graph dwarf -F 199 -- sleep 30
#   tests/perf_remote.sh fetch squeeze-test rccread
#   perf report -i perf_captures/rccread/rccread.data \
#       --kallsyms=perf_captures/rccread/kallsyms.rccread --stdio
set -euo pipefail

usage() {
    sed -n '3,28p' "$0" | sed 's/^# \{0,1\}//'
    exit 1
}

[ $# -ge 3 ] || usage
verb=$1
host=$2
name=$3
shift 3
case "$name" in
*[!A-Za-z0-9._-]*) echo "FATAL: capture name must be [A-Za-z0-9._-]" >&2 && exit 1 ;;
esac

case "$verb" in
capture)
    if [ $# -eq 0 ]; then
        set -- -a -g --call-graph dwarf -F 199 -- sleep 30
    fi
    # shellcheck disable=SC2029 # remote-side expansion of the args is intended
    ssh "$host" "set -e; cd /tmp; perf record -o /tmp/${name}.data $* ;
        if ! perf archive /tmp/${name}.data >/dev/null 2>&1; then
            echo 'WARN: perf archive found no build-ids (empty/tiny capture?) — continuing without DSO bundle' >&2;
            tar cjf /tmp/${name}.data.tar.bz2 --files-from /dev/null;
        fi;
        cp /proc/kallsyms /tmp/kallsyms.${name};
        ls -la /tmp/${name}.data /tmp/${name}.data.tar.bz2 /tmp/kallsyms.${name}"
    echo "capture '${name}' complete on ${host} — fetch with: $0 fetch ${host} ${name}"
    ;;
fetch)
    dest=${1:-./perf_captures/${name}}
    mkdir -p "$dest" "$HOME/.debug"
    scp -q "${host}:/tmp/${name}.data" "${host}:/tmp/${name}.data.tar.bz2" \
        "${host}:/tmp/kallsyms.${name}" "$dest/"
    tar xjf "${dest}/${name}.data.tar.bz2" -C "$HOME/.debug"
    echo "DSOs unpacked into ~/.debug (build-id cache). Analyze with:"
    echo "  perf report -i ${dest}/${name}.data --kallsyms=${dest}/kallsyms.${name} [--stdio]"
    echo "  perf script -i ${dest}/${name}.data --kallsyms=${dest}/kallsyms.${name}   # flamegraphs etc."
    ;;
*)
    usage
    ;;
esac
