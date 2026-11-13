//! Discover current LAN addresses independently of the startup address. A PC
//! network change must not require restarting workers or pairing phones again.
use std::{
    io,
    net::{Ipv4Addr, SocketAddr},
    time::Duration,
};
use tokio::sync::watch;

#[derive(Clone)]
pub(crate) struct Routes(watch::Receiver<Vec<String>>);

#[derive(Clone, Debug, Eq, PartialEq, Ord, PartialOrd)]
struct Address {
    index: u32,
    ip: Ipv4Addr,
    gateway: Option<Ipv4Addr>,
}

fn usable(ip: Ipv4Addr) -> bool {
    !ip.is_unspecified()
        && !ip.is_loopback()
        && !ip.is_link_local()
        && !ip.is_multicast()
        && !ip.is_broadcast()
        && ip.octets()[0] < 240
}

fn urls(addresses: &[Address], port: u16) -> Vec<String> {
    let mut ips: Vec<_> = addresses
        .iter()
        .map(|a| a.ip)
        .filter(|ip| usable(*ip))
        .collect();
    ips.sort_unstable();
    ips.dedup();
    ips.into_iter()
        .take(16)
        .map(|ip| format!("http://{ip}:{port}"))
        .collect()
}

impl Routes {
    pub(crate) fn current(&self) -> Vec<String> {
        self.0.borrow().clone()
    }

    pub(crate) async fn start(
        bound: SocketAddr,
        mut stop: watch::Receiver<bool>,
        tunnel: Option<crate::tunnel::Manager>,
    ) -> Self {
        // An explicit bind must never advertise addresses it does not serve.
        if !bound.ip().is_unspecified() || !bound.is_ipv4() {
            return Self(watch::channel(vec![format!("http://{bound}")]).1);
        }
        let initial = tokio::task::spawn_blocking(discover)
            .await
            .ok()
            .and_then(Result::ok);
        let (send, receive) = watch::channel(
            initial
                .as_ref()
                .map_or_else(Vec::new, |a| urls(a, bound.port())),
        );
        tokio::spawn(async move {
            let mut previous = initial;
            loop {
                tokio::select! {
                    _ = stop.changed() => break,
                    _ = tokio::time::sleep(Duration::from_secs(2)) => {},
                }
                if *stop.borrow() {
                    break;
                }
                let next = tokio::select! {
                    _ = stop.changed() => break,
                    next = tokio::task::spawn_blocking(discover) => next,
                };
                // A failed OS query is not proof that every adapter vanished.
                let Ok(Ok(next)) = next else { continue };
                if previous.as_ref() == Some(&next) {
                    continue;
                }
                send.send_replace(urls(&next, bound.port()));
                previous = Some(next);
                if let Some(tunnel) = &tunnel {
                    tunnel.network_changed();
                }
            }
        });
        Self(receive)
    }
}

#[cfg(windows)]
fn discover() -> io::Result<Vec<Address>> {
    use windows_sys::Win32::{
        Foundation::{ERROR_BUFFER_OVERFLOW, ERROR_NO_DATA, NO_ERROR},
        NetworkManagement::{IpHelper::*, Ndis::IfOperStatusUp},
        Networking::WinSock::{IpDadStatePreferred, AF_INET, SOCKADDR_IN, SOCKET_ADDRESS},
    };
    unsafe fn ipv4(address: SOCKET_ADDRESS) -> Option<Ipv4Addr> {
        if address.lpSockaddr.is_null()
            || address.iSockaddrLength < std::mem::size_of::<SOCKADDR_IN>() as i32
        {
            return None;
        }
        let address = &*address.lpSockaddr.cast::<SOCKADDR_IN>();
        (address.sin_family == AF_INET)
            .then(|| Ipv4Addr::from(address.sin_addr.S_un.S_addr.to_ne_bytes()))
    }
    // u64 storage supplies the alignment required by IP_ADAPTER_ADDRESSES.
    // Windows can grow the buffer while adapters change; bound both retries
    // and allocation. Pointers are only traversed after a successful OS call.
    let mut size = 15_000u32;
    for _ in 0..3 {
        if size > 1024 * 1024 {
            return Err(io::ErrorKind::OutOfMemory.into());
        }
        let mut buffer = vec![0u64; (size as usize).div_ceil(8)];
        let head = buffer.as_mut_ptr().cast::<IP_ADAPTER_ADDRESSES_LH>();
        let result = unsafe {
            GetAdaptersAddresses(
                AF_INET as u32,
                GAA_FLAG_INCLUDE_GATEWAYS
                    | GAA_FLAG_SKIP_ANYCAST
                    | GAA_FLAG_SKIP_MULTICAST
                    | GAA_FLAG_SKIP_DNS_SERVER,
                std::ptr::null(),
                head,
                &mut size,
            )
        };
        if result == ERROR_BUFFER_OVERFLOW {
            continue;
        }
        if result == ERROR_NO_DATA {
            return Ok(vec![]);
        }
        if result != NO_ERROR {
            return Err(io::Error::from_raw_os_error(result as i32));
        }
        let mut found = Vec::new();
        let mut next = head;
        while !next.is_null() {
            let adapter = unsafe { &*next };
            next = adapter.Next;
            if adapter.OperStatus != IfOperStatusUp
                || ![IF_TYPE_ETHERNET_CSMACD, IF_TYPE_IEEE80211].contains(&adapter.IfType)
            {
                continue;
            }
            let mut gateway = adapter.FirstGatewayAddress;
            let mut route = None;
            while !gateway.is_null() {
                let item = unsafe { &*gateway };
                route = unsafe { ipv4(item.Address) }.filter(|ip| usable(*ip));
                if route.is_some() {
                    break;
                }
                gateway = item.Next;
            }
            // Exclude host-only VM/VPN adapters, which phones cannot reach.
            if route.is_none() {
                continue;
            }
            let mut unicast = adapter.FirstUnicastAddress;
            while !unicast.is_null() {
                let item = unsafe { &*unicast };
                unicast = item.Next;
                if item.DadState != IpDadStatePreferred {
                    continue;
                }
                if let Some(ip) = unsafe { ipv4(item.Address) }.filter(|ip| usable(*ip)) {
                    found.push(Address {
                        index: unsafe { adapter.Anonymous1.Anonymous.IfIndex },
                        ip,
                        gateway: route,
                    });
                }
            }
        }
        found.sort_unstable();
        found.dedup();
        return Ok(found);
    }
    Err(io::ErrorKind::Interrupted.into())
}

#[cfg(unix)]
fn discover() -> io::Result<Vec<Address>> {
    let mut head = std::ptr::null_mut();
    if unsafe { libc::getifaddrs(&mut head) } != 0 {
        return Err(io::Error::last_os_error());
    }
    struct Guard(*mut libc::ifaddrs);
    impl Drop for Guard {
        fn drop(&mut self) {
            unsafe { libc::freeifaddrs(self.0) };
        }
    }
    let _guard = Guard(head);
    let mut found = Vec::new();
    let mut next = head;
    while !next.is_null() {
        let item = unsafe { &*next };
        next = item.ifa_next;
        if item.ifa_addr.is_null()
            || item.ifa_flags & (libc::IFF_UP as u32) == 0
            || item.ifa_flags & (libc::IFF_LOOPBACK as u32) != 0
        {
            continue;
        }
        if unsafe { (*item.ifa_addr).sa_family as i32 } != libc::AF_INET {
            continue;
        }
        let ip = Ipv4Addr::from(
            unsafe { (*item.ifa_addr.cast::<libc::sockaddr_in>()).sin_addr.s_addr }.to_ne_bytes(),
        );
        if usable(ip) {
            found.push(Address {
                index: unsafe { libc::if_nametoindex(item.ifa_name) },
                ip,
                gateway: None,
            });
        }
    }
    found.sort_unstable();
    found.dedup();
    Ok(found)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn moving_networks_replaces_old_routes_and_offline_removes_them() {
        let old = vec![Address {
            index: 1,
            ip: "192.168.1.30".parse().unwrap(),
            gateway: Some("192.168.1.1".parse().unwrap()),
        }];
        let next = vec![Address {
            index: 7,
            ip: "10.1.2.3".parse().unwrap(),
            gateway: Some("10.1.2.1".parse().unwrap()),
        }];
        let mut state = crate::mobile_connection::State::default();
        let first = state.snapshot(urls(&old, 10105), None);
        let moved = state.snapshot(urls(&next, 10105), None);
        assert_eq!(
            moved["localUrls"],
            serde_json::json!(["http://10.1.2.3:10105"])
        );
        assert!(moved["revision"].as_u64() > first["revision"].as_u64());
        assert_eq!(moved["desktopInstanceId"], first["desktopInstanceId"]);
        assert_eq!(
            state.snapshot(urls(&[], 10105), None)["localUrls"],
            serde_json::json!([])
        );
    }
    #[test]
    fn routes_are_stable_and_exclude_unusable_addresses() {
        let entries: Vec<_> = [
            "192.168.1.3",
            "0.0.0.0",
            "127.0.0.1",
            "169.254.2.1",
            "224.0.0.1",
            "255.255.255.255",
            "192.168.1.3",
        ]
        .into_iter()
        .map(|s| Address {
            index: 1,
            ip: s.parse().unwrap(),
            gateway: None,
        })
        .collect();
        assert_eq!(urls(&entries, 1234), ["http://192.168.1.3:1234"]);
    }
    #[tokio::test]
    async fn explicit_listener_only_advertises_its_bound_address() {
        let (_stop, stopped) = watch::channel(false);
        let routes = Routes::start("127.0.0.1:1234".parse().unwrap(), stopped, None).await;
        assert_eq!(routes.current(), ["http://127.0.0.1:1234"]);
    }
    #[test]
    fn operating_system_discovery_returns_only_usable_sorted_addresses() {
        let addresses = discover().unwrap();
        assert!(addresses.iter().all(|a| usable(a.ip)));
        assert!(addresses.windows(2).all(|pair| pair[0] < pair[1]));
    }
}
