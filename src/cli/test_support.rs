//! A real engram on a real port, for the client tests: the client speaks only HTTP.

/// `(base_url, token)`, plus the core behind them.
pub async fn serve_test_app() -> (String, String, crate::core::Core) {
    let core = crate::core::test_support::test_core().await;
    let handle = core.clone();
    let (app, token) = crate::web::test_support::app_with_token(core).await;
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("a port");
    let addr = listener.local_addr().expect("the port it got");
    tokio::spawn(async move {
        axum::serve(listener, app).await.expect("serve");
    });
    (format!("http://{addr}"), token, handle)
}
