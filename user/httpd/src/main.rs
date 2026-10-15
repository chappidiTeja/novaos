//! httpd [-p port] [-d docroot] [-n requests]
//!
//! A small web server: serves the files under `docroot` (default /www) on `port` (default 80)
//! with HTTP/1.0 semantics, one connection at a time. GET and HEAD only; a request path
//! with `..` or odd characters is refused; a directory without a trailing slash is
//! redirected, and `/dir/` serves `/dir/index.html`. With -n it exits after that many
//! requests (for tests).

#![no_std]
#![no_main]

use core::fmt::Write;
use nova::*;

const REQUEST_TIMEOUT: u64 = 500; // 5 seconds
const MAX_REQUEST: usize = 8192;

fn parse_num(s: &str) -> Option<u64> {
    if s.is_empty() || s.len() > 6 || !s.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    Some(s.bytes().fold(0u64, |v, b| v * 10 + (b - b'0') as u64))
}

fn content_type(path: &str) -> &'static str {
    let ext = path.rsplit_once('.').map_or("", |(_, e)| e);
    match ext {
        "html" | "htm" => "text/html; charset=utf-8",
        "txt" | "md" => "text/plain; charset=utf-8",
        "css" => "text/css",
        "js" => "text/javascript",
        "json" => "application/json",
        "svg" => "image/svg+xml",
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "gif" => "image/gif",
        "ico" => "image/x-icon",
        _ => "application/octet-stream",
    }
}

/// Send a status line and headers.
fn send_head(conn: u64, status: u32, reason: &str, ctype: &str, length: u64, location: Option<&str>) -> bool {
    let mut h = CString::<512>::new();
    let ok = write!(
        h,
        "HTTP/1.0 {} {}\r\nServer: NovaOS-httpd/0.1\r\nContent-Type: {}\r\nContent-Length: {}\r\nConnection: close\r\n",
        status, reason, ctype, length
    )
    .is_ok()
        && match location {
            Some(l) => write!(h, "Location: {}\r\n", l).is_ok(),
            None => true,
        }
        && h.push_str("\r\n");
    ok && write_all(conn, h.as_str().as_bytes()) >= 0
}

/// A short HTML page for an error, answered with `status`.
fn send_error(conn: u64, status: u32, reason: &str, head_only: bool) -> u64 {
    let mut body = CString::<256>::new();
    let _ = write!(body, "<html><body><h1>{} {}</h1></body></html>\n", status, reason);
    let len = body.len() as u64;
    if send_head(conn, status, reason, "text/html; charset=utf-8", len, None) && !head_only {
        write_all(conn, body.as_str().as_bytes());
    }
    len
}

/// The request path is safe to map onto the disk?
fn safe_target(t: &str) -> bool {
    t.starts_with('/')
        && t.len() <= 200
        && !t.contains("..")
        && !t.contains("//")
        && t.bytes().all(|b| b > 0x20 && b < 0x7f && b != b'\\')
}

fn handle(conn: u64, docroot: &str) -> Option<(u32, u64)> {
    // Read the request headers.
    let mut req = [0u8; MAX_REQUEST];
    let mut have = 0;
    loop {
        if req[..have].windows(4).any(|w| w == b"\r\n\r\n") || req[..have].windows(2).any(|w| w == b"\n\n") {
            break;
        }
        if have == req.len() {
            let n = send_error(conn, 431, "Request Header Fields Too Large", false);
            return Some((431, n));
        }
        let n = recvfrom(conn, &mut req[have..], None, REQUEST_TIMEOUT);
        if n == nova::errno::ETIMEDOUT {
            let n = send_error(conn, 408, "Request Timeout", false);
            return Some((408, n));
        }
        if n <= 0 {
            return None; // the client went away
        }
        have += n as usize;
    }
    let line_end = req[..have].iter().position(|&b| b == b'\r' || b == b'\n').unwrap_or(have);
    let Ok(line) = core::str::from_utf8(&req[..line_end]) else {
        let n = send_error(conn, 400, "Bad Request", false);
        return Some((400, n));
    };
    let mut parts = line.split(' ');
    let (Some(method), Some(target), Some(version), None) = (parts.next(), parts.next(), parts.next(), parts.next()) else {
        let n = send_error(conn, 400, "Bad Request", false);
        return Some((400, n));
    };
    if !version.starts_with("HTTP/1.") {
        let n = send_error(conn, 400, "Bad Request", false);
        return Some((400, n));
    }
    let head_only = method == "HEAD";
    if method != "GET" && !head_only {
        let n = send_error(conn, 405, "Method Not Allowed", false);
        return Some((405, n));
    }
    // Strip a query string; the path must be safe before it touches the disk.
    let target = target.split('?').next().unwrap_or("/");
    if !safe_target(target) {
        let n = send_error(conn, 400, "Bad Request", head_only);
        return Some((400, n));
    }
    let mut path = CString::<300>::new();
    let _ = write!(path, "{}{}", docroot, target);
    if target.ends_with('/') {
        path.push_str("index.html");
    }
    let fd = open(path.as_cstr(), O_RDONLY);
    if fd < 0 {
        let n = send_error(conn, 404, "Not Found", head_only);
        return Some((404, n));
    }
    let fd = fd as u64;
    let mut st = Stat::default();
    if fstat(fd, &mut st) < 0 {
        close(fd);
        let n = send_error(conn, 500, "Internal Server Error", head_only);
        return Some((500, n));
    }
    if st.kind == KIND_DIR {
        close(fd);
        if target.ends_with('/') {
            let n = send_error(conn, 404, "Not Found", head_only);
            return Some((404, n));
        }
        let mut loc = CString::<260>::new();
        let _ = write!(loc, "{}/", target);
        let n = send_error_redirect(conn, loc.as_str(), head_only);
        return Some((301, n));
    }
    if !send_head(conn, 200, "OK", content_type(path.as_str()), st.size, None) {
        close(fd);
        return None;
    }
    let mut sent = 0u64;
    if !head_only {
        let mut buf = [0u8; 4096];
        loop {
            let n = read(fd, &mut buf);
            if n <= 0 {
                break;
            }
            if write_all(conn, &buf[..n as usize]) < 0 {
                break;
            }
            sent += n as u64;
        }
    }
    close(fd);
    Some((200, sent))
}

/// Finish a connection politely: say we are done sending, then read and discard whatever the
/// client still sends (up to 2 s). Closing with unread data would reset the connection and
/// could destroy the answer before the client has read it.
fn linger_close(conn: u64) {
    shutdown(conn, SHUT_WR);
    let mut sink = [0u8; 2048];
    let deadline = ticks() + 200;
    while ticks() < deadline {
        let n = recvfrom(conn, &mut sink, None, 50);
        if n == 0 || (n < 0 && n != nova::errno::ETIMEDOUT) {
            break;
        }
    }
    close(conn);
}

fn send_error_redirect(conn: u64, location: &str, head_only: bool) -> u64 {
    let mut body = CString::<300>::new();
    let _ = write!(body, "<html><body>Moved to <a href=\"{}\">{}</a></body></html>\n", location, location);
    let len = body.len() as u64;
    if send_head(conn, 301, "Moved Permanently", "text/html; charset=utf-8", len, Some(location)) && !head_only {
        write_all(conn, body.as_str().as_bytes());
    }
    len
}

fn main(args: &Args) -> i32 {
    let mut port = 80u64;
    let mut docroot = "/www";
    let mut limit = u64::MAX;
    let mut i = 1;
    while i < args.len() {
        match args.get(i) {
            "-p" | "-n" | "-d" => {
                let flag = args.get(i);
                i += 1;
                if i >= args.len() {
                    eprintln!("httpd: {} needs a value", flag);
                    return 2;
                }
                match flag {
                    "-d" => docroot = args.get(i),
                    "-p" => match parse_num(args.get(i)).filter(|&p| (1..=65535).contains(&p)) {
                        Some(p) => port = p,
                        None => {
                            eprintln!("httpd: bad port '{}'", args.get(i));
                            return 2;
                        }
                    },
                    _ => match parse_num(args.get(i)) {
                        Some(n) => limit = n,
                        None => {
                            eprintln!("httpd: bad count '{}'", args.get(i));
                            return 2;
                        }
                    },
                }
            }
            _ => {
                eprintln!("usage: httpd [-p port] [-d docroot] [-n requests]");
                return 2;
            }
        }
        i += 1;
    }
    let docroot = docroot.strip_suffix('/').unwrap_or(docroot);

    let sock = socket(SOCK_TCP);
    if sock < 0 {
        report("httpd", "socket", sock);
        return 1;
    }
    let sock = sock as u64;
    let r = bind(sock, port as u16);
    if r < 0 {
        report("httpd", "bind", r);
        return 1;
    }
    let r = listen(sock, 16);
    if r < 0 {
        report("httpd", "listen", r);
        return 1;
    }
    println!("httpd: serving {} on port {}", docroot, port);

    let mut served = 0u64;
    while served < limit {
        let mut peer = SockAddr::default();
        let conn = accept(sock, Some(&mut peer), FOREVER);
        if conn < 0 {
            report("httpd", "accept", conn);
            sleep(10);
            continue;
        }
        let conn = conn as u64;
        match handle(conn, docroot) {
            Some((status, bytes)) => println!("{} {}:{} -> {} ({} bytes)", "request", Ip4(peer.ip), peer.port, status, bytes),
            None => println!("request {}:{} -> client went away", Ip4(peer.ip), peer.port),
        }
        linger_close(conn);
        served += 1;
    }
    close(sock);
    0
}

entry!(main);
