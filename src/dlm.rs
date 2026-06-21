use crate::error::{Result, SqueezefsError};
use log::{debug, error};
use redis::AsyncCommands;
use std::time::Duration;
use tokio::sync::oneshot;
use tokio::task::JoinHandle;
use tokio::time;
use uuid::Uuid;

#[derive(Clone)]
pub struct DlmClient {
    client_id: String,
    redis_client: redis::Client,
}

pub struct LockLease {
    file_path: String,
    client_id: String,
    fencing_token: u64,
    heartbeat_tx: Option<oneshot::Sender<()>>,
    _heartbeat_handle: Option<JoinHandle<()>>,
    redis_client: redis::Client,
    range: Option<(u64, u64)>,
}

impl DlmClient {
    pub fn new(redis_url: &str) -> Result<Self> {
        let redis_client = redis::Client::open(redis_url)?;
        let client_id = Uuid::new_v4().to_string();
        Ok(Self {
            client_id,
            redis_client,
        })
    }

    pub fn redis_client(&self) -> &redis::Client {
        &self.redis_client
    }

    /// Acquire a lease for a file-level or byte-range lock.
    /// - `file_path`: path to file
    /// - `range`: Option of (start, end) byte range
    /// - `ttl`: duration the lock is valid for (typically 5 seconds)
    pub async fn acquire_lock(
        &self,
        file_path: &str,
        range: Option<(u64, u64)>,
        ttl: Duration,
    ) -> Result<LockLease> {
        let lock_key = if let Some((start, end)) = range {
            format!("lock:{}:range:{}-{}", file_path, start, end)
        } else {
            format!("lock:{}", file_path)
        };

        let mut con = self.redis_client.get_multiplexed_tokio_connection().await?;
        let ttl_ms = ttl.as_millis() as u64;

        // Perform SET key client_id NX PX ttl_ms
        let acquired: Option<String> = redis::Cmd::set_options(
            &lock_key,
            &self.client_id,
            redis::SetOptions::default()
                .conditional_set(redis::ExistenceCheck::NX)
                .with_expiration(redis::SetExpiry::PX(ttl_ms.try_into().unwrap())),
        )
        .query_async(&mut con)
        .await?;

        if acquired.is_none() {
            return Err(SqueezefsError::LockFailed {
                reason: format!("Lock is already held on {}", lock_key),
            });
        }

        // Generate a monotonic fencing token for this file
        let fencing_gen_key = format!("fencing_generator:{}", file_path);
        let fencing_token: u64 = con.incr(&fencing_gen_key, 1).await?;

        // Start heartbeat renewal thread
        let (heartbeat_tx, mut heartbeat_rx) = oneshot::channel::<()>();
        let client_id_clone = self.client_id.clone();
        let lock_key_clone = lock_key.clone();
        let redis_client_clone = self.redis_client.clone();
        let interval_duration = ttl / 3; // Renew at 1/3 of TTL (e.g. every 1.6s for 5s TTL)

        let heartbeat_handle = tokio::spawn(async move {
            let mut interval = time::interval(interval_duration);
            // First tick is immediate, skip it
            interval.tick().await;

            let mut con = match redis_client_clone.get_multiplexed_tokio_connection().await {
                Ok(c) => c,
                Err(e) => {
                    error!("Heartbeat failed to establish Redis connection: {:?}", e);
                    return;
                }
            };

            loop {
                tokio::select! {
                    _ = &mut heartbeat_rx => {
                        debug!("Heartbeat task received cancellation signal for key: {}", lock_key_clone);
                        break;
                    }
                    _ = interval.tick() => {
                        // Lua script or SET command to renew ONLY if we still own it
                        // script: if redis.call('get', KEYS[1]) == ARGV[1] then return redis.call('pexpire', KEYS[1], ARGV[2]) else return 0 end
                        let script = redis::Script::new(
                            r#"
                            if redis.call("get", KEYS[1]) == ARGV[1] then
                                return redis.call("pexpire", KEYS[1], ARGV[2])
                            else
                                return 0
                            end
                            "#
                        );

                        match script.key(&lock_key_clone).arg(&client_id_clone).arg(ttl_ms).invoke_async::<_, i32>(&mut con).await {
                            Ok(1) => {
                                debug!("Successfully renewed lease for key: {}", lock_key_clone);
                            }
                            Ok(_) => {
                                error!("Failed to renew lease for key: {}, lock was stolen or expired!", lock_key_clone);
                                break;
                            }
                            Err(e) => {
                                error!("Error executing lease renewal script for key {}: {:?}", lock_key_clone, e);
                            }
                        }
                    }
                }
            }
        });

        Ok(LockLease {
            file_path: file_path.to_string(),
            client_id: self.client_id.clone(),
            fencing_token,
            heartbeat_tx: Some(heartbeat_tx),
            _heartbeat_handle: Some(heartbeat_handle),
            redis_client: self.redis_client.clone(),
            range,
        })
    }
}

impl LockLease {
    pub fn fencing_token(&self) -> u64 {
        self.fencing_token
    }

    pub fn file_path(&self) -> &str {
        &self.file_path
    }

    pub fn range(&self) -> Option<(u64, u64)> {
        self.range
    }

    /// Explicitly release the lease.
    pub async fn release(mut self) -> Result<()> {
        self.stop_heartbeat();

        let lock_key = if let Some((start, end)) = self.range {
            format!("lock:{}:range:{}-{}", self.file_path, start, end)
        } else {
            format!("lock:{}", self.file_path)
        };

        let mut con = self.redis_client.get_multiplexed_tokio_connection().await?;
        // Release ONLY if we still own it to avoid releasing other client's lock
        let script = redis::Script::new(
            r#"
            if redis.call("get", KEYS[1]) == ARGV[1] then
                return redis.call("del", KEYS[1])
            else
                return 0
            end
            "#,
        );
        let _res: i32 = script
            .key(&lock_key)
            .arg(&self.client_id)
            .invoke_async(&mut con)
            .await?;

        Ok(())
    }

    fn stop_heartbeat(&mut self) {
        if let Some(tx) = self.heartbeat_tx.take() {
            let _ = tx.send(());
        }
    }
}

impl Drop for LockLease {
    fn drop(&mut self) {
        self.stop_heartbeat();
        // Since drop is synchronous, spawn background task to delete the Redis lock key
        let file_path = self.file_path.clone();
        let client_id = self.client_id.clone();
        let range = self.range;
        let redis_client = self.redis_client.clone();

        tokio::spawn(async move {
            let lock_key = if let Some((start, end)) = range {
                format!("lock:{}:range:{}-{}", file_path, start, end)
            } else {
                format!("lock:{}", file_path)
            };
            if let Ok(mut con) = redis_client.get_multiplexed_tokio_connection().await {
                let script = redis::Script::new(
                    r#"
                    if redis.call("get", KEYS[1]) == ARGV[1] then
                        return redis.call("del", KEYS[1])
                    else
                        return 0
                    end
                    "#,
                );
                let _: Result<i32> = script
                    .key(&lock_key)
                    .arg(&client_id)
                    .invoke_async(&mut con)
                    .await
                    .map_err(|e| e.into());
            }
        });
    }
}
