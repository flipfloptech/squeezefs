#!/usr/bin/env bash
# In-container half of the sqz kernel build (see README.md; host half is
# build.sh). Contract: /src = docker/kernel-sqz/ mounted READ-ONLY,
# /work = named volume (tarball cache + build tree), /out = host dist dir.
#
# Produces EL8-installable kernel RPMs via `make binrpm-pkg` with
# LOCALVERSION=-sqz (the non-negotiable sqz tag), from:
#   the TRACK's pinned base tarball (sha256-pinned; table below)
#   + the TRACK's series in /src/patches[-TRACK] (SERIES.md is the manifest)
#   + client base config + /src/config-fragment (checklist asserted).
#
# TRACK selects the row: 6.19.14 (default — the FIELD build, the EL8
# fleet RPMs), 7.1 (linux-7.1.6, patches-7.1/), 7.2 (linux-7.2.6,
# patches-7.2/ — rebased from 7.2.3 on 2026-09-21, SERIES.md). An unknown
# TRACK refuses loud before any fetch.
set -euo pipefail

TRACK=${TRACK:-6.19.14}
case "$TRACK" in
	6.19.14)
		KVER=6.19.14
		SHA256=cde8bf6739be4a0777fedbbba5330b8188c55680c45a922a4dfa289cbec6f185
		PATCHES=/src/patches ;;
	7.1)
		KVER=7.1.6
		SHA256=995dd7188d924662b94b48fd6fb783587267590e5b8bb33dade2c771e7d855c1
		PATCHES=/src/patches-7.1 ;;
	7.2)
		KVER=7.2.6
		SHA256=039aef84f2b0994aeda3f4fcfc3d02ec9d7a9bbb9020ea264c43f446c860f606
		PATCHES=/src/patches-7.2 ;;
	*)
		echo "unknown TRACK='${TRACK}' (want 6.19.14 | 7.1 | 7.2)"; exit 1 ;;
esac
TARBALL=linux-${KVER}.tar.xz
URL=https://cdn.kernel.org/pub/linux/kernel/v${KVER%%.*}.x/${TARBALL}
JOBS=${JOBS:-$(nproc)}
echo "track: ${TRACK} → linux-${KVER} + $(ls "${PATCHES}"/*.patch | wc -l) patches from ${PATCHES}"

# gcc-toolset (pinned in the Dockerfile).
source /opt/rh/${SQZ_TOOLSET:?}/enable
echo "toolchain: $(gcc --version | head -1) | pahole $(pahole --version) | $(rpmbuild --version)"

cd /work
if ! echo "${SHA256}  ${TARBALL}" | sha256sum -c - 2>/dev/null; then
	echo "fetching ${TARBALL}"
	curl -fsSL -o "${TARBALL}" "${URL}"
	echo "${SHA256}  ${TARBALL}" | sha256sum -c -
fi

rm -rf "linux-${KVER}" && tar xf "${TARBALL}"
cd "linux-${KVER}"

echo "== applying series (SERIES.md manifest) =="
for p in "${PATCHES}"/*.patch; do
	echo "  $(basename "$p")"
	patch -p1 --fuzz=0 --silent < "$p"
done

echo "== config: client base + olddefconfig + fragment =="
cp /src/config-base-7.1.2-1.el8.elrepo.x86_64 .config
make olddefconfig
# Apply the fragment (scripts/config keeps olddefconfig honest after).
while IFS= read -r line; do
	case "$line" in
		''|'#'*) continue ;;
		CONFIG_*=n) scripts/config -d "${line%%=*}" ;;
		CONFIG_*='"'*'"') opt=${line%%=*}; val=${line#*=}; scripts/config --set-str "$opt" "${val//\"/}" ;;
		CONFIG_*=m) scripts/config -m "${line%%=*}" ;;
		CONFIG_*=y) scripts/config -e "${line%%=*}" ;;
	esac
done < /src/config-fragment
make olddefconfig

echo "== ENABLE CHECKLIST assertion (fail loud) =="
fail=0
while IFS= read -r line; do
	case "$line" in
		''|'#'*) continue ;;
		CONFIG_LOCALVERSION_AUTO=n) want='# CONFIG_LOCALVERSION_AUTO is not set' ;;
		*) want="$line" ;;
	esac
	if ! grep -qxF "$want" .config; then
		echo "MISSING from final .config: $want"; fail=1
	fi
done < /src/config-fragment
[ "$fail" = 0 ] || { echo "config checklist FAILED"; exit 1; }
grep -qx 'CONFIG_LOCALVERSION="-sqz"' .config || { echo "sqz tag missing"; exit 1; }
echo "checklist OK ($(grep -c '^CONFIG_' /src/config-fragment) entries verified)"

echo "== build: make -j${JOBS} binrpm-pkg (INSTALL_MOD_STRIP=1) =="
make -j"${JOBS}" INSTALL_MOD_STRIP=1 binrpm-pkg 2>&1 | tail -40

echo "== artifacts =="
mkdir -p /out
# binrpm-pkg writes into the in-tree rpmbuild _topdir (see the rpmbuild
# --define in scripts/Makefile.package), not ~/rpmbuild — but probe both.
# NOTE: find must tolerate the absent probe path: under `set -euo pipefail`
# a bare `found=$(find missing … | wc -l)` dies on find's nonzero exit
# BEFORE the guard runs (the v2 build's first artifact-stage failure).
found=$( { find "/work/linux-${KVER}/rpmbuild/RPMS" /root/rpmbuild/RPMS \
	-name '*.rpm' 2>/dev/null || true; } | wc -l)
[ "$found" -gt 0 ] || { echo "no RPMs produced"; exit 1; }
{ find "/work/linux-${KVER}/rpmbuild/RPMS" /root/rpmbuild/RPMS \
	-name '*.rpm' -exec cp -v {} /out/ \; 2>/dev/null || true; }
cp .config /out/config-${KVER}-sqz
sha256sum /out/*.rpm | tee /out/SHA256SUMS
echo "sqz kernel build complete: $(make -s kernelrelease)"
