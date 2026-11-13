//! Live LAN access control without restarting agent workers or invalidating phones.
//! Local administration stays reachable on the bound port. Disabled remote peers
//! are closed before HTTP parsing; existing remote streams are also canceled.
use std::{
    future::Future,
    io,
    net::SocketAddr,
    pin::Pin,
    task::{Context, Poll},
    time::Duration,
};
use tokio::{
    io::{AsyncRead, AsyncWrite, ReadBuf},
    net::{TcpListener, TcpStream},
    sync::watch,
};
#[derive(Clone)]
pub(crate) struct Access(watch::Sender<bool>);
impl Access {
    pub(crate) fn new(enabled: bool) -> Self {
        Self(watch::channel(enabled).0)
    }
    pub(crate) fn enabled(&self) -> bool {
        *self.0.borrow()
    }
    pub(crate) fn set(&self, enabled: bool) {
        self.0.send_if_modified(|old| {
            if *old == enabled {
                false
            } else {
                *old = enabled;
                true
            }
        });
    }
    pub(crate) fn permits(&self, peer: Option<SocketAddr>) -> bool {
        self.enabled() || peer.is_some_and(loopback)
    }
    pub(crate) fn changes(&self) -> watch::Receiver<bool> {
        self.0.subscribe()
    }
    fn stream(&self, stream: TcpStream, peer: SocketAddr) -> Option<Stream> {
        // Subscribe before reading the preference so a disable cannot race an
        // accept into an unobserved open connection.
        let mut changes = self.changes();
        if !self.permits(Some(peer)) {
            return None;
        }
        let cancelled = (!loopback(peer)).then(|| {
            Box::pin(async move {
                let _ = changes.changed().await;
            }) as Pin<Box<dyn Future<Output = ()> + Send>>
        });
        Some(Stream {
            stream,
            cancelled,
            closed: false,
        })
    }
}
fn loopback(peer: SocketAddr) -> bool {
    peer.ip().is_loopback()
        || match peer.ip() {
            std::net::IpAddr::V6(ip) => ip.to_ipv4_mapped().is_some_and(|ip| ip.is_loopback()),
            _ => false,
        }
}
pub struct Listener {
    listener: TcpListener,
    access: Access,
}
impl Listener {
    pub(crate) fn new(listener: TcpListener, access: Access) -> Self {
        Self { listener, access }
    }
}
impl axum::serve::Listener for Listener {
    type Io = Stream;
    type Addr = SocketAddr;
    async fn accept(&mut self) -> (Stream, SocketAddr) {
        loop {
            match self.listener.accept().await {
                Ok((stream, peer)) => {
                    if let Some(stream) = self.access.stream(stream, peer) {
                        return (stream, peer);
                    }
                }
                Err(_) => tokio::time::sleep(Duration::from_millis(100)).await,
            }
        }
    }
    fn local_addr(&self) -> io::Result<SocketAddr> {
        self.listener.local_addr()
    }
}
pub struct Stream {
    stream: TcpStream,
    cancelled: Option<Pin<Box<dyn Future<Output = ()> + Send>>>,
    closed: bool,
}
impl Stream {
    fn allowed(&mut self, cx: &mut Context<'_>) -> io::Result<()> {
        if !self.closed
            && self
                .cancelled
                .as_mut()
                .is_some_and(|f| f.as_mut().poll(cx).is_ready())
        {
            self.closed = true;
            self.cancelled = None;
        }
        if self.closed {
            Err(io::ErrorKind::ConnectionAborted.into())
        } else {
            Ok(())
        }
    }
}
impl AsyncRead for Stream {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        if let Err(e) = self.allowed(cx) {
            return Poll::Ready(Err(e));
        }
        Pin::new(&mut self.stream).poll_read(cx, buf)
    }
}
impl AsyncWrite for Stream {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        if let Err(e) = self.allowed(cx) {
            return Poll::Ready(Err(e));
        }
        Pin::new(&mut self.stream).poll_write(cx, buf)
    }
    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        if let Err(e) = self.allowed(cx) {
            return Poll::Ready(Err(e));
        }
        Pin::new(&mut self.stream).poll_flush(cx)
    }
    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.stream).poll_shutdown(cx)
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    async fn pair() -> (TcpStream, TcpStream) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let client = TcpStream::connect(listener.local_addr().unwrap());
        let (client, server) = tokio::join!(client, listener.accept());
        (client.unwrap(), server.unwrap().0)
    }
    #[tokio::test]
    async fn disabling_wakes_blocked_remote_read_and_reenable_requires_a_new_connection() {
        let access = Access::new(true);
        let (_client, server) = pair().await;
        let mut remote = access
            .stream(server, "203.0.113.8:3000".parse().unwrap())
            .unwrap();
        let reader = tokio::spawn(async move {
            let mut b = [0; 1];
            let first = remote.read(&mut b).await.unwrap_err().kind();
            assert_eq!(remote.read(&mut b).await.unwrap_err().kind(), first);
            assert_eq!(remote.write(b"X").await.unwrap_err().kind(), first);
            assert_eq!(remote.flush().await.unwrap_err().kind(), first);
            first
        });
        tokio::task::yield_now().await;
        access.set(false);
        access.set(true);
        assert_eq!(
            tokio::time::timeout(Duration::from_secs(1), reader)
                .await
                .unwrap()
                .unwrap(),
            io::ErrorKind::ConnectionAborted
        );
        let (mut client, server) = pair().await;
        let mut remote = access
            .stream(server, "203.0.113.8:3000".parse().unwrap())
            .unwrap();
        client.write_all(b"A").await.unwrap();
        let mut b = [0; 1];
        remote.read_exact(&mut b).await.unwrap();
        assert_eq!(&b, b"A");
    }
    #[tokio::test]
    async fn disabled_remote_access_preserves_loopback_and_rejects_new_remote_streams() {
        let access = Access::new(false);
        for peer in ["127.0.0.1:1", "[::1]:1", "[::ffff:127.0.0.1]:1"] {
            assert!(access.permits(Some(peer.parse().unwrap())));
        }
        assert!(!access.permits(None));
        assert!(!access.permits(Some("203.0.113.8:1".parse().unwrap())));
        let (_, server) = pair().await;
        assert!(access
            .stream(server, "203.0.113.8:1".parse().unwrap())
            .is_none());
        let (mut client, server) = pair().await;
        let mut local = access
            .stream(server, "127.0.0.1:1".parse().unwrap())
            .unwrap();
        access.set(true);
        access.set(false);
        client.write_all(b"L").await.unwrap();
        let mut b = [0; 1];
        local.read_exact(&mut b).await.unwrap();
        assert_eq!(&b, b"L");
    }
}
