// Copyright 2020 Google LLC
//
// Use of this source code is governed by an MIT-style license that can be found
// in the LICENSE file or at https://opensource.org/licenses/MIT.

mod conn;

use super::*;

/// Collects information about available network interfaces.
///
/// A system agnostic [`interfaces`] function is available in the parent module
/// and should be the preferred choice in general.
///
/// This function is a wrapper around [`getifaddrs`][1] macOS call.
///
/// [1]: https://developer.apple.com/library/archive/documentation/System/Conceptual/ManPages_iPhoneOS/man3/getifaddrs.3.html
///
/// [`interfaces`]: super::interfaces
pub fn interfaces() -> std::io::Result<impl Iterator<Item = Interface>> {
    // Note that the this function is implemented nearly identically to the
    // Linux one. However, despite identical structure names (except for the
    // MAC address structure), their memory layout is completely different and
    // the code cannot (or rather: it should not) be shared.
    let mut addrs = std::mem::MaybeUninit::uninit();

    // SAFETY: `getifaddrs` [1] returns a pointer (through an output parameter)
    // so there is no potential of unsafety here and the function is marked as
    // such because it operates on raw pointers.
    //
    // [1]: https://developer.apple.com/library/archive/documentation/System/Conceptual/ManPages_iPhoneOS/man3/getifaddrs.3.html
    let code = unsafe {
        libc::getifaddrs(addrs.as_mut_ptr())
    };
    if code != 0 {
        return Err(std::io::Error::from_raw_os_error(code));
    }

    // SAFETY: We check return code above. If there was no error, `getifaddrs`
    // should initialized the `addrs` variable to a correct value.
    let addrs = unsafe {
        addrs.assume_init()
    };

    let mut ifaces = std::collections::HashMap::new();

    let mut addr_iter = addrs;
    // SAFETY: We iterate over the linked list of addresses until we hit the
    // last entry. The validity of the `addr_iter` pointer is ensured by:
    //
    //   * Starting at the value reported by the `getifaddrs` call.
    //   * Always moving to the entry pointed by the `ifa_next` field (at the
    //     end of the loop).
    while let Some(addr) = unsafe { addr_iter.as_ref() } {
        use std::os::unix::ffi::OsStrExt as _;

        // We advance the iterator immediately to avoid getting stuck because of
        // the `continue` statements in the code below.
        addr_iter = addr.ifa_next;

        // SAFETY: `ifa_ddr` is not guaranteed to be not null, so we have to
        // verify it. But if it is not null, it is guaranteed to point to valid
        // address instance.
        let family = match unsafe { addr.ifa_addr.as_ref() } {
            Some(addr) => addr.sa_family,
            None => continue,
        };

        // SAFETY: `ifa_name` is a string with interface name. While Apple docs
        // do not mention whether it is null-terminated, it is a safe bet to
        // assume so given the similarity to the Linux version of `getifaddrs`.
        let name = std::ffi::OsStr::from_bytes(unsafe {
            std::ffi::CStr::from_ptr(addr.ifa_name)
        }.to_bytes());

        let entry = ifaces.entry(name).or_insert(Interface {
            name: name.to_os_string(),
            ip_addrs: Vec::new(),
            mac_addr: None,
        });

        match i32::from(family) {
            libc::AF_INET => {
                // SAFETY: For `AF_INET` family the `ifa_addr` field is instance
                // of the IPv4 address [1, 2]. Again, the documentation on this
                // is quite bad.
                //
                // [1]: https://developer.apple.com/documentation/kernel/sockaddr_in
                // [2]: https://github.com/apple/darwin-xnu/blob/2ff845c2e033bd0ff64b5b6aa6063a1f8f65aa32/bsd/netinet/in.h#L394-L403
                let ipv4_addr_u32 = unsafe {
                    *(addr.ifa_addr as *const libc::sockaddr_in)
                }.sin_addr.s_addr;

                // Unlike on Linux, Apple documentation does not say anything
                // whatsoever about the endianness of the address value [1, 2].
                // We give them the benefit of a doubt and assume that they do
                // a sane thing and follow the Linux convention here.
                //
                // Hence, we have to convert from network endian (big endian)
                // order to what the Rust IPv4 type constructor expects (host
                // endian).
                //
                // [1]: https://developer.apple.com/documentation/kernel/in_addr_t
                // [2]: https://github.com/apple/darwin-xnu/blob/2ff845c2e033bd0ff64b5b6aa6063a1f8f65aa32/bsd/sys/_types/_in_addr_t.h#L31
                let ipv4_addr_u32 = u32::from_be(ipv4_addr_u32);

                let ipv4_addr = std::net::Ipv4Addr::from(ipv4_addr_u32);
                entry.ip_addrs.push(ipv4_addr.into());
            }
            libc::AF_INET6 => {
                // SAFETY: For `AF_INET6` family the `ifa_addr` field is an
                // instance of the IPv6 address [1, 2]. The comment on the
                // `sin6_family` field confirms it (unlike for `AF_INET`). Thus,
                // the case is safe.
                //
                // [1]: https://developer.apple.com/documentation/kernel/sockaddr_in6
                // [2]: https://github.com/apple/darwin-xnu/blob/2ff845c2e033bd0ff64b5b6aa6063a1f8f65aa32/bsd/netinet6/in6.h#L181-L188
                let ipv6_addr_octets = unsafe {
                    *(addr.ifa_addr as *const libc::sockaddr_in6)
                }.sin6_addr.s6_addr;

                let ipv6_addr = std::net::Ipv6Addr::from(ipv6_addr_octets);
                entry.ip_addrs.push(ipv6_addr.into());
            }
            libc::AF_LINK => {
                // SAFETY: For `AF_LINK` family the `ifa_addr` field is an
                // instance of a link-level address [1, 2] (whatever that means
                // exactly). Again, the comment on the `sdl_family` field seems
                // to confirm this and the cast is safe.
                //
                // [1]: https://developer.apple.com/documentation/kernel/sockaddr_dl
                // [2]: https://github.com/apple/darwin-xnu/blob/2ff845c2e033bd0ff64b5b6aa6063a1f8f65aa32/bsd/net/if_dl.h#L95-L110
                let sockaddr = unsafe {
                    *(addr.ifa_addr as *const libc::sockaddr_dl)
                };

                // Unfortunatelly, it is not uncommon to have some other non-MAC
                // addresses with the `AF_LINK` family. We simply ignore such.
                if sockaddr.sdl_alen != 6 {
                    continue;
                }

                // SAFETY: The original `sdl_data` is typed as `i8` (because it
                // contains the name) but the actual address bytes should be in-
                // terpreted as normal bytes (verified empirically). Validity of
                // indexing is ensured by the `sdl_alen` check above.
                let mac_addr = unsafe {
                    let data = sockaddr.sdl_data.as_ptr()
                        .offset(isize::from(sockaddr.sdl_nlen))
                        .cast::<u8>();

                    MacAddr::from([
                        *data.offset(0),
                        *data.offset(1),
                        *data.offset(2),
                        *data.offset(3),
                        *data.offset(4),
                        *data.offset(5),
                    ])
                };

                // TODO: There should only be one MAC address associated with
                // a given interface. Consider logging a warning in case this
                // assumption does not hold.
                entry.mac_addr.replace(mac_addr);
            }
            _ => continue,
        }
    }

    // We need to collect the interfaces to free the addresses below. Otherwise,
    // the keys of the hash map will point to dangling references (since the map
    // keys are owned by the address list).
    let ifaces = ifaces.into_values().collect::<Vec<_>>();

    // SAFETY: The `getifaddrs` call at the beginning of this function creates
    // a linked list that we are responsible for freeing using the `freeifaddrs`
    // function [1]. This is safe as we never release the allocated memory.
    //
    // [1]: https://developer.apple.com/library/archive/documentation/System/Conceptual/ManPages_iPhoneOS/man3/freeifaddrs.3.html
    unsafe {
        libc::freeifaddrs(addrs);
    }

    Ok(ifaces.into_iter())
}

/// Returns an iterator over IPv4 TCP connections for the specified process.
pub fn tcp_v4_connections(pid: u32) -> std::io::Result<impl Iterator<Item = std::io::Result<TcpConnectionV4>>> {
    conn::tcp_v4(pid)
}

/// Returns an iterator over IPv6 TCP connections for the specified process.
pub fn tcp_v6_connections(pid: u32) -> std::io::Result<impl Iterator<Item = std::io::Result<TcpConnectionV6>>> {
    conn::tcp_v6(pid)
}

/// Returns an iterator over IPv4 UDP connections for the specified process.
pub fn udp_v4_connections(pid: u32) -> std::io::Result<impl Iterator<Item = std::io::Result<UdpConnectionV4>>> {
    conn::udp_v4(pid)
}

/// Returns an iterator over IPv6 UDP connections for the specified process.
pub fn udp_v6_connections(pid: u32) -> std::io::Result<impl Iterator<Item = std::io::Result<UdpConnectionV6>>> {
    conn::udp_v6(pid)
}

#[cfg(test)]
mod tests {

    use super::*;

    #[test]
    fn interfaces_loopback_exists() {
        let mut ifaces = interfaces().unwrap();

        // On macOS the loopback interface seems to be always named `lo0` but it
        // does not appear to be documented anywhere, so to be on the safe side
        // we do not make such specific assertions.
        assert! {
            ifaces.any(|iface| {
                iface.ip_addrs().iter().any(|ip_addr| {
                    ip_addr.is_loopback()
                })
            })
        };
    }
}
