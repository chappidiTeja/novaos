//! cat [files...]: copy files (or standard input) to standard output.

#![no_std]
#![no_main]

use nova::*;

/// Copy everything readable from `fd` to stdout. Returns 0 or a negative error.
fn copy(fd: u64) -> i64 {
    let mut buf = [0u8; 4096];
    loop {
        let n = read(fd, &mut buf);
        if n < 0 {
            return n;
        }
        if n == 0 {
            return 0;
        }
        let w = write_all(STDOUT, &buf[..n as usize]);
        if w < 0 {
            return w;
        }
    }
}

fn main(args: &Args) -> i32 {
    if args.len() < 2 {
        let r = copy(STDIN);
        if r < 0 {
            report("cat", "standard input", r);
            return 1;
        }
        return 0;
    }
    let mut status = 0;
    for i in 1..args.len() {
        let fd = open(args.cstr(i), O_RDONLY);
        if fd < 0 {
            report("cat", args.get(i), fd);
            status = 1;
            continue;
        }
        let r = copy(fd as u64);
        if r < 0 {
            report("cat", args.get(i), r);
            status = 1;
        }
        close(fd as u64);
    }
    status
}

entry!(main);
