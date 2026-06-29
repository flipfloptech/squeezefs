use bytes::Bytes;
use rcgen::{BasicConstraints, CertificateParams, DnType, IsCa, KeyPair, KeyUsagePurpose};
use squeezefs::tiering::dht::{ClusterSecurityConfig, DhtNode, LocalCacheReader};
use std::sync::Arc;

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
    let cert_der = ca_cert.der().to_vec();
    let key_der = ca_key_pair.serialize_der();
    (cert_der, key_der)
}

#[tokio::test]
async fn test_mtls_authorized_connection() {
    let (ca_cert, ca_key) = generate_test_ca();
    let security_config = ClusterSecurityConfig {
        ca_cert: Some(ca_cert),
        ca_key: Some(ca_key),
    };

    // Node 1 (runs on port 23201)
    let node1 = Arc::new(DhtNode::new_with_security(
        "127.0.0.1:23201".to_string(),
        Arc::new(DummyCache),
        security_config.clone(),
    ));
    node1.clone().start().await.unwrap();

    // Node 2 (runs on port 23202)
    let node2 = Arc::new(DhtNode::new_with_security(
        "127.0.0.1:23202".to_string(),
        Arc::new(DummyCache),
        security_config.clone(),
    ));
    node2.clone().start().await.unwrap();

    // Join nodes
    node1.add_peer("127.0.0.1:23202".to_string());

    // Fetch value - should succeed since both present valid certificates signed by same CA
    let val = node1
        .fetch_remote_value("127.0.0.1:23202", Bytes::from("hello"))
        .await
        .unwrap();

    assert_eq!(val, Some(Bytes::from("world")));
}

#[tokio::test]
async fn test_mtls_unauthorized_connection() {
    let (ca_cert, ca_key) = generate_test_ca();
    let security_config_node1 = ClusterSecurityConfig {
        ca_cert: Some(ca_cert),
        ca_key: Some(ca_key),
    };

    // Node 1 (runs on port 23203, uses Cluster CA)
    let node1 = Arc::new(DhtNode::new_with_security(
        "127.0.0.1:23203".to_string(),
        Arc::new(DummyCache),
        security_config_node1,
    ));
    node1.clone().start().await.unwrap();

    // Node 2 (runs on port 23204, uses a completely different CA/self-signed config)
    let (wrong_ca_cert, wrong_ca_key) = generate_test_ca();
    let security_config_node2 = ClusterSecurityConfig {
        ca_cert: Some(wrong_ca_cert),
        ca_key: Some(wrong_ca_key),
    };
    let node2 = Arc::new(DhtNode::new_with_security(
        "127.0.0.1:23204".to_string(),
        Arc::new(DummyCache),
        security_config_node2,
    ));
    node2.clone().start().await.unwrap();

    // Tries to fetch remote value - should fail because cert verifiers reject foreign certs
    let result = node1
        .fetch_remote_value("127.0.0.1:23204", Bytes::from("hello"))
        .await;

    assert!(
        result.is_err(),
        "Connection from node2 should be rejected because certificates do not match CA"
    );
}
