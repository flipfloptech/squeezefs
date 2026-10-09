//! The shim's binary census (charter §12; decision 0003 §F "Shim
//! `-z nodelete`"): `libsqueezefs_il.so` in the cwd (build.zig stages it
//! beside the binary under test) is read as ELF64 — readelf-free — and must
//! carry the SONAME, `DT_FLAGS_1 NODELETE`, a `DT_NEEDED` set of libc alone,
//! and a defined dynamic symbol set EQUAL to the ONE table in
//! `src/shim/interpose.zig` (every interposition present, nothing of Zig's
//! or the client library's exported).

const std = @import("std");
const elf = std.elf;
const squeezefs = @import("squeezefs");
const interpose = squeezefs.shim.interpose;

const SO_NAME = "libsqueezefs_il.so";

const Census = struct {
    soname: ?[]const u8 = null,
    flags_1: u64 = 0,
    needed: std.ArrayList([]const u8) = .empty,
    exports: std.ArrayList([]const u8) = .empty,

    fn deinit(self: *Census, gpa: std.mem.Allocator) void {
        self.needed.deinit(gpa);
        self.exports.deinit(gpa);
    }
};

fn cstr(bytes: []const u8, off: usize) []const u8 {
    if (off >= bytes.len) return "";
    const end = std.mem.indexOfScalarPos(u8, bytes, off, 0) orelse bytes.len;
    return bytes[off..end];
}

fn section(bytes: []const u8, ehdr: *const elf.Elf64_Ehdr, i: usize) !*const elf.Elf64_Shdr {
    const off = ehdr.e_shoff + i * ehdr.e_shentsize;
    if (off + @sizeOf(elf.Elf64_Shdr) > bytes.len) return error.Truncated;
    return @ptrCast(@alignCast(bytes.ptr + off));
}

fn parse(gpa: std.mem.Allocator, bytes: []align(8) const u8) !Census {
    if (bytes.len < @sizeOf(elf.Elf64_Ehdr)) return error.Truncated;
    const ehdr: *const elf.Elf64_Ehdr = @ptrCast(bytes.ptr);
    if (!std.mem.eql(u8, ehdr.e_ident[0..4], elf.MAGIC)) return error.NotElf;
    var c: Census = .{};
    errdefer c.deinit(gpa);
    var i: usize = 0;
    while (i < ehdr.e_shnum) : (i += 1) {
        const sh = try section(bytes, ehdr, i);
        switch (sh.sh_type) {
            @backingInt(elf.SHT.DYNAMIC) => {
                const strtab = try section(bytes, ehdr, sh.sh_link);
                const strs = bytes[strtab.sh_offset..][0..strtab.sh_size];
                const n = sh.sh_size / @sizeOf(elf.Elf64_Dyn);
                var k: usize = 0;
                while (k < n) : (k += 1) {
                    const d: *const elf.Elf64_Dyn = @ptrCast(@alignCast(bytes.ptr + sh.sh_offset + k * @sizeOf(elf.Elf64_Dyn)));
                    switch (d.d_tag) {
                        elf.DT_SONAME => c.soname = cstr(strs, d.d_val),
                        elf.DT_FLAGS_1 => c.flags_1 = d.d_val,
                        elf.DT_NEEDED => try c.needed.append(gpa, cstr(strs, d.d_val)),
                        else => {},
                    }
                }
            },
            @backingInt(elf.SHT.DYNSYM) => {
                const strtab = try section(bytes, ehdr, sh.sh_link);
                const strs = bytes[strtab.sh_offset..][0..strtab.sh_size];
                const n = sh.sh_size / @sizeOf(elf.Elf64_Sym);
                var k: usize = 0;
                while (k < n) : (k += 1) {
                    const s: *const elf.Elf64_Sym = @ptrCast(@alignCast(bytes.ptr + sh.sh_offset + k * @sizeOf(elf.Elf64_Sym)));
                    if (s.st_shndx == 0) continue; // undefined (imported)
                    const name = cstr(strs, s.st_name);
                    if (name.len == 0) continue;
                    try c.exports.append(gpa, name);
                }
            },
            else => {},
        }
    }
    return c;
}

test "libsqueezefs_il.so: SONAME, DT_FLAGS_1 NODELETE, libc the only DT_NEEDED, and the defined dynamic symbols == the ONE table (every interposition present, nothing else exported)" {
    const gpa = std.testing.allocator;
    const io = std.testing.io;
    const bytes = std.Io.Dir.cwd().readFileAllocOptions(io, SO_NAME, gpa, .limited(64 << 20), .of(u64), null) catch |err| switch (err) {
        error.FileNotFound => return error.SkipZigTest,
        else => return err,
    };
    defer gpa.free(bytes);
    var c = try parse(gpa, bytes);
    defer c.deinit(gpa);
    try std.testing.expectEqualStrings(SO_NAME, c.soname orelse return error.NoSoname);
    try std.testing.expect(c.flags_1 & elf.DF_1_NODELETE != 0);
    for (c.needed.items) |n| try std.testing.expect(std.mem.startsWith(u8, n, "libc.so") or std.mem.startsWith(u8, n, "ld-linux"));
    // Every table row is exported; nothing outside the table is.
    var violations: std.Io.Writer.Allocating = .init(gpa);
    defer violations.deinit();
    for (interpose.table) |row| {
        var found = false;
        for (c.exports.items) |e| {
            if (std.mem.eql(u8, e, row.name)) found = true;
        }
        if (!found) try violations.writer.print("  missing export: {s}\n", .{row.name});
    }
    for (c.exports.items) |e| {
        var known = false;
        for (interpose.table) |row| {
            if (std.mem.eql(u8, e, row.name)) known = true;
        }
        if (!known) try violations.writer.print("  stray export: {s}\n", .{e});
    }
    if (violations.written().len != 0) {
        std.debug.print("shim export census:\n{s}", .{violations.written()});
        return error.ExportCensusViolated;
    }
    try std.testing.expectEqual(interpose.table.len, c.exports.items.len);
}
