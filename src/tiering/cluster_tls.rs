//! Cluster TLS/mTLS construction — the cert/CA machinery shared across
//! cluster wire surfaces (design-volume-lifecycle KD-15: the reuse
//! boundary is this rustls construction, never any transport wrap).
//!
//! Consumer: **`src/cluster_wire.rs`** (DLM S3 — the one cluster
//! transport; `job_wire` rides it). This module formerly lived inside the
//! p2p/DHT subsystem (`src/tiering/dht.rs`), deleted as unreachable
//! (pre-rc spec ENG-13); the TLS core survives because the cluster wire
//! is live machinery.
//!
//! # There is exactly one TLS posture here: CA-pinned mTLS
//!
//! DLM S3 **deleted the accept-everything certificate verifier**. The
//! CA-less posture used to install a `.dangerous()` client verifier
//! against a `with_no_client_auth()` self-signed server — a TLS object
//! that authenticated nobody, which VAL-6 then had to special-case out of
//! the verification-strength ladder. Both constructors now REQUIRE the CA
//! pair ([`ClusterCa`]), so the type system carries the invariant the
//! ladder depends on: a TLS session on this wire is mutually
//! authenticated, or it does not exist. The honest alternative to mTLS is
//! plaintext plus cluster_wire's storage-trust authentication (a
//! server-issued challenge, a possession proof of the shared volume's
//! `job:enroll` secret, and a per-frame session MAC).

use rcgen::{BasicConstraints, CertificateParams, DnType, IsCa, KeyPair};
use std::sync::{Arc, Once};

use rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer};

/// Cluster CA material as an operator CONFIGURES it (both halves
/// optional, because a config file / env pair can carry either, neither,
/// or a half). [`ClusterSecurityConfig::ca_pair`] is the one place a
/// half-configured pair becomes a refusal instead of a panic.
#[derive(Clone, Debug, Default)]
pub struct ClusterSecurityConfig {
    pub ca_cert: Option<Vec<u8>>,
    pub ca_key: Option<Vec<u8>>,
}

/// A COMPLETE cluster CA pair — the only thing this module will build a
/// rustls configuration from. The node certificate the cluster machinery
/// presents is signed by the CA key, so a cert without its key can never
/// produce an authenticated channel; making that unrepresentable is
/// cheaper than checking for it at every call site (it used to be an
/// `unwrap()` waiting for the first connection).
#[derive(Clone, Debug)]
pub struct ClusterCa {
    pub cert_der: Vec<u8>,
    pub key_der: Vec<u8>,
}

impl ClusterSecurityConfig {
    /// The complete pair, or `None` when either half is missing.
    pub fn ca_pair(&self) -> Option<ClusterCa> {
        match (self.ca_cert.as_ref(), self.ca_key.as_ref()) {
            (Some(cert), Some(key)) => Some(ClusterCa {
                cert_der: cert.clone(),
                key_der: key.clone(),
            }),
            _ => None,
        }
    }
}

static RUSTLS_INIT: Once = Once::new();

fn init_rustls() {
    RUSTLS_INIT.call_once(|| {
        let _ = rustls::crypto::ring::default_provider().install_default();
    });
}

fn generate_node_cert_signed_by_ca(
    ca_cert_der: &[u8],
    ca_key_der: &[u8],
) -> std::io::Result<(Vec<CertificateDer<'static>>, PrivateKeyDer<'static>)> {
    let ca_key_pair = KeyPair::try_from(ca_key_der.to_vec())
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidInput, e))?;

    let mut ca_params = CertificateParams::default();
    ca_params.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
    ca_params
        .distinguished_name
        .push(DnType::CommonName, "SqueezeFS Cluster CA");
    let ca_cert = ca_params
        .self_signed(&ca_key_pair)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidInput, e))?;

    let mut node_params =
        CertificateParams::new(vec!["localhost".to_string(), "127.0.0.1".to_string()])
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidInput, e))?;
    node_params
        .distinguished_name
        .push(DnType::CommonName, "SqueezeFS Node");

    let node_key_pair = KeyPair::generate()
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidInput, e))?;

    let node_cert = node_params
        .signed_by(&node_key_pair, &ca_cert, &ca_key_pair)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidInput, e))?;

    let certs = vec![
        CertificateDer::from(node_cert.der().to_vec()),
        CertificateDer::from(ca_cert_der.to_vec()),
    ];
    let key = PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(node_key_pair.serialize_der()));

    Ok((certs, key))
}

/// The transport-agnostic rustls **server** construction: CA-pinned mTLS,
/// the only posture this module builds. The client-cert verifier is rooted
/// at the cluster CA and the node cert is signed by it, so both directions
/// are authenticated. Consumed by `cluster_wire::tls_acceptor`.
pub(crate) fn rustls_server_config(ca: &ClusterCa) -> std::io::Result<rustls::ServerConfig> {
    init_rustls();

    let mut roots = rustls::RootCertStore::empty();
    roots
        .add(CertificateDer::from(ca.cert_der.clone()))
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidInput, e))?;
    let client_cert_verifier = rustls::server::WebPkiClientVerifier::builder(Arc::new(roots))
        .build()
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidInput, e))?;

    let (certs, key) = generate_node_cert_signed_by_ca(&ca.cert_der, &ca.key_der)?;

    rustls::ServerConfig::builder()
        .with_client_cert_verifier(client_cert_verifier)
        .with_single_cert(certs, key)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidInput, e))
}

/// The transport-agnostic rustls **client** construction: CA-rooted
/// validation plus the CA-signed client-auth cert. Consumed by
/// `cluster_wire::tls_connector`.
pub(crate) fn rustls_client_config(ca: &ClusterCa) -> std::io::Result<rustls::ClientConfig> {
    init_rustls();

    let mut roots = rustls::RootCertStore::empty();
    roots
        .add(CertificateDer::from(ca.cert_der.clone()))
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidInput, e))?;

    let (certs, key) = generate_node_cert_signed_by_ca(&ca.cert_der, &ca.key_der)?;

    rustls::ClientConfig::builder()
        .with_root_certificates(roots)
        .with_client_auth_cert(certs, key)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidInput, e))
}

#[cfg(test)]
mod tests {
    //! mTLS contract pins ported from the deleted `tests/mtls_tests.rs`
    //! (which exercised them through the deleted DhtNode/quinn wrap):
    //! the CA-pinned server admits a CA-carrying client and refuses a
    //! client that presents no certificate. Exercised over tokio-rustls —
    //! the transport the live consumer (`cluster_wire`) actually uses.
    //!
    //! DLM S3 note: the refusal leg used to build its client through this
    //! module with a CA-less config, which is exactly the
    //! accept-everything posture that was deleted. It now builds a bare
    //! rustls client that TRUSTS the CA but presents no client cert — the
    //! same property (the server refuses an unauthenticated client),
    //! proven without a dangerous verifier existing anywhere in the tree.

    use super::*;
    use rcgen::KeyUsagePurpose;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    fn generate_test_ca() -> (Vec<u8>, Vec<u8>) {
        let mut ca_params = CertificateParams::default();
        ca_params.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
        ca_params
            .distinguished_name
            .push(DnType::CommonName, "SqueezeFS Cluster CA");
        ca_params.key_usages = vec![
            KeyUsagePurpose::KeyCertSign,
            KeyUsagePurpose::DigitalSignature,
        ];
        let ca_key_pair = KeyPair::generate().unwrap();
        let ca_cert = ca_params.self_signed(&ca_key_pair).unwrap();
        (ca_cert.der().to_vec(), ca_key_pair.serialize_der())
    }

    async fn spawn_tls_echo_server(
        ca: &ClusterCa,
    ) -> (std::net::SocketAddr, tokio::task::JoinHandle<()>) {
        let server_cfg = rustls_server_config(ca).expect("server config");
        let acceptor = tokio_rustls::TlsAcceptor::from(Arc::new(server_cfg));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind");
        let addr = listener.local_addr().expect("local addr");
        let handle = tokio::spawn(async move {
            if let Ok((stream, _)) = listener.accept().await {
                // A failed handshake (unauthorized client) just drops.
                if let Ok(mut tls) = acceptor.accept(stream).await {
                    let mut buf = [0u8; 5];
                    if tls.read_exact(&mut buf).await.is_ok() {
                        let _ = tls.write_all(&buf).await;
                    }
                }
            }
        });
        (addr, handle)
    }

    async fn tls_echo_roundtrip(
        addr: std::net::SocketAddr,
        client_cfg: rustls::ClientConfig,
    ) -> std::io::Result<[u8; 5]> {
        let connector = tokio_rustls::TlsConnector::from(Arc::new(client_cfg));
        let stream = tokio::net::TcpStream::connect(addr).await?;
        let server_name = rustls::pki_types::ServerName::try_from("localhost")
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidInput, e))?;
        let mut tls = connector.connect(server_name, stream).await?;
        tls.write_all(b"hello").await?;
        let mut buf = [0u8; 5];
        tls.read_exact(&mut buf).await?;
        Ok(buf)
    }

    /// A client that VALIDATES the cluster CA but presents no client
    /// certificate — the honest shape of "unauthorized peer" now that the
    /// accept-everything verifier is gone.
    fn certless_client(ca: &ClusterCa) -> rustls::ClientConfig {
        init_rustls();
        let mut roots = rustls::RootCertStore::empty();
        roots
            .add(CertificateDer::from(ca.cert_der.clone()))
            .expect("root add");
        rustls::ClientConfig::builder()
            .with_root_certificates(roots)
            .with_no_client_auth()
    }

    #[tokio::test]
    async fn mtls_authorized_client_completes_handshake() {
        let (ca_cert, ca_key) = generate_test_ca();
        let ca = ClusterSecurityConfig {
            ca_cert: Some(ca_cert),
            ca_key: Some(ca_key),
        }
        .ca_pair()
        .expect("a complete pair");
        let (addr, server) = spawn_tls_echo_server(&ca).await;
        let echoed = tls_echo_roundtrip(addr, rustls_client_config(&ca).expect("client config"))
            .await
            .expect("CA-carrying client must complete the mTLS handshake");
        assert_eq!(&echoed, b"hello", "echo through the mTLS session");
        server.await.expect("server task");
    }

    #[tokio::test]
    async fn mtls_unauthorized_client_refused() {
        let (ca_cert, ca_key) = generate_test_ca();
        let ca = ClusterSecurityConfig {
            ca_cert: Some(ca_cert),
            ca_key: Some(ca_key),
        }
        .ca_pair()
        .expect("a complete pair");
        let (addr, server) = spawn_tls_echo_server(&ca).await;
        // No client certificate: the CA-pinned server's WebPki client
        // verifier must refuse it.
        let result = tls_echo_roundtrip(addr, certless_client(&ca)).await;
        assert!(
            result.is_err(),
            "cert-less client must be refused by the CA-pinned server"
        );
        server.abort();
    }

    #[test]
    fn a_half_configured_ca_is_never_a_pair() {
        // The construction that used to reach an `unwrap()` on the first
        // connection is now unrepresentable: no pair, no rustls config.
        assert!(ClusterSecurityConfig::default().ca_pair().is_none());
        assert!(ClusterSecurityConfig {
            ca_cert: Some(vec![1, 2, 3]),
            ca_key: None,
        }
        .ca_pair()
        .is_none());
        assert!(ClusterSecurityConfig {
            ca_cert: None,
            ca_key: Some(vec![1, 2, 3]),
        }
        .ca_pair()
        .is_none());
    }
}
