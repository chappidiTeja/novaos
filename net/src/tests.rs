use crate::dhcp;
use crate::dns::{self, DnsError};
use crate::stack::{Config, NetError, Stack, UDP_QUEUE_MAX};
use crate::wire::*;
use std::vec::Vec;

fn xorshift(state: &mut u64) -> u64 {
    let mut x = *state;
    x ^= x << 13;
    x ^= x >> 7;
    x ^= x << 17;
    *state = x;
    x
}

// ---------------------------------------------------------------- wire formats

#[test]
fn checksum_known_vector() {
    // The IPv4 header from the Wikipedia checksum example; the correct value is 0xB861.
    let mut h = [0x45, 0x00, 0x00, 0x73, 0x00, 0x00, 0x40, 0x00, 0x40, 0x11, 0x00, 0x00, 0xc0, 0xa8, 0x00, 0x01, 0xc0, 0xa8, 0x00, 0xc7];
    assert_eq!(checksum(&h), 0xB861);
    h[10] = 0xB8;
    h[11] = 0x61;
    assert_eq!(checksum(&h), 0);
}

#[test]
fn checksum_odd_length_and_carries() {
    assert_eq!(checksum(&[]), 0xFFFF);
    assert_eq!(checksum(&[0x12]), !0x1200u16);
    // Many 0xFFFF words: the sum folds repeatedly.
    let data = [0xFFu8; 4000];
    assert_eq!(checksum(&data), 0);
    // Appending the checksum makes the whole thing verify.
    let msg = [1u8, 2, 3, 4, 5, 6, 7];
    let c = checksum(&msg);
    let mut with = msg.to_vec();
    with.push(0); // pad to even so the checksum word lines up
    with.extend_from_slice(&c.to_be_bytes());
    assert_eq!(checksum(&with), 0);
}

#[test]
fn checksums_match_an_independent_implementation() {
    // Expected values come from a separate implementation (Python's struct module), not from
    // this crate: a bug that build and parse share would pass every round-trip test.
    // Two carry folds are needed here: the sum is 0x1FFFF.
    assert_eq!(checksum(&[0xFF, 0xFF, 0xFF, 0xFF, 0x00, 0x01]), 0xFFFE);
    // UDP 192.168.0.1:12345 -> 192.168.0.199:53, payload "hello!!" (odd length).
    let udp = build_udp([192, 168, 0, 1], [192, 168, 0, 199], 12345, 53, b"hello!!");
    assert_eq!(&udp[..], &[0x30, 0x39, 0x00, 0x35, 0x00, 0x0f, 0xe8, 0x55, 0x68, 0x65, 0x6c, 0x6c, 0x6f, 0x21, 0x21]);
    // ICMP echo request, id 0x1234, seq 7, data "abcdef".
    let icmp = build_icmp(ICMP_ECHO_REQUEST, 0, echo_rest(0x1234, 7), b"abcdef");
    assert_eq!(u16::from_be_bytes([icmp[2], icmp[3]]), 0xBB97);
    // IPv4 header 10.0.0.2 -> 10.0.0.3, UDP, id 0x1c46, 15 bytes of payload.
    let ip = build_ipv4([10, 0, 0, 2], [10, 0, 0, 3], PROTO_UDP, 0x1c46, 64, &[0u8; 15]);
    assert_eq!(u16::from_be_bytes([ip[10], ip[11]]), 0x0A80);
}

#[test]
fn ethernet_roundtrip_and_padding() {
    let f = build_eth([1; 6], [2; 6], ETHERTYPE_IPV4, &[9, 9, 9]);
    assert_eq!(f.len(), ETH_MIN_FRAME);
    let p = parse_eth(&f).unwrap();
    assert_eq!((p.dst, p.src, p.ethertype), ([1; 6], [2; 6], ETHERTYPE_IPV4));
    assert_eq!(&p.payload[..3], &[9, 9, 9]);
    let big = build_eth([1; 6], [2; 6], ETHERTYPE_IPV4, &[7u8; 1500]);
    assert_eq!(big.len(), 14 + 1500);
    assert!(parse_eth(&f[..13]).is_none());
}

#[test]
fn arp_roundtrip_and_rejects() {
    let a = Arp { op: ARP_REQUEST, sha: [1; 6], spa: [10, 0, 0, 1], tha: [0; 6], tpa: [10, 0, 0, 2] };
    let b = build_arp(&a);
    assert_eq!(parse_arp(&b), Some(a));
    let mut bad = b;
    bad[1] = 6; // hardware type is not Ethernet
    assert!(parse_arp(&bad).is_none());
    let mut bad = b;
    bad[7] = 9; // unknown operation
    assert!(parse_arp(&bad).is_none());
    assert!(parse_arp(&b[..27]).is_none());
}

#[test]
fn ipv4_roundtrip_and_rejects() {
    let p = build_ipv4([1, 2, 3, 4], [5, 6, 7, 8], PROTO_UDP, 77, 64, b"hello");
    let ip = parse_ipv4(&p).unwrap();
    assert_eq!((ip.src, ip.dst, ip.proto, ip.ttl, ip.id), ([1, 2, 3, 4], [5, 6, 7, 8], PROTO_UDP, 64, 77));
    assert_eq!(ip.payload, b"hello");

    let mut bad = p.clone();
    bad[8] ^= 1; // TTL changed, checksum now wrong
    assert_eq!(parse_ipv4(&bad).err(), Some(IpError::BadChecksum));
    let mut bad = p.clone();
    bad[0] = 0x65;
    assert_eq!(parse_ipv4(&bad).err(), Some(IpError::BadVersion));
    assert_eq!(parse_ipv4(&p[..19]).err(), Some(IpError::Short));
    // Total length larger than what arrived.
    assert_eq!(parse_ipv4(&p[..p.len() - 1]).err(), Some(IpError::BadHeader));
    // Ethernet padding after the packet is ignored.
    let mut padded = p.clone();
    padded.extend_from_slice(&[0; 30]);
    assert_eq!(parse_ipv4(&padded).unwrap().payload, b"hello");
    // A fragment (more-fragments flag) is refused; recompute the checksum so only that is wrong.
    let mut frag = p.clone();
    frag[6] = 0x20;
    frag[7] = 0;
    frag[10] = 0;
    frag[11] = 0;
    let c = checksum(&frag[..20]);
    frag[10..12].copy_from_slice(&c.to_be_bytes());
    assert_eq!(parse_ipv4(&frag).err(), Some(IpError::Fragment));
    // Header length field below 5 words.
    let mut small = p.clone();
    small[0] = 0x44;
    assert_eq!(parse_ipv4(&small).err(), Some(IpError::BadHeader));
}

#[test]
fn ipv4_header_with_options() {
    // IHL = 6: a 24-byte header; the payload must start after the options.
    let mut p = std::vec![0x46, 0, 0, 28, 0, 1, 0, 0, 64, PROTO_UDP, 0, 0, 1, 1, 1, 1, 2, 2, 2, 2, 1, 1, 1, 1, 0xAA, 0xBB, 0xCC, 0xDD];
    let c = checksum(&p[..24]);
    p[10..12].copy_from_slice(&c.to_be_bytes());
    let ip = parse_ipv4(&p).unwrap();
    assert_eq!(ip.payload, &[0xAA, 0xBB, 0xCC, 0xDD]);
    assert_eq!(ip.header.len(), 24);
}

#[test]
fn udp_roundtrip_and_rejects() {
    let (s, d) = ([10, 0, 0, 1], [10, 0, 0, 2]);
    let u = build_udp(s, d, 1234, 53, b"payload!");
    let p = parse_udp(s, d, &u).unwrap();
    assert_eq!((p.sport, p.dport, p.payload), (1234, 53, &b"payload!"[..]));
    // Wrong pseudo-header addresses make the checksum fail.
    assert!(parse_udp(s, [10, 0, 0, 3], &u).is_none());
    // A flipped payload bit fails.
    let mut bad = u.clone();
    bad[9] ^= 0x10;
    assert!(parse_udp(s, d, &bad).is_none());
    // Checksum zero = none: accepted even when the data changed.
    let mut none = u.clone();
    none[6] = 0;
    none[7] = 0;
    none[9] ^= 0x10;
    assert!(parse_udp(s, d, &none).is_some());
    // Length field longer than the data, or shorter than a header.
    let mut long = u.clone();
    long[5] = 200;
    assert!(parse_udp(s, d, &long).is_none());
    let mut short = u.clone();
    short[4] = 0;
    short[5] = 4;
    assert!(parse_udp(s, d, &short).is_none());
    // Empty payload works.
    let e = build_udp(s, d, 1, 2, b"");
    assert_eq!(parse_udp(s, d, &e).unwrap().payload.len(), 0);
}

#[test]
fn icmp_roundtrip_and_corruption() {
    let m = build_icmp(ICMP_ECHO_REQUEST, 0, echo_rest(0x1234, 0x0007), b"abcdef");
    let p = parse_icmp(&m).unwrap();
    assert_eq!((p.kind, p.code, p.rest, p.data), (8, 0, [0x12, 0x34, 0, 7], &b"abcdef"[..]));
    let mut bad = m.clone();
    bad[10] ^= 1;
    assert!(parse_icmp(&bad).is_none());
    assert!(parse_icmp(&m[..7]).is_none());
}

#[test]
fn parsers_survive_random_garbage() {
    let mut seed = 0x1234_5678_9ABC_DEF0u64;
    for round in 0..40_000 {
        let len = (xorshift(&mut seed) % 120) as usize;
        let mut buf: Vec<u8> = (0..len).map(|_| xorshift(&mut seed) as u8).collect();
        // Half the rounds: start from something plausible so parsers get deeper.
        if round % 2 == 0 && len >= 20 {
            buf[0] = 0x45;
            let l = (len as u16).to_be_bytes();
            buf[2] = l[0];
            buf[3] = l[1];
        }
        let _ = parse_eth(&buf);
        let _ = parse_arp(&buf);
        let _ = parse_ipv4(&buf);
        let _ = parse_icmp(&buf);
        let _ = parse_udp([1; 4], [2; 4], &buf);
        let _ = dhcp::parse_reply(&buf);
        let _ = dns::parse_response(0, &buf);
        // A whole stack must also survive being fed garbage frames.
        let mut s = Stack::new([2, 0, 0, 0, 0, 1], 1);
        s.set_static(Config { ip: [10, 0, 0, 2], mask: [255, 255, 255, 0], gateway: None, dns: None });
        s.receive(&buf, 0);
    }
}

// ---------------------------------------------------------------------- DHCP

/// A server's reply, built by the test (independent of the crate's builder).
fn dhcp_reply(msg: u8, xid: u32, chaddr: Mac, yiaddr: Ip, lease: Option<u32>, server: Ip) -> Vec<u8> {
    let mut p = std::vec![0u8; 236];
    p[0] = 2;
    p[1] = 1;
    p[2] = 6;
    p[4..8].copy_from_slice(&xid.to_be_bytes());
    p[16..20].copy_from_slice(&yiaddr);
    p[28..34].copy_from_slice(&chaddr);
    p.extend_from_slice(&[0x63, 0x82, 0x53, 0x63]);
    p.extend_from_slice(&[53, 1, msg]);
    p.extend_from_slice(&[1, 4, 255, 255, 255, 0]);
    p.extend_from_slice(&[3, 4, 10, 0, 0, 1]);
    p.extend_from_slice(&[6, 4, 10, 0, 0, 53]);
    if let Some(l) = lease {
        p.extend_from_slice(&[51, 4]);
        p.extend_from_slice(&l.to_be_bytes());
    }
    p.extend_from_slice(&[54, 4]);
    p.extend_from_slice(&server);
    p.push(255);
    p
}

/// Wrap a DHCP payload the way a server sends it (broadcast).
fn dhcp_frame(payload: &[u8]) -> Vec<u8> {
    let udp = build_udp([10, 0, 0, 1], BROADCAST_IP, 67, 68, payload);
    let ip = build_ipv4([10, 0, 0, 1], BROADCAST_IP, PROTO_UDP, 5, 64, &udp);
    build_eth(BROADCAST_MAC, [2, 0, 0, 0, 0, 0xFE], ETHERTYPE_IPV4, &ip)
}

/// (message type, xid) of a client's DHCP frame, or None if it is not one.
fn client_dhcp(frame: &[u8]) -> Option<(u8, u32)> {
    let e = parse_eth(frame)?;
    let ip = parse_ipv4(e.payload).ok()?;
    let u = parse_udp(ip.src, ip.dst, ip.payload)?;
    if u.dport != 67 || u.payload.len() < 240 || u.payload[0] != 1 {
        return None;
    }
    let xid = u32::from_be_bytes(u.payload[4..8].try_into().unwrap());
    let mut i = 240;
    while i + 2 < u.payload.len() && u.payload[i] != 255 {
        if u.payload[i] == 53 {
            return Some((u.payload[i + 2], xid));
        }
        i += 2 + u.payload[i + 1] as usize;
    }
    None
}

const MAC_A: Mac = [2, 0, 0, 0, 0, 0x0A];
const MAC_B: Mac = [2, 0, 0, 0, 0, 0x0B];
const IP_A: Ip = [10, 0, 0, 2];
const IP_B: Ip = [10, 0, 0, 3];

fn cfg(ip: Ip, gw: Option<Ip>) -> Config {
    Config { ip, mask: [255, 255, 255, 0], gateway: gw, dns: Some([10, 0, 0, 53]) }
}

/// Run a full DHCP exchange against a scripted server; returns the stack, bound.
fn bound_stack(lease: Option<u32>) -> Stack {
    let mut s = Stack::new(MAC_A, 42);
    s.dhcp_start(0);
    let (t, xid) = client_dhcp(&s.next_frame().unwrap()).unwrap();
    assert_eq!(t, 1);
    s.receive(&dhcp_frame(&dhcp_reply(2, xid, MAC_A, IP_A, lease, [10, 0, 0, 1])), 10);
    let (t, xid2) = client_dhcp(&s.next_frame().unwrap()).unwrap();
    assert_eq!((t, xid2), (3, xid));
    s.receive(&dhcp_frame(&dhcp_reply(5, xid, MAC_A, IP_A, lease, [10, 0, 0, 1])), 20);
    s
}

#[test]
fn dhcp_builders_and_parser_agree() {
    let d = dhcp::build_discover(MAC_A, 0xCAFEBABE);
    assert!(d.len() >= 300);
    assert_eq!(&d[236..240], &[0x63, 0x82, 0x53, 0x63]);
    assert_eq!(&d[28..34], &MAC_A);
    assert_eq!(d[0], 1);
    let r = dhcp::build_request(MAC_A, 5, IP_A, [10, 0, 0, 1]);
    assert!(r.windows(6).any(|w| w == [50, 4, 10, 0, 0, 2]));
    assert!(r.windows(6).any(|w| w == [54, 4, 10, 0, 0, 1]));

    let reply = dhcp_reply(2, 0x1111, MAC_A, IP_A, Some(600), [10, 0, 0, 1]);
    let p = dhcp::parse_reply(&reply).unwrap();
    assert_eq!(p.msg, dhcp::MsgType::Offer);
    assert_eq!(p.xid, 0x1111);
    assert_eq!(p.yiaddr, IP_A);
    assert_eq!(p.mask, Some([255, 255, 255, 0]));
    assert_eq!(p.router, Some([10, 0, 0, 1]));
    assert_eq!(p.dns, Some([10, 0, 0, 53]));
    assert_eq!(p.server, Some([10, 0, 0, 1]));
    assert_eq!(p.lease_secs, Some(600));
}

#[test]
fn dhcp_parser_rejects_bad_input() {
    let good = dhcp_reply(2, 1, MAC_A, IP_A, None, [10, 0, 0, 1]);
    let mut no_cookie = good.clone();
    no_cookie[236] = 0;
    assert!(dhcp::parse_reply(&no_cookie).is_none());
    let mut request_op = good.clone();
    request_op[0] = 1;
    assert!(dhcp::parse_reply(&request_op).is_none());
    // An option whose length runs past the end.
    let mut cut = good.clone();
    cut.truncate(good.len() - 4);
    cut.push(54);
    cut.push(200);
    assert!(dhcp::parse_reply(&cut).is_none());
    // No message-type option at all.
    let mut no_type = good.clone();
    no_type[240] = 0;
    no_type[241] = 0;
    no_type[242] = 0;
    assert!(dhcp::parse_reply(&no_type).is_none());
    assert!(dhcp::parse_reply(&good[..100]).is_none());
}

#[test]
fn dhcp_full_exchange_configures_the_stack() {
    let s = bound_stack(Some(3600));
    let c = s.config().unwrap();
    assert_eq!(c.ip, IP_A);
    assert_eq!(c.mask, [255, 255, 255, 0]);
    assert_eq!(c.gateway, Some([10, 0, 0, 1]));
    assert_eq!(c.dns, Some([10, 0, 0, 53]));
    assert!(s.dhcp_bound());
}

#[test]
fn dhcp_ignores_wrong_xid_and_wrong_client() {
    let mut s = Stack::new(MAC_A, 42);
    s.dhcp_start(0);
    let (_, xid) = client_dhcp(&s.next_frame().unwrap()).unwrap();
    s.receive(&dhcp_frame(&dhcp_reply(2, xid ^ 1, MAC_A, IP_A, None, [10, 0, 0, 1])), 5);
    assert!(s.next_frame().is_none(), "an offer with the wrong xid must be ignored");
    s.receive(&dhcp_frame(&dhcp_reply(2, xid, MAC_B, IP_A, None, [10, 0, 0, 1])), 5);
    assert!(s.next_frame().is_none(), "an offer for another client must be ignored");
    // An offer without a server id cannot be answered.
    let mut no_server = dhcp_reply(2, xid, MAC_A, IP_A, None, [10, 0, 0, 1]);
    let n = no_server.len();
    no_server[n - 7] = 99; // turn option 54 into an unknown option
    s.receive(&dhcp_frame(&no_server), 5);
    assert!(s.next_frame().is_none());
    assert!(s.config().is_none());
    // The real one still works.
    s.receive(&dhcp_frame(&dhcp_reply(2, xid, MAC_A, IP_A, None, [10, 0, 0, 1])), 6);
    assert_eq!(client_dhcp(&s.next_frame().unwrap()).unwrap().0, 3);
}

#[test]
fn dhcp_ack_without_request_is_ignored() {
    let mut s = Stack::new(MAC_A, 42);
    s.dhcp_start(0);
    let (_, xid) = client_dhcp(&s.next_frame().unwrap()).unwrap();
    // An ACK straight after DISCOVER (skipping OFFER/REQUEST) must not configure us.
    s.receive(&dhcp_frame(&dhcp_reply(5, xid, MAC_A, IP_A, None, [10, 0, 0, 1])), 5);
    assert!(s.config().is_none());
}

#[test]
fn dhcp_retransmits_with_backoff() {
    let mut s = Stack::new(MAC_A, 42);
    s.dhcp_start(0);
    let mut sent_at = Vec::new();
    for t in 0..=60_000u64 {
        if t % 50 == 0 {
            s.poll(t);
        }
        while let Some(f) = s.next_frame() {
            if client_dhcp(&f).is_some() {
                sent_at.push(t);
            }
        }
    }
    // 0, then +2 s, +4 s, +8 s, +16 s, +16 s ...
    assert_eq!(sent_at, std::vec![0, 2000, 6000, 14000, 30000, 46000]);
}

#[test]
fn dhcp_nak_restarts_discovery() {
    let mut s = Stack::new(MAC_A, 42);
    s.dhcp_start(0);
    let (_, xid) = client_dhcp(&s.next_frame().unwrap()).unwrap();
    s.receive(&dhcp_frame(&dhcp_reply(2, xid, MAC_A, IP_A, None, [10, 0, 0, 1])), 1);
    s.next_frame().unwrap(); // the REQUEST
    s.receive(&dhcp_frame(&dhcp_reply(6, xid, MAC_A, ZERO_IP, None, [10, 0, 0, 1])), 2);
    assert!(s.config().is_none());
    let (t, _) = client_dhcp(&s.next_frame().unwrap()).unwrap();
    assert_eq!(t, 1, "after a NAK we start over with a DISCOVER");
}

#[test]
fn dhcp_lease_renews_and_expires() {
    // Lease 100 s: renewal REQUEST at 50 s.
    let mut s = bound_stack(Some(100));
    assert!(s.config().is_some());
    s.poll(40_000);
    assert!(s.next_frame().is_none());
    s.poll(20 + 50_000);
    let (t, xid) = client_dhcp(&s.next_frame().unwrap()).unwrap();
    assert_eq!(t, 3, "renewal is a REQUEST");
    assert!(s.config().is_some(), "address kept while renewing");
    // The server answers: still configured, new lease.
    s.receive(&dhcp_frame(&dhcp_reply(5, xid, MAC_A, IP_A, Some(100), [10, 0, 0, 1])), 50_100);
    assert!(s.dhcp_bound());
    assert!(s.config().is_some());
    // Next renewal is due 50 s after that.
    s.poll(50_100 + 49_000);
    assert!(s.next_frame().is_none());

    // Now a server that goes silent: the address is dropped at the end of the lease.
    let mut s = bound_stack(Some(100));
    // The lease was granted at t = 20 ms, so it ends at 100.02 s.
    for t in (50_000..=100_000u64).step_by(100) {
        s.poll(t);
        while s.next_frame().is_some() {}
    }
    assert!(s.config().is_some(), "still ours until the lease ends");
    s.poll(100_100);
    assert!(s.config().is_none(), "lease expired");
    let (t, _) = client_dhcp(&s.next_frame().unwrap()).unwrap();
    assert_eq!(t, 1, "and discovery starts again");
}

#[test]
fn dhcp_reply_does_not_leak_into_udp_sockets() {
    // Even if a program bound port 68, the DHCP client gets its packet first and
    // the program sees nothing, so a program cannot hijack or spy on our lease.
    let mut s = Stack::new(MAC_A, 42);
    assert_eq!(s.udp_bind(68), Ok(68));
    s.dhcp_start(0);
    let (_, xid) = client_dhcp(&s.next_frame().unwrap()).unwrap();
    s.receive(&dhcp_frame(&dhcp_reply(2, xid, MAC_A, IP_A, None, [10, 0, 0, 1])), 1);
    assert_eq!(s.udp_pending(68), 0);
}

// ------------------------------------------------------------------------ DNS

fn dns_answer(id: u16, name: &str, answers: &[(u16, &[u8])], rcode: u16) -> Vec<u8> {
    let q = dns::build_query(id, name).unwrap();
    let mut r = q.clone();
    r[2..4].copy_from_slice(&(0x8180u16 | rcode).to_be_bytes());
    r[6..8].copy_from_slice(&(answers.len() as u16).to_be_bytes());
    for (rtype, data) in answers {
        r.extend_from_slice(&[0xC0, 0x0C]); // pointer to the question name
        r.extend_from_slice(&rtype.to_be_bytes());
        r.extend_from_slice(&1u16.to_be_bytes());
        r.extend_from_slice(&60u32.to_be_bytes());
        r.extend_from_slice(&(data.len() as u16).to_be_bytes());
        r.extend_from_slice(data);
    }
    r
}

#[test]
fn dns_query_layout() {
    let q = dns::build_query(0xABCD, "www.example.com").unwrap();
    assert_eq!(&q[0..2], &[0xAB, 0xCD]);
    assert_eq!(&q[2..4], &[0x01, 0x00]);
    assert_eq!(&q[4..6], &[0, 1]);
    assert_eq!(&q[12..], b"\x03www\x07example\x03com\x00\x00\x01\x00\x01");
    // A trailing dot is the same name.
    assert_eq!(dns::build_query(1, "a.b.").unwrap(), dns::build_query(1, "a.b").unwrap());
}

#[test]
fn dns_rejects_bad_names() {
    assert_eq!(dns::build_query(1, "").unwrap_err(), DnsError::BadName);
    assert_eq!(dns::build_query(1, "a..b").unwrap_err(), DnsError::BadName);
    assert_eq!(dns::build_query(1, ".").unwrap_err(), DnsError::BadName);
    let long_label = "a".repeat(64);
    assert_eq!(dns::build_query(1, &long_label).unwrap_err(), DnsError::BadName);
    assert!(dns::build_query(1, &"a".repeat(63)).is_ok());
    let long_name = std::vec!["abcdefghi"; 30].join(".");
    assert_eq!(dns::build_query(1, &long_name).unwrap_err(), DnsError::BadName);
    assert_eq!(dns::build_query(1, "has space.com").unwrap_err(), DnsError::BadName);
}

#[test]
fn dns_parses_answers() {
    let r = dns_answer(7, "example.com", &[(1, &[93, 184, 216, 34])], 0);
    assert_eq!(dns::parse_response(7, &r), Ok([93, 184, 216, 34]));
    // CNAME first, then the address.
    let r = dns_answer(7, "example.com", &[(5, b"\x03foo\xC0\x0C"), (1, &[1, 2, 3, 4])], 0);
    assert_eq!(dns::parse_response(7, &r), Ok([1, 2, 3, 4]));
    // Only a CNAME / only an AAAA record: no IPv4 address.
    let r = dns_answer(7, "example.com", &[(28, &[0; 16])], 0);
    assert_eq!(dns::parse_response(7, &r), Err(DnsError::NoAddress));
}

#[test]
fn dns_rejects_bad_responses() {
    let good = dns_answer(7, "example.com", &[(1, &[1, 2, 3, 4])], 0);
    assert_eq!(dns::parse_response(8, &good), Err(DnsError::WrongId));
    let nx = dns_answer(7, "nope.example", &[], 3);
    assert_eq!(dns::parse_response(7, &nx), Err(DnsError::NoSuchName));
    let sf = dns_answer(7, "example.com", &[], 2);
    assert_eq!(dns::parse_response(7, &sf), Err(DnsError::ServerError(2)));
    // The query itself (QR bit clear) is not an answer.
    let q = dns::build_query(7, "example.com").unwrap();
    assert_eq!(dns::parse_response(7, &q), Err(DnsError::Malformed));
    // Truncated at every length.
    for n in 0..good.len() {
        assert!(dns::parse_response(7, &good[..n]).is_err(), "truncated to {n} bytes must fail");
    }
    // A compression pointer loop cannot hang the parser (we never follow pointers).
    let mut evil = good.clone();
    let n = evil.len();
    evil[n - 16] = 0xC0;
    evil[n - 15] = (n - 16) as u8;
    let _ = dns::parse_response(7, &evil);
}

// ------------------------------------------------------------- the stack itself

struct Pair {
    a: Stack,
    b: Stack,
    /// Every frame that crossed the wire, with who sent it (0 = a, 1 = b).
    log: Vec<(u8, Vec<u8>)>,
}

impl Pair {
    fn new(gateway_a: Option<Ip>) -> Pair {
        let mut a = Stack::new(MAC_A, 1);
        let mut b = Stack::new(MAC_B, 2);
        a.set_static(cfg(IP_A, gateway_a));
        b.set_static(cfg(IP_B, None));
        Pair { a, b, log: Vec::new() }
    }

    /// Move frames between the two stacks until the wire is quiet.
    fn run(&mut self, now: u64) {
        for _ in 0..200 {
            let mut moved = false;
            while let Some(f) = self.a.next_frame() {
                self.log.push((0, f.clone()));
                self.b.receive(&f, now);
                moved = true;
            }
            while let Some(f) = self.b.next_frame() {
                self.log.push((1, f.clone()));
                self.a.receive(&f, now);
                moved = true;
            }
            if !moved {
                return;
            }
        }
        panic!("the two stacks never went quiet");
    }

    fn count(&self, who: u8, pred: impl Fn(&[u8]) -> bool) -> usize {
        self.log.iter().filter(|(w, f)| *w == who && pred(f)).count()
    }
}

fn is_arp_request(f: &[u8]) -> bool {
    parse_eth(f).map_or(false, |e| e.ethertype == ETHERTYPE_ARP && parse_arp(e.payload).map_or(false, |a| a.op == ARP_REQUEST))
}

fn icmp_kind(f: &[u8]) -> Option<(u8, u8)> {
    let e = parse_eth(f)?;
    let ip = parse_ipv4(e.payload).ok()?;
    if ip.proto != PROTO_ICMP {
        return None;
    }
    let m = parse_icmp(ip.payload)?;
    Some((m.kind, m.code))
}

#[test]
fn ping_resolves_arp_and_gets_a_reply() {
    let mut p = Pair::new(None);
    p.a.ping_send(IP_B, 0xBEEF, 3, &[0x55; 32], 100).unwrap();
    p.run(100);
    let r = p.a.ping_recv().expect("an echo reply");
    assert_eq!((r.from, r.id, r.seq, r.len), (IP_B, 0xBEEF, 3, 32));
    assert!(p.a.ping_recv().is_none());
    assert_eq!(p.a.arp_lookup(IP_B, 100), Some(MAC_B));
    assert_eq!(p.count(0, is_arp_request), 1);
    // The second ping needs no ARP.
    p.a.ping_send(IP_B, 0xBEEF, 4, &[], 200).unwrap();
    p.run(200);
    assert_eq!(p.a.ping_recv().unwrap().seq, 4);
    assert_eq!(p.count(0, is_arp_request), 1);
}

#[test]
fn ping_take_picks_only_its_own_reply() {
    let mut p = Pair::new(None);
    p.a.ping_send(IP_B, 100, 1, b"one", 0).unwrap();
    p.a.ping_send(IP_B, 200, 1, b"two!", 0).unwrap();
    p.run(0);
    // A different (id, seq) finds nothing and takes nothing.
    assert!(p.a.ping_take(100, 2).is_none());
    assert_eq!(p.a.ping_take(200, 1).unwrap().len, 4);
    assert!(p.a.ping_take(200, 1).is_none(), "a reply can only be taken once");
    assert_eq!(p.a.ping_take(100, 1).unwrap().len, 3);
}

#[test]
fn ping_to_ourselves_never_touches_the_wire() {
    let mut p = Pair::new(None);
    p.a.ping_send(IP_A, 1, 1, b"me", 0).unwrap();
    assert!(p.a.next_frame().is_none());
    let r = p.a.ping_recv().expect("loopback reply");
    assert_eq!((r.from, r.len), (IP_A, 2));
    p.a.ping_send([127, 0, 0, 1], 1, 2, b"", 0).unwrap();
    assert_eq!(p.a.ping_recv().unwrap().seq, 2);
}

#[test]
fn broadcast_ping_is_not_answered() {
    let mut p = Pair::new(None);
    // Needs the broadcast frame to reach b.
    p.a.ping_send([10, 0, 0, 255], 9, 1, b"x", 0).unwrap();
    p.run(0);
    assert_eq!(p.count(1, |f| icmp_kind(f).is_some()), 0, "no reply to a broadcast ping");
    assert!(p.a.ping_recv().is_none());
}

#[test]
fn no_config_and_no_route_errors() {
    let mut s = Stack::new(MAC_A, 1);
    assert_eq!(s.ping_send(IP_B, 1, 1, b"", 0), Err(NetError::NoConfig));
    assert_eq!(s.udp_send(1000, IP_B, 7, b"", 0), Err(NetError::NoConfig));
    s.set_static(cfg(IP_A, None));
    assert_eq!(s.ping_send([8, 8, 8, 8], 1, 1, b"", 0), Err(NetError::NoRoute));
    assert_eq!(s.udp_send(1000, [8, 8, 8, 8], 7, b"", 0), Err(NetError::NoRoute));
}

#[test]
fn off_link_traffic_goes_through_the_gateway() {
    // b plays the gateway (10.0.0.3); a sends to 8.8.8.8.
    let mut p = Pair::new(Some(IP_B));
    p.a.udp_send(5000, [8, 8, 8, 8], 53, b"query", 0).unwrap();
    // First frame out is an ARP request for the gateway, not for 8.8.8.8.
    let first = p.a.next_frame().unwrap();
    let arp = parse_arp(parse_eth(&first).unwrap().payload).unwrap();
    assert_eq!(arp.tpa, IP_B);
    p.b.receive(&first, 0);
    p.run(0);
    // The data frame is addressed to the gateway's MAC but carries 8.8.8.8 as the IP destination.
    let sent: Vec<_> = p.log.iter().filter(|(w, _)| *w == 0).collect();
    let data = sent.iter().find_map(|(_, f)| {
        let e = parse_eth(f)?;
        let ip = parse_ipv4(e.payload).ok()?;
        (ip.proto == PROTO_UDP).then(|| (e.dst, ip.dst))
    });
    assert_eq!(data, Some((MAC_B, [8, 8, 8, 8])));
}

#[test]
fn packets_wait_for_arp_then_go_out_in_order() {
    let mut p = Pair::new(None);
    let sock = p.b.udp_bind(7000).unwrap();
    for i in 0..5u8 {
        p.a.udp_send(4000, IP_B, 7000, &[i], 0).unwrap();
    }
    p.run(0);
    let got: Vec<u8> = (0..5).map(|_| p.b.udp_recv(sock).unwrap().data[0]).collect();
    assert_eq!(got, [0, 1, 2, 3, 4]);
    assert_eq!(p.count(0, is_arp_request), 1, "one ARP request for the whole burst");
}

#[test]
fn unanswered_arp_gives_up_after_four_tries_and_drops_the_packets() {
    let mut a = Stack::new(MAC_A, 1);
    a.set_static(cfg(IP_A, None));
    a.udp_send(4000, [10, 0, 0, 99], 7, b"lost", 0).unwrap();
    let mut requests = 0;
    for t in 0..10_000u64 {
        if t % 100 == 0 {
            a.poll(t);
        }
        while let Some(f) = a.next_frame() {
            if is_arp_request(&f) {
                requests += 1;
            }
        }
    }
    assert_eq!(requests, 4);
    // A later send starts over (the old packet is gone, not stuck forever).
    a.udp_send(4000, [10, 0, 0, 99], 7, b"again", 20_000).unwrap();
    assert!(a.next_frame().map_or(false, |f| is_arp_request(&f)));
}

#[test]
fn arp_cache_expires() {
    let mut p = Pair::new(None);
    p.a.ping_send(IP_B, 1, 1, b"", 0).unwrap();
    p.run(0);
    assert!(p.a.arp_lookup(IP_B, 299_999).is_some());
    assert!(p.a.arp_lookup(IP_B, 300_001).is_none());
    p.a.poll(300_001);
    assert!(p.a.arp_entries().is_empty());
}

#[test]
fn arp_cache_is_not_poisoned_by_unrelated_broadcasts() {
    let mut a = Stack::new(MAC_A, 1);
    a.set_static(cfg(IP_A, None));
    // A request for somebody else, from a stranger: must not create a cache entry.
    let req = Arp { op: ARP_REQUEST, sha: [2, 9, 9, 9, 9, 9], spa: [10, 0, 0, 77], tha: [0; 6], tpa: [10, 0, 0, 5] };
    a.receive(&build_eth(BROADCAST_MAC, req.sha, ETHERTYPE_ARP, &build_arp(&req)), 0);
    assert!(a.arp_entries().is_empty());
    assert!(a.next_frame().is_none(), "and we do not answer for others");
    // A request for us is answered and learned.
    let req = Arp { op: ARP_REQUEST, sha: [2, 9, 9, 9, 9, 9], spa: [10, 0, 0, 77], tha: [0; 6], tpa: IP_A };
    a.receive(&build_eth(BROADCAST_MAC, req.sha, ETHERTYPE_ARP, &build_arp(&req)), 0);
    assert_eq!(a.arp_lookup([10, 0, 0, 77], 0), Some([2, 9, 9, 9, 9, 9]));
    let reply = parse_arp(parse_eth(&a.next_frame().unwrap()).unwrap().payload).unwrap();
    assert_eq!((reply.op, reply.tpa, reply.spa), (ARP_REPLY, [10, 0, 0, 77], IP_A));
    // A fake "reply" with the broadcast MAC as its sender is refused.
    let bad = Arp { op: ARP_REPLY, sha: BROADCAST_MAC, spa: [10, 0, 0, 78], tha: MAC_A, tpa: IP_A };
    a.receive(&build_eth(MAC_A, [2, 8, 8, 8, 8, 8], ETHERTYPE_ARP, &build_arp(&bad)), 0);
    assert_eq!(a.arp_lookup([10, 0, 0, 78], 0), None);
}

#[test]
fn arp_cache_has_a_size_limit() {
    let mut a = Stack::new(MAC_A, 1);
    a.set_static(cfg(IP_A, None));
    for i in 0..200u8 {
        let r = Arp { op: ARP_REQUEST, sha: [2, 0, 0, 0, 1, i], spa: [10, 0, 0, i.max(1)], tha: [0; 6], tpa: IP_A };
        a.receive(&build_eth(BROADCAST_MAC, r.sha, ETHERTYPE_ARP, &build_arp(&r)), i as u64);
        while a.next_frame().is_some() {}
    }
    assert!(a.arp_entries().len() <= 64);
}

#[test]
fn udp_delivery_and_reply() {
    let mut p = Pair::new(None);
    let server = p.b.udp_bind(9000).unwrap();
    let client = p.a.udp_bind(0).unwrap();
    assert!(client >= 49152);
    p.a.udp_send(client, IP_B, server, b"ping?", 0).unwrap();
    p.run(0);
    let d = p.b.udp_recv(server).unwrap();
    assert_eq!((d.src_ip, d.src_port, &d.data[..]), (IP_A, client, &b"ping?"[..]));
    p.b.udp_send(server, d.src_ip, d.src_port, b"pong!", 0).unwrap();
    p.run(0);
    let r = p.a.udp_recv(client).unwrap();
    assert_eq!((r.src_ip, r.src_port, &r.data[..]), (IP_B, 9000, &b"pong!"[..]));
    assert!(p.a.udp_recv(client).is_none());
}

#[test]
fn closed_port_answers_with_icmp_unreachable() {
    let mut p = Pair::new(None);
    p.a.udp_send(4000, IP_B, 12345, b"anyone?", 0).unwrap();
    p.run(0);
    let unreachable = p.log.iter().filter(|(w, f)| *w == 1 && icmp_kind(f) == Some((ICMP_UNREACHABLE, ICMP_PORT_UNREACHABLE))).count();
    assert_eq!(unreachable, 1);
    // But a broadcast datagram to a closed port gets no reply.
    let mut p = Pair::new(None);
    p.a.udp_send(4000, [10, 0, 0, 255], 12345, b"hi all", 0).unwrap();
    p.run(0);
    assert_eq!(p.count(1, |f| icmp_kind(f).is_some()), 0);
}

#[test]
fn udp_ports_and_queues() {
    let mut s = Stack::new(MAC_A, 1);
    s.set_static(cfg(IP_A, None));
    assert_eq!(s.udp_bind(80), Ok(80));
    assert_eq!(s.udp_bind(80), Err(NetError::PortInUse));
    s.udp_unbind(80);
    assert_eq!(s.udp_bind(80), Ok(80));
    // Ephemeral ports are distinct and in range.
    let mut seen = std::collections::BTreeSet::new();
    for _ in 0..500 {
        let p = s.udp_bind(0).unwrap();
        assert!(p >= 49152);
        assert!(seen.insert(p), "port {p} handed out twice");
    }
    // Too-big payload is refused, the biggest allowed one is fine.
    assert_eq!(s.udp_send(80, IP_A, 80, &[0; MAX_UDP_PAYLOAD + 1], 0), Err(NetError::TooBig));
    assert_eq!(s.udp_send(80, IP_A, 81, &[0; MAX_UDP_PAYLOAD], 0), Ok(()));
}

#[test]
fn ephemeral_ports_run_out_cleanly() {
    let mut s = Stack::new(MAC_A, 1);
    let span = 65536 - 49152;
    for _ in 0..span {
        s.udp_bind(0).unwrap();
    }
    assert_eq!(s.udp_bind(0), Err(NetError::NoPorts));
    s.udp_unbind(50000);
    assert_eq!(s.udp_bind(0), Ok(50000));
}

#[test]
fn udp_queue_overflow_drops_and_counts() {
    let mut p = Pair::new(None);
    let port = p.b.udp_bind(9999).unwrap();
    // Resolve ARP first (packets waiting for ARP are a separate, smaller queue).
    p.a.ping_send(IP_B, 1, 1, b"", 0).unwrap();
    p.run(0);
    for i in 0..(UDP_QUEUE_MAX + 10) {
        p.a.udp_send(4000, IP_B, port, &[i as u8], 0).unwrap();
    }
    p.run(0);
    assert_eq!(p.b.udp_pending(port), UDP_QUEUE_MAX);
    assert_eq!(p.b.stats.udp_overflow, 10);
    // The oldest ones were kept, in order.
    assert_eq!(p.b.udp_recv(port).unwrap().data[0], 0);
}

#[test]
fn corrupted_and_misaddressed_frames_are_dropped() {
    let mut p = Pair::new(None);
    let port = p.b.udp_bind(9000).unwrap();
    // Build one good frame by hand to damage in different ways.
    p.a.udp_send(4000, IP_B, port, b"data", 0).unwrap();
    let arp = p.a.next_frame().unwrap();
    p.b.receive(&arp, 0);
    let reply = p.b.next_frame().unwrap();
    p.a.receive(&reply, 0);
    let good = p.a.next_frame().unwrap();

    let before = p.b.stats.rx_dropped;
    let mut bad_payload = good.clone();
    bad_payload[14 + 20 + 8] ^= 0xFF; // first payload byte: the UDP checksum no longer matches
    // (Flipping the last byte of the frame would only hit Ethernet padding, which is harmless.)
    p.b.receive(&bad_payload, 0);
    let mut bad_ip = good.clone();
    bad_ip[14 + 8] ^= 1; // TTL changed
    p.b.receive(&bad_ip, 0);
    let mut other_host = good.clone();
    other_host[0..6].copy_from_slice(&[2, 7, 7, 7, 7, 7]); // not our MAC
    p.b.receive(&other_host, 0);
    // A packet that is perfectly valid at every layer (right checksums) but addressed to
    // another host on the wire to our MAC: it must be dropped for that reason alone.
    let other_dst = [10, 0, 0, 99];
    let udp = build_udp(IP_A, other_dst, 4000, port, b"data");
    let ip = build_ipv4(IP_A, other_dst, PROTO_UDP, 9, 64, &udp);
    let other_ip = build_eth(MAC_B, MAC_A, ETHERTYPE_IPV4, &ip);
    p.b.receive(&other_ip, 0);
    assert_eq!(p.b.stats.rx_dropped - before, 4);
    assert_eq!(p.b.udp_pending(port), 0);
    p.b.receive(&good, 0);
    assert_eq!(p.b.udp_pending(port), 1);
    // Damage in the Ethernet padding (outside the IP packet) is not damage to the packet.
    let mut padded = good.clone();
    *padded.last_mut().unwrap() ^= 0xFF;
    p.b.receive(&padded, 0);
    assert_eq!(p.b.udp_pending(port), 2);
}

#[test]
fn dns_query_goes_to_the_resolver() {
    let mut p = Pair::new(None);
    // Make b the resolver by giving a that address.
    p.a.set_static(Config { ip: IP_A, mask: [255, 255, 255, 0], gateway: None, dns: Some(IP_B) });
    let port = p.b.udp_bind(dns::PORT).unwrap();
    let me = p.a.udp_bind(0).unwrap();
    p.a.dns_query(me, 0x4242, "example.org", 0).unwrap();
    p.run(0);
    let q = p.b.udp_recv(port).unwrap();
    assert_eq!(q.src_port, me);
    assert_eq!(&q.data[0..2], &[0x42, 0x42]);
    // b answers; a parses the answer.
    let mut ans = q.data.clone();
    ans[2] = 0x81;
    ans[3] = 0x80;
    ans[7] = 1;
    ans.extend_from_slice(&[0xC0, 0x0C, 0, 1, 0, 1, 0, 0, 0, 60, 0, 4, 9, 9, 9, 9]);
    p.b.udp_send(dns::PORT, q.src_ip, q.src_port, &ans, 0).unwrap();
    p.run(0);
    let r = p.a.udp_recv(me).unwrap();
    assert_eq!(dns::parse_response(0x4242, &r.data), Ok([9, 9, 9, 9]));
    // No resolver configured: error, not a crash.
    let mut s = Stack::new(MAC_A, 1);
    s.set_static(Config { ip: IP_A, mask: [255, 255, 255, 0], gateway: None, dns: None });
    assert_eq!(s.dns_query(1, 1, "example.org", 0), Err(NetError::NoRoute));
}
