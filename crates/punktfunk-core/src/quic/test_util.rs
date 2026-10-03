use super::endpoint;

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

/// [`connect_pair`] negotiating `punktfunk/2` on the shared-socket endpoints.
pub(crate) async fn connect_pair_v2() -> (
    quinn::Endpoint,
    quinn::Endpoint,
    quinn::Connection,
    quinn::Connection,
) {
    let (cert, key) = endpoint::generate_identity().unwrap();
    let (server, _media) = endpoint::server_shared(
        "127.0.0.1:0".parse().unwrap(),
        &cert,
        &key,
        std::time::Duration::from_secs(8),
    )
    .unwrap();
    let addr = server.local_addr().unwrap();
    let (client, _) = endpoint::client_shared(None, None, &[super::v2::registry::ALPN]);
    let (client, _media) = client.unwrap();
    let accept = tokio::spawn({
        let server = server.clone();
        async move { server.accept().await.unwrap().await.unwrap() }
    });
    let client_conn = client.connect(addr, "punktfunk").unwrap().await.unwrap();
    (server, client, accept.await.unwrap(), client_conn)
}
