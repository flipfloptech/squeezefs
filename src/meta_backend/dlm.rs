use std::sync::Arc;
use tokio::sync::RwLock;

#[derive(Clone)]
pub struct DlmLockManager {
    locks: std::sync::Arc<dashmap::DashMap<String, Arc<RwLock<()>>>>,
}

impl DlmLockManager {
    pub fn new() -> Self {
        Self {
            locks: std::sync::Arc::new(dashmap::DashMap::new()),
        }
    }

    fn get_lock(&self, key: &str) -> Arc<RwLock<()>> {
        let entry = self
            .locks
            .entry(key.to_string())
            .or_insert_with(|| Arc::new(RwLock::new(())));
        entry.value().clone()
    }

    /// Acquires an exclusive lock on the given key (e.g. `I<ino>` or `D<parent>:<name>`)
    pub async fn lock_exclusive(&self, key: &str) -> DlmGuard {
        let lock = self.get_lock(key);
        let raw_guard = lock.write_owned().await;
        DlmGuard {
            _key: key.to_string(),
            _guard: DlmGuardInner::Exclusive(raw_guard),
        }
    }

    /// Acquires a shared lock on the given key
    pub async fn lock_shared(&self, key: &str) -> DlmGuard {
        let lock = self.get_lock(key);
        let raw_guard = lock.read_owned().await;
        DlmGuard {
            _key: key.to_string(),
            _guard: DlmGuardInner::Shared(raw_guard),
        }
    }
}

pub enum DlmGuardInner {
    Shared(tokio::sync::OwnedRwLockReadGuard<()>),
    Exclusive(tokio::sync::OwnedRwLockWriteGuard<()>),
}

pub struct DlmGuard {
    _key: String,
    _guard: DlmGuardInner,
}
