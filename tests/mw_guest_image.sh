#!/usr/bin/env bash
# tests/mw_guest_image.sh — rung-6b qemu guest image plumbing (PR 6b)
# =============================================================================
#
# Builds the BOOT PAIR the mw fleet's `--vm` guests run (design-full-
# multi-writer rung 6b; docs/design-mw-multipath-kernel.md §6 — the
# validation venue for sqz kernel patch 0030), from the sqz kernel RPMs
# the series' container venue produces (docker/kernel-sqz/build.sh):
#
#   <out>/vmlinuz           the 6.19.14-sqz bzImage (from the kernel RPM)
#   <out>/initramfs.img     busybox initramfs (gzip cpio, ~modules only)
#   <out>/kernel-release    the `uname -r` the pair boots (6.19.14-sqz)
#   <out>/modules/          the extracted /lib/modules tree (rebuild cache)
#
# Deliberately NOT a distro pipeline (the rung charter): the rootfs is a
# minimal busybox initramfs — busybox (host mkinitcpio's copy or
# SQZ_MWGUEST_BUSYBOX) + its glibc closure + the MODULE CLOSURE for
# exactly what the guest needs (virtio_net for the slirp NIC, 9p/
# 9pnet_virtio for the host share, nvme-tcp for the fabric, fuse for
# mounts) resolved transitively from modules.dep. The squeezefs binary
# is NOT baked in: it rides the 9p share (`hostshare` tag), staged fresh
# by mw_fleet.sh at every guest boot, so guests never lag the HOST-built
# binary (the KD-7 same-commit discipline's guest face).
#
# Guest control plane (mw_fleet.sh vm_exec): the init script mounts the
# 9p share at /share (cache=none — host↔guest visibility without cache
# staleness) and loops a JOB EXECUTOR: every /share/jobs/<n>.sh without
# a matching <n>.rc is run with sh, stdout+stderr to <n>.out, exit code
# to <n>.rc (rename-atomic). Shutdown is a job running `poweroff -f`.
#
# Verbs
#   build [--force]   build (or rebuild) the boot pair into --out
#   status            print what exists and the kernel release
#
# Env knobs (rig-local, scrubbed from product invocations by mw_fleet)
#   SQZ_MWGUEST_RPM_DIR   kernel RPM dir (default <repo>/dist/kernel-sqz)
#   SQZ_MWGUEST_OUT       output dir (default <repo>/target/mw-guest —
#                         a build product like target/release, cached
#                         across fleets; NOT fleet-state residue)
#   SQZ_MWGUEST_BUSYBOX   busybox binary (default /usr/lib/initcpio/busybox
#                         — the mkinitcpio copy every Arch-family dev box
#                         carries; static or dynamic both work, the ldd
#                         closure rides along)
#
# Refusals are LOUD with the remedy (the dev_substrate pattern).

set -euo pipefail

REPO="$(cd "$(dirname "$0")/.." && pwd)"
RPM_DIR="${SQZ_MWGUEST_RPM_DIR:-$REPO/dist/kernel-sqz}"
OUT="${SQZ_MWGUEST_OUT:-$REPO/target/mw-guest}"
BUSYBOX="${SQZ_MWGUEST_BUSYBOX:-/usr/lib/initcpio/busybox}"

# The module closure roots (deps resolved transitively via modules.dep):
#   virtio_net — the slirp user-net NIC;  9pnet_virtio + 9p — the host
#   share;  nvme-tcp — the fabric (pulls nvme-fabrics + nvme-core);
#   fuse — the mounts.
GUEST_MODULES=(virtio_net 9pnet_virtio 9p nvme-tcp fuse)

log() { echo "[mwguest] $*"; }
die() {
    echo "[mwguest] ERROR: $*" >&2
    exit 1
}

find_kernel_rpm() { # -> path of the RPM carrying boot/vmlinuz*
    local rpm
    for rpm in "$RPM_DIR"/kernel-*.rpm; do
        [ -f "$rpm" ] || continue
        case "$(basename "$rpm")" in
        *headers* | *devel* | *debug*) continue ;;
        esac
        # binrpm-pkg places vmlinuz under /lib/modules/<ver>/ (modern
        # scripts/package) or /boot (older) — accept either.
        if bsdtar -tf "$rpm" 2>/dev/null | grep -qE "(boot|modules/[^/]+)/vmlinuz"; then
            echo "$rpm"
            return 0
        fi
    done
    return 1
}

build_image() {
    local force=0
    [ "${1:-}" = "--force" ] && force=1
    if [ "$force" = "0" ] && [ -f "$OUT/vmlinuz" ] && [ -f "$OUT/initramfs.img" ]; then
        log "boot pair exists at $OUT ($(cat "$OUT/kernel-release" 2>/dev/null || echo '?')) — use --force to rebuild"
        return 0
    fi
    command -v bsdtar >/dev/null 2>&1 || die "bsdtar is required (RPM extraction)"
    command -v cpio >/dev/null 2>&1 || die "cpio is required (initramfs assembly)"
    [ -x "$BUSYBOX" ] || die "busybox not found at '$BUSYBOX' — install mkinitcpio (its /usr/lib/initcpio/busybox) or set SQZ_MWGUEST_BUSYBOX"
    [ -d "$RPM_DIR" ] || die "no kernel RPM dir at $RPM_DIR — build the sqz kernel first: docker/kernel-sqz/build.sh (SERIES.md manifest, patch 0030 included)"
    local rpm
    rpm="$(find_kernel_rpm)" ||
        die "no kernel RPM carrying boot/vmlinuz under $RPM_DIR — build the sqz kernel first: docker/kernel-sqz/build.sh"
    log "kernel RPM: $rpm"

    rm -rf "$OUT/extract" "$OUT/initrd"
    mkdir -p "$OUT/extract" "$OUT/initrd"

    # --- extract the RPM (vmlinuz + /lib/modules tree) -----------------------
    bsdtar -xf "$rpm" -C "$OUT/extract" || die "RPM extraction failed"
    local vmlinuz kver moddir
    vmlinuz="$(find "$OUT/extract" \( -path "*/boot/vmlinuz-*" -o -path "*/lib/modules/*/vmlinuz" \) -type f | head -1)"
    [ -n "$vmlinuz" ] || die "no vmlinuz in the RPM payload"
    moddir="$(find "$OUT/extract" -type d -path "*/lib/modules/*" -name "*-sqz" | head -1)"
    [ -n "$moddir" ] || die "no /lib/modules/<ver>-sqz tree in the RPM payload"
    kver="$(basename "$moddir")"
    log "kernel release: $kver"
    cp "$vmlinuz" "$OUT/vmlinuz"
    echo "$kver" >"$OUT/kernel-release"
    rm -rf "$OUT/modules" && mkdir -p "$OUT/modules"
    cp -a "$moddir" "$OUT/modules/$kver"

    # --- module closure from modules.dep (transitively closed per line) -----
    # The RPM ships no modules.dep (an install-time %post product) —
    # generate it against the extracted tree.
    local dep="$OUT/modules/$kver/modules.dep"
    if [ ! -f "$dep" ]; then
        command -v depmod >/dev/null 2>&1 || die "depmod is required (modules.dep generation)"
        rm -rf "$OUT/depmod" && mkdir -p "$OUT/depmod/lib/modules"
        ln -s "$OUT/modules/$kver" "$OUT/depmod/lib/modules/$kver"
        depmod -b "$OUT/depmod" "$kver" || die "depmod failed"
        rm -rf "$OUT/depmod"
    fi
    [ -f "$dep" ] || die "modules.dep missing even after depmod"
    local root="$OUT/initrd" line path deps m found
    mkdir -p "$root/lib/modules/$kver"
    # All depmod metadata incl. the .bin indexes (kmod's modprobe reads
    # the binary indexes, busybox modprobe the text ones — carry both).
    local f
    for f in "$OUT/modules/$kver"/modules.*; do
        [ -f "$f" ] && cp "$f" "$root/lib/modules/$kver/"
    done
    copy_mod() { # relative module path
        local rel="$1" d
        [ -f "$root/lib/modules/$kver/$rel" ] && return 0
        d="$(dirname "$rel")"
        mkdir -p "$root/lib/modules/$kver/$d"
        cp "$OUT/modules/$kver/$rel" "$root/lib/modules/$kver/$rel" ||
            die "module $rel missing from the RPM tree"
    }
    for m in "${GUEST_MODULES[@]}"; do
        # modules.dep matches with -/_ equivalence on the basename.
        found=""
        while IFS= read -r line; do
            path="${line%%:*}"
            local base
            base="$(basename "$path" .ko)"
            if [ "${base//-/_}" = "${m//-/_}" ]; then
                found="$line"
                break
            fi
        done <"$dep"
        [ -n "$found" ] || die "module '$m' not found in modules.dep — the guest kernel config regressed (docker/kernel-sqz/config-fragment asserts it)"
        copy_mod "${found%%:*}"
        deps="${found#*:}"
        for path in $deps; do copy_mod "$path"; done
        log "module closure: $m (+$(echo "$deps" | wc -w) deps)"
    done

    # --- busybox + host tool gap-fillers, each with its lib closure ----------
    # The mkinitcpio busybox omits mount/modprobe/timeout — copy the host
    # binaries (glibc closures ride along; the guest runs the host's ABI).
    mkdir -p "$root/bin" "$root/sbin" "$root/proc" "$root/sys" "$root/dev" \
        "$root/tmp" "$root/run" "$root/etc" "$root/share" "$root/mnt"
    copy_with_closure() { # binary dest-name
        local bin="$1" dest="$2" lib
        cp "$bin" "$root/bin/$dest"
        while IFS= read -r lib; do
            [ -f "$lib" ] || continue
            mkdir -p "$root$(dirname "$lib")"
            cp -n "$lib" "$root$lib" 2>/dev/null || true
            # Arch aliases /lib64 to /usr/lib; the loader's advertised
            # path must exist verbatim inside the initramfs.
            case "$lib" in
            */ld-linux-x86-64.so.2)
                mkdir -p "$root/lib64"
                cp -n "$lib" "$root/lib64/ld-linux-x86-64.so.2" 2>/dev/null || true
                ;;
            esac
        done < <(ldd "$bin" 2>/dev/null | awk '{ for (i=1;i<=NF;i++) if ($i ~ /^\//) print $i }' | sort -u)
    }
    copy_with_closure "$BUSYBOX" busybox
    local tool
    for tool in mount modprobe timeout; do
        command -v "$tool" >/dev/null 2>&1 || die "host tool '$tool' is required (initramfs gap-filler)"
        copy_with_closure "$(command -v "$tool")" "$tool"
    done

    # --- the init script (job-executor control plane) ------------------------
    cat >"$root/init" <<'INIT'
#!/bin/busybox sh
# mw-guest init (rung 6b): bring up the slirp NIC + 9p share + fabric
# modules, then loop the /share/jobs executor (mw_fleet.sh vm_exec).
export PATH=/bin:/sbin
/bin/busybox --install -s /bin 2>/dev/null
mount -t proc proc /proc
mount -t sysfs sysfs /sys
mount -t devtmpfs dev /dev 2>/dev/null
mkdir -p /dev/pts && mount -t devpts devpts /dev/pts 2>/dev/null
for m in virtio_net 9pnet_virtio 9p nvme-tcp fuse; do
    modprobe "$m" || echo "MWGUEST: modprobe $m FAILED"
done
ip link set lo up
ip link set eth0 up
# slirp user-net statics: guest 10.0.2.15/24, host gateway 10.0.2.2.
ip addr add 10.0.2.15/24 dev eth0
ip route add default via 10.0.2.2
# The host share: cache=none is load-bearing — the job loop must see
# host-written files immediately, and the host must see .rc/.out
# without a cache flush.
if ! mount -t 9p -o trans=virtio,version=9p2000.L,cache=none,msize=1048576 hostshare /share; then
    echo "MWGUEST: 9p share mount FAILED"
fi
mkdir -p /share/jobs
# KD-MW-2 node identity: the per-guest stable token mw_fleet.sh staged
# on the share (stable across THIS guest's reboots — the vm dir owns
# it — and distinct across guests; an initramfs has no machine-id).
if [ -f /share/node-id ]; then
    mkdir -p /etc/squeezefs
    cp /share/node-id /etc/squeezefs/node-id
fi
uname -r >/share/guest-ready.tmp && mv /share/guest-ready.tmp /share/guest-ready
echo "MWGUEST: ready $(uname -r)"
while :; do
    for j in /share/jobs/*.sh; do
        [ -f "$j" ] || continue
        rc="${j%.sh}.rc"
        [ -f "$rc" ] && continue
        out="${j%.sh}.out"
        sh "$j" >"$out" 2>&1
        echo $? >"$rc.tmp" && mv "$rc.tmp" "$rc"
    done
    sleep 0.2
done
INIT
    chmod +x "$root/init"

    # --- pack ---------------------------------------------------------------
    (cd "$root" && find . -print0 | cpio --null -o -H newc --quiet | gzip -1) \
        >"$OUT/initramfs.img"
    rm -rf "$OUT/extract"
    log "boot pair built: $OUT/vmlinuz + $OUT/initramfs.img ($kver, $(du -h "$OUT/initramfs.img" | cut -f1))"
}

status_image() {
    if [ -f "$OUT/vmlinuz" ] && [ -f "$OUT/initramfs.img" ]; then
        echo "[mwguest] boot pair at $OUT: $(cat "$OUT/kernel-release" 2>/dev/null || echo '?') ($(du -h "$OUT/initramfs.img" | cut -f1) initramfs)"
    else
        echo "[mwguest] no boot pair at $OUT — run: tests/mw_guest_image.sh build"
        return 1
    fi
}

VERB="${1:-}"
shift || true
case "$VERB" in
build) build_image "$@" ;;
status) status_image ;;
*)
    awk 'NR > 1 && /^#/ { sub(/^# ?/, ""); print; next } NR > 1 { exit }' "$0"
    exit 1
    ;;
esac
