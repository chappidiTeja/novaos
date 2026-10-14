//! mkdir dirs...: create directories.

#![no_std]
#![no_main]

use nova::*;

fn main(args: &Args) -> i32 {
    if args.len() < 2 {
        eprintln!("usage: mkdir directory...");
        return 2;
    }
    let mut status = 0;
    for i in 1..args.len() {
        let r = mkdir(args.cstr(i));
        if r < 0 {
            report("mkdir", args.get(i), r);
            status = 1;
        }
    }
    status
}

entry!(main);
