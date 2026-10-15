//! grep [-i] [-v] [-n] [-c] [-l] [-r] [-H] pattern [files...]
//!
//! Prints the lines that match the pattern (see the `novaregex` crate for the syntax).
//!   -i  ignore case        -v  lines that do NOT match     -n  prefix line numbers
//!   -c  only count         -l  only file names             -r  descend into directories
//!   -E  accepted and ignored (the syntax is always extended)
//!   -H  always prefix the file name (the default with several files or -r)
//! Exit status: 0 if any line was selected, 1 if none, 2 on an error.

#![no_std]
#![no_main]

extern crate alloc;

use alloc::string::String;
use alloc::vec::Vec;
use nova::*;
use novaregex::Regex;

struct Opts {
    invert: bool,
    number: bool,
    count: bool,
    list: bool,
    prefix: bool,
}

fn cstr(path: &str) -> Vec<u8> {
    let mut v = Vec::with_capacity(path.len() + 1);
    v.extend_from_slice(path.as_bytes());
    v.push(0);
    v
}

fn read_all(fd: u64) -> Result<Vec<u8>, i64> {
    let mut out = Vec::new();
    let mut buf = [0u8; 4096];
    loop {
        let n = read(fd, &mut buf);
        if n < 0 {
            return Err(n);
        }
        if n == 0 {
            return Ok(out);
        }
        out.extend_from_slice(&buf[..n as usize]);
    }
}

/// Search one stream. Returns whether any line was selected.
fn search(re: &Regex, o: &Opts, name: &str, data: &[u8]) -> bool {
    let mut selected = 0u64;
    let mut lineno = 0u64;
    let body = data.strip_suffix(b"\n").unwrap_or(data);
    if data.is_empty() {
        return false;
    }
    for raw in body.split(|&b| b == b'\n') {
        lineno += 1;
        let line = String::from_utf8_lossy(raw);
        if re.is_match(&line) == o.invert {
            continue;
        }
        selected += 1;
        if o.count || o.list {
            continue;
        }
        if o.prefix {
            print!("{}:", name);
        }
        if o.number {
            print!("{}:", lineno);
        }
        println!("{}", line);
    }
    if o.list {
        if selected > 0 {
            println!("{}", name);
        }
    } else if o.count {
        if o.prefix {
            print!("{}:", name);
        }
        println!("{}", selected);
    }
    selected > 0
}

/// Search a path; directories are entered only with -r. Returns (matched, error).
fn search_path(re: &Regex, o: &Opts, path: &str, recurse: bool) -> (bool, bool) {
    let mut st = Stat::default();
    if stat(&cstr(path), &mut st) < 0 {
        report("grep", path, stat(&cstr(path), &mut st));
        return (false, true);
    }
    if st.kind == KIND_DIR {
        if !recurse {
            eprintln!("grep: {}: is a directory", path);
            return (false, true);
        }
        let fd = open(&cstr(path), O_RDONLY);
        if fd < 0 {
            report("grep", path, fd);
            return (false, true);
        }
        let mut names = Vec::new();
        let mut entry = DirEntry::EMPTY;
        while readdir(fd as u64, &mut entry) == 1 {
            let n = entry.name();
            if n == b"." || n == b".." {
                continue;
            }
            if let Ok(s) = core::str::from_utf8(n) {
                names.push(String::from(s));
            }
        }
        close(fd as u64);
        names.sort();
        let (mut any, mut err) = (false, false);
        for n in names {
            let child = if path.ends_with('/') { alloc::format!("{}{}", path, n) } else { alloc::format!("{}/{}", path, n) };
            let (m, e) = search_path(re, o, &child, true);
            any |= m;
            err |= e;
        }
        return (any, err);
    }
    let fd = open(&cstr(path), O_RDONLY);
    if fd < 0 {
        report("grep", path, fd);
        return (false, true);
    }
    let data = read_all(fd as u64);
    close(fd as u64);
    match data {
        Ok(d) => (search(re, o, path, &d), false),
        Err(e) => {
            report("grep", path, e);
            (false, true)
        }
    }
}

fn main(args: &Args) -> i32 {
    let mut o = Opts { invert: false, number: false, count: false, list: false, prefix: false };
    let (mut icase, mut recurse, mut force_prefix) = (false, false, false);
    let mut i = 1;
    while i < args.len() {
        let a = args.get(i);
        if a == "--" {
            i += 1;
            break;
        }
        if !a.starts_with('-') || a.len() < 2 {
            break;
        }
        for c in a[1..].chars() {
            match c {
                'i' => icase = true,
                'v' => o.invert = true,
                'n' => o.number = true,
                'c' => o.count = true,
                'l' => o.list = true,
                'r' => recurse = true,
                'H' => force_prefix = true,
                'E' => {} // the syntax is always the extended one
                _ => {
                    eprintln!("grep: unknown option -{}", c);
                    return 2;
                }
            }
        }
        i += 1;
    }
    if i >= args.len() {
        eprintln!("usage: grep [-ivncl rH] pattern [files...]");
        return 2;
    }
    let re = match Regex::new(args.get(i), icase) {
        Ok(r) => r,
        Err(e) => {
            eprintln!("grep: bad pattern: {}", e);
            return 2;
        }
    };
    i += 1;
    let files = args.len() - i;
    o.prefix = force_prefix || files > 1 || recurse;
    if files == 0 {
        return match read_all(STDIN) {
            Ok(d) => {
                if search(&re, &o, "(standard input)", &d) {
                    0
                } else {
                    1
                }
            }
            Err(e) => {
                report("grep", "standard input", e);
                2
            }
        };
    }
    let (mut any, mut err) = (false, false);
    for k in i..args.len() {
        let (m, e) = search_path(&re, &o, args.get(k), recurse);
        any |= m;
        err |= e;
    }
    if err {
        2
    } else if any {
        0
    } else {
        1
    }
}

entry!(main);
