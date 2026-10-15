//! Wire formats: building and parsing packets. Pure functions on byte slices;
//! nothing here trusts its input (every length is checked before it is used).

use alloc::vec::Vec;

pub type Mac = [u8; 6];
pub type Ip = [u8; 4];

pub const BROADCAST_MAC: Mac = [0xFF; 6];
pub const ZERO_IP: Ip = [0; 4];
pub const BROADCAST_IP: Ip = [255; 4];

pub const ETHERTYPE_IPV4: u16 = 0x0800;
pub const ETHERTYPE_ARP: u16 = 0x0806;
pub const PROTO_ICMP: u8 = 1;
pub const PROTO_TCP: u8 = 6;
pub const PROTO_UDP: u8 = 17;

pub const MTU: usize = 1500;
pub const ETH_HEADER: usize = 14;
pub const ETH_MIN_FRAME: usize = 60;
pub const IP_HEADER: usize = 20;
pub const UDP_HEADER: usize = 8;
pub const MAX_UDP_PAYLOAD: usize = MTU - IP_HEADER - UDP_HEADER;

// ---- Internet checksum (RFC 1071) -------------------------------------------

/// Add `data` (as big-endian 16-bit words, a trailing odd byte padded with zero) to `acc`.
pub fn sum(data: &[u8], acc: u64) -> u64 {
    let mut s = acc;
    let mut chunks = data.chunks_exact(2);
    for c in &mut chunks {
        s += u16::from_be_bytes([c[0], c[1]]) as u64;
    }
    if let [last] = chunks.remainder() {
        s += (*last as u64) << 8;
    }
    s
}

/// Fold carries and complement.
pub fn fold(mut s: u64) -> u16 {
    while s >> 16 != 0 {
        s = (s & 0xFFFF) + (s >> 16);
    }
    !(s as u16)
}

pub fn checksum(data: &[u8]) -> u16 {
    fold(sum(data, 0))
}

/// Checksum of a TCP/UDP segment including the IPv4 pseudo-header.
pub fn transport_checksum(src: Ip, dst: Ip, proto: u8, segment: &[u8]) -> u16 {
    let mut s = sum(&src, 0);
    s = sum(&dst, s);
    s += proto as u64;
    s += segment.len() as u64;
    fold(sum(segment, s))
}

// ---- Ethernet ---------------------------------------------------------------

pub struct EthFrame<'a> {
    pub dst: Mac,
    pub src: Mac,
    pub ethertype: u16,
    pub payload: &'a [u8],
}

pub fn parse_eth(b: &[u8]) -> Option<EthFrame<'_>> {
    if b.len() < ETH_HEADER {
        return None;
    }
    Some(EthFrame {
        dst: b[0..6].try_into().ok()?,
        src: b[6..12].try_into().ok()?,
        ethertype: u16::from_be_bytes([b[12], b[13]]),
        payload: &b[ETH_HEADER..],
    })
}

/// A frame padded with zeros to the 60-byte Ethernet minimum.
pub fn build_eth(dst: Mac, src: Mac, ethertype: u16, payload: &[u8]) -> Vec<u8> {
    let mut f = Vec::with_capacity(ETH_HEADER + payload.len().max(ETH_MIN_FRAME - ETH_HEADER));
    f.extend_from_slice(&dst);
    f.extend_from_slice(&src);
    f.extend_from_slice(&ethertype.to_be_bytes());
    f.extend_from_slice(payload);
    if f.len() < ETH_MIN_FRAME {
        f.resize(ETH_MIN_FRAME, 0);
    }
    f
}

// ---- ARP --------------------------------------------------------------------

pub const ARP_REQUEST: u16 = 1;
pub const ARP_REPLY: u16 = 2;
pub const ARP_LEN: usize = 28;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Arp {
    pub op: u16,
    pub sha: Mac,
    pub spa: Ip,
    pub tha: Mac,
    pub tpa: Ip,
}

pub fn parse_arp(b: &[u8]) -> Option<Arp> {
    if b.len() < ARP_LEN {
        return None;
    }
    let htype = u16::from_be_bytes([b[0], b[1]]);
    let ptype = u16::from_be_bytes([b[2], b[3]]);
    if htype != 1 || ptype != ETHERTYPE_IPV4 || b[4] != 6 || b[5] != 4 {
        return None;
    }
    let op = u16::from_be_bytes([b[6], b[7]]);
    if op != ARP_REQUEST && op != ARP_REPLY {
        return None;
    }
    Some(Arp {
        op,
        sha: b[8..14].try_into().ok()?,
        spa: b[14..18].try_into().ok()?,
        tha: b[18..24].try_into().ok()?,
        tpa: b[24..28].try_into().ok()?,
    })
}

pub fn build_arp(a: &Arp) -> [u8; ARP_LEN] {
    let mut b = [0u8; ARP_LEN];
    b[0..2].copy_from_slice(&1u16.to_be_bytes());
    b[2..4].copy_from_slice(&ETHERTYPE_IPV4.to_be_bytes());
    b[4] = 6;
    b[5] = 4;
    b[6..8].copy_from_slice(&a.op.to_be_bytes());
    b[8..14].copy_from_slice(&a.sha);
    b[14..18].copy_from_slice(&a.spa);
    b[18..24].copy_from_slice(&a.tha);
    b[24..28].copy_from_slice(&a.tpa);
    b
}

// ---- IPv4 -------------------------------------------------------------------

#[derive(Debug, PartialEq, Eq)]
pub enum IpError {
    Short,
    BadVersion,
    BadHeader,
    BadChecksum,
    /// A fragment: this stack does not reassemble.
    Fragment,
}

pub struct Ipv4<'a> {
    pub src: Ip,
    pub dst: Ip,
    pub proto: u8,
    pub ttl: u8,
    pub id: u16,
    /// The whole IP header (used to build ICMP errors).
    pub header: &'a [u8],
    pub payload: &'a [u8],
}

pub fn parse_ipv4(b: &[u8]) -> Result<Ipv4<'_>, IpError> {
    if b.len() < IP_HEADER {
        return Err(IpError::Short);
    }
    if b[0] >> 4 != 4 {
        return Err(IpError::BadVersion);
    }
    let header_len = ((b[0] & 0x0F) as usize) * 4;
    let total = u16::from_be_bytes([b[2], b[3]]) as usize;
    if header_len < IP_HEADER || total < header_len || total > b.len() {
        return Err(IpError::BadHeader);
    }
    if checksum(&b[..header_len]) != 0 {
        return Err(IpError::BadChecksum);
    }
    let frag = u16::from_be_bytes([b[6], b[7]]);
    if frag & 0x3FFF != 0 {
        // More-fragments flag or a nonzero offset.
        return Err(IpError::Fragment);
    }
    Ok(Ipv4 {
        src: b[12..16].try_into().unwrap(),
        dst: b[16..20].try_into().unwrap(),
        proto: b[9],
        ttl: b[8],
        id: u16::from_be_bytes([b[4], b[5]]),
        header: &b[..header_len],
        payload: &b[header_len..total],
    })
}

/// An IPv4 packet with the Don't-Fragment flag set and no options.
pub fn build_ipv4(src: Ip, dst: Ip, proto: u8, id: u16, ttl: u8, payload: &[u8]) -> Vec<u8> {
    let total = IP_HEADER + payload.len();
    let mut p = Vec::with_capacity(total);
    p.push(0x45);
    p.push(0);
    p.extend_from_slice(&(total as u16).to_be_bytes());
    p.extend_from_slice(&id.to_be_bytes());
    p.extend_from_slice(&0x4000u16.to_be_bytes());
    p.push(ttl);
    p.push(proto);
    p.extend_from_slice(&[0, 0]);
    p.extend_from_slice(&src);
    p.extend_from_slice(&dst);
    let c = checksum(&p[..IP_HEADER]);
    p[10..12].copy_from_slice(&c.to_be_bytes());
    p.extend_from_slice(payload);
    p
}

// ---- ICMP -------------------------------------------------------------------

pub const ICMP_ECHO_REPLY: u8 = 0;
pub const ICMP_UNREACHABLE: u8 = 3;
pub const ICMP_ECHO_REQUEST: u8 = 8;
pub const ICMP_PORT_UNREACHABLE: u8 = 3;

pub struct Icmp<'a> {
    pub kind: u8,
    pub code: u8,
    pub rest: [u8; 4],
    pub data: &'a [u8],
}

pub fn parse_icmp(b: &[u8]) -> Option<Icmp<'_>> {
    if b.len() < 8 || checksum(b) != 0 {
        return None;
    }
    Some(Icmp { kind: b[0], code: b[1], rest: b[4..8].try_into().ok()?, data: &b[8..] })
}

pub fn build_icmp(kind: u8, code: u8, rest: [u8; 4], data: &[u8]) -> Vec<u8> {
    let mut p = Vec::with_capacity(8 + data.len());
    p.push(kind);
    p.push(code);
    p.extend_from_slice(&[0, 0]);
    p.extend_from_slice(&rest);
    p.extend_from_slice(data);
    let c = checksum(&p);
    p[2..4].copy_from_slice(&c.to_be_bytes());
    p
}

pub fn echo_rest(id: u16, seq: u16) -> [u8; 4] {
    let mut r = [0u8; 4];
    r[0..2].copy_from_slice(&id.to_be_bytes());
    r[2..4].copy_from_slice(&seq.to_be_bytes());
    r
}

// ---- UDP --------------------------------------------------------------------

pub struct Udp<'a> {
    pub sport: u16,
    pub dport: u16,
    pub payload: &'a [u8],
}

/// `src`/`dst` are the IP addresses from the enclosing packet (for the checksum).
pub fn parse_udp(src: Ip, dst: Ip, b: &[u8]) -> Option<Udp<'_>> {
    if b.len() < UDP_HEADER {
        return None;
    }
    let len = u16::from_be_bytes([b[4], b[5]]) as usize;
    if len < UDP_HEADER || len > b.len() {
        return None;
    }
    let seg = &b[..len];
    let sent = u16::from_be_bytes([b[6], b[7]]);
    // A zero checksum means "none" in IPv4.
    if sent != 0 && transport_checksum(src, dst, PROTO_UDP, seg) != 0 {
        return None;
    }
    Some(Udp {
        sport: u16::from_be_bytes([b[0], b[1]]),
        dport: u16::from_be_bytes([b[2], b[3]]),
        payload: &seg[UDP_HEADER..],
    })
}

pub fn build_udp(src: Ip, dst: Ip, sport: u16, dport: u16, payload: &[u8]) -> Vec<u8> {
    let len = UDP_HEADER + payload.len();
    let mut p = Vec::with_capacity(len);
    p.extend_from_slice(&sport.to_be_bytes());
    p.extend_from_slice(&dport.to_be_bytes());
    p.extend_from_slice(&(len as u16).to_be_bytes());
    p.extend_from_slice(&[0, 0]);
    p.extend_from_slice(payload);
    let mut c = transport_checksum(src, dst, PROTO_UDP, &p);
    if c == 0 {
        c = 0xFFFF;
    }
    p[6..8].copy_from_slice(&c.to_be_bytes());
    p
}
