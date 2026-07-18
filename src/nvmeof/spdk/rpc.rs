//! SPDK JSON-RPC 2.0 client **v2** (`docs/design-nvmeof-target-management.md`
//! §6.5/§6.9, PR 3/N3) — replaces the deleted hand-rolled single-shot
//! client (no timeout, no version negotiation, id=1 forever):
//!
//! * **Timeouts**: connect 5 s; per-call 10 s; 60 s for the named slow
//!   verbs (`save_config`/`load_config`/`bdev_aio_create` on slow media).
//! * **Monotonically increasing request ids** per client, echoed back and
//!   verified (a mismatched id is a protocol violation, never silently
//!   accepted).
//! * **Typed errors** (`SpdkRpcError { code, message, method }`) — no
//!   more `Error::other(format!…)`.
//! * **Version handshake**: `spdk_get_version` checked at `target start`
//!   and cached per-process for verb preflights; drift against the
//!   pinned tag is classified (minor/major) and gated per §6.2
//!   (`--accept-version-drift` on mutating verbs only; `target status`
//!   always reports; `target stop` warns-and-proceeds).
//! * **Socket** at `/run/squeezefs/nvmeof/spdk.sock` (dir 0700 root,
//!   socket 0600) — never `/var/tmp` (§Security).
//!
//! Framing: SPDK's unix-socket JSON-RPC carries no length prefix — a
//! response is one JSON value, possibly split across reads. The client
//! accumulates and incrementally parses exactly one value (the
//! `rpc.py` behavior), under the per-call deadline.
//!
//! Unit tier: `tests/nvmeof_rpc_tests.rs` speaks to an in-process fake
//! server on a **real `UnixListener`** (§6.8 zero-mock policy — framing,
//! timeouts and error mapping are tested for real; no env forks).

use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicU64;
use std::time::Duration;

/// Connect timeout (§6.5).
pub const RPC_CONNECT_TIMEOUT: Duration = Duration::from_secs(5);
/// Default per-call timeout (§6.5).
pub const RPC_CALL_TIMEOUT: Duration = Duration::from_secs(10);
/// Slow-verb per-call timeout (§6.5).
pub const RPC_SLOW_CALL_TIMEOUT: Duration = Duration::from_secs(60);
/// The named slow verbs (§6.5).
pub const RPC_SLOW_METHODS: &[&str] = &["save_config", "load_config", "bdev_aio_create"];

/// Typed SPDK RPC error (§6.5 — structured, never a formatted blob).
#[derive(Debug, thiserror::Error)]
pub enum SpdkRpcError {
    /// The socket does not answer (absent, refused, or hung during
    /// connect) — the "RPC dead" preflight class.
    #[error("RPC socket {socket} not answering ({detail})")]
    Connect { socket: PathBuf, detail: String },
    /// The per-call deadline fired after connect.
    #[error(
        "RPC call '{method}' timed out after {budget_ms} ms — the target is running but \
         unresponsive"
    )]
    Timeout { method: String, budget_ms: u128 },
    /// The server answered with a JSON-RPC error member.
    #[error("RPC '{method}' failed: {message} (code {code})")]
    Rpc {
        method: String,
        code: i64,
        message: String,
    },
    /// Framing / schema / id-echo violation.
    #[error("RPC '{method}': protocol violation: {detail}")]
    Protocol { method: String, detail: String },
    /// Transport I/O failure mid-call.
    #[error("RPC '{method}': I/O error: {source}")]
    Io {
        method: String,
        #[source]
        source: io::Error,
    },
}

/// The parsed `spdk_get_version` handshake result.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SpdkVersion {
    /// The raw reported string (e.g. `"SPDK v26.05"` / `"SPDK v26.09-pre"`).
    pub raw: String,
    pub major: u32,
    pub minor: u32,
}

/// Drift classification against the pinned tag (§6.5: any tag mismatch
/// is drift; a major mismatch is the louder class).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VersionDrift {
    Minor,
    Major,
}

impl SpdkVersion {
    /// Parse the `spdk_get_version` result object (prefers the numeric
    /// `fields`, falls back to parsing the `version` string).
    pub fn parse(result: &serde_json::Value) -> Result<SpdkVersion, String> {
        let _ = result;
        unimplemented!("N3 skeleton — implemented by the feat commit")
    }

    /// `None` when the reported version matches the pinned tag.
    pub fn drift(&self) -> Option<VersionDrift> {
        unimplemented!("N3 skeleton — implemented by the feat commit")
    }
}

/// The pinned `(major, minor)` parsed from
/// `lifecycle::SPDK_PINNED_TAG` (`"v26.05"` → `(26, 5)`).
pub fn pinned_major_minor() -> (u32, u32) {
    unimplemented!("N3 skeleton — implemented by the feat commit")
}

/// JSON-RPC 2.0 client over a unix socket. One connection per call
/// (one-shot CLI verbs); request ids increase monotonically across the
/// client's lifetime.
pub struct SpdkRpcClient {
    socket_path: PathBuf,
    next_id: AtomicU64,
    connect_timeout: Duration,
    call_timeout: Duration,
    slow_call_timeout: Duration,
}

impl SpdkRpcClient {
    /// Production timeouts (§6.5).
    pub fn new(socket_path: impl Into<PathBuf>) -> Self {
        Self::with_timeouts(
            socket_path,
            RPC_CONNECT_TIMEOUT,
            RPC_CALL_TIMEOUT,
            RPC_SLOW_CALL_TIMEOUT,
        )
    }

    /// Timeout injection seam for the unit tier (same code paths,
    /// shorter budgets).
    pub fn with_timeouts(
        socket_path: impl Into<PathBuf>,
        connect_timeout: Duration,
        call_timeout: Duration,
        slow_call_timeout: Duration,
    ) -> Self {
        SpdkRpcClient {
            socket_path: socket_path.into(),
            next_id: AtomicU64::new(1),
            connect_timeout,
            call_timeout,
            slow_call_timeout,
        }
    }

    pub fn socket_path(&self) -> &Path {
        &self.socket_path
    }

    /// One JSON-RPC call: connect → send → incremental-parse exactly one
    /// response value → verify the id echo → map `result`/`error`.
    pub fn call(
        &self,
        method: &str,
        params: Option<serde_json::Value>,
    ) -> Result<serde_json::Value, SpdkRpcError> {
        let _ = (method, params, &self.connect_timeout, &self.call_timeout);
        let _ = (&self.slow_call_timeout, &self.next_id);
        unimplemented!("N3 skeleton — implemented by the feat commit")
    }

    /// The version handshake (`spdk_get_version` → `SpdkVersion`).
    pub fn version(&self) -> Result<SpdkVersion, SpdkRpcError> {
        unimplemented!("N3 skeleton — implemented by the feat commit")
    }
}

/// Per-process version-handshake cache (§6.5: checked at `target start`,
/// cached for verb preflights) keyed by socket path.
pub fn handshake_cached(socket: &Path) -> Result<SpdkVersion, SpdkRpcError> {
    let _ = socket;
    unimplemented!("N3 skeleton — implemented by the feat commit")
}
