//! TCP tests. Two whole stacks are joined by a simulated link that can lose, duplicate,
//! reorder and delay frames (seeded, so every run is repeatable), with virtual time
//! stepping one millisecond at a time. The application side is simulated as well.

use super::*;
use crate::stack::{Config, Stack};
use crate::wire::{build_eth, build_ipv4, ETHERTYPE_IPV4};
use std::boxed::Box;
use std::vec::Vec;

const A_IP: Ip = [10, 0, 0, 2];
const B_IP: Ip = [10, 0, 0, 3];
const A_MAC: [u8; 6] = [2, 0, 0, 0, 0, 0xA];
const B_MAC: [u8; 6] = [2, 0, 0, 0, 0, 0xB];

fn xorshift(state: &mut u64) -> u64 {
    let mut x = *state;
    x ^= x << 13;
    x ^= x >> 7;
    x ^= x << 17;
    *state = x;
    x
}

fn pattern(n: usize, seed: u64) -> Vec<u8> {
    let mut s = seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1;
    (0..n).map(|_| (xorshift(&mut s) >> 24) as u8).collect()
}

// ---- the simulated network ---------------------------------------------------------------

struct Net {
    a: Stack,
    b: Stack,
    now: u64,
    /// (deliver at, destination 0 = a / 1 = b, frame)
    queue: Vec<(u64, u8, Vec<u8>)>,
    rng: u64,
    loss_pm: u32,
    dup_pm: u32,
    reorder_pm: u32,
    delay: u64,
    jitter: u64,
    /// Return true to drop the frame sent by `from` (0 = a, 1 = b).
    drop_if: Option<Box<dyn FnMut(u8, &[u8]) -> bool>>,
    /// Extra milliseconds of delay for a frame (to make one overtake another).
    extra_delay: Option<Box<dyn FnMut(u8, &[u8]) -> u64>>,
    frames: [u64; 2],
    dropped: u64,
}

impl Net {
    fn new() -> Net {
        let mut a = Stack::new(A_MAC, 11);
        let mut b = Stack::new(B_MAC, 22);
        a.set_static(Config { ip: A_IP, mask: [255, 255, 255, 0], gateway: None, dns: None });
        b.set_static(Config { ip: B_IP, mask: [255, 255, 255, 0], gateway: None, dns: None });
        let mut net = Net::bare(a, b);
        // The hosts learn each other's hardware addresses first (a ping does it), so TCP
        // tests see TCP traffic and not ARP.
        let now = net.now;
        net.a.ping_send(B_IP, 1, 1, b"", now).unwrap();
        net.run(10);
        assert!(net.a.ping_recv().is_some(), "the hosts cannot reach each other");
        net
    }

    fn bare(a: Stack, b: Stack) -> Net {
        Net {
            a,
            b,
            now: 1000,
            queue: Vec::new(),
            rng: 0x1234_5678_9ABC_DEF1,
            loss_pm: 0,
            dup_pm: 0,
            reorder_pm: 0,
            delay: 1,
            jitter: 0,
            drop_if: None,
            extra_delay: None,
            frames: [0; 2],
            dropped: 0,
        }
    }

    fn seeded(seed: u64, loss_pm: u32, dup_pm: u32, reorder_pm: u32) -> Net {
        let mut n = Net::new();
        n.rng = seed.wrapping_mul(0x2545_F491_4F6C_DD1D) | 1;
        n.loss_pm = loss_pm;
        n.dup_pm = dup_pm;
        n.reorder_pm = reorder_pm;
        n.jitter = 2;
        n
    }

    fn chance(&mut self, per_mille: u32) -> bool {
        per_mille > 0 && (xorshift(&mut self.rng) % 1000) < per_mille as u64
    }

    fn tick(&mut self) {
        self.now += 1;
        let now = self.now;
        // Deliver what is due, oldest first.
        self.queue.sort_by_key(|(at, _, _)| *at);
        let due = self.queue.iter().take_while(|(at, _, _)| *at <= now).count();
        let delivered: Vec<(u64, u8, Vec<u8>)> = self.queue.drain(..due).collect();
        for (_, to, frame) in delivered {
            if to == 0 {
                self.a.receive(&frame, now);
            } else {
                self.b.receive(&frame, now);
            }
        }
        self.a.poll(now);
        self.b.poll(now);
        for from in 0..2u8 {
            loop {
                let frame = if from == 0 { self.a.next_frame() } else { self.b.next_frame() };
                let Some(frame) = frame else { break };
                self.frames[from as usize] += 1;
                if let Some(f) = self.drop_if.as_mut() {
                    if f(from, &frame) {
                        self.dropped += 1;
                        continue;
                    }
                }
                if self.chance(self.loss_pm) {
                    self.dropped += 1;
                    continue;
                }
                let copies = if self.chance(self.dup_pm) { 2 } else { 1 };
                for _ in 0..copies {
                    let mut at = now + self.delay;
                    if let Some(e) = self.extra_delay.as_mut() {
                        at += e(from, &frame);
                    }
                    if self.jitter > 0 {
                        at += xorshift(&mut self.rng) % (self.jitter + 1);
                    }
                    if self.chance(self.reorder_pm) {
                        at += 1 + xorshift(&mut self.rng) % 8;
                    }
                    self.queue.push((at, 1 - from, frame.clone()));
                }
            }
        }
    }

    fn run(&mut self, ms: u64) {
        for _ in 0..ms {
            self.tick();
        }
    }

    /// Step until `f` is true; false if `limit_ms` of virtual time pass first.
    fn run_until(&mut self, limit_ms: u64, mut f: impl FnMut(&mut Net) -> bool) -> bool {
        for _ in 0..limit_ms {
            if f(self) {
                return true;
            }
            self.tick();
        }
        f(self)
    }

    fn a_tcp<R>(&mut self, f: impl FnOnce(&mut Tcp, Ip, u64) -> R) -> R {
        let now = self.now;
        self.a.with_tcp(now, |t, ip| f(t, ip, now))
    }

    fn b_tcp<R>(&mut self, f: impl FnOnce(&mut Tcp, Ip, u64) -> R) -> R {
        let now = self.now;
        self.b.with_tcp(now, |t, ip| f(t, ip, now))
    }

    /// Hand a hand-made TCP segment to one of the stacks as if it came off the wire.
    fn inject(&mut self, to_b: bool, seg: &Segment<'_>) {
        let (src, dst, smac, dmac) = if to_b { (A_IP, B_IP, A_MAC, B_MAC) } else { (B_IP, A_IP, B_MAC, A_MAC) };
        let tcp = build(src, dst, seg);
        let ip = build_ipv4(src, dst, PROTO_TCP, 4242, 64, &tcp);
        let frame = build_eth(dmac, smac, ETHERTYPE_IPV4, &ip);
        let now = self.now;
        if to_b {
            self.b.receive(&frame, now);
        } else {
            self.a.receive(&frame, now);
        }
    }

    fn check(&self) {
        self.a.tcp.check_invariants();
        self.b.tcp.check_invariants();
    }
}

fn connect_pair(net: &mut Net, port: u16) -> (u32, u32) {
    net.b_tcp(|t, _, _| t.listen(port, 8)).unwrap();
    let ca = net.a_tcp(|t, ip, now| t.connect(ip, B_IP, port, now)).unwrap();
    let ok = net.run_until(200_000, |n| {
        n.a_tcp(|t, _, _| t.connect_result(ca)) == Ok(true) && n.b_tcp(|t, _, _| t.accept_ready(port))
    });
    assert!(ok, "the connection was never established");
    let cb = net.b_tcp(|t, _, _| t.accept(port)).unwrap();
    (ca, cb)
}

/// One application: writes `to_send` (then shuts down its sending side) and collects what arrives.
struct App {
    id: u32,
    to_send: Vec<u8>,
    sent: usize,
    got: Vec<u8>,
    shut: bool,
    eof: bool,
    reading: bool,
    chunk: usize,
    rng: u64,
}

impl App {
    fn new(id: u32, to_send: Vec<u8>, seed: u64) -> App {
        App { id, to_send, sent: 0, got: Vec::new(), shut: false, eof: false, reading: true, chunk: 8000, rng: seed | 1 }
    }

    fn step(&mut self, tcp: &mut Tcp, now: u64) {
        while self.sent < self.to_send.len() {
            let want = (1 + (xorshift(&mut self.rng) as usize % self.chunk)).min(self.to_send.len() - self.sent);
            match tcp.send(self.id, &self.to_send[self.sent..self.sent + want], now) {
                Ok(n) => self.sent += n,
                Err(TcpError::WouldBlock) => break,
                Err(e) => panic!("send failed: {e:?}"),
            }
        }
        if self.sent == self.to_send.len() && !self.shut {
            tcp.shutdown_write(self.id, now).unwrap();
            self.shut = true;
        }
        let mut buf = [0u8; 4096];
        while self.reading && !self.eof {
            let take = 1 + (xorshift(&mut self.rng) as usize % buf.len());
            match tcp.recv(self.id, &mut buf[..take], now) {
                Ok(0) => self.eof = true,
                Ok(n) => self.got.extend_from_slice(&buf[..n]),
                Err(TcpError::WouldBlock) => break,
                Err(e) => panic!("recv failed: {e:?}"),
            }
        }
    }
}

/// Run both applications until each has everything the other sent (and seen the end of stream).
fn transfer(net: &mut Net, a: &mut App, b: &mut App, limit_ms: u64) -> u64 {
    let start = net.now;
    let done = net.run_until(limit_ms, |n| {
        n.a_tcp(|t, _, now| a.step(t, now));
        n.b_tcp(|t, _, now| b.step(t, now));
        a.eof && b.eof && a.shut && b.shut
    });
    assert!(
        done,
        "transfer did not finish in {limit_ms} ms: a sent {}/{} got {}/{} eof {}; b sent {}/{} got {}/{} eof {}",
        a.sent, a.to_send.len(), a.got.len(), b.to_send.len(), a.eof,
        b.sent, b.to_send.len(), b.got.len(), a.to_send.len(), b.eof,
    );
    assert!(a.got == b.to_send, "a received corrupted data ({} vs {} bytes)", a.got.len(), b.to_send.len());
    assert!(b.got == a.to_send, "b received corrupted data ({} vs {} bytes)", b.got.len(), a.to_send.len());
    net.now - start
}

fn finish_and_free(net: &mut Net, ca: u32, cb: u32) {
    net.a_tcp(|t, _, now| t.close(ca, now));
    net.b_tcp(|t, _, now| t.close(cb, now));
    net.run(35_000);
    assert_eq!(net.a.tcp.connection_count(), 0, "a still holds connection state");
    assert_eq!(net.b.tcp.connection_count(), 0, "b still holds connection state");
    net.check();
}

// ---- sequence numbers and the segment format --------------------------------------------------

#[test]
fn sequence_arithmetic_wraps() {
    assert!(seq_lt(1, 2));
    assert!(!seq_lt(2, 1));
    assert!(!seq_lt(5, 5));
    assert!(seq_le(5, 5));
    // Across the wrap point: 0xFFFFFFF0 is "before" 0x10.
    assert!(seq_lt(0xFFFF_FFF0, 0x10));
    assert!(seq_gt(0x10, 0xFFFF_FFF0));
    assert!(seq_ge(0x10, 0x10));
    assert!(seq_in(0xFFFF_FFF0, 0xFFFF_FFFF, 0x10));
    assert!(seq_in(0xFFFF_FFF0, 0x0, 0x10));
    assert!(!seq_in(0xFFFF_FFF0, 0x10, 0x10));
    assert!(!seq_in(0xFFFF_FFF0, 0xFFFF_FFEF, 0x10));
    assert!(!seq_in(100, 100, 100), "an empty window contains nothing");
}

#[test]
fn segments_match_an_independent_implementation() {
    // Bytes produced by a separate Python implementation.
    let syn = [
        0x04, 0xd2, 0x00, 0x50, 0x01, 0x02, 0x03, 0x04, 0, 0, 0, 0, 0x60, 0x02, 0xff, 0xff, 0x7a, 0xfa, 0, 0, 0x02, 0x04, 0x05, 0xb4,
    ];
    let s = parse(A_IP, B_IP, &syn).expect("a valid SYN");
    assert_eq!((s.sport, s.dport, s.seq, s.ack, s.flags, s.window, s.mss), (1234, 80, 0x0102_0304, 0, SYN, 65535, Some(1460)));
    assert!(s.payload.is_empty());
    let built = build(A_IP, B_IP, &Segment { sport: 1234, dport: 80, seq: 0x0102_0304, ack: 0, flags: SYN, window: 65535, mss: Some(1460), payload: &[] });
    assert_eq!(built, syn);

    let data = [
        0x04, 0xd2, 0x00, 0x50, 0x00, 0x00, 0x03, 0xe8, 0x00, 0x00, 0x07, 0xd0, 0x50, 0x18, 0x20, 0x00, 0x8c, 0x47, 0, 0, b'G', b'E', b'T', b' ', b'/', b' ',
        b'H', b'T', b'T', b'P', b'/', b'1', b'.', b'0', b'\r', b'\n', b'\r',
    ];
    let s = parse(A_IP, B_IP, &data).expect("a valid data segment");
    assert_eq!((s.seq, s.ack, s.flags, s.window), (1000, 2000, PSH | ACK, 8192));
    assert_eq!(s.payload, b"GET / HTTP/1.0\r\n\r");
    let built = build(A_IP, B_IP, &Segment { sport: 1234, dport: 80, seq: 1000, ack: 2000, flags: PSH | ACK, window: 8192, mss: None, payload: b"GET / HTTP/1.0\r\n\r" });
    assert_eq!(built, data);
}

#[test]
fn segment_parser_rejects_bad_input() {
    let good = build(A_IP, B_IP, &Segment { sport: 1, dport: 2, seq: 3, ack: 4, flags: ACK, window: 5, mss: None, payload: b"abc" });
    assert!(parse(A_IP, B_IP, &good).is_some());
    assert!(parse(A_IP, [10, 0, 0, 9], &good).is_none(), "wrong pseudo-header address");
    let mut bad = good.clone();
    bad[20] ^= 1;
    assert!(parse(A_IP, B_IP, &bad).is_none(), "payload changed");
    assert!(parse(A_IP, B_IP, &good[..19]).is_none(), "shorter than a header");
    // Data offset beyond the segment, or below five words (checksum fixed up so only that is wrong).
    let fix = |mut v: Vec<u8>| {
        v[16] = 0;
        v[17] = 0;
        let c = transport_checksum(A_IP, B_IP, PROTO_TCP, &v);
        v[16..18].copy_from_slice(&c.to_be_bytes());
        v
    };
    let mut off = good.clone();
    off[12] = 0xF0;
    assert!(parse(A_IP, B_IP, &fix(off)).is_none(), "offset past the end");
    let mut off = good.clone();
    off[12] = 0x40;
    assert!(parse(A_IP, B_IP, &fix(off)).is_none(), "offset below the minimum");
    // Options: an unknown option is skipped, a zero-length one is refused, one that runs off the end is refused.
    let with_opts = |opts: &[u8]| {
        let mut v = good[..20].to_vec();
        v[12] = (((20 + opts.len()) / 4) as u8) << 4;
        v.extend_from_slice(opts);
        v.extend_from_slice(b"abc");
        fix(v)
    };
    let bytes = with_opts(&[1, 1, 3, 3, 7, 0, 0, 0]);
    let s = parse(A_IP, B_IP, &bytes).expect("unknown option window-scale is skipped");
    assert_eq!(s.mss, None);
    assert_eq!(s.payload, b"abc");
    assert!(parse(A_IP, B_IP, &with_opts(&[9, 0, 1, 1])).is_none(), "option length 0");
    assert!(parse(A_IP, B_IP, &with_opts(&[2, 9, 0, 0])).is_none(), "option longer than the header");
    let bytes = with_opts(&[2, 4, 2, 0x58]);
    let s = parse(A_IP, B_IP, &bytes).unwrap();
    assert_eq!(s.mss, Some(600));
    // Reserved/ECN bits in the flags byte are ignored.
    let mut ecn = good.clone();
    ecn[13] |= 0xC0;
    assert_eq!(parse(A_IP, B_IP, &fix(ecn)).unwrap().flags, ACK);
}

#[test]
fn parsers_survive_random_garbage() {
    let mut s = 0xABCD_EF01_2345_6789u64;
    for _ in 0..50_000 {
        let len = (xorshift(&mut s) % 90) as usize;
        let buf: Vec<u8> = (0..len).map(|_| xorshift(&mut s) as u8).collect();
        let _ = parse(A_IP, B_IP, &buf);
    }
    // Also with a valid checksum, so the option and offset logic gets exercised.
    for _ in 0..50_000 {
        let len = 20 + (xorshift(&mut s) % 60) as usize;
        let mut buf: Vec<u8> = (0..len).map(|_| xorshift(&mut s) as u8).collect();
        buf[16] = 0;
        buf[17] = 0;
        let c = transport_checksum(A_IP, B_IP, PROTO_TCP, &buf);
        buf[16..18].copy_from_slice(&c.to_be_bytes());
        let _ = parse(A_IP, B_IP, &buf);
    }
}

// ---- connections ----------------------------------------------------------------------------------

#[test]
fn handshake_and_a_small_exchange() {
    let mut net = Net::new();
    let (ca, cb) = connect_pair(&mut net, 8080);
    assert_eq!(net.a_tcp(|t, _, _| t.state(ca)), Some(State::Established));
    assert_eq!(net.b_tcp(|t, _, _| t.state(cb)), Some(State::Established));
    assert_eq!(net.b_tcp(|t, _, _| t.local_port(cb)), Some(8080));
    let a_port = net.a_tcp(|t, _, _| t.local_port(ca)).unwrap();
    assert!(a_port >= EPHEMERAL_FIRST);
    assert_eq!(net.b_tcp(|t, _, _| t.peer(cb)), Some((A_IP, a_port)));
    assert_eq!(net.a_tcp(|t, _, _| t.peer(ca)), Some((B_IP, 8080)));

    assert_eq!(net.a_tcp(|t, _, now| t.send(ca, b"GET / HTTP/1.0\r\n\r\n", now)), Ok(18));
    let mut buf = [0u8; 100];
    assert_eq!(net.b_tcp(|t, _, now| t.recv(cb, &mut buf, now)), Err(TcpError::WouldBlock), "not there yet");
    net.run(20);
    assert_eq!(net.b_tcp(|t, _, now| t.recv(cb, &mut buf, now)), Ok(18));
    assert_eq!(&buf[..18], b"GET / HTTP/1.0\r\n\r\n");
    assert_eq!(net.b_tcp(|t, _, now| t.send(cb, b"HTTP/1.0 200 OK\r\n", now)), Ok(17));
    net.run(20);
    assert_eq!(net.a_tcp(|t, _, now| t.recv(ca, &mut buf, now)), Ok(17));
    assert_eq!(net.a.tcp.stats.retransmits + net.b.tcp.stats.retransmits, 0);
    net.check();
}

#[test]
fn connecting_to_a_closed_port_is_refused() {
    let mut net = Net::new();
    let ca = net.a_tcp(|t, ip, now| t.connect(ip, B_IP, 9999, now)).unwrap();
    assert!(net.run_until(1000, |n| n.a_tcp(|t, _, _| t.connect_result(ca)).is_err()));
    assert_eq!(net.a_tcp(|t, _, _| t.connect_result(ca)), Err(TcpError::Refused));
    assert_eq!(net.b.tcp.stats.resets_sent, 1);
    // The refusal came quickly: no retransmissions were needed.
    assert!(net.now < 1000 + 50);
    net.a_tcp(|t, _, now| t.close(ca, now));
    assert_eq!(net.a.tcp.connection_count(), 0);
}

#[test]
fn syn_retransmissions_back_off_and_finally_give_up() {
    let mut net = Net::new();
    net.drop_if = Some(Box::new(|from, f| {
        // Drop every TCP frame from a, but let ARP through.
        from == 0 && f.len() > 14 && f[12] == 0x08 && f[13] == 0x00
    }));
    let ca = net.a_tcp(|t, ip, now| t.connect(ip, B_IP, 80, now)).unwrap();
    let start = net.now;
    let mut sent_at = Vec::new();
    let mut seen = 0;
    let mut failed_at = None;
    for _ in 0..200_000 {
        net.tick();
        if net.dropped > seen {
            sent_at.push(net.now - start);
            seen = net.dropped;
        }
        if failed_at.is_none() && net.a_tcp(|t, _, _| t.connect_result(ca)).is_err() {
            failed_at = Some(net.now - start);
            break;
        }
    }
    // The original SYN (about 1 ms after connect) plus six retransmissions, 1, 2, 4, 8, 16, 32 s apart.
    assert_eq!(sent_at.len(), 7, "seven SYNs in all: {sent_at:?}");
    assert!(sent_at[0] <= 3, "the first SYN leaves at once: {sent_at:?}");
    for (got, want) in sent_at[1..].iter().zip([1000u64, 3000, 7000, 15000, 31000, 63000]) {
        assert!(got.abs_diff(want) <= 3, "retransmission at {got} ms, wanted about {want} ({sent_at:?})");
    }
    let failed = failed_at.expect("the connect must fail in the end");
    assert!(failed.abs_diff(123_000) <= 5, "gave up at {failed} ms (the timer is capped at 60 s)");
    assert_eq!(net.a_tcp(|t, _, _| t.connect_result(ca)), Err(TcpError::TimedOut));
}

#[test]
fn a_lost_syn_ack_is_resent() {
    let mut net = Net::new();
    let mut dropped_one = false;
    net.drop_if = Some(Box::new(move |from, f| {
        // Drop the first SYN-ACK from b.
        let is_synack = from == 1 && f.len() > 47 && f[12] == 0x08 && f[13] == 0x00 && f[23] == PROTO_TCP && f[14 + 20 + 13] & (SYN | ACK) == (SYN | ACK);
        if is_synack && !dropped_one {
            dropped_one = true;
            return true;
        }
        false
    }));
    let (ca, cb) = connect_pair(&mut net, 80);
    assert!(net.dropped >= 1);
    assert_eq!(net.a_tcp(|t, _, now| t.send(ca, b"hi", now)), Ok(2));
    net.run(50);
    let mut buf = [0u8; 4];
    assert_eq!(net.b_tcp(|t, _, now| t.recv(cb, &mut buf, now)), Ok(2));
    net.check();
}

#[test]
fn a_full_exchange_on_a_clean_link_is_fast_and_exact() {
    let mut net = Net::new();
    let (ca, cb) = connect_pair(&mut net, 5001);
    let mut a = App::new(ca, pattern(1 << 20, 1), 5);
    let mut b = App::new(cb, pattern(1 << 20, 2), 6);
    let took = transfer(&mut net, &mut a, &mut b, 20_000);
    // 2 MB over a 2 ms round trip with a 64 KB window: about 64 KB per 2 ms in each direction.
    assert!(took < 1500, "1 MB each way took {took} virtual ms");
    assert_eq!(net.a.tcp.stats.retransmits + net.b.tcp.stats.retransmits, 0, "nothing was lost, nothing should be resent");
    finish_and_free(&mut net, ca, cb);
}

#[test]
fn closing_in_both_directions_frees_everything() {
    let mut net = Net::new();
    let (ca, cb) = connect_pair(&mut net, 80);
    net.a_tcp(|t, _, now| t.send(ca, b"bye", now)).unwrap();
    net.a_tcp(|t, _, now| t.close(ca, now));
    net.run(30);
    // b sees the data, then the end of the stream; a (an orphan now) waits in FIN-WAIT-2.
    let mut buf = [0u8; 8];
    assert_eq!(net.b_tcp(|t, _, now| t.recv(cb, &mut buf, now)), Ok(3));
    assert_eq!(net.b_tcp(|t, _, now| t.recv(cb, &mut buf, now)), Ok(0), "end of stream");
    assert_eq!(net.b_tcp(|t, _, _| t.state(cb)), Some(State::CloseWait));
    assert_eq!(net.a.tcp.connection_count(), 1);
    net.b_tcp(|t, _, now| t.close(cb, now));
    net.run(30);
    assert_eq!(net.b.tcp.connection_count(), 0, "the passive closer is done at once");
    assert_eq!(net.a.tcp.connection_count(), 1, "the active closer lingers in TIME-WAIT");
    net.run(29_000);
    assert_eq!(net.a.tcp.connection_count(), 1, "still waiting before the 30 s are over");
    net.run(2_000);
    assert_eq!(net.a.tcp.connection_count(), 0);
    net.check();
}

#[test]
fn both_sides_closing_at_once() {
    let mut net = Net::new();
    let (ca, cb) = connect_pair(&mut net, 80);
    // Both FINs cross on the wire: each side passes through CLOSING.
    net.delay = 20;
    net.a_tcp(|t, _, now| t.close(ca, now));
    net.b_tcp(|t, _, now| t.close(cb, now));
    net.run(200);
    net.check();
    net.run(31_000);
    assert_eq!(net.a.tcp.connection_count(), 0);
    assert_eq!(net.b.tcp.connection_count(), 0);
}

#[test]
fn half_close_still_lets_the_other_side_answer() {
    let mut net = Net::new();
    let (ca, cb) = connect_pair(&mut net, 80);
    net.a_tcp(|t, _, now| t.send(ca, b"request", now)).unwrap();
    net.a_tcp(|t, _, now| t.shutdown_write(ca, now)).unwrap();
    // a may not write any more, but can still read.
    assert_eq!(net.a_tcp(|t, _, now| t.send(ca, b"more", now)), Err(TcpError::BrokenPipe));
    net.run(30);
    let mut buf = [0u8; 64];
    assert_eq!(net.b_tcp(|t, _, now| t.recv(cb, &mut buf, now)), Ok(7));
    assert_eq!(net.b_tcp(|t, _, now| t.recv(cb, &mut buf, now)), Ok(0));
    assert_eq!(net.b_tcp(|t, _, now| t.send(cb, b"a long answer after the question was closed", now)), Ok(43));
    net.run(30);
    assert_eq!(net.a_tcp(|t, _, now| t.recv(ca, &mut buf, now)), Ok(43));
    assert_eq!(net.a_tcp(|t, _, now| t.recv(ca, &mut buf, now)), Err(TcpError::WouldBlock), "b has not closed yet");
    net.b_tcp(|t, _, now| t.close(cb, now));
    net.run(30);
    assert_eq!(net.a_tcp(|t, _, now| t.recv(ca, &mut buf, now)), Ok(0));
}

#[test]
fn closing_with_unread_data_resets() {
    let mut net = Net::new();
    let (ca, cb) = connect_pair(&mut net, 80);
    net.a_tcp(|t, _, now| t.send(ca, b"nobody reads this", now)).unwrap();
    net.run(30);
    net.b_tcp(|t, _, now| t.close(cb, now)); // unread data: reset, not a polite goodbye
    net.run(30);
    let mut buf = [0u8; 4];
    assert_eq!(net.a_tcp(|t, _, now| t.recv(ca, &mut buf, now)), Err(TcpError::Reset));
    assert_eq!(net.b.tcp.connection_count(), 0);
    assert_eq!(net.b.tcp.stats.resets_sent, 1);
}

#[test]
fn abort_resets_the_other_side() {
    let mut net = Net::new();
    let (ca, cb) = connect_pair(&mut net, 80);
    net.a_tcp(|t, _, now| t.abort(ca, now));
    net.run(30);
    let mut buf = [0u8; 4];
    assert_eq!(net.b_tcp(|t, _, now| t.recv(cb, &mut buf, now)), Err(TcpError::Reset));
    assert_eq!(net.b_tcp(|t, _, now| t.send(cb, b"x", now)), Err(TcpError::BrokenPipe), "writing to a reset connection fails");
    assert_eq!(net.b_tcp(|t, _, now| t.recv(cb, &mut buf, now)), Ok(0), "and afterwards it reads as closed");
}

// ---- loss, duplication, reordering ---------------------------------------------------------------------

#[test]
fn a_single_lost_segment_is_repaired_by_fast_retransmit() {
    let mut net = Net::new();
    let (ca, cb) = connect_pair(&mut net, 80);
    // Drop the 3rd data segment a sends.
    let mut data_segments = 0;
    net.drop_if = Some(Box::new(move |from, f| {
        if from == 0 && f.len() > 60 && f[23] == PROTO_TCP {
            data_segments += 1;
            return data_segments == 3;
        }
        false
    }));
    let mut a = App::new(ca, pattern(60_000, 3), 7);
    let mut b = App::new(cb, Vec::new(), 8);
    let took = transfer(&mut net, &mut a, &mut b, 20_000);
    assert_eq!(net.dropped, 1);
    assert_eq!(net.a.tcp.stats.fast_retransmits, 1, "three duplicate ACKs trigger one fast retransmit");
    assert!(took < 300, "recovered without waiting for a timer: {took} ms");
    net.check();
}

#[test]
fn a_lost_last_segment_waits_for_the_timer() {
    let mut net = Net::new();
    let (ca, cb) = connect_pair(&mut net, 80);
    net.run(100);
    let mut first = true;
    net.drop_if = Some(Box::new(move |from, f| {
        if from == 0 && f.len() > 60 && f[23] == PROTO_TCP && first {
            first = false;
            return true;
        }
        false
    }));
    let t0 = net.now;
    net.a_tcp(|t, _, now| t.send(ca, b"just one small segment", now)).unwrap();
    let mut buf = [0u8; 64];
    let mut arrived_at = None;
    for _ in 0..2000 {
        net.tick();
        if let Ok(n) = net.b_tcp(|t, _, now| t.recv(cb, &mut buf, now)) {
            assert_eq!(n, 22);
            arrived_at = Some(net.now - t0);
            break;
        }
    }
    let at = arrived_at.expect("the data must arrive after the retransmission");
    // The timer was at its 200 ms minimum (the handshake measured a round trip of a few ms).
    assert!((195..=230).contains(&at), "retransmission came after {at} ms");
    assert_eq!(net.a.tcp.stats.retransmits, 1);
}

#[test]
fn reordering_two_segments_is_not_a_loss() {
    let mut net = Net::new();
    let (ca, cb) = connect_pair(&mut net, 80);
    // Four full segments go out in one burst. The 2nd is held back 5 ms, so the 3rd and 4th
    // overtake it: the receiver sends two duplicate ACKs, which must not trigger a resend.
    let mut count = 0;
    net.extra_delay = Some(Box::new(move |from, f| {
        if from == 0 && f.len() > 60 && f[23] == PROTO_TCP {
            count += 1;
            if count == 2 {
                return 5;
            }
        }
        0
    }));
    let data = pattern(4 * 1460, 4);
    assert_eq!(net.a_tcp(|t, _, now| t.send(ca, &data, now)), Ok(data.len()));
    net.run(100);
    let mut buf = vec![0u8; 8000];
    assert_eq!(net.b_tcp(|t, _, now| t.recv(cb, &mut buf, now)), Ok(data.len()));
    assert!(buf[..data.len()] == data[..]);
    assert_eq!(net.a.tcp.stats.fast_retransmits, 0, "two duplicate ACKs are not enough to resend");
    assert_eq!(net.a.tcp.stats.retransmits, 0);
    net.check();
}

#[test]
fn duplicated_frames_do_no_harm() {
    let mut net = Net::new();
    net.dup_pm = 500; // every other frame twice
    let (ca, cb) = connect_pair(&mut net, 80);
    let mut a = App::new(ca, pattern(200_000, 5), 11);
    let mut b = App::new(cb, pattern(50_000, 6), 12);
    transfer(&mut net, &mut a, &mut b, 60_000);
    finish_and_free(&mut net, ca, cb);
}

#[test]
fn random_loss_duplication_and_reordering_never_corrupt_the_stream() {
    // (loss, duplicates, reordering) in per mille
    let profiles = [(20, 0, 0), (50, 20, 50), (100, 50, 100), (200, 50, 100)];
    for seed in 1..=6u64 {
        for &(loss, dup, reorder) in &profiles {
            let mut net = Net::seeded(seed, loss, dup, reorder);
            let (ca, cb) = connect_pair(&mut net, 4000);
            let mut a = App::new(ca, pattern(150_000, seed * 10 + 1), seed + 100);
            let mut b = App::new(cb, pattern(90_000, seed * 10 + 2), seed + 200);
            let took = transfer(&mut net, &mut a, &mut b, 900_000);
            assert!(net.dropped > 0 || loss == 0);
            net.check();
            // Not asserting speed (heavy loss is slow), only that it always ends and is exact.
            let _ = took;
            net.a_tcp(|t, _, now| t.close(ca, now));
            net.b_tcp(|t, _, now| t.close(cb, now));
            net.run(130_000);
            assert_eq!(net.a.tcp.connection_count() + net.b.tcp.connection_count(), 0, "seed {seed} loss {loss}: leaked state");
        }
    }
}

#[test]
fn sequence_numbers_wrap_around_cleanly() {
    for start in [0xFFFF_FF00u32, 0xFFFF_FFFF, 0xFFFF_0000, 0x7FFF_FFF0] {
        let mut net = Net::new();
        net.a.tcp.force_iss = Some(start);
        net.b.tcp.force_iss = Some(start.wrapping_sub(50));
        let (ca, cb) = connect_pair(&mut net, 80);
        let mut a = App::new(ca, pattern(300_000, 7), 13);
        let mut b = App::new(cb, pattern(300_000, 8), 14);
        transfer(&mut net, &mut a, &mut b, 30_000);
        net.check();
    }
}

// ---- flow control ------------------------------------------------------------------------------------

#[test]
fn a_reader_that_stops_reading_stops_the_writer() {
    let mut net = Net::new();
    let (ca, cb) = connect_pair(&mut net, 80);
    let data = pattern(600_000, 9);
    // a writes as much as it can; b reads nothing.
    let mut accepted = 0;
    for _ in 0..3000 {
        if accepted < data.len() {
            match net.a_tcp(|t, _, now| t.send(ca, &data[accepted..(accepted + 4000).min(data.len())], now)) {
                Ok(n) => accepted += n,
                Err(TcpError::WouldBlock) => {}
                Err(e) => panic!("{e:?}"),
            }
        }
        net.tick();
    }
    // 128 KB of send buffer + 64 KB the receiver buffered (the rest is flow-controlled).
    assert!(accepted >= 150_000 && accepted <= 200_000, "accepted {accepted} bytes with nobody reading");
    assert_eq!(net.b_tcp(|t, _, _| t.available(cb)), RECV_CAP);
    net.check();
    // Now b reads everything as it comes; a keeps writing; the whole thing arrives.
    let mut got = Vec::new();
    let mut buf = [0u8; 8192];
    let done = net.run_until(60_000, |n| {
        if accepted < data.len() {
            match n.a_tcp(|t, _, now| t.send(ca, &data[accepted..(accepted + 4000).min(data.len())], now)) {
                Ok(k) => accepted += k,
                Err(TcpError::WouldBlock) => {}
                Err(e) => panic!("{e:?}"),
            }
        }
        while let Ok(k) = n.b_tcp(|t, _, now| t.recv(cb, &mut buf, now)) {
            if k == 0 {
                break;
            }
            got.extend_from_slice(&buf[..k]);
        }
        got.len() == data.len()
    });
    assert!(done, "got {} of {}
  a: {}
  b: {}
  now {}", got.len(), data.len(), dump(&net.a.tcp, ca), dump(&net.b.tcp, cb), net.now);
    assert!(got == data);
}

#[test]
fn a_lost_window_update_is_recovered_by_probing() {
    let mut net = Net::new();
    let (ca, cb) = connect_pair(&mut net, 80);
    let data = pattern(400_000, 10);
    let mut accepted = 0;
    for _ in 0..3000 {
        if accepted < data.len() {
            if let Ok(n) = net.a_tcp(|t, _, now| t.send(ca, &data[accepted..(accepted + 4000).min(data.len())], now)) {
                accepted += n;
            }
        }
        net.tick();
    }
    // From now on every frame b sends is lost for three seconds, including the window update.
    let until = net.now + 3000;
    let flag = std::rc::Rc::new(std::cell::Cell::new(until));
    let seen = flag.clone();
    net.drop_if = Some(Box::new(move |from, _| from == 1 && seen.get() > 0));
    let mut got = Vec::new();
    let mut buf = [0u8; 8192];
    let done = net.run_until(120_000, |n| {
        if n.now >= until {
            flag.set(0);
        }
        if accepted < data.len() {
            if let Ok(k) = n.a_tcp(|t, _, now| t.send(ca, &data[accepted..(accepted + 4000).min(data.len())], now)) {
                accepted += k;
            }
        }
        while let Ok(k) = n.b_tcp(|t, _, now| t.recv(cb, &mut buf, now)) {
            if k == 0 {
                break;
            }
            got.extend_from_slice(&buf[..k]);
        }
        got.len() == data.len()
    });
    assert!(done, "stuck at {} of {} bytes after the lost window update", got.len(), data.len());
    assert!(got == data);
    net.check();
}

// ---- listeners ---------------------------------------------------------------------------------------

#[test]
fn the_accept_backlog_is_respected() {
    let mut net = Net::new();
    net.b_tcp(|t, _, _| t.listen(80, 2)).unwrap();
    assert_eq!(net.b_tcp(|t, _, _| t.listen(80, 2)), Err(TcpError::PortInUse), "a port can only be listened on once");
    let clients: Vec<u32> = (0..4).map(|_| net.a_tcp(|t, ip, now| t.connect(ip, B_IP, 80, now)).unwrap()).collect();
    net.run(100);
    // Two fit in the backlog; the other two SYNs were ignored and wait to retry.
    let established = clients.iter().filter(|&&c| net.a_tcp(|t, _, _| t.connect_result(c)) == Ok(true)).count();
    assert_eq!(established, 2);
    // The program accepts: room appears, and the waiting clients get in on their next retry.
    assert!(net.b_tcp(|t, _, _| t.accept(80)).is_some());
    assert!(net.b_tcp(|t, _, _| t.accept(80)).is_some());
    assert!(net.b_tcp(|t, _, _| t.accept(80)).is_none());
    assert!(net.run_until(5000, |n| clients.iter().all(|&c| n.a_tcp(|t, _, _| t.connect_result(c)) == Ok(true))));
    net.check();
}

#[test]
fn closing_a_listener_resets_connections_nobody_accepted() {
    let mut net = Net::new();
    net.b_tcp(|t, _, _| t.listen(80, 8)).unwrap();
    let c1 = net.a_tcp(|t, ip, now| t.connect(ip, B_IP, 80, now)).unwrap();
    let c2 = net.a_tcp(|t, ip, now| t.connect(ip, B_IP, 80, now)).unwrap();
    net.run(50);
    net.b_tcp(|t, _, now| t.unlisten(80, now));
    net.run(50);
    let mut buf = [0u8; 4];
    for c in [c1, c2] {
        assert_eq!(net.a_tcp(|t, _, now| t.recv(c, &mut buf, now)), Err(TcpError::Reset));
    }
    assert_eq!(net.b.tcp.connection_count(), 0);
    // And new connection attempts are refused.
    let c3 = net.a_tcp(|t, ip, now| t.connect(ip, B_IP, 80, now)).unwrap();
    net.run(50);
    assert_eq!(net.a_tcp(|t, _, _| t.connect_result(c3)), Err(TcpError::Refused));
    net.check();
}

#[test]
fn a_server_handles_many_clients_at_once() {
    let mut net = Net::new();
    net.b_tcp(|t, _, _| t.listen(7, 64)).unwrap();
    let mut pairs = Vec::new();
    for i in 0..40u32 {
        let ca = net.a_tcp(|t, ip, now| t.connect(ip, B_IP, 7, now)).unwrap();
        pairs.push((ca, i));
    }
    net.run(500);
    let mut served = Vec::new();
    while let Some(cb) = net.b_tcp(|t, _, _| t.accept(7)) {
        served.push(cb);
    }
    assert_eq!(served.len(), 40);
    // Echo: each client sends its number, the server sends it back doubled.
    for &(ca, i) in &pairs {
        net.a_tcp(|t, _, now| t.send(ca, &[i as u8; 3], now)).unwrap();
    }
    net.run(100);
    let mut buf = [0u8; 8];
    for &cb in &served {
        let n = net.b_tcp(|t, _, now| t.recv(cb, &mut buf, now)).unwrap();
        assert_eq!(n, 3);
        let v = buf[0];
        net.b_tcp(|t, _, now| t.send(cb, &[v.wrapping_mul(2); 3], now)).unwrap();
    }
    net.run(100);
    let mut sum = 0u32;
    for &(ca, _) in &pairs {
        let n = net.a_tcp(|t, _, now| t.recv(ca, &mut buf, now)).unwrap();
        assert_eq!(n, 3);
        sum += buf[0] as u32;
    }
    assert_eq!(sum, (0..40u32).map(|i| (i as u8).wrapping_mul(2) as u32).sum::<u32>());
    net.check();
}

#[test]
fn the_connection_table_has_a_limit() {
    let mut net = Net::new();
    let mut last = Ok(0);
    for _ in 0..300 {
        last = net.a_tcp(|t, ip, now| t.connect(ip, B_IP, 80, now));
        if last.is_err() {
            break;
        }
    }
    assert_eq!(last, Err(TcpError::TooMany));
    assert_eq!(net.a.tcp.connection_count(), 256);
    net.check();
}

// ---- hostile segments ---------------------------------------------------------------------------------

fn seg<'a>(sport: u16, dport: u16, seq: u32, ack: u32, flags: u8, payload: &'a [u8]) -> Segment<'a> {
    Segment { sport, dport, seq, ack, flags, window: 4096, mss: None, payload }
}

fn state_of(net: &mut Net, on_b: bool, id: u32) -> Option<State> {
    if on_b {
        net.b_tcp(|t, _, _| t.state(id))
    } else {
        net.a_tcp(|t, _, _| t.state(id))
    }
}

#[test]
fn resets_are_only_obeyed_when_they_fit() {
    let mut net = Net::new();
    let (ca, cb) = connect_pair(&mut net, 80);
    let a_port = net.a_tcp(|t, _, _| t.local_port(ca)).unwrap();
    let (rcv_nxt, snd_nxt) = {
        let t = &net.b.tcp.conns[&cb];
        (t.rcv_nxt, t.snd_nxt)
    };
    // A reset far outside the window: ignored.
    net.inject(true, &seg(a_port, 80, rcv_nxt.wrapping_add(1 << 30), 0, RST, &[]));
    assert_eq!(state_of(&mut net, true, cb), Some(State::Established));
    // Inside the window but not exactly the next byte: answered with an ACK, not obeyed.
    while net.b.next_frame().is_some() {}
    net.inject(true, &seg(a_port, 80, rcv_nxt.wrapping_add(10), 0, RST, &[]));
    assert_eq!(state_of(&mut net, true, cb), Some(State::Established));
    assert!(net.b.next_frame().is_some(), "a challenge ACK went out");
    // Exactly the next byte: obeyed.
    net.inject(true, &seg(a_port, 80, rcv_nxt, 0, RST, &[]));
    assert_eq!(state_of(&mut net, true, cb), Some(State::Closed));
    let mut buf = [0u8; 4];
    assert_eq!(net.b_tcp(|t, _, now| t.recv(cb, &mut buf, now)), Err(TcpError::Reset));
    let _ = snd_nxt;
}

#[test]
fn forged_segments_do_not_confuse_an_established_connection() {
    let mut net = Net::new();
    let (ca, cb) = connect_pair(&mut net, 80);
    let a_port = net.a_tcp(|t, _, _| t.local_port(ca)).unwrap();
    let (rcv_nxt, snd_nxt) = {
        let t = &net.b.tcp.conns[&cb];
        (t.rcv_nxt, t.snd_nxt)
    };
    // An ACK for data b never sent changes nothing (and is answered).
    net.inject(true, &seg(a_port, 80, rcv_nxt, snd_nxt.wrapping_add(5000), ACK, &[]));
    assert_eq!(net.b.tcp.conns[&cb].snd_una, snd_nxt);
    // Data from the wrong place in the stream is not delivered.
    net.inject(true, &seg(a_port, 80, rcv_nxt.wrapping_sub(500), snd_nxt, ACK, b"old data from the past"));
    net.inject(true, &seg(a_port, 80, rcv_nxt.wrapping_add(1 << 29), snd_nxt, ACK, b"data from the far future"));
    assert_eq!(net.b_tcp(|t, _, _| t.available(cb)), 0);
    // A SYN on a live connection is answered with an ACK and otherwise ignored.
    net.inject(true, &seg(a_port, 80, rcv_nxt.wrapping_add(7), 0, SYN, &[]));
    assert_eq!(state_of(&mut net, true, cb), Some(State::Established));
    // A segment with no flags at all is ignored.
    net.inject(true, &seg(a_port, 80, rcv_nxt, snd_nxt, 0, b"xx"));
    assert_eq!(net.b_tcp(|t, _, _| t.available(cb)), 0);
    // And the connection still works.
    net.a_tcp(|t, _, now| t.send(ca, b"real", now)).unwrap();
    net.run(30);
    let mut buf = [0u8; 8];
    assert_eq!(net.b_tcp(|t, _, now| t.recv(cb, &mut buf, now)), Ok(4));
    net.check();
}

#[test]
fn unsolicited_segments_get_the_right_reset() {
    let mut net = Net::new();
    // An ACK to nowhere: reset with seq = its ack number.
    net.inject(true, &seg(1111, 80, 5, 777, ACK, &[]));
    let f = net.b.next_frame().expect("a reset");
    let ip = crate::wire::parse_ipv4(&f[14..]).unwrap();
    let r = parse(ip.src, ip.dst, ip.payload).unwrap();
    assert_eq!((r.flags, r.seq, r.dport, r.sport), (RST, 777, 1111, 80));
    // A SYN to a closed port: RST|ACK acknowledging the SYN.
    net.inject(true, &seg(1112, 80, 1000, 0, SYN, &[]));
    let f = net.b.next_frame().expect("a reset");
    let ip = crate::wire::parse_ipv4(&f[14..]).unwrap();
    let r = parse(ip.src, ip.dst, ip.payload).unwrap();
    assert_eq!((r.flags, r.seq, r.ack), (RST | ACK, 0, 1001));
    // Never answer a reset with a reset.
    net.inject(true, &seg(1113, 80, 1, 2, RST, &[]));
    assert!(net.b.next_frame().is_none());
    // Garbage with a bad checksum is dropped silently.
    let mut bad = build(A_IP, B_IP, &seg(1114, 80, 1, 2, ACK, b"x"));
    bad[20] ^= 0xFF;
    let ip = build_ipv4(A_IP, B_IP, PROTO_TCP, 1, 64, &bad);
    let frame = build_eth(B_MAC, A_MAC, ETHERTYPE_IPV4, &ip);
    net.b.receive(&frame, net.now);
    assert!(net.b.next_frame().is_none());
    assert_eq!(net.b.tcp.stats.bad_segments, 1);
}

#[test]
fn a_syn_flood_cannot_exhaust_the_listener() {
    let mut net = Net::new();
    net.b_tcp(|t, _, _| t.listen(80, 8)).unwrap();
    for port in 2000..2300u16 {
        net.inject(true, &Segment { sport: port, dport: 80, seq: port as u32 * 7, ack: 0, flags: SYN, window: 1000, mss: Some(1460), payload: &[] });
    }
    assert!(net.b.tcp.connection_count() <= 8, "half-open connections are limited to the backlog");
    net.b.tcp.check_invariants();
    // They time out and disappear: the half-open ones are retried and finally dropped.
    net.run(200_000);
    assert_eq!(net.b.tcp.connection_count(), 0);
}

#[test]
fn time_wait_answers_a_repeated_fin() {
    let mut net = Net::new();
    let (ca, cb) = connect_pair(&mut net, 80);
    net.a_tcp(|t, _, now| t.close(ca, now));
    net.run(30);
    net.b_tcp(|t, _, now| t.close(cb, now));
    net.run(30);
    // a is in TIME-WAIT (an orphan). A late FIN retransmission from b must be acknowledged again.
    let a_port = {
        let (_, t) = net.a.tcp.conns.iter().next().expect("a lingers in TIME-WAIT");
        assert_eq!(t.state, State::TimeWait);
        t.local_port
    };
    let (snd_nxt_b, rcv_nxt_a) = {
        let (_, t) = net.a.tcp.conns.iter().next().unwrap();
        (t.rcv_nxt.wrapping_sub(1), t.snd_nxt)
    };
    net.a.tcp.out.clear();
    net.inject(false, &seg(80, a_port, snd_nxt_b, rcv_nxt_a, FIN | ACK, &[]));
    assert!(net.a.next_frame().is_some(), "the FIN is acknowledged again");
}

// ---- fuzzing the state machine -----------------------------------------------------------------------------

#[test]
fn random_segments_and_calls_never_break_the_state_machine() {
    for seed in 1..=12u64 {
        let mut rng = seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1;
        let mut net = Net::new();
        net.b_tcp(|t, _, _| t.listen(80, 4)).unwrap();
        let mut ids_a: Vec<u32> = Vec::new();
        let mut ids_b: Vec<u32> = Vec::new();
        for step in 0..4000 {
            let r = xorshift(&mut rng);
            match r % 10 {
                0 => {
                    if let Ok(id) = net.a_tcp(|t, ip, now| t.connect(ip, B_IP, if r % 7 == 0 { 81 } else { 80 }, now)) {
                        ids_a.push(id);
                    }
                }
                1 => {
                    if let Some(id) = net.b_tcp(|t, _, _| t.accept(80)) {
                        ids_b.push(id);
                    }
                }
                2 | 3 => {
                    // Application calls on random connections of random sides.
                    let (list, on_b) = if r & 0x100 != 0 { (&ids_b, true) } else { (&ids_a, false) };
                    if let Some(&id) = list.get((r >> 12) as usize % list.len().max(1)) {
                        let now = net.now;
                        let call = (r >> 20) % 6;
                        let mut buf = [0u8; 700];
                        let payload = pattern(1 + (r >> 30) as usize % 3000, r);
                        let stack = if on_b { &mut net.b } else { &mut net.a };
                        stack.with_tcp(now, |t, _| match call {
                            0 | 1 => {
                                let _ = t.send(id, &payload, now);
                            }
                            2 => {
                                let _ = t.recv(id, &mut buf, now);
                            }
                            3 => {
                                let _ = t.shutdown_write(id, now);
                            }
                            4 => {
                                if r % 5 == 0 {
                                    t.abort(id, now);
                                } else {
                                    t.close(id, now);
                                }
                            }
                            _ => {
                                let _ = (t.readable(id), t.writable(id), t.available(id), t.state(id));
                            }
                        });
                    }
                }
                4..=6 => {
                    // A forged segment aimed at a real connection, with fields near the live ones.
                    let to_b = r & 1 != 0;
                    let conns: Vec<(u16, u16, u32, u32)> = {
                        let tcp = if to_b { &net.b.tcp } else { &net.a.tcp };
                        tcp.conns.values().map(|t| (t.remote_port, t.local_port, t.rcv_nxt, t.snd_nxt)).collect()
                    };
                    if let Some(&(sport, dport, rcv_nxt, snd_nxt)) = conns.get((r >> 8) as usize % conns.len().max(1)) {
                        let jitter = |x: u64| ((x % 9) as i64 - 4) as i32 as u32;
                        let seq = if r % 3 == 0 { xorshift(&mut rng) as u32 } else { rcv_nxt.wrapping_add(jitter(xorshift(&mut rng))) };
                        let ack = if r % 3 == 1 { xorshift(&mut rng) as u32 } else { snd_nxt.wrapping_add(jitter(xorshift(&mut rng))) };
                        let flags = (xorshift(&mut rng) as u8) & 0x1F;
                        let payload = pattern((xorshift(&mut rng) % 200) as usize, r);
                        net.inject(to_b, &Segment { sport, dport, seq, ack, flags, window: xorshift(&mut rng) as u16, mss: if r % 11 == 0 { Some(xorshift(&mut rng) as u16) } else { None }, payload: &payload });
                    }
                }
                _ => {
                    net.run(1 + xorshift(&mut rng) % 60);
                }
            }
            if step % 50 == 0 {
                net.check();
            }
        }
        net.check();
        // Whatever happened, closing everything and waiting leaves nothing behind.
        let now = net.now;
        net.a.with_tcp(now, |t, _| t.abort_all(now));
        net.b.with_tcp(now, |t, _| t.abort_all(now));
        net.run(200);
        assert_eq!(net.a.tcp.connection_count() + net.b.tcp.connection_count(), 0, "seed {seed}");
    }
}

/// For failure messages: the interesting fields of one side of a connection.
#[allow(dead_code)]
fn dump(tcp: &Tcp, id: u32) -> String {
    match tcp.conns.get(&id) {
        None => format!("conn {id}: gone"),
        Some(t) => format!(
            "conn {id}: {:?} una={} nxt={} flight={} snd_wnd={} cwnd={} ssthresh={} dup={} rec={} rto={} rexmit_at={:?} retries={} sendbuf={} | rcv_nxt={} recvbuf={} ooo={}/{} win={}",
            t.state, t.snd_una, t.snd_nxt, t.snd_nxt.wrapping_sub(t.snd_una), t.snd_wnd, t.cwnd, t.ssthresh, t.dupacks, t.in_recovery, t.rto, t.rexmit_at, t.retries,
            t.send_buf.len(), t.rcv_nxt, t.recv_buf.len(), t.ooo.len(), t.ooo_bytes(), t.window()
        ),
    }
}

#[test]
fn a_retransmitted_segment_does_not_pollute_the_round_trip_estimate() {
    // Karn's rule: the ACK for a segment that was sent twice says nothing about the round
    // trip, so the estimate (a few ms on this link) must not jump to the timeout (200 ms).
    let mut net = Net::new();
    let (ca, cb) = connect_pair(&mut net, 80);
    net.run(100);
    let mut first = true;
    net.drop_if = Some(Box::new(move |from, f| {
        if from == 0 && f.len() > 60 && f[23] == PROTO_TCP && first {
            first = false;
            return true;
        }
        false
    }));
    net.a_tcp(|t, _, now| t.send(ca, b"lost once", now)).unwrap();
    net.run(1000);
    let mut buf = [0u8; 16];
    assert_eq!(net.b_tcp(|t, _, now| t.recv(cb, &mut buf, now)), Ok(9));
    let t = &net.a.tcp.conns[&ca];
    assert!(t.srtt.unwrap() <= 10, "smoothed round trip is {} ms after a retransmission", t.srtt.unwrap());
    // The timeout also cut the slow-start threshold (to two segments, the minimum).
    assert_eq!(t.ssthresh, 2 * 1460, "a timeout must lower ssthresh");
}

#[test]
fn an_acknowledgement_from_outside_the_window_is_not_believed() {
    let mut net = Net::new();
    let (ca, _cb) = connect_pair(&mut net, 80);
    // b's answers are lost, so a's data stays unacknowledged.
    net.drop_if = Some(Box::new(|from, _| from == 1));
    let data = pattern(3000, 21);
    net.a_tcp(|t, _, now| t.send(ca, &data, now)).unwrap();
    net.run(50);
    let (una, nxt, rcv_nxt, a_port) = {
        let t = &net.a.tcp.conns[&ca];
        (t.snd_una, t.snd_nxt, t.rcv_nxt, t.local_port)
    };
    assert_eq!(nxt.wrapping_sub(una), 3000);
    // A forged "everything arrived" from b's address, with a sequence number far outside a's
    // receive window: a blind attacker's guess. It must change nothing.
    net.inject(false, &seg(80, a_port, rcv_nxt.wrapping_add(1 << 30), nxt, ACK, &[]));
    assert_eq!(net.a.tcp.conns[&ca].snd_una, una, "an ACK from outside the window moved snd_una");
    // A genuine one (the right sequence number) is believed.
    net.inject(false, &seg(80, a_port, rcv_nxt, nxt, ACK, &[]));
    assert_eq!(net.a.tcp.conns[&ca].snd_una, nxt);
}

#[test]
fn a_hostile_mss_is_clamped() {
    let mut net = Net::new();
    net.b_tcp(|t, _, _| t.listen(80, 8)).unwrap();
    net.inject(true, &Segment { sport: 2000, dport: 80, seq: 1, ack: 0, flags: SYN, window: 65535, mss: Some(9000), payload: &[] });
    net.inject(true, &Segment { sport: 2001, dport: 80, seq: 1, ack: 0, flags: SYN, window: 65535, mss: Some(1), payload: &[] });
    net.inject(true, &Segment { sport: 2002, dport: 80, seq: 1, ack: 0, flags: SYN, window: 65535, mss: None, payload: &[] });
    let mss_of = |net: &Net, port: u16| net.b.tcp.conns.values().find(|t| t.remote_port == port).unwrap().mss;
    assert_eq!(mss_of(&net, 2000), 1460, "never above what fits in one packet of ours");
    assert_eq!(mss_of(&net, 2001), 64, "never absurdly small");
    assert_eq!(mss_of(&net, 2002), 536, "the default when the peer says nothing");
}

// ---- a stack talking to itself (the kernel's loopback path) ---------------------------------------------

fn lone_stack() -> Stack {
    let mut s = Stack::new(A_MAC, 99);
    s.set_static(Config { ip: A_IP, mask: [255, 255, 255, 0], gateway: None, dns: None });
    s
}

#[test]
fn connections_to_our_own_address_complete_at_once() {
    let mut s = lone_stack();
    let now = 1000;
    s.with_tcp(now, |t, _| t.listen(7100, 8)).unwrap();
    let mut clients = Vec::new();
    for _ in 0..6 {
        let id = s.with_tcp(now, |t, ip| t.connect(ip, A_IP, 7100, now)).unwrap();
        assert_eq!(s.with_tcp(now, |t, _| t.connect_result(id)), Ok(true), "the handshake finishes inside connect()");
        clients.push(id);
    }
    for _ in 0..6 {
        assert!(s.with_tcp(now, |t, _| t.accept(7100)).is_some());
    }
    assert!(s.with_tcp(now, |t, _| t.accept(7100)).is_none());
    assert!(s.next_frame().is_none(), "nothing reaches the wire");
    s.tcp.check_invariants();
}

#[test]
fn megabytes_through_loopback_need_no_timer() {
    let mut s = lone_stack();
    let mut now = 1000;
    s.with_tcp(now, |t, _| t.listen(7101, 4)).unwrap();
    let c = s.with_tcp(now, |t, ip| t.connect(ip, A_IP, 7101, now)).unwrap();
    let sv = s.with_tcp(now, |t, _| t.accept(7101)).unwrap();
    let total = 3 << 20;
    let data = pattern(total, 31);
    let (mut sent, mut echoed) = (0, 0);
    let mut buf = vec![0u8; 16384];
    let mut back = Vec::with_capacity(total);
    // Like the kernel test: write a chunk, let the "server" echo it, read it back.
    while sent < total {
        let n = (total - sent).min(16384);
        let mut off = 0;
        while off < n {
            off += s.with_tcp(now, |t, _| t.send(c, &data[sent + off..sent + n], now)).unwrap();
        }
        sent += n;
        let mut got = 0;
        while got < n {
            match s.with_tcp(now, |t, _| t.recv(sv, &mut buf, now)) {
                Ok(k) => {
                    let mut o = 0;
                    while o < k {
                        o += s.with_tcp(now, |t, _| t.send(sv, &buf[o..k], now)).unwrap();
                    }
                    got += k;
                }
                Err(TcpError::WouldBlock) => {}
                Err(e) => panic!("{e:?}"),
            }
        }
        while echoed < sent {
            match s.with_tcp(now, |t, _| t.recv(c, &mut buf, now)) {
                Ok(k) => {
                    back.extend_from_slice(&buf[..k]);
                    echoed += k;
                }
                Err(TcpError::WouldBlock) => {
                    // Nothing yet: only a timer could help now. Let a little time pass.
                    now += 10;
                    s.poll(now);
                }
                Err(e) => panic!("{e:?}"),
            }
        }
    }
    assert!(back == data, "the echo is corrupted");
    assert!(now < 1000 + 50, "loopback stalled waiting for timers: {} ms of virtual time", now - 1000);
    assert_eq!(s.tcp.stats.retransmits, 0, "a lossless loopback must never retransmit");
    s.tcp.check_invariants();
}
