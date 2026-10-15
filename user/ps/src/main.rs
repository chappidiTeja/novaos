//! ps: list the processes (pid, parent, state, CPU time, memory, name).

#![no_std]
#![no_main]

use nova::*;

fn state_letter(p: &ProcInfo) -> &'static str {
    match p.state {
        PROC_RUNNING => "R",
        PROC_WAITING => "S",
        PROC_ZOMBIE => "Z",
        _ => "?",
    }
}

fn main(_args: &Args) -> i32 {
    let mut list = [ProcInfo::EMPTY; 128];
    let n = proclist(&mut list);
    if n < 0 {
        report("ps", "process list", n);
        return 1;
    }
    let now = ticks();
    println!("  PID  PPID S    CPU   AGE   MEM NAME");
    for p in &list[..n as usize] {
        let cpu = p.cpu_ticks;
        let age = now.saturating_sub(p.start_tick);
        println!(
            "{:5} {:5} {} {:3}.{:02} {:3}.{:02} {:4}K {}",
            p.pid,
            p.ppid,
            state_letter(p),
            cpu / 100,
            cpu % 100,
            age / 100,
            age % 100,
            p.mem_pages * 4,
            p.name()
        );
    }
    0
}

entry!(main);
