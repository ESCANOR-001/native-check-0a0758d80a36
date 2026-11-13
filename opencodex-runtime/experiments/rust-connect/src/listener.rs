//! Keep Windows wildcard and loopback listeners from sharing the same port.
//! Preflight checks and the actual server must use the same binding rules.
use std::{
    io,
    net::{TcpListener, ToSocketAddrs},
};

pub fn bind(address: impl ToSocketAddrs) -> io::Result<TcpListener> {
    #[cfg(not(windows))]
    {
        TcpListener::bind(address)
    }
    #[cfg(windows)]
    {
        let reserved = reserve(address)?;
        reserved.socket.listen(128)?;
        Ok(reserved.socket.into())
    }
}

/// Reserve a port for a setup check without advertising a network listener.
/// Calling listen during preflight makes Windows ask about the temporary CLI
/// executable as well as the real server, whose stable path it must remember.
pub struct Reservation {
    #[cfg(windows)]
    socket: socket2::Socket,
    #[cfg(not(windows))]
    _listener: TcpListener,
}
pub fn reserve(address: impl ToSocketAddrs) -> io::Result<Reservation> {
    #[cfg(not(windows))]
    {
        Ok(Reservation {
            _listener: TcpListener::bind(address)?,
        })
    }
    #[cfg(windows)]
    {
        use socket2::{Domain, Protocol, Socket, Type};
        use std::os::windows::io::AsRawSocket;
        use windows_sys::Win32::Networking::WinSock::{
            setsockopt, WSAGetLastError, SOL_SOCKET, SO_EXCLUSIVEADDRUSE,
        };
        let mut last = io::Error::new(io::ErrorKind::InvalidInput, "No listener address");
        for address in address.to_socket_addrs()? {
            let attempt = || {
                let socket = Socket::new(
                    Domain::for_address(address),
                    Type::STREAM,
                    Some(Protocol::TCP),
                )?;
                let enabled = 1i32;
                if unsafe {
                    setsockopt(
                        socket.as_raw_socket() as _,
                        SOL_SOCKET,
                        SO_EXCLUSIVEADDRUSE,
                        (&enabled as *const i32).cast(),
                        std::mem::size_of_val(&enabled) as i32,
                    )
                } != 0
                {
                    return Err(io::Error::from_raw_os_error(unsafe { WSAGetLastError() }));
                }
                socket.bind(&address.into())?;
                Ok(Reservation { socket })
            };
            match attempt() {
                Ok(listener) => return Ok(listener),
                Err(error) => last = error,
            }
        }
        Err(last)
    }
}

pub async fn bind_async(
    address: impl tokio::net::ToSocketAddrs,
) -> io::Result<tokio::net::TcpListener> {
    #[cfg(not(windows))]
    {
        tokio::net::TcpListener::bind(address).await
    }
    #[cfg(windows)]
    {
        let mut last = io::Error::new(io::ErrorKind::InvalidInput, "No listener address");
        for address in tokio::net::lookup_host(address).await? {
            match bind(address) {
                Ok(listener) => {
                    listener.set_nonblocking(true)?;
                    return tokio::net::TcpListener::from_std(listener);
                }
                Err(error) => last = error,
            }
        }
        Err(last)
    }
}

#[cfg(all(test, windows))]
mod tests {
    use super::*;
    #[test]
    fn wildcard_cannot_hide_an_existing_loopback_listener_and_retry_succeeds() {
        let old = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = old.local_addr().unwrap().port();
        assert!(bind(("0.0.0.0", port)).is_err());
        drop(old);
        let owned = bind(("0.0.0.0", port)).unwrap();
        assert!(TcpListener::bind(("127.0.0.1", port)).is_err());
        drop(owned);
        assert!(bind(("0.0.0.0", port)).is_ok());
    }
    #[test]
    fn setup_reserves_port_without_accepting_connections() {
        let reserved = reserve("127.0.0.1:0").unwrap();
        let address = reserved.socket.local_addr().unwrap().as_socket().unwrap();
        assert!(bind(address).is_err());
        assert!(std::net::TcpStream::connect_timeout(
            &address,
            std::time::Duration::from_millis(150)
        )
        .is_err());
        drop(reserved);
        assert!(bind(address).is_ok());
    }
}
