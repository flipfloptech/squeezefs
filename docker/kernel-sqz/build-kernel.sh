#!/usr/bin/env bash
# In-container half of the sqz kernel build (see README.md; host half is
# build.sh). Contract: /src = docker/kernel-sqz/ mounted READ-ONLY,
# /work = named volume (tarball cache + build tree), /out = host dist dir.
#
# Produces EL8-installable kernel RPMs via `make binrpm-pkg` with
# LOCALVERSION=-sqz (the non-negotiable sqz tag), from:
#   linux-6.19.14 (base the FUSE-zc series applies to; sha256-pinned)
#   + the 26-patch series in /src/patches (SERIES.md is the manifest)
#   + client base config + /src/config-fragment (checklist asserted).
set -euo pipefail

KVER=6.19.14
TARBALL=linux-${KVER}.tar.xz
SHA256=cde8bf6739be4a0777fedbbba5330b8188c55680c45a922a4dfa289cbec6f185
URL=https://cdn.kernel.org/pub/linux/kernel/v6.x/${TARBALL}
JOBS=${JOBS:-$(nproc)}

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
for p in /src/patches/*.patch; do
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
find /root/rpmbuild/RPMS -name '*.rpm' -exec cp -v {} /out/ \;
cp .config /out/config-${KVER}-sqz
sha256sum /out/*.rpm | tee /out/SHA256SUMS
echo "sqz kernel build complete: $(make -s kernelrelease)"
