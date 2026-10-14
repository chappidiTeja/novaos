#![no_std]
#![no_main]
#![feature(abi_x86_interrupt)]

extern crate alloc;

mod elf;
mod gdt;
mod gfx;
mod idt;
mod ipc;
mod keyboard;
mod limine;
mod mem;
mod pci;
mod pic;
mod port;
mod proc;
mod sched;
mod serial;
mod sync;
mod syscall;
mod timer;
mod vfs;
mod virtio_blk;

use core::arch::asm;
use core::panic::PanicInfo;

/// Limine base revision tag: tells the bootloader which protocol revision we speak.
#[used]
#[link_section = ".requests"]
static BASE_REVISION: [u64; 3] = [0xf9562b2d5c95a6c8, 0x6a7b384944536bdc, 3];

const TIMER_HZ: u32 = 100;

pub fn halt_forever() -> ! {
    loop {
        unsafe { asm!("cli; hlt", options(nomem, nostack)) };
    }
}

/// Used before the scheduler exists.
fn wait_for_interrupt() {
    unsafe { asm!("hlt", options(nomem, nostack)) };
}


#[no_mangle]
extern "C" fn _start() -> ! {
    serial::init();
    // Bring the screen up first, so everything below shows on it as well as on serial.
    let screen = gfx::console::init();
    println!("Hello from NovaOS!");
    match screen {
        Some((w, h, cols, rows)) => println!("[ok] screen: {}x{} pixels, {}x{} text", w, h, cols, rows),
        None => println!("[gfx] no usable framebuffer; text goes to the serial port only"),
    }

    // The "selftest" boot entry (see limine-test.conf) runs every subsystem's
    // self-test and exercises hostile programs. The normal entry skips them, so
    // it boots straight into the shell without touching the disk's contents.
    let selftest = limine::cmdline_has("selftest");
    if selftest {
        println!("[test] running with self-tests enabled");
    }

    gdt::init();
    println!("[ok] GDT + TSS loaded");

    idt::init();
    println!("[ok] IDT loaded");

    pic::init();
    timer::init(TIMER_HZ);
    println!("[ok] PIC remapped, PIT at {} Hz", TIMER_HZ);

    mem::init();
    if selftest {
        mem::self_test();
    }

    unsafe { asm!("sti", options(nomem, nostack)) };

    if selftest {
        unsafe { asm!("int3", options(nomem, nostack)) };
        while timer::ticks() < TIMER_HZ as u64 {
            wait_for_interrupt();
        }
        println!("[ok] timer: {} ticks (~1 second)", timer::ticks());
    }

    sched::init();
    println!("[ok] scheduler: main + idle threads");
    if selftest {
        sched::self_test();
        ipc::selftest::self_test();
    }

    match vfs::init() {
        Ok(()) => {
            if selftest {
                vfs::self_test();
            }
        }
        Err(reason) => println!("[disk] not available: {}", reason),
    }

    syscall::init();
    println!("[ok] syscall interface enabled");
    if selftest {
        proc::self_test();
        // Repaint everything from the character grid; the screen test then checks
        // that the grid and the picture agree after all the scrolling above.
        gfx::console::redraw_all();
        println!("[ok] screen redrawn from the character grid");
    }

    // Hand the machine to the shell on the disk; without a disk, fall back to a
    // tiny demo that reads one line.
    if proc::spawn_init() {
        println!("[ok] started /bin/sh");
    } else {
        proc::spawn_console_demo();
    }

    // Nothing left for the boot thread to do; let the idle thread have the CPU.
    loop {
        sched::sleep_ticks(1000);
    }
}

#[panic_handler]
fn panic(info: &PanicInfo) -> ! {
    serial::force_unlock();
    println!("KERNEL PANIC: {}", info);
    halt_forever()
}
