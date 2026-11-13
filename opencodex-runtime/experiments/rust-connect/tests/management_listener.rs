use remodex_connect_native::{
    auth::Auth, dashboard::Dashboard, management_listener, server::App, store::Store,
};
use std::net::SocketAddr;
#[tokio::test]
async fn dashboard_and_tunnel_phone_origins_stay_separate_and_close_together() {
    let root = tempfile::tempdir().unwrap();
    std::fs::write(root.path().join("index.html"), "<html>fixture</html>").unwrap();
    let phone = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let phone_address = phone.local_addr().unwrap();
    let admin = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let admin_address = admin.local_addr().unwrap();
    let mut app = App::new(
        Auth::new(Store::open(&root.path().join("state")).unwrap()),
        "Fixture",
    )
    .unwrap();
    app.dashboard = Some(Dashboard::new(root.path(), &format!("http://{phone_address}")).unwrap());
    let management = management_listener::Server::start(admin, app.clone());
    let signal = app.stopping.clone();
    let mut stopping = signal.subscribe();
    let phone_task = tokio::spawn(async move {
        axum::serve(
            phone,
            app.phone_router()
                .into_make_service_with_connect_info::<SocketAddr>(),
        )
        .with_graceful_shutdown(async move {
            let _ = stopping.wait_for(|v| *v).await;
        })
        .await
        .unwrap();
    });
    let client = reqwest::Client::builder().no_proxy().build().unwrap();
    let session = client
        .get(format!("http://{admin_address}/opencodex-session"))
        .send()
        .await
        .unwrap();
    assert_eq!(session.status(), 200);
    assert!(session
        .text()
        .await
        .unwrap()
        .contains("opencodex-session-token"));
    for path in ["/opencodex-session", "/api/android-remote", "/"] {
        let response = client
            .get(format!("http://{phone_address}{path}"))
            .header("Host", admin_address.to_string())
            .send()
            .await
            .unwrap();
        assert!(
            !response.status().is_success(),
            "phone route {path} exposed administration"
        );
        assert!(!response
            .text()
            .await
            .unwrap()
            .contains("opencodex-session-token"));
    }
    assert_eq!(
        client
            .get(format!("http://{phone_address}/.well-known/t3/environment"))
            .send()
            .await
            .unwrap()
            .status(),
        200
    );
    signal.send_replace(true);
    management.shutdown().await.unwrap();
    phone_task.await.unwrap();
    for address in [phone_address, admin_address] {
        assert!(tokio::net::TcpStream::connect(address).await.is_err());
    }
}
