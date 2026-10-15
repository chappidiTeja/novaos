//! fetch [-i] [-L] [-o file] http://host[:port]/path
//!
//! Download a page over HTTP. The body goes to standard output (or to `file`); `-i` also
//! prints the response headers; `-L` follows up to five redirects. Exit status 0 for a
//! 2xx answer, 1 for any other answer or error. (No HTTPS: there is no TLS yet.)

#![no_std]
#![no_main]

use nova::*;

const TIMEOUT: u64 = 1000; // 10 seconds, in ticks

struct Url<'a> {
    host: &'a str,
    port: u16,
    path: &'a str,
}

fn parse_port(s: &str) -> Option<u16> {
    if s.is_empty() || s.len() > 5 || !s.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    let v = s.bytes().fold(0u32, |v, b| v * 10 + (b - b'0') as u32);
    u16::try_from(v).ok().filter(|&p| p != 0)
}

fn parse_url(s: &str) -> Option<Url<'_>> {
    let rest = s.strip_prefix("http://")?;
    let (hostport, path) = match rest.find('/') {
        Some(i) => (&rest[..i], &rest[i..]),
        None => (rest, "/"),
    };
    let (host, port) = match hostport.rsplit_once(':') {
        Some((h, p)) => (h, parse_port(p)?),
        None => (hostport, 80),
    };
    if host.is_empty() || host.len() > 200 || path.len() > 1000 {
        return None;
    }
    Some(Url { host, port, path })
}

/// Position of the blank line that ends the headers, as the index just past it.
fn header_end(buf: &[u8]) -> Option<usize> {
    buf.windows(4).position(|w| w == b"\r\n\r\n").map(|i| i + 4)
}

fn status_of(headers: &[u8]) -> Option<u32> {
    // "HTTP/1.x NNN reason"
    let line_end = headers.iter().position(|&b| b == b'\r').unwrap_or(headers.len());
    let line = core::str::from_utf8(&headers[..line_end]).ok()?;
    let mut parts = line.split(' ');
    let version = parts.next()?;
    if !version.starts_with("HTTP/1.") {
        return None;
    }
    parts.next()?.parse().ok()
}

/// The value of header `name` (case-insensitive), if present.
fn header<'a>(headers: &'a [u8], name: &str) -> Option<&'a str> {
    let text = core::str::from_utf8(headers).ok()?;
    for line in text.split("\r\n").skip(1) {
        if let Some((k, v)) = line.split_once(':') {
            if k.trim().eq_ignore_ascii_case(name) {
                return Some(v.trim());
            }
        }
    }
    None
}

enum Outcome {
    Done(u32),
    Redirect,
    Failed,
}

/// One request/response. Writes the body to `out_fd`. On a redirect, the new address is
/// left in `location`.
fn get(url: &Url<'_>, show_headers: bool, out_fd: u64, location: &mut CString<1100>) -> Outcome {
    let mut name = CString::<256>::new();
    name.push_str(url.host);
    let mut ip = [0u8; 4];
    let r = resolve(name.as_cstr(), &mut ip, 500, None);
    if r < 0 {
        eprintln!("fetch: cannot resolve {}: {}", url.host, strerror(r));
        return Outcome::Failed;
    }
    let sock = socket(SOCK_TCP);
    if sock < 0 {
        eprintln!("fetch: socket: {}", strerror(sock));
        return Outcome::Failed;
    }
    let sock = sock as u64;
    let r = connect_timeout(sock, &SockAddr::new(ip, url.port), TIMEOUT);
    if r < 0 {
        eprintln!("fetch: cannot connect to {}:{}: {}", Ip4(ip), url.port, strerror(r));
        close(sock);
        return Outcome::Failed;
    }
    let mut request = CString::<1500>::new();
    let ok = request.push_str("GET ")
        && request.push_str(url.path)
        && request.push_str(" HTTP/1.0\r\nHost: ")
        && request.push_str(url.host)
        && request.push_str("\r\nUser-Agent: NovaOS-fetch/0.1\r\nAccept: */*\r\nConnection: close\r\n\r\n");
    if !ok {
        eprintln!("fetch: request too long");
        close(sock);
        return Outcome::Failed;
    }
    let r = sendto(sock, request.as_str().as_bytes(), None);
    if r < 0 {
        eprintln!("fetch: send: {}", strerror(r));
        close(sock);
        return Outcome::Failed;
    }

    // Collect the headers (they must fit in 8 KiB), then stream the body.
    let mut head = [0u8; 8192];
    let mut have = 0;
    let end = loop {
        if let Some(e) = header_end(&head[..have]) {
            break e;
        }
        if have == head.len() {
            eprintln!("fetch: response headers too long");
            close(sock);
            return Outcome::Failed;
        }
        let n = recvfrom(sock, &mut head[have..], None, TIMEOUT);
        if n < 0 {
            eprintln!("fetch: receive: {}", strerror(n));
            close(sock);
            return Outcome::Failed;
        }
        if n == 0 {
            eprintln!("fetch: connection closed before the headers ended");
            close(sock);
            return Outcome::Failed;
        }
        have += n as usize;
    };
    let headers = &head[..end];
    let Some(status) = status_of(headers) else {
        eprintln!("fetch: not an HTTP response");
        close(sock);
        return Outcome::Failed;
    };
    if matches!(status, 301 | 302 | 303 | 307 | 308) {
        if let Some(loc) = header(headers, "location") {
            location.clear();
            if location.push_str(loc) {
                close(sock);
                return Outcome::Redirect;
            }
        }
    }
    if show_headers {
        write_all(out_fd, headers);
    }
    if !(200..300).contains(&status) {
        eprintln!("fetch: server answered {}", status);
    }
    // Whatever followed the headers in the buffer, then the rest of the stream.
    write_all(out_fd, &head[end..have]);
    let mut buf = [0u8; 4096];
    loop {
        let n = recvfrom(sock, &mut buf, None, TIMEOUT);
        if n < 0 {
            eprintln!("fetch: receive: {}", strerror(n));
            close(sock);
            return Outcome::Failed;
        }
        if n == 0 {
            break;
        }
        if write_all(out_fd, &buf[..n as usize]) < 0 {
            close(sock);
            return Outcome::Failed;
        }
    }
    close(sock);
    Outcome::Done(status)
}

fn main(args: &Args) -> i32 {
    let mut show_headers = false;
    let mut follow = false;
    let mut out_path: Option<usize> = None;
    let mut target: Option<usize> = None;
    let mut i = 1;
    while i < args.len() {
        match args.get(i) {
            "-i" => show_headers = true,
            "-L" => follow = true,
            "-o" => {
                i += 1;
                if i >= args.len() {
                    eprintln!("fetch: -o needs a file name");
                    return 2;
                }
                out_path = Some(i);
            }
            a if a.starts_with('-') => {
                eprintln!("fetch: unknown option {}", a);
                return 2;
            }
            _ if target.is_none() => target = Some(i),
            _ => {
                eprintln!("usage: fetch [-i] [-L] [-o file] http://host[:port]/path");
                return 2;
            }
        }
        i += 1;
    }
    let Some(target) = target else {
        eprintln!("usage: fetch [-i] [-L] [-o file] http://host[:port]/path");
        return 2;
    };
    let mut out_fd = STDOUT;
    if let Some(p) = out_path {
        let fd = open(args.cstr(p), O_WRONLY | O_CREAT | O_TRUNC);
        if fd < 0 {
            report("fetch", args.get(p), fd);
            return 1;
        }
        out_fd = fd as u64;
    }

    let mut current = CString::<1100>::new();
    current.push_str(args.get(target));
    for _ in 0..6 {
        let text = CString::<1100>::from_str(current.as_str());
        let Some(url) = parse_url(text.as_str()) else {
            eprintln!("fetch: '{}' is not an http:// address", text.as_str());
            return 2;
        };
        let mut next = CString::<1100>::new();
        match get(&url, show_headers, out_fd, &mut next) {
            Outcome::Done(status) => {
                if out_fd != STDOUT {
                    close(out_fd);
                }
                return if (200..300).contains(&status) { 0 } else { 1 };
            }
            Outcome::Failed => return 1,
            Outcome::Redirect => {
                if !follow {
                    eprintln!("fetch: moved to {} (use -L to follow)", next.as_str());
                    return 1;
                }
                // A relative redirect keeps the same server.
                if next.as_str().starts_with('/') {
                    use core::fmt::Write;
                    let mut abs = CString::<1100>::new();
                    if write!(abs, "http://{}:{}{}", url.host, url.port, next.as_str()).is_err() {
                        eprintln!("fetch: redirect address too long");
                        return 1;
                    }
                    current = abs;
                } else {
                    current = next;
                }
            }
        }
    }
    eprintln!("fetch: too many redirects");
    1
}

entry!(main);
