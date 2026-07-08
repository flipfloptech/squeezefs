use crate::cache::pool::{BufferPool, POOLED_BUF_ALIGN};
use crate::error::SqueezefsError;
use ring::aead::{LessSafeKey, Nonce, UnboundKey, AES_256_GCM, CHACHA20_POLY1305};
use ring::rand::{SecureRandom, SystemRandom};
use rsa::pkcs1::DecodeRsaPrivateKey;
use rsa::RsaPrivateKey;
use std::sync::Arc;

/// On-disk AEAD header prefix: `[2B wrapped_key_len][1B nonce_len]`.
const ENCRYPT_HEADER_PREFIX_LEN: usize = 3;

/// AEAD nonce length emitted by every writer (4-byte salt + 8-byte counter).
const NONCE_LEN: usize = 12;

/// §5.7: conservative wrapped-key bound for scratch sizing when no
/// prewrapped session-key blob exists at pool-init time — an RSA-4096 OAEP
/// wrap (a 256 B RSA-2048 assumption would silently push 4096-bit-key
/// configs onto the overflow bounce the pool exists to avoid).
const WRAPPED_KEY_LEN_FALLBACK: usize = 512;

/// Resolved compression mode (P2-3) — avoids string matching on every block.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CompressionMode {
    None,
    Lz4,
    Zstd,
}

/// Resolved encryption mode (P2-3).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EncryptMode {
    None,
    Aes256GcmRsa,
    ChaCha20Rsa,
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
            "aes256gcm-rsa" => Ok(Self::Aes256GcmRsa),
            "chacha20-rsa" => Ok(Self::ChaCha20Rsa),
            other => Err(SqueezefsError::InvalidOperation(format!(
                "Unsupported encryption algo: {other}"
            ))),
        }
    }

    fn algorithm(self) -> Option<&'static ring::aead::Algorithm> {
        match self {
            Self::None => None,
            Self::Aes256GcmRsa => Some(&AES_256_GCM),
            Self::ChaCha20Rsa => Some(&CHACHA20_POLY1305),
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
    pub private_key: Option<Arc<RsaPrivateKey>>,
    pub unwrap_cache: moka::sync::Cache<Vec<u8>, Arc<LessSafeKey>>,
    pub key_unwrap_count: Arc<std::sync::atomic::AtomicUsize>,
    /// Session data key: RSA-wrapped blob (shared) + raw key bytes for fallback paths.
    pub prewrapped_key: Option<(Arc<[u8]>, [u8; 32])>,
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
}

impl CryptoCompressState {
    pub fn new(compression: String, encrypt_algo: String, private_key_pem: Option<&str>) -> Self {
        let compression_mode =
            CompressionMode::parse(&compression).unwrap_or(CompressionMode::None);
        let encrypt_mode = EncryptMode::parse(&encrypt_algo).unwrap_or(EncryptMode::None);

        let private_key = private_key_pem.and_then(|pem| {
            if pem.is_empty() || pem == "none" {
                None
            } else {
                RsaPrivateKey::from_pkcs1_pem(pem)
                    .inspect_err(|e| {
                        log::error!("Failed to parse private key PEM: {:?}", e);
                    })
                    .ok()
                    .map(Arc::new)
            }
        });

        let mut prewrapped_key = None;
        if let Some(ref priv_key) = private_key {
            if encrypt_mode != EncryptMode::None {
                let mut key_bytes = [0u8; 32];
                let mut rng = rand::thread_rng();
                if SystemRandom::new().fill(&mut key_bytes).is_ok() {
                    let public_key = priv_key.to_public_key();
                    if let Ok(wrapped) =
                        public_key.encrypt(&mut rng, rsa::Oaep::new::<sha2::Sha256>(), &key_bytes)
                    {
                        let wrapped: Arc<[u8]> = Arc::from(wrapped.into_boxed_slice());
                        prewrapped_key = Some((wrapped, key_bytes));
                    }
                }
            }
        }

        let mut precomputed_encrypt_key = None;
        let mut aead_tag_len = 0usize;
        if let Some((_, ref key_bytes)) = prewrapped_key {
            if let Some(algorithm) = encrypt_mode.algorithm() {
                aead_tag_len = algorithm.tag_len();
                if let Ok(unbound_key) = UnboundKey::new(algorithm, key_bytes) {
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
            private_key,
            unwrap_cache,
            key_unwrap_count: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
            prewrapped_key,
            precomputed_encrypt_key,
            aead_tag_len,
            nonce_counter,
            salt,
            scratch_pool: Arc::new(once_cell::sync::OnceCell::new()),
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
    /// header + the ACTUAL prewrapped session-key blob (512 B RSA-4096
    /// fallback when none exists at pool-init time) + nonce + tag.
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
    /// pooled path's fit check against the pool's buffer size.
    fn worst_case_scratch_len(&self, input_len: usize) -> usize {
        Self::scratch_compress_term(input_len) + self.scratch_encrypt_overhead()
    }

    /// Initialize the §5.7 CRYPTO_SCRATCH_POOL for `block_size`-byte writes:
    /// buffers are `worst_case(block_size)` rounded up to the next 4 KiB
    /// (≈ `block_size` + 128 KiB for 4 MiB blocks — deliberately not
    /// `ALIGNED_BUF_POOL`, whose exactly-`block_size` buffers cannot hold
    /// worst-case transform output). Idempotent (first init wins); no-op for
    /// passthrough states, which never transform.
    pub fn init_scratch_pool(&self, block_size: usize) {
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

    pub fn decompress<'a>(
        &self,
        data: &'a [u8],
    ) -> Result<std::borrow::Cow<'a, [u8]>, SqueezefsError> {
        match self.compression_mode {
            CompressionMode::Lz4 => {
                let decompressed = lz4_flex::decompress_size_prepended(data).map_err(|e| {
                    SqueezefsError::InvalidOperation(format!("LZ4 decompression failed: {:?}", e))
                })?;
                Ok(std::borrow::Cow::Owned(decompressed))
            }
            CompressionMode::Zstd => {
                let decompressed = zstd::decode_all(std::io::Cursor::new(data)).map_err(|e| {
                    SqueezefsError::InvalidOperation(format!("ZSTD decompression failed: {:?}", e))
                })?;
                Ok(std::borrow::Cow::Owned(decompressed))
            }
            CompressionMode::None => Ok(std::borrow::Cow::Borrowed(data)),
        }
    }

    /// Resolve the RSA-wrapped data-key blob + AEAD key for a write —
    /// session-key fast path (P2-3: reuse RSA-wrapped blob + precomputed
    /// `LessSafeKey`) or the per-write wrap fallback. Shared by the heap
    /// `encrypt` and the §5.7 pooled scratch path.
    fn resolve_encrypt_key(&self) -> Result<(Arc<[u8]>, Arc<LessSafeKey>), SqueezefsError> {
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

        let (wrapped_key, key_bytes) = if let Some((ref wrapped, key)) = self.prewrapped_key {
            (wrapped.clone(), key)
        } else {
            let mut key_bytes = [0u8; 32];
            SystemRandom::new().fill(&mut key_bytes).map_err(|_| {
                SqueezefsError::InvalidOperation("Failed to generate random data key".to_string())
            })?;

            let private_key = self.private_key.as_ref().ok_or_else(|| {
                SqueezefsError::InvalidOperation(
                    "RSA Private Key is required for encryption but not configured".to_string(),
                )
            })?;
            let public_key = private_key.to_public_key();

            let mut rng = rand::thread_rng();
            let wrapped_key = public_key
                .encrypt(&mut rng, rsa::Oaep::new::<sha2::Sha256>(), &key_bytes)
                .map_err(|e| {
                    SqueezefsError::InvalidOperation(format!("RSA key wrap failed: {:?}", e))
                })?;
            (Arc::from(wrapped_key.into_boxed_slice()), key_bytes)
        };

        let algorithm = self.encrypt_mode.algorithm().ok_or_else(|| {
            SqueezefsError::InvalidOperation(format!(
                "Unsupported encryption algo: {}",
                self.encrypt_algo
            ))
        })?;

        let unbound_key = UnboundKey::new(algorithm, &key_bytes).map_err(|_| {
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
            let private_key = self.private_key.as_ref().ok_or_else(|| {
                SqueezefsError::InvalidOperation(
                    "RSA Private Key is required for decryption but not configured".to_string(),
                )
            })?;
            let decrypted_key = private_key
                .decrypt(rsa::Oaep::new::<sha2::Sha256>(), wrapped_key)
                .map_err(|e| {
                    SqueezefsError::InvalidOperation(format!("RSA key unwrap failed: {:?}", e))
                })?;

            let algorithm = self.encrypt_mode.algorithm().ok_or_else(|| {
                SqueezefsError::InvalidOperation(format!(
                    "Unsupported encryption algo: {}",
                    self.encrypt_algo
                ))
            })?;

            let unbound_key = UnboundKey::new(algorithm, &decrypted_key).map_err(|_| {
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

    /// §5.7 pooled transform: compress into the scratch at the sealed-payload
    /// offset, then encrypt **in place within the scratch** — header first,
    /// `seal_in_place_separate_tag` at the offset (a raw fixed scratch has no
    /// `Extend`, so `seal_in_place_append_tag` cannot apply), tag written
    /// after the ciphertext. Exactly one pooled transform buffer total,
    /// returned as `Bytes` over the 4096-aligned backing (recycles when the
    /// last handle drops; the DMA takes `WriteData::Aligned` whenever the
    /// ciphertext lands on a 4 KiB multiple). The input is never mutated.
    fn process_write_pooled(
        &self,
        pool: &Arc<BufferPool>,
        data: &[u8],
    ) -> Result<bytes::Bytes, SqueezefsError> {
        let mut scratch = pool.alloc();
        let total = if self.encrypt_mode != EncryptMode::None {
            let (wrapped_key, less_safe_key) = self.resolve_encrypt_key()?;
            let wrapped_len = wrapped_key.len();
            let header_len = ENCRYPT_HEADER_PREFIX_LEN + wrapped_len + NONCE_LEN;
            let out = scratch.backing_mut();

            let plain_len = self.compress_into_scratch(data, &mut out[header_len..])?;

            // The exact on-disk header `encrypt` emits:
            // [2B wrapped_key_len][1B nonce_len][wrapped_key][nonce].
            out[0] = (wrapped_len >> 8) as u8;
            out[1] = (wrapped_len & 0xFF) as u8;
            out[2] = NONCE_LEN as u8;
            out[ENCRYPT_HEADER_PREFIX_LEN..ENCRYPT_HEADER_PREFIX_LEN + wrapped_len]
                .copy_from_slice(&wrapped_key);
            let nonce_bytes = self.next_nonce_bytes();
            out[ENCRYPT_HEADER_PREFIX_LEN + wrapped_len..header_len].copy_from_slice(&nonce_bytes);
            let nonce = Nonce::try_assume_unique_for_key(&nonce_bytes).map_err(|_| {
                SqueezefsError::InvalidOperation("Failed to construct nonce".to_string())
            })?;

            let tag = less_safe_key
                .seal_in_place_separate_tag(
                    nonce,
                    ring::aead::Aad::empty(),
                    &mut out[header_len..header_len + plain_len],
                )
                .map_err(|_| SqueezefsError::InvalidOperation("AEAD seal failed".to_string()))?;
            let tag_bytes = tag.as_ref();
            out[header_len + plain_len..header_len + plain_len + tag_bytes.len()]
                .copy_from_slice(tag_bytes);
            header_len + plain_len + tag_bytes.len()
        } else {
            self.compress_into_scratch(data, scratch.backing_mut())?
        };
        scratch.set_written_len(total);
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
        if self.encrypt_mode != EncryptMode::None {
            let encrypted = self.encrypt(&compressed)?;
            Ok(bytes::Bytes::from(encrypted))
        } else {
            Ok(bytes::Bytes::from(compressed.into_owned()))
        }
    }

    pub fn process_read<'a>(
        &self,
        data: &'a [u8],
    ) -> Result<std::borrow::Cow<'a, [u8]>, SqueezefsError> {
        if self.is_passthrough() {
            return Ok(std::borrow::Cow::Borrowed(data));
        }
        let decrypted = if self.encrypt_mode != EncryptMode::None {
            Some(self.decrypt(data)?)
        } else {
            None
        };

        match decrypted {
            Some(v) => {
                let decompressed = self.decompress(&v)?;
                Ok(std::borrow::Cow::Owned(decompressed.into_owned()))
            }
            None => self.decompress(data),
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
    use rsa::pkcs1::EncodeRsaPrivateKey;

    #[test]
    fn test_crypto_unwrap_caching() {
        let mut rng = rand::thread_rng();
        let priv_key = RsaPrivateKey::new(&mut rng, 2048).unwrap();
        let pem = priv_key.to_pkcs1_pem(rsa::pkcs1::LineEnding::LF).unwrap();

        let state =
            CryptoCompressState::new("none".to_string(), "aes256gcm-rsa".to_string(), Some(&pem));
        let data = b"some block payload";

        assert!(state.precomputed_encrypt_key.is_some());
        assert!(state.prewrapped_key.is_some());
        assert!(state.aead_tag_len > 0);
        assert_eq!(state.encrypt_mode, EncryptMode::Aes256GcmRsa);

        let encrypted = state.encrypt(data).unwrap();

        // First decryption: should miss cache and decrypt via RSA
        let decrypted1 = state.decrypt(&encrypted).unwrap();
        assert_eq!(decrypted1, data);
        assert_eq!(
            state
                .key_unwrap_count
                .load(std::sync::atomic::Ordering::Relaxed),
            1
        );

        // Second decryption: should hit cache and bypass RSA decryption
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
        let mut rng = rand::thread_rng();
        let priv_key = RsaPrivateKey::new(&mut rng, 2048).unwrap();
        let pem = priv_key.to_pkcs1_pem(rsa::pkcs1::LineEnding::LF).unwrap();
        let state =
            CryptoCompressState::new("none".to_string(), "aes256gcm-rsa".to_string(), Some(&pem));

        let a = state.encrypt(b"block-a").unwrap();
        let b = state.encrypt(b"block-b").unwrap();

        // Same RSA-wrapped session key prefix (2-byte len + wrapped key bytes).
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
            EncryptMode::ChaCha20Rsa
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
    //    RSA-4096 fallback when none exists at pool-init time) + nonce
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

    /// One RSA-2048 keypair for every scratch-pool test (keygen is the
    /// slow part; the tests pin transform behavior, not keygen).
    static TEST_PEM: once_cell::sync::Lazy<String> = once_cell::sync::Lazy::new(|| {
        let mut rng = rand::thread_rng();
        RsaPrivateKey::new(&mut rng, 2048)
            .unwrap()
            .to_pkcs1_pem(rsa::pkcs1::LineEnding::LF)
            .unwrap()
            .to_string()
    });

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
        let state = CryptoCompressState::new(
            "lz4".to_string(),
            "aes256gcm-rsa".to_string(),
            Some(&TEST_PEM),
        );
        let wrapped_len = state
            .prewrapped_key
            .as_ref()
            .expect("session key must prewrap with a private key configured")
            .0
            .len();
        assert_eq!(wrapped_len, 256, "RSA-2048 wrap must be 256 bytes");

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
    fn test_scratch_pool_sizing_falls_back_to_512b_rsa4096_wrap() {
        // Encryption configured but no private key at pool-init time: no
        // prewrapped blob exists, so sizing must assume a 512 B RSA-4096
        // wrap (a 256 B RSA-2048 assumption would silently push
        // 4096-bit-key configs onto the overflow bounce).
        let bs = 256 * 1024;
        let state = CryptoCompressState::new("lz4".to_string(), "aes256gcm-rsa".to_string(), None);
        assert!(state.prewrapped_key.is_none());
        state.init_scratch_pool(bs);
        let compress_term = std::cmp::max(
            4 + lz4_flex::block::get_maximum_output_size(bs),
            zstd::zstd_safe::compress_bound(bs),
        );
        let expected = (compress_term + 3 + 512 + 12 + state.aead_tag_len).next_multiple_of(4096);
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
        let deep = lcg_bytes(bs, 0xD1CE);
        let deep_out = state
            .process_write(bytes::Bytes::from(deep.clone()))
            .unwrap();
        assert_eq!(pool_len(&state), cap0 - 1, "deep write must be pooled");
        assert_eq!(
            deep_out.as_ref(),
            lz4_flex::compress_prepend_size(&deep).as_slice()
        );
        drop(deep_out);
        assert_eq!(pool_len(&state), cap0);

        // Then cycle short writes through every pooled buffer: stale bytes
        // from the deep write must never leak past the written length.
        let short = mixed_payload(32 * 1024);
        let expected = lz4_flex::compress_prepend_size(&short);
        for _ in 0..cap0 {
            let out = state
                .process_write(bytes::Bytes::from(short.clone()))
                .unwrap();
            assert_eq!(pool_len(&state), cap0 - 1, "short write must be pooled");
            assert_eq!(
                out.as_ref(),
                expected.as_slice(),
                "pooled lz4 image must be byte-identical to compress_prepend_size"
            );
            assert_eq!(state.process_read(&out).unwrap().as_ref(), short.as_slice());
        }

        // Empty payload keeps the framing too ([0,0,0,0] prefix).
        let empty_out = state.process_write(bytes::Bytes::new()).unwrap();
        assert_eq!(
            empty_out.as_ref(),
            lz4_flex::compress_prepend_size(&[]).as_slice()
        );
        assert!(state.process_read(&empty_out).unwrap().is_empty());
    }

    #[rstest]
    #[case::lz4("lz4", "none")]
    #[case::zstd("zstd", "none")]
    #[case::aes("none", "aes256gcm-rsa")]
    #[case::chacha("none", "chacha20-rsa")]
    #[case::lz4_aes("lz4", "aes256gcm-rsa")]
    #[case::zstd_chacha("zstd", "chacha20-rsa")]
    fn test_pre_scratch_volume_read_back_parity(#[case] comp: &str, #[case] enc: &str) {
        // A volume written before the scratch sub-commit holds blocks
        // produced by `compress_prepend_size` / `encode_all` / heap
        // `encrypt`. Both directions must hold through the untouched read
        // path: pre-scratch blocks decode on a pool-enabled state, and
        // pooled blocks decode exactly like pre-scratch ones.
        let bs = 128 * 1024;
        let pem = if enc == "none" {
            None
        } else {
            Some(TEST_PEM.as_str())
        };
        let state = CryptoCompressState::new(comp.to_string(), enc.to_string(), pem);
        state.init_scratch_pool(bs);
        let payload = mixed_payload(96 * 1024);

        // Pre-scratch writer image (the exact primitives the old
        // `process_write` used).
        let compressed: Vec<u8> = match comp {
            "lz4" => lz4_flex::compress_prepend_size(&payload),
            "zstd" => zstd::encode_all(std::io::Cursor::new(&payload[..]), 3).unwrap(),
            _ => payload.clone(),
        };
        let old_blob: Vec<u8> = if enc != "none" {
            state.encrypt(&compressed).unwrap()
        } else {
            compressed
        };
        assert_eq!(
            state.process_read(&old_blob).unwrap().as_ref(),
            payload.as_slice(),
            "pre-scratch volume block must decode through the untouched read path"
        );

        // Pooled writer image, decoded by the same untouched read path.
        let cap0 = pool_len(&state);
        let new_blob = state
            .process_write(bytes::Bytes::from(payload.clone()))
            .unwrap();
        assert_eq!(pool_len(&state), cap0 - 1, "transform must be pooled");
        assert_eq!(
            state.process_read(&new_blob).unwrap().as_ref(),
            payload.as_slice(),
            "pooled block must decode through the untouched read path"
        );
        if enc == "none" && comp == "lz4" {
            assert_eq!(new_blob.as_ref(), old_blob.as_slice());
        }
    }

    #[test]
    fn test_scratch_pool_checkout_recycle_and_alignment() {
        let bs = 256 * 1024;
        let state = CryptoCompressState::new(
            "lz4".to_string(),
            "aes256gcm-rsa".to_string(),
            Some(&TEST_PEM),
        );
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
    fn test_incompressible_input_within_bound_stays_pooled() {
        // Incompressible data expands (prefix + literal framing) but stays
        // within the worst-case bound — it must NOT bounce to the heap.
        let bs = 128 * 1024;
        let state = CryptoCompressState::new("lz4".to_string(), "none".to_string(), None);
        state.init_scratch_pool(bs);
        let cap0 = pool_len(&state);

        let payload = lcg_bytes(bs, 0xBAD5EED);
        let out = state
            .process_write(bytes::Bytes::from(payload.clone()))
            .unwrap();
        assert!(
            out.len() > payload.len(),
            "entropy payload must expand under lz4"
        );
        assert_eq!(
            pool_len(&state),
            cap0 - 1,
            "expansion within bound stays pooled"
        );
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
        let state = CryptoCompressState::new(
            "lz4".to_string(),
            "aes256gcm-rsa".to_string(),
            Some(&TEST_PEM),
        );
        state.init_scratch_pool(bs);
        let cap0 = pool_len(&state);

        let oversized = bytes::Bytes::from(mixed_payload(2 * bs));
        let out = state.process_write(oversized.clone()).unwrap();
        assert_eq!(
            pool_len(&state),
            cap0,
            "overflow bounce must not touch the pool"
        );
        assert_eq!(
            state.process_read(&out).unwrap().as_ref(),
            oversized.as_ref()
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
            lz4_flex::compress_prepend_size(&payload).as_slice()
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
        let state = CryptoCompressState::new(
            "zstd".to_string(),
            "chacha20-rsa".to_string(),
            Some(&TEST_PEM),
        );
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
        let state = CryptoCompressState::new(
            "lz4".to_string(),
            "aes256gcm-rsa".to_string(),
            Some(&TEST_PEM),
        );
        state.init_scratch_pool(bs);
        let payload = mixed_payload(64 * 1024);

        let pooled = state
            .process_write(bytes::Bytes::from(payload.clone()))
            .unwrap();
        let compressed = state.compress(&payload).unwrap();
        let heap = state.encrypt(&compressed).unwrap();

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

        // The sealed body opens to the compressed image; the full read
        // path recovers the plaintext for both writers.
        assert_eq!(state.decrypt(&pooled).unwrap().as_slice(), &*compressed);
        assert_eq!(
            state.process_read(&pooled).unwrap().as_ref(),
            payload.as_slice()
        );
        assert_eq!(
            state.process_read(&heap).unwrap().as_ref(),
            payload.as_slice()
        );
    }
}
