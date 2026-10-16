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
//! The binary (and the SDK libraries, and the shim) are ReleaseSafe in both
//! profiles; the TEST artifacts default to Debug (the lock-order rail and the
//! fiber-stack canary are debug-only rails). `-Doptimize=<mode>` /
//! `--release=<mode>` sets every artifact.
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
//!   zig build gate         — fmt + test + rails + smoke + bench-smoke + the
//!                            shim/nvmet/sdk smokes + suites-smoke: the
//!                            per-commit gate. Its test steps run side by
//!                            side (each stages its own pid-tagged rig); the
//!                            unit tests and the smoke run as shard sets of
//!                            one compiled binary each (`-Dunit-shards`,
//!                            `-Dsmoke-shards`; derived from the host), and a
//!                            `[venue-exclusive]` test runs after all of them.
//!   zig build costs        — regenerate the shard sets' cost tables
//!                            (tests/costs/) from this venue's gate runs.
//!   zig build suites-smoke — the ~2-minute pjdfstest subset against a fresh
//!                            mount (scripts/suites/pjdfstest.sh; Linux, root).
//!   zig build gate-release — pjdfstest + LTP + fstests -g auto, three clean
//!                            runs each under the adjudication ledgers
//!                            (docs/gates/*-exceptions.md): the release gate.
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
    // The product binary is ReleaseSafe in both profiles (the module doc);
    // the test artifacts default to Debug (the lock-order rail and the
    // fiber-stack canary are debug-only). `-Doptimize=<mode>` or
    // `--release=<mode>` sets EVERY artifact explicitly. Not
    // `standardOptimizeOption(.{ .preferred_optimize_mode })`: that returns
    // Debug unless `--release` is passed, so the suites and brackets ran a
    // Debug daemon whose canary fills and `undefined` memsets touched every
    // pool byte (rung 9's 3–5.6 GB RSS; `docs/evidence/rung-09-memory-footprint.md`).
    const explicit_optimize: ?std.builtin.Optimize = b.option(std.builtin.Optimize, "optimize", "Optimize mode for every artifact (default: the binary ReleaseSafe, the tests Debug)") orelse switch (b.graph.release_mode) {
        .off => null,
        .any, .safe => .safe,
        .fast => .fast,
        .small => .small,
    };
    const optimize: std.builtin.Optimize = explicit_optimize orelse .debug;
    const exe_optimize: std.builtin.Optimize = explicit_optimize orelse .safe;

    // ---- build identity: train, commit, profile (charter §11 "Versioning") ----
    const build_options = b.addOptions();
    build_options.addOption([]const u8, "profile", @tagName(profile));
    build_options.addOption([]const u8, "train", train_version);
    build_options.addOption([]const u8, "git_commit", gitCommit(b));
    const build_options_mod = build_options.createModule();

    // ---- the library module every artifact imports (no build identity):
    //      one instance per optimize mode — the test artifacts' (Debug by
    //      default) and the binary's (ReleaseSafe) ----
    // The bench baseline rig's two resolved paths (`cli/bench_baseline_rig.zig`):
    // the install prefix (the binary it drives) and the repo's bench/ dir.
    // Untracked: strings, no step dependency, so the library never cycles on
    // the binary it is part of.
    const bench_options = b.addOptions();
    bench_options.addOptionPathUntracked("install_prefix", .{ .relative = .{ .base = .install_prefix } });
    bench_options.addOptionPathUntracked("bench_dir", b.path("bench"));
    const bench_options_mod = bench_options.createModule();
    const squeezefs_mod = libraryModule(b, target, optimize, bench_options_mod, true);
    const squeezefs_exe_mod = if (exe_optimize == optimize) squeezefs_mod else libraryModule(b, target, exe_optimize, bench_options_mod, false);

    // ---- the one CLI binary (charter §8) ----
    const exe = b.addExecutable(.{
        .name = "squeezefs",
        .root_module = b.createModule(.{
            .root_source_file = b.path("src/main.zig"),
            .target = target,
            .optimize = exe_optimize,
            .imports = &.{
                .{ .name = "squeezefs", .module = squeezefs_exe_mod },
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
        // A CACHED step, so its output directory (`o/<digest>`) is a
        // function of every byte the shim can read: the compiler, the build
        // identity (`build_options`, an argument above) and every file under
        // src/ (a superset of the shim's transitive imports, which no
        // `zig build-lib` flag reports). Never `has_side_effects`: a
        // side-effect step's directory is the hash of its argv alone, which
        // every worktree sharing a cache dir computes alike — a neighbour's
        // build-lib rewrote the .so between this graph's build-lib and its
        // staging, and the shim smoke paired the daemon with another
        // build's shim (docs/evidence/rung-09-gates.md §11).
        build_lib.addFileInput(.zig_exe);
        addTreeInputs(b, build_lib, "src");
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
    gate_fresh = b.option(bool, "fresh", "Run every test step even when cached (the gate train's repeated runs of one binary)") orelse false;
    const release_gate = b.option(bool, "release-gate", "Release-gate mode: environment skips fail the gate (except needs_reference_hardware / other_step)") orelse false;

    // ---- the project test runner and the shard sets
    //      (docs/evidence/rung-10-gate-speed.md): every test binary but the
    //      rails' runs under tests/test_runner.zig — the stock runner plus
    //      `--shard`, the committed cost table (tests/costs/) and the
    //      heartbeat — and the two long runs, the unit tests and the smoke,
    //      run as N processes of ONE compiled binary the build runner
    //      executes side by side. A `[venue-exclusive]` test runs in its
    //      binary's exclusive step, after every other test step
    //      (`Shards.wireGate`). ----
    const test_runner: std.Build.Step.Compile.TestRunner = .{ .path = b.path("tests/test_runner.zig"), .mode = .server };
    const test_filters = b.option([]const []const u8, "test-filter", "Only run tests whose names contain this text (repeatable)") orelse &.{};
    const host = HostFacts.read();
    var shards: Shards = .{
        .b = b,
        .release_gate = release_gate,
        // A filtered run is a developer's loop over a few tests: one process,
        // every test (exclusive ones too), no table to judge.
        .sharded = test_filters.len == 0,
        .costs_step = b.step("costs", "Regenerate tests/costs/<binary>.<os>.tsv from this venue's shard runs (the partition's cost table; commit the result)"),
    };
    const unit_shard_count = b.option(u32, "unit-shards", "Processes for the unit tests (default: derived from this host's cores and memory)") orelse host.unitShards();
    const smoke_shard_count = b.option(u32, "smoke-shards", "Processes for the smoke (default: derived from this host's memory and cores)") orelse host.smokeShards();

    // ---- unit tests over the library module ----
    const unit_tests = b.addTest(.{
        .name = "squeezefs-unit",
        .root_module = squeezefs_mod,
        .filters = test_filters,
        .test_runner = test_runner,
    });
    // `.exclusive`: the unit binary holds every test, the smoke's
    // `[venue-exclusive]` rows too (there they skip, `other_step`).
    const unit_runs = shards.add(unit_tests, unit_shard_count, .{ .exclusive = true, .balanced = true });
    for (unit_runs.all) |run_unit_tests| {
        // The fuzz campaign's replay seam (`core/fuzzkit.zig`): the artefact path
        // rides the step's own environment so a replay is part of the run's
        // cache manifest — a probe with a new artefact is never a cached pass.
        if (b.graph.environ_map.get("SQUEEZEFS_TEST_FUZZ_REPLAY")) |p| run_unit_tests.setEnvironmentVariable("SQUEEZEFS_TEST_FUZZ_REPLAY", p);
    }
    const test_step = b.step("test", "Run unit tests");
    unit_runs.wire(test_step);

    // The gate steps are declared here so the steps below can hang off them;
    // their dependencies are wired where each piece lands.
    const gate_step = b.step("gate", "fmt + unit tests + rails + smoke + bench-smoke + shim/nvmet/sdk smokes + suites-smoke, side by side (the per-commit gate)");
    const gate_release_step = b.step("gate-release", "gate + bench-compare against the committed baseline + the external suites (pjdfstest, LTP, fstests -g auto; 3 clean runs each, adjudicated; Linux venue, root, hours) — run with -Drelease-gate=true: skips fail");

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
        // The stock runner: the rails test the shard plan
        // (tests/shard_plan.zig), which the project runner imports, and a
        // file belongs to one module.
    });
    const run_rails_tests = gated(b.addRunArtifact(rails_tests), release_gate);
    // The rails scan the source tree relative to the repository root.
    run_rails_tests.setCwd(b.path("."));
    const rails_step = b.step("rails", "Run static-rail convention tests");
    rails_step.dependOn(&run_rails_tests.step);

    // ---- the project runner's hang watchdog, proven (Linux: it reads
    //      /proc): tests/hangwatch_selftest.zig in terminal mode under a 2 s
    //      limit — two 1.2 s tests pass, the spinning third ends the run with
    //      the HUNG line, the thread dump and exit 124 ----
    if (target.result.os.tag == .linux) {
        const hang_tests = b.addTest(.{
            .name = "squeezefs-hangwatch-selftest",
            .root_module = b.createModule(.{
                .root_source_file = b.path("tests/hangwatch_selftest.zig"),
                .target = target,
                .optimize = optimize,
            }),
            .test_runner = test_runner,
        });
        const run_hang = gated(std.Build.Step.Run.create(b, "run hangwatch self-test"), release_gate);
        run_hang.addArtifactArg(hang_tests);
        run_hang.addArg("--hang-limit-ms=2000");
        run_hang.expectExitCode(124);
        for ([_][]const u8{
            "1/3 hangwatch_selftest.test.hangwatch: a quick test passes (1.2 s)...OK\n",
            "2/3 hangwatch_selftest.test.hangwatch: a second quick test passes (1.2 s)...OK\n",
            "\nHUNG TEST: \"hangwatch_selftest.test.hangwatch: a spinning test never returns\" ran ",
            " (cost table: no row; limit: 2.0s) [squeezefs-hangwatch-selftest]\n",
            " state R cpu ", // the spinning thread, from /proc/self/task
            "user stack of thread ",
            "in test.hangwatch: a spinning test never returns (hangwatch_selftest.zig)", // its user stack is in the spin
        }) |m| run_hang.expectStdErrMatch(m);
        rails_step.dependOn(&run_hang.step);
    }

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
            .test_runner = test_runner,
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
        .test_runner = test_runner,
    });
    // Off Linux every row skips (charter §1.2): one part, nothing to balance.
    const smoke_on_venue = target.result.os.tag == .linux;
    const smoke_runs = shards.add(smoke_tests, if (smoke_on_venue) smoke_shard_count else 1, .{ .exclusive = true, .balanced = smoke_on_venue });
    for (smoke_runs.all) |run_smoke_tests| {
        run_smoke_tests.setCwd(exe.getEmittedBinDirectory());
        // The smoke drives a real mount on the venue's kernel: its outcome is
        // not a function of the build inputs, so every gate run re-runs it.
        run_smoke_tests.has_side_effects = true;
    }
    const smoke_step = b.step("smoke", "Run the smoke (format/mount/umount/status against a real mount)");
    smoke_runs.wire(smoke_step);

    // ---- the bench smoke (charter §11: one iteration of every workload
    //      per commit): `squeezefs bench --smoke` against a mount the smoke
    //      creates, driven like the smoke above; skips loudly off the venue ----
    const bench_smoke_tests = b.addTest(.{
        .name = "squeezefs-bench-smoke",
        .root_module = squeezefs_mod,
        .filters = &.{"bench gate:"},
        .test_runner = test_runner,
    });
    const bench_smoke_runs = shards.add(bench_smoke_tests, 1, .{});
    for (bench_smoke_runs.all) |run_bench_smoke| {
        run_bench_smoke.setCwd(exe.getEmittedBinDirectory());
        run_bench_smoke.has_side_effects = true;
    }
    const bench_smoke_step = b.step("bench-smoke", "Run `squeezefs bench --smoke` against a mount the step creates (one iteration of every workload)");
    bench_smoke_runs.wire(bench_smoke_step);

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
        const record = b.addTest(.{ .name = "squeezefs-bench-baseline", .root_module = squeezefs_mod, .filters = &.{"bench baseline:"}, .test_runner = test_runner });
        const run_record = gated(b.addRunArtifact(record), release_gate);
        run_record.setEnvironmentVariable("SQUEEZEFS_TEST_BENCH_RIG", "1");
        run_record.has_side_effects = true;
        run_record.step.dependOn(b.getInstallStep());
        bench_baseline_step.dependOn(&run_record.step);
        const compare = b.addTest(.{ .name = "squeezefs-bench-compare", .root_module = squeezefs_mod, .filters = &.{"bench compare:"}, .test_runner = test_runner });
        const run_compare = gated(b.addRunArtifact(compare), release_gate);
        run_compare.setEnvironmentVariable("SQUEEZEFS_TEST_BENCH_RIG", "1");
        run_compare.has_side_effects = true;
        run_compare.step.dependOn(b.getInstallStep());
        bench_compare_step.dependOn(&run_compare.step);
        // In the release gate the judge runs AFTER the whole per-commit gate:
        // its numbers never share the venue with a smoke's mounts.
        const run_compare_after_gate = gated(b.addRunArtifact(compare), release_gate);
        run_compare_after_gate.setEnvironmentVariable("SQUEEZEFS_TEST_BENCH_RIG", "1");
        run_compare_after_gate.has_side_effects = true;
        run_compare_after_gate.step.dependOn(gate_step);
        run_compare_after_gate.step.dependOn(b.getInstallStep());
        gate_release_step.dependOn(&run_compare_after_gate.step);
    } else {
        bench_baseline_step.dependOn(&b.addFail("the bench baseline is Linux-only (charter §1.2): record it on the venue").step);
        bench_compare_step.dependOn(&b.addFail("the bench compare is Linux-only (charter §1.2): run it on the venue").step);
    }

    // ---- the shim smoke (Linux): the LD_PRELOAD battery with the staged
    //      .so beside the binary under test (its rig pid-tagged: it runs
    //      beside the other test steps) ----
    const shim_smoke_step = b.step("shim-smoke", "Run the LD_PRELOAD shim battery against a mount the step creates");
    if (shim_dir) |dir| {
        const shim_smoke_tests = b.addTest(.{
            .name = "squeezefs-shim-smoke",
            .root_module = squeezefs_mod,
            .filters = &.{"shim smoke:"},
            .test_runner = test_runner,
        });
        const shim_smoke_runs = shards.add(shim_smoke_tests, 1, .{});
        for (shim_smoke_runs.all) |run_shim_smoke| {
            run_shim_smoke.setCwd(dir);
            run_shim_smoke.has_side_effects = true;
        }
        shim_smoke_runs.wire(shim_smoke_step);
    }

    // ---- the nvmeof fidelity tier (charter §8, §11 release gate): the
    //      `nvmet fidelity:` legs through the binary against the venue's
    //      kernel nvmet target (loop + tcp), zero mocks; its subsystems,
    //      ports and namespace uuids are the run's own (`ops/nvmet/
    //      fidelity.zig`), so it runs beside the other test steps; skips
    //      loudly off the venue ----
    // `-Dnvmet-filter=<text>` narrows the tier to the legs whose name
    // contains the text (filters OR together, so the text replaces the
    // tier's prefix — spell `nvmet fidelity: <leg>`).
    const nvmet_filter = b.option([]const u8, "nvmet-filter", "Run only the nvmet fidelity legs whose name contains this text");
    const nvmet_smoke_tests = b.addTest(.{
        .name = "squeezefs-nvmet-smoke",
        .root_module = squeezefs_mod,
        .filters = &.{nvmet_filter orelse "nvmet fidelity:"},
        .test_runner = test_runner,
    });
    const nvmet_smoke_runs = shards.add(nvmet_smoke_tests, 1, .{});
    for (nvmet_smoke_runs.all) |run_nvmet_smoke| {
        run_nvmet_smoke.setCwd(exe.getEmittedBinDirectory());
        run_nvmet_smoke.has_side_effects = true;
    }
    const nvmet_smoke_step = b.step("nvmet-smoke", "Run the nvmeof fidelity tier (share/connect/format/mount/PR legs on the kernel nvmet target)");
    nvmet_smoke_runs.wire(nvmet_smoke_step);

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
    // The test binaries' runs come in through `shards.wireGate` below (the
    // exclusive class after every other test step); the top-level `test`,
    // `smoke`, … steps are a developer's.
    gate_step.dependOn(fmt_step);
    gate_step.dependOn(rails_step);

    // ---- the release gate (charter §11): the per-commit gate plus the
    //      microbench compare against the committed baseline (wired above,
    //      after the gate); run with `-Drelease-gate=true` so environment
    //      skips fail ----
    gate_release_step.dependOn(gate_step);

    // ---- the external conformance suites (charter §11 "Gates"): the
    //      harness is scripts/suites/*.sh on the Linux venue as root.
    //      `suites-smoke` = the ~2-minute pjdfstest subset against a fresh
    //      mount (one run; part of the gate); the release gate above also
    //      runs the full pjdfstest + LTP (fs, syscalls) + fstests `-g auto`
    //      with the 3-run discipline and the adjudication ledgers (hours;
    //      never per commit). Both drive the binary THIS build emitted
    //      (`--bin`), so a stale install can never be judged. ----
    const suites_smoke_step = b.step("suites-smoke", "pjdfstest gate subset (~2 min) against a fresh mount (Linux venue, root)");
    if (target.result.os.tag == .linux) {
        const smoke = b.addSystemCommand(&.{ "bash", "scripts/suites/pjdfstest.sh", "--bin" });
        smoke.addFileArg(exe.getEmittedBin());
        smoke.setEnvironmentVariable("PJD_SMOKE", "1");
        smoke.setEnvironmentVariable("SUITE_RUNS", "1");
        smoke.has_side_effects = true;
        // Beside the test steps (its set dir and mountpoint are pid-tagged),
        // before the exclusive class.
        shards.venue_steps.append(b.allocator, &smoke.step) catch @panic("OOM");
        suites_smoke_step.dependOn(&smoke.step);
        gate_step.dependOn(suites_smoke_step);
        inline for (.{ "pjdfstest.sh", "ltp.sh", "fstests.sh" }) |script| {
            const run = b.addSystemCommand(&.{ "bash", "scripts/suites/" ++ script, "--bin" });
            run.addFileArg(exe.getEmittedBin());
            run.has_side_effects = true;
            gate_release_step.dependOn(&run.step);
        }
    } else {
        suites_smoke_step.dependOn(&b.addFail("the external suites run only on the Linux venue (charter §1.2): scripts/venue zig build suites-smoke").step);
        gate_release_step.dependOn(&b.addFail("the release gate's external suites run only on the Linux venue (charter §1.2): scripts/venue zig build gate-release").step);
    }

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
        // Product artifacts like the binary: ReleaseSafe whatever the test mode.
        const sdk_core = b.createModule(.{
            .root_source_file = b.path("src/squeezefs.zig"),
            .target = target,
            .optimize = exe_optimize,
        });
        sdk_core.link_libc = true;
        // The root also names the train (`sqz_version` prints it; read
        // through `@import("root")` so the library instance never imports
        // the build identity).
        const sdk_root = b.addWriteFiles().add("sdk_root.zig", "pub const squeezefs_sdk_train: []const u8 = @import(\"build_options\").train;\ncomptime {\n    _ = @import(\"squeezefs\").sdk.abi;\n}\n");
        const sdk_mod = b.createModule(.{
            .root_source_file = sdk_root,
            .target = target,
            .optimize = exe_optimize,
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

        // The C body's flags. Zig supplies the libc headers and crt; a host GCC
        // installation is no input to this compile, yet clang's Linux driver probes
        // /usr/lib/gcc for one on every target and under -Werror its findings are
        // fatal (Ubuntu 26.04: gcc-16's libgcc dir beside gcc-15's libstdc++ →
        // -Wgcc-install-dir-libstdcxx). `--gcc-toolchain` names the prefix the probe
        // searches (relative: the compile runs in the build root): our header dir
        // holds no lib/gcc tree, so the probe finds nothing.
        const c_smoke_flags: []const []const u8 = &.{ "-std=c11", "-Wall", "-Wextra", "-Werror", "--gcc-toolchain=include" };
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
        sdk_smoke.root_module.addCSourceFile(.{ .file = b.path("tests/sdk/c_smoke.c"), .flags = c_smoke_flags });
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
        sdk_smoke_shared.root_module.addCSourceFile(.{ .file = b.path("tests/sdk/c_smoke.c"), .flags = c_smoke_flags });
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

    shards.wireGate(gate_step);

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

/// The library module (`src/squeezefs.zig`) with its imports, in one
/// optimize mode. `public` names it `squeezefs` for dependents (one
/// instance may carry the name).
///
/// The vendored zstd (vendor/zstd/VERSION; decision 0002 rows 118, 477): the
/// single-file amalgamation compiled into the library for compression AND
/// decompression (a static context of each per executor lane; std's zstd
/// decoder was 10× slower and is retired); the header translated into the
/// `zstd_c` module `data/transform.zig` imports. `ZSTD_STATIC_LINKING_ONLY`
/// for the static-context API (`ZSTD_initStaticCCtx/DCtx`: contexts that
/// malloc nothing per op, charter §12). Portable: the dev box runs the
/// codec tests too. The amalgamation is its own static library, not a C
/// source of the library module: the fuzz campaign (`zig build fuzz`,
/// charter §11) rebuilds the test module with `-ffuzz`, and clang's
/// coverage pass emits `__sanitizer_cov_trace_const_cmp*` hooks into C it
/// instruments that Zig's fuzzer runtime does not define (the link fails);
/// the decoders under fuzz are Zig, so zstd needs no instrumentation.
fn libraryModule(b: *std.Build, target: std.Build.ResolvedTarget, optimize: std.builtin.Optimize, bench_options_mod: *std.Build.Module, public: bool) *std.Build.Module {
    const options: std.Build.Module.CreateOptions = .{
        .root_source_file = b.path("src/squeezefs.zig"),
        .target = target,
        .optimize = optimize,
        .link_libc = true,
    };
    const mod = if (public) b.addModule("squeezefs", options) else b.createModule(options);
    mod.addImport("bench_options", bench_options_mod);
    const zstd_c = b.addTranslateC(.{
        .root_source_file = b.path("vendor/zstd/zstd.h"),
        .target = target,
        .optimize = optimize,
    });
    zstd_c.defineCMacro("ZSTD_STATIC_LINKING_ONLY", "1");
    zstd_c.addIncludePath(b.path("vendor/zstd"));
    mod.addImport("zstd_c", zstd_c.createModule());
    const zstd_mod = b.createModule(.{ .target = target, .optimize = optimize, .link_libc = true });
    zstd_mod.addCSourceFile(.{ .file = b.path("vendor/zstd/zstd.c"), .flags = &.{ "-DZSTD_STATIC_LINKING_ONLY", "-std=c99" } });
    mod.linkLibrary(b.addLibrary(.{ .name = "zstd", .linkage = .static, .root_module = zstd_mod }));
    return mod;
}

/// Every file under `sub` (relative to the build root) as a file input of
/// `run`, sorted (the cache manifest compares its file list in order, so a
/// walk order that moved would only cost a miss — sorted, it never does).
fn addTreeInputs(b: *std.Build, run: *std.Build.Step.Run, sub: []const u8) void {
    // The list is a function of the tree, which the configure cache cannot
    // track (a new file must join it): re-run this configure logic.
    b.graph.poisonCache();
    const io = b.graph.io;
    var dir = b.root.openDir(io, sub, .{ .iterate = true }) catch |e| std.debug.panic("cannot open {s}/ to list a step's inputs: {t}", .{ sub, e });
    defer dir.close(io);
    var walker = dir.walk(b.allocator) catch @panic("OOM");
    defer walker.deinit();
    var paths: std.ArrayList([]const u8) = .empty;
    while (walker.next(io) catch |e| std.debug.panic("cannot walk {s}/ to list a step's inputs: {t}", .{ sub, e })) |entry| {
        if (entry.kind != .file) continue;
        paths.append(b.allocator, b.pathJoin(&.{ sub, entry.path })) catch @panic("OOM");
    }
    std.mem.sort([]const u8, paths.items, {}, struct {
        fn lessThan(_: void, l: []const u8, r: []const u8) bool {
            return std.mem.lessThan(u8, l, r);
        }
    }.lessThan);
    for (paths.items) |p| run.addFileInput(b.path(p));
}

/// A test run step under the release-gate option: the harness knob the ONE
/// testkit reads (`src/core/testkit.zig`) rides the step's environment.
/// `-Dfresh`: every gated run step executes even when its binary and inputs are
/// cached — the gate train's repeated runs of ONE binary (charter §11: three consecutive
/// clean runs). Without it a repeat answers from the build cache and proves nothing; a
/// source edit cannot force it either, since the compiler caches by ZIR and a comment
/// changes none.
var gate_fresh: bool = false;

fn gated(run: *std.Build.Step.Run, release_gate: bool) *std.Build.Step.Run {
    if (release_gate) run.setEnvironmentVariable("SQUEEZEFS_TEST_RELEASE_GATE", "1");
    if (gate_fresh) run.has_side_effects = true;
    return run;
}

/// The host the build runs on, read at configure time: the shard counts are a
/// function of it (`-Dunit-shards` / `-Dsmoke-shards` win). The per-process
/// figures are the venue's measured peaks (docs/evidence/rung-10-gate-speed.md).
const HostFacts = struct {
    cpus: u32,
    ram_bytes: u64,

    /// A unit shard's resident bound: the whole unsharded unit run peaked at
    /// 1 GiB (one test's working set; a shard runs a subset), ×2 headroom.
    const unit_shard_rss: u64 = 2 << 30;
    /// A smoke shard's resident bound: the test process (≤ 0.8 GiB measured)
    /// plus a three-daemon row's daemons (≤ 0.6 GiB each measured), rounded up.
    const smoke_shard_rss: u64 = 3 << 30;

    fn read() HostFacts {
        return .{
            .cpus = @intCast(std.Thread.getCpuCount() catch 1),
            .ram_bytes = std.process.totalSystemMemory() catch 0,
        };
    }

    /// One unit shard per two cores: the heavy Debug tests (the property
    /// and scale suites) each keep a core busy, and past half the cores
    /// they stretch one another — 11 shards on the 11-core dev box ran its
    /// longest test 2.3× slower than alone and timing-bounded tests met an
    /// oversubscribed host — while beside them the binary's compile (the
    /// venue's critical path) needs its core; and no more than the RAM
    /// holds at `unit_shard_rss` each.
    fn unitShards(h: HostFacts) u32 {
        return @max(1, @min(h.cpus / 2, ramShare(h.ram_bytes, unit_shard_rss)));
    }

    /// One smoke shard per two cores: a storm row drives two daemons that
    /// each keep a core busy; and no more than the RAM holds at
    /// `smoke_shard_rss` each.
    fn smokeShards(h: HostFacts) u32 {
        return @max(1, @min(h.cpus / 2, ramShare(h.ram_bytes, smoke_shard_rss)));
    }

    fn ramShare(ram: u64, per: u64) u32 {
        return @intCast(@min(ram / per, std.math.maxInt(u32)));
    }
};

/// The shard sets (the test-runner section of `build`): `count` run steps of
/// ONE compiled test binary, each `--shard=I/N` of it, plus — for a binary
/// that declares the class — its exclusive step (`--shard=exclusive`: its
/// `[venue-exclusive]` tests). The exclusive step comes twice: once in the
/// binary's own top-level step, after its parts (a developer's
/// `zig build smoke`), once in the gate, after EVERY other test step of the
/// gate (`wireGate`). Each run balances by the binary's cost table for this
/// OS (`tests/costs/<binary>.<os>.tsv`, when committed) and writes what it
/// measured; `zig build costs` merges a binary's files into its table.
const Shards = struct {
    b: *std.Build,
    release_gate: bool,
    sharded: bool,
    costs_step: *std.Build.Step,
    /// Every non-exclusive test step of the gate.
    venue_steps: std.ArrayList(*std.Build.Step) = .empty,
    /// The gate's exclusive steps: after every venue step.
    gate_exclusive: std.ArrayList(*std.Build.Step.Run) = .empty,

    const Options = struct {
        /// The binary holds `[venue-exclusive]` tests: give it the steps.
        /// Without them a marked test refuses the run by name (it would
        /// otherwise run in no step at all).
        exclusive: bool = false,
        /// The parts balance by the binary's cost table and `costs`
        /// regenerates it (a set of one part has nothing to balance).
        balanced: bool = false,
    };

    const Set = struct {
        parts: []*std.Build.Step.Run,
        local_exclusive: ?*std.Build.Step.Run,
        gate_exclusive: ?*std.Build.Step.Run,
        all: []*std.Build.Step.Run,

        /// The binary's own top-level step: its parts and, after them, its
        /// exclusive step.
        fn wire(set: Set, step: *std.Build.Step) void {
            for (set.parts) |r| step.dependOn(&r.step);
            if (set.local_exclusive) |x| step.dependOn(&x.step);
        }
    };

    /// The caller configures every run in `all` alike (cwd, environment,
    /// side effects).
    fn add(s: *Shards, compile: *std.Build.Step.Compile, count: u32, options: Options) Set {
        const b = s.b;
        if (count == 0) std.debug.panic("{s}: a shard count of 0 runs nothing", .{compile.name});
        if (!s.sharded) {
            const run = gated(b.addRunArtifact(compile), s.release_gate);
            const runs = b.allocator.dupe(*std.Build.Step.Run, &.{run}) catch @panic("OOM");
            s.venue_steps.append(b.allocator, &run.step) catch @panic("OOM");
            return .{ .parts = runs, .local_exclusive = null, .gate_exclusive = null, .all = runs };
        }
        const table = b.fmt("tests/costs/{s}.{t}.tsv", .{ compile.name, compile.rootModuleTarget().os.tag });
        const have_table = options.balanced and if (b.root.access(b.graph.io, table, .{})) |_| true else |_| false;
        // The table's rows in byte order: one stable diff whichever host merges.
        const merge: ?*std.Build.Step.Run = if (options.balanced) b.addSystemCommand(&.{"sort"}) else null;
        if (merge) |m| m.setEnvironmentVariable("LC_ALL", "C");
        const n_exclusive: u32 = if (options.exclusive) 2 else 0;
        const all = b.allocator.alloc(*std.Build.Step.Run, count + n_exclusive) catch @panic("OOM");
        for (all, 0..) |*slot, i| {
            const run = gated(b.addRunArtifact(compile), s.release_gate);
            if (i < count) {
                run.addArg(b.fmt("--shard={d}/{d}", .{ i + 1, count }));
                if (!options.exclusive) run.addArg("--no-exclusive-step");
                if (count > 1) run.setName(b.fmt("run test {s} {d}/{d}", .{ compile.name, i + 1, count }));
                s.venue_steps.append(b.allocator, &run.step) catch @panic("OOM");
            } else {
                run.addArg("--shard=exclusive");
                run.setName(b.fmt("run test {s} exclusive", .{compile.name}));
            }
            if (have_table) run.addPrefixedFileArg("--costs=", b.path(table));
            // The local exclusive step is a developer's re-run of the gate's.
            if (merge) |m| if (i <= count) m.addFileArg(run.addPrefixedOutputFileArg("--costs-out=", "costs.tsv"));
            slot.* = run;
        }
        if (merge) |m| {
            const update = b.addUpdateSourceFiles();
            update.addCopyFileToSource(m.captureStdOut(.{}), table);
            s.costs_step.dependOn(&update.step);
        }
        const parts = all[0..count];
        var set: Set = .{ .parts = parts, .local_exclusive = null, .gate_exclusive = null, .all = all };
        if (options.exclusive) {
            set.gate_exclusive = all[count];
            set.local_exclusive = all[count + 1];
            for (parts) |p| set.local_exclusive.?.step.dependOn(&p.step);
            s.gate_exclusive.append(b.allocator, all[count]) catch @panic("OOM");
        }
        return set;
    }

    /// The gate runs every venue step, then the exclusive class alone.
    fn wireGate(s: *Shards, gate_step: *std.Build.Step) void {
        for (s.venue_steps.items) |v| gate_step.dependOn(v);
        for (s.gate_exclusive.items) |x| {
            for (s.venue_steps.items) |v| x.step.dependOn(v);
            gate_step.dependOn(&x.step);
        }
    }
};

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
