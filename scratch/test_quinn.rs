fn main() {
    let socket = std::net::UdpSocket::bind("127.0.0.1:0").unwrap();
    let _ = socket.set_send_buffer_size(16 * 1024 * 1024);
    let _ = socket.set_receive_buffer_size(16 * 1024 * 1024);
    
    // Test compilation of Endpoint::new
    // In Quinn 0.11, default-features is false but runtime-tokio is enabled.
    // Let's check how DhtNode initializes the Endpoint.
}
