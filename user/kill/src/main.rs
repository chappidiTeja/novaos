//! kill [-SIGNAL] pid...: send a signal (default TERM) to processes. kill -l lists the names.

#![no_std]
#![no_main]

use nova::*;

fn parse_pid(s: &str) -> Option<u64> {
    if s.is_empty() || s.len() > 9 || !s.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    Some(s.bytes().fold(0u64, |v, b| v * 10 + (b - b'0') as u64))
}

fn main(args: &Args) -> i32 {
    let mut sig = SIGTERM;
    let mut first = 1;
    if args.get(1) == "-l" {
        println!("HUP INT QUIT KILL USR1 USR2 ALRM TERM");
        return 0;
    }
    if let Some(flag) = args.get(1).strip_prefix('-') {
        match signal_from_name(flag) {
            Some(s) => sig = s,
            None => {
                eprintln!("kill: unknown signal '{}'", flag);
                return 2;
            }
        }
        first = 2;
    }
    if first >= args.len() {
        eprintln!("usage: kill [-SIGNAL] pid...   (kill -l lists the signals)");
        return 2;
    }
    let mut status = 0;
    for i in first..args.len() {
        let Some(pid) = parse_pid(args.get(i)) else {
            eprintln!("kill: '{}' is not a process id", args.get(i));
            status = 1;
            continue;
        };
        if pid == 0 {
            eprintln!("kill: refusing process 0");
            status = 1;
            continue;
        }
        let r = kill(pid, sig);
        if r < 0 {
            eprintln!("kill: {}: {}", pid, strerror(r));
            status = 1;
        }
    }
    status
}

entry!(main);
