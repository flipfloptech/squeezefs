//! SqueezeFS build graph.
//!
//! Two build profiles (charter §11 "Two build profiles"):
//!   - `gate` (default): the fast dev/gate profile — ReleaseSafe + thin LTO.
//!     Every assertion and tripwire stays armed; this is what the gate runs.
//!   - `dist`: ReleaseSafe + fat LTO, debug info kept in the binary and then
//!     split into `<name>.debug` with a `.gnu_debuglink` by the `dist` step.
//!     ReleaseSafe, not ReleaseFast (decision 0003 Q8): a true bug aborts
//!     loud (charter §1.8; decision 0002 row 9) instead of becoming UB; the
//!     1–3 % the bounds checks cost on the hot paths is a counted A/B at
//!     rung 9, which must PRECEDE taking the faster posture (charter §1).
//! The binary names its profile (`squeezefs --version` prints train, commit,
//! profile — `build_options`, imported by `main` alone: the library never
//! depends on the commit string, so a HEAD move invalidates only the exe).
//!
//! The one C dependency is the vendored zstd amalgamation (`vendor/zstd`),
//! compiled into the library module and linked with libc.
//!
//! Steps:
//!   zig build              — build the `squeezefs` binary for the host target.
//!   zig build test         — unit tests (portable modules run everywhere;
//!                            Linux-only modules compile their tests only on Linux).
//!   zig build rails        — static-rail tests (knob literals registered, …).
//!   zig build smoke        — THE SMOKE: the built binary as a child process
//!                            against a real mount (the Linux venue; skips
//!                            loudly elsewhere).
//!   zig build bench-smoke  — `squeezefs bench --smoke` against a mount the
//!                            step creates (one iteration of every workload).
//!   zig build fmt          — `zig fmt --check` over the tree.
//!   zig build gate         — fmt + test + rails + smoke + bench-smoke: the
//!                            per-commit gate.
//!   zig build dist         — the dist profile artifact + split debug info.
//!   zig build shim         — libsqueezefs_il.so (the LD_PRELOAD shim) into lib/ (Linux).
//!   zig build shim-smoke   — the LD_PRELOAD battery against a mount (Linux, root).
//!   zig build nvmet-smoke  — the nvmeof fidelity tier against the kernel nvmet
//!                            target (Linux, root; part of the gate).
const std = @import("std");

const Profile = enum { gate, dist };

pub fn build(b: *std.Build) void {
    const target = b.standardTargetOptions(.{});
    const profile = b.option(Profile, "profile", "Build profile: gate (default, ReleaseSafe + thin LTO) or dist (ReleaseSafe + fat LTO + split debug)") orelse .gate;
    // Both profiles are ReleaseSafe (see the module doc); -Doptimize overrides
    // it (tests want Debug for the best diagnostics; `zig build -Doptimize=Debug test`).
    const optimize = b.standardOptimizeOption(.{ .preferred_optimize_mode = .ReleaseSafe });

    // ---- build identity: train, commit, profile (charter §11 "Versioning") ----
    const build_options = b.addOptions();
    build_options.addOption([]const u8, "profile", @tagName(profile));
    build_options.addOption([]const u8, "train", train_version);
    build_options.addOption([]const u8, "git_commit", gitCommit(b));
    const build_options_mod = build_options.createModule();

    // ---- the library module every artifact imports (no build identity) ----
    const squeezefs_mod = b.addModule("squeezefs", .{
        .root_source_file = b.path("src/squeezefs.zig"),
        .target = target,
        .optimize = optimize,
    });
    // The bench baseline rig's two resolved paths (`cli/bench_baseline_rig.zig`):
    // the install prefix (the binary it drives) and the repo's bench/ dir.
    // Untracked: strings, no step dependency, so the library never cycles on
    // the binary it is part of.
    const bench_options = b.addOptions();
    bench_options.addOptionPathUntracked("install_prefix", .{ .relative = .{ .base = .install_prefix } });
    bench_options.addOptionPathUntracked("bench_dir", b.path("bench"));
    squeezefs_mod.addImport("bench_options", bench_options.createModule());

    // ---- vendored zstd (vendor/zstd/VERSION; decision 0002 row 118):
    //      the single-file amalgamation compiled into the library for
    //      COMPRESSION only (std's zstd is decompress-only); the header
    //      translated into the `zstd_c` module `data/transform.zig`
    //      imports. `ZSTD_STATIC_LINKING_ONLY` for the static-context API
    //      (`ZSTD_initStaticCCtx`: a compressor that mallocs nothing per
    //      op, charter §12). Portable: the dev box runs the codec tests too. ----
    const zstd_c = b.addTranslateC(.{
        .root_source_file = b.path("vendor/zstd/zstd.h"),
        .target = target,
        .optimize = optimize,
    });
    zstd_c.defineCMacro("ZSTD_STATIC_LINKING_ONLY", "1");
    zstd_c.addIncludePath(b.path("vendor/zstd"));
    squeezefs_mod.addImport("zstd_c", zstd_c.createModule());
    // The amalgamation is its own static library, not a C source of the
    // library module: the fuzz campaign (`zig build fuzz`, charter §11)
    // rebuilds the test module with `-ffuzz`, and clang's coverage pass
    // emits `__sanitizer_cov_trace_const_cmp*` hooks into C it instruments
    // that Zig's fuzzer runtime does not define (the link fails); the
    // decoders under fuzz are Zig, so zstd needs no instrumentation.
    const zstd_mod = b.createModule(.{ .target = target, .optimize = optimize, .link_libc = true });
    zstd_mod.addCSourceFile(.{ .file = b.path("vendor/zstd/zstd.c"), .flags = &.{ "-DZSTD_STATIC_LINKING_ONLY", "-std=c99" } });
    const zstd_lib = b.addLibrary(.{ .name = "zstd", .linkage = .static, .root_module = zstd_mod });
    squeezefs_mod.linkLibrary(zstd_lib);
    squeezefs_mod.link_libc = true;

    // ---- the one CLI binary (charter §8) ----
    const exe = b.addExecutable(.{
        .name = "squeezefs",
        .root_module = b.createModule(.{
            .root_source_file = b.path("src/main.zig"),
            .target = target,
            .optimize = optimize,
            .imports = &.{
                .{ .name = "squeezefs", .module = squeezefs_mod },
                .{ .name = "build_options", .module = build_options_mod },
            },
        }),
    });
    // LTO needs LLD, which Zig drives on Linux (the product target). The macOS
    // dev box builds portable modules only and never ships a binary.
    if (target.result.os.tag == .linux) {
        exe.lto = switch (profile) {
            .gate => .thin,
            .dist => .full,
        };
    }
    b.installArtifact(exe);

    const run_step = b.step("run", "Run the squeezefs binary");
    const run_cmd = b.addRunArtifact(exe);
    run_cmd.step.dependOn(b.getInstallStep());
    run_cmd.addPassthruArgs();
    run_step.dependOn(&run_cmd.step);

    // ---- the LD_PRELOAD shim `libsqueezefs_il.so` (charter §12; decision
    //      0003 §F): an explicit `zig build-lib` because `Step.Compile` has
    //      no `-z nodelete` (pthread_atfork handlers can never be
    //      unregistered, so `dlclose` must be a no-op) and no `-fsoname`
    //      (by-path links must record a clean DT_NEEDED). ReleaseSafe
    //      whatever the profile: a shim bug trips the panic handler loud
    //      instead of becoming UB inside a host process. Linux targets
    //      only; `zig build shim` installs it to lib/. The build identity
    //      rides the same `build_options` as the daemon (same-folder
    //      pairing, charter §11). ----
    const shim_step = b.step("shim", "Build and install libsqueezefs_il.so (Linux targets)");
    const shim_dir: ?std.Build.LazyPath = if (target.result.os.tag == .linux) blk: {
        const build_lib = b.addSystemCommand(&.{ b.graph.zig_exe, "build-lib", "-dynamic", "-lc", "-z", "nodelete", "-fsoname=libsqueezefs_il.so", "--name", "squeezefs_il", "-O", "ReleaseSafe" });
        build_lib.addArgs(&.{ "-target", target.query.zigTriple(b.allocator) catch @panic("OOM"), "-mcpu", target.query.serializeCpuAlloc(b.allocator) catch @panic("OOM") });
        build_lib.addArg("--cache-dir");
        build_lib.addDirectoryArg(.cache_root);
        build_lib.addArg("--global-cache-dir");
        build_lib.addDirectoryArg(.{ .relative = .{ .base = .global_cache } });
        build_lib.addArgs(&.{ "--dep", "build_options" });
        build_lib.addPrefixedFileArg("-Mroot=", b.path("src/shim_so.zig"));
        build_lib.addPrefixedFileArg("-Mbuild_options=", build_options.getOutput());
        const so = build_lib.addPrefixedOutputFileArg("-femit-bin=", "libsqueezefs_il.so");
        // The compiler's own cache makes the re-run cheap; the step cannot
        // enumerate the shim's transitive sources as inputs.
        build_lib.has_side_effects = true;
        shim_step.dependOn(&b.addInstallLibFile(so, "libsqueezefs_il.so").step);
        b.getInstallStep().dependOn(shim_step);
        // The census and the shim smoke find the artifacts in their cwd: a
        // staged directory holding the .so beside the binary under test.
        const staged = b.addWriteFiles();
        _ = staged.addCopyFile(so, "libsqueezefs_il.so");
        _ = staged.addCopyFile(exe.getEmittedBin(), "squeezefs");
        break :blk staged.getDirectory();
    } else null;

    // ---- release-gate mode (charter §11 "a release-gate mode turns skips
    //      into failures"): every test run step carries the harness knob the
    //      ONE testkit reads; a `testkit.skip` then FAILS the test except for
    //      the classes a venue cannot honestly satisfy (`core/testkit.zig`) ----
    const release_gate = b.option(bool, "release-gate", "Release-gate mode: environment skips fail the gate (except needs_reference_hardware / other_step)") orelse false;

    // ---- unit tests over the library module ----
    const test_filters = b.option([]const []const u8, "test-filter", "Only run tests whose names contain this text (repeatable)") orelse &.{};
    const unit_tests = b.addTest(.{
        .name = "squeezefs-unit",
        .root_module = squeezefs_mod,
        .filters = test_filters,
    });
    const run_unit_tests = gated(b.addRunArtifact(unit_tests), release_gate);
    // The fuzz campaign's replay seam (`core/fuzzkit.zig`): the artefact path
    // rides the step's own environment so a replay is part of the run's
    // cache manifest — a probe with a new artefact is never a cached pass.
    if (b.graph.environ_map.get("SQUEEZEFS_TEST_FUZZ_REPLAY")) |p| run_unit_tests.setEnvironmentVariable("SQUEEZEFS_TEST_FUZZ_REPLAY", p);
    const test_step = b.step("test", "Run unit tests");
    test_step.dependOn(&run_unit_tests.step);

    // The gate steps are declared here so the steps below can hang off them;
    // their dependencies are wired where each piece lands.
    const gate_step = b.step("gate", "fmt + unit tests + rails + smoke + bench-smoke + sdk-smoke (the per-commit gate)");
    const gate_release_step = b.step("gate-release", "gate + bench-compare against the committed baseline (run with -Drelease-gate=true: skips fail)");

    // ---- static rails: source-scanning convention tests (charter §11) ----
    const rails_tests = b.addTest(.{
        .name = "squeezefs-rails",
        .root_module = b.createModule(.{
            .root_source_file = b.path("tests/rails.zig"),
            .target = target,
            .optimize = optimize,
            .imports = &.{
                .{ .name = "squeezefs", .module = squeezefs_mod },
            },
        }),
    });
    const run_rails_tests = gated(b.addRunArtifact(rails_tests), release_gate);
    // The rails scan the source tree relative to the repository root.
    run_rails_tests.setCwd(b.path("."));
    const rails_step = b.step("rails", "Run static-rail convention tests");
    rails_step.dependOn(&run_rails_tests.step);

    // ---- the shim's binary census (Linux): SONAME, DT_FLAGS_1 NODELETE and
    //      the exported symbol set == the ONE table, read off the .so's
    //      dynamic section (readelf-free) ----
    if (shim_dir) |dir| {
        const census_tests = b.addTest(.{
            .name = "squeezefs-shim-census",
            .root_module = b.createModule(.{
                .root_source_file = b.path("tests/shim_census.zig"),
                .target = target,
                .optimize = optimize,
                .imports = &.{
                    .{ .name = "squeezefs", .module = squeezefs_mod },
                },
            }),
        });
        const run_census = gated(b.addRunArtifact(census_tests), release_gate);
        run_census.setCwd(dir);
        rails_step.dependOn(&run_census.step);
    }

    // ---- the smoke: the library's `smoke:` tests with the binary under
    //      test as `./squeezefs` in the cwd (the emitted-bin directory, so
    //      the smoke drives the binary of THIS build and the step depends
    //      on it); in the unit run above they find no binary and skip ----
    // `-Dtest-filter` narrows the smoke too (one stage's test, looped).
    const smoke_tests = b.addTest(.{
        .name = "squeezefs-smoke",
        .root_module = squeezefs_mod,
        .filters = if (test_filters.len != 0) test_filters else &.{"smoke:"},
    });
    const run_smoke_tests = gated(b.addRunArtifact(smoke_tests), release_gate);
    run_smoke_tests.setCwd(exe.getEmittedBinDirectory());
    // The smoke drives a real mount on the venue's kernel: its outcome is
    // not a function of the build inputs, so every gate run re-runs it.
    run_smoke_tests.has_side_effects = true;
    const smoke_step = b.step("smoke", "Run the smoke (format/mount/umount/status against a real mount)");
    smoke_step.dependOn(&run_smoke_tests.step);

    // ---- the bench smoke (charter §11: one iteration of every workload
    //      per commit): `squeezefs bench --smoke` against a mount the smoke
    //      creates, driven like the smoke above; skips loudly off the venue ----
    const bench_smoke_tests = b.addTest(.{
        .name = "squeezefs-bench-smoke",
        .root_module = squeezefs_mod,
        .filters = &.{"bench gate:"},
    });
    const run_bench_smoke = gated(b.addRunArtifact(bench_smoke_tests), release_gate);
    run_bench_smoke.setCwd(exe.getEmittedBinDirectory());
    run_bench_smoke.has_side_effects = true;
    // After the smoke: the two drive real mounts on one venue and must
    // never share it (the build graph runs independent steps in parallel).
    run_bench_smoke.step.dependOn(&run_smoke_tests.step);
    const bench_smoke_step = b.step("bench-smoke", "Run `squeezefs bench --smoke` against a mount the step creates (one iteration of every workload)");
    bench_smoke_step.dependOn(&run_bench_smoke.step);

    // ---- the microbench baseline (charter §11 "a committed microbench
    //      baseline with per-group regression thresholds"): the rig is its
    //      own test root fed the binary and the repo's bench/ dir as resolved
    //      paths; `bench-baseline` records, `bench-compare` judges (part of
    //      gate-release, never of the per-commit gate) ----
    const bench_baseline_step = b.step("bench-baseline", "Record the microbench baseline + thresholds for THIS venue into bench/ (3 runs; Linux, root)");
    const bench_compare_step = b.step("bench-compare", "Judge one microbench run against bench/'s committed baseline for THIS venue (gate-release)");
    if (target.result.os.tag == .linux) {
        // The rig drives the INSTALLED binary (`<prefix>/bin/squeezefs`): the
        // prefix and the repo's bench/ dir reach it through `bench_options`
        // (untracked paths: no step the library could cycle on).
        const record = b.addTest(.{ .name = "squeezefs-bench-baseline", .root_module = squeezefs_mod, .filters = &.{"bench baseline:"} });
        const run_record = gated(b.addRunArtifact(record), release_gate);
        run_record.has_side_effects = true;
        run_record.step.dependOn(b.getInstallStep());
        bench_baseline_step.dependOn(&run_record.step);
        const compare = b.addTest(.{ .name = "squeezefs-bench-compare", .root_module = squeezefs_mod, .filters = &.{"bench compare:"} });
        const run_compare = gated(b.addRunArtifact(compare), release_gate);
        run_compare.has_side_effects = true;
        run_compare.step.dependOn(b.getInstallStep());
        bench_compare_step.dependOn(&run_compare.step);
        // In the release gate the judge runs AFTER the whole per-commit gate:
        // its numbers never share the venue with a smoke's mounts.
        const run_compare_after_gate = gated(b.addRunArtifact(compare), release_gate);
        run_compare_after_gate.has_side_effects = true;
        run_compare_after_gate.step.dependOn(gate_step);
        run_compare_after_gate.step.dependOn(b.getInstallStep());
        gate_release_step.dependOn(&run_compare_after_gate.step);
    } else {
        bench_baseline_step.dependOn(&b.addFail("the bench baseline is Linux-only (charter §1.2): record it on the venue").step);
        bench_compare_step.dependOn(&b.addFail("the bench compare is Linux-only (charter §1.2): run it on the venue").step);
    }

    // ---- the shim smoke (Linux): the LD_PRELOAD battery with the staged
    //      .so beside the binary under test; after the bench smoke (one
    //      mount on the venue at a time) ----
    const shim_smoke_step = b.step("shim-smoke", "Run the LD_PRELOAD shim battery against a mount the step creates");
    if (shim_dir) |dir| {
        const shim_smoke_tests = b.addTest(.{
            .name = "squeezefs-shim-smoke",
            .root_module = squeezefs_mod,
            .filters = &.{"shim smoke:"},
        });
        const run_shim_smoke = gated(b.addRunArtifact(shim_smoke_tests), release_gate);
        run_shim_smoke.setCwd(dir);
        run_shim_smoke.has_side_effects = true;
        run_shim_smoke.step.dependOn(&run_bench_smoke.step);
        shim_smoke_step.dependOn(&run_shim_smoke.step);
    }

    // ---- the nvmeof fidelity tier (charter §8, §11 release gate): the
    //      `nvmet fidelity:` legs through the binary against the venue's
    //      kernel nvmet target (loop + tcp), zero mocks; after the shim
    //      smoke (one mount on the venue at a time); skips loudly off the
    //      venue ----
    const nvmet_smoke_tests = b.addTest(.{
        .name = "squeezefs-nvmet-smoke",
        .root_module = squeezefs_mod,
        .filters = &.{"nvmet fidelity:"},
    });
    const run_nvmet_smoke = gated(b.addRunArtifact(nvmet_smoke_tests), release_gate);
    run_nvmet_smoke.setCwd(exe.getEmittedBinDirectory());
    run_nvmet_smoke.has_side_effects = true;
    run_nvmet_smoke.step.dependOn(&run_bench_smoke.step);
    if (shim_dir != null) run_nvmet_smoke.step.dependOn(shim_smoke_step);
    const nvmet_smoke_step = b.step("nvmet-smoke", "Run the nvmeof fidelity tier (share/connect/format/mount/PR legs on the kernel nvmet target)");
    nvmet_smoke_step.dependOn(&run_nvmet_smoke.step);

    // ---- the nightly fuzz campaign (charter §11 "nightly fuzz"): the
    //      driver runs the build runner's fuzzer on every census target for
    //      `SQUEEZEFS_TEST_FUZZ_SECONDS` each, persists the corpus under
    //      `SQUEEZEFS_TEST_FUZZ_CORPUS_DIR`, saves + minimizes crashes and
    //      writes the campaign report (`src/gates/fuzz_campaign.zig`) ----
    const fuzz_campaign = b.addExecutable(.{
        .name = "fuzz-campaign",
        .root_module = b.createModule(.{
            .root_source_file = b.path("src/gates/fuzz_campaign.zig"),
            .target = target,
            .optimize = optimize,
            .imports = &.{.{ .name = "squeezefs", .module = squeezefs_mod }},
        }),
    });
    const run_fuzz_campaign = b.addRunArtifact(fuzz_campaign);
    run_fuzz_campaign.addArg("--zig");
    run_fuzz_campaign.addFileArg(.zig_exe);
    run_fuzz_campaign.addArg("--cache-dir");
    run_fuzz_campaign.addDirectoryArg(.cache_root);
    run_fuzz_campaign.addArg("--prefix");
    run_fuzz_campaign.addDirectoryArg(.{ .relative = .{ .base = .install_prefix } });
    run_fuzz_campaign.addArg("--repo");
    run_fuzz_campaign.addDirectoryArg(b.path("."));
    if (b.option([]const u8, "fuzz-filter", "Run only the fuzz targets whose name contains this text")) |f| run_fuzz_campaign.addArgs(&.{ "--filter", f });
    // A campaign's outcome is the fuzzer's, never a function of the build inputs.
    run_fuzz_campaign.has_side_effects = true;
    const fuzz_step = b.step("fuzz", "The nightly fuzz campaign: every fuzz target under the fuzzer for SQUEEZEFS_TEST_FUZZ_SECONDS each, persistent corpus, crash artefacts, report");
    fuzz_step.dependOn(&run_fuzz_campaign.step);

    // ---- fmt check ----
    const fmt_step = b.step("fmt", "Check formatting (zig fmt --check)");
    const fmt = b.addFmt(.{
        .paths = &.{ b.path("build.zig"), b.path("src"), b.path("tests") },
        .check = true,
    });
    fmt_step.dependOn(&fmt.step);

    // ---- the per-commit gate ----
    gate_step.dependOn(fmt_step);
    gate_step.dependOn(test_step);
    gate_step.dependOn(rails_step);
    gate_step.dependOn(smoke_step);
    gate_step.dependOn(bench_smoke_step);
    gate_step.dependOn(shim_smoke_step);
    gate_step.dependOn(nvmet_smoke_step);

    // ---- the release gate (charter §11): the per-commit gate plus the
    //      microbench compare against the committed baseline (wired above,
    //      after the gate); run with `-Drelease-gate=true` so environment
    //      skips fail ----
    gate_release_step.dependOn(gate_step);

    // ---- @anchor:sdk_build — the native SDK: libsqueezefs_sdk.{a,so} + the
    //      header + the C smoke (charter §7, §12; il_sdk §10.2–10.3) ----
    //
    // The export fns live in `src/sdk/abi.zig` INSIDE the library tree (they
    // import `ipc/client`, `ipc/wire`, `ipc/layout`, `core/errors` by
    // relative path), so the artifact's root module is a second instance of
    // the library tree (`sdk_core`: libc, NO zstd — the SDK must export only
    // `sqz_*`) reached through a one-line generated root that forces their
    // analysis: an `export fn` is emitted only from a file the root reaches,
    // which is also why the daemon binary never carries them.
    const sdk_step = b.step("sdk", "Build and install libsqueezefs_sdk.a, libsqueezefs_sdk.so and include/squeezefs_sdk.h (Linux)");
    const sdk_smoke_step = b.step("sdk-smoke", "Compile tests/sdk/c_smoke.c against the header, link the static SDK and run it against the in-process stub host (Linux)");
    if (target.result.os.tag == .linux) {
        const sdk_core = b.createModule(.{
            .root_source_file = b.path("src/squeezefs.zig"),
            .target = target,
            .optimize = optimize,
        });
        sdk_core.link_libc = true;
        // The root also names the train (`sqz_version` prints it; read
        // through `@import("root")` so the library instance never imports
        // the build identity).
        const sdk_root = b.addWriteFiles().add("sdk_root.zig", "pub const squeezefs_sdk_train: []const u8 = @import(\"build_options\").train;\ncomptime {\n    _ = @import(\"squeezefs\").sdk.abi;\n}\n");
        const sdk_mod = b.createModule(.{
            .root_source_file = sdk_root,
            .target = target,
            .optimize = optimize,
            .imports = &.{ .{ .name = "squeezefs", .module = sdk_core }, .{ .name = "build_options", .module = build_options_mod } },
        });
        sdk_mod.link_libc = true;
        const sdk_static = b.addLibrary(.{ .name = "squeezefs_sdk", .linkage = .static, .root_module = sdk_mod });
        // A static library does not bundle compiler-rt by default (only an
        // exe or a dynamic library does): a gcc/clang consumer would miss
        // the `__zig_*`/compiler-rt symbols the Zig code references.
        sdk_static.bundle_compiler_rt = true;
        // `.version` gives the default SONAME `libsqueezefs_sdk.so.1`.
        const sdk_shared = b.addLibrary(.{ .name = "squeezefs_sdk", .linkage = .dynamic, .root_module = sdk_mod, .version = .{ .major = 1, .minor = 0, .patch = 0 } });
        sdk_shared.installHeader(b.path("include/squeezefs_sdk.h"), "squeezefs_sdk.h");
        sdk_step.dependOn(&b.addInstallArtifact(sdk_static, .{}).step);
        sdk_step.dependOn(&b.addInstallArtifact(sdk_shared, .{}).step);

        // The C smoke: a Zig driver (the stub host out of the library) + the
        // C body compiled natively against the header, linked with the .a.
        const sdk_smoke = b.addExecutable(.{
            .name = "sdk-c-smoke",
            .root_module = b.createModule(.{
                .root_source_file = b.path("tests/sdk/c_smoke_main.zig"),
                .target = target,
                .optimize = optimize,
                .imports = &.{.{ .name = "squeezefs", .module = squeezefs_mod }},
            }),
        });
        sdk_smoke.root_module.addCSourceFile(.{ .file = b.path("tests/sdk/c_smoke.c"), .flags = &.{ "-std=c11", "-Wall", "-Wextra", "-Werror" } });
        sdk_smoke.root_module.addIncludePath(b.path("include"));
        sdk_smoke.root_module.linkLibrary(sdk_static);
        sdk_smoke.root_module.link_libc = true;
        const run_sdk_smoke = b.addRunArtifact(sdk_smoke);
        // Drives sockets, memfds and threads: its outcome is not a function
        // of the build inputs alone.
        run_sdk_smoke.has_side_effects = true;
        sdk_smoke_step.dependOn(&run_sdk_smoke.step);
        // The same program against the .so (SONAME resolution + the runtime
        // unload pin inside a shared object).
        const sdk_smoke_shared = b.addExecutable(.{
            .name = "sdk-c-smoke-shared",
            .root_module = b.createModule(.{
                .root_source_file = b.path("tests/sdk/c_smoke_main.zig"),
                .target = target,
                .optimize = optimize,
                .imports = &.{.{ .name = "squeezefs", .module = squeezefs_mod }},
            }),
        });
        sdk_smoke_shared.root_module.addCSourceFile(.{ .file = b.path("tests/sdk/c_smoke.c"), .flags = &.{ "-std=c11", "-Wall", "-Wextra", "-Werror" } });
        sdk_smoke_shared.root_module.addIncludePath(b.path("include"));
        sdk_smoke_shared.root_module.linkLibrary(sdk_shared);
        sdk_smoke_shared.root_module.link_libc = true;
        const run_sdk_smoke_shared = b.addRunArtifact(sdk_smoke_shared);
        run_sdk_smoke_shared.has_side_effects = true;
        sdk_smoke_step.dependOn(&run_sdk_smoke_shared.step);
        gate_step.dependOn(sdk_smoke_step);
    } else {
        sdk_step.dependOn(&b.addFail("the SDK is Linux-only (charter §1.2): build it on the venue").step);
        sdk_smoke_step.dependOn(&b.addFail("the SDK's C smoke is Linux-only (charter §1.2): run it on the venue").step);
    }

    // ---- dist: split debug info with a debuglink ----
    // objcopy --only-keep-debug squeezefs squeezefs.debug
    // objcopy --strip-debug --add-gnu-debuglink=squeezefs.debug squeezefs
    // Only meaningful under -Dprofile=dist; the step refuses otherwise so a
    // gate-profile binary is never shipped as dist.
    const dist_step = b.step("dist", "Install the dist artifact and split its debug info (requires -Dprofile=dist)");
    if (profile == .dist) {
        const keep_debug = b.addSystemCommand(&.{ "objcopy", "--only-keep-debug" });
        keep_debug.addFileArg(exe.getEmittedBin());
        const debug_file = keep_debug.addOutputFileArg("squeezefs.debug");
        const strip_link = b.addSystemCommand(&.{ "objcopy", "--strip-debug" });
        // The debuglink records the basename ("squeezefs.debug"); the install
        // below places both files side by side in bin/, which is where gdb and
        // the symbolizer look first.
        strip_link.addPrefixedFileArg("--add-gnu-debuglink=", debug_file);
        strip_link.addFileArg(exe.getEmittedBin());
        const stripped = strip_link.addOutputFileArg("squeezefs");
        dist_step.dependOn(&b.addInstallBinFile(stripped, "squeezefs").step);
        dist_step.dependOn(&b.addInstallBinFile(debug_file, "squeezefs.debug").step);
    } else {
        const refuse = b.addFail("the dist step requires -Dprofile=dist");
        dist_step.dependOn(&refuse.step);
    }
}

/// A test run step under the release-gate option: the harness knob the ONE
/// testkit reads (`src/core/testkit.zig`) rides the step's environment.
fn gated(run: *std.Build.Step.Run, release_gate: bool) *std.Build.Step.Run {
    if (release_gate) run.setEnvironmentVariable("SQUEEZEFS_TEST_RELEASE_GATE", "1");
    return run;
}

/// The release train. Bumped only as a release act (`scripts/release.sh`)
/// across all first-party components (charter §11 "Versioning"): this
/// constant, `build.zig.zon`'s `.version` (checked equal below), the SDK's
/// ABI words where the act says the ABI moved. The git commit is the
/// fine-grained identity; the daemon and shim refuse to pair across builds.
const train_version = "0.1.0";

comptime {
    const zon_version = @import("build.zig.zon").version;
    if (!std.mem.eql(u8, zon_version, train_version)) @compileError("build.zig.zon .version (" ++ zon_version ++ ") and build.zig train_version (" ++ train_version ++ ") disagree — the release act bumps both");
}

/// `git describe`-style identity: short commit, `-dirty` when the tree has
/// uncommitted changes, `unknown` outside a repository (a tarball build).
fn gitCommit(b: *std.Build) []const u8 {
    // The identity depends on the repository state, which the configure cache
    // cannot track (a working-tree edit without `git add` must still read
    // `-dirty`: the daemon and shim refuse to pair across builds, so a stale
    // "clean" here would be a lie). Poisoning re-runs this configure logic on
    // every `zig build` — two git subprocesses, milliseconds.
    b.graph.poisonCache();
    var code: u8 = 0;
    const raw = b.runAllowFail(&.{ "git", "rev-parse", "--short=12", "HEAD" }, &code, .ignore) catch return "unknown";
    const commit = std.mem.trim(u8, raw, " \n\r\t");
    if (commit.len == 0) return "unknown";
    const status = b.runAllowFail(&.{ "git", "status", "--porcelain", "--untracked-files=no" }, &code, .ignore) catch return commit;
    if (std.mem.trim(u8, status, " \n\r\t").len != 0) return b.fmt("{s}-dirty", .{commit});
    return commit;
}
