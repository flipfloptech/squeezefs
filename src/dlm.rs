use crate::error::Result;
use crate::stripe_locks::StripeLocks;
use once_cell::sync::Lazy;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;
use xxhash_rust::xxh3::xxh3_64;

/// Typed lock/fencing object key.
///
/// The hot path (`inode_{N}` objects) is pure binary — no `format!`, no
/// digit parsing on reads, no heap allocation, integer hashing. String
/// forms exist only at the API boundary (callers pass `&str` paths) and
/// for the rare non-inode object; a networked DLM backend would render
/// these to wire bytes at the transport edge, never on the local path.
#[derive(Clone, PartialEq, Eq, Hash, Debug)]
pub enum ObjectKey {
    /// Whole-file lock on `inode_{0}`.
    Ino(u64),
    /// Byte-range lock on `inode_{0}`: `(ino, start, end)`.
    InoRange(u64, u64, u64),
    /// Whole-object lock on a non-inode path (rare).
    Path(Box<str>),
    /// Byte-range lock on a non-inode path (rare).
    PathRange(Box<str>, u64, u64),
}

impl ObjectKey {
    /// Parse a caller path into its binary form without allocating for the
    /// `inode_{N}` fast path.
    fn from_path(file_path: &str, range: Option<(u64, u64)>) -> Self {
        match (ino_of_path(file_path), range) {
            (Some(ino), None) => Self::Ino(ino),
            (Some(ino), Some((s, e))) => Self::InoRange(ino, s, e),
            (None, None) => Self::Path(file_path.into()),
            (None, Some((s, e))) => Self::PathRange(file_path.into(), s, e),
        }
    }

    /// The fencing generator identity: per *file* object (ranges share the
    /// file's generator, matching the historical `fencing_generator:{path}`
    /// keyspace).
    fn fencing_identity(&self) -> ObjectKey {
        match self {
            Self::Ino(i) | Self::InoRange(i, _, _) => Self::Ino(*i),
            Self::Path(p) | Self::PathRange(p, _, _) => Self::Path(p.clone()),
        }
    }

    /// Stripe selector for the waiter-notify array. Collisions are benign
    /// (spurious wakeups re-check and re-wait); correctness never depends on
    /// this hash.
    fn stripe_seed(&self) -> u64 {
        match self {
            Self::Ino(i) => *i,
            Self::InoRange(i, s, e) => i ^ s.rotate_left(16) ^ e.rotate_left(32),
            Self::Path(p) => xxh3_64(p.as_bytes()),
            Self::PathRange(p, s, e) => {
                xxh3_64(p.as_bytes()) ^ s.rotate_left(16) ^ e.rotate_left(32)
            }
        }
    }
}

/// `inode_{N}` → `N` without allocating. Strict: the entire suffix must be
/// ASCII digits that parse into a `u64`, otherwise the path is treated as an
/// opaque string key (never a lossy alias of some inode).
fn ino_of_path(path: &str) -> Option<u64> {
    let digits = path.strip_prefix("inode_")?;
    if digits.is_empty() || !digits.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    digits.parse().ok()
}

/// Lock table: object → owner nonce. Owner identity is a process-unique
/// `u64` (not a cloned `String` per acquisition).
static LOCK_MAP: Lazy<scc::HashMap<ObjectKey, u64>> = Lazy::new(scc::HashMap::new);
/// Fencing generators: file object → shared monotonic counter. `Arc` so a
/// lease caches its generator and later reads are a plain atomic load.
static FENCING_MAP: Lazy<scc::HashMap<ObjectKey, Arc<AtomicU64>>> = Lazy::new(scc::HashMap::new);
/// Per-stripe release notifications. A release wakes only its own stripe —
/// never every waiter in the process (the old single global `Notify` was a
/// thundering herd and let unrelated churn burn waiters' retry budgets).
static LOCK_WAITERS: Lazy<StripeLocks<tokio::sync::Notify, 1024>> = Lazy::new(StripeLocks::new);

static CLIENT_NONCE: AtomicU64 = AtomicU64::new(1);

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
    client_nonce: u64,
    redis_url: String,
    meta_client: Arc<MetaClient>,
}

impl DlmClient {
    pub fn new(redis_url: &str) -> Result<Self> {
        let client_nonce = CLIENT_NONCE.fetch_add(1, Ordering::Relaxed);
        let client_id = format!("local_dlm_client_{}", uuid::Uuid::new_v4());
        Ok(Self {
            client_id,
            client_nonce,
            redis_url: redis_url.to_string(),
            meta_client: Arc::new(MetaClient::Local),
        })
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

    /// Current fencing generation for a path-form object key (zero-alloc for
    /// `inode_{N}` paths).
    pub fn get_fencing_token(&self, file_path: &str) -> u64 {
        match ino_of_path(file_path) {
            Some(ino) => self.get_fencing_token_ino(ino),
            None => FENCING_MAP
                .read_sync(&ObjectKey::Path(file_path.into()), |_, v| {
                    v.load(Ordering::Acquire)
                })
                .unwrap_or(0),
        }
    }

    /// Current fencing generation for an inode object — binary fast path.
    pub fn get_fencing_token_ino(&self, ino: u64) -> u64 {
        FENCING_MAP
            .read_sync(&ObjectKey::Ino(ino), |_, v| v.load(Ordering::Acquire))
            .unwrap_or(0)
    }

    pub async fn get_pubsub_connection(&self) -> Result<MockPubSub> {
        Ok(MockPubSub {})
    }

    /// Acquire an exclusive lease on `file_path` (optionally a byte range),
    /// waiting up to `ttl` for the current holder to release.
    ///
    /// Wait protocol (per attempt):
    /// 1. `enable()` this key's stripe notification **before** checking the
    ///    table — a release landing between the check and the wait is then
    ///    still observed (tokio's documented lost-wakeup discipline).
    /// 2. Try to claim the vacant entry.
    /// 3. Otherwise wait for a stripe release or the deadline. Stripe
    ///    collisions only cause spurious re-checks, never missed wakeups.
    ///
    /// The wait budget is **time** (`ttl`), not wakeup counts: unrelated
    /// churn cannot starve a waiter into a spurious failure, and a quiet
    /// system fails loudly at the deadline instead of hanging.
    pub async fn acquire_lock(
        &self,
        file_path: &str,
        range: Option<(u64, u64)>,
        ttl: Duration,
    ) -> Result<LockLease> {
        let key = ObjectKey::from_path(file_path, range);
        let notify = LOCK_WAITERS.get_inode_lock(key.stripe_seed());
        let deadline = tokio::time::Instant::now() + ttl;

        loop {
            let notified = notify.notified();
            tokio::pin!(notified);
            // Register interest BEFORE the availability check (lost-wakeup fix).
            notified.as_mut().enable();

            let acquired = match LOCK_MAP.entry_sync(key.clone()) {
                scc::hash_map::Entry::Occupied(_) => false,
                scc::hash_map::Entry::Vacant(vac) => {
                    let _ = vac.insert_entry(self.client_nonce);
                    true
                }
            };

            if acquired {
                let fencing_token = FENCING_MAP
                    .entry_sync(key.fencing_identity())
                    .or_insert_with(|| Arc::new(AtomicU64::new(0)))
                    .get()
                    .fetch_add(1, Ordering::AcqRel)
                    + 1;

                return Ok(LockLease {
                    inner: Arc::new(LockLeaseInner {
                        key,
                        client_nonce: self.client_nonce,
                        fencing_token,
                        released: AtomicBool::new(false),
                    }),
                });
            }

            if tokio::time::timeout_at(deadline, notified).await.is_err() {
                return Err(crate::error::SqueezefsError::LockFailed {
                    reason: format!("lock {:?} still held after {:?} wait budget", key, ttl),
                });
            }
        }
    }
}

struct LockLeaseInner {
    key: ObjectKey,
    client_nonce: u64,
    fencing_token: u64,
    released: AtomicBool,
}

impl LockLeaseInner {
    /// Single-pass conditional unlock: remove the entry only if this lease's
    /// client still owns it, then wake this key's stripe. Idempotent across
    /// explicit `release()` + final-clone `Drop`.
    fn unlock(&self) {
        if self.released.swap(true, Ordering::AcqRel) {
            return;
        }
        let nonce = self.client_nonce;
        let removed = LOCK_MAP
            .remove_if_sync(&self.key, |owner| *owner == nonce)
            .is_some();
        if removed {
            LOCK_WAITERS
                .get_inode_lock(self.key.stripe_seed())
                .notify_waiters();
        }
    }
}

impl Drop for LockLeaseInner {
    fn drop(&mut self) {
        self.unlock();
    }
}

#[derive(Clone)]
pub struct LockLease {
    inner: Arc<LockLeaseInner>,
}

impl LockLease {
    pub async fn is_held(&self) -> bool {
        LOCK_MAP
            .read_sync(&self.inner.key, |_, owner| {
                *owner == self.inner.client_nonce
            })
            .unwrap_or(false)
    }

    /// The generation this lease was fenced at (snapshot at acquire).
    pub fn fencing_token(&self) -> u64 {
        self.inner.fencing_token
    }

    pub async fn release(self) -> Result<()> {
        self.inner.unlock();
        Ok(())
    }
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
