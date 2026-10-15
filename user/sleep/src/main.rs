//! sleep seconds: wait. Accepts a fraction ("0.5") or a number of ticks with a 't' suffix.

#![no_std]
#![no_main]

use nova::*;

/// "2", "0.5", "1.25" -> hundredths of a second.
fn parse_hundredths(s: &str) -> Option<u64> {
    let (whole, frac) = s.split_once('.').unwrap_or((s, ""));
    if whole.is_empty() && frac.is_empty() || frac.len() > 2 {
        return None;
    }
    let digits = |t: &str| t.bytes().all(|b| b.is_ascii_digit());
    if !digits(whole) || !digits(frac) || whole.len() > 6 {
        return None;
    }
    let w = whole.bytes().fold(0u64, |v, b| v * 10 + (b - b'0') as u64);
    let mut f = frac.bytes().fold(0u64, |v, b| v * 10 + (b - b'0') as u64);
    if frac.len() == 1 {
        f *= 10;
    }
    Some(w * 100 + f)
}

fn main(args: &Args) -> i32 {
    if args.len() != 2 {
        eprintln!("usage: sleep seconds");
        return 2;
    }
    let Some(ticks) = parse_hundredths(args.get(1)) else {
        eprintln!("sleep: bad time '{}'", args.get(1));
        return 2;
    };
    // One system call sleeps at most 1000 ticks; longer waits repeat.
    let mut left = ticks;
    while left > 0 {
        let step = left.min(1000);
        sleep(step);
        left -= step;
    }
    0
}

entry!(main);
