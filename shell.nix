{ pkgs ? import <nixpkgs> {} }:

pkgs.mkShell {
  buildInputs = [
    pkgs.cargo
    pkgs.rustc
    pkgs.rustfmt
    pkgs.clippy
    pkgs.go-task
    pkgs.cargo-audit # task audit (the ENG-2 gate leg) — both lockfiles

    # System deps per AGENTS.md §Build (pkg-config, libfuse3, clang/libclang).
    pkgs.pkg-config
    pkgs.fuse3
    pkgs.clang
    pkgs.llvmPackages.libclang

    # Rig toolchain (tests/run_mw_matrix.sh, tests/mw_fleet.sh,
    # tests/dev_substrate.sh): the pinned ior 4.0.0 builds with mpicc,
    # legs orchestrate with mpirun/python3, substrates need nvme-cli.
    # Root legs must carry this PATH through sudo:
    #   sudo env "PATH=$PATH" bash tests/<rig>.sh ...
    pkgs.openmpi
    pkgs.python3
    pkgs.nvme-cli
  ];

  # tikv-jemalloc-sys (jemalloc 5.3.0) returns `char *` from an
  # int-returning wrapper around strerror_r; GCC >= 14 promotes
  # -Wint-conversion to an error by default, so its `make` fails against
  # the Nix glibc. Downgrade it back to a warning for C deps only.
  CFLAGS = "-Wno-error=int-conversion";

  # bindgen-style -sys crates locate libclang through this.
  LIBCLANG_PATH = "${pkgs.llvmPackages.libclang.lib}/lib";
}
