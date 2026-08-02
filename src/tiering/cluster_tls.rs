//! Cluster TLS/mTLS construction — the cert/CA/verifier machinery shared
//! across cluster wire surfaces (design-volume-lifecycle KD-15: the reuse
//! boundary is this rustls construction, never any transport wrap).
//!
//! Sole consumer today: the §5.1.6 job-shard execution wire
//! (`src/job_wire.rs`, tokio-rustls acceptor/connector). This module
//! formerly lived inside the p2p/DHT subsystem (`src/tiering/dht.rs`),
//! deleted as unreachable (pre-rc spec ENG-13); the TLS core survives
//! because the job wire is live machinery.

use rcgen::{BasicConstraints, CertificateParams, DnType, IsCa, KeyPair};
use std::sync::{Arc, Once};

use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer, ServerName, UnixTime};
use rustls::{DigitallySignedStruct, SignatureScheme};

/// Cluster CA material for mTLS. With a CA configured, servers pin
/// client certs to it and clients validate + present CA-signed certs;
/// without one, servers run self-signed and clients skip verification
/// (the plaintext-adjacent dev posture — the job wire logs it loudly).
#[derive(Clone, Debug, Default)]
pub struct ClusterSecurityConfig {
    pub ca_cert: Option<Vec<u8>>,
    pub ca_key: Option<Vec<u8>>,
}

static RUSTLS_INIT: Once = Once::new();

fn init_rustls() {
    RUSTLS_INIT.call_once(|| {
        let _ = rustls::crypto::ring::default_provider().install_default();
    });
}

/// Verification-skipping client verifier for the no-CA posture only
/// (matches the self-signed server side). VAL-6 hardening owns refusing
/// CA-less configs on custody-bearing wires.
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

/// The transport-agnostic rustls **server** construction over
/// [`ClusterSecurityConfig`] — CA-pinned mTLS (client-cert verifier +
/// CA-signed node cert) when a CA is configured, self-signed otherwise.
/// Consumed by the §5.1.6 job wire's tokio-rustls acceptor
/// (design-volume-lifecycle KD-15).
pub(crate) fn rustls_server_config(
    security: &ClusterSecurityConfig,
) -> std::io::Result<rustls::ServerConfig> {
    init_rustls();

    let server_config = if let Some(ref ca_cert_der) = security.ca_cert {
        let mut roots = rustls::RootCertStore::empty();
        roots
            .add(CertificateDer::from(ca_cert_der.clone()))
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidInput, e))?;
        let client_cert_verifier = rustls::server::WebPkiClientVerifier::builder(Arc::new(roots))
            .build()
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidInput, e))?;

        let (certs, key) =
            generate_node_cert_signed_by_ca(ca_cert_der, security.ca_key.as_ref().unwrap())?;

        rustls::ServerConfig::builder()
            .with_client_cert_verifier(client_cert_verifier)
            .with_single_cert(certs, key)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidInput, e))?
    } else {
        let cert = rcgen::generate_simple_self_signed(vec![
            "localhost".to_string(),
            "127.0.0.1".to_string(),
        ])
        .map_err(std::io::Error::other)?;
        let certs = vec![CertificateDer::from(cert.cert.der().to_vec())];
        let key = PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(cert.key_pair.serialize_der()));

        rustls::ServerConfig::builder()
            .with_no_client_auth()
            .with_single_cert(certs, key)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidInput, e))?
    };
    Ok(server_config)
}

/// The transport-agnostic rustls **client** construction over
/// [`ClusterSecurityConfig`] — CA-rooted validation + client-auth cert
/// when a CA is configured, verification-less otherwise (matching the
/// self-signed server posture). Consumed by the §5.1.6 job wire's
/// tokio-rustls connector.
pub(crate) fn rustls_client_config(
    security: &ClusterSecurityConfig,
) -> std::io::Result<rustls::ClientConfig> {
    init_rustls();

    let client_config = if let Some(ref ca_cert_der) = security.ca_cert {
        let mut roots = rustls::RootCertStore::empty();
        roots
            .add(CertificateDer::from(ca_cert_der.clone()))
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidInput, e))?;

        let (certs, key) =
            generate_node_cert_signed_by_ca(ca_cert_der, security.ca_key.as_ref().unwrap())?;

        rustls::ClientConfig::builder()
            .with_root_certificates(roots)
            .with_client_auth_cert(certs, key)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidInput, e))?
    } else {
        let client_config = rustls::ClientConfig::builder()
            .dangerous()
            .with_custom_certificate_verifier(Arc::new(DummyVerifier))
            .with_no_client_auth();
        client_config
    };
    Ok(client_config)
}

#[cfg(test)]
mod tests {
    //! mTLS contract pins ported from the deleted `tests/mtls_tests.rs`
    //! (which exercised them through the deleted DhtNode/quinn wrap):
    //! CA-pinned server admits a CA-carrying client and refuses a
    //! CA-less one. Exercised over tokio-rustls — the transport the live
    //! consumer (job wire) actually uses.

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
        security: &ClusterSecurityConfig,
    ) -> (std::net::SocketAddr, tokio::task::JoinHandle<()>) {
        let server_cfg = rustls_server_config(security).expect("server config");
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
        client_security: &ClusterSecurityConfig,
    ) -> std::io::Result<[u8; 5]> {
        let client_cfg = rustls_client_config(client_security)?;
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

    #[tokio::test]
    async fn mtls_authorized_client_completes_handshake() {
        let (ca_cert, ca_key) = generate_test_ca();
        let security = ClusterSecurityConfig {
            ca_cert: Some(ca_cert),
            ca_key: Some(ca_key),
        };
        let (addr, server) = spawn_tls_echo_server(&security).await;
        let echoed = tls_echo_roundtrip(addr, &security)
            .await
            .expect("CA-carrying client must complete the mTLS handshake");
        assert_eq!(&echoed, b"hello", "echo through the mTLS session");
        server.await.expect("server task");
    }

    #[tokio::test]
    async fn mtls_unauthorized_client_refused() {
        let (ca_cert, ca_key) = generate_test_ca();
        let security = ClusterSecurityConfig {
            ca_cert: Some(ca_cert),
            ca_key: Some(ca_key),
        };
        let (addr, server) = spawn_tls_echo_server(&security).await;
        // No CA on the client: no client cert is presented, so the
        // CA-pinned server's WebPki client verifier must refuse it.
        let result = tls_echo_roundtrip(addr, &ClusterSecurityConfig::default()).await;
        assert!(
            result.is_err(),
            "cert-less client must be refused by the CA-pinned server"
        );
        server.abort();
    }
}
