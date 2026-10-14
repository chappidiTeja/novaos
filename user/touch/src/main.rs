//! touch files...: create empty files that do not exist yet.

#![no_std]
#![no_main]

use nova::*;

fn main(args: &Args) -> i32 {
    if args.len() < 2 {
        eprintln!("usage: touch file...");
        return 2;
    }
    let mut status = 0;
    for i in 1..args.len() {
        let fd = open(args.cstr(i), O_WRONLY | O_CREAT);
        if fd < 0 {
            report("touch", args.get(i), fd);
            status = 1;
        } else {
            close(fd as u64);
        }
    }
    status
}

entry!(main);
