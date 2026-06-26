use crate::error::{Result, SqueezefsError};
use log::{debug, error, warn};
use once_cell::sync::Lazy;
use redis::aio::ConnectionLike;
use std::net::{IpAddr, SocketAddr};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;
use uuid::Uuid;

static ACQUIRE_SCRIPT: Lazy<redis::Script> = Lazy::new(|| {
    redis::Script::new(
        r#"
        local acquired = redis.call("SET", KEYS[1], ARGV[1], "NX", "PX", ARGV[2])
        if acquired then
            return redis.call("INCR", KEYS[2])
        else
            return nil
        end
        "#,
    )
});

const RENEW_SCRIPT_CODE: &str = r#"
        if redis.call("get", KEYS[1]) == ARGV[1] then
            return redis.call("pexpire", KEYS[1], ARGV[2])
        else
            return 0
        end
        "#;

static RELEASE_SCRIPT: Lazy<redis::Script> = Lazy::new(|| {
    redis::Script::new(
        r#"
        if redis.call("get", KEYS[1]) == ARGV[1] then
            return redis.call("del", KEYS[1])
        else
            return 0
        end
        "#,
    )
});

static SINGLE_CONN_POOL: Lazy<
    dashmap::DashMap<String, redis::aio::MultiplexedConnection, ahash::RandomState>,
> = Lazy::new(|| dashmap::DashMap::with_hasher(ahash::RandomState::new()));

pub static SENTINEL_CONN_POOL: Lazy<
    dashmap::DashMap<String, redis::aio::MultiplexedConnection, ahash::RandomState>,
> = Lazy::new(|| dashmap::DashMap::with_hasher(ahash::RandomState::new()));

#[derive(Clone)]
pub struct BoundConnection {
    pub conn: std::sync::Arc<std::sync::RwLock<redis::aio::MultiplexedConnection>>,
    pub local_ip: IpAddr,
    pub remote_addr: SocketAddr,
    pub conn_info: redis::ConnectionInfo,
}

pub fn parse_inode_from_key(key: &str) -> Option<u64> {
    if key.starts_with("squeezefs:") {
        let parts: Vec<&str> = key.split(':').collect();
        if parts.len() >= 3 {
            if let Ok(ino) = parts[2].parse::<u64>() {
                return Some(ino);
            }
        }
    }
    if let Some(stripped) = key.strip_prefix("metadata:inode_") {
        if let Ok(ino) = stripped.parse::<u64>() {
            return Some(ino);
        }
    }
    if let Some(stripped) = key.strip_prefix("inline_data:inode_") {
        if let Ok(ino) = stripped.parse::<u64>() {
            return Some(ino);
        }
    }
    if let Some(remainder) = key.strip_prefix("mapping:inode_") {
        if let Some(pos) = remainder.find('_') {
            if let Ok(ino) = remainder[..pos].parse::<u64>() {
                return Some(ino);
            }
        }
    }
    None
}

#[derive(Clone)]
pub enum MetaClient {
    Single(redis::Client),
    SingleBound {
        client: redis::Client,
        bound_conns: Vec<BoundConnection>,
        current_idx: std::sync::Arc<AtomicUsize>,
    },
    Cluster(redis::cluster::ClusterClient),
    Sentinel {
        client: std::sync::Arc<tokio::sync::Mutex<redis::sentinel::SentinelClient>>,
        service_name: String,
    },
    Sharded {
        shards: Vec<MetaClient>,
    },
}

pub enum MetaConnection {
    Single {
        conn: redis::aio::MultiplexedConnection,
        client: Option<redis::Client>,
        bound_conn: Option<Box<BoundConnection>>,
        sentinel_client:
            Option<std::sync::Arc<tokio::sync::Mutex<redis::sentinel::SentinelClient>>>,
        service_name: Option<String>,
    },
    Cluster(redis::cluster_async::ClusterConnection),
}

async fn reconnect_bound(bound: &BoundConnection) -> Result<redis::aio::MultiplexedConnection> {
    let socket = match bound.local_ip {
        IpAddr::V4(_) => tokio::net::TcpSocket::new_v4()?,
        IpAddr::V6(_) => tokio::net::TcpSocket::new_v6()?,
    };
    socket.bind(SocketAddr::new(bound.local_ip, 0))?;
    let stream = socket.connect(bound.remote_addr).await?;
    let (new_conn, driver) =
        redis::aio::MultiplexedConnection::new(&bound.conn_info.redis, stream).await?;
    tokio::spawn(driver);
    *bound.conn.write().unwrap() = new_conn.clone();
    Ok(new_conn)
}

impl ConnectionLike for MetaConnection {
    fn req_packed_command<'a>(
        &'a mut self,
        cmd: &'a redis::Cmd,
    ) -> redis::RedisFuture<'a, redis::Value> {
        match self {
            MetaConnection::Single {
                conn,
                client,
                bound_conn,
                sentinel_client,
                service_name,
            } => {
                let client_opt = client.clone();
                let bound_conn_opt = bound_conn.clone();
                let sentinel_opt = sentinel_client.clone();
                let service_name_opt = service_name.clone();
                Box::pin(async move {
                    let res = conn.req_packed_command(cmd).await;
                    if let Err(ref e) = res {
                        if e.is_connection_refusal() || e.is_connection_dropped() || e.is_io_error()
                        {
                            if let Some(bound) = &bound_conn_opt {
                                warn!(
                                    "Cached SingleBound connection broken: {:?}. Reconnecting...",
                                    e
                                );
                                match reconnect_bound(bound).await {
                                    Ok(new_conn) => {
                                        *conn = new_conn;
                                        return conn.req_packed_command(cmd).await;
                                    }
                                    Err(reconnect_err) => {
                                        error!(
                                            "Failed to reconnect SingleBound: {:?}",
                                            reconnect_err
                                        );
                                    }
                                }
                            } else if let Some(sentinel) = &sentinel_opt {
                                warn!(
                                    "Cached Sentinel connection broken: {:?}. Reconnecting...",
                                    e
                                );
                                let mut guard = sentinel.lock().await;
                                match guard.get_async_connection().await {
                                    Ok(new_conn) => {
                                        if let Some(svc) = &service_name_opt {
                                            SENTINEL_CONN_POOL
                                                .insert(svc.clone(), new_conn.clone());
                                        }
                                        *conn = new_conn;
                                        return conn.req_packed_command(cmd).await;
                                    }
                                    Err(reconnect_err) => {
                                        error!("Failed to reconnect Sentinel: {:?}", reconnect_err);
                                    }
                                }
                            } else if let Some(client_ref) = &client_opt {
                                warn!("Cached Redis connection broken: {:?}. Reconnecting...", e);
                                let db = client_ref.get_connection_info().redis.db;
                                let addr_str =
                                    format!("{:?}/{}", client_ref.get_connection_info().addr, db);
                                match client_ref.get_multiplexed_tokio_connection().await {
                                    Ok(new_conn) => {
                                        SINGLE_CONN_POOL.insert(addr_str.clone(), new_conn.clone());
                                        *conn = new_conn;
                                        return conn.req_packed_command(cmd).await;
                                    }
                                    Err(reconnect_err) => {
                                        error!("Failed to reconnect to Redis: {:?}", reconnect_err);
                                    }
                                }
                            }
                        }
                    }
                    res
                })
            }
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
            MetaConnection::Single {
                conn,
                client,
                bound_conn,
                sentinel_client,
                service_name,
            } => {
                let client_opt = client.clone();
                let bound_conn_opt = bound_conn.clone();
                let sentinel_opt = sentinel_client.clone();
                let service_name_opt = service_name.clone();
                Box::pin(async move {
                    let res = conn.req_packed_commands(cmd, offset, count).await;
                    if let Err(ref e) = res {
                        if e.is_connection_refusal() || e.is_connection_dropped() || e.is_io_error()
                        {
                            if let Some(bound) = &bound_conn_opt {
                                warn!("Cached SingleBound connection broken in pipeline: {:?}. Reconnecting...", e);
                                match reconnect_bound(bound).await {
                                    Ok(new_conn) => {
                                        *conn = new_conn;
                                        return conn.req_packed_commands(cmd, offset, count).await;
                                    }
                                    Err(reconnect_err) => {
                                        error!(
                                            "Failed to reconnect SingleBound: {:?}",
                                            reconnect_err
                                        );
                                    }
                                }
                            } else if let Some(sentinel) = &sentinel_opt {
                                warn!("Cached Sentinel connection broken in pipeline: {:?}. Reconnecting...", e);
                                let mut guard = sentinel.lock().await;
                                match guard.get_async_connection().await {
                                    Ok(new_conn) => {
                                        if let Some(svc) = &service_name_opt {
                                            SENTINEL_CONN_POOL
                                                .insert(svc.clone(), new_conn.clone());
                                        }
                                        *conn = new_conn;
                                        return conn.req_packed_commands(cmd, offset, count).await;
                                    }
                                    Err(reconnect_err) => {
                                        error!("Failed to reconnect Sentinel: {:?}", reconnect_err);
                                    }
                                }
                            } else if let Some(client_ref) = &client_opt {
                                warn!("Cached Redis connection broken in pipeline: {:?}. Reconnecting...", e);
                                let db = client_ref.get_connection_info().redis.db;
                                let addr_str =
                                    format!("{:?}/{}", client_ref.get_connection_info().addr, db);
                                match client_ref.get_multiplexed_tokio_connection().await {
                                    Ok(new_conn) => {
                                        SINGLE_CONN_POOL.insert(addr_str.clone(), new_conn.clone());
                                        *conn = new_conn;
                                        return conn.req_packed_commands(cmd, offset, count).await;
                                    }
                                    Err(reconnect_err) => {
                                        error!("Failed to reconnect to Redis: {:?}", reconnect_err);
                                    }
                                }
                            }
                        }
                    }
                    res
                })
            }
            MetaConnection::Cluster(c) => c.req_packed_commands(cmd, offset, count),
        }
    }

    fn get_db(&self) -> i64 {
        match self {
            MetaConnection::Single { conn, .. } => conn.get_db(),
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
        if redis_url.starts_with("redis+sharded://") {
            let remainder = redis_url.strip_prefix("redis+sharded://").unwrap();
            let cleaned = remainder.replace("redis://", "");
            let nodes: Vec<&str> = cleaned
                .split(',')
                .map(|s| s.trim())
                .filter(|s| !s.is_empty())
                .collect();
            let mut shards = Vec::new();
            for node in nodes {
                let node_url = if node.starts_with("redis://") {
                    node.to_string()
                } else {
                    format!("redis://{}", node)
                };
                let shard_client = MetaClient::new(&node_url)?;
                shards.push(shard_client);
            }
            return Ok(Self::Sharded { shards });
        }

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
                service_name.clone(),
                None,
                redis::sentinel::SentinelServerType::Master,
            )?;
            Ok(Self::Sentinel {
                client: std::sync::Arc::new(tokio::sync::Mutex::new(client)),
                service_name,
            })
        } else {
            let client = redis::Client::open(redis_url)?;
            Ok(Self::Single(client))
        }
    }

    pub async fn new_with_local_ips(redis_url: &str, local_ips: Vec<IpAddr>) -> Result<Self> {
        if redis_url.starts_with("redis+sharded://") {
            let remainder = redis_url.strip_prefix("redis+sharded://").unwrap();
            let cleaned = remainder.replace("redis://", "");
            let nodes: Vec<&str> = cleaned
                .split(',')
                .map(|s| s.trim())
                .filter(|s| !s.is_empty())
                .collect();
            let mut shards = Vec::new();
            for node in nodes {
                let node_url = if node.starts_with("redis://") {
                    node.to_string()
                } else {
                    format!("redis://{}", node)
                };
                let shard_client =
                    Box::pin(MetaClient::new_with_local_ips(&node_url, local_ips.clone())).await?;
                shards.push(shard_client);
            }
            return Ok(Self::Sharded { shards });
        }

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
                service_name.clone(),
                None,
                redis::sentinel::SentinelServerType::Master,
            )?;
            Ok(Self::Sentinel {
                client: std::sync::Arc::new(tokio::sync::Mutex::new(client)),
                service_name,
            })
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
                                    bound_conns.push(BoundConnection {
                                        conn: std::sync::Arc::new(std::sync::RwLock::new(conn)),
                                        local_ip: ip,
                                        remote_addr,
                                        conn_info: conn_info.clone(),
                                    });
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
            Self::Single(client) => {
                let db = client.get_connection_info().redis.db;
                let addr_str = format!("{:?}/{}", client.get_connection_info().addr, db);
                let conn = if let Some(conn) = SINGLE_CONN_POOL.get(&addr_str).map(|r| r.clone()) {
                    conn
                } else {
                    let new_conn = client.get_multiplexed_tokio_connection().await?;
                    SINGLE_CONN_POOL.insert(addr_str, new_conn.clone());
                    new_conn
                };
                Ok(MetaConnection::Single {
                    conn,
                    client: Some(client.clone()),
                    bound_conn: None,
                    sentinel_client: None,
                    service_name: None,
                })
            }
            Self::SingleBound {
                client,
                bound_conns,
                current_idx,
            } => {
                if !bound_conns.is_empty() {
                    let idx = current_idx.fetch_add(1, Ordering::Relaxed);
                    let bound = bound_conns[idx % bound_conns.len()].clone();
                    let conn_val = bound.conn.read().unwrap().clone();
                    return Ok(MetaConnection::Single {
                        conn: conn_val,
                        client: Some(client.clone()),
                        bound_conn: Some(Box::new(bound)),
                        sentinel_client: None,
                        service_name: None,
                    });
                }
                Err(SqueezefsError::InvalidOperation(
                    "No bound connections available".to_string(),
                ))
            }
            Self::Cluster(c) => {
                let conn = c.get_async_connection().await?;
                Ok(MetaConnection::Cluster(conn))
            }
            Self::Sentinel {
                client,
                service_name,
            } => {
                let conn =
                    if let Some(conn) = SENTINEL_CONN_POOL.get(service_name).map(|r| r.clone()) {
                        conn
                    } else {
                        let mut guard = client.lock().await;
                        let new_conn = guard.get_async_connection().await?;
                        SENTINEL_CONN_POOL.insert(service_name.clone(), new_conn.clone());
                        new_conn
                    };
                Ok(MetaConnection::Single {
                    conn,
                    client: None,
                    bound_conn: None,
                    sentinel_client: Some(client.clone()),
                    service_name: Some(service_name.clone()),
                })
            }
            Self::Sharded { shards } => {
                if shards.is_empty() {
                    return Err(SqueezefsError::InvalidOperation(
                        "Sharded client has no shards".to_string(),
                    ));
                }
                Box::pin(shards[0].get_connection()).await
            }
        }
    }

    pub fn shard_count(&self) -> usize {
        match self {
            Self::Sharded { shards } => shards.len(),
            _ => 1,
        }
    }

    pub async fn get_connection_for_inode(&self, ino: u64) -> Result<MetaConnection> {
        match self {
            Self::Sharded { shards } => {
                if shards.is_empty() {
                    return Err(SqueezefsError::InvalidOperation(
                        "Sharded client has no shards".to_string(),
                    ));
                }
                let idx = (ino % shards.len() as u64) as usize;
                shards[idx].get_connection().await
            }
            _ => self.get_connection().await,
        }
    }

    pub async fn get_connection_for_key(&self, key: &str) -> Result<MetaConnection> {
        if let Some(ino) = parse_inode_from_key(key) {
            self.get_connection_for_inode(ino).await
        } else {
            self.get_connection().await
        }
    }
}

enum HeartbeatCommand {
    Register {
        lock_key: String,
        client_id: String,
        ttl_ms: u64,
    },
    Deregister {
        lock_key: String,
    },
}

struct ActiveLease {
    lock_key: String,
    client_id: String,
    ttl_ms: u64,
    next_renewal: tokio::time::Instant,
}

async fn run_heartbeat_manager(
    meta_client: MetaClient,
    mut rx: tokio::sync::mpsc::Receiver<HeartbeatCommand>,
) {
    use std::collections::HashMap;
    use tokio::time::{interval, Instant, MissedTickBehavior};

    let mut leases: HashMap<String, ActiveLease> = HashMap::new();
    let mut interval = interval(Duration::from_millis(500));
    interval.set_missed_tick_behavior(MissedTickBehavior::Skip);

    let mut con_opt: Option<MetaConnection> = None;

    loop {
        tokio::select! {
            cmd_opt = rx.recv() => {
                match cmd_opt {
                    Some(HeartbeatCommand::Register { lock_key, client_id, ttl_ms }) => {
                        let interval_dur = Duration::from_millis(ttl_ms / 3);
                        let next_renewal = Instant::now() + interval_dur;
                        leases.insert(lock_key.clone(), ActiveLease {
                            lock_key,
                            client_id,
                            ttl_ms,
                            next_renewal,
                        });
                    }
                    Some(HeartbeatCommand::Deregister { lock_key }) => {
                        leases.remove(&lock_key);
                    }
                    None => {
                        break;
                    }
                }
            }
            _ = interval.tick() => {
                if leases.is_empty() {
                    continue;
                }

                let now = Instant::now();
                let mut keys_to_renew = Vec::new();
                for (key, lease) in leases.iter_mut() {
                    if now >= lease.next_renewal {
                        keys_to_renew.push(key.clone());
                    }
                }

                if keys_to_renew.is_empty() {
                    continue;
                }

                let mut con = match con_opt.take() {
                    Some(c) => c,
                    None => {
                        match meta_client.get_connection().await {
                            Ok(c) => c,
                            Err(e) => {
                                error!("Heartbeat manager failed to connect to Redis: {:?}", e);
                                for key in &keys_to_renew {
                                    if let Some(lease) = leases.get_mut(key) {
                                        lease.next_renewal = now + Duration::from_millis(500);
                                    }
                                }
                                continue;
                            }
                        }
                    }
                };

                let mut pipe = redis::pipe();
                for key in &keys_to_renew {
                    if let Some(lease) = leases.get(key) {
                        pipe.cmd("EVAL")
                            .arg(RENEW_SCRIPT_CODE)
                            .arg(1)
                            .arg(&lease.lock_key)
                            .arg(&lease.client_id)
                            .arg(lease.ttl_ms);
                    }
                }

                match pipe.query_async::<_, Vec<i32>>(&mut con).await {
                    Ok(results) => {
                        con_opt = Some(con);
                        for (i, key) in keys_to_renew.into_iter().enumerate() {
                            if let Some(lease) = leases.get_mut(&key) {
                                let res_val = results.get(i).copied().unwrap_or(0);
                                if res_val == 1 {
                                    debug!("Heartbeat manager: Successfully renewed lease for key: {}", key);
                                    let interval_dur = Duration::from_millis(lease.ttl_ms / 3);
                                    lease.next_renewal = Instant::now() + interval_dur;
                                } else {
                                    error!("Heartbeat manager: Failed to renew lease for key: {} - lock stolen or expired", key);
                                    leases.remove(&key);
                                }
                            }
                        }
                    }
                    Err(e) => {
                        error!("Heartbeat manager: Error executing renewal pipeline: {:?}", e);
                        for key in keys_to_renew {
                            if let Some(lease) = leases.get_mut(&key) {
                                lease.next_renewal = now + Duration::from_millis(500);
                            }
                        }
                    }
                }
            }
        }
    }
}

#[derive(Clone)]
pub struct DlmClient {
    client_id: String,
    meta_client: MetaClient,
    heartbeat_tx:
        std::sync::Arc<once_cell::sync::OnceCell<tokio::sync::mpsc::Sender<HeartbeatCommand>>>,
}

pub struct LockLease {
    file_path: String,
    client_id: String,
    fencing_token: u64,
    lock_key: String,
    heartbeat_tx: Option<tokio::sync::mpsc::Sender<HeartbeatCommand>>,
    meta_client: MetaClient,
    range: Option<(u64, u64)>,
}

impl DlmClient {
    fn get_heartbeat_tx(&self) -> &tokio::sync::mpsc::Sender<HeartbeatCommand> {
        self.heartbeat_tx.get_or_init(|| {
            let (heartbeat_tx, heartbeat_rx) = tokio::sync::mpsc::channel(1024);
            let meta_client_clone = self.meta_client.clone();
            tokio::spawn(async move {
                run_heartbeat_manager(meta_client_clone, heartbeat_rx).await;
            });
            heartbeat_tx
        })
    }

    fn init_with_client(client_id: String, meta_client: MetaClient) -> Self {
        Self {
            client_id,
            meta_client,
            heartbeat_tx: std::sync::Arc::new(once_cell::sync::OnceCell::new()),
        }
    }

    pub fn new(redis_url: &str) -> Result<Self> {
        let meta_client = MetaClient::new(redis_url)?;
        let client_id = Uuid::new_v4().to_string();
        Ok(Self::init_with_client(client_id, meta_client))
    }

    pub async fn new_with_local_ips(redis_url: &str, local_ips: Vec<IpAddr>) -> Result<Self> {
        let meta_client = MetaClient::new_with_local_ips(redis_url, local_ips).await?;
        let client_id = Uuid::new_v4().to_string();
        Ok(Self::init_with_client(client_id, meta_client))
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

    pub fn shard_count(&self) -> usize {
        self.meta_client.shard_count()
    }

    pub async fn get_connection_for_inode(&self, ino: u64) -> Result<MetaConnection> {
        self.meta_client.get_connection_for_inode(ino).await
    }

    pub async fn get_connection_for_key(&self, key: &str) -> Result<MetaConnection> {
        self.meta_client.get_connection_for_key(key).await
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
        crate::coz_progress!("dlm_acquire_lock");
        let lock_key = if let Some((start, end)) = range {
            format!("lock:{}:range:{}-{}", file_path, start, end)
        } else {
            format!("lock:{}", file_path)
        };

        let mut con = self.meta_client.get_connection().await?;
        let ttl_ms = ttl.as_millis() as u64;

        // Generate a monotonic fencing token key
        let fencing_gen_key = format!("fencing_generator:{}", file_path);

        // Perform atomic lock acquire + fencing token increment via Lua
        let fencing_token: Option<u64> = ACQUIRE_SCRIPT
            .key(&lock_key)
            .key(&fencing_gen_key)
            .arg(&self.client_id)
            .arg(ttl_ms)
            .invoke_async(&mut con)
            .await?;

        let fencing_token = match fencing_token {
            Some(token) => token,
            None => {
                return Err(SqueezefsError::LockFailed {
                    reason: format!("Lock is already held on {}", lock_key),
                });
            }
        };

        // Register with manager
        let tx = self.get_heartbeat_tx().clone();
        let _ = tx
            .send(HeartbeatCommand::Register {
                lock_key: lock_key.clone(),
                client_id: self.client_id.clone(),
                ttl_ms,
            })
            .await;

        Ok(LockLease {
            file_path: file_path.to_string(),
            client_id: self.client_id.clone(),
            fencing_token,
            lock_key,
            heartbeat_tx: Some(tx),
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

        let mut con = self.meta_client.get_connection().await?;
        // Release ONLY if we still own it to avoid releasing other client's lock
        let _res: i32 = RELEASE_SCRIPT
            .key(&self.lock_key)
            .arg(&self.client_id)
            .invoke_async(&mut con)
            .await?;

        Ok(())
    }

    fn stop_heartbeat(&mut self) {
        if let Some(tx) = self.heartbeat_tx.take() {
            let key = self.lock_key.clone();
            if let Ok(handle) = tokio::runtime::Handle::try_current() {
                handle.spawn(async move {
                    let _ = tx
                        .send(HeartbeatCommand::Deregister { lock_key: key })
                        .await;
                });
            }
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

        if let Ok(handle) = tokio::runtime::Handle::try_current() {
            handle.spawn(async move {
                let lock_key = if let Some((start, end)) = range {
                    format!("lock:{}:range:{}-{}", file_path, start, end)
                } else {
                    format!("lock:{}", file_path)
                };
                if let Ok(mut con) = meta_client.get_connection().await {
                    let _: Result<i32> = RELEASE_SCRIPT
                        .key(&lock_key)
                        .arg(&client_id)
                        .invoke_async(&mut con)
                        .await
                        .map_err(|e| e.into());
                }
            });
        }
    }
}
