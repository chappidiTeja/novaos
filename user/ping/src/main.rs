//! ping [-c count] [-s size] [-i ticks] host: send echo requests and report the round trips.
//! -i is the pause between requests in timer ticks of 10 ms (default 100 = one second).

#![no_std]
#![no_main]

use nova::*;

fn parse_num(s: &str) -> Option<u64> {
    if s.is_empty() || s.len() > 6 || !s.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    Some(s.bytes().fold(0u64, |v, b| v * 10 + (b - b'0') as u64))
}

/// Microseconds as "1.234" milliseconds.
struct Ms(u64);

impl core::fmt::Display for Ms {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "{}.{:03}", self.0 / 1000, self.0 % 1000)
    }
}

fn usage() -> i32 {
    eprintln!("usage: ping [-c count] [-s size] [-i ticks] host");
    2
}

fn main(args: &Args) -> i32 {
    let mut count = 4u64;
    let mut size = 32u64;
    let mut interval = 100u64;
    let mut host: Option<&str> = None;
    let mut host_at = 0;
    let mut i = 1;
    while i < args.len() {
        match args.get(i) {
            "-c" | "-s" | "-i" => {
                let flag = args.get(i);
                i += 1;
                let Some(v) = parse_num(args.get(i)) else { return usage() };
                if flag == "-i" {
                    interval = v;
                } else if flag == "-c" {
                    count = v.max(1);
                } else {
                    size = v;
                }
            }
            h if host.is_none() && !h.starts_with('-') => {
                host = Some(h);
                host_at = i;
            }
            _ => return usage(),
        }
        i += 1;
    }
    let Some(host) = host else { return usage() };
    if size > 1400 {
        eprintln!("ping: size must be at most 1400");
        return 2;
    }

    let mut ip = [0u8; 4];
    let r = resolve(args.cstr(host_at), &mut ip, 500, None);
    if r < 0 {
        eprintln!("ping: cannot resolve {}: {}", host, strerror(r));
        return 1;
    }
    println!("PING {} ({}): {} bytes of data", host, Ip4(ip), size);

    let (mut sent, mut got) = (0u64, 0u64);
    let (mut min, mut max, mut sum) = (u64::MAX, 0u64, 0u64);
    for n in 1..=count {
        sent += 1;
        let r = ping(ip, size, 200);
        if r >= 0 {
            let us = r as u64;
            got += 1;
            sum += us;
            min = min.min(us);
            max = max.max(us);
            println!("reply from {}: bytes={} seq={} time={} ms", Ip4(ip), size, n, Ms(us));
        } else {
            println!("seq={}: {}", n, strerror(r));
        }
        if n < count {
            sleep(interval);
        }
    }
    let lost = (sent - got) * 100 / sent;
    println!("--- {} ping statistics ---", host);
    println!("{} sent, {} received, {}% loss", sent, got, lost);
    if got > 0 {
        println!("rtt min/avg/max = {}/{}/{} ms", Ms(min), Ms(sum / got), Ms(max));
        0
    } else {
        1
    }
}

entry!(main);
