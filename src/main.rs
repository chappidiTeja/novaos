#![no_std]
#![no_main]
#![feature(abi_x86_interrupt)]

extern crate alloc;

mod gdt;
mod idt;
mod keyboard;
mod limine;
mod mem;
mod pic;
mod port;
mod serial;
mod sync;
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

    println!("Type on the keyboard:");
    loop {
        wait_for_interrupt();
    }
}

#[panic_handler]
fn panic(info: &PanicInfo) -> ! {
    println!("KERNEL PANIC: {}", info);
    halt_forever()
}
