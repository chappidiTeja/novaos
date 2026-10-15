//! NovaOS network stack: Ethernet, ARP, IPv4, ICMP, UDP, DHCP, DNS (and TCP in `tcp`).
//! `no_std` (needs `alloc`); the kernel supplies frames and a clock.

#![cfg_attr(not(test), no_std)]

extern crate alloc;

pub mod dhcp;
pub mod dns;
pub mod stack;
pub mod tcp;
pub mod wire;

#[cfg(test)]
mod tests;

pub use stack::{Config, Datagram, NetError, PingReply, Stack};
pub use wire::{Ip, Mac};
