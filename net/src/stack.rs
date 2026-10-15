//! The network stack as one state machine. It owns no hardware: the kernel feeds
//! it received frames and the current time (milliseconds), and collects frames
//! to send. That keeps all of it testable on the host (two stacks wired together).

use crate::dhcp::{self, MsgType};
use crate::dns;
use crate::tcp::Tcp;
use crate::wire::*;
use alloc::collections::{BTreeMap, VecDeque};
use alloc::vec::Vec;

const ARP_TTL_MS: u64 = 300_000;
const ARP_RETRY_MS: u64 = 1_000;
const ARP_TRIES: u8 = 4;
const ARP_CACHE_MAX: usize = 64;
const WAITING_MAX: usize = 16;
pub const UDP_QUEUE_MAX: usize = 32;
const PING_QUEUE_MAX: usize = 16;
pub const EPHEMERAL_FIRST: u16 = 49152;
const LOOPBACK_MAX: usize = 32;
const TTL: u8 = 64;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NetError {
    /// No IP address yet (DHCP has not finished, or no static address).
    NoConfig,
    /// The destination is off-link and there is no gateway.
    NoRoute,
    PortInUse,
    /// Payload does not fit one packet (no fragmentation).
    TooBig,
    /// All ephemeral ports are taken.
    NoPorts,
    NotBound,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Config {
    pub ip: Ip,
    pub mask: Ip,
    pub gateway: Option<Ip>,
    pub dns: Option<Ip>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Datagram {
    pub src_ip: Ip,
    pub src_port: u16,
    pub data: Vec<u8>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PingReply {
    pub from: Ip,
    pub id: u16,
    pub seq: u16,
    pub len: usize,
    pub ttl: u8,
}

#[derive(Clone, Copy, Debug, Default)]
pub struct Stats {
    pub rx_frames: u64,
    pub tx_frames: u64,
    pub rx_dropped: u64,
    pub udp_overflow: u64,
    /// Datagrams queued for programs, and echo replies received (also counts loopback).
    pub udp_rx: u64,
    pub ping_rx: u64,
}

#[derive(Clone, Copy)]
struct ArpEntry {
    ip: Ip,
    mac: Mac,
    expires: u64,
}

struct ArpPending {
    ip: Ip,
    tries: u8,
    next: u64,
}

struct Waiting {
    hop: Ip,
    packet: Vec<u8>,
}

#[derive(Clone, Copy, Debug)]
enum Dhcp {
    Off,
    Discovering { xid: u32, next_send: u64, tries: u32 },
    /// `lease_end` is set while renewing an address we already hold.
    Requesting { xid: u32, ip: Ip, server: Ip, next_send: u64, tries: u32, lease_end: Option<u64> },
    Bound { server: Ip, renew_at: u64, lease_end: u64 },
}

pub struct Stack {
    pub mac: Mac,
    cfg: Option<Config>,
    arp: Vec<ArpEntry>,
    arp_pending: Vec<ArpPending>,
    waiting: VecDeque<Waiting>,
    out: VecDeque<Vec<u8>>,
    udp: BTreeMap<u16, VecDeque<Datagram>>,
    pings: VecDeque<PingReply>,
    loopback: VecDeque<Vec<u8>>,
    in_loopback: bool,
    pub tcp: Tcp,
    dhcp: Dhcp,
    ip_id: u16,
    next_ephemeral: u16,
    rng: u32,
    pub stats: Stats,
}

fn backoff_ms(tries: u32) -> u64 {
    1000u64 << tries.min(4)
}

impl Stack {
    pub fn new(mac: Mac, seed: u32) -> Stack {
        let mix = seed ^ u32::from_be_bytes([mac[2], mac[3], mac[4], mac[5]]);
        Stack {
            mac,
            cfg: None,
            arp: Vec::new(),
            arp_pending: Vec::new(),
            waiting: VecDeque::new(),
            out: VecDeque::new(),
            udp: BTreeMap::new(),
            pings: VecDeque::new(),
            loopback: VecDeque::new(),
            in_loopback: false,
            tcp: Tcp::new(mix.rotate_left(7) ^ 0x5bd1_e995),
            dhcp: Dhcp::Off,
            ip_id: (mix >> 8) as u16,
            next_ephemeral: EPHEMERAL_FIRST + (mix % 1000) as u16,
            rng: if mix == 0 { 0x9E37_79B9 } else { mix },
            stats: Stats::default(),
        }
    }

    fn rand(&mut self) -> u32 {
        let mut x = self.rng;
        x ^= x << 13;
        x ^= x >> 17;
        x ^= x << 5;
        self.rng = x;
        x
    }

    fn next_id(&mut self) -> u16 {
        self.ip_id = self.ip_id.wrapping_add(1);
        self.ip_id
    }

    pub fn config(&self) -> Option<Config> {
        self.cfg
    }

    /// A counter that goes up whenever something a program might be waiting for happened:
    /// a frame arrived, a datagram or echo reply was queued, a TCP segment was handled. The
    /// kernel compares it before and after an operation to wake sleepers (loopback traffic
    /// never touches the network card, so frame counts alone would miss it).
    pub fn work(&self) -> u64 {
        self.stats.rx_frames
            + self.stats.udp_rx
            + self.stats.ping_rx
            + self.tcp.stats.segments_in
            + self.tcp.stats.segments_out
            + self.tcp.stats.connections_opened
    }

    /// Change the configuration. Connections tied to an address we no longer have are reset.
    fn set_cfg(&mut self, new: Option<Config>, now: u64) {
        let old_ip = self.cfg.map(|c| c.ip);
        let new_ip = new.map(|c| c.ip);
        if old_ip.is_some() && old_ip != new_ip {
            // The resets must leave while the old address (and its routes) still exist.
            self.tcp.abort_all(now);
            self.flush_tcp(now);
        }
        self.cfg = new;
        self.tcp.ip = new_ip.unwrap_or(ZERO_IP);
    }

    /// Run `f` on the TCP layer, then put whatever segments it produced on the wire.
    pub fn with_tcp<R>(&mut self, now: u64, f: impl FnOnce(&mut Tcp, Ip) -> R) -> R {
        let ip = self.cfg.map_or(ZERO_IP, |c| c.ip);
        self.tcp.ip = ip;
        let r = f(&mut self.tcp, ip);
        self.flush_tcp(now);
        r
    }

    fn flush_tcp(&mut self, now: u64) {
        let segments = core::mem::take(&mut self.tcp.out);
        for tx in segments {
            let _ = self.ip_output(tx.dst, PROTO_TCP, &tx.segment, now);
        }
    }

    pub fn set_static(&mut self, cfg: Config) {
        self.dhcp = Dhcp::Off;
        self.set_cfg(Some(cfg), 0);
    }

    pub fn dhcp_bound(&self) -> bool {
        matches!(self.dhcp, Dhcp::Bound { .. })
    }

    /// Begin (or restart) DHCP. The first DISCOVER goes out immediately.
    pub fn dhcp_start(&mut self, now: u64) {
        self.set_cfg(None, now);
        self.restart_discovery(now);
    }

    fn restart_discovery(&mut self, now: u64) {
        let xid = self.rand();
        self.dhcp = Dhcp::Discovering { xid, next_send: now, tries: 0 };
        self.dhcp_tick(now);
    }

    // ---- outgoing frames ---------------------------------------------------

    pub fn next_frame(&mut self) -> Option<Vec<u8>> {
        let f = self.out.pop_front()?;
        self.stats.tx_frames += 1;
        Some(f)
    }

    fn push_frame(&mut self, dst: Mac, ethertype: u16, payload: &[u8]) {
        let f = build_eth(dst, self.mac, ethertype, payload);
        self.out.push_back(f);
    }

    // ---- routing and ARP -----------------------------------------------------

    fn on_link(cfg: &Config, ip: Ip) -> bool {
        (0..4).all(|i| (ip[i] ^ cfg.ip[i]) & cfg.mask[i] == 0)
    }

    fn is_directed_broadcast(cfg: &Config, ip: Ip) -> bool {
        (0..4).all(|i| ip[i] | cfg.mask[i] == 0xFF) && Self::on_link(cfg, ip) && cfg.mask != [255; 4]
    }

    fn next_hop(cfg: &Config, dst: Ip) -> Result<Ip, NetError> {
        if dst == BROADCAST_IP || Self::is_directed_broadcast(cfg, dst) || Self::on_link(cfg, dst) {
            Ok(dst)
        } else {
            cfg.gateway.ok_or(NetError::NoRoute)
        }
    }

    pub fn arp_lookup(&self, ip: Ip, now: u64) -> Option<Mac> {
        self.arp.iter().find(|e| e.ip == ip && e.expires > now).map(|e| e.mac)
    }

    pub fn arp_entries(&self) -> Vec<(Ip, Mac)> {
        self.arp.iter().map(|e| (e.ip, e.mac)).collect()
    }

    fn arp_learn(&mut self, ip: Ip, mac: Mac, now: u64) {
        let expires = now + ARP_TTL_MS;
        if let Some(e) = self.arp.iter_mut().find(|e| e.ip == ip) {
            e.mac = mac;
            e.expires = expires;
            return;
        }
        if self.arp.len() >= ARP_CACHE_MAX {
            // Evict the entry closest to expiry.
            if let Some((i, _)) = self.arp.iter().enumerate().min_by_key(|(_, e)| e.expires) {
                self.arp.swap_remove(i);
            }
        }
        self.arp.push(ArpEntry { ip, mac, expires });
    }

    fn send_arp_request(&mut self, target: Ip) {
        let Some(cfg) = self.cfg else { return };
        let a = Arp { op: ARP_REQUEST, sha: self.mac, spa: cfg.ip, tha: [0; 6], tpa: target };
        self.push_frame(BROADCAST_MAC, ETHERTYPE_ARP, &build_arp(&a));
    }

    /// Put an IP packet on the wire toward `hop`, resolving its MAC first if needed.
    fn transmit_ip(&mut self, hop: Ip, packet: Vec<u8>, now: u64) {
        let Some(cfg) = self.cfg else { return };
        if hop == BROADCAST_IP || Self::is_directed_broadcast(&cfg, hop) {
            self.push_frame(BROADCAST_MAC, ETHERTYPE_IPV4, &packet);
        } else if let Some(mac) = self.arp_lookup(hop, now) {
            self.push_frame(mac, ETHERTYPE_IPV4, &packet);
        } else {
            if self.waiting.len() >= WAITING_MAX {
                self.waiting.pop_front();
            }
            self.waiting.push_back(Waiting { hop, packet });
            if !self.arp_pending.iter().any(|p| p.ip == hop) {
                self.arp_pending.push(ArpPending { ip: hop, tries: 1, next: now + ARP_RETRY_MS });
                self.send_arp_request(hop);
            }
        }
    }

    fn flush_waiting(&mut self, ip: Ip, mac: Mac) {
        self.arp_pending.retain(|p| p.ip != ip);
        let mut i = 0;
        while i < self.waiting.len() {
            if self.waiting[i].hop == ip {
                let w = self.waiting.remove(i).unwrap();
                self.push_frame(mac, ETHERTYPE_IPV4, &w.packet);
            } else {
                i += 1;
            }
        }
    }

    /// Send an IP packet carrying `payload` to `dst`.
    pub fn ip_output(&mut self, dst: Ip, proto: u8, payload: &[u8], now: u64) -> Result<(), NetError> {
        let cfg = self.cfg.ok_or(NetError::NoConfig)?;
        if payload.len() > MTU - IP_HEADER {
            return Err(NetError::TooBig);
        }
        let id = self.next_id();
        let packet = build_ipv4(cfg.ip, dst, proto, id, TTL, payload);
        if dst == cfg.ip || dst[0] == 127 {
            // To ourselves: never touches the wire.
            self.loopback.push_back(packet);
            self.drain_loopback(now);
            return Ok(());
        }
        let hop = Self::next_hop(&cfg, dst)?;
        self.transmit_ip(hop, packet, now);
        Ok(())
    }

    fn drain_loopback(&mut self, now: u64) {
        if self.in_loopback {
            return;
        }
        self.in_loopback = true;
        let mut n = 0;
        while let Some(p) = self.loopback.pop_front() {
            n += 1;
            if n > LOOPBACK_MAX {
                self.loopback.clear();
                break;
            }
            self.handle_ipv4(&p, now);
        }
        self.in_loopback = false;
    }

    // ---- receiving -------------------------------------------------------------

    pub fn receive(&mut self, frame: &[u8], now: u64) {
        self.stats.rx_frames += 1;
        let Some(eth) = parse_eth(frame) else {
            self.stats.rx_dropped += 1;
            return;
        };
        if eth.dst != self.mac && eth.dst != BROADCAST_MAC {
            self.stats.rx_dropped += 1;
            return;
        }
        match eth.ethertype {
            ETHERTYPE_ARP => self.handle_arp(eth.payload, now),
            ETHERTYPE_IPV4 => self.handle_ipv4(eth.payload, now),
            _ => self.stats.rx_dropped += 1,
        }
    }

    fn handle_arp(&mut self, payload: &[u8], now: u64) {
        let Some(a) = parse_arp(payload) else {
            self.stats.rx_dropped += 1;
            return;
        };
        let Some(cfg) = self.cfg else { return };
        if a.spa == ZERO_IP || a.spa == BROADCAST_IP || a.sha == BROADCAST_MAC || a.sha == [0; 6] {
            return;
        }
        let for_us = a.tpa == cfg.ip;
        // Only learn from packets addressed to us, or refresh what we already know.
        // (Learning from every broadcast would let any host fill the cache.)
        let known = self.arp.iter().any(|e| e.ip == a.spa);
        if for_us || known {
            self.arp_learn(a.spa, a.sha, now);
            self.flush_waiting(a.spa, a.sha);
        }
        if a.op == ARP_REQUEST && for_us {
            let r = Arp { op: ARP_REPLY, sha: self.mac, spa: cfg.ip, tha: a.sha, tpa: a.spa };
            self.push_frame(a.sha, ETHERTYPE_ARP, &build_arp(&r));
        }
    }

    fn handle_ipv4(&mut self, bytes: &[u8], now: u64) {
        let ip = match parse_ipv4(bytes) {
            Ok(ip) => ip,
            Err(_) => {
                self.stats.rx_dropped += 1;
                return;
            }
        };
        let cfg = self.cfg;
        let unicast_to_us = cfg.map_or(false, |c| ip.dst == c.ip) || (cfg.is_some() && ip.dst[0] == 127);
        let broadcast = ip.dst == BROADCAST_IP || cfg.map_or(false, |c| Self::is_directed_broadcast(&c, ip.dst));
        // Without an address the only thing we accept is the DHCP answer.
        let dhcp_waiting = cfg.is_none() && !matches!(self.dhcp, Dhcp::Off);
        if !unicast_to_us && !broadcast && !dhcp_waiting {
            self.stats.rx_dropped += 1;
            return;
        }
        match ip.proto {
            PROTO_ICMP if cfg.is_some() => self.handle_icmp(&ip, unicast_to_us, now),
            PROTO_UDP => self.handle_udp(&ip, unicast_to_us, now),
            PROTO_TCP if cfg.is_some() && unicast_to_us => {
                self.tcp.input(ip.src, ip.dst, ip.payload, now);
                self.flush_tcp(now);
            }
            _ => self.stats.rx_dropped += 1,
        }
    }

    fn handle_icmp(&mut self, ip: &Ipv4<'_>, unicast_to_us: bool, now: u64) {
        let Some(m) = parse_icmp(ip.payload) else {
            self.stats.rx_dropped += 1;
            return;
        };
        match m.kind {
            // Never answer broadcast pings (amplification), only ones sent to us.
            ICMP_ECHO_REQUEST if unicast_to_us && m.code == 0 => {
                let reply = build_icmp(ICMP_ECHO_REPLY, 0, m.rest, m.data);
                let _ = self.ip_output(ip.src, PROTO_ICMP, &reply, now);
            }
            ICMP_ECHO_REPLY if m.code == 0 => {
                if self.pings.len() >= PING_QUEUE_MAX {
                    self.pings.pop_front();
                }
                self.stats.ping_rx += 1;
                self.pings.push_back(PingReply {
                    from: ip.src,
                    id: u16::from_be_bytes([m.rest[0], m.rest[1]]),
                    seq: u16::from_be_bytes([m.rest[2], m.rest[3]]),
                    len: m.data.len(),
                    ttl: ip.ttl,
                });
            }
            _ => {}
        }
    }

    fn handle_udp(&mut self, ip: &Ipv4<'_>, unicast_to_us: bool, now: u64) {
        let Some(u) = parse_udp(ip.src, ip.dst, ip.payload) else {
            self.stats.rx_dropped += 1;
            return;
        };
        if u.dport == dhcp::CLIENT_PORT && u.sport == dhcp::SERVER_PORT && !matches!(self.dhcp, Dhcp::Off) {
            self.handle_dhcp(u.payload, now);
            return;
        }
        if self.cfg.is_none() {
            self.stats.rx_dropped += 1;
            return;
        }
        if let Some(q) = self.udp.get_mut(&u.dport) {
            if q.len() >= UDP_QUEUE_MAX {
                self.stats.udp_overflow += 1;
            } else {
                q.push_back(Datagram { src_ip: ip.src, src_port: u.sport, data: u.payload.to_vec() });
                self.stats.udp_rx += 1;
            }
        } else if unicast_to_us {
            // Nobody listens: tell the sender (original header + first 8 bytes).
            let mut data = Vec::new();
            data.extend_from_slice(ip.header);
            data.extend_from_slice(&ip.payload[..ip.payload.len().min(8)]);
            let m = build_icmp(ICMP_UNREACHABLE, ICMP_PORT_UNREACHABLE, [0; 4], &data);
            let _ = self.ip_output(ip.src, PROTO_ICMP, &m, now);
        }
    }

    // ---- DHCP client -------------------------------------------------------------

    fn send_dhcp(&mut self, payload: Vec<u8>) {
        let udp = build_udp(ZERO_IP, BROADCAST_IP, dhcp::CLIENT_PORT, dhcp::SERVER_PORT, &payload);
        let id = self.next_id();
        let ip = build_ipv4(ZERO_IP, BROADCAST_IP, PROTO_UDP, id, TTL, &udp);
        self.push_frame(BROADCAST_MAC, ETHERTYPE_IPV4, &ip);
    }

    fn dhcp_tick(&mut self, now: u64) {
        match self.dhcp {
            Dhcp::Off => {}
            Dhcp::Discovering { xid, next_send, tries } => {
                if now >= next_send {
                    let p = dhcp::build_discover(self.mac, xid);
                    self.send_dhcp(p);
                    let tries = tries + 1;
                    self.dhcp = Dhcp::Discovering { xid, next_send: now + backoff_ms(tries), tries };
                }
            }
            Dhcp::Requesting { xid, ip, server, next_send, tries, lease_end } => {
                if lease_end.map_or(false, |end| now >= end) {
                    // Could not renew in time: the address is no longer ours.
                    self.set_cfg(None, now);
                    self.restart_discovery(now);
                } else if now >= next_send {
                    if tries >= 5 && lease_end.is_none() {
                        self.restart_discovery(now);
                    } else {
                        let p = dhcp::build_request(self.mac, xid, ip, server);
                        self.send_dhcp(p);
                        let wait = if lease_end.is_some() { 5_000 } else { backoff_ms(tries + 1) };
                        self.dhcp = Dhcp::Requesting {
                            xid,
                            ip,
                            server,
                            next_send: now + wait,
                            tries: tries + 1,
                            lease_end,
                        };
                    }
                }
            }
            Dhcp::Bound { server, renew_at, lease_end } => {
                if now >= renew_at {
                    if let Some(cfg) = self.cfg {
                        let xid = self.rand();
                        self.dhcp = Dhcp::Requesting {
                            xid,
                            ip: cfg.ip,
                            server,
                            next_send: now,
                            tries: 0,
                            lease_end: Some(lease_end),
                        };
                        self.dhcp_tick(now);
                    }
                }
            }
        }
    }

    fn handle_dhcp(&mut self, payload: &[u8], now: u64) {
        let Some(r) = dhcp::parse_reply(payload) else { return };
        if r.chaddr != self.mac {
            return;
        }
        match self.dhcp {
            Dhcp::Discovering { xid, .. } if r.xid == xid && r.msg == MsgType::Offer => {
                let (Some(server), true) = (r.server, r.yiaddr != ZERO_IP) else { return };
                self.dhcp = Dhcp::Requesting {
                    xid,
                    ip: r.yiaddr,
                    server,
                    next_send: now,
                    tries: 0,
                    lease_end: None,
                };
                self.dhcp_tick(now);
            }
            Dhcp::Requesting { xid, server, lease_end, .. } if r.xid == xid => match r.msg {
                MsgType::Ack if r.yiaddr != ZERO_IP => {
                    let lease_ms = (r.lease_secs.unwrap_or(3600).max(2) as u64) * 1000;
                    let new_cfg = Config {
                        ip: r.yiaddr,
                        mask: r.mask.unwrap_or([255, 255, 255, 0]),
                        gateway: r.router,
                        dns: r.dns,
                    };
                    self.set_cfg(Some(new_cfg), now);
                    self.dhcp = Dhcp::Bound {
                        server: r.server.unwrap_or(server),
                        renew_at: now + lease_ms / 2,
                        lease_end: now + lease_ms,
                    };
                    let _ = lease_end;
                }
                MsgType::Nak => {
                    self.set_cfg(None, now);
                    self.restart_discovery(now);
                }
                _ => {}
            },
            _ => {}
        }
    }

    // ---- periodic work -----------------------------------------------------------

    /// Call regularly (every few tens of milliseconds is plenty).
    pub fn poll(&mut self, now: u64) {
        self.dhcp_tick(now);
        self.tcp.poll(now);
        self.flush_tcp(now);
        // ARP retries and give-up.
        let mut give_up: Vec<Ip> = Vec::new();
        let mut resend: Vec<Ip> = Vec::new();
        for p in self.arp_pending.iter_mut() {
            if now >= p.next {
                if p.tries >= ARP_TRIES {
                    give_up.push(p.ip);
                } else {
                    p.tries += 1;
                    p.next = now + ARP_RETRY_MS;
                    resend.push(p.ip);
                }
            }
        }
        for ip in resend {
            self.send_arp_request(ip);
        }
        for ip in give_up {
            self.arp_pending.retain(|p| p.ip != ip);
            self.waiting.retain(|w| w.hop != ip);
        }
        self.arp.retain(|e| e.expires > now);
    }

    // ---- UDP sockets -------------------------------------------------------------

    /// Reserve `port` (0 = pick a free one). Returns the port.
    pub fn udp_bind(&mut self, port: u16) -> Result<u16, NetError> {
        if port != 0 {
            if self.udp.contains_key(&port) {
                return Err(NetError::PortInUse);
            }
            self.udp.insert(port, VecDeque::new());
            return Ok(port);
        }
        let span = 65536 - EPHEMERAL_FIRST as u32;
        for _ in 0..span {
            let p = self.next_ephemeral;
            self.next_ephemeral = if p == 65535 { EPHEMERAL_FIRST } else { p + 1 };
            if !self.udp.contains_key(&p) {
                self.udp.insert(p, VecDeque::new());
                return Ok(p);
            }
        }
        Err(NetError::NoPorts)
    }

    pub fn udp_unbind(&mut self, port: u16) {
        self.udp.remove(&port);
    }

    pub fn udp_recv(&mut self, port: u16) -> Option<Datagram> {
        self.udp.get_mut(&port)?.pop_front()
    }

    pub fn udp_pending(&self, port: u16) -> usize {
        self.udp.get(&port).map_or(0, |q| q.len())
    }

    pub fn udp_send(&mut self, sport: u16, dst: Ip, dport: u16, data: &[u8], now: u64) -> Result<(), NetError> {
        if data.len() > MAX_UDP_PAYLOAD {
            return Err(NetError::TooBig);
        }
        let cfg = self.cfg.ok_or(NetError::NoConfig)?;
        let seg = build_udp(cfg.ip, dst, sport, dport, data);
        self.ip_output(dst, PROTO_UDP, &seg, now)
    }

    // ---- ping ----------------------------------------------------------------------

    pub fn ping_send(&mut self, dst: Ip, id: u16, seq: u16, data: &[u8], now: u64) -> Result<(), NetError> {
        if data.len() > MTU - IP_HEADER - 8 {
            return Err(NetError::TooBig);
        }
        let m = build_icmp(ICMP_ECHO_REQUEST, 0, echo_rest(id, seq), data);
        self.ip_output(dst, PROTO_ICMP, &m, now)
    }

    pub fn ping_recv(&mut self) -> Option<PingReply> {
        self.pings.pop_front()
    }

    /// The reply to one particular request, leaving other programs' replies queued.
    pub fn ping_take(&mut self, id: u16, seq: u16) -> Option<PingReply> {
        let i = self.pings.iter().position(|p| p.id == id && p.seq == seq)?;
        self.pings.remove(i)
    }

    // ---- DNS convenience -------------------------------------------------------------

    /// Send a DNS A query for `name` from `sport` to the configured resolver.
    pub fn dns_query(&mut self, sport: u16, id: u16, name: &str, now: u64) -> Result<(), NetError> {
        let server = self.cfg.ok_or(NetError::NoConfig)?.dns.ok_or(NetError::NoRoute)?;
        let q = dns::build_query(id, name).map_err(|_| NetError::TooBig)?;
        self.udp_send(sport, server, dns::PORT, &q, now)
    }
}
