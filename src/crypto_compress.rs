use crate::error::SqueezefsError;
use ring::aead::{LessSafeKey, Nonce, UnboundKey, AES_256_GCM, CHACHA20_POLY1305};
use ring::rand::{SecureRandom, SystemRandom};
use rsa::pkcs1::DecodeRsaPrivateKey;
use rsa::RsaPrivateKey;
use std::sync::Arc;

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
        }
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

    pub fn encrypt(&self, data: &[u8]) -> Result<Vec<u8>, SqueezefsError> {
        if self.encrypt_mode == EncryptMode::None {
            return Err(SqueezefsError::InvalidOperation(
                "encrypt() called with encrypt mode none".to_string(),
            ));
        }

        // Session key path (P2-3): reuse RSA-wrapped blob + precomputed LessSafeKey.
        let (wrapped_key, less_safe_key) = if let Some(ref key) = self.precomputed_encrypt_key {
            let wrapped = self
                .prewrapped_key
                .as_ref()
                .map(|(w, _)| w.clone())
                .ok_or_else(|| {
                    SqueezefsError::InvalidOperation(
                        "precomputed encrypt key without prewrapped blob".to_string(),
                    )
                })?;
            (wrapped, key.clone())
        } else {
            let (wrapped_key, key_bytes) = if let Some((ref wrapped, key)) = self.prewrapped_key {
                (wrapped.clone(), key)
            } else {
                let mut key_bytes = [0u8; 32];
                SystemRandom::new().fill(&mut key_bytes).map_err(|_| {
                    SqueezefsError::InvalidOperation(
                        "Failed to generate random data key".to_string(),
                    )
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
            (wrapped_key, Arc::new(LessSafeKey::new(unbound_key)))
        };

        let seq = self
            .nonce_counter
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let mut nonce_bytes = [0u8; 12];
        nonce_bytes[0..4].copy_from_slice(&self.salt);
        nonce_bytes[4..12].copy_from_slice(&seq.to_be_bytes());

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

    pub fn process_write(&self, data: bytes::Bytes) -> Result<bytes::Bytes, SqueezefsError> {
        // P2-3: enum-mode fast path — no string trim/match per block.
        if self.is_passthrough() {
            return Ok(data);
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
}
