use bytes::Bytes;
use parking_lot::{Mutex, RwLock};
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::net::{IpAddr, SocketAddr};
use std::sync::atomic::AtomicUsize;
use std::sync::{Arc, Once};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use xxhash_rust::xxh3::xxh3_64;

use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer, ServerName, UnixTime};
use rustls::{DigitallySignedStruct, SignatureScheme};

static RUSTLS_INIT: Once = Once::new();

fn init_rustls() {
    RUSTLS_INIT.call_once(|| {
        let _ = rustls::crypto::ring::default_provider().install_default();
    });
}

#[derive(Debug)]
struct DummyVerifier;

impl ServerCertVerifier for DummyVerifier {
    fn verify_server_cert(
        &self,
        _end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _server_name: &ServerName<'_>,
        _ocsp_response: &[u8],
        _now: UnixTime,
    ) -> Result<ServerCertVerified, rustls::Error> {
        Ok(ServerCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        _message: &[u8],
        _cert: &CertificateDer<'_>,
        _dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        Ok(HandshakeSignatureValid::assertion())
    }

    fn verify_tls13_signature(
        &self,
        _message: &[u8],
        _cert: &CertificateDer<'_>,
        _dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        Ok(HandshakeSignatureValid::assertion())
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        rustls::crypto::ring::default_provider()
            .signature_verification_algorithms
            .supported_schemes()
    }
}

fn make_server_config() -> std::io::Result<quinn::ServerConfig> {
    init_rustls();
    let cert =
        rcgen::generate_simple_self_signed(vec!["localhost".to_string(), "127.0.0.1".to_string()])
            .map_err(std::io::Error::other)?;
    let cert_der = cert.cert.der().to_vec();
    let key_der = cert.key_pair.serialize_der();

    let certs = vec![CertificateDer::from(cert_der)];
    let key = PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(key_der));

    let server_config = rustls::ServerConfig::builder()
        .with_no_client_auth()
        .with_single_cert(certs, key)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidInput, e))?;

    let quinn_server_config = quinn::crypto::rustls::QuicServerConfig::try_from(server_config)
        .map_err(std::io::Error::other)?;

    let mut server_config = quinn::ServerConfig::with_crypto(Arc::new(quinn_server_config));

    let mut transport = quinn::TransportConfig::default();
    transport
        .stream_receive_window(8_388_608u32.into())
        .receive_window(16_777_216u32.into())
        .send_window(8_388_608)
        .max_concurrent_bidi_streams(10_000u32.into());
    server_config.transport_config(Arc::new(transport));

    Ok(server_config)
}

fn make_client_config() -> quinn::ClientConfig {
    init_rustls();
    let client_config = rustls::ClientConfig::builder()
        .dangerous()
        .with_custom_certificate_verifier(Arc::new(DummyVerifier))
        .with_no_client_auth();

    let quinn_client_config =
        quinn::crypto::rustls::QuicClientConfig::try_from(client_config).unwrap();
    let mut client_config = quinn::ClientConfig::new(Arc::new(quinn_client_config));

    let mut transport = quinn::TransportConfig::default();
    transport
        .stream_receive_window(8_388_608u32.into())
        .receive_window(16_777_216u32.into())
        .send_window(8_388_608)
        .max_concurrent_bidi_streams(10_000u32.into());
    client_config.transport_config(Arc::new(transport));

    client_config
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub enum Message {
    Ping,
    Pong,
    RegisterProvider {
        key_hash: u64,
        provider_addr: String,
    },
    FindProvider {
        key_hash: u64,
    },
    ProviderResponse {
        provider_addr: Option<String>,
    },
    FetchValue {
        key: Bytes,
    },
    ValueResponse {
        value: Option<Bytes>,
    },
    StoreValue {
        key: Bytes,
        value: Bytes,
    },
}

#[derive(Serialize, Deserialize, Debug, Clone)]
enum WireMessage {
    Ping,
    Pong,
    RegisterProvider {
        key_hash: u64,
        provider_addr: String,
    },
    FindProvider {
        key_hash: u64,
    },
    ProviderResponse {
        provider_addr: Option<String>,
    },
    FetchValue {
        key: Bytes,
    },
    ValueResponseHeader {
        has_value: bool,
        value_len: u32,
    },
    StoreValueHeader {
        key: Bytes,
        value_len: u32,
    },
}

pub async fn write_msg<W>(writer: &mut W, msg: &Message) -> std::io::Result<()>
where
    W: tokio::io::AsyncWrite + Unpin,
{
    let (wire_msg, payload) = match msg {
        Message::Ping => (WireMessage::Ping, None),
        Message::Pong => (WireMessage::Pong, None),
        Message::RegisterProvider {
            key_hash,
            provider_addr,
        } => (
            WireMessage::RegisterProvider {
                key_hash: *key_hash,
                provider_addr: provider_addr.clone(),
            },
            None,
        ),
        Message::FindProvider { key_hash } => (
            WireMessage::FindProvider {
                key_hash: *key_hash,
            },
            None,
        ),
        Message::ProviderResponse { provider_addr } => (
            WireMessage::ProviderResponse {
                provider_addr: provider_addr.clone(),
            },
            None,
        ),
        Message::FetchValue { key } => (WireMessage::FetchValue { key: key.clone() }, None),
        Message::ValueResponse { value } => match value {
            Some(val) => (
                WireMessage::ValueResponseHeader {
                    has_value: true,
                    value_len: val.len() as u32,
                },
                Some(val.clone()),
            ),
            None => (
                WireMessage::ValueResponseHeader {
                    has_value: false,
                    value_len: 0,
                },
                None,
            ),
        },
        Message::StoreValue { key, value } => (
            WireMessage::StoreValueHeader {
                key: key.clone(),
                value_len: value.len() as u32,
            },
            Some(value.clone()),
        ),
    };

    let body_bytes = bincode::serialize(&wire_msg)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
    let len_bytes = (body_bytes.len() as u32).to_le_bytes();

    let mut buf = Vec::with_capacity(4 + body_bytes.len());
    buf.extend_from_slice(&len_bytes);
    buf.extend_from_slice(&body_bytes);
    writer.write_all(&buf).await?;

    if let Some(p) = payload {
        writer.write_all(&p).await?;
    }

    Ok(())
}

async fn read_msg<R>(reader: &mut R) -> std::io::Result<Message>
where
    R: tokio::io::AsyncRead + Unpin,
{
    let mut len_bytes = [0u8; 4];
    reader.read_exact(&mut len_bytes).await?;
    let len = u32::from_le_bytes(len_bytes) as usize;
    let mut buf = vec![0u8; len];
    reader.read_exact(&mut buf).await?;
    let wire_msg: WireMessage = bincode::deserialize(&buf)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;

    let msg = match wire_msg {
        WireMessage::Ping => Message::Ping,
        WireMessage::Pong => Message::Pong,
        WireMessage::RegisterProvider {
            key_hash,
            provider_addr,
        } => Message::RegisterProvider {
            key_hash,
            provider_addr,
        },
        WireMessage::FindProvider { key_hash } => Message::FindProvider { key_hash },
        WireMessage::ProviderResponse { provider_addr } => {
            Message::ProviderResponse { provider_addr }
        }
        WireMessage::FetchValue { key } => Message::FetchValue { key },
        WireMessage::ValueResponseHeader {
            has_value,
            value_len,
        } => {
            if has_value {
                let mut payload = vec![0u8; value_len as usize];
                reader.read_exact(&mut payload).await?;
                Message::ValueResponse {
                    value: Some(Bytes::from(payload)),
                }
            } else {
                Message::ValueResponse { value: None }
            }
        }
        WireMessage::StoreValueHeader { key, value_len } => {
            let mut payload = vec![0u8; value_len as usize];
            reader.read_exact(&mut payload).await?;
            Message::StoreValue {
                key,
                value: Bytes::from(payload),
            }
        }
    };

    Ok(msg)
}

/// Trait to read from the local cache tiers (Memory and NVMe).
/// The DHT node uses this to serve FetchValue requests from other peers.
pub trait LocalCacheReader: Send + Sync {
    fn get_local(&self, key: &Bytes) -> Option<Bytes>;
    fn put_local(&self, key: Bytes, value: Bytes);
}

pub struct DhtNode {
    peer_addr: String,
    peer_id: u64,
    routing_table: RwLock<HashSet<String>>,
    providers: RwLock<HashMap<u64, String>>,
    local_reader: Arc<dyn LocalCacheReader>,
    #[allow(dead_code)]
    local_ips: Vec<IpAddr>,
    #[allow(dead_code)]
    local_ip_counter: AtomicUsize,
    endpoint: quinn::Endpoint,
    connection_pool: Mutex<HashMap<String, quinn::Connection>>,
}

impl DhtNode {
    pub fn new(peer_addr: String, local_reader: Arc<dyn LocalCacheReader>) -> Self {
        Self::new_with_ips(peer_addr, local_reader, Vec::new())
    }

    pub fn new_with_ips(
        peer_addr: String,
        local_reader: Arc<dyn LocalCacheReader>,
        local_ips: Vec<IpAddr>,
    ) -> Self {
        let peer_id = xxh3_64(peer_addr.as_bytes());
        let socket_addr: SocketAddr = peer_addr.parse().expect("Invalid peer address");
        let server_config = make_server_config().expect("Failed to create server config");
        let mut endpoint = quinn::Endpoint::server(server_config, socket_addr)
            .expect("Failed to bind UDP socket for QUIC endpoint");
        endpoint.set_default_client_config(make_client_config());

        Self {
            peer_addr,
            peer_id,
            routing_table: RwLock::new(HashSet::new()),
            providers: RwLock::new(HashMap::new()),
            local_reader,
            local_ips,
            local_ip_counter: AtomicUsize::new(0),
            endpoint,
            connection_pool: Mutex::new(HashMap::new()),
        }
    }

    async fn get_quic_connection(&self, peer_addr: &str) -> std::io::Result<quinn::Connection> {
        {
            let pool = self.connection_pool.lock();
            if let Some(conn) = pool.get(peer_addr).filter(|c| c.close_reason().is_none()) {
                return Ok(conn.clone());
            }
        }

        let remote_addr: SocketAddr = peer_addr
            .parse()
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidInput, e))?;

        let conn = self
            .endpoint
            .connect(remote_addr, "localhost")
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::ConnectionRefused, e))?
            .await
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::ConnectionRefused, e))?;

        let mut pool = self.connection_pool.lock();
        pool.insert(peer_addr.to_string(), conn.clone());
        Ok(conn)
    }

    pub fn peer_addr(&self) -> &str {
        &self.peer_addr
    }

    pub fn peer_id(&self) -> u64 {
        self.peer_id
    }

    /// Add a peer to the routing table.
    pub fn add_peer(&self, addr: String) {
        if addr != self.peer_addr {
            self.routing_table.write().insert(addr);
        }
    }

    /// Returns the address of the peer (including self) closest to `key_hash` in XOR distance.
    pub fn find_closest_peer(&self, key_hash: u64) -> String {
        let mut closest_addr = self.peer_addr.clone();
        let mut min_distance = self.peer_id ^ key_hash;

        let table = self.routing_table.read();
        for peer in table.iter() {
            let pid = xxh3_64(peer.as_bytes());
            let distance = pid ^ key_hash;
            if distance < min_distance {
                min_distance = distance;
                closest_addr = peer.clone();
            }
        }
        closest_addr
    }

    /// Returns the top `count` closest peers (including self if applicable) to `key_hash`
    /// sorted by XOR distance in ascending order.
    pub fn find_closest_peers(&self, key_hash: u64, count: usize) -> Vec<String> {
        let mut peers = Vec::new();
        peers.push((self.peer_addr.clone(), self.peer_id));

        {
            let table = self.routing_table.read();
            for peer in table.iter() {
                let pid = xxh3_64(peer.as_bytes());
                peers.push((peer.clone(), pid));
            }
        }

        peers.sort_by_key(|(_, pid)| pid ^ key_hash);
        peers
            .into_iter()
            .take(count)
            .map(|(addr, _)| addr)
            .collect()
    }

    /// Update the routing table with the exact set of active peers.
    pub fn set_peers(&self, mut peers: std::collections::HashSet<String>) {
        peers.remove(&self.peer_addr);
        let mut table = self.routing_table.write();
        *table = peers;
    }

    pub fn get_local_value(&self, key: &Bytes) -> Option<Bytes> {
        self.local_reader.get_local(key)
    }

    /// Start the DHT node UDP listener.
    pub async fn start(self: Arc<Self>) -> std::io::Result<()> {
        let node = self.clone();
        tokio::spawn(async move {
            while let Some(conn) = node.endpoint.accept().await {
                let node_clone = node.clone();
                tokio::spawn(async move {
                    if let Err(e) = node_clone.handle_connection(conn).await {
                        eprintln!("DHT connection error: {:?}", e);
                    }
                });
            }
        });
        Ok(())
    }

    async fn handle_connection(self: Arc<Self>, conn: quinn::Incoming) -> std::io::Result<()> {
        let connection = conn
            .await
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::ConnectionAborted, e))?;

        loop {
            let (mut send, mut recv) = match connection.accept_bi().await {
                Ok(streams) => streams,
                Err(_) => break, // Connection closed/lost
            };

            let node = self.clone();
            tokio::spawn(async move {
                if let Err(e) = node.handle_stream(&mut send, &mut recv).await {
                    eprintln!("DHT stream error: {:?}", e);
                }
            });
        }
        Ok(())
    }

    async fn handle_stream(
        &self,
        send: &mut quinn::SendStream,
        recv: &mut quinn::RecvStream,
    ) -> std::io::Result<()> {
        let msg = read_msg(recv).await?;

        match msg {
            Message::Ping => {
                write_msg(send, &Message::Pong).await?;
            }
            Message::RegisterProvider {
                key_hash,
                provider_addr,
            } => {
                self.providers.write().insert(key_hash, provider_addr);
                write_msg(send, &Message::Pong).await?;
            }
            Message::FindProvider { key_hash } => {
                let closest = self.find_closest_peer(key_hash);
                if closest == self.peer_addr {
                    let provider_addr = self.providers.read().get(&key_hash).cloned();
                    write_msg(send, &Message::ProviderResponse { provider_addr }).await?;
                } else {
                    let provider_addr = self
                        .forward_find_provider(&closest, key_hash)
                        .await
                        .unwrap_or(None);
                    write_msg(send, &Message::ProviderResponse { provider_addr }).await?;
                }
            }
            Message::FetchValue { key } => {
                let value = self.local_reader.get_local(&key);
                write_msg(send, &Message::ValueResponse { value }).await?;
            }
            Message::StoreValue { key, value } => {
                self.local_reader.put_local(key, value);
                write_msg(send, &Message::Pong).await?;
            }
            _ => {}
        }

        let _ = send.finish();
        Ok(())
    }

    async fn forward_find_provider(
        &self,
        peer_addr: &str,
        key_hash: u64,
    ) -> std::io::Result<Option<String>> {
        let conn = self.get_quic_connection(peer_addr).await?;
        match self.try_find_provider_on_conn(&conn, key_hash).await {
            Ok(res) => Ok(res),
            Err(_) => {
                {
                    let mut pool = self.connection_pool.lock();
                    pool.remove(peer_addr);
                }
                let conn = self.get_quic_connection(peer_addr).await?;
                self.try_find_provider_on_conn(&conn, key_hash).await
            }
        }
    }

    async fn try_find_provider_on_conn(
        &self,
        conn: &quinn::Connection,
        key_hash: u64,
    ) -> std::io::Result<Option<String>> {
        let (mut send, mut recv) = conn
            .open_bi()
            .await
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::ConnectionAborted, e))?;

        write_msg(&mut send, &Message::FindProvider { key_hash }).await?;
        let _ = send.finish();

        match read_msg(&mut recv).await? {
            Message::ProviderResponse { provider_addr } => Ok(provider_addr),
            _ => Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "Invalid response",
            )),
        }
    }

    /// Register ourselves as the provider of key `K` in the DHT.
    /// Finds the node closest to `hash(K)` and registers the provider.
    pub async fn register_provider(&self, key_hash: u64) -> std::io::Result<()> {
        self.providers
            .write()
            .insert(key_hash, self.peer_addr.clone());

        let closest = self.find_closest_peer(key_hash);
        if closest == self.peer_addr {
            Ok(())
        } else {
            let conn = self.get_quic_connection(&closest).await?;
            match self.try_register_on_conn(&conn, key_hash).await {
                Ok(()) => Ok(()),
                Err(_) => {
                    {
                        let mut pool = self.connection_pool.lock();
                        pool.remove(&closest);
                    }
                    let conn = self.get_quic_connection(&closest).await?;
                    self.try_register_on_conn(&conn, key_hash).await
                }
            }
        }
    }

    async fn try_register_on_conn(
        &self,
        conn: &quinn::Connection,
        key_hash: u64,
    ) -> std::io::Result<()> {
        let (mut send, mut recv) = conn
            .open_bi()
            .await
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::ConnectionAborted, e))?;

        write_msg(
            &mut send,
            &Message::RegisterProvider {
                key_hash,
                provider_addr: self.peer_addr.clone(),
            },
        )
        .await?;
        let _ = send.finish();

        match read_msg(&mut recv).await? {
            Message::Pong => Ok(()),
            _ => Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "Invalid response",
            )),
        }
    }

    /// Queries the DHT to find the provider address for `key_hash`.
    pub async fn find_provider(&self, key_hash: u64) -> std::io::Result<Option<String>> {
        let closest = self.find_closest_peer(key_hash);
        if closest == self.peer_addr {
            Ok(self.providers.read().get(&key_hash).cloned())
        } else {
            self.forward_find_provider(&closest, key_hash).await
        }
    }

    /// Directly fetches a value from a remote provider node.
    pub async fn fetch_remote_value(
        &self,
        provider_addr: &str,
        key: Bytes,
    ) -> std::io::Result<Option<Bytes>> {
        let conn = self.get_quic_connection(provider_addr).await?;
        match self.try_fetch_on_conn(&conn, &key).await {
            Ok(res) => Ok(res),
            Err(_) => {
                {
                    let mut pool = self.connection_pool.lock();
                    pool.remove(provider_addr);
                }
                let conn = self.get_quic_connection(provider_addr).await?;
                self.try_fetch_on_conn(&conn, &key).await
            }
        }
    }

    async fn try_fetch_on_conn(
        &self,
        conn: &quinn::Connection,
        key: &Bytes,
    ) -> std::io::Result<Option<Bytes>> {
        let (mut send, mut recv) = conn
            .open_bi()
            .await
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::ConnectionAborted, e))?;

        write_msg(&mut send, &Message::FetchValue { key: key.clone() }).await?;
        let _ = send.finish();

        match read_msg(&mut recv).await? {
            Message::ValueResponse { value } => Ok(value),
            _ => Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "Invalid response",
            )),
        }
    }

    /// Directly stores a value on a remote peer.
    pub async fn store_remote_value(
        &self,
        peer_addr: &str,
        key: Bytes,
        value: Bytes,
    ) -> std::io::Result<()> {
        let conn = self.get_quic_connection(peer_addr).await?;
        match self.try_store_on_conn(&conn, &key, &value).await {
            Ok(()) => Ok(()),
            Err(_) => {
                {
                    let mut pool = self.connection_pool.lock();
                    pool.remove(peer_addr);
                }
                let conn = self.get_quic_connection(peer_addr).await?;
                self.try_store_on_conn(&conn, &key, &value).await
            }
        }
    }

    async fn try_store_on_conn(
        &self,
        conn: &quinn::Connection,
        key: &Bytes,
        value: &Bytes,
    ) -> std::io::Result<()> {
        let (mut send, mut recv) = conn
            .open_bi()
            .await
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::ConnectionAborted, e))?;

        write_msg(
            &mut send,
            &Message::StoreValue {
                key: key.clone(),
                value: value.clone(),
            },
        )
        .await?;
        let _ = send.finish();

        match read_msg(&mut recv).await? {
            Message::Pong => Ok(()),
            _ => Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "Invalid response",
            )),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct DummyCache;
    impl LocalCacheReader for DummyCache {
        fn get_local(&self, key: &Bytes) -> Option<Bytes> {
            if key == &Bytes::from("hello") {
                Some(Bytes::from("world"))
            } else {
                None
            }
        }
        fn put_local(&self, _key: Bytes, _value: Bytes) {}
    }

    #[tokio::test]
    async fn test_dht_p2p_basic() {
        // Spawn Node 1 (runs on 127.0.0.1:23001)
        let node1 = Arc::new(DhtNode::new(
            "127.0.0.1:23001".to_string(),
            Arc::new(DummyCache),
        ));
        node1.clone().start().await.unwrap();

        // Spawn Node 2 (runs on 127.0.0.1:23002)
        let node2 = Arc::new(DhtNode::new(
            "127.0.0.1:23002".to_string(),
            Arc::new(DummyCache),
        ));
        node2.clone().start().await.unwrap();

        // Join nodes by linking them
        node1.add_peer("127.0.0.1:23002".to_string());
        node2.add_peer("127.0.0.1:23001".to_string());

        // Node 2 registers provider of some key
        let key_hash = xxh3_64(b"test_key");
        node2.register_provider(key_hash).await.unwrap();

        // Node 1 queries the DHT for the provider of key_hash
        let provider = node1.find_provider(key_hash).await.unwrap();
        assert!(provider.is_some());
        assert_eq!(provider.unwrap(), "127.0.0.1:23002");

        // Node 1 fetches the remote value directly from the provider
        let val = node1
            .fetch_remote_value("127.0.0.1:23002", Bytes::from("hello"))
            .await
            .unwrap();
        assert_eq!(val, Some(Bytes::from("world")));
    }
}
