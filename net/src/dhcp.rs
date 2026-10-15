//! DHCP (RFC 2131) message building and parsing. The state machine that uses
//! these lives in `stack.rs`.

use crate::wire::{Ip, Mac};
use alloc::vec::Vec;

pub const SERVER_PORT: u16 = 67;
pub const CLIENT_PORT: u16 = 68;

const COOKIE: [u8; 4] = [0x63, 0x82, 0x53, 0x63];
const HEADER: usize = 236;

const OPT_MASK: u8 = 1;
const OPT_ROUTER: u8 = 3;
const OPT_DNS: u8 = 6;
const OPT_REQUESTED_IP: u8 = 50;
const OPT_LEASE: u8 = 51;
const OPT_MSG_TYPE: u8 = 53;
const OPT_SERVER_ID: u8 = 54;
const OPT_PARAM_LIST: u8 = 55;
const OPT_CLIENT_ID: u8 = 61;
const OPT_END: u8 = 255;
const OPT_PAD: u8 = 0;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MsgType {
    Discover,
    Offer,
    Request,
    Decline,
    Ack,
    Nak,
    Release,
}

impl MsgType {
    fn code(self) -> u8 {
        match self {
            MsgType::Discover => 1,
            MsgType::Offer => 2,
            MsgType::Request => 3,
            MsgType::Decline => 4,
            MsgType::Ack => 5,
            MsgType::Nak => 6,
            MsgType::Release => 7,
        }
    }
    fn from_code(c: u8) -> Option<MsgType> {
        Some(match c {
            1 => MsgType::Discover,
            2 => MsgType::Offer,
            3 => MsgType::Request,
            4 => MsgType::Decline,
            5 => MsgType::Ack,
            6 => MsgType::Nak,
            7 => MsgType::Release,
            _ => return None,
        })
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Reply {
    pub msg: MsgType,
    pub xid: u32,
    pub yiaddr: Ip,
    pub chaddr: Mac,
    pub mask: Option<Ip>,
    pub router: Option<Ip>,
    pub dns: Option<Ip>,
    pub server: Option<Ip>,
    pub lease_secs: Option<u32>,
}

fn base(mac: Mac, xid: u32, msg: MsgType) -> Vec<u8> {
    let mut p = alloc::vec![0u8; HEADER];
    p[0] = 1; // BOOTREQUEST
    p[1] = 1; // Ethernet
    p[2] = 6;
    p[4..8].copy_from_slice(&xid.to_be_bytes());
    // Ask for the reply to be broadcast: we have no address to receive a unicast on yet.
    p[10..12].copy_from_slice(&0x8000u16.to_be_bytes());
    p[28..34].copy_from_slice(&mac);
    p.extend_from_slice(&COOKIE);
    p.extend_from_slice(&[OPT_MSG_TYPE, 1, msg.code()]);
    p.extend_from_slice(&[OPT_CLIENT_ID, 7, 1]);
    p.extend_from_slice(&mac);
    p
}

fn finish(mut p: Vec<u8>) -> Vec<u8> {
    p.push(OPT_END);
    // Some servers insist on the classic 300-byte minimum.
    if p.len() < 300 {
        p.resize(300, 0);
    }
    p
}

pub fn build_discover(mac: Mac, xid: u32) -> Vec<u8> {
    let mut p = base(mac, xid, MsgType::Discover);
    p.extend_from_slice(&[OPT_PARAM_LIST, 4, OPT_MASK, OPT_ROUTER, OPT_DNS, OPT_LEASE]);
    finish(p)
}

/// A REQUEST for `requested` from `server` (both for the first request and for renewals).
pub fn build_request(mac: Mac, xid: u32, requested: Ip, server: Ip) -> Vec<u8> {
    let mut p = base(mac, xid, MsgType::Request);
    p.extend_from_slice(&[OPT_REQUESTED_IP, 4]);
    p.extend_from_slice(&requested);
    p.extend_from_slice(&[OPT_SERVER_ID, 4]);
    p.extend_from_slice(&server);
    p.extend_from_slice(&[OPT_PARAM_LIST, 4, OPT_MASK, OPT_ROUTER, OPT_DNS, OPT_LEASE]);
    finish(p)
}

/// Parse a server's reply (OFFER, ACK, NAK). Anything malformed is `None`.
pub fn parse_reply(b: &[u8]) -> Option<Reply> {
    if b.len() < HEADER + 4 || b[0] != 2 || b[1] != 1 || b[2] != 6 {
        return None;
    }
    if b[HEADER..HEADER + 4] != COOKIE {
        return None;
    }
    let mut r = Reply {
        msg: MsgType::Discover,
        xid: u32::from_be_bytes(b[4..8].try_into().ok()?),
        yiaddr: b[16..20].try_into().ok()?,
        chaddr: b[28..34].try_into().ok()?,
        mask: None,
        router: None,
        dns: None,
        server: None,
        lease_secs: None,
    };
    let mut have_type = false;
    let opts = &b[HEADER + 4..];
    let mut i = 0;
    while i < opts.len() {
        let code = opts[i];
        if code == OPT_END {
            break;
        }
        if code == OPT_PAD {
            i += 1;
            continue;
        }
        let len = *opts.get(i + 1)? as usize;
        let val = opts.get(i + 2..i + 2 + len)?;
        let ip = |v: &[u8]| -> Option<Ip> { v.get(..4)?.try_into().ok() };
        match code {
            OPT_MSG_TYPE if len == 1 => {
                r.msg = MsgType::from_code(val[0])?;
                have_type = true;
            }
            OPT_MASK => r.mask = ip(val),
            OPT_ROUTER => r.router = ip(val),
            OPT_DNS => r.dns = ip(val),
            OPT_SERVER_ID => r.server = ip(val),
            OPT_LEASE if len == 4 => r.lease_secs = Some(u32::from_be_bytes(val.try_into().ok()?)),
            _ => {}
        }
        i += 2 + len;
    }
    if have_type {
        Some(r)
    } else {
        None
    }
}
