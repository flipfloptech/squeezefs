//! Per-volume compression and encryption (charter §6.5; §6.2 item 3;
//! `docs/porting/data_transform_config.md` §1–§4, §6): the ONE 96 B
//! `TransformFrameWire` every transformed block image carries, the
//! per-FRAME best-effort compression rule with its store-raw escape, the
//! AEAD layer (AES-256-GCM / ChaCha20-Poly1305, one seal per frame, AAD =
//! the header ‖ the frame's table entry ‖ its index), the
//! `hkdf-sha256/aead-kw/v2` key wrap with its exact labels and lengths, the
//! key-file laws and process hardening, and the geometry derivations
//! (`maxStoredImageLen`, the chunk headroom, the scratch size, the mount
//! refusal). `processWrite`/`processRead` are the hooks every write and
//! read path consumes (the pack, the write-through, the fill ladder);
//! `frameSpan`/`decodeFrames` are the SUB-BLOCK read's (decision 0003 §K7).
//!
//! Format v4 (decision 0002 rows 113–120): the reference's `[u32 LE len |
//! raw<<31] ‖ image` frame with lz4's nested prepend-size, a big-endian
//! `wrapped_key_len` beside a little-endian word, two zstd frame shapes
//! and an EMPTY AAD is replaced by ONE fixed little-endian header, checked
//! field by field, authenticated whole.
//!
//! The FRAMED image (frame version 2, decision 0003 §K7): `header page
//! (the 96 B header ‖ a hash64-guarded frame table ‖ zero pad to 4 KiB) ‖
//! frames`, each frame `FRAME_GRAIN` (64 KiB) of plaintext compressed with
//! its own raw escape (the charter's "frame bit 31" is the entry's
//! `STORE_RAW`: that frame's body IS its plaintext, the escape sitting
//! BELOW the AEAD layer) and sealed under its own nonce (the block's base
//! counter + the frame index — a block reserves `frames` counters, so no
//! two frames of any two blocks share one). One AEAD/compression frame per
//! block meant every sub-block read decoded the whole 4 MiB image (1025×
//! device bytes at 4 KiB); with the grain a 4 KiB read DMAs the header
//! page and the frame it falls in and decodes 64 KiB. 64 KiB because lz4's
//! match window is 64 KiB (no ratio cost), zstd −3 loses a few percent at
//! 64 KiB against 4 MiB, AES-GCM pays 28 B per frame (0.04 %), and the
//! 64-entry table fits the header page beside the header.
//!
//! Passthrough volumes (`none`/`none`) keep the reference's NO-frame
//! contract: the image IS the payload byte-identical (ranged reads, W1,
//! the zc bridges depend on it) — `processWrite` returns its input.
//!
//! The CODEC CONTEXTS (decision 0003 §K10): `Transform` is the volume's
//! immutable half (the modes, the key state, the raw lever) and `Codec`
//! the per-LANE mutable half — the zstd compression AND decompression
//! contexts, each a static workspace the library never grows (`ZSTD_
//! initStaticCCtx/DCtx`; "use one different context per thread" is the
//! library's thread-safety contract). lz4 and the AEADs keep no state
//! between calls. A mount owns ONE `CodecPool` sized to the executor's
//! lane population (`codecPoolSize`) and ONE `ScratchPool` of stored-image
//! buffers (`scratchPoolCap`); a decode on any lane takes a codec with no
//! lock and no allocation. The ONE plane-wide transform lock this replaces
//! serialized every frame decode of every lane behind std's zstd decoder
//! (601 µs per 64 KiB frame at ρ ≈ 4 in ReleaseSafe against the C
//! context's 59 µs): 849 IOPS on a 9-lane venue whose lz4 twin ran 7.8 k
//! (`docs/evidence/rung-09-read-amplification.md` §4.1, §7).
//!
//! Portable: the codecs, the AEADs (`std.crypto`), the derivations and the
//! key wrap run on every host. The key-FILE laws and the process hardening
//! are Linux syscalls and compile only there.

const std = @import("std");
const testkit = @import("../core/testkit.zig");
const fuzzkit = @import("../core/fuzzkit.zig");
const builtin = @import("builtin");
const linux = std.os.linux;
const zc = @import("zstd_c");
const checksum = @import("../core/checksum.zig");
const counters = @import("../core/counters.zig");
const clock = @import("../core/clock.zig");
const derive = @import("../core/derive.zig");
const knobs = @import("../core/knobs.zig");
const log = @import("../core/log.zig");
const mpmc = @import("../sync/mpmc.zig");
const semaphore = @import("../sync/semaphore.zig");

const assert = std.debug.assert;
const Allocator = std.mem.Allocator;
const Aes256Gcm = std.crypto.aead.aes_gcm.Aes256Gcm;
const ChaCha20Poly1305 = std.crypto.aead.chacha_poly.ChaCha20Poly1305;
const HkdfSha256 = std.crypto.kdf.hkdf.HkdfSha256;

// ---------------------------------------------------------------------------
// The declared modes (u8 enums in the config record — never a string per block).
// ---------------------------------------------------------------------------

pub const Compression = enum(u8) {
    none = 0,
    lz4 = 1,
    zstd = 2,

    pub fn fromWire(b: u8) ?Compression {
        return switch (b) {
            0 => .none,
            1 => .lz4,
            2 => .zstd,
            else => null,
        };
    }

    /// The CLI spelling (`--compress`).
    pub fn parse(s: []const u8) ?Compression {
        if (std.mem.eql(u8, s, "none")) return .none;
        if (std.mem.eql(u8, s, "lz4")) return .lz4;
        if (std.mem.eql(u8, s, "zstd")) return .zstd;
        return null;
    }
};

pub const Encryption = enum(u8) {
    none = 0,
    aes256gcm = 1,
    chacha20poly1305 = 2,

    pub fn fromWire(b: u8) ?Encryption {
        return switch (b) {
            0 => .none,
            1 => .aes256gcm,
            2 => .chacha20poly1305,
            else => null,
        };
    }

    /// The CLI spelling (`--encrypt`).
    pub fn parse(s: []const u8) ?Encryption {
        if (std.mem.eql(u8, s, "none")) return .none;
        if (std.mem.eql(u8, s, "aes-256-gcm")) return .aes256gcm;
        if (std.mem.eql(u8, s, "chacha20-poly1305")) return .chacha20poly1305;
        return null;
    }

    pub fn name(self: Encryption) []const u8 {
        return switch (self) {
            .none => "none",
            .aes256gcm => "aes-256-gcm",
            .chacha20poly1305 => "chacha20-poly1305",
        };
    }
};

/// What a volume declares at format (the config record's transform fields).
pub const VolumeTransform = struct {
    compression: Compression = .none,
    encryption: Encryption = .none,
    /// The logical block: a power of two in `[MIN_BLOCK_SIZE, MAX_BLOCK_SIZE]`.
    block_size: u32,

    /// The reference's passthrough contract: no frame, byte-identical.
    pub fn isPassthrough(self: VolumeTransform) bool {
        return self.compression == .none and self.encryption == .none;
    }
};

// ---------------------------------------------------------------------------
// Constants with their WHY (spec §6.3).
// ---------------------------------------------------------------------------

/// Both AEADs' nonce.
pub const NONCE_LEN: usize = 12;
/// Both AEADs' tag.
pub const TAG_LEN: usize = 16;
/// 256-bit keys: the session data key and the KEK.
pub const DATA_KEY_LEN: usize = 32;
pub const KEK_LEN: usize = 32;
/// RFC 5869 recommends a salt of the hash length.
pub const KDF_SALT_LEN: usize = 32;
/// "Enough to name a key file, far too short to be a key."
pub const KEY_ID_LEN: usize = 8;
/// `"SK"(2) ‖ 0x02(1) ‖ nonce(12) ‖ ct(32) ‖ tag(16)`.
pub const WRAP_V2_LEN: usize = 3 + NONCE_LEN + DATA_KEY_LEN + TAG_LEN;
/// The scheme byte in the blob and in the config record.
pub const KEY_WRAP_SCHEME_V2: u8 = 2;
/// HKDF has no work factor: the file holds key MATERIAL, not a passphrase.
pub const MIN_KEY_MATERIAL_BYTES: usize = 32;
/// `--encrypt-key /dev/zero`-class mistakes stop here.
pub const MAX_KEY_FILE_BYTES: usize = 64 * 1024;
pub const KEK_INFO = "squeezefs:key-wrap:v2:kek";
pub const KEY_ID_INFO = "squeezefs:key-wrap:v2:key-id";
pub const KEY_WRAP_SCHEME_NAME = "hkdf-sha256/aead-kw/v2";
/// Resolution rung 3: `/etc/squeezefs/keys/<key_id hex>.key`.
pub const KEY_DIR = "/etc/squeezefs/keys";

/// The 4 KiB DMA grain every image is zero-padded to (PERF-10: the pad
/// costs zero device bytes inside a chunk and buys the aligned zero-copy
/// DMA branch; recycled scratch bytes must never reach the device, so the
/// pad is ZEROS).
pub const DMA_GRAIN: u32 = 4096;
/// The table grain: the header ‖ the frame table always fit in the image's
/// FIRST DMA grain, so a sub-block read fetches that grain alone to learn
/// its frames' window. Frame 0's body begins right after the table
/// (`tableLen(count)`), never on a page boundary: a padded page per image
/// would have doubled every small packed tenant's slot (`pack.packSlotLen`
/// = ceil_4k(image)) and the read DMA that serves it.
pub const HEADER_PAGE_LEN: usize = DMA_GRAIN;
/// The frame grain (plaintext bytes per frame): lz4's `MAX_DISTANCE` is
/// 64 KiB so no match is lost; zstd −3 at 64 KiB costs a few percent of
/// ratio against a 4 MiB frame; AES-GCM pays one 16 B tag per frame; a
/// 4 KiB read decodes 64 KiB instead of 4 MiB (decision 0003 §K7).
pub const FRAME_GRAIN_LOG2: u8 = 16;
pub const FRAME_GRAIN: u32 = 1 << FRAME_GRAIN_LOG2;
/// The smallest logical block is the DMA grain; the largest is the 4 MiB
/// accumulation buffer of charter §6.2 (the allocator stride).
pub const MIN_BLOCK_SIZE: u32 = 4096;
pub const MAX_BLOCK_SIZE: u32 = 4 << 20;
/// The charter's default block.
pub const DEFAULT_BLOCK_SIZE: u32 = 4 << 20;

pub fn blockSizeValid(block: u64) bool {
    return block >= MIN_BLOCK_SIZE and block <= MAX_BLOCK_SIZE and std.math.isPowerOfTwo(block);
}

comptime {
    assert(WRAP_V2_LEN == 63);
    assert(Aes256Gcm.nonce_length == NONCE_LEN and ChaCha20Poly1305.nonce_length == NONCE_LEN);
    assert(Aes256Gcm.tag_length == TAG_LEN and ChaCha20Poly1305.tag_length == TAG_LEN);
    assert(Aes256Gcm.key_length == DATA_KEY_LEN and ChaCha20Poly1305.key_length == DATA_KEY_LEN);
    assert(HkdfSha256.prk_length == 32);
}

// ---------------------------------------------------------------------------
// §1.3 The frame header.
// ---------------------------------------------------------------------------

pub const MAGIC = "SQZB";
/// Version 3 = the framed image with the table's checksum sqzhash128's low
/// 64 bits (decision 0003 §M); version 2 (the same image under xxh3-64) and
/// version 1 (one frame per block) are refused: pre-release, no
/// compatibility owed — decision 0002 rows 480.
pub const FRAME_VERSION: u8 = 3;
/// A frame ENTRY's flag: the body IS the plaintext (incompressible, or a
/// compression-less volume). Never set in the block header.
pub const FLAG_STORE_RAW: u8 = 1 << 0;
/// The block header's flag: every frame body is `ciphertext ‖ tag`.
pub const FLAG_ENCRYPTED: u8 = 1 << 1;
const HEADER_FLAGS_KNOWN: u8 = FLAG_ENCRYPTED;
const ENTRY_FLAGS_KNOWN: u8 = FLAG_STORE_RAW;

/// Every transformed block image: `header ‖ table ‖ frames`, zero-padded to
/// the DMA grain. Little-endian throughout; every field read by position.
pub const TransformFrameWire = extern struct {
    magic: [4]u8,
    version: u8,
    /// bit1 ENCRYPTED; every other bit must be 0.
    flags: u8,
    /// The volume's declared mode; a mismatch refuses.
    compression: u8,
    encryption: u8,
    /// Bytes after the table (`tableLen(count)`): Σ frame bodies (each
    /// `ciphertext ‖ tag` when encrypted).
    body_len_le: [4]u8,
    /// Declared plaintext length ≤ block_size (DUR-8e, checked FIRST).
    plain_len_le: [4]u8,
    /// `"SK" ‖ 0x02 ‖ nonce12 ‖ ct32 ‖ tag16`; all-zero when !ENCRYPTED.
    key_blob: [WRAP_V2_LEN]u8,
    /// The block's BASE nonce (frame i seals under base + i); all-zero
    /// when !ENCRYPTED.
    nonce: [NONCE_LEN]u8,
    reserved: [5]u8,
};

pub const HEADER_LEN: usize = @sizeOf(TransformFrameWire);
comptime {
    assert(HEADER_LEN == 96);
    assert(@offsetOf(TransformFrameWire, "body_len_le") == 8);
    assert(@offsetOf(TransformFrameWire, "plain_len_le") == 12);
    assert(@offsetOf(TransformFrameWire, "key_blob") == 16);
    assert(@offsetOf(TransformFrameWire, "nonce") == 79);
    assert(@offsetOf(TransformFrameWire, "reserved") == 91);
}

/// The frame table, right after the header inside the header page: a
/// 16 B head then `frame_count` entries. The hash64 guards the entries (a
/// plaintext volume has no AEAD to catch a torn table; an encrypted one
/// binds every entry into its frame's AAD as well).
pub const FrameTableWire = extern struct {
    /// hash64 over the entries (`frame_count × 16` bytes).
    entries_csum_le: [8]u8,
    frame_count_le: [2]u8,
    /// `FRAME_GRAIN_LOG2` (the reader never assumes the constant).
    frame_grain_log2: u8,
    reserved: [5]u8,
};

/// One frame: where its body sits in the image and what it decodes to.
pub const FrameEntryWire = extern struct {
    /// Body offset from the image start (the first = `tableLen(count)`;
    /// frames are contiguous).
    off_le: [4]u8,
    /// Body bytes (`ciphertext ‖ tag` when encrypted).
    body_len_le: [4]u8,
    /// Plaintext bytes: the grain for every frame but the last.
    plain_len_le: [4]u8,
    /// bit0 STORE_RAW; every other bit must be 0.
    flags: u8,
    reserved: [3]u8,
};

pub const TABLE_HEAD_LEN: usize = @sizeOf(FrameTableWire);
pub const ENTRY_LEN: usize = @sizeOf(FrameEntryWire);
/// The entries the header page can hold beside the header and the table
/// head; the largest block (4 MiB) at the grain needs 64.
pub const MAX_FRAMES: usize = (HEADER_PAGE_LEN - HEADER_LEN - TABLE_HEAD_LEN) / ENTRY_LEN;
comptime {
    assert(TABLE_HEAD_LEN == 16 and ENTRY_LEN == 16);
    assert(MAX_FRAMES >= MAX_BLOCK_SIZE / FRAME_GRAIN);
    assert(HEADER_LEN + TABLE_HEAD_LEN + MAX_FRAMES * ENTRY_LEN <= HEADER_PAGE_LEN);
}

/// The frames a plaintext of `plain_len` takes (0 for the empty payload).
pub fn framesFor(plain_len: u32) u32 {
    return (plain_len + FRAME_GRAIN - 1) / FRAME_GRAIN;
}

/// The header ‖ table head ‖ `count` entries: where frame 0's body begins.
/// 128 B for a one-frame image, 1,136 B for a 4 MiB block's 64 frames.
pub fn tableLen(count: u32) usize {
    return HEADER_LEN + TABLE_HEAD_LEN + @as(usize, count) * ENTRY_LEN;
}

/// Frame `i`'s nonce: the block's base counter + i (the base was minted
/// as a RANGE of `frames` counters, so no frame of any block shares one).
pub fn frameNonce(base: [NONCE_LEN]u8, i: u32) [NONCE_LEN]u8 {
    var out = base;
    const counter = std.mem.readInt(u64, base[4..12], .big);
    std.mem.writeInt(u64, out[4..12], counter +% i, .big);
    return out;
}

/// A decoded frame entry.
pub const Entry = struct {
    off: u32,
    body_len: u32,
    plain_len: u32,
    flags: u8,

    pub fn storeRaw(self: Entry) bool {
        return self.flags & FLAG_STORE_RAW != 0;
    }

    fn encode(self: Entry, out: *[ENTRY_LEN]u8) void {
        var w: FrameEntryWire = undefined;
        std.mem.writeInt(u32, &w.off_le, self.off, .little);
        std.mem.writeInt(u32, &w.body_len_le, self.body_len, .little);
        std.mem.writeInt(u32, &w.plain_len_le, self.plain_len, .little);
        w.flags = self.flags;
        w.reserved = @splat(0);
        out.* = std.mem.toBytes(w);
    }

    /// The AAD a frame seals under: the header ‖ this entry ‖ the index.
    fn aad(self: Entry, header: *const [HEADER_LEN]u8, i: u32, out: *[HEADER_LEN + ENTRY_LEN + 4]u8) void {
        out[0..HEADER_LEN].* = header.*;
        self.encode(out[HEADER_LEN..][0..ENTRY_LEN]);
        std.mem.writeInt(u32, out[HEADER_LEN + ENTRY_LEN ..][0..4], i, .little);
    }
};

/// The decoded table: the entries live in the header page the caller holds.
pub const Table = struct {
    count: u32,
    entries: []align(1) const FrameEntryWire,

    pub fn get(self: Table, i: u32) Entry {
        const w = self.entries[i];
        return .{ .off = std.mem.readInt(u32, &w.off_le, .little), .body_len = std.mem.readInt(u32, &w.body_len_le, .little), .plain_len = std.mem.readInt(u32, &w.plain_len_le, .little), .flags = w.flags };
    }
};

/// The decoded header.
pub const Header = struct {
    flags: u8,
    compression: Compression,
    encryption: Encryption,
    body_len: u32,
    plain_len: u32,
    key_blob: [WRAP_V2_LEN]u8,
    nonce: [NONCE_LEN]u8,

    pub fn encrypted(self: Header) bool {
        return self.flags & FLAG_ENCRYPTED != 0;
    }

    pub fn encode(self: Header, out: *[HEADER_LEN]u8) void {
        var w: TransformFrameWire = undefined;
        w.magic = MAGIC.*;
        w.version = FRAME_VERSION;
        w.flags = self.flags;
        w.compression = @backingInt(self.compression);
        w.encryption = @backingInt(self.encryption);
        std.mem.writeInt(u32, &w.body_len_le, self.body_len, .little);
        std.mem.writeInt(u32, &w.plain_len_le, self.plain_len, .little);
        w.key_blob = self.key_blob;
        w.nonce = self.nonce;
        w.reserved = @splat(0);
        out.* = std.mem.toBytes(w);
    }
};

/// Why an image was refused — one class per named tripwire.
pub const FrameRefusal = enum {
    /// Shorter than the header, or `body_len` past the image window.
    len,
    /// `plain_len > block_size` (DUR-8e): refused before anything else.
    plain_len,
    magic,
    version,
    /// Unknown flag bits, non-zero reserved bytes, a non-zero key blob or
    /// nonce on a plaintext frame, STORE_RAW on a `none`-compression frame
    /// cleared, or an encrypted frame's lengths that cannot hold a tag.
    flags,
    /// `compression`/`encryption` ≠ the volume's declared modes.
    mode,
    /// The AEAD open failed (a foreign key, a flipped header or body byte).
    aad,
    /// The key blob did not unwrap under this volume's KEK.
    key,
    /// The decompressor refused the body or produced ≠ `plain_len` bytes.
    body,
    /// The frame table: a torn hash64, a count ≠ `framesFor(plain_len)`,
    /// a grain ≠ the constant, non-contiguous or oversized bodies, a
    /// plain length off the grain, an entry's flag/length law.
    table,
};

pub const DecodeError = error{ FrameLen, FramePlainLen, FrameMagic, FrameVersion, FrameFlags, FrameMode, FrameAuth, FrameKey, FrameBody, FrameTable };

pub fn refusalOf(e: DecodeError) FrameRefusal {
    return switch (e) {
        error.FrameLen => .len,
        error.FramePlainLen => .plain_len,
        error.FrameMagic => .magic,
        error.FrameVersion => .version,
        error.FrameFlags => .flags,
        error.FrameMode => .mode,
        error.FrameAuth => .aad,
        error.FrameKey => .key,
        error.FrameBody => .body,
        error.FrameTable => .table,
    };
}

fn refusalError(r: FrameRefusal) DecodeError {
    return switch (r) {
        .len => error.FrameLen,
        .plain_len => error.FramePlainLen,
        .magic => error.FrameMagic,
        .version => error.FrameVersion,
        .flags => error.FrameFlags,
        .mode => error.FrameMode,
        .aad => error.FrameAuth,
        .key => error.FrameKey,
        .body => error.FrameBody,
        .table => error.FrameTable,
    };
}

/// The header decoder in its exact check order: length ≥ header →
/// `plain_len ≤ block_size` (the DUR-8e bound, before any other field is
/// trusted) → magic → version → flags/reserved → the volume's modes → the
/// flag/mode law → the plaintext-volume zero law. Total: any byte string
/// yields a header or a named refusal; nothing here allocates. The body
/// window and the per-frame laws are the TABLE decoder's (`decodeTable`).
pub fn decodeHeader(cfg: VolumeTransform, image: []const u8) DecodeError!Header {
    if (image.len < HEADER_LEN) return error.FrameLen;
    const w = std.mem.bytesToValue(TransformFrameWire, image[0..HEADER_LEN]);
    const plain_len = std.mem.readInt(u32, &w.plain_len_le, .little);
    if (plain_len > cfg.block_size) return error.FramePlainLen;
    if (!std.mem.eql(u8, &w.magic, MAGIC)) return error.FrameMagic;
    if (w.version != FRAME_VERSION) return error.FrameVersion;
    if (w.flags & ~HEADER_FLAGS_KNOWN != 0 or !std.mem.allEqual(u8, &w.reserved, 0)) return error.FrameFlags;
    const compression = Compression.fromWire(w.compression) orelse return error.FrameMode;
    const encryption = Encryption.fromWire(w.encryption) orelse return error.FrameMode;
    if (compression != cfg.compression or encryption != cfg.encryption) return error.FrameMode;
    const body_len = std.mem.readInt(u32, &w.body_len_le, .little);
    const h: Header = .{ .flags = w.flags, .compression = compression, .encryption = encryption, .body_len = body_len, .plain_len = plain_len, .key_blob = w.key_blob, .nonce = w.nonce };
    // Consistency: the flag says what the modes say.
    if (h.encrypted() != (encryption != .none)) return error.FrameFlags;
    if (!h.encrypted() and (!std.mem.allEqual(u8, &w.key_blob, 0) or !std.mem.allEqual(u8, &w.nonce, 0))) return error.FrameFlags;
    return h;
}

/// The table decoder over the image's first grain: the hash64 over the
/// entries, the count (`framesFor(plain_len)`), the grain, then every
/// entry's laws — contiguous bodies from `tableLen(count)` summing to `body_len`,
/// plaintexts of the grain but the last, an encrypted body carrying its
/// tag, a raw body exactly its plaintext, a compressed body strictly
/// shorter, a compression-less volume all raw. Total over any page.
pub fn decodeTable(cfg: VolumeTransform, h: Header, page: []const u8) DecodeError!Table {
    if (page.len < HEADER_PAGE_LEN) return error.FrameLen;
    const tw = std.mem.bytesToValue(FrameTableWire, page[HEADER_LEN..][0..TABLE_HEAD_LEN]);
    const count = std.mem.readInt(u16, &tw.frame_count_le, .little);
    if (count != framesFor(h.plain_len) or count > MAX_FRAMES) return error.FrameTable;
    if (tw.frame_grain_log2 != FRAME_GRAIN_LOG2 or !std.mem.allEqual(u8, &tw.reserved, 0)) return error.FrameTable;
    const entries_bytes = page[HEADER_LEN + TABLE_HEAD_LEN ..][0 .. @as(usize, count) * ENTRY_LEN];
    if (std.mem.readInt(u64, &tw.entries_csum_le, .little) != checksum.hash64(entries_bytes)) return error.FrameTable;
    const t: Table = .{ .count = count, .entries = std.mem.bytesAsSlice(FrameEntryWire, entries_bytes) };
    var off: u64 = tableLen(count);
    var plain_left: u64 = h.plain_len;
    var i: u32 = 0;
    while (i < count) : (i += 1) {
        const e = t.get(i);
        if (e.off != off) return error.FrameTable;
        if (e.flags & ~ENTRY_FLAGS_KNOWN != 0 or !std.mem.allEqual(u8, &t.entries[i].reserved, 0)) return error.FrameTable;
        const want_plain: u64 = if (i + 1 < count) FRAME_GRAIN else plain_left;
        if (e.plain_len != want_plain or e.plain_len == 0) return error.FrameTable;
        if (cfg.compression == .none and !e.storeRaw()) return error.FrameTable;
        const inner: u64 = if (h.encrypted()) blk: {
            if (e.body_len < TAG_LEN) return error.FrameTable;
            break :blk e.body_len - TAG_LEN;
        } else e.body_len;
        if (e.storeRaw()) {
            if (inner != e.plain_len) return error.FrameTable;
        } else if (inner >= e.plain_len or inner == 0) return error.FrameTable;
        off += e.body_len;
        plain_left -= e.plain_len;
    }
    if (off - tableLen(count) != h.body_len or plain_left != 0) return error.FrameTable;
    return t;
}

/// The frames a block-relative window `[rel_s, rel_e)` needs and the image
/// bytes to DMA for them, rounded to the DMA grain outward (`hi` is
/// clipped to the image's padded end). An image may be SHORTER than its
/// block (a promoted sub-block image; a tail block's upload trimmed to its
/// live frames, decision 0003 §AK): the window is served from the
/// plaintext up to `plain_end` and reads zeros past it — a span whose
/// `plain_end` is not past `plain_base` names no frame at all.
pub const FrameSpan = struct {
    first: u32,
    last: u32,
    /// Image offsets of the DMA window (4 KiB-aligned).
    lo: u64,
    hi: u64,
    /// The plaintext offset of `first`'s start.
    plain_base: u64,
    /// `min(rel_e, plain_len)`: where the image's plaintext ends inside the window.
    plain_end: u64,

    /// Wholly past the image's plaintext: zeros, no frame to read.
    pub fn isEmpty(self: FrameSpan) bool {
        return self.plain_end <= self.plain_base;
    }
};

pub fn frameSpan(h: Header, t: Table, rel_s: u64, rel_e: u64) DecodeError!FrameSpan {
    if (rel_e <= rel_s) return error.FrameTable;
    const end = @min(rel_e, h.plain_len);
    if (end <= rel_s) return .{ .first = 0, .last = 0, .lo = 0, .hi = 0, .plain_base = rel_s, .plain_end = rel_s };
    if (t.count == 0) return error.FrameTable;
    const first: u32 = @intCast(rel_s / FRAME_GRAIN);
    const last: u32 = @intCast((end - 1) / FRAME_GRAIN);
    const a = t.get(first);
    const b = t.get(last);
    const image_end = derive.alignUp(tableLen(t.count) + @as(u64, h.body_len), DMA_GRAIN);
    return .{ .first = first, .last = last, .lo = derive.alignDown(a.off, DMA_GRAIN), .hi = @min(derive.alignUp(@as(u64, b.off) + b.body_len, DMA_GRAIN), image_end), .plain_base = @as(u64, first) * FRAME_GRAIN, .plain_end = end };
}

/// The plaintext a block's framed image must carry: through its last
/// non-zero byte — every byte past it reads back as zeros past the image's
/// plaintext (`FrameSpan.plain_end`, the fill's zero completion, the seeds'
/// zero fill), and a short last frame is the format's own shape. A tail
/// block's upload transforms only this (decision 0003 §AK): the stored
/// image is then `storedImageBound(liveLen)`, never the whole block — an
/// encrypted set stored every zero frame raw at 64 KiB + a tag. The scan
/// skips whole zero frames, then walks the last live frame's tail.
pub fn liveLen(plain: []const u8) usize {
    var end = std.mem.alignForward(usize, plain.len, FRAME_GRAIN);
    while (end != 0) {
        const start = end - FRAME_GRAIN;
        const frame = plain[@min(start, plain.len)..@min(end, plain.len)];
        if (!std.mem.allEqual(u8, frame, 0)) {
            var n = frame.len;
            while (frame[n - 1] == 0) n -= 1;
            return start + n;
        }
        end = start;
    }
    return 0;
}

// ---------------------------------------------------------------------------
// §4 Geometry: max stored image, headroom, chunk, scratch, the refusal.
// ---------------------------------------------------------------------------

/// The largest image a block can produce: the table, the payload (every
/// frame's raw escape makes the compression term never exceed its
/// plaintext), and one AEAD tag per frame on an encrypted volume
/// (`tableLen(frames) + block + frames × 16`). Passthrough: the payload
/// itself (no frame).
pub fn maxStoredImageLen(cfg: VolumeTransform) u64 {
    if (cfg.isPassthrough()) return cfg.block_size;
    const frames = framesFor(cfg.block_size);
    return tableLen(frames) + @as(u64, cfg.block_size) + (if (cfg.encryption != .none) @as(u64, frames) * TAG_LEN else 0);
}

/// The device bytes a framed image of `plain_len` plaintext can occupy:
/// the table (inside the first DMA grain), the payload — every frame's raw
/// escape bounds its body by its plaintext — and one AEAD tag per frame on
/// an encrypted volume, padded to the DMA grain the write lands at
/// (`maxStoredImageLen` is the whole block's, unpadded). A tail block's
/// upload trimmed to its live frames lands at most this (decision 0003 §AK).
pub fn storedImageBound(cfg: VolumeTransform, plain_len: u64) u64 {
    const frames = framesFor(@intCast(plain_len));
    return derive.alignUp(tableLen(frames) + plain_len + (if (cfg.encryption != .none) @as(u64, frames) * TAG_LEN else 0), DMA_GRAIN);
}

/// The worst-case non-payload bytes of an image (the 64-frame table 1,136
/// + 64 tags 1,024 = 2,160) rounded to the DMA grain so chunk-interior
/// windows stay O_DIRECT-aligned: 4096 (the reference's 547 B incl. the
/// retired 512 B RSA ghost rounded to 4096 too, decision 0002 row 115).
pub const TRANSFORM_BLOCK_HEADROOM: u32 = DMA_GRAIN;

comptime {
    assert(tableLen(MAX_BLOCK_SIZE / FRAME_GRAIN) + (MAX_BLOCK_SIZE / FRAME_GRAIN) * TAG_LEN <= TRANSFORM_BLOCK_HEADROOM);
}

/// The allocator stride (the chunk): the block the operator named stays a
/// power of two (every block boundary stays on the 1 MiB writeback grid of
/// charter §6.2) and the chunk GROWS by the headroom on a transformed
/// volume — the reference shrank the block to `4 MiB − 4 KiB` instead
/// (decision 0002 row 116). Passthrough: `chunk = block`.
pub fn chunkSizeFor(cfg: VolumeTransform) u32 {
    return if (cfg.isPassthrough()) cfg.block_size else cfg.block_size + TRANSFORM_BLOCK_HEADROOM;
}

/// The geometry law: a transformed volume's chunk must hold the largest
/// image, or its full-size incompressible blocks are unreadable.
pub fn geometryCheck(cfg: VolumeTransform, chunk_size: u32) error{ChunkCannotHoldImage}!void {
    if (chunk_size < maxStoredImageLen(cfg)) return error.ChunkCannotHoldImage;
}

pub fn writeGeometryRefusal(w: *std.Io.Writer, cfg: VolumeTransform, chunk_size: u32) std.Io.Writer.Error!void {
    try w.print("transformed volume geometry: chunk {d} B cannot hold max_stored_image_len({d} B block) = {d} B; the set's config is corrupt or was written by a tool this binary does not know — reformat", .{ chunk_size, cfg.block_size, maxStoredImageLen(cfg) });
}

/// The write side's scratch: the table, per frame a compressed-or-raw
/// body that never exceeds its plaintext (bounded output: the compressor
/// is given exactly `plain.len − 1` bytes and the escape takes the
/// overflow) plus its tag, rounded to the DMA grain. 4 MiB ⇒ 4,198,400 B
/// = block + 4 KiB. The reference sized its scratch from library BOUNDS
/// instead (`4 + max(4 + lz4_bound, zstd_bound) + 98`, 4,214,784 B = block
/// + 20 KiB at 4 MiB) and bounced the overflow to the heap; v4 never needs
/// the bound.
pub fn writeScratchLen(block_size: u32) u64 {
    const frames = framesFor(block_size);
    return derive.alignUp(tableLen(frames) + @as(u64, block_size) + @as(u64, frames) * TAG_LEN, DMA_GRAIN);
}

/// ONE scratch size per volume, for the write scratch AND the read
/// destination: `writeScratchLen` = block + 4 KiB. The read side needs no
/// slack — the C decoder writes exactly the frame's content size into its
/// destination (std's streaming decoder, retired in decision 0002 row 118,
/// wanted a whole extra block of window room behind the plaintext: block
/// + 128 KiB). Inside the charter's `block + 128 KiB` envelope (§6.2 item
/// 3), which the reference sized from its libraries' bounds.
pub fn scratchLen(block_size: u32) u64 {
    return writeScratchLen(block_size);
}

/// Stored-image scratches a mount owns per `ScratchPool`:
/// `clamp(cpus / 4, 4, 16)` — above the concurrent-transform count every
/// handout rotates through a cache-cold buffer (+4..10 % measured; the
/// reference's `scratch_pool_cap`). A buffer is held across the DMA its
/// image feeds, so the pool, not a lane, is the unit: an acquirer past the
/// cap parks (`transform_scratch_pool_waits`), never spills to the heap.
pub fn scratchPoolCap(cpus: u32) u32 {
    return @intCast(derive.clamp(cpus / 4, 4, 16));
}

/// Codec contexts a mount owns: ONE per executor lane (the fuse handler
/// lanes, the meta pool, the lease and main lanes — every thread a
/// transform runs on), floor 1. A transform never parks while it holds a
/// codec (pure CPU; its scratch is the `ScratchPool`'s), so one per lane
/// thread is exact: the pool never runs dry and
/// `transform_codec_pool_waits` stays 0 by derivation.
pub fn codecPoolSize(lanes: usize) usize {
    return @max(lanes, 1);
}

/// Capacity in blocks of a data volume: chunks 0 and `last` are reserved
/// (the identity sectors live there; both ends are signature regions).
pub fn capacityBlocks(volume_len: u64, chunk_size: u32) u64 {
    const chunks = volume_len / chunk_size;
    return chunks -| 2;
}

// ---------------------------------------------------------------------------
// §2.1 LZ4 — the BLOCK format, first-party (std has no lz4; decision 0002
// row 117). Compressor: the greedy hash-table scheme of the reference
// implementation's fast path, with a BOUNDED output (the caller's capacity
// is the escape's threshold). Decompressor: total over arbitrary bytes,
// exact destination size, no size prefix.
// ---------------------------------------------------------------------------

pub const lz4 = struct {
    /// A match is ≥ 4 bytes (the token's low nibble counts from 4).
    pub const MINMATCH: usize = 4;
    /// The last match may not start within 12 bytes of the end, and the
    /// last 5 bytes are always literals (the format's end-of-block rules,
    /// which let a reference decoder copy in wild 8-byte steps).
    pub const MFLIMIT: usize = 12;
    pub const LASTLITERALS: usize = 5;
    /// A 16-bit offset.
    pub const MAX_DISTANCE: usize = 65_535;
    /// `LZ4_MEMORY_USAGE = 14`: a 16 KiB table of 4096 u32 positions — the
    /// reference library's default, the point where the table stays in L1.
    pub const HASH_LOG: u6 = 12;
    const HASH_SIZE: usize = 1 << HASH_LOG;
    /// After this many consecutive misses the search step doubles (the
    /// reference's `LZ4_skipTrigger`): incompressible data is skipped
    /// through, which is what makes "store raw" cheap.
    const SKIP_TRIGGER: u5 = 6;
    const ML_BITS: u4 = 4;
    const ML_MASK: u8 = (1 << ML_BITS) - 1;
    const RUN_MASK: u8 = (1 << ML_BITS) - 1;

    pub const CompressError = error{OutputTooSmall};

    /// The worst-case compressed size of the reference bound (`n + n/255
    /// + 16`); our compressor never exceeds it (tie-tested) but is never
    /// GIVEN it — the caller's capacity is the raw escape's threshold.
    pub fn compressBound(n: usize) usize {
        return n + n / 255 + 16;
    }

    inline fn read32(src: []const u8, i: usize) u32 {
        return std.mem.readInt(u32, src[i..][0..4], .little);
    }

    inline fn hash(v: u32) usize {
        // Fibonacci hashing (2654435761 = 2^32 / φ): the reference's
        // `LZ4_hash4`.
        return @intCast((v *% 2654435761) >> @as(u5, 32 - HASH_LOG));
    }

    /// Emit `n` as a 4-bit nibble plus 255-continuation bytes.
    fn putLength(dst: []u8, pos: *usize, n: usize) CompressError!void {
        var rest = n;
        while (rest >= 255) : (rest -= 255) {
            if (pos.* >= dst.len) return error.OutputTooSmall;
            dst[pos.*] = 255;
            pos.* += 1;
        }
        if (pos.* >= dst.len) return error.OutputTooSmall;
        dst[pos.*] = @intCast(rest);
        pos.* += 1;
    }

    /// Compress `src` into `dst`; the compressed length, or `OutputTooSmall`
    /// when the block does not fit `dst` (the raw escape's signal).
    pub fn compressBlock(src: []const u8, dst: []u8) CompressError!usize {
        var table: [HASH_SIZE]u32 = @splat(0);
        var op: usize = 0;
        var anchor: usize = 0;
        if (src.len >= MFLIMIT + 1) {
            const match_limit = src.len - LASTLITERALS;
            const search_limit = src.len - MFLIMIT;
            var ip: usize = 1;
            table[hash(read32(src, 0))] = 0;
            outer: while (true) {
                // Find a match, accelerating through incompressible runs.
                var step: usize = 1;
                var search_count: usize = 1 << SKIP_TRIGGER;
                var ref: usize = undefined;
                while (true) {
                    if (ip > search_limit) break :outer;
                    const h = hash(read32(src, ip));
                    ref = table[h];
                    table[h] = @intCast(ip);
                    if (ip - ref <= MAX_DISTANCE and ref < ip and read32(src, ref) == read32(src, ip)) break;
                    ip += step;
                    search_count += 1;
                    step = search_count >> SKIP_TRIGGER;
                }
                // Catch up: extend the match backwards into the literal run.
                while (ip > anchor and ref > 0 and src[ip - 1] == src[ref - 1]) {
                    ip -= 1;
                    ref -= 1;
                }
                // Match length past the first 4 bytes, bounded by the end law.
                var ml: usize = MINMATCH;
                while (ip + ml < match_limit and src[ip + ml] == src[ref + ml]) ml += 1;
                // Token + literals + offset + match length.
                const lit = ip - anchor;
                const token_pos = op;
                if (op >= dst.len) return error.OutputTooSmall;
                op += 1;
                if (lit >= RUN_MASK) {
                    dst[token_pos] = RUN_MASK << ML_BITS;
                    try putLength(dst, &op, lit - RUN_MASK);
                } else dst[token_pos] = @as(u8, @intCast(lit)) << ML_BITS;
                if (dst.len - op < lit + 2) return error.OutputTooSmall;
                @memcpy(dst[op..][0..lit], src[anchor..ip]);
                op += lit;
                std.mem.writeInt(u16, dst[op..][0..2], @intCast(ip - ref), .little);
                op += 2;
                const extra = ml - MINMATCH;
                if (extra >= ML_MASK) {
                    dst[token_pos] |= ML_MASK;
                    try putLength(dst, &op, extra - ML_MASK);
                } else dst[token_pos] |= @intCast(extra);
                ip += ml;
                anchor = ip;
                if (ip > search_limit) break;
                // Seed the table with the position two back (the reference's
                // post-match fill), then continue searching from `ip`.
                table[hash(read32(src, ip - 2))] = @intCast(ip - 2);
            }
        }
        // Last literals.
        const last = src.len - anchor;
        if (op >= dst.len) return error.OutputTooSmall;
        const token_pos = op;
        op += 1;
        if (last >= RUN_MASK) {
            dst[token_pos] = RUN_MASK << ML_BITS;
            try putLength(dst, &op, last - RUN_MASK);
        } else dst[token_pos] = @as(u8, @intCast(last)) << ML_BITS;
        if (dst.len - op < last) return error.OutputTooSmall;
        @memcpy(dst[op..][0..last], src[anchor..]);
        op += last;
        return op;
    }

    pub const DecompressError = error{Corrupt};

    /// Decompress exactly `dst.len` bytes from `src`; total (any `src`
    /// yields `dst` filled or `Corrupt`; never reads or writes out of
    /// bounds). Trailing or missing input is `Corrupt`: the frame's
    /// `body_len` is exact.
    pub fn decompressBlock(src: []const u8, dst: []u8) DecompressError!void {
        var ip: usize = 0;
        var op: usize = 0;
        while (true) {
            if (ip >= src.len) return error.Corrupt;
            const token = src[ip];
            ip += 1;
            var lit: usize = token >> ML_BITS;
            if (lit == RUN_MASK) {
                while (true) {
                    if (ip >= src.len) return error.Corrupt;
                    const b = src[ip];
                    ip += 1;
                    lit += b;
                    // Bounded by the destination before it can overflow.
                    if (lit > dst.len) return error.Corrupt;
                    if (b != 255) break;
                }
            }
            if (lit > dst.len - op or lit > src.len - ip) return error.Corrupt;
            @memcpy(dst[op..][0..lit], src[ip..][0..lit]);
            op += lit;
            ip += lit;
            if (ip == src.len) {
                // The last sequence is literals only and lands exactly on
                // the declared length.
                return if (op == dst.len) {} else error.Corrupt;
            }
            if (src.len - ip < 2) return error.Corrupt;
            const offset: usize = std.mem.readInt(u16, src[ip..][0..2], .little);
            ip += 2;
            if (offset == 0 or offset > op) return error.Corrupt;
            var ml: usize = (token & ML_MASK) + MINMATCH;
            if (token & ML_MASK == ML_MASK) {
                while (true) {
                    if (ip >= src.len) return error.Corrupt;
                    const b = src[ip];
                    ip += 1;
                    ml += b;
                    if (ml > dst.len) return error.Corrupt;
                    if (b != 255) break;
                }
            }
            if (ml > dst.len - op) return error.Corrupt;
            // Overlap-safe copy: a run of `offset` bytes repeats.
            var copied: usize = 0;
            while (copied < ml) {
                const n = @min(offset, ml - copied);
                const from = op - offset;
                @memcpy(dst[op..][0..n], dst[from..][0..n]);
                op += n;
                copied += n;
            }
        }
    }
};

// ---------------------------------------------------------------------------
// §2.1 zstd — level 3 (the library default), ONE frame shape: single-shot
// `ZSTD_compress2` with the content size pledged, in a STATIC context the
// caller's allocator provides once (no malloc per op; decision 0002 row
// 118). Decompression is the same library's single-shot
// `ZSTD_decompressDCtx` in a static `ZSTD_DCtx` — one per lane (the
// library's "one context per thread" contract), exact to the content
// size, total over arbitrary input.
// ---------------------------------------------------------------------------

pub const zstd = struct {
    /// `ZSTD_CLEVEL_DEFAULT`: the library's default, the reference's choice.
    pub const LEVEL: c_int = 3;

    /// The static workspaces' alignment: the library asks for 8.
    const WORKSPACE_ALIGN: std.mem.Alignment = .@"8";

    pub const InitError = error{ OutOfMemory, ZstdInit };

    pub const Compressor = struct {
        workspace: []align(WORKSPACE_ALIGN.toByteUnits()) u8,
        cctx: *zc.ZSTD_CCtx,

        /// One context sized for `LEVEL` by the library's own estimate.
        pub fn init(gpa: Allocator) InitError!Compressor {
            const est = zc.ZSTD_estimateCCtxSize(LEVEL);
            if (zc.ZSTD_isError(est) != 0) return error.ZstdInit;
            const ws = try gpa.alignedAlloc(u8, WORKSPACE_ALIGN, est);
            errdefer gpa.free(ws);
            const cctx = zc.ZSTD_initStaticCCtx(ws.ptr, est) orelse return error.ZstdInit;
            if (zc.ZSTD_isError(zc.ZSTD_CCtx_setParameter(cctx, zc.ZSTD_c_compressionLevel, LEVEL)) != 0) return error.ZstdInit;
            if (zc.ZSTD_isError(zc.ZSTD_CCtx_setParameter(cctx, zc.ZSTD_c_contentSizeFlag, 1)) != 0) return error.ZstdInit;
            // The frame carries no xxh64: the AEAD tag or the device-level
            // checksum discipline authenticates the body, and `plain_len`
            // pins the content size.
            if (zc.ZSTD_isError(zc.ZSTD_CCtx_setParameter(cctx, zc.ZSTD_c_checksumFlag, 0)) != 0) return error.ZstdInit;
            return .{ .workspace = ws, .cctx = cctx };
        }

        pub fn deinit(self: *Compressor, gpa: Allocator) void {
            // A static context owns nothing outside its workspace.
            gpa.free(self.workspace);
            self.* = undefined;
        }

        /// Compress `src` into `dst` (bounded: `dst.len` is the escape's
        /// threshold); the frame length.
        pub fn compress(self: *Compressor, src: []const u8, dst: []u8) error{ OutputTooSmall, ZstdFailed }!usize {
            const n = zc.ZSTD_compress2(self.cctx, dst.ptr, dst.len, src.ptr, src.len);
            if (zc.ZSTD_isError(n) != 0) {
                const code = zc.ZSTD_getErrorCode(n);
                if (code == zc.ZSTD_error_dstSize_tooSmall) return error.OutputTooSmall;
                return error.ZstdFailed;
            }
            return n;
        }
    };

    pub const Decompressor = struct {
        workspace: []align(WORKSPACE_ALIGN.toByteUnits()) u8,
        dctx: *zc.ZSTD_DCtx,

        /// One context by the library's own estimate (≈ 96 KiB: the entropy
        /// tables and the literal buffer; a single-shot decode writes its
        /// window straight into the destination and allocates nothing).
        pub fn init(gpa: Allocator) InitError!Decompressor {
            const est = zc.ZSTD_estimateDCtxSize();
            if (zc.ZSTD_isError(est) != 0) return error.ZstdInit;
            const ws = try gpa.alignedAlloc(u8, WORKSPACE_ALIGN, est);
            errdefer gpa.free(ws);
            const dctx = zc.ZSTD_initStaticDCtx(ws.ptr, est) orelse return error.ZstdInit;
            return .{ .workspace = ws, .dctx = dctx };
        }

        pub fn deinit(self: *Decompressor, gpa: Allocator) void {
            gpa.free(self.workspace);
            self.* = undefined;
        }

        /// Decode exactly `plain_len` bytes of ONE frame from `src` into
        /// `out` (`out.len ≥ plain_len`). Total: any input yields the bytes
        /// or `Corrupt` — a frame declaring more content than `plain_len`
        /// refuses on the bounded destination, one declaring less on the
        /// count, trailing bytes on the frame walk (the library decodes a
        /// sequence of frames and refuses a remainder that is not one).
        pub fn decompress(self: *Decompressor, src: []const u8, out: []u8, plain_len: u32) error{Corrupt}!void {
            assert(out.len >= plain_len);
            const n = zc.ZSTD_decompressDCtx(self.dctx, out.ptr, plain_len, src.ptr, src.len);
            if (zc.ZSTD_isError(n) != 0 or n != plain_len) return error.Corrupt;
        }
    };

    /// A lane's pair: the compressor and the decompressor.
    pub const Context = struct {
        c: Compressor,
        d: Decompressor,

        pub fn init(gpa: Allocator) InitError!Context {
            var c = try Compressor.init(gpa);
            errdefer c.deinit(gpa);
            const d = try Decompressor.init(gpa);
            return .{ .c = c, .d = d };
        }

        pub fn deinit(self: *Context, gpa: Allocator) void {
            self.c.deinit(gpa);
            self.d.deinit(gpa);
            self.* = undefined;
        }
    };
};

// ---------------------------------------------------------------------------
// §2.1 The AEAD layer: one primitive per volume, dispatched on the mode.
// ---------------------------------------------------------------------------

/// Seal `m` into `c` (same length; `c.ptr == m.ptr` is legal) with `tag`.
pub fn seal(enc: Encryption, c: []u8, tag: *[TAG_LEN]u8, m: []const u8, ad: []const u8, nonce: [NONCE_LEN]u8, key: [DATA_KEY_LEN]u8) void {
    switch (enc) {
        .none => unreachable,
        .aes256gcm => Aes256Gcm.encrypt(c, tag, m, ad, nonce, key),
        .chacha20poly1305 => ChaCha20Poly1305.encrypt(c, tag, m, ad, nonce, key),
    }
}

/// Open `c` into `m` (`m.ptr == c.ptr` is legal: both primitives are
/// stream ciphers that read a block before writing it — pinned by a test).
pub fn open(enc: Encryption, m: []u8, c: []const u8, tag: [TAG_LEN]u8, ad: []const u8, nonce: [NONCE_LEN]u8, key: [DATA_KEY_LEN]u8) error{AuthenticationFailed}!void {
    switch (enc) {
        .none => unreachable,
        .aes256gcm => try Aes256Gcm.decrypt(m, c, tag, ad, nonce, key),
        .chacha20poly1305 => try ChaCha20Poly1305.decrypt(m, c, tag, ad, nonce, key),
    }
}

// ---------------------------------------------------------------------------
// §3.1 The key wrap `hkdf-sha256/aead-kw/v2`.
// ---------------------------------------------------------------------------

/// What HKDF yields from the key file and the volume's salt. Zeroized by
/// its owner; no `format` method (a key type never prints).
pub const DerivedKey = struct {
    kek: [KEK_LEN]u8,
    key_id: [KEY_ID_LEN]u8,

    pub fn zeroize(self: *DerivedKey) void {
        std.crypto.secureZero(u8, &self.kek);
    }
};

/// `PRK = Extract(salt, material)`; `KEK = Expand(PRK, KEK_INFO, 32)`;
/// `key_id = Expand(PRK, KEY_ID_INFO, 8)`. Deterministic and salt-bound.
pub fn deriveVolumeKey(material: []const u8, salt: *const [KDF_SALT_LEN]u8) DerivedKey {
    var prk = HkdfSha256.extract(salt, material);
    defer std.crypto.secureZero(u8, &prk);
    var out: DerivedKey = undefined;
    HkdfSha256.expand(&out.kek, KEK_INFO, prk);
    HkdfSha256.expand(&out.key_id, KEY_ID_INFO, prk);
    return out;
}

/// `"SK" ‖ 0x02 ‖ nonce ‖ seal(KEK, nonce, aad = key_id, data_key) ‖ tag`.
pub fn wrapDataKey(enc: Encryption, kek: [KEK_LEN]u8, key_id: [KEY_ID_LEN]u8, nonce: [NONCE_LEN]u8, data_key: [DATA_KEY_LEN]u8) [WRAP_V2_LEN]u8 {
    var blob: [WRAP_V2_LEN]u8 = undefined;
    blob[0] = 'S';
    blob[1] = 'K';
    blob[2] = KEY_WRAP_SCHEME_V2;
    blob[3..][0..NONCE_LEN].* = nonce;
    var tag: [TAG_LEN]u8 = undefined;
    seal(enc, blob[3 + NONCE_LEN ..][0..DATA_KEY_LEN], &tag, &data_key, &key_id, nonce, kek);
    blob[3 + NONCE_LEN + DATA_KEY_LEN ..][0..TAG_LEN].* = tag;
    return blob;
}

pub const UnwrapError = error{ MalformedKeyBlob, UnsupportedKeyWrapVersion, ForeignKey };

pub fn writeUnwrapRefusal(w: *std.Io.Writer, e: UnwrapError, blob: []const u8) std.Io.Writer.Error!void {
    switch (e) {
        error.MalformedKeyBlob => try w.print("malformed key-wrap blob ({d} bytes)", .{blob.len}),
        error.UnsupportedKeyWrapVersion => try w.print("unsupported key-wrap version {d} (this binary writes version {d})", .{ if (blob.len > 2) blob[2] else 0, KEY_WRAP_SCHEME_V2 }),
        error.ForeignKey => try w.writeAll("the block was written under a different encryption key (or the header is corrupt)"),
    }
}

/// Inverse of `wrapDataKey`. Total over attacker bytes: `len < 15 ∨ magic ≠
/// "SK"` ⇒ malformed; `blob[2] ≠ 2` ⇒ unsupported version; the AEAD's
/// verdict ⇒ foreign key. The 63 B fixed width is the v2 law.
pub fn unwrapDataKey(enc: Encryption, kek: [KEK_LEN]u8, key_id: [KEY_ID_LEN]u8, blob: []const u8) UnwrapError![DATA_KEY_LEN]u8 {
    if (blob.len < 3 + NONCE_LEN or blob[0] != 'S' or blob[1] != 'K') return error.MalformedKeyBlob;
    if (blob[2] != KEY_WRAP_SCHEME_V2) return error.UnsupportedKeyWrapVersion;
    if (blob.len != WRAP_V2_LEN) return error.MalformedKeyBlob;
    const nonce: [NONCE_LEN]u8 = blob[3..][0..NONCE_LEN].*;
    const ct = blob[3 + NONCE_LEN ..][0..DATA_KEY_LEN];
    const tag: [TAG_LEN]u8 = blob[3 + NONCE_LEN + DATA_KEY_LEN ..][0..TAG_LEN].*;
    var key: [DATA_KEY_LEN]u8 = undefined;
    open(enc, &key, ct, tag, &key_id, nonce, kek) catch {
        std.crypto.secureZero(u8, &key);
        return error.ForeignKey;
    };
    return key;
}

// ---------------------------------------------------------------------------
// §3.2–§3.4 Key material: resolution order, the key-file laws (validated
// on the FD, no TOCTOU), process hardening. Linux only.
// ---------------------------------------------------------------------------

pub const KeySource = enum {
    /// `--encrypt-key <path|->`.
    flag,
    /// `SQUEEZEFS_ENCRYPT_KEY_FILE`.
    env,
    /// `/etc/squeezefs/keys/<key_id>.key`.
    key_dir,
};

pub const KeyFileError = error{
    /// The path is a symlink (`O_NOFOLLOW` ⇒ ELOOP).
    KeyFileIsSymlink,
    KeyFileOpenFailed,
    KeyFileStatFailed,
    KeyFileNotRegular,
    /// `mode & 0o077 != 0`.
    KeyFilePermissions,
    /// `st_uid ∉ {euid, 0}`.
    KeyFileOwner,
    KeyFileTooLarge,
    KeyFileReadFailed,
    /// Trimmed material shorter than 32 B.
    KeyMaterialTooShort,
    /// `-` on stdin produced more than `MAX_KEY_FILE_BYTES`.
    KeyStdinTooLarge,
    /// No `--encrypt-key`, no env path, no key-dir file.
    NoKeySource,
    /// `derive(material, salt).key_id ≠ the recorded key_id`.
    KeyMismatch,
};

pub fn writeKeyFileRefusal(w: *std.Io.Writer, e: KeyFileError, path: []const u8) std.Io.Writer.Error!void {
    switch (e) {
        error.KeyFileIsSymlink => try w.print("encryption key file {s} is a symlink — refusing to follow it", .{path}),
        error.KeyFileOpenFailed => try w.print("encryption key file {s} cannot be opened", .{path}),
        error.KeyFileStatFailed => try w.print("encryption key file {s} cannot be stat'ed", .{path}),
        error.KeyFileNotRegular => try w.print("encryption key file {s} is not a regular file", .{path}),
        error.KeyFilePermissions => try w.print("encryption key file {s} is readable or writable by group/other — `chmod 600` it", .{path}),
        error.KeyFileOwner => try w.print("encryption key file {s} is owned by neither the invoking user nor root", .{path}),
        error.KeyFileTooLarge => try w.print("encryption key file {s} is larger than {d} bytes — that is not a key file", .{ path, MAX_KEY_FILE_BYTES }),
        error.KeyFileReadFailed => try w.print("encryption key file {s} could not be read", .{path}),
        error.KeyMaterialTooShort => try w.print("encryption key file {s} holds fewer than {d} bytes of material after trimming whitespace — generate one with `head -c 32 /dev/urandom | base64`", .{ path, MIN_KEY_MATERIAL_BYTES }),
        error.KeyStdinTooLarge => try w.print("key material on stdin exceeds {d} bytes — that is not a key file", .{MAX_KEY_FILE_BYTES}),
        error.NoKeySource => try w.print("no encryption key: pass --encrypt-key <path|->, set SQUEEZEFS_ENCRYPT_KEY_FILE=<path>, or install {s}", .{path}),
        error.KeyMismatch => try w.print("encryption key {s} is not the key this set was formatted with (its key id differs) — a wrong key file is refused at mount instead of failing every read", .{path}),
    }
}

/// The ASCII-whitespace trim of the file's bytes (both ends).
pub fn trimMaterial(raw: []const u8) []const u8 {
    return std.mem.trim(u8, raw, " \t\r\n\x0b\x0c");
}

/// `/etc/squeezefs/keys/<16 hex>.key` into `buf`.
pub fn defaultKeyPath(buf: *[128]u8, key_id: [KEY_ID_LEN]u8) [:0]const u8 {
    return std.fmt.bufPrintSentinel(buf, "{s}/{x}.key", .{ KEY_DIR, &key_id }, 0) catch unreachable;
}

pub const linux_keys = if (builtin.os.tag == .linux) struct {
    /// `read_key_file`: open `O_RDONLY|O_NOFOLLOW|O_CLOEXEC` → statx on the
    /// FD (regular; `mode & 0o077 == 0`; owner ∈ {euid, 0} for EVERY euid —
    /// the reference let root load a file ANY user planted, decision 0002
    /// row 120; size ≤ 64 KiB) → read → trim → ≥ 32 B. `out.len ≥
    /// MAX_KEY_FILE_BYTES`; the returned slice is the trimmed material
    /// inside `out`, which the caller zeroizes whole.
    pub fn readKeyFile(path: []const u8, out: []u8) KeyFileError![]const u8 {
        assert(out.len >= MAX_KEY_FILE_BYTES);
        hardenProcess();
        var zbuf: [linux.PATH_MAX]u8 = undefined;
        const z = pathZ(&zbuf, path) catch return error.KeyFileOpenFailed;
        const rc = linux.open(z, .{ .ACCMODE = .RDONLY, .NOFOLLOW = true, .CLOEXEC = true }, 0);
        switch (linux.errno(rc)) {
            .SUCCESS => {},
            .LOOP => return error.KeyFileIsSymlink,
            else => return error.KeyFileOpenFailed,
        }
        const fd: linux.fd_t = @intCast(rc);
        defer _ = linux.close(fd);
        var stx: linux.Statx = undefined;
        if (linux.errno(linux.statx(fd, "", linux.AT.EMPTY_PATH, .{ .TYPE = true, .MODE = true, .UID = true, .SIZE = true }, &stx)) != .SUCCESS) return error.KeyFileStatFailed;
        if (stx.mode & linux.S.IFMT != linux.S.IFREG) return error.KeyFileNotRegular;
        if (stx.mode & 0o077 != 0) return error.KeyFilePermissions;
        const euid = linux.geteuid();
        if (stx.uid != euid and stx.uid != 0) return error.KeyFileOwner;
        if (stx.size > MAX_KEY_FILE_BYTES) return error.KeyFileTooLarge;
        var n: usize = 0;
        while (n < out.len) {
            const r = linux.read(fd, out[n..].ptr, out.len - n);
            switch (linux.errno(r)) {
                .SUCCESS => {},
                .INTR => continue,
                else => return error.KeyFileReadFailed,
            }
            if (r == 0) break;
            n += r;
        }
        // The file grew past the stat'ed size while we read it: not a key file.
        if (n > MAX_KEY_FILE_BYTES) return error.KeyFileTooLarge;
        const material = trimMaterial(out[0..n]);
        if (material.len < MIN_KEY_MATERIAL_BYTES) return error.KeyMaterialTooShort;
        return material;
    }

    /// `-`: stdin up to `MAX + 1` bytes; more is not a key file.
    pub fn readKeyStdin(out: []u8) KeyFileError![]const u8 {
        assert(out.len > MAX_KEY_FILE_BYTES);
        hardenProcess();
        var n: usize = 0;
        while (n <= MAX_KEY_FILE_BYTES) {
            const r = linux.read(0, out[n..].ptr, @min(out.len - n, MAX_KEY_FILE_BYTES + 1 - n));
            switch (linux.errno(r)) {
                .SUCCESS => {},
                .INTR => continue,
                else => return error.KeyFileReadFailed,
            }
            if (r == 0) break;
            n += r;
        }
        if (n > MAX_KEY_FILE_BYTES) return error.KeyStdinTooLarge;
        const material = trimMaterial(out[0..n]);
        if (material.len < MIN_KEY_MATERIAL_BYTES) return error.KeyMaterialTooShort;
        return material;
    }

    pub const Resolved = struct {
        material: []const u8,
        source: KeySource,
        /// The path that served (for the refusal texts; "-" for stdin).
        path: []const u8,
    };

    /// §3.2: `--encrypt-key` → `SQUEEZEFS_ENCRYPT_KEY_FILE` → the key dir
    /// path of `key_id`; none ⇒ `NoKeySource` naming all three. `path_buf`
    /// holds the key-dir spelling; `out` the material (zeroize it).
    pub fn resolveKeyMaterial(env: *const std.process.Environ.Map, flag_path: ?[]const u8, key_id: [KEY_ID_LEN]u8, path_buf: *[128]u8, out: []u8) KeyFileError!Resolved {
        if (flag_path) |p| {
            if (std.mem.eql(u8, p, "-")) return .{ .material = try readKeyStdin(out), .source = .flag, .path = "-" };
            return .{ .material = try readKeyFile(p, out), .source = .flag, .path = p };
        }
        if (knobs.present(env.get("SQUEEZEFS_ENCRYPT_KEY_FILE"))) |p| {
            return .{ .material = try readKeyFile(p, out), .source = .env, .path = p };
        }
        const def = defaultKeyPath(path_buf, key_id);
        var zbuf: [linux.PATH_MAX]u8 = undefined;
        const z = pathZ(&zbuf, def) catch return error.NoKeySource;
        var stx: linux.Statx = undefined;
        if (linux.errno(linux.statx(linux.AT.FDCWD, z, linux.AT.SYMLINK_NOFOLLOW, .{ .TYPE = true }, &stx)) != .SUCCESS) return error.NoKeySource;
        return .{ .material = try readKeyFile(def, out), .source = .key_dir, .path = def };
    }

    /// Resolve + derive + verify the recorded `key_id` (a wrong key file
    /// is a loud refusal at mount instead of an AEAD failure on every
    /// read). The material is zeroized before return; the caller owns the
    /// `DerivedKey` and zeroizes it.
    pub fn resolveVolumeKey(env: *const std.process.Environ.Map, flag_path: ?[]const u8, salt: *const [KDF_SALT_LEN]u8, key_id: [KEY_ID_LEN]u8, path_buf: *[128]u8, served: *[]const u8) KeyFileError!DerivedKey {
        var material_buf: [MAX_KEY_FILE_BYTES + 1]u8 = undefined;
        defer std.crypto.secureZero(u8, &material_buf);
        const r = try resolveKeyMaterial(env, flag_path, key_id, path_buf, &material_buf);
        served.* = r.path;
        var derived = deriveVolumeKey(r.material, salt);
        if (!std.mem.eql(u8, &derived.key_id, &key_id)) {
            derived.zeroize();
            return error.KeyMismatch;
        }
        return derived;
    }

    var hardened_logged: std.atomic.Value(bool) = .init(false);

    /// VAL-7h: `prctl(PR_SET_DUMPABLE, 0)` + `RLIMIT_CORE = {0, 0}` on every
    /// key-touching call (never latched: a forked child whose flag was
    /// cleared gets it again), the info line once per process; a failure
    /// WARNS — a core dump of this daemon could spill key material. Never
    /// called on a plaintext mount.
    pub fn hardenProcess() void {
        const d = linux.prctl(@backingInt(linux.PR.SET_DUMPABLE), 0, 0, 0, 0);
        if (linux.errno(d) != .SUCCESS) log.warn("prctl(PR_SET_DUMPABLE, 0) failed ({t}): a core dump of this process could spill encryption key material", .{linux.errno(d)});
        const zero: linux.rlimit = .{ .cur = 0, .max = 0 };
        const r = linux.setrlimit(.CORE, &zero);
        if (linux.errno(r) != .SUCCESS) log.warn("setrlimit(RLIMIT_CORE, 0) failed ({t}): a core dump of this process could spill encryption key material", .{linux.errno(r)});
        // One line per process (the flag is a monotone latch; the ops above are not).
        if (hardened_logged.cmpxchgStrong(false, true, .monotonic, .monotonic) == null) {
            log.info("key material handled: process marked non-dumpable, core size 0", .{});
        }
    }

    fn pathZ(buf: *[linux.PATH_MAX]u8, path: []const u8) error{NameTooLong}![:0]const u8 {
        if (path.len >= buf.len) return error.NameTooLong;
        @memcpy(buf[0..path.len], path);
        buf[path.len] = 0;
        return buf[0..path.len :0];
    }
} else struct {};

// ---------------------------------------------------------------------------
// The per-mount crypto state: the session data key, its pre-wrapped blob,
// the KEK, the nonce word, the unwrap cache.
// ---------------------------------------------------------------------------

/// `max(16, writers_known × 2)`: one blob per writer-process incarnation
/// that still has live blocks; the floor is the shipped posture for a
/// single-writer set whose history spans a dozen remounts.
pub fn unwrapCacheCapacity(writers_known: u32) u32 {
    return @max(16, writers_known * 2);
}

pub const CryptoState = struct {
    encryption: Encryption,
    data_key: [DATA_KEY_LEN]u8,
    /// The KEY WRAP in force: the session key's blob under the current
    /// KEK and the unwrap arms (`Wrap`), published as an IMMUTABLE
    /// snapshot behind one pointer so a rotation never tears a 63 B blob
    /// a lane is copying into a header (release pairs with every reader's
    /// acquire; a retired wrap is zeroized only after the plane's codec
    /// barrier proved every reader of it returned — `retireWrap`).
    wrap: std.atomic.Value(*const Wrap),
    /// The wrap a rotation retired, kept until `retireWrap` (the plane's
    /// quiescence barrier) zeroizes it.
    retired: ?*Wrap = null,
    /// `nonce = salt ‖ BE counter`: the salt is a per-mount random prefix;
    /// the counter a monotone word starting at the config's durable floor.
    nonce_salt: [4]u8,
    nonce_counter: std.atomic.Value(u64),
    cache: []CacheEntry,
    gpa: Allocator,

    /// One key wrap: the current KEK + id, the session key's blob under it,
    /// and the PREVIOUS KEK + id while a rotation window is open (the
    /// unwrap-only arm of the retiring key). Immutable once published.
    pub const Wrap = struct {
        kek: [KEK_LEN]u8,
        key_id: [KEY_ID_LEN]u8,
        /// The session key wrapped under `kek`; rides every header.
        key_blob: [WRAP_V2_LEN]u8,
        prev: ?Prev,

        pub const Prev = struct { kek: [KEK_LEN]u8, key_id: [KEY_ID_LEN]u8 };

        fn zeroize(self: *Wrap) void {
            std.crypto.secureZero(u8, &self.kek);
            if (self.prev) |*p| std.crypto.secureZero(u8, &p.kek);
        }
    };

    /// Which key a header's blob is wrapped under.
    pub const BlobClass = enum { current, previous, foreign };

    const CacheEntry = struct {
        /// 0 empty → 1 filling → 2 ready (release; readers acquire). An
        /// entry is immutable once ready, so a reader that sees 2 reads a
        /// whole, consistent (blob, key) pair.
        state: std.atomic.Value(u8) = .init(0),
        blob: [WRAP_V2_LEN]u8 = @splat(0),
        key: [DATA_KEY_LEN]u8 = @splat(0),
    };

    pub const InitError = error{ OutOfMemory, RandomFailed };

    /// A fresh session key and salt from `io.randomSecure` (a failure
    /// refuses: never a weaker fallback, decision 0002 row 119); the blob
    /// pre-wrapped under `derived.kek` with a fresh wrap nonce. `derived`
    /// (and `prev`, the retiring key of an open rotation window) are
    /// copied; the caller zeroizes its own.
    pub fn init(gpa: Allocator, io: std.Io, encryption: Encryption, derived: DerivedKey, prev: ?DerivedKey, nonce_floor: u64, writers_known: u32) InitError!*CryptoState {
        assert(encryption != .none);
        const self = try gpa.create(CryptoState);
        errdefer gpa.destroy(self);
        const cache = try gpa.alloc(CacheEntry, unwrapCacheCapacity(writers_known));
        errdefer gpa.free(cache);
        @memset(cache, .{});
        const wrap = try gpa.create(Wrap);
        errdefer gpa.destroy(wrap);
        self.* = .{
            .encryption = encryption,
            .data_key = undefined,
            .wrap = .init(wrap),
            .nonce_salt = undefined,
            .nonce_counter = .init(nonce_floor),
            .cache = cache,
            .gpa = gpa,
        };
        io.randomSecure(&self.data_key) catch return error.RandomFailed;
        io.randomSecure(&self.nonce_salt) catch return error.RandomFailed;
        wrap.* = try self.wrapUnder(io, derived, if (prev) |p| .{ .kek = p.kek, .key_id = p.key_id } else null);
        return self;
    }

    fn wrapUnder(self: *CryptoState, io: std.Io, derived: DerivedKey, prev: ?Wrap.Prev) InitError!Wrap {
        var wrap_nonce: [NONCE_LEN]u8 = undefined;
        io.randomSecure(&wrap_nonce) catch return error.RandomFailed;
        return .{ .kek = derived.kek, .key_id = derived.key_id, .key_blob = wrapDataKey(self.encryption, derived.kek, derived.key_id, wrap_nonce, self.data_key), .prev = prev };
    }

    /// Zeroize every key-bearing byte, then free.
    pub fn deinit(self: *CryptoState) void {
        std.crypto.secureZero(u8, &self.data_key);
        const w = @constCast(self.wrap.load(.acquire));
        w.zeroize();
        self.gpa.destroy(w);
        if (self.retired) |r| {
            r.zeroize();
            self.gpa.destroy(r);
        }
        for (self.cache) |*e| std.crypto.secureZero(u8, &e.key);
        const gpa = self.gpa;
        gpa.free(self.cache);
        gpa.destroy(self);
    }

    /// The wrap in force (the writer's blob, the readers' unwrap arms).
    pub fn current(self: *const CryptoState) *const Wrap {
        // acquire pairs with the release of `beginRotation`'s store.
        return self.wrap.load(.acquire);
    }

    /// The session key's blob under the current KEK (every header's).
    pub fn keyBlob(self: *const CryptoState) [WRAP_V2_LEN]u8 {
        return self.current().key_blob;
    }

    pub fn keyId(self: *const CryptoState) [KEY_ID_LEN]u8 {
        return self.current().key_id;
    }

    /// A rotation window is open: the previous key is an unwrap arm.
    pub fn rotating(self: *const CryptoState) bool {
        return self.current().prev != null;
    }

    pub const RotateError = error{ OutOfMemory, RandomFailed, RotationOpen };

    /// Open the rotation window in RAM: from here every header wraps the
    /// session key under `next`; blocks under the old KEK still unwrap
    /// (`prev`). The old wrap is kept until `retireWrap` — the plane calls
    /// that behind its codec-pool barrier, which proves every reader that
    /// loaded the old pointer has returned. Refused while a window is open.
    pub fn beginRotation(self: *CryptoState, io: std.Io, next: DerivedKey) RotateError!void {
        const old = self.current();
        if (old.prev != null or self.retired != null) return error.RotationOpen;
        const wrap = try self.gpa.create(Wrap);
        errdefer self.gpa.destroy(wrap);
        wrap.* = try self.wrapUnder(io, next, .{ .kek = old.kek, .key_id = old.key_id });
        self.retired = @constCast(old);
        // release pairs with `current`'s acquire.
        self.wrap.store(wrap, .release);
    }

    /// Close the window in RAM: a wrap without the previous arm is
    /// published and the two-key wrap retired — its KEK (the previous key)
    /// is zeroized by `retireWrap`, which the caller runs behind its
    /// quiescence barrier (a reader may still hold the retired pointer).
    /// Only after the sweep's last publish is durable and the config
    /// flipped. A retired wrap of an earlier `beginRotation` nobody retired
    /// is zeroized first (safe: the window's whole span has passed since).
    pub fn endRotation(self: *CryptoState) void {
        const cur = self.current();
        if (cur.prev == null) return;
        const wrap = self.gpa.create(Wrap) catch {
            // Without a fresh snapshot the arm stays: a future `endRotation`
            // retries; the key is still forgotten at `deinit`.
            return;
        };
        wrap.* = .{ .kek = cur.kek, .key_id = cur.key_id, .key_blob = cur.key_blob, .prev = null };
        self.retireWrap();
        self.retired = @constCast(cur);
        // release pairs with `current`'s acquire.
        self.wrap.store(wrap, .release);
    }

    /// Zeroize + free the wrap a rotation retired. Safe only once no reader
    /// can hold it: the plane runs it behind its codec-pool barrier.
    pub fn retireWrap(self: *CryptoState) void {
        const r = self.retired orelse return;
        self.retired = null;
        r.zeroize();
        self.gpa.destroy(r);
    }

    /// The next block nonce. Exhaustion of the 64-bit counter is
    /// unreachable (2^64 blocks) and still a tripwire.
    pub fn nextNonce(self: *CryptoState) [NONCE_LEN]u8 {
        return self.nextNonces(1);
    }

    /// A RANGE of `count` counters (one per frame of a block): the base
    /// nonce; frame i's is `frameNonce(base, i)`.
    pub fn nextNonces(self: *CryptoState, count: u32) [NONCE_LEN]u8 {
        const step: u64 = @max(count, 1);
        const n = self.nonce_counter.fetchAdd(step, .monotonic);
        if (n > std.math.maxInt(u64) - step) stats.crypto_nonce_exhaustion.fire("nonce counter wrapped");
        var out: [NONCE_LEN]u8 = undefined;
        out[0..4].* = self.nonce_salt;
        std.mem.writeInt(u64, out[4..12], n, .big);
        return out;
    }

    /// The live counter word (the next record's durable floor; a gauge).
    pub fn nonceCounter(self: *const CryptoState) u64 {
        return self.nonce_counter.load(.monotonic);
    }

    /// Which key wrapped `blob`: the current KEK, the previous (an open
    /// window's retiring key), or neither. An AEAD open per arm; the
    /// rotation sweep's classifier (a cold path).
    pub fn classify(self: *const CryptoState, blob: *const [WRAP_V2_LEN]u8) BlobClass {
        const w = self.current();
        if (std.mem.eql(u8, blob, &w.key_blob)) return .current;
        if (unwrapDataKey(self.encryption, w.kek, w.key_id, blob)) |k| {
            var key = k;
            std.crypto.secureZero(u8, &key);
            return .current;
        } else |_| {}
        if (w.prev) |p| if (unwrapDataKey(self.encryption, p.kek, p.key_id, blob)) |k| {
            var key = k;
            std.crypto.secureZero(u8, &key);
            return .previous;
        } else |_| {};
        return .foreign;
    }

    /// The data key a header's blob names: our own blob is the fast path
    /// (every block this mount wrote); a foreign blob (an earlier
    /// incarnation's) unwraps under the KEK — or, inside a rotation
    /// window, under the previous KEK — through the cache.
    pub fn keyFor(self: *CryptoState, blob: *const [WRAP_V2_LEN]u8) UnwrapError![DATA_KEY_LEN]u8 {
        const w = self.current();
        if (std.mem.eql(u8, blob, &w.key_blob)) return self.data_key;
        stats.crypto_key_unwraps.inc();
        for (self.cache) |*e| {
            if (e.state.load(.acquire) != 2) continue;
            if (std.mem.eql(u8, &e.blob, blob)) {
                stats.crypto_key_unwrap_hits.inc();
                return e.key;
            }
        }
        const key = unwrapDataKey(self.encryption, w.kek, w.key_id, blob) catch |e| blk: {
            const p = w.prev orelse return e;
            if (e != error.ForeignKey) return e;
            stats.crypto_key_unwraps_previous.inc();
            break :blk try unwrapDataKey(self.encryption, p.kek, p.key_id, blob);
        };
        // Claim an empty slot (CAS 0 → 1); fill; publish (release 2 — paired
        // with the readers' acquire above). A full table just does not cache.
        for (self.cache) |*e| {
            if (e.state.cmpxchgStrong(0, 1, .acquire, .monotonic) != null) continue;
            e.blob = blob.*;
            e.key = key;
            e.state.store(2, .release);
            return key;
        }
        stats.crypto_unwrap_cache_full.inc();
        return key;
    }
};

// ---------------------------------------------------------------------------
// The transform: processWrite / processRead.
// ---------------------------------------------------------------------------

pub const WriteError = error{
    /// `plain.len > block_size`.
    PlainTooLarge,
    /// `scratch.len < scratchLen(block_size)`.
    ScratchTooSmall,
    /// An encrypted volume with no crypto state (a caller bug).
    NoKey,
    /// The zstd library refused for a reason other than space.
    CompressFailed,
};

pub const ReadError = DecodeError || error{
    /// `out.len < scratchLen(block_size)`.
    ScratchTooSmall,
    NoKey,
};

/// The volume's IMMUTABLE transform half: the declared modes, the key state
/// (its own atomics) and the raw lever. Shared by every lane; a `Codec`
/// carries the per-lane state and does the work.
pub const Transform = struct {
    cfg: VolumeTransform,
    /// Present iff `cfg.encryption != .none`.
    crypto: ?*CryptoState,
    /// `SQUEEZEFS_TEST_TRANSFORM_FORCE_RAW`: every block takes the raw arm.
    force_raw: bool,

    pub const InitError = error{KeyPostureMismatch};

    pub fn init(env: *const std.process.Environ.Map, cfg: VolumeTransform, crypto: ?*CryptoState) InitError!Transform {
        if ((cfg.encryption != .none) != (crypto != null)) return error.KeyPostureMismatch;
        if (crypto) |c| if (c.encryption != cfg.encryption) return error.KeyPostureMismatch;
        return .{
            .cfg = cfg,
            .crypto = crypto,
            .force_raw = knobs.boolKnob("SQUEEZEFS_TEST_TRANSFORM_FORCE_RAW", env),
        };
    }

    /// Whether a `Codec` of this volume carries state between calls (zstd's
    /// static contexts); lz4 and the AEADs are pure functions of their
    /// arguments, so a stateless volume's codecs are interchangeable.
    pub fn codecHasState(self: *const Transform) bool {
        return self.cfg.compression == .zstd;
    }
};

/// The per-LANE codec: the zstd contexts (iff the volume compresses with
/// zstd) over the shared `Transform`. One codec is one thread's worth of
/// concurrency — the library's contract — and it holds nothing a caller
/// needs past a call's return (the image lives in the caller's scratch).
pub const Codec = struct {
    t: *const Transform,
    zstd: ?zstd.Context,

    pub const InitError = zstd.InitError;

    pub fn init(gpa: Allocator, t: *const Transform) InitError!Codec {
        return .{ .t = t, .zstd = if (t.codecHasState()) try zstd.Context.init(gpa) else null };
    }

    pub fn deinit(self: *Codec, gpa: Allocator) void {
        if (self.zstd) |*z| z.deinit(gpa);
        self.* = undefined;
    }

    pub fn cfg(self: *const Codec) VolumeTransform {
        return self.t.cfg;
    }

    /// The image of `plain` (`len ≤ block_size`): on a passthrough volume
    /// `plain` itself (same pointer, no frame); otherwise `header page ‖
    /// frames ‖ zero pad to 4 KiB` inside `scratch` (`len ≥ scratchLen(block)`),
    /// exactly ONE pooled buffer: every frame's body is compressed straight
    /// into its final offset and sealed in place; the input is never mutated.
    pub fn processWrite(self: *Codec, plain: []const u8, scratch: []u8) WriteError![]const u8 {
        const t = self.t;
        if (plain.len > t.cfg.block_size) return error.PlainTooLarge;
        if (t.cfg.isPassthrough()) return plain;
        if (scratch.len < scratchLen(t.cfg.block_size)) return error.ScratchTooSmall;
        const frames = framesFor(@intCast(plain.len));
        const encrypted = t.cfg.encryption != .none;
        // 1. Every frame: compress best-effort into its final place — `n ≥
        //    len` (equal too: an equal-size body is pure decompress cost for
        //    zero gain) ⇒ the raw plaintext with the entry's flag; a tag's
        //    room is reserved after each body on an encrypted volume.
        var entries: [MAX_FRAMES]Entry = undefined;
        const table_len = tableLen(frames);
        var off: usize = table_len;
        var i: u32 = 0;
        while (i < frames) : (i += 1) {
            const p = plain[@as(usize, i) * FRAME_GRAIN ..][0..@min(FRAME_GRAIN, plain.len - @as(usize, i) * FRAME_GRAIN)];
            const body = scratch[off..];
            var flags: u8 = 0;
            var inner_len: usize = p.len;
            if (t.cfg.compression != .none and !t.force_raw) {
                const t0 = clock.monotonicNs();
                const bounded = body[0 .. p.len - 1];
                const n: ?usize = switch (t.cfg.compression) {
                    .none => unreachable,
                    .lz4 => lz4.compressBlock(p, bounded) catch null,
                    .zstd => self.zstd.?.c.compress(p, bounded) catch |e| switch (e) {
                        error.OutputTooSmall => null,
                        error.ZstdFailed => return error.CompressFailed,
                    },
                };
                stats.transform_phase_ns_compress.record(clock.monotonicNs() -| t0);
                if (n) |len| {
                    inner_len = len;
                } else {
                    flags |= FLAG_STORE_RAW;
                    stats.compress_stored_raw.inc();
                }
            } else {
                flags |= FLAG_STORE_RAW;
                if (t.cfg.compression != .none) stats.compress_stored_raw.inc();
            }
            if (flags & FLAG_STORE_RAW != 0) @memcpy(body[0..p.len], p);
            const body_len = inner_len + (if (encrypted) TAG_LEN else 0);
            entries[i] = .{ .off = @intCast(off), .body_len = @intCast(body_len), .plain_len = @intCast(p.len), .flags = flags };
            off += body_len;
        }
        // 2. The header (the AAD's first part) and the table with its hash64;
        //    frame 0's body starts right after.
        var h: Header = .{ .flags = 0, .compression = t.cfg.compression, .encryption = t.cfg.encryption, .body_len = @intCast(off - table_len), .plain_len = @intCast(plain.len), .key_blob = @splat(0), .nonce = @splat(0) };
        if (encrypted) {
            const c = t.crypto orelse return error.NoKey;
            h.flags |= FLAG_ENCRYPTED;
            h.key_blob = c.keyBlob();
            h.nonce = c.nextNonces(frames);
        }
        h.encode(scratch[0..HEADER_LEN]);
        const entries_bytes = scratch[HEADER_LEN + TABLE_HEAD_LEN ..][0 .. @as(usize, frames) * ENTRY_LEN];
        i = 0;
        while (i < frames) : (i += 1) entries[i].encode(entries_bytes[@as(usize, i) * ENTRY_LEN ..][0..ENTRY_LEN]);
        var tw: FrameTableWire = undefined;
        std.mem.writeInt(u64, &tw.entries_csum_le, checksum.hash64(entries_bytes), .little);
        std.mem.writeInt(u16, &tw.frame_count_le, @intCast(frames), .little);
        tw.frame_grain_log2 = FRAME_GRAIN_LOG2;
        tw.reserved = @splat(0);
        scratch[HEADER_LEN..][0..TABLE_HEAD_LEN].* = std.mem.toBytes(tw);
        // 3. Seal every frame in place under its own nonce and AAD.
        if (encrypted) {
            const c = t.crypto.?;
            const t1 = clock.monotonicNs();
            i = 0;
            while (i < frames) : (i += 1) {
                const e = entries[i];
                const inner_len = e.body_len - TAG_LEN;
                const body = scratch[e.off..][0..e.body_len];
                var aad: [HEADER_LEN + ENTRY_LEN + 4]u8 = undefined;
                e.aad(scratch[0..HEADER_LEN], i, &aad);
                var tag: [TAG_LEN]u8 = undefined;
                seal(t.cfg.encryption, body[0..inner_len], &tag, body[0..inner_len], &aad, frameNonce(h.nonce, i), c.data_key);
                body[inner_len..][0..TAG_LEN].* = tag;
            }
            stats.transform_phase_ns_seal.record(clock.monotonicNs() -| t1);
        }
        // 4. Zero pad to the DMA grain (recycled scratch bytes never reach
        //    the device).
        const padded: usize = @intCast(derive.alignUp(off, DMA_GRAIN));
        @memset(scratch[off..padded], 0);
        if (padded > chunkSizeFor(t.cfg)) {
            // Unreachable by construction (the geometry law); the belt.
            stats.stored_image_overflow_refusals.fire("padded image exceeds the chunk");
            return error.PlainTooLarge;
        }
        return scratch[0..padded];
    }

    /// The plaintext of `image` (a device window: trailing bytes beyond
    /// the last frame are ignored by construction). On a passthrough
    /// volume `image` itself. Otherwise a view into `out` (`len ≥` the
    /// image's plaintext — the decoder writes exactly each frame's content,
    /// so a block-sized seed buffer holds any image of the block): every
    /// frame is opened IN PLACE in `image` (when
    /// encrypted) then decompressed, or copied when raw, into its plaintext
    /// offset. Every refusal counts on its tripwire.
    pub fn processRead(self: *Codec, image: []u8, out: []u8) ReadError![]const u8 {
        if (self.t.cfg.isPassthrough()) return image;
        return self.readInner(image, out) catch |e| {
            switch (e) {
                error.ScratchTooSmall, error.NoKey => {},
                else => |de| stats.fireRefusal(refusalOf(de)),
            }
            return e;
        };
    }

    /// The read without the counting (the fuzz target).
    pub fn readInner(self: *Codec, image: []u8, out: []u8) ReadError![]const u8 {
        const cfg_ = self.t.cfg;
        const h = try decodeHeader(cfg_, image);
        if (out.len < h.plain_len) return error.ScratchTooSmall;
        const tb = try decodeTable(cfg_, h, image);
        if (image.len < tableLen(tb.count) + @as(usize, h.body_len)) return error.FrameLen;
        if (tb.count == 0) return out[0..0];
        const span: FrameSpan = .{ .first = 0, .last = tb.count - 1, .lo = 0, .hi = image.len, .plain_base = 0, .plain_end = h.plain_len };
        return self.decodeSpan(h, tb, image, image, 0, span, out);
    }

    /// The SUB-BLOCK read (decision 0003 §K7): `page` is the image's first
    /// grain (the table), `window` the image bytes `[window_off, …)` the
    /// second DMA landed covering `span` (from `frameSpan`); the frames
    /// `first..=last` decode contiguously into `out` (`len ≥ the span's
    /// plaintext) and the view returned starts at `span.plain_base`.
    /// Counted refusals as `processRead`.
    pub fn decodeFrames(self: *Codec, page: []const u8, window: []u8, window_off: u64, span: FrameSpan, out: []u8) ReadError![]const u8 {
        return self.decodeFramesInner(page, window, window_off, span, out) catch |e| {
            switch (e) {
                error.ScratchTooSmall, error.NoKey => {},
                else => |de| stats.fireRefusal(refusalOf(de)),
            }
            return e;
        };
    }

    pub fn decodeFramesInner(self: *Codec, page: []const u8, window: []u8, window_off: u64, span: FrameSpan, out: []u8) ReadError![]const u8 {
        const cfg_ = self.t.cfg;
        const h = try decodeHeader(cfg_, page);
        const tb = try decodeTable(cfg_, h, page);
        if (span.first > span.last or span.last >= tb.count) return error.FrameTable;
        const a = tb.get(span.first);
        const b = tb.get(span.last);
        if (a.off < window_off or @as(u64, b.off) + b.body_len > window_off + window.len) return error.FrameLen;
        if (out.len < @as(u64, span.last - span.first + 1) * FRAME_GRAIN) return error.ScratchTooSmall;
        return self.decodeSpan(h, tb, page, window, window_off, span, out);
    }

    /// Frames `span.first..=span.last` of the image whose header page is
    /// `page`, bodies in `window` (image bytes from `window_off`), into
    /// `out` at `(i − first) × grain`; the plaintext view returned.
    fn decodeSpan(self: *Codec, h: Header, tb: Table, page: []const u8, window: []u8, window_off: u64, span: FrameSpan, out: []u8) ReadError![]const u8 {
        var key: [DATA_KEY_LEN]u8 = undefined;
        if (h.encrypted()) {
            const c = self.t.crypto orelse return error.NoKey;
            key = c.keyFor(&h.key_blob) catch return error.FrameKey;
        }
        var total: usize = 0;
        var i: u32 = span.first;
        while (i <= span.last) : (i += 1) {
            const e = tb.get(i);
            const body = window[@intCast(e.off - window_off)..][0..e.body_len];
            const dst = out[total..];
            var inner: []u8 = body;
            if (h.encrypted()) {
                const ct = body[0 .. body.len - TAG_LEN];
                const tag: [TAG_LEN]u8 = body[body.len - TAG_LEN ..][0..TAG_LEN].*;
                var aad: [HEADER_LEN + ENTRY_LEN + 4]u8 = undefined;
                e.aad(page[0..HEADER_LEN], i, &aad);
                const t0 = clock.monotonicNs();
                // Raw ⇒ open straight into the destination; compressed ⇒ in place.
                const dest: []u8 = if (e.storeRaw()) dst[0..ct.len] else ct;
                open(self.t.cfg.encryption, dest, ct, tag, &aad, frameNonce(h.nonce, i), key) catch return error.FrameAuth;
                stats.transform_phase_ns_open.record(clock.monotonicNs() -| t0);
                inner = dest;
            }
            if (e.storeRaw()) {
                if (inner.ptr != dst.ptr) @memcpy(dst[0..e.plain_len], inner[0..e.plain_len]);
            } else {
                const t1 = clock.monotonicNs();
                defer stats.transform_phase_ns_decompress.record(clock.monotonicNs() -| t1);
                switch (h.compression) {
                    .none => unreachable, // the table decoder refused a non-raw `none`
                    .lz4 => lz4.decompressBlock(inner, dst[0..e.plain_len]) catch return error.FrameBody,
                    .zstd => self.zstd.?.d.decompress(inner, dst, e.plain_len) catch return error.FrameBody,
                }
            }
            total += e.plain_len;
        }
        return out[0..total];
    }
};

// ---------------------------------------------------------------------------
// The mount-owned pools (decision 0003 §K10): codecs per lane, scratches
// per concurrent stored image. Both are Vyukov free lists under a
// semaphore — the fast path is one pop and one push; an acquirer that
// finds the pool empty parks on the semaphore (counted) and never
// allocates. The release pushes the item BEFORE it returns the permit, so
// a permit holder always finds an item (the wrap-around cell a popper has
// claimed but not yet re-sequenced is nanoseconds away: spin).
// ---------------------------------------------------------------------------

pub const CodecPool = struct {
    codecs: []Codec,
    free: mpmc.BoundedMpmc(*Codec),
    permits: semaphore.Semaphore,
    /// A stateless volume's codecs are interchangeable: every acquire is
    /// `&codecs[0]` with no atomics (lz4 / AEAD-only volumes).
    shared: bool,

    pub const InitError = Codec.InitError;

    /// `n ≥ 1` codecs over `t` (`codecPoolSize(lanes)`); a stateless volume
    /// allocates ONE whatever `n` says.
    pub fn init(gpa: Allocator, t: *const Transform, n: usize) InitError!CodecPool {
        const shared = !t.codecHasState();
        const count: usize = if (shared) 1 else @max(n, 1);
        const codecs = try gpa.alloc(Codec, count);
        errdefer gpa.free(codecs);
        var built: usize = 0;
        errdefer for (codecs[0..built]) |*c| c.deinit(gpa);
        for (codecs) |*c| {
            c.* = try Codec.init(gpa, t);
            built += 1;
        }
        var free = try mpmc.BoundedMpmc(*Codec).init(gpa, count);
        errdefer free.deinit(gpa);
        for (codecs) |*c| free.pushSpin(c);
        return .{ .codecs = codecs, .free = free, .permits = semaphore.Semaphore.init(count), .shared = shared };
    }

    pub fn deinit(self: *CodecPool, gpa: Allocator) void {
        for (self.codecs) |*c| c.deinit(gpa);
        gpa.free(self.codecs);
        self.free.deinit(gpa);
        self.* = undefined;
    }

    pub fn capacity(self: *const CodecPool) usize {
        return self.codecs.len;
    }

    /// A codec of our own until `release`; parks only past the capacity
    /// (counted), never allocates.
    pub fn acquire(self: *CodecPool, io: std.Io) *Codec {
        if (self.shared) return &self.codecs[0];
        if (self.permits.tryAcquire(io, 1) == null) {
            stats.transform_codec_pool_waits.inc();
            _ = self.permits.acquireUncancelable(io, 1);
        }
        while (true) {
            if (self.free.tryPop()) |c| return c;
            std.atomic.spinLoopHint();
        }
    }

    pub fn release(self: *CodecPool, io: std.Io, c: *Codec) void {
        if (self.shared) return;
        self.free.pushSpin(c);
        self.permits.release(io, 1);
    }
};

/// Stored-image scratches (`scratchLen(block)` each, DMA-aligned): a
/// buffer is held from the transform through the DMA its image feeds.
pub const ScratchPool = struct {
    pub const Buf = []align(DMA_GRAIN) u8;

    bufs: []Buf,
    free: mpmc.BoundedMpmc(Buf),
    permits: semaphore.Semaphore,

    /// `n ≥ 1` buffers of `len` bytes (`scratchPoolCap(cpus)` × `scratchLen`).
    pub fn init(gpa: Allocator, n: usize, len: usize) Allocator.Error!ScratchPool {
        const count: usize = @max(n, 1);
        const bufs = try gpa.alloc(Buf, count);
        errdefer gpa.free(bufs);
        var built: usize = 0;
        errdefer for (bufs[0..built]) |b| gpa.free(b);
        for (bufs) |*b| {
            b.* = try gpa.alignedAlloc(u8, .fromByteUnits(DMA_GRAIN), len);
            built += 1;
        }
        var free = try mpmc.BoundedMpmc(Buf).init(gpa, count);
        errdefer free.deinit(gpa);
        for (bufs) |b| free.pushSpin(b);
        return .{ .bufs = bufs, .free = free, .permits = semaphore.Semaphore.init(count) };
    }

    pub fn deinit(self: *ScratchPool, gpa: Allocator) void {
        for (self.bufs) |b| gpa.free(b);
        gpa.free(self.bufs);
        self.free.deinit(gpa);
        self.* = undefined;
    }

    pub fn capacity(self: *const ScratchPool) usize {
        return self.bufs.len;
    }

    pub fn acquire(self: *ScratchPool, io: std.Io) Buf {
        if (self.permits.tryAcquire(io, 1) == null) {
            stats.transform_scratch_pool_waits.inc();
            _ = self.permits.acquireUncancelable(io, 1);
        }
        while (true) {
            if (self.free.tryPop()) |b| return b;
            std.atomic.spinLoopHint();
        }
    }

    pub fn release(self: *ScratchPool, io: std.Io, b: Buf) void {
        self.free.pushSpin(b);
        self.permits.release(io, 1);
    }
};

// ---------------------------------------------------------------------------
// Observability (spec §6.2).
// ---------------------------------------------------------------------------

pub const stats = struct {
    /// Blocks that took the raw escape: ≈ the incompressible share of the
    /// write mix; a compressible workload keeps it ~0.
    pub var compress_stored_raw: counters.ShardedCounter = .{};
    pub var crypto_key_unwraps: counters.ShardedCounter = .{};
    pub var crypto_key_unwrap_hits: counters.ShardedCounter = .{};
    /// Unwraps that fell to the PREVIOUS key of an open rotation window
    /// (blocks the sweep has not yet rewritten): 0 outside a rotation.
    pub var crypto_key_unwraps_previous: counters.ShardedCounter = .{};
    pub var crypto_unwrap_cache_full: counters.ShardedCounter = .{};
    pub var crypto_nonce_exhaustion: counters.Tripwire = .init("crypto_nonce_exhaustion");
    pub var stored_image_overflow_refusals: counters.Tripwire = .init("stored_image_overflow_refusals");
    pub var transform_frame_refusals_len: counters.Tripwire = .init("transform_frame_refusals_len");
    pub var transform_plain_len_refusals: counters.Tripwire = .init("transform_plain_len_refusals");
    pub var transform_frame_refusals_magic: counters.Tripwire = .init("transform_frame_refusals_magic");
    pub var transform_frame_refusals_version: counters.Tripwire = .init("transform_frame_refusals_version");
    pub var transform_frame_refusals_flags: counters.Tripwire = .init("transform_frame_refusals_flags");
    pub var transform_frame_refusals_mode: counters.Tripwire = .init("transform_frame_refusals_mode");
    pub var transform_frame_refusals_aad: counters.Tripwire = .init("transform_frame_refusals_aad");
    pub var transform_frame_refusals_key: counters.Tripwire = .init("transform_frame_refusals_key");
    pub var transform_frame_refusals_body: counters.Tripwire = .init("transform_frame_refusals_body");
    pub var transform_frame_refusals_table: counters.Tripwire = .init("transform_frame_refusals_table");
    /// Acquires that found the pool empty and parked: 0 by derivation for
    /// the codecs (one per lane); for the scratches the write side's
    /// concurrency past `scratchPoolCap` (a park, never a heap spill).
    pub var transform_codec_pool_waits: counters.ShardedCounter = .{};
    pub var transform_scratch_pool_waits: counters.ShardedCounter = .{};
    pub var transform_phase_ns_compress: counters.ShardedLatencyHistogram = .{};
    pub var transform_phase_ns_seal: counters.ShardedLatencyHistogram = .{};
    pub var transform_phase_ns_open: counters.ShardedLatencyHistogram = .{};
    pub var transform_phase_ns_decompress: counters.ShardedLatencyHistogram = .{};

    pub fn refusalTripwire(r: FrameRefusal) *counters.Tripwire {
        return switch (r) {
            .len => &transform_frame_refusals_len,
            .plain_len => &transform_plain_len_refusals,
            .magic => &transform_frame_refusals_magic,
            .version => &transform_frame_refusals_version,
            .flags => &transform_frame_refusals_flags,
            .mode => &transform_frame_refusals_mode,
            .aad => &transform_frame_refusals_aad,
            .key => &transform_frame_refusals_key,
            .body => &transform_frame_refusals_body,
            .table => &transform_frame_refusals_table,
        };
    }

    fn fireRefusal(r: FrameRefusal) void {
        refusalTripwire(r).fire(@tagName(r));
    }
};

// ---------------------------------------------------------------------------
// Tests.
// ---------------------------------------------------------------------------

const testing = std.testing;

fn testEnv() std.process.Environ.Map {
    return std.process.Environ.Map.init(testing.allocator);
}

fn fill(buf: []u8, seed: u64) void {
    var prng = std.Random.DefaultPrng.init(seed);
    prng.random().bytes(buf);
}

/// A compressible payload: runs of a slowly varying byte with a sprinkle of noise.
fn fillCompressible(buf: []u8, seed: u64) void {
    var prng = std.Random.DefaultPrng.init(seed);
    const r = prng.random();
    var i: usize = 0;
    while (i < buf.len) {
        const run = @min(buf.len - i, r.intRangeAtMost(usize, 8, 200));
        const b: u8 = r.int(u8);
        @memset(buf[i..][0..run], b);
        i += run;
    }
}

test "TransformFrameWire is 96 B with the spec's offsets; encode∘decode = id; passthrough declares no frame" {
    try testing.expectEqual(@as(usize, 96), HEADER_LEN);
    const cfg: VolumeTransform = .{ .compression = .lz4, .encryption = .aes256gcm, .block_size = 1 << 20 };
    var blob: [WRAP_V2_LEN]u8 = undefined;
    fill(&blob, 1);
    const h: Header = .{ .flags = FLAG_ENCRYPTED, .compression = .lz4, .encryption = .aes256gcm, .body_len = 1000 + TAG_LEN, .plain_len = 4096, .key_blob = blob, .nonce = .{ 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12 } };
    var img: [HEADER_LEN + 1016]u8 = undefined;
    h.encode(img[0..HEADER_LEN]);
    try testing.expectEqualStrings("SQZB", img[0..4]);
    try testing.expectEqual(FRAME_VERSION, img[4]);
    try testing.expectEqual(FLAG_ENCRYPTED, img[5]);
    try testing.expectEqual(@as(u8, 1), img[6]);
    try testing.expectEqual(@as(u8, 1), img[7]);
    try testing.expectEqual(@as(u32, 1016), std.mem.readInt(u32, img[8..12], .little));
    try testing.expectEqual(@as(u32, 4096), std.mem.readInt(u32, img[12..16], .little));
    try testing.expectEqualSlices(u8, &blob, img[16..79]);
    try testing.expect(std.mem.allEqual(u8, img[91..96], 0));
    const back = try decodeHeader(cfg, &img);
    try testing.expectEqual(h, back);
    try testing.expect((VolumeTransform{ .block_size = 4096 }).isPassthrough());
    try testing.expect(!cfg.isPassthrough());
}

test "decodeHeader refuses by class in order: len, plain_len FIRST (before magic), magic, version, flags/reserved, mode, the flag/mode law; decodeTable refuses every table law by name" {
    const cfg: VolumeTransform = .{ .compression = .lz4, .encryption = .none, .block_size = 65536 };
    const good: Header = .{ .flags = 0, .compression = .lz4, .encryption = .none, .body_len = 100, .plain_len = 4096, .key_blob = @splat(0), .nonce = @splat(0) };
    var img: [HEADER_LEN + 100]u8 = undefined;
    good.encode(img[0..HEADER_LEN]);
    _ = try decodeHeader(cfg, &img);
    try testing.expectError(error.FrameLen, decodeHeader(cfg, img[0 .. HEADER_LEN - 1]));
    // plain_len precedes magic: a garbage magic with an oversized plain_len is a plain_len refusal.
    var bad = img;
    std.mem.writeInt(u32, bad[12..16], 65537, .little);
    bad[0] = 'X';
    try testing.expectError(error.FramePlainLen, decodeHeader(cfg, &bad));
    bad = img;
    bad[0] = 'X';
    try testing.expectError(error.FrameMagic, decodeHeader(cfg, &bad));
    bad = img;
    bad[4] = 1; // the single-frame format is refused, not read
    try testing.expectError(error.FrameVersion, decodeHeader(cfg, &bad));
    bad = img;
    bad[5] = 0x80;
    try testing.expectError(error.FrameFlags, decodeHeader(cfg, &bad));
    bad = img;
    bad[5] = FLAG_STORE_RAW; // an ENTRY flag, never the header's
    try testing.expectError(error.FrameFlags, decodeHeader(cfg, &bad));
    bad = img;
    bad[95] = 1;
    try testing.expectError(error.FrameFlags, decodeHeader(cfg, &bad));
    bad = img;
    bad[6] = 2; // zstd on an lz4 volume
    try testing.expectError(error.FrameMode, decodeHeader(cfg, &bad));
    bad = img;
    bad[7] = 7;
    try testing.expectError(error.FrameMode, decodeHeader(cfg, &bad));
    // ENCRYPTED on a plaintext volume; a nonce byte on a plaintext frame.
    bad = img;
    bad[5] = FLAG_ENCRYPTED;
    try testing.expectError(error.FrameFlags, decodeHeader(cfg, &bad));
    bad = img;
    bad[80] = 1;
    try testing.expectError(error.FrameFlags, decodeHeader(cfg, &bad));
    // The table: built from a real two-frame image (a 128 KiB block,
    // 70,000 B of plaintext), then each law broken in turn.
    const gpa = testing.allocator;
    var env = testEnv();
    defer env.deinit();
    const cfg2: VolumeTransform = .{ .compression = .lz4, .encryption = .none, .block_size = 131072 };
    const xf = try Transform.init(&env, cfg2, null);
    var t = try Codec.init(gpa, &xf);
    defer t.deinit(gpa);
    var plain: [131072]u8 = undefined;
    fillCompressible(&plain, 11);
    const scratch = try gpa.alloc(u8, scratchLen(cfg2.block_size));
    defer gpa.free(scratch);
    const image = try gpa.dupe(u8, try t.processWrite(plain[0..70000], scratch));
    defer gpa.free(image);
    const h = try decodeHeader(cfg2, image);
    const table = try decodeTable(cfg2, h, image);
    try testing.expectEqual(@as(u32, 2), table.count);
    try testing.expectEqual(@as(u32, FRAME_GRAIN), table.get(0).plain_len);
    try testing.expectEqual(@as(u32, 70000 - FRAME_GRAIN), table.get(1).plain_len);
    try testing.expectEqual(@as(u32, @intCast(tableLen(2))), table.get(0).off);
    try testing.expectEqual(table.get(0).off + table.get(0).body_len, table.get(1).off);
    const mut = try gpa.alloc(u8, image.len);
    defer gpa.free(mut);
    // A short page.
    try testing.expectError(error.FrameLen, decodeTable(cfg2, h, image[0 .. HEADER_PAGE_LEN - 1]));
    // A torn entry byte: the hash64 catches it.
    @memcpy(mut, image);
    mut[HEADER_LEN + TABLE_HEAD_LEN + 1] ^= 1;
    try testing.expectError(error.FrameTable, decodeTable(cfg2, h, mut));
    // The count off the plaintext; the grain off the constant; a reserved byte.
    @memcpy(mut, image);
    std.mem.writeInt(u16, mut[HEADER_LEN + 8 ..][0..2], 1, .little);
    try testing.expectError(error.FrameTable, decodeTable(cfg2, h, mut));
    @memcpy(mut, image);
    mut[HEADER_LEN + 10] = FRAME_GRAIN_LOG2 + 1;
    try testing.expectError(error.FrameTable, decodeTable(cfg2, h, mut));
    @memcpy(mut, image);
    mut[HEADER_LEN + 11] = 1;
    try testing.expectError(error.FrameTable, decodeTable(cfg2, h, mut));
    // Re-stamping the hash64 after a law break still refuses: a non-contiguous
    // body, a raw body ≠ its plaintext, a compressed body ≥ its plaintext, a
    // last frame of zero plaintext, the sum off `body_len`.
    const Restamp = struct {
        fn run(buf: []u8, count: u32) void {
            const eb = buf[HEADER_LEN + TABLE_HEAD_LEN ..][0 .. @as(usize, count) * ENTRY_LEN];
            std.mem.writeInt(u64, buf[HEADER_LEN..][0..8], checksum.hash64(eb), .little);
        }
    };
    const e1 = HEADER_LEN + TABLE_HEAD_LEN + ENTRY_LEN;
    @memcpy(mut, image);
    std.mem.writeInt(u32, mut[e1..][0..4], table.get(1).off + 1, .little);
    Restamp.run(mut, 2);
    try testing.expectError(error.FrameTable, decodeTable(cfg2, h, mut));
    @memcpy(mut, image);
    mut[e1 + 12] = FLAG_STORE_RAW;
    Restamp.run(mut, 2);
    try testing.expectError(error.FrameTable, decodeTable(cfg2, h, mut));
    @memcpy(mut, image);
    std.mem.writeInt(u32, mut[e1 + 4 ..][0..4], table.get(1).plain_len, .little);
    Restamp.run(mut, 2);
    try testing.expectError(error.FrameTable, decodeTable(cfg2, h, mut));
    @memcpy(mut, image);
    mut[e1 + 13] = 1;
    Restamp.run(mut, 2);
    try testing.expectError(error.FrameTable, decodeTable(cfg2, h, mut));
    // A compression-less volume's entry without the raw flag.
    const enc_only: VolumeTransform = .{ .compression = .none, .encryption = .aes256gcm, .block_size = 65536 };
    var eh: Header = .{ .flags = FLAG_ENCRYPTED, .compression = .none, .encryption = .aes256gcm, .body_len = 100 + TAG_LEN, .plain_len = 100, .key_blob = @splat(1), .nonce = @splat(2) };
    var page: [HEADER_PAGE_LEN]u8 = @splat(0);
    eh.encode(page[0..HEADER_LEN]);
    const entry: Entry = .{ .off = @intCast(tableLen(1)), .body_len = 100 + TAG_LEN, .plain_len = 100, .flags = 0 };
    entry.encode(page[HEADER_LEN + TABLE_HEAD_LEN ..][0..ENTRY_LEN]);
    std.mem.writeInt(u16, page[HEADER_LEN + 8 ..][0..2], 1, .little);
    page[HEADER_LEN + 10] = FRAME_GRAIN_LOG2;
    Restamp.run(&page, 1);
    try testing.expectError(error.FrameTable, decodeTable(enc_only, eh, &page));
    var raw_entry = entry;
    raw_entry.flags = FLAG_STORE_RAW;
    raw_entry.encode(page[HEADER_LEN + TABLE_HEAD_LEN ..][0..ENTRY_LEN]);
    Restamp.run(&page, 1);
    _ = try decodeTable(enc_only, eh, &page);
    // An encrypted body shorter than a tag.
    raw_entry.body_len = 15;
    raw_entry.encode(page[HEADER_LEN + TABLE_HEAD_LEN ..][0..ENTRY_LEN]);
    Restamp.run(&page, 1);
    try testing.expectError(error.FrameTable, decodeTable(enc_only, eh, &page));
    // The empty payload: zero frames — a window over it is wholly past the
    // plaintext (zeros, no frame); an empty window still refuses.
    const empty = try gpa.dupe(u8, try t.processWrite("", scratch));
    defer gpa.free(empty);
    const eh0 = try decodeHeader(cfg2, empty);
    const et = try decodeTable(cfg2, eh0, empty);
    try testing.expectEqual(@as(u32, 0), et.count);
    try testing.expectEqual(@as(usize, HEADER_PAGE_LEN), empty.len);
    try testing.expect((try frameSpan(eh0, et, 0, 1)).isEmpty());
    try testing.expectError(error.FrameTable, frameSpan(eh0, et, 5, 5));
}

test "liveLen: the plaintext through its last non-zero byte — an all-zero block is 0, the last live byte ends it whichever frame holds it; the trimmed image round-trips and its stored bytes meet storedImageBound (decision 0003 §AK)" {
    const gpa = testing.allocator;
    const block: u32 = 4 * FRAME_GRAIN;
    const plain = try gpa.alloc(u8, block);
    defer gpa.free(plain);
    @memset(plain, 0);
    try testing.expectEqual(@as(usize, 0), liveLen(plain));
    plain[0] = 1;
    try testing.expectEqual(@as(usize, 1), liveLen(plain));
    plain[FRAME_GRAIN - 1] = 3;
    try testing.expectEqual(@as(usize, FRAME_GRAIN), liveLen(plain));
    plain[2 * FRAME_GRAIN + 77] = 9;
    try testing.expectEqual(@as(usize, 2 * FRAME_GRAIN + 78), liveLen(plain));
    try testing.expectEqual(@as(usize, 2 * FRAME_GRAIN + 78), liveLen(plain[0 .. 2 * FRAME_GRAIN + 100]));
    try testing.expectEqual(@as(usize, FRAME_GRAIN), liveLen(plain[0 .. 2 * FRAME_GRAIN + 50]));
    var env = testEnv();
    defer env.deinit();
    var d = deriveVolumeKey("a-32-byte-or-longer-key-material-string!!", &(@as([32]u8, @splat(5))));
    defer d.zeroize();
    const gcm = try CryptoState.init(gpa, testing.io, .aes256gcm, d, null, 0, 1);
    defer gcm.deinit();
    const scratch = try gpa.alloc(u8, @intCast(scratchLen(block)));
    defer gpa.free(scratch);
    const out = try gpa.alloc(u8, block);
    defer gpa.free(out);
    for ([_]VolumeTransform{ .{ .compression = .lz4, .block_size = block }, .{ .encryption = .aes256gcm, .block_size = block } }) |cfg| {
        const xf = try Transform.init(&env, cfg, if (cfg.encryption == .none) null else gcm);
        var t = try Codec.init(gpa, &xf);
        defer t.deinit(gpa);
        const live = liveLen(plain);
        const image = try gpa.dupe(u8, try t.processWrite(plain[0..live], scratch));
        defer gpa.free(image);
        try testing.expect(std.mem.alignForward(usize, image.len, DMA_GRAIN) <= storedImageBound(cfg, live));
        try testing.expect(storedImageBound(cfg, live) < storedImageBound(cfg, block));
        @memset(out, 0xEE);
        const got = try t.processRead(image, out);
        try testing.expectEqualSlices(u8, plain[0..live], got);
    }
}

test "geometry: max_stored_image_len = table + block (+16 per frame encrypted) ≤ block + headroom; chunk = block + 4096 on transformed volumes, block on passthrough; the refusal; capacity reserves two chunks" {
    const block: u32 = 4 << 20;
    const plain: VolumeTransform = .{ .block_size = block };
    const lz: VolumeTransform = .{ .compression = .lz4, .block_size = block };
    const enc: VolumeTransform = .{ .compression = .zstd, .encryption = .chacha20poly1305, .block_size = block };
    try testing.expectEqual(@as(u64, block), maxStoredImageLen(plain));
    try testing.expectEqual(@as(usize, 1136), tableLen(64));
    try testing.expectEqual(@as(usize, 128), tableLen(1));
    try testing.expectEqual(@as(u64, 1136 + block), maxStoredImageLen(lz));
    try testing.expectEqual(@as(u64, 1136 + block + 64 * 16), maxStoredImageLen(enc));
    try testing.expectEqual(@as(u32, 4096), TRANSFORM_BLOCK_HEADROOM);
    try testing.expectEqual(block, chunkSizeFor(plain));
    try testing.expectEqual(@as(u32, 4_198_400), chunkSizeFor(lz));
    try testing.expectEqual(@as(u32, 4_198_400), chunkSizeFor(enc));
    try testing.expectEqual(@as(u32, 64), framesFor(block));
    try testing.expectEqual(@as(u32, 0), framesFor(0));
    try testing.expectEqual(@as(u32, 1), framesFor(1));
    try testing.expectEqual(@as(u32, 1), framesFor(FRAME_GRAIN));
    try testing.expectEqual(@as(u32, 2), framesFor(FRAME_GRAIN + 1));
    // The tie: table + block + frames × 16 ≤ block + 4096 for every admissible block.
    var b: u32 = MIN_BLOCK_SIZE;
    while (b <= MAX_BLOCK_SIZE) : (b *= 2) {
        const c: VolumeTransform = .{ .compression = .lz4, .encryption = .aes256gcm, .block_size = b };
        try testing.expect(maxStoredImageLen(c) <= chunkSizeFor(c));
        try geometryCheck(c, chunkSizeFor(c));
        try testing.expectError(error.ChunkCannotHoldImage, geometryCheck(c, b));
    }
    try geometryCheck(plain, block);
    var buf: [256]u8 = undefined;
    var w: std.Io.Writer = .fixed(&buf);
    try writeGeometryRefusal(&w, lz, block);
    try testing.expect(std.mem.indexOf(u8, w.buffered(), "chunk 4194304 B cannot hold max_stored_image_len(4194304 B block) = 4195440 B") != null);
    // Capacity: chunks 0 and last are reserved.
    try testing.expectEqual(@as(u64, 0), capacityBlocks(2 * 4_198_400, 4_198_400));
    try testing.expectEqual(@as(u64, 1), capacityBlocks(3 * 4_198_400 + 100, 4_198_400));
    try testing.expectEqual(@as(u64, 254), capacityBlocks(1 << 30, 4 << 20));
    try testing.expect(blockSizeValid(4 << 20) and blockSizeValid(4096) and !blockSizeValid(8 << 20) and !blockSizeValid(3 << 20) and !blockSizeValid(2048));
}

test "scratch: write scratch = ceil_4k(table + block + frames × 16) = block + 4 KiB (4,198,400 at 4 MiB; the reference's bound-sized 4,214,784 is never needed); ONE scratch = the write scratch (the C decoder needs no window slack), inside the charter's block + 128 KiB envelope; the pool derivations clamp(cpus/4, 4, 16) and one codec per lane" {
    try testing.expectEqual(@as(u64, 4_198_400), writeScratchLen(4 << 20));
    try testing.expectEqual(@as(u64, 4_198_400), scratchLen(4 << 20));
    try testing.expect(scratchLen(4 << 20) < 4_214_784);
    try testing.expectEqual(@as(u64, 8192), writeScratchLen(4096));
    try testing.expectEqual(@as(u64, 8192), scratchLen(4096));
    var b: u32 = MIN_BLOCK_SIZE;
    while (b <= MAX_BLOCK_SIZE) : (b *= 2) {
        try testing.expectEqual(writeScratchLen(b), scratchLen(b));
        try testing.expect(scratchLen(b) >= @as(u64, b) + tableLen(framesFor(b)) + @as(u64, framesFor(b)) * TAG_LEN);
        try testing.expect(scratchLen(b) <= @as(u64, b) + (128 << 10));
        try testing.expectEqual(@as(u64, 0), writeScratchLen(b) % DMA_GRAIN);
    }
    // The pools: clamp(cpus / 4, 4, 16) scratches; one codec per lane, floor 1.
    try testing.expectEqual(@as(u32, 4), scratchPoolCap(1));
    try testing.expectEqual(@as(u32, 4), scratchPoolCap(9));
    try testing.expectEqual(@as(u32, 8), scratchPoolCap(32));
    try testing.expectEqual(@as(u32, 16), scratchPoolCap(256));
    try testing.expectEqual(@as(usize, 1), codecPoolSize(0));
    try testing.expectEqual(@as(usize, 12), codecPoolSize(12));
}

// LZ4 block vectors produced by the reference `lz4 v1.10.0` CLI (`lz4 -BI
// -B4 --no-frame-crc -c`), the block cut out of the frame (7 B frame
// header, 4 B LE block size, the block, 4 B end mark).
const lz4_vec_abc_plain: [48]u8 = blk: {
    var b: [48]u8 = undefined;
    for (0..16) |i| b[i * 3 ..][0..3].* = "abc".*;
    break :blk b;
};
const lz4_vec_abc_block = [_]u8{ 0x3f, 'a', 'b', 'c', 0x03, 0x00, 0x15, 0x50, 'b', 'c', 'a', 'b', 'c' };
const lz4_vec_fox_plain = "The quick brown fox jumps over the lazy dog. The quick brown fox jumps over the lazy dog. The quick brown fox jumps over the lazy dog.";
const lz4_vec_fox_block = [_]u8{ 0xff, 0x1e } ++ "The quick brown fox jumps over the lazy dog. ".* ++ [_]u8{ 0x2d, 0x00, 0x41, 0x50 } ++ " dog.".*;
const lz4_vec_a_plain: [322]u8 = @splat('a');
const lz4_vec_a_block = [_]u8{ 0x1f, 'a', 0x01, 0x00, 0xff, 0x2a, 0x50, 'a', 'a', 'a', 'a', 'a' };

test "lz4: the reference CLI's block vectors decode to their plaintexts; our compressor's blocks decode too and never exceed the reference bound" {
    var out: [512]u8 = undefined;
    try lz4.decompressBlock(&lz4_vec_abc_block, out[0..lz4_vec_abc_plain.len]);
    try testing.expectEqualStrings(&lz4_vec_abc_plain, out[0..lz4_vec_abc_plain.len]);
    try lz4.decompressBlock(&lz4_vec_fox_block, out[0..lz4_vec_fox_plain.len]);
    try testing.expectEqualStrings(lz4_vec_fox_plain, out[0..lz4_vec_fox_plain.len]);
    try lz4.decompressBlock(&lz4_vec_a_block, out[0..lz4_vec_a_plain.len]);
    try testing.expectEqualSlices(u8, &lz4_vec_a_plain, out[0..lz4_vec_a_plain.len]);
    // A wrong declared length is Corrupt, never a short or long fill.
    try testing.expectError(error.Corrupt, lz4.decompressBlock(&lz4_vec_abc_block, out[0 .. lz4_vec_abc_plain.len - 1]));
    try testing.expectError(error.Corrupt, lz4.decompressBlock(&lz4_vec_abc_block, out[0 .. lz4_vec_abc_plain.len + 1]));
    // Our compressor on the same inputs: valid blocks, shorter than the input.
    for ([_][]const u8{ &lz4_vec_abc_plain, lz4_vec_fox_plain, &lz4_vec_a_plain }) |p| {
        var comp: [512]u8 = undefined;
        const n = try lz4.compressBlock(p, &comp);
        try testing.expect(n < p.len);
        try testing.expect(n <= lz4.compressBound(p.len));
        try lz4.decompressBlock(comp[0..n], out[0..p.len]);
        try testing.expectEqualSlices(u8, p, out[0..p.len]);
    }
    // The empty block: one zero token.
    var tiny: [8]u8 = undefined;
    try testing.expectEqual(@as(usize, 1), try lz4.compressBlock("", &tiny));
    try testing.expectEqual(@as(u8, 0), tiny[0]);
    try lz4.decompressBlock(tiny[0..1], out[0..0]);
}

test "lz4: round-trip property over sizes 0..70000 of compressible and random data; incompressible input refuses a bounded output (the raw escape) and stays under the bound otherwise" {
    const gpa = testing.allocator;
    const src = try gpa.alloc(u8, 70_000);
    defer gpa.free(src);
    const comp = try gpa.alloc(u8, lz4.compressBound(70_000));
    defer gpa.free(comp);
    const back = try gpa.alloc(u8, 70_000);
    defer gpa.free(back);
    var prng = std.Random.DefaultPrng.init(0x1234);
    const r = prng.random();
    for (0..300) |i| {
        const len = if (i < 30) i else r.intRangeAtMost(usize, 0, 70_000);
        if (i % 3 == 0) fill(src[0..len], i) else fillCompressible(src[0..len], i);
        const n = try lz4.compressBlock(src[0..len], comp);
        try testing.expect(n <= lz4.compressBound(len));
        try lz4.decompressBlock(comp[0..n], back[0..len]);
        try testing.expectEqualSlices(u8, src[0..len], back[0..len]);
        if (i % 3 != 0 and len > 64) try testing.expect(n < len);
    }
    // Bounded output: random bytes never fit `len − 1`.
    fill(src[0..4096], 99);
    try testing.expectError(error.OutputTooSmall, lz4.compressBlock(src[0..4096], comp[0..4095]));
    // A long literal run and a long match exercise the 255-continuation
    // arms on both sides.
    @memset(src[0..70_000], 0x5a);
    const n = try lz4.compressBlock(src[0..70_000], comp);
    try testing.expect(n < 300);
    try lz4.decompressBlock(comp[0..n], back[0..70_000]);
    try testing.expectEqualSlices(u8, src[0..70_000], back[0..70_000]);
    fill(src[0..20_000], 7);
    const n2 = try lz4.compressBlock(src[0..20_000], comp);
    try lz4.decompressBlock(comp[0..n2], back[0..20_000]);
    try testing.expectEqualSlices(u8, src[0..20_000], back[0..20_000]);
}

fn lz4Fuzz(_: void, smith: *std.testing.Smith) !void {
    var buf: [2048]u8 = undefined;
    const len = smith.slice(&buf);
    var out: [4096]u8 = undefined;
    // Every destination size; the decoder either fills exactly or refuses.
    for ([_]usize{ 0, 1, 7, 64, 500, 4096 }) |dst_len| {
        lz4.decompressBlock(buf[0..len], out[0..dst_len]) catch {};
    }
}

test "fuzz: the lz4 decompressor is total on arbitrary bytes" {
    try fuzzkit.fuzz({}, lz4Fuzz, .{ .corpus = &.{ fuzzSlice(&lz4_vec_abc_block), fuzzSlice(&lz4_vec_fox_block), fuzzSlice(&lz4_vec_a_block), fuzzSlice(&.{ 0x1f, 'a', 0x00, 0x00 }), fuzzSlice(&.{ 0xff, 0xff, 0xff }) } });
}

test "property: the lz4 decompressor is total under a PRNG, including bit flips of valid blocks" {
    var prng = std.Random.DefaultPrng.init(0xf00d);
    const r = prng.random();
    var buf: [1024]u8 = undefined;
    var out: [2048]u8 = undefined;
    for (0..20_000) |_| {
        const len = r.intRangeAtMost(usize, 0, buf.len);
        r.bytes(buf[0..len]);
        lz4.decompressBlock(buf[0..len], out[0..r.intRangeAtMost(usize, 0, out.len)]) catch {};
    }
    var src: [600]u8 = undefined;
    fillCompressible(&src, 3);
    var comp: [700]u8 = undefined;
    const n = try lz4.compressBlock(&src, &comp);
    for (0..5000) |_| {
        var mut = comp;
        for (0..r.intRangeAtMost(usize, 1, 3)) |_| mut[r.intRangeLessThan(usize, 0, n)] ^= @as(u8, 1) << r.int(u3);
        lz4.decompressBlock(mut[0..n], &src) catch {};
        lz4.decompressBlock(mut[0..r.intRangeAtMost(usize, 0, n)], &src) catch {};
    }
}

test "lz4 throughput scoping row (dev box, test build; not an acceptance number)" {
    const gpa = testing.allocator;
    const len: usize = 4 << 20;
    const src = try gpa.alloc(u8, len);
    defer gpa.free(src);
    const comp = try gpa.alloc(u8, len);
    defer gpa.free(comp);
    const back = try gpa.alloc(u8, len);
    defer gpa.free(back);
    fillCompressible(src, 42);
    const rounds: usize = 3;
    var n: usize = 0;
    const t0 = clock.monotonicNs();
    for (0..rounds) |_| n = try lz4.compressBlock(src, comp);
    const t1 = clock.monotonicNs();
    for (0..rounds) |_| try lz4.decompressBlock(comp[0..n], back);
    const t2 = clock.monotonicNs();
    try testing.expectEqualSlices(u8, src, back);
    const c_mbs = (len * rounds * 1000) / @max(t1 - t0, 1);
    const d_mbs = (len * rounds * 1000) / @max(t2 - t1, 1);
    std.debug.print("\nlz4 scoping: 4 MiB compressible block → {d} B ({d}.{d:0>2}×); compress {d} MB/s, decompress {d} MB/s ({s}, {s})\n", .{ n, len / n, (len * 100 / n) % 100, c_mbs, d_mbs, @tagName(builtin.mode), @tagName(builtin.os.tag) });
}

test "zstd: a static context compresses level-3 single-shot frames the static DCtx reads back exactly; incompressible input refuses a bounded output; the window is the content size (single segment); two contexts decode the same frame independently" {
    const gpa = testing.allocator;
    var z = try zstd.Context.init(gpa);
    defer z.deinit(gpa);
    const src = try gpa.alloc(u8, 1 << 20);
    defer gpa.free(src);
    const comp = try gpa.alloc(u8, 1 << 20);
    defer gpa.free(comp);
    // One byte past the largest plaintext: room for the `len + 1` refusal.
    const out = try gpa.alloc(u8, (1 << 20) + 1);
    defer gpa.free(out);
    // Below a frame's own overhead nothing shrinks: the escape stores raw.
    for ([_]usize{ 1, 10 }) |len| {
        fillCompressible(src[0..len], len);
        try testing.expectError(error.OutputTooSmall, z.c.compress(src[0..len], comp[0 .. len - 1]));
    }
    for ([_]usize{ 4096, 65536, 1 << 20 }) |len| {
        fillCompressible(src[0..len], len);
        const n = try z.c.compress(src[0..len], comp[0 .. len - 1]);
        try testing.expect(n < len);
        // Frame descriptor: Single_Segment_flag (bit 5) set, no checksum (bit 2).
        try testing.expect(comp[4] & 0x20 != 0);
        try testing.expect(comp[4] & 0x04 == 0);
        // The destination is EXACTLY the plaintext: no window slack.
        try z.d.decompress(comp[0..n], out[0..len], @intCast(len));
        try testing.expectEqualSlices(u8, src[0..len], out[0..len]);
        // A wrong plain_len (either direction) and trailing bytes refuse.
        try testing.expectError(error.Corrupt, z.d.decompress(comp[0..n], out, @intCast(len - 1)));
        try testing.expectError(error.Corrupt, z.d.decompress(comp[0..n], out, @intCast(len + 1)));
        comp[n] = 0;
        try testing.expectError(error.Corrupt, z.d.decompress(comp[0 .. n + 1], out, @intCast(len)));
        // A second context reads the frame a first one wrote (per-lane
        // contexts share nothing but the bytes).
        var other = try zstd.Decompressor.init(gpa);
        defer other.deinit(gpa);
        @memset(out[0..len], 0);
        try other.decompress(comp[0..n], out[0..len], @intCast(len));
        try testing.expectEqualSlices(u8, src[0..len], out[0..len]);
    }
    fill(src[0..4096], 1);
    try testing.expectError(error.OutputTooSmall, z.c.compress(src[0..4096], comp[0..4095]));
    // The empty frame round-trips at plain_len 0 (the transform never
    // writes it — the empty payload is stored raw — but the decoder is total).
    const n0 = try z.c.compress(src[0..0], comp);
    try z.d.decompress(comp[0..n0], out, 0);
}

test "property: the zstd decoder is total under a PRNG, including bit flips of valid frames" {
    const gpa = testing.allocator;
    var z = try zstd.Context.init(gpa);
    defer z.deinit(gpa);
    var prng = std.Random.DefaultPrng.init(0x2222);
    const r = prng.random();
    var buf: [2048]u8 = undefined;
    const out = try gpa.alloc(u8, 4096);
    defer gpa.free(out);
    for (0..3000) |_| {
        const len = r.intRangeAtMost(usize, 0, buf.len);
        r.bytes(buf[0..len]);
        z.d.decompress(buf[0..len], out, r.intRangeAtMost(u32, 0, 4096)) catch {};
    }
    var src: [3000]u8 = undefined;
    fillCompressible(&src, 5);
    var comp: [3000]u8 = undefined;
    const n = try z.c.compress(&src, &comp);
    for (0..1500) |_| {
        var mut = comp;
        for (0..r.intRangeAtMost(usize, 1, 3)) |_| mut[r.intRangeLessThan(usize, 0, n)] ^= @as(u8, 1) << r.int(u3);
        z.d.decompress(mut[0..n], out, src.len) catch {};
        z.d.decompress(mut[0..r.intRangeAtMost(usize, 0, n)], out, src.len) catch {};
    }
}

test "AEAD: both primitives seal/open with the header as AAD; in-place open is sound; a flipped AAD byte or tag byte fails" {
    var key: [32]u8 = undefined;
    fill(&key, 1);
    const nonce: [12]u8 = .{ 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12 };
    var ad: [96]u8 = undefined;
    fill(&ad, 2);
    var m: [777]u8 = undefined;
    fill(&m, 3);
    for ([_]Encryption{ .aes256gcm, .chacha20poly1305 }) |enc| {
        var c: [777]u8 = undefined;
        var tag: [16]u8 = undefined;
        seal(enc, &c, &tag, &m, &ad, nonce, key);
        try testing.expect(!std.mem.eql(u8, &c, &m));
        // In place: open c → c.
        var buf = c;
        try open(enc, &buf, &buf, tag, &ad, nonce, key);
        try testing.expectEqualSlices(u8, &m, &buf);
        // In place seal too.
        var buf2 = m;
        var tag2: [16]u8 = undefined;
        seal(enc, &buf2, &tag2, &buf2, &ad, nonce, key);
        try testing.expectEqualSlices(u8, &c, &buf2);
        var bad_ad = ad;
        bad_ad[5] ^= 1;
        var out: [777]u8 = undefined;
        try testing.expectError(error.AuthenticationFailed, open(enc, &out, &c, tag, &bad_ad, nonce, key));
        var bad_tag = tag;
        bad_tag[0] ^= 1;
        try testing.expectError(error.AuthenticationFailed, open(enc, &out, &c, bad_tag, &ad, nonce, key));
    }
}

test "key wrap v2: derivation is deterministic and salt-bound; the blob is 'SK' ‖ 0x02 ‖ nonce ‖ ct ‖ tag = 63 B; round-trips under both algorithms; a foreign key, a tampered blob, a bad version and a truncation refuse by name without panicking" {
    const material = "0123456789abcdef0123456789abcdef-the-operator-key-material";
    var salt_a: [32]u8 = undefined;
    fill(&salt_a, 10);
    var salt_b: [32]u8 = undefined;
    fill(&salt_b, 11);
    var ka = deriveVolumeKey(material, &salt_a);
    defer ka.zeroize();
    const ka2 = deriveVolumeKey(material, &salt_a);
    try testing.expectEqual(ka, ka2);
    var kb = deriveVolumeKey(material, &salt_b);
    defer kb.zeroize();
    try testing.expect(!std.mem.eql(u8, &ka.kek, &kb.kek));
    try testing.expect(!std.mem.eql(u8, &ka.key_id, &kb.key_id));
    // Pinned against RFC 5869 HKDF-SHA256 by construction: the labels.
    try testing.expectEqualStrings("squeezefs:key-wrap:v2:kek", KEK_INFO);
    try testing.expectEqualStrings("squeezefs:key-wrap:v2:key-id", KEY_ID_INFO);
    try testing.expectEqualStrings("hkdf-sha256/aead-kw/v2", KEY_WRAP_SCHEME_NAME);
    var data_key: [32]u8 = undefined;
    fill(&data_key, 12);
    const wrap_nonce: [12]u8 = @splat(7);
    for ([_]Encryption{ .aes256gcm, .chacha20poly1305 }) |enc| {
        const blob = wrapDataKey(enc, ka.kek, ka.key_id, wrap_nonce, data_key);
        try testing.expectEqual(@as(usize, 63), blob.len);
        try testing.expectEqualStrings("SK", blob[0..2]);
        try testing.expectEqual(@as(u8, 2), blob[2]);
        try testing.expectEqualSlices(u8, &wrap_nonce, blob[3..15]);
        try testing.expectEqual(data_key, try unwrapDataKey(enc, ka.kek, ka.key_id, &blob));
        try testing.expectError(error.ForeignKey, unwrapDataKey(enc, kb.kek, ka.key_id, &blob));
        try testing.expectError(error.ForeignKey, unwrapDataKey(enc, ka.kek, kb.key_id, &blob));
        var tampered = blob;
        tampered[20] ^= 1;
        try testing.expectError(error.ForeignKey, unwrapDataKey(enc, ka.kek, ka.key_id, &tampered));
        var badver = blob;
        badver[2] = 1;
        try testing.expectError(error.UnsupportedKeyWrapVersion, unwrapDataKey(enc, ka.kek, ka.key_id, &badver));
        try testing.expectError(error.MalformedKeyBlob, unwrapDataKey(enc, ka.kek, ka.key_id, blob[0..14]));
        try testing.expectError(error.MalformedKeyBlob, unwrapDataKey(enc, ka.kek, ka.key_id, blob[0..62]));
        var badmagic = blob;
        badmagic[0] = 'X';
        try testing.expectError(error.MalformedKeyBlob, unwrapDataKey(enc, ka.kek, ka.key_id, &badmagic));
        try testing.expectError(error.MalformedKeyBlob, unwrapDataKey(enc, ka.kek, ka.key_id, ""));
        var buf: [256]u8 = undefined;
        var w: std.Io.Writer = .fixed(&buf);
        try writeUnwrapRefusal(&w, error.UnsupportedKeyWrapVersion, &badver);
        try testing.expect(std.mem.indexOf(u8, w.buffered(), "version 1") != null);
    }
    var pb: [128]u8 = undefined;
    const p = defaultKeyPath(&pb, .{ 0xde, 0xad, 0xbe, 0xef, 0x00, 0x11, 0x22, 0x33 });
    try testing.expectEqualStrings("/etc/squeezefs/keys/deadbeef00112233.key", p);
    try testing.expectEqualStrings("abc", trimMaterial(" \t\nabc\r\n"));
}

test "CryptoState: a fresh session key per init; own blob is the fast path; a foreign blob unwraps once and hits the cache after; nonces are salt ‖ BE counter from the floor; the cache capacity derivation" {
    const gpa = testing.allocator;
    var d = deriveVolumeKey("material-material-material-material-material", &(@as([32]u8, @splat(9))));
    defer d.zeroize();
    const a = try CryptoState.init(gpa, testing.io, .aes256gcm, d, null, 1000, 1);
    defer a.deinit();
    const b = try CryptoState.init(gpa, testing.io, .aes256gcm, d, null, 0, 1);
    defer b.deinit();
    try testing.expect(!std.mem.eql(u8, &a.data_key, &b.data_key));
    try testing.expect(!std.mem.eql(u8, &a.keyBlob(), &b.keyBlob()));
    try testing.expectEqual(a.data_key, try a.keyFor(&a.keyBlob()));
    const unwraps0 = stats.crypto_key_unwraps.load();
    const hits0 = stats.crypto_key_unwrap_hits.load();
    try testing.expectEqual(b.data_key, try a.keyFor(&b.keyBlob()));
    try testing.expectEqual(b.data_key, try a.keyFor(&b.keyBlob()));
    try testing.expectEqual(unwraps0 + 2, stats.crypto_key_unwraps.load());
    try testing.expectEqual(hits0 + 1, stats.crypto_key_unwrap_hits.load());
    var junk: [63]u8 = b.keyBlob();
    junk[30] ^= 1;
    try testing.expectError(error.ForeignKey, a.keyFor(&junk));
    const n1 = a.nextNonce();
    const n2 = a.nextNonce();
    try testing.expectEqualSlices(u8, n1[0..4], n2[0..4]);
    try testing.expectEqual(@as(u64, 1000), std.mem.readInt(u64, n1[4..12], .big));
    try testing.expectEqual(@as(u64, 1001), std.mem.readInt(u64, n2[4..12], .big));
    try testing.expectEqual(@as(u64, 1002), a.nonceCounter());
    try testing.expectEqual(@as(u32, 16), unwrapCacheCapacity(1));
    try testing.expectEqual(@as(u32, 16), unwrapCacheCapacity(8));
    try testing.expectEqual(@as(u32, 34), unwrapCacheCapacity(17));
    try testing.expectEqual(@as(usize, 16), a.cache.len);
}

test "the rotation arm: beginRotation re-wraps the SAME session key under the next KEK (a new blob; headers written after carry it), the old blob still unwraps as `previous` through the retiring arm and is counted; a state opened WITH a previous key reads both; a second begin refuses while open; endRotation forgets the previous key (its blob is foreign after); a block sealed before the swap decodes after it" {
    const gpa = testing.allocator;
    var ka = deriveVolumeKey("material-A-material-A-material-A-material-A", &(@as([32]u8, @splat(1))));
    defer ka.zeroize();
    var kb = deriveVolumeKey("material-B-material-B-material-B-material-B", &(@as([32]u8, @splat(2))));
    defer kb.zeroize();
    const c = try CryptoState.init(gpa, testing.io, .aes256gcm, ka, null, 0, 1);
    defer c.deinit();
    const blob_a = c.keyBlob();
    const session = c.data_key;
    try testing.expect(!c.rotating());
    // A block sealed under A before the swap.
    var env = testEnv();
    defer env.deinit();
    const cfg: VolumeTransform = .{ .compression = .lz4, .encryption = .aes256gcm, .block_size = 64 << 10 };
    const xf = try Transform.init(&env, cfg, c);
    var codec = try Codec.init(gpa, &xf);
    defer codec.deinit(gpa);
    const scratch = try gpa.alloc(u8, scratchLen(cfg.block_size));
    defer gpa.free(scratch);
    const plain = try gpa.alloc(u8, 10_000);
    defer gpa.free(plain);
    for (plain, 0..) |*b, i| b.* = @intCast((i * 7) & 0xff);
    const image_a = try gpa.dupe(u8, try codec.processWrite(plain, scratch));
    defer gpa.free(image_a);
    try testing.expectEqualSlices(u8, &blob_a, image_a[16..79]);
    // The swap.
    try c.beginRotation(testing.io, kb);
    try testing.expect(c.rotating());
    try testing.expectEqualSlices(u8, &session, &c.data_key);
    const blob_b = c.keyBlob();
    try testing.expect(!std.mem.eql(u8, &blob_a, &blob_b));
    try testing.expectEqualSlices(u8, &kb.key_id, &c.keyId());
    try testing.expectError(error.RotationOpen, c.beginRotation(testing.io, ka));
    try testing.expectEqual(CryptoState.BlobClass.current, c.classify(&blob_b));
    try testing.expectEqual(CryptoState.BlobClass.previous, c.classify(&blob_a));
    var junk = blob_a;
    junk[40] ^= 1;
    try testing.expectEqual(CryptoState.BlobClass.foreign, c.classify(&junk));
    // The old block decodes after the swap (the previous arm, counted); a
    // new one carries the new blob.
    const prev0 = stats.crypto_key_unwraps_previous.load();
    const out = try gpa.alloc(u8, scratchLen(cfg.block_size));
    defer gpa.free(out);
    const back = try codec.processRead(image_a, out);
    try testing.expectEqualSlices(u8, plain, back);
    try testing.expectEqual(prev0 + 1, stats.crypto_key_unwraps_previous.load());
    const image_b = try codec.processWrite(plain, scratch);
    try testing.expectEqualSlices(u8, &blob_b, image_b[16..79]);
    try testing.expectEqual(image_a.len, image_b.len);
    // A state opened with a previous key (the remount inside a window).
    const resumed = try CryptoState.init(gpa, testing.io, .aes256gcm, kb, ka, 0, 1);
    defer resumed.deinit();
    try testing.expect(resumed.rotating());
    try testing.expectEqual(CryptoState.BlobClass.previous, resumed.classify(&blob_a));
    try testing.expectEqual(session, try resumed.keyFor(&blob_a));
    try testing.expectEqual(session, try resumed.keyFor(&blob_b));
    // The close: the previous key is forgotten — its blob is foreign now.
    c.retireWrap();
    c.endRotation();
    c.retireWrap();
    try testing.expect(!c.rotating());
    try testing.expectEqual(CryptoState.BlobClass.foreign, c.classify(&blob_a));
    try testing.expectEqual(CryptoState.BlobClass.current, c.classify(&blob_b));
    // The blob was cached while the arm was live: the cache answers it
    // still (the key, not the wrap, is what a reader needs); a fresh state
    // without the arm refuses it.
    const fresh = try CryptoState.init(gpa, testing.io, .aes256gcm, kb, null, 0, 1);
    defer fresh.deinit();
    try testing.expectError(error.ForeignKey, fresh.keyFor(&blob_a));
    c.endRotation();
}

fn roundTrip(gpa: Allocator, cfg: VolumeTransform, crypto: ?*CryptoState, plain: []const u8, expect_raw: bool) !void {
    var env = testEnv();
    defer env.deinit();
    const xf = try Transform.init(&env, cfg, crypto);
    var t = try Codec.init(gpa, &xf);
    defer t.deinit(gpa);
    const scratch = try gpa.alloc(u8, scratchLen(cfg.block_size));
    defer gpa.free(scratch);
    const out = try gpa.alloc(u8, scratchLen(cfg.block_size));
    defer gpa.free(out);
    const raw0 = stats.compress_stored_raw.load();
    const copy = try gpa.dupe(u8, plain);
    defer gpa.free(copy);
    const image = try t.processWrite(copy, scratch);
    try testing.expectEqualSlices(u8, plain, copy); // never mutated
    try testing.expectEqual(@as(usize, 0), image.len % DMA_GRAIN);
    try testing.expect(image.len <= chunkSizeFor(cfg));
    try testing.expect(image.len <= maxStoredImageLen(cfg) + DMA_GRAIN - 1);
    const h = try decodeHeader(cfg, image);
    try testing.expectEqual(@as(u32, @intCast(plain.len)), h.plain_len);
    try testing.expectEqual(cfg.encryption != .none, h.encrypted());
    const table = try decodeTable(cfg, h, image);
    try testing.expectEqual(framesFor(@intCast(plain.len)), table.count);
    // `expect_raw` speaks for every frame (the fixtures are uniform).
    var i: u32 = 0;
    while (i < table.count) : (i += 1) try testing.expectEqual(expect_raw, table.get(i).storeRaw());
    try testing.expectEqual(raw0 + @as(u64, if (expect_raw and cfg.compression != .none) table.count else 0), stats.compress_stored_raw.load());
    // The pad is zeros.
    try testing.expect(std.mem.allEqual(u8, image[tableLen(table.count) + h.body_len ..], 0));
    // Read from a copy (the image buffer is the device window and may be mutated).
    const img = try gpa.dupe(u8, image);
    defer gpa.free(img);
    const back = try t.processRead(img, out);
    try testing.expectEqualSlices(u8, plain, back);
    // A device window longer than the image (trailing bytes) still decodes.
    const wide = try gpa.alloc(u8, image.len + 4096);
    defer gpa.free(wide);
    @memcpy(wide[0..image.len], image);
    fill(wide[image.len..], 77);
    try testing.expectEqualSlices(u8, plain, try t.processRead(wide, out));
    // The SUB-BLOCK read: every window of every frame pair decodes to the
    // same bytes through the header page + the span's DMA window alone.
    if (table.count > 0) {
        var prng = std.Random.DefaultPrng.init(plain.len);
        const r = prng.random();
        for (0..24) |_| {
            const rel_s = r.intRangeLessThan(u64, 0, plain.len);
            const rel_e = r.intRangeAtMost(u64, rel_s + 1, plain.len);
            const span = try frameSpan(h, table, rel_s, rel_e);
            try testing.expectEqual(@as(u64, 0), span.lo % DMA_GRAIN);
            try testing.expect(span.hi <= image.len and span.hi % DMA_GRAIN == 0 and span.lo < span.hi);
            try testing.expectEqual(rel_s / FRAME_GRAIN, span.first);
            try testing.expectEqual((rel_e - 1) / FRAME_GRAIN, span.last);
            const window = try gpa.dupe(u8, image[@intCast(span.lo)..@intCast(span.hi)]);
            defer gpa.free(window);
            const view = try t.decodeFrames(image[0..HEADER_PAGE_LEN], window, span.lo, span, out);
            try testing.expectEqualSlices(u8, plain[@intCast(rel_s)..@intCast(rel_e)], view[@intCast(rel_s - span.plain_base)..@intCast(rel_e - span.plain_base)]);
        }
    }
}

test "processWrite/processRead: passthrough is the identity (same pointer, no frame); lz4 and zstd shrink compressible blocks and store incompressible ones raw; the empty payload is raw; both AEADs over compressed and raw bodies; encrypt-only is raw+sealed" {
    const gpa = testing.allocator;
    var env = testEnv();
    defer env.deinit();
    const block: u32 = 65536;
    var plain: [65536]u8 = undefined;
    fillCompressible(&plain, 1);
    var noise: [65536]u8 = undefined;
    fill(&noise, 2);
    {
        const xf = try Transform.init(&env, .{ .block_size = block }, null);
        var t = try Codec.init(gpa, &xf);
        defer t.deinit(gpa);
        var scratch: [8]u8 = undefined;
        const img = try t.processWrite(&plain, &scratch);
        try testing.expectEqual(@intFromPtr(&plain), @intFromPtr(img.ptr));
        try testing.expectEqual(plain.len, img.len);
        var out: [8]u8 = undefined;
        try testing.expectEqual(@intFromPtr(&plain), @intFromPtr((try t.processRead(&plain, &out)).ptr));
        try testing.expectError(error.PlainTooLarge, t.processWrite(&(plain ++ [_]u8{0}), &scratch));
    }
    var d = deriveVolumeKey("a-32-byte-or-longer-key-material-string!!", &(@as([32]u8, @splat(3))));
    defer d.zeroize();
    const gcm = try CryptoState.init(gpa, testing.io, .aes256gcm, d, null, 0, 1);
    defer gcm.deinit();
    const cha = try CryptoState.init(gpa, testing.io, .chacha20poly1305, d, null, 0, 1);
    defer cha.deinit();
    for ([_]Compression{ .lz4, .zstd }) |comp| {
        try roundTrip(gpa, .{ .compression = comp, .block_size = block }, null, &plain, false);
        try roundTrip(gpa, .{ .compression = comp, .block_size = block }, null, &noise, true);
        try roundTrip(gpa, .{ .compression = comp, .block_size = block }, null, "", true);
        try roundTrip(gpa, .{ .compression = comp, .block_size = block }, null, plain[0..5000], false);
        try roundTrip(gpa, .{ .compression = comp, .encryption = .aes256gcm, .block_size = block }, gcm, &plain, false);
        try roundTrip(gpa, .{ .compression = comp, .encryption = .aes256gcm, .block_size = block }, gcm, &noise, true);
        try roundTrip(gpa, .{ .compression = comp, .encryption = .chacha20poly1305, .block_size = block }, cha, &plain, false);
        try roundTrip(gpa, .{ .compression = comp, .encryption = .chacha20poly1305, .block_size = block }, cha, noise[0..4095], true);
    }
    try roundTrip(gpa, .{ .encryption = .aes256gcm, .block_size = block }, gcm, &plain, true);
    try roundTrip(gpa, .{ .encryption = .chacha20poly1305, .block_size = block }, cha, &noise, true);
    try roundTrip(gpa, .{ .encryption = .chacha20poly1305, .block_size = block }, cha, "", true);
    // The key posture must match the config.
    try testing.expectError(error.KeyPostureMismatch, Transform.init(&env, .{ .encryption = .aes256gcm, .block_size = block }, null));
    try testing.expectError(error.KeyPostureMismatch, Transform.init(&env, .{ .encryption = .aes256gcm, .block_size = block }, cha));
    try testing.expectError(error.KeyPostureMismatch, Transform.init(&env, .{ .block_size = block }, gcm));
}

test "processRead decodes into a destination exactly the image's plaintext long — the write side's seed decodes a block's image into a block-sized buffer (decision 0003 §AK: every write into a mapped block of a transformed set without a cache path answered EIO); a destination one byte short refuses ScratchTooSmall" {
    const gpa = testing.allocator;
    var env = testEnv();
    defer env.deinit();
    const block: u32 = 4 * FRAME_GRAIN;
    const plain = try gpa.alloc(u8, block);
    defer gpa.free(plain);
    fillCompressible(plain, 7);
    const scratch = try gpa.alloc(u8, @intCast(scratchLen(block)));
    defer gpa.free(scratch);
    const out = try gpa.alloc(u8, block);
    defer gpa.free(out);
    for ([_]Compression{ .lz4, .zstd }) |comp| {
        const xf = try Transform.init(&env, .{ .compression = comp, .block_size = block }, null);
        var t = try Codec.init(gpa, &xf);
        defer t.deinit(gpa);
        const image = try gpa.dupe(u8, try t.processWrite(plain, scratch));
        defer gpa.free(image);
        try testing.expectEqualSlices(u8, plain, try t.processRead(image, out));
        try testing.expectError(error.ScratchTooSmall, t.processRead(image, out[0 .. block - 1]));
    }
}

test "the force-raw seam stores a compressible block raw (the raw bit, the headroom and the window widening exercised with compressible data)" {
    const gpa = testing.allocator;
    var env = testEnv();
    defer env.deinit();
    try env.put("SQUEEZEFS_TEST_TRANSFORM_FORCE_RAW", "1");
    const cfg: VolumeTransform = .{ .compression = .lz4, .block_size = 65536 };
    const xf = try Transform.init(&env, cfg, null);
    var t = try Codec.init(gpa, &xf);
    defer t.deinit(gpa);
    try testing.expect(xf.force_raw);
    var plain: [65536]u8 = undefined;
    fillCompressible(&plain, 4);
    const scratch = try gpa.alloc(u8, scratchLen(cfg.block_size));
    defer gpa.free(scratch);
    const image = try t.processWrite(&plain, scratch);
    const h = try decodeHeader(cfg, image);
    const table = try decodeTable(cfg, h, image);
    try testing.expect(table.get(0).storeRaw());
    try testing.expectEqual(@as(u32, 65536), h.body_len);
    // The widest plaintext image: the header page + the block = block + 4 KiB
    // (the headroom's second page is the encrypted tags').
    try testing.expectEqual(@as(usize, 65536 + 4096), image.len);
    try testing.expect(image.len <= chunkSizeFor(cfg));
}

test "AAD covers the header: any flipped header byte of an encrypted image fails the open (or a prior field check); a flipped body byte fails the open; a foreign-key blob refuses by name; the tripwires count each class" {
    const gpa = testing.allocator;
    var env = testEnv();
    defer env.deinit();
    var d = deriveVolumeKey("another-32-byte-or-longer-key-material!!", &(@as([32]u8, @splat(5))));
    defer d.zeroize();
    const c = try CryptoState.init(gpa, testing.io, .aes256gcm, d, null, 0, 1);
    defer c.deinit();
    const cfg: VolumeTransform = .{ .compression = .lz4, .encryption = .aes256gcm, .block_size = 65536 };
    const xf = try Transform.init(&env, cfg, c);
    var t = try Codec.init(gpa, &xf);
    defer t.deinit(gpa);
    var plain: [20000]u8 = undefined;
    fillCompressible(&plain, 6);
    const scratch = try gpa.alloc(u8, scratchLen(cfg.block_size));
    defer gpa.free(scratch);
    const out = try gpa.alloc(u8, scratchLen(cfg.block_size));
    defer gpa.free(out);
    const image = try gpa.dupe(u8, try t.processWrite(&plain, scratch));
    defer gpa.free(image);
    const h = try decodeHeader(cfg, image);
    const mut = try gpa.alloc(u8, image.len);
    defer gpa.free(mut);
    // Every header byte: refused. A nonce byte reaches the AEAD (the header
    // is the AAD's first part); a blob byte fails the unwrap first
    // (FrameKey); the rest fail a field law or the AEAD. Every TABLE byte:
    // the hash64 (FrameTable) or — re-stamped — the entry's AAD binding.
    var auth_fail: usize = 0;
    var key_fail: usize = 0;
    for (0..HEADER_LEN) |i| {
        @memcpy(mut, image);
        mut[i] ^= 0x01;
        if (t.readInner(mut, out)) |_| {
            return error.TestUnexpectedResult;
        } else |e| {
            if (e == error.FrameAuth) {
                auth_fail += 1;
                try testing.expect(i < 16 or i >= 79);
            }
            if (e == error.FrameKey) {
                key_fail += 1;
                try testing.expect(i >= 16 and i < 79);
            }
        }
    }
    try testing.expectEqual(@as(usize, WRAP_V2_LEN), key_fail);
    try testing.expect(auth_fail >= NONCE_LEN);
    const table = try decodeTable(cfg, h, image);
    for (0..TABLE_HEAD_LEN + @as(usize, table.count) * ENTRY_LEN) |i| {
        @memcpy(mut, image);
        mut[HEADER_LEN + i] ^= 0x01;
        try testing.expectError(error.FrameTable, t.readInner(mut, out));
    }
    // A body byte of the first frame.
    @memcpy(mut, image);
    mut[tableLen(table.count) + 5] ^= 0x01;
    try testing.expectError(error.FrameAuth, t.readInner(mut, out));
    // The last frame's tag byte.
    @memcpy(mut, image);
    mut[tableLen(table.count) + h.body_len - 1] ^= 0x01;
    try testing.expectError(error.FrameAuth, t.readInner(mut, out));
    // A frame's body re-sealed where another frame sits (an index swap):
    // the AAD's index refuses it — the sub-block read of frame 0 with
    // frame 0's window is fine, with frame 1's bytes under frame 0's entry
    // is not.
    try testing.expect(table.count >= 1);
    // A blob that is well-formed but wrapped under another KEK.
    var d2 = deriveVolumeKey("a-third-32-byte-or-longer-key-material!!!", &(@as([32]u8, @splat(6))));
    defer d2.zeroize();
    const other = try CryptoState.init(gpa, testing.io, .aes256gcm, d2, null, 0, 1);
    defer other.deinit();
    @memcpy(mut, image);
    mut[16..79].* = other.keyBlob();
    try testing.expectError(error.FrameKey, t.readInner(mut, out));
    // The counting path: each class fires its tripwire (one log per class;
    // the rate limit is per site).
    const classes = [_]FrameRefusal{ .aad, .key, .plain_len, .magic, .table };
    for (classes) |cls| stats.refusalTripwire(cls).resetLogBudgetForTest();
    log.testing.expectErrorLogs(5);
    @memcpy(mut, image);
    mut[HEADER_LEN + TABLE_HEAD_LEN + 2] ^= 0x01;
    const tab0 = stats.transform_frame_refusals_table.load();
    try testing.expectError(error.FrameTable, t.processRead(mut, out));
    try testing.expectEqual(tab0 + 1, stats.transform_frame_refusals_table.load());
    @memcpy(mut, image);
    mut[tableLen(framesFor(@intCast(plain.len))) + 5] ^= 0x01;
    const aad0 = stats.transform_frame_refusals_aad.load();
    try testing.expectError(error.FrameAuth, t.processRead(mut, out));
    try testing.expectEqual(aad0 + 1, stats.transform_frame_refusals_aad.load());
    @memcpy(mut, image);
    mut[16..79].* = other.keyBlob();
    const key0 = stats.transform_frame_refusals_key.load();
    try testing.expectError(error.FrameKey, t.processRead(mut, out));
    try testing.expectEqual(key0 + 1, stats.transform_frame_refusals_key.load());
    @memcpy(mut, image);
    std.mem.writeInt(u32, mut[12..16], 65537, .little);
    const pl0 = stats.transform_plain_len_refusals.load();
    try testing.expectError(error.FramePlainLen, t.processRead(mut, out));
    try testing.expectEqual(pl0 + 1, stats.transform_plain_len_refusals.load());
    @memcpy(mut, image);
    mut[0] = 'x';
    try testing.expectError(error.FrameMagic, t.processRead(mut, out));
    try log.testing.expectAllFired();
    // A caller-shape error does not count as a refusal.
    try testing.expectError(error.ScratchTooSmall, t.processRead(image, out[0..10]));
}

test "plain_len above the block is refused before any work; the read path takes no allocator at all (the type pins zero allocations per op)" {
    const gpa = testing.allocator;
    var env = testEnv();
    defer env.deinit();
    const cfg: VolumeTransform = .{ .compression = .zstd, .block_size = 65536 };
    const xf = try Transform.init(&env, cfg, null);
    var t = try Codec.init(gpa, &xf);
    defer t.deinit(gpa);
    var plain: [30000]u8 = undefined;
    fillCompressible(&plain, 8);
    const scratch = try gpa.alloc(u8, scratchLen(cfg.block_size));
    defer gpa.free(scratch);
    const out = try gpa.alloc(u8, scratchLen(cfg.block_size));
    defer gpa.free(out);
    const image = try gpa.dupe(u8, try t.processWrite(&plain, scratch));
    defer gpa.free(image);
    std.mem.writeInt(u32, image[12..16], 65537, .little);
    try testing.expectError(error.FramePlainLen, t.readInner(image, out));
    std.mem.writeInt(u32, image[12..16], 30000, .little);
    try testing.expectEqualSlices(u8, &plain, try t.readInner(image, out));
}

/// One lane of the pool test: `rounds` sub-block decodes of `image`'s
/// frame span through a codec acquired per op, each checked byte-exact.
const PoolLane = struct {
    pool: *CodecPool,
    image: []const u8,
    plain: []const u8,
    span: FrameSpan,
    rounds: usize,
    gate: *std.atomic.Value(u32),
    failed: *std.atomic.Value(u32),
    peak_inside: *std.atomic.Value(u32),
    inside: *std.atomic.Value(u32),

    fn run(l: PoolLane) void {
        const gpa = std.heap.page_allocator;
        const window = gpa.alloc(u8, @intCast(l.span.hi - l.span.lo)) catch {
            _ = l.failed.fetchAdd(1, .monotonic);
            return;
        };
        defer gpa.free(window);
        const out = gpa.alloc(u8, @as(usize, l.span.last - l.span.first + 1) * FRAME_GRAIN) catch {
            _ = l.failed.fetchAdd(1, .monotonic);
            return;
        };
        defer gpa.free(out);
        // Every lane starts together: the pool must serve them all at once.
        while (l.gate.load(.acquire) == 0) std.atomic.spinLoopHint();
        for (0..l.rounds) |_| {
            const c = l.pool.acquire(testing.io);
            const now = l.inside.fetchAdd(1, .acq_rel) + 1;
            _ = l.peak_inside.fetchMax(now, .acq_rel);
            @memcpy(window, l.image[@intCast(l.span.lo)..@intCast(l.span.hi)]);
            const view = c.decodeFrames(l.image[0..HEADER_PAGE_LEN], window, l.span.lo, l.span, out) catch {
                _ = l.failed.fetchAdd(1, .monotonic);
                _ = l.inside.fetchSub(1, .acq_rel);
                l.pool.release(testing.io, c);
                return;
            };
            if (!std.mem.eql(u8, view, l.plain[@intCast(l.span.plain_base)..][0..view.len])) _ = l.failed.fetchAdd(1, .monotonic);
            _ = l.inside.fetchSub(1, .acq_rel);
            l.pool.release(testing.io, c);
        }
    }
};

test "the codec pool (decision 0003 §K10): N lanes decode the same zstd frames at once through N codecs — every decode byte-exact, the lanes genuinely overlap, and a pool sized to the lanes parks nobody (transform_codec_pool_waits stays 0); a pool one short of the lanes parks and still decodes right; a stateless (lz4) volume's pool is one shared codec with no atomics" {
    const gpa = testing.allocator;
    var env = testEnv();
    defer env.deinit();
    const lanes: usize = 8;
    const cfg: VolumeTransform = .{ .compression = .zstd, .block_size = 4 * FRAME_GRAIN };
    const xf = try Transform.init(&env, cfg, null);
    var pool = try CodecPool.init(gpa, &xf, codecPoolSize(lanes));
    defer pool.deinit(gpa);
    try testing.expectEqual(lanes, pool.capacity());
    try testing.expect(!pool.shared);
    const plain = try gpa.alloc(u8, cfg.block_size);
    defer gpa.free(plain);
    fillCompressible(plain, 21);
    const scratch = try gpa.alloc(u8, scratchLen(cfg.block_size));
    defer gpa.free(scratch);
    const image = blk: {
        const c = pool.acquire(testing.io);
        defer pool.release(testing.io, c);
        break :blk try gpa.dupe(u8, try c.processWrite(plain, scratch));
    };
    defer gpa.free(image);
    const h = try decodeHeader(cfg, image);
    const tb = try decodeTable(cfg, h, image);
    try testing.expect(!tb.get(1).storeRaw()); // a compressed frame: the decoder has work to do
    const span = try frameSpan(h, tb, FRAME_GRAIN + 100, 2 * FRAME_GRAIN + 100);
    var gate: std.atomic.Value(u32) = .init(0);
    var failed: std.atomic.Value(u32) = .init(0);
    var peak: std.atomic.Value(u32) = .init(0);
    var inside: std.atomic.Value(u32) = .init(0);
    const waits0 = stats.transform_codec_pool_waits.load();
    var threads: [lanes]std.Thread = undefined;
    for (&threads) |*th| th.* = try std.Thread.spawn(.{}, PoolLane.run, .{PoolLane{ .pool = &pool, .image = image, .plain = plain, .span = span, .rounds = 400, .gate = &gate, .failed = &failed, .peak_inside = &peak, .inside = &inside }});
    gate.store(1, .release);
    for (threads) |th| th.join();
    try testing.expectEqual(@as(u32, 0), failed.load(.acquire));
    // The lanes overlapped inside the decode (not serialized behind one
    // context), and nobody parked: the pool is the lane count.
    try testing.expect(peak.load(.acquire) >= 2);
    try testing.expectEqual(waits0, stats.transform_codec_pool_waits.load());
    try testing.expectEqual(lanes, pool.permits.available(testing.io));
    // One codec short of the lanes: the overflow PARKS (counted) and every
    // decode is still right — the pool never over-admits.
    var short = try CodecPool.init(gpa, &xf, lanes - 1);
    defer short.deinit(gpa);
    var gate2: std.atomic.Value(u32) = .init(0);
    var failed2: std.atomic.Value(u32) = .init(0);
    var peak2: std.atomic.Value(u32) = .init(0);
    var inside2: std.atomic.Value(u32) = .init(0);
    for (&threads) |*th| th.* = try std.Thread.spawn(.{}, PoolLane.run, .{PoolLane{ .pool = &short, .image = image, .plain = plain, .span = span, .rounds = 400, .gate = &gate2, .failed = &failed2, .peak_inside = &peak2, .inside = &inside2 }});
    gate2.store(1, .release);
    for (threads) |th| th.join();
    try testing.expectEqual(@as(u32, 0), failed2.load(.acquire));
    try testing.expect(peak2.load(.acquire) <= lanes - 1);
    try testing.expect(stats.transform_codec_pool_waits.load() > waits0);
    try testing.expectEqual(lanes - 1, short.permits.available(testing.io));
    // A stateless volume: one codec, handed out by pointer, no permits touched.
    const lz_xf = try Transform.init(&env, .{ .compression = .lz4, .block_size = 4 * FRAME_GRAIN }, null);
    var lz = try CodecPool.init(gpa, &lz_xf, codecPoolSize(lanes));
    defer lz.deinit(gpa);
    try testing.expect(lz.shared);
    try testing.expectEqual(@as(usize, 1), lz.capacity());
    const a = lz.acquire(testing.io);
    const b = lz.acquire(testing.io);
    try testing.expectEqual(a, b);
    try testing.expectEqual(@as(usize, 1), lz.permits.available(testing.io));
    lz.release(testing.io, a);
    lz.release(testing.io, b);
}

test "the scratch pool: capacity handouts are distinct DMA-aligned buffers of the scratch length; the one past the cap parks (counted) until a release; every buffer returns" {
    const gpa = testing.allocator;
    var pool = try ScratchPool.init(gpa, 2, scratchLen(4096));
    defer pool.deinit(gpa);
    try testing.expectEqual(@as(usize, 2), pool.capacity());
    const waits0 = stats.transform_scratch_pool_waits.load();
    const a = pool.acquire(testing.io);
    const b = pool.acquire(testing.io);
    try testing.expect(a.ptr != b.ptr);
    try testing.expectEqual(scratchLen(4096), a.len);
    try testing.expectEqual(@as(usize, 0), @intFromPtr(a.ptr) % DMA_GRAIN);
    try testing.expectEqual(waits0, stats.transform_scratch_pool_waits.load());
    const Third = struct {
        fn run(p: *ScratchPool, got: *std.atomic.Value(usize)) void {
            const c = p.acquire(testing.io);
            got.store(@intFromPtr(c.ptr), .release);
            p.release(testing.io, c);
        }
    };
    var got: std.atomic.Value(usize) = .init(0);
    const th = try std.Thread.spawn(.{}, Third.run, .{ &pool, &got });
    while (pool.permits.waiters(testing.io) == 0) try std.Thread.yield();
    try testing.expectEqual(waits0 + 1, stats.transform_scratch_pool_waits.load());
    pool.release(testing.io, a);
    th.join();
    try testing.expectEqual(@intFromPtr(a.ptr), got.load(.acquire));
    pool.release(testing.io, b);
    try testing.expectEqual(@as(usize, 2), pool.permits.available(testing.io));
}

fn frameFuzz(_: void, smith: *std.testing.Smith) !void {
    var buf: [4096]u8 = undefined;
    const len = smith.slice(&buf);
    const cfgs = [_]VolumeTransform{
        .{ .compression = .lz4, .block_size = 4096 },
        .{ .compression = .zstd, .block_size = 4096 },
        .{ .compression = .none, .encryption = .aes256gcm, .block_size = 4096 },
    };
    for (cfgs) |cfg| _ = decodeHeader(cfg, buf[0..len]) catch {};
}

test "fuzz: decodeHeader is total on arbitrary bytes" {
    try fuzzkit.fuzz({}, frameFuzz, .{ .corpus = &.{fuzzSlice("SQZB\x02\x00\x01\x00\x10\x00\x00\x00\x00\x10\x00\x00" ++ @as([80]u8, @splat(0)) ++ @as([16]u8, @splat(0xaa)))} });
}

fn tableFuzz(_: void, smith: *std.testing.Smith) !void {
    var page: [HEADER_PAGE_LEN + 64]u8 = undefined;
    const len = smith.slice(&page);
    var window: [4096]u8 = undefined;
    const wlen = smith.slice(&window);
    const window_off = smith.value(u16);
    const cfgs = [_]VolumeTransform{
        .{ .compression = .lz4, .block_size = 131072 },
        .{ .compression = .zstd, .block_size = 131072 },
        .{ .compression = .none, .encryption = .aes256gcm, .block_size = 131072 },
    };
    var env = testEnv();
    defer env.deinit();
    for (cfgs) |cfg| {
        const h = decodeHeader(cfg, page[0..len]) catch continue;
        const t = decodeTable(cfg, h, page[0..len]) catch continue;
        if (t.count == 0) continue;
        const span = frameSpan(h, t, 0, h.plain_len) catch continue;
        if (cfg.encryption != .none) continue;
        const xf = Transform.init(&env, cfg, null) catch continue;
        var tr = Codec.init(testing.allocator, &xf) catch continue;
        defer tr.deinit(testing.allocator);
        var out: [2 * FRAME_GRAIN + 1]u8 = undefined;
        _ = tr.decodeFrames(page[0..len], window[0..wlen], window_off, span, &out) catch {};
    }
}

test "fuzz: decodeTable, frameSpan and decodeFrames are total on arbitrary header pages and windows" {
    try fuzzkit.fuzz({}, tableFuzz, .{ .corpus = &.{fuzzSlice("SQZB\x02\x00\x01\x00\x10\x00\x00\x00\x00\x10\x00\x00" ++ @as([80]u8, @splat(0)) ++ @as([16]u8, @splat(0)) ++ @as([16]u8, @splat(0)))} });
}

test "property: readInner is total under a PRNG over mutations of valid lz4/zstd/AEAD images (no panic, every refusal typed)" {
    const gpa = testing.allocator;
    var env = testEnv();
    defer env.deinit();
    var d = deriveVolumeKey("fuzz-32-byte-or-longer-key-material-string", &(@as([32]u8, @splat(7))));
    defer d.zeroize();
    const c = try CryptoState.init(gpa, testing.io, .chacha20poly1305, d, null, 0, 1);
    defer c.deinit();
    const block: u32 = 8192;
    const cfgs = [_]VolumeTransform{
        .{ .compression = .lz4, .block_size = block },
        .{ .compression = .zstd, .block_size = block },
        .{ .compression = .lz4, .encryption = .chacha20poly1305, .block_size = block },
    };
    var prng = std.Random.DefaultPrng.init(0xabc);
    const r = prng.random();
    const scratch = try gpa.alloc(u8, scratchLen(block));
    defer gpa.free(scratch);
    const out = try gpa.alloc(u8, scratchLen(block));
    defer gpa.free(out);
    var plain: [8192]u8 = undefined;
    fillCompressible(&plain, 9);
    for (cfgs) |cfg| {
        const xf = try Transform.init(&env, cfg, if (cfg.encryption != .none) c else null);
        var t = try Codec.init(gpa, &xf);
        defer t.deinit(gpa);
        const image = try gpa.dupe(u8, try t.processWrite(plain[0..6000], scratch));
        defer gpa.free(image);
        const mut = try gpa.alloc(u8, image.len);
        defer gpa.free(mut);
        for (0..2000) |_| {
            @memcpy(mut, image);
            for (0..r.intRangeAtMost(usize, 1, 4)) |_| mut[r.intRangeLessThan(usize, 0, image.len)] ^= @as(u8, 1) << r.int(u3);
            _ = t.readInner(mut, out) catch {};
            _ = t.readInner(mut[0..r.intRangeAtMost(usize, 0, image.len)], out) catch {};
            // The sub-block decoder over the same mutations: a torn page or
            // window is a typed refusal, never a panic.
            if (decodeHeader(cfg, mut)) |h| {
                if (decodeTable(cfg, h, mut)) |tb| {
                    if (tb.count > 0) {
                        if (frameSpan(h, tb, 0, h.plain_len)) |span| {
                            const lo: usize = @intCast(@min(span.lo, mut.len));
                            const hi: usize = @intCast(@min(span.hi, mut.len));
                            _ = t.decodeFramesInner(mut[0..HEADER_PAGE_LEN], mut[lo..hi], lo, span, out) catch {};
                        } else |_| {}
                    }
                } else |_| {}
            } else |_| {}
        }
        for (0..500) |_| {
            const len = r.intRangeAtMost(usize, 0, mut.len);
            r.bytes(mut[0..len]);
            _ = t.readInner(mut[0..len], out) catch {};
        }
    }
}

/// A `Smith.slice` input: `u32 LE len ‖ bytes`, built at comptime.
fn fuzzSlice(comptime bytes: []const u8) []const u8 {
    const framed = comptime blk: {
        var len: [4]u8 = undefined;
        std.mem.writeInt(u32, &len, bytes.len, .little);
        break :blk len ++ bytes[0..bytes.len].*;
    };
    return &framed;
}

test "key files (Linux): a 0600 regular file owned by us reads and trims; group/other bits, a symlink, a directory, short material, an oversized file refuse by class; resolution order flag → env → key dir; a wrong key refuses KeyMismatch; hardenProcess leaves the process non-dumpable with core 0" {
    if (builtin.os.tag != .linux) return testkit.skip(@src(), .no_linux, "transform: Linux only");
    const io = testing.io;
    var dir = testing.tmpDir(.{});
    defer dir.cleanup();
    var pb: [std.fs.max_path_bytes]u8 = undefined;
    const base = pb[0..try dir.dir.realPath(io, &pb)];
    const gpa = testing.allocator;
    const key_path = try std.fmt.allocPrint(gpa, "{s}/k.key", .{base});
    defer gpa.free(key_path);
    const material = "  0123456789abcdef0123456789abcdef-material\n";
    try dir.dir.writeFile(io, .{ .sub_path = "k.key", .data = material });
    {
        var zb: [linux.PATH_MAX]u8 = undefined;
        const z = try linux_keys.pathZ(&zb, key_path);
        _ = linux.chmod(z, 0o600);
    }
    var out: [MAX_KEY_FILE_BYTES + 1]u8 = undefined;
    defer std.crypto.secureZero(u8, &out);
    log.testing.expectErrorLogs(0);
    const m = try linux_keys.readKeyFile(key_path, &out);
    try testing.expectEqualStrings(trimMaterial(material), m);
    // Hardened.
    try testing.expectEqual(@as(usize, 0), linux.prctl(@backingInt(linux.PR.GET_DUMPABLE), 0, 0, 0, 0));
    var lim: linux.rlimit = undefined;
    try testing.expectEqual(@as(usize, 0), linux.getrlimit(.CORE, &lim));
    try testing.expectEqual(@as(linux.rlim_t, 0), lim.cur);
    try testing.expectEqual(@as(linux.rlim_t, 0), lim.max);
    // Permissions.
    {
        var zb: [linux.PATH_MAX]u8 = undefined;
        const z = try linux_keys.pathZ(&zb, key_path);
        _ = linux.chmod(z, 0o640);
        try testing.expectError(error.KeyFilePermissions, linux_keys.readKeyFile(key_path, &out));
        _ = linux.chmod(z, 0o600);
    }
    // A symlink.
    try dir.dir.symLink(io, "k.key", "link.key", .{});
    const link_path = try std.fmt.allocPrint(gpa, "{s}/link.key", .{base});
    defer gpa.free(link_path);
    try testing.expectError(error.KeyFileIsSymlink, linux_keys.readKeyFile(link_path, &out));
    // A directory.
    try testing.expectError(error.KeyFileNotRegular, linux_keys.readKeyFile(base, &out));
    // Short material.
    try dir.dir.writeFile(io, .{ .sub_path = "short.key", .data = "too-short\n" });
    const short_path = try std.fmt.allocPrint(gpa, "{s}/short.key", .{base});
    defer gpa.free(short_path);
    {
        var zb: [linux.PATH_MAX]u8 = undefined;
        _ = linux.chmod(try linux_keys.pathZ(&zb, short_path), 0o600);
    }
    try testing.expectError(error.KeyMaterialTooShort, linux_keys.readKeyFile(short_path, &out));
    // Oversized.
    {
        const big = try gpa.alloc(u8, MAX_KEY_FILE_BYTES + 1);
        defer gpa.free(big);
        @memset(big, 'x');
        try dir.dir.writeFile(io, .{ .sub_path = "big.key", .data = big });
    }
    const big_path = try std.fmt.allocPrint(gpa, "{s}/big.key", .{base});
    defer gpa.free(big_path);
    {
        var zb: [linux.PATH_MAX]u8 = undefined;
        _ = linux.chmod(try linux_keys.pathZ(&zb, big_path), 0o600);
    }
    try testing.expectError(error.KeyFileTooLarge, linux_keys.readKeyFile(big_path, &out));
    // Missing.
    try testing.expectError(error.KeyFileOpenFailed, linux_keys.readKeyFile("/build/tmp/does-not-exist.key", &out));
    // Resolution: the flag wins over the env; the env over the key dir; none ⇒ NoKeySource.
    var env = testEnv();
    defer env.deinit();
    var path_buf: [128]u8 = undefined;
    const key_id: [8]u8 = .{ 1, 2, 3, 4, 5, 6, 7, 8 };
    try testing.expectError(error.NoKeySource, linux_keys.resolveKeyMaterial(&env, null, key_id, &path_buf, &out));
    try env.put("SQUEEZEFS_ENCRYPT_KEY_FILE", short_path);
    try testing.expectError(error.KeyMaterialTooShort, linux_keys.resolveKeyMaterial(&env, null, key_id, &path_buf, &out));
    const via_flag = try linux_keys.resolveKeyMaterial(&env, key_path, key_id, &path_buf, &out);
    try testing.expectEqual(KeySource.flag, via_flag.source);
    try env.put("SQUEEZEFS_ENCRYPT_KEY_FILE", key_path);
    const via_env = try linux_keys.resolveKeyMaterial(&env, null, key_id, &path_buf, &out);
    try testing.expectEqual(KeySource.env, via_env.source);
    try testing.expectEqualStrings(trimMaterial(material), via_env.material);
    // The id check.
    const salt: [32]u8 = @splat(0x42);
    const derived = deriveVolumeKey(trimMaterial(material), &salt);
    var served: []const u8 = "";
    var vk = try linux_keys.resolveVolumeKey(&env, null, &salt, derived.key_id, &path_buf, &served);
    defer vk.zeroize();
    try testing.expectEqual(derived.kek, vk.kek);
    try testing.expectEqualStrings(key_path, served);
    try testing.expectError(error.KeyMismatch, linux_keys.resolveVolumeKey(&env, null, &salt, key_id, &path_buf, &served));
    var buf: [512]u8 = undefined;
    var w: std.Io.Writer = .fixed(&buf);
    try writeKeyFileRefusal(&w, error.NoKeySource, "/etc/squeezefs/keys/0102030405060708.key");
    try testing.expect(std.mem.indexOf(u8, w.buffered(), "--encrypt-key") != null and std.mem.indexOf(u8, w.buffered(), "SQUEEZEFS_ENCRYPT_KEY_FILE") != null and std.mem.indexOf(u8, w.buffered(), "/etc/squeezefs/keys/") != null);
    try log.testing.expectAllFired();
}

fn transformNamesFuzz(_: void, smith: *std.testing.Smith) !void {
    var buf: [64]u8 = undefined;
    const len = smith.slice(&buf);
    const s = buf[0..len];
    _ = Compression.parse(s);
    _ = Encryption.parse(s);
}

test "fuzz: the compression and encryption name parsers are total on arbitrary text" {
    try fuzzkit.fuzz({}, transformNamesFuzz, .{ .corpus = &.{ fuzzkit.slice("lz4"), fuzzkit.slice("aes-256-gcm"), fuzzkit.slice("ZSTD:3") } });
}
