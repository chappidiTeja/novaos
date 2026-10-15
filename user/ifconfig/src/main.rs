//! ifconfig: show the network card's state, address and traffic counters.

#![no_std]
#![no_main]

use nova::*;

fn main(_args: &Args) -> i32 {
    let mut info = NetInfo::default();
    let r = netinfo(&mut info);
    if r < 0 {
        eprintln!("ifconfig: {}", strerror(r));
        return 1;
    }
    println!("eth0: link {}", if info.flags & NET_LINK_UP != 0 { "up" } else { "down" });
    let m = info.mac;
    println!("  mac      {:02x}:{:02x}:{:02x}:{:02x}:{:02x}:{:02x}", m[0], m[1], m[2], m[3], m[4], m[5]);
    if info.flags & NET_HAS_ADDRESS != 0 {
        println!("  address  {}", Ip4(info.ip));
        println!("  netmask  {}", Ip4(info.mask));
        if info.gateway != [0; 4] {
            println!("  gateway  {}", Ip4(info.gateway));
        }
        if info.dns != [0; 4] {
            println!("  dns      {}", Ip4(info.dns));
        }
        if info.flags & NET_DHCP_BOUND != 0 {
            println!("  (configured by DHCP)");
        }
    } else {
        println!("  no address yet");
    }
    println!(
        "  frames   received {}  sent {}  dropped {} (in) {} (out)",
        info.rx_frames, info.tx_frames, info.rx_dropped, info.tx_dropped
    );
    0
}

entry!(main);
