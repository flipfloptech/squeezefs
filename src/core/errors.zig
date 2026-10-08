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
    /// The cluster wire's typed retryable classes (charter §5.5: "surface
    /// as EAGAIN, never EIO/EINVAL"; `wire/frame.zig` `RefusalClass`).
    /// "Not now": the durable state is healthy, nothing was written.
    Deferred,
    /// The commit door refused a slot ANOTHER appender leases: re-resolve.
    SlotMoved,
    /// A served guard refused a key whose slot the server no longer leases.
    StaleHolderView,
    /// The object's holder has no published endpoint yet (or is dead until
    /// recovery re-leases its slots).
    HolderUnreachable,
    /// The token holder's membership screen refused this member inside a
    /// failover's re-assertion window.
    MembershipPending,
    /// The peer's listener closed before its challenge and the bounded
    /// dial retry ran out (its connection cap, or a shutdown).
    ListenerRefused,
    /// The owner's era moved under the caller; the SAME request is
    /// admissible on the retry.
    StaleOwnerEra,
    /// The node asked holds no manager lease for the volume named (a
    /// failover moved it): re-resolve the manager and re-dial.
    NotManager,
    /// The appender's extent grant is empty and its manager cannot refill
    /// now: "the heap is not full, the appender is out of grant" (charter
    /// §5.5 — EAGAIN-class, never ENOSPC).
    GrantExhausted,
    /// A token reader's channel to the holder is not ok (a failed round,
    /// a re-point in progress): the serve is refused, never answered from
    /// the bounded-staleness projection (decision 0002 row 261).
    TokenChannelStale,
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
    /// `rmdir`/`rename` onto a directory that still has entries (ENOTEMPTY).
    DirectoryNotEmpty,
    /// `link` at the hard-link ceiling (EMLINK).
    TooManyLinks,
    /// A directory chain deeper than any legal tree (ELOOP): the
    /// `dir_rename` ancestry walk met a cycle that already exists.
    TooManySymlinks,
    /// The caller's buffer cannot hold the value (ERANGE: `getxattr`/`listxattr`).
    BufferTooSmall,
    /// No such extended attribute (ENODATA; Linux's ENOATTR).
    NoAttribute,
    /// An operation this filesystem does not implement (ENOSYS: the kernel
    /// latches `fc->no_<op>` and stops asking).
    NotImplemented,
    /// A handle to a RETIRED `.stats`/`.config`/`.trace` generation (ESTALE:
    /// the zero entry TTL mints fresh on the next walk).
    StaleHandle,
    /// The operation is forbidden on this object whatever the mode bits
    /// (EPERM: a reserved xattr, a virtual inode's mutation) — distinct
    /// from `AccessDenied` (EACCES: the mode bits said no).
    PermissionDenied,
    /// An errno outside the table (logged with its number; replies EIO).
    Unexpected,
    /// A cluster-wire frame's HMAC did not verify: a SESSION error (the
    /// session is poisoned), never a skipped frame.
    MacInvalid,
    /// The cluster-wire session's transport failed (EOF, reset, a poisoned
    /// mux): every call parked on it fails; the caller re-dials.
    ConnectionLost,
    /// The peer's storage-trust gate refused our proof (we do not hold the
    /// set's `job:enroll` secret, or its schema is not ours).
    HandshakeRefused,
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
    /// A cluster-wire word names what durable state cannot — the verb's
    /// SCREEN refused it before any effect (`STATUS_REJECTED`; EINVAL: a
    /// peer bug, never retried).
    Rejected,
    /// An unknown wire schema, framing version or verb (charter §1.7:
    /// "unknown wire schema refuses" — never interpreted).
    WireSchemaRefused,
    /// A wire payload that did not decode (length, shape, reserved bytes).
    WireMalformed,
    /// A `refused` reply whose payload carried no typed class and no
    /// errno this table knows (the fallback spelling; a decoded
    /// `WireError` maps through the ONE table instead).
    WireRefused,
    /// The membership owner does not know the member's lease: self-fence
    /// (purge), then re-join fresh — never a retry (ESTALE).
    UnknownLease,
    /// A quiescent `-o ro` reader saw a writer appear: every metadata op
    /// answers ESTALE until the remount joins as a token reader.
    ReaderFailStopped,
};

pub const Error = Retryable || Terminal || Refusal;

pub const Class = enum { retryable, terminal, refusal };

pub fn classOf(err: Error) Class {
    return switch (err) {
        error.QueueFull, error.LaneQueueFull, error.Interrupted, error.WouldBlock, error.Deferred, error.SlotMoved, error.StaleHolderView, error.HolderUnreachable, error.MembershipPending, error.ListenerRefused, error.StaleOwnerEra, error.NotManager, error.GrantExhausted, error.TokenChannelStale => .retryable,
        error.UnalignedDirectWrite, error.GrainNotAdmissible, error.NotADeviceOrFile, error.DirectOpenRefused, error.CannotStat, error.ReadSizeExceedsPool, error.WriterGuardFenced, error.DismountRefused, error.DirectIoUnsupported, error.VolumeFailed, error.LaneStopped, error.Busy, error.UnsupportedVolume, error.MalformedKey, error.ValueTooLarge, error.Rejected, error.WireSchemaRefused, error.WireMalformed, error.WireRefused, error.UnknownLease, error.ReaderFailStopped => .refusal,
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
    .{ .e = .NOTEMPTY, .err = error.DirectoryNotEmpty },
    .{ .e = .MLINK, .err = error.TooManyLinks },
    .{ .e = .LOOP, .err = error.TooManySymlinks },
    .{ .e = .RANGE, .err = error.BufferTooSmall },
    .{ .e = .NODATA, .err = error.NoAttribute },
    .{ .e = .NOSYS, .err = error.NotImplemented },
    .{ .e = .STALE, .err = error.StaleHandle },
    // PERM also decodes to AccessDenied above (the first row wins on the
    // way in); on the way out PermissionDenied is the one EPERM reply.
    .{ .e = .PERM, .err = error.PermissionDenied },
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
        // Backpressure and the wire's typed retryable classes: retry, never
        // "bad argument" (charter §5.5).
        error.QueueFull, error.LaneQueueFull, error.Deferred, error.SlotMoved, error.StaleHolderView, error.HolderUnreachable, error.MembershipPending, error.ListenerRefused, error.StaleOwnerEra, error.NotManager, error.GrantExhausted, error.TokenChannelStale => .AGAIN,
        // Refusals: the caller's shape or the device's admissibility; a
        // wire word the screen refused; a foreign vocabulary.
        error.UnalignedDirectWrite, error.GrainNotAdmissible, error.NotADeviceOrFile, error.DirectOpenRefused, error.CannotStat, error.ReadSizeExceedsPool, error.DirectIoUnsupported, error.UnsupportedVolume, error.MalformedKey, error.Rejected, error.WireSchemaRefused, error.WireMalformed, error.WireRefused => .INVAL,
        error.DismountRefused => .ROFS,
        // A lease the owner does not know: the handle is stale; so is every
        // handle of a fail-stopped quiescent reader.
        error.UnknownLease, error.ReaderFailStopped => .STALE,
        // A peer that refused our storage-trust proof.
        error.HandshakeRefused => .ACCES,
        // The fence, a dead lane, a lost op, a short transfer, a poisoned
        // wire session: EIO.
        error.WriterGuardFenced, error.VolumeFailed, error.LaneStopped, error.RingFailed, error.WorkerClosed, error.WorkerShutdown, error.ShortWrite, error.ShortRead, error.SubmissionQueueOverflow, error.HoleCheckpointFailed, error.Unexpected, error.MacInvalid, error.ConnectionLost => .IO,
        else => unreachable, // every table row returned above
    };
}

const testing = std.testing;

test "every table row round-trips; aliases map to the first row's errno" {
    for (table) |row| {
        // Every error encodes to its row's errno; an aliased errno decodes
        // to its FIRST row's error (PERM → AccessDenied, MFILE/NFILE →
        // TooManyOpenFiles).
        // Encode: the error's first row (an alias row's error may encode to
        // its own first errno, which decodes back to the same error).
        const back = toErrno(row.err);
        if (back != row.e) try testing.expectEqual(row.err, fromErrno(back));
        // Decode: the errno's first row (PERM's first row is AccessDenied;
        // PermissionDenied is the reply-only spelling of EPERM).
        const decoded = fromErrno(row.e);
        if (decoded != row.err) try testing.expect(row.e == .PERM or toErrno(decoded) == row.e);
    }
    try testing.expectEqual(linux.E.IO, toErrno(error.InputOutput));
    try testing.expectEqual(linux.E.NOSPC, toErrno(error.NoSpaceLeft));
    try testing.expectEqual(linux.E.BADE, toErrno(error.ReservationConflict));
    // ACCES and PERM both decode to AccessDenied; the reply is ACCES (the first row).
    try testing.expectEqual(error.AccessDenied, fromErrno(.PERM));
    try testing.expectEqual(linux.E.ACCES, toErrno(error.AccessDenied));
    // PermissionDenied is the EPERM reply; StaleHandle the ESTALE one.
    try testing.expectEqual(linux.E.PERM, toErrno(error.PermissionDenied));
    try testing.expectEqual(linux.E.STALE, toErrno(error.StaleHandle));
    try testing.expectEqual(error.StaleHandle, fromErrno(.STALE));
    try testing.expectEqual(linux.E.LOOP, toErrno(error.TooManySymlinks));
    try testing.expectEqual(Class.terminal, classOf(error.TooManySymlinks));
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
    // The FUSE handlers' POSIX rows: every one terminal, every one a
    // kernel errno the application reads by name.
    try testing.expectEqual(linux.E.NOTEMPTY, toErrno(error.DirectoryNotEmpty));
    try testing.expectEqual(linux.E.MLINK, toErrno(error.TooManyLinks));
    try testing.expectEqual(linux.E.RANGE, toErrno(error.BufferTooSmall));
    try testing.expectEqual(linux.E.NODATA, toErrno(error.NoAttribute));
    try testing.expectEqual(linux.E.NOSYS, toErrno(error.NotImplemented));
    try testing.expectEqual(error.NotImplemented, fromErrno(.NOSYS));
    try testing.expectEqual(Class.terminal, classOf(error.NoAttribute));
}

test "the cluster wire's classes: every typed retryable is EAGAIN; a screen refusal and a foreign schema are EINVAL; an unknown lease is ESTALE; a poisoned session EIO" {
    const retryable = [_]Error{ error.Deferred, error.SlotMoved, error.StaleHolderView, error.HolderUnreachable, error.MembershipPending, error.ListenerRefused, error.StaleOwnerEra, error.NotManager, error.GrantExhausted };
    for (retryable) |e| {
        try testing.expectEqual(Class.retryable, classOf(e));
        try testing.expectEqual(linux.E.AGAIN, toErrno(e));
    }
    try testing.expectEqual(Class.refusal, classOf(error.Rejected));
    try testing.expectEqual(linux.E.INVAL, toErrno(error.Rejected));
    try testing.expectEqual(linux.E.INVAL, toErrno(error.WireSchemaRefused));
    try testing.expectEqual(linux.E.INVAL, toErrno(error.WireMalformed));
    try testing.expectEqual(linux.E.STALE, toErrno(error.UnknownLease));
    try testing.expectEqual(linux.E.STALE, toErrno(error.ReaderFailStopped));
    try testing.expectEqual(Class.refusal, classOf(error.ReaderFailStopped));
    try testing.expectEqual(Class.retryable, classOf(error.TokenChannelStale));
    try testing.expectEqual(linux.E.AGAIN, toErrno(error.TokenChannelStale));
    try testing.expectEqual(linux.E.ACCES, toErrno(error.HandshakeRefused));
    try testing.expectEqual(Class.terminal, classOf(error.MacInvalid));
    try testing.expectEqual(linux.E.IO, toErrno(error.MacInvalid));
    try testing.expectEqual(linux.E.IO, toErrno(error.ConnectionLost));
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
