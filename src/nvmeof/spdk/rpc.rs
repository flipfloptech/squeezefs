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

use std::collections::HashMap;
use std::io::{self, Read, Write};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

use serde_json::{json, Value};

use super::lifecycle::SPDK_PINNED_TAG;

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

/// Parse `major.minor` out of a version spelling (`"v26.05"`,
/// `"SPDK v26.09-pre"`, `"26.05.1"`).
fn parse_major_minor(s: &str) -> Option<(u32, u32)> {
    let start = s.find(|c: char| c.is_ascii_digit())?;
    let tail = &s[start..];
    let mut parts = tail.split(['.', '-', ' ']);
    let major: u32 = parts.next()?.parse().ok()?;
    let minor_raw = parts.next()?;
    let minor_digits: String = minor_raw
        .chars()
        .take_while(|c| c.is_ascii_digit())
        .collect();
    let minor: u32 = minor_digits.parse().ok()?;
    Some((major, minor))
}

impl SpdkVersion {
    /// Parse the `spdk_get_version` result object (prefers the numeric
    /// `fields`, falls back to parsing the `version` string).
    pub fn parse(result: &Value) -> Result<SpdkVersion, String> {
        let raw = result
            .get("version")
            .and_then(Value::as_str)
            .map(str::to_string);
        if let Some(fields) = result.get("fields") {
            if let (Some(major), Some(minor)) = (
                fields.get("major").and_then(Value::as_u64),
                fields.get("minor").and_then(Value::as_u64),
            ) {
                return Ok(SpdkVersion {
                    raw: raw.unwrap_or_else(|| format!("SPDK v{major}.{minor:02}")),
                    major: major as u32,
                    minor: minor as u32,
                });
            }
        }
        let raw = raw.ok_or_else(|| {
            format!("spdk_get_version result carries neither parseable 'fields' nor a 'version' string: {result}")
        })?;
        let (major, minor) = parse_major_minor(&raw)
            .ok_or_else(|| format!("cannot parse major.minor out of version string '{raw}'"))?;
        Ok(SpdkVersion { raw, major, minor })
    }

    /// `None` when the reported version matches the pinned tag.
    pub fn drift(&self) -> Option<VersionDrift> {
        let (pin_major, pin_minor) = pinned_major_minor();
        if self.major != pin_major {
            Some(VersionDrift::Major)
        } else if self.minor != pin_minor {
            Some(VersionDrift::Minor)
        } else {
            None
        }
    }
}

/// The pinned `(major, minor)` parsed from
/// `lifecycle::SPDK_PINNED_TAG` (`"v26.05"` → `(26, 5)`).
pub fn pinned_major_minor() -> (u32, u32) {
    parse_major_minor(SPDK_PINNED_TAG)
        .unwrap_or_else(|| unreachable!("SPDK_PINNED_TAG '{SPDK_PINNED_TAG}' is vMAJOR.MINOR"))
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

    fn budget_for(&self, method: &str) -> Duration {
        if RPC_SLOW_METHODS.contains(&method) {
            self.slow_call_timeout
        } else {
            self.call_timeout
        }
    }

    /// One JSON-RPC call: connect → send → incremental-parse exactly one
    /// response value → verify the id echo → map `result`/`error`.
    pub fn call(&self, method: &str, params: Option<Value>) -> Result<Value, SpdkRpcError> {
        let budget = self.budget_for(method);
        let deadline = Instant::now() + budget;
        let timeout_err = || SpdkRpcError::Timeout {
            method: method.to_string(),
            budget_ms: budget.as_millis(),
        };

        // Connect. AF_UNIX connects resolve immediately (accepted or
        // refused); the connect timeout bounds the pathological
        // full-backlog case via the socket timeouts set right after.
        let mut stream =
            UnixStream::connect(&self.socket_path).map_err(|e| SpdkRpcError::Connect {
                socket: self.socket_path.clone(),
                detail: match e.kind() {
                    io::ErrorKind::NotFound => "socket file absent — target not running".into(),
                    io::ErrorKind::ConnectionRefused => {
                        "connection refused — stale socket, no listener".into()
                    }
                    _ => e.to_string(),
                },
            })?;
        stream
            .set_write_timeout(Some(self.connect_timeout))
            .map_err(|e| SpdkRpcError::Io {
                method: method.to_string(),
                source: e,
            })?;

        // Send the request.
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let mut request = json!({
            "jsonrpc": "2.0",
            "method": method,
            "id": id,
        });
        if let Some(params) = params {
            request["params"] = params;
        }
        stream
            .write_all(request.to_string().as_bytes())
            .and_then(|()| stream.flush())
            .map_err(|e| SpdkRpcError::Io {
                method: method.to_string(),
                source: e,
            })?;

        // Read exactly one JSON value under the deadline.
        let mut buf: Vec<u8> = Vec::with_capacity(4096);
        let mut chunk = [0u8; 8192];
        let response: Value = loop {
            match serde_json::Deserializer::from_slice(&buf)
                .into_iter::<Value>()
                .next()
            {
                Some(Ok(v)) => break v,
                Some(Err(e)) if e.is_eof() => {}
                None => {}
                Some(Err(e)) => {
                    return Err(SpdkRpcError::Protocol {
                        method: method.to_string(),
                        detail: format!("response is not valid JSON: {e}"),
                    });
                }
            }
            let remaining = deadline
                .checked_duration_since(Instant::now())
                .ok_or_else(timeout_err)?;
            stream
                .set_read_timeout(Some(remaining))
                .map_err(|e| SpdkRpcError::Io {
                    method: method.to_string(),
                    source: e,
                })?;
            match stream.read(&mut chunk) {
                Ok(0) => {
                    return Err(SpdkRpcError::Protocol {
                        method: method.to_string(),
                        detail: "connection closed before a complete response".to_string(),
                    });
                }
                Ok(n) => buf.extend_from_slice(&chunk[..n]),
                Err(e)
                    if e.kind() == io::ErrorKind::WouldBlock
                        || e.kind() == io::ErrorKind::TimedOut =>
                {
                    return Err(timeout_err());
                }
                Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
                Err(e) => {
                    return Err(SpdkRpcError::Io {
                        method: method.to_string(),
                        source: e,
                    });
                }
            }
        };

        // Verify the id echo — never accept another request's answer.
        let echoed = response.get("id").and_then(Value::as_u64);
        if echoed != Some(id) {
            return Err(SpdkRpcError::Protocol {
                method: method.to_string(),
                detail: format!(
                    "response id {} does not echo request id {id}",
                    echoed.map_or("<absent>".to_string(), |v| v.to_string())
                ),
            });
        }

        // Map result/error members.
        if let Some(err) = response.get("error") {
            return Err(SpdkRpcError::Rpc {
                method: method.to_string(),
                code: err.get("code").and_then(Value::as_i64).unwrap_or(0),
                message: err
                    .get("message")
                    .and_then(Value::as_str)
                    .unwrap_or("<no message>")
                    .to_string(),
            });
        }
        match response.get("result") {
            Some(result) => Ok(result.clone()),
            None => Err(SpdkRpcError::Protocol {
                method: method.to_string(),
                detail: "response carries neither 'result' nor 'error'".to_string(),
            }),
        }
    }

    /// The version handshake (`spdk_get_version` → `SpdkVersion`).
    pub fn version(&self) -> Result<SpdkVersion, SpdkRpcError> {
        let result = self.call("spdk_get_version", None)?;
        SpdkVersion::parse(&result).map_err(|detail| SpdkRpcError::Protocol {
            method: "spdk_get_version".to_string(),
            detail,
        })
    }
}

/// Per-process version-handshake cache (§6.5: checked at `target start`,
/// cached for verb preflights) keyed by socket path. Only successful
/// handshakes are cached — a dead target is re-probed every time.
pub fn handshake_cached(socket: &Path) -> Result<SpdkVersion, SpdkRpcError> {
    static CACHE: OnceLock<Mutex<HashMap<PathBuf, SpdkVersion>>> = OnceLock::new();
    let cache = CACHE.get_or_init(|| Mutex::new(HashMap::new()));
    if let Some(v) = cache
        .lock()
        .expect("version cache poisoned")
        .get(socket)
        .cloned()
    {
        return Ok(v);
    }
    let version = SpdkRpcClient::new(socket).version()?;
    cache
        .lock()
        .expect("version cache poisoned")
        .insert(socket.to_path_buf(), version.clone());
    Ok(version)
}
