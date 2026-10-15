//! Small helpers around the file system calls, in terms of `String` (the shell has a heap).

use alloc::string::String;
use alloc::vec::Vec;
use nova::*;

/// A NUL-terminated copy of `path`, ready for a system call.
pub fn cstr(path: &str) -> Vec<u8> {
    let mut v = Vec::with_capacity(path.len() + 1);
    v.extend_from_slice(path.as_bytes());
    v.push(0);
    v
}

pub fn stat_path(path: &str) -> Option<Stat> {
    let mut st = Stat::default();
    if stat(&cstr(path), &mut st) < 0 {
        None
    } else {
        Some(st)
    }
}

pub fn exists(path: &str) -> bool {
    stat_path(path).is_some()
}

pub fn is_dir(path: &str) -> bool {
    stat_path(path).map_or(false, |s| s.kind == KIND_DIR)
}

pub fn is_file(path: &str) -> bool {
    stat_path(path).map_or(false, |s| s.kind == KIND_FILE)
}

pub fn file_size(path: &str) -> Option<u64> {
    stat_path(path).map(|s| s.size)
}

/// The names in a directory ("" means the working directory), without "." and "..".
pub fn list_dir(dir: &str) -> Vec<String> {
    let path = if dir.is_empty() { "." } else { dir };
    let fd = open(&cstr(path), O_RDONLY);
    if fd < 0 {
        return Vec::new();
    }
    let mut names = Vec::new();
    let mut entry = DirEntry::EMPTY;
    while readdir(fd as u64, &mut entry) == 1 {
        let name = entry.name();
        if name == b"." || name == b".." {
            continue;
        }
        if let Ok(s) = core::str::from_utf8(name) {
            names.push(String::from(s));
        }
    }
    close(fd as u64);
    names
}

/// Everything in a file, or the error code.
pub fn read_file(path: &str) -> Result<Vec<u8>, i64> {
    let fd = open(&cstr(path), O_RDONLY);
    if fd < 0 {
        return Err(fd);
    }
    let mut out = Vec::new();
    let mut buf = [0u8; 4096];
    loop {
        let n = read(fd as u64, &mut buf);
        if n < 0 {
            close(fd as u64);
            return Err(n);
        }
        if n == 0 {
            break;
        }
        out.extend_from_slice(&buf[..n as usize]);
    }
    close(fd as u64);
    Ok(out)
}

/// The working directory.
pub fn cwd() -> String {
    let mut buf = [0u8; 256];
    String::from(getcwd(&mut buf).unwrap_or("/"))
}

/// `dir` joined with `name` (`name` alone if `dir` is empty).
pub fn join(dir: &str, name: &str) -> String {
    if dir.is_empty() {
        String::from(name)
    } else if dir.ends_with('/') {
        alloc::format!("{}{}", dir, name)
    } else {
        alloc::format!("{}/{}", dir, name)
    }
}
