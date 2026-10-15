//! printenv [NAME...]: print the environment (NAME=value lines), or the value of the named variables.

#![no_std]
#![no_main]

use nova::*;

fn main(args: &Args) -> i32 {
    if args.len() < 2 {
        for kv in args.env() {
            println!("{}", kv);
        }
        return 0;
    }
    let mut status = 0;
    for i in 1..args.len() {
        match args.getenv(args.get(i)) {
            Some(v) => println!("{}", v),
            None => status = 1,
        }
    }
    status
}

entry!(main);
