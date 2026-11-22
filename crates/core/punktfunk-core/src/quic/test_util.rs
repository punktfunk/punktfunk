use super::endpoint;

/// `::1`, or `None` on a box without IPv6 loopback, where `test` says it skipped.
pub(crate) fn loopback_v6(test: &str) -> Option<std::net::IpAddr> {
    let ip = std::net::IpAddr::V6(std::net::Ipv6Addr::LOCALHOST);
    match std::net::UdpSocket::bind((ip, 0)) {
        Ok(_) => Some(ip),
        Err(e) => {
            eprintln!("{test}: skipped, no IPv6 loopback: {e}");
            None
        }
    }
}

/// Dropping a `quinn::Endpoint` closes its connections; keep both endpoints in the caller's scope.
pub(crate) async fn connect_pair() -> (
    quinn::Endpoint,
    quinn::Endpoint,
    quinn::Connection,
    quinn::Connection,
) {
    let server = endpoint::server("127.0.0.1:0".parse().unwrap()).unwrap();
    let addr = server.local_addr().unwrap();
    let client = endpoint::client_insecure().unwrap();
    let accept = tokio::spawn(async move {
        let incoming = server.accept().await.expect("incoming connection");
        let conn = incoming.await.expect("host side connects");
        (server, conn)
    });
    let client_conn = client
        .connect(addr, "punktfunk")
        .unwrap()
        .await
        .expect("client side connects");
    let (server, host_conn) = accept.await.unwrap();
    (server, client, host_conn, client_conn)
}
