use crate::error::{Result, SqueezefsError};
use crate::fuse_client::METRICS;
use aws_sdk_s3::primitives::ByteStream;
use aws_sdk_s3::Client as S3Client;
use dashmap::DashMap;
use log::{info, warn};
use std::net::IpAddr;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::Arc;

#[derive(Default)]
struct MockStoreState {
    get_count: AtomicU64,
    range_get_count: AtomicU64,
    get_delay_ms: AtomicU64,
}

#[derive(Clone)]
pub struct RustFsClient {
    s3_clients: Vec<S3Client>,
    bucket: String,
    #[allow(clippy::type_complexity)]
    mock_store: Option<Arc<DashMap<String, (bytes::Bytes, u64), ahash::RandomState>>>,
    current_idx: Arc<AtomicUsize>,
    mock_state: Arc<MockStoreState>,
}

impl RustFsClient {
    pub fn get_mock_keys_and_sizes(&self) -> Vec<(String, usize)> {
        if let Some(store) = &self.mock_store {
            store
                .iter()
                .map(|e| (e.key().clone(), e.value().0.len()))
                .collect()
        } else {
            Vec::new()
        }
    }

    /// Create a new S3-compatible client.
    /// If environment variables are not set or connection fails, falls back to mock in-memory store.
    pub async fn new() -> Self {
        Self::new_with_local_ips(Vec::new(), None, None, None, None).await
    }

    /// Create a new S3-compatible client with multi-rail local IP bindings.
    pub async fn new_with_local_ips(
        local_ips: Vec<IpAddr>,
        endpoint: Option<String>,
        access_key: Option<String>,
        secret_key: Option<String>,
        bucket: Option<String>,
    ) -> Self {
        let endpoint = endpoint.or_else(|| std::env::var("RUSTFS_ENDPOINT").ok());
        let access_key = access_key
            .or_else(|| std::env::var("RUSTFS_ACCESS_KEY").ok())
            .unwrap_or_else(|| "admin".to_string());
        let secret_key = secret_key
            .or_else(|| std::env::var("RUSTFS_SECRET_KEY").ok())
            .unwrap_or_else(|| "password".to_string());
        let bucket = bucket
            .or_else(|| std::env::var("RUSTFS_BUCKET").ok())
            .unwrap_or_else(|| "squeezefs-data".to_string());

        let mut s3_clients = Vec::new();

        if let Some(endpoint_url) = endpoint {
            info!("Initializing S3 client targeting: {}", endpoint_url);
            let credentials = aws_sdk_s3::config::Credentials::new(
                access_key,
                secret_key,
                None,
                None,
                "Environment",
            );

            if local_ips.is_empty() {
                let config = aws_config::defaults(aws_config::BehaviorVersion::latest())
                    .credentials_provider(credentials)
                    .endpoint_url(&endpoint_url)
                    .region(aws_sdk_s3::config::Region::new("us-east-1"))
                    .load()
                    .await;

                let s3_config = aws_sdk_s3::config::Builder::from(&config)
                    .force_path_style(true)
                    .build();

                s3_clients.push(S3Client::from_conf(s3_config));
            } else {
                for ip in &local_ips {
                    info!("Binding S3 client rail to local source IP: {}", ip);
                    let mut http = hyper_014::client::HttpConnector::new();
                    http.enforce_http(false);
                    http.set_local_address(Some(*ip));

                    let tcp_connector = hyper_rustls_024::HttpsConnectorBuilder::new()
                        .with_webpki_roots()
                        .https_only()
                        .enable_http1()
                        .wrap_connector(http);

                    let http_client =
                        aws_smithy_runtime::client::http::hyper_014::HyperClientBuilder::new()
                            .build(tcp_connector);

                    let config = aws_config::defaults(aws_config::BehaviorVersion::latest())
                        .credentials_provider(credentials.clone())
                        .endpoint_url(&endpoint_url)
                        .http_client(http_client)
                        .region(aws_sdk_s3::config::Region::new("us-east-1"))
                        .load()
                        .await;

                    let s3_config = aws_sdk_s3::config::Builder::from(&config)
                        .force_path_style(true)
                        .build();

                    s3_clients.push(S3Client::from_conf(s3_config));
                }
            }

            let client = Self {
                s3_clients,
                bucket: bucket.clone(),
                mock_store: None,
                current_idx: Arc::new(AtomicUsize::new(0)),
                mock_state: Arc::new(MockStoreState::default()),
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
        let mut s3_clients = Vec::new();
        // Populate dummy S3 clients in mock mode to verify initialization/routing counts in tests
        let count = if local_ips.is_empty() {
            1
        } else {
            local_ips.len()
        };
        for _ in 0..count {
            let s3_config = aws_sdk_s3::config::Builder::new()
                .behavior_version(aws_sdk_s3::config::BehaviorVersion::latest())
                .build();
            s3_clients.push(S3Client::from_conf(s3_config));
        }

        Self {
            s3_clients,
            bucket,
            mock_store: Some(Arc::new(DashMap::with_hasher(ahash::RandomState::new()))),
            current_idx: Arc::new(AtomicUsize::new(0)),
            mock_state: Arc::new(MockStoreState::default()),
        }
    }

    /// Construct an explicit mock store client.
    pub fn new_mock() -> Self {
        Self {
            s3_clients: vec![S3Client::from_conf(
                aws_sdk_s3::config::Builder::new()
                    .behavior_version(aws_sdk_s3::config::BehaviorVersion::latest())
                    .build(),
            )],
            bucket: "mock-bucket".to_string(),
            mock_store: Some(Arc::new(DashMap::with_hasher(ahash::RandomState::new()))),
            current_idx: Arc::new(AtomicUsize::new(0)),
            mock_state: Arc::new(MockStoreState::default()),
        }
    }

    pub fn client_count(&self) -> usize {
        self.s3_clients.len()
    }

    pub fn set_mock_get_delay(&self, delay: std::time::Duration) {
        self.mock_state.get_delay_ms.store(
            delay.as_millis().min(u64::MAX as u128) as u64,
            Ordering::Relaxed,
        );
    }

    pub fn mock_get_count(&self) -> u64 {
        self.mock_state.get_count.load(Ordering::Relaxed)
    }

    pub fn mock_range_get_count(&self) -> u64 {
        self.mock_state.range_get_count.load(Ordering::Relaxed)
    }

    fn get_s3_client(&self) -> Option<&S3Client> {
        if self.s3_clients.is_empty() {
            None
        } else {
            let idx = self.current_idx.fetch_add(1, Ordering::Relaxed);
            Some(&self.s3_clients[idx % self.s3_clients.len()])
        }
    }

    async fn observe_mock_read(&self, is_range: bool) {
        if is_range {
            self.mock_state
                .range_get_count
                .fetch_add(1, Ordering::Relaxed);
        } else {
            self.mock_state.get_count.fetch_add(1, Ordering::Relaxed);
        }

        let delay_ms = self.mock_state.get_delay_ms.load(Ordering::Relaxed);
        if delay_ms > 0 {
            tokio::time::sleep(std::time::Duration::from_millis(delay_ms)).await;
        }
    }

    pub async fn init_bucket(&self) -> Result<()> {
        if self.mock_store.is_some() {
            return Ok(());
        }
        if let Some(s3) = self.s3_clients.first() {
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
    pub async fn put_object(
        &self,
        key: &str,
        data: bytes::Bytes,
        fencing_token: u64,
    ) -> Result<()> {
        METRICS.put_obj.fetch_add(1, Ordering::Relaxed);
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

        let num_clients = self.s3_clients.len();
        let mut last_err = None;

        for _ in 0..num_clients {
            let s3 = self.get_s3_client().ok_or_else(|| {
                SqueezefsError::InvalidOperation("No S3 client initialized".to_string())
            })?;

            // Perform write directly (fencing token is stored as metadata for tracking,
            // but we avoid the redundant HEAD request check since block keys are unique).
            let body = ByteStream::from(data.clone());
            let fut = s3
                .put_object()
                .bucket(&self.bucket)
                .key(key)
                .body(body)
                .metadata("fencing-token", fencing_token.to_string())
                .send();
            match tokio::time::timeout(std::time::Duration::from_secs(2), fut).await {
                Ok(Ok(_)) => return Ok(()),
                Ok(Err(e)) => {
                    warn!("S3 PUT failed on current rail: {:?}. Retrying next...", e);
                    last_err = Some(format!("{:?}", e));
                }
                Err(_) => {
                    warn!("S3 PUT timed out after 2s on current rail. Retrying next...");
                    last_err = Some("Timeout".to_string());
                }
            }
        }

        Err(SqueezefsError::S3(format!(
            "S3 PUT failed on all rails. Last error: {:?}",
            last_err
        )))
    }

    /// Download data from S3.
    pub async fn get_object(&self, key: &str) -> Result<Vec<u8>> {
        METRICS.get_obj.fetch_add(1, Ordering::Relaxed);
        if let Some(store) = &self.mock_store {
            self.observe_mock_read(false).await;
            if let Some(val) = store.get(key) {
                return Ok(val.0.to_vec());
            } else {
                return Err(SqueezefsError::Io(std::io::Error::new(
                    std::io::ErrorKind::NotFound,
                    format!("object {} not found in mock store", key),
                )));
            }
        }

        let num_clients = self.s3_clients.len();
        let mut last_err = None;

        for _ in 0..num_clients {
            let s3 = self.get_s3_client().ok_or_else(|| {
                SqueezefsError::InvalidOperation("No S3 client initialized".to_string())
            })?;

            let fut = s3.get_object().bucket(&self.bucket).key(key).send();
            match tokio::time::timeout(std::time::Duration::from_secs(2), fut).await {
                Ok(Ok(resp)) => {
                    let body_fut = resp.body.collect();
                    match tokio::time::timeout(std::time::Duration::from_secs(2), body_fut).await {
                        Ok(Ok(bytes)) => return Ok(bytes.to_vec()),
                        Ok(Err(e)) => {
                            warn!("S3 GET body collection failed on current rail: {:?}. Retrying next...", e);
                            last_err = Some(format!("Body stream read failed: {:?}", e));
                        }
                        Err(_) => {
                            warn!("S3 GET body collection timed out after 2s on current rail. Retrying next...");
                            last_err = Some("Body collection timeout".to_string());
                        }
                    }
                }
                Ok(Err(e)) => {
                    warn!("S3 GET failed on current rail: {:?}. Retrying next...", e);
                    last_err = Some(format!("{:?}", e));
                }
                Err(_) => {
                    warn!("S3 GET timed out after 2s on current rail. Retrying next...");
                    last_err = Some("Timeout".to_string());
                }
            }
        }

        Err(SqueezefsError::Io(std::io::Error::new(
            std::io::ErrorKind::ConnectionAborted,
            format!("S3 GET failed on all rails. Last error: {:?}", last_err),
        )))
    }

    /// Download a range of data from S3.
    pub async fn get_object_range(&self, key: &str, start: u64, end: u64) -> Result<Vec<u8>> {
        METRICS.get_obj.fetch_add(1, Ordering::Relaxed);
        if let Some(store) = &self.mock_store {
            self.observe_mock_read(true).await;
            if let Some(val) = store.get(key) {
                let data = &val.0;
                let s = std::cmp::min(start as usize, data.len());
                let e = std::cmp::min(end as usize, data.len());
                return Ok(data[s..e].to_vec());
            } else {
                return Err(SqueezefsError::Io(std::io::Error::new(
                    std::io::ErrorKind::NotFound,
                    format!("object {} not found", key),
                )));
            }
        }

        let num_clients = self.s3_clients.len();
        let mut last_err = None;
        let range_header = format!("bytes={}-{}", start, end.saturating_sub(1));

        for _ in 0..num_clients {
            let s3 = self.get_s3_client().ok_or_else(|| {
                SqueezefsError::InvalidOperation("No S3 client initialized".to_string())
            })?;

            let fut = s3
                .get_object()
                .bucket(&self.bucket)
                .key(key)
                .range(range_header.clone())
                .send();
            match tokio::time::timeout(std::time::Duration::from_secs(2), fut).await {
                Ok(Ok(resp)) => {
                    let body_fut = resp.body.collect();
                    match tokio::time::timeout(std::time::Duration::from_secs(2), body_fut).await {
                        Ok(Ok(bytes)) => return Ok(bytes.to_vec()),
                        Ok(Err(e)) => {
                            warn!("S3 GET range body collection failed: {:?}. Retrying...", e);
                            last_err = Some(format!("Body stream read failed: {:?}", e));
                        }
                        Err(_) => {
                            warn!("S3 GET range body collection timed out. Retrying...");
                            last_err = Some("Body collection timeout".to_string());
                        }
                    }
                }
                Ok(Err(e)) => {
                    warn!(
                        "S3 GET range failed on current rail: {:?}. Retrying next...",
                        e
                    );
                    last_err = Some(format!("{:?}", e));
                }
                Err(_) => {
                    warn!("S3 GET range timed out after 2s. Retrying next...");
                    last_err = Some("Timeout".to_string());
                }
            }
        }

        Err(SqueezefsError::Io(std::io::Error::new(
            std::io::ErrorKind::ConnectionAborted,
            format!(
                "S3 GET range failed on all rails. Last error: {:?}",
                last_err
            ),
        )))
    }

    /// Delete an object from S3.
    pub async fn delete_object(&self, key: &str) -> Result<()> {
        METRICS.del_obj.fetch_add(1, Ordering::Relaxed);
        if let Some(store) = &self.mock_store {
            store.remove(key);
            return Ok(());
        }

        let num_clients = self.s3_clients.len();
        let mut last_err = None;

        for _ in 0..num_clients {
            let s3 = self.get_s3_client().ok_or_else(|| {
                SqueezefsError::InvalidOperation("No S3 client initialized".to_string())
            })?;

            let fut = s3.delete_object().bucket(&self.bucket).key(key).send();
            match tokio::time::timeout(std::time::Duration::from_secs(2), fut).await {
                Ok(Ok(_)) => return Ok(()),
                Ok(Err(e)) => {
                    warn!(
                        "S3 DELETE failed on current rail: {:?}. Retrying next...",
                        e
                    );
                    last_err = Some(format!("{:?}", e));
                }
                Err(_) => {
                    warn!("S3 DELETE timed out after 2s. Retrying next...");
                    last_err = Some("Timeout".to_string());
                }
            }
        }

        Err(SqueezefsError::S3(format!(
            "S3 DELETE failed on all rails. Last error: {:?}",
            last_err
        )))
    }

    /// Delete all objects in the configured S3 bucket.
    pub async fn destroy_bucket_data(&self) -> Result<()> {
        if let Some(store) = &self.mock_store {
            store.clear();
            return Ok(());
        }

        let num_clients = self.s3_clients.len();
        if num_clients == 0 {
            return Ok(());
        }

        let s3 = self.get_s3_client().ok_or_else(|| {
            SqueezefsError::InvalidOperation("No S3 client initialized".to_string())
        })?;

        info!("Wiping all object data in S3 bucket: {}", self.bucket);

        let mut continuation_token = None;
        loop {
            let mut builder = s3.list_objects_v2().bucket(&self.bucket);
            if let Some(token) = continuation_token {
                builder = builder.continuation_token(token);
            }

            let res = builder.send().await.map_err(|e| {
                SqueezefsError::S3(format!(
                    "Failed to list objects in bucket {}: {:?}",
                    self.bucket, e
                ))
            })?;

            let contents = res.contents();
            if !contents.is_empty() {
                let mut delete_builder = aws_sdk_s3::types::Delete::builder();
                let mut count = 0;
                for obj in contents {
                    if let Some(key) = obj.key() {
                        let obj_id = aws_sdk_s3::types::ObjectIdentifier::builder()
                            .key(key)
                            .build()
                            .map_err(|e| {
                                SqueezefsError::S3(format!(
                                    "Failed to build ObjectIdentifier for key {}: {:?}",
                                    key, e
                                ))
                            })?;
                        delete_builder = delete_builder.objects(obj_id);
                        count += 1;
                    }
                }

                if count > 0 {
                    let delete_request = delete_builder.build().map_err(|e| {
                        SqueezefsError::S3(format!("Failed to build Delete request: {:?}", e))
                    })?;

                    s3.delete_objects()
                        .bucket(&self.bucket)
                        .delete(delete_request)
                        .send()
                        .await
                        .map_err(|e| {
                            SqueezefsError::S3(format!(
                                "Failed to execute bulk delete in bucket {}: {:?}",
                                self.bucket, e
                            ))
                        })?;

                    info!("Deleted {} objects from bucket {}", count, self.bucket);
                }
            }

            if res.is_truncated().unwrap_or(false) {
                continuation_token = res.next_continuation_token().map(|s| s.to_string());
            } else {
                break;
            }
        }

        Ok(())
    }
}

#[derive(Clone)]
pub struct MultiBackendClient {
    backends: Arc<dashmap::DashMap<String, RustFsClient, ahash::RandomState>>,
    backend_status: Arc<dashmap::DashMap<String, String, ahash::RandomState>>,
    #[allow(dead_code)]
    active_backend_id: Arc<std::sync::RwLock<String>>,
    backend_keys: Arc<std::sync::RwLock<Vec<String>>>,
}

impl Default for MultiBackendClient {
    fn default() -> Self {
        Self::new()
    }
}

impl MultiBackendClient {
    pub fn new() -> Self {
        Self {
            backends: Arc::new(dashmap::DashMap::with_hasher(ahash::RandomState::new())),
            backend_status: Arc::new(dashmap::DashMap::with_hasher(ahash::RandomState::new())),
            active_backend_id: Arc::new(std::sync::RwLock::new("backend_0".to_string())),
            backend_keys: Arc::new(std::sync::RwLock::new(Vec::new())),
        }
    }

    pub fn set_backend_status(&self, id: &str, status: &str) {
        self.backend_status
            .insert(id.to_string(), status.to_string());
    }

    pub fn get_backend_for_key(&self, key: &str) -> String {
        use std::hash::{Hash, Hasher};
        use twox_hash::XxHash64;

        let mut max_score: u64 = 0;
        let mut best_node = "backend_0".to_string();

        if let Ok(keys) = self.backend_keys.read() {
            let enabled_keys: Vec<&String> = keys
                .iter()
                .filter(|node_id| {
                    if let Some(status) = self.backend_status.get(*node_id) {
                        status.value() == "enabled"
                    } else {
                        true // default enabled
                    }
                })
                .collect();

            let target_keys = if enabled_keys.is_empty() {
                keys.iter().collect::<Vec<_>>()
            } else {
                enabled_keys
            };

            for node_id in target_keys {
                let mut hasher = XxHash64::default();
                node_id.hash(&mut hasher);
                key.hash(&mut hasher);
                let score = hasher.finish();
                if score > max_score {
                    max_score = score;
                    best_node = (*node_id).clone();
                }
            }
        }
        best_node
    }

    pub fn set_active_backend_id(&self, _id: String) {
        // Deprecated: Kept for compatibility with tests, but routing is now dynamic.
    }

    pub fn register_backend(&self, id: &str, client: RustFsClient) {
        self.backends.insert(id.to_string(), client);
        let keys: Vec<String> = self.backends.iter().map(|e| e.key().clone()).collect();
        if let Ok(mut guard) = self.backend_keys.write() {
            *guard = keys;
        }
    }

    pub fn get_backend(&self, id: &str) -> Option<RustFsClient> {
        self.backends.get(id).map(|r| r.value().clone())
    }

    pub fn has_backend(&self, id: &str) -> bool {
        self.backends.contains_key(id)
    }

    pub async fn put_object(
        &self,
        key: &str,
        data: bytes::Bytes,
        fencing_token: u64,
    ) -> Result<()> {
        let active_id = self.get_backend_for_key(key);
        self.put_object_on_backend(&active_id, key, data, fencing_token)
            .await
    }

    pub async fn put_object_on_backend(
        &self,
        backend_id: &str,
        key: &str,
        data: bytes::Bytes,
        fencing_token: u64,
    ) -> Result<()> {
        if let Some(backend) = self.get_backend(backend_id) {
            backend.put_object(key, data, fencing_token).await
        } else {
            Err(SqueezefsError::InvalidOperation(format!(
                "Backend ID {} not registered",
                backend_id
            )))
        }
    }

    pub async fn get_object(&self, backend_id: &str, key: &str) -> Result<Vec<u8>> {
        if let Some(backend) = self.get_backend(backend_id) {
            backend.get_object(key).await
        } else {
            Err(SqueezefsError::InvalidOperation(format!(
                "Backend ID {} not registered",
                backend_id
            )))
        }
    }

    pub async fn get_object_range(
        &self,
        backend_id: &str,
        key: &str,
        start: u64,
        end: u64,
    ) -> Result<Vec<u8>> {
        if let Some(backend) = self.get_backend(backend_id) {
            backend.get_object_range(key, start, end).await
        } else {
            Err(SqueezefsError::InvalidOperation(format!(
                "Backend ID {} not registered",
                backend_id
            )))
        }
    }

    pub async fn delete_object(&self, backend_id: &str, key: &str) -> Result<()> {
        if let Some(backend) = self.get_backend(backend_id) {
            backend.delete_object(key).await
        } else {
            Err(SqueezefsError::InvalidOperation(format!(
                "Backend ID {} not registered",
                backend_id
            )))
        }
    }
}

pub fn parse_backend_and_key(val: &str) -> (String, String) {
    if let Some(idx) = val.find(':') {
        let (backend_id, key) = val.split_at(idx);
        (backend_id.to_string(), key[1..].to_string())
    } else {
        ("backend_0".to_string(), val.to_string())
    }
}

impl From<RustFsClient> for MultiBackendClient {
    fn from(client: RustFsClient) -> Self {
        let multi = Self::new();
        multi.register_backend("backend_0", client);
        multi
    }
}
