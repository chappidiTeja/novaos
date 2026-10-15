//! cksum [files...]: print the CRC-32 (the one used by zip and PNG), the size in bytes and the
//! name of each file; of standard input if there are no files.

#![no_std]
#![no_main]

use nova::*;

const fn make_table() -> [u32; 256] {
    let mut table = [0u32; 256];
    let mut i = 0;
    while i < 256 {
        let mut c = i as u32;
        let mut k = 0;
        while k < 8 {
            c = if c & 1 != 0 { 0xEDB8_8320 ^ (c >> 1) } else { c >> 1 };
            k += 1;
        }
        table[i] = c;
        i += 1;
    }
    table
}

static TABLE: [u32; 256] = make_table();

fn crc_of(fd: u64) -> Result<(u32, u64), i64> {
    let mut crc = !0u32;
    let mut size = 0u64;
    let mut buf = [0u8; 4096];
    loop {
        let n = read(fd, &mut buf);
        if n < 0 {
            return Err(n);
        }
        if n == 0 {
            return Ok((!crc, size));
        }
        for &b in &buf[..n as usize] {
            crc = TABLE[((crc ^ b as u32) & 0xFF) as usize] ^ (crc >> 8);
        }
        size += n as u64;
    }
}

fn main(args: &Args) -> i32 {
    if args.len() < 2 {
        return match crc_of(STDIN) {
            Ok((crc, size)) => {
                println!("{:08x} {}", crc, size);
                0
            }
            Err(e) => {
                report("cksum", "standard input", e);
                1
            }
        };
    }
    let mut status = 0;
    for i in 1..args.len() {
        let fd = open(args.cstr(i), O_RDONLY);
        if fd < 0 {
            report("cksum", args.get(i), fd);
            status = 1;
            continue;
        }
        match crc_of(fd as u64) {
            Ok((crc, size)) => println!("{:08x} {} {}", crc, size, args.get(i)),
            Err(e) => {
                report("cksum", args.get(i), e);
                status = 1;
            }
        }
        close(fd as u64);
    }
    status
}

entry!(main);
