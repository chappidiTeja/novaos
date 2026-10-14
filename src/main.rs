#![no_std]
#![no_main]
#![feature(abi_x86_interrupt)]

extern crate alloc;

mod elf;
mod gdt;
mod idt;
mod ipc;
mod keyboard;
mod limine;
mod mem;
mod pic;
mod port;
mod proc;
mod sched;
mod serial;
mod sync;
mod syscall;
mod timer;

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
    println!("Hello from NovaOS!");

    gdt::init();
    println!("[ok] GDT + TSS loaded");

    idt::init();
    println!("[ok] IDT loaded");

    pic::init();
    timer::init(TIMER_HZ);
    println!("[ok] PIC remapped, PIT at {} Hz", TIMER_HZ);

    mem::init();
    mem::self_test();

    unsafe { asm!("sti", options(nomem, nostack)) };

    unsafe { asm!("int3", options(nomem, nostack)) };

    while timer::ticks() < TIMER_HZ as u64 {
        wait_for_interrupt();
    }
    println!("[ok] timer: {} ticks (~1 second)", timer::ticks());

    sched::init();
    println!("[ok] scheduler: main + idle threads");
    sched::self_test();
    ipc::selftest::self_test();

    syscall::init();
    println!("[ok] syscall interface enabled");
    proc::self_test();

    // Interactive demo: reads a line from the keyboard or serial port.
    proc::spawn_console_demo();

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
