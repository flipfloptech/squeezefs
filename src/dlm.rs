use crate::error::Result;
use once_cell::sync::Lazy;
use parking_lot::Mutex;
use std::collections::HashMap;
use std::net::IpAddr;
use std::sync::Arc;
use std::time::Duration;

static LOCK_MAP: Lazy<Mutex<HashMap<String, String>>> = Lazy::new(|| Mutex::new(HashMap::new()));
static FENCING_MAP: Lazy<Mutex<HashMap<String, u64>>> = Lazy::new(|| Mutex::new(HashMap::new()));

#[derive(Clone)]
pub struct BoundConnection {}

#[derive(Clone)]
pub struct MetaConnection {}

#[derive(Clone)]
pub enum MetaClient {
    Local,
}

impl MetaClient {
    pub fn new(_redis_url: &str) -> Result<Self> {
        Ok(Self::Local)
    }
    pub async fn new_with_local_ips(_redis_url: &str, _local_ips: Vec<IpAddr>) -> Result<Self> {
        Ok(Self::Local)
    }
    pub async fn get_connection(&self) -> Result<MetaConnection> {
        Ok(MetaConnection {})
    }
    pub async fn get_connection_for_inode(&self, _ino: u64) -> Result<MetaConnection> {
        Ok(MetaConnection {})
    }
    pub async fn get_connection_for_key(&self, _key: &str) -> Result<MetaConnection> {
        Ok(MetaConnection {})
    }
    pub fn shard_count(&self) -> usize {
        1
    }
}

#[derive(Clone)]
pub struct DlmClient {
    client_id: String,
    redis_url: String,
    meta_client: Arc<MetaClient>,
}

impl DlmClient {
    pub fn new(redis_url: &str) -> Result<Self> {
        let client_id = format!("local_dlm_client_{}", uuid::Uuid::new_v4());
        Ok(Self {
            client_id,
            redis_url: redis_url.to_string(),
            meta_client: Arc::new(MetaClient::Local),
        })
    }

    pub async fn new_with_local_ips(redis_url: &str, _local_ips: Vec<IpAddr>) -> Result<Self> {
        Self::new(redis_url)
    }

    pub fn client_id(&self) -> &str {
        &self.client_id
    }

    pub fn redis_url(&self) -> &str {
        &self.redis_url
    }

    pub fn meta_client(&self) -> Arc<MetaClient> {
        self.meta_client.clone()
    }

    pub fn connection_count(&self) -> usize {
        1
    }

    pub fn shard_count(&self) -> usize {
        1
    }

    pub async fn get_connection(&self) -> Result<MetaConnection> {
        Ok(MetaConnection {})
    }

    pub async fn get_connection_for_inode(&self, _ino: u64) -> Result<MetaConnection> {
        Ok(MetaConnection {})
    }

    pub async fn get_connection_for_key(&self, _key: &str) -> Result<MetaConnection> {
        Ok(MetaConnection {})
    }
    pub fn get_fencing_token(&self, file_path: &str) -> u64 {
        let gen_key = format!("fencing_generator:{}", file_path);
        let map = FENCING_MAP.lock();
        map.get(&gen_key).copied().unwrap_or(0)
    }

    pub async fn get_pubsub_connection(&self) -> Result<MockPubSub> {
        Ok(MockPubSub {})
    }

    pub async fn publish_recall(&self, _client_id: &str, _ino: u64) -> Result<()> {
        Ok(())
    }

    pub async fn acquire_lock(
        &self,
        file_path: &str,
        range: Option<(u64, u64)>,
        ttl: Duration,
    ) -> Result<LockLease> {
        self.acquire_lock_with_retry(file_path, range, ttl, 3).await
    }

    pub async fn acquire_lock_with_retry(
        &self,
        file_path: &str,
        range: Option<(u64, u64)>,
        _ttl: Duration,
        _max_retries: usize,
    ) -> Result<LockLease> {
        let lock_key = if let Some((start, end)) = range {
            format!("lock:{}:range:{}-{}", file_path, start, end)
        } else {
            format!("lock:{}", file_path)
        };

        let mut retries = 0;
        loop {
            let acquired = {
                let mut map = LOCK_MAP.lock();
                if map.contains_key(&lock_key) {
                    false
                } else {
                    map.insert(lock_key.clone(), self.client_id.clone());
                    true
                }
            };

            if acquired {
                let gen_key = format!("fencing_generator:{}", file_path);
                let fencing_token = {
                    let mut map = FENCING_MAP.lock();
                    let entry = map.entry(gen_key).or_insert(0);
                    *entry += 1;
                    *entry
                };

                return Ok(LockLease {
                    file_path: file_path.to_string(),
                    client_id: self.client_id.clone(),
                    fencing_token,
                    lock_key,
                });
            }

            if retries >= _max_retries {
                return Err(crate::error::SqueezefsError::LockFailed {
                    reason: format!("Lock key {} already held", lock_key),
                });
            }
            retries += 1;
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }

    pub async fn acquire_delegation(&self, inode: u64, _ttl: Duration) -> Result<DelegationResult> {
        Ok(DelegationResult::Acquired(DelegationLease {
            inode,
            client_id: self.client_id.clone(),
            delegation_key: format!("delegation:{}", inode),
        }))
    }
}

#[derive(Clone)]
pub struct LockLease {
    file_path: String,
    client_id: String,
    fencing_token: u64,
    lock_key: String,
}

impl LockLease {
    pub async fn is_held(&self) -> bool {
        let map = LOCK_MAP.lock();
        if let Some(owner) = map.get(&self.lock_key) {
            owner == &self.client_id
        } else {
            false
        }
    }

    pub fn fencing_token(&self) -> u64 {
        self.fencing_token
    }

    pub fn lock_key(&self) -> &str {
        &self.lock_key
    }

    pub fn file_path(&self) -> &str {
        &self.file_path
    }

    pub fn client_id(&self) -> &str {
        &self.client_id
    }

    pub async fn release(self) -> Result<()> {
        let mut map = LOCK_MAP.lock();
        if let Some(owner) = map.get(&self.lock_key) {
            if owner == &self.client_id {
                map.remove(&self.lock_key);
            }
        }
        Ok(())
    }
}

impl Drop for LockLease {
    fn drop(&mut self) {
        let mut map = LOCK_MAP.lock();
        if let Some(owner) = map.get(&self.lock_key) {
            if owner == &self.client_id {
                map.remove(&self.lock_key);
            }
        }
    }
}

#[derive(Debug)]
pub struct DelegationLease {
    pub inode: u64,
    pub client_id: String,
    pub delegation_key: String,
}

impl DelegationLease {
    pub async fn is_held(&self) -> bool {
        true
    }

    pub async fn release(self) -> Result<()> {
        Ok(())
    }
}

#[derive(Debug)]
pub enum DelegationResult {
    Acquired(DelegationLease),
    HeldBy(String),
}

#[derive(Clone)]
pub struct MockPubSub {}

impl MockPubSub {
    pub async fn subscribe(&mut self, _channel: &str) -> Result<()> {
        Ok(())
    }

    pub fn on_message(self) -> MockMessageStream {
        MockMessageStream {}
    }
}

pub struct MockMessageStream {}

impl MockMessageStream {
    pub async fn next(&mut self) -> Option<MockMessage> {
        tokio::time::sleep(std::time::Duration::from_secs(999999)).await;
        None
    }
}

pub struct MockMessage {}

impl MockMessage {
    pub fn get_payload(&self) -> std::result::Result<String, crate::error::SqueezefsError> {
        Ok(String::new())
    }
}
