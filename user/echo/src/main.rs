//! echo [-n] [words...]: print the words separated by spaces.

#![no_std]
#![no_main]

use nova::*;

fn main(args: &Args) -> i32 {
    let mut first = 1;
    let mut newline = true;
    if args.get(1) == "-n" {
        newline = false;
        first = 2;
    }
    for i in first..args.len() {
        if i > first {
            print!(" ");
        }
        print!("{}", args.get(i));
    }
    if newline {
        println!();
    }
    0
}

entry!(main);
