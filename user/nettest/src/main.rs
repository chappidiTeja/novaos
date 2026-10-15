//! nettest: tests of the network system calls from user space.
//!
//!   nettest local                       needs only the OS's own network (QEMU user network)
//!   nettest ext HOST ECHO_PORT DNS_IP DNS_PORT
//!                                       also talks to servers the test script runs on the
//!                                       host: a UDP echo server on HOST:ECHO_PORT and a
//!                                       DNS server at DNS_IP:DNS_PORT
//!
//! Prints "FAIL: ..." for every wrong result and ends with NETTEST PASSED or FAILED.

#![no_std]
#![no_main]

mod tcp;

use core::sync::atomic::{AtomicU32, Ordering};
use nova::errno::*;
use nova::*;

static FAILS: AtomicU32 = AtomicU32::new(0);
static CHECKS: AtomicU32 = AtomicU32::new(0);

fn check(ok: bool, what: &str) {
    CHECKS.fetch_add(1, Ordering::Relaxed);
    if !ok {
        FAILS.fetch_add(1, Ordering::Relaxed);
        println!("FAIL: {}", what);
    }
}

fn expect(got: i64, want: i64, what: &str) {
    CHECKS.fetch_add(1, Ordering::Relaxed);
    if got != want {
        FAILS.fetch_add(1, Ordering::Relaxed);
        println!("FAIL: {} (got {}, wanted {})", what, got, want);
    }
}

fn sock() -> u64 {
    let fd = socket(SOCK_UDP);
    check(fd >= 0, "socket() works");
    fd as u64
}

fn bound(port: u16) -> u64 {
    let fd = sock();
    expect(bind(fd, port), port as i64, "bind to a chosen port");
    fd
}

/// Wait (up to 8 s) for DHCP to give us an address.
fn wait_address() -> NetInfo {
    let mut info = NetInfo::default();
    for _ in 0..160 {
        if netinfo(&mut info) == 0 && info.flags & NET_HAS_ADDRESS != 0 {
            return info;
        }
        sleep(5);
    }
    println!("FAIL: no network address after 8 seconds");
    FAILS.fetch_add(1, Ordering::Relaxed);
    exit(1);
}

/// A syscall with a deliberately bad pointer argument.
fn raw(nr: u64, a: u64, b: u64, c: u64, d: u64, e: u64) -> i64 {
    unsafe { syscall(nr, a, b, c, d, e) }
}

const NOWHERE: u64 = 0x10; // not mapped in any program
const KERNEL: u64 = 0xFFFF_8000_0000_0000;

fn local() {
    let info = wait_address();
    let me = info.ip;
    println!("local address {}", Ip4(me));

    // ---- descriptors and kinds
    expect(socket(0), EINVAL, "socket kind 0 is refused");
    expect(socket(99), EINVAL, "an unknown socket kind is refused");
    expect(bind(99, 1), EBADF, "bind on a closed descriptor");
    expect(bind(0, 1), ENOTSOCK, "bind on the console");
    expect(sendto(1, b"x", None), ENOTSOCK, "sendto on the console");
    expect(raw(SYS_BIND, 3, 70000, 0, 0, 0), EBADF, "bind with a closed descriptor and a huge port");
    let a = sock();
    expect(raw(SYS_BIND, a, 70000, 0, 0, 0), EINVAL, "port above 65535");
    expect(bind(a, 7001), 7001, "bind");
    expect(bind(a, 7002), EINVAL, "binding twice");
    let b = sock();
    expect(bind(b, 7001), EADDRINUSE, "a port can be bound only once");
    let eph = bind(b, 0);
    check(eph >= 49152 && eph <= 65535, "port 0 picks an ephemeral port");

    // ---- sending
    let c = bound(7003);
    let to = SockAddr::new(me, 7001);
    let unbound = sock();
    expect(sendto(unbound, b"x", None), EDESTADDRREQ, "sending without a destination");
    expect(sendto(c, b"x", Some(&SockAddr::new(me, 0))), EINVAL, "destination port 0");
    let big = [7u8; 1473];
    expect(sendto(c, &big, Some(&to)), EMSGSIZE, "1473-byte datagram does not fit one packet");
    expect(sendto(c, &big[..1472], Some(&to)), 1472, "1472-byte datagram does");
    expect(raw(SYS_SENDTO, c, NOWHERE, 10, &to as *const SockAddr as u64, 0), EFAULT, "unmapped buffer");
    expect(raw(SYS_SENDTO, c, KERNEL, 10, &to as *const SockAddr as u64, 0), EFAULT, "kernel buffer");
    expect(raw(SYS_SENDTO, c, big.as_ptr() as u64, 10, NOWHERE, 0), EFAULT, "unmapped address");
    expect(raw(SYS_SENDTO, c, big.as_ptr() as u64, 1 << 30, 0, 0), EINVAL, "absurd length");

    // ---- receiving, in order and exactly
    let mut buf = [0u8; 2000];
    let mut from = SockAddr::default();
    expect(recvfrom(a, &mut buf, Some(&mut from), 100), 1472, "the big datagram arrives whole");
    check(buf[..1472].iter().all(|&x| x == 7), "and unchanged");
    check(from.ip == me && from.port == 7003, "with the right sender");
    expect(sendto(c, b"hello", Some(&to)), 5, "send hello");
    expect(sendto(c, b"second", Some(&to)), 6, "send second");
    expect(recvfrom(a, &mut buf, Some(&mut from), 100), 5, "first datagram first");
    check(&buf[..5] == b"hello", "contents");
    let mut small = [0u8; 3];
    expect(recvfrom(a, &mut small, None, 100), 3, "a short buffer cuts the datagram");
    check(&small == b"sec", "the part that fits");
    expect(recvfrom(a, &mut buf, None, 0), EAGAIN, "the rest of the cut datagram is gone");

    // A bad destination pointer must not eat the datagram.
    sendto(c, b"keep", Some(&to));
    expect(raw(SYS_RECVFROM, a, NOWHERE, 16, 0, 100), EFAULT, "unmapped receive buffer");
    expect(raw(SYS_RECVFROM, a, buf.as_mut_ptr() as u64, 16, NOWHERE, 100), EFAULT, "unmapped sender address");
    expect(recvfrom(a, &mut buf, None, 100), 4, "the datagram survived both failures");
    check(&buf[..4] == b"keep", "intact");

    // ---- timeouts
    expect(recvfrom(a, &mut buf, None, 0), EAGAIN, "nothing queued: would block");
    let t0 = clock_us();
    expect(recvfrom(a, &mut buf, None, 5), ETIMEDOUT, "waiting 5 ticks for nothing");
    let waited = clock_us() - t0;
    check(waited >= 35_000 && waited < 150_000, "and it waited about 50 ms (a tick is 10 ms)");

    // ---- connected sockets
    let x = bound(7010);
    let y = bound(7011);
    let z = bound(7012);
    expect(connect(x, &SockAddr::new(me, 7011)), 0, "connect");
    sendto(z, b"stranger", Some(&SockAddr::new(me, 7010)));
    expect(recvfrom(x, &mut buf, None, 20), ETIMEDOUT, "a connected socket ignores other senders");
    sendto(y, b"friend", Some(&SockAddr::new(me, 7010)));
    expect(recvfrom(x, &mut buf, None, 100), 6, "but hears its peer");
    expect(write(x, b"via write"), 9, "write sends to the peer");
    expect(read(y, &mut buf), 9, "read receives");
    check(&buf[..9] == b"via write", "same bytes");
    expect(fd_ready(y), 0, "nothing waiting");
    write(x, b"!");
    expect(fd_ready(y), 1, "a datagram is waiting");
    expect(read(y, &mut buf), 1, "read it");
    expect(fd_ready(y), 0, "and now nothing again");
    expect(connect(x, &SockAddr::new(me, 0)), EINVAL, "connect to port 0");
    expect(raw(SYS_CONNECT, x, NOWHERE, 0, 0, 0), EFAULT, "connect with a bad address pointer");

    // ---- datagrams to a port nobody listens on vanish quietly
    expect(sendto(c, b"void", Some(&SockAddr::new(me, 9))), 4, "send to a closed port");
    expect(recvfrom(c, &mut buf, None, 10), ETIMEDOUT, "nothing comes back");

    // ---- closing frees the port
    expect(close(a), 0, "close");
    let again = bound(7001);
    expect(close(again), 0, "close again");
    for fd in [b, c, unbound, x, y, z] {
        close(fd);
    }
    let mut held = [0u64; 80];
    let mut n = 0;
    loop {
        let fd = socket(SOCK_UDP);
        if fd < 0 {
            expect(fd, EMFILE, "running out of descriptors says so");
            break;
        }
        held[n] = fd as u64;
        n += 1;
    }
    check(n > 4, "a program can hold several sockets");
    for &fd in &held[..n] {
        close(fd);
    }
    let last = bound(7001);
    close(last);

    // ---- ping
    let r = ping(me, 16, 100);
    check(r >= 0, "ping ourselves");
    let gw = info.gateway;
    let r = ping(gw, 32, 300);
    check(r > 0 && r < 1_000_000, "ping the gateway");
    expect(ping(gw, 1401, 100), EINVAL, "ping payload over 1400");
    expect(ping([10, 0, 2, 77], 8, 30), ETIMEDOUT, "ping a silent address");

    // ---- name lookup
    let mut out = [0u8; 4];
    expect(resolve(b"1.2.3.4\0", &mut out, 10, None), 0, "an address resolves to itself");
    check(out == [1, 2, 3, 4], "unchanged");
    expect(resolve(b"1.2.3\0", &mut out, 10, None), EINVAL, "a malformed address is not a name");
    expect(resolve(b"bad name\0", &mut out, 10, None), EINVAL, "a space is not allowed in a name");
    expect(resolve(b"a..b\0", &mut out, 10, None), EINVAL, "empty label");
    expect(resolve(b"\0", &mut out, 10, None), EINVAL, "empty name");
    expect(raw(SYS_RESOLVE, b"1.2.3.4\0".as_ptr() as u64, NOWHERE, 10, 0, 0), EFAULT, "bad output pointer");
    expect(raw(SYS_RESOLVE, NOWHERE, out.as_mut_ptr() as u64, 10, 0, 0), EFAULT, "bad name pointer");

    // ---- info and clock
    expect(raw(SYS_NETINFO, NOWHERE, 0, 0, 0, 0), EFAULT, "netinfo with a bad pointer");
    let t0 = clock_us();
    sleep(10);
    let dt = clock_us() - t0;
    check(dt >= 85_000 && dt < 140_000, "sleeping 10 ticks takes about 100 ms on the microsecond clock");
    check(clock_us() >= t0, "the clock never goes back");
}

fn usage() -> i32 {
    eprintln!("usage: nettest local | ext HOST ECHO_PORT DNS_IP DNS_PORT | tcp-local | tcp-ext HOST ECHO_PORT SINK_PORT");
    2
}

fn parse_port(s: &str) -> Option<u16> {
    let v = s.bytes().try_fold(0u32, |v, b| if b.is_ascii_digit() { Some(v * 10 + (b - b'0') as u32) } else { None })?;
    u16::try_from(v).ok().filter(|&p| p != 0)
}

fn parse_args(args: &Args) -> Option<([u8; 4], u16, SockAddr)> {
    let host = parse_ip(args.get(2))?;
    let port = parse_port(args.get(3))?;
    let dns = SockAddr::new(parse_ip(args.get(4))?, parse_port(args.get(5))?);
    Some((host, port, dns))
}

fn external(host: [u8; 4], port: u16, dns: SockAddr) {
    let _ = wait_address();
    let echo = SockAddr::new(host, port);
    let mut buf = [0u8; 2000];
    let mut from = SockAddr::default();

    // One datagram of each size, echoed back unchanged by a program on another machine.
    let s = sock();
    for &len in &[1usize, 7, 100, 512, 1000, 1472] {
        let mut data = [0u8; 1472];
        for (i, b) in data[..len].iter_mut().enumerate() {
            *b = (i * 31 + len) as u8;
        }
        expect(sendto(s, &data[..len], Some(&echo)), len as i64, "send to the host");
        let n = recvfrom(s, &mut buf, Some(&mut from), 300);
        expect(n, len as i64, "echo has the same length");
        check(buf[..len] == data[..len], "and the same bytes");
        check(from == echo, "and comes from the server we asked");
    }

    // A burst: nothing lost, order kept.
    for i in 0..20u8 {
        sendto(s, &[i, i, i, i], Some(&echo));
    }
    let mut all_in_order = true;
    for i in 0..20u8 {
        let n = recvfrom(s, &mut buf, None, 300);
        if n != 4 || buf[0] != i {
            all_in_order = false;
        }
    }
    check(all_in_order, "20 datagrams in a burst come back complete and in order");

    // Two sockets at once get their own answers.
    let t = sock();
    sendto(s, b"for-s", Some(&echo));
    sendto(t, b"for-t", Some(&echo));
    let n = recvfrom(t, &mut buf, None, 300);
    check(n == 5 && &buf[..5] == b"for-t", "socket t gets t's answer");
    let n = recvfrom(s, &mut buf, None, 300);
    check(n == 5 && &buf[..5] == b"for-s", "socket s gets s's answer");

    // A connected socket talks with plain read and write.
    let u = sock();
    expect(connect(u, &echo), 0, "connect to the echo server");
    expect(write(u, b"plain"), 5, "write");
    let n = read(u, &mut buf[..16]);
    check(n == 5 && &buf[..5] == b"plain", "read what came back");
    for fd in [s, t, u] {
        close(fd);
    }

    // Name lookup against the host's DNS server (answers alpha.test and beta.test only).
    let mut ip = [0u8; 4];
    expect(resolve(b"alpha.test\0", &mut ip, 300, Some(&dns)), 0, "resolve alpha.test");
    check(ip == [192, 0, 2, 10], "to 192.0.2.10");
    expect(resolve(b"beta.test\0", &mut ip, 300, Some(&dns)), 0, "resolve beta.test");
    check(ip == [192, 0, 2, 20], "to 192.0.2.20");
    expect(resolve(b"Alpha.TEST\0", &mut ip, 300, Some(&dns)), 0, "names are case-insensitive");
    expect(resolve(b"nope.test\0", &mut ip, 300, Some(&dns)), -2, "an unknown name says so");
    let t0 = clock_us();
    expect(resolve(b"alpha.test\0", &mut ip, 150, Some(&SockAddr::new([10, 0, 2, 99], 53))), ETIMEDOUT, "no server: times out");
    let took = clock_us() - t0;
    check(took >= 1_400_000 && took < 2_500_000, "after about the time we allowed");
}

fn main(args: &Args) -> i32 {
    match args.get(1) {
        "local" => local(),
        "tcp-local" => tcp::local(),
        "tcp-ext" => match (parse_ip(args.get(2)), parse_port(args.get(3)), parse_port(args.get(4))) {
            (Some(host), Some(echo), Some(sink)) => tcp::external(host, echo, sink),
            _ => return usage(),
        },
        "tcp-serve" => match (parse_port(args.get(2)), parse_port(args.get(3))) {
            (Some(port), Some(count)) => return tcp::serve(port, count as u32),
            _ => return usage(),
        },
        "tcp-child" => match parse_port(args.get(2)) {
            Some(port) => return tcp::child(port),
            None => return usage(),
        },
        "ext" => match parse_args(args) {
            Some((host, port, dns)) => external(host, port, dns),
            None => {
                eprintln!("usage: nettest ext HOST ECHO_PORT DNS_IP DNS_PORT");
                return 2;
            }
        },
        _ => return usage(),
    }
    let fails = FAILS.load(Ordering::Relaxed);
    let checks = CHECKS.load(Ordering::Relaxed);
    if fails == 0 {
        println!("NETTEST PASSED ({} checks)", checks);
        0
    } else {
        println!("NETTEST FAILED ({} of {} checks)", fails, checks);
        1
    }
}

entry!(main);
