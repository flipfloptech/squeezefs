# Rocky Linux 8 (latest 8.x) kernel-build image for the `sqz` custom
# kernel (see docker/kernel-sqz/README.md and SERIES.md).
#
# Toolchain pin: gcc-toolset-14 (GCC 14) — new enough for a 6.19/7.x
# kernel, packaged by the distro the target box runs (Rocky 8.10), so
# the resulting kernel RPMs are EL8-installable with the EL8 glibc/rpm
# stack. pahole (dwarves) ≥ 1.26 is required for CONFIG_DEBUG_INFO_BTF
# but EL8 ships 1.22 — we build pahole v1.30 from source.
FROM docker.io/rockylinux/rockylinux:8

RUN dnf -y install dnf-plugins-core \
 && dnf config-manager --set-enabled powertools \
 && dnf -y install \
      gcc-toolset-14 gcc-toolset-14-binutils-devel \
      make git patch diffutils findutils hostname file \
      bison flex bc openssl openssl-devel \
      elfutils-libelf-devel elfutils-devel zlib-devel \
      ncurses-devel perl python3 python3.12 \
      rsync cpio kmod xz tar bzip2 zstd \
      rpm-build cmake \
      dwarves \
 && dnf clean all \
 && ln -sf /usr/bin/python3.12 /usr/local/bin/python3
# NOTE: `dwarves` (EL8's pahole 1.22 — too old for BTF on this kernel) is
# installed ONLY to satisfy the kernel.spec BuildRequires; the from-source
# pahole v1.30 below lands in /usr/local/bin, which shadows it on PATH.
# Same story for `python3` (3.6, the RPM dep) vs the /usr/local/bin
# python3 -> 3.12 shim the kernel scripts actually resolve.

# pahole v1.30 from source (BTF encoder; EL8 dwarves is too old).
# Built with the base gcc (fine — pahole is a userspace tool).
RUN dnf -y install gcc gcc-c++ \
 && git clone --depth 1 --branch v1.30 --recurse-submodules \
      https://git.kernel.org/pub/scm/devel/pahole/pahole.git /tmp/pahole \
 && cmake -S /tmp/pahole -B /tmp/pahole/build -D__LIB=lib -DCMAKE_BUILD_TYPE=Release \
 && make -C /tmp/pahole/build -j"$(nproc)" install \
 && rm -rf /tmp/pahole \
 && echo /usr/local/lib64 > /etc/ld.so.conf.d/local-lib64.conf \
 && ldconfig \
 && pahole --version

# Kernel builds run under gcc-toolset-14 (enabled by build-kernel.sh).
ENV SQZ_TOOLSET=gcc-toolset-14
