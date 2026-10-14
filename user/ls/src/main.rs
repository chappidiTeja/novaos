//! ls [-l] [paths...]: list directories (default "."), sorted by name.
//! Directories are shown with a trailing '/'. With -l each line starts with the
//! type (d or -) and the size in bytes.

#![no_std]
#![no_main]

use nova::*;

const MAX_ENTRIES: usize = 128;

#[derive(Clone, Copy)]
struct Item {
    name: [u8; 64],
    len: usize,
    kind: u64,
}

const EMPTY_ITEM: Item = Item { name: [0; 64], len: 0, kind: 0 };

fn print_item(item: &Item, long: bool, dir: &str) {
    let name = core::str::from_utf8(&item.name[..item.len]).unwrap_or("?");
    let suffix = if item.kind == KIND_DIR { "/" } else { "" };
    if long {
        let mut full = CString::<320>::new();
        if dir != "." && !dir.is_empty() {
            full.push_str(dir);
            if !dir.ends_with('/') {
                full.push_str("/");
            }
        }
        full.push_str(name);
        let mut st = Stat::default();
        let size = if stat(full.as_cstr(), &mut st) == 0 { st.size } else { 0 };
        let kind = if item.kind == KIND_DIR { 'd' } else { '-' };
        println!("{} {:>9} {}{}", kind, size, name, suffix);
    } else {
        println!("{}{}", name, suffix);
    }
}

/// List one path. Returns the exit status contribution.
fn list(program: &str, path_c: &[u8], path: &str, long: bool, show_header: bool) -> i32 {
    let mut st = Stat::default();
    let r = stat(path_c, &mut st);
    if r < 0 {
        report(program, path, r);
        return 1;
    }
    if st.kind != KIND_DIR {
        let item = Item { kind: st.kind, ..EMPTY_ITEM };
        let mut item = item;
        let bytes = path.as_bytes();
        let start = bytes.iter().rposition(|&b| b == b'/').map_or(0, |i| i + 1);
        let name = &bytes[start..];
        let n = name.len().min(64);
        item.name[..n].copy_from_slice(&name[..n]);
        item.len = n;
        print_item(&item, long, ".");
        return 0;
    }

    let fd = open(path_c, O_RDONLY);
    if fd < 0 {
        report(program, path, fd);
        return 1;
    }
    let mut items = [EMPTY_ITEM; MAX_ENTRIES];
    let mut count = 0;
    let mut entry = DirEntry::EMPTY;
    let mut truncated = false;
    while readdir(fd as u64, &mut entry) == 1 {
        let name = entry.name();
        if name == b"." || name == b".." {
            continue;
        }
        if count == MAX_ENTRIES {
            truncated = true;
            break;
        }
        items[count].name[..name.len()].copy_from_slice(name);
        items[count].len = name.len();
        items[count].kind = entry.kind;
        count += 1;
    }
    close(fd as u64);

    // Insertion sort by name.
    for i in 1..count {
        let mut j = i;
        while j > 0 && items[j - 1].name[..items[j - 1].len] > items[j].name[..items[j].len] {
            items.swap(j - 1, j);
            j -= 1;
        }
    }
    if show_header {
        println!("{}:", path);
    }
    for item in &items[..count] {
        print_item(item, long, path);
    }
    if truncated {
        eprintln!("{}: only the first {} entries are shown", program, MAX_ENTRIES);
    }
    0
}

fn main(args: &Args) -> i32 {
    let mut long = false;
    let mut paths = 0;
    for i in 1..args.len() {
        match args.get(i) {
            "-l" => long = true,
            _ => paths += 1,
        }
    }
    if paths == 0 {
        return list("ls", b".\0", ".", long, false);
    }
    let mut status = 0;
    for i in 1..args.len() {
        if args.get(i) == "-l" {
            continue;
        }
        status |= list("ls", args.cstr(i), args.get(i), long, paths > 1);
    }
    status
}

entry!(main);
