//! wc [files...]: count lines, words and bytes (of standard input if no files).
//! A word is a run of bytes other than ASCII white space (space, \t \n \r \x0b \x0c).

#![no_std]
#![no_main]

use nova::*;

fn is_space(b: u8) -> bool {
    matches!(b, b' ' | b'\t' | b'\n' | b'\r' | 0x0b | 0x0c)
}

/// (lines, words, bytes) of everything readable from `fd`.
fn count(fd: u64) -> Result<(u64, u64, u64), i64> {
    let mut buf = [0u8; 4096];
    let (mut lines, mut words, mut bytes) = (0u64, 0u64, 0u64);
    let mut in_word = false;
    loop {
        let n = read(fd, &mut buf);
        if n < 0 {
            return Err(n);
        }
        if n == 0 {
            return Ok((lines, words, bytes));
        }
        for &b in &buf[..n as usize] {
            if b == b'\n' {
                lines += 1;
            }
            if is_space(b) {
                in_word = false;
            } else if !in_word {
                in_word = true;
                words += 1;
            }
        }
        bytes += n as u64;
    }
}

fn main(args: &Args) -> i32 {
    if args.len() < 2 {
        return match count(STDIN) {
            Ok((l, w, b)) => {
                println!("{} {} {}", l, w, b);
                0
            }
            Err(e) => {
                report("wc", "standard input", e);
                1
            }
        };
    }
    let mut status = 0;
    for i in 1..args.len() {
        let fd = open(args.cstr(i), O_RDONLY);
        if fd < 0 {
            report("wc", args.get(i), fd);
            status = 1;
            continue;
        }
        match count(fd as u64) {
            Ok((l, w, b)) => println!("{} {} {} {}", l, w, b, args.get(i)),
            Err(e) => {
                report("wc", args.get(i), e);
                status = 1;
            }
        }
        close(fd as u64);
    }
    status
}

entry!(main);
