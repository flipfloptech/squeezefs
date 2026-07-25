//! Control-plane **wire format** — the bootstrap xattr blob and the
//! AF_UNIX `SOCK_SEQPACKET` ctl messages (design-preload-interception
//! §5.2). Pure bytes: no syscalls, no I/O — the socket/`SCM_RIGHTS`
//! plumbing lives with the daemon host (`src/ipc_host.rs`) and the shim
//! (PR L4-5); this module is the encoding both speak.
//!
//! **ABI discipline (KD-7)**: like [`crate::layout`], this format is
//! version-locked to the build commit (equality checked in HELLO) with
//! [`crate::layout::IPC_ABI`] as the coarse structural guard — explicitly
//! **not** a stable ABI, forward-only. Every message is one SEQPACKET
//! datagram: fixed little-endian layout, a `u32` tag first.
//!
//! **Trust boundary**: decoders accept bytes from the untrusted peer.
//! They must be total (never panic on any input) and bounded (fixed-size
//! reads only) — malformed input is a [`WireError`], which the daemon
//! answers by refusing/poisoning loudly, never by crashing.

use crate::layout::Geometry;

/// The bootstrap virtual-xattr name (synthesized by the daemon's GETXATTR
/// only on interception-enabled mounts; reserved — filtered from
/// `listxattr`, EPERM on set).
pub const BOOTSTRAP_XATTR: &str = "user.squeezefs.il0";

/// Nonce width (anti-replay freshness, §5.2 nonce lifecycle).
pub const NONCE_LEN: usize = 32;

/// Fixed on-wire width of a build-commit identity string. The §5.2 prose
/// sketched `[u8;41]` (40-hex + NUL); the shipped field is 48 so the
/// `-dirty` suffix (`src/version.rs` form, 46 chars max) — which the skew
/// gate must SEE to refuse — actually fits. NUL-padded.
pub const BUILD_COMMIT_LEN: usize = 48;

/// Fixed on-wire width of a socket-name field (abstract AF_UNIX names are
/// ≤ 107 bytes; NUL-padded).
pub const SOCKET_NAME_LEN: usize = 108;

/// Decode failures (total, attributed — the peer is untrusted).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WireError {
    /// Datagram shorter than its fixed layout.
    Truncated,
    /// Unknown message tag.
    UnknownTag(u32),
    /// A fixed string field was not valid UTF-8 / not NUL-terminated
    /// inside its width.
    BadString,
    /// A refusal class code outside the known set.
    BadClass(u32),
}

impl core::fmt::Display for WireError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::Truncated => write!(f, "ctl datagram truncated"),
            Self::UnknownTag(t) => write!(f, "unknown ctl message tag {t}"),
            Self::BadString => write!(f, "ctl string field invalid"),
            Self::BadClass(c) => write!(f, "unknown refusal class {c}"),
        }
    }
}

impl std::error::Error for WireError {}

/// Refusal classes — the §8 daemon-side refusal ledger
/// (`ipc_bind_refused_{version,flags,mode,budget,nonce}` + the peercred
/// defense-in-depth class the adversarial matrix pins).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u32)]
pub enum RefuseClass {
    /// ABI / build-commit skew, incl. `unknown`/`-dirty` degenerate
    /// identities on either side (KD-7).
    Version = 1,
    /// Stale/unknown nonce (anti-replay, §5.2).
    Nonce = 2,
    /// fd flag/type screen: `O_PATH`, `O_APPEND`, `O_TMPFILE`-class,
    /// `O_SYNC`/`O_DSYNC`, non-`S_ISREG` (§5.2 screen rules 1–2).
    Flags = 3,
    /// The fd is not a rights-bearing capability **on this mount**:
    /// `st_dev` does not match (or the mount device is not resolved yet)
    /// — §5.2 screen rule 1's device check, §8 ledger class `mode`.
    Mode = 4,
    /// Admission refusal: arena budget (R5 component / shed target) or
    /// per-uid session cap (§5.7).
    Budget = 5,
    /// HELLO's claimed (pid, uid) contradicts kernel `SO_PEERCRED`
    /// (defense-in-depth; the fd stays the authorizer).
    Peercred = 6,
    /// Daemon-internal failure (memfd/map) — never the client's fault,
    /// always log-loud daemon-side.
    Internal = 7,
    /// Data plane not armed on this mount (no `-o interception`): the
    /// VL2 control-plane-only posture refuses every data HELLO before
    /// any fd screen. Carried on the wire so the shim's refusal line
    /// can name the actual cause + remedy (user directive 2026-07-25)
    /// instead of the formerly-opaque `Flags` class.
    Disabled = 8,
}

impl RefuseClass {
    pub fn from_u32(v: u32) -> Result<Self, WireError> {
        Ok(match v {
            1 => Self::Version,
            2 => Self::Nonce,
            3 => Self::Flags,
            4 => Self::Mode,
            5 => Self::Budget,
            6 => Self::Peercred,
            7 => Self::Internal,
            8 => Self::Disabled,
            other => return Err(WireError::BadClass(other)),
        })
    }
}

// ---------------------------------------------------------------------------
// fixed-field helpers
// ---------------------------------------------------------------------------

fn put_u32(out: &mut Vec<u8>, v: u32) {
    out.extend_from_slice(&v.to_le_bytes());
}

fn put_u64(out: &mut Vec<u8>, v: u64) {
    out.extend_from_slice(&v.to_le_bytes());
}

/// Length-prefixed variable string (admin frames). Truncation is a
/// caller bug: callers cap before encoding; encode clamps defensively.
fn put_var_str(out: &mut Vec<u8>, s: &str, cap: usize) {
    let bytes = &s.as_bytes()[..s.len().min(cap)];
    put_u32(out, bytes.len() as u32);
    out.extend_from_slice(bytes);
}

fn put_str<const N: usize>(out: &mut Vec<u8>, s: &str) {
    let mut buf = [0u8; N];
    let bytes = s.as_bytes();
    let n = bytes.len().min(N - 1); // always NUL-terminated inside width
    buf[..n].copy_from_slice(&bytes[..n]);
    out.extend_from_slice(&buf);
}

struct Reader<'a> {
    buf: &'a [u8],
    off: usize,
}

impl<'a> Reader<'a> {
    fn new(buf: &'a [u8]) -> Self {
        Self { buf, off: 0 }
    }

    fn take(&mut self, n: usize) -> Result<&'a [u8], WireError> {
        let end = self.off.checked_add(n).ok_or(WireError::Truncated)?;
        if end > self.buf.len() {
            return Err(WireError::Truncated);
        }
        let s = &self.buf[self.off..end];
        self.off = end;
        Ok(s)
    }

    fn u32(&mut self) -> Result<u32, WireError> {
        Ok(u32::from_le_bytes(self.take(4)?.try_into().unwrap()))
    }

    fn u64(&mut self) -> Result<u64, WireError> {
        Ok(u64::from_le_bytes(self.take(8)?.try_into().unwrap()))
    }

    fn str_fixed(&mut self, n: usize) -> Result<String, WireError> {
        let raw = self.take(n)?;
        let end = raw
            .iter()
            .position(|b| *b == 0)
            .ok_or(WireError::BadString)?;
        core::str::from_utf8(&raw[..end])
            .map(str::to_owned)
            .map_err(|_| WireError::BadString)
    }

    fn nonce(&mut self) -> Result<[u8; NONCE_LEN], WireError> {
        Ok(self.take(NONCE_LEN)?.try_into().unwrap())
    }

    /// Length-prefixed variable string (admin frames), bounded by `cap`.
    fn str_var(&mut self, cap: usize) -> Result<String, WireError> {
        let n = self.u32()? as usize;
        if n > cap {
            return Err(WireError::BadString);
        }
        core::str::from_utf8(self.take(n)?)
            .map(str::to_owned)
            .map_err(|_| WireError::BadString)
    }
}

// ---------------------------------------------------------------------------
// bootstrap blob
// ---------------------------------------------------------------------------

/// Blob magic (`SQZIL0\0\0` LE) — distinguishes a real bootstrap answer
/// from arbitrary xattr bytes.
pub const BLOB_MAGIC: u64 = u64::from_le_bytes(*b"SQZIL0\0\0");

/// The `user.squeezefs.il0` fixed-layout payload (§5.2): everything a shim
/// needs to rendezvous — one round trip per (process, mount).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BootstrapBlob {
    pub abi: u32,
    /// Reserved feature bits (0 in v1).
    pub flags: u32,
    /// This daemon's `src/version.rs` build-commit form (`-dirty`
    /// included — the skew gate must see it).
    pub build_commit: String,
    /// Abstract AF_UNIX socket name (no leading NUL on the wire).
    pub socket: String,
    /// RESERVED (empty in v1): the OQ-6 filesystem-path socket for
    /// container fleets — carried now so no ABI bump is needed later.
    pub socket_path: String,
    /// Current anti-replay nonce (multi-use within its TTL).
    pub nonce: [u8; NONCE_LEN],
}

impl BootstrapBlob {
    pub fn encode(&self) -> Vec<u8> {
        let mut out =
            Vec::with_capacity(8 + 4 + 4 + BUILD_COMMIT_LEN + 2 * SOCKET_NAME_LEN + NONCE_LEN);
        put_u64(&mut out, BLOB_MAGIC);
        put_u32(&mut out, self.abi);
        put_u32(&mut out, self.flags);
        put_str::<BUILD_COMMIT_LEN>(&mut out, &self.build_commit);
        put_str::<SOCKET_NAME_LEN>(&mut out, &self.socket);
        put_str::<SOCKET_NAME_LEN>(&mut out, &self.socket_path);
        out.extend_from_slice(&self.nonce);
        out
    }

    pub fn decode(buf: &[u8]) -> Result<Self, WireError> {
        let mut r = Reader::new(buf);
        let magic = r.u64()?;
        if magic != BLOB_MAGIC {
            return Err(WireError::BadString);
        }
        Ok(Self {
            abi: r.u32()?,
            flags: r.u32()?,
            build_commit: r.str_fixed(BUILD_COMMIT_LEN)?,
            socket: r.str_fixed(SOCKET_NAME_LEN)?,
            socket_path: r.str_fixed(SOCKET_NAME_LEN)?,
            nonce: r.nonce()?,
        })
    }
}

// ---------------------------------------------------------------------------
// ctl messages
// ---------------------------------------------------------------------------

const TAG_HELLO: u32 = 1;
const TAG_SESSION_OK: u32 = 2;
const TAG_REFUSE: u32 = 3;
const TAG_BIND: u32 = 4;
const TAG_BIND_OK: u32 = 5;
const TAG_BIND_REFUSED: u32 = 6;
const TAG_UNBIND: u32 = 7;
const TAG_ADMIN_HELLO: u32 = 8;
const TAG_ADMIN_OK: u32 = 9;
const TAG_ADMIN_REQ: u32 = 10;
const TAG_ADMIN_REPLY: u32 = 11;

/// Upper bound on any encoded ctl message (receive-buffer sizing).
/// Admin frames (VL2 §5.1.4) carry variable JSON bodies and use the
/// larger bound; data-plane frames stay tiny.
pub const CTL_MSG_MAX: usize = 64 * 1024;

/// Per-field caps inside admin frames (frames stay under CTL_MSG_MAX
/// with headroom; oversize replies refuse instead of truncating).
pub const ADMIN_VERB_MAX: usize = 64;
pub const ADMIN_BODY_MAX: usize = 60 * 1024;

/// One ctl datagram (§5.2 bind protocol). fd attachments ride `SCM_RIGHTS`
/// beside the datagram, never inside it: `Hello` and `Bind` each carry
/// exactly one fd; `SessionOk` carries the sealed session memfd.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CtlMsg {
    /// Client → daemon: session establishment. The attached fd is the
    /// kernel-verified mount-membership credential (screened §5.2); the
    /// claimed (pid, uid) must match `SO_PEERCRED` (defense-in-depth).
    Hello {
        abi: u32,
        pid: u32,
        uid: u32,
        build_commit: String,
        nonce: [u8; NONCE_LEN],
    },
    /// Daemon → client: session established; the attached fd is the
    /// sealed memfd, geometry describes its layout.
    SessionOk { geometry: Geometry },
    /// Daemon → client: session refused (class counted daemon-side).
    Refuse { class: RefuseClass },
    /// Client → daemon: bind the attached fd (screened §5.2).
    Bind,
    /// Daemon → client: bound. `read_ok`/`write_ok` are the per-op rights
    /// derived from the fd's access mode (both directions, §5.2 rule 3).
    BindOk {
        binding_id: u64,
        ino: u64,
        read_ok: bool,
        write_ok: bool,
    },
    /// Daemon → client: bind refused (class counted daemon-side).
    BindRefused { class: RefuseClass },
    /// Client → daemon: release a binding (shim close-path, refcounted
    /// client-side — last close sends this).
    Unbind { binding_id: u64 },
    /// Client → daemon: open an ADMIN control session (VL2 §5.1.4).
    /// No fd credential — `SO_PEERCRED` is the check (uid 0 or the
    /// mount-owning uid; claimed pid/uid must match the kernel's).
    AdminHello { pid: u32, uid: u32 },
    /// Daemon → client: admin session admitted.
    AdminOk,
    /// Client → daemon: one admin verb (`job-list`, `job-pause`, …)
    /// with a verb-specific argument string.
    AdminReq { verb: String, arg: String },
    /// Daemon → client: verb outcome; `body` is verb-specific JSON or
    /// an error message when `ok` is false.
    AdminReply { ok: bool, body: String },
}

impl CtlMsg {
    pub fn encode(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(CTL_MSG_MAX);
        match self {
            Self::Hello {
                abi,
                pid,
                uid,
                build_commit,
                nonce,
            } => {
                put_u32(&mut out, TAG_HELLO);
                put_u32(&mut out, *abi);
                put_u32(&mut out, *pid);
                put_u32(&mut out, *uid);
                put_str::<BUILD_COMMIT_LEN>(&mut out, build_commit);
                out.extend_from_slice(nonce);
            }
            Self::SessionOk { geometry } => {
                put_u32(&mut out, TAG_SESSION_OK);
                put_u32(&mut out, geometry.ring_entries);
                put_u32(&mut out, geometry.slots);
                put_u64(&mut out, geometry.arena_bytes);
                put_u32(&mut out, geometry.max_op_bytes);
            }
            Self::Refuse { class } => {
                put_u32(&mut out, TAG_REFUSE);
                put_u32(&mut out, *class as u32);
            }
            Self::Bind => put_u32(&mut out, TAG_BIND),
            Self::BindOk {
                binding_id,
                ino,
                read_ok,
                write_ok,
            } => {
                put_u32(&mut out, TAG_BIND_OK);
                put_u64(&mut out, *binding_id);
                put_u64(&mut out, *ino);
                put_u32(&mut out, u32::from(*read_ok));
                put_u32(&mut out, u32::from(*write_ok));
            }
            Self::BindRefused { class } => {
                put_u32(&mut out, TAG_BIND_REFUSED);
                put_u32(&mut out, *class as u32);
            }
            Self::Unbind { binding_id } => {
                put_u32(&mut out, TAG_UNBIND);
                put_u64(&mut out, *binding_id);
            }
            Self::AdminHello { pid, uid } => {
                put_u32(&mut out, TAG_ADMIN_HELLO);
                put_u32(&mut out, *pid);
                put_u32(&mut out, *uid);
            }
            Self::AdminOk => put_u32(&mut out, TAG_ADMIN_OK),
            Self::AdminReq { verb, arg } => {
                put_u32(&mut out, TAG_ADMIN_REQ);
                put_var_str(&mut out, verb, ADMIN_VERB_MAX);
                put_var_str(&mut out, arg, ADMIN_BODY_MAX);
            }
            Self::AdminReply { ok, body } => {
                put_u32(&mut out, TAG_ADMIN_REPLY);
                put_u32(&mut out, u32::from(*ok));
                put_var_str(&mut out, body, ADMIN_BODY_MAX);
            }
        }
        debug_assert!(out.len() <= CTL_MSG_MAX);
        out
    }

    pub fn decode(buf: &[u8]) -> Result<Self, WireError> {
        let mut r = Reader::new(buf);
        Ok(match r.u32()? {
            TAG_HELLO => Self::Hello {
                abi: r.u32()?,
                pid: r.u32()?,
                uid: r.u32()?,
                build_commit: r.str_fixed(BUILD_COMMIT_LEN)?,
                nonce: r.nonce()?,
            },
            TAG_SESSION_OK => Self::SessionOk {
                geometry: Geometry {
                    ring_entries: r.u32()?,
                    slots: r.u32()?,
                    arena_bytes: r.u64()?,
                    max_op_bytes: r.u32()?,
                    _pad: 0,
                },
            },
            TAG_REFUSE => Self::Refuse {
                class: RefuseClass::from_u32(r.u32()?)?,
            },
            TAG_BIND => Self::Bind,
            TAG_BIND_OK => Self::BindOk {
                binding_id: r.u64()?,
                ino: r.u64()?,
                read_ok: r.u32()? != 0,
                write_ok: r.u32()? != 0,
            },
            TAG_BIND_REFUSED => Self::BindRefused {
                class: RefuseClass::from_u32(r.u32()?)?,
            },
            TAG_UNBIND => Self::Unbind {
                binding_id: r.u64()?,
            },
            TAG_ADMIN_HELLO => Self::AdminHello {
                pid: r.u32()?,
                uid: r.u32()?,
            },
            TAG_ADMIN_OK => Self::AdminOk,
            TAG_ADMIN_REQ => Self::AdminReq {
                verb: r.str_var(ADMIN_VERB_MAX)?,
                arg: r.str_var(ADMIN_BODY_MAX)?,
            },
            TAG_ADMIN_REPLY => Self::AdminReply {
                ok: r.u32()? != 0,
                body: r.str_var(ADMIN_BODY_MAX)?,
            },
            other => return Err(WireError::UnknownTag(other)),
        })
    }
}

/// Whether a build-commit identity is degenerate for the skew gate:
/// `unknown` (no-git tarball) or `-dirty` (modified tree). Equality
/// between two such identities proves nothing (§5.2 screen rule 4) — both
/// sides refuse unless the counted dev override is set.
pub fn build_commit_degenerate(commit: &str) -> bool {
    commit == "unknown" || commit.ends_with("-dirty") || commit.is_empty()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn blob_roundtrip_and_negative_probe() {
        let blob = BootstrapBlob {
            abi: crate::layout::IPC_ABI,
            flags: 0,
            build_commit: format!("{}-dirty", "a".repeat(40)),
            socket: "sqz-il0-123".into(),
            socket_path: String::new(),
            nonce: [7u8; NONCE_LEN],
        };
        let bytes = blob.encode();
        assert_eq!(BootstrapBlob::decode(&bytes).unwrap(), blob);
        assert!(
            blob.build_commit.len() < BUILD_COMMIT_LEN,
            "the 40-hex + -dirty form must fit the fixed width"
        );
        assert!(BootstrapBlob::decode(b"garbage").is_err());
        assert!(BootstrapBlob::decode(&[]).is_err());
        assert!(BootstrapBlob::decode(&bytes[..bytes.len() - 1]).is_err());
    }

    #[test]
    fn ctl_roundtrip_every_variant() {
        let msgs = [
            CtlMsg::Hello {
                abi: 1,
                pid: 42,
                uid: 1000,
                build_commit: "unknown".into(),
                nonce: [9u8; NONCE_LEN],
            },
            CtlMsg::SessionOk {
                geometry: Geometry::default_v1(),
            },
            CtlMsg::Refuse {
                class: RefuseClass::Version,
            },
            CtlMsg::Bind,
            CtlMsg::BindOk {
                binding_id: 7,
                ino: 99,
                read_ok: true,
                write_ok: false,
            },
            CtlMsg::BindRefused {
                class: RefuseClass::Flags,
            },
            CtlMsg::Refuse {
                class: RefuseClass::Disabled,
            },
            CtlMsg::Unbind { binding_id: 7 },
        ];
        for m in msgs {
            let bytes = m.encode();
            assert!(bytes.len() <= CTL_MSG_MAX);
            assert_eq!(CtlMsg::decode(&bytes).unwrap(), m, "roundtrip {m:?}");
        }
    }

    #[test]
    fn decoders_are_total_on_garbage() {
        for len in 0..64 {
            let junk: Vec<u8> = (0..len).map(|i| (i * 37 + 11) as u8).collect();
            let _ = CtlMsg::decode(&junk); // must never panic
            let _ = BootstrapBlob::decode(&junk);
        }
        assert_eq!(
            CtlMsg::decode(&999u32.to_le_bytes()),
            Err(WireError::UnknownTag(999))
        );
        assert_eq!(RefuseClass::from_u32(0), Err(WireError::BadClass(0)));
        assert_eq!(RefuseClass::from_u32(8), Ok(RefuseClass::Disabled));
        assert_eq!(RefuseClass::from_u32(9), Err(WireError::BadClass(9)));
    }

    #[test]
    fn degenerate_identities() {
        assert!(build_commit_degenerate("unknown"));
        assert!(build_commit_degenerate(&format!(
            "{}-dirty",
            "a".repeat(40)
        )));
        assert!(build_commit_degenerate(""));
        assert!(!build_commit_degenerate(&"a".repeat(40)));
    }
}
