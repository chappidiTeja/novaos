//! cp source destination: copy a file. If destination is a directory the copy
//! goes inside it under the source's name.

#![no_std]
#![no_main]

use nova::*;

fn base_name(path: &str) -> &str {
    let trimmed = path.trim_end_matches('/');
    match trimmed.rfind('/') {
        Some(i) => &trimmed[i + 1..],
        None => trimmed,
    }
}

fn main(args: &Args) -> i32 {
    if args.len() != 3 {
        eprintln!("usage: cp source destination");
        return 2;
    }
    let (source, dest) = (args.get(1), args.get(2));

    let mut st = Stat::default();
    let mut target = CString::<320>::new();
    if stat(args.cstr(2), &mut st) == 0 && st.kind == KIND_DIR {
        target.push_str(dest);
        if !dest.ends_with('/') {
            target.push_str("/");
        }
        if !target.push_str(base_name(source)) {
            report("cp", dest, -36);
            return 1;
        }
    } else if !target.push_str(dest) {
        report("cp", dest, -36);
        return 1;
    }

    let input = open(args.cstr(1), O_RDONLY);
    if input < 0 {
        report("cp", source, input);
        return 1;
    }
    let output = open(target.as_cstr(), O_WRONLY | O_CREAT | O_TRUNC);
    if output < 0 {
        report("cp", target.as_str(), output);
        close(input as u64);
        return 1;
    }

    let mut buf = [0u8; 4096];
    let mut status = 0;
    loop {
        let n = read(input as u64, &mut buf);
        if n < 0 {
            report("cp", source, n);
            status = 1;
            break;
        }
        if n == 0 {
            break;
        }
        let w = write_all(output as u64, &buf[..n as usize]);
        if w < 0 {
            report("cp", target.as_str(), w);
            status = 1;
            break;
        }
    }
    close(input as u64);
    close(output as u64);
    status
}

entry!(main);
