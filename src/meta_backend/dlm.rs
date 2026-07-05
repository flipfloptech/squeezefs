use std::sync::Arc;
use tokio::sync::RwLock;

#[derive(Clone)]
pub struct DlmLockManager {
    locks: Vec<Arc<RwLock<()>>>,
}

impl DlmLockManager {
    pub fn new() -> Self {
        let mut locks = Vec::with_capacity(4096);
        for _ in 0..4096 {
            locks.push(Arc::new(RwLock::new(())));
        }
        Self { locks }
    }

    fn get_lock(&self, key: &str) -> Arc<RwLock<()>> {
        use std::hash::{Hash, Hasher};
        let mut hasher = std::collections::hash_map::DefaultHasher::new();
        key.hash(&mut hasher);
        let idx = (hasher.finish() as usize) % self.locks.len();
        self.locks[idx].clone()
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
