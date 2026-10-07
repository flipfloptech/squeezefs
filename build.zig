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
    squeezefs_mod.addCSourceFile(.{ .file = b.path("vendor/zstd/zstd.c"), .flags = &.{ "-DZSTD_STATIC_LINKING_ONLY", "-std=c99" } });
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

    // ---- unit tests over the library module ----
    const test_filters = b.option([]const []const u8, "test-filter", "Only run tests whose names contain this text (repeatable)") orelse &.{};
    const unit_tests = b.addTest(.{
        .name = "squeezefs-unit",
        .root_module = squeezefs_mod,
        .filters = test_filters,
    });
    const run_unit_tests = b.addRunArtifact(unit_tests);
    const test_step = b.step("test", "Run unit tests");
    test_step.dependOn(&run_unit_tests.step);

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
    const run_rails_tests = b.addRunArtifact(rails_tests);
    // The rails scan the source tree relative to the repository root.
    run_rails_tests.setCwd(b.path("."));
    const rails_step = b.step("rails", "Run static-rail convention tests");
    rails_step.dependOn(&run_rails_tests.step);

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
    const run_smoke_tests = b.addRunArtifact(smoke_tests);
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
    const run_bench_smoke = b.addRunArtifact(bench_smoke_tests);
    run_bench_smoke.setCwd(exe.getEmittedBinDirectory());
    run_bench_smoke.has_side_effects = true;
    // After the smoke: the two drive real mounts on one venue and must
    // never share it (the build graph runs independent steps in parallel).
    run_bench_smoke.step.dependOn(&run_smoke_tests.step);
    const bench_smoke_step = b.step("bench-smoke", "Run `squeezefs bench --smoke` against a mount the step creates (one iteration of every workload)");
    bench_smoke_step.dependOn(&run_bench_smoke.step);

    // ---- fmt check ----
    const fmt_step = b.step("fmt", "Check formatting (zig fmt --check)");
    const fmt = b.addFmt(.{
        .paths = &.{ b.path("build.zig"), b.path("src"), b.path("tests") },
        .check = true,
    });
    fmt_step.dependOn(&fmt.step);

    // ---- the per-commit gate ----
    const gate_step = b.step("gate", "fmt + unit tests + rails + smoke + bench-smoke (the per-commit gate)");
    gate_step.dependOn(fmt_step);
    gate_step.dependOn(test_step);
    gate_step.dependOn(rails_step);
    gate_step.dependOn(smoke_step);
    gate_step.dependOn(bench_smoke_step);

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

/// The release train. Bumped only as a release act across all first-party
/// components (charter §11 "Versioning"). The git commit is the fine-grained
/// identity; the daemon and shim refuse to pair across builds.
const train_version = "0.1.0";

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
