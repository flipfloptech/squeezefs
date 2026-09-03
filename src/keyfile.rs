//! Encryption key input, derivation, and mount-time resolution
//! (VAL-3 + KW-1 + VAL-7h — `docs/design-key-handling.md`).
//!
//! The key material NEVER rides `argv` and NEVER lands on the volume it
//! protects. `--encrypt-key` names a path (or `-` for stdin); the file is
//! opened `O_NOFOLLOW` and validated on the fd; only a KDF salt and an
//! 8-byte key id are persisted in [`crate::FormatConfig`]; and the actual
//! key resolves at mount from the flag, the
//! [`KEY_FILE_ENV`] path, or `/etc/squeezefs/keys/<key_id>.key`.
//!
//! The wrap primitive is HKDF-SHA-256 → the volume's own AEAD (`ring`),
//! not RSA-OAEP: execution-plan ruling D3 replaces `rsa` outright so
//! RUSTSEC-2023-0071 (Marvin) leaves the tree instead of being
//! adjudicated. See the design note §2 for why this and not X25519/HPKE.
//!
//! [`KEY_FILE_ENV`]: crate::keyfile::KEY_FILE_ENV

use ring::hkdf;
use ring::rand::{SecureRandom, SystemRandom};
use std::io::Read;
use std::os::unix::io::{FromRawFd, OwnedFd};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use zeroize::{Zeroize, Zeroizing};

/// Scheme id persisted in [`EncryptKeyRef::scheme`]: HKDF-SHA-256 KDF over
/// the operator's key material, AEAD key wrap under the derived KEK.
pub const KEY_WRAP_SCHEME_V2: &str = "hkdf-sha256/aead-kw/v2";

/// Mount-time key-file path (a PATH in the environment — never material).
pub const KEY_FILE_ENV: &str = "SQUEEZEFS_ENCRYPT_KEY_FILE";

/// Last resort of the mount resolution order: `<KEY_DIR>/<key_id>.key`.
const KEY_DIR: &str = "/etc/squeezefs/keys";

/// Minimum key-file material. HKDF has no work factor by design (§3): the
/// file must hold KEY MATERIAL, not a passphrase — `head -c 32
/// /dev/urandom | base64 > key` is the documented generator.
pub const MIN_KEY_MATERIAL_BYTES: usize = 32;

/// Key files are small by construction; the cap stops `--encrypt-key
/// /dev/zero`-class mistakes from being slurped into locked-down memory.
const MAX_KEY_FILE_BYTES: u64 = 64 * 1024;

/// HKDF salt width (RFC 5869 recommends the hash length).
const KDF_SALT_LEN: usize = 32;

/// Derived key-encryption-key width (both AEADs take 256-bit keys).
pub(crate) const KEK_LEN: usize = 32;

/// Derived key-identity width: enough to name a key file, far too short
/// to be a key.
const KEY_ID_LEN: usize = 8;

const INFO_KEK: &[u8] = b"squeezefs:key-wrap:v2:kek";
const INFO_KEY_ID: &[u8] = b"squeezefs:key-wrap:v2:key-id";

// ---------------------------------------------------------------------------
// Redaction / zeroization primitives
// ---------------------------------------------------------------------------

/// A string that may hold key material: zeroized on drop, redacted in
/// `Debug`, and (as a `FormatConfig` field) never serialized.
#[derive(Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(transparent)]
pub struct RedactedSecret(String);

impl RedactedSecret {
    pub fn new(s: String) -> Self {
        Self(s)
    }
}

impl std::fmt::Debug for RedactedSecret {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("<redacted>")
    }
}

impl Drop for RedactedSecret {
    fn drop(&mut self) {
        self.0.zeroize();
    }
}

/// Key material read from a file or stdin. Zeroized on drop; `Debug`
/// redacts.
#[derive(Clone)]
pub struct KeyMaterial(Zeroizing<Vec<u8>>);

impl KeyMaterial {
    /// Trim ASCII whitespace (a key file written with `echo` must derive
    /// the same key as one written without the newline) and enforce the
    /// minimum entropy budget.
    pub fn from_bytes(bytes: Vec<u8>) -> Result<Self, String> {
        let z = Zeroizing::new(bytes);
        let trimmed: &[u8] = {
            let s = z.as_slice();
            let start = s.iter().position(|b| !b.is_ascii_whitespace());
            match start {
                None => &[],
                Some(start) => {
                    let end = s.iter().rposition(|b| !b.is_ascii_whitespace()).unwrap();
                    &s[start..=end]
                }
            }
        };
        if trimmed.len() < MIN_KEY_MATERIAL_BYTES {
            return Err(format!(
                "encryption key material is {} B after trimming; at least \
                 {MIN_KEY_MATERIAL_BYTES} B are required. The key file must hold KEY \
                 MATERIAL, not a passphrase — generate one with \
                 `head -c 32 /dev/urandom | base64 > key && chmod 600 key`",
                trimmed.len()
            ));
        }
        Ok(Self(Zeroizing::new(trimmed.to_vec())))
    }

    pub fn as_bytes(&self) -> &[u8] {
        &self.0
    }
}

impl std::fmt::Debug for KeyMaterial {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "KeyMaterial({} B, <redacted>)", self.0.len())
    }
}

/// The per-volume key derived from the operator's material and the
/// volume's salt: a 256-bit KEK plus the public 64-bit identity that names
/// which key file a volume expects.
#[derive(Clone)]
pub struct VolumeKey {
    kek: Zeroizing<[u8; KEK_LEN]>,
    key_id: [u8; KEY_ID_LEN],
}

impl VolumeKey {
    pub(crate) fn kek(&self) -> &[u8; KEK_LEN] {
        &self.kek
    }

    pub(crate) fn key_id(&self) -> &[u8; KEY_ID_LEN] {
        &self.key_id
    }

    /// The public identity persisted in [`EncryptKeyRef::key_id`].
    pub fn key_id_hex(&self) -> String {
        hex(&self.key_id)
    }
}

impl std::fmt::Debug for VolumeKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "VolumeKey {{ key_id: {}, kek: <redacted> }}",
            self.key_id_hex()
        )
    }
}

// ---------------------------------------------------------------------------
// The durable (non-secret) key reference
// ---------------------------------------------------------------------------

/// Everything about the encryption key that is safe to persist on the
/// volume it protects: the KDF salt (public by RFC 5869 construction) and
/// the key id (an 8-byte HKDF output naming the expected key file).
#[derive(serde::Serialize, serde::Deserialize, Clone, Debug, PartialEq, Eq)]
pub struct EncryptKeyRef {
    /// [`KEY_WRAP_SCHEME_V2`]. Forward-only: an unknown scheme refuses.
    pub scheme: String,
    /// 32 bytes, hex.
    pub kdf_salt: String,
    /// 8 bytes, hex.
    pub key_id: String,
}

fn hex(bytes: &[u8]) -> String {
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        use std::fmt::Write;
        let _ = write!(s, "{b:02x}");
    }
    s
}

/// Parse the persisted salt hex back into bytes.
pub fn salt_from_hex(s: &str) -> Result<[u8; KDF_SALT_LEN], String> {
    if s.len() != KDF_SALT_LEN * 2 {
        return Err(format!(
            "malformed KDF salt in the format config: {} hex chars, expected {}",
            s.len(),
            KDF_SALT_LEN * 2
        ));
    }
    let mut out = [0u8; KDF_SALT_LEN];
    for (i, b) in out.iter_mut().enumerate() {
        *b = u8::from_str_radix(&s[i * 2..i * 2 + 2], 16)
            .map_err(|_| "malformed KDF salt in the format config (not hex)".to_string())?;
    }
    Ok(out)
}

/// A fresh per-volume KDF salt (one per `format` invocation).
pub fn new_kdf_salt() -> [u8; KDF_SALT_LEN] {
    let mut salt = [0u8; KDF_SALT_LEN];
    // A salt that failed to randomize would silently weaken every
    // derivation on the volume — fail the caller instead by panicking
    // here is wrong (library code), so fall back to a second source and
    // mix the clock. `SystemRandom` failing is a kernel-level condition.
    if SystemRandom::new().fill(&mut salt).is_err() {
        for chunk in salt.chunks_mut(8) {
            let r = fastrand::u64(..).to_le_bytes();
            chunk.copy_from_slice(&r[..chunk.len()]);
        }
    }
    salt
}

/// HKDF-Extract(salt, material) → Expand to the KEK and the key id.
pub fn derive_volume_key(material: &KeyMaterial, salt: &[u8; KDF_SALT_LEN]) -> VolumeKey {
    struct Len(usize);
    impl hkdf::KeyType for Len {
        fn len(&self) -> usize {
            self.0
        }
    }
    let prk = hkdf::Salt::new(hkdf::HKDF_SHA256, salt).extract(material.as_bytes());
    let mut kek = Zeroizing::new([0u8; KEK_LEN]);
    // `expand` only fails on an output length beyond 255*hash_len.
    prk.expand(&[INFO_KEK], Len(KEK_LEN))
        .expect("HKDF expand of 32 B never fails")
        .fill(kek.as_mut())
        .expect("HKDF fill of 32 B never fails");
    let mut key_id = [0u8; KEY_ID_LEN];
    prk.expand(&[INFO_KEY_ID], Len(KEY_ID_LEN))
        .expect("HKDF expand of 8 B never fails")
        .fill(&mut key_id)
        .expect("HKDF fill of 8 B never fails");
    VolumeKey { kek, key_id }
}

/// The durable reference `format` persists for a derived key.
pub fn make_key_ref(salt: &[u8; KDF_SALT_LEN], key: &VolumeKey) -> EncryptKeyRef {
    EncryptKeyRef {
        scheme: KEY_WRAP_SCHEME_V2.to_string(),
        kdf_salt: hex(salt),
        key_id: key.key_id_hex(),
    }
}

// ---------------------------------------------------------------------------
// VAL-7h — process hygiene
// ---------------------------------------------------------------------------

static HARDEN_LOGGED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// Make this process undumpable before it holds key material: no core
/// dump, no same-uid `ptrace`, no `/proc/<pid>/mem`. Deliberately scoped —
/// it runs only on paths that actually touch a key (`format
/// --encrypt-key`, an encrypted mount), never on plaintext mounts.
///
/// The syscalls are re-issued on every call rather than latched: they cost
/// two syscalls, and a latch would silently skip re-hardening a forked
/// child whose flag was cleared. Only the log line is once-per-process.
pub fn harden_process_memory() {
    let first = !HARDEN_LOGGED.swap(true, std::sync::atomic::Ordering::SeqCst);
    // SAFETY: both calls are process-scoped prctl/setrlimit with constant
    // arguments; neither dereferences caller memory.
    unsafe {
        if libc::prctl(libc::PR_SET_DUMPABLE, 0, 0, 0, 0) != 0 {
            log::warn!(
                "PR_SET_DUMPABLE(0) failed ({}) — a core dump of this daemon could \
                 spill encryption key material",
                std::io::Error::last_os_error()
            );
        }
        let rl = libc::rlimit {
            rlim_cur: 0,
            rlim_max: 0,
        };
        if libc::setrlimit(libc::RLIMIT_CORE, &rl) != 0 {
            log::warn!(
                "RLIMIT_CORE=0 failed ({}) — a core dump of this daemon could spill \
                 encryption key material",
                std::io::Error::last_os_error()
            );
        }
    }
    if first {
        log::info!("encryption key material in play: process is undumpable, core dumps disabled");
    }
}

// ---------------------------------------------------------------------------
// Key input
// ---------------------------------------------------------------------------

/// `-` reads stdin; anything else is a path.
pub fn read_key_source(spec: &str) -> Result<KeyMaterial, String> {
    if spec == "-" {
        let mut stdin = std::io::stdin();
        read_key_reader(&mut stdin, "stdin")
    } else {
        read_key_file(Path::new(spec))
    }
}

/// Read key material from an already-open stream (stdin, or a test
/// cursor). Hardens the process first.
pub fn read_key_reader<R: Read>(src: &mut R, what: &str) -> Result<KeyMaterial, String> {
    harden_process_memory();
    let mut buf = Zeroizing::new(Vec::new());
    src.take(MAX_KEY_FILE_BYTES + 1)
        .read_to_end(&mut buf)
        .map_err(|e| format!("cannot read encryption key material from {what}: {e}"))?;
    if buf.len() as u64 > MAX_KEY_FILE_BYTES {
        return Err(format!(
            "encryption key material from {what} exceeds {MAX_KEY_FILE_BYTES} B — \
             that is not a key file"
        ));
    }
    KeyMaterial::from_bytes(buf.to_vec())
}

/// Read key material from `path`, refusing anything an operator would not
/// want to hand a key to. Opened `O_NOFOLLOW | O_CLOEXEC` and validated on
/// the **fd**, so there is no TOCTOU window between the checks and the
/// read.
pub fn read_key_file(path: &Path) -> Result<KeyMaterial, String> {
    harden_process_memory();
    let display = path.display();
    let c_path = std::ffi::CString::new(path.as_os_str().as_encoded_bytes())
        .map_err(|_| format!("encryption key path {display} contains a NUL byte"))?;
    // SAFETY: `c_path` is a valid NUL-terminated string for the duration
    // of the call; the returned fd is immediately adopted by `OwnedFd`.
    let fd = unsafe {
        libc::open(
            c_path.as_ptr(),
            libc::O_RDONLY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
        )
    };
    if fd < 0 {
        let err = std::io::Error::last_os_error();
        if err.raw_os_error() == Some(libc::ELOOP) {
            return Err(format!(
                "encryption key file {display} is a symlink — refusing to follow it \
                 (O_NOFOLLOW). Pass the real path"
            ));
        }
        return Err(format!("cannot open encryption key file {display}: {err}"));
    }
    // SAFETY: `fd` is a fresh, owned descriptor from `open` above.
    let owned = unsafe { OwnedFd::from_raw_fd(fd) };

    let mut st: libc::stat = unsafe { std::mem::zeroed() };
    // SAFETY: `owned` is a live descriptor; `st` is a valid out-param.
    if unsafe { libc::fstat(fd, &mut st) } != 0 {
        return Err(format!(
            "cannot stat encryption key file {display}: {}",
            std::io::Error::last_os_error()
        ));
    }
    if st.st_mode & libc::S_IFMT != libc::S_IFREG {
        return Err(format!(
            "encryption key file {display} is not a regular file — refusing"
        ));
    }
    let mode = st.st_mode & 0o7777;
    if mode & 0o077 != 0 {
        return Err(format!(
            "encryption key file {display} is mode {mode:o}: readable or writable by \
             group/other. Refusing to load a key from it — `chmod 600 {display}`"
        ));
    }
    // SAFETY: geteuid is always safe.
    let euid = unsafe { libc::geteuid() };
    if euid != 0 && st.st_uid != euid {
        return Err(format!(
            "encryption key file {display} is owned by uid {} but this process runs as \
             uid {euid} — refusing (`chown {euid} {display}`)",
            st.st_uid
        ));
    }
    if st.st_size as u64 > MAX_KEY_FILE_BYTES {
        return Err(format!(
            "encryption key file {display} is {} B (> {MAX_KEY_FILE_BYTES} B) — that is \
             not a key file",
            st.st_size
        ));
    }

    let mut f = std::fs::File::from(owned);
    let mut buf = Zeroizing::new(Vec::with_capacity(st.st_size.max(0) as usize));
    f.read_to_end(&mut buf)
        .map_err(|e| format!("cannot read encryption key file {display}: {e}"))?;
    KeyMaterial::from_bytes(buf.to_vec())
        .map_err(|e| format!("{e} (from encryption key file {display})"))
}

// ---------------------------------------------------------------------------
// Mount-time resolution
// ---------------------------------------------------------------------------

/// Material handed in on the command line (`mount --encrypt-key`), read in
/// the PARENT before `--daemon` forks so `-` (stdin) works for daemonized
/// mounts. Fork copies it into the child; the parent clears its copy.
static STASHED: Mutex<Option<KeyMaterial>> = Mutex::new(None);

pub fn stash_key_material(m: KeyMaterial) {
    *STASHED.lock().unwrap_or_else(|e| e.into_inner()) = Some(m);
}

pub fn clear_key_material() {
    *STASHED.lock().unwrap_or_else(|e| e.into_inner()) = None;
}

fn stashed() -> Option<KeyMaterial> {
    STASHED.lock().unwrap_or_else(|e| e.into_inner()).clone()
}

/// `<KEY_DIR>/<key_id>.key` — the zero-configuration rung of the
/// resolution order.
pub fn default_key_path(key_id: &str) -> PathBuf {
    PathBuf::from(KEY_DIR).join(format!("{key_id}.key"))
}

/// Resolve the volume key for `key_ref`: `--encrypt-key` material, then
/// [`KEY_FILE_ENV`], then [`default_key_path`]. Verifies the derived key
/// id against the volume's, so a wrong key file is a loud refusal at mount
/// instead of an AEAD failure on every read.
pub fn resolve_volume_key(key_ref: &EncryptKeyRef) -> Result<VolumeKey, String> {
    if key_ref.scheme != KEY_WRAP_SCHEME_V2 {
        return Err(format!(
            "this volume declares encryption key-wrap scheme '{}', which this binary does \
             not implement (expected '{KEY_WRAP_SCHEME_V2}') — upgrade squeezefs",
            key_ref.scheme
        ));
    }
    let salt = salt_from_hex(&key_ref.kdf_salt)?;
    let env_path = std::env::var_os(KEY_FILE_ENV).map(PathBuf::from);
    let default_path = default_key_path(&key_ref.key_id);

    let (material, source) = if let Some(m) = stashed() {
        (m, "--encrypt-key".to_string())
    } else if let Some(p) = env_path {
        let src = format!("{KEY_FILE_ENV}={}", p.display());
        (read_key_file(&p).map_err(|e| format!("{src}: {e}"))?, src)
    } else if default_path.exists() {
        let src = default_path.display().to_string();
        (
            read_key_file(&default_path).map_err(|e| format!("{src}: {e}"))?,
            src,
        )
    } else {
        return Err(format!(
            "this volume is encrypted (key id {}) but no key source was found. Provide \
             the key file one of three ways: `--encrypt-key <path>` on the mount, \
             `{KEY_FILE_ENV}=<path>`, or place it at {}",
            key_ref.key_id,
            default_path.display()
        ));
    };

    let key = derive_volume_key(&material, &salt);
    if key.key_id_hex() != key_ref.key_id {
        return Err(format!(
            "the encryption key from {source} derives key id {} but this volume expects \
             {} — wrong key file. Refusing to mount (mounting with the wrong key would \
             fail every read)",
            key.key_id_hex(),
            key_ref.key_id
        ));
    }
    Ok(key)
}

/// The pre-KW-1 (RSA key-wrap) refusal — the stated compatibility posture
/// (`docs/design-key-handling.md` §6). An encrypted volume with no
/// [`EncryptKeyRef`] was written by a binary whose wrap primitive is gone
/// from this tree; refuse the mount with the exact remedy rather than fail
/// every read. Plaintext volumes are never affected.
pub fn legacy_encrypted_volume_refusal(cfg: &crate::FormatConfig) -> Option<String> {
    let encrypted = matches!(
        crate::crypto_compress::EncryptMode::parse(&cfg.encrypt_algo),
        Ok(mode) if mode != crate::crypto_compress::EncryptMode::None
    );
    if !encrypted || cfg.encrypt_key_ref.is_some() {
        return None;
    }
    let compromised = if cfg.encrypt_key.is_some() {
        "\n  NOTE: this volume's format config carried the key material itself, in \
         CLEARTEXT, on the volume it encrypts (the defect VAL-3 names). Treat that key \
         — and everything it ever protected — as compromised; do not reuse it."
    } else {
        ""
    };
    Some(format!(
        "refusing to mount: this volume was formatted by a pre-KW-1 binary \
         (encrypt_algo = \"{algo}\", no key reference in the format config). Its blocks \
         are wrapped with RSA-OAEP, whose implementation (rsa 0.9.x, RUSTSEC-2023-0071 \
         \"Marvin\" — no fixed release exists) has been REMOVED from squeezefs, so this \
         binary cannot unwrap them. Refusing the mount instead of failing every read.\
         \n  Remedy (forward-only, the same law as the v2 metadata format):\
         \n    1. copy the data off with a squeezefs binary at or before commit 7d1ec2e1, \
         or restore from backup;\
         \n    2. reformat with this binary:\
         \n         squeezefs format --force --encrypt-algo aes256gcm \\\
         \n           --encrypt-key /path/to/keyfile sqmeta://<meta> sqdata://<data>\
         \n       (the key file stays OFF the volume — only a KDF salt and a key id are \
         persisted);\
         \n    3. copy the data back.\
         \n  Volumes formatted the DOCUMENTED way (`--encrypt-key <path>`) never accepted \
         a single write on a pre-KW-1 binary — the path was parsed as PEM content and \
         every write failed — so reformatting them loses nothing.{compromised}",
        algo = cfg.encrypt_algo
    ))
}

/// The one entry point mount uses: `Ok(None)` for a plaintext volume,
/// `Ok(Some(key))` for an encrypted one, `Err(loud message)` otherwise.
pub fn mount_volume_key(cfg: &crate::FormatConfig) -> Result<Option<VolumeKey>, String> {
    if let Some(msg) = legacy_encrypted_volume_refusal(cfg) {
        return Err(msg);
    }
    let mode = crate::crypto_compress::EncryptMode::parse(&cfg.encrypt_algo)
        .map_err(|e| format!("format config declares an unusable encryption algorithm: {e}"))?;
    if mode == crate::crypto_compress::EncryptMode::None {
        if stashed().is_some() {
            return Err(
                "--encrypt-key was given but this volume is not encrypted (encrypt_algo = \
                 \"none\"). Refusing rather than mounting a plaintext volume as if a key \
                 mattered — re-run without the flag, or format an encrypted volume"
                    .to_string(),
            );
        }
        return Ok(None);
    }
    let key_ref = cfg
        .encrypt_key_ref
        .as_ref()
        .expect("legacy_encrypted_volume_refusal covers the None case");
    resolve_volume_key(key_ref).map(Some)
}
