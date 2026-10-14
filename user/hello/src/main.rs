//! Test program for NovaOS user mode. The kernel starts it with a "mode" in rdi
//! that selects one behaviour, so a single ELF covers well-behaved and hostile cases.

#![no_std]
#![no_main]

use core::arch::asm;
use core::hint::black_box;
use core::panic::PanicInfo;
use core::ptr::{addr_of_mut, read_volatile, write_volatile};

const SYS_EXIT: u64 = 0;
const SYS_WRITE: u64 = 1;
const SYS_GETPID: u64 = 2;
const SYS_SLEEP: u64 = 3;
const SYS_YIELD: u64 = 4;
const SYS_TICKS: u64 = 5;
const SYS_READ: u64 = 6;
const SYS_PIPE: u64 = 7;
const SYS_CLOSE: u64 = 8;
const SYS_OPEN: u64 = 9;
const SYS_LSEEK: u64 = 10;
const SYS_FSTAT: u64 = 11;
const SYS_STAT: u64 = 12;
const SYS_READDIR: u64 = 13;
const SYS_MKDIR: u64 = 14;
const SYS_UNLINK: u64 = 15;
const SYS_RMDIR: u64 = 16;
const SYS_CHDIR: u64 = 17;
const SYS_GETCWD: u64 = 18;

const O_RDONLY: u64 = 0;
const O_WRONLY: u64 = 1;
const O_RDWR: u64 = 2;
const O_CREAT: u64 = 0x40;
const O_EXCL: u64 = 0x80;
const O_TRUNC: u64 = 0x200;
const O_APPEND: u64 = 0x400;
const SEEK_SET: u64 = 0;
const SEEK_CUR: u64 = 1;
const SEEK_END: u64 = 2;

const ENOENT: i64 = -2;
const EEXIST: i64 = -17;
const ENOTDIR: i64 = -20;
const EISDIR: i64 = -21;
const ERANGE: i64 = -34;
const ENAMETOOLONG: i64 = -36;
const ENOTEMPTY: i64 = -39;
const EBADF: i64 = -9;
const EFAULT: i64 = -14;
const EINVAL: i64 = -22;
const EPIPE: i64 = -32;
const ENOSYS: i64 = -38;

/// Top of the user stack, as laid out by the kernel.
const STACK_TOP: u64 = 0x7fff_ffff_f000;

/// The kernel clobbers rdi, rsi, rdx, r10, r8, r9 on every system call.
#[inline(always)]
unsafe fn syscall(nr: u64, a1: u64, a2: u64, a3: u64) -> i64 {
    let ret: i64;
    asm!(
        "syscall",
        inlateout("rax") nr as i64 => ret,
        inlateout("rdi") a1 => _,
        inlateout("rsi") a2 => _,
        inlateout("rdx") a3 => _,
        out("r10") _,
        out("r8") _,
        out("r9") _,
        out("rcx") _,
        out("r11") _,
        options(nostack),
    );
    ret
}

fn write(fd: u64, ptr: u64, len: u64) -> i64 {
    unsafe { syscall(SYS_WRITE, fd, ptr, len) }
}

fn read(fd: u64, ptr: u64, len: u64) -> i64 {
    unsafe { syscall(SYS_READ, fd, ptr, len) }
}

fn close(fd: u64) -> i64 {
    unsafe { syscall(SYS_CLOSE, fd, 0, 0) }
}

fn print(s: &str) {
    write(1, s.as_ptr() as u64, s.len() as u64);
}

fn print_num(mut n: u64) {
    let mut buf = [0u8; 20];
    let mut i = buf.len();
    loop {
        i -= 1;
        buf[i] = b'0' + (n % 10) as u8;
        n /= 10;
        if n == 0 {
            break;
        }
    }
    write(1, buf[i..].as_ptr() as u64, (buf.len() - i) as u64);
}

fn print_signed(n: i64) {
    if n < 0 {
        print("-");
    }
    print_num(n.unsigned_abs());
}

fn exit(code: i64) -> ! {
    unsafe { syscall(SYS_EXIT, code as u64, 0, 0) };
    loop {}
}

fn getpid() -> u64 {
    unsafe { syscall(SYS_GETPID, 0, 0, 0) as u64 }
}

fn sleep(ticks: u64) {
    unsafe { syscall(SYS_SLEEP, ticks, 0, 0) };
}

/// Lives in .bss: every process must start with it zeroed and see only its own copy.
static mut SHARED: u64 = 0;

#[no_mangle]
pub extern "C" fn _start(mode: u64) -> ! {
    match mode {
        0 => hello(),
        1 => null_deref(),
        2 => privileged_instruction(),
        3 => bad_syscall_arguments(),
        4 | 5 => isolation(mode),
        6 => syscall_flood(),
        7 => read_kernel_memory(),
        8 => stack_overflow(),
        9 => write_to_code(),
        10 => stdin_reader(),
        11 => console_demo(),
        12 => pipe_writer(),
        13 => pipe_reader(),
        14 => pipe_syscalls(),
        15 => broken_pipe(),
        16 => fs_syscalls(),
        _ => exit(99),
    }
}

// ---- file system calls (paths are NUL-terminated byte strings) -------------------

fn open(path: &[u8], flags: u64) -> i64 {
    unsafe { syscall(SYS_OPEN, path.as_ptr() as u64, flags, 0) }
}

fn lseek(fd: i64, offset: i64, whence: u64) -> i64 {
    unsafe { syscall(SYS_LSEEK, fd as u64, offset as u64, whence) }
}

fn fstat(fd: i64, out: &mut [u64; 5]) -> i64 {
    unsafe { syscall(SYS_FSTAT, fd as u64, out.as_mut_ptr() as u64, 0) }
}

fn stat(path: &[u8], out: &mut [u64; 5]) -> i64 {
    unsafe { syscall(SYS_STAT, path.as_ptr() as u64, out.as_mut_ptr() as u64, 0) }
}

fn path_call(nr: u64, path: &[u8]) -> i64 {
    unsafe { syscall(nr, path.as_ptr() as u64, 0, 0) }
}

/// Counts failed checks and says which ones failed.
struct Checks {
    failed: u32,
}

impl Checks {
    fn ok(&mut self, what: &str, condition: bool) {
        if !condition {
            print("FAIL ");
            print(what);
            print("\n");
            self.failed += 1;
        }
    }

    fn expect(&mut self, what: &str, got: i64, want: i64) {
        if got != want {
            print("FAIL ");
            print(what);
            print(": got ");
            print_signed(got);
            print(", wanted ");
            print_signed(want);
            print("\n");
            self.failed += 1;
        }
    }
}

fn rd(fd: i64, buf: &mut [u8]) -> i64 {
    read(fd as u64, buf.as_mut_ptr() as u64, buf.len() as u64)
}

fn wr(fd: i64, data: &[u8]) -> i64 {
    write(fd as u64, data.as_ptr() as u64, data.len() as u64)
}

fn fs_syscalls() -> ! {
    let mut c = Checks { failed: 0 };
    let mut buf = [0u8; 256];
    let mut st = [0u64; 5];

    // A file the disk builder put on the disk: read, stat, seek.
    let fd = open(b"/hello.txt\0", O_RDONLY);
    c.ok("open /hello.txt", fd >= 3);
    c.expect("read it", rd(fd, &mut buf), 19);
    c.ok("contents", &buf[..19] == b"Hello from NovaFS!\n");
    c.expect("read at end of file", rd(fd, &mut buf), 0);
    c.expect("fstat", fstat(fd, &mut st), 0);
    c.ok("fstat says: regular file of 19 bytes", st[1] == 1 && st[2] == 19);
    c.expect("seek from start", lseek(fd, 6, SEEK_SET), 6);
    c.expect("read after seek", rd(fd, &mut buf[..4]), 4);
    c.ok("seeked contents", &buf[..4] == b"from");
    c.expect("seek from current", lseek(fd, 1, SEEK_CUR), 11);
    c.expect("seek from end", lseek(fd, -3, SEEK_END), 16);
    c.expect("seek before the start", lseek(fd, -100, SEEK_SET), EINVAL);
    c.expect("write to a read-only file", wr(fd, b"x"), EBADF);
    c.expect("close", close(fd as u64), 0);
    c.expect("read from a closed descriptor", rd(fd, &mut buf), EBADF);

    // Ways opening can fail.
    c.expect("open a missing file", open(b"/nope\0", O_RDONLY), ENOENT);
    c.expect("open a directory for writing", open(b"/docs\0", O_WRONLY), EISDIR);
    c.expect("open below a file", open(b"/hello.txt/x\0", O_RDONLY), ENOTDIR);
    c.expect(
        "path pointing into the kernel",
        unsafe { syscall(SYS_OPEN, 0xffff_8000_0000_0000, 0, 0) },
        EFAULT,
    );
    // Path length limit: 255 characters. Built from short components ("/a/a/a...")
    // so only the overall length, not any single name, can be what is rejected.
    let mut at_limit = [0u8; 256];
    for i in 0..127 {
        at_limit[2 * i] = b'/';
        at_limit[2 * i + 1] = b'a';
    }
    at_limit[254] = b'/'; // 255 characters, then the NUL
    c.expect("255-character path is accepted (and not found)", open(&at_limit, O_RDONLY), ENOENT);
    let mut over_limit = [0u8; 257];
    for i in 0..128 {
        over_limit[2 * i] = b'/';
        over_limit[2 * i + 1] = b'a';
    }
    c.expect("256-character path is refused", open(&over_limit, O_RDONLY), ENAMETOOLONG);
    let mut very_long = [0u8; 301];
    for i in 0..150 {
        very_long[2 * i] = b'/';
        very_long[2 * i + 1] = b'a';
    }
    c.expect("300-character path is refused", open(&very_long, O_RDONLY), ENAMETOOLONG);
    let mut long_name = [b'n'; 62];
    long_name[0] = b'/';
    long_name[61] = 0;
    c.expect("name longer than the limit", open(&long_name, O_CREAT | O_RDWR), ENAMETOOLONG);

    // Create, write, seek, read, append, truncate.
    let f = open(b"/tmp/u.txt\0", O_CREAT | O_RDWR);
    c.ok("create /tmp/u.txt", f >= 3);
    c.expect("write", wr(f, b"abc"), 3);
    c.expect("rewind", lseek(f, 0, SEEK_SET), 0);
    c.expect("read back", rd(f, &mut buf), 3);
    c.ok("read-back contents", &buf[..3] == b"abc");
    c.expect(
        "exclusive create of an existing file",
        open(b"/tmp/u.txt\0", O_CREAT | O_EXCL | O_RDWR),
        EEXIST,
    );
    c.expect("close", close(f as u64), 0);
    let a = open(b"/tmp/u.txt\0", O_WRONLY | O_APPEND);
    c.expect("append", wr(a, b"def"), 3);
    c.expect("append again", wr(a, b"g"), 1);
    c.expect("read from a write-only file", rd(a, &mut buf), EBADF);
    c.expect("fstat after append", fstat(a, &mut st), 0);
    c.ok("size is 7", st[2] == 7);
    c.expect("close", close(a as u64), 0);
    let t = open(b"/tmp/u.txt\0", O_RDWR | O_TRUNC);
    c.expect("fstat after truncating open", fstat(t, &mut st), 0);
    c.ok("size is 0", st[2] == 0);
    c.expect("close", close(t as u64), 0);

    // A 100 000-byte file written and read in different chunk sizes.
    let big = open(b"/tmp/big.bin\0", O_CREAT | O_RDWR);
    let mut chunk = [0u8; 4000];
    for part in 0..25usize {
        for (i, byte) in chunk.iter_mut().enumerate() {
            *byte = ((part * 4000 + i) * 7 + 3) as u8;
        }
        c.expect("write 4000 bytes", wr(big, &chunk), 4000);
    }
    c.expect("rewind", lseek(big, 0, SEEK_SET), 0);
    let mut total = 0usize;
    let mut intact = true;
    let mut piece = [0u8; 3000];
    loop {
        let n = rd(big, &mut piece);
        if n <= 0 {
            break;
        }
        for (i, &byte) in piece[..n as usize].iter().enumerate() {
            if byte != ((total + i) * 7 + 3) as u8 {
                intact = false;
            }
        }
        total += n as usize;
    }
    c.ok("100000 bytes came back", total == 100_000);
    c.ok("100000 bytes are intact", intact);
    c.expect("close", close(big as u64), 0);
    c.expect("remove the big file", path_call(SYS_UNLINK, b"/tmp/big.bin\0"), 0);

    // Directories, the working directory, and relative paths.
    c.expect("mkdir", path_call(SYS_MKDIR, b"/tmp/d1\0"), 0);
    c.expect("mkdir again", path_call(SYS_MKDIR, b"/tmp/d1\0"), EEXIST);
    c.expect("mkdir under a missing directory", path_call(SYS_MKDIR, b"/nonexistent/x\0"), ENOENT);
    c.expect("chdir", path_call(SYS_CHDIR, b"/tmp/d1\0"), 0);
    let n = unsafe { syscall(SYS_GETCWD, buf.as_mut_ptr() as u64, buf.len() as u64, 0) };
    c.expect("getcwd length", n, 8);
    c.ok("getcwd is /tmp/d1", &buf[..8] == b"/tmp/d1\0");
    let rel = open(b"f.txt\0", O_CREAT | O_RDWR);
    c.ok("create by relative path", rel >= 3);
    c.expect("write", wr(rel, b"x"), 1);
    c.expect("close", close(rel as u64), 0);
    c.expect("stat absolute", stat(b"/tmp/d1/f.txt\0", &mut st), 0);
    c.ok("size 1", st[2] == 1);
    c.expect("stat relative", stat(b"f.txt\0", &mut st), 0);
    c.expect("stat through ..", stat(b"../d1/f.txt\0", &mut st), 0);
    c.expect("stat ..", stat(b"..\0", &mut st), 0);
    c.ok("'..' is a directory", st[1] == 2);

    let dir = open(b".\0", O_RDONLY);
    c.ok("open the current directory", dir >= 3);
    let mut entry = [0u8; 88];
    let (mut count, mut saw_file) = (0, false);
    while unsafe { syscall(SYS_READDIR, dir as u64, entry.as_mut_ptr() as u64, 0) } == 1 {
        count += 1;
        let len = entry[16] as usize;
        if &entry[24..24 + len] == b"f.txt" {
            saw_file = true;
        }
    }
    c.ok("readdir finds f.txt", saw_file);
    c.expect("readdir sees ., .. and f.txt", count, 3);
    c.expect("read from a directory", rd(dir, &mut buf), EISDIR);
    c.expect("close", close(dir as u64), 0);
    c.expect("rmdir of a non-empty directory", path_call(SYS_RMDIR, b"/tmp/d1\0"), ENOTEMPTY);

    // Unlinking a file that is still open keeps it alive until it is closed.
    let held = open(b"f.txt\0", O_RDONLY);
    c.expect("unlink while open", path_call(SYS_UNLINK, b"f.txt\0"), 0);
    c.expect("it is gone by name", stat(b"f.txt\0", &mut st), ENOENT);
    c.expect("but still readable", rd(held, &mut buf), 1);
    c.ok("with its contents", buf[0] == b'x');
    c.expect("close", close(held as u64), 0);

    c.expect("chdir back to the root", path_call(SYS_CHDIR, b"/\0"), 0);
    let n = unsafe { syscall(SYS_GETCWD, buf.as_mut_ptr() as u64, buf.len() as u64, 0) };
    c.expect("getcwd of the root", n, 2);
    c.expect(
        "getcwd into a buffer that is too small",
        unsafe { syscall(SYS_GETCWD, buf.as_mut_ptr() as u64, 1, 0) },
        ERANGE,
    );
    c.expect("rmdir", path_call(SYS_RMDIR, b"/tmp/d1\0"), 0);
    c.expect("rmdir of a file", path_call(SYS_RMDIR, b"/hello.txt\0"), ENOTDIR);
    c.expect("unlink", path_call(SYS_UNLINK, b"/tmp/u.txt\0"), 0);
    c.expect("unlink again", path_call(SYS_UNLINK, b"/tmp/u.txt\0"), ENOENT);

    let docs = open(b"/docs\0", O_RDONLY);
    let file = open(b"/hello.txt\0", O_RDONLY);
    c.expect(
        "readdir on a plain file",
        unsafe { syscall(SYS_READDIR, file as u64, entry.as_mut_ptr() as u64, 0) },
        ENOTDIR,
    );
    c.expect("close", close(docs as u64), 0);
    c.expect("close", close(file as u64), 0);

    if c.failed == 0 {
        print("file system call checks passed\n");
    }
    exit(c.failed as i64)
}

/// Reads exactly three bytes from stdin; the kernel sends "abc" once this
/// process is blocked in read.
fn stdin_reader() -> ! {
    let mut got = [0u8; 3];
    let mut n = 0;
    while n < 3 {
        let r = read(0, got[n..].as_mut_ptr() as u64, (3 - n) as u64);
        if r <= 0 {
            exit(2);
        }
        n += r as usize;
    }
    if &got == b"abc" {
        print("stdin delivered abc\n");
        exit(0)
    }
    print("stdin delivered the wrong bytes\n");
    exit(1)
}

/// Interactive: echo what the user types, then repeat the whole line.
fn console_demo() -> ! {
    print("type a line and press Enter: ");
    let mut line = [0u8; 128];
    let mut n = 0;
    loop {
        let mut byte = [0u8; 1];
        if read(0, byte.as_mut_ptr() as u64, 1) <= 0 {
            exit(1);
        }
        if byte[0] == b'\n' || byte[0] == b'\r' {
            break;
        }
        write(1, byte.as_ptr() as u64, 1); // echo
        if n < line.len() {
            line[n] = byte[0];
            n += 1;
        }
    }
    print("\nyou typed: ");
    write(1, line.as_ptr() as u64, n as u64);
    print("\n");
    exit(0)
}

fn pattern(index: usize) -> u8 {
    (index % 251) as u8
}

/// Total bytes sent through the pipe between process 12 and process 13.
const PIPE_TOTAL: usize = 50_000;

/// Writes through fd 3 and exits without closing it. Starts late and ends with
/// a pause, so the reader is asleep both when the data starts and when this
/// process exits: only the kernel closing the descriptor at exit can wake it.
fn pipe_writer() -> ! {
    sleep(5);
    let mut chunk = [0u8; 1000];
    let mut sent = 0;
    while sent < PIPE_TOTAL {
        for (i, byte) in chunk.iter_mut().enumerate() {
            *byte = pattern(sent + i);
        }
        let r = write(3, chunk.as_ptr() as u64, chunk.len() as u64);
        if r != chunk.len() as i64 {
            exit(1);
        }
        sent += chunk.len();
    }
    sleep(5); // let the reader drain everything and go back to sleep
    exit(0)
}

/// Reads fd 3 until end of file, checking every byte.
fn pipe_reader() -> ! {
    let mut buf = [0u8; 700];
    let mut received = 0;
    loop {
        let r = read(3, buf.as_mut_ptr() as u64, buf.len() as u64);
        if r < 0 {
            exit(2);
        }
        if r == 0 {
            break; // end of file: the writer has exited
        }
        for (i, &byte) in buf[..r as usize].iter().enumerate() {
            if byte != pattern(received + i) {
                print("pipe data corrupted\n");
                exit(3);
            }
        }
        received += r as usize;
    }
    if received == PIPE_TOTAL {
        print("pipe delivered all bytes in order\n");
        exit(0)
    }
    print("pipe lost bytes\n");
    exit(1)
}

/// Descriptors and pipes used entirely inside one process.
fn pipe_syscalls() -> ! {
    let mut fds = [0u64; 2];
    let made = unsafe { syscall(SYS_PIPE, fds.as_mut_ptr() as u64, 0, 0) };
    let (rd, wr) = (fds[0], fds[1]);
    let mut buf = [0u8; 8];

    let wrote = write(wr, b"hello".as_ptr() as u64, 5);
    let got = read(rd, buf.as_mut_ptr() as u64, 8);
    let same = &buf[..5] == b"hello";
    let closed_writer = close(wr);
    let eof = read(rd, buf.as_mut_ptr() as u64, 8); // empty and no writer: EOF
    let closed_reader = close(rd);
    let closed_twice = close(rd);
    let bad_fd = read(99, buf.as_mut_ptr() as u64, 1);
    let write_to_read_end = write(rd, b"x".as_ptr() as u64, 1); // rd is closed now
    let bad_ptr = unsafe { syscall(SYS_PIPE, 0x10, 0, 0) };

    // The destination of a read must be writable: the code segment is not.
    let mut second = [0u64; 2];
    unsafe { syscall(SYS_PIPE, second.as_mut_ptr() as u64, 0, 0) };
    write(second[1], b"y".as_ptr() as u64, 1);
    let into_code = read(second[0], black_box(_start as *const () as usize) as u64, 1);
    // The byte must still be there: a rejected read consumes nothing.
    let still_there = read(second[0], buf.as_mut_ptr() as u64, 1);

    let ok = made == 0
        && wrote == 5
        && got == 5
        && same
        && closed_writer == 0
        && eof == 0
        && closed_reader == 0
        && closed_twice == EBADF
        && bad_fd == EBADF
        && write_to_read_end == EBADF
        && bad_ptr == EFAULT
        && into_code == EFAULT
        && still_there == 1;
    if ok {
        print("pipe syscall checks passed\n");
        exit(0)
    }
    print("pipe syscall checks FAILED: ");
    for r in [
        made, wrote, got, closed_writer, eof, closed_reader, closed_twice, bad_fd,
        write_to_read_end, bad_ptr, into_code, still_there,
    ] {
        print_signed(r);
        print(" ");
    }
    print("\n");
    exit(1)
}

/// fd 3 is a pipe whose read end no longer exists anywhere.
fn broken_pipe() -> ! {
    let r = write(3, b"anyone?".as_ptr() as u64, 7);
    if r == EPIPE {
        print("broken pipe reported\n");
        exit(0)
    }
    print("write to a dead pipe returned ");
    print_signed(r);
    print("\n");
    exit(1)
}

fn hello() -> ! {
    if unsafe { read_volatile(addr_of_mut!(SHARED)) } != 0 {
        exit(2); // .bss was not zeroed
    }
    print("hello from user mode (pid ");
    print_num(getpid());
    print(")\n");
    exit(7)
}

fn null_deref() -> ! {
    print("about to read address 0\n");
    let value = unsafe { read_volatile(black_box(0usize) as *const u8) };
    print_num(value as u64); // not reached
    exit(0)
}

fn privileged_instruction() -> ! {
    print("about to execute hlt in ring 3\n");
    unsafe { asm!("hlt") };
    exit(0)
}

fn bad_syscall_arguments() -> ! {
    let msg = b"mode3 ";
    let msg_ptr = msg.as_ptr() as u64;
    let kernel = write(1, 0xffff_8000_0000_1000, 16); // kernel memory
    let null_page = write(1, 8, 16); // unmapped
    let too_long = write(1, msg_ptr, 1 << 40);
    let off_stack_top = write(1, STACK_TOP - 4, 16); // runs into the unmapped page above
    let empty = write(1, msg_ptr, 0);
    let unknown = unsafe { syscall(9999, 0, 0, 0) };
    let bad_fd = write(7, msg_ptr, 1);

    let ok = kernel == EFAULT
        && null_page == EFAULT
        && too_long == EINVAL
        && off_stack_top == EFAULT
        && empty == 0
        && unknown == ENOSYS
        && bad_fd == EBADF;
    if ok {
        print("syscall argument checks passed\n");
        exit(0)
    }
    print("syscall argument checks FAILED: ");
    for r in [kernel, null_page, too_long, off_stack_top, empty, unknown, bad_fd] {
        print_signed(r);
        print(" ");
    }
    print("\n");
    exit(1)
}

fn isolation(mode: u64) -> ! {
    let shared = addr_of_mut!(SHARED);
    if unsafe { read_volatile(shared) } != 0 {
        exit(2);
    }
    let mine = 0x1111_0000 + mode;
    unsafe { write_volatile(shared, mine) };
    sleep(10); // the other copy of this program writes its own value meanwhile
    if unsafe { read_volatile(shared) } == mine {
        print("isolation ok\n");
        exit(0)
    }
    print("isolation BROKEN\n");
    exit(1)
}

fn syscall_flood() -> ! {
    let pid = getpid();
    let mut last_tick = unsafe { syscall(SYS_TICKS, 0, 0, 0) };
    for round in 0..20_000u64 {
        if getpid() != pid {
            exit(1);
        }
        if round % 1000 == 0 {
            unsafe { syscall(SYS_YIELD, 0, 0, 0) };
            sleep(1);
            let now = unsafe { syscall(SYS_TICKS, 0, 0, 0) };
            if now < last_tick {
                exit(2);
            }
            last_tick = now;
        }
    }
    print("20000 system calls ok\n");
    exit(0)
}

fn read_kernel_memory() -> ! {
    print("about to read kernel memory\n");
    let value = unsafe { read_volatile(black_box(0xffff_8000_0000_0000usize) as *const u8) };
    print_num(value as u64); // not reached
    exit(0)
}

#[inline(never)]
#[allow(unconditional_recursion)] // the point is to run off the end of the stack
fn recurse(depth: u64) -> u64 {
    let buf = [depth as u8; 512];
    black_box(&buf);
    let below = recurse(depth + 1);
    black_box(&buf);
    below + buf[0] as u64
}

fn stack_overflow() -> ! {
    print("about to overflow the stack\n");
    let n = recurse(0);
    print_num(n); // not reached
    exit(0)
}

fn write_to_code() -> ! {
    print("about to write to the code segment\n");
    let code = black_box(_start as *const () as usize) as *mut u8;
    unsafe { write_volatile(code, 0) };
    exit(0)
}

#[panic_handler]
fn panic(_info: &PanicInfo) -> ! {
    print("user panic\n");
    exit(100)
}
