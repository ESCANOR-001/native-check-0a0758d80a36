// The tungstenite handshake callback requires its unboxed HTTP error type.
#![allow(clippy::result_large_err)]
use futures_util::{SinkExt, StreamExt};
use remodex_connect_native::agent::{Connection, Event, MAX_FRAME};
use serde_json::json;
use std::time::Duration;
use tokio::net::TcpListener;
use tokio_tungstenite::tungstenite::{
    handshake::server::{Request, Response},
    Message,
};
const TOKEN: &str = "disposable-fixture-capability-token";

#[tokio::test]
async fn shared_token_is_revalidated_before_each_connection() {
    use remodex_connect_native::agent::SharedServer;
    let profile = tempfile::tempdir().unwrap();
    let other = tempfile::tempdir().unwrap();
    let path = profile.path().join("token");
    std::fs::write(&path, TOKEN).unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
    }
    let mut config = SharedServer::Capability {
        address: "127.0.0.1:18999".parse().unwrap(),
        token_file: path.clone(),
    };
    config.validate(profile.path()).unwrap();
    let other_path = other.path().join("token");
    if let SharedServer::Capability { token_file, .. } = &mut config {
        *token_file = other_path.clone();
    }
    std::fs::write(&other_path, TOKEN).unwrap();
    assert!(config.validate(profile.path()).is_err());
    if let SharedServer::Capability { token_file, .. } = &mut config {
        *token_file = path.clone();
    }
    std::fs::write(&path, "x".repeat(4097)).unwrap();
    assert!(config.validate(profile.path()).is_err());
    assert!(config.connect(profile.path()).await.is_err());
    std::fs::remove_file(&path).unwrap();
    std::fs::create_dir(&path).unwrap();
    assert!(config.validate(profile.path()).is_err());
    std::fs::remove_dir(&path).unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::{symlink, PermissionsExt};
        let target = profile.path().join("actual");
        std::fs::write(&target, TOKEN).unwrap();
        std::fs::set_permissions(&target, std::fs::Permissions::from_mode(0o600)).unwrap();
        symlink(&target, &path).unwrap();
        assert!(config.validate(profile.path()).is_err());
        std::fs::remove_file(&path).unwrap();
        std::fs::hard_link(&target, &path).unwrap();
        assert!(config.validate(profile.path()).is_err());
        std::fs::remove_file(&path).unwrap();
        std::fs::rename(&target, &path).unwrap();
        config.validate(profile.path()).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
        assert!(config.connect(profile.path()).await.is_err());
        std::fs::remove_file(&path).unwrap();
        let name = std::ffi::CString::new(path.as_os_str().as_encoded_bytes()).unwrap();
        assert_eq!(unsafe { libc::mkfifo(name.as_ptr(), 0o600) }, 0);
        assert!(config.validate(profile.path()).is_err());
    }
}

#[tokio::test]
async fn shared_connection_preserves_protocol_and_closing_one_client_keeps_server_available() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let server = tokio::spawn(async move {
        for _ in 0..2 {
            let (stream, _) = listener.accept().await.unwrap();
            let mut ws =
                tokio_tungstenite::accept_hdr_async(stream, |r: &Request, response: Response| {
                    assert_eq!(r.headers()["authorization"], format!("Bearer {TOKEN}"));
                    Ok(response)
                })
                .await
                .unwrap();
            while let Some(Ok(message)) = ws.next().await {
                if let Message::Text(_) = &message {
                    ws.send(message).await.unwrap();
                } else if message.is_close() {
                    break;
                }
            }
        }
    });
    for client in 0..2 {
        let mut c = Connection::connect_loopback(address, TOKEN).await.unwrap();
        for value in [
            json!({"id":client,"method":"turn/steer","params":{"expectedTurnId":"same","text":"你好 🙂"}}),
            json!({"id":"approval","result":{"decision":"decline"}}),
            json!({"text":"x".repeat(256*1024)}),
        ] {
            c.send(&value).await.unwrap();
            let Some(Event::Message(actual)) =
                tokio::time::timeout(Duration::from_secs(2), c.events.recv())
                    .await
                    .unwrap()
            else {
                panic!("Missing shared event")
            };
            assert_eq!(actual, value);
        }
        c.close().await;
    }
    tokio::time::timeout(Duration::from_secs(2), server)
        .await
        .unwrap()
        .unwrap();
}

#[tokio::test]
async fn shared_connection_rejects_external_addresses_and_failed_auth_without_leaking_token() {
    assert!(
        Connection::connect_loopback("192.0.2.1:18999".parse().unwrap(), TOKEN)
            .await
            .is_err()
    );
    assert!(
        Connection::connect_loopback("127.0.0.1:18999".parse().unwrap(), "bad\nheader-value")
            .await
            .is_err()
    );
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let server = tokio::spawn(async move {
        let (stream, _) = listener.accept().await.unwrap();
        let result = tokio_tungstenite::accept_hdr_async(stream, |_: &Request, _: Response| {
            Err(tokio_tungstenite::tungstenite::http::Response::builder()
                .status(401)
                .body(Some("Denied".into()))
                .unwrap())
        })
        .await;
        assert!(result.is_err());
    });
    let error = match Connection::connect_loopback(address, TOKEN).await {
        Ok(_) => panic!("Auth unexpectedly succeeded"),
        Err(e) => e,
    };
    assert!(!error.to_string().contains(TOKEN));
    server.await.unwrap();
}

#[tokio::test]
async fn shared_writes_continue_while_notification_consumer_is_backpressured() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let server = tokio::spawn(async move {
        let (stream, _) = listener.accept().await.unwrap();
        let mut ws = tokio_tungstenite::accept_async(stream).await.unwrap();
        for n in 0..8 {
            ws.send(Message::Text(json!({"notice":n}).to_string().into()))
                .await
                .unwrap();
        }
        let received = tokio::time::timeout(Duration::from_secs(2), ws.next())
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(received.to_text().unwrap()).unwrap()["id"],
            "prompt-once"
        );
        ws.send(Message::Text(
            json!({"id":"prompt-once","result":{}}).to_string().into(),
        ))
        .await
        .unwrap();
        while let Some(Ok(message)) = ws.next().await {
            if message.is_close() {
                break;
            }
        }
    });
    let mut c = Connection::connect_loopback(address, TOKEN).await.unwrap();
    tokio::time::sleep(Duration::from_millis(100)).await;
    tokio::time::timeout(
        Duration::from_secs(1),
        c.send(&json!({"id":"prompt-once","method":"turn/start"})),
    )
    .await
    .unwrap()
    .unwrap();
    for n in 0..9 {
        let Some(Event::Message(v)) = tokio::time::timeout(Duration::from_secs(2), c.events.recv())
            .await
            .unwrap()
        else {
            panic!("Lost ordered notification")
        };
        if n < 8 {
            assert_eq!(v["notice"], n);
        } else {
            assert_eq!(v["id"], "prompt-once");
        }
    }
    c.close().await;
    server.await.unwrap();
}

#[tokio::test]
async fn shared_connection_closes_on_invalid_frames_without_reconnecting_or_replaying() {
    for oversized in [false, true] {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let mut ws = tokio_tungstenite::accept_async(stream).await.unwrap();
            let sent = ws.next().await.unwrap().unwrap();
            assert!(sent.is_text());
            let bad = if oversized {
                "x".repeat(MAX_FRAME + 1)
            } else {
                "{incomplete".into()
            };
            let _ = ws.send(Message::Text(bad.into())).await;
            let _ = tokio::time::timeout(Duration::from_secs(2), ws.next()).await;
            assert!(
                tokio::time::timeout(Duration::from_millis(100), listener.accept())
                    .await
                    .is_err()
            );
        });
        let mut c = Connection::connect_loopback(address, TOKEN).await.unwrap();
        c.send(&json!({"id":"only-once","method":"turn/start"}))
            .await
            .unwrap();
        assert!(matches!(
            tokio::time::timeout(Duration::from_secs(3), c.events.recv())
                .await
                .unwrap(),
            Some(Event::Closed { forced: true, .. })
        ));
        c.close().await;
        server.await.unwrap();
    }
}

#[tokio::test]
async fn closing_a_backpressured_shared_connection_does_not_wait_for_a_consumer() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let server = tokio::spawn(async move {
        let (stream, _) = listener.accept().await.unwrap();
        let mut ws = tokio_tungstenite::accept_async(stream).await.unwrap();
        for n in 0..8 {
            let _ = ws
                .send(Message::Text(json!({"id":n}).to_string().into()))
                .await;
        }
        while let Some(Ok(message)) = ws.next().await {
            if message.is_close() {
                break;
            }
        }
    });
    let c = Connection::connect_loopback(address, TOKEN).await.unwrap();
    tokio::time::sleep(Duration::from_millis(100)).await;
    tokio::time::timeout(Duration::from_secs(2), c.close())
        .await
        .unwrap();
    tokio::time::timeout(Duration::from_secs(2), server)
        .await
        .unwrap()
        .unwrap();
}

#[cfg(unix)]
#[tokio::test]
async fn private_unix_socket_uses_same_protocol_and_rejects_public_or_redirected_endpoints() {
    use remodex_connect_native::agent::SharedServer;
    use std::os::unix::fs::{symlink, PermissionsExt};
    let profile = tempfile::tempdir().unwrap();
    let outside = tempfile::tempdir().unwrap();
    let socket = profile.path().join("rpc.sock");
    let listener = tokio::net::UnixListener::bind(&socket).unwrap();
    std::fs::set_permissions(&socket, std::fs::Permissions::from_mode(0o600)).unwrap();
    let config = SharedServer::Unix {
        socket_path: socket.clone(),
    };
    config.validate(profile.path()).unwrap();
    assert!(config.validate(outside.path()).is_err());
    let link = profile.path().join("link.sock");
    symlink(&socket, &link).unwrap();
    assert!(SharedServer::Unix { socket_path: link }
        .validate(profile.path())
        .is_err());
    let ordinary = profile.path().join("ordinary");
    std::fs::write(&ordinary, "not a socket").unwrap();
    assert!(SharedServer::Unix {
        socket_path: ordinary
    }
    .validate(profile.path())
    .is_err());
    let server = tokio::spawn(async move {
        for _ in 0..2 {
            let (stream, _) = listener.accept().await.unwrap();
            let mut ws = tokio_tungstenite::accept_async(stream).await.unwrap();
            while let Some(Ok(message)) = ws.next().await {
                if message.is_text() {
                    ws.send(message).await.unwrap();
                } else if message.is_close() {
                    break;
                }
            }
        }
    });
    for n in 0..2 {
        let mut c = config.connect(profile.path()).await.unwrap();
        let value = json!({"id":n,"method":"thread/read","params":{"threadId":"isolated"}});
        c.send(&value).await.unwrap();
        let Some(Event::Message(actual)) =
            tokio::time::timeout(Duration::from_secs(2), c.events.recv())
                .await
                .unwrap()
        else {
            panic!("Missing Unix reply")
        };
        assert_eq!(actual, value);
        c.close().await;
    }
    server.await.unwrap();
    std::fs::set_permissions(&socket, std::fs::Permissions::from_mode(0o666)).unwrap();
    assert!(config.connect(profile.path()).await.is_err());
    assert!(serde_json::from_value::<SharedServer>(
        json!({"socket_path":socket,"address":"127.0.0.1:1234","token_file":"/tmp/token"})
    )
    .is_err());
}

#[cfg(unix)]
#[tokio::test]
async fn codex_short_socket_alias_is_bound_to_the_requested_isolated_path() {
    use remodex_connect_native::agent::SharedServer;
    use sha2::{Digest, Sha256};
    use std::os::unix::fs::{symlink, DirBuilderExt, PermissionsExt};
    let profile = tempfile::tempdir().unwrap();
    let path = profile.path().join("rpc.sock");
    // Match Codex's actual fixed rendezvous root, not macOS's long TMPDIR.
    let directory = std::path::Path::new("/tmp")
        .canonicalize()
        .unwrap()
        .join(format!("codex-daemon-{}", unsafe { libc::geteuid() }));
    let mut builder = std::fs::DirBuilder::new();
    builder.mode(0o700);
    if let Err(e) = builder.create(&directory) {
        assert_eq!(e.kind(), std::io::ErrorKind::AlreadyExists);
    }
    let target = directory.join(format!(
        "{:x}",
        Sha256::digest(path.as_os_str().as_encoded_bytes())
    ));
    let listener = tokio::net::UnixListener::bind(&target).unwrap();
    std::fs::set_permissions(&target, std::fs::Permissions::from_mode(0o600)).unwrap();
    symlink(&target, &path).unwrap();
    let config = SharedServer::Unix {
        socket_path: path.clone(),
    };
    config.validate(profile.path()).unwrap();
    let wrong = profile.path().join("wrong.sock");
    symlink(&target, &wrong).unwrap();
    assert!(SharedServer::Unix { socket_path: wrong }
        .validate(profile.path())
        .is_err());
    let server = tokio::spawn(async move {
        let (stream, _) = listener.accept().await.unwrap();
        let mut ws = tokio_tungstenite::accept_async(stream).await.unwrap();
        while let Some(Ok(m)) = ws.next().await {
            if m.is_text() {
                ws.send(m).await.unwrap();
            } else if m.is_close() {
                break;
            }
        }
    });
    let mut c = config.connect(profile.path()).await.unwrap();
    c.send(&json!({"id":1})).await.unwrap();
    assert!(matches!(c.events.recv().await, Some(Event::Message(_))));
    c.close().await;
    server.await.unwrap();
    std::fs::remove_file(target).unwrap();
}
