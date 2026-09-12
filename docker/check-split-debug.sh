#!/usr/bin/env bash
# Split-debug-info contract for TAGGED-RELEASE (`dist`) artifacts — called
# by check-artifacts.sh when the built profile is `dist`, and runnable
# standalone against any artifact directory (it executes nothing, so it
# works on a host that cannot run the foreign-glibc binaries).
#
#   check-split-debug.sh <dir>            # squeezefs + libsqueezefs_il.so
#
# The law (2026-09-12): a dist artifact ships STRIPPED of DWARF — the 1.2.4
# daemon was 329 MiB, 305 of them .debug_* — with its debug info beside it
# as `<name>.debug`, joined by a `.gnu_debuglink` whose CRC32 matches the
# sidecar, so perf/gdb resolve symbols from the same directory (or from
# /usr/lib/debug) and every profiling row stays symbolicated while the
# shipped binary is ~24 MiB. `release` (the dev / A-B / gate profile) keeps
# its symbols in-binary and is not subject to this check.
set -euo pipefail
dir=${1:?usage: check-split-debug.sh <dir>}

for f in "$dir/squeezefs" "$dir/libsqueezefs_il.so"; do
  name=$(basename "$f")
  [ -f "$f" ] || { echo "FAIL: $f missing" >&2; exit 1; }
  if readelf -S "$f" | grep -q '\.debug_info'; then
    echo "FAIL: $name still carries .debug_info — a dist artifact ships stripped with a .debug sidecar" >&2
    exit 1
  fi
  [ -f "$f.debug" ] || { echo "FAIL: $name.debug (the split DWARF) is missing beside $name" >&2; exit 1; }
  # (readelf warns about the sidecar's missing interpreter — it is not a program; silenced.)
  readelf -S "$f.debug" 2>/dev/null | grep -q '\.debug_info' || { echo "FAIL: $name.debug carries no .debug_info" >&2; exit 1; }
  # .gnu_debuglink = NUL-terminated file name, padded to 4, then CRC32 LE.
  python3 - "$f" "$name.debug" "$f.debug" <<'PY'
import subprocess, sys, zlib
binary, want_name, sidecar = sys.argv[1:4]
hdr = subprocess.run(["readelf", "-S", "-W", binary], capture_output=True, text=True, check=True).stdout
row = next((l for l in hdr.splitlines() if ".gnu_debuglink" in l), None)
if row is None:
    sys.exit(f"FAIL: {binary} has no .gnu_debuglink section")
# readelf -S -W row: [Nr] Name Type Address Off Size ...
cols = row.split("]", 1)[1].split()
off, size = int(cols[3], 16), int(cols[4], 16)
with open(binary, "rb") as fh:
    fh.seek(off); sec = fh.read(size)
link_name = sec.split(b"\0", 1)[0].decode()
crc_link = int.from_bytes(sec[-4:], "little")
crc_side = zlib.crc32(open(sidecar, "rb").read()) & 0xffffffff
if link_name != want_name:
    sys.exit(f"FAIL: .gnu_debuglink names {link_name!r}, want {want_name!r}")
if crc_link != crc_side:
    sys.exit(f"FAIL: .gnu_debuglink CRC {crc_link:08x} != sidecar CRC {crc_side:08x} (a swapped sidecar)")
print(f"  {want_name.removesuffix('.debug')}: stripped; .gnu_debuglink -> {link_name} (crc {crc_link:08x} verified)")
PY
  echo "    sizes: $(du -h "$f" | cut -f1) binary, $(du -h "$f.debug" | cut -f1) DWARF sidecar"
done
echo "split-debug checks passed: $dir"
