use crate::error::{Result, SqueezefsError};
use log::{debug, error, warn};
use redis::aio::ConnectionLike;
use redis::AsyncCommands;
use std::net::{IpAddr, SocketAddr};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;
use tokio::sync::oneshot;
use tokio::task::JoinHandle;
use tokio::time;
use uuid::Uuid;

#[derive(Clone)]
pub enum MetaClient {
    Single(redis::Client),
    SingleBound {
        client: redis::Client,
        bound_conns: Vec<redis::aio::MultiplexedConnection>,
        current_idx: std::sync::Arc<AtomicUsize>,
    },
    Cluster(redis::cluster::ClusterClient),
    Sentinel(std::sync::Arc<tokio::sync::Mutex<redis::sentinel::SentinelClient>>),
}

pub enum MetaConnection {
    Single(redis::aio::MultiplexedConnection),
    Cluster(redis::cluster_async::ClusterConnection),
}

impl ConnectionLike for MetaConnection {
    fn req_packed_command<'a>(
        &'a mut self,
        cmd: &'a redis::Cmd,
    ) -> redis::RedisFuture<'a, redis::Value> {
        match self {
            MetaConnection::Single(c) => c.req_packed_command(cmd),
            MetaConnection::Cluster(c) => c.req_packed_command(cmd),
        }
    }

    fn req_packed_commands<'a>(
        &'a mut self,
        cmd: &'a redis::Pipeline,
        offset: usize,
        count: usize,
    ) -> redis::RedisFuture<'a, Vec<redis::Value>> {
        match self {
            MetaConnection::Single(c) => c.req_packed_commands(cmd, offset, count),
            MetaConnection::Cluster(c) => c.req_packed_commands(cmd, offset, count),
        }
    }

    fn get_db(&self) -> i64 {
        match self {
            MetaConnection::Single(c) => c.get_db(),
            MetaConnection::Cluster(c) => c.get_db(),
        }
    }
}

fn resolve_redis_addr(redis_url: &str) -> Result<SocketAddr> {
    let cleaned = redis_url.strip_prefix("redis://").unwrap_or(redis_url);
    let host_port = cleaned.split('/').next().unwrap_or(cleaned);
    let host_port = host_port.split('?').next().unwrap_or(host_port);

    let parts: Vec<&str> = host_port.split(':').collect();
    let host = parts[0];
    let port = if parts.len() > 1 {
        parts[1].parse::<u16>().unwrap_or(6379)
    } else {
        6379
    };

    use std::net::ToSocketAddrs;
    let addrs = (host, port).to_socket_addrs().map_err(|e| {
        SqueezefsError::Io(std::io::Error::new(
            std::io::ErrorKind::AddrNotAvailable,
            format!("DNS lookup failed for {}:{}: {:?}", host, port, e),
        ))
    })?;

    if let Some(addr) = addrs.into_iter().next() {
        return Ok(addr);
    }

    Err(SqueezefsError::Io(std::io::Error::new(
        std::io::ErrorKind::AddrNotAvailable,
        format!("No resolved addresses for host: {}", host),
    )))
}

impl MetaClient {
    pub fn new(redis_url: &str) -> Result<Self> {
        let is_sentinel = redis_url.starts_with("redis-sentinel://");
        let is_cluster = !is_sentinel
            && (redis_url.contains(',')
                || redis_url.starts_with("redis+cluster://")
                || redis_url.contains("cluster=true"));

        if is_cluster {
            let cleaned_url = redis_url.replace("redis+cluster://", "redis://");
            let nodes: Vec<&str> = cleaned_url
                .split(',')
                .map(|s| s.trim())
                .filter(|s| !s.is_empty())
                .collect();
            let client = redis::cluster::ClusterClientBuilder::new(nodes)
                .read_from_replicas()
                .build()?;
            Ok(Self::Cluster(client))
        } else if let Some(remainder) = redis_url.strip_prefix("redis-sentinel://") {
            let parts: Vec<&str> = remainder.split('/').collect();
            if parts.len() < 2 {
                return Err(SqueezefsError::InvalidOperation(
                    "Invalid sentinel URL. Expected format: redis-sentinel://host1:port1,host2:port2/service_name".to_string()
                ));
            }
            let service_name = parts[1].to_string();
            let nodes: Vec<String> = parts[0]
                .split(',')
                .map(|s| {
                    let host_port = s.trim();
                    if host_port.starts_with("redis://") {
                        host_port.to_string()
                    } else {
                        format!("redis://{}", host_port)
                    }
                })
                .filter(|s| !s.is_empty())
                .collect();
            let client = redis::sentinel::SentinelClient::build(
                nodes,
                service_name,
                None,
                redis::sentinel::SentinelServerType::Master,
            )?;
            Ok(Self::Sentinel(std::sync::Arc::new(
                tokio::sync::Mutex::new(client),
            )))
        } else {
            let client = redis::Client::open(redis_url)?;
            Ok(Self::Single(client))
        }
    }

    pub async fn new_with_local_ips(redis_url: &str, local_ips: Vec<IpAddr>) -> Result<Self> {
        let is_sentinel = redis_url.starts_with("redis-sentinel://");
        let is_cluster = !is_sentinel
            && (redis_url.contains(',')
                || redis_url.starts_with("redis+cluster://")
                || redis_url.contains("cluster=true"));

        if is_cluster {
            let cleaned_url = redis_url.replace("redis+cluster://", "redis://");
            let nodes: Vec<&str> = cleaned_url
                .split(',')
                .map(|s| s.trim())
                .filter(|s| !s.is_empty())
                .collect();
            let client = redis::cluster::ClusterClientBuilder::new(nodes)
                .read_from_replicas()
                .build()?;
            Ok(Self::Cluster(client))
        } else if let Some(remainder) = redis_url.strip_prefix("redis-sentinel://") {
            let parts: Vec<&str> = remainder.split('/').collect();
            if parts.len() < 2 {
                return Err(SqueezefsError::InvalidOperation(
                    "Invalid sentinel URL. Expected format: redis-sentinel://host1:port1,host2:port2/service_name".to_string()
                ));
            }
            let service_name = parts[1].to_string();
            let nodes: Vec<String> = parts[0]
                .split(',')
                .map(|s| {
                    let host_port = s.trim();
                    if host_port.starts_with("redis://") {
                        host_port.to_string()
                    } else {
                        format!("redis://{}", host_port)
                    }
                })
                .filter(|s| !s.is_empty())
                .collect();
            let client = redis::sentinel::SentinelClient::build(
                nodes,
                service_name,
                None,
                redis::sentinel::SentinelServerType::Master,
            )?;
            Ok(Self::Sentinel(std::sync::Arc::new(
                tokio::sync::Mutex::new(client),
            )))
        } else {
            let client = redis::Client::open(redis_url)?;
            if local_ips.is_empty() {
                Ok(Self::Single(client))
            } else {
                let remote_addr = match resolve_redis_addr(redis_url) {
                    Ok(addr) => addr,
                    Err(e) => {
                        warn!("Could not resolve Redis address: {:?}. Falling back to default client.", e);
                        return Ok(Self::Single(client));
                    }
                };

                let mut bound_conns = Vec::new();
                let conn_info = client.get_connection_info();

                for ip in local_ips {
                    let socket = match ip {
                        IpAddr::V4(_) => tokio::net::TcpSocket::new_v4()?,
                        IpAddr::V6(_) => tokio::net::TcpSocket::new_v6()?,
                    };
                    let _ = socket.bind(SocketAddr::new(ip, 0));

                    match socket.connect(remote_addr).await {
                        Ok(stream) => {
                            match redis::aio::MultiplexedConnection::new(&conn_info.redis, stream)
                                .await
                            {
                                Ok((conn, driver)) => {
                                    tokio::spawn(driver);
                                    bound_conns.push(conn);
                                }
                                Err(e) => {
                                    warn!("Failed to establish MultiplexedConnection on interface {}: {:?}", ip, e);
                                }
                            }
                        }
                        Err(e) => {
                            warn!("Failed to connect to Redis on interface {}: {:?}", ip, e);
                        }
                    }
                }

                if bound_conns.is_empty() {
                    warn!("Failed to connect on all interfaces. Falling back to default routing.");
                    Ok(Self::Single(client))
                } else {
                    Ok(Self::SingleBound {
                        client,
                        bound_conns,
                        current_idx: std::sync::Arc::new(AtomicUsize::new(0)),
                    })
                }
            }
        }
    }

    pub async fn get_connection(&self) -> Result<MetaConnection> {
        match self {
            Self::Single(c) => {
                let conn = c.get_multiplexed_tokio_connection().await?;
                Ok(MetaConnection::Single(conn))
            }
            Self::SingleBound {
                bound_conns,
                current_idx,
                ..
            } => {
                if !bound_conns.is_empty() {
                    let idx = current_idx.fetch_add(1, Ordering::Relaxed);
                    let conn = bound_conns[idx % bound_conns.len()].clone();
                    return Ok(MetaConnection::Single(conn));
                }
                Err(SqueezefsError::InvalidOperation(
                    "No bound connections available".to_string(),
                ))
            }
            Self::Cluster(c) => {
                let conn = c.get_async_connection().await?;
                Ok(MetaConnection::Cluster(conn))
            }
            Self::Sentinel(c) => {
                let mut guard = c.lock().await;
                let conn = guard.get_async_connection().await?;
                Ok(MetaConnection::Single(conn))
            }
        }
    }
}

#[derive(Clone)]
pub struct DlmClient {
    client_id: String,
    meta_client: MetaClient,
}

pub struct LockLease {
    file_path: String,
    client_id: String,
    fencing_token: u64,
    heartbeat_tx: Option<oneshot::Sender<()>>,
    _heartbeat_handle: Option<JoinHandle<()>>,
    meta_client: MetaClient,
    range: Option<(u64, u64)>,
}

impl DlmClient {
    pub fn new(redis_url: &str) -> Result<Self> {
        let meta_client = MetaClient::new(redis_url)?;
        let client_id = Uuid::new_v4().to_string();
        Ok(Self {
            client_id,
            meta_client,
        })
    }

    pub async fn new_with_local_ips(redis_url: &str, local_ips: Vec<IpAddr>) -> Result<Self> {
        let meta_client = MetaClient::new_with_local_ips(redis_url, local_ips).await?;
        let client_id = Uuid::new_v4().to_string();
        Ok(Self {
            client_id,
            meta_client,
        })
    }

    pub fn connection_count(&self) -> usize {
        match &self.meta_client {
            MetaClient::Single(_) => 1,
            MetaClient::SingleBound { bound_conns, .. } => bound_conns.len(),
            _ => 1,
        }
    }

    pub fn meta_client(&self) -> &MetaClient {
        &self.meta_client
    }

    pub fn client_id(&self) -> &str {
        &self.client_id
    }

    pub async fn get_connection(&self) -> Result<MetaConnection> {
        self.meta_client.get_connection().await
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

        let mut con = self.meta_client.get_connection().await?;
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
        let meta_client_clone = self.meta_client.clone();
        let interval_duration = ttl / 3; // Renew at 1/3 of TTL (e.g. every 1.6s for 5s TTL)

        let heartbeat_handle = tokio::spawn(async move {
            let mut interval = time::interval(interval_duration);
            // First tick is immediate, skip it
            interval.tick().await;

            let mut con = match meta_client_clone.get_connection().await {
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
                                debug!("Successfully renewed lease for key: {} (client_id: {}, ttl: {}ms)", lock_key_clone, client_id_clone, ttl_ms);
                            }
                            Ok(_) => {
                                error!("Failed to renew lease for key: {} (client_id: {}, ttl: {}ms) - lock was stolen or expired!", lock_key_clone, client_id_clone, ttl_ms);
                                break;
                            }
                            Err(e) => {
                                error!("Error executing lease renewal script for key {} (client_id: {}, ttl: {}ms): {:?}", lock_key_clone, client_id_clone, ttl_ms, e);
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
            meta_client: self.meta_client.clone(),
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

        let mut con = self.meta_client.get_connection().await?;
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
        let meta_client = self.meta_client.clone();

        tokio::spawn(async move {
            let lock_key = if let Some((start, end)) = range {
                format!("lock:{}:range:{}-{}", file_path, start, end)
            } else {
                format!("lock:{}", file_path)
            };
            if let Ok(mut con) = meta_client.get_connection().await {
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
