//! sqzhash128 — the ONE hash of the on-disk format and the wire (charter
//! §5.1 "Checksums everywhere"; decision 0003 §M).
//!
//! PROVENANCE: ported bit-for-bit on 2026-10-11 from the owner's
//! `zig_hash_bench` project (`src/sqzhash.zig`, the hash's only definition;
//! SMHasher3-clean in full — all 252 tests with `--extra` — both as 128 bits
//! and on the low 64 bits alone, verification values `0xAB49D6E0` and
//! `0xD6703A98`). THIS FILE IS THE FORMAT'S DEFINITION: any change to its
//! output is a format change (a version bump, a decision 0002 row, new
//! pinned vectors below). The vectors at the bottom were produced by the
//! source implementation and are asserted forever.
//!
//! A keyed 128-bit non-cryptographic hash built from AES rounds, designed
//! for low latency on short, dependent inputs (names and keys) and for
//! load-bound throughput on large ones (block checksums).
//!
//! Construction (F is one AES round without a key, `absorb(a, b) = F(a ^ b)`):
//!
//! - A `Key` expands a 128-bit secret once into 16 distinct lane states and
//!   four round keys. Keys are cheap to make but meant to be reused, for
//!   instance one per mount; the secret keeps lane states unpredictable so
//!   names chosen by users cannot be aimed at one bucket.
//! - Up to 16 bytes are one block, padded with the length.
//! - 17 to 256 bytes (every name up to NAME_MAX): each block is absorbed
//!   into its own state under its own key, half the blocks read from the
//!   front of the input and half from the back. No state absorbs two blocks.
//! - Larger inputs use 8 lanes up to 1 KiB and 16 beyond, each lane absorbing
//!   one block per chunk with one round between blocks; the last chunk is
//!   aligned to the end of the input and overlaps the previous one. This is
//!   load-bound, but crafted differences can cancel between two blocks of a
//!   lane (about one try in 127, whatever the key), so these sizes suit
//!   checksums of data an attacker does not choose, not flood-resistant
//!   tables.
//! - States combine in one step: all but two pass two more keyed rounds, in
//!   parallel, and everything is xored. Two differences can only cancel
//!   after three rounds each, which takes a 2^-128 coincidence; with two
//!   rounds each it would take about 2^-32, which SMHasher's sparse keys
//!   find (see `mergeLanes`).
//! - The finalizer is two keyed rounds, so every input byte and the length
//!   pass through at least four rounds, most bytes through five.
//!
//! The 128-bit result's low 64 bits are suitable on their own, for tables,
//! filters and the format's 8-byte checksum fields.
//!
//! The round function has two paths — the AArch64 AES instructions when the
//! target has them, else `std.crypto.core.aes`'s round (AES-NI on x86_64, a
//! software round elsewhere). Both compute the same textbook AES round, and
//! the tests prove it: the whole hash is instantiated on BOTH paths
//! (`Engine(.native)` / `Engine(.portable)`) and compared on every vector,
//! and the primitive itself is checked against a table-driven AES round.

const std = @import("std");
const builtin = @import("builtin");
const assert = std.debug.assert;
const aes = std.crypto.core.aes;

const Vec = @Vector(16, u8); // one AES block: 16 bytes
/// The AES block: 16 bytes, the unit every rule below counts in.
const block_len = 16;
const zero: Vec = @splat(0);

/// Inputs up to this size use one lane.
const small_max = block_len;
/// Inputs up to this size make every block a leaf of a merge tree.
const tree_max = 16 * block_len;
/// Inputs up to this size use 8 lanes, beyond that 16.
const medium_max = 64 * block_len;
/// The bulk lane count: 16 loads and 32 AES per chunk — the L1 load limit.
const large_lanes = 16;
const large_chunk_len = large_lanes * block_len;

/// Which round-function path an `Engine` is built on. `.native` is what the
/// product runs; `.portable` exists so the tests can run both on one machine.
const Path = enum { native, portable };

// ---------------------------------------------------------------------------
// Path-independent primitives.

/// The value `state ^ pending`, with the xor left for the next round to do:
/// F(state ^ pending) is `absorb(state, pending)`, which on AArch64 takes the
/// xor off the critical path.
const Pending = struct {
    state: Vec,
    pending: Vec = zero,

    inline fn value(p: Pending) Vec {
        return p.state ^ p.pending;
    }
};

/// Xor of `terms`, as a balanced tree so the compiler can use EOR3.
inline fn xorAll(comptime n: usize, terms: [n]Vec) Vec {
    if (n == 1) return terms[0];
    if (n == 2) return terms[0] ^ terms[1];
    if (n == 3) return terms[0] ^ terms[1] ^ terms[2];
    // Three-way split: EOR3 takes three operands.
    const third = (n + 2) / 3;
    const rest = n - third;
    const half = rest / 2;
    return xorAll(third, terms[0..third].*) ^ xorAll(half, terms[third..][0..half].*) ^ xorAll(rest - half, terms[third + half ..][0 .. rest - half].*);
}

inline fn load(bytes: []const u8, offset: usize) Vec {
    return @as(*align(1) const Vec, @ptrCast(bytes[offset..][0..block_len])).*;
}

/// The length in both 64-bit halves: one DUP, where a zero upper half takes
/// MOVI plus INS.
inline fn lenVec(len: u64) Vec {
    const halves: @Vector(2, u64) = @splat(std.mem.nativeToLittle(u64, len));
    return @bitCast(halves);
}

inline fn toInt(v: Vec) u128 {
    return std.mem.littleToNative(u128, @bitCast(v));
}

inline fn fromInt(x: u128) Vec {
    return @bitCast(std.mem.nativeToLittle(u128, x));
}

// ---------------------------------------------------------------------------
// Key schedule constants

/// Round constants for the key schedule, from SplitMix64 seeded with the
/// bytes "SQZHASH\0": nothing up the sleeve, and distinct per lane.
const schedule_constants: [2][key_vectors]Vec = blk: {
    @setEvalBranchQuota(10_000);
    var state: u64 = 0x53515A4841534800; // "SQZHASH\0" as a big-endian u64
    var out: [2][key_vectors]Vec = undefined;
    for (&out) |*round| {
        for (round) |*c| {
            const halves: @Vector(2, u64) = .{ splitMix64(&state), splitMix64(&state) };
            c.* = @bitCast(halves);
        }
    }
    break :blk out;
};

/// SplitMix64 (Steele, Lea & Flood 2014): the published constants verbatim.
fn splitMix64(state: *u64) u64 {
    state.* +%= 0x9E3779B97F4A7C15;
    var z = state.*;
    z = (z ^ (z >> 30)) *% 0xBF58476D1CE4E5B9;
    z = (z ^ (z >> 27)) *% 0x94D049BB133111EB;
    return z ^ (z >> 31);
}

/// Lane 0's state, the length key, the final key, two middle keys, lanes 1 to 15.
const key_vectors = 5 + (large_lanes - 1);

// ---------------------------------------------------------------------------
// Short inputs. The 16-byte load past the end mirrors gxhash and the Rust
// crate: memory is mapped in whole pages, so a read that stays within one
// page of a valid byte cannot fault.

const over_read_page: ?usize = switch (builtin.os.tag) {
    .linux, .windows, .freebsd, .netbsd, .openbsd, .dragonfly => std.heap.page_size_min,
    else => if (builtin.os.tag.isDarwin()) std.heap.page_size_min else null,
};

const zero_block: Vec align(block_len) = zero;

/// The input bytes followed by padding bytes equal to the length. Reads a
/// whole block when that stays within the page, which keeps the bytes in
/// vector registers; near the end of a page it falls back to in-bounds loads.
inline fn partialBlock(input: []const u8) Vec {
    const len = input.len;
    assert(len < block_len);
    if (over_read_page) |page| {
        // An empty slice may hold any pointer, so it reads a static block.
        const src: [*]const u8 = if (len == 0) @ptrCast(&zero_block) else input.ptr;
        // Offsets in the page below `page - block_len` leave room for the
        // whole block, and they are the ones without every bit of it set.
        comptime assert(std.math.isPowerOfTwo(page));
        if (~@intFromPtr(src) & (page - block_len) != 0 and !std.debug.inValgrind()) {
            // Volatile, so the compiler does not treat reading past the slice
            // as undefined behavior. The extra bytes are replaced below.
            const block = @as(*align(1) const volatile Vec, @ptrCast(src)).*;
            const len_vec: Vec = @splat(@intCast(len));
            return @select(u8, std.simd.iota(u8, block_len) < len_vec, block, len_vec);
        }
    }
    return partialBlockInRegisters(input);
}

/// `partialBlock` built from in-bounds loads.
inline fn partialBlockInRegisters(input: []const u8) Vec {
    const len = input.len;
    assert(len < block_len);
    // The length byte replicated into every byte of a u64: the pad.
    const pad = 0x0101010101010101 * @as(u64, len);
    if (len >= 8) {
        // The last 8 bytes overlap the first 8; shift out the overlap.
        const tail = if (len == 8) 0 else read64(input, len - 8) >> @intCast(8 * (16 - len));
        return vecFromHalves(read64(input, 0), tail | (pad << @intCast(8 * (len - 8))));
    }
    var lo: u64 = 0;
    if (len >= 4) {
        // Two overlapping 4-byte reads cover 4 to 7 bytes.
        const tail = @as(u64, read32(input, len - 4)) >> @intCast(8 * (8 - len));
        lo = read32(input, 0) | (tail << 32);
    } else if (len > 0) {
        // First, middle and last byte cover 1 to 3 bytes.
        lo = @as(u64, input[0]) | (@as(u64, input[len / 2]) << @intCast(8 * (len / 2))) |
            (@as(u64, input[len - 1]) << @intCast(8 * (len - 1)));
    }
    return vecFromHalves(lo | (pad << @intCast(8 * len)), pad);
}

inline fn vecFromHalves(lo: u64, hi: u64) Vec {
    const halves: @Vector(2, u64) = .{ std.mem.nativeToLittle(u64, lo), std.mem.nativeToLittle(u64, hi) };
    return @bitCast(halves);
}

inline fn read32(bytes: []const u8, offset: usize) u32 {
    return std.mem.readInt(u32, bytes[offset..][0..4], .little);
}

inline fn read64(bytes: []const u8, offset: usize) u64 {
    return std.mem.readInt(u64, bytes[offset..][0..8], .little);
}

// ---------------------------------------------------------------------------
// The software AES round (FIPS 197 §5.1.1–5.1.3: SubBytes, ShiftRows,
// MixColumns; no AddRoundKey). It runs only in comptime evaluation — the
// format key's expansion — and serves the tests as the textbook both
// hardware paths must agree with.

/// Multiplication in GF(2^8) modulo the AES polynomial x^8 + x^4 + x^3 + x + 1.
fn gmul(a_in: u8, b_in: u8) u8 {
    var a = a_in;
    var b = b_in;
    var p: u8 = 0;
    for (0..8) |_| {
        if (b & 1 != 0) p ^= a;
        a = xtime(a);
        b >>= 1;
    }
    return p;
}

/// Multiplication by x in GF(2^8): shift, reduce by the AES polynomial.
fn xtime(x: u8) u8 {
    return (x << 1) ^ (if (x & 0x80 != 0) @as(u8, 0x1B) else 0);
}

/// The S-box from its definition: the field inverse (`a^254`, square-and-
/// multiply), then the affine map `x ^ rotl(x,1) ^ rotl(x,2) ^ rotl(x,3) ^
/// rotl(x,4) ^ 0x63`.
fn buildSbox() [256]u8 {
    var out: [256]u8 = undefined;
    for (0..256) |i| {
        const a: u8 = @intCast(i);
        var inv: u8 = 1;
        var base = a;
        var e: u8 = 254; // a^254 = a^-1 in GF(2^8) (a^255 = 1)
        while (e != 0) : (e >>= 1) {
            if (e & 1 != 0) inv = gmul(inv, base);
            base = gmul(base, base);
        }
        if (a == 0) inv = 0;
        out[i] = inv ^ std.math.rotl(u8, inv, 1) ^ std.math.rotl(u8, inv, 2) ^ std.math.rotl(u8, inv, 3) ^ std.math.rotl(u8, inv, 4) ^ 0x63; // FIPS 197 §5.1.1 (5.2)
    }
    return out;
}

const sbox: [256]u8 = blk: {
    // 256 inverses × 14 field multiplications × 8 steps.
    @setEvalBranchQuota(100_000);
    break :blk buildSbox();
};

/// MixColumns(ShiftRows(SubBytes(state))) on the column-major 16-byte block.
fn softRound(in: Vec) Vec {
    const bytes: [16]u8 = in;
    var s: [16]u8 = undefined;
    for (0..16) |i| s[i] = sbox[bytes[i]];
    // ShiftRows: row r of the column-major state rotates left by r.
    var r: [16]u8 = undefined;
    for (0..4) |c| {
        for (0..4) |row| r[row + 4 * c] = s[row + 4 * ((c + row) % 4)]; // 4 rows × 4 columns
    }
    // MixColumns: the circulant (2 3 1 1) matrix per column.
    var out: [16]u8 = undefined;
    for (0..4) |c| {
        const a0 = r[4 * c];
        const a1 = r[4 * c + 1];
        const a2 = r[4 * c + 2];
        const a3 = r[4 * c + 3];
        out[4 * c] = xtime(a0) ^ (xtime(a1) ^ a1) ^ a2 ^ a3;
        out[4 * c + 1] = a0 ^ xtime(a1) ^ (xtime(a2) ^ a2) ^ a3;
        out[4 * c + 2] = a0 ^ a1 ^ xtime(a2) ^ (xtime(a3) ^ a3);
        out[4 * c + 3] = (xtime(a0) ^ a0) ^ a1 ^ a2 ^ xtime(a3);
    }
    return out;
}

// ---------------------------------------------------------------------------
// The hash over one round-function path.

fn Engine(comptime path: Path) type {
    return struct {
        const Self = @This();

        const use_arm_aes = path == .native and builtin.cpu.arch == .aarch64 and builtin.cpu.has(.aarch64, .aes);

        /// F(lane ^ block): SubBytes, ShiftRows, MixColumns of the xor.
        inline fn absorb(lane: Vec, block: Vec) Vec {
            // Comptime evaluation (the format key's expansion) has no AES
            // instructions: the table round, proven equal to both paths below.
            if (@inComptime()) return softRound(lane ^ block);
            if (use_arm_aes) {
                // AESE xors its operands before SubBytes, so the xor comes for free.
                return asm (
                    \\ aese  %[out].16b, %[block].16b
                    \\ aesmc %[out].16b, %[out].16b
                    : [out] "=w" (-> Vec),
                    : [lane] "0" (lane),
                      [block] "w" (block),
                );
            }
            // std.crypto's `encrypt` matches x86 AESENC: F(x) ^ round_key.
            const x: [16]u8 = lane ^ block;
            const z: [16]u8 = zero;
            return aes.Block.fromBytes(&x).encrypt(aes.Block.fromBytes(&z)).toBytes();
        }

        /// Combines `lane_count` states (leaves or lanes) into one for the
        /// finalizer. Every state passes two more keyed rounds, except the last,
        /// which passes one, and the results are xored. The length goes into the
        /// key of the first state's first extra round.
        ///
        /// Why these round counts, learned from SMHasher and SMHasher3:
        ///
        /// - Two inputs that reach an xor after exactly two rounds each can cancel
        ///   with probability about 2^-32: a difference confined to one AES column
        ///   after the first round is determined after the second by just four
        ///   S-boxes. After one round each it is about 2^-7 (the cancellation that
        ///   breaks GxHash above 128 bytes). After three rounds the last round has
        ///   16 active S-boxes and a coincidence is about 2^-128. Both tree shapes
        ///   tried before (binary and 4-way joins) put several leaves at exactly two
        ///   rounds, and SMHasher's sparse keys found dozens of full collisions. Here
        ///   exactly one state arrives with two rounds.
        /// - Three rounds between an input byte and the output is not quite
        ///   random: SMHasher3 saw 3 to 17% too many 32-bit collisions on structured
        ///   small keys. Four rounds is its floor, so with the two-round finalizer
        ///   every state gets at least two rounds here.
        /// - The same goes for the length. Injected two rounds before the output,
        ///   hashes of inputs that differ only in length had xor differences with
        ///   256 possible values per 32-bit window. It now gets four rounds.
        ///
        /// The extra rounds run in parallel, so latency is 2 rounds plus the xor
        /// tree whatever the count; the cost is 2 AES per input instead of 1.
        inline fn mergeLanes(comptime lane_count: usize, lanes: [lane_count]Vec, len: u64, key: *const Key) Pending {
            comptime assert(lane_count >= 2 and lane_count <= 16);
            var terms: [lane_count]Vec = undefined;
            terms[0] = absorb(absorb(lanes[0], lenKey(len, key)), key.mid(1));
            inline for (1..lane_count - 1) |i| terms[i] = absorb(absorb(lanes[i], key.mid(0)), key.mid(1));
            terms[lane_count - 1] = absorb(lanes[lane_count - 1], key.mid(0));
            return .{ .state = terms[0], .pending = xorAll(lane_count - 1, terms[1..].*) };
        }

        /// The length, folded into the key of the round that absorbs it.
        inline fn lenKey(len: u64, key: *const Key) Vec {
            return lenVec(len) ^ key.final(0);
        }

        /// Two keyed rounds.
        inline fn finalize(h: Pending, key: *const Key) Vec {
            return absorb(absorb(h.state, h.pending), key.final(1));
        }

        /// The expanded form of a secret. Make one per secret and reuse it; the
        /// expansion is 40 AES rounds.
        pub const Key = struct {
            /// Lane 0's initial state, the length key, the final key, the two keys
            /// of the extra rounds in `mergeLanes`, then the initial states of lanes
            /// 1 to 15 (all lane states distinct). Inputs up to 16 bytes need only
            /// the first three vectors.
            v: [key_vectors]Vec align(64),

            pub fn init(secret: u128) Key {
                // A comptime expansion: 40 software rounds of 16 S-box
                // lookups, 16 row moves and 4 column mixes.
                @setEvalBranchQuota(20_000);
                const s = fromInt(secret);
                var key: Key = undefined;
                inline for (0..key_vectors) |i| key.v[i] = expand(s, i);
                return key;
            }

            inline fn final(key: *const Key, comptime j: usize) Vec {
                return key.v[1 + j];
            }

            inline fn mid(key: *const Key, comptime j: usize) Vec {
                return key.v[3 + j];
            }

            /// Initial states for `n` lanes (or keys for `n` tree leaves): lane 0's
            /// is `v[0]`, the rest follow the key vectors above.
            inline fn lanes(key: *const Key, comptime n: usize) [n]Vec {
                var out: [n]Vec = undefined;
                out[0] = key.v[0];
                inline for (1..n) |i| out[i] = key.v[4 + i];
                return out;
            }

            /// Two rounds, so no two outputs are related by a constant xor.
            inline fn expand(s: Vec, comptime i: usize) Vec {
                return absorb(absorb(s, schedule_constants[0][i]), schedule_constants[1][i]);
            }
        };

        // -------------------------------------------------------------------
        // Compression

        inline fn absorbChunk(comptime lane_count: usize, lanes: *[lane_count]Vec, chunk: *const [lane_count * block_len]u8) void {
            inline for (0..lane_count) |i| lanes[i] = absorb(lanes[i], load(chunk, i * block_len));
        }

        /// 17 to 256 bytes: every block is absorbed into its own state under its own
        /// key (2, 4, 6, 8, 12, or 16 of them), so no state absorbs two blocks and
        /// repeated blocks give different states; see `mergeLanes` for how the
        /// states combine. Half the blocks are read from the front and half from the
        /// back, so the halves overlap when the length is not `n` blocks; the 6- and
        /// 12-block cases keep that overlap under a quarter of the work. The cost
        /// against a lane that absorbs block after block is two extra rounds per
        /// block, which shows in throughput from 129 bytes up.
        inline fn compressTree(comptime n: usize, input: []const u8, key: *const Key) Pending {
            const len = input.len;
            const half = n / 2;
            assert(len > half * block_len and len <= n * block_len);
            const keys = key.lanes(n);
            var lanes: [n]Vec = undefined;
            const back = len - half * block_len;
            inline for (0..half) |i| {
                lanes[i] = absorb(keys[i], load(input, i * block_len));
                lanes[half + i] = absorb(keys[half + i], load(input, back + i * block_len));
            }
            return mergeLanes(n, lanes, len, key);
        }

        /// Absorbs whole 16-lane chunks, four per iteration: with 16 loads and 32
        /// AES instructions per 256 bytes the loop is bound by instruction issue,
        /// so loop overhead shows directly.
        inline fn absorbLargeChunks(lanes: *[large_lanes]Vec, data: []const u8) void {
            assert(data.len % large_chunk_len == 0);
            const unroll = 4;
            var pos: usize = 0;
            while (pos + unroll * large_chunk_len <= data.len) : (pos += unroll * large_chunk_len) {
                inline for (0..unroll) |c| absorbChunk(large_lanes, lanes, data[pos + c * large_chunk_len ..][0..large_chunk_len]);
            }
            while (pos < data.len) : (pos += large_chunk_len) {
                absorbChunk(large_lanes, lanes, data[pos..][0..large_chunk_len]);
            }
        }

        /// Each lane absorbs one block per chunk of `lane_count` blocks. The last
        /// chunk is aligned on the end of the input and may overlap the previous one.
        inline fn compressLanes(comptime lane_count: usize, input: []const u8, key: *const Key) Pending {
            const chunk_len = lane_count * block_len;
            assert(input.len > chunk_len);
            var lanes: [lane_count]Vec = key.lanes(lane_count);
            const last = input.len - chunk_len;
            if (lane_count == large_lanes) {
                // Every chunk that starts before the last one.
                const whole = ((last + chunk_len - 1) / chunk_len) * chunk_len;
                absorbLargeChunks(&lanes, input[0..whole]);
            } else {
                var pos: usize = 0;
                while (pos < last) : (pos += chunk_len) absorbChunk(lane_count, &lanes, input[pos..][0..chunk_len]);
            }
            absorbChunk(lane_count, &lanes, input[last..][0..chunk_len]);
            return mergeLanes(lane_count, lanes, input.len, key);
        }

        fn compress(input: []const u8, key: *const Key) Vec {
            // Six tree sizes and two lane counts unroll here at comptime.
            @setEvalBranchQuota(20_000);
            const len = input.len;
            // A balanced decision tree with the common name lengths (up to 32 bytes)
            // nearest the root: a chain of size checks costs a cycle of front-end
            // time per taken branch, which showed as 4 to 5 cycles per hash at the
            // end of the chain.
            const h: Pending = if (len <= 2 * block_len)
                if (len <= small_max)
                    // One block, then a round with the length: four rounds in all
                    // with the finalizer, the floor SMHasher3 can accept.
                    .{ .state = absorb(absorb(key.v[0], if (len == block_len) load(input, 0) else partialBlock(input)), lenKey(len, key)) }
                else
                    compressTree(2, input, key)
            else if (len <= tree_max)
                if (len <= 8 * block_len) // the six tree sizes: 2, 4, 6, 8, 12, 16 blocks
                    if (len <= 4 * block_len) // 4 blocks
                        compressTree(4, input, key) // 4 leaves
                    else if (len <= 6 * block_len) compressTree(6, input, key) else compressTree(8, input, key) // 6 or 8 leaves
                else if (len <= 12 * block_len) compressTree(12, input, key) else compressTree(16, input, key) // 12 or 16 leaves
            else if (len <= medium_max)
                compressLanes(8, input, key) // 8 lanes to 1 KiB
            else
                compressLanes(large_lanes, input, key);
            return finalize(h, key);
        }

        // -------------------------------------------------------------------
        // Public API

        /// One-shot sqzhash128 of `input` under `key`.
        pub fn hash(key: *const Key, input: []const u8) u128 {
            return toInt(compress(input, key));
        }

        /// sqzhash64: the low 64 bits of sqzhash128. The state is 128 bits wide
        /// whatever the output, so this costs the same as `hash` minus one register
        /// move; it passes SMHasher3 on its own.
        pub fn hash64(key: *const Key, input: []const u8) u64 {
            return @truncate(hash(key, input));
        }

        /// Streaming sqzhash128. Produces the same result as `hash` regardless of
        /// how the input is split across `update` calls.
        ///
        /// The state is the construction's MINIMUM (charter R5: a per-object
        /// state is derived and bounded — the staging ring keeps one per live
        /// cell, the fibers one per frame): the key by POINTER (the format key
        /// is a comptime constant, a name key lives once per mount; a copy was
        /// 320 B per state), and ONE of two phases:
        ///
        /// - `prefix` while the input is ≤ `medium_max` (1 KiB): every regime
        ///   below 16 lanes (one block, the 2…16-leaf tree, 8 lanes) is a
        ///   function of the WHOLE input in a shape the 16-lane state cannot
        ///   produce, so the bytes themselves are the state and `final` hashes
        ///   them one-shot.
        /// - `lanes` beyond: the 16 lane states and the last `chunk_len` bytes
        ///   seen. The final chunk is aligned on the END of the input and may
        ///   start at any byte, so `final` takes it from that window; a chunk
        ///   is absorbed only once a byte FOLLOWS it, which proves it is not
        ///   the final one.
        pub const Hasher = struct {
            const chunk_len = large_chunk_len;
            /// The one-shot regimes end here; the lanes begin at the next byte.
            const prefix_len = medium_max;

            const Phase = union(enum) {
                /// `[0, total_len)` of the input, `total_len ≤ prefix_len`.
                prefix: [prefix_len]u8,
                lanes: Lanes,
            };

            const Lanes = struct {
                lanes: [large_lanes]Vec,
                /// The last `chunk_len` input bytes, in order (`total_len >
                /// prefix_len > chunk_len` in this phase). Its tail past
                /// `absorbed(total_len)` — 1 to `chunk_len` bytes — awaits a
                /// byte that follows; its head is the previous chunk's end.
                window: [chunk_len]u8,
            };

            comptime {
                // The window must reach back into the previous chunk, and the
                // prefix must hold three whole chunks plus the window at the
                // switch (`absorbLargeChunks` over `prefix_len - chunk_len`).
                assert(prefix_len >= 2 * chunk_len);
                assert(prefix_len % chunk_len == 0);
                // The derived minimum: the larger phase + the union's tag, both
                // at the lane vector's alignment, the length and the key pointer.
                assert(prefix_len > @sizeOf(Lanes));
                assert(@sizeOf(Hasher) <= prefix_len + @alignOf(Vec) + @sizeOf(u64) + @sizeOf(*const Key));
            }

            key: *const Key,
            total_len: u64 = 0,
            phase: Phase = .{ .prefix = undefined },

            /// `key` must outlive the hasher (it is read by `update` and `final`).
            pub fn initWithKey(key: *const Key) Hasher {
                return .{ .key = key };
            }

            /// Bytes absorbed into the lanes after `total` bytes of input: every
            /// whole chunk that a byte follows (the one holding the last byte
            /// may be the end-aligned final chunk, so it waits).
            inline fn absorbed(total: u64) u64 {
                return ((total - 1) / chunk_len) * chunk_len;
            }

            pub fn update(self: *Hasher, input: []const u8) void {
                switch (self.phase) {
                    .prefix => |*prefix| {
                        const have: usize = @intCast(self.total_len);
                        if (input.len <= prefix_len - have) {
                            @memcpy(prefix[have..][0..input.len], input);
                            self.total_len += input.len;
                            return;
                        }
                        // The input carries the total past the prefix: fill it,
                        // absorb every chunk but the last (a byte follows each)
                        // and switch phases with the last chunk as the window —
                        // through locals, since the two phases share storage.
                        const fill = prefix_len - have;
                        @memcpy(prefix[have..], input[0..fill]);
                        var lanes = self.key.lanes(large_lanes);
                        absorbLargeChunks(&lanes, prefix[0 .. prefix_len - chunk_len]);
                        const window: [chunk_len]u8 = prefix[prefix_len - chunk_len ..].*;
                        self.phase = .{ .lanes = .{ .lanes = lanes, .window = window } };
                        self.total_len = prefix_len;
                        self.updateLanes(input[fill..]);
                    },
                    .lanes => self.updateLanes(input),
                }
            }

            fn updateLanes(self: *Hasher, input: []const u8) void {
                const st = &self.phase.lanes;
                const before = self.total_len;
                const after = before + input.len;
                const done_before = absorbed(before);
                const done_after = absorbed(after);
                if (done_after > done_before) {
                    var lanes = st.lanes;
                    // The first chunk to absorb completes the held bytes with the
                    // input's head; the rest lie whole inside the input.
                    const held: usize = @intCast(before - done_before);
                    if (held == chunk_len) {
                        absorbChunk(large_lanes, &lanes, &st.window);
                    } else {
                        var first: [chunk_len]u8 = undefined;
                        @memcpy(first[0..held], st.window[chunk_len - held ..]);
                        @memcpy(first[held..], input[0 .. chunk_len - held]);
                        absorbChunk(large_lanes, &lanes, &first);
                    }
                    absorbLargeChunks(&lanes, input[chunk_len - held .. @intCast(done_after - before)]);
                    st.lanes = lanes;
                }
                // The window slides: the last `chunk_len` bytes of window ‖ input.
                if (input.len >= chunk_len) {
                    st.window = input[input.len - chunk_len ..][0..chunk_len].*;
                } else {
                    std.mem.copyForwards(u8, st.window[0 .. chunk_len - input.len], st.window[input.len..]);
                    @memcpy(st.window[chunk_len - input.len ..], input);
                }
                self.total_len = after;
            }

            /// Returns the hash of everything passed to `update` so far. Does not
            /// modify the hasher, so more data may be added afterwards.
            pub fn final(self: *const Hasher) u128 {
                @setEvalBranchQuota(20_000);
                switch (self.phase) {
                    .prefix => |*prefix| return hash(self.key, prefix[0..@intCast(self.total_len)]),
                    .lanes => |*st| {
                        // Every chunk a byte follows is in the lanes; the window IS
                        // the end-aligned final chunk.
                        var lanes = st.lanes;
                        absorbChunk(large_lanes, &lanes, &st.window);
                        return toInt(finalize(mergeLanes(large_lanes, lanes, self.total_len, self.key), self.key));
                    },
                }
            }

            /// The low 64 bits of `final`: the format's 8-byte checksum projection.
            pub fn finalLow64(self: *const Hasher) u64 {
                return @truncate(self.final());
            }
        };
    };
}

/// The product's engine: `native.Key` (the expanded form of a 128-bit
/// secret, made once per secret and reused — per mount for the name key, at
/// comptime for the format key in `core/checksum.zig`), `native.hash` (one-
/// shot sqzhash128), `native.hash64` (its low 64 bits) and `native.Hasher`
/// (streaming: `initWithKey`, `update`, `final` / `finalLow64`).
pub const native = Engine(.native);

// ---------------------------------------------------------------------------
// Tests. There is no external reference; the fixed vectors below were
// produced by the source implementation and guard against accidental change.

const testing = std.testing;
const testkit = @import("testkit.zig");

fn fillTestData(buf: []u8) void {
    for (buf, 0..) |*b, i| b.* = @truncate(i *% 31 +% 7);
}

const test_lengths = [_]usize{
    0,    1,    2,    3,    7,    8,    9,    15,   16,   17,     31,  32,  33,   47,   48,   49,
    63,   64,   65,   127,  128,  129,  255,  256,  257,  511,    512, 513, 1023, 1024, 1025, 2047,
    2048, 2049, 4095, 4096, 4097, 4352, 8191, 8192, 8193, 20_000,
};

/// The source implementation's own vectors (key 0 unless noted), kept so
/// the port is proven bit-exact with it.
const source_vectors = [_]struct { len: usize, hash: u128 }{
    .{ .len = 1, .hash = 0xe2f09255bfd8db50c46913475784ff00 },
    .{ .len = 7, .hash = 0x20c1b01c282969bb52e1fe5534981df1 },
    .{ .len = 15, .hash = 0xebb86122092fcffb0092f90e06f9809a },
    .{ .len = 16, .hash = 0x31e1e3c93e8e50f698350533f5c2efe6 },
    .{ .len = 17, .hash = 0x15573bf637d78609a819cb553eba2477 },
    .{ .len = 32, .hash = 0x53fb083281ec19c17c62d450a1acbdcc },
    .{ .len = 33, .hash = 0x334291c0725f0fa0b6608a0a1a5a269c },
    .{ .len = 48, .hash = 0xe3701f88582f5e46027cfd553c228b11 },
    .{ .len = 64, .hash = 0x7c40b908685e23f9dcd6f3e7d4f15509 },
    .{ .len = 65, .hash = 0x9c00e691aa2fc4d03b60479763f21279 },
    .{ .len = 96, .hash = 0xb4ba7b68c4b81566affd946eacf91bfc },
    .{ .len = 97, .hash = 0x903536bc361ca37a7becd96bf8f37005 },
    .{ .len = 128, .hash = 0xab67f5faf88383e321f31097f7f90f8e },
    .{ .len = 129, .hash = 0x0a756c3386c48c6390ebbc18e1fc513b },
    .{ .len = 160, .hash = 0x4c4701d6be636e9075f4ab5c202c2f23 },
    .{ .len = 176, .hash = 0x4ac474719a35b536191c2fcde0ac2b4a },
    .{ .len = 192, .hash = 0x757ebae6fae1e99350b84fdbf9db2bbb },
    .{ .len = 193, .hash = 0xfeff6aeec1f381be3e75ee8ac57f84bd },
    .{ .len = 255, .hash = 0x54565e2fb05eefc73f876be0d5cbc59c },
    .{ .len = 256, .hash = 0x397aa3ad802bdfa3d1aa660a608e6d3a },
    .{ .len = 257, .hash = 0x32b0122f085f4259b7eca3b9baba9382 },
    .{ .len = 1024, .hash = 0x29612950bec54edfa031722d5c297de0 },
    .{ .len = 1025, .hash = 0x39b949cc52dd02b1965d1e3ee06388ba },
    .{ .len = 4242, .hash = 0x5c236a104a8abe20e21395322e0d30bb },
};

/// The format's pinned vectors (decision 0003 §M): `fillTestData` inputs of
/// every size class under the FORMAT KEY (`core/checksum.zig`) and under a
/// volume seed widened by the name-key rule (zero-extended), the u128 and its
/// low 64 bits. Produced once by the source implementation on 2026-10-11.
pub const PinnedVector = struct { len: usize, h128: u128, low64: u64 };

pub const pinned_format_key: u128 = 0x5351_5A46_5320_464F_524D_4154_204B_4559;
pub const pinned_seed_key: u128 = 0xF1D5_A000_0000_0004;

pub const pinned_format_vectors = [_]PinnedVector{
    .{ .len = 0, .h128 = 0xf5d2588c13349a3b60faebc5c5bda514, .low64 = 0x60faebc5c5bda514 },
    .{ .len = 1, .h128 = 0xd2deda4aed7026f7f363f3549f60655d, .low64 = 0xf363f3549f60655d },
    .{ .len = 7, .h128 = 0x2a22d6a44ed9a8a573aa5b134b6776e7, .low64 = 0x73aa5b134b6776e7 },
    .{ .len = 16, .h128 = 0x0da917d38e8d1a0406727f2a6adecca9, .low64 = 0x06727f2a6adecca9 },
    .{ .len = 17, .h128 = 0x48e726bf1d6f0b5a7a6964f648de2469, .low64 = 0x7a6964f648de2469 },
    .{ .len = 48, .h128 = 0x21c1edec94401e5c6379e4046ed863e3, .low64 = 0x6379e4046ed863e3 },
    .{ .len = 100, .h128 = 0xd6ba1538337c6e32556dae81be9f0dfd, .low64 = 0x556dae81be9f0dfd },
    .{ .len = 128, .h128 = 0xebe627c9055195999b2de675a7e39440, .low64 = 0x9b2de675a7e39440 },
    .{ .len = 256, .h128 = 0x2f80bbb3733dbc8fd341b5e61f2d2d94, .low64 = 0xd341b5e61f2d2d94 },
    .{ .len = 257, .h128 = 0x421216d48c596a2f9ea1848b158e4fa3, .low64 = 0x9ea1848b158e4fa3 },
    .{ .len = 1024, .h128 = 0xfab35eea68311f2f8becb504d90ffac6, .low64 = 0x8becb504d90ffac6 },
    .{ .len = 4096, .h128 = 0x205bc4c59b3a297443f67887f27d66b8, .low64 = 0x43f67887f27d66b8 },
    .{ .len = 65537, .h128 = 0x7325c4baee250450daab10772c3c01cc, .low64 = 0xdaab10772c3c01cc },
    .{ .len = 262144, .h128 = 0xce66013faa999bc9a61e7f074079227e, .low64 = 0xa61e7f074079227e },
};

pub const pinned_seed_vectors = [_]PinnedVector{
    .{ .len = 0, .h128 = 0x66debbdbc3515a8b68ce44dfef5cf7ad, .low64 = 0x68ce44dfef5cf7ad },
    .{ .len = 1, .h128 = 0x63b86e1cf08e348a5e6233baa1628810, .low64 = 0x5e6233baa1628810 },
    .{ .len = 7, .h128 = 0xeae72d6243b26d6439da84f8c0f7e5ae, .low64 = 0x39da84f8c0f7e5ae },
    .{ .len = 16, .h128 = 0xb767f7f46ee5b5b2ba02e86aa194c025, .low64 = 0xba02e86aa194c025 },
    .{ .len = 17, .h128 = 0xa496c14689148c5d0e409f8f8744dae7, .low64 = 0x0e409f8f8744dae7 },
    .{ .len = 48, .h128 = 0x02efa4130dd5d018f619a8e5caf56009, .low64 = 0xf619a8e5caf56009 },
    .{ .len = 100, .h128 = 0x285ae8f7dc85a2b91aed521de0988ae9, .low64 = 0x1aed521de0988ae9 },
    .{ .len = 128, .h128 = 0xbe41577fd53e378185b3e0d37666ca07, .low64 = 0x85b3e0d37666ca07 },
    .{ .len = 256, .h128 = 0xdb1ad2483242a2086eb49df982cb4e59, .low64 = 0x6eb49df982cb4e59 },
    .{ .len = 257, .h128 = 0xbdf4076623bd09d2529b1e294f010bcc, .low64 = 0x529b1e294f010bcc },
    .{ .len = 1024, .h128 = 0x16f93d06b4a65ee508d929b95bcf737d, .low64 = 0x08d929b95bcf737d },
    .{ .len = 4096, .h128 = 0xdcecfdef13a4442dc7a08a8dce6763c2, .low64 = 0xc7a08a8dce6763c2 },
    .{ .len = 65537, .h128 = 0x7707a9edeb7f273a3d323995155a023a, .low64 = 0x3d323995155a023a },
    .{ .len = 262144, .h128 = 0x991aaab1d622bda0187e18f0ebbc8cb9, .low64 = 0x187e18f0ebbc8cb9 },
};

/// The largest pinned input: 256 KiB, the default node size.
const pinned_max_len: usize = 262_144;

fn expectPinned(comptime E: type, secret: u128, vectors: []const PinnedVector, buf: []const u8) !void {
    const key: E.Key = .init(secret);
    for (vectors) |v| {
        errdefer std.debug.print("secret={x} len={d}\n", .{ secret, v.len });
        const h = E.hash(&key, buf[0..v.len]);
        try testing.expectEqual(v.h128, h);
        try testing.expectEqual(v.low64, @as(u64, @truncate(h)));
        try testing.expectEqual(v.low64, E.hash64(&key, buf[0..v.len]));
        var s: E.Hasher = .initWithKey(&key);
        s.update(buf[0..v.len]);
        try testing.expectEqual(v.h128, s.final());
        try testing.expectEqual(v.low64, s.finalLow64());
    }
}

test "pinned vectors: the format key and a zero-extended volume seed, every size class, u128 and low 64, one-shot and streaming — asserted forever (a change here is a format change)" {
    const buf = try testing.allocator.alloc(u8, pinned_max_len);
    defer testing.allocator.free(buf);
    fillTestData(buf);
    try expectPinned(native, pinned_format_key, &pinned_format_vectors, buf);
    try expectPinned(native, pinned_seed_key, &pinned_seed_vectors, buf);
}

test "path equality: the AArch64 AES path and the std.crypto round produce identical output on every pinned vector and every test length under three keys" {
    const portable = Engine(.portable);
    const buf = try testing.allocator.alloc(u8, pinned_max_len);
    defer testing.allocator.free(buf);
    fillTestData(buf);
    // Both engines against the pins.
    try expectPinned(portable, pinned_format_key, &pinned_format_vectors, buf);
    try expectPinned(portable, pinned_seed_key, &pinned_seed_vectors, buf);
    // Both engines against each other on every test length.
    for ([_]u128{ 0, 42, 0xdeadbeef_cafebabe_0123456789abcdef }) |secret| {
        const kn: native.Key = .init(secret);
        const kp: portable.Key = .init(secret);
        try testing.expectEqualSlices(Vec, &kn.v, &kp.v);
        for (test_lengths) |len| {
            errdefer std.debug.print("secret={x} len={d}\n", .{ secret, len });
            try testing.expectEqual(native.hash(&kn, buf[0..len]), portable.hash(&kp, buf[0..len]));
        }
    }
}

test "the round primitive on both paths IS the textbook AES round (the FIPS 197 S-box, ShiftRows, MixColumns — the software round comptime evaluation runs) on 10,000 random blocks; a comptime key expansion equals the runtime one" {
    // FIPS 197 figure 7: S[00] = 63, S[01] = 7c, S[53] = ed, S[ff] = 16.
    try testing.expectEqual(@as(u8, 0x63), sbox[0x00]);
    try testing.expectEqual(@as(u8, 0x7c), sbox[0x01]);
    try testing.expectEqual(@as(u8, 0xed), sbox[0x53]);
    try testing.expectEqual(@as(u8, 0x16), sbox[0xff]);
    // FIPS 197 appendix B, round 1: the state after SubBytes/ShiftRows/
    // MixColumns of the round-0 output `19 3d e3 be a0 f4 e2 2b 9a c6 8d 2a
    // e9 f8 48 08` is `04 66 81 e5 e0 cb 19 9a 48 f8 d3 7a 28 06 26 4c`.
    const fips_in: Vec = .{ 0x19, 0x3d, 0xe3, 0xbe, 0xa0, 0xf4, 0xe2, 0x2b, 0x9a, 0xc6, 0x8d, 0x2a, 0xe9, 0xf8, 0x48, 0x08 };
    const fips_out: Vec = .{ 0x04, 0x66, 0x81, 0xe5, 0xe0, 0xcb, 0x19, 0x9a, 0x48, 0xf8, 0xd3, 0x7a, 0x28, 0x06, 0x26, 0x4c };
    try testing.expectEqual(fips_out, softRound(fips_in));
    try testing.expectEqual(fips_out, native.absorb(fips_in, zero));
    const portable = Engine(.portable);
    var prng: std.Random.DefaultPrng = .init(0x5A5A);
    const r = prng.random();
    for (0..10_000) |_| {
        var a: [16]u8 = undefined;
        var b: [16]u8 = undefined;
        r.bytes(&a);
        r.bytes(&b);
        const want = softRound(@as(Vec, a) ^ @as(Vec, b));
        try testing.expectEqual(want, native.absorb(a, b));
        try testing.expectEqual(want, portable.absorb(a, b));
    }
    const comptime_key: native.Key = comptime .init(pinned_format_key);
    const runtime_key: native.Key = .init(pinned_format_key);
    try testing.expectEqualSlices(Vec, &runtime_key.v, &comptime_key.v);
}

test "source vectors: the port is bit-exact with the owner's implementation (key 0, key 42, the empty string, Hello World)" {
    const key: native.Key = .init(0);
    try testing.expectEqual(0xb83407438b5dce146627a19a78f30461, native.hash(&key, ""));
    try testing.expectEqual(0x3109f96f5061a0a70da109a89e74fe6e, native.hash(&key, "Hello World"));
    var buf: [4242]u8 = undefined;
    fillTestData(&buf);
    for (source_vectors) |v| {
        errdefer std.debug.print("len={d}\n", .{v.len});
        try testing.expectEqual(v.hash, native.hash(&key, buf[0..v.len]));
    }
    const k42: native.Key = .init(42);
    try testing.expectEqual(0x4e80850b4df84bbe930885e8ca33a8cc, native.hash(&k42, &buf));
}

test "hash64 is the low half of hash" {
    const key: native.Key = .init(9);
    var buf: [300]u8 = undefined;
    fillTestData(&buf);
    for ([_]usize{ 0, 1, 16, 17, 64, 200, 300 }) |len| {
        try testing.expectEqual(@as(u64, @truncate(native.hash(&key, buf[0..len]))), native.hash64(&key, buf[0..len]));
    }
}

test "streaming matches one-shot for assorted lengths and chunk sizes" {
    var buf: [20_000]u8 = undefined;
    fillTestData(&buf);
    for ([_]u128{ 0, 42, 0xdeadbeef_cafebabe_0123456789abcdef }) |secret| {
        const key: native.Key = .init(secret);
        for (test_lengths) |len| {
            const expected = native.hash(&key, buf[0..len]);
            // Splits around the lane chunk (the window's slide and the held
            // chunk), around the 1 KiB phase switch (a fill that lands short
            // of it, on it and past it), and whole-input.
            for ([_]usize{ 1, 15, 16, 17, 255, 256, 257, 1000, 1023, 1024, 1025, 4096, 4097, 5000, 20_000 }) |chunk| {
                errdefer std.debug.print("len={d} chunk={d}\n", .{ len, chunk });
                var h: native.Hasher = .initWithKey(&key);
                var rest = buf[0..len];
                while (rest.len > 0) {
                    const n = @min(chunk, rest.len);
                    h.update(rest[0..n]);
                    rest = rest[n..];
                }
                try testing.expectEqual(expected, h.final());
                try testing.expectEqual(@as(u64, @truncate(expected)), h.finalLow64());
            }
        }
    }
}

test "the streaming state is the construction's minimum (charter R5 — one lives per staged cell): the key by pointer, and the larger of the two phases — the whole input while it is ≤ 1 KiB (every regime below 16 lanes is a function of all of it), else 16 lane states + the last chunk" {
    // Phase 1: the one-shot regimes (one block, the 2…16-leaf tree, 8 lanes)
    // end at `medium_max`; the bytes themselves are the state until then.
    const prefix_phase = medium_max;
    // Phase 2: 16 lane states and the end-aligned final chunk's window.
    const lane_phase = large_lanes * block_len + large_chunk_len;
    try testing.expect(prefix_phase > lane_phase);
    // The union carries the larger phase and its tag, rounded to the lane
    // vector's alignment; beside it the length and the key pointer — and
    // nothing else (the tie is equality: 1056 B, against the 4736 B of the
    // 16-chunk buffer plus a key copy it replaces).
    const bound = prefix_phase + @alignOf(Vec) + @sizeOf(u64) + @sizeOf(*const native.Key);
    try testing.expectEqual(@as(usize, 1056), bound);
    try testing.expectEqual(bound, @sizeOf(native.Hasher));
    // The key is shared, never copied: the state is smaller than one expansion
    // plus the lane phase, which any copy would exceed.
    try testing.expect(@sizeOf(native.Key) == 20 * block_len);
    try testing.expect(@sizeOf(native.Hasher) < @sizeOf(native.Key) + prefix_phase);
}

test "final does not disturb streaming state" {
    var buf: [10_000]u8 = undefined;
    fillTestData(&buf);
    const key: native.Key = .init(7);
    var h: native.Hasher = .initWithKey(&key);
    var pos: usize = 0;
    while (pos < buf.len) : (pos += 250) {
        h.update(buf[pos..][0..250]);
        try testing.expectEqual(native.hash(&key, buf[0 .. pos + 250]), h.final());
    }
}

test "lengths, keys, and single-byte changes all give distinct hashes" {
    const key: native.Key = .init(0);
    const other: native.Key = .init(1);
    var seen: std.AutoHashMap(u128, usize) = .init(testing.allocator);
    defer seen.deinit();
    // Zero-filled inputs of every length up to 300 are all distinct.
    const zeros: [300]u8 = @splat(0);
    for (0..zeros.len + 1) |len| {
        const h = native.hash(&key, zeros[0..len]);
        try testing.expectEqual(null, try seen.fetchPut(h, len));
        try testing.expect(h != native.hash(&other, zeros[0..len]));
    }
    // Changing any one byte of a 300-byte input changes the hash.
    var buf: [300]u8 = undefined;
    fillTestData(&buf);
    const base = native.hash(&key, &buf);
    for (&buf) |*b| {
        b.* ^= 0x80;
        try testing.expect(native.hash(&key, &buf) != base);
        b.* ^= 0x80;
    }
}

test "every input bit affects the hash" {
    // SMHasher's sanity check 1. It caught a join whose equal inputs
    // cancelled, which made whole blocks vanish at lengths where the front
    // and back halves of the input share a block (48, 160, 176).
    var prng: std.Random.DefaultPrng = .init(883743);
    const key: native.Key = .init(5);
    var buf: [256]u8 = undefined;
    for (1..buf.len + 1) |len| {
        prng.random().bytes(buf[0..len]);
        const base = native.hash(&key, buf[0..len]);
        for (0..len * 8) |bit| {
            buf[bit / 8] ^= @as(u8, 1) << @intCast(bit % 8);
            errdefer std.debug.print("len={d} bit={d}\n", .{ len, bit });
            try testing.expect(native.hash(&key, buf[0..len]) != base);
            buf[bit / 8] ^= @as(u8, 1) << @intCast(bit % 8);
        }
    }
}

test "repeated and swapped blocks do not collide" {
    // Equal blocks in two positions must not cancel each other, and swapping
    // two blocks must change the result, for the default key as well.
    for ([_]u128{ 0, 77 }) |secret| {
        const key: native.Key = .init(secret);
        var buf: [4096]u8 = undefined;
        var other: [4096]u8 = undefined;
        for ([_]usize{ 32, 48, 64, 96, 128, 160, 176, 192, 256, 512, 1024, 4096 }) |len| {
            errdefer std.debug.print("len={d}\n", .{len});
            // The same 16-byte block repeated, with two different blocks.
            for (0..len) |i| buf[i] = @truncate(0x11 + i % block_len);
            for (0..len) |i| other[i] = @truncate(0x77 + i % block_len);
            try testing.expect(native.hash(&key, buf[0..len]) != native.hash(&key, other[0..len]));
            // Distinct blocks, then two of them swapped.
            fillTestData(buf[0..len]);
            @memcpy(other[0..len], buf[0..len]);
            @memcpy(other[0..block_len], buf[block_len .. 2 * block_len]);
            @memcpy(other[block_len .. 2 * block_len], buf[0..block_len]);
            try testing.expect(native.hash(&key, buf[0..len]) != native.hash(&key, other[0..len]));
        }
    }
}

test "sparse keys do not collide in the low 64 bits" {
    // SMHasher's Sparse keyset, reduced: every key of the given length with
    // one or two bits set. A random 64-bit function gives no collisions
    // here; both tree designs tried before gave dozens at 64 bytes with
    // three bits set, and this test's two-bit keys found them too.
    const key: native.Key = .init(0);
    for ([_]usize{ 48, 64, 96, 128, 256 }) |len| {
        const nbits = len * 8;
        const count = 1 + nbits + nbits * (nbits - 1) / 2;
        const hashes = try testing.allocator.alloc(u64, count);
        defer testing.allocator.free(hashes);
        const buf = try testing.allocator.alloc(u8, len);
        defer testing.allocator.free(buf);
        @memset(buf, 0);
        var n: usize = 0;
        hashes[n] = @truncate(native.hash(&key, buf));
        n += 1;
        for (0..nbits) |a| {
            buf[a / 8] ^= @as(u8, 1) << @intCast(a % 8);
            hashes[n] = @truncate(native.hash(&key, buf));
            n += 1;
            for (a + 1..nbits) |b| {
                buf[b / 8] ^= @as(u8, 1) << @intCast(b % 8);
                hashes[n] = @truncate(native.hash(&key, buf));
                n += 1;
                buf[b / 8] ^= @as(u8, 1) << @intCast(b % 8);
            }
            buf[a / 8] ^= @as(u8, 1) << @intCast(a % 8);
        }
        std.mem.sort(u64, hashes, {}, std.sort.asc(u64));
        for (1..count) |i| {
            errdefer std.debug.print("len={d}: collision {x}\n", .{ len, hashes[i] });
            try testing.expect(hashes[i] != hashes[i - 1]);
        }
    }
}

test "crafted one-round cancellations do not collide up to 256 bytes" {
    // A one-byte difference becomes, after one round, the MixColumns image
    // (2g, g, g, 3g) of an unknown S-box difference g. Xoring that image into
    // a block absorbed by the same state one round later cancels it for
    // exactly one g, whatever the key. Names (up to 256 bytes) use the tree,
    // where no state absorbs twice, so no guess may collide.
    var prng: std.Random.DefaultPrng = .init(99);
    const r = prng.random();
    var buf: [256]u8 = undefined;
    var x: [256]u8 = undefined;
    for ([_]usize{ 33, 65, 100, 128, 129, 200, 255, 256 }) |len| {
        for (0..8) |_| {
            const key: native.Key = .init(r.int(u128));
            r.bytes(buf[0..len]);
            const base = native.hash(&key, buf[0..len]);
            // Every later block start, including the back half's.
            var second: usize = block_len;
            while (second + 4 <= len) : (second += 1) {
                if (second % block_len != 0 and second != len - block_len) continue;
                for (1..256) |gi| {
                    const g: u8 = @intCast(gi);
                    const g2 = (g << 1) ^ (if (g & 0x80 != 0) @as(u8, 0x1b) else 0);
                    @memcpy(x[0..len], buf[0..len]);
                    x[0] ^= 1;
                    x[second] ^= g2;
                    x[second + 1] ^= g;
                    x[second + 2] ^= g;
                    x[second + 3] ^= g2 ^ g;
                    errdefer std.debug.print("len={d} second={d} g={d}\n", .{ len, second, g });
                    try testing.expect(native.hash(&key, x[0..len]) != base);
                }
            }
        }
    }
}

test "avalanche: flipping one input bit flips about half the output bits" {
    const key: native.Key = .init(3);
    for ([_]usize{ 1, 8, 16, 24, 64, 200, 600, 3000 }) |len| {
        const buf = try testing.allocator.alloc(u8, len);
        defer testing.allocator.free(buf);
        fillTestData(buf);
        const base = native.hash(&key, buf);
        var total: u64 = 0;
        var min: u32 = 128;
        for (0..len * 8) |bit| {
            buf[bit / 8] ^= @as(u8, 1) << @intCast(bit % 8);
            const diff: u32 = @popCount(native.hash(&key, buf) ^ base);
            buf[bit / 8] ^= @as(u8, 1) << @intCast(bit % 8);
            total += diff;
            min = @min(min, diff);
        }
        const mean = @as(f64, @floatFromInt(total)) / @as(f64, @floatFromInt(len * 8));
        errdefer std.debug.print("len={d} mean={d:.2} min={d}\n", .{ len, mean, min });
        try testing.expect(mean > 60 and mean < 68);
        try testing.expect(min >= 36);
    }
}

test "partialBlock matches copy-and-pad for every short length and alignment" {
    var buf: [64]u8 = undefined;
    fillTestData(&buf);
    for (0..block_len) |len| {
        for (0..8) |offset| {
            const input = buf[offset..][0..len];
            var expected: [block_len]u8 = @splat(@intCast(len));
            @memcpy(expected[0..len], input);
            try testing.expectEqual(@as(Vec, expected), partialBlock(input));
            try testing.expectEqual(@as(Vec, expected), partialBlockInRegisters(input));
        }
    }
    const empty: []const u8 = @as([*]const u8, @ptrFromInt(0x10))[0..0];
    try testing.expectEqual(zero, partialBlock(empty));
}

test "short inputs that end at an inaccessible page are read in bounds" {
    if (over_read_page == null) return testkit.skip(@src(), .no_kernel_feature, "the whole-page over-read applies only to page-mapped targets (Linux, Darwin, the BSDs, Windows)");
    const page_size = std.heap.pageSize();
    const mem: []align(std.heap.page_size_min) u8 = @alignCast(try std.heap.page_allocator.alloc(u8, 2 * page_size));
    defer std.heap.page_allocator.free(mem);
    const guard: []align(std.heap.page_size_min) u8 = @alignCast(mem[page_size..]);
    try std.process.protectMemory(guard, .{});
    defer std.process.protectMemory(guard, .{ .read = true, .write = true }) catch {};
    fillTestData(mem[0..page_size]);

    const key: native.Key = .init(42);
    var copy: [block_len]u8 = undefined;
    for (0..block_len) |len| {
        const input = mem[page_size - len .. page_size];
        @memcpy(copy[0..len], input);
        try testing.expectEqual(native.hash(&key, copy[0..len]), native.hash(&key, input));
    }
}
