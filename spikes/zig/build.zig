const std = @import("std");

pub fn build(b: *std.Build) void {
    const target = b.standardTargetOptions(.{});
    const optimize = b.standardOptimizeOption(.{});
    const module = b.createModule(.{
        .root_source_file = b.path("src/main.zig"),
        .target = target,
        .optimize = optimize,
        .link_libc = true,
    });
    module.linkSystemLibrary("sqlite3", .{});
    const executable = b.addExecutable(.{
        .name = "distill-spike-zig",
        .root_module = module,
    });
    b.installArtifact(executable);

    const tests = b.addTest(.{ .root_module = module });
    const run_tests = b.addRunArtifact(tests);
    const test_step = b.step("test", "Run spike tests");
    test_step.dependOn(&run_tests.step);
}
