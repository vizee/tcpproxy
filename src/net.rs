use std::ffi;
use std::mem;
use std::net;
use std::ptr;

use libc;

use crate::sys::SysResult;

fn into_c_sin(sa: &net::SocketAddrV4) -> libc::sockaddr_in {
    libc::sockaddr_in {
        sin_family: libc::AF_INET as libc::sa_family_t,
        sin_port: sa.port().to_be(),
        sin_addr: libc::in_addr {
            s_addr: u32::from(*sa.ip()),
        },
        ..unsafe { mem::zeroed() }
    }
}

fn into_c_sin6(sa: &net::SocketAddrV6) -> libc::sockaddr_in6 {
    let mut inaddr: libc::in6_addr = unsafe { mem::zeroed() };
    inaddr.s6_addr = sa.ip().octets();
    libc::sockaddr_in6 {
        sin6_family: libc::AF_INET6 as libc::sa_family_t,
        sin6_port: sa.port().to_be(),
        sin6_flowinfo: sa.flowinfo(),
        sin6_addr: inaddr,
        sin6_scope_id: sa.scope_id(),
    }
}

fn from_c_sin(sin: &libc::sockaddr_in) -> net::SocketAddrV4 {
    net::SocketAddrV4::new(
        net::Ipv4Addr::from(sin.sin_addr.s_addr.to_be()),
        sin.sin_port.to_be(),
    )
}

fn from_c_sin6(sin6: &libc::sockaddr_in6) -> net::SocketAddrV6 {
    net::SocketAddrV6::new(
        net::Ipv6Addr::from(sin6.sin6_addr.s6_addr),
        sin6.sin6_port.to_be(),
        sin6.sin6_flowinfo,
        sin6.sin6_scope_id,
    )
}

pub fn resolve_address<F>(host: Option<&str>, port: &str, af: i32, socktype: i32, passive: bool, mut f: F) -> Result<(), i32>
    where F: FnMut(&libc::addrinfo) -> bool {
    let host = host.map(|s| ffi::CString::new(s).unwrap());
    let port = ffi::CString::new(port).unwrap();
    let mut hints: libc::addrinfo = unsafe { mem::zeroed() };
    hints.ai_family = af;
    hints.ai_socktype = socktype;
    hints.ai_flags = if passive { libc::AI_PASSIVE } else { 0 };
    let mut res: *mut libc::addrinfo = ptr::null_mut();
    let r = unsafe { libc::getaddrinfo(host.as_ref().map(|s| s.as_ptr()).unwrap_or(ptr::null()), port.as_ptr(), &hints as *const _, &mut res as *mut _) };
    if r != 0 {
        return Err(r);
    }
    let mut it = res;
    while !it.is_null() {
        let ai = unsafe { &*it };
        if f(ai) {
            break;
        }
        it = ai.ai_next;
    }
    unsafe { libc::freeaddrinfo(res) };
    Ok(())
}

pub fn resolve_first(addr: &str, af: i32, socktype: i32, passive: bool) -> Result<net::SocketAddr, String> {
    let mut part = addr.splitn(2, ':');
    let host = part
        .next()
        .and_then(|s| if s.is_empty() { None } else { Some(s) });
    let port = if let Some(s) = part.next() {
        s
    } else {
        return Err("missing service".to_string());
    };
    let mut sa = None;
    resolve_address(host, port, af, socktype, passive, |ai| {
        sa = match ai.ai_family {
            libc::AF_INET => Some(from_c_sin(unsafe { &*(ai.ai_addr as *const libc::sockaddr_in) }).into()),
            libc::AF_INET6 => Some(from_c_sin6(unsafe { &*(ai.ai_addr as *const libc::sockaddr_in6) }).into()),
            _ => None,
        };
        sa.is_some()
    }).map_err(|e| format!("EAI: {}", e))
        .and_then(|_| sa.ok_or("nothing resolved".to_string()))
}

pub fn connect_tcp(addr: &net::SocketAddr) -> SysResult<i32> {
    let fd = syscall!(libc::socket(
        match *addr {
            net::SocketAddr::V4(_) => libc::AF_INET,
            net::SocketAddr::V6(_) => libc::AF_INET6,
        },
        libc::SOCK_STREAM | libc::SOCK_NONBLOCK,
        0,
    ))?;
    let r = match addr {
        &net::SocketAddr::V4(sa) => {
            let sin = into_c_sin(&sa);
            syscall!(libc::connect(
                fd,
                &sin as *const _ as *const _,
                mem::size_of_val(&sin) as libc::socklen_t
            ))
        }
        &net::SocketAddr::V6(sa) => {
            let sin = into_c_sin6(&sa);
            syscall!(libc::connect(
                fd,
                &sin as *const _ as *const _,
                mem::size_of_val(&sin) as libc::socklen_t
            ))
        }
    };
    if let Err(e) = r {
        if e != libc::EINPROGRESS {
            unsafe { libc::close(fd) };
            return Err(e);
        }
    }
    Ok(fd)
}

pub fn listen_tcp(addr: &net::SocketAddr) -> SysResult<i32> {
    let fd = syscall!(libc::socket(
        match *addr {
            net::SocketAddr::V4(_) => libc::AF_INET,
            net::SocketAddr::V6(_) => libc::AF_INET6,
        },
        libc::SOCK_STREAM | libc::SOCK_NONBLOCK | libc::SOCK_CLOEXEC,
        0,
    ))?;
    let r = match addr {
        &net::SocketAddr::V4(sa) => {
            let sin = into_c_sin(&sa);
            syscall!(libc::bind(
                fd,
                &sin as *const _ as *const _,
                mem::size_of_val(&sin) as libc::socklen_t
            ))
        }
        &net::SocketAddr::V6(sa) => {
            let sin = into_c_sin6(&sa);
            syscall!(libc::bind(
                fd,
                &sin as *const _ as *const _,
                mem::size_of_val(&sin) as libc::socklen_t
            ))
        }
    };
    r.and_then(|_| syscall!(libc::listen(fd, libc::SOMAXCONN)))
        .map(|_| fd)
        .or_else(|e| {
            unsafe { libc::close(fd) };
            Err(e)
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_resolve_address() {
        let r = resolve_address(Some("localhost"), "http", libc::AF_UNSPEC, 0, false, |ai| {
            println!("ai_flags {} ai_family {} ai_socktype {} ai_protocol {} ai_addrlen {} ai_addr {:p} ai_canonname {:p} ai_next {:p}",
                ai.ai_flags,
                ai.ai_family,
                ai.ai_socktype,
                ai.ai_protocol,
                ai.ai_addrlen,
                ai.ai_addr,
                ai.ai_canonname,
                ai.ai_next,
            );
            false
        });
        assert_eq!(r, Ok(()));
    }

    #[test]
    fn test_resolve_first() {
        let r = resolve_first("www.bilibili.com:http", libc::AF_UNSPEC, 0, false);
        println!("{:?}", r);
        assert!(r.is_ok());
    }
}
