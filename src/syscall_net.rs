//! Network system calls (a child module of `syscall`, sharing its helpers).
//!
//!   32 socket(kind)                    kind 1 = UDP datagrams, 2 = TCP stream; returns a descriptor
//!   33 bind(fd, port)                  port 0 picks a free one (UDP); returns the port
//!   34 sendto(fd, buf, len, addr)      UDP: addr -> 8 bytes (ip[4], port u16 LE, 2 pad), or 0 to
//!                                      use the connected peer. TCP: addr is ignored and all
//!                                      `len` bytes are written (waiting for room). Returns bytes sent
//!   35 recvfrom(fd, buf, len, addr_out, timeout)
//!                                      UDP: one datagram (a longer one is cut). TCP: up to `len`
//!                                      bytes, 0 = the other side closed. addr_out (may be 0) gets
//!                                      the sender. timeout is in ticks: 0 only looks (EAGAIN),
//!                                      u64::MAX waits for ever
//!   36 connect(fd, addr, timeout)      UDP: set the default peer (other senders are ignored).
//!                                      TCP: open the connection and wait for it (timeout in ticks)
//!   37 listen(fd, backlog)             TCP: start accepting on the bound port
//!   38 accept(fd, addr_out, timeout)   TCP: next connection as a new descriptor
//!   39 netinfo(out)                    64 bytes, see `net::netinfo`
//!   40 resolve(name, out_ip, timeout, server)
//!                                      name -> 4 bytes at out_ip; server is 0 (use DHCP's
//!                                      resolver) or a pointer to an 8-byte address (ip, port)
//!   41 ping(ip, payload_len, timeout)  ip = the four bytes as one big-endian number;
//!                                      returns the round trip in microseconds
//!   42 clock_us()                      microseconds since boot
//!   43 shutdown(fd, how)               TCP: how = 1 stops sending (the other side sees end of file)
//!   44 getsockname(fd, out)            local address (8 bytes)
//!   45 getpeername(fd, out)            the peer's address (TCP)
//!
//! `read`/`write` on a socket descriptor act like recvfrom/sendto with no address.

use super::{copy_cstr_from_user, copy_from_user, copy_to_user, lookup_fd, walk_user, EBADF, EINVAL, EMFILE, MAX_IO};
use crate::ipc::fd::File;
use crate::net::{self, TcpSocket, UdpSocket, ENETDOWN};
use crate::{proc, timer};
use novanet::wire::Ip;

const ENOTSOCK: i64 = -88;
const SOCK_UDP: u64 = 1;
const SOCK_TCP: u64 = 2;
const DNS_NAME_MAX: usize = 253;

fn timeout_arg(ticks: u64) -> Option<u64> {
    if ticks == u64::MAX {
        None
    } else {
        Some(ticks)
    }
}

fn read_addr(ptr: u64) -> Result<(Ip, u16), i64> {
    let raw = copy_from_user(ptr, 8)?;
    Ok(([raw[0], raw[1], raw[2], raw[3]], u16::from_le_bytes([raw[4], raw[5]])))
}

fn write_addr(ptr: u64, ip: Ip, port: u16) -> Result<(), i64> {
    let mut raw = [0u8; 8];
    raw[0..4].copy_from_slice(&ip);
    raw[4..6].copy_from_slice(&port.to_le_bytes());
    copy_to_user(ptr, &raw)
}

fn socket_of(fd: u64) -> Result<alloc::sync::Arc<File>, i64> {
    let file = lookup_fd(fd).ok_or(EBADF)?;
    match &*file {
        File::Udp(_) | File::Tcp(_) => Ok(file),
        _ => Err(ENOTSOCK),
    }
}

fn tcp_of(fd: u64) -> Result<alloc::sync::Arc<File>, i64> {
    let file = socket_of(fd)?;
    match &*file {
        File::Tcp(_) => Ok(file),
        _ => Err(EINVAL),
    }
}

fn udp(file: &File) -> &UdpSocket {
    match file {
        File::Udp(s) => s,
        _ => unreachable!("checked by the caller"),
    }
}

fn tcp(file: &File) -> &TcpSocket {
    match file {
        File::Tcp(s) => s,
        _ => unreachable!("checked by the caller"),
    }
}

fn install(file: File) -> i64 {
    let Some(fds) = proc::current_fds() else { return EBADF };
    let installed = fds.lock().install(file);
    match installed {
        Some(fd) => fd as i64,
        None => EMFILE,
    }
}

pub fn sys_socket(kind: u64) -> i64 {
    let file = match kind {
        SOCK_UDP => File::Udp(UdpSocket::new()),
        SOCK_TCP => File::Tcp(TcpSocket::new()),
        _ => return EINVAL,
    };
    if !net::available() {
        return ENETDOWN;
    }
    install(file)
}

pub fn sys_bind(fd: u64, port: u64) -> i64 {
    // The descriptor is checked first: a bad descriptor is EBADF whatever else is wrong.
    let file = match socket_of(fd) {
        Ok(f) => f,
        Err(e) => return e,
    };
    let Ok(port) = u16::try_from(port) else { return EINVAL };
    match &*file {
        File::Udp(s) => match s.bind(port) {
            Ok(()) => s.local_port() as i64,
            Err(e) => e,
        },
        File::Tcp(s) => match s.bind(port) {
            Ok(()) => port as i64,
            Err(e) => e,
        },
        _ => ENOTSOCK,
    }
}

pub fn sys_sendto(fd: u64, ptr: u64, len: u64, addr_ptr: u64) -> i64 {
    if len > MAX_IO {
        return EINVAL;
    }
    let file = match socket_of(fd) {
        Ok(f) => f,
        Err(e) => return e,
    };
    let to = if addr_ptr == 0 {
        None
    } else {
        match read_addr(addr_ptr) {
            Ok(a) => Some(a),
            Err(e) => return e,
        }
    };
    let data = match copy_from_user(ptr, len) {
        Ok(d) => d,
        Err(e) => return e,
    };
    let sent = match &*file {
        File::Udp(s) => s.send_to(&data, to),
        File::Tcp(s) => s.send(&data),
        _ => return ENOTSOCK,
    };
    match sent {
        Ok(n) => n as i64,
        Err(e) => e,
    }
}

pub fn sys_recvfrom(fd: u64, ptr: u64, len: u64, addr_out: u64, timeout: u64) -> i64 {
    if len > MAX_IO {
        return EINVAL;
    }
    let file = match socket_of(fd) {
        Ok(f) => f,
        Err(e) => return e,
    };
    // Check both destinations first: data taken from the socket and then lost to a bad
    // pointer could never be recovered.
    if let Err(e) = walk_user(ptr, len, true, |_, _| {}) {
        return e;
    }
    if addr_out != 0 {
        if let Err(e) = walk_user(addr_out, 8, true, |_, _| {}) {
            return e;
        }
    }
    let (data, from) = match &*file {
        File::Udp(s) => match s.recv_from(len as usize, timeout_arg(timeout)) {
            Ok((data, ip, port)) => (data, Some((ip, port))),
            Err(e) => return e,
        },
        File::Tcp(s) => match s.recv(len as usize, timeout_arg(timeout)) {
            Ok(data) => (data, s.peer_addr().ok()),
            Err(e) => return e,
        },
        _ => return ENOTSOCK,
    };
    if let Err(e) = copy_to_user(ptr, &data) {
        return e;
    }
    if addr_out != 0 {
        let (ip, port) = from.unwrap_or(([0; 4], 0));
        if let Err(e) = write_addr(addr_out, ip, port) {
            return e;
        }
    }
    data.len() as i64
}

pub fn sys_connect(fd: u64, addr_ptr: u64, timeout: u64) -> i64 {
    let file = match socket_of(fd) {
        Ok(f) => f,
        Err(e) => return e,
    };
    let (ip, port) = match read_addr(addr_ptr) {
        Ok(a) => a,
        Err(e) => return e,
    };
    if port == 0 {
        return EINVAL;
    }
    let result = match &*file {
        File::Udp(s) => s.connect(ip, port),
        File::Tcp(s) => s.connect(ip, port, timeout_arg(timeout)),
        _ => return ENOTSOCK,
    };
    match result {
        Ok(()) => 0,
        Err(e) => e,
    }
}

pub fn sys_listen(fd: u64, backlog: u64) -> i64 {
    let file = match tcp_of(fd) {
        Ok(f) => f,
        Err(e) => return e,
    };
    match tcp(&file).listen(backlog.min(128) as usize) {
        Ok(_) => 0,
        Err(e) => e,
    }
}

pub fn sys_accept(fd: u64, addr_out: u64, timeout: u64) -> i64 {
    let file = match tcp_of(fd) {
        Ok(f) => f,
        Err(e) => return e,
    };
    if addr_out != 0 {
        if let Err(e) = walk_user(addr_out, 8, true, |_, _| {}) {
            return e;
        }
    }
    let (sock, ip, port) = match tcp(&file).accept(timeout_arg(timeout)) {
        Ok(a) => a,
        Err(e) => return e,
    };
    if addr_out != 0 {
        // The pointer was checked above; ignore a race that unmapped it since.
        let _ = write_addr(addr_out, ip, port);
    }
    install(File::Tcp(sock))
}

pub fn sys_shutdown(fd: u64, how: u64) -> i64 {
    let file = match tcp_of(fd) {
        Ok(f) => f,
        Err(e) => return e,
    };
    if how != 1 {
        return EINVAL;
    }
    match tcp(&file).shutdown_write() {
        Ok(()) => 0,
        Err(e) => e,
    }
}

pub fn sys_getsockname(fd: u64, out_ptr: u64) -> i64 {
    let file = match socket_of(fd) {
        Ok(f) => f,
        Err(e) => return e,
    };
    let addr = match &*file {
        File::Udp(s) => {
            let ip = net::config().map(|c| c.ip).unwrap_or([0; 4]);
            Ok((ip, s.local_port()))
        }
        File::Tcp(s) => s.local_addr(),
        _ => return ENOTSOCK,
    };
    match addr {
        Ok((ip, port)) => match write_addr(out_ptr, ip, port) {
            Ok(()) => 0,
            Err(e) => e,
        },
        Err(e) => e,
    }
}

pub fn sys_getpeername(fd: u64, out_ptr: u64) -> i64 {
    let file = match socket_of(fd) {
        Ok(f) => f,
        Err(e) => return e,
    };
    let addr = match &*file {
        File::Tcp(s) => s.peer_addr(),
        _ => Err(net::ENOTCONN),
    };
    match addr {
        Ok((ip, port)) => match write_addr(out_ptr, ip, port) {
            Ok(()) => 0,
            Err(e) => e,
        },
        Err(e) => e,
    }
}

pub fn sys_netinfo(out_ptr: u64) -> i64 {
    match net::netinfo() {
        Some(raw) => match copy_to_user(out_ptr, &raw) {
            Ok(()) => 0,
            Err(e) => e,
        },
        None => ENETDOWN,
    }
}

pub fn sys_resolve(name_ptr: u64, out_ptr: u64, timeout: u64, server_ptr: u64) -> i64 {
    let name = match copy_cstr_from_user(name_ptr, DNS_NAME_MAX) {
        Ok(n) => n,
        Err(e) => return e,
    };
    if let Err(e) = walk_user(out_ptr, 4, true, |_, _| {}) {
        return e;
    }
    let server = if server_ptr == 0 {
        None
    } else {
        match read_addr(server_ptr) {
            Ok((ip, port)) => Some((ip, if port == 0 { 53 } else { port })),
            Err(e) => return e,
        }
    };
    if !net::available() && net::parse_ip(&name).is_none() {
        return ENETDOWN;
    }
    match net::resolve(&name, server, timeout_arg(timeout).unwrap_or(30_000)) {
        Ok(ip) => match copy_to_user(out_ptr, &ip) {
            Ok(()) => 0,
            Err(e) => e,
        },
        Err(e) => e,
    }
}

pub fn sys_ping(ip: u64, payload_len: u64, timeout: u64) -> i64 {
    let Ok(ip) = u32::try_from(ip) else { return EINVAL };
    if payload_len > 1400 {
        return EINVAL;
    }
    match net::ping(ip.to_be_bytes(), payload_len as usize, timeout_arg(timeout).unwrap_or(30_000)) {
        Ok(us) => us as i64,
        Err(e) => e,
    }
}

pub fn sys_clock_us() -> i64 {
    timer::now_us() as i64
}
