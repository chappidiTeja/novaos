//! nslookup name [server[:port]]: look up a host name (optionally asking a particular DNS server).

#![no_std]
#![no_main]

use nova::*;

/// "a.b.c.d" or "a.b.c.d:port".
fn parse_server(s: &str) -> Option<SockAddr> {
    let (ip, port) = match s.split_once(':') {
        Some((ip, port)) => {
            if port.is_empty() || port.len() > 5 || !port.bytes().all(|b| b.is_ascii_digit()) {
                return None;
            }
            let p = port.bytes().fold(0u32, |v, b| v * 10 + (b - b'0') as u32);
            (ip, u16::try_from(p).ok().filter(|&p| p != 0)?)
        }
        None => (s, 53),
    };
    Some(SockAddr::new(parse_ip(ip)?, port))
}

fn main(args: &Args) -> i32 {
    if args.len() < 2 || args.len() > 3 {
        eprintln!("usage: nslookup name [server[:port]]");
        return 2;
    }
    let server = if args.len() == 3 {
        match parse_server(args.get(2)) {
            Some(s) => Some(s),
            None => {
                eprintln!("nslookup: '{}' is not an address", args.get(2));
                return 2;
            }
        }
    } else {
        None
    };
    let mut ip = [0u8; 4];
    let r = resolve(args.cstr(1), &mut ip, 500, server.as_ref());
    if r < 0 {
        eprintln!("nslookup: {}: {}", args.get(1), if r == -2 { "no such host" } else { strerror(r) });
        return 1;
    }
    println!("{} has address {}", args.get(1), Ip4(ip));
    0
}

entry!(main);
