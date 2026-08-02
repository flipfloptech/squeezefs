use crate::cache::pool::{BufferPool, POOLED_BUF_ALIGN};
use crate::error::SqueezefsError;
use crate::keyfile::VolumeKey;
use ring::aead::{LessSafeKey, Nonce, UnboundKey, AES_256_GCM, CHACHA20_POLY1305};
use ring::rand::{SecureRandom, SystemRandom};
use std::sync::Arc;
use zeroize::{Zeroize, Zeroizing};

/// Self-delimiting transform frame: every non-passthrough `process_write`
/// image is prefixed `[u32 LE word]` where `word = image_len |
/// FRAME_RAW_FLAG?`. Block reads return the full device window — stored
/// image + trailing device bytes — and neither decoder tolerates the
/// padding (lz4's `decompress_size_prepended` rejects trailing bytes with
/// `OffsetZero`; AEAD opens `data[header..]`, so padding lands inside the
/// tag check). Passthrough images carry NO frame (byte-identity is the
/// passthrough contract — §5.6 ranged reads depend on it). Forward-only:
/// unframed legacy blobs refuse loud in `process_read` (their cold device
/// reads never worked, so there is no behavior to preserve).
const FRAME_LEN_BYTES: usize = 4;

/// FIND-RW4-A store-raw escape marker (bit 31 of the frame word): the
/// image payload was stored RAW — compression was attempted and did not
/// shrink the block (incompressible data expands under lz4/zstd), so the
/// raw payload was stored instead, bounding every stored image by
/// `max_stored_image_len`. Encryption, when configured, still applies
/// over the raw payload (the escape sits below the AEAD layer);
/// `process_read` dispatches on the marker, never guesses. Image lengths
/// are bounded by the block size (≤ 4 MiB class), so bit 31 was always 0
/// in pre-fix frames — the flagged decoder is a STRICT SUPERSET of the
/// pre-fix encoding.
const FRAME_RAW_FLAG: u32 = 1 << 31;

/// Length bits of the frame word (see [`FRAME_RAW_FLAG`]).
const FRAME_LEN_MASK: u32 = FRAME_RAW_FLAG - 1;

/// On-disk AEAD header prefix: `[2B wrapped_key_len][1B nonce_len]`.
const ENCRYPT_HEADER_PREFIX_LEN: usize = 3;

/// AEAD nonce length emitted by every writer (4-byte salt + 8-byte counter).
const NONCE_LEN: usize = 12;

/// §5.7: conservative wrapped-key bound for scratch sizing when no
/// prewrapped session-key blob exists at pool-init time. KW-1 wraps are
/// [`WRAP_V2_LEN`] = 63 B, but this bound also sizes the ON-DISK geometry
/// gate ([`CryptoCompressState::max_stored_image_len`], which decides the
/// format-time `block_size` clamp), so it stays at the retired RSA-4096
/// value this wave: shrinking it moves the clamp, i.e. it is an on-disk
/// change owed to the Phase-8 reformat window
/// (`docs/design-key-handling.md` §7).
const WRAPPED_KEY_LEN_FALLBACK: usize = 512;

/// KW-1 wrap-blob layout (`docs/design-key-handling.md` §2):
/// `"SK" ‖ version ‖ nonce(12) ‖ AEAD_seal(KEK, aad = key_id, data key)`.
const WRAP_MAGIC: [u8; 2] = *b"SK";
const WRAP_VERSION_V2: u8 = 2;
const WRAP_PREFIX_LEN: usize = 3;
/// The AEAD data key both supported record algorithms take.
const DATA_KEY_LEN: usize = 32;
/// Total wrap-blob length: prefix + nonce + ciphertext + tag.
const WRAP_V2_LEN: usize = WRAP_PREFIX_LEN + NONCE_LEN + DATA_KEY_LEN + AEAD_TAG_LEN_MAX;

/// Largest AEAD tag either supported algorithm emits (AES-256-GCM and
/// ChaCha20-Poly1305 are both 16 B) — the conservative term in
/// [`CryptoCompressState::max_stored_image_len`].
const AEAD_TAG_LEN_MAX: usize = 16;

/// FIND-RW4-A: per-block on-disk headroom a TRANSFORMED
/// (compressed/encrypted) volume must reserve inside each allocator chunk
/// so that a full `block_size` payload stored RAW (the incompressible-
/// block escape) still fits: frame word + worst-case AEAD envelope
/// (`[2B wrapped_key_len][1B nonce_len]` + the conservative
/// `WRAPPED_KEY_LEN_FALLBACK` wrap + nonce + tag = 547 B), rounded up to
/// one 4 KiB LBA so chunk-interior windows stay
/// O_DIRECT-aligned. `format` clamps `block_size` to
/// `CHUNK_SIZE - TRANSFORM_BLOCK_HEADROOM` on transformed volumes; mounts
/// refuse transformed volumes whose geometry cannot satisfy it (pre-fix
/// formats — reformat required).
pub const TRANSFORM_BLOCK_HEADROOM: u64 = 4096;

// The headroom must cover the worst-case non-payload bytes of a stored
// raw-escape image (frame + AEAD header + the conservative wrap bound +
// nonce + tag).
const _: () = assert!(
    TRANSFORM_BLOCK_HEADROOM as usize
        >= FRAME_LEN_BYTES
            + ENCRYPT_HEADER_PREFIX_LEN
            + WRAPPED_KEY_LEN_FALLBACK
            + NONCE_LEN
            + AEAD_TAG_LEN_MAX
);

/// Resolved compression mode (P2-3) — avoids string matching on every block.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CompressionMode {
    None,
    Lz4,
    Zstd,
}

/// Resolved encryption mode (P2-3). The record cipher; the key-wrap
/// scheme is named by [`crate::keyfile::EncryptKeyRef::scheme`], not here
/// (KW-1 — the `-rsa` spellings are deprecated aliases retained only so a
/// pre-KW-1 config still parses far enough to be refused precisely).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EncryptMode {
    None,
    Aes256Gcm,
    ChaCha20,
}

impl CompressionMode {
    pub fn parse(s: &str) -> Result<Self, SqueezefsError> {
        match s.trim() {
            "none" | "" => Ok(Self::None),
            "lz4" => Ok(Self::Lz4),
            "zstd" => Ok(Self::Zstd),
            other => Err(SqueezefsError::InvalidOperation(format!(
                "Unsupported compression algorithm: {other}"
            ))),
        }
    }
}

impl EncryptMode {
    pub fn parse(s: &str) -> Result<Self, SqueezefsError> {
        match s.trim() {
            "none" | "" => Ok(Self::None),
            // The `-rsa` spellings are the pre-KW-1 on-disk names: the
            // record cipher was always this AEAD; only the wrap changed.
            "aes256gcm" | "aes256gcm-rsa" => Ok(Self::Aes256Gcm),
            "chacha20" | "chacha20-rsa" => Ok(Self::ChaCha20),
            other => Err(SqueezefsError::InvalidOperation(format!(
                "Unsupported encryption algo: {other}"
            ))),
        }
    }

    /// The canonical spelling `format` persists (KW-1 — never a `-rsa`
    /// name on a volume this binary formats).
    pub fn as_str(self) -> &'static str {
        match self {
            Self::None => "none",
            Self::Aes256Gcm => "aes256gcm",
            Self::ChaCha20 => "chacha20",
        }
    }

    fn algorithm(self) -> Option<&'static ring::aead::Algorithm> {
        match self {
            Self::None => None,
            Self::Aes256Gcm => Some(&AES_256_GCM),
            Self::ChaCha20 => Some(&CHACHA20_POLY1305),
        }
    }
}

#[derive(Clone)]
pub struct CryptoCompressState {
    pub compression: String,
    pub encrypt_algo: String,
    /// Pre-parsed modes for the hot path (P2-3).
    pub compression_mode: CompressionMode,
    pub encrypt_mode: EncryptMode,
    /// KW-1: the key-encryption key derived at mount from the operator's
    /// key file ([`crate::keyfile`]). `None` on plaintext volumes and on
    /// ad-hoc states; an encrypted state without one refuses every
    /// transform loud (it must never silently store plaintext).
    kek: Option<Arc<WrapKey>>,
    pub unwrap_cache: moka::sync::Cache<Vec<u8>, Arc<LessSafeKey>>,
    pub key_unwrap_count: Arc<std::sync::atomic::AtomicUsize>,
    /// Session data key: the wrap blob every block header carries (shared)
    /// + the raw key bytes for the fallback paths (zeroized on drop).
    pub prewrapped_key: Option<(Arc<[u8]>, Zeroizing<[u8; DATA_KEY_LEN]>)>,
    pub precomputed_encrypt_key: Option<Arc<LessSafeKey>>,
    /// AEAD tag length for the configured algorithm (0 if encrypt is off).
    pub aead_tag_len: usize,
    pub nonce_counter: Arc<std::sync::atomic::AtomicU64>,
    pub salt: [u8; 4],
    /// §5.7 CRYPTO_SCRATCH_POOL: worst-case-sized transform scratch for the
    /// non-passthrough write path, shared across clones (one pool per
    /// mount's crypto state). `None` until [`Self::init_scratch_pool`] runs
    /// (`DataRouter::set_crypto` initializes it from the configured block
    /// size); uninitialized states stay on the heap `Vec` path.
    scratch_pool: Arc<once_cell::sync::OnceCell<Arc<BufferPool>>>,
    /// **DUR-8e**: the configured block size — the bound every declared
    /// plaintext length must respect before anything allocates from it.
    /// `0` until [`Self::init_scratch_pool`] records it (then
    /// `crate::default_block_size()` answers).
    max_plaintext: Arc<std::sync::atomic::AtomicUsize>,
}

/// The key-encryption key: the AEAD built over the KDF-derived KEK plus
/// the key id that binds every wrap blob as AAD. Never `Debug`-printable.
struct WrapKey {
    aead: LessSafeKey,
    key_id: [u8; 8],
}

/// VAL-3: nothing that holds key material may print it. A manual `Debug`
/// (rather than none) also stops a future `#[derive(Debug)]` from
/// reintroducing the leak.
impl std::fmt::Debug for CryptoCompressState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CryptoCompressState")
            .field("compression", &self.compression)
            .field("encrypt_algo", &self.encrypt_algo)
            .field("keyed", &self.kek.is_some())
            .field("key_id", &self.kek.as_ref().map(|k| hex8(&k.key_id)))
            .field("session_key", &"<redacted>")
            .finish()
    }
}

fn hex8(bytes: &[u8; 8]) -> String {
    use std::fmt::Write;
    let mut s = String::with_capacity(16);
    for b in bytes {
        let _ = write!(s, "{b:02x}");
    }
    s
}

/// KW-1 wrap: `"SK" ‖ 0x02 ‖ nonce ‖ AEAD_seal(KEK, nonce, aad = key_id,
/// data key)`. Free function so the constructor can wrap before `Self`
/// exists.
fn wrap_with(
    kek: Option<&WrapKey>,
    data_key: &[u8; DATA_KEY_LEN],
) -> Result<Vec<u8>, SqueezefsError> {
    let kek = kek.ok_or_else(|| {
        SqueezefsError::InvalidOperation(
            "no encryption key is configured for this volume: provide it with \
             `--encrypt-key <path>`, SQUEEZEFS_ENCRYPT_KEY_FILE, or \
             /etc/squeezefs/keys/<key_id>.key"
                .to_string(),
        )
    })?;
    let mut nonce_bytes = [0u8; NONCE_LEN];
    SystemRandom::new().fill(&mut nonce_bytes).map_err(|_| {
        SqueezefsError::InvalidOperation("failed to generate a key-wrap nonce".to_string())
    })?;
    let nonce = Nonce::assume_unique_for_key(nonce_bytes);
    let mut in_out = Vec::with_capacity(DATA_KEY_LEN + AEAD_TAG_LEN_MAX);
    in_out.extend_from_slice(data_key);
    kek.aead
        .seal_in_place_append_tag(nonce, ring::aead::Aad::from(kek.key_id), &mut in_out)
        .map_err(|_| SqueezefsError::InvalidOperation("key wrap failed".to_string()))?;
    let mut blob = Vec::with_capacity(WRAP_V2_LEN);
    blob.extend_from_slice(&WRAP_MAGIC);
    blob.push(WRAP_VERSION_V2);
    blob.extend_from_slice(&nonce_bytes);
    blob.extend_from_slice(&in_out);
    debug_assert_eq!(blob.len(), WRAP_V2_LEN, "the KW-1 wrap blob is fixed-width");
    Ok(blob)
}

impl CryptoCompressState {
    /// `volume_key` is the mount-resolved [`VolumeKey`]
    /// (`docs/design-key-handling.md` §4) — NEVER a PEM string, and never
    /// anything that came off `argv` or the volume.
    pub fn new(compression: String, encrypt_algo: String, volume_key: Option<&VolumeKey>) -> Self {
        let compression_mode =
            CompressionMode::parse(&compression).unwrap_or(CompressionMode::None);
        let encrypt_mode = EncryptMode::parse(&encrypt_algo).unwrap_or(EncryptMode::None);

        // The wrap rides the volume's OWN record AEAD (one primitive per
        // volume, KW-1 §2). Ad-hoc/plaintext states carry no KEK.
        let kek = match (volume_key, encrypt_mode.algorithm()) {
            (Some(vk), Some(algorithm)) => match UnboundKey::new(algorithm, vk.kek()) {
                Ok(unbound) => Some(Arc::new(WrapKey {
                    aead: LessSafeKey::new(unbound),
                    key_id: *vk.key_id(),
                })),
                Err(_) => {
                    log::error!("failed to build the key-wrap AEAD from the derived KEK");
                    None
                }
            },
            _ => None,
        };

        let mut prewrapped_key = None;
        if kek.is_some() {
            let mut key_bytes = Zeroizing::new([0u8; DATA_KEY_LEN]);
            if SystemRandom::new().fill(key_bytes.as_mut()).is_ok() {
                match wrap_with(kek.as_deref(), &key_bytes) {
                    Ok(wrapped) => {
                        let wrapped: Arc<[u8]> = Arc::from(wrapped.into_boxed_slice());
                        prewrapped_key = Some((wrapped, key_bytes));
                    }
                    Err(e) => log::error!("failed to wrap the session data key: {e}"),
                }
            }
        }

        let mut precomputed_encrypt_key = None;
        let mut aead_tag_len = 0usize;
        if let Some((_, ref key_bytes)) = prewrapped_key {
            if let Some(algorithm) = encrypt_mode.algorithm() {
                aead_tag_len = algorithm.tag_len();
                if let Ok(unbound_key) = UnboundKey::new(algorithm, key_bytes.as_ref()) {
                    precomputed_encrypt_key = Some(Arc::new(LessSafeKey::new(unbound_key)));
                }
            }
        } else if let Some(algorithm) = encrypt_mode.algorithm() {
            aead_tag_len = algorithm.tag_len();
        }

        let mut salt = [0u8; 4];
        let _ = SystemRandom::new().fill(&mut salt);
        let nonce_counter = Arc::new(std::sync::atomic::AtomicU64::new(0));

        let unwrap_cache = moka::sync::Cache::builder().max_capacity(10000).build();

        Self {
            compression,
            encrypt_algo,
            compression_mode,
            encrypt_mode,
            kek,
            unwrap_cache,
            key_unwrap_count: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
            prewrapped_key,
            precomputed_encrypt_key,
            aead_tag_len,
            nonce_counter,
            salt,
            scratch_pool: Arc::new(once_cell::sync::OnceCell::new()),
            max_plaintext: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
        }
    }

    /// §5.7 worst-case compressed size for `input_len` bytes, independent of
    /// the configured mode (the pool must hold either compressor's worst
    /// case). The lz4 term carries the 4-byte size-prefix framing
    /// `compress_prepend_size` emits.
    fn scratch_compress_term(input_len: usize) -> usize {
        std::cmp::max(
            4 + lz4_flex::block::get_maximum_output_size(input_len),
            zstd::zstd_safe::compress_bound(input_len),
        )
    }

    /// §5.7 on-disk AEAD overhead: `[2B wrapped_key_len][1B nonce_len]`
    /// header + the ACTUAL prewrapped session-key blob
    /// ([`WRAPPED_KEY_LEN_FALLBACK`] when none exists at pool-init time —
    /// a KW-1 wrap is [`WRAP_V2_LEN`]) + nonce + tag.
    fn scratch_encrypt_overhead(&self) -> usize {
        if self.encrypt_mode == EncryptMode::None {
            return 0;
        }
        let wrapped_len = self
            .prewrapped_key
            .as_ref()
            .map(|(w, _)| w.len())
            .unwrap_or(WRAPPED_KEY_LEN_FALLBACK);
        ENCRYPT_HEADER_PREFIX_LEN + wrapped_len + NONCE_LEN + self.aead_tag_len
    }

    /// Worst-case transform output for `input_len` bytes (§5.7) — the
    /// pooled path's fit check against the pool's buffer size. Includes
    /// the self-delimiting frame prefix. Deliberately sized for the
    /// compressors' EXPANDED intermediate output: the store-raw escape
    /// decides only after compression ran into the scratch.
    fn worst_case_scratch_len(&self, input_len: usize) -> usize {
        FRAME_LEN_BYTES + Self::scratch_compress_term(input_len) + self.scratch_encrypt_overhead()
    }

    /// FIND-RW4-A: the normative upper bound on a STORED image for a
    /// `payload_len`-byte payload. With the store-raw escape the
    /// compression term never exceeds the raw payload, so the bound is
    /// `frame + worst-case AEAD envelope + payload`. Deliberately
    /// CONSERVATIVE on the wrapped-key term (`WRAPPED_KEY_LEN_FALLBACK`,
    /// never the blob actually present in THIS process): readers size their
    /// device windows with it, and a window must cover any writer's
    /// output regardless of which process wrote the block. Passthrough
    /// states store byte-identical payloads (no frame).
    pub fn max_stored_image_len(&self, payload_len: usize) -> usize {
        if self.is_passthrough() {
            return payload_len;
        }
        let encrypt_overhead = if self.encrypt_mode == EncryptMode::None {
            0
        } else {
            ENCRYPT_HEADER_PREFIX_LEN + WRAPPED_KEY_LEN_FALLBACK + NONCE_LEN + AEAD_TAG_LEN_MAX
        };
        FRAME_LEN_BYTES + encrypt_overhead + payload_len
    }

    /// FIND-RW4-A mount/format geometry gate: a TRANSFORMED volume must be
    /// able to store the worst-case image of a full `block_size` payload
    /// inside one `chunk_size` allocator chunk — otherwise incompressible
    /// blocks either overflow the chunk (silent neighbor corruption) or
    /// cannot be stored at all. Every pre-fix compressed/encrypted format
    /// (`block_size == chunk_size`) fails this check; post-fix formats
    /// reserve [`TRANSFORM_BLOCK_HEADROOM`]. `Err` carries the operator
    /// message (forward-only: reformat is the remedy — no shim).
    pub fn transform_geometry_check(
        &self,
        block_size: u64,
        chunk_size: u64,
    ) -> std::result::Result<(), String> {
        if self.is_passthrough() {
            return Ok(());
        }
        let worst = self.max_stored_image_len(block_size as usize) as u64;
        if worst > chunk_size {
            return Err(format!(
                "compressed/encrypted volume geometry cannot hold incompressible blocks \
                 (FIND-RW4-A): worst-case stored image for a {block_size} B block is \
                 {worst} B > the {chunk_size} B allocator chunk. This volume was \
                 formatted before the incompressible-block fix; its full-size \
                 incompressible blocks were never readable. Reformat with a current \
                 binary (format now reserves {TRANSFORM_BLOCK_HEADROOM} B of per-chunk \
                 headroom on transformed volumes) — refusing to mount."
            ));
        }
        Ok(())
    }

    /// Initialize the §5.7 CRYPTO_SCRATCH_POOL for `block_size`-byte writes:
    /// buffers are `worst_case(block_size)` rounded up to the next 4 KiB
    /// (≈ `block_size` + 128 KiB for 4 MiB blocks — deliberately not
    /// `ALIGNED_BUF_POOL`, whose exactly-`block_size` buffers cannot hold
    /// worst-case transform output). Idempotent (first init wins); no-op for
    /// passthrough states, which never transform.
    pub fn init_scratch_pool(&self, block_size: usize) {
        // DUR-8e: record the plaintext bound even for passthrough states
        // (harmless there, and one less thing to get wrong later).
        self.max_plaintext
            .store(block_size, std::sync::atomic::Ordering::Relaxed);
        if self.is_passthrough() {
            return;
        }
        self.scratch_pool.get_or_init(|| {
            let buf_size = self
                .worst_case_scratch_len(block_size)
                .next_multiple_of(POOLED_BUF_ALIGN);
            let cores = std::thread::available_parallelism()
                .map(|n| n.get())
                .unwrap_or(4);
            // Small on purpose: the queue is FIFO, so a capacity far above
            // the concurrent-transform count rotates every handout through a
            // cache-cold worst-case buffer (measured +4..10% on the 4 MiB
            // lz4/aes micro-benches at cores*4 buffers). Pool-empty handouts
            // fall back to a fresh aligned allocation — exactly today's
            // per-transform cost, paid only by burst excess over this
            // steady-state hot set.
            let capacity = (cores / 4).clamp(4, 16);
            Arc::new(BufferPool::new(capacity, buf_size))
        });
    }

    /// §5.7 observability: buffer size of the initialized scratch pool
    /// (`None` = heap-path state).
    pub fn scratch_pool_buf_len(&self) -> Option<usize> {
        self.scratch_pool.get().map(|p| p.buf_size())
    }

    /// True when neither compression nor encryption is configured (zero-copy path).
    #[inline]
    pub fn is_passthrough(&self) -> bool {
        self.compression_mode == CompressionMode::None && self.encrypt_mode == EncryptMode::None
    }

    pub fn compress<'a>(
        &self,
        data: &'a [u8],
    ) -> Result<std::borrow::Cow<'a, [u8]>, SqueezefsError> {
        match self.compression_mode {
            CompressionMode::Lz4 => {
                let compressed = lz4_flex::compress_prepend_size(data);
                Ok(std::borrow::Cow::Owned(compressed))
            }
            CompressionMode::Zstd => {
                let compressed = zstd::encode_all(std::io::Cursor::new(data), 3).map_err(|e| {
                    SqueezefsError::InvalidOperation(format!("ZSTD compression failed: {:?}", e))
                })?;
                Ok(std::borrow::Cow::Owned(compressed))
            }
            CompressionMode::None => Ok(std::borrow::Cow::Borrowed(data)),
        }
    }

    /// **DUR-8e** — the largest plaintext a stored image may declare.
    /// One block is the product's maximum unit of stored plaintext (the
    /// inline/staged forms are strictly smaller), so anything above it is
    /// a corrupt or hostile length field, never a legitimate payload.
    /// Sourced from the configured block size, which
    /// [`Self::init_scratch_pool`] records at mount.
    fn max_plaintext_len(&self) -> usize {
        match self
            .max_plaintext
            .load(std::sync::atomic::Ordering::Relaxed)
        {
            0 => crate::routing::default_block_size() as usize,
            n => n,
        }
    }

    pub fn decompress<'a>(
        &self,
        data: &'a [u8],
    ) -> Result<std::borrow::Cow<'a, [u8]>, SqueezefsError> {
        let cap = self.max_plaintext_len();
        match self.compression_mode {
            CompressionMode::Lz4 => {
                // DUR-8e: `decompress_size_prepended` ALLOCATES from the
                // on-disk 4-byte length prefix. On a compression-only
                // volume nothing authenticates that field, so bound it
                // before the allocation, not after.
                if data.len() < 4 {
                    return Err(SqueezefsError::InvalidOperation(
                        "LZ4 image shorter than its size prefix".to_string(),
                    ));
                }
                let declared = u32::from_le_bytes([data[0], data[1], data[2], data[3]]) as usize;
                if declared > cap {
                    return Err(SqueezefsError::InvalidOperation(format!(
                        "LZ4 image declares a {declared} B plaintext, above the {cap} B \
                         block bound — corrupt or hostile length field"
                    )));
                }
                let decompressed = lz4_flex::decompress_size_prepended(data).map_err(|e| {
                    SqueezefsError::InvalidOperation(format!("LZ4 decompression failed: {:?}", e))
                })?;
                Ok(std::borrow::Cow::Owned(decompressed))
            }
            CompressionMode::Zstd => {
                // DUR-8e: zstd streams, so the bound rides the READER —
                // one byte past the cap and the image is refused, with
                // allocation bounded by construction.
                use std::io::Read;
                let mut dec = zstd::stream::read::Decoder::new(std::io::Cursor::new(data))
                    .map_err(|e| {
                        SqueezefsError::InvalidOperation(format!("ZSTD decoder init failed: {e:?}"))
                    })?;
                let mut out = Vec::new();
                let read = dec
                    .by_ref()
                    .take(cap as u64 + 1)
                    .read_to_end(&mut out)
                    .map_err(|e| {
                        SqueezefsError::InvalidOperation(format!(
                            "ZSTD decompression failed: {e:?}"
                        ))
                    })?;
                if read > cap {
                    return Err(SqueezefsError::InvalidOperation(format!(
                        "ZSTD image expands past the {cap} B block bound — corrupt or \
                         hostile payload"
                    )));
                }
                Ok(std::borrow::Cow::Owned(out))
            }
            CompressionMode::None => Ok(std::borrow::Cow::Borrowed(data)),
        }
    }

    /// KW-1: wrap a 32-byte session data key under this volume's KEK. The
    /// blob is what every block header carries; it is versioned and bound
    /// to the key id as AAD, so a blob from another volume fails the tag
    /// check instead of unwrapping to a wrong key.
    pub fn wrap_session_key(
        &self,
        data_key: &[u8; DATA_KEY_LEN],
    ) -> Result<Vec<u8>, SqueezefsError> {
        wrap_with(self.kek.as_deref(), data_key)
    }

    /// KW-1 inverse. Refuses an unknown version, a short blob, a foreign
    /// key and a tampered blob — never panics on attacker-shaped bytes.
    pub fn unwrap_session_key(
        &self,
        blob: &[u8],
    ) -> Result<Zeroizing<[u8; DATA_KEY_LEN]>, SqueezefsError> {
        let kek = self.kek.as_deref().ok_or_else(|| {
            SqueezefsError::InvalidOperation(
                "no encryption key is configured for this volume: provide it with \
                 `--encrypt-key <path>`, SQUEEZEFS_ENCRYPT_KEY_FILE, or \
                 /etc/squeezefs/keys/<key_id>.key"
                    .to_string(),
            )
        })?;
        if blob.len() < WRAP_PREFIX_LEN + NONCE_LEN || blob[..2] != WRAP_MAGIC {
            return Err(SqueezefsError::InvalidOperation(
                "malformed key-wrap blob in the block header".to_string(),
            ));
        }
        if blob[2] != WRAP_VERSION_V2 {
            return Err(SqueezefsError::InvalidOperation(format!(
                "unsupported key-wrap version {} in the block header (this binary \
                 implements version {WRAP_VERSION_V2})",
                blob[2]
            )));
        }
        let mut nonce_bytes = [0u8; NONCE_LEN];
        nonce_bytes.copy_from_slice(&blob[WRAP_PREFIX_LEN..WRAP_PREFIX_LEN + NONCE_LEN]);
        let mut in_out = blob[WRAP_PREFIX_LEN + NONCE_LEN..].to_vec();
        let plain = kek
            .aead
            .open_in_place(
                Nonce::assume_unique_for_key(nonce_bytes),
                ring::aead::Aad::from(kek.key_id),
                &mut in_out,
            )
            .map_err(|_| {
                SqueezefsError::InvalidOperation(
                    "key unwrap failed: this block was written under a different \
                     encryption key (or the header is corrupt)"
                        .to_string(),
                )
            })?;
        if plain.len() != DATA_KEY_LEN {
            in_out.zeroize();
            return Err(SqueezefsError::InvalidOperation(
                "key unwrap produced a data key of the wrong length".to_string(),
            ));
        }
        let mut out = Zeroizing::new([0u8; DATA_KEY_LEN]);
        out.copy_from_slice(plain);
        in_out.zeroize();
        Ok(out)
    }

    /// Resolve the wrapped data-key blob + AEAD key for a write —
    /// session-key fast path (P2-3: reuse the wrap blob + precomputed
    /// `LessSafeKey`) or the per-write wrap fallback. Shared by the heap
    /// `encrypt` and the §5.7 pooled scratch path; `pub` because it is
    /// also the microbenched cached-resolve fast path.
    pub fn resolve_encrypt_key(&self) -> Result<(Arc<[u8]>, Arc<LessSafeKey>), SqueezefsError> {
        if let Some(ref key) = self.precomputed_encrypt_key {
            let wrapped = self
                .prewrapped_key
                .as_ref()
                .map(|(w, _)| w.clone())
                .ok_or_else(|| {
                    SqueezefsError::InvalidOperation(
                        "precomputed encrypt key without prewrapped blob".to_string(),
                    )
                })?;
            return Ok((wrapped, key.clone()));
        }

        let (wrapped_key, key_bytes) = if let Some((ref wrapped, ref key)) = self.prewrapped_key {
            (wrapped.clone(), key.clone())
        } else {
            let mut key_bytes = Zeroizing::new([0u8; DATA_KEY_LEN]);
            SystemRandom::new().fill(key_bytes.as_mut()).map_err(|_| {
                SqueezefsError::InvalidOperation("Failed to generate random data key".to_string())
            })?;
            let wrapped_key = self.wrap_session_key(&key_bytes)?;
            (Arc::from(wrapped_key.into_boxed_slice()), key_bytes)
        };

        let algorithm = self.encrypt_mode.algorithm().ok_or_else(|| {
            SqueezefsError::InvalidOperation(format!(
                "Unsupported encryption algo: {}",
                self.encrypt_algo
            ))
        })?;

        let unbound_key = UnboundKey::new(algorithm, key_bytes.as_ref()).map_err(|_| {
            SqueezefsError::InvalidOperation("Failed to create unbound key".to_string())
        })?;
        Ok((wrapped_key, Arc::new(LessSafeKey::new(unbound_key))))
    }

    /// Next unique AEAD nonce: 4-byte process salt + 8-byte big-endian
    /// sequence (the construction every writer has always emitted).
    fn next_nonce_bytes(&self) -> [u8; NONCE_LEN] {
        let seq = self
            .nonce_counter
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let mut nonce_bytes = [0u8; NONCE_LEN];
        nonce_bytes[0..4].copy_from_slice(&self.salt);
        nonce_bytes[4..12].copy_from_slice(&seq.to_be_bytes());
        nonce_bytes
    }

    pub fn encrypt(&self, data: &[u8]) -> Result<Vec<u8>, SqueezefsError> {
        if self.encrypt_mode == EncryptMode::None {
            return Err(SqueezefsError::InvalidOperation(
                "encrypt() called with encrypt mode none".to_string(),
            ));
        }

        let (wrapped_key, less_safe_key) = self.resolve_encrypt_key()?;
        let nonce_bytes = self.next_nonce_bytes();
        let nonce = Nonce::try_assume_unique_for_key(&nonce_bytes).map_err(|_| {
            SqueezefsError::InvalidOperation("Failed to construct nonce".to_string())
        })?;

        let tag_len = self.aead_tag_len;
        let mut in_out = Vec::with_capacity(data.len() + tag_len);
        in_out.extend_from_slice(data);

        less_safe_key
            .seal_in_place_append_tag(nonce, ring::aead::Aad::empty(), &mut in_out)
            .map_err(|_| SqueezefsError::InvalidOperation("AEAD seal failed".to_string()))?;

        let mut payload =
            Vec::with_capacity(3 + wrapped_key.len() + nonce_bytes.len() + in_out.len());
        payload.push((wrapped_key.len() >> 8) as u8);
        payload.push((wrapped_key.len() & 0xFF) as u8);
        payload.push(nonce_bytes.len() as u8);
        payload.extend_from_slice(&wrapped_key);
        payload.extend_from_slice(&nonce_bytes);
        payload.extend_from_slice(&in_out);

        Ok(payload)
    }

    pub fn decrypt(&self, data: &[u8]) -> Result<Vec<u8>, SqueezefsError> {
        if data.len() < 3 {
            return Err(SqueezefsError::InvalidOperation(
                "Encrypted data too short".to_string(),
            ));
        }
        let wrapped_key_len = ((data[0] as usize) << 8) + (data[1] as usize);
        let nonce_len = data[2] as usize;

        if 3 + wrapped_key_len + nonce_len > data.len() {
            return Err(SqueezefsError::InvalidOperation(
                "Malformed encrypted data header".to_string(),
            ));
        }

        let wrapped_key = &data[3..3 + wrapped_key_len];
        let nonce_bytes = &data[3 + wrapped_key_len..3 + wrapped_key_len + nonce_len];
        let ciphertext_payload = &data[3 + wrapped_key_len + nonce_len..];

        let less_safe_key = if let Some(cached_key) = self.unwrap_cache.get(wrapped_key) {
            cached_key
        } else {
            let decrypted_key = self.unwrap_session_key(wrapped_key)?;

            let algorithm = self.encrypt_mode.algorithm().ok_or_else(|| {
                SqueezefsError::InvalidOperation(format!(
                    "Unsupported encryption algo: {}",
                    self.encrypt_algo
                ))
            })?;

            let unbound_key = UnboundKey::new(algorithm, decrypted_key.as_ref()).map_err(|_| {
                SqueezefsError::InvalidOperation("Failed to create unbound key".to_string())
            })?;
            let key = Arc::new(LessSafeKey::new(unbound_key));

            self.unwrap_cache.insert(wrapped_key.to_vec(), key.clone());
            self.key_unwrap_count
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            key
        };

        let nonce = Nonce::try_assume_unique_for_key(nonce_bytes).map_err(|_| {
            SqueezefsError::InvalidOperation("Failed to construct nonce".to_string())
        })?;

        let mut in_out = ciphertext_payload.to_vec();
        let decrypted_len = {
            let decrypted_slice = less_safe_key
                .open_in_place(nonce, ring::aead::Aad::empty(), &mut in_out)
                .map_err(|_| SqueezefsError::InvalidOperation("AEAD open failed".to_string()))?;
            decrypted_slice.len()
        };
        in_out.truncate(decrypted_len);
        Ok(in_out)
    }

    /// Compress (or copy, mode `None`) `data` into the head of `out`,
    /// returning the bytes written. The lz4 image reproduces
    /// `compress_prepend_size`'s 4-byte little-endian size prefix — raw
    /// `compress_into` emits no framing and the contractually untouched
    /// read path (`decompress_size_prepended`) requires it (§5.7 on-disk
    /// compatibility). `out` must hold [`Self::scratch_compress_term`] of
    /// `data.len()` bytes; the pooled caller's fit check guarantees it.
    fn compress_into_scratch(&self, data: &[u8], out: &mut [u8]) -> Result<usize, SqueezefsError> {
        match self.compression_mode {
            CompressionMode::Lz4 => {
                out[..4].copy_from_slice(&(data.len() as u32).to_le_bytes());
                let n = lz4_flex::block::compress_into(data, &mut out[4..]).map_err(|e| {
                    SqueezefsError::InvalidOperation(format!(
                        "LZ4 compression into scratch failed: {:?}",
                        e
                    ))
                })?;
                Ok(4 + n)
            }
            CompressionMode::Zstd => zstd::bulk::Compressor::new(3)
                .and_then(|mut c| c.compress_to_buffer(data, out))
                .map_err(|e| {
                    SqueezefsError::InvalidOperation(format!(
                        "ZSTD compression into scratch failed: {:?}",
                        e
                    ))
                }),
            CompressionMode::None => {
                out[..data.len()].copy_from_slice(data);
                Ok(data.len())
            }
        }
    }

    /// FIND-RW4-A store-raw escape, scratch leg: compress into `out`, and
    /// when the compressed form would not SHRINK the payload (equal counts
    /// — an equal-size image is pure decompress cost for zero gain),
    /// overwrite it with the raw payload instead. Returns
    /// `(bytes_written, raw_flag)`. The scratch is sized for the
    /// compressors' worst case, so the intermediate expanded output always
    /// fits; the extra memcpy is paid only by incompressible blocks (whose
    /// pre-fix alternative was an unreadable frame).
    fn compress_or_raw_into_scratch(
        &self,
        data: &[u8],
        out: &mut [u8],
    ) -> Result<(usize, bool), SqueezefsError> {
        let n = self.compress_into_scratch(data, out)?;
        if self.compression_mode != CompressionMode::None && n >= data.len() {
            crate::fuse_client::METRICS
                .compress_stored_raw
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            out[..data.len()].copy_from_slice(data);
            return Ok((data.len(), true));
        }
        Ok((n, false))
    }

    /// §5.7 pooled transform: compress into the scratch at the sealed-payload
    /// offset, then encrypt **in place within the scratch** — header first,
    /// `seal_in_place_separate_tag` at the offset (a raw fixed scratch has no
    /// `Extend`, so `seal_in_place_append_tag` cannot apply), tag written
    /// after the ciphertext. Exactly one pooled transform buffer total,
    /// returned as `Bytes` over the 4096-aligned backing (recycles when the
    /// last handle drops; the DMA takes `WriteData::Aligned` whenever the
    /// ciphertext lands on a 4 KiB multiple). The input is never mutated.
    ///
    /// The image is emitted INSIDE the self-delimiting frame (see
    /// `FRAME_LEN_BYTES` / `FRAME_RAW_FLAG`): everything transform-related
    /// lands at `out[FRAME_LEN_BYTES..]`, and the frame word is written
    /// last.
    fn process_write_pooled(
        &self,
        pool: &Arc<BufferPool>,
        data: &[u8],
    ) -> Result<bytes::Bytes, SqueezefsError> {
        const F: usize = FRAME_LEN_BYTES;
        let mut scratch = pool.alloc();
        let (image_len, raw_flag) = if self.encrypt_mode != EncryptMode::None {
            let (wrapped_key, less_safe_key) = self.resolve_encrypt_key()?;
            let wrapped_len = wrapped_key.len();
            let header_len = ENCRYPT_HEADER_PREFIX_LEN + wrapped_len + NONCE_LEN;
            let out = scratch.backing_mut();

            let (plain_len, raw_flag) =
                self.compress_or_raw_into_scratch(data, &mut out[F + header_len..])?;

            // The exact on-disk header `encrypt` emits:
            // [2B wrapped_key_len][1B nonce_len][wrapped_key][nonce].
            out[F] = (wrapped_len >> 8) as u8;
            out[F + 1] = (wrapped_len & 0xFF) as u8;
            out[F + 2] = NONCE_LEN as u8;
            out[F + ENCRYPT_HEADER_PREFIX_LEN..F + ENCRYPT_HEADER_PREFIX_LEN + wrapped_len]
                .copy_from_slice(&wrapped_key);
            let nonce_bytes = self.next_nonce_bytes();
            out[F + ENCRYPT_HEADER_PREFIX_LEN + wrapped_len..F + header_len]
                .copy_from_slice(&nonce_bytes);
            let nonce = Nonce::try_assume_unique_for_key(&nonce_bytes).map_err(|_| {
                SqueezefsError::InvalidOperation("Failed to construct nonce".to_string())
            })?;

            let tag = less_safe_key
                .seal_in_place_separate_tag(
                    nonce,
                    ring::aead::Aad::empty(),
                    &mut out[F + header_len..F + header_len + plain_len],
                )
                .map_err(|_| SqueezefsError::InvalidOperation("AEAD seal failed".to_string()))?;
            let tag_bytes = tag.as_ref();
            out[F + header_len + plain_len..F + header_len + plain_len + tag_bytes.len()]
                .copy_from_slice(tag_bytes);
            (header_len + plain_len + tag_bytes.len(), raw_flag)
        } else {
            let out = scratch.backing_mut();
            self.compress_or_raw_into_scratch(data, &mut out[F..])?
        };
        let word = image_len as u32 | if raw_flag { FRAME_RAW_FLAG } else { 0 };
        scratch.backing_mut()[..F].copy_from_slice(&word.to_le_bytes());
        scratch.set_written_len(F + image_len);
        debug_assert!(
            F + image_len <= self.max_stored_image_len(data.len()),
            "stored image exceeded its FIND-RW4-A bound"
        );
        Ok(scratch.into_bytes())
    }

    pub fn process_write(&self, data: bytes::Bytes) -> Result<bytes::Bytes, SqueezefsError> {
        // P2-3: enum-mode fast path — no string trim/match per block.
        if self.is_passthrough() {
            return Ok(data);
        }
        // §5.7 CRYPTO_SCRATCH_POOL: one pooled worst-case transform buffer
        // when the mount initialized the pool and the input fits its sizing
        // basis. Everything else — uninitialized ad-hoc states, or overflow
        // past the pooled worst case — bounces to the heap `Vec` path below
        // (byte-compatible; pinned by the read-back parity tests).
        if let Some(pool) = self.scratch_pool.get() {
            if self.worst_case_scratch_len(data.len()) <= pool.buf_size() {
                return self.process_write_pooled(pool, &data);
            }
        }
        let compressed = self.compress(&data)?;
        // FIND-RW4-A store-raw escape (heap leg): a compressed image that
        // did not shrink is replaced by the raw payload + frame marker.
        let raw_flag =
            self.compression_mode != CompressionMode::None && compressed.len() >= data.len();
        let plain: &[u8] = if raw_flag {
            crate::fuse_client::METRICS
                .compress_stored_raw
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            &data
        } else {
            &compressed
        };
        let image: std::borrow::Cow<[u8]> = if self.encrypt_mode != EncryptMode::None {
            std::borrow::Cow::Owned(self.encrypt(plain)?)
        } else {
            std::borrow::Cow::Borrowed(plain)
        };
        // Self-delimiting frame (heap leg — see `FRAME_LEN_BYTES` /
        // `FRAME_RAW_FLAG`).
        let word = image.len() as u32 | if raw_flag { FRAME_RAW_FLAG } else { 0 };
        let mut framed = Vec::with_capacity(FRAME_LEN_BYTES + image.len());
        framed.extend_from_slice(&word.to_le_bytes());
        framed.extend_from_slice(&image);
        debug_assert!(
            framed.len() <= self.max_stored_image_len(data.len()),
            "stored image exceeded its FIND-RW4-A bound"
        );
        Ok(bytes::Bytes::from(framed))
    }

    pub fn process_read<'a>(
        &self,
        data: &'a [u8],
    ) -> Result<std::borrow::Cow<'a, [u8]>, SqueezefsError> {
        if self.is_passthrough() {
            return Ok(std::borrow::Cow::Borrowed(data));
        }
        // Frame parse: block reads return the full device WINDOW — the
        // stored image plus whatever trailing bytes the device holds.
        // Neither decoder tolerates the padding (lz4's
        // `decompress_size_prepended` rejects trailing bytes; AEAD opens
        // `data[header..]`, so padding lands inside the tag check), which
        // is exactly why every image is written self-delimiting. Unframed
        // legacy blobs refuse LOUD (forward-only): cold device reads of
        // such volumes never worked, so there is no behavior to preserve
        // — rewrite/reformat is the remedy, never a sniffing shim.
        if data.len() < FRAME_LEN_BYTES {
            return Err(SqueezefsError::InvalidOperation(
                "transform image shorter than its frame header".to_string(),
            ));
        }
        let word = u32::from_le_bytes(data[..FRAME_LEN_BYTES].try_into().unwrap());
        // FIND-RW4-A dispatch: bit 31 = store-raw escape (image payload
        // was not compressed). Pre-fix frames always carried 0 there —
        // strict-superset decoding.
        let raw_image = word & FRAME_RAW_FLAG != 0;
        let image_len = (word & FRAME_LEN_MASK) as usize;
        let image = data
            .get(FRAME_LEN_BYTES..FRAME_LEN_BYTES + image_len)
            .ok_or_else(|| {
                SqueezefsError::InvalidOperation(format!(
                    "malformed transform frame: claims {image_len} image bytes, {} available \
                     — unframed legacy volume or corrupt block (reformat/rewrite required)",
                    data.len() - FRAME_LEN_BYTES
                ))
            })?;
        let decrypted = if self.encrypt_mode != EncryptMode::None {
            Some(self.decrypt(image)?)
        } else {
            None
        };

        match (decrypted, raw_image) {
            // Store-raw escape: the (decrypted) payload IS the plaintext.
            (Some(v), true) => Ok(std::borrow::Cow::Owned(v)),
            (None, true) => Ok(std::borrow::Cow::Borrowed(image)),
            (Some(v), false) => {
                let decompressed = self.decompress(&v)?;
                Ok(std::borrow::Cow::Owned(decompressed.into_owned()))
            }
            (None, false) => Ok(std::borrow::Cow::Owned(
                self.decompress(image)?.into_owned(),
            )),
        }
    }

    pub async fn process_write_async(
        &self,
        data: bytes::Bytes,
    ) -> Result<bytes::Bytes, SqueezefsError> {
        if self.is_passthrough() {
            return Ok(data);
        }
        if data.len() < 65536 {
            return self.process_write(data);
        }
        let state = self.clone();
        tokio::task::spawn_blocking(move || state.process_write(data))
            .await
            .map_err(|e| SqueezefsError::Io(std::io::Error::new(std::io::ErrorKind::Other, e)))?
    }

    pub async fn process_read_async(
        &self,
        data: bytes::Bytes,
    ) -> Result<bytes::Bytes, SqueezefsError> {
        if self.is_passthrough() {
            return Ok(data);
        }
        if data.len() < 65536 {
            let res = self.process_read(&data)?;
            return Ok(bytes::Bytes::from(res.into_owned()));
        }
        let state = self.clone();
        tokio::task::spawn_blocking(move || {
            let res = state.process_read(&data)?;
            Ok(bytes::Bytes::from(res.into_owned()))
        })
        .await
        .map_err(|e| SqueezefsError::Io(std::io::Error::new(std::io::ErrorKind::Other, e)))?
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The mount-resolved volume key every keyed test uses (KW-1: derived
    /// from operator key material + the volume's KDF salt, never a PEM).
    fn test_key() -> crate::keyfile::VolumeKey {
        let material =
            crate::keyfile::KeyMaterial::from_bytes(b"unit-test-key-material-0123456789".to_vec())
                .expect("material");
        crate::keyfile::derive_volume_key(&material, &[0x11u8; 32])
    }

    /// Frame an image the way the writers do (test-side reference).
    fn framed(image: &[u8]) -> Vec<u8> {
        let mut v = Vec::with_capacity(4 + image.len());
        v.extend_from_slice(&(image.len() as u32).to_le_bytes());
        v.extend_from_slice(image);
        v
    }

    /// RAW-flagged frame (the FIND-RW4-A store-raw escape form).
    fn framed_raw(payload: &[u8]) -> Vec<u8> {
        let mut v = Vec::with_capacity(4 + payload.len());
        v.extend_from_slice(&(payload.len() as u32 | FRAME_RAW_FLAG).to_le_bytes());
        v.extend_from_slice(payload);
        v
    }

    #[test]
    fn test_crypto_unwrap_caching() {
        let key = test_key();
        let state =
            CryptoCompressState::new("none".to_string(), "aes256gcm".to_string(), Some(&key));
        let data = b"some block payload";

        assert!(state.precomputed_encrypt_key.is_some());
        assert!(state.prewrapped_key.is_some());
        assert!(state.aead_tag_len > 0);
        assert_eq!(state.encrypt_mode, EncryptMode::Aes256Gcm);

        let encrypted = state.encrypt(data).unwrap();

        // First decryption: should miss the cache and unwrap
        let decrypted1 = state.decrypt(&encrypted).unwrap();
        assert_eq!(decrypted1, data);
        assert_eq!(
            state
                .key_unwrap_count
                .load(std::sync::atomic::Ordering::Relaxed),
            1
        );

        // Second decryption: should hit the cache and bypass the unwrap
        let decrypted2 = state.decrypt(&encrypted).unwrap();
        assert_eq!(decrypted2, data);
        assert_eq!(
            state
                .key_unwrap_count
                .load(std::sync::atomic::Ordering::Relaxed),
            1
        );
    }

    #[test]
    fn test_session_key_reused_across_encrypts() {
        let key = test_key();
        let state =
            CryptoCompressState::new("none".to_string(), "aes256gcm".to_string(), Some(&key));

        let a = state.encrypt(b"block-a").unwrap();
        let b = state.encrypt(b"block-b").unwrap();

        // Same wrapped session-key prefix (2-byte len + wrap blob bytes).
        let wrap_len = ((a[0] as usize) << 8) + (a[1] as usize);
        assert_eq!(&a[3..3 + wrap_len], &b[3..3 + wrap_len]);
        // Nonces/ciphertexts differ.
        assert_ne!(a, b);

        assert_eq!(state.decrypt(&a).unwrap(), b"block-a");
        assert_eq!(state.decrypt(&b).unwrap(), b"block-b");
        // Both decrypts share one session unwrap after first miss.
        assert_eq!(
            state
                .key_unwrap_count
                .load(std::sync::atomic::Ordering::Relaxed),
            1
        );
    }

    #[test]
    fn test_passthrough_process_write_is_identity() {
        let state = CryptoCompressState::new("none".to_string(), "none".to_string(), None);
        assert!(state.is_passthrough());
        let payload = bytes::Bytes::from_static(b"hello");
        let out = state.process_write(payload.clone()).unwrap();
        assert_eq!(out, payload);
        // Same pointer when fully passthrough (zero-copy).
        assert_eq!(out.as_ptr(), payload.as_ptr());
    }

    #[test]
    fn test_mode_parse() {
        assert_eq!(CompressionMode::parse("lz4").unwrap(), CompressionMode::Lz4);
        assert_eq!(
            EncryptMode::parse("chacha20-rsa").unwrap(),
            EncryptMode::ChaCha20
        );
        assert!(CompressionMode::parse("bogus").is_err());
    }

    #[test]
    fn test_lz4_roundtrip_process() {
        let state = CryptoCompressState::new("lz4".to_string(), "none".to_string(), None);
        let payload = bytes::Bytes::from(vec![7u8; 64 * 1024]);
        let written = state.process_write(payload.clone()).unwrap();
        assert!(written.len() < payload.len());
        let read = state.process_read(&written).unwrap();
        assert_eq!(&*read, payload.as_ref());
    }

    // -----------------------------------------------------------------
    // §5.7 CRYPTO_SCRATCH_POOL (zero-copy write-path design, severable
    // sub-commit): worst-case-sized pooled transform scratch for the
    // non-passthrough write path. Contract pinned here:
    //
    //  - pool buffers are sized at init as worst_case(block_size) =
    //    on-disk header ([2B wrapped_key_len][1B nonce_len][wrapped_key]
    //    [nonce]) with the ACTUAL prewrapped key blob length (512 B
    //    conservative WRAPPED_KEY_LEN_FALLBACK when none exists at
    //    pool-init time) + nonce
    //    (12 B) + max(4-byte-prefixed lz4 maximum output, zstd compress
    //    bound) + AEAD tag, rounded up to the next 4 KiB;
    //  - pooled lz4 output is BYTE-IDENTICAL to `compress_prepend_size`
    //    (the 4-byte little-endian size-prefix framing the untouched
    //    `decompress_size_prepended` read path requires — the on-disk
    //    compatibility pin);
    //  - blocks written by the pre-scratch writer primitives decode
    //    through the (contractually untouched) read path, and pooled
    //    blocks decode through that same read path (read-back parity);
    //  - exactly one pooled buffer per transform, checked out for the
    //    output's lifetime, recycled on drop, 4096-aligned backing;
    //  - inputs whose worst case exceeds the pool's buffer size bounce
    //    to today's heap `Vec`s; states never initialized stay on the
    //    heap path (and keep emitting pre-scratch bytes);
    //  - `process_write` never mutates its input snapshot (the same
    //    plaintext backs read-LRU / RYW reads).
    // -----------------------------------------------------------------

    use rstest::rstest;

    /// One mount-resolved volume key for every scratch-pool test.
    static TEST_KEY: once_cell::sync::Lazy<crate::keyfile::VolumeKey> =
        once_cell::sync::Lazy::new(test_key);

    /// Deterministic high-entropy filler (no rand dependency on content).
    fn lcg_bytes(len: usize, mut seed: u64) -> Vec<u8> {
        let mut v = Vec::with_capacity(len);
        for _ in 0..len {
            seed = seed
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            v.push((seed >> 33) as u8);
        }
        v
    }

    /// Compressible-run + entropy-tail payload (exercises literal and
    /// match paths of both compressors).
    fn mixed_payload(len: usize) -> Vec<u8> {
        let mut v = vec![0x5Au8; len];
        let tail = len / 4;
        v[len - tail..].copy_from_slice(&lcg_bytes(tail, 0x5EED));
        v
    }

    fn pool_len(state: &CryptoCompressState) -> usize {
        state
            .scratch_pool
            .get()
            .expect("CRYPTO_SCRATCH_POOL must be initialized")
            .len()
    }

    /// §5.7 sizing formula, recomputed independently from the dependency
    /// bounds (the lz4 term carries the 4-byte size-prefix framing).
    fn expected_scratch_buf_len(state: &CryptoCompressState, block_size: usize) -> usize {
        let compress_term = std::cmp::max(
            4 + lz4_flex::block::get_maximum_output_size(block_size),
            zstd::zstd_safe::compress_bound(block_size),
        );
        let encrypt_overhead = if state.encrypt_mode != EncryptMode::None {
            let wrapped = state
                .prewrapped_key
                .as_ref()
                .map(|(w, _)| w.len())
                .unwrap_or(512);
            3 + wrapped + 12 + state.aead_tag_len
        } else {
            0
        };
        (compress_term + encrypt_overhead).next_multiple_of(4096)
    }

    #[test]
    fn test_scratch_pool_sizing_uses_actual_prewrapped_key_blob() {
        let bs = 256 * 1024;
        let state =
            CryptoCompressState::new("lz4".to_string(), "aes256gcm".to_string(), Some(&TEST_KEY));
        let wrapped_len = state
            .prewrapped_key
            .as_ref()
            .expect("session key must prewrap with a volume key configured")
            .0
            .len();
        assert_eq!(
            wrapped_len, WRAP_V2_LEN,
            "the KW-1 wrap is a fixed 63 bytes"
        );

        state.init_scratch_pool(bs);
        let buf_len = state
            .scratch_pool_buf_len()
            .expect("pool must initialize for non-passthrough state");
        assert_eq!(
            buf_len,
            expected_scratch_buf_len(&state, bs),
            "pool buffer must be the §5.7 worst case for the ACTUAL wrapped key blob"
        );
        assert_eq!(buf_len % 4096, 0, "worst case must round to 4 KiB");
        // Tight rounding: next multiple, not an overshoot.
        assert!(
            buf_len - expected_scratch_buf_len(&state, bs) < 4096,
            "rounded to the NEXT 4 KiB, not beyond"
        );
        // ≈ block_size + 128 KiB for the design's shape.
        assert!(buf_len > bs && buf_len <= bs + 128 * 1024);
    }

    #[test]
    fn test_scratch_pool_sizing_falls_back_to_the_conservative_wrap_bound() {
        // Encryption configured but no volume key at pool-init time: no
        // prewrapped blob exists, so sizing must assume the conservative
        // WRAPPED_KEY_LEN_FALLBACK (the geometry bound readers size their
        // device windows with — see its doc comment).
        let bs = 256 * 1024;
        let state = CryptoCompressState::new("lz4".to_string(), "aes256gcm".to_string(), None);
        assert!(state.prewrapped_key.is_none());
        state.init_scratch_pool(bs);
        let compress_term = std::cmp::max(
            4 + lz4_flex::block::get_maximum_output_size(bs),
            zstd::zstd_safe::compress_bound(bs),
        );
        let expected = (compress_term + 3 + WRAPPED_KEY_LEN_FALLBACK + 12 + state.aead_tag_len)
            .next_multiple_of(4096);
        assert_eq!(state.scratch_pool_buf_len(), Some(expected));
    }

    #[test]
    fn test_scratch_pool_init_idempotent_and_passthrough_noop() {
        // Passthrough never transforms: a scratch pool would be dead weight.
        let passthrough = CryptoCompressState::new("none".to_string(), "none".to_string(), None);
        passthrough.init_scratch_pool(4 * 1024 * 1024);
        assert_eq!(
            passthrough.scratch_pool_buf_len(),
            None,
            "passthrough state must not allocate a scratch pool"
        );

        // First init wins; re-init with a different block size is a no-op.
        let state = CryptoCompressState::new("lz4".to_string(), "none".to_string(), None);
        state.init_scratch_pool(64 * 1024);
        let first = state.scratch_pool_buf_len().unwrap();
        state.init_scratch_pool(4 * 1024 * 1024);
        assert_eq!(state.scratch_pool_buf_len(), Some(first));
    }

    #[test]
    fn test_lz4_scratch_framing_byte_identical_to_prepend_size() {
        // On-disk compatibility pin: raw `compress_into` emits no framing,
        // so the pooled writer must reproduce the exact
        // `compress_prepend_size` image (4-byte LE uncompressed size +
        // block stream) that `decompress_size_prepended` requires.
        let bs = 256 * 1024;
        let state = CryptoCompressState::new("lz4".to_string(), "none".to_string(), None);
        state.init_scratch_pool(bs);
        let cap0 = pool_len(&state);
        assert!(cap0 > 0, "pool must preallocate");

        // Deep write first: leaves long stale content in a recycled buffer.
        // Entropy payload — the FIND-RW4-A store-raw escape emits the
        // RAW-flagged frame (compression would expand it).
        let deep = lcg_bytes(bs, 0xD1CE);
        let deep_out = state
            .process_write(bytes::Bytes::from(deep.clone()))
            .unwrap();
        assert_eq!(pool_len(&state), cap0 - 1, "deep write must be pooled");
        assert_eq!(deep_out.as_ref(), framed_raw(&deep).as_slice());
        assert_eq!(
            state.process_read(&deep_out).unwrap().as_ref(),
            deep.as_slice()
        );
        drop(deep_out);
        assert_eq!(pool_len(&state), cap0);

        // Then cycle short writes through every pooled buffer: stale bytes
        // from the deep write must never leak past the written length.
        let short = mixed_payload(32 * 1024);
        let expected = framed(&lz4_flex::compress_prepend_size(&short));
        for _ in 0..cap0 {
            let out = state
                .process_write(bytes::Bytes::from(short.clone()))
                .unwrap();
            assert_eq!(pool_len(&state), cap0 - 1, "short write must be pooled");
            assert_eq!(
                out.as_ref(),
                expected.as_slice(),
                "pooled lz4 image must be the framed compress_prepend_size image"
            );
            assert_eq!(state.process_read(&out).unwrap().as_ref(), short.as_slice());
        }

        // Empty payload: lz4's 4-byte size prefix cannot shrink 0 bytes,
        // so the escape stores it raw — a lone RAW-flagged zero-length
        // frame word.
        let empty_out = state.process_write(bytes::Bytes::new()).unwrap();
        assert_eq!(empty_out.as_ref(), framed_raw(&[]).as_slice());
        assert!(state.process_read(&empty_out).unwrap().is_empty());
    }

    #[rstest]
    #[case::lz4("lz4", "none")]
    #[case::zstd("zstd", "none")]
    #[case::aes("none", "aes256gcm")]
    #[case::chacha("none", "chacha20")]
    #[case::lz4_aes("lz4", "aes256gcm")]
    #[case::zstd_chacha("zstd", "chacha20")]
    fn test_pre_scratch_volume_read_back_parity(#[case] comp: &str, #[case] enc: &str) {
        // Heap-vs-pooled writer parity through the framed read path, plus
        // the forward-only legacy rule: an UNFRAMED pre-framing blob (the
        // exact primitives the old writers used) must refuse LOUD — its
        // cold device reads never worked (padding broke both decoders),
        // and a sniffing shim is exactly what the standing directive
        // forbids.
        let bs = 128 * 1024;
        let key = if enc == "none" {
            None
        } else {
            Some(&*TEST_KEY)
        };
        let state = CryptoCompressState::new(comp.to_string(), enc.to_string(), key);
        state.init_scratch_pool(bs);
        let payload = mixed_payload(96 * 1024);

        // Pre-framing writer image (the exact primitives the old
        // `process_write` used) — refuses loud today.
        let compressed: Vec<u8> = match comp {
            "lz4" => lz4_flex::compress_prepend_size(&payload),
            "zstd" => zstd::encode_all(std::io::Cursor::new(&payload[..]), 3).unwrap(),
            _ => payload.clone(),
        };
        let old_blob: Vec<u8> = if enc != "none" {
            state.encrypt(&compressed).unwrap()
        } else {
            compressed.clone()
        };
        assert!(
            state.process_read(&old_blob).is_err(),
            "unframed legacy blob must refuse loud (forward-only)"
        );

        // Pooled writer image, decoded by the framed read path.
        let cap0 = pool_len(&state);
        let new_blob = state
            .process_write(bytes::Bytes::from(payload.clone()))
            .unwrap();
        assert_eq!(pool_len(&state), cap0 - 1, "transform must be pooled");
        assert_eq!(
            state.process_read(&new_blob).unwrap().as_ref(),
            payload.as_slice(),
            "pooled block must decode through the framed read path"
        );
        // Padded to a device window: still decodes (the frame's purpose).
        let mut padded = new_blob.to_vec();
        padded.resize(bs, 0xEE);
        assert_eq!(
            state.process_read(&padded).unwrap().as_ref(),
            payload.as_slice(),
            "device-window padding must be ignored by the frame parse"
        );
        if enc == "none" && comp == "lz4" {
            assert_eq!(new_blob[FRAME_LEN_BYTES..], old_blob[..]);
        }
    }

    #[test]
    fn test_scratch_pool_checkout_recycle_and_alignment() {
        let bs = 256 * 1024;
        let state =
            CryptoCompressState::new("lz4".to_string(), "aes256gcm".to_string(), Some(&TEST_KEY));
        state.init_scratch_pool(bs);
        let cap0 = pool_len(&state);
        assert!(cap0 > 0);

        let payload = bytes::Bytes::from(mixed_payload(bs));
        let out = state.process_write(payload.clone()).unwrap();
        // Exactly ONE pooled transform buffer, checked out for the
        // output's lifetime (compress-into-scratch + seal-in-place —
        // no second transform buffer).
        assert_eq!(pool_len(&state), cap0 - 1);
        // 4096-aligned backing: the DMA takes `WriteData::Aligned` whenever
        // the ciphertext lands on a 4 KiB multiple.
        assert_eq!(out.as_ptr() as usize % 4096, 0);
        assert_eq!(state.process_read(&out).unwrap().as_ref(), payload.as_ref());
        drop(out);
        assert_eq!(pool_len(&state), cap0, "backing must recycle on drop");

        // Recycled buffers serve subsequent transforms.
        let out2 = state.process_write(payload.clone()).unwrap();
        assert_eq!(pool_len(&state), cap0 - 1);
        assert_eq!(
            state.process_read(&out2).unwrap().as_ref(),
            payload.as_ref()
        );
        drop(out2);
        assert_eq!(pool_len(&state), cap0);
    }

    #[test]
    fn test_incompressible_input_stays_pooled_and_stores_raw() {
        // Incompressible data would expand under lz4; the FIND-RW4-A
        // escape stores it RAW instead — bounded by frame + payload — and
        // the transform stays pooled (the scratch holds the compressor's
        // intermediate expansion before the escape decision).
        let bs = 128 * 1024;
        let state = CryptoCompressState::new("lz4".to_string(), "none".to_string(), None);
        state.init_scratch_pool(bs);
        let cap0 = pool_len(&state);

        let payload = lcg_bytes(bs, 0xBAD5EED);
        let out = state
            .process_write(bytes::Bytes::from(payload.clone()))
            .unwrap();
        assert_eq!(
            out.len(),
            FRAME_LEN_BYTES + payload.len(),
            "raw escape bounds the stored image at frame + raw payload"
        );
        assert_eq!(
            out.len(),
            state.max_stored_image_len(payload.len()),
            "compression-only worst case is exactly the raw-escape image"
        );
        assert_eq!(pool_len(&state), cap0 - 1, "raw escape stays pooled");
        assert_eq!(
            state.process_read(&out).unwrap().as_ref(),
            payload.as_slice()
        );
    }

    #[test]
    fn test_scratch_overflow_bounces_to_heap() {
        // Inputs past the pool's sizing basis (worst case > buffer size)
        // must bounce to today's heap `Vec` path — and only those.
        let bs = 64 * 1024;
        let state =
            CryptoCompressState::new("lz4".to_string(), "aes256gcm".to_string(), Some(&TEST_KEY));
        state.init_scratch_pool(bs);
        let cap0 = pool_len(&state);

        let oversized = bytes::Bytes::from(mixed_payload(2 * bs));
        let out = state.process_write(oversized.clone()).unwrap();
        assert_eq!(
            pool_len(&state),
            cap0,
            "overflow bounce must not touch the pool"
        );
        // DUR-8e: the read side bounds a declared plaintext by the
        // configured block size, so this deliberately-oversized image is
        // read back by a state sized for it (a 2×-block plaintext is not
        // a shape the product's write paths can produce — they are all
        // per-block or smaller — which is exactly why the bound is safe).
        let reader =
            CryptoCompressState::new("lz4".to_string(), "aes256gcm".to_string(), Some(&TEST_KEY));
        reader.init_scratch_pool(2 * bs);
        assert_eq!(
            reader.process_read(&out).unwrap().as_ref(),
            oversized.as_ref()
        );
        assert!(
            state.process_read(&out).is_err(),
            "a plaintext above the configured block size must be refused, not allocated"
        );
    }

    #[test]
    fn test_uninitialized_state_stays_on_pre_scratch_heap_path() {
        // No `init_scratch_pool` (e.g. ad-hoc states): the heap path stays,
        // and its bytes are exactly the pre-scratch writer's bytes.
        let state = CryptoCompressState::new("lz4".to_string(), "none".to_string(), None);
        assert_eq!(state.scratch_pool_buf_len(), None);
        let payload = mixed_payload(96 * 1024);
        let out = state
            .process_write(bytes::Bytes::from(payload.clone()))
            .unwrap();
        assert_eq!(
            out.as_ref(),
            framed(&lz4_flex::compress_prepend_size(&payload)).as_slice()
        );
        assert_eq!(
            state.process_read(&out).unwrap().as_ref(),
            payload.as_slice()
        );
    }

    #[test]
    fn test_process_write_never_mutates_input() {
        // Input immutability (§5.7): the same plaintext snapshot backs
        // read-LRU and RYW reads; an in-place-over-input "optimization" is
        // explicitly rejected.
        let bs = 128 * 1024;
        let state =
            CryptoCompressState::new("zstd".to_string(), "chacha20".to_string(), Some(&TEST_KEY));
        state.init_scratch_pool(bs);
        let pristine = mixed_payload(bs);
        let input = bytes::Bytes::from(pristine.clone());
        let input_ptr = input.as_ptr();
        let out = state.process_write(input.clone()).unwrap();
        assert_ne!(out.as_ptr(), input_ptr, "output must be a fresh buffer");
        assert_eq!(
            input.as_ref(),
            pristine.as_slice(),
            "input snapshot must stay immutable"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn test_scratch_pool_shared_across_clones_async() {
        // `process_write_async` clones the state into `spawn_blocking` for
        // ≥ 64 KiB payloads: the clone must share the SAME pool (recycle
        // returns the buffer to the mount's pool, not a per-clone orphan).
        let bs = 256 * 1024;
        let state = CryptoCompressState::new("lz4".to_string(), "none".to_string(), None);
        state.init_scratch_pool(bs);
        let cap0 = pool_len(&state);

        let payload = bytes::Bytes::from(mixed_payload(bs));
        let out = state.process_write_async(payload.clone()).await.unwrap();
        assert_eq!(
            pool_len(&state),
            cap0 - 1,
            "clone must draw from the shared pool"
        );
        assert_eq!(out.as_ptr() as usize % 4096, 0);
        assert_eq!(state.process_read(&out).unwrap().as_ref(), payload.as_ref());
        drop(out);
        assert_eq!(
            pool_len(&state),
            cap0,
            "clone must recycle into the shared pool"
        );
    }

    #[test]
    fn test_encrypted_scratch_header_parity() {
        // The pooled writer emits the exact on-disk header `encrypt` emits:
        // [2B wrapped_key_len][1B nonce_len][wrapped_key][nonce] — no mode
        // byte — sealing at the header offset with a separate tag.
        let bs = 128 * 1024;
        let state =
            CryptoCompressState::new("lz4".to_string(), "aes256gcm".to_string(), Some(&TEST_KEY));
        state.init_scratch_pool(bs);
        let payload = mixed_payload(64 * 1024);

        let framed_pooled = state
            .process_write(bytes::Bytes::from(payload.clone()))
            .unwrap();
        let compressed = state.compress(&payload).unwrap();
        let heap = state.encrypt(&compressed).unwrap();
        // The AEAD IMAGE sits inside the frame; `encrypt` emits the raw
        // (unframed) image — compare the layouts at the image level.
        let pooled = &framed_pooled[FRAME_LEN_BYTES..];

        // Identical header framing and (session) wrapped-key bytes.
        let wkl = ((pooled[0] as usize) << 8) + (pooled[1] as usize);
        assert_eq!(&pooled[..2], &heap[..2]);
        assert_eq!(pooled[2], 12, "nonce_len byte");
        assert_eq!(heap[2], 12);
        assert_eq!(&pooled[3..3 + wkl], &heap[3..3 + wkl]);
        assert_eq!(
            &pooled[3..3 + wkl],
            state.prewrapped_key.as_ref().unwrap().0.as_ref()
        );

        // AEAD length arithmetic: header + compressed + tag.
        let compressed_len = compressed.len();
        assert_eq!(
            pooled.len(),
            3 + wkl + 12 + compressed_len + state.aead_tag_len
        );

        // The sealed image opens to the compressed bytes; the framed read
        // path recovers the plaintext; the raw (unframed) heap image
        // refuses loud (forward-only).
        assert_eq!(state.decrypt(pooled).unwrap().as_slice(), &*compressed);
        assert_eq!(
            state.process_read(&framed_pooled).unwrap().as_ref(),
            payload.as_slice()
        );
        assert!(
            state.process_read(&heap).is_err(),
            "unframed image must refuse loud through the framed read path"
        );
    }
}
