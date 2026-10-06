//! ONE typed error set and ONE errno table for the I/O and metadata planes
//! (charter §12 "Errors are error unions with typed classes … errno mapping
//! lives in one table"; decision 0003 WP-C row 23). A metadata module keeps
//! its own error NAMES and maps each onto a member here (`coreError`); the
//! errno and the class are read from this table alone.
//!
//! Three classes, three sub-sets — a module declares its own `Error` as a
//! union of the sub-sets (or of `Error` itself) so coercion between layers
//! is free and `classOf`/`toErrno` are total over everything a handler can
//! see. The table is read in both directions: a CQE's `-res` through
//! `fromErrno`, a reply through `toErrno`.
//!
//! The reply law: backpressure is `EAGAIN` (the caller may retry), never
//! `EINVAL` — the reference answered EINVAL for a full queue, which lies to
//! the application ("bad argument") and defeats retry logic keyed on
//! EAGAIN/EBUSY (decision 0002 ledger). Refusals (a caller bug, an
//! inadmissible device) are `EINVAL`; a fence or a dead lane is `EIO`; a
//! dismounted volume is `EROFS`; the one reservation-conflict errno
//! (`EBADE`, the block layer's `BLK_STS_RESV_CONFLICT`) round-trips.

const std = @import("std");
const linux = std.os.linux;
const log = @import("log.zig");

/// Nothing final happened: the caller may try again later.
pub const Retryable = error{
    /// A bounded request queue refused (backpressure; counted per layer).
    QueueFull,
    /// A lane's inbound queue refused (the journal lane's backpressure).
    LaneQueueFull,
    Interrupted,
    WouldBlock,
};

/// The device, the kernel or the lane answered, and the answer is final
/// for this op.
pub const Terminal = error{
    InputOutput,
    NoSpaceLeft,
    QuotaExceeded,
    FileNotFound,
    AccessDenied,
    BadFileDescriptor,
    IsDir,
    NotDir,
    PathAlreadyExists,
    TooManyOpenFiles,
    NameTooLong,
    FileTooBig,
    ReadOnlyFileSystem,
    NotSupported,
    Canceled,
    OutOfMemory,
    /// The kernel said EINVAL (e.g. an O_DIRECT shape the driver rejects).
    InvalidArgument,
    /// A write SQE completed with 0 bytes.
    ShortWrite,
    /// The kernel returned fewer bytes than asked (past-EOF on a file; a
    /// block device is all-or-EIO).
    ShortRead,
    /// An SQE could not be pushed even after flushing the SQ.
    SubmissionQueueOverflow,
    /// The lane's ring broke; every op it held failed with this.
    RingFailed,
    /// The pool or lane shut down before the op completed.
    WorkerClosed,
    /// Teardown drained the request before it reached the ring.
    WorkerShutdown,
    /// EBADE — the block layer's `BLK_STS_RESV_CONFLICT`; latches the fence.
    ReservationConflict,
    /// The checkpoint past a journal hole this ack depended on could not drain.
    HoleCheckpointFailed,
    /// An errno outside the table (logged with its number; replies EIO).
    Unexpected,
};

/// A caller bug or an inadmissible device/volume state: never retried.
pub const Refusal = error{
    UnalignedDirectWrite,
    GrainNotAdmissible,
    NotADeviceOrFile,
    DirectOpenRefused,
    CannotStat,
    /// Pooled read larger than the pool slot.
    ReadSizeExceedsPool,
    /// Fence latched / custody poisoned (EIO until remount).
    WriterGuardFenced,
    /// Dismount teardown completed (EROFS).
    DismountRefused,
    /// O_DIRECT open failed — buffered device I/O is not a supported mode.
    DirectIoUnsupported,
    /// The volume latched FAILED: every later op answers this until remount.
    VolumeFailed,
    /// The lane is stopping or stopped.
    LaneStopped,
    /// Another writer holds the volume's single-writer guard (EBUSY).
    Busy,
    /// A blank, pre-v4 or capability-stamped volume this binary does not
    /// mount (EINVAL: reformat or a later binary).
    UnsupportedVolume,
    /// A key that decodes as no kind, or not as the kind it was staged
    /// under (EINVAL: a caller bug).
    MalformedKey,
    /// A record value or journal entry above its cap (E2BIG, `setxattr(2)`'s
    /// "exceeds a filesystem-specific limit").
    ValueTooLarge,
};

pub const Error = Retryable || Terminal || Refusal;

pub const Class = enum { retryable, terminal, refusal };

pub fn classOf(err: Error) Class {
    return switch (err) {
        error.QueueFull, error.LaneQueueFull, error.Interrupted, error.WouldBlock => .retryable,
        error.UnalignedDirectWrite, error.GrainNotAdmissible, error.NotADeviceOrFile, error.DirectOpenRefused, error.CannotStat, error.ReadSizeExceedsPool, error.WriterGuardFenced, error.DismountRefused, error.DirectIoUnsupported, error.VolumeFailed, error.LaneStopped, error.Busy, error.UnsupportedVolume, error.MalformedKey, error.ValueTooLarge => .refusal,
        else => .terminal,
    };
}

const Row = struct { e: linux.E, err: Error };

/// THE errno table: the kernel errnos with a one-to-one error. Both
/// directions read it; `toErrno` adds the class rows for errors the kernel
/// never produces.
const table = [_]Row{
    .{ .e = .IO, .err = error.InputOutput },
    .{ .e = .NOSPC, .err = error.NoSpaceLeft },
    .{ .e = .DQUOT, .err = error.QuotaExceeded },
    .{ .e = .NOENT, .err = error.FileNotFound },
    .{ .e = .ACCES, .err = error.AccessDenied },
    .{ .e = .PERM, .err = error.AccessDenied },
    .{ .e = .BADF, .err = error.BadFileDescriptor },
    .{ .e = .ISDIR, .err = error.IsDir },
    .{ .e = .NOTDIR, .err = error.NotDir },
    .{ .e = .EXIST, .err = error.PathAlreadyExists },
    .{ .e = .MFILE, .err = error.TooManyOpenFiles },
    .{ .e = .NFILE, .err = error.TooManyOpenFiles },
    .{ .e = .NAMETOOLONG, .err = error.NameTooLong },
    .{ .e = .FBIG, .err = error.FileTooBig },
    .{ .e = .ROFS, .err = error.ReadOnlyFileSystem },
    .{ .e = .OPNOTSUPP, .err = error.NotSupported },
    .{ .e = .CANCELED, .err = error.Canceled },
    .{ .e = .NOMEM, .err = error.OutOfMemory },
    .{ .e = .INVAL, .err = error.InvalidArgument },
    .{ .e = .INTR, .err = error.Interrupted },
    .{ .e = .AGAIN, .err = error.WouldBlock },
    .{ .e = .BADE, .err = error.ReservationConflict },
    .{ .e = .BUSY, .err = error.Busy },
    .{ .e = .@"2BIG", .err = error.ValueTooLarge },
};

/// Kernel → error. An errno outside the table is `Unexpected`, logged once
/// per occurrence with its number so the table can grow from evidence.
pub fn fromErrno(e: linux.E) Error {
    for (table) |row| if (row.e == e) return row.err;
    log.warn("errno {d} ({t}) is outside the table — reported as Unexpected (EIO)", .{ @backingInt(e), e });
    return error.Unexpected;
}

/// A CQE's `res`: negative is `-errno`.
pub fn fromCqeRes(res: i32) Error {
    return fromErrno(@fromBackingInt(@as(u16, @intCast(-res))));
}

/// Error → errno for the reply path (total).
pub fn toErrno(err: Error) linux.E {
    for (table) |row| if (row.err == err) return row.e;
    return switch (err) {
        // Backpressure: retry, never "bad argument".
        error.QueueFull, error.LaneQueueFull => .AGAIN,
        // Refusals: the caller's shape or the device's admissibility.
        error.UnalignedDirectWrite, error.GrainNotAdmissible, error.NotADeviceOrFile, error.DirectOpenRefused, error.CannotStat, error.ReadSizeExceedsPool, error.DirectIoUnsupported, error.UnsupportedVolume, error.MalformedKey => .INVAL,
        error.DismountRefused => .ROFS,
        // The fence, a dead lane, a lost op, a short transfer: EIO.
        error.WriterGuardFenced, error.VolumeFailed, error.LaneStopped, error.RingFailed, error.WorkerClosed, error.WorkerShutdown, error.ShortWrite, error.ShortRead, error.SubmissionQueueOverflow, error.HoleCheckpointFailed, error.Unexpected => .IO,
        else => unreachable, // every table row returned above
    };
}

const testing = std.testing;

test "every table row round-trips; aliases map to the first row's errno" {
    for (table) |row| try testing.expectEqual(row.err, fromErrno(row.e));
    try testing.expectEqual(linux.E.IO, toErrno(error.InputOutput));
    try testing.expectEqual(linux.E.NOSPC, toErrno(error.NoSpaceLeft));
    try testing.expectEqual(linux.E.BADE, toErrno(error.ReservationConflict));
    // ACCES and PERM both decode to AccessDenied; the reply is ACCES (the first row).
    try testing.expectEqual(error.AccessDenied, fromErrno(.PERM));
    try testing.expectEqual(linux.E.ACCES, toErrno(error.AccessDenied));
    try testing.expectEqual(error.InputOutput, fromCqeRes(-@as(i32, @backingInt(linux.E.IO))));
}

test "an errno outside the table is Unexpected and replies EIO" {
    try testing.expectEqual(error.Unexpected, fromErrno(.NOTSOCK));
    try testing.expectEqual(linux.E.IO, toErrno(error.Unexpected));
}

test "classes: backpressure is retryable and replies EAGAIN, never EINVAL; refusals EINVAL; the fence EIO; dismount EROFS" {
    try testing.expectEqual(Class.retryable, classOf(error.QueueFull));
    try testing.expectEqual(Class.retryable, classOf(error.LaneQueueFull));
    try testing.expectEqual(linux.E.AGAIN, toErrno(error.QueueFull));
    try testing.expectEqual(linux.E.AGAIN, toErrno(error.LaneQueueFull));
    try testing.expectEqual(Class.refusal, classOf(error.UnalignedDirectWrite));
    try testing.expectEqual(linux.E.INVAL, toErrno(error.UnalignedDirectWrite));
    try testing.expectEqual(linux.E.INVAL, toErrno(error.ReadSizeExceedsPool));
    try testing.expectEqual(Class.refusal, classOf(error.WriterGuardFenced));
    try testing.expectEqual(linux.E.IO, toErrno(error.WriterGuardFenced));
    try testing.expectEqual(Class.refusal, classOf(error.DismountRefused));
    try testing.expectEqual(linux.E.ROFS, toErrno(error.DismountRefused));
    try testing.expectEqual(Class.terminal, classOf(error.InputOutput));
    try testing.expectEqual(Class.terminal, classOf(error.RingFailed));
    try testing.expectEqual(linux.E.IO, toErrno(error.RingFailed));
    try testing.expectEqual(linux.E.IO, toErrno(error.WorkerClosed));
    // The metadata plane's refusals: the guard, an unmountable volume, a
    // malformed key, an oversized value.
    try testing.expectEqual(Class.refusal, classOf(error.Busy));
    try testing.expectEqual(linux.E.BUSY, toErrno(error.Busy));
    try testing.expectEqual(error.Busy, fromErrno(.BUSY));
    try testing.expectEqual(Class.refusal, classOf(error.UnsupportedVolume));
    try testing.expectEqual(linux.E.INVAL, toErrno(error.UnsupportedVolume));
    try testing.expectEqual(Class.refusal, classOf(error.MalformedKey));
    try testing.expectEqual(linux.E.INVAL, toErrno(error.MalformedKey));
    try testing.expectEqual(Class.refusal, classOf(error.ValueTooLarge));
    try testing.expectEqual(linux.E.@"2BIG", toErrno(error.ValueTooLarge));
}

test "toErrno is total: every member of the set has an errno" {
    inline for (@typeInfo(Error).error_set.error_names.?) |name| {
        const err: Error = @field(Error, name);
        _ = toErrno(err);
        _ = classOf(err);
    }
}

test "toErrno is total: every member of the set has a reply errno" {
    inline for (@typeInfo(Error).error_set.error_names.?) |name| {
        const err: Error = @field(Error, name);
        _ = toErrno(err);
        _ = classOf(err);
    }
}
