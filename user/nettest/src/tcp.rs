//! TCP tests for `nettest`.
//!
//!   nettest tcp-local                    sockets by themselves: misuse, loopback, a child process
//!   nettest tcp-ext HOST ECHO_PORT SINK_PORT
//!                                        against the echo and sink servers on the host
//!   nettest tcp-serve PORT COUNT         (for the host to test the other direction) accept COUNT
//!                                        connections, echo everything, then send "done N"
//!   nettest tcp-child PORT               (used by tcp-local) accept one connection and echo it

use crate::{check, expect, raw, wait_address, KERNEL, NOWHERE};
use nova::errno::*;
use nova::*;

fn pattern(buf: &mut [u8], offset: usize) {
    for (i, b) in buf.iter_mut().enumerate() {
        let n = offset + i;
        *b = (n ^ (n >> 8) ^ (n >> 16) ^ 0x5A) as u8;
    }
}

fn tcp() -> u64 {
    let fd = socket(SOCK_TCP);
    check(fd >= 0, "a TCP socket can be created");
    fd as u64
}

/// A listening socket on `port`.
fn listener(port: u16) -> u64 {
    let fd = tcp();
    expect(bind(fd, port), port as i64, "bind");
    expect(listen(fd, 8), 0, "listen");
    fd
}

/// Send all of `data` and read the same number of bytes back, comparing.
fn echo_round(fd: u64, data: &[u8], scratch: &mut [u8]) -> bool {
    if sendto(fd, data, None) != data.len() as i64 {
        return false;
    }
    let mut got = 0;
    while got < data.len() {
        let n = recvfrom(fd, &mut scratch[..data.len() - got], None, 1000);
        if n <= 0 {
            return false;
        }
        if scratch[..n as usize] != data[got..got + n as usize] {
            return false;
        }
        got += n as usize;
    }
    true
}

pub fn local() {
    let info = wait_address();
    let me = info.ip;
    let mut buf = [0u8; 4096];
    let mut peer = SockAddr::default();

    // ---- misuse of unconnected sockets
    let t = tcp();
    expect(listen(t, 5), EINVAL, "listen needs a bound port");
    expect(bind(t, 7100), 7100, "bind");
    expect(bind(t, 7101), EINVAL, "binding twice");
    expect(sendto(t, b"x", None), ENOTCONN, "send on an unconnected socket");
    expect(recvfrom(t, &mut buf, None, 0), ENOTCONN, "receive on an unconnected socket");
    expect(shutdown(t, SHUT_WR), ENOTCONN, "shutdown on an unconnected socket");
    expect(accept(t, None, 0), EINVAL, "accept before listen");
    expect(raw(SYS_GETPEERNAME, t, &mut peer as *mut SockAddr as u64, 0, 0, 0), ENOTCONN, "no peer yet");
    expect(listen(t, 8), 0, "listen");
    let u = tcp();
    expect(bind(u, 7100), 7100, "a second socket may name the same port");
    expect(listen(u, 5), EADDRINUSE, "but only one can listen on it");
    close(u);
    expect(accept(t, None, 0), EAGAIN, "no connection waiting");
    let t0 = clock_us();
    expect(accept(t, None, 5), ETIMEDOUT, "waiting 5 ticks for nobody");
    check(clock_us() - t0 >= 35_000 && clock_us() - t0 < 150_000, "and it waited about 50 ms (a tick is 10 ms)");
    expect(fd_ready(t), 0, "a listener with nobody waiting is not ready");
    expect(listen(socket(SOCK_UDP) as u64, 5), EINVAL, "listen on a UDP socket");
    expect(shutdown(socket(SOCK_UDP) as u64, SHUT_WR), EINVAL, "shutdown on a UDP socket");

    // ---- connecting to ourselves
    let c = tcp();
    expect(connect_timeout(c, &SockAddr::new(me, 7100), 200), 0, "connect to our own listener");
    expect(connect(c, &SockAddr::new(me, 7100)), -106, "connecting twice");
    expect(fd_ready(t), 1, "the listener is ready: a connection waits");
    expect(raw(SYS_ACCEPT, t, NOWHERE, 0, 0, 0), EFAULT, "accept with a bad address pointer");
    expect(fd_ready(t), 1, "and the connection was not lost by that failure");
    let s = accept(t, Some(&mut peer), 0);
    check(s >= 0, "accept");
    let s = s as u64;
    let mut mine = SockAddr::default();
    expect(getsockname(c, &mut mine), 0, "getsockname");
    check(mine.ip == me && mine.port >= 49152, "the client got an ephemeral port");
    check(peer.ip == me && peer.port == mine.port, "and the server saw that port");
    let mut other = SockAddr::default();
    expect(getpeername(c, &mut other), 0, "getpeername");
    check(other == SockAddr::new(me, 7100), "the client's peer is the listener's port");
    expect(getpeername(s, &mut other), 0, "getpeername on the server side");
    check(other.port == mine.port, "is the client");
    expect(getsockname(s, &mut other), 0, "server getsockname");
    check(other.port == 7100, "keeps the listening port");

    // ---- talking
    expect(sendto(c, b"hello server", None), 12, "send");
    expect(recvfrom(s, &mut buf, None, 100), 12, "receive");
    check(&buf[..12] == b"hello server", "the same bytes");
    expect(write(s, b"hello client"), 12, "write on a socket descriptor");
    expect(fd_ready(c), 1, "readable");
    expect(read(c, &mut buf), 12, "read on a socket descriptor");
    expect(fd_ready(c), 0, "and now empty");
    expect(sendto(c, b"", None), 0, "an empty send is fine");
    expect(recvfrom(s, &mut buf, None, 0), EAGAIN, "and sends nothing");
    // A stream has no message boundaries: three sends can come back as one read, and a
    // short buffer takes only what fits.
    sendto(c, b"abc", None);
    sendto(c, b"defg", None);
    sendto(c, b"hij", None);
    sleep(5);
    let mut small = [0u8; 4];
    expect(recvfrom(s, &mut small, None, 100), 4, "a short buffer takes what fits");
    check(&small == b"abcd", "from the front");
    expect(recvfrom(s, &mut buf, None, 100), 6, "the rest comes in one piece");
    check(&buf[..6] == b"efghij", "in order");
    // Bad pointers do not consume data.
    sendto(c, b"keep", None);
    sleep(3);
    expect(raw(SYS_RECVFROM, s, NOWHERE, 16, 0, 100), EFAULT, "receive into unmapped memory");
    expect(raw(SYS_RECVFROM, s, KERNEL, 16, 0, 100), EFAULT, "receive into kernel memory");
    expect(recvfrom(s, &mut buf, None, 100), 4, "the data survived");
    expect(raw(SYS_SENDTO, c, NOWHERE, 10, 0, 0), EFAULT, "send from unmapped memory");

    // ---- ending: shutdown gives the other side an end of file but keeps the reverse direction
    expect(shutdown(c, 2), EINVAL, "only shutting down the sending side is supported");
    expect(sendto(c, b"last words", None), 10, "last words");
    expect(shutdown(c, SHUT_WR), 0, "shutdown");
    expect(sendto(c, b"x", None), EPIPE, "no more sending after shutdown");
    expect(recvfrom(s, &mut buf, None, 100), 10, "the server gets the data");
    expect(recvfrom(s, &mut buf, None, 100), 0, "then end of file");
    expect(recvfrom(s, &mut buf, None, 0), 0, "and again, not an error");
    expect(sendto(s, b"still talking", None), 13, "the server can still answer");
    expect(recvfrom(c, &mut buf, None, 100), 13, "and the client still hears it");
    expect(close(s), 0, "the server closes");
    expect(recvfrom(c, &mut buf, None, 100), 0, "the client sees end of file");
    expect(close(c), 0, "close");

    // ---- refused, unreachable, and a flood of connections
    let r = tcp();
    expect(connect_timeout(r, &SockAddr::new(me, 7999), 200), ECONNREFUSED, "nobody listens on 7999");
    expect(connect(r, &SockAddr::new(me, 7999)), EINVAL, "a failed socket cannot be used again");
    let r = tcp();
    let t0 = clock_us();
    expect(connect_timeout(r, &SockAddr::new([10, 0, 2, 77], 80), 100), ETIMEDOUT, "a silent address");
    let took = clock_us() - t0;
    check(took >= 900_000 && took < 1_500_000, "after the time we allowed (1 s)");
    let mut clients = [0u64; 6];
    for slot in clients.iter_mut() {
        let k = tcp();
        expect(connect_timeout(k, &SockAddr::new(me, 7100), 200), 0, "several clients connect before anyone accepts");
        *slot = k;
    }
    let mut served = 0;
    while accept(t, None, 0) >= 0 {
        served += 1;
    }
    check(served >= 6 && served <= 8, "the listener queued them all (backlog 8)");

    // ---- a listener can be closed and its port reused at once
    close(t);
    for k in clients {
        close(k);
    }
    let again = listener(7100);
    close(again);

    // ---- running out of descriptors says so, and everything is released
    let mut held = [0u64; 80];
    let mut n = 0;
    loop {
        let fd = socket(SOCK_TCP);
        if fd < 0 {
            expect(fd, EMFILE, "out of descriptors");
            break;
        }
        held[n] = fd as u64;
        n += 1;
    }
    for &fd in &held[..n] {
        close(fd);
    }
    let last = listener(7100);
    close(last);

    bulk_with_child(me);
}

/// Start a copy of this program as an echo server and push megabytes through it.
fn bulk_with_child(me: [u8; 4]) {
    let argv = b"nettest\0tcp-child\07102\0";
    let child = spawn(b"/bin/nettest\0", argv, 3, None);
    check(child > 0, "spawn the echo server process");
    // The child needs a moment to start listening; a failed socket cannot be reused, so each
    // attempt uses a fresh one.
    let mut sock = 0;
    let mut connected = false;
    for _ in 0..100 {
        sock = tcp();
        if connect_timeout(sock, &SockAddr::new(me, 7102), 100) == 0 {
            connected = true;
            break;
        }
        close(sock);
        sleep(5);
    }
    if !connected {
        check(false, "the child server never started listening");
        return;
    }
    let mut out = [0u8; 16384];
    let mut back = [0u8; 16384];
    let total = 3usize << 20;
    let t0 = clock_us();
    let mut ok = true;
    let mut sent = 0;
    while sent < total && ok {
        let len = out.len().min(total - sent);
        pattern(&mut out[..len], sent);
        ok = echo_round(sock, &out[..len], &mut back);
        sent += len;
    }
    check(ok, "3 MB echoed through another process, byte for byte");
    let ms = (clock_us() - t0) / 1000;
    println!("tcp loopback echo: {} KB in {} ms", sent / 1024, ms);
    expect(shutdown(sock, SHUT_WR), 0, "shut down our side");
    expect(recvfrom(sock, &mut back, None, 500), 0, "the child ends the stream after its end of file");
    close(sock);
    expect(waitpid(child as u64), 0, "the echo server exits cleanly");
}

/// Accept one connection and echo until the client is done.
pub fn child(port: u16) -> i32 {
    let l = socket(SOCK_TCP);
    if l < 0 || bind(l as u64, port) < 0 || listen(l as u64, 4) < 0 {
        eprintln!("tcp-child: cannot listen on {}", port);
        return 1;
    }
    let c = accept(l as u64, None, 1000);
    if c < 0 {
        eprintln!("tcp-child: accept failed: {}", strerror(c));
        return 1;
    }
    let c = c as u64;
    let mut buf = [0u8; 16384];
    loop {
        let n = recvfrom(c, &mut buf, None, 1000);
        if n < 0 {
            eprintln!("tcp-child: receive failed: {}", strerror(n));
            return 1;
        }
        if n == 0 {
            break;
        }
        if write_all(c, &buf[..n as usize]) < 0 {
            return 1;
        }
    }
    close(c);
    close(l as u64);
    0
}

/// For a client on the host: accept `count` connections; echo each, then say "done N".
pub fn serve(port: u16, count: u32) -> i32 {
    let _ = wait_address();
    let l = socket(SOCK_TCP);
    if l < 0 || bind(l as u64, port) < 0 || listen(l as u64, 8) < 0 {
        eprintln!("tcp-serve: cannot listen on {}", port);
        return 1;
    }
    println!("tcp-serve: listening on {} for {} connections", port, count);
    let mut buf = [0u8; 8192];
    for _ in 0..count {
        let mut peer = SockAddr::default();
        let c = accept(l as u64, Some(&mut peer), 2000);
        if c < 0 {
            eprintln!("tcp-serve: accept failed: {}", strerror(c));
            return 1;
        }
        let c = c as u64;
        let mut total = 0u64;
        loop {
            let n = recvfrom(c, &mut buf, None, 1000);
            if n < 0 {
                eprintln!("tcp-serve: receive failed: {}", strerror(n));
                break;
            }
            if n == 0 {
                break;
            }
            total += n as u64;
            if write_all(c, &buf[..n as usize]) < 0 {
                break;
            }
        }
        let mut done = CString::<64>::new();
        use core::fmt::Write;
        let _ = write!(done, "done {}\n", total);
        write_all(c, done.as_str().as_bytes());
        close(c);
        println!("tcp-serve: {}:{} sent {} bytes", Ip4(peer.ip), peer.port, total);
    }
    close(l as u64);
    0
}

pub fn external(host: [u8; 4], echo_port: u16, sink_port: u16) {
    let _ = wait_address();
    let mut out = [0u8; 16384];
    let mut back = [0u8; 16384];

    // Echo: small and awkward sizes first, then a megabyte or two.
    let s = tcp();
    expect(connect_timeout(s, &SockAddr::new(host, echo_port), 500), 0, "connect to the host's echo server");
    let mut ok = true;
    for &len in &[1usize, 2, 7, 100, 1459, 1460, 1461, 4096, 16384] {
        pattern(&mut out[..len], len);
        ok &= echo_round(s, &out[..len], &mut back);
    }
    check(ok, "echo of 1 .. 16384 bytes, around the segment size");
    let total = 2usize << 20;
    let t0 = clock_us();
    let mut sent = 0;
    ok = true;
    while sent < total && ok {
        let len = out.len().min(total - sent);
        pattern(&mut out[..len], sent);
        ok = echo_round(s, &out[..len], &mut back);
        sent += len;
    }
    check(ok, "2 MB echoed by the host, byte for byte");
    let ms = ((clock_us() - t0) / 1000).max(1);
    println!("tcp echo with the host: {} KB in {} ms ({} KB/s each way)", sent / 1024, ms, sent as u64 / 1024 * 1000 / ms);
    expect(shutdown(s, SHUT_WR), 0, "shutdown");
    expect(recvfrom(s, &mut back, None, 500), 0, "the host closes its side too");
    close(s);

    // Sink: push 8 MB one way; the host answers with a byte count and a checksum.
    let s = tcp();
    expect(connect_timeout(s, &SockAddr::new(host, sink_port), 500), 0, "connect to the sink");
    let total = 8usize << 20;
    let mut checksum: u32 = 0;
    let mut sent = 0;
    let t0 = clock_us();
    while sent < total {
        let len = out.len().min(total - sent);
        pattern(&mut out[..len], sent);
        for &b in &out[..len] {
            checksum = checksum.wrapping_add(b as u32);
        }
        if sendto(s, &out[..len], None) != len as i64 {
            check(false, "sending 8 MB to the sink");
            break;
        }
        sent += len;
    }
    expect(shutdown(s, SHUT_WR), 0, "shutdown after the upload");
    let n = recvfrom(s, &mut back, None, 3000);
    let reply = if n > 0 { core::str::from_utf8(&back[..n as usize]).unwrap_or("") } else { "" };
    let ms = ((clock_us() - t0) / 1000).max(1);
    println!("tcp upload to the host: {} KB in {} ms ({} KB/s)", sent / 1024, ms, sent as u64 / 1024 * 1000 / ms);
    let mut expected = CString::<64>::new();
    {
        use core::fmt::Write;
        let _ = write!(expected, "{} {}\n", total, checksum);
    }
    check(reply == expected.as_str(), "the host counted every byte and got the same checksum");
    if reply != expected.as_str() {
        println!("  host said {:?}, expected {:?}", reply, expected.as_str());
    }
    close(s);
    // A connection to a port nobody listens on is refused by a real machine as well.
    let r = tcp();
    expect(connect_timeout(r, &SockAddr::new(host, 1), 500), ECONNREFUSED, "the host refuses port 1");
}
