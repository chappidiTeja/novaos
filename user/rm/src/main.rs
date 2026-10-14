//! rm files...: remove files (use rmdir for directories).

#![no_std]
#![no_main]

use nova::*;

fn main(args: &Args) -> i32 {
    if args.len() < 2 {
        eprintln!("usage: rm file...");
        return 2;
    }
    let mut status = 0;
    for i in 1..args.len() {
        let r = unlink(args.cstr(i));
        if r < 0 {
            report("rm", args.get(i), r);
            status = 1;
        }
    }
    status
}

entry!(main);
