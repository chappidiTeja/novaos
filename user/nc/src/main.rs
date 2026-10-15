//! nc host port      connect and copy standard input to the connection and the connection
//! nc -l port        to standard output; wait for one connection instead
//!
//! When standard input ends, the sending side of the connection is shut down (the other
//! end reads end-of-file); the program ends when the other side closes.

#![no_std]
#![no_main]

use nova::*;

fn parse_port(s: &str) -> Option<u16> {
    if s.is_empty() || s.len() > 5 || !s.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    let v = s.bytes().fold(0u32, |v, b| v * 10 + (b - b'0') as u32);
    u16::try_from(v).ok().filter(|&p| p != 0)
}

fn usage() -> i32 {
    eprintln!("usage: nc host port | nc -l port");
    2
}

fn main(args: &Args) -> i32 {
    let sock;
    if args.get(1) == "-l" {
        if args.len() != 3 {
            return usage();
        }
        let Some(port) = parse_port(args.get(2)) else { return usage() };
        let listener = socket(SOCK_TCP);
        if listener < 0 {
            report("nc", "socket", listener);
            return 1;
        }
        let listener = listener as u64;
        let r = bind(listener, port);
        if r < 0 {
            report("nc", "bind", r);
            return 1;
        }
        let r = listen(listener, 1);
        if r < 0 {
            report("nc", "listen", r);
            return 1;
        }
        let c = accept(listener, None, FOREVER);
        if c < 0 {
            report("nc", "accept", c);
            return 1;
        }
        close(listener);
        sock = c as u64;
    } else {
        if args.len() != 3 {
            return usage();
        }
        let Some(port) = parse_port(args.get(2)) else { return usage() };
        let mut ip = [0u8; 4];
        let r = resolve(args.cstr(1), &mut ip, 500, None);
        if r < 0 {
            report("nc", args.get(1), r);
            return 1;
        }
        let s = socket(SOCK_TCP);
        if s < 0 {
            report("nc", "socket", s);
            return 1;
        }
        let r = connect_timeout(s as u64, &SockAddr::new(ip, port), 1000);
        if r < 0 {
            report("nc", "connect", r);
            return 1;
        }
        sock = s as u64;
    }

    let mut stdin_open = true;
    let mut buf = [0u8; 2048];
    loop {
        let mut idle = true;
        if stdin_open && fd_ready(STDIN) == 1 {
            idle = false;
            let n = read(STDIN, &mut buf);
            if n <= 0 {
                stdin_open = false;
                shutdown(sock, SHUT_WR);
            } else if sendto(sock, &buf[..n as usize], None) < 0 {
                break;
            }
        }
        if fd_ready(sock) == 1 {
            idle = false;
            let n = recvfrom(sock, &mut buf, None, 0);
            if n == 0 {
                break; // the other side is done
            }
            if n > 0 {
                write_all(STDOUT, &buf[..n as usize]);
            } else if n != nova::errno::EAGAIN {
                report("nc", "receive", n);
                close(sock);
                return 1;
            }
        }
        if idle {
            sleep(1);
        }
    }
    close(sock);
    0
}

entry!(main);
