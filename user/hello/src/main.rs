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
        _ => exit(99),
    }
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
