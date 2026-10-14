use core::arch::asm;
use core::mem::size_of;
use core::ptr::{addr_of, addr_of_mut};

use crate::gdt::{DOUBLE_FAULT_IST, KERNEL_CS};
use crate::{keyboard, pic, println, sched, timer};

pub const IRQ_TIMER: u8 = pic::IRQ_BASE;
pub const IRQ_KEYBOARD: u8 = pic::IRQ_BASE + 1;
pub const IRQ_SERIAL: u8 = pic::IRQ_BASE + 4;

#[repr(C)]
#[derive(Clone, Copy)]
struct Entry {
    offset_low: u16,
    selector: u16,
    ist: u8,
    attr: u8,
    offset_mid: u16,
    offset_high: u32,
    zero: u32,
}

impl Entry {
    const MISSING: Entry = Entry {
        offset_low: 0,
        selector: 0,
        ist: 0,
        attr: 0,
        offset_mid: 0,
        offset_high: 0,
        zero: 0,
    };

    fn new(handler: usize, ist: u8) -> Entry {
        Entry {
            offset_low: handler as u16,
            selector: KERNEL_CS,
            ist,
            attr: 0x8E, // present, ring 0, interrupt gate
            offset_mid: (handler >> 16) as u16,
            offset_high: (handler >> 32) as u32,
            zero: 0,
        }
    }
}

/// What the CPU pushes before entering a handler.
#[repr(C)]
pub struct InterruptStackFrame {
    pub rip: u64,
    pub cs: u64,
    pub rflags: u64,
    pub rsp: u64,
    pub ss: u64,
}

#[repr(C, packed)]
struct DescriptorPointer {
    limit: u16,
    base: u64,
}

static mut IDT: [Entry; 256] = [Entry::MISSING; 256];

fn set(vector: u8, handler: usize, ist: u8) {
    unsafe { (*addr_of_mut!(IDT))[vector as usize] = Entry::new(handler, ist) };
}

pub fn init() {
    set(0, divide_error as *const () as usize, 0);
    set(3, breakpoint as *const () as usize, 0);
    set(6, invalid_opcode as *const () as usize, 0);
    set(8, double_fault as *const () as usize, DOUBLE_FAULT_IST);
    set(13, general_protection as *const () as usize, 0);
    set(14, page_fault as *const () as usize, 0);
    set(IRQ_TIMER, timer_irq as *const () as usize, 0);
    set(IRQ_KEYBOARD, keyboard_irq as *const () as usize, 0);
    set(IRQ_SERIAL, serial_irq as *const () as usize, 0);

    let ptr = DescriptorPointer {
        limit: (size_of::<[Entry; 256]>() - 1) as u16,
        base: addr_of!(IDT) as u64,
    };
    unsafe {
        asm!("lidt [{}]", in(reg) addr_of!(ptr), options(readonly, nostack, preserves_flags));
    }
}

/// An exception we cannot recover from. If it came from ring 3 only that process
/// is terminated; if it came from the kernel itself, the machine stops.
fn fatal(name: &str, vector: u8, frame: &InterruptStackFrame, error_code: Option<u64>) -> ! {
    if frame.cs & 3 == 3 {
        crate::proc::kill_current(name, vector, frame.rip);
    }
    crate::serial::force_unlock();
    println!("\n*** EXCEPTION: {} ***", name);
    if let Some(code) = error_code {
        println!("error code: {:#x}", code);
    }
    println!(
        "rip={:#x} cs={:#x} rflags={:#x} rsp={:#x} ss={:#x}",
        frame.rip, frame.cs, frame.rflags, frame.rsp, frame.ss
    );
    crate::halt_forever()
}

extern "x86-interrupt" fn divide_error(frame: InterruptStackFrame) {
    fatal("DIVIDE ERROR (#DE)", 0, &frame, None)
}

extern "x86-interrupt" fn breakpoint(frame: InterruptStackFrame) {
    println!("EXCEPTION: BREAKPOINT at {:#x} (recovered)", frame.rip);
}

extern "x86-interrupt" fn invalid_opcode(frame: InterruptStackFrame) {
    fatal("INVALID OPCODE (#UD)", 6, &frame, None)
}

extern "x86-interrupt" fn double_fault(frame: InterruptStackFrame, code: u64) -> ! {
    // Runs on its own IST stack and is never attributed to a process.
    crate::serial::force_unlock();
    println!("\n*** EXCEPTION: DOUBLE FAULT (#DF) ***");
    println!("error code: {:#x}", code);
    println!(
        "rip={:#x} cs={:#x} rflags={:#x} rsp={:#x} ss={:#x}",
        frame.rip, frame.cs, frame.rflags, frame.rsp, frame.ss
    );
    crate::halt_forever()
}

extern "x86-interrupt" fn general_protection(frame: InterruptStackFrame, code: u64) {
    fatal("GENERAL PROTECTION FAULT (#GP)", 13, &frame, Some(code))
}

extern "x86-interrupt" fn page_fault(frame: InterruptStackFrame, code: u64) {
    let cr2: u64;
    unsafe { asm!("mov {}, cr2", out(reg) cr2, options(nomem, nostack, preserves_flags)) };
    if frame.cs & 3 != 3 {
        println!("faulting address (CR2): {:#x}", cr2);
    }
    fatal("PAGE FAULT (#PF)", 14, &frame, Some(code))
}

extern "x86-interrupt" fn timer_irq(_frame: InterruptStackFrame) {
    timer::tick();
    // Acknowledge first: the scheduler below may switch to another thread and
    // not come back for a while, and the PIC sends no more timer IRQs until EOI.
    pic::end_of_interrupt(IRQ_TIMER);
    sched::on_tick();
}

extern "x86-interrupt" fn keyboard_irq(_frame: InterruptStackFrame) {
    keyboard::handle_interrupt();
    pic::end_of_interrupt(IRQ_KEYBOARD);
}

extern "x86-interrupt" fn serial_irq(_frame: InterruptStackFrame) {
    // The UART has a FIFO: drain every byte that is waiting.
    while let Some(byte) = crate::serial::try_read() {
        crate::ipc::console::push_input(byte);
    }
    pic::end_of_interrupt(IRQ_SERIAL);
}
