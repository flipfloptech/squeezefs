use crate::error::SqueezefsError;
use ring::aead::{LessSafeKey, Nonce, UnboundKey, AES_256_GCM, CHACHA20_POLY1305};
use ring::rand::{SecureRandom, SystemRandom};
use rsa::pkcs1::DecodeRsaPrivateKey;
use rsa::RsaPrivateKey;
use std::sync::Arc;

#[derive(Clone)]
pub struct CryptoCompressState {
    pub compression: String,
    pub encrypt_algo: String,
    pub private_key: Option<Arc<RsaPrivateKey>>,
    pub unwrap_cache: Arc<dashmap::DashMap<Vec<u8>, Vec<u8>, ahash::RandomState>>,
    pub key_unwrap_count: Arc<std::sync::atomic::AtomicUsize>,
}

impl CryptoCompressState {
    pub fn new(compression: String, encrypt_algo: String, private_key_pem: Option<&str>) -> Self {
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

        Self {
            compression,
            encrypt_algo,
            private_key,
            unwrap_cache: Arc::new(dashmap::DashMap::with_hasher(ahash::RandomState::new())),
            key_unwrap_count: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
        }
    }

    pub fn compress(&self, data: &[u8]) -> Result<Vec<u8>, SqueezefsError> {
        match self.compression.as_str() {
            "lz4" => {
                let compressed = lz4_flex::compress_prepend_size(data);
                Ok(compressed)
            }
            "zstd" => {
                let compressed = zstd::encode_all(std::io::Cursor::new(data), 3).map_err(|e| {
                    SqueezefsError::InvalidOperation(format!("ZSTD compression failed: {:?}", e))
                })?;
                Ok(compressed)
            }
            "none" | "" => Ok(data.to_vec()),
            _ => Err(SqueezefsError::InvalidOperation(format!(
                "Unsupported compression algorithm: {}",
                self.compression
            ))),
        }
    }

    pub fn decompress(&self, data: &[u8]) -> Result<Vec<u8>, SqueezefsError> {
        match self.compression.as_str() {
            "lz4" => {
                let decompressed = lz4_flex::decompress_size_prepended(data).map_err(|e| {
                    SqueezefsError::InvalidOperation(format!("LZ4 decompression failed: {:?}", e))
                })?;
                Ok(decompressed)
            }
            "zstd" => {
                let decompressed = zstd::decode_all(std::io::Cursor::new(data)).map_err(|e| {
                    SqueezefsError::InvalidOperation(format!("ZSTD decompression failed: {:?}", e))
                })?;
                Ok(decompressed)
            }
            "none" | "" => Ok(data.to_vec()),
            _ => Err(SqueezefsError::InvalidOperation(format!(
                "Unsupported compression algorithm: {}",
                self.compression
            ))),
        }
    }

    pub fn encrypt(&self, data: &[u8]) -> Result<Vec<u8>, SqueezefsError> {
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

        let mut nonce_bytes = [0u8; 12];
        SystemRandom::new().fill(&mut nonce_bytes).map_err(|_| {
            SqueezefsError::InvalidOperation("Failed to generate random nonce".to_string())
        })?;

        let algorithm = match self.encrypt_algo.as_str() {
            "aes256gcm-rsa" => &AES_256_GCM,
            "chacha20-rsa" => &CHACHA20_POLY1305,
            _ => {
                return Err(SqueezefsError::InvalidOperation(format!(
                    "Unsupported encryption algo: {}",
                    self.encrypt_algo
                )))
            }
        };

        let unbound_key = UnboundKey::new(algorithm, &key_bytes).map_err(|_| {
            SqueezefsError::InvalidOperation("Failed to create unbound key".to_string())
        })?;
        let less_safe_key = LessSafeKey::new(unbound_key);
        let nonce = Nonce::try_assume_unique_for_key(&nonce_bytes).map_err(|_| {
            SqueezefsError::InvalidOperation("Failed to construct nonce".to_string())
        })?;

        let mut in_out = data.to_vec();
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

        let key_bytes = if let Some(cached_key) = self.unwrap_cache.get(wrapped_key) {
            cached_key.value().clone()
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
            self.unwrap_cache
                .insert(wrapped_key.to_vec(), decrypted_key.clone());
            self.key_unwrap_count
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            decrypted_key
        };

        let algorithm = match self.encrypt_algo.as_str() {
            "aes256gcm-rsa" => &AES_256_GCM,
            "chacha20-rsa" => &CHACHA20_POLY1305,
            _ => {
                return Err(SqueezefsError::InvalidOperation(format!(
                    "Unsupported encryption algo: {}",
                    self.encrypt_algo
                )))
            }
        };

        let unbound_key = UnboundKey::new(algorithm, &key_bytes).map_err(|_| {
            SqueezefsError::InvalidOperation("Failed to create unbound key".to_string())
        })?;
        let less_safe_key = LessSafeKey::new(unbound_key);
        let nonce = Nonce::try_assume_unique_for_key(nonce_bytes).map_err(|_| {
            SqueezefsError::InvalidOperation("Failed to construct nonce".to_string())
        })?;

        let mut in_out = ciphertext_payload.to_vec();
        let decrypted_slice = less_safe_key
            .open_in_place(nonce, ring::aead::Aad::empty(), &mut in_out)
            .map_err(|_| SqueezefsError::InvalidOperation("AEAD open failed".to_string()))?;

        Ok(decrypted_slice.to_vec())
    }

    pub fn process_write(&self, data: &[u8]) -> Result<Vec<u8>, SqueezefsError> {
        let compressed = self.compress(data)?;
        if self.encrypt_algo != "none" && !self.encrypt_algo.is_empty() {
            self.encrypt(&compressed)
        } else {
            Ok(compressed)
        }
    }

    pub fn process_read(&self, data: &[u8]) -> Result<Vec<u8>, SqueezefsError> {
        let decrypted = if self.encrypt_algo != "none" && !self.encrypt_algo.is_empty() {
            self.decrypt(data)?
        } else {
            data.to_vec()
        };
        self.decompress(&decrypted)
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
}
