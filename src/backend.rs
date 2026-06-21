use crate::error::{Result, SqueezefsError};
use aws_sdk_s3::primitives::ByteStream;
use aws_sdk_s3::Client as S3Client;
use dashmap::DashMap;
use log::{info, warn};
use std::sync::Arc;

#[derive(Clone)]
pub struct RustFsClient {
    s3_client: Option<S3Client>,
    bucket: String,
    #[allow(clippy::type_complexity)]
    mock_store: Option<Arc<DashMap<String, (Vec<u8>, u64)>>>,
}

impl RustFsClient {
    /// Create a new S3-compatible client.
    /// If environment variables are not set or connection fails, falls back to mock in-memory store.
    pub async fn new() -> Self {
        let endpoint = std::env::var("RUSTFS_ENDPOINT").ok();
        let access_key = std::env::var("RUSTFS_ACCESS_KEY").unwrap_or_else(|_| "admin".to_string());
        let secret_key =
            std::env::var("RUSTFS_SECRET_KEY").unwrap_or_else(|_| "password".to_string());
        let bucket =
            std::env::var("RUSTFS_BUCKET").unwrap_or_else(|_| "squeezefs-data".to_string());

        if let Some(endpoint_url) = endpoint {
            info!("Initializing S3 client targeting: {}", endpoint_url);
            let credentials = aws_sdk_s3::config::Credentials::new(
                access_key,
                secret_key,
                None,
                None,
                "Environment",
            );

            let config = aws_config::defaults(aws_config::BehaviorVersion::latest())
                .credentials_provider(credentials)
                .endpoint_url(endpoint_url)
                .region(aws_sdk_s3::config::Region::new("us-east-1"))
                .load()
                .await;

            let s3_config = aws_sdk_s3::config::Builder::from(&config)
                .force_path_style(true)
                .build();

            let s3_client = S3Client::from_conf(s3_config);

            // Verify bucket initialization in the background
            let client = Self {
                s3_client: Some(s3_client),
                bucket: bucket.clone(),
                mock_store: None,
            };

            match client.init_bucket().await {
                Ok(_) => {
                    info!("Successfully connected to S3-compatible backend bucket.");
                    return client;
                }
                Err(e) => {
                    warn!("Failed to initialize S3 bucket ({:?}). Falling back to in-memory mock backend.", e);
                }
            }
        }

        warn!("No S3 endpoint provided or bucket init failed. Running with in-memory mock store.");
        Self {
            s3_client: None,
            bucket,
            mock_store: Some(Arc::new(DashMap::new())),
        }
    }

    /// Construct an explicit mock store client.
    pub fn new_mock() -> Self {
        Self {
            s3_client: None,
            bucket: "mock-bucket".to_string(),
            mock_store: Some(Arc::new(DashMap::new())),
        }
    }

    async fn init_bucket(&self) -> Result<()> {
        if let Some(s3) = &self.s3_client {
            let exists = s3.head_bucket().bucket(&self.bucket).send().await;
            if exists.is_err() {
                info!("Bucket {} does not exist, creating it.", self.bucket);
                s3.create_bucket()
                    .bucket(&self.bucket)
                    .send()
                    .await
                    .map_err(|e| SqueezefsError::S3(format!("Failed to create bucket: {:?}", e)))?;
            }
        }
        Ok(())
    }

    /// Upload data to S3.
    /// Before uploading, checks if a newer fencing token has already been written.
    pub async fn put_object(&self, key: &str, data: Vec<u8>, fencing_token: u64) -> Result<()> {
        if let Some(store) = &self.mock_store {
            if let Some(existing) = store.get(key) {
                let (_, existing_token) = *existing;
                if fencing_token <= existing_token {
                    return Err(SqueezefsError::FencingTokenExpired {
                        token: fencing_token,
                        expected: existing_token + 1,
                    });
                }
            }
            store.insert(key.to_string(), (data, fencing_token));
            return Ok(());
        }

        let s3 = self.s3_client.as_ref().ok_or_else(|| {
            SqueezefsError::InvalidOperation("No S3 client initialized".to_string())
        })?;

        // 1. Fetch current object metadata to check fencing token
        if let Ok(head_output) = s3.head_object().bucket(&self.bucket).key(key).send().await {
            if let Some(metadata) = head_output.metadata() {
                if let Some(existing_token_str) = metadata.get("fencing-token") {
                    if let Ok(existing_token) = existing_token_str.parse::<u64>() {
                        if fencing_token <= existing_token {
                            return Err(SqueezefsError::FencingTokenExpired {
                                token: fencing_token,
                                expected: existing_token + 1,
                            });
                        }
                    }
                }
            }
        }

        // 2. Perform write
        let body = ByteStream::from(data);
        s3.put_object()
            .bucket(&self.bucket)
            .key(key)
            .body(body)
            .metadata("fencing-token", fencing_token.to_string())
            .send()
            .await
            .map_err(|e| SqueezefsError::S3(format!("S3 PUT failed: {:?}", e)))?;

        Ok(())
    }

    /// Download data from S3.
    pub async fn get_object(&self, key: &str) -> Result<Vec<u8>> {
        if let Some(store) = &self.mock_store {
            if let Some(val) = store.get(key) {
                return Ok(val.0.clone());
            } else {
                return Err(SqueezefsError::Io(std::io::Error::new(
                    std::io::ErrorKind::NotFound,
                    format!("object {} not found", key),
                )));
            }
        }

        let s3 = self.s3_client.as_ref().ok_or_else(|| {
            SqueezefsError::InvalidOperation("No S3 client initialized".to_string())
        })?;

        let resp = s3
            .get_object()
            .bucket(&self.bucket)
            .key(key)
            .send()
            .await
            .map_err(|e| {
                SqueezefsError::Io(std::io::Error::new(
                    std::io::ErrorKind::NotFound,
                    format!("S3 GET failed: {:?}", e),
                ))
            })?;

        let bytes = resp.body.collect().await.map_err(|e| {
            SqueezefsError::Io(std::io::Error::other(format!(
                "Failed to read body stream: {:?}",
                e
            )))
        })?;

        Ok(bytes.to_vec())
    }

    /// Delete an object from S3.
    pub async fn delete_object(&self, key: &str) -> Result<()> {
        if let Some(store) = &self.mock_store {
            store.remove(key);
            return Ok(());
        }

        let s3 = self.s3_client.as_ref().ok_or_else(|| {
            SqueezefsError::InvalidOperation("No S3 client initialized".to_string())
        })?;

        s3.delete_object()
            .bucket(&self.bucket)
            .key(key)
            .send()
            .await
            .map_err(|e| SqueezefsError::S3(format!("S3 DELETE failed: {:?}", e)))?;

        Ok(())
    }
}
